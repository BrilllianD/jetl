//! The Debug tool window: a toolbar (Resume, Pause, Step Over, Step Into, Step Out, Stop; each enabled by the
//! session's phase), a tab row of its own (Frames, Variables, Console) and the tab's content. The Console is the program's
//! output, at most `MAX_LINES` lines. Locations are underlined and open on a click, a double click or Enter, as in
//! the Run window. The selection follows new output while it sits on the last line and that line is in view.
//! Frames, while the program is paused, is a thread row (it opens the thread menu) above the selected thread's
//! stack, as rows the App made (`FrameRow`); a choice comes back as `Effect::DebugFrame`. The tab row has a row to
//! itself because the toolbar leaves no room for it at 80 columns. Variables, while paused, is the selected frame's
//! scopes as a tree (`vars::VarsView`) whose children are read when a node opens; the App asks (`debug_sync_vars`)
//! and the tree goes with the selection seq: every frame choice, thread choice and stop replaces it (`bump`). The
//! title names the configuration and the state.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::protocol::Thread;
use super::session::{Phase, SessionId};
use super::vars::{MAX_CHILDREN, VarAsk, VarItem, VarsLoad, VarsView};
use crate::actions::DebugAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Mouse};
use crate::run::matcher::RunLine;
use crate::run::panel::{MAX_LINES, draw_line, put, row_style};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::util::text::fit;

/// Where a Debug start is before any session is live. The session's phase takes over once it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStage {
    /// The build step is running.
    Building,
    /// The build ended and its executables wait for the user's choice.
    Choosing,
    /// The build step failed, or could not run.
    Failed,
    /// Stopped before the program started: cancelled, refused, or no executable.
    NotStarted,
}

/// The window's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DebugTab {
    Frames,
    Variables,
    #[default]
    Console,
}

/// The tabs in tab order.
pub const TABS: [DebugTab; 3] = [DebugTab::Frames, DebugTab::Variables, DebugTab::Console];

impl DebugTab {
    fn label(self) -> &'static str {
        match self {
            DebugTab::Frames => "Frames",
            DebugTab::Variables => "Variables",
            DebugTab::Console => "Console",
        }
    }
}

/// How a piece of a variable row is styled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    Plain,
    Dim,
    Error,
}

/// A row of the frame list, made by the App.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRow {
    /// Valid for the current stop only; 0 is a valid id.
    pub id: i64,
    pub name: String,
    /// `demo.c:9`; `None` shows as `no source`.
    pub place: Option<String>,
    /// The file and 0-based line; `None` when the frame has no file.
    pub location: Option<(PathBuf, usize)>,
}

/// A toolbar button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Resume,
    Pause,
    StepOver,
    StepInto,
    StepOut,
    Stop,
}

impl Btn {
    const ALL: [Btn; 6] = [Btn::Resume, Btn::Pause, Btn::StepOver, Btn::StepInto, Btn::StepOut, Btn::Stop];

    fn label(self) -> &'static str {
        match self {
            Btn::Resume => "Resume",
            Btn::Pause => "Pause",
            Btn::StepOver => "Step Over",
            Btn::StepInto => "Step Into",
            Btn::StepOut => "Step Out",
            Btn::Stop => "Stop",
        }
    }

    fn action(self) -> DebugAction {
        match self {
            Btn::Resume => DebugAction::Resume,
            Btn::Pause => DebugAction::Pause,
            Btn::StepOver => DebugAction::StepOver,
            Btn::StepInto => DebugAction::StepInto,
            Btn::StepOut => DebugAction::StepOut,
            Btn::Stop => DebugAction::Stop,
        }
    }

    fn enabled(self, phase: Option<Phase>, building: bool) -> bool {
        match self {
            Btn::Resume | Btn::StepOver | Btn::StepInto | Btn::StepOut => phase == Some(Phase::Suspended),
            Btn::Pause => phase == Some(Phase::Running),
            Btn::Stop => building || phase.is_some_and(|p| p != Phase::Ended),
        }
    }
}

#[derive(Debug, Default)]
pub struct DebugPanel {
    lines: VecDeque<RunLine>,
    state: ListState,
    /// The session whose output this is; `None` before the first start and after a root switch.
    session: Option<SessionId>,
    name: String,
    /// Why the program is paused, while it is.
    reason: Option<String>,
    /// Where a start stands before its session is live; `None` once it is, and for a start with no build.
    stage: Option<BuildStage>,
    /// Screen cells of the drawn toolbar buttons.
    hits: Vec<(Btn, Rect)>,
    list_rect: Rect,
    /// Screen cells of the visible links and their line.
    link_rects: Vec<(Rect, usize)>,
    tab: DebugTab,
    /// Rises on every frame choice, thread choice and stop: an answer asked under another value is stale. Only
    /// `bump` raises it.
    seq: u64,
    /// The Variables tab's tree, for the current `seq` only.
    vars: VarsView,
    /// The selected thread's choices, from `threads`.
    threads: Vec<Thread>,
    /// The thread whose stack the list shows.
    thread: Option<i64>,
    frames: Vec<FrameRow>,
    frames_state: ListState,
    /// A stack was asked for and has not come back.
    loading: bool,
    /// Screen cells of the drawn tabs.
    tab_hits: Vec<(DebugTab, Rect)>,
    thread_rect: Rect,
}

impl DebugPanel {
    /// A new session: the old output goes.
    pub fn start(&mut self, name: &str, session: SessionId) {
        self.lines.clear();
        self.state = ListState::default();
        self.session = Some(session);
        self.name = name.into();
        self.reason = None;
        self.stage = None;
        self.clear_stack();
    }

    pub fn tab(&self) -> DebugTab {
        self.tab
    }

    pub fn set_tab(&mut self, tab: DebugTab) {
        self.tab = tab;
    }

    /// The next or previous tab, wrapping.
    pub fn step_tab(&mut self, forward: bool) {
        let at = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        let n = TABS.len();
        self.tab = TABS[if forward { (at + 1) % n } else { (at + n - 1) % n }];
    }

    pub fn selection_seq(&self) -> u64 {
        self.seq
    }

    /// The selection moved: an answer asked before is stale, and so is the tree it would have filled.
    fn bump(&mut self) {
        self.seq += 1;
        self.vars = VarsView::default();
    }

    /// The selected frame's id and the seq, once per seq, while the Variables tab shows and a frame is selected.
    pub fn scopes_wanted(&mut self) -> Option<(i64, u64)> {
        if self.tab != DebugTab::Variables || !matches!(self.vars.load, VarsLoad::Idle) {
            return None;
        }
        let id = self.selected_frame()?.id;
        self.vars.load = VarsLoad::Asked;
        Some((id, self.seq))
    }

    /// The children asked for since the last call.
    pub fn take_var_asks(&mut self) -> Vec<VarAsk> {
        self.vars.tree.take_asks()
    }

    /// The scopes arrived: they are the roots, `open` of them are expanded (their children are asked), and the
    /// selection is on the first row. Only the answer to an ask counts; a second one is ignored.
    pub fn show_scopes(&mut self, items: Vec<VarItem>, open: &[usize]) {
        if !matches!(self.vars.load, VarsLoad::Asked) {
            return;
        }
        self.vars.tree.set_roots(items);
        for &i in open {
            self.vars.tree.expand(i);
        }
        self.vars.load = VarsLoad::Shown;
        self.vars.list = ListState::default();
        self.vars.node = 0;
        self.vars.sync();
    }

    /// The scopes could not be read. Asked again only after the next selection.
    pub fn scopes_failed(&mut self, message: String) {
        if matches!(self.vars.load, VarsLoad::Asked) {
            self.vars.load = VarsLoad::Failed(message);
        }
    }

    pub fn show_children(&mut self, node: usize, items: Vec<VarItem>) {
        self.vars.show_children(node, items);
        self.vars.sync();
    }

    pub fn children_failed(&mut self, node: usize, message: String) {
        self.vars.tree.set_failed(node, message);
        self.vars.sync();
    }

    /// Threads, thread and frames go; a stack answer asked before is stale from now on.
    pub fn clear_stack(&mut self) {
        self.threads.clear();
        self.thread = None;
        self.frames.clear();
        self.frames_state = ListState::default();
        self.loading = false;
        self.bump();
    }

    /// A stop begins: the old stack goes, and the stack of `thread` is awaited.
    pub fn stop_began(&mut self, thread: Option<i64>) {
        self.clear_stack();
        self.thread = thread;
        self.loading = true;
    }

    /// A stop with no `threadId`: the thread is known now; the stack is still awaited.
    pub fn set_thread(&mut self, thread: i64) {
        self.thread = Some(thread);
        self.loading = true;
    }

    pub fn thread(&self) -> Option<i64> {
        self.thread
    }

    pub fn set_threads(&mut self, threads: Vec<Thread>) {
        self.threads = threads;
    }

    pub fn threads(&self) -> &[Thread] {
        &self.threads
    }

    /// The user chose another thread: its stack is awaited. Returns the new seq, which that answer has to carry.
    pub fn select_thread(&mut self, thread: i64) -> u64 {
        self.thread = Some(thread);
        self.frames.clear();
        self.frames_state = ListState::default();
        self.loading = true;
        self.bump();
        self.seq
    }

    /// No stack will come, as no thread is known to ask: the tab stops waiting and says there are no frames.
    pub fn stack_unavailable(&mut self) {
        self.loading = false;
    }

    /// The stack of `thread` arrived (possibly empty): row 0 is selected. The seq stays.
    pub fn show_stack(&mut self, thread: i64, frames: Vec<FrameRow>) {
        self.thread = Some(thread);
        self.frames = frames;
        self.frames_state = ListState::default();
        self.frames_state.set_len(self.frames.len());
        self.loading = false;
    }

    pub fn frame(&self, index: usize) -> Option<&FrameRow> {
        self.frames.get(index)
    }

    pub fn selected_frame(&self) -> Option<&FrameRow> {
        self.frames.get(self.frames_state.selected)
    }

    /// Alt+F10 shows the execution point: when row 0 is that place and another row is selected, select it.
    pub fn select_top_at(&mut self, path: &Path, line: usize) {
        let at_top = self.frames.first().and_then(|f| f.location.as_ref()).is_some_and(|(p, l)| p == path && *l == line);
        if at_top && self.frames_state.selected != 0 {
            self.frames_state.select(0);
            self.bump();
        }
    }

    /// Where a menu opens: under the thread row, else under the first tab, else the corner. The menu draws one row
    /// below its anchor. Off the Frames tab the thread row is not drawn yet, but it will be, right under the tab row.
    pub fn menu_anchor(&self) -> (u16, u16) {
        if self.thread_rect.width > 0 {
            (self.thread_rect.x, self.thread_rect.y)
        } else if let Some((_, r)) = self.tab_hits.first() {
            (r.x, r.y + u16::from(self.thread.is_some()))
        } else {
            (0, 0)
        }
    }

    pub fn set_stage(&mut self, stage: Option<BuildStage>) {
        self.stage = stage;
    }

    /// The start is not over: the build runs, or its executables wait for a choice. Stop works then.
    fn pending(&self) -> bool {
        matches!(self.stage, Some(BuildStage::Building | BuildStage::Choosing))
    }

    /// Back to the state before any session.
    pub fn clear(&mut self) {
        let seq = self.seq;
        *self = Self::default();
        self.seq = seq;
        self.bump();
    }

    pub fn session(&self) -> Option<SessionId> {
        self.session
    }

    pub fn set_reason(&mut self, reason: Option<String>) {
        self.reason = reason;
    }

    pub fn push(&mut self, lines: Vec<RunLine>) {
        self.lines.extend(lines);
        let dropped = self.lines.len().saturating_sub(MAX_LINES);
        self.lines.drain(..dropped);
        self.state.grew(self.lines.len(), dropped);
    }

