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
        })
    }

    pub(crate) fn layered_snapshot(&self) -> Result<String> {
        let l = self.layer.as_ref().expect("layered world");
        let mut files: Vec<(String, PathBuf, std::fs::Metadata)> = Vec::new();
        let mut removed: Vec<String> = Vec::new();
        walk(&l.upper, "", &mut files, &mut removed)?;

        // Respect the project's .gitignore and rune's excludes for added files.
        if !files.is_empty() {
            let mut input = Vec::new();
            for (rel, _, _) in &files {
                input.extend_from_slice(rel.as_bytes());
                input.push(0);
            }
            let ignored = self.git_stdin(&self.index_path, &["check-ignore", "--stdin", "-z", "--no-index"], &input)?;
            let ignored: std::collections::HashSet<&str> = ignored.split('\0').filter(|s| !s.is_empty()).collect();
            if !ignored.is_empty() {
                files.retain(|(rel, _, _)| !ignored.contains(rel.as_str()));
            }
        }

        // Start from a cached index of the base tree.
        let base_index = self.git_dir.join("base-index").join(&l.base_tree);
        if !base_index.exists() {
            std::fs::create_dir_all(base_index.parent().unwrap())?;
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = PathBuf::from(format!("{}.tmp-{}-{seq}", base_index.display(), std::process::id()));
            self.git_with_index(&tmp, &["read-tree", &l.base_tree])?;
            std::fs::rename(&tmp, &base_index)?;
        }
        // Not with_extension: the key is part of the file name, and every fork needs its own scratch index.
        let idx = PathBuf::from(format!("{}.next", self.index_path.display()));
        std::fs::copy(&base_index, &idx)?;

        if !removed.is_empty() {
            let mut input = Vec::new();
            for r in &removed {
                input.extend_from_slice(r.as_bytes());
                input.push(0);
            }
            self.git_stdin(&idx, &["rm", "-r", "--cached", "-q", "--ignore-unmatch", "--pathspec-from-file=-", "--pathspec-file-nul"], &input)?;
        }

        // Hash only what changed since the last snapshot of this layer.
        let mut cache = self.hashes.borrow_mut();
        let mut todo: Vec<usize> = Vec::new();
        for (i, (_, abs, m)) in files.iter().enumerate() {
            let fresh = cache.get(abs).is_some_and(|c| Some(c.0) == m.modified().ok() && c.1 == m.len() && c.2 == m.mode());
            if !fresh {
                todo.push(i);
            }
        }
        let (links, regular): (Vec<usize>, Vec<usize>) = todo.into_iter().partition(|&i| files[i].2.file_type().is_symlink());
        if !regular.is_empty() {
            let mut input = String::new();
            for &i in &regular {
                input.push_str(&files[i].1.to_string_lossy());
                input.push('\n');
            }
            let out = self.git_stdin(&idx, &["hash-object", "-w", "--no-filters", "--stdin-paths"], input.as_bytes())?;
            for (&i, sha) in regular.iter().zip(out.lines()) {
                let m = &files[i].2;
                cache.insert(files[i].1.clone(), (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), m.mode(), sha.to_string()));
            }
        }
        for &i in &links {
            let target = std::fs::read_link(&files[i].1)?;
            let sha = self.git_stdin(&idx, &["hash-object", "-w", "--stdin"], target.to_string_lossy().as_bytes())?;
            let m = &files[i].2;
            cache.insert(files[i].1.clone(), (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), m.mode(), sha.trim().to_string()));
        }
        if !files.is_empty() {
            let mut info = Vec::new();
            for (rel, abs, m) in &files {
                let mode = if m.file_type().is_symlink() {
                    "120000"
                } else if m.mode() & 0o111 != 0 {
                    "100755"
                } else {
                    "100644"
                };
                let sha = &cache.get(abs).expect("hashed").3;
                info.extend_from_slice(format!("{mode} {sha}\t{rel}").as_bytes());
                info.push(0);
            }
            self.git_stdin(&idx, &["update-index", "-z", "--index-info"], &info)?;
        }
        let tree = self.git_with_index(&idx, &["write-tree"])?.trim().to_string();
        std::fs::rename(&idx, &self.index_path)?;
        Ok(tree)
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
    /// An engine whose world is a mounted layer. Same database and object store as the parent.
    pub fn open_layered(parent: &Engine, layer: &Layer, key: &str) -> Result<Engine> {
        Ok(Engine {
            store: Store::open(&parent.home.join("rune.db"))?,
            world: World::layered(&parent.world, layer, key)?,
            home: parent.home.clone(),
        })
    }
}
