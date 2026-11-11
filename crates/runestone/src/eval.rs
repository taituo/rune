//! `rune eval`: tasks, a miner that makes tasks from a repo's history, a runner that tries each task under
//! several modes in isolated copies, and a report.
//!
//! A task is a starting tree (a repo at a commit, or a directory), a prompt, an optional visible check the
//! agent may use, and hidden tests the agent never sees: they are laid over the final tree only to score it.

use crate::agent::model::Provider;
use crate::agent::session::Checks;
use crate::agent::{Config, Permission, Quiet, Session};
use crate::checks;
use crate::engine::Engine;
use crate::search;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub name: String,
    /// Repo or directory; relative paths are relative to the task file.
    pub repo: PathBuf,
    /// Commit to start from (repo must be git). None: copy `repo` as it is (without .git).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub prompt: String,
    /// Check command the agent's session runs at stable points (modes checks, rollback, search).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<String>,
    pub hidden: HiddenTests,
    /// Seconds for each check / hidden run.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_steps")]
    pub max_steps: usize,
    /// Approaches for search mode; a generic set when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hypotheses: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HiddenTests {
    pub cmd: String,
    /// Inline files: path → content.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    /// A directory (relative to the task file) laid over the tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// Paths taken from this commit of `repo` (what the miner writes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

fn default_timeout() -> u64 {
    600
}
fn default_steps() -> usize {
    40
}

