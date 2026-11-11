//! In-process git object store: hashing, writing and reading blobs and trees without spawning git.
//!
//! Loose objects are written directly (zlib + SHA-1, atomic rename), the same bytes git itself writes.
//! Reads try the loose file first and fall back to one long-lived `git cat-file --batch` for packed
//! objects. Tree diffs walk only subtrees whose hashes differ.

use crate::world::Change;
use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use sha1::{Digest, Sha1};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: String,
    pub name: String,
    pub sha: String,
}

impl Entry {
    pub fn is_tree(&self) -> bool {
        self.mode == "40000"
    }
}

struct CatFile {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct Odb {
    git_dir: PathBuf,
    cat: RefCell<Option<CatFile>>,
    trees: RefCell<HashMap<String, std::rc::Rc<Vec<Entry>>>>,
}

pub fn object_id(kind: &str, data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("{kind} {}\0", data.len()).as_bytes());
    h.update(data);
    hex::encode(h.finalize())
}

impl Odb {
    pub fn new(git_dir: &Path) -> Self {
        Odb { git_dir: git_dir.to_path_buf(), cat: RefCell::new(None), trees: RefCell::new(HashMap::new()) }
    }

    fn loose_path(&self, sha: &str) -> PathBuf {
        self.git_dir.join("objects").join(&sha[..2]).join(&sha[2..])
    }

