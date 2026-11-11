//! The agent loop, built on the record. Each step folds the effective chain into messages, renders the
//! exact request, stores its bytes, calls the model and records the response; each tool call is recorded
//! with world snapshots before and after. Resume is re-reading: fold the chain and continue.

use super::fold::{current_task, effective_chain, fold, spawn_config};
use super::model::{Delta, Provider};
use super::{prompt, tools};
use crate::classify::{self, Class, State};
use crate::engine::Engine;
use crate::hook::{shrink, summary};
use crate::rewind::{self, Mode};
use crate::store::{Event, Store};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub max_tokens: u64,
    pub effort: Option<String>,
    /// "summarized" to see thinking; None leaves the model's default (omitted on current models).
    pub thinking_display: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config { model: "claude-opus-5-5".into(), max_tokens: 32_000, effort: None, thinking_display: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Run everything.
    Auto,
    /// Block compensable, irreversible and unknown actions.
    Dry,
    /// Ask the human for those.
    Ask,
}

#[allow(unused_variables)]
pub trait Ui {
    fn text(&mut self, t: &str) {}
    fn thinking(&mut self, t: &str) {}
    fn tool(&mut self, name: &str, summary: &str, class: &Class) {}
    fn tool_done(&mut self, text: &str, is_error: bool) {}
    fn note(&mut self, t: &str) {}
    fn retry(&mut self, t: &str) {}
    fn usage(&mut self, usage: &Value) {}
    fn ask(&mut self, question: &str) -> bool {
        false
    }
}

pub struct Quiet;
impl Ui for Quiet {}

pub struct TurnResult {
    pub text: String,
    pub steps: usize,
    pub stop: String,
}

pub struct Session<'a> {
    pub eng: &'a Engine,
    pub branch: String,
    pub provider: Box<dyn Provider + 'a>,
    pub ui: Box<dyn Ui + 'a>,
    pub perm: Permission,
    pub max_steps: usize,
}

/// Render the exact request for the conversation at the end of `chain`.
pub fn render(store: &Store, chain: &[Event]) -> Result<Vec<u8>> {
    let spawn = spawn_config(chain).context("this branch has no rune session; start one with `rune chat`")?;
    let cfg = &spawn.body["config"];
    let system = String::from_utf8(store.blob(spawn.str("system").unwrap_or_default())?)?;
    let tools: Value = serde_json::from_slice(&store.blob(spawn.str("tools").unwrap_or_default())?)?;
    let folded = fold(store, chain)?;
    let mut body = json!({
        "model": cfg["model"],
        "max_tokens": cfg["max_tokens"],
        "stream": true,
        "system": [{ "type": "text", "text": system }],
        "tools": tools,
        "messages": folded.messages,
        "cache_control": { "type": "ephemeral" },
    });
    if let Some(d) = cfg["thinking_display"].as_str() {
        body["thinking"] = json!({ "type": "adaptive", "display": d });
    }
    if let Some(e) = cfg["effort"].as_str() {
        body["output_config"] = json!({ "effort": e });
    }
    Ok(serde_json::to_vec(&body)?)
}

/// Re-render every recorded request from the record and compare with the stored bytes. A mismatch means
/// the fold or renderer drifted: the record no longer explains what the model saw.
pub fn verify_requests(eng: &Engine, branch: &str) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    for ev in eng.lineage(branch)? {
        if ev.kind != "model.call" {
            continue;
        }
        let Some(parent) = ev.parents.first() else { continue };
        let chain = effective_chain(&eng.store, parent)?;
        let rendered = render(&eng.store, &chain)?;
        let stored = eng.store.blob(ev.str("request").unwrap_or_default())?;
        out.push((ev.short().to_string(), rendered == stored));
    }
    Ok(out)
}

impl<'a> Session<'a> {
    /// Start a session on an empty branch: the system prompt, tool definitions and config are stored once.
    pub fn spawn(eng: &Engine, branch: &str, cfg: &Config, date: &str) -> Result<Event> {
        if let Some(h) = eng.store.head(branch)? {
            let chain = effective_chain(&eng.store, &h)?;
            if spawn_config(&chain).is_some() {
                bail!("branch {branch} already has a session");
            }
        }
        let system = eng.store.put_blob(prompt::system(&eng.world.worktree, date).as_bytes())?;
        let tools = eng.store.put_blob(&serde_json::to_vec(&tools::definitions())?)?;
        eng.record(
            branch,
            "spawn",
            json!({
                "harness": "rune",
                "system": system,
                "tools": tools,
                "config": { "model": cfg.model, "max_tokens": cfg.max_tokens, "effort": cfg.effort, "thinking_display": cfg.thinking_display },
            }),
        )
    }

    pub fn chain(&self) -> Result<Vec<Event>> {
        let head = self.eng.store.head(&self.branch)?.with_context(|| format!("no branch {}", self.branch))?;
        effective_chain(&self.eng.store, &head)
    }

