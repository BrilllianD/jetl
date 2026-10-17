//! Run tool window: a toolbar (configuration picker, Run, Rerun, Stop), the
//! last run's status line (name, spinner or exit status, elapsed time,
//! command) and its output, at most `MAX_LINES`
//! lines (the oldest are dropped). Matched locations are underlined and
//! colored by severity; a click on one, Enter or a double click opens it.
//! The selection follows new output while it sits on the last line and that
//! line is in view, like the Event Log.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::matcher::{Link, RunLine, Severity};
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
    EditConfigs,
}

impl Btn {
    /// The text between the padding spaces; the picker's is its arrow.
    fn label(self) -> &'static str {
        match self {
            Btn::Picker => "▾",
            Btn::Run => "▶ Run",
            Btn::Rerun => "↻ Rerun",
            Btn::Stop => "■ Stop",
            Btn::EditConfigs => "Edit Configurations…",
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
}

impl RunPanel {
    /// A new run: the old output goes.
    pub fn start(&mut self, name: &str, command: &str) {
        self.lines.clear();
        self.state = ListState::default();
        self.header =
            Some(Header { name: name.into(), command: command.into(), started: Instant::now(), ended: None, status: Status::Running });
    }

    pub fn push(&mut self, lines: Vec<RunLine>) {
        self.lines.extend(lines);
        let dropped = self.lines.len().saturating_sub(MAX_LINES);
        self.lines.drain(..dropped);
        self.state.grew(self.lines.len(), dropped);
    }

    pub fn finish(&mut self, status: Status) {
        if let Some(h) = &mut self.header {
            h.ended = Some(Instant::now());
            h.status = status;
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.state = ListState::default();
        self.header = None;
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

    pub fn handle_action(&mut self, a: &RunAction, cx: &mut Ctx) -> bool {
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
            RunAction::Rerun | RunAction::Stop | RunAction::RunSelected | RunAction::EditConfigs => return false,
        }
        true
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
                    Some(Btn::EditConfigs) => Some(Action::Run(RunAction::EditConfigs)),
                    _ => None,
                };
                if let Some(a) = action {
                    cx.effects.push(Effect::Perform(a));
                }
                true
            }
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let Some(i) = self.state.hit(self.list_rect, m.x, m.y) else { return false };
                self.state.select(i);
                if button == MouseButton::Right {
                    let items = vec![
                        MenuItem::new("Jump to Source", Action::Run(RunAction::Open)).enabled(self.target(i).is_some()),
                        MenuItem::new("Run Selected", Action::Run(RunAction::RunSelected)),
                        MenuItem::new("Rerun", Action::Run(RunAction::Rerun)),
                        MenuItem::new("Stop", Action::Run(RunAction::Stop)).enabled(self.running()),
                        MenuItem::new("Edit Configurations…", Action::Run(RunAction::EditConfigs)),
                    ];
                    cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, m.x, m.y)));
                } else if m.clicks == 2 || self.link_rects.iter().any(|(r, _)| r.contains((m.x, m.y).into())) {
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, selected: Option<&str>) {
        let title = match &self.header {
            Some(h) => format!(" Run: {} ", h.name),
            None => " Run ".to_string(),
        };
        let block = tool_window_block(&title, focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        self.link_rects.clear();
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
        for btn in [Btn::Run, Btn::Rerun, Btn::Stop, Btn::EditConfigs] {
            let text = format!(" {} ", btn.label());
            if x + 1 + text.width() as u16 > right {
                break;
            }
            let enabled = match btn {
                Btn::Rerun => ran,
                Btn::Stop => running,
                _ => true,
            };
            let style = if enabled { theme.title.bg(theme.panel_bg) } else { theme.dim.bg(theme.panel_bg) };
            x += 1;
            let rect = put(buf, &mut x, r.y, right, &text, style);
            self.hits.push((btn, rect));
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
        let rest = [(format!(" {text}"), style), (format!("  {elapsed}"), theme.dim), (format!("  $ {}", h.command), theme.dim)];
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
        self.state.fit(self.lines.len(), list.height as usize);
        for (vi, i) in self.state.visible().enumerate() {
            let line = &self.lines[i];
            let y = list.y + vi as u16;
            let selected = i == self.state.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(list.x, y, list.width, 1), base);
            }
            let style = match line.severity {
                Some(Severity::Error) => base.fg(theme.error.fg.unwrap_or(theme.fg)),
                Some(Severity::Warning) => base.fg(theme.warn.fg.unwrap_or(theme.fg)),
                None => base,
            };
            let x0 = list.x + 1;
            let width = (right - x0) as usize;
            match &line.link {
                Some(link) => {
                    let (pre, at, post) = (&line.text[..link.span.start], &line.text[link.span.clone()], &line.text[link.span.end..]);
                    let (x, _) = buf.set_stringn(x0, y, pre, width, style);
                    if x < right {
                        let (x2, _) = buf.set_stringn(x, y, at, (right - x) as usize, style.add_modifier(Modifier::UNDERLINED));
                        self.link_rects.push((Rect::new(x, y, x2 - x, 1), i));
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
        if self.lines.is_empty() && h.status == Status::Running && list.height > 0 {
            buf.set_stringn(list.x + 1, list.y, "No output yet", list.width.saturating_sub(1) as usize, theme.dim);
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
        p.start("t", "t");
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
        p.start("t", "t");
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
}
