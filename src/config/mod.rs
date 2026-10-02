//! Configuration, paths and logging.

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
        let state_dir = dirs
            .state_dir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| dirs.data_local_dir().to_path_buf());
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            state_dir,
            cache_dir: dirs.cache_dir().to_path_buf(),
        })
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
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("jetl.log")
    }
}

/// Install a file logger. Returns a guard that must live until exit so the
/// non-blocking writer flushes.
pub fn init_logging(paths: &Paths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(&paths.state_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log_file())?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("jetl=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_ansi(false).with_target(true))
        .init();
    tracing::info!("jetl {} starting", env!("CARGO_PKG_VERSION"));
    Ok(guard)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub editor: EditorConfig,
    pub ui: UiConfig,
    pub git: GitConfig,
    pub worktree: WorktreeConfig,
    pub keymap: KeymapConfig,
    pub files: FilesConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    pub tab_width: usize,
    pub max_highlight_bytes: usize,
    pub scrolloff: usize,
    pub hscrolloff: usize,
    pub cjk_ambiguous_wide: bool,
    pub auto_close: bool,
    pub highlight_occurrences: bool,
    pub trim_trailing_whitespace: bool,
    pub final_newline: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub left_width: u16,
    pub bottom_height: u16,
    pub double_click_ms: u64,
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
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self { tab_width: 4, max_highlight_bytes: 2 * 1024 * 1024, scrolloff: 3, hscrolloff: 5, cjk_ambiguous_wide: false, auto_close: true, highlight_occurrences: true, trim_trailing_whitespace: true, final_newline: true }
    }
}
impl Default for UiConfig {
    fn default() -> Self {
        Self { theme: "darcula".into(), left_width: 32, bottom_height: 14, double_click_ms: 400 }
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
        Self { show_hidden: true, show_ignored: false }
    }
}
impl Default for Config {
    fn default() -> Self {
        Self {
            editor: Default::default(),
            ui: Default::default(),
            git: Default::default(),
            worktree: Default::default(),
            keymap: Default::default(),
            files: Default::default(),
        }
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