    /// ` Debug `, or ` Debug: <name> · <state> `. `phase` is the live session's; none after it ended.
    pub fn title(&self, phase: Option<Phase>) -> String {
        if self.session.is_none() {
            return " Debug ".into();
        }
        let state = match phase {
            Some(Phase::Initializing | Phase::Starting) => "starting".to_string(),
            Some(Phase::Running) => "running".to_string(),
            Some(Phase::Suspended) => match &self.reason {
                Some(r) => format!("paused ({r})"),
                None => "paused".to_string(),
            },
            Some(Phase::Stopping) => "stopping".to_string(),
            Some(Phase::Ended) | None => match self.stage {
                Some(BuildStage::Building) => "building",
                Some(BuildStage::Choosing) => "choosing an executable",
                Some(BuildStage::Failed) => "build failed",
                Some(BuildStage::NotStarted) => "not started",
                None => "ended",
            }
            .to_string(),
        };
        format!(" Debug: {} · {state} ", self.name)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, phase: Option<Phase>) {
        let title = self.title(phase);
        let block = tool_window_block(&title, focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        let building = self.pending();
        self.hits.clear();
        self.tab_hits.clear();
        self.thread_rect = Rect::default();
        self.link_rects.clear();
        self.list_rect = Rect::default();
        if r.height == 0 || r.width < 4 {
            return;
        }
        let buf = frame.buffer_mut();
        let right = r.right();
        buf.set_style(Rect::new(r.x, r.y, r.width, 1), Style::default().bg(theme.panel_bg));
        let mut x = r.x;
        for btn in Btn::ALL {
            let text = format!(" {} ", btn.label());
            if x + 1 + text.width() as u16 > right {
                break;
            }
            let style = if btn.enabled(phase, building) { theme.title.bg(theme.panel_bg) } else { theme.dim.bg(theme.panel_bg) };
            x += 1;
            let rect = put(buf, &mut x, r.y, right, &text, style);
            self.hits.push((btn, rect));
        }
        if r.height < 2 {
            return;
        }
        let tabs_y = r.y + 1;
        buf.set_style(Rect::new(r.x, tabs_y, r.width, 1), Style::default().bg(theme.panel_bg));
        let mut tx = r.x;
        for tab in TABS {
            let style = if tab == self.tab { theme.tab_active } else { theme.tab };
            let rect = put(buf, &mut tx, tabs_y, right, &format!(" {} ", tab.label()), style);
            self.tab_hits.push((tab, rect));
        }
        if r.height < 3 {
            return;
        }
        let content = Rect::new(r.x, r.y + 2, r.width, r.height - 2);
        match self.tab {
            DebugTab::Frames => self.render_frames(buf, content, focused, theme, phase),
            DebugTab::Variables => self.render_vars(buf, content, focused, theme, phase),
            DebugTab::Console => self.render_console(buf, content, focused, theme),
        }
    }

    fn render_console(&mut self, buf: &mut ratatui::buffer::Buffer, content: Rect, focused: bool, theme: &Theme) {
        let right = content.right();
        let list = content;
        self.list_rect = list;
        self.state.fit(self.lines.len(), list.height as usize);
        for (vi, i) in self.state.visible().enumerate() {
            let y = list.y + vi as u16;
            let base = row_style(i == self.state.selected, focused, theme);
            if i == self.state.selected {
                buf.set_style(Rect::new(list.x, y, list.width, 1), base);
            }
            draw_line(buf, &self.lines[i], list.x + 1, y, right, base, theme, &mut self.link_rects, i);
        }
        if self.lines.is_empty() {
            let hint = if self.session.is_none() { "Shift+F9 starts the selected configuration" } else { "No output yet" };
            buf.set_stringn(list.x + 1, list.y, hint, list.width.saturating_sub(1) as usize, theme.dim);
        }
    }

    /// The Variables tab: the selected frame's scopes as a tree. Only while paused; the tree is valid for one
    /// selection only.
    fn render_vars(&mut self, buf: &mut ratatui::buffer::Buffer, content: Rect, focused: bool, theme: &Theme, phase: Option<Phase>) {
        let hint = |buf: &mut ratatui::buffer::Buffer, text: &str, style: Style| {
            buf.set_stringn(content.x + 1, content.y, text, content.width.saturating_sub(1) as usize, style);
        };
        if self.session.is_none() {
            return hint(buf, "Shift+F9 starts the selected configuration", theme.dim);
        }
        if phase != Some(Phase::Suspended) {
            return hint(buf, "Variables show while the program is paused", theme.dim);
        }
        self.list_rect = content;
        let rows = self.vars.sync();
        self.vars.list.fit(rows.len(), content.height as usize);
        if rows.is_empty() {
            return match &self.vars.load {
                VarsLoad::Failed(m) => hint(buf, &format!("Could not read the variables: {m}"), theme.error),
                VarsLoad::Shown => hint(buf, "No variables", theme.dim),
                VarsLoad::Idle | VarsLoad::Asked if !self.frames.is_empty() || self.loading => {
                    hint(buf, "Reading the variables…", theme.dim)
                }
                VarsLoad::Idle | VarsLoad::Asked => hint(buf, "No frames", theme.dim),
            };
        }
        let right = content.right();
        for (vi, i) in self.vars.list.visible().enumerate() {
            let y = content.y + vi as u16;
            let selected = i == self.vars.list.selected;
            let base = row_style(selected, focused, theme);
            if selected {
                buf.set_style(Rect::new(content.x, y, content.width, 1), base);
            }
            let node = rows[i];
            let depth = self.vars.tree.node(node).map_or(0, |n| n.depth);
            let mut x = content.x + 1 + (2 * depth).min(content.width as usize) as u16;
            let room = (right.saturating_sub(x) as usize).saturating_sub(2);
            for (text, kind) in self.var_pieces(node, Some(room / 2)) {
                let style = match kind {
                    Piece::Plain => base,
                    Piece::Dim if selected => base,
                    Piece::Dim => base.fg(theme.dim.fg.unwrap_or(theme.fg)),
                    Piece::Error => base.fg(theme.error.fg.unwrap_or(theme.fg)),
                };
                put(buf, &mut x, y, right, &text, style);
            }
        }
    }

    /// The pieces of a row after its indent, in order: marker, name, a loading mark, then either the failure or
    /// the note on children left out, the value and the type. `name_room` cuts a name that shares its row with a
    /// value or a type; a scope with neither has the whole row.
    fn var_pieces(&self, node: usize, name_room: Option<usize>) -> Vec<(String, Piece)> {
        let Some(n) = self.vars.tree.node(node) else { return Vec::new() };
        if let Some(parent) = self.vars.tree.more_of(node).and_then(|p| self.vars.tree.node(p)) {
            return Self::more_pieces(parent.hidden, parent.loading, n.failed.as_deref());
        }
        let item = &n.item;
        let shared = !item.value.is_empty() || !item.ty.is_empty();
        let name = match name_room {
            Some(room) if shared => fit(&item.name, room),
            _ => item.name.clone(),
        };
        let mut out = vec![(format!("{} ", self.vars.tree.marker(node)), Piece::Plain), (name, Piece::Plain)];
        if n.loading {
            out.push((" …".into(), Piece::Plain));
        }
        if let Some(m) = &n.failed {
            out.push((format!("  could not read: {m}"), Piece::Error));
            return out;
        }
        if n.hidden > 0 {
            let kept = n.children.as_ref().map_or(0, Vec::len);
            out.push((format!("  first {kept} of {} shown", kept + n.hidden), Piece::Dim));
        }
        if !item.value.is_empty() {
            out.push((format!(" = {}", item.value), Piece::Plain));
        }
        if !item.ty.is_empty() {
            out.push((format!("  {}", item.ty), Piece::Dim));
        }
        out
    }

    /// The pieces of a load-more row after its indent: a blank marker, what it will read, and what is left or why
    /// the last page failed. `hidden` and `loading` are the container's, `failed` is the row's.
    fn more_pieces(hidden: usize, loading: bool, failed: Option<&str>) -> Vec<(String, Piece)> {
        let page = hidden.min(MAX_CHILDREN);
        let mut out = vec![("  ".to_string(), Piece::Plain)];
        if loading {
            out.push((format!("reading {page} more …"), Piece::Plain));
            return out;
        }
        out.push((format!("load {page} more"), Piece::Plain));
        if let Some(m) = failed {
            out.push((format!("  could not read: {m}"), Piece::Error));
        } else if hidden > MAX_CHILDREN {
            out.push((format!("  {hidden} left"), Piece::Dim));
        }
        out
    }

    /// The Frames tab: the thread row, then the stack. Only while paused; the rows are valid for that stop only.
    fn render_frames(&mut self, buf: &mut ratatui::buffer::Buffer, content: Rect, focused: bool, theme: &Theme, phase: Option<Phase>) {
        let hint = |buf: &mut ratatui::buffer::Buffer, text: &str| {
            buf.set_stringn(content.x + 1, content.y, text, content.width.saturating_sub(1) as usize, theme.dim);
        };
        if self.session.is_none() {
            return hint(buf, "Shift+F9 starts the selected configuration");
        }
        if phase != Some(Phase::Suspended) {
            return hint(buf, "Frames show while the program is paused");
        }
        let mut list = content;
        if let Some(id) = self.thread {
            let name = self.threads.iter().find(|t| t.id == id).map(|t| t.name.as_str()).filter(|n| !n.is_empty());
            let text = match name {
                Some(n) => format!(" Thread {id}: {n} ▾ "),
                None => format!(" Thread {id} ▾ "),
            };
            let mut x = content.x;
            self.thread_rect = put(buf, &mut x, content.y, content.right(), &text, theme.tab_active);
            list = Rect::new(content.x, content.y + 1, content.width, content.height - 1);
        }
        if list.height == 0 {
            return;
        }
        self.list_rect = list;
        self.frames_state.fit(self.frames.len(), list.height as usize);
        if self.frames.is_empty() {
            let text = if self.loading { "Reading the stack…" } else { "No frames" };
            buf.set_stringn(list.x + 1, list.y, text, list.width.saturating_sub(1) as usize, theme.dim);
            return;
        }
        let right = list.right();
        for (vi, i) in self.frames_state.visible().enumerate() {
            let y = list.y + vi as u16;
            let selected = i == self.frames_state.selected;
            let base = row_style(selected, focused, theme);
            if selected {
                buf.set_style(Rect::new(list.x, y, list.width, 1), base);
            }
            let f = &self.frames[i];
            let mut x = list.x + 1;
            put(buf, &mut x, y, right, &f.name, base);
            let place_style = if selected { base } else { base.fg(theme.dim.fg.unwrap_or(theme.fg)) };
            put(buf, &mut x, y, right, &format!("  {}", f.place.as_deref().unwrap_or("no source")), place_style);
        }
    }

    /// A press on a button performs its action when the phase allows it, and is consumed either way.
    pub fn handle_mouse(&mut self, m: &Mouse, phase: Option<Phase>, cx: &mut Ctx) -> bool {
        let pos = (m.x, m.y).into();
        let building = self.pending();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if self.hits.iter().any(|(_, r)| r.contains(pos)) => {
                if let Some((btn, _)) = self.hits.iter().find(|(_, r)| r.contains(pos)).copied()
                    && btn.enabled(phase, building)
                {
                    cx.effects.push(Effect::Perform(crate::actions::Action::Debug(btn.action())));
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) if self.tab_hits.iter().any(|(_, r)| r.contains(pos)) => {
                if let Some((tab, _)) = self.tab_hits.iter().find(|(_, r)| r.contains(pos)).copied() {
                    self.set_tab(tab);
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) if self.thread_rect.width > 0 && self.thread_rect.contains(pos) => {
                cx.effects.push(Effect::Perform(crate::actions::Action::Debug(DebugAction::ChooseThread)));
                true
            }
            MouseEventKind::Down(MouseButton::Left) => match self.tab {
                DebugTab::Frames => {
                    let Some(i) = self.frames_state.hit(self.list_rect, m.x, m.y) else { return false };
                    // A press on the selected row keeps the tree and its answers; only another row is a choice.
                    if i != self.frames_state.selected {
                        self.bump();
                    }
                    self.frames_state.select(i);
                    cx.effects.push(Effect::DebugFrame { index: i, focus: m.clicks == 2 });
                    true
                }
                DebugTab::Variables => self.vars_press(m),
                DebugTab::Console => {
                    let Some(i) = self.state.hit(self.list_rect, m.x, m.y) else { return false };
                    self.state.select(i);
                    if m.clicks == 2 || self.link_rects.iter().any(|(r, _)| r.contains(pos)) {
                        self.open(i, cx);
                    }
                    true
                }
            },
            MouseEventKind::ScrollUp => {
                self.list_state_mut().scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.list_state_mut().scroll(3);
                true
            }
            _ => false,
        }
    }

    fn list_state_mut(&mut self) -> &mut ListState {
        match self.tab {
            DebugTab::Frames => &mut self.frames_state,
            DebugTab::Variables => &mut self.vars.list,
            DebugTab::Console => &mut self.state,
        }
    }

    /// The Frames tab's keys: a move that changes the selection chooses that frame; Enter chooses it and takes focus.
    fn frames_action(&mut self, a: &DebugAction, cx: &mut Ctx) -> bool {
        self.frames_state.set_len(self.frames.len());
        let page = self.frames_state.viewport.max(1);
        let before = self.frames_state.selected;
        match a {
            DebugAction::Up => self.frames_state.up(1),
            DebugAction::Down => self.frames_state.down(1),
            DebugAction::PageUp => self.frames_state.up(page),
            DebugAction::PageDown => self.frames_state.down(page),
            DebugAction::Home => self.frames_state.home(),
            DebugAction::End => self.frames_state.end(),
            DebugAction::Open => {
                if !self.frames.is_empty() {
                    cx.effects.push(Effect::DebugFrame { index: self.frames_state.selected, focus: true });
                }
                return true;
            }
            _ => return false,
        }
        if self.frames_state.selected != before {
            self.bump();
            cx.effects.push(Effect::DebugFrame { index: self.frames_state.selected, focus: false });
        }
        true
    }

    /// The Variables tab's keys. Moves only select; no key here chooses a frame or opens a file.
    fn vars_action(&mut self, a: &DebugAction) -> bool {
        let rows = self.vars.sync();
        let page = self.vars.list.viewport.max(1);
        match a {
            DebugAction::Up => self.vars.list.up(1),
            DebugAction::Down => self.vars.list.down(1),
            DebugAction::PageUp => self.vars.list.up(page),
            DebugAction::PageDown => self.vars.list.down(page),
            DebugAction::Home => self.vars.list.home(),
            DebugAction::End => self.vars.list.end(),
            DebugAction::Open => self.vars_toggle(),
            DebugAction::Expand => {
                let node = self.vars.node;
                match self.vars.tree.marker(node) {
                    '▸' => self.vars.tree.expand(node),
                    '▾' => {
                        let first = self.vars.tree.node(node).and_then(|n| n.children.as_ref()).and_then(|c| c.first().copied());
                        if let Some(c) = first {
                            self.vars.node = c;
                        }
                    }
                    _ => {
                        self.vars.tree.load_more(node);
                    }
                }
            }
            DebugAction::Collapse => {
                let node = self.vars.node;
                if self.vars.tree.marker(node) == '▾' {
                    self.vars.tree.collapse(node);
                } else if let Some(parent) = self.vars.tree.node(node).and_then(|n| n.parent) {
                    self.vars.node = parent;
                }
            }
            _ => return false,
        }
        if matches!(
            a,
            DebugAction::Up | DebugAction::Down | DebugAction::PageUp | DebugAction::PageDown | DebugAction::Home | DebugAction::End
        ) && let Some(&n) = rows.get(self.vars.list.selected)
        {
            self.vars.node = n;
        }
        self.vars.sync();
        true
    }

    /// Opens a closed node and closes an open one; a leaf stays. On a load-more row, reads the next page.
    fn vars_toggle(&mut self) {
        let node = self.vars.node;
        match self.vars.tree.marker(node) {
            '▾' => self.vars.tree.collapse(node),
            '▸' => self.vars.tree.expand(node),
            _ => {
                self.vars.tree.load_more(node);
            }
        }
    }

    /// A press in the Variables list selects the row. Anywhere on a load-more row it reads the next page; on another
    /// row's marker, two cells, it opens or closes the node.
    fn vars_press(&mut self, m: &Mouse) -> bool {
        let rows = self.vars.sync();
        let Some(i) = self.vars.list.hit(self.list_rect, m.x, m.y) else { return false };
        let Some(&node) = rows.get(i) else { return false };
        self.vars.list.select(i);
        self.vars.node = node;
        if self.vars.tree.load_more(node) {
            self.vars.sync();
            return true;
        }
        let depth = self.vars.tree.node(node).map_or(0, |n| n.depth);
        let at = self.list_rect.x + 1 + (2 * depth).min(self.list_rect.width as usize) as u16;
        if (m.x == at || m.x == at + 1) && self.vars.tree.marker(node) != ' ' {
            self.vars_toggle();
        }
        self.vars.sync();
        true
    }

    pub fn handle_action(&mut self, a: &DebugAction, cx: &mut Ctx) -> bool {
        match self.tab {
            DebugTab::Frames => self.frames_action(a, cx),
            DebugTab::Variables => self.vars_action(a),
            DebugTab::Console => self.console_action(a, cx),
        }
    }

    fn console_action(&mut self, a: &DebugAction, cx: &mut Ctx) -> bool {
        self.state.set_len(self.lines.len());
        let page = self.state.viewport.max(1);
        match a {
            DebugAction::Up => self.state.up(1),
            DebugAction::Down => self.state.down(1),
            DebugAction::PageUp => self.state.up(page),
            DebugAction::PageDown => self.state.down(page),
            DebugAction::Home => self.state.home(),
            DebugAction::End => self.state.end(),
            DebugAction::Open => self.open(self.state.selected, cx),
            _ => return false,
        }
        true
    }

    /// Opens line `i`'s own location; a line without one does nothing.
    fn open(&self, i: usize, cx: &mut Ctx) {
        let Some(l) = self.lines.get(i).and_then(|l| l.link.as_ref()) else { return };
        cx.effects.push(Effect::OpenFileAt { path: l.path.clone(), line: l.line, col: l.col });
    }

    /// The cells of the toolbar button labelled `label`.
    #[cfg(test)]
    pub fn hit_rect(&self, label: &str) -> Option<Rect> {
        self.hits.iter().find(|(b, _)| b.label() == label).map(|(_, r)| *r)
    }

    /// How many rows the Variables tree has.
    #[cfg(test)]
    pub fn var_count(&self) -> usize {
        self.vars.tree.rows().len()
    }

    /// The Variables tab's rows as text, indent and marker included, names uncut.
    #[cfg(test)]
    pub fn var_rows(&mut self) -> Vec<String> {
        let rows = self.vars.sync();
        rows.into_iter()
            .map(|r| {
                let depth = self.vars.tree.node(r).map_or(0, |n| n.depth);
                let text: String = self.var_pieces(r, None).into_iter().map(|(t, _)| t).collect();
                format!("{}{text}", " ".repeat(2 * depth))
            })
            .collect()
    }

    /// The Variables tab's selected row as text, trimmed of its indent.
    #[cfg(test)]
    pub fn var_selected(&mut self) -> String {
        let rows = self.var_rows();
        rows.get(self.vars.list.selected).map(|r| r.trim().to_string()).unwrap_or_default()
    }

    /// The Console's severities, line by line.
    #[cfg(test)]
    pub fn severities(&self) -> Vec<Option<crate::run::matcher::Severity>> {
        self.lines.iter().map(|l| l.severity).collect()
    }

    /// The Console's lines.
    #[cfg(test)]
    pub fn texts(&self) -> Vec<&str> {
        self.lines.iter().map(|l| l.text.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::actions::Action;
    use crate::run::matcher::{Link, Severity};

    const PHASES: [Option<Phase>; 7] = [
        None,
        Some(Phase::Initializing),
        Some(Phase::Starting),
        Some(Phase::Running),
        Some(Phase::Suspended),
        Some(Phase::Stopping),
        Some(Phase::Ended),
    ];

    fn started() -> DebugPanel {
        let mut p = DebugPanel::default();
        p.start("demo", SessionId(1));
        p
    }

    fn draw(p: &mut DebugPanel, w: u16, h: u16, phase: Option<Phase>) -> Terminal<TestBackend> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| p.render(f, f.area(), true, &theme, phase)).unwrap();
        term
    }

    /// Runs `f` with a throwaway `Ctx`; the effects it left.
    fn with_cx(f: impl FnOnce(&mut Ctx)) -> Vec<Effect> {
        let (config, theme, workspace) =
            (crate::config::Config::default(), Theme::default(), crate::workspace::Workspace::new("/tmp".into()));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        f(&mut cx);
        cx.effects
    }

    fn press(x: u16, y: u16, clicks: u8) -> Mouse {
        Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks, mods: KeyModifiers::NONE }
    }

    fn linked(text: &str, line: usize, col: usize) -> RunLine {
        let link = Link { path: PathBuf::from("/tmp/a.c"), line, col, span: 0..text.len() };
        RunLine { text: text.into(), severity: None, link: Some(link) }
    }

    #[test]
    fn console_is_capped_and_follows_unless_scrolled_up() {
        let mut p = started();
        p.push((0..MAX_LINES + 10).map(|i| RunLine::plain(i.to_string())).collect());
        assert_eq!(p.lines.len(), MAX_LINES);
        assert_eq!(p.lines[0].text, "10", "the oldest lines go");
        assert_eq!(p.state.selected, MAX_LINES - 1, "follows the end");
        p.state.fit(MAX_LINES, 10);
        p.state.up(1);
        p.push(vec![RunLine::plain("x"), RunLine::plain("y")]);
        assert_eq!(p.lines[p.state.selected].text, (MAX_LINES + 8).to_string(), "scrolled up: stays on its line");
    }

    #[test]
    fn buttons_perform_by_phase() {
        let want = |btn: Btn, phase: Option<Phase>| -> Option<DebugAction> {
            let ok = match btn {
                Btn::Resume | Btn::StepOver | Btn::StepInto | Btn::StepOut => phase == Some(Phase::Suspended),
                Btn::Pause => phase == Some(Phase::Running),
                Btn::Stop => {
                    matches!(phase, Some(Phase::Initializing | Phase::Starting | Phase::Running | Phase::Suspended | Phase::Stopping))
                }
            };
            ok.then_some(match btn {
                Btn::Resume => DebugAction::Resume,
                Btn::Pause => DebugAction::Pause,
                Btn::StepOver => DebugAction::StepOver,
                Btn::StepInto => DebugAction::StepInto,
                Btn::StepOut => DebugAction::StepOut,
                Btn::Stop => DebugAction::Stop,
            })
        };
        for btn in Btn::ALL {
            for phase in PHASES {
                let mut p = started();
                draw(&mut p, 100, 6, phase);
                let r = p.hit_rect(btn.label()).unwrap_or_else(|| panic!("{} not drawn", btn.label()));
                let mut handled = false;
                let fx = with_cx(|cx| handled = p.handle_mouse(&press(r.x, r.y, 1), phase, cx));
                assert!(handled, "{} in {phase:?}: a press on a button is consumed", btn.label());
                let got: Vec<DebugAction> = fx
                    .into_iter()
                    .map(|e| match e {
                        Effect::Perform(Action::Debug(a)) => a,
                        e => panic!("unexpected effect {e:?}"),
                    })
                    .collect();
                assert_eq!(got, want(btn, phase).into_iter().collect::<Vec<_>>(), "{} in {phase:?}", btn.label());
            }
        }
    }

    #[test]
    fn the_title_says_the_stage() {
        let mut p = started();
        for (stage, want) in [
            (Some(BuildStage::Building), "building"),
            (Some(BuildStage::Choosing), "choosing an executable"),
            (Some(BuildStage::Failed), "build failed"),
            (Some(BuildStage::NotStarted), "not started"),
            (None, "ended"),
        ] {
            p.set_stage(stage);
            assert_eq!(p.title(None), format!(" Debug: demo · {want} "));
            assert_eq!(p.title(Some(Phase::Ended)), format!(" Debug: demo · {want} "));
            assert_eq!(p.title(Some(Phase::Running)), " Debug: demo · running ", "a live session's phase wins");
        }
        p.set_stage(Some(BuildStage::Building));
        p.start("demo", SessionId(2));
        assert_eq!(p.title(None), " Debug: demo · ended ", "start clears the stage");
    }

    #[test]
    fn stop_is_live_only_while_building() {
        for (stage, stop) in [
            (Some(BuildStage::Building), true),
            (Some(BuildStage::Choosing), true),
            (Some(BuildStage::Failed), false),
            (Some(BuildStage::NotStarted), false),
            (None, false),
        ] {
            for btn in Btn::ALL {
                let mut p = started();
                p.set_stage(stage);
                draw(&mut p, 100, 6, None);
                let r = p.hit_rect(btn.label()).unwrap();
                let fx = with_cx(|cx| assert!(p.handle_mouse(&press(r.x, r.y, 1), None, cx)));
                let want = btn == Btn::Stop && stop;
                assert_eq!(fx.len(), usize::from(want), "{} with {stage:?}: {fx:?}", btn.label());
                if want {
                    assert!(matches!(fx[0], Effect::Perform(Action::Debug(DebugAction::Stop))), "{fx:?}");
                }
            }
        }
    }

    #[test]
    fn a_link_opens_by_click_and_by_enter() {
        let mut p = started();
        p.push(vec![RunLine::plain("plain"), linked("a.c:3", 2, 4)]);
        draw(&mut p, 60, 6, Some(Phase::Running));
        // Enter on the link line.
        let fx = with_cx(|cx| {
            p.handle_action(&DebugAction::Home, cx);
            p.handle_action(&DebugAction::Down, cx);
            p.handle_action(&DebugAction::Open, cx);
        });
        assert_eq!(fx.len(), 1);
        assert!(matches!(&fx[0], Effect::OpenFileAt { line: 2, col: 4, .. }), "{:?}", fx[0]);
        // Enter on the plain line: nothing.
        let fx = with_cx(|cx| {
            p.handle_action(&DebugAction::Up, cx);
            p.handle_action(&DebugAction::Open, cx);
        });
        assert!(fx.is_empty());
        // A click on the link's cells.
        let (r, _) = p.link_rects[0];
        let fx = with_cx(|cx| {
            assert!(p.handle_mouse(&press(r.x, r.y, 1), Some(Phase::Running), cx));
        });
        assert!(matches!(fx.as_slice(), [Effect::OpenFileAt { line: 2, col: 4, .. }]), "{fx:?}");
        // A single click on the plain line selects only.
        let plain_y = p.list_rect.y;
        let fx = with_cx(|cx| {
            assert!(p.handle_mouse(&press(p.list_rect.x + 30, plain_y, 1), Some(Phase::Running), cx));
        });
        assert!(fx.is_empty());
        assert_eq!(p.state.selected, 0);
    }

    #[test]
    fn title_names_the_configuration_and_the_phase() {
        let mut p = DebugPanel::default();
        assert_eq!(p.title(None), " Debug ");
        assert_eq!(p.title(Some(Phase::Running)), " Debug ", "no session yet");
        p.start("demo", SessionId(1));
        for (phase, want) in [
            (Some(Phase::Initializing), "starting"),
            (Some(Phase::Starting), "starting"),
            (Some(Phase::Running), "running"),
            (Some(Phase::Suspended), "paused"),
            (Some(Phase::Stopping), "stopping"),
            (Some(Phase::Ended), "ended"),
            (None, "ended"),
        ] {
            assert_eq!(p.title(phase), format!(" Debug: demo · {want} "));
        }
        p.set_reason(Some("breakpoint".into()));
        assert_eq!(p.title(Some(Phase::Suspended)), " Debug: demo · paused (breakpoint) ");
        assert_eq!(p.title(Some(Phase::Running)), " Debug: demo · running ", "the reason belongs to a pause");
        p.set_reason(None);
        assert_eq!(p.title(Some(Phase::Suspended)), " Debug: demo · paused ");
        p.clear();
        assert_eq!(p.title(None), " Debug ");
    }

    #[test]
    fn a_narrow_panel_drops_whole_buttons() {
        let mut p = started();
        let term = draw(&mut p, 30, 5, Some(Phase::Suspended));
        let inner_right = 29;
        assert!(!p.hits.is_empty() && p.hits.len() < Btn::ALL.len(), "{} buttons", p.hits.len());
        for (btn, r) in &p.hits {
            assert_eq!(r.width as usize, format!(" {} ", btn.label()).width(), "{} is clipped", btn.label());
            assert!(r.right() <= inner_right, "{} past the border", btn.label());
        }
        assert!(p.hit_rect("Stop").is_none());
        let row: String = (0..30).map(|x| term.backend().buffer()[(x, 1)].symbol().to_string()).collect();
        assert!(!row.contains("Step Into"), "{row}");
    }

    #[test]
    fn tiny_areas_do_not_panic() {
        for h in 0..=3 {
            for w in 0..=5 {
                let mut p = started();
                p.push(vec![RunLine::plain("x")]);
                draw(&mut p, w, h, Some(Phase::Running));
            }
        }
        // Three rows leave the toolbar one row and the list none: the hint must not be written over the border.
        let term = draw(&mut started(), 30, 3, Some(Phase::Running));
        let bottom: String = (0..30).map(|x| term.backend().buffer()[(x, 2)].symbol().to_string()).collect();
        assert!(bottom.starts_with('└') && !bottom.contains("No output"), "{bottom:?}");
        // Every size, on both tabs, paused with rows.
        for tab in TABS {
            for h in 0..=6 {
                for w in 0..=5 {
                    let mut p = suspended_with(vec![row(0, "add", Some("demo.c:4")), row(1, "main", None)]);
                    p.push(vec![RunLine::plain("x")]);
                    p.set_tab(tab);
                    if tab == DebugTab::Variables {
                        fill_vars(&mut p);
                    }
                    draw(&mut p, w, h, Some(Phase::Suspended));
                }
            }
            // Four rows leave the border, the toolbar and the tabs: the bottom row is border only.
            let mut p = suspended_with(vec![row(0, "add", Some("demo.c:4"))]);
            p.set_tab(tab);
            if tab == DebugTab::Variables {
                fill_vars(&mut p);
            }
            let term = draw(&mut p, 30, 4, Some(Phase::Suspended));
            assert_eq!(row_text(&term, 3, 30), format!("└{}┘", "─".repeat(28)), "{tab:?}");
        }
        // Positive control: two rows more show a hint.
        let term = draw(&mut started(), 30, 6, Some(Phase::Running));
        assert!(row_text(&term, 3, 30).contains("No output yet"), "{}", row_text(&term, 3, 30));
        let mut p = started();
        p.stop_began(Some(1));
        p.set_tab(DebugTab::Frames);
        let term = draw(&mut p, 30, 6, Some(Phase::Suspended));
        assert!(row_text(&term, 4, 30).contains("Reading the stack"), "{}", row_text(&term, 4, 30));
    }

    fn row(id: i64, name: &str, place: Option<&str>) -> FrameRow {
        FrameRow { id, name: name.into(), place: place.map(Into::into), location: place.map(|_| (PathBuf::from("/tmp/demo.c"), 0)) }
    }

    /// Paused on thread 1, `rows` listed, the Frames tab up.
    fn suspended_with(rows: Vec<FrameRow>) -> DebugPanel {
        let mut p = started();
        p.stop_began(Some(1));
        p.set_threads(vec![Thread { id: 1, name: "t".into() }]);
        p.show_stack(1, rows);
        p.set_tab(DebugTab::Frames);
        p
    }

    fn row_text(term: &Terminal<TestBackend>, y: u16, w: u16) -> String {
        (0..w).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect()
    }

    fn three() -> Vec<FrameRow> {
        vec![row(0, "add", Some("demo.c:4")), row(1, "main", Some("demo.c:9")), row(2, "start", None)]
    }

    #[test]
    fn tabs_switch_by_click_and_step() {
        let mut p = started();
        draw(&mut p, 60, 8, Some(Phase::Running));
        assert_eq!(p.tab(), DebugTab::Console);
        let at = |p: &DebugPanel, tab| p.tab_hits.iter().find(|(t, _)| *t == tab).map(|(_, r)| *r).unwrap();
        let r = at(&p, DebugTab::Frames);
        let mut handled = false;
        let fx = with_cx(|cx| handled = p.handle_mouse(&press(r.x, r.y, 1), Some(Phase::Running), cx));
        assert!(handled && fx.is_empty(), "{fx:?}");
        assert_eq!(p.tab(), DebugTab::Frames);
        draw(&mut p, 60, 8, Some(Phase::Running));
        let r = at(&p, DebugTab::Console);
        let fx = with_cx(|cx| assert!(p.handle_mouse(&press(r.x, r.y, 1), Some(Phase::Running), cx)));
        assert!(fx.is_empty(), "{fx:?}");
        assert_eq!(p.tab(), DebugTab::Console);
        draw(&mut p, 60, 8, Some(Phase::Running));
        let r = at(&p, DebugTab::Variables);
        let fx = with_cx(|cx| assert!(p.handle_mouse(&press(r.x, r.y, 1), Some(Phase::Running), cx)));
        assert!(fx.is_empty(), "{fx:?}");
        assert_eq!(p.tab(), DebugTab::Variables);
        p.set_tab(DebugTab::Console);
        p.step_tab(true);
        assert_eq!(p.tab(), DebugTab::Frames, "wraps forward");
        p.step_tab(true);
        assert_eq!(p.tab(), DebugTab::Variables);
        p.step_tab(true);
        assert_eq!(p.tab(), DebugTab::Console);
        p.step_tab(false);
        assert_eq!(p.tab(), DebugTab::Variables);
        p.step_tab(false);
        assert_eq!(p.tab(), DebugTab::Frames);
        p.step_tab(false);
        assert_eq!(p.tab(), DebugTab::Console, "wraps back");
    }

    #[test]
    fn the_tab_row_is_under_the_toolbar() {
        let mut p = started();
        p.push(vec![RunLine::plain("hello")]);
        let term = draw(&mut p, 60, 5, Some(Phase::Running));
        assert!(row_text(&term, 1, 60).contains("Resume"), "{}", row_text(&term, 1, 60));
        assert!(row_text(&term, 2, 60).starts_with("│ Frames  Variables  Console "), "{}", row_text(&term, 2, 60));
        assert!(row_text(&term, 3, 60).contains("hello"), "{}", row_text(&term, 3, 60));
        assert_eq!(p.list_rect.y, 3);
    }

    #[test]
    fn a_frame_row_shows_its_place() {
        let long = "x".repeat(100);
        let mut p = suspended_with(vec![row(0, "add", Some("demo.c:4")), row(1, "main", None), row(2, &long, Some("demo.c:9"))]);
        let term = draw(&mut p, 40, 9, Some(Phase::Suspended));
        let add = row_text(&term, 4, 40);
        let main = row_text(&term, 5, 40);
        let clipped = row_text(&term, 6, 40);
        assert!(add.starts_with("│ add  demo.c:4"), "{add}");
        assert!(main.starts_with("│ main  no source"), "{main}");
        assert!(clipped.starts_with("│ xxx") && clipped.ends_with("x│"), "{clipped}");
        assert!(!clipped.contains("demo.c"), "the place is past the clip: {clipped}");
    }

    #[test]
    fn keys_choose_a_frame() {
        let mut p = suspended_with(three());
        draw(&mut p, 60, 10, Some(Phase::Suspended));
        let keys = |p: &mut DebugPanel, a: DebugAction| with_cx(|cx| assert!(p.handle_action(&a, cx)));
        let one = |fx: Vec<Effect>| match fx.as_slice() {
            [Effect::DebugFrame { index, focus }] => Some((*index, *focus)),
            [] => None,
            e => panic!("{e:?}"),
        };
        assert_eq!(one(keys(&mut p, DebugAction::Down)), Some((1, false)));
        assert_eq!(one(keys(&mut p, DebugAction::Down)), Some((2, false)));
        assert_eq!(one(keys(&mut p, DebugAction::Down)), None, "no move, no choice");
        assert_eq!(one(keys(&mut p, DebugAction::Home)), Some((0, false)));
        assert_eq!(one(keys(&mut p, DebugAction::Home)), None);
        assert_eq!(one(keys(&mut p, DebugAction::End)), Some((2, false)));
        assert_eq!(one(keys(&mut p, DebugAction::Up)), Some((1, false)));
        assert_eq!(one(keys(&mut p, DebugAction::Open)), Some((1, true)), "Enter focuses the editor");
        // The Console's keys stay the Console's.
        p.push(vec![RunLine::plain("a"), RunLine::plain("b"), RunLine::plain("c")]);
        p.set_tab(DebugTab::Console);
        let frames_at = p.frames_state.selected;
        assert_eq!(one(keys(&mut p, DebugAction::Home)), None);
        assert_eq!(one(keys(&mut p, DebugAction::Down)), None);
        assert_eq!((p.state.selected, p.frames_state.selected), (1, frames_at));
    }

    #[test]
    fn clicks_choose_a_frame() {
        let mut p = suspended_with(three());
        draw(&mut p, 60, 10, Some(Phase::Suspended));
        let list = p.list_rect;
        let click = |p: &mut DebugPanel, row: u16, clicks: u8| -> (bool, Vec<Effect>) {
            let mut handled = false;
            let fx = with_cx(|cx| handled = p.handle_mouse(&press(list.x + 3, list.y + row, clicks), Some(Phase::Suspended), cx));
            (handled, fx)
        };
        let one = |fx: Vec<Effect>| match fx.as_slice() {
            [Effect::DebugFrame { index, focus }] => (*index, *focus),
            e => panic!("{e:?}"),
        };
        let (handled, fx) = click(&mut p, 1, 1);
        assert!(handled);
        assert_eq!(one(fx), (1, false));
        let chosen = p.selection_seq();
        let (_, fx) = click(&mut p, 1, 1);
        assert_eq!(one(fx), (1, false), "also on the selected row: it brings the frame back");
        assert_eq!(p.selection_seq(), chosen, "but it is no new choice: the variables and their answers stay");
        let (_, fx) = click(&mut p, 2, 2);
        assert_eq!(one(fx), (2, true), "a double click focuses the editor");
        assert!(p.selection_seq() > chosen, "another row is a choice");
        let (handled, fx) = click(&mut p, 3, 1);
        assert!(!handled && fx.is_empty(), "below the rows: {fx:?}");
        // While a stop's own answer is pending the list is empty: there is no row to press, so a press cannot
        // end that stop (the seq it carries stays current).
        p.stop_began(Some(1));
        let waiting = p.selection_seq();
        let (handled, fx) = click(&mut p, 0, 1);
        assert!(!handled && fx.is_empty() && p.selection_seq() == waiting, "{fx:?}");
    }

    #[test]
    fn every_choice_moves_the_seq() {
        let mut p = suspended_with(three());
        draw(&mut p, 60, 10, Some(Phase::Suspended));
        let seq = |p: &DebugPanel| p.selection_seq();
        let s0 = seq(&p);
        with_cx(|cx| {
            p.handle_action(&DebugAction::Down, cx);
        });
        let s1 = seq(&p);
        assert!(s1 > s0, "a key choice");
        with_cx(|cx| {
            p.handle_action(&DebugAction::Down, cx);
        });
        assert_eq!(seq(&p), s1 + 1);
        with_cx(|cx| {
            p.handle_action(&DebugAction::Down, cx);
        });
        assert_eq!(seq(&p), s1 + 1, "a Down at the end chooses nothing");
        let s2 = seq(&p);
        let list = p.list_rect;
        with_cx(|cx| assert!(p.handle_mouse(&press(list.x + 3, list.y, 1), Some(Phase::Suspended), cx)));
        assert!(seq(&p) > s2, "a click");
        let s3 = seq(&p);
        p.show_stack(1, three());
        p.set_threads(vec![Thread { id: 1, name: "t".into() }]);
        p.set_tab(DebugTab::Console);
        p.set_tab(DebugTab::Frames);
        p.set_thread(1);
        assert_eq!(seq(&p), s3, "stack answers, tabs and a known thread do not count");
        assert!(p.select_thread(4) > s3, "a thread choice");
        let s4 = seq(&p);
        p.clear_stack();
        assert!(seq(&p) > s4, "clear_stack");
        let s5 = seq(&p);
        p.stop_began(Some(1));
        assert!(seq(&p) > s5, "stop_began");
    }

    #[test]
    fn the_thread_row_opens_the_menu() {
        let mut p = suspended_with(three());
        let term = draw(&mut p, 60, 10, Some(Phase::Suspended));
        assert!(row_text(&term, 3, 60).contains(" Thread 1: t ▾ "), "{}", row_text(&term, 3, 60));
        let r = p.thread_rect;
        assert!(r.width > 0);
        let mut handled = false;
        let fx = with_cx(|cx| handled = p.handle_mouse(&press(r.x + 2, r.y, 1), Some(Phase::Suspended), cx));
        assert!(handled);
        assert!(matches!(fx.as_slice(), [Effect::Perform(Action::Debug(DebugAction::ChooseThread))]), "{fx:?}");
        assert_eq!(p.menu_anchor(), (r.x, r.y), "the menu draws one row below its anchor itself");
        // Only while paused.
        for phase in PHASES.into_iter().filter(|ph| *ph != Some(Phase::Suspended)) {
            draw(&mut p, 60, 10, phase);
            assert_eq!(p.thread_rect, Rect::default(), "{phase:?}");
        }
        // No name: no colon.
        let mut p = suspended_with(three());
        p.set_threads(vec![Thread { id: 1, name: "t".into() }, Thread { id: 7, name: String::new() }]);
        p.show_stack(7, three());
        let term = draw(&mut p, 60, 10, Some(Phase::Suspended));
        assert!(row_text(&term, 3, 60).contains(" Thread 7 ▾ "), "{}", row_text(&term, 3, 60));
        // No thread known: no row, the list starts higher.
        let mut p = started();
        p.set_tab(DebugTab::Frames);
        draw(&mut p, 60, 10, Some(Phase::Suspended));
        assert_eq!(p.thread_rect, Rect::default());
        assert_eq!(p.menu_anchor().1, p.tab_hits[0].1.y);
        // A thread known but the Console up (`t` from there switches to Frames): the thread row is the tab row's next.
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Console);
        draw(&mut p, 60, 10, Some(Phase::Suspended));
        assert_eq!(p.thread_rect, Rect::default());
        assert_eq!(p.menu_anchor().1, p.tab_hits[0].1.y + 1);
        p.set_tab(DebugTab::Frames);
        let frames = draw(&mut p, 60, 10, Some(Phase::Suspended));
        assert!(row_text(&frames, p.menu_anchor().1, 60).contains("Thread 1"), "the thread row is the anchor's");
    }

