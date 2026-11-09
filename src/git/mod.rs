//! Git integration: a service that runs git2 work in `spawn_blocking` and
//! `git` CLI commands in tokio tasks, the Git tool window, and the diff code
//! shared with the editor gutter.
//!
//! Every result comes back as `AppMsg::Git` tagged with the service
//! generation; the app drops results whose generation is stale (the root
//! changed while they ran).

pub mod cli;
pub mod diff;
pub mod graph;
pub mod merge;
pub mod messages;
pub mod model;
pub mod panel;
pub mod rebase;
pub mod repo;
pub mod shelf;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
pub use model::{
    Blame, BranchInfo, CommitDetails, DiffKey, InProgress, InProgressKind, LogFilter, LogRev, Section, StashEntry, StatusSnapshot,
};
pub use panel::GitPanel;
pub use repo::BlobWhich;

/// Operations that change repository state. Their results arrive as
/// [`GitMsg::OpDone`] and trigger a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitOp {
    Stage,
    Unstage,
    /// Throw away working tree (and staged) changes of some paths.
    Discard,
    /// Stash push, pop, apply or drop.
    Stash,
    /// Shelve changes as a patch (carries the entry name).
    Shelve(String),
    /// Apply a shelf entry (carries the entry name).
    Unshelve(String),
    Commit {
        push: bool,
        amend: bool,
    },
    Push,
    /// `git push -u <remote> <branch>`; carries the branch.
    PushUpstream(String),
    Pull,
    Fetch,
    Switch(String),
    NewBranch(String),
    /// `git switch --detach <sha>`.
    Checkout(String),
    CherryPick(String),
    Revert(String),
    Reset {
        sha: String,
        mode: model::ResetMode,
    },
    Merge(String),
    Rebase(String),
    /// `git rebase -i` of a jetl todo; carries the base's short sha or "root".
    InteractiveRebase(String),
    RenameBranch(String),
    DeleteBranch {
        name: String,
        force: bool,
    },
    Tag(String),
    DeleteTag(String),
    /// `git worktree add`; `path` is the new worktree.
    WorktreeAdd {
        path: PathBuf,
    },
    /// `git -C <main> worktree remove [--force] <path>`.
    WorktreeRemove {
        path: PathBuf,
        force: bool,
    },
    WorktreePrune,
    /// A conflicted path resolved by taking one side whole; carries the
    /// console line's path.
    Resolve(String),
    /// `git <op> --continue|--skip|--abort`.
    Progress {
        kind: model::InProgressKind,
        action: model::ProgressAction,
    },
}

impl GitOp {
    /// Whether the operation may rewrite files in the work tree.
    pub fn touches_worktree(&self) -> bool {
        matches!(
            self,
            GitOp::Pull
                | GitOp::Switch(_)
                | GitOp::NewBranch(_)
                | GitOp::Discard
                | GitOp::Stash
                | GitOp::Shelve(_)
                | GitOp::Unshelve(_)
                | GitOp::Checkout(_)
                | GitOp::CherryPick(_)
                | GitOp::Revert(_)
                | GitOp::Reset { mode: model::ResetMode::Hard, .. }
                | GitOp::Merge(_)
                | GitOp::Rebase(_)
                | GitOp::InteractiveRebase(_)
                | GitOp::Resolve(_)
                | GitOp::Progress { .. }
        )
    }
}

#[derive(Debug)]
pub enum GitMsg {
    Status {
        generation: u64,
        seq: u64,
        result: Result<StatusSnapshot>,
    },
    Blob {
        generation: u64,
        /// Request order; the editor keeps the newest per buffer.
        seq: u64,
        path: PathBuf,
        which: BlobWhich,
        text: Option<String>,
    },
    Diff {
        generation: u64,
        key: DiffKey,
        result: Result<diff::DiffDoc>,
    },
    Branches {
        generation: u64,
        result: Result<Vec<BranchInfo>>,
    },
    /// A log page for `filter`; the panel drops pages of another filter.
    Log {
        generation: u64,
        skip: usize,
        filter: LogFilter,
        result: Result<Vec<model::LogCommit>>,
    },
    CommitsDiff {
        generation: u64,
        title: String,
        spans: Vec<model::DiffSpan>,
        result: Result<String>,
    },
    CommitDetails {
        generation: u64,
        sha: String,
        result: Result<CommitDetails>,
    },
    /// The union of the files of `spans`; the Log tab drops it unless the
    /// spans are the current selection's.
    CommitsFiles {
        generation: u64,
        spans: Vec<model::DiffSpan>,
        tab: Option<String>,
        result: Result<Vec<(model::CommitFile, DiffKey)>>,
    },
    /// Blame of an open file; the editor keeps the newest per buffer.
    Blame {
        generation: u64,
        seq: u64,
        path: PathBuf,
        result: Result<Blame>,
    },
    Stashes {
        generation: u64,
        result: Result<Vec<StashEntry>>,
    },
    /// The shelf of the project; the app drops one whose `version` is not newer
    /// than the one it holds. `done` is a toast for the operation that answered.
    Shelf {
        generation: u64,
        version: u64,
        result: Result<Vec<shelf::ShelfEntry>, String>,
        done: Option<String>,
    },
    /// The patch of a shelf entry, to open in a patch tab.
    ShelfPatch {
        generation: u64,
        id: u64,
        title: String,
        result: Result<String, String>,
    },
    /// The HEAD commit message, to prefill the box when amend turns on.
    HeadMessage {
        generation: u64,
        result: Result<String>,
    },
    /// The commit message history of the repository, newest first.
    CommitMessages {
        generation: u64,
        messages: Vec<String>,
    },
    /// The remote for `git push -u` of `branch` (after a push failed for
    /// lack of an upstream).
    PushRemote {
        generation: u64,
        branch: String,
        result: Result<repo::PushRemote>,
    },
    /// The interactive rebase plan from the commit asked for, or why not.
    RebasePlan {
        generation: u64,
        result: Result<rebase::RebasePlan, String>,
    },
    /// The stages and work file of a conflicted path, for the resolver.
    Conflict {
        generation: u64,
        path: PathBuf,
        result: Result<merge::ConflictSides>,
    },
    /// A finished operation; `label` is what the console and spinner show.
    OpDone {
        generation: u64,
        id: u64,
        op: GitOp,
        label: String,
        ok: bool,
        output: String,
        /// Files the operation renamed in the work tree (old, new), absolute.
        renames: Vec<(PathBuf, PathBuf)>,
    },
}

/// The tree and staged renames before a work tree operation (`None`: not
/// tracked, or the repository is unreadable).
async fn base_of(root: PathBuf, track: bool) -> Option<repo::RenameBase> {
    if !track {
        return None;
    }
    tokio::task::spawn_blocking(move || repo::rename_base(&root).ok()).await.ok().flatten()
}

