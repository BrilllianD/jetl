//! Editor: a tab strip of buffers plus the text view.

pub mod blame;
pub mod brackets;
pub mod buffer;
pub mod expand;
mod find;
pub mod format;
pub mod groups;
pub mod gutter;
pub mod indent;
mod io;
pub use io::DiskChange;
pub mod nav;
pub mod search;
pub mod selection;
pub mod split;
pub mod syntax;
pub mod undo;
pub mod view;
pub mod view_state;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::actions::EditorAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::config::{Autosave, Config};
use crate::event::{Effect, Level, Mouse};
use crate::ui::dialog::{Dialog, DialogId};
use buffer::{Buffer, BufferId};
use find::{LastSearch, refresh_search, render_find_bar};
use groups::{GroupId, TabGroup};
use nav::{NavPos, NavRing};
use search::Field;
use selection::{Selection, apply_motion, line_range, word_at};
use split::{SplitNode, SplitterHit};
use undo::TxKind;
use view::{RenderOpts, col_to_char};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Char,
    Word(usize, usize),
    Line(usize, usize),
}

pub struct Editor {
    /// The open documents; tab order lives in the groups.
    pub buffers: Vec<Buffer>,
    /// The panes; at least one, each a leaf of `tree`.
    groups: Vec<TabGroup>,
    tree: SplitNode,
    /// The bars between the groups as last drawn.
    splitters: Vec<SplitterHit>,
    /// The group keys and the mouse act on.
    focused: GroupId,
    next_group: GroupId,
    /// Most recently used buffer ids, most recent first.
    mru: Vec<BufferId>,
    next_id: BufferId,
    tab_width: usize,
    scrolloff: usize,
    hscrolloff: usize,
    auto_close: bool,
    highlight_occurrences: bool,
    trim_trailing_whitespace: bool,
    final_newline: bool,
    autosave: Autosave,
    autosave_idle: Duration,
    /// When a buffer version last changed, as seen by [`Editor::autosave_due`].
    last_edit: Instant,
    /// A text drag and the group it started in.
    drag: Option<(GroupId, DragMode)>,
    /// Files larger than this open without syntax highlighting.
    max_highlight_bytes: usize,
    /// `editor.max_tabs`: see [`Editor::enforce_tab_limit`].
    max_tabs: usize,
    /// `[format]`: formatters per language and whether saves run them.
    pub format: crate::config::FormatConfig,
    last_search: Option<LastSearch>,
    /// Back/forward history of caret positions.
    nav: NavRing,
    /// A closed file being reopened by back/forward and the caret to put
    /// in it once it loads.
    pending_nav: Option<(PathBuf, usize)>,
}

/// Shown in place of a pinned tab's close button (one column wide).
const PIN_MARK: &str = "⚲";

/// Indent dialog items: 0 is a tab, else that many spaces.
const INDENT_CHOICES: [usize; 4] = [0, 2, 4, 8];

/// A jump moving the caret by more than this many lines is remembered for
/// back/forward.
const NAV_JUMP_LINES: usize = 10;

fn nav_pos(b: &Buffer) -> NavPos {
    NavPos { id: b.id, path: b.path.clone(), pos: b.view.sel.head }
}

impl Editor {
    pub fn new(config: &Config) -> Self {
        Self {
            buffers: Vec::new(),
            groups: vec![TabGroup::new(0)],
            tree: SplitNode::Leaf(0),
            splitters: Vec::new(),
            focused: 0,
            next_group: 1,
            mru: Vec::new(),
            next_id: 1,
            tab_width: config.editor.tab_width,
            scrolloff: config.editor.scrolloff,
            hscrolloff: config.editor.hscrolloff,
            auto_close: config.editor.auto_close,
            highlight_occurrences: config.editor.highlight_occurrences,
            trim_trailing_whitespace: config.editor.trim_trailing_whitespace,
            final_newline: config.editor.final_newline,
            autosave: config.editor.autosave,
            autosave_idle: Duration::from_millis(config.editor.autosave_idle_ms),
            last_edit: Instant::now(),
            drag: None,
            max_highlight_bytes: config.editor.max_highlight_bytes,
            max_tabs: config.editor.max_tabs,
            format: config.format.clone(),
            last_search: None,
            nav: NavRing::default(),
            pending_nav: None,
        }
    }

