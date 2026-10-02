//! Effects panels push through `Ctx`, and git requests.

use super::App;
use crate::actions::PaletteKind;
use crate::event::{Effect, Level};
use crate::git::{GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId};

impl App {
    pub fn apply_effects(&mut self, effects: Vec<Effect>) {
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
                Effect::Dialog(d) => self.dialog = Some(d),
                Effect::FileSaved { path: p, quiet } => {
                    if !quiet {
                        self.notify(Level::Info, format!("Saved {}", p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
                    }
                    self.project.refresh();
                    self.git.refresh_status();
                }
                Effect::Clipboard(text) => self.copy_to_clipboard(&text),
                Effect::SwitchRoot(p) => self.switch_root(p),
                Effect::GitRefresh => {
                    self.project.refresh();
                    self.git.refresh_status();
                    self.git.load_branches();
                    self.git.load_worktrees();
                }
                Effect::Git(req) => self.git_request(req),
                Effect::Quit => self.request_quit(),
            }
        }
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
            GitRequest::Commit { message, push } => {
                let subject = message.lines().next().unwrap_or("").to_string();
                self.git.run_repo(GitOp::Commit { push }, format!("commit: {subject}"), move |root| repo::commit(root, &message));
            }
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
