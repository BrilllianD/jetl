//! Local history in the app: the Local History window (`history/panel.rs`)
//! and its requests (the list, the diff of a snapshot against the current
//! text, reverting to a snapshot, labels: Put Label and removal), the
//! window following Project tree renames, snapshots of files on disk before
//! they are overwritten, and periodic snapshots of unsaved edits
//! (`history/` holds the store and its worker).

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{App, Ctx};
use crate::event::{AppMsg, Level};
use crate::git::DiffKey;
use crate::history::{Capture, CaptureText, Entry, HistoryMsg, Kind, MAX_LABEL_CHARS, Request};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::{PanelId, ToolWindowId};

/// Why Put Label does nothing.
const HISTORY_OFF: &str = "Local history is off ([local_history] enabled = false)";

impl App {
    /// `editor.local_history`: the Local History window on the selected
    /// tree entry when the tree has focus, else on the active file.
    pub(super) fn local_history(&mut self) {
        if let Some(path) = self.history_target() {
            self.show_local_history(path);
        }
    }

    /// The file local history actions work on: the selected tree entry when
    /// the tree has focus, else the active file; `None`, with a
    /// notification, when there is none or it is a folder.
    fn history_target(&mut self) -> Option<PathBuf> {
        let path = if self.focus.is(PanelId::Project) {
            self.project.selected_row().map(|r| r.path.clone())
        } else {
            self.editor.active_path().map(Path::to_path_buf)
        };
        let Some(path) = path else {
            self.notify(Level::Info, "No file selected");
            return None;
        };
        if path.is_dir() {
            self.notify(Level::Info, "Local history is per file: select a file");
            return None;
        }
        Some(path)
    }

    /// Show and focus the Local History window on `path`; its snapshots
    /// are asked for and arrive as `HistoryMsg::List`.
    pub(crate) fn show_local_history(&mut self, path: PathBuf) {
        self.layout.show(ToolWindowId::LocalHistory);
        self.focus.set(PanelId::LocalHistory);
        self.history_panel.set_target(path.clone(), &self.workspace.root);
        self.history.list(&path);
    }

    /// The window opened with no file yet: the active file, if any.
    pub(super) fn history_window_opened(&mut self) {
        if self.history_panel.target().is_none()
            && let Some(path) = self.editor.active_path().map(Path::to_path_buf)
        {
            self.history_panel.set_target(path.clone(), &self.workspace.root);
            self.history.list(&path);
        }
    }

    /// A Project tree rename succeeded: carry the history (queued before
    /// the tabs follow, so snapshots of the old path queued until now are
    /// carried), and move the Local History window along, listing again
    /// after the carry.
    pub(super) fn history_renamed(&mut self, from: &Path, to: &Path) {
        self.history.rename(from, to);
        if let Some(path) = self.history_panel.retarget(from, to, &self.workspace.root) {
            self.history.list(&path);
        }
    }

    /// `editor.put_label`: ask a name for a label on the Local History target.
    pub(super) fn put_label(&mut self) {
        if !self.history.enabled() {
            return self.notify(Level::Info, HISTORY_OFF);
        }
        if let Some(path) = self.history_target() {
            self.ask_put_label(path);
        }
    }

    /// The Put Label dialog for `path` (also the window's Put Label).
    fn ask_put_label(&mut self, path: PathBuf) {
        if !self.history.enabled() {
            return self.notify(Level::Info, HISTORY_OFF);
        }
        let rel = crate::ui::palette::display_path(&path, &self.workspace.root);
        self.dialog = Some(Dialog::input(DialogId::PutLabel(path), "Put Label", &format!("Label for {rel}:"), "", "Put"));
    }

