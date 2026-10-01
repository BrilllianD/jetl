//! Editor: a tab strip of buffers plus the text view.

pub mod buffer;
pub mod gutter;
pub mod search;
pub mod selection;
pub mod syntax;
pub mod undo;
pub mod view;

use std::path::{Path, PathBuf};

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
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
use selection::{Selection, apply_motion, line_range, word_at};
use undo::TxKind;
use view::{RenderOpts, col_to_char};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Char,
    Word(usize, usize),
    Line(usize, usize),
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

    /// Add a loaded file as a new tab and make it active.
    pub fn open_text(&mut self, path: PathBuf, text: &str, lossy: bool) -> usize {
        if let Some(i) = self.index_of(&path) {
            self.activate_index(i);
            return i;
        }
        let id = self.next_id;
        self.next_id += 1;
        let buf = Buffer::from_text(id, Some(path), text, lossy);
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
                    b.search = None;
                } else {
                    b.sel.collapse();
                }
            }
            E::ScrollUp(n) => b.scroll_line = b.scroll_line.saturating_sub(*n),
            E::ScrollDown(n) => b.scroll_line = (b.scroll_line + n).min(b.rope.len_lines().saturating_sub(1)),
            E::Find | E::Replace | E::FindNext | E::FindPrev | E::ToggleComment => {
                cx.notify(Level::Info, "Find/replace arrives in phase 2");
            }
            E::NextTab | E::PrevTab | E::CloseTab | E::CloseAllTabs | E::Switcher | E::Save | E::SaveAll | E::GotoLine => {}
        }
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, _cx: &mut Ctx) -> bool {
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
        let opts = RenderOpts { focused, tab_width: self.tab_width, scrolloff: self.scrolloff, hscrolloff: self.hscrolloff, theme };
        let out = view::render(&mut self.buffers[i], screen, area, &opts);
        self.text_area = out.text_area;
        if focused {
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
