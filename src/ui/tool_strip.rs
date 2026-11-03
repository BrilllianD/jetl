//! The vertical strip on the left edge: the main menu button on top, then a
//! button per tool window (its Alt-number over its icon, or only the icon in
//! one row when they do not fit, or a More button for the rest), and the
//! tooltip of the hovered button. A second strip on the right edge holds the Right-side
//! windows. Buttons follow `LayoutState::order`; a left drag reorders or moves a
//! button, and the drop target is drawn in `strip_drop`.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, AppAction};
use crate::app::App;
use crate::config::Icons;
use crate::ui::focus::{Side, ToolWindowId};
use crate::ui::layout::{StripButton, StripDrop};
use crate::ui::menu_bar::menu_glyph;

/// Rows of a tool button: the Alt-number, then the icon.
const BUTTON_H: u16 = 2;

pub fn render(app: &mut App, frame: &mut Frame, area: Rect, right: Rect) {
    // Read before `strip_hits` is cleared: the target comes from the last frame.
    let drop = app.strip_drop();
    let order = app.layout.order().to_vec();
    let theme = &app.theme;
    let icons = app.config.ui.icons;
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::default().bg(theme.strip_bg));
    if !right.is_empty() {
        buf.set_style(right, Style::default().bg(theme.strip_bg));
    }
    app.layout.strip_hits.clear();
    if area.height < 1 || area.width < 2 {
        return;
    }
    let menu = Rect::new(area.x, area.y, area.width, 1);
    buf.set_style(menu, theme.strip_item);
    centred(buf, menu, menu_glyph(icons), theme.strip_item);
    app.layout.strip_hits.push((StripButton::Menu, menu));

    // Per strip: two-row buttons when all of them fit, else one-row icon-only
    // buttons, else one-row buttons for the first windows and a More button
    // on the strip's last row. Left-side windows stack from the top under the
    // menu button, bottom-side windows from the bottom.
    let n_all = ToolWindowId::ALL.len() as u16;
    let n_main = ToolWindowId::ALL.iter().filter(|tw| app.layout.side_of(**tw) != Side::Right).count() as u16;
    let n_right = n_all - n_main;
    let gap2 = u16::from(area.height > n_all * (BUTTON_H + 1));
    let h = area.height;
    let left_mode = if h > 2 * n_main {
        StripMode { height: BUTTON_H, gap: gap2, cap: usize::MAX, more: false }
    } else if h > n_main {
        StripMode { height: 1, gap: 0, cap: usize::MAX, more: false }
    } else if h >= 2 {
        StripMode { height: 1, gap: 0, cap: usize::from(h - 2), more: true }
    } else {
        StripMode { height: 1, gap: 0, cap: 0, more: false }
    };
    let menu_gap = match left_mode {
        StripMode { more: true, .. } => 0,
        StripMode { height: BUTTON_H, .. } => u16::from(h >= 2 + n_main * BUTTON_H),
        _ => u16::from(h >= 2 + n_main),
    };
    let rh = if right.width >= 2 { right.height } else { 0 };
    let right_mode = if rh >= 2 + 2 * n_right {
        StripMode { height: BUTTON_H, gap: gap2, cap: usize::MAX, more: false }
    } else if rh >= 2 + n_right {
        StripMode { height: 1, gap: 0, cap: usize::MAX, more: false }
    } else if rh >= 3 {
        StripMode { height: 1, gap: 0, cap: usize::from(rh - 3), more: true }
    } else {
        StripMode { height: 1, gap: 0, cap: 0, more: false }
    };
    let mut top = area.y + 1 + menu_gap;
    let mut bottom = area.bottom() - u16::from(left_mode.more);
    let mut rtop = right.y + 2;
    let rbottom = right.bottom() - u16::from(right_mode.more);
    let (mut placed_left, mut placed_right) = (0usize, 0usize);
    for tw in order {
        let side = app.layout.side_of(tw);
        let is_right = side == Side::Right;
        let (mode, placed) = if is_right { (right_mode, &mut placed_right) } else { (left_mode, &mut placed_left) };
        if *placed >= mode.cap {
            continue;
        }
        let bh = mode.height;
        let rect = match side {
            Side::Left => {
                if top + bh > bottom {
                    continue;
                }
                let r = Rect::new(area.x, top, area.width, bh);
                top += bh + mode.gap;
                r
            }
            Side::Bottom => {
                if bottom < top + bh {
                    continue;
                }
                bottom -= bh;
                let r = Rect::new(area.x, bottom, area.width, bh);
                bottom = bottom.saturating_sub(mode.gap);
                r
            }
            Side::Right => {
                if right.width < 2 || rtop + bh > rbottom {
                    continue;
                }
                let r = Rect::new(right.x, rtop, right.width, bh);
                rtop += bh + mode.gap;
                r
            }
        };
        *placed += 1;
        let active = app.layout.is_visible(tw);
        let focused = app.focus.is(tw.panel());
        let style = if active { theme.strip_item_active } else { theme.strip_item };
        let style = if focused { style.add_modifier(Modifier::UNDERLINED) } else { style };
        let style = if drop == Some(StripDrop::Onto(tw)) { style.patch(theme.strip_drop) } else { style };
        buf.set_style(rect, style);
        if bh == BUTTON_H {
            if let Some(n) = tw.number() {
                centred(buf, rect, &n.to_string(), style.add_modifier(Modifier::BOLD));
            }
            centred(buf, Rect { y: rect.y + 1, ..rect }, tw.icon(icons), style);
        } else {
            centred(buf, rect, tw.icon(icons), style);
        }
        app.layout.strip_hits.push((StripButton::Tool(tw), rect));
    }
    for (is_right, mode, strip) in [(false, left_mode, area), (true, right_mode, right)] {
        if !mode.more {
            continue;
        }
        let rect = Rect::new(strip.x, strip.bottom() - 1, strip.width, 1);
        let any_visible = app.layout.strip_overflow(is_right).into_iter().any(|tw| app.layout.is_visible(tw));
        let style = if any_visible { theme.strip_item_active } else { theme.strip_item };
        buf.set_style(rect, style);
        centred(buf, rect, more_glyph(icons), style);
        app.layout.strip_hits.push((StripButton::More, rect));
    }
    if let Some(StripDrop::End(side)) = drop {
        let strip = if side == Side::Right { right } else { area };
        let free = |row: u16| {
            let line = Rect::new(strip.x, row, strip.width, 1);
            !app.layout.strip_hits.iter().any(|(_, r)| r.intersects(line))
        };
        let row = match side {
            Side::Left => (area.y + 1..area.bottom()).find(|r| free(*r)),
            Side::Bottom => (area.y + 1..area.bottom()).rev().find(|r| free(*r)),
            // Only when the right strip holds no button, which a real drag cannot reach (it is drawn only with one).
            Side::Right => (right.y + 2..right.bottom()).find(|r| free(*r)),
        };
        if let Some(row) = row {
            let line = Rect::new(strip.x, row, strip.width, 1);
            let style = theme.strip_item.patch(theme.strip_drop);
            buf.set_style(line, style);
            for x in line.x..line.right() {
                buf[(x, row)].set_symbol("━");
            }
        }
    }
}

