//! The config watcher: a debounced, non-recursive watch of the config
//! directory (plus the directory of a `ui.theme` file outside it, and of
//! the targets of config files that are symlinks). Changes to the files
//! jetl reads become an `AppMsg::ConfigChanged` naming which kinds changed;
//! `app/config_reload.rs` reloads them. Not tied to the project root, so it
//! carries no generation and runs whatever `files.watch` says.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc::UnboundedSender;

use super::theme::Theme;
use super::{Config, Paths};
use crate::event::AppMsg;
use crate::files::watch::{DEBOUNCE, changes_content};

/// Which configuration a changed file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConfigFile {
    Config,
    Keymap,
    Theme,
}

/// The files to react to, each with its kind.
pub type Targets = Vec<(PathBuf, ConfigFile)>;

/// `config.toml`, `keymap.toml` and the theme's files ([`Theme::sources`]).
/// A target that is a symlink adds its resolved path with the same kind, so
/// editing the file a dotfile manager links to is noticed too.
pub fn targets(paths: &Paths, config: &Config) -> Targets {
    let mut out = vec![(paths.config_file(), ConfigFile::Config), (paths.keymap_file(), ConfigFile::Keymap)];
    out.extend(Theme::sources(config, &paths.config_dir).into_iter().map(|p| (p, ConfigFile::Theme)));
    let links: Vec<(PathBuf, ConfigFile)> = out
        .iter()
        .filter(|(p, _)| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
        .filter_map(|(p, kind)| std::fs::canonicalize(p).ok().map(|c| (c, *kind)))
        .collect();
    for link in links {
        if !out.contains(&link) {
            out.push(link);
        }
    }
    out
}

/// The kinds of the targets among `changed` (exact path match), sorted and
/// deduplicated.
pub fn matches<'a>(targets: &Targets, changed: impl Iterator<Item = &'a PathBuf>) -> Vec<ConfigFile> {
    let mut out: Vec<ConfigFile> = changed.flat_map(|p| targets.iter().filter(move |(t, _)| t == p).map(|(_, kind)| *kind)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Keeps the watch alive; dropping it stops watching.
pub struct ConfigWatcher {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    targets: Arc<Mutex<Targets>>,
    /// Directories watched now (non-recursive). One that could not be
    /// watched is missing here and tried again by `set_targets`.
    dirs: Vec<PathBuf>,
}

impl std::fmt::Debug for ConfigWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigWatcher").field("dirs", &self.dirs).finish()
    }
}

