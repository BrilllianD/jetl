//! The Settings window: a modal page tree over `config.toml`'s keys
//! (`config::catalog`). Edits are staged in `pending` until Apply or OK.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::catalog::{Kind, PAGES, Setting, setting};
use crate::config::edit::{Edit, Value};
use crate::config::reload::class_of;
use crate::config::theme::{self, Theme};
use crate::config::{Config, FileText};
use crate::event::Mouse;
use crate::ui::toasts::wrap;
use crate::ui::widgets::list::ListState;
use crate::util::text::fit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Tree,
    Page,
    Buttons,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsResult {
    Close,
    OpenConfigFile,
    /// Ask for a value of this key (Choice, Int, Text, List rows).
    EditValue(&'static str),
    /// Write the pending edits; `close` after a successful write (OK).
    Apply {
        close: bool,
    },
    /// Esc / Cancel with pending edits: ask before closing.
    ConfirmDiscard,
}

const BUTTONS: [&str; 3] = ["[ OK ]", "[ Cancel ]", "[ Apply ]"];
const OPEN_FILE_ROW: &str = "Open config.toml";
const MAX_HELP_ROWS: usize = 5;
/// Cells before a label: the pending marker and a space.
const MARK_W: usize = 2;

pub struct SettingsWindow {
    config: Result<Config, String>,
    defaults: Config,
    /// Staged edits, one per key, in the order they were made.
    pending: Vec<(&'static str, Edit)>,
    /// Rows of the tree: every page of `PAGES`, so the selection is the page.
    tree: ListState,
    rows: ListState,
    pane: Pane,
    /// 0 OK, 1 Cancel, 2 Apply.
    button: usize,
    rect: Rect,
    tree_rect: Rect,
    rows_rect: Rect,
    value_col: u16,
    button_rects: Vec<Rect>,
}

fn load(file: &FileText, path: &Path) -> Result<Config, String> {
    let name = path.file_name().map_or_else(|| "config.toml".into(), |n| n.to_string_lossy());
    match file {
        FileText::Missing => Ok(Config::default()),
        FileText::Unreadable(e) => Err(format!("Cannot read {name}: {e}")),
        FileText::Text(t) => Config::parse(t).map_err(|e| format!("{name} is invalid: {}", theme::error_summary(&e.to_string()))),
    }
}

impl SettingsWindow {
    pub fn new(file: &FileText, path: &Path) -> Self {
        Self {
            config: load(file, path),
            defaults: Config::default(),
            pending: Vec::new(),
            tree: ListState::default(),
            rows: ListState::default(),
            pane: Pane::Tree,
            button: 0,
            rect: Rect::default(),
            tree_rect: Rect::default(),
            rows_rect: Rect::default(),
            value_col: 0,
            button_rects: Vec::new(),
        }
    }

    /// The file changed: show its values, keeping the page and selections.
    pub fn file_changed(&mut self, file: &FileText, path: &Path) {
        self.config = load(file, path);
        self.rows.set_len(self.row_count());
    }

    pub fn pending(&self) -> &[(&'static str, Edit)] {
        &self.pending
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// The value in the file (the defaults while it is missing or invalid).
    fn file_value(&self, st: &Setting) -> Value {
        (st.get)(self.config.as_ref().unwrap_or(&self.defaults))
    }

    /// Stage `edit` for `key`, replacing an earlier one; an edit that
    /// changes nothing is dropped, so setting a value back clears its marker.
    pub fn set_pending(&mut self, key: &'static str, edit: Edit) {
        self.pending.retain(|(k, _)| *k != key);
        let Some(st) = setting(key) else { return };
        let file = self.file_value(st);
        let noop = match &edit {
            Edit::Set(v) => *v == file,
            Edit::Reset => (st.get)(&self.defaults) == file,
        };
        if !noop {
            self.pending.push((key, edit));
        }
    }

    fn pending_edit(&self, key: &str) -> Option<&Edit> {
        self.pending.iter().find(|(k, _)| *k == key).map(|(_, e)| e)
    }

    /// A key's value as the window shows it: the staged one, else the file's.
    pub fn value(&self, key: &str) -> Value {
        let Some(st) = setting(key) else { return Value::Str(String::new()) };
        match self.pending_edit(key) {
            Some(Edit::Set(v)) => v.clone(),
            Some(Edit::Reset) => (st.get)(&self.defaults),
            None => self.file_value(st),
        }
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains((x, y).into())
    }

    fn page(&self) -> usize {
        self.tree.selected
    }

    fn row_count(&self) -> usize {
        match self.config {
            Ok(_) => PAGES[self.page()].keys.len(),
            Err(_) => 1,
        }
    }

    /// The selected row's key on the page; `None` on a parent page or in
    /// the error state.
    pub fn selected_key(&self) -> Option<&'static str> {
        self.config.as_ref().ok()?;
        PAGES[self.page()].keys.get(self.rows.selected).copied()
    }

    fn select_page(&mut self, page: usize) {
        if page != self.page() {
            self.tree.select(page);
            self.rows = ListState::default();
        }
        self.rows.set_len(self.row_count());
    }

    fn selected_row_opens_file(&self) -> bool {
        match &self.config {
            Err(_) => self.row_count() > 0,
            Ok(_) => self.selected_key().and_then(setting).is_some_and(|s| s.kind == Kind::External),
        }
    }

    fn set_bool(&mut self, key: &'static str, on: bool) {
        self.set_pending(key, Edit::Set(Value::Bool(on)));
    }

    /// Enter, Space or a double click on the selected row.
    fn activate(&mut self) -> Option<SettingsResult> {
        if self.selected_row_opens_file() {
            return Some(SettingsResult::OpenConfigFile);
        }
        let key = self.selected_key()?;
        match setting(key)?.kind {
            Kind::Bool => {
                let on = self.value(key) == Value::Bool(true);
                self.set_bool(key, !on);
                None
            }
            Kind::External => Some(SettingsResult::OpenConfigFile),
            _ => Some(SettingsResult::EditValue(key)),
        }
    }

    /// Left / Right on the selected row, when it takes them for itself.
    fn step_value(&mut self, forward: bool) -> bool {
        let Some(key) = self.selected_key() else { return false };
        let Some(st) = setting(key) else { return false };
        match st.kind {
            Kind::Bool => self.set_bool(key, forward),
            Kind::Choice { options, custom } => {
                let Value::Str(cur) = self.value(key) else { return false };
                let at = options.iter().position(|o| *o == cur);
                let n = options.len();
                let next = match at {
                    Some(at) if forward => (at + 1) % n,
                    Some(at) => (at + n - 1) % n,
                    None if custom => return false,
                    None => 0,
                };
                self.set_pending(key, Edit::Set(Value::Str(options[next].to_string())));
            }
            _ => return false,
        }
        true
    }

    fn escape(&self) -> SettingsResult {
        if self.pending.is_empty() { SettingsResult::Close } else { SettingsResult::ConfirmDiscard }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<SettingsResult> {
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        self.tree.len = PAGES.len();
        self.rows.set_len(self.row_count());
        match key.code {
            KeyCode::Esc => return Some(self.escape()),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT) => {
                return (!self.pending.is_empty()).then_some(SettingsResult::Apply { close: false });
            }
            KeyCode::Tab => {
                self.pane = match self.pane {
                    Pane::Tree => Pane::Page,
                    Pane::Page => Pane::Buttons,
                    Pane::Buttons => Pane::Tree,
                };
                return None;
            }
            KeyCode::BackTab => {
                self.pane = match self.pane {
                    Pane::Tree => Pane::Buttons,
                    Pane::Page => Pane::Tree,
                    Pane::Buttons => Pane::Page,
                };
                return None;
            }
            _ => {}
        }
        match self.pane {
            Pane::Tree => {
                let before = self.page();
                if move_list(&mut self.tree, key.code) {
                    if self.page() != before {
                        self.rows = ListState::default();
                    }
                    self.rows.set_len(self.row_count());
                } else if matches!(key.code, KeyCode::Right | KeyCode::Enter) {
                    if PAGES[before].keys.is_empty()
                        && let Some(child) = PAGES.iter().position(|p| p.parent == Some(before))
                    {
                        self.select_page(child);
                    }
                    self.pane = Pane::Page;
                }
                None
            }
            Pane::Page => {
                if move_list(&mut self.rows, key.code) {
                    return None;
                }
                let editable = self.config.is_ok();
                match key.code {
                    KeyCode::Left | KeyCode::Right if editable && self.step_value(key.code == KeyCode::Right) => {}
                    KeyCode::Left => self.pane = Pane::Tree,
                    KeyCode::Enter | KeyCode::Char(' ') => return self.activate(),
                    KeyCode::Delete if editable => {
                        if let Some(k) = self.selected_key().filter(|k| setting(k).is_some_and(|s| s.kind != Kind::External)) {
                            self.set_pending(k, Edit::Reset);
                        }
                    }
                    _ => {}
                }
                None
            }
            Pane::Buttons => match key.code {
                KeyCode::Left => {
                    self.button = self.button.saturating_sub(1);
                    None
                }
                KeyCode::Right => {
                    self.button = (self.button + 1).min(BUTTONS.len() - 1);
                    None
                }
                KeyCode::Enter => self.press(self.button),
                _ => None,
            },
        }
    }

    fn press(&mut self, button: usize) -> Option<SettingsResult> {
        match button {
            0 if self.pending.is_empty() => Some(SettingsResult::Close),
            0 => Some(SettingsResult::Apply { close: true }),
            1 => Some(self.escape()),
            _ => (!self.pending.is_empty()).then_some(SettingsResult::Apply { close: false }),
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<SettingsResult> {
        let p = (m.x, m.y).into();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.button_rects.iter().position(|r| r.contains(p)) {
                    self.pane = Pane::Buttons;
                    self.button = i;
                    return self.press(i);
                }
                if let Some(i) = self.tree.hit(self.tree_rect, m.x, m.y) {
                    self.pane = Pane::Tree;
                    self.select_page(i);
                } else if let Some(i) = self.rows.hit(self.rows_rect, m.x, m.y) {
                    self.pane = Pane::Page;
                    self.rows.select(i);
                    let bool_cell = m.x >= self.value_col && self.selected_key().and_then(setting).is_some_and(|s| s.kind == Kind::Bool);
                    if bool_cell {
                        // The first click toggles; the second of a double click must not undo it.
                        return if m.clicks <= 1 { self.activate() } else { None };
                    }
                    if m.clicks >= 2 {
                        return self.activate();
                    }
                }
                None
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                if self.tree_rect.contains(p) {
                    self.tree.scroll(delta);
                } else if self.rows_rect.contains(p) {
                    self.rows.scroll(delta);
                }
                None
            }
            _ => None,
        }
    }

    /// The value shown for a row, and whether it is a placeholder.
    fn value_text(&self, st: &Setting) -> (String, bool) {
        if self.config.is_err() {
            return (String::new(), false);
        }
        match (self.value(st.key), st.kind) {
            (_, Kind::External) => ("Edit in config.toml…".into(), true),
            (Value::Bool(b), _) => (if b { "[x]" } else { "[ ]" }.into(), false),
            (Value::Int(n), _) => (n.to_string(), false),
            (Value::Str(s), Kind::Choice { .. }) => (format!("{s} ▾"), false),
            (Value::Str(s), _) if s.is_empty() => ("(empty)".into(), true),
            (Value::Str(s), _) => (s, false),
            (Value::List(l), _) if l.is_empty() => ("(none)".into(), true),
            (Value::List(l), _) => (l.join(", "), false),
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let w = area.width.saturating_sub(4).min(110);
        let h = area.height.saturating_sub(2).min(32);
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 3;
        self.rect = Rect::new(x, y, w, h);
        frame.render_widget(Clear, self.rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.popup_border)
            .title(" Settings ")
            .title_style(theme.title_focused)
            .style(Style::default().bg(theme.popup_bg).fg(theme.fg));
        let inner = block.inner(self.rect);
        frame.render_widget(block, self.rect);
        if inner.height < 5 || inner.width < 20 {
            return;
        }
        let base = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let buf = frame.buffer_mut();

        // Buttons on the last row, right-aligned.
        let by = inner.bottom() - 1;
        let total: u16 = BUTTONS.iter().map(|b| b.width() as u16).sum::<u16>() + 2 * (BUTTONS.len() as u16 - 1);
        let mut bx = inner.right().saturating_sub(1 + total).max(inner.x);
        self.button_rects.clear();
        for (i, b) in BUTTONS.iter().enumerate() {
            let bw = b.width() as u16;
            let style = if self.pane == Pane::Buttons && self.button == i {
                theme.list_selected
            } else if i == 2 && self.pending.is_empty() {
                base.patch(theme.dim)
            } else {
                base
            };
            buf.set_string(bx, by, b, style);
            self.button_rects.push(Rect::new(bx, by, bw, 1));
            bx += bw + 2;
        }

        let body_y = inner.y;
        let body_h = inner.height - 2;
        let tw = (inner.width / 3).clamp(16, 26);
        self.tree_rect = Rect::new(inner.x, body_y, tw, body_h);
        for yy in body_y..body_y + body_h {
            buf.set_string(inner.x + tw, yy, "│", theme.popup_border);
        }

        // Tree.
        self.tree.fit(PAGES.len(), body_h as usize);
        let tree_style = if self.pane == Pane::Tree { theme.list_selected } else { theme.list_selected_unfocused };
        for (row, i) in self.tree.visible().enumerate() {
            let p = &PAGES[i];
            let text = format!("{}{}", if p.parent.is_some() { "  " } else { "" }, p.title);
            let style = if i == self.page() { tree_style } else { base };
            let r = Rect::new(inner.x, body_y + row as u16, tw, 1);
            buf.set_style(r, style);
            buf.set_string(r.x, r.y, fit(&text, tw as usize), style);
        }

        // Page.
        let px = inner.x + tw + 1;
        let pw = (inner.width - tw - 1) as usize;
        let page = &PAGES[self.page()];
        let crumb = match page.parent {
            Some(p) => format!("{} › {}", PAGES[p].title, page.title),
            None => page.title.to_string(),
        };
        buf.set_string(px, body_y, fit(&crumb, pw), theme.title_focused);
        let mut top = body_y + 2;
        let bottom = body_y + body_h;
        self.rows_rect = Rect::default();
        self.value_col = px;

        match &self.config {
            Err(msg) => {
                let lines = wrap(msg, pw, 6);
                for l in &lines {
                    buf.set_string(px, top, l, base.patch(theme.error));
                    top += 1;
                }
                top += 1;
                self.rows.fit(1, 1);
                self.rows_rect = Rect::new(px, top, pw as u16, 1);
                let style = if self.pane == Pane::Page { theme.list_selected } else { theme.list_selected_unfocused };
                buf.set_style(self.rows_rect, style);
                buf.set_string(px, top, OPEN_FILE_ROW, style);
            }
            Ok(_) if page.keys.is_empty() => {
                let kids: Vec<&str> = PAGES.iter().filter(|p| p.parent == Some(self.page())).map(|p| p.title).collect();
                let text = format!("Select a page: {}", kids.join(", "));
                for l in wrap(&text, pw, (bottom.saturating_sub(top)) as usize) {
                    buf.set_string(px, top, l, base.patch(theme.dim));
                    top += 1;
                }
            }
            Ok(_) => {
                let sel = page.keys.get(self.rows.selected).and_then(|k| setting(k));
                let mut help: Vec<String> = Vec::new();
                if let Some(st) = sel {
                    help.extend(wrap(st.help, pw, 2));
                    if let Some(c) = class_of(st.key) {
                        help.extend(wrap(c.describe(), pw, 1));
                    }
                    if st.kind != Kind::External {
                        help.push("Delete resets to the default.".into());
                    }
                    help.truncate(MAX_HELP_ROWS);
                }
                let help_h = help.len() as u16;
                let sep = u16::from(help_h > 0);
                let rows_h = (bottom - top).saturating_sub(help_h + sep);
                self.rows.fit(page.keys.len(), rows_h as usize);
                self.rows_rect = Rect::new(px, top, pw as u16, rows_h);
                let label_w = page.keys.iter().filter_map(|k| setting(k)).map(|s| s.label.width()).max().unwrap_or(0) + 2 + MARK_W;
                let label_w = label_w.min(pw / 2);
                self.value_col = px + label_w as u16;
                let sel_style = if self.pane == Pane::Page { theme.list_selected } else { theme.list_selected_unfocused };
                for (row, i) in self.rows.visible().enumerate() {
                    let Some(st) = setting(page.keys[i]) else { continue };
                    let ry = top + row as u16;
                    let style = if i == self.rows.selected { sel_style } else { base };
                    buf.set_style(Rect::new(px, ry, pw as u16, 1), style);
                    let edit = self.pending_edit(st.key);
                    let mstyle = style.patch(theme.git_modified);
                    if edit.is_some() {
                        buf.set_string(px, ry, "*", mstyle);
                    }
                    buf.set_string(px + MARK_W as u16, ry, fit(st.label, label_w.saturating_sub(2 + MARK_W)), style);
                    let tag = class_of(st.key).map_or("", |c| c.tag());
                    let tag_w = if tag.is_empty() { 0 } else { tag.width() + 1 };
                    let avail = pw.saturating_sub(label_w + tag_w);
                    let (text, placeholder) = self.value_text(st);
                    let vstyle = if edit.is_some() {
                        mstyle
                    } else if placeholder {
                        style.patch(theme.dim)
                    } else {
                        style
                    };
                    buf.set_string(self.value_col, ry, fit(&text, avail), vstyle);
                    if matches!(edit, Some(Edit::Reset)) {
                        let used = text.width().min(avail);
                        buf.set_string(self.value_col + used as u16, ry, fit(" (default)", avail - used), style.patch(theme.dim));
                    }
                    if tag_w > 0 {
                        let tstyle = if class_of(st.key) == Some(crate::config::reload::ReloadClass::Restart) {
                            style.patch(theme.warn)
                        } else {
                            style.patch(theme.dim)
                        };
                        buf.set_string(px + (pw - tag.width()) as u16, ry, tag, tstyle);
                    }
                }
                for (i, l) in help.iter().enumerate() {
                    buf.set_string(px, bottom - help_h + i as u16, l, base.patch(theme.dim));
                }
            }
        }
    }
}

/// Up/Down/Home/End/PageUp/PageDown on a list; whether the key was one.
fn move_list(list: &mut ListState, code: KeyCode) -> bool {
    let page = list.viewport.max(1);
    match code {
        KeyCode::Up => list.up(1),
        KeyCode::Down => list.down(1),
        KeyCode::Home => list.home(),
        KeyCode::End => list.end(),
        KeyCode::PageUp => list.up(page),
        KeyCode::PageDown => list.down(page),
        _ => return false,
    }
    true
}
