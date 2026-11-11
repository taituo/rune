use runestone::agent::fold::{effective_chain, fold};
use runestone::agent::model::{msg, text, tool_use, Scripted};
use runestone::agent::{verify_requests, Config, Permission, Quiet, Session};
use runestone::rewind::{self, Mode};
use runestone::Engine;
use serde_json::{json, Value};
use std::fs;
use std::rc::Rc;

struct Shared(Rc<Scripted>);
impl runestone::agent::model::Provider for Shared {
    fn send(&self, body: &[u8], on: &mut dyn FnMut(runestone::agent::model::Delta)) -> anyhow::Result<Value> {
        self.0.send(body, on)
    }
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Engine) {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    fs::write(wt.path().join("src.txt"), "hello\n").unwrap();
    let eng = Engine::open_in(home.path(), wt.path()).unwrap();
    (home, wt, eng)
}

fn session<'a>(eng: &'a Engine, script: &Rc<Scripted>, perm: Permission) -> Session<'a> {
    let b = eng.branch("s/t");
    if eng.store.head(&b).unwrap().is_none() {
        Session::spawn(eng, &b, &Config::default(), "2026-11-11").unwrap();
    }
    Session { eng, branch: b, provider: Box::new(Shared(script.clone())), ui: Box::new(Quiet), perm, max_steps: 50 }
}

fn request(script: &Scripted, i: usize) -> Value {
    serde_json::from_slice(&script.requests.borrow()[i]).unwrap()
}

fn all_results(req: &Value) -> Vec<(String, bool)> {
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| (b["content"].as_str().unwrap_or("").to_string(), b["is_error"].as_bool().unwrap_or(false)))
        .collect()
}

#[test]
fn tool_loop_and_exact_rerender() {
    let (_h, wt, eng) = setup();
    let f = wt.path().join("src.txt");
    let script = Rc::new(Scripted::new(vec![
        msg(vec![text("Reading."), tool_use("t1", "Read", json!({ "file_path": f }))]),
        msg(vec![tool_use("t2", "Edit", json!({ "file_path": "src.txt", "old_string": "hello", "new_string": "world" }))]),
        msg(vec![tool_use("t3", "Bash", json!({ "command": "cat src.txt" }))]),
        msg(vec![text("Done: hello is now world.")]),
    ]));
    let mut s = session(&eng, &script, Permission::Dry);
    let r = s.turn("change hello to world").unwrap();
    assert_eq!(r.text, "Done: hello is now world.");
    assert_eq!(fs::read_to_string(&f).unwrap(), "world\n");

    let req0 = request(&script, 0);
    assert_eq!(req0["model"], "claude-opus-5-5");
    assert_eq!(req0["cache_control"]["type"], "ephemeral");
    assert_eq!(req0["messages"][0]["content"][0]["text"], "change hello to world");
    assert!(req0["tools"].as_array().unwrap().iter().any(|t| t["name"] == "Rewind"));

    let req3 = request(&script, 3);
    let results = all_results(&req3);
    assert_eq!(results.len(), 3);
    assert!(results[0].0.contains("hello"));
    assert!(results[2].0.contains("world"));
    // History is append-only: each request extends the previous one.
    let m2 = request(&script, 2)["messages"].as_array().unwrap().clone();
    let m3 = req3["messages"].as_array().unwrap().clone();
    assert_eq!(&m3[..m2.len()], &m2[..]);

    // Every request can be rebuilt byte for byte from the record.
    let v = verify_requests(&eng, &s.branch).unwrap();
    assert_eq!(v.len(), 4);
    assert!(v.iter().all(|x| x.1));

    // The Edit is attributed to the agent, with its call id.
    let blame = eng.blame(&s.branch, "src.txt").unwrap();
    assert_eq!(blame[0].0.str("author"), Some("agent"));
    assert_eq!(blame[0].0.str("call"), Some("t2"));
}

#[test]
fn read_before_edit_and_stale_read_guard() {
    let (_h, wt, eng) = setup();
    let d = wt.path().to_path_buf();
    let d2 = d.clone();
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("e0", "Edit", json!({ "file_path": "src.txt", "old_string": "hello", "new_string": "x" }))]),
        msg(vec![tool_use("r1", "Read", json!({ "file_path": "src.txt" }))]),
        msg(vec![tool_use("b1", "Bash", json!({ "command": "true" }))]),
        msg(vec![tool_use("e1", "Edit", json!({ "file_path": "src.txt", "old_string": "hello", "new_string": "x" }))]),
        msg(vec![text("stopping")]),
    ]));
    // A human edits the file while the agent works (between Read and Edit): simulate via the Bash call.
    let script2 = script.clone();
    {
        let mut resp = script2.responses.borrow_mut();
        resp[2] = msg(vec![tool_use("b1", "Bash", json!({ "command": format!("echo human > {}", d2.join("src.txt").display()) }))]);
    }
    let mut s = session(&eng, &script, Permission::Auto);
    s.turn("edit it").unwrap();
    let last = request(&script, 4);
    let results = all_results(&last);
    assert!(results[0].1 && results[0].0.contains("have not read it"), "{:?}", results[0]);
    assert!(results[3].1 && results[3].0.contains("changed since you last read it"), "{:?}", results[3]);
    assert_eq!(fs::read_to_string(d.join("src.txt")).unwrap(), "human\n");
}

