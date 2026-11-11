//! Irreversibility on two axes.
//!
//! `state`: can a rewind restore what the action changed? `disclosure`: has information already left?
//! A deleted comment already notified its watchers, so the axes are independent. Gate on the worse one.
//! Static rules only here; "observed in a dry world" and "model" tiers come later. Unknown fails safe.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Read,
    Local,
    LocalOutside,
    Compensable,
    Unknown,
    Irreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disclosure {
    None,
    Internal,
    External,
}

#[derive(Debug, Clone, Serialize)]
pub struct Class {
    pub state: State,
    pub disclosure: Disclosure,
    pub reason: String,
}

impl Class {
    fn new(state: State, disclosure: Disclosure, reason: impl Into<String>) -> Self {
        Class { state, disclosure, reason: reason.into() }
    }
    /// Would a rewind past this action leave something live in the outside world?
    pub fn crosses_world(&self) -> bool {
        self.state >= State::Compensable || self.disclosure == Disclosure::External
    }
    /// Blocked in dry-world mode.
    pub fn gated(&self) -> bool {
        self.state >= State::Compensable
    }
    fn join(self, o: Class) -> Class {
        let reason = if o.state > self.state || (o.state == self.state && o.disclosure > self.disclosure) {
            o.reason.clone()
        } else {
            self.reason.clone()
        };
        Class { state: self.state.max(o.state), disclosure: self.disclosure.max(o.disclosure), reason }
    }
}

/// Classify a tool call by tool name and input (Claude Code, Kiro and MCP-style names).
pub fn tool(name: &str, input: &Value) -> Class {
    let n = name.to_ascii_lowercase();
    let cmd = input.get("command").or_else(|| input.get("cmd")).and_then(Value::as_str);
    match n.as_str() {
        "bash" | "shell" | "execute_bash" | "execute_cmd" | "run_shell_command" | "exec" => {
            return shell(cmd.unwrap_or(""));
        }
        "read" | "grep" | "glob" | "ls" | "fs_read" | "notebookread" | "todowrite" | "todoread" | "think" => {
            return Class::new(State::Read, Disclosure::None, format!("{name} reads"));
        }
        "webfetch" | "websearch" | "web_fetch" | "web_search" => {
            return Class::new(State::Read, Disclosure::External, format!("{name}: query leaves the machine"));
        }
        "edit" | "multiedit" | "write" | "notebookedit" | "fs_write" | "str_replace" | "create" => {
            return Class::new(State::Local, Disclosure::None, format!("{name} edits files"));
        }
        _ => {}
    }
    if let Some(c) = cmd {
        return shell(c);
    }
    // MCP / integration tools: classify by the operation name (last segment).
    let op = n.rsplit(|c| c == '_' || c == '.' || c == '/').next().unwrap_or(&n);
    let op_full = n.rsplit("__").next().unwrap_or(&n);
    let has = |prefixes: &[&str]| prefixes.iter().any(|p| op_full.starts_with(p) || op.starts_with(p));
    if has(&["get", "list", "search", "read", "describe", "fetch", "query", "find", "view", "show"]) {
        return Class::new(State::Read, Disclosure::Internal, format!("{name}: read operation"));
    }
    if has(&["delete", "remove", "merge", "send", "post", "publish", "deploy", "transition", "close"]) {
        return Class::new(State::Irreversible, Disclosure::External, format!("{name}: cannot be taken back"));
    }
    if has(&["create", "add", "update", "comment", "edit", "set", "assign", "upload", "write"]) {
        return Class::new(State::Compensable, Disclosure::External, format!("{name}: external write with an inverse"));
    }
    Class::new(State::Unknown, Disclosure::External, format!("{name}: unclassified tool"))
}

/// Classify a shell command line. A compound command takes the worst of its parts.
pub fn shell(line: &str) -> Class {
    let parts = split_commands(line);
    if parts.is_empty() {
        return Class::new(State::Read, Disclosure::None, "empty command");
    }
    parts
        .iter()
        .map(|words| simple(words))
        .reduce(Class::join)
        .unwrap()
}

