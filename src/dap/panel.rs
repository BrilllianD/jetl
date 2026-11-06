//! The Debug tool window: a toolbar (Resume, Pause, Step Over, Step Into, Step Out, Stop; each enabled by the
//! session's phase), a tab row of its own (Frames, Console) and the tab's content. The Console is the program's
//! output, at most `MAX_LINES` lines. Locations are underlined and open on a click, a double click or Enter, as in
//! the Run window. The selection follows new output while it sits on the last line and that line is in view.
//! Frames, while the program is paused, is a thread row (it opens the thread menu) above the selected thread's
//! stack, as rows the App made (`FrameRow`); a choice comes back as `Effect::DebugFrame`. The tab row has a row to
//! itself because the toolbar leaves no room for it at 80 columns. The title names the configuration and the state.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::protocol::Thread;
use super::session::{Phase, SessionId};
use crate::actions::DebugAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Mouse};
use crate::run::matcher::RunLine;
use crate::run::panel::{MAX_LINES, draw_line, put, row_style};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;

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
    #[default]
    Console,
}

/// The tabs in tab order.
pub const TABS: [DebugTab; 2] = [DebugTab::Frames, DebugTab::Console];

impl DebugTab {
    fn label(self) -> &'static str {
        match self {
            DebugTab::Frames => "Frames",
            DebugTab::Console => "Console",
        }
    }
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
    /// Rises on every frame choice, thread choice and stop: an answer asked under another value is stale.
    seq: u64,
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

    /// Threads, thread and frames go; a stack answer asked before is stale from now on.
    pub fn clear_stack(&mut self) {
        self.threads.clear();
        self.thread = None;
        self.frames.clear();
        self.frames_state = ListState::default();
        self.loading = false;
        self.seq += 1;
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
        self.seq += 1;
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
            self.seq += 1;
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
        *self = Self::default();
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
        if self.tab == DebugTab::Frames {
            self.render_frames(buf, content, focused, theme, phase);
            return;
        }
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
            MouseEventKind::Down(MouseButton::Left) if self.tab == DebugTab::Frames => {
                let Some(i) = self.frames_state.hit(self.list_rect, m.x, m.y) else { return false };
                self.frames_state.select(i);
                self.seq += 1;
                cx.effects.push(Effect::DebugFrame { index: i, focus: m.clicks == 2 });
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(i) = self.state.hit(self.list_rect, m.x, m.y) else { return false };
                self.state.select(i);
                if m.clicks == 2 || self.link_rects.iter().any(|(r, _)| r.contains(pos)) {
                    self.open(i, cx);
                }
                true
            }
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
        if self.tab == DebugTab::Frames { &mut self.frames_state } else { &mut self.state }
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
            self.seq += 1;
            cx.effects.push(Effect::DebugFrame { index: self.frames_state.selected, focus: false });
        }
        true
    }

    pub fn handle_action(&mut self, a: &DebugAction, cx: &mut Ctx) -> bool {
        if self.tab == DebugTab::Frames {
            return self.frames_action(a, cx);
        }
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
                    draw(&mut p, w, h, Some(Phase::Suspended));
                }
            }
            // Four rows leave the border, the toolbar and the tabs: the bottom row is border only.
            let mut p = suspended_with(vec![row(0, "add", Some("demo.c:4"))]);
            p.set_tab(tab);
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
        p.step_tab(true);
        assert_eq!(p.tab(), DebugTab::Frames);
        p.step_tab(true);
        assert_eq!(p.tab(), DebugTab::Console, "wraps forward");
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
        assert!(row_text(&term, 2, 60).starts_with("│ Frames  Console "), "{}", row_text(&term, 2, 60));
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
        let (_, fx) = click(&mut p, 1, 1);
        assert_eq!(one(fx), (1, false), "also on the selected row: it brings the frame back");
        let (_, fx) = click(&mut p, 2, 2);
        assert_eq!(one(fx), (2, true), "a double click focuses the editor");
        let (handled, fx) = click(&mut p, 3, 1);
        assert!(!handled && fx.is_empty(), "below the rows: {fx:?}");
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
}
