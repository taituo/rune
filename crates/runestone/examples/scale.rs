//! How does a step's cost grow with session length? Runs N scripted tool steps against a worktree and
//! reports, at checkpoints: wall time per step, and the split between fold+render and recording.
//!
//!   cargo run --release --example scale -- <worktree> <steps>

use runestone::agent::fold::{effective_chain, fold};
use runestone::agent::model::{msg, text, tool_use, Scripted};
use runestone::agent::{render, Config, Permission, Quiet, Session};
use runestone::Engine;
use serde_json::json;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let wt = std::path::PathBuf::from(&args[1]);
    let steps: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500);
    let home = tempfile::tempdir()?;
    let eng = Engine::open_in(home.path(), &wt)?;
    let file = std::fs::read_dir(&wt)?
        .flatten()
        .find(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .expect("a file in the worktree");

    let mut script = Vec::new();
    for i in 0..steps {
        script.push(msg(vec![text("step"), tool_use(&format!("t{i}"), "Read", json!({ "file_path": file, "limit": 5 }))]));
    }
    script.push(msg(vec![text("done")]));
    let branch = eng.branch("s/scale");
    Session::spawn(&eng, &branch, &Config::default(), "2026-11-11")?;

    // Drive the loop one step at a time by giving the session a 1-response script per step.
    let marks = [10usize, 100, 250, 500, 1000, 2000];
    let mut s = Session { eng: &eng, branch: branch.clone(), provider: Box::new(Scripted::new(vec![])), ui: Box::new(Quiet), perm: Permission::Auto, max_steps: 1, checks: None };
    eng.record(&branch, "turn.start", json!({ "prompt": "scale test" }))?;
    let mut last = Instant::now();
    println!("{:>6} {:>10} {:>12} {:>12} {:>10}", "step", "events", "step ms", "fold+render", "req KB");
    for (i, resp) in script.into_iter().enumerate().take(steps) {
        s.provider = Box::new(Scripted::new(vec![resp]));
        let _ = s.run_loop();
        let n = i + 1;
        if marks.contains(&n) || n == steps {
            let step_ms = last.elapsed().as_secs_f64() * 1000.0;
            let head = eng.store.head(&branch)?.unwrap();
            let t = Instant::now();
            let chain = effective_chain(&eng.store, &head)?;
            let _ = fold(&eng.store, &chain)?;
            let req = render(&eng.store, &chain)?;
            let fr = t.elapsed().as_secs_f64() * 1000.0;
            println!("{n:>6} {:>10} {step_ms:>12.1} {fr:>12.1} {:>10}", chain.len(), req.len() / 1024);
        }
        last = Instant::now();
    }
    Ok(())
}
