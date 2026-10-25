//! Tasks tool window (bottom by default, View > Tasks): the yman tracker's
//! tasks in a table with the selected task's details beside it. Open tasks
//! are listed by default (Alt+C shows closed ones, dim); typing filters by
//! id, title, tags and assignee, Alt+T picks a tag, Alt+A an assignee.
//! Tab or Enter moves to the details, which scroll; Alt+J (or a click on a
//! related id) jumps to a related task.
//!
//! The panel holds view state only: the data stays in `YmanService`. Rows
//! are rebuilt when its `revision` moved, keeping the selection by task id.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{LoadedTask, Snapshot, TaskRow, TrackerState, YmanService};
use crate::actions::{Action, TasksAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::PanelId;
use crate::ui::hover::{self, Highlight, Kind};
use crate::ui::menu::{ContextMenu, MenuItem};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::util::text::{fit, wrap_phrases};
use crate::workspace::SessionTasks;

/// Below this body width only the focused area is shown.
const NARROW: u16 = 90;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Area {
    #[default]
    List,
    Details,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Closed,
    Tag,
    Assignee,
    Related,
    Refresh,
}

/// A run of a details line; `id` makes it a clickable task id.
struct Seg {
    text: String,
    style: Style,
    id: Option<String>,
}

type DetailLine = Vec<Seg>;

fn seg(text: impl Into<String>, style: Style) -> Seg {
    Seg { text: text.into(), style, id: None }
}

#[derive(Debug, Default)]
pub struct TasksPanel {
    pub list: ListState,
    /// Indices into `Snapshot::tasks` that pass the filters.
    rows: Vec<usize>,
    /// The `YmanService::revision` the rows came from.
    built: Option<u64>,
    filter: String,
    tag: Option<String>,
    /// `Some("-")` is unassigned.
    assignee: Option<String>,
    show_closed: bool,
    area: Area,
    /// The selected task's id, updated on every selection change.
    keep: Option<String>,
    /// The id last passed to `YmanService::load_task`.
    pub(crate) requested: Option<String>,
    /// The details' first line.
    scroll: usize,
    scroll_max: usize,
    details_page: usize,
    list_rect: Rect,
    details_rect: Rect,
    hits: Vec<(Btn, Rect)>,
    related_hits: Vec<(Rect, String)>,
}

/// Break `text` at spaces into lines at most `width` cells wide (a longer word is left to `fit`).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.width() + 1 + word.width() <= width => {
                l.push(' ');
                l.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

fn snapshot(yman: &YmanService) -> Option<&Snapshot> {
    match &yman.state {
        TrackerState::Ready(s) => Some(s),
        _ => None,
    }
}

/// `2026-10-25T16:06:18Z` as `2026-10-25 16:06`; any other shape as it is.
fn short_time(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 16 && b[10] == b'T' && s.is_char_boundary(16) { format!("{} {}", &s[..10], &s[11..16]) } else { s.to_string() }
}

impl TasksPanel {
    /// The selected task's id (none without rows).
    pub fn selected_id(&self) -> Option<&str> {
        self.keep.as_deref().filter(|_| !self.rows.is_empty())
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn session_state(&self) -> SessionTasks {
        SessionTasks {
            filter: self.filter.clone(),
            tag: self.tag.clone(),
            assignee: self.assignee.clone(),
            show_closed: self.show_closed,
            selected: self.keep.clone(),
            ..Default::default()
        }
    }

    pub fn restore(&mut self, s: &SessionTasks) {
        self.filter = s.filter.clone();
        self.tag = s.tag.clone();
        self.assignee = s.assignee.clone();
        self.show_closed = s.show_closed;
        self.keep = s.selected.clone();
        self.built = None;
        self.area = Area::List;
    }

    fn passes(&self, t: &TaskRow, open: bool) -> bool {
        if !(self.show_closed || open) {
            return false;
        }
        if let Some(tag) = &self.tag
            && !t.tags.iter().any(|x| x.eq_ignore_ascii_case(tag))
        {
            return false;
        }
        if let Some(who) = &self.assignee {
            let name = t.assignee.as_deref().map(str::trim).unwrap_or("");
            let ok = if who == "-" { name.is_empty() || name == "-" } else { name.eq_ignore_ascii_case(who) };
            if !ok {
                return false;
            }
        }
        if self.filter.trim().is_empty() {
            return true;
        }
        let hay = format!("{} {} {} {}", t.id, t.title, t.tags.join(" "), t.assignee.as_deref().unwrap_or("")).to_lowercase();
        self.filter.to_lowercase().split_whitespace().all(|term| hay.contains(term))
    }

    fn set_keep(&mut self, id: Option<String>) {
        if self.keep != id {
            self.scroll = 0;
            self.keep = id;
        }
    }

    /// The selected row's id as the snapshot has it.
    fn remember(&mut self, yman: &YmanService) {
        let id = snapshot(yman).and_then(|s| self.rows.get(self.list.selected).map(|&i| s.tasks[i].id.clone()));
        if id.is_some() {
            self.set_keep(id);
        }
    }

    /// Rebuild the rows when the service changed, keeping the selected id.
    pub fn sync(&mut self, yman: &YmanService) {
        if self.built == Some(yman.revision) {
            return;
        }
        self.built = Some(yman.revision);
        let Some(snap) = snapshot(yman) else {
            // The selected id stays: a reload finds the task again.
            self.rows.clear();
            self.list.set_len(0);
            return;
        };
        self.rows = snap.tasks.iter().enumerate().filter(|(_, t)| self.passes(t, snap.open.contains(&t.id))).map(|(i, _)| i).collect();
        self.list.set_len(self.rows.len());
        if let Some(k) = &self.keep
            && let Some(at) = self.rows.iter().position(|&i| snap.tasks[i].id == *k)
        {
            self.list.select(at);
        }
        let id = self.rows.get(self.list.selected).map(|&i| snap.tasks[i].id.clone());
        self.set_keep(id);
    }

    fn refilter(&mut self, yman: &YmanService) {
        self.built = None;
        self.sync(yman);
    }

    /// Typing filters the list while it has focus; Backspace deletes, Esc
    /// clears. Other keys go to the keymap.
    pub fn handle_key(&mut self, key: KeyEvent, yman: &YmanService) -> bool {
        if self.area != Area::List || key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return false;
        }
        match key.code {
            KeyCode::Char(' ') if self.filter.is_empty() => return false,
            KeyCode::Char(c) => self.filter.push(c),
            KeyCode::Backspace if !self.filter.is_empty() => {
                self.filter.pop();
            }
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            _ => return false,
        }
        self.refilter(yman);
        true
    }

    pub fn set_tag(&mut self, v: Option<String>, yman: &YmanService) {
        self.tag = v;
        self.refilter(yman);
    }

    pub fn set_assignee(&mut self, v: Option<String>, yman: &YmanService) {
        self.assignee = v;
        self.refilter(yman);
    }

    /// Select `id`, clearing the filters and showing closed tasks when that
    /// is needed to list it.
    pub fn jump(&mut self, id: &str, yman: &YmanService, cx: &mut Ctx) -> bool {
        self.sync(yman);
        let found = snapshot(yman).and_then(|s| s.tasks.iter().find(|t| t.id == id).map(|t| (t, s.open.contains(&t.id))));
        let Some((task, open)) = found else {
            cx.notify(Level::Info, format!("Task {id} is not in the list"));
            return false;
        };
        if !self.passes(task, open) {
            self.filter.clear();
            self.tag = None;
            self.assignee = None;
            if !open {
                self.show_closed = true;
            }
        }
        self.set_keep(Some(id.to_string()));
        self.area = Area::List;
        self.refilter(yman);
        true
    }

    fn dialog(&self, cx: &mut Ctx, id: DialogId, title: &str, labels: Vec<String>) {
        cx.effects.push(Effect::Dialog(Dialog::choice(id, title, "", labels)));
    }

    fn filter_tag(&self, yman: &YmanService, cx: &mut Ctx) {
        let Some(snap) = snapshot(yman) else { return cx.notify(Level::Info, "The task list is not loaded") };
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &snap.tasks {
            for tag in &t.tags {
                *counts.entry(tag).or_default() += 1;
            }
        }
        let mut values = vec![String::new()];
        let mut labels = vec!["Any tag".to_string()];
        for (tag, n) in counts {
            values.push(tag.to_string());
            labels.push(format!("{tag} ({n})"));
        }
        self.dialog(cx, DialogId::TasksTag(values), "Filter by Tag", labels);
    }

    fn filter_assignee(&self, yman: &YmanService, cx: &mut Ctx) {
        let Some(snap) = snapshot(yman) else { return cx.notify(Level::Info, "The task list is not loaded") };
        let names: std::collections::BTreeSet<&str> =
            snap.tasks.iter().filter_map(|t| t.assignee.as_deref()).map(str::trim).filter(|n| !n.is_empty() && *n != "-").collect();
        let mut values = vec![String::new(), "-".to_string()];
        let mut labels = vec!["Anyone".to_string(), "Unassigned".to_string()];
        for n in names {
            values.push(n.to_string());
            labels.push(n.to_string());
        }
        self.dialog(cx, DialogId::TasksAssignee(values), "Filter by Assignee", labels);
    }

    fn jump_related(&self, yman: &YmanService, cx: &mut Ctx) {
        let Some(LoadedTask { id, result }) = &yman.task else {
            return cx.notify(Level::Info, "The task's details are still loading");
        };
        let Ok(t) = result else { return cx.notify(Level::Info, "The task's details are still loading") };
        if self.selected_id() != Some(id.as_str()) {
            return cx.notify(Level::Info, "The task's details are still loading");
        }
        let title = |id: &str| snapshot(yman).and_then(|s| s.tasks.iter().find(|x| x.id == id)).map(|x| x.title.as_str()).unwrap_or("");
        let mut values: Vec<String> = Vec::new();
        let mut labels = Vec::new();
        for (arrow, ids) in [("→", &t.related), ("←", &t.related_by)] {
            for rel in ids {
                if !values.contains(rel) {
                    labels.push(format!("{arrow} {rel} {}", title(rel)).trim_end().to_string());
                    values.push(rel.clone());
                }
            }
        }
        if values.is_empty() {
            return cx.notify(Level::Info, "The task has no related tasks");
        }
        self.dialog(cx, DialogId::TasksJump(values), "Jump to Related Task", labels);
    }

    fn context_menu(&self, x: u16, y: u16, cx: &mut Ctx) {
        let selected = self.selected_id().is_some();
        let closed = if self.show_closed { "Hide Closed Tasks" } else { "Show Closed Tasks" };
        let items = vec![
            MenuItem::new("Show Details", Action::Tasks(TasksAction::Details)).enabled(selected),
            MenuItem::new("Jump to Related Task…", Action::Tasks(TasksAction::JumpRelated)),
            MenuItem::separator(),
            MenuItem::new(closed, Action::Tasks(TasksAction::ToggleClosed)),
            MenuItem::new("Filter by Tag…", Action::Tasks(TasksAction::FilterTag)),
            MenuItem::new("Filter by Assignee…", Action::Tasks(TasksAction::FilterAssignee)),
            MenuItem::separator(),
            MenuItem::new("Refresh", Action::Tasks(TasksAction::Refresh)),
        ];
        cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, x, y)));
    }

    fn scroll_details(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).clamp(0, self.scroll_max as isize) as usize;
    }

    pub fn handle_action(&mut self, a: &TasksAction, yman: &YmanService, cx: &mut Ctx) {
        use TasksAction as T;
        self.sync(yman);
        let in_details = self.area == Area::Details;
        let page = self.list.viewport.max(1);
        let dpage = self.details_page.saturating_sub(1).max(1) as isize;
        match a {
            T::Up if in_details => self.scroll_details(-1),
            T::Down if in_details => self.scroll_details(1),
            T::PageUp if in_details => self.scroll_details(-dpage),
            T::PageDown if in_details => self.scroll_details(dpage),
            T::Home if in_details => self.scroll = 0,
            T::End if in_details => self.scroll = self.scroll_max,
            T::Up => self.list.up(1),
            T::Down => self.list.down(1),
            T::PageUp => self.list.up(page),
            T::PageDown => self.list.down(page),
            T::Home => self.list.home(),
            T::End => self.list.end(),
            T::NextSection => {
                self.area = if in_details || self.selected_id().is_none() { Area::List } else { Area::Details };
            }
            T::Details => {
                if self.selected_id().is_some() {
                    self.area = Area::Details;
                }
            }
            T::ToggleClosed => {
                self.show_closed = !self.show_closed;
                self.refilter(yman);
            }
            T::FilterTag => self.filter_tag(yman, cx),
            T::FilterAssignee => self.filter_assignee(yman, cx),
            T::JumpRelated => self.jump_related(yman, cx),
            T::ContextMenu => {
                let row = self.list.selected.saturating_sub(self.list.offset) as u16;
                self.context_menu(self.list_rect.x + 2, self.list_rect.y + row, cx);
            }
            T::Escape => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refilter(yman);
                } else if in_details {
                    self.area = Area::List;
                } else {
                    cx.focus(PanelId::Editor);
                }
            }
            // The App reloads the tracker.
            T::Refresh => {}
        }
        if !in_details {
            self.remember(yman);
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse, yman: &YmanService, cx: &mut Ctx) -> bool {
        self.sync(yman);
        let at = ratatui::layout::Position::new(m.x, m.y);
        match m.kind {
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                if button == MouseButton::Left
                    && let Some((b, _)) = self.hits.iter().find(|(_, r)| r.contains(at))
                {
                    let a = match b {
                        Btn::Closed => TasksAction::ToggleClosed,
                        Btn::Tag => TasksAction::FilterTag,
                        Btn::Assignee => TasksAction::FilterAssignee,
                        Btn::Related => TasksAction::JumpRelated,
                        Btn::Refresh => TasksAction::Refresh,
                    };
                    cx.effects.push(Effect::Perform(Action::Tasks(a)));
                    return true;
                }
                if let Some(i) = self.list.hit(self.list_rect, m.x, m.y) {
                    self.list.select(i);
                    self.remember(yman);
                    self.area = Area::List;
                    if button == MouseButton::Right {
                        self.context_menu(m.x, m.y, cx);
                    } else if m.clicks >= 2 {
                        self.area = Area::Details;
                    }
                    return true;
                }
                if button == MouseButton::Left && self.details_rect.contains(at) {
                    self.area = Area::Details;
                    if let Some((_, id)) = self.related_hits.iter().find(|(r, _)| r.contains(at)) {
                        let id = id.clone();
                        self.jump(&id, yman, cx);
                    }
                    return true;
                }
                false
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if self.details_rect.contains(at) {
                    self.scroll_details(if down { 3 } else { -3 });
                } else if self.list_rect.contains(at) {
                    self.list.scroll(if down { 3 } else { -3 });
                } else {
                    return false;
                }
                true
            }
            _ => false,
        }
    }

    fn title(&self, yman: &YmanService) -> String {
        let Some(snap) = snapshot(yman) else { return " Tasks ".to_string() };
        let total = if self.show_closed { snap.tasks.len() } else { snap.tasks.iter().filter(|t| snap.open.contains(&t.id)).count() };
        if self.rows.len() != total {
            format!(" Tasks: {} of {total} ", self.rows.len())
        } else if self.show_closed {
            format!(" Tasks: {total} ")
        } else {
            format!(" Tasks: {total} open ")
        }
    }

    fn filters_active(&self) -> bool {
        !self.filter.is_empty() || self.tag.is_some() || self.assignee.is_some()
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, yman: &YmanService) {
        self.sync(yman);
        let title = self.title(yman);
        let block = tool_window_block(&title, focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        self.hits.clear();
        self.related_hits.clear();
        self.list_rect = Rect::default();
        self.details_rect = Rect::default();
        if r.height < 2 || r.width < 10 {
            return;
        }
        let buf = frame.buffer_mut();
        self.render_toolbar(buf, Rect::new(r.x, r.y, r.width, 1), focused, theme, yman);
        let body = Rect::new(r.x, r.y + 1, r.width, r.height - 1);
        let (list, details) = if body.width < NARROW {
            if focused && self.area == Area::Details { (Rect::default(), body) } else { (body, Rect::default()) }
        } else {
            let lw = (body.width * 3 / 5).max(50).min(body.width - 12);
            let rest = body.width - lw;
            buf.set_style(Rect::new(body.x + lw, body.y, 1, body.height), Style::default().bg(theme.bg));
            for y in body.y..body.bottom() {
                buf.set_stringn(body.x + lw, y, "│", 1, theme.splitter);
            }
            (Rect::new(body.x, body.y, lw, body.height), Rect::new(body.x + lw + 1, body.y, rest - 1, body.height))
        };
        self.render_list(buf, list, focused, theme, yman);
        self.render_details(buf, details, focused, theme, yman);
    }

    fn render_toolbar(&mut self, buf: &mut Buffer, r: Rect, focused: bool, theme: &Theme, yman: &YmanService) {
        buf.set_style(r, Style::default().bg(theme.panel_bg));
        let closed = if self.show_closed { "[Closed: shown]" } else { "[Closed: hidden]" }.to_string();
        let tag = format!("[Tag: {}]", self.tag.as_deref().unwrap_or("any"));
        let who = match self.assignee.as_deref() {
            None => "any",
            Some("-") => "none",
            Some(n) => n,
        };
        let who = format!("[Assignee: {who}]");
        let details_loaded = matches!(&yman.task, Some(LoadedTask { id, result: Ok(_) }) if self.selected_id() == Some(id.as_str()));
        let buttons = [
            (Btn::Closed, closed, true),
            (Btn::Tag, tag, true),
            (Btn::Assignee, who, true),
            (Btn::Related, "[Related…]".to_string(), details_loaded),
            (Btn::Refresh, "[Refresh]".to_string(), true),
        ];
        // The typed filter wins over trailing buttons: they drop from the right to make room.
        let typed = (!self.filter.is_empty()).then(|| format!("filter: {} ", self.filter));
        // The text plus the gap cell after the last button.
        let reserved = typed.as_ref().map_or(0, |t| (t.width() as u16 + 1).min(r.width));
        let limit = r.right() - reserved;
        let mut x = r.x;
        for (b, label, on) in buttons {
            // Whole buttons only; the keys and the menu still work.
            if x + label.width() as u16 > limit {
                break;
            }
            let style = if on { theme.title } else { theme.dim }.bg(theme.panel_bg);
            let w = label.width() as u16;
            buf.set_stringn(x, r.y, &label, w as usize, style);
            self.hits.push((b, Rect::new(x, r.y, w, 1)));
            x += w + 1;
        }
        let (text, style) = if let Some(t) = typed {
            (t, theme.status_accent)
        } else if focused && self.area == Area::List && !self.rows.is_empty() {
            ("type to filter ".to_string(), theme.dim)
        } else {
            return;
        };
        let w = (text.width() as u16).min(r.width);
        if r.right() >= x + w {
            buf.set_stringn(r.right() - w, r.y, &text, w as usize, style.bg(theme.panel_bg));
        }
    }

    fn empty_text(&self, yman: &YmanService) -> Vec<String> {
        match &yman.state {
            TrackerState::Idle | TrackerState::Loading => vec!["Loading tasks…".into()],
            TrackerState::Unavailable(e) => vec![e.message().lines().next().unwrap_or("").to_string(), "F5 retries".into()],
            TrackerState::Ready(s) if s.tasks.is_empty() => vec!["No tasks yet".into()],
            TrackerState::Ready(s) => {
                if !self.filters_active() && !self.show_closed && s.open.is_empty() {
                    vec!["No open tasks (Alt+C shows closed ones)".into()]
                } else {
                    let hint = if !self.filter.is_empty() {
                        "Esc clears the typed text"
                    } else if self.tag.is_some() && self.assignee.is_some() {
                        "Alt+T and Alt+A change the tag and assignee filters"
                    } else if self.tag.is_some() {
                        "Alt+T changes the tag filter"
                    } else {
                        "Alt+A changes the assignee filter"
                    };
                    vec![format!("No task matches the filter ({hint})")]
                }
            }
        }
    }

    fn render_list(&mut self, buf: &mut Buffer, r: Rect, focused: bool, theme: &Theme, yman: &YmanService) {
        if r.width == 0 || r.height == 0 {
            return;
        }
        let Some(snap) = snapshot(yman).filter(|_| !self.rows.is_empty()) else {
            let room = r.width.saturating_sub(1) as usize;
            let lines = self.empty_text(yman).iter().flat_map(|l| wrap(l, room)).collect::<Vec<_>>();
            for (i, line) in lines.iter().take(r.height as usize).enumerate() {
                buf.set_stringn(r.x + 1, r.y + i as u16, fit(line, room), room, theme.dim);
            }
            return;
        };
        let rows_area = Rect::new(r.x, r.y + 1, r.width, r.height - 1);
        self.list.fit(self.rows.len(), rows_area.height as usize);
        let shown: Vec<(usize, &TaskRow)> = self.list.visible().map(|i| (i, &snap.tasks[self.rows[i]])).collect();
        let wide = |f: fn(&TaskRow) -> usize, min: usize, max: usize| shown.iter().map(|(_, t)| f(t)).max().unwrap_or(0).clamp(min, max);
        let idw = wide(|t| t.id.width(), 2, 8);
        let stw = wide(|t| t.status.width(), 6, 10);
        let show_tags = r.width >= 68;
        let show_who = r.width >= 50;
        let tagw = if show_tags { wide(|t| t.tags.join(", ").width(), 4, 24) } else { 0 };
        let whow = if show_who { wide(|t| t.assignee.as_deref().unwrap_or("").width(), 8, 12) } else { 0 };
        let fixed = 1 + 1 + 1 + idw + 1 + stw + 1 + if show_tags { 1 + tagw } else { 0 } + if show_who { 1 + whow } else { 0 } + 1;
        let titlew = (r.width as usize).saturating_sub(fixed);
        let draw = |buf: &mut Buffer, y: u16, cells: [&str; 6], style: Style| {
            let mut x = r.x + 1;
            let widths = [1, idw, stw, titlew, tagw, whow];
            for (k, (text, w)) in cells.iter().zip(widths).enumerate() {
                if (k == 4 && !show_tags) || (k == 5 && !show_who) || x >= r.right() {
                    continue;
                }
                let room = w.min((r.right() - x) as usize);
                buf.set_stringn(x, y, fit(text, room), room, style);
                x += w as u16 + 1;
            }
        };
        let dim = Style::default().fg(theme.fg).patch(theme.dim);
        draw(buf, r.y, ["P", "ID", "Status", "Title", "Tags", "Assignee"], dim);
        self.list_rect = rows_area;
        let focused = focused && self.area == Area::List;
        for (vi, (i, t)) in shown.iter().enumerate() {
            let y = rows_area.y + vi as u16;
            let selected = *i == self.list.selected;
            let closed = !snap.open.contains(&t.id);
            let mut base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            if closed {
                base = base.patch(theme.dim);
            }
            let (p, tags) = (t.priority.to_string(), t.tags.join(", "));
            draw(buf, y, [&p, &t.id, &t.status, &t.title, &tags, t.assignee.as_deref().unwrap_or("")], base);
        }
    }

    fn render_details(&mut self, buf: &mut Buffer, r: Rect, focused: bool, theme: &Theme, yman: &YmanService) {
        if r.width == 0 || r.height == 0 {
            return;
        }
        self.details_rect = r;
        let msg = |buf: &mut Buffer, text: &str| {
            buf.set_stringn(r.x + 1, r.y, text, r.width.saturating_sub(1) as usize, theme.dim);
        };
        let Some(sel) = self.selected_id().map(str::to_string) else { return msg(buf, "No task selected") };
        let t = match &yman.task {
            Some(LoadedTask { id, result }) if *id == sel => match result {
                Ok(t) => t,
                Err(e) => return msg(buf, &e.message()),
            },
            _ => return msg(buf, "Loading…"),
        };
        let width = (r.width as usize).saturating_sub(1);
        let lines = detail_lines(t, width, theme);
        self.scroll_max = lines.len().saturating_sub(r.height as usize);
        self.scroll = self.scroll.min(self.scroll_max);
        self.details_page = r.height as usize;
        if focused && self.area == Area::Details {
            buf.set_style(Rect::new(r.x, r.y, 1, r.height), Style::default().bg(theme.panel_bg));
        }
        let right = r.right();
        for (vi, line) in lines.iter().skip(self.scroll).take(r.height as usize).enumerate() {
            let y = r.y + vi as u16;
            let mut x = r.x + 1;
            for s in line {
                if x >= right {
                    break;
                }
                let room = (right - x) as usize;
                let w = s.text.width();
                let (nx, _) = buf.set_stringn(x, y, &s.text, room, s.style);
                if let Some(id) = &s.id
                    && w <= room
                {
                    self.related_hits.push((Rect::new(x, y, w as u16, 1), id.clone()));
                }
                x = nx;
            }
        }
    }
}

