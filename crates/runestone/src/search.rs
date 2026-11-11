//! Branch search over real worlds.
//!
//! From the current state of a session, N attempts run in parallel, each in its own overlay fork and on
//! its own branch of the record, each told to follow a different hypothesis (diversity is forced at the
//! branch point; sampling alone mostly repeats the same first steps). Every attempt is scored by the
//! visible check and by hidden tests the agents never see: the hidden files are stacked on top of the
//! attempt's layer only at evaluation time, so an attempt cannot read or overwrite them. The best attempt
//! is merged into the live worktree keep-human, and the session continues from its conversation. The
//! others stay in the record as branches.

use crate::agent::fold::{effective_chain, spawn_config};
use crate::agent::model::Provider;
use crate::agent::{Permission, Quiet, Session};
use crate::checks::{exec, parse, Score};
use crate::engine::Engine;
use crate::layer::{ensure_base, in_namespace, Layer};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Hidden {
    pub dir: PathBuf,
    pub cmd: String,
}

pub struct Spec {
    pub prompt: String,
    pub hypotheses: Vec<String>,
    pub visible: Option<String>,
    pub hidden: Option<Hidden>,
    pub max_steps: usize,
    pub perm: Permission,
    pub timeout: Duration,
    pub keep_dirs: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub index: usize,
    pub hypothesis: String,
    pub branch: String,
    pub tree: Option<String>,
    pub steps: usize,
    pub stop: String,
    pub visible: Option<Score>,
    pub hidden: Option<Score>,
    pub diff_lines: u64,
    pub files: Vec<String>,
    pub error: Option<String>,
    pub mount_ms: f64,
    pub ms: u64,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub base_tree: String,
    pub start: String,
    pub outcomes: Vec<Outcome>,
    pub winner: Option<usize>,
    pub merged: bool,
    pub merge_conflicts: Vec<String>,
    pub base_ms: u64,
    pub total_ms: u64,
}

pub type ProviderFactory<'a> = dyn Fn(usize) -> Box<dyn Provider> + Sync + 'a;

