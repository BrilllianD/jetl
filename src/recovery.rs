//! Crash recovery of unsaved buffers.
//!
//! The run loop keeps a [`RecoverySnapshot`] of every dirty buffer (rope
//! clones, O(1) each) in a mutex shared with the panic hook
//! ([`Recorder`]). On a panic the hook takes it without ever blocking
//! ([`take_snapshot`]: a bounded `try_lock`) and writes it, best effort, to
//! this process's own directory, `state/recovery/<pid>-<start secs>/`: one
//! `<id>.txt` per buffer, then `manifest.json` ([`write_dump`]). A clean
//! exit removes that directory ([`Recorder::clear`]).
//!
//! On the next session load of the same root, [`scan`] finds the dumps of
//! processes that are gone (a live owner's directory is never offered nor
//! removed; dumps of other roots are left for when that project opens) and
//! the app offers to restore them (`app/recovery.rs`).
//!
//! Hot exit reuses the dump format for a quit with unsaved buffers
//! ([`write_hot_exit`]): `state/hot_exit/<root_key>/`, one per root,
//! published by a rename so a failed write leaves the old dump alone. The
//! next session load of that root takes it ([`take_hot_exit`]) and restores
//! it silently. The last process to quit owns the directory; an earlier
//! process's dump is moved to the crash dumps and offered the usual way.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};

use ropey::Rope;
use serde::{Deserialize, Serialize};

use crate::editor::buffer::{Buffer, BufferId};

/// The manifest's file name in a dump directory; written last.
pub const MANIFEST: &str = "manifest.json";
/// The manifest format this build reads and writes.
const VERSION: u32 = 1;

/// What the panic hook would write: the root and every dirty buffer that
/// is not one of the conflict resolver's scratch buffers.
#[derive(Debug, Clone, Default)]
pub struct RecoverySnapshot {
    pub root: PathBuf,
    pub buffers: Vec<SnapBuffer>,
}

/// One dirty buffer in a [`RecoverySnapshot`].
#[derive(Debug, Clone)]
pub struct SnapBuffer {
    pub id: BufferId,
    pub path: Option<PathBuf>,
    pub name: String,
    pub version: u64,
    pub saved_state: u64,
    pub text: Rope,
}

impl SnapBuffer {
    pub fn of(b: &Buffer) -> Self {
        Self { id: b.id, path: b.path.clone(), name: b.name.clone(), version: b.version, saved_state: b.saved_state, text: b.rope.clone() }
    }
}

/// `manifest.json` of a dump directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version, 1.
    pub version: u32,
    pub pid: u32,
    pub root: PathBuf,
    /// When the dump was written, in Unix seconds.
    pub time: u64,
    pub buffers: Vec<Entry>,
}

/// One buffer of a [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The text's file in the dump directory, `<id>.txt`.
    pub file: String,
    pub path: Option<PathBuf>,
    /// `path.is_none()`; readers trust `path`.
    #[serde(default)]
    pub untitled: bool,
    pub name: String,
}

/// A buffer to offer for recovery, from [`scan`].
#[derive(Debug, Clone)]
pub struct FoundEntry {
    pub text_file: PathBuf,
    pub path: Option<PathBuf>,
    pub name: String,
}

impl FoundEntry {
    /// How the dialog lists it: the path relative to `root` (else the full
    /// path), or `name (untitled)`.
    pub fn label(&self, root: &Path) -> String {
        match &self.path {
            Some(p) => p.strip_prefix(root).unwrap_or(p).display().to_string(),
            None => format!("{} (untitled)", self.name),
        }
    }
}

/// The dumps of one root: every directory to remove once answered, and the
/// entries to offer (newest dump first, the newest entry per path only).
#[derive(Debug, Clone)]
pub struct Found {
    pub root: PathBuf,
    /// When the newest dump was written, in Unix seconds.
    pub time: u64,
    pub dirs: Vec<PathBuf>,
    pub entries: Vec<FoundEntry>,
}

/// This process's dump directory name, `<pid>-<unix secs>`.
pub fn instance_name() -> String {
    format!("{}-{}", std::process::id(), crate::recent_projects::now())
}

