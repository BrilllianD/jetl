//! The vertical strip on the left edge: the main menu button on top (unless
//! the toolbar row shows it), then a
//! button per tool window (its Alt-number over its icon, or only the icon in
//! one row when they do not fit, or a More button for the rest), and the
//! tooltip of the hovered button. A second strip on the right edge holds the Right-side
//! windows and the Bottom Right ones. Each strip draws three groups: Left Top, Left
//! Bottom (below a one-row `─` separator, drawn only when both have a button) and Bottom
//! Left from the foot; the right strip mirrors it with Bottom Right at its foot. Buttons
//! follow `LayoutState::order` inside a group; a left drag reorders or moves a button,
//! and the drop target is drawn in `strip_drop`.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, AppAction};
use crate::app::App;
use crate::config::Icons;
use crate::ui::focus::{Dock, Side, ToolWindowId};
use crate::ui::layout::{LayoutState, StripButton, StripDrop};
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
    // The toolbar row hosts the menu button while it shows.
    let menu_rows = u16::from(app.layout.rects.toolbar.is_empty());
    if menu_rows == 1 {
        let menu = Rect::new(area.x, area.y, area.width, 1);
        buf.set_style(menu, theme.strip_item);
        centred(buf, menu, menu_glyph(icons), theme.strip_item);
        app.layout.strip_hits.push((StripButton::Menu, menu));
    }

    // Per strip: two-row buttons when all of them fit, else one-row icon-only
    // buttons, else one-row buttons for the first windows and a More button
    // on the strip's last row. Left-side windows stack from the top under the
    // menu button, bottom-side windows from the bottom.
    let n_all = ToolWindowId::ALL.len() as u16;
    let n_right = ToolWindowId::ALL.iter().filter(|tw| app.layout.on_right_strip(**tw)).count() as u16;
    let n_main = n_all - n_right;
    let has = |d: Dock| ToolWindowId::ALL.iter().any(|t| app.layout.dock_of(*t) == d);
    let [lt, lb, bl] = LayoutState::strip_docks(false);
    let [rt, rb, br] = LayoutState::strip_docks(true);
    let sep_l = u16::from(has(lt) && has(lb));
    let sep_r = u16::from(has(rt) && has(rb));
    let gap2 = u16::from(area.height > n_all * (BUTTON_H + 1) + 2 * sep_l.max(sep_r));
    let h = area.height;
    let avail = h.saturating_sub(menu_rows + sep_l);
    let left_mode = strip_mode(avail, n_main, gap2, h > menu_rows);
    let menu_gap = match left_mode {
        _ if menu_rows == 0 => 0,
        StripMode { more: true, .. } => 0,
        StripMode { height: BUTTON_H, .. } => u16::from(h >= 2 + n_main * BUTTON_H + sep_l),
        _ => u16::from(h >= 2 + n_main + sep_l),
    };
    let rh = if right.width >= 2 { right.height } else { 0 };
    let roff = 2 * menu_rows;
    let ravail = rh.saturating_sub(roff + sep_r);
    let right_mode = strip_mode(ravail, n_right, gap2, rh >= 1);
    let mut top = area.y + menu_rows + menu_gap;
    let mut bottom = area.bottom() - u16::from(left_mode.more);
    let mut rtop = right.y + roff;
    let mut rbottom = right.bottom() - u16::from(right_mode.more);
    // Who gets a button when a strip is short: the first `cap` windows in `order`.
    // `strip_mode` leaves room for every window it admits, so one that gets no button is always listed by a More cell.
    let mut seen = (0usize, 0usize);
    let allowed: Vec<ToolWindowId> = order
        .iter()
        .copied()
        .filter(|tw| {
            let (mode, n) = if app.layout.on_right_strip(*tw) { (right_mode, &mut seen.1) } else { (left_mode, &mut seen.0) };
            *n += 1;
            *n <= mode.cap
        })
        .collect();
    let mut seps: Vec<Rect> = Vec::new();
    let (mut upper_l, mut upper_r) = (0usize, 0usize);
    let (mut sep_l_drawn, mut sep_r_drawn) = (false, false);
    for dock in [lt, lb, bl, rt, rb, br] {
        let group: Vec<ToolWindowId> = order.iter().copied().filter(|tw| app.layout.dock_of(*tw) == dock && allowed.contains(tw)).collect();
        for tw in group {
            let is_right = app.layout.on_right_strip(tw);
            let mode = if is_right { right_mode } else { left_mode };
            let bh = mode.height;
            let rect = if dock == lt || dock == lb {
                if dock == lb && upper_l > 0 && !sep_l_drawn {
                    if top + 1 + mode.gap + bh > bottom {
                        continue;
                    }
                    seps.push(Rect::new(area.x, top, area.width, 1));
                    top += 1 + mode.gap;
                    sep_l_drawn = true;
                }
                if top + bh > bottom {
                    continue;
                }
                let r = Rect::new(area.x, top, area.width, bh);
                top += bh + mode.gap;
                if dock == lt {
                    upper_l += 1;
                }
                r
            } else if dock == bl {
                if bottom < top + bh {
                    continue;
                }
                bottom -= bh;
                let r = Rect::new(area.x, bottom, area.width, bh);
                bottom = bottom.saturating_sub(mode.gap);
                r
            } else if dock == rt || dock == rb {
                if right.width < 2 {
                    continue;
                }
                if dock == rb && upper_r > 0 && !sep_r_drawn {
                    if rtop + 1 + mode.gap + bh > rbottom {
                        continue;
                    }
                    seps.push(Rect::new(right.x, rtop, right.width, 1));
                    rtop += 1 + mode.gap;
                    sep_r_drawn = true;
                }
                if rtop + bh > rbottom {
                    continue;
                }
                let r = Rect::new(right.x, rtop, right.width, bh);
                rtop += bh + mode.gap;
                if dock == rt {
                    upper_r += 1;
                }
                r
            } else {
                if right.width < 2 || rbottom < rtop + bh {
                    continue;
                }
                rbottom -= bh;
                let r = Rect::new(right.x, rbottom, right.width, bh);
                rbottom = rbottom.saturating_sub(mode.gap);
                r
            };
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
    }
    for r in seps {
        buf.set_style(r, theme.strip_item);
        for x in r.x..r.right() {
            buf[(x, r.y)].set_symbol("─");
        }
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
    if let Some(StripDrop::End(dock)) = drop {
        let right_strip = dock.side == Side::Right || (dock.side == Side::Bottom && dock.split);
        let (strip, start) = if right_strip { (right, right.y + roff) } else { (area, area.y + menu_rows) };
        let free = |row: u16| {
            let line = Rect::new(strip.x, row, strip.width, 1);
            !app.layout.strip_hits.iter().any(|(_, r)| r.intersects(line))
        };
        // The right strip is reachable on a very short terminal, where it shows only More; the bar
        // is then never drawn, as no free row exists at or below `right.y + roff`.
        let row = if dock.side == Side::Bottom {
            (start..strip.bottom()).rev().find(|r| free(*r))
        } else if dock.split {
            let from = app.layout.strip_upper_end(right_strip).unwrap_or(start).max(start);
            (from..strip.bottom()).find(|r| free(*r))
        } else {
            (start..strip.bottom()).find(|r| free(*r))
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

/// How a strip with `n` windows lays out when `avail` rows are left for its buttons. `room`: the strip
/// has a row for a More cell even when it has none for a button (the rows `avail` leaves out for the
/// separator and the right strip's alignment are not drawn then).
fn strip_mode(avail: u16, n: u16, gap: u16, room: bool) -> StripMode {
    if avail >= 2 * n {
        StripMode { height: BUTTON_H, gap, cap: usize::MAX, more: false }
    } else if avail >= n {
        StripMode { height: 1, gap: 0, cap: usize::MAX, more: false }
    } else if avail >= 1 {
        StripMode { height: 1, gap: 0, cap: usize::from(avail - 1), more: true }
    } else {
        StripMode { height: 1, gap: 0, cap: 0, more: room }
    }
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

/// An overlay is open, so a hover tooltip stays hidden.
pub(crate) fn overlay_open(app: &App) -> bool {
    app.palette.is_some()
        || app.dialog.is_some()
        || app.menu.is_some()
        || app.menu_bar.is_some()
        || app.settings.is_some()
        || app.run_configs.is_some()
}

/// The hovered button's name and shortcut on one line right of it. Hidden
/// while an overlay is open, and once the button is gone from the strip.
pub fn render_tooltip(app: &App, frame: &mut Frame, area: Rect) {
    let Some(hover) = app.strip_hover else { return };
    if overlay_open(app) || !app.layout.strip_hits.contains(&hover) {
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
