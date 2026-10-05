//! Editor: a tab strip of buffers plus the text view.

pub mod blame;
mod bookmarks;
pub use bookmarks::BookmarkTarget;
pub mod brackets;
pub mod buffer;
mod carets;
mod column;
pub mod expand;
mod find;
pub mod fold;
pub mod format;
pub mod groups;
pub mod gutter;
pub mod indent;
mod io;
pub use io::DiskChange;
pub mod merge;
pub mod nav;
pub mod rows;
pub mod search;
pub mod selection;
pub mod split;
pub mod syntax;
pub mod undo;
pub mod view;
pub mod view_state;
pub mod wrap;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::actions::EditorAction;
use crate::app::{AfterSave, Ctx};
use crate::config::theme::Theme;
use crate::config::{Autosave, Config};
use crate::event::{Effect, Level, Mouse};
use crate::ui::dialog::{Dialog, DialogId};
use buffer::{Buffer, BufferId};
use find::{LastSearch, refresh_search, render_find_bar};
use groups::{GroupId, TabGroup};
use nav::{NavPos, NavRing};
use rows::{Row, Rows};
use search::Field;
use selection::{Selection, apply_motion_all, line_range, word_at};
use split::{SplitNode, SplitterHit};
use undo::TxKind;
use view::RenderOpts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Char,
    Word(usize, usize),
    Line(usize, usize),
    /// Alt held on the press (a caret was added or removed there), at this
    /// (line, visual column). Moving to another cell makes it a column drag
    /// from there.
    AltPending((usize, usize)),
    /// A column block from this (line, visual column) to the pointer.
    Column((usize, usize)),
}

/// What [`Editor::path_setup`] works out from a path, for [`adopt_path`].
pub(super) struct PathSetup {
    lang: Option<&'static syntax::LanguageDef>,
    syntax: Option<syntax::Syntax>,
    mtime: Option<std::time::SystemTime>,
    settings: indent::FileSettings,
    wrap: bool,
}

/// Make `b` the buffer of `path`: its name, language (detected again, a
/// language picked by hand is dropped), highlighting, mtime, `.editorconfig`
/// settings, indent and soft wrap. The text and undo history stay.
pub(super) fn adopt_path(b: &mut Buffer, path: PathBuf, s: PathSetup, tab_width: usize) {
    b.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    b.path = Some(path);
    b.language_choice = buffer::LanguageChoice::Auto;
    b.syntax = s.syntax;
    b.detected_language = s.lang;
    b.disk_mtime = s.mtime;
    b.indent = s.settings.indent(&b.rope, tab_width);
    b.settings = s.settings;
    b.view.wrap.on = s.wrap;
    b.fold_cache = None;
    b.changed_since_render = true;
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
    /// `editor.soft_wrap` and `editor.soft_wrap_files`: whether a file
    /// opens wrapped ([`Editor::soft_wrap_for`]).
    soft_wrap: bool,
    soft_wrap_files: Vec<String>,
    /// `[format]`: formatters per language and whether saves run them.
    pub format: crate::config::FormatConfig,
    last_search: Option<LastSearch>,
    /// Back/forward history of caret positions.
    nav: NavRing,
    /// A closed file being reopened by back/forward or a numbered
    /// bookmark, and where to put the caret once it loads.
    pending_nav: Option<(PathBuf, nav::PendingAt)>,
    /// Numbered bookmarks of files without a tab: digit to path and line
    /// (see `bookmarks.rs`).
    closed_digits: std::collections::BTreeMap<u8, (PathBuf, usize)>,
    /// The last copy made at several carets: the clipboard text and each
    /// caret's piece, so pasting it at as many carets splits it again.
    multi_clip: Option<(String, Vec<String>)>,
    /// Column selection mode: a plain drag and Shift+arrows make blocks.
    /// Not saved with the session.
    column_mode: bool,
    /// The conflict resolver, while open (its panes are the groups then).
    merge: Option<merge::MergeSession>,
    /// The groups' area as last drawn (the resolver picks two or three
    /// panes by its width).
    center: Rect,
}

/// Shown in a group without tabs, as phrases that wrap as a whole.
const EMPTY_HINT: [&str; 3] = ["No files open.", "Alt+1 opens the Project view;", "Ctrl+Shift+N finds a file."];

/// Shown in place of a pinned tab's close button (one column wide).
const PIN_MARK: &str = "⚲";

/// Indent dialog items: 0 is a tab, else that many spaces.
const INDENT_CHOICES: [usize; 4] = [0, 2, 4, 8];

/// A jump moving the caret by more than this many lines is remembered for
/// back/forward.
const NAV_JUMP_LINES: usize = 10;

fn nav_pos(b: &Buffer) -> NavPos {
    NavPos { id: b.id, path: b.path.clone(), pos: b.view.sel().head }
}

impl Editor {
    pub fn new(config: &Config) -> Self {
        let mut ed = Self {
            buffers: Vec::new(),
            groups: vec![TabGroup::new(0)],
            tree: SplitNode::Leaf(0),
            splitters: Vec::new(),
            focused: 0,
            next_group: 1,
            mru: Vec::new(),
            next_id: 1,
            // The config fields are set by `apply_config` below.
            tab_width: 0,
            scrolloff: 0,
            hscrolloff: 0,
            auto_close: false,
            highlight_occurrences: false,
            trim_trailing_whitespace: false,
            final_newline: false,
            autosave: Autosave::Off,
            autosave_idle: Duration::ZERO,
            last_edit: Instant::now(),
            drag: None,
            max_highlight_bytes: 0,
            max_tabs: 0,
            soft_wrap: false,
            soft_wrap_files: Vec::new(),
            format: Default::default(),
            last_search: None,
            nav: NavRing::default(),
            pending_nav: None,
            closed_digits: Default::default(),
            multi_clip: None,
            column_mode: false,
            merge: None,
            center: Rect::default(),
        };
        ed.apply_config(config);
        ed
    }