#[test]
fn self_rewind_restores_world_and_conversation() {
    let (_h, wt, eng) = setup();
    let d = wt.path().to_path_buf();
    let script = Rc::new(Scripted::new(vec![
        // turn 1
        msg(vec![tool_use("w1", "Write", json!({ "file_path": "a.txt", "content": "v1\n" }))]),
        msg(vec![text("Wrote a.txt.")]),
        // turn 2: a bad path, then a rewind
        msg(vec![tool_use("e2", "Edit", json!({ "file_path": "a.txt", "old_string": "v1", "new_string": "broken" }))]),
        msg(vec![tool_use("t2", "Bash", json!({ "command": "grep -qx v1 a.txt" }))]),
        msg(vec![tool_use("rw", "Rewind", json!({ "lesson": "Editing a.txt breaks the check; leave it and add b2.txt instead.", "evidence": "grep -qx v1 a.txt exited 1" }))]),
        // after the rewind
        msg(vec![tool_use("w3", "Write", json!({ "file_path": "b2.txt", "content": "ok\n" }))]),
        msg(vec![text("Added b2.txt; a.txt untouched.")]),
    ]));
    let mut s = session(&eng, &script, Permission::Dry);
    s.turn("create a.txt").unwrap();
    fs::write(d.join("human.txt"), "mine\n").unwrap(); // human, between turns
    let r = s.turn("make the check pass").unwrap();
    assert_eq!(r.text, "Added b2.txt; a.txt untouched.");

    assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "v1\n"); // agent's bad edit reverted
    assert!(d.join("human.txt").exists()); // human's work kept
    assert!(d.join("b2.txt").exists());

    // The request after the rewind: no trace of the failed path; the note carries lesson and prompt.
    let after = request(&script, 5);
    let raw = serde_json::to_string(&after).unwrap();
    assert!(!raw.contains("broken"), "failed path leaked into context");
    let last_user = after["messages"].as_array().unwrap().last().unwrap();
    let note = last_user["content"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap();
    assert!(note.contains("Lesson: Editing a.txt breaks"), "{note}");
    assert!(note.contains("make the check pass"), "prompt must be repeated: {note}");
    assert!(note.contains("human.txt"), "kept outside changes are reported: {note}");

    // Exactness holds across the rewind.
    assert!(verify_requests(&eng, &s.branch).unwrap().iter().all(|x| x.1));
    let rw = eng.lineage(&s.branch).unwrap().into_iter().find(|e| e.kind == "world.rewind").unwrap();
    assert_eq!(rw.turn(), Some(2));
    assert_eq!(rw.str("lesson").map(|l| l.starts_with("Editing")), Some(true));
}

#[test]
fn rewind_rejects_bad_lessons_and_limits() {
    let (_h, _wt, eng) = setup();
    let long = "x".repeat(301);
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("r0", "Rewind", json!({ "lesson": "no evidence", "evidence": "" }))]),
        msg(vec![tool_use("r1", "Rewind", json!({ "lesson": long, "evidence": "e" }))]),
        msg(vec![tool_use("r2", "Rewind", json!({ "point": "before_call", "call_id": "nope", "lesson": "l", "evidence": "e" }))]),
        msg(vec![text("ok")]),
    ]));
    let mut s = session(&eng, &script, Permission::Dry);
    s.turn("x").unwrap();
    let res = all_results(&request(&script, 3));
    assert!(res[0].0.contains("lesson and the evidence"));
    assert!(res[1].0.contains("300 characters"));
    assert!(res[2].0.contains("No earlier tool call"));
}

#[test]
fn dry_world_blocks_external_actions() {
    let (_h, _wt, eng) = setup();
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("p", "Bash", json!({ "command": "git push origin main" }))]),
        msg(vec![text("I could not push.")]),
    ]));
    let mut s = session(&eng, &script, Permission::Dry);
    s.turn("ship it").unwrap();
    let res = all_results(&request(&script, 1));
    assert!(res[0].1 && res[0].0.starts_with("Blocked"), "{:?}", res[0]);
    assert!(eng.lineage(&s.branch).unwrap().iter().any(|e| e.kind == "tool.blocked"));
    assert!(verify_requests(&eng, &s.branch).unwrap().iter().all(|x| x.1));
}

