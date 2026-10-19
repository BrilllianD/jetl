//! Local history in the app: the Local History window (`history/panel.rs`)
//! and its requests (the list, the project view's scan, the diff of a
//! snapshot against the current text, reverting to a snapshot, labels: Put
//! Label and removal), the window following Project tree renames and root
//! switches, snapshots of files on disk before they are overwritten, and
//! periodic snapshots of unsaved edits (`history/` holds the store and its
//! worker).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{App, Ctx};
use crate::editor::buffer::BufferId;
use crate::event::{AppMsg, Level};
use crate::git::DiffKey;
use crate::history::panel::{LocalHistoryPanel, View};
use crate::history::{Capture, CaptureText, ChangesRevert, Entry, HistoryMsg, Kind, MAX_LABEL_CHARS, Request};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::{PanelId, ToolWindowId};

/// Why Put Label does nothing.
const HISTORY_OFF: &str = "Local history is off ([local_history] enabled = false)";

/// A revert waiting for its file's tab to open (`App::pending_revert`).
#[derive(Debug)]
pub(crate) enum PendingRevert {
    /// The whole snapshot.
    Text(String),
    /// The whole text of a label's member: the focus stays, no message of
    /// its own (`App::revert_label` toasts once).
    Member(String),
    /// Changes of the window's diff.
    Changes(Box<ChangesRevert>),
}

impl App {
    /// `editor.local_history`: the Local History window on the selected
    /// tree entry when the tree has focus, else on the active file; a folder
    /// shows its files with history, and so does the root with no file.
    pub(super) fn local_history(&mut self) {
        match self.selected_or_active() {
            Some(p) if !p.is_dir() => self.show_local_history(p),
            Some(dir) => self.show_local_history_dir(dir),
            None => self.show_local_history_dir(self.workspace.root.clone()),
        }
    }

    /// The selected tree entry when the tree has focus, else the active file.
    fn selected_or_active(&self) -> Option<PathBuf> {
        if self.focus.is(PanelId::Project) {
            self.project.selected_row().map(|r| r.path.clone())
        } else {
            self.editor.active_path().map(Path::to_path_buf)
        }
    }

    /// The file or folder Put Label works on (`selected_or_active`); `None`,
    /// with a notification, when there is none.
    fn history_target(&mut self) -> Option<PathBuf> {
        let Some(path) = self.selected_or_active() else {
            self.notify(Level::Info, "No file selected");
            return None;
        };
        Some(path)
    }

