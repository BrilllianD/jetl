//! Bottom status bar: project name, branch, notifications, cursor position.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::event::Level;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusHit {
    Branch,
    Position,
    Notification,
}

pub fn render(app: &mut App, frame: &mut Frame, area: Rect) {
    let theme = app.theme.clone();
    let buf = frame.buffer_mut();
    let base = Style::default().bg(theme.status_bg).fg(theme.status_fg);
    buf.set_style(area, base);
    app.status_hits.clear();
    if area.width < 10 {
        return;
    }
    let mut x = area.x + 1;
    let name = app.workspace.name();
    buf.set_stringn(x, area.y, &name, area.width as usize, base.add_modifier(ratatui::style::Modifier::BOLD));
    x += name.width() as u16 + 2;

    if let Some(branch) = app.branch_label() {
        let s = format!(" {branch}");
        let w = s.width() as u16;
        let r = Rect::new(x, area.y, w.min(area.right().saturating_sub(x)), 1);
        buf.set_stringn(x, area.y, &s, r.width as usize, theme.status_accent.bg(theme.status_bg));
        app.status_hits.push((StatusHit::Branch, r));
        x += w + 2;
    }

    // Right side: mode (modal only), position, line ending, keyboard mode.
    let mut right_parts: Vec<(String, Style)> = Vec::new();
    if let Some(pos) = app.editor.status_position() {
        right_parts.push((pos, base));
    }
    if let Some(le) = app.editor.status_line_ending() {
        right_parts.push((le, base));
    }
    if app.dispatcher.modal {
        right_parts.push((app.mode.label().to_string(), theme.status_accent.bg(theme.status_bg)));
    }
    if !app.keyboard_enhanced {
        right_parts.push(("legacy keys".into(), theme.dim.bg(theme.status_bg)));
    }
    let mut rx = area.right();
    for (s, st) in right_parts.iter().rev() {
        let w = s.width() as u16 + 2;
        if rx < x + w {
            break;
        }
        rx -= w;
        buf.set_stringn(rx + 1, area.y, s, w as usize, *st);
        if s.starts_with("Ln") {
            app.status_hits.push((StatusHit::Position, Rect::new(rx, area.y, w, 1)));
        }
    }

    // Middle: pending chord or latest notification.
    let middle_w = rx.saturating_sub(x) as usize;
    if middle_w > 4 {
        if let Some(p) = app.dispatcher.pending_display() {
            buf.set_stringn(x, area.y, format!("{p} …"), middle_w, theme.status_accent.bg(theme.status_bg));
        } else if let Some(n) = app.current_notification() {
            let st = match n.level {
                Level::Info => theme.info,
                Level::Warn => theme.warn,
                Level::Error => theme.error,
            }
            .bg(theme.status_bg);
            let text = crate::util::text::fit(&n.text, middle_w);
            buf.set_stringn(x, area.y, &text, middle_w, st);
            app.status_hits.push((StatusHit::Notification, Rect::new(x, area.y, middle_w as u16, 1)));
        }
    }
}
