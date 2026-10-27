//! Git operations in progress (Continue / Skip / Abort) and their dialogs.

use super::App;
use crate::actions::{Action, GitAction};
use crate::event::Level;
use crate::git::model::{ProgressAction, ResetMode};
use crate::git::{GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId, DialogKind, DialogResult};

/// What Stage and Continue shows: `git add <rels> && git <continue args>`,
/// each word shell-quoted, `--` only when a path starts with `-`.
fn stage_and_continue_label(rels: &[String], args: &[String]) -> String {
    let dashes = if rels.iter().any(|r| r.starts_with('-')) { " --" } else { "" };
    let word = |w: &String| crate::run::quote_word(w);
    format!(
        "git add{dashes} {} && git {}",
        rels.iter().map(word).collect::<Vec<_>>().join(" "),
        args.iter().map(word).collect::<Vec<_>>().join(" ")
    )
}

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

    /// Git > Push…: Push, the two toggles (each reopens the dialog with
    /// its flag flipped and `selected` kept), Cancel.
    pub(super) fn push_dialog(&mut self, tags: bool, force_lease: bool, selected: usize) {
        let mark = |on: bool| if on { "[x]" } else { "[ ]" };
        let branch = self.branch_label().unwrap_or_else(|| "HEAD".into());
        let runs =
            crate::git::cli::push_steps(force_lease, tags).iter().map(|s| format!("git {}", s.join(" "))).collect::<Vec<_>>().join(" && ");
        let msg = format!(
            "Push '{branch}' to its upstream.\nTags: also push every local tag.\nForce with lease: overwrite the remote branch only\nif it is still where it was last fetched.\nRuns: {runs}"
        );
        let tags_b = format!("Tags {}", mark(tags));
        let force_b = format!("Force with lease {}", mark(force_lease));
        let mut d = Dialog::confirm(DialogId::PushOptions { tags, force_lease }, "Push", &msg, &["Push", &tags_b, &force_b, "Cancel"]);
        if let DialogKind::Confirm { selected: s, .. } = &mut d.kind {
            *s = selected;
        }
        self.dialog = Some(d);
    }

    /// The answers of the Continue and Abort dialogs; false for other dialogs.
    pub(super) fn finish_git_ops_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        match (id, res) {
            (DialogId::ContinueWithConflicts { .. }, DialogResult::Button(0)) => self.resolve_conflicts(),
            (DialogId::ContinueWithConflicts { kind, rels }, DialogResult::Button(1)) => {
                if let Some(args) = kind.args(ProgressAction::Continue) {
                    // `rels` are relative to the work tree, the command runs in the project root.
                    // The label shows the plain paths, not the pathspec magic.
                    let label = stage_and_continue_label(rels, &args);
                    let add = ["add".to_string(), "--".to_string()]
                        .into_iter()
                        .chain(rels.iter().map(|r| format!(":(top,literal){r}")))
                        .collect();
                    self.git.run_cli_steps_as(GitOp::Progress { kind: *kind, action: ProgressAction::Continue }, vec![add, args], label);
                }
            }
            (DialogId::AbortOperation(kind), DialogResult::Button(0)) => {
                if let Some(args) = kind.args(ProgressAction::Abort) {
                    self.git.run_cli_steps(GitOp::Progress { kind: *kind, action: ProgressAction::Abort }, vec![args]);
                }
            }
            (DialogId::ResetMode { sha }, DialogResult::Choice(i)) => match ResetMode::ALL.get(*i) {
                Some(ResetMode::Hard) => {
                    let msg = format!(
                        "Reset hard to {}?\nUncommitted changes in tracked files are discarded; Local History keeps a copy of the changed files.",
                        &sha[..sha.len().min(8)]
                    );
                    self.dialog = Some(Dialog::confirm(DialogId::ResetHard { sha: sha.clone() }, "Reset Hard", &msg, &["Reset", "Cancel"]));
                }
                Some(&mode) => self.git_request(GitRequest::Reset { sha: sha.clone(), mode }),
                None => {}
            },
            (DialogId::ResetHard { sha }, DialogResult::Button(0)) => {
                self.git_request(GitRequest::Reset { sha: sha.clone(), mode: ResetMode::Hard });
            }
            (DialogId::PushOptions { tags, force_lease }, DialogResult::Button(0)) => {
                self.git_request(GitRequest::PushWith { tags: *tags, force_lease: *force_lease });
            }
            (DialogId::PushOptions { tags, force_lease }, DialogResult::Button(1)) => self.push_dialog(!*tags, *force_lease, 1),
            (DialogId::PushOptions { tags, force_lease }, DialogResult::Button(2)) => self.push_dialog(*tags, !*force_lease, 2),
            (
                DialogId::PushOptions { .. }
                | DialogId::ContinueWithConflicts { .. }
                | DialogId::AbortOperation(_)
                | DialogId::ResetMode { .. }
                | DialogId::ResetHard { .. },
                _,
            ) => {}
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
    use crate::event::{AppMsg, Level};
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
        with_history(&mut app);
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
        let rows = crate::app::history::tests::snapshots(&app, &root.join("a.txt"));
        assert!(rows.iter().any(|(label, text)| label == "Before git add a.txt && git rebase --continue" && text == "both\n"), "{rows:?}");
        assert!(rows.iter().all(|(label, _)| !label.contains(":(top")), "{rows:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_and_continue_label_is_the_plain_command() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cont = s(&["rebase", "--continue"]);
        assert_eq!(super::stage_and_continue_label(&s(&["a.txt"]), &cont), "git add a.txt && git rebase --continue");
        assert_eq!(super::stage_and_continue_label(&s(&["a.txt", "sub/c.txt"]), &cont), "git add a.txt sub/c.txt && git rebase --continue");
        assert_eq!(super::stage_and_continue_label(&s(&["a b.txt", "-x"]), &cont), "git add -- 'a b.txt' -x && git rebase --continue");
    }

    #[tokio::test]
    async fn rebase_stage_and_continue_from_a_subdirectory_project() {
        let (mut app, dir) = test_app("rebase-subdir");
        init_repo(&app, &[("top.txt", "t\n")]);
        let top = app.workspace.root.clone();
        let main = crate::git::repo::status(&top).unwrap().branch.unwrap();
        let sub = top.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "a\n").unwrap();
        git(&top, &["add", "."]);
        git(&top, &["commit", "-qm", "sub"]);
        git(&top, &["switch", "-qc", "dev"]);
        std::fs::write(sub.join("a.txt"), "dev\n").unwrap();
        git(&top, &["commit", "-qam", "dev change"]);
        git(&top, &["switch", "-q", &main]);
        std::fs::write(sub.join("a.txt"), "main\n").unwrap();
        git(&top, &["commit", "-qam", "main change"]);
        git(&top, &["switch", "-q", "dev"]);
        app.workspace.root = sub.clone();
        app.git.root = sub.clone();
        app.git_request(GitRequest::Rebase(main));
        handle_next(&mut app, op_done).await;
        feed_status(&mut app, 1);
        app.dialog.take();
        std::fs::write(sub.join("a.txt"), "both\n").unwrap();
        app.perform(Action::Git(GitAction::ContinueOperation));
        let d = app.dialog.take().expect("continue dialog");
        assert_eq!(d.id, DialogId::ContinueWithConflicts { kind: InProgressKind::Rebase, rels: vec!["sub/a.txt".into()] });
        app.finish_dialog(d.id, DialogResult::Button(1));
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), "Rebase finished");
        assert_eq!(crate::git::repo::status(&top).unwrap().in_progress, None);
        assert_eq!(git_out(&top, &["log", "-1", "--format=%s"]), "dev change");
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

    /// `a.txt` is `b\n` after a second commit; returns the first commit.
    fn two_commits(app: &App) -> String {
        init_repo(app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        git(&root, &["commit", "-qam", "second"]);
        git_out(&root, &["rev-parse", "HEAD~1"])
    }

    #[tokio::test]
    async fn reset_soft_from_the_log_menu_keeps_changes_staged() {
        let (mut app, dir) = test_app("reset-soft");
        let first = two_commits(&app);
        let root = app.workspace.root.clone();
        feed_status(&mut app, 1);
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        app.perform(Action::Git(GitAction::Down));
        assert_eq!(app.git_panel.log_cursor(), 1);
        app.perform(Action::Git(GitAction::LogReset));
        let d = app.dialog.take().expect("reset dialog");
        assert!(matches!(&d.id, DialogId::ResetMode { sha } if *sha == first), "{:?}", d.id);
        assert!(d.message.contains("init") && d.message.contains("reflog"), "{}", d.message);
        app.finish_dialog(d.id, DialogResult::Choice(0));
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), format!("Reset to {} (soft)", &first[..8]));
        assert_eq!(git_out(&root, &["rev-parse", "HEAD"]), first);
        assert_eq!(git_out(&root, &["diff", "--cached", "--name-only"]), "a.txt");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "b\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reset_mixed_keeps_changes_unstaged() {
        let (mut app, dir) = test_app("reset-mixed");
        let first = two_commits(&app);
        let root = app.workspace.root.clone();
        app.finish_dialog(DialogId::ResetMode { sha: first.clone() }, DialogResult::Choice(1));
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), format!("Reset to {} (mixed)", &first[..8]));
        assert_eq!(git_out(&root, &["rev-parse", "HEAD"]), first);
        assert_eq!(git_out(&root, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(git_out(&root, &["diff", "--name-only"]), "a.txt");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "b\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reset_hard_asks_and_snapshots_first() {
        use crate::ui::dialog::DialogKind;
        let (mut app, dir) = test_app("reset-hard");
        with_history(&mut app);
        let first = two_commits(&app);
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "dirty\n").unwrap();
        app.finish_dialog(DialogId::ResetMode { sha: first.clone() }, DialogResult::Choice(2));
        let d = app.dialog.take().expect("hard confirm");
        assert_eq!(d.id, DialogId::ResetHard { sha: first.clone() });
        assert!(matches!(&d.kind, DialogKind::Confirm { buttons, .. } if buttons == &["Reset", "Cancel"]), "{:?}", d.kind);
        assert!(d.message.contains("Local History"), "{}", d.message);
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, op_done).await;
        assert_eq!(git_out(&root, &["rev-parse", "HEAD"]), first);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
        let rows = crate::app::history::tests::snapshots(&app, &root.join("a.txt"));
        assert!(rows.contains(&(format!("Before git reset --hard {first}"), "dirty\n".to_string())), "{rows:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn push_tags_and_force_with_lease_against_a_bare_remote() {
        let (mut app, dir) = test_app("push-options");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        let remote = dir.join("remote.git");
        git(&dir, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        crate::git::repo::tests::isolate(&remote);
        git(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&root, &["push", "-q", "-u", "origin", &main]);
        git(&root, &["tag", "v1"]);
        let head_ref = format!("refs/heads/{main}");

        app.git_request(GitRequest::PushWith { tags: true, force_lease: false });
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), "Pushed");
        assert!(git_out(&root, &["ls-remote", "--tags", "origin"]).contains("refs/tags/v1"));

        git(&root, &["commit", "-q", "--amend", "-m", "rewritten"]);
        let before = git_out(&root, &["ls-remote", "origin", &head_ref]);
        app.git_request(GitRequest::PushWith { tags: false, force_lease: false });
        handle_next(&mut app, op_done).await;
        let n = app.notifications.last().unwrap();
        assert_eq!(n.level, Level::Error);
        assert!(n.text.contains("failed to push"), "{}", n.text);
        assert_eq!(git_out(&root, &["ls-remote", "origin", &head_ref]), before);

        app.git_request(GitRequest::PushWith { tags: false, force_lease: true });
        handle_next(&mut app, op_done).await;
        assert_eq!(last_note(&app), "Pushed");
        let head = git_out(&root, &["rev-parse", "HEAD"]);
        assert_eq!(git_out(&root, &["ls-remote", "origin", &head_ref]).split_whitespace().next(), Some(head.as_str()));

        let other = dir.join("other");
        git(&dir, &["clone", "-q", "-b", &main, remote.to_str().unwrap(), other.to_str().unwrap()]);
        crate::git::repo::tests::isolate(&other);
        std::fs::write(other.join("b.txt"), "b\n").unwrap();
        git(&other, &["add", "b.txt"]);
        git(&other, &["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "other"]);
        git(&other, &["push", "-q"]);
        let theirs = git_out(&other, &["rev-parse", "HEAD"]);
        git(&root, &["commit", "-q", "--amend", "-m", "again"]);
        app.git_request(GitRequest::PushWith { tags: false, force_lease: true });
        handle_next(&mut app, op_done).await;
        assert_eq!(app.notifications.last().unwrap().level, Level::Error);
        assert_eq!(git_out(&root, &["ls-remote", "origin", &head_ref]).split_whitespace().next(), Some(theirs.as_str()));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn stash_include_untracked_round_trip() {
        let (mut app, dir) = test_app("stash-untracked");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        feed_status(&mut app, 1);
        app.perform(Action::Git(GitAction::StashIncludeUntracked));
        app.perform(Action::Git(GitAction::StashPush));
        let d = app.dialog.take().unwrap();
        assert_eq!(d.id, DialogId::StashMessage { untracked: true });
        assert!(d.message.contains("untracked files included"), "{}", d.message);
        app.finish_dialog(d.id, DialogResult::Text("wip".into()));
        handle_next(&mut app, op_done).await;
        assert!(!root.join("new.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
        assert!(git_out(&root, &["stash", "list"]).contains("wip"));
        assert_eq!(git_out(&root, &["show", "--name-only", "--format=", "stash@{0}^3"]), "new.txt");

        let oid = git_out(&root, &["rev-parse", "stash@{0}"]);
        app.git_request(GitRequest::Stash(crate::git::repo::StashOp::Pop { oid }));
        handle_next(&mut app, op_done).await;
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "changed\n");

        app.perform(Action::Git(GitAction::StashIncludeUntracked));
        app.perform(Action::Git(GitAction::StashPush));
        assert_eq!(app.dialog.take().unwrap().id, DialogId::StashMessage { untracked: false });
        std::fs::remove_dir_all(dir).ok();
    }

    /// `init` (base.txt) then `one`..`four`, each adding `<n>.txt`; shas oldest first.
    fn four_commits(app: &App) -> Vec<String> {
        init_repo(app, &[("base.txt", "b\n")]);
        let root = app.workspace.root.clone();
        let mut shas = vec![git_out(&root, &["rev-parse", "HEAD"])];
        for n in ["one", "two", "three", "four"] {
            std::fs::write(root.join(format!("{n}.txt")), format!("body {n}\n")).unwrap();
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", n, "-m", &format!("body {n}")]);
            shas.push(git_out(&root, &["rev-parse", "HEAD"]));
        }
        shas
    }

    /// Run an interactive rebase from `shas[from]` after `edit` changed the plan;
    /// returns the one-line-per-field log, oldest at the bottom.
    async fn rebase_with(name: &str, from: usize, edit: impl FnOnce(&mut crate::git::rebase::RebasePlan)) -> String {
        use crate::git::rebase::load_plan;
        let (mut app, dir) = test_app(name);
        let shas = four_commits(&app);
        let root = app.workspace.root.clone();
        let mut plan = load_plan(&root, &shas[from]).await.unwrap();
        edit(&mut plan);
        app.git_request(GitRequest::InteractiveRebase(plan));
        handle_next(&mut app, op_done).await;
        assert_eq!(app.notifications.last().unwrap().level, Level::Info);
        assert_eq!(last_note(&app), "Interactive rebase done");
        assert_eq!(crate::git::repo::status(&root).unwrap().in_progress, None);
        let log = git_out(&root, &["log", "--format=%s%n%b"]);
        let _ = std::fs::remove_dir_all(&dir);
        log
    }

    #[tokio::test]
    async fn interactive_rebase_reorders() {
        let log = rebase_with("irb-reorder", 1, |p| p.entries.swap(0, 1)).await;
        assert_eq!(log, "four\nbody four\n\nthree\nbody three\n\none\nbody one\n\ntwo\nbody two\n\ninit");
    }

    #[tokio::test]
    async fn interactive_rebase_squashes_two() {
        use crate::git::rebase::TodoAction;
        let log = rebase_with("irb-squash", 1, |p| p.entries[1].action = TodoAction::Squash).await;
        assert_eq!(log, "four\nbody four\n\nthree\nbody three\n\none\nbody one\n\ntwo\n\nbody two\n\ninit");
    }

    #[tokio::test]
    async fn interactive_rebase_fixes_up() {
        use crate::git::rebase::TodoAction;
        let log = rebase_with("irb-fixup", 1, |p| p.entries[1].action = TodoAction::Fixup).await;
        assert_eq!(log, "four\nbody four\n\nthree\nbody three\n\none\nbody one\n\ninit");
    }

    #[tokio::test]
    async fn interactive_rebase_rewords_with_a_multiline_message() {
        use crate::git::rebase::TodoAction;
        let log = rebase_with("irb-reword", 1, |p| {
            p.entries[1].action = TodoAction::Reword;
            p.entries[1].new_message = Some("Two \"quoted\" it's\n\nsecond line\nthird line\n".into());
        })
        .await;
        assert_eq!(log, "four\nbody four\n\nthree\nbody three\n\nTwo \"quoted\" it's\nsecond line\nthird line\n\none\nbody one\n\ninit");
    }

    #[tokio::test]
    async fn interactive_rebase_drops_one() {
        use crate::git::rebase::TodoAction;
        let log = rebase_with("irb-drop", 1, |p| p.entries[2].action = TodoAction::Drop).await;
        assert_eq!(log, "four\nbody four\n\ntwo\nbody two\n\none\nbody one\n\ninit");
    }

    #[tokio::test]
    async fn interactive_rebase_from_the_root_commit() {
        use crate::git::rebase::TodoAction;
        let log = rebase_with("irb-root", 0, |p| {
            assert_eq!(p.base, None);
            p.entries[0].action = TodoAction::Reword;
            p.entries[0].new_message = Some("start".into());
            p.entries[4].action = TodoAction::Drop;
        })
        .await;
        assert_eq!(log, "three\nbody three\n\ntwo\nbody two\n\none\nbody one\n\nstart");
    }

    #[tokio::test]
    async fn interactive_rebase_conflict_takes_the_rebase_path() {
        let (mut app, dir) = test_app("irb-conflict");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        let mut shas = Vec::new();
        for (n, text) in [("one", "1\n"), ("two", "2\n")] {
            std::fs::write(root.join("a.txt"), text).unwrap();
            git(&root, &["commit", "-qam", n]);
            shas.push(git_out(&root, &["rev-parse", "HEAD"]));
        }
        let mut plan = crate::git::rebase::load_plan(&root, &shas[0]).await.unwrap();
        plan.entries.reverse();
        app.git_request(GitRequest::InteractiveRebase(plan));
        handle_next(&mut app, op_done).await;
        assert!(last_note(&app).starts_with("Rebase stopped on conflicts"), "{}", last_note(&app));
        assert!(app.pending_conflicts);
        feed_status(&mut app, 1);
        assert_eq!(app.in_progress().map(|i| i.kind), Some(InProgressKind::Rebase));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn interactive_rebase_refuses_when_head_moved() {
        let (mut app, dir) = test_app("irb-moved");
        let shas = four_commits(&app);
        let root = app.workspace.root.clone();
        let plan = crate::git::rebase::load_plan(&root, &shas[1]).await.unwrap();
        std::fs::write(root.join("five.txt"), "5\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "five"]);
        let five = git_out(&root, &["rev-parse", "HEAD"]);
        app.git_request(GitRequest::InteractiveRebase(plan));
        handle_next(&mut app, op_done).await;
        let note = app.notifications.last().unwrap();
        assert_eq!(note.level, Level::Error);
        assert!(note.text.ends_with("HEAD moved since the rebase was planned; start it again"), "{}", note.text);
        assert_eq!(git_out(&root, &["rev-parse", "HEAD"]), five);
        assert!(!root.join(".git/rebase-merge").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn interactive_rebase_refused_mid_operation() {
        let (mut app, dir) = test_app("irb-mid-op");
        let shas = four_commits(&app);
        let root = app.workspace.root.clone();
        let plan = crate::git::rebase::load_plan(&root, &shas[1]).await.unwrap();
        std::fs::write(root.join(".git/MERGE_HEAD"), format!("{}\n", shas[4])).unwrap();
        feed_status(&mut app, 1);
        app.git_request(GitRequest::InteractiveRebase(plan));
        assert_eq!(app.notifications.last().unwrap().level, Level::Warn);
        assert!(app.git.busy.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
