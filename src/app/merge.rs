//! Conflict resolution in the app: Git > Resolve Conflicts (pick a
//! conflicted file, then merge it in the three-pane resolver or take one
//! side whole), the resolver opening when the stages arrive, and the
//! dialog after an operation stopped on conflicts, and the offer of the
//! conflicted files left after each resolve.

use std::path::PathBuf;

use super::{App, Ctx};
use crate::editor::merge::BUSY;
use crate::event::Level;
use crate::git::merge::{ConflictSides, MergeInput};
use crate::git::model::FileStatus;
use crate::git::repo::ConflictSide;
use crate::git::{GitRequest, Section};
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::focus::PanelId;

impl App {
    /// The conflicted paths of the last status (relative to the work tree).
    pub(super) fn conflicted(&self) -> Vec<String> {
        let Some(s) = self.git_panel.snapshot() else { return Vec::new() };
        s.entries.iter().filter(|e| e.section == Section::Unstaged && e.status == FileStatus::Conflicted).map(|e| e.rel.clone()).collect()
    }

    /// Whether `path` is conflicted in the last status.
    pub(super) fn is_conflicted(&self, path: &std::path::Path) -> bool {
        self.git_panel.snapshot().is_some_and(|s| s.entries.iter().any(|e| e.status == FileStatus::Conflicted && e.abs == path))
    }

    /// Open `path`, or the conflict resolver when the last status lists it
    /// as conflicted (and the resolver is not open already): the project
    /// tree's and the Commit tab's open.
    pub(super) fn open_or_resolve(&mut self, path: PathBuf) {
        if self.is_conflicted(&path) && !self.editor.merge_active() {
            self.git_request(GitRequest::LoadConflict(path));
        } else {
            self.open_file(path, true);
        }
    }

