//! The shelf: local changes put aside as patches, outside `git stash`.
//!
//! A project's entries live in `state/shelf/<key>/` (`Paths::shelf_dir`,
//! [`root_dir`]): one `<id>.patch` per entry (the raw bytes `git apply`
//! reads, binary patches included) and `index.json` with the names. A patch
//! file is the source of truth: a row without one is dropped, a patch without
//! a row is listed as an orphan, so a lost index write (two processes on one
//! root) never loses a shelved change. `index.json` is kept as raw JSON, so a
//! row this build cannot read survives every rewrite, and a file that is not
//! valid JSON is never overwritten.
//!
//! The two jobs, [`shelve`] and [`unshelve`], run at the work-tree top (`git
//! apply` reads patch paths relative to its working directory, and the status
//! paths are relative to the top).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use super::cli::{self, CliOutput};
use super::model::Section;
use super::repo;
use crate::util::fs::write_atomic;

/// Version written to a new `index.json`.
pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShelfEntry {
    pub id: u64,
    pub name: String,
    /// Unix ms.
    pub time: u64,
    /// Paths relative to the work-tree top.
    pub files: Vec<String>,
}

/// One file of a whole-file shelve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShelveFile {
    /// Path relative to the work-tree top (the new path of a rename).
    pub rel: String,
    /// Every path of the change (a rename: old and new).
    pub paths: Vec<String>,
    pub untracked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShelveWhat {
    /// Whole files: every change against HEAD (staged and unstaged), untracked files whole.
    Files(Vec<ShelveFile>),
    /// A diff block or lines of one modified file: `stored` is the forward patch,
    /// `revert` the patch that takes it out of the work tree.
    Part { rel: String, stored: String, revert: String },
}

impl ShelveWhat {
    /// The name used when the dialog answer is empty.
    pub fn fallback_name(&self) -> String {
        let last = |rel: &str| rel.rsplit('/').next().unwrap_or(rel).to_string();
        match self {
            ShelveWhat::Files(list) => match list.as_slice() {
                [f] => format!("Changes to {}", last(&f.rel)),
                list => format!("Changes to {} files", list.len()),
            },
            ShelveWhat::Part { rel, .. } => format!("Part of {}", last(rel)),
        }
    }
}

/// Serializes the index read-modify-write between background tasks. The value
/// counts the states handed out: each [`list`], [`add`] and [`delete`] bumps it
/// under the guard, so a higher version saw a later state.
static FILE_LOCK: Mutex<u64> = Mutex::new(0);

fn lock() -> MutexGuard<'static, u64> {
    FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `base/<key>`, `<key>` being the 16 hex digits of the FNV-1a hash of the
/// root's path, as the session store keys it.
pub fn root_dir(base: &Path, root: &Path) -> PathBuf {
    base.join(format!("{:016x}", crate::workspace::fnv1a64(root.as_os_str().as_encoded_bytes())))
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn patch_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id}.patch"))
}

/// The index as it is on disk: its version, the lowest id never handed out
/// (so a deleted entry's id is not reused, which a patch tab keyed by it would
/// confuse) and its rows, unread.
struct Index {
    version: u64,
    next_id: u64,
    rows: Vec<serde_json::Value>,
    /// Top-level keys this build does not know (a newer build wrote them).
    extra: serde_json::Map<String, serde_json::Value>,
}

fn read_index(dir: &Path) -> Result<Index, String> {
    let file = index_path(dir);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Index { version: u64::from(FORMAT), next_id: 1, rows: Vec::new(), extra: serde_json::Map::new() });
        }
        Err(e) => return Err(format!("Cannot read {}: {e}", file.display())),
    };
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Cannot read {}: not valid JSON ({e})", file.display()))?;
    if !v.is_object() {
        return Err(format!("Cannot read {}: not valid JSON (not an object)", file.display()));
    }
    let version = v["version"].as_u64().unwrap_or(u64::from(FORMAT));
    let rows = match v.get("entries") {
        None => Vec::new(),
        Some(e) => e.as_array().cloned().ok_or_else(|| format!("Cannot read {}: entries is not a list", file.display()))?,
    };
    let extra = v
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "version" | "next_id" | "entries"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    Ok(Index { version, next_id: v["next_id"].as_u64().unwrap_or(1), rows, extra })
}

fn row_id(v: &serde_json::Value) -> Option<u64> {
    v.get("id")?.as_u64()
}

/// The numbers `n` of the `<n>.patch` files in `dir`.
fn patch_numbers(dir: &Path) -> Vec<u64> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name();
            name.to_str()?.strip_suffix(".patch")?.parse::<u64>().ok()
        })
        .collect()
}

