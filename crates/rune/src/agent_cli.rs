//! `rune chat` and `rune run`: rune's own agent loop in the terminal. Plain streaming output; the API is
//! the library, so a richer UI is just another client.

use anyhow::{Context, Result};
use runestone::agent::fold::{effective_chain, fold};
use runestone::agent::model::Anthropic;
use runestone::agent::{self, Config, Permission, Session, Ui};
use runestone::classify::{Class, State};
use runestone::rewind::{self, Mode};
use runestone::{ops, Engine};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::PathBuf;

const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const OFF: &str = "\x1b[0m";

#[derive(Default)]
pub struct StdUi {
    mid_line: bool,
    in_thinking: bool,
    pub tokens_in: u64,
    pub tokens_cached: u64,
    pub tokens_out: u64,
}

impl StdUi {
    fn newline(&mut self) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
        if self.in_thinking {
            print!("{OFF}");
            self.in_thinking = false;
        }
    }
}

impl Ui for StdUi {
    fn text(&mut self, t: &str) {
        if self.in_thinking {
            println!("{OFF}");
            self.in_thinking = false;
            self.mid_line = false;
        }
        print!("{t}");
        self.mid_line = !t.ends_with('\n');
        let _ = std::io::stdout().flush();
    }
    fn thinking(&mut self, t: &str) {
        if !self.in_thinking {
            self.newline();
            print!("{DIM}");
            self.in_thinking = true;
        }
        print!("{t}");
        self.mid_line = !t.ends_with('\n');
        let _ = std::io::stdout().flush();
    }
    fn tool(&mut self, name: &str, summary: &str, class: &Class) {
        self.newline();
        let tag = match class.state {
            State::Read | State::Local => String::new(),
            s => format!(" {YELLOW}[{s:?}]{OFF}"),
        };
        println!("{CYAN}●{OFF} {name}({summary}){tag}");
    }
    fn tool_done(&mut self, text: &str, is_error: bool) {
        let lines: Vec<&str> = text.lines().collect();
        let color = if is_error { RED } else { DIM };
        for (i, l) in lines.iter().take(4).enumerate() {
            let l: String = l.chars().take(160).collect();
            println!("  {color}{}{l}{OFF}", if i == 0 { "⎿ " } else { "  " });
        }
        if lines.len() > 4 {
            println!("  {DIM}  … +{} lines{OFF}", lines.len() - 4);
        }
    }
    fn note(&mut self, t: &str) {
        self.newline();
        println!("{YELLOW}{t}{OFF}");
    }
    fn retry(&mut self, t: &str) {
        self.newline();
        println!("{DIM}{t}{OFF}");
    }
    fn usage(&mut self, u: &Value) {
        self.tokens_in += u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        self.tokens_cached += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        self.tokens_out += u["output_tokens"].as_u64().unwrap_or(0);
    }
    fn check(&mut self, r: &runestone::checks::CheckResult) {
        self.newline();
        let sc = &r.score;
        let counts = if sc.passed + sc.failed > 0 { format!("{} passed, {} failed", sc.passed, sc.failed) } else { format!("exit {}", r.exit) };
        let how = if r.cached { "cached".to_string() } else { format!("{:.1}s", r.duration_ms as f64 / 1000.0) };
        if sc.green {
            println!("{DIM}✓ check {counts} ({how}){OFF}");
        } else {
            println!("{RED}✗ check {counts} ({how}){OFF}");
        }
    }
    fn ask(&mut self, q: &str) -> bool {
        self.newline();
        print!("{YELLOW}{q} [y/N] {OFF}");
        let _ = std::io::stdout().flush();
        let mut s = String::new();
        let _ = std::io::stdin().lock().read_line(&mut s);
        matches!(s.trim(), "y" | "Y" | "yes")
    }
}

pub struct Opts {
    pub api: String,
    pub base_url: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub mode: Permission,
    pub show_thinking: bool,
    pub resume: bool,
    pub max_steps: usize,
    /// Check command (None: detect; Some("") disables)
    pub check: Option<String>,
    pub check_every: usize,
    pub rollback_after: usize,
}

/// The provider for a session's API. A resumed session keeps the API it was started with.
pub fn provider(api: &str, base_url: Option<&str>) -> Result<Box<dyn runestone::agent::model::Provider>> {
    match api {
        "openai" => Ok(Box::new(runestone::agent::openai::OpenAi::from_env(base_url)?)),
        "anthropic" => {
            if let Some(b) = base_url {
                std::env::set_var("ANTHROPIC_BASE_URL", b);
            }
            Ok(Box::new(Anthropic::from_env()?))
        }
        other => anyhow::bail!("unknown --api {other} (anthropic | openai)"),
    }
}

