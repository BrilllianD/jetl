//! Find tool window (bottom, Alt+3): project-wide search with a query and
//! a replacement field, case/word/regex toggles, and results grouped by
//! file. Typing searches live; Up/Down move through the results, Enter
//! opens one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use regex::Regex;
use unicode_width::UnicodeWidthStr;

use crate::actions::FindAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::editor::search::build_pattern;
use crate::event::{AppMsg, Effect, Level, Mouse};
use crate::files::find::{Batch, Hit, MAX_HITS, Search};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::PanelId;
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Query,
    Replace,
}

/// A results row: a file header or one hit (index into `hits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    File { first: usize, count: usize },
    Hit(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Query,
    Replace,
    Case,
    Word,
    Regex,
    ReplaceAll,
}

pub struct FindPanel {
    pub query: TextInput,
    pub replace: TextInput,
    pub show_replace: bool,
    field: Field,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    hits: Vec<Hit>,
    rows: Vec<Row>,
    pub list: ListState,
    /// Seq of the newest search; a running search stops when it moves on.
    seq: u64,
    cancel: Arc<AtomicU64>,
    searching: bool,
    capped: bool,
    files_searched: usize,
    error: Option<String>,
    root: PathBuf,
    btns: Vec<(Btn, Rect)>,
    list_rect: Rect,
}

impl Default for FindPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl FindPanel {
    pub fn new() -> Self {
        Self {
            query: TextInput::new(false),
            replace: TextInput::new(false),
            show_replace: false,
            field: Field::Query,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            hits: Vec::new(),
            rows: Vec::new(),
            list: ListState::default(),
            seq: 0,
            cancel: Arc::new(AtomicU64::new(0)),
            searching: false,
            capped: false,
            files_searched: 0,
            error: None,
            root: PathBuf::new(),
            btns: Vec::new(),
            list_rect: Rect::default(),
        }
    }

    /// Open with `query` (the editor selection, if any) and the replace
    /// field shown or not; searches when the query changed.
    pub fn open(&mut self, query: Option<&str>, replace: bool, cx: &mut Ctx) {
        self.show_replace |= replace;
        self.field = Field::Query;
        if let Some(q) = query.filter(|q| !q.is_empty() && *q != self.query.text) {
            self.query.set_text(q);
            self.search(cx);
        } else if self.hits.is_empty() && !self.query.text.is_empty() {
            self.search(cx);
        }
    }

    /// Drop results and stop the running search (root switch).
    pub fn reset(&mut self) {
        self.seq += 1;
        self.cancel.store(self.seq, Ordering::Relaxed);
        self.hits.clear();
        self.rows.clear();
        self.list = ListState::default();
        self.searching = false;
        self.error = None;
    }

    pub fn pattern(&self) -> Result<Regex, String> {
        build_pattern(&self.query.text, self.regex, self.case_sensitive, self.whole_word)
    }

    /// Start a new search with the current query and options, cancelling
    /// the previous one.
    pub fn search(&mut self, cx: &mut Ctx) {
        self.reset();
        self.root = cx.workspace.root.clone();
        if self.query.text.is_empty() {
            return;
        }
        let re = match self.pattern() {
            Ok(re) => re,
            Err(e) => {
                self.error = Some(e.lines().last().unwrap_or("invalid pattern").trim().to_string());
                return;
            }
        };
        self.searching = true;
        self.capped = false;
        self.files_searched = 0;
        let search = Search {
            root: self.root.clone(),
            show_hidden: cx.config.files.show_hidden,
            show_ignored: cx.config.files.show_ignored,
            re,
            seq: self.seq,
            cancel: self.cancel.clone(),
        };
        let (generation, tx) = (cx.workspace.generation, cx.bus.clone());
        tokio::task::spawn_blocking(move || search.run(|batch| drop(tx.send(AppMsg::Find { generation, batch }))));
    }

    /// Hits of the running search; older searches' batches are ignored.
    pub fn on_batch(&mut self, batch: Batch) {
        if batch.seq != self.seq {
            return;
        }
        self.files_searched = batch.files_searched;
        self.searching = !batch.done;
        self.capped |= batch.capped;
        for hit in batch.hits {
            let i = self.hits.len();
            let same_file = self.hits.last().is_some_and(|h| h.path == hit.path);
            self.hits.push(hit);
            if same_file {
                if let Some(Row::File { count, .. }) = self.rows.iter_mut().rev().find(|r| matches!(r, Row::File { .. })) {
                    *count += 1;
                }
            } else {
                self.rows.push(Row::File { first: i, count: 1 });
            }
            self.rows.push(Row::Hit(i));
        }
        let first = self.list.len == 0;
        self.list.set_len(self.rows.len());
        // Start on the first hit, not its file header.
        if first && self.rows.len() > 1 {
            self.list.select(1);
        }
    }

