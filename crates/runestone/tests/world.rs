use runestone::rewind::{self, Mode};
use runestone::{classify, hook, ops, Engine};
use serde_json::json;
use std::fs;
use std::path::Path;

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Engine) {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let lines: Vec<String> = (1..=60).map(|i| format!("line {i}")).collect();
    fs::write(wt.path().join("handler.go"), lines.join("\n") + "\n").unwrap();
    fs::write(wt.path().join(".env"), "SECRET=1\n").unwrap();
    let eng = Engine::open_in(home.path(), wt.path()).unwrap();
    (home, wt, eng)
}

fn edit_line(dir: &Path, file: &str, n: usize, text: &str) {
    let p = dir.join(file);
    let s = fs::read_to_string(&p).unwrap();
    let mut l: Vec<String> = s.lines().map(String::from).collect();
    l[n - 1] = text.to_string();
    fs::write(&p, l.join("\n") + "\n").unwrap();
}

fn line(dir: &Path, file: &str, n: usize) -> String {
    fs::read_to_string(dir.join(file)).unwrap().lines().nth(n - 1).unwrap().to_string()
}

/// An agent turn: pre, the edit, post, end.
fn agent_turn(eng: &Engine, b: &str, call: &str, f: impl FnOnce()) {
    eng.record(b, "turn.start", json!({})).unwrap();
    eng.record(b, "tool.pre", json!({ "call": call, "tool": "Edit", "summary": call })).unwrap();
    f();
    eng.record(b, "tool.post", json!({ "call": call, "tool": "Edit", "summary": call })).unwrap();
    eng.record(b, "turn.end", json!({})).unwrap();
}

#[test]
fn attribution_and_keep_human_rewind() {
    let (_h, wt, eng) = setup();
    let d = wt.path();
    let b = eng.branch("s/test");
    eng.record(&b, "spawn", json!({})).unwrap();

    agent_turn(&eng, &b, "c1", || edit_line(d, "handler.go", 5, "agent t1"));
    edit_line(d, "handler.go", 50, "human between turns"); // outside
    let t2 = eng.record(&b, "turn.start", json!({})).unwrap();
    assert_eq!(t2.str("author"), Some("outside"));
    let note = eng.outside_note(&t2).expect("note");
    assert!(note.contains("handler.go") && note.contains("turn 1"), "{note}");
    eng.record(&b, "tool.pre", json!({ "call": "c2", "tool": "Edit" })).unwrap();
    edit_line(d, "handler.go", 15, "agent t2");
    fs::write(d.join("new.txt"), "agent file\n").unwrap();
    let post = eng.record(&b, "tool.post", json!({ "call": "c2", "tool": "Edit" })).unwrap();
    assert_eq!(post.str("author"), Some("agent"));
    eng.record(&b, "turn.end", json!({})).unwrap();
    fs::write(d.join("notes.md"), "human, unrecorded\n").unwrap(); // live tail

    // blame sees agent (t1, t2) and outside (t2 start)
    let blame = eng.blame(&b, "handler.go").unwrap();
    let authors: Vec<&str> = blame.iter().map(|(e, _)| e.str("author").unwrap()).collect();
    assert_eq!(authors, vec!["agent", "outside", "agent", "outside"]); // last = initial import at spawn

    // .env is excluded: never in the record
    assert!(eng.world.git(&["ls-tree", "-r", "--name-only", post.tree().unwrap()]).unwrap().lines().all(|l| l != ".env"));

    let target = eng.lineage(&b).unwrap().into_iter().find(|e| e.kind == "turn.end" && e.turn() == Some(1)).unwrap();
    let plan = rewind::plan(&eng, &b, &target, Mode::KeepHuman).unwrap();
    assert!(plan.parked.is_empty());
    assert_eq!(plan.dropped.iter().filter(|s| s.author == "agent").count(), 1);
    rewind::apply(&eng, &plan, false).unwrap();

    assert_eq!(line(d, "handler.go", 5), "agent t1"); // before the target: kept
    assert_eq!(line(d, "handler.go", 15), "line 15"); // agent after the target: reverted
    assert_eq!(line(d, "handler.go", 50), "human between turns"); // human: kept
    assert!(!d.join("new.txt").exists());
    assert!(d.join("notes.md").exists()); // unrecorded human work: kept
    assert!(d.join(".env").exists()); // excluded: untouched

    // undo restores the pre-rewind world exactly
    rewind::undo(&eng, &b).unwrap();
    assert_eq!(line(d, "handler.go", 15), "agent t2");
    assert!(d.join("new.txt").exists());
}

