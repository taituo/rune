//! The conversation is a fold over the record. There is no separate message store: what the model sees
//! next is computed from the events on the effective chain, so rewinding the record rewinds the
//! conversation and the world together.
//!
//! The effective chain follows first parents, except at a `world.rewind`: from there it jumps to the
//! rewind's target (or, for an undo, to the parent of the undone rewind). The rewind's note is appended
//! after the target's messages, so history stays append-only from the target on, which keeps prompt
//! caching and thinking-block signatures valid.

use crate::store::{Event, Store};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn effective_chain(store: &Store, head: &str) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    let mut cur = Some(head.to_string());
    while let Some(id) = cur {
        let ev = store.get(&id)?;
        cur = if ev.kind == "world.rewind" {
            if let Some(t) = ev.str("target") {
                Some(t.to_string())
            } else if let Some(u) = ev.str("undo") {
                store.get(u)?.parents.first().cloned()
            } else {
                ev.parents.first().cloned()
            }
        } else {
            ev.parents.first().cloned()
        };
        out.push(ev);
    }
    out.reverse();
    Ok(out)
}

/// Session configuration written at spawn and read back on every request (exact, not re-derived).
pub fn spawn_config(chain: &[Event]) -> Option<&Event> {
    chain.iter().rev().find(|e| e.kind == "spawn" && e.body.get("system").is_some())
}

fn result_text(store: &Store, ev: &Event) -> Result<String> {
    if let Some(h) = ev.str("result_blob") {
        return Ok(String::from_utf8_lossy(&store.blob(h)?).into_owned());
    }
    Ok(ev.str("result").unwrap_or("").to_string())
}

pub struct Folded {
    pub messages: Vec<Value>,
    /// path -> content hash the agent last saw (read or wrote). Drives the read-before-write guard.
    pub seen: HashMap<String, String>,
}

pub fn fold(store: &Store, chain: &[Event]) -> Result<Folded> {
    let mut m = Msgs::default();
    let mut seen = HashMap::new();
    for ev in chain {
        match ev.kind.as_str() {
            "turn.start" => {
                let mut blocks = Vec::new();
                if let Some(n) = ev.str("note") {
                    blocks.push(json!({ "type": "text", "text": n }));
                }
                if let Some(p) = ev.str("prompt") {
                    blocks.push(json!({ "type": "text", "text": p }));
                }
                if !blocks.is_empty() {
                    m.user(blocks);
                }
            }
            "check" => {
                if let Some(n) = ev.str("note") {
                    m.user(vec![json!({ "type": "text", "text": n })]);
                }
            }
            "world.rewind" => {
                if let Some(n) = ev.str("note") {
                    m.user(vec![json!({ "type": "text", "text": n })]);
                }
                // The restored world differs from what the agent last saw; force fresh reads.
                seen.clear();
            }
            "model.call" => {
                if let Some(content) = ev.body.get("response").and_then(|r| r.get("content")) {
                    m.assistant(content.clone());
                }
            }
            "tool.post" | "tool.blocked" => {
                let (Some(call), true) = (ev.str("call"), ev.body.get("result").is_some() || ev.body.get("result_blob").is_some()) else {
                    continue;
                };
                let is_error = ev.body.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                m.tool_result(call, &result_text(store, ev)?, is_error);
                if let Some(Value::Object(s)) = ev.body.get("seen") {
                    for (k, v) in s {
                        if let Some(v) = v.as_str() {
                            seen.insert(k.clone(), v.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Folded { messages: m.done(), seen })
}

#[derive(Default)]
struct Msgs {
    v: Vec<Value>,
}

impl Msgs {
    fn last_role(&self) -> Option<&str> {
        self.v.last().and_then(|m| m["role"].as_str())
    }

    /// tool_use ids in the last assistant message that have no result yet.
    fn dangling(&self) -> Vec<String> {
        let Some(last) = self.v.last() else { return vec![] };
        if last["role"] != "assistant" {
            return vec![];
        }
        last["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| b["id"].as_str().map(String::from))
            .collect()
    }

    fn push_user_blocks(&mut self, blocks: Vec<Value>) {
        if self.last_role() == Some("user") {
            if let Some(Value::Array(c)) = self.v.last_mut().map(|m| &mut m["content"]) {
                c.extend(blocks);
                return;
            }
        }
        self.v.push(json!({ "role": "user", "content": blocks }));
    }

    fn user(&mut self, blocks: Vec<Value>) {
        // A conversation cut mid-call (crash, rewind to inside a turn) leaves tool_use without results.
        let missing: Vec<Value> = self
            .dangling()
            .into_iter()
            .map(|id| json!({ "type": "tool_result", "tool_use_id": id, "content": "Interrupted: this call did not complete.", "is_error": true }))
            .collect();
        if !missing.is_empty() {
            self.push_user_blocks(missing);
        }
        self.push_user_blocks(blocks);
    }

    fn assistant(&mut self, content: Value) {
        self.v.push(json!({ "role": "assistant", "content": content }));
    }

    fn tool_result(&mut self, call: &str, text: &str, is_error: bool) {
        let mut b = json!({ "type": "tool_result", "tool_use_id": call, "content": text });
        if is_error {
            b["is_error"] = json!(true);
        }
        self.push_user_blocks(vec![b]);
    }

    fn done(self) -> Vec<Value> {
        self.v
    }
}

/// The current task: the latest user request, or the request a self-rewind is still working on.
pub fn current_task(chain: &[Event]) -> Option<(usize, String)> {
    chain.iter().enumerate().rev().find_map(|(i, e)| match e.kind.as_str() {
        "turn.start" => Some((i, e.id.clone())),
        "world.rewind" => e.str("task").map(|t| (i, t.to_string())),
        _ => None,
    })
}
