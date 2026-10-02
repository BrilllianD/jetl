//! The app side of the worktree dialogs: the create flow (kind, branch,
//! path, then "switch?"), removal with a force retry, and prune. Every git
//! call is a `GitService::run_cli` operation; results come back through
//! `App::git_op_done` -> [`App::worktree_op_done`].

use std::path::{Path, PathBuf};

use crate::app::App;
use crate::event::Level;
use crate::git::GitOp;
use crate::ui::dialog::{Dialog, DialogId, DialogResult};

use super::{default_path, expand_tilde, free_branches};

/// git's complaint when `worktree remove` needs `--force`.
pub(crate) const DIRTY_MARKER: &str = "contains modified or untracked files";

impl App {
    /// Answers of the `Worktree*` dialogs (called from `finish_dialog`).
    pub fn finish_worktree_dialog(&mut self, id: DialogId, res: DialogResult) {
        match (id, res) {
            (DialogId::WorktreeCreateKind, DialogResult::Choice(0)) => {
                // Fresh branches, so the "free" filter is current.
                self.worktrees.pending_existing = true;
                self.git.load_branches();
            }
            (DialogId::WorktreeCreateKind, DialogResult::Choice(_)) => {
                self.dialog = Some(Dialog::input(
                    DialogId::WorktreeNewBranchName,
                    "New Worktree",
                    "Name of the new branch (starts at HEAD):",
                    "",
                    "Next",
                ));
            }
            (DialogId::WorktreeExistingBranch(names), DialogResult::Choice(i)) => {
                if let Some(b) = names.get(i) {
                    self.ask_worktree_path(b.clone(), false);
                }
            }
            (DialogId::WorktreeNewBranchName, DialogResult::Text(t)) => {
                let name = t.trim().to_string();
                if name.is_empty() || name.starts_with('-') || name.contains(char::is_whitespace) {
                    self.notify(Level::Warn, format!("Invalid branch name {name:?}"));
                } else if self.git_panel.branches.iter().any(|b| b.name == name) {
                    self.notify(Level::Warn, format!("Branch {name} already exists; choose \"Existing branch\""));
                } else {
                    self.ask_worktree_path(name, true);
                }
            }
            (DialogId::WorktreePath { branch, new_branch }, DialogResult::Text(t)) => self.create_worktree(branch, new_branch, t.trim()),
            (DialogId::WorktreeSwitchAfterCreate(path), DialogResult::Button(0)) => self.switch_root(path),
            (DialogId::WorktreeRemove { path, force }, DialogResult::Button(0)) => self.remove_worktree(path, force),
            (id, res) => tracing::debug!("worktree dialog {id:?} -> {res:?} ignored"),
        }
    }

    /// The branch list arrived for the "Existing branch" step.
    pub fn open_existing_branch_dialog(&mut self) {
        let names: Vec<String> = self.git_panel.branches.iter().map(|b| b.name.clone()).collect();
        let free = free_branches(&names, &self.worktrees.list);
        if free.is_empty() {
            self.notify(Level::Info, "Every local branch is checked out in a worktree; create a new branch instead");
            return;
        }
        self.dialog = Some(Dialog::choice(
            DialogId::WorktreeExistingBranch(free.clone()),
            "New Worktree",
            "Branch to check out (not checked out elsewhere):",
            free,
        ));
    }

    fn main_root(&self) -> PathBuf {
        self.worktrees.main_root().map(Path::to_path_buf).unwrap_or_else(|| self.git.root.clone())
    }

    fn ask_worktree_path(&mut self, branch: String, new_branch: bool) {
        let default = default_path(&self.config.worktree.default_dir, &self.main_root(), &branch);
        let msg = format!("Directory for the worktree of {branch}:");
        let id = DialogId::WorktreePath { branch, new_branch };
        self.dialog = Some(Dialog::input(id, "New Worktree", &msg, &default.to_string_lossy(), "Create"));
    }

    /// `git worktree add [-b <new>] <path> [<branch>]`, run in the current
    /// root so a new branch starts at this worktree's HEAD.
    fn create_worktree(&mut self, branch: String, new_branch: bool, path: &str) {
        if path.is_empty() {
            return self.notify(Level::Warn, "New worktree: the path is empty");
        }
        // Relative paths resolve like git would, against the current root.
        let path = self.workspace.root.join(expand_tilde(path));
        let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
        if new_branch {
            args.extend(["-b".into(), branch.clone()]);
        }
        args.push(path.to_string_lossy().into_owned());
        if !new_branch {
            args.push(branch);
        }
        self.git.run_cli(GitOp::WorktreeAdd { path }, args);
    }

    /// `git -C <main> worktree remove [--force] <path>`. The main and the
    /// current worktree are refused.
    fn remove_worktree(&mut self, path: PathBuf, force: bool) {
        let main = self.main_root();
        if path == main {
            return self.notify(Level::Warn, "The main worktree cannot be removed");
        }
        if self.workspace.root.starts_with(&path) {
            return self.notify(Level::Warn, "Cannot remove the current worktree; switch to another one first");
        }
        let mut args: Vec<String> = vec!["-C".into(), main.to_string_lossy().into_owned(), "worktree".into(), "remove".into()];
        if force {
            args.push("--force".into());
        }
        args.push(path.to_string_lossy().into_owned());
        self.git.run_cli(GitOp::WorktreeRemove { path, force }, args);
    }

    pub fn prune_worktrees(&mut self) {
        self.git.run_cli(GitOp::WorktreePrune, vec!["worktree".into(), "prune".into(), "-v".into()]);
    }

    /// Follow-up of a finished worktree operation. Returns false when `op`
    /// is not one; the caller then reports it generically.
    pub fn worktree_op_done(&mut self, op: &GitOp, ok: bool, output: &str) -> bool {
        match op {
            GitOp::WorktreeAdd { path } if ok => {
                self.notify(Level::Info, format!("Created worktree {}", path.display()));
                let msg = format!("Switch to the new worktree?\n{}", path.display());
                self.dialog =
                    Some(Dialog::confirm(DialogId::WorktreeSwitchAfterCreate(path.clone()), "Worktree created", &msg, &["Switch", "Stay"]));
            }
            GitOp::WorktreeRemove { path, force: false } if !ok && output.contains(DIRTY_MARKER) => {
                let msg = format!("{} contains modified or untracked files.\nForce removal? Those changes will be lost.", path.display());
                self.dialog = Some(Dialog::confirm(
                    DialogId::WorktreeRemove { path: path.clone(), force: true },
                    "Remove Worktree",
                    &msg,
                    &["Force remove", "Cancel"],
                ));
                // The default button of a destructive retry is Cancel.
                if let Some(Dialog { kind: crate::ui::dialog::DialogKind::Confirm { selected, .. }, .. }) = &mut self.dialog {
                    *selected = 1;
                }
            }
            GitOp::WorktreeRemove { path, .. } if ok => self.notify(Level::Info, format!("Removed worktree {}", path.display())),
            GitOp::WorktreePrune if ok => {
                let text = if output.trim().is_empty() { "Nothing to prune" } else { "Pruned stale worktrees" };
                self.notify(Level::Info, text);
            }
            _ => return false,
        }
        true
    }
}