#[test]
fn dependent_human_hunk_is_parked() {
    let (_h, wt, eng) = setup();
    let d = wt.path();
    let b = eng.branch("s/dep");
    eng.record(&b, "spawn", json!({})).unwrap();
    agent_turn(&eng, &b, "c1", || edit_line(d, "handler.go", 5, "agent t1"));
    agent_turn(&eng, &b, "c2", || edit_line(d, "handler.go", 15, "if err != nil && !retry {"));
    edit_line(d, "handler.go", 15, "if err != nil && !retry && ok {"); // human builds on agent's line
    edit_line(d, "handler.go", 40, "independent human edit");
    eng.record(&b, "turn.start", json!({})).unwrap();

    let target = eng.lineage(&b).unwrap().into_iter().find(|e| e.kind == "turn.end" && e.turn() == Some(1)).unwrap();
    let plan = rewind::plan(&eng, &b, &target, Mode::KeepHuman).unwrap();
    assert_eq!(plan.parked.len(), 1, "{}", plan.render());
    assert_eq!(plan.parked[0].paths, vec!["handler.go"]);
    assert!(plan.parked[0].patch.contains("&& ok {"));
    let ev = rewind::apply(&eng, &plan, false).unwrap();
    assert_eq!(line(d, "handler.go", 15), "line 15"); // agent's line gone, human's dependent edit parked
    // Conflicted *paths* keep ours; v1 parks the whole file's segment, including the independent hunk.
    let parked = ev.body["parked"][0]["file"].as_str().unwrap();
    assert!(fs::read_to_string(parked).unwrap().contains("independent human edit"));
}

#[test]
fn hard_rewind_and_external_guard() {
    let (_h, wt, eng) = setup();
    let d = wt.path();
    let b = eng.branch("s/hard");
    eng.record(&b, "spawn", json!({})).unwrap();
    agent_turn(&eng, &b, "c1", || edit_line(d, "handler.go", 1, "t1"));
    eng.record(&b, "turn.start", json!({})).unwrap();
    let cls = classify::shell("git push origin feature/retry");
    eng.record(&b, "tool.pre", json!({ "call": "p", "tool": "Bash", "summary": "git push origin feature/retry", "class": cls })).unwrap();
    eng.record(&b, "tool.post", json!({ "call": "p", "tool": "Bash" })).unwrap();
    edit_line(d, "handler.go", 2, "human");
    eng.record(&b, "turn.end", json!({})).unwrap();

    let target = eng.lineage(&b).unwrap().into_iter().find(|e| e.kind == "turn.end" && e.turn() == Some(1)).unwrap();
    let plan = rewind::plan(&eng, &b, &target, Mode::Hard).unwrap();
    assert_eq!(plan.external.len(), 1);
    assert!(rewind::apply(&eng, &plan, false).is_err(), "must refuse to cross a push silently");
    rewind::apply(&eng, &plan, true).unwrap();
    assert_eq!(line(d, "handler.go", 2), "line 2"); // hard mode drops human work too
}

#[test]
fn refuses_when_world_moved_after_plan() {
    let (_h, wt, eng) = setup();
    let d = wt.path();
    let b = eng.branch("s/race");
    eng.record(&b, "spawn", json!({})).unwrap();
    agent_turn(&eng, &b, "c1", || edit_line(d, "handler.go", 1, "t1"));
    let target = eng.lineage(&b).unwrap()[0].clone();
    let plan = rewind::plan(&eng, &b, &target, Mode::KeepHuman).unwrap();
    edit_line(d, "handler.go", 3, "late human edit");
    assert!(rewind::apply(&eng, &plan, false).is_err());
    assert_eq!(line(d, "handler.go", 3), "late human edit");
}

#[test]
fn fork_and_bisect() {
    let (h, wt, eng) = setup();
    let d = wt.path();
    let b = eng.branch("s/bisect");
    fs::write(d.join("status.txt"), "ok\n").unwrap();
    eng.record(&b, "spawn", json!({})).unwrap();
    for i in 1..=6 {
        agent_turn(&eng, &b, &format!("c{i}"), || {
            edit_line(d, "handler.go", i, &format!("change {i}"));
            if i == 4 {
                fs::write(d.join("status.txt"), "fail\n").unwrap();
            }
        });
    }
    let r = ops::bisect(&eng, &b, "grep -qx ok status.txt").unwrap();
    assert_eq!(r.first_bad.turn(), Some(4));
    assert_eq!(r.first_bad.str("author"), Some("agent"));
    // second run is all cache hits
    let r2 = ops::bisect(&eng, &b, "grep -qx ok status.txt").unwrap();
    assert!(r2.probes.iter().all(|p| p.cached));

    let at = eng.lineage(&b).unwrap().into_iter().find(|e| e.kind == "turn.end" && e.turn() == Some(2)).unwrap();
    let fdir = h.path().join("fork-a");
    let (fb, _) = ops::fork(&eng, &at, "try-a", &fdir).unwrap();
    assert_eq!(line(&fdir, "handler.go", 2), "change 2");
    assert_eq!(line(&fdir, "handler.go", 3), "line 3");
    let child = Engine::open_in(h.path(), &fdir).unwrap();
    let ev = child.record(&fb, "turn.start", json!({})).unwrap();
    assert!(runestone::engine::changes(&ev).is_empty(), "fork starts at an identical world");
    assert_eq!(ev.turn(), Some(3));
    // the fork shares history with the parent
    assert!(child.lineage(&fb).unwrap().iter().any(|e| e.id == at.id));
}

