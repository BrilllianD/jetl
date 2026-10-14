//! The Settings window (`ui::settings`): opening it and routing its input.

use crossterm::event::KeyEvent;

use super::App;
use crate::config::FileText;
use crate::event::Mouse;
use crate::ui::settings::{SettingsResult, SettingsWindow};

impl App {
    pub(super) fn open_settings_window(&mut self) {
        self.palette = None;
        self.menu = None;
        self.menu_bar = None;
        self.hover = None;
        self.completion_close();
        let path = self.paths.config_file();
        self.settings = Some(SettingsWindow::new(&FileText::read(&path), &path));
        self.dirty_frame = true;
    }

    pub(super) fn settings_key(&mut self, key: KeyEvent) {
        self.dirty_frame = true;
        if let Some(res) = self.settings.as_mut().and_then(|s| s.handle_key(key)) {
            self.finish_settings(res);
        }
    }

    pub(super) fn settings_mouse(&mut self, m: &Mouse) {
        if let Some(res) = self.settings.as_mut().and_then(|s| s.handle_mouse(m)) {
            self.finish_settings(res);
        }
    }

    fn finish_settings(&mut self, res: SettingsResult) {
        self.dirty_frame = true;
        match res {
            SettingsResult::Close => self.settings = None,
            SettingsResult::OpenConfigFile => {
                self.settings = None;
                self.open_settings();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::actions::{Action, AppAction};
    use crate::app::config_reload::SETTINGS_STUB;
    use crate::app::tests::test_app;
    use crate::ui::focus::{PanelId, ToolWindowId};

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl_alt_s() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL | KeyModifiers::ALT)
    }

    #[tokio::test]
    async fn ctrl_alt_s_opens_and_esc_closes() {
        let (mut app, dir) = test_app("settings-open");
        app.handle_key(ctrl_alt_s());
        assert!(app.settings.is_some());
        key(&mut app, KeyCode::Esc);
        assert!(app.settings.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn git_window_keeps_ctrl_alt_s_for_stash() {
        let (mut app, dir) = test_app("settings-git-shadow");
        app.toggle_tool_window(ToolWindowId::Git);
        app.focus.set(PanelId::Git);
        app.handle_key(ctrl_alt_s());
        assert!(app.settings.is_none(), "git.stash_push wins in the Git window");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn external_row_opens_config_toml() {
        let (mut app, dir) = test_app("settings-external");
        app.perform(Action::App(AppAction::Settings));
        // Tools > Run is page 13: Tools (11) is a parent, Terminal 12, Run 13.
        for _ in 0..13 {
            key(&mut app, KeyCode::Down);
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.settings.as_ref().unwrap().selected_key(), Some("run.tasks"));
        key(&mut app, KeyCode::Enter);
        assert!(app.settings.is_none());
        assert_eq!(std::fs::read_to_string(app.paths.config_file()).unwrap(), SETTINGS_STUB);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn typing_while_open_leaves_the_buffer_alone() {
        let (mut app, dir) = test_app("settings-typing");
        let before = app.editor.active_buffer().map(|b| b.rope.to_string());
        app.perform(Action::App(AppAction::Settings));
        key(&mut app, KeyCode::Char('x'));
        app.handle_paste("pasted".into());
        assert!(app.settings.is_some());
        assert_eq!(app.editor.active_buffer().map(|b| b.rope.to_string()), before);
        let _ = std::fs::remove_dir_all(dir);
    }
}
