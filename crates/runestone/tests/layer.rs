//! Layered (overlay) worlds. These tests re-run themselves inside an unprivileged user + mount
//! namespace, because mounting overlayfs needs one.

use runestone::layer::{ensure_base, in_namespace, Layer};
use runestone::{Engine, World};
use std::fs;
use std::process::Command;

/// Returns true when the caller should run the test body (we are inside the namespace).
fn in_ns(name: &str) -> bool {
    if in_namespace() {
        return true;
    }
    let ok = Command::new("unshare").args(["--user", "--map-root-user", "--mount", "true"]).status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        // Visible only with --nocapture; RUNE_REQUIRE_NS=1 turns the skip into a failure.
        assert!(std::env::var("RUNE_REQUIRE_NS").is_err(), "{name}: no unprivileged user namespaces (RUNE_REQUIRE_NS=1)");
        eprintln!("SKIPPED {name}: no unprivileged user namespaces");
        return false;
    }
    let exe = std::env::current_exe().unwrap();
    let st = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .arg(exe)
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("RUNE_NS", "1")
        .status()
        .unwrap();
    assert!(st.success(), "{name} failed inside the namespace");
    false
}

#[test]
fn layered_snapshot_matches_full_scan() {
    if !in_ns("layered_snapshot_matches_full_scan") {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let d = wt.path();
    fs::create_dir_all(d.join("src/old")).unwrap();
    fs::write(d.join(".gitignore"), "*.log\n").unwrap();
    fs::write(d.join("src/a.rs"), "a\n").unwrap();
    fs::write(d.join("src/b.rs"), "b\n").unwrap();
    fs::write(d.join("src/old/x.rs"), "x\n").unwrap();
    fs::write(d.join("src/old/y.rs"), "y\n").unwrap();
    fs::write(d.join("run.sh"), "echo hi\n").unwrap();
    let eng = Engine::open_in(home.path(), d).unwrap();
    let base = eng.world.snapshot().unwrap();
    let base_dir = ensure_base(&eng.world, home.path(), &base).unwrap();
    let layer = Layer::create(home.path(), "t1", &base, vec![base_dir]).unwrap();
    layer.mount().unwrap();
    let lw = World::layered(&eng.world, &layer, "fork-t1").unwrap();
    assert_eq!(lw.snapshot().unwrap(), base, "untouched layer = base tree");

    let m = &layer.merged;
    fs::write(m.join("src/a.rs"), "a changed\n").unwrap(); // modify
    fs::write(m.join("src/c.rs"), "c\n").unwrap(); // add
    fs::remove_file(m.join("src/b.rs")).unwrap(); // delete -> whiteout
    fs::remove_dir_all(m.join("src/old")).unwrap(); // replace a dir -> opaque
    fs::create_dir(m.join("src/old")).unwrap();
    fs::write(m.join("src/old/z.rs"), "z\n").unwrap();
    fs::write(m.join("debug.log"), "ignored\n").unwrap(); // .gitignore
    fs::create_dir_all(m.join("target/debug")).unwrap();
    fs::write(m.join("target/debug/bin"), "artefact").unwrap(); // build output
    Command::new("chmod").args(["+x", m.join("run.sh").to_str().unwrap()]).status().unwrap(); // mode only
    std::os::unix::fs::symlink("src/a.rs", m.join("link")).unwrap();

    let layered = lw.snapshot().unwrap();
    let again = lw.snapshot().unwrap();
    assert_eq!(layered, again, "stable when nothing changed");

    // Reference: a full scan of the same view, in a separate shadow repo.
    let scan = World::open(home.path(), m).unwrap().snapshot().unwrap();
    assert_eq!(layered, scan, "layered snapshot must equal a full scan");

    let names = lw.git(&["ls-tree", "-r", "--name-only", &layered]).unwrap();
    assert!(names.contains("src/old/z.rs") && !names.contains("src/old/x.rs") && !names.contains("src/b.rs"));
    assert!(!names.contains("debug.log") && !names.contains("target/"));

    // The live worktree was never touched.
    assert_eq!(fs::read_to_string(d.join("src/a.rs")).unwrap(), "a\n");
    assert!(d.join("src/b.rs").exists());
    layer.remove();
}
