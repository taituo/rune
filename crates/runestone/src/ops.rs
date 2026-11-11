//! Fork and turn bisect: cheap once worlds are content-addressed.

use crate::engine::{changes, Engine};
use crate::store::Event;
use anyhow::{bail, Result};
use serde_json::json;
use std::path::Path;
use std::process::Command;

/// Materialize the world at `at` into `dir` and start a new branch there. The new branch's first event
/// points at `at`, so its history is shared, not copied.
pub fn fork(eng: &Engine, at: &Event, name: &str, dir: &Path) -> Result<(String, Event)> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} is not empty", dir.display());
    }
    let tree = at.tree().unwrap_or_default().to_string();
    eng.world.materialize(&tree, dir)?;
    let child = Engine::open_in(&eng.home, dir)?;
    let branch = child.branch(name);
    if eng.store.head(&branch)?.is_some() {
        bail!("branch {branch} exists");
    }
    // Seed the child's object store with the parent's objects via alternates (no copy).
    let alt = child.world.git_dir.join("objects/info/alternates");
    std::fs::create_dir_all(alt.parent().unwrap())?;
    std::fs::write(&alt, format!("{}\n", eng.world.git_dir.join("objects").display()))?;
    let body = json!({
        "tree": tree, "turn": at.turn().unwrap_or(0), "worktree": child.world.key,
        "author": "fork", "changed": [], "forked_from": at.id, "from_worktree": eng.world.key,
    });
    let ev = eng.store.put(vec![at.id.clone()], "fork", body)?;
    if !eng.store.cas_head(&branch, None, &ev.id)? {
        bail!("branch {branch} exists");
    }
    Ok((branch, ev))
}

pub struct Probe {
    pub event: Event,
    pub exit: i64,
    pub cached: bool,
}

/// Run `cmd` against the world at `ev`, cached by (tree, cmd).
pub fn probe(eng: &Engine, ev: &Event, cmd: &str) -> Result<Probe> {
    let tree = ev.tree().unwrap_or_default();
    if let Some((exit, _)) = eng.store.result(tree, cmd)? {
        return Ok(Probe { event: ev.clone(), exit, cached: true });
    }
    let dir = eng.home.join("tmp").join(format!("probe-{}-{}", &tree[..12], std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    eng.world.materialize(tree, &dir)?;
    let out = Command::new("sh").arg("-c").arg(cmd).current_dir(&dir).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    let exit = out.status.code().unwrap_or(-1) as i64;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let tail: String = text.chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect();
    eng.store.put_result(tree, cmd, exit, &tail)?;
    Ok(Probe { event: ev.clone(), exit, cached: false })
}

pub struct Bisect {
    pub first_bad: Event,
    pub last_good: Event,
    pub probes: Vec<Probe>,
}

/// Find the first recorded world on `branch` where `cmd` fails, assuming it passes at the first candidate.
/// Candidates are worlds that changed (one per distinct tree), so tool-level precision comes for free.
pub fn bisect(eng: &Engine, branch: &str, cmd: &str) -> Result<Bisect> {
    let mut cands: Vec<Event> = Vec::new();
    for ev in eng.lineage(branch)? {
        let new_tree = cands.last().map(|l: &Event| l.tree() != ev.tree()).unwrap_or(true);
        if new_tree && ev.tree().is_some() {
            cands.push(ev);
        }
    }
    if cands.len() < 2 {
        bail!("need at least two distinct worlds on {branch}");
    }
    let mut probes = Vec::new();
    let (mut lo, mut hi) = (0usize, cands.len() - 1);
    let first = probe(eng, &cands[lo], cmd)?;
    let good = first.exit == 0;
    probes.push(first);
    if !good {
        bail!("`{cmd}` already fails at the first world ({})", cands[lo].short());
    }
    let last = probe(eng, &cands[hi], cmd)?;
    let bad = last.exit != 0;
    probes.push(last);
    if !bad {
        bail!("`{cmd}` passes at the head; nothing to bisect");
    }
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        let p = probe(eng, &cands[mid], cmd)?;
        if p.exit == 0 {
            lo = mid;
        } else {
            hi = mid;
        }
        probes.push(p);
    }
    Ok(Bisect { first_bad: cands[hi].clone(), last_good: cands[lo].clone(), probes })
}

pub fn describe(ev: &Event) -> String {
    let ch: Vec<String> = changes(ev).iter().map(|c| format!("{} {} ({})", c.status, c.path, c.stat())).collect();
    format!(
        "{} t{} {} by {} {} [{}]",
        ev.short(),
        ev.turn().unwrap_or(0),
        ev.kind,
        ev.str("author").unwrap_or("?"),
        ev.str("summary").map(|s| format!("\"{s}\"")).unwrap_or_default(),
        ch.join(", ")
    )
}
