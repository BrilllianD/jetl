//! The yman task tracker: CLI runner, JSON types and the service that loads
//! the tracker off the main loop. Results come back as `AppMsg::Yman` tagged
//! with the service generation, and `on_msg` drops stale ones.

pub mod cli;
pub mod model;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
use crate::util::coalesce::Coalesce;

pub use cli::YmanError;
pub use model::{Snapshot, TagCount, TaskDetails, TaskRow, TrackerStatus};

#[derive(Debug)]
pub enum YmanMsg {
    Snapshot { generation: u64, seq: u64, result: Result<Snapshot, YmanError> },
    Task { generation: u64, seq: u64, id: String, result: Result<TaskDetails, YmanError> },
    Tags { generation: u64, result: Result<Vec<TagCount>, YmanError> },
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
    task_seq: u64,
    pub state: TrackerState,
    pub task: Option<LoadedTask>,
    pub tags: Option<Result<Vec<TagCount>, YmanError>>,
    /// Bumped every time `state`, `task` or `tags` changes.
    pub revision: u64,
}

/// `yman ls` then `yman status`; a status error is logged and leaves
/// `status` empty (the list still counts).
pub async fn load_snapshot(program: &OsStr, root: &Path) -> Result<Snapshot, YmanError> {
    let tasks = cli::json::<Vec<TaskRow>>(program, root, &cli::list_args()).await?;
    let status = match cli::json::<TrackerStatus>(program, root, &cli::status_args()).await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("yman status: {e}");
            None
        }
    };
    Ok(Snapshot { tasks, status })
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
            task_seq: 0,
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
        if let Some(t) = &self.task {
            let id = t.id.clone();
            self.load_task(&id);
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

    pub fn load_task(&mut self, id: &str) {
        self.task_seq += 1;
        let (program, root, generation, seq, tx, id) =
            (self.program.clone(), self.root.clone(), self.generation, self.task_seq, self.tx.clone(), id.to_string());
        tokio::spawn(async move {
            let result = cli::json::<TaskDetails>(&program, &root, &cli::show_args(&id)).await;
            let _ = tx.send(AppMsg::Yman(YmanMsg::Task { generation, seq, id, result }));
        });
    }

    pub fn load_tags(&mut self) {
        let (program, root, generation, tx) = (self.program.clone(), self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let result = cli::json::<Vec<TagCount>>(&program, &root, &cli::tags_args()).await;
            let _ = tx.send(AppMsg::Yman(YmanMsg::Tags { generation, result }));
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
                if seq == self.task_seq {
                    self.task = Some(LoadedTask { id, result });
                    self.revision += 1;
                }
            }
            YmanMsg::Tags { result, .. } => {
                self.tags = Some(result);
                self.revision += 1;
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
        let (a, b) = (next(&mut rx).await, next(&mut rx).await);
        let id = |m: &YmanMsg| match m {
            YmanMsg::Task { id, .. } => id.clone(),
            _ => panic!("not a task"),
        };
        let (first, second) = if id(&a) == "2" { (a, b) } else { (b, a) };
        s.on_msg(first);
        s.on_msg(second);
        assert_eq!(s.task.as_ref().map(|t| t.id.as_str()), Some("2"));
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success();
        assert!(ok, "git {args:?}");
    }

    #[tokio::test]
    async fn reads_a_temp_tracker() {
        if !yman_available() {
            eprintln!("skipping: yman not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("jetl-yman-{}", std::process::id()));
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
            let out = std::process::Command::new(PROGRAM).args(args).current_dir(&dir).output().unwrap();
            assert!(out.status.success(), "yman {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        yman(&["init", "--offline"]);
        yman(&["add", "First", "-t", "alpha", "-m", "body text"]);
        let snap = load_snapshot(p, &dir).await.unwrap();
        assert_eq!(snap.tasks.len(), 1);
        assert_eq!(snap.tasks[0].title, "First");
        assert_eq!(snap.tasks[0].tags, ["alpha"]);
        assert!(!snap.status.expect("status").remote.origin);
        let id = snap.tasks[0].id.clone();
        let t = cli::json::<TaskDetails>(p, &dir, &show_args(&id)).await.unwrap();
        assert_eq!(t.body.trim_end(), "body text");
        let err = cli::json::<TaskDetails>(p, &dir, &show_args("999")).await.unwrap_err();
        assert!(matches!(err, YmanError::NoSuchTask(_)), "{err:?}");
        let tags = cli::json::<Vec<TagCount>>(p, &dir, &tags_args()).await.unwrap();
        assert!(tags.iter().any(|t| t.tag == "alpha"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
