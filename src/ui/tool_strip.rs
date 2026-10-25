//! The vertical strip on the left edge: the main menu button on top, then a
//! button per tool window (its Alt-number over its icon), and the tooltip of
//! the hovered button. A second strip on the right edge holds the Right-side
//! windows.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, AppAction};
use crate::app::App;
use crate::ui::focus::{Side, ToolWindowId};
use crate::ui::layout::StripButton;
use crate::ui::menu_bar::menu_glyph;

/// Rows of a tool button: the Alt-number, then the icon.
const BUTTON_H: u16 = 2;

pub fn render(app: &mut App, frame: &mut Frame, area: Rect, right: Rect) {
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

    // A blank row between buttons when that fits; buttons that still do not
    // fit are skipped. Left-side windows stack from the top under the menu
    // button, bottom-side windows from the bottom.
    let gap = u16::from(area.height > ToolWindowId::ALL.len() as u16 * (BUTTON_H + 1));
    // The blank row under the menu button goes when the left strip's
    // buttons would not fit with it (24-row terminals).
    let main_buttons = ToolWindowId::ALL.iter().filter(|tw| app.layout.side_of(**tw) != Side::Right).count() as u16;
    let menu_gap = u16::from(area.height >= 2 + main_buttons * BUTTON_H);
    let mut top = area.y + 1 + menu_gap;
    let mut bottom = area.bottom();
    let mut rtop = right.y + 2;
    let rbottom = right.bottom();
    for tw in ToolWindowId::ALL {
        let rect = match app.layout.side_of(tw) {
            Side::Left => {
                if top + BUTTON_H > bottom {
                    continue;
                }
                let r = Rect::new(area.x, top, area.width, BUTTON_H);
                top += BUTTON_H + gap;
                r
            }
            Side::Bottom => {
                if bottom < top + BUTTON_H {
                    continue;
                }
                bottom -= BUTTON_H;
                let r = Rect::new(area.x, bottom, area.width, BUTTON_H);
                bottom = bottom.saturating_sub(gap);
                r
            }
            Side::Right => {
                if right.width < 2 || rtop + BUTTON_H > rbottom {
                    continue;
                }
                let r = Rect::new(right.x, rtop, right.width, BUTTON_H);
                rtop += BUTTON_H + gap;
                r
            }
        };
        let active = app.layout.is_visible(tw);
        let focused = app.focus.is(tw.panel());
        let style = if active { theme.strip_item_active } else { theme.strip_item };
        let style = if focused { style.add_modifier(Modifier::UNDERLINED) } else { style };
        buf.set_style(rect, style);
        if let Some(n) = tw.number() {
            centred(buf, rect, &n.to_string(), style.add_modifier(Modifier::BOLD));
        }
        centred(buf, Rect { y: rect.y + 1, ..rect }, tw.icon(icons), style);
        app.layout.strip_hits.push((StripButton::Tool(tw), rect));
    }
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
        StripButton::Menu => ("Main Menu", AppAction::MainMenu),
        StripButton::Tool(tw) => (tw.title(), AppAction::ToggleToolWindow(tw)),
    };
    let key = app.shortcut(&Action::App(action)).map(|k| format!(" {k} ")).unwrap_or_default();
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
