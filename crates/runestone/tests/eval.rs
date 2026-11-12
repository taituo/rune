//! `rune eval`: task files, the miner, the runner over scripted models, the report.

use runestone::agent::model::{msg, text, tool_use, Provider, Scripted};
use runestone::agent::{Config, Permission};
use runestone::eval::{self, Mode, RunOpts, Task};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn task_files_parse_and_resolve_relative_paths() {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir_all(d.path().join("tasks")).unwrap();
    let p = d.path().join("tasks/t.json");
    fs::write(
        &p,
        json!({ "name": "t", "repo": "../proj", "prompt": "fix", "hidden": { "cmd": "true", "dir": "hid" } }).to_string(),
    )
    .unwrap();
    let t = Task::load(&p).unwrap();
    assert_eq!(t.repo, d.path().join("tasks/../proj"));
    assert_eq!(t.hidden.dir.as_deref(), Some(d.path().join("tasks/hid").as_path()));
    assert_eq!((t.timeout, t.max_steps, t.base.as_deref()), (600, 40, None));

    for bad in [
        json!({ "name": "t", "repo": ".", "prompt": "", "hidden": { "cmd": "true" } }),
        json!({ "name": "t", "repo": ".", "prompt": "x", "hidden": { "cmd": "" } }),
        json!({ "name": "t", "repo": ".", "prompt": "x", "hidden": { "cmd": "true", "commit": "abc" } }),
        json!({ "name": "t", "repo": ".", "prompt": "x" }),
    ] {
        fs::write(&p, bad.to_string()).unwrap();
        assert!(Task::load(&p).is_err(), "{bad}");
    }
    // Round trip: what the miner writes, the loader reads.
    let t2 = Task { hidden: eval::HiddenTests { cmd: "c".into(), commit: Some("abc".into()), paths: vec!["tests/a.rs".into()], ..Default::default() }, ..t };
    fs::write(&p, serde_json::to_string(&t2).unwrap()).unwrap();
    assert_eq!(Task::load(&p).unwrap().hidden, t2.hidden);
}

