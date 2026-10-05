//! Hot reload of `config.toml`, `keymap.toml` and the theme files.
//!
//! The config watcher (`config::watch`) sends `AppMsg::ConfigChanged` with
//! the kinds of files that changed. Each file reloads on its own: its text
//! is compared with what it held when last processed (repeats, `touch` and
//! jetl's own writes are silent), a file that cannot be read or parsed
//! keeps the current value and reports the error, and a valid one replaces
//! it. `apply_config` swaps `App::config` and applies the keys that take
//! effect at once (`config::reload` classes); keys that need a restart are
//! named in the notification.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{App, Ctx};
use crate::config::reload::{self, ConfigDiff, ReloadClass};
use crate::config::theme::{OnBadFile, Theme};
use crate::config::watch::{self, ConfigFile, ConfigWatcher};
use crate::config::{ColorMode, Config, FileText};
use crate::dispatch::Mode;
use crate::event::Level;

/// What the theme was last built from: the settings that pick it and the
/// text of each of its files.
#[derive(Debug, Clone, PartialEq)]
struct ThemeInputs {
    theme: String,
    colors: ColorMode,
    files: Vec<(PathBuf, FileText)>,
}

impl ThemeInputs {
    fn read(config: &Config, config_dir: &Path) -> Self {
        let files = Theme::sources(config, config_dir).into_iter().map(|p| (p.clone(), FileText::read(&p))).collect();
        Self { theme: config.ui.theme.clone(), colors: config.ui.colors, files }
    }
}

/// The config watcher and what each file held when last processed.
#[derive(Debug, Default)]
pub(crate) struct ConfigReload {
    /// `None` when it could not start (changes then apply at the next start).
    watcher: Option<ConfigWatcher>,
    config_text: FileText,
    keymap_text: FileText,
    theme_inputs: Option<ThemeInputs>,
}

/// What one batch reloaded and the problems it found.
#[derive(Default)]
struct Report {
    /// File names, for the summary.
    reloaded: Vec<String>,
    errors: Vec<String>,
}

/// What Open Settings writes into a `config.toml` it creates.
pub(super) const SETTINGS_STUB: &str = "# jetl settings; every key and its default: docs/CONFIG.md\n";

impl App {
    /// Remember what the config files hold now and start watching them.
    /// `keymap_text` is the text the keymap was just loaded from; the
    /// config and theme files are read again here. The config directory is
    /// created if missing, so that it can be watched.
    pub(super) fn start_config_reload(&mut self, keymap_text: FileText) {
        let r = &mut self.config_reload;
        r.keymap_text = keymap_text;
        r.config_text = FileText::read(&self.paths.config_file());
        r.theme_inputs = Some(ThemeInputs::read(&self.config, &self.paths.config_dir));
        if let Err(e) = std::fs::create_dir_all(&self.paths.config_dir) {
            tracing::warn!("cannot create {}: {e}", self.paths.config_dir.display());
        }
        match ConfigWatcher::start(watch::targets(&self.paths, &self.config), self.bus_tx.clone()) {
            Ok(w) => self.config_reload.watcher = Some(w),
            Err(e) => tracing::warn!("config watcher unavailable ({e:#}); config changes apply at the next start"),
        }
    }

    /// Reload the files of `files` whose text changed, then notify: one
    /// summary first, then each error, so the status bar ends on an error.
    pub(super) fn config_changed(&mut self, files: Vec<ConfigFile>) {
        let mut report = Report::default();
        let mut force_theme = false;
        let mut diff = ConfigDiff::default();
        if files.contains(&ConfigFile::Config) {
            if let Some(d) = self.reload_config_file(&mut report) {
                force_theme = d.has("ui.theme") || d.has("ui.colors");
                diff = d;
            }
            // Also retries directories that could not be watched before.
            self.retarget_config_watch();
        }
        if files.contains(&ConfigFile::Keymap) {
            self.reload_keymap_file(&mut report);
        }
        if files.contains(&ConfigFile::Theme) || force_theme {
            self.reload_theme(force_theme, &mut report);
        }
        if !report.reloaded.is_empty() {
            let (level, text) = reload_message(&report.reloaded, &diff);
            self.notify(level, text);
        }
        for e in report.errors {
            self.notify(Level::Error, e);
        }
        self.dirty_frame = true;
    }