/// How one strip lays out its buttons: their height, the blank rows between
/// them, how many get a button (`cap`), and whether the last row is More.
#[derive(Clone, Copy)]
struct StripMode {
    height: u16,
    gap: u16,
    cap: usize,
    more: bool,
}

/// The glyph of a strip's More button; one cell in both icon modes.
pub fn more_glyph(_icons: Icons) -> &'static str {
    "\u{22ef}"
}

/// `text` centred by display width on the first row of `rect`.
fn centred(buf: &mut Buffer, rect: Rect, text: &str, style: Style) {
    let x = rect.x + rect.width.saturating_sub(text.width() as u16) / 2;
    // The button's colours, already painted, to restore below.
    let under = buf[(x, rect.y)].style().patch(style);
    let (end, _) = buf.set_stringn(x, rect.y, text, rect.right().saturating_sub(x) as usize, style);
    // set_stringn resets the cell behind a wide glyph, colours included.
    buf.set_style(Rect::new(x, rect.y, end - x, 1), under);
}

/// The hovered button's name and shortcut on one line right of it. Hidden
/// while an overlay is open, and once the button is gone from the strip.
pub fn render_tooltip(app: &App, frame: &mut Frame, area: Rect) {
    let Some(hover) = app.strip_hover else { return };
    if app.palette.is_some()
        || app.dialog.is_some()
        || app.menu.is_some()
        || app.menu_bar.is_some()
        || app.settings.is_some()
        || app.run_configs.is_some()
        || !app.layout.strip_hits.contains(&hover)
    {
        return;
    }
    let (button, rect) = hover;
    let (name, action) = match button {
        StripButton::Menu => ("Main Menu", Some(AppAction::MainMenu)),
        StripButton::Tool(tw) => (tw.title(), Some(AppAction::ToggleToolWindow(tw))),
        StripButton::More => ("More Tool Windows", None),
    };
    let key = action.and_then(|a| app.shortcut(&Action::App(a))).map(|k| format!(" {k} ")).unwrap_or_default();
    let theme = &app.theme;
    let base = Style::default().bg(theme.popup_bg).fg(theme.fg);
    let name = format!(" {name} ");
    let w = (name.width() + key.width()) as u16;
    // A right-strip button's tooltip goes to its left.
    let (x, room) = if app.layout.rects.strip_right.contains(rect.as_position()) {
        let x = rect.x.saturating_sub(w).max(area.x);
        (x, rect.x - x)
    } else {
        (rect.right(), area.right().saturating_sub(rect.right()))
    };
    if room == 0 {
        return;
    }
    let buf = frame.buffer_mut();
    buf.set_style(Rect::new(x, rect.y, w.min(room), 1), base);
    let (end, _) = buf.set_stringn(x, rect.y, &name, room as usize, base);
    buf.set_stringn(end, rect.y, &key, (x + room).saturating_sub(end) as usize, base.patch(theme.dim));
}
