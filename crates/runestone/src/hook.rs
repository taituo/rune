//! Hook ingestion for existing harnesses (Claude Code, Kiro CLI). Each hook gets a JSON document on stdin.
//!
//! Fail open for the agent, closed for the record: any error exits 0 with no output, but a `gap` event is
//! written when the store is reachable, so the missing snapshot is visible.

use crate::classify;
use crate::engine::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

impl Outcome {
    fn ok() -> Self {
        Outcome { stdout: String::new(), stderr: String::new(), exit: 0 }
    }
}

/// Map a harness event name to a lifecycle kind.
pub fn kind_of(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().replace(['_', '-'], "").as_str() {
        "sessionstart" | "agentspawn" => Some("spawn"),
        "userpromptsubmit" => Some("turn.start"),
        "pretooluse" => Some("tool.pre"),
        "posttooluse" => Some("tool.post"),
        "stop" | "subagentstop" => Some("turn.end"),
        _ => None,
    }
}

fn s<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// One-line summary of a tool input: the command or the file path. Stored in the record, not injected.
pub fn summary(tool: &str, input: &Value) -> String {
    if let Some(c) = s(input, &["command", "cmd"]) {
        return clip(c.lines().next().unwrap_or(""), 200);
    }
    if let Some(p) = s(input, &["file_path", "path", "notebook_path", "pattern", "url", "query"]) {
        return clip(p, 200);
    }
    clip(&serde_json::to_string(input).unwrap_or_default(), 120).replace(tool, "")
}

/// Truncate large strings in a tool input before it goes into the record.
pub fn shrink(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() > 4096 => json!(format!("{}… [{} bytes]", clip(s, 1024), s.len())),
        Value::Array(a) => Value::Array(a.iter().map(shrink).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), shrink(v))).collect()),
        x => x.clone(),
    }
}

pub fn handle(raw: &str, harness: &str) -> Outcome {
    let input: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Outcome::ok(),
    };
    let cwd = s(&input, &["cwd"]).map(PathBuf::from).or_else(|| std::env::current_dir().ok());
    let Some(cwd) = cwd else { return Outcome::ok() };
    let Ok(eng) = Engine::open(&cwd) else { return Outcome::ok() };
    let session = s(&input, &["session_id", "sessionId", "conversation_id"]).unwrap_or("default").to_string();
    let branch = eng.branch(&format!("s/{session}"));
    match run(&eng, &branch, &input, harness, &session) {
        Ok(o) => o,
        Err(e) => {
            let _ = eng.store.append(&branch, "gap", |_| Ok(json!({ "error": e.to_string(), "author": "unknown" })));
            Outcome::ok()
        }
    }
}

fn run(eng: &Engine, branch: &str, input: &Value, harness: &str, session: &str) -> anyhow::Result<Outcome> {
    let name = s(input, &["hook_event_name", "hookEventName", "event"]).unwrap_or("");
    let Some(kind) = kind_of(name) else { return Ok(Outcome::ok()) };
    let tool = s(input, &["tool_name", "toolName"]).unwrap_or("").to_string();
    let tool_input = input.get("tool_input").or_else(|| input.get("toolInput")).cloned().unwrap_or(Value::Null);
    let call = s(input, &["tool_use_id", "toolUseId", "call_id"]).map(str::to_string);
    let mut extra = json!({ "harness": harness, "session": session });
    if !tool.is_empty() {
        extra["tool"] = json!(tool);
        extra["summary"] = json!(summary(&tool, &tool_input));
        extra["input"] = shrink(&tool_input);
    }
    if let Some(c) = &call {
        extra["call"] = json!(c);
    }
    if let Some(p) = s(input, &["prompt"]) {
        extra["prompt"] = json!(clip(p, 2000));
    }
    if kind == "tool.post" {
        if let Some(r) = input.get("tool_response").or_else(|| input.get("tool_output")) {
            extra["exit"] = r.get("exit_code").or_else(|| r.get("exitCode")).cloned().unwrap_or(Value::Null);
        }
    }

    let mut out = Outcome::ok();
    if kind == "tool.pre" {
        let cls = classify::tool(&tool, &tool_input);
        extra["class"] = serde_json::to_value(&cls)?;
        if dry_world() && cls.gated() {
            eng.record(branch, "tool.blocked", extra)?;
            out.exit = 2;
            out.stderr = format!(
                "Blocked by rune (dry world): `{}` is {:?}: {}. Queued for a human; continue without it.",
                summary(&tool, &tool_input),
                cls.state,
                cls.reason
            );
            return Ok(out);
        }
    }
    let ev = eng.record(branch, kind, extra)?;
    if kind == "turn.start" {
        if let Some(note) = eng.outside_note(&ev) {
            out.stdout = note;
        }
    }
    Ok(out)
}

fn dry_world() -> bool {
    std::env::var("RUNE_DRY_WORLD").map(|v| v == "1" || v == "true").unwrap_or(false)
}
