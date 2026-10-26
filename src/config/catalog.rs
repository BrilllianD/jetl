//! The Settings window's catalog: a row for every `config.toml` key of
//! [`super::reload::KEYS`] (a test checks it), grouped into pages.

use super::edit::Value;
use super::{Autosave, ColorMode, Config, Icons};

/// How a key is shown and edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    /// One of `options`; `custom` adds "Other…" (a typed value, `ui.theme` file paths).
    Choice {
        options: &'static [&'static str],
        custom: bool,
    },
    Text,
    /// Strings, edited as one comma-separated line.
    List,
    /// A table: edited in config.toml (the row opens it).
    External,
}

pub struct Setting {
    /// `editor.scrolloff`, one of `reload::KEYS`.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    /// External rows return `Value::Str(String::new())`.
    pub get: fn(&Config) -> Value,
}

pub struct Page {
    pub title: &'static str,
    /// Index into [`PAGES`] of the parent; `None` for a top-level node.
    pub parent: Option<usize>,
    /// Keys shown on the page, in order; empty for a parent-only node.
    pub keys: &'static [&'static str],
}

pub fn setting(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

const fn int(min: i64, max: i64) -> Kind {
    Kind::Int { min, max }
}
const ANY: Kind = int(0, i64::MAX);
fn n(v: impl TryInto<i64>) -> Value {
    Value::Int(v.try_into().unwrap_or(i64::MAX))
}
fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}
fn none(_: &Config) -> Value {
    Value::Str(String::new())
}

macro_rules! row {
    ($key:literal, $label:literal, $help:literal, $kind:expr, $get:expr) => {
        Setting { key: $key, label: $label, help: $help, kind: $kind, get: $get }
    };
}

const THEMES: &[&str] = &["auto", "github-dark", "darcula", "light", "omarchy"];