    /// The menu `debug.choose_thread` opens, drawn over the panel in a terminal `h` rows high; the thread row's
    /// `y`, and the rows of the terminal as text.
    fn menu_over_panel(h: u16) -> (u16, Vec<String>) {
        use crate::ui::menu::{ContextMenu, MenuItem};
        let theme = Theme::default();
        let mut p = suspended_with(three());
        let mut term = Terminal::new(TestBackend::new(60, h)).unwrap();
        term.draw(|f| {
            p.render(f, f.area(), true, &theme, Some(Phase::Suspended));
            let (x, y) = p.menu_anchor();
            let item = MenuItem::new("Thread 1: t", Action::Debug(DebugAction::SelectThread(1)));
            ContextMenu::new(vec![item], x, y).shifted().render(f, f.area(), &theme);
        })
        .unwrap();
        (p.thread_rect.y, (0..h).map(|y| row_text(&term, y, 60)).collect())
    }

    #[test]
    fn the_thread_menu_is_under_the_thread_row_and_clear_of_it_when_flipped() {
        // Room below: the menu's top border is the very next row.
        let (y, rows) = menu_over_panel(12);
        assert!(rows[y as usize].contains("Thread 1: t ▾"), "{rows:?}");
        assert!(rows[y as usize + 1].contains('╭'), "directly under the thread row: {rows:?}");
        // No room: the menu goes above, its bottom border on the row above, and the thread row is not covered.
        let (y, rows) = menu_over_panel(y + 3);
        assert!(rows[y as usize].contains("Thread 1: t ▾"), "the thread row stays: {rows:?}");
        assert!(rows[y as usize - 1].contains('╰'), "directly above the thread row: {rows:?}");
    }

