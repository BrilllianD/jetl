//! Local history: snapshots of files taken on every save and before
//! operations that overwrite or delete them (git operations that rewrite
//! the work tree, reloads, Save As over a file, Overwrite, tree deletes).
//! One global store (`Paths::history_dir`, see `store.rs`), shared by every
//! project and instance; File > Local History… lists a file's snapshots and
//! the Git window's History tab diffs one against the current text.
//!
//! All store work runs on one named thread, `jetl-history`, fed through a
//! [`Handle`]. Jobs run in order, so the dedupe check, index appends and
//! prune never race within the process, and a list queued after a capture
//! sees it. The startup prune is the first job. A destructive operation
//! asks for a snapshot first and waits on the returned [`Ack`] (at most
//! [`ACK_TIMEOUT`]) before it touches the files.

pub mod store;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
pub use store::{Entry, Kind};

/// The longest a destructive operation waits for its snapshot.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// At most this many files are snapshotted before one git operation or
/// one tree delete.
pub const MAX_FILES: usize = 500;

/// `[local_history]` in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LocalHistoryConfig {
    /// Take snapshots at all (the store is still pruned).
    pub enabled: bool,
    /// Snapshots older than this many days are pruned.
    pub keep_days: u64,
    /// The oldest snapshots are pruned while the store holds more.
    pub max_total_mb: u64,
    /// Larger files are not snapshotted.
    pub max_file_kb: u64,
}

impl Default for LocalHistoryConfig {
    fn default() -> Self {
        Self { enabled: true, keep_days: 5, max_total_mb: 100, max_file_kb: 2048 }
    }
}

impl LocalHistoryConfig {
    fn max_file_bytes(&self) -> u64 {
        self.max_file_kb.saturating_mul(1024)
    }
}

/// The text of a snapshot.
#[derive(Debug)]
pub enum CaptureText {
    Text(String),
    /// A buffer's rope (an O(1) clone on the main thread).
    Rope(ropey::Rope),
    Bytes(Vec<u8>),
}

impl CaptureText {
    fn into_bytes(self) -> Vec<u8> {
        match self {
            CaptureText::Text(s) => s.into_bytes(),
            CaptureText::Rope(r) => r.to_string().into_bytes(),
            CaptureText::Bytes(b) => b,
        }
    }
}

/// One snapshot to take.
#[derive(Debug)]
pub struct Capture {
    pub path: PathBuf,
    pub text: CaptureText,
    pub kind: Kind,
    pub detail: Option<String>,
}

impl Capture {
    pub fn text(path: PathBuf, text: String, kind: Kind) -> Self {
        Self { path, text: CaptureText::Text(text), kind, detail: None }
    }

    pub fn rope(path: PathBuf, rope: ropey::Rope, kind: Kind) -> Self {
        Self { path, text: CaptureText::Rope(rope), kind, detail: None }
    }
}

/// What a panel or the editor asks of local history (`Effect::History`).
#[derive(Debug)]
pub enum Request {
    Capture(Capture),
    /// Open the Local History palette of a file.
    Show(PathBuf),
    /// Put snapshot `id` back into the file's buffer (undoable).
    Revert {
        path: PathBuf,
        id: String,
    },
}

/// Results of the worker (`AppMsg::LocalHistory`). The store is global, so
/// there is no generation: a list is applied while the palette shows its
/// path, a blob while it is wanted for that path.
#[derive(Debug)]
pub enum HistoryMsg {
    /// The snapshots of `path`, newest first.
    List { path: PathBuf, entries: Vec<Entry> },
    /// A snapshot's text, for a revert.
    Blob { path: PathBuf, id: String, result: Result<Vec<u8>, String> },
}

/// Done when the snapshot a destructive operation asked for is stored.
pub struct Ack(Receiver<()>);

impl Ack {
    /// Wait for the snapshot, at most [`ACK_TIMEOUT`]; a dead worker
    /// returns at once.
    pub fn wait(self) {
        if let Err(RecvTimeoutError::Timeout) = self.0.recv_timeout(ACK_TIMEOUT) {
            tracing::warn!("local history: snapshot not done in {ACK_TIMEOUT:?}; going on");
        }
    }
}