    pub fn turn(&mut self, prompt: &str) -> Result<TurnResult> {
        let ev = self.eng.record(&self.branch, "turn.start", json!({ "harness": "rune", "prompt": prompt }))?;
        if let Some(n) = ev.str("note") {
            self.ui.note(n);
        }
        self.run_loop()
    }

    /// Continue from the current head (after a crash, or after a rewind the human applied).
    pub fn run_loop(&mut self) -> Result<TurnResult> {
        let tmp = self.eng.home.join("tmp");
        for step in 1..=self.max_steps {
            let chain = self.chain()?;
            let folded = fold(&self.eng.store, &chain)?;
            let req = render(&self.eng.store, &chain)?;
            let req_hash = self.eng.store.put_blob(&req)?;
            let ui = &mut self.ui;
            let resp = self.provider.send(&req, &mut |d| match d {
                Delta::Text(t) => ui.text(t),
                Delta::Thinking(t) => ui.thinking(t),
                Delta::Retry(t) => ui.retry(t),
                Delta::ToolUse(_) => {}
            });
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    self.eng.record(&self.branch, "model.error", json!({ "request": req_hash, "error": format!("{e:#}") }))?;
                    return Err(e);
                }
            };
            self.ui.usage(&resp["usage"]);
            let content = resp["content"].clone();
            let text: String = content
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let stop = resp["stop_reason"].as_str().unwrap_or("").to_string();
            self.eng.record(
                &self.branch,
                "model.call",
                json!({
                    "request": req_hash,
                    "response": { "id": resp["id"], "model": resp["model"], "content": content, "stop_reason": stop, "usage": resp["usage"] },
                    "summary": text.chars().take(120).collect::<String>(),
                }),
            )?;
            let uses: Vec<Value> = content.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use").cloned().collect();
            if stop == "pause_turn" {
                continue;
            }
            if uses.is_empty() {
                self.eng.record(&self.branch, "turn.end", json!({ "stop": stop }))?;
                return Ok(TurnResult { text, steps: step, stop });
            }
            let mut ctx = tools::Ctx { worktree: self.eng.world.worktree.clone(), tmp: tmp.clone(), seen: folded.seen, offline: false };
            for tu in uses {
                let (id, name, input) = (tu["id"].as_str().unwrap_or(""), tu["name"].as_str().unwrap_or(""), &tu["input"]);
                if name == "Rewind" {
                    match self.self_rewind(id, input)? {
                        None => break, // rewound: everything after the point, including the rest of this message, is gone
                        Some(reason) => {
                            self.ui.tool(name, &reason, &Class { state: State::Read, disclosure: classify::Disclosure::None, reason: String::new() });
                            self.ui.tool_done(&reason, true);
                            self.eng.record(&self.branch, "tool.post", json!({ "call": id, "tool": name, "summary": "rejected", "result": reason, "is_error": true }))?;
                            continue;
                        }
                    }
                }
                self.call_tool(&mut ctx, id, name, input)?;
            }
        }
        self.eng.record(&self.branch, "turn.end", json!({ "stop": "max_steps" }))?;
        Ok(TurnResult { text: format!("Stopped after {} steps.", self.max_steps), steps: self.max_steps, stop: "max_steps".into() })
    }

    fn call_tool(&mut self, ctx: &mut tools::Ctx, id: &str, name: &str, input: &Value) -> Result<()> {
        let sum = summary(name, input);
        let mut class = classify::tool(name, input);
        if matches!(name, "Write" | "Edit") {
            if let Some(p) = input["file_path"].as_str() {
                if !ctx.inside(&ctx.resolve(p)) {
                    class = Class { state: State::LocalOutside, disclosure: class.disclosure, reason: format!("{name} outside the worktree") };
                }
            }
        }
        if input.get("__unparsed").is_some() {
            let msg = "The tool input was not valid JSON (the response may have hit max_tokens). Retry with a smaller call.";
            self.eng.record(&self.branch, "tool.post", json!({ "call": id, "tool": name, "summary": sum, "result": msg, "is_error": true }))?;
            return Ok(());
        }
        // Dry world: known outside-world actions are blocked; commands we cannot classify run with no
        // network, so they can change the workspace but cannot reach anything outside it.
        let mut offline = false;
        let blocked = if class.gated() {
            match self.perm {
                Permission::Auto => None,
                Permission::Dry if class.state == State::Unknown && name == "Bash" && tools::offline_available() => {
                    offline = true;
                    None
                }
                Permission::Dry => Some(format!(
                    "Blocked: `{sum}` is {:?} ({}). Actions that reach outside the local workspace are disabled in this session. Continue without it, and tell the user what you would have run.",
                    class.state, class.reason
                )),
                Permission::Ask => {
                    let q = format!("Allow {name} `{sum}`? ({:?}/{:?}: {})", class.state, class.disclosure, class.reason);
                    if self.ui.ask(&q) {
                        None
                    } else {
                        Some(format!("The user declined `{sum}`. Do not retry it; continue without it or ask the user."))
                    }
                }
            }
        } else if self.perm == Permission::Dry && name == "Bash" && class.disclosure == classify::Disclosure::External {
            // e.g. curl GET, npm install: local by state, but data would leave. Offline if we can, else block.
            if tools::offline_available() {
                offline = true;
                None
            } else {
                Some(format!("Blocked: `{sum}` would send data outside ({}). Network access is disabled in this session.", class.reason))
            }
        } else {
            None
        };
        self.ui.tool(name, &sum, &class);
        if let Some(msg) = blocked {
            self.ui.tool_done(&msg, true);
            self.eng.record(
                &self.branch,
                "tool.blocked",
                json!({ "call": id, "tool": name, "summary": sum, "class": class, "result": msg, "is_error": true }),
            )?;
            return Ok(());
        }
        let mut pre = json!({ "call": id, "tool": name, "summary": sum, "input": shrink(input), "class": class });
        if offline {
            pre["sandbox"] = json!("offline");
        }
        self.eng.record(&self.branch, "tool.pre", pre)?;
        ctx.offline = offline;
        let out = ctx.run(name, input);
        ctx.offline = false;
        for (k, v) in &out.seen {
            ctx.seen.insert(k.clone(), v.clone());
        }
        self.ui.tool_done(&out.text, out.is_error);
        let mut body = json!({ "call": id, "tool": name, "summary": sum, "is_error": out.is_error, "exit": out.exit, "seen": out.seen });
        if out.text.len() > 16_384 {
            body["result_blob"] = json!(self.eng.store.put_blob(out.text.as_bytes())?);
        } else {
            body["result"] = json!(out.text);
        }
        self.eng.record(&self.branch, "tool.post", body)?;
        Ok(())
    }

    /// Returns None when the rewind happened, or Some(reason) when it was rejected.
    fn self_rewind(&mut self, call: &str, input: &Value) -> Result<Option<String>> {
        let chain = self.chain()?;
        let Some((_, task)) = current_task(&chain) else { return Ok(Some("There is no request to rewind within.".into())) };
        let lesson = input["lesson"].as_str().unwrap_or("").trim().to_string();
        let evidence = input["evidence"].as_str().unwrap_or("").trim().to_string();
        if lesson.is_empty() || evidence.is_empty() {
            return Ok(Some("Rewind needs both a lesson and the evidence behind it.".into()));
        }
        if lesson.chars().count() > 300 {
            return Ok(Some("The lesson is longer than 300 characters. Shorten it to what failed and what to do instead.".into()));
        }
        let done = self
            .eng
            .lineage(&self.branch)?
            .iter()
            .filter(|e| e.kind == "world.rewind" && e.str("task") == Some(&task) && e.body.get("lesson").is_some())
            .count();
        if done >= 3 {
            return Ok(Some("You have already rewound 3 times for this request. Continue, or tell the user what is blocking you.".into()));
        }
        let is_task_start = |e: &Event| (e.kind == "turn.start" && e.id == task) || (e.kind == "world.rewind" && e.str("task") == Some(&task));
        let target_idx = match input["point"].as_str().unwrap_or("request_start") {
            "before_call" => {
                let id = input["call_id"].as_str().unwrap_or("");
                let found = chain.iter().position(|e| {
                    e.kind == "model.call"
                        && e.body["response"]["content"].as_array().into_iter().flatten().any(|b| b["type"] == "tool_use" && b["id"] == id)
                });
                match found {
                    Some(i) if i > 0 => i - 1,
                    _ => return Ok(Some(format!("No earlier tool call with id {id:?} in this conversation."))),
                }
            }
            _ => match chain.iter().rposition(|e| is_task_start(e)) {
                Some(i) if chain[i].kind == "turn.start" => i - 1,
                Some(i) => i,
                None => return Ok(Some("Could not find the start of the current request.".into())),
            },
        };
        let target = chain[target_idx].clone();
        let plan = rewind::plan(self.eng, &self.branch, &target, Mode::KeepHuman)?;
        if !plan.external.is_empty() {
            let list: Vec<String> = plan.external.iter().map(|e| e.label.clone()).collect();
            let q = format!("The agent wants to rewind past actions that stay live outside: {}. Allow?", list.join("; "));
            if !(self.perm == Permission::Ask && self.ui.ask(&q)) {
                return Ok(Some(format!(
                    "Cannot rewind past actions that reached the outside world ({}). Pick a later point, or continue.",
                    list.join("; ")
                )));
            }
        }
        let has_prompt = chain[..=target_idx].iter().any(|e| is_task_start(e));
        let prompt = if has_prompt { None } else { self.eng.store.get(&task)?.str("prompt").map(str::to_string) };
        let cur_turn = chain.last().and_then(Event::turn).unwrap_or(0);
        let t_turn = target.turn().unwrap_or(0);
        let turn = if t_turn < cur_turn { t_turn + 1 } else { cur_turn };
        let note = rewind::note(&plan, Some((&lesson, &evidence)), prompt.as_deref());
        rewind::apply_with(
            self.eng,
            &plan,
            true,
            json!({ "note": note, "lesson": lesson, "evidence": evidence, "task": task, "turn": turn, "call": call, "self": true }),
        )?;
        self.ui.note(&note);
        Ok(None)
    }
}
