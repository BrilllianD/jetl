//! Editor: a tab strip of buffers plus the text view.

pub mod buffer;
pub mod gutter;
pub mod search;
pub mod selection;
pub mod syntax;
pub mod undo;
pub mod view;

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::actions::EditorAction;
use crate::app::Ctx;
use crate::config::Config;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::tabs::TabHits;
use buffer::{Buffer, BufferId};
use search::{Field, SearchState};
use selection::{Selection, apply_motion, line_range, word_at};
use undo::{Change, TxKind};
use view::{RenderOpts, col_to_char};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Char,
    Word(usize, usize),
    Line(usize, usize),
}

/// Clickable parts of the find bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindHit {
    QueryField,
    ReplaceField,
    Case,
    Word,
    Regex,
    Prev,
    Next,
    Close,
    ReplaceOne,
    ReplaceAll,
}

/// Query and options of the last search, reused by F3 with the bar closed
/// and when the bar reopens without a selection.
#[derive(Debug, Clone)]
struct LastSearch {
    query: String,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
}

impl LastSearch {
    fn to_state(&self) -> SearchState {
        let mut s = SearchState::new(&self.query, false);
        s.case_sensitive = self.case_sensitive;
        s.whole_word = self.whole_word;
        s.regex = self.regex;
        s
    }
}

pub struct Editor {
    pub buffers: Vec<Buffer>,
    pub active: Option<usize>,
    /// Most recently used buffer ids, most recent first.
    mru: Vec<BufferId>,
    next_id: BufferId,
    pub tab_hits: TabHits,
    tab_scroll: usize,
    text_area: Rect,
    last_area: Rect,
    tab_width: usize,
    scrolloff: usize,
    hscrolloff: usize,
    clipboard: String,
    drag: Option<DragMode>,
    /// Files larger than this open without syntax highlighting.
    max_highlight_bytes: usize,
    last_search: Option<LastSearch>,
    find_area: Rect,
    find_hits: Vec<(FindHit, Rect)>,
}

impl Editor {
    pub fn new(config: &Config) -> Self {
        Self {
            buffers: Vec::new(),
            active: None,
            mru: Vec::new(),
            next_id: 1,
            tab_hits: TabHits::default(),
            tab_scroll: 0,
            text_area: Rect::default(),
            last_area: Rect::default(),
            tab_width: config.editor.tab_width,
            scrolloff: config.editor.scrolloff,
            hscrolloff: config.editor.hscrolloff,
            clipboard: String::new(),
            drag: None,
            max_highlight_bytes: config.editor.max_highlight_bytes,
            last_search: None,
            find_area: Rect::default(),
            find_hits: Vec::new(),
        }
    }

    pub fn active_buffer(&self) -> Option<&Buffer> {
        self.active.and_then(|i| self.buffers.get(i))
    }
    pub fn active_buffer_mut(&mut self) -> Option<&mut Buffer> {
        self.active.and_then(move |i| self.buffers.get_mut(i))
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
    pub fn open_paths(&self) -> Vec<PathBuf> {
        self.buffers.iter().filter_map(|b| b.path.clone()).collect()
    }
    pub fn recent_paths(&self) -> Vec<PathBuf> {
        self.mru.iter().filter_map(|id| self.buffers.iter().find(|b| b.id == *id)).filter_map(|b| b.path.clone()).collect()
    }

    /// Add a loaded file as a new tab and make it active. Syntax
    /// highlighting is attached only below `max_highlight_bytes`.
    pub fn open_text(&mut self, path: PathBuf, text: &str, lossy: bool, theme: &Theme) -> usize {
        if let Some(i) = self.index_of(&path) {
            self.activate_index(i);
            return i;
        }
        let id = self.next_id;
        self.next_id += 1;
        let syntax = if text.len() <= self.max_highlight_bytes { syntax::Syntax::for_path(&path, theme) } else { None };
        let mut buf = Buffer::from_text(id, Some(path), text, lossy);
        buf.syntax = syntax;
        // JetBrains inserts the new tab right after the active one.
        let at = self.active.map(|a| a + 1).unwrap_or(self.buffers.len()).min(self.buffers.len());
        self.buffers.insert(at, buf);
        self.activate_index(at);
        at
    }

    fn activate_index(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        self.active = Some(i);
        let id = self.buffers[i].id;
        self.mru.retain(|x| *x != id);
        self.mru.insert(0, id);
        self.buffers[i].undo.break_group = true;
    }

    /// Put back a session's caret and scroll for tab `i` (clamped to the
    /// file as it is now). The first render still keeps the caret visible.
    pub fn restore_view(&mut self, i: usize, line: usize, col: usize, scroll_line: usize, scroll_col: usize) {
        let Some(b) = self.buffers.get_mut(i) else { return };
        b.goto_line(line, col);
        b.scroll_line = scroll_line.min(b.rope.len_lines().saturating_sub(1));
        b.scroll_col = scroll_col;
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
            self.activate_index(i);
        }
    }

