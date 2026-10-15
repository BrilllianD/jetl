//! Git operations in progress (Continue / Skip / Abort) and their dialogs.

use super::App;
use crate::actions::{Action, GitAction};
use crate::event::Level;
use crate::git::GitOp;
use crate::git::model::ProgressAction;
use crate::ui::dialog::{Dialog, DialogId, DialogResult};

impl App {
    /// Git > Continue / Skip / Abort Operation.
    pub(super) fn progress_request(&mut self, action: ProgressAction) {
        let Some(ip) = self.in_progress() else {
            return self.notify(Level::Info, "No merge, rebase, cherry-pick or revert in progress");
        };
        let kind = ip.kind;
        let Some(args) = kind.args(action) else {
            let verb = match action {
                ProgressAction::Continue => "continue",
                ProgressAction::Skip => "skip",
                ProgressAction::Abort => "abort",
            };
            return self.notify(Level::Info, format!("Git cannot {verb} a {}", kind.name()));
        };
        if action == ProgressAction::Continue {
            let rels = self.conflicted();
            if !rels.is_empty() {
                let mut list = rels.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
                if rels.len() > 3 {
                    list.push_str(", …");
                }
                let msg = format!(
                    "{} file(s) still have conflicts: {list}.\nStage and Continue stages them as they are, conflict markers included.",
                    rels.len()
                );
                let id = DialogId::ContinueWithConflicts { kind, rels };
                self.dialog = Some(Dialog::confirm(id, "Continue", &msg, &["Resolve…", "Stage and Continue", "Cancel"]));
                return;
            }
        }
        if action == ProgressAction::Abort {
            let msg = format!(
                "Abort the {}?\nThe branch goes back to where it was; Local History keeps a copy of the changed files.",
                kind.name()
            );
            self.dialog = Some(Dialog::confirm(DialogId::AbortOperation(kind), "Abort", &msg, &["Abort", "Cancel"]));
            return;
        }
        self.git.run_cli_steps(GitOp::Progress { kind, action }, vec![args]);
    }

    /// Whether `action` makes sense now: the operation actions need one in progress.
    pub(super) fn git_action_available(&self, action: &Action) -> bool {
        let wanted = match action {
            Action::Git(GitAction::ContinueOperation) => ProgressAction::Continue,
            Action::Git(GitAction::SkipOperation) => ProgressAction::Skip,
            Action::Git(GitAction::AbortOperation) => ProgressAction::Abort,
            _ => return true,
        };
        self.in_progress().is_some_and(|ip| ip.kind.args(wanted).is_some())
    }