#[test]
fn dry_world_runs_unknown_commands_offline() {
    if !runestone::agent::tools::offline_available() {
        eprintln!("skipping: no unprivileged user namespaces here");
        return;
    }
    let (_h, wt, eng) = setup();
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("s", "Bash", json!({ "command": "./gen.sh && curl -sS -m 3 https://example.com >/dev/null && echo NET || echo NONET" }))]),
        msg(vec![text("done")]),
    ]));
    fs::write(wt.path().join("gen.sh"), "#!/bin/sh\necho generated > out.txt\n").unwrap();
    std::process::Command::new("chmod").args(["+x", wt.path().join("gen.sh").to_str().unwrap()]).status().unwrap();
    let mut s = session(&eng, &script, Permission::Dry);
    s.turn("generate").unwrap();
    let res = all_results(&request(&script, 1));
    assert!(res[0].0.contains("NONET") && res[0].0.contains("without network"), "{:?}", res[0]);
    assert_eq!(fs::read_to_string(wt.path().join("out.txt")).unwrap(), "generated\n"); // local effect kept
    let pre = eng.lineage(&s.branch).unwrap().into_iter().find(|e| e.kind == "tool.pre").unwrap();
    assert_eq!(pre.str("sandbox"), Some("offline"));
    // The file change is still attributed to the agent's call.
    assert_eq!(eng.blame(&s.branch, "out.txt").unwrap()[0].0.str("call"), Some("s"));
}

#[test]
fn human_rewind_then_continue() {
    let (_h, wt, eng) = setup();
    let d = wt.path().to_path_buf();
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("w1", "Write", json!({ "file_path": "one.txt", "content": "1\n" }))]),
        msg(vec![text("one")]),
        msg(vec![tool_use("w2", "Write", json!({ "file_path": "two.txt", "content": "2\n" }))]),
        msg(vec![text("two")]),
        msg(vec![text("three")]),
    ]));
    let mut s = session(&eng, &script, Permission::Dry);
    s.turn("first").unwrap();
    s.turn("second").unwrap();
    let chain = s.chain().unwrap();
    let t1 = chain.iter().find(|e| e.kind == "turn.end" && e.turn() == Some(1)).unwrap().clone();
    let plan = rewind::plan(&eng, &s.branch, &t1, Mode::KeepHuman).unwrap();
    rewind::apply(&eng, &plan, false).unwrap();
    assert!(!d.join("two.txt").exists());

    let r = s.turn("third").unwrap();
    assert_eq!(r.text, "three");
    let req = request(&script, 4);
    let raw = serde_json::to_string(&req).unwrap();
    assert!(!raw.contains("\"second\"") && !raw.contains("two.txt"), "rewound turn leaked: {}", serde_json::to_string_pretty(&req["messages"]).unwrap());
    assert!(raw.contains("restored to the end of turn 1"));
    // The new turn is numbered after the target.
    let head = eng.store.head(&s.branch).unwrap().unwrap();
    let chain = effective_chain(&eng.store, &head).unwrap();
    assert_eq!(chain.iter().rev().find(|e| e.kind == "turn.start").unwrap().turn(), Some(2));
    assert!(verify_requests(&eng, &s.branch).unwrap().iter().all(|x| x.1));
    // Fold and the sent request agree on message count.
    assert_eq!(fold(&eng.store, &chain).unwrap().messages.len() - 1, req["messages"].as_array().unwrap().len());
}

#[test]
fn bash_background_and_timeout() {
    let (_h, wt, eng) = setup();
    let ctx = runestone::agent::tools::Ctx {
        worktree: eng.world.worktree.clone(),
        tmp: wt.path().join(".tmp-test"),
        seen: Default::default(),
        offline: false,
    };
    let t = std::time::Instant::now();
    let o = ctx.run("Bash", &json!({ "command": "sleep 20 & echo started" }));
    assert!(t.elapsed().as_secs() < 5, "background child must not block");
    assert!(o.text.contains("started"));
    let o = ctx.run("Bash", &json!({ "command": "sleep 10", "timeout": 1000 }));
    assert!(o.is_error && o.text.contains("timed out"));
    let o = ctx.run("Bash", &json!({ "command": "echo hi; exit 3" }));
    assert_eq!(o.exit, Some(3));
    assert!(o.text.contains("[exit code 3]"));
    let o = ctx.run("Glob", &json!({ "pattern": "*.txt" }));
    assert!(o.text.contains("src.txt"));
    let o = ctx.run("Grep", &json!({ "pattern": "hel+o", "output_mode": "content" }));
    assert!(o.text.contains("hello"), "{}", o.text);
}