/// Write `snap` into `dir`: each buffer to `<id>.txt` (through a `.tmp`
/// and a rename; one that fails is skipped), then the manifest the same
/// way. No fsync: this runs in a dying process. Returns the number of
/// buffers written; `Ok(0)` without creating anything when `snap` is empty.
pub fn write_dump(dir: &Path, snap: &RecoverySnapshot, pid: u32, time: u64) -> std::io::Result<usize> {
    if snap.buffers.is_empty() {
        return Ok(0);
    }
    std::fs::create_dir_all(dir)?;
    let mut buffers = Vec::with_capacity(snap.buffers.len());
    for b in &snap.buffers {
        let file = format!("{}.txt", b.id);
        if let Err(e) = write_rope(&dir.join(&file), &b.text) {
            tracing::warn!("recovery: writing {}: {e}", b.name);
            continue;
        }
        // serde_json cannot store a path that is not UTF-8: keep the text
        // as an untitled entry named after it.
        let entry = match &b.path {
            Some(p) if p.to_str().is_none() => Entry { file, path: None, untitled: true, name: p.display().to_string() },
            p => Entry { file, path: p.clone(), untitled: p.is_none(), name: b.name.clone() },
        };
        buffers.push(entry);
    }
    let n = buffers.len();
    let manifest = Manifest { version: VERSION, pid, root: snap.root.clone(), time, buffers };
    let json = serde_json::to_vec_pretty(&manifest).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, dir.join(MANIFEST))?;
    Ok(n)
}

/// `text` to `path` through `path.tmp` and a rename.
fn write_rope(path: &Path, text: &Rope) -> std::io::Result<()> {
    let tmp = path.with_extension("txt.tmp");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    for chunk in text.chunks() {
        w.write_all(chunk.as_bytes())?;
    }
    w.into_inner().map_err(|e| e.into_error())?;
    std::fs::rename(&tmp, path)
}

/// The manifest of the dump in `dir`; `None` when missing, unreadable, not
/// valid or of another format version.
pub fn read_manifest(dir: &Path) -> Option<Manifest> {
    let bytes = std::fs::read(dir.join(MANIFEST)).ok()?;
    serde_json::from_slice::<Manifest>(&bytes).ok().filter(|m| m.version == VERSION)
}

/// The dumps under `recovery_dir` to offer for `root`. Directories not
/// named `<pid>-<secs>` and those of a live process are left alone; a dead
/// process's directory without a manifest (an unfinished dump) or with no
/// buffers is removed; an invalid manifest is logged and left; dumps of
/// other roots are left for when that project opens.
pub fn scan(recovery_dir: &Path, root: &Path) -> Option<Found> {
    let rd = std::fs::read_dir(recovery_dir).ok()?;
    let mut dumps: Vec<(u64, String, PathBuf, Manifest)> = Vec::new();
    for entry in rd.flatten() {
        let dir = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        let Some(pid) = owner_pid(&name) else { continue };
        if !dir.is_dir() || process_alive(pid) {
            continue;
        }
        if !dir.join(MANIFEST).exists() {
            remove_dumps(std::slice::from_ref(&dir));
            continue;
        }
        let Some(m) = read_manifest(&dir) else {
            tracing::warn!("recovery: unreadable manifest in {}; left alone", dir.display());
            continue;
        };
        if m.root != root {
            continue;
        }
        if m.buffers.is_empty() {
            remove_dumps(std::slice::from_ref(&dir));
            continue;
        }
        dumps.push((m.time, name, dir, m));
    }
    if dumps.is_empty() {
        return None;
    }
    // Newest first; the name breaks ties so the order is stable.
    dumps.sort_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
    let time = dumps[0].0;
    let mut entries: Vec<FoundEntry> = Vec::new();
    let mut dirs = Vec::with_capacity(dumps.len());
    for (_, _, dir, m) in dumps {
        push_entries(&dir, m, &mut entries);
        dirs.push(dir);
    }
    Some(Found { root: root.to_path_buf(), time, dirs, entries })
}

/// Add the entries of the dump `m` in `dir` to `entries`: only those whose
/// text file is a plain file name inside `dir`, and for a path only the
/// first (newest) one.
fn push_entries(dir: &Path, m: Manifest, entries: &mut Vec<FoundEntry>) {
    for e in m.buffers {
        // Only a plain file name inside the dump directory.
        if Path::new(&e.file).file_name().and_then(|n| n.to_str()) != Some(e.file.as_str()) {
            continue;
        }
        if e.path.is_some() && entries.iter().any(|f| f.path == e.path) {
            continue;
        }
        entries.push(FoundEntry { text_file: dir.join(&e.file), path: e.path, name: e.name });
    }
}

/// The name of `root`'s directory under the hot-exit dir (and of its
/// session file): its path's FNV-1a hash in 16 hex digits.
pub fn root_key(root: &Path) -> String {
    format!("{:016x}", crate::workspace::fnv1a64(root.as_os_str().as_encoded_bytes()))
}