    #[test]
    fn frames_hints_by_state() {
        let text = |p: &mut DebugPanel, phase| {
            let term = draw(p, 60, 10, phase);
            (0..10).map(|y| row_text(&term, y, 60)).collect::<Vec<_>>().join("\n")
        };
        let mut none = DebugPanel::default();
        none.set_tab(DebugTab::Frames);
        assert!(text(&mut none, None).contains("Shift+F9 starts the selected configuration"));
        let mut p = started();
        p.set_tab(DebugTab::Frames);
        for phase in [Some(Phase::Running), Some(Phase::Stopping), Some(Phase::Starting), None] {
            assert!(text(&mut p, phase).contains("Frames show while the program is paused"), "{phase:?}");
        }
        p.stop_began(None);
        p.stack_unavailable();
        assert!(text(&mut p, Some(Phase::Suspended)).contains("No frames"));
        // A retried `threads` found the thread, and its stack is on the way.
        p.set_thread(1);
        assert!(text(&mut p, Some(Phase::Suspended)).contains("Reading the stack…"));
        p.stop_began(Some(1));
        assert!(text(&mut p, Some(Phase::Suspended)).contains("Reading the stack…"));
        p.show_stack(1, Vec::new());
        let t = text(&mut p, Some(Phase::Suspended));
        assert!(t.contains("No frames") && !t.contains("Reading"), "{t}");
        // A panel that is stopping draws no rows, and takes no clicks on them.
        let mut p = suspended_with(three());
        let t = text(&mut p, Some(Phase::Stopping));
        assert!(!t.contains("add") && !t.contains("demo.c"), "{t}");
        assert_eq!(p.list_rect, Rect::default());
    }

