//! Branch search at scale: N parallel attempts on a large tree, each doing `steps` model calls
//! (Read/Edit pairs on files in its own package). Compares the per-step cost of layered (overlay)
//! snapshots with the scan-based world on the same workload.
//!
//!   cargo run --release --example search_bench -- <worktree> <attempts> <steps>
//!
//! The tree is expected to have pkg<i>/f<k>.go files (see the README's bench section).

use runestone::agent::model::{msg, text, tool_use, Provider, Scripted};
use runestone::agent::{Config, Permission, Quiet, Session};
use runestone::layer::in_namespace;
use runestone::search::{self, Spec};
use runestone::Engine;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn script(i: usize, steps: usize) -> Vec<Value> {
    let mut v = Vec::new();
    for k in 0..steps / 2 {
        let f = format!("pkg{i}/f{k}.go");
        v.push(msg(vec![tool_use(&format!("r{i}_{k}"), "Read", json!({ "file_path": f, "limit": 3 }))]));
        v.push(msg(vec![tool_use(
            &format!("e{i}_{k}"),
            "Edit",
            json!({ "file_path": f, "old_string": "package x", "new_string": format!("package x // attempt {i} step {k}") }),
        )]));
    }
    v.push(msg(vec![text("done")]));
    v
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if !in_namespace() && std::env::var("RUNE_COPY").is_err() {
        let st = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount"])
            .arg(std::env::current_exe()?)
            .args(&args[1..])
            .env("RUNE_NS", "1")
            .status()?;
        std::process::exit(st.code().unwrap_or(1));
    }
    let wt = std::path::PathBuf::from(&args[1]);
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let steps: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    let home = tempfile::tempdir()?;
    let eng = Engine::open_in(home.path(), &wt)?;

    // Baseline: one attempt's workload on the scan-based world.
    let b0 = eng.branch("s/scan");
    Session::spawn(&eng, &b0, &Config::default(), "2026-11-11")?;
    let t = Instant::now();
    let mut s = Session {
        eng: &eng,
        branch: b0.clone(),
        provider: Box::new(Scripted::new(script(199, steps))),
        ui: Box::new(Quiet),
        perm: Permission::Auto,
        max_steps: steps + 5,
        checks: None,
    };
    s.turn("bench")?;
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    let scan_events = eng.lineage(&b0)?.len();
    // put the files back so the search starts from the original tree
    let first = eng.lineage(&b0)?.into_iter().find(|e| e.kind == "spawn").unwrap();
    eng.world.checkout(first.tree().unwrap())?;
    println!("scan world:    1 attempt × {steps} steps: {scan_ms:.0} ms  ({:.1} ms/step, {scan_events} events)", scan_ms / (steps + 1) as f64);

    // Layered search.
    let b = eng.branch("s/main");
    Session::spawn(&eng, &b, &Config::default(), "2026-11-11")?;
    let spec = Spec {
        prompt: "bench".into(),
        hypotheses: (0..n).map(|i| format!("approach {i}")).collect(),
        visible: Some("true".into()),
        hidden: None,
        max_steps: steps + 5,
        perm: Permission::Auto,
        timeout: Duration::from_secs(120),
        keep_dirs: false,
    };
    let scripts: Vec<Vec<Value>> = (0..n).map(|i| script(i, steps)).collect();
    let factory = |i: usize| -> Box<dyn Provider> { Box::new(Scripted::new(scripts[i].clone())) };
    let r = search::run(&eng, &b, &spec, &factory)?;
    let errs: Vec<_> = r.outcomes.iter().filter_map(|o| o.error.clone()).collect();
    let ms: Vec<u64> = r.outcomes.iter().map(|o| o.ms).collect();
    let mounts: Vec<f64> = r.outcomes.iter().map(|o| o.mount_ms).collect();
    let mean = ms.iter().sum::<u64>() as f64 / ms.len() as f64;
    println!(
        "layered world: {n} attempts × {steps} steps in parallel: {} ms wall (base materialized in {} ms)",
        r.total_ms, r.base_ms
    );
    println!(
        "               per attempt {mean:.0} ms ({:.1} ms/step), mount {:.1}–{:.1} ms, merged: {}, errors: {}",
        mean / (steps + 1) as f64,
        mounts.iter().cloned().fold(f64::MAX, f64::min),
        mounts.iter().cloned().fold(0.0, f64::max),
        r.merged,
        errs.len()
    );
    for e in errs.iter().take(3) {
        println!("  error: {e}");
    }
    Ok(())
}
