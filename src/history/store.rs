//! The local history store on disk: pure filesystem functions, run by the
//! history worker (`history::Handle`), never on the main thread.
//!
//! ```text
//! history/blobs/<id[0..2]>/<id>        raw bytes
//! history/blobs-zlib/<id[0..2]>/<id>   the bytes as a zlib stream
//! history/index/<fnv1a64(key):016x>.jsonl   one `Entry` per line, oldest first
//! history/prune.lock
//! ```
//!
//! A file's key is its canonical path ([`key`]), so a file and every
//! symlink to it (or to a directory above it) share one history; the caller
//! keeps the path it asked with for display. A key recorded before keys were
//! canonical (an older jetl stored the path as opened) is moved under its
//! canonical key by the next [`prune`], which runs when jetl starts; until
//! then [`list`] also reads the entries stored under the path as given. The
//! line format is unchanged (`v` stays 1), so an older jetl reads the index
//! and finds a file's history when it opens the canonical path.
//!
//! Blobs are shared by every entry with the same bytes (any file). Every
//! index line names its path, so two paths whose hashes collide share a
//! file and are told apart when reading. Several jetl instances may write
//! at once: blobs go through a temp file named after the id and the pid,
//! then a rename (idempotent); an index line is one `write_all` on an
//! `O_APPEND` file; prune holds `prune.lock` and deletes an unreferenced
//! blob only once it is older than [`BLOB_GRACE`].
//!
//! A blob's id is the git blob sha1 of its raw bytes, whichever way it is
//! stored. A blob of [`MIN_COMPRESS`] bytes and up is stored compressed
//! (under `blobs-zlib/`) when that is smaller, otherwise raw (under
//! `blobs/`, where every blob an older jetl wrote is too); [`read_blob`]
//! tries the compressed file first. An older jetl never looks in
//! `blobs-zlib/`: it cannot show those snapshots (a "No such file" error)
//! and never deletes them. [`prune`] counts the bytes on disk.
//!
//! Autosaves come in runs (idle autosave may write every few seconds), so
//! an autosave whose file's last entry is an autosave of the same run (the
//! run's first one taken less than [`AUTOSAVE_RUN`] ago) replaces that
//! entry instead of adding one: [`record`] rewrites the line in place
//! (temp file and rename) while holding `prune.lock`, or appends when
//! another instance holds it. The replaced entry's blob is collected by the
//! next [`prune`] unless another entry still names it. The window is
//! measured from the run's first autosave (the entry's `since`), so a long
//! editing session still leaves one entry per window.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// An unreferenced blob younger than this is kept: another instance may
/// have written it and not yet appended its index line.
pub const BLOB_GRACE: Duration = Duration::from_secs(10 * 60);

/// A blob shorter than this is stored raw: the zlib header, checksum and
/// block header rarely pay off.
const MIN_COMPRESS: usize = 64;

/// The deflate level (miniz_oxide's default).
const LEVEL: u8 = 6;

/// A sanity cap on an inflated blob (a damaged stream); snapshots are at
/// most `max_file_kb`.
const MAX_INFLATED: usize = 1 << 30;

/// A prune lock older than this is taken to be left by a dead instance.
pub const LOCK_STALE: Duration = Duration::from_secs(60 * 60);

/// How long a prune waits before trying a held lock once more: a
/// [`record`] merging an autosave holds it for a moment.
const LOCK_RETRY: Duration = Duration::from_millis(50);

/// Autosaves of one file within this long of the first autosave of their
/// run are merged into one entry (see the module doc).
pub const AUTOSAVE_RUN: Duration = Duration::from_secs(5 * 60);

/// What a snapshot was taken for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An explicit save (Ctrl+S, Save All, closing, quitting): the text written.
    Save,
    /// An autosave: the text written.
    Autosave,
    /// Save As: the text written to the new path.
    SaveAs,
    /// The file Save As was about to overwrite.
    BeforeSaveAs,
    /// The file changed on disk that an Overwrite answer replaced.
    BeforeOverwrite,
    /// The tab's text before a reload from disk replaced it.
    BeforeReload,
    /// A file deleted from the Project tree.
    BeforeDelete,
    /// A changed file before a git operation that rewrites the work tree;
    /// the detail is the operation's label.
    BeforeGit,
}

impl Kind {
    /// The row label: "Saved", "Before git pull --ff-only".
    pub fn label(self, detail: Option<&str>) -> String {
        match self {
            Kind::Save => "Saved".into(),
            Kind::Autosave => "Autosaved".into(),
            Kind::SaveAs => "Saved As".into(),
            Kind::BeforeSaveAs => "Before Save As overwrote it".into(),
            Kind::BeforeOverwrite => "Before overwrite".into(),
            Kind::BeforeReload => "Before reload".into(),
            Kind::BeforeDelete => "Before delete".into(),
            Kind::BeforeGit => match detail {
                Some(d) if !d.is_empty() => format!("Before {d}"),
                _ => "Before git".into(),
            },
        }
    }
}

/// One index line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    pub path: String,
    /// Unix milliseconds.
    pub at: u64,
    pub blob: String,
    /// Bytes.
    pub len: u64,
    pub lines: u64,
    pub kind: Kind,
    pub detail: Option<String>,
    /// An autosave that replaced others of its run: when the run's first
    /// one was taken (Unix ms). Absent from the line otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

impl Entry {
    pub fn label(&self) -> String {
        self.kind.label(self.detail.as_deref())
    }

    /// When the autosave run this entry stands for began.
    fn run_start(&self) -> u64 {
        self.since.unwrap_or(self.at)
    }
}

/// Why a snapshot was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    TooLarge,
    Binary,
    /// A file of the store itself.
    Internal,
    /// Index lines are JSON strings.
    NotUtf8Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    Skipped(Skip),
    /// The bytes are those of the file's last entry already.
    Same,
    Added(Entry),
    /// An autosave that replaced the file's last entry, an autosave of the
    /// same run.
    Replaced(Entry),
}

/// What a prune did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Another instance holds the lock; nothing was done.
    pub locked: bool,
    pub entries: usize,
    pub blobs: usize,
    /// Entries moved under their canonical key ([`key`]).
    pub moved: usize,
    /// The bytes on disk of the blobs still referenced, after the prune.
    pub bytes: u64,
}

fn blobs_dir(dir: &Path) -> PathBuf {
    dir.join("blobs")
}

