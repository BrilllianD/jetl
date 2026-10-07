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
pub mod repo;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
pub use model::{Blame, BranchInfo, CommitDetails, DiffKey, LogFilter, LogRev, Section, StashEntry, StatusSnapshot};
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
    Merge(String),
    Rebase(String),
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
                | GitOp::Checkout(_)
                | GitOp::CherryPick(_)
                | GitOp::Revert(_)
                | GitOp::Merge(_)
                | GitOp::Rebase(_)
                | GitOp::Resolve(_)
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
    CommitDetails {
        generation: u64,
        sha: String,
        result: Result<CommitDetails>,
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
    },
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
            | GitMsg::PushRemote { generation, .. }
            | GitMsg::HeadMessage { generation, .. }
            | GitMsg::Stashes { generation, .. }
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
    LoadBlame(PathBuf),
    CheckoutCommit(String),
    CherryPick(String),
    /// Confirmed already (`DialogId::RevertCommit`).
    Revert(String),
    NewBranchAt {
        name: String,
        rev: String,
    },
    /// The branches popup's operations for this branch.
    BranchOps(BranchInfo),
    Merge(String),
    /// Confirmed already (`DialogId::RebaseOnto`).
    Rebase(String),
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

    pub fn load_blob(&self, path: PathBuf, which: BlobWhich) {
        let p = path.clone();
        let seq = self.blob_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.blocking(
            move |root| repo::blob_text(root, &p, which),
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

    pub fn load_blame(&self, path: PathBuf) {
        let p = path.clone();
        let seq = self.blame_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.blocking(move |root| repo::blame(root, &p), move |generation, result| GitMsg::Blame { generation, seq, path, result });
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
            // `App::git_request` loads these itself (`App::load_local_diff`).
            DiffKey::Local { .. } => tracing::debug!("local history diff reached the git service: {key:?}"),
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
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            if let Some(ack) = pre {
                ack.wait();
            }
            let (ok, output) = match work(&root) {
                Ok(s) => (true, s),
                Err(e) => (false, format!("{e:#}")),
            };
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op, label, ok, output }));
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
        let label = format!("git {}", args.join(" "));
        let id = self.start(&label);
        let pre = self.before(&op, &label);
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            if let Some(ack) = pre {
                let _ = tokio::task::spawn_blocking(move || ack.wait()).await;
            }
            let (ok, output) = match cli::git_with_input(&root, &args, input).await {
                Ok(o) => (o.ok, o.combined()),
                Err(e) => (false, format!("{e:#}")),
            };
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op, label, ok, output }));
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
            let _ = tx.send(AppMsg::Git(GitMsg::OpDone { generation, id, op: GitOp::Fetch, label, ok, output }));
        });
        id
    }
}
