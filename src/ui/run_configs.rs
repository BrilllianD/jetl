//! The Run Configurations window (Run > Edit Configurations…): a modal list
//! of the run configurations and a form for the selected one. Edits are
//! staged in the entries; Apply / OK turn them into `ConfigEdit`s for
//! `.jetl/run.toml` (`run::edit`). Only project entries can be edited;
//! config.toml and detected ones are read-only and can be copied; launch.json ones are read-only and cannot be copied.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::run::edit::{ConfigEdit, Slot};
use crate::run::{MatcherKind, Origin, RunTask, TaskCommand, quote_word, split_words};
use crate::ui::settings::move_list;
use crate::ui::toasts::wrap;
use crate::ui::widgets::list::ListState;
use crate::util::text::fit;

/// A field of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Command,
    Args,
    Cwd,
    Env,
    Matcher,
}

impl Field {
    pub const ALL: [Field; 6] = [Field::Name, Field::Command, Field::Args, Field::Cwd, Field::Env, Field::Matcher];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Command => "Command",
            Field::Args => "Arguments",
            Field::Cwd => "Working directory",
            Field::Env => "Environment",
            Field::Matcher => "Output",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Field::Name => "Shown in the picker and the Run window.",
            Field::Command => "A string runs through sh -c; a list of words (from config files) runs directly.",
            Field::Args => "Appended to the command, as shell words: --release 'a b'.",
            Field::Cwd => "Relative to the project root; empty means the root.",
            Field::Env => "Added to jetl's environment, as KEY=value words.",
            Field::Matcher => {
                "How the output is read: auto picks from the command; problems links file locations; tests shows a test tree; none shows plain text."
            }
        }
    }
}

/// A value for `RunConfigsWindow::set_field`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Text(String),
    Matcher(Option<MatcherKind>),
}

/// One configuration of the list.
///
/// Read-only means `slot` is `None` and `new` is false: config.toml and
/// detected entries. Project entries have a `slot` and the `original` they
/// were loaded as; entries added or copied in the window have `new`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub task: RunTask,
    /// Where it sits in `.jetl/run.toml`; `None` for config.toml, detected and new entries.
    pub slot: Option<Slot>,
    /// The value loaded from disk (project entries), to tell changed ones.
    pub original: Option<RunTask>,
    /// Added or copied in this window, not in the file yet.
    pub new: bool,
}

impl Entry {
    fn editable(&self) -> bool {
        self.slot.is_some() || self.new
    }

    /// Whether `f` differs from the file's value (never for entries without one).
    fn changed(&self, f: Field) -> bool {
        let Some(o) = &self.original else { return false };
        let (a, b) = (&self.task, o);
        match f {
            Field::Name => a.name != b.name,
            Field::Command => a.command != b.command,
            Field::Args => a.args != b.args,
            Field::Cwd => a.cwd.as_deref().filter(|c| !c.is_empty()) != b.cwd.as_deref().filter(|c| !c.is_empty()),
            Field::Env => a.env != b.env,
            Field::Matcher => a.matcher != b.matcher,
        }
    }

    /// The entry was added or differs from the file.
    fn touched(&self) -> bool {
        self.new || self.original.as_ref().is_some_and(|o| *o != self.task)
    }