fn simple(words: &[String]) -> Class {
    // Strip env assignments and wrappers.
    let mut w: Vec<&str> = words.iter().map(String::as_str).collect();
    loop {
        match w.first() {
            Some(f) if f.contains('=') && !f.starts_with('-') && !f.starts_with('=') => {
                w.remove(0);
            }
            Some(&("sudo" | "time" | "nohup" | "env" | "exec" | "command" | "nice" | "timeout")) => {
                w.remove(0);
                if let Some(n) = w.first() {
                    if n.parse::<f64>().is_ok() || n.ends_with('s') && n[..n.len() - 1].parse::<f64>().is_ok() {
                        w.remove(0);
                    }
                }
            }
            _ => break,
        }
    }
    let Some(&prog) = w.first() else {
        return Class::new(State::Read, Disclosure::None, "empty");
    };
    let prog = prog.rsplit('/').next().unwrap_or(prog);
    let args = &w[1..];
    let has = |f: &str| args.iter().any(|a| *a == f || a.starts_with(&format!("{f}=")));
    let sub = args.iter().find(|a| !a.starts_with('-')).copied().unwrap_or("");
    let c = |s, d, r: &str| Class::new(s, d, format!("{prog}: {r}"));
    use Disclosure::*;
    use State::*;
    match prog {
        "ls" | "cat" | "head" | "tail" | "less" | "more" | "grep" | "rg" | "ag" | "wc" | "echo" | "printf"
        | "pwd" | "which" | "whoami" | "date" | "env" | "printenv" | "stat" | "file" | "du" | "df" | "tree"
        | "diff" | "cmp" | "sort" | "uniq" | "cut" | "tr" | "jq" | "yq" | "basename" | "dirname"
        | "realpath" | "true" | "false" | "test" | "[" | "type" | "uname" | "ps" | "id" | "sleep" | "fd"
        | "column" | "nl" | "xxd" | "od" | "sha256sum" | "md5sum" | "tee" | "cd" | "pushd" | "popd"
        | "export" | "unset" | "set" | "alias" | "read" | ":" => {
            if prog == "tee" && !args.is_empty() {
                c(Local, None, "writes files")
            } else {
                c(Read, None, "reads")
            }
        }
        "find" => {
            if has("-delete") || has("-exec") || has("-execdir") {
                c(Unknown, None, "find with -delete/-exec")
            } else {
                c(Read, None, "reads")
            }
        }
        "sed" | "perl" => {
            if args.iter().any(|a| a.starts_with("-i")) {
                c(Local, None, "in-place edit")
            } else {
                c(Read, None, "filter")
            }
        }
        "awk" | "gawk" => c(Read, None, "filter"),
        "cp" | "mv" | "rm" | "mkdir" | "rmdir" | "touch" | "chmod" | "ln" | "patch" | "tar" | "unzip"
        | "gzip" | "gunzip" | "truncate" => {
            if args.iter().any(|a| a.starts_with('/') && !a.starts_with("/tmp")) || args.iter().any(|a| a.starts_with('~')) {
                c(LocalOutside, None, "touches paths outside the tree")
            } else {
                c(Local, None, "changes files")
            }
        }
        "git" => git(sub, args),
        "cargo" => match sub {
            "publish" => c(Irreversible, External, "publishes a crate"),
            "install" => c(LocalOutside, External, "installs into ~/.cargo"),
            "fmt" | "clippy" | "build" | "test" | "check" | "run" | "bench" | "doc" | "fix" | "update" | "add" | "new" | "init" => {
                c(Local, None, "builds in the tree")
            }
            _ => c(Unknown, None, "unknown cargo subcommand"),
        },
        "go" => match sub {
            "install" => c(LocalOutside, External, "installs into GOPATH"),
            "get" | "mod" => c(Local, External, "fetches modules"),
            _ => c(Local, None, "builds in the tree"),
        },
        "npm" | "pnpm" | "yarn" | "bun" => {
            if sub == "publish" {
                c(Irreversible, External, "publishes a package")
            } else if has("-g") || has("--global") {
                c(LocalOutside, External, "global install")
            } else if matches!(sub, "install" | "i" | "add" | "ci" | "update") {
                c(Local, External, "installs into the tree")
            } else if matches!(sub, "test" | "run" | "exec" | "x" | "build" | "lint") {
                c(Local, None, "runs a script")
            } else {
                c(Unknown, None, "unknown subcommand")
            }
        }
        "pip" | "pip3" | "uv" | "poetry" => {
            if has("--user") || (!has("-t") && !has("--target") && matches!(sub, "install")) && prog.starts_with("pip") {
                c(LocalOutside, External, "installs outside the tree")
            } else {
                c(Local, External, "installs or syncs")
            }
        }
        "python" | "python3" | "node" | "deno" | "ruby" | "bash" | "sh" | "zsh" | "make" | "just" => {
            if prog == "python" || prog == "python3" {
                if args.first() == Some(&"-m") && args.get(1).is_some_and(|m| *m == "pytest" || *m == "unittest") {
                    return c(Local, None, "runs tests");
                }
            }
            c(Unknown, None, "runs arbitrary code")
        }
        "pytest" | "jest" | "vitest" | "tsc" | "eslint" | "prettier" | "black" | "ruff" | "gofmt" | "rustfmt" | "mypy" => {
            c(Local, None, "tool runs in the tree")
        }
        "curl" | "wget" | "http" | "xh" => {
            let method = args
                .windows(2)
                .find(|p| p[0] == "-X" || p[0] == "--request")
                .map(|p| p[1].to_ascii_uppercase());
            let sends = args.iter().any(|a| {
                a.starts_with("-d") || a.starts_with("--data") || a.starts_with("-F") || *a == "--form"
                    || *a == "-T" || *a == "--upload-file" || *a == "--json" || *a == "--post-data"
            });
            match method.as_deref() {
                Some("GET") | Some("HEAD") | Option::None if !sends => {
                    let s = if prog == "wget" || has("-o") || has("-O") || has("--output") { Local } else { Read };
                    c(s, External, "GET: data in the URL leaves the machine")
                }
                Some("DELETE") => c(Irreversible, External, "HTTP DELETE"),
                _ => c(Irreversible, External, "HTTP write"),
            }
        }
        "ssh" | "scp" | "rsync" | "sftp" => c(Unknown, External, "remote host"),
        "kubectl" | "oc" => match sub {
            "get" | "describe" | "logs" | "top" | "explain" | "version" | "config" | "auth" | "diff" => c(Read, Internal, "reads the cluster"),
            _ if args.iter().any(|a| a.starts_with("--dry-run")) => c(Read, Internal, "dry run"),
            _ => c(Irreversible, External, "changes the cluster"),
        },
        "helm" => match sub {
            "list" | "status" | "get" | "template" | "lint" | "show" | "search" | "history" => c(Read, Internal, "reads"),
            "rollback" | "upgrade" | "install" => c(Compensable, External, "release can be rolled back"),
            _ => c(Irreversible, External, "changes the cluster"),
        },
        "terraform" | "tofu" => match sub {
            "plan" | "validate" | "fmt" | "show" | "output" | "init" | "graph" | "providers" => c(Read, Internal, "no apply"),
            _ => c(Irreversible, External, "applies infrastructure"),
        },
        "docker" | "podman" => match sub {
            "push" => c(Irreversible, External, "pushes an image"),
            "ps" | "images" | "inspect" | "logs" | "version" | "info" => c(Read, None, "reads"),
            _ => c(LocalOutside, None, "local container state"),
        },
        "gh" => {
            let action = args.iter().filter(|a| !a.starts_with('-')).nth(1).copied().unwrap_or("");
            match (sub, action) {
                (_, "view" | "list" | "status" | "diff" | "checks") => c(Read, Internal, "reads GitHub"),
                ("api", _) => {
                    if has("-X") || has("--method") || has("-f") || has("-F") || has("--field") {
                        c(Irreversible, External, "GitHub API write")
                    } else {
                        c(Read, Internal, "GitHub API read")
                    }
                }
                (_, "merge" | "delete") | ("release", _) => c(Irreversible, External, "cannot be taken back"),
                _ => c(Compensable, External, "GitHub write with an inverse"),
            }
        }
        _ => c(Unknown, None, "unclassified command"),
    }
}