fn zblobs_dir(dir: &Path) -> PathBuf {
    dir.join("blobs-zlib")
}

fn index_dir(dir: &Path) -> PathBuf {
    dir.join("index")
}

/// FNV-1a, 64 bits: stable across builds and platforms (unlike `DefaultHasher`).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The index file of key `key` (see [`key`]).
pub fn index_file(dir: &Path, key: &str) -> PathBuf {
    index_dir(dir).join(format!("{:016x}.jsonl", fnv1a64(key.as_bytes())))
}

/// Symlinks followed at most when resolving a missing path (as `SYMLOOP_MAX`).
const MAX_LINKS: u32 = 40;

/// The key of `path` in the store: its canonical path, every symlink
/// resolved. A missing file (deleted, or not yet written) still gets the
/// key it had while it existed: a dangling symlink is followed to its
/// target, and otherwise the nearest existing ancestor is canonicalized and
/// the rest joined on. Touches the filesystem: call it off the main thread.
pub fn key(path: &Path) -> PathBuf {
    resolve(path, MAX_LINKS)
}

fn resolve(path: &Path, links: u32) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    let is_link = links > 0 && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if let Some(target) = is_link.then(|| std::fs::read_link(path).ok()).flatten() {
        // An absolute target replaces the parent.
        let target = match path.parent() {
            Some(p) => p.join(target),
            None => target,
        };
        return resolve(&target, links - 1);
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => resolve(parent, links).join(name),
        _ => path.to_path_buf(),
    }
}

/// A blob id is 40 lowercase hex digits; anything else never names a file.
fn valid_id(id: &str) -> bool {
    id.len() == 40 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn blob_file(dir: &Path, id: &str) -> PathBuf {
    blobs_dir(dir).join(&id[..2]).join(id)
}

fn zblob_file(dir: &Path, id: &str) -> PathBuf {
    zblobs_dir(dir).join(&id[..2]).join(id)
}

/// `bytes` as a zlib stream, when that is smaller and they are at least
/// [`MIN_COMPRESS`] long.
fn compress(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < MIN_COMPRESS {
        return None;
    }
    let z = miniz_oxide::deflate::compress_to_vec_zlib(bytes, LEVEL);
    (z.len() < bytes.len()).then_some(z)
}

/// The bytes of zlib stream `z`; a damaged stream is an `InvalidData` error.
fn inflate(z: &[u8]) -> io::Result<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(z, MAX_INFLATED)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("the stored snapshot is damaged ({e})")))
}

/// The bytes blob `id` takes on disk: both copies when there are two, 0
/// when there is none (or `id` is not one).
fn stored_len(dir: &Path, id: &str) -> u64 {
    if !valid_id(id) {
        return 0;
    }
    [zblob_file(dir, id), blob_file(dir, id)].iter().map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)).sum()
}

/// The id of `bytes`: their git blob sha1.
pub fn blob_id(bytes: &[u8]) -> io::Result<String> {
    git2::Oid::hash_object(git2::ObjectType::Blob, bytes).map(|o| o.to_string()).map_err(io::Error::other)
}

/// Lines as ropey counts them, minus the empty last one: "a\nb" and
/// "a\nb\n" both have 2.
pub fn count_lines(bytes: &[u8]) -> u64 {
    let breaks = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    breaks + u64::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

/// The entries of `path` in the index file, oldest first; corrupt lines and
/// lines of other paths (a hash collision) are left out.
fn read_entries(file: &Path, path: &str) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    text.lines().filter_map(|l| serde_json::from_str::<Entry>(l).ok()).filter(|e| e.path == path).collect()
}

/// The entries of `key`, oldest first, with those still stored under
/// `given` (the path as opened, when it differs: an index not yet
/// migrated by [`prune`]).
fn entries_of(dir: &Path, key: &str, given: Option<&str>) -> Vec<Entry> {
    let mut v = read_entries(&index_file(dir, key), key);
    if let Some(given) = given.filter(|g| *g != key) {
        v.extend(read_entries(&index_file(dir, given), given));
        // Stable: entries at the same time keep their file order.
        v.sort_by_key(|e| e.at);
    }
    v
}

/// Snapshot `bytes` as the content of `path` (stored under its [`key`]) at
/// `at_ms`, unless it is too large (over `max_bytes`), binary, a file of
/// the store, or the same as the file's last entry. An autosave replaces
/// the last entry when that is an autosave of the same run (see the module
/// doc).
pub fn record(
    dir: &Path,
    path: &Path,
    bytes: &[u8],
    kind: Kind,
    detail: Option<String>,
    at_ms: u64,
    max_bytes: u64,
) -> io::Result<Recorded> {
    if bytes.len() as u64 > max_bytes {
        return Ok(Recorded::Skipped(Skip::TooLarge));
    }
    if crate::git::diff::is_binary(bytes) {
        return Ok(Recorded::Skipped(Skip::Binary));
    }
    let canonical = key(path);
    if path.starts_with(dir) || canonical.starts_with(key(dir)) {
        return Ok(Recorded::Skipped(Skip::Internal));
    }
    let Some(key) = canonical.to_str() else { return Ok(Recorded::Skipped(Skip::NotUtf8Path)) };
    let id = blob_id(bytes)?;
    let index = index_file(dir, key);
    let entries = entries_of(dir, key, path.to_str());
    let last = entries.last();
    if last.is_some_and(|e| e.blob == id) {
        return Ok(Recorded::Same);
    }
    write_blob(dir, &id, bytes)?;
    let mut entry = Entry {
        v: 1,
        path: key.to_string(),
        at: at_ms,
        blob: id,
        len: bytes.len() as u64,
        lines: count_lines(bytes),
        kind,
        detail,
        since: None,
    };
    // An entry still under the path as given (not yet migrated) is left be.
    let run = last.filter(|p| {
        kind == Kind::Autosave
            && p.kind == Kind::Autosave
            && p.path == key
            && at_ms.saturating_sub(p.run_start()) < AUTOSAVE_RUN.as_millis() as u64
    });
    if let Some(prev) = run {
        entry.since = Some(prev.run_start());
        if replace_last(dir, &index, prev, &entry)? {
            return Ok(Recorded::Replaced(entry));
        }
        entry.since = None;
    }
    let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
    line.push('\n');
    std::fs::create_dir_all(index_dir(dir))?;
    // One write of one short line on an append-only file: lines of two
    // instances never interleave.
    std::fs::OpenOptions::new().create(true).append(true).open(&index)?.write_all(line.as_bytes())?;
    Ok(Recorded::Added(entry))
}