    /// The answers of the Continue and Abort dialogs; false for other dialogs.
    pub(super) fn finish_git_ops_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        match (id, res) {
            (DialogId::ContinueWithConflicts { .. }, DialogResult::Button(0)) => self.resolve_conflicts(),
            (DialogId::ContinueWithConflicts { kind, rels }, DialogResult::Button(1)) => {
                if let Some(args) = kind.args(ProgressAction::Continue) {
                    let add = ["add".to_string(), "--".to_string()].into_iter().chain(rels.iter().cloned()).collect();
                    self.git.run_cli_steps(GitOp::Progress { kind: *kind, action: ProgressAction::Continue }, vec![add, args]);
                }
            }
            (DialogId::AbortOperation(kind), DialogResult::Button(0)) => {
                if let Some(args) = kind.args(ProgressAction::Abort) {
                    self.git.run_cli_steps(GitOp::Progress { kind: *kind, action: ProgressAction::Abort }, vec![args]);
                }
            }
            (DialogId::ContinueWithConflicts { .. } | DialogId::AbortOperation(_), _) => {}
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::actions::{Action, GitAction};
    use crate::app::App;
    use crate::app::tests::{feed_status, handle_next, init_repo, test_app, with_history};
    use crate::event::AppMsg;
    use crate::git::model::{InProgress, InProgressKind};
    use crate::git::{GitMsg, GitRequest};
    use crate::ui::dialog::{DialogId, DialogResult};

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn git_out(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn op_done(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Git(GitMsg::OpDone { .. }))
    }

    fn last_note(app: &App) -> String {
        app.notifications.last().unwrap().text.clone()
    }

    /// Two branches that change `a.txt` differently; leaves `main` checked
    /// out and returns its name.
    fn diverged(app: &App) -> String {
        init_repo(app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        git(&root, &["config", "commit.gpgsign", "false"]);
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&root, &["switch", "-qc", "dev"]);
        std::fs::write(root.join("a.txt"), "dev\n").unwrap();
        git(&root, &["commit", "-qam", "dev change"]);
        git(&root, &["switch", "-q", &main]);
        std::fs::write(root.join("a.txt"), "main\n").unwrap();
        git(&root, &["commit", "-qam", "main change"]);
        main
    }

    fn has(app: &App, id: &str) -> bool {
        app.palette_actions().iter().any(|e| e.id == id)
    }

    #[tokio::test]
    async fn rebase_conflict_stage_and_continue() {
        let (mut app, dir) = test_app("rebase-continue");
        let main = diverged(&app);
        let root = app.workspace.root.clone();
        git(&root, &["switch", "-q", "dev"]);
        feed_status(&mut app, 1);
        assert!(!has(&app, "git.continue_operation"));
        app.git_request(GitRequest::Rebase(main));
        handle_next(&mut app, op_done).await;
        let note = last_note(&app);
        assert!(note.starts_with("Rebase stopped on conflicts") && note.contains("Continue Operation"), "{note}");
        feed_status(&mut app, 2);
        let d = app.dialog.take().expect("resolve conflicts");
        assert!(matches!(d.id, DialogId::ResolveConflictPick(_)), "{:?}", d.id);
        assert_eq!(app.in_progress(), Some(InProgress { kind: InProgressKind::Rebase, step: Some((1, 1)) }));
        for id in ["git.continue_operation", "git.skip_operation", "git.abort_operation"] {
            assert!(has(&app, id), "{id}");
        }
        std::fs::write(root.join("a.txt"), "both\n").unwrap();
        app.perform(Action::Git(GitAction::ContinueOperation));
        let d = app.dialog.take().expect("continue dialog");
        assert_eq!(d.id, DialogId::ContinueWithConflicts { kind: InProgressKind::Rebase, rels: vec!["a.txt".into()] });
        app.finish_dialog(d.id, DialogResult::Button(1));
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), "Rebase finished");
        let st = crate::git::repo::status(&root).unwrap();
        assert_eq!(st.branch.as_deref(), Some("dev"));
        assert_eq!(st.in_progress, None);
        assert_eq!(git_out(&root, &["log", "-1", "--format=%s"]), "dev change");
        assert_eq!(git_out(&root, &["log", "-1", "--format=%s", "HEAD~1"]), "main change");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "both\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rebase_abort_restores_the_branch() {
        let (mut app, dir) = test_app("rebase-abort");
        with_history(&mut app);
        let main = diverged(&app);
        let root = app.workspace.root.clone();
        git(&root, &["switch", "-q", "dev"]);
        let before = git_out(&root, &["rev-parse", "dev"]);
        app.git_request(GitRequest::Rebase(main));
        handle_next(&mut app, op_done).await;
        feed_status(&mut app, 1);
        app.dialog.take();
        app.perform(Action::Git(GitAction::AbortOperation));
        let d = app.dialog.take().expect("abort dialog");
        assert_eq!(d.id, DialogId::AbortOperation(InProgressKind::Rebase));
        assert!(d.message.contains("Local History"), "{}", d.message);
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), "Rebase aborted");
        assert_eq!(git_out(&root, &["rev-parse", "HEAD"]), before);
        let st = crate::git::repo::status(&root).unwrap();
        assert_eq!(st.branch.as_deref(), Some("dev"));
        assert_eq!(st.in_progress, None);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "dev\n");
        let rows = crate::app::history::tests::snapshots(&app, &root.join("a.txt"));
        assert!(rows.iter().any(|(label, text)| label == "Before git rebase --abort" && text.contains("<<<<<<<")), "{rows:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cherry_pick_conflict_skip_and_commit_refused() {
        let (mut app, dir) = test_app("cherry-skip");
        diverged(&app);
        let root = app.workspace.root.clone();
        let dev = git_out(&root, &["rev-parse", "dev"]);
        app.git_request(GitRequest::CherryPick(dev));
        handle_next(&mut app, op_done).await;
        feed_status(&mut app, 1);
        app.dialog.take();
        assert_eq!(app.in_progress().map(|ip| ip.kind), Some(InProgressKind::CherryPick));
        app.git_request(GitRequest::Commit { message: "x".into(), push: false, amend: false });
        assert_eq!(last_note(&app), "A cherry-pick is in progress: Git > Continue Operation commits it");
        assert!(app.git.busy.is_empty());
        app.perform(Action::Git(GitAction::SkipOperation));
        assert!(app.dialog.is_none());
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), "Skipped the commit");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "main\n");
        assert_eq!(git_out(&root, &["log", "-1", "--format=%s"]), "main change");
        assert_eq!(crate::git::repo::status(&root).unwrap().in_progress, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn merge_has_no_skip() {
        let (mut app, dir) = test_app("merge-no-skip");
        diverged(&app);
        app.git_request(GitRequest::Merge("dev".into()));
        handle_next(&mut app, op_done).await;
        feed_status(&mut app, 1);
        app.dialog.take();
        app.perform(Action::Git(GitAction::SkipOperation));
        assert_eq!(last_note(&app), "Git cannot skip a merge");
        assert!(app.git.busy.is_empty());
        assert!(has(&app, "git.continue_operation") && has(&app, "git.abort_operation"));
        assert!(!has(&app, "git.skip_operation"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