#[test]
fn hidden_paths_cannot_leave_the_tree() {
    let d = tempfile::tempdir().unwrap();
    let mut t = Task::load(&{
        let p = d.path().join("t.json");
        fs::write(&p, json!({ "name": "t", "repo": ".", "prompt": "x", "hidden": { "cmd": "true" } }).to_string()).unwrap();
        p
    })
    .unwrap();
    t.hidden.files.insert("../escape.txt".into(), "x".into());
    let w = d.path().join("w");
    fs::create_dir_all(&w).unwrap();
    assert!(t.apply_hidden(&w).is_err());
    assert!(!d.path().join("escape.txt").exists());
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null") // hermetic: no signing hooks or user config
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

#[test]
fn miner_holds_out_the_commits_tests() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().join("proj");
    fs::create_dir_all(r.join("tests")).unwrap();
    git(&r, &["init", "-q"]);
    fs::write(r.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    fs::write(r.join("tests/test_calc.py"), "from calc import add\n").unwrap();
    git(&r, &["add", "-A"]);
    git(&r, &["commit", "-qm", "start"]);
    // code + test: a task
    fs::write(r.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
    fs::write(r.join("tests/test_calc.py"), "from calc import add\nassert add(2, 2) == 4\n").unwrap();
    git(&r, &["commit", "-qam", "Fix add\n\nIt subtracted."]);
    // code only, and docs + test: not tasks
    fs::write(r.join("calc.py"), "def add(a, b):\n    return b + a\n").unwrap();
    git(&r, &["commit", "-qam", "Refactor"]);
    fs::write(r.join("README"), "x\n").unwrap();
    fs::write(r.join("tests/test_calc.py"), "from calc import add\nassert add(1, 2) == 3\n").unwrap();
    git(&r, &["add", "-A"]);
    git(&r, &["commit", "-qm", "Docs and a test"]);

    let tasks = eval::mine(&r, &eval::MineOpts { limit: 50, max_code_lines: 100 }).unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    let t = &tasks[0];
    assert!(t.prompt.starts_with("Fix add\n\nIt subtracted."));
    assert_eq!(t.hidden.paths, vec!["tests/test_calc.py"]);
    assert_eq!(t.hidden.cmd, "python3 -m pytest -q tests/test_calc.py 2>&1");

    // The base is the parent: the bug is there and the commit's test is not.
    let w = d.path().join("w");
    fs::create_dir_all(&w).unwrap();
    t.materialize(&w).unwrap();
    assert!(fs::read_to_string(w.join("calc.py")).unwrap().contains("a - b"));
    assert!(!w.join(".git").exists());
    t.apply_hidden(&w).unwrap();
    assert!(fs::read_to_string(w.join("tests/test_calc.py")).unwrap().contains("add(2, 2) == 4"));
}

/// Three small Python tasks; the scripted model fixes one honestly, games another, and does nothing on
/// the third. The hidden tests must tell them apart.
fn tasks(dir: &Path) -> Vec<Task> {
    let mut v = Vec::new();
    for (name, bug) in [("honest", "a - b"), ("gamed", "a * b"), ("idle", "a - b")] {
        let repo = dir.join(name);
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("calc.py"), format!("def add(a, b):\n    return {bug}\n")).unwrap();
        fs::write(repo.join("test_visible.py"), "from calc import add\nassert add(2, 2) == 4\nprint('1 passed')\n").unwrap();
        let p = dir.join(format!("{name}.json"));
        fs::write(
            &p,
            json!({
                "name": name, "repo": name, "prompt": "add() is wrong; fix it.",
                "visible": "python3 test_visible.py 2>&1",
                "hidden": { "cmd": "python3 test_hidden.py 2>&1", "files": { "test_hidden.py": "from calc import add\nassert add(2, 3) == 5\nassert add(-1, 1) == 0\n" } },
                "timeout": 30, "max_steps": 10
            })
            .to_string(),
        )
        .unwrap();
        v.push(Task::load(&p).unwrap());
    }
    v
}

fn script(task: &str) -> Scripted {
    let edit = |new: &str| {
        vec![
            msg(vec![tool_use("r", "Read", json!({ "file_path": "calc.py" }))]),
            msg(vec![tool_use("e", "Edit", json!({ "file_path": "calc.py", "old_string": "return", "new_string": new }))]),
            msg(vec![text("Done.")]),
        ]
    };
    Scripted::new(match task {
        "honest" => edit("return a + b  #"),
        // Passes the visible test (2*2 == 4 already) by special-casing it; hidden tests catch it.
        "gamed" => edit("return 4 if (a, b) == (2, 2) else"),
        _ => vec![msg(vec![text("Nothing to do.")])],
    })
}

#[test]
fn runner_scores_by_hidden_tests_and_resumes() {
    let d = tempfile::tempdir().unwrap();
    let ts = tasks(d.path());
    let calls = AtomicUsize::new(0);
    let providers = |t: &Task, _m: Mode, _r: usize, _a: usize| -> anyhow::Result<Box<dyn Provider>> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(script(&t.name)))
    };
    let opts = RunOpts {
        modes: vec![Mode::Plain, Mode::Checks, Mode::Search],
        runs: 1,
        jobs: 3,
        out: d.path().join("out"),
        perm: Permission::Auto,
        config: Config::default(),
        search_width: 2,
    };
    let res = eval::run(&ts, &opts, &providers).unwrap();
    assert_eq!(res.len(), 9);
    for r in &res {
        assert!(r.error.is_none(), "{}/{:?}: {:?}", r.task, r.mode, r.error);
        assert_eq!(r.pass, r.task == "honest", "{}/{:?}: {}", r.task, r.mode, r.hidden_tail);
    }
    let plain = |t: &str| res.iter().find(|r| r.task == t && r.mode == Mode::Plain).unwrap();
    assert_eq!(plain("honest").steps, 3);
    assert_eq!(plain("idle").steps, 1);
    let made = calls.load(Ordering::SeqCst);
    assert_eq!(made, 3 + 3 + 3 * 2, "one provider per session, two per search");

    // Scoring did not leave the hidden test in the agent's tree.
    assert!(!plain("honest").worktree.join("test_hidden.py").exists());

    // Resumable: done cells are not run again; a deleted result is.
    let again = eval::run(&ts, &opts, &providers).unwrap();
    assert_eq!((again.len(), calls.load(Ordering::SeqCst)), (9, made));
    fs::remove_file(d.path().join("out/results/idle__plain__1.json")).unwrap();
    let third = eval::run(&ts, &opts, &providers).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), made + 1);
    assert!(third.iter().all(|r| r.error.is_none()), "{third:?}");

    let md = eval::write_report(&opts.out, &again).unwrap();
    let text = fs::read_to_string(md).unwrap();
    assert!(text.contains("| plain | 3 | 1 | 33% | 0 |"), "{text}");
    assert!(text.contains("| honest | 1/1 | 1/1 | 1/1 |"), "{text}");
    assert!(text.contains("| gamed | 0/1 | 0/1 | 0/1 |"), "{text}");
    let j: serde_json::Value = serde_json::from_slice(&fs::read(opts.out.join("report.json")).unwrap()).unwrap();
    assert_eq!(j["cells"].as_array().unwrap().len(), 9);
}

#[test]
fn validate_keeps_only_tasks_whose_tests_flip() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().join("proj");
    fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q"]);
    fs::write(r.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    fs::write(r.join("test_calc.py"), "from calc import add\n").unwrap();
    git(&r, &["add", "-A"]);
    git(&r, &["commit", "-qm", "start"]);
    fs::write(r.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
    fs::write(r.join("test_calc.py"), "from calc import add\nassert add(2, 2) == 4\n").unwrap();
    git(&r, &["commit", "-qam", "Fix add"]);
    let mut t = eval::mine(&r, &eval::MineOpts { limit: 10, max_code_lines: 100 }).unwrap().remove(0);
    t.hidden.cmd = "python3 test_calc.py".into(); // no pytest needed
    let scratch = d.path().join("scratch");
    assert_eq!(eval::validate(&t, &scratch).unwrap(), (true, true));
    t.hidden.cmd = "true".into(); // passes everywhere: nothing to do at the base
    assert_eq!(eval::validate(&t, &scratch).unwrap(), (false, true));
}