    /// `config.toml`, when its text changed: the diff it applied, or `None`
    /// (unchanged, or rejected with an error in `report`).
    fn reload_config_file(&mut self, report: &mut Report) -> Option<ConfigDiff> {
        let file = self.paths.config_file();
        let text = FileText::read(&file);
        if text == self.config_reload.config_text {
            return None;
        }
        let parsed = Config::from_file_text(&text, &file);
        self.config_reload.config_text = text;
        match parsed {
            Ok(new) => {
                report.reloaded.push("config.toml".into());
                Some(self.apply_config(new))
            }
            Err(e) => {
                report.errors.push(format!("{e}; keeping the current settings"));
                None
            }
        }
    }

    /// `keymap.toml`, when its text changed.
    fn reload_keymap_file(&mut self, report: &mut Report) {
        let file = self.paths.keymap_file();
        let text = FileText::read(&file);
        if text == self.config_reload.keymap_text {
            return;
        }
        let loaded = crate::config::keymap::Keymap::from_file_text(&text, &file);
        self.config_reload.keymap_text = text;
        match loaded {
            Ok((keymap, errors)) => {
                self.keymap = keymap;
                self.dispatcher.reset();
                report.errors.extend(errors);
                report.reloaded.push("keymap.toml".into());
            }
            Err(e) => report.errors.push(format!("{e}; keeping the current keymap")),
        }
    }

    /// The theme, when one of its files changed, or always with `force`
    /// (`ui.theme` or `ui.colors` changed). A document that fails as a
    /// whole keeps the current theme.
    fn reload_theme(&mut self, force: bool, report: &mut Report) {
        let inputs = ThemeInputs::read(&self.config, &self.paths.config_dir);
        let old = self.config_reload.theme_inputs.replace(inputs.clone());
        if !force && old.as_ref() == Some(&inputs) {
            return;
        }
        let names: Vec<String> = inputs
            .files
            .iter()
            .filter(|f| !old.as_ref().is_some_and(|o| o.files.contains(f)))
            .map(|(p, _)| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()))
            .collect();
        let colorterm = crate::config::color::colorterm_env();
        let r = Theme::load_report(&self.config, &self.paths.config_dir, colorterm.as_deref(), OnBadFile::Reject);
        report.errors.extend(r.messages);
        if r.rejected {
            return;
        }
        if r.theme != self.theme {
            self.theme = r.theme;
            self.editor.restyle(&self.theme);
            self.git_panel.theme_changed();
        }
        report.reloaded.extend(names);
    }

    /// Swap in `new` and apply what changed at once (the table in
    /// docs/CONFIG.md); returns what changed. Keys of other classes are
    /// stored and used when they are next read.
    pub(crate) fn apply_config(&mut self, new: Config) -> ConfigDiff {
        let diff = reload::diff(&self.config, &new);
        self.config = new;
        if diff.is_empty() {
            return diff;
        }
        self.editor.apply_config(&self.config);
        self.git.tab_width = self.config.editor.tab_width;
        self.git_panel.set_log_page(self.config.git.log_page);
        self.git_panel.set_log_all(self.config.git.log_all);
        if diff.has("git.log_all") && self.git_panel.log_loaded() {
            let mut cx = ctx!(self);
            self.git_panel.request_log(&mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
        }
        self.dispatcher.double_tap = Duration::from_millis(self.config.ui.double_click_ms);
        if diff.has("keymap.mode") {
            let modal = self.config.keymap.mode == "vim";
            self.dispatcher.modal = modal;
            self.dispatcher.reset();
            self.mode = if modal { Mode::Normal } else { Mode::Insert };
        }
        self.terminal.set_scrollback(self.config.terminal.scrollback);
        if diff.has_section("clipboard") {
            // Detected again, from the new commands, on next use.
            self.clipboard.tools = None;
        }
        if diff.has("files.watch") {
            self.start_watcher();
        }
        if diff.has("lsp.enabled") {
            self.lsp_restart();
        }
        if diff.has_section("local_history") {
            self.history.set_settings(self.config.local_history.clone());
            if diff.has("local_history.keep_days") || diff.has("local_history.max_total_mb") {
                self.history.prune();
            }
        }
        tracing::info!("config reloaded: {:?}", diff.changed);
        diff
    }

    /// Watch the files the current config names (the `ui.theme` file may
    /// have moved).
    fn retarget_config_watch(&mut self) {
        let targets = watch::targets(&self.paths, &self.config);
        if let Some(w) = self.config_reload.watcher.as_mut() {
            w.set_targets(targets);
        }
    }

    /// Open Settings created `config.toml` with `text`: its own write is
    /// not a change.
    pub(super) fn settings_created(&mut self, text: &str) {
        self.config_reload.config_text = FileText::Text(text.to_string());
    }
}

