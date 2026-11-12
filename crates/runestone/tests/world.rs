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

#[test]
fn in_process_diff_matches_git_numstat() {
    let (_h, wt, eng) = setup();
    let d = wt.path();
    fs::create_dir_all(d.join("a/b")).unwrap();
    fs::write(d.join("a/b/deep.txt"), "1\n2\n3\n").unwrap();
    fs::write(d.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    fs::write(d.join("noeol.txt"), "x\ny").unwrap();
    fs::write(d.join("turns_into_dir"), "file\n").unwrap();
    let t1 = eng.world.snapshot().unwrap();
    edit_line(d, "handler.go", 7, "changed seven");
    fs::write(d.join("handler.go"), fs::read_to_string(d.join("handler.go")).unwrap() + "appended\n").unwrap();
    fs::remove_dir_all(d.join("a")).unwrap();
    fs::write(d.join("bin.dat"), [0u8, 9, 9]).unwrap();
    fs::write(d.join("noeol.txt"), "x\ny\nz").unwrap();
    fs::remove_file(d.join("turns_into_dir")).unwrap();
    fs::create_dir(d.join("turns_into_dir")).unwrap();
    fs::write(d.join("turns_into_dir/inner"), "in\n").unwrap();
    fs::write(d.join("new.txt"), "").unwrap();
    let t2 = eng.world.snapshot().unwrap();

    let ours: Vec<String> = eng.world.diff(&t1, &t2).unwrap().iter().map(|c| format!("{}\t{}\t{}\t{}", c.status, c.added.map(|x| x.to_string()).unwrap_or("-".into()), c.deleted.map(|x| x.to_string()).unwrap_or("-".into()), c.path)).collect();
    let ns = eng.world.git(&["diff-tree", "-r", "--no-renames", "--numstat", &t1, &t2]).unwrap();
    let st = eng.world.git(&["diff-tree", "-r", "--no-renames", "--name-status", &t1, &t2]).unwrap();
    let mut git: Vec<String> = ns
        .lines()
        .zip(st.lines())
        .map(|(n, s)| {
            let f: Vec<&str> = n.split('\t').collect();
            format!("{}\t{}\t{}\t{}", s.chars().next().unwrap(), f[0], f[1], f[2])
        })
        .collect();
    git.sort_by(|a, b| a.split('\t').nth(3).cmp(&b.split('\t').nth(3)));
    assert_eq!(ours, git);
}

#[test]
fn two_sessions_in_one_worktree_do_not_collide() {
    // Two engines over the same worktree (two `rune chat` processes) snapshot concurrently.
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    for i in 0..200 {
        fs::write(wt.path().join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
    }
    let errors = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..4 {
            let (home, wt, errors) = (home.path(), wt.path(), &errors);
            s.spawn(move || {
                let eng = Engine::open_in(home, wt).unwrap();
                let b = eng.branch(&format!("s/{t}"));
                for k in 0..15 {
                    fs::write(wt.join(format!("t{t}.txt")), format!("{k}\n")).unwrap();
                    if let Err(e) = eng.record(&b, "manual", json!({})) {
                        eprintln!("thread {t}: {e:#}");
                        errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        }
    });
    assert_eq!(errors.load(std::sync::atomic::Ordering::Relaxed), 0, "concurrent snapshots failed");
}

/// Tiny deterministic RNG for the property test.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() as usize) % xs.len()]
    }
}

#[test]
fn in_process_scan_matches_git_add_write_tree() {
    use std::os::unix::fs::PermissionsExt;
    const DIRS: &[&str] = &["", "src", "src/deep/er", "docs", "target", "node_modules/x", "logs", "a b", "ünï", "keep", "build/out"];
    const NAMES: &[&str] = &["a.txt", "main.rs", "x.log", ".env.local", "run.sh", "data.db", "ok.keep", "README", "space name.md", "ä.txt", "empty"];
    for seed in 1..=12u64 {
        let mut r = Lcg(seed);
        let home = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let p = wt.path();
        fs::write(p.join(".gitignore"), "*.log\n/logs/\n!keep/*.log\nbuild/\n").unwrap();
        fs::create_dir_all(p.join("src")).unwrap();
        fs::write(p.join("src/.gitignore"), "*.tmp\n!important.tmp\n").unwrap();
        fs::create_dir_all(p.join("empty-dir/inner")).unwrap();
        let eng = Engine::open_in(home.path(), p).unwrap();
        for round in 0..6 {
            for _ in 0..25 {
                let d = r.pick(DIRS).to_string();
                let n = r.pick(NAMES).to_string();
                let rel = if d.is_empty() { n.clone() } else { format!("{d}/{n}") };
                let abs = p.join(&rel);
                fs::create_dir_all(abs.parent().unwrap()).unwrap();
                match r.next() % 7 {
                    0 => {
                        let _ = fs::remove_file(&abs);
                    }
                    1 => {
                        let _ = fs::remove_file(&abs);
                        let target = if r.next() % 2 == 0 { "a.txt" } else { "../missing" };
                        let _ = std::os::unix::fs::symlink(target, &abs);
                    }
                    2 if abs.is_file() => {
                        let mode = if r.next() % 2 == 0 { 0o755 } else { 0o644 };
                        fs::set_permissions(&abs, fs::Permissions::from_mode(mode)).unwrap();
                    }
                    3 => {
                        let _ = fs::remove_file(&abs);
                        fs::write(&abs, "").unwrap();
                    }
                    _ => {
                        if fs::symlink_metadata(&abs).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                            fs::remove_file(&abs).unwrap();
                        }
                        // Same size rewrite in the same second: only content changes.
                        fs::write(&abs, format!("round {round} {}\n", r.next() % 10)).unwrap();
                    }
                }
            }
            fs::write(p.join("src/x.tmp"), "t").unwrap();
            fs::write(p.join("src/important.tmp"), format!("{round}")).unwrap();
            let ours = eng.world.snapshot().unwrap();
            let git = eng.world.git_snapshot().unwrap();
            assert_eq!(ours, git, "seed {seed} round {round}");
            // Again from the cache, without changes.
            assert_eq!(eng.world.snapshot().unwrap(), git, "seed {seed} round {round} (cached)");
        }
    }
}

#[test]
fn stat_cache_survives_processes_and_sees_same_size_rewrites() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    for i in 0..50 {
        fs::write(wt.path().join(format!("f{i}.txt")), format!("{i:03}\n")).unwrap();
    }
    // Age the files past the racy window so they are cached.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for i in 0..50 {
        let f = fs::File::options().write(true).open(wt.path().join(format!("f{i}.txt"))).unwrap();
        f.set_modified(old).unwrap();
    }
    let t1 = Engine::open_in(home.path(), wt.path()).unwrap().world.snapshot().unwrap();
    // A new engine (a new process in hook mode) reads the persisted cache.
    let eng = Engine::open_in(home.path(), wt.path()).unwrap();
    assert_eq!(eng.world.snapshot().unwrap(), t1);
    // Same size, mtime forced back to the cached value: ctime still differs, so it is re-hashed.
    let f = wt.path().join("f7.txt");
    fs::write(&f, "777\n").unwrap();
    fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
    let t2 = eng.world.snapshot().unwrap();
    assert_ne!(t2, t1);
    assert_eq!(t2, eng.world.git_snapshot().unwrap());
}

