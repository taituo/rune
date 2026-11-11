//! Keep-human rewind as a rebase: start from the world at the target event and replay every non-agent
//! segment after it (outside edits, unknown writers, and live changes not yet recorded), skipping the
//! agent's. Each replay is a plumbing cherry-pick, so nothing touches the worktree until the plan is
//! applied.
//!
//! A human hunk that depended on agent work after the target no longer has a base. It shows up as a
//! conflict and is parked as a patch, never merged: a fix merged onto code that is gone is confident
//! nonsense. The rewind is a new event; the pre-rewind tree stays in the record, so undo is a pointer move.

use crate::engine::{changes, Engine};
use crate::store::Event;
use crate::world::Change;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    KeepHuman,
    Hard,
}

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub event: String,
    pub kind: String,
    pub author: String,
    pub turn: u64,
    pub label: String,
    pub changed: Vec<String>,
    #[serde(skip)]
    pub from_tree: String,
    #[serde(skip)]
    pub to_tree: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Parked {
    pub event: String,
    pub paths: Vec<String>,
    pub patch: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct External {
    pub event: String,
    pub turn: u64,
    pub label: String,
    pub state: String,
    pub disclosure: String,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub branch: String,
    pub mode: Mode,
    pub target: String,
    pub target_turn: u64,
    pub live_tree: String,
    pub result_tree: String,
    pub dropped: Vec<Segment>,
    pub kept: Vec<Segment>,
    pub parked: Vec<Parked>,
    pub external: Vec<External>,
    pub final_changes: Vec<String>,
}

fn label(ev: &Event) -> String {
    let tool = ev.str("tool").unwrap_or("");
    let summary = ev.str("summary").unwrap_or("");
    match ev.kind.as_str() {
        "tool.post" | "tool.pre" => format!("{tool} {summary}").trim().to_string(),
        k => k.to_string(),
    }
}

pub fn plan(eng: &Engine, branch: &str, target: &Event, mode: Mode) -> Result<Plan> {
    let lineage = eng.lineage(branch)?;
    let pos = lineage
        .iter()
        .position(|e| e.id == target.id)
        .ok_or_else(|| anyhow::anyhow!("event {} is not on branch {branch}", target.short()))?;
    let base = target.tree().unwrap_or_default().to_string();
    let live = eng.world.snapshot()?;

    // Segments after the target, plus the live tail (changes since the last recorded event).
    let mut segs: Vec<Segment> = Vec::new();
    let mut external = Vec::new();
    for w in lineage[pos..].windows(2) {
        let (prev, cur) = (&w[0], &w[1]);
        let cls = cur.body.get("class");
        if let Some(c) = cls {
            let state = c.get("state").and_then(Value::as_str).unwrap_or("unknown");
            let disc = c.get("disclosure").and_then(Value::as_str).unwrap_or("none");
            let crosses = matches!(state, "compensable" | "irreversible" | "unknown") || disc == "external";
            if crosses && cur.kind == "tool.pre" {
                external.push(External {
                    event: cur.short().into(),
                    turn: cur.turn().unwrap_or(0),
                    label: label(cur),
                    state: state.into(),
                    disclosure: disc.into(),
                });
            }
        }
        let ch: Vec<Change> = changes(cur);
        if ch.is_empty() {
            continue;
        }
        segs.push(Segment {
            event: cur.short().into(),
            kind: cur.kind.clone(),
            author: cur.str("author").unwrap_or("unknown").into(),
            turn: cur.turn().unwrap_or(0),
            label: label(cur),
            changed: ch.iter().map(|c| c.path.clone()).collect(),
            from_tree: prev.tree().unwrap_or_default().into(),
            to_tree: cur.tree().unwrap_or_default().into(),
        });
    }
    let head_tree = lineage.last().and_then(Event::tree).unwrap_or_default().to_string();
    if !head_tree.is_empty() && head_tree != live {
        let ch = eng.world.diff(&head_tree, &live)?;
        segs.push(Segment {
            event: "live".into(),
            kind: "live".into(),
            author: "outside".into(),
            turn: lineage.last().and_then(Event::turn).unwrap_or(0),
            label: "changes since the last recorded event".into(),
            changed: ch.iter().map(|c| c.path.clone()).collect(),
            from_tree: head_tree,
            to_tree: live.clone(),
        });
    }

    let mut cur = base.clone();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut parked = Vec::new();
    for s in segs {
        let keep = mode == Mode::KeepHuman && matches!(s.author.as_str(), "outside" | "unknown");
        if !keep {
            dropped.push(s);
            continue;
        }
        let m = eng.world.cherry(&s.from_tree, &cur, &s.to_tree)?;
        if m.conflicts.is_empty() {
            cur = m.tree;
        } else {
            // Apply the clean paths, keep ours for the conflicted ones, park their patch.
            cur = eng.world.take_paths(&m.tree, &cur, &m.conflicts)?;
            let patch = eng.world.patch(&s.from_tree, &s.to_tree, &m.conflicts)?;
            parked.push(Parked { event: s.event.clone(), paths: m.conflicts.clone(), patch });
        }
        kept.push(s);
    }
    let final_changes = eng
        .world
        .diff(&live, &cur)?
        .iter()
        .map(|c| format!("{} {}", c.status, c.path))
        .collect();
    Ok(Plan {
        branch: branch.into(),
        mode,
        target: target.id.clone(),
        target_turn: target.turn().unwrap_or(0),
        live_tree: live,
        result_tree: cur,
        dropped,
        kept,
        parked,
        external,
        final_changes,
    })
}

impl Plan {
    pub fn render(&self) -> String {
        let mut s = String::new();
        let mode = match self.mode {
            Mode::KeepHuman => "keep-human",
            Mode::Hard => "hard",
        };
        s += &format!("rewind {} to turn {} @{} ({mode})\n", self.branch, self.target_turn, &self.target[..12]);
        let agent_calls = self.dropped.iter().filter(|d| d.author == "agent").count();
        let dfiles: std::collections::BTreeSet<&String> = self.dropped.iter().flat_map(|d| &d.changed).collect();
        s += &format!("  revert   {} segments ({} agent calls) in {} files\n", self.dropped.len(), agent_calls, dfiles.len());
        for d in &self.dropped {
            s += &format!("           t{} {:<8} {:<7} {}  [{}]\n", d.turn, d.event, d.author, d.label, d.changed.join(", "));
        }
        s += &format!("  keep     {} non-agent segments\n", self.kept.len());
        for k in &self.kept {
            s += &format!("           t{} {:<8} {:<7} [{}]\n", k.turn, k.event, k.author, k.changed.join(", "));
        }
        if !self.parked.is_empty() {
            s += &format!("  parked   {} dependent hunks (edited code the agent wrote after the target)\n", self.parked.len());
            for p in &self.parked {
                s += &format!("           {} [{}]\n", p.event, p.paths.join(", "));
            }
        }
        if !self.external.is_empty() {
            s += "  outside world, still live after rewind:\n";
            for e in &self.external {
                s += &format!("           t{} {} {}/{}  {}\n", e.turn, e.event, e.state, e.disclosure, e.label);
            }
        }
        s += &format!("  result   {} files change in the worktree\n", self.final_changes.len());
        for c in self.final_changes.iter().take(20) {
            s += &format!("           {c}\n");
        }
        s
    }
}

/// Apply a plan: park patches, check out the result, append a `world.rewind` event.
pub fn apply(eng: &Engine, plan: &Plan, accept_external: bool) -> Result<Event> {
    if !plan.external.is_empty() && !accept_external {
        bail!("{} external actions stay live after this rewind; pass --accept-external", plan.external.len());
    }
    let live_now = eng.world.snapshot()?;
    if live_now != plan.live_tree {
        bail!("the worktree changed since the plan was made; plan again");
    }
    let dir = eng.home.join("parked").join(&plan.target[..12]);
    let mut parked_files = Vec::new();
    for (i, p) in plan.parked.iter().enumerate() {
        std::fs::create_dir_all(&dir)?;
        let f = dir.join(format!("{:02}-{}.patch", i, p.event));
        std::fs::write(&f, &p.patch)?;
        parked_files.push(json!({ "event": p.event, "paths": p.paths, "file": f.to_string_lossy() }));
    }
    let pre = eng.world.checkout(&plan.result_tree)?;
    let target = eng.store.get(&plan.target)?;
    eng.record_tree(
        &plan.branch,
        "world.rewind",
        &plan.result_tree,
        json!({
            "target": plan.target,
            "pre_tree": pre,
            "mode": plan.mode,
            "parked": parked_files,
            "rewound_turn": target.turn().unwrap_or(0),
            "external_live": plan.external,
        }),
    )
}

/// Undo the last rewind on a branch: restore its pre-rewind tree exactly.
pub fn undo(eng: &Engine, branch: &str) -> Result<Event> {
    let last = eng
        .lineage(branch)?
        .into_iter()
        .rev()
        .find(|e| e.kind == "world.rewind")
        .ok_or_else(|| anyhow::anyhow!("no rewind to undo on {branch}"))?;
    let pre = last.str("pre_tree").unwrap_or_default().to_string();
    let before = eng.world.checkout(&pre)?;
    eng.record_tree(branch, "world.rewind", &pre, json!({ "undo": last.id, "pre_tree": before, "mode": "undo" }))
}
