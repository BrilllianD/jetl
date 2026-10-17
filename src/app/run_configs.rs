//! The Run Configurations window (`ui::run_configs`): opening it, routing its
//! input and writing `.jetl/run.toml`. Nothing here starts a process.

use crossterm::event::KeyEvent;

use super::App;
use crate::config::FileText;
use crate::event::{Level, Mouse};
use crate::run::edit;
use crate::run::{self, MatcherKind, Origin, RunTask};
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::run_configs::{ConfigsResult, Field, FieldValue, RunConfigsWindow};

impl App {
    /// A window over the files as they are now. Reads two small files on the
    /// main thread, as the Settings window does.
    fn build_run_configs(&self) -> RunConfigsWindow {
        let root = &self.workspace.root;
        let (base, read_error) = match FileText::read(&edit::project_file(root)) {
            FileText::Missing => (String::new(), None),
            FileText::Text(t) => (t, None),
            FileText::Unreadable(e) => (String::new(), Some(format!(".jetl/run.toml: {e}"))),
        };
        let project = match read_error {
            Some(e) => Err(e),
            None => edit::load_project(&base),
        };
        let mut others: Vec<RunTask> = self.config.run.all().into_iter().map(|t| RunTask { origin: Origin::Config, ..t }).collect();
        if others.is_empty() && project.as_ref().is_ok_and(|p| p.is_empty()) {
            others = run::detect(root);
        }
        RunConfigsWindow::new(base, project, others)
    }

    pub(super) fn open_run_configs(&mut self) {
        self.close_palette();
        self.menu = None;
        self.menu_bar = None;
        self.hover = None;
        self.completion_close();
        self.run_configs = Some(self.build_run_configs());
        self.dirty_frame = true;
    }

    /// The window shows the file as it is now, unless it holds edits: then
    /// Apply reports the outside change.
    pub(super) fn run_configs_file_changed(&mut self) {
        let Some(old) = self.run_configs.as_ref().filter(|w| !w.dirty()) else { return };
        let mut w = self.build_run_configs();
        w.keep_view_of(old);
        self.run_configs = Some(w);
        self.dirty_frame = true;
    }

    pub(super) fn run_configs_key(&mut self, key: KeyEvent) {
        self.dirty_frame = true;
        if let Some(res) = self.run_configs.as_mut().and_then(|w| w.handle_key(key)) {
            self.finish_run_configs(res);
        }
    }

    pub(super) fn run_configs_mouse(&mut self, m: &Mouse) {
        if let Some(res) = self.run_configs.as_mut().and_then(|w| w.handle_mouse(m)) {
            self.finish_run_configs(res);
        }
    }

    fn finish_run_configs(&mut self, res: ConfigsResult) {
        self.dirty_frame = true;
        match res {
            ConfigsResult::Close => self.run_configs = None,
            ConfigsResult::EditField(f) => self.dialog = self.run_config_field_dialog(f, None, ""),
            ConfigsResult::Apply { close } => self.apply_run_configs(close),
            ConfigsResult::ConfirmDiscard => {
                self.dialog = Some(Dialog::confirm(
                    DialogId::RunConfigsDiscard,
                    "Run Configurations",
                    "Discard changes to run configurations?",
                    &["Discard", "Keep Editing"],
                ));
            }
            ConfigsResult::Notify(t) => self.notify(Level::Info, t),
        }
    }

    fn apply_run_configs(&mut self, close: bool) {
        let Some(w) = self.run_configs.as_ref() else { return };
        if let Err(e) = w.validate() {
            return self.notify(Level::Error, e);
        }
        let (edits, renames) = (w.edits(), w.renames());
        match edit::write_project(&self.workspace.root, w.base(), &edits) {
            Ok(_) => {
                for (old, new) in renames {
                    if self.run.selected.as_deref() == Some(old.as_str()) {
                        self.run.selected = Some(new);
                    }
                }
                self.load_run_tasks();
                if close {
                    self.run_configs = None;
                } else if let Some(old) = self.run_configs.take() {
                    let mut w = self.build_run_configs();
                    w.keep_view_of(&old);
                    self.run_configs = Some(w);
                }
            }
            Err(e) => self.notify(Level::Error, e),
        }
    }

    /// The dialog that asks for a new value of `f`: `typed` is what to
    /// prefill an input with (else the current value), `note` leads its message.
    fn run_config_field_dialog(&self, f: Field, typed: Option<&str>, note: &str) -> Option<Dialog> {
        let w = self.run_configs.as_ref()?;
        let id = DialogId::RunConfigField(f);
        if f == Field::Matcher {
            let items = std::iter::once("auto").chain(MatcherKind::ALL.iter().map(|m| m.label())).map(String::from).collect();
            return Some(Dialog::choice(id, f.label(), "", items));
        }
        let hint = match f {
            Field::Args => "Shell words:",
            Field::Env => "Shell words, KEY=value:",
            _ => "",
        };
        let initial = typed.map_or_else(|| w.field_initial(f), str::to_string);
        Some(Dialog::input(id, f.label(), &format!("{note}{hint}"), &initial, "OK"))
    }