/// The files the operation renamed since `base`.
async fn renames_after(root: PathBuf, base: Option<repo::RenameBase>) -> Vec<(PathBuf, PathBuf)> {
    let Some(base) = base else { return Vec::new() };
    tokio::task::spawn_blocking(move || repo::renames_since(&root, &base)).await.unwrap_or_default()
}

impl GitMsg {
    pub fn generation(&self) -> u64 {
        match self {
            GitMsg::Status { generation, .. }
            | GitMsg::Blob { generation, .. }
            | GitMsg::Diff { generation, .. }
            | GitMsg::Branches { generation, .. }
            | GitMsg::Log { generation, .. }
            | GitMsg::CommitDetails { generation, .. }
            | GitMsg::CommitsFiles { generation, .. }
            | GitMsg::CommitsDiff { generation, .. }
            | GitMsg::PushRemote { generation, .. }
            | GitMsg::RebasePlan { generation, .. }
            | GitMsg::HeadMessage { generation, .. }
            | GitMsg::Stashes { generation, .. }
            | GitMsg::Shelf { generation, .. }
            | GitMsg::ShelfPatch { generation, .. }
            | GitMsg::Blame { generation, .. }
            | GitMsg::CommitMessages { generation, .. }
            | GitMsg::Conflict { generation, .. }
            | GitMsg::OpDone { generation, .. } => *generation,
        }
    }
}

/// Requests panels make through `Effect::Git`; the app forwards them to
/// the service (or opens dialogs).
#[derive(Debug, Clone)]
pub enum GitRequest {
    Refresh,
    Stage(Vec<String>),
    Unstage(Vec<String>),
    StageAll,
    UnstageAll,
    /// Confirmed already (`DialogId::GitDiscard`).
    Discard(Vec<(Section, String)>),
    /// `amend` replaces HEAD instead of adding a commit.
    Commit {
        message: String,
        push: bool,
        amend: bool,
    },
    LoadHeadMessage,
    Stash(repo::StashOp),
    LoadStashes,
    /// Shelve changes under a name (the dialog has been answered).
    Shelve {
        name: String,
        what: shelf::ShelveWhat,
    },
    /// Apply a shelf entry, or one file of it.
    Unshelve {
        id: u64,
        name: String,
        file: Option<String>,
    },
    /// Confirmed already (`DialogId::ShelfDelete`).
    ShelfDelete {
        id: u64,
        name: String,
    },
    LoadShelf,
    /// Open a shelf entry's patch in a patch tab titled `title`.
    ShelfPatch {
        id: u64,
        title: String,
    },
    LoadBlame(PathBuf, Option<PathBuf>),
    CheckoutCommit(String),
    CherryPick(String),
    /// Confirmed already (`DialogId::RevertCommit`).
    Revert(String),
    /// `git reset <mode> <sha>`. Confirmed already when hard.
    Reset {
        sha: String,
        mode: model::ResetMode,
    },
    NewBranchAt {
        name: String,
        rev: String,
    },
    /// The branches popup's operations for this branch.
    BranchOps(BranchInfo),
    /// Run one of the branches popup's operations (`App::branch_op`).
    BranchOp {
        branch: BranchInfo,
        op: model::BranchOp,
    },
    Merge(String),
    /// Confirmed already (`DialogId::RebaseOnto`).
    Rebase(String),
    InteractiveRebase(rebase::RebasePlan),
    /// Load the interactive rebase plan from this commit; answered by `GitMsg::RebasePlan`.
    LoadRebasePlan(String),
    CheckoutTracking(String),
    RenameBranch {
        from: String,
        to: String,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    NewTag {
        name: String,
        sha: String,
    },
    DeleteTag(String),
    /// Show a commit in the Log tab (from the blame column).
    ShowCommit(String),
    /// `git apply` a patch (a diff block); `op` is what it counts as
    /// (stage, unstage or discard).
    ApplyPatch {
        patch: String,
        cached: bool,
        reverse: bool,
        op: GitOp,
    },
    Push,
    /// Git > Push…: the dialog with tags and force with lease.
    PushDialog,
    PushWith {
        tags: bool,
        force_lease: bool,
    },
    Pull,
    Fetch,
    BranchesPopup,
    NewBranch,
    LoadDiff(DiffKey),
    LoadLog {
        skip: usize,
        filter: LogFilter,
    },
    LoadCommit(String),
    /// The union of the files changed by commit spans, answered with
    /// `GitMsg::CommitsFiles`.
    LoadCommitsFiles(Vec<model::DiffSpan>),
    /// The union of the files of spans, opened as a diff tab titled title (GitMsg::CommitsFiles with tab set).
    CommitsFilesTab {
        title: String,
        spans: Vec<model::DiffSpan>,
    },
    /// The combined diff of commit spans, opened as a patch tab.
    CommitsDiff {
        title: String,
        spans: Vec<model::DiffSpan>,
    },
    LoadWorktrees,
    WorktreePrune,
    /// Read a conflicted file's stages and open the resolver on them.
    LoadConflict(PathBuf),
    /// Resolve a conflicted path (relative to the work tree) by taking
    /// one side whole.
    ResolveSide {
        rel: String,
        side: repo::ConflictSide,
    },
    /// Stage a conflicted path the resolver just wrote (relative to the work tree); runs as GitOp::Resolve.
    StageResolved(String),
    /// Continue, skip or abort the operation in progress.
    Progress(model::ProgressAction),
}

impl GitRequest {
    /// Whether git refuses this request while a `kind` operation is in progress.
    pub fn refused_mid_operation(&self, kind: model::InProgressKind) -> bool {
        use model::InProgressKind as K;
        (matches!(kind, K::Merge | K::Rebase | K::CherryPick | K::Revert | K::Am) && matches!(self, GitRequest::Commit { .. }))
            || kind.blocks_switching()
                && matches!(
                    self,
                    GitRequest::CheckoutCommit(_)
                        | GitRequest::CherryPick(_)
                        | GitRequest::Revert(_)
                        | GitRequest::NewBranchAt { .. }
                        | GitRequest::NewBranch
                        | GitRequest::Merge(_)
                        | GitRequest::Rebase(_)
                        | GitRequest::InteractiveRebase(_)
                        | GitRequest::LoadRebasePlan(_)
                        | GitRequest::CheckoutTracking(_)
                        | GitRequest::Shelve { .. }
                        | GitRequest::Unshelve { .. }
                )
    }
}

pub struct GitService {
    pub root: PathBuf,
    /// The workspace generation of `root`, copied in by `set_root`.
    pub generation: u64,
    tx: UnboundedSender<AppMsg>,
    next_id: u64,
    status_seq: AtomicU64,
    /// At most one status walk at a time.
    status: crate::util::coalesce::Coalesce,
    blob_seq: AtomicU64,
    blame_seq: AtomicU64,
    /// Running operations (id, label), oldest first.
    pub busy: Vec<(u64, String)>,
    pub tab_width: usize,
    /// Where commit message histories live (`git::messages`).
    pub messages_dir: PathBuf,
    /// Local history: an operation that rewrites the work tree waits for
    /// a snapshot of the changed files first (set by `App::new`).
    pub history: crate::history::Handle,
}

/// Each step as `git <args>`, joined with " && ".
fn steps_label(steps: &[Vec<String>]) -> String {
    steps.iter().map(|a| format!("git {}", a.join(" "))).collect::<Vec<_>>().join(" && ")
}

impl GitService {
    pub fn new(root: PathBuf, generation: u64, tx: UnboundedSender<AppMsg>) -> Self {
        Self {
            root,
            generation,
            tx,
            next_id: 1,
            status_seq: AtomicU64::new(0),
            status: Default::default(),
            blob_seq: AtomicU64::new(0),
            blame_seq: AtomicU64::new(0),
            busy: Vec::new(),
            tab_width: 4,
            messages_dir: PathBuf::new(),
            history: crate::history::Handle::disabled(),
        }
    }

