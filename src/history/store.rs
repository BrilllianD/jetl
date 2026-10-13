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
//! Automatic snapshots (autosaves and snapshots of unsaved edits,
//! [`Kind::automatic`]) come in runs (idle autosave may write every few
//! seconds), so an automatic snapshot whose file's last entry is an
//! automatic one of the same run (the run's first one taken less than
//! [`AUTOSAVE_RUN`] ago) replaces that entry instead of adding one:
//! [`record`] rewrites the line in place (temp file and rename) while
//! holding `prune.lock`, or appends when another instance holds it. The
//! replaced entry's blob is collected by the next [`prune`] unless another
//! entry still names it. The window is measured from the run's first
//! snapshot (the entry's `since`), so a long editing session still leaves
//! one entry per window. Any other kind ends the run.
//!
//! A snapshot of the bytes an unsaved snapshot (the file's last entry)
//! holds is not `Same` unless it is one too: it replaces that entry the
//! same way, so a save of that text shows as "Saved".
//!
//! A rename in the Project tree carries a file's history (or every file's
//! under a folder) to the new key: [`rename`] appends copies of the entries
//! (blobs shared) and a `Renamed` entry; the old key keeps its own, so a new
//! file at the old path starts from that path's history, never mixing with
//! the renamed one's. Index files may hold entries out of time order after
//! that; reads sort by time and prune rewrites them sorted.
//!
//! A label (Put Label, `Kind::Label`, the name in `detail`) is an entry the
//! user makes and removes: [`record`] always adds it (never `Same`, never
//! replacing; its blob is shared like any other), it ends an automatic run,
//! [`prune`] keeps it past `keep_ms` and drops it only after every other
//! entry when the store is over its size, [`rename`] moves it (dropping the
//! old key's copy under the lock), and [`remove_entry`] deletes its line
//! under `prune.lock`.
//!
//! The Local History window's project view asks [`recent`]: the files under
//! a folder that have history, newest change first, each marked present,
//! deleted or renamed away. It scans the whole index (as [`prune`] does)
//! through an [`IndexCache`] the worker keeps, which reads an index file
//! again only when it changed and keeps a summary per key, not the entries.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// A rename that carries nothing new and repeats the newest "Renamed from"
/// marker within this long is an echo of the same move seen twice (the
/// Project tree and git both report it), and writes no second marker.
pub const RENAME_ECHO: Duration = Duration::from_secs(60);

/// Automatic snapshots of one file within this long of the first one of
/// their run are merged into one entry (see the module doc).
pub const AUTOSAVE_RUN: Duration = Duration::from_secs(5 * 60);

/// What a snapshot was taken for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An explicit save (Ctrl+S, Save All, closing, quitting): the text written.
    Save,
    /// An autosave: the text written.
    Autosave,
    /// The text of a tab with unsaved edits, taken every
    /// `snapshot_unsaved_secs`.
    Unsaved,
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
    /// A file renamed or moved in the Project tree: its text then; the detail is the old path.
    Renamed,
    /// A label the user put on the file (Put Label): its text then; the detail is the name.
    Label,
}

impl Kind {
    /// The row label: "Saved", "Before git pull --ff-only".
    pub fn label(self, detail: Option<&str>) -> String {
        match self {
            Kind::Save => "Saved".into(),
            Kind::Autosave => "Autosaved".into(),
            Kind::Unsaved => "Unsaved changes".into(),
            Kind::SaveAs => match detail {
                Some(d) if !d.is_empty() => format!("Saved As from {d}"),
                _ => "Saved As".into(),
            },
            Kind::BeforeSaveAs => "Before Save As overwrote it".into(),
            Kind::BeforeOverwrite => "Before overwrite".into(),
            Kind::BeforeReload => "Before reload".into(),
            Kind::BeforeDelete => "Before delete".into(),
            Kind::BeforeGit => match detail {
                Some(d) if !d.is_empty() => format!("Before {d}"),
                _ => "Before git".into(),
            },
            Kind::Renamed => match detail {
                Some(d) if !d.is_empty() => format!("Renamed from {d}"),
                _ => "Renamed".into(),
            },
            Kind::Label => match detail {
                Some(d) if !d.is_empty() => format!("Label: {d}"),
                _ => "Label".into(),
            },
        }
    }

    /// Snapshots taken on their own (autosaves, unsaved edits): consecutive
    /// ones of a file merge.
    pub fn automatic(self) -> bool {
        matches!(self, Kind::Autosave | Kind::Unsaved)
    }