    pub fn activate(&mut self, i: usize, _cx: &mut Ctx) {
        self.activate_index(i);
    }

    /// Close tab `i`, asking first when it has unsaved changes.
    pub fn close(&mut self, i: usize, cx: &mut Ctx) {
        let Some(b) = self.buffers.get(i) else { return };
        if b.dirty() {
            let msg = format!("{} has unsaved changes.", b.name);
            cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::CloseTabDirty(i), "Unsaved changes", &msg, &["Save", "Don't save", "Cancel"])));
            return;
        }
        self.force_close(i);
    }

    pub fn force_close(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        let id = self.buffers[i].id;
        self.buffers.remove(i);
        self.mru.retain(|x| *x != id);
        if self.buffers.is_empty() {
            self.active = None;
            return;
        }
        // Prefer the most recently used remaining tab.
        let next = self.mru.first().and_then(|id| self.buffers.iter().position(|b| b.id == *id)).unwrap_or(i.min(self.buffers.len() - 1));
        self.activate_index(next);
    }

    pub fn close_all(&mut self, cx: &mut Ctx) {
        if self.has_dirty() {
            cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::CloseAllDirty, "Unsaved changes", "Some files have unsaved changes.", &["Save all", "Don't save", "Cancel"])));
            return;
        }
        self.force_close_all();
    }

    pub fn force_close_all(&mut self) {
        self.buffers.clear();
        self.mru.clear();
        self.active = None;
    }

    /// Write buffer `i` to disk atomically. Synchronous on purpose: saves are
    /// small and the result must be known before quitting.
    pub fn save(&mut self, i: usize, cx: &mut Ctx) -> bool {
        let Some(b) = self.buffers.get_mut(i) else { return false };
        let Some(path) = b.path.clone() else {
            cx.notify(Level::Error, "buffer has no file path");
            return false;
        };
        if !b.dirty() {
            return true;
        }
        match write_atomic(&path, &b.text()) {
            Ok(()) => {
                b.mark_saved();
                if b.lossy {
                    cx.notify(Level::Warn, format!("{} was not valid UTF-8; saved re-encoded as UTF-8", b.name));
                    b.lossy = false;
                }
                cx.effects.push(Effect::FileSaved(path));
                true
            }
            Err(e) => {
                cx.notify(Level::Error, format!("save {}: {e}", path.display()));
                false
            }
        }
    }

    pub fn save_all(&mut self, cx: &mut Ctx) -> bool {
        let mut ok = true;
        for i in 0..self.buffers.len() {
            if self.buffers[i].dirty() {
                ok &= self.save(i, cx);
            }
        }
        ok
    }

    pub fn goto_line(&mut self, line: usize, col: usize) {
        if let Some(b) = self.active_buffer_mut() {
            b.goto_line(line.saturating_sub(1), col.saturating_sub(1));
        }
    }

    pub fn status_position(&self) -> Option<String> {
        let b = self.active_buffer()?;
        let (line, _) = b.cursor_line_col();
        let col = view::visual_col(&b.rope, b.sel.head, self.tab_width);
        let sel = b.sel.range();
        if sel.is_empty() {
            Some(format!("Ln {}, Col {}", line + 1, col + 1))
        } else {
            Some(format!("Ln {}, Col {} ({} chars)", line + 1, col + 1, sel.len()))
        }
    }

    pub fn status_line_ending(&self) -> Option<String> {
        self.active_buffer().map(|b| b.line_ending.label().to_string())
    }

    /// Set the HEAD text the gutter compares against; marks are recomputed
    /// on the next [`Editor::update_gutters`].
    pub fn set_diff_base(&mut self, path: &Path, text: Option<String>) {
        let Some(i) = self.index_of(path) else { return };
        let b = &mut self.buffers[i];
        if text.is_none() {
            b.gutter.clear();
        }
        b.diff_base = text;
        b.gutter_version = u64::MAX;
    }

    /// Recompute gutter marks of buffers edited since the last call (run from
    /// the app tick, which debounces typing). Returns true when any changed.
    pub fn update_gutters(&mut self) -> bool {
        let limit = self.max_highlight_bytes;
        let mut changed = false;
        for b in &mut self.buffers {
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
            let old = b.text();
            if old == text {
                continue;
            }
            let caret = b.sel.head.min(text.chars().count());
            b.apply(vec![Change::replace(0, old, text)], Selection::point(caret), TxKind::Other);
            b.mark_saved();
            n += 1;
        }
        n
    }

    pub fn handle_action(&mut self, a: &EditorAction, cx: &mut Ctx) -> bool {
        use EditorAction as E;
        match a {
            E::NextTab => {
                if let Some(i) = self.active {
                    let n = self.buffers.len();
                    if n > 0 {
                        self.activate_index((i + 1) % n);
                    }
                }
                return true;
            }
            E::PrevTab => {
                if let Some(i) = self.active {
                    let n = self.buffers.len();
                    if n > 0 {
                        self.activate_index((i + n - 1) % n);
                    }
                }
                return true;
            }
            E::CloseTab => {
                if let Some(i) = self.active {
                    self.close(i, cx);
                }
                return true;
            }
            E::CloseAllTabs => {
                self.close_all(cx);
                return true;
            }
            E::Switcher => {
                if let Some(id) = self.mru.get(1).copied() {
                    if let Some(i) = self.buffers.iter().position(|b| b.id == id) {
                        self.activate_index(i);
                    }
                }
                return true;
            }
            E::SaveAll => {
                self.save_all(cx);
                return true;
            }
            E::Save => {
                if let Some(i) = self.active {
                    self.save(i, cx);
                }
                return true;
            }
            E::GotoLine => {
                if self.active.is_some() {
                    cx.effects.push(Effect::Dialog(Dialog::input(DialogId::GotoLine, "Go to Line", "[Line] [:column]:", "", "OK")));
                }
                return true;
            }
            _ => {}
        }
        let rows = self.text_area.height as usize;
        let tab_width = self.tab_width;
        let Some(ai) = self.active else { return false };
        let b = &mut self.buffers[ai];
        match a {
            E::InsertChar(c) => {
                let kind = if c.is_whitespace() { TxKind::Other } else { TxKind::Typing };
                b.insert(&c.to_string(), kind);
            }
            E::InsertText(s) => {
                let s = if b.line_ending == buffer::LineEnding::CrLf { s.replace("\r\n", "\n").replace('\n', "\r\n") } else { s.replace("\r\n", "\n") };
                b.insert(&s, TxKind::Other);
            }
            E::InsertNewline => b.newline(),
            E::InsertTab => b.insert_tab(tab_width),
            E::Backspace => b.backspace(),
            E::Delete => b.delete_forward(),
            E::DeleteWordBack => b.delete_word_back(),
            E::Move(m, extend) => {
                let page = rows.saturating_sub(1).max(1);
                apply_motion(&b.rope, &mut b.sel, *m, *extend, &mut b.desired_col, page);
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
                let text = if b.sel.is_empty() { b.line_text(b.cursor_line_col().0) + b.line_ending.as_str() } else { b.selected_text() };
                self.clipboard = text;
                cx.effects.push(Effect::Clipboard(self.clipboard.clone()));
            }
            E::Cut => {
                if b.sel.is_empty() {
                    let (line, _) = b.cursor_line_col();
                    let r = line_range(&b.rope, line);
                    b.sel = Selection::new(r.start, r.end);
                }
                self.clipboard = b.selected_text();
                b.delete_selection();
                cx.effects.push(Effect::Clipboard(self.clipboard.clone()));
            }
            E::Paste => {
                if !self.clipboard.is_empty() {
                    let s = self.clipboard.clone();
                    b.insert(&s, TxKind::Other);
                }
            }
            E::Escape => {
                if b.search.is_some() {
                    self.close_find();
                } else {
                    b.sel.collapse();
                }
            }
            E::ScrollUp(n) => b.scroll_line = b.scroll_line.saturating_sub(*n),
            E::ScrollDown(n) => b.scroll_line = (b.scroll_line + n).min(b.rope.len_lines().saturating_sub(1)),
            E::Find => self.open_find(false),
            E::Replace => self.open_find(true),
            E::FindNext => self.find_step(true, cx),
            E::FindPrev => self.find_step(false, cx),
            E::ReplaceOne => self.replace_one(cx),
            E::ReplaceAll => self.replace_all(cx),
            E::FindToggleCase | E::FindToggleWord | E::FindToggleRegex => {
                if let Some(s) = &mut b.search {
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
                if let Some(s) = &mut b.search {
                    if s.show_replace {
                        s.field = if s.field == Field::Query { Field::Replace } else { Field::Query };
                    }
                    s.focused = true;
                }
            }
            E::ToggleComment => {
                let token = b.path.as_deref().and_then(syntax::detect).and_then(|d| d.line_comment);
                match token {
                    Some(t) => b.toggle_line_comment(t),
                    None => cx.notify(Level::Info, "No line comment for this file type"),
                }
            }
            E::NextTab | E::PrevTab | E::CloseTab | E::CloseAllTabs | E::Switcher | E::Save | E::SaveAll | E::GotoLine => {}
        }
        true
    }

    /// The active buffer's find bar has keyboard focus.
    pub fn find_focused(&self) -> bool {
        self.active_buffer().and_then(|b| b.search.as_ref()).is_some_and(|s| s.focused)
    }

    /// Open the find bar (or refocus it). A single-line selection becomes
    /// the query; otherwise the last query is reused.
    fn open_find(&mut self, replace: bool) {
        let last = self.last_search.clone();
        let Some(b) = self.active_buffer_mut() else { return };
        let sel_text = Some(b.selected_text()).filter(|t| !t.is_empty() && !t.contains('\n'));
        let origin = b.sel.range().start;
        let s = b.search.get_or_insert_with(|| last.map(|l| l.to_state()).unwrap_or_default());
        if let Some(t) = sel_text {
            s.query.set_text(&t);
        }
        s.show_replace = replace;
        s.field = Field::Query;
        s.focused = true;
        s.origin = origin;
        refresh_search(b);
        self.remember_search();
    }

    fn close_find(&mut self) {
        self.remember_search();
        if let Some(b) = self.active_buffer_mut() {
            b.search = None;
        }
    }

    fn remember_search(&mut self) {
        let Some(s) = self.active_buffer().and_then(|b| b.search.as_ref()) else { return };
        if !s.query.text.is_empty() {
            self.last_search = Some(LastSearch { query: s.query.text.clone(), case_sensitive: s.case_sensitive, whole_word: s.whole_word, regex: s.regex });
        }
    }

    /// Select the next/previous match from the selection. With the bar
    /// closed this searches for the last query without opening it.
    fn find_step(&mut self, forward: bool, cx: &mut Ctx) {
        let last = self.last_search.clone();
        let Some(b) = self.active_buffer_mut() else { return };
        let mut temp = None;
        let s = match b.search.as_mut() {
            Some(s) => s,
            None => match last {
                Some(l) => temp.insert(l.to_state()),
                None => {
                    cx.notify(Level::Info, "Nothing to search for; press Ctrl+F");
                    return;
                }
            },
        };
        s.update(&b.rope, b.version);
        if let Some(e) = &s.error {
            cx.notify(Level::Warn, e.clone());
            return;
        }
        let r = b.sel.range();
        let found = if forward { s.first_from(r.end) } else { s.last_before(r.start) };
        let Some(i) = found else {
            if !s.query.text.is_empty() {
                cx.notify(Level::Info, format!("'{}' not found", s.query.text));
            }
            return;
        };
        let m = s.select(i);
        let wrapped = if forward { m.start < r.end } else { m.start >= r.start };
        s.origin = m.start;
        b.sel = Selection::new(m.start, m.end);
        b.desired_col = None;
        b.undo.break_group = true;
        if wrapped {
            cx.notify(Level::Info, "Search wrapped");
        }
    }

    /// Replace the selected match, then select the next one. When the
    /// selection is not a match, only move to the next match.
    fn replace_one(&mut self, cx: &mut Ctx) {
        let Some(b) = self.active_buffer_mut() else { return };
        let Some(s) = b.search.as_mut() else { return };
        s.update(&b.rope, b.version);
        let r = b.sel.range();
        let i = s.matches.partition_point(|m| m.start < r.start);
        if s.matches.get(i) != Some(&r) {
            self.find_step(true, cx);
            return;
        }
        let rep = if s.regex {
            let bytes = b.rope.char_to_byte(r.start)..b.rope.char_to_byte(r.end);
            s.replacement_at(&b.rope.to_string(), bytes)
        } else {
            s.replace.text.clone()
        };
        let old = b.rope.slice(r.clone()).to_string();
        let after = r.start + rep.chars().count();
        b.apply(vec![Change::replace(r.start, old, rep)], Selection::point(after), TxKind::Other);
        let Some(s) = b.search.as_mut() else { return };
        s.update(&b.rope, b.version);
        if let Some(i) = s.first_from(after) {
            let m = s.select(i);
            s.origin = m.start;
            b.sel = Selection::new(m.start, m.end);
        }
    }

    /// Replace every match in one transaction, so one undo reverts it.
    fn replace_all(&mut self, cx: &mut Ctx) {
        let Some(b) = self.active_buffer_mut() else { return };
        let Some(s) = b.search.as_mut() else { return };
        s.update(&b.rope, b.version);
        if s.matches.is_empty() {
            cx.notify(Level::Info, "No matches to replace");
            return;
        }
        let text = s.regex.then(|| b.rope.to_string());
        // Last to first so earlier positions stay valid.
        let changes: Vec<Change> = s
            .matches
            .iter()
            .rev()
            .map(|r| {
                let rep = match &text {
                    Some(t) => s.replacement_at(t, b.rope.char_to_byte(r.start)..b.rope.char_to_byte(r.end)),
                    None => s.replace.text.clone(),
                };
                Change::replace(r.start, b.rope.slice(r.clone()).to_string(), rep)
            })
            .collect();
        let n = changes.len();
        let caret = s.matches[0].start;
        b.apply(changes, Selection::point(caret), TxKind::Other);
        if let Some(s) = b.search.as_mut() {
            s.origin = caret;
            s.update(&b.rope, b.version);
        }
        cx.notify(Level::Info, format!("Replaced {n} occurrence{}", if n == 1 { "" } else { "s" }));
    }

    /// Keys while the find bar has focus. Returns false for keys the bar
    /// does not use, so global shortcuts (Ctrl+S, F3, Alt+1) still dispatch.
    pub fn handle_find_key(&mut self, key: KeyEvent, cx: &mut Ctx) -> bool {
        use EditorAction as E;
        let Some(ai) = self.active else { return false };
        let b = &mut self.buffers[ai];
        let Some(s) = b.search.as_mut().filter(|s| s.focused) else { return false };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let action = match key.code {
            KeyCode::Esc => Some(E::Escape),
            KeyCode::Enter if shift => Some(E::FindPrev),
            KeyCode::Enter if s.field == Field::Replace => Some(E::ReplaceOne),
            KeyCode::Enter => Some(E::FindNext),
            KeyCode::Up => Some(E::FindPrev),
            KeyCode::Down => Some(E::FindNext),
            KeyCode::Tab | KeyCode::BackTab => Some(E::FindToggleField),
            KeyCode::Char(c) if alt && !ctrl => match c.to_ascii_lowercase() {
                'c' => Some(E::FindToggleCase),
                'w' => Some(E::FindToggleWord),
                'x' => Some(E::FindToggleRegex),
                _ => return false,
            },
            _ => None,
        };
        if let Some(a) = action {
            self.handle_action(&a, cx);
            return true;
        }
        let input = s.active_input();
        match key.code {
            KeyCode::Backspace if ctrl || alt => input.delete_word_back(),
            KeyCode::Char('h') if ctrl => input.backspace(),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            KeyCode::Char(c) if !ctrl && !alt => input.insert(c),
            _ => return false,
        }
        if s.field == Field::Query {
            refresh_search(b);
            self.remember_search();
        }
        true
    }

    /// Paste into the focused find bar field (newlines dropped).
    pub fn paste_into_find(&mut self, text: &str) {
        let Some(b) = self.active_buffer_mut() else { return };
        let Some(s) = b.search.as_mut() else { return };
        s.active_input().insert_str(&text.replace(['\r', '\n'], ""));
        if s.field == Field::Query {
            refresh_search(b);
            self.remember_search();
        }
    }

    /// Mouse down inside the find bar.
    fn click_find_bar(&mut self, m: &Mouse, cx: &mut Ctx) {
        use EditorAction as E;
        let p = ratatui::layout::Position::new(m.x, m.y);
        let Some((hit, rect)) = self.find_hits.iter().find(|(_, r)| r.contains(p)).copied() else { return };
        let action = match hit {
            FindHit::QueryField | FindHit::ReplaceField => {
                if let Some(s) = self.active_buffer_mut().and_then(|b| b.search.as_mut()) {
                    s.focused = true;
                    s.field = if hit == FindHit::QueryField { Field::Query } else { Field::Replace };
                    s.active_input().click(rect, m.x, m.y);
                }
                return;
            }
            FindHit::Case => E::FindToggleCase,
            FindHit::Word => E::FindToggleWord,
            FindHit::Regex => E::FindToggleRegex,
            FindHit::Prev => E::FindPrev,
            FindHit::Next => E::FindNext,
            FindHit::Close => E::Escape,
            FindHit::ReplaceOne => E::ReplaceOne,
            FindHit::ReplaceAll => E::ReplaceAll,
        };
        self.handle_action(&action, cx);
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            if self.find_area.contains(ratatui::layout::Position::new(m.x, m.y)) {
                self.click_find_bar(m, cx);
                return true;
            }
            // A click in the text hands keyboard focus back to the text.
            if let Some(s) = self.active_buffer_mut().and_then(|b| b.search.as_mut()) {
                s.focused = false;
            }
        }
        let text_area = self.text_area;
        let tab_width = self.tab_width;
        let Some(ai) = self.active else { return false };
        let b = &mut self.buffers[ai];
        let pos_at = |b: &Buffer, x: u16, y: u16| -> usize {
            let row = y.saturating_sub(text_area.y) as usize;
            let line = (b.scroll_line + row).min(b.rope.len_lines().saturating_sub(1));
            let col = x.saturating_sub(text_area.x) as usize + b.scroll_col;
            if x < text_area.x { b.rope.line_to_char(line) } else { col_to_char(&b.rope, line, col, tab_width) }
        };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let p = pos_at(b, m.x, m.y);
                let extend = m.mods.contains(KeyModifiers::SHIFT);
                match m.clicks {
                    2 => {
                        let r = word_at(&b.rope, p);
                        b.sel = Selection::new(r.start, r.end);
                        self.drag = Some(DragMode::Word(r.start, r.end));
                    }
                    3 => {
                        let (line, _) = selection::pos_to_line_col(&b.rope, p);
                        let r = line_range(&b.rope, line);
                        b.sel = Selection::new(r.start, r.end);
                        self.drag = Some(DragMode::Line(r.start, r.end));
                    }
                    _ => {
                        b.sel.set_head(p, extend);
                        self.drag = Some(DragMode::Char);
                    }
                }
                b.desired_col = None;
                b.undo.break_group = true;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Only a drag that started in the text selects (not one
                // that started on a find bar button).
                if self.drag.is_none() {
                    return true;
                }
                let p = pos_at(b, m.x, m.y);
                match self.drag {
                    Some(DragMode::Word(s, e)) => {
                        let w = word_at(&b.rope, p);
                        if p < s { b.sel = Selection::new(e, w.start) } else { b.sel = Selection::new(s, w.end.max(e)) }
                    }
                    Some(DragMode::Line(s, e)) => {
                        let (line, _) = selection::pos_to_line_col(&b.rope, p);
                        let r = line_range(&b.rope, line);
                        if r.start < s { b.sel = Selection::new(e, r.start) } else { b.sel = Selection::new(s, r.end.max(e)) }
                    }
                    _ => b.sel.set_head(p, true),
                }
                // Auto-scroll while dragging past the edges.
                if m.y < text_area.y {
                    b.scroll_line = b.scroll_line.saturating_sub(1);
                } else if m.y >= text_area.bottom() {
                    b.scroll_line = (b.scroll_line + 1).min(b.rope.len_lines().saturating_sub(1));
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                true
            }
            MouseEventKind::ScrollUp => {
                b.scroll_line = b.scroll_line.saturating_sub(3);
                true
            }
            MouseEventKind::ScrollDown => {
                b.scroll_line = (b.scroll_line + 3).min(b.rope.len_lines().saturating_sub(1));
                true
            }
            MouseEventKind::ScrollLeft => {
                b.scroll_col = b.scroll_col.saturating_sub(4);
                true
            }
            MouseEventKind::ScrollRight => {
                b.scroll_col += 4;
                true
            }
            _ => false,
        }
    }

    pub fn handle_tab_mouse(&mut self, m: &Mouse, _area: Rect, _cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft => {
                self.tab_scroll = self.tab_scroll.saturating_sub(1);
                true
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollRight => {
                self.tab_scroll = (self.tab_scroll + 1).min(self.buffers.len().saturating_sub(1));
                true
            }
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, tabs: Rect, area: Rect, focused: bool, theme: &Theme) {
        self.last_area = area;
        self.render_tabs(frame, tabs, theme);
        let screen = frame.buffer_mut();
        screen.set_style(area, Style::default().bg(theme.bg));
        let Some(i) = self.active else {
            self.text_area = area;
            let hint = "No files open. Alt+1 opens the Project view; Ctrl+Shift+N finds a file.";
            if area.height > 2 && area.width > 10 {
                let x = area.x + area.width.saturating_sub(hint.width() as u16) / 2;
                screen.set_stringn(x, area.y + area.height / 2, hint, area.width as usize, theme.dim);
            }
            return;
        };
        // The find bar takes the top rows; the text view gets the rest so
        // `text_area` (and with it mouse mapping) stays exact.
        self.find_hits.clear();
        self.find_area = Rect::default();
        let b = &mut self.buffers[i];
        let mut text_rect = area;
        let mut bar_focused = false;
        if let Some(s) = b.search.as_mut() {
            // Matches follow buffer edits; recomputed only on change.
            s.update(&b.rope, b.version);
            let h = if s.show_replace { 2 } else { 1 }.min(area.height);
            self.find_area = Rect::new(area.x, area.y, area.width, h);
            text_rect = Rect::new(area.x, area.y + h, area.width, area.height - h);
            bar_focused = focused && s.focused;
            self.find_hits = render_find_bar(frame, self.find_area, s, bar_focused, theme);
        }
        let opts = RenderOpts { focused, tab_width: self.tab_width, scrolloff: self.scrolloff, hscrolloff: self.hscrolloff, theme };
        let out = view::render(&mut self.buffers[i], frame.buffer_mut(), text_rect, &opts);
        self.text_area = out.text_area;
        if focused && !bar_focused {
            if let Some((x, y)) = out.cursor {
                frame.set_cursor_position((x, y));
            }
        }
    }

    fn render_tabs(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let screen = frame.buffer_mut();
        screen.set_style(area, theme.tab);
        self.tab_hits.clear();
        if area.width == 0 || self.buffers.is_empty() {
            return;
        }
        // Keep the active tab visible by advancing the scroll offset.
        let widths: Vec<u16> = self.buffers.iter().map(|b| b.name.width() as u16 + 5).collect();
        if let Some(a) = self.active {
            if a < self.tab_scroll {
                self.tab_scroll = a;
            }
            loop {
                let total: u16 = widths[self.tab_scroll..=a].iter().sum();
                if total <= area.width || self.tab_scroll >= a {
                    break;
                }
                self.tab_scroll += 1;
            }
        }
        let mut x = area.x;
        if self.tab_scroll > 0 {
            screen.set_string(x, area.y, "‹", theme.dim);
            x += 1;
        }
        for (i, b) in self.buffers.iter().enumerate().skip(self.tab_scroll) {
            let w = widths[i];
            if x + w > area.right() {
                if x < area.right() {
                    screen.set_string(area.right() - 1, area.y, "›", theme.dim);
                }
                break;
            }
            let active = self.active == Some(i);
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
            screen.set_string(cx, area.y, "×", close_style);
            self.tab_hits.close.push((i, Rect::new(cx, area.y, 1, 1)));
            self.tab_hits.tabs.push((i, rect));
            x += w;
        }
    }
}