#[test]
fn checkout_moves_between_trees_in_process() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let p = wt.path();
    fs::create_dir_all(p.join("d/e")).unwrap();
    fs::write(p.join("d/e/f.txt"), "f\n").unwrap();
    fs::write(p.join("x"), "file x\n").unwrap();
    fs::write(p.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(p.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("x", p.join("link")).unwrap();
    fs::create_dir_all(p.join("target")).unwrap();
    fs::write(p.join("target/artifact"), "keep me").unwrap();
    let eng = Engine::open_in(home.path(), p).unwrap();
    let a = eng.world.snapshot().unwrap();
    // B: x becomes a directory, d/ goes away, run.sh loses its exec bit, link points elsewhere.
    fs::remove_file(p.join("x")).unwrap();
    fs::create_dir_all(p.join("x")).unwrap();
    fs::write(p.join("x/inner"), "inner\n").unwrap();
    fs::remove_dir_all(p.join("d")).unwrap();
    fs::set_permissions(p.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(p.join("link")).unwrap();
    std::os::unix::fs::symlink("run.sh", p.join("link")).unwrap();
    let b = eng.world.snapshot().unwrap();
    assert_ne!(a, b);
    assert_eq!(eng.world.checkout(&a).unwrap(), b);
    assert_eq!(eng.world.snapshot().unwrap(), a);
    assert_eq!(eng.world.git_snapshot().unwrap(), a);
    assert_eq!(fs::read_to_string(p.join("x")).unwrap(), "file x\n");
    assert!(fs::metadata(p.join("run.sh")).unwrap().permissions().mode() & 0o100 != 0);
    assert_eq!(fs::read_link(p.join("link")).unwrap(), std::path::PathBuf::from("x"));
    assert_eq!(fs::read_to_string(p.join("target/artifact")).unwrap(), "keep me", "ignored files untouched");
    eng.world.checkout(&b).unwrap();
    assert_eq!(eng.world.snapshot().unwrap(), b);
    assert!(!p.join("d").exists(), "emptied directories are pruned");
}

#[test]
fn cached_directory_listings_follow_adds_removes_and_rule_changes() {
    // Listings are cached only for directories untouched for a while; wait it out (1 s window here).
    std::env::set_var("RUNE_RACY_SECS", "0");
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let p = wt.path();
    for d in ["a", "a/b", "c"] {
        fs::create_dir_all(p.join(d)).unwrap();
        for f in ["x.txt", "y.log", "z.rs"] {
            fs::write(p.join(d).join(f), d).unwrap();
        }
    }
    fs::write(p.join("a/.gitignore"), "*.log\n").unwrap();
    let eng = Engine::open_in(home.path(), p).unwrap();
    let settle = || std::thread::sleep(std::time::Duration::from_millis(1100));
    let check = |what: &str| {
        let ours = eng.world.snapshot().unwrap();
        let git = eng.world.git_snapshot().unwrap();
        if ours != git {
            eprintln!("{}", eng.world.git(&["diff-tree", "-r", "--name-status", &git, &ours]).unwrap());
        }
        assert_eq!(ours, git, "{what}");
        assert_eq!(eng.world.snapshot().unwrap(), ours, "{what} (again, from the cache)");
    };
    settle();
    check("start");
    settle();
    check("listings cached");
    fs::write(p.join("a/b/new.txt"), "n").unwrap();
    settle();
    check("file added in a cached directory");
    fs::remove_file(p.join("c/z.rs")).unwrap();
    settle();
    check("file removed");
    fs::write(p.join("a/.gitignore"), "*.log\n*.rs\n").unwrap();
    settle();
    check("nested .gitignore changed (applies to a/b too)");
    fs::write(eng.world.git_dir.join("info/exclude"), "*.txt\n").unwrap();
    settle();
    check("info/exclude changed");
    fs::remove_dir_all(p.join("a/b")).unwrap();
    fs::write(p.join("a/b"), "now a file").unwrap();
    settle();
    check("directory replaced by a file");
    // A fresh engine reads the persisted listings.
    let again = Engine::open_in(home.path(), p).unwrap();
    assert_eq!(again.world.snapshot().unwrap(), eng.world.git_snapshot().unwrap());
}