/// Keep `snap` for the next session of its root (hot exit): the dump goes
/// into `hot_dir/<root_key>/` in the [`write_dump`] layout, written whole
/// into a staging directory and renamed over, so a failure leaves any
/// existing dump untouched. Returns the number of buffers written;
/// `Ok(0)` and nothing touched when `snap` is empty; an error when not
/// every buffer was written.
///
/// The last process to quit owns the directory: a dump found there that
/// another process wrote (`pid` differs) is moved to
/// `recovery_dir/<its pid>-<its time>`, where [`scan`] offers it as a dead
/// owner's crash dump; one of this `pid` is replaced; a directory without a
/// manifest is removed, one with an unreadable manifest (damaged, or of
/// another format version) is moved to `recovery_dir/0-<time>`. Staging
/// directories of this root left by a killed process (more than an hour old
/// at `time`) are removed.
pub fn write_hot_exit(hot_dir: &Path, recovery_dir: &Path, snap: &RecoverySnapshot, pid: u32, time: u64) -> std::io::Result<usize> {
    if snap.buffers.is_empty() {
        return Ok(0);
    }
    let key = root_key(&snap.root);
    let target = hot_dir.join(&key);
    let staging = hot_dir.join(format!(".{key}-{pid}.tmp"));
    remove_dumps(std::slice::from_ref(&staging));
    remove_stale_staging(hot_dir, &key, time);
    let n = match write_dump(&staging, snap, pid, time) {
        Ok(n) if n == snap.buffers.len() => n,
        Ok(n) => {
            remove_dumps(std::slice::from_ref(&staging));
            return Err(std::io::Error::other(format!("{n} of {} buffers written", snap.buffers.len())));
        }
        Err(e) => {
            remove_dumps(std::slice::from_ref(&staging));
            return Err(e);
        }
    };
    let published = demote_earlier(&target, recovery_dir, pid, time).and_then(|()| std::fs::rename(&staging, &target));
    if let Err(e) = published {
        remove_dumps(std::slice::from_ref(&staging));
        return Err(e);
    }
    Ok(n)
}

/// Clear `target` for a new hot-exit dump: another process's dump moves to
/// `recovery_dir` as `<pid>-<time>`, one with an unreadable manifest as
/// `0-<now>` (no owner known; [`prune`] ages it by that name), one of this
/// `pid` or without a manifest goes.
fn demote_earlier(target: &Path, recovery_dir: &Path, pid: u32, now: u64) -> std::io::Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let (owner, time) = match read_manifest(target) {
        Some(m) if m.pid == pid => return std::fs::remove_dir_all(target),
        Some(m) => (m.pid, m.time),
        None if !target.join(MANIFEST).exists() => return std::fs::remove_dir_all(target),
        None => (0, now),
    };
    std::fs::create_dir_all(recovery_dir)?;
    for n in 0..=100 {
        let to = recovery_dir.join(format!("{owner}-{}", time + n));
        if to.exists() {
            continue;
        }
        std::fs::rename(target, &to)?;
        tracing::info!("hot exit: an earlier dump moved to {}", to.display());
        return Ok(());
    }
    Err(std::io::Error::other("no free name in the recovery directory for an earlier dump"))
}

/// Remove the `.{key}-<pid>.tmp` staging directories of `hot_dir` last
/// modified more than an hour before `now` (Unix seconds): a killed process
/// leaves its own behind, and a later process has another pid.
fn remove_stale_staging(hot_dir: &Path, key: &str, now: u64) {
    let Ok(rd) = std::fs::read_dir(hot_dir) else { return };
    let prefix = format!(".{key}-");
    let stale: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".tmp")))
        .filter(|e| {
            let secs = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
            secs.is_some_and(|d| now.saturating_sub(d.as_secs()) > 3600)
        })
        .map(|e| e.path())
        .collect();
    remove_dumps(&stale);
}

/// Remove `root`'s hot-exit dump when `pid` wrote it (the process saved
/// everything after its quit wrote, so the dump is stale); another
/// process's dump stays.
pub fn remove_own_hot_exit(hot_dir: &Path, root: &Path, pid: u32) {
    let dir = hot_dir.join(root_key(root));
    if read_manifest(&dir).is_some_and(|m| m.pid == pid && m.root == root) {
        remove_dumps(&[dir]);
    }
}

/// The hot-exit dump of `root` kept by an earlier quit: its entries, with
/// `dirs` holding the directory to remove once restored. `None` when there
/// is none or it belongs to another root (a key collision; left alone). Only
/// a directory without a manifest is removed; one whose manifest cannot be
/// read (damaged, or written by a build with another format version) is
/// left in place.
pub fn take_hot_exit(hot_dir: &Path, root: &Path) -> Option<Found> {
    let dir = hot_dir.join(root_key(root));
    let Some(m) = read_manifest(&dir) else {
        if dir.is_dir() && !dir.join(MANIFEST).exists() {
            remove_dumps(std::slice::from_ref(&dir));
        }
        return None;
    };
    if m.root != root {
        return None;
    }
    let (time, mut entries) = (m.time, Vec::new());
    push_entries(&dir, m, &mut entries);
    Some(Found { root: root.to_path_buf(), time, dirs: vec![dir], entries })
}

/// How long a dump is kept for a project that never opens again.
pub const MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

