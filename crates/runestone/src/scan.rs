//! In-process snapshot of a plain worktree, and in-process checkout.
//!
//! A snapshot walks the tree with git's ignore rules, stats every file and hashes only those whose stat
//! data changed since they were last hashed (a persistent stat cache, like git's index), then builds the
//! tree objects itself. It produces the same tree id as `git add -A && git write-tree`; where it cannot be
//! sure of that (an embedded repository, a file name that is not UTF-8) it says so and the caller falls
//! back to git.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use crate::objects::Entry;
use crate::world::World;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// What a file looked like when it was hashed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Stat {
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
    size: u64,
    ino: u64,
    mode: u32,
}

impl Stat {
    fn of(m: &std::fs::Metadata) -> Stat {
        Stat { mtime: m.mtime(), mtime_ns: m.mtime_nsec(), ctime: m.ctime(), ctime_ns: m.ctime_nsec(), size: m.size(), ino: m.ino(), mode: m.mode() }
    }
}

#[derive(Default)]
pub(crate) struct StatCache {
    loaded: bool,
    dirty: bool,
    entries: HashMap<String, (Stat, String)>,
}

/// Why the in-process scan declined; the caller uses git instead.
#[derive(Debug)]
pub(crate) struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "in-process scan unsupported: {}", self.0)
    }
}
impl std::error::Error for Unsupported {}

/// Git's ignore rules during a walk. Directories that are ignored are never entered, so each path is
/// matched only against its own patterns, not its parents' (git does the same).
struct Rules {
    exclude: Option<Gitignore>,
    global: Option<Gitignore>,
}

impl Rules {
    fn new(root: &Path, exclude_file: &Path) -> Rules {
        let mut b = GitignoreBuilder::new(root);
        for pat in crate::world::DEFAULT_EXCLUDES {
            let _ = b.add_line(None, pat);
        }
        if exclude_file.exists() {
            let _ = b.add(exclude_file);
        }
        let (global, _) = Gitignore::global();
        Rules { exclude: b.build().ok(), global: (!global.is_empty()).then_some(global) }
    }

    /// Precedence: the nearest .gitignore first, then outer ones, then info/exclude, then the global file.
    fn ignored(&self, stack: &[Gitignore], abs: &Path, rel: &str, is_dir: bool) -> bool {
        for gi in stack.iter().rev() {
            let m = gi.matched(abs, is_dir);
            if !m.is_none() {
                return m.is_ignore();
            }
        }
        if let Some(gi) = &self.exclude {
            let m = gi.matched(abs, is_dir);
            if !m.is_none() {
                return m.is_ignore();
            }
        }
        self.global.as_ref().is_some_and(|gi| gi.matched(Path::new(rel), is_dir).is_ignore())
    }
}

/// Files modified this recently are hashed every time: a write in the same clock tick as the snapshot
/// could otherwise keep its old stat data and hide behind the cache ("racy git").
const RACY_SECS: i64 = 2;

