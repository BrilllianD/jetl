//! The Local History tool window (bottom), two views:
//! - the file view: one file's snapshots, newest first, next to the diff of
//!   the selected one against the file's current text (its open buffer,
//!   else the disk). Revert puts the selected snapshot back, Revert Change
//!   only the diff's selected changes (both undoable); Put Label and Remove
//!   Label work in place. It shows one file until `editor.local_history`
//!   picks another; a Project tree rename takes it along.
//! - the project view (`project.rs`): the files of the project or a folder
//!   that have history, deleted ones included, and their labels grouped,
//!   next to the diff of the selected file's newest snapshot; Enter drills
//!   into a file's view, Backspace (`[All Files]`) comes back.
//!
//! Typing filters the list shown. The panel holds state only. It asks
//! through `Effect::History`: the list (`HistoryMsg::List`, applied by the
//! app while the window shows that path), the scan (`HistoryMsg::Recent`,
//! applied while the project view shows that folder; one in flight at a
//! time), the diff (`HistoryMsg::Diff`, applied only for the newest
//! request), a revert (of the snapshot, or of changes with the text they
//! were computed for), and the label dialogs.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::project::{ProjectView, Selection};
use super::{ChangesRevert, Entry, RecentFile, Request};
use crate::actions::{Action, HistoryAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::DiffKey;
use crate::git::diff::DiffDoc;
use crate::git::panel::{DiffOpts, DiffPane, DiffToggle, render_diff, render_diff_header};
use crate::ui::focus::PanelId;
use crate::ui::menu::{ContextMenu, MenuItem};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;

/// Below this many columns inside the window, the list and the diff take
/// turns: the focused one fills the width.
const NARROW: u16 = 60;
/// The list's width beside the diff: two fifths, within these bounds.
const LIST_MIN: u16 = 26;
const LIST_MAX: u16 = 48;
/// The project view's list (paths) beside the diff: half, within these.
const FILES_MIN: u16 = 30;
const FILES_MAX: u16 = 64;

/// Which view the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// One file's snapshots.
    File,
    /// The files of a folder that have history.
    Project,
}

/// Which part has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    List,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    /// The project view (from a file's).
    Files,
    /// A file's own view (from the project view's row).
    ShowHistory,
    Revert,
    RevertChanges,
    PutLabel,
    RemoveLabel,
    Toggle(DiffToggle),
}

/// One snapshot row, formatted once.
#[derive(Debug, Clone)]
struct Row {
    entry: Entry,
    /// `YYYY-MM-DD HH:MM:SS`, local time.
    when: String,
    /// What the snapshot is ("Saved", "Label: v1", "Renamed from a.txt").
    label: String,
    /// Size and lines added or removed since the next older snapshot.
    info: String,
}

impl Row {
    /// The date without the year and the seconds, for the list.
    fn short(&self) -> &str {
        self.when.get(5..16).unwrap_or(&self.when)
    }
}

/// The file the file view shows.
#[derive(Debug, Clone)]
struct Target {
    path: PathBuf,
    /// Relative to the root when under it.
    rel: String,
}

#[derive(Debug)]
pub struct LocalHistoryPanel {
    view: View,
    /// The project view, kept while a file's view is shown.
    project: Option<ProjectView>,
    target: Option<Target>,
    /// Select this snapshot when the file's list arrives (a label's file
    /// picked in the project view).
    want: Option<Entry>,
    rows: Vec<Row>,
    /// Asked for and not answered yet.
    loading: bool,
    filter: String,
    /// The rows that match the filter (indices into `rows`).
    shown: Vec<usize>,
    list: ListState,
    area: Area,
    diff: DiffPane,
    diff_opts: DiffOpts,
    /// The newest diff request; older answers are dropped.
    diff_seq: u64,
    hits: Vec<(Btn, Rect)>,
    list_rect: Rect,
    diff_rect: Rect,
}

impl Default for LocalHistoryPanel {
    fn default() -> Self {
        Self {
            view: View::File,
            project: None,
            target: None,
            want: None,
            rows: Vec::new(),
            loading: false,
            filter: String::new(),
            shown: Vec::new(),
            list: ListState::default(),
            area: Area::List,
            diff: DiffPane::new(true),
            diff_opts: DiffOpts { unified: None, collapse: true },
            diff_seq: 0,
            hits: Vec::new(),
            list_rect: Rect::default(),
            diff_rect: Rect::default(),
        }
    }
}

/// The same snapshot in a newer list: a rename rewrites `path`, so it is
/// left out.
fn same(a: &Entry, b: &Entry) -> bool {
    a.at == b.at && a.blob == b.blob && a.kind == b.kind && a.detail == b.detail
}

fn rel_to(path: &Path, root: &Path) -> String {
    crate::ui::palette::display_path(path, root)
}

impl LocalHistoryPanel {
    /// The file shown.
    pub fn target(&self) -> Option<&Path> {
        self.target.as_ref().map(|t| t.path.as_path())
    }

    /// The view shown.
    pub fn view(&self) -> View {
        self.view
    }

    /// The project view's folder, while there is one.
    pub fn project_scope(&self) -> Option<&Path> {
        self.project.as_ref().map(|p| p.scope.as_path())
    }

    /// Show `path`'s snapshots (the list follows, `set_entries`). Another
    /// file starts over; the same one keeps its rows, filter and selection.
    pub fn set_target(&mut self, path: PathBuf, root: &Path) {
        self.want = None;
        if self.view != View::File {
            self.view = View::File;
            self.area = Area::List;
            self.diff.clear();
        }
        if self.target() != Some(path.as_path()) {
            self.rows.clear();
            self.shown.clear();
            self.filter.clear();
            self.list = ListState::default();
            self.diff.clear();
            self.area = Area::List;
        }
        self.loading = true;
        let rel = rel_to(&path, root);
        self.target = Some(Target { path, rel });
    }

    /// The project view of folder `scope` (as it was when it shows that
    /// folder already), scanned again.
    pub fn show_project(&mut self, scope: PathBuf, cx: &mut Ctx) {
        if self.project.as_ref().is_none_or(|p| p.scope != scope) {
            self.project = Some(ProjectView::new(scope, &cx.workspace.root));
        }
        self.enter_project(cx);
    }

    /// Back to the project view: the one kept, else the root's with the
    /// file shown selected.
    fn files(&mut self, cx: &mut Ctx) {
        if self.view == View::Project {
            return;
        }
        if self.project.is_none() {
            let root = cx.workspace.root.clone();
            let mut p = ProjectView::new(root.clone(), &root);
            p.want = self.target().map(Path::to_path_buf);
            self.project = Some(p);
        }
        self.enter_project(cx);
    }

    fn enter_project(&mut self, cx: &mut Ctx) {
        if self.view != View::Project {
            self.view = View::Project;
            self.area = Area::List;
            self.diff.clear();
        }
        self.rescan(cx);
        self.request_diff(cx);
    }

    /// Scan the project view's folder again: now, or once the scan in
    /// flight has landed (so a burst of snapshots costs two scans).
    pub fn rescan(&mut self, cx: &mut Ctx) {
        let Some(p) = &mut self.project else { return };
        if p.scanning {
            p.again = true;
            return;
        }
        p.scanning = true;
        cx.effects.push(Effect::History(Request::Recent(p.scope.clone())));
    }

    /// Whether the project view is shown on a folder that holds `path`.
    pub fn project_shows(&self, path: &Path) -> bool {
        self.view == View::Project && self.project.as_ref().is_some_and(|p| path.starts_with(&p.scope))
    }