    #[test]
    fn a_start_keeps_the_tab_and_drops_the_stack() {
        let mut p = suspended_with(three());
        p.start("again", SessionId(2));
        assert_eq!(p.tab(), DebugTab::Frames);
        assert!(p.frame(0).is_none() && p.threads().is_empty() && p.thread().is_none());
        p.clear();
        assert_eq!(p.tab(), DebugTab::Console);
    }

    #[test]
    fn the_console_grows_behind_the_frames_tab() {
        let mut p = started();
        p.push(vec![RunLine::plain("a")]);
        p.set_tab(DebugTab::Frames);
        p.push(vec![RunLine::plain("b"), RunLine::plain("c")]);
        p.set_tab(DebugTab::Console);
        assert_eq!(p.state.selected, 2, "still following the end");
        assert_eq!(p.texts(), ["a", "b", "c"]);
    }

    #[test]
    fn an_empty_console_says_so() {
        let mut p = DebugPanel::default();
        let term = draw(&mut p, 60, 5, None);
        let all: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(all.contains("Shift+F9 starts the selected configuration"));
        p.start("demo", SessionId(1));
        let term = draw(&mut p, 60, 5, Some(Phase::Running));
        let all: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(all.contains("No output yet"));
        assert!(!all.contains("Shift+F9"));
    }

