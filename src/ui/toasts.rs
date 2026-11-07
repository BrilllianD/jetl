//! Notification toasts: stacked boxes in the top-right corner that expire on
//! their own, timed from the first draw (errors last longest) and close on click. The status bar keeps
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
const MAX_LINES: usize = 4;
/// The narrowest a toast gets when it moves right of a gutter.
const MIN_W: u16 = 30;

#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub level: Level,
    pub text: String,
    /// When it was first drawn; its time starts there.
    pub shown: Option<Instant>,
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
    /// that toast, from its next draw, instead of stacking a copy.
    pub fn push(&mut self, level: Level, text: &str) {
        if let Some(i) = self.items.iter().position(|t| t.text == text && t.level == level) {
            let mut t = self.items.remove(i);
            t.shown = None;
            self.items.push(t);
            return;
        }
        self.next_id += 1;
        self.items.push(Toast { id: self.next_id, level, text: text.to_string(), shown: None });
        if self.items.len() > MAX_TOASTS {
            self.items.remove(0);
        }
    }

    /// Drop toasts whose time is up; one never drawn keeps its time. Returns
    /// true when any was removed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.items.len();
        self.items.retain(|t| t.shown.is_none_or(|at| now.saturating_duration_since(at) < ttl(t.level)));
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

    /// Draw the toasts in their right-hand column. A toast that would cover
    /// a rect of `avoid` (open overlays) moves down to the first free row
    /// below it; one that no longer fits is not drawn. The left edge keeps
    /// right of any of `gutters` (editor line numbers and marks) it would touch,
    /// when it can clear them and keep `MIN_W` cells; other gutters stay covered.
    /// A toast drawn for the first time is stamped `now`.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, avoid: &[Rect], gutters: &[Rect], now: Instant) {
        self.rects.clear();
        if self.items.is_empty() || area.width < 24 || area.height < 6 {
            return;
        }
        let right = area.right().saturating_sub(1);
        let mut w = (area.width / 2).clamp(22, 52);
        let mut x = right.saturating_sub(w);
        // A toast's edge on a gutter hides its marks: start right of it. Only a gutter
        // that leaves MIN_W cells counts; a toast narrower than that would still cover it.
        let room = right.saturating_sub(MIN_W);
        if let Some(clear) =
            gutters.iter().filter(|g| !g.is_empty() && g.x < right && g.right() > x && g.right() <= room).map(|g| g.right()).max()
        {
            x = clear;
            w = right - x;
        }
        let avoid: Vec<Rect> = avoid.iter().copied().filter(|r| !r.is_empty()).collect();
        // Keep the lower half free so toasts never cover much of the editor. The first
        // one may go lower, cut to the lines that fit above the status bar row, or a
        // short terminal would never show it; around overlays they may reach down to
        // that row.
        let half = area.y + area.height / 2;
        let full = area.bottom().saturating_sub(1);
        let mut y = area.y + 1;
        for t in self.items.iter_mut().rev() {
            let first = avoid.is_empty() && self.rects.is_empty();
            let max_lines = if first { MAX_LINES.min(usize::from(full.saturating_sub(y)).saturating_sub(2)) } else { MAX_LINES };
            let lines = wrap(&t.text, (w - 4) as usize, max_lines);
            let h = lines.len() as u16 + 2;
            let mut rect = Rect::new(x, y, w, h);
            while let Some(below) = avoid.iter().filter(|r| r.intersects(rect)).map(|r| r.bottom()).max() {
                y = below;
                rect = Rect::new(x, y, w, h);
            }
            let limit = if avoid.is_empty() && !self.rects.is_empty() { half } else { full };
            if y + h > limit {
                break;
            }
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
            t.shown.get_or_insert(now);
            y += h;
        }
    }
}