    #[cfg(test)]
    pub fn current_seq(&self) -> u64 {
        self.seq
    }

    #[cfg(test)]
    pub fn cancel_handle(&self) -> Arc<AtomicU64> {
        self.cancel.clone()
    }

    pub fn hit_count(&self) -> usize {
        self.hits.len()
    }

    /// Distinct files with hits, in result order.
    pub fn hit_files(&self) -> Vec<PathBuf> {
        self.rows.iter().filter_map(|r| if let Row::File { first, .. } = r { Some(self.hits[*first].path.clone()) } else { None }).collect()
    }

    fn selected_hit(&self) -> Option<&Hit> {
        match self.rows.get(self.list.selected)? {
            Row::Hit(i) => self.hits.get(*i),
            Row::File { first, .. } => self.hits.get(*first),
        }
    }

    fn open_selected(&self, cx: &mut Ctx) {
        if let Some(h) = self.selected_hit() {
            cx.effects.push(Effect::OpenFileAt { path: h.path.clone(), line: h.line, col: h.col });
        }
    }

    /// Select the next or previous hit (wrapping) and open it; used from
    /// anywhere through `find.next_result` / `find.prev_result`.
    pub fn step(&mut self, forward: bool, cx: &mut Ctx) {
        let n = self.rows.len();
        if self.hits.is_empty() {
            cx.notify(Level::Info, "No find results; Ctrl+Shift+F or Alt+3 searches the project");
            return;
        }
        let mut i = self.list.selected;
        for _ in 0..n {
            i = if forward { (i + 1) % n } else { (i + n - 1) % n };
            if matches!(self.rows[i], Row::Hit(_)) {
                break;
            }
        }
        self.list.select(i);
        self.open_selected(cx);
    }

    fn input(&mut self) -> &mut TextInput {
        match self.field {
            Field::Query => &mut self.query,
            Field::Replace => &mut self.replace,
        }
    }

