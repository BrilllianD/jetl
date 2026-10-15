//! The main menu bar (`app.main_menu`, F10): building it from
//! `ui::menu_bar::MAIN_MENU`, its keys and mouse, running the chosen row.

use crossterm::event::{KeyEvent, MouseEvent};

use super::App;
use crate::actions::{Action, AppAction};
use crate::config::keymap::KeyCombo;
use crate::event::Mouse;
use crate::ui::menu::{MenuItem, MenuResult};
use crate::ui::menu_bar::{MAIN_MENU, Menu, MenuBar, Row, needs_buffer};

impl App {
    /// Open the menu bar with File open, or close it when open. The tool
    /// strip's menu button runs the same action.
    pub(crate) fn toggle_main_menu(&mut self) {
        if self.menu_bar.take().is_none() {
            self.menu = None;
            self.menu_bar = Some(self.build_main_menu());
        }
    }

    /// The menus with shortcuts from the keymap; rows that act on the
    /// active tab are disabled while none is open.
    fn build_main_menu(&self) -> MenuBar {
        let has_buffer = self.editor.active_buffer().is_some();
        let menus = MAIN_MENU
            .iter()
            .map(|&(title, mnemonic, rows)| {
                let mut items: Vec<MenuItem> = rows
                    .iter()
                    .filter_map(|row| match row {
                        Row::Sep => Some(MenuItem::separator()),
                        Row::Item(label, id) => {
                            let action = Action::from_id(id)?;
                            let enabled = (has_buffer || !needs_buffer(&action)) && self.git_action_available(&action);
                            Some(MenuItem::new(*label, action).enabled(enabled))
                        }
                    })
                    .collect();
                self.fill_shortcuts(&mut items);
                Menu { title: title.to_string(), mnemonic, items }
            })
            .collect();
        MenuBar::new(menus, self.config.ui.icons)
    }

    pub(super) fn main_menu_key(&mut self, key: KeyEvent) {
        // The key bound to `app.main_menu` closes the bar, like F10.
        let combo = KeyCombo::normalize(key.code, key.modifiers);
        let toggles = self.keymap.table("global").and_then(|t| t.bindings.get(&vec![combo])) == Some(&Action::App(AppAction::MainMenu));
        let res = if toggles { Some(MenuResult::Cancel) } else { self.menu_bar.as_mut().and_then(|b| b.handle_key(key)) };
        self.finish_main_menu(res);
    }

    pub(super) fn main_menu_mouse(&mut self, m: &MouseEvent, mouse: &Mouse) {
        if matches!(m.kind, crossterm::event::MouseEventKind::Up(_)) {
            self.drag = None;
        }
        let res = self.menu_bar.as_mut().and_then(|b| b.handle_mouse(mouse));
        self.finish_main_menu(res);
    }

    fn finish_main_menu(&mut self, res: Option<MenuResult>) {
        match res {
            Some(MenuResult::Run(a)) => {
                self.menu_bar = None;
                self.perform(a);
            }
            Some(MenuResult::Cancel) => self.menu_bar = None,
            None => {}
        }
    }
}
