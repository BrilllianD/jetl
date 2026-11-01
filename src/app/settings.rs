//! The Settings window (`ui::settings`): opening it and routing its input.

use crossterm::event::KeyEvent;

use super::App;
use crate::actions::{Action, REGISTRY};
use crate::config::FileText;
use crate::config::catalog::{Kind, setting};
use crate::config::edit::{Edit, KeymapEdit, Value};
use crate::config::keymap::{KeyCombo, chord_key, chord_text, parse_chord};
use crate::event::Level;
use crate::event::Mouse;
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::settings::{SettingsResult, SettingsWindow};
use crate::ui::settings_keymap::context_label;

impl App {
    pub(super) fn open_settings_window(&mut self) {
        self.close_palette();
        self.menu = None;
        self.menu_bar = None;
        self.hover = None;
        self.completion_close();
        let path = self.paths.config_file();
        let mut w = SettingsWindow::new(&FileText::read(&path), &path);
        w.set_keymap(&self.keymap);
        self.settings = Some(w);
        self.dirty_frame = true;
    }

    /// Run `doctor::probe` off the main thread for the Settings window's
    /// Dependencies page; the answer comes back as `AppMsg::Dependencies`.
    pub(super) fn check_dependencies(&mut self) {
        // Process-wide, so an answer meant for a window closed since cannot match a new one.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let Some(w) = &mut self.settings else { return };
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        w.probe_started(seq);
        let (cfg, root, generation, tx) =
            (self.config.clone(), self.workspace.root.clone(), self.workspace.generation, self.bus_tx.clone());
        let term = crate::doctor::TermCaps::running();
        tokio::task::spawn_blocking(move || {
            let report = crate::doctor::probe(&cfg, &root, &term);
            let _ = tx.send(crate::event::AppMsg::Dependencies { generation, seq, report });
        });
    }