pub fn run(eng: &Engine, branch: &str, spec: &Spec, providers: &ProviderFactory) -> Result<SearchResult> {
    if !in_namespace() {
        bail!("search needs a user + mount namespace for overlay forks (the rune CLI sets this up)");
    }
    if spec.hypotheses.is_empty() {
        bail!("search needs at least one hypothesis");
    }
    let head = eng.store.head(branch)?.with_context(|| format!("no branch {branch}"))?;
    if spawn_config(&effective_chain(&eng.store, &head)?).is_none() {
        bail!("{branch} has no rune session to search from");
    }
    let t0 = Instant::now();
    let sid = format!("{:x}", (crate::store::now() * 1000.0) as u64);
    let start = eng.record(
        branch,
        "search.start",
        json!({ "search": sid, "prompt": spec.prompt, "hypotheses": spec.hypotheses, "visible": spec.visible }),
    )?;
    let base = start.tree().unwrap_or_default().to_string();
    let base_dir = ensure_base(&eng.world, &eng.home, &base)?;
    let base_ms = t0.elapsed().as_millis() as u64;

    let n = spec.hypotheses.len();
    let (home, main_wt, start_id) = (eng.home.clone(), eng.world.worktree.clone(), start.id.clone());
    let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = spec
            .hypotheses
            .iter()
            .enumerate()
            .map(|(i, hyp)| {
                let (home, main_wt, start_id, base, base_dir, sid) =
                    (home.clone(), main_wt.clone(), start_id.clone(), base.clone(), base_dir.clone(), sid.clone());
                scope.spawn(move || {
                    let t = Instant::now();
                    let mut o = Outcome {
                        index: i,
                        hypothesis: hyp.clone(),
                        branch: String::new(),
                        tree: None,
                        steps: 0,
                        stop: String::new(),
                        visible: None,
                        hidden: None,
                        diff_lines: 0,
                        files: vec![],
                        error: None,
                        mount_ms: 0.0,
                        ms: 0,
                    };
                    if let Err(e) = attempt(&mut o, &home, &main_wt, &start_id, &base, &base_dir, &sid, n, spec, providers) {
                        o.error = Some(format!("{e:#}"));
                    }
                    o.ms = t.elapsed().as_millis() as u64;
                    o
                })
            })
            .collect();
        handles
            .into_iter()
            .enumerate()
            .map(|(i, h)| {
                h.join().unwrap_or_else(|_| Outcome {
                    index: i,
                    hypothesis: spec.hypotheses[i].clone(),
                    branch: String::new(),
                    tree: None,
                    steps: 0,
                    stop: "panic".into(),
                    visible: None,
                    hidden: None,
                    diff_lines: 0,
                    files: vec![],
                    error: Some("attempt panicked".into()),
                    mount_ms: 0.0,
                    ms: 0,
                })
            })
            .collect()
    });

    let winner = select(&outcomes, spec);
    let mut merged = false;
    let mut conflicts = vec![];
    let summary: Vec<_> = outcomes
        .iter()
        .map(|o| json!({ "i": o.index, "hypothesis": o.hypothesis, "branch": o.branch, "visible": o.visible, "hidden": o.hidden, "diff_lines": o.diff_lines, "error": o.error }))
        .collect();
    match winner {
        Some(w) => {
            let win = &outcomes[w];
            let wtree = win.tree.clone().unwrap_or_default();
            // Keep-human merge: whatever changed in the live tree during the search stays.
            let live = eng.world.snapshot()?;
            let m = eng.world.cherry(&base, &live, &wtree)?;
            if m.conflicts.is_empty() {
                eng.world.checkout(&m.tree)?;
                let note = format!(
                    "[harness event, not from the user] {n} attempts ran in parallel from the same point; this one (approach: {}) was selected{}. Its changes are now in the worktree.",
                    win.hypothesis,
                    match (&win.hidden, &win.visible) {
                        (Some(h), _) => format!(" on held-out checks ({} passed)", h.passed),
                        (None, Some(v)) => format!(" on the visible check ({} passed)", v.passed),
                        _ => String::new(),
                    }
                );
                let ev = eng.record_tree(
                    &win.branch,
                    "search.merge",
                    &m.tree,
                    json!({ "search": sid, "winner": w, "outcomes": summary, "note": note }),
                )?;
                // The session continues from the winner's conversation, if nobody moved it meanwhile.
                merged = eng.store.cas_head(branch, Some(&start.id), &ev.id)?;
                if !merged {
                    conflicts.push(format!("{branch} moved during the search; winner left on {}", win.branch));
                }
            } else {
                conflicts = m.conflicts;
                eng.record(branch, "search.end", json!({ "search": sid, "winner": w, "outcomes": summary, "conflicts": conflicts }))?;
            }
        }
        None => {
            eng.record(branch, "search.end", json!({ "search": sid, "winner": null, "outcomes": summary }))?;
        }
    }
    Ok(SearchResult {
        id: sid,
        base_tree: base,
        start: start.id,
        outcomes,
        winner,
        merged,
        merge_conflicts: conflicts,
        base_ms,
        total_ms: t0.elapsed().as_millis() as u64,
    })
}

