//! The find/replace bar: keys, stepping, replace, mouse, rendering.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::Editor;
use super::buffer::Buffer;
use super::search::{Field, SearchState};
use super::selection::Selection;
use super::undo::{Change, TxKind};
use crate::actions::EditorAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Level, Mouse};

/// Clickable parts of the find bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FindHit {
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
pub(super) struct LastSearch {
    query: String,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
}

impl LastSearch {
    pub(super) fn to_state(&self) -> SearchState {
        let mut s = SearchState::new(&self.query, false);
        s.case_sensitive = self.case_sensitive;
        s.whole_word = self.whole_word;
        s.regex = self.regex;
        s
    }
}

impl Editor {
    /// The active buffer's find bar has keyboard focus.
    pub fn find_focused(&self) -> bool {
        self.active_buffer().and_then(|b| b.search.as_ref()).is_some_and(|s| s.focused)
    }

    /// Open the find bar (or refocus it). A single-line selection becomes
    /// the query; otherwise the last query is reused.
    pub(super) fn open_find(&mut self, replace: bool) {
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

    pub(super) fn close_find(&mut self) {
        self.remember_search();
        if let Some(b) = self.active_buffer_mut() {
            b.search = None;
        }
    }

    pub(super) fn remember_search(&mut self) {
        let Some(s) = self.active_buffer().and_then(|b| b.search.as_ref()) else { return };
        if !s.query.text.is_empty() {
            self.last_search = Some(LastSearch {
                query: s.query.text.clone(),
                case_sensitive: s.case_sensitive,
                whole_word: s.whole_word,
                regex: s.regex,
            });
        }
    }

    /// Select the next/previous match from the selection. With the bar
    /// closed this searches for the last query without opening it.
    pub(super) fn find_step(&mut self, forward: bool, cx: &mut Ctx) {
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
    pub(super) fn replace_one(&mut self, cx: &mut Ctx) {
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
    pub(super) fn replace_all(&mut self, cx: &mut Ctx) {
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
    pub(super) fn click_find_bar(&mut self, m: &Mouse, cx: &mut Ctx) {
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
}

/// Recompute matches and select the first one at or after the bar's origin.
pub(super) fn refresh_search(b: &mut Buffer) {
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
pub(super) fn render_find_bar(frame: &mut Frame, area: Rect, s: &mut SearchState, focused: bool, theme: &Theme) -> Vec<(FindHit, Rect)> {
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
    for (hit, label, enabled) in
        [(FindHit::Case, " Cc ", s.case_sensitive), (FindHit::Word, " W ", s.whole_word), (FindHit::Regex, " .* ", s.regex)]
    {
        put(frame, &mut x, y, " ", bar);
        let r = put(frame, &mut x, y, label, if enabled { on } else { off });
        hits.push((hit, r));
    }
    put(frame, &mut x, y, "  ", bar);
    let status_style = if s.error.is_some() {
        theme.error.bg(theme.panel_bg)
    } else if s.matches.is_empty() && !s.query.text.is_empty() {
        theme.warn.bg(theme.panel_bg)
    } else {
        bar
    };
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
