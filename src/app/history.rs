//! Local history in the app: the Local History palette, the History tab's
//! diff, reverting to a snapshot, snapshots of files on disk before they
//! are overwritten, and periodic snapshots of unsaved edits (`history/`
//! holds the store and its worker).

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{App, Ctx};
use crate::actions::PaletteKind;
use crate::event::{AppMsg, Level};
use crate::git::{DiffKey, GitMsg};
use crate::history::{Capture, CaptureText, HistoryMsg, Kind, Request};
use crate::ui::focus::{PanelId, ToolWindowId};
use crate::ui::palette::Palette;

impl App {
    /// `editor.local_history`: the selected tree entry when the tree has
    /// focus, else the active file.
    pub(super) fn local_history(&mut self) {
        let path = if self.focus.is(PanelId::Project) {
            self.project.selected_row().map(|r| r.path.clone())
        } else {
            self.editor.active_path().map(Path::to_path_buf)
        };
        let Some(path) = path else {
            return self.notify(Level::Info, "No file selected");
        };
        if path.is_dir() {
            return self.notify(Level::Info, "Local history is per file: select a file");
        }
        self.open_local_history(path);
    }

    /// The Local History palette of `path`; its snapshots are asked for and
    /// arrive as `HistoryMsg::List`.
    pub(crate) fn open_local_history(&mut self, path: PathBuf) {
        let mut p = Palette::new(PaletteKind::LocalHistory, self.workspace.root.clone());
        p.set_history_target(path.clone());
        self.menu = None;
        self.palette = Some(p);
        self.dispatcher.reset();
        self.history.list(&path);
    }

    pub(super) fn history_request(&mut self, req: Request) {
        match req {
            Request::Capture(c) => self.history.capture(c),
            Request::Show(path) => self.open_local_history(path),
            Request::Revert { path, id } => self.history.blob(path, id),
        }
    }

    pub(super) fn history_msg(&mut self, msg: HistoryMsg) {
        match msg {
            HistoryMsg::List { path, entries } => {
                if let Some(p) = self.palette.as_mut().filter(|p| p.history_path() == Some(path.as_path())) {
                    p.set_history(entries, crate::ui::event_log::local_offset);
                }
            }
            HistoryMsg::Blob { path, id, result } => {
                // Only the snapshot the History tab still shows.
                let shown = matches!(self.git_panel.history_key(), Some(DiffKey::Local { path: p, id: i, .. }) if *p == path && *i == id);
                if !shown {
                    return tracing::debug!("dropping a snapshot no longer shown: {}", path.display());
                }
                match result.map(String::from_utf8) {
                    Ok(Ok(text)) => self.revert_to(path, text),
                    Ok(Err(_)) => self.notify(Level::Warn, "The snapshot is not valid UTF-8; not reverted"),
                    Err(e) => self.notify(Level::Error, format!("Local history: {e}")),
                }
            }
        }
    }