    /// Run `f` on the Local History window with a context, then apply what
    /// it asked for.
    fn with_history_panel(&mut self, f: impl FnOnce(&mut LocalHistoryPanel, &mut Ctx)) {
        let mut cx = ctx!(self);
        f(&mut self.history_panel, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// Show and focus the Local History window's project view on folder
    /// `dir`: its files with history, from a scan (`HistoryMsg::Recent`).
    pub(crate) fn show_local_history_dir(&mut self, dir: PathBuf) {
        self.layout.show(ToolWindowId::LocalHistory);
        self.focus.set(PanelId::LocalHistory);
        self.with_history_panel(|p, cx| p.show_project(dir, cx));
    }

    /// Scan the project view again while the window shows it on a folder
    /// holding `path` (a snapshot stored, a label, a tree delete); scans are
    /// coalesced by the panel.
    pub(super) fn history_files_changed(&mut self, path: &Path) {
        if self.layout.is_visible(ToolWindowId::LocalHistory) && self.history_panel.project_shows(path) {
            self.with_history_panel(LocalHistoryPanel::rescan);
        }
    }

    /// The root switched: the project view lists the new root's files.
    pub(crate) fn history_root_changed(&mut self) {
        let root = self.workspace.root.clone();
        if self.history_panel.reroot(&root) && self.layout.is_visible(ToolWindowId::LocalHistory) {
            self.with_history_panel(LocalHistoryPanel::rescan);
        }
    }

    /// Show and focus the Local History window on `path`; its snapshots
    /// are asked for and arrive as `HistoryMsg::List`.
    pub(crate) fn show_local_history(&mut self, path: PathBuf) {
        self.layout.show(ToolWindowId::LocalHistory);
        self.focus.set(PanelId::LocalHistory);
        self.history_panel.set_target(path.clone(), &self.workspace.root);
        self.history.list(&path);
    }

    /// The window opened: the project view is scanned again; with nothing
    /// shown yet, the active file, else the project's files.
    pub(super) fn history_window_opened(&mut self) {
        if self.history_panel.view() == View::Project {
            return self.with_history_panel(LocalHistoryPanel::rescan);
        }
        if self.history_panel.target().is_some() {
            return;
        }
        match self.editor.active_path().map(Path::to_path_buf) {
            Some(path) => {
                self.history_panel.set_target(path.clone(), &self.workspace.root);
                self.history.list(&path);
            }
            None => {
                let root = self.workspace.root.clone();
                self.with_history_panel(|p, cx| p.show_project(root, cx));
            }
        }
    }

    /// A Project tree rename succeeded: carry the history (queued before
    /// the tabs follow, so snapshots of the old path queued until now are
    /// carried), and move the Local History window along, listing again
    /// after the carry.
    pub(super) fn history_renamed(&mut self, from: &Path, to: &Path) {
        self.history.rename(from, to);
        self.history_retarget(from, to);
    }

    /// Files moved by something other than the Project tree (git renames
    /// after an operation): carry their history. The window follows when the
    /// worker answers `HistoryMsg::Moved`.
    pub(super) fn history_moved(&mut self, pairs: Vec<(PathBuf, PathBuf)>) {
        self.history.moved(pairs);
    }

    /// Move the Local History window along with a carried `from` -> `to`,
    /// listing again after the carry.
    fn history_retarget(&mut self, from: &Path, to: &Path) {
        if let Some(path) = self.history_panel.retarget(from, to, &self.workspace.root) {
            self.history.list(&path);
        }
        if self.history_panel.renamed(from, to, &self.workspace.root) && self.layout.is_visible(ToolWindowId::LocalHistory) {
            self.with_history_panel(LocalHistoryPanel::rescan);
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
        let of = if path == self.workspace.root {
            "the project".to_string()
        } else if path.is_dir() {
            format!("{rel}/")
        } else {
            rel
        };
        self.dialog = Some(Dialog::input(DialogId::PutLabel(path), "Put Label", &format!("Label for {of}:"), "", "Put"));
    }

    /// The Put Label dialog's name: trimmed, cut to [`MAX_LABEL_CHARS`]; the
    /// open tab's text (unsaved edits included), else the file on disk. On a
    /// folder: its open tabs' text, and the worker adds the other files.
    pub(super) fn finish_put_label(&mut self, path: PathBuf, text: &str) {
        let name = text.trim().chars().take(MAX_LABEL_CHARS).collect::<String>().trim_end().to_string();
        if name.is_empty() {
            return self.notify(Level::Warn, "Put Label: a label needs a name");
        }
        let queued = if path.is_dir() {
            let open = self
                .editor
                .buffers
                .iter()
                .filter_map(|b| b.path.as_ref().filter(|p| p.starts_with(&path)).map(|p| (p.clone(), CaptureText::Rope(b.rope.clone()))))
                .collect();
            self.history.put_label_dir(path, open, name, self.workspace.root.clone())
        } else {
            let tab = self.editor.index_of(&path).map(|i| CaptureText::Rope(self.editor.buffers[i].rope.clone()));
            self.history.put_label(path, tab, name)
        };
        if !queued {
            // Turned off while the dialog was open.
            self.notify(Level::Info, HISTORY_OFF);
        }
    }

    /// Revert on a label row of the Local History window: ask first.
    fn ask_revert_label(&mut self, at: u64, name: String, members: Vec<(PathBuf, Entry)>) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        let n = members.len();
        let msg = format!("Revert {n} file{} to label \"{name}\"?\nThe edits stay unsaved in their tabs.", if n == 1 { "" } else { "s" });
        self.dialog = Some(Dialog::confirm(DialogId::RevertLabel { at, name, members }, "Revert to Label", &msg, &["Revert", "Cancel"]));
    }

    /// Remove Label in the Local History window: ask first.
    fn ask_remove_label(&mut self, name: String, entries: Vec<(PathBuf, Entry)>) {
        let of = match entries.as_slice() {
            [(path, _)] => crate::ui::palette::display_path(path, &self.workspace.root),
            many => format!("{} files", many.len()),
        };
        let msg = format!("Remove label \"{name}\" from the local history of {of}?\nIts text goes too unless another snapshot holds it.");
        self.dialog = Some(Dialog::confirm(DialogId::RemoveLabel { name, entries }, "Remove Label", &msg, &["Remove", "Cancel"]));
    }

    /// The remove dialog's answer; the window lists again once the worker
    /// reports (`HistoryMsg::LabelRemoved`).
    pub(super) fn finish_remove_label(&mut self, name: String, entries: Vec<(PathBuf, Entry)>, remove: bool) {
        if remove {
            self.history.remove_label(name, entries);
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
            Request::Recent(scope) => self.history.recent(&scope),
            Request::Diff { key, seq } => self.load_local_diff(key, seq),
            Request::Revert { path, id } => self.history.blob(path, id),
            Request::RevertChanges(rev) => self.revert_changes(*rev),
            Request::PutLabel(path) => self.ask_put_label(path),
            Request::RemoveLabel { name, entries } => self.ask_remove_label(name, entries),
            Request::RevertLabel { at, name, members } => self.ask_revert_label(at, name, members),
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
            HistoryMsg::Recent { scope, files } => {
                let root = self.workspace.root.clone();
                self.with_history_panel(|p, cx| p.set_files(&scope, files, &root, crate::ui::event_log::local_offset, cx));
            }
            // A snapshot of the file shown landed (a save, the unsaved
            // timer, a git operation): list again (or scan).
            HistoryMsg::Stored { path } => {
                self.relist_if_shown(&path);
                self.history_files_changed(&path);
            }
            HistoryMsg::Diff { key, seq, result } => {
                self.history_panel.set_diff(&key, seq, result);
                // The build that held the others back is over: start the
                // newest one asked for meanwhile.
                if self.local_diff.done() {
                    if let Some((k, s)) = self.local_diff_next.take() {
                        self.start_local_diff(k, s);
                    }
                } else {
                    self.local_diff_next = None;
                }
            }
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
            HistoryMsg::Blobs { at, name, items } => self.revert_label(at, name, items),
            HistoryMsg::Labelled { path, name, result } => match result {
                Ok(n) => {
                    let rel = crate::ui::palette::display_path(&path, &self.workspace.root);
                    let dir = path.is_dir();
                    let of = if path == self.workspace.root { "the project".to_string() } else { format!("{rel}/") };
                    if !dir {
                        self.notify(Level::Info, format!("Put label \"{name}\" on {rel}"));
                    } else if n == 0 {
                        self.notify(Level::Info, format!("Put Label: no files to label under {of}"));
                    } else {
                        self.notify(Level::Info, format!("Put label \"{name}\" on {of} ({n} file{})", if n == 1 { "" } else { "s" }));
                    }
                    self.relist_if_shown(&path);
                    if dir && self.history_panel.view() == View::Project && self.layout.is_visible(ToolWindowId::LocalHistory) {
                        self.with_history_panel(LocalHistoryPanel::rescan);
                    } else {
                        self.history_files_changed(&path);
                    }
                }
                Err(e) => self.notify(Level::Warn, format!("Put Label: {e}")),
            },
            HistoryMsg::Moved { pairs } => {
                for (from, to) in &pairs {
                    self.history_retarget(from, to);
                }
            }
            HistoryMsg::LabelRemoved { paths, name, result } => {
                match result {
                    Ok(0) => self.notify(Level::Info, format!("Label \"{name}\" was already gone")),
                    Ok(1) => self.notify(Level::Info, format!("Removed label \"{name}\"")),
                    Ok(n) => self.notify(Level::Info, format!("Removed label \"{name}\" from {n} files")),
                    Err(e) => self.notify(Level::Warn, format!("Remove label: {e}")),
                }
                for path in &paths {
                    self.relist_if_shown(path);
                    self.history_files_changed(path);
                }
            }
        }
    }

    /// The diff of a snapshot against the open buffer (a rope clone) or the
    /// file on disk (a missing file is empty), built off the main thread;
    /// answered as `HistoryMsg::Diff` with the window's `seq`.
    pub(super) fn load_local_diff(&mut self, key: DiffKey, seq: u64) {
        if self.local_diff.request() {
            self.start_local_diff(key, seq);
        } else {
            // One build at a time: the newest request waits for it.
            self.local_diff_next = Some((key, seq));
        }
    }

    /// The tick's part: the diff compares a snapshot with an open tab that
    /// has changed since (the diff was built from another `version`); ask
    /// again, at most every half second; nothing while a request waits for
    /// the build in flight (it starts when that build lands). Scroll and
    /// block stay (same key).
    pub(super) fn history_diff_follow(&mut self, now: Instant) {
        if self.local_diff_next.is_some() {
            return;
        }
        let Some((path, v, at)) = &self.local_diff_follow else { return };
        if !self.layout.is_visible(ToolWindowId::LocalHistory) || now.duration_since(*at) < Duration::from_millis(500) {
            return;
        }
        if !matches!(self.history_panel.diff_state().0, Some(DiffKey::Local { path: p, .. }) if p == path) {
            return;
        }
        if self.editor.index_of(path).is_some_and(|i| self.editor.buffers[i].version != *v) {
            self.with_history_panel(|p, cx| p.reload_diff(cx));
        }
    }

    fn start_local_diff(&mut self, key: DiffKey, seq: u64) {
        let DiffKey::Local { path, id, .. } = &key else { return };
        let tab = self.editor.index_of(path);
        self.local_diff_follow = tab.map(|i| (path.clone(), self.editor.buffers[i].version, Instant::now()));
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
        self.revert_to_with(path, text, true);
    }

    /// `revert_to`; `focus` false leaves the focus where it is (one file of
    /// a label revert).
    fn revert_to_with(&mut self, path: PathBuf, text: String, focus: bool) {
        let quiet = !focus;
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        if self.editor.index_of(&path).is_some() {
            return self.apply_revert_with(&path, &text, quiet);
        }
        if path.exists() {
            let pending = if focus { PendingRevert::Text(text) } else { PendingRevert::Member(text) };
            self.pending_revert.insert(path.clone(), pending);
            return self.open_file(path, focus);
        }
        self.recreate_parent(&path);
        self.editor.open_text(path.clone(), "", false, &self.theme);
        if focus {
            self.editor.enforce_tab_limit();
        }
        self.apply_revert_with(&path, &text, quiet);
    }

    /// The texts of a label arrived: every member goes back to its text, one
    /// undoable edit per tab, unsaved; the focus stays in the window. Members
    /// that are not UTF-8 or whose blob is gone are skipped and named; queued
    /// members that fail to open are reported afterwards
    /// (`pending_revert_failed`).
    fn revert_label(&mut self, at: u64, name: String, items: Vec<(PathBuf, Result<Vec<u8>, String>)>) {
        if !self.history_panel.shows_label(at, &name) {
            return tracing::debug!("dropping the texts of label {name}: no longer shown");
        }
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        let (mut done, mut skipped) = (0, Vec::new());
        for (path, bytes) in items {
            match bytes.map_err(|e| e.to_string()).and_then(|b| String::from_utf8(b).map_err(|_| "not valid UTF-8".to_string())) {
                Ok(text) => {
                    self.revert_member(path, text);
                    done += 1;
                }
                Err(e) => {
                    tracing::debug!("label revert skips {}: {e}", path.display());
                    skipped.push(crate::ui::palette::display_path(&path, &self.workspace.root));
                }
            }
        }
        self.editor.enforce_tab_limit();
        self.focus.set(PanelId::LocalHistory);
        let mut msg =
            format!("Reverted {done} file{} to label \"{name}\" (unsaved; Ctrl+Z in each tab undoes)", if done == 1 { "" } else { "s" });
        if !skipped.is_empty() {
            msg.push_str(&format!("; skipped {} (no text to restore): {}", skipped.len(), skipped.join(", ")));
        }
        self.notify(if skipped.is_empty() { Level::Info } else { Level::Warn }, msg);
    }

    /// A queued revert whose file did not open: its entry goes, and a label
    /// member (`PendingRevert::Member`) is named in a warning, since the
    /// label's toast already counted it.
    pub(super) fn pending_revert_failed(&mut self, path: &Path) {
        if let Some(PendingRevert::Member(_)) = self.pending_revert.remove(path) {
            let rel = crate::ui::palette::display_path(path, &self.workspace.root);
            self.notify(Level::Warn, format!("Revert to label: {rel} was not reverted (the file did not open)"));
        }
    }

    /// The root is about to switch: every queued revert goes, and each label
    /// member (`PendingRevert::Member`) is named in a warning, since the
    /// label's toast already counted it. Called before the root changes, so
    /// the names are relative to the old root.
    pub(crate) fn pending_reverts_dropped(&mut self) {
        let mut members: Vec<PathBuf> =
            self.pending_revert.drain().filter_map(|(path, pending)| matches!(pending, PendingRevert::Member(_)).then_some(path)).collect();
        members.sort();
        for path in members {
            let rel = crate::ui::palette::display_path(&path, &self.workspace.root);
            self.notify(Level::Warn, format!("Revert to label: {rel} was not reverted (the project root changed)"));
        }
    }

    /// One file of a label revert: `revert_to` without the focus.
    fn revert_member(&mut self, path: PathBuf, text: String) {
        self.revert_to_with(path, text, false);
    }

    /// Put `text` into `path`'s open tab as one undoable change, show the
    /// tab and compare again.
    pub(super) fn apply_revert(&mut self, path: &Path, text: &str) {
        self.apply_revert_with(path, text, false);
    }

    /// `apply_revert`; `quiet` keeps the focus and the message for the
    /// caller (a label revert).
    pub(super) fn apply_revert_with(&mut self, path: &Path, text: &str, quiet: bool) {
        let Some(i) = self.editor.index_of(path) else { return };
        let mut cx = ctx!(self);
        self.editor.activate(i, &mut cx);
        let mut fx = cx.effects;
        let name = self.editor.buffers[i].name.clone();
        let changed = self.editor.restore_text(i, text);
        if !quiet {
            self.focus.set(PanelId::Editor);
            let msg = if changed {
                format!("Reverted {name} to {} (Ctrl+Z undoes)", self.history_panel.selected_title())
            } else {
                format!("{name} already has the text of that snapshot")
            };
            self.notify(Level::Info, msg);
        }
        let mut cx = ctx!(self);
        self.history_panel.reload_diff(&mut cx);
        fx.extend(cx.effects);
        self.apply_effects(fx);
    }

    /// Changes of the window's diff to put back: into the open tab, else
    /// once the file opens (`pending_revert`; the focus stays); a file
    /// deleted meanwhile gets an empty tab with its path.
    fn revert_changes(&mut self, rev: ChangesRevert) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        let path = rev.path.clone();
        if self.editor.index_of(&path).is_some() {
            return self.apply_changes_revert(rev);
        }
        if path.exists() {
            self.pending_revert.insert(path.clone(), PendingRevert::Changes(Box::new(rev)));
            return self.open_file(path, false);
        }
        self.recreate_parent(&path);
        self.editor.open_text(path, "", false, &self.theme);
        self.editor.enforce_tab_limit();
        self.apply_changes_revert(rev);
    }

    /// A deleted file comes back in a tab: its folder too (it may have been
    /// deleted with it), so the save can write it.
    fn recreate_parent(&mut self, path: &Path) {
        let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty() && !d.exists()) else { return };
        match std::fs::create_dir_all(dir) {
            Ok(()) => self.reload_parents(path),
            Err(e) => self.notify(Level::Warn, format!("Cannot create {}: {e}", dir.display())),
        }
    }

