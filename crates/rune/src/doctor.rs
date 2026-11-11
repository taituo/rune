//! `rune doctor`: what works on this machine and what does not, with the fix for each gap.

use std::path::Path;
use std::process::{Command, Stdio};

struct Report {
    fatal: bool,
}

impl Report {
    fn ok(&self, what: &str, detail: &str) {
        println!("  ok    {what:<22} {detail}");
    }
    fn warn(&self, what: &str, detail: &str) {
        println!("  warn  {what:<22} {detail}");
    }
    fn fail(&mut self, what: &str, detail: &str) {
        self.fatal = true;
        println!("  FAIL  {what:<22} {detail}");
    }
}

fn quiet(cmd: &mut Command) -> Option<(bool, String)> {
    let out = cmd.stdin(Stdio::null()).output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), s.trim().to_string()))
}

fn sysctl(name: &str) -> Option<String> {
    std::fs::read_to_string(format!("/proc/sys/{}", name.replace('.', "/"))).ok().map(|s| s.trim().to_string())
}

fn git_version() -> Option<(u32, u32, String)> {
    let (_, s) = quiet(Command::new("git").arg("--version"))?;
    let v = s.split_whitespace().nth(2)?.to_string();
    let mut it = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    Some((it.next()?, it.next().unwrap_or(0), v))
}

/// Mount an overlay with `userxattr` inside a fresh user + mount namespace, the way search forks do.
fn overlay_works(scratch: &Path) -> Result<(), String> {
    for d in ["lower", "upper", "work", "merged"] {
        std::fs::create_dir_all(scratch.join(d)).map_err(|e| e.to_string())?;
    }
    std::fs::write(scratch.join("lower/f"), "x").map_err(|e| e.to_string())?;
    let p = |d: &str| scratch.join(d).display().to_string();
    let script = format!(
        "mount -t overlay overlay -o lowerdir={},upperdir={},workdir={},userxattr {} && echo y > {}/f && test -f {}/f",
        p("lower"),
        p("upper"),
        p("work"),
        p("merged"),
        p("merged"),
        p("upper")
    );
    match quiet(Command::new("unshare").args(["--user", "--map-root-user", "--mount", "sh", "-c", &script])) {
        Some((true, _)) => Ok(()),
        Some((false, e)) => Err(e.lines().last().unwrap_or("failed").to_string()),
        None => Err("unshare not found".into()),
    }
}

pub fn run() -> i32 {
    let mut r = Report { fatal: false };
    println!("rune doctor\n");

    println!("core");
    match git_version() {
        Some((maj, min, v)) if (maj, min) >= (2, 40) => r.ok("git", &v),
        Some((_, _, v)) => r.fail("git", &format!("{v}: need ≥ 2.40 (merge-tree --write-tree --merge-base)")),
        None => r.fail("git", "not found on PATH"),
    }
    let home = runestone::engine::rune_home();
    let probe = home.join(format!(".doctor.{}", std::process::id()));
    match std::fs::create_dir_all(&home).and_then(|_| std::fs::write(&probe, "x")) {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            r.ok("RUNE_HOME", &home.display().to_string());
        }
        Err(e) => r.fail("RUNE_HOME", &format!("{}: {e} (set RUNE_HOME to a writable dir)", home.display())),
    }
    match quiet(Command::new("python3").arg("--version")) {
        Some((true, v)) => r.ok("python3", &v),
        _ => r.warn("python3", "missing: fixtures and some checks need it"),
    }

    println!("\nisolation");
    let user = quiet(Command::new("unshare").args(["--user", "--map-root-user", "true"]));
    let userns = matches!(user, Some((true, _)));
    match &user {
        Some((true, _)) => r.ok("user namespaces", "unshare --user works"),
        None => r.warn("user namespaces", "`unshare` not found (install util-linux)"),
        Some((false, e)) => {
            let mut why = e.lines().last().unwrap_or("").to_string();
            if sysctl("kernel.apparmor_restrict_unprivileged_userns").as_deref() == Some("1") {
                why = "blocked by AppArmor: sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0".into();
            } else if sysctl("kernel.unprivileged_userns_clone").as_deref() == Some("0") {
                why = "disabled: sudo sysctl kernel.unprivileged_userns_clone=1".into();
            } else if Path::new("/.dockerenv").exists() || std::env::var("container").is_ok() {
                why = format!("{why} (inside a container: run it with user namespaces allowed)");
            }
            r.warn("user namespaces", &why)
        }
    }
    let net = userns
        && matches!(quiet(Command::new("unshare").args(["--user", "--map-root-user", "--net", "true"])), Some((true, _)));
    if net {
        r.ok("offline Bash", "commands can run without network");
    } else {
        r.warn("offline Bash", "unavailable: dry mode blocks unclassified commands; use --mode ask or auto");
    }
    let mount = userns
        && matches!(quiet(Command::new("unshare").args(["--user", "--map-root-user", "--mount", "true"])), Some((true, _)));
    let overlay = if mount {
        let scratch = std::env::temp_dir().join(format!("rune-doctor-{}", std::process::id()));
        let res = overlay_works(&scratch);
        let _ = std::fs::remove_dir_all(&scratch);
        match res {
            Ok(()) => {
                r.ok("overlay forks", "userxattr overlay mounts in a user namespace");
                true
            }
            Err(e) => {
                r.warn("overlay forks", &format!("{e}: search falls back to directory copies"));
                false
            }
        }
    } else {
        r.warn("overlay forks", "no mount namespace: search uses directory copies, fork jail is off");
        false
    };

    println!("\nmodel");
    let api = std::env::var("RUNE_API").unwrap_or_else(|_| "anthropic".into());
    let model = std::env::var("RUNE_MODEL").unwrap_or_else(|_| "claude-opus-5-5".into());
    let mut model_ok = false;
    if api == "openai" {
        match std::env::var("RUNE_BASE_URL") {
            Err(_) => r.warn("gateway", "RUNE_API=openai but RUNE_BASE_URL is not set"),
            Ok(base) => {
                let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
                match runestone::agent::openai::list_models(&base, key.as_deref()) {
                    Ok(ms) => {
                        r.ok("gateway", &format!("{base}: {} models", ms.len()));
                        if ms.iter().any(|m| m == &model) {
                            r.ok("model", &model);
                            model_ok = true;
                        } else {
                            r.warn("model", &format!("{model} not served; set RUNE_MODEL to one of `rune models`"));
                        }
                    }
                    Err(e) => r.warn("gateway", &format!("{base}: {e:#}")),
                }
            }
        }
    } else if std::env::var("ANTHROPIC_API_KEY").map(|k| !k.is_empty()).unwrap_or(false) {
        r.ok("anthropic", &format!("key set, model {model} (not called)"));
        model_ok = true;
    } else {
        r.warn("anthropic", "ANTHROPIC_API_KEY not set (or use RUNE_API=openai RUNE_BASE_URL=…)");
    }

    println!("\nwhat works here");
    let yes = |b: bool| if b { "yes" } else { "no" };
    println!("  record, rewind, fork, verify     {}", yes(!r.fatal));
    println!("  chat / run with a model          {}", yes(!r.fatal && model_ok));
    println!("  dry mode runs unknown commands   {}", yes(net));
    println!("  search forks                     {}", if overlay { "overlay" } else { "copy (slower on big trees, no jail)" });
    if r.fatal {
        1
    } else {
        0
    }
}
