//! Context menu: a small list of actions drawn at the pointer, over
//! everything but dialogs. It takes keys and mouse like a dialog: Up/Down
//! (skipping disabled rows and separators), Enter, Esc, a click on a row,
//! or a click outside to close. Rows that do not fit scroll, with `▲`/`▼`
//! on the border. A row can open a submenu (Right, Enter, Space or the
//! pointer resting on it; Left or Esc close it). The main menu's dropdowns
//! are the same widget (`ui::menu_bar`).

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::actions::Action;
use crate::config::theme::Theme;
use crate::event::Mouse;

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    /// None for a divider line between groups, never selected or run.
    pub action: Option<Action>,
    pub enabled: bool,
    /// Filled in by the app from the keymap.
    pub shortcut: String,
    /// Rows of the submenu this row opens; empty for a plain row.
    pub children: Vec<MenuItem>,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, action: Action) -> Self {
        Self { label: label.into(), action: Some(action), enabled: true, shortcut: String::new(), children: Vec::new() }
    }

    pub fn separator() -> Self {
        Self { label: String::new(), action: None, enabled: true, shortcut: String::new(), children: Vec::new() }
    }

    /// A row that opens a submenu of `children` (Right, Enter, Space or hover).
    pub fn submenu(label: impl Into<String>, children: Vec<MenuItem>) -> Self {
        let enabled = children.iter().any(MenuItem::selectable);
        Self { label: label.into(), action: None, enabled, shortcut: String::new(), children }
    }

    pub fn is_separator(&self) -> bool {
        self.action.is_none() && self.children.is_empty()
    }

    /// What the right edge shows: `▸` for a parent row, else the shortcut.
    fn trailing(&self) -> &str {
        if self.children.is_empty() { &self.shortcut } else { "▸" }
    }

    /// Whether keys and the pointer can select this row.
    fn selectable(&self) -> bool {
        self.enabled && !self.is_separator()
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuResult {
    Cancel,
    Run(Action),
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    pub items: Vec<MenuItem>,
    selected: usize,
    /// Screen cell the menu opens at (its top-left corner when it fits).
    x: u16,
    y: u16,
    rect: Rect,
    /// First row drawn, when the rows do not fit.
    scroll: usize,
    /// Shift left to fit instead of flipping (a menu bar dropdown).
    shift: bool,
    /// The open submenu, always the selected row's.
    sub: Option<Box<ContextMenu>>,
}

impl ContextMenu {
    pub fn new(items: Vec<MenuItem>, x: u16, y: u16) -> Self {
        let selected = items.iter().position(MenuItem::selectable).unwrap_or(0);
        Self { items, selected, x, y, rect: Rect::default(), scroll: 0, shift: false, sub: None }
    }

    fn step(&mut self, forward: bool) {
        let n = self.items.len();
        let mut i = self.selected;
        for _ in 0..n {
            i = if forward { (i + 1) % n } else { (i + n - 1) % n };
            if self.items[i].selectable() {
                self.selected = i;
                return;
            }
        }
    }

    /// Open the selected row's submenu. Returns whether that row is a parent
    /// (so the caller does not run it).
    fn open_sub(&mut self) -> bool {
        let Some(item) = self.items.get(self.selected).filter(|it| it.selectable() && !it.children.is_empty()) else {
            return false;
        };
        if self.sub.is_none() {
            self.sub = Some(Box::new(ContextMenu::new(item.children.clone(), 0, 0)));
        }
        true
    }

    fn run(&self, i: usize) -> Option<MenuResult> {
        let item = self.items.get(i).filter(|it| it.selectable())?;
        item.action.clone().map(MenuResult::Run)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<MenuResult> {
        if let Some(sub) = &mut self.sub {
            if sub.sub.is_none() && matches!(key.code, KeyCode::Left | KeyCode::Esc) {
                self.sub = None;
                return None;
            }
            return sub.handle_key(key);
        }
        match key.code {
            KeyCode::Esc => Some(MenuResult::Cancel),
            KeyCode::Up | KeyCode::BackTab => {
                self.step(false);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.step(true);
                None
            }
            KeyCode::Right => {
                self.open_sub();
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.open_sub() {
                    None
                } else {
                    self.run(self.selected)
                }
            }
            _ => None,
        }
    }

    /// Move the point the menu opens at (the main menu anchors its
    /// dropdown under the open title, known once the bar is drawn).
    pub fn move_to(&mut self, x: u16, y: u16) {
        (self.x, self.y) = (x, y);
    }

    /// Take over another menu's screen rect until the next render, so a
    /// click right after the rows change still lands inside.
    /// When the menu does not fit right of its anchor, move it left just
    /// enough rather than opening left of the anchor.
    pub fn shifted(mut self) -> Self {
        self.shift = true;
        self
    }

    pub fn set_rect(&mut self, rect: Rect) {
        self.rect = rect;
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The menu's rect, then each open submenu's, outermost first.
    pub fn rects(&self) -> Vec<Rect> {
        let mut rects = vec![self.rect];
        if let Some(sub) = &self.sub {
            rects.extend(sub.rects());
        }
        rects
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains((x, y).into()) || self.sub.as_ref().is_some_and(|s| s.contains(x, y))
    }

    fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        let inner = Rect::new(self.rect.x + 1, self.rect.y + 1, self.rect.width.saturating_sub(2), self.rect.height.saturating_sub(2));
        inner.contains((x, y).into()).then(|| self.scroll + (y - inner.y) as usize).filter(|&i| i < self.items.len())
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<MenuResult> {
        if let Some(sub) = &mut self.sub
            && sub.contains(m.x, m.y)
        {
            return sub.handle_mouse(m);
        }
        match m.kind {
            MouseEventKind::Down(_) if !self.rect.contains((m.x, m.y).into()) => Some(MenuResult::Cancel),
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Right) => {
                // Up(Right): press, drag to an item, release (as in GUIs).
                let i = self.row_at(m.x, m.y)?;
                if self.items[i].selectable() && !self.items[i].children.is_empty() {
                    if i != self.selected {
                        self.sub = None;
                    }
                    self.selected = i;
                    self.open_sub();
                    return None;
                }
                self.run(i)
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if let Some(i) = self.row_at(m.x, m.y) {
                    if i != self.selected {
                        self.sub = None;
                    }
                    if self.items[i].selectable() {
                        self.selected = i;
                        self.open_sub();
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn size(&self, area: Rect) -> (u16, u16) {
        let label_w = self.items.iter().map(|i| i.label.width()).max().unwrap_or(0) as u16;
        let key_w = self.items.iter().map(|i| i.trailing().width()).max().unwrap_or(0) as u16;
        let w = (label_w + if key_w > 0 { key_w + 3 } else { 0 } + 4).min(area.width);
        let h = (self.items.len() as u16 + 2).min(area.height);
        (w, h)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let (w, h) = self.size(area);
        // Open down-right of the pointer, flipping when it would not fit
        // (shifting left when the flip would not fit either).
        let shifted = area.right().saturating_sub(w).max(area.x);
        let x = if self.x + w <= area.right() {
            self.x
        } else if !self.shift && self.x >= area.x + w {
            self.x - w
        } else {
            shifted
        };
        let y = if self.y + 1 + h <= area.bottom() { self.y + 1 } else { self.y.saturating_sub(h).max(area.y) };
        self.rect = Rect::new(x, y, w, h);
        self.draw(frame, area, theme);
    }

    /// Draw the menu at `self.rect`, then its open submenu beside the selected row.
    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        frame.render_widget(Clear, self.rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.popup_border)
            .style(Style::default().bg(theme.popup_bg).fg(theme.fg));
        let inner = block.inner(self.rect);
        frame.render_widget(block, self.rect);
        // Keep the selected row on screen.
        let rows = inner.height as usize;
        let max_scroll = self.items.len().saturating_sub(rows);
        if rows > 0 {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + rows {
                self.scroll = self.selected + 1 - rows;
            }
        }
        self.scroll = self.scroll.min(max_scroll);
        let buf = frame.buffer_mut();
        let mid = self.rect.x + self.rect.width / 2;
        if self.scroll > 0 {
            buf.set_string(mid, self.rect.y, "▲", theme.popup_border.bg(theme.popup_bg));
        }
        if self.scroll < max_scroll {
            buf.set_string(mid, self.rect.bottom() - 1, "▼", theme.popup_border.bg(theme.popup_bg));
        }
        for (i, item) in self.items.iter().enumerate().skip(self.scroll).take(rows) {
            let y = inner.y + (i - self.scroll) as u16;
            if item.is_separator() {
                // Joined to the border on both sides.
                let line = format!("├{}┤", "─".repeat(inner.width as usize));
                buf.set_string(self.rect.x, y, line, theme.popup_border.bg(theme.popup_bg));
                continue;
            }
            let base = if !item.enabled {
                theme.dim.bg(theme.popup_bg)
            } else if i == self.selected {
                theme.list_selected
            } else {
                Style::default().fg(theme.fg).bg(theme.popup_bg)
            };
            buf.set_style(Rect::new(inner.x, y, inner.width, 1), base);
            buf.set_stringn(inner.x + 1, y, &item.label, inner.width.saturating_sub(2) as usize, base);
            let trailing = item.trailing();
            if !trailing.is_empty() {
                let kx = inner.right().saturating_sub(trailing.width() as u16 + 1);
                buf.set_stringn(kx, y, trailing, (inner.right() - kx) as usize, base.patch(theme.dim));
            }
        }
        let (parent, selected, scroll) = (self.rect, self.selected, self.scroll);
        if let Some(sub) = &mut self.sub {
            let row_y = inner.y + (selected - scroll) as u16;
            let (w, h) = sub.size(area);
            let x = if parent.right() + w <= area.right() {
                parent.right()
            } else if parent.x >= area.x + w {
                parent.x - w
            } else {
                area.right().saturating_sub(w).max(area.x)
            };
            let y = row_y.saturating_sub(1).min(area.bottom().saturating_sub(h)).max(area.y);
            sub.rect = Rect::new(x, y, w, h);
            sub.draw(frame, area, theme);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{AppAction, TreeAction};
    use crossterm::event::{KeyModifiers, MouseEventKind};

    fn menu() -> ContextMenu {
        ContextMenu::new(
            vec![
                MenuItem::new("Off", Action::Tree(TreeAction::Refresh)).enabled(false),
                MenuItem::new("New", Action::Tree(TreeAction::New)),
                MenuItem::new("Quit", Action::App(AppAction::Quit)),
            ],
            10,
            5,
        )
    }

    #[test]
    fn keys_skip_disabled_rows() {
        let mut m = menu();
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(m.selected, 1, "starts on the first enabled row");
        m.handle_key(key(KeyCode::Down));
        m.handle_key(key(KeyCode::Down));
        assert_eq!(m.selected, 1, "wraps past the disabled row");
        assert_eq!(m.handle_key(key(KeyCode::Enter)), Some(MenuResult::Run(Action::Tree(TreeAction::New))));
        assert_eq!(m.handle_key(key(KeyCode::Esc)), Some(MenuResult::Cancel));
    }

    #[test]
    fn separators_are_skipped_and_inert() {
        let quit = || Action::App(AppAction::Quit);
        let mut m = ContextMenu::new(
            vec![
                MenuItem::separator(),
                MenuItem::new("New", Action::Tree(TreeAction::New)),
                MenuItem::separator(),
                MenuItem::new("Quit", quit()),
            ],
            10,
            5,
        );
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(m.selected, 1, "starts past a leading separator");
        m.handle_key(key(KeyCode::Down));
        assert_eq!(m.selected, 3);
        m.handle_key(key(KeyCode::Tab));
        assert_eq!(m.selected, 1, "wraps past the separator at the top");
        m.handle_key(key(KeyCode::BackTab));
        assert_eq!(m.selected, 3);
        m.rect = Rect::new(10, 6, 12, 6);
        let at = |kind, x, y| Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE };
        assert_eq!(m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 12, 9)), None, "click on a separator");
        m.handle_mouse(&at(MouseEventKind::Moved, 12, 9));
        assert_eq!(m.selected, 3, "hover on a separator keeps the selection");
        assert_eq!(m.handle_key(key(KeyCode::Enter)), Some(MenuResult::Run(quit())));
    }

    #[test]
    fn separator_renders_as_a_line() {
        let mut m = ContextMenu::new(
            vec![
                MenuItem::new("New", Action::Tree(TreeAction::New)),
                MenuItem::separator(),
                MenuItem::new("Quit", Action::App(AppAction::Quit)),
            ],
            0,
            0,
        );
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 8)).unwrap();
        term.draw(|f| m.render(f, f.area(), &Theme::default())).unwrap();
        let buf = term.backend().buffer();
        let row: String = (m.rect.x..m.rect.right()).map(|x| buf[(x, m.rect.y + 2)].symbol()).collect();
        assert_eq!(row, format!("├{}┤", "─".repeat(m.rect.width as usize - 2)));
    }

    #[test]
    fn long_menus_scroll_to_the_selection() {
        let items = (0..30).map(|i| MenuItem::new(format!("Row {i}"), Action::App(AppAction::About))).collect();
        let mut m = ContextMenu::new(items, 0, 0);
        m.items[29].action = Some(Action::App(AppAction::Quit));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 24)).unwrap();
        let mut draw = |m: &mut ContextMenu| term.draw(|f| m.render(f, Rect::new(0, 1, 30, 23), &Theme::default())).unwrap().buffer.clone();
        let buf = draw(&mut m);
        let rows = m.rect.height as usize - 2;
        assert_eq!(rows, 21, "23 rows below the anchor, two for the border");
        let mid = m.rect.x + m.rect.width / 2;
        assert_eq!(buf[(mid, m.rect.bottom() - 1)].symbol(), "▼");
        assert_ne!(buf[(mid, m.rect.y)].symbol(), "▲");
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        for _ in 0..29 {
            m.handle_key(key(KeyCode::Down));
            let buf = draw(&mut m);
            let y = m.rect.y + 1 + (m.selected - m.scroll) as u16;
            let line: String = (m.rect.x + 2..m.rect.x + 8).map(|x| buf[(x, y)].symbol()).collect();
            assert_eq!(line.trim_end(), format!("Row {}", m.selected), "the selected row is drawn");
        }
        let buf = draw(&mut m);
        assert_eq!(buf[(mid, m.rect.y)].symbol(), "▲");
        assert_ne!(buf[(mid, m.rect.bottom() - 1)].symbol(), "▼");
        assert_eq!(m.handle_key(key(KeyCode::Enter)), Some(MenuResult::Run(Action::App(AppAction::Quit))));
        let at = |kind, x, y| Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE };
        let last = m.rect.bottom() - 2;
        assert_eq!(
            m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), m.rect.x + 2, last)),
            Some(MenuResult::Run(Action::App(AppAction::Quit))),
            "a click maps through the scroll"
        );
        m.handle_mouse(&at(MouseEventKind::Moved, m.rect.x + 2, m.rect.y + 1));
        assert_eq!(m.selected, 30 - rows);
    }

    #[test]
    fn mouse_runs_rows_and_closes_outside() {
        let mut m = menu();
        m.rect = Rect::new(10, 6, 12, 5);
        let at = |kind, x, y| Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE };
        assert_eq!(m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 12, 7)), None, "disabled row");
        assert_eq!(
            m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 12, 9)),
            Some(MenuResult::Run(Action::App(AppAction::Quit)))
        );
        assert_eq!(m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 0, 0)), Some(MenuResult::Cancel));
    }

    fn nested() -> ContextMenu {
        ContextMenu::new(
            vec![
                MenuItem::new("New", Action::Tree(TreeAction::New)),
                MenuItem::submenu(
                    "More",
                    vec![
                        MenuItem::new("Off", Action::Tree(TreeAction::Refresh)).enabled(false),
                        MenuItem::new("About", Action::App(AppAction::About)),
                        MenuItem::new("Quit", Action::App(AppAction::Quit)),
                    ],
                ),
            ],
            2,
            2,
        )
    }

    fn draw(m: &mut ContextMenu) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        term.draw(|f| m.render(f, f.area(), &Theme::default())).unwrap().buffer.clone()
    }

    fn at(kind: MouseEventKind, x: u16, y: u16) -> Mouse {
        Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE }
    }

    #[test]
    fn submenu_keys_open_and_close_only_the_child() {
        let mut m = nested();
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        m.handle_key(key(KeyCode::Down));
        assert_eq!(m.selected, 1);
        assert_eq!(m.handle_key(key(KeyCode::Right)), None);
        assert_eq!(m.sub.as_ref().map(|s| s.selected), Some(1), "first enabled child");
        m.handle_key(key(KeyCode::Down));
        assert_eq!(m.sub.as_ref().unwrap().selected, 2);
        assert_eq!(m.handle_key(key(KeyCode::Left)), None);
        assert!(m.sub.is_none());
        assert_eq!(m.selected, 1);
        assert_eq!(m.handle_key(key(KeyCode::Enter)), None);
        assert!(m.sub.is_some());
        assert_eq!(m.handle_key(key(KeyCode::Esc)), None);
        assert!(m.sub.is_none());
        assert_eq!(m.handle_key(key(KeyCode::Char(' '))), None);
        assert!(m.sub.is_some());
        assert_eq!(m.handle_key(key(KeyCode::Enter)), Some(MenuResult::Run(Action::App(AppAction::About))));
        m.handle_key(key(KeyCode::Esc));
        assert_eq!(m.handle_key(key(KeyCode::Esc)), Some(MenuResult::Cancel));
    }

    #[test]
    fn submenu_parent_rules() {
        let off = || MenuItem::new("Off", Action::Tree(TreeAction::Refresh)).enabled(false);
        assert!(!MenuItem::submenu("x", vec![]).enabled);
        assert!(!MenuItem::submenu("x", vec![off()]).enabled);
        assert!(!MenuItem::submenu("x", vec![off()]).is_separator());
        let mut m = ContextMenu::new(vec![MenuItem::new("New", Action::Tree(TreeAction::New)), MenuItem::submenu("x", vec![off()])], 0, 0);
        m.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        m.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(m.sub.is_none());
    }

    #[test]
    fn hover_opens_and_closes_the_submenu() {
        let mut m = nested();
        draw(&mut m);
        let row = m.rect.y + 2;
        m.handle_mouse(&at(MouseEventKind::Moved, m.rect.x + 2, row));
        assert!(m.sub.is_some());
        draw(&mut m);
        let sub = m.sub.as_ref().unwrap().rect;
        m.handle_mouse(&at(MouseEventKind::Moved, sub.x + 2, sub.y + 3));
        assert_eq!(m.sub.as_ref().unwrap().selected, 2);
        assert_eq!(
            m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), sub.x + 2, sub.y + 3)),
            Some(MenuResult::Run(Action::App(AppAction::Quit)))
        );
        m.handle_mouse(&at(MouseEventKind::Moved, m.rect.x + 2, m.rect.y + 1));
        assert!(m.sub.is_none());
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn click_outside_menu_and_submenu_cancels() {
        let mut m = nested();
        m.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        m.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        draw(&mut m);
        let sub = m.sub.as_ref().unwrap().rect;
        assert!(m.contains(sub.x + 1, sub.y + 1));
        assert!(!m.contains(59, 19));
        assert_eq!(m.rects().len(), 2);
        assert_eq!(m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), sub.x + 2, sub.y + 1)), None, "disabled child");
        assert!(m.sub.is_some());
        assert_eq!(m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 59, 19)), Some(MenuResult::Cancel));
    }

    #[test]
    fn submenu_opens_beside_the_parent_row() {
        let mut m = nested();
        m.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        m.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        draw(&mut m);
        let sub = m.sub.as_ref().unwrap().rect;
        assert_eq!(sub.x, m.rect.right());
        assert_eq!(sub.y + 1, m.rect.y + 2);
        m.move_to(40, 2);
        draw(&mut m);
        let sub = m.sub.as_ref().unwrap().rect;
        assert_eq!(sub.right(), m.rect.x, "flipped to the left");
    }

    #[test]
    fn parent_row_draws_an_arrow() {
        let mut m = nested();
        let buf = draw(&mut m);
        assert_eq!(buf[(m.rect.right() - 3, m.rect.y + 2)].symbol(), "▸");
    }
}