    pub fn write(&self, kind: &str, data: &[u8]) -> Result<String> {
        let sha = object_id(kind, data);
        if kind == "tree" && sha == EMPTY_TREE {
            return Ok(sha);
        }
        let path = self.loose_path(&sha);
        if path.exists() {
            return Ok(sha);
        }
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 64), flate2::Compression::fast());
        enc.write_all(format!("{kind} {}\0", data.len()).as_bytes())?;
        enc.write_all(data)?;
        let bytes = enc.finish()?;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = dir.join(format!("tmp_obj_{}_{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::write(&tmp, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444));
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            if !path.exists() {
                bail!("could not write object {sha}");
            }
        }
        Ok(sha)
    }

    pub fn write_blob_file(&self, path: &Path) -> Result<String> {
        let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        self.write("blob", &data)
    }

    /// (type, data)
    pub fn read(&self, sha: &str) -> Result<(String, Vec<u8>)> {
        if sha == EMPTY_TREE {
            return Ok(("tree".into(), vec![]));
        }
        if let Ok(f) = std::fs::File::open(self.loose_path(sha)) {
            let mut raw = Vec::new();
            ZlibDecoder::new(f).read_to_end(&mut raw)?;
            let nul = raw.iter().position(|&b| b == 0).context("corrupt object header")?;
            let header = String::from_utf8_lossy(&raw[..nul]).into_owned();
            let kind = header.split(' ').next().unwrap_or("").to_string();
            return Ok((kind, raw[nul + 1..].to_vec()));
        }
        self.read_packed(sha)
    }

    fn read_packed(&self, sha: &str) -> Result<(String, Vec<u8>)> {
        let mut slot = self.cat.borrow_mut();
        if slot.is_none() {
            let mut child = Command::new("git")
                .env("GIT_DIR", &self.git_dir)
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("spawn git cat-file")?;
            let stdin = child.stdin.take().unwrap();
            let stdout = BufReader::new(child.stdout.take().unwrap());
            *slot = Some(CatFile { _child: child, stdin, stdout });
        }
        let c = slot.as_mut().unwrap();
        writeln!(c.stdin, "{sha}")?;
        c.stdin.flush()?;
        let mut header = String::new();
        c.stdout.read_line(&mut header)?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() < 3 {
            bail!("object {sha} not found");
        }
        let size: usize = parts[2].parse()?;
        let mut data = vec![0; size + 1]; // + trailing newline
        c.stdout.read_exact(&mut data)?;
        data.truncate(size);
        Ok((parts[1].to_string(), data))
    }

    pub fn read_tree(&self, sha: &str) -> Result<std::rc::Rc<Vec<Entry>>> {
        if let Some(t) = self.trees.borrow().get(sha) {
            return Ok(t.clone());
        }
        let (kind, data) = self.read(sha)?;
        if kind != "tree" {
            bail!("{sha} is a {kind}, not a tree");
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let sp = data[i..].iter().position(|&b| b == b' ').context("bad tree")? + i;
            let nul = data[sp..].iter().position(|&b| b == 0).context("bad tree")? + sp;
            let mode = String::from_utf8_lossy(&data[i..sp]).into_owned();
            let name = String::from_utf8_lossy(&data[sp + 1..nul]).into_owned();
            let sha = hex::encode(&data[nul + 1..nul + 21]);
            out.push(Entry { mode, name, sha });
            i = nul + 21;
        }
        let rc = std::rc::Rc::new(out);
        self.trees.borrow_mut().insert(sha.to_string(), rc.clone());
        Ok(rc)
    }

    /// Write a tree in git's canonical order (directories sort as if their name ended in '/').
    pub fn write_tree(&self, entries: &mut [Entry]) -> Result<String> {
        entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        let mut data = Vec::new();
        for e in entries.iter() {
            data.extend_from_slice(e.mode.as_bytes());
            data.push(b' ');
            data.extend_from_slice(e.name.as_bytes());
            data.push(0);
            data.extend_from_slice(&hex::decode(&e.sha)?);
        }
        let sha = self.write("tree", &data)?;
        self.trees.borrow_mut().insert(sha.clone(), std::rc::Rc::new(entries.to_vec()));
        Ok(sha)
    }

    /// Changed files between two trees, sorted by path, with line counts (None for binary).
    pub fn diff(&self, a: &str, b: &str) -> Result<Vec<Change>> {
        let mut out = Vec::new();
        self.diff_into(Some(a), Some(b), "", &mut out)?;
        out.sort_by(|x, y| x.path.cmp(&y.path));
        Ok(out)
    }

    fn diff_into(&self, a: Option<&str>, b: Option<&str>, prefix: &str, out: &mut Vec<Change>) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let ea = match a {
            Some(s) => self.read_tree(s)?,
            None => Default::default(),
        };
        let eb = match b {
            Some(s) => self.read_tree(s)?,
            None => Default::default(),
        };
        let ma: HashMap<&str, &Entry> = ea.iter().map(|e| (e.name.as_str(), e)).collect();
        let mb: HashMap<&str, &Entry> = eb.iter().map(|e| (e.name.as_str(), e)).collect();
        let mut names: Vec<&str> = ma.keys().chain(mb.keys()).copied().collect();
        names.sort_unstable();
        names.dedup();
        for n in names {
            let path = if prefix.is_empty() { n.to_string() } else { format!("{prefix}/{n}") };
            match (ma.get(n), mb.get(n)) {
                (Some(x), Some(y)) if x.sha == y.sha && x.mode == y.mode => {}
                (Some(x), Some(y)) if x.is_tree() && y.is_tree() => self.diff_into(Some(&x.sha), Some(&y.sha), &path, out)?,
                (Some(x), Some(y)) if x.is_tree() != y.is_tree() => {
                    // file <-> directory: the old side disappears, the new side appears
                    self.side(x, &path, 'D', out)?;
                    self.side(y, &path, 'A', out)?;
                }
                (Some(x), Some(y)) => {
                    let (old, new) = (self.blob(&x.sha)?, self.blob(&y.sha)?);
                    let (added, deleted) = line_diff(&old, &new);
                    let status = if (x.mode == "120000") != (y.mode == "120000") { 'T' } else { 'M' };
                    out.push(Change { status, path, added, deleted });
                }
                (Some(x), None) => self.side(x, &path, 'D', out)?,
                (None, Some(y)) => self.side(y, &path, 'A', out)?,
                (None, None) => {}
            }
        }
        Ok(())
    }

    fn side(&self, e: &Entry, path: &str, status: char, out: &mut Vec<Change>) -> Result<()> {
        if e.is_tree() {
            let (a, b) = if status == 'D' { (Some(e.sha.as_str()), None) } else { (None, Some(e.sha.as_str())) };
            return self.diff_into(a, b, path, out);
        }
        if e.mode == "160000" {
            out.push(Change { status, path: path.into(), added: None, deleted: None });
            return Ok(());
        }
        let lines = count_lines(&self.blob(&e.sha)?);
        let (added, deleted) = if status == 'A' { (lines, Some(0)) } else { (Some(0), lines) };
        let (added, deleted) = if lines.is_none() { (None, None) } else { (added, deleted) };
        out.push(Change { status, path: path.into(), added, deleted });
        Ok(())
    }

    fn blob(&self, sha: &str) -> Result<Vec<u8>> {
        Ok(self.read(sha)?.1)
    }
}

fn sort_key(e: &Entry) -> Vec<u8> {
    let mut k = e.name.as_bytes().to_vec();
    if e.is_tree() {
        k.push(b'/');
    }
    k
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8000)].contains(&0)
}

fn count_lines(b: &[u8]) -> Option<u64> {
    if is_binary(b) {
        return None;
    }
    let n = b.iter().filter(|&&c| c == b'\n').count() as u64;
    Some(if !b.is_empty() && *b.last().unwrap() != b'\n' { n + 1 } else { n })
}

/// (added, deleted) lines, as git's numstat counts them; None for binary.
fn line_diff(old: &[u8], new: &[u8]) -> (Option<u64>, Option<u64>) {
    if is_binary(old) || is_binary(new) {
        return (None, None);
    }
    let (o, n) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
    let diff = similar::TextDiff::configure().algorithm(similar::Algorithm::Myers).diff_lines(&*o, &*n);
    let (mut a, mut d) = (0u64, 0u64);
    for op in diff.ops() {
        for c in diff.iter_changes(op) {
            match c.tag() {
                similar::ChangeTag::Insert => a += 1,
                similar::ChangeTag::Delete => d += 1,
                similar::ChangeTag::Equal => {}
            }
        }
    }
    (Some(a), Some(d))
}