    /// Take the `[editor]` and `[format]` settings from `config` (startup
    /// and hot reload). Never touches a view: open tabs keep their wrap,
    /// indentation and folds; files opened afterwards use the new defaults.
    pub fn apply_config(&mut self, config: &Config) {
        // Exhaustive, so a new `[editor]` key must be handled here.
        let crate::config::EditorConfig {
            tab_width,
            max_highlight_bytes,
            scrolloff,
            hscrolloff,
            auto_close,
            highlight_occurrences,
            trim_trailing_whitespace,
            final_newline,
            autosave,
            autosave_idle_ms,
            max_tabs,
            soft_wrap,
            soft_wrap_files,
        } = &config.editor;
        self.tab_width = *tab_width;
        self.max_highlight_bytes = *max_highlight_bytes;
        self.scrolloff = *scrolloff;
        self.hscrolloff = *hscrolloff;
        self.auto_close = *auto_close;
        self.highlight_occurrences = *highlight_occurrences;
        self.trim_trailing_whitespace = *trim_trailing_whitespace;
        self.final_newline = *final_newline;
        if self.autosave != *autosave {
            // Turning on idle autosave waits a full interval from now.
            self.last_edit = Instant::now();
        }
        self.autosave = *autosave;
        self.autosave_idle = Duration::from_millis(*autosave_idle_ms);
        self.max_tabs = *max_tabs;
        self.soft_wrap = *soft_wrap;
        self.soft_wrap_files.clone_from(soft_wrap_files);
        self.format.clone_from(&config.format);
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
    /// Unsaved edits in a file's buffer (the resolver's scratch buffers do
    /// not count).
    pub fn has_dirty(&self) -> bool {
        self.buffers.iter().any(|b| b.dirty() && !self.is_merge_buffer(b.id))
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
        let setup = self.path_setup(&path, text, theme);
        let pending = self.pending_nav.take_if(|(p, _)| *p == path).map(|(_, pos)| pos);
        let digits = self.unpark_digits(&path);
        // A file not on disk yet has no line ending of its own.
        let le = setup.settings.end_of_line.filter(|_| setup.mtime.is_none() && text.is_empty());
        let mut buf = Buffer::from_text(id, None, text, lossy);
        let last = buf.rope.len_lines() - 1;
        buf.bookmarks.extend(digits.into_iter().map(|(line, d)| (line.min(last), Some(d))));
        adopt_path(&mut buf, path, setup, self.tab_width);
        if let Some(le) = le {
            buf.line_ending = le;
        }
        self.buffers.push(buf);
        let at = self.buffers.len() - 1;
        match pending {
            // Reopened by back/forward, which already updated the history.
            Some(nav::PendingAt::Pos(pos)) => {
                self.set_active(at);
                self.buffers[at].goto_pos(pos);
            }
            Some(nav::PendingAt::Line(line)) => {
                self.set_active(at);
                self.buffers[at].goto_line(line, 0);
            }
            None => self.activate_index(at),
        }
        at
    }

    /// What a buffer takes from its file's path and text: the language (the
    /// path, else a shebang or modeline; highlighting only below
    /// `max_highlight_bytes`), the file's mtime, `.editorconfig` and soft
    /// wrap. Shared by [`Editor::open_text`] and Save As.
    pub(super) fn path_setup(&self, path: &Path, text: &str, theme: &Theme) -> PathSetup {
        // Only the first and last lines are read for a shebang or modeline.
        let lang = syntax::detect_text(path, text);
        let syntax = lang.filter(|_| text.len() <= self.max_highlight_bytes).and_then(|d| syntax::Syntax::new(d, theme));
        PathSetup {
            lang,
            syntax,
            mtime: buffer::file_mtime(path),
            settings: indent::FileSettings::for_path(path),
            wrap: self.soft_wrap_for(Some(path)),
        }
    }

    /// The first untitled buffer with unsaved edits in tab order (the
    /// resolver's buffers do not count), among `within` when given.
    pub fn untitled_dirty(&self, within: Option<&[BufferId]>) -> Option<BufferId> {
        self.tab_order().into_iter().map(|i| &self.buffers[i]).find_map(|b| {
            let wanted = b.path.is_none() && b.dirty() && !self.is_merge_buffer(b.id);
            (wanted && within.is_none_or(|ids| ids.contains(&b.id))).then_some(b.id)
        })
    }

    /// Add an empty buffer without a file, named `name`, as a new tab and
    /// make it active.
    pub fn open_untitled(&mut self, name: &str) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let mut buf = Buffer::from_text(id, None, "", false);
        buf.name = name.to_string();
        buf.indent = indent::FileSettings::default().indent(&buf.rope, self.tab_width);
        buf.view.wrap.on = self.soft_wrap_for(None);
        self.buffers.push(buf);
        let at = self.buffers.len() - 1;
        self.activate_index(at);
        at
    }

    /// Whether a file at `path` opens soft-wrapped: all do with
    /// `editor.soft_wrap`, else those `editor.soft_wrap_files` matches.
    pub fn soft_wrap_for(&self, path: Option<&Path>) -> bool {
        let name = path.and_then(Path::file_name).map(|n| n.to_string_lossy()).unwrap_or_default();
        self.soft_wrap
            || self.soft_wrap_files.iter().any(|m| match m.strip_prefix('*') {
                Some(suffix) => name.len() > suffix.len() && name.ends_with(suffix),
                None => *m == name,
            })
    }

    /// `editor.scrolloff` as applied.
    #[cfg(test)]
    pub fn scrolloff(&self) -> usize {
        self.scrolloff
    }

    /// Take the capture styles of every buffer (the conflict resolver's
    /// panes included) from `theme` again (hot reload).
    pub fn restyle(&mut self, theme: &Theme) {
        for b in &mut self.buffers {
            if let Some(s) = b.syntax.as_mut() {
                s.restyle(theme);
            }
        }
    }