    /// Accepted a Local History row: its diff in the Git window.
    pub(super) fn show_local_diff(&mut self, path: PathBuf, id: String, at: u64, title: String) {
        let rel = crate::ui::palette::display_path(&path, &self.workspace.root);
        self.layout.show(ToolWindowId::Git);
        self.focus.set(PanelId::Git);
        let mut cx = ctx!(self);
        self.git_panel.show_local_diff(DiffKey::Local { path, rel, id, at }, title, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// `GitRequest::LoadDiff` of a snapshot: the snapshot against the open
    /// buffer (a rope clone) or the file on disk (a missing file is empty),
    /// built off the main thread; answered as a `GitMsg::Diff`, keyed by
    /// `DiffKey` like every diff.
    pub(super) fn load_local_diff(&mut self, key: DiffKey) {
        let DiffKey::Local { path, id, .. } = &key else { return };
        let current = self.editor.index_of(path).map(|i| self.editor.buffers[i].rope.clone());
        let (dir, path, id) = (self.history.dir().to_path_buf(), path.clone(), id.clone());
        let (generation, tx, tab_width) = (self.git.generation, self.bus_tx.clone(), self.git.tab_width);
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                let old = crate::history::store::read_blob(&dir, &id).map_err(|e| anyhow::anyhow!("snapshot {id}: {e}"))?;
                let new = match current {
                    Some(rope) => rope.to_string().into_bytes(),
                    None => match std::fs::read(&path) {
                        Ok(b) => b,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                        Err(e) => anyhow::bail!("{}: {e}", path.display()),
                    },
                };
                Ok(crate::git::diff::DiffDoc::build(&old, &new, tab_width))
            })();
            let _ = tx.send(AppMsg::Git(GitMsg::Diff { generation, key, result }));
        });
    }

    /// A snapshot's text arrived for a revert: into the open tab, else once
    /// the file opens (`pending_revert`); a file deleted meanwhile gets an
    /// empty tab with its path, so saving recreates it.
    fn revert_to(&mut self, path: PathBuf, text: String) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        if self.editor.index_of(&path).is_some() {
            return self.apply_revert(&path, &text);
        }
        if path.exists() {
            self.pending_revert = Some((path.clone(), text));
            return self.open_file(path, true);
        }
        self.editor.open_text(path.clone(), "", false, &self.theme);
        self.editor.enforce_tab_limit();
        self.apply_revert(&path, &text);
    }

    /// Put `text` into `path`'s open tab as one undoable change, show the
    /// tab and compare again.
    pub(super) fn apply_revert(&mut self, path: &Path, text: &str) {
        let Some(i) = self.editor.index_of(path) else { return };
        let mut cx = ctx!(self);
        self.editor.activate(i, &mut cx);
        let mut fx = cx.effects;
        self.focus.set(PanelId::Editor);
        let name = self.editor.buffers[i].name.clone();
        let msg = if self.editor.restore_text(i, text) {
            format!("Reverted {name} to {} (Ctrl+Z undoes)", self.git_panel.history_title())
        } else {
            format!("{name} already has the text of that snapshot")
        };
        self.notify(Level::Info, msg);
        let mut cx = ctx!(self);
        self.git_panel.reload_history_diff(&mut cx);
        fx.extend(cx.effects);
        self.apply_effects(fx);
    }

    /// The tick's part: when due, snapshot the unsaved edits of open files
    /// and arm the next period. Nothing is taken (nor marked seen) while
    /// local history is off.
    pub(super) fn unsaved_snapshots(&mut self, now: Instant) {
        let (Some(due), Some(every)) = (self.unsaved_due, self.config.local_history.unsaved_every()) else { return };
        if now < due {
            return;
        }
        self.unsaved_due = Some(now + every);
        if !self.history.enabled() {
            return;
        }
        let max = self.config.local_history.max_file_bytes();
        for c in self.editor.unsaved_captures(max) {
            self.history.capture(c);
        }
    }

    /// Snapshot a file on disk (main thread, bounded by `max_file_kb`)
    /// before it is overwritten.
    pub(super) fn capture_disk(&mut self, path: &Path, kind: Kind) {
        if !self.history.enabled() {
            return;
        }
        let max = self.history.settings().max_file_bytes();
        let small = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= max);
        if let Some(bytes) = small.then(|| std::fs::read(path).ok()).flatten() {
            self.history.capture(Capture { path: path.to_path_buf(), text: CaptureText::Bytes(bytes), kind, detail: None });
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;
    use crate::actions::{Action, AppAction, EditorAction, GitAction, PaletteAction};
    use crate::app::tests::{handle_next, init_repo, load, test_app, with_history};
    use crate::files::ops::FsOp;
    use crate::git::panel::GitTab;
    use crate::history::store;
    use crate::ui::dialog::{DialogId, DialogResult};
    use crate::ui::palette::{PaletteResult, Source};

    /// `path`'s snapshots once the worker is done, newest first: label and
    /// text.
    pub(crate) fn snapshots(app: &App, path: &Path) -> Vec<(String, String)> {
        app.history.flush(Duration::from_secs(5));
        store::list(app.history.dir(), path)
            .into_iter()
            .map(|e| (e.label(), String::from_utf8(store::read_blob(app.history.dir(), &e.blob).unwrap()).unwrap()))
            .collect()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn type_text(app: &mut App, text: &str) {
        app.perform(Action::Editor(EditorAction::InsertText(text.into())));
    }

    fn history_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::LocalHistory(_))
    }

    fn diff_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Git(GitMsg::Diff { .. }))
    }

    #[tokio::test]
    async fn saves_list_diff_revert_and_undo() {
        let (mut app, dir) = test_app("history-flow");
        with_history(&mut app);
        assert!(app.history.dir().starts_with(&dir), "never the real state directory");
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "two ");
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(snapshots(&app, &path), pairs(&[("Saved", "two one\n")]));
        type_text(&mut app, "three ");
        app.perform(Action::Editor(EditorAction::Save));
        // A save of a clean buffer writes nothing and records nothing.
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(snapshots(&app, &path), pairs(&[("Saved", "two three one\n"), ("Saved", "two one\n")]));

        // File > Local History…: the rows arrive from the worker.
        app.perform(Action::App(AppAction::LocalHistory));
        assert_eq!(app.palette.as_ref().map(|p| p.source()), Some(Source::LocalHistory));
        handle_next(&mut app, history_msg).await;
        let p = app.palette.as_mut().unwrap();
        p.handle_action(&PaletteAction::Down);
        let Some(PaletteResult::Accept(accept)) = p.handle_action(&PaletteAction::Accept) else { panic!("a row") };
        app.finish_palette(PaletteResult::Accept(accept));
        assert_eq!(app.git_panel.tab, GitTab::History);
        assert!(app.focus.is(PanelId::Git));
        assert!(app.git_panel.history_title().ends_with("  Saved"), "{}", app.git_panel.history_title());
        handle_next(&mut app, diff_msg).await;
        assert!(matches!(app.git_panel.history_key(), Some(DiffKey::Local { rel, .. }) if rel == "a.txt"));

        // Alt+R: the older snapshot comes back as one undoable edit.
        app.perform(Action::Git(GitAction::HistoryRevert));
        handle_next(&mut app, history_msg).await;
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.text(), "two one\n");
        assert!(b.dirty());
        assert!(app.focus.is(PanelId::Editor));
        assert!(app.notifications.last().is_some_and(|n| n.text.starts_with("Reverted a.txt to ")), "{:?}", app.notifications);
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(app.editor.active_buffer().unwrap().text(), "two three one\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unsaved_edits_are_snapshotted_on_the_timer() {
        let (mut app, dir) = test_app("history-unsaved");
        let set = |app: &mut App, f: &dyn Fn(&mut crate::history::LocalHistoryConfig)| {
            let mut config = app.config.clone();
            f(&mut config.local_history);
            app.apply_config(config);
        };
        set(&mut app, &|c| c.snapshot_unsaved_secs = 30);
        with_history(&mut app);
        let due = app.unsaved_due.expect("armed");
        let secs = Duration::from_secs;
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        app.unsaved_snapshots(due - Duration::from_millis(1));
        assert!(snapshots(&app, &path).is_empty(), "not due yet");
        app.unsaved_snapshots(due);
        assert_eq!(snapshots(&app, &path), pairs(&[("Unsaved changes", "x one\n")]));
        assert_eq!(app.unsaved_due, Some(due + secs(30)));
        app.unsaved_snapshots(due);
        assert_eq!(app.unsaved_due, Some(due + secs(30)), "not due again");
        // The next period's snapshot replaces this one (one run).
        type_text(&mut app, "y ");
        app.unsaved_snapshots(due + secs(30));
        assert_eq!(snapshots(&app, &path), pairs(&[("Unsaved changes", "x y one\n")]));
        // A save of that text relabels it.
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(snapshots(&app, &path), pairs(&[("Saved", "x y one\n")]));

        // Off: nothing is taken, and the edits are taken once it is back on.
        type_text(&mut app, "z ");
        set(&mut app, &|c| c.enabled = false);
        app.unsaved_snapshots(due + secs(60));
        assert_eq!(snapshots(&app, &path).len(), 1);
        set(&mut app, &|c| c.enabled = true);
        app.unsaved_snapshots(due + secs(90));
        assert_eq!(snapshots(&app, &path), pairs(&[("Unsaved changes", "x y z one\n"), ("Saved", "x y one\n")]));

        // 0 stops the timer.
        set(&mut app, &|c| c.snapshot_unsaved_secs = 0);
        assert_eq!(app.unsaved_due, None);
        type_text(&mut app, "w ");
        app.unsaved_snapshots(due + secs(3600));
        assert_eq!(snapshots(&app, &path).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn revert_of_a_file_that_is_not_open_applies_once_it_loads() {
        let (mut app, dir) = test_app("history-unopened");
        with_history(&mut app);
        let path = app.workspace.root.join("b.txt");
        std::fs::write(&path, "new\n").unwrap();
        let store::Recorded::Added(e) =
            store::record(app.history.dir(), &path, b"old\n", Kind::Save, None, crate::history::now_ms(), 1 << 20).unwrap()
        else {
            panic!("recorded")
        };
        app.show_local_diff(path.clone(), e.blob, e.at, "then  Saved".into());
        app.perform(Action::Git(GitAction::HistoryRevert));
        handle_next(&mut app, history_msg).await;
        assert!(app.editor.index_of(&path).is_none());
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        let b = app.editor.active_buffer().unwrap();
        assert_eq!((b.text(), b.dirty()), ("old\n".to_string(), true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n", "nothing written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deletes_overwrites_and_git_operations_snapshot_first() {
        let (mut app, dir) = test_app("history-before");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        // A tree delete: the files are stored before they go.
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/x.txt"), "x\n").unwrap();
        app.run_fs_op(FsOp::Delete { path: root.join("d") });
        handle_next(&mut app, |m| matches!(m, AppMsg::FsOpDone { .. })).await;
        assert!(!root.join("d").exists());
        assert_eq!(snapshots(&app, &root.join("d/x.txt")), pairs(&[("Before delete", "x\n")]));

        // Overwrite of an outside change keeps the outside text.
        let a = load(&mut app, "a.txt", "mine\n");
        type_text(&mut app, "x ");
        std::fs::write(&a, "theirs\n").unwrap();
        app.editor.active_buffer_mut().unwrap().disk_mtime = Some(std::time::UNIX_EPOCH);
        let id = app.editor.active_buffer().unwrap().id;
        app.finish_dialog(DialogId::OverwriteExternal(id), DialogResult::Button(0));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "x mine\n");
        assert_eq!(snapshots(&app, &a), pairs(&[("Saved", "x mine\n"), ("Before overwrite", "theirs\n")]));

        // A git operation that rewrites the work tree stores changed files.
        init_repo(&app, &[("g.txt", "g\n")]);
        std::fs::write(root.join("g.txt"), "changed\n").unwrap();
        app.git_request(crate::git::GitRequest::Discard(vec![(crate::git::Section::Unstaged, "g.txt".into())]));
        handle_next(&mut app, |m| matches!(m, AppMsg::Git(GitMsg::OpDone { .. }))).await;
        assert_eq!(std::fs::read_to_string(root.join("g.txt")).unwrap(), "g\n");
        assert_eq!(snapshots(&app, &root.join("g.txt")), pairs(&[("Before discard g.txt", "changed\n")]));

        // keep_days = 0 prunes every entry once the config reloads.
        std::thread::sleep(Duration::from_millis(5));
        let mut config = app.config.clone();
        config.local_history.keep_days = 0;
        app.apply_config(config);
        assert!(snapshots(&app, &a).is_empty());
        // Off: saves record nothing.
        let mut config = app.config.clone();
        config.local_history.enabled = false;
        app.apply_config(config);
        type_text(&mut app, "y ");
        app.perform(Action::Editor(EditorAction::Save));
        assert!(snapshots(&app, &a).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_history_needs_a_file() {
        let (mut app, dir) = test_app("history-target");
        app.perform(Action::App(AppAction::LocalHistory));
        assert!(app.palette.is_none());
        assert_eq!(app.notifications.last().map(|n| n.text.as_str()), Some("No file selected"));
        app.focus.set(PanelId::Project);
        std::fs::create_dir_all(app.workspace.root.join("sub")).unwrap();
        app.project.select_path(&app.workspace.root.join("sub"));
        app.perform(Action::App(AppAction::LocalHistory));
        assert!(app.palette.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
