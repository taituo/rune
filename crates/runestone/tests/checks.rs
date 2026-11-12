use runestone::agent::model::{msg, text, tool_use, Scripted};
use runestone::agent::session::Checks;
use runestone::agent::{verify_requests, Config, Permission, Quiet, Session};
use runestone::checks::{self, parse};
use runestone::Engine;
use serde_json::{json, Value};
use std::fs;
use std::rc::Rc;
use std::time::Duration;

struct Shared(Rc<Scripted>);
impl runestone::agent::model::Provider for Shared {
    fn send(&self, body: &[u8], on: &mut dyn FnMut(runestone::agent::model::Delta)) -> anyhow::Result<Value> {
        self.0.send(body, on)
    }
}

#[test]
fn parses_common_runners() {
    let cargo = "running 3 tests\n...\ntest result: ok. 8 passed; 0 failed; 0 ignored\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored\nerror: test failed, to rerun pass `--lib`";
    let s = parse(cargo, 101);
    assert_eq!((s.passed, s.failed, s.green, s.value), (10, 1, false, 10.0));
    let s = parse("==== 3 passed, 1 failed in 0.12s ====", 1);
    assert_eq!((s.passed, s.failed), (3, 1));
    let s = parse("Tests:       1 failed, 3 passed, 4 total", 1);
    assert_eq!((s.passed, s.failed), (3, 1));
    let s = parse("ok  \texample.com/a\t0.1s\nFAIL\texample.com/b\t0.2s\nFAIL", 1);
    assert_eq!((s.passed, s.failed), (1, 1));
    let s = parse("whatever\nrune-score: 42.5\n", 0);
    assert_eq!(s.value, 42.5);
    assert_eq!(parse("built fine", 0).value, 1.0);
    assert_eq!(parse("boom", 2).value, 0.0);
}

/// A check script whose score is the number of "ok" lines in results.txt; any "bad" line fails it.
const SCORE_SH: &str = "p=$(grep -c '^ok$' results.txt 2>/dev/null); f=$(grep -c '^bad$' results.txt 2>/dev/null); echo \"${p:-0} passed, ${f:-0} failed\"; [ \"${f:-0}\" -eq 0 ]";

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Engine) {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    fs::write(wt.path().join("readme.txt"), "project\n").unwrap();
    let eng = Engine::open_in(home.path(), wt.path()).unwrap();
    (home, wt, eng)
}

#[test]
fn results_are_cached_by_tree() {
    let (_h, wt, eng) = setup();
    fs::write(wt.path().join("results.txt"), "ok\nok\n").unwrap();
    let tree = eng.world.snapshot().unwrap();
    let a = checks::run(&eng, &tree, SCORE_SH, Duration::from_secs(30)).unwrap();
    assert!(!a.cached && a.score.green && a.score.passed == 2, "{a:?}");
    let b = checks::run(&eng, &tree, SCORE_SH, Duration::from_secs(30)).unwrap();
    assert!(b.cached);
    assert_eq!(a.score, b.score);
    // The check ran in its own workspace: the live directory has no build artefacts from it.
    assert!(!wt.path().join("results.txt.out").exists());
}

#[test]
fn regression_rolls_back_to_best_and_keeps_the_abandoned_branch() {
    let (_h, wt, eng) = setup();
    let d = wt.path().to_path_buf();
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("w1", "Write", json!({ "file_path": "results.txt", "content": "ok\nok\n" }))]),
        msg(vec![tool_use("w2", "Write", json!({ "file_path": "results.txt", "content": "ok\nbad\n" }))]),
        msg(vec![tool_use("w3", "Write", json!({ "file_path": "results.txt", "content": "bad\nbad\n" }))]),
        // after the rollback
        msg(vec![text("Back at the best state; stopping here.")]),
    ]));
    let b = eng.branch("s/c");
    Session::spawn(&eng, &b, &Config::default(), "2026-11-11").unwrap();
    let mut s = Session {
        eng: &eng,
        branch: b.clone(),
        provider: Box::new(Shared(script.clone())),
        ui: Box::new(Quiet),
        perm: Permission::Dry,
        max_steps: 20,
        checks: Some(Checks { cmd: SCORE_SH.into(), every: 1, rollback_after: 2, timeout: Duration::from_secs(30) }),
    };
    fs::write(d.join("human.txt"), "mine\n").unwrap(); // a human file, written before the turn
    let r = s.turn("make the results good").unwrap();
    assert_eq!(r.text, "Back at the best state; stopping here.");

    // World: back to the best checked state; the human's file is still there.
    assert_eq!(fs::read_to_string(d.join("results.txt")).unwrap(), "ok\nok\n");
    assert!(d.join("human.txt").exists());

    let req = |i: usize| -> String { String::from_utf8(script.requests.borrow()[i].clone()).unwrap() };
    // The model saw the red check after its second write…
    assert!(req(2).contains("failing (1 passed, 1 failed)"), "{}", req(2));
    // …and after the rollback, a note instead of the failed path.
    let after = req(3);
    assert!(after.contains("got worse 2 times in a row"), "{after}");
    assert!(after.contains("Undone attempts"));
    assert!(after.contains("results.txt"));
    assert!(!after.contains("bad\\nbad") && !after.contains("\"w3\""), "the failed path leaked into context");

    // Exact re-render still holds across the rollback.
    assert!(verify_requests(&eng, &b).unwrap().iter().all(|x| x.1));

    // The abandoned branch is still in the record, and its world is pinned against git gc.
    let lineage = eng.lineage(&b).unwrap();
    let abandoned = lineage.iter().find(|e| e.kind == "tool.post" && e.str("call") == Some("w3")).expect("abandoned event kept");
    let tree = abandoned.tree().unwrap();
    assert!(eng.world.git_dir.join(format!("refs/rune/t/{tree}")).exists(), "abandoned tree must be pinned");
    let rw = lineage.iter().find(|e| e.kind == "world.rewind").unwrap();
    assert_eq!(rw.body["auto"], json!(true));
    assert_eq!(rw.body["streak"], json!(2));
}