    /// Editing keys for the focused field; false for keys the keymap
    /// handles (Up/Down, Enter, toggles, Escape, global shortcuts).
    pub fn handle_key(&mut self, key: KeyEvent, cx: &mut Ctx) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let before = self.query.text.clone();
        let input = self.input();
        match key.code {
            // Ctrl+Shift+Left/Right resize the window through the keymap.
            KeyCode::Left | KeyCode::Right if ctrl && shift => return false,
            KeyCode::Backspace if ctrl || alt => input.delete_word_back(),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            KeyCode::Char(c) if !ctrl && !alt => input.insert(c),
            _ => return false,
        }
        if self.query.text != before {
            self.search(cx);
        }
        true
    }

    pub fn paste(&mut self, text: &str, cx: &mut Ctx) {
        let before = self.query.text.clone();
        self.input().insert_str(&text.replace(['\r', '\n'], " "));
        if self.query.text != before {
            self.search(cx);
        }
    }

    pub fn handle_action(&mut self, a: &FindAction, cx: &mut Ctx) -> bool {
        let page = self.list.viewport.max(1);
        match a {
            FindAction::Up => self.list.up(1),
            FindAction::Down => self.list.down(1),
            FindAction::PageUp => self.list.up(page),
            FindAction::PageDown => self.list.down(page),
            FindAction::Open => self.open_selected(cx),
            FindAction::NextField => {
                self.field = if self.field == Field::Query && self.show_replace { Field::Replace } else { Field::Query };
            }
            FindAction::ToggleCase => self.toggle(|p| &mut p.case_sensitive, cx),
            FindAction::ToggleWord => self.toggle(|p| &mut p.whole_word, cx),
            FindAction::ToggleRegex => self.toggle(|p| &mut p.regex, cx),
            FindAction::ToggleReplace => {
                self.show_replace = !self.show_replace;
                self.field = if self.show_replace { Field::Replace } else { Field::Query };
            }
            FindAction::ReplaceAll => self.ask_replace_all(cx),
            FindAction::Escape => cx.focus(PanelId::Editor),
            // Handled by the app (they need the editor and the layout).
            FindAction::InFiles | FindAction::ReplaceInFiles | FindAction::NextResult | FindAction::PrevResult => return false,
        }
        true
    }

    fn toggle(&mut self, flag: impl FnOnce(&mut Self) -> &mut bool, cx: &mut Ctx) {
        let f = flag(self);
        *f = !*f;
        self.search(cx);
    }

    fn ask_replace_all(&mut self, cx: &mut Ctx) {
        if self.searching {
            cx.notify(Level::Info, "Still searching; replace when the search finishes");
            return;
        }
        if self.hits.is_empty() {
            cx.notify(Level::Info, "Nothing to replace");
            return;
        }
        if !self.show_replace {
            self.show_replace = true;
            self.field = Field::Replace;
            return;
        }
        let files = self.hit_files().len();
        let msg = format!(
            "Replace {} match{} in {files} file{} with \"{}\"?\nOpen files change in the editor (undoable); others are written to disk.",
            self.hits.len(),
            if self.hits.len() == 1 { "" } else { "es" },
            if files == 1 { "" } else { "s" },
            self.replace.text
        );
        cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::ReplaceInFiles, "Replace in Files", &msg, &["Replace", "Cancel"])));
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let p = ratatui::layout::Position::new(m.x, m.y);
                if let Some((b, r)) = self.btns.iter().find(|(_, r)| r.contains(p)).copied() {
                    match b {
                        Btn::Query => {
                            self.field = Field::Query;
                            self.query.click(r, m.x, m.y);
                        }
                        Btn::Replace => {
                            self.field = Field::Replace;
                            self.replace.click(r, m.x, m.y);
                        }
                        Btn::Case => self.toggle(|p| &mut p.case_sensitive, cx),
                        Btn::Word => self.toggle(|p| &mut p.whole_word, cx),
                        Btn::Regex => self.toggle(|p| &mut p.regex, cx),
                        Btn::ReplaceAll => self.ask_replace_all(cx),
                    }
                    return true;
                }
                if let Some(i) = self.list.hit(self.list_rect, m.x, m.y) {
                    self.list.select(i);
                    if m.clicks >= 2 {
                        self.open_selected(cx);
                    }
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.list.scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.list.scroll(3);
                true
            }
            _ => false,
        }
    }

    fn status(&self) -> (String, Option<Level>) {
        if let Some(e) = &self.error {
            return (e.clone(), Some(Level::Error));
        }
        if self.query.text.is_empty() {
            return ("type to search the project".into(), None);
        }
        let (n, files) = (self.hits.len(), self.hit_files().len());
        let found = format!(
            "{n}{} match{} in {files} file{}",
            if self.capped { "+" } else { "" },
            if n == 1 { "" } else { "es" },
            if files == 1 { "" } else { "s" }
        );
        if self.searching {
            (format!("Searching… {found} ({} files)", self.files_searched), None)
        } else if n == 0 {
            (format!("No matches ({} files searched)", self.files_searched), Some(Level::Warn))
        } else if self.capped {
            (format!("{found}; stopped at {MAX_HITS}"), Some(Level::Warn))
        } else {
            (found, None)
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let block = tool_window_block(" Find ", focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.btns.clear();
        self.list_rect = Rect::default();
        if inner.height == 0 || inner.width < 20 {
            return;
        }
        let bar = Style::default().bg(theme.panel_bg).fg(theme.fg);
        let input_style = Style::default().bg(theme.bg).fg(theme.fg);
        let right = inner.right();
        let put = |frame: &mut Frame, x: &mut u16, y: u16, text: &str, style: Style| -> Rect {
            let w = (text.width() as u16).min(right.saturating_sub(*x));
            if w > 0 {
                frame.buffer_mut().set_stringn(*x, y, text, w as usize, style);
            }
            let r = Rect::new(*x, y, w, 1);
            *x += w;
            r
        };
        let header_h = if self.show_replace { 2 } else { 1 }.min(inner.height);
        frame.buffer_mut().set_style(Rect::new(inner.x, inner.y, inner.width, header_h), bar);
        let input_w = inner.width.saturating_sub(10 + 16).clamp(10, 60);

        let (y, mut x) = (inner.y, inner.x + 1);
        put(frame, &mut x, y, "Find:    ", bar);
        let field = Rect::new(x, y, input_w.min(right.saturating_sub(x)), 1);
        self.query.render(frame, field, input_style, "text or regex", theme.dim.bg(theme.bg), focused && self.field == Field::Query);
        self.btns.push((Btn::Query, field));
        x += field.width;
        let (on, off) = (theme.strip_item_active, theme.dim.bg(theme.panel_bg));
        for (b, label, enabled) in
            [(Btn::Case, " Cc ", self.case_sensitive), (Btn::Word, " W ", self.whole_word), (Btn::Regex, " .* ", self.regex)]
        {
            put(frame, &mut x, y, " ", bar);
            let r = put(frame, &mut x, y, label, if enabled { on } else { off });
            self.btns.push((b, r));
        }
        put(frame, &mut x, y, "  ", bar);
        let (status, level) = self.status();
        let st = match level {
            Some(Level::Error) => theme.error.bg(theme.panel_bg),
            Some(Level::Warn) => theme.warn.bg(theme.panel_bg),
            _ => theme.dim.bg(theme.panel_bg),
        };
        put(frame, &mut x, y, &status, st);

        if self.show_replace && header_h > 1 {
            let (y, mut x) = (inner.y + 1, inner.x + 1);
            put(frame, &mut x, y, "Replace: ", bar);
            let field = Rect::new(x, y, input_w.min(right.saturating_sub(x)), 1);
            self.replace.render(frame, field, input_style, "replace with", theme.dim.bg(theme.bg), focused && self.field == Field::Replace);
            self.btns.push((Btn::Replace, field));
            x += field.width;
            put(frame, &mut x, y, " ", bar);
            let r = put(frame, &mut x, y, " Replace all ", theme.tab_active);
            self.btns.push((Btn::ReplaceAll, r));
        }
        self.btns.retain(|(_, r)| r.width > 0);

        let lr = Rect::new(inner.x, inner.y + header_h, inner.width, inner.height - header_h);
        self.list_rect = lr;
        self.list.set_viewport(lr.height as usize);
        let buf = frame.buffer_mut();
        for (vi, i) in self.list.visible().enumerate() {
            let y = lr.y + vi as u16;
            let selected = i == self.list.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(lr.x, y, lr.width, 1), base);
            }
            match self.rows[i] {
                Row::File { first, count } => {
                    let path = &self.hits[first].path;
                    let rel = rel_path(&self.root, path);
                    let (dir, name) = match rel.rfind('/') {
                        Some(k) => (&rel[..=k], &rel[k + 1..]),
                        None => ("", rel.as_str()),
                    };
                    let mut x = lr.x + 1;
                    for (text, style) in [
                        (name, base.add_modifier(ratatui::style::Modifier::BOLD)),
                        ("  ", base),
                        (dir, base.patch(theme.dim)),
                        ("  ", base),
                    ] {
                        let w = (text.width() as u16).min(lr.right().saturating_sub(x));
                        buf.set_stringn(x, y, text, w as usize, style);
                        x += w;
                    }
                    let n = format!("{count}");
                    buf.set_stringn(x, y, &n, lr.right().saturating_sub(x) as usize, base.patch(theme.dim));
                }
                Row::Hit(h) => {
                    let hit = &self.hits[h];
                    let num = format!("{:>6}  ", hit.line + 1);
                    let mut x = lr.x + 1;
                    buf.set_stringn(x, y, &num, lr.right().saturating_sub(x) as usize, base.patch(theme.line_number));
                    x += num.width() as u16;
                    // Text before, in and after the match.
                    let pre: String = hit.preview.chars().take(hit.preview_col).collect();
                    let mid: String = hit.preview.chars().skip(hit.preview_col).take(hit.len).collect();
                    let post: String = hit.preview.chars().skip(hit.preview_col + hit.len).collect();
                    let hl = base.bg(theme.search_match_bg);
                    for (text, style) in [(pre.trim_start(), base), (mid.as_str(), hl), (post.as_str(), base)] {
                        if x >= lr.right() {
                            break;
                        }
                        (x, _) = buf.set_stringn(x, y, text, lr.right().saturating_sub(x) as usize, style);
                    }
                }
            }
        }
    }
}

/// `path` relative to `root` with `/` separators, or the full path.
fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}
