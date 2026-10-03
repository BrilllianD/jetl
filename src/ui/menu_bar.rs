//! Main menu, JetBrains-style: a menu bar drawn over the top screen row with
//! the open menu's dropdown (a `ContextMenu`) under its title. Hidden until
//! `app.main_menu` (F10). Left/Right and mnemonic letters switch menus, the
//! dropdown takes Up/Down/Enter, Esc or a click outside closes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Clear;
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, EditorAction, GitAction, TreeAction};
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::menu::{ContextMenu, MenuItem, MenuResult};

/// A row of the menu table: a label and a registry action id, or a divider.
pub enum Row {
    Item(&'static str, &'static str),
    Sep,
}

use Row::{Item, Sep};

/// The main menu: title, mnemonic and rows of each menu.
pub const MAIN_MENU: &[(&str, char, &[Row])] = &[
    (
        "File",
        'F',
        &[
            Item("Go to File…", "palette.files"),
            Item("Recent Files…", "palette.recent_files"),
            Item("Recent Projects…", "palette.recent_projects"),
            Sep,
            Item("Save", "editor.save"),
            Item("Save All", "editor.save_all"),
            Sep,
            Item("Close Tab", "editor.close_tab"),
            Item("Close Other Tabs", "editor.close_other_tabs"),
            Item("Close All Tabs", "editor.close_all_tabs"),
            Sep,
            Item("Exit", "app.quit"),
        ],
    ),
    (
        "Edit",
        'E',
        &[
            Item("Undo", "editor.undo"),
            Item("Redo", "editor.redo"),
            Sep,
            Item("Cut", "editor.cut"),
            Item("Copy", "editor.copy"),
            Item("Paste", "editor.paste"),
            Item("Select All", "editor.select_all"),
            Sep,
            Item("Find…", "editor.find"),
            Item("Replace…", "editor.replace"),
            Item("Find in Files…", "find.in_files"),
            Item("Replace in Files…", "find.replace_in_files"),
            Sep,
            Item("Duplicate Line", "editor.duplicate_line"),
            Item("Delete Line", "editor.delete_line"),
            Item("Join Lines", "editor.join_lines"),
            Item("Move Line Up", "editor.move_line_up"),
            Item("Move Line Down", "editor.move_line_down"),
            Sep,
            Item("Comment with Line Comment", "editor.toggle_comment"),
            Item("Comment with Block Comment", "editor.toggle_block_comment"),
            Item("Reformat Code", "editor.format"),
        ],
    ),
    (
        "View",
        'V',
        &[
            Item("Project", "tool_window.project"),
            Item("Worktrees", "tool_window.worktrees"),
            Item("Structure", "tool_window.structure"),
            Item("Find", "tool_window.find"),
            Item("Run", "tool_window.run"),
            Item("Terminal", "tool_window.terminal"),
            Item("Git", "tool_window.git"),
            Item("Event Log", "tool_window.event_log"),
            Sep,
            Item("Hide All Tool Windows", "tool_window.hide_all"),
            Item("Zen Mode", "app.zen"),
            Sep,
            Item("Git Blame", "editor.toggle_blame"),
        ],
    ),
    (
        "Navigate",
        'N',
        &[
            Item("Search Everywhere…", "palette.search_everywhere"),
            Item("File…", "palette.files"),
            Item("Symbol…", "palette.symbols"),
            Item("Line…", "editor.goto_line"),
            Item("Recent Files…", "palette.recent_files"),
            Sep,
            Item("Back", "editor.nav_back"),
            Item("Forward", "editor.nav_forward"),
            Sep,
            Item("Toggle Bookmark", "editor.toggle_bookmark"),
            Item("Bookmarks…", "editor.show_bookmarks"),
            Sep,
            Item("Next Change", "editor.next_change"),
            Item("Previous Change", "editor.prev_change"),
            Item("Matching Bracket", "editor.goto_bracket"),
            Item("Select in Project", "tree.select_in_project"),
        ],
    ),
    (
        "Git",
        'G',
        &[
            Item("Commit…", "git.commit_box"),
            Item("Commit and Push", "git.commit_and_push"),
            Item("Push", "git.push"),
            Item("Pull", "git.pull"),
            Item("Fetch", "git.fetch"),
            Sep,
            Item("Branches…", "git.branches_popup"),
            Item("New Branch…", "git.new_branch"),
            Sep,
            Item("Stash Changes…", "git.stash_push"),
            Sep,
            Item("File History", "git.file_history"),
            Item("Revert Change", "editor.revert_change"),
        ],
    ),
    (
        "Run",
        'R',
        &[
            Item("Run…", "run.pick"),
            Item("Rerun", "run.rerun"),
            Item("Stop", "run.stop"),
            Sep,
            Item("Next Problem", "run.next_problem"),
            Item("Previous Problem", "run.prev_problem"),
        ],
    ),
    ("Help", 'H', &[Item("Find Action…", "palette.actions"), Sep, Item("About jetl", "app.about")]),
];