    #[test]
    fn severity_colours_a_line() {
        let mut p = started();
        let mut line = RunLine::plain("bad");
        line.severity = Some(Severity::Error);
        p.push(vec![line]);
        let theme = Theme::default();
        let term = draw(&mut p, 30, 5, Some(Phase::Running));
        let cell = &term.backend().buffer()[(p.list_rect.x + 1, p.list_rect.y)];
        assert_eq!(cell.symbol(), "b");
        assert_eq!(cell.fg, theme.error.fg.unwrap());
    }

    // ---- the Variables tab

    fn vi(name: &str, value: &str, ty: &str, reference: i64) -> VarItem {
        VarItem { name: name.into(), value: value.into(), ty: ty.into(), reference, children: 0 }
    }

    /// Asks and shows the scopes of the selected frame: `Arguments` and `Locals` open, `Registers` closed.
    /// Rows: Arguments, a, b, Locals, p, arr, Registers (nodes 0, 3, 4, 1, 5, 6, 2).
    fn fill_vars(p: &mut DebugPanel) {
        let (frame, seq) = p.scopes_wanted().expect("the tab asks");
        assert_eq!((frame, seq), (p.selected_frame().unwrap().id, p.selection_seq()));
        p.show_scopes(vec![vi("Arguments", "", "", 1), vi("Locals", "", "", 2), vi("Registers", "", "", 3)], &[0, 1]);
        let asks = p.take_var_asks();
        assert_eq!(asks.iter().map(|a| (a.node, a.reference)).collect::<Vec<_>>(), [(0, 1), (1, 2)]);
        p.show_children(0, vec![vi("a", "1", "int", 0), vi("b", "2", "int", 0)]);
        let arr = VarItem { children: 5000, ..vi("arr", "", "int [5000]", 6) };
        p.show_children(1, vec![vi("p", "", "struct point", 5), arr]);
    }

    /// Paused with three frames, the Variables tab up and `fill_vars` done.
    fn with_vars() -> DebugPanel {
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        fill_vars(&mut p);
        p
    }

    fn key_in(p: &mut DebugPanel, a: DebugAction) -> (bool, Vec<Effect>) {
        let mut handled = false;
        let fx = with_cx(|cx| handled = p.handle_action(&a, cx));
        (handled, fx)
    }

    /// A row's text after its indent and marker.
    fn after_marker(row: &str) -> &str {
        row.trim_start().trim_start_matches(['▾', '▸']).trim_start()
    }

    /// Selects the row whose text starts with `name` after its indent and marker.
    fn go_to(p: &mut DebugPanel, name: &str) {
        let rows = p.vars.sync();
        let texts = p.var_rows();
        let i = texts.iter().position(|t| after_marker(t).starts_with(name)).unwrap_or_else(|| panic!("{name} in {texts:?}"));
        p.vars.list.select(i);
        p.vars.node = rows[i];
        p.vars.sync();
    }

    fn selected_text(p: &mut DebugPanel) -> String {
        let texts = p.var_rows();
        texts[p.vars.list.selected].trim().to_string()
    }

    #[test]
    fn the_variables_tab_lists_the_scopes_and_their_children() {
        let mut p = with_vars();
        assert_eq!(
            p.var_rows(),
            ["▾ Arguments", "    a = 1  int", "    b = 2  int", "▾ Locals", "  ▸ p  struct point", "  ▸ arr  int [5000]", "▸ Registers"]
        );
    }

    #[test]
    fn each_seq_rise_drops_the_tree() {
        type Rise = fn(&mut DebugPanel);
        let rises: [(&str, Rise); 6] = [
            ("clear_stack", |p| p.clear_stack()),
            ("stop_began", |p| p.stop_began(Some(1))),
            ("select_thread", |p| {
                p.select_thread(4);
            }),
            ("select_top_at moving", |p| {
                p.frames_state.select(1);
                p.select_top_at(Path::new("/tmp/demo.c"), 0);
            }),
            ("a Frames key", |p| {
                p.set_tab(DebugTab::Frames);
                draw(p, 60, 12, Some(Phase::Suspended));
                assert!(key_in(p, DebugAction::Down).1.len() == 1);
            }),
            ("a Frames click", |p| {
                p.set_tab(DebugTab::Frames);
                draw(p, 60, 12, Some(Phase::Suspended));
                let list = p.list_rect;
                with_cx(|cx| assert!(p.handle_mouse(&press(list.x + 3, list.y + 1, 1), Some(Phase::Suspended), cx)));
            }),
        ];
        for (what, rise) in rises {
            let mut p = with_vars();
            let seq = p.selection_seq();
            assert_eq!(p.var_rows().len(), 7, "{what}: the control");
            rise(&mut p);
            assert!(p.selection_seq() > seq, "{what}");
            assert!(p.var_rows().is_empty(), "{what}: the tree goes with the seq");
            assert!(matches!(p.vars.load, VarsLoad::Idle), "{what}: and is asked again");
            assert!(p.take_var_asks().is_empty(), "{what}");
        }
        // Nothing moved: nothing dropped.
        let mut p = with_vars();
        p.select_top_at(Path::new("/tmp/demo.c"), 0);
        p.select_top_at(Path::new("/tmp/elsewhere.c"), 0);
        assert_eq!(p.var_rows().len(), 7, "row 0 is selected already, and another place is not row 0's");
    }

    #[test]
    fn show_stack_keeps_the_seq() {
        let mut p = with_vars();
        let seq = p.selection_seq();
        p.show_stack(1, three());
        p.set_threads(vec![Thread { id: 1, name: "t".into() }]);
        assert_eq!(p.selection_seq(), seq);
        assert_eq!(p.var_rows().len(), 7, "the stack answer leaves the tree alone");
        assert!(matches!(p.vars.load, VarsLoad::Shown));
    }

