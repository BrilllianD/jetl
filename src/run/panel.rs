//! Run tool window: the last task's status line (name, spinner or exit
//! status, elapsed time, command) above its output, at most `MAX_LINES`
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

use super::matcher::{Link, RunLine, Severity};
use crate::actions::{Action, RunAction};
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
            RunAction::Rerun | RunAction::Stop => return false,
        }
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let Some(i) = self.state.hit(self.list_rect, m.x, m.y) else { return false };
                self.state.select(i);
                if button == MouseButton::Right {
                    let items = vec![
                        MenuItem::new("Jump to Source", Action::Run(RunAction::Open)).enabled(self.target(i).is_some()),
                        MenuItem::new("Rerun", Action::Run(RunAction::Rerun)),
                        MenuItem::new("Stop", Action::Run(RunAction::Stop)).enabled(self.running()),
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let title = match &self.header {
            Some(h) => format!(" Run: {} ", h.name),
            None => " Run ".to_string(),
        };
        let block = tool_window_block(&title, focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        self.link_rects.clear();
        if r.height == 0 || r.width < 4 {
            self.list_rect = Rect::default();
            return;
        }
        let buf = frame.buffer_mut();
        let right = r.right();
        let Some(h) = &self.header else {
            self.list_rect = Rect::default();
            let hint = "No task has run yet: Alt+Shift+F10 picks one";
            buf.set_stringn(r.x + 1, r.y, hint, r.width.saturating_sub(1) as usize, theme.dim);
            return;
        };
        // Status line.
        let (glyph, text, style) = status_text(h, theme);
        let (x, _) = buf.set_stringn(r.x + 1, r.y, glyph, (right - r.x - 1) as usize, style.add_modifier(Modifier::BOLD));
        let elapsed = format_elapsed(h.ended.unwrap_or_else(Instant::now) - h.started);
        let rest = [(format!(" {text}"), style), (format!("  {elapsed}"), theme.dim), (format!("  $ {}", h.command), theme.dim)];
        let mut x = x;
        for (s, st) in rest {
            if x >= right {
                break;
            }
            x = buf.set_stringn(x, r.y, &s, (right - x) as usize, st).0;
        }
        // Output.
        let list = Rect::new(r.x, r.y + 1, r.width, r.height - 1);
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
}