/// Remove the dumps under `recovery_dir` older than `max_age` seconds at
/// `now` (startup, off the main thread), so those of projects never
/// reopened do not stay forever. A dump's age is its manifest's time, else
/// (no or an unreadable manifest) its owner's start time from the directory
/// name. Directories not named `<pid>-<secs>`, those of a live process and
/// the dumps of `keep_root` (offered when its session loads) are left
/// alone. Returns how many were removed.
pub fn prune(recovery_dir: &Path, keep_root: &Path, now: u64, max_age: u64) -> usize {
    let Ok(rd) = std::fs::read_dir(recovery_dir) else { return 0 };
    let mut old = Vec::new();
    for entry in rd.flatten() {
        let dir = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        let Some((pid, started)) = owner(&name) else { continue };
        if !dir.is_dir() || process_alive(pid) {
            continue;
        }
        let manifest = read_manifest(&dir);
        if manifest.as_ref().is_some_and(|m| m.root == keep_root) {
            continue;
        }
        let time = manifest.map_or(started, |m| m.time);
        if now.saturating_sub(time) > max_age {
            old.push(dir);
        }
    }
    remove_dumps(&old);
    old.iter().filter(|d| !d.exists()).count()
}

/// Remove dump directories; errors other than a missing one are logged.
pub fn remove_dumps(dirs: &[PathBuf]) {
    for d in dirs {
        if let Err(e) = std::fs::remove_dir_all(d)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("recovery: removing {}: {e}", d.display());
        }
    }
}

/// The owner's pid of a dump directory named `<u32>-<u64>`.
fn owner_pid(dir_name: &str) -> Option<u32> {
    owner(dir_name).map(|(pid, _)| pid)
}

/// The owner's pid and start time of a dump directory named `<u32>-<u64>`.
fn owner(dir_name: &str) -> Option<(u32, u64)> {
    let (pid, secs) = dir_name.split_once('-')?;
    Some((pid.parse().ok()?, secs.parse().ok()?))
}

/// Whether process `pid` exists (one we may not signal counts too).
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; `kill` only checks that the process
    // exists and may be signalled.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_: u32) -> bool {
    false
}

