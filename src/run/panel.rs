//! Run tool window: a toolbar (configuration picker, Run, Rerun, Stop), the
//! last run's status line (name, spinner or exit status, elapsed time,
//! command) and its output, at most `MAX_LINES`
//! lines (the oldest are dropped). Matched locations are underlined and
//! colored by severity; a click on one, Enter or a double click opens it.
//! The selection follows new output while it sits on the last line and that
//! line is in view, like the Event Log.
//!
//! A run whose matcher kind is `Tests` also feeds a `TestRun`; the Tests
//! view shows it as a tree (suites, groups, tests, the output of failed
//! tests below them) next to the raw Output view.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::matcher::{Link, RunLine, Severity};
use super::testing::{TestCase, TestRun, TestStatus};
use crate::actions::{Action, AppAction, PaletteKind, RunAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::menu::{ContextMenu, MenuItem};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;

/// Output lines kept per run.
pub const MAX_LINES: usize = 10_000;

/// How far below a `error: …` header Enter looks for its location.
const LOOKAHEAD: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(i32),
    /// Stopped by `run.stop`.
    Stopped,
    /// Killed by a signal jetl did not send.
    Signal(i32),
    /// It could not start, or waiting for it failed.
    Failed(String),
}

/// A toolbar button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Picker,
    Run,
    Rerun,
    Stop,
    RerunFailed,
    EditConfigs,
    /// A tab of the view switch.
    View(View),
}

/// What the Run window's list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Output,
    Tests,
}

/// A row that can be collapsed, by name (rows move while a run streams).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Suite(String),
    Group(String, String),
    /// Suite and id of a failed test.
    Case(String, String),
}

/// A row of the test tree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Suite(String),
    Group(String, String),
    /// Index into `TestRun::cases`.
    Case(usize),
    /// Output line `.1` of case `.0`.
    Output(usize, usize),
}

fn case_key(c: &TestCase) -> RowKey {
    RowKey::Case(c.suite.clone(), c.id.clone())
}

/// The rows of the tree: suites in the order the run met them, inside a
/// suite the tests by group and name; failed tests are expanded.
fn build_rows(t: &TestRun, collapsed: &HashSet<RowKey>) -> Vec<Row> {
    let mut rows = Vec::new();
    for suite in t.suites() {
        rows.push(Row::Suite(suite.clone()));
        if collapsed.contains(&RowKey::Suite(suite.clone())) {
            continue;
        }
        let mut ids: Vec<usize> = (0..t.cases().len()).filter(|&i| t.cases()[i].suite == *suite).collect();
        ids.sort_by(|&a, &b| {
            let (a, b) = (&t.cases()[a], &t.cases()[b]);
            (&a.group, &a.name).cmp(&(&b.group, &b.name))
        });
        let mut group: Option<&str> = None;
        for i in ids {
            let c = &t.cases()[i];
            if !c.group.is_empty() {
                if group != Some(c.group.as_str()) {
                    rows.push(Row::Group(suite.clone(), c.group.clone()));
                    group = Some(c.group.as_str());
                }
                if collapsed.contains(&RowKey::Group(suite.clone(), c.group.clone())) {
                    continue;
                }
            }
            rows.push(Row::Case(i));
            if c.status == TestStatus::Failed && !collapsed.contains(&case_key(c)) {
                rows.extend((0..c.output.len()).map(|j| Row::Output(i, j)));
            }
        }
    }
    rows
}

impl Btn {
    /// The text between the padding spaces; the picker's is its arrow.
    fn label(self) -> &'static str {
        match self {
            Btn::Picker => "▾",
            Btn::Run => "▶ Run",
            Btn::Rerun => "↻ Rerun",
            Btn::Stop => "■ Stop",
            Btn::RerunFailed => "Rerun Failed",
            Btn::EditConfigs => "Edit Configurations…",
            Btn::View(View::Output) => "Output",
            Btn::View(View::Tests) => "Tests",
        }
    }
}

#[derive(Debug)]
struct Header {
    name: String,
    command: String,
    started: Instant,
    ended: Option<Instant>,
    status: Status,
}

#[derive(Debug, Default)]
pub struct RunPanel {
    pub lines: VecDeque<RunLine>,
    pub state: ListState,
    header: Option<Header>,
    list_rect: Rect,
    /// Screen cells of the visible links and their line.
    link_rects: Vec<(Rect, usize)>,
    /// Screen cells of the drawn toolbar buttons.
    hits: Vec<(Btn, Rect)>,
    view: View,
    /// The parsed results of a `Tests` run.
    tests: Option<TestRun>,
    collapsed: HashSet<RowKey>,
    tree_state: ListState,
    rows: Vec<Row>,
    /// Screen cells of the drawn expanders and their row.
    expanders: Vec<(Rect, usize)>,
}

impl RunPanel {
    /// A new run: the old output goes.
    pub fn start(&mut self, name: &str, command: &str, tests: bool) {
        self.lines.clear();
        self.state = ListState::default();
        self.tests = tests.then(TestRun::default);
        self.view = if tests { View::Tests } else { View::Output };
        self.collapsed.clear();
        self.tree_state = ListState::default();
        self.rows.clear();
        self.header =
            Some(Header { name: name.into(), command: command.into(), started: Instant::now(), ended: None, status: Status::Running });
    }

