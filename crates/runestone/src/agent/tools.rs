//! Tools shaped like the ones current models were trained with: Read, Write, Edit, Bash, Grep, Glob.
//! Plus Rewind, which the session handles itself.
//!
//! Every Read and Write pins the content hash the agent saw. Write and Edit refuse when the file changed
//! since then (optimistic locking): agents otherwise overwrite human edits from a stale copy.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read as _;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const MAX_OUTPUT: usize = 30_000;

pub fn definitions() -> Value {
    json!([
        {
            "name": "Read",
            "description": "Read a file. Returns lines prefixed with line numbers (cat -n style). Reads up to 2000 lines by default; use offset (1-based line) and limit for longer files. You must Read a file before you Edit or overwrite it.",
            "input_schema": { "type": "object", "properties": {
                "file_path": { "type": "string", "description": "Absolute path, or relative to the working directory" },
                "offset": { "type": "integer", "description": "Line number to start at (1-based)" },
                "limit": { "type": "integer", "description": "Number of lines to read" }
            }, "required": ["file_path"] }
        },
        {
            "name": "Write",
            "description": "Create a file or replace one entirely. Parent directories are created. To change an existing file, prefer Edit; overwriting an existing file requires a prior Read.",
            "input_schema": { "type": "object", "properties": {
                "file_path": { "type": "string" },
                "content": { "type": "string" }
            }, "required": ["file_path", "content"] }
        },
        {
            "name": "Edit",
            "description": "Replace an exact string in a file. old_string must match the file exactly (including indentation) and be unique unless replace_all is true. Requires a prior Read of the file; fails if the file changed since you read it.",
            "input_schema": { "type": "object", "properties": {
                "file_path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            }, "required": ["file_path", "old_string", "new_string"] }
        },
        {
            "name": "Bash",
            "description": "Run a shell command with bash in the working directory (each call starts there; use `cd dir && …`). Returns stdout and stderr combined, and the exit code when non-zero. Default timeout 120000 ms, max 600000. Commands ending in & keep running in the background. Prefer Read, Edit, Grep and Glob over cat, sed, grep and find.",
            "input_schema": { "type": "object", "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "description": "Milliseconds" },
                "description": { "type": "string", "description": "What the command does, in 5-10 words" }
            }, "required": ["command"] }
        },
        {
            "name": "Grep",
            "description": "Search file contents with a regular expression (ripgrep syntax). output_mode: \"files_with_matches\" (default), \"content\" (matching lines with line numbers), or \"count\".",
            "input_schema": { "type": "object", "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "File or directory (default: working directory)" },
                "glob": { "type": "string", "description": "Filter files, e.g. \"*.rs\"" },
                "output_mode": { "type": "string", "enum": ["files_with_matches", "content", "count"] },
                "-i": { "type": "boolean", "description": "Case insensitive" },
                "-C": { "type": "integer", "description": "Context lines (content mode)" },
                "head_limit": { "type": "integer", "description": "Limit output to the first N lines" }
            }, "required": ["pattern"] }
        },
        {
            "name": "Glob",
            "description": "Find files by glob pattern, e.g. \"**/*.rs\" or \"src/**/mod.rs\". Returns paths sorted by modification time, newest first.",
            "input_schema": { "type": "object", "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Directory to search (default: working directory)" }
            }, "required": ["pattern"] }
        },
        {
            "name": "Rewind",
            "description": "Rewind yourself: forget everything after a chosen point and restore the files to match that point (changes made by other people are kept). Use it when your current approach has clearly failed (the same error keeps coming back, tests stay red, you are going in circles) instead of pushing on with a cluttered context. You keep only the lesson. point: \"request_start\" (default) restarts the user's current request; \"before_call\" resumes just before one of your earlier tool calls, given by call_id. The lesson says what failed and what to do instead (max 300 characters); evidence is the concrete observation behind it (an error line, a failing test, a log entry). At most 3 rewinds per request. It cannot undo actions that reached the outside world (pushes, deploys, sent messages).",
            "input_schema": { "type": "object", "properties": {
                "point": { "type": "string", "enum": ["request_start", "before_call"] },
                "call_id": { "type": "string", "description": "A tool_use id from your earlier calls (with before_call)" },
                "lesson": { "type": "string" },
                "evidence": { "type": "string" }
            }, "required": ["lesson", "evidence"] }
        }
    ])
}

pub struct Ctx {
    pub worktree: PathBuf,
    pub tmp: PathBuf,
    /// path -> hash of the content the agent last saw
    pub seen: HashMap<String, String>,
    /// Run the next Bash call without network (its own user + network namespace).
    pub offline: bool,
    /// The path the agent sees for `worktree` (forks show the main worktree's path). Paths the agent
    /// sends are mapped from it, and tool output is mapped back to it, so a fork's conversation stays
    /// valid after the fork directory is gone.
    pub alias: Option<PathBuf>,
}