/// Rows whose patch exists (a row without a number is kept untouched).
fn live_rows(dir: &Path, rows: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    rows.into_iter().filter(|r| row_id(r).is_none_or(|id| patch_path(dir, id).exists())).collect()
}

/// Write `rows` back; `next_id` rises to cover every row and patch.
fn write_index(dir: &Path, old: &Index, rows: &[serde_json::Value]) -> Result<(), String> {
    let next_id = rows.iter().filter_map(row_id).chain(patch_numbers(dir)).max().map_or(1, |m| m + 1).max(old.next_id);
    let mut doc = old.extra.clone();
    doc.insert("version".into(), old.version.max(u64::from(FORMAT)).into());
    doc.insert("next_id".into(), next_id.into());
    doc.insert("entries".into(), rows.into());
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    match write_atomic(&index_path(dir), &text) {
        Err(e) => Err(format!("Cannot write {}: {e}", index_path(dir).display())),
        Ok(perm) => {
            if let Some(p) = perm {
                tracing::warn!("shelf: permissions not copied: {p}");
            }
            Ok(())
        }
    }
}

fn list_locked(dir: &Path) -> Result<Vec<ShelfEntry>, String> {
    let rows = read_index(dir)?.rows;
    let known: Vec<u64> = rows.iter().filter_map(row_id).collect();
    let mut out: Vec<ShelfEntry> = rows
        .iter()
        .filter_map(|r| serde_json::from_value::<ShelfEntry>(r.clone()).ok())
        .filter(|e| patch_path(dir, e.id).exists())
        .collect();
    for n in patch_numbers(dir) {
        if known.contains(&n) {
            continue;
        }
        let path = patch_path(dir, n);
        let time = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        let files = std::fs::read(&path).map(|p| patch_files(&p)).unwrap_or_default();
        out.push(ShelfEntry { id: n, name: format!("Shelved changes {n}"), time, files });
    }
    out.sort_by_key(|e| std::cmp::Reverse((e.time, e.id)));
    Ok(out)
}

/// The entries, newest first, with the version taken under the lock.
pub fn list(dir: &Path) -> (u64, Result<Vec<ShelfEntry>, String>) {
    let mut guard = lock();
    *guard += 1;
    let version = *guard;
    (version, list_locked(dir))
}

/// Create the first free `<n>.patch` for `n >= first` and open it for writing.
/// `create_new` makes the claim atomic, so two adds never share a file.
fn create_patch(dir: &Path, first: u64) -> Result<(u64, std::fs::File), String> {
    let mut id = first;
    let mut tries = 0;
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(patch_path(dir, id)) {
            Ok(f) => return Ok((id, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 100 => {
                tries += 1;
                id += 1;
            }
            Err(e) => return Err(format!("Cannot write {}: {e}", patch_path(dir, id).display())),
        }
    }
}

/// Store `patch` as a new entry. Nothing is left behind on failure.
pub fn add(dir: &Path, name: &str, time: u64, files: &[String], patch: &[u8]) -> Result<ShelfEntry, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    // An unreadable index is refused before anything is written.
    let index = read_index(dir)?;
    let first = (1 + index.rows.iter().filter_map(row_id).chain(patch_numbers(dir)).max().unwrap_or(0)).max(index.next_id);
    let (id, mut file) = create_patch(dir, first)?;
    let undo = |msg: String| {
        let _ = std::fs::remove_file(patch_path(dir, id));
        Err(msg)
    };
    if let Err(e) = file.write_all(patch).and_then(|_| file.sync_all()) {
        return undo(format!("Cannot write {}: {e}", patch_path(dir, id).display()));
    }
    drop(file);
    let entry = ShelfEntry { id, name: name.to_string(), time, files: files.to_vec() };
    let mut guard = lock();
    *guard += 1;
    let index = match read_index(dir) {
        Ok(r) => r,
        Err(e) => return undo(e),
    };
    let mut rows = live_rows(dir, index.rows.clone());
    match serde_json::to_value(&entry) {
        Ok(v) => rows.push(v),
        Err(e) => return undo(e.to_string()),
    }
    if let Err(e) = write_index(dir, &index, &rows) {
        return undo(e);
    }
    Ok(entry)
}

