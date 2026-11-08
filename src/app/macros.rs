//! Keyboard macros in the app: the recorder hook around `App::perform`, the
//! playback loop, Save Macro As and the cached list of saved macros
//! (`macros.rs` owns the format and the file).

use super::App;
use crate::actions::{Action, EditorAction};
use crate::event::{AppMsg, Level};
use crate::macros::{MacroOp, NamedMacro, Stopped};
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::palette::{ActionEntry, Source};

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl App {
    /// Read the macros file off the main thread (`main.rs` at startup; tests
    /// never call it). From then on Find Action re-reads it when it opens.
    pub fn load_macros(&mut self) {
        self.macros_watch = true;
        self.macros_request(None, None);
    }

    pub(super) fn macros_request(&mut self, op: Option<MacroOp>, done: Option<(Level, String)>) {
        let (file, tx) = (self.paths.macros_file(), self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let (version, macros, error) = crate::macros::update(&file, op.as_ref());
            let _ = tx.send(AppMsg::Macros { version, macros, error, done });
        });
    }

    /// `done` applies before the version check: a reply overtaken by a later
    /// one still reports its own success.
    pub(super) fn macros_loaded(&mut self, version: u64, macros: Vec<NamedMacro>, error: Option<String>, done: Option<(Level, String)>) {
        match (error, done) {
            (Some(e), _) => self.notify(Level::Warn, e),
            (None, Some((level, text))) => self.notify(level, text),
            (None, None) => {}
        }
        if version <= self.macros_version {
            tracing::debug!("dropping stale macros");
            return;
        }
        self.macros_version = version;
        let changed = self.named_macros != macros;
        self.named_macros = macros;
        if changed && self.palette.as_ref().is_some_and(|p| p.wants(Source::Actions)) {
            let actions = self.palette_actions();
            if let Some(p) = &mut self.palette {
                p.set_actions(actions);
            }
        }
    }

    /// The Find Action rows of the saved macros, sorted by name.
    pub(super) fn macro_palette_entries(&self) -> Vec<ActionEntry> {
        let mut names: Vec<&str> = self.named_macros.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        names
            .into_iter()
            .map(|n| ActionEntry {
                id: format!("macro.{n}").into(),
                desc: format!("Play macro '{n}'").into(),
                action: Action::Editor(EditorAction::MacroPlayNamed(n.to_string())),
                shortcut: None,
            })
            .collect()
    }

    /// The four macro actions (`App::perform_action`).
    pub(super) fn macro_action(&mut self, a: EditorAction) {
        match a {
            EditorAction::MacroRecord => self.macro_toggle_record(),
            EditorAction::MacroPlay => self.macro_play_last(),
            EditorAction::MacroSaveAs => self.macro_save_as(),
            EditorAction::MacroPlayNamed(name) => self.macro_play_named(&name),
            _ => {}
        }
    }

    fn macro_toggle_record(&mut self) {
        if !self.macros.is_recording() {
            self.macros.start();
            let key = Action::Editor(EditorAction::MacroRecord);
            let text = match self.keymap.shortcut_for(&key, key.contexts()) {
                Some(s) => format!("Recording macro ({s} stops)"),
                None => "Recording macro".to_string(),
            };
            self.notify(Level::Info, text);
            return;
        }
        let Stopped { steps, skipped } = self.macros.stop();
        if steps == 0 {
            self.notify(Level::Info, "Macro recording stopped: nothing recorded");
            return;
        }
        let mut text = format!("Macro recorded: {steps} step{}", plural(steps));
        if skipped > 0 {
            text.push_str(&format!("; {skipped} left out (dialogs, popups, find bar text)"));
        }
        self.notify(Level::Info, text);
    }

    /// Whether `a`, just performed from the keyboard, belongs in the recording
    /// (`App::perform` decided it does): an action that left an overlay open
    /// is counted and left out.
    pub(super) fn macro_note(&mut self, a: Action) {
        if self.editor_overlay_open() {
            self.macros.skip();
        } else {
            self.macro_push(a);
        }
    }

    fn macro_push(&mut self, a: Action) {
        if !self.macros.push(a) {
            self.notify(Level::Warn, format!("Macro recording stopped at {} steps", crate::macros::MAX_STEPS));
        }
    }

    /// After the find bar's raw key: record the action it ran, or count the
    /// key as left out when it edited the bar's text.
    pub(super) fn macro_find_key(&mut self, used: bool) {
        let ran = self.editor.take_find_action();
        if !self.macros.is_recording() || !used {
            return;
        }
        match ran {
            Some(a) => {
                let a = Action::Editor(a);
                if self.macros.wants(&a) {
                    self.macro_push(a);
                }
            }
            None => self.macros.skip(),
        }
    }

    /// A bracketed paste into the focused editor is a step; a paste into any
    /// other field is not.
    pub(super) fn macro_note_paste(&mut self, text: &str) {
        let a = Action::Editor(EditorAction::InsertText(text.to_string()));
        if !self.macros.wants(&a) || self.settings.is_some() || self.run_configs.is_some() || self.switcher.is_some() {
            return;
        }
        if matches!(self.paste_target(), Some(super::clipboard::PasteTarget::Editor(_))) {
            self.macro_push(a);
        }
    }

    fn macro_play_last(&mut self) {
        if self.macros.is_recording() {
            self.notify(Level::Info, "Stop recording before playing a macro");
            return;
        }
        let Some(steps) = self.macros.last().map(<[Action]>::to_vec) else {
            self.notify(Level::Info, "No macro recorded yet");
            return;
        };
        self.macro_play(steps);
    }

    fn macro_play_named(&mut self, name: &str) {
        if self.macros.is_recording() {
            self.notify(Level::Info, "Stop recording before playing a macro");
            return;
        }
        let Some(saved) = self.named_macros.iter().find(|m| m.name == name) else {
            self.notify(Level::Warn, format!("No macro named '{name}'"));
            return;
        };
        match crate::macros::from_saved(&saved.steps) {
            Ok(steps) => self.macro_play(steps),
            Err(id) => self.notify(Level::Warn, format!("Macro '{name}' uses an action this build does not know: {id}")),
        }
    }

    /// Run the steps one after another through `perform`, then fold each
    /// buffer's new undo steps into one. Stops before a step when an overlay
    /// is open; does not wait for asynchronous results.
    fn macro_play(&mut self, steps: Vec<Action>) {
        if self.macros.playing {
            return;
        }
        let marks = self.editor.undo_marks();
        self.macros.playing = true;
        let total = steps.len();
        let mut stopped_at = None;
        for (i, a) in steps.into_iter().enumerate() {
            if self.editor_overlay_open() || self.should_quit {
                stopped_at = Some(i);
                break;
            }
            if a == Action::Editor(EditorAction::Paste) && !self.clipboard.register.is_empty() {
                let text = self.clipboard.register.clone();
                self.handle_paste(text);
            } else {
                self.perform(a);
            }
        }
        self.macros.playing = false;
        self.editor.squash_undo(&marks);
        if let Some(i) = stopped_at {
            self.notify(Level::Warn, format!("Macro stopped after {i} of {total} steps: a dialog or popup opened"));
        }
        self.dirty_frame = true;
    }

    fn macro_save_as(&mut self) {
        if self.macros.is_recording() {
            self.notify(Level::Info, "Stop recording before saving a macro");
        } else if self.macros.last().is_none() {
            self.notify(Level::Info, "No macro recorded yet");
        } else {
            self.dialog = Some(Dialog::input(DialogId::MacroSaveAs, "Save Macro As", "Name of the macro:", "", "Save"));
        }
    }

    /// The answer of the Save Macro As dialog.
    pub(super) fn finish_macro_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        if *id != DialogId::MacroSaveAs {
            return false;
        }
        let DialogResult::Text(text) = res else { return true };
        let name = text.trim().to_string();
        if name.is_empty() {
            return true;
        }
        let Some(last) = self.macros.last() else {
            self.notify(Level::Info, "No macro recorded yet");
            return true;
        };
        let (steps, left_out) = crate::macros::to_saved(last);
        if steps.is_empty() {
            self.notify(Level::Warn, "Nothing in this macro can be saved");
            return true;
        }
        let done = if left_out > 0 {
            (Level::Warn, format!("Saved macro '{name}'; {left_out} step{} without an action id left out", plural(left_out)))
        } else if self.named_macros.iter().any(|m| m.name == name) {
            (Level::Info, format!("Replaced macro '{name}'"))
        } else {
            (Level::Info, format!("Saved macro '{name}'"))
        };
        self.macros_request(Some(MacroOp::Put(NamedMacro { name, steps })), Some(done));
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::actions::Motion;
    use crate::app::tests::{load, test_app};
    use crate::config::keymap::KeyCombo;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    const CS: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::SHIFT);

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.handle_key(KeyEvent::new(code, mods));
    }

    fn key(app: &mut App, code: KeyCode) {
        press(app, code, KeyModifiers::NONE);
    }

    fn typ(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    /// Ctrl+Shift+F4.
    fn rec(app: &mut App) {
        press(app, KeyCode::F(4), CS);
    }

    /// Ctrl+Shift+F5.
    fn play(app: &mut App) {
        press(app, KeyCode::F(5), CS);
    }

    fn text(app: &App) -> String {
        app.editor.active_buffer().unwrap().text()
    }

    fn last_toast(app: &App) -> String {
        app.notifications.last().map(|n| n.text.clone()).unwrap_or_default()
    }

    fn last_warning(app: &App) -> Option<&str> {
        app.notifications.iter().rev().find(|n| n.level == Level::Warn).map(|n| n.text.as_str())
    }

    fn ins(c: char) -> Action {
        Action::Editor(EditorAction::InsertChar(c))
    }

    fn mv(m: Motion) -> Action {
        Action::Editor(EditorAction::Move(m, false))
    }

    fn last(app: &App) -> Vec<Action> {
        app.macros.last().expect("a last macro").to_vec()
    }

    /// Handle bus messages up to and including the first `Macros` reply.
    async fn settle_macros(app: &mut App) {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), app.bus_rx.recv()).await.expect("no macros reply").expect("bus closed");
            let macros = matches!(msg, AppMsg::Macros { .. });
            app.handle_msg(msg);
            if macros {
                return;
            }
        }
    }

    /// Whether a message the bus holds now matches (and drops them).
    fn bus_has(app: &mut App, f: impl Fn(&AppMsg) -> bool) -> bool {
        let mut found = false;
        while let Ok(m) = app.bus_rx.try_recv() {
            found |= f(&m);
        }
        found
    }

    fn answer(app: &mut App, res: DialogResult) {
        let d = app.dialog.take().expect("no dialog");
        app.finish_dialog(d.id, res);
    }

    #[tokio::test]
    async fn record_then_play_repeats_and_one_undo_reverts_one_playback() {
        let (mut app, dir) = test_app("macro-play");
        load(&mut app, "a.txt", "one\ntwo\nthree\n");
        app.editor.active_buffer_mut().unwrap().goto_line(0, 0);
        rec(&mut app);
        assert!(app.macros.is_recording());
        assert_eq!(last_toast(&app), "Recording macro (Ctrl+Shift+F4 stops)");
        typ(&mut app, "- ");
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Home);
        rec(&mut app);
        assert_eq!(last_toast(&app), "Macro recorded: 4 steps");
        assert_eq!(last(&app), [ins('-'), ins(' '), mv(Motion::Down), mv(Motion::LineStart)]);
        assert_eq!(text(&app), "- one\ntwo\nthree\n");
        // The caret is on line 1 now; two playbacks.
        play(&mut app);
        play(&mut app);
        assert_eq!(text(&app), "- one\n- two\n- three\n");
        press(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(text(&app), "- one\n- two\nthree\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn macro_keys_are_not_recorded_and_play_is_refused_while_recording() {
        let (mut app, dir) = test_app("macro-refuse");
        load(&mut app, "a.txt", "one\n");
        rec(&mut app);
        typ(&mut app, "a");
        play(&mut app);
        assert_eq!(last_toast(&app), "Stop recording before playing a macro");
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        assert_eq!(last_toast(&app), "Stop recording before saving a macro");
        assert!(app.dialog.is_none());
        rec(&mut app);
        assert_eq!(last(&app), [ins('a')]);
        assert_eq!(text(&app), "aone\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn nothing_recorded_and_no_macro_yet_are_reported() {
        let (mut app, dir) = test_app("macro-empty");
        load(&mut app, "a.txt", "one\n");
        play(&mut app);
        assert_eq!(last_toast(&app), "No macro recorded yet");
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        assert_eq!(last_toast(&app), "No macro recorded yet");
        rec(&mut app);
        rec(&mut app);
        assert_eq!(last_toast(&app), "Macro recording stopped: nothing recorded");
        assert!(app.macros.last().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn dialog_opening_actions_are_left_out() {
        let (mut app, dir) = test_app("macro-dialog");
        load(&mut app, "a.txt", "one\ntwo\nthree\n");
        rec(&mut app);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(app.dialog.is_some());
        key(&mut app, KeyCode::Char('2'));
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        key(&mut app, KeyCode::Char('x'));
        rec(&mut app);
        assert_eq!(last(&app), [ins('x')]);
        assert_eq!(last_toast(&app), "Macro recorded: 1 step; 1 left out (dialogs, popups, find bar text)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn find_bar_actions_are_recorded_and_its_text_is_not() {
        let (mut app, dir) = test_app("macro-find");
        load(&mut app, "a.txt", "one\ntwo\nthree\n");
        rec(&mut app);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(app.editor.find_focused());
        typ(&mut app, "two");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Esc);
        assert!(!app.editor.find_focused());
        key(&mut app, KeyCode::Char('X'));
        assert_eq!(app.macros.skipped_for_test(), 3);
        rec(&mut app);
        assert_eq!(
            last(&app),
            [Action::Editor(EditorAction::Find), Action::Editor(EditorAction::FindNext), Action::Editor(EditorAction::Escape), ins('X')]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bracketed_paste_into_the_editor_is_recorded() {
        let (mut app, dir) = test_app("macro-paste");
        load(&mut app, "a.txt", "one\n");
        rec(&mut app);
        app.handle_term_event(Event::Paste("ab".into()));
        assert_eq!(text(&app), "abone\n");
        rec(&mut app);
        assert_eq!(last(&app), [Action::Editor(EditorAction::InsertText("ab".into()))]);
        // Into the find bar: not recorded.
        app.macros.set_last(vec![ins('q')]);
        rec(&mut app);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        app.handle_term_event(Event::Paste("zz".into()));
        assert_eq!(text(&app), "abone\n");
        rec(&mut app);
        assert_eq!(last(&app), [Action::Editor(EditorAction::Find)]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn mouse_and_menu_by_mouse_are_not_recorded() {
        let (mut app, dir) = test_app("macro-mouse");
        load(&mut app, "a.txt", "one\n");
        rec(&mut app);
        // Outside `handle_key`: what a click on a menu item does.
        app.perform(ins('m'));
        assert_eq!(text(&app), "mone\n");
        typ(&mut app, "k");
        rec(&mut app);
        assert_eq!(last(&app), [ins('k')]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn playback_stops_when_a_dialog_opens() {
        let (mut app, dir) = test_app("macro-stop");
        load(&mut app, "a.txt", "one\n");
        app.macros.set_last(vec![Action::Editor(EditorAction::GotoLine), ins('x')]);
        play(&mut app);
        assert!(app.dialog.is_some());
        assert_eq!(text(&app), "one\n");
        assert_eq!(last_warning(&app), Some("Macro stopped after 1 of 2 steps: a dialog or popup opened"));
        assert!(!app.macros.playing);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn playback_paste_uses_the_register() {
        let (mut app, dir) = test_app("macro-register");
        load(&mut app, "a.txt", "abc\n");
        app.editor.active_buffer_mut().unwrap().goto_line(0, 0);
        // What Ctrl+C leaves (a real copy would write an OSC 52 to the test output).
        app.clipboard.register = "ab".into();
        app.macros.set_last(vec![Action::Editor(EditorAction::Paste)]);
        play(&mut app);
        assert_eq!(text(&app), "ababc\n");
        assert!(!bus_has(&mut app, |m| matches!(m, AppMsg::Paste { .. })), "no clipboard read was started");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn save_as_refuses_an_empty_result_and_a_blank_name() {
        let (mut app, dir) = test_app("macro-leftout");
        load(&mut app, "a.txt", "one\n");
        let run = Action::Run(crate::actions::RunAction::Select("t".into()));
        app.macros.set_last(vec![run.clone()]);
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        answer(&mut app, DialogResult::Text("only".into()));
        assert_eq!(last_warning(&app), Some("Nothing in this macro can be saved"));
        assert!(!app.paths.macros_file().exists());
        app.macros.set_last(vec![ins('a'), run.clone(), run]);
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        answer(&mut app, DialogResult::Text("   ".into()));
        assert!(!app.paths.macros_file().exists(), "an empty name saves nothing");
        assert!(app.notifications.iter().all(|n| !n.text.starts_with("Saved")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn save_as_reports_left_out_steps_in_the_toast() {
        let (mut app, dir) = test_app("macro-leftout2");
        load(&mut app, "a.txt", "one\n");
        let run = Action::Run(crate::actions::RunAction::Select("t".into()));
        app.macros.set_last(vec![ins('a'), run.clone(), run]);
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        answer(&mut app, DialogResult::Text("part".into()));
        settle_macros(&mut app).await;
        assert_eq!(last_warning(&app), Some("Saved macro 'part'; 2 steps without an action id left out"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn save_as_writes_the_store_and_find_action_plays_it() {
        let (mut app, dir) = test_app("macro-save");
        load(&mut app, "a.txt", "one\n");
        rec(&mut app);
        typ(&mut app, "ab");
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        rec(&mut app);
        assert_eq!(text(&app), "abone\nabone\n");
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        assert!(matches!(app.dialog.as_ref().map(|d| &d.id), Some(DialogId::MacroSaveAs)));
        answer(&mut app, DialogResult::Text("  dup ".into()));
        settle_macros(&mut app).await;
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(app.paths.macros_file()).unwrap()).unwrap();
        let want = serde_json::json!({"version":1,"macros":[{"name":"dup","steps":[{"chars":"ab"},{"action":"editor.duplicate_line"}]}]});
        assert_eq!(v, want);
        assert_eq!(last_toast(&app), "Saved macro 'dup'");
        let rows = app.palette_actions();
        let row = rows.iter().find(|a| a.id == "macro.dup").expect("a row for the saved macro");
        assert_eq!(row.desc, "Play macro 'dup'");
        assert_eq!(row.action, Action::Editor(EditorAction::MacroPlayNamed("dup".into())));
        assert_eq!(row.shortcut, None);
        // Saving under the same name replaces.
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        answer(&mut app, DialogResult::Text("dup".into()));
        settle_macros(&mut app).await;
        assert_eq!(last_toast(&app), "Replaced macro 'dup'");
        // Performing the row plays it; one Ctrl+Z reverts it.
        app.editor.active_buffer_mut().unwrap().goto_line(0, 0);
        let before = text(&app);
        app.perform(row.action.clone());
        assert_ne!(text(&app), before);
        press(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(text(&app), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn named_macro_with_an_unknown_id_is_refused() {
        let (mut app, dir) = test_app("macro-unknown");
        load(&mut app, "a.txt", "one\n");
        let file = app.paths.macros_file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"{"version":1,"macros":[{"name":"old","steps":[{"chars":"a"},{"action":"editor.no_such_thing"}]},{"name":"self","steps":[{"action":"editor.macro_play"}]}]}"#,
        )
        .unwrap();
        app.load_macros();
        settle_macros(&mut app).await;
        assert_eq!(app.named_macros.len(), 2);
        app.perform(Action::Editor(EditorAction::MacroPlayNamed("old".into())));
        assert_eq!(last_warning(&app), Some("Macro 'old' uses an action this build does not know: editor.no_such_thing"));
        assert_eq!(text(&app), "one\n", "a refused macro plays nothing");
        app.perform(Action::Editor(EditorAction::MacroPlayNamed("self".into())));
        assert_eq!(last_warning(&app), Some("Macro 'self' uses an action this build does not know: editor.macro_play"));
        app.perform(Action::Editor(EditorAction::MacroPlayNamed("gone".into())));
        assert_eq!(last_warning(&app), Some("No macro named 'gone'"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_corrupt_store_warns_and_is_not_overwritten() {
        let (mut app, dir) = test_app("macro-corrupt");
        load(&mut app, "a.txt", "one\n");
        let file = app.paths.macros_file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{bad").unwrap();
        app.macros.set_last(vec![ins('a')]);
        app.perform(Action::Editor(EditorAction::MacroSaveAs));
        answer(&mut app, DialogResult::Text("z".into()));
        settle_macros(&mut app).await;
        let w = last_warning(&app).expect("a warning");
        assert!(w.starts_with("macros ") && w.contains("not valid JSON"), "{w}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{bad");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stale_macros_reply_is_dropped() {
        let (mut app, dir) = test_app("macro-stale");
        let m = |n: &str| NamedMacro { name: n.into(), steps: vec![crate::macros::SavedStep::Chars { chars: "a".into() }] };
        app.macros_loaded(2, vec![m("new")], None, None);
        app.macros_loaded(1, vec![m("old")], None, Some((Level::Info, "Saved macro 'old'".into())));
        assert_eq!(app.named_macros, [m("new")]);
        assert_eq!(last_toast(&app), "Saved macro 'old'", "done applies to a stale reply");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_cap_stops_recording_with_a_warning() {
        let (mut app, dir) = test_app("macro-cap");
        load(&mut app, "a.txt", "");
        rec(&mut app);
        // Cheap steps: the cap is about the count, not about editing.
        app.macros.keyboard = true;
        for _ in 0..crate::macros::MAX_STEPS {
            app.perform(mv(Motion::Left));
        }
        app.macros.keyboard = false;
        assert!(!app.macros.is_recording());
        assert_eq!(last_warning(&app), Some("Macro recording stopped at 10000 steps"));
        assert_eq!(app.macros.last().map(<[Action]>::len), Some(crate::macros::MAX_STEPS));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_macro_chords_are_the_ones_the_keymap_shows() {
        let (app, dir) = test_app("macro-chord");
        let bound = |code| {
            let chord = [KeyCombo::normalize(code, CS)];
            app.keymap.table("global").and_then(|t| t.bindings.get(chord.as_slice())).cloned()
        };
        assert_eq!(bound(KeyCode::F(4)), Some(Action::Editor(EditorAction::MacroRecord)));
        assert_eq!(bound(KeyCode::F(5)), Some(Action::Editor(EditorAction::MacroPlay)));
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(all(test, feature = "lang-rust"))]
mod completion_tests {
    use super::*;
    use crate::app::lsp::tests::{incremental, open_on_server, with_fake_servers};
    use crate::app::tests::test_app;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    #[tokio::test]
    async fn playback_starts_no_completion() {
        let (mut app, dir) = test_app("macro-completion");
        let mut caps = incremental();
        caps["completionProvider"] = json!({"triggerCharacters": [".", ":"]});
        let mut servers = with_fake_servers(&mut app, caps);
        let _server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {\n    \n}\n").await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        app.macros.set_last(vec![Action::Editor(EditorAction::InsertChar('p'))]);
        app.perform(Action::Editor(EditorAction::MacroPlay));
        assert_eq!(app.editor.active_buffer().unwrap().text(), "fn main() {\n    p\n}\n");
        assert!(app.lsp.completion.request.is_none(), "playback asked for a completion");
        // The same char typed by hand does.
        app.perform(Action::Editor(EditorAction::Undo));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.lsp.completion.request.is_some(), "typing asks for a completion");
        let _ = std::fs::remove_dir_all(dir);
    }
}
