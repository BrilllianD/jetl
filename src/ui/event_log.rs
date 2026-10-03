//! Event Log tool window: every notification of the session
//! (`App::notifications`), oldest first, one row each: level glyph, local
//! time and the first line of the text (a dim `+N lines` marks the rest).
//! Enter or a double click shows the full text; copy takes the full text
//! too. The selection follows new entries while it sits on the last one and
//! that row is in view; moving up or wheeling away stops following.

use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::actions::{Action, EventLogAction};
use crate::app::{Ctx, Notification};
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::dialog::Dialog;
use crate::ui::menu::{ContextMenu, MenuItem};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;

#[derive(Debug, Default)]
pub struct EventLogPanel {
    pub state: ListState,
    list_rect: Rect,
}

impl EventLogPanel {
    /// Whether new entries move the selection: it is on the last entry and
    /// that row is visible (or the log is empty).
    pub fn following(&self) -> bool {
        let s = &self.state;
        s.len == 0 || (s.selected + 1 == s.len && (s.viewport == 0 || s.offset + s.viewport >= s.len))
    }

    /// The log grew to `len` entries after dropping `dropped` from the front.
    pub fn pushed(&mut self, len: usize, dropped: usize) {
        let follow = self.following();
        self.state.selected = self.state.selected.saturating_sub(dropped);
        self.state.offset = self.state.offset.saturating_sub(dropped);
        self.state.set_len(len);
        if follow {
            self.state.end();
        }
    }

    pub fn clear(&mut self) {
        self.state.set_len(0);
    }

    pub fn select(&mut self, i: usize) {
        self.state.select(i);
    }

    pub fn handle_action(&mut self, a: &EventLogAction, log: &[Notification], cx: &mut Ctx) -> bool {
        self.state.set_len(log.len());
        let page = self.state.viewport.max(1);
        match a {
            EventLogAction::Up => self.state.up(1),
            EventLogAction::Down => self.state.down(1),
            EventLogAction::PageUp => self.state.up(page),
            EventLogAction::PageDown => self.state.down(page),
            EventLogAction::Home => self.state.home(),
            EventLogAction::End => self.state.end(),
            EventLogAction::Details => self.details(log, cx),
            // Copy and clear change app state; `App::perform` runs them.
            EventLogAction::Copy | EventLogAction::Clear => return false,
        }
        true
    }

    fn details(&self, log: &[Notification], cx: &mut Ctx) {
        if let Some(n) = log.get(self.state.selected) {
            let title = format!("{} at {}", level_name(n.level), clock(n.wall));
            cx.effects.push(Effect::Dialog(Dialog::info(&title, &n.text)));
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse, log: &[Notification], cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let Some(i) = self.state.hit(self.list_rect, m.x, m.y) else { return false };
                self.state.select(i);
                if button == MouseButton::Right {
                    let items = vec![
                        MenuItem::new("Copy", Action::EventLog(EventLogAction::Copy)),
                        MenuItem::new("Show Full Text", Action::EventLog(EventLogAction::Details)),
                        MenuItem::new("Clear Log", Action::EventLog(EventLogAction::Clear)),
                    ];
                    cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, m.x, m.y)));
                } else if m.clicks == 2 {
                    self.details(log, cx);
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, log: &[Notification]) {
        let title = if log.is_empty() { " Event Log ".to_string() } else { format!(" Event Log ({}) ", log.len()) };
        let block = tool_window_block(&title, focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        self.list_rect = r;
        self.state.fit(log.len(), r.height as usize);
        if r.height == 0 || r.width < 4 {
            return;
        }
        let buf = frame.buffer_mut();
        if log.is_empty() {
            buf.set_stringn(r.x + 1, r.y, "No events", r.width.saturating_sub(1) as usize, theme.dim);
            return;
        }
        let right = r.right();
        for (vi, i) in self.state.visible().enumerate() {
            let n = &log[i];
            let y = r.y + vi as u16;
            let selected = i == self.state.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let level = match n.level {
                Level::Info => theme.info,
                Level::Warn => theme.warn,
                Level::Error => theme.error,
            };
            let glyph_style = if selected { base.fg(level.fg.unwrap_or(theme.fg)) } else { level };
            let (x, _) = buf.set_stringn(r.x + 1, y, glyph(n.level), (right - r.x - 1) as usize, glyph_style.add_modifier(Modifier::BOLD));
            if x + 2 >= right {
                continue;
            }
            let (x, _) = buf.set_stringn(x + 1, y, clock(n.wall), (right - x - 1) as usize, if selected { base } else { theme.dim });
            if x + 2 >= right {
                continue;
            }
            let mut lines = n.text.lines();
            let first = lines.next().unwrap_or("");
            let (x, _) = buf.set_stringn(x + 2, y, first, (right - x - 2) as usize, base);
            let more = lines.count();
            if more > 0 && x + 1 < right {
                let marker = format!(" +{more} line{}", if more == 1 { "" } else { "s" });
                buf.set_stringn(x, y, marker, (right - x) as usize, if selected { base } else { theme.dim });
            }
        }
    }
}

fn glyph(level: Level) -> &'static str {
    match level {
        Level::Info => "i",
        Level::Warn => "!",
        Level::Error => "×",
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Info => "Info",
        Level::Warn => "Warning",
        Level::Error => "Error",
    }
}

/// `HH:MM:SS` of a wall clock time in the local time zone.
pub fn clock(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    hms(secs + local_offset(secs))
}

fn hms(secs: i64) -> String {
    let s = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Seconds east of UTC at `secs`, from the C library's time zone data.
#[cfg(unix)]
fn local_offset(secs: i64) -> i64 {
    let t = secs as libc::time_t;
    // SAFETY: `localtime_r` only writes the `tm` it is given; a zeroed `tm`
    // is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() { 0 } else { tm.tm_gmtoff as i64 }
}

#[cfg(not(unix))]
fn local_offset(_secs: i64) -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_format() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(1_700_000_000), "22:13:20");
        assert_eq!(hms(1_700_000_000 + 3 * 3600), "01:13:20");
        assert_eq!(hms(-1), "23:59:59");
    }

    #[test]
    fn follows_only_from_the_last_visible_row() {
        let mut p = EventLogPanel::default();
        assert!(p.following(), "an empty log follows");
        p.pushed(1, 0);
        p.pushed(2, 0);
        assert_eq!(p.state.selected, 1);
        p.state.fit(2, 5);
        p.state.up(1);
        p.pushed(3, 0);
        assert_eq!(p.state.selected, 0, "a selection moved up stays put");
        p.state.end();
        for len in 4..=10 {
            p.pushed(len, 0);
        }
        assert_eq!((p.state.selected, p.state.offset), (9, 5), "follows and scrolls");
        p.state.scroll(-2);
        p.pushed(11, 0);
        assert_eq!(p.state.selected, 9, "wheeled away from the end: no follow");
        // Dropping entries from the front keeps the same entry selected.
        p.pushed(6, 5);
        assert_eq!(p.state.selected, 4);
    }
}