impl ConfigWatcher {
    /// Watch the directories of `targets`. Errs only when the watcher
    /// itself cannot be made; a directory that cannot be watched is logged.
    pub fn start(targets: Targets, tx: UnboundedSender<AppMsg>) -> anyhow::Result<Self> {
        let shared: Arc<Mutex<Targets>> = Arc::default();
        let in_callback = shared.clone();
        let debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
            Ok(events) => {
                let files = {
                    let targets = in_callback.lock().unwrap_or_else(PoisonError::into_inner);
                    matches(&targets, events.iter().filter(|e| changes_content(&e.kind)).flat_map(|e| e.paths.iter()))
                };
                if !files.is_empty() {
                    let _ = tx.send(AppMsg::ConfigChanged { files });
                }
            }
            Err(errors) => {
                for e in errors {
                    tracing::warn!("config watcher: {e}");
                }
            }
        })?;
        let mut w = ConfigWatcher { debouncer, targets: shared, dirs: Vec::new() };
        w.set_targets(targets);
        Ok(w)
    }

    /// React to `targets` from now on: stop watching directories no longer
    /// needed, watch new ones, and retry those that failed before.
    pub fn set_targets(&mut self, targets: Targets) {
        let mut wanted: Vec<PathBuf> = Vec::new();
        for dir in targets.iter().filter_map(|(p, _)| p.parent()) {
            if !wanted.iter().any(|d| d == dir) {
                wanted.push(dir.to_path_buf());
            }
        }
        for dir in std::mem::take(&mut self.dirs) {
            if wanted.contains(&dir) {
                self.dirs.push(dir);
            } else if let Err(e) = self.debouncer.unwatch(&dir) {
                tracing::debug!("config watcher: unwatch {}: {e}", dir.display());
            }
        }
        for dir in wanted {
            if self.dirs.contains(&dir) {
                continue;
            }
            match self.debouncer.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => self.dirs.push(dir),
                Err(e) => tracing::warn!("config watcher: cannot watch {}: {e}", dir.display()),
            }
        }
        *self.targets.lock().unwrap_or_else(PoisonError::into_inner) = targets;
    }

    #[cfg(test)]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    fn test_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-cfgwatch-{case}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn paths(config_dir: &Path) -> Paths {
        Paths { config_dir: config_dir.to_path_buf(), state_dir: config_dir.join("state"), cache_dir: config_dir.join("cache") }
    }

    #[test]
    fn targets_include_theme_file_outside_config_dir() {
        let cfg = Path::new("/nonexistent-jetl/config");
        let config = Config::parse("[ui]\ntheme = \"/abs/other/mine.toml\"\n").unwrap();
        let t = targets(&paths(cfg), &config);
        assert_eq!(
            t,
            vec![
                (cfg.join("config.toml"), ConfigFile::Config),
                (cfg.join("keymap.toml"), ConfigFile::Keymap),
                (PathBuf::from("/abs/other/mine.toml"), ConfigFile::Theme),
                (cfg.join("theme.toml"), ConfigFile::Theme),
            ]
        );
        assert_eq!(targets(&paths(cfg), &Config::default()).len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn targets_follow_symlinks() {
        let dir = test_dir("links");
        let cfg = dir.join("config");
        std::fs::create_dir_all(dir.join("dots")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(dir.join("dots/config.toml"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("dots/config.toml"), cfg.join("config.toml")).unwrap();
        let t = targets(&paths(&cfg), &Config::default());
        assert!(t.contains(&(cfg.join("config.toml"), ConfigFile::Config)));
        assert!(t.contains(&(dir.join("dots/config.toml"), ConfigFile::Config)));
        assert_eq!(t.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matches_dedups_and_ignores_others() {
        let cfg = Path::new("/c");
        let t = targets(&paths(cfg), &Config::default());
        let changed: Vec<PathBuf> =
            ["config.toml", "config.toml", ".config.toml.jetl-tmp", "theme.toml"].iter().map(|n| cfg.join(n)).collect();
        assert_eq!(matches(&t, changed.iter()), [ConfigFile::Config, ConfigFile::Theme]);
        assert!(matches(&t, [cfg.join("theme.toml.swp")].iter()).is_empty());
    }

    /// Waits for a `ConfigChanged` that names `want`.
    async fn wait_for(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, want: ConfigFile) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(AppMsg::ConfigChanged { files })) if files.contains(&want) => return,
                Ok(Some(AppMsg::ConfigChanged { .. })) => {}
                other => panic!("expected ConfigChanged with {want:?}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn watcher_reports_target_changes_and_retargets() {
        let dir = test_dir("live");
        let cfg = dir.join("config");
        let other = dir.join("other");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut w = ConfigWatcher::start(targets(&paths(&cfg), &Config::default()), tx).unwrap();
        assert_eq!(w.dirs(), std::slice::from_ref(&cfg));
        std::fs::write(cfg.join("keymap.toml"), "[global]\n").unwrap();
        wait_for(&mut rx, ConfigFile::Keymap).await;
        let mine = other.join("mine.toml");
        let config = Config::parse(&format!("[ui]\ntheme = {:?}\n", mine.display().to_string())).unwrap();
        w.set_targets(targets(&paths(&cfg), &config));
        assert!(w.dirs().contains(&other), "{:?}", w.dirs());
        std::fs::write(&mine, "bg = \"#010203\"\n").unwrap();
        wait_for(&mut rx, ConfigFile::Theme).await;
        // Back to the built-in theme: the other directory is dropped.
        w.set_targets(targets(&paths(&cfg), &Config::default()));
        assert_eq!(w.dirs(), std::slice::from_ref(&cfg));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