/// Recompute matches and select the first one at or after the bar's origin.
fn refresh_search(b: &mut Buffer) {
    let Some(s) = b.search.as_mut() else { return };
    s.update(&b.rope, b.version);
    match s.first_from(s.origin) {
        Some(i) => {
            let m = s.select(i);
            b.sel = Selection::new(m.start, m.end);
            b.desired_col = None;
        }
        None => s.current = None,
    }
}

/// Draw the find bar into `area` (1 row, 2 with replace) and return the
/// clickable regions:
/// `Find:    [query] Cc W .*  n/m  ↑ ↓ ×`
/// `Replace: [text]  [Replace] [Replace all]`
fn render_find_bar(frame: &mut Frame, area: Rect, s: &mut SearchState, focused: bool, theme: &Theme) -> Vec<(FindHit, Rect)> {
    let mut hits = Vec::new();
    let bar = Style::default().bg(theme.panel_bg).fg(theme.fg);
    let input_style = Style::default().bg(theme.bg).fg(theme.fg);
    frame.buffer_mut().set_style(area, bar);
    let right = area.right();
    // Writes `text` at `*x` clipped to the bar and advances `*x`.
    let put = |frame: &mut Frame, x: &mut u16, y: u16, text: &str, style: Style| -> Rect {
        let w = (text.width() as u16).min(right.saturating_sub(*x));
        if w > 0 {
            frame.buffer_mut().set_stringn(*x, y, text, w as usize, style);
        }
        let r = Rect::new(*x, y, w, 1);
        *x += w;
        r
    };
    let status = s.status();
    // Reserve room for typical statuses so the input does not jump.
    let status_w = (status.width() as u16).max(12);
    let label_w = 9u16;
    // Fixed parts after the input: toggles, status, arrows and close.
    let tail = 1 + 4 + 1 + 3 + 1 + 4 + 2 + status_w + 2 + 9;
    let input_w = area.width.saturating_sub(1 + label_w + tail).clamp(10, 40).min(area.width.saturating_sub(1 + label_w));

    let y = area.y;
    let mut x = area.x + 1;
    put(frame, &mut x, y, "Find:    ", bar);
    let field = Rect::new(x, y, input_w.min(right.saturating_sub(x)), 1);
    s.query.render(frame, field, input_style, "search", theme.dim.bg(theme.bg), focused && s.field == Field::Query);
    hits.push((FindHit::QueryField, field));
    x += field.width;
    let on = theme.strip_item_active;
    let off = theme.dim.bg(theme.panel_bg);
    for (hit, label, enabled) in [(FindHit::Case, " Cc ", s.case_sensitive), (FindHit::Word, " W ", s.whole_word), (FindHit::Regex, " .* ", s.regex)] {
        put(frame, &mut x, y, " ", bar);
        let r = put(frame, &mut x, y, label, if enabled { on } else { off });
        hits.push((hit, r));
    }
    put(frame, &mut x, y, "  ", bar);
    let status_style = if s.error.is_some() { theme.error.bg(theme.panel_bg) } else if s.matches.is_empty() && !s.query.text.is_empty() { theme.warn.bg(theme.panel_bg) } else { bar };
    put(frame, &mut x, y, &format!("{status:<w$}", w = status_w as usize), status_style);
    put(frame, &mut x, y, "  ", bar);
    for (hit, label) in [(FindHit::Prev, " ↑ "), (FindHit::Next, " ↓ "), (FindHit::Close, " × ")] {
        let r = put(frame, &mut x, y, label, theme.title.bg(theme.panel_bg));
        hits.push((hit, r));
    }

    if s.show_replace && area.height > 1 {
        let y = area.y + 1;
        let mut x = area.x + 1;
        put(frame, &mut x, y, "Replace: ", bar);
        let field = Rect::new(x, y, input_w.min(right.saturating_sub(x)), 1);
        s.replace.render(frame, field, input_style, "replace with", theme.dim.bg(theme.bg), focused && s.field == Field::Replace);
        hits.push((FindHit::ReplaceField, field));
        x += field.width;
        for (hit, label) in [(FindHit::ReplaceOne, " Replace "), (FindHit::ReplaceAll, " Replace all ")] {
            put(frame, &mut x, y, " ", bar);
            let r = put(frame, &mut x, y, label, theme.tab_active);
            hits.push((hit, r));
        }
    }
    hits.retain(|(_, r)| r.width > 0);
    hits
}