/// The details as lines of runs, wrapped to `width` cells.
fn detail_lines(t: &super::TaskDetails, width: usize, theme: &Theme) -> Vec<DetailLine> {
    let fg = Style::default().fg(theme.fg);
    let dim = fg.patch(theme.dim);
    let bold = fg.add_modifier(Modifier::BOLD);
    let mut out: Vec<DetailLine> = Vec::new();
    let wrapped = |out: &mut Vec<DetailLine>, text: &str, style: Style, indent: usize| {
        for l in wrap_phrases(&[text], width.saturating_sub(indent).max(1)) {
            out.push(vec![seg(format!("{}{l}", " ".repeat(indent)), style)]);
        }
    };
    wrapped(&mut out, &format!("#{} {}", t.id, t.title), bold, 0);
    let who = t.assignee.as_deref().map(str::trim).filter(|a| !a.is_empty() && *a != "-").unwrap_or("unassigned");
    let mut meta = vec![t.status.clone(), format!("P{}", t.priority), who.to_string()];
    if !t.tags.is_empty() {
        meta.push(t.tags.join(", "));
    }
    wrapped(&mut out, &meta.join(" · "), dim, 0);
    let link = fg.fg(theme.status_accent.fg.unwrap_or(theme.fg)).add_modifier(Modifier::UNDERLINED);
    for (label, ids) in [("Related: ", &t.related), ("Related by: ", &t.related_by)] {
        if ids.is_empty() {
            continue;
        }
        let mut line = vec![seg(label, fg)];
        for (k, id) in ids.iter().enumerate() {
            if k > 0 {
                line.push(seg(", ", fg));
            }
            line.push(Seg { text: id.clone(), style: link, id: Some(id.clone()) });
        }
        out.push(line);
    }
    wrapped(&mut out, &format!("Created {} · Updated {}", short_time(&t.created), short_time(&t.updated)), dim, 0);
    out.push(Vec::new());
    let body = hover::markdown(&t.body, Highlight { theme, fallback: None });
    for l in hover::wrap_lines(&body, width.max(1)) {
        let style = match l.kind {
            Kind::Text => fg,
            Kind::Code => Style::default().bg(theme.bg).fg(theme.fg),
            Kind::Heading => bold,
            Kind::Rule => dim,
        };
        out.push(doc_segs(&l, style));
    }
    if !t.attachments.is_empty() {
        out.push(Vec::new());
        out.push(vec![seg("Attachments", bold)]);
        for a in &t.attachments {
            let mut line = vec![seg(format!("  {}  {}  {}", a.name, short_time(&a.added), a.by.as_deref().unwrap_or("")).trim_end(), fg)];
            if a.missing {
                line.push(seg(" (missing)", dim));
            }
            out.push(line);
        }
    }
    if !t.discussion.is_empty() {
        out.push(Vec::new());
        let mut head = vec![seg("Discussion", bold)];
        if t.discussion.len() < t.discussion_total {
            head.push(seg(format!(" ({} of {})", t.discussion.len(), t.discussion_total), dim));
        }
        out.push(head);
        for c in &t.discussion {
            wrapped(&mut out, &format!("  {}  {}", short_time(&c.ts), c.author.as_deref().unwrap_or("anonymous")), dim, 0);
            for line in c.text.lines() {
                if line.trim().is_empty() {
                    out.push(Vec::new());
                } else {
                    for l in wrap_phrases(&[line], width.saturating_sub(4).max(1)) {
                        out.push(vec![seg(format!("    {l}"), fg)]);
                    }
                }
            }
        }
    }
    out
}

