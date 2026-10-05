//! The local history store on disk: pure filesystem functions, run by the
//! history worker (`history::Handle`), never on the main thread.
//!
//! ```text
//! history/blobs/<id[0..2]>/<id>   raw bytes, id = git blob sha1 of them
//! history/index/<fnv1a64(path):016x>.jsonl   one `Entry` per line, oldest first
//! history/prune.lock
//! ```
//!
//! Blobs are shared by every entry with the same bytes (any file). Every
//! index line names its path, so two paths whose hashes collide share a
//! file and are told apart when reading. Several jetl instances may write
//! at once: blobs go through a temp file named after the id and the pid,
//! then a rename (idempotent); an index line is one `write_all` on an
//! `O_APPEND` file; prune holds `prune.lock` and deletes an unreferenced
//! blob only once it is older than [`BLOB_GRACE`].

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// An unreferenced blob younger than this is kept: another instance may
/// have written it and not yet appended its index line.
pub const BLOB_GRACE: Duration = Duration::from_secs(10 * 60);

/// A prune lock older than this is taken to be left by a dead instance.
pub const LOCK_STALE: Duration = Duration::from_secs(60 * 60);

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
}

impl Entry {
    pub fn label(&self) -> String {
        self.kind.label(self.detail.as_deref())
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
}

/// What a prune did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Another instance holds the lock; nothing was done.
    pub locked: bool,
    pub entries: usize,
    pub blobs: usize,
}

fn blobs_dir(dir: &Path) -> PathBuf {
    dir.join("blobs")
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

/// The index file of `path` (keyed by the path as given, not canonicalized).
pub fn index_file(dir: &Path, path: &str) -> PathBuf {
    index_dir(dir).join(format!("{:016x}.jsonl", fnv1a64(path.as_bytes())))
}

/// A blob id is 40 lowercase hex digits; anything else never names a file.
fn valid_id(id: &str) -> bool {
    id.len() == 40 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn blob_file(dir: &Path, id: &str) -> PathBuf {
    blobs_dir(dir).join(&id[..2]).join(id)
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

/// Snapshot `bytes` as `path`'s content at `at_ms`, unless it is too large
/// (over `max_bytes`), binary, a file of the store, or the same as the
/// path's last entry.
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
    if path.starts_with(dir) {
        return Ok(Recorded::Skipped(Skip::Internal));
    }
    let Some(key) = path.to_str() else { return Ok(Recorded::Skipped(Skip::NotUtf8Path)) };
    let id = blob_id(bytes)?;
    let index = index_file(dir, key);
    if read_entries(&index, key).last().is_some_and(|e| e.blob == id) {
        return Ok(Recorded::Same);
    }
    write_blob(dir, &id, bytes)?;
    let entry =
        Entry { v: 1, path: key.to_string(), at: at_ms, blob: id, len: bytes.len() as u64, lines: count_lines(bytes), kind, detail };
    let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
    line.push('\n');
    std::fs::create_dir_all(index_dir(dir))?;
    // One write of one short line on an append-only file: lines of two
    // instances never interleave.
    std::fs::OpenOptions::new().create(true).append(true).open(&index)?.write_all(line.as_bytes())?;
    Ok(Recorded::Added(entry))
}

/// Write blob `id` unless it exists; an existing one gets a fresh mtime so
/// a prune in another instance does not take it for an old orphan.
fn write_blob(dir: &Path, id: &str, bytes: &[u8]) -> io::Result<()> {
    let file = blob_file(dir, id);
    if file.is_file() {
        if let Ok(f) = std::fs::File::options().write(true).open(&file) {
            let _ = f.set_modified(SystemTime::now());
        }
        return Ok(());
    }
    let parent = file.parent().unwrap_or(dir);
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{id}.{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
    }
    std::fs::rename(&tmp, &file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The snapshots of `path`, newest first.
pub fn list(dir: &Path, path: &Path) -> Vec<Entry> {
    let Some(key) = path.to_str() else { return Vec::new() };
    let mut v = read_entries(&index_file(dir, key), key);
    v.reverse();
    v
}

/// The bytes of blob `id`.
pub fn read_blob(dir: &Path, id: &str) -> io::Result<Vec<u8>> {
    if !valid_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a snapshot id: {id}")));
    }
    std::fs::read(blob_file(dir, id))
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

/// Drop entries older than `keep_ms` before `now_ms`, then the oldest ones
/// (of any file) while the blobs still referenced exceed `max_bytes`;
/// rewrite the changed index files (an emptied one is removed) and delete
/// the blobs no entry references (once older than [`BLOB_GRACE`]). Does
/// nothing while another instance prunes. Never creates `dir`.
pub fn prune(dir: &Path, now_ms: u64, keep_ms: u64, max_bytes: u64) -> io::Result<PruneStats> {
    if !dir.is_dir() {
        return Ok(PruneStats::default());
    }
    let Some(_lock) = PruneLock::take(dir)? else {
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
    let mut stats = PruneStats::default();
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
        refs.entry(e.blob.as_str()).or_insert((0, e.len)).0 += 1;
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
    stats.blobs = gc_blobs(dir, &keep);
    Ok(stats)
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
    let tmp = file.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file)
}

/// Delete the blobs (and leftover temp files) not in `keep` that are older
/// than [`BLOB_GRACE`], and the empty fan-out directories; returns how many
/// blobs went.
fn gc_blobs(dir: &Path, keep: &HashSet<String>) -> usize {
    let mut removed = 0;
    let Ok(fans) = std::fs::read_dir(blobs_dir(dir)) else { return 0 };
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

    fn blob_count(dir: &Path) -> usize {
        std::fs::read_dir(blobs_dir(dir))
            .map(|fans| fans.flatten().map(|f| std::fs::read_dir(f.path()).map(|r| r.count()).unwrap_or(0)).sum())
            .unwrap_or(0)
    }

    /// Make every blob older than the grace period.
    fn age_blobs(dir: &Path) {
        let old = SystemTime::now() - BLOB_GRACE - Duration::from_secs(60);
        for fan in std::fs::read_dir(blobs_dir(dir)).unwrap().flatten() {
            for b in std::fs::read_dir(fan.path()).unwrap().flatten() {
                std::fs::File::options().write(true).open(b.path()).unwrap().set_modified(old).unwrap();
            }
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