/// Remove entry `id` (its patch, then its row) and list what is left.
pub fn delete(dir: &Path, id: u64) -> (u64, Result<Vec<ShelfEntry>, String>) {
    let mut guard = lock();
    *guard += 1;
    let version = *guard;
    if let Err(e) = std::fs::remove_file(patch_path(dir, id))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return (version, Err(format!("Cannot delete {}: {e}", patch_path(dir, id).display())));
    }
    // The patch is gone, so the entry is too; an unreadable index is left as it is.
    if let Ok(index) = read_index(dir)
        && index.rows.iter().any(|r| row_id(r) == Some(id) || row_id(r).is_some_and(|i| !patch_path(dir, i).exists()))
    {
        let rows: Vec<_> = live_rows(dir, index.rows.clone()).into_iter().filter(|r| row_id(r) != Some(id)).collect();
        if let Err(e) = write_index(dir, &index, &rows) {
            tracing::warn!("shelf: {e}");
        }
    }
    (version, list_locked(dir))
}

pub fn read_patch(dir: &Path, id: u64) -> Result<Vec<u8>, String> {
    std::fs::read(patch_path(dir, id)).map_err(|e| format!("Cannot read shelf entry {id}: {e}"))
}

/// Undo git's C-style quoting at the start of `s` (which begins with `"`):
/// the bytes and the rest after the closing quote.
fn unquote(s: &str) -> Option<(String, &str)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &s[i + 1..])),
            b'\\' => {
                i += 1;
                match *b.get(i)? {
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    c @ b'0'..=b'7' => {
                        let mut v = u32::from(c - b'0');
                        for _ in 0..2 {
                            match b.get(i + 1) {
                                Some(d @ b'0'..=b'7') => {
                                    v = v * 8 + u32::from(d - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        out.push(v as u8);
                    }
                    c => out.push(c),
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

/// The old and new path of a `diff --git` line's remainder.
fn diff_line_paths(rest: &str) -> Option<(String, String)> {
    let strip = |p: String, pre: &str| p.strip_prefix(pre).map(str::to_string).unwrap_or(p);
    if rest.starts_with('"') {
        let (a, tail) = unquote(rest)?;
        let tail = tail.trim_start();
        let b = if tail.starts_with('"') { unquote(tail)?.0 } else { tail.to_string() };
        return Some((strip(a, "a/"), strip(b, "b/")));
    }
    // `a/X b/X`: the halves are equal, so the middle space is exact.
    let n = rest.len();
    if n % 2 == 1 {
        let m = n / 2;
        if rest.is_char_boundary(m) && rest.as_bytes()[m] == b' ' && rest[..m].strip_prefix("a/") == rest[m + 1..].strip_prefix("b/") {
            let p = rest[..m].strip_prefix("a/")?;
            return Some((p.to_string(), p.to_string()));
        }
    }
    let (a, b) = rest.split_once(" b/")?;
    Some((a.strip_prefix("a/").unwrap_or(a).to_string(), b.to_string()))
}

/// The files a patch touches: the new path of every `diff --git` line (the
/// old one when the new is `/dev/null`), once each, in order.
pub fn patch_files(patch: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(patch);
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("diff --git ") else { continue };
        let Some((x, y)) = diff_line_paths(rest) else { continue };
        let f = if y == "/dev/null" { x } else { y };
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

/// Whether the patch has an `index <pre>..<post>` line (what `--3way` needs).
pub fn has_blob_ids(patch: &[u8]) -> bool {
    patch.split(|b| *b == b'\n').any(|l| l.starts_with(b"index ") && l.windows(2).any(|w| w == b".."))
}

/// `patch` with `index <pre>..<post>` after its first (`diff --git`) line.
/// A patch that already has an `index` line is returned as it is.
pub fn with_blob_ids(patch: &str, pre: &str, post: &str) -> String {
    if has_blob_ids(patch.as_bytes()) {
        return patch.to_string();
    }
    match patch.split_once('\n') {
        Some((first, rest)) => format!("{first}\nindex {pre}..{post}\n{rest}"),
        None => patch.to_string(),
    }
}

/// A backslash before each glob character of `path` (`git apply --include` takes a glob).
pub fn glob_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn run(top: &Path, args: &[String], input: Option<Vec<u8>>) -> Result<CliOutput, String> {
    cli::git_with_input(top, args, input).await.map_err(|e| format!("{e:#}"))
}

/// The work-tree top of `root`.
async fn toplevel(root: &Path) -> Result<PathBuf, String> {
    let out = run(root, &cli::args(&["rev-parse", "--show-toplevel"]), None).await?;
    if !out.ok {
        return Err(out.combined());
    }
    Ok(PathBuf::from(out.stdout_text().trim()))
}

/// The patch of whole files against HEAD: tracked ones together, untracked ones each.
async fn files_patch(top: &Path, list: &[ShelveFile]) -> Result<Vec<u8>, String> {
    if !run(top, &cli::args(&["rev-parse", "--verify", "-q", "HEAD"]), None).await?.ok {
        return Err("Cannot shelve before the first commit".into());
    }
    let mut patch = Vec::new();
    let tracked: Vec<String> = list.iter().filter(|f| !f.untracked).flat_map(|f| f.paths.iter().cloned()).collect();
    if !tracked.is_empty() {
        let out = run(top, &cli::shelf_diff_args(&tracked), None).await?;
        if !out.ok {
            return Err(out.combined());
        }
        patch.extend(out.stdout);
    }
    for f in list.iter().filter(|f| f.untracked) {
        let out = run(top, &cli::untracked_diff_args(&f.rel), None).await?;
        // `--no-index` exits 1 when the files differ.
        if out.stdout.is_empty() && !out.ok {
            return Err(out.combined());
        }
        patch.extend(out.stdout);
    }
    Ok(patch)
}

/// The patch of one hunk or lines, with the blob ids `--3way` needs: the index
/// blob as the preimage, the work-tree file as the postimage.
async fn part_patch(top: &Path, rel: &str, stored: &str) -> Result<Vec<u8>, String> {
    let pre = run(top, &["rev-parse".into(), "--verify".into(), format!(":{rel}")], None).await?;
    if !pre.ok {
        return Err(pre.combined());
    }
    let post = run(top, &["hash-object".into(), "--".into(), rel.into()], None).await?;
    if !post.ok {
        return Err(post.combined());
    }
    Ok(with_blob_ids(stored, pre.stdout_text().trim(), post.stdout_text().trim()).into_bytes())
}

/// Store `what` as entry `name`, then take it out of the work tree. The patch
/// is stored (and synced) first: a failed store reverts nothing.
pub async fn shelve(root: PathBuf, dir: PathBuf, name: String, what: ShelveWhat, time: u64) -> (bool, String) {
    let top = match toplevel(&root).await {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    let patch = match &what {
        ShelveWhat::Files(list) => files_patch(&top, list).await,
        ShelveWhat::Part { rel, stored, .. } => part_patch(&top, rel, stored).await,
    };
    let patch = match patch {
        Ok(p) => p,
        Err(e) => return (false, e),
    };
    if patch.is_empty() {
        return (false, "Nothing to shelve: no changes against HEAD in the selected files".into());
    }
    let files = patch_files(&patch);
    // What the revert below takes out must undo cleanly from the work tree: for
    // whole files the stored patch itself (else the discard would drop what it
    // does not hold, a textconv driver's text, say); for a part, the revert
    // patch (the stored one keeps unselected old lines, so it is not the work
    // tree's). Nothing is stored or reverted otherwise.
    let undo = match &what {
        ShelveWhat::Files(_) => patch.clone(),
        ShelveWhat::Part { revert, .. } => revert.clone().into_bytes(),
    };
    match run(&top, &cli::apply_check_reverse_args(), Some(undo)).await {
        Ok(o) if o.ok => {}
        Ok(o) => return (false, format!("Cannot shelve: the patch does not match the work tree, nothing changed: {}", o.combined())),
        Err(e) => return (false, format!("Cannot shelve: {e}")),
    }
    let stored = {
        let (dir, name, files) = (dir.clone(), name.clone(), files.clone());
        tokio::task::spawn_blocking(move || add(&dir, &name, time, &files, &patch)).await
    };
    match stored {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return (false, format!("Cannot shelve: {e}")),
        Err(e) => return (false, format!("Cannot shelve: {e}")),
    }
    let reverted: Result<(), String> = match what {
        ShelveWhat::Files(list) => {
            let items: Vec<(Section, String)> = list
                .iter()
                .flat_map(|f| {
                    let section = if f.untracked { Section::Untracked } else { Section::Staged };
                    let paths = if f.untracked { vec![f.rel.clone()] } else { f.paths.clone() };
                    paths.into_iter().map(move |p| (section, p))
                })
                .collect();
            let root = root.clone();
            match tokio::task::spawn_blocking(move || repo::discard(&root, &items)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            }
        }
        ShelveWhat::Part { revert, .. } => {
            let args = cli::args(&["apply", "--reverse", "--whitespace=nowarn", "-"]);
            match run(&top, &args, Some(revert.into_bytes())).await {
                Ok(o) if o.ok => Ok(()),
                Ok(o) => Err(o.combined()),
                Err(e) => Err(e),
            }
        }
    };
    match reverted {
        Ok(()) => {
            let n = files.len();
            (true, format!("Shelved '{name}' ({n} file{})", if n == 1 { "" } else { "s" }))
        }
        Err(e) => (false, format!("Shelved as '{name}', but reverting the work tree failed: {e}")),
    }
}

/// Apply entry `id` (or its `file`) to the work tree: a plain `git apply`
/// first, which leaves the index alone, then `git apply --3way` when the
/// context no longer matches. The entry is always kept.
pub async fn unshelve(root: PathBuf, dir: PathBuf, id: u64, name: String, file: Option<String>) -> (bool, String) {
    let top = match toplevel(&root).await {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    let patch = match tokio::task::spawn_blocking(move || read_patch(&dir, id)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return (false, e),
        Err(e) => return (false, e.to_string()),
    };
    let include = file.as_deref().map(glob_escape);
    let done = match &file {
        Some(f) => format!("Unshelved {f} from '{name}'"),
        None => format!("Unshelved '{name}'"),
    };
    let plain = match run(&top, &cli::unshelve_args(false, include.as_deref()), Some(patch.clone())).await {
        Ok(o) if o.ok => return (true, done),
        Ok(o) => o.combined(),
        Err(e) => e,
    };
    if !has_blob_ids(&patch) {
        return (false, plain);
    }
    match run(&top, &cli::unshelve_args(true, include.as_deref()), Some(patch)).await {
        Ok(o) if o.ok => (true, format!("{done}; 3-way merged, the result is staged")),
        Ok(o) => (false, o.combined()),
        Err(e) => (false, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("jetl-shelf-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn files(f: &[&str]) -> Vec<String> {
        f.iter().map(|s| s.to_string()).collect()
    }

    fn listed(dir: &Path) -> Vec<ShelfEntry> {
        list(dir).1.unwrap()
    }

    #[test]
    fn add_list_delete_round_trip() {
        let t = Tmp::new("round");
        assert_eq!(listed(&t.0), vec![]);
        let a = add(&t.0, "first", 100, &files(&["a.txt"]), b"patch a").unwrap();
        let b = add(&t.0, "second", 200, &files(&["b.txt", "c.txt"]), b"patch b").unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert_eq!(read_patch(&t.0, 2).unwrap(), b"patch b");
        assert_eq!(listed(&t.0), vec![b.clone(), a.clone()]);
        let index: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(t.0.join("index.json")).unwrap()).unwrap();
        assert_eq!(index["version"], 1);
        assert_eq!(index["entries"][0]["name"], "first");
        assert_eq!(index["entries"][1]["files"], serde_json::json!(["b.txt", "c.txt"]));
        let (_, left) = delete(&t.0, 2);
        assert_eq!(left.unwrap(), vec![a.clone()]);
        assert!(!t.0.join("2.patch").exists());
        let (_, left) = delete(&t.0, 1);
        assert_eq!(left.unwrap(), vec![]);
        assert!(read_patch(&t.0, 1).unwrap_err().starts_with("Cannot read shelf entry 1: "));
    }

    #[test]
    fn ids_never_reuse_a_patch_number() {
        let t = Tmp::new("ids");
        add(&t.0, "one", 1, &[], b"1").unwrap();
        let two = add(&t.0, "two", 2, &[], b"2").unwrap();
        assert_eq!(two.id, 2);
        let _ = delete(&t.0, 2);
        // The index remembers 2 was handed out, though nothing holds it now.
        assert_eq!(add(&t.0, "again", 3, &[], b"3").unwrap().id, 3);
        std::fs::write(t.0.join("7.patch"), b"orphan").unwrap();
        assert_eq!(add(&t.0, "after orphan", 4, &[], b"4").unwrap().id, 8);
        // A row in the index counts too, even when its id is above every patch.
        let v = serde_json::json!({"version": 1, "entries": [{"id": 20, "name": "x", "time": 1, "files": []}]});
        let t2 = Tmp::new("ids2");
        std::fs::create_dir_all(&t2.0).unwrap();
        std::fs::write(t2.0.join("20.patch"), b"p").unwrap();
        std::fs::write(t2.0.join("index.json"), v.to_string()).unwrap();
        assert_eq!(add(&t2.0, "next", 5, &[], b"5").unwrap().id, 21);
    }

    #[test]
    fn orphan_patch_is_listed_and_dangling_row_dropped() {
        let t = Tmp::new("orphan");
        add(&t.0, "kept", 10, &files(&["k.txt"]), b"k").unwrap();
        let dangling = add(&t.0, "dangling", 20, &[], b"d").unwrap();
        std::fs::remove_file(t.0.join(format!("{}.patch", dangling.id))).unwrap();
        let patch = "diff --git a/x.txt b/x.txt\n--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-a\n+b\n";
        std::fs::write(t.0.join("9.patch"), patch).unwrap();
        let got = listed(&t.0);
        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"kept") && names.contains(&"Shelved changes 9") && !names.contains(&"dangling"), "{names:?}");
        let orphan = got.iter().find(|e| e.id == 9).unwrap();
        assert_eq!(orphan.files, files(&["x.txt"]));
        assert!(orphan.time > 0);
        // The next rewrite drops the dangling row.
        add(&t.0, "new", 30, &[], b"n").unwrap();
        let index = std::fs::read_to_string(t.0.join("index.json")).unwrap();
        assert!(!index.contains("dangling"), "{index}");
    }

    #[test]
    fn invalid_index_is_never_overwritten() {
        let t = Tmp::new("invalid");
        std::fs::create_dir_all(&t.0).unwrap();
        std::fs::write(t.0.join("index.json"), "{").unwrap();
        let err = add(&t.0, "x", 1, &[], b"p").unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        assert_eq!(std::fs::read_to_string(t.0.join("index.json")).unwrap(), "{");
        assert_eq!(patch_numbers(&t.0), Vec::<u64>::new());
        assert!(list(&t.0).1.unwrap_err().starts_with("Cannot read "));
        // Delete removes the patch and leaves the broken index alone.
        std::fs::write(t.0.join("3.patch"), b"p").unwrap();
        let (_, left) = delete(&t.0, 3);
        assert!(left.is_err());
        assert!(!t.0.join("3.patch").exists());
        assert_eq!(std::fs::read_to_string(t.0.join("index.json")).unwrap(), "{");
    }

    #[test]
    fn unknown_rows_survive_a_rewrite() {
        let t = Tmp::new("unknown");
        std::fs::create_dir_all(&t.0).unwrap();
        std::fs::write(t.0.join("9.patch"), b"p").unwrap();
        std::fs::write(t.0.join("index.json"), r#"{"version":2,"entries":[{"id":9,"future":true}]}"#).unwrap();
        let added = add(&t.0, "new", 1, &[], b"n").unwrap();
        assert_eq!(added.id, 10);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(t.0.join("index.json")).unwrap()).unwrap();
        assert_eq!(v["version"], 2);
        assert_eq!(v["entries"][0], serde_json::json!({"id": 9, "future": true}));
        assert_eq!(v["entries"][1]["name"], "new");
        // The row this build cannot read is not listed as an entry, and its patch is not an orphan.
        assert_eq!(listed(&t.0).iter().map(|e| e.id).collect::<Vec<_>>(), vec![10]);
        let _ = delete(&t.0, 10);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(t.0.join("index.json")).unwrap()).unwrap();
        assert_eq!(v["entries"], serde_json::json!([{"id": 9, "future": true}]));
    }

    #[test]
    fn two_adds_keep_each_others_entries() {
        let t = Tmp::new("two");
        let dir = t.0.clone();
        let run = |tag: &'static str, dir: PathBuf| {
            std::thread::spawn(move || {
                for i in 0..20 {
                    add(&dir, &format!("{tag}{i}"), i, &[], tag.as_bytes()).unwrap();
                }
            })
        };
        let (a, b) = (run("a", dir.clone()), run("b", dir));
        a.join().unwrap();
        b.join().unwrap();
        let got = listed(&t.0);
        assert_eq!(got.len(), 40);
        let mut ids: Vec<u64> = got.iter().map(|e| e.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 40);
        // Each patch still holds its own writer's bytes.
        for e in &got {
            assert_eq!(read_patch(&t.0, e.id).unwrap(), &e.name.as_bytes()[..1]);
        }
    }

    #[test]
    fn newest_first() {
        let t = Tmp::new("order");
        add(&t.0, "old", 100, &[], b"1").unwrap();
        add(&t.0, "new", 300, &[], b"2").unwrap();
        add(&t.0, "mid", 200, &[], b"3").unwrap();
        add(&t.0, "mid too", 200, &[], b"4").unwrap();
        let names: Vec<String> = listed(&t.0).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["new", "mid too", "mid", "old"]);
    }

    #[test]
    fn versions_only_grow() {
        let t = Tmp::new("version");
        let (a, _) = list(&t.0);
        let (b, _) = delete(&t.0, 1);
        let (c, _) = list(&t.0);
        assert!(a < b && b < c);
    }

    #[test]
    fn root_dir_is_keyed_like_sessions() {
        let root = Path::new("/home/me/proj");
        let key = format!("{:016x}", crate::workspace::fnv1a64(b"/home/me/proj"));
        assert_eq!(root_dir(Path::new("/state/shelf"), root), PathBuf::from("/state/shelf").join(&key));
        assert_eq!(key.len(), 16);
        assert_ne!(root_dir(Path::new("/s"), root), root_dir(Path::new("/s"), Path::new("/home/me/other")));
    }

    #[test]
    fn glob_escape_escapes_glob_chars() {
        assert_eq!(glob_escape("plain/a.txt"), "plain/a.txt");
        assert_eq!(glob_escape("a*b?c[d]\\e"), "a\\*b\\?c\\[d]\\\\e");
    }

    #[test]
    fn with_blob_ids_inserts_after_diff_line() {
        let p = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(with_blob_ids(p, "aaa", "bbb"), "diff --git a/x b/x\nindex aaa..bbb\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n");
        let has = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n";
        assert_eq!(with_blob_ids(has, "aaa", "bbb"), has);
    }

    #[test]
    fn has_blob_ids_needs_a_range() {
        assert!(has_blob_ids(b"diff --git a/x b/x\nindex 1234..5678 100644\n"));
        assert!(!has_blob_ids(b"diff --git a/x b/x\n--- a/x\n"));
        assert!(!has_blob_ids(b"diff --git a/x b/x\n+index 1..2\n"));
    }

    #[test]
    fn patch_files_reads_diff_lines() {
        let plain =
            b"diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/src/a.rs b/src/a.rs\n";
        assert_eq!(patch_files(plain), files(&["src/a.rs"]));
        let rename = b"diff --git a/old name.txt b/new name.txt\nsimilarity index 100%\nrename from old name.txt\nrename to new name.txt\n";
        assert_eq!(patch_files(rename), files(&["new name.txt"]));
        let added = b"diff --git a/n.txt b/n.txt\nnew file mode 100644\n";
        assert_eq!(patch_files(added), files(&["n.txt"]));
        let spaced = b"diff --git a/with space.txt b/with space.txt\n";
        assert_eq!(patch_files(spaced), files(&["with space.txt"]));
        let quoted = "diff --git \"a/tab\\there.txt\" \"b/tab\\there.txt\"\ndiff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\ndiff --git \"a/q\\\"uote\" \"b/q\\\"uote\"\n";
        assert_eq!(patch_files(quoted.as_bytes()), files(&["tab\there.txt", "caf\u{e9}.txt", "q\"uote"]));
        let gone = b"diff --git a/x b//dev/null\n";
        assert_eq!(patch_files(gone), files(&["x"]));
    }

    #[test]
    fn create_patch_never_takes_an_existing_file() {
        let t = Tmp::new("create");
        std::fs::create_dir_all(&t.0).unwrap();
        std::fs::write(t.0.join("5.patch"), b"old bytes").unwrap();
        let (id, mut f) = create_patch(&t.0, 5).unwrap();
        assert_eq!(id, 6);
        f.write_all(b"new").unwrap();
        drop(f);
        assert_eq!(std::fs::read(t.0.join("5.patch")).unwrap(), b"old bytes");
        assert_eq!(std::fs::read(t.0.join("6.patch")).unwrap(), b"new");
    }

    #[test]
    fn unknown_top_level_keys_survive_a_rewrite() {
        let t = Tmp::new("topkeys");
        std::fs::create_dir_all(&t.0).unwrap();
        std::fs::write(t.0.join("index.json"), r#"{"version":2,"entries":[],"future":{"a":[1,2]}}"#).unwrap();
        let e = add(&t.0, "new", 1, &[], b"n").unwrap();
        let read = || -> serde_json::Value { serde_json::from_str(&std::fs::read_to_string(t.0.join("index.json")).unwrap()).unwrap() };
        assert_eq!(read()["future"], serde_json::json!({"a": [1, 2]}));
        let _ = delete(&t.0, e.id);
        assert_eq!(read()["future"], serde_json::json!({"a": [1, 2]}));
        assert_eq!(read()["version"], 2);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repo with a textconv driver (repo-local config) for `*.bin`, one commit of `t.bin` and `plain.txt`.
    fn textconv_repo(name: &str) -> (Tmp, PathBuf) {
        let t = Tmp::new(name);
        let repo = t.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q"]);
        git_in(&repo, &["config", "user.name", "t"]);
        git_in(&repo, &["config", "user.email", "t@example.com"]);
        git_in(&repo, &["config", "diff.conv.textconv", "tr a-z A-Z <"]);
        std::fs::write(repo.join(".gitattributes"), "*.bin diff=conv\n").unwrap();
        std::fs::write(repo.join("t.bin"), b"alpha\nbeta\n").unwrap();
        std::fs::write(repo.join("plain.txt"), b"one\n").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-qm", "init"]);
        (t, repo)
    }

    fn file(rel: &str, untracked: bool) -> ShelveFile {
        ShelveFile { rel: rel.into(), paths: vec![rel.into()], untracked }
    }

    #[tokio::test]
    async fn textconv_files_shelve_and_unshelve_byte_for_byte() {
        let (t, repo) = textconv_repo("textconv");
        let dir = t.0.join("shelf");
        let tracked = b"alpha\nBETA changed\n\x01\xff tail\n".to_vec();
        // A NUL makes git treat the untracked file as binary: a `GIT binary patch` hunk.
        let untracked = b"gamma\0delta\n".to_vec();
        std::fs::write(repo.join("t.bin"), &tracked).unwrap();
        std::fs::write(repo.join("new.bin"), &untracked).unwrap();
        let what = ShelveWhat::Files(vec![file("t.bin", false), file("new.bin", true)]);
        let (ok, msg) = shelve(repo.clone(), dir.clone(), "tc".into(), what, 1).await;
        assert!(ok, "{msg}");
        assert_eq!(std::fs::read(repo.join("t.bin")).unwrap(), b"alpha\nbeta\n");
        assert!(!repo.join("new.bin").exists());
        let stored = String::from_utf8_lossy(&std::fs::read(dir.join("1.patch")).unwrap()).into_owned();
        assert!(stored.contains("GIT binary patch"), "{stored}");
        let (ok, msg) = unshelve(repo.clone(), dir, 1, "tc".into(), None).await;
        assert!(ok, "{msg}");
        assert_eq!(std::fs::read(repo.join("t.bin")).unwrap(), tracked);
        assert_eq!(std::fs::read(repo.join("new.bin")).unwrap(), untracked);
    }

    #[tokio::test]
    async fn a_line_selection_shelves_its_lines_only() {
        let (t, repo) = textconv_repo("part-lines");
        let dir = t.0.join("shelf");
        std::fs::write(repo.join("p.txt"), b"a\n").unwrap();
        git_in(&repo, &["add", "p.txt"]);
        git_in(&repo, &["commit", "-qm", "p"]);
        std::fs::write(repo.join("p.txt"), b"b\n").unwrap();
        // Only `+b` selected: the stored patch keeps the unselected `a` as context,
        // so it does not reverse-apply to the work tree; the revert does.
        let stored = "diff --git a/p.txt b/p.txt\n--- a/p.txt\n+++ b/p.txt\n@@ -1 +1,2 @@\n a\n+b\n".to_string();
        let revert = "diff --git a/p.txt b/p.txt\n--- a/p.txt\n+++ b/p.txt\n@@ -0,0 +1 @@\n+b\n".to_string();
        let what = ShelveWhat::Part { rel: "p.txt".into(), stored, revert };
        let (ok, msg) = shelve(repo.clone(), dir.clone(), "part".into(), what, 1).await;
        assert!(ok, "{msg}");
        assert_eq!(std::fs::read(repo.join("p.txt")).unwrap(), b"");
        assert_eq!(patch_numbers(&dir).len(), 1);
    }

    #[tokio::test]
    async fn a_patch_that_does_not_undo_reverts_nothing_and_stores_nothing() {
        let (t, repo) = textconv_repo("verify");
        let dir = t.0.join("shelf");
        std::fs::write(repo.join("plain.txt"), b"one\ntwo\n").unwrap();
        git_in(&repo, &["add", "plain.txt"]);
        // The stored patch claims a line the work tree does not hold.
        let stored = "diff --git a/plain.txt b/plain.txt\n--- a/plain.txt\n+++ b/plain.txt\n@@ -1 +1,2 @@\n one\n+other\n".to_string();
        let revert = stored.clone();
        let what = ShelveWhat::Part { rel: "plain.txt".into(), stored, revert };
        let (ok, msg) = shelve(repo.clone(), dir.clone(), "bad".into(), what, 1).await;
        assert!(!ok);
        assert!(msg.contains("nothing changed"), "{msg}");
        assert_eq!(std::fs::read(repo.join("plain.txt")).unwrap(), b"one\ntwo\n");
        assert_eq!(patch_numbers(&dir), Vec::<u64>::new());
        assert_eq!(listed(&dir), vec![]);
    }
}