    /// Apply changes of the window's diff to their file's open tab as one
    /// undoable edit, only while its text is still the one the diff
    /// compared; show the tab (the focus stays) and compare again.
    pub(super) fn apply_changes_revert(&mut self, rev: ChangesRevert) {
        let Some(i) = self.editor.index_of(&rev.path) else { return };
        let name = self.editor.buffers[i].name.clone();
        let n = rev.changes.len();
        if self.editor.buffers[i].read_only {
            self.notify(Level::Info, format!("{name} is read-only; not reverted"));
        } else if self.editor.restore_changes(i, &rev.base, rev.changes) {
            let mut cx = ctx!(self);
            self.editor.activate(i, &mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
            let what = if n == 1 { "1 change".to_string() } else { format!("{n} changes") };
            self.notify(Level::Info, format!("Reverted {what} in {name} to {} (Ctrl+Z in the editor undoes)", rev.title));
        } else {
            self.notify(Level::Warn, format!("{name} changed since the diff was made; compared again: revert once it shows"));
        }
        let mut cx = ctx!(self);
        self.history_panel.reload_diff(&mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// The tick's part: when the hourly prune is due, queue it and re-arm.
    /// Does not check `enabled()`, like the startup prune: a disabled store
    /// still ages out.
    pub(super) fn history_prune_tick(&mut self, now: Instant) {
        if self.prune_due.is_some_and(|due| now >= due) {
            self.history.prune();
            self.prune_due = Some(now + crate::history::PRUNE_EVERY);
        }
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

    /// Snapshot the unsaved edits of the buffers `ids` (all with `None`) just
    /// before a "Don't Save" discards them. Needs local history on, not the
    /// unsaved timer.
    pub(super) fn snapshot_discarded(&mut self, ids: Option<&[BufferId]>) {
        if !self.history.enabled() {
            return;
        }
        let max = self.config.local_history.max_file_bytes();
        for c in self.editor.discard_captures(ids, max) {
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
    use crate::git::{GitMsg, GitOp};
    use crate::history::store;
    use crate::ui::dialog::{DialogId, DialogResult};

    /// `path`'s snapshots once the worker is done, newest first: label and
    /// text.
    pub(crate) fn snapshots(app: &App, path: &Path) -> Vec<(String, String)> {
        assert!(app.history.flush(Duration::from_secs(5)), "the local history worker did not drain its queue");
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
        diff_landed(&mut app).await;
        assert!(diff_shows(&app, &path, "two three one\n"));
        history(&mut app, HistoryAction::Down);
        diff_landed(&mut app).await;
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
        diff_landed(&mut app).await;
        assert!(app.history_panel.diff_state().1);
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(app.editor.active_buffer().unwrap().text(), "two three one\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Store a snapshot of `path` holding `text`.
    fn record(app: &App, path: &Path, text: &str) {
        let r = store::record(app.history.dir(), path, text.as_bytes(), Kind::Save, None, crate::history::now_ms(), 1 << 20).unwrap();
        assert!(matches!(r, store::Recorded::Added(_)));
    }

    /// Handle diff answers until the window's newest one has landed (an
    /// older request's answer may come first).
    async fn diff_landed(app: &mut App) {
        while app.history_panel.diff_loading() {
            handle_next(app, diff_msg).await;
        }
    }

    /// The window on `path`, its list and the diff of the newest snapshot
    /// loaded, the diff focused.
    async fn window_with_diff(app: &mut App, path: &Path) {
        app.show_local_history(path.to_path_buf());
        handle_next(app, list_of(path)).await;
        diff_landed(app).await;
        history(app, HistoryAction::Right);
    }

    fn last_note(app: &App) -> String {
        app.notifications.last().map(|n| n.text.clone()).unwrap_or_default()
    }

    #[tokio::test]
    async fn diff_builds_coalesce() {
        let (mut app, dir) = test_app("history-diff-coalesce");
        with_history(&mut app);
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "two ");
        app.perform(Action::Editor(EditorAction::Save));
        assert!(app.history.flush(Duration::from_secs(5)));
        window_with_diff(&mut app, &path).await;
        let before = app.local_diff.requests;
        type_text(&mut app, "x");
        // Two requests back to back: the first builds, the second waits.
        app.with_history_panel(|p, cx| p.reload_diff(cx));
        app.with_history_panel(|p, cx| p.reload_diff(cx));
        assert_eq!(app.local_diff.requests, before + 2);
        assert!(app.local_diff_next.is_some(), "the second request is held back");
        // The first answer is stale (older seq): not applied, the held one starts.
        handle_next(&mut app, diff_msg).await;
        assert!(app.history_panel.diff_loading());
        assert!(app.local_diff_next.is_none());
        handle_next(&mut app, diff_msg).await;
        assert!(!app.history_panel.diff_loading());
        assert!(app.history_panel.diff_new_text().contains('x'));
        let third = tokio::time::timeout(Duration::from_millis(300), async {
            while let Some(m) = app.bus_rx.recv().await {
                if diff_msg(&m) {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(third.is_err(), "no third build");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn diff_follows_edits() {
        let (mut app, dir) = test_app("history-diff-follows");
        with_history(&mut app);
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "two ");
        app.perform(Action::Editor(EditorAction::Save));
        assert!(app.history.flush(Duration::from_secs(5)));
        window_with_diff(&mut app, &path).await;
        assert!(!app.history_panel.diff_new_text().contains('x'));
        let later = Instant::now() + Duration::from_secs(1);
        // Nothing changed in the tab: no request.
        let n = app.local_diff.requests;
        app.history_diff_follow(later);
        assert_eq!(app.local_diff.requests, n);
        type_text(&mut app, "x");
        app.history_diff_follow(later);
        assert_eq!(app.local_diff.requests, n + 1);
        diff_landed(&mut app).await;
        assert!(app.history_panel.diff_new_text().contains('x'));
        // The new diff was built from the edited tab: no further request.
        app.history_diff_follow(later + Duration::from_secs(1));
        assert_eq!(app.local_diff.requests, n + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn diff_follow_waits_while_a_request_is_held() {
        let (mut app, dir) = test_app("history-diff-held");
        with_history(&mut app);
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "two ");
        app.perform(Action::Editor(EditorAction::Save));
        assert!(app.history.flush(Duration::from_secs(5)));
        window_with_diff(&mut app, &path).await;
        let later = Instant::now() + Duration::from_secs(1);
        type_text(&mut app, "x");
        app.history_diff_follow(later);
        app.with_history_panel(|p, cx| p.reload_diff(cx));
        assert!(app.local_diff_next.is_some(), "a request is held");
        let n = app.local_diff.requests;
        type_text(&mut app, "y");
        app.history_diff_follow(later + Duration::from_secs(1));
        assert_eq!(app.local_diff.requests, n, "no request while one is held");
        handle_next(&mut app, diff_msg).await;
        handle_next(&mut app, diff_msg).await;
        assert!(app.history_panel.diff_new_text().contains("xy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn selected_changes_revert_as_one_undoable_edit() {
        let (mut app, dir) = test_app("history-changes");
        with_history(&mut app);
        let path = app.workspace.root.join("a.txt");
        record(&app, &path, "a\nb\nc\nd\ne\n");
        load(&mut app, "a.txt", "A\nb\nC\nd\nE\n");
        window_with_diff(&mut app, &path).await;
        assert_eq!(app.history_panel.changes(), (3, 0..1));
        // The second change alone; the focus stays in the window.
        history(&mut app, HistoryAction::NextChange);
        history(&mut app, HistoryAction::RevertChanges);
        assert_eq!(app.editor.active_buffer().unwrap().text(), "A\nb\nc\nd\nE\n");
        assert!(app.focus.is(PanelId::LocalHistory));
        let note = last_note(&app);
        assert!(note.starts_with("Reverted 1 change in a.txt to ") && note.ends_with("  Saved (Ctrl+Z in the editor undoes)"), "{note}");
        // Compared again: the change that followed is current.
        diff_landed(&mut app).await;
        assert_eq!(app.history_panel.changes(), (2, 1..2));
        // Every change left: Ctrl+A, then the revert.
        history(&mut app, HistoryAction::SelectAllChanges);
        history(&mut app, HistoryAction::RevertChanges);
        assert_eq!(last_note(&app).split(" to ").next(), Some("Reverted 2 changes in a.txt"));
        let b = app.editor.active_buffer().unwrap();
        assert_eq!((b.text(), b.dirty()), ("a\nb\nc\nd\ne\n".to_string(), true));
        diff_landed(&mut app).await;
        assert_eq!(app.history_panel.changes(), (0, 0..0));
        // One undo step each.
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(app.editor.active_buffer().unwrap().text(), "A\nb\nc\nd\nE\n");
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(app.editor.active_buffer().unwrap().text(), "A\nb\nC\nd\nE\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nb\nC\nd\nE\n", "nothing written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn revert_selected_lines_reverts_only_those() {
        let (mut app, dir) = test_app("history-lines");
        with_history(&mut app);
        let path = app.workspace.root.join("a.txt");
        record(&app, &path, "a\nb\nc\nd\n");
        load(&mut app, "a.txt", "a\nB\nC\nD\n");
        window_with_diff(&mut app, &path).await;
        assert_eq!(app.history_panel.changes(), (1, 0..1));
        // The second changed row.
        history(&mut app, HistoryAction::LineDown);
        history(&mut app, HistoryAction::LineDown);
        history(&mut app, HistoryAction::RevertChanges);
        assert_eq!(app.editor.active_buffer().unwrap().text(), "a\nB\nc\nD\n");
        // The line selection is gone after the revert.
        diff_landed(&mut app).await;
        assert_eq!(app.history_panel.changes(), (2, 0..1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn changes_made_for_another_text_are_refused_and_compared_again() {
        let (mut app, dir) = test_app("history-changes-stale");
        with_history(&mut app);
        let path = app.workspace.root.join("a.txt");
        record(&app, &path, "a\nb\n");
        load(&mut app, "a.txt", "A\nb\n");
        window_with_diff(&mut app, &path).await;
        // An edit after the diff was made.
        type_text(&mut app, "x");
        history(&mut app, HistoryAction::RevertChanges);
        assert_eq!(app.editor.active_buffer().unwrap().text(), "xA\nb\n");
        assert!(last_note(&app).starts_with("a.txt changed since the diff was made"), "{}", last_note(&app));
        // The new comparison reverts.
        diff_landed(&mut app).await;
        history(&mut app, HistoryAction::RevertChanges);
        assert_eq!(app.editor.active_buffer().unwrap().text(), "a\nb\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn changes_of_a_file_that_is_not_open_apply_once_it_loads() {
        let (mut app, dir) = test_app("history-changes-unopened");
        with_history(&mut app);
        let path = app.workspace.root.join("b.txt");
        std::fs::write(&path, "B\nx\nY\n").unwrap();
        record(&app, &path, "b\nx\ny\n");
        window_with_diff(&mut app, &path).await;
        assert_eq!(app.history_panel.changes(), (2, 0..1));
        history(&mut app, HistoryAction::RevertChanges);
        assert!(app.editor.index_of(&path).is_none());
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        let b = app.editor.active_buffer().unwrap();
        assert_eq!((b.text(), b.dirty()), ("b\nx\nY\n".to_string(), true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "B\nx\nY\n", "nothing written");
        assert!(app.focus.is(PanelId::LocalHistory));

        // The disk changed between the diff and the load: refused.
        let c = app.workspace.root.join("c.txt");
        std::fs::write(&c, "C\n").unwrap();
        record(&app, &c, "c\n");
        window_with_diff(&mut app, &c).await;
        std::fs::write(&c, "C2\n").unwrap();
        history(&mut app, HistoryAction::RevertChanges);
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_buffer().unwrap().text(), "C2\n");
        assert!(last_note(&app).starts_with("c.txt changed since the diff was made"), "{}", last_note(&app));
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
        diff_landed(&mut app).await;
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
    async fn hourly_prune_collects_orphan_blobs() {
        let (mut app, dir) = test_app("history-hourly-prune");
        with_history(&mut app);
        assert!(app.history.flush(Duration::from_secs(5)));
        let blob = store::write_blob_for_test(app.history.dir(), &"ab".repeat(20), b"orphan").unwrap();
        let old = std::time::SystemTime::now() - store::BLOB_GRACE - Duration::from_secs(60);
        std::fs::File::options().write(true).open(&blob).unwrap().set_modified(old).unwrap();
        app.prune_due = Some(Instant::now());
        let now = Instant::now();
        app.history_prune_tick(now);
        assert!(app.history.flush(Duration::from_secs(5)));
        assert!(!blob.exists(), "orphan collected");
        assert_eq!(app.prune_due, Some(now + crate::history::PRUNE_EVERY));
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

    const DISCARD: &str = "Before discarding unsaved edits";

    #[tokio::test]
    async fn dont_save_on_close_snapshots_the_edits() {
        let (mut app, dir) = test_app("history-discard-tab");
        with_history(&mut app);
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        let id = app.editor.buffers[app.editor.active().unwrap()].id;
        app.finish_dialog(DialogId::CloseTabDirty(id), DialogResult::Button(1));
        assert!(app.editor.index_of_id(id).is_none(), "closed");
        assert_eq!(snapshots(&app, &path), pairs(&[(DISCARD, "x one\n")]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dont_save_on_quit_snapshots_every_dirty_tab() {
        let (mut app, dir) = test_app("history-discard-quit");
        with_history(&mut app);
        let a = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        let b = load(&mut app, "b.txt", "two\n");
        type_text(&mut app, "y ");
        load(&mut app, "c.txt", "clean\n");
        app.finish_dialog(DialogId::QuitDirty, DialogResult::Button(1));
        assert!(app.should_quit);
        assert_eq!(snapshots(&app, &a), pairs(&[(DISCARD, "x one\n")]));
        assert_eq!(snapshots(&app, &b), pairs(&[(DISCARD, "y two\n")]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dont_save_relabels_an_unsaved_snapshot_of_the_same_text() {
        let (mut app, dir) = test_app("history-discard-relabel");
        let mut config = app.config.clone();
        config.local_history.snapshot_unsaved_secs = 30;
        app.apply_config(config);
        with_history(&mut app);
        let due = app.unsaved_due.expect("armed");
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        app.unsaved_snapshots(due);
        assert_eq!(snapshots(&app, &path), pairs(&[("Unsaved changes", "x one\n")]));
        let id = app.editor.buffers[app.editor.active().unwrap()].id;
        app.finish_dialog(DialogId::CloseTabDirty(id), DialogResult::Button(1));
        assert_eq!(snapshots(&app, &path), pairs(&[(DISCARD, "x one\n")]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dont_save_with_history_off_stores_nothing() {
        let (mut app, dir) = test_app("history-discard-off");
        let path = load(&mut app, "a.txt", "one\n");
        type_text(&mut app, "x ");
        app.finish_dialog(DialogId::QuitDirty, DialogResult::Button(1));
        assert!(app.should_quit);
        assert!(!app.history.enabled());
        assert!(store::list(app.history.dir(), &path).is_empty());
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
        app.run_fs_op(FsOp::Delete { path: root.join("d"), recursive: true });
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

    #[tokio::test]
    async fn git_checkout_rename_carries_history() {
        let (mut app, dir) = test_app("history-git-rename");
        with_history(&mut app);
        init_repo(&app, &[("a.txt", "one\ntwo\nthree\nfour\n")]);
        let root = app.workspace.root.clone();
        let (a, b) = (root.join("a.txt"), root.join("b.txt"));
        record(&app, &a, "one\ntwo\nthree\nfour\n");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(&root).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&["switch", "-qc", "dev"]);
        git(&["mv", "a.txt", "b.txt"]);
        git(&["commit", "-qm", "rename"]);
        git(&["switch", "-q", &main]);
        app.git.run_cli(GitOp::Switch("dev".into()), vec!["switch".into(), "dev".into()]);
        let op_done = |m: &AppMsg| matches!(m, AppMsg::Git(GitMsg::OpDone { .. }));
        handle_next(&mut app, op_done).await;
        assert!(b.exists() && !a.exists());
        // The window on the old path follows once the worker carried.
        app.show_local_history(a.clone());
        handle_next(&mut app, |m| matches!(m, AppMsg::LocalHistory(HistoryMsg::Moved { .. }))).await;
        assert_eq!(app.history_panel.target(), Some(b.as_path()));
        let renamed = format!("Renamed from {}", a.display());
        assert_eq!(snapshots(&app, &b), pairs(&[(&renamed, "one\ntwo\nthree\nfour\n"), ("Saved", "one\ntwo\nthree\nfour\n")]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watcher_rename_carries_history() {
        use crate::files::watch::Changes;
        let (mut app, dir) = test_app("history-watcher-rename");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let (a, b) = (root.join("a.txt"), root.join("b.txt"));
        std::fs::write(&a, "one\n").unwrap();
        record(&app, &a, "one\n");
        std::fs::rename(&a, &b).unwrap();
        let deliver = |app: &mut App| {
            let generation = app.workspace.generation;
            let changes = Changes { renames: vec![(a.clone(), b.clone())], ..Default::default() };
            app.handle_msg(AppMsg::FsChanged { generation, changes });
        };
        deliver(&mut app);
        let renamed = format!("Renamed from {}", a.display());
        assert_eq!(snapshots(&app, &b), pairs(&[(&renamed, "one\n"), ("Saved", "one\n")]));
        // The same report again (echo) adds no second marker.
        deliver(&mut app);
        assert_eq!(snapshots(&app, &b).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fs_op_done(m: &AppMsg) -> bool {
        matches!(m, AppMsg::FsOpDone { .. })
    }

    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn language_server_renames_and_deletes_carry_local_history() {
        use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
        use crate::lsp::{EditOrigin, sync};
        let (mut app, dir) = test_app("history-lsp-ops");
        with_history(&mut app);
        let mut servers = with_fake_servers(&mut app, incremental());
        let server = open_on_server(&mut app, &mut servers, "a.rs", "one\n").await;
        // An unchanged buffer is not saved: edit it first.
        type_text(&mut app, "x ");
        app.perform(Action::Editor(EditorAction::Save));
        let root = app.workspace.root.clone();
        let (a, b, c, e) = (root.join("a.rs"), root.join("b.rs"), root.join("c.rs"), root.join("e.rs"));
        std::fs::write(&c, "gone\n").unwrap();
        let uri = |p: &Path| sync::uri(p).unwrap().as_str().to_string();
        server.request(
            1,
            "workspace/applyEdit",
            serde_json::json!({"edit": {"documentChanges": [
                {"kind": "rename", "oldUri": uri(&a), "newUri": uri(&b)},
                {"kind": "delete", "uri": uri(&c)},
            ]}}),
        );
        handle_next(&mut app, is_lsp).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::EditsApplied { .. })).await;
        assert_eq!(snapshots(&app, &b)[0].0, format!("Renamed from {}", a.display()));
        assert_eq!(snapshots(&app, &c)[0], ("Before delete".to_string(), "gone\n".to_string()));
        // A previous root's report: only the history store follows.
        std::fs::rename(&b, &e).unwrap();
        app.handle_msg(AppMsg::EditsApplied {
            generation: app.workspace.generation + 1,
            origin: EditOrigin::CodeAction("x".into()),
            done: Default::default(),
            errors: vec![],
            ops: vec![FsOp::Rename { from: b.clone(), to: e.clone(), overwrite: false }],
            failed: None,
        });
        assert!(snapshots(&app, &e)[0].0.starts_with("Renamed from"));
        assert_eq!(app.editor.active_buffer().unwrap().path.as_deref(), Some(b.as_path()), "the tab is not retargeted");
        let _ = std::fs::remove_dir_all(&dir);
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
        diff_landed(&mut app).await;
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
            matches!(&id, DialogId::RemoveLabel { name, entries } if name == "before refactor" && entries.len() == 1 && entries[0].0 == a && entries[0].1.detail.as_deref() == Some("before refactor")),
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
        app.project.refresh();
        assert!(app.project.select_path(&app.workspace.root.join("sub")));
        app.perform(Action::App(AppAction::PutLabel));
        assert!(app.dialog.is_some(), "a folder takes a label");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_label_on_a_folder_labels_its_files() {
        let (mut app, dir) = test_app("history-label-dir");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let (a, b, c) = (src.join("a.txt"), src.join("b.txt"), root.join("outside.txt"));
        for (p, t) in [(&a, "disk a\n"), (&b, "disk b\n"), (&c, "out\n")] {
            std::fs::write(p, t).unwrap();
            record(&app, p, t);
        }
        // An open tab with unsaved edits under the folder, no history yet.
        let open = load(&mut app, "src/open.txt", "open\n");
        type_text(&mut app, "x ");
        app.focus.set(PanelId::Project);
        app.project.refresh();
        assert!(app.project.select_path(&src));
        app.perform(Action::App(AppAction::PutLabel));
        let dlg = app.dialog.take().expect("a dialog");
        assert_eq!(dlg.id, DialogId::PutLabel(src.clone()));
        assert!(dlg.message.contains("Label for src/:"));
        app.finish_dialog(DialogId::PutLabel(src.clone()), DialogResult::Text("v1".into()));
        handle_next(&mut app, label_msg).await;
        let last = app.notifications.last().map(|n| n.text.clone()).unwrap_or_default();
        assert_eq!(last, "Put label \"v1\" on src/ (3 files)");
        let label_of = |p: &Path| {
            let l: Vec<_> = store::list(app.history.dir(), p).into_iter().filter(|e| e.kind == Kind::Label).collect();
            assert_eq!(l.len(), 1, "{}", p.display());
            (l[0].label(), l[0].at, store::read_blob(app.history.dir(), &l[0].blob).unwrap())
        };
        let (la, lb, lo) = (label_of(&a), label_of(&b), label_of(&open));
        assert_eq!((la.0.as_str(), la.2.as_slice()), ("Label: v1", b"disk a\n".as_slice()));
        assert_eq!(lo.2, b"x open\n");
        assert!(la.1 == lb.1 && lb.1 == lo.1, "one time");
        assert!(store::list(app.history.dir(), &c).iter().all(|e| e.kind != Kind::Label), "outside the folder");
        // Nothing under a folder: said so.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        app.finish_dialog(DialogId::PutLabel(empty.clone()), DialogResult::Text("v2".into()));
        handle_next(&mut app, label_msg).await;
        let last = app.notifications.last().map(|n| n.text.clone()).unwrap_or_default();
        assert_eq!(last, "Put Label: no files to label under empty/");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_label_on_the_root_says_the_project() {
        let (mut app, dir) = test_app("history-label-root");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let a = root.join("a.txt");
        std::fs::write(&a, "a\n").unwrap();
        record(&app, &a, "a\n");
        app.ask_put_label(root.clone());
        let dlg = app.dialog.take().expect("a dialog");
        assert_eq!(dlg.id, DialogId::PutLabel(root.clone()));
        assert!(dlg.message.contains("Label for the project:"));
        app.finish_dialog(DialogId::PutLabel(root.clone()), DialogResult::Text("v1".into()));
        handle_next(&mut app, label_msg).await;
        let last = app.notifications.last().map(|n| n.text.clone()).unwrap_or_default();
        assert_eq!(last, "Put label \"v1\" on the project (1 file)");
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
    async fn a_folder_or_no_file_shows_the_project_view() {
        let (mut app, dir) = test_app("history-target");
        let root = app.workspace.root.clone();
        // No file: the project's files.
        app.perform(Action::App(AppAction::LocalHistory));
        assert!(app.layout.is_visible(ToolWindowId::LocalHistory));
        assert_eq!(app.history_panel.view(), View::Project);
        assert_eq!(app.history_panel.project_scope(), Some(root.as_path()));
        // A folder selected in the tree: its files.
        app.focus.set(PanelId::Project);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        app.project.refresh();
        assert!(app.project.select_path(&root.join("sub")));
        app.perform(Action::App(AppAction::LocalHistory));
        assert_eq!(app.history_panel.project_scope(), Some(root.join("sub").as_path()));
        assert!(app.focus.is(PanelId::LocalHistory));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scan of `scope`.
    fn scan_of(scope: &Path) -> impl Fn(&AppMsg) -> bool + '_ {
        move |m| matches!(m, AppMsg::LocalHistory(HistoryMsg::Recent { scope: s, .. }) if s == scope)
    }

    /// Write `rel` under the root and store a snapshot of it.
    fn saved(app: &App, rel: &str, text: &str) -> PathBuf {
        let path = app.workspace.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        record(app, &path, text);
        path
    }

    #[tokio::test]
    async fn the_project_view_lists_a_folders_files_and_drills_in() {
        let (mut app, dir) = test_app("history-project");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let a = saved(&app, "d/a.txt", "a\n");
        std::thread::sleep(Duration::from_millis(2));
        saved(&app, "d/sub/b.txt", "b\n");
        saved(&app, "x.txt", "x\n");
        // File > Local History… on a folder in the tree: its files only.
        app.project.refresh();
        app.focus.set(PanelId::Project);
        assert!(app.project.select_path(&root.join("d")));
        app.perform(Action::App(AppAction::LocalHistory));
        assert_eq!(app.history_panel.view(), View::Project);
        handle_next(&mut app, scan_of(&root.join("d"))).await;
        assert_eq!(app.history_panel.project_rows(), ["d/sub/b.txt · Saved", "d/a.txt · Saved"]);
        diff_landed(&mut app).await;
        assert!(diff_shows(&app, &root.join("d/sub/b.txt"), "b\n"));
        // Enter: the file's own snapshots; Backspace: back on its row.
        history(&mut app, HistoryAction::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!((app.history_panel.view(), app.history_panel.target()), (View::File, Some(a.as_path())));
        handle_next(&mut app, list_of(&a)).await;
        assert_eq!(app.history_panel.labels(), ["Saved"]);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.history_panel.view(), View::Project);
        handle_next(&mut app, scan_of(&root.join("d"))).await;
        assert!(app.history_panel.selected_title().ends_with("  Saved"));
        diff_landed(&mut app).await;
        assert!(diff_shows(&app, &a, "a\n"));
        // Live: a save of a file under it puts that file on top.
        let b = load(&mut app, "d/sub/b.txt", "b\n");
        app.focus.set(PanelId::LocalHistory);
        type_text(&mut app, "2");
        app.perform(Action::Editor(EditorAction::Save));
        handle_next(&mut app, stored_msg).await;
        handle_next(&mut app, scan_of(&root.join("d"))).await;
        assert_eq!(app.history_panel.project_rows()[0], "d/sub/b.txt · Saved");
        assert_eq!(snapshots(&app, &b).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_deleted_file_comes_back_from_the_project_view() {
        let (mut app, dir) = test_app("history-project-deleted");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let x = saved(&app, "d/x.txt", "x\n");
        app.show_local_history_dir(root.clone());
        handle_next(&mut app, scan_of(&root)).await;
        assert_eq!(app.history_panel.project_rows(), ["d/x.txt · Saved"]);
        // A tree delete of its folder: the row says so (nothing new stored).
        app.run_fs_op(FsOp::Delete { path: root.join("d"), recursive: true });
        handle_next(&mut app, fs_op_done).await;
        handle_next(&mut app, scan_of(&root)).await;
        assert_eq!(app.history_panel.project_rows(), ["d/x.txt · deleted"]);
        diff_landed(&mut app).await;
        assert!(diff_shows(&app, &x, "x\n"));
        // Alt+R: an unsaved tab with its text, the folder back; Save writes it.
        history(&mut app, HistoryAction::Revert);
        handle_next(&mut app, blob_msg).await;
        let b = app.editor.active_buffer().unwrap();
        assert_eq!((b.path.as_deref(), b.text(), b.dirty()), (Some(x.as_path()), "x\n".to_string(), true));
        assert!(!x.exists() && root.join("d").is_dir());
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(std::fs::read_to_string(&x).unwrap(), "x\n");
        // Its text is its last snapshot's (nothing stored), yet the row is back.
        handle_next(&mut app, scan_of(&root)).await;
        assert_eq!(app.history_panel.project_rows(), ["d/x.txt · Saved"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn blobs_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::LocalHistory(HistoryMsg::Blobs { .. }))
    }

    fn file_loaded(m: &AppMsg) -> bool {
        matches!(m, AppMsg::FileLoaded { .. })
    }

    #[tokio::test]
    async fn revert_label_restores_every_member() {
        let (mut app, dir) = test_app("history-revert-label");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let d = root.join("d");
        let closed = saved(&app, "d/closed.txt", "closed v1\n");
        let gone = saved(&app, "d/sub/gone.txt", "gone v1\n");
        let open = load(&mut app, "d/open.txt", "open v1\n");
        app.finish_dialog(DialogId::PutLabel(d.clone()), DialogResult::Text("v1".into()));
        handle_next(&mut app, label_msg).await;
        assert_eq!(last_note(&app), "Put label \"v1\" on d/ (3 files)");
        // Everything changes afterwards: an unsaved edit, a rewrite, a delete.
        type_text(&mut app, "edit ");
        std::fs::write(&closed, "closed v2\n").unwrap();
        std::fs::remove_dir_all(d.join("sub")).unwrap();
        app.show_local_history_dir(d.clone());
        handle_next(&mut app, scan_of(&d)).await;
        let rows = app.history_panel.project_rows();
        let label = rows.iter().position(|r| r.starts_with("Label")).unwrap_or_else(|| panic!("a label row in {rows:?}"));
        for _ in 0..label {
            history(&mut app, HistoryAction::Down);
        }
        history(&mut app, HistoryAction::Revert);
        let dlg = app.dialog.take().expect("a confirm");
        assert_eq!(dlg.title, "Revert to Label");
        assert!(dlg.message.starts_with("Revert 3 files to label \"v1\"?"), "{}", dlg.message);
        assert!(matches!(dlg.id, DialogId::RevertLabel { .. }));
        app.finish_dialog(dlg.id, DialogResult::Button(0));
        handle_next(&mut app, blobs_msg).await;
        handle_next(&mut app, file_loaded).await;
        let text = |p: &Path| {
            let i = app.editor.index_of(p).unwrap_or_else(|| panic!("a tab for {}", p.display()));
            (app.editor.buffers[i].text(), app.editor.buffers[i].dirty())
        };
        assert_eq!(text(&open), ("open v1\n".to_string(), true));
        assert_eq!(text(&closed), ("closed v1\n".to_string(), true));
        assert_eq!(text(&gone), ("gone v1\n".to_string(), true));
        assert!(gone.parent().unwrap().is_dir());
        assert_eq!(std::fs::read_to_string(&closed).unwrap(), "closed v2\n", "nothing is saved");
        assert!(app.focus.is(PanelId::LocalHistory));
        assert_eq!(last_note(&app), "Reverted 3 files to label \"v1\" (unsaved; Ctrl+Z in each tab undoes)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_label_member_that_cannot_open_is_reported() {
        let (mut app, dir) = test_app("history-revert-label-fail");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let d = root.join("d");
        let closed = saved(&app, "d/closed.txt", "closed v1\n");
        let _open = load(&mut app, "d/open.txt", "open v1\n");
        app.finish_dialog(DialogId::PutLabel(d.clone()), DialogResult::Text("v1".into()));
        handle_next(&mut app, label_msg).await;
        std::fs::write(&closed, "closed v2\n").unwrap();
        app.show_local_history_dir(d.clone());
        handle_next(&mut app, scan_of(&d)).await;
        let rows = app.history_panel.project_rows();
        let label = rows.iter().position(|r| r.starts_with("Label")).unwrap_or_else(|| panic!("a label row in {rows:?}"));
        for _ in 0..label {
            history(&mut app, HistoryAction::Down);
        }
        history(&mut app, HistoryAction::Revert);
        let dlg = app.dialog.take().expect("a confirm");
        app.finish_dialog(dlg.id, DialogResult::Button(0));
        // decode_file refuses a file with a NUL.
        std::fs::write(&closed, b"bin\0ary").unwrap();
        handle_next(&mut app, blobs_msg).await;
        handle_next(&mut app, file_loaded).await;
        assert!(app.editor.index_of(&closed).is_none());
        assert!(!app.pending_revert.contains_key(&closed));
        assert_eq!(last_note(&app), "Revert to label: d/closed.txt was not reverted (the file did not open)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_label_member_queued_when_the_root_switches_is_reported() {
        let (mut app, dir) = test_app("history-revert-label-switch");
        with_history(&mut app);
        let root = app.workspace.root.clone();
        let d = root.join("d");
        let closed = saved(&app, "d/closed.txt", "closed v1\n");
        let _open = load(&mut app, "d/open.txt", "open v1\n");
        app.finish_dialog(DialogId::PutLabel(d.clone()), DialogResult::Text("v1".into()));
        handle_next(&mut app, label_msg).await;
        std::fs::write(&closed, "closed v2\n").unwrap();
        app.show_local_history_dir(d.clone());
        handle_next(&mut app, scan_of(&d)).await;
        let rows = app.history_panel.project_rows();
        let label = rows.iter().position(|r| r.starts_with("Label")).unwrap_or_else(|| panic!("a label row in {rows:?}"));
        for _ in 0..label {
            history(&mut app, HistoryAction::Down);
        }
        history(&mut app, HistoryAction::Revert);
        let dlg = app.dialog.take().expect("a confirm");
        app.finish_dialog(dlg.id, DialogResult::Button(0));
        handle_next(&mut app, blobs_msg).await;
        assert!(matches!(app.pending_revert.get(&closed), Some(PendingRevert::Member(_))), "queued, its load in flight");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other = std::fs::canonicalize(&other).unwrap();
        app.do_switch_root(other);
        assert!(app.pending_revert.is_empty());
        let texts: Vec<&str> = app.notifications.iter().map(|n| n.text.as_str()).collect();
        assert!(texts.contains(&"Revert to label: d/closed.txt was not reverted (the project root changed)"), "{texts:?}");
        // The old root's load arrives stale and opens nothing.
        handle_next(&mut app, file_loaded).await;
        assert!(app.editor.index_of(&closed).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_window_opened_empty_takes_the_active_file() {
        let (mut app, dir) = test_app("history-toggle");
        with_history(&mut app);
        let a = load(&mut app, "a.txt", "one\n");
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        assert_eq!(app.history_panel.target(), Some(a.as_path()));
        assert_eq!(app.history_panel.view(), View::File);
        handle_next(&mut app, list_of(&a)).await;
        // Another tab does not move it.
        let b = load(&mut app, "b.txt", "two\n");
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        assert_eq!(app.history_panel.target(), Some(a.as_path()));
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        // No file open: the project's files.
        let (mut app2, dir2) = test_app("history-toggle-empty");
        app2.toggle_tool_window(ToolWindowId::LocalHistory);
        assert!(app2.layout.is_visible(ToolWindowId::LocalHistory));
        assert_eq!((app2.history_panel.target(), app2.history_panel.view()), (None, View::Project));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
