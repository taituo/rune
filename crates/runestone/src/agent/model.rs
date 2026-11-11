//! Model providers. A provider takes the exact request bytes (already rendered and stored) and returns the
//! final message as JSON, streaming deltas to the UI. Retries happen here; the record sees one call.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::BufRead;
use std::time::Duration;

pub enum Delta<'a> {
    Text(&'a str),
    Thinking(&'a str),
    ToolUse(&'a str),
    Retry(&'a str),
}

pub trait Provider {
    fn send(&self, body: &[u8], on: &mut dyn FnMut(Delta)) -> Result<Value>;
}

/// Anthropic Messages API with SSE streaming.
pub struct Anthropic {
    pub base_url: String,
    pub api_key: String,
    agent: ureq::Agent,
}

impl Anthropic {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
        Ok(Self::new(&base_url, &api_key, true))
    }

    pub fn new(base_url: &str, api_key: &str, proxy_from_env: bool) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(600))
            .try_proxy_from_env(proxy_from_env)
            .build();
        Anthropic { base_url: base_url.to_string(), api_key: api_key.to_string(), agent }
    }

    fn once(&self, body: &[u8], on: &mut dyn FnMut(Delta)) -> std::result::Result<Value, Attempt> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self
            .agent
            .post(&url)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .set("accept", "text/event-stream")
            .send_bytes(body);
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let wait = r.header("retry-after").and_then(|s| s.parse::<u64>().ok());
                let text = r.into_string().unwrap_or_default();
                let retry = matches!(code, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529);
                return Err(Attempt { retry, wait, err: anyhow!("HTTP {code}: {}", text.trim()) });
            }
            Err(e) => return Err(Attempt { retry: true, wait: None, err: anyhow!("transport: {e}") }),
        };
        let reader = std::io::BufReader::new(resp.into_reader());
        let mut acc = Accumulator::default();
        for line in reader.lines() {
            let line = line.map_err(|e| Attempt { retry: true, wait: None, err: anyhow!("stream: {e}") })?;
            let Some(data) = line.strip_prefix("data:") else { continue };
            let ev: Value = match serde_json::from_str(data.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match acc.feed(&ev, on) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => {
                    let kind = ev["error"]["type"].as_str().unwrap_or("");
                    let retry = matches!(kind, "overloaded_error" | "api_error" | "rate_limit_error");
                    return Err(Attempt { retry, wait: None, err: e });
                }
            }
        }
        acc.finish().map_err(|e| Attempt { retry: true, wait: None, err: e })
    }
}

struct Attempt {
    retry: bool,
    wait: Option<u64>,
    err: anyhow::Error,
}

impl Provider for Anthropic {
    fn send(&self, body: &[u8], on: &mut dyn FnMut(Delta)) -> Result<Value> {
        let mut delay = 2u64;
        for attempt in 1..=6 {
            match self.once(body, on) {
                Ok(v) => return Ok(v),
                Err(a) if a.retry && attempt < 6 => {
                    let wait = a.wait.unwrap_or(delay).min(60);
                    on(Delta::Retry(&format!("{} — retrying in {wait}s", a.err)));
                    std::thread::sleep(Duration::from_secs(wait));
                    delay *= 2;
                }
                Err(a) => return Err(a.err),
            }
        }
        bail!("unreachable")
    }
}

/// Rebuilds the final message from SSE events, identical in shape to a non-streaming response.
#[derive(Default)]
pub struct Accumulator {
    msg: Option<Value>,
    json_bufs: std::collections::HashMap<usize, String>,
    done: bool,
}