    /// Git > Resolve Conflicts: a choice of the conflicted files.
    pub(super) fn resolve_conflicts(&mut self) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, BUSY);
        }
        let rels = self.conflicted();
        if rels.is_empty() {
            return self.notify(Level::Info, "No conflicted files");
        }
        self.dialog = Some(Dialog::choice(DialogId::ResolveConflictPick(rels.clone()), "Resolve Conflicts", "Conflicted files:", rels));
    }

    /// What to do with conflicted `rel`: merge it (when `merge`), or take
    /// yours or theirs.
    fn resolve_conflict_dialog(&mut self, abs: PathBuf, rel: String, merge: bool) {
        let msg = format!("{rel} has conflicts.");
        let buttons: &[&str] =
            if merge { &["Merge…", "Accept Yours", "Accept Theirs", "Cancel"] } else { &["Accept Yours", "Accept Theirs", "Cancel"] };
        self.dialog = Some(Dialog::confirm(DialogId::ResolveConflict { abs, rel, merge }, "Resolve Conflicts", &msg, buttons));
    }

    /// The answers of the resolve and resolver dialogs; false for other
    /// dialogs.
    pub(super) fn finish_merge_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        match (id, res) {
            (DialogId::ResolveConflictPick(rels), DialogResult::Choice(i)) => {
                let (Some(rel), Some(wd)) = (rels.get(*i), self.git_panel.snapshot().map(|s| s.workdir.clone())) else { return true };
                self.resolve_conflict_dialog(wd.join(rel), rel.clone(), true);
            }
            (DialogId::ResolveConflict { abs, rel, merge }, DialogResult::Button(b)) => {
                let b = if *merge { *b } else { b + 1 };
                match b {
                    0 if self.editor.merge_active() => self.notify(Level::Info, BUSY),
                    0 => self.git_request(GitRequest::LoadConflict(abs.clone())),
                    1 | 2 => {
                        let side = if b == 1 { ConflictSide::Ours } else { ConflictSide::Theirs };
                        self.git_request(GitRequest::ResolveSide { rel: rel.clone(), side });
                    }
                    _ => {}
                }
            }
            (DialogId::MergeApply, DialogResult::Button(0)) => self.with_editor(|ed, cx| ed.merge_apply(true, cx)),
            (DialogId::MergeCancel, DialogResult::Button(0)) => self.with_editor(|ed, cx| ed.merge_cancel(true, cx)),
            (DialogId::MergeQuit, DialogResult::Button(0)) => {
                self.editor.merge_close();
                self.request_quit();
            }
            (DialogId::ResolveConflictPick(_) | DialogId::ResolveConflict { .. } | DialogId::MergeApply, _)
            | (DialogId::MergeCancel | DialogId::MergeQuit, _) => {}
            _ => return false,
        }
        true
    }

    /// Run `f` on the editor with a `Ctx`, then its effects.
    fn with_editor(&mut self, f: impl FnOnce(&mut crate::editor::Editor, &mut Ctx)) {
        let mut cx = ctx!(self);
        f(&mut self.editor, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// A conflicted file's stages arrived: open the resolver on them, or
    /// offer yours and theirs whole when no line merge is possible.
    pub(super) fn conflict_loaded(&mut self, path: PathBuf, result: anyhow::Result<ConflictSides>) {
        let sides = match result {
            Ok(s) => s,
            Err(e) => return self.notify(Level::Error, format!("Resolve conflicts: {e:#}")),
        };
        if self.editor.merge_active() {
            return self.notify(Level::Info, BUSY);
        }
        match MergeInput::classify(&sides) {
            Ok(MergeInput::Text(texts)) => {
                let work = String::from_utf8_lossy(&sides.work).into_owned();
                let labels = [sides.ours_label, sides.theirs_label];
                self.editor.open_merge(path, sides.rel, texts, labels, work, &self.theme);
                self.hover = None;
                self.completion = None;
                self.focus.set(PanelId::Editor);
            }
            Ok(MergeInput::SidesOnly(reason)) => {
                self.notify(Level::Warn, format!("{reason}: take yours or theirs whole"));
                self.resolve_conflict_dialog(path, sides.rel, false);
            }
            Err(e) => self.notify(Level::Error, format!("Resolve conflicts: {}: {e:#}", sides.rel)),
        }
    }

    /// After an operation stopped on conflicts, the first status that
    /// lists conflicted files opens Resolve Conflicts (unless a dialog or
    /// the resolver is up).
    pub(super) fn offer_conflicts(&mut self) {
        if !self.pending_conflicts || self.dialog.is_some() || self.editor.merge_active() {
            return;
        }
        if !self.conflicted().is_empty() {
            self.pending_conflicts = false;
            self.resolve_conflicts();
        }
    }

    /// After a conflicted file was resolved, the first status that no
    /// longer lists it offers the conflicted files left (Resolve Conflicts
    /// again), or says that none are; a status from before the resolve
    /// waits, and so does a failed one (the files left are unknown). Nothing is offered over a dialog or the resolver.
    pub(super) fn offer_next_conflict(&mut self) {
        let Some(rel) = &self.resolved_conflict else { return };
        if self.git_panel.snapshot().is_none() {
            return;
        }
        let left = self.conflicted();
        if left.contains(rel) {
            return;
        }
        self.resolved_conflict = None;
        if left.is_empty() {
            self.notify(Level::Info, "All conflicts resolved");
        } else if self.dialog.is_none() && !self.editor.merge_active() {
            self.resolve_conflicts();
        }
    }

    /// Ask before dropping the resolver's work when quitting; true when
    /// the quit may go on.
    pub(super) fn quit_merge(&mut self) -> bool {
        if !self.editor.merge_active() {
            return true;
        }
        if self.editor.merge_edited() {
            self.dialog = Some(Dialog::confirm(
                DialogId::MergeQuit,
                "Merge in progress",
                "The merge result is not applied. Discard it and exit?",
                &["Discard and exit", "Cancel"],
            ));
            return false;
        }
        self.editor.merge_close();
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::actions::{Action, EditorAction, GitAction, MergeAction};
    use crate::app::tests::{feed_status, handle_next, init_repo, test_app};
    use crate::event::AppMsg;
    use crate::git::GitMsg;
    use crate::ui::dialog::{DialogId, DialogResult};
    use crate::ui::focus::PanelId;

    fn git(root: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn merge(a: MergeAction) -> Action {
        Action::Editor(EditorAction::Merge(a))
    }

    fn conflict(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Git(GitMsg::Conflict { .. }))
    }

    fn op_done(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Git(GitMsg::OpDone { .. }))
    }

    #[tokio::test]
    async fn a_merge_conflict_opens_the_resolver_and_apply_stages_the_result() {
        let (mut app, dir) = test_app("merge-resolver");
        let root = app.workspace.root.clone();
        init_repo(&app, &[("a.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")]);
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&root, &["switch", "-qc", "dev"]);
        std::fs::write(root.join("a.txt"), "one\nTWO dev\nthree\nFOUR dev\nfive\nSIX dev\n").unwrap();
        git(&root, &["commit", "-qam", "dev"]);
        git(&root, &["switch", "-q", &main]);
        std::fs::write(root.join("a.txt"), "one\nTWO main\nthree\nfour\nfive\nSIX main\n").unwrap();
        git(&root, &["commit", "-qam", "main"]);
        app.git_request(crate::git::GitRequest::Merge("dev".into()));
        handle_next(&mut app, op_done).await;
        // The next status offers the conflicted files.
        feed_status(&mut app, 1);
        let d = app.dialog.take().expect("Resolve Conflicts");
        assert_eq!(d.id, DialogId::ResolveConflictPick(vec!["a.txt".into()]));
        app.finish_dialog(d.id, DialogResult::Choice(0));
        let d = app.dialog.take().expect("merge or take a side");
        assert!(matches!(&d.id, DialogId::ResolveConflict { merge: true, .. }));
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, conflict).await;
        assert!(app.editor.merge_active());
        assert!(app.contexts().contains(&"editor.merge"));
        // Other files stay closed meanwhile; Esc with nothing typed closes.
        app.open_file(root.join("a.txt"), true);
        assert_eq!(app.notifications.last().unwrap().text, crate::editor::merge::BUSY);
        app.perform(merge(MergeAction::Cancel));
        assert!(!app.editor.merge_active() && app.editor.buffers.is_empty());
        // Enter on the U entry in the Commit tab opens it too.
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::OpenFile));
        handle_next(&mut app, conflict).await;
        assert!(app.editor.merge_active());
        assert_eq!(app.focus.current, PanelId::Editor);
        app.perform(merge(MergeAction::AcceptLeft));
        app.perform(merge(MergeAction::NextConflict));
        app.perform(merge(MergeAction::AcceptLeft));
        app.perform(merge(MergeAction::AcceptRight));
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!(app.notifications.last().unwrap().text, crate::editor::merge::BUSY);
        app.perform(merge(MergeAction::Apply));
        assert!(app.dialog.is_none(), "nothing left unresolved");
        assert!(!app.editor.merge_active());
        let merged = "one\nTWO main\nthree\nFOUR dev\nfive\nSIX main\nSIX dev\n";
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), merged);
        assert_eq!(app.editor.active_buffer().unwrap().text(), merged, "the file's tab shows it");
        handle_next(&mut app, op_done).await;
        let repo = git2::Repository::open(&root).unwrap();
        assert!(!repo.index().unwrap().has_conflicts(), "staged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_conflicted_file_opens_the_resolver_from_the_tree_and_changes() {
        use crate::actions::TreeAction;
        let (mut app, dir) = test_app("merge-tree-open");
        let root = app.workspace.root.clone();
        init_repo(&app, &[("a.txt", "one\ntwo\n"), ("b.txt", "b\n")]);
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&root, &["switch", "-qc", "dev"]);
        std::fs::write(root.join("a.txt"), "ONE dev\ntwo\n").unwrap();
        git(&root, &["commit", "-qam", "dev"]);
        git(&root, &["switch", "-q", &main]);
        std::fs::write(root.join("a.txt"), "ONE main\ntwo\n").unwrap();
        git(&root, &["commit", "-qam", "main"]);
        app.git_request(crate::git::GitRequest::Merge("dev".into()));
        handle_next(&mut app, op_done).await;
        feed_status(&mut app, 1);
        app.dialog = None;
        // Changes: the U entry is selected and no marker diff is requested.
        assert!(app.git_panel.diff_key().is_none());
        let req = app.project.begin_refresh();
        match req {
            Some(req) => {
                let generation = app.workspace.generation;
                app.handle_msg(AppMsg::TreeLoaded { generation, seq: req.seq, dirs: req.run() });
            }
            None => handle_next(&mut app, |m| matches!(m, AppMsg::TreeLoaded { .. })).await,
        }
        // Ctrl+D on it goes to the resolver, not to a diff tab.
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::ShowDiff));
        handle_next(&mut app, conflict).await;
        assert!(app.editor.merge_active());
        assert!(app.editor.buffers.iter().all(|b| b.diff.is_none()));
        app.perform(merge(MergeAction::Cancel));
        // Tree: Enter on the conflicted file opens the resolver.
        assert!(app.project.select_path(&root.join("a.txt")));
        app.focus.set(PanelId::Project);
        app.perform(Action::Tree(TreeAction::Open));
        handle_next(&mut app, conflict).await;
        assert!(app.editor.merge_active());
        assert_eq!(app.focus.current, PanelId::Editor);
        // With the resolver open the plain open answers BUSY.
        app.open_or_resolve(root.join("a.txt"));
        assert_eq!(app.notifications.last().unwrap().text, crate::editor::merge::BUSY);
        app.perform(merge(MergeAction::Cancel));
        assert!(!app.editor.merge_active());
        // Tree: a double-click on the conflicted file opens the resolver too.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let left = app.layout.rects.left;
        let y = (left.y..left.bottom())
            .find(|&y| {
                let row: String = (left.x..left.right()).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect();
                row.contains("a.txt")
            })
            .expect("a.txt row in the tree");
        let press = |app: &mut crate::app::App| {
            use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
            for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
                app.handle_mouse(MouseEvent { kind, column: left.x + 2, row: y, modifiers: KeyModifiers::NONE });
            }
        };
        app.focus.set(PanelId::Editor);
        press(&mut app);
        assert!(app.focus.is(PanelId::Project), "the press lands on the tree");
        assert!(!app.editor.merge_active(), "one click only selects");
        press(&mut app);
        handle_next(&mut app, conflict).await;
        assert!(app.editor.merge_active());
        assert_eq!(app.focus.current, PanelId::Editor);
        app.perform(merge(MergeAction::Cancel));
        assert!(!app.editor.merge_active());

        // A plain file opens as before.
        assert!(app.project.select_path(&root.join("b.txt")));
        app.focus.set(PanelId::Project);
        app.perform(Action::Tree(TreeAction::Open));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert!(app.editor.index_of(&root.join("b.txt")).is_some());
        assert!(!app.editor.merge_active());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resolving_a_file_offers_the_next_one() {
        let (mut app, dir) = test_app("merge-next");
        let root = app.workspace.root.clone();
        init_repo(&app, &[("a.txt", "one\ntwo\n"), ("b.txt", "x\ny\n")]);
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&root, &["switch", "-qc", "dev"]);
        std::fs::write(root.join("a.txt"), "ONE dev\ntwo\n").unwrap();
        std::fs::write(root.join("b.txt"), "X dev\ny\n").unwrap();
        git(&root, &["commit", "-qam", "dev"]);
        git(&root, &["switch", "-q", &main]);
        std::fs::write(root.join("a.txt"), "ONE main\ntwo\n").unwrap();
        std::fs::write(root.join("b.txt"), "X main\ny\n").unwrap();
        git(&root, &["commit", "-qam", "main"]);
        app.git_request(crate::git::GitRequest::Merge("dev".into()));
        handle_next(&mut app, op_done).await;
        feed_status(&mut app, 1);
        let d = app.dialog.take().expect("Resolve Conflicts");
        assert_eq!(d.id, DialogId::ResolveConflictPick(vec!["a.txt".into(), "b.txt".into()]));
        app.finish_dialog(d.id, DialogResult::Choice(0));
        let d = app.dialog.take().expect("merge or take a side");
        assert!(matches!(&d.id, DialogId::ResolveConflict { merge: true, .. }));
        // Taken before the resolve starts, so it still lists a.txt.
        let stale = crate::git::repo::status(&root);
        app.finish_dialog(d.id, DialogResult::Button(1));
        handle_next(&mut app, op_done).await;
        // A status from before the resolve still lists a.txt: nothing yet.
        app.handle_msg(AppMsg::Git(GitMsg::Status { generation: app.git.generation, seq: 2, result: stale }));
        assert!(app.dialog.is_none());
        feed_status(&mut app, 3);
        let d = app.dialog.take().expect("the next file");
        assert_eq!(d.id, DialogId::ResolveConflictPick(vec!["b.txt".into()]));
        app.finish_dialog(d.id, DialogResult::Choice(0));
        let d = app.dialog.take().expect("merge or take a side");
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, conflict).await;
        app.perform(merge(MergeAction::AcceptLeft));
        app.perform(merge(MergeAction::Apply));
        handle_next(&mut app, op_done).await;
        // A failed status says nothing about the files left: no verdict yet.
        app.handle_msg(AppMsg::Git(GitMsg::Status {
            generation: app.git.generation,
            seq: 4,
            result: Err(anyhow::anyhow!("status failed")),
        }));
        assert!(app.dialog.is_none());
        assert!(app.notifications.iter().all(|n| n.text != "All conflicts resolved"));
        feed_status(&mut app, 5);
        assert!(app.dialog.is_none());
        assert_eq!(app.notifications.last().unwrap().text, "All conflicts resolved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quitting_with_merge_work_asks_and_the_session_keeps_the_old_tabs() {
        let (mut app, dir) = test_app("merge-quit");
        let a = crate::app::tests::load(&mut app, "a.txt", "a\n");
        let texts = crate::git::merge::MergeTexts { base: "x\n".into(), ours: "y\n".into(), theirs: "z\n".into() };
        app.editor.open_merge(a.clone(), "a.txt".into(), texts, ["main".into(), "dev".into()], "x\n".into(), &app.theme);
        let session = app.session();
        assert_eq!(session.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), [a]);
        assert_eq!(session.active, Some(0));
        app.perform(merge(MergeAction::AcceptLeft));
        app.request_quit();
        let d = app.dialog.take().expect("asks");
        assert_eq!(d.id, DialogId::MergeQuit);
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert!(app.should_quit && !app.editor.merge_active());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
