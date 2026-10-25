//! The Switcher popup (`app.switcher`, Ctrl+Tab): building its rows from the
//! editor's recent tabs and the tool windows, its keys and mouse, and
//! opening the chosen row.

use crossterm::event::KeyEvent;

use super::{App, Ctx, PanelId};
use crate::actions::{Action, AppAction, EditorAction};
use crate::event::Mouse;
use crate::ui::focus::ToolWindowId;
use crate::ui::switcher::{FileRow, Pick, Switcher, SwitcherResult, ToolRow};

impl App {
    /// Open the popup. `back`: select the least recent file; `hold`: Ctrl is
    /// down, so releasing it opens the selection.
    pub(crate) fn open_switcher(&mut self, back: bool, hold: bool) {
        self.palette = None;
        self.menu = None;
        self.menu_bar = None;
        self.hover = None;
        self.strip_hover = None;
        self.completion_close();
        self.signature_close();
        self.dispatcher.reset();
        let editor = &self.editor;
        let recent = editor.mru_ids().iter().filter_map(|id| editor.index_of_id(*id));
        let rest = (0..editor.buffers.len()).filter(|i| !editor.mru_ids().contains(&editor.buffers[*i].id));
        let files: Vec<FileRow> = recent
            .chain(rest)
            .map(|i| {
                let b = &editor.buffers[i];
                let detail = match &b.path {
                    Some(p) => p.strip_prefix(&self.workspace.root).unwrap_or(p).display().to_string(),
                    None => b.name.clone(),
                };
                FileRow { id: b.id, name: b.name.clone(), detail, dirty: b.dirty() }
            })
            .collect();
        let tools: Vec<ToolRow> = ToolWindowId::ALL
            .into_iter()
            .map(|id| ToolRow {
                id,
                title: id.title(),
                shortcut: self
                    .keymap
                    .bindings_for(&Action::App(AppAction::ToggleToolWindow(id)), &["global"])
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
            })
            .collect();
        self.switcher = Some(Switcher::new(files, tools, back, hold));
        self.dirty_frame = true;
    }

    pub(super) fn switcher_key(&mut self, key: KeyEvent) {
        self.dirty_frame = true;
        if let Some(res) = self.switcher.as_mut().and_then(|s| s.handle_key(key)) {
            self.finish_switcher(res);
        }
    }

    pub(super) fn switcher_mouse(&mut self, mouse: &Mouse) {
        if let Some(res) = self.switcher.as_mut().and_then(|s| s.handle_mouse(mouse)) {
            self.finish_switcher(res);
        }
    }