impl World {
    fn stat_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.stat", self.index_path.display()))
    }

    fn load_stats(&self, cache: &mut StatCache) {
        if cache.loaded {
            return;
        }
        cache.loaded = true;
        let Ok(text) = std::fs::read_to_string(self.stat_path()) else { return };
        for line in text.lines() {
            let mut f = line.splitn(10, ' ');
            let mut n = || f.next().and_then(|x| x.parse::<i64>().ok());
            let (Some(mt), Some(mtn), Some(ct), Some(ctn), Some(size), Some(ino), Some(mode)) = (n(), n(), n(), n(), n(), n(), n()) else {
                continue;
            };
            let (Some(sha), Some(path)) = (f.next(), f.next()) else { continue };
            let st = Stat { mtime: mt, mtime_ns: mtn, ctime: ct, ctime_ns: ctn, size: size as u64, ino: ino as u64, mode: mode as u32 };
            cache.entries.insert(path.to_string(), (st, sha.to_string()));
        }
    }

    fn save_stats(&self, cache: &mut StatCache) -> Result<()> {
        if !cache.dirty {
            return Ok(());
        }
        let mut out = String::with_capacity(cache.entries.len() * 96);
        for (path, (s, sha)) in &cache.entries {
            if path.contains('\n') {
                continue;
            }
            out += &format!("{} {} {} {} {} {} {} {sha} {path}\n", s.mtime, s.mtime_ns, s.ctime, s.ctime_ns, s.size, s.ino, s.mode);
        }
        let path = self.stat_path();
        let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &path)?;
        cache.dirty = false;
        Ok(())
    }

    /// The tree of the live worktree, computed in-process. Err(Unsupported) when git must do it.
    ///
    /// Two phases: a parallel walk (stat, look up the cache, hash what changed; subtrees spread over
    /// threads), then the tree objects bottom-up on this thread.
    pub(crate) fn scan_snapshot(&self) -> Result<String> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let rules = Rules::new(&self.worktree, &self.git_dir.join("info/exclude"));
        let mut cache = self.stats.borrow_mut();
        self.load_stats(&mut cache);
        let walker = Walker { git_dir: &self.git_dir, rules: &rules, cache: &cache.entries, now };
        let root = walker.walk_parallel(&self.worktree)?;
        // Usually nothing to store: every file was a hit, or changed too recently to cache (racy). A file
        // hashed long enough after its last change is added; a cached file that is gone forces a rebuild.
        let mut n = Counts::default();
        root.count(&mut n);
        let tree = if n.in_cache != cache.entries.len() {
            let mut fresh = HashMap::with_capacity(cache.entries.len());
            let t = self.build(&root, "", &mut Update::Fresh(&mut fresh))?;
            cache.entries = fresh;
            cache.dirty = true;
            t
        } else if n.new_cacheable > 0 {
            cache.dirty = true;
            self.build(&root, "", &mut Update::Add(&mut cache.entries))?
        } else {
            self.build(&root, "", &mut Update::None)?
        };
        self.save_stats(&mut cache)?;
        Ok(tree.unwrap_or_else(|| crate::objects::EMPTY_TREE.to_string()))
    }

    fn build(&self, node: &Node, rel: &str, up: &mut Update) -> Result<Option<String>> {
        let mut entries = Vec::with_capacity(node.items.len());
        for (name, item) in &node.items {
            let r = || if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            match item {
                Item::Dir(child) => {
                    if let Some(sha) = self.build(child, &r(), up)? {
                        entries.push(Entry { mode: "40000".into(), name: name.clone(), sha });
                    }
                }
                Item::File { mode, sha, stat, hit, .. } => {
                    match (stat, &mut *up) {
                        (Some(st), Update::Fresh(m)) => {
                            m.insert(r(), (st.clone(), sha.clone()));
                        }
                        (Some(st), Update::Add(m)) if !hit => {
                            m.insert(r(), (st.clone(), sha.clone()));
                        }
                        _ => {}
                    }
                    entries.push(Entry { mode: (*mode).into(), name: name.clone(), sha: sha.clone() });
                }
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.odb.write_tree(&mut entries)?))
    }

    /// Make the worktree go from tree `from` (what it is now) to tree `to`, touching only what differs.
    /// New and changed files are written to a temporary name and renamed into place.
    pub(crate) fn apply_tree(&self, from: &str, to: &str) -> Result<()> {
        let mut changes = Vec::new();
        self.tree_changes(Some(from), Some(to), "", &mut changes)?;
        // Removals first (deepest first), so a directory can become a file and the other way round.
        let mut gone: Vec<&String> = changes.iter().filter(|c| c.1.is_some() && c.2.is_none()).map(|c| &c.0).collect();
        gone.sort_by(|a, b| b.cmp(a));
        for p in &gone {
            let abs = self.worktree.join(p);
            match std::fs::remove_file(&abs) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("remove {}", abs.display())),
            }
            // Prune directories the removal emptied (never the worktree itself).
            let mut d = abs.parent();
            while let Some(dir) = d {
                if dir == self.worktree || std::fs::remove_dir(dir).is_err() {
                    break;
                }
                d = dir.parent();
            }
        }
        for (p, _, new) in &changes {
            let Some((mode, sha)) = new else { continue };
            let abs = self.worktree.join(p);
            if let Ok(m) = std::fs::symlink_metadata(&abs) {
                if m.is_dir() {
                    bail!("cannot restore {p}: a directory with untracked or ignored files is in the way");
                }
            }
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
            }
            let (kind, data) = self.odb.read(sha)?;
            if kind != "blob" {
                bail!("{p}: expected a blob, found {kind}");
            }
            let tmp = abs.with_file_name(format!(".{}.rune-{}.tmp", abs.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
            let _ = std::fs::remove_file(&tmp);
            if mode == "120000" {
                std::os::unix::fs::symlink(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&data), &tmp)?;
            } else {
                std::fs::write(&tmp, &data)?;
                let umask = current_umask();
                let bits = if mode == "100755" { 0o777 } else { 0o666 } & !umask;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(bits))?;
            }
            std::fs::rename(&tmp, &abs).with_context(|| format!("write {}", abs.display()))?;
        }
        Ok(())
    }

    /// (path, old (mode, blob), new (mode, blob)) for every file that differs; recurses only into subtrees
    /// whose ids differ. Gitlinks are left alone, as git's checkout leaves submodule contents alone.
    #[allow(clippy::type_complexity)]
    fn tree_changes(&self, a: Option<&str>, b: Option<&str>, prefix: &str, out: &mut Vec<(String, Option<(String, String)>, Option<(String, String)>)>) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let ea = match a {
            Some(s) => self.odb.read_tree(s)?,
            None => Default::default(),
        };
        let eb = match b {
            Some(s) => self.odb.read_tree(s)?,
            None => Default::default(),
        };
        let ma: HashMap<&str, &Entry> = ea.iter().map(|e| (e.name.as_str(), e)).collect();
        let mb: HashMap<&str, &Entry> = eb.iter().map(|e| (e.name.as_str(), e)).collect();
        let mut names: Vec<&str> = ma.keys().chain(mb.keys()).copied().collect();
        names.sort_unstable();
        names.dedup();
        for n in names {
            let path = if prefix.is_empty() { n.to_string() } else { format!("{prefix}/{n}") };
            let (x, y) = (ma.get(n), mb.get(n));
            if let (Some(x), Some(y)) = (x, y) {
                if x.sha == y.sha && x.mode == y.mode {
                    continue;
                }
            }
            let tree_of = |e: Option<&&Entry>| e.filter(|e| e.is_tree()).map(|e| e.sha.clone());
            let file_of = |e: Option<&&Entry>| e.filter(|e| !e.is_tree() && e.mode != "160000").map(|e| (e.mode.clone(), e.sha.clone()));
            let (ta, tb) = (tree_of(x), tree_of(y));
            if ta.is_some() || tb.is_some() {
                self.tree_changes(ta.as_deref(), tb.as_deref(), &path, out)?;
            }
            let (fa, fb) = (file_of(x), file_of(y));
            if fa.is_some() || fb.is_some() {
                out.push((path, fa, fb));
            }
        }
        Ok(())
    }
}