fn git(sub: &str, args: &[&str]) -> Class {
    use Disclosure::*;
    use State::*;
    let has = |f: &str| args.iter().any(|a| *a == f || a.starts_with(&format!("{f}=")));
    let c = |s, d, r: &str| Class::new(s, d, format!("git {sub}: {r}"));
    match sub {
        "status" | "diff" | "log" | "show" | "blame" | "rev-parse" | "ls-files" | "grep" | "describe"
        | "shortlog" | "reflog" | "cat-file" | "ls-tree" | "remote" | "config" if !has("--add") && !has("--unset") => {
            c(Read, None, "reads")
        }
        "branch" | "tag" if args.len() <= 1 || has("--list") || has("-l") => c(Read, None, "lists"),
        "push" => {
            if has("--dry-run") || has("-n") {
                c(Read, External, "dry run")
            } else if has("--force") || has("-f") || has("--force-with-lease") || args.iter().any(|a| a.starts_with('+')) {
                c(Irreversible, External, "force push rewrites remote history")
            } else if has("--delete") || has("-d") {
                c(Irreversible, External, "deletes a remote ref")
            } else {
                c(Compensable, External, "push (the branch can be deleted, but CI may have fetched it)")
            }
        }
        "fetch" | "pull" | "clone" | "ls-remote" => c(Local, Internal, "reads a remote"),
        _ => c(Local, None, "changes the repository"),
    }
}

