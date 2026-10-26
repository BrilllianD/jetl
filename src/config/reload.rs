//! Hot reload classes: when a changed `config.toml` key takes effect.
//!
//! Every key is listed once in the `sections!` table below. The generated
//! `diff` destructures each section exhaustively, so adding a field to a
//! config struct fails to compile until it is classified here (and, when
//! `Live`, applied in `App::apply_config` or `Editor::apply_config`).

use super::{ClipboardConfig, Config, EditorConfig, FilesConfig, FormatConfig, GitConfig, KeymapConfig, UiConfig, WorktreeConfig};
use crate::history::LocalHistoryConfig;
use crate::lsp::LspConfig;
use crate::run::RunConfig;
use crate::terminal::TerminalConfig;

/// When a changed key takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadClass {
    /// At once.
    Live,
    /// For files (and views) opened afterwards; open tabs keep theirs.
    NewViews,
    /// When the Terminal window's next shell starts.
    NextShell,
    /// After Help > Restart Language Servers.
    LspRestart,
    /// Only after a restart of jetl.
    Restart,
}

impl ReloadClass {
    /// Short label for a row; empty for `Live`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Live => "",
            Self::NewViews => "new tabs",
            Self::NextShell => "next shell",
            Self::LspRestart => "restart servers",
            Self::Restart => "restart jetl",
        }
    }

    /// One sentence on when a change takes effect.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Live => "Applies at once.",
            Self::NewViews => "Applies to files opened afterwards; open tabs keep theirs.",
            Self::NextShell => "Applies when the Terminal window's next shell starts.",
            Self::LspRestart => "Applies after Help > Restart Language Servers.",
            Self::Restart => "Applies after a restart of jetl.",
        }
    }
}

/// The class of a dotted key (`ui.left_width`), `None` for an unknown key.
pub fn class_of(key: &str) -> Option<ReloadClass> {
    KEYS.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
}

/// The keys whose values differ between two configs, with their class.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigDiff {
    pub changed: Vec<(&'static str, ReloadClass)>,
}

impl ConfigDiff {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    /// Whether the dotted key (`git.log_all`) changed.
    pub fn has(&self, key: &str) -> bool {
        self.changed.iter().any(|(k, _)| *k == key)
    }

    /// Whether any key of a section (`clipboard`) changed.
    pub fn has_section(&self, section: &str) -> bool {
        self.changed.iter().any(|(k, _)| k.split_once('.').is_some_and(|(s, _)| s == section))
    }

    /// The changed keys of one class, in table order.
    pub fn keys(&self, class: ReloadClass) -> Vec<&'static str> {
        self.changed.iter().filter(|(_, c)| *c == class).map(|(k, _)| *k).collect()
    }
}

macro_rules! sections {
    ($( $sec:ident : $ty:ident { $($f:ident => $class:ident),* $(,)? } )*) => {
        /// Every key with its class, in table order.
        pub const KEYS: &[(&str, ReloadClass)] = &[
            $($( (concat!(stringify!($sec), ".", stringify!($f)), ReloadClass::$class), )*)*
        ];

        /// The keys whose values differ from `old` to `new`.
        pub fn diff(old: &Config, new: &Config) -> ConfigDiff {
            // Exhaustive: a new section or field must be classified here.
            let Config { $($sec: _),* } = old;
            let mut changed = Vec::new();
            $(
                let $ty { $($f: _),* } = &old.$sec;
                $(
                    if old.$sec.$f != new.$sec.$f {
                        changed.push((concat!(stringify!($sec), ".", stringify!($f)), ReloadClass::$class));
                    }
                )*
            )*
            ConfigDiff { changed }
        }
    };
}

