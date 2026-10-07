//! Local history: snapshots of files taken on every save and before
//! operations that overwrite or delete them (git operations that rewrite
//! the work tree, a forced worktree remove, reloads, Save As over a file,
//! Overwrite, tree deletes)
//! and, when `snapshot_unsaved_secs` is set, of unsaved edits on a timer;
//! a rename in the Project tree carries a file's snapshots to its new path;
//! Put Label adds a named snapshot that prune keeps.
//! One global store (`Paths::history_dir`, see `store.rs`), shared by every
//! project and instance, with snapshots stored compressed. The Local History
//! tool window (`panel.rs`, File > Local History…) lists a file's snapshots
//! next to the diff of the selected one against the current text, or (its
//! project view, `project.rs`) the files of the project or a folder that
//! have history, deleted ones included.
//!
//! All store work runs on one named thread, `jetl-history`, fed through a
//! [`Handle`]. Jobs run in order, so the dedupe check, index appends and
//! prune never race within the process, and a list queued after a capture
//! sees it. The startup prune is the first job. A destructive operation
//! asks for a snapshot first and waits on the returned [`Ack`] (at most
//! [`ACK_TIMEOUT`]) before it touches the files.

pub mod panel;
pub mod project;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
pub use store::{Entry, FileState, Kind, RecentFile};

/// The longest a destructive operation waits for its snapshot.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// At most this many files are snapshotted before one git operation or
/// one tree delete.
pub const MAX_FILES: usize = 500;

/// Any `snapshot_unsaved_secs` under this counts as this many seconds.
pub const MIN_UNSAVED_SECS: u64 = 10;

/// A label name is cut to this many chars (an index line stays short, so
/// its single `O_APPEND` write stays safe).
pub const MAX_LABEL_CHARS: usize = 100;

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
    /// Every this many seconds, snapshot the unsaved edits of open files;
    /// 0 is off, less than [`MIN_UNSAVED_SECS`] counts as that.
    pub snapshot_unsaved_secs: u64,
}

impl Default for LocalHistoryConfig {
    fn default() -> Self {
        Self { enabled: true, keep_days: 5, max_total_mb: 100, max_file_kb: 2048, snapshot_unsaved_secs: 0 }
    }
}

impl LocalHistoryConfig {
    pub(crate) fn max_file_bytes(&self) -> u64 {
        self.max_file_kb.saturating_mul(1024)
    }

    /// The period of snapshots of unsaved edits; `None` when off.
    pub fn unsaved_every(&self) -> Option<Duration> {
        (self.snapshot_unsaved_secs > 0).then(|| Duration::from_secs(self.snapshot_unsaved_secs.max(MIN_UNSAVED_SECS)))
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
    /// List a file's snapshots again (`HistoryMsg::List`).
    List(PathBuf),
    /// Scan for the files under a folder that have history
    /// (`HistoryMsg::Recent`).
    Recent(PathBuf),
    /// The diff of a snapshot (`DiffKey::Local`) against the current text,
    /// answered as `HistoryMsg::Diff` with `seq`.
    Diff {
        key: crate::git::DiffKey,
        seq: u64,
    },
    /// Put snapshot `id` back into the file's buffer (undoable).
    Revert {
        path: PathBuf,
        id: String,
    },
    /// Put the selected changes of the window's diff back into the file's
    /// buffer (one undoable edit).
    RevertChanges(Box<ChangesRevert>),
    /// Ask a name for a label on this file (Put Label).
    PutLabel(PathBuf),
    /// Ask before removing label `name`: these entries (one, or every file's
    /// of a label put on several), each with the file it is shown as.
    RemoveLabel {
        name: String,
        entries: Vec<(PathBuf, Entry)>,
    },
}

/// Changes of a Local History diff to put back: they apply only to the text
/// the diff compared the snapshot with.
#[derive(Debug, Clone)]
pub struct ChangesRevert {
    pub path: PathBuf,
    /// The current text the diff was built from.
    pub base: String,
    /// One per change, last first (`editor::gutter::revert_spans`).
    pub changes: Vec<crate::editor::undo::Change>,
    /// The snapshot's date and label, for the message.
    pub title: String,
}

/// Results of the worker and of the diff loads (`AppMsg::LocalHistory`).
/// The store is global, so there is no generation: a list, a stored
/// snapshot or a label change matter while the Local History window shows
/// its path, a blob while the window has that snapshot selected, a diff
/// while it is the window's newest request.
#[derive(Debug)]
pub enum HistoryMsg {
    /// A snapshot of `path` (as it was queued) was stored.
    Stored { path: PathBuf },
    /// A snapshot against the current text (`App::load_local_diff`).
    Diff { key: crate::git::DiffKey, seq: u64, result: anyhow::Result<crate::git::diff::DiffDoc> },
    /// The snapshots of `path`, newest first.
    List { path: PathBuf, entries: Vec<Entry> },
    /// The files under `scope` (as asked) that have history, newest change
    /// first.
    Recent { scope: PathBuf, files: Vec<RecentFile> },
    /// A snapshot's text, for a revert.
    Blob { path: PathBuf, id: String, result: Result<Vec<u8>, String> },
    /// A label was put on `path`, or why not.
    Labelled { path: PathBuf, name: String, result: Result<(), String> },
    /// Label `name` was removed from `paths`: how many entries went (0: they
    /// were gone already).
    LabelRemoved { paths: Vec<PathBuf>, name: String, result: Result<usize, String> },
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
    Rename { from: PathBuf, to: PathBuf, at: u64 },
    Label { path: PathBuf, text: Option<CaptureText>, name: String, at: u64 },
    RemoveLabel { name: String, entries: Vec<(PathBuf, Entry)> },
    List(PathBuf),
    Recent(PathBuf),
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
        let worker = Worker { dir: dir.clone(), settings: settings.clone(), bus, cache: None };
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

