//! Context menu: a small list of actions drawn at the pointer, over
//! everything but dialogs. It takes keys and mouse like a dialog: Up/Down
//! (skipping disabled rows and separators), Enter, Esc, a click on a row,
//! or a click outside to close. The main menu's dropdowns are the same
//! widget (`ui::menu_bar`).

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
    pub action: Action,
    pub enabled: bool,
    /// Filled in by the app from the keymap.
    pub shortcut: String,
    /// A divider line between groups; never selected or run.
    pub separator: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, action: Action) -> Self {
        Self { label: label.into(), action, enabled: true, shortcut: String::new(), separator: false }
    }

    pub fn separator() -> Self {
        Self { separator: true, ..Self::new("", Action::App(crate::actions::AppAction::Escape)) }
    }

    /// Whether keys and the pointer can select this row.
    fn selectable(&self) -> bool {
        self.enabled && !self.separator
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
}

impl ContextMenu {
    pub fn new(items: Vec<MenuItem>, x: u16, y: u16) -> Self {
        let selected = items.iter().position(MenuItem::selectable).unwrap_or(0);
        Self { items, selected, x, y, rect: Rect::default() }
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

    fn run(&self, i: usize) -> Option<MenuResult> {
        let item = self.items.get(i).filter(|it| it.selectable())?;
        Some(MenuResult::Run(item.action.clone()))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<MenuResult> {
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
            KeyCode::Enter | KeyCode::Char(' ') => self.run(self.selected),
            _ => None,
        }
    }

    /// Move the point the menu opens at (the main menu anchors its
    /// dropdown under the open title, known once the bar is drawn).
    pub fn move_to(&mut self, x: u16, y: u16) {
        (self.x, self.y) = (x, y);
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains((x, y).into())
    }

    fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        let inner = Rect::new(self.rect.x + 1, self.rect.y + 1, self.rect.width.saturating_sub(2), self.rect.height.saturating_sub(2));
        inner.contains((x, y).into()).then(|| (y - inner.y) as usize)
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<MenuResult> {
        match m.kind {
            MouseEventKind::Down(_) if !self.contains(m.x, m.y) => Some(MenuResult::Cancel),
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Right) => {
                // Up(Right): press, drag to an item, release (as in GUIs).
                self.row_at(m.x, m.y).and_then(|i| self.run(i))
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if let Some(i) = self.row_at(m.x, m.y).filter(|&i| self.items[i].selectable()) {
                    self.selected = i;
                }
                None
            }
            _ => None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let label_w = self.items.iter().map(|i| i.label.width()).max().unwrap_or(0) as u16;
        let key_w = self.items.iter().map(|i| i.shortcut.width()).max().unwrap_or(0) as u16;
        let w = (label_w + if key_w > 0 { key_w + 3 } else { 0 } + 4).min(area.width);
        let h = (self.items.len() as u16 + 2).min(area.height);
        // Open down-right of the pointer, flipping when it would not fit.
        let x = if self.x + w <= area.right() { self.x } else { self.x.saturating_sub(w).max(area.x) };
        let y = if self.y + 1 + h <= area.bottom() { self.y + 1 } else { self.y.saturating_sub(h).max(area.y) };
        self.rect = Rect::new(x, y, w, h);
        frame.render_widget(Clear, self.rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.popup_border)
            .style(Style::default().bg(theme.popup_bg).fg(theme.fg));
        let inner = block.inner(self.rect);
        frame.render_widget(block, self.rect);
        let buf = frame.buffer_mut();
        for (i, item) in self.items.iter().enumerate().take(inner.height as usize) {
            let y = inner.y + i as u16;
            if item.separator {
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
            if !item.shortcut.is_empty() {
                let kx = inner.right().saturating_sub(item.shortcut.width() as u16 + 1);
                buf.set_stringn(kx, y, &item.shortcut, (inner.right() - kx) as usize, base.patch(theme.dim));
            }
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
}