    pub fn active_buffer(&self) -> Option<&Buffer> {
        self.active().and_then(|i| self.buffers.get(i))
    }
    pub fn active_buffer_mut(&mut self) -> Option<&mut Buffer> {
        self.active().and_then(move |i| self.buffers.get_mut(i))
    }
    pub fn active_path(&self) -> Option<&Path> {
        self.active_buffer().and_then(|b| b.path.as_deref())
    }
    pub fn has_dirty(&self) -> bool {
        self.buffers.iter().any(|b| b.dirty())
    }
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.buffers.iter().position(|b| b.path.as_deref() == Some(path))
    }
    pub fn index_of_id(&self, id: BufferId) -> Option<usize> {
        self.buffers.iter().position(|b| b.id == id)
    }
    pub fn open_paths(&self) -> Vec<PathBuf> {
        self.buffers.iter().filter_map(|b| b.path.clone()).collect()
    }
    pub fn recent_paths(&self) -> Vec<PathBuf> {
        self.mru.iter().filter_map(|id| self.buffers.iter().find(|b| b.id == *id)).filter_map(|b| b.path.clone()).collect()
    }

    /// Add a loaded file as a new tab and make it active. The language
    /// comes from the path, else a shebang or modeline; its highlighting is
    /// attached only below `max_highlight_bytes`. The
    /// indent and save options come from `.editorconfig` (looked up here,
    /// once per open: a failed open per ancestor directory without one),
    /// the text and the config.
    pub fn open_text(&mut self, path: PathBuf, text: &str, lossy: bool, theme: &Theme) -> usize {
        if let Some(i) = self.index_of(&path) {
            self.activate_index(i);
            return i;
        }
        let id = self.next_id;
        self.next_id += 1;
        // Only the first and last lines are read for a shebang or modeline.
        let lang = syntax::detect_text(&path, text);
        let syntax = lang.filter(|_| text.len() <= self.max_highlight_bytes).and_then(|d| syntax::Syntax::new(d, theme));
        let mtime = buffer::file_mtime(&path);
        let pending = self.pending_nav.take_if(|(p, _)| *p == path).map(|(_, pos)| pos);
        let settings = indent::FileSettings::for_path(&path);
        let mut buf = Buffer::from_text(id, Some(path), text, lossy);
        buf.syntax = syntax;
        buf.detected_language = lang;
        buf.disk_mtime = mtime;
        buf.indent = settings.indent(&buf.rope, self.tab_width);
        // A file not on disk yet has no line ending of its own.
        if let Some(le) = settings.end_of_line.filter(|_| mtime.is_none() && text.is_empty()) {
            buf.line_ending = le;
        }
        buf.settings = settings;
        self.buffers.push(buf);
        let at = self.buffers.len() - 1;
        match pending {
            // Reopened by back/forward, which already updated the history.
            Some(pos) => {
                self.set_active(at);
                self.buffers[at].goto_pos(pos);
            }
            None => self.activate_index(at),
        }
        at
    }

    /// Put back a session's caret and scroll for tab `i` (clamped to the
    /// file as it is now). The first render still keeps the caret visible.
    pub fn restore_view(&mut self, i: usize, line: usize, col: usize, scroll_line: usize, scroll_col: usize) {
        let Some(b) = self.buffers.get_mut(i) else { return };
        b.goto_line(line, col);
        b.view.scroll_line = scroll_line.min(b.rope.len_lines().saturating_sub(1));
        b.view.scroll_col = scroll_col;
    }

    /// Restore tab recency from a session (`recent` most recent first; tabs
    /// not listed keep their order after them), then activate `active`.
    pub fn restore_recent(&mut self, recent: &[PathBuf], active: Option<&Path>) {
        let mut ids: Vec<BufferId> = recent.iter().filter_map(|p| self.index_of(p)).map(|i| self.buffers[i].id).collect();
        for id in &self.mru {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        self.mru = ids;
        if let Some(i) = active.and_then(|p| self.index_of(p)) {
            self.set_active(i);
        }
    }

    /// Forget the back/forward history (a session was restored or the root
    /// changed).
    pub fn clear_nav(&mut self) {
        self.nav.clear();
        self.pending_nav = None;
    }

    /// Remember the active buffer's caret for back/forward (before a jump
    /// that may not move far, such as go to definition).
    pub fn push_nav_here(&mut self) {
        if let Some(here) = self.active_buffer().map(nav_pos) {
            self.nav.push(here);
        }
    }

    /// Remember the active buffer's caret when `from` (taken before an
    /// action) is more than [`NAV_JUMP_LINES`] lines from it.
    fn note_jump(&mut self, from: Option<NavPos>) {
        let (Some(from), Some(b)) = (from, self.active_buffer()) else { return };
        if from.id != b.id {
            return;
        }
        let line = |p: usize| b.rope.char_to_line(p.min(b.rope.len_chars()));
        if line(from.pos).abs_diff(line(b.view.sel.head)) > NAV_JUMP_LINES {
            self.nav.push(from);
        }
    }

    /// Ctrl+Alt+Left / Ctrl+Alt+Right: return to the previous (next)
    /// position, reopening its file when it was closed.
    fn nav_step(&mut self, back: bool, cx: &mut Ctx) {
        let here = self.active_buffer().map(nav_pos);
        let buffers = &self.buffers;
        let usable = |p: &NavPos| p.path.is_some() || buffers.iter().any(|b| b.id == p.id);
        let target = if back { self.nav.back(here, usable) } else { self.nav.forward(here, usable) };
        let Some(p) = target else {
            cx.notify(Level::Info, if back { "No earlier position" } else { "No later position" });
            return;
        };
        let found = self.buffers.iter().position(|b| b.id == p.id).or_else(|| p.path.as_deref().and_then(|path| self.index_of(path)));
        match (found, p.path) {
            (Some(i), _) => {
                self.set_active(i);
                self.buffers[i].goto_pos(p.pos);
            }
            (None, Some(path)) => {
                self.pending_nav = Some((path.clone(), p.pos));
                cx.effects.push(Effect::OpenFile(path));
            }
            (None, None) => {}
        }
    }

    /// Shift+F11: a choice dialog of the bookmarks of all open buffers.
    fn show_bookmarks(&mut self, cx: &mut Ctx) {
        let root = cx.workspace.root();
        let mut targets = Vec::new();
        let mut items = Vec::new();
        for b in &self.buffers {
            let name = b
                .path
                .as_deref()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| b.name.clone());
            for &line in &b.bookmarks {
                items.push(format!("{name}:{} {}", line + 1, b.line_text(line).trim()));
                targets.push((b.id, line));
            }
        }
        if items.is_empty() {
            cx.notify(Level::Info, "No bookmarks; F11 toggles one on the caret line");
            return;
        }
        cx.effects.push(Effect::Dialog(Dialog::choice(DialogId::Bookmarks(targets), "Bookmarks", "Go to bookmark:", items)));
    }

    /// Jump to bookmarked `line` of buffer `id` (from the bookmarks dialog);
    /// the position left is remembered for back.
    pub fn goto_bookmark(&mut self, id: BufferId, line: usize) {
        let Some(i) = self.buffers.iter().position(|b| b.id == id) else { return };
        self.push_nav_here();
        self.set_active(i);
        self.buffers[i].goto_line(line, 0);
    }

    /// Follow a rename of `from` (a file or a directory) to `to`: tabs of
    /// it or under it keep their text and history under the new path.
    pub fn retarget(&mut self, from: &Path, to: &Path) {
        for b in &mut self.buffers {
            if let Some(moved) = b.path.as_deref().and_then(|p| nav::moved_path(p, from, to)) {
                b.name = moved.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                b.path = Some(moved);
            }
        }
        if let Some((p, _)) = &mut self.pending_nav
            && let Some(moved) = nav::moved_path(p, from, to)
        {
            *p = moved;
        }
        self.nav.retarget(from, to);
    }

    /// Close the tabs of `path` or under it, unsaved edits included (the
    /// file was deleted).
    pub fn close_under(&mut self, path: &Path) -> usize {
        let mut closed = 0;
        while let Some(i) = self.buffers.iter().position(|b| b.path.as_deref().is_some_and(|p| p.starts_with(path))) {
            self.force_close(i);
            closed += 1;
        }
        closed
    }

    pub fn goto_line(&mut self, line: usize, col: usize) {
        let from = self.active_buffer().map(nav_pos);
        if let Some(b) = self.active_buffer_mut() {
            b.goto_line(line.saturating_sub(1), col.saturating_sub(1));
        }
        self.note_jump(from);
    }

    pub fn status_position(&self) -> Option<String> {
        let b = self.active_buffer()?;
        let (line, _) = b.cursor_line_col();
        let col = view::visual_col(&b.rope, b.view.sel.head, self.tab_display(b));
        let sel = b.view.sel.range();
        if sel.is_empty() {
            Some(format!("Ln {}, Col {}", line + 1, col + 1))
        } else {
            Some(format!("Ln {}, Col {} ({} chars)", line + 1, col + 1, sel.len()))
        }
    }

    pub fn status_line_ending(&self) -> Option<String> {
        self.active_buffer().map(|b| b.line_ending.label().to_string())
    }

    /// Indentation of the active buffer for the status bar: "Tab" or
    /// "N spaces".
    pub fn status_indent(&self) -> Option<String> {
        self.active_buffer().map(|b| b.indent.label())
    }

    /// Columns per tab stop when drawing buffer `b`: `editor.tab_width`
    /// unless its `.editorconfig` sets one.
    fn tab_display(&self, b: &Buffer) -> usize {
        b.settings.tab_display(b.indent).unwrap_or(self.tab_width)
    }

    /// Indent choices for the active buffer (status bar click); the items
    /// match [`INDENT_CHOICES`].
    pub fn indent_dialog(&self) -> Option<Dialog> {
        let b = self.active_buffer()?;
        let items = INDENT_CHOICES.iter().map(|w| if *w == 0 { "Tab".to_string() } else { format!("{w} spaces") }).collect();
        Some(Dialog::choice(DialogId::Indent(b.id), "Indent", &format!("Indent with (now {}):", b.indent.label()), items))
    }

    /// Set buffer `id`'s indent to item `i` of [`Editor::indent_dialog`];
    /// tabs keep the buffer's tab display width. The text is left as is.
    pub fn set_indent(&mut self, id: BufferId, i: usize) {
        let Some(&w) = INDENT_CHOICES.get(i) else { return };
        let Some(bi) = self.buffers.iter().position(|b| b.id == id) else { return };
        let tab = self.tab_display(&self.buffers[bi]);
        self.buffers[bi].indent = if w == 0 { indent::IndentStyle::tabs(tab) } else { indent::IndentStyle::spaces(w) };
    }

    pub fn status_language(&self) -> Option<String> {
        self.active_buffer().map(|b| b.language().map_or("Plain text", |d| d.name).to_string())
    }

    /// Choice of LF / CRLF for the active buffer (status bar click).
    pub fn line_ending_dialog(&self) -> Option<Dialog> {
        let b = self.active_buffer()?;
        let msg = format!("Convert line endings (now {}):", b.line_ending.label());
        Some(Dialog::choice(DialogId::LineEnding(b.id), "Line Separators", &msg, vec!["LF".into(), "CRLF".into()]))
    }

    /// Convert buffer `id` to line ending `le` as one undoable edit.
    pub fn set_line_ending(&mut self, id: BufferId, le: buffer::LineEnding) {
        if let Some(b) = self.buffers.iter_mut().find(|b| b.id == id) {
            b.convert_line_endings(le);
        }
    }

    /// Choice of plain text and every known language for the active buffer.
    pub fn language_dialog(&self) -> Option<Dialog> {
        let b = self.active_buffer()?;
        let now = b.language().map_or("Plain text", |d| d.name);
        let items = std::iter::once("Plain text".to_string()).chain(syntax::LANGUAGES.iter().map(|l| l.name.to_string())).collect();
        Some(Dialog::choice(DialogId::Language(b.id), "Language", &format!("Highlight as (now {now}):"), items))
    }

    /// Override the language of buffer `id` (`None` is plain text). Files
    /// above `max_highlight_bytes` get the language but no highlighting.
    pub fn set_language(&mut self, id: BufferId, def: Option<&'static syntax::LanguageDef>, theme: &Theme) {
        let max = self.max_highlight_bytes;
        let Some(b) = self.buffers.iter_mut().find(|b| b.id == id) else { return };
        let syn = def.filter(|_| b.rope.len_bytes() <= max).and_then(|d| syntax::Syntax::new(d, theme));
        b.set_language(def, syn);
    }

    /// Set the HEAD text the gutter compares against; marks are recomputed
    /// on the next [`Editor::update_gutters`].
    /// HEAD text of `path` from blob request `seq`; an older request that
    /// finishes after a newer one is ignored.
    pub fn set_diff_base(&mut self, path: &Path, seq: u64, text: Option<String>) {
        let Some(i) = self.index_of(path) else { return };
        let b = &mut self.buffers[i];
        if seq < b.diff_base_seq {
            return;
        }
        b.diff_base_seq = seq;
        if text.is_none() {
            b.gutter.clear();
        }
        b.diff_base = text;
        b.gutter_version = u64::MAX;
    }

    /// Show or hide the blame column of the active buffer; showing it asks
    /// for the blame (`GitRequest::LoadBlame`).
    pub fn toggle_blame(&mut self, cx: &mut Ctx) {
        let Some(b) = self.active_buffer_mut() else { return };
        if b.blame.take().is_some() {
            return;
        }
        let Some(path) = b.path.clone() else {
            cx.notify(Level::Info, "Blame needs a file");
            return;
        };
        b.blame = Some(blame::BlameState::new());
        cx.effects.push(Effect::Git(crate::git::GitRequest::LoadBlame(path)));
    }

    /// Paths of buffers showing blame (reloaded when HEAD moves).
    pub fn blame_paths(&self) -> Vec<PathBuf> {
        self.buffers.iter().filter(|b| b.blame.is_some()).filter_map(|b| b.path.clone()).collect()
    }

    /// A blame result for `path` from request `seq`; an older request that
    /// finishes after a newer one is ignored. An error hides the column and
    /// comes back for a notification.
    pub fn set_blame(&mut self, path: &Path, seq: u64, result: anyhow::Result<crate::git::Blame>) -> Option<String> {
        let i = self.index_of(path)?;
        let b = &mut self.buffers[i];
        let state = b.blame.as_mut()?;
        if seq < state.seq {
            return None;
        }
        state.seq = seq;
        match result {
            Ok(data) => {
                state.data = Some(data);
                state.map_version = u64::MAX;
                b.gutter_version = u64::MAX;
                None
            }
            Err(e) => {
                b.blame = None;
                Some(format!("Blame: {e:#}"))
            }
        }
    }

    /// Recompute gutter marks (and blame line maps) of buffers edited since
    /// the last call (run from the app tick, which debounces typing).
    /// Returns true when any changed.
    pub fn update_gutters(&mut self) -> bool {
        let limit = self.max_highlight_bytes;
        let mut changed = false;
        for b in &mut self.buffers {
            if let (Some(state), Some(base)) = (&b.blame, &b.diff_base)
                && state.data.is_some()
                && state.map_version != b.version
            {
                let map = if b.rope.len_bytes() <= limit && base.len() <= limit { blame::line_map(base, &b.text()) } else { Vec::new() };
                let version = b.version;
                if let Some(state) = &mut b.blame {
                    (state.map, state.map_version) = (map, version);
                }
                changed = true;
            }
            if b.gutter_version == b.version {
                continue;
            }
            b.gutter_version = b.version;
            let marks = match &b.diff_base {
                Some(base) if b.rope.len_bytes() <= limit && base.len() <= limit => gutter::compute(base, &b.text()),
                _ => Vec::new(),
            };
            if marks != b.gutter {
                b.gutter = marks;
                changed = true;
            }
        }
        changed
    }

    /// Re-read unmodified buffers whose file changed on disk (after a branch
    /// switch or pull). The reload is one undoable transaction. Returns the
    /// number of reloaded buffers.
    pub fn reload_clean_from_disk(&mut self) -> usize {
        let mut n = 0;
        for b in &mut self.buffers {
            let Some(path) = b.path.clone() else { continue };
            if b.dirty() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(text) = String::from_utf8(bytes) else { continue };
            let changed = b.text() != text;
            io::replace_with_disk_text(b, text, &path);
            n += usize::from(changed);
        }
        n
    }

    pub fn handle_action(&mut self, a: &EditorAction, cx: &mut Ctx) -> bool {
        use EditorAction as E;
        match a {
            E::NextTab | E::PrevTab => {
                let (g, group) = (self.focused, self.focused_group());
                if let Some(i) = group.active {
                    let n = group.views.len();
                    self.activate_slot(g, if *a == E::NextTab { (i + 1) % n } else { (i + n - 1) % n });
                }
                return true;
            }
            E::CloseTab => {
                if let Some(slot) = self.focused_group().active {
                    self.close_slot(self.focused, slot, cx);
                }
                return true;
            }
            E::CloseAllTabs => {
                // With one group that is every buffer, asked about at once.
                if self.groups.len() == 1 {
                    self.close_all(cx);
                } else {
                    let ids = self.focused_group().views.iter().map(|v| v.id).collect();
                    self.close_tabs(ids, cx);
                }
                return true;
            }
            E::CloseOtherTabs => {
                let group = self.focused_group();
                let ids =
                    group.views.iter().enumerate().filter(|(i, v)| !v.pinned && Some(*i) != group.active).map(|(_, v)| v.id).collect();
                self.close_tabs(ids, cx);
                return true;
            }
            E::CloseUnpinnedTabs => {
                let ids = self.focused_group().views.iter().filter(|v| !v.pinned).map(|v| v.id).collect();
                self.close_tabs(ids, cx);
                return true;
            }
            E::TogglePinTab => {
                if let Some(slot) = self.focused_group().active {
                    let pinned = self.is_pinned_slot(self.focused, slot);
                    self.set_pinned_slot(self.focused, slot, !pinned);
                }
                return true;
            }
            E::MoveTabLeft | E::MoveTabRight => {
                self.step_tab(*a == E::MoveTabRight);
                return true;
            }
            E::SplitRight | E::SplitDown => {
                self.split(if *a == E::SplitRight { split::Axis::Horizontal } else { split::Axis::Vertical }, cx);
                return true;
            }
            E::NextGroup | E::PrevGroup => {
                self.cycle_group(*a == E::NextGroup);
                return true;
            }
            E::Unsplit => {
                self.unsplit(self.focused);
                return true;
            }
            E::UnsplitAll => {
                self.unsplit_all();
                return true;
            }
            E::MoveTabToOppositeGroup => {
                self.move_to_opposite(cx);
                return true;
            }
            E::Switcher => {
                if let Some(id) = self.mru.get(1).copied()
                    && let Some(i) = self.buffers.iter().position(|b| b.id == id)
                {
                    self.activate_index(i);
                }
                return true;
            }
            E::SaveAll => {
                self.save_all_with(cx, self.format.on_save);
                return true;
            }
            E::Save => {
                if let Some(i) = self.active() {
                    self.save_with(i, cx, self.format.on_save);
                }
                return true;
            }
            E::Format => {
                self.format_active(cx);
                return true;
            }
            E::GotoLine => {
                if self.active().is_some() {
                    cx.effects.push(Effect::Dialog(Dialog::input(DialogId::GotoLine, "Go to Line", "[Line] [:column]:", "", "OK")));
                }
                return true;
            }
            E::NavBack | E::NavForward => {
                self.nav_step(*a == E::NavBack, cx);
                return true;
            }
            E::ShowBookmarks => {
                self.show_bookmarks(cx);
                return true;
            }
            E::LineEndingLf | E::LineEndingCrlf => {
                if let Some(id) = self.active_buffer().map(|b| b.id) {
                    self.set_line_ending(id, if *a == E::LineEndingLf { buffer::LineEnding::Lf } else { buffer::LineEnding::CrLf });
                }
                return true;
            }
            E::ToggleBlame => {
                self.toggle_blame(cx);
                return true;
            }
            E::SetLanguage => {
                if let Some(d) = self.language_dialog() {
                    cx.effects.push(Effect::Dialog(d));
                }
                return true;
            }
            _ => {}
        }
        // Jumps far enough away are remembered for back/forward.
        let jump_from = if matches!(a, E::GotoBracket | E::FindNext | E::FindPrev | E::NextChange | E::PrevChange) {
            self.active_buffer().map(nav_pos)
        } else {
            None
        };
        let rows = self.text_area().height as usize;
        let auto_close = self.auto_close;
        let Some(ai) = self.active() else { return false };
        let b = &mut self.buffers[ai];
        // Any other action ends an expand/shrink sequence; scrolling and
        // copying leave the selection alone, so they keep it.
        if !matches!(a, E::ExpandSelection | E::ShrinkSelection | E::Copy | E::ScrollUp(_) | E::ScrollDown(_)) {
            b.view.expand_stack.clear();
        }
        let pairs = || if auto_close { brackets::pairs(b.language().map(|d| d.name)) } else { &[] };
        match a {
            E::InsertChar(c) => {
                let pairs = pairs();
                if let Some(s) = &mut b.syntax {
                    s.reparse(&b.rope);
                }
                let tree = b.syntax.as_ref().and_then(|s| s.tree());
                let may_pair = !tree.is_some_and(|t| brackets::in_literal_at(&b.rope, t, b.view.sel.head));
                // A closer that auto-close steps over keeps the line as is.
                let steps_over =
                    pairs.iter().any(|(_, cl)| cl == c) && b.view.sel.head < b.rope.len_chars() && b.rope.char(b.view.sel.head) == *c;
                match b.closer_indent(*c, tree).filter(|_| !steps_over) {
                    Some(indent) => b.type_dedented(*c, &indent),
                    None => b.type_char(*c, pairs, may_pair),
                }
            }
            E::InsertText(s) => {
                let s = if b.line_ending == buffer::LineEnding::CrLf {
                    s.replace("\r\n", "\n").replace('\n', "\r\n")
                } else {
                    s.replace("\r\n", "\n")
                };
                b.insert(&s, TxKind::Other);
            }
            E::InsertNewline => {
                let colon = b.language().is_some_and(|d| d.indent_after_colon);
                b.newline_smart(colon);
            }
            E::InsertTab => {
                let r = b.view.sel.range();
                if b.rope.char_to_line(r.start) != b.rope.char_to_line(r.end) {
                    b.indent_lines();
                } else {
                    b.insert_tab();
                }
            }
            E::Unindent => b.unindent_lines(),
            E::MoveLineUp => b.move_lines(true),
            E::MoveLineDown => b.move_lines(false),
            E::JoinLines => b.join_lines(),
            E::ToggleCase => b.toggle_case(),
            E::Backspace => {
                if !b.backspace_pair(pairs()) {
                    b.backspace();
                }
            }
            E::Delete => b.delete_forward(),
            E::DeleteWordBack => b.delete_word_back(),
            E::DeleteWordForward => b.delete_word_forward(),
            E::Move(m, extend) => {
                let page = rows.saturating_sub(1).max(1);
                apply_motion(&b.rope, &mut b.view.sel, *m, *extend, &mut b.view.desired_col, page);
                b.undo.break_group = true;
            }
            E::Undo => {
                if !b.undo() {
                    cx.notify(Level::Info, "Nothing to undo");
                }
            }
            E::Redo => {
                if !b.redo() {
                    cx.notify(Level::Info, "Nothing to redo");
                }
            }
            E::DuplicateLine => b.duplicate_line(),
            E::DeleteLine => b.delete_line(),
            E::SelectAll => b.select_all(),
            E::SelectLine => b.select_line(),
            E::Copy => {
                let text =
                    if b.view.sel.is_empty() { b.line_text(b.cursor_line_col().0) + b.line_ending.as_str() } else { b.selected_text() };
                cx.effects.push(Effect::Clipboard(text));
            }
            E::Cut => {
                if b.view.sel.is_empty() {
                    let (line, _) = b.cursor_line_col();
                    let r = line_range(&b.rope, line);
                    b.view.sel = Selection::new(r.start, r.end);
                }
                cx.effects.push(Effect::Clipboard(b.selected_text()));
                b.delete_selection();
            }
            // The app reads the clipboard (`App::paste`) and inserts it as
            // `InsertText`.
            E::Paste => {}
            E::Escape => {
                if b.view.search.is_some() {
                    self.close_find();
                } else {
                    b.view.sel.collapse();
                }
            }
            E::ScrollUp(n) => b.view.scroll_line = b.view.scroll_line.saturating_sub(*n),
            E::ScrollDown(n) => b.view.scroll_line = (b.view.scroll_line + n).min(b.rope.len_lines().saturating_sub(1)),
            E::Find => self.open_find(false),
            E::Replace => self.open_find(true),
            E::FindNext => self.find_step(true, cx),
            E::FindPrev => self.find_step(false, cx),
            E::ReplaceOne => self.replace_one(cx),
            E::ReplaceAll => self.replace_all(cx),
            E::FindToggleCase | E::FindToggleWord | E::FindToggleRegex => {
                if let Some(s) = &mut b.view.search {
                    match a {
                        E::FindToggleCase => s.case_sensitive ^= true,
                        E::FindToggleWord => s.whole_word ^= true,
                        _ => s.regex ^= true,
                    }
                    refresh_search(b);
                    self.remember_search();
                }
            }
            E::FindToggleField => {
                if let Some(s) = &mut b.view.search {
                    if s.show_replace {
                        s.field = if s.field == Field::Query { Field::Replace } else { Field::Query };
                    }
                    s.focused = true;
                }
            }
            E::ToggleComment => {
                let lang = b.language();
                match (lang.and_then(|d| d.line_comment), lang.and_then(|d| d.block_comment)) {
                    (Some(t), _) => b.toggle_line_comment(t),
                    // CSS, HTML and XML have no line comment; JetBrains
                    // comments the lines with a block comment instead.
                    (None, Some((open, close))) => b.toggle_block_comment_lines(open, close),
                    (None, None) => cx.notify(Level::Info, "No line comment for this file type"),
                }
            }
            E::ToggleBlockComment => match b.language().and_then(|d| d.block_comment) {
                Some((open, close)) => b.toggle_block_comment(open, close),
                None => cx.notify(Level::Info, "No block comment for this file type"),
            },
            E::GotoBracket => {
                if let Some(s) = &mut b.syntax {
                    s.reparse(&b.rope);
                }
                let tree = b.syntax.as_ref().and_then(|s| s.tree());
                if let Some((_, other)) = brackets::pair_at(&b.rope, tree, b.view.sel.head) {
                    b.view.sel.set_head(other, false);
                    b.view.desired_col = None;
                    b.undo.break_group = true;
                }
            }
            E::RevertChange => {
                if b.diff_base.is_none() {
                    cx.notify(Level::Info, "No HEAD version to revert to");
                } else if !b.revert_change_at_caret() {
                    cx.notify(Level::Info, "No change at the caret");
                }
            }
            E::NextChange | E::PrevChange => {
                let forward = *a == E::NextChange;
                if !b.goto_change(forward) {
                    cx.notify(Level::Info, if forward { "No more changes below" } else { "No more changes above" });
                }
            }
            E::ExpandSelection => b.expand_selection(),
            E::ShrinkSelection => b.shrink_selection(),
            E::ToggleBookmark => b.toggle_bookmark(),
            E::NextTab
            | E::PrevTab
            | E::CloseTab
            | E::CloseAllTabs
            | E::CloseOtherTabs
            | E::CloseUnpinnedTabs
            | E::TogglePinTab
            | E::MoveTabLeft
            | E::MoveTabRight
            | E::Switcher
            | E::SplitRight
            | E::SplitDown
            | E::NextGroup
            | E::PrevGroup
            | E::Unsplit
            | E::UnsplitAll
            | E::MoveTabToOppositeGroup
            | E::Save
            | E::SaveAll
            | E::Format
            | E::GotoLine
            | E::NavBack
            | E::NavForward
            | E::ShowBookmarks
            | E::LineEndingLf
            | E::LineEndingCrlf
            | E::SetLanguage
            | E::ToggleBlame => {}
        }
        self.note_jump(jump_from);
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            if self.focused_group().find_area.contains(ratatui::layout::Position::new(m.x, m.y)) {
                self.click_find_bar(m, cx);
                return true;
            }
            // A click in the text hands keyboard focus back to the text.
            if let Some(s) = self.active_buffer_mut().and_then(|b| b.view.search.as_mut()) {
                s.focused = false;
            }
        }
        let (g, text_area) = (self.focused, self.text_area());
        let Some(ai) = self.active() else { return false };
        let tab_width = self.tab_display(&self.buffers[ai]);
        let b = &mut self.buffers[ai];
        let pos_at = |b: &Buffer, x: u16, y: u16| -> usize {
            let row = y.saturating_sub(text_area.y) as usize;
            let line = (b.view.scroll_line + row).min(b.rope.len_lines().saturating_sub(1));
            let col = x.saturating_sub(text_area.x) as usize + b.view.scroll_col;
            if x < text_area.x { b.rope.line_to_char(line) } else { col_to_char(&b.rope, line, col, tab_width) }
        };
        // A click in the blame column shows that line's commit.
        if m.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(blame) = &b.blame
            && m.x < text_area.x.saturating_sub(view::gutter_width(b)) + blame::WIDTH
            && m.y >= text_area.y
            && m.y < text_area.bottom()
        {
            let line = b.view.scroll_line + (m.y - text_area.y) as usize;
            if let Some(c) = blame.commit_at(line) {
                cx.effects.push(Effect::Git(crate::git::GitRequest::ShowCommit(c.sha.clone())));
            }
            return true;
        }
        // A click on a git mark offers the change commands for that line.
        if m.kind == MouseEventKind::Down(MouseButton::Left) && m.x + 2 == text_area.x && m.y >= text_area.y && m.y < text_area.bottom() {
            let line = b.view.scroll_line + (m.y - text_area.y) as usize;
            if gutter::mark_at(&b.gutter, line).is_some() {
                b.view.sel = Selection::point(b.rope.line_to_char(line));
                b.view.desired_col = None;
                use crate::actions::{Action, EditorAction as E};
                use crate::ui::menu::{ContextMenu, MenuItem};
                let items = vec![
                    MenuItem::new("Revert Change", Action::Editor(E::RevertChange)),
                    MenuItem::new("Next Change", Action::Editor(E::NextChange)),
                    MenuItem::new("Previous Change", Action::Editor(E::PrevChange)),
                ];
                cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, m.x, m.y + 1)));
                return true;
            }
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let p = pos_at(b, m.x, m.y);
                let from = nav_pos(b);
                if b.rope.char_to_line(b.view.sel.head).abs_diff(b.rope.char_to_line(p)) > NAV_JUMP_LINES {
                    self.nav.push(from);
                }
                // Ctrl+click: the caret to the clicked char, then go to
                // what is defined there.
                if m.mods.contains(KeyModifiers::CONTROL) && m.clicks == 1 && m.x >= text_area.x {
                    b.view.sel = Selection::point(p);
                    b.view.desired_col = None;
                    b.undo.break_group = true;
                    cx.effects.push(Effect::Lsp(crate::lsp::LspRequest::Definition));
                    return true;
                }
                let extend = m.mods.contains(KeyModifiers::SHIFT);
                match m.clicks {
                    2 => {
                        let r = word_at(&b.rope, p);
                        b.view.sel = Selection::new(r.start, r.end);
                        self.drag = Some((g, DragMode::Word(r.start, r.end)));
                    }
                    3 => {
                        let (line, _) = selection::pos_to_line_col(&b.rope, p);
                        let r = line_range(&b.rope, line);
                        b.view.sel = Selection::new(r.start, r.end);
                        self.drag = Some((g, DragMode::Line(r.start, r.end)));
                    }
                    _ => {
                        b.view.sel.set_head(p, extend);
                        self.drag = Some((g, DragMode::Char));
                    }
                }
                b.view.desired_col = None;
                b.undo.break_group = true;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Only a drag that started in this group's text selects
                // (not one that started on a find bar button).
                let Some((_, mode)) = self.drag.filter(|(dg, _)| *dg == g) else { return true };
                let p = pos_at(b, m.x, m.y);
                match mode {
                    DragMode::Word(s, e) => {
                        let w = word_at(&b.rope, p);
                        if p < s { b.view.sel = Selection::new(e, w.start) } else { b.view.sel = Selection::new(s, w.end.max(e)) }
                    }
                    DragMode::Line(s, e) => {
                        let (line, _) = selection::pos_to_line_col(&b.rope, p);
                        let r = line_range(&b.rope, line);
                        if r.start < s { b.view.sel = Selection::new(e, r.start) } else { b.view.sel = Selection::new(s, r.end.max(e)) }
                    }
                    DragMode::Char => b.view.sel.set_head(p, true),
                }
                // Auto-scroll while dragging past the edges.
                if m.y < text_area.y {
                    b.view.scroll_line = b.view.scroll_line.saturating_sub(1);
                } else if m.y >= text_area.bottom() {
                    b.view.scroll_line = (b.view.scroll_line + 1).min(b.rope.len_lines().saturating_sub(1));
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                true
            }
            _ => wheel(b, m.kind),
        }
    }

    /// The wheel over group `g` when it is not the focused one: its view
    /// scrolls, focus stays.
    pub fn scroll_group(&mut self, g: GroupId, m: &Mouse) {
        let Some(slot) = self.group(g).and_then(|x| x.active) else { return };
        self.make_live(g, slot);
        if let Some(i) = self.group(g).and_then(TabGroup::active_id).and_then(|id| self.index_of_id(id)) {
            wheel(&mut self.buffers[i], m.kind);
        }
        if let Some(slot) = self.focused_group().active {
            self.make_live(self.focused, slot);
        }
    }

    /// The wheel over group `g`'s tab bar scrolls the tabs.
    pub fn handle_tab_mouse(&mut self, g: GroupId, m: &Mouse) -> bool {
        let Some(group) = self.group_mut(g) else { return false };
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft => {
                group.tab_scroll = group.tab_scroll.saturating_sub(1);
                true
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollRight => {
                group.tab_scroll = (group.tab_scroll + 1).min(group.views.len().saturating_sub(1));
                true
            }
            _ => false,
        }
    }

    /// Draw the groups over the centre: `tabs` (one row, none in zen mode)
    /// above `area`. Each group gets a tab bar of that height over its text.
    /// The focused group is drawn last, so its tab stays the live view of
    /// its buffer between frames.
    pub fn render(&mut self, frame: &mut Frame, tabs: Rect, area: Rect, focused: bool, theme: &Theme) {
        self.sync_parked();
        let center = Rect::new(area.x, tabs.y, area.width, tabs.height + area.height);
        let mut leaves = Vec::new();
        self.splitters.clear();
        self.tree.layout(center, &mut leaves, &mut self.splitters);
        let screen = frame.buffer_mut();
        for bar in &self.splitters {
            screen.set_style(bar.rect, theme.splitter);
            let (cells, glyph): (Vec<(u16, u16)>, _) = match bar.axis {
                split::Axis::Horizontal => ((bar.rect.y..bar.rect.bottom()).map(|y| (bar.rect.x, y)).collect(), "│"),
                split::Axis::Vertical => ((bar.rect.x..bar.rect.right()).map(|x| (x, bar.rect.y)).collect(), "─"),
            };
            for c in cells {
                screen[c].set_symbol(glyph);
            }
        }
        leaves.sort_by_key(|(g, _)| *g == self.focused);
        for (g, r) in leaves {
            let Some(gi) = self.group_index(g) else { continue };
            if let Some(slot) = self.groups[gi].active {
                self.make_live(g, slot);
            }
            let strip = tabs.height.min(r.height);
            let tabs_r = Rect::new(r.x, r.y, r.width, strip);
            let body = Rect::new(r.x, r.y + strip, r.width, r.height - strip);
            self.render_group(frame, gi, tabs_r, body, focused && g == self.focused, theme);
        }
    }

    /// Draw group `gi` (an index into `groups`): its tab bar in `tabs`, its
    /// shown tab in `area`. `focused` is whether keys go to it.
    fn render_group(&mut self, frame: &mut Frame, gi: usize, tabs: Rect, area: Rect, focused: bool, theme: &Theme) {
        let group = &mut self.groups[gi];
        render_tabs(group, &self.buffers, frame, tabs, theme);
        group.tab_area = tabs;
        group.last_area = Rect::new(area.x, tabs.y, area.width, tabs.height + area.height);
        group.caret_cell = None;
        group.find_hits.clear();
        group.find_area = Rect::default();
        let screen = frame.buffer_mut();
        screen.set_style(area, Style::default().bg(theme.bg));
        let Some(i) = group.active_id().and_then(|id| self.buffers.iter().position(|b| b.id == id)) else {
            group.text_area = area;
            let hint = "No files open. Alt+1 opens the Project view; Ctrl+Shift+N finds a file.";
            if area.height > 2 && area.width > 10 {
                let x = area.x + area.width.saturating_sub(hint.width() as u16) / 2;
                screen.set_stringn(x, area.y + area.height / 2, hint, area.width as usize, theme.dim);
            }
            return;
        };
        let opts = RenderOpts {
            focused,
            tab_width: self.tab_display(&self.buffers[i]),
            scrolloff: self.scrolloff,
            hscrolloff: self.hscrolloff,
            occurrences: self.highlight_occurrences,
            theme,
        };
        // The find bar takes the top rows; the text view gets the rest so
        // `text_area` (and with it mouse mapping) stays exact.
        let group = &mut self.groups[gi];
        let b = &mut self.buffers[i];
        let mut text_rect = area;
        let mut bar_focused = false;
        if let Some(s) = b.view.search.as_mut() {
            // Matches follow buffer edits; recomputed only on change.
            s.update(&b.rope, b.version);
            let h = if s.show_replace { 2 } else { 1 }.min(area.height);
            group.find_area = Rect::new(area.x, area.y, area.width, h);
            text_rect = Rect::new(area.x, area.y + h, area.width, area.height - h);
            bar_focused = focused && s.focused;
            group.find_hits = render_find_bar(frame, group.find_area, s, bar_focused, theme);
        }
        let out = view::render(b, frame.buffer_mut(), text_rect, &opts);
        group.text_area = out.text_area;
        group.caret_cell = out.cursor;
        if focused
            && !bar_focused
            && let Some((x, y)) = out.cursor
        {
            frame.set_cursor_position((x, y));
        }
    }
}