    fn note(&self) -> Option<&'static str> {
        match self.task.origin {
            _ if self.editable() => None,
            Origin::Detected => Some("Detected from the project files: Copy makes an editable project copy"),
            Origin::LaunchJson => Some("From .vscode/launch.json: edit that file to change it"),
            _ => Some("Defined in config.toml: Copy makes an editable project copy"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigsResult {
    Close,
    EditField(Field),
    /// Write the edits; `close` after a successful write (OK).
    Apply {
        close: bool,
    },
    /// Esc / Cancel with changes: ask before closing.
    ConfirmDiscard,
    Notify(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    List,
    Form,
    Buttons,
}

const BUTTONS: [&str; 3] = ["[ OK ]", "[ Cancel ]", "[ Apply ]"];
const LIST_BUTTONS: [&str; 3] = ["[+]", "[−]", "[Copy]"];
const MAX_HELP_ROWS: usize = 6;
/// Cells before a label: the changed marker and a space.
const MARK_W: usize = 2;
const NEW_NAME: &str = "New configuration";

pub struct RunConfigsWindow {
    entries: Vec<Entry>,
    /// Slots of project entries removed in the window.
    removed: Vec<Slot>,
    /// The text of `.jetl/run.toml` the entries were loaded from.
    base: String,
    /// Why the project file cannot be edited: every entry is read-only.
    error: Option<String>,
    list: ListState,
    rows: ListState,
    pane: Pane,
    /// 0 OK, 1 Cancel, 2 Apply.
    button: usize,
    rect: Rect,
    list_rect: Rect,
    rows_rect: Rect,
    button_rects: Vec<Rect>,
    list_button_rects: Vec<Rect>,
}

/// `base`, else `base 2`, `base 3`, … not among `names`.
fn unique(base: &str, names: &[&str]) -> String {
    if !names.contains(&base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base} {n}")).find(|c| !names.contains(&c.as_str())).unwrap_or_default()
}

impl RunConfigsWindow {
    /// `others` (config.toml entries, or the detected defaults) come first,
    /// then the project's. A `project` error shows in the form and makes
    /// every entry read-only.
    pub fn new(base: String, project: Result<Vec<(Slot, RunTask)>, String>, others: Vec<RunTask>) -> Self {
        let mut entries: Vec<Entry> = others.into_iter().map(|task| Entry { task, slot: None, original: None, new: false }).collect();
        let error = match project {
            Ok(p) => {
                entries.extend(p.into_iter().map(|(slot, task)| Entry {
                    original: Some(task.clone()),
                    task,
                    slot: Some(slot),
                    new: false,
                }));
                None
            }
            Err(e) => Some(e),
        };
        let mut list = ListState::default();
        list.set_len(entries.len());
        let mut rows = ListState::default();
        rows.set_len(Field::ALL.len());
        Self {
            entries,
            removed: Vec::new(),
            base,
            error,
            list,
            rows,
            pane: Pane::List,
            button: 0,
            rect: Rect::default(),
            list_rect: Rect::default(),
            rows_rect: Rect::default(),
            button_rects: Vec::new(),
            list_button_rects: Vec::new(),
        }
    }

    /// Show the same view (selection, pane) as `old`, a window over the
    /// file's earlier state.
    pub fn keep_view_of(&mut self, old: &Self) {
        let name = old.selected().map(|e| e.task.name.as_str());
        let at = name.and_then(|n| self.entries.iter().position(|e| e.task.name == n)).unwrap_or(old.list.selected);
        self.list.select(at);
        self.rows.select(old.rows.selected);
        self.pane = old.pane;
        self.button = old.button;
    }

    /// Select the entry named `name`, if there is one.
    pub fn select_name(&mut self, name: &str) {
        if let Some(i) = self.entries.iter().position(|e| e.task.name == name) {
            self.list.select(i);
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.list.selected)
    }

    #[cfg(test)]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains((x, y).into())
    }

    /// Removals, then updates of changed entries, then additions, each in list order.
    pub fn edits(&self) -> Vec<ConfigEdit> {
        if self.error.is_some() {
            return Vec::new();
        }
        let mut v: Vec<ConfigEdit> = self.removed.iter().map(|s| ConfigEdit::Remove(*s)).collect();
        for e in &self.entries {
            if let (Some(slot), Some(o)) = (e.slot, &e.original)
                && e.task != *o
            {
                v.push(ConfigEdit::Update(slot, e.task.clone()));
            }
        }
        v.extend(self.entries.iter().filter(|e| e.new).map(|e| ConfigEdit::Add(e.task.clone())));
        v
    }

    pub fn dirty(&self) -> bool {
        !self.edits().is_empty()
    }

    /// Old and new name of every edited project entry that was renamed.
    pub fn renames(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .filter(|e| e.slot.is_some())
            .filter_map(|e| e.original.as_ref().filter(|o| o.name != e.task.name).map(|o| (o.name.clone(), e.task.name.clone())))
            .collect()
    }

    /// Whether the editable entries can be written: each has a name and a
    /// command, and no two entries share a name.
    pub fn validate(&self) -> Result<(), String> {
        for e in self.entries.iter().filter(|e| e.editable() && self.error.is_none()) {
            if e.task.name.trim().is_empty() {
                return Err("A configuration has no name".into());
            }
            if e.task.command.display().trim().is_empty() {
                return Err(format!("Configuration '{}' has no command", e.task.name));
            }
        }
        // A config.toml entry may share a name with a project one; only the file we write must be unique.
        let editable: Vec<&Entry> = self.entries.iter().filter(|e| e.editable()).collect();
        for (i, e) in editable.iter().enumerate() {
            if editable[..i].iter().any(|o| o.task.name == e.task.name) {
                return Err(format!("Two configurations are named '{}'", e.task.name));
            }
        }
        Ok(())
    }

    fn editable(&self, e: &Entry) -> bool {
        self.error.is_none() && e.editable()
    }

    /// The text of `f` in `e` and whether it is a placeholder.
    fn field_text(e: &Entry, f: Field) -> (String, bool) {
        let t = &e.task;
        match f {
            Field::Name => (t.name.clone(), false),
            Field::Command => (t.command.display(), false),
            Field::Args if t.args.is_empty() => ("(none)".into(), true),
            Field::Args => (t.args.iter().map(|a| quote_word(a)).collect::<Vec<_>>().join(" "), false),
            Field::Cwd => match t.cwd.as_deref().filter(|c| !c.is_empty()) {
                Some(c) => (c.to_string(), false),
                None => ("(project root)".into(), true),
            },
            Field::Env if t.env.is_empty() => ("(none)".into(), true),
            Field::Env => (t.env.iter().map(|(k, v)| format!("{k}={}", quote_word(v))).collect::<Vec<_>>().join(" "), false),
            Field::Matcher => match t.matcher {
                Some(m) => (m.label().into(), false),
                None => (format!("auto ({})", t.matcher_kind().label()), true),
            },
        }
    }

    /// What an input dialog for `f` starts with: the value, empty for a placeholder.
    pub fn field_initial(&self, f: Field) -> String {
        match self.selected() {
            Some(e) => match Self::field_text(e, f) {
                (_, true) => String::new(),
                (t, false) => t,
            },
            None => String::new(),
        }
    }

    /// Set `f` of the selected entry; an error leaves it unchanged.
    pub fn set_field(&mut self, f: Field, value: FieldValue) -> Result<(), String> {
        let at = self.list.selected;
        let editable = self.entries.get(at).is_some_and(|e| self.editable(e));
        if !editable {
            return Err("This configuration is read-only".into());
        }
        let task = &mut self.entries[at].task;
        match (f, value) {
            (Field::Matcher, FieldValue::Matcher(m)) => task.matcher = m,
            (Field::Matcher, _) | (_, FieldValue::Matcher(_)) => return Err("Not a value of this field".into()),
            (Field::Name, FieldValue::Text(t)) => {
                let t = t.trim();
                if t.is_empty() {
                    return Err("Name is empty".into());
                }
                task.name = t.to_string();
            }
            (Field::Command, FieldValue::Text(t)) => {
                let t = t.trim();
                if t.is_empty() {
                    return Err("Command is empty".into());
                }
                if t != task.command.display() {
                    task.command = TaskCommand::Shell(t.to_string());
                }
            }
            (Field::Args, FieldValue::Text(t)) => task.args = split_words(&t)?,
            (Field::Cwd, FieldValue::Text(t)) => {
                let t = t.trim();
                task.cwd = (!t.is_empty()).then(|| t.to_string());
            }
            (Field::Env, FieldValue::Text(t)) => {
                let mut env = BTreeMap::new();
                for w in split_words(&t)? {
                    match w.split_once('=') {
                        Some((k, v)) if !k.is_empty() => env.insert(k.to_string(), v.to_string()),
                        _ => return Err(format!("Not KEY=value: {w}")),
                    };
                }
                task.env = env;
            }
        }
        Ok(())
    }

    fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.task.name.as_str()).collect()
    }

    fn push_new(&mut self, task: RunTask) {
        self.entries.push(Entry { task: RunTask { origin: Origin::Project, ..task }, slot: None, original: None, new: true });
        self.list.set_len(self.entries.len());
        self.list.end();
    }

    fn locked(&self) -> Option<ConfigsResult> {
        self.error.as_ref().map(|e| ConfigsResult::Notify(e.clone()))
    }

    fn add(&mut self) -> Option<ConfigsResult> {
        if let Some(r) = self.locked() {
            return Some(r);
        }
        let name = unique(NEW_NAME, &self.names());
        self.push_new(RunTask {
            name,
            command: TaskCommand::Shell(String::new()),
            cwd: None,
            env: BTreeMap::new(),
            args: Vec::new(),
            matcher: None,
            origin: Origin::Project,
        });
        None
    }

    fn copy(&mut self) -> Option<ConfigsResult> {
        if let Some(r) = self.locked() {
            return Some(r);
        }
        let e = self.selected()?;
        if e.task.origin == Origin::LaunchJson {
            return Some(ConfigsResult::Notify(
                "A launch.json configuration cannot be copied: its debug settings live in .vscode/launch.json".into(),
            ));
        }
        let name = unique(&format!("{} (copy)", e.task.name), &self.names());
        let task = RunTask { name, ..e.task.clone() };
        self.push_new(task);
        None
    }

    fn remove(&mut self) -> Option<ConfigsResult> {
        let at = self.list.selected;
        let e = self.entries.get(at)?;
        if !self.editable(e) {
            return Some(self.locked().unwrap_or_else(|| ConfigsResult::Notify("Only project configurations can be removed".into())));
        }
        let e = self.entries.remove(at);
        if let Some(s) = e.slot {
            self.removed.push(s);
        }
        self.list.set_len(self.entries.len());
        None
    }

    fn escape(&self) -> ConfigsResult {
        if self.dirty() { ConfigsResult::ConfirmDiscard } else { ConfigsResult::Close }
    }

    /// Enter, Space or a double click on the selected field.
    fn edit_field(&self) -> Option<ConfigsResult> {
        let e = self.selected()?;
        if !self.editable(e) {
            let note = self.error.as_deref().or(e.note()).unwrap_or_default();
            return Some(ConfigsResult::Notify(note.to_string()));
        }
        Some(ConfigsResult::EditField(Field::ALL[self.rows.selected]))
    }

    fn clear_field(&mut self) -> Option<ConfigsResult> {
        let f = Field::ALL[self.rows.selected];
        if matches!(f, Field::Name | Field::Command) {
            return None;
        }
        let e = self.selected()?;
        if !self.editable(e) {
            return self.edit_field();
        }
        let value = if f == Field::Matcher { FieldValue::Matcher(None) } else { FieldValue::Text(String::new()) };
        let _ = self.set_field(f, value);
        None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<ConfigsResult> {
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        self.list.set_len(self.entries.len());
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return Some(self.escape()),
            KeyCode::Char('s') if ctrl => return self.dirty().then_some(ConfigsResult::Apply { close: false }),
            KeyCode::Tab => {
                self.pane = match self.pane {
                    Pane::List => Pane::Form,
                    Pane::Form => Pane::Buttons,
                    Pane::Buttons => Pane::List,
                };
                return None;
            }
            KeyCode::BackTab => {
                self.pane = match self.pane {
                    Pane::List => Pane::Buttons,
                    Pane::Form => Pane::List,
                    Pane::Buttons => Pane::Form,
                };
                return None;
            }
            KeyCode::Char('d') if ctrl && self.pane != Pane::Buttons => return self.copy(),
            _ => {}
        }
        match self.pane {
            Pane::List => {
                if move_list(&mut self.list, key.code) {
                    return None;
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Right => {
                        if !self.entries.is_empty() {
                            self.pane = Pane::Form;
                        }
                        None
                    }
                    KeyCode::Insert => self.add(),
                    KeyCode::Delete => self.remove(),
                    _ => None,
                }
            }
            Pane::Form => {
                if move_list(&mut self.rows, key.code) {
                    return None;
                }
                match key.code {
                    KeyCode::Left => {
                        self.pane = Pane::List;
                        None
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => self.edit_field(),
                    KeyCode::Delete => self.clear_field(),
                    _ => None,
                }
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

    fn press(&mut self, button: usize) -> Option<ConfigsResult> {
        match button {
            0 if !self.dirty() => Some(ConfigsResult::Close),
            0 => Some(ConfigsResult::Apply { close: true }),
            1 => Some(self.escape()),
            _ => self.dirty().then_some(ConfigsResult::Apply { close: false }),
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<ConfigsResult> {
        let p = (m.x, m.y).into();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.button_rects.iter().position(|r| r.contains(p)) {
                    self.pane = Pane::Buttons;
                    self.button = i;
                    return self.press(i);
                }
                if let Some(i) = self.list_button_rects.iter().position(|r| r.contains(p)) {
                    return match i {
                        0 => self.add(),
                        1 => self.remove(),
                        _ => self.copy(),
                    };
                }
                if let Some(i) = self.list.hit(self.list_rect, m.x, m.y) {
                    self.pane = Pane::List;
                    self.list.select(i);
                    if m.clicks >= 2 {
                        self.pane = Pane::Form;
                    }
                } else if let Some(i) = self.rows.hit(self.rows_rect, m.x, m.y) {
                    self.pane = Pane::Form;
                    self.rows.select(i);
                    if m.clicks >= 2 {
                        return self.edit_field();
                    }
                }
                None
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                if self.list_rect.contains(p) {
                    self.list.scroll(delta);
                } else if self.rows_rect.contains(p) {
                    self.rows.scroll(delta);
                }
                None
            }
            _ => None,
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
            .title(" Run Configurations ")
            .title_style(theme.title_focused)
            .style(Style::default().bg(theme.popup_bg).fg(theme.fg));
        let inner = block.inner(self.rect);
        frame.render_widget(block, self.rect);
        let total: u16 = BUTTONS.iter().map(|b| b.width() as u16).sum::<u16>() + 2 * (BUTTONS.len() as u16 - 1);
        let list_total: u16 = LIST_BUTTONS.iter().map(|b| b.width() as u16).sum::<u16>() + (LIST_BUTTONS.len() as u16 - 1);
        if inner.height < 6 || inner.width < 40.max(total + list_total + 2) {
            // Only the frame fits; drop the rects so a click after a resize cannot hit the old layout.
            self.button_rects.clear();
            self.list_button_rects.clear();
            self.list_rect = Rect::default();
            self.rows_rect = Rect::default();
            return;
        }
        let base = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let dirty = self.dirty();
        let selected_editable = self.selected().is_some_and(|e| self.editable(e));
        let locked = self.error.is_some();
        let buf = frame.buffer_mut();

        // Buttons on the last row: the window's right-aligned, the list's on the left.
        let by = inner.bottom() - 1;
        let mut bx = inner.right().saturating_sub(1 + total).max(inner.x);
        self.button_rects.clear();
        for (i, b) in BUTTONS.iter().enumerate() {
            let bw = b.width() as u16;
            let style = if self.pane == Pane::Buttons && self.button == i {
                theme.list_selected
            } else if i == 2 && !dirty {
                base.patch(theme.dim)
            } else {
                base
            };
            buf.set_string(bx, by, b, style);
            self.button_rects.push(Rect::new(bx, by, bw, 1));
            bx += bw + 2;
        }
        let mut lx = inner.x;
        self.list_button_rects.clear();
        for (i, b) in LIST_BUTTONS.iter().enumerate() {
            let bw = b.width() as u16;
            let usable = !locked && (i != 1 || selected_editable) && (i != 2 || self.selected().is_some());
            buf.set_string(lx, by, b, if usable { base } else { base.patch(theme.dim) });
            self.list_button_rects.push(Rect::new(lx, by, bw, 1));
            lx += bw + 1;
        }

        let body_y = inner.y;
        let body_h = inner.height - 2;
        let tw = (inner.width / 3).clamp(18, 30);
        self.list_rect = Rect::new(inner.x, body_y, tw, body_h);
        for yy in body_y..body_y + body_h {
            buf.set_string(inner.x + tw, yy, "│", theme.popup_border);
        }

        // The list.
        self.list.fit(self.entries.len(), body_h as usize);
        let list_style = if self.pane == Pane::List { theme.list_selected } else { theme.list_selected_unfocused };
        for (row, i) in self.list.visible().enumerate() {
            let e = &self.entries[i];
            let style = if i == self.list.selected { list_style } else { base };
            let r = Rect::new(inner.x, body_y + row as u16, tw, 1);
            buf.set_style(r, style);
            if e.touched() {
                buf.set_string(r.x, r.y, "*", style.patch(theme.git_modified));
            }
            let origin = e.task.origin.label();
            let name_w = e.task.name.width();
            let room = (tw as usize).saturating_sub(MARK_W + 1 + origin.width());
            let show_origin = name_w <= room || room >= 6;
            let name_room = if show_origin { room } else { (tw as usize).saturating_sub(MARK_W) };
            buf.set_string(r.x + MARK_W as u16, r.y, fit(&e.task.name, name_room), style);
            if show_origin {
                buf.set_string(r.right() - origin.width() as u16, r.y, origin, style.patch(theme.dim));
            }
        }

        // The form.
        let px = inner.x + tw + 2;
        let pw = (inner.width - tw - 2) as usize;
        let bottom = body_y + body_h;
        let mut top = body_y;
        self.rows_rect = Rect::default();
        if let Some(msg) = &self.error {
            for l in wrap(msg, pw, 3) {
                buf.set_string(px, top, l, base.patch(theme.error));
                top += 1;
            }
        }
        if top + 1 >= bottom {
            // The error took the room (a short window); nothing fits below it.
            return;
        }
        let Some(e) = self.entries.get(self.list.selected) else {
            buf.set_string(px, top, fit("No run configurations: [+] adds one.", pw), base.patch(theme.dim));
            return;
        };
        buf.set_string(px, top, fit(&e.task.name, pw), theme.title_focused);
        top += 2;
        let mut help = wrap(Field::ALL[self.rows.selected].help(), pw, 2);
        match (e.note(), self.error.is_none()) {
            (Some(n), _) => help.extend(wrap(n, pw, 2)),
            (None, true) => help.push("Saved to .jetl/run.toml".into()),
            _ => {}
        }
        let fields = Field::ALL.len() as u16;
        // Keep a row for every field.
        let room = bottom.saturating_sub(top).saturating_sub(fields + 1) as usize;
        help.truncate(MAX_HELP_ROWS.min(room));
        let help_h = help.len() as u16;
        let sep = u16::from(help_h > 0);
        let rows_h = bottom.saturating_sub(top).saturating_sub(help_h + sep);
        self.rows.fit(Field::ALL.len(), rows_h as usize);
        self.rows_rect = Rect::new(px, top, pw as u16, rows_h);
        let label_w = Field::ALL.iter().map(|f| f.label().width()).max().unwrap_or(0) + 2 + MARK_W;
        let label_w = label_w.min(pw / 2);
        let value_col = px + label_w as u16;
        let sel_style = if self.pane == Pane::Form { theme.list_selected } else { theme.list_selected_unfocused };
        for (row, i) in self.rows.visible().enumerate() {
            let f = Field::ALL[i];
            let ry = top + row as u16;
            let style = if i == self.rows.selected { sel_style } else { base };
            buf.set_style(Rect::new(px, ry, pw as u16, 1), style);
            let changed = e.changed(f);
            let mstyle = style.patch(theme.git_modified);
            if changed {
                buf.set_string(px, ry, "*", mstyle);
            }
            buf.set_string(px + MARK_W as u16, ry, fit(f.label(), label_w.saturating_sub(2 + MARK_W)), style);
            let (text, placeholder) = Self::field_text(e, f);
            let vstyle = if changed {
                mstyle
            } else if placeholder {
                style.patch(theme.dim)
            } else {
                style
            };
            buf.set_string(value_col, ry, fit(&text, pw.saturating_sub(label_w)), vstyle);
        }
        for (i, l) in help.iter().enumerate() {
            buf.set_string(px, bottom - help_h + i as u16, fit(l, pw), base.patch(theme.dim));
        }
    }
}

#[cfg(test)]
impl RunConfigsWindow {
    pub fn list_rect(&self) -> Rect {
        self.list_rect
    }

    pub fn rows_rect(&self) -> Rect {
        self.rows_rect
    }

    pub fn button_rects(&self) -> &[Rect] {
        &self.button_rects
    }

    pub fn list_button_rects(&self) -> &[Rect] {
        &self.list_button_rects
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::run::edit::Array;

    fn task(name: &str, command: TaskCommand, origin: Origin) -> RunTask {
        RunTask { name: name.into(), command, cwd: None, env: BTreeMap::new(), args: Vec::new(), matcher: None, origin }
    }

    fn slot(index: usize) -> Slot {
        Slot { array: Array::Configs, index }
    }

    fn sh(name: &str, cmd: &str) -> RunTask {
        task(name, TaskCommand::Shell(cmd.into()), Origin::Project)
    }

    /// `global` (config.toml), then project `a` and `b`.
    fn window() -> RunConfigsWindow {
        let global = task("global", TaskCommand::Argv(vec!["cargo".into(), "check".into()]), Origin::Config);
        RunConfigsWindow::new(String::new(), Ok(vec![(slot(0), sh("a", "echo a")), (slot(1), sh("b", "echo b"))]), vec![global])
    }

    fn key(w: &mut RunConfigsWindow, code: KeyCode) -> Option<ConfigsResult> {
        w.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(w: &mut RunConfigsWindow, c: char) -> Option<ConfigsResult> {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn text(s: &str) -> FieldValue {
        FieldValue::Text(s.into())
    }

    fn draw_at(w: &mut RunConfigsWindow, width: u16, height: u16) -> Terminal<TestBackend> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| w.render(f, f.area(), &theme)).unwrap();
        term
    }

    fn screen(term: &Terminal<TestBackend>) -> String {
        let b = term.backend().buffer();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn click(w: &mut RunConfigsWindow, x: u16, y: u16, clicks: u8) -> Option<ConfigsResult> {
        w.handle_mouse(&Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks, mods: KeyModifiers::NONE })
    }

    #[test]
    fn edits_track_updates_removals_and_additions() {
        let mut w = window();
        assert!(!w.dirty());
        // Select `a` (index 1), change its arguments.
        w.list.select(1);
        w.set_field(Field::Args, text("x 'y z'")).unwrap();
        assert!(w.dirty());
        assert!(matches!(w.edits().as_slice(), [ConfigEdit::Update(s, t)] if *s == slot(0) && t.args == ["x", "y z"]));
        // Setting them back clears the update: dirty compares values.
        w.set_field(Field::Args, text("")).unwrap();
        assert!(!w.dirty(), "{:?}", w.edits());
        w.set_field(Field::Args, text("x")).unwrap();
        // Remove `b`, add one, copy the config entry.
        w.list.select(2);
        assert_eq!(key(&mut w, KeyCode::Delete), None);
        assert_eq!(w.entries().len(), 2);
        assert_eq!(key(&mut w, KeyCode::Insert), None);
        w.list.select(0);
        assert_eq!(ctrl(&mut w, 'd'), None);
        let edits = w.edits();
        assert_eq!(edits.len(), 4, "{edits:?}");
        assert_eq!(edits[0], ConfigEdit::Remove(slot(1)));
        assert!(matches!(&edits[1], ConfigEdit::Update(s, t) if *s == slot(0) && t.args == ["x"]));
        assert!(matches!(&edits[2], ConfigEdit::Add(t) if t.name == NEW_NAME));
        assert!(
            matches!(&edits[3], ConfigEdit::Add(t) if t.name == "global (copy)" && t.command == TaskCommand::Argv(vec!["cargo".into(), "check".into()]))
        );
        // Removing a new entry leaves no trace.
        assert_eq!(key(&mut w, KeyCode::Delete), None);
        assert_eq!(w.edits().len(), 3);
    }

    #[test]
    fn launch_json_entries_are_read_only() {
        let lj = task("(gdb) Launch", TaskCommand::Shell("${workspaceFolder}/demo".into()), Origin::LaunchJson);
        let mut w = RunConfigsWindow::new(String::new(), Ok(vec![]), vec![lj]);
        let e = w.selected().unwrap();
        assert_eq!(e.note(), Some("From .vscode/launch.json: edit that file to change it"));
        assert_eq!(w.set_field(Field::Name, text("other")), Err("This configuration is read-only".into()));
        assert_eq!(
            ctrl(&mut w, 'd'),
            Some(ConfigsResult::Notify(
                "A launch.json configuration cannot be copied: its debug settings live in .vscode/launch.json".into()
            ))
        );
        assert_eq!(w.entries.len(), 1, "no copy was made");
        assert!(w.edits().is_empty() && !w.dirty());
    }

    #[test]
    fn read_only_entries_refuse_edits() {
        let mut w = window();
        assert_eq!(key(&mut w, KeyCode::Enter), None, "to the form");
        let Some(ConfigsResult::Notify(n)) = key(&mut w, KeyCode::Enter) else { panic!("expected a note") };
        assert_eq!(n, "Defined in config.toml: Copy makes an editable project copy");
        key(&mut w, KeyCode::Down);
        key(&mut w, KeyCode::Down);
        assert!(matches!(key(&mut w, KeyCode::Delete), Some(ConfigsResult::Notify(_))), "Delete cannot clear a read-only field");
        key(&mut w, KeyCode::Left);
        assert_eq!(key(&mut w, KeyCode::Delete), Some(ConfigsResult::Notify("Only project configurations can be removed".into())));
        assert!(w.set_field(Field::Args, text("x")).is_err());
        assert!(!w.dirty());
        assert_eq!(ctrl(&mut w, 'd'), None);
        let e = w.selected().unwrap();
        assert_eq!((e.task.name.as_str(), e.task.origin, e.new), ("global (copy)", Origin::Project, true));
        assert_eq!(w.edits().len(), 1);
        // The copy is editable.
        w.set_field(Field::Args, text("x")).unwrap();
    }

    #[test]
    fn field_parsing() {
        let mut w = window();
        w.list.select(1);
        w.set_field(Field::Args, text("--release 'a b'")).unwrap();
        assert_eq!(w.selected().unwrap().task.args, ["--release", "a b"]);
        w.set_field(Field::Env, text("A=1 B='x y'")).unwrap();
        let env = &w.selected().unwrap().task.env;
        assert_eq!((env["A"].as_str(), env["B"].as_str()), ("1", "x y"));
        assert_eq!(w.set_field(Field::Env, text("oops")), Err("Not KEY=value: oops".into()));
        assert_eq!(w.set_field(Field::Env, text("=v")), Err("Not KEY=value: =v".into()));
        assert_eq!(w.selected().unwrap().task.env.len(), 2, "an error leaves the entry unchanged");
        assert!(w.set_field(Field::Args, text("'x")).is_err());
        assert_eq!(w.selected().unwrap().task.args, ["--release", "a b"]);
        assert!(w.set_field(Field::Name, text("  ")).is_err());
        assert_eq!(w.selected().unwrap().task.name, "a");
        w.set_field(Field::Name, text(" c ")).unwrap();
        assert_eq!(w.renames(), [("a".to_string(), "c".to_string())]);
        assert!(w.set_field(Field::Command, text("")).is_err());
        w.set_field(Field::Cwd, text("web")).unwrap();
        assert_eq!(w.selected().unwrap().task.cwd.as_deref(), Some("web"));
        w.set_field(Field::Cwd, text("")).unwrap();
        assert_eq!(w.selected().unwrap().task.cwd, None);
        w.set_field(Field::Matcher, FieldValue::Matcher(Some(MatcherKind::Plain))).unwrap();
        assert_eq!(w.selected().unwrap().task.matcher, Some(MatcherKind::Plain));
        // An unchanged command text keeps a word list.
        let mut w = RunConfigsWindow::new(
            String::new(),
            Ok(vec![(slot(0), task("t", TaskCommand::Argv(vec!["cargo".into(), "test".into()]), Origin::Project))]),
            vec![],
        );
        w.set_field(Field::Command, text("cargo test")).unwrap();
        assert_eq!(w.selected().unwrap().task.command, TaskCommand::Argv(vec!["cargo".into(), "test".into()]));
        assert!(!w.dirty());
        w.set_field(Field::Command, text("cargo test -q")).unwrap();
        assert_eq!(w.selected().unwrap().task.command, TaskCommand::Shell("cargo test -q".into()));
    }

    #[test]
    fn validate_rejects_duplicates_and_empty_commands() {
        let mut w = window();
        assert_eq!(w.validate(), Ok(()));
        w.list.select(1);
        w.set_field(Field::Name, text("b")).unwrap();
        assert_eq!(w.validate(), Err("Two configurations are named 'b'".into()));
        w.set_field(Field::Name, text("global")).unwrap();
        assert_eq!(w.validate(), Ok(()), "a config.toml entry may share a name with a project one");
        w.set_field(Field::Name, text("a")).unwrap();
        assert_eq!(w.validate(), Ok(()));
        key(&mut w, KeyCode::Insert);
        assert_eq!(w.validate(), Err("Configuration 'New configuration' has no command".into()));
        key(&mut w, KeyCode::Insert);
        assert_eq!(w.selected().unwrap().task.name, "New configuration 2", "names stay unique");
        w.set_field(Field::Command, text("make")).unwrap();
        w.list.select(w.list.selected - 1);
        w.set_field(Field::Command, text("make")).unwrap();
        assert_eq!(w.validate(), Ok(()));
    }

    #[test]
    fn invalid_project_file_is_read_only() {
        let mut w = RunConfigsWindow::new(
            "oops".into(),
            Err(".jetl/run.toml: expected an equals sign".into()),
            vec![task("global", TaskCommand::Shell("x".into()), Origin::Config)],
        );
        let screen_text = screen(&draw_at(&mut w, 100, 30));
        assert!(screen_text.contains(".jetl/run.toml: expected an equals sign"), "{screen_text}");
        assert!(screen_text.contains("global"));
        assert!(w.set_field(Field::Args, text("x")).is_err());
        assert!(matches!(key(&mut w, KeyCode::Insert), Some(ConfigsResult::Notify(_))));
        assert!(matches!(ctrl(&mut w, 'd'), Some(ConfigsResult::Notify(_))));
        assert!(w.edits().is_empty());
        assert_eq!(ctrl(&mut w, 's'), None, "Apply is inert");
        assert_eq!(key(&mut w, KeyCode::Esc), Some(ConfigsResult::Close));
        // The buttons are as inert.
        let apply = w.button_rects()[2];
        assert_eq!(click(&mut w, apply.x, apply.y, 1), None);
    }

    #[test]
    fn invalid_project_file_renders_in_a_small_terminal() {
        let msg = ".jetl/run.toml: TOML parse error at line 3, column 9, expected an equals sign after the key name here";
        for height in 6..=14 {
            for width in [40, 50, 60, 80] {
                let mut w = RunConfigsWindow::new(
                    String::new(),
                    Err(msg.into()),
                    vec![task("global", TaskCommand::Shell("x".into()), Origin::Config)],
                );
                draw_at(&mut w, width, height);
            }
        }
    }

    #[test]
    fn buttons_and_keys_follow_the_dirty_state() {
        let mut w = window();
        w.list.select(1);
        key(&mut w, KeyCode::Tab);
        key(&mut w, KeyCode::Tab);
        assert_eq!(key(&mut w, KeyCode::Enter), Some(ConfigsResult::Close), "OK of a clean window closes");
        w.set_field(Field::Args, text("x")).unwrap();
        assert_eq!(key(&mut w, KeyCode::Enter), Some(ConfigsResult::Apply { close: true }));
        key(&mut w, KeyCode::Right);
        assert_eq!(key(&mut w, KeyCode::Enter), Some(ConfigsResult::ConfirmDiscard));
        key(&mut w, KeyCode::Right);
        assert_eq!(key(&mut w, KeyCode::Enter), Some(ConfigsResult::Apply { close: false }));
        assert_eq!(ctrl(&mut w, 's'), Some(ConfigsResult::Apply { close: false }));
        assert_eq!(key(&mut w, KeyCode::Esc), Some(ConfigsResult::ConfirmDiscard));
    }

    #[test]
    fn form_keys_pick_and_clear_fields() {
        let mut w = window();
        w.list.select(1);
        key(&mut w, KeyCode::Enter);
        assert_eq!(key(&mut w, KeyCode::Enter), Some(ConfigsResult::EditField(Field::Name)));
        key(&mut w, KeyCode::Down);
        key(&mut w, KeyCode::Down);
        assert_eq!(key(&mut w, KeyCode::Char(' ')), Some(ConfigsResult::EditField(Field::Args)));
        w.set_field(Field::Args, text("x")).unwrap();
        assert_eq!(key(&mut w, KeyCode::Delete), None);
        assert!(w.selected().unwrap().task.args.is_empty());
        key(&mut w, KeyCode::End);
        w.set_field(Field::Matcher, FieldValue::Matcher(Some(MatcherKind::Plain))).unwrap();
        key(&mut w, KeyCode::Delete);
        assert_eq!(w.selected().unwrap().task.matcher, None);
        key(&mut w, KeyCode::Home);
        key(&mut w, KeyCode::Delete);
        assert_eq!(w.selected().unwrap().task.name, "a", "Name is not cleared");
    }

    #[test]
    fn mouse_selects_and_double_click_edits() {
        let mut w = window();
        draw_at(&mut w, 100, 30);
        let l = w.list_rect();
        assert_eq!(click(&mut w, l.x + 1, l.y + 1, 1), None);
        assert_eq!(w.selected().unwrap().task.name, "a");
        let r = w.rows_rect();
        assert_eq!(click(&mut w, r.x + 2, r.y + 2, 1), None);
        assert_eq!(w.rows.selected, 2);
        assert_eq!(click(&mut w, r.x + 2, r.y + 2, 2), Some(ConfigsResult::EditField(Field::Args)));
        // [Copy] and [−].
        let b = w.list_button_rects().to_vec();
        assert_eq!(click(&mut w, b[2].x, b[2].y, 1), None);
        assert_eq!(w.selected().unwrap().task.name, "a (copy)");
        assert_eq!(click(&mut w, b[1].x, b[1].y, 1), None);
        assert_eq!(w.entries().len(), 3);
        assert_eq!(click(&mut w, b[0].x, b[0].y, 1), None);
        assert_eq!(w.selected().unwrap().task.name, NEW_NAME);
        // The list entry of a config.toml entry cannot be removed with [−].
        w.list.select(0);
        assert!(matches!(click(&mut w, b[1].x, b[1].y, 1), Some(ConfigsResult::Notify(_))));
    }

    #[test]
    fn renders_list_form_and_buttons() {
        let mut w = window();
        w.list.select(1);
        w.set_field(Field::Cwd, text("web")).unwrap();
        let s = screen(&draw_at(&mut w, 80, 24));
        for want in [
            "Run Configurations",
            "global",
            "config.toml",
            ".jetl/run.toml",
            "Working directory",
            "(none)",
            "web",
            "Environment",
            "auto (problems)",
            "Saved to .jetl/run.toml",
            "[ OK ]",
            "[ Cancel ]",
            "[ Apply ]",
            "[+]",
            "[−]",
            "[Copy]",
        ] {
            assert!(s.contains(want), "missing {want:?}:\n{s}");
        }
        assert!(s.contains("* a"), "the changed entry is marked:\n{s}");
        assert!(s.contains("* Working directory"), "the changed field is marked:\n{s}");
    }

    #[test]
    fn a_frame_only_redraw_drops_click_targets() {
        let mut w = window();
        draw_at(&mut w, 100, 30);
        assert!(!w.button_rects().is_empty() && !w.list_button_rects().is_empty());
        assert!(w.list_rect().width > 0 && w.rows_rect().width > 0);
        let ok = w.button_rects()[0];
        let plus = w.list_button_rects()[0];
        let row = (w.rows_rect().x + 2, w.rows_rect().y);
        let list = (w.list_rect().x + 1, w.list_rect().y + 1);
        draw_at(&mut w, 30, 24);
        assert!(w.button_rects().is_empty() && w.list_button_rects().is_empty());
        assert_eq!((w.list_rect(), w.rows_rect()), (Rect::default(), Rect::default()));
        assert_eq!(click(&mut w, ok.x, ok.y, 1), None);
        assert_eq!(click(&mut w, plus.x, plus.y, 1), None);
        assert_eq!(click(&mut w, row.0, row.1, 2), None);
        assert_eq!(click(&mut w, list.0, list.1, 1), None);
        assert_eq!(w.entries().len(), 3);
    }
}