    pub(super) fn dependencies_checked(&mut self, generation: u64, seq: u64, report: crate::doctor::Report) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping stale dependency check");
            if let Some(w) = &mut self.settings {
                w.probe_dropped(seq);
            }
            return;
        }
        if let Some(w) = &mut self.settings {
            w.dependencies_loaded(seq, report);
        }
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
            SettingsResult::OpenConfigFile if self.settings.as_ref().is_some_and(|w| w.has_pending()) => {
                self.notify(Level::Warn, "Apply or discard the changed settings first");
            }
            SettingsResult::OpenConfigFile => {
                self.settings = None;
                self.open_settings();
            }
            SettingsResult::CheckDependencies => self.check_dependencies(),
            SettingsResult::EditValue(key) => self.dialog = self.value_dialog(key, None, ""),
            SettingsResult::Apply { close } => self.apply_settings(close),
            SettingsResult::EditBinding { add } => self.dialog = self.binding_dialog(add),
            SettingsResult::ConfirmDiscard => {
                let n = self.settings.as_ref().map_or(0, |w| w.pending().len() + w.keymap_pending().len());
                let noun = if n == 1 { "setting" } else { "settings" };
                self.dialog = Some(Dialog::confirm(
                    DialogId::SettingsDiscard,
                    "Settings",
                    &format!("Discard {n} changed {noun}?"),
                    &["Discard", "Keep Editing"],
                ));
            }
        }
    }

    /// Write the staged settings to `config.toml`, then the staged keys to
    /// `keymap.toml`. A failure notifies and keeps what was not written staged.
    fn apply_settings(&mut self, close: bool) {
        let Some(w) = &self.settings else { return };
        let (edits, keys) = (w.pending().to_vec(), w.keymap_pending().to_vec());
        if !edits.is_empty() {
            if let Err(e) = self.write_settings(&edits) {
                self.notify(Level::Error, e);
                return;
            }
            let path = self.paths.config_file();
            if let Some(w) = &mut self.settings {
                w.clear_pending();
                w.file_changed(&FileText::read(&path), &path);
            }
        }
        if !keys.is_empty() {
            if let Err(e) = crate::config::edit::write_keymap(&self.paths.keymap_file(), &keys) {
                self.notify(Level::Error, e);
                return;
            }
            // Cleared before the reload, which hands the new keymap to the window as its base.
            if let Some(w) = &mut self.settings {
                w.clear_keymap_pending();
            }
            self.config_changed(vec![crate::config::watch::ConfigFile::Keymap]);
        }
        if close {
            self.settings = None;
        }
    }

    /// The key input for the selected action (`add`) or key row of the Keymap page.
    fn binding_dialog(&self, add: bool) -> Option<Dialog> {
        let s = self.settings.as_ref()?.keymap_selection()?;
        let old = if add { None } else { s.chord.clone() };
        Some(key_dialog(s.ctx, s.action, old, None, ""))
    }

    /// The answer of the key input: stage it, or ask again / ask to replace.
    fn finish_binding(&mut self, ctx: &str, action: &Action, old: &Option<Vec<KeyCombo>>, res: &DialogResult) {
        let DialogResult::Text(t) = res else { return };
        let reopen = |note: &str| Some(key_dialog(ctx.to_string(), action.clone(), old.clone(), Some(t), note));
        let chord = match parse_chord(t) {
            Ok(c) => c,
            Err(e) => {
                self.dialog = reopen(&format!("Not a key ({e}). "));
                return;
            }
        };
        if parse_chord(&chord_key(&chord)).ok().as_ref() != Some(&chord) {
            self.dialog = reopen("This key cannot be written to keymap.toml. ");
            return;
        }
        if old.as_ref() == Some(&chord) {
            return;
        }
        let taken = self.settings.as_ref().and_then(|w| w.shown_keymap().table(ctx)).and_then(|t| t.bindings.get(&chord)).cloned();
        match taken {
            Some(other) if &other != action => {
                let desc = REGISTRY.iter().find(|r| r.action == other).map_or("?", |r| r.desc);
                self.dialog = Some(Dialog::confirm(
                    DialogId::KeymapReplace { ctx: ctx.to_string(), action: action.clone(), old: old.clone(), chord: chord.clone() },
                    "Keymap",
                    &format!("{} is bound to \"{desc}\" in {}. Replace it?", chord_text(&chord), context_label(ctx)),
                    &["Replace", "Cancel"],
                ));
            }
            _ => self.stage_binding(ctx, action, old, chord),
        }
    }

    /// Stage `chord` for `action` in `ctx`, replacing the key `old` when editing.
    fn stage_binding(&mut self, ctx: &str, action: &Action, old: &Option<Vec<KeyCombo>>, chord: Vec<KeyCombo>) {
        let (Some(id), Some(w)) = (action.id(), &mut self.settings) else { return };
        let mut edits: Vec<KeymapEdit> = old.iter().map(|c| KeymapEdit::Unbind { ctx: ctx.to_string(), chord: c.clone() }).collect();
        edits.push(KeymapEdit::Bind { ctx: ctx.to_string(), chord, id });
        w.stage_keymap(edits);
    }

    /// The dialog that asks for a new value of `key`: `typed` is what to
    /// prefill an input with (else the current value), `note` leads its message.
    fn value_dialog(&self, key: &'static str, typed: Option<&str>, note: &str) -> Option<Dialog> {
        let st = setting(key)?;
        let cur = self.settings.as_ref()?.value(key);
        let id = DialogId::SettingValue(key);
        let initial = typed.map_or_else(|| text_of(&cur), str::to_string);
        Some(match st.kind {
            Kind::Choice { options, custom } if typed.is_none() => {
                let mut items: Vec<String> = options.iter().map(|o| o.to_string()).collect();
                if custom {
                    items.push("Other…".into());
                }
                Dialog::choice(id, st.label, "", items)
            }
            Kind::Int { max, .. } => {
                let msg = if max == i64::MAX {
                    "A whole number, 0 or more:".to_string()
                } else {
                    format!("A whole number from {} to {}:", int_min(st.kind), max)
                };
                Dialog::input(id, st.label, &format!("{note}{msg}"), &initial, "OK")
            }
            Kind::List => Dialog::input(id, st.label, &format!("{note}Comma-separated:"), &initial, "OK"),
            _ => Dialog::input(id, st.label, note, &initial, "OK"),
        })
    }

    /// The answers of the Settings window's dialogs; false for other dialogs.
    pub(super) fn finish_settings_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        match id {
            DialogId::SettingsDiscard => {
                if matches!(res, DialogResult::Button(0)) {
                    self.settings = None;
                }
            }
            DialogId::KeymapBinding { ctx, action, old } => {
                if self.settings.is_some() {
                    self.finish_binding(ctx, action, old, res);
                }
            }
            DialogId::KeymapReplace { ctx, action, old, chord } => {
                if matches!(res, DialogResult::Button(0)) {
                    self.stage_binding(ctx, action, old, chord.clone());
                }
            }
            DialogId::SettingValue(key) => {
                if self.settings.is_some() {
                    self.finish_setting_value(key, res);
                }
            }
            _ => return false,
        }
        true
    }

    fn finish_setting_value(&mut self, key: &'static str, res: &DialogResult) {
        let Some(st) = setting(key) else { return };
        let edit = match (res, st.kind) {
            (DialogResult::Choice(i), Kind::Choice { options, .. }) => {
                if let Some(o) = options.get(*i) {
                    Value::Str(o.to_string())
                } else {
                    // "Other…": ask for the text.
                    self.dialog =
                        self.value_dialog(key, Some(&self.settings.as_ref().map_or(String::new(), |w| text_of(&w.value(key)))), "");
                    return;
                }
            }
            (DialogResult::Text(t), Kind::Int { min, max }) => match t.trim().parse::<i64>() {
                Ok(n) if (min..=max).contains(&n) => Value::Int(n),
                _ => {
                    self.dialog = self.value_dialog(key, Some(t), "Not a number in range. ");
                    return;
                }
            },
            (DialogResult::Text(t), Kind::List) => {
                Value::List(t.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect())
            }
            (DialogResult::Text(t), _) => Value::Str(t.clone()),
            _ => return,
        };
        if let Some(w) = &mut self.settings {
            w.set_pending(key, Edit::Set(edit));
        }
    }
}