/// Greedy wrap at spaces (or anywhere for long words) into at most
/// `max_lines` lines of `width` cells; a cut last line ends with `…`.
pub(crate) fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn toasts_go_below_overlays() {
        let theme = Theme::default();
        let mut ts = Toasts::default();
        ts.push(Level::Info, "one");
        ts.push(Level::Warn, "two");
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let area = Rect::new(0, 0, 80, 24);
        let overlay = Rect::new(2, 3, 76, 7);
        term.draw(|f| ts.render(f, area, &theme, &[overlay], &[], Instant::now())).unwrap();
        assert_eq!(ts.rects.len(), 2);
        for (_, r) in &ts.rects {
            assert!(r.y >= 10, "{r:?}");
            assert!(!r.intersects(overlay));
        }
        term.draw(|f| ts.render(f, area, &theme, &[area], &[], Instant::now())).unwrap();
        assert!(ts.rects.is_empty());
        term.draw(|f| ts.render(f, area, &theme, &[], &[], Instant::now())).unwrap();
        assert_eq!(ts.rects[0].1.y, 1);
    }

    #[test]
    fn toasts_expire_by_level() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        ts.push(Level::Info, "saved");
        ts.push(Level::Error, "push failed");
        draw_at(&mut ts, &[], t0);
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
        ts.push(Level::Info, "Nothing to undo");
        ts.push(Level::Warn, "w");
        draw_at(&mut ts, &[], t0);
        let later = t0 + Duration::from_secs(3);
        ts.push(Level::Info, "Nothing to undo");
        draw_at(&mut ts, &[], later);
        assert_eq!(ts.items().len(), 2);
        // The repeat moved to the top and lives from its new time.
        assert_eq!(ts.items()[1].text, "Nothing to undo");
        assert!(!ts.expire(t0 + ttl(Level::Info)));
        assert!(!ts.expire(later + ttl(Level::Info) - Duration::from_millis(1)));
        for i in 0..10 {
            ts.push(Level::Info, &format!("n{i}"));
        }
        assert_eq!(ts.items().len(), MAX_TOASTS);
        assert_eq!(ts.items().last().unwrap().text, "n9");
    }

    fn draw_rects(ts: &mut Toasts, area: Rect, avoid: &[Rect], gutters: &[Rect]) -> Vec<Rect> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| ts.render(f, area, &theme, avoid, gutters, Instant::now())).unwrap();
        ts.rects.iter().map(|(_, r)| *r).collect()
    }

    #[test]
    fn toasts_stay_right_of_a_gutter() {
        let area = Rect::new(0, 0, 80, 24);
        let at = |gutters: &[Rect]| {
            let mut ts = Toasts::default();
            ts.push(Level::Info, "short");
            let r = draw_rects(&mut ts, area, &[], gutters);
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].right(), 79, "{gutters:?}");
            (r[0].x, r[0].width)
        };
        assert_eq!(at(&[]), (39, 40));
        assert_eq!(at(&[Rect::new(3, 2, 7, 21)]), (39, 40));
        assert_eq!(at(&[Rect::new(36, 2, 7, 21)]), (43, 36));
        assert_eq!(at(&[Rect::new(36, 2, 7, 21), Rect::new(3, 2, 7, 21)]), (43, 36));
        assert_eq!(at(&[Rect::new(3, 2, 7, 21), Rect::new(36, 2, 7, 21)]), (43, 36));
        // A gutter no toast of MIN_W cells can clear is left alone.
        assert_eq!(at(&[Rect::new(60, 2, 7, 21)]), (39, 40));
        assert_eq!(at(&[Rect::new(36, 2, 7, 21), Rect::new(58, 2, 7, 21)]), (43, 36));
        assert_eq!(at(&[Rect::new(40, 2, 0, 21)]), (39, 40));
    }

    #[test]
    fn narrow_area_keeps_its_width() {
        let mut ts = Toasts::default();
        ts.push(Level::Info, "short");
        let r = draw_rects(&mut ts, Rect::new(0, 0, 40, 24), &[], &[Rect::new(20, 2, 7, 21)]);
        assert_eq!((r[0].x, r[0].width), (17, 22));
    }

    #[test]
    fn lower_half_stays_free() {
        let area = Rect::new(0, 0, 80, 24);
        let mut ts = Toasts::default();
        for t in ["a", "b", "c", "d"] {
            ts.push(Level::Info, t);
        }
        let r = draw_rects(&mut ts, area, &[], &[]);
        assert_eq!(r.len(), 3);
        assert_eq!(r.iter().map(|r| r.y).collect::<Vec<_>>(), vec![1, 4, 7]);
        assert!(r.iter().all(|r| r.bottom() <= 12), "{r:?}");
        // The newest is on top.
        assert_eq!(ts.rects[0].0, ts.items().last().unwrap().id);

        let long = |c: char| format!("{0} {0} {1}", c.to_string().repeat(30), c.to_string().repeat(10));
        let mut ts = Toasts::default();
        ts.push(Level::Info, &long('x'));
        ts.push(Level::Info, &long('y'));
        ts.push(Level::Info, &long('z'));
        let r = draw_rects(&mut ts, area, &[], &[Rect::new(36, 2, 7, 21)]);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[1].bottom(), 11, "{r:?}");
        assert!(r.iter().all(|r| r.x == 43));
    }

    #[test]
    fn first_toast_fits_a_short_terminal() {
        let mut ts = Toasts::default();
        ts.push(Level::Info, "one\ntwo\nthree");
        let r = draw_rects(&mut ts, Rect::new(0, 0, 80, 8), &[], &[]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].y, 1);
    }

    #[test]
    fn first_toast_is_cut_to_fit_heights_6_and_7() {
        let t0 = Instant::now();
        for (h, lines) in [(6u16, 2u16), (7, 3)] {
            let mut ts = Toasts::default();
            ts.push(Level::Warn, "one\ntwo\nthree\nfour\nfive");
            let theme = Theme::default();
            let mut term = Terminal::new(TestBackend::new(80, h)).unwrap();
            let buf = term.draw(|f| ts.render(f, Rect::new(0, 0, 80, h), &theme, &[], &[], t0)).unwrap().buffer.clone();
            assert_eq!(ts.rects.len(), 1, "height {h}");
            assert_eq!(ts.rects[0].1.height, lines + 2, "height {h}");
            let last: String = (0..80).map(|x| buf[(x, lines + 1)].symbol().to_string()).collect();
            assert!(last.contains('…'), "height {h}: {last:?}");
            assert!(ts.items()[0].shown.is_some());
            assert!(!ts.expire(t0 + ttl(Level::Warn) - Duration::from_millis(1)));
            assert!(ts.expire(t0 + ttl(Level::Warn)));
        }
    }

    #[test]
    fn long_message_reads_in_full_beside_a_gutter() {
        let mut ts = Toasts::default();
        ts.push(Level::Warn, "Language servers are off, so completion, hover and diagnostics are unavailable until they are on");
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let gutter = [Rect::new(36, 2, 7, 21)];
        let buf = term.draw(|f| ts.render(f, Rect::new(0, 0, 80, 24), &theme, &[], &gutter, Instant::now())).unwrap().buffer.clone();
        let row = |y: u16| (43..79).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
        let rows: Vec<String> = (1..7).map(row).collect();
        assert_eq!(ts.rects[0].1.height, 6, "{rows:?}");
        assert_eq!(rows[4].trim_matches('│').trim(), "until they are on", "{rows:?}");
        assert!(rows.iter().all(|r| !r.contains('…')), "{rows:?}");
    }

    fn draw_at(ts: &mut Toasts, avoid: &[Rect], now: Instant) {
        let area = Rect::new(0, 0, 80, 24);
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| ts.render(f, area, &theme, avoid, &[], now)).unwrap();
    }

    #[test]
    fn a_toast_that_cannot_draw_keeps_its_time() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        ts.push(Level::Warn, "held");
        draw_at(&mut ts, &[Rect::new(0, 0, 80, 24)], t0);
        assert!(ts.rects.is_empty());
        assert!(!ts.expire(t0 + Duration::from_secs(3600)));
        assert_eq!(ts.items().len(), 1);
        let t1 = t0 + Duration::from_secs(3600);
        draw_at(&mut ts, &[], t1);
        assert_eq!(ts.rects.len(), 1);
        assert!(!ts.expire(t1 + ttl(Level::Warn) - Duration::from_millis(1)));
        assert!(ts.expire(t1 + ttl(Level::Warn)));
        assert!(ts.is_empty());
    }

    #[test]
    fn only_the_first_draw_starts_the_time() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        ts.push(Level::Info, "once");
        draw_at(&mut ts, &[], t0);
        draw_at(&mut ts, &[], t0 + Duration::from_secs(3));
        assert!(ts.expire(t0 + ttl(Level::Info)));
    }

    #[test]
    fn a_drawn_toast_runs_on_under_an_overlay() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        ts.push(Level::Warn, "seen");
        draw_at(&mut ts, &[], t0);
        draw_at(&mut ts, &[Rect::new(0, 0, 80, 24)], t0 + Duration::from_secs(1));
        assert!(ts.rects.is_empty());
        assert!(ts.expire(t0 + ttl(Level::Warn)));
    }

    #[test]
    fn a_toast_below_a_full_stack_waits() {
        let t0 = Instant::now();
        let mut ts = Toasts::default();
        let long = |c: char| format!("{0} {0} {1}", c.to_string().repeat(30), c.to_string().repeat(10));
        ts.push(Level::Info, &long('x'));
        ts.push(Level::Info, &long('y'));
        ts.push(Level::Info, &long('z'));
        draw_at(&mut ts, &[], t0);
        assert_eq!(ts.rects.len(), 2);
        assert!(ts.expire(t0 + ttl(Level::Info)));
        assert_eq!(ts.items().len(), 1);
        assert!(ts.items()[0].text.starts_with('x'));
        let t1 = t0 + ttl(Level::Info);
        draw_at(&mut ts, &[], t1);
        assert_eq!(ts.rects.len(), 1);
        assert!(!ts.expire(t1 + ttl(Level::Info) - Duration::from_millis(1)));
        assert!(ts.expire(t1 + ttl(Level::Info)));
    }

    #[test]
    fn click_dismisses_only_the_hit_toast() {
        let mut ts = Toasts::default();
        ts.push(Level::Info, "a");
        ts.push(Level::Info, "b");
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
