//! OpenAI-compatible chat/completions: request rendering and a streaming provider.
//!
//! The record keeps one conversation format (Anthropic-style content blocks). A session that speaks this
//! API renders its requests here, deterministically, so the stored bytes are exactly what was sent and
//! `rune verify` still holds. Responses are converted back into the same block format, so folding,
//! tools, rewind and checks do not care which API produced them.
//!
//! Mapping:
//! - text blocks → message `content`; thinking blocks are not sent (this API has no place for them)
//! - assistant `tool_use` → `tool_calls` (arguments as a JSON string)
//! - user `tool_result` → one `tool` message per result, placed right after the assistant message; any
//!   text in the same user turn (harness notes, the next prompt) follows as a user message
//! - `is_error` results are prefixed with "Error: ", since this API has no error flag

use super::model::{Delta, Provider};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::io::BufRead;
use std::time::Duration;

pub fn render(cfg: &Value, system: &str, tools: &Value, messages: &[Value]) -> Value {
    let mut out = vec![json!({ "role": "system", "content": system })];
    for m in messages {
        let blocks = m["content"].as_array().cloned().unwrap_or_default();
        match m["role"].as_str() {
            Some("assistant") => {
                let text: Vec<&str> = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                let calls: Vec<Value> = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_use")
                    .map(|b| {
                        json!({
                            "id": b["id"],
                            "type": "function",
                            "function": { "name": b["name"], "arguments": serde_json::to_string(&b["input"]).unwrap_or_default() }
                        })
                    })
                    .collect();
                let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text.join("\n")) } });
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                out.push(msg);
            }
            _ => {
                let mut texts = Vec::new();
                for b in &blocks {
                    match b["type"].as_str() {
                        Some("tool_result") => {
                            let content = match &b["content"] {
                                Value::String(s) => s.clone(),
                                Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"),
                                _ => String::new(),
                            };
                            let content = if b["is_error"].as_bool() == Some(true) { format!("Error: {content}") } else { content };
                            out.push(json!({ "role": "tool", "tool_call_id": b["tool_use_id"], "content": content }));
                        }
                        Some("text") => texts.push(b["text"].as_str().unwrap_or("").to_string()),
                        _ => {}
                    }
                }
                if !texts.is_empty() {
                    out.push(json!({ "role": "user", "content": texts.join("\n\n") }));
                }
            }
        }
    }
    let tools: Vec<Value> = tools
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| json!({ "type": "function", "function": { "name": t["name"], "description": t["description"], "parameters": t["input_schema"] } }))
        .collect();
    let mut body = json!({
        "model": cfg["model"],
        "max_tokens": cfg["max_tokens"],
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": out,
        "tools": tools,
    });
    if let Some(e) = cfg["effort"].as_str() {
        // low | medium | high are understood by most compatible servers; others pass through as given
        body["reasoning_effort"] = json!(e);
    }
    body
}

pub struct OpenAi {
    pub url: String,
    pub api_key: Option<String>,
    agent: ureq::Agent,
}

/// `base` may or may not end in /v1.
pub fn endpoint(base: &str, path: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{b}/{path}")
    } else {
        format!("{b}/v1/{path}")
    }
}

impl OpenAi {
    pub fn new(base_url: &str, api_key: Option<String>, proxy_from_env: bool) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(600))
            .try_proxy_from_env(proxy_from_env)
            .build();
        OpenAi { url: endpoint(base_url, "chat/completions"), api_key, agent }
    }

    pub fn from_env(base_url: Option<&str>) -> Result<Self> {
        let base = base_url
            .map(str::to_string)
            .or_else(|| std::env::var("RUNE_BASE_URL").ok())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .context("no base URL: pass --base-url or set RUNE_BASE_URL")?;
        let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
        // A gateway on the local network should not go through an HTTP proxy.
        let local = base.contains("://10.") || base.contains("://192.168.") || base.contains("://127.") || base.contains("://localhost");
        Ok(Self::new(&base, key, !local))
    }

    fn once(&self, body: &[u8], on: &mut dyn FnMut(Delta)) -> std::result::Result<Value, (bool, anyhow::Error)> {
        let mut req = self.agent.post(&self.url).set("content-type", "application/json").set("accept", "text/event-stream");
        if let Some(k) = &self.api_key {
            req = req.set("authorization", &format!("Bearer {k}"));
        }
        let resp = match req.send_bytes(body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                return Err((matches!(code, 408 | 409 | 429 | 500 | 502 | 503 | 504), anyhow!("HTTP {code} from {}: {}", self.url, text.trim())));
            }
            Err(e) => return Err((true, anyhow!("transport: {e}"))),
        };
        let is_json = resp.content_type().contains("json");
        let reader = std::io::BufReader::new(resp.into_reader());
        if is_json {
            // A server that ignored "stream": one JSON response.
            let v: Value = serde_json::from_reader(reader).map_err(|e| (false, anyhow!("bad JSON response: {e}")))?;
            return from_message(&v, on).map_err(|e| (false, e));
        }
        let mut acc = Acc::default();
        for line in reader.lines() {
            let line = line.map_err(|e| (true, anyhow!("stream: {e}")))?;
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };
            if ev.get("error").is_some() {
                return Err((true, anyhow!("stream error: {}", ev["error"])));
            }
            acc.feed(&ev, on);
        }
        acc.finish().map_err(|e| (true, e))
    }
}