/// A markdown line as runs: its syntax spans over `base`.
fn doc_segs(l: &hover::DocLine, base: Style) -> DetailLine {
    let chars: Vec<char> = l.text.chars().collect();
    let part = |from: usize, to: usize| chars[from.min(chars.len())..to.min(chars.len())].iter().collect::<String>();
    let mut out = Vec::new();
    let mut at = 0;
    for (range, style) in &l.spans {
        if range.start > at {
            out.push(seg(part(at, range.start), base));
        }
        let from = range.start.max(at);
        if range.end > from {
            out.push(seg(part(from, range.end), base.patch(*style)));
        }
        at = at.max(range.end);
    }
    if at < chars.len() {
        out.push(seg(part(at, chars.len()), base));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::event::AppMsg;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    pub(crate) fn row(id: &str, status: &str, title: &str, tags: &[&str], who: Option<&str>) -> TaskRow {
        TaskRow {
            id: id.into(),
            priority: 2,
            status: status.into(),
            title: title.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            assignee: who.map(str::to_string),
            related: vec![],
            created: String::new(),
            updated: String::new(),
            attachments: 0,
            comments: 0,
            dir: String::new(),
        }
    }

    pub(crate) fn snap(rows: Vec<TaskRow>) -> Snapshot {
        let open = rows.iter().filter(|t| t.status != "done").map(|t| t.id.clone()).collect();
        Snapshot { tasks: rows, open, status: None }
    }

    pub(crate) fn sample() -> Snapshot {
        snap(vec![
            row("1", "todo", "Fix the parser", &["ui"], Some("claude")),
            row("2", "doing", "Crash on save", &["bug"], None),
            row("3", "done", "Old chore", &["ui"], Some("claude")),
            row("10", "todo", "Polish the toolbar", &["ui", "bug"], Some("bob")),
        ])
    }

    fn service() -> (YmanService, UnboundedReceiver<AppMsg>) {
        let (tx, rx) = unbounded_channel();
        (YmanService::new(std::env::temp_dir(), 1, tx), rx)
    }

    pub(crate) fn set_snapshot(y: &mut YmanService, s: Snapshot) {
        y.state = TrackerState::Ready(s);
        y.revision += 1;
    }

    fn ids(p: &TasksPanel, y: &YmanService) -> Vec<String> {
        let s = snapshot(y).unwrap();
        p.rows.iter().map(|&i| s.tasks[i].id.clone()).collect()
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn rows_filter_by_closed_tag_assignee_and_text() {
        let (mut y, _rx) = service();
        set_snapshot(&mut y, sample());
        let mut p = TasksPanel::default();
        p.sync(&y);
        assert_eq!(ids(&p, &y), ["1", "2", "10"], "closed tasks are hidden");
        p.show_closed = true;
        p.refilter(&y);
        assert_eq!(ids(&p, &y), ["1", "2", "3", "10"]);
        p.set_tag(Some("UI".into()), &y);
        assert_eq!(ids(&p, &y), ["1", "3", "10"], "tags compare case-insensitively");
        p.set_tag(None, &y);
        p.set_assignee(Some("-".into()), &y);
        assert_eq!(ids(&p, &y), ["2"]);
        p.set_assignee(Some("Bob".into()), &y);
        assert_eq!(ids(&p, &y), ["10"]);
        p.set_assignee(None, &y);
        p.filter = "ui bob".into();
        p.refilter(&y);
        assert_eq!(ids(&p, &y), ["10"], "terms are ANDed");
        p.filter = "PARSER".into();
        p.refilter(&y);
        assert_eq!(ids(&p, &y), ["1"]);
    }

    #[test]
    fn selection_follows_the_id_across_reloads() {
        let (mut y, _rx) = service();
        set_snapshot(&mut y, sample());
        let mut p = TasksPanel::default();
        p.sync(&y);
        p.list.select(2);
        p.remember(&y);
        assert_eq!(p.selected_id(), Some("10"));
        let mut rows = sample().tasks;
        rows.insert(0, row("0", "todo", "New first", &[], None));
        set_snapshot(&mut y, snap(rows));
        p.sync(&y);
        assert_eq!((p.selected_id(), p.list.selected), (Some("10"), 3));
        let rows = vec![row("1", "todo", "a", &[], None), row("2", "todo", "b", &[], None)];
        set_snapshot(&mut y, snap(rows));
        p.sync(&y);
        assert_eq!((p.selected_id(), p.list.selected), (Some("2"), 1), "the index is clamped");
        // While the tracker reloads, the id is remembered.
        y.state = TrackerState::Loading;
        y.revision += 1;
        p.sync(&y);
        assert_eq!(p.selected_id(), None);
        assert_eq!(p.keep.as_deref(), Some("2"));
    }

    #[test]
    fn typing_filters_and_esc_clears() {
        let (mut y, _rx) = service();
        set_snapshot(&mut y, sample());
        let mut p = TasksPanel::default();
        p.sync(&y);
        assert!(!p.handle_key(key(KeyCode::Char(' ')), &y), "a space starts nothing");
        assert!(p.handle_key(key(KeyCode::Char('b')), &y));
        assert!(p.handle_key(key(KeyCode::Char('u')), &y));
        assert_eq!(ids(&p, &y), ["2", "10"]);
        assert!(p.handle_key(key(KeyCode::Backspace), &y));
        assert!(p.handle_key(key(KeyCode::Esc), &y));
        assert_eq!(ids(&p, &y), ["1", "2", "10"]);
        assert!(!p.handle_key(key(KeyCode::Esc), &y), "nothing to clear");
        assert!(!p.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT), &y));
        p.area = Area::Details;
        assert!(!p.handle_key(key(KeyCode::Char('x')), &y), "keys in the details are not the filter");
    }

    #[test]
    fn empty_rows_keep_the_restored_selection() {
        let (mut y, _rx) = service();
        let mut p = TasksPanel { keep: Some("2".into()), ..Default::default() };
        p.sync(&y);
        assert!(p.rows.is_empty());
        p.handle_key(key(KeyCode::Down), &y);
        p.handle_key(key(KeyCode::Home), &y);
        assert_eq!(p.keep.as_deref(), Some("2"));
        set_snapshot(&mut y, sample());
        p.sync(&y);
        assert_eq!(p.selected_id(), Some("2"));
    }

    fn draw(p: &mut TasksPanel, y: &YmanService, w: u16, h: u16) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let theme = Theme::default();
        term.draw(|f| p.render(f, f.area(), true, &theme, y)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h).map(|row| (0..w).map(|col| buf[(col, row)].symbol().to_string()).collect::<String>()).collect()
    }

    #[test]
    fn typed_filter_stays_visible_when_the_toolbar_is_narrow() {
        let (mut y, _rx) = service();
        set_snapshot(&mut y, sample());
        let mut p = TasksPanel::default();
        for c in "parser".chars() {
            p.handle_key(key(KeyCode::Char(c)), &y);
        }
        let lines = draw(&mut p, &y, 80, 12);
        assert!(lines[1].contains("filter: parser"), "{:?}", lines[1]);
        assert!(lines[1].contains("[Closed: hidden]"), "{:?}", lines[1]);
        assert!(!lines[1].contains("[Refresh]"), "trailing buttons drop first: {:?}", lines[1]);
    }

    #[test]
    fn typed_filter_shows_whatever_button_ends_at_the_limit() {
        let (mut y, _rx) = service();
        set_snapshot(&mut y, sample());
        let mut p = TasksPanel::default();
        let mut typed = String::new();
        for c in "abcdefghijklmnopqrstuvwxyz".chars() {
            p.handle_key(key(KeyCode::Char(c)), &y);
            typed.push(c);
            let lines = draw(&mut p, &y, 80, 12);
            assert!(lines[1].contains(&format!(" filter: {typed}")), "{:?}", lines[1]);
        }
    }

    #[test]
    fn empty_state_wraps_and_names_the_active_filter() {
        let (mut y, _rx) = service();
        y.state = TrackerState::Unavailable(crate::yman::YmanError::NotInitialized);
        y.revision += 1;
        let mut p = TasksPanel::default();
        let lines = draw(&mut p, &y, 40, 10);
        let text = lines.join(" ");
        assert!(text.contains("create one"), "{lines:?}");
        set_snapshot(&mut y, sample());
        p.set_tag(Some("nope".into()), &y);
        let text = draw(&mut p, &y, 120, 10).join("\n");
        assert!(text.contains("Alt+T changes the tag filter"), "{text}");
        assert!(!text.contains("Esc clears"), "{text}");
        p.filter = "zzz".into();
        p.refilter(&y);
        assert!(draw(&mut p, &y, 120, 10).join("\n").contains("Esc clears the typed text"));
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        assert_eq!(wrap("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap("toolongword x", 4), ["toolongword", "x"]);
        assert!(wrap("", 4).is_empty());
    }

    #[test]
    fn details_list_related_attachments_and_discussion() {
        let theme = Theme::default();
        let t = super::super::TaskDetails {
            id: "5".into(),
            priority: 1,
            status: "todo".into(),
            title: "T".into(),
            tags: vec![],
            assignee: Some("-".into()),
            links: vec![],
            related: vec!["1".into(), "2".into()],
            related_by: vec![],
            created: "2026-10-25T16:06:18Z".into(),
            updated: "2026-10-25T16:06:18Z".into(),
            dir: String::new(),
            body: "body".into(),
            attachments: vec![super::super::Attachment { name: "a.txt".into(), added: String::new(), by: None, missing: true }],
            discussion: vec![super::super::Comment { ts: "2026-10-25T16:20:00Z".into(), author: None, text: "hi\n\nthere".into() }],
            discussion_total: 4,
        };
        let text: Vec<String> = detail_lines(&t, 60, &theme).iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect();
        assert_eq!(text[0], "#5 T");
        assert_eq!(text[1], "todo · P1 · unassigned");
        assert_eq!(text[2], "Related: 1, 2");
        assert!(text.contains(&"Attachments".to_string()) && text.contains(&"  a.txt (missing)".to_string()), "{text:?}");
        assert!(text.contains(&"Discussion (1 of 4)".to_string()), "{text:?}");
        assert!(text.contains(&"  2026-10-25 16:20  anonymous".to_string()), "{text:?}");
        assert!(text.contains(&"    hi".to_string()) && text.contains(&"    there".to_string()), "{text:?}");
        let ids: Vec<_> = detail_lines(&t, 60, &theme)[2].iter().filter_map(|s| s.id.clone()).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn short_time_formats() {
        assert_eq!(short_time("2026-10-25T16:06:18Z"), "2026-10-25 16:06");
        assert_eq!(short_time("yesterday"), "yesterday");
        assert_eq!(short_time(""), "");
    }
}