    pub fn push(&mut self, lines: Vec<RunLine>) {
        if let Some(t) = &mut self.tests {
            t.feed(&lines);
        }
        self.lines.extend(lines);
        let dropped = self.lines.len().saturating_sub(MAX_LINES);
        self.lines.drain(..dropped);
        self.state.grew(self.lines.len(), dropped);
        self.rebuild_rows();
    }

    /// Rebuild the tree; the selection stays on its row when it still exists.
    fn rebuild_rows(&mut self) {
        let Some(t) = &self.tests else { return };
        let old = self.rows.get(self.tree_state.selected).cloned();
        self.rows = build_rows(t, &self.collapsed);
        if let Some(i) = old.and_then(|o| self.rows.iter().position(|r| *r == o)) {
            self.tree_state.selected = i;
        }
        self.tree_state.set_len(self.rows.len());
    }

    pub fn tests(&self) -> Option<&TestRun> {
        self.tests.as_ref()
    }

    pub fn view(&self) -> View {
        self.view
    }

    fn has_failed(&self) -> bool {
        self.tests.as_ref().is_some_and(|t| t.failed().next().is_some())
    }

    pub fn finish(&mut self, status: Status) {
        if let Some(h) = &mut self.header {
            h.ended = Some(Instant::now());
            h.status = status;
        }
        if self.tests.as_ref().is_some_and(|t| t.cases().is_empty()) {
            self.view = View::Output;
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.state = ListState::default();
        self.header = None;
        self.tests = None;
        self.view = View::Output;
        self.collapsed.clear();
        self.tree_state = ListState::default();
        self.rows.clear();
    }

    pub fn status(&self) -> Option<&Status> {
        self.header.as_ref().map(|h| &h.status)
    }

    /// The command line shown in the header.
    #[cfg(test)]
    pub fn command(&self) -> Option<&str> {
        self.header.as_ref().map(|h| h.command.as_str())
    }

    pub fn running(&self) -> bool {
        self.status() == Some(&Status::Running)
    }

    /// Stop the clock of the header, so a snapshot shows a fixed time.
    #[cfg(test)]
    pub fn freeze_elapsed(&mut self) {
        if let Some(h) = &mut self.header {
            h.ended = Some(h.started);
        }
    }

    /// Select the tree row of the test called `name`.
    #[cfg(test)]
    pub fn select_test(&mut self, name: &str) {
        let t = self.tests.as_ref().unwrap();
        let i = self.rows.iter().position(|r| matches!(r, Row::Case(c) if t.cases()[*c].name == name)).unwrap();
        self.tree_state.set_len(self.rows.len());
        self.tree_state.select(i);
    }

    /// The cells of the toolbar button whose label contains `label`.
    #[cfg(test)]
    pub fn hit_rect(&self, label: &str) -> Option<Rect> {
        self.hits.iter().find(|(b, _)| b.label().contains(label)).map(|(_, r)| *r)
    }

    /// Where line `i` leads: its own location, or for a header line
    /// (`error[E0308]: …`) the first one in the lines right below it.
    fn target(&self, i: usize) -> Option<&Link> {
        let line = self.lines.get(i)?;
        if let Some(l) = &line.link {
            return Some(l);
        }
        line.severity?;
        self.lines.iter().skip(i + 1).take(LOOKAHEAD).take_while(|l| !l.text.trim().is_empty()).find_map(|l| l.link.as_ref())
    }

    fn open(&self, i: usize, cx: &mut Ctx) -> bool {
        let Some(l) = self.target(i) else { return false };
        cx.effects.push(Effect::OpenFileAt { path: l.path.clone(), line: l.line, col: l.col });
        true
    }

    /// Select and open the next (or previous) line with a location,
    /// wrapping around.
    pub fn step(&mut self, forward: bool, cx: &mut Ctx) {
        let n = self.lines.len();
        if !self.lines.iter().any(|l| l.link.is_some()) {
            cx.notify(Level::Info, "No problems in the Run output");
            return;
        }
        self.state.set_len(n);
        let mut i = self.state.selected;
        for _ in 0..n {
            i = if forward { (i + 1) % n } else { (i + n - 1) % n };
            if self.lines[i].link.is_some() {
                break;
            }
        }
        self.state.select(i);
        self.open(i, cx);
    }

    /// The key of the row if it can be collapsed (a failed test with output).
    fn row_key(&self, i: usize) -> Option<RowKey> {
        match self.rows.get(i)? {
            Row::Suite(s) => Some(RowKey::Suite(s.clone())),
            Row::Group(s, g) => Some(RowKey::Group(s.clone(), g.clone())),
            Row::Case(c) | Row::Output(c, _) => {
                let case = &self.tests.as_ref()?.cases()[*c];
                (case.status == TestStatus::Failed && !case.output.is_empty()).then(|| case_key(case))
            }
        }
    }

    /// Where tree row `i` leads: a test's failure location, an output line's link.
    fn row_link(&self, i: usize) -> Option<&Link> {
        let cases = self.tests.as_ref()?.cases();
        match self.rows.get(i)? {
            Row::Case(c) => cases[*c].link(),
            Row::Output(c, j) => cases[*c].output[*j].link.as_ref(),
            _ => None,
        }
    }

    fn open_row(&self, i: usize, cx: &mut Ctx) {
        match self.row_link(i) {
            Some(l) => cx.effects.push(Effect::OpenFileAt { path: l.path.clone(), line: l.line, col: l.col }),
            None => cx.notify(Level::Info, "No location for this test"),
        }
    }

    /// Expand or collapse the selected row; on an output line that is its test.
    fn set_expanded(&mut self, expand: bool) {
        let i = self.tree_state.selected;
        let Some(key) = self.row_key(i) else { return };
        if expand {
            self.collapsed.remove(&key);
        } else {
            if let Some(Row::Output(c, _)) = self.rows.get(i).cloned() {
                self.tree_state.selected = self.rows.iter().position(|r| *r == Row::Case(c)).unwrap_or(i);
            }
            self.collapsed.insert(key);
        }
        self.rebuild_rows();
    }

    /// Select and open the next (or previous) failed test, wrapping around.
    fn step_failed(&mut self, forward: bool, cx: &mut Ctx) {
        let Some(t) = &self.tests else { return };
        let failed: Vec<usize> =
            (0..self.rows.len()).filter(|&i| matches!(&self.rows[i], Row::Case(c) if t.cases()[*c].status == TestStatus::Failed)).collect();
        let cur = self.tree_state.selected;
        let next = if forward {
            failed.iter().copied().find(|&i| i > cur).or(failed.first().copied())
        } else {
            failed.iter().rev().copied().find(|&i| i < cur).or(failed.last().copied())
        };
        let Some(i) = next else {
            cx.notify(Level::Info, "No failed tests");
            return;
        };
        self.tree_state.select(i);
        self.open_row(i, cx);
    }

    fn handle_tree_action(&mut self, a: &RunAction, cx: &mut Ctx) -> bool {
        self.tree_state.set_len(self.rows.len());
        let page = self.tree_state.viewport.max(1);
        match a {
            RunAction::Up => self.tree_state.up(1),
            RunAction::Down => self.tree_state.down(1),
            RunAction::PageUp => self.tree_state.up(page),
            RunAction::PageDown => self.tree_state.down(page),
            RunAction::Home => self.tree_state.home(),
            RunAction::End => self.tree_state.end(),
            RunAction::Open => self.open_row(self.tree_state.selected, cx),
            RunAction::NextProblem => self.step_failed(true, cx),
            RunAction::PrevProblem => self.step_failed(false, cx),
            RunAction::Expand => self.set_expanded(true),
            RunAction::Collapse => self.set_expanded(false),
            _ => return false,
        }
        true
    }

    pub fn handle_action(&mut self, a: &RunAction, cx: &mut Ctx) -> bool {
        if *a == RunAction::ToggleView {
            if self.tests.is_some() {
                self.view = if self.view == View::Tests { View::Output } else { View::Tests };
            }
            return true;
        }
        if self.view == View::Tests && self.tests.is_some() && self.handle_tree_action(a, cx) {
            return true;
        }
        self.state.set_len(self.lines.len());
        let page = self.state.viewport.max(1);
        match a {
            RunAction::Up => self.state.up(1),
            RunAction::Down => self.state.down(1),
            RunAction::PageUp => self.state.up(page),
            RunAction::PageDown => self.state.down(page),
            RunAction::Home => self.state.home(),
            RunAction::End => self.state.end(),
            RunAction::Open => {
                self.open(self.state.selected, cx);
            }
            RunAction::NextProblem => self.step(true, cx),
            RunAction::PrevProblem => self.step(false, cx),
            // They start and stop processes; `App::perform` runs them.
            RunAction::Rerun | RunAction::RerunFailed | RunAction::Stop | RunAction::RunSelected | RunAction::EditConfigs => return false,
            RunAction::ToggleView | RunAction::Expand | RunAction::Collapse => {}
        }
        true
    }

    fn list_mut(&mut self) -> &mut ListState {
        if self.view == View::Tests && self.tests.is_some() { &mut self.tree_state } else { &mut self.state }
    }

    pub fn handle_mouse(&mut self, m: &Mouse, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if self.hits.iter().any(|(_, r)| r.contains((m.x, m.y).into())) => {
                let btn = self.hits.iter().find(|(_, r)| r.contains((m.x, m.y).into())).map(|(b, _)| *b);
                let action = match btn {
                    Some(Btn::Picker) => Some(Action::App(AppAction::OpenPalette(PaletteKind::RunTasks))),
                    Some(Btn::Run) => Some(Action::Run(RunAction::RunSelected)),
                    Some(Btn::Rerun) if self.header.is_some() => Some(Action::Run(RunAction::Rerun)),
                    Some(Btn::Stop) if self.running() => Some(Action::Run(RunAction::Stop)),
                    Some(Btn::RerunFailed) if self.has_failed() => Some(Action::Run(RunAction::RerunFailed)),
                    Some(Btn::View(v)) if v != self.view => Some(Action::Run(RunAction::ToggleView)),
                    Some(Btn::EditConfigs) => Some(Action::Run(RunAction::EditConfigs)),
                    _ => None,
                };
                if let Some(a) = action {
                    cx.effects.push(Effect::Perform(a));
                }
                true
            }
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let tree = self.view == View::Tests && self.tests.is_some();
                let Some(i) = (if tree { &self.tree_state } else { &self.state }).hit(self.list_rect, m.x, m.y) else { return false };
                if tree {
                    self.tree_state.select(i);
                } else {
                    self.state.select(i);
                }
                let has_link = if tree { self.row_link(i).is_some() } else { self.target(i).is_some() };
                if button == MouseButton::Right {
                    let items = vec![
                        MenuItem::new("Jump to Source", Action::Run(RunAction::Open)).enabled(has_link),
                        MenuItem::new("Run Selected", Action::Run(RunAction::RunSelected)),
                        MenuItem::new("Rerun", Action::Run(RunAction::Rerun)),
                        MenuItem::new("Rerun Failed Tests", Action::Run(RunAction::RerunFailed)).enabled(self.has_failed()),
                        MenuItem::new("Stop", Action::Run(RunAction::Stop)).enabled(self.running()),
                        MenuItem::new("Edit Configurations…", Action::Run(RunAction::EditConfigs)),
                    ];
                    cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, m.x, m.y)));
                } else if tree && self.expanders.iter().any(|(r, row)| *row == i && r.contains((m.x, m.y).into())) {
                    self.set_expanded(!self.row_key(i).is_some_and(|k| !self.collapsed.contains(&k)));
                } else if m.clicks == 2 || self.link_rects.iter().any(|(r, _)| r.contains((m.x, m.y).into())) {
                    if tree {
                        self.open_row(i, cx);
                    } else {
                        self.open(i, cx);
                    }
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.list_mut().scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.list_mut().scroll(3);
                true
            }
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, selected: Option<&str>) {
        let title = match &self.header {
            Some(h) => format!(" Run: {} ", h.name),
            None => " Run ".to_string(),
        };
        let block = tool_window_block(&title, focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        self.link_rects.clear();
        self.expanders.clear();
        self.hits.clear();
        self.list_rect = Rect::default();
        if r.height == 0 || r.width < 4 {
            return;
        }
        let buf = frame.buffer_mut();
        let right = r.right();
        // Toolbar: the picker always, then as many whole buttons as fit.
        let bar = Style::default().bg(theme.panel_bg);
        buf.set_style(Rect::new(r.x, r.y, r.width, 1), bar);
        let mut x = r.x;
        let picker = format!(" {} ▾ ", selected.unwrap_or("Select Configuration"));
        let rect = put(buf, &mut x, r.y, right, &picker, theme.tab_active);
        self.hits.push((Btn::Picker, rect));
        let running = self.running();
        let ran = self.header.is_some();
        let failed = self.has_failed();
        let mut buttons = vec![Btn::Run, Btn::Rerun, Btn::Stop];
        if self.tests.is_some() {
            buttons.push(Btn::RerunFailed);
        }
        buttons.push(Btn::EditConfigs);
        let mut dropped = false;
        for btn in buttons {
            let text = format!(" {} ", btn.label());
            if x + 1 + text.width() as u16 > right {
                dropped = true;
                break;
            }
            let enabled = match btn {
                Btn::Rerun => ran,
                Btn::Stop => running,
                Btn::RerunFailed => failed,
                _ => true,
            };
            let style = if enabled { theme.title.bg(theme.panel_bg) } else { theme.dim.bg(theme.panel_bg) };
            x += 1;
            let rect = put(buf, &mut x, r.y, right, &text, style);
            self.hits.push((btn, rect));
        }
        // The view tabs, right-aligned; the first to go when space is short.
        if self.tests.is_some() && !dropped {
            let tabs = [View::Output, View::Tests].map(|v| (v, format!(" {} ", Btn::View(v).label())));
            let width: u16 = tabs.iter().map(|(_, t)| t.width() as u16).sum();
            if x + 1 + width <= right {
                let mut tx = right - width;
                for (v, text) in tabs {
                    let style = if v == self.view { theme.tab_active } else { theme.tab };
                    let rect = put(buf, &mut tx, r.y, right, &text, style);
                    self.hits.push((Btn::View(v), rect));
                }
            }
        }
        if r.height < 2 {
            return;
        }
        let Some(h) = &self.header else {
            let hint = "No configuration has run yet: Alt+Shift+F10 picks one, Shift+F10 runs the selected one";
            buf.set_stringn(r.x + 1, r.y + 1, hint, r.width.saturating_sub(1) as usize, theme.dim);
            return;
        };
        // Status line.
        let (glyph, text, style) = status_text(h, theme);
        let (x, _) = buf.set_stringn(r.x + 1, r.y + 1, glyph, (right - r.x - 1) as usize, style.add_modifier(Modifier::BOLD));
        let elapsed = format_elapsed(h.ended.unwrap_or_else(Instant::now) - h.started);
        let mut rest = vec![(format!(" {text}"), style), (format!("  {elapsed}"), theme.dim)];
        if let (Some(t), View::Tests) = (&self.tests, self.view) {
            let c = t.counts();
            let base = if h.status == Status::Running { theme.info } else { theme.dim };
            let failed = if c.failed > 0 { theme.error } else { base };
            rest.push((format!("  Tests: {} passed, ", c.passed), base));
            rest.push((format!("{} failed", c.failed), failed));
            rest.push((format!(", {} ignored", c.ignored), base));
        }
        rest.push((format!("  $ {}", h.command), theme.dim));
        let mut x = x;
        for (s, st) in rest {
            if x >= right {
                break;
            }
            x = buf.set_stringn(x, r.y + 1, &s, (right - x) as usize, st).0;
        }
        if r.height < 3 {
            return;
        }
        // Output.
        let list = Rect::new(r.x, r.y + 2, r.width, r.height - 2);
        self.list_rect = list;
        if self.view == View::Tests && self.tests.is_some() {
            self.render_tree(buf, list, focused, theme, running);
            return;
        }
        self.state.fit(self.lines.len(), list.height as usize);
        for (vi, i) in self.state.visible().enumerate() {
            let line = &self.lines[i];
            let y = list.y + vi as u16;
            let base = row_style(i == self.state.selected, focused, theme);
            if i == self.state.selected {
                buf.set_style(Rect::new(list.x, y, list.width, 1), base);
            }
            draw_line(buf, line, list.x + 1, y, right, base, theme, &mut self.link_rects, i);
        }
        if self.lines.is_empty() && h.status == Status::Running && list.height > 0 {
            buf.set_stringn(list.x + 1, list.y, "No output yet", list.width.saturating_sub(1) as usize, theme.dim);
        }
    }

    /// The Tests view's tree in `list`.
    fn render_tree(&mut self, buf: &mut Buffer, list: Rect, focused: bool, theme: &Theme, running: bool) {
        let Some(t) = &self.tests else { return };
        let right = list.right();
        self.tree_state.fit(self.rows.len(), list.height as usize);
        let cases = t.cases();
        let fg = |s: Style| s.fg.unwrap_or(theme.fg);
        for (vi, i) in self.tree_state.visible().enumerate() {
            let y = list.y + vi as u16;
            let selected = i == self.tree_state.selected;
            let base = row_style(selected, focused, theme);
            if selected {
                buf.set_style(Rect::new(list.x, y, list.width, 1), base);
            }
            let expander = |expanded: bool| if expanded { "▾" } else { "▸" };
            // (indent, expander, glyph with its colour, name, name style)
            let (indent, exp, glyph, name, name_style) = match &self.rows[i] {
                Row::Suite(s) => {
                    let open = !self.collapsed.contains(&RowKey::Suite(s.clone()));
                    (0, Some(expander(open)), None, s.as_str(), base.add_modifier(Modifier::BOLD))
                }
                Row::Group(s, g) => {
                    let open = !self.collapsed.contains(&RowKey::Group(s.clone(), g.clone()));
                    (1, Some(expander(open)), None, g.as_str(), base)
                }
                Row::Case(c) => {
                    let case = &cases[*c];
                    let exp = (case.status == TestStatus::Failed && !case.output.is_empty())
                        .then(|| expander(!self.collapsed.contains(&case_key(case))));
                    let glyph = match case.status {
                        TestStatus::Passed => ("✓", base.fg(fg(theme.git_added))),
                        TestStatus::Failed => ("✗", base.fg(fg(theme.error))),
                        TestStatus::Ignored => ("○", base.fg(fg(theme.dim))),
                    };
                    (if case.group.is_empty() { 1 } else { 2 }, exp, Some(glyph), case.name.as_str(), base)
                }
                Row::Output(c, j) => {
                    let case = &cases[*c];
                    let indent = if case.group.is_empty() { 3 } else { 4 };
                    draw_line(buf, &case.output[*j], list.x + 1 + indent, y, right, base, theme, &mut self.link_rects, i);
                    continue;
                }
            };
            let x0 = list.x + 1 + indent;
            if x0 >= right {
                continue;
            }
            if let Some(e) = exp {
                buf.set_stringn(x0, y, e, 1, name_style);
                self.expanders.push((Rect::new(x0, y, 1, 1), i));
            }
            let mut x = x0 + 2;
            if let Some((g, style)) = glyph
                && x < right
            {
                x = buf.set_stringn(x, y, g, (right - x) as usize, style).0 + 1;
            }
            if x < right {
                buf.set_stringn(x, y, name, (right - x) as usize, name_style);
            }
        }
        if self.rows.is_empty() && running && list.height > 0 {
            buf.set_stringn(list.x + 1, list.y, "Waiting for test results…", list.width.saturating_sub(1) as usize, theme.dim);
        }
    }
}