pub static SETTINGS: &[Setting] = &[
    row!(
        "ui.theme",
        "Theme",
        "A built-in theme, \"auto\" (Omarchy's palette if any) or a .toml theme file. Colors: theme.toml.",
        Kind::Choice { options: THEMES, custom: true },
        |c| s(&c.ui.theme)
    ),
    row!(
        "ui.colors",
        "Color depth",
        "\"auto\" uses true color when $COLORTERM is truecolor or 24bit, else the 256-color palette.",
        Kind::Choice { options: &["auto", "truecolor", "256"], custom: false },
        |c| s(match c.ui.colors {
            ColorMode::Auto => "auto",
            ColorMode::TrueColor => "truecolor",
            ColorMode::Ansi256 => "256",
        })
    ),
    row!(
        "ui.icons",
        "Icons",
        "Tool strip and menu icons: one-cell symbols, or emoji (needs a color emoji font).",
        Kind::Choice { options: &["unicode", "emoji"], custom: false },
        |c| s(match c.ui.icons {
            Icons::Unicode => "unicode",
            Icons::Emoji => "emoji",
        })
    ),
    row!("ui.double_click_ms", "Double click interval (ms)", "Double click and Shift Shift interval.", ANY, |c| n(c.ui.double_click_ms)),
    row!("ui.left_width", "Left tool window width", "Initial width of the left tool window (Project, Worktrees).", int(1, 1000), |c| n(c
        .ui
        .left_width)),
    row!("ui.bottom_height", "Bottom tool window height", "Initial height of the bottom tool window (Git).", int(1, 1000), |c| n(c
        .ui
        .bottom_height)),
    row!(
        "editor.tab_width",
        "Tab width",
        "Columns per tab stop and indent level when neither .editorconfig nor the file decides.",
        int(1, 32),
        |c| n(c.editor.tab_width)
    ),
    row!("editor.scrolloff", "Lines kept above and below the caret", "Lines kept visible above and below the caret.", ANY, |c| n(c
        .editor
        .scrolloff)),
    row!("editor.hscrolloff", "Columns kept left and right of the caret", "Columns kept visible left and right of the caret.", ANY, |c| n(
        c.editor.hscrolloff
    )),
    row!(
        "editor.auto_close",
        "Close brackets and quotes in pairs",
        "Type brackets and quotes in pairs, step over closers, wrap selections.",
        Kind::Bool,
        |c| Value::Bool(c.editor.auto_close)
    ),
    row!(
        "editor.highlight_occurrences",
        "Highlight the word at the caret",
        "Highlight the word at the caret (or the selected text) elsewhere on screen.",
        Kind::Bool,
        |c| Value::Bool(c.editor.highlight_occurrences)
    ),
    row!(
        "editor.indent_guides",
        "Show indent guides",
        "Vertical lines at the indent stops in each line's leading whitespace.",
        Kind::Bool,
        |c| Value::Bool(c.editor.indent_guides)
    ),
    row!(
        "editor.inline_blame",
        "Inline blame on the caret line",
        "Author, date and summary of the commit that last changed the caret line, shown after its text (files in a git repository).",
        Kind::Bool,
        |c| Value::Bool(c.editor.inline_blame)
    ),
    row!(
        "editor.breadcrumbs",
        "Show breadcrumbs",
        "A row at the bottom of each editor pane with the definitions around the caret (impl, function, class, heading), from the language's tags query.",
        Kind::Bool,
        |c| Value::Bool(c.editor.breadcrumbs)
    ),
    row!(
        "editor.sticky_scroll",
        "Sticky scroll",
        "Pin the header lines of the definitions scrolled past (impl, function, class, heading) at the top of each editor pane; click one to go to it.",
        Kind::Bool,
        |c| Value::Bool(c.editor.sticky_scroll)
    ),
    row!(
        "editor.snippets",
        "Live templates (snippets)",
        "Tab after a snippet's prefix expands it; snippets are listed in completion. Files: snippets/<language>.toml and all.toml.",
        Kind::Bool,
        |c| Value::Bool(c.editor.snippets)
    ),
    row!(
        "editor.max_tabs",
        "Tab limit",
        "Open tabs beyond this close the least recently used clean, unpinned tab. 0 means no limit.",
        ANY,
        |c| n(c.editor.max_tabs)
    ),
    row!(
        "editor.max_highlight_bytes",
        "Largest file with syntax highlighting (bytes)",
        "Files above this size open without syntax highlighting or gutter marks.",
        ANY,
        |c| n(c.editor.max_highlight_bytes)
    ),
    row!("editor.soft_wrap", "Soft-wrap every file", "Soft-wrap every file when it is opened.", Kind::Bool, |c| Value::Bool(
        c.editor.soft_wrap
    )),
    row!(
        "editor.soft_wrap_files",
        "Soft-wrap these files",
        "Files soft-wrapped when opened anyway: *.ext matches names ending in .ext, anything else a whole file name.",
        Kind::List,
        |c| Value::List(c.editor.soft_wrap_files.clone())
    ),
    row!(
        "editor.soft_wrap_extra_indent",
        "Extra indent of wrapped rows",
        "Extra cells continuation rows are shifted by, after the line's own indent (at most half the width together).",
        ANY,
        |c| n(c.editor.soft_wrap_extra_indent)
    ),
    row!(
        "editor.autosave",
        "Autosave",
        "\"off\", \"focus\" (save when the terminal loses focus) or \"idle\" (save after a pause in editing).",
        Kind::Choice { options: &["off", "focus", "idle"], custom: false },
        |c| s(match c.editor.autosave {
            Autosave::Off => "off",
            Autosave::Focus => "focus",
            Autosave::Idle => "idle",
        })
    ),
    row!("editor.autosave_idle_ms", "Autosave pause (ms)", "Pause in editing before an \"idle\" autosave.", ANY, |c| n(c
        .editor
        .autosave_idle_ms)),
    row!(
        "editor.trim_trailing_whitespace",
        "Strip trailing whitespace on save",
        "On save, strip trailing spaces and tabs (not on the caret line, never in Markdown).",
        Kind::Bool,
        |c| Value::Bool(c.editor.trim_trailing_whitespace)
    ),
    row!(
        "editor.final_newline",
        "End files with a line break on save",
        "On save, end a non-empty file with a line break.",
        Kind::Bool,
        |c| Value::Bool(c.editor.final_newline)
    ),
    row!("format.on_save", "Format on save", "Ctrl+S, Save All and Save As run the file's formatter before writing.", Kind::Bool, |c| {
        Value::Bool(c.format.on_save)
    }),
    row!("format.timeout_ms", "Formatter timeout (ms)", "A formatter still running after this is killed.", ANY, |c| n(c.format.timeout_ms)),
    row!(
        "format.commands",
        "Formatter commands",
        "One command per language id (rust, python, ...). Edit it in config.toml.",
        Kind::External,
        none
    ),
    row!(
        "keymap.mode",
        "Keymap mode",
        "\"vim\" turns on the [mode.*] keymap tables (reserved, minimal). Bindings: keymap.toml.",
        Kind::Choice { options: &["default", "vim"], custom: false },
        |c| s(&c.keymap.mode)
    ),
    row!(
        "git.pull_mode",
        "Pull mode",
        "\"ff-only\" (git pull --ff-only), \"rebase\" (--rebase) or \"merge\" (--no-rebase).",
        Kind::Choice { options: &["ff-only", "rebase", "merge"], custom: false },
        |c| s(&c.git.pull_mode)
    ),
    row!(
        "git.log_all",
        "Log shows all branches",
        "The Log tab shows all branches (branches, remote branches, tags and HEAD); off: HEAD only.",
        Kind::Bool,
        |c| Value::Bool(c.git.log_all)
    ),
    row!("git.log_page", "Commits per Log page", "Commits loaded per Log page.", ANY, |c| n(c.git.log_page)),
    row!(
        "worktree.default_dir",
        "Default worktree path",
        "Suggested path for new worktrees: {parent}, {repo} and {branch} expand; \"~/\" expands.",
        Kind::Text,
        |c| s(&c.worktree.default_dir)
    ),
    row!(
        "worktree.switch_closes_tabs",
        "Switching worktrees closes tabs",
        "Switching worktrees closes the tabs of the old root.",
        Kind::Bool,
        |c| Value::Bool(c.worktree.switch_closes_tabs)
    ),
    row!(
        "files.show_hidden",
        "Show hidden files",
        "Show dot files in the Project tree and Go to File. The tree's toggle, saved in the session, wins once a root has one.",
        Kind::Bool,
        |c| Value::Bool(c.files.show_hidden)
    ),
    row!(
        "files.show_ignored",
        "Show ignored files",
        "Show files ignored by .gitignore in the Project tree and Go to File.",
        Kind::Bool,
        |c| Value::Bool(c.files.show_ignored)
    ),
    row!(
        "files.watch",
        "Follow outside changes",
        "Follow changes made outside jetl (tree, git state, open files); also feeds language servers' file change events.",
        Kind::Bool,
        |c| Value::Bool(c.files.watch)
    ),
    row!("terminal.shell", "Shell", "The Terminal window's shell. Empty means $SHELL, else /bin/sh.", Kind::Text, |c| s(&c.terminal.shell)),
    row!("terminal.args", "Shell arguments", "Arguments of the shell, e.g. -l for a login shell.", Kind::List, |c| Value::List(
        c.terminal.args.clone()
    )),
    row!("terminal.scrollback", "Scrollback lines", "Lines kept above the screen for the mouse wheel.", ANY, |c| n(c.terminal.scrollback)),
    row!("run.tasks", "Run tasks", "Older name of run.configs; still read. Edit them in config.toml.", Kind::External, none),
    row!(
        "run.configs",
        "Run configurations",
        "Named commands with arguments, directory, environment and output matcher (Alt+Shift+F10 picks one). Edit them in config.toml, or per project with Run > Edit Configurations….",
        Kind::External,
        none
    ),
    row!(
        "clipboard.paste_command",
        "Paste command",
        "Prints the system clipboard. Empty means detect, \"none\" means jetl's own last copy only.",
        Kind::Text,
        |c| s(&c.clipboard.paste_command)
    ),
    row!(
        "clipboard.copy_command",
        "Copy command",
        "Takes copied text on stdin, besides OSC 52. Empty means the detected tool's partner, \"none\" means OSC 52 only.",
        Kind::Text,
        |c| s(&c.clipboard.copy_command)
    ),
    row!("local_history.enabled", "Local history", "Snapshot files on save and before they are overwritten or deleted.", Kind::Bool, |c| {
        Value::Bool(c.local_history.enabled)
    }),
    row!(
        "local_history.keep_days",
        "Keep snapshots (days)",
        "Snapshots older than this are pruned (0 drops every one at the next prune; labels are kept).",
        ANY,
        |c| n(c.local_history.keep_days)
    ),
    row!(
        "local_history.max_total_mb",
        "Store size limit (MB)",
        "The oldest snapshots are pruned while the store holds more (compressed size on disk; labels go last).",
        ANY,
        |c| n(c.local_history.max_total_mb)
    ),
    row!("local_history.max_file_kb", "Largest file (KB)", "Larger files are not snapshotted.", ANY, |c| n(c.local_history.max_file_kb)),
    row!(
        "local_history.snapshot_unsaved_secs",
        "Snapshot unsaved edits every (s)",
        "Every this many seconds, snapshot unsaved edits of open files (0: off; at least 10).",
        ANY,
        |c| n(c.local_history.snapshot_unsaved_secs)
    ),
    row!("lsp.enabled", "Language servers", "Start language servers for open files.", Kind::Bool, |c| Value::Bool(c.lsp.enabled)),
    row!(
        "lsp.completion_auto",
        "Completion while typing",
        "Open the completion popup while typing (else Ctrl+Space only).",
        Kind::Bool,
        |c| Value::Bool(c.lsp.completion_auto)
    ),
    row!(
        "lsp.completion_min_chars",
        "Characters before completion",
        "Identifier characters typed before the popup opens by itself.",
        ANY,
        |c| n(c.lsp.completion_min_chars)
    ),
    row!(
        "lsp.inlay_hints",
        "Inlay hints",
        "Show the language server's inlay hints (types, parameter names) in the text.",
        Kind::Bool,
        |c| Value::Bool(c.lsp.inlay_hints)
    ),
    row!("lsp.max_diagnostics_per_file", "Problems kept per file", "Problems kept per file, the most severe first.", ANY, |c| n(c
        .lsp
        .max_diagnostics_per_file)),
    row!("lsp.scratches", "Servers for scratch files", "Start language servers for scratch files too (own clients).", Kind::Bool, |c| {
        Value::Bool(c.lsp.scratches)
    }),
    row!("lsp.startup_timeout_ms", "Startup timeout (ms)", "A server that has not initialized by then is killed.", ANY, |c| n(c
        .lsp
        .startup_timeout_ms)),
    row!("lsp.request_timeout_ms", "Request timeout (ms)", "A request to a server unanswered by then fails.", ANY, |c| n(c
        .lsp
        .request_timeout_ms)),
    row!(
        "lsp.servers",
        "Servers",
        "Changes to the built-in servers, or new ones, as [lsp.servers.<name>] tables. Edit them in config.toml.",
        Kind::External,
        none
    ),
];