    /// A scan's answer, while the project view shows `scope`: the rows
    /// follow (filter and selection kept) and the selection's diff is asked
    /// again; a scan asked meanwhile goes out now.
    pub fn set_files(&mut self, scope: &Path, files: Vec<RecentFile>, root: &Path, offset: impl Fn(i64) -> i64, cx: &mut Ctx) {
        let Some(p) = self.project.as_mut().filter(|p| p.scope == scope) else { return };
        p.scanning = false;
        p.set_files(files, root, offset);
        let again = std::mem::take(&mut p.again);
        if self.view == View::Project {
            self.request_diff(cx);
        }
        if again {
            self.rescan(cx);
        }
    }

    /// The root switched: the project view shows the new root's files.
    /// Returns whether it is the view shown (to be scanned).
    pub fn reroot(&mut self, root: &Path) -> bool {
        if self.project.is_none() {
            return false;
        }
        self.project = Some(ProjectView::new(root.to_path_buf(), root));
        if self.view == View::Project {
            self.diff.clear();
        }
        self.view == View::Project
    }

    /// A Project tree rename of the project view's folder or one above it:
    /// follow it. Returns whether the project view is shown (to be scanned:
    /// a rename under it changes its rows too).
    pub fn renamed(&mut self, from: &Path, to: &Path, root: &Path) -> bool {
        let Some(p) = &mut self.project else { return false };
        let moved = p.retarget(from, to, root);
        self.view == View::Project && (moved || from.starts_with(&p.scope) || to.starts_with(&p.scope))
    }

    /// A Project tree rename of `from` (a file, or a folder above the file
    /// shown): follow it. Returns the new path when the window moved.
    pub fn retarget(&mut self, from: &Path, to: &Path, root: &Path) -> Option<PathBuf> {
        let t = self.target.as_mut()?;
        let new = if t.path == from { to.to_path_buf() } else { to.join(t.path.strip_prefix(from).ok()?) };
        t.rel = rel_to(&new, root);
        t.path = new.clone();
        Some(new)
    }

