//! The top row (`[ui] toolbar`): a left group from the left edge and a right
//! group flush right, built from `Item`s by `place` and collapsed by numbered
//! steps when the row is too narrow. A press runs the button's action.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::actions::{Action, AppAction};
use crate::app::App;
use crate::ui::layout::ToolbarButton;
use crate::ui::menu_bar::menu_glyph;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Left,
    Right,
}

/// What a collapse step does to an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shrink {
    /// Remove the item.
    Drop,
    /// Replace the item's text.
    Text(String),
}

/// One widget of the row. `text` includes the item's own padding.
#[derive(Debug, Clone)]
pub struct Item {
    pub button: ToolbarButton,
    pub group: Group,
    pub text: String,
    pub style: Style,
    /// Collapse steps that apply to this item, by step number. An item without
    /// a `Drop` step is never dropped.
    pub shrinks: Vec<(u8, Shrink)>,
}

pub const STEP_SETTINGS: u8 = 1;
pub const STEP_SEARCH: u8 = 2;
pub const STEP_PROJECT: u8 = 3;
pub const STEP_BRANCH_CUT: u8 = 4;
pub const STEP_BRANCH_DROP: u8 = 5;
pub const STEP_RUN_CONFIG_CUT: u8 = 6;

/// `name` cut to `cols` columns, with `…` after the cut.
pub fn shorten(name: &str, cols: usize) -> String {
    if name.width() <= cols {
        return name.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in name.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > cols {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

fn width(item: &Item) -> u16 {
    item.text.width() as u16
}

fn group_width(items: &[Item], group: Group) -> u16 {
    let mut n = 0u16;
    let mut sum = 0u16;
    for i in items.iter().filter(|i| i.group == group) {
        sum = sum.saturating_add(width(i));
        n += 1;
    }
    sum.saturating_add(n.saturating_sub(1))
}

fn needed(items: &[Item]) -> u16 {
    let both = items.iter().any(|i| i.group == Group::Left) && items.iter().any(|i| i.group == Group::Right);
    group_width(items, Group::Left).saturating_add(group_width(items, Group::Right)).saturating_add(u16::from(both))
}

/// Lays `items` out on the first row of `area`: collapse steps 1 to 6 while
/// the row is too narrow, then clip. Returns the kept items with their final
/// text and visible cells, in input order.
pub fn place(mut items: Vec<Item>, area: Rect) -> Vec<(Item, Rect)> {
    for step in 1..=6u8 {
        if needed(&items) <= area.width {
            break;
        }
        items.retain_mut(|item| {
            for (n, shrink) in item.shrinks.clone() {
                if n != step {
                    continue;
                }
                match shrink {
                    Shrink::Drop => return false,
                    Shrink::Text(t) => item.text = t,
                }
            }
            true
        });
    }
    let left_first = items.iter().find(|i| i.group == Group::Left).map_or(0, width);
    let right_w = group_width(&items, Group::Right);
    let has_right = items.iter().any(|i| i.group == Group::Right);
    let right_x = area.right().saturating_sub(right_w).max(area.x.saturating_add(left_first).saturating_add(1));
    let limit = if has_right { right_x.saturating_sub(1) } else { area.right() };
    let (mut lx, mut rx) = (area.x, right_x);
    let mut out = Vec::new();
    for item in items {
        let w = width(&item);
        let (x, end) = match item.group {
            Group::Left => (&mut lx, limit),
            Group::Right => (&mut rx, area.right()),
        };
        let visible = w.min(end.saturating_sub(*x));
        let rect = Rect::new(*x, area.y, visible, 1);
        *x = x.saturating_add(w).saturating_add(1);
        if visible > 0 {
            out.push((item, rect));
        }
    }
    out
}

/// The widgets in group order (left, then right); 351-353 add theirs here.
fn items(app: &App) -> Vec<Item> {
    vec![Item {
        button: ToolbarButton::Menu,
        group: Group::Left,
        text: format!(" {} ", menu_glyph(app.config.ui.icons)),
        style: app.theme.strip_item,
        shrinks: vec![],
    }]
}

/// The action a press on `button` runs.
pub fn action(button: ToolbarButton) -> Action {
    match button {
        ToolbarButton::Menu => Action::App(AppAction::MainMenu),
    }
}

/// The name shown in the tooltip.
pub fn title(button: ToolbarButton) -> &'static str {
    match button {
        ToolbarButton::Menu => "Main Menu",
    }
}

pub fn render(app: &mut App, frame: &mut Frame, area: Rect) {
    app.layout.toolbar_hits.clear();
    if area.is_empty() {
        return;
    }
    let placed = place(items(app), area);
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::default().bg(app.theme.strip_bg));
    for (item, rect) in placed {
        buf.set_style(rect, item.style);
        let under = buf[(rect.x, rect.y)].style();
        let (end, _) = buf.set_stringn(rect.x, rect.y, &item.text, rect.width as usize, item.style);
        // set_stringn resets the cell behind a wide glyph, colours included.
        buf.set_style(Rect::new(rect.x, rect.y, end - rect.x, 1), under);
        app.layout.toolbar_hits.push((item.button, rect));
    }
}

/// The hovered button's name and shortcut on the row below it. Hidden while
/// an overlay is open, and once the button is gone from the row.
pub fn render_tooltip(app: &App, frame: &mut Frame, area: Rect) {
    let Some(hover) = app.toolbar_hover else { return };
    if app.palette.is_some()
        || app.dialog.is_some()
        || app.menu.is_some()
        || app.menu_bar.is_some()
        || app.settings.is_some()
        || app.run_configs.is_some()
        || !app.layout.toolbar_hits.contains(&hover)
    {
        return;
    }
    let (button, rect) = hover;
    let key = app.shortcut(&action(button)).map(|k| format!(" {k} ")).unwrap_or_default();
    let theme = &app.theme;
    let base = Style::default().bg(theme.popup_bg).fg(theme.fg);
    let name = format!(" {} ", title(button));
    let w = (name.width() + key.width()) as u16;
    let y = rect.bottom();
    if y >= area.bottom() {
        return;
    }
    let x = rect.x.min(area.right().saturating_sub(w));
    let room = area.right().saturating_sub(x);
    let buf = frame.buffer_mut();
    buf.set_style(Rect::new(x, y, w.min(room), 1), base);
    let (end, _) = buf.set_stringn(x, y, &name, room as usize, base);
    buf.set_stringn(end, y, &key, (x + room).saturating_sub(end) as usize, base.patch(theme.dim));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(group: Group, text: &str, shrinks: Vec<(u8, Shrink)>) -> Item {
        Item { button: ToolbarButton::Menu, group, text: text.into(), style: Style::default(), shrinks }
    }

    fn xs(placed: &[(Item, Rect)]) -> Vec<(u16, u16)> {
        placed.iter().map(|(_, r)| (r.x, r.width)).collect()
    }

    #[test]
    fn groups_sit_at_their_edges() {
        let items = vec![
            it(Group::Left, "aaa", vec![]),
            it(Group::Left, "bb", vec![]),
            it(Group::Right, "cccc", vec![]),
            it(Group::Right, "dd", vec![]),
        ];
        let placed = place(items, Rect::new(0, 0, 40, 1));
        // Right group: 4 + 1 + 2 = 7 columns, flush right.
        assert_eq!(xs(&placed), vec![(0, 3), (4, 2), (33, 4), (38, 2)]);
    }

    #[test]
    fn steps_stop_once_the_row_fits() {
        let items = || {
            vec![
                it(Group::Left, "menu", vec![]),
                it(Group::Right, "set", vec![(STEP_SETTINGS, Shrink::Drop)]),
                it(Group::Right, "search", vec![(STEP_SEARCH, Shrink::Drop)]),
            ]
        };
        // 4 + 1 + (3 + 1 + 6) = 15 needed.
        assert_eq!(place(items(), Rect::new(0, 0, 15, 1)).len(), 3);
        let placed = place(items(), Rect::new(0, 0, 13, 1));
        let texts: Vec<_> = placed.iter().map(|(i, _)| i.text.as_str()).collect();
        assert_eq!(texts, ["menu", "search"]);
    }

    #[test]
    fn text_shrink_applies_only_when_earlier_steps_are_not_enough() {
        let items = || {
            vec![
                it(Group::Left, "menu", vec![]),
                it(Group::Left, "project", vec![(STEP_PROJECT, Shrink::Text("▾".into()))]),
                it(Group::Right, "set", vec![(STEP_SETTINGS, Shrink::Drop)]),
            ]
        };
        // 4 + 1 + 7 + 1 + 3 = 16 needed; dropping set gives 12.
        let placed = place(items(), Rect::new(0, 0, 12, 1));
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].0.text, "project");
        let placed = place(items(), Rect::new(0, 0, 8, 1));
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].0.text, "▾");
    }

    #[test]
    fn what_still_does_not_fit_is_clipped_and_the_menu_is_kept() {
        let items = vec![
            it(Group::Left, "menu", vec![]),
            it(Group::Left, "branch", vec![]),
            it(Group::Left, "lost", vec![]),
            it(Group::Right, "run", vec![]),
        ];
        // right_x = 14 - 3 = 11; left limit 10.
        let placed = place(items, Rect::new(0, 0, 14, 1));
        assert_eq!(xs(&placed), vec![(0, 4), (5, 5), (11, 3)]);
        let placed = place(vec![it(Group::Left, "menu", vec![]), it(Group::Right, "run-config", vec![])], Rect::new(0, 0, 9, 1));
        assert_eq!(placed[0].1, Rect::new(0, 0, 4, 1));
    }

    #[test]
    fn shorten_cuts_by_width() {
        assert_eq!(shorten("feature/top-bar", 12), "feature/top-…");
        assert_eq!(shorten("main", 12), "main");
        let cut = shorten("日本語日本語日本語", 5);
        assert!(cut.width() <= 6, "{cut}");
        assert_eq!(cut, "日本…");
    }
}
