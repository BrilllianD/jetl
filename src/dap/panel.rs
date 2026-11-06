//! The Debug tool window: a toolbar (Resume, Pause, Step Over, Step Into, Step Out, Stop; each enabled by the
//! session's phase) and the Console, the program's output, at most `MAX_LINES` lines. Locations are underlined
//! and open on a click, a double click or Enter, as in the Run window. The selection follows new output while it
//! sits on the last line and that line is in view. The title names the configuration and the state.

use std::collections::VecDeque;

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

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
    /// The build step failed, or could not run.
    Failed,
    /// Stopped before the program started: cancelled, refused, or no executable.
    NotStarted,
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
    }

    pub fn set_stage(&mut self, stage: Option<BuildStage>) {
        self.stage = stage;
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
        let building = self.stage == Some(BuildStage::Building);
        self.hits.clear();
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
        let list = Rect::new(r.x, r.y + 1, r.width, r.height - 1);
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

    /// A press on a button performs its action when the phase allows it, and is consumed either way.
    pub fn handle_mouse(&mut self, m: &Mouse, phase: Option<Phase>, cx: &mut Ctx) -> bool {
        let pos = (m.x, m.y).into();
        let building = self.stage == Some(BuildStage::Building);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if self.hits.iter().any(|(_, r)| r.contains(pos)) => {
                if let Some((btn, _)) = self.hits.iter().find(|(_, r)| r.contains(pos)).copied()
                    && btn.enabled(phase, building)
                {
                    cx.effects.push(Effect::Perform(crate::actions::Action::Debug(btn.action())));
                }
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
                self.state.scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.state.scroll(3);
                true
            }
            _ => false,
        }
    }

    pub fn handle_action(&mut self, a: &DebugAction, cx: &mut Ctx) -> bool {
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
        for (stage, stop) in
            [(Some(BuildStage::Building), true), (Some(BuildStage::Failed), false), (Some(BuildStage::NotStarted), false), (None, false)]
        {
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