/// The wheel over a view: three lines up or down, four columns sideways.
fn wheel(b: &mut Buffer, kind: MouseEventKind) -> bool {
    let v = &mut b.view;
    match kind {
        MouseEventKind::ScrollUp => v.scroll_line = v.scroll_line.saturating_sub(3),
        MouseEventKind::ScrollDown => v.scroll_line = (v.scroll_line + 3).min(b.rope.len_lines().saturating_sub(1)),
        MouseEventKind::ScrollLeft => v.scroll_col = v.scroll_col.saturating_sub(4),
        MouseEventKind::ScrollRight => v.scroll_col += 4,
        _ => return false,
    }
    true
}

/// Draw `group`'s tab bar into `area` and record its hit rects by slot.
fn render_tabs(group: &mut TabGroup, buffers: &[Buffer], frame: &mut Frame, area: Rect, theme: &Theme) {
    let screen = frame.buffer_mut();
    screen.set_style(area, theme.tab);
    group.tab_hits.clear();
    if area.is_empty() || group.views.is_empty() {
        return;
    }
    let tabs: Vec<(&groups::View, &Buffer)> =
        group.views.iter().filter_map(|v| buffers.iter().find(|b| b.id == v.id).map(|b| (v, b))).collect();
    if tabs.len() != group.views.len() {
        return;
    }
    // Keep the active tab visible by advancing the scroll offset.
    let widths: Vec<u16> = tabs.iter().map(|(_, b)| b.name.width() as u16 + 5).collect();
    group.tab_scroll = group.tab_scroll.min(tabs.len() - 1);
    if let Some(a) = group.active {
        if a < group.tab_scroll {
            group.tab_scroll = a;
        }
        loop {
            let total: u16 = widths[group.tab_scroll..=a].iter().sum();
            if total <= area.width || group.tab_scroll >= a {
                break;
            }
            group.tab_scroll += 1;
        }
    }
    let mut x = area.x;
    if group.tab_scroll > 0 {
        screen.set_string(x, area.y, "‹", theme.dim);
        x += 1;
    }
    for (i, (v, b)) in tabs.iter().enumerate().skip(group.tab_scroll) {
        let w = widths[i];
        if x + w > area.right() {
            if x < area.right() {
                screen.set_string(area.right() - 1, area.y, "›", theme.dim);
            }
            break;
        }
        let active = group.active == Some(i);
        let style = if active { theme.tab_active } else { theme.tab };
        let rect = Rect::new(x, area.y, w, 1);
        screen.set_style(rect, style);
        let mut cx = x + 1;
        if b.dirty() {
            screen.set_string(cx, area.y, "*", theme.tab_dirty.bg(style.bg.unwrap_or(theme.panel_bg)));
        } else {
            screen.set_string(cx, area.y, " ", style);
        }
        cx += 1;
        screen.set_stringn(cx, area.y, &b.name, (area.right() - cx) as usize, style);
        cx += b.name.width() as u16 + 1;
        let close_style = if active { style } else { theme.dim.bg(theme.panel_bg) };
        // A pinned tab shows a pin in place of its close button; a click
        // on it unpins.
        screen.set_string(cx, area.y, if v.pinned { PIN_MARK } else { "×" }, close_style);
        group.tab_hits.close.push((i, Rect::new(cx, area.y, 1, 1)));
        group.tab_hits.tabs.push((i, rect));
        x += w;
    }
}