    /// Point the service at a new root (`Workspace::generation` already
    /// bumped). Results of work started for the old root are dropped by
    /// generation.
    pub fn set_root(&mut self, root: PathBuf, generation: u64) {
        self.root = root;
        self.generation = generation;
        self.busy.clear();
        self.status.reset();
    }

    #[cfg(test)]
    pub fn status_requests(&self) -> u64 {
        self.status.requests
    }

    pub fn is_busy(&self) -> bool {
        !self.busy.is_empty()
    }

    /// Mark an operation finished (any generation).
    pub fn finish(&mut self, id: u64) {
        self.busy.retain(|(i, _)| *i != id);
    }

    fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Path) -> T + Send + 'static,
        wrap: impl FnOnce(u64, T) -> GitMsg + Send + 'static,
    ) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let out = work(&root);
            let _ = tx.send(AppMsg::Git(wrap(generation, out)));
        });
    }

    /// Load the status, or once more after the walk in flight finishes.
    pub fn refresh_status(&mut self) {
        if self.status.request() {
            self.spawn_status();
        }
    }

    /// A status result of the current generation arrived.
    pub fn status_done(&mut self) {
        if self.status.done() {
            self.spawn_status();
        }
    }

    fn spawn_status(&self) {
        let seq = self.status_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.blocking(repo::status, move |generation, result| GitMsg::Status { generation, seq, result });
    }

    pub fn load_blob(&self, path: PathBuf, which: BlobWhich, fallback: Option<PathBuf>) {
        let p = path.clone();
        let seq = self.blob_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.blocking(
            move |root| repo::blob_text(root, &p, which, fallback.as_deref()),
            move |generation, text| GitMsg::Blob { generation, seq, path, which, text },
        );
    }

    pub fn load_conflict(&self, path: PathBuf) {
        let p = path.clone();
        self.blocking(move |root| repo::conflict_sides(root, &p), move |generation, result| GitMsg::Conflict { generation, path, result });
    }

    pub fn load_branches(&self) {
        self.blocking(repo::branches, |generation, result| GitMsg::Branches { generation, result });
    }

    pub fn load_blame(&self, path: PathBuf, fallback: Option<PathBuf>) {
        let p = path.clone();
        let seq = self.blame_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.blocking(
            move |root| repo::blame(root, &p, fallback.as_deref()),
            move |generation, result| GitMsg::Blame { generation, seq, path, result },
        );
    }

    pub fn load_stashes(&self) {
        self.blocking(repo::stashes, |generation, result| GitMsg::Stashes { generation, result });
    }

    pub fn load_head_message(&self) {
        self.blocking(repo::head_message, |generation, result| GitMsg::HeadMessage { generation, result });
    }

    pub fn load_messages(&self) {
        let dir = self.messages_dir.clone();
        self.blocking(move |root| messages::load(&dir, root), |generation, messages| GitMsg::CommitMessages { generation, messages });
    }

    pub fn load_push_remote(&self, branch: String) {
        let b = branch.clone();
        self.blocking(move |root| repo::push_remote(root, &b), move |generation, result| GitMsg::PushRemote { generation, branch, result });
    }

    pub fn load_rebase_plan(&self, sha: String) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let result = rebase::load_plan(&root, &sha).await;
            let _ = tx.send(AppMsg::Git(GitMsg::RebasePlan { generation, result }));
        });
    }

    pub fn load_diff(&self, key: DiffKey) {
        let tab_width = self.tab_width;
        match key.clone() {
            DiffKey::Change { section, rel, old } => {
                self.blocking(
                    move |root| {
                        repo::change_texts(root, section, &rel, old.as_deref())
                            .map(|(old, new)| diff::DiffDoc::build(&old, &new, tab_width))
                    },
                    move |generation, result| GitMsg::Diff { generation, key, result },
                );
            }
            DiffKey::Commit { sha, rel, old } => {
                let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
                tokio::spawn(async move {
                    let result = async {
                        let old = cli::show_file(&root, &format!("{sha}^"), old.as_deref().unwrap_or(&rel)).await?;
                        let new = cli::show_file(&root, &sha, &rel).await?;
                        tokio::task::spawn_blocking(move || diff::DiffDoc::build(&old, &new, tab_width)).await.map_err(anyhow::Error::from)
                    }
                    .await;
                    let _ = tx.send(AppMsg::Git(GitMsg::Diff { generation, key, result }));
                });
            }
            DiffKey::Range { base, top, rel, old } => {
                let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
                tokio::spawn(async move {
                    let result = async {
                        let old = match &base {
                            Some(b) => cli::show_file(&root, b, old.as_deref().unwrap_or(&rel)).await?,
                            None => Vec::new(),
                        };
                        let new = cli::show_file(&root, &top, &rel).await?;
                        tokio::task::spawn_blocking(move || diff::DiffDoc::build(&old, &new, tab_width)).await.map_err(anyhow::Error::from)
                    }
                    .await;
                    let _ = tx.send(AppMsg::Git(GitMsg::Diff { generation, key, result }));
                });
            }
            // The Local History window loads these itself (`App::load_local_diff`).
            DiffKey::Local { .. } | DiffKey::Text { .. } => {
                tracing::debug!("in-memory or local history diff reached the git service: {key:?}")
            }
        }
    }

    pub fn load_log(&self, skip: usize, count: usize, all: bool, filter: LogFilter) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let out = cli::git(&root, &cli::log_args(skip, count, all, &filter)).await;
            let result = out.map(|o| {
                // An unborn branch has no log; show it as empty.
                if o.ok { cli::parse_log(&o.stdout_text()) } else { Vec::new() }
            });
            let _ = tx.send(AppMsg::Git(GitMsg::Log { generation, skip, filter, result }));
        });
    }

    /// The combined diff of `spans`, answered with `GitMsg::CommitsDiff`.
    pub fn load_commits_diff(&self, title: String, spans: Vec<model::DiffSpan>) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let result = cli::commits_diff(&root, &spans).await;
            let _ = tx.send(AppMsg::Git(GitMsg::CommitsDiff { generation, title, spans, result }));
        });
    }

    /// The union of the files `spans` change, answered with `GitMsg::CommitsFiles`.
    pub fn load_commits_files(&self, spans: Vec<model::DiffSpan>, tab: Option<String>) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let result = cli::spans_files(&root, &spans).await;
            let _ = tx.send(AppMsg::Git(GitMsg::CommitsFiles { generation, spans, tab, result }));
        });
    }

    pub fn load_commit(&self, sha: String) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let result = cli::commit_details(&root, &sha).await;
            let _ = tx.send(AppMsg::Git(GitMsg::CommitDetails { generation, sha, result }));
        });
    }

    /// `git worktree list --porcelain`, answered with `AppMsg::Worktrees`
    /// (same generation rule as `GitMsg`). Paths are canonicalized so they
    /// compare with the project root.
    pub fn load_worktrees(&self) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let result = async {
                let out = cli::git_ok(&root, &cli::args(&["worktree", "list", "--porcelain"])).await?;
                let mut list = crate::worktree::parse_porcelain(&out.stdout_text());
                for w in &mut list {
                    if let Ok(p) = tokio::fs::canonicalize(&w.path).await {
                        w.path = p;
                    }
                }
                Ok(list)
            }
            .await;
            let _ = tx.send(AppMsg::Worktrees { generation, result });
        });
    }

    fn start(&mut self, label: &str) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.busy.push((id, label.to_string()));
        id
    }

    /// A local history snapshot of the changed files, asked for before an
    /// operation that rewrites the work tree. `worktree remove --force`
    /// snapshots the worktree it deletes, not the root; a plain remove is
    /// not snapshotted, since git refuses it when any file is modified or
    /// untracked (ignored files go either way and are never snapshotted).
    /// The `.git` check keeps a directory that is no longer a worktree from
    /// snapshotting an enclosing repository instead.
    fn before(&self, op: &GitOp, label: &str) -> Option<crate::history::Ack> {
        match op {
            GitOp::WorktreeRemove { path, force: true } if path.join(".git").exists() => {
                self.history.before_git(path, "git worktree remove --force")
            }
            _ if op.touches_worktree() => self.history.before_git(&self.root, label),
            _ => None,
        }
    }

    /// Run a git2 operation in `spawn_blocking`. `work` returns a line for
    /// the console on success.
    pub fn run_repo(&mut self, op: GitOp, label: String, work: impl FnOnce(&Path) -> Result<String> + Send + 'static) -> u64 {
        let id = self.start(&label);
        let pre = self.before(&op, &label);
        let track = op.touches_worktree() && self.history.enabled();
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            if let Some(ack) = pre {
                ack.wait();
            }
            let base = if track { repo::rename_base(&root).ok() } else { None };
            let (ok, output) = match work(&root) {
                Ok(s) => (true, s),
                Err(e) => (false, format!("{e:#}")),
            };
            let renames = base.map(|b| repo::renames_since(&root, &b)).unwrap_or_default();
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op, label, ok, output, renames }));
        });
        id
    }

    /// Run `git <args>` as an operation; output goes to the console.
    /// Worktree commands use this with their own `GitOp`s.
    pub fn run_cli(&mut self, op: GitOp, args: Vec<String>) -> u64 {
        self.run_cli_input(op, args, None)
    }

    /// [`GitService::run_cli`] with `input` on stdin.
    pub fn run_cli_input(&mut self, op: GitOp, args: Vec<String>, input: Option<Vec<u8>>) -> u64 {
        let label = steps_label(std::slice::from_ref(&args));
        self.spawn_cli(op, vec![args], input, label)
    }

    /// Run several `git <args>` as one operation, in order, stopping at the
    /// first failure; the label joins them with " && ", the output too.
    pub fn run_cli_steps(&mut self, op: GitOp, steps: Vec<Vec<String>>) -> u64 {
        let label = steps_label(&steps);
        self.spawn_cli(op, steps, None, label)
    }

    /// [`GitService::run_cli_steps`] shown as `label` (status bar, Git console,
    /// Local History) instead of the commands it runs.
    pub fn run_cli_steps_as(&mut self, op: GitOp, steps: Vec<Vec<String>>, label: String) -> u64 {
        self.spawn_cli(op, steps, None, label)
    }

    fn spawn_cli(&mut self, op: GitOp, steps: Vec<Vec<String>>, mut input: Option<Vec<u8>>, label: String) -> u64 {
        let id = self.start(&label);
        let pre = self.before(&op, &label);
        let track = op.touches_worktree() && self.history.enabled();
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            if let Some(ack) = pre {
                let _ = tokio::task::spawn_blocking(move || ack.wait()).await;
            }
            let base = base_of(root.clone(), track).await;
            let mut ok = true;
            let mut output = String::new();
            for args in &steps {
                let (step_ok, text) = match cli::git_with_input(&root, args, input.take()).await {
                    Ok(o) => (o.ok, o.combined()),
                    Err(e) => (false, format!("{e:#}")),
                };
                if !output.is_empty() && !text.is_empty() {
                    output.push('\n');
                }
                output.push_str(&text);
                if !step_ok {
                    ok = false;
                    break;
                }
            }
            let renames = renames_after(root, base).await;
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op, label, ok, output, renames }));
        });
        id
    }

    /// Run `work` as the operation `op`: the Local History snapshot of an
    /// operation that rewrites the work tree first, then the renames it made,
    /// answered as a [`GitMsg::OpDone`].
    fn spawn_op(&mut self, op: GitOp, label: String, work: impl std::future::Future<Output = (bool, String)> + Send + 'static) -> u64 {
        let id = self.start(&label);
        let pre = self.before(&op, &label);
        let track = op.touches_worktree() && self.history.enabled();
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            if let Some(ack) = pre {
                let _ = tokio::task::spawn_blocking(move || ack.wait()).await;
            }
            let base = base_of(root.clone(), track).await;
            let (ok, output) = work.await;
            let renames = renames_after(root, base).await;
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op, label, ok, output, renames }));
        });
        id
    }

    /// `git stash pop|apply` of the stash with commit `oid` (see
    /// [`cli::stash_apply`]); answers as a [`GitOp::Stash`] result.
    pub fn stash_apply(&mut self, oid: String, pop: bool, label: String) -> u64 {
        let root = self.root.clone();
        let work = async move { cli::stash_apply(&root, &oid, pop).await.unwrap_or_else(|e| (false, format!("{e:#}"))) };
        self.spawn_op(GitOp::Stash, label, work)
    }

    /// Store `what` in the shelf `dir` as `name` and take it out of the work
    /// tree (see [`shelf::shelve`]); answers as a [`GitOp::Shelve`] result.
    pub fn shelve(&mut self, dir: PathBuf, name: String, what: shelf::ShelveWhat) -> u64 {
        let time =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        let label = format!("shelve '{name}'");
        let work = shelf::shelve(self.root.clone(), dir, name.clone(), what, time);
        self.spawn_op(GitOp::Shelve(name), label, work)
    }

    /// Apply a shelf entry (see [`shelf::unshelve`]); answers as a [`GitOp::Unshelve`] result.
    pub fn unshelve(&mut self, dir: PathBuf, id: u64, name: String, file: Option<String>) -> u64 {
        let label = format!("unshelve '{name}'");
        let work = shelf::unshelve(self.root.clone(), dir, id, name.clone(), file);
        self.spawn_op(GitOp::Unshelve(name), label, work)
    }

    /// Read the shelf `dir`, answered with [`GitMsg::Shelf`].
    pub fn load_shelf(&self, dir: PathBuf) {
        let (generation, tx) = (self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let (version, result) = shelf::list(&dir);
            let _ = tx.send(AppMsg::Git(GitMsg::Shelf { generation, version, result, done: None }));
        });
    }

    /// Delete a shelf entry, answered with the new list.
    pub fn delete_shelf_entry(&self, dir: PathBuf, id: u64, name: String) {
        let (generation, tx) = (self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let (version, result) = shelf::delete(&dir, id);
            let done = result.is_ok().then(|| format!("Deleted shelf entry '{name}'"));
            let _ = tx.send(AppMsg::Git(GitMsg::Shelf { generation, version, result, done }));
        });
    }

    /// Read the patch of a shelf entry, answered with [`GitMsg::ShelfPatch`].
    pub fn load_shelf_patch(&self, dir: PathBuf, id: u64, title: String) {
        let (generation, tx) = (self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let result = shelf::read_patch(&dir, id).map(|b| String::from_utf8_lossy(&b).into_owned());
            let _ = tx.send(AppMsg::Git(GitMsg::ShelfPatch { generation, id, title, result }));
        });
    }

    /// `git rebase -i` with jetl's todo (see [`rebase::prepare`]); answers as a
    /// [`GitOp::InteractiveRebase`] result.
    pub fn interactive_rebase(&mut self, plan: rebase::RebasePlan) -> u64 {
        let base = plan.base.as_deref().map(|b| b[..b.len().min(8)].to_string());
        let label = format!("git rebase -i {}", base.as_deref().unwrap_or("--root"));
        let op = GitOp::InteractiveRebase(base.unwrap_or_else(|| "root".into()));
        let id = self.start(&label);
        let pre = self.before(&op, &label);
        let track = op.touches_worktree() && self.history.enabled();
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            if let Some(ack) = pre {
                let _ = tokio::task::spawn_blocking(move || ack.wait()).await;
            }
            let base = base_of(root.clone(), track).await;
            let (ok, output) = match rebase::prepare(&root, &plan).await {
                Ok(args) => match cli::git(&root, &args).await {
                    Ok(o) => (o.ok, o.combined()),
                    Err(e) => (false, format!("{e:#}")),
                },
                Err(e) => (false, e),
            };
            let renames = renames_after(root, base).await;
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op, label, ok, output, renames }));
        });
        id
    }

    /// `git fetch --prune`, failing early when no remote is configured
    /// (plain `git fetch` silently succeeds then).
    pub fn fetch(&mut self) -> u64 {
        let label = "git fetch --prune".to_string();
        let id = self.start(&label);
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let res = async {
                let remotes = cli::git_ok(&root, &cli::args(&["remote"])).await?;
                if remotes.stdout_text().trim().is_empty() {
                    anyhow::bail!("No remote configured; nothing to fetch. Add one with `git remote add origin <url>`.");
                }
                let o = cli::git(&root, &cli::args(&["fetch", "--prune"])).await?;
                Ok((o.ok, o.combined()))
            }
            .await;
            let (ok, output) = res.unwrap_or_else(|e| (false, format!("{e:#}")));
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op: GitOp::Fetch, label, ok, output, renames: Vec::new() }));
        });
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::args;
    use crate::git::model::{ProgressAction, ResetMode};
    use crate::git::repo::tests::TempRepo;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Run git in `dir`, asserting success; returns trimmed stdout.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with one commit (its config is isolated by TempRepo::new).
    fn repo(name: &str) -> TempRepo {
        let r = TempRepo::new(name);
        r.write("a.txt", "a\n");
        git(&r.dir, &["add", "-A"]);
        git(&r.dir, &["commit", "-qm", "first"]);
        r
    }

    fn service(root: &Path, generation: u64) -> (GitService, UnboundedReceiver<AppMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (GitService::new(root.to_path_buf(), generation, tx), rx)
    }

    /// The next git message (10 s at most).
    async fn next_git(rx: &mut UnboundedReceiver<AppMsg>) -> GitMsg {
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await {
            Ok(Some(AppMsg::Git(m))) => m,
            Ok(Some(_)) => panic!("expected a git message"),
            Ok(None) => panic!("channel closed"),
            Err(_) => panic!("no git message within 10 s"),
        }
    }

    /// Removes its directory on drop.
    struct TmpDir(PathBuf);

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn op_results_keep_the_generation_they_started_in() {
        let r = repo("gen");
        let (mut svc, mut rx) = service(&r.dir, 1);
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let a = svc.run_repo(GitOp::Stage, "stage".into(), move |_| {
            wait.recv().unwrap();
            Ok("staged".into())
        });
        assert_eq!(svc.busy, vec![(a, "stage".to_string())]);
        assert!(svc.is_busy());
        svc.set_root(r.dir.clone(), 2);
        assert!(svc.busy.is_empty());
        go.send(()).unwrap();
        let msg = next_git(&mut rx).await;
        assert_eq!(msg.generation(), 1);
        match msg {
            GitMsg::OpDone { generation, id, op, label, ok, output, renames } => {
                assert_eq!((generation, id, op, label.as_str(), ok, output.as_str()), (1, a, GitOp::Stage, "stage", true, "staged"));
                assert!(renames.is_empty());
            }
            other => panic!("expected OpDone, got {other:?}"),
        }

        let b = svc.run_repo(GitOp::Unstage, "u".into(), |_| Ok(String::new()));
        assert_eq!(b, a + 1);
        match next_git(&mut rx).await {
            GitMsg::OpDone { generation, id, .. } => assert_eq!((generation, id), (2, b)),
            other => panic!("expected OpDone, got {other:?}"),
        }
        svc.finish(b);
        assert!(!svc.busy.iter().any(|(i, _)| *i == b));

        svc.load_branches();
        match next_git(&mut rx).await {
            GitMsg::Branches { generation, .. } => assert_eq!(generation, 2),
            other => panic!("expected Branches, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_root_lets_a_new_status_walk_start() {
        let r = repo("status");
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.refresh_status();
        svc.refresh_status();
        match next_git(&mut rx).await {
            GitMsg::Status { generation, seq, result } => {
                assert_eq!((generation, seq), (1, 1));
                assert!(result.is_ok());
            }
            other => panic!("expected Status, got {other:?}"),
        }
        assert_eq!(svc.status_requests(), 2);
        svc.status_done();
        match next_git(&mut rx).await {
            GitMsg::Status { generation, seq, .. } => assert_eq!((generation, seq), (1, 2)),
            other => panic!("expected Status, got {other:?}"),
        }
        // The walk of seq 2 finished: the next request runs at once.
        svc.status_done();
        svc.refresh_status();
        svc.set_root(r.dir.clone(), 2);
        svc.refresh_status();
        let mut got = Vec::new();
        for _ in 0..2 {
            match next_git(&mut rx).await {
                GitMsg::Status { generation, seq, .. } => got.push((generation, seq)),
                other => panic!("expected Status, got {other:?}"),
            }
        }
        got.sort();
        assert_eq!(got, vec![(1, 3), (2, 4)]);
    }

    #[tokio::test]
    async fn cli_steps_stop_at_the_first_failure() {
        let r = repo("steps");
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.run_cli_steps(GitOp::Tag("t".into()), vec![args(&["tag", "t1"]), args(&["tag", "--no-such-flag"]), args(&["tag", "t3"])]);
        match next_git(&mut rx).await {
            GitMsg::OpDone { ok, label, output, .. } => {
                assert!(!ok);
                assert_eq!(label, "git tag t1 && git tag --no-such-flag && git tag t3");
                assert!(output.contains("no-such-flag"), "{output}");
            }
            other => panic!("expected OpDone, got {other:?}"),
        }
        let tags = git(&r.dir, &["tag", "-l"]);
        assert!(tags.lines().any(|t| t == "t1"), "{tags}");
        assert!(!tags.lines().any(|t| t == "t3"), "{tags}");
    }

    #[tokio::test]
    async fn cli_input_goes_to_stdin() {
        let r = repo("stdin");
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.run_cli_input(GitOp::Stage, args(&["hash-object", "--stdin"]), Some(b"hello\n".to_vec()));
        match next_git(&mut rx).await {
            GitMsg::OpDone { ok, output, .. } => {
                assert!(ok, "{output}");
                assert_eq!(output, "ce013625030ba8dba906f756967f9e9ca394464a");
            }
            other => panic!("expected OpDone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repo_errors_carry_their_chain() {
        let r = repo("chain");
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.run_repo(GitOp::Commit { push: false, amend: false }, "commit".into(), |_| Err(anyhow::anyhow!("inner").context("outer")));
        match next_git(&mut rx).await {
            GitMsg::OpDone { ok, output, .. } => {
                assert!(!ok);
                assert_eq!(output, "outer: inner");
            }
            other => panic!("expected OpDone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_without_a_remote_fails_early() {
        let r = repo("fetch-none");
        let (mut svc, mut rx) = service(&r.dir, 1);
        let id = svc.fetch();
        assert_eq!(svc.busy, vec![(id, "git fetch --prune".to_string())]);
        match next_git(&mut rx).await {
            GitMsg::OpDone { op, label, ok, output, .. } => {
                assert_eq!(op, GitOp::Fetch);
                assert_eq!(label, "git fetch --prune");
                assert!(!ok);
                assert_eq!(output, "No remote configured; nothing to fetch. Add one with `git remote add origin <url>`.");
            }
            other => panic!("expected OpDone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_prunes_deleted_remote_branches() {
        let r = repo("fetch-prune");
        let remote = TmpDir(std::env::temp_dir().join(format!("jetl-test-git-fetch-remote-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&remote.0);
        std::fs::create_dir_all(&remote.0).unwrap();
        git(&remote.0, &["init", "-q", "--bare"]);
        crate::git::repo::tests::isolate(&remote.0);
        let url = remote.0.to_str().unwrap();
        git(&r.dir, &["remote", "add", "origin", url]);
        git(&r.dir, &["push", "-q", "origin", "HEAD:refs/heads/gone"]);
        git(&r.dir, &["fetch", "-q", "origin"]);
        let tracking = |dir: &Path| {
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", "-q", "refs/remotes/origin/gone"])
                .current_dir(dir)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(tracking(&r.dir));
        // Delete on the remote side only; a push --delete would drop the local ref too.
        git(&r.dir, &["--git-dir", url, "update-ref", "-d", "refs/heads/gone"]);

        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.fetch();
        match next_git(&mut rx).await {
            GitMsg::OpDone { ok, op, output, .. } => {
                assert!(ok, "{output}");
                assert_eq!(op, GitOp::Fetch);
            }
            other => panic!("expected OpDone, got {other:?}"),
        }
        assert!(!tracking(&r.dir));
    }

    #[test]
    fn touches_worktree_lists_the_ops_that_rewrite_files() {
        let reset = |mode| GitOp::Reset { sha: "x".into(), mode };
        let yes = [
            GitOp::Pull,
            GitOp::Switch("b".into()),
            GitOp::NewBranch("b".into()),
            GitOp::Discard,
            GitOp::Stash,
            GitOp::Shelve("n".into()),
            GitOp::Unshelve("n".into()),
            GitOp::Checkout("x".into()),
            GitOp::CherryPick("x".into()),
            GitOp::Revert("x".into()),
            reset(ResetMode::Hard),
            GitOp::Merge("b".into()),
            GitOp::Rebase("b".into()),
            GitOp::InteractiveRebase("x".into()),
            GitOp::Resolve("a".into()),
            GitOp::Progress { kind: InProgressKind::Merge, action: ProgressAction::Abort },
        ];
        for op in yes {
            assert!(op.touches_worktree(), "{op:?}");
        }
        let no = [
            GitOp::Stage,
            GitOp::Unstage,
            GitOp::Commit { push: false, amend: false },
            GitOp::Push,
            GitOp::PushUpstream("b".into()),
            GitOp::Fetch,
            reset(ResetMode::Soft),
            reset(ResetMode::Mixed),
            GitOp::RenameBranch("b".into()),
            GitOp::DeleteBranch { name: "b".into(), force: false },
            GitOp::Tag("t".into()),
            GitOp::DeleteTag("t".into()),
            GitOp::WorktreeAdd { path: PathBuf::from("w") },
            GitOp::WorktreeRemove { path: PathBuf::from("w"), force: false },
            GitOp::WorktreePrune,
        ];
        for op in no {
            assert!(!op.touches_worktree(), "{op:?}");
        }
    }

    #[test]
    fn requests_refused_while_an_operation_is_in_progress() {
        let commit = GitRequest::Commit { message: "m".into(), push: false, amend: false };
        let switching = [
            GitRequest::CheckoutCommit("x".into()),
            GitRequest::CherryPick("x".into()),
            GitRequest::Revert("x".into()),
            GitRequest::NewBranchAt { name: "n".into(), rev: "x".into() },
            GitRequest::NewBranch,
            GitRequest::Merge("b".into()),
            GitRequest::Rebase("b".into()),
            GitRequest::InteractiveRebase(rebase::RebasePlan { base: None, head: "h".into(), entries: Vec::new() }),
            GitRequest::LoadRebasePlan("x".into()),
            GitRequest::CheckoutTracking("o/b".into()),
            GitRequest::Shelve { name: "n".into(), what: shelf::ShelveWhat::Files(Vec::new()) },
            GitRequest::Unshelve { id: 1, name: "n".into(), file: None },
        ];
        let never = [
            GitRequest::Refresh,
            GitRequest::Stage(vec![]),
            GitRequest::Fetch,
            GitRequest::Progress(ProgressAction::Abort),
            GitRequest::LoadShelf,
            GitRequest::ShelfDelete { id: 1, name: "n".into() },
            GitRequest::ShelfPatch { id: 1, title: "t".into() },
        ];
        for kind in [
            InProgressKind::Merge,
            InProgressKind::Rebase,
            InProgressKind::CherryPick,
            InProgressKind::Revert,
            InProgressKind::Bisect,
            InProgressKind::Am,
        ] {
            assert_eq!(commit.refused_mid_operation(kind), kind != InProgressKind::Bisect, "commit during {kind:?}");
            for req in &switching {
                assert_eq!(req.refused_mid_operation(kind), kind.blocks_switching(), "{req:?} during {kind:?}");
            }
            for req in &never {
                assert!(!req.refused_mid_operation(kind), "{req:?} during {kind:?}");
            }
        }
    }

    // ---- shelf -----------------------------------------------------------

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    /// The next `OpDone` as `(ok, output)`.
    async fn op_result(rx: &mut UnboundedReceiver<AppMsg>) -> (bool, String) {
        match next_git(rx).await {
            GitMsg::OpDone { ok, output, .. } => (ok, output),
            other => panic!("expected OpDone, got {other:?}"),
        }
    }

    fn shelved(rel: &str, untracked: bool) -> shelf::ShelveFile {
        shelf::ShelveFile { rel: rel.into(), paths: vec![rel.into()], untracked }
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    /// `lines(n)` with line `at` (1-based) replaced.
    fn with_line(n: usize, at: usize, text: &str) -> String {
        (1..=n).map(|i| if i == at { format!("{text}\n") } else { format!("line {i}\n") }).collect()
    }

    async fn unshelve_entry(
        svc: &mut GitService,
        rx: &mut UnboundedReceiver<AppMsg>,
        dir: &Path,
        name: &str,
        file: Option<&str>,
    ) -> (bool, String) {
        let id = shelf::list(dir).1.unwrap().iter().find(|e| e.name == name).expect("entry").id;
        svc.unshelve(dir.to_path_buf(), id, name.into(), file.map(String::from));
        op_result(rx).await
    }

    #[tokio::test]
    async fn shelve_files_round_trip() {
        let r = repo("shelve-rt");
        r.write("b.txt", "b\n");
        git(&r.dir, &["add", "-A"]);
        git(&r.dir, &["commit", "-qm", "second"]);
        r.write("a.txt", "a staged\n");
        git(&r.dir, &["add", "a.txt"]);
        r.write("b.txt", "b unstaged\n");
        r.write("c.txt", "c new\n");
        let store = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-rt-{}", std::process::id())));
        let (mut svc, mut rx) = service(&r.dir, 1);
        let what = shelf::ShelveWhat::Files(vec![shelved("a.txt", false), shelved("b.txt", false), shelved("c.txt", true)]);
        svc.shelve(store.0.clone(), "wip".into(), what);
        assert_eq!(svc.busy[0].1, "shelve 'wip'");
        let (ok, out) = op_result(&mut rx).await;
        assert!(ok, "{out}");
        assert_eq!(out, "Shelved 'wip' (3 files)");
        assert_eq!(git(&r.dir, &["status", "--porcelain"]), "");
        assert_eq!(read(&r.dir, "a.txt"), "a\n");
        let list = shelf::list(&store.0).1.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].name.as_str(), list[0].files.clone()), ("wip", vec!["a.txt".to_string(), "b.txt".into(), "c.txt".into()]));

        let (ok, out) = unshelve_entry(&mut svc, &mut rx, &store.0, "wip", None).await;
        assert!(ok, "{out}");
        assert_eq!(out, "Unshelved 'wip'");
        assert_eq!(
            (read(&r.dir, "a.txt"), read(&r.dir, "b.txt"), read(&r.dir, "c.txt")),
            ("a staged\n".into(), "b unstaged\n".into(), "c new\n".into())
        );
        assert_eq!(shelf::list(&store.0).1.unwrap().len(), 1, "the entry is kept");
    }

    #[tokio::test]
    async fn unshelve_falls_back_to_3way() {
        let r = repo("shelve-3way");
        r.write("a.txt", &lines(6));
        git(&r.dir, &["commit", "-qam", "six"]);
        r.write("a.txt", &with_line(6, 2, "TWO"));
        let store = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-3way-{}", std::process::id())));
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.shelve(store.0.clone(), "n".into(), shelf::ShelveWhat::Files(vec![shelved("a.txt", false)]));
        assert!(op_result(&mut rx).await.0);
        // An unrelated change near enough to break the plain apply's context.
        r.write("a.txt", &with_line(6, 5, "FIVE"));
        git(&r.dir, &["commit", "-qam", "other"]);
        let (ok, out) = unshelve_entry(&mut svc, &mut rx, &store.0, "n", None).await;
        assert!(ok, "{out}");
        assert_eq!(out, "Unshelved 'n'; 3-way merged, the result is staged");
        let want = with_line(6, 2, "TWO").replace("line 5", "FIVE");
        assert_eq!(read(&r.dir, "a.txt"), want);
        assert_eq!(git(&r.dir, &["status", "--porcelain"]), "M  a.txt");
    }

    #[tokio::test]
    async fn unshelve_conflict_keeps_entry() {
        let r = repo("shelve-conflict");
        r.write("a.txt", &lines(6));
        git(&r.dir, &["commit", "-qam", "six"]);
        r.write("a.txt", &with_line(6, 2, "mine"));
        let store = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-conflict-{}", std::process::id())));
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.shelve(store.0.clone(), "n".into(), shelf::ShelveWhat::Files(vec![shelved("a.txt", false)]));
        assert!(op_result(&mut rx).await.0);
        r.write("a.txt", &with_line(6, 2, "theirs"));
        git(&r.dir, &["commit", "-qam", "other"]);
        let (ok, out) = unshelve_entry(&mut svc, &mut rx, &store.0, "n", None).await;
        assert!(!ok, "{out}");
        assert!(out.contains("with conflicts"), "{out}");
        assert!(read(&r.dir, "a.txt").contains("<<<<<<<"));
        assert!(git(&r.dir, &["status", "--porcelain"]).starts_with("UU a.txt"));
        assert_eq!(shelf::list(&store.0).1.unwrap().len(), 1);
    }

    /// The first changed block of `new` against `old` as a `Part` of `rel`.
    fn part(rel: &str, old: &str, new: &str, block: usize) -> shelf::ShelveWhat {
        let doc = diff::DiffDoc::build(old.as_bytes(), new.as_bytes(), 4);
        let p = doc.block_patch(block, rel, rel, 3).unwrap();
        shelf::ShelveWhat::Part { rel: rel.into(), stored: p.clone(), revert: p }
    }

    #[tokio::test]
    async fn shelve_part_keeps_the_other_hunk() {
        let r = repo("shelve-part");
        r.write("a.txt", &lines(30));
        git(&r.dir, &["commit", "-qam", "thirty"]);
        let changed = with_line(30, 3, "THREE").replace("line 25", "TWENTY-FIVE");
        r.write("a.txt", &changed);
        let store = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-part-{}", std::process::id())));
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.shelve(store.0.clone(), "first block".into(), part("a.txt", &lines(30), &changed, 0));
        let (ok, out) = op_result(&mut rx).await;
        assert!(ok, "{out}");
        assert_eq!(out, "Shelved 'first block' (1 file)");
        // Line 3 is back to HEAD; line 25 keeps its change.
        assert_eq!(read(&r.dir, "a.txt"), with_line(30, 25, "TWENTY-FIVE"));
        // Commit a change to a context line of the shelved block, with an unchanged line between
        // them: the plain apply no longer fits, a 3-way merge does.
        let now = read(&r.dir, "a.txt").replace("line 5\n", "FIVE\n");
        r.write("a.txt", &now);
        git(&r.dir, &["commit", "-qam", "next to it"]);
        let (ok, out) = unshelve_entry(&mut svc, &mut rx, &store.0, "first block", None).await;
        assert!(ok, "{out}");
        assert!(out.ends_with("; 3-way merged, the result is staged"), "{out}");
        let want = now.replace("line 3\n", "THREE\n");
        assert_eq!(read(&r.dir, "a.txt"), want);
    }

    #[tokio::test]
    async fn unshelve_one_file_with_include() {
        let r = repo("shelve-include");
        r.write("b.txt", "b\n");
        git(&r.dir, &["add", "-A"]);
        git(&r.dir, &["commit", "-qm", "second"]);
        r.write("a.txt", "a2\n");
        r.write("b.txt", "b2\n");
        let store = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-include-{}", std::process::id())));
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.shelve(store.0.clone(), "both".into(), shelf::ShelveWhat::Files(vec![shelved("a.txt", false), shelved("b.txt", false)]));
        assert!(op_result(&mut rx).await.0);
        let (ok, out) = unshelve_entry(&mut svc, &mut rx, &store.0, "both", Some("b.txt")).await;
        assert!(ok, "{out}");
        assert_eq!(out, "Unshelved b.txt from 'both'");
        assert_eq!((read(&r.dir, "a.txt"), read(&r.dir, "b.txt")), ("a\n".into(), "b2\n".into()));
    }

    #[tokio::test]
    async fn shelve_in_a_subdirectory_project() {
        let r = repo("shelve-subdir");
        r.write("sub/s.txt", "s\n");
        git(&r.dir, &["add", "-A"]);
        git(&r.dir, &["commit", "-qm", "sub"]);
        r.write("sub/s.txt", "s2\n");
        r.write("a.txt", "a2\n");
        let store = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-subdir-{}", std::process::id())));
        let (mut svc, mut rx) = service(&r.dir.join("sub"), 1);
        svc.shelve(store.0.clone(), "x".into(), shelf::ShelveWhat::Files(vec![shelved("sub/s.txt", false), shelved("a.txt", false)]));
        let (ok, out) = op_result(&mut rx).await;
        assert!(ok, "{out}");
        assert_eq!(git(&r.dir, &["status", "--porcelain"]), "");
        assert_eq!(shelf::list(&store.0).1.unwrap()[0].files, vec!["a.txt".to_string(), "sub/s.txt".into()]);
        let (ok, out) = unshelve_entry(&mut svc, &mut rx, &store.0, "x", None).await;
        assert!(ok, "{out}");
        assert_eq!((read(&r.dir, "sub/s.txt"), read(&r.dir, "a.txt")), ("s2\n".into(), "a2\n".into()));
    }

    #[tokio::test]
    async fn nothing_to_shelve() {
        let r = repo("shelve-nothing");
        let store = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-nothing-{}", std::process::id())));
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.shelve(store.0.clone(), "x".into(), shelf::ShelveWhat::Files(vec![shelved("a.txt", false)]));
        let (ok, out) = op_result(&mut rx).await;
        assert!(!ok);
        assert_eq!(out, "Nothing to shelve: no changes against HEAD in the selected files");
        assert!(!store.0.exists(), "nothing is stored");
    }

    #[tokio::test]
    async fn store_failure_reverts_nothing() {
        let r = repo("shelve-nostore");
        r.write("a.txt", "mine\n");
        // A file where the shelf directory would go: the store cannot be created.
        let blocker = TmpDir(std::env::temp_dir().join(format!("jetl-test-shelf-blocker-{}", std::process::id())));
        std::fs::write(&blocker.0, "not a directory").unwrap();
        let (mut svc, mut rx) = service(&r.dir, 1);
        svc.shelve(blocker.0.join("store"), "x".into(), shelf::ShelveWhat::Files(vec![shelved("a.txt", false)]));
        let (ok, out) = op_result(&mut rx).await;
        assert!(!ok);
        assert!(out.starts_with("Cannot shelve: "), "{out}");
        assert_eq!(read(&r.dir, "a.txt"), "mine\n", "the work tree is untouched");
        let _ = std::fs::remove_file(&blocker.0);
    }
}