/// "a", "a and b", "a, b and c".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The summary of a batch: "Reloaded config.toml and keymap.toml", a
/// warning naming the changed keys that need a restart of jetl or of the
/// language servers.
fn reload_message(names: &[String], diff: &ConfigDiff) -> (Level, String) {
    let mut text = format!("Reloaded {}", join_names(names));
    let restart = diff.keys(ReloadClass::Restart);
    let lsp = diff.keys(ReloadClass::LspRestart);
    if !restart.is_empty() {
        text.push_str(&format!("; restart jetl to apply {}", restart.join(", ")));
    }
    if !lsp.is_empty() {
        text.push_str(&format!("; Restart Language Servers (Help menu) to apply {}", lsp.join(", ")));
    }
    let level = if restart.is_empty() && lsp.is_empty() { Level::Info } else { Level::Warn };
    (level, text)
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;
    use crate::actions::{Action, AppAction, EditorAction};
    use crate::app::tests::test_app;
    use crate::config::keymap::parse_chord;
    use crate::config::theme::LIGHT;
    use crate::event::AppMsg;

    /// Every config.toml a test writes keeps the language servers off: a
    /// reload to the default would start real servers.
    const NO_LSP: &str = "[lsp]\nenabled = false\n";

    fn write(app: &App, name: &str, text: &str) {
        std::fs::write(app.paths.config_dir.join(name), text).unwrap();
    }

    fn reload(app: &mut App, files: &[ConfigFile]) {
        app.handle_msg(AppMsg::ConfigChanged { files: files.to_vec() });
    }

    fn last(app: &App) -> (Level, String) {
        let n = app.notifications.last().expect("a notification");
        (n.level, n.text.clone())
    }

    fn bound(app: &App, keys: &str) -> Option<Action> {
        app.keymap.table("global").unwrap().bindings.get(&parse_chord(keys).unwrap()).cloned()
    }

    #[tokio::test]
    async fn config_reload_applies_live_settings() {
        let (mut app, dir) = test_app("reload-live");
        write(&app, "config.toml", &format!("[editor]\nscrolloff = 7\n[ui]\ndouble_click_ms = 250\n[keymap]\nmode = \"vim\"\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.config.editor.scrolloff, 7);
        assert_eq!(app.editor.scrolloff(), 7, "the editor took it");
        assert_eq!(app.dispatcher.double_tap, Duration::from_millis(250));
        assert!(app.dispatcher.modal);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(last(&app), (Level::Info, "Reloaded config.toml".into()));
        // The same text again (a duplicate event, a touch) is silent.
        let count = app.notifications.len();
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.notifications.len(), count);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restart_keys_are_named_once() {
        let (mut app, dir) = test_app("reload-restart");
        let base = "[ui]\nleft_width = 50\n[lsp]\nenabled = false\nstartup_timeout_ms = 1\n";
        write(&app, "config.toml", base);
        reload(&mut app, &[ConfigFile::Config]);
        let (level, text) = last(&app);
        assert_eq!(level, Level::Warn);
        assert!(text.contains("restart jetl to apply ui.left_width"), "{text}");
        assert!(text.contains("Restart Language Servers (Help menu) to apply lsp.startup_timeout_ms"), "{text}");
        write(&app, "config.toml", &format!("{base}[editor]\nscrolloff = 5\n"));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(last(&app), (Level::Info, "Reloaded config.toml".into()));
        assert_eq!(app.config.editor.scrolloff, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn broken_config_keeps_the_old_one() {
        let (mut app, dir) = test_app("reload-broken-config");
        write(&app, "config.toml", &format!("[editor]\nscrolloff = 7\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        write(&app, "config.toml", &format!("[editor\nscrolloff = 1\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.config.editor.scrolloff, 7);
        let (level, text) = last(&app);
        assert_eq!(level, Level::Error);
        assert!(text.contains("config.toml") && text.ends_with("keeping the current settings"), "{text}");
        // A wrong type rejects the file too.
        write(&app, "config.toml", &format!("[editor]\nscrolloff = \"x\"\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.config.editor.scrolloff, 7);
        assert_eq!(last(&app).0, Level::Error);
        write(&app, "config.toml", &format!("[editor]\nscrolloff = 1\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.config.editor.scrolloff, 1);
        assert_eq!(last(&app), (Level::Info, "Reloaded config.toml".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keymap_reload_and_errors() {
        let (mut app, dir) = test_app("reload-keymap");
        let about = Some(Action::App(AppAction::About));
        assert_ne!(bound(&app, "f2"), about);
        write(&app, "keymap.toml", "[global]\n\"f2\" = \"app.about\"\n");
        reload(&mut app, &[ConfigFile::Keymap]);
        assert_eq!(bound(&app, "f2"), about);
        assert_eq!(last(&app), (Level::Info, "Reloaded keymap.toml".into()));
        write(&app, "keymap.toml", "[global\n");
        reload(&mut app, &[ConfigFile::Keymap]);
        assert_eq!(bound(&app, "f2"), about, "the current keymap stays");
        let (level, text) = last(&app);
        assert_eq!(level, Level::Error);
        assert!(text.ends_with("keeping the current keymap"), "{text}");
        write(&app, "keymap.toml", "[global]\n\"f3\" = \"no.such\"\n\"f4\" = \"app.about\"\n");
        let count = app.notifications.len();
        reload(&mut app, &[ConfigFile::Keymap]);
        assert_eq!(bound(&app, "f4"), about);
        assert_ne!(bound(&app, "f2"), about, "back to the default");
        let new: Vec<_> = app.notifications[count..].iter().map(|n| (n.level, n.text.clone())).collect();
        assert_eq!(new.len(), 2, "{new:?}");
        assert_eq!(new[0], (Level::Info, "Reloaded keymap.toml".into()));
        assert_eq!(new[1].0, Level::Error);
        assert!(new[1].1.ends_with("(entry ignored)"), "{new:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn files_reload_independently() {
        let (mut app, dir) = test_app("reload-independent");
        let ctrl_q = bound(&app, "ctrl-q");
        write(&app, "keymap.toml", "[global]\n\"ctrl-q\" = \"none\"\n[[[");
        write(&app, "config.toml", &format!("[editor]\nscrolloff = 6\n{NO_LSP}"));
        let count = app.notifications.len();
        reload(&mut app, &[ConfigFile::Config, ConfigFile::Keymap]);
        assert_eq!(app.config.editor.scrolloff, 6);
        assert_eq!(bound(&app, "ctrl-q"), ctrl_q);
        let new: Vec<_> = app.notifications[count..].iter().map(|n| (n.level, n.text.clone())).collect();
        assert_eq!(new.len(), 2, "{new:?}");
        assert_eq!(new[0], (Level::Info, "Reloaded config.toml".into()));
        assert_eq!(new[1].0, Level::Error);
        assert!(new[1].1.contains("keymap.toml") && new[1].1.ends_with("keeping the current keymap"), "{new:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deleted_keymap_restores_defaults() {
        let (mut app, dir) = test_app("reload-deleted-keymap");
        write(&app, "keymap.toml", "[global]\n\"ctrl-q\" = \"none\"\n");
        reload(&mut app, &[ConfigFile::Keymap]);
        assert_eq!(bound(&app, "ctrl-q"), None);
        std::fs::remove_file(app.paths.keymap_file()).unwrap();
        reload(&mut app, &[ConfigFile::Keymap]);
        assert_eq!(bound(&app, "ctrl-q"), Some(Action::App(AppAction::Quit)));
        assert_eq!(last(&app), (Level::Info, "Reloaded keymap.toml".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn config_theme_change_reloads_theme() {
        let (mut app, dir) = test_app("reload-ui-theme");
        write(&app, "config.toml", &format!("[ui]\ntheme = \"light\"\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.theme, Theme::builtin(&LIGHT, false));
        assert_eq!(last(&app), (Level::Info, "Reloaded config.toml".into()));
        write(&app, "config.toml", &format!("[ui]\ntheme = \"nope\"\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.theme, Theme::builtin(&LIGHT, false));
        let (level, text) = last(&app);
        assert_eq!(level, Level::Error);
        assert!(text.contains("unknown theme") && text.ends_with("keeping the current theme"), "{text}");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let mine = other.join("mine.toml");
        std::fs::write(&mine, "bg = \"#010203\"\n").unwrap();
        write(&app, "config.toml", &format!("[ui]\ntheme = {:?}\n{NO_LSP}", mine.display().to_string()));
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.theme.bg, Color::Rgb(1, 2, 3));
        if let Some(w) = &app.config_reload.watcher {
            assert!(w.dirs().contains(&other), "{:?}", w.dirs());
        }
        // The theme file itself reloads too.
        std::fs::write(&mine, "bg = \"#040506\"\n").unwrap();
        reload(&mut app, &[ConfigFile::Theme]);
        assert_eq!(app.theme.bg, Color::Rgb(4, 5, 6));
        assert_eq!(last(&app), (Level::Info, "Reloaded mine.toml".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn broken_theme_toml_keeps_the_old_theme() {
        let (mut app, dir) = test_app("reload-broken-theme");
        write(&app, "theme.toml", "selection_bg = \"#123456\"\n");
        reload(&mut app, &[ConfigFile::Theme]);
        assert_eq!(app.theme.selection_bg, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(last(&app), (Level::Info, "Reloaded theme.toml".into()));
        write(&app, "theme.toml", "[[[");
        let count = app.notifications.len();
        reload(&mut app, &[ConfigFile::Theme]);
        assert_eq!(app.theme.selection_bg, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(app.notifications.len(), count + 1, "only the error");
        let (level, text) = last(&app);
        assert_eq!(level, Level::Error);
        assert!(text.contains("theme.toml") && text.ends_with("keeping the current theme"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn theme_reload_restyles_buffers() {
        let (mut app, dir) = test_app("reload-restyle");
        let path = app.workspace.root.join("a.rs");
        app.editor.open_text(path, "fn main() {}\n", false, &app.theme);
        write(&app, "theme.toml", "[syntax]\nkeyword = \"#010203\"\n");
        reload(&mut app, &[ConfigFile::Theme]);
        let b = app.editor.active_buffer_mut().unwrap();
        let style = b.syntax.as_mut().unwrap().highlights(&b.rope, 0, 1).style_at(0).unwrap();
        assert_eq!(style.fg, Some(Color::Rgb(1, 2, 3)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn soft_wrap_default_keeps_each_views_wrap() {
        let (mut app, dir) = test_app("reload-wrap");
        let root = app.workspace.root.clone();
        app.editor.open_text(root.join("a.rs"), "fn main() {}\n", false, &app.theme);
        app.perform(Action::Editor(EditorAction::SplitRight));
        write(&app, "config.toml", &format!("[editor]\nsoft_wrap = true\n{NO_LSP}"));
        reload(&mut app, &[ConfigFile::Config]);
        assert!(!app.editor.active_buffer().unwrap().view.wrap.on);
        app.perform(Action::Editor(EditorAction::NextGroup));
        assert!(!app.editor.active_buffer().unwrap().view.wrap.on, "the other group's view too");
        app.editor.check_invariants();
        app.editor.open_text(root.join("b.rs"), "fn b() {}\n", false, &app.theme);
        assert!(app.editor.active_buffer().unwrap().view.wrap.on, "a file opened now wraps");
        app.editor.check_invariants();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn open_settings_write_is_silent() {
        let (mut app, dir) = test_app("reload-open-settings");
        app.perform(Action::App(AppAction::OpenSettings));
        assert_eq!(std::fs::read_to_string(app.paths.config_file()).unwrap(), SETTINGS_STUB);
        let count = app.notifications.len();
        reload(&mut app, &[ConfigFile::Config]);
        assert_eq!(app.notifications.len(), count);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_message_wording() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let none = ConfigDiff::default();
        assert_eq!(reload_message(&names(&["config.toml"]), &none), (Level::Info, "Reloaded config.toml".into()));
        assert_eq!(
            reload_message(&names(&["config.toml", "keymap.toml"]), &none),
            (Level::Info, "Reloaded config.toml and keymap.toml".into())
        );
        assert_eq!(
            reload_message(&names(&["config.toml", "keymap.toml", "theme.toml"]), &none).1,
            "Reloaded config.toml, keymap.toml and theme.toml"
        );
        let diff = ConfigDiff {
            changed: vec![
                ("ui.left_width", ReloadClass::Restart),
                ("ui.bottom_height", ReloadClass::Restart),
                ("lsp.servers", ReloadClass::LspRestart),
                ("editor.scrolloff", ReloadClass::Live),
            ],
        };
        assert_eq!(
            reload_message(&names(&["config.toml"]), &diff),
            (
                Level::Warn,
                "Reloaded config.toml; restart jetl to apply ui.left_width, ui.bottom_height; \
                 Restart Language Servers (Help menu) to apply lsp.servers"
                    .into()
            )
        );
        let lsp_only = ConfigDiff { changed: vec![("lsp.request_timeout_ms", ReloadClass::LspRestart)] };
        assert_eq!(
            reload_message(&names(&["config.toml"]), &lsp_only).1,
            "Reloaded config.toml; Restart Language Servers (Help menu) to apply lsp.request_timeout_ms"
        );
    }
}