#[cfg(test)]
mod tests {
    use super::io::idle_autosave_due;
    use super::*;
    use crate::workspace::Workspace;
    use crossterm::event::{KeyCode, KeyEvent};

    /// Run `f` with a throwaway `Ctx`; returns the notifications it produced.
    fn with_cx(f: impl FnOnce(&mut Ctx)) -> Vec<String> {
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(PathBuf::from("/tmp"));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        f(&mut cx);
        cx.effects.into_iter().filter_map(|e| if let Effect::Notify(_, t) = e { Some(t) } else { None }).collect()
    }

    fn editor_with(name: &str, text: &str) -> Editor {
        let mut ed = Editor::new(&Config::default());
        ed.open_text(PathBuf::from(format!("/tmp/{name}")), text, false, &Theme::default());
        ed
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn find_bar_typing_selects_and_steps_with_wrap() {
        let mut ed = editor_with("a.txt", "foo bar foo baz foo");
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::Find, cx);
            assert!(ed.find_focused());
            for c in "foo".chars() {
                assert!(ed.handle_find_key(key(KeyCode::Char(c), KeyModifiers::NONE), cx));
            }
            assert_eq!(ed.active_buffer().unwrap().view.sel.range(), 0..3);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::NONE), cx));
            assert_eq!(ed.active_buffer().unwrap().view.sel.range(), 8..11);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::SHIFT), cx));
            assert_eq!(ed.active_buffer().unwrap().view.sel.range(), 0..3);
            // Shift+Enter from the first match wraps to the last.
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::SHIFT), cx));
            assert_eq!(ed.active_buffer().unwrap().view.sel.range(), 16..19);
            // Ctrl+S is not consumed by the bar.
            assert!(!ed.handle_find_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL), cx));
            // Alt+C toggles case sensitivity.
            assert!(ed.handle_find_key(key(KeyCode::Char('c'), KeyModifiers::ALT), cx));
            assert!(ed.active_buffer().unwrap().view.search.as_ref().unwrap().case_sensitive);
            // Esc closes the bar and leaves the match selected.
            assert!(ed.handle_find_key(key(KeyCode::Esc, KeyModifiers::NONE), cx));
            assert!(ed.active_buffer().unwrap().view.search.is_none());
            assert!(!ed.find_focused());
            // F3 with the bar closed reuses the last query.
            ed.handle_action(&EditorAction::FindNext, cx);
            assert_eq!(ed.active_buffer().unwrap().view.sel.range(), 0..3);
            assert!(ed.active_buffer().unwrap().view.search.is_none());
        });
        assert_eq!(notes.iter().filter(|n| *n == "Search wrapped").count(), 2);
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut ed = editor_with("a.txt", "foo foo foo");
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::Replace, cx);
            let b = ed.active_buffer_mut().unwrap();
            let s = b.view.search.as_mut().unwrap();
            s.query.set_text("foo");
            s.replace.set_text("quux");
            let before = b.undo.len();
            ed.handle_action(&EditorAction::ReplaceAll, cx);
            let b = ed.active_buffer_mut().unwrap();
            assert_eq!(b.text(), "quux quux quux");
            assert_eq!(b.undo.len(), before + 1);
            assert!(b.undo());
            assert_eq!(b.text(), "foo foo foo");
        });
        assert!(notes.contains(&"Replaced 3 occurrences".to_string()));
    }

    #[test]
    fn replace_one_with_regex_groups_moves_to_next() {
        let mut ed = editor_with("a.txt", "a1 a2 b3");
        with_cx(|cx| {
            ed.handle_action(&EditorAction::Replace, cx);
            let b = ed.active_buffer_mut().unwrap();
            let s = b.view.search.as_mut().unwrap();
            s.regex = true;
            s.query.set_text(r"\ba(\d)");
            s.replace.set_text("<$1>");
            s.field = Field::Replace;
            refresh_search(b);
            assert_eq!(b.view.sel.range(), 0..2);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::NONE), cx));
            let b = ed.active_buffer().unwrap();
            assert_eq!(b.text(), "<1> a2 b3");
            assert_eq!(b.view.sel.range(), 4..6);
        });
    }

    #[test]
    fn large_file_opens_without_syntax() {
        let big = "let x = 1;\n".repeat(5_000_000 / 11 + 1);
        let ed = editor_with("big.rs", &big);
        assert!(ed.active_buffer().unwrap().syntax.is_none());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn small_rust_file_gets_syntax_and_comment_toggle() {
        let mut ed = editor_with("small.rs", "fn main() {}\n");
        assert!(ed.active_buffer().unwrap().syntax.is_some());
        with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleComment, cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "// fn main() {}\n");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn enter_in_braces_and_closer_dedent() {
        let mut ed = editor_with("a.rs", "fn f() {\n    let s = \"{\";\n");
        ed.active_buffer_mut().unwrap().view.sel = Selection::point(8);
        with_cx(|cx| {
            // Auto-close types `{}` and Enter splits it over three lines.
            ed.handle_action(&EditorAction::InsertNewline, cx);
            ed.handle_action(&EditorAction::InsertText("if x ".into()), cx);
            ed.handle_action(&EditorAction::InsertChar('{'), cx);
            ed.handle_action(&EditorAction::InsertNewline, cx);
        });
        let b = ed.active_buffer().unwrap();
        assert_eq!(b.text(), "fn f() {\n    if x {\n        \n    }\n    let s = \"{\";\n");
        assert_eq!(b.cursor_line_col(), (2, 8));
        // `}` on a blank line dedents to the opener's line; the `{` in the
        // string literal is skipped.
        let mut ed = editor_with("a.rs", "fn f() {\n    let s = \"{\";\n    ");
        let end = ed.active_buffer().unwrap().len_chars();
        ed.active_buffer_mut().unwrap().view.sel = Selection::point(end);
        with_cx(|cx| {
            ed.handle_action(&EditorAction::InsertChar('}'), cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "fn f() {\n    let s = \"{\";\n}");
        // Typing `}` at an existing closer steps over it, line unchanged.
        let mut ed = editor_with("a.rs", "fn f() {\n    }");
        ed.active_buffer_mut().unwrap().view.sel = Selection::point(13);
        with_cx(|cx| {
            ed.handle_action(&EditorAction::InsertChar('}'), cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "fn f() {\n    }");
        assert_eq!(ed.active_buffer().unwrap().view.sel.head, 14);
    }

    #[cfg(feature = "lang-css")]
    #[test]
    fn css_ctrl_slash_falls_back_to_block_comment() {
        let mut ed = editor_with("a.css", "a {\n    color: red;\n}\n");
        ed.active_buffer_mut().unwrap().view.sel = Selection::point(10);
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleComment, cx);
        });
        assert!(notes.is_empty(), "{notes:?}");
        let b = ed.active_buffer().unwrap();
        assert_eq!(b.text(), "a {\n    /* color: red; */\n}\n");
        assert_eq!(b.view.sel.head, 13, "caret stays on the same character");
        with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleComment, cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "a {\n    color: red;\n}\n");
        // A selection over two lines becomes one block from the first indent.
        ed.active_buffer_mut().unwrap().view.sel = Selection::new(2, 10);
        with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleComment, cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "/* a {\n    color: red; */\n}\n");
        assert!(ed.active_buffer_mut().unwrap().undo());
        assert_eq!(ed.active_buffer().unwrap().text(), "a {\n    color: red;\n}\n", "one undo step");
    }

    #[test]
    fn block_comment_without_delimiters_notifies() {
        let mut ed = editor_with("a.txt", "plain");
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleBlockComment, cx);
        });
        assert_eq!(notes, vec!["No block comment for this file type".to_string()]);
        assert_eq!(ed.active_buffer().unwrap().text(), "plain");
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_colon_indents() {
        let mut ed = editor_with("a.py", "def f():");
        ed.active_buffer_mut().unwrap().view.sel = Selection::point(8);
        with_cx(|cx| {
            ed.handle_action(&EditorAction::InsertNewline, cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "def f():\n    ");
    }

    #[test]
    fn save_conflict_reload_takes_the_disk_text_undoably() {
        let dir = std::env::temp_dir().join(format!("jetl-test-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a\n").unwrap();
        std::fs::write(&b, "b\n").unwrap();
        let mut ed = Editor::new(&Config::default());
        ed.open_text(a.clone(), "a\n", false, &Theme::default());
        ed.open_text(b.clone(), "b\n", false, &Theme::default());
        ed.buffers[0].insert("mine ", TxKind::Typing);
        ed.buffers[1].insert("mine ", TxKind::Typing);
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&a, "theirs a\n").unwrap();
        std::fs::write(&b, "theirs b\n").unwrap();
        let notes = with_cx(|cx| {
            assert!(!ed.save_all(cx));
            let dialogs: Vec<_> = cx.effects.iter().filter(|e| matches!(e, Effect::Dialog(_))).collect();
            assert_eq!(dialogs.len(), 1, "one dialog; the other conflict only warns");
            ed.reload_from_disk(ed.buffers[0].id, cx);
        });
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(ed.buffers[0].text(), "theirs a\n");
        assert!(!ed.buffers[0].dirty());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "theirs b\n");
        assert!(ed.buffers[0].undo());
        assert_eq!(ed.buffers[0].text(), "mine a\n");
        // A file deleted meanwhile is recreated without asking.
        std::fs::remove_file(&b).unwrap();
        with_cx(|cx| assert!(ed.save(1, cx)));
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "mine b\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn idle_autosave_waits_for_the_pause() {
        let t0 = Instant::now();
        let idle = Duration::from_millis(1500);
        let at = |ms| t0 + Duration::from_millis(ms);
        assert!(!idle_autosave_due(Autosave::Idle, t0, at(1499), idle, true));
        assert!(idle_autosave_due(Autosave::Idle, t0, at(1500), idle, true));
        assert!(!idle_autosave_due(Autosave::Idle, t0, at(5000), idle, false), "nothing to save");
        assert!(!idle_autosave_due(Autosave::Off, t0, at(5000), idle, true));
        assert!(!idle_autosave_due(Autosave::Focus, t0, at(5000), idle, true));
        // A clock that appears to run backwards is not a pause.
        assert!(!idle_autosave_due(Autosave::Idle, at(10), t0, idle, true));
    }

    #[test]
    fn autosave_is_quiet_and_holds_back_on_conflict_and_failure() {
        let dir = std::env::temp_dir().join(format!("jetl-test-autosave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(&Config::default());
        ed.open_text(path.clone(), "one\n", false, &Theme::default());
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(dir.clone()));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };

        ed.buffers[0].insert("x", TxKind::Typing);
        ed.autosave(&mut cx);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xone\n");
        assert!(!ed.buffers[0].dirty());
        assert!(matches!(cx.effects.as_slice(), [Effect::FileSaved { quiet: true, .. }]));

        // Changed on disk under unsaved edits: warn once, keep the disk text.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "theirs\n").unwrap();
        cx.effects.clear();
        ed.buffers[0].insert("y", TxKind::Typing);
        ed.autosave(&mut cx);
        ed.autosave(&mut cx);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        assert_eq!(cx.effects.iter().filter(|e| matches!(e, Effect::Notify(Level::Warn, _))).count(), 1);
        assert!(!ed.buffers[0].autosave_candidate());
        // An explicit save asks; Overwrite writes and lifts the hold.
        cx.effects.clear();
        assert!(!ed.save(0, &mut cx));
        let id = ed.buffers[0].id;
        assert!(matches!(cx.effects.as_slice(), [Effect::Dialog(d)] if d.id == DialogId::OverwriteExternal(id)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        assert!(ed.overwrite(id, &mut cx));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xyone\n");
        assert!(!ed.buffers[0].autosave_conflict);

        // A failed write waits for the next edit.
        let _ = std::fs::remove_dir_all(&dir);
        ed.buffers[0].insert("z", TxKind::Typing);
        assert!(ed.buffers[0].autosave_candidate());
        ed.buffers[0].disk_mtime = None;
        ed.autosave(&mut cx);
        assert!(ed.buffers[0].dirty());
        assert!(!ed.buffers[0].autosave_candidate());
        ed.buffers[0].insert("w", TxKind::Typing);
        assert!(ed.buffers[0].autosave_candidate());

        // Untitled buffers are never candidates.
        let mut untitled = Buffer::from_text(9, None, "", false);
        untitled.insert("q", TxKind::Typing);
        assert!(!untitled.autosave_candidate());
    }

    fn caret(ed: &Editor) -> (String, usize) {
        let b = ed.active_buffer().unwrap();
        (b.name.clone(), b.cursor_line_col().0)
    }

    #[test]
    fn nav_back_and_forward_over_jumps_and_tabs() {
        let text = "x\n".repeat(100);
        let mut ed = editor_with("a.txt", &text);
        // A short move is not remembered; a far goto line is.
        ed.goto_line(5, 1);
        ed.goto_line(60, 1);
        ed.open_text(PathBuf::from("/tmp/b.txt"), &text, false, &Theme::default());
        assert_eq!(caret(&ed), ("b.txt".into(), 0));
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::NavBack, cx);
            assert_eq!(caret(&ed), ("a.txt".into(), 59));
            ed.handle_action(&EditorAction::NavBack, cx);
            assert_eq!(caret(&ed), ("a.txt".into(), 4));
            ed.handle_action(&EditorAction::NavBack, cx);
            ed.handle_action(&EditorAction::NavForward, cx);
            assert_eq!(caret(&ed), ("a.txt".into(), 59));
            ed.handle_action(&EditorAction::NavForward, cx);
            assert_eq!(caret(&ed), ("b.txt".into(), 0));
        });
        assert_eq!(notes, vec!["No earlier position".to_string()]);
    }

    #[test]
    fn nav_back_reopens_a_closed_file_at_its_caret() {
        let text = "x\n".repeat(30);
        let mut ed = editor_with("a.txt", &text);
        ed.goto_line(20, 1);
        ed.open_text(PathBuf::from("/tmp/b.txt"), &text, false, &Theme::default());
        ed.show_slot(0);
        ed.force_close(0);
        assert_eq!(caret(&ed).0, "b.txt");
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(PathBuf::from("/tmp"));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        ed.handle_action(&EditorAction::NavBack, &mut cx);
        assert!(matches!(cx.effects.as_slice(), [Effect::OpenFile(p)] if p == Path::new("/tmp/a.txt")));
        // The app loads the file; it opens at the remembered caret.
        ed.open_text(PathBuf::from("/tmp/a.txt"), &text, false, &theme);
        assert_eq!(caret(&ed), ("a.txt".into(), 19));
        ed.handle_action(&EditorAction::NavForward, &mut cx);
        assert_eq!(caret(&ed).0, "b.txt");
    }

    #[test]
    fn bookmarks_dialog_lists_and_jumps() {
        let mut ed = editor_with("a.txt", "one\ntwo\nthree\n");
        ed.goto_line(2, 1);
        with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleBookmark, cx);
            ed.handle_action(&EditorAction::ShowBookmarks, cx);
            let Some(Effect::Dialog(d)) = cx.effects.pop() else { panic!("no dialog") };
            let crate::ui::dialog::DialogKind::Choice { items, .. } = &d.kind else { panic!() };
            assert_eq!(items, &vec!["a.txt:2 two".to_string()]);
            let DialogId::Bookmarks(t) = d.id else { panic!() };
            ed.goto_line(3, 1);
            ed.goto_bookmark(t[0].0, t[0].1);
            assert_eq!(caret(&ed).1, 1);
            ed.handle_action(&EditorAction::NavBack, cx);
            assert_eq!(caret(&ed).1, 2);
        });
    }

    #[test]
    fn editorconfig_drives_indent_save_and_new_file_line_ending() {
        let dir = std::env::temp_dir().join(format!("jetl-test-editor-ec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".editorconfig"), "root = true\n[*.c]\nindent_style = tab\ntab_width = 8\ntrim_trailing_whitespace = false\ninsert_final_newline = false\nend_of_line = crlf\n").unwrap();
        let path = dir.join("a.c");
        std::fs::write(&path, "int x;\n").unwrap();
        let mut ed = Editor::new(&Config::default());
        ed.open_text(path.clone(), "int x;\n", false, &Theme::default());
        assert_eq!(ed.status_indent().as_deref(), Some("Tab"));
        assert_eq!(ed.tab_display(&ed.buffers[0]), 8);
        assert_eq!(ed.buffers[0].line_ending, buffer::LineEnding::Lf, "an existing file keeps its line ending");
        with_cx(|cx| {
            ed.buffers[0].goto_line(1, 0);
            ed.handle_action(&EditorAction::InsertTab, cx);
            ed.buffers[0].insert("y; ", TxKind::Typing);
            ed.buffers[0].goto_line(0, 0);
            assert!(ed.save(0, cx));
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "int x;\n\ty; ", "no trim, no final newline");
        // A file not on disk yet takes the line ending; a 2-space file without
        // settings detects its indent, and the dialog switches it.
        ed.open_text(dir.join("new.c"), "", false, &Theme::default());
        assert_eq!(ed.active_buffer().unwrap().line_ending, buffer::LineEnding::CrLf);
        ed.open_text(dir.join("b.txt"), "a\n  b\n    c\n  d\n    e\n", false, &Theme::default());
        assert_eq!(ed.status_indent().as_deref(), Some("2 spaces"));
        let id = ed.active_buffer().unwrap().id;
        ed.set_indent(id, 0);
        assert_eq!(ed.active_buffer().unwrap().indent, indent::IndentStyle::tabs(4));
        ed.set_indent(id, 3);
        assert_eq!(ed.status_indent().as_deref(), Some("8 spaces"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(all(feature = "lang-python", feature = "lang-rust"))]
    #[test]
    fn shebang_sets_language_unless_overridden() {
        let mut ed = editor_with("script", "#!/usr/bin/env python3\nprint(1)\n");
        assert_eq!(ed.status_language().as_deref(), Some("python"));
        assert_eq!(ed.buffers[0].syntax.as_ref().map(|s| s.def.name), Some("python"));
        let id = ed.buffers[0].id;
        ed.set_language(id, syntax::language_for_name("rust"), &Theme::default());
        assert_eq!(ed.status_language().as_deref(), Some("rust"), "the language picked by hand wins");
        ed.set_language(id, None, &Theme::default());
        assert_eq!(ed.status_language().as_deref(), Some("Plain text"));
        // Too large for highlighting: the shebang still names the language
        // (line comments toggle with `#`).
        let mut config = Config::default();
        config.editor.max_highlight_bytes = 8;
        let mut ed = Editor::new(&config);
        ed.open_text(PathBuf::from("/tmp/big-script"), "#!/usr/bin/env python3\nprint(1)\n", false, &Theme::default());
        assert!(ed.buffers[0].syntax.is_none());
        assert_eq!(ed.buffers[0].language().and_then(|d| d.line_comment), Some("#"));
    }

    fn names(ed: &Editor) -> Vec<String> {
        ed.check_invariants();
        ed.tab_names()
    }

    /// An editor with tabs `a.txt`, `b.txt`, … opened in that order (the
    /// last one active) under `config`.
    fn editor_tabs(config: &Config, n: usize) -> Editor {
        let mut ed = Editor::new(config);
        for c in ('a'..).take(n) {
            ed.open_text(PathBuf::from(format!("/tmp/{c}.txt")), "x\n", false, &Theme::default());
        }
        ed
    }

    fn act(ed: &mut Editor, a: EditorAction) -> Vec<Effect> {
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(PathBuf::from("/tmp"));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        ed.handle_action(&a, &mut cx);
        cx.effects
    }

    #[test]
    fn tabs_move_and_pinned_tabs_stay_first() {
        let mut ed = editor_tabs(&Config::default(), 4);
        assert_eq!(names(&ed), ["a.txt", "b.txt", "c.txt", "d.txt"]);
        act(&mut ed, EditorAction::MoveTabLeft);
        act(&mut ed, EditorAction::MoveTabLeft);
        assert_eq!(names(&ed), ["a.txt", "d.txt", "b.txt", "c.txt"]);
        assert_eq!(ed.active_buffer().unwrap().name, "d.txt", "the moved tab stays active");
        // Pinning c moves it to the front; d follows it.
        ed.show_slot(3);
        act(&mut ed, EditorAction::TogglePinTab);
        assert_eq!(names(&ed), ["c.txt", "a.txt", "d.txt", "b.txt"]);
        ed.show_slot(2);
        act(&mut ed, EditorAction::TogglePinTab);
        assert_eq!(names(&ed), ["c.txt", "d.txt", "a.txt", "b.txt"]);
        // Pinned tabs move only among themselves, the others after them.
        act(&mut ed, EditorAction::MoveTabRight);
        assert_eq!(names(&ed), ["c.txt", "d.txt", "a.txt", "b.txt"]);
        act(&mut ed, EditorAction::MoveTabLeft);
        assert_eq!(names(&ed), ["d.txt", "c.txt", "a.txt", "b.txt"]);
        ed.show_slot(2);
        act(&mut ed, EditorAction::MoveTabLeft);
        assert_eq!(names(&ed), ["d.txt", "c.txt", "a.txt", "b.txt"]);
        // A new tab never opens among the pinned ones.
        ed.show_slot(0);
        ed.open_text(PathBuf::from("/tmp/e.txt"), "", false, &Theme::default());
        assert_eq!(names(&ed), ["d.txt", "c.txt", "e.txt", "a.txt", "b.txt"]);
        // Close others keeps the active tab and the pinned ones.
        ed.show_slot(3);
        assert!(act(&mut ed, EditorAction::CloseOtherTabs).is_empty());
        assert_eq!(names(&ed), ["d.txt", "c.txt", "a.txt"]);
        assert_eq!(ed.active_buffer().unwrap().name, "a.txt");
        // Unpinning moves the tab to the start of the others.
        ed.show_slot(0);
        act(&mut ed, EditorAction::TogglePinTab);
        assert_eq!(names(&ed), ["c.txt", "d.txt", "a.txt"]);
        assert!(!ed.focused_group().views[1].pinned && ed.focused_group().views[0].pinned);
        // Close all but pinned, then close all.
        act(&mut ed, EditorAction::CloseUnpinnedTabs);
        assert_eq!(names(&ed), ["c.txt"]);
        act(&mut ed, EditorAction::CloseAllTabs);
        assert!(ed.buffers.is_empty());
    }

    #[test]
    fn close_others_asks_about_unsaved_tabs() {
        let mut ed = editor_tabs(&Config::default(), 3);
        ed.show_slot(1);
        act(&mut ed, EditorAction::InsertText("dirty ".into()));
        ed.show_slot(0);
        let fx = act(&mut ed, EditorAction::CloseOtherTabs);
        let ids = match fx.as_slice() {
            [Effect::Dialog(d)] => match &d.id {
                DialogId::CloseTabsDirty(ids) => ids.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert_eq!(ids, [ed.buffers[1].id, ed.buffers[2].id]);
        assert_eq!(ed.buffers.len(), 3, "nothing closes before the answer");
        ed.force_close_ids(&ids);
        assert_eq!(names(&ed), ["a.txt"]);
    }

    #[test]
    fn tab_drag_moves_once_the_pointer_reaches_the_new_place() {
        let mut ed = editor_tabs(&Config::default(), 3);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 5)).unwrap();
        let mut draw = |ed: &mut Editor| {
            term.draw(|f| ed.render(f, Rect::new(0, 0, 80, 1), Rect::new(0, 1, 80, 4), true, &Theme::default())).unwrap();
        };
        // Rename b so the tabs differ in width: a (10), bbbbbbbbbb (15), c (10).
        ed.buffers[1].name = "bbbbbbbbbb".into();
        draw(&mut ed);
        let a = ed.buffers[0].id;
        assert_eq!(ed.focused_group().tab_hits.rect_of(1), Some(Rect::new(10, 0, 15, 1)));
        // Over the start of b: a would land at 15..25, so it stays.
        ed.drag_tab(0, a, 12);
        assert_eq!(names(&ed), ["a.txt", "bbbbbbbbbb", "c.txt"]);
        ed.drag_tab(0, a, 16);
        assert_eq!(names(&ed), ["bbbbbbbbbb", "a.txt", "c.txt"]);
        // Stale hit rects are ignored until the next frame.
        ed.drag_tab(0, a, 30);
        assert_eq!(names(&ed), ["bbbbbbbbbb", "a.txt", "c.txt"]);
        draw(&mut ed);
        ed.drag_tab(0, a, 30);
        assert_eq!(names(&ed), ["bbbbbbbbbb", "c.txt", "a.txt"]);
        // Back to the front in one move.
        draw(&mut ed);
        ed.drag_tab(0, a, 1);
        assert_eq!(names(&ed), ["a.txt", "bbbbbbbbbb", "c.txt"]);
        // A pinned tab cannot be dragged past the unpinned ones.
        ed.set_pinned(0, true);
        draw(&mut ed);
        ed.drag_tab(0, a, 30);
        assert_eq!(names(&ed), ["a.txt", "bbbbbbbbbb", "c.txt"]);
    }

    #[test]
    fn tab_limit_closes_the_least_recent_clean_unpinned_tab() {
        let mut config = Config::default();
        config.editor.max_tabs = 3;
        let mut ed = editor_tabs(&config, 3);
        // Recency, oldest first: b, then a (pinned), c (dirty), d.
        ed.show_slot(1);
        ed.show_slot(0);
        act(&mut ed, EditorAction::TogglePinTab);
        ed.show_slot(2);
        act(&mut ed, EditorAction::InsertText("dirty ".into()));
        ed.open_text(PathBuf::from("/tmp/d.txt"), "", false, &Theme::default());
        assert_eq!(ed.enforce_tab_limit(), 1);
        assert_eq!(names(&ed), ["a.txt", "c.txt", "d.txt"]);
        // Only pinned, dirty and active tabs are left: the limit gives way.
        ed.show_slot(2);
        act(&mut ed, EditorAction::InsertText("dirty ".into()));
        ed.open_text(PathBuf::from("/tmp/e.txt"), "", false, &Theme::default());
        assert_eq!(ed.enforce_tab_limit(), 0);
        assert_eq!(names(&ed), ["a.txt", "c.txt", "d.txt", "e.txt"]);
        // No limit by default.
        let mut ed = editor_tabs(&Config::default(), 20);
        assert_eq!((ed.enforce_tab_limit(), ed.buffers.len()), (0, 20));
    }
}
