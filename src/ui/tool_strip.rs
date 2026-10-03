//! The vertical strip of tool window buttons on the left edge.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::app::App;
use crate::ui::focus::{Side, ToolWindowId};

pub fn render(app: &mut App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::default().bg(theme.strip_bg));
    app.layout.strip_hits.clear();
    if area.height < 4 || area.width < 2 {
        return;
    }
    // Each button is its number and its title; when they do not all fit,
    // the longest titles are cut first.
    let mut heights: Vec<u16> = ToolWindowId::ALL.iter().map(|tw| (tw.title().chars().count() as u16 + 1).min(area.height / 2)).collect();
    while heights.iter().map(|h| h + 1).sum::<u16>() > area.height {
        let Some(max) = heights.iter_mut().max().filter(|h| **h > 2) else { break };
        *max -= 1;
    }
    // Left-side windows stack from the top, bottom-side windows from the bottom.
    let mut top = area.y;
    let mut bottom = area.bottom();
    for (tw, h) in ToolWindowId::ALL.into_iter().zip(heights) {
        let label: Vec<char> = tw.title().chars().collect();
        let rect = match tw.side() {
            Side::Left => {
                if top + h > bottom {
                    continue;
                }
                let r = Rect::new(area.x, top, area.width, h);
                top += h + 1;
                r
            }
            Side::Bottom => {
                if bottom < top + h {
                    continue;
                }
                bottom -= h;
                let r = Rect::new(area.x, bottom, area.width, h);
                bottom = bottom.saturating_sub(1);
                r
            }
        };
        let active = app.layout.is_visible(tw);
        let focused = app.focus.is(tw.panel());
        let style = if active { theme.strip_item_active } else { theme.strip_item };
        let style = if focused { style.add_modifier(ratatui::style::Modifier::UNDERLINED) } else { style };
        buf.set_style(rect, style);
        if let Some(n) = tw.number() {
            buf.set_string(rect.x + 1, rect.y, n.to_string(), style.add_modifier(ratatui::style::Modifier::BOLD));
        }
        for (i, ch) in label.iter().enumerate() {
            let y = rect.y + 1 + i as u16;
            if y >= rect.bottom() {
                break;
            }
            buf.set_string(rect.x + 1, y, ch.to_string(), style);
        }
        app.layout.strip_hits.push((tw, rect));
    }
}