    /// The snapshots of the file shown, newest first. The filter stays and
    /// so does the selected snapshot (else the same row); the diff of the
    /// selection is asked for again, as the current text may have changed.
    /// `offset` gives seconds east of UTC at a time (fixed in tests).
    pub fn set_entries(&mut self, entries: Vec<Entry>, root: &Path, offset: impl Fn(i64) -> i64, cx: &mut Ctx) {
        let keep = self.want.take().or_else(|| self.selected().map(|r| r.entry.clone()));
        let at = self.list.selected;
        self.rows = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let delta = match entries.get(i + 1) {
                    None => String::new(),
                    Some(older) if older.lines == e.lines => "  =".into(),
                    Some(older) if e.lines > older.lines => lines(e.lines - older.lines, "+"),
                    Some(older) => lines(older.lines - e.lines, "−"),
                };
                Row {
                    entry: e.clone(),
                    when: super::local_time(e.at, &offset),
                    label: e.label_in(root),
                    info: format!("{}{delta}", human_size(e.len)),
                }
            })
            .collect();
        self.loading = false;
        self.refilter(keep.as_ref(), at);
        if self.view == View::File {
            self.request_diff(cx);
        }
    }

    /// Recompute the rows that match the filter; select `keep` when it is
    /// still shown, else row `at` (clamped).
    fn refilter(&mut self, keep: Option<&Entry>, at: usize) {
        let terms: Vec<String> = self.filter.split_whitespace().map(str::to_lowercase).collect();
        self.shown = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                let text = format!("{} {}", r.when, r.label).to_lowercase();
                terms.iter().all(|t| text.contains(t.as_str()))
            })
            .collect();
        self.list.set_len(self.shown.len());
        let found = keep.and_then(|k| self.shown.iter().position(|&i| same(&self.rows[i].entry, k)));
        self.list.select(found.unwrap_or(at));
    }

    fn selected(&self) -> Option<&Row> {
        self.rows.get(*self.shown.get(self.list.selected)?)
    }

    /// The file and snapshot selected in the view shown: the diff, Revert
    /// and Revert Change work on it.
    fn selection(&self) -> Option<Selection> {
        match self.view {
            View::File => {
                let (t, r) = (self.target.as_ref()?, self.selected()?);
                Some(Selection {
                    path: t.path.clone(),
                    rel: t.rel.clone(),
                    entry: r.entry.clone(),
                    label: r.label.clone(),
                    when: r.when.clone(),
                    deleted: false,
                })
            }
            View::Project => self.project.as_ref()?.selection(),
        }
    }

    /// The selected snapshot's date and label (the revert message).
    pub fn selected_title(&self) -> String {
        self.selection().map(|s| format!("{}  {}", s.when, s.label)).unwrap_or_default()
    }

    /// Whether the window shows `path` with snapshot `id` selected: a
    /// revert's text is applied only then.
    pub fn shows(&self, path: &Path, id: &str) -> bool {
        self.selection().is_some_and(|s| s.path == path && s.entry.blob == id)
    }

    /// Whether the project view still lists label `name` put at `at`: a
    /// label revert's texts are applied only then.
    pub fn shows_label(&self, at: u64, name: &str) -> bool {
        self.view == View::Project && self.project.as_ref().is_some_and(|p| p.has_label(at, name))
    }

    /// Ask for the diff of the selection (a new request each time: the
    /// current text may have changed); nothing selected clears it.
    fn request_diff(&mut self, cx: &mut Ctx) {
        let Some(s) = self.selection() else {
            self.diff.clear();
            if self.area == Area::Diff {
                self.area = Area::List;
            }
            return;
        };
        let key = DiffKey::Local { path: s.path, rel: s.rel, id: s.entry.blob, at: s.entry.at };
        self.diff_seq += 1;
        self.diff.begin(key.clone());
        cx.effects.push(Effect::History(Request::Diff { key, seq: self.diff_seq }));
    }

    /// Compare with the current text again (after a revert).
    pub fn reload_diff(&mut self, cx: &mut Ctx) {
        self.request_diff(cx);
    }

    /// A diff answer: only the newest request's.
    pub fn set_diff(&mut self, key: &DiffKey, seq: u64, result: anyhow::Result<DiffDoc>) {
        if seq == self.diff_seq {
            self.diff.set(key, result);
        }
    }

    /// The theme changed: the diff's syntax colors are computed again.
    pub fn theme_changed(&mut self) {
        self.diff.restyle();
    }

    /// The selection moved: the diff follows when it shows another snapshot.
    fn selection_moved(&mut self, cx: &mut Ctx) {
        // Two files may hold the same text: the path counts too.
        let id = self.selection().map(|s| (s.path, s.entry.blob));
        let shown = match &self.diff.key {
            Some(DiffKey::Local { path, id, .. }) => Some((path.clone(), id.clone())),
            _ => None,
        };
        if id != shown || id.is_none() {
            self.request_diff(cx);
        }
    }

    /// The list of the view shown.
    fn list_mut(&mut self) -> &mut ListState {
        match (self.view, &mut self.project) {
            (View::Project, Some(p)) => &mut p.list,
            _ => &mut self.list,
        }
    }

    /// The filter of the view shown.
    fn filter_mut(&mut self) -> &mut String {
        match (self.view, &mut self.project) {
            (View::Project, Some(p)) => &mut p.filter,
            _ => &mut self.filter,
        }
    }

    /// Typing filters the list while it has focus (speed search), Backspace
    /// deletes, Esc clears; other keys go to the keymap.
    pub fn handle_key(&mut self, key: KeyEvent, cx: &mut Ctx) -> bool {
        let shown = match self.view {
            View::File => self.target.is_some(),
            View::Project => self.project.is_some(),
        };
        if self.area != Area::List || !shown || key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return false;
        }
        let filter = self.filter_mut();
        match key.code {
            KeyCode::Char(' ') if filter.is_empty() => return false,
            KeyCode::Char(c) => filter.push(c),
            KeyCode::Backspace if !filter.is_empty() => {
                filter.pop();
            }
            KeyCode::Esc if !filter.is_empty() => filter.clear(),
            _ => return false,
        }
        self.filter_changed(cx);
        true
    }

    fn filter_changed(&mut self, cx: &mut Ctx) {
        match (self.view, &mut self.project) {
            (View::Project, Some(p)) => {
                let keep = p.selected_id();
                p.refilter(keep.as_ref(), 0);
            }
            _ => {
                let keep = self.selected().map(|r| r.entry.clone());
                self.refilter(keep.as_ref(), 0);
            }
        }
        self.selection_moved(cx);
    }

    /// Enter in the project view: a label row opens or closes; a file (or
    /// a label's file) shows its own view, with that snapshot selected.
    fn open_in_project(&mut self, cx: &mut Ctx) {
        let Some(p) = &mut self.project else { return };
        if p.on_label() {
            p.toggle();
            return self.selection_moved(cx);
        }
        let Some(s) = p.selection() else { return };
        let root = cx.workspace.root.clone();
        self.set_target(s.path.clone(), &root);
        self.want = Some(s.entry);
        cx.effects.push(Effect::History(Request::List(s.path)));
    }

    pub fn handle_action(&mut self, a: &HistoryAction, cx: &mut Ctx) -> bool {
        use HistoryAction as H;
        let page = |rows: usize| rows.saturating_sub(1).max(1);
        match a {
            H::Up | H::Down | H::PageUp | H::PageDown | H::Home | H::End if self.area == Area::Diff => {
                let delta = match a {
                    H::Up => -1,
                    H::Down => 1,
                    H::PageUp => -(page(self.diff.page) as isize),
                    H::PageDown => page(self.diff.page) as isize,
                    H::Home => isize::MIN / 2,
                    _ => isize::MAX / 2,
                };
                self.diff.scroll_by(delta);
            }
            H::ExtendUp | H::ExtendDown if self.area == Area::Diff => {
                let forward = *a == H::ExtendDown;
                if !self.diff.extend_block(forward) {
                    cx.notify(Level::Info, if forward { "No more changes below" } else { "No more changes above" });
                }
            }
            H::Up | H::Down | H::PageUp | H::PageDown | H::Home | H::End | H::ExtendUp | H::ExtendDown => {
                let list = self.list_mut();
                let n = page(list.viewport);
                match a {
                    H::Up | H::ExtendUp => list.up(1),
                    H::Down | H::ExtendDown => list.down(1),
                    H::PageUp => list.up(n),
                    H::PageDown => list.down(n),
                    H::Home => list.home(),
                    _ => list.end(),
                }
                self.selection_moved(cx);
            }
            H::Left => self.area = Area::List,
            H::Right => {
                if self.diff.key.is_some() {
                    self.area = Area::Diff;
                }
            }
            H::NextSection => match self.area {
                Area::Diff => self.area = Area::List,
                Area::List if self.diff.key.is_some() => self.area = Area::Diff,
                Area::List => {}
            },
            H::OpenFile if self.view == View::Project => self.open_in_project(cx),
            H::OpenFile => {
                if let Some(t) = &self.target {
                    cx.open_file(t.path.clone());
                }
            }
            H::Files => self.files(cx),
            H::NextChange | H::PrevChange => {
                let forward = *a == H::NextChange;
                if !self.diff.step_block(forward) {
                    cx.notify(Level::Info, if forward { "No more changes below" } else { "No more changes above" });
                }
            }
            H::ToggleDiffLayout => self.diff_opts.unified = Some(!self.diff.unified),
            H::ToggleDiffCollapse => self.diff_opts.collapse = !self.diff_opts.collapse,
            H::Refresh if self.view == View::Project => {
                // Always asks: a lost answer cannot wedge the scans.
                if let Some(p) = &mut self.project {
                    (p.scanning, p.again) = (false, false);
                }
                self.rescan(cx);
            }
            H::Refresh => {
                if let Some(t) = &self.target {
                    cx.effects.push(Effect::History(Request::List(t.path.clone())));
                }
            }
            H::Revert => self.revert(cx),
            H::RevertChanges => self.revert_changes(cx),
            H::SelectAllChanges => {
                if !self.diff.select_all_blocks() {
                    cx.notify(Level::Info, "No changes in the diff");
                }
            }
            H::PutLabel => self.put_label(cx),
            H::RemoveLabel => self.remove_label(cx),
            H::ContextMenu if self.area == Area::Diff => self.context_menu(self.diff_rect.x + 2, self.diff_rect.y, cx),
            H::ContextMenu => {
                let list = self.list_mut();
                let y = list.selected.saturating_sub(list.offset) as u16;
                self.context_menu(self.list_rect.x + 2, self.list_rect.y + y, cx);
            }
            H::Escape => {
                if !self.filter_mut().is_empty() {
                    self.filter_mut().clear();
                    self.filter_changed(cx);
                } else if self.area == Area::Diff {
                    self.area = Area::List;
                } else {
                    cx.focus(PanelId::Editor);
                }
            }
        }
        true
    }

    fn revert(&mut self, cx: &mut Ctx) {
        match self.selection() {
            Some(s) => cx.effects.push(Effect::History(Request::Revert { path: s.path, id: s.entry.blob })),
            None if self.on_label() => {
                if let Some((at, name, members)) = self.project.as_ref().and_then(ProjectView::label_group) {
                    cx.effects.push(Effect::History(Request::RevertLabel { at, name, members }));
                }
            }
            None => cx.notify(Level::Info, "Select a snapshot to revert to"),
        }
    }

    /// A label row of the project view is selected.
    fn on_label(&self) -> bool {
        self.view == View::Project && self.project.as_ref().is_some_and(ProjectView::on_label)
    }

    /// The changes the diff has selected, for Revert Change: `None` when
    /// there is nothing to revert, else the selected range.
    fn revertible(&self) -> Option<std::ops::Range<usize>> {
        let doc = self.diff.doc.as_ref().filter(|d| !d.binary && !d.lossy && !self.diff.loading)?;
        let range = self.diff.selected_blocks();
        (!range.is_empty() && range.end <= doc.blocks(false).len()).then_some(range)
    }

    /// Put the diff's selected changes back: the request carries the edits
    /// and the text they were computed for (they apply only to that text).
    fn revert_changes(&mut self, cx: &mut Ctx) {
        let Some(s) = self.selection() else {
            return cx.notify(Level::Info, "Select a snapshot, then the changes to revert in its diff");
        };
        let doc = match &self.diff.doc {
            Some(doc) if matches!(&self.diff.key, Some(DiffKey::Local { path, id, .. }) if *id == s.entry.blob && *path == s.path) => doc,
            _ => return cx.notify(Level::Info, "The diff is still loading"),
        };
        if self.diff.loading {
            return cx.notify(Level::Info, "Comparing with the current text again; revert once it shows");
        }
        if doc.binary || doc.lossy {
            return cx.notify(Level::Warn, "The snapshot or the file is not valid UTF-8 text; not reverted");
        }
        let Some(range) = self.revertible() else {
            return cx.notify(Level::Info, "No changes: the file has this snapshot's text");
        };
        let spans: Vec<_> = range.clone().filter_map(|i| doc.block_lines(i)).collect();
        let changes = crate::editor::gutter::revert_spans(&doc.old, &doc.new, &spans);
        let title = format!("{}  {}", s.when, s.label);
        let req = ChangesRevert { path: s.path, base: doc.new.concat(), changes, title };
        cx.effects.push(Effect::History(Request::RevertChanges(Box::new(req))));
        // After the reload, the change that followed the range is current.
        self.diff.block = range.start;
        self.diff.anchor = None;
    }

    fn put_label(&mut self, cx: &mut Ctx) {
        if self.view == View::Project {
            // The selected file, else the folder the view scans.
            let path = self.selection().map(|s| s.path).or_else(|| self.project_scope().map(Path::to_path_buf));
            return match path {
                Some(path) => cx.effects.push(Effect::History(Request::PutLabel(path))),
                None => cx.notify(Level::Info, "No file shown: File > Local History… picks one"),
            };
        }
        match &self.target {
            Some(t) => cx.effects.push(Effect::History(Request::PutLabel(t.path.clone()))),
            None => cx.notify(Level::Info, "No file shown: File > Local History… picks one"),
        }
    }

    fn remove_label(&mut self, cx: &mut Ctx) {
        let label = match self.view {
            View::Project => match &self.project {
                Some(p) if p.shown_len() > 0 => p.selected_label(),
                _ => return,
            },
            View::File => {
                let (Some(t), Some(r)) = (&self.target, self.selected()) else { return };
                r.entry.kind.pinned().then(|| (r.entry.detail.clone().unwrap_or_default(), vec![(t.path.clone(), r.entry.clone())]))
            }
        };
        match label {
            Some((name, entries)) => cx.effects.push(Effect::History(Request::RemoveLabel { name, entries })),
            None => cx.notify(Level::Info, "Only labels can be removed; other snapshots go by age ([local_history] keep_days)"),
        }
    }

    /// Whether Remove Label applies to the selection.
    fn label_selected(&self) -> bool {
        match self.view {
            View::Project => self.project.as_ref().is_some_and(|p| p.selected_label().is_some()),
            View::File => self.selected().is_some_and(|r| r.entry.kind.pinned()),
        }
    }

    fn context_menu(&self, x: u16, y: u16, cx: &mut Ctx) {
        let selected = self.selection().is_some();
        let label = self.label_selected();
        let h = |a| Action::History(a);
        let items = match self.view {
            View::Project => vec![
                MenuItem::new("Show History", h(HistoryAction::OpenFile)).enabled(selected),
                MenuItem::new("Revert", h(HistoryAction::Revert)).enabled(selected || self.on_label()),
                MenuItem::new("Revert Selected Changes", h(HistoryAction::RevertChanges)).enabled(self.revertible().is_some()),
                MenuItem::new("Remove Label…", h(HistoryAction::RemoveLabel)).enabled(label),
            ],
            View::File => vec![
                MenuItem::new("Revert", h(HistoryAction::Revert)).enabled(selected),
                MenuItem::new("Revert Selected Changes", h(HistoryAction::RevertChanges)).enabled(self.revertible().is_some()),
                MenuItem::new("Put Label…", h(HistoryAction::PutLabel)),
                MenuItem::new("Remove Label…", h(HistoryAction::RemoveLabel)).enabled(label),
                MenuItem::new("Open File", h(HistoryAction::OpenFile)),
                MenuItem::new("All Files", h(HistoryAction::Files)),
            ],
        };
        cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, x, y)));
    }

    pub fn handle_mouse(&mut self, m: &Mouse, cx: &mut Ctx) -> bool {
        let p = Position::new(m.x, m.y);
        match m.kind {
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                if button == MouseButton::Left
                    && let Some(b) = self.hits.iter().find(|(_, r)| r.contains(p)).map(|(b, _)| *b)
                {
                    match b {
                        Btn::Files => self.files(cx),
                        Btn::ShowHistory => self.open_in_project(cx),
                        Btn::Revert => self.revert(cx),
                        Btn::RevertChanges => self.revert_changes(cx),
                        Btn::PutLabel => self.put_label(cx),
                        Btn::RemoveLabel => self.remove_label(cx),
                        Btn::Toggle(DiffToggle::Layout) => self.diff_opts.unified = Some(!self.diff.unified),
                        Btn::Toggle(DiffToggle::Collapse) => self.diff_opts.collapse = !self.diff_opts.collapse,
                    }
                    return true;
                }
                let rect = self.list_rect;
                if let Some(i) = self.list_mut().hit(rect, m.x, m.y) {
                    self.area = Area::List;
                    self.list_mut().select(i);
                    self.selection_moved(cx);
                    if button == MouseButton::Right {
                        self.context_menu(m.x, m.y, cx);
                    } else if m.clicks >= 2 {
                        self.handle_action(&HistoryAction::OpenFile, cx);
                    }
                } else if self.diff_rect.contains(p) && self.diff.key.is_some() {
                    self.area = Area::Diff;
                    if button == MouseButton::Left {
                        // Shift or Ctrl: many terminals keep Shift+click for
                        // their own selection.
                        self.diff.click(m.y, m.mods.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL));
                    } else {
                        // A right click on a change outside the selection
                        // selects it first.
                        let (sel, anchor, block) = (self.diff.selected_blocks(), self.diff.anchor, self.diff.block);
                        if self.diff.click(m.y, false) && sel.contains(&self.diff.block) {
                            (self.diff.anchor, self.diff.block) = (anchor, block);
                        }
                        self.context_menu(m.x, m.y, cx);
                    }
                }
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d: isize = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                if self.list_rect.contains(p) {
                    self.list_mut().scroll(d as i32);
                } else if self.diff_rect.contains(p) {
                    self.diff.scroll_by(d);
                }
                true
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight if self.diff_rect.contains(p) => {
                let pane = &mut self.diff;
                pane.hscroll = if m.kind == MouseEventKind::ScrollLeft { pane.hscroll.saturating_sub(4) } else { pane.hscroll + 4 };
                true
            }
            _ => false,
        }
    }

    /// The labels of the rows shown, in order (tests).
    #[cfg(test)]
    pub fn labels(&self) -> Vec<String> {
        self.shown.iter().map(|&i| self.rows[i].label.clone()).collect()
    }

    /// The project view's rows (tests; `ProjectView::rows`).
    #[cfg(test)]
    pub fn project_rows(&self) -> Vec<String> {
        self.project.as_ref().map(|p| p.rows()).unwrap_or_default()
    }

    /// The selected row's label (tests).
    #[cfg(test)]
    pub fn selected_label(&self) -> Option<&str> {
        self.selected().map(|r| r.label.as_str())
    }

    #[cfg(test)]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The snapshot the diff shows and whether its doc is there (tests).
    #[cfg(test)]
    pub fn diff_state(&self) -> (Option<&DiffKey>, bool) {
        (self.diff.key.as_ref(), self.diff.doc.is_some())
    }

    /// Whether the newest diff request is still unanswered (tests).
    #[cfg(test)]
    pub fn diff_loading(&self) -> bool {
        self.diff.loading
    }

    /// The diff's changes and the selected ones (tests).
    #[cfg(test)]
    pub fn changes(&self) -> (usize, std::ops::Range<usize>) {
        (self.diff.doc.as_ref().map_or(0, |d| d.blocks(false).len()), self.diff.selected_blocks())
    }

    // ---- rendering -------------------------------------------------------

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let title = match (self.view, &self.project, &self.target) {
            (View::Project, Some(p), _) => format!(" Local History: {} ", p.rel),
            (View::File, _, Some(t)) => format!(" Local History: {} ", t.rel),
            _ => " Local History ".to_string(),
        };
        let block = tool_window_block(&title, focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.hits.clear();
        self.list_rect = Rect::default();
        self.diff_rect = Rect::default();
        if inner.height < 2 || inner.width < 10 {
            return;
        }
        let buf = frame.buffer_mut();
        self.render_toolbar(buf, Rect::new(inner.x, inner.y, inner.width, 1), focused, theme);
        let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
        let project = self.view == View::Project && self.project.is_some();
        if !project && self.target.is_none() {
            let msg = "Open a file, then File > Local History… (Alt+Shift+H) shows its snapshots";
            buf.set_stringn(body.x + 1, body.y, msg, body.width.saturating_sub(1) as usize, theme.dim);
            return;
        }
        let (list, diff) = if body.width < NARROW {
            match self.area {
                Area::List => (body, Rect::default()),
                Area::Diff => (Rect::default(), body),
            }
        } else {
            let lw = if project { (body.width / 2).clamp(FILES_MIN, FILES_MAX) } else { (body.width * 2 / 5).clamp(LIST_MIN, LIST_MAX) };
            for y in body.y..body.bottom() {
                buf[(body.x + lw, y)].set_symbol("│").set_style(theme.splitter);
            }
            (Rect::new(body.x, body.y, lw, body.height), Rect::new(body.x + lw + 1, body.y, body.width - lw - 1, body.height))
        };
        match &mut self.project {
            Some(p) if project => {
                if list.width > 0 && list.height > 0 && p.render_list(buf, list, focused && self.area == Area::List, theme) {
                    self.list_rect = list;
                }
            }
            _ => self.render_list(buf, list, focused, theme),
        }
        self.render_diff_side(buf, diff, focused, theme);
    }

    /// The buttons on the left, the filter on the right.
    fn render_toolbar(&mut self, buf: &mut Screen, r: Rect, focused: bool, theme: &Theme) {
        buf.set_style(r, Style::default().bg(theme.panel_bg));
        let selected = self.selection().is_some();
        let changes = match self.diff.selected_blocks().len() {
            n if n > 1 => format!("[Revert {n} Changes]"),
            _ => "[Revert Change]".to_string(),
        };
        let project = self.view == View::Project;
        let (first, label_btn) = if project {
            ((Btn::ShowHistory, "[Show History]", selected || self.on_label()), None)
        } else {
            ((Btn::Files, "[All Files]", true), Some((Btn::PutLabel, "[Put Label…]", self.target.is_some())))
        };
        let enabled = [
            Some(first),
            Some((Btn::Revert, "[Revert]", selected || self.on_label())),
            Some((Btn::RevertChanges, changes.as_str(), self.revertible().is_some())),
            label_btn,
            Some((Btn::RemoveLabel, "[Remove Label]", self.label_selected())),
        ];
        let enabled = enabled.into_iter().flatten();
        let mut x = r.x;
        for (b, label, on) in enabled {
            // Whole buttons only; the keys and the menu still work.
            if x + label.width() as u16 > r.right() {
                break;
            }
            let style = if on { theme.title } else { theme.dim }.bg(theme.panel_bg);
            let rect = put(buf, &mut x, r.y, r.right(), label, style);
            self.hits.push((b, rect));
            x += 1;
        }
        let (filter, has_rows) = match (&self.project, project) {
            (Some(p), true) => (p.filter.as_str(), !p.is_empty()),
            _ => (self.filter.as_str(), !self.rows.is_empty()),
        };
        let (text, style) = if !filter.is_empty() {
            (format!("filter: {filter} "), theme.status_accent)
        } else if focused && self.area == Area::List && has_rows {
            ("type to filter ".to_string(), theme.dim)
        } else {
            return;
        };
        let w = text.width() as u16;
        if r.right() >= x + w {
            buf.set_stringn(r.right() - w, r.y, &text, w as usize, style.bg(theme.panel_bg));
        }
    }

    fn render_list(&mut self, buf: &mut Screen, r: Rect, focused: bool, theme: &Theme) {
        if r.width == 0 || r.height == 0 {
            return;
        }
        let msg = |buf: &mut Screen, text: &str| {
            buf.set_stringn(r.x + 1, r.y, text, r.width.saturating_sub(1) as usize, theme.dim);
        };
        if self.rows.is_empty() {
            let name = self.target().and_then(Path::file_name).map(|n| n.to_string_lossy()).unwrap_or_default();
            let text = if self.loading { "Loading…".to_string() } else { format!("No local history for {name} yet: saves record it") };
            return msg(buf, &text);
        }
        if self.shown.is_empty() {
            return msg(buf, &format!("No snapshot matches \"{}\"", self.filter));
        }
        self.list_rect = r;
        self.list.fit(self.shown.len(), r.height as usize);
        let focused = focused && self.area == Area::List;
        for (vi, i) in self.list.visible().enumerate() {
            let row = Rect::new(r.x, r.y + vi as u16, r.width, 1);
            let selected = i == self.list.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(row, base);
            }
            let h = &self.rows[self.shown[i]];
            let label_style = if h.entry.kind.pinned() { base.add_modifier(Modifier::BOLD) } else { base };
            let dim = base.patch(theme.dim);
            // One cell of padding on each side.
            let room = (r.width as usize).saturating_sub(2 + h.short().width() + 2);
            let label = crate::util::text::fit(&h.label, room);
            let gap = room.saturating_sub(label.width());
            let mut parts = vec![(h.short(), dim), ("  ", base), (label.as_str(), label_style)];
            let pad;
            if gap >= h.info.width() + 2 {
                pad = " ".repeat(gap - h.info.width());
                parts.push((pad.as_str(), base));
                parts.push((h.info.as_str(), dim));
            }
            crate::ui::widgets::file_rows::spans(buf, row, &parts);
        }
    }

    fn render_diff_side(&mut self, buf: &mut Screen, r: Rect, focused: bool, theme: &Theme) {
        if r.width == 0 || r.height == 0 {
            return;
        }
        // The label first: the list shows the date, and a narrow header
        // cuts the end.
        let title = self.selection().map(|s| format!("{}  {}  ↔  {}", s.label, s.when, if s.deleted { "deleted" } else { "current" }));
        let Some(title) = title else {
            let msg = match (&self.project, self.view) {
                (Some(p), View::Project) => p.label_note(),
                _ => (!self.rows.is_empty()).then(|| "Select a snapshot to compare it with the current text".to_string()),
            };
            if let Some(msg) = msg {
                buf.set_stringn(r.x + 1, r.y, &msg, r.width.saturating_sub(1) as usize, theme.dim);
            }
            return;
        };
        let rest = Rect::new(r.x, r.y + 1, r.width, r.height - 1);
        self.diff_rect = rest;
        let opts = self.diff_opts;
        // First: it picks the layout the toggles show.
        render_diff(buf, rest, &mut self.diff, theme, opts);
        let style = if focused && self.area == Area::Diff { theme.title_focused } else { theme.title };
        let toggles = self.diff.doc.is_some().then_some((self.diff.unified, opts));
        let hits = render_diff_header(buf, (r.x + 1, r.y, r.right()), &title, style, toggles, theme);
        self.hits.extend(hits.into_iter().map(|(t, rect)| (Btn::Toggle(t), rect)));
    }
}