    /// Put back a session's caret, scroll and folds for tab `i` (clamped
    /// to the file as it is now). The first render still keeps the caret
    /// visible, unfolding what hides it.
    pub fn restore_view(
        &mut self,
        i: usize,
        (line, col): (usize, usize),
        (scroll_line, scroll_col): (usize, usize),
        folds: &[crate::workspace::SessionFold],
    ) {
        let Some(b) = self.buffers.get_mut(i) else { return };
        b.goto_line(line, col);
        b.view.folds = fold::Folds::from_session(folds, &b.rope);
        b.view.scroll_line = Rows::new(&b.view.folds, b.rope.len_lines()).visible(scroll_line);
        b.view.scroll_seg = 0;
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
        if line(from.pos).abs_diff(line(b.view.sel().head)) > NAV_JUMP_LINES {
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
                self.pending_nav = Some((path.clone(), nav::PendingAt::Pos(p.pos)));
                cx.effects.push(Effect::OpenFile(path));
            }
            (None, None) => {}
        }
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
        self.retarget_closed(from, to);
    }

    /// Close the tabs of `path` or under it, unsaved edits included (the
    /// file was deleted).
    pub fn close_under(&mut self, path: &Path) -> usize {
        let mut closed = 0;
        while let Some(i) = self.buffers.iter().position(|b| b.path.as_deref().is_some_and(|p| p.starts_with(path))) {
            self.force_close(i);
            closed += 1;
        }
        self.drop_closed_under(path);
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
        let col = view::visual_col(&b.rope, b.view.sel().head, self.tab_display(b));
        let chars: usize = b.view.sels.iter().map(|s| s.range().len()).sum();
        let carets = b.view.sels.len();
        let mut s = match (carets, chars) {
            (1, 0) => format!("Ln {}, Col {}", line + 1, col + 1),
            (1, n) => format!("Ln {}, Col {} ({n} chars)", line + 1, col + 1),
            (k, 0) => format!("Ln {}, Col {} ({k} carets)", line + 1, col + 1),
            (k, n) => format!("Ln {}, Col {} ({k} carets, {n} chars)", line + 1, col + 1),
        };
        if self.column_mode {
            s.push_str(" Column");
        }
        Some(s)
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
        if self.merge_guard(a, cx) {
            return true;
        }
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
                // Then the untitled buffers, one prompt at a time, unless a
                // file changed on disk is being asked about.
                if !cx.effects.iter().any(|e| matches!(e, Effect::Dialog(_)))
                    && let Some(id) = self.untitled_dirty(None)
                {
                    cx.effects.push(Effect::SaveAs { id, then: AfterSave::SaveAll });
                }
                return true;
            }
            E::Save => {
                // An untitled buffer asks where to save it.
                if let Some(b) = self.active_buffer().filter(|b| b.path.is_none()) {
                    cx.effects.push(Effect::SaveAs { id: b.id, then: AfterSave::Nothing });
                } else if let Some(i) = self.active() {
                    self.save_with(i, cx, self.format.on_save);
                }
                return true;
            }
            E::SaveAs => {
                if let Some(b) = self.active_buffer() {
                    cx.effects.push(Effect::SaveAs { id: b.id, then: AfterSave::Nothing });
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
            E::ToggleNumberedBookmark => {
                self.choose_digit(cx);
                return true;
            }
            E::GotoNumberedBookmark(d) => {
                self.goto_digit(*d, cx);
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
            E::ToggleColumnMode => {
                if self.column_mode || self.active_buffer().is_none_or(Buffer::blocks_supported) {
                    self.column_mode ^= true;
                } else {
                    cx.notify(Level::Info, "Column selection does not work with soft wrap");
                }
                return true;
            }
            E::ToggleSoftWrap => {
                let Some(b) = self.active_buffer_mut() else { return true };
                let v = &mut b.view;
                v.wrap.on ^= true;
                // Desired columns change meaning (chars or row x).
                for (_, d) in v.sels.iter_mut_desired() {
                    *d = None;
                }
                (v.scroll_col, v.scroll_seg, v.follow_key) = (0, 0, None);
                if v.wrap.on && self.column_mode {
                    self.column_mode = false;
                    cx.notify(Level::Info, "Column selection mode is off: it does not work with soft wrap");
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
        let tab_width = self.tab_display(&self.buffers[ai]);
        let b = &mut self.buffers[ai];
        // Any other action ends an expand/shrink sequence; scrolling and
        // copying leave the selection alone, so they keep it.
        if !matches!(a, E::ExpandSelection | E::ShrinkSelection | E::Copy | E::ScrollUp(_) | E::ScrollDown(_)) {
            b.view.expand_stack.clear();
        }
        let pairs = || if auto_close { brackets::pairs(b.language().map(|d| d.name)) } else { &[] };
        match a {
            E::InsertChar(c) => b.insert_char(*c, pairs()),
            E::InsertText(s) => {
                let s = if b.line_ending == buffer::LineEnding::CrLf {
                    s.replace("\r\n", "\n").replace('\n', "\r\n")
                } else {
                    s.replace("\r\n", "\n")
                };
                match paste_pieces(b, &s, self.multi_clip.as_ref()) {
                    Some(pieces) => b.insert_pieces(&pieces),
                    None => b.insert(&s, TxKind::Other),
                }
            }
            E::InsertNewline => {
                let colon = b.language().is_some_and(|d| d.indent_after_colon);
                b.newline_smart(colon);
            }
            E::InsertTab => {
                let spans = |s: &Selection| b.rope.char_to_line(s.range().start) != b.rope.char_to_line(s.range().end);
                if b.view.sels.iter().any(spans) {
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
            E::Backspace => b.backspace_pairs(pairs()),
            E::Delete => b.delete_forward(),
            E::DeleteWordBack => b.delete_word_back(),
            E::DeleteWordForward => b.delete_word_forward(),
            E::Move(m, extend) => {
                let page = rows.saturating_sub(1).max(1);
                // In column selection mode Shift+arrows, Home and End move
                // the block's corner instead.
                let block = self.column_mode && *extend && b.blocks_supported() && b.extend_block(*m, page, tab_width);
                if !block {
                    let rows = Rows::view(&b.view.folds, &b.view.wrap, &b.rope, b.version);
                    apply_motion_all(&b.rope, &rows, &mut b.view.sels, *m, *extend, page);
                }
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
            E::Copy | E::Cut => {
                let pieces = b.clip_pieces();
                let text = join_pieces(&pieces, b.line_ending.as_str());
                self.multi_clip = (pieces.len() > 1).then(|| (text.clone(), pieces));
                cx.effects.push(Effect::Clipboard(text));
                if *a == E::Cut {
                    b.cut_carets();
                }
            }
            // The app reads the clipboard (`App::paste`) and inserts it as
            // `InsertText`.
            E::Paste => {}
            E::Escape => {
                // The find bar goes first, then the other carets (the primary
                // keeps its selection), then the selection.
                if b.view.search.is_some() {
                    self.close_find();
                } else if b.view.sels.is_multi() {
                    b.view.sels.collapse_to_primary();
                    b.view.occ_stack.clear();
                    b.undo.break_group = true;
                } else {
                    let mut s = b.view.sel();
                    s.collapse();
                    b.view.set_sel(s);
                }
            }
            E::ScrollUp(n) | E::ScrollDown(n) => {
                let delta = if matches!(a, E::ScrollUp(_)) { -(*n as isize) } else { *n as isize };
                let top = b.rows().step_row(b.view.top(), delta);
                b.view.set_top(top);
            }
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
                let mut moved = false;
                for (s, d) in b.view.sels.iter_mut_desired() {
                    if let Some((_, other)) = brackets::pair_at(&b.rope, tree, s.head) {
                        s.set_head(other, false);
                        *d = None;
                        moved = true;
                    }
                }
                if moved {
                    b.view.sels.normalize();
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
            E::AddNextOccurrence => {
                if !b.add_next_occurrence() {
                    cx.notify(Level::Info, "No more occurrences");
                }
            }
            E::UnselectOccurrence => b.unselect_occurrence(),
            E::SelectAllOccurrences => {
                if let Some(n) = b.select_all_occurrences() {
                    cx.notify(Level::Info, format!("{n} occurrences; carets on the first {}", selection::MAX_CARETS));
                }
            }
            E::CloneCaretAbove | E::CloneCaretBelow => b.clone_caret(*a == E::CloneCaretAbove),
            E::FoldCollapse
            | E::FoldCollapseRecursive
            | E::FoldExpand
            | E::FoldExpandRecursive
            | E::FoldCollapseAll
            | E::FoldExpandAll
            | E::FoldSelection => {
                match a {
                    E::FoldCollapse | E::FoldCollapseRecursive => b.fold_collapse(*a == E::FoldCollapseRecursive),
                    E::FoldExpand | E::FoldExpandRecursive => b.fold_expand(*a == E::FoldExpandRecursive),
                    E::FoldCollapseAll => b.fold_collapse_all(),
                    E::FoldExpandAll => b.view.folds.clear(),
                    _ => b.fold_selection(),
                }
                // The rows above the caret changed: keep it in view.
                b.view.follow_key = None;
            }
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
            | E::SaveAs
            | E::SaveAll
            | E::Format
            | E::GotoLine
            | E::NavBack
            | E::NavForward
            | E::ShowBookmarks
            | E::ToggleNumberedBookmark
            | E::GotoNumberedBookmark(_)
            | E::LineEndingLf
            | E::LineEndingCrlf
            | E::SetLanguage
            | E::ToggleBlame
            | E::ToggleColumnMode
            | E::ToggleSoftWrap
            | E::Merge(_) => {}
        }
        // A motion or edit that put a caret into a fold unfolds it.
        if let Some(b) = self.active_buffer_mut() {
            b.reveal_carets();
        }
        self.note_jump(jump_from);
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, area: Rect, cx: &mut Ctx) -> bool {
        let handled = self.mouse(m, area, cx);
        if let Some(b) = self.active_buffer_mut() {
            b.reveal_carets();
        }
        handled
    }

    fn mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        // The conflict resolver's glyphs left of the gutter.
        if m.kind == MouseEventKind::Down(MouseButton::Left) && self.merge_mouse(m.x, m.y, cx.theme) {
            return true;
        }
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
        // The row on screen row `y` (the last one below the text).
        let row_at = |b: &Buffer, y: u16| -> Row {
            let rows = b.rows();
            rows.at_offset(b.view.top(), y.saturating_sub(text_area.y) as usize).unwrap_or(rows.last_row())
        };
        let line_at = |b: &Buffer, y: u16| -> usize { row_at(b, y).line };
        // The char under the pointer; in the gutter, the row's first char.
        let pos_at = |b: &Buffer, x: u16, y: u16| -> usize {
            let (rows, row) = (b.rows(), row_at(b, y));
            if x < text_area.x {
                return rows.seg_range(&b.rope, row).start;
            }
            let scroll_col = if rows.wraps() { 0 } else { b.view.scroll_col };
            view::row_x_to_char(&b.rope, &rows, row, (x - text_area.x) as usize + scroll_col, tab_width)
        };
        // The (line, visual column) under the pointer for column blocks;
        // the column is not clamped to the line.
        let cell_at =
            |b: &Buffer, x: u16, y: u16| -> (usize, usize) { (line_at(b, y), x.saturating_sub(text_area.x) as usize + b.view.scroll_col) };
        // A click in the blame column shows that line's commit (on its
        // first row: the others show none).
        if m.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(blame) = &b.blame
            && m.x < text_area.x.saturating_sub(view::gutter_width(b, 0)) + blame::WIDTH
            && m.y >= text_area.y
            && m.y < text_area.bottom()
        {
            let row = row_at(b, m.y);
            if let Some(c) = blame.commit_at(row.line).filter(|_| row.seg == 0) {
                cx.effects.push(Effect::Git(crate::git::GitRequest::ShowCommit(c.sha.clone())));
            }
            return true;
        }
        // A click on a git mark offers the change commands for that line.
        if m.kind == MouseEventKind::Down(MouseButton::Left) && m.x + 3 == text_area.x && m.y >= text_area.y && m.y < text_area.bottom() {
            let Row { line, seg } = row_at(b, m.y);
            if seg == 0 && gutter::mark_at(&b.gutter, line).is_some() {
                b.view.set_sel(Selection::point(b.rope.line_to_char(line)));
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
        // The fold marker column collapses or expands there (on a line's
        // first row); a click on a fold's placeholder (after the header's
        // last row) expands it, the caret at the header's end.
        let row_here = |b: &Buffer| (m.y >= text_area.y).then(|| b.rows().at_offset(b.view.top(), (m.y - text_area.y) as usize)).flatten();
        if m.kind == MouseEventKind::Down(MouseButton::Left)
            && m.y < text_area.bottom()
            && let Some(row) = row_here(b)
        {
            let line = row.line;
            if m.x + 2 == text_area.x {
                if row.seg == 0 {
                    b.toggle_fold(line);
                }
                return true;
            }
            let end = b.rope.line_to_char(line) + selection::line_len_no_eol(&b.rope, line);
            let rows = b.rows();
            let scroll_col = if rows.wraps() { 0 } else { b.view.scroll_col };
            let (end_row, end_x) = view::char_to_row_x(&b.rope, &rows, end, tab_width);
            let past_end = m.x >= text_area.x && end_row == row && (m.x - text_area.x) as usize + scroll_col >= end_x;
            if past_end && b.view.folds.expand_at(line, &b.rope, false) {
                b.view.set_sel(Selection::point(end));
                b.undo.break_group = true;
                return true;
            }
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let p = pos_at(b, m.x, m.y);
                // Alt+click: a caret there, or away with the one there.
                if m.mods.contains(KeyModifiers::ALT) && m.x >= text_area.x {
                    match b.view.sels.index_at(p).filter(|_| b.view.sels.is_multi()) {
                        Some(i) => b.view.sels.remove(i),
                        None => {
                            b.view.sels.push(Selection::point(p), true);
                        }
                    }
                    b.undo.break_group = true;
                    self.drag = Some((g, DragMode::AltPending(cell_at(b, m.x, m.y))));
                    return true;
                }
                let from = nav_pos(b);
                if b.rope.char_to_line(b.view.sel().head).abs_diff(b.rope.char_to_line(p)) > NAV_JUMP_LINES {
                    self.nav.push(from);
                }
                // Ctrl+click: the caret to the clicked char, then go to
                // what is defined there.
                if m.mods.contains(KeyModifiers::CONTROL) && m.clicks == 1 && m.x >= text_area.x {
                    b.view.set_sel(Selection::point(p));
                    b.undo.break_group = true;
                    cx.effects.push(Effect::Lsp(crate::lsp::LspRequest::Definition));
                    return true;
                }
                let extend = m.mods.contains(KeyModifiers::SHIFT);
                match m.clicks {
                    2 => {
                        let r = word_at(&b.rope, p);
                        b.view.set_sel(Selection::new(r.start, r.end));
                        self.drag = Some((g, DragMode::Word(r.start, r.end)));
                    }
                    3 => {
                        let r = view_line_range(b, p);
                        b.view.set_sel(Selection::new(r.start, r.end));
                        self.drag = Some((g, DragMode::Line(r.start, r.end)));
                    }
                    // Column selection mode: a drag makes a block, Shift+click
                    // moves the block's corner.
                    _ if self.column_mode && b.blocks_supported() => {
                        let head = cell_at(b, m.x, m.y);
                        let anchor = if extend { b.current_block(tab_width).anchor } else { head };
                        if extend {
                            b.set_block(view_state::Block { anchor, head }, tab_width);
                        } else {
                            b.view.set_sel(Selection::point(p));
                        }
                        self.drag = Some((g, DragMode::Column(anchor)));
                    }
                    _ => {
                        let mut s = b.view.sel();
                        s.set_head(p, extend);
                        b.view.set_sel(s);
                        self.drag = Some((g, DragMode::Char));
                    }
                }
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
                        b.view.set_sel(if p < s { Selection::new(e, w.start) } else { Selection::new(s, w.end.max(e)) });
                    }
                    DragMode::Line(s, e) => {
                        let r = view_line_range(b, p);
                        b.view.set_sel(if r.start < s { Selection::new(e, r.start) } else { Selection::new(s, r.end.max(e)) });
                    }
                    DragMode::Char => {
                        let mut s = b.view.sel();
                        s.set_head(p, true);
                        b.view.set_sel(s);
                    }
                    // Alt+drag: a column block once the pointer leaves the
                    // pressed cell (Alt+Shift+drag too).
                    DragMode::AltPending(anchor) | DragMode::Column(anchor) => {
                        let head = cell_at(b, m.x, m.y);
                        if matches!(mode, DragMode::AltPending(_)) && (head == anchor || !b.blocks_supported()) {
                            return true;
                        }
                        b.set_block(view_state::Block { anchor, head }, tab_width);
                        self.drag = Some((g, DragMode::Column(anchor)));
                    }
                }
                // Auto-scroll while dragging past the edges.
                let delta = if m.y < text_area.y { -1 } else { (m.y >= text_area.bottom()) as isize };
                if delta != 0 {
                    let top = b.rows().step_row(b.view.top(), delta);
                    b.view.set_top(top);
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
    ///
    /// With the conflict resolver open its header takes the top row, and
    /// the pane leading the linked scroll is drawn first so the others can
    /// follow it.
    pub fn render(&mut self, frame: &mut Frame, tabs: Rect, area: Rect, focused: bool, theme: &Theme) {
        self.sync_parked();
        let mut center = Rect::new(area.x, tabs.y, area.width, tabs.height + area.height);
        self.center = center;
        if self.merge.is_some() {
            self.merge_fit(center.width);
            let header = Rect::new(center.x, center.y, center.width, center.height.min(1));
            self.render_merge_header(frame, header, theme);
            (center.y, center.height) = (center.y + header.height, center.height - header.height);
        }
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
        let master = self.merge_master();
        let shows = |ed: &Editor, g: GroupId| ed.group(g).and_then(TabGroup::active_id);
        match master {
            Some(id) => leaves.sort_by_key(|(g, _)| shows(self, *g) != Some(id)),
            None => leaves.sort_by_key(|(g, _)| *g == self.focused),
        }
        let decors: std::collections::HashMap<BufferId, view::LineDecor> = match &self.merge {
            Some(m) => m.ids.iter().filter_map(|id| Some((*id, self.merge_decor(*id, theme)?))).collect(),
            None => Default::default(),
        };
        for (n, (g, r)) in leaves.into_iter().enumerate() {
            let Some(gi) = self.group_index(g) else { continue };
            if let Some(slot) = self.groups[gi].active {
                self.make_live(g, slot);
            }
            let strip = tabs.height.min(r.height);
            let tabs_r = Rect::new(r.x, r.y, r.width, strip);
            let body = Rect::new(r.x, r.y + strip, r.width, r.height - strip);
            let shown = shows(self, g);
            let follow = master.is_none_or(|id| shown == Some(id));
            let decor = shown.and_then(|id| decors.get(&id));
            self.render_group(frame, gi, tabs_r, body, focused && g == self.focused, (follow, decor), theme);
            if n == 0
                && let Some(id) = master
            {
                self.merge_follow(id);
            }
        }
        if master.is_some() {
            self.merge_paint_splitters(frame, &decors);
            self.merge_note_tops();
        }
    }

    /// Draw group `gi` (an index into `groups`): its tab bar in `tabs`, its
    /// shown tab in `area`. `focused` is whether keys go to it; `follow`
    /// whether the view scrolls to a caret that moved, `decor` the
    /// resolver's marks.
    #[allow(clippy::too_many_arguments)]
    fn render_group(
        &mut self,
        frame: &mut Frame,
        gi: usize,
        tabs: Rect,
        area: Rect,
        focused: bool,
        (follow, decor): (bool, Option<&view::LineDecor>),
        theme: &Theme,
    ) {
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
            if area.height > 2 && area.width > 10 {
                // Centred lines, broken between the phrases (or words) so a
                // narrow pane cuts nothing.
                let max = area.width.saturating_sub(2) as usize;
                let mut lines = crate::util::text::wrap_phrases(&EMPTY_HINT, max);
                let h = area.height as usize;
                if lines.len() > h {
                    lines.truncate(h);
                    let last = format!("{} …", lines[h - 1]);
                    lines[h - 1] = crate::util::text::fit(&last, max);
                }
                // One line sits on the middle row, as before; more centre on it.
                let top = area.y + (area.height + 1 - lines.len() as u16) / 2;
                for (row, line) in lines.iter().enumerate() {
                    let x = area.x + area.width.saturating_sub(line.width() as u16) / 2;
                    screen.set_stringn(x, top + row as u16, line, max, theme.dim);
                }
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
            follow,
            decor,
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

/// The clipboard text for `pieces` copied at several carets: one after the
/// other, a line break between those that do not end with one.
fn join_pieces(pieces: &[String], eol: &str) -> String {
    let mut out = String::new();
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(p);
    }
    out
}

/// What each caret of `b` gets when `s` (in the buffer's line ending) is
/// pasted, or `None` for all of `s` at every caret: the pieces of a copy
/// made at as many carets (`multi_clip`, its text and pieces), else one
/// line of `s` per caret when it has exactly that many lines.
fn paste_pieces(b: &Buffer, s: &str, multi_clip: Option<&(String, Vec<String>)>) -> Option<Vec<String>> {
    let n = b.view.sels.len();
    if n < 2 {
        return None;
    }
    let eol = b.line_ending.as_str();
    let lf = |t: &str| t.replace("\r\n", "\n");
    if let Some((text, pieces)) = multi_clip
        && pieces.len() == n
        && lf(text) == lf(s)
    {
        return Some(pieces.iter().map(|p| lf(p).replace('\n', eol)).collect());
    }
    let body = s.strip_suffix(eol).unwrap_or(s);
    let lines: Vec<String> = body.split(eol).map(str::to_string).collect();
    (lines.len() == n).then_some(lines)
}

/// The line of `p` with its line break, as a triple click selects it; a
/// folded line takes the lines its fold hides along.
fn view_line_range(b: &Buffer, p: usize) -> std::ops::Range<usize> {
    let line = b.rope.char_to_line(p.min(b.rope.len_chars()));
    let r = line_range(&b.rope, line);
    match Rows::new(&b.view.folds, b.rope.len_lines()).next(line) {
        Some(n) => r.start..b.rope.line_to_char(n),
        None if b.view.folds.header_at(line).is_some() => r.start..b.rope.len_chars(),
        None => r,
    }
}

/// The wheel over a view: three rows up or down, four columns sideways
/// (none when wrapped).
fn wheel(b: &mut Buffer, kind: MouseEventKind) -> bool {
    let wraps = b.rows().wraps();
    let step = |b: &Buffer, d: isize| b.rows().step_row(b.view.top(), d);
    match kind {
        MouseEventKind::ScrollUp => b.view.set_top(step(b, -3)),
        MouseEventKind::ScrollDown => b.view.set_top(step(b, 3)),
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight if wraps => {}
        MouseEventKind::ScrollLeft => b.view.scroll_col = b.view.scroll_col.saturating_sub(4),
        MouseEventKind::ScrollRight => b.view.scroll_col += 4,
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
    use crate::actions::Motion;
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
            assert_eq!(ed.active_buffer().unwrap().view.sel().range(), 0..3);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::NONE), cx));
            assert_eq!(ed.active_buffer().unwrap().view.sel().range(), 8..11);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::SHIFT), cx));
            assert_eq!(ed.active_buffer().unwrap().view.sel().range(), 0..3);
            // Shift+Enter from the first match wraps to the last.
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::SHIFT), cx));
            assert_eq!(ed.active_buffer().unwrap().view.sel().range(), 16..19);
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
            assert_eq!(ed.active_buffer().unwrap().view.sel().range(), 0..3);
            assert!(ed.active_buffer().unwrap().view.search.is_none());
        });
        assert_eq!(notes.iter().filter(|n| *n == "Search wrapped").count(), 2);
    }

    #[test]
    fn escape_closes_find_then_drops_carets_then_the_selection() {
        let mut ed = editor_with("a.txt", "one two three");
        act(&mut ed, EditorAction::Find);
        let b = ed.active_buffer_mut().unwrap();
        b.view.sels = selection::Selections::new(vec![Selection::point(1), Selection::new(4, 7)], 1);
        act(&mut ed, EditorAction::Escape);
        let b = ed.active_buffer().unwrap();
        assert!(b.view.search.is_none());
        assert_eq!(b.view.sels.len(), 2, "the find bar goes first");
        act(&mut ed, EditorAction::Escape);
        assert_eq!(ed.active_buffer().unwrap().view.sels.as_slice(), [Selection::new(4, 7)], "the primary keeps its selection");
        act(&mut ed, EditorAction::Escape);
        assert_eq!(ed.active_buffer().unwrap().view.sels.as_slice(), [Selection::point(7)]);
    }

    fn clipboard(fx: &[Effect]) -> Option<&str> {
        fx.iter().find_map(|e| if let Effect::Clipboard(t) = e { Some(t.as_str()) } else { None })
    }

    #[test]
    fn copy_and_paste_at_several_carets() {
        let mut ed = editor_with("a.txt", "a1\nb2\nc3\n");
        let sels = |v: Vec<Selection>| selection::Selections::new(v, 0);
        ed.active_buffer_mut().unwrap().view.sels = sels(vec![Selection::new(1, 2), Selection::new(4, 5), Selection::new(7, 8)]);
        let fx = act(&mut ed, EditorAction::Copy);
        assert_eq!(clipboard(&fx), Some("1\n2\n3"));
        // Back at the line starts: each caret gets its own piece.
        ed.active_buffer_mut().unwrap().view.sels = sels(vec![Selection::point(0), Selection::point(3), Selection::point(6)]);
        act(&mut ed, EditorAction::InsertText("1\n2\n3".into()));
        assert_eq!(ed.active_buffer().unwrap().text(), "1a1\n2b2\n3c3\n");
        // Text from elsewhere with one line per caret is split too.
        act(&mut ed, EditorAction::InsertText("x\ny\nz\n".into()));
        assert_eq!(ed.active_buffer().unwrap().text(), "1xa1\n2yb2\n3zc3\n");
        // Otherwise all of it goes to every caret.
        act(&mut ed, EditorAction::InsertText("-".into()));
        assert_eq!(ed.active_buffer().unwrap().text(), "1x-a1\n2y-b2\n3z-c3\n");
        // Cut with empty carets takes their lines, once each.
        ed.active_buffer_mut().unwrap().view.sels = sels(vec![Selection::point(0), Selection::point(1), Selection::point(13)]);
        let fx = act(&mut ed, EditorAction::Cut);
        assert_eq!(clipboard(&fx), Some("1x-a1\n3z-c3\n"));
        assert_eq!(ed.active_buffer().unwrap().text(), "2y-b2\n");
    }

    #[test]
    fn carets_render_as_cells_and_alt_click_adds_and_removes() {
        let mut ed = editor_with("a.txt", "abc\nde\nxyz\n");
        ed.active_buffer_mut().unwrap().view.sels =
            selection::Selections::new(vec![Selection::new(0, 2), Selection::point(6), Selection::point(9)], 2);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 6)).unwrap();
        let theme = Theme::default();
        let mut draw = |ed: &mut Editor, focused: bool| {
            term.draw(|f| ed.render(f, Rect::new(0, 0, 40, 1), Rect::new(0, 1, 40, 5), focused, &theme)).unwrap().buffer.clone()
        };
        let screen = draw(&mut ed, true);
        let x0 = ed.text_area().x;
        let reversed = |x: u16, y: u16| screen[(x, y)].modifier.contains(ratatui::style::Modifier::REVERSED);
        assert_eq!(screen[(x0, 1)].bg, theme.selection_bg, "the selection");
        assert_eq!(screen[(x0 + 1, 1)].bg, theme.selection_bg);
        assert!(reversed(x0 + 2, 1), "the selection's head");
        assert!(reversed(x0 + 2, 2), "a caret at the line end: the cell after the text");
        assert!(!reversed(x0 + 2, 3), "the primary is the terminal cursor");
        assert_eq!(ed.focused_group().caret_cell, Some((x0 + 2, 3)));
        assert_eq!(ed.status_position().as_deref(), Some("Ln 3, Col 3 (3 carets, 2 chars)"));
        // Without focus every caret of a multi-caret view is a cell.
        let screen = draw(&mut ed, false);
        assert!(screen[(x0 + 2, 3)].modifier.contains(ratatui::style::Modifier::REVERSED));
        // Alt+click on a caret removes it, elsewhere adds one (the primary).
        let alt_click = |ed: &mut Editor, x: u16, y: u16| {
            let m = Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks: 1, mods: KeyModifiers::ALT };
            with_cx(|cx| {
                ed.handle_mouse(&m, Rect::default(), cx);
                let up = Mouse { kind: MouseEventKind::Up(MouseButton::Left), ..m };
                ed.handle_mouse(&up, Rect::default(), cx);
            });
        };
        alt_click(&mut ed, x0 + 2, 2);
        assert_eq!(ed.active_buffer().unwrap().view.sels.heads().collect::<Vec<_>>(), [2, 9]);
        alt_click(&mut ed, x0, 2);
        let sels = &ed.active_buffer().unwrap().view.sels;
        assert_eq!((sels.heads().collect::<Vec<_>>(), sels.primary()), (vec![2, 4, 9], Selection::point(4)));
        // A plain click goes back to one caret.
        let m = Mouse { kind: MouseEventKind::Down(MouseButton::Left), x: x0, y: 1, clicks: 1, mods: KeyModifiers::NONE };
        with_cx(|cx| {
            ed.handle_mouse(&m, Rect::default(), cx);
        });
        assert_eq!(ed.active_buffer().unwrap().view.sels.as_slice(), [Selection::point(0)]);
    }

    /// Send mouse events with `mods` to `ed`: a press at the first cell,
    /// motion over the others, a release at the last.
    fn mouse_path(ed: &mut Editor, mods: KeyModifiers, cells: &[(u16, u16)]) {
        let ev = |kind, (x, y): (u16, u16)| Mouse { kind, x, y, clicks: 1, mods };
        with_cx(|cx| {
            ed.handle_mouse(&ev(MouseEventKind::Down(MouseButton::Left), cells[0]), Rect::default(), cx);
            for &c in &cells[1..] {
                ed.handle_mouse(&ev(MouseEventKind::Drag(MouseButton::Left), c), Rect::default(), cx);
            }
            ed.handle_mouse(&ev(MouseEventKind::Up(MouseButton::Left), cells[cells.len() - 1]), Rect::default(), cx);
        });
    }

    fn sel_texts(ed: &Editor) -> Vec<String> {
        let b = ed.active_buffer().unwrap();
        b.view.sels.iter().map(|s| b.rope.slice(s.range()).to_string()).collect()
    }

    #[test]
    fn alt_drag_makes_a_column_block_and_alt_click_still_adds_carets() {
        let mut ed = editor_with("a.cfg", "\tab日本語x\n\tab日本語x\nab\n\tab日本語x\n");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();
        let theme = Theme::default();
        term.draw(|f| ed.render(f, Rect::new(0, 0, 40, 1), Rect::new(0, 1, 40, 7), true, &theme)).unwrap();
        let (x0, y0) = (ed.text_area().x, ed.text_area().y);
        // Cells to columns and back: from col 5 (b) on the first line to
        // col 7 (the right half of 日) on the last, through the short line.
        mouse_path(&mut ed, KeyModifiers::ALT, &[(x0 + 5, y0), (x0 + 6, y0 + 2), (x0 + 7, y0 + 3)]);
        assert_eq!(sel_texts(&ed), ["b日", "b日", "", "b日"]);
        let b = ed.active_buffer().unwrap();
        assert_eq!(b.view.sels.primary_index(), 3);
        assert_eq!(b.view.block, Some(view_state::Block { anchor: (0, 5), head: (3, 7) }));
        // Each caret head maps back to the cell its line was cut at.
        let heads: Vec<usize> = b.view.sels.heads().map(|h| view::visual_col(&b.rope, h, 4)).collect();
        assert_eq!(heads, [8, 8, 2, 8]);
        // Alt+Shift+drag too, right to left past the line ends.
        mouse_path(&mut ed, KeyModifiers::ALT | KeyModifiers::SHIFT, &[(x0 + 30, y0 + 1), (x0 + 12, y0)]);
        assert_eq!(sel_texts(&ed), ["x", "x"]);
        assert!(ed.active_buffer().unwrap().view.sels.iter().all(|s| s.head < s.anchor));
        // Typing goes to every line, one undo step.
        act(&mut ed, EditorAction::InsertChar('Q'));
        assert_eq!(ed.active_buffer().unwrap().text(), "\tab日本語Q\n\tab日本語Q\nab\n\tab日本語x\n");
        act(&mut ed, EditorAction::Undo);
        assert_eq!(sel_texts(&ed), ["x", "x"]);
        // Alt+click without moving keeps adding a caret.
        mouse_path(&mut ed, KeyModifiers::ALT, &[(x0, y0 + 2)]);
        assert_eq!(ed.active_buffer().unwrap().view.sels.len(), 3);
        // A motion report on the pressed cell is not a drag either.
        mouse_path(&mut ed, KeyModifiers::ALT, &[(x0 + 1, y0 + 3), (x0 + 1, y0 + 3)]);
        assert_eq!(ed.active_buffer().unwrap().view.sels.len(), 4);
    }

    #[test]
    fn column_mode_keys_drag_and_status() {
        let mut ed = editor_with("a.cfg", "one\ntwo\nx\nfour\n");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();
        let theme = Theme::default();
        term.draw(|f| ed.render(f, Rect::new(0, 0, 40, 1), Rect::new(0, 1, 40, 7), true, &theme)).unwrap();
        let (x0, y0) = (ed.text_area().x, ed.text_area().y);
        act(&mut ed, EditorAction::Move(Motion::Right, false));
        act(&mut ed, EditorAction::ToggleColumnMode);
        assert_eq!(ed.status_position().as_deref(), Some("Ln 1, Col 2 Column"));
        for _ in 0..3 {
            act(&mut ed, EditorAction::Move(Motion::Down, true));
        }
        assert_eq!(ed.status_position().as_deref(), Some("Ln 4, Col 2 (4 carets) Column"));
        act(&mut ed, EditorAction::Move(Motion::LineEnd, true));
        assert_eq!(sel_texts(&ed), ["ne", "wo", "", "our"]);
        act(&mut ed, EditorAction::Move(Motion::Left, true));
        act(&mut ed, EditorAction::Move(Motion::Left, true));
        act(&mut ed, EditorAction::Move(Motion::Left, true));
        act(&mut ed, EditorAction::InsertText("_".into()));
        assert_eq!(ed.active_buffer().unwrap().text(), "o_ne\nt_wo\nx_\nf_our\n");
        // A plain drag is a block; Shift+click moves its corner.
        mouse_path(&mut ed, KeyModifiers::NONE, &[(x0, y0), (x0 + 2, y0 + 1)]);
        assert_eq!(sel_texts(&ed), ["o_", "t_"]);
        mouse_path(&mut ed, KeyModifiers::SHIFT, &[(x0 + 3, y0 + 3)]);
        assert_eq!(sel_texts(&ed), ["o_n", "t_w", "x_", "f_o"]);
        // Off again: Shift+Down extends the selection as usual.
        act(&mut ed, EditorAction::ToggleColumnMode);
        act(&mut ed, EditorAction::Escape);
        act(&mut ed, EditorAction::Escape);
        act(&mut ed, EditorAction::Move(Motion::Down, true));
        assert_eq!(ed.active_buffer().unwrap().view.sels.len(), 1);
        assert_eq!(ed.status_position().as_deref(), Some("Ln 5, Col 1 (3 chars)"));
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
            assert_eq!(b.view.sel().range(), 0..2);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::NONE), cx));
            let b = ed.active_buffer().unwrap();
            assert_eq!(b.text(), "<1> a2 b3");
            assert_eq!(b.view.sel().range(), 4..6);
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
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(8));
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
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(end));
        with_cx(|cx| {
            ed.handle_action(&EditorAction::InsertChar('}'), cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "fn f() {\n    let s = \"{\";\n}");
        // Typing `}` at an existing closer steps over it, line unchanged.
        let mut ed = editor_with("a.rs", "fn f() {\n    }");
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(13));
        with_cx(|cx| {
            ed.handle_action(&EditorAction::InsertChar('}'), cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "fn f() {\n    }");
        assert_eq!(ed.active_buffer().unwrap().view.sel().head, 14);
    }

    #[cfg(feature = "lang-css")]
    #[test]
    fn css_ctrl_slash_falls_back_to_block_comment() {
        let mut ed = editor_with("a.css", "a {\n    color: red;\n}\n");
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(10));
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleComment, cx);
        });
        assert!(notes.is_empty(), "{notes:?}");
        let b = ed.active_buffer().unwrap();
        assert_eq!(b.text(), "a {\n    /* color: red; */\n}\n");
        assert_eq!(b.view.sel().head, 13, "caret stays on the same character");
        with_cx(|cx| {
            ed.handle_action(&EditorAction::ToggleComment, cx);
        });
        assert_eq!(ed.active_buffer().unwrap().text(), "a {\n    color: red;\n}\n");
        // A selection over two lines becomes one block from the first indent.
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::new(2, 10));
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
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(8));
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
    fn apply_config_changes_defaults_not_views() {
        let mut ed = editor_with("a.rs", "fn main() {}\n");
        ed.open_text(PathBuf::from("/tmp/n.md"), "notes\n", false, &Theme::default());
        assert!(!ed.buffers[0].view.wrap.on && ed.buffers[1].view.wrap.on);
        let config = Config::parse("[editor]\nsoft_wrap = true\nsoft_wrap_files = []\nscrolloff = 9\n").unwrap();
        ed.apply_config(&config);
        assert!(!ed.buffers[0].view.wrap.on, "an open tab keeps its wrap");
        assert!(ed.buffers[1].view.wrap.on);
        assert!(ed.soft_wrap_for(Some(Path::new("b.rs"))));
        assert_eq!(ed.scrolloff, 9);
        ed.apply_config(&Config::parse("[editor]\nsoft_wrap_files = []\n").unwrap());
        assert!(!ed.soft_wrap_for(Some(Path::new("c.md"))));
        assert!(ed.buffers[1].view.wrap.on);
    }

    #[test]
    fn turning_idle_autosave_on_waits() {
        let mut ed = editor_with("a.txt", "one\n");
        ed.buffers[0].insert("x", TxKind::Typing);
        // The edit was made long ago, while autosave was off.
        ed.last_edit = Instant::now() - Duration::from_secs(60);
        ed.buffers[0].autosave_seen = ed.buffers[0].version;
        let now = Instant::now();
        assert!(!ed.autosave_due(now));
        ed.apply_config(&Config::parse("[editor]\nautosave = \"idle\"\n").unwrap());
        let now = Instant::now();
        assert!(!ed.autosave_due(now), "no save at once");
        assert!(ed.autosave_due(now + Duration::from_secs(2)));
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
            assert_eq!(items, &vec!["◆ a.txt:2 two".to_string()]);
            let DialogId::Bookmarks(t) = d.id else { panic!() };
            ed.goto_line(3, 1);
            ed.goto_bookmark_target(&t[0], cx);
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

    #[test]
    fn recover_text_is_undoable_and_holds_autosave() {
        let dir = std::env::temp_dir().join(format!("jetl-test-editor-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "disk\n").unwrap();
        let mut ed = Editor::new(&Config::default());
        ed.open_text(path.clone(), "disk\n", false, &Theme::default());
        assert!(ed.recover_text(0, "recovered\n"));
        assert!(!ed.recover_text(0, "recovered\n"), "already the same");
        let b = &ed.buffers[0];
        assert_eq!(b.text(), "recovered\n");
        assert!(b.dirty() && b.recovered);
        assert!(!b.autosave_candidate(), "no autosave before an explicit save");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk\n");
        assert!(ed.buffers[0].undo());
        assert_eq!(ed.buffers[0].text(), "disk\n");
        assert!(!ed.buffers[0].dirty(), "undo goes back to the file's text");
        assert!(ed.buffers[0].redo());
        with_cx(|cx| assert!(ed.save(0, cx)));
        assert!(!ed.buffers[0].recovered && !ed.buffers[0].dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "recovered\n");
        let i = ed.open_untitled("scratch");
        let b = &ed.buffers[i];
        assert_eq!((b.path.as_deref(), b.name.as_str()), (None, "scratch"));
        assert_eq!(ed.active(), Some(i));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn save_as_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-save-as-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn save_as_gives_an_untitled_buffer_its_path() {
        let dir = save_as_dir("untitled");
        let mut ed = Editor::new(&Config::default());
        let i = ed.open_untitled("notes (recovered)");
        ed.buffers[i].insert("fn main() {}\n", TxKind::Typing);
        ed.buffers[i].recovered = true;
        let path = dir.join("a.rs");
        let mut fx = Vec::new();
        let notes = with_cx(|cx| {
            assert!(ed.save_as(i, path.clone(), cx));
            fx = std::mem::take(&mut cx.effects);
        });
        assert!(notes.is_empty(), "{notes:?}");
        let b = &ed.buffers[i];
        assert_eq!((b.path.as_deref(), b.name.as_str()), (Some(path.as_path()), "a.rs"));
        assert_eq!(b.language().map(|l| l.name), Some("rust"));
        assert!(b.syntax.is_some() && b.disk_mtime.is_some());
        assert!(!b.dirty() && !b.recovered);
        assert!(matches!(fx.as_slice(), [Effect::FileSaved { path: p, quiet: false }] if *p == path));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn main() {}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_as_of_a_file_writes_a_copy_and_follows_it() {
        let dir = save_as_dir("copy");
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a\n").unwrap();
        let mut ed = Editor::new(&Config::default());
        let i = ed.open_text(a.clone(), "a\n", false, &Theme::default());
        ed.buffers[i].insert("mine ", TxKind::Typing);
        with_cx(|cx| assert!(ed.save_as(i, b.clone(), cx)));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n", "the old file is left alone");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "mine a\n");
        assert_eq!(ed.buffers[i].path.as_deref(), Some(b.as_path()));
        assert_eq!(ed.index_of(&a), None);
        assert!(!ed.buffers[i].dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_as_leaves_the_buffer_as_it_was() {
        let dir = save_as_dir("fail");
        let mut ed = Editor::new(&Config::default());
        let i = ed.open_untitled("untitled");
        ed.buffers[i].insert("text\n", TxKind::Typing);
        let notes = with_cx(|cx| assert!(!ed.save_as(i, dir.join("missing/x.txt"), cx)));
        assert_eq!(notes.len(), 1, "{notes:?}");
        let b = &ed.buffers[i];
        assert_eq!((b.path.as_deref(), b.name.as_str()), (None, "untitled"));
        assert!(b.dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_of_an_untitled_buffer_asks_for_a_path() {
        let mut ed = editor_with("a.txt", "a\n");
        ed.buffers[0].insert("x", TxKind::Typing);
        let j = ed.open_untitled("untitled");
        let id = ed.buffers[j].id;
        ed.buffers[j].insert("u", TxKind::Typing);
        let mut fx = Vec::new();
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::Save, cx);
            fx = std::mem::take(&mut cx.effects);
        });
        assert!(notes.is_empty(), "no 'buffer has no file path': {notes:?}");
        assert!(matches!(fx.as_slice(), [Effect::SaveAs { id: x, then: AfterSave::Nothing }] if *x == id));
        assert_eq!(ed.untitled_dirty(None), Some(id));
        assert_eq!(ed.untitled_dirty(Some(&[ed.buffers[0].id])), None);
    }
}