#[allow(clippy::too_many_arguments)]
fn attempt(
    o: &mut Outcome,
    home: &std::path::Path,
    main_wt: &std::path::Path,
    start_id: &str,
    base: &str,
    base_dir: &std::path::Path,
    sid: &str,
    n: usize,
    spec: &Spec,
    providers: &ProviderFactory,
) -> Result<()> {
    let i = o.index;
    let parent = Engine::open_in(home, main_wt)?;
    let fork_branch = parent.branch(&format!("search/{sid}/{i}"));
    o.branch = fork_branch.clone();
    let key = format!("{sid}-{i}");
    let layer = Layer::create(home, &key, base, vec![base_dir.to_path_buf()])?;
    let tm = Instant::now();
    layer.mount()?;
    o.mount_ms = tm.elapsed().as_secs_f64() * 1000.0;
    let result = (|| -> Result<()> {
        let feng = Engine::open_layered(&parent, &layer, &key)?;
        let start = feng.store.get(start_id)?;
        let fork = feng.store.put(
            vec![start_id.to_string()],
            "fork",
            json!({ "tree": base, "turn": start.turn().unwrap_or(0), "worktree": key, "author": "fork", "changed": [],
                    "search": sid, "attempt": i, "hypothesis": o.hypothesis }),
        )?;
        if !feng.store.cas_head(&fork_branch, None, &fork.id)? {
            bail!("branch {fork_branch} exists");
        }
        let note = format!(
            "[harness event, not from the user] This is attempt {} of {n}, running in parallel with others from the same starting point. Take this approach: {}\nStay with this approach; the best attempt will be kept.",
            i + 1,
            o.hypothesis
        );
        let mut s = Session {
            eng: &feng,
            branch: fork_branch.clone(),
            provider: providers(i),
            ui: Box::new(Quiet),
            perm: spec.perm,
            max_steps: spec.max_steps,
            checks: None,
        };
        match s.turn_with(&spec.prompt, Some(&note)) {
            Ok(r) => {
                o.steps = r.steps;
                o.stop = r.stop;
            }
            Err(e) => {
                o.stop = "error".into();
                o.error = Some(format!("{e:#}"));
            }
        }
        let tree = feng.world.snapshot()?;
        let changes = feng.world.diff(base, &tree)?;
        o.diff_lines = changes.iter().map(|c| c.added.unwrap_or(1) + c.deleted.unwrap_or(1)).sum();
        o.files = changes.iter().map(|c| format!("{} {}", c.status, c.path)).collect();
        if let Some(cmd) = &spec.visible {
            let (exit, out, _) = exec(&layer.merged, cmd, spec.timeout)?;
            o.visible = Some(parse(&out, exit));
        }
        o.tree = Some(tree.clone());
        feng.record_tree(
            &fork_branch,
            "search.result",
            &tree,
            json!({ "search": sid, "attempt": i, "visible": o.visible, "diff_lines": o.diff_lines }),
        )?;
        let _ = std::fs::remove_file(&feng.world.index_path);
        Ok(())
    })();
    layer.umount()?;
    // Held-out evaluation: hidden files stacked over this attempt's layer, which is now read-only.
    if result.is_ok() {
        if let Some(h) = &spec.hidden {
            let ev = Layer::create(home, &format!("{key}-eval"), base, vec![h.dir.clone(), layer.upper.clone(), base_dir.to_path_buf()])?;
            ev.mount()?;
            let r = exec(&ev.merged, &h.cmd, spec.timeout);
            ev.remove();
            let (exit, out, _) = r?;
            o.hidden = Some(parse(&out, exit));
        }
    }
    if !spec.keep_dirs {
        layer.remove();
    }
    result
}

/// Best attempt: must pass the strongest check available (hidden, else visible); then by hidden score,
/// visible score, and the smallest diff.
pub fn select(outcomes: &[Outcome], spec: &Spec) -> Option<usize> {
    let passes = |o: &Outcome| match (&spec.hidden, &spec.visible) {
        (Some(_), _) => o.hidden.as_ref().is_some_and(|s| s.green),
        (None, Some(_)) => o.visible.as_ref().is_some_and(|s| s.green),
        _ => true,
    };
    let v = |s: &Option<Score>| s.as_ref().map(|s| s.value).unwrap_or(0.0);
    outcomes
        .iter()
        .filter(|o| o.error.is_none() && o.tree.is_some() && passes(o))
        .max_by(|a, b| {
            v(&a.hidden)
                .total_cmp(&v(&b.hidden))
                .then(v(&a.visible).total_cmp(&v(&b.visible)))
                .then(b.diff_lines.cmp(&a.diff_lines))
                .then(b.index.cmp(&a.index))
        })
        .map(|o| o.index)
}