impl Accumulator {
    /// Returns Ok(true) at message_stop.
    pub fn feed(&mut self, ev: &Value, on: &mut dyn FnMut(Delta)) -> Result<bool> {
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => {
                let mut m = ev["message"].clone();
                m["content"] = json!([]);
                self.msg = Some(m);
            }
            "content_block_start" => {
                let i = ev["index"].as_u64().unwrap_or(0) as usize;
                let mut block = ev["content_block"].clone();
                if block["type"] == "tool_use" || block["type"] == "server_tool_use" {
                    if let Some(n) = block["name"].as_str() {
                        on(Delta::ToolUse(n));
                    }
                    block["input"] = json!({});
                    self.json_bufs.insert(i, String::new());
                }
                let content = self.content()?;
                while content.len() <= i {
                    content.push(Value::Null);
                }
                content[i] = block;
            }
            "content_block_delta" => {
                let i = ev["index"].as_u64().unwrap_or(0) as usize;
                let d = &ev["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        on(Delta::Text(t));
                        append(&mut self.content()?[i]["text"], t);
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        on(Delta::Thinking(t));
                        append(&mut self.content()?[i]["thinking"], t);
                    }
                    "signature_delta" => {
                        self.content()?[i]["signature"] = d["signature"].clone();
                    }
                    "input_json_delta" => {
                        self.json_bufs.entry(i).or_default().push_str(d["partial_json"].as_str().unwrap_or(""));
                    }
                    "citations_delta" => {
                        let b = &mut self.content()?[i];
                        if !b["citations"].is_array() {
                            b["citations"] = json!([]);
                        }
                        b["citations"].as_array_mut().unwrap().push(d["citation"].clone());
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = ev["index"].as_u64().unwrap_or(0) as usize;
                if let Some(buf) = self.json_bufs.remove(&i) {
                    let input = if buf.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&buf).unwrap_or_else(|_| json!({ "__unparsed": buf }))
                    };
                    self.content()?[i]["input"] = input;
                }
            }
            "message_delta" => {
                let m = self.msg.as_mut().ok_or_else(|| anyhow!("delta before start"))?;
                if let Value::Object(d) = &ev["delta"] {
                    for (k, v) in d {
                        m[k] = v.clone();
                    }
                }
                if let Value::Object(u) = &ev["usage"] {
                    for (k, v) in u {
                        m["usage"][k] = v.clone();
                    }
                }
            }
            "message_stop" => {
                self.done = true;
                return Ok(true);
            }
            "error" => bail!("stream error: {}", ev["error"]),
            _ => {}
        }
        Ok(false)
    }

    fn content(&mut self) -> Result<&mut Vec<Value>> {
        self.msg
            .as_mut()
            .and_then(|m| m["content"].as_array_mut())
            .ok_or_else(|| anyhow!("content before message_start"))
    }

    pub fn finish(self) -> Result<Value> {
        if !self.done {
            bail!("stream ended before message_stop");
        }
        self.msg.ok_or_else(|| anyhow!("empty stream"))
    }
}

fn append(slot: &mut Value, t: &str) {
    match slot {
        Value::String(s) => s.push_str(t),
        _ => *slot = json!(t),
    }
}

/// Replays scripted responses. Used by tests, and the basis for forging: intent is scripted, every tool
/// result is still computed by real execution.
pub struct Scripted {
    pub responses: RefCell<std::collections::VecDeque<Value>>,
    pub requests: RefCell<Vec<Vec<u8>>>,
}

impl Scripted {
    pub fn new(responses: Vec<Value>) -> Self {
        Scripted { responses: RefCell::new(responses.into()), requests: RefCell::new(vec![]) }
    }

    /// A JSON array of assistant messages (each `{"content": [...], "stop_reason": ...}`), or of bare
    /// content arrays.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let v: Value = serde_json::from_slice(&std::fs::read(path).with_context(|| format!("read {}", path.display()))?)?;
        let items = v.as_array().context("script must be a JSON array")?;
        let msgs = items
            .iter()
            .map(|m| if m.is_array() { msg(m.as_array().cloned().unwrap_or_default()) } else { m.clone() })
            .collect();
        Ok(Self::new(msgs))
    }
}

impl Provider for Scripted {
    fn send(&self, body: &[u8], on: &mut dyn FnMut(Delta)) -> Result<Value> {
        self.requests.borrow_mut().push(body.to_vec());
        let r = self.responses.borrow_mut().pop_front().ok_or_else(|| anyhow!("script exhausted"))?;
        for b in r["content"].as_array().into_iter().flatten() {
            if let Some(t) = b["text"].as_str() {
                on(Delta::Text(t));
            }
        }
        Ok(r)
    }
}

/// Shorthand for scripted assistant messages.
pub fn msg(content: Vec<Value>) -> Value {
    let stop = if content.iter().any(|b| b["type"] == "tool_use") { "tool_use" } else { "end_turn" };
    json!({ "type": "message", "role": "assistant", "content": content, "stop_reason": stop, "usage": {} })
}

pub fn text(t: &str) -> Value {
    json!({ "type": "text", "text": t })
}

pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}
