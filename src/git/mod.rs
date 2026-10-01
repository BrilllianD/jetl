//! Git integration: a service that runs git2 work in `spawn_blocking` and
//! `git` CLI commands in tokio tasks, the Git tool window, and the diff code
//! shared with the editor gutter.
//!
//! Every result comes back as `AppMsg::Git` tagged with the service
//! generation; the app drops results whose generation is stale (the root
//! changed while they ran).

pub mod cli;
pub mod diff;
pub mod model;
pub mod panel;
pub mod repo;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
pub use model::{BranchInfo, CommitDetails, DiffKey, LogRow, Section, StatusSnapshot};
pub use panel::GitPanel;
pub use repo::BlobWhich;

/// Operations that change repository state. Their results arrive as
/// [`GitMsg::OpDone`] and trigger a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitOp {
    Stage,
    Unstage,
    Commit { push: bool },
    Push,
    /// `git push -u origin <branch>`.
    PushUpstream(String),
    Pull,
    Fetch,
    Switch(String),
    NewBranch(String),
    /// `git worktree add`; `path` is the new worktree.
    WorktreeAdd { path: PathBuf },
    /// `git -C <main> worktree remove [--force] <path>`.
    WorktreeRemove { path: PathBuf, force: bool },
    WorktreePrune,
}

impl GitOp {
    /// Whether the operation may rewrite files in the work tree.
    pub fn touches_worktree(&self) -> bool {
        matches!(self, GitOp::Pull | GitOp::Switch(_) | GitOp::NewBranch(_))
    }
}

#[derive(Debug)]
pub enum GitMsg {
    Status { generation: u64, seq: u64, result: Result<StatusSnapshot> },
    Blob { generation: u64, path: PathBuf, which: BlobWhich, text: Option<String> },
    Diff { generation: u64, key: DiffKey, result: Result<diff::DiffDoc> },
    Branches { generation: u64, result: Result<Vec<BranchInfo>> },
    Log { generation: u64, skip: usize, result: Result<Vec<LogRow>> },
    CommitDetails { generation: u64, sha: String, result: Result<CommitDetails> },
    /// A finished operation; `label` is what the console and spinner show.
    OpDone { generation: u64, id: u64, op: GitOp, label: String, ok: bool, output: String },
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
    Commit { message: String, push: bool },
    Push,
    Pull,
    Fetch,
    BranchesPopup,
    NewBranch,
    LoadDiff(DiffKey),
    LoadLog { skip: usize },
    LoadCommit(String),
    LoadWorktrees,
    WorktreePrune,
}

pub struct GitService {
    pub root: PathBuf,
    pub generation: u64,
    tx: UnboundedSender<AppMsg>,
    next_id: u64,
    status_seq: AtomicU64,
    /// Running operations (id, label), oldest first.
    pub busy: Vec<(u64, String)>,
    pub tab_width: usize,
}

impl GitService {
    pub fn new(root: PathBuf, tx: UnboundedSender<AppMsg>) -> Self {
        Self { root, generation: 1, tx, next_id: 1, status_seq: AtomicU64::new(0), busy: Vec::new(), tab_width: 4 }
    }

    /// Point the service at a new root. Results of work started for the old
    /// root are dropped by generation.
    pub fn set_root(&mut self, root: PathBuf) {
        self.root = root;
        self.generation += 1;
        self.busy.clear();
    }

    pub fn is_busy(&self) -> bool {
        !self.busy.is_empty()
    }

    /// Mark an operation finished (any generation).
    pub fn finish(&mut self, id: u64) {
        self.busy.retain(|(i, _)| *i != id);
    }

    fn blocking<T: Send + 'static>(&self, work: impl FnOnce(&Path) -> T + Send + 'static, wrap: impl FnOnce(u64, T) -> GitMsg + Send + 'static) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let out = work(&root);
            let _ = tx.send(AppMsg::Git(wrap(generation, out)));
        });
    }

    pub fn refresh_status(&self) {
        let seq = self.status_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.blocking(repo::status, move |generation, result| GitMsg::Status { generation, seq, result });
    }

    pub fn load_blob(&self, path: PathBuf, which: BlobWhich) {
        let p = path.clone();
        self.blocking(move |root| repo::blob_text(root, &p, which), move |generation, text| GitMsg::Blob { generation, path, which, text });
    }

    pub fn load_branches(&self) {
        self.blocking(repo::branches, |generation, result| GitMsg::Branches { generation, result });
    }

    pub fn load_diff(&self, key: DiffKey) {
        let tab_width = self.tab_width;
        match key.clone() {
            DiffKey::Change { section, rel } => {
                self.blocking(
                    move |root| repo::change_texts(root, section, &rel).map(|(old, new)| diff::DiffDoc::build(&old, &new, tab_width)),
                    move |generation, result| GitMsg::Diff { generation, key, result },
                );
            }
            DiffKey::Commit { sha, rel } => {
                let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
                tokio::spawn(async move {
                    let result = async {
                        let old = cli::show_file(&root, &format!("{sha}^"), &rel).await?;
                        let new = cli::show_file(&root, &sha, &rel).await?;
                        tokio::task::spawn_blocking(move || diff::DiffDoc::build(&old, &new, tab_width)).await.map_err(anyhow::Error::from)
                    }
                    .await;
                    let _ = tx.send(AppMsg::Git(GitMsg::Diff { generation, key, result }));
                });
            }
        }
    }

    pub fn load_log(&self, skip: usize, count: usize, all: bool) {
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let out = cli::git(&root, &cli::log_args(skip, count, all)).await;
            let result = out.map(|o| {
                // An unborn branch has no log; show it as empty.
                if o.ok { cli::parse_log(&o.stdout_text()) } else { Vec::new() }
            });
            let _ = tx.send(AppMsg::Git(GitMsg::Log { generation, skip, result }));
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

    /// Run a git2 operation in `spawn_blocking`. `work` returns a line for
    /// the console on success.
    pub fn run_repo(&mut self, op: GitOp, label: String, work: impl FnOnce(&Path) -> Result<String> + Send + 'static) -> u64 {
        let id = self.start(&label);
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
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
        let label = format!("git {}", args.join(" "));
        let id = self.start(&label);
        let (root, generation, tx) = (self.root.clone(), self.generation, self.tx.clone());
        tokio::spawn(async move {
            let (ok, output) = match cli::git(&root, &args).await {
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