struct Node {
    items: Vec<(String, Item)>,
}

enum Update<'a> {
    None,
    /// Insert the files hashed now that may be cached.
    Add(&'a mut HashMap<String, (Stat, String)>),
    /// Build the whole cache anew.
    Fresh(&'a mut HashMap<String, (Stat, String)>),
}

#[derive(Default)]
struct Counts {
    /// Files seen whose path has a cache entry (current or stale).
    in_cache: usize,
    /// Files hashed now, old enough to cache, with no entry yet.
    new_cacheable: usize,
}

impl Node {
    fn count(&self, n: &mut Counts) {
        for (_, it) in &self.items {
            match it {
                Item::Dir(d) => d.count(n),
                Item::File { stat, hit, in_cache, .. } => {
                    n.in_cache += *in_cache as usize;
                    n.new_cacheable += (stat.is_some() && !hit) as usize;
                }
            }
        }
    }
}

enum Item {
    Dir(Node),
    /// stat: Some when the entry may be cached (not racy).
    File { mode: &'static str, sha: String, stat: Option<Stat>, hit: bool, in_cache: bool },
}

struct Walker<'a> {
    git_dir: &'a Path,
    rules: &'a Rules,
    cache: &'a HashMap<String, (Stat, String)>,
    now: i64,
}

impl Walker<'_> {
    /// Walk the root on this thread, but hand its subdirectories to a pool of threads.
    fn walk_parallel(&self, root: &Path) -> Result<Node> {
        let mut stack = Vec::new();
        let pushed = push_gitignore(root, &mut stack);
        let _ = pushed;
        let (mut node, subdirs) = self.read_dir(root, "", &stack, false)?;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
        let results: Vec<(usize, Result<Node>)> = if threads <= 1 || subdirs.len() <= 1 {
            subdirs.iter().map(|(i, abs, rel)| (*i, self.walk(abs, rel, &mut stack.clone()))).collect()
        } else {
            let queue = std::sync::Mutex::new(subdirs.iter());
            let out = std::sync::Mutex::new(Vec::new());
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(|| loop {
                        let next = queue.lock().unwrap().next();
                        let Some((i, abs, rel)) = next else { break };
                        let r = self.walk(abs, rel, &mut stack.clone());
                        out.lock().unwrap().push((*i, r));
                    });
                }
            });
            out.into_inner().unwrap()
        };
        for (i, r) in results {
            node.items[i].1 = Item::Dir(r?);
        }
        Ok(node)
    }

    fn walk(&self, dir: &Path, rel: &str, stack: &mut Vec<Gitignore>) -> Result<Node> {
        let pushed = push_gitignore(dir, stack);
        let res = self.read_dir(dir, rel, stack, true).map(|(n, _)| n);
        if pushed {
            stack.pop();
        }
        res
    }

    /// Entries of one directory. With `recurse`, subdirectories are walked here; otherwise they are
    /// returned (index into items, abs, rel) with a placeholder for the caller to fill.
    #[allow(clippy::type_complexity)]
    fn read_dir(&self, dir: &Path, rel: &str, stack: &[Gitignore], recurse: bool) -> Result<(Node, Vec<(usize, PathBuf, String)>)> {
        let mut items = Vec::new();
        let mut subdirs = Vec::new();
        for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let e = e?;
            let name = match e.file_name().into_string() {
                Ok(n) => n,
                Err(n) => return Err(Unsupported(format!("non-UTF-8 name {n:?}")).into()),
            };
            if name == ".git" {
                if rel.is_empty() {
                    continue; // the project's own repository
                }
                return Err(Unsupported(format!("embedded repository at {rel}")).into());
            }
            let Ok(ft) = e.file_type() else { continue };
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let abs = e.path();
            if ft.is_dir() {
                if self.rules.ignored(stack, &abs, &r, true) {
                    continue;
                }
                if recurse {
                    let mut st = stack.to_vec();
                    items.push((name, Item::Dir(self.walk(&abs, &r, &mut st)?)));
                } else {
                    subdirs.push((items.len(), abs, r));
                    items.push((name, Item::Dir(Node { items: Vec::new() })));
                }
            } else if ft.is_file() || ft.is_symlink() {
                if self.rules.ignored(stack, &abs, &r, false) {
                    continue;
                }
                let Ok(m) = e.metadata() else { continue }; // vanished while we walked
                let st = Stat::of(&m);
                let entry = self.cache.get(&r);
                let in_cache = entry.is_some();
                let cached = entry.filter(|(s, _)| *s == st).map(|(_, sha)| sha.clone());
                let hit = cached.is_some();
                let sha = match cached {
                    Some(sha) => sha,
                    None => {
                        let data = if ft.is_symlink() {
                            std::fs::read_link(&abs)?.into_os_string().into_encoded_bytes()
                        } else {
                            match std::fs::read(&abs) {
                                Ok(d) => d,
                                Err(_) if !abs.exists() => continue,
                                Err(err) => return Err(err).with_context(|| format!("read {}", abs.display())),
                            }
                        };
                        crate::objects::write_loose(self.git_dir, "blob", &data)?
                    }
                };
                let racy = st.mtime >= self.now - RACY_SECS || st.ctime >= self.now - RACY_SECS;
                let mode = if ft.is_symlink() {
                    "120000"
                } else if m.mode() & 0o100 != 0 {
                    "100755"
                } else {
                    "100644"
                };
                items.push((name, Item::File { mode, sha, stat: (!racy).then_some(st), hit, in_cache }));
            }
            // sockets, fifos, devices: git skips them too
        }
        Ok((Node { items }, subdirs))
    }
}

/// Push `dir`'s .gitignore onto the stack if it has one; true when pushed.
fn push_gitignore(dir: &Path, stack: &mut Vec<Gitignore>) -> bool {
    match std::fs::metadata(dir.join(".gitignore")) {
        Ok(m) if m.is_file() => {
            let mut b = GitignoreBuilder::new(dir);
            let _ = b.add(dir.join(".gitignore"));
            match b.build() {
                Ok(gi) if !gi.is_empty() => {
                    stack.push(gi);
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn current_umask() -> u32 {
    // umask can only be read by setting it; restore at once.
    unsafe {
        let m = libc::umask(0o022);
        libc::umask(m);
        m as u32
    }
}

