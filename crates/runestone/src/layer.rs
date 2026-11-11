//! Layered worlds: overlayfs forks whose snapshots cost O(changes), not O(tree).
//!
//! A base tree is materialized once into a shared, read-only cache directory. Each fork mounts
//! `overlay(lower = base, upper = its own dir)`; everything the agent writes lands in the upper dir, so
//! the upper dir *is* the diff. A snapshot walks the upper dir (files, whiteouts for deletions, opaque
//! directories for replaced ones), hashes only what changed since the last snapshot, and patches a
//! cached index of the base tree. Mounting needs an unprivileged user + mount namespace (`RUNE_NS=1`;
//! the CLI re-executes itself under `unshare`).

use crate::engine::Engine;
use crate::store::Store;
use crate::world::World;
use anyhow::{bail, Context, Result};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directories never captured from an upper layer (build output, dependencies, VCS metadata).
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "venv", "__pycache__", ".next", "dist", "build"];

pub struct LayerRef {
    pub base_tree: String,
    pub upper: PathBuf,
}

pub fn in_namespace() -> bool {
    std::env::var("RUNE_NS").as_deref() == Ok("1")
}

/// Materialize `tree` once into the shared base cache and return its directory.
pub fn ensure_base(world: &World, home: &Path, tree: &str) -> Result<PathBuf> {
    let root = home.join("layers/base");
    let dir = root.join(tree);
    let done = root.join(format!("{tree}.done"));
    if done.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&root)?;
    let tmp = root.join(format!("{tree}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    world.materialize(tree, &tmp)?;
    match std::fs::rename(&tmp, &dir) {
        Ok(()) => {}
        Err(_) if dir.exists() => {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(e) => return Err(e.into()),
    }
    std::fs::write(&done, b"")?;
    Ok(dir)
}

pub struct Layer {
    pub id: String,
    pub base_tree: String,
    pub lowers: Vec<PathBuf>,
    pub root: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub merged: PathBuf,
    mounted: std::cell::Cell<bool>,
}

impl Layer {
    /// A fresh fork directory over `lowers` (first = topmost). Not mounted yet.
    pub fn create(home: &Path, id: &str, base_tree: &str, lowers: Vec<PathBuf>) -> Result<Layer> {
        let root = home.join("layers/forks").join(id);
        if root.exists() {
            let _ = Command::new("umount").arg(root.join("merged")).status();
            std::fs::remove_dir_all(&root)?;
        }
        let (upper, work, merged) = (root.join("upper"), root.join("work"), root.join("merged"));
        for d in [&upper, &work, &merged] {
            std::fs::create_dir_all(d)?;
        }
        Ok(Layer {
            id: id.into(),
            base_tree: base_tree.into(),
            lowers,
            root,
            upper,
            work,
            merged,
            mounted: std::cell::Cell::new(false),
        })
    }

    pub fn mount(&self) -> Result<()> {
        if !in_namespace() {
            bail!("layered worlds need a user + mount namespace (run through the rune CLI, which sets RUNE_NS=1)");
        }
        let lowers: Vec<String> = self.lowers.iter().map(|p| p.display().to_string()).collect();
        let opts = format!(
            "userxattr,lowerdir={},upperdir={},workdir={}",
            lowers.join(":"),
            self.upper.display(),
            self.work.display()
        );
        let out = Command::new("mount").args(["-t", "overlay", "overlay", "-o", &opts]).arg(&self.merged).output()?;
        if !out.status.success() {
            bail!("mount overlay: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        self.mounted.set(true);
        Ok(())
    }

    pub fn umount(&self) -> Result<()> {
        if self.mounted.get() {
            let out = Command::new("umount").arg(&self.merged).output()?;
            if !out.status.success() {
                // A process still running in the fork (a dev server) holds it; detach lazily.
                let _ = Command::new("umount").arg("-l").arg(&self.merged).status();
            }
            self.mounted.set(false);
        }
        Ok(())
    }

    /// Unmount and delete the fork's directories. Its trees stay in the record.
    pub fn remove(&self) {
        let _ = self.umount();
        let _ = make_writable(&self.work);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn make_writable(p: &Path) -> std::io::Result<()> {
    // overlay leaves a 000-mode "work/work" directory behind
    for e in std::fs::read_dir(p)?.flatten() {
        let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

impl World {
    /// A world over a mounted layer. Shares the parent's object store; keeps its own index.
    pub fn layered(parent: &World, layer: &Layer, key: &str) -> Result<World> {
        let worktree = std::fs::canonicalize(&layer.merged).context("layer merged dir")?;
        Ok(World {
            worktree,
            git_dir: parent.git_dir.clone(),
            key: key.to_string(),
            index_path: parent.git_dir.join(format!("index.layer.{key}")),
            layer: Some(LayerRef { base_tree: layer.base_tree.clone(), upper: layer.upper.clone() }),
            hashes: Default::default(),
            alias: Some(parent.worktree.clone()),
            odb: crate::objects::Odb::new(&parent.git_dir),
        })
    }

    pub(crate) fn layered_snapshot(&self) -> Result<String> {
        let l = self.layer.as_ref().expect("layered world");
        let mut files: Vec<(String, PathBuf, std::fs::Metadata)> = Vec::new();
        let mut removed: Vec<String> = Vec::new();
        walk(&l.upper, "", &mut files, &mut removed)?;

        // Respect the project's .gitignore files and rune's excludes for added files.
        let mut ig = Ignores::new(&self.worktree, &self.git_dir.join("info/exclude"));
        files.retain(|(rel, _, _)| !ig.ignored(rel));

        // Blobs: hash only what changed since the last snapshot of this layer, written in-process.
        let mut items: Vec<(String, Option<(String, String)>)> = removed.into_iter().map(|r| (r, None)).collect();
        {
            let mut cache = self.hashes.borrow_mut();
            for (rel, abs, m) in &files {
                let fresh = cache.get(abs).filter(|c| Some(c.0) == m.modified().ok() && c.1 == m.len() && c.2 == m.mode());
                let sha = match fresh {
                    Some(c) => c.3.clone(),
                    None => {
                        let sha = if m.file_type().is_symlink() {
                            self.odb.write("blob", std::fs::read_link(abs)?.to_string_lossy().as_bytes())?
                        } else {
                            self.odb.write_blob_file(abs)?
                        };
                        cache.insert(abs.clone(), (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), m.mode(), sha.clone()));
                        sha
                    }
                };
                let mode = if m.file_type().is_symlink() {
                    "120000"
                } else if m.mode() & 0o111 != 0 {
                    "100755"
                } else {
                    "100644"
                };
                items.push((rel.clone(), Some((mode.to_string(), sha))));
            }
        }
        // Component-wise order keeps each directory's changes contiguous, and puts a deletion of "d"
        // (whiteout or opaque dir) before additions under "d/".
        items.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')).then(a.1.is_some().cmp(&b.1.is_some())));
        let tree = self.build_tree(Some(&l.base_tree), "", &items)?;
        Ok(tree.unwrap_or_else(|| crate::objects::EMPTY_TREE.to_string()))
    }

    /// Rebuild only the directories that have changes; untouched subtrees keep their hashes.
    fn build_tree(&self, base: Option<&str>, dir: &str, items: &[(String, Option<(String, String)>)]) -> Result<Option<String>> {
        use crate::objects::Entry;
        let mut entries: std::collections::BTreeMap<String, Entry> = match base {
            Some(b) => self.odb.read_tree(b)?.iter().map(|e| (e.name.clone(), e.clone())).collect(),
            None => Default::default(),
        };
        let rel = |p: &str| -> String { if dir.is_empty() { p.to_string() } else { p[dir.len() + 1..].to_string() } };
        let mut i = 0;
        while i < items.len() {
            let r = rel(&items[i].0);
            let first = r.split('/').next().unwrap_or("").to_string();
            let mut j = i;
            while j < items.len() && rel(&items[j].0).split('/').next() == Some(first.as_str()) {
                j += 1;
            }
            let child = if dir.is_empty() { first.clone() } else { format!("{dir}/{first}") };
            let mut deeper = Vec::new();
            for it in &items[i..j] {
                if it.0 == child {
                    match &it.1 {
                        None => {
                            entries.remove(&first);
                        }
                        Some((mode, sha)) => {
                            entries.insert(first.clone(), Entry { mode: mode.clone(), name: first.clone(), sha: sha.clone() });
                        }
                    }
                } else {
                    deeper.push(it.clone());
                }
            }
            if !deeper.is_empty() {
                let sub = entries.get(&first).filter(|e| e.is_tree()).map(|e| e.sha.clone());
                match self.build_tree(sub.as_deref(), &child, &deeper)? {
                    Some(t) => {
                        entries.insert(first.clone(), Entry { mode: "40000".into(), name: first.clone(), sha: t });
                    }
                    None => {
                        entries.remove(&first);
                    }
                }
            }
            i = j;
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let mut v: Vec<Entry> = entries.into_values().collect();
        Ok(Some(self.odb.write_tree(&mut v)?))
    }
}

/// .gitignore matching in-process: rune's excludes, then each ancestor directory's .gitignore, the
/// deepest one deciding (as git does).
struct Ignores {
    root: PathBuf,
    exclude: Option<ignore::gitignore::Gitignore>,
    dirs: std::collections::HashMap<String, Option<ignore::gitignore::Gitignore>>,
}

impl Ignores {
    fn new(root: &Path, exclude_file: &Path) -> Self {
        let mut b = ignore::gitignore::GitignoreBuilder::new(root);
        for pat in crate::world::DEFAULT_EXCLUDES {
            let _ = b.add_line(None, pat);
        }
        if exclude_file.exists() {
            let _ = b.add(exclude_file);
        }
        Ignores { root: root.to_path_buf(), exclude: b.build().ok(), dirs: Default::default() }
    }

    fn matcher(&mut self, dir: &str) -> Option<&ignore::gitignore::Gitignore> {
        if !self.dirs.contains_key(dir) {
            let d = if dir.is_empty() { self.root.clone() } else { self.root.join(dir) };
            let f = d.join(".gitignore");
            let gi = if f.exists() {
                let mut b = ignore::gitignore::GitignoreBuilder::new(&d);
                let _ = b.add(&f);
                b.build().ok()
            } else {
                None
            };
            self.dirs.insert(dir.to_string(), gi);
        }
        self.dirs.get(dir).and_then(|g| g.as_ref())
    }

    fn ignored(&mut self, rel: &str) -> bool {
        let full = self.root.join(rel);
        let comps: Vec<&str> = rel.split('/').collect();
        for depth in (0..comps.len()).rev() {
            let dir = comps[..depth].join("/");
            if let Some(gi) = self.matcher(&dir) {
                let m = gi.matched_path_or_any_parents(&full, false);
                if m.is_ignore() {
                    return true;
                }
                if m.is_whitelist() {
                    return false;
                }
            }
        }
        match &self.exclude {
            Some(gi) => gi.matched_path_or_any_parents(&full, false).is_ignore(),
            None => false,
        }
    }
}

/// Walk an upper dir: regular files and symlinks are additions; whiteouts (char 0/0) are deletions;
/// opaque directories replace whatever the lower layers had under them.
fn walk(dir: &Path, rel: &str, files: &mut Vec<(String, PathBuf, std::fs::Metadata)>, removed: &mut Vec<String>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let m = std::fs::symlink_metadata(e.path())?;
        let ft = m.file_type();
        if ft.is_char_device() && m.rdev() == 0 {
            removed.push(r);
        } else if ft.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let opaque = xattr::get(e.path(), "user.overlay.opaque").ok().flatten().is_some_and(|v| v == b"y")
                || xattr::get(e.path(), "trusted.overlay.opaque").ok().flatten().is_some_and(|v| v == b"y");
            if opaque {
                removed.push(r.clone());
            }
            walk(&e.path(), &r, files, removed)?;
        } else if ft.is_file() || ft.is_symlink() {
            files.push((r, e.path(), m));
        }
    }
    Ok(())
}

impl Engine {
    /// An engine over a plain directory copy of `base_tree` (for machines without user namespaces).
    /// The copy is checked out with the same index the fork snapshots with, so its stat cache is fresh
    /// and the first scan only stats files. Objects are shared with the parent.
    pub fn open_copy(parent: &Engine, dir: &Path, base_tree: &str, key: &str) -> Result<Engine> {
        std::fs::create_dir_all(dir)?;
        let index = parent.world.git_dir.join(format!("index.copy.{key}"));
        let _ = std::fs::remove_file(&index);
        parent.world.git_idx(&index, dir, &["read-tree", base_tree])?;
        parent.world.git_idx(&index, dir, &["checkout-index", "-a", "-f"])?;
        let world = World {
            worktree: std::fs::canonicalize(dir)?,
            git_dir: parent.world.git_dir.clone(),
            key: key.to_string(),
            index_path: index,
            layer: None,
            hashes: Default::default(),
            alias: Some(parent.world.worktree.clone()),
            odb: crate::objects::Odb::new(&parent.world.git_dir),
        };
        Ok(Engine { store: Store::open(&parent.home.join("rune.db"))?, world, home: parent.home.clone() })
    }

    /// An engine whose world is a mounted layer. Same database and object store as the parent.
    pub fn open_layered(parent: &Engine, layer: &Layer, key: &str) -> Result<Engine> {
        Ok(Engine {
            store: Store::open(&parent.home.join("rune.db"))?,
            world: World::layered(&parent.world, layer, key)?,
            home: parent.home.clone(),
        })
    }
}