sections! {
    editor: EditorConfig {
        tab_width => NewViews,
        max_highlight_bytes => NewViews,
        scrolloff => Live,
        hscrolloff => Live,
        auto_close => Live,
        highlight_occurrences => Live,
        indent_guides => Live,
        inline_blame => Live,
        breadcrumbs => Live,
        snippets => Live,
        trim_trailing_whitespace => Live,
        final_newline => Live,
        autosave => Live,
        autosave_idle_ms => Live,
        max_tabs => Live,
        soft_wrap => NewViews,
        soft_wrap_files => NewViews,
        soft_wrap_extra_indent => Live,
    }
    ui: UiConfig {
        theme => Live,
        left_width => Restart,
        bottom_height => Restart,
        double_click_ms => Live,
        icons => Live,
        colors => Live,
    }
    git: GitConfig {
        pull_mode => Live,
        log_all => Live,
        log_page => Live,
    }
    worktree: WorktreeConfig {
        default_dir => Live,
        switch_closes_tabs => Live,
    }
    keymap: KeymapConfig {
        mode => Live,
    }
    files: FilesConfig {
        show_hidden => Live,
        show_ignored => Live,
        watch => Live,
    }
    clipboard: ClipboardConfig {
        paste_command => Live,
        copy_command => Live,
    }
    format: FormatConfig {
        on_save => Live,
        timeout_ms => Live,
        commands => Live,
    }
    run: RunConfig {
        tasks => Live,
        configs => Live,
    }
    terminal: TerminalConfig {
        shell => NextShell,
        args => NextShell,
        scrollback => NextShell,
    }
    lsp: LspConfig {
        enabled => Live,
        startup_timeout_ms => LspRestart,
        request_timeout_ms => LspRestart,
        max_diagnostics_per_file => Live,
        completion_auto => Live,
        completion_min_chars => Live,
        inlay_hints => Live,
        scratches => Live,
        servers => LspRestart,
    }
    local_history: LocalHistoryConfig {
        enabled => Live,
        keep_days => Live,
        max_total_mb => Live,
        max_file_kb => Live,
        snapshot_unsaved_secs => Live,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_of_looks_keys_up() {
        assert_eq!(class_of("ui.left_width"), Some(ReloadClass::Restart));
        assert_eq!(class_of("editor.scrolloff"), Some(ReloadClass::Live));
        assert_eq!(class_of("nope.x"), None);
        assert_eq!(ReloadClass::Live.tag(), "");
        assert_eq!(ReloadClass::Restart.tag(), "restart jetl");
        assert_eq!(ReloadClass::Restart.describe(), "Applies after a restart of jetl.");
    }

    #[test]
    fn diff_of_equal_configs_is_empty() {
        assert!(diff(&Config::default(), &Config::default()).is_empty());
        let c = Config::parse("[editor]\nscrolloff = 9\n[lsp]\nservers.x.command = [\"x\"]\n").unwrap();
        assert!(diff(&c, &c.clone()).is_empty());
    }

    #[test]
    fn diff_names_changed_keys_with_classes() {
        let new = Config::parse("[ui]\nleft_width = 50\n[editor]\nsoft_wrap = true\n[lsp]\nservers.x.command = [\"x\"]\n").unwrap();
        let d = diff(&Config::default(), &new);
        assert_eq!(
            d.changed,
            vec![
                ("editor.soft_wrap", ReloadClass::NewViews),
                ("ui.left_width", ReloadClass::Restart),
                ("lsp.servers", ReloadClass::LspRestart),
            ]
        );
        assert!(d.has("ui.left_width") && !d.has("ui.theme"));
        assert!(d.has_section("lsp") && !d.has_section("clipboard") && !d.has_section("l"));
        assert_eq!(d.keys(ReloadClass::Restart), ["ui.left_width"]);
        assert!(d.keys(ReloadClass::Live).is_empty());
    }

    #[test]
    fn restart_keys_are_documented() {
        let doc = include_str!("../../docs/CONFIG.md");
        for (key, class) in KEYS {
            if matches!(class, ReloadClass::Restart | ReloadClass::LspRestart) {
                assert!(doc.contains(&format!("`{key}`")), "docs/CONFIG.md does not name {key}");
            }
        }
    }

    #[test]
    fn keys_are_unique() {
        let mut names: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KEYS.len());
        // 18 editor, 6 ui, 3 git, 2 worktree, 1 keymap, 3 files, 2 clipboard,
        // 3 format, 2 run, 3 terminal, 9 lsp, 5 local_history.
        assert_eq!(KEYS.len(), 57);
    }
}