impl Provider for OpenAi {
    fn send(&self, body: &[u8], on: &mut dyn FnMut(Delta)) -> Result<Value> {
        let mut delay = 2u64;
        for attempt in 1..=5 {
            match self.once(body, on) {
                Ok(v) => return Ok(v),
                Err((true, e)) if attempt < 5 => {
                    on(Delta::Retry(&format!("{e} — retrying in {delay}s")));
                    std::thread::sleep(Duration::from_secs(delay));
                    delay = (delay * 2).min(30);
                }
                Err((_, e)) => return Err(e),
            }
        }
        bail!("unreachable")
    }
}

fn stop_reason(finish: &str) -> &'static str {
    match finish {
        "tool_calls" | "function_call" => "tool_use",
        "length" => "max_tokens",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

/// Gateways differ: chat/completions names (prompt_tokens, prompt_tokens_details) or Responses-API names
/// (input_tokens, input_tokens_details). Read both.
fn usage(u: &Value) -> Value {
    let prompt = u["prompt_tokens"].as_u64().or_else(|| u["input_tokens"].as_u64()).unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| u["input_tokens_details"]["cached_tokens"].as_u64())
        .unwrap_or(0);
    json!({
        "input_tokens": prompt.saturating_sub(cached),
        "cache_read_input_tokens": cached,
        "output_tokens": u["completion_tokens"].as_u64().or_else(|| u["output_tokens"].as_u64()).unwrap_or(0),
    })
}

fn parse_args(name: &str, args: &str) -> Value {
    if args.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(args).unwrap_or_else(|_| json!({ "__unparsed": args, "__tool": name }))
}

/// Non-streaming response → internal message.
fn from_message(v: &Value, on: &mut dyn FnMut(Delta)) -> Result<Value> {
    let choice = &v["choices"][0];
    let m = &choice["message"];
    let mut content = Vec::new();
    if let Some(r) = m["reasoning_content"].as_str().filter(|s| !s.is_empty()) {
        on(Delta::Thinking(r));
    }
    if let Some(t) = m["content"].as_str().filter(|s| !s.is_empty()) {
        on(Delta::Text(t));
        content.push(json!({ "type": "text", "text": t }));
    }
    for (i, c) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
        let name = c["function"]["name"].as_str().unwrap_or("");
        let id = c["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("call_{i}"));
        content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": parse_args(name, c["function"]["arguments"].as_str().unwrap_or("")) }));
    }
    Ok(json!({
        "id": v["id"], "model": v["model"], "type": "message", "role": "assistant", "content": content,
        "stop_reason": stop_reason(choice["finish_reason"].as_str().unwrap_or("stop")),
        "usage": usage(&v["usage"]),
    }))
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct Acc {
    id: Value,
    model: Value,
    text: String,
    calls: std::collections::BTreeMap<u64, Call>,
    finish: Option<String>,
    usage: Value,
    seen_any: bool,
}

impl Acc {
    fn feed(&mut self, ev: &Value, on: &mut dyn FnMut(Delta)) {
        self.seen_any = true;
        if self.id.is_null() {
            self.id = ev["id"].clone();
            self.model = ev["model"].clone();
        }
        if ev["usage"].is_object() {
            self.usage = ev["usage"].clone();
        }
        let Some(choice) = ev["choices"].get(0) else { return };
        let d = &choice["delta"];
        for key in ["reasoning_content", "reasoning"] {
            if let Some(r) = d[key].as_str().filter(|s| !s.is_empty()) {
                on(Delta::Thinking(r));
            }
        }
        if let Some(t) = d["content"].as_str().filter(|s| !s.is_empty()) {
            on(Delta::Text(t));
            self.text.push_str(t);
        }
        for c in d["tool_calls"].as_array().into_iter().flatten() {
            let idx = c["index"].as_u64().unwrap_or(self.calls.len() as u64);
            let entry = self.calls.entry(idx).or_default();
            if let Some(id) = c["id"].as_str() {
                entry.id = id.to_string();
            }
            if let Some(n) = c["function"]["name"].as_str() {
                if entry.name.is_empty() {
                    on(Delta::ToolUse(n));
                }
                entry.name.push_str(n);
            }
            if let Some(a) = c["function"]["arguments"].as_str() {
                entry.args.push_str(a);
            }
        }
        if let Some(f) = choice["finish_reason"].as_str() {
            self.finish = Some(f.to_string());
        }
    }

    fn finish(self) -> Result<Value> {
        if !self.seen_any {
            bail!("empty stream");
        }
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(json!({ "type": "text", "text": self.text }));
        }
        for (i, c) in &self.calls {
            let id = if c.id.is_empty() { format!("call_{i}") } else { c.id.clone() };
            content.push(json!({ "type": "tool_use", "id": id, "name": c.name, "input": parse_args(&c.name, &c.args) }));
        }
        let finish = self.finish.unwrap_or_else(|| if self.calls.is_empty() { "stop".into() } else { "tool_calls".into() });
        let mut m = Map::new();
        m.insert("id".into(), self.id);
        m.insert("model".into(), self.model);
        m.insert("type".into(), json!("message"));
        m.insert("role".into(), json!("assistant"));
        m.insert("content".into(), json!(content));
        m.insert("stop_reason".into(), json!(stop_reason(&finish)));
        m.insert("usage".into(), usage(&self.usage));
        Ok(Value::Object(m))
    }
}

/// List model ids served at `base` (GET /v1/models).
pub fn list_models(base: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    let local = base.contains("://10.") || base.contains("://192.168.") || base.contains("://127.") || base.contains("://localhost");
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).try_proxy_from_env(!local).build();
    let mut req = agent.get(&endpoint(base, "models"));
    if let Some(k) = api_key {
        req = req.set("authorization", &format!("Bearer {k}"));
    }
    let v: Value = serde_json::from_reader(req.call().map_err(|e| anyhow!("GET models: {e}"))?.into_reader())?;
    Ok(v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect())
}