    /// Carry `from`'s history to `to` after the Project tree renamed it
    /// (a file or a folder). Queue it after the rename succeeded and before
    /// the tabs follow, so snapshots of the old path queued before it are
    /// carried too.
    pub fn rename(&self, from: &Path, to: &Path) {
        if self.enabled() {
            self.send(Job::Rename { from: from.to_path_buf(), to: to.to_path_buf(), at: now_ms() });
        }
    }

    /// Put label `name` on `path`: `text` (an open tab's, unsaved edits
    /// included), else the file on disk, read on the worker. Answered by
    /// `HistoryMsg::Labelled`; `false` (nothing queued) while local history is off.
    pub fn put_label(&self, path: PathBuf, text: Option<CaptureText>, name: String) -> bool {
        self.enabled() && self.send(Job::Label { path, text, name, at: now_ms() })
    }

    /// Remove label `name`: `entries` (as listed, each with the path it is
    /// shown as); answered by `HistoryMsg::LabelRemoved`. Runs while local
    /// history is off too, as prune does.
    pub fn remove_label(&self, name: String, entries: Vec<(PathBuf, Entry)>) {
        self.send(Job::RemoveLabel { name, entries });
    }

    /// Ask for `path`'s snapshots (`HistoryMsg::List`); answered even when
    /// snapshots are off.
    pub fn list(&self, path: &Path) {
        self.send(Job::List(path.to_path_buf()));
    }