/// Can we run commands in an unprivileged user + network namespace on this machine?
pub fn offline_available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("unshare")
            .args(["--user", "--map-root-user", "--net", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

pub struct Outcome {
    pub text: String,
    pub is_error: bool,
    pub exit: Option<i64>,
    /// Hashes to pin after this call (path -> content hash)
    pub seen: HashMap<String, String>,
}

impl Outcome {
    fn ok(text: impl Into<String>) -> Self {
        Outcome { text: text.into(), is_error: false, exit: None, seen: HashMap::new() }
    }
    fn err(text: impl Into<String>) -> Self {
        Outcome { text: text.into(), is_error: true, exit: None, seen: HashMap::new() }
    }
}

pub fn hash_bytes(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

impl Ctx {
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        let joined = if p.is_absolute() { p.to_path_buf() } else { self.worktree.join(p) };
        let joined = normalize(&joined);
        match &self.alias {
            Some(a) => match joined.strip_prefix(a) {
                Ok(rest) => self.worktree.join(rest),
                Err(_) => joined,
            },
            None => joined,
        }
    }

    /// Replace the real root with the alias in text shown to the agent.
    fn present(&self, text: String) -> String {
        match &self.alias {
            Some(a) => text.replace(&*self.worktree.to_string_lossy(), &a.to_string_lossy()),
            None => text,
        }
    }

    pub fn inside(&self, p: &Path) -> bool {
        p.starts_with(&self.worktree)
    }

    pub fn run(&self, name: &str, input: &Value) -> Outcome {
        let r = match name {
            "Read" => self.read(input),
            "Write" => self.write(input),
            "Edit" => self.edit(input),
            "Bash" => self.bash(input),
            "Grep" => self.grep(input),
            "Glob" => self.glob(input),
            _ => Ok(Outcome::err(format!("Unknown tool: {name}"))),
        };
        let mut o = r.unwrap_or_else(|e| Outcome::err(format!("Error: {e:#}")));
        o.text = self.present(std::mem::take(&mut o.text));
        o
    }

    fn path_arg(&self, input: &Value) -> Result<PathBuf> {
        match input["file_path"].as_str().or_else(|| input["path"].as_str()) {
            Some(p) if !p.is_empty() => Ok(self.resolve(p)),
            _ => bail!("file_path is required"),
        }
    }

    /// Optimistic lock: may the agent write `path` right now?
    fn check_fresh(&self, path: &Path) -> Option<Outcome> {
        let Ok(bytes) = std::fs::read(path) else { return None }; // new file: fine
        let key = path.to_string_lossy().to_string();
        match self.seen.get(&key) {
            None => Some(Outcome::err(format!(
                "{} exists and you have not read it in this conversation. Read it first, then edit.",
                path.display()
            ))),
            Some(h) if *h != hash_bytes(&bytes) => Some(Outcome::err(format!(
                "{} changed since you last read it. Read it again before editing.",
                path.display()
            ))),
            _ => None,
        }
    }

    fn read(&self, input: &Value) -> Result<Outcome> {
        let path = self.path_arg(input)?;
        if path.is_dir() {
            return Ok(Outcome::err(format!("{} is a directory. Use Glob or Bash `ls`.", path.display())));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Ok(Outcome::err(format!("Cannot read {}: {e}", path.display()))),
        };
        let mut out = Outcome::ok(String::new());
        out.seen.insert(path.to_string_lossy().into(), hash_bytes(&bytes));
        if bytes[..bytes.len().min(8192)].contains(&0) {
            out.text = format!("(binary file, {} bytes; not shown)", bytes.len());
            return Ok(out);
        }
        let text = String::from_utf8_lossy(&bytes);
        if text.is_empty() {
            out.text = "(empty file)".into();
            return Ok(out);
        }
        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
        let lines: Vec<&str> = text.lines().collect();
        if offset > lines.len() {
            out.text = format!("(file has {} lines; offset {offset} is past the end)", lines.len());
            return Ok(out);
        }
        let end = (offset - 1 + limit).min(lines.len());
        let mut s = String::new();
        for (i, l) in lines[offset - 1..end].iter().enumerate() {
            let l = if l.chars().count() > 2000 { format!("{}… [line truncated]", l.chars().take(2000).collect::<String>()) } else { l.to_string() };
            s += &format!("{:>6}\t{}\n", offset + i, l);
        }
        if end < lines.len() {
            s += &format!("… ({} more lines; use offset {} to continue)\n", lines.len() - end, end + 1);
        }
        out.text = s;
        Ok(out)
    }

    fn write(&self, input: &Value) -> Result<Outcome> {
        let path = self.path_arg(input)?;
        let content = input["content"].as_str().unwrap_or("");
        if let Some(e) = self.check_fresh(&path) {
            return Ok(e);
        }
        let existed = path.exists();
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&path, content)?;
        let mut o = Outcome::ok(format!(
            "{} {} ({} lines)",
            if existed { "Overwrote" } else { "Created" },
            path.display(),
            content.lines().count()
        ));
        o.seen.insert(path.to_string_lossy().into(), hash_bytes(content.as_bytes()));
        Ok(o)
    }

    fn edit(&self, input: &Value) -> Result<Outcome> {
        let path = self.path_arg(input)?;
        let old = input["old_string"].as_str().unwrap_or("");
        let new = input["new_string"].as_str().unwrap_or("");
        let all = input["replace_all"].as_bool().unwrap_or(false);
        if !path.exists() {
            return Ok(Outcome::err(format!("{} does not exist. Use Write to create it.", path.display())));
        }
        if let Some(e) = self.check_fresh(&path) {
            return Ok(e);
        }
        if old.is_empty() {
            return Ok(Outcome::err("old_string is empty. Use Write to replace a whole file."));
        }
        if old == new {
            return Ok(Outcome::err("old_string and new_string are identical; nothing to change."));
        }
        let text = std::fs::read_to_string(&path)?;
        let n = text.matches(old).count();
        if n == 0 {
            return Ok(Outcome::err("old_string was not found in the file. Read the file again and match it exactly, including whitespace."));
        }
        if n > 1 && !all {
            return Ok(Outcome::err(format!("old_string matches {n} places. Add surrounding context to make it unique, or set replace_all.")));
        }
        let at = text.find(old).unwrap_or(0);
        let updated = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        std::fs::write(&path, &updated)?;
        let first_line = text[..at].matches('\n').count() + 1;
        let lines: Vec<&str> = updated.lines().collect();
        let span = new.matches('\n').count() + 1;
        let (from, to) = (first_line.saturating_sub(3).max(1), (first_line + span + 2).min(lines.len()));
        let mut snippet = String::new();
        for i in from..=to.max(from) {
            if let Some(l) = lines.get(i - 1) {
                snippet += &format!("{i:>6}\t{l}\n");
            }
        }
        let mut o = Outcome::ok(format!(
            "Edited {} ({} replacement{}). Lines {from}-{to} now:\n{snippet}",
            path.display(),
            if all { n } else { 1 },
            if all && n != 1 { "s" } else { "" }
        ));
        o.seen.insert(path.to_string_lossy().into(), hash_bytes(updated.as_bytes()));
        Ok(o)
    }

    fn bash(&self, input: &Value) -> Result<Outcome> {
        let raw = input["command"].as_str().unwrap_or("");
        if raw.trim().is_empty() {
            return Ok(Outcome::err("command is empty"));
        }
        // In a fork, the agent addresses the main worktree's path; point it at the fork instead.
        // String-level only: a path built indirectly (env var, symlink) can still reach the real tree.
        let mapped;
        let cmd = match &self.alias {
            Some(a) => {
                mapped = raw.replace(&*a.to_string_lossy(), &self.worktree.to_string_lossy());
                mapped.as_str()
            }
            None => raw,
        };
        let timeout = Duration::from_millis(input["timeout"].as_u64().unwrap_or(120_000).clamp(1_000, 600_000));
        std::fs::create_dir_all(&self.tmp)?;
        // Output goes to a file, not a pipe: a background child (`npm run dev &`) holding the pipe open would
        // otherwise block us until it exits.
        let out_path = self.tmp.join(format!("bash-{}-{:x}.out", std::process::id(), rand_u64()));
        let out_file = std::fs::File::create(&out_path)?;
        let err_file = out_file.try_clone()?;
        let mut command = if self.offline {
            let mut c = Command::new("unshare");
            c.args(["--user", "--map-root-user", "--net", "bash", "-c", cmd]);
            c
        } else {
            let mut c = Command::new("bash");
            c.arg("-c").arg(cmd);
            c
        };
        let mut child = command
            .current_dir(&self.worktree)
            .stdin(Stdio::null())
            .stdout(out_file)
            .stderr(err_file)
            .env("RUNE", "1")
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("TERM", "dumb")
            .process_group(0)
            .spawn()?;
        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break Some(s);
            }
            if start.elapsed() > timeout {
                timed_out = true;
                let pg = format!("-{}", child.id());
                let _ = Command::new("kill").args(["-TERM", "--", &pg]).status();
                std::thread::sleep(Duration::from_millis(500));
                let _ = Command::new("kill").args(["-KILL", "--", &pg]).status();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut buf = Vec::new();
        std::fs::File::open(&out_path)?.read_to_end(&mut buf)?;
        let _ = std::fs::remove_file(&out_path);
        let mut text = clip_middle(&String::from_utf8_lossy(&buf), MAX_OUTPUT);
        let exit = status.and_then(|s| s.code()).map(|c| c as i64);
        if timed_out {
            text += &format!("\n[timed out after {}s; process group killed]", timeout.as_secs());
        } else if let Some(c) = exit {
            if c != 0 {
                text += &format!("\n[exit code {c}]");
            }
        }
        if text.trim().is_empty() {
            text = "(no output)".into();
        }
        if self.offline {
            text += "\n[ran without network access]";
        }
        Ok(Outcome { text, is_error: timed_out || exit.is_some_and(|c| c != 0), exit, seen: HashMap::new() })
    }

    fn grep(&self, input: &Value) -> Result<Outcome> {
        let pattern = input["pattern"].as_str().unwrap_or("");
        let path = input["path"].as_str().map(|p| self.resolve(p)).unwrap_or_else(|| self.worktree.clone());
        let mode = input["output_mode"].as_str().unwrap_or("files_with_matches");
        let ci = input["-i"].as_bool().unwrap_or(false);
        let ctx = input["-C"].as_u64();
        let head = input["head_limit"].as_u64().map(|n| n as usize);
        let has_rg = Command::new("rg").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok();
        let mut c;
        if has_rg {
            c = Command::new("rg");
            c.args(["--no-heading", "--color", "never", "--hidden", "-g", "!.git"]);
            match mode {
                "content" => {
                    c.arg("-n");
                    if let Some(n) = ctx {
                        c.args(["-C", &n.to_string()]);
                    }
                }
                "count" => {
                    c.arg("-c");
                }
                _ => {
                    c.arg("-l");
                }
            }
            if ci {
                c.arg("-i");
            }
            if let Some(g) = input["glob"].as_str() {
                c.args(["-g", g]);
            }
            c.arg("-e").arg(pattern).arg(&path);
        } else {
            c = Command::new("grep");
            c.args(["-rE", "--exclude-dir=.git"]);
            match mode {
                "content" => {
                    c.arg("-n");
                    if let Some(n) = ctx {
                        c.args(["-C", &n.to_string()]);
                    }
                }
                "count" => {
                    c.arg("-c");
                }
                _ => {
                    c.arg("-l");
                }
            }
            if ci {
                c.arg("-i");
            }
            if let Some(g) = input["glob"].as_str() {
                c.arg(format!("--include={g}"));
            }
            c.arg("-e").arg(pattern).arg(&path);
        }
        let out = c.current_dir(&self.worktree).stdin(Stdio::null()).output()?;
        if out.status.code() == Some(1) {
            return Ok(Outcome::ok("No matches found."));
        }
        if !out.status.success() {
            return Ok(Outcome::err(String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines: Vec<&str> = text.lines().filter(|l| !(mode == "count" && l.ends_with(":0"))).collect();
        let total = lines.len();
        if let Some(h) = head {
            lines.truncate(h);
        }
        let mut s = clip_middle(&lines.join("\n"), 20_000);
        if lines.len() < total {
            s += &format!("\n… ({} more)", total - lines.len());
        }
        Ok(Outcome::ok(if s.trim().is_empty() { "No matches found.".into() } else { s }))
    }

    fn glob(&self, input: &Value) -> Result<Outcome> {
        let pattern = input["pattern"].as_str().unwrap_or("");
        let base = input["path"].as_str().map(|p| self.resolve(p)).unwrap_or_else(|| self.worktree.clone());
        let set = globset::GlobBuilder::new(pattern).literal_separator(true).build()?.compile_matcher();
        let mut hits: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        let walker = walkdir::WalkDir::new(&base).into_iter().filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !(e.depth() > 0 && e.file_type().is_dir() && matches!(n.as_ref(), ".git" | "node_modules" | "target" | ".venv" | "__pycache__"))
        });
        for e in walker.flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            let rel = e.path().strip_prefix(&base).unwrap_or(e.path());
            if set.is_match(rel) {
                let t = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
                hits.push((t, e.path().to_path_buf()));
            }
        }
        if hits.is_empty() {
            return Ok(Outcome::ok("No files found."));
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        let total = hits.len();
        let mut s: Vec<String> = hits.iter().take(200).map(|h| h.1.display().to_string()).collect();
        if total > 200 {
            s.push(format!("… ({} more; narrow the pattern)", total - 200));
        }
        Ok(Outcome::ok(s.join("\n")))
    }
}

/// Keep the head and tail of long output; the middle is usually the least useful part.
pub fn clip_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let mut a = half;
    while !s.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = s.len() - half;
    while !s.is_char_boundary(b) {
        b += 1;
    }
    format!("{}\n… [{} bytes omitted] …\n{}", &s[..a], b - a, &s[b..])
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn rand_u64() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (t.as_nanos() as u64).rotate_left(17) ^ std::process::id() as u64
}