/// Replace `prev`, the last line of its path in index file `index`, by
/// `entry`, holding `prune.lock` so that no prune rewrites the file
/// meanwhile; other lines stay as they are. `false`, with nothing written,
/// when another instance holds the lock or `prev` is no longer the path's
/// last line.
fn replace_last(dir: &Path, index: &Path, prev: &Entry, entry: &Entry) -> io::Result<bool> {
    let Ok(Some(_lock)) = PruneLock::take(dir) else { return Ok(false) };
    let Ok(text) = std::fs::read_to_string(index) else { return Ok(false) };
    let mut lines: Vec<&str> = text.lines().collect();
    let parse = |l: &str| serde_json::from_str::<Entry>(l).ok().filter(|e| e.path == prev.path);
    let Some(i) = lines.iter().rposition(|l| parse(l).is_some()) else { return Ok(false) };
    if parse(lines[i]).as_ref() != Some(prev) {
        return Ok(false);
    }
    let line = serde_json::to_string(entry).map_err(io::Error::other)?;
    lines[i] = &line;
    let mut text = lines.join("\n");
    text.push('\n');
    write_index(index, &text)?;
    Ok(true)
}

/// Write blob `id` (compressed when that is smaller) unless it exists in
/// either form; an existing one gets a fresh mtime so a prune in another
/// instance does not take it for an old orphan.
fn write_blob(dir: &Path, id: &str, bytes: &[u8]) -> io::Result<()> {
    for file in [zblob_file(dir, id), blob_file(dir, id)] {
        if file.is_file() {
            if let Ok(f) = std::fs::File::options().write(true).open(&file) {
                let _ = f.set_modified(SystemTime::now());
            }
            return Ok(());
        }
    }
    let (file, data) = match compress(bytes) {
        Some(z) => (zblob_file(dir, id), Cow::Owned(z)),
        None => (blob_file(dir, id), Cow::Borrowed(bytes)),
    };
    let parent = file.parent().unwrap_or(dir);
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{id}.{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&data)?;
    }
    std::fs::rename(&tmp, &file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The snapshots of `path` (by its [`key`]), newest first. Their `path`
/// is the key, not `path`: show the path asked with.
pub fn list(dir: &Path, path: &Path) -> Vec<Entry> {
    let canonical = key(path);
    let Some(key) = canonical.to_str() else { return Vec::new() };
    let mut v = entries_of(dir, key, path.to_str());
    v.reverse();
    v
}

/// The bytes of blob `id`, from its compressed file or else its raw one.
/// A damaged compressed blob with no raw copy is an `InvalidData` error,
/// never garbage.
pub fn read_blob(dir: &Path, id: &str) -> io::Result<Vec<u8>> {
    if !valid_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a snapshot id: {id}")));
    }
    match std::fs::read(zblob_file(dir, id)) {
        Ok(z) => inflate(&z).or_else(|e| std::fs::read(blob_file(dir, id)).map_err(|_| e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::read(blob_file(dir, id)),
        Err(e) => Err(e),
    }
}

/// Holds `prune.lock` until dropped.
struct PruneLock(PathBuf);

impl PruneLock {
    fn take(dir: &Path) -> io::Result<Option<Self>> {
        let file = dir.join("prune.lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Some(Self(file)));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                    if age.is_some_and(|a| a > LOCK_STALE) {
                        let _ = std::fs::remove_file(&file);
                        continue;
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }
}

impl Drop for PruneLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One index file read for a prune.
struct IndexFile {
    file: PathBuf,
    entries: Vec<Entry>,
    /// Lines that did not parse (dropped when the file is rewritten).
    corrupt: bool,
    changed: bool,
}

/// Move entries stored under a path that is not their [`key`] (written by
/// an older jetl, through a symlink) under the key, then drop entries older
/// than `keep_ms` before `now_ms`, then the oldest ones (of any file) while
/// the bytes on disk of the blobs still referenced exceed `max_bytes`;
/// rewrite the changed index files (an emptied one is removed) and delete
/// the blobs no entry references (once older than [`BLOB_GRACE`]). Does
/// nothing while another instance prunes. Never creates `dir`.
pub fn prune(dir: &Path, now_ms: u64, keep_ms: u64, max_bytes: u64) -> io::Result<PruneStats> {
    if !dir.is_dir() {
        return Ok(PruneStats::default());
    }
    let lock = match PruneLock::take(dir)? {
        Some(lock) => Some(lock),
        None => {
            std::thread::sleep(LOCK_RETRY);
            PruneLock::take(dir)?
        }
    };
    let Some(_lock) = lock else {
        return Ok(PruneStats { locked: true, ..Default::default() });
    };
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(index_dir(dir)) {
        for e in rd.flatten() {
            let file = e.path();
            if file.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            let mut corrupt = false;
            let entries = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    let e = serde_json::from_str::<Entry>(l).ok();
                    corrupt |= e.is_none();
                    e
                })
                .collect();
            files.push(IndexFile { file, entries, corrupt, changed: false });
        }
    }
    let mut stats = PruneStats { moved: migrate_keys(dir, &mut files), ..Default::default() };
    // By age.
    let cutoff = now_ms.saturating_sub(keep_ms);
    for f in &mut files {
        let before = f.entries.len();
        f.entries.retain(|e| e.at >= cutoff);
        if f.entries.len() != before {
            f.changed = true;
            stats.entries += before - f.entries.len();
        }
    }
    // By size: the unique blobs still referenced, oldest entries first out.
    let mut refs: HashMap<&str, (usize, u64)> = HashMap::new();
    for e in files.iter().flat_map(|f| &f.entries) {
        refs.entry(e.blob.as_str()).or_insert_with(|| (0, stored_len(dir, &e.blob))).0 += 1;
    }
    let mut total: u64 = refs.values().map(|(_, len)| len).sum();
    let mut drop_set: HashSet<(usize, usize)> = HashSet::new();
    if total > max_bytes {
        let mut order: Vec<(u64, usize, usize)> =
            files.iter().enumerate().flat_map(|(fi, f)| f.entries.iter().enumerate().map(move |(ei, e)| (e.at, fi, ei))).collect();
        order.sort_unstable();
        for (_, fi, ei) in order {
            if total <= max_bytes {
                break;
            }
            let e = &files[fi].entries[ei];
            if let Some(r) = refs.get_mut(e.blob.as_str()) {
                r.0 -= 1;
                if r.0 == 0 {
                    total -= r.1;
                }
            }
            drop_set.insert((fi, ei));
        }
    }
    let keep: HashSet<String> = refs.iter().filter(|(_, (n, _))| *n > 0).map(|(id, _)| id.to_string()).collect();
    drop(refs);
    for (fi, f) in files.iter_mut().enumerate() {
        let before = f.entries.len();
        let mut ei = 0;
        f.entries.retain(|_| {
            let k = !drop_set.contains(&(fi, ei));
            ei += 1;
            k
        });
        if f.entries.len() != before {
            f.changed = true;
            stats.entries += before - f.entries.len();
        }
    }
    for f in files.iter().filter(|f| f.changed || (f.corrupt && !f.entries.is_empty())) {
        rewrite_index(&f.file, &f.entries)?;
    }
    stats.bytes = total;
    stats.blobs = gc_blobs(dir, &keep);
    Ok(stats)
}

/// Move every entry whose path is not its canonical [`key`] into the index
/// of the key (created when missing), merged by time, so two keys of one
/// file become one history before the prune counts entries; returns how
/// many moved. A path that does not resolve (a deleted file whose folders
/// are gone too) keeps its key.
fn migrate_keys(dir: &Path, files: &mut Vec<IndexFile>) -> usize {
    let mut keys: HashMap<String, Option<String>> = HashMap::new();
    let mut moving: Vec<Entry> = Vec::new();
    for f in files.iter_mut() {
        let file = f.file.clone();
        let mut rekeyed = false;
        f.entries.retain_mut(|e| {
            let k = keys.entry(e.path.clone()).or_insert_with(|| key(Path::new(&e.path)).to_str().map(str::to_string));
            let Some(k) = k.as_ref().filter(|k| **k != e.path) else { return true };
            e.path = k.clone();
            rekeyed = true;
            // Stays when the key's index is this file (a hash collision).
            if index_file(dir, k) == file {
                return true;
            }
            moving.push(e.clone());
            false
        });
        f.changed |= rekeyed;
    }
    let moved = moving.len();
    let mut at: HashMap<PathBuf, usize> = files.iter().enumerate().map(|(i, f)| (f.file.clone(), i)).collect();
    let mut touched = HashSet::new();
    for e in moving {
        let file = index_file(dir, &e.path);
        let fi = *at.entry(file.clone()).or_insert_with(|| {
            files.push(IndexFile { file, entries: Vec::new(), corrupt: false, changed: false });
            files.len() - 1
        });
        files[fi].entries.push(e);
        files[fi].changed = true;
        touched.insert(fi);
    }
    for fi in touched {
        // Stable: entries at the same time keep their order.
        files[fi].entries.sort_by_key(|e| e.at);
    }
    moved
}

/// Replace an index file by `entries` (through a temp file and a rename);
/// none left removes it.
fn rewrite_index(file: &Path, entries: &[Entry]) -> io::Result<()> {
    if entries.is_empty() {
        return match std::fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let mut text = String::new();
    for e in entries {
        text.push_str(&serde_json::to_string(e).map_err(io::Error::other)?);
        text.push('\n');
    }
    write_index(file, &text)
}

/// Replace index file `file` by `text` through a temp file and a rename.
/// Only under `prune.lock`: the temp name is the pid's.
fn write_index(file: &Path, text: &str) -> io::Result<()> {
    let tmp = file.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Delete the blobs (and leftover temp files) not in `keep` that are older
/// than [`BLOB_GRACE`], raw and compressed, and the empty fan-out
/// directories; returns how many blobs went.
fn gc_blobs(dir: &Path, keep: &HashSet<String>) -> usize {
    gc_tree(&blobs_dir(dir), keep) + gc_tree(&zblobs_dir(dir), keep)
}

/// [`gc_blobs`] in one tree of fan-out directories.
fn gc_tree(root: &Path, keep: &HashSet<String>) -> usize {
    let mut removed = 0;
    let Ok(fans) = std::fs::read_dir(root) else { return 0 };
    for fan in fans.flatten() {
        let fan = fan.path();
        let Ok(rd) = std::fs::read_dir(&fan) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if keep.contains(&name) {
                continue;
            }
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a > BLOB_GRACE);
            if old && std::fs::remove_file(e.path()).is_ok() && !name.starts_with('.') {
                removed += 1;
            }
        }
        // Only succeeds when empty.
        let _ = std::fs::remove_dir(&fan);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    const MB: u64 = 1 << 20;

    fn rec(dir: &Path, path: &str, text: &str, at: u64) -> Recorded {
        record(dir, Path::new(path), text.as_bytes(), Kind::Save, None, at, MB).unwrap()
    }

    /// The files in one blob tree (`blobs_dir` or `zblobs_dir`).
    fn tree_files(root: &Path) -> Vec<PathBuf> {
        let Ok(fans) = std::fs::read_dir(root) else { return Vec::new() };
        fans.flatten().flat_map(|f| std::fs::read_dir(f.path()).into_iter().flatten().flatten().map(|b| b.path())).collect()
    }

    /// The blob files, raw and compressed.
    fn blob_count(dir: &Path) -> usize {
        tree_files(&blobs_dir(dir)).len() + tree_files(&zblobs_dir(dir)).len()
    }

    /// Make every blob older than the grace period.
    fn age_blobs(dir: &Path) {
        let old = SystemTime::now() - BLOB_GRACE - Duration::from_secs(60);
        for b in tree_files(&blobs_dir(dir)).into_iter().chain(tree_files(&zblobs_dir(dir))) {
            std::fs::File::options().write(true).open(b).unwrap().set_modified(old).unwrap();
        }
    }

    /// About 7 KB of source text: compresses well.
    fn source() -> String {
        "fn main() { println!(\"hello\"); }\n".repeat(200)
    }

    /// 4 KB of xorshift noise without a NUL: not binary, but deflate makes
    /// it larger.
    fn noise() -> Vec<u8> {
        let mut x: u32 = 0x9e37_79b9;
        (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as u8).max(1)
            })
            .collect()
    }

    fn added(r: Recorded) -> Entry {
        match r {
            Recorded::Added(e) => e,
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn record_and_list_round_trip_with_dedupe() {
        let dir = temp("roundtrip");
        assert!(list(&dir, Path::new("/p/a.txt")).is_empty());
        let Recorded::Added(e) = rec(&dir, "/p/a.txt", "one\ntwo\n", 10) else { panic!("added") };
        assert_eq!((e.len, e.lines, e.kind, e.at), (8, 2, Kind::Save, 10));
        assert_eq!(read_blob(&dir, &e.blob).unwrap(), b"one\ntwo\n");
        // The same bytes again: nothing new.
        assert_eq!(rec(&dir, "/p/a.txt", "one\ntwo\n", 11), Recorded::Same);
        assert_eq!(list(&dir, Path::new("/p/a.txt")).len(), 1);
        // A, B, A: three entries, two blobs.
        rec(&dir, "/p/a.txt", "B", 12);
        rec(&dir, "/p/a.txt", "one\ntwo\n", 13);
        let l = list(&dir, Path::new("/p/a.txt"));
        assert_eq!(l.iter().map(|e| e.at).collect::<Vec<_>>(), [13, 12, 10], "newest first");
        assert_eq!(l[0].blob, l[2].blob);
        assert_eq!(blob_count(&dir), 2);
        // Another file with the same bytes shares the blob.
        rec(&dir, "/p/b.txt", "B", 14);
        assert_eq!(blob_count(&dir), 2);
        assert_eq!(list(&dir, Path::new("/p/b.txt")).len(), 1);
        assert!(read_blob(&dir, "../../etc/passwd").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_binary_and_internal_files_are_skipped() {
        let dir = temp("skip");
        let r = |path: &Path, bytes: &[u8], max| record(&dir, path, bytes, Kind::Save, None, 1, max).unwrap();
        assert_eq!(r(Path::new("/p/big"), b"12345", 4), Recorded::Skipped(Skip::TooLarge));
        assert!(matches!(r(Path::new("/p/big"), b"1234", 4), Recorded::Added(_)));
        assert_eq!(r(Path::new("/p/bin"), b"a\0b", MB), Recorded::Skipped(Skip::Binary));
        assert_eq!(r(&dir.join("index/x.jsonl"), b"x", MB), Recorded::Skipped(Skip::Internal));
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collisions_and_corrupt_lines_are_filtered() {
        let dir = temp("collide");
        rec(&dir, "/p/a.txt", "a", 1);
        // Another path's line in the same file (a hash collision) and garbage.
        let file = index_file(&dir, "/p/a.txt");
        let mut other = list(&dir, Path::new("/p/a.txt"))[0].clone();
        other.path = "/p/other".into();
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(&serde_json::to_string(&other).unwrap());
        text.push_str("\n{not json\n");
        std::fs::write(&file, text).unwrap();
        assert_eq!(list(&dir, Path::new("/p/a.txt")).len(), 1);
        // The dedupe check reads the path's own last entry.
        assert_eq!(rec(&dir, "/p/a.txt", "a", 2), Recorded::Same);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_by_age_then_size_keeps_shared_blobs() {
        let dir = temp("prune");
        let day = 86_400_000;
        rec(&dir, "/p/old.txt", "old", 0);
        rec(&dir, "/p/a.txt", "aaaa", 10 * day);
        rec(&dir, "/p/b.txt", "aaaa", 10 * day + 1);
        rec(&dir, "/p/b.txt", "bbbbbb", 10 * day + 2);
        age_blobs(&dir);
        // Five days kept at day 11: the day-0 entry goes, its file too.
        let s = prune(&dir, 11 * day, 5 * day, MB).unwrap();
        assert_eq!((s.entries, s.blobs, s.locked), (1, 1, false));
        assert!(list(&dir, Path::new("/p/old.txt")).is_empty());
        assert!(!index_file(&dir, "/p/old.txt").exists());
        // 10 bytes referenced; a cap of 7 drops the oldest entries until the
        // blobs fit: a.txt's goes, but its blob stays for b.txt's first one,
        // which goes next; "bbbbbb" (6) fits.
        let s = prune(&dir, 11 * day, 5 * day, 7).unwrap();
        assert_eq!((s.entries, s.blobs), (2, 1));
        assert!(list(&dir, Path::new("/p/a.txt")).is_empty());
        assert_eq!(list(&dir, Path::new("/p/b.txt")).iter().map(|e| e.len).collect::<Vec<_>>(), [6]);
        assert_eq!(blob_count(&dir), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_young_orphans_and_skips_while_locked() {
        let dir = temp("grace");
        rec(&dir, "/p/a.txt", "a", 0);
        // Everything is too old, but the orphaned blob is young.
        let s = prune(&dir, 1_000_000, 0, MB).unwrap();
        assert_eq!((s.entries, s.blobs), (1, 0));
        assert_eq!(blob_count(&dir), 1);
        age_blobs(&dir);
        // Another instance prunes: nothing happens.
        std::fs::write(dir.join("prune.lock"), "1").unwrap();
        assert!(prune(&dir, 1_000_000, 0, MB).unwrap().locked);
        assert_eq!(blob_count(&dir), 1);
        // A stale lock is taken over.
        let stale = SystemTime::now() - LOCK_STALE - Duration::from_secs(60);
        std::fs::File::options().write(true).open(dir.join("prune.lock")).unwrap().set_modified(stale).unwrap();
        let s = prune(&dir, 1_000_000, 0, MB).unwrap();
        assert_eq!((s.locked, s.blobs), (false, 1));
        assert_eq!(blob_count(&dir), 0);
        assert!(!dir.join("prune.lock").exists());
        // A missing store is not created.
        let none = temp("grace-none");
        assert_eq!(prune(&none, 1, 0, 0).unwrap(), PruneStats::default());
        assert!(!none.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real file, a symlink to it and a symlinked folder above it.
    #[cfg(unix)]
    fn linked(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let dir = temp(name);
        let real = dir.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("a.txt"), "a").unwrap();
        std::os::unix::fs::symlink(real.join("a.txt"), dir.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("linkdir")).unwrap();
        let real = std::fs::canonicalize(&real).unwrap();
        (dir.join("history"), real, dir.join("link.txt"), dir.join("linkdir"))
    }

    #[cfg(unix)]
    fn ats(dir: &Path, path: &Path) -> Vec<u64> {
        list(dir, path).iter().map(|e| e.at).collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_file_and_its_symlinks_share_one_history() {
        let (hist, real, link, linkdir) = linked("symlink");
        let a = real.join("a.txt");
        let r = |p: &Path, text: &str, at| record(&hist, p, text.as_bytes(), Kind::Save, None, at, MB).unwrap();
        assert!(matches!(r(&a, "1", 1), Recorded::Added(_)));
        assert!(matches!(r(&link, "2", 2), Recorded::Added(_)));
        // The same bytes as the last entry, through another path: no new one.
        assert_eq!(r(&linkdir.join("a.txt"), "2", 3), Recorded::Same);
        assert!(matches!(r(&linkdir.join("a.txt"), "3", 4), Recorded::Added(_)));
        for p in [&a, &link, &linkdir.join("a.txt")] {
            assert_eq!(ats(&hist, p), [4, 2, 1], "{}", p.display());
        }
        assert!(list(&hist, &a).iter().all(|e| e.path == a.to_str().unwrap()), "stored under the canonical path");
        assert_eq!(std::fs::read_dir(index_dir(&hist)).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(hist.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_deleted_file_is_still_listed_by_the_path_it_was_opened_with() {
        let (hist, real, link, linkdir) = linked("deleted");
        let x = linkdir.join("sub/x.txt");
        std::fs::write(&x, "x").unwrap();
        rec(&hist, link.to_str().unwrap(), "a", 1);
        rec(&hist, x.to_str().unwrap(), "x", 2);
        std::fs::remove_file(real.join("a.txt")).unwrap();
        std::fs::remove_dir_all(real.join("sub")).unwrap();
        // A dangling symlink is followed; a missing folder is joined onto
        // the nearest one that exists.
        assert_eq!(key(&link), real.join("a.txt"));
        assert_eq!(key(&x), real.join("sub/x.txt"));
        assert_eq!(ats(&hist, &link), [1]);
        assert_eq!(ats(&hist, &real.join("a.txt")), [1]);
        assert_eq!(ats(&hist, &x), [2]);
        // A prune keeps them where they are.
        let s = prune(&hist, 3, 10, MB).unwrap();
        assert_eq!((s.entries, s.moved), (0, 0));
        assert_eq!(ats(&hist, &x), [2]);
        // Nothing resolves at all: the path as given.
        assert_eq!(key(Path::new("/no/such/dir/f")), Path::new("/no/such/dir/f"));
        let _ = std::fs::remove_dir_all(hist.parent().unwrap());
    }

    /// Append an entry for `path` as an older jetl did: under the path as
    /// given.
    #[cfg(unix)]
    fn legacy(dir: &Path, path: &Path, text: &str, at: u64) {
        let path = path.to_str().unwrap();
        let id = blob_id(text.as_bytes()).unwrap();
        write_blob(dir, &id, text.as_bytes()).unwrap();
        let e =
            Entry { v: 1, path: path.into(), at, blob: id, len: text.len() as u64, lines: 1, kind: Kind::Save, detail: None, since: None };
        std::fs::create_dir_all(index_dir(dir)).unwrap();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(index_file(dir, path)).unwrap();
        writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn legacy_keys_of_one_file_are_merged_by_prune() {
        let (hist, real, link, linkdir) = linked("legacy");
        let a = real.join("a.txt");
        legacy(&hist, &link, "l1", 1);
        legacy(&hist, &a, "r2", 2);
        legacy(&hist, &linkdir.join("a.txt"), "d3", 3);
        legacy(&hist, &link, "l4", 4);
        // Before the migration the path as given is read too.
        assert_eq!(ats(&hist, &link), [4, 2, 1]);
        assert_eq!(ats(&hist, &a), [2]);
        // A prune moves them under the canonical key, merged by time, and
        // counts them with the rest: the oldest goes by age.
        let s = prune(&hist, 10, 8, MB).unwrap();
        assert_eq!((s.moved, s.entries), (3, 1));
        for p in [&a, &link, &linkdir.join("a.txt")] {
            assert_eq!(ats(&hist, p), [4, 3, 2], "{}", p.display());
        }
        assert_eq!(std::fs::read_dir(index_dir(&hist)).unwrap().count(), 1, "the legacy indexes are gone");
        // Size: 6 bytes referenced, a cap of 3 drops the two oldest.
        let s = prune(&hist, 10, 8, 3).unwrap();
        assert_eq!((s.moved, s.entries), (0, 2));
        assert_eq!(ats(&hist, &link), [4]);
        // Only a legacy key: its canonical index is created.
        legacy(&hist, &linkdir.join("b.txt"), "b", 5);
        std::fs::write(real.join("b.txt"), "b").unwrap();
        assert_eq!(prune(&hist, 10, 8, MB).unwrap().moved, 1);
        assert_eq!(list(&hist, &real.join("b.txt"))[0].path, real.join("b.txt").to_str().unwrap());
        let _ = std::fs::remove_dir_all(hist.parent().unwrap());
    }

    #[test]
    fn compress_and_inflate_round_trip() {
        let text = source();
        let z = compress(text.as_bytes()).expect("compressible");
        assert!(z.len() < text.len() / 4);
        assert_eq!(inflate(&z).unwrap(), text.as_bytes());
        assert_eq!(compress(&text.as_bytes()[..MIN_COMPRESS - 1]), None);
        assert!(compress(&text.as_bytes()[..MIN_COMPRESS]).is_some());
        assert_eq!(compress(&noise()), None);
    }

    #[test]
    fn compressible_blobs_are_stored_compressed() {
        let dir = temp("zstore");
        let text = source();
        let e = added(rec(&dir, "/p/a.rs", &text, 1));
        assert!(zblob_file(&dir, &e.blob).is_file());
        assert!(!blob_file(&dir, &e.blob).exists());
        assert!(stored_len(&dir, &e.blob) < text.len() as u64 / 4);
        assert_eq!(read_blob(&dir, &e.blob).unwrap(), text.as_bytes());
        assert_eq!(e.len, text.len() as u64, "the index keeps the raw size");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiny_and_incompressible_blobs_stay_raw() {
        let dir = temp("zraw");
        let short = "ab".repeat(31) + "c";
        assert_eq!(short.len(), 63);
        let noise = noise();
        let entries = [
            added(rec(&dir, "/p/a.txt", "a", 1)),
            added(rec(&dir, "/p/b.txt", &short, 2)),
            added(record(&dir, Path::new("/p/c.bin"), &noise, Kind::Save, None, 3, MB).unwrap()),
        ];
        for (e, bytes) in entries.iter().zip([b"a".as_slice(), short.as_bytes(), &noise]) {
            assert!(blob_file(&dir, &e.blob).is_file());
            assert!(!zblob_file(&dir, &e.blob).exists());
            assert_eq!(read_blob(&dir, &e.blob).unwrap(), bytes);
        }
        assert_eq!(stored_len(&dir, &entries[2].blob), 4096);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_raw_blobs_are_read_and_reused() {
        let dir = temp("zlegacy");
        let text = source();
        let id = blob_id(text.as_bytes()).unwrap();
        // As an older jetl wrote it: raw.
        let raw = blob_file(&dir, &id);
        std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
        std::fs::write(&raw, &text).unwrap();
        assert_eq!(read_blob(&dir, &id).unwrap(), text.as_bytes());
        // The same bytes saved again reuse it, not a compressed copy.
        assert_eq!(added(rec(&dir, "/p/a.rs", &text, 1)).blob, id);
        assert!(!zblob_file(&dir, &id).exists());
        assert_eq!(blob_count(&dir), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressed_blobs_dedupe_across_files() {
        let dir = temp("zdedupe");
        let a = source();
        let b = a.replace("hello", "world");
        rec(&dir, "/p/a.rs", &a, 1);
        rec(&dir, "/p/b.rs", &a, 2);
        assert_eq!(tree_files(&zblobs_dir(&dir)).len(), 1);
        rec(&dir, "/p/c.rs", &a, 3);
        rec(&dir, "/p/c.rs", &b, 4);
        rec(&dir, "/p/c.rs", &a, 5);
        assert_eq!(list(&dir, Path::new("/p/c.rs")).len(), 3);
        assert_eq!(blob_count(&dir), 2);
        assert_eq!(tree_files(&zblobs_dir(&dir)).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_counts_bytes_on_disk() {
        let dir = temp("zprune");
        let (t1, t2) = (source(), source().replace("hello", "world"));
        let c1 = added(rec(&dir, "/p/a.rs", &t1, 1));
        let c2 = added(rec(&dir, "/p/a.rs", &t2, 2));
        let (s1, s2) = (stored_len(&dir, &c1.blob), stored_len(&dir, &c2.blob));
        assert!(s1 + s2 < c1.len, "compressed sizes well under the raw ones");
        age_blobs(&dir);
        // Raw lengths would be over the cap and drop one.
        let s = prune(&dir, 3, 10, s1 + s2).unwrap();
        assert_eq!((s.entries, s.blobs, s.bytes), (0, 0, s1 + s2));
        let s = prune(&dir, 3, 10, s2).unwrap();
        assert_eq!((s.entries, s.blobs, s.bytes), (1, 1, s2));
        assert!(!zblob_file(&dir, &c1.blob).exists());
        assert_eq!(list(&dir, Path::new("/p/a.rs"))[0].blob, c2.blob);
        // A missing blob takes no space; an invalid id neither panics nor counts.
        let file = index_file(&dir, "/p/a.rs");
        let mut text = std::fs::read_to_string(&file).unwrap();
        for (blob, at) in [(blob_id(b"gone").unwrap(), 3), ("x".to_string(), 4)] {
            let e = Entry { v: 1, path: "/p/a.rs".into(), at, blob, len: 1 << 30, lines: 1, kind: Kind::Save, detail: None, since: None };
            text.push_str(&serde_json::to_string(&e).unwrap());
            text.push('\n');
        }
        std::fs::write(&file, text).unwrap();
        assert_eq!(stored_len(&dir, "x"), 0);
        let s = prune(&dir, 5, 10, s2).unwrap();
        assert_eq!((s.entries, s.bytes), (0, s2));
        // Both copies of one blob count.
        std::fs::create_dir_all(blob_file(&dir, &c2.blob).parent().unwrap()).unwrap();
        std::fs::write(blob_file(&dir, &c2.blob), &t2).unwrap();
        assert_eq!(stored_len(&dir, &c2.blob), s2 + t2.len() as u64);
        assert_eq!(prune(&dir, 5, 10, MB).unwrap().bytes, s2 + t2.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_removes_unreferenced_compressed_blobs() {
        let dir = temp("zgc");
        rec(&dir, "/p/a.rs", &source(), 0);
        // Every entry is too old, but the orphaned blob is young.
        let s = prune(&dir, 1_000_000, 0, MB).unwrap();
        assert_eq!((s.entries, s.blobs), (1, 0));
        assert_eq!(tree_files(&zblobs_dir(&dir)).len(), 1);
        age_blobs(&dir);
        let s = prune(&dir, 1_000_000, 0, MB).unwrap();
        assert_eq!(s.blobs, 1);
        assert_eq!(blob_count(&dir), 0);
        assert_eq!(std::fs::read_dir(zblobs_dir(&dir)).unwrap().count(), 0, "empty fan-out dirs go");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_compressed_blob_is_a_clean_error() {
        let dir = temp("zdamaged");
        let text = source();
        let e = added(rec(&dir, "/p/a.rs", &text, 1));
        let file = zblob_file(&dir, &e.blob);
        let z = std::fs::read(&file).unwrap();
        let mut flipped = z.clone();
        flipped[z.len() / 2] ^= 0xff;
        for bad in [flipped, z[..z.len() / 2].to_vec(), b"garbage".to_vec(), Vec::new()] {
            std::fs::write(&file, &bad).unwrap();
            let err = read_blob(&dir, &e.blob).expect_err("damaged");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
            assert!(err.to_string().contains("damaged"), "{err}");
        }
        // A raw copy is the fallback.
        std::fs::create_dir_all(blob_file(&dir, &e.blob).parent().unwrap()).unwrap();
        std::fs::write(blob_file(&dir, &e.blob), &text).unwrap();
        assert_eq!(read_blob(&dir, &e.blob).unwrap(), text.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn auto(dir: &Path, path: &str, text: &str, at: u64) -> Recorded {
        record(dir, Path::new(path), text.as_bytes(), Kind::Autosave, None, at, MB).unwrap()
    }

    const MIN: u64 = 60_000;

    /// `(at, since, text)` of `path`'s entries, newest first.
    fn runs(dir: &Path, path: &str) -> Vec<(u64, Option<u64>, String)> {
        list(dir, Path::new(path))
            .into_iter()
            .map(|e| (e.at, e.since, String::from_utf8(read_blob(dir, &e.blob).unwrap()).unwrap()))
            .collect()
    }

    #[test]
    fn autosaves_within_the_window_merge_into_one_entry() {
        let dir = temp("autorun");
        assert!(matches!(auto(&dir, "/p/a.txt", "a", 1000), Recorded::Added(_)));
        // Another path's line (a hash collision) and a corrupt one in the
        // same file survive the rewrite.
        let file = index_file(&dir, "/p/a.txt");
        let mut other = list(&dir, Path::new("/p/a.txt"))[0].clone();
        other.path = "/p/other".into();
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(&serde_json::to_string(&other).unwrap());
        text.push_str("\n{not json\n");
        std::fs::write(&file, &text).unwrap();
        let Recorded::Replaced(e) = auto(&dir, "/p/a.txt", "b", 2000) else { panic!("replaced") };
        assert_eq!((e.at, e.since), (2000, Some(1000)));
        assert!(matches!(auto(&dir, "/p/a.txt", "c", 3000), Recorded::Replaced(_)));
        assert_eq!(runs(&dir, "/p/a.txt"), [(3000, Some(1000), "c".into())]);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(text.ends_with("{not json\n"));
        assert_eq!(read_entries(&file, "/p/other").len(), 1);
        // The same bytes again are still `Same`.
        assert_eq!(auto(&dir, "/p/a.txt", "c", 4000), Recorded::Same);
        assert_eq!(runs(&dir, "/p/a.txt").len(), 1);
        assert!(!dir.join("prune.lock").exists(), "the lock is released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_kinds_break_an_autosave_run() {
        let dir = temp("autobreak");
        auto(&dir, "/p/a.txt", "a", 1);
        rec(&dir, "/p/a.txt", "b", 2);
        auto(&dir, "/p/a.txt", "c", 3);
        assert_eq!(list(&dir, Path::new("/p/a.txt")).len(), 3);
        // A snapshot before a git operation is not merged into the run, and
        // the autosave after it starts a new one.
        let git = record(&dir, Path::new("/p/a.txt"), b"d", Kind::BeforeGit, Some("git pull".into()), 4, MB).unwrap();
        assert!(matches!(git, Recorded::Added(_)));
        auto(&dir, "/p/a.txt", "e", 5);
        auto(&dir, "/p/a.txt", "f", 6);
        let r = runs(&dir, "/p/a.txt");
        assert_eq!(
            r.iter().map(|(at, since, _)| (*at, *since)).collect::<Vec<_>>(),
            [(6, Some(5)), (4, None), (3, None), (2, None), (1, None)]
        );
        // Another file's autosave neither merges nor breaks this run.
        auto(&dir, "/p/b.txt", "x", 7);
        auto(&dir, "/p/a.txt", "g", 8);
        assert_eq!(runs(&dir, "/p/a.txt")[0], (8, Some(5), "g".into()));
        assert_eq!(runs(&dir, "/p/b.txt").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_is_measured_from_the_runs_first_autosave() {
        let dir = temp("autowindow");
        auto(&dir, "/p/a.txt", "a", 0);
        // Past the window: a new entry.
        auto(&dir, "/p/b.txt", "a", 0);
        auto(&dir, "/p/b.txt", "b", 5 * MIN);
        assert_eq!(runs(&dir, "/p/b.txt").len(), 2);
        // Four minutes apart each, but the third is five minutes after the
        // run's first: it starts a new run, which the fourth joins.
        auto(&dir, "/p/a.txt", "b", 4 * MIN);
        auto(&dir, "/p/a.txt", "c", 8 * MIN);
        auto(&dir, "/p/a.txt", "d", 12 * MIN);
        assert_eq!(runs(&dir, "/p/a.txt"), [(12 * MIN, Some(8 * MIN), "d".into()), (4 * MIN, Some(0), "b".into())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_replaced_blob_is_collected_only_when_unreferenced() {
        let dir = temp("autogc");
        let (y, z, w) = (source(), source().replace("hello", "z"), source().replace("hello", "w"));
        rec(&dir, "/p/b.rs", &y, 0);
        let ey = added(auto(&dir, "/p/a.rs", &y, 1));
        let Recorded::Replaced(ez) = auto(&dir, "/p/a.rs", &z, 2) else { panic!("replaced") };
        let Recorded::Replaced(ew) = auto(&dir, "/p/a.rs", &w, 3) else { panic!("replaced") };
        assert_eq!(blob_count(&dir), 3);
        // Young orphans stay.
        assert_eq!(prune(&dir, 10, 100, MB).unwrap().blobs, 0);
        age_blobs(&dir);
        let s = prune(&dir, 10, 100, MB).unwrap();
        assert_eq!((s.entries, s.blobs), (0, 1));
        assert!(read_blob(&dir, &ez.blob).is_err(), "z was only in the replaced entry");
        assert_eq!(read_blob(&dir, &ey.blob).unwrap(), y.as_bytes(), "y is still b.rs's");
        assert_eq!(read_blob(&dir, &ew.blob).unwrap(), w.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_autosave_appends_while_another_instance_prunes() {
        let dir = temp("autolock");
        auto(&dir, "/p/a.txt", "a", 1);
        std::fs::write(dir.join("prune.lock"), "1").unwrap();
        assert!(matches!(auto(&dir, "/p/a.txt", "b", 2), Recorded::Added(_)));
        assert!(dir.join("prune.lock").exists(), "not ours to remove");
        std::fs::remove_file(dir.join("prune.lock")).unwrap();
        // The appended entry starts the run the next autosave joins.
        assert!(matches!(auto(&dir, "/p/a.txt", "c", 3), Recorded::Replaced(_)));
        assert_eq!(runs(&dir, "/p/a.txt").iter().map(|(at, since, _)| (*at, *since)).collect::<Vec<_>>(), [(3, Some(2)), (1, None)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_appends_keep_every_line() {
        let dir = temp("threads");
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..100 {
                        rec(&dir, "/p/a.txt", &format!("{t}-{i}"), i);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(list(&dir, Path::new("/p/a.txt")).len(), 200);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
