//! Configuration, paths and logging.

pub mod color;
pub mod keymap;
pub mod theme;

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::Deserialize;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "jetl").context("no home directory")?;
        let state_dir = dirs.state_dir().map(|p| p.to_path_buf()).unwrap_or_else(|| dirs.data_local_dir().to_path_buf());
        Ok(Self { config_dir: dirs.config_dir().to_path_buf(), state_dir, cache_dir: dirs.cache_dir().to_path_buf() })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn keymap_file(&self) -> PathBuf {
        self.config_dir.join("keymap.toml")
    }
    pub fn theme_file(&self) -> PathBuf {
        self.config_dir.join("theme.toml")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }
    pub fn recent_projects_file(&self) -> PathBuf {
        self.state_dir.join("recent_projects.json")
    }
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("jetl.log")
    }
    /// Crash dumps of unsaved buffers, one directory per process
    /// (`recovery.rs`).
    pub fn recovery_dir(&self) -> PathBuf {
        self.state_dir.join("recovery")
    }
}

/// Install a file logger. Returns a guard that must live until exit so the
/// non-blocking writer flushes.
pub fn init_logging(paths: &Paths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(&paths.state_dir)?;
    let file = std::fs::OpenOptions::new().create(true).append(true).open(paths.log_file())?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("jetl=info"));
    tracing_subscriber::registry().with(filter).with(fmt::layer().with_writer(writer).with_ansi(false).with_target(true)).init();
    tracing::info!("jetl {} starting", env!("CARGO_PKG_VERSION"));
    Ok(guard)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub editor: EditorConfig,
    pub ui: UiConfig,
    pub git: GitConfig,
    pub worktree: WorktreeConfig,
    pub keymap: KeymapConfig,
    pub files: FilesConfig,
    pub clipboard: ClipboardConfig,
    pub format: FormatConfig,
    pub run: crate::run::RunConfig,
    pub terminal: crate::terminal::TerminalConfig,
    pub lsp: crate::lsp::LspConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    pub tab_width: usize,
    pub max_highlight_bytes: usize,
    pub scrolloff: usize,
    pub hscrolloff: usize,
    pub auto_close: bool,
    pub highlight_occurrences: bool,
    pub trim_trailing_whitespace: bool,
    pub final_newline: bool,
    pub autosave: Autosave,
    pub autosave_idle_ms: u64,
    /// Open tabs beyond this close the least recently used clean, unpinned
    /// tab; 0 means no limit.
    pub max_tabs: usize,
    /// Soft-wrap every file when opened.
    pub soft_wrap: bool,
    /// Files soft-wrapped when opened even with `soft_wrap` off: `*.ext`
    /// matches a name ending in `.ext`, anything else a whole file name.
    pub soft_wrap_files: Vec<String>,
}

/// When dirty buffers with a path are written without Ctrl+S.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autosave {
    #[default]
    Off,
    /// When the terminal loses focus (needs focus reporting, which most
    /// terminals and tmux with `focus-events on` provide).
    Focus,
    /// After `autosave_idle_ms` without edits.
    Idle,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub left_width: u16,
    pub bottom_height: u16,
    pub double_click_ms: u64,
    pub icons: Icons,
    /// Color depth: true color or the 256-color palette.
    pub colors: ColorMode,
}

/// Color depth of the theme (`ui.colors`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum ColorMode {
    /// True color when `$COLORTERM` is `truecolor` or `24bit`, else 256 colors.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "truecolor")]
    TrueColor,
    /// Every RGB color drawn as the nearest color of the 256-color palette.
    #[serde(rename = "256")]
    Ansi256,
}

impl ColorMode {
    /// Whether RGB colors are drawn as they are, given `$COLORTERM`.
    pub fn truecolor(self, colorterm: Option<&str>) -> bool {
        match self {
            ColorMode::Auto => color::truecolor_from_env(colorterm),
            ColorMode::TrueColor => true,
            ColorMode::Ansi256 => false,
        }
    }
}

/// Symbols on the tool strip and the main menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Icons {
    /// Color emoji, two cells each (needs a color emoji font).
    Emoji,
    /// One-cell symbols that any monospace font has.
    #[default]
    Unicode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub pull_mode: String,
    pub log_all: bool,
    pub log_page: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    /// Template for new worktree paths. `{parent}`, `{repo}`, `{branch}` expand.
    pub default_dir: String,
    pub switch_closes_tabs: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KeymapConfig {
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FilesConfig {
    pub show_hidden: bool,
    pub show_ignored: bool,
    /// Watch the project for changes made outside jetl.
    pub watch: bool,
}

/// System clipboard tools for paste (`util::clipboard`). Empty means
/// detect from the environment; `"none"` turns the tool off.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Prints the clipboard text on stdout.
    pub paste_command: String,
    /// Reads text to put on the clipboard from stdin.
    pub copy_command: String,
}