/// Pick or create the branch: an explicit name, the latest one (--resume), or a fresh session.
fn open_session<'a>(eng: &'a Engine, branch: Option<&str>, o: &Opts) -> Result<Session<'a>> {
    let name = match branch {
        Some(b) => eng.branch(b),
        None if o.resume => eng.latest_branch()?.context("no session to resume in this worktree")?,
        None => eng.branch(&format!("s/{}", runestone::store::now() as u64)),
    };
    let has_session = match eng.store.head(&name)? {
        Some(h) => agent::fold::spawn_config(&effective_chain(&eng.store, &h)?).is_some(),
        None => false,
    };
    if !has_session {
        let cfg = Config {
            api: o.api.clone(),
            model: o.model.clone(),
            effort: o.effort.clone(),
            thinking_display: o.show_thinking.then(|| "summarized".to_string()),
            ..Config::default()
        };
        Session::spawn(eng, &name, &cfg, &today())?;
    }
    let prov = provider(&agent::session::api_of(&eng.store, &name)?, o.base_url.as_deref())?;
    Ok(Session {
        eng,
        branch: name,
        provider: prov,
        ui: Box::new(StdUi::default()),
        perm: o.mode,
        max_steps: o.max_steps,
        checks: checks_for(eng, o),
    })
}

fn checks_for(eng: &Engine, o: &Opts) -> Option<agent::session::Checks> {
    let cmd = match &o.check {
        Some(c) if c.is_empty() => return None,
        Some(c) => c.clone(),
        None => runestone::checks::detect(&eng.world.worktree)?,
    };
    Some(agent::session::Checks {
        cmd,
        every: o.check_every,
        rollback_after: o.rollback_after,
        timeout: std::time::Duration::from_secs(600),
    })
}

/// Ctrl-C during a turn interrupts it (the running command is killed, the turn ends cleanly);
/// a second Ctrl-C, or one at the prompt, exits.
fn install_interrupt(eng: &Engine) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    use std::sync::atomic::Ordering;
    let in_turn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (flag, cancel) = (in_turn.clone(), eng.cancel.clone());
    let _ = ctrlc::set_handler(move || {
        if flag.load(Ordering::SeqCst) && !cancel.swap(true, Ordering::SeqCst) {
            eprintln!("\n{YELLOW}interrupting… (Ctrl-C again to quit){OFF}");
        } else {
            std::process::exit(130);
        }
    });
    in_turn
}

pub fn run_once(eng: &Engine, branch: Option<&str>, o: &Opts, prompt: &str) -> Result<()> {
    let mut s = open_session(eng, branch, o)?;
    let in_turn = install_interrupt(eng);
    in_turn.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = s.turn(prompt)?;
    in_turn.store(false, std::sync::atomic::Ordering::SeqCst);
    println!();
    eprintln!("{DIM}[{} · {} steps · {} · {}]{OFF}", short_branch(&s.branch), r.steps, r.stop, usage_line(&r.usage));
    Ok(())
}

pub fn usage_line(u: &agent::session::Usage) -> String {
    format!(
        "in {} · cache read {} ({:.0}%) · cache write {} · out {}",
        u.input,
        u.cache_read,
        u.cache_hit() * 100.0,
        u.cache_write,
        u.output
    )
}

pub fn chat(eng: &Engine, branch: Option<&str>, o: &Opts) -> Result<()> {
    let mut s = open_session(eng, branch, o)?;
    let in_turn = install_interrupt(eng);
    eprintln!(
        "{DIM}rune · {} · {} · {} · checks: {} · /help for commands{OFF}",
        short_branch(&s.branch),
        agent::session::api_of(&eng.store, &s.branch).unwrap_or_default(),
        eng.world.worktree.display(),
        s.checks.as_ref().map(|c| c.cmd.as_str()).unwrap_or("off")
    );
    let stdin = std::io::stdin();
    loop {
        print!("\n› ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        loop {
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                return Ok(());
            }
            let l = line.trim_end_matches('\n');
            if let Some(stripped) = l.strip_suffix('\\') {
                input.push_str(stripped);
                input.push('\n');
                print!("  ");
                std::io::stdout().flush()?;
            } else {
                input.push_str(l);
                break;
            }
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if let Some(cmd) = input.strip_prefix('/') {
            match slash(&mut s, cmd) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => eprintln!("{RED}{e:#}{OFF}"),
            }
            continue;
        }
        in_turn.store(true, std::sync::atomic::Ordering::SeqCst);
        let res = s.turn(input);
        in_turn.store(false, std::sync::atomic::Ordering::SeqCst);
        match res {
            Ok(r) if r.stop == "interrupted" => eprintln!("\n{YELLOW}[interrupted after {} steps; the record is consistent, continue with a new message]{OFF}", r.steps),
            Ok(r) => eprintln!("\n{DIM}[{} steps · {}]{OFF}", r.steps, usage_line(&r.usage)),
            Err(e) => eprintln!("\n{RED}{e:#}{OFF}\n{DIM}(the record is intact; /continue retries){OFF}"),
        }
        println!();
    }
}