/// Title of the keyless page that shows `doctor::probe`'s report (`ui::settings`).
pub const DEPENDENCIES: &str = "Dependencies";

macro_rules! page {
    ($title:literal, $parent:expr, [$($key:literal),* $(,)?]) => {
        Page { title: $title, parent: $parent, keys: &[$($key),*] }
    };
}

pub static PAGES: &[Page] = &[
    page!("Appearance", None, ["ui.theme", "ui.colors", "ui.icons", "ui.double_click_ms", "ui.left_width", "ui.bottom_height"]),
    page!("Editor", None, []),
    page!(
        "General",
        Some(1),
        [
            "editor.tab_width",
            "editor.scrolloff",
            "editor.hscrolloff",
            "editor.auto_close",
            "editor.highlight_occurrences",
            "editor.indent_guides",
            "editor.inline_blame",
            "editor.breadcrumbs",
            "editor.sticky_scroll",
            "editor.snippets",
            "editor.max_tabs",
            "editor.max_highlight_bytes"
        ]
    ),
    page!("Soft Wraps", Some(1), ["editor.soft_wrap", "editor.soft_wrap_files", "editor.soft_wrap_extra_indent"]),
    page!("Saving", Some(1), ["editor.autosave", "editor.autosave_idle_ms", "editor.trim_trailing_whitespace", "editor.final_newline"]),
    page!("Formatting", Some(1), ["format.on_save", "format.timeout_ms", "format.commands"]),
    page!("Keymap", None, ["keymap.mode"]),
    page!("Version Control", None, []),
    page!("Git", Some(7), ["git.pull_mode", "git.log_all", "git.log_page"]),
    page!("Worktrees", Some(7), ["worktree.default_dir", "worktree.switch_closes_tabs"]),
    page!("Files", None, ["files.show_hidden", "files.show_ignored", "files.watch"]),
    page!("Tools", None, []),
    page!("Terminal", Some(11), ["terminal.shell", "terminal.args", "terminal.scrollback"]),
    page!("Run", Some(11), ["run.tasks", "run.configs"]),
    page!("Clipboard", Some(11), ["clipboard.paste_command", "clipboard.copy_command"]),
    page!(
        "Local History",
        Some(11),
        [
            "local_history.enabled",
            "local_history.keep_days",
            "local_history.max_total_mb",
            "local_history.max_file_kb",
            "local_history.snapshot_unsaved_secs"
        ]
    ),
    page!(
        "Language Servers",
        None,
        [
            "lsp.enabled",
            "lsp.completion_auto",
            "lsp.completion_min_chars",
            "lsp.inlay_hints",
            "lsp.max_diagnostics_per_file",
            "lsp.scratches",
            "lsp.startup_timeout_ms",
            "lsp.request_timeout_ms",
            "lsp.servers"
        ]
    ),
    page!("Dependencies", None, []),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::edit::{Edit, apply_edits};
    use crate::config::reload::KEYS;
    use crate::config::theme;

    #[test]
    fn dependencies_page_is_last_and_keyless() {
        let last = PAGES.len() - 1;
        let p = &PAGES[last];
        assert_eq!(p.title, DEPENDENCIES);
        assert!(p.keys.is_empty() && p.parent.is_none());
        assert!(!PAGES.iter().any(|q| q.parent == Some(last)), "no children");
    }

    #[test]
    fn help_names_keymap_and_theme_files() {
        for (key, file) in [("ui.theme", "theme.toml"), ("keymap.mode", "keymap.toml")] {
            let help = setting(key).unwrap().help;
            assert!(help.contains(file), "{key}: {help}");
            assert!(crate::ui::toasts::wrap(help, 49, usize::MAX).len() <= 2, "{key} wraps to more than two rows");
        }
    }

    #[test]
    fn catalog_covers_every_key_once() {
        let mut have: Vec<&str> = SETTINGS.iter().map(|s| s.key).collect();
        let mut want: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
        have.sort_unstable();
        want.sort_unstable();
        assert_eq!(have, want, "SETTINGS and reload::KEYS differ");
        let mut paged: Vec<&str> = PAGES.iter().flat_map(|p| p.keys.iter().copied()).collect();
        paged.sort_unstable();
        assert_eq!(paged, want, "every key sits on exactly one page");
        for (i, p) in PAGES.iter().enumerate() {
            assert!(p.parent.is_none_or(|q| q < i), "{} has a later parent", p.title);
            for k in p.keys {
                assert!(setting(k).is_some(), "{k}");
            }
        }
    }

    #[test]
    fn choice_options_match_serde_names() {
        for st in SETTINGS {
            let Kind::Choice { options, .. } = st.kind else { continue };
            for o in options {
                let out =
                    apply_edits("", &[(st.key, Edit::Set(Value::Str((*o).into())))]).unwrap_or_else(|e| panic!("{}={o}: {e}", st.key));
                assert_eq!((st.get)(&Config::parse(&out).unwrap()), Value::Str((*o).into()), "{}={o}", st.key);
            }
        }
    }

    #[test]
    fn theme_options_list_every_builtin() {
        let Kind::Choice { options, .. } = setting("ui.theme").unwrap().kind else { panic!("ui.theme is a Choice") };
        for b in theme::BUILTIN_THEMES {
            assert!(options.contains(&b.name), "{}", b.name);
        }
        assert!(options.contains(&theme::AUTO) && options.contains(&theme::OMARCHY));
    }

    #[test]
    fn int_defaults_lie_within_bounds() {
        for st in SETTINGS {
            let Kind::Int { min, max } = st.kind else { continue };
            let Value::Int(v) = (st.get)(&Config::default()) else { panic!("{} gives no Int", st.key) };
            assert!((min..=max).contains(&v), "{}: {v} outside {min}..={max}", st.key);
        }
    }
}
