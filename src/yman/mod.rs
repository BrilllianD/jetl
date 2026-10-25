//! The yman task tracker: CLI runner, JSON types and the service that loads
//! the tracker off the main loop. Results come back as `AppMsg::Yman` tagged
//! with the service generation, and `on_msg` drops stale ones. Writes run one
//! at a time through `mutate`; the result comes back from `on_msg` for the App
//! to report, after a refresh. `yman sync` is one of those writes
//! (`MutationKind::Sync`).

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
    Mutation { generation: u64, done: MutationDone },
}

impl YmanMsg {
    pub fn generation(&self) -> u64 {
        match self {
            YmanMsg::Snapshot { generation, .. }
            | YmanMsg::Task { generation, .. }
            | YmanMsg::Tags { generation, .. }
            | YmanMsg::Mutation { generation, .. } => *generation,
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

/// What the App does after a mutation; 310 adds `Sync`, E4 adds `Add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationKind {
    /// A change of this task (status, priority, assignee, tags, comment).
    Task(String),
    /// `yman sync`: fetch, merge and push the task history.
    Sync,
}

/// One yman write command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    pub kind: MutationKind,
    /// Short human text, e.g. `Done #12`: shown while it runs (`Tasks` title
    /// `· <label>…`) and as the prefix of its notifications.
    pub label: String,
    /// yman's arguments, without the program (`cli::*_args` builders).
    pub args: Vec<String>,
    /// Written to yman's stdin, which is then closed; `None` = stdin is /dev/null.
    pub input: Option<Vec<u8>>,
}

/// The result `on_msg` hands back to the App.
#[derive(Debug)]
pub struct MutationDone {
    pub kind: MutationKind,
    pub label: String,
    /// `Ok` only for exit code 0; `stdout` holds yman's data output (for
    /// example the new id of `yman add`), `stderr` its notes and warnings.
    pub result: Result<cli::YmanOutput, YmanError>,
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
    /// The mutation in flight; one at a time.
    pub(crate) running: Option<Mutation>,
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
            running: None,
        }
    }

    pub fn set_root(&mut self, root: PathBuf, generation: u64) {
        self.root = root;
        self.generation = generation;
        self.snapshot.reset();
        self.task_load.reset();
        self.tags_load.reset();
        self.task_want = None;
        self.running = None;
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

    /// Start a write command. `false`, with nothing started, while another
    /// one runs. Does not need [`Self::want`].
    pub fn mutate(&mut self, m: Mutation) -> bool {
        if self.running.is_some() {
            return false;
        }
        let (program, root, generation, tx) = (self.program.clone(), self.root.clone(), self.generation, self.tx.clone());
        let (kind, label, args, input) = (m.kind.clone(), m.label.clone(), m.args.clone(), m.input.clone());
        self.running = Some(m);
        self.revision += 1;
        tokio::spawn(async move {
            let result = cli::run_ok(&program, &root, &args, input).await;
            let _ = tx.send(AppMsg::Yman(YmanMsg::Mutation { generation, done: MutationDone { kind, label, result } }));
        });
        true
    }

    /// The label of the running mutation.
    pub fn busy(&self) -> Option<&str> {
        self.running.as_ref().map(|m| m.label.as_str())
    }

    /// The tracker status of the loaded snapshot; `None` before it loaded or
    /// when `yman status` failed.
    pub fn tracker_status(&self) -> Option<&TrackerStatus> {
        match &self.state {
            TrackerState::Ready(s) => s.status.as_ref(),
            _ => None,
        }
    }

    /// The loaded status says there is no `origin`, so `yman sync` cannot run.
    pub fn local_only(&self) -> bool {
        self.tracker_status().is_some_and(|s| !s.remote.origin)
    }

    /// Apply a result; results of another generation are dropped. A finished
    /// mutation is returned for the App to report, after a refresh.
    pub fn on_msg(&mut self, msg: YmanMsg) -> Option<MutationDone> {
        if msg.generation() != self.generation {
            tracing::debug!("dropping stale yman result");
            return None;
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
                None
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
                None
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
                None
            }
            YmanMsg::Mutation { done, .. } => {
                self.running = None;
                self.revision += 1;
                self.refresh();
                Some(done)
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

    fn mutation(label: &str, args: &[&str], input: Option<Vec<u8>>) -> Mutation {
        Mutation { kind: MutationKind::Task("1".into()), label: label.into(), args: args.iter().map(|a| a.to_string()).collect(), input }
    }

    async fn next_mutation(s: &mut YmanService, rx: &mut UnboundedReceiver<AppMsg>) -> MutationDone {
        loop {
            let m = next(rx).await;
            let is_mutation = matches!(m, YmanMsg::Mutation { .. });
            let done = s.on_msg(m);
            if is_mutation {
                return done.expect("a current mutation result");
            }
            assert!(done.is_none());
        }
    }

    #[tokio::test]
    async fn mutations_run_one_at_a_time_and_refresh() {
        let (mut s, mut rx) = service();
        s.want();
        let first = next(&mut rx).await;
        s.on_msg(first);
        let before = s.snapshot_requests();
        s.program = "sh".into();
        assert!(s.mutate(mutation("Test #1", &["-c", "echo 'note: hi' >&2"], None)));
        assert_eq!(s.busy(), Some("Test #1"));
        assert!(!s.mutate(mutation("Other #1", &["-c", "true"], None)));
        assert_eq!(s.busy(), Some("Test #1"));
        let done = next_mutation(&mut s, &mut rx).await;
        assert_eq!(done.label, "Test #1");
        assert_eq!(done.result.unwrap().stderr.trim(), "note: hi");
        assert_eq!(s.busy(), None);
        assert!(s.snapshot_requests() > before);
    }

    #[tokio::test]
    async fn mutation_failure_maps_the_exit() {
        let (mut s, mut rx) = service();
        s.program = "sh".into();
        assert!(s.mutate(mutation("Test #9", &["-c", "echo 'error: task 9 not found' >&2; exit 4"], None)));
        let done = next_mutation(&mut s, &mut rx).await;
        assert_eq!(done.result.unwrap_err(), YmanError::NoSuchTask("task 9 not found".into()));
        assert_eq!(s.snapshot_requests(), 0);
        assert_eq!(s.busy(), None);
    }

    #[tokio::test]
    async fn stale_mutation_is_dropped() {
        let (mut s, mut rx) = service();
        s.program = "sh".into();
        assert!(s.mutate(mutation("Test #1", &["-c", "true"], None)));
        s.set_root(std::env::temp_dir(), 2);
        assert_eq!(s.busy(), None);
        let old = next(&mut rx).await;
        assert!(matches!(old, YmanMsg::Mutation { generation: 1, .. }));
        assert!(s.on_msg(old).is_none());
    }

    /// A git repository with an offline yman tracker, removed with the guard.
    async fn temp_tracker(tag: &str) -> TempDir {
        let guard = TempDir(std::env::temp_dir().join(format!("jetl-yman-{tag}-{}", std::process::id())));
        let dir = guard.0.clone();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        crate::git::repo::tests::isolate(&dir);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "user.email", "t@t"]);
        let out = isolated(PROGRAM, &dir).args(["init", "--offline"]).output().unwrap();
        assert!(out.status.success(), "yman init: {}", String::from_utf8_lossy(&out.stderr));
        guard
    }

    #[tokio::test]
    async fn mutations_change_a_temp_tracker() {
        use cli::StatusChange::{Done, Start};
        if !yman_available() {
            eprintln!("skipping: yman not on PATH");
            return;
        }
        let guard = temp_tracker("mut").await;
        let dir = guard.0.clone();
        let out = isolated(PROGRAM, &dir).args(["add", "First", "-t", "alpha"]).output().unwrap();
        assert!(out.status.success(), "yman add: {}", String::from_utf8_lossy(&out.stderr));
        let p = OsStr::new(PROGRAM);
        let id = load_snapshot(p, &dir).await.unwrap().tasks[0].id.clone();
        let (tx, mut rx) = unbounded_channel();
        let mut s = YmanService::new(dir.clone(), 1, tx);
        let steps = [
            cli::status_change_args(&id, &Start, None),
            cli::priority_args(&id, 3),
            cli::assign_args(&id, Some("bob")),
            cli::tag_change_args(&id, &["x".to_string()], &["alpha".to_string()]),
            cli::comment_args(&id),
            cli::status_change_args(&id, &Done, Some("-shipped")),
        ];
        for (i, args) in steps.into_iter().enumerate() {
            let input = (i == 4).then(|| b"-dash comment".to_vec());
            let m = Mutation { kind: MutationKind::Task(id.clone()), label: format!("Step {i}"), args, input };
            assert!(s.mutate(m));
            let done = next_mutation(&mut s, &mut rx).await;
            if let Err(e) = &done.result {
                panic!("step {i}: {e}");
            }
        }
        let t = cli::json::<TaskDetails>(p, &dir, &show_args(&id)).await.unwrap();
        assert_eq!(t.status, "done");
        assert_eq!(t.priority, 3);
        assert_eq!(t.assignee.as_deref(), Some("bob"));
        assert_eq!(t.tags, ["x"]);
        let texts: Vec<&str> = t.discussion.iter().map(|c| c.text.as_str()).collect();
        assert!(texts.iter().any(|c| c.contains("-dash comment")), "{texts:?}");
        assert!(texts.iter().any(|c| c.contains("-shipped")), "{texts:?}");
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

    #[tokio::test]
    async fn sync_reaches_a_local_bare_remote() {
        if !yman_available() {
            eprintln!("skipping: yman not on PATH");
            return;
        }
        let guard = TempDir(std::env::temp_dir().join(format!("jetl-yman-sync-{}", std::process::id())));
        let base = guard.0.clone();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let remote = base.join("remote.git");
        git(&base, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q"]);
        crate::git::repo::tests::isolate(&work);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["remote", "add", "origin", remote.to_str().unwrap()]);
        for args in [&["init"][..], &["add", "First"][..]] {
            let out = isolated(PROGRAM, &work).args(args).output().unwrap();
            assert!(out.status.success(), "yman {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
        let p = OsStr::new(PROGRAM);
        let st = cli::json::<TrackerStatus>(p, &work, &cli::status_args()).await.unwrap();
        assert!(st.remote.origin && st.remote.ahead >= 1, "{st:?}");
        let sync = || Mutation { kind: MutationKind::Sync, label: "Sync tasks".into(), args: vec!["sync".into()], input: None };
        let (tx, mut rx) = unbounded_channel();
        let mut s = YmanService::new(work.clone(), 1, tx);
        assert!(s.mutate(sync()));
        let done = next_mutation(&mut s, &mut rx).await;
        assert_eq!(done.kind, MutationKind::Sync);
        let out = done.result.unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("synced"), "{out:?}");
        let st = cli::json::<TrackerStatus>(p, &work, &cli::status_args()).await.unwrap();
        assert!(st.remote.fetched && st.remote.ahead == 0 && st.remote.behind == 0, "{st:?}");

        let solo = base.join("solo");
        std::fs::create_dir_all(&solo).unwrap();
        git(&solo, &["init", "-q"]);
        crate::git::repo::tests::isolate(&solo);
        git(&solo, &["config", "user.name", "t"]);
        git(&solo, &["config", "user.email", "t@t"]);
        let out = isolated(PROGRAM, &solo).args(["init", "--offline"]).output().unwrap();
        assert!(out.status.success(), "yman init: {}", String::from_utf8_lossy(&out.stderr));
        let (tx, mut rx) = unbounded_channel();
        let mut s = YmanService::new(solo.clone(), 1, tx);
        assert!(s.mutate(sync()));
        let done = next_mutation(&mut s, &mut rx).await;
        let e = done.result.unwrap_err();
        assert!(e.message().contains("no \"origin\" remote"), "{}", e.message());
    }
}
