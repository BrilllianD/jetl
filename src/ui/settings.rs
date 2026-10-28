//! The Settings window: a modal page tree over `config.toml`'s keys
//! (`config::catalog`). Edits are staged in `pending` until Apply or OK.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::catalog::{DEPENDENCIES, KEYMAP, Kind, PAGES, Setting, setting};
use crate::config::edit::{Edit, KeymapEdit, Value};
use crate::config::keymap::Keymap;
use crate::config::reload::class_of;
use crate::config::theme::{self, Theme};
use crate::config::{Config, FileText};
use crate::doctor::{Check, Group, Report, Status};
use crate::event::Mouse;
use crate::ui::settings_keymap::{KeymapPage, KeymapSelection};
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
    /// Settings › Keymap: ask for a key for the selected action (`add`) or a
    /// new key for the selected key row.
    EditBinding {
        add: bool,
    },
    /// Run the dependency probe (`App::check_dependencies`).
    CheckDependencies,
}

const BUTTONS: [&str; 3] = ["[ OK ]", "[ Cancel ]", "[ Apply ]"];
const OPEN_FILE_ROW: &str = "Open config.toml";
pub(crate) const MAX_HELP_ROWS: usize = 7;
/// Cells before a label: the pending marker and a space.
pub(crate) const MARK_W: usize = 2;