    /// The Put Label dialog's name: trimmed, cut to [`MAX_LABEL_CHARS`]; the
    /// open tab's text (unsaved edits included), else the file on disk.
    pub(super) fn finish_put_label(&mut self, path: PathBuf, text: &str) {
        let name = text.trim().chars().take(MAX_LABEL_CHARS).collect::<String>().trim_end().to_string();
        if name.is_empty() {
            return self.notify(Level::Warn, "Put Label: a label needs a name");
        }
        let tab = self.editor.index_of(&path).map(|i| CaptureText::Rope(self.editor.buffers[i].rope.clone()));
        if !self.history.put_label(path, tab, name) {
            // Turned off while the dialog was open.
            self.notify(Level::Info, HISTORY_OFF);
        }
    }

    /// Remove Label in the Local History window: ask first.
    fn ask_remove_label(&mut self, path: PathBuf, entry: Entry) {
        let name = entry.detail.clone().unwrap_or_default();
        let rel = crate::ui::palette::display_path(&path, &self.workspace.root);
        let msg = format!("Remove label \"{name}\" from the local history of {rel}?\nIts text goes too unless another snapshot holds it.");
        self.dialog =
            Some(Dialog::confirm(DialogId::RemoveLabel { path, entry: Box::new(entry) }, "Remove Label", &msg, &["Remove", "Cancel"]));
    }

    /// The remove dialog's answer; the window lists again once the worker
    /// reports (`HistoryMsg::LabelRemoved`).
    pub(super) fn finish_remove_label(&mut self, path: PathBuf, entry: Entry, remove: bool) {
        if remove {
            self.history.remove_label(path, entry);
        }
    }

    /// List `path` again when the window shows it.
    fn relist_if_shown(&mut self, path: &Path) {
        if self.history_panel.target() == Some(path) {
            self.history.list(path);
        }
    }

    pub(super) fn history_request(&mut self, req: Request) {
        match req {
            Request::Capture(c) => self.history.capture(c),
            Request::List(path) => self.history.list(&path),
            Request::Diff { key, seq } => self.load_local_diff(key, seq),
            Request::Revert { path, id } => self.history.blob(path, id),
            Request::PutLabel(path) => self.ask_put_label(path),
            Request::RemoveLabel { path, entry } => self.ask_remove_label(path, *entry),
        }
    }

    pub(super) fn history_msg(&mut self, msg: HistoryMsg) {
        match msg {
            HistoryMsg::List { path, entries } => {
                if self.history_panel.target() == Some(path.as_path()) {
                    let mut cx = ctx!(self);
                    self.history_panel.set_entries(entries, &self.workspace.root, crate::ui::event_log::local_offset, &mut cx);
                    let fx = cx.effects;
                    self.apply_effects(fx);
                }
            }
            // A snapshot of the file shown landed (a save, the unsaved
            // timer, a git operation): list again.
            HistoryMsg::Stored { path } => self.relist_if_shown(&path),
            HistoryMsg::Diff { key, seq, result } => self.history_panel.set_diff(&key, seq, result),
            HistoryMsg::Blob { path, id, result } => {
                // Only the snapshot the window still has selected.
                if !self.history_panel.shows(&path, &id) {
                    return tracing::debug!("dropping a snapshot no longer selected: {}", path.display());
                }
                match result.map(String::from_utf8) {
                    Ok(Ok(text)) => self.revert_to(path, text),
                    Ok(Err(_)) => self.notify(Level::Warn, "The snapshot is not valid UTF-8; not reverted"),
                    Err(e) => self.notify(Level::Error, format!("Local history: {e}")),
                }
            }
            HistoryMsg::Labelled { path, name, result } => match result {
                Ok(()) => {
                    let rel = crate::ui::palette::display_path(&path, &self.workspace.root);
                    self.notify(Level::Info, format!("Put label \"{name}\" on {rel}"));
                    self.relist_if_shown(&path);
                }
                Err(e) => self.notify(Level::Warn, format!("Put Label: {e}")),
            },
            HistoryMsg::LabelRemoved { path, name, result } => {
                match result {
                    Ok(true) => self.notify(Level::Info, format!("Removed label \"{name}\"")),
                    Ok(false) => self.notify(Level::Info, format!("Label \"{name}\" was already gone")),
                    Err(e) => self.notify(Level::Warn, format!("Remove label: {e}")),
                }
                self.relist_if_shown(&path);
            }
        }
    }