/// Split a command line into simple commands on `;`, `&&`, `||`, `|`, `&`, newlines, and `$(...)` /
/// backtick substitutions, honouring quotes. Not a full shell parser; unknowns still fail safe.
pub fn split_commands(line: &str) -> Vec<Vec<String>> {
    let mut cmds: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let flush_word =|cur: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(cur));
            *in_word = false;
        }
    };
    let mut subs: Vec<String> = Vec::new();
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    cur.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    } else if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
                        let (inner, end) = take_paren(&chars, i + 2);
                        subs.push(inner);
                        i = end;
                    }
                    if i < chars.len() {
                        cur.push(chars[i]);
                    }
                    i += 1;
                }
            }
            '\\' => {
                if i + 1 < chars.len() {
                    cur.push(chars[i + 1]);
                    in_word = true;
                    i += 1;
                }
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let (inner, end) = take_paren(&chars, i + 2);
                subs.push(inner);
                cur.push_str("$()");
                in_word = true;
                i = end;
            }
            '`' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j] != '`' {
                    j += 1;
                }
                subs.push(chars[start..j.min(chars.len())].iter().collect());
                i = j;
            }
            ';' | '|' | '&' | '\n' | '(' | ')' => {
                flush_word(&mut cur, &mut in_word, &mut words);
                if !words.is_empty() {
                    cmds.push(std::mem::take(&mut words));
                }
            }
            '>' | '<' => {
                flush_word(&mut cur, &mut in_word, &mut words);
                // Redirect: a write to a file is a local change.
                let is_write = ch == '>';
                while i + 1 < chars.len() && (chars[i + 1] == '>' || chars[i + 1] == '&') {
                    i += 1;
                }
                if is_write {
                    // skip whitespace and capture target
                    let mut j = i + 1;
                    while j < chars.len() && chars[j] == ' ' {
                        j += 1;
                    }
                    let mut target = String::new();
                    while j < chars.len() && !" ;|&\n".contains(chars[j]) {
                        target.push(chars[j]);
                        j += 1;
                    }
                    if !target.is_empty() && target != "/dev/null" && !target.chars().all(|c| c.is_ascii_digit()) {
                        let t = if target.starts_with('/') && !target.starts_with("/tmp") { "/" } else { "" };
                        cmds.push(vec!["touch".into(), format!("{t}{target}")]);
                    }
                    i = j - 1;
                }
            }
            c if c.is_whitespace() => flush_word(&mut cur, &mut in_word, &mut words),
            c => {
                cur.push(c);
                in_word = true;
            }
        }
        i += 1;
    }
    flush_word(&mut cur, &mut in_word, &mut words);
    if !words.is_empty() {
        cmds.push(words);
    }
    for s in subs {
        cmds.extend(split_commands(&s));
    }
    cmds
}

fn take_paren(chars: &[char], start: usize) -> (String, usize) {
    let mut depth = 1;
    let mut j = start;
    while j < chars.len() {
        match chars[j] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    (chars[start..j.min(chars.len())].iter().collect(), j)
}