/// Whether a menu row needs an active editor tab: editor actions (but
/// Back/Forward, which reopen closed files), file history and Select in
/// Project. Such rows are disabled while no tab is open.
pub fn needs_buffer(action: &Action) -> bool {
    match action {
        Action::Editor(a) => !matches!(a, EditorAction::NavBack | EditorAction::NavForward),
        Action::Git(GitAction::FileHistory) | Action::Tree(TreeAction::SelectInProject) => true,
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub title: String,
    /// Letter that opens this menu while the bar is up (underlined).
    pub mnemonic: char,
    pub items: Vec<MenuItem>,
}

/// The main menu's symbol, also the tool strip's menu button. One cell wide
/// everywhere: `≡` is East Asian ambiguous and `☰` is wide.
pub const MENU_GLYPH: &str = "≣";

/// Leading cells of the bar: the menu symbol padded.
const ICON: &str = " ≣ ";

#[derive(Debug, Clone)]
pub struct MenuBar {
    menus: Vec<Menu>,
    open: usize,
    dropdown: ContextMenu,
    /// The bar row and each title's cell from the last frame, for the mouse.
    bar: Rect,
    titles: Vec<Rect>,
}

impl MenuBar {
    /// A bar with the first menu open. `menus` must not be empty.
    pub fn new(menus: Vec<Menu>) -> Self {
        let dropdown = ContextMenu::new(menus[0].items.clone(), 0, 0);
        Self { menus, open: 0, dropdown, bar: Rect::default(), titles: Vec::new() }
    }

    pub fn open_title(&self) -> &str {
        &self.menus[self.open].title
    }

    pub fn dropdown(&self) -> &ContextMenu {
        &self.dropdown
    }

    fn open(&mut self, i: usize) {
        if i != self.open {
            self.open = i;
            // Anchored under its title by the next render.
            self.dropdown = ContextMenu::new(self.menus[i].items.clone(), 0, 0);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<MenuResult> {
        let n = self.menus.len();
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if let KeyCode::Char(c) = key.code
            && plain
            && let Some(i) = self.menus.iter().position(|m| m.mnemonic.eq_ignore_ascii_case(&c))
        {
            self.open(i);
            return None;
        }
        match key.code {
            KeyCode::Esc => Some(MenuResult::Cancel),
            KeyCode::F(10) if key.modifiers.is_empty() => Some(MenuResult::Cancel),
            KeyCode::Left => {
                self.open((self.open + n - 1) % n);
                None
            }
            KeyCode::Right => {
                self.open((self.open + 1) % n);
                None
            }
            _ => self.dropdown.handle_key(key),
        }
    }

    /// Whether a point is on the bar or the open dropdown.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.bar.contains((x, y).into()) || self.dropdown.contains(x, y)
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<MenuResult> {
        if !self.bar.contains((m.x, m.y).into()) {
            // The dropdown runs its rows, follows the pointer and closes on
            // a press outside it.
            return self.dropdown.handle_mouse(m);
        }
        let title = self.titles.iter().position(|r| r.contains((m.x, m.y).into()));
        match (m.kind, title) {
            // A press on the open title or the icon closes, as the button would.
            (MouseEventKind::Down(_), Some(i)) if i == self.open => Some(MenuResult::Cancel),
            (MouseEventKind::Down(_), None) if m.x < self.bar.x + ICON.width() as u16 => Some(MenuResult::Cancel),
            (MouseEventKind::Down(_) | MouseEventKind::Moved | MouseEventKind::Drag(_), Some(i)) => {
                self.open(i);
                None
            }
            _ => None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area.height < 2 {
            return;
        }
        self.bar = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(Clear, self.bar);
        let style = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let buf = frame.buffer_mut();
        buf.set_style(self.bar, style);
        buf.set_stringn(self.bar.x, self.bar.y, ICON, self.bar.width as usize, theme.popup_border.bg(theme.popup_bg));
        self.titles.clear();
        let mut x = self.bar.x + ICON.width() as u16;
        for (i, menu) in self.menus.iter().enumerate() {
            let w = (menu.title.width() as u16 + 2).min(self.bar.right().saturating_sub(x));
            let cell = Rect::new(x, self.bar.y, w, 1);
            let s = if i == self.open { theme.list_selected } else { style };
            buf.set_style(cell, s);
            buf.set_stringn(x + 1, cell.y, &menu.title, w.saturating_sub(1) as usize, s);
            if let Some((at, _)) = menu.title.char_indices().find(|(_, c)| c.eq_ignore_ascii_case(&menu.mnemonic)) {
                let mx = x + 1 + menu.title[..at].width() as u16;
                if mx < cell.right() {
                    buf[(mx, cell.y)].set_style(Style::default().add_modifier(Modifier::UNDERLINED));
                }
            }
            self.titles.push(cell);
            x += w;
        }
        let anchor = self.titles.get(self.open).map_or(self.bar.x, |r| r.x);
        self.dropdown.move_to(anchor, self.bar.y);
        let below = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        self.dropdown.render(frame, below, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::AppAction;
    use crossterm::event::MouseButton;

    fn bar() -> MenuBar {
        let a = |id| Action::from_id(id).unwrap();
        MenuBar::new(vec![
            Menu { title: "File".into(), mnemonic: 'F', items: vec![MenuItem::new("Exit", a("app.quit"))] },
            Menu {
                title: "Edit".into(),
                mnemonic: 'E',
                items: vec![MenuItem::new("Undo", a("editor.undo")), MenuItem::separator(), MenuItem::new("Copy", a("editor.copy"))],
            },
            Menu { title: "Help".into(), mnemonic: 'H', items: vec![MenuItem::new("About jetl", a("app.about"))] },
        ])
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn draw(b: &mut MenuBar) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();
        term.draw(|f| b.render(f, f.area(), &Theme::default())).unwrap();
    }

    fn at(kind: MouseEventKind, x: u16, y: u16) -> Mouse {
        Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE }
    }

    #[test]
    fn icon_is_one_cell() {
        assert_eq!((ICON.width(), MENU_GLYPH.width_cjk()), (3, 1));
        assert_eq!(ICON.trim(), MENU_GLYPH);
    }

    #[test]
    fn left_right_wrap() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Left));
        assert_eq!(b.open_title(), "Help");
        b.handle_key(key(KeyCode::Right));
        assert_eq!(b.open_title(), "File");
        b.handle_key(key(KeyCode::Right));
        assert_eq!(b.open_title(), "Edit");
        assert_eq!(b.handle_key(key(KeyCode::Esc)), Some(MenuResult::Cancel));
        assert_eq!(b.handle_key(key(KeyCode::F(10))), Some(MenuResult::Cancel));
    }

    #[test]
    fn mnemonics_jump_case_insensitively() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Char('h')));
        assert_eq!(b.open_title(), "Help");
        b.handle_key(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT));
        assert_eq!(b.open_title(), "Edit");
        b.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(b.open_title(), "Edit", "not with Ctrl");
        b.handle_key(key(KeyCode::Char('x')));
        assert_eq!(b.open_title(), "Edit");
    }

    #[test]
    fn dropdown_skips_separators_and_enter_runs() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Char('e')));
        b.handle_key(key(KeyCode::Down));
        assert_eq!(b.handle_key(key(KeyCode::Enter)), Some(MenuResult::Run(Action::from_id("editor.copy").unwrap())));
        b.handle_key(key(KeyCode::Right));
        assert_eq!(b.handle_key(key(KeyCode::Char(' '))), Some(MenuResult::Run(Action::App(AppAction::About))));
    }

    #[test]
    fn mouse_switches_titles_and_cancels_outside() {
        let mut b = bar();
        draw(&mut b);
        // " ≣  File  Edit  Help ": Edit's cell starts at 9.
        assert_eq!(b.titles[1].x, 9);
        assert!(b.dropdown.contains(3, 2), "File's dropdown is under its title");
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 10, 0)), None);
        assert_eq!(b.open_title(), "Edit");
        draw(&mut b);
        assert!(b.dropdown.contains(10, 2) && !b.dropdown.contains(3, 2), "the dropdown follows");
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Moved, 16, 0)), None);
        assert_eq!(b.open_title(), "Help", "hover switches while open");
        draw(&mut b);
        assert_eq!(
            b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 17, 2)),
            Some(MenuResult::Run(Action::App(AppAction::About)))
        );
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 40, 8)), Some(MenuResult::Cancel));
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 30, 0)), None, "empty bar space");
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 16, 0)), Some(MenuResult::Cancel), "open title");
    }

    #[test]
    fn table_ids_resolve_and_menus_are_not_empty() {
        for (title, mnemonic, rows) in MAIN_MENU {
            assert!(rows.iter().any(|r| matches!(r, Item(..))), "{title} is empty");
            assert!(title.contains(*mnemonic), "{title} has no '{mnemonic}'");
            for row in *rows {
                if let Item(label, id) = row {
                    assert!(Action::from_id(id).is_some(), "{title} > {label}: unknown action {id}");
                }
            }
        }
        let mnemonics: std::collections::HashSet<_> = MAIN_MENU.iter().map(|m| m.1.to_ascii_lowercase()).collect();
        assert_eq!(mnemonics.len(), MAIN_MENU.len(), "mnemonics are unique");
    }
}
