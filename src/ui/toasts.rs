//! Notification toasts: stacked boxes in the top-right corner that expire on
//! their own (errors last longest) and close on click. The status bar keeps
//! showing the latest notification as well.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::theme::Theme;
use crate::event::Level;

/// At most this many toasts are kept; a new one pushes out the oldest.
pub const MAX_TOASTS: usize = 4;
/// Text lines per toast before it is cut with an ellipsis.
const MAX_LINES: usize = 3;

#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

#[derive(Debug, Default)]
pub struct Toasts {
    /// Oldest first; drawn newest on top.
    items: Vec<Toast>,
    next_id: u64,
    /// Screen rect of each drawn toast, for click to dismiss.
    rects: Vec<(u64, Rect)>,
}

/// How long a toast stays up.
pub fn ttl(level: Level) -> Duration {
    match level {
        Level::Info => Duration::from_secs(4),
        Level::Warn => Duration::from_secs(8),
        Level::Error => Duration::from_secs(12),
    }
}

impl Toasts {
    pub fn items(&self) -> &[Toast] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Show a notification. Repeating the text of a visible toast restarts
    /// that toast instead of stacking a copy.
    pub fn push(&mut self, level: Level, text: &str, now: Instant) {
        if let Some(i) = self.items.iter().position(|t| t.text == text && t.level == level) {
            let mut t = self.items.remove(i);
            t.at = now;
            self.items.push(t);
            return;
        }
        self.next_id += 1;
        self.items.push(Toast { id: self.next_id, level, text: text.to_string(), at: now });
        if self.items.len() > MAX_TOASTS {
            self.items.remove(0);
        }
    }

    /// Drop toasts whose time is up. Returns true when any was removed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.items.len();
        self.items.retain(|t| now.saturating_duration_since(t.at) < ttl(t.level));
        self.items.len() != before
    }

    /// Close the toast under a click. Returns true when one was hit.
    pub fn dismiss_at(&mut self, x: u16, y: u16) -> bool {
        let p = ratatui::layout::Position::new(x, y);
        let Some(id) = self.rects.iter().find(|(_, r)| r.contains(p)).map(|(id, _)| *id) else { return false };
        self.items.retain(|t| t.id != id);
        self.rects.retain(|(i, _)| *i != id);
        true
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.rects.clear();
        if self.items.is_empty() || area.width < 24 || area.height < 6 {
            return;
        }
        let w = (area.width / 2).clamp(22, 52);
        let x = area.right().saturating_sub(w + 1);
        // Keep the lower half free so toasts never cover much of the editor.
        let bottom = area.y + area.height / 2 + 2;
        let mut y = area.y + 1;
        for t in self.items.iter().rev() {
            let lines = wrap(&t.text, (w - 4) as usize, MAX_LINES);
            let h = lines.len() as u16 + 2;
            if y + h > bottom {
                break;
            }
            let rect = Rect::new(x, y, w, h);
            let (accent, title) = match t.level {
                Level::Info => (theme.info, " Info "),
                Level::Warn => (theme.warn, " Warning "),
                Level::Error => (theme.error, " Error "),
            };
            let body = Style::default().bg(theme.popup_bg).fg(theme.fg);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(accent.bg(theme.popup_bg))
                .title(title)
                .title_style(accent.bg(theme.popup_bg))
                .style(body);
            frame.render_widget(block, rect);
            let buf = frame.buffer_mut();
            buf.set_string(rect.right() - 3, rect.y, "×", theme.dim.bg(theme.popup_bg));
            for (i, line) in lines.iter().enumerate() {
                buf.set_stringn(rect.x + 2, rect.y + 1 + i as u16, line, (w - 4) as usize, body);
            }
            self.rects.push((t.id, rect));
            y += h;
        }
    }
}

/// Greedy wrap at spaces (or anywhere for long words) into at most
/// `max_lines` lines of `width` cells; a cut last line ends with `…`.
fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    for para in text.lines() {
        let mut cur = String::new();
        for word in para.split(' ') {
            let sep = usize::from(!cur.is_empty());
            if cur.width() + sep + word.width() <= width {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(word);
                continue;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            // A word longer than the line is split by cells.
            for c in word.chars() {
                if cur.width() + c.width().unwrap_or(0) > width {
                    lines.push(std::mem::take(&mut cur));
                }
                cur.push(c);
            }
        }
        lines.push(cur);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = &mut lines[max_lines - 1];
        *last = crate::util::text::fit(&format!("{last} …"), width);
        if !last.ends_with('…') {
            last.push('…');
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toasts_expire_by_level() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        ts.push(Level::Info, "saved", t0);
        ts.push(Level::Error, "push failed", t0);
        assert!(!ts.expire(t0 + Duration::from_secs(3)));
        assert_eq!(ts.items().len(), 2);
        assert!(ts.expire(t0 + ttl(Level::Info)));
        assert_eq!(ts.items().len(), 1);
        assert_eq!(ts.items()[0].level, Level::Error);
        assert!(!ts.expire(t0 + ttl(Level::Error) - Duration::from_millis(1)));
        assert!(ts.expire(t0 + ttl(Level::Error)));
        assert!(ts.is_empty());
    }

    #[test]
    fn repeats_restart_and_stack_is_capped() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        ts.push(Level::Info, "Nothing to undo", t0);
        ts.push(Level::Warn, "w", t0);
        let later = t0 + Duration::from_secs(3);
        ts.push(Level::Info, "Nothing to undo", later);
        assert_eq!(ts.items().len(), 2);
        // The repeat moved to the top and lives from its new time.
        assert_eq!(ts.items()[1].text, "Nothing to undo");
        assert!(!ts.expire(t0 + ttl(Level::Info)));
        for i in 0..10 {
            ts.push(Level::Info, &format!("n{i}"), later);
        }
        assert_eq!(ts.items().len(), MAX_TOASTS);
        assert_eq!(ts.items().last().unwrap().text, "n9");
    }

    #[test]
    fn click_dismisses_only_the_hit_toast() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        ts.push(Level::Info, "a", t0);
        ts.push(Level::Info, "b", t0);
        let (a, b) = (ts.items()[0].id, ts.items()[1].id);
        ts.rects = vec![(b, Rect::new(10, 1, 20, 3)), (a, Rect::new(10, 4, 20, 3))];
        assert!(!ts.dismiss_at(5, 2));
        assert!(ts.dismiss_at(12, 5));
        assert_eq!(ts.items().len(), 1);
        assert_eq!(ts.items()[0].text, "b");
    }

    #[test]
    fn wrap_limits_lines() {
        assert_eq!(wrap("hello world", 20, 3), vec!["hello world"]);
        assert_eq!(wrap("hello world", 6, 3), vec!["hello", "world"]);
        let w = wrap("aaaa bbbb cccc dddd eeee", 4, 2);
        assert_eq!(w.len(), 2);
        assert!(w[1].ends_with('…'));
        assert_eq!(wrap("abcdefgh", 3, 5), vec!["abc", "def", "gh"]);
    }
}
