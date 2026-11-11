//! Automatic checks: build and test a recorded world, not the live directory.
//!
//! A check runs in a long-lived per-worktree workspace that is moved to the checked tree by rewriting
//! only changed files; ignored build output stays, so incremental builds stay warm. It runs without
//! network when the machine allows. Results are keyed by (tree, command): a world is never checked twice.

use crate::engine::Engine;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Score {
    pub passed: u64,
    pub failed: u64,
    pub green: bool,
    /// Higher is better. Passed tests when the output says, else 1 for green and 0 for red.
    pub value: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub cmd: String,
    pub tree: String,
    pub exit: i64,
    pub score: Score,
    pub tail: String,
    pub duration_ms: u64,
    pub cached: bool,
}

/// Guess the project's check command from its files.
pub fn detect(dir: &Path) -> Option<String> {
    let has = |f: &str| dir.join(f).exists();
    if has("Cargo.toml") {
        return Some("cargo test --quiet 2>&1".into());
    }
    if has("go.mod") {
        return Some("go test ./... 2>&1".into());
    }
    if has("package.json") {
        let pj = std::fs::read_to_string(dir.join("package.json")).unwrap_or_default();
        if pj.contains("\"test\"") {
            return Some("npm test --silent 2>&1".into());
        }
    }
    if has("pyproject.toml") || has("pytest.ini") || has("setup.py") || dir.join("tests").is_dir() {
        return Some("python3 -m pytest -q 2>&1".into());
    }
    if let Ok(mk) = std::fs::read_to_string(dir.join("Makefile")) {
        if mk.lines().any(|l| l.starts_with("test:")) {
            return Some("make test 2>&1".into());
        }
    }
    None
}

/// Parse common test runners' summaries. `rune-score: N` in the output overrides everything.
pub fn parse(output: &str, exit: i64) -> Score {
    let green = exit == 0;
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut found = false;
    let mut explicit: Option<f64> = None;
    let num_before = |line: &str, word: &str| -> Option<u64> {
        let i = line.find(word)?;
        line[..i].trim_end().rsplit(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    };
    for line in output.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("rune-score:") {
            explicit = v.trim().parse().ok();
        }
        // cargo: "test result: ok. 8 passed; 0 failed; ..."
        // pytest: "3 passed, 1 failed in 0.1s"; jest: "Tests: 1 failed, 3 passed, 4 total"
        if l.starts_with("test result:") || l.contains(" passed") || l.contains(" failed") {
            let p = num_before(l, " passed");
            let f = num_before(l, " failed");
            if p.is_some() || f.is_some() {
                found = true;
                passed += p.unwrap_or(0);
                failed += f.unwrap_or(0);
            }
        }
        // go: "ok  	pkg	0.1s" / "FAIL	pkg	0.1s"
        if l.starts_with("ok  \t") || (l.starts_with("ok ") && l.contains('\t')) {
            passed += 1;
            found = true;
        } else if l.starts_with("FAIL\t") {
            failed += 1;
            found = true;
        }
    }
    let value = match explicit {
        Some(v) => v,
        None if found => passed as f64,
        None => {
            if green {
                1.0
            } else {
                0.0
            }
        }
    };
    Score { passed, failed, green, value }
}

pub fn run(eng: &Engine, tree: &str, cmd: &str, timeout: Duration) -> Result<CheckResult> {
    if let Some((exit, out)) = eng.store.result(tree, cmd)? {
        return Ok(CheckResult {
            cmd: cmd.into(),
            tree: tree.into(),
            exit,
            score: parse(&out, exit),
            tail: tail(&out, 40),
            duration_ms: 0,
            cached: true,
        });
    }
    let dir = eng.home.join("checks").join(&eng.world.key);
    eng.world.checkout_into(&dir, tree)?;
    let (exit, out, duration_ms) = exec(&dir, cmd, timeout)?;
    let kept = tail(&out, 200);
    eng.store.put_result(tree, cmd, exit, &kept)?;
    Ok(CheckResult {
        cmd: cmd.into(),
        tree: tree.into(),
        exit,
        // Scored from the same kept tail as a cache hit would be, so fresh and cached results agree.
        score: parse(&kept, exit),
        tail: tail(&out, 40),
        duration_ms,
        cached: false,
    })
}

/// Run `cmd` in `dir` (offline when possible), in its own process group, killed on timeout.
/// Returns (exit, combined output, milliseconds). Exit -2 = timed out.
pub fn exec(dir: &Path, cmd: &str, timeout: Duration) -> Result<(i64, String, u64)> {
    let start = Instant::now();
    let mut c = Command::new("bash");
    c.arg("-c").arg(cmd);
    if crate::agent::tools::offline_available() {
        crate::sandbox::offline(&mut c, true); // no network, loopback up
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out_path = std::env::temp_dir().join(format!("rune-check-{}-{seq}.out", std::process::id()));
    let f = std::fs::File::create(&out_path)?;
    let mut child = std::os::unix::process::CommandExt::process_group(
        c.current_dir(dir).stdin(Stdio::null()).stdout(f.try_clone()?).stderr(f).env("CI", "1").env("RUNE_CHECK", "1"),
        0,
    )
    .spawn()?;
    let exit = loop {
        if let Some(s) = child.try_wait()? {
            break s.code().unwrap_or(-1) as i64;
        }
        if start.elapsed() > timeout {
            let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).status();
            let _ = child.wait();
            break -2;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = String::from_utf8_lossy(&std::fs::read(&out_path).unwrap_or_default()).into_owned();
    let _ = std::fs::remove_file(&out_path);
    let out = if exit == -2 { format!("{out}\n[check timed out after {}s]", timeout.as_secs()) } else { out };
    Ok((exit, out, start.elapsed().as_millis() as u64))
}

fn tail(s: &str, lines: usize) -> String {
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}