    /// The diff of a snapshot against the open buffer (a rope clone) or the
    /// file on disk (a missing file is empty), built off the main thread;
    /// answered as `HistoryMsg::Diff` with the window's `seq`.
    pub(super) fn load_local_diff(&mut self, key: DiffKey, seq: u64) {
        let DiffKey::Local { path, id, .. } = &key else { return };
        let current = self.editor.index_of(path).map(|i| self.editor.buffers[i].rope.clone());
        let (dir, path, id) = (self.history.dir().to_path_buf(), path.clone(), id.clone());
        let (tx, tab_width) = (self.bus_tx.clone(), self.git.tab_width);
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
            let _ = tx.send(AppMsg::LocalHistory(HistoryMsg::Diff { key, seq, result }));
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
            format!("Reverted {name} to {} (Ctrl+Z undoes)", self.history_panel.selected_title())
        } else {
            format!("{name} already has the text of that snapshot")
        };
        self.notify(Level::Info, msg);
        let mut cx = ctx!(self);
        self.history_panel.reload_diff(&mut cx);
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

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::actions::{Action, AppAction, EditorAction, HistoryAction};
    use crate::app::tests::{handle_next, init_repo, load, test_app, with_history};
    use crate::files::ops::FsOp;
    use crate::git::GitMsg;
    use crate::history::store;
    use crate::ui::dialog::{DialogId, DialogResult};

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