/// The base style of a list row.
fn row_style(selected: bool, focused: bool, theme: &Theme) -> Style {
    if selected { if focused { theme.list_selected } else { theme.list_selected_unfocused } } else { Style::default().fg(theme.fg) }
}

/// Draw an output line at `x0`: severity colour, the link underlined (and
/// pushed to `link_rects` with `row`).
#[allow(clippy::too_many_arguments)]
fn draw_line(
    buf: &mut Buffer,
    line: &RunLine,
    x0: u16,
    y: u16,
    right: u16,
    base: Style,
    theme: &Theme,
    link_rects: &mut Vec<(Rect, usize)>,
    row: usize,
) {
    if x0 >= right {
        return;
    }
    let style = match line.severity {
        Some(Severity::Error) => base.fg(theme.error.fg.unwrap_or(theme.fg)),
        Some(Severity::Warning) => base.fg(theme.warn.fg.unwrap_or(theme.fg)),
        None => base,
    };
    let width = (right - x0) as usize;
    match &line.link {
        Some(link) => {
            let (pre, at, post) = (&line.text[..link.span.start], &line.text[link.span.clone()], &line.text[link.span.end..]);
            let (x, _) = buf.set_stringn(x0, y, pre, width, style);
            if x < right {
                let (x2, _) = buf.set_stringn(x, y, at, (right - x) as usize, style.add_modifier(Modifier::UNDERLINED));
                link_rects.push((Rect::new(x, y, x2 - x, 1), row));
                if x2 < right {
                    buf.set_stringn(x2, y, post, (right - x2) as usize, style);
                }
            }
        }
        None => {
            buf.set_stringn(x0, y, &line.text, width, style);
        }
    }
}