pub struct SettingsWindow {
    config: Result<Config, String>,
    defaults: Config,
    /// Staged edits, one per key, in the order they were made.
    pending: Vec<(&'static str, Edit)>,
    /// Staged keymap edits, in order; written to `keymap.toml` by Apply/OK.
    keymap_pending: Vec<KeymapEdit>,
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
    /// The live templates folder, named in the help of `editor.snippets`.
    snippets_dir: PathBuf,
    /// The last dependency report (Dependencies page).
    deps: Option<Report>,
    /// The probe whose answer is awaited; 0 = none started.
    deps_seq: u64,
    /// A probe is running.
    deps_running: bool,
    /// The Keymap page's tree, search and selection.
    keymap_page: KeymapPage,
}

enum DepLine<'a> {
    Group(Group),
    Check(&'a Check),
}

/// Group headings and checks, a heading before each run of one group.
fn dep_lines(r: &Report) -> Vec<DepLine<'_>> {
    let mut out = Vec::new();
    let mut last = None;
    for c in &r.checks {
        if last != Some(c.group) {
            out.push(DepLine::Group(c.group));
            last = Some(c.group);
        }
        out.push(DepLine::Check(c));
    }
    out
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
            keymap_pending: Vec::new(),
            tree: ListState::default(),
            rows: ListState::default(),
            pane: Pane::Tree,
            button: 0,
            rect: Rect::default(),
            tree_rect: Rect::default(),
            rows_rect: Rect::default(),
            value_col: 0,
            button_rects: Vec::new(),
            snippets_dir: path.parent().map(|d| d.join(crate::config::snippets::SNIPPETS_DIR)).unwrap_or_default(),
            deps: None,
            deps_seq: 0,
            deps_running: false,
            keymap_page: KeymapPage::default(),
        }
    }

    /// A probe with this seq was started; its answer is the one to show.
    pub fn probe_started(&mut self, seq: u64) {
        self.deps_seq = seq;
        self.deps_running = true;
    }

    /// The answer of probe `seq` was dropped as stale: stop showing it as running.
    pub fn probe_dropped(&mut self, seq: u64) {
        if seq == self.deps_seq {
            self.deps_running = false;
        }
    }

    /// Whether a probe is awaited, and its seq.
    #[cfg(test)]
    pub fn awaited_probe(&self) -> (bool, u64) {
        (self.deps_running, self.deps_seq)
    }

    /// The answer of probe `seq`; false (and ignored) unless it is the latest started.
    pub fn dependencies_loaded(&mut self, seq: u64, report: Report) -> bool {
        if seq != self.deps_seq {
            return false;
        }
        self.deps = Some(report);
        self.deps_running = false;
        self.rows.set_len(self.row_count());
        true
    }

    /// The last report shown, if any.
    pub fn dependencies(&self) -> Option<&Report> {
        self.deps.as_ref()
    }

    fn on_deps(&self) -> bool {
        PAGES[self.page()].title == DEPENDENCIES
    }

    fn on_keymap(&self) -> bool {
        PAGES[self.page()].title == KEYMAP
    }

    /// Show `km` on the Keymap page; called on open and after every keymap
    /// reload. It becomes the base: the staged edits are applied on top again.
    pub fn set_keymap(&mut self, km: &Keymap) {
        self.keymap_page.set_staged(km, &self.keymap_pending);
    }

    pub fn keymap_pending(&self) -> &[KeymapEdit] {
        &self.keymap_pending
    }

    pub fn clear_keymap_pending(&mut self) {
        self.keymap_pending.clear();
        self.keymap_page.restage(&[]);
    }

    /// Whether Apply has anything to write: staged settings or keymap edits.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() || !self.keymap_pending.is_empty()
    }

    /// Stage keymap edits; when the shown keymap is back to the base, nothing stays staged.
    pub fn stage_keymap(&mut self, edits: Vec<KeymapEdit>) {
        self.keymap_pending.extend(edits);
        self.keymap_page.restage(&self.keymap_pending);
        if self.keymap_page.is_unchanged() {
            self.keymap_pending.clear();
        }
    }

    /// The selected action or key row of the Keymap page.
    pub fn keymap_selection(&self) -> Option<KeymapSelection> {
        self.keymap_page.selection()
    }

    /// The Keymap page's keymap with the staged edits applied.
    pub fn shown_keymap(&self) -> &Keymap {
        self.keymap_page.keymap()
    }

    #[cfg(test)]
    pub fn keymap_page(&self) -> &KeymapPage {
        &self.keymap_page
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

    #[cfg(test)]
    pub fn tree_rect(&self) -> Rect {
        self.tree_rect
    }

    #[cfg(test)]
    pub fn rows_rect(&self) -> Rect {
        self.rows_rect
    }

    #[cfg(test)]
    pub fn button_rects(&self) -> &[Rect] {
        &self.button_rects
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains((x, y).into())
    }

    fn page(&self) -> usize {
        self.tree.selected
    }

    fn row_count(&self) -> usize {
        if self.on_keymap() {
            return 0;
        }
        if self.on_deps() {
            return self.deps.as_ref().map_or(0, |r| dep_lines(r).len());
        }
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
        if self.on_deps() {
            return false;
        }
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
        if self.on_deps() {
            return Some(SettingsResult::CheckDependencies);
        }
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
        if self.has_pending() { SettingsResult::ConfirmDiscard } else { SettingsResult::Close }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<SettingsResult> {
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        self.tree.len = PAGES.len();
        self.rows.set_len(self.row_count());
        match key.code {
            KeyCode::Esc if self.pane == Pane::Page && self.on_keymap() && self.keymap_page.clear_query() => return None,
            KeyCode::Esc => return Some(self.escape()),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT) => {
                return self.has_pending().then_some(SettingsResult::Apply { close: false });
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
                    if self.page() != before && self.on_deps() {
                        return Some(SettingsResult::CheckDependencies);
                    }
                } else if matches!(key.code, KeyCode::Right | KeyCode::Enter) {
                    if PAGES[before].keys.is_empty()
                        && PAGES[before].title != KEYMAP
                        && let Some(child) = PAGES.iter().position(|p| p.parent == Some(before))
                    {
                        self.select_page(child);
                    }
                    self.pane = Pane::Page;
                }
                None
            }
            Pane::Page => {
                if self.on_keymap() {
                    match (key.code, self.keymap_page.selection()) {
                        (KeyCode::Enter, Some(s)) => return Some(SettingsResult::EditBinding { add: s.chord.is_none() }),
                        (KeyCode::Insert, Some(_)) => return Some(SettingsResult::EditBinding { add: true }),
                        (KeyCode::Delete, Some(KeymapSelection { ctx, chord: Some(chord), .. })) => {
                            self.stage_keymap(vec![KeymapEdit::Unbind { ctx, chord }]);
                            return None;
                        }
                        _ => {}
                    }
                    if !self.keymap_page.handle_key(key) && key.code == KeyCode::Left {
                        self.pane = Pane::Tree;
                    }
                    return None;
                }
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
            0 if !self.has_pending() => Some(SettingsResult::Close),
            0 => Some(SettingsResult::Apply { close: true }),
            1 => Some(self.escape()),
            _ => self.has_pending().then_some(SettingsResult::Apply { close: false }),
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
                    let before = self.page();
                    self.select_page(i);
                    if self.page() != before && self.on_deps() {
                        return Some(SettingsResult::CheckDependencies);
                    }
                } else if self.on_keymap() {
                    if self.keymap_page.click(m.x, m.y, m.clicks) {
                        self.pane = Pane::Page;
                        if m.clicks >= 2
                            && self.keymap_page.hits_row(m.x, m.y)
                            && let Some(s) = self.keymap_page.selection()
                        {
                            return Some(SettingsResult::EditBinding { add: s.chord.is_none() });
                        }
                    }
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
                    if self.on_keymap() {
                        self.keymap_page.scroll(delta);
                    } else {
                        self.rows.scroll(delta);
                    }
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
        let total: u16 = BUTTONS.iter().map(|b| b.width() as u16).sum::<u16>() + 2 * (BUTTONS.len() as u16 - 1);
        if inner.height < 5 || inner.width < total + 1 {
            // Only the frame fits; drop the rects so a click after a resize cannot hit the old layout.
            self.button_rects.clear();
            self.tree_rect = Rect::default();
            self.rows_rect = Rect::default();
            return;
        }
        let base = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let buf = frame.buffer_mut();

        // Buttons on the last row, right-aligned.
        let by = inner.bottom() - 1;
        let mut bx = inner.right().saturating_sub(1 + total).max(inner.x);
        self.button_rects.clear();
        for (i, b) in BUTTONS.iter().enumerate() {
            let bw = b.width() as u16;
            let style = if self.pane == Pane::Buttons && self.button == i {
                theme.list_selected
            } else if i == 2 && !self.has_pending() {
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
            None if self.on_deps() && self.deps_running => format!("{} (checking…)", page.title),
            None => page.title.to_string(),
        };
        buf.set_string(px, body_y, fit(&crumb, pw), theme.title_focused);
        let mut top = body_y + 2;
        let bottom = body_y + body_h;
        self.rows_rect = Rect::default();
        self.value_col = px;

        if self.on_deps() {
            self.render_dependencies(buf, px, pw, top, bottom, base, theme);
            return;
        }
        if self.on_keymap() {
            let area = Rect::new(px, top, pw as u16, bottom.saturating_sub(top));
            self.keymap_page.render(frame, area, base, self.pane == Pane::Page, theme);
            self.rows_rect = self.keymap_page.rows_rect();
            return;
        }
        match &self.config {
            Err(msg) => {
                // The "Open config.toml" row has priority over message lines.
                let free = bottom.saturating_sub(top) as usize;
                let msg_rows = free.saturating_sub(2).min(6);
                if msg_rows > 0 {
                    for l in wrap(msg, pw, msg_rows) {
                        buf.set_string(px, top, l, base.patch(theme.error));
                        top += 1;
                    }
                    top += 1;
                }
                if top < bottom {
                    self.rows.fit(1, 1);
                    self.rows_rect = Rect::new(px, top, pw as u16, 1);
                    let style = if self.pane == Pane::Page { theme.list_selected } else { theme.list_selected_unfocused };
                    buf.set_style(self.rows_rect, style);
                    buf.set_string(px, top, fit(OPEN_FILE_ROW, pw), style);
                }
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
                    if st.key == "editor.snippets" {
                        help.extend(wrap(&format!("Folder: {}", self.snippets_dir.display()), pw, 2));
                    }
                    if let Some(c) = class_of(st.key) {
                        help.extend(wrap(c.describe(), pw, 1));
                    }
                    if st.kind != Kind::External {
                        help.push("Delete resets to the default.".into());
                    }
                    help.push(format!("Key: {}", st.key));
                    // Keep at least one row for the list itself.
                    help.truncate(MAX_HELP_ROWS.min(bottom.saturating_sub(top).saturating_sub(2) as usize));
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
                    if tag_w > 0 && label_w + tag.width() < pw {
                        let tstyle = if class_of(st.key) == Some(crate::config::reload::ReloadClass::Restart) {
                            style.patch(theme.warn)
                        } else {
                            style.patch(theme.dim)
                        };
                        buf.set_string(px + (pw - tag.width()) as u16, ry, tag, tstyle);
                    }
                }
                for (i, l) in help.iter().enumerate() {
                    buf.set_string(px, bottom - help_h + i as u16, fit(l, pw), base.patch(theme.dim));
                }
            }
        }
    }
}

impl SettingsWindow {
    #[allow(clippy::too_many_arguments)]
    fn render_dependencies(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        px: u16,
        pw: usize,
        top: u16,
        bottom: u16,
        base: Style,
        theme: &Theme,
    ) {
        let Some(report) = &self.deps else {
            let msg = if self.deps_running { "Checking…" } else { "Not checked. Enter checks." };
            buf.set_string(px, top, fit(msg, pw), base.patch(theme.dim));
            self.rows.fit(0, 0);
            return;
        };
        let lines = dep_lines(report);
        let mut help: Vec<String> = Vec::new();
        if let Some(DepLine::Check(c)) = lines.get(self.rows.selected) {
            help.extend(wrap(&format!("{}: {}", c.name, c.detail), pw, 3));
        }
        help.push(if self.deps_running {
            "Checking…".into()
        } else if report.missing() == 0 {
            "Everything found. Enter checks again.".into()
        } else {
            format!("{} missing. Enter checks again.", report.missing())
        });
        help.truncate(MAX_HELP_ROWS.min(bottom.saturating_sub(top).saturating_sub(2) as usize));
        let help_h = help.len() as u16;
        let sep = u16::from(help_h > 0);
        let rows_h = (bottom - top).saturating_sub(help_h + sep);
        // The help height follows the selection, so the viewport can shrink under
        // it; keep the selected row in view then (wheel scrolling leaves it alone).
        let resized = self.rows.viewport != rows_h as usize;
        self.rows.fit(lines.len(), rows_h as usize);
        if resized {
            self.rows.clamp();
        }
        self.rows_rect = Rect::new(px, top, pw as u16, rows_h);

        let status_w = 8;
        let name_w = report.checks.iter().map(|c| c.name.width()).max().unwrap_or(0).min(pw / 3);
        let detail_off = 2 + status_w + name_w + 2;
        let sel_style = if self.pane == Pane::Page { theme.list_selected } else { theme.list_selected_unfocused };
        for (row, i) in self.rows.visible().enumerate() {
            let ry = top + row as u16;
            let style = if i == self.rows.selected { sel_style } else { base };
            buf.set_style(Rect::new(px, ry, pw as u16, 1), style);
            match &lines[i] {
                DepLine::Group(g) => buf.set_string(px, ry, fit(g.label(), pw), style.patch(theme.title)),
                DepLine::Check(c) => {
                    let s = match c.status {
                        Status::Ok => theme.git_added,
                        Status::Missing => theme.warn,
                        Status::Off | Status::Unknown => theme.dim,
                    };
                    if pw > 2 {
                        buf.set_string(px + 2, ry, fit(c.status.label(), (pw - 2).min(status_w)), style.patch(s));
                    }
                    if pw > 2 + status_w {
                        buf.set_string(px + 2 + status_w as u16, ry, fit(&c.name, name_w.min(pw - 2 - status_w)), style);
                    }
                    if pw > detail_off {
                        buf.set_string(px + detail_off as u16, ry, fit(&c.detail, pw - detail_off), style);
                    }
                }
            }
        }
        for (i, l) in help.iter().enumerate() {
            buf.set_string(px, bottom - help_h + i as u16, fit(l, pw), base.patch(theme.dim));
        }
    }
}

/// Up/Down/Home/End/PageUp/PageDown on a list; whether the key was one.
pub(crate) fn move_list(list: &mut ListState, code: KeyCode) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn window() -> SettingsWindow {
        let mut w =
            SettingsWindow::new(&FileText::Text("[lsp]\nenabled = false\n".into()), Path::new("/tmp/jetl-test-settings/config.toml"));
        assert_eq!(PAGES[2].title, "General");
        draw(&mut w); // sizes the tree list
        w.select_page(2);
        draw(&mut w);
        w
    }

    fn draw(w: &mut SettingsWindow) {
        draw_at(w, 80, 24);
    }

    fn draw_at(w: &mut SettingsWindow, width: u16, height: u16) {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| w.render(f, f.area(), &theme)).unwrap();
    }

    fn click(w: &mut SettingsWindow, x: u16, y: u16, clicks: u8) -> Option<SettingsResult> {
        w.handle_mouse(&Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks, mods: KeyModifiers::NONE })
    }

    fn row_y(w: &SettingsWindow, key: &str) -> u16 {
        w.rows_rect.y + PAGES[2].keys.iter().position(|k| *k == key).unwrap() as u16
    }

    #[test]
    fn snippets_row_names_the_folder() {
        let mut w = window();
        w.rows.selected = PAGES[2].keys.iter().position(|k| *k == "editor.snippets").unwrap();
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| w.render(f, f.area(), &theme)).unwrap();
        let buf = term.backend().buffer();
        let lines: Vec<String> = (0..30).map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect()).collect();
        assert!(lines.iter().any(|l| l.contains("Folder: /tmp/jetl-test-settings/snippets")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("Key: editor.snippets")), "{lines:#?}");
    }

    #[test]
    fn a_frame_only_redraw_drops_the_old_click_targets() {
        let mut w = window();
        assert!(!w.button_rects.is_empty());
        assert!(w.tree_rect.width > 0);
        assert!(w.rows_rect.width > 0);
        let ok = w.button_rects[0];
        let tree_pt = (w.tree_rect.x + 1, w.tree_rect.y + 6);
        let row_pt = (w.rows_rect.x + 2, w.rows_rect.y);
        let (page, pane) = (w.page(), w.pane);
        draw_at(&mut w, 30, 24);
        assert!(w.button_rects.is_empty());
        assert_eq!(w.tree_rect, Rect::default());
        assert_eq!(w.rows_rect, Rect::default());
        assert_eq!(click(&mut w, ok.x, ok.y, 1), None);
        assert_eq!(click(&mut w, tree_pt.0, tree_pt.1, 1), None);
        assert_eq!(click(&mut w, row_pt.0, row_pt.1, 2), None);
        assert_eq!((w.page(), w.pane), (page, pane));
    }

    #[test]
    fn bool_cell_click_toggles_and_a_double_click_does_not_undo_it() {
        let mut w = window();
        let y = row_y(&w, "editor.auto_close");
        let vc = w.value_col;
        let before = w.value("editor.auto_close");
        assert_eq!(click(&mut w, vc, y, 1), None);
        assert_ne!(w.value("editor.auto_close"), before);
        assert_eq!(w.pending().len(), 1);
        assert_eq!(click(&mut w, vc, y, 2), None);
        assert_ne!(w.value("editor.auto_close"), before);
        assert_eq!(w.pending().len(), 1);
        assert_eq!(click(&mut w, vc, y, 1), None);
        assert_eq!(w.value("editor.auto_close"), before);
        assert!(w.pending().is_empty());
    }

    #[test]
    fn double_click_on_a_row_acts_as_enter() {
        let mut w = window();
        let y_int = row_y(&w, "editor.scrolloff");
        let x = w.rows_rect.x + 2;
        assert_eq!(click(&mut w, x, y_int, 1), None);
        assert_eq!(w.selected_key(), Some("editor.scrolloff"));
        assert_eq!(w.pane, Pane::Page);
        assert_eq!(click(&mut w, x, y_int, 2), Some(SettingsResult::EditValue("editor.scrolloff")));

        let y_bool = row_y(&w, "editor.auto_close");
        let before = w.value("editor.auto_close");
        assert!(x < w.value_col);
        assert_eq!(click(&mut w, x, y_bool, 1), None);
        assert_eq!(w.value("editor.auto_close"), before);
        assert_eq!(click(&mut w, x, y_bool, 2), None);
        assert_ne!(w.value("editor.auto_close"), before);
        assert_eq!(w.pending().len(), 1);
    }

    fn report() -> Report {
        let c = |group, name: &str, status, detail: &str| Check { group, name: name.into(), status, detail: detail.into() };
        Report {
            checks: vec![
                c(Group::Lsp, "rust-analyzer", Status::Ok, "/usr/bin/rust-analyzer (rust)"),
                c(Group::Lsp, "gopls", Status::Missing, "gopls not found (go)"),
                c(Group::Tool, "git", Status::Ok, "/usr/bin/git"),
            ],
        }
    }

    fn lines(w: &mut SettingsWindow, width: u16, height: u16) -> Vec<String> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| w.render(f, f.area(), &theme)).unwrap();
        let buf = term.backend().buffer();
        (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect()).collect()
    }

    fn keymap_window() -> SettingsWindow {
        let mut w = window();
        w.set_keymap(&crate::config::keymap::default_keymap());
        w.select_page(PAGES.iter().position(|p| p.title == KEYMAP).unwrap());
        w.pane = Pane::Tree;
        draw_at(&mut w, 100, 30);
        w
    }

    #[test]
    fn keymap_page_shows_contexts_and_types_into_search() {
        let mut w = keymap_window();
        assert_eq!(w.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(w.pane, Pane::Page);
        assert_eq!(PAGES[w.page()].title, KEYMAP);
        let l = lines(&mut w, 100, 30).join("\n");
        assert!(l.contains("Search:") && l.contains("▸ Global ("), "{l}");
        for c in "quit".chars() {
            w.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(w.keymap_page().query(), "quit");
        let l = lines(&mut w, 100, 30).join("\n");
        assert!(l.contains("Exit jetl") && l.contains("Ctrl+Q"), "{l}");
        assert_eq!(w.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(w.keymap_page().query(), "");
        assert_eq!(w.handle_key(key(KeyCode::Esc)), Some(SettingsResult::Close));
    }

    #[test]
    fn left_on_a_collapsed_context_returns_to_the_tree() {
        let mut w = keymap_window();
        w.handle_key(key(KeyCode::Enter));
        assert_eq!(w.pane, Pane::Page);
        w.handle_key(key(KeyCode::Left));
        assert_eq!(w.pane, Pane::Tree);
    }

    #[test]
    fn mode_page_holds_keymap_mode() {
        let mut w = window();
        w.select_page(PAGES.iter().position(|p| p.title == "Mode").unwrap());
        assert_eq!(w.selected_key(), Some("keymap.mode"));
    }

    #[test]
    fn keymap_rows_click_selects_and_marker_click_toggles() {
        use crate::ui::settings_keymap::KeymapRow;
        let mut w = keymap_window();
        let r = w.keymap_page().rows_rect();
        click(&mut w, r.x + 5, r.y + 1, 1);
        assert_eq!(w.pane, Pane::Page);
        assert_eq!(w.keymap_page().selected_row(), Some(&w.keymap_page().rows()[1]));
        click(&mut w, r.x + MARK_W as u16, r.y, 1);
        assert!(matches!(w.keymap_page().rows()[0], KeymapRow::Context { expanded: true, .. }));
        // The second click of a double click on the marker must not undo the first.
        click(&mut w, r.x + MARK_W as u16, r.y, 2);
        assert!(matches!(w.keymap_page().rows()[0], KeymapRow::Context { expanded: true, .. }));
    }

    #[test]
    fn keymap_double_click_on_context_text_toggles() {
        use crate::ui::settings_keymap::KeymapRow;
        let mut w = keymap_window();
        let r = w.keymap_page().rows_rect();
        click(&mut w, r.x + 5, r.y, 1);
        assert!(matches!(w.keymap_page().rows()[0], KeymapRow::Context { expanded: false, .. }));
        click(&mut w, r.x + 5, r.y, 2);
        assert!(matches!(w.keymap_page().rows()[0], KeymapRow::Context { expanded: true, .. }));
    }

    #[test]
    fn keymap_click_on_search_line_or_empty_area_focuses_the_page() {
        let mut w = keymap_window();
        let r = w.keymap_page().rows_rect();
        click(&mut w, r.x + 10, r.y - 2, 1);
        assert_eq!(w.pane, Pane::Page);
        w.pane = Pane::Tree;
        click(&mut w, r.x + 10, r.y + r.height, 1);
        assert_eq!(w.pane, Pane::Page);
    }

    fn bind_edit(key: &str) -> KeymapEdit {
        KeymapEdit::Bind { ctx: "global".into(), chord: crate::config::keymap::parse_chord(key).unwrap(), id: "app.quit" }
    }

    fn unbind_edit(key: &str) -> KeymapEdit {
        KeymapEdit::Unbind { ctx: "global".into(), chord: crate::config::keymap::parse_chord(key).unwrap() }
    }

    /// The Keymap page with the `Ctrl+Q` key row selected (via the search).
    fn on_ctrl_q_key() -> SettingsWindow {
        let mut w = keymap_window();
        w.handle_key(key(KeyCode::Enter));
        for c in "ctrl+q".chars() {
            w.handle_key(key(KeyCode::Char(c)));
        }
        assert!(w.keymap_selection().is_some_and(|s| s.chord.is_none()));
        w.handle_key(key(KeyCode::Down));
        assert!(w.keymap_selection().is_some_and(|s| s.chord.is_some()));
        w
    }

    #[test]
    fn keymap_delete_stages_and_enables_apply() {
        let mut w = on_ctrl_q_key();
        assert_eq!(w.press(2), None);
        assert_eq!(w.handle_key(key(KeyCode::Delete)), None);
        assert_eq!(w.keymap_pending(), [unbind_edit("ctrl-q")]);
        assert!(w.pending().is_empty());
        assert_eq!(w.press(2), Some(SettingsResult::Apply { close: false }));
        assert_eq!(w.press(0), Some(SettingsResult::Apply { close: true }));
        assert_eq!(w.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(w.handle_key(key(KeyCode::Esc)), Some(SettingsResult::ConfirmDiscard));
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(w.handle_key(ctrl_s), Some(SettingsResult::Apply { close: false }));
    }

    #[test]
    fn keymap_enter_and_insert_ask_for_a_key() {
        let mut w = on_ctrl_q_key();
        assert_eq!(w.handle_key(key(KeyCode::Enter)), Some(SettingsResult::EditBinding { add: false }));
        assert_eq!(w.handle_key(key(KeyCode::Insert)), Some(SettingsResult::EditBinding { add: true }));
        w.handle_key(key(KeyCode::Up));
        assert_eq!(w.handle_key(key(KeyCode::Enter)), Some(SettingsResult::EditBinding { add: true }));
    }

    #[test]
    fn undoing_a_staged_edit_clears_it() {
        let mut w = keymap_window();
        w.stage_keymap(vec![bind_edit("ctrl-alt-y")]);
        assert!(w.has_pending());
        w.stage_keymap(vec![unbind_edit("ctrl-alt-y")]);
        assert!(w.keymap_pending().is_empty());
        assert!(!w.has_pending());
    }

    #[test]
    fn set_keymap_keeps_staged_edits() {
        let mut w = keymap_window();
        w.stage_keymap(vec![unbind_edit("ctrl-q")]);
        w.set_keymap(&crate::config::keymap::default_keymap());
        assert_eq!(w.keymap_pending().len(), 1);
        let ctrl_q = crate::config::keymap::parse_chord("ctrl-q").unwrap();
        assert!(!w.shown_keymap().table("global").unwrap().bindings.contains_key(&ctrl_q));
    }

    #[test]
    fn keymap_double_click_on_a_row_asks_for_a_key() {
        let mut w = on_ctrl_q_key();
        draw_at(&mut w, 100, 30);
        let r = w.keymap_page().rows_rect();
        let key_row = w.keymap_page().rows().iter().position(|r| matches!(r, crate::ui::settings_keymap::KeymapRow::Key { .. })).unwrap();
        let y = r.y + key_row as u16;
        assert_eq!(click(&mut w, r.x + 8, y, 1), None);
        assert_eq!(click(&mut w, r.x + 8, y, 2), Some(SettingsResult::EditBinding { add: false }));
    }

    #[test]
    fn keymap_double_click_on_blank_space_below_the_rows_does_nothing() {
        let mut w = on_ctrl_q_key();
        draw_at(&mut w, 100, 60);
        let r = w.keymap_page().rows_rect();
        let n = w.keymap_page().rows().len() as u16;
        assert!(n < r.height, "need blank space below the rows");
        assert_eq!(click(&mut w, r.x + 8, r.y + n, 2), None);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn entering_the_dependencies_page_asks_for_a_probe() {
        let mut w = window();
        assert_eq!(w.handle_key(key(KeyCode::End)), Some(SettingsResult::CheckDependencies));
        assert_eq!(PAGES[w.page()].title, DEPENDENCIES);
        assert_eq!(w.handle_key(key(KeyCode::End)), None);
        assert_eq!(w.handle_key(key(KeyCode::Up)), None);
        assert_eq!(w.handle_key(key(KeyCode::Down)), Some(SettingsResult::CheckDependencies));
        assert_eq!(w.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(w.pane, Pane::Page);
        assert_eq!(w.handle_key(key(KeyCode::Enter)), Some(SettingsResult::CheckDependencies));
        assert_eq!(w.handle_key(key(KeyCode::Char(' '))), Some(SettingsResult::CheckDependencies));
    }

    #[test]
    fn a_tree_click_on_dependencies_asks_for_a_probe() {
        let mut w = window();
        draw_at(&mut w, 80, 26);
        let (x, y) = (w.tree_rect.x + 1, w.tree_rect.y + (PAGES.len() - 1) as u16);
        assert_eq!(click(&mut w, x, y, 1), Some(SettingsResult::CheckDependencies));
        assert_eq!(click(&mut w, x, y, 1), None);
    }

    #[test]
    fn only_the_latest_probe_answer_is_shown() {
        let mut w = window();
        w.probe_started(1);
        w.probe_started(2);
        assert!(!w.dependencies_loaded(1, report()));
        assert!(w.dependencies().is_none());
        assert!(w.dependencies_loaded(2, report()));
        assert!(w.dependencies().is_some());
    }

    #[test]
    fn selected_dependency_row_stays_in_view_when_help_grows() {
        let c = |i: usize, detail: String| Check { group: Group::Tool, name: format!("c{i}"), status: Status::Ok, detail };
        let checks = (0..30).map(|i| c(i, if i % 3 == 0 { "long detail words ".repeat(12) } else { "ok".into() })).collect();
        let mut w = window();
        w.handle_key(key(KeyCode::End));
        w.probe_started(1);
        w.dependencies_loaded(1, Report { checks });
        lines(&mut w, 50, 14);
        w.handle_key(key(KeyCode::Enter));
        w.handle_key(key(KeyCode::Enter));
        for _ in 0..34 {
            w.handle_key(key(KeyCode::Down));
            lines(&mut w, 50, 14);
            assert!(w.rows.visible().any(|i| i == w.rows.selected), "sel {} not in {:?}", w.rows.selected, w.rows.visible());
        }
    }

    #[test]
    fn dependencies_page_renders_not_checked_checking_then_the_report() {
        let mut w = window();
        w.handle_key(key(KeyCode::End));
        let l = lines(&mut w, 80, 24);
        assert!(l.iter().any(|l| l.contains("Not checked. Enter checks.")), "{l:#?}");
        assert!(!l.iter().any(|l| l.contains("Checking…")), "{l:#?}");
        assert!(!l.iter().any(|l| l.contains("(checking…)")), "{l:#?}");
        w.probe_started(1);
        let l = lines(&mut w, 80, 24);
        assert!(l.iter().any(|l| l.contains("Dependencies (checking…)")), "{l:#?}");
        assert!(l.iter().any(|l| l.contains("Checking…")), "{l:#?}");
        w.probe_dropped(1);
        let l = lines(&mut w, 80, 24);
        assert!(l.iter().any(|l| l.contains("Not checked. Enter checks.")), "{l:#?}");
        assert!(!l.iter().any(|l| l.contains("Checking…")), "{l:#?}");
        assert!(!l.iter().any(|l| l.contains("(checking…)")), "{l:#?}");
        w.probe_started(2);
        w.dependencies_loaded(2, report());
        let l = lines(&mut w, 80, 24);
        for want in ["Language servers", "Tools", "missing", "gopls not found (go)", "1 missing. Enter checks again."] {
            assert!(l.iter().any(|l| l.contains(want)), "{want}: {l:#?}");
        }
        assert!(!l.iter().any(|l| l.contains("(checking…)")), "{l:#?}");
    }

    #[test]
    fn invalid_config_keeps_the_dependencies_page() {
        let mut w = SettingsWindow::new(&FileText::Text("[editor\n".into()), Path::new("/tmp/jetl-test-settings/config.toml"));
        lines(&mut w, 80, 24);
        w.handle_key(key(KeyCode::End));
        w.probe_started(1);
        w.dependencies_loaded(1, report());
        let l = lines(&mut w, 80, 24);
        assert!(l.iter().any(|l| l.contains("Language servers")), "{l:#?}");
        assert!(!l.iter().any(|l| l.contains("Open config.toml")), "{l:#?}");
        assert_eq!(w.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(w.handle_key(key(KeyCode::Enter)), Some(SettingsResult::CheckDependencies));
    }

    #[test]
    fn a_tree_click_selects_the_page() {
        let mut w = window();
        assert_eq!(PAGES[6].title, "Keymap");
        let (x, y) = (w.tree_rect.x + 1, w.tree_rect.y + 6);
        assert_eq!(click(&mut w, x, y, 1), None);
        assert_eq!(w.page(), 6);
        assert_eq!(w.pane, Pane::Tree);
    }
}