/// Write `text` at `*x` clipped to `right`, advance `*x`, return its rect.
fn put(buf: &mut Screen, x: &mut u16, y: u16, right: u16, text: &str, style: Style) -> Rect {
    let w = (text.width() as u16).min(right.saturating_sub(*x));
    if w > 0 {
        buf.set_stringn(*x, y, text, w as usize, style);
    }
    let r = Rect::new(*x, y, w, 1);
    *x += w;
    r
}

/// "  +3 lines" / "  −1 line" with `sign` in front.
fn lines(n: u64, sign: &str) -> String {
    format!("  {sign}{n} line{}", if n == 1 { "" } else { "s" })
}

/// "512 B", "1.2 KB", "3.4 MB".
fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::history::Kind;
    use crate::workspace::Workspace;

    /// Run `f` with a context; returns its effects.
    fn with_cx(f: impl FnOnce(&mut Ctx)) -> Vec<Effect> {
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/w")));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        f(&mut cx);
        cx.effects
    }

    /// An entry of /w/a.txt at minute `min` past midnight UTC, day 20 000.
    fn e(min: u64, kind: Kind, detail: Option<&str>, lines: u64) -> Entry {
        Entry {
            v: 1,
            path: "/w/a.txt".into(),
            at: (20_000 * 86_400 + min * 60) * 1000,
            blob: format!("{min:040}"),
            len: 10,
            lines,
            kind,
            detail: detail.map(Into::into),
            since: None,
        }
    }

    fn panel(entries: Vec<Entry>) -> LocalHistoryPanel {
        let mut p = LocalHistoryPanel::default();
        p.set_target(PathBuf::from("/w/a.txt"), Path::new("/w"));
        with_cx(|cx| p.set_entries(entries, Path::new("/w"), |_| 0, cx));
        p
    }

    fn requests(fx: Vec<Effect>) -> Vec<String> {
        fx.into_iter()
            .map(|e| match e {
                Effect::History(Request::Diff { key: DiffKey::Local { id, .. }, seq }) => format!("diff {} #{seq}", &id[36..]),
                Effect::History(Request::Revert { id, .. }) => format!("revert {}", &id[36..]),
                Effect::History(Request::PutLabel(p)) => format!("label {}", p.display()),
                Effect::History(Request::RevertLabel { name, members, .. }) => format!("revert label {name} of {}", members.len()),
                Effect::History(Request::RemoveLabel { name, entries }) => {
                    let paths: Vec<String> = entries.iter().map(|(p, _)| p.display().to_string()).collect();
                    if paths.len() == 1 { format!("remove {name}") } else { format!("remove {name} of {}", paths.join(" ")) }
                }
                Effect::History(Request::Recent(p)) => format!("scan {}", p.display()),
                Effect::History(Request::List(p)) => format!("list {}", p.display()),
                Effect::History(Request::RevertChanges(r)) => {
                    let ats: Vec<usize> = r.changes.iter().map(|c| c.at).collect();
                    format!("changes {ats:?} of {:?} to {}", r.base, r.title)
                }
                Effect::Notify(_, t) => format!("notify {t}"),
                Effect::Focus(p) => format!("focus {p:?}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn act(p: &mut LocalHistoryPanel, a: HistoryAction) -> Vec<String> {
        requests(with_cx(|cx| {
            p.handle_action(&a, cx);
        }))
    }

    fn typed(p: &mut LocalHistoryPanel, code: KeyCode) -> Vec<String> {
        requests(with_cx(|cx| {
            assert!(p.handle_key(KeyEvent::new(code, KeyModifiers::NONE), cx));
        }))
    }

    #[test]
    fn rows_have_the_short_date_label_and_change() {
        let p = panel(vec![e(3, Kind::Save, None, 4), e(2, Kind::Label, Some("v1"), 2), e(1, Kind::Autosave, None, 2)]);
        let r: Vec<(&str, &str, &str)> = p.rows.iter().map(|r| (r.short(), r.label.as_str(), r.info.as_str())).collect();
        assert_eq!(
            r,
            [("10-04 00:03", "Saved", "10 B  +2 lines"), ("10-04 00:02", "Label: v1", "10 B  ="), ("10-04 00:01", "Autosaved", "10 B")]
        );
        assert_eq!(p.selected_title(), "2024-10-04 00:03:00  Saved");
    }

    #[test]
    fn the_filter_matches_every_term_and_keeps_the_selection() {
        let mut p = panel(vec![e(3, Kind::Save, None, 1), e(2, Kind::Label, Some("Before refactor"), 1), e(1, Kind::Save, None, 1)]);
        act(&mut p, HistoryAction::End);
        assert_eq!(p.selected_label(), Some("Saved"));
        // Case-insensitive; the oldest "Saved" stays selected.
        assert!(typed(&mut p, KeyCode::Char('S')).is_empty(), "the same snapshot: no new diff");
        assert_eq!(p.labels(), ["Saved", "Saved"]);
        assert_eq!(p.list.selected, 1);
        // Every term must match, the date too.
        for c in " 00:02".chars() {
            typed(&mut p, KeyCode::Char(c));
        }
        assert!(p.labels().is_empty());
        for _ in 0..6 {
            typed(&mut p, KeyCode::Backspace);
        }
        typed(&mut p, KeyCode::Backspace);
        for c in "refactor 2024".chars() {
            typed(&mut p, KeyCode::Char(c));
        }
        assert_eq!(p.labels(), ["Label: Before refactor"]);
        // Esc clears the filter first, then the window lets go.
        assert!(act(&mut p, HistoryAction::Escape).is_empty(), "the same snapshot");
        assert_eq!(p.labels().len(), 3);
        assert_eq!(p.selected_label(), Some("Label: Before refactor"));
        assert_eq!(act(&mut p, HistoryAction::Escape), ["focus Editor"]);
        // Space does not start a filter (nothing bound to it either).
        assert!(with_cx(|cx| assert!(!p.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), cx))).is_empty());
    }

    #[test]
    fn a_new_list_keeps_the_selected_snapshot_else_its_row() {
        let mut p = panel(vec![e(2, Kind::Save, None, 1), e(1, Kind::Save, None, 1)]);
        act(&mut p, HistoryAction::Down);
        // A new snapshot on top: still the same one, compared again.
        let fx = requests(with_cx(|cx| {
            p.set_entries(vec![e(3, Kind::Save, None, 1), e(2, Kind::Save, None, 1), e(1, Kind::Save, None, 1)], Path::new("/w"), |_| 0, cx)
        }));
        assert_eq!(p.list.selected, 2);
        assert_eq!(fx, ["diff 0001 #3"]);
        // An entry replaced in place (an autosave run): the same row.
        with_cx(|cx| {
            p.set_entries(
                vec![e(3, Kind::Save, None, 1), e(2, Kind::Save, None, 1), e(9, Kind::Autosave, None, 1)],
                Path::new("/w"),
                |_| 0,
                cx,
            )
        });
        assert_eq!((p.list.selected, p.selected_label()), (2, Some("Autosaved")));
        // A rename rewrote `path`: still the same snapshot.
        let mut moved = e(2, Kind::Save, None, 1);
        moved.path = "/w/b.txt".into();
        act(&mut p, HistoryAction::Up);
        with_cx(|cx| {
            p.set_entries(vec![e(4, Kind::Renamed, Some("/w/a.txt"), 1), e(3, Kind::Save, None, 1), moved], Path::new("/w"), |_| 0, cx)
        });
        assert_eq!(p.list.selected, 2);
    }

    #[test]
    fn only_the_newest_diff_lands() {
        let mut p = panel(vec![e(2, Kind::Save, None, 1), e(1, Kind::Save, None, 1)]);
        let key = p.diff.key.clone().unwrap();
        let doc = || Ok(DiffDoc::build(b"a\n", b"b\n", 4));
        assert_eq!(act(&mut p, HistoryAction::Refresh), ["list /w/a.txt"]);
        // A reload of the same snapshot while one runs.
        with_cx(|cx| p.reload_diff(cx));
        p.set_diff(&key, 1, doc());
        assert!(p.diff.doc.is_none(), "an older request");
        p.set_diff(&key, 2, doc());
        assert!(p.diff.doc.is_some());
    }

    #[test]
    fn actions_ask_for_the_selected_snapshot() {
        let mut p = panel(vec![e(2, Kind::Save, None, 1), e(1, Kind::Label, Some("v1"), 1)]);
        assert_eq!(act(&mut p, HistoryAction::Revert), ["revert 0002"]);
        assert_eq!(act(&mut p, HistoryAction::PutLabel), ["label /w/a.txt"]);
        let only = act(&mut p, HistoryAction::RemoveLabel);
        assert!(only[0].starts_with("notify Only labels can be removed"), "{only:?}");
        assert_eq!(act(&mut p, HistoryAction::Down), ["diff 0001 #2"]);
        assert_eq!(act(&mut p, HistoryAction::RemoveLabel), ["remove v1"]);
        assert!(p.shows(Path::new("/w/a.txt"), &e(1, Kind::Label, None, 1).blob));
        assert!(!p.shows(Path::new("/w/b.txt"), &e(1, Kind::Label, None, 1).blob));
        // The diff and back.
        assert_eq!(p.area, Area::List);
        act(&mut p, HistoryAction::NextSection);
        assert_eq!(p.area, Area::Diff);
        assert!(act(&mut p, HistoryAction::Down).is_empty(), "the diff scrolls; the selection stays");
        act(&mut p, HistoryAction::Escape);
        assert_eq!(p.area, Area::List);
        act(&mut p, HistoryAction::Right);
        act(&mut p, HistoryAction::Left);
        assert_eq!(p.area, Area::List);
        // Nothing shown: nothing to ask.
        let mut empty = LocalHistoryPanel::default();
        assert!(act(&mut empty, HistoryAction::Revert)[0].starts_with("notify"));
        assert!(act(&mut empty, HistoryAction::PutLabel)[0].starts_with("notify"));
        assert!(act(&mut empty, HistoryAction::Refresh).is_empty());
    }

    /// `p` with the diff of its selection answered: `old` against `new`.
    fn with_diff(p: &mut LocalHistoryPanel, old: &str, new: &str) {
        let key = p.diff.key.clone().unwrap();
        p.set_diff(&key, p.diff_seq, Ok(DiffDoc::build(old.as_bytes(), new.as_bytes(), 4)));
    }

    #[test]
    fn selected_changes_are_reverted_against_the_text_they_were_made_for() {
        let mut p = panel(vec![e(2, Kind::Save, None, 1), e(1, Kind::Save, None, 1)]);
        assert!(act(&mut p, HistoryAction::RevertChanges)[0].starts_with("notify The diff is still loading"));
        // Three changes: lines 0, 2 and 4.
        with_diff(&mut p, "a\nb\nc\nd\ne\n", "A\nb\nC\nd\nE\n");
        let base = "\"A\\nb\\nC\\nd\\nE\\n\"";
        let title = "2024-10-04 00:02:00  Saved";
        // The current change alone.
        assert_eq!(act(&mut p, HistoryAction::RevertChanges), [format!("changes [0] of {base} to {title}")]);
        // In the diff, Shift+Down grows the selection; last first.
        act(&mut p, HistoryAction::Right);
        act(&mut p, HistoryAction::NextChange);
        act(&mut p, HistoryAction::ExtendDown);
        assert_eq!(p.diff.selected_blocks(), 1..3);
        let mut buf = Screen::empty(Rect::new(0, 0, 80, 1));
        p.render_toolbar(&mut buf, Rect::new(0, 0, 80, 1), true, &Theme::default());
        let row: String = (0..80).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(row.starts_with("[All Files] [Revert] [Revert 2 Changes] [Put Label…]"), "{row}");
        assert_eq!(act(&mut p, HistoryAction::RevertChanges), [format!("changes [8, 4] of {base} to {title}")]);
        // The range's first is current, alone; the app compares again, and
        // until that lands the old diff is not reverted from.
        assert_eq!(p.diff.selected_blocks(), 1..2);
        with_cx(|cx| p.reload_diff(cx));
        assert!(act(&mut p, HistoryAction::RevertChanges)[0].starts_with("notify Comparing with the current text again"));
        with_diff(&mut p, "a\nb\nc\nd\ne\n", "A\nb\nc\nd\ne\n");
        assert_eq!(p.diff.selected_blocks(), 0..1, "clamped to the one change left");
        act(&mut p, HistoryAction::SelectAllChanges);
        assert_eq!(p.diff.selected_blocks(), 0..1);
        // Nothing to revert: identical, binary or not UTF-8.
        with_cx(|cx| p.reload_diff(cx));
        with_diff(&mut p, "same\n", "same\n");
        assert!(act(&mut p, HistoryAction::RevertChanges)[0].starts_with("notify No changes"));
        assert!(act(&mut p, HistoryAction::SelectAllChanges)[0].starts_with("notify No changes in the diff"));
        with_cx(|cx| p.reload_diff(cx));
        let key = p.diff.key.clone().unwrap();
        p.set_diff(&key, p.diff_seq, Ok(DiffDoc::build(b"caf\xe9\n", b"cafe\n", 4)));
        assert!(act(&mut p, HistoryAction::RevertChanges)[0].starts_with("notify The snapshot or the file is not valid UTF-8"));
        // In the list, Shift+Down moves the selection.
        act(&mut p, HistoryAction::Left);
        assert_eq!(act(&mut p, HistoryAction::ExtendDown), ["diff 0001 #5"]);
        assert_eq!(p.list.selected, 1);
    }

    #[test]
    fn clicks_in_the_diff_pick_and_extend_the_changes() {
        let mut p = panel(vec![e(2, Kind::Save, None, 1), e(1, Kind::Save, None, 1)]);
        with_diff(&mut p, "a\nb\nc\nd\ne\n", "A\nb\nC\nd\nE\n");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        term.draw(|f| p.render(f, f.area(), true, &Theme::default())).unwrap();
        // Unified at this width: -a +A b -c +C d -e +E.
        let (x, top) = (p.diff_rect.x + 5, p.diff_rect.y);
        let click = |p: &mut LocalHistoryPanel, button, y: u16, mods| {
            let m = Mouse { kind: MouseEventKind::Down(button), x, y, clicks: 1, mods };
            requests(with_cx(|cx| {
                p.handle_mouse(&m, cx);
            }))
        };
        assert!(click(&mut p, MouseButton::Left, top + 3, KeyModifiers::NONE).is_empty());
        assert_eq!((p.area, p.diff.selected_blocks()), (Area::Diff, 1..2));
        click(&mut p, MouseButton::Left, top + 6, KeyModifiers::CONTROL);
        assert_eq!(p.diff.selected_blocks(), 1..3);
        // A right click inside the selection keeps it; outside, picks that one.
        let menu = click(&mut p, MouseButton::Right, top + 4, KeyModifiers::NONE);
        assert!(menu[0].starts_with("ContextMenu"), "{menu:?}");
        assert_eq!(p.diff.selected_blocks(), 1..3);
        click(&mut p, MouseButton::Right, top, KeyModifiers::NONE);
        assert_eq!(p.diff.selected_blocks(), 0..1);
    }

    /// The project view of /w with `project::tests::fixture`'s files.
    fn project_panel() -> LocalHistoryPanel {
        let mut p = LocalHistoryPanel::default();
        let fx = requests(with_cx(|cx| p.show_project(PathBuf::from("/w"), cx)));
        assert_eq!(fx, ["scan /w"]);
        let fx = requests(with_cx(|cx| p.set_files(Path::new("/w"), super::super::project::tests::fixture(), Path::new("/w"), |_| 0, cx)));
        assert_eq!(fx, ["diff 0009 #1"], "the newest file's newest snapshot");
        p
    }

    #[test]
    fn the_project_view_drills_into_a_file_and_back() {
        let mut p = project_panel();
        assert_eq!(p.view(), View::Project);
        // A filter, then Enter on a file: its own view, its list asked.
        typed(&mut p, KeyCode::Char('a'));
        assert_eq!(act(&mut p, HistoryAction::OpenFile), ["list /w/src/a.rs"]);
        assert_eq!((p.view(), p.target()), (View::File, Some(Path::new("/w/src/a.rs"))));
        // Backspace: the project view as it was, scanned again.
        assert_eq!(act(&mut p, HistoryAction::Files), ["scan /w", "diff 0009 #2"]);
        assert_eq!((p.view(), p.project.as_ref().unwrap().filter.as_str()), (View::Project, "a"));
        // A label's file: its view selects the label once listed.
        typed(&mut p, KeyCode::Backspace);
        act(&mut p, HistoryAction::Down);
        act(&mut p, HistoryAction::Down);
        assert!(act(&mut p, HistoryAction::OpenFile).is_empty(), "the label opens");
        act(&mut p, HistoryAction::Down);
        assert_eq!(act(&mut p, HistoryAction::OpenFile), ["list /w/b.txt"]);
        let entries = vec![e(7, Kind::Save, None, 1), e(5, Kind::Label, Some("v1"), 1)];
        let fx = requests(with_cx(|cx| p.set_entries(entries, Path::new("/w"), |_| 0, cx)));
        assert_eq!((fx, p.selected_label()), (vec!["diff 0005 #5".to_string()], Some("Label: v1")));
    }

    #[test]
    fn back_from_a_file_with_no_project_view_selects_it_among_the_roots_files() {
        let mut p = panel(vec![e(1, Kind::Save, None, 1)]);
        assert_eq!(act(&mut p, HistoryAction::Files), ["scan /w"]);
        let mut files = super::super::project::tests::fixture();
        files.push(super::super::project::tests::file("a.txt", 0, &[], crate::history::FileState::Present));
        with_cx(|cx| p.set_files(Path::new("/w"), files, Path::new("/w"), |_| 0, cx));
        assert_eq!(p.selected_title(), "2024-10-04 00:00:00  Saved");
        assert!(p.shows(Path::new("/w/a.txt"), &e(0, Kind::Save, None, 1).blob));
        // Another folder's answer is dropped.
        with_cx(|cx| p.set_files(Path::new("/w/d"), Vec::new(), Path::new("/w"), |_| 0, cx));
        assert!(p.shows(Path::new("/w/a.txt"), &e(0, Kind::Save, None, 1).blob));
    }

    #[test]
    fn scans_are_coalesced_and_f5_always_asks() {
        let mut p = LocalHistoryPanel::default();
        assert_eq!(requests(with_cx(|cx| p.show_project(PathBuf::from("/w"), cx))), ["scan /w"]);
        assert!(requests(with_cx(|cx| p.rescan(cx))).is_empty(), "one in flight");
        assert!(requests(with_cx(|cx| p.rescan(cx))).is_empty());
        // The answer lands: once more for the requests meanwhile.
        let fx = requests(with_cx(|cx| p.set_files(Path::new("/w"), Vec::new(), Path::new("/w"), |_| 0, cx)));
        assert_eq!(fx, ["scan /w"]);
        assert!(requests(with_cx(|cx| p.rescan(cx))).is_empty());
        assert_eq!(act(&mut p, HistoryAction::Refresh), ["scan /w"]);
        assert!(p.project_shows(Path::new("/w/x/y.txt")) && !p.project_shows(Path::new("/v/y.txt")));
    }

    #[test]
    fn project_view_actions_work_on_the_selected_row() {
        let mut p = project_panel();
        // A deleted file: its newest snapshot is put back.
        act(&mut p, HistoryAction::Down);
        assert_eq!(act(&mut p, HistoryAction::Revert), ["revert 0007"]);
        let only = act(&mut p, HistoryAction::RemoveLabel);
        assert!(only[0].starts_with("notify Only labels can be removed"), "{only:?}");
        assert_eq!(act(&mut p, HistoryAction::PutLabel), ["label /w/b.txt"]);
        // A label of two files: removed from both, reverted as a whole.
        act(&mut p, HistoryAction::Down);
        assert_eq!(act(&mut p, HistoryAction::PutLabel), ["label /w"], "a label row: the scanned folder");
        assert_eq!(act(&mut p, HistoryAction::RemoveLabel), ["remove v1 of /w/b.txt /w/src/a.rs"]);
        assert_eq!(act(&mut p, HistoryAction::Revert), ["revert label v1 of 2"]);
        assert!(p.diff.key.is_none(), "no diff for a label");
        // One of its files: that file's label only.
        act(&mut p, HistoryAction::OpenFile);
        act(&mut p, HistoryAction::Down);
        assert_eq!(act(&mut p, HistoryAction::RemoveLabel), ["remove v1"]);
        assert_eq!(act(&mut p, HistoryAction::Revert), ["revert 0005"]);
        // A rename of the folder shown is followed; the root switch starts over.
        assert!(p.renamed(Path::new("/w"), Path::new("/v"), Path::new("/")));
        assert_eq!(p.project_scope(), Some(Path::new("/v")));
        assert!(p.reroot(Path::new("/r")));
        assert_eq!(p.project_scope(), Some(Path::new("/r")));
    }

    #[test]
    fn a_rename_takes_the_file_or_its_folder_along() {
        let mut p = LocalHistoryPanel::default();
        p.set_target(PathBuf::from("/w/d/a.txt"), Path::new("/w"));
        assert_eq!(p.retarget(Path::new("/w/d/a.txt"), Path::new("/w/d/b.txt"), Path::new("/w")), Some(PathBuf::from("/w/d/b.txt")));
        assert_eq!(p.retarget(Path::new("/w/d"), Path::new("/w/e"), Path::new("/w")), Some(PathBuf::from("/w/e/b.txt")));
        assert_eq!(p.target.as_ref().map(|t| t.rel.as_str()), Some("e/b.txt"));
        // A sibling whose name starts the same, another file: not followed.
        assert_eq!(p.retarget(Path::new("/w/e/b"), Path::new("/w/x"), Path::new("/w")), None);
        assert_eq!(p.retarget(Path::new("/w/e/c.txt"), Path::new("/w/x"), Path::new("/w")), None);
        assert_eq!(p.target(), Some(Path::new("/w/e/b.txt")));
        // Another file starts over; the same one keeps its filter.
        let mut p = panel(vec![e(1, Kind::Save, None, 1)]);
        typed(&mut p, KeyCode::Char('s'));
        p.set_target(PathBuf::from("/w/a.txt"), Path::new("/w"));
        assert_eq!((p.filter(), p.rows.len()), ("s", 1));
        p.set_target(PathBuf::from("/w/b.txt"), Path::new("/w"));
        assert_eq!((p.filter(), p.rows.len(), p.loading), ("", 0, true));
    }
}