    /// Ask for the files under folder `scope` that have history
    /// (`HistoryMsg::Recent`); answered even when snapshots are off.
    pub fn recent(&self, scope: &Path) {
        self.send(Job::Recent(scope.to_path_buf()));
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
    /// The index in brief for project view scans; made by the first one.
    cache: Option<store::IndexCache>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Job>) {
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
                Job::Rename { from, to, at } => {
                    if s.enabled {
                        self.rename(&from, &to, at, &s);
                    }
                }
                Job::Label { path, text, name, at } => {
                    let result = self.label(&path, text, &name, at, &s);
                    let _ = self.bus.send(AppMsg::LocalHistory(HistoryMsg::Labelled { path, name, result }));
                }
                Job::RemoveLabel { name, entries } => {
                    let result = self.remove_labels(&entries);
                    let paths = entries.into_iter().map(|(p, _)| p).collect();
                    let _ = self.bus.send(AppMsg::LocalHistory(HistoryMsg::LabelRemoved { paths, name, result }));
                }
                Job::List(path) => {
                    let entries = store::list(&self.dir, &path);
                    let _ = self.bus.send(AppMsg::LocalHistory(HistoryMsg::List { path, entries }));
                }
                Job::Recent(scope) => {
                    let files = store::recent(&self.dir, &scope, self.cache.get_or_insert_with(Default::default));
                    let _ = self.bus.send(AppMsg::LocalHistory(HistoryMsg::Recent { scope, files }));
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
                            "local history pruned: {} entries, {} blobs; {} moved to canonical keys; store holds {} KB",
                            st.entries,
                            st.blobs,
                            st.moved,
                            st.bytes / 1024
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

    /// Store a snapshot; a new or replaced entry is reported
    /// (`HistoryMsg::Stored`), so the Local History window can list again.
    fn record(&self, path: &Path, bytes: &[u8], kind: Kind, detail: Option<String>, at: u64, s: &LocalHistoryConfig) {
        match store::record(&self.dir, path, bytes, kind, detail, at, s.max_file_bytes()) {
            Ok(r) => {
                tracing::debug!("local history {}: {r:?}", path.display());
                if matches!(r, store::Recorded::Added(_) | store::Recorded::Replaced(_)) {
                    let _ = self.bus.send(AppMsg::LocalHistory(HistoryMsg::Stored { path: path.to_path_buf() }));
                }
            }
            Err(e) => tracing::warn!("local history {}: {e}", path.display()),
        }
    }

    fn rename(&self, from: &Path, to: &Path, at: u64, s: &LocalHistoryConfig) {
        match store::rename(&self.dir, from, to, at, s.max_file_bytes(), |p| Self::read_file(p, s)) {
            Ok(c) => tracing::debug!(
                "local history: carried {} entries of {} files from {} to {}, moved {} labels{}",
                c.entries,
                c.files,
                from.display(),
                to.display(),
                c.labels,
                if c.unlocked { " (prune lock held)" } else { "" }
            ),
            Err(e) => tracing::warn!("local history rename {} -> {}: {e}", from.display(), to.display()),
        }
    }

    /// Remove label entries, each under the lock it takes; how many went.
    fn remove_labels(&self, entries: &[(PathBuf, Entry)]) -> Result<usize, String> {
        let mut removed = 0;
        for (_, entry) in entries {
            match store::remove_entry(&self.dir, entry) {
                Ok(store::Removal::Done) => removed += 1,
                Ok(store::Removal::Gone) => {}
                Ok(store::Removal::Locked) => return Err("another jetl instance is pruning local history; try again".to_string()),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(removed)
    }

    /// Store label `name` of `path`: `text`, else the file on disk.
    fn label(&self, path: &Path, text: Option<CaptureText>, name: &str, at: u64, s: &LocalHistoryConfig) -> Result<(), String> {
        if !s.enabled {
            return Err("local history is off".into());
        }
        let bytes = match text {
            Some(t) => t.into_bytes(),
            None => Self::read_for_label(path, s)?,
        };
        match store::record(&self.dir, path, &bytes, Kind::Label, Some(name.to_string()), at, s.max_file_bytes()) {
            Ok(store::Recorded::Added(_) | store::Recorded::Replaced(_)) => Ok(()),
            Ok(store::Recorded::Skipped(store::Skip::TooLarge)) => Err(too_large(s)),
            Ok(store::Recorded::Skipped(k)) => Err(k.reason().into()),
            Ok(store::Recorded::Same) => Err("nothing stored".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The file a label is put on, from disk (symlinks followed): a regular
    /// file within the size limit, else why not.
    fn read_for_label(path: &Path, s: &LocalHistoryConfig) -> Result<Vec<u8>, String> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("the file does not exist".into()),
            Err(e) => return Err(e.to_string()),
        };
        if !meta.is_file() {
            return Err("it is not a regular file".into());
        }
        if meta.len() > s.max_file_bytes() {
            return Err(too_large(s));
        }
        std::fs::read(path).map_err(|e| e.to_string())
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

fn too_large(s: &LocalHistoryConfig) -> String {
    format!("{} ({} KB)", store::Skip::TooLarge.reason(), s.max_file_kb)
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

    /// The next message that is not a `Stored` report.
    fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>) -> Result<AppMsg, tokio::sync::mpsc::error::TryRecvError> {
        loop {
            match rx.try_recv() {
                Ok(AppMsg::LocalHistory(HistoryMsg::Stored { .. })) => {}
                other => return other,
            }
        }
    }

    fn listed(h: &Handle, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, path: &Path) -> Vec<Entry> {
        h.list(path);
        h.flush(Duration::from_secs(5));
        match next(rx) {
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
        match next(&mut rx) {
            Ok(AppMsg::LocalHistory(HistoryMsg::Blob { result: Ok(bytes), .. })) => assert_eq!(bytes, b"one\n"),
            other => panic!("expected the blob, got {other:?}"),
        }
        // A compressible text is stored compressed and read back whole.
        let big = "fn main() { println!(\"hello\"); }\n".repeat(200);
        h.capture(Capture::text(path.clone(), big.clone(), Kind::Save));
        let l = listed(&h, &mut rx, &path);
        assert_eq!((l.len(), l[0].len), (2, big.len() as u64));
        assert!(h.dir().join("blobs-zlib").join(&l[0].blob[..2]).join(&l[0].blob).is_file());
        h.blob(path.clone(), l[0].blob.clone());
        h.flush(Duration::from_secs(5));
        match next(&mut rx) {
            Ok(AppMsg::LocalHistory(HistoryMsg::Blob { result: Ok(bytes), .. })) => assert_eq!(bytes, big.as_bytes()),
            other => panic!("expected the blob, got {other:?}"),
        }
        // Off: nothing more is taken, lists still answer.
        h.set_settings(LocalHistoryConfig { enabled: false, ..Default::default() });
        h.capture(Capture::text(path.clone(), "two\n".into(), Kind::Save));
        assert!(h.before_delete(&path).is_none());
        assert_eq!(listed(&h, &mut rx, &path).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stored_snapshot_is_reported_and_a_same_one_is_not() {
        let dir = temp("stored");
        let (h, mut rx) = handle(&dir);
        let path = dir.join("a.txt");
        h.capture(Capture::text(path.clone(), "one\n".into(), Kind::Save));
        h.capture(Capture::text(path.clone(), "one\n".into(), Kind::Save));
        h.flush(Duration::from_secs(5));
        let mut stored = Vec::new();
        while let Ok(m) = rx.try_recv() {
            match m {
                AppMsg::LocalHistory(HistoryMsg::Stored { path }) => stored.push(path),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(stored, [path]);
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
    fn a_rename_queued_after_a_capture_carries_it() {
        let dir = temp("rename");
        let (a, b, c) = (dir.join("a.txt"), dir.join("b.txt"), dir.join("c.txt"));
        std::fs::write(&a, "one\n").unwrap();
        let (h, mut rx) = handle(&dir);
        h.capture(Capture::text(a.clone(), "one\n".into(), Kind::Save));
        std::fs::rename(&a, &b).unwrap();
        h.rename(&a, &b);
        let l = listed(&h, &mut rx, &b);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].kind, l[0].detail.as_deref()), (Kind::Renamed, a.to_str()));
        assert_eq!(l[1].label(), "Saved");
        // Off: nothing is carried.
        h.set_settings(LocalHistoryConfig { enabled: false, ..Default::default() });
        std::fs::rename(&b, &c).unwrap();
        h.rename(&b, &c);
        assert!(listed(&h, &mut rx, &c).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disabled_handle_does_nothing() {
        let h = Handle::disabled();
        assert!(!h.enabled());
        assert!(h.before_git(Path::new("/"), "x").is_none());
        assert!(h.before_delete(Path::new("/")).is_none());
        h.capture(Capture::text(PathBuf::from("/a"), String::new(), Kind::Save));
        h.rename(Path::new("/a"), Path::new("/b"));
        assert!(!h.put_label(PathBuf::from("/a"), None, "v1".into()));
        let label =
            Entry { v: 1, path: "/a".into(), at: 1, blob: String::new(), len: 0, lines: 0, kind: Kind::Label, detail: None, since: None };
        h.remove_label("v1".into(), vec![(PathBuf::from("/a"), label)]);
        h.recent(Path::new("/"));
        h.flush(Duration::from_millis(10));
    }

    /// The next `Labelled` / `LabelRemoved` answer, after a flush.
    fn answer(h: &Handle, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>) -> HistoryMsg {
        h.flush(Duration::from_secs(5));
        match next(rx) {
            Ok(AppMsg::LocalHistory(m @ (HistoryMsg::Labelled { .. } | HistoryMsg::LabelRemoved { .. }))) => m,
            other => panic!("expected a label answer, got {other:?}"),
        }
    }

    fn label_result(m: HistoryMsg) -> Result<(), String> {
        match m {
            HistoryMsg::Labelled { result, .. } => result,
            other => panic!("expected Labelled, got {other:?}"),
        }
    }

    fn removed_result(m: HistoryMsg) -> Result<usize, String> {
        match m {
            HistoryMsg::LabelRemoved { result, .. } => result,
            other => panic!("expected LabelRemoved, got {other:?}"),
        }
    }

    #[test]
    fn a_label_is_answered_and_reads_the_disk_without_text() {
        let dir = temp("label");
        let a = dir.join("a.txt");
        std::fs::write(&a, "disk\n").unwrap();
        let (h, mut rx) = handle(&dir);
        assert!(h.put_label(a.clone(), None, "v1".into()));
        assert_eq!(label_result(answer(&h, &mut rx)), Ok(()));
        let l = listed(&h, &mut rx, &a);
        assert_eq!(l[0].label(), "Label: v1");
        assert_eq!(store::read_blob(h.dir(), &l[0].blob).unwrap(), b"disk\n");
        assert!(h.put_label(a.clone(), Some(CaptureText::Text("tab\n".into())), "v2".into()));
        assert_eq!(label_result(answer(&h, &mut rx)), Ok(()));
        let l = listed(&h, &mut rx, &a);
        assert_eq!((l[0].label(), store::read_blob(h.dir(), &l[0].blob).unwrap()), ("Label: v2".into(), b"tab\n".to_vec()));
        // Not stored, and why.
        h.put_label(dir.join("missing.txt"), None, "x".into());
        let e = label_result(answer(&h, &mut rx)).unwrap_err();
        assert!(e.contains("does not exist"), "{e}");
        std::fs::write(dir.join("bin"), b"a\0b").unwrap();
        h.put_label(dir.join("bin"), None, "x".into());
        let e = label_result(answer(&h, &mut rx)).unwrap_err();
        assert!(e.contains("binary"), "{e}");
        h.put_label(dir.clone(), None, "x".into());
        let e = label_result(answer(&h, &mut rx)).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        // Removal.
        let v2 = listed(&h, &mut rx, &a)[0].clone();
        h.remove_label("v2".into(), vec![(a.clone(), v2.clone())]);
        assert_eq!(removed_result(answer(&h, &mut rx)), Ok(1));
        let l = listed(&h, &mut rx, &a);
        assert_eq!(l.iter().map(Entry::label).collect::<Vec<_>>(), ["Label: v1"]);
        h.remove_label("v2".into(), vec![(a.clone(), v2)]);
        assert_eq!(removed_result(answer(&h, &mut rx)), Ok(0));
        // Off: no label is put, but one is still removed.
        h.set_settings(LocalHistoryConfig { enabled: false, ..Default::default() });
        assert!(!h.put_label(a.clone(), None, "v3".into()));
        h.remove_label("v1".into(), vec![(a.clone(), l[0].clone())]);
        assert_eq!(removed_result(answer(&h, &mut rx)), Ok(1));
        assert!(listed(&h, &mut rx, &a).is_empty());
        // A label of several files goes in one request.
        let b = dir.join("b.txt");
        std::fs::write(&b, "b\n").unwrap();
        h.set_settings(LocalHistoryConfig::default());
        for p in [&a, &b] {
            h.put_label(p.clone(), None, "both".into());
            assert_eq!(label_result(answer(&h, &mut rx)), Ok(()));
        }
        let both = vec![(a.clone(), listed(&h, &mut rx, &a)[0].clone()), (b.clone(), listed(&h, &mut rx, &b)[0].clone())];
        h.remove_label("both".into(), both);
        match answer(&h, &mut rx) {
            HistoryMsg::LabelRemoved { paths, name, result } => {
                assert_eq!((paths, name, result), (vec![a.clone(), b.clone()], "both".into(), Ok(2)))
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scan_queued_after_a_capture_sees_it() {
        let dir = std::fs::canonicalize(temp("recent")).unwrap();
        let (h, mut rx) = handle(&dir);
        let a = dir.join("p/a.txt");
        h.capture(Capture::text(a.clone(), "one\n".into(), Kind::Save));
        h.recent(&dir.join("p"));
        h.flush(Duration::from_secs(5));
        match next(&mut rx) {
            Ok(AppMsg::LocalHistory(HistoryMsg::Recent { scope, files })) => {
                assert_eq!(scope, dir.join("p"));
                assert_eq!(files.iter().map(|f| (f.path.clone(), f.state.clone())).collect::<Vec<_>>(), [(a, FileState::Deleted)]);
            }
            other => panic!("expected the scan, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_defaults_and_parsing() {
        let c = crate::config::Config::parse("[local_history]\nkeep_days = 0\n").unwrap().local_history;
        assert_eq!(c, LocalHistoryConfig { keep_days: 0, ..Default::default() });
        assert_eq!(LocalHistoryConfig::default().max_file_bytes(), 2048 * 1024);
        assert_eq!(LocalHistoryConfig::default().unsaved_every(), None);
        let every = |secs: &str| {
            crate::config::Config::parse(&format!("[local_history]\nsnapshot_unsaved_secs = {secs}\n"))
                .unwrap()
                .local_history
                .unsaved_every()
        };
        assert_eq!(every("5"), Some(Duration::from_secs(10)));
        assert_eq!(every("90"), Some(Duration::from_secs(90)));
    }
}
