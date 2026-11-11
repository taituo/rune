mod agent_cli;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use runestone::rewind::{self, Mode};
use runestone::{classify, hook, ops, Engine, Event};
use serde_json::json;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "rune", version, about = "A coding agent whose conversation and world are versioned together")]
struct Cli {
    /// Worktree (default: current directory)
    #[arg(short = 'C', long, global = true)]
    dir: Option<PathBuf>,
    /// Branch name within this worktree (default: the most recently moved one)
    #[arg(short, long, global = true)]
    branch: Option<String>,
    /// Output JSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    KeepHuman,
    Hard,
}

#[derive(Clone, Copy, ValueEnum)]
enum PermArg {
    /// Ask before compensable, irreversible or unknown actions
    Ask,
    /// Block them
    Dry,
    /// Run everything
    Auto,
}

#[derive(clap::Args, Clone)]
struct AgentArgs {
    #[arg(long, default_value = "claude-opus-5-5")]
    model: String,
    /// low | medium | high | xhigh | max (default: the model's)
    #[arg(long)]
    effort: Option<String>,
    #[arg(long, value_enum)]
    mode: Option<PermArg>,
    /// Stream summarized thinking
    #[arg(long)]
    show_thinking: bool,
    /// Continue the most recent session in this worktree
    #[arg(long, short = 'r')]
    resume: bool,
    #[arg(long, default_value_t = 400)]
    max_steps: usize,
    /// Check command run at stable points (default: detected from the project; "" disables)
    #[arg(long)]
    check: Option<String>,
    /// Also check mid-turn after this many file-changing tool calls (0: only at turn end)
    #[arg(long, default_value_t = 0)]
    check_every: usize,
    /// Restore the best checked state after this many consecutive worse checks (0: never)
    #[arg(long, default_value_t = 0)]
    rollback_after: usize,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive session with rune's own agent loop
    Chat {
        #[command(flatten)]
        a: AgentArgs,
    },
    /// Run one request and exit (default mode: dry)
    Run {
        #[command(flatten)]
        a: AgentArgs,
        prompt: Vec<String>,
    },
    /// Print the conversation exactly as the model will see it next
    Transcript,
    /// Run N attempts in parallel from the current state, one per hypothesis, each in its own overlay
    /// fork; pick the best by hidden tests, then visible ones, then smallest diff; merge it keep-human
    Search {
        #[command(flatten)]
        a: AgentArgs,
        /// One approach per attempt (repeat)
        #[arg(long = "hyp", required = true)]
        hypotheses: Vec<String>,
        /// Directory of held-out test files, stacked over each attempt only at evaluation time
        #[arg(long)]
        hidden: Option<PathBuf>,
        /// Command that runs the held-out tests
        #[arg(long)]
        hidden_cmd: Option<String>,
        /// Scripted attempts instead of a model: DIR/attempt-<i>.json (for experiments)
        #[arg(long)]
        script: Option<PathBuf>,
        /// Keep each attempt's overlay directories for inspection
        #[arg(long)]
        keep_dirs: bool,
        prompt: Vec<String>,
    },
    /// Ingest a harness hook event from stdin (fails open)
    Hook {
        #[arg(long, default_value = "claude")]
        harness: String,
    },
    /// Record an event by hand (turn.start, tool.pre, tool.post, turn.end, manual)
    Record {
        kind: String,
        /// Short label for tool events
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        tool: Option<String>,
    },
    /// Show a branch's events
    Log {
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// List branches of this worktree
    Heads,
    /// Per-path history: which event, which author, which call
    Blame { path: String },
    /// The outside-changes note the agent would get at the start of its next turn
    Changes,
    /// Plan (and with --apply, run) a rewind
    Rewind {
        /// Turn to rewind to (state at the end of that turn)
        #[arg(long, conflicts_with_all = ["to", "undo"])]
        turn: Option<u64>,
        /// Event id (prefix) to rewind to
        #[arg(long, conflicts_with = "undo")]
        to: Option<String>,
        /// Undo the last rewind on the branch
        #[arg(long)]
        undo: bool,
        /// Rewind to the last passing check
        #[arg(long, conflicts_with_all = ["turn", "to", "undo"])]
        green: bool,
        #[arg(long, value_enum, default_value = "keep-human")]
        mode: ModeArg,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        accept_external: bool,
    },
    /// Materialize the world at an event into a new directory, as a new branch
    Fork {
        /// Event id prefix (default: head)
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        turn: Option<u64>,
        #[arg(long, default_value = "fork")]
        name: String,
        dir: PathBuf,
    },
    /// Find the first recorded world where a command fails
    Bisect {
        #[arg(long)]
        cmd: String,
    },
    /// Classify a tool call or shell command by irreversibility
    Classify {
        #[arg(long, default_value = "Bash")]
        tool: String,
        /// Tool input as JSON (for non-shell tools)
        #[arg(long)]
        input: Option<String>,
        /// Shell command
        command: Vec<String>,
    },
    /// Measure snapshot latency on this worktree (decides per-call vs per-turn snapshots)
    Bench {
        #[arg(short, default_value_t = 10)]
        n: usize,
    },
    /// Run the project's check (build + tests) on the current world and record the result
    Check {
        /// Command (default: detected from the project)
        #[arg(long)]
        cmd: Option<String>,
    },
    /// Verify every event id on a branch against its content
    Verify,
    /// Print hook configuration for a harness
    Install {
        #[arg(value_parser = ["claude", "kiro"])]
        harness: String,
    },
}

fn main() {
    let cli = Cli::parse();
    // Overlay forks need an unprivileged user + mount namespace: re-run ourselves inside one.
    if matches!(cli.cmd, Cmd::Search { .. }) && !runestone::layer::in_namespace() {
        let exe = std::env::current_exe().expect("own path");
        let status = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount"])
            .arg(exe)
            .args(std::env::args_os().skip(1))
            .env("RUNE_NS", "1")
            .status();
        match status {
            Ok(s) => std::process::exit(s.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("rune: search needs `unshare` (util-linux) and unprivileged user namespaces: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Cmd::Hook { harness } = &cli.cmd {
        let mut raw = String::new();
        let _ = std::io::stdin().read_to_string(&mut raw);
        let out = hook::handle(&raw, harness);
        if !out.stdout.is_empty() {
            println!("{}", out.stdout);
        }
        if !out.stderr.is_empty() {
            eprintln!("{}", out.stderr);
        }
        std::process::exit(out.exit);
    }
    if let Err(e) = run(cli) {
        eprintln!("rune: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let dir = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    if let Cmd::Classify { tool, input, command } = &cli.cmd {
        let c = if command.is_empty() {
            let v = input.as_deref().map(serde_json::from_str).transpose()?.unwrap_or(json!({}));
            classify::tool(tool, &v)
        } else {
            classify::shell(&command.join(" "))
        };
        if cli.json {
            println!("{}", serde_json::to_string(&c)?);
        } else {
            println!("{:?}/{:?}  {}", c.state, c.disclosure, c.reason);
        }
        return Ok(());
    }
    if let Cmd::Install { harness } = &cli.cmd {
        install(harness);
        return Ok(());
    }

    let eng = Engine::open(&dir)?;
    let opts = |a: &AgentArgs, default: PermArg| agent_cli::Opts {
        model: a.model.clone(),
        effort: a.effort.clone(),
        mode: match a.mode.unwrap_or(default) {
            PermArg::Ask => runestone::agent::Permission::Ask,
            PermArg::Dry => runestone::agent::Permission::Dry,
            PermArg::Auto => runestone::agent::Permission::Auto,
        },
        show_thinking: a.show_thinking,
        resume: a.resume,
        max_steps: a.max_steps,
        check: a.check.clone(),
        check_every: a.check_every,
        rollback_after: a.rollback_after,
    };
    match &cli.cmd {
        Cmd::Chat { a } => return agent_cli::chat(&eng, cli.branch.as_deref(), &opts(a, PermArg::Ask)),
        Cmd::Run { a, prompt } => {
            let p = if prompt.is_empty() {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                prompt.join(" ")
            };
            return agent_cli::run_once(&eng, cli.branch.as_deref(), &opts(a, PermArg::Dry), p.trim());
        }
        Cmd::Search { a, hypotheses, hidden, hidden_cmd, script, keep_dirs, prompt } => {
            let o = opts(a, PermArg::Dry);
            let branch = match &cli.branch {
                Some(b) => eng.branch(b),
                None => eng.latest_branch()?.unwrap_or_else(|| eng.branch(&format!("s/{}", runestone::store::now() as u64))),
            };
            // Searching from a plain worktree is fine: start a session for it.
            let has_session = match eng.store.head(&branch)? {
                Some(h) => runestone::agent::fold::spawn_config(&runestone::agent::fold::effective_chain(&eng.store, &h)?).is_some(),
                None => false,
            };
            if !has_session {
                let cfg = runestone::agent::Config {
                    model: a.model.clone(),
                    effort: a.effort.clone(),
                    thinking_display: a.show_thinking.then(|| "summarized".to_string()),
                    ..Default::default()
                };
                runestone::agent::Session::spawn(&eng, &branch, &cfg, &agent_cli::today())?;
            }
            let visible = match &a.check {
                Some(c) if c.is_empty() => None,
                Some(c) => Some(c.clone()),
                None => runestone::checks::detect(&eng.world.worktree),
            };
            let hidden = match (hidden, hidden_cmd) {
                (Some(d), Some(c)) => Some(runestone::search::Hidden { dir: std::fs::canonicalize(d)?, cmd: c.clone() }),
                (None, None) => None,
                _ => bail!("--hidden and --hidden-cmd go together"),
            };
            let spec = runestone::search::Spec {
                prompt: prompt.join(" "),
                hypotheses: hypotheses.clone(),
                visible,
                hidden,
                max_steps: o.max_steps,
                perm: o.mode,
                timeout: std::time::Duration::from_secs(600),
                keep_dirs: *keep_dirs,
            };
            let script = script.clone();
            let factory = move |i: usize| -> Box<dyn runestone::agent::model::Provider> {
                match &script {
                    Some(dir) => Box::new(
                        runestone::agent::model::Scripted::from_file(&dir.join(format!("attempt-{i}.json"))).expect("script file"),
                    ),
                    None => Box::new(runestone::agent::model::Anthropic::from_env().expect("ANTHROPIC_API_KEY")),
                }
            };
            let r = runestone::search::run(&eng, &branch, &spec, &factory)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            println!("search {} from {} ({} attempts, base ready in {} ms, total {} ms)", r.id, &r.start[..12], r.outcomes.len(), r.base_ms, r.total_ms);
            let sc = |s: &Option<runestone::checks::Score>| match s {
                Some(s) => format!("{}{}/{}", if s.green { "✓" } else { "✗" }, s.passed, s.passed + s.failed),
                None => "-".into(),
            };
            for o in &r.outcomes {
                let mark = if Some(o.index) == r.winner { "★" } else { " " };
                println!(
                    "{mark} {} visible {:<7} hidden {:<7} diff {:>4}  {:>5} ms  {}{}",
                    o.index,
                    sc(&o.visible),
                    sc(&o.hidden),
                    o.diff_lines,
                    o.ms,
                    o.hypothesis,
                    o.error.as_ref().map(|e| format!("  ERROR {e}")).unwrap_or_default()
                );
            }
            match r.winner {
                Some(w) if r.merged => println!("merged attempt {w} into the worktree; the session continues from it. Others: rune heads"),
                Some(w) => println!("attempt {w} won but was not merged: {}", r.merge_conflicts.join(", ")),
                None => println!("no attempt passed; nothing merged. All attempts are on their branches: rune heads"),
            }
            return Ok(());
        }
        _ => {}
    }
    let branch = match &cli.branch {
        Some(b) => eng.branch(b),
        None => eng.latest_branch()?.unwrap_or_else(|| eng.branch("main")),
    };

    match cli.cmd {
        Cmd::Record { kind, summary, tool } => {
            let mut extra = json!({});
            if let Some(s) = summary {
                extra["summary"] = json!(s);
            }
            if let Some(t) = tool {
                extra["tool"] = json!(t);
            }
            let ev = eng.record(&branch, &kind, extra)?;
            println!("{}", ops::describe(&ev));
            if kind == "turn.start" {
                if let Some(n) = eng.outside_note(&ev) {
                    println!("{n}");
                }
            }
        }
        Cmd::Log { n } => {
            let lin = eng.lineage(&branch)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&lin.iter().rev().take(n).collect::<Vec<_>>())?);
            } else {
                println!("{branch}");
                for ev in lin.iter().rev().take(n) {
                    println!("  {}", ops::describe(ev));
                }
            }
        }
        Cmd::Heads => {
            for (name, id, ts) in eng.store.heads(&format!("{}/", eng.world.key))? {
                let short = name.split_once('/').map(|x| x.1).unwrap_or(&name);
                println!("{short:<40} {}  {:.0}", &id[..12], ts);
            }
        }
        Cmd::Blame { path } => {
            println!("{path}");
            for (ev, c) in eng.blame(&branch, &path)? {
                let who = match ev.str("author") {
                    Some("agent") => format!("agent  {:<6} {}", ev.str("tool").unwrap_or(""), ev.str("summary").map(|s| format!("\"{s}\"")).unwrap_or_default()),
                    Some(a) => a.to_string(),
                    None => "?".into(),
                };
                println!("  t{:<3} {}  {:<10} {}", ev.turn().unwrap_or(0), ev.short(), c.stat(), who);
            }
        }
        Cmd::Changes => {
            let lin = eng.lineage(&branch)?;
            let Some(head) = lin.last() else { bail!("no events on {branch}") };
            let live = eng.world.snapshot()?;
            let ch = eng.world.diff(head.tree().unwrap_or_default(), &live)?;
            if ch.is_empty() {
                println!("No outside changes since the last recorded event.");
            } else {
                for c in ch {
                    println!("  {} {} ({})", c.status, c.path, c.stat());
                }
            }
        }
        Cmd::Rewind { turn, to, undo, green, mode, apply, accept_external } => {
            if undo {
                let ev = rewind::undo(&eng, &branch)?;
                println!("undone: {}", ops::describe(&ev));
                return Ok(());
            }
            let target = if green {
                let head = eng.store.head(&branch)?.context("no such branch")?;
                let chain = runestone::agent::fold::effective_chain(&eng.store, &head)?;
                agent_cli::last_green(&chain).context("no passing check on this branch")?
            } else {
                resolve(&eng, &branch, to.as_deref(), turn)?
            };
            let mode = match mode {
                ModeArg::KeepHuman => Mode::KeepHuman,
                ModeArg::Hard => Mode::Hard,
            };
            let plan = rewind::plan(&eng, &branch, &target, mode)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print!("{}", plan.render());
            }
            if apply {
                let ev = rewind::apply(&eng, &plan, accept_external)?;
                println!("applied: {}", ops::describe(&ev));
                for p in ev.body.get("parked").and_then(|v| v.as_array()).into_iter().flatten() {
                    println!("  parked patch: {}", p["file"].as_str().unwrap_or(""));
                }
            } else {
                println!("(dry run; pass --apply)");
            }
        }
        Cmd::Fork { at, turn, name, dir } => {
            let ev = resolve(&eng, &branch, at.as_deref(), turn)?;
            let (b, f) = ops::fork(&eng, &ev, &name, &dir)?;
            println!("forked {} into {} as {} ({})", ev.short(), dir.display(), b, f.short());
        }
        Cmd::Bisect { cmd } => {
            let r = ops::bisect(&eng, &branch, &cmd)?;
            for p in &r.probes {
                println!("  {} exit {}{}", p.event.short(), p.exit, if p.cached { " (cached)" } else { "" });
            }
            println!("last good:  {}", ops::describe(&r.last_good));
            println!("first bad:  {}", ops::describe(&r.first_bad));
        }
        Cmd::Bench { n } => bench(&eng, n)?,
        Cmd::Verify => {
            let lin = eng.lineage(&branch)?;
            let bad: Vec<&Event> = lin.iter().filter(|e| !runestone::store::verify(e)).collect();
            println!("{} events, {} mismatched ids", lin.len(), bad.len());
            for e in bad {
                println!("  {}", e.id);
            }
            if lin.iter().any(|e| e.kind == "model.call") {
                let r = runestone::agent::verify_requests(&eng, &branch)?;
                let bad: Vec<&String> = r.iter().filter(|x| !x.1).map(|x| &x.0).collect();
                println!("{} model requests re-rendered from the record, {} differ from what was sent", r.len(), bad.len());
                for b in bad {
                    println!("  {b}");
                }
            }
        }
        Cmd::Transcript => agent_cli::transcript(&eng, &branch, cli.json)?,
        Cmd::Check { cmd } => {
            let cmd = cmd.or_else(|| runestone::checks::detect(&eng.world.worktree)).context("no check command detected; pass --cmd")?;
            let tree = eng.world.snapshot()?;
            let r = runestone::checks::run(&eng, &tree, &cmd, std::time::Duration::from_secs(600))?;
            let ev = eng.record_tree(&branch, "check", &tree, json!({ "cmd": cmd, "exit": r.exit, "score": r.score, "cached": r.cached, "duration_ms": r.duration_ms, "output": r.tail }))?;
            println!("{} {} (passed {}, failed {}, {}){}", if r.score.green { "✓" } else { "✗" }, cmd, r.score.passed, r.score.failed, if r.cached { "cached".into() } else { format!("{} ms", r.duration_ms) }, format!("  [{}]", ev.short()));
        }
        Cmd::Hook { .. } | Cmd::Classify { .. } | Cmd::Install { .. } | Cmd::Chat { .. } | Cmd::Run { .. } | Cmd::Search { .. } => unreachable!(),
    }
    Ok(())
}

fn resolve(eng: &Engine, branch: &str, id: Option<&str>, turn: Option<u64>) -> Result<Event> {
    if let Some(id) = id {
        return eng.store.resolve(id);
    }
    let lin = eng.lineage(branch)?;
    if let Some(t) = turn {
        // End of turn t: its turn.end, or the last event of that turn.
        return lin
            .iter()
            .rev()
            .find(|e| e.turn() == Some(t) && e.kind == "turn.end")
            .or_else(|| lin.iter().rev().find(|e| e.turn() == Some(t)))
            .cloned()
            .with_context(|| format!("no turn {t} on {branch}"));
    }
    lin.last().cloned().with_context(|| format!("no events on {branch}"))
}

fn bench(eng: &Engine, n: usize) -> Result<()> {
    let files = eng.world.git(&["ls-files"])?.lines().count();
    let mut times = Vec::new();
    for i in 0..n.max(1) {
        if i == 0 {
            // cold: drop the shadow index
            let _ = std::fs::remove_file(eng.world.git_dir.join("index"));
        }
        let t = Instant::now();
        eng.world.snapshot()?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let cold = times[0];
    let mut warm = times[1..].to_vec();
    warm.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| warm.get(((warm.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(cold);
    println!("worktree: {} ({} tracked files)", eng.world.worktree.display(), files.max(eng.world.git(&["ls-files"])?.lines().count()));
    println!("cold snapshot: {cold:.1} ms");
    println!("warm snapshot: p50 {:.1} ms  p95 {:.1} ms  (n={})", pct(0.5), pct(0.95), warm.len());
    let verdict = if pct(0.95) < 150.0 { "per-call snapshots are fine" } else { "snapshot at turn boundaries, or move to overlay write-capture" };
    println!("verdict: {verdict}");
    Ok(())
}

fn install(harness: &str) {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "rune".into());
    match harness {
        "claude" => {
            let hooks = json!([{ "type": "command", "command": format!("{exe} hook --harness claude"), "timeout": 10 }]);
            let plain = json!([{ "hooks": hooks }]);
            let tools = json!([{ "matcher": "*", "hooks": hooks }]);
            let cfg = json!({ "hooks": {
                "SessionStart": plain, "UserPromptSubmit": plain,
                "PreToolUse": tools, "PostToolUse": tools, "Stop": plain,
            }});
            println!("# Merge into .claude/settings.json (or ~/.claude/settings.json):");
            println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
        }
        _ => {
            let cmd = format!("{exe} hook --harness kiro");
            let plain = json!([{ "command": cmd }]);
            let tools = json!([{ "matcher": "*", "command": cmd }]);
            let cfg = json!({ "hooks": {
                "agentSpawn": plain, "userPromptSubmit": plain,
                "preToolUse": tools, "postToolUse": tools, "stop": plain,
            }});
            println!("# Merge into your Kiro agent config (.kiro/agents/<agent>.json):");
            println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
        }
    }
    println!("# Set RUNE_DRY_WORLD=1 to block compensable/irreversible/unknown actions at pre-tool.");
}