    fn finish_switcher(&mut self, res: SwitcherResult) {
        self.switcher = None;
        self.dirty_frame = true;
        match res {
            SwitcherResult::Cancel => {}
            SwitcherResult::Open(Pick::File(id)) => {
                if self.editor.merge_active() {
                    // `merge_guard` refuses with the one busy notification.
                    let mut cx = ctx!(self);
                    self.editor.handle_action(&EditorAction::Switcher, &mut cx);
                    let fx = cx.effects;
                    self.apply_effects(fx);
                } else if let Some(i) = self.editor.index_of_id(id) {
                    self.focus.set(PanelId::Editor);
                    let mut cx = ctx!(self);
                    self.editor.activate(i, &mut cx);
                    let fx = cx.effects;
                    self.apply_effects(fx);
                }
            }
            SwitcherResult::Open(Pick::Tool(tw)) => {
                if !(self.layout.is_visible(tw) && self.focus.is(tw.panel())) {
                    self.toggle_tool_window(tw);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};

    use crate::actions::{Action, EditorAction};
    use crate::app::tests::{load, test_app};
    use crate::ui::focus::{PanelId, ToolWindowId};
    use crate::ui::menu_bar::needs_buffer;
    use crate::ui::switcher::{Column, Pick};

    fn press(app: &mut crate::app::App, code: KeyCode, mods: KeyModifiers) {
        app.handle_key(KeyEvent::new(code, mods));
    }

    fn release_ctrl(app: &mut crate::app::App) {
        app.handle_key(KeyEvent::new_with_kind(KeyCode::Modifier(ModifierKeyCode::LeftControl), KeyModifiers::NONE, KeyEventKind::Release));
    }

    fn active(app: &crate::app::App) -> &str {
        &app.editor.active_buffer().unwrap().name
    }

    #[tokio::test]
    async fn ctrl_tab_held_cycles_and_release_opens() {
        let (mut app, dir) = test_app("switcher-hold");
        for n in ["a.txt", "b.txt", "c.txt"] {
            load(&mut app, n, "x\n");
        }
        press(&mut app, KeyCode::Tab, KeyModifiers::CONTROL);
        assert!(app.switcher.is_some());
        assert_eq!(active(&app), "c.txt");
        press(&mut app, KeyCode::Tab, KeyModifiers::CONTROL);
        release_ctrl(&mut app);
        assert!(app.switcher.is_none());
        assert_eq!(active(&app), "a.txt");
        assert!(app.focus.is(PanelId::Editor));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn ctrl_tab_tap_switches_to_the_previous_tab() {
        let (mut app, dir) = test_app("switcher-tap");
        for n in ["a.txt", "b.txt"] {
            load(&mut app, n, "x\n");
        }
        press(&mut app, KeyCode::Tab, KeyModifiers::CONTROL);
        release_ctrl(&mut app);
        assert_eq!(active(&app), "a.txt");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn ctrl_shift_tab_starts_backwards() {
        let (mut app, dir) = test_app("switcher-back");
        for n in ["a.txt", "b.txt", "c.txt"] {
            load(&mut app, n, "x\n");
        }
        press(&mut app, KeyCode::BackTab, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        let id = app.editor.buffers.iter().find(|b| b.name == "a.txt").unwrap().id;
        assert_eq!(app.switcher.as_ref().unwrap().selected(), Some(Pick::File(id)));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(active(&app), "a.txt");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn switcher_from_the_menu_waits_for_enter_and_opens_tool_windows() {
        let (mut app, dir) = test_app("switcher-tools");
        load(&mut app, "a.txt", "x\n");
        app.layout.hide_all();
        app.perform(Action::Editor(EditorAction::Switcher));
        release_ctrl(&mut app);
        assert!(app.switcher.is_some(), "no Ctrl was held");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        for _ in 0..ToolWindowId::ALL.len() {
            if app.switcher.as_ref().unwrap().selected() == Some(Pick::Tool(ToolWindowId::Git)) {
                break;
            }
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(app.switcher.as_ref().unwrap().selected(), Some(Pick::Tool(ToolWindowId::Git)));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.switcher.is_none());
        assert!(app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Git));
        // Picking the visible, focused window again keeps it.
        app.perform(Action::Editor(EditorAction::Switcher));
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        for _ in 0..ToolWindowId::ALL.len() {
            if app.switcher.as_ref().unwrap().selected() == Some(Pick::Tool(ToolWindowId::Git)) {
                break;
            }
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Git));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn esc_changes_nothing() {
        let (mut app, dir) = test_app("switcher-esc");
        for n in ["a.txt", "b.txt"] {
            load(&mut app, n, "x\n");
        }
        app.focus.set(PanelId::Project);
        let focus = app.focus.current;
        press(&mut app, KeyCode::Tab, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.switcher.is_none());
        assert_eq!(active(&app), "b.txt");
        assert_eq!(app.focus.current, focus);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn with_no_tabs_it_lists_tool_windows() {
        let (mut app, dir) = test_app("switcher-empty");
        app.perform(Action::Editor(EditorAction::Switcher));
        assert_eq!(app.switcher.as_ref().unwrap().column(), Column::Tools);
        assert!(!needs_buffer(&Action::Editor(EditorAction::Switcher)));
        std::fs::remove_dir_all(dir).ok();
    }
}