fn int_min(kind: Kind) -> i64 {
    match kind {
        Kind::Int { min, .. } => min,
        _ => 0,
    }
}

/// The key-capture dialog for `action` in `ctx`: `old` is the key being edited
/// (`None` adds one), `typed` what to prefill instead, `note` leads the message.
fn key_dialog(ctx: String, action: Action, old: Option<Vec<KeyCombo>>, typed: Option<&str>, note: &str) -> Dialog {
    let title = if old.is_some() { "Edit Key" } else { "Add Key" };
    let desc = REGISTRY.iter().find(|r| r.action == action).map_or("?", |r| r.desc);
    let message = format!("{note}{desc} in {}. Press the key, or type it as in ctrl-shift-f, Ctrl+K Ctrl+C or g g:", context_label(&ctx));
    let initial = typed.map_or_else(|| old.as_deref().map(chord_key).unwrap_or_default(), str::to_string);
    Dialog::key_capture(DialogId::KeymapBinding { ctx, action, old }, title, &message, &initial)
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::List(l) => l.join(", "),
        Value::Bool(b) => b.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    async fn dependencies_page_runs_the_probe() {
        use crate::doctor::{Group, Status};
        use crate::event::AppMsg;
        let (mut app, dir) = test_app("settings-deps");
        setup(&mut app, NO_LSP);
        key(&mut app, KeyCode::End);
        crate::app::tests::handle_next(&mut app, |m| matches!(m, AppMsg::Dependencies { .. })).await;
        let r = app.settings.as_ref().unwrap().dependencies().expect("report shown");
        assert!(r.checks.iter().filter(|c| c.group == Group::Lsp).all(|c| c.status == Status::Off), "{r:?}");
        assert!(r.checks.iter().any(|c| c.group == Group::Grammar));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn body(app: &mut App) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let buf = term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()).collect()
    }

    #[tokio::test]
    async fn stale_dependency_answers_are_dropped() {
        use crate::doctor::Report;
        use crate::event::AppMsg;
        let (mut app, dir) = test_app("settings-deps-stale");
        setup(&mut app, NO_LSP);
        key(&mut app, KeyCode::End);
        let generation = app.workspace.generation;
        let (running, seq) = app.settings.as_ref().unwrap().awaited_probe();
        assert!(running);
        assert!(body(&mut app).iter().any(|l| l.contains("Checking…")));
        app.handle_msg(AppMsg::Dependencies { generation: generation + 1, seq, report: Report::default() });
        assert!(app.settings.as_ref().unwrap().dependencies().is_none());
        assert!(!app.settings.as_ref().unwrap().awaited_probe().0);
        let l = body(&mut app);
        assert!(l.iter().any(|l| l.contains("Not checked. Enter checks.")), "{l:#?}");
        assert!(!l.iter().any(|l| l.contains("Checking…")), "{l:#?}");
        assert!(!l.iter().any(|l| l.contains("(checking…)")), "{l:#?}");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        let (running, s2) = app.settings.as_ref().unwrap().awaited_probe();
        assert!(running);
        assert_ne!(s2, seq);
        assert!(body(&mut app).iter().any(|l| l.contains("Checking…")));
        key(&mut app, KeyCode::Esc);
        assert!(app.settings.is_none());
        app.handle_msg(AppMsg::Dependencies { generation, seq, report: Report::default() });
        assert!(app.settings.is_none());
        let _ = std::fs::remove_dir_all(dir);
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
        // Tools > Run is page 14: Tools (12) is a parent, Terminal 13, Run 14.
        for _ in 0..14 {
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
    async fn keymap_page_follows_a_keymap_reload() {
        let (mut app, dir) = test_app("settings-keymap-reload");
        setup(&mut app, NO_LSP);
        let chord = crate::config::keymap::parse_chord("ctrl-alt-y").unwrap();
        let quit = Action::App(AppAction::Quit);
        let bound = |app: &App| app.settings.as_ref().unwrap().keymap_page().keymap().tables["global"].bindings.get(&chord) == Some(&quit);
        assert!(!bound(&app));
        std::fs::write(app.paths.keymap_file(), "[global]\n\"ctrl-alt-y\" = \"app.quit\"\n").unwrap();
        app.config_changed(vec![crate::config::watch::ConfigFile::Keymap]);
        assert!(bound(&app), "an open window follows the reload");
        key(&mut app, KeyCode::Esc);
        assert!(app.settings.is_none());
        app.perform(Action::App(AppAction::Settings));
        assert!(bound(&app), "a new window gets app.keymap");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn kp(app: &App) -> usize {
        app.settings.as_ref().unwrap().keymap_pending().len()
    }

    /// Open Settings on the Keymap page (page 6) with the page focused.
    fn keymap_page(app: &mut App) {
        for _ in 0..6 {
            key(app, KeyCode::Down);
        }
        key(app, KeyCode::Enter);
    }

    /// Search `query`, then select the first Action row of the first context.
    fn search(app: &mut App, query: &str) {
        type_text(app, query);
        assert!(app.settings.as_ref().unwrap().keymap_selection().is_some_and(|s| s.chord.is_none()));
    }

    fn global_binds(app: &App, key: &str) -> Option<Action> {
        app.keymap.table("global").and_then(|t| t.bindings.get(&parse_chord(key).unwrap())).cloned()
    }

    fn capture(app: &App) -> &crate::ui::key_capture::KeyCaptureForm {
        let Some(crate::ui::dialog::DialogKind::KeyCapture(f)) = app.dialog.as_ref().map(|d| &d.kind) else {
            panic!("no key capture dialog")
        };
        f
    }

    fn dialog_input(app: &App) -> String {
        capture(app).text().to_string()
    }

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.handle_key(KeyEvent::new(code, mods));
    }

    const MINE: &str = "# mine\n[global]\n\"f2\" = \"app.about\"\n";

    #[tokio::test]
    async fn keymap_edit_then_ok_rebinds_after_the_reload() {
        let (mut app, dir) = test_app("settings-keymap-ok");
        setup(&mut app, NO_LSP);
        keymap_page(&mut app);
        search(&mut app, "app.quit");
        let s = app.settings.as_ref().unwrap().keymap_selection().unwrap();
        assert_eq!((s.ctx.as_str(), s.action.clone()), ("global", Action::App(AppAction::Quit)));
        key(&mut app, KeyCode::Insert);
        assert!(matches!(app.dialog.as_ref().unwrap().id, DialogId::KeymapBinding { old: None, .. }));
        press(&mut app, KeyCode::Char('y'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(dialog_input(&app), "Ctrl+Alt+Y");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(kp(&app), 1);
        assert_eq!(global_binds(&app, "ctrl-alt-y"), None);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Enter);
        assert!(app.settings.is_none());
        assert_eq!(global_binds(&app, "ctrl-alt-y"), Some(Action::App(AppAction::Quit)));
        let file = std::fs::read_to_string(app.paths.keymap_file()).unwrap();
        assert!(file.contains("\"Ctrl+Alt+Y\" = \"app.quit\""), "{file}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn write_mine(app: &mut App) {
        std::fs::create_dir_all(&app.paths.config_dir).unwrap();
        std::fs::write(app.paths.keymap_file(), MINE).unwrap();
        app.config_changed(vec![crate::config::watch::ConfigFile::Keymap]);
    }

    /// Settings on the Keymap page with the `Ctrl+Q` key row selected.
    fn on_ctrl_q(app: &mut App) {
        keymap_page(app);
        search(app, "ctrl+q");
        key(app, KeyCode::Down);
        let s = app.settings.as_ref().unwrap().keymap_selection().unwrap();
        assert_eq!(s.chord, Some(parse_chord("ctrl-q").unwrap()));
    }

    #[tokio::test]
    async fn keymap_apply_keeps_the_window_open_with_nothing_pending() {
        let (mut app, dir) = test_app("settings-keymap-apply");
        write_mine(&mut app);
        setup(&mut app, NO_LSP);
        on_ctrl_q(&mut app);
        key(&mut app, KeyCode::Delete);
        assert_eq!(kp(&app), 1);
        ctrl_s(&mut app);
        let w = app.settings.as_ref().expect("window stays open");
        assert!(w.keymap_pending().is_empty() && !w.has_pending());
        assert_eq!(global_binds(&app, "ctrl-q"), None);
        let file = std::fs::read_to_string(app.paths.keymap_file()).unwrap();
        assert!(file.starts_with(MINE), "{file}");
        assert!(file.contains("\"Ctrl+Q\" = \"none\""), "{file}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn keymap_cancel_then_discard_leaves_keymap_toml_untouched() {
        let (mut app, dir) = test_app("settings-keymap-discard");
        write_mine(&mut app);
        setup(&mut app, NO_LSP);
        on_ctrl_q(&mut app);
        key(&mut app, KeyCode::Delete);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::SettingsDiscard);
        key(&mut app, KeyCode::Enter);
        assert!(app.settings.is_none());
        assert_eq!(std::fs::read_to_string(app.paths.keymap_file()).unwrap(), MINE);
        assert!(global_binds(&app, "ctrl-q").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_taken_key_asks_before_replacing_and_bad_keys_reopen() {
        let (mut app, dir) = test_app("settings-keymap-replace");
        setup(&mut app, NO_LSP);
        keymap_page(&mut app);
        search(&mut app, "app.quit");
        key(&mut app, KeyCode::Insert);
        app.handle_paste("ctrl-".into());
        key(&mut app, KeyCode::Enter);
        let d = app.dialog.as_ref().unwrap();
        assert!(matches!(d.id, DialogId::KeymapBinding { .. }));
        assert!(d.message.starts_with("Not a key ("), "{}", d.message);
        assert_eq!(dialog_input(&app), "ctrl-");
        assert!(capture(&app).capturing());
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(dialog_input(&app), "Ctrl+S");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.dialog.as_ref().unwrap().id, DialogId::KeymapReplace { .. }));
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        let w = app.settings.as_ref().unwrap();
        let quit = Action::App(AppAction::Quit);
        assert_eq!(w.shown_keymap().table("global").unwrap().bindings.get(&parse_chord("ctrl-s").unwrap()), Some(&quit));
        assert_eq!(kp(&app), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn edit_prefills_the_key_and_keeps_case() {
        let (mut app, dir) = test_app("settings-keymap-prefill");
        setup(&mut app, NO_LSP);
        on_ctrl_q(&mut app);
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.dialog.as_ref().unwrap().id, DialogId::KeymapBinding { old: Some(_), .. }));
        assert_eq!(dialog_input(&app), "Ctrl+Q");
        assert!(capture(&app).capturing());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn open_add_quit(app: &mut App) {
        setup(app, NO_LSP);
        keymap_page(app);
        search(app, "app.quit");
        key(app, KeyCode::Insert);
    }

    #[tokio::test]
    async fn add_key_captures_a_two_stroke_chord() {
        use crate::ui::key_capture::Part;
        let (mut app, dir) = test_app("settings-keymap-two-stroke");
        open_add_quit(&mut app);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::ALT);
        assert_eq!(dialog_input(&app), "Alt+K");
        assert_eq!(capture(&app).focus(), Part::Ok);
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::BackTab);
        assert_eq!(capture(&app).focus(), Part::Toggle);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(dialog_input(&app), "Alt+K");
        key(&mut app, KeyCode::Tab);
        assert_eq!(capture(&app).focus(), Part::Second);
        assert!(capture(&app).capturing());
        press(&mut app, KeyCode::Char('c'), KeyModifiers::ALT);
        assert_eq!(dialog_input(&app), "Alt+K Alt+C");
        assert_eq!(capture(&app).focus(), Part::Ok);
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(kp(&app), 1);
        let w = app.settings.as_ref().unwrap();
        let got = w.shown_keymap().table("global").unwrap().bindings.get(&parse_chord("alt-k alt-c").unwrap());
        assert_eq!(got, Some(&Action::App(AppAction::Quit)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn add_key_text_fallback_and_esc() {
        use crate::ui::key_capture::Part;
        let (mut app, dir) = test_app("settings-keymap-fallback");
        open_add_quit(&mut app);
        key(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
        assert!(app.settings.is_some());
        assert_eq!(kp(&app), 0);
        key(&mut app, KeyCode::Insert);
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(dialog_input(&app), "a");
        assert_eq!(capture(&app).focus(), Part::Ok);
        key(&mut app, KeyCode::BackTab);
        assert_eq!(capture(&app).focus(), Part::Text);
        key(&mut app, KeyCode::Backspace);
        type_text(&mut app, "f13");
        assert_eq!(dialog_input(&app), "f13");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(kp(&app), 1);
        let w = app.settings.as_ref().unwrap();
        let got = w.shown_keymap().table("global").unwrap().bindings.get(&parse_chord("f13").unwrap());
        assert_eq!(got, Some(&Action::App(AppAction::Quit)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn capture_records_the_paste_key_and_tab_moves_focus() {
        use crate::ui::key_capture::Part;
        let (mut app, dir) = test_app("settings-keymap-paste-key");
        open_add_quit(&mut app);
        press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(app.dialog.is_some());
        assert_eq!(dialog_input(&app), "Ctrl+V");
        for _ in 0..3 {
            key(&mut app, KeyCode::BackTab);
        }
        assert_eq!(capture(&app).focus(), Part::First);
        assert!(capture(&app).capturing());
        key(&mut app, KeyCode::Tab);
        assert_eq!(dialog_input(&app), "Ctrl+V");
        assert_eq!(capture(&app).focus(), Part::Toggle);
        key(&mut app, KeyCode::Tab);
        assert_eq!(capture(&app).focus(), Part::Text);
        assert!(!capture(&app).capturing());
        assert_eq!(app.paste_target(), Some(crate::app::clipboard::PasteTarget::Dialog));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn typing_while_open_leaves_the_buffer_alone() {
        let (mut app, dir) = test_app("settings-typing");
        crate::app::tests::load(&mut app, "a.txt", "hello\n");
        let before = app.editor.active_buffer().map(|b| b.rope.to_string());
        app.perform(Action::App(AppAction::Settings));
        key(&mut app, KeyCode::Char('x'));
        app.handle_paste("pasted".into());
        assert!(app.settings.is_some());
        assert_eq!(before.as_deref(), Some("hello\n"));
        assert_eq!(app.editor.active_buffer().map(|b| b.rope.to_string()), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    const NO_LSP: &str = "[lsp]\nenabled = false\n";

    fn setup(app: &mut App, text: &str) {
        std::fs::create_dir_all(&app.paths.config_dir).unwrap();
        std::fs::write(app.paths.config_file(), text).unwrap();
        app.config_changed(vec![crate::config::watch::ConfigFile::Config]);
        app.perform(Action::App(AppAction::Settings));
    }

    /// Select `key`'s page, then its row, with the page focused.
    fn select(app: &mut App, page: usize, key: &str) {
        for _ in 0..page {
            key_(app, KeyCode::Down);
        }
        key_(app, KeyCode::Enter);
        for _ in 0..60 {
            if app.settings.as_ref().unwrap().selected_key() == Some(key) {
                return;
            }
            key_(app, KeyCode::Down);
        }
        panic!("no row {key}");
    }

    fn key_(app: &mut App, code: KeyCode) {
        key(app, code);
    }

    fn ctrl_s(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    fn file(app: &App) -> String {
        std::fs::read_to_string(app.paths.config_file()).unwrap()
    }

    fn pending(app: &App) -> usize {
        app.settings.as_ref().unwrap().pending().len()
    }

    fn last(app: &App) -> (Level, String) {
        let n = app.notifications.last().expect("a notification");
        (n.level, n.text.clone())
    }

    #[tokio::test]
    async fn toggle_and_apply() {
        let (mut app, dir) = test_app("settings-toggle");
        setup(&mut app, NO_LSP);
        select(&mut app, 2, "editor.auto_close");
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(pending(&app), 1);
        assert!(app.config.editor.auto_close, "staged, not applied");
        ctrl_s(&mut app);
        assert!(!app.config.editor.auto_close);
        let text = file(&app);
        assert!(text.contains("auto_close = false") && text.contains("enabled = false"), "{text}");
        assert!(app.settings.is_some());
        assert_eq!(pending(&app), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn toggling_back_clears_the_edit() {
        let (mut app, dir) = test_app("settings-toggle-back");
        setup(&mut app, NO_LSP);
        select(&mut app, 2, "editor.auto_close");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert_eq!(pending(&app), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn int_dialog_validates_then_ok_applies() {
        let (mut app, dir) = test_app("settings-int");
        setup(&mut app, NO_LSP);
        select(&mut app, 2, "editor.scrolloff");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::SettingValue("editor.scrolloff"));
        // The input is prefilled with 3: replace it.
        key(&mut app, KeyCode::Backspace);
        type_text(&mut app, "x");
        key(&mut app, KeyCode::Enter);
        let d = app.dialog.as_ref().expect("reopened");
        assert!(d.message.starts_with("Not a number in range. "), "{}", d.message);
        for _ in 0..3 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "9");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(pending(&app), 1);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Enter);
        assert!(app.settings.is_none());
        assert_eq!(app.config.editor.scrolloff, 9);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn restart_key_is_named_after_apply() {
        let (mut app, dir) = test_app("settings-restart");
        setup(&mut app, NO_LSP);
        select(&mut app, 0, "ui.left_width");
        key(&mut app, KeyCode::Enter);
        for _ in 0..4 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "41");
        key(&mut app, KeyCode::Enter);
        ctrl_s(&mut app);
        let (level, text) = last(&app);
        assert_eq!(level, Level::Warn);
        assert!(text.contains("ui.left_width"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn choice_cycles_and_other_takes_text() {
        let (mut app, dir) = test_app("settings-choice");
        setup(&mut app, NO_LSP);
        select(&mut app, 4, "editor.autosave");
        let before = app.settings.as_ref().unwrap().value("editor.autosave");
        key(&mut app, KeyCode::Right);
        let after = app.settings.as_ref().unwrap().value("editor.autosave");
        assert_ne!(before, after);
        key(&mut app, KeyCode::Left);
        assert_eq!(app.settings.as_ref().unwrap().value("editor.autosave"), before);
        assert_eq!(pending(&app), 0);
        key(&mut app, KeyCode::Right);
        ctrl_s(&mut app);
        assert_eq!(app.config.editor.autosave, crate::config::Autosave::Focus);

        key(&mut app, KeyCode::Esc);
        assert!(app.settings.is_none());
        setup(&mut app, NO_LSP);
        select(&mut app, 0, "ui.theme");
        key(&mut app, KeyCode::Enter);
        for _ in 0..5 {
            key(&mut app, KeyCode::Down);
        }
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.dialog.as_ref().unwrap().kind, crate::ui::dialog::DialogKind::Input { .. }));
        for _ in 0..20 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "mine.toml");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.settings.as_ref().unwrap().pending(), &[("ui.theme", Edit::Set(Value::Str("mine.toml".into())))]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn list_is_split_on_commas() {
        let (mut app, dir) = test_app("settings-list");
        setup(&mut app, NO_LSP);
        select(&mut app, 3, "editor.soft_wrap_files");
        key(&mut app, KeyCode::Enter);
        for _ in 0..80 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "*.md, , *.txt ");
        key(&mut app, KeyCode::Enter);
        ctrl_s(&mut app);
        assert_eq!(app.config.editor.soft_wrap_files, ["*.md", "*.txt"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn delete_resets_to_the_default() {
        let (mut app, dir) = test_app("settings-reset");
        setup(&mut app, &format!("{NO_LSP}[editor]\nscrolloff = 9\n"));
        assert_eq!(app.config.editor.scrolloff, 9);
        select(&mut app, 2, "editor.scrolloff");
        key(&mut app, KeyCode::Delete);
        assert_eq!(app.settings.as_ref().unwrap().value("editor.scrolloff"), Value::Int(3));
        ctrl_s(&mut app);
        assert!(!file(&app).contains("scrolloff"), "{}", file(&app));
        assert_eq!(app.config.editor.scrolloff, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn esc_with_edits_asks_first() {
        let (mut app, dir) = test_app("settings-discard");
        setup(&mut app, NO_LSP);
        select(&mut app, 2, "editor.auto_close");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::SettingsDiscard);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Enter);
        assert!(app.settings.is_some());
        assert_eq!(pending(&app), 1);
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Enter);
        assert!(app.settings.is_none());
        assert_eq!(file(&app), NO_LSP);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn invalid_file_keeps_the_edit() {
        let (mut app, dir) = test_app("settings-invalid");
        setup(&mut app, NO_LSP);
        select(&mut app, 2, "editor.auto_close");
        key(&mut app, KeyCode::Enter);
        std::fs::write(app.paths.config_file(), "[editor\n").unwrap();
        ctrl_s(&mut app);
        assert_eq!(last(&app).0, Level::Error);
        assert_eq!(file(&app), "[editor\n");
        assert!(app.settings.is_some());
        assert_eq!(pending(&app), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn external_row_refuses_while_edits_are_pending() {
        let (mut app, dir) = test_app("settings-external-pending");
        setup(&mut app, NO_LSP);
        select(&mut app, 2, "editor.auto_close");
        key(&mut app, KeyCode::Enter);
        app.finish_settings(SettingsResult::OpenConfigFile);
        assert!(app.settings.is_some());
        assert_eq!(last(&app).0, Level::Warn);
        let _ = std::fs::remove_dir_all(dir);
    }
}
