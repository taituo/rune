//! World snapshots in a shadow git object store the agent never sees.
//!
//! One bare repository per worktree, keyed by the worktree's real path. The real repository's `.git`,
//! branches and stash are never touched. Objects are content-addressed, so storage grows only by changed
//! blobs, and an unchanged tree costs nothing.
//!
//! v1 snapshots by scan (`add -A` + `write-tree`). The Linux write-capture path (overlay upper = diff)
//! slots in behind the same `snapshot()` call.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Excluded by default: secrets, databases, dependency and build output. An excluded path is never
/// restored by a rewind, so the default list stays strict.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".env*", "*.pem", "*.key", "id_rsa*", "id_ed25519*", "*.sqlite", "*.sqlite3", "*.db",
    "node_modules/", "target/", "dist/", "build/", ".venv/", "venv/", "__pycache__/", ".next/",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub status: char, // A, M, D, T
    pub path: String,
    pub added: Option<u64>, // None for binary
    pub deleted: Option<u64>,
}

impl Change {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!([self.status.to_string(), self.path, self.added, self.deleted])
    }
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let a = v.as_array()?;
        Some(Change {
            status: a.first()?.as_str()?.chars().next()?,
            path: a.get(1)?.as_str()?.to_string(),
            added: a.get(2).and_then(|x| x.as_u64()),
            deleted: a.get(3).and_then(|x| x.as_u64()),
        })
    }
    pub fn stat(&self) -> String {
        match (self.added, self.deleted) {
            (Some(a), Some(d)) => format!("+{a} -{d}"),
            _ => "binary".into(),
        }
    }
}

pub struct Merge {
    pub tree: String,
    pub conflicts: Vec<String>,
}

pub struct World {
    pub worktree: PathBuf,
    pub git_dir: PathBuf,
    pub key: String,
    /// Index file used for snapshots of this world (each fork has its own; objects are shared).
    pub(crate) index_path: PathBuf,
    /// Set for a layered (overlay) fork: snapshots read the upper layer instead of scanning the tree.
    pub(crate) layer: Option<crate::layer::LayerRef>,
    /// path -> (mtime, size, mode, blob) so unchanged upper files are not re-hashed.
    pub(crate) hashes: std::cell::RefCell<std::collections::HashMap<PathBuf, (std::time::SystemTime, u64, u32, String)>>,
    /// The path the agent should see instead of `worktree` (a fork shows the main worktree's path).
    pub alias: Option<PathBuf>,
    /// In-process object store (no git subprocess for hashing, trees, diffs).
    pub(crate) odb: crate::objects::Odb,
}

impl World {
    pub fn open(home: &Path, worktree: &Path) -> Result<Self> {
        let worktree = std::fs::canonicalize(worktree)
            .with_context(|| format!("worktree {}", worktree.display()))?;
        let key = hex::encode(Sha256::digest(worktree.to_string_lossy().as_bytes()))[..16].to_string();
        let git_dir = home.join("repos").join(&key);
        let index_path = git_dir.join("index");
        let odb = crate::objects::Odb::new(&git_dir);
        let w = World { worktree, git_dir, key, index_path, layer: None, hashes: Default::default(), alias: None, odb };
        if !w.git_dir.join("HEAD").exists() {
            let _l = w.lock()?;
            if !w.git_dir.join("HEAD").exists() {
                w.init()?;
            }
        }
        Ok(w)
    }

    fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.git_dir)?;
        restrict(&self.git_dir);
        if let Some(repos) = self.git_dir.parent() {
            restrict(repos);
            if let Some(home) = repos.parent() {
                restrict(home);
            }
        }
        run(Command::new("git").args(["init", "-q", "--bare"]).arg(&self.git_dir))?;
        for (k, v) in [("core.untrackedCache", "true"), ("gc.auto", "0"), ("core.autocrlf", "false")] {
            self.git(&["config", k, v])?;
        }
        std::fs::create_dir_all(self.git_dir.join("info"))?;
        std::fs::write(self.git_dir.join("info/exclude"), DEFAULT_EXCLUDES.join("\n") + "\n")?;
        std::fs::write(self.git_dir.join("rune-worktree"), self.worktree.to_string_lossy().as_bytes())?;
        Ok(())
    }

    fn cmd(&self, index: &Path, work_tree: &Path) -> Command {
        let mut c = Command::new("git");
        c.env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", work_tree)
            .env("GIT_INDEX_FILE", index)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "rune")
            .env("GIT_AUTHOR_EMAIL", "rune@localhost")
            .env("GIT_COMMITTER_NAME", "rune")
            .env("GIT_COMMITTER_EMAIL", "rune@localhost")
            .env("GIT_AUTHOR_DATE", "@0 +0000")
            .env("GIT_COMMITTER_DATE", "@0 +0000")
            .env_remove("GIT_CEILING_DIRECTORIES")
            .current_dir(work_tree)
            .stdin(Stdio::null());
        c
    }

    fn index(&self) -> PathBuf {
        self.index_path.clone()
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        run(self.cmd(&self.index(), &self.worktree).args(args))
    }

    pub(crate) fn git_idx(&self, index: &Path, work_tree: &Path, args: &[&str]) -> Result<String> {
        run(self.cmd(index, work_tree).args(args))
    }

    /// Exclusive lock for this worktree's shadow repo and index. Two rune processes (or threads) on the
    /// same worktree take turns instead of colliding on git's index.lock. Released when dropped.
    pub(crate) fn lock(&self) -> Result<std::fs::File> {
        let path = self.git_dir.with_extension("lock");
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        f.lock()?;
        Ok(f)
    }

    /// Snapshot the live worktree. Unchanged tree = same id, no new objects. A layered fork reads only its
    /// upper layer, so the cost follows the changes, not the size of the tree.
    pub fn snapshot(&self) -> Result<String> {
        if self.layer.is_some() {
            return self.layered_snapshot();
        }
        let _l = self.lock()?;
        self.snapshot_locked()
    }

    fn snapshot_locked(&self) -> Result<String> {
        self.git(&["add", "-A", "--ignore-errors", "."]).or_else(|_| self.git(&["add", "-A", "."]))?;
        Ok(self.git(&["write-tree"])?.trim().to_string())
    }

    /// Deterministic commit wrapping a tree (fixed author/date, no parent): git's merge machinery needs
    /// commits, and the same tree always maps to the same commit.
    pub fn commit_of(&self, tree: &str) -> Result<String> {
        Ok(self.git(&["commit-tree", tree, "-m", "rune"])?.trim().to_string())
    }

    pub fn diff(&self, a: &str, b: &str) -> Result<Vec<Change>> {
        if a == b {
            return Ok(vec![]);
        }
        self.odb.diff(a, b)
    }

    /// Unified diff between two trees, optionally limited to paths.
    pub fn patch(&self, a: &str, b: &str, paths: &[String]) -> Result<String> {
        let mut args: Vec<&str> = vec!["diff-tree", "-p", "--binary", "--no-renames", a, b, "--"];
        args.extend(paths.iter().map(String::as_str));
        self.git(&args)
    }

    /// Apply the change base→theirs on top of ours (a cherry-pick in plumbing, no worktree involved).
    pub fn cherry(&self, base: &str, ours: &str, theirs: &str) -> Result<Merge> {
        let (b, o, t) = (self.commit_of(base)?, self.commit_of(ours)?, self.commit_of(theirs)?);
        let base_arg = format!("--merge-base={b}");
        let out = self
            .cmd(&self.index(), &self.worktree)
            .args(["merge-tree", "--write-tree", "-z", "--name-only", &base_arg, &o, &t])
            .output()?;
        // exit 0 = clean, 1 = conflicts, anything else = error
        match out.status.code() {
            Some(0) | Some(1) => {}
            _ => bail!("merge-tree failed: {}", String::from_utf8_lossy(&out.stderr)),
        }
        let s = String::from_utf8_lossy(&out.stdout);
        let mut fields = s.split('\0');
        let tree = fields.next().unwrap_or_default().trim().to_string();
        let mut conflicts = Vec::new();
        if out.status.code() == Some(1) {
            for f in fields {
                if f.is_empty() {
                    break; // end of the conflicted-file list; informational messages follow
                }
                if !conflicts.contains(&f.to_string()) {
                    conflicts.push(f.to_string());
                }
            }
        }
        Ok(Merge { tree, conflicts })
    }

    /// `tree` with each of `paths` taken from `from` (or removed if absent there).
    pub fn take_paths(&self, tree: &str, from: &str, paths: &[String]) -> Result<String> {
        let tmp = self.git_dir.join(format!("index.take.{}.{}", std::process::id(), next_seq()));
        let _guard = Cleanup(tmp.clone());
        self.git_idx(&tmp, &self.worktree, &["read-tree", tree])?;
        for p in paths {
            let entry = self.git(&["ls-tree", "-z", from, "--", p])?;
            let entry = entry.trim_end_matches('\0');
            self.git_idx(&tmp, &self.worktree, &["update-index", "--force-remove", "--", p])?;
            if !entry.is_empty() {
                // "<mode> <type> <sha>\t<path>"
                let meta = entry.split('\t').next().unwrap_or_default();
                let f: Vec<&str> = meta.split(' ').collect();
                if f.len() == 3 {
                    let info = format!("{},{},{}", f[0], f[2], p);
                    self.git_idx(&tmp, &self.worktree, &["update-index", "--add", "--cacheinfo", &info])?;
                }
            }
        }
        Ok(self.git_idx(&tmp, &self.worktree, &["write-tree"])?.trim().to_string())
    }

    /// Make the live worktree equal to `tree`. Snapshots first, so the pre-state is never lost, and uses a
    /// two-tree merge that refuses if the worktree moved between the snapshot and the checkout. Files are
    /// written fresh, so their mtimes are "now": build tools see restored sources as newer than old
    /// artifacts and rebuild. Excluded paths are never touched.
    pub fn checkout(&self, tree: &str) -> Result<String> {
        if self.layer.is_some() {
            let live = self.snapshot()?;
            if live != tree {
                self.git(&["read-tree", "-m", "-u", &live, tree])?;
            }
            return Ok(live);
        }
        let _l = self.lock()?;
        let live = self.snapshot_locked()?;
        if live != tree {
            self.git(&["read-tree", "-m", "-u", &live, tree])?;
        }
        Ok(live)
    }

    /// Keep `tree` reachable for git's gc. Events reference trees only from the database, so without a ref
    /// an abandoned branch's files could be collected.
    pub fn pin(&self, tree: &str) -> Result<()> {
        if tree == EMPTY_TREE {
            return Ok(());
        }
        // A loose ref file, written atomically; equivalent to `git update-ref` for a new ref.
        let path = self.git_dir.join("refs/rune/t").join(tree);
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_file_name(format!(".{tree}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, format!("{tree}\n"))?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(())
    }

    /// Move a long-lived directory (the check workspace) to `tree`, rewriting only what changed. Ignored
    /// paths (`target/`, `node_modules/` …) are left alone, so build caches stay warm between checks.
    pub fn checkout_into(&self, dir: &Path, tree: &str) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let index = self.git_dir.join("check-index");
        if index.exists() {
            self.git_idx(&index, dir, &["read-tree", "--reset", "-u", tree])?;
        } else {
            self.git_idx(&index, dir, &["read-tree", tree])?;
            self.git_idx(&index, dir, &["checkout-index", "-a", "-f"])?;
        }
        Ok(())
    }

    /// Write `tree` into a fresh directory (a fork's world).
    pub fn materialize(&self, tree: &str, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        // Unique per call: parallel attempts materialize at the same time.
        let tmp = self.git_dir.join(format!("index.mat.{}.{}", std::process::id(), next_seq()));
        let _guard = Cleanup(tmp.clone());
        self.git_idx(&tmp, dir, &["read-tree", tree])?;
        self.git_idx(&tmp, dir, &["checkout-index", "-a", "-f"])?;
        Ok(())
    }
}

fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn restrict(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    }
}

fn run(c: &mut Command) -> Result<String> {
    let out: Output = c.output().context("spawn git")?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", c.get_args().collect::<Vec<_>>(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