    /// Entries the user made and removes (labels): never `Same`, kept by the
    /// age cut and dropped last by the size cut, moved (not copied) by a rename.
    pub fn pinned(self) -> bool {
        matches!(self, Kind::Label)
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
    /// An automatic snapshot that replaced others of its run: when the
    /// run's first one was taken (Unix ms). Absent from the line otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

impl Entry {
    pub fn label(&self) -> String {
        self.kind.label(self.detail.as_deref())
    }

    /// [`Entry::label`] with the path in a detail ("Renamed from …",
    /// "Saved As from …") shown relative to `root` when under it.
    pub fn label_in(&self, root: &Path) -> String {
        match (self.kind, self.detail.as_deref()) {
            (Kind::Renamed | Kind::SaveAs, Some(d)) => match Path::new(d).strip_prefix(root) {
                Ok(rel) if !rel.as_os_str().is_empty() => self.kind.label(Some(&rel.to_string_lossy())),
                _ => self.kind.label(Some(d)),
            },
            _ => self.label(),
        }
    }

    /// When the automatic snapshot run this entry stands for began.
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

impl Skip {
    /// Why, as the end of a sentence: "… because it is a binary file".
    pub fn reason(self) -> &'static str {
        match self {
            Skip::TooLarge => "it is larger than max_file_kb",
            Skip::Binary => "it is a binary file",
            Skip::Internal => "it is a file of the local history store",
            Skip::NotUtf8Path => "its path is not UTF-8",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    Skipped(Skip),
    /// The bytes are those of the file's last entry already.
    Same,
    Added(Entry),
    /// An automatic snapshot that replaced the file's last entry, an
    /// automatic one of the same run, or a snapshot that relabelled an
    /// unsaved snapshot of the same text.
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

/// The entries of `key`, oldest first by time (a [`rename`] appends older
/// entries after newer ones), with those still stored under `given` (the
/// path as opened, when it differs: an index not yet migrated by
/// [`prune`]).
fn entries_of(dir: &Path, key: &str, given: Option<&str>) -> Vec<Entry> {
    let mut v = read_entries(&index_file(dir, key), key);
    if let Some(given) = given.filter(|g| *g != key) {
        v.extend(read_entries(&index_file(dir, given), given));
    }
    // Stable: entries at the same time keep their file order.
    v.sort_by_key(|e| e.at);
    v
}

/// Snapshot `bytes` as the content of `path` (stored under its [`key`]) at
/// `at_ms`, unless it is too large (over `max_bytes`), binary, a file of
/// the store, or the same as the file's last entry. An automatic snapshot
/// replaces the last entry when that is an automatic one of the same run,
/// and any other snapshot of an unsaved snapshot's text relabels it (see
/// the module doc). A label (`Kind::pinned`) is always added: never `Same`,
/// never replacing an entry.
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
    let same = !kind.pinned() && last.is_some_and(|e| e.blob == id);
    // A save of the text an unsaved snapshot holds relabels that snapshot.
    let promote = same && last.is_some_and(|e| e.kind == Kind::Unsaved && kind != Kind::Unsaved);
    if same && !promote {
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
    let in_run =
        |p: &Entry| kind.automatic() && p.kind.automatic() && at_ms.saturating_sub(p.run_start()) < AUTOSAVE_RUN.as_millis() as u64;
    // An entry still under the path as given (not yet migrated) is left be.
    let prev = last.filter(|p| p.path == key && (promote || in_run(p)));
    if let Some(prev) = prev {
        entry.since = in_run(prev).then(|| prev.run_start());
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

/// What a [`rename`] carried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Carried {
    /// Files whose history was carried (one, or those under a folder).
    pub files: usize,
    /// Entries copied, markers not counted.
    pub entries: usize,
    /// Another instance held `prune.lock`: the lines were appended without it.
    pub unlocked: bool,
    /// Labels moved off the old key (0 when unlocked: they stay copied).
    pub labels: usize,
}

/// One file's history to carry.
struct Moved {
    /// The old path as the caller named it (the marker's detail).
    old_given: PathBuf,
    new_key: String,
    new_given: PathBuf,
    /// The old key's entries, oldest first.
    old: Vec<Entry>,
}

/// Carry the history of `from` to `to` after `from` was renamed or moved
/// there (a file, or every file under a folder): the old key's entries
/// are appended to the new key's index unchanged but for `path`
/// (blobs are shared), those the new key has already left out, then a
/// `Kind::Renamed` entry at `at_ms` whose detail is the old path and whose
/// text is the file's now (`read`, within `max_bytes` and not binary)
/// or else the newest carried one. The old key keeps its entries, except
/// its labels, which move: they are dropped from the old key under
/// `prune.lock` (and stay copied when another instance holds it).
pub fn rename(
    dir: &Path,
    from: &Path,
    to: &Path,
    at_ms: u64,
    max_bytes: u64,
    read: impl Fn(&Path) -> Option<Vec<u8>>,
) -> io::Result<Carried> {
    if from.starts_with(dir) || to.starts_with(dir) {
        return Ok(Carried::default());
    }
    let (from_canon, to_canon) = (key(from), key(to));
    if to_canon.starts_with(key(dir)) {
        return Ok(Carried::default());
    }
    let (Some(from_key), Some(to_key)) = (from_canon.to_str(), to_canon.to_str()) else { return Ok(Carried::default()) };
    if from_key == to_key {
        return Ok(Carried::default());
    }
    let mut moved = Vec::new();
    if std::fs::symlink_metadata(to).is_ok_and(|m| m.is_dir()) {
        for (k, old) in histories_under(dir, Path::new(from_key)) {
            let Ok(rest) = Path::new(&k).strip_prefix(from_key) else { continue };
            let Some(new_key) = to_canon.join(rest).to_str().map(str::to_string) else { continue };
            moved.push(Moved { old_given: from.join(rest), new_key, new_given: to.join(rest), old });
        }
    } else {
        let old = entries_of(dir, from_key, from.to_str());
        if !old.is_empty() {
            moved.push(Moved { old_given: from.to_path_buf(), new_key: to_key.to_string(), new_given: to.to_path_buf(), old });
        }
    }
    if moved.is_empty() {
        return Ok(Carried::default());
    }
    let lock = PruneLock::take(dir)?.or_else(|| {
        std::thread::sleep(LOCK_RETRY);
        PruneLock::take(dir).ok().flatten()
    });
    let mut carried = Carried { unlocked: lock.is_none(), ..Default::default() };
    std::fs::create_dir_all(index_dir(dir))?;
    for m in &moved {
        carried.entries += carry(dir, m, at_ms, max_bytes, &read)?;
        carried.files += 1;
    }
    // Labels move: the old key's copies would be pinned where no one sees them.
    if lock.is_some() {
        for m in &moved {
            carried.labels += drop_labels(dir, &m.old)?;
        }
    }
    drop(lock);
    Ok(carried)
}

/// Drop the labels among `old` (entries of one file, from any of its index
/// files) from their index files; returns how many lines went. Only under
/// `prune.lock`.
fn drop_labels(dir: &Path, old: &[Entry]) -> io::Result<usize> {
    let labels: Vec<&Entry> = old.iter().filter(|e| e.kind.pinned()).collect();
    let mut paths: Vec<&str> = labels.iter().map(|e| e.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    let mut dropped = 0;
    for path in paths {
        dropped += drop_lines(&index_file(dir, path), |x| labels.contains(&x))?;
    }
    Ok(dropped)
}

/// What a [`remove_entry`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// The entry's line was removed.
    Done,
    /// No such line (already removed, by another instance perhaps).
    Gone,
    /// Another instance holds `prune.lock`: nothing was done.
    Locked,
}

/// Remove `entry` (a label, as listed: `path` is its key) from its index
/// file under `prune.lock`; other lines stay as they are. Its blob is left
/// to prune's gc. `Locked` when another instance holds the lock after one retry.
pub fn remove_entry(dir: &Path, entry: &Entry) -> io::Result<Removal> {
    if !dir.is_dir() {
        return Ok(Removal::Gone);
    }
    let lock = PruneLock::take(dir)?.or_else(|| {
        std::thread::sleep(LOCK_RETRY);
        PruneLock::take(dir).ok().flatten()
    });
    let Some(_lock) = lock else { return Ok(Removal::Locked) };
    Ok(if drop_lines(&index_file(dir, &entry.path), |e| e == entry)? > 0 { Removal::Done } else { Removal::Gone })
}

/// Drop the lines of index file `file` that parse to an entry `drop`
/// accepts; lines that do not parse and every other line stay verbatim. A
/// file left empty is removed. Returns how many lines went. Only under
/// `prune.lock` (see [`write_index`]).
fn drop_lines(file: &Path, drop: impl Fn(&Entry) -> bool) -> io::Result<usize> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut dropped = 0;
    for l in text.lines() {
        match serde_json::from_str::<Entry>(l) {
            Ok(e) if drop(&e) => dropped += 1,
            _ => kept.push(l),
        }
    }
    if dropped == 0 {
        return Ok(0);
    }
    if kept.iter().all(|l| l.trim().is_empty()) {
        match std::fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => return Ok(dropped),
        }
    }
    let mut text = kept.join("\n");
    text.push('\n');
    write_index(file, &text)?;
    Ok(dropped)
}

/// Every key strictly under `prefix` (component-wise) with its entries,
/// oldest first: one scan of the whole index, as [`prune`] does.
fn histories_under(dir: &Path, prefix: &Path) -> BTreeMap<String, Vec<Entry>> {
    let mut out: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(index_dir(dir)) else { return out };
    for e in rd.flatten() {
        let file = e.path();
        if file.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        for e in text.lines().filter_map(|l| serde_json::from_str::<Entry>(l).ok()) {
            if Path::new(&e.path).strip_prefix(prefix).is_ok_and(|rest| !rest.as_os_str().is_empty()) {
                out.entry(e.path.clone()).or_default().push(e);
            }
        }
    }
    for v in out.values_mut() {
        // Stable: entries at the same time keep their file order.
        v.sort_by_key(|e| e.at);
    }
    out
}

/// Append `m`'s entries the new key does not have yet (and no "Renamed
/// from" its own path), then its `Renamed` marker; returns how many entries
/// were copied.
fn carry(dir: &Path, m: &Moved, at_ms: u64, max_bytes: u64, read: &impl Fn(&Path) -> Option<Vec<u8>>) -> io::Result<usize> {
    let new_given = m.new_given.to_str();
    let existing = entries_of(dir, &m.new_key, new_given);
    let have: HashSet<(u64, &str, Kind, Option<&str>)> =
        existing.iter().map(|e| (e.at, e.blob.as_str(), e.kind, e.detail.as_deref())).collect();
    let copies: Vec<Entry> = m
        .old
        .iter()
        .filter(|e| !have.contains(&(e.at, e.blob.as_str(), e.kind, e.detail.as_deref())))
        .filter(|e| !(e.kind == Kind::Renamed && e.detail.is_some() && e.detail.as_deref() == new_given))
        .map(|e| Entry { path: m.new_key.clone(), ..e.clone() })
        .collect();
    if copies.is_empty()
        && existing.last().is_some_and(|l| {
            l.kind == Kind::Renamed
                && l.detail.as_deref() == m.old_given.to_str()
                && at_ms.saturating_sub(l.at) < RENAME_ECHO.as_millis() as u64
        })
    {
        return Ok(0);
    }
    let now = read(&m.new_given).filter(|b| b.len() as u64 <= max_bytes && !crate::git::diff::is_binary(b));
    let (blob, len, lines) = match now {
        Some(bytes) => {
            let id = blob_id(&bytes)?;
            write_blob(dir, &id, &bytes)?;
            (id, bytes.len() as u64, count_lines(&bytes))
        }
        None => {
            // `old` is never empty.
            let last = m.old.last().ok_or_else(|| io::Error::other("nothing to carry"))?;
            (last.blob.clone(), last.len, last.lines)
        }
    };
    let marker = Entry {
        v: 1,
        path: m.new_key.clone(),
        at: at_ms,
        blob,
        len,
        lines,
        kind: Kind::Renamed,
        detail: m.old_given.to_str().map(Into::into),
        since: None,
    };
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(index_file(dir, &m.new_key))?;
    for e in copies.iter().chain([&marker]) {
        let mut line = serde_json::to_string(e).map_err(io::Error::other)?;
        line.push('\n');
        // One write per line, as `record` appends.
        f.write_all(line.as_bytes())?;
    }
    Ok(copies.len())
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

/// `write_blob`, returning the file it wrote (tests).
#[cfg(test)]
pub(crate) fn write_blob_for_test(dir: &Path, id: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    write_blob(dir, id, bytes)?;
    Ok(if zblob_file(dir, id).is_file() { zblob_file(dir, id) } else { blob_file(dir, id) })
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

/// One file's history in brief, as the project view needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyHistory {
    pub key: String,
    /// The latest entry by time.
    pub newest: Entry,
    /// How many entries.
    pub count: usize,
    /// Its labels (`Kind::pinned`), oldest first.
    pub labels: Vec<Entry>,
    /// The old paths of its `Renamed` markers, and when.
    pub renames: Vec<(String, u64)>,
}

impl KeyHistory {
    /// Fold `other` (the same key in another index file) in.
    fn merge(&mut self, other: &KeyHistory) {
        if other.newest.at >= self.newest.at {
            self.newest = other.newest.clone();
        }
        self.count += other.count;
        self.labels.extend(other.labels.iter().cloned());
        self.labels.sort_by_key(|e| e.at);
        self.renames.extend(other.renames.iter().cloned());
    }
}

/// When an index file last changed: a rewrite goes through a rename, so on
/// unix the inode tells it apart even with the same size and mtime.
type Stamp = (Option<SystemTime>, u64, u64);

fn stamp(meta: &std::fs::Metadata) -> Stamp {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let ino = 0;
    (meta.modified().ok(), meta.len(), ino)
}

/// The index in brief, kept between [`recent`] scans by the worker: an
/// index file is read again only when its stamp changed, and only a
/// [`KeyHistory`] per key is kept (not the entries).
#[derive(Debug, Default)]
pub struct IndexCache {
    files: HashMap<PathBuf, (Stamp, Vec<KeyHistory>)>,
}

impl IndexCache {
    /// Read the index files that changed since the last call; forget those
    /// gone.
    fn refresh(&mut self, dir: &Path) {
        let mut seen = HashSet::new();
        if let Ok(rd) = std::fs::read_dir(index_dir(dir)) {
            for e in rd.flatten() {
                let file = e.path();
                if file.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&file) else { continue };
                let st = stamp(&meta);
                seen.insert(file.clone());
                if self.files.get(&file).is_some_and(|(s, _)| *s == st) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&file) else { continue };
                self.files.insert(file, (st, summarize(&text)));
            }
        }
        self.files.retain(|f, _| seen.contains(f));
    }

    fn keys(&self) -> impl Iterator<Item = &KeyHistory> {
        self.files.values().flat_map(|(_, keys)| keys)
    }
}

/// One [`KeyHistory`] per key of an index file's text (corrupt lines left out).
fn summarize(text: &str) -> Vec<KeyHistory> {
    let mut by_key: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for e in text.lines().filter_map(|l| serde_json::from_str::<Entry>(l).ok()) {
        by_key.entry(e.path.clone()).or_default().push(e);
    }
    by_key
        .into_iter()
        .filter_map(|(key, mut v)| {
            // Stable: entries at the same time keep their file order.
            v.sort_by_key(|e| e.at);
            let labels = v.iter().filter(|e| e.kind.pinned()).cloned().collect();
            let renames = v.iter().filter(|e| e.kind == Kind::Renamed).filter_map(|e| Some((e.detail.clone()?, e.at))).collect();
            let count = v.len();
            Some(KeyHistory { key, newest: v.pop()?, count, labels, renames })
        })
        .collect()
}

/// Where a file with local history is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    Present,
    /// Gone from disk.
    Deleted,
    /// Gone by a Project tree rename (its history was carried there); the
    /// new path, under the scope as asked when it is in it.
    Renamed(PathBuf),
}

/// A file under a scope with local history ([`recent`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFile {
    /// Under the scope as asked (`scope.join(rest of the key)`).
    pub path: PathBuf,
    pub newest: Entry,
    pub count: usize,
    /// Its labels, oldest first.
    pub labels: Vec<Entry>,
    pub state: FileState,
}

/// The files strictly under `scope` (by key, component-wise) that have
/// local history, newest change first: one scan of the index through
/// `cache`, a stat per file for the deleted check, and a canonicalize per
/// rename marker of a missing file.
pub fn recent(dir: &Path, scope: &Path, cache: &mut IndexCache) -> Vec<RecentFile> {
    cache.refresh(dir);
    let scope_key = key(scope);
    let mut under: BTreeMap<&str, KeyHistory> = BTreeMap::new();
    for k in cache.keys() {
        if !Path::new(&k.key).strip_prefix(&scope_key).is_ok_and(|rest| !rest.as_os_str().is_empty()) {
            continue;
        }
        match under.get_mut(k.key.as_str()) {
            Some(have) => have.merge(k),
            None => {
                under.insert(&k.key, k.clone());
            }
        }
    }
    let shown = |k: &str| match Path::new(k).strip_prefix(&scope_key) {
        Ok(rest) => scope.join(rest),
        Err(_) => PathBuf::from(k),
    };
    let missing: HashSet<&str> = under.keys().copied().filter(|k| std::fs::symlink_metadata(k).is_err()).collect();
    // Old key -> (new key, when), for the missing files a rename left.
    let mut moved: HashMap<String, (&str, u64)> = HashMap::new();
    if !missing.is_empty() {
        for k in cache.keys() {
            for (detail, at) in &k.renames {
                let old = if missing.contains(detail.as_str()) {
                    detail.clone()
                } else {
                    match key(Path::new(detail)).to_str() {
                        Some(o) if missing.contains(o) => o.to_string(),
                        _ => continue,
                    }
                };
                if moved.get(&old).is_none_or(|(_, t)| *t <= *at) {
                    moved.insert(old, (&k.key, *at));
                }
            }
        }
    }
    let mut out: Vec<RecentFile> = under
        .into_iter()
        .map(|(k, h)| {
            let state = if !missing.contains(k) {
                FileState::Present
            } else {
                match moved.get(k) {
                    Some((to, at)) if *at >= h.newest.at => FileState::Renamed(shown(to)),
                    _ => FileState::Deleted,
                }
            };
            RecentFile { path: shown(k), newest: h.newest, count: h.count, labels: h.labels, state }
        })
        .collect();
    out.sort_by(|a, b| b.newest.at.cmp(&a.newest.at).then_with(|| a.path.cmp(&b.path)));
    out
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

/// An index line this build cannot read as an [`Entry`] (a newer jetl's kind) but that is one: kept verbatim by the prune.
#[derive(Deserialize)]
struct ForeignLine {
    #[allow(dead_code)]
    v: u32,
    at: u64,
    #[allow(dead_code)]
    kind: String,
    #[serde(default)]
    blob: Option<String>,
}

/// One index file read for a prune.
struct IndexFile {
    file: PathBuf,
    entries: Vec<Entry>,
    /// Lines of an unknown kind as `(at, line)`, kept verbatim.
    foreign: Vec<(u64, String)>,
    /// Lines that did not parse (dropped when the file is rewritten).
    corrupt: bool,
    changed: bool,
}

/// Move entries stored under a path that is not their [`key`] (written by
/// an older jetl, through a symlink) under the key, then drop entries older
/// than `keep_ms` before `now_ms` (labels excepted), then the oldest
/// entries other than labels (of any file), and then the oldest labels,
/// while the bytes on disk of the blobs still referenced exceed `max_bytes`;
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
            let mut entries = Vec::new();
            let mut foreign = Vec::new();
            for l in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(e) = serde_json::from_str::<Entry>(l) {
                    entries.push(e);
                } else if let Ok(f) = serde_json::from_str::<ForeignLine>(l) {
                    foreign.push((f.at, l.to_string()));
                } else {
                    corrupt = true;
                }
            }
            files.push(IndexFile { file, entries, foreign, corrupt, changed: false });
        }
    }
    let mut stats = PruneStats { moved: migrate_keys(dir, &mut files), ..Default::default() };
    // By age.
    let cutoff = now_ms.saturating_sub(keep_ms);
    for f in &mut files {
        let before = f.entries.len();
        f.entries.retain(|e| e.at >= cutoff || e.kind.pinned());
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
    // Foreign lines are never cut, so their blobs stay referenced.
    let foreign_blobs: Vec<String> =
        files.iter().flat_map(|f| &f.foreign).filter_map(|(_, l)| serde_json::from_str::<ForeignLine>(l).ok()?.blob).collect();
    for id in &foreign_blobs {
        refs.entry(id.as_str()).or_insert_with(|| (0, stored_len(dir, id))).0 += 1;
    }
    let mut total: u64 = refs.values().map(|(_, len)| len).sum();
    let mut drop_set: HashSet<(usize, usize)> = HashSet::new();
    if total > max_bytes {
        // Unlabelled entries first, oldest first; labels only when they alone
        // exceed the cap.
        let mut order: Vec<(bool, u64, usize, usize)> = files
            .iter()
            .enumerate()
            .flat_map(|(fi, f)| f.entries.iter().enumerate().map(move |(ei, e)| (e.kind.pinned(), e.at, fi, ei)))
            .collect();
        order.sort_unstable();
        for (_, _, fi, ei) in order {
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
    for f in files.iter_mut().filter(|f| f.changed || (f.corrupt && !(f.entries.is_empty() && f.foreign.is_empty()))) {
        // In time order again (a rename appends older entries after newer ones).
        f.entries.sort_by_key(|e| e.at);
        rewrite_index(&f.file, &f.entries, &f.foreign)?;
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
            files.push(IndexFile { file, entries: Vec::new(), foreign: Vec::new(), corrupt: false, changed: false });
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

/// Replace an index file by `entries` and the `foreign` lines (verbatim),
/// merged by time, entries first at equal times (through a temp file and a
/// rename); none of either left removes it.
fn rewrite_index(file: &Path, entries: &[Entry], foreign: &[(u64, String)]) -> io::Result<()> {
    if entries.is_empty() && foreign.is_empty() {
        return match std::fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let mut lines: Vec<(u64, u8, String)> = Vec::with_capacity(entries.len() + foreign.len());
    for e in entries {
        lines.push((e.at, 0, serde_json::to_string(e).map_err(io::Error::other)?));
    }
    lines.extend(foreign.iter().map(|(at, l)| (*at, 1, l.clone())));
    lines.sort_by_key(|(at, rank, _)| (*at, *rank));
    let mut text = String::new();
    for (_, _, l) in lines {
        text.push_str(&l);
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

    fn unsaved(dir: &Path, path: &str, text: &str, at: u64) -> Recorded {
        record(dir, Path::new(path), text.as_bytes(), Kind::Unsaved, None, at, MB).unwrap()
    }

    /// `(label, at, since)` of `path`'s entries, newest first.
    fn labels(dir: &Path, path: &str) -> Vec<(String, u64, Option<u64>)> {
        list(dir, Path::new(path)).into_iter().map(|e| (e.label(), e.at, e.since)).collect()
    }

    #[test]
    fn unsaved_snapshots_and_autosaves_share_a_run() {
        let dir = temp("unsavedrun");
        let p = "/p/a.txt";
        assert!(matches!(unsaved(&dir, p, "a", 0), Recorded::Added(_)));
        let Recorded::Replaced(e) = unsaved(&dir, p, "b", MIN) else { panic!("replaced") };
        assert_eq!(e.since, Some(0));
        let Recorded::Replaced(e) = auto(&dir, p, "c", 2 * MIN) else { panic!("replaced") };
        assert_eq!((e.kind, e.since), (Kind::Autosave, Some(0)));
        let Recorded::Replaced(e) = unsaved(&dir, p, "d", 3 * MIN) else { panic!("replaced") };
        assert_eq!((e.kind, e.since), (Kind::Unsaved, Some(0)));
        // A save ends the run.
        assert!(matches!(rec(&dir, p, "e", 4 * MIN), Recorded::Added(_)));
        let Recorded::Added(e) = unsaved(&dir, p, "f", 9 * MIN / 2) else { panic!("added") };
        assert_eq!(e.since, None);
        // Past the window measured from f.
        assert!(matches!(unsaved(&dir, p, "g", 10 * MIN), Recorded::Added(_)));
        let u = || "Unsaved changes".to_string();
        assert_eq!(
            labels(&dir, p),
            [(u(), 10 * MIN, None), (u(), 9 * MIN / 2, None), ("Saved".into(), 4 * MIN, None), (u(), 3 * MIN, Some(0))]
        );
        assert_eq!(runs(&dir, p)[3].2, "d");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_of_the_unsaved_text_relabels_its_snapshot() {
        let dir = temp("unsavedpromote");
        let p = "/p/a.txt";
        unsaved(&dir, p, "x", 1);
        let Recorded::Replaced(e) = rec(&dir, p, "x", 2) else { panic!("replaced") };
        assert_eq!((e.kind, e.since), (Kind::Save, None));
        assert_eq!(labels(&dir, p), [("Saved".into(), 2, None)]);
        assert_eq!(read_blob(&dir, &e.blob).unwrap(), b"x");
        // The same text again, saved or not: nothing new.
        assert_eq!(rec(&dir, p, "x", 3), Recorded::Same);
        assert_eq!(unsaved(&dir, p, "x", 4), Recorded::Same);
        // An autosave of the unsaved text keeps the run.
        unsaved(&dir, p, "y", 5);
        let Recorded::Replaced(e) = auto(&dir, p, "y", 6) else { panic!("replaced") };
        assert_eq!((e.kind, e.since), (Kind::Autosave, Some(5)));
        assert_eq!(labels(&dir, p).len(), 2);
        // Another instance prunes: the save appends next to the snapshot.
        std::fs::write(dir.join("prune.lock"), "1").unwrap();
        assert!(matches!(unsaved(&dir, p, "z", 7), Recorded::Added(_)));
        let blobs = blob_count(&dir);
        assert!(matches!(rec(&dir, p, "z", 8), Recorded::Added(_)));
        assert_eq!(blob_count(&dir), blobs, "one blob for both");
        let l = list(&dir, Path::new(p));
        assert_eq!((l[0].label(), l[1].label()), ("Saved".into(), "Unsaved changes".into()));
        assert_eq!(l[0].blob, l[1].blob);
        assert!(dir.join("prune.lock").exists(), "not ours to remove");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn now(_: &Path) -> Option<Vec<u8>> {
        Some(b"now".to_vec())
    }

    fn mv(dir: &Path, from: &str, to: &str, at: u64) -> Carried {
        rename(dir, Path::new(from), Path::new(to), at, MB, now).unwrap()
    }

    fn text_of(dir: &Path, e: &Entry) -> String {
        String::from_utf8(read_blob(dir, &e.blob).unwrap()).unwrap()
    }

    #[test]
    fn a_rename_carries_the_history_and_marks_it() {
        let dir = temp("rename");
        rec(&dir, "/p/a.txt", "1", 1);
        rec(&dir, "/p/a.txt", "2", 2);
        assert_eq!(mv(&dir, "/p/a.txt", "/p/b.txt", 10), Carried { files: 1, entries: 2, unlocked: false, labels: 0 });
        assert_eq!(
            labels(&dir, "/p/b.txt"),
            [("Renamed from /p/a.txt".into(), 10, None), ("Saved".into(), 2, None), ("Saved".into(), 1, None)]
        );
        let b = list(&dir, Path::new("/p/b.txt"));
        assert_eq!(text_of(&dir, &b[0]), "now");
        assert!(b.iter().all(|e| e.path == "/p/b.txt"));
        assert_eq!(labels(&dir, "/p/a.txt").len(), 2, "the old key keeps its entries");
        // Copies share their blobs.
        assert_eq!(blob_count(&dir), 3);
        let bytes: u64 = b.iter().map(|e| stored_len(&dir, &e.blob)).sum();
        assert_eq!(prune(&dir, 20, 100, MB).unwrap().bytes, bytes);
        // The marker's text saved again is nothing new; it ends an autosave run.
        assert_eq!(rec(&dir, "/p/b.txt", "now", 11), Recorded::Same);
        assert!(matches!(auto(&dir, "/p/b.txt", "x", 12), Recorded::Added(_)));
        assert!(!dir.join("prune.lock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_echo_writes_one_marker_within_the_window() {
        let dir = temp("rename-echo");
        rec(&dir, "/p/a.txt", "1", 1);
        mv(&dir, "/p/a.txt", "/p/b.txt", 10);
        let marks = |dir: &Path| labels(dir, "/p/b.txt").iter().filter(|l| l.0.starts_with("Renamed from")).count();
        assert_eq!(marks(&dir), 1);
        let again = mv(&dir, "/p/a.txt", "/p/b.txt", 10 + 5_000);
        assert_eq!((again.entries, marks(&dir)), (0, 1), "the echo adds nothing");
        mv(&dir, "/p/a.txt", "/p/b.txt", 10 + RENAME_ECHO.as_millis() as u64);
        assert_eq!(marks(&dir), 2, "past the window a rename marks again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_without_history_or_to_itself_does_nothing() {
        let dir = temp("rename-none");
        assert_eq!(mv(&dir, "/p/a.txt", "/p/b.txt", 1), Carried::default());
        assert!(!index_dir(&dir).exists());
        rec(&dir, "/p/a.txt", "1", 1);
        assert_eq!(mv(&dir, "/p/a.txt", "/p/a.txt", 2), Carried::default());
        assert_eq!(labels(&dir, "/p/a.txt").len(), 1);
        // Files of the store itself.
        let inside = dir.join("index/x.jsonl");
        assert_eq!(rename(&dir, &inside, Path::new("/p/c.txt"), 3, MB, now).unwrap(), Carried::default());
        assert_eq!(rename(&dir, Path::new("/p/a.txt"), &inside, 3, MB, now).unwrap(), Carried::default());
        assert_eq!(std::fs::read_dir(index_dir(&dir)).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn renaming_a_symlink_keeps_its_targets_history() {
        let (hist, real, link, _) = linked("rename-link");
        rec(&hist, link.to_str().unwrap(), "1", 1);
        let link2 = link.with_file_name("link2.txt");
        std::fs::rename(&link, &link2).unwrap();
        let read = |p: &Path| std::fs::read(p).ok();
        assert_eq!(rename(&hist, &link, &link2, 2, MB, read).unwrap(), Carried::default());
        assert_eq!(ats(&hist, &link2), [1]);
        assert_eq!(ats(&hist, &real.join("a.txt")), [1]);
        let _ = std::fs::remove_dir_all(hist.parent().unwrap());
    }

    #[test]
    fn renaming_back_and_forth_does_not_duplicate() {
        let dir = temp("rename-back");
        rec(&dir, "/p/a.txt", "1", 1);
        rec(&dir, "/p/a.txt", "2", 2);
        mv(&dir, "/p/a.txt", "/p/b.txt", 10);
        rec(&dir, "/p/b.txt", "3", 11);
        assert_eq!(mv(&dir, "/p/b.txt", "/p/a.txt", 20).entries, 1);
        assert_eq!(
            labels(&dir, "/p/a.txt"),
            [("Renamed from /p/b.txt".into(), 20, None), ("Saved".into(), 11, None), ("Saved".into(), 2, None), ("Saved".into(), 1, None)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_rename_carries_every_file_under_it() {
        let root = temp("rename-folder");
        let hist = root.join("history");
        let p = root.join("p");
        std::fs::create_dir_all(p.join("d/sub")).unwrap();
        let p = std::fs::canonicalize(&p).unwrap();
        let (d, e) = (p.join("d"), p.join("e"));
        for (f, text) in [("d/x.txt", "x"), ("d/sub/y.txt", "y"), ("dx.txt", "dx")] {
            std::fs::write(p.join(f), text).unwrap();
            record(&hist, &p.join(f), text.as_bytes(), Kind::Save, None, 1, MB).unwrap();
        }
        std::fs::write(d.join("x.txt"), "x2").unwrap();
        let indexes = std::fs::read_dir(index_dir(&hist)).unwrap().count();
        std::fs::rename(&d, &e).unwrap();
        let c = rename(&hist, &d, &e, 5, MB, |f| std::fs::read(f).ok()).unwrap();
        assert_eq!((c.files, c.entries, c.unlocked), (2, 2, false));
        for (rel, text) in [("x.txt", "x2"), ("sub/y.txt", "y")] {
            let l = list(&hist, &e.join(rel));
            assert_eq!(l.len(), 2, "{rel}");
            assert_eq!((l[0].kind, l[0].detail.clone()), (Kind::Renamed, Some(d.join(rel).to_str().unwrap().into())));
            assert_eq!(text_of(&hist, &l[0]), text);
            assert_eq!(list(&hist, &d.join(rel)).len(), 1, "the old key still lists");
        }
        assert_eq!(list(&hist, &p.join("dx.txt")).len(), 1);
        assert_eq!(std::fs::read_dir(index_dir(&hist)).unwrap().count(), indexes + 2, "nothing for dx.txt");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn carried_entries_merge_by_time_with_the_new_paths_own() {
        let dir = temp("rename-merge");
        rec(&dir, "/p/a.txt", "a1", 1);
        rec(&dir, "/p/b.txt", "b5", 5);
        rec(&dir, "/p/a.txt", "a10", 10);
        rec(&dir, "/p/b.txt", "b15", 15);
        mv(&dir, "/p/a.txt", "/p/b.txt", 20);
        let ats = |dir: &Path| list(dir, Path::new("/p/b.txt")).iter().map(|e| e.at).collect::<Vec<_>>();
        assert_eq!(ats(&dir), [20, 15, 10, 5, 1]);
        let file = index_file(&dir, "/p/b.txt");
        let on_disk = || read_entries(&file, "/p/b.txt").iter().map(|e| e.at).collect::<Vec<_>>();
        assert_eq!(on_disk(), [5, 15, 1, 10, 20]);
        // A prune rewrites the file (here for its corrupt line) in time order.
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(f, "{{not json").unwrap();
        prune(&dir, 30, 100, MB).unwrap();
        assert_eq!(on_disk(), [1, 5, 10, 15, 20]);
        // The marker is the last line: an autosave run after it merges.
        auto(&dir, "/p/b.txt", "c", 21);
        assert!(matches!(auto(&dir, "/p/b.txt", "d", 22), Recorded::Replaced(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_marker_falls_back_to_the_newest_carried_snapshot() {
        let dir = temp("rename-fallback");
        rec(&dir, "/p/a.txt", "1", 1);
        let newest = added(rec(&dir, "/p/a.txt", "2", 2));
        let big = "x".repeat(10);
        let reads: [Option<Vec<u8>>; 3] = [None, Some(b"a\0".to_vec()), Some(big.into_bytes())];
        for (i, bytes) in reads.into_iter().enumerate() {
            let blobs = blob_count(&dir);
            let to = format!("/p/b{i}.txt");
            let c = rename(&dir, Path::new("/p/a.txt"), Path::new(&to), 10, 4, |_| bytes.clone()).unwrap();
            assert_eq!(c.entries, 2);
            let l = list(&dir, Path::new(&to));
            assert_eq!((l[0].kind, &l[0].blob, l[0].len), (Kind::Renamed, &newest.blob, 1));
            assert_eq!(blob_count(&dir), blobs);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_appends_while_another_instance_prunes() {
        let dir = temp("rename-lock");
        rec(&dir, "/p/a.txt", "1", 1);
        std::fs::write(dir.join("prune.lock"), "1").unwrap();
        assert_eq!(mv(&dir, "/p/a.txt", "/p/b.txt", 10), Carried { files: 1, entries: 1, unlocked: true, labels: 0 });
        assert_eq!(labels(&dir, "/p/b.txt").len(), 2);
        assert!(dir.join("prune.lock").exists(), "not ours to remove");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renamed_kind_name_and_labels() {
        assert_eq!(serde_json::to_string(&Kind::Renamed).unwrap(), "\"renamed\"");
        assert_eq!(serde_json::from_str::<Kind>("\"renamed\"").unwrap(), Kind::Renamed);
        assert!(!Kind::Renamed.automatic());
        assert_eq!(Kind::Renamed.label(Some("/p/a")), "Renamed from /p/a");
        assert_eq!(Kind::Renamed.label(None), "Renamed");
        assert_eq!(Kind::SaveAs.label(None), "Saved As");
        assert_eq!(Kind::SaveAs.label(Some("/p/a")), "Saved As from /p/a");
        let e = |kind, detail: Option<&str>| Entry {
            v: 1,
            path: "/p/b".into(),
            at: 1,
            blob: String::new(),
            len: 0,
            lines: 0,
            kind,
            detail: detail.map(Into::into),
            since: None,
        };
        let root = Path::new("/p");
        assert_eq!(e(Kind::Renamed, Some("/p/a")).label_in(root), "Renamed from a");
        assert_eq!(e(Kind::Renamed, Some("/p/d/x.txt")).label_in(root), "Renamed from d/x.txt");
        assert_eq!(e(Kind::Renamed, Some("/q/a")).label_in(root), "Renamed from /q/a");
        assert_eq!(e(Kind::SaveAs, Some("/p/a")).label_in(root), "Saved As from a");
        assert_eq!(e(Kind::SaveAs, None).label_in(root), "Saved As");
        assert_eq!(e(Kind::BeforeGit, Some("/p/a")).label_in(root), "Before /p/a");
        assert_eq!(e(Kind::Save, None).label_in(root), "Saved");
    }

    #[test]
    fn unsaved_kind_name_and_label() {
        assert_eq!(serde_json::to_string(&Kind::Unsaved).unwrap(), "\"unsaved\"");
        assert_eq!(serde_json::from_str::<Kind>("\"unsaved\"").unwrap(), Kind::Unsaved);
        assert_eq!(Kind::Unsaved.label(None), "Unsaved changes");
        assert!(Kind::Unsaved.automatic() && Kind::Autosave.automatic() && !Kind::Save.automatic());
    }

    fn lab(dir: &Path, path: &str, text: &str, name: &str, at: u64) -> Recorded {
        record(dir, Path::new(path), text.as_bytes(), Kind::Label, Some(name.into()), at, MB).unwrap()
    }

    #[test]
    fn a_label_is_never_same_and_shares_the_blob() {
        let dir = temp("label-same");
        let p = "/p/a.txt";
        rec(&dir, p, "a", 1);
        assert!(matches!(lab(&dir, p, "a", "v1", 2), Recorded::Added(_)));
        assert_eq!(blob_count(&dir), 1, "the saved text's blob is shared");
        assert!(matches!(lab(&dir, p, "a", "v2", 3), Recorded::Added(_)));
        assert_eq!(labels(&dir, p), [("Label: v2".into(), 3, None), ("Label: v1".into(), 2, None), ("Saved".into(), 1, None)]);
        // A save of the labelled text adds nothing (as after a Renamed marker).
        assert_eq!(rec(&dir, p, "a", 4), Recorded::Same);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_label_ends_a_run_and_never_replaces_an_unsaved_snapshot() {
        let dir = temp("label-run");
        let p = "/p/a.txt";
        unsaved(&dir, p, "x", 0);
        assert!(matches!(lab(&dir, p, "x", "l", 1), Recorded::Added(_)));
        assert_eq!(labels(&dir, p), [("Label: l".into(), 1, None), ("Unsaved changes".into(), 0, None)]);
        let Recorded::Added(e) = auto(&dir, p, "y", 2) else { panic!("added") };
        assert_eq!(e.since, None);
        let Recorded::Replaced(e) = auto(&dir, p, "z", 3) else { panic!("replaced") };
        assert_eq!(e.since, Some(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line like `e` but of a kind this build does not know.
    fn foreign_line(e: &Entry) -> String {
        let mut line = serde_json::to_string(e).unwrap();
        let from = format!("\"kind\":{}", serde_json::to_string(&e.kind).unwrap());
        assert!(line.contains(&from), "kind field in {line}");
        line = line.replace(&from, "\"kind\":\"before_teleport\"");
        line
    }

    #[test]
    fn prune_keeps_lines_of_unknown_kinds() {
        let dir = temp("foreign");
        let day = 86_400_000;
        let p = "/p/a.txt";
        rec(&dir, p, "old", 0);
        let fresh = added(rec(&dir, p, "fresh-and-foreign", 10 * day));
        let foreign = foreign_line(&fresh);
        let file = index_file(&dir, p);
        let old_line = std::fs::read_to_string(&file).unwrap().lines().next().unwrap().to_string();
        std::fs::write(&file, format!("{old_line}\nnot json\n{foreign}\n")).unwrap();
        age_blobs(&dir);
        // The day-0 save is age-cut, so the file is rewritten.
        let s = prune(&dir, 11 * day, 5 * day, MB).unwrap();
        assert_eq!(s.entries, 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{foreign}\n"));
        assert_eq!(s.blobs, 1, "only the old save's blob went");
        assert_eq!(blob_count(&dir), 1);
        assert_eq!(read_blob(&dir, &fresh.blob).unwrap(), b"fresh-and-foreign");
        // A size cut of zero does not take the foreign blob either.
        let s = prune(&dir, 11 * day, 5 * day, 0).unwrap();
        assert_eq!(s.blobs, 0);
        assert_eq!(read_blob(&dir, &fresh.blob).unwrap(), b"fresh-and-foreign");
        assert!(list(&dir, Path::new(p)).is_empty(), "lists keep skipping it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_of_only_foreign_lines_is_not_removed() {
        let dir = temp("foreign-only");
        let p = "/p/a.txt";
        let e = added(rec(&dir, p, "text", 5));
        let foreign = foreign_line(&e);
        let file = index_file(&dir, p);
        std::fs::write(&file, format!("garbage\n{foreign}\n")).unwrap();
        age_blobs(&dir);
        prune(&dir, 6, 100, MB).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{foreign}\n"));
        assert_eq!(blob_count(&dir), 1);
        // Untouched when nothing is corrupt or cut.
        prune(&dir, 6, 100, MB).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{foreign}\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_labels_past_keep_days_and_drops_them_last_by_size() {
        let dir = temp("label-prune");
        let day = 86_400_000;
        rec(&dir, "/p/o.txt", "old", 0);
        lab(&dir, "/p/a.txt", "kept", "v1", 0);
        let s = prune(&dir, 11 * day, 5 * day, MB).unwrap();
        assert_eq!(s.entries, 1);
        assert!(list(&dir, Path::new("/p/o.txt")).is_empty());
        assert_eq!(labels(&dir, "/p/a.txt"), [("Label: v1".into(), 0, None)]);
        let _ = std::fs::remove_dir_all(&dir);

        // Raw blobs (under MIN_COMPRESS): the stored length is the length.
        let dir = temp("label-prune-size");
        lab(&dir, "/p/a.txt", "LLLL", "a", 1);
        rec(&dir, "/p/b.txt", "ssssss", 5);
        lab(&dir, "/p/c.txt", "MMMMMMMM", "c", 3);
        // The save goes first, though newer than both labels.
        let s = prune(&dir, 6, 100, 12).unwrap();
        assert_eq!((s.entries, s.bytes), (1, 12));
        assert!(list(&dir, Path::new("/p/b.txt")).is_empty());
        // Labels alone over the cap: the oldest goes.
        let s = prune(&dir, 6, 100, 8).unwrap();
        assert_eq!((s.entries, s.bytes), (1, 8));
        assert!(list(&dir, Path::new("/p/a.txt")).is_empty());
        assert_eq!(labels(&dir, "/p/c.txt").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_entry_drops_only_that_line_under_the_lock() {
        let dir = temp("label-remove");
        let p = "/p/a.txt";
        let label = added(lab(&dir, p, "a", "v1", 1));
        let save = added(rec(&dir, p, "b", 2));
        // Another path's line (a hash collision) and garbage in the same file.
        let file = index_file(&dir, p);
        let mut other = save.clone();
        other.path = "/p/other".into();
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(&serde_json::to_string(&other).unwrap());
        text.push_str("\n{not json\n");
        std::fs::write(&file, &text).unwrap();
        assert_eq!(remove_entry(&dir, &label).unwrap(), Removal::Done);
        let after = std::fs::read_to_string(&file).unwrap();
        let expect = text.lines().skip(1).map(|l| format!("{l}\n")).collect::<String>();
        assert_eq!(after, expect, "every other line kept verbatim");
        assert_eq!(labels(&dir, p), [("Saved".into(), 2, None)]);
        assert!(!dir.join("prune.lock").exists(), "the lock is released");
        assert_eq!(remove_entry(&dir, &label).unwrap(), Removal::Gone);
        // Another instance prunes: nothing changes.
        std::fs::write(dir.join("prune.lock"), "1").unwrap();
        assert_eq!(remove_entry(&dir, &save).unwrap(), Removal::Locked);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), after);
        assert!(dir.join("prune.lock").exists(), "not ours to remove");
        std::fs::remove_file(dir.join("prune.lock")).unwrap();
        // The only line: the index file goes.
        let only = added(lab(&dir, "/p/z.txt", "z", "v", 3));
        assert_eq!(remove_entry(&dir, &only).unwrap(), Removal::Done);
        assert!(!index_file(&dir, "/p/z.txt").exists());
        // A missing store is not created.
        let none = temp("label-remove-none");
        assert_eq!(remove_entry(&none, &only).unwrap(), Removal::Gone);
        assert!(!none.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_moves_labels_off_the_old_key() {
        let dir = temp("label-rename");
        rec(&dir, "/p/a.txt", "1", 1);
        lab(&dir, "/p/a.txt", "1", "v1", 2);
        assert_eq!(mv(&dir, "/p/a.txt", "/p/b.txt", 10), Carried { files: 1, entries: 2, unlocked: false, labels: 1 });
        assert!(labels(&dir, "/p/b.txt").iter().any(|(l, ..)| l == "Label: v1"));
        assert_eq!(labels(&dir, "/p/a.txt"), [("Saved".into(), 1, None)], "the old key keeps the rest");
        // Renaming back: exactly one label, under a.
        assert_eq!(mv(&dir, "/p/b.txt", "/p/a.txt", 20).labels, 1);
        let count = |p: &str| labels(&dir, p).iter().filter(|(l, ..)| l.starts_with("Label")).count();
        assert_eq!((count("/p/a.txt"), count("/p/b.txt")), (1, 0));
        // Another instance prunes: the labels stay copied.
        std::fs::write(dir.join("prune.lock"), "1").unwrap();
        let c = mv(&dir, "/p/a.txt", "/p/c.txt", 30);
        assert_eq!((c.unlocked, c.labels), (true, 0));
        assert_eq!((count("/p/a.txt"), count("/p/c.txt")), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_lists_the_files_under_a_scope_newest_first() {
        let root = temp("recent");
        let hist = root.join("history");
        let p = root.join("p");
        std::fs::create_dir_all(p.join("d/sub")).unwrap();
        let p = std::fs::canonicalize(&p).unwrap();
        let d = p.join("d");
        let mut cache = IndexCache::default();
        assert!(recent(&hist, &d, &mut cache).is_empty(), "no store yet");
        for (f, at) in [("d/a.txt", 3), ("d/sub/b.txt", 5), ("d/gone.txt", 4), ("dx.txt", 9), ("other.txt", 8)] {
            let path = p.join(f);
            std::fs::write(&path, f).unwrap();
            rec(&hist, path.to_str().unwrap(), f, at);
        }
        rec(&hist, d.join("a.txt").to_str().unwrap(), "a2", 6);
        lab(&hist, d.join("a.txt").to_str().unwrap(), "a2", "v1", 7);
        std::fs::remove_file(d.join("gone.txt")).unwrap();
        let r = recent(&hist, &d, &mut cache);
        let rows: Vec<(PathBuf, u64, usize, usize, FileState)> =
            r.iter().map(|f| (f.path.clone(), f.newest.at, f.count, f.labels.len(), f.state.clone())).collect();
        assert_eq!(
            rows,
            [
                (d.join("a.txt"), 7, 3, 1, FileState::Present),
                (d.join("sub/b.txt"), 5, 1, 0, FileState::Present),
                (d.join("gone.txt"), 4, 1, 0, FileState::Deleted),
            ],
            "a sibling with a common prefix and a file outside the scope are left out"
        );
        assert_eq!(r[0].newest.kind, Kind::Label);
        // The whole folder above: every file.
        assert_eq!(recent(&hist, &p, &mut cache).len(), 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recent_marks_a_file_renamed_away() {
        let root = temp("recent-renamed");
        let hist = root.join("history");
        let p = root.join("p");
        std::fs::create_dir_all(&p).unwrap();
        let p = std::fs::canonicalize(&p).unwrap();
        let (a, b) = (p.join("a.txt"), p.join("b.txt"));
        std::fs::write(&a, "a").unwrap();
        rec(&hist, a.to_str().unwrap(), "a", 1);
        std::fs::rename(&a, &b).unwrap();
        rename(&hist, &a, &b, 5, MB, |f| std::fs::read(f).ok()).unwrap();
        let mut cache = IndexCache::default();
        let state = |cache: &mut IndexCache, path: &Path| recent(&hist, &p, cache).into_iter().find(|f| f.path == path).map(|f| f.state);
        assert_eq!(state(&mut cache, &a), Some(FileState::Renamed(b.clone())));
        assert_eq!(state(&mut cache, &b), Some(FileState::Present));
        // A new file at the old path: present again; deleted after a later
        // snapshot: deleted, not renamed.
        std::fs::write(&a, "new").unwrap();
        rec(&hist, a.to_str().unwrap(), "new", 7);
        assert_eq!(state(&mut cache, &a), Some(FileState::Present));
        std::fs::remove_file(&a).unwrap();
        assert_eq!(state(&mut cache, &a), Some(FileState::Deleted));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_index_cache_reads_only_changed_files() {
        let dir = temp("recent-cache");
        let mut cache = IndexCache::default();
        rec(&dir, "/p/a.txt", "a", 1);
        rec(&dir, "/p/b.txt", "b", 2);
        let ats = |cache: &mut IndexCache| -> Vec<(String, u64, usize)> {
            recent(&dir, Path::new("/p"), cache).into_iter().map(|f| (f.path.display().to_string(), f.newest.at, f.count)).collect()
        };
        assert_eq!(ats(&mut cache), [("/p/b.txt".into(), 2, 1), ("/p/a.txt".into(), 1, 1)]);
        // An append and a rewrite (a label removed under the lock) show.
        rec(&dir, "/p/a.txt", "a2", 3);
        let l = added(lab(&dir, "/p/b.txt", "b", "v1", 4));
        assert_eq!(ats(&mut cache), [("/p/b.txt".into(), 4, 2), ("/p/a.txt".into(), 3, 2)]);
        assert_eq!(remove_entry(&dir, &l).unwrap(), Removal::Done);
        assert_eq!(ats(&mut cache), [("/p/a.txt".into(), 3, 2), ("/p/b.txt".into(), 2, 1)]);
        // A file not read again: its stamp is the same (proved by a stale
        // entry planted in the cache surviving).
        let file = index_file(&dir, "/p/a.txt");
        cache.files.get_mut(&file).unwrap().1[0].count = 99;
        assert_eq!(ats(&mut cache)[0].2, 99);
        // An index file gone drops its keys.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(ats(&mut cache), [("/p/b.txt".into(), 2, 1)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn label_kind_name_and_label() {
        assert_eq!(serde_json::to_string(&Kind::Label).unwrap(), "\"label\"");
        assert_eq!(serde_json::from_str::<Kind>("\"label\"").unwrap(), Kind::Label);
        assert_eq!(Kind::Label.label(Some("v1")), "Label: v1");
        assert_eq!(Kind::Label.label(None), "Label");
        assert!(Kind::Label.pinned() && !Kind::Label.automatic() && !Kind::Save.pinned());
        let e = Entry {
            v: 1,
            path: "/p/b".into(),
            at: 1,
            blob: String::new(),
            len: 0,
            lines: 0,
            kind: Kind::Label,
            detail: Some("/p/a".into()),
            since: None,
        };
        assert_eq!(e.label_in(Path::new("/p")), "Label: /p/a");
        assert_eq!(Skip::Binary.reason(), "it is a binary file");
    }
}