    #[test]
    fn scopes_are_wanted_once_on_the_variables_tab() {
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Console);
        assert_eq!(p.scopes_wanted(), None, "Console");
        p.set_tab(DebugTab::Frames);
        assert_eq!(p.scopes_wanted(), None, "Frames");
        p.set_tab(DebugTab::Variables);
        let seq = p.selection_seq();
        assert_eq!(p.scopes_wanted(), Some((0, seq)));
        assert_eq!(p.scopes_wanted(), None, "once per seq");
        p.stop_began(Some(1));
        assert_eq!(p.scopes_wanted(), None, "no frames yet");
        p.show_stack(1, vec![row(5, "f", None)]);
        assert_eq!(p.scopes_wanted(), Some((5, p.selection_seq())), "asked again after a bump, with the frame's own id");
        p.set_tab(DebugTab::Console);
        p.clear_stack();
        p.set_tab(DebugTab::Variables);
        assert_eq!(p.scopes_wanted(), None, "no frames");
    }

    #[test]
    fn variables_keys() {
        let mut p = with_vars();
        draw(&mut p, 60, 12, Some(Phase::Suspended));
        let seq = p.selection_seq();
        assert_eq!(selected_text(&mut p), "▾ Arguments");
        for (a, want) in [
            (DebugAction::Down, "a = 1  int"),
            (DebugAction::PageDown, "▸ Registers"),
            (DebugAction::Up, "▸ arr  int [5000]"),
            (DebugAction::Home, "▾ Arguments"),
            (DebugAction::End, "▸ Registers"),
            (DebugAction::PageUp, "▾ Arguments"),
        ] {
            let (handled, fx) = key_in(&mut p, a.clone());
            assert!(handled && fx.is_empty(), "{a:?}: {fx:?}");
            assert_eq!(selected_text(&mut p), want, "{a:?}");
        }
        assert_eq!(p.selection_seq(), seq, "moves are not choices");
        // Right on a closed node opens it and asks once; Right on it again goes to its first child.
        go_to(&mut p, "Registers");
        let (handled, fx) = key_in(&mut p, DebugAction::Expand);
        assert!(handled && fx.is_empty());
        assert_eq!(p.take_var_asks().iter().map(|a| a.reference).collect::<Vec<_>>(), [3]);
        assert!(p.vars.tree.node(2).unwrap().expanded);
        p.show_children(2, vec![vi("rip", "0x1", "", 0), vi("rax", "0x2", "", 0)]);
        assert_eq!(p.var_rows().len(), 9);
        key_in(&mut p, DebugAction::Expand);
        assert_eq!(selected_text(&mut p), "rip = 0x1", "an open node goes to its first child");
        // Right on a leaf does nothing.
        let at = p.vars.list.selected;
        key_in(&mut p, DebugAction::Expand);
        assert_eq!((p.vars.list.selected, p.var_rows().len(), p.take_var_asks().len()), (at, 9, 0));
        // Left on a leaf goes to the parent; on an open node it closes it; on a closed one it goes up.
        key_in(&mut p, DebugAction::Collapse);
        assert_eq!(selected_text(&mut p), "▾ Registers");
        key_in(&mut p, DebugAction::Collapse);
        assert_eq!(selected_text(&mut p), "▸ Registers");
        assert_eq!(p.var_rows().len(), 7);
        let at = p.vars.list.selected;
        key_in(&mut p, DebugAction::Collapse);
        assert_eq!(p.vars.list.selected, at, "a closed root has no parent");
        // Enter toggles, never opens a file or chooses a frame.
        let (handled, fx) = key_in(&mut p, DebugAction::Open);
        assert!(handled && fx.is_empty(), "{fx:?}");
        assert_eq!(p.var_rows().len(), 9, "opened");
        key_in(&mut p, DebugAction::Open);
        assert_eq!(p.var_rows().len(), 7, "closed");
        go_to(&mut p, "a = 1");
        key_in(&mut p, DebugAction::Open);
        assert_eq!(p.var_rows().len(), 7, "a leaf stays");
        assert!(p.take_var_asks().is_empty());
        // Other actions are not this tab's.
        let (handled, fx) = key_in(&mut p, DebugAction::Start);
        assert!(!handled && fx.is_empty());
        assert_eq!(p.selection_seq(), seq, "no key chose a frame");
        // Right and Left are for the tree only.
        for tab in [DebugTab::Frames, DebugTab::Console] {
            p.set_tab(tab);
            for a in [DebugAction::Expand, DebugAction::Collapse] {
                let (handled, fx) = key_in(&mut p, a.clone());
                assert!(!handled && fx.is_empty(), "{tab:?} {a:?}");
            }
        }
    }

    /// `n` leaves named `start`, `start + 1`, ... with their number as the value.
    fn page(start: usize, n: usize) -> Vec<VarItem> {
        (start..start + n).map(|i| vi(&i.to_string(), &i.to_string(), "int", 0)).collect()
    }

    /// Locals (node 0) holding `arr` (node 1, reference 6, `int []`, hint `hint`), open with its first 1000 rows
    /// answered. The load-more row is the last row.
    fn paged_panel(hint: i64) -> DebugPanel {
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        p.scopes_wanted().expect("the tab asks");
        p.show_scopes(vec![vi("Locals", "", "", 2)], &[0]);
        p.take_var_asks();
        p.show_children(0, vec![VarItem { children: hint, ..vi("arr", "", "int []", 6) }]);
        p.vars.tree.expand(1);
        p.take_var_asks();
        p.show_children(1, page(0, 1000));
        p
    }

    fn last_row(p: &mut DebugPanel) -> String {
        p.var_rows().pop().unwrap()
    }

    fn arr_row(p: &mut DebugPanel) -> String {
        p.var_rows().into_iter().find(|r| r.contains("arr")).unwrap()
    }

    #[test]
    fn the_row_says_what_it_will_do() {
        let mut p = paged_panel(5000);
        assert_eq!(last_row(&mut p), "      load 1000 more  4000 left");
        assert_eq!(arr_row(&mut p), "  ▾ arr  first 1000 of 5000 shown  int []");
        let row = p.vars.tree.node(1).unwrap().more.expect("the row");
        assert!(p.vars.tree.load_more(row));
        assert_eq!(last_row(&mut p), "      reading 1000 more …");
        p.children_failed(1, "timed out".into());
        assert_eq!(last_row(&mut p), "      load 1000 more  could not read: timed out");
        assert_eq!(arr_row(&mut p), "  ▾ arr  first 1000 of 5000 shown  int []", "the container says nothing of it");
        for (loaded, left) in [(2000, "3000 left"), (3000, "2000 left")] {
            assert!(p.vars.tree.load_more(row));
            p.show_children(1, page(loaded - 1000, 1000));
            assert_eq!(arr_row(&mut p), format!("  ▾ arr  first {loaded} of 5000 shown  int []"));
            assert_eq!(last_row(&mut p), format!("      load 1000 more  {left}"));
        }
        assert!(p.vars.tree.load_more(row));
        p.show_children(1, page(3000, 1000));
        assert_eq!(last_row(&mut p), "      load 1000 more", "one page left: no count");
        assert!(p.vars.tree.load_more(row));
        p.show_children(1, page(4000, 1000));
        assert_eq!(arr_row(&mut p), "  ▾ arr  int []", "no note at the end");
        assert_eq!(last_row(&mut p), "      4999 = 4999  int", "and no row");
        assert_eq!(p.var_count(), 2 + 5000);
        // Fewer than a page left.
        let mut p = paged_panel(1500);
        assert_eq!(last_row(&mut p), "      load 500 more");
        let mut p = paged_panel(1001);
        assert_eq!(last_row(&mut p), "      load 1 more");
    }

    #[test]
    fn enter_and_right_load_and_left_goes_to_the_parent() {
        let mut p = paged_panel(5000);
        draw(&mut p, 60, 12, Some(Phase::Suspended));
        for (a, name, left) in [(DebugAction::Open, "Enter", 4000), (DebugAction::Expand, "Right", 3000)] {
            key_in(&mut p, DebugAction::End);
            assert_eq!(selected_text(&mut p), format!("load 1000 more  {left} left"), "{name}");
            let (handled, fx) = key_in(&mut p, a.clone());
            assert!(handled && fx.is_empty(), "{name}: {fx:?}");
            let asks = p.take_var_asks();
            assert_eq!(asks.len(), 1, "{name}");
            assert_eq!(
                (asks[0].node, asks[0].reference, asks[0].start),
                (1, 6, p.vars.tree.node(1).unwrap().children.as_ref().unwrap().len()),
                "{name}"
            );
            // A second press while the page is out asks nothing.
            key_in(&mut p, a);
            assert!(p.take_var_asks().is_empty(), "{name}: two presses, one ask");
            p.show_children(1, page(p.vars.tree.node(1).unwrap().children.as_ref().unwrap().len(), 1000));
            assert!(selected_text(&mut p).ends_with(" int"), "{name}: the selection left the row: {}", selected_text(&mut p));
        }
        assert_eq!(arr_row(&mut p), "  ▾ arr  first 3000 of 5000 shown  int []");
        // Left on the row selects the container.
        key_in(&mut p, DebugAction::End);
        let (handled, _) = key_in(&mut p, DebugAction::Collapse);
        assert!(handled);
        assert_eq!(selected_text(&mut p), "▾ arr  first 3000 of 5000 shown  int []");
        assert!(p.take_var_asks().is_empty());
    }

    #[test]
    fn a_press_anywhere_on_the_row_loads() {
        // A fresh panel for each place, scrolled to the row at 60x12; the row's screen position.
        let at_row = || {
            let mut p = paged_panel(5000);
            key_in(&mut p, DebugAction::End);
            draw(&mut p, 60, 12, Some(Phase::Suspended));
            let y = p.list_rect.y + (p.vars.list.selected - p.vars.list.offset) as u16;
            (p, y)
        };
        let click = |p: &mut DebugPanel, x, y| with_cx(|cx| assert!(p.handle_mouse(&press(x, y, 1), Some(Phase::Suspended), cx)));
        let (mut p, y) = at_row();
        let marker = p.list_rect.x + 1 + 4;
        click(&mut p, marker, y);
        assert_eq!(p.take_var_asks().iter().map(|a| a.start).collect::<Vec<_>>(), [1000], "the marker's cell");
        let (mut p, y) = at_row();
        let last = p.list_rect.right() - 1;
        click(&mut p, last, y);
        assert_eq!(p.take_var_asks().iter().map(|a| a.start).collect::<Vec<_>>(), [1000], "the last cell");
        // A second press while out asks nothing.
        click(&mut p, marker, y);
        assert!(p.take_var_asks().is_empty());
        // A child row is only selected.
        let (mut p, y) = at_row();
        click(&mut p, marker + 3, y - 1);
        assert!(p.take_var_asks().is_empty());
        assert!(selected_text(&mut p).starts_with("999 = 999"), "{}", selected_text(&mut p));
    }

    #[test]
    fn the_failure_is_cut_at_the_border() {
        let mut p = paged_panel(5000);
        let row = p.vars.tree.node(1).unwrap().more.expect("the row");
        p.vars.tree.load_more(row);
        p.take_var_asks();
        p.children_failed(1, "x".repeat(200));
        key_in(&mut p, DebugAction::End);
        let term = draw(&mut p, 50, 12, Some(Phase::Suspended));
        let y = p.list_rect.y + (p.vars.list.selected - p.vars.list.offset) as u16;
        let text = row_text(&term, y, 50);
        assert!(text.contains("load 1000 more  could not read: xxx"), "the label comes first and is whole: {text}");
        let border = |y| term.backend().buffer()[(49, y)].symbol().to_string();
        assert_eq!(border(y), border(y - 1), "the border cell is the border: {text}");
        assert!(!text.contains(&"x".repeat(30)), "cut: {text}");
    }

    #[test]
    fn a_new_seq_takes_the_row_with_the_tree() {
        let mut p = paged_panel(5000);
        let row = p.vars.tree.node(1).unwrap().more.expect("the row");
        assert!(p.vars.tree.load_more(row));
        p.stop_began(Some(1));
        assert!(p.var_rows().is_empty());
        assert!(p.take_var_asks().is_empty());
    }

    fn text_of(p: &mut DebugPanel, phase: Option<Phase>) -> String {
        let term = draw(p, 80, 10, phase);
        (0..10).map(|y| row_text(&term, y, 80)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn the_variables_hints_by_state() {
        let mut none = DebugPanel::default();
        none.set_tab(DebugTab::Variables);
        assert!(text_of(&mut none, None).contains("Shift+F9 starts the selected configuration"));
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        for phase in [Some(Phase::Running), Some(Phase::Stopping), Some(Phase::Starting), None] {
            let t = text_of(&mut p, phase);
            assert!(t.contains("Variables show while the program is paused"), "{phase:?}");
        }
        // A tree in memory is not drawn while the program runs.
        let mut filled = with_vars();
        let t = text_of(&mut filled, Some(Phase::Stopping));
        assert!(t.contains("Variables show while") && !t.contains("Arguments"), "{t}");
        assert_eq!(filled.list_rect, Rect::default());
        let s = Some(Phase::Suspended);
        assert!(text_of(&mut p, s).contains("Reading the variables…"), "Idle with frames");
        p.scopes_wanted();
        assert!(text_of(&mut p, s).contains("Reading the variables…"), "Asked");
        let mut loading = started();
        loading.set_tab(DebugTab::Variables);
        loading.stop_began(Some(1));
        assert!(text_of(&mut loading, s).contains("Reading the variables…"), "no frames yet, the stack is on its way");
        loading.stack_unavailable();
        let t = text_of(&mut loading, s);
        assert!(t.contains("No frames") && !t.contains("Reading"), "{t}");
        p.show_scopes(Vec::new(), &[]);
        let t = text_of(&mut p, s);
        assert!(t.contains("No variables") && !t.contains("Reading"), "Shown, empty: {t}");
        let mut failed = suspended_with(three());
        failed.set_tab(DebugTab::Variables);
        failed.scopes_wanted();
        failed.scopes_failed("boom".into());
        let t = text_of(&mut failed, s);
        assert!(t.contains("Could not read the variables: boom") && !t.contains("Reading"), "{t}");
        assert!(failed.scopes_wanted().is_none(), "a failure is asked again only after the next selection");
        // Row endings: the children left out, and a failure.
        let mut p = with_vars();
        let items: Vec<VarItem> = (0..1000).map(|i| vi(&i.to_string(), "0", "", 0)).collect();
        p.vars.tree.expand(6);
        p.take_var_asks();
        p.show_children(6, items);
        go_to(&mut p, "arr");
        let t = text_of(&mut p, s);
        assert!(t.contains("arr  first 1000 of 5000 shown  int [5000]"), "{t}");
        p.vars.tree.expand(2);
        p.take_var_asks();
        p.children_failed(2, "boom".into());
        let rows = p.var_rows();
        assert_eq!(rows.last().map(String::as_str), Some("▸ Registers  could not read: boom"));
        go_to(&mut p, "Registers");
        let t = text_of(&mut p, s);
        assert!(t.contains("Registers  could not read: boom"), "{t}");
    }

    #[test]
    fn the_note_on_children_left_out_comes_before_a_long_type() {
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        p.scopes_wanted().expect("the tab asks");
        p.show_scopes(vec![vi("Locals", "", "", 2)], &[0]);
        p.take_var_asks();
        let arr = VarItem { children: 5000, ..vi("arr", "", &"T".repeat(200), 6) };
        p.show_children(0, vec![arr]);
        p.vars.tree.expand(1);
        p.take_var_asks();
        p.show_children(1, (0..1000).map(|i| vi(&i.to_string(), "0", "", 0)).collect());
        go_to(&mut p, "arr");
        let term = draw(&mut p, 40, 10, Some(Phase::Suspended));
        let t = (0..10).map(|y| row_text(&term, y, 40)).collect::<Vec<_>>().join("\n");
        assert!(t.contains("arr  first 1000 of 5000 shown  T"), "the note is drawn before the type: {t}");
    }

    #[test]
    fn a_row_with_no_value_has_no_equals() {
        let mut p = with_vars();
        let rows = p.var_rows();
        assert!(rows.contains(&"  ▸ p  struct point".to_string()), "{rows:?}");
        assert!(rows.contains(&"    a = 1  int".to_string()), "{rows:?}");
        let t = text_of(&mut p, Some(Phase::Suspended));
        assert!(t.contains("p  struct point") && !t.contains("p = "), "{t}");
        assert!(t.contains("a = 1  int"), "{t}");
        assert!(!t.contains("Locals = ") && !t.contains("Arguments = "), "a scope has neither");
    }

    #[test]
    fn the_marker_cell_toggles_and_a_row_selects() {
        let mut p = with_vars();
        // Registers' `Locals` > p > in > deep, so a node at depth 2 has a marker.
        p.vars.tree.expand(5);
        p.take_var_asks();
        p.show_children(5, vec![vi("in", "", "struct inner", 7), vi("x", "1", "int", 0)]);
        p.vars.tree.expand(7);
        p.take_var_asks();
        p.show_children(7, vec![vi("deeper", "", "struct d", 8)]);
        draw(&mut p, 60, 14, Some(Phase::Suspended));
        let list = p.list_rect;
        let rows = p.var_rows();
        let at = |p: &DebugPanel, name: &str| -> (u16, u16, usize) {
            let i = rows.iter().position(|r| after_marker(r).starts_with(name)).unwrap();
            let depth = p.vars.tree.node(p.vars.tree.rows()[i]).unwrap().depth;
            (list.x + 1 + 2 * depth as u16, list.y + i as u16 - p.vars.list.offset as u16, depth)
        };
        let click = |p: &mut DebugPanel, x: u16, y: u16| -> (bool, Vec<Effect>) {
            let mut handled = false;
            let fx = with_cx(|cx| handled = p.handle_mouse(&press(x, y, 1), Some(Phase::Suspended), cx));
            (handled, fx)
        };
        let (x, y, depth) = at(&p, "Registers");
        assert_eq!(depth, 0);
        let (handled, fx) = click(&mut p, x + 1, y);
        assert!(handled && fx.is_empty());
        assert!(p.vars.tree.node(2).unwrap().expanded, "the marker's second cell toggles");
        assert_eq!(selected_text(&mut p), "▾ Registers …");
        p.take_var_asks();
        click(&mut p, x, y);
        assert!(!p.vars.tree.node(2).unwrap().expanded, "the first cell too");
        click(&mut p, x + 2, y);
        assert!(!p.vars.tree.node(2).unwrap().expanded, "the name selects only");
        click(&mut p, x + 9, y);
        assert!(!p.vars.tree.node(2).unwrap().expanded);
        // Depth 2: `in`. Depth 0's marker cell on its row only selects it.
        let (x2, y2, depth2) = at(&p, "in ");
        assert_eq!(depth2, 2);
        assert!(p.vars.tree.node(7).unwrap().expanded);
        click(&mut p, x2 - 4, y2);
        assert!(p.vars.tree.node(7).unwrap().expanded, "depth 0's marker cell is not depth 2's");
        assert_eq!(selected_text(&mut p), "▾ in  struct inner", "the press selected the row of its own cell");
        click(&mut p, x2 + 1, y2);
        assert!(!p.vars.tree.node(7).unwrap().expanded, "depth 2's marker toggles");
        assert_eq!(selected_text(&mut p), "▸ in  struct inner");
        // Below the rows nothing is hit.
        let (handled, _) = click(&mut p, list.x + 3, list.y + 13);
        assert!(!handled);
    }

    /// Every row of the terminal below `from` has its border in the last column, and the text of no row passes it.
    fn borders_hold(term: &Terminal<TestBackend>, w: u16, h: u16, from: u16, what: &str) {
        for y in from..h - 1 {
            assert_eq!(term.backend().buffer()[(w - 1, y)].symbol(), "│", "{what}: row {y}: {}", row_text(term, y, w));
        }
    }

    #[test]
    fn long_and_wide_text_stays_inside_the_border() {
        let value = "日本語".repeat(50);
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        p.scopes_wanted();
        p.show_scopes(vec![vi("日本語".repeat(20).as_str(), "", "", 1)], &[0]);
        p.take_var_asks();
        let long_type = "T".repeat(300);
        p.show_children(0, vec![vi("v", &value, &long_type, 4), vi(&"n".repeat(60), "1", &value, 0), vi("w", "x", &value, 0)]);
        let term = draw(&mut p, 24, 9, Some(Phase::Suspended));
        borders_hold(&term, 24, 9, 1, "width 24");
        let row = row_text(&term, 4, 24);
        assert!(row.contains("日"), "positive control, the value is drawn: {row}");
        // The control: a row that fits ends before the border without a clip.
        let mut q = suspended_with(three());
        q.set_tab(DebugTab::Variables);
        fill_vars(&mut q);
        let term = draw(&mut q, 24, 9, Some(Phase::Suspended));
        assert!(row_text(&term, 4, 24).contains("a = 1  int"), "{}", row_text(&term, 4, 24));
        borders_hold(&term, 24, 9, 1, "short rows");
    }

    #[test]
    fn a_long_name_leaves_room_for_the_value() {
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        p.scopes_wanted();
        let scope = "S".repeat(100);
        p.show_scopes(vec![vi(&scope, "", "", 1)], &[0]);
        p.take_var_asks();
        p.show_children(0, vec![vi(&"n".repeat(100), "1", "", 0), vi("short", "2", "int", 0)]);
        let term = draw(&mut p, 60, 9, Some(Phase::Suspended));
        let scope_row = row_text(&term, 3, 60);
        assert!(scope_row.contains(&"S".repeat(50)) && !scope_row.contains('…'), "a scope row has the whole width: {scope_row}");
        let long = row_text(&term, 4, 60);
        assert!(long.contains("… = 1"), "the name is cut and the value is on the row: {long}");
        let name = long.chars().filter(|c| *c == 'n').count();
        assert!((20..=28).contains(&name), "about half of the 54 cells left after indent and marker: {name} in {long}");
        assert!(row_text(&term, 5, 60).contains("short = 2  int"), "{}", row_text(&term, 5, 60));
        // A name with a type only shares the row too.
        let mut q = suspended_with(three());
        q.set_tab(DebugTab::Variables);
        q.scopes_wanted();
        q.show_scopes(vec![vi(&"m".repeat(100), "", "struct point", 1)], &[]);
        let term = draw(&mut q, 60, 9, Some(Phase::Suspended));
        assert!(row_text(&term, 3, 60).contains("…  struct point"), "{}", row_text(&term, 3, 60));
    }

    #[test]
    fn a_failed_row_shows_it_past_a_long_value() {
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        p.scopes_wanted();
        p.show_scopes(vec![vi("Locals", "", "", 1)], &[0]);
        p.take_var_asks();
        let value = "v".repeat(500);
        p.show_children(0, vec![vi("ptr", &value, "char *", 5), vi("other", &value, "char *", 6)]);
        p.vars.tree.expand(1);
        p.vars.tree.expand(2);
        p.take_var_asks();
        p.children_failed(2, "timed out".into());
        let t = text_of(&mut p, Some(Phase::Suspended));
        assert!(t.contains("other  could not read: timed out"), "{t}");
        assert!(t.contains("ptr … = vvv"), "loading shows right after the name, before the value: {t}");
        assert!(!t.contains("char *"), "the type is past the clip");
        let mut q = with_vars();
        q.vars.tree.expand(2);
        assert!(q.var_rows().last().unwrap().ends_with("Registers …"));
    }

    #[test]
    fn a_second_scopes_answer_is_ignored() {
        let mut p = with_vars();
        let rows = p.var_rows();
        // An ask in flight survives a second answer.
        p.vars.tree.expand(2);
        p.show_scopes(vec![vi("Other", "", "", 9)], &[0]);
        assert_eq!(p.var_rows().len(), rows.len(), "the second answer builds nothing");
        assert_eq!(p.take_var_asks().iter().map(|a| a.node).collect::<Vec<_>>(), [2], "the ask is still queued");
        p.scopes_failed("late".into());
        assert!(matches!(p.vars.load, VarsLoad::Shown), "a failure after Shown changes nothing");
        assert_eq!(p.var_rows().len(), rows.len());
        // Idle: nothing was asked, so nothing is taken.
        let mut q = suspended_with(three());
        q.set_tab(DebugTab::Variables);
        q.show_scopes(vec![vi("Early", "", "", 9)], &[0]);
        q.scopes_failed("early".into());
        assert!(matches!(q.vars.load, VarsLoad::Idle) && q.var_rows().is_empty() && q.take_var_asks().is_empty());
    }

    #[test]
    fn a_children_answer_for_a_node_not_waiting_is_ignored() {
        let mut p = with_vars();
        let rows = p.var_rows();
        p.show_children(0, vec![vi("again", "", "", 0)]);
        p.children_failed(0, "late".into());
        p.show_children(99, vec![vi("none", "", "", 0)]);
        assert_eq!(p.var_rows(), rows);
    }

    #[test]
    fn a_clear_does_not_reuse_a_seq() {
        let mut p = with_vars();
        let before = p.selection_seq();
        assert!(before > 0);
        p.clear();
        assert!(p.selection_seq() > before, "{} after {before}", p.selection_seq());
        assert_ne!(p.selection_seq(), 0);
        assert!(p.var_rows().is_empty());
        assert_eq!(p.tab(), DebugTab::Console, "the rest is the default");
    }

    #[test]
    fn every_tab_has_its_own_press_and_scroll() {
        let long = |p: &mut DebugPanel| {
            p.push((0..30).map(|i| RunLine::plain(format!("line {i}"))).collect());
            let frames: Vec<FrameRow> = (0..30).map(|i| row(i, &format!("f{i}"), Some("d.c:1"))).collect();
            p.show_stack(1, frames);
        };
        let mut seen = Vec::new();
        for tab in TABS {
            let mut p = suspended_with(three());
            long(&mut p);
            p.set_tab(DebugTab::Variables);
            p.scopes_wanted();
            let items: Vec<VarItem> = (0..30).map(|i| vi(&format!("s{i}"), "", "", 0)).collect();
            p.show_scopes(items, &[]);
            p.set_tab(tab);
            p.state.home();
            draw(&mut p, 60, 9, Some(Phase::Suspended));
            let list = p.list_rect;
            let others = |p: &DebugPanel| (p.state.selected, p.frames_state.selected, p.vars.list.selected);
            let before = others(&p);
            with_cx(|cx| assert!(p.handle_mouse(&press(list.x + 3, list.y + 2, 1), Some(Phase::Suspended), cx)));
            let after = others(&p);
            let moved: Vec<bool> = [before.0 != after.0, before.1 != after.1, before.2 != after.2].to_vec();
            let want = match tab {
                DebugTab::Console => [true, false, false],
                DebugTab::Frames => [false, true, false],
                DebugTab::Variables => [false, false, true],
            };
            assert_eq!(moved, want, "{tab:?}: a press reaches its own tab's selection");
            let offsets = |p: &DebugPanel| (p.state.offset, p.frames_state.offset, p.vars.list.offset);
            let before = offsets(&p);
            with_cx(|cx| {
                assert!(p.handle_mouse(
                    &Mouse { kind: MouseEventKind::ScrollDown, x: list.x + 3, y: list.y, clicks: 1, mods: KeyModifiers::NONE },
                    Some(Phase::Suspended),
                    cx
                ))
            });
            let after = offsets(&p);
            let moved: Vec<bool> = [before.0 != after.0, before.1 != after.1, before.2 != after.2].to_vec();
            assert_eq!(moved, want, "{tab:?}: the wheel scrolls its own tab's list");
            seen.push(tab);
        }
        assert_eq!(seen, TABS);
    }

    #[test]
    fn a_deep_row_is_clipped() {
        let mut p = suspended_with(three());
        p.set_tab(DebugTab::Variables);
        p.scopes_wanted();
        p.show_scopes(vec![vi("root", "", "", 1)], &[0]);
        let mut node = 0;
        for d in 0..100 {
            p.take_var_asks();
            p.show_children(node, vec![vi(&format!("n{d}"), "v", "t", 1)]);
            node = p.vars.tree.node(node).unwrap().children.as_ref().unwrap()[0];
            p.vars.tree.expand(node);
        }
        p.take_var_asks();
        assert_eq!(p.vars.tree.node(node).unwrap().depth, 100);
        p.vars.node = node;
        for w in [4, 5, 12, 30] {
            let term = draw(&mut p, w, 7, Some(Phase::Suspended));
            if w >= 12 {
                borders_hold(&term, w, 7, 1, &format!("width {w}"));
            }
        }
        // `2 * depth` wraps a u16 at depth 32768, where an unclamped indent would put the rows back inside the border.
        let mut q = suspended_with(three());
        q.set_tab(DebugTab::Variables);
        q.scopes_wanted();
        q.show_scopes(vec![vi("root", "", "", 1)], &[0]);
        let mut node = 0;
        for _ in 0..32770 {
            q.vars.tree.expand(node);
            q.vars.tree.set_children(node, vec![vi("n", "v", "t", 1)]);
            node = q.vars.tree.node(node).unwrap().children.as_ref().unwrap()[0];
        }
        assert_eq!(q.vars.tree.node(node).unwrap().depth, 32770);
        q.vars.node = node;
        let term = draw(&mut q, 30, 7, Some(Phase::Suspended));
        borders_hold(&term, 30, 7, 1, "depth 32770");
        for y in 3..6 {
            let row = row_text(&term, y, 30);
            assert_eq!(row.trim_matches(|c: char| c == '│' || c == ' '), "", "row {y} draws nothing past the border: {row}");
        }
    }
}