/// Write `text` at `*x` clipped to `right`, advance `*x`, return its rect.
fn put(buf: &mut ratatui::buffer::Buffer, x: &mut u16, y: u16, right: u16, text: &str, style: Style) -> Rect {
    let w = (text.width() as u16).min(right.saturating_sub(*x));
    if w > 0 {
        buf.set_stringn(*x, y, text, w as usize, style);
    }
    let r = Rect::new(*x, y, w, 1);
    *x += w;
    r
}

fn status_text(h: &Header, theme: &Theme) -> (&'static str, String, Style) {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    match &h.status {
        Status::Running => {
            let frame = FRAMES[(h.started.elapsed().as_millis() / 100) as usize % FRAMES.len()];
            (frame, "running".into(), theme.info)
        }
        Status::Exited(0) => ("✓", "exit code 0".into(), theme.git_added),
        Status::Exited(c) => ("✗", format!("exit code {c}"), theme.error),
        Status::Stopped => ("■", "stopped".into(), theme.warn),
        Status::Signal(s) => ("✗", format!("killed by signal {s}"), theme.error),
        Status::Failed(e) => ("✗", e.clone(), theme.error),
    }
}

/// `0.4s`, `12s`, `3m 05s`.
fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 10 {
        format!("{:.1}s", d.as_secs_f64())
    } else if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_format() {
        assert_eq!(format_elapsed(Duration::from_millis(400)), "0.4s");
        assert_eq!(format_elapsed(Duration::from_secs(12)), "12s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn output_is_capped_and_followed() {
        let mut p = RunPanel::default();
        p.start("t", "t", false);
        p.push((0..MAX_LINES + 5).map(|i| RunLine::plain(i.to_string())).collect());
        assert_eq!(p.lines.len(), MAX_LINES);
        assert_eq!(p.lines[0].text, "5", "the oldest lines go");
        assert_eq!(p.state.selected, MAX_LINES - 1, "follows the end");
        p.state.fit(MAX_LINES, 10);
        p.state.up(3);
        p.push(vec![RunLine::plain("x"), RunLine::plain("y")]);
        assert_eq!(p.lines[p.state.selected].text, (MAX_LINES + 1).to_string(), "scrolled up: stays on its line");
    }

    use crate::app::tests::test_app;
    use crate::event::AppMsg;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn draw(p: &mut RunPanel, w: u16, h: u16) -> Terminal<TestBackend> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| p.render(f, f.area(), true, &theme, None)).unwrap();
        term
    }

    fn click(app: &mut crate::app::App, label: &str) {
        let r = app.run_panel.hit_rect(label).unwrap();
        let m =
            Mouse { kind: MouseEventKind::Down(MouseButton::Left), x: r.x, y: r.y, clicks: 1, mods: crossterm::event::KeyModifiers::NONE };
        app.forward_mouse(crate::ui::focus::PanelId::Run, Rect::default(), &m);
    }

    #[tokio::test]
    async fn toolbar_buttons_perform_their_actions() {
        let (mut app, dir) = test_app("run-toolbar");
        draw(&mut app.run_panel, 80, 8);
        // Picker: the Run picker opens.
        assert!(app.palette.is_none());
        click(&mut app, "▾");
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::RunTasks));
        app.palette = None;
        // Run asks for the selected configuration (it waits for the list, no
        // picker); Rerun without a last run opens the picker.
        app.run.selected = Some("x".into());
        click(&mut app, "Run");
        assert!(app.palette.is_none(), "Run is not Rerun");
        app.run.selected = None;
        click(&mut app, "Run");
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::RunTasks), "nothing selected: the picker");
        app.palette = None;
        // Stop: inert while nothing runs.
        let toasts = app.notifications.len();
        click(&mut app, "Stop");
        assert_eq!(app.notifications.len(), toasts, "no 'No task is running' toast: the button is inert");
        // After a run starts the button stops it.
        let text = "[[run.tasks]]\nname = \"wait\"\ncommand = \"sleep 30\"\n";
        app.run_task(crate::config::Config::parse(text).unwrap().run.tasks.remove(0));
        assert!(app.run.running());
        draw(&mut app.run_panel, 80, 8);
        click(&mut app, "Stop");
        while app.run.running() {
            crate::app::tests::handle_next(&mut app, |m| matches!(m, AppMsg::RunOutput { .. } | AppMsg::RunExited { .. })).await;
        }
        assert_eq!(app.run_panel.status(), Some(&Status::Stopped), "Stop killed the run");
        // Rerun starts the last run again.
        let seq = app.run.seq;
        click(&mut app, "Rerun");
        assert_eq!(app.run.seq, seq + 1);
        app.run.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_configurations_button_opens_the_window() {
        let (mut app, dir) = test_app("run-toolbar-edit");
        draw(&mut app.run_panel, 100, 8);
        assert!(app.run_configs.is_none());
        click(&mut app, "Edit Configurations");
        assert!(app.run_configs.is_some());
        assert!(!app.run.running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn narrow_toolbar_keeps_the_picker() {
        let mut p = RunPanel::default();
        draw(&mut p, 80, 6);
        assert_eq!(p.hits.len(), 5);
        draw(&mut p, 60, 6);
        assert!(p.hit_rect("Edit").is_none() && p.hits.len() == 4, "{:?}", p.hits);
        let term = draw(&mut p, 20, 6);
        assert!(p.hit_rect("▾").is_some());
        assert!(p.hit_rect("Run").is_none() && p.hits.len() == 1, "{:?}", p.hits);
        let row: String = (0..20).map(|x| term.backend().buffer()[(x, 1)].symbol().to_string()).collect();
        assert!(row.contains("Select"), "{row}");
        draw(&mut p, 5, 2);
        draw(&mut p, 4, 1);
    }

    #[test]
    fn output_starts_on_the_third_row() {
        let mut p = RunPanel::default();
        p.start("t", "t", false);
        p.push(vec![RunLine::plain("a")]);
        draw(&mut p, 40, 8);
        // Border, toolbar, status.
        assert_eq!(p.list_rect.y, 3);
        let term = draw(&mut p, 40, 8);
        let row: String = (0..40).map(|x| term.backend().buffer()[(x, 3)].symbol().to_string()).collect();
        assert!(row.contains('a'), "{row}");
        draw(&mut p, 40, 3);
        assert_eq!(p.list_rect, Rect::default(), "no room for a list");
    }

    const LIBTEST: &str = include_str!("fixtures/libtest.txt");
    const TWO_BINARIES: &str = include_str!("fixtures/libtest_two_binaries.txt");

    fn project(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-panel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        for f in ["src/lib.rs", "tests/cli.rs"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        dir
    }

    fn matched(root: &std::path::Path, text: &str) -> Vec<RunLine> {
        let mut m = crate::run::matcher::Matcher::new(root, root);
        text.lines().map(|l| m.line(l.to_string())).collect()
    }

    /// The tree rows, as text.
    fn labels(p: &RunPanel) -> Vec<String> {
        let cases = p.tests.as_ref().unwrap().cases();
        p.rows
            .iter()
            .map(|r| match r {
                Row::Suite(s) => format!("suite {s}"),
                Row::Group(_, g) => format!("group {g}"),
                Row::Case(c) => cases[*c].name.clone(),
                Row::Output(c, j) => format!("out {}", cases[*c].output[*j].text),
            })
            .collect()
    }

    fn select(p: &mut RunPanel, label: &str) {
        let i = labels(p).iter().position(|l| l == label).unwrap_or_else(|| panic!("no row {label}"));
        p.tree_state.set_len(p.rows.len());
        p.tree_state.select(i);
    }

    fn selected(p: &RunPanel) -> String {
        labels(p)[p.tree_state.selected].clone()
    }

    /// Perform a panel action with a throwaway `Ctx`; returns its effects.
    fn act(p: &mut RunPanel, a: RunAction) -> Vec<Effect> {
        let (config, theme, workspace) =
            (crate::config::Config::default(), Theme::default(), crate::workspace::Workspace::new("/tmp".into()));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        p.handle_action(&a, &mut cx);
        cx.effects
    }

    fn libtest_panel(root: &std::path::Path) -> RunPanel {
        let mut p = RunPanel::default();
        p.start("cargo test", "cargo test", true);
        p.push(matched(root, LIBTEST));
        p
    }

    #[test]
    fn tree_rows_expand_failed_and_keep_collapse() {
        let root = project("rows");
        let mut p = libtest_panel(&root);
        // Sorted by group, then name; the failed test is expanded.
        assert_eq!(
            labels(&p),
            [
                "suite src/lib.rs",
                "group tests",
                "adds",
                "panics",
                "slow",
                "group tests::nested",
                "fails",
                "out thread 'tests::nested::fails' panicked at src/lib.rs:12:9:",
                "out assertion `left == right` failed",
                "out   left: 2",
                "out  right: 3",
                "out note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
            ]
        );
        select(&mut p, "fails");
        act(&mut p, RunAction::Collapse);
        assert_eq!(labels(&p).len(), 7, "the output is hidden");
        assert_eq!(selected(&p), "fails");
        // More lines arrive: the collapse stays.
        p.push(matched(&root, "   Running tests/cli.rs (target/debug/deps/cli-0123)\n\nrunning 1 test\ntest outside ... ok\n"));
        assert_eq!(labels(&p).len(), 9, "{:?}", labels(&p));
        assert!(!labels(&p).iter().any(|l| l.starts_with("out ")), "still collapsed");
        act(&mut p, RunAction::Expand);
        assert_eq!(labels(&p).iter().filter(|l| l.starts_with("out ")).count(), 5);
        // A group collapses its tests; the suite its groups.
        select(&mut p, "group tests");
        act(&mut p, RunAction::Collapse);
        assert!(!labels(&p).contains(&"adds".to_string()) && labels(&p).contains(&"fails".to_string()));
        select(&mut p, "suite src/lib.rs");
        act(&mut p, RunAction::Collapse);
        assert_eq!(labels(&p), ["suite src/lib.rs", "suite tests/cli.rs", "outside"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn selection_stays_on_its_row_while_streaming() {
        let root = project("select");
        let mut p = RunPanel::default();
        p.start("cargo test", "cargo test", true);
        // The second binary's suite sorts after the first; a new test in an
        // earlier group would shift `adds` down, the selection follows the row.
        p.push(matched(
            &root,
            "     Running unittests src/lib.rs (a)\n\nrunning 2 tests\ntest tests::adds ... ok\ntest tests::zed ... ok\n",
        ));
        select(&mut p, "adds");
        p.push(matched(&root, "test a::first ... ok\n"));
        assert_eq!(selected(&p), "adds", "{:?}", labels(&p));
        assert_eq!(p.tree_state.selected, 4, "shifted down by the new group and test");
        p.push(matched(&root, "   Running tests/cli.rs (b)\n\nrunning 1 test\ntest outside ... ok\n"));
        assert_eq!(selected(&p), "adds");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_jumps_to_the_failure() {
        let root = project("open");
        let mut p = libtest_panel(&root);
        select(&mut p, "fails");
        let effects = act(&mut p, RunAction::Open);
        assert!(matches!(&effects[..], [Effect::OpenFileAt { path, line: 11, col: 8 }] if *path == root.join("src/lib.rs")), "{effects:?}");
        // An output line opens its own link; a test without one says so.
        select(&mut p, "out thread 'tests::nested::fails' panicked at src/lib.rs:12:9:");
        let effects = act(&mut p, RunAction::Open);
        assert!(matches!(&effects[..], [Effect::OpenFileAt { line: 11, .. }]), "{effects:?}");
        select(&mut p, "adds");
        let effects = act(&mut p, RunAction::Open);
        assert!(matches!(&effects[..], [Effect::Notify(_, t)] if t == "No location for this test"), "{effects:?}");
        // F4 steps through the failed tests and opens them.
        select(&mut p, "adds");
        let effects = act(&mut p, RunAction::NextProblem);
        assert_eq!(selected(&p), "fails");
        assert!(matches!(&effects[..], [Effect::OpenFileAt { .. }]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_tests_switches_to_output() {
        let root = project("switch");
        let mut p = RunPanel::default();
        p.start("cargo test", "cargo test", true);
        assert_eq!(p.view(), View::Tests);
        p.push(matched(&root, "error[E0425]: cannot find value `x` in this scope\n --> src/lib.rs:1:1\n"));
        p.finish(Status::Exited(101));
        assert_eq!(p.view(), View::Output, "a compile error has no test tree");
        let mut p = libtest_panel(&root);
        p.finish(Status::Exited(101));
        assert_eq!(p.view(), View::Tests);
        // A plain run has no tree and the toggle does nothing.
        let mut plain = RunPanel::default();
        plain.start("x", "x", false);
        act(&mut plain, RunAction::ToggleView);
        assert_eq!(plain.view(), View::Output);
        act(&mut p, RunAction::ToggleView);
        assert_eq!(p.view(), View::Output);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn screen(term: &Terminal<TestBackend>) -> Vec<String> {
        let b = term.backend().buffer();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol().to_string()).collect()).collect()
    }

    #[test]
    fn counts_in_the_header() {
        let root = project("counts");
        let mut p = libtest_panel(&root);
        p.finish(Status::Exited(101));
        let rows = screen(&draw(&mut p, 80, 10));
        assert!(rows[2].contains("Tests: 2 passed, 1 failed, 1 ignored"), "{rows:?}");
        // The tree is drawn with glyphs.
        assert!(rows[3].contains("▾ src/lib.rs"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("✓ adds")) && rows.iter().any(|r| r.contains("○ slow")), "{rows:?}");
        // The Output view has no counts.
        act(&mut p, RunAction::ToggleView);
        let rows = screen(&draw(&mut p, 80, 10));
        assert!(!rows[2].contains("Tests:"), "{rows:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tabs_and_rerun_failed_follow_the_run() {
        let root = project("tabs");
        let mut p = RunPanel::default();
        draw(&mut p, 120, 8);
        assert!(p.hit_rect("Tests").is_none() && p.hit_rect("Rerun Failed").is_none(), "a plain run has neither");
        p.start("cargo test", "cargo test", true);
        p.push(matched(&root, TWO_BINARIES));
        draw(&mut p, 120, 8);
        assert!(p.hit_rect("Output").is_some() && p.hit_rect("Tests").is_some() && p.hit_rect("Rerun Failed").is_some());
        // The tabs go before any left button.
        draw(&mut p, 80, 8);
        assert!(p.hit_rect("Tests").is_none() && p.hit_rect("Rerun Failed").is_some(), "{:?}", p.hits);
        let _ = std::fs::remove_dir_all(&root);
    }
}