/// External formatters (`editor::format`): the buffer text goes in on
/// stdin and the formatted text comes back on stdout.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FormatConfig {
    /// Format before an explicit save (Ctrl+S, save all).
    pub on_save: bool,
    /// A formatter that has not finished by then is killed.
    pub timeout_ms: u64,
    /// Command and arguments per language id (`LanguageDef::name`); `{path}`
    /// in an argument becomes the file's path.
    #[serde(flatten)]
    pub commands: std::collections::HashMap<String, Vec<String>>,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self { on_save: false, timeout_ms: 5000, commands: Default::default() }
    }
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            tab_width: 4,
            max_highlight_bytes: 2 * 1024 * 1024,
            scrolloff: 3,
            hscrolloff: 5,
            auto_close: true,
            highlight_occurrences: true,
            trim_trailing_whitespace: true,
            final_newline: true,
            autosave: Autosave::Off,
            autosave_idle_ms: 1500,
            max_tabs: 0,
            soft_wrap: false,
            soft_wrap_files: ["*.md", "*.txt", "*.rst", "*.adoc"].map(String::from).to_vec(),
        }
    }
}
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "darcula".into(),
            left_width: 32,
            bottom_height: 14,
            double_click_ms: 400,
            icons: Icons::Unicode,
            colors: ColorMode::Auto,
        }
    }
}
impl Default for GitConfig {
    fn default() -> Self {
        Self { pull_mode: "ff-only".into(), log_all: false, log_page: 500 }
    }
}
impl Default for WorktreeConfig {
    fn default() -> Self {
        Self { default_dir: "{parent}/{repo}.{branch}".into(), switch_closes_tabs: true }
    }
}
impl Default for KeymapConfig {
    fn default() -> Self {
        Self { mode: "default".into() }
    }
}
impl Default for FilesConfig {
    fn default() -> Self {
        Self { show_hidden: true, show_ignored: false, watch: true }
    }
}

impl Config {
    /// Load `config.toml`. A missing file gives the defaults silently; an
    /// unreadable or invalid one is logged and also gives the defaults, with
    /// a message for a startup notification.
    pub fn load(paths: &Paths) -> (Self, Option<String>) {
        let file = paths.config_file();
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
            Err(e) => {
                tracing::error!("cannot read {}: {e}; using defaults", file.display());
                return (Config::default(), Some(format!("{}: {e}; using defaults", file.display())));
            }
        };
        match Self::parse(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                tracing::error!("invalid {}: {e}; using defaults", file.display());
                (Config::default(), Some(format!("{}: {}; using defaults", file.display(), theme::error_summary(&e.to_string()))))
            }
        }
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str::<Config>(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_other_defaults() {
        let cfg = Config::parse("[editor]\ntab_width = 2\n[worktree]\nswitch_closes_tabs = false\n").unwrap();
        assert_eq!(cfg.editor.tab_width, 2);
        assert_eq!(cfg.editor.scrolloff, EditorConfig::default().scrolloff);
        assert!(!cfg.worktree.switch_closes_tabs);
        assert_eq!(cfg.git.pull_mode, "ff-only");
        assert_eq!(cfg.editor.autosave, Autosave::Off);
        assert_eq!(Config::parse("[editor]\nautosave = \"idle\"\n").unwrap().editor.autosave, Autosave::Idle);
        assert!(Config::parse("[editor]\nautosave = \"Idle\"\n").is_err());
        assert_eq!(cfg.ui.icons, Icons::Unicode);
        assert_eq!(Config::parse("[ui]\nicons = \"emoji\"\n").unwrap().ui.icons, Icons::Emoji);
    }

    #[test]
    fn removed_keys_are_ignored() {
        // `editor.cjk_ambiguous_wide` was reserved and removed; old configs still load.
        let cfg = Config::parse("[editor]\ncjk_ambiguous_wide = true\ntab_width = 2\n").unwrap();
        assert_eq!(cfg.editor.tab_width, 2);
        assert_eq!(Config::parse("[editor]\ntab_width = 3\n").unwrap().editor.tab_width, 3);
    }

    #[test]
    fn color_mode_parses() {
        assert_eq!(Config::default().ui.colors, ColorMode::Auto);
        assert_eq!(Config::parse("[ui]\ncolors = \"256\"\n").unwrap().ui.colors, ColorMode::Ansi256);
        assert_eq!(Config::parse("[ui]\ncolors = \"truecolor\"\n").unwrap().ui.colors, ColorMode::TrueColor);
        assert_eq!(Config::parse("[ui]\ncolors = \"auto\"\n").unwrap().ui.colors, ColorMode::Auto);
        assert!(Config::parse("[ui]\ncolors = \"16\"\n").is_err());
        assert!(ColorMode::Auto.truecolor(Some("24bit")));
        assert!(!ColorMode::Auto.truecolor(None));
        assert!(ColorMode::TrueColor.truecolor(None));
        assert!(!ColorMode::Ansi256.truecolor(Some("truecolor")));
    }

    #[test]
    fn format_commands_are_keyed_by_language() {
        let cfg = Config::default().format;
        assert!(!cfg.on_save && cfg.commands.is_empty(), "off unless configured");
        let text = "[format]\non_save = true\ntimeout_ms = 900\nrust = [\"rustfmt\", \"--emit\", \"stdout\"]\n\
                    typescript = [\"prettier\", \"--stdin-filepath\", \"{path}\"]\n";
        let cfg = Config::parse(text).unwrap().format;
        assert!(cfg.on_save);
        assert_eq!(cfg.timeout_ms, 900);
        assert_eq!(cfg.commands["rust"], ["rustfmt", "--emit", "stdout"]);
        assert_eq!(cfg.commands["typescript"][2], "{path}");
        assert_eq!(cfg.commands.len(), 2);
        assert!(Config::parse("[format]\nrust = \"rustfmt\"\n").is_err(), "a command is a list of words");
    }

    #[test]
    fn invalid_config_falls_back() {
        assert!(Config::parse("[editor]\ntab_width = \"wide\"\n").is_err());
        let dir = std::env::temp_dir().join(format!("jetl-test-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths { config_dir: dir.clone(), state_dir: dir.clone(), cache_dir: dir.clone() };
        assert!(Config::load(&paths).1.is_none());
        std::fs::write(paths.config_file(), "[editor\ntab_width = 2").unwrap();
        let (cfg, msg) = Config::load(&paths);
        assert!(msg.unwrap().contains("using defaults"));
        assert_eq!(cfg.editor.tab_width, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