/// Write via a temp file in the same directory, then rename over the target.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.jetl-tmp", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Preserve permissions of the original file when it exists.
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use crossterm::event::KeyEvent;

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
            assert_eq!(ed.active_buffer().unwrap().sel.range(), 0..3);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::NONE), cx));
            assert_eq!(ed.active_buffer().unwrap().sel.range(), 8..11);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::SHIFT), cx));
            assert_eq!(ed.active_buffer().unwrap().sel.range(), 0..3);
            // Shift+Enter from the first match wraps to the last.
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::SHIFT), cx));
            assert_eq!(ed.active_buffer().unwrap().sel.range(), 16..19);
            // Ctrl+S is not consumed by the bar.
            assert!(!ed.handle_find_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL), cx));
            // Alt+C toggles case sensitivity.
            assert!(ed.handle_find_key(key(KeyCode::Char('c'), KeyModifiers::ALT), cx));
            assert!(ed.active_buffer().unwrap().search.as_ref().unwrap().case_sensitive);
            // Esc closes the bar and leaves the match selected.
            assert!(ed.handle_find_key(key(KeyCode::Esc, KeyModifiers::NONE), cx));
            assert!(ed.active_buffer().unwrap().search.is_none());
            assert!(!ed.find_focused());
            // F3 with the bar closed reuses the last query.
            ed.handle_action(&EditorAction::FindNext, cx);
            assert_eq!(ed.active_buffer().unwrap().sel.range(), 0..3);
            assert!(ed.active_buffer().unwrap().search.is_none());
        });
        assert_eq!(notes.iter().filter(|n| *n == "Search wrapped").count(), 2);
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut ed = editor_with("a.txt", "foo foo foo");
        let notes = with_cx(|cx| {
            ed.handle_action(&EditorAction::Replace, cx);
            let b = ed.active_buffer_mut().unwrap();
            let s = b.search.as_mut().unwrap();
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
            let s = b.search.as_mut().unwrap();
            s.regex = true;
            s.query.set_text(r"\ba(\d)");
            s.replace.set_text("<$1>");
            s.field = Field::Replace;
            refresh_search(b);
            assert_eq!(b.sel.range(), 0..2);
            assert!(ed.handle_find_key(key(KeyCode::Enter, KeyModifiers::NONE), cx));
            let b = ed.active_buffer().unwrap();
            assert_eq!(b.text(), "<1> a2 b3");
            assert_eq!(b.sel.range(), 4..6);
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
}