/// A copy of the shared snapshot for the panic hook: `try_lock` up to 50
/// times, 2 ms apart, never a blocking `lock` (the panicking thread might
/// hold it). A poisoned lock is still read: the value is only ever replaced
/// whole. `None` when it stayed busy.
fn take_snapshot(m: &Mutex<RecoverySnapshot>) -> Option<RecoverySnapshot> {
    const TRIES: usize = 50;
    for i in 0..TRIES {
        match m.try_lock() {
            Ok(g) => return Some(g.clone()),
            Err(TryLockError::Poisoned(e)) => return Some(e.into_inner().clone()),
            Err(TryLockError::WouldBlock) => {
                if i + 1 < TRIES {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }
    None
}

/// Where the panic hook writes and what.
struct Armed {
    dir: PathBuf,
    snap: Arc<Mutex<RecoverySnapshot>>,
}

static ARMED: OnceLock<Armed> = OnceLock::new();

/// Let the panic hook write `snap` into `dir`. The first call wins; tests
/// never call it.
pub fn arm(dir: PathBuf, snap: Arc<Mutex<RecoverySnapshot>>) {
    let _ = ARMED.set(Armed { dir, snap });
}

/// The panic hook's part: write the armed snapshot. `None` when not armed,
/// nothing is dirty, the lock stayed busy or the write failed.
pub fn dump_on_panic() -> Option<(PathBuf, usize)> {
    let armed = ARMED.get()?;
    let snap = take_snapshot(&armed.snap)?;
    match write_dump(&armed.dir, &snap, std::process::id(), crate::recent_projects::now()) {
        Ok(0) => None,
        Ok(n) => Some((armed.dir.clone(), n)),
        Err(e) => {
            tracing::error!("recovery: writing {}: {e}", armed.dir.display());
            None
        }
    }
}

/// Tell the user on stderr where a dump of `n` buffers went (after the
/// terminal is restored). Not `eprintln!`: it panics when stderr cannot be
/// written.
pub fn print_dump_note(dir: &Path, n: usize) {
    let _ = writeln!(
        std::io::stderr(),
        "jetl: unsaved changes of {n} buffer(s) saved to {}; they are offered when the project opens again",
        dir.display()
    );
}

/// The run loop's side: keeps the shared snapshot current and this
/// process's dump directory.
pub struct Recorder {
    /// This process's dump directory (created only by a dump).
    pub dir: PathBuf,
    shared: Arc<Mutex<RecoverySnapshot>>,
    last: RecoverySnapshot,
    generation: u64,
    stored: u64,
}

impl Recorder {
    pub fn new(recovery_dir: &Path) -> Self {
        Self {
            dir: recovery_dir.join(instance_name()),
            shared: Arc::default(),
            last: RecoverySnapshot::default(),
            generation: 0,
            stored: 0,
        }
    }

    /// Hand the shared snapshot to the panic hook.
    pub fn arm(&self) {
        arm(self.dir.clone(), self.shared.clone());
    }

    /// Whether the stored snapshot still describes `dirty` (in order) and
    /// `generation`: same buffers, versions, saved states and paths.
    /// Allocates nothing.
    pub fn is_current<'a>(&self, generation: u64, mut dirty: impl Iterator<Item = &'a Buffer>) -> bool {
        if generation != self.generation {
            return false;
        }
        let mut last = self.last.buffers.iter();
        loop {
            match (last.next(), dirty.next()) {
                (None, None) => return true,
                (Some(s), Some(b)) if s.id == b.id && s.version == b.version && s.saved_state == b.saved_state && s.path == b.path => {}
                _ => return false,
            }
        }
    }

    /// Make `snap` the one the panic hook writes.
    pub fn store(&mut self, generation: u64, snap: RecoverySnapshot) {
        self.generation = generation;
        self.last = snap.clone();
        let mut guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::mem::replace(&mut *guard, snap);
        drop(guard);
        drop(old);
        self.stored += 1;
    }

    /// A clean exit: nothing left to write, and this process's dump (from
    /// a background thread's panic) goes.
    pub fn clear(&mut self) {
        self.store(self.generation, RecoverySnapshot::default());
        remove_dumps(std::slice::from_ref(&self.dir));
    }

    pub fn last(&self) -> &RecoverySnapshot {
        &self.last
    }

    /// How many snapshots were stored (tests).
    pub fn stored(&self) -> u64 {
        self.stored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::undo::TxKind;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-test-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn snap_buffer(id: BufferId, path: Option<&Path>, name: &str, text: &str) -> SnapBuffer {
        SnapBuffer { id, path: path.map(Path::to_path_buf), name: name.into(), version: 1, saved_state: 0, text: Rope::from_str(text) }
    }

    const DEAD: u32 = 999_999_999;

    #[test]
    fn dump_round_trip() {
        let rec = temp_dir("round-trip");
        let root = PathBuf::from("/tmp/project");
        let dump = rec.join(format!("{DEAD}-1700000000"));
        let text = "line é世\r\nsecond\r\n";
        let snap = RecoverySnapshot {
            root: root.clone(),
            buffers: vec![snap_buffer(3, Some(&root.join("src/a.rs")), "a.rs", text), snap_buffer(7, None, "scratch", "note\n")],
        };
        assert_eq!(write_dump(&dump, &snap, DEAD, 1_700_000_000).unwrap(), 2);
        let want = Manifest {
            version: 1,
            pid: DEAD,
            root: root.clone(),
            time: 1_700_000_000,
            buffers: vec![
                Entry { file: "3.txt".into(), path: Some(root.join("src/a.rs")), untitled: false, name: "a.rs".into() },
                Entry { file: "7.txt".into(), path: None, untitled: true, name: "scratch".into() },
            ],
        };
        assert_eq!(read_manifest(&dump), Some(want));
        assert_eq!(std::fs::read(dump.join("3.txt")).unwrap(), text.as_bytes());
        assert_eq!(std::fs::read(dump.join("7.txt")).unwrap(), b"note\n");
        assert!(!dump.join("3.txt.tmp").exists() && !dump.join("manifest.json.tmp").exists());

        let found = scan(&rec, &root).expect("offered for its root");
        assert_eq!(found.dirs, std::slice::from_ref(&dump));
        let got: Vec<(Option<PathBuf>, &str, PathBuf)> =
            found.entries.iter().map(|e| (e.path.clone(), e.name.as_str(), e.text_file.clone())).collect();
        assert_eq!(got, [(Some(root.join("src/a.rs")), "a.rs", dump.join("3.txt")), (None, "scratch", dump.join("7.txt"))]);
        assert_eq!(found.entries[0].label(&root), "src/a.rs");
        assert_eq!(found.entries[1].label(&root), "scratch (untitled)");

        assert!(scan(&rec, Path::new("/tmp/other")).is_none());
        assert!(dump.join(MANIFEST).exists(), "another root's dump is left alone");
        remove_dumps(std::slice::from_ref(&dump));
        assert!(!dump.exists());
        let _ = std::fs::remove_dir_all(&rec);
    }

    #[test]
    fn empty_snapshot_writes_nothing() {
        let rec = temp_dir("empty");
        let dump = rec.join(format!("{DEAD}-1"));
        let snap = RecoverySnapshot { root: "/tmp/project".into(), buffers: vec![] };
        assert_eq!(write_dump(&dump, &snap, DEAD, 1).unwrap(), 0);
        assert!(!dump.exists());
    }

    #[test]
    fn scan_skips_live_and_removes_dead_empty() {
        let rec = temp_dir("scan");
        let root = PathBuf::from("/tmp/project");
        let a = root.join("a.txt");
        let one = |id, text: &str| RecoverySnapshot { root: root.clone(), buffers: vec![snap_buffer(id, Some(&a), "a.txt", text)] };
        // A live owner (this test's process): neither offered nor removed.
        let live = rec.join(format!("{}-1", std::process::id()));
        write_dump(&live, &one(1, "live"), std::process::id(), 50).unwrap();
        // A dead owner without a manifest: an unfinished dump.
        let unfinished = rec.join(format!("{DEAD}-1"));
        std::fs::create_dir_all(&unfinished).unwrap();
        std::fs::write(unfinished.join("1.txt"), "partial").unwrap();
        // Not a dump directory.
        let notes = rec.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        // Two dead dumps of the same file: the newer wins.
        let older = rec.join(format!("{DEAD}-10"));
        write_dump(&older, &one(1, "older"), DEAD, 100).unwrap();
        let newer = rec.join(format!("{DEAD}-20"));
        write_dump(&newer, &one(2, "newer"), DEAD, 200).unwrap();

        let found = scan(&rec, &root).expect("dead dumps offered");
        assert_eq!(found.dirs, [newer.clone(), older.clone()]);
        assert_eq!(found.entries.len(), 1);
        assert_eq!(found.entries[0].text_file, newer.join("2.txt"));
        assert!(live.join(MANIFEST).exists());
        assert!(!unfinished.exists());
        assert!(notes.exists());
        let _ = std::fs::remove_dir_all(&rec);
    }

    #[test]
    fn prune_removes_only_old_dead_dumps_of_other_roots() {
        let rec = temp_dir("prune");
        let root = PathBuf::from("/tmp/project");
        let other = PathBuf::from("/tmp/other");
        let day = 24 * 60 * 60;
        let now = 100 * day;
        let dump = |name: &str, root: &Path, pid: u32, time: u64| {
            let dir = rec.join(name);
            let snap = RecoverySnapshot { root: root.to_path_buf(), buffers: vec![snap_buffer(1, Some(&root.join("a")), "a", "x")] };
            write_dump(&dir, &snap, pid, time).unwrap();
            dir
        };
        // Started long ago, but dumped recently: the manifest's time counts.
        let recent = dump(&format!("{DEAD}-1"), &other, DEAD, now - 29 * day);
        let old = dump(&format!("{DEAD}-2"), &other, DEAD, now - 31 * day);
        // The current root's old dump is offered when its session loads.
        let current = dump(&format!("{DEAD}-3"), &root, DEAD, now - 40 * day);
        let live = dump(&format!("{}-4", std::process::id()), &other, std::process::id(), now - 40 * day);
        // No manifest: the start time in the name counts.
        let unfinished_old = rec.join(format!("{DEAD}-{}", now - 31 * day));
        let unfinished_new = rec.join(format!("{DEAD}-{}", now - day));
        for d in [&unfinished_old, &unfinished_new] {
            std::fs::create_dir_all(d).unwrap();
        }
        let notes = rec.join("notes");
        std::fs::create_dir_all(&notes).unwrap();

        assert_eq!(prune(&rec, &root, now, MAX_AGE_SECS), 2);
        assert!(!old.exists() && !unfinished_old.exists());
        for kept in [&recent, &current, &live, &unfinished_new, &notes] {
            assert!(kept.exists(), "{} kept", kept.display());
        }
        assert_eq!(prune(&rec.join("missing"), &root, now, MAX_AGE_SECS), 0);
        let _ = std::fs::remove_dir_all(&rec);
    }

    #[test]
    fn busy_lock_gives_up() {
        let m = Mutex::new(RecoverySnapshot::default());
        let held = m.lock().unwrap();
        assert!(take_snapshot(&m).is_none(), "bounded: gives up instead of hanging");
        drop(held);
        assert!(take_snapshot(&m).is_some());
    }

    #[test]
    fn poisoned_lock_is_still_read() {
        let m = Arc::new(Mutex::new(RecoverySnapshot { root: "/tmp/project".into(), buffers: vec![] }));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _held = m2.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(m.is_poisoned());
        assert_eq!(take_snapshot(&m).map(|s| s.root), Some(PathBuf::from("/tmp/project")));
    }

    #[test]
    fn is_current_tracks_version_saved_state_path() {
        let rec = temp_dir("is-current");
        let mut r = Recorder::new(&rec);
        let mut b = Buffer::from_text(1, Some(PathBuf::from("/tmp/project/a.txt")), "a\n", false);
        assert!(r.is_current(0, std::iter::empty()));
        assert!(!r.is_current(1, std::iter::empty()), "a root switch");
        b.insert("x", TxKind::Typing);
        assert!(!r.is_current(0, std::iter::once(&b)));
        let store =
            |r: &mut Recorder, b: &Buffer| r.store(0, RecoverySnapshot { root: "/tmp/project".into(), buffers: vec![SnapBuffer::of(b)] });
        store(&mut r, &b);
        assert!(r.is_current(0, std::iter::once(&b)));
        assert_eq!(r.last().buffers[0].text.to_string(), "xa\n");
        b.insert("y", TxKind::Typing);
        assert!(!r.is_current(0, std::iter::once(&b)), "version");
        store(&mut r, &b);
        b.mark_saved();
        assert!(!r.is_current(0, std::iter::once(&b)), "saved state");
        store(&mut r, &b);
        b.path = Some(PathBuf::from("/tmp/project/b.txt"));
        assert!(!r.is_current(0, std::iter::once(&b)), "path");
        store(&mut r, &b);
        assert!(r.is_current(0, std::iter::once(&b)));
        assert!(!r.is_current(0, std::iter::empty()), "the buffer was saved or closed");
        assert_eq!(r.stored(), 4);
        r.clear();
        assert!(r.last().buffers.is_empty() && !r.dir.exists());
    }

    fn hot_snap(root: &Path, buffers: Vec<SnapBuffer>) -> RecoverySnapshot {
        RecoverySnapshot { root: root.to_path_buf(), buffers }
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn hot_exit_round_trip() {
        let base = temp_dir("hot-round-trip");
        let (hot, rec) = (base.join("hot"), base.join("recovery"));
        let root = PathBuf::from("/tmp/project");
        let a = root.join("a.txt");
        let snap = hot_snap(&root, vec![snap_buffer(3, Some(&a), "a.txt", "mine\n"), snap_buffer(7, None, "notes", "note\n")]);
        assert_eq!(write_hot_exit(&hot, &rec, &snap, DEAD, 100).unwrap(), 2);
        let dir = hot.join(root_key(&root));
        assert!(dir.join(MANIFEST).is_file());
        assert_eq!(std::fs::read_to_string(dir.join("3.txt")).unwrap(), "mine\n");
        assert_eq!(std::fs::read_to_string(dir.join("7.txt")).unwrap(), "note\n");
        assert_eq!(dir_names(&hot), vec![root_key(&root)], "no staging leftovers");
        let found = take_hot_exit(&hot, &root).unwrap();
        assert_eq!(found.dirs, vec![dir.clone()]);
        assert_eq!(found.entries.len(), 2);
        assert_eq!((found.entries[0].path.as_deref(), found.entries[0].name.as_str()), (Some(a.as_path()), "a.txt"));
        assert_eq!(found.entries[0].text_file, dir.join("3.txt"));
        assert_eq!((found.entries[1].path.as_deref(), found.entries[1].name.as_str()), (None, "notes"));
        assert_eq!(found.entries[1].text_file, dir.join("7.txt"));
        assert!(take_hot_exit(&hot, Path::new("/tmp/other")).is_none());
        assert!(dir.is_dir(), "taking does not remove a dump with a manifest");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_later_quit_moves_the_earlier_dump_to_recovery() {
        let base = temp_dir("hot-demote");
        let (hot, rec) = (base.join("hot"), base.join("recovery"));
        let root = PathBuf::from("/tmp/project");
        let a = root.join("a.txt");
        let one = |text: &str| hot_snap(&root, vec![snap_buffer(1, Some(&a), "a.txt", text)]);
        let kept = |hot: &Path| std::fs::read_to_string(hot.join(root_key(&root)).join("1.txt")).unwrap();
        write_hot_exit(&hot, &rec, &one("first"), DEAD, 100).unwrap();
        write_hot_exit(&hot, &rec, &one("second"), DEAD - 1, 200).unwrap();
        assert_eq!(kept(&hot), "second");
        assert!(rec.join(format!("{DEAD}-100")).is_dir());
        let offered = scan(&rec, &root).unwrap();
        assert_eq!(offered.entries.len(), 1);
        assert_eq!(std::fs::read_to_string(&offered.entries[0].text_file).unwrap(), "first");
        write_hot_exit(&hot, &rec, &one("third"), DEAD - 1, 300).unwrap();
        assert_eq!(kept(&hot), "third");
        assert_eq!(dir_names(&rec), vec![format!("{DEAD}-100")], "same pid replaces, no second recovery dir");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_snapshot_keeps_the_existing_dump() {
        let base = temp_dir("hot-empty");
        let (hot, rec) = (base.join("hot"), base.join("recovery"));
        let root = PathBuf::from("/tmp/project");
        let snap = hot_snap(&root, vec![snap_buffer(1, None, "n", "text\n")]);
        write_hot_exit(&hot, &rec, &snap, DEAD, 100).unwrap();
        let dir = hot.join(root_key(&root));
        let before = (std::fs::read(dir.join(MANIFEST)).unwrap(), std::fs::read(dir.join("1.txt")).unwrap());
        assert_eq!(write_hot_exit(&hot, &rec, &hot_snap(&root, vec![]), DEAD - 1, 200).unwrap(), 0);
        assert_eq!((std::fs::read(dir.join(MANIFEST)).unwrap(), std::fs::read(dir.join("1.txt")).unwrap()), before);
        assert!(!rec.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failed_write_creates_nothing() {
        let base = temp_dir("hot-fail");
        std::fs::create_dir_all(&base).unwrap();
        let hot = base.join("hot");
        std::fs::write(&hot, "not a directory").unwrap();
        let root = PathBuf::from("/tmp/project");
        let snap = hot_snap(&root, vec![snap_buffer(1, None, "n", "text\n")]);
        assert!(write_hot_exit(&hot, &base.join("recovery"), &snap, DEAD, 100).is_err());
        assert!(hot.is_file());
        assert_eq!(std::fs::read_to_string(&hot).unwrap(), "not a directory");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn root_key_matches_the_session_file() {
        let root = PathBuf::from("/tmp/some/project");
        let file = crate::workspace::Session::file_for(Path::new("/tmp/sessions"), &root);
        assert_eq!(file.file_name().unwrap().to_string_lossy(), format!("{}.json", root_key(&root)));
    }

    #[test]
    fn an_unreadable_manifest_is_kept_on_load_and_moved_on_the_next_write() {
        for (label, manifest) in [
            ("corrupt", "{ not json".to_string()),
            ("newer", r#"{"version":99,"pid":1,"root":"/tmp/project","time":5,"buffers":[]}"#.into()),
        ] {
            let base = temp_dir(&format!("hot-unreadable-{label}"));
            let (hot, rec) = (base.join("hot"), base.join("recovery"));
            let root = PathBuf::from("/tmp/project");
            let dir = hot.join(root_key(&root));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(MANIFEST), &manifest).unwrap();
            std::fs::write(dir.join("1.txt"), "kept\n").unwrap();
            assert!(take_hot_exit(&hot, &root).is_none());
            assert!(dir.join("1.txt").is_file(), "{label}: load leaves the dump");
            let snap = hot_snap(&root, vec![snap_buffer(1, None, "n", "new\n")]);
            assert_eq!(write_hot_exit(&hot, &rec, &snap, DEAD, 700).unwrap(), 1);
            assert_eq!(std::fs::read_to_string(rec.join("0-700").join("1.txt")).unwrap(), "kept\n", "{label}: moved, not deleted");
            assert_eq!(std::fs::read_to_string(rec.join("0-700").join(MANIFEST)).unwrap(), manifest);
            assert_eq!(std::fs::read_to_string(dir.join("1.txt")).unwrap(), "new\n");
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn a_directory_without_a_manifest_is_removed_on_load() {
        let base = temp_dir("hot-no-manifest");
        let hot = base.join("hot");
        let root = PathBuf::from("/tmp/project");
        let dir = hot.join(root_key(&root));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(take_hot_exit(&hot, &root).is_none());
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn writing_removes_old_staging_dirs_of_the_same_key_only() {
        let base = temp_dir("hot-staging");
        let (hot, rec) = (base.join("hot"), base.join("recovery"));
        let root = PathBuf::from("/tmp/project");
        let key = root_key(&root);
        let (stale, other_key) = (hot.join(format!(".{key}-4242.tmp")), hot.join(".0000000000000000-4242.tmp"));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&other_key).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let snap = hot_snap(&root, vec![snap_buffer(1, None, "n", "x\n")]);
        write_hot_exit(&hot, &rec, &snap, DEAD, now).unwrap();
        assert!(stale.exists(), "modified just now: kept");
        write_hot_exit(&hot, &rec, &snap, DEAD, now + 7200).unwrap();
        assert!(!stale.exists(), "more than an hour old: removed");
        assert!(other_key.exists(), "another key's staging dir is not ours");
        assert!(hot.join(&key).is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_own_hot_exit_checks_the_pid() {
        let base = temp_dir("hot-remove-own");
        let (hot, rec) = (base.join("hot"), base.join("recovery"));
        let root = PathBuf::from("/tmp/project");
        let snap = hot_snap(&root, vec![snap_buffer(1, None, "n", "x\n")]);
        write_hot_exit(&hot, &rec, &snap, DEAD, 100).unwrap();
        remove_own_hot_exit(&hot, &root, DEAD - 1);
        assert!(hot.join(root_key(&root)).is_dir());
        remove_own_hot_exit(&hot, &root, DEAD);
        assert!(!hot.join(root_key(&root)).exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