/// Returns true to exit.
fn slash(s: &mut Session, cmd: &str) -> Result<bool> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let eng = s.eng;
    match parts.first().copied().unwrap_or("") {
        "exit" | "quit" | "q" => return Ok(true),
        "help" => println!(
            "/log [n]            recent events\n/rewind N [hard]    restore conversation and files to the end of turn N\n/rewind green       restore to the last passing check\n/check              run the check now\n/undo               undo the last rewind\n/fork N DIR         copy the world at turn N into DIR as a new branch\n/continue           resume the loop from the current head\n/verify             re-render every request from the record and compare\n/exit"
        ),
        "log" => {
            let n: usize = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(15);
            for ev in eng.lineage(&s.branch)?.iter().rev().take(n) {
                println!("  {}", ops::describe(ev));
            }
        }
        "check" => {
            if s.checks.is_none() {
                println!("no check command (start with --check CMD)");
            } else {
                s.run_check()?;
            }
        }
        "rewind" => {
            let mode = if parts.get(2) == Some(&"hard") { Mode::Hard } else { Mode::KeepHuman };
            let chain = s.chain()?;
            let target = if parts.get(1) == Some(&"green") {
                last_green(&chain).context("no passing check in this conversation")?
            } else {
                let n: u64 = parts.get(1).and_then(|x| x.parse().ok()).context("usage: /rewind N|green [hard]")?;
                chain
                    .iter()
                    .rev()
                    .find(|e| e.turn() == Some(n) && e.kind == "turn.end")
                    .or_else(|| chain.iter().rev().find(|e| e.turn() == Some(n)))
                    .cloned()
                    .with_context(|| format!("no turn {n} in this conversation"))?
            };
            let plan = rewind::plan(eng, &s.branch, &target, mode)?;
            print!("{}", plan.render());
            if !s.ui.ask("Apply this rewind?") {
                return Ok(false);
            }
            let ext = !plan.external.is_empty() && s.ui.ask("Some actions stay live in the outside world. Continue anyway?");
            let ev = rewind::apply(eng, &plan, ext)?;
            s.ui.note(ev.str("note").unwrap_or("rewound"));
        }
        "undo" => {
            let ev = rewind::undo(eng, &s.branch)?;
            s.ui.note(ev.str("note").unwrap_or("undone"));
        }
        "fork" => {
            let n: u64 = parts.get(1).and_then(|x| x.parse().ok()).context("usage: /fork N DIR")?;
            let dir = PathBuf::from(parts.get(2).context("usage: /fork N DIR")?);
            let chain = s.chain()?;
            let at = chain.iter().rev().find(|e| e.turn() == Some(n) && e.kind == "turn.end").cloned().context("no such turn")?;
            let (b, _) = ops::fork(eng, &at, &format!("s/fork-{}", runestone::store::now() as u64), &dir)?;
            println!("forked into {} as {}. Continue there with: rune -C {} chat -b {}", dir.display(), b, dir.display(), short_branch(&b));
        }
        "continue" => {
            s.run_loop()?;
            println!();
        }
        "verify" => {
            let r = agent::verify_requests(eng, &s.branch)?;
            let bad = r.iter().filter(|x| !x.1).count();
            println!("{} requests re-rendered, {} mismatched", r.len(), bad);
        }
        other => println!("unknown command /{other}; /help"),
    }
    Ok(false)
}

/// The latest passing check on a chain.
pub fn last_green(chain: &[runestone::Event]) -> Option<runestone::Event> {
    chain.iter().rev().find(|e| e.kind == "check" && e.body["score"]["green"].as_bool() == Some(true)).cloned()
}

/// Print the conversation exactly as the model would see it next.
pub fn transcript(eng: &Engine, branch: &str, json: bool) -> Result<()> {
    let head = eng.store.head(branch)?.context("no such branch")?;
    let chain = effective_chain(&eng.store, &head)?;
    let f = fold(&eng.store, &chain)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&f.messages)?);
        return Ok(());
    }
    for m in &f.messages {
        let role = m["role"].as_str().unwrap_or("");
        for b in m["content"].as_array().into_iter().flatten() {
            let line = match b["type"].as_str().unwrap_or("") {
                "text" => b["text"].as_str().unwrap_or("").to_string(),
                "thinking" => format!("{DIM}(thinking){OFF}"),
                "tool_use" => format!("{CYAN}{}{OFF} {}", b["name"].as_str().unwrap_or(""), b["input"]),
                "tool_result" => {
                    let c = b["content"].as_str().unwrap_or("");
                    format!("{DIM}⎿ {}{OFF}", c.lines().next().unwrap_or(""))
                }
                t => format!("({t})"),
            };
            println!("{role:>9}: {line}");
        }
    }
    Ok(())
}

pub fn short_branch(b: &str) -> &str {
    b.split_once('/').map(|x| x.1).unwrap_or(b)
}

/// Today's date (UTC) without a date crate.
pub fn today() -> String {
    let days = (runestone::store::now() / 86_400.0) as i64;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}