    /// A list of `path`'s snapshots.
    fn list_of(path: &Path) -> impl Fn(&AppMsg) -> bool + '_ {
        move |m| matches!(m, AppMsg::LocalHistory(HistoryMsg::List { path: p, .. }) if p == path)
    }

    fn diff_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::LocalHistory(HistoryMsg::Diff { .. }))
    }

    fn blob_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::LocalHistory(HistoryMsg::Blob { .. }))
    }

    fn stored_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::LocalHistory(HistoryMsg::Stored { .. }))
    }

    fn label_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::LocalHistory(HistoryMsg::Labelled { .. } | HistoryMsg::LabelRemoved { .. }))
    }

    fn history(app: &mut App, a: HistoryAction) {
        app.perform(Action::History(a));
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// The window's diff of `path` is loaded and its snapshot holds `text`.
    fn diff_shows(app: &App, path: &Path, text: &str) -> bool {
        match app.history_panel.diff_state() {
            (Some(DiffKey::Local { path: p, id, .. }), true) => {
                p == path && store::read_blob(app.history.dir(), id).unwrap() == text.as_bytes()
            }
            _ => false,
        }
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

        // File > Local History…: the window on the file, its rows from the worker.
        app.perform(Action::App(AppAction::LocalHistory));
        assert!(app.layout.is_visible(ToolWindowId::LocalHistory));
        assert!(app.focus.is(PanelId::LocalHistory));
        assert_eq!(app.history_panel.target(), Some(path.as_path()));
        handle_next(&mut app, list_of(&path)).await;
        assert_eq!(app.history_panel.labels(), ["Saved", "Saved"]);
        // The newest is selected and its diff follows.
        handle_next(&mut app, diff_msg).await;
        assert!(diff_shows(&app, &path, "two three one\n"));
        history(&mut app, HistoryAction::Down);
        handle_next(&mut app, diff_msg).await;
        assert!(diff_shows(&app, &path, "two one\n"));

        // Alt+R: the older snapshot comes back as one undoable edit.
        history(&mut app, HistoryAction::Revert);
        handle_next(&mut app, blob_msg).await;
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.text(), "two one\n");
        assert!(b.dirty());
        assert!(app.focus.is(PanelId::Editor));
        assert!(app.notifications.last().is_some_and(|n| n.text.starts_with("Reverted a.txt to ")), "{:?}", app.notifications);
        assert!(app.notifications.last().is_some_and(|n| n.text.ends_with("  Saved (Ctrl+Z undoes)")), "{:?}", app.notifications);
        // The window compares again, with the reverted text.
        handle_next(&mut app, diff_msg).await;
        assert!(app.history_panel.diff_state().1);
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(app.editor.active_buffer().unwrap().text(), "two three one\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_list_follows_new_snapshots_keeping_filter_and_selection() {
        let (mut app, dir) = test_app("history-live");
        with_history(&mut app);
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        app.perform(Action::Editor(EditorAction::Save));
        app.finish_dialog(DialogId::PutLabel(path.clone()), DialogResult::Text("v1".into()));
        type_text(&mut app, "y ");
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(snapshots(&app, &path).len(), 3);
        app.perform(Action::App(AppAction::LocalHistory));
        handle_next(&mut app, list_of(&path)).await;
        assert_eq!(app.history_panel.labels(), ["Saved", "Label: v1", "Saved"]);
        // Typing filters (case-insensitive); the selection stays on the row.
        history(&mut app, HistoryAction::Down);
        key(&mut app, KeyCode::Char('S'));
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(app.history_panel.filter(), "Sa");
        assert_eq!(app.history_panel.labels(), ["Saved", "Saved"]);
        history(&mut app, HistoryAction::Down);
        assert_eq!(app.history_panel.selected_label(), Some("Saved"));
        // A save while the window shows the file: listed again by itself.
        type_text(&mut app, "z ");
        app.perform(Action::Editor(EditorAction::Save));
        handle_next(&mut app, stored_msg).await;
        handle_next(&mut app, list_of(&path)).await;
        assert_eq!(app.history_panel.labels(), ["Saved", "Saved", "Saved"]);
        assert_eq!(app.history_panel.filter(), "Sa");
        // Still the oldest save: the third row now.
        handle_next(&mut app, diff_msg).await;
        assert!(diff_shows(&app, &path, "x one\n"));
        // Esc clears the filter first, then leaves the window.
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.history_panel.filter(), "");
        assert_eq!(app.history_panel.labels().len(), 4);
        assert!(app.focus.is(PanelId::LocalHistory));
        key(&mut app, KeyCode::Esc);
        assert!(app.focus.is(PanelId::Editor));
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
        assert_eq!(e.blob.len(), 40);
        app.focus.set(PanelId::Project);
        app.project.select_path(&path);
        app.show_local_history(path.clone());
        handle_next(&mut app, list_of(&path)).await;
        history(&mut app, HistoryAction::Revert);
        handle_next(&mut app, blob_msg).await;
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
    async fn worktree_force_remove_snapshots_its_changed_files() {
        let (mut app, dir) = test_app("history-worktree");
        with_history(&mut app);
        init_repo(&app, &[("a.txt", "one\n")]);
        let root = app.workspace.root.clone();
        let wt = std::fs::canonicalize(&dir).unwrap().join("wt");
        let wt_arg = wt.to_string_lossy().into_owned();
        crate::git::cli::git_ok(&root, &crate::git::cli::args(&["worktree", "add", "-b", "feature", &wt_arg])).await.unwrap();
        std::fs::write(wt.join("a.txt"), "edited in the worktree\n").unwrap();
        std::fs::write(wt.join("new.txt"), "untracked\n").unwrap();
        // The open root's own changes are not part of it.
        std::fs::write(root.join("a.txt"), "root edit\n").unwrap();
        let op_done = |m: &AppMsg| matches!(m, AppMsg::Git(GitMsg::OpDone { .. }));

        // A plain remove fails on the dirty tree and deletes nothing: no
        // snapshot; the dialog offers the forced retry.
        app.finish_dialog(DialogId::WorktreeRemove { path: wt.clone(), force: false }, DialogResult::Button(0));
        handle_next(&mut app, op_done).await;
        assert!(wt.exists());
        assert!(snapshots(&app, &wt.join("a.txt")).is_empty());
        let retry = app.dialog.as_ref().map(|d| d.id.clone());
        assert_eq!(retry, Some(DialogId::WorktreeRemove { path: wt.clone(), force: true }));

        app.dialog = None;
        app.finish_dialog(DialogId::WorktreeRemove { path: wt.clone(), force: true }, DialogResult::Button(0));
        handle_next(&mut app, op_done).await;
        assert!(!wt.exists(), "{:?}", app.notifications);
        let label = "Before git worktree remove --force";
        assert_eq!(snapshots(&app, &wt.join("a.txt")), pairs(&[(label, "edited in the worktree\n")]));
        assert_eq!(snapshots(&app, &wt.join("new.txt")), pairs(&[(label, "untracked\n")]));
        assert!(snapshots(&app, &root.join("a.txt")).is_empty(), "the root is not snapshotted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fs_op_done(m: &AppMsg) -> bool {
        matches!(m, AppMsg::FsOpDone { .. })
    }

    #[tokio::test]
    async fn tree_renames_carry_local_history() {
        let (mut app, dir) = test_app("history-rename");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let a = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        app.perform(Action::Editor(EditorAction::Save));
        type_text(&mut app, "y ");
        app.finish_dialog(DialogId::TreeRename(a.clone()), DialogResult::Text("b.txt".into()));
        handle_next(&mut app, fs_op_done).await;
        let b = root.join("b.txt");
        // The marker holds the text on disk, not the unsaved edits.
        let renamed = format!("Renamed from {}", a.display());
        assert_eq!(snapshots(&app, &b), pairs(&[(&renamed, "x one\n"), ("Saved", "x one\n")]));
        assert_eq!(snapshots(&app, &a), pairs(&[("Saved", "x one\n")]), "the old path keeps its own");
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(snapshots(&app, &b)[0], ("Saved".to_string(), "x y one\n".to_string()));

        // The window on b.txt names the old path relative to the root.
        app.show_local_history(b.clone());
        handle_next(&mut app, list_of(&b)).await;
        assert_eq!(app.history_panel.selected_label(), Some("Saved"));
        history(&mut app, HistoryAction::Down);
        assert_eq!(app.history_panel.selected_label(), Some("Renamed from a.txt"));

        // Every file under a renamed folder; the window follows its file.
        std::fs::create_dir_all(root.join("d")).unwrap();
        let x = load(&mut app, "d/x.txt", "x\n");
        type_text(&mut app, "1");
        app.perform(Action::Editor(EditorAction::Save));
        app.show_local_history(x.clone());
        app.finish_dialog(DialogId::TreeRename(root.join("d")), DialogResult::Text("e".into()));
        handle_next(&mut app, fs_op_done).await;
        let ex = root.join("e/x.txt");
        assert_eq!(app.history_panel.target(), Some(ex.as_path()));
        handle_next(&mut app, list_of(&ex)).await;
        assert_eq!(app.history_panel.labels(), ["Renamed from d/x.txt", "Saved"]);
        handle_next(&mut app, diff_msg).await;
        assert!(diff_shows(&app, &ex, "1x\n"));
        let e = snapshots(&app, &ex);
        assert_eq!(e[0].0, format!("Renamed from {}", root.join("d/x.txt").display()));
        assert_eq!(e.len(), 2);
        // A sibling whose name starts the same is not under it.
        app.show_local_history(root.join("dd.txt"));
        app.finish_dialog(DialogId::TreeRename(ex.clone()), DialogResult::Text("y.txt".into()));
        handle_next(&mut app, fs_op_done).await;
        assert_eq!(app.history_panel.target(), Some(root.join("dd.txt").as_path()));

        // A result that arrives after the root switched still carries, and
        // the window still follows.
        let c = load(&mut app, "c.txt", "c\n");
        type_text(&mut app, "1");
        app.perform(Action::Editor(EditorAction::Save));
        app.show_local_history(c.clone());
        app.finish_dialog(DialogId::TreeRename(c.clone()), DialogResult::Text("c2.txt".into()));
        app.workspace.generation += 1;
        handle_next(&mut app, fs_op_done).await;
        assert_eq!(snapshots(&app, &root.join("c2.txt"))[0].0, format!("Renamed from {}", c.display()));
        assert_eq!(app.history_panel.target(), Some(root.join("c2.txt").as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_as_starts_a_new_history_that_names_the_old_path() {
        let (mut app, dir) = test_app("history-save-as");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let c = load(&mut app, "c.txt", "c\n");
        type_text(&mut app, "1");
        app.perform(Action::Editor(EditorAction::Save));
        type_text(&mut app, "2");
        app.perform(Action::Editor(EditorAction::SaveAs));
        let d = app.dialog.take().expect("the Save As prompt");
        app.finish_dialog(d.id, DialogResult::Text("c2.txt".into()));
        let c2 = root.join("c2.txt");
        assert_eq!(std::fs::read_to_string(&c2).unwrap(), "12c\n");
        let from = format!("Saved As from {}", c.display());
        assert_eq!(snapshots(&app, &c2), pairs(&[(&from, "12c\n")]), "nothing carried");
        assert_eq!(snapshots(&app, &c), pairs(&[("Saved", "1c\n")]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_label_names_the_tab_text_and_removal_asks_first() {
        let (mut app, dir) = test_app("history-label");
        with_history(&mut app);
        let a = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        let last = |app: &App| app.notifications.last().map(|n| n.text.clone()).unwrap_or_default();
        let dialog = |app: &mut App| app.dialog.take().expect("a dialog").id;
        app.perform(Action::App(AppAction::PutLabel));
        assert_eq!(dialog(&mut app), DialogId::PutLabel(a.clone()));
        // No name: nothing stored.
        app.finish_dialog(DialogId::PutLabel(a.clone()), DialogResult::Text("   ".into()));
        assert_eq!(last(&app), "Put Label: a label needs a name");
        assert!(snapshots(&app, &a).is_empty());
        // The tab's text, unsaved edits included.
        app.finish_dialog(DialogId::PutLabel(a.clone()), DialogResult::Text("  before refactor ".into()));
        handle_next(&mut app, label_msg).await;
        assert_eq!(last(&app), "Put label \"before refactor\" on a.txt");
        assert_eq!(snapshots(&app, &a), pairs(&[("Label: before refactor", "x one\n")]));
        // A save of that text adds no row.
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(snapshots(&app, &a).len(), 1);

        // In the window: Put Label asks for the file shown, and the new row
        // shows up once stored.
        app.show_local_history(a.clone());
        handle_next(&mut app, list_of(&a)).await;
        assert_eq!(app.history_panel.labels(), ["Label: before refactor"]);
        history(&mut app, HistoryAction::PutLabel);
        assert_eq!(dialog(&mut app), DialogId::PutLabel(a.clone()));
        app.finish_dialog(DialogId::PutLabel(a.clone()), DialogResult::Text("second".into()));
        handle_next(&mut app, label_msg).await;
        handle_next(&mut app, list_of(&a)).await;
        assert_eq!(app.history_panel.labels(), ["Label: second", "Label: before refactor"]);
        assert_eq!(app.history_panel.selected_label(), Some("Label: before refactor"), "the selection stays");

        // Delete on a label asks first; Cancel keeps it.
        history(&mut app, HistoryAction::RemoveLabel);
        let id = dialog(&mut app);
        assert!(
            matches!(&id, DialogId::RemoveLabel { path, entry } if *path == a && entry.detail.as_deref() == Some("before refactor")),
            "{id:?}"
        );
        app.finish_dialog(id, DialogResult::Button(1));
        assert_eq!(snapshots(&app, &a).len(), 2);
        assert!(app.focus.is(PanelId::LocalHistory));
        // Remove: the row goes in place.
        history(&mut app, HistoryAction::RemoveLabel);
        let id = dialog(&mut app);
        app.finish_dialog(id, DialogResult::Button(0));
        handle_next(&mut app, label_msg).await;
        assert_eq!(last(&app), "Removed label \"before refactor\"");
        handle_next(&mut app, list_of(&a)).await;
        assert_eq!(app.history_panel.labels(), ["Label: second"]);
        assert_eq!(snapshots(&app, &a).len(), 1);

        // Delete on a snapshot that is not a label only says why not.
        type_text(&mut app, "y ");
        app.perform(Action::Editor(EditorAction::Save));
        handle_next(&mut app, stored_msg).await;
        handle_next(&mut app, list_of(&a)).await;
        history(&mut app, HistoryAction::Home);
        assert_eq!(app.history_panel.selected_label(), Some("Saved"));
        history(&mut app, HistoryAction::RemoveLabel);
        assert!(app.dialog.is_none());
        assert!(last(&app).starts_with("Only labels can be removed"), "{}", last(&app));

        // A long name is cut.
        app.finish_dialog(DialogId::PutLabel(a.clone()), DialogResult::Text("n".repeat(150)));
        handle_next(&mut app, label_msg).await;
        assert_eq!(snapshots(&app, &a)[0].0, format!("Label: {}", "n".repeat(MAX_LABEL_CHARS)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_label_needs_local_history_and_a_file() {
        let (mut app, dir) = test_app("history-label-target");
        app.perform(Action::App(AppAction::PutLabel));
        assert!(app.dialog.is_none());
        assert_eq!(app.notifications.last().map(|n| n.text.as_str()), Some(HISTORY_OFF));
        with_history(&mut app);
        app.perform(Action::App(AppAction::PutLabel));
        assert!(app.dialog.is_none());
        assert_eq!(app.notifications.last().map(|n| n.text.as_str()), Some("No file selected"));
        app.focus.set(PanelId::Project);
        std::fs::create_dir_all(app.workspace.root.join("sub")).unwrap();
        app.project.select_path(&app.workspace.root.join("sub"));
        app.perform(Action::App(AppAction::PutLabel));
        assert!(app.dialog.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_label_outlives_keep_days() {
        let (mut app, dir) = test_app("history-label-keep");
        with_history(&mut app);
        let a = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        app.perform(Action::Editor(EditorAction::Save));
        app.finish_dialog(DialogId::PutLabel(a.clone()), DialogResult::Text("keep".into()));
        handle_next(&mut app, label_msg).await;
        assert_eq!(snapshots(&app, &a).len(), 2);
        std::thread::sleep(Duration::from_millis(5));
        let mut config = app.config.clone();
        config.local_history.keep_days = 0;
        app.apply_config(config);
        assert_eq!(snapshots(&app, &a), pairs(&[("Label: keep", "x one\n")]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_history_needs_a_file() {
        let (mut app, dir) = test_app("history-target");
        app.perform(Action::App(AppAction::LocalHistory));
        assert!(!app.layout.is_visible(ToolWindowId::LocalHistory));
        assert_eq!(app.notifications.last().map(|n| n.text.as_str()), Some("No file selected"));
        app.focus.set(PanelId::Project);
        std::fs::create_dir_all(app.workspace.root.join("sub")).unwrap();
        app.project.select_path(&app.workspace.root.join("sub"));
        app.perform(Action::App(AppAction::LocalHistory));
        assert!(!app.layout.is_visible(ToolWindowId::LocalHistory));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_window_opened_empty_takes_the_active_file() {
        let (mut app, dir) = test_app("history-toggle");
        with_history(&mut app);
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        assert!(app.layout.is_visible(ToolWindowId::LocalHistory));
        assert_eq!(app.history_panel.target(), None);
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        assert!(!app.layout.is_visible(ToolWindowId::LocalHistory));
        let a = load(&mut app, "a.txt", "one\n");
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        assert_eq!(app.history_panel.target(), Some(a.as_path()));
        handle_next(&mut app, list_of(&a)).await;
        // Another tab does not move it.
        let b = load(&mut app, "b.txt", "two\n");
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        assert_eq!(app.history_panel.target(), Some(a.as_path()));
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
