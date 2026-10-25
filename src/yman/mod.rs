//! The yman task tracker: CLI runner, JSON types and the service that loads
//! the tracker off the main loop. Results come back as `AppMsg::Yman` tagged
//! with the service generation, and `on_msg` drops stale ones.

pub mod cli;
pub mod model;
pub mod panel;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
use crate::util::coalesce::Coalesce;

pub use cli::YmanError;
pub use model::{Attachment, Comment, Snapshot, TagCount, TaskDetails, TaskRow, TrackerStatus};

#[derive(Debug)]
pub enum YmanMsg {
    Snapshot { generation: u64, seq: u64, result: Result<Snapshot, YmanError> },
    Task { generation: u64, seq: u64, id: String, result: Result<TaskDetails, YmanError> },
    Tags { generation: u64, seq: u64, result: Result<Vec<TagCount>, YmanError> },
}

impl YmanMsg {
    pub fn generation(&self) -> u64 {
        match self {
            YmanMsg::Snapshot { generation, .. } | YmanMsg::Task { generation, .. } | YmanMsg::Tags { generation, .. } => *generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // one value per service; `Ready(Snapshot)` is the binding interface
pub enum TrackerState {
    /// Never wanted.
    Idle,
    /// Wanted, the first result is pending.
    Loading,
    Ready(Snapshot),
    Unavailable(YmanError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedTask {
    pub id: String,
    pub result: Result<TaskDetails, YmanError>,
}

pub struct YmanService {
    pub root: PathBuf,
    pub generation: u64,
    tx: UnboundedSender<AppMsg>,
    pub(crate) program: OsString,
    wanted: bool,
    snapshot: Coalesce,
    snapshot_seq: u64,
    applied_snapshot: u64,
    task_load: Coalesce,
    task_seq: u64,
    /// The task the newest `load_task` asked for.
    task_want: Option<String>,
    tags_load: Coalesce,
    tags_seq: u64,
    applied_tags: u64,
    pub state: TrackerState,
    pub task: Option<LoadedTask>,
    pub tags: Option<Result<Vec<TagCount>, YmanError>>,
    /// Bumped every time `state`, `task` or `tags` changes.
    pub revision: u64,
}

/// `yman ls -a`, `yman ls` (the open ids) then `yman status`; a status error is logged and leaves
/// `status` empty (the list still counts).
pub async fn load_snapshot(program: &OsStr, root: &Path) -> Result<Snapshot, YmanError> {
    let tasks = cli::json::<Vec<TaskRow>>(program, root, &cli::list_args()).await?;
    let open_listing = match cli::json::<Vec<TaskRow>>(program, root, &cli::open_args()).await {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::warn!("yman ls: {e}");
            None
        }
    };
    let open = Snapshot::open_ids(&tasks, open_listing);
    let status = match cli::json::<TrackerStatus>(program, root, &cli::status_args()).await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("yman status: {e}");
            None
        }
    };
    Ok(Snapshot { tasks, open, status })
}

impl YmanService {
    pub fn new(root: PathBuf, generation: u64, tx: UnboundedSender<AppMsg>) -> Self {
        Self {
            root,
            generation,
            tx,
            program: OsString::from(cli::PROGRAM),
            wanted: false,
            snapshot: Coalesce::default(),
            snapshot_seq: 0,
            applied_snapshot: 0,
            task_load: Coalesce::default(),
            task_seq: 0,
            task_want: None,
            tags_load: Coalesce::default(),
            tags_seq: 0,
            applied_tags: 0,
            state: TrackerState::Idle,
            task: None,
            tags: None,
            revision: 0,
        }
    }

    pub fn set_root(&mut self, root: PathBuf, generation: u64) {
        self.root = root;
        self.generation = generation;
        self.snapshot.reset();
        self.task_load.reset();
        self.tags_load.reset();
        self.task_want = None;
        self.task = None;
        self.tags = None;
        self.state = if self.wanted { TrackerState::Loading } else { TrackerState::Idle };
        self.revision += 1;
        self.refresh();
    }

    /// Start loading and keep refreshing from now on.
    pub fn want(&mut self) {
        self.wanted = true;
        if self.state == TrackerState::Idle {
            self.state = TrackerState::Loading;
            self.revision += 1;
        }
        self.refresh();
    }

    pub fn is_wanted(&self) -> bool {
        self.wanted
    }

    /// A coalesced snapshot load, plus a reload of the shown task and of the
    /// tags when they were loaded before. No-op until [`Self::want`].
    pub fn refresh(&mut self) {
        if !self.wanted {
            return;
        }
        tracing::debug!("yman refresh");
        if self.snapshot.request() {
            self.spawn_snapshot();
        }
        if self.task_want.is_some() && self.task_load.request() {
            self.spawn_task();
        }
        if self.tags.is_some() {
            self.load_tags();
        }
    }

    fn spawn_snapshot(&mut self) {
        self.snapshot_seq += 1;
        let (program, root, generation, seq, tx) =
            (self.program.clone(), self.root.clone(), self.generation, self.snapshot_seq, self.tx.clone());
        tokio::spawn(async move {
            let result = load_snapshot(&program, &root).await;
            let _ = tx.send(AppMsg::Yman(YmanMsg::Snapshot { generation, seq, result }));
        });
    }

    /// Load a task's details; one load runs at a time and the newest id
    /// asked for is loaded after it.
    pub fn load_task(&mut self, id: &str) {
        self.task_want = Some(id.to_string());
        if self.task_load.request() {
            self.spawn_task();
        }
    }

    fn spawn_task(&mut self) {
        let Some(id) = self.task_want.clone() else { return };
        self.task_seq += 1;
        let (program, root, generation, seq, tx) =
            (self.program.clone(), self.root.clone(), self.generation, self.task_seq, self.tx.clone());
        tokio::spawn(async move {
            let result = cli::json::<TaskDetails>(&program, &root, &cli::show_args(&id)).await;
            let _ = tx.send(AppMsg::Yman(YmanMsg::Task { generation, seq, id, result }));
        });
    }

    /// Load the tags; coalesced like [`Self::load_task`].
    pub fn load_tags(&mut self) {
        if self.tags_load.request() {
            self.spawn_tags();
        }
    }

    fn spawn_tags(&mut self) {
        self.tags_seq += 1;
        let (program, root, generation, seq, tx) =
            (self.program.clone(), self.root.clone(), self.generation, self.tags_seq, self.tx.clone());
        tokio::spawn(async move {
            let result = cli::json::<Vec<TagCount>>(&program, &root, &cli::tags_args()).await;
            let _ = tx.send(AppMsg::Yman(YmanMsg::Tags { generation, seq, result }));
        });
    }

    /// Apply a result; results of another generation are dropped.
    pub fn on_msg(&mut self, msg: YmanMsg) {
        if msg.generation() != self.generation {
            tracing::debug!("dropping stale yman result");
            return;
        }
        match msg {
            YmanMsg::Snapshot { seq, result, .. } => {
                let again = self.snapshot.done();
                if seq > self.applied_snapshot {
                    self.applied_snapshot = seq;
                    self.state = match result {
                        Ok(s) => TrackerState::Ready(s),
                        Err(e) => TrackerState::Unavailable(e),
                    };
                    self.revision += 1;
                }
                if again {
                    self.spawn_snapshot();
                }
            }
            YmanMsg::Task { seq, id, result, .. } => {
                let again = self.task_load.done();
                // Only the newest load of the id still wanted counts.
                if seq == self.task_seq && self.task_want.as_deref() == Some(id.as_str()) {
                    self.task = Some(LoadedTask { id, result });
                    self.revision += 1;
                }
                if again {
                    self.spawn_task();
                }
            }
            YmanMsg::Tags { seq, result, .. } => {
                let again = self.tags_load.done();
                if seq > self.applied_tags {
                    self.applied_tags = seq;
                    self.tags = Some(result);
                    self.revision += 1;
                }
                if again {
                    self.spawn_tags();
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot_requests(&self) -> u64 {
        self.snapshot.requests
    }
}

#[cfg(test)]
mod tests {
    use super::cli::{PROGRAM, list_args, show_args, tags_args, yman_available};
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn service() -> (YmanService, UnboundedReceiver<AppMsg>) {
        let (tx, rx) = unbounded_channel();
        let mut s = YmanService::new(std::env::temp_dir(), 1, tx);
        s.program = "jetl-no-such-yman-xyz".into();
        (s, rx)
    }

    async fn next(rx: &mut UnboundedReceiver<AppMsg>) -> YmanMsg {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("message in time") {
            Some(AppMsg::Yman(m)) => m,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn nothing_runs_until_wanted() {
        let (mut s, mut rx) = service();
        s.refresh();
        assert_eq!(s.snapshot_requests(), 0);
        assert_eq!(s.state, TrackerState::Idle);
        let rev = s.revision;
        s.want();
        assert_eq!(s.state, TrackerState::Loading);
        assert!(s.is_wanted());
        let m = next(&mut rx).await;
        assert!(matches!(m, YmanMsg::Snapshot { .. }));
        s.on_msg(m);
        assert_eq!(s.state, TrackerState::Unavailable(YmanError::NoBinary));
        assert!(s.revision > rev + 1);
    }

    #[tokio::test]
    async fn refreshes_coalesce() {
        let (mut s, mut rx) = service();
        s.want();
        s.refresh();
        s.refresh();
        for _ in 0..2 {
            let m = next(&mut rx).await;
            s.on_msg(m);
        }
        assert!(tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn stale_results_are_dropped() {
        let (mut s, mut rx) = service();
        s.want();
        let old = next(&mut rx).await;
        s.set_root(std::env::temp_dir(), 2);
        s.on_msg(old);
        assert_eq!(s.state, TrackerState::Loading);
        let new = next(&mut rx).await;
        assert_eq!(new.generation(), 2);
        s.on_msg(new);
        assert_eq!(s.state, TrackerState::Unavailable(YmanError::NoBinary));
    }

    #[tokio::test]
    async fn only_the_newest_task_load_counts() {
        let (mut s, mut rx) = service();
        s.load_task("1");
        s.load_task("2");
        let first = next(&mut rx).await;
        assert!(matches!(&first, YmanMsg::Task { id, .. } if id == "1"));
        s.on_msg(first);
        assert!(s.task.is_none(), "the older id is not wanted any more");
        let second = next(&mut rx).await;
        assert!(matches!(&second, YmanMsg::Task { id, .. } if id == "2"));
        s.on_msg(second);
        assert_eq!(s.task.as_ref().map(|t| t.id.as_str()), Some("2"));
        assert!(tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn refresh_keeps_the_wanted_task() {
        let (mut s, mut rx) = service();
        s.want();
        s.load_task("1");
        // The snapshot and the task result, in either order.
        for _ in 0..2 {
            let m = next(&mut rx).await;
            s.on_msg(m);
        }
        s.load_task("2");
        s.refresh();
        // Task 2, the snapshot, then the held-back task reload.
        for _ in 0..3 {
            let m = next(&mut rx).await;
            s.on_msg(m);
        }
        assert_eq!(s.task.as_ref().map(|t| t.id.as_str()), Some("2"));
    }

    #[tokio::test]
    async fn tag_loads_coalesce_and_old_results_lose() {
        let (mut s, mut rx) = service();
        s.load_tags();
        s.load_tags();
        s.load_tags();
        let first = next(&mut rx).await;
        s.on_msg(first);
        let second = next(&mut rx).await;
        let seq = |m: &YmanMsg| match m {
            YmanMsg::Tags { seq, .. } => *seq,
            _ => panic!("not tags"),
        };
        assert_eq!((s.tags_seq, seq(&second)), (2, 2));
        s.on_msg(second);
        assert!(tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err());
        let rev = s.revision;
        s.on_msg(YmanMsg::Tags { generation: 1, seq: 1, result: Ok(Vec::new()) });
        assert_eq!(s.revision, rev, "an older tag list is dropped");
    }

    /// A command that cannot reach any repository but the one in `dir`.
    fn isolated(program: &str, dir: &Path) -> std::process::Command {
        let mut c = std::process::Command::new(program);
        c.current_dir(dir).env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE");
        c
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = isolated("git", dir).args(args).status().unwrap().success();
        assert!(ok, "git {args:?}");
    }

    /// A temp directory removed on drop, also when the test fails.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn reads_a_temp_tracker() {
        if !yman_available() {
            eprintln!("skipping: yman not on PATH");
            return;
        }
        let guard = TempDir(std::env::temp_dir().join(format!("jetl-yman-{}", std::process::id())));
        let dir = guard.0.clone();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = OsStr::new(PROGRAM);
        assert_eq!(cli::json::<Vec<TaskRow>>(p, &dir, &list_args()).await.unwrap_err(), YmanError::NotGitRepo);
        git(&dir, &["init", "-q"]);
        crate::git::repo::tests::isolate(&dir);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "user.email", "t@t"]);
        assert_eq!(cli::json::<Vec<TaskRow>>(p, &dir, &list_args()).await.unwrap_err(), YmanError::NotInitialized);
        let yman = |args: &[&str]| {
            let out = isolated(PROGRAM, &dir).args(args).output().unwrap();
            assert!(out.status.success(), "yman {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        yman(&["init", "--offline"]);
        yman(&["add", "First", "-t", "alpha", "-m", "body text"]);
        yman(&["add", "Second"]);
        let first = load_snapshot(p, &dir).await.unwrap();
        let id2 = first.tasks.iter().find(|t| t.title == "Second").unwrap().id.clone();
        yman(&["done", &id2]);
        let snap = load_snapshot(p, &dir).await.unwrap();
        assert_eq!(snap.tasks.len(), 2);
        let row = snap.tasks.iter().find(|t| t.title == "First").unwrap();
        assert_eq!(row.tags, ["alpha"]);
        assert!(snap.open.contains(&row.id));
        assert!(!snap.open.contains(&id2), "a done task is closed");
        assert!(!snap.status.expect("status").remote.origin);
        let id = row.id.clone();
        let t = cli::json::<TaskDetails>(p, &dir, &show_args(&id)).await.unwrap();
        assert_eq!(t.body.trim_end(), "body text");
        let err = cli::json::<TaskDetails>(p, &dir, &show_args("999")).await.unwrap_err();
        assert!(matches!(err, YmanError::NoSuchTask(_)), "{err:?}");
        let tags = cli::json::<Vec<TagCount>>(p, &dir, &tags_args()).await.unwrap();
        assert!(tags.iter().any(|t| t.tag == "alpha"));
    }
}