    /// The answers of the window's dialogs; false for other dialogs.
    pub(super) fn finish_run_configs_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        match id {
            DialogId::RunConfigsDiscard => {
                if matches!(res, DialogResult::Button(0)) {
                    self.run_configs = None;
                }
            }
            DialogId::RunConfigField(f) => {
                let value = match res {
                    DialogResult::Text(t) => FieldValue::Text(t.clone()),
                    DialogResult::Choice(0) => FieldValue::Matcher(None),
                    DialogResult::Choice(i) => FieldValue::Matcher(MatcherKind::ALL.get(i - 1).copied()),
                    _ => return true,
                };
                let Some(w) = &mut self.run_configs else { return true };
                if let Err(e) = w.set_field(*f, value) {
                    let typed = if let DialogResult::Text(t) = res { Some(t.as_str()) } else { None };
                    self.dialog = self.run_config_field_dialog(*f, typed, &format!("{e}. "));
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::actions::{Action, RunAction};
    use crate::app::tests::{handle_next, test_app};
    use crate::event::AppMsg;
    use crate::ui::dialog::DialogKind;

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn file(app: &App) -> std::path::PathBuf {
        edit::project_file(&app.workspace.root)
    }

    fn write(app: &App, text: &str) {
        std::fs::create_dir_all(file(app).parent().unwrap()).unwrap();
        std::fs::write(file(app), text).unwrap();
    }

    fn open(app: &mut App) {
        app.perform(Action::Run(RunAction::EditConfigs));
        assert!(app.run_configs.is_some());
    }

    /// Answer the open dialog as the user would.
    fn answer(app: &mut App, res: DialogResult) {
        let d = app.dialog.take().expect("a dialog");
        app.finish_dialog(d.id, res);
    }

    fn errors(app: &App) -> Vec<String> {
        app.notifications.iter().filter(|n| n.level == Level::Error).map(|n| n.text.clone()).collect()
    }

    fn names(app: &App) -> Vec<String> {
        let w = app.run_configs.as_ref().unwrap();
        w.entries().iter().map(|e| e.task.name.clone()).collect()
    }

    const A: &str = "# mine\n[[configs]]\nname = \"a\"\ncommand = \"echo a\"\n";

    #[tokio::test]
    async fn apply_writes_the_project_file_keeping_comments() {
        let (mut app, dir) = test_app("rc-apply");
        write(&app, A);
        open(&mut app);
        // `a` is selected; Form, the Arguments row.
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let d = app.dialog.as_ref().expect("an input dialog");
        assert_eq!(d.id, DialogId::RunConfigField(Field::Args));
        assert!(matches!(d.kind, DialogKind::Input { .. }));
        answer(&mut app, DialogResult::Text("x 'y z'".into()));
        ctrl(&mut app, 's');
        assert_eq!(
            std::fs::read_to_string(file(&app)).unwrap(),
            "# mine\n[[configs]]\nname = \"a\"\ncommand = \"echo a\"\nargs = [\"x\", \"y z\"]\n"
        );
        handle_next(&mut app, |m| matches!(m, AppMsg::RunTasks { .. })).await;
        assert_eq!(app.run.tasks[0].args, ["x", "y z"]);
        assert!(app.run_configs.as_ref().is_some_and(|w| !w.dirty()), "the window shows the file again");
        assert!(!app.run.running());
        assert_eq!(app.run.seq, 0, "saving runs nothing");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_bad_value_asks_again_with_the_typed_text() {
        let (mut app, dir) = test_app("rc-bad-value");
        write(&app, A);
        open(&mut app);
        key(&mut app, KeyCode::Enter);
        for _ in 0..4 {
            key(&mut app, KeyCode::Down);
        }
        key(&mut app, KeyCode::Enter);
        answer(&mut app, DialogResult::Text("oops".into()));
        let d = app.dialog.as_ref().expect("asked again");
        assert!(d.message.starts_with("Not KEY=value: oops. "), "{}", d.message);
        assert!(!app.run_configs.as_ref().unwrap().dirty());
        // Output is a choice: auto, problems, none.
        answer(&mut app, DialogResult::Cancel);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let DialogKind::Choice { items, .. } = &app.dialog.as_ref().unwrap().kind else { panic!("a choice") };
        assert_eq!(items, &["auto", "problems", "none"]);
        answer(&mut app, DialogResult::Choice(2));
        ctrl(&mut app, 's');
        assert!(std::fs::read_to_string(file(&app)).unwrap().contains("matcher = \"none\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rename_follows_the_selection() {
        let (mut app, dir) = test_app("rc-rename");
        write(&app, A);
        app.run.selected = Some("a".into());
        open(&mut app);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::RunConfigField(Field::Name));
        answer(&mut app, DialogResult::Text("b".into()));
        // Tab to the buttons, OK.
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.run.selected.as_deref(), Some("b"));
        assert!(app.run_configs.is_none());
        assert!(std::fs::read_to_string(file(&app)).unwrap().contains("name = \"b\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn outside_change_is_not_overwritten() {
        let (mut app, dir) = test_app("rc-outside");
        write(&app, A);
        open(&mut app);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        answer(&mut app, DialogResult::Text("x".into()));
        let outside = "[[configs]]\nname = \"other\"\ncommand = \"true\"\n";
        write(&app, outside);
        // A change while editing leaves the staged edits alone.
        app.run_configs_file_changed();
        assert!(app.run_configs.as_ref().unwrap().dirty());
        ctrl(&mut app, 's');
        assert_eq!(errors(&app), [".jetl/run.toml changed on disk; reopen Edit Configurations"]);
        assert_eq!(std::fs::read_to_string(file(&app)).unwrap(), outside);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_clean_window_follows_the_file() {
        let (mut app, dir) = test_app("rc-follow");
        write(&app, A);
        open(&mut app);
        assert_eq!(names(&app), ["a"]);
        write(&app, &format!("{A}[[configs]]\nname = \"c\"\ncommand = \"true\"\n"));
        app.run_configs_file_changed();
        assert_eq!(names(&app), ["a", "c"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn invalid_entry_writes_nothing() {
        let (mut app, dir) = test_app("rc-invalid");
        open(&mut app);
        key(&mut app, KeyCode::Insert);
        ctrl(&mut app, 's');
        assert_eq!(errors(&app), ["Configuration 'New configuration' has no command"]);
        assert!(!file(&app).exists());
        assert!(!file(&app).parent().unwrap().exists(), "not even the folder");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn detected_defaults_are_listed_when_nothing_is_configured() {
        let (mut app, dir) = test_app("rc-detected");
        std::fs::write(app.workspace.root.join("Cargo.toml"), "").unwrap();
        open(&mut app);
        assert_eq!(names(&app), ["cargo build", "cargo test", "cargo run"]);
        assert!(app.run_configs.as_ref().unwrap().entries().iter().all(|e| e.slot.is_none() && !e.new));
        key(&mut app, KeyCode::Down);
        ctrl(&mut app, 'd');
        assert_eq!(names(&app).last().map(String::as_str), Some("cargo test (copy)"));
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Enter);
        assert!(app.run_configs.is_none());
        let text = std::fs::read_to_string(file(&app)).unwrap();
        assert!(text.contains("[[configs]]\nname = \"cargo test (copy)\"\ncommand = [\"cargo\", \"test\"]\n"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn read_only_entries_say_where_they_come_from() {
        let (mut app, dir) = test_app("rc-readonly");
        std::fs::create_dir_all(&app.paths.config_dir).unwrap();
        std::fs::write(app.paths.config_file(), "[lsp]\nenabled = false\n[[run.configs]]\nname = \"g\"\ncommand = \"true\"\n").unwrap();
        app.config_changed(vec![crate::config::watch::ConfigFile::Config]);
        open(&mut app);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert!(app.notifications.iter().any(|n| n.text == "Defined in config.toml: Copy makes an editable project copy"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn esc_with_changes_asks_before_discarding() {
        let (mut app, dir) = test_app("rc-discard");
        write(&app, A);
        open(&mut app);
        key(&mut app, KeyCode::Esc);
        assert!(app.run_configs.is_none(), "clean: closes");
        open(&mut app);
        key(&mut app, KeyCode::Delete);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::RunConfigsDiscard);
        answer(&mut app, DialogResult::Button(1));
        assert!(app.run_configs.is_some());
        key(&mut app, KeyCode::Esc);
        answer(&mut app, DialogResult::Button(0));
        assert!(app.run_configs.is_none());
        assert_eq!(std::fs::read_to_string(file(&app)).unwrap(), A, "nothing was written");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn modal_takes_keys_and_paste() {
        let (mut app, dir) = test_app("rc-modal");
        let before = app.editor.active_buffer().map(|b| b.rope.to_string());
        open(&mut app);
        key(&mut app, KeyCode::Char('x'));
        app.handle_paste("pasted".into());
        assert!(app.run_configs.is_some());
        assert_eq!(app.editor.active_buffer().map(|b| b.rope.to_string()), before);
        assert!(!app.run.running());
        let _ = std::fs::remove_dir_all(dir);
    }
}
