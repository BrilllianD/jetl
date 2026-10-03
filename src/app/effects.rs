//! Effects panels push through `Ctx`, and git requests.

use super::App;
use crate::actions::PaletteKind;
use crate::event::{AppMsg, Effect, Level};
use crate::git::{GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId};

impl App {
    /// Apply effects in order. Refreshes are collected and run once after the
    /// batch, so saving N files re-reads the tree and git status once.
    pub fn apply_effects(&mut self, effects: Vec<Effect>) {
        let (mut tree, mut status, mut lists) = (false, false, false);
        for e in effects {
            match e {
                Effect::Notify(level, text) => self.notify(level, text),
                Effect::Focus(p) => {
                    if let Some(tw) = p.tool_window() {
                        self.layout.show(tw);
                    }
                    self.focus.set(p);
                }
                Effect::OpenFile(p) => self.open_file(p, true),
                Effect::OpenFileAt { path, line, col } => self.open_file_at(path, line, col),
                Effect::Dialog(d) => self.dialog = Some(d),
                Effect::ContextMenu(mut m) => {
                    // One shortcut per row (the shortest) keeps the menu narrow.
                    for item in &mut m.items {
                        item.shortcut =
                            self.keymap.bindings_for(&item.action, item.action.contexts()).into_iter().next().unwrap_or_default();
                    }
                    self.menu = Some(m);
                }
                Effect::FileSaved { path: p, quiet } => {
                    if !quiet {
                        self.notify(Level::Info, format!("Saved {}", p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
                    }
                    (tree, status) = (true, true);
                }
                Effect::Clipboard(text) => self.copy_to_clipboard(&text),
                Effect::SwitchRoot(p) => self.switch_root(p),
                Effect::RefreshTree => tree = true,
                Effect::GitRefresh => (tree, status, lists) = (true, true, true),
                Effect::Git(req) => self.git_request(req),
                Effect::Quit => self.request_quit(),
            }
        }
        if tree {
            self.refresh_tree();
        }
        if status {
            self.git.refresh_status();
        }
        if lists {
            self.git.load_branches();
            self.git.load_worktrees();
        }
    }

    /// Re-read the tree's expanded directories in `spawn_blocking`; the
    /// panel merges the result in `AppMsg::TreeLoaded`.
    pub fn refresh_tree(&mut self) {
        if let Some(req) = self.project.begin_refresh() {
            self.spawn_tree_refresh(req);
        }
    }

    pub(super) fn spawn_tree_refresh(&mut self, req: crate::files::tree::TreeRefresh) {
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let dirs = req.run();
            let _ = tx.send(AppMsg::TreeLoaded { generation, seq: req.seq, dirs });
        });
    }

    /// Run a request a panel made through `Effect::Git`.
    pub(super) fn git_request(&mut self, req: GitRequest) {
        use crate::git::repo;
        match req {
            GitRequest::Refresh => self.apply_effects(vec![Effect::GitRefresh]),
            GitRequest::Stage(rels) => {
                let label = format!("stage {}", rels.join(" "));
                self.git.run_repo(GitOp::Stage, label, move |root| repo::stage(root, &rels).map(|_| String::new()));
            }
            GitRequest::Unstage(rels) => {
                let label = format!("unstage {}", rels.join(" "));
                self.git.run_repo(GitOp::Unstage, label, move |root| repo::unstage(root, &rels).map(|_| String::new()));
            }
            GitRequest::StageAll => {
                self.git.run_repo(GitOp::Stage, "stage all".into(), |root| repo::stage_all(root).map(|_| String::new()));
            }
            GitRequest::UnstageAll => {
                self.git.run_repo(GitOp::Unstage, "unstage all".into(), |root| repo::unstage_all(root).map(|_| String::new()));
            }
            GitRequest::Discard(items) => {
                let label = format!("discard {}", items.iter().map(|(_, p)| p.as_str()).collect::<Vec<_>>().join(" "));
                self.git.run_repo(GitOp::Discard, label, move |root| repo::discard(root, &items));
            }
            GitRequest::Commit { message, push, amend } => {
                let subject = message.lines().next().unwrap_or("").to_string();
                let label = format!("{}: {subject}", if amend { "amend" } else { "commit" });
                let dir = self.git.messages_dir.clone();
                self.git.run_repo(GitOp::Commit { push, amend }, label, move |root| {
                    let sha = if amend { repo::amend(root, &message) } else { repo::commit(root, &message) }?;
                    if let Err(e) = crate::git::messages::record(&dir, root, &message) {
                        tracing::warn!("commit message history: {e:#}");
                    }
                    Ok(sha)
                });
            }
            GitRequest::LoadHeadMessage => self.git.load_head_message(),
            GitRequest::Push => {
                self.git.run_cli(GitOp::Push, vec!["push".into()]);
            }
            GitRequest::Pull => {
                let mode = match self.config.git.pull_mode.as_str() {
                    "rebase" => "--rebase",
                    "merge" => "--no-rebase",
                    _ => "--ff-only",
                };
                self.git.run_cli(GitOp::Pull, vec!["pull".into(), mode.into()]);
            }
            GitRequest::Fetch => {
                self.git.fetch();
            }
            GitRequest::BranchesPopup => self.open_palette(PaletteKind::Branches),
            GitRequest::NewBranch => {
                self.dialog = Some(Dialog::input(
                    DialogId::NewBranchName,
                    "New Branch",
                    "Create and switch to a new branch from HEAD:",
                    "",
                    "Create",
                ));
            }
            GitRequest::LoadDiff(key) => self.git.load_diff(key),
            GitRequest::LoadLog { skip } => self.git.load_log(skip, self.config.git.log_page, self.config.git.log_all),
            GitRequest::LoadCommit(sha) => self.git.load_commit(sha),
            GitRequest::LoadWorktrees => self.git.load_worktrees(),
            GitRequest::WorktreePrune => self.prune_worktrees(),
        }
    }

    pub(super) fn switch_branch(&mut self, name: String) {
        if self.branch_label().as_deref() == Some(name.as_str()) {
            self.notify(Level::Info, format!("Already on {name}"));
        } else {
            self.git.run_cli(GitOp::Switch(name.clone()), vec!["switch".into(), name]);
        }
    }
}