enum Job {
    Capture { capture: Capture, at: u64 },
    BeforeGit { root: PathBuf, label: String, at: u64, done: Sender<()> },
    BeforeDelete { path: PathBuf, at: u64, done: Sender<()> },
    List(PathBuf),
    Blob { path: PathBuf, id: String },
    Prune,
    Flush(Sender<()>),
}

/// The app's side of the worker; clones share it. Dropping the last one
/// ends the thread once its queue is done.
#[derive(Clone)]
pub struct Handle {
    tx: Option<Sender<Job>>,
    settings: Arc<Mutex<LocalHistoryConfig>>,
    dir: PathBuf,
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SS` of Unix ms `at_ms`; `offset` gives seconds east
/// of UTC at a time (`ui::event_log::local_offset`, or a fixed one in
/// tests).
pub fn local_time(at_ms: u64, offset: impl Fn(i64) -> i64) -> String {
    let secs = i64::try_from(at_ms / 1000).unwrap_or(i64::MAX);
    let off = offset(secs);
    let date = crate::editor::blame::date(secs, i32::try_from(off / 60).unwrap_or(0));
    format!("{date} {}", crate::ui::event_log::hms(secs + off))
}

impl Handle {
    /// Start the worker on the store `dir` (created by the first snapshot).
    pub fn spawn(dir: PathBuf, settings: LocalHistoryConfig, bus: UnboundedSender<AppMsg>) -> Self {
        let settings = Arc::new(Mutex::new(settings));
        let (tx, rx) = mpsc::channel();
        let worker = Worker { dir: dir.clone(), settings: settings.clone(), bus };
        let spawned = std::thread::Builder::new().name("jetl-history".into()).spawn(move || worker.run(rx));
        let tx = match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::error!("local history: cannot start its thread: {e}");
                None
            }
        };
        Self { tx, settings, dir }
    }

    /// No worker: every request is dropped.
    pub fn disabled() -> Self {
        Self { tx: None, settings: Arc::new(Mutex::new(LocalHistoryConfig { enabled: false, ..Default::default() })), dir: PathBuf::new() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn settings(&self) -> LocalHistoryConfig {
        self.settings.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_settings(&self, s: LocalHistoryConfig) {
        if let Ok(mut g) = self.settings.lock() {
            *g = s;
        }
    }

    /// Taking snapshots (a worker runs and `[local_history] enabled`).
    pub fn enabled(&self) -> bool {
        self.tx.is_some() && self.settings().enabled
    }

    fn send(&self, job: Job) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.send(job).is_ok())
    }

    /// Queue a snapshot, stamped now.
    pub fn capture(&self, capture: Capture) {
        if self.enabled() {
            self.send(Job::Capture { capture, at: now_ms() });
        }
    }

    /// Snapshot the changed files of `root`'s repository (staged, unstaged
    /// and untracked, as on disk) before git operation `label` runs.
    pub fn before_git(&self, root: &Path, label: &str) -> Option<Ack> {
        if !self.enabled() {
            return None;
        }
        let (done, ack) = mpsc::channel();
        self.send(Job::BeforeGit { root: root.to_path_buf(), label: label.to_string(), at: now_ms(), done }).then_some(Ack(ack))
    }

    /// Snapshot `path` (a file, or the files under a directory that the
    /// tree would show) before it is deleted.
    pub fn before_delete(&self, path: &Path) -> Option<Ack> {
        if !self.enabled() {
            return None;
        }
        let (done, ack) = mpsc::channel();
        self.send(Job::BeforeDelete { path: path.to_path_buf(), at: now_ms(), done }).then_some(Ack(ack))
    }

    /// Ask for `path`'s snapshots (`HistoryMsg::List`); answered even when
    /// snapshots are off.
    pub fn list(&self, path: &Path) {
        self.send(Job::List(path.to_path_buf()));
    }

    /// Ask for a snapshot's bytes (`HistoryMsg::Blob`).
    pub fn blob(&self, path: PathBuf, id: String) {
        self.send(Job::Blob { path, id });
    }

    /// Queue a prune with the current settings.
    pub fn prune(&self) {
        self.send(Job::Prune);
    }

    /// Wait (at most `timeout`) until the jobs queued so far are done; at
    /// quit, so queued snapshots are not lost.
    pub fn flush(&self, timeout: Duration) {
        let (done, rx) = mpsc::channel();
        if self.send(Job::Flush(done)) && rx.recv_timeout(timeout).is_err() {
            tracing::warn!("local history: queue not drained in {timeout:?} at quit");
        }
    }
}

struct Worker {
    dir: PathBuf,
    settings: Arc<Mutex<LocalHistoryConfig>>,
    bus: UnboundedSender<AppMsg>,
}

impl Worker {
    fn run(self, rx: Receiver<Job>) {
        while let Ok(job) = rx.recv() {
            let s = self.settings.lock().map(|s| s.clone()).unwrap_or_default();
            match job {
                Job::Capture { capture, at } => {
                    if s.enabled {
                        self.record(&capture.path, &capture.text.into_bytes(), capture.kind, capture.detail, at, &s);
                    }
                }
                Job::BeforeGit { root, label, at, done } => {
                    if s.enabled {
                        self.before_git(&root, &label, at, &s);
                    }
                    let _ = done.send(());
                }
                Job::BeforeDelete { path, at, done } => {
                    if s.enabled {
                        self.before_delete(&path, at, &s);
                    }
                    let _ = done.send(());
                }
                Job::List(path) => {
                    let entries = store::list(&self.dir, &path);
                    let _ = self.bus.send(AppMsg::LocalHistory(HistoryMsg::List { path, entries }));
                }
                Job::Blob { path, id } => {
                    let result = store::read_blob(&self.dir, &id).map_err(|e| e.to_string());
                    let _ = self.bus.send(AppMsg::LocalHistory(HistoryMsg::Blob { path, id, result }));
                }
                Job::Prune => {
                    let keep = s.keep_days.saturating_mul(86_400_000);
                    match store::prune(&self.dir, now_ms(), keep, s.max_total_mb.saturating_mul(1 << 20)) {
                        Ok(st) if st.locked => tracing::info!("local history: another instance is pruning"),
                        Ok(st) => tracing::info!(
                            "local history pruned: {} entries, {} blobs; {} moved to canonical keys",
                            st.entries,
                            st.blobs,
                            st.moved
                        ),
                        Err(e) => tracing::warn!("local history prune: {e}"),
                    }
                }
                Job::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    }

    fn record(&self, path: &Path, bytes: &[u8], kind: Kind, detail: Option<String>, at: u64, s: &LocalHistoryConfig) {
        match store::record(&self.dir, path, bytes, kind, detail, at, s.max_file_bytes()) {
            Ok(r) => tracing::debug!("local history {}: {r:?}", path.display()),
            Err(e) => tracing::warn!("local history {}: {e}", path.display()),
        }
    }

    /// Read a file for a snapshot when it is a regular file within the size
    /// limit.
    fn read_file(path: &Path, s: &LocalHistoryConfig) -> Option<Vec<u8>> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if !meta.is_file() || meta.len() > s.max_file_bytes() {
            return None;
        }
        std::fs::read(path).ok()
    }

    fn before_git(&self, root: &Path, label: &str, at: u64, s: &LocalHistoryConfig) {
        let snap = match crate::git::repo::status(root) {
            Ok(snap) => snap,
            Err(e) => return tracing::debug!("local history before {label}: {e:#}"),
        };
        let mut seen = std::collections::HashSet::new();
        let paths = snap
            .entries
            .into_iter()
            .filter(|e| e.status != crate::git::model::FileStatus::Deleted)
            .map(|e| e.abs)
            .filter(|p| seen.insert(p.clone()))
            .take(MAX_FILES);
        for path in paths {
            if let Some(bytes) = Self::read_file(&path, s) {
                self.record(&path, &bytes, Kind::BeforeGit, Some(label.to_string()), at, s);
            }
        }
    }

    fn before_delete(&self, path: &Path, at: u64, s: &LocalHistoryConfig) {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return };
        if !meta.is_dir() {
            if let Some(bytes) = Self::read_file(path, s) {
                self.record(path, &bytes, Kind::BeforeDelete, None, at, s);
            }
            return;
        }
        let files = crate::files::walk::builder(path, true, false)
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .take(MAX_FILES);
        for e in files {
            if let Some(bytes) = Self::read_file(e.path(), s) {
                self.record(e.path(), &bytes, Kind::BeforeDelete, None, at, s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-history-mod-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn handle(dir: &Path) -> (Handle, tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Handle::spawn(dir.join("history"), LocalHistoryConfig::default(), tx), rx)
    }

    fn listed(h: &Handle, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, path: &Path) -> Vec<Entry> {
        h.list(path);
        h.flush(Duration::from_secs(5));
        match rx.try_recv() {
            Ok(AppMsg::LocalHistory(HistoryMsg::List { path: p, entries })) if p == path => entries,
            other => panic!("expected the list, got {other:?}"),
        }
    }

    #[test]
    fn a_list_queued_after_a_capture_sees_it() {
        let dir = temp("fifo");
        let (h, mut rx) = handle(&dir);
        let path = dir.join("a.txt");
        h.capture(Capture::text(path.clone(), "one\n".into(), Kind::Save));
        let l = listed(&h, &mut rx, &path);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].label(), "Saved");
        h.blob(path.clone(), l[0].blob.clone());
        h.flush(Duration::from_secs(5));
        match rx.try_recv() {
            Ok(AppMsg::LocalHistory(HistoryMsg::Blob { result: Ok(bytes), .. })) => assert_eq!(bytes, b"one\n"),
            other => panic!("expected the blob, got {other:?}"),
        }
        // Off: nothing more is taken, lists still answer.
        h.set_settings(LocalHistoryConfig { enabled: false, ..Default::default() });
        h.capture(Capture::text(path.clone(), "two\n".into(), Kind::Save));
        assert!(h.before_delete(&path).is_none());
        assert_eq!(listed(&h, &mut rx, &path).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_list_through_a_symlink_keeps_the_path_asked_with() {
        let dir = temp("symlink");
        let real = dir.join("a.txt");
        std::fs::write(&real, "one\n").unwrap();
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (h, mut rx) = handle(&dir);
        h.capture(Capture::text(real.clone(), "one\n".into(), Kind::Save));
        // `listed` checks the answer names `link`, not the target.
        assert_eq!(listed(&h, &mut rx, &link).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn before_git_stores_changed_files_before_the_ack() {
        let dir = temp("git");
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("clean.txt"), "c\n").unwrap();
        crate::git::repo::stage_all(&root).unwrap();
        crate::git::repo::commit(&root, "init").unwrap();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let (h, _rx) = handle(&dir);
        h.before_git(&root, "git switch main").expect("an ack").wait();
        let hist = h.dir().to_path_buf();
        let a = store::list(&hist, &root.join("a.txt"));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].label(), "Before git switch main");
        assert_eq!(store::read_blob(&hist, &a[0].blob).unwrap(), b"changed\n");
        assert_eq!(store::list(&hist, &root.join("new.txt")).len(), 1);
        assert!(store::list(&hist, &root.join("clean.txt")).is_empty(), "clean files are in git already");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn before_delete_walks_a_directory() {
        let dir = temp("delete");
        let d = dir.join("d");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("x.txt"), "x").unwrap();
        std::fs::write(d.join("sub/y.txt"), "y").unwrap();
        std::fs::write(d.join("bin"), b"\0\0").unwrap();
        let (h, _rx) = handle(&dir);
        h.before_delete(&d).expect("an ack").wait();
        let hist = h.dir().to_path_buf();
        assert_eq!(store::list(&hist, &d.join("x.txt"))[0].label(), "Before delete");
        assert_eq!(store::list(&hist, &d.join("sub/y.txt")).len(), 1);
        assert!(store::list(&hist, &d.join("bin")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disabled_handle_does_nothing() {
        let h = Handle::disabled();
        assert!(!h.enabled());
        assert!(h.before_git(Path::new("/"), "x").is_none());
        assert!(h.before_delete(Path::new("/")).is_none());
        h.capture(Capture::text(PathBuf::from("/a"), String::new(), Kind::Save));
        h.flush(Duration::from_millis(10));
    }

    #[test]
    fn config_defaults_and_parsing() {
        let c = crate::config::Config::parse("[local_history]\nkeep_days = 0\n").unwrap().local_history;
        assert_eq!(c, LocalHistoryConfig { keep_days: 0, ..Default::default() });
        assert_eq!(LocalHistoryConfig::default().max_file_bytes(), 2048 * 1024);
    }
}