#[test]
fn no_rollback_without_a_regression() {
    let (_h, _wt, eng) = setup();
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("w1", "Write", json!({ "file_path": "results.txt", "content": "ok\n" }))]),
        msg(vec![tool_use("w2", "Write", json!({ "file_path": "results.txt", "content": "ok\nok\n" }))]),
        msg(vec![text("improving")]),
    ]));
    let b = eng.branch("s/n");
    Session::spawn(&eng, &b, &Config::default(), "2026-11-11").unwrap();
    let mut s = Session {
        eng: &eng,
        branch: b.clone(),
        provider: Box::new(Shared(script.clone())),
        ui: Box::new(Quiet),
        perm: Permission::Dry,
        max_steps: 20,
        checks: Some(Checks { cmd: SCORE_SH.into(), every: 1, rollback_after: 1, timeout: Duration::from_secs(30) }),
    };
    s.turn("go").unwrap();
    let lin = eng.lineage(&b).unwrap();
    assert!(!lin.iter().any(|e| e.kind == "world.rewind"));
    assert_eq!(lin.iter().filter(|e| e.kind == "check").count(), 3, "baseline + one per edit");
}

#[test]
fn the_first_edit_can_roll_back_against_the_baseline() {
    // The start already scores 2; the very first edit makes it worse. Without a baseline check there would
    // be nothing to compare against (found in the trap fixture with --rollback-after 1).
    let (_h, wt, eng) = setup();
    fs::write(wt.path().join("results.txt"), "ok\nok\n").unwrap();
    let script = Rc::new(Scripted::new(vec![
        msg(vec![tool_use("r1", "Read", json!({ "file_path": "results.txt" }))]),
        msg(vec![tool_use("w1", "Write", json!({ "file_path": "results.txt", "content": "ok\nbad\n" }))]),
        msg(vec![text("after rollback")]),
    ]));
    let b = eng.branch("s/base");
    Session::spawn(&eng, &b, &Config::default(), "2026-11-12").unwrap();
    let mut s = Session {
        eng: &eng,
        branch: b.clone(),
        provider: Box::new(Shared(script.clone())),
        ui: Box::new(Quiet),
        perm: Permission::Dry,
        max_steps: 20,
        checks: Some(Checks { cmd: SCORE_SH.into(), every: 1, rollback_after: 1, timeout: Duration::from_secs(30) }),
    };
    s.turn("go").unwrap();
    let lin = eng.lineage(&b).unwrap();
    assert!(lin.iter().any(|e| e.kind == "world.rewind" && e.body["auto"] == json!(true)), "rolled back");
    assert_eq!(fs::read_to_string(wt.path().join("results.txt")).unwrap(), "ok\nok\n");
    // A second turn on the same tree does not re-run the baseline.
    let n = |l: &[runestone::store::Event]| l.iter().filter(|e| e.kind == "check").count();
    let before = n(&lin);
    let script2 = Rc::new(Scripted::new(vec![msg(vec![text("nothing")])]));
    s.provider = Box::new(Shared(script2));
    s.turn("again").unwrap();
    assert_eq!(n(&eng.lineage(&b).unwrap()), before);
}

#[test]
fn offline_runs_keep_loopback_but_no_network() {
    if !runestone::agent::tools::offline_available() {
        assert!(std::env::var("RUNE_REQUIRE_NS").is_err(), "no user namespaces (RUNE_REQUIRE_NS=1)");
        eprintln!("SKIPPED: no user namespaces");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A test that serves on 127.0.0.1 and talks to itself, like a mock-server test.
    let local = r#"python3 -c "
import socket
s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen()
c = socket.create_connection(s.getsockname()); print('loopback ok')
""#;
    let (exit, out, _) = runestone::checks::exec(dir.path(), local, std::time::Duration::from_secs(20)).unwrap();
    assert_eq!(exit, 0, "{out}");
    let remote = r#"python3 -c "
import socket
socket.create_connection(('1.1.1.1', 80), timeout=3)
""#;
    let (exit, _, _) = runestone::checks::exec(dir.path(), remote, std::time::Duration::from_secs(20)).unwrap();
    assert_ne!(exit, 0, "reached the network");
}