impl Task {
    pub fn load(path: &Path) -> Result<Task> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut t: Task = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        if t.repo.is_relative() {
            t.repo = base.join(&t.repo);
        }
        if let Some(d) = &t.hidden.dir {
            if d.is_relative() {
                t.hidden.dir = Some(base.join(d));
            }
        }
        if t.name.is_empty() || t.prompt.is_empty() || t.hidden.cmd.is_empty() {
            bail!("{}: name, prompt and hidden.cmd are required", path.display());
        }
        if t.hidden.commit.is_some() != !t.hidden.paths.is_empty() {
            bail!("{}: hidden.commit and hidden.paths go together", path.display());
        }
        Ok(t)
    }

    /// Write the starting tree into `dir` (which must exist and be empty).
    pub fn materialize(&self, dir: &Path) -> Result<()> {
        match &self.base {
            Some(base) => {
                let mut archive = Command::new("git")
                    .arg("-C")
                    .arg(&self.repo)
                    .args(["archive", "--format=tar", base])
                    .stdout(std::process::Stdio::piped())
                    .spawn()?;
                let st = Command::new("tar").arg("-x").arg("-C").arg(dir).stdin(archive.stdout.take().unwrap()).status()?;
                if !archive.wait()?.success() || !st.success() {
                    bail!("git archive {base} of {} failed", self.repo.display());
                }
            }
            None => copy_dir(&self.repo, dir)?,
        }
        Ok(())
    }

    /// Lay the hidden tests over `dir`.
    pub fn apply_hidden(&self, dir: &Path) -> Result<()> {
        for (p, content) in &self.hidden.files {
            write_under(dir, p, content.as_bytes())?;
        }
        if let Some(d) = &self.hidden.dir {
            copy_dir(d, dir)?;
        }
        if let Some(c) = &self.hidden.commit {
            for p in &self.hidden.paths {
                let out = Command::new("git").arg("-C").arg(&self.repo).args(["show", &format!("{c}:{p}")]).output()?;
                if !out.status.success() {
                    bail!("git show {c}:{p}: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                write_under(dir, p, &out.stdout)?;
            }
        }
        Ok(())
    }
}

fn write_under(dir: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let rel = Path::new(rel);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("hidden path {} leaves the tree", rel.display());
    }
    let to = dir.join(rel);
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(to, bytes)?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for e in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))?.flatten() {
        if e.file_name() == ".git" {
            continue;
        }
        let to = dst.join(e.file_name());
        let ft = e.file_type()?;
        if ft.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir(&e.path(), &to)?;
        } else if ft.is_symlink() {
            let _ = std::fs::remove_file(&to);
            std::os::unix::fs::symlink(std::fs::read_link(e.path())?, &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- miner

fn is_test_path(p: &str) -> bool {
    let name = p.rsplit('/').next().unwrap_or(p);
    p.starts_with("tests/")
        || p.contains("/tests/")
        || p.starts_with("test/")
        || p.contains("/test/")
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
}

fn is_code_path(p: &str) -> bool {
    const EXT: &[&str] = &["rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp", "rb", "cs", "swift", "zig"];
    p.rsplit_once('.').is_some_and(|(_, e)| EXT.contains(&e))
}

/// The command that runs exactly these test files, by ecosystem.
fn hidden_cmd(repo_root: &Path, tests: &[String]) -> Option<String> {
    let rs: Vec<&String> = tests.iter().filter(|p| p.ends_with(".rs")).collect();
    if !rs.is_empty() {
        let mut parts = Vec::new();
        for p in rs {
            // tests/<name>.rs or <crate dir>/tests/<name>.rs → cargo test --test <name> [-p <package>]
            let (dir, file) = p.rsplit_once("tests/").map(|(d, f)| (d.trim_end_matches('/'), f)).unwrap_or(("", p));
            let Some(name) = file.strip_suffix(".rs").filter(|n| !n.contains('/')) else { continue };
            let pkg = if dir.is_empty() { None } else { package_name(&repo_root.join(dir).join("Cargo.toml")) };
            parts.push(match pkg {
                Some(pkg) => format!("cargo test -q --offline -p {pkg} --test {name} 2>&1"),
                None => format!("cargo test -q --offline --test {name} 2>&1"),
            });
        }
        return (!parts.is_empty()).then(|| parts.join(" && "));
    }
    let py: Vec<&str> = tests.iter().filter(|p| p.ends_with(".py")).map(|s| s.as_str()).collect();
    if !py.is_empty() {
        return Some(format!("python3 -m pytest -q {} 2>&1", py.join(" ")));
    }
    if tests.iter().any(|p| p.ends_with(".go")) {
        return Some("go test ./... 2>&1".into());
    }
    if tests.iter().any(|p| p.ends_with(".js") || p.ends_with(".ts")) {
        return Some("npm test --silent 2>&1".into());
    }
    None
}

fn package_name(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let mut in_pkg = false;
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with('[') {
            in_pkg = l == "[package]";
        } else if in_pkg {
            if let Some(v) = l.strip_prefix("name").map(str::trim).and_then(|r| r.strip_prefix('=')) {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn git_out(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub struct MineOpts {
    /// Commits to look at, newest first.
    pub limit: usize,
    /// Skip commits whose code change (non-test lines added + removed) is bigger than this.
    pub max_code_lines: u64,
}

/// Commits that change both code and tests become tasks: start at the parent, the commit's message is the
/// prompt, the commit's versions of its test files are the hidden tests.
pub fn mine(repo: &Path, opts: &MineOpts) -> Result<Vec<Task>> {
    let repo = std::fs::canonicalize(repo)?;
    let root = PathBuf::from(git_out(&repo, &["rev-parse", "--show-toplevel"])?.trim());
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let mut tasks = Vec::new();
    for sha in git_out(&root, &["rev-list", "--no-merges", "-n", &opts.limit.to_string(), "HEAD"])?.lines() {
        let parents = git_out(&root, &["rev-list", "--parents", "-n", "1", sha])?;
        if parents.split_whitespace().count() != 2 {
            continue; // root commit
        }
        let mut tests = Vec::new();
        let mut code_lines = 0u64;
        let mut has_code = false;
        for l in git_out(&root, &["show", "--format=", "--numstat", "--diff-filter=AM", sha])?.lines() {
            let mut f = l.split('\t');
            let (Some(a), Some(d), Some(p)) = (f.next(), f.next(), f.next()) else { continue };
            if is_test_path(p) && is_code_path(p) {
                tests.push(p.to_string());
            } else if is_code_path(p) {
                has_code = true;
                code_lines += a.parse::<u64>().unwrap_or(0) + d.parse::<u64>().unwrap_or(0);
            }
        }
        if tests.is_empty() || !has_code || code_lines > opts.max_code_lines {
            continue;
        }
        let Some(cmd) = hidden_cmd(&root, &tests) else { continue };
        let msg = git_out(&root, &["log", "-1", "--format=%B", sha])?;
        let parent = git_out(&root, &["rev-parse", &format!("{sha}^")])?.trim().to_string();
        tasks.push(Task {
            name: format!("{name}-{}", &sha[..8]),
            repo: root.clone(),
            base: Some(parent),
            prompt: format!("{}\n\nMake this change in the repository, with tests.", msg.trim()),
            visible: None,
            hidden: HiddenTests { cmd, commit: Some(sha.to_string()), paths: tests, ..Default::default() },
            timeout: default_timeout(),
            max_steps: default_steps(),
            hypotheses: Vec::new(),
        });
    }
    Ok(tasks)
}

// ---------------------------------------------------------------- runner

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One session, no checks.
    Plain,
    /// Checks at stable points, reported to the model; no rollback.
    Checks,
    /// Checks, and a rollback to the best state after two worse results.
    Rollback,
    /// Parallel attempts selected by the visible check (never the hidden tests).
    Search,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode> {
        Ok(match s {
            "plain" => Mode::Plain,
            "checks" => Mode::Checks,
            "rollback" => Mode::Rollback,
            "search" => Mode::Search,
            _ => bail!("unknown mode {s} (plain, checks, rollback, search)"),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Plain => "plain",
            Mode::Checks => "checks",
            Mode::Rollback => "rollback",
            Mode::Search => "search",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellResult {
    pub task: String,
    pub mode: Mode,
    pub run: usize,
    pub pass: bool,
    pub hidden_exit: Option<i64>,
    pub hidden_tail: String,
    pub steps: usize,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u64,
    pub rewinds: usize,
    pub stop: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub error: Option<String>,
}

/// Makes the provider for (task, mode, run, attempt). Attempt is 0 outside search.
pub type Providers<'a> = dyn Fn(&Task, Mode, usize, usize) -> Result<Box<dyn Provider>> + Sync + 'a;

pub struct RunOpts {
    pub modes: Vec<Mode>,
    pub runs: usize,
    pub jobs: usize,
    pub out: PathBuf,
    pub perm: Permission,
    pub config: Config,
    /// Attempts in search mode when the task names no hypotheses.
    pub search_width: usize,
}

const GENERIC_HYPOTHESES: &[&str] = &[
    "Make the smallest change that does what is asked.",
    "Read the surrounding code and tests first, then make the change the way the codebase already does similar things.",
    "Write or extend a test for the change first, then make it pass.",
    "Question the obvious approach: check whether the change belongs in a different place than it first seems.",
];

fn cell_path(out: &Path, task: &str, mode: Mode, run: usize) -> PathBuf {
    out.join("results").join(format!("{task}__{}__{run}.json", mode.name()))
}

/// Run every (task, mode, run) cell not already done in `out`, `jobs` at a time. Returns all results in
/// `out`, including earlier ones (resumable: delete a result file to rerun that cell).
pub fn run(tasks: &[Task], opts: &RunOpts, providers: &Providers) -> Result<Vec<CellResult>> {
    std::fs::create_dir_all(opts.out.join("results"))?;
    let mut todo = Vec::new();
    for t in tasks {
        for &m in &opts.modes {
            for r in 1..=opts.runs {
                if !cell_path(&opts.out, &t.name, m, r).exists() {
                    todo.push((t, m, r));
                }
            }
        }
    }
    let queue = std::sync::Mutex::new(todo.into_iter());
    std::thread::scope(|s| {
        for _ in 0..opts.jobs.max(1) {
            s.spawn(|| loop {
                let next = queue.lock().unwrap().next();
                let Some((t, m, r)) = next else { break };
                let res = run_cell(t, m, r, opts, providers);
                let path = cell_path(&opts.out, &t.name, m, r);
                if let Ok(bytes) = serde_json::to_vec_pretty(&res) {
                    let tmp = path.with_extension("tmp");
                    if std::fs::write(&tmp, bytes).is_ok() {
                        let _ = std::fs::rename(tmp, path);
                    }
                }
            });
        }
    });
    load_results(&opts.out)
}

pub fn load_results(out: &Path) -> Result<Vec<CellResult>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(out.join("results"))?.flatten() {
        if e.path().extension().is_some_and(|x| x == "json") {
            v.push(serde_json::from_slice::<CellResult>(&std::fs::read(e.path())?)?);
        }
    }
    v.sort_by(|a, b| (&a.task, a.mode.name(), a.run).cmp(&(&b.task, b.mode.name(), b.run)));
    Ok(v)
}

fn run_cell(task: &Task, mode: Mode, run: usize, opts: &RunOpts, providers: &Providers) -> CellResult {
    let wt = opts.out.join("work").join(&task.name).join(format!("{}-{run}", mode.name()));
    let mut res = CellResult {
        task: task.name.clone(),
        mode,
        run,
        pass: false,
        hidden_exit: None,
        hidden_tail: String::new(),
        steps: 0,
        input_tokens: 0,
        cache_read_tokens: 0,
        output_tokens: 0,
        wall_ms: 0,
        rewinds: 0,
        stop: String::new(),
        branch: String::new(),
        worktree: wt.clone(),
        error: None,
    };
    let t0 = Instant::now();
    if let Err(e) = attempt(task, mode, run, opts, providers, &wt, &mut res) {
        res.error = Some(format!("{e:#}"));
    }
    res.wall_ms = t0.elapsed().as_millis() as u64;
    res
}

fn attempt(task: &Task, mode: Mode, run: usize, opts: &RunOpts, providers: &Providers, wt: &Path, res: &mut CellResult) -> Result<()> {
    let _ = std::fs::remove_dir_all(wt);
    std::fs::create_dir_all(wt)?;
    task.materialize(wt)?;
    let home = opts.out.join("home");
    let eng = Engine::open_in(&home, wt)?;
    // A rerun of the cell gets a new branch; the old one stays in the record.
    let branch = eng.branch(&format!("eval/{}/{run}/{:x}", mode.name(), (crate::store::now() * 1000.0) as u64));
    res.branch = branch.clone();
    Session::spawn(&eng, &branch, &opts.config, "2026-01-01")?;
    let timeout = Duration::from_secs(task.timeout);
    let visible = task.visible.clone().or_else(|| checks::detect(wt));
    let mut branches = vec![branch.clone()];
    match mode {
        Mode::Search => {
            let hypotheses: Vec<String> = if task.hypotheses.is_empty() {
                GENERIC_HYPOTHESES.iter().take(opts.search_width.max(1)).map(|s| s.to_string()).collect()
            } else {
                task.hypotheses.clone()
            };
            let spec = search::Spec {
                prompt: task.prompt.clone(),
                hypotheses,
                visible: visible.clone(),
                hidden: None, // selection must not see the tests it is scored on
                max_steps: task.max_steps,
                perm: opts.perm,
                timeout,
                keep_dirs: false,
            };
            let factory = |i: usize| providers(task, mode, run, i).expect("provider");
            let r = search::run(&eng, &branch, &spec, &factory)?;
            res.stop = match r.winner {
                Some(w) => format!("winner {w} of {}", r.outcomes.len()),
                None => format!("no winner of {}", r.outcomes.len()),
            };
            branches.extend(r.outcomes.iter().map(|o| o.branch.clone()));
        }
        _ => {
            let checks = match (mode, &visible) {
                (Mode::Checks, Some(cmd)) => Some(Checks { cmd: cmd.clone(), every: 5, rollback_after: 0, timeout }),
                (Mode::Rollback, Some(cmd)) => Some(Checks { cmd: cmd.clone(), every: 5, rollback_after: 2, timeout }),
                _ => None,
            };
            let mut s = Session {
                eng: &eng,
                branch: branch.clone(),
                provider: providers(task, mode, run, 0)?,
                ui: Box::new(Quiet),
                perm: opts.perm,
                max_steps: task.max_steps,
                checks,
            };
            let r = s.turn(&task.prompt)?;
            res.stop = r.stop;
        }
    }
    // Totals over every branch the cell used (search attempts included), each event once.
    let mut seen = HashSet::new();
    for b in &branches {
        for e in eng.lineage(b).unwrap_or_default() {
            if !seen.insert(e.id.clone()) {
                continue;
            }
            match e.kind.as_str() {
                "model.call" => {
                    let u = &e.body["response"]["usage"];
                    res.steps += 1;
                    res.input_tokens += u["input_tokens"].as_u64().unwrap_or(0);
                    res.cache_read_tokens += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    res.output_tokens += u["output_tokens"].as_u64().unwrap_or(0);
                }
                "world.rewind" => res.rewinds += 1,
                _ => {}
            }
        }
    }
    // Score: the final tree with the hidden tests laid over it, in a separate directory.
    let tree = eng.world.snapshot()?;
    let scored = wt.with_extension("scored");
    let _ = std::fs::remove_dir_all(&scored);
    eng.world.checkout_into(&scored, &tree)?;
    task.apply_hidden(&scored)?;
    let (exit, out, _) = checks::exec(&scored, &task.hidden.cmd, timeout)?;
    res.hidden_exit = Some(exit);
    res.pass = exit == 0;
    res.hidden_tail = out.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let _ = std::fs::remove_dir_all(&scored);
    Ok(())
}

// ---------------------------------------------------------------- report

#[derive(Debug, Serialize)]
pub struct ModeSummary {
    pub mode: Mode,
    pub cells: usize,
    pub passed: usize,
    pub errors: usize,
    pub pass_rate: f64,
    pub mean_steps: f64,
    pub mean_input_tokens: f64,
    pub mean_output_tokens: f64,
    pub mean_wall_s: f64,
    pub rewinds: usize,
}

pub fn summarize(results: &[CellResult]) -> Vec<ModeSummary> {
    let mut modes: Vec<Mode> = Vec::new();
    for r in results {
        if !modes.contains(&r.mode) {
            modes.push(r.mode);
        }
    }
    modes.sort_by_key(|m| *m as u8);
    modes
        .into_iter()
        .map(|m| {
            let rs: Vec<&CellResult> = results.iter().filter(|r| r.mode == m).collect();
            let n = rs.len().max(1) as f64;
            let mean = |f: &dyn Fn(&CellResult) -> f64| rs.iter().map(|r| f(r)).sum::<f64>() / n;
            let passed = rs.iter().filter(|r| r.pass).count();
            ModeSummary {
                mode: m,
                cells: rs.len(),
                passed,
                errors: rs.iter().filter(|r| r.error.is_some()).count(),
                pass_rate: passed as f64 / n,
                mean_steps: mean(&|r| r.steps as f64),
                mean_input_tokens: mean(&|r| (r.input_tokens + r.cache_read_tokens) as f64),
                mean_output_tokens: mean(&|r| r.output_tokens as f64),
                mean_wall_s: mean(&|r| r.wall_ms as f64 / 1000.0),
                rewinds: rs.iter().map(|r| r.rewinds).sum(),
            }
        })
        .collect()
}

/// Markdown: one row per mode, then one row per task with each mode's passes out of its runs.
pub fn markdown(results: &[CellResult]) -> String {
    let sum = summarize(results);
    let mut s = String::from("| mode | cells | pass | pass rate | errors | steps | input tok | output tok | wall s | rewinds |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for m in &sum {
        s += &format!(
            "| {} | {} | {} | {:.0}% | {} | {:.1} | {:.0} | {:.0} | {:.1} | {} |\n",
            m.mode.name(),
            m.cells,
            m.passed,
            m.pass_rate * 100.0,
            m.errors,
            m.mean_steps,
            m.mean_input_tokens,
            m.mean_output_tokens,
            m.mean_wall_s,
            m.rewinds
        );
    }
    let modes: Vec<Mode> = sum.iter().map(|m| m.mode).collect();
    s += &format!("\n| task | {} |\n|---|{}\n", modes.iter().map(|m| m.name()).collect::<Vec<_>>().join(" | "), "---|".repeat(modes.len()));
    let mut tasks: Vec<&str> = results.iter().map(|r| r.task.as_str()).collect();
    tasks.dedup();
    for t in tasks {
        let cells: Vec<String> = modes
            .iter()
            .map(|&m| {
                let rs: Vec<&CellResult> = results.iter().filter(|r| r.task == t && r.mode == m).collect();
                let p = rs.iter().filter(|r| r.pass).count();
                let err = if rs.iter().any(|r| r.error.is_some()) { " (err)" } else { "" };
                format!("{p}/{}{err}", rs.len())
            })
            .collect();
        s += &format!("| {t} | {} |\n", cells.join(" | "));
    }
    s
}

pub fn write_report(out: &Path, results: &[CellResult]) -> Result<PathBuf> {
    std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&json!({ "modes": summarize(results), "cells": results }))?)?;
    let md = out.join("report.md");
    std::fs::write(&md, markdown(results))?;
    Ok(md)
}