#[test]
fn event_ids_verify() {
    let (_h, wt, eng) = setup();
    let b = eng.branch("s/v");
    eng.record(&b, "spawn", json!({})).unwrap();
    agent_turn(&eng, &b, "c1", || fs::write(wt.path().join("a.txt"), "x").unwrap());
    let lin = eng.lineage(&b).unwrap();
    assert!(lin.iter().all(runestone::store::verify));
    let mut forged = lin[2].clone();
    forged.body["author"] = json!("outside");
    assert!(!runestone::store::verify(&forged));
}

#[test]
fn classifier_table() {
    use classify::{Disclosure as D, State as S};
    let cases: &[(&str, S, D)] = &[
        ("git status", S::Read, D::None),
        ("git diff HEAD~1", S::Read, D::None),
        ("git push origin feature/retry", S::Compensable, D::External),
        ("git push --force origin main", S::Irreversible, D::External),
        ("git push --dry-run", S::Read, D::External),
        ("cd a && make && git push", S::Unknown, D::External),
        ("cargo test 2>&1 | tail -20", S::Local, D::None),
        ("sed -i 's/a/b/' src/x.rs", S::Local, D::None),
        ("curl -s https://example.com/api", S::Read, D::External),
        ("curl -X POST -d '{}' https://example.com/api", S::Irreversible, D::External),
        ("kubectl get pods", S::Read, D::Internal),
        ("kubectl apply -f deploy.yaml", S::Irreversible, D::External),
        ("kubectl apply -f deploy.yaml --dry-run=client", S::Read, D::Internal),
        ("terraform plan", S::Read, D::Internal),
        ("echo hi > out.txt", S::Local, D::None),
        ("ls $(git rev-parse --show-toplevel)", S::Read, D::None),
        ("echo \"$(curl -X DELETE https://x/y)\"", S::Irreversible, D::External),
        ("FOO=1 timeout 30 pytest -q", S::Local, D::None),
        ("./deploy.sh", S::Unknown, D::None),
        ("gh pr create --fill", S::Compensable, D::External),
        ("gh pr view 12", S::Read, D::Internal),
    ];
    for (cmd, s, d) in cases {
        let c = classify::shell(cmd);
        assert_eq!((c.state, c.disclosure), (*s, *d), "{cmd}: {}", c.reason);
    }
    let t = classify::tool("mcp__jira__add_comment", &json!({}));
    assert_eq!(t.state, S::Compensable);
    let t = classify::tool("mcp__slack__send_message", &json!({}));
    assert_eq!(t.state, S::Irreversible);
    let t = classify::tool("mcp__jira__search_issues", &json!({}));
    assert_eq!(t.state, S::Read);
    assert_eq!(classify::tool("Read", &json!({})).state, S::Read);
}

#[test]
fn hook_ingestion_claude_format() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    std::env::set_var("RUNE_HOME", home.path());
    let d = wt.path();
    fs::write(d.join("a.txt"), "1\n").unwrap();
    let cwd = d.to_string_lossy();
    let send = |v: serde_json::Value| hook::handle(&v.to_string(), "claude");
    let base = |ev: &str| json!({ "session_id": "abc", "cwd": cwd, "hook_event_name": ev });

    assert_eq!(send(base("SessionStart")).exit, 0);
    send(base("UserPromptSubmit"));
    let mut pre = base("PreToolUse");
    pre["tool_name"] = json!("Bash");
    pre["tool_input"] = json!({ "command": "echo 2 > a.txt" });
    pre["tool_use_id"] = json!("toolu_1");
    assert_eq!(send(pre.clone()).exit, 0);
    fs::write(d.join("a.txt"), "2\n").unwrap();
    let mut post = pre.clone();
    post["hook_event_name"] = json!("PostToolUse");
    send(post);
    send(base("Stop"));
    fs::write(d.join("b.txt"), "human\n").unwrap();
    let o = send(base("UserPromptSubmit"));
    assert!(o.stdout.contains("A b.txt"), "note: {}", o.stdout);
    assert!(!o.stdout.contains("human\n"), "never inject content");

    // dry world blocks a push at pre-tool with exit 2
    std::env::set_var("RUNE_DRY_WORLD", "1");
    let mut push = base("PreToolUse");
    push["tool_name"] = json!("Bash");
    push["tool_input"] = json!({ "command": "git push origin main" });
    let o = send(push);
    std::env::remove_var("RUNE_DRY_WORLD");
    assert_eq!(o.exit, 2);
    assert!(o.stderr.contains("dry world"));

    // garbage fails open
    assert_eq!(hook::handle("{not json", "claude").exit, 0);

    let eng = Engine::open_in(home.path(), d).unwrap();
    let lin = eng.lineage(&eng.branch("s/abc")).unwrap();
    let post = lin.iter().find(|e| e.kind == "tool.post").unwrap();
    assert_eq!(post.str("author"), Some("agent"));
    assert_eq!(post.str("call"), Some("toolu_1"));
    assert!(lin.iter().any(|e| e.kind == "tool.blocked"));
}
