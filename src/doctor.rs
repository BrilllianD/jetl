//! Dependency probe behind `jetl --doctor` and the Settings Dependencies
//! page: language servers, formatters, clipboard tools, compiled grammars,
//! terminal capabilities, git and yman.
//!
//! The probe is synchronous and free of child processes. Terminal
//! capabilities are an input ([`TermCaps`]), not measured inside [`probe`]:
//! the running app must not query the terminal while its event stream reads
//! stdin, so the CLI asks ([`TermCaps::query`]) and the app reads what
//! `tui::init` negotiated ([`TermCaps::running`]).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use unicode_width::UnicodeWidthStr;

use crate::config::{ColorMode, Config};
use crate::util::clipboard::Paste;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Lsp,
    Formatter,
    Clipboard,
    Grammar,
    Terminal,
    Tool,
}

impl Group {
    pub const ALL: [Group; 6] = [Group::Lsp, Group::Formatter, Group::Clipboard, Group::Grammar, Group::Terminal, Group::Tool];

    /// "Language servers", "Formatters", "Clipboard", "Grammars", "Terminal", "Tools".
    pub fn label(self) -> &'static str {
        match self {
            Group::Lsp => "Language servers",
            Group::Formatter => "Formatters",
            Group::Clipboard => "Clipboard",
            Group::Grammar => "Grammars",
            Group::Terminal => "Terminal",
            Group::Tool => "Tools",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Missing,
    Off,
    Unknown,
}

impl Status {
    /// "ok", "missing", "off", "unknown".
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Missing => "missing",
            Status::Off => "off",
            Status::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub group: Group,
    /// Short name: server name, language id, "paste", "kitty keyboard", "git".
    pub name: String,
    pub status: Status,
    /// One line: where it was found, or why it is missing and what that costs.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// In `Group::ALL` order; within a group in probe order.
    pub checks: Vec<Check>,
}

impl Report {
    /// Rows with `Status::Missing`.
    pub fn missing(&self) -> usize {
        self.checks.iter().filter(|c| c.status == Status::Missing).count()
    }

    /// The `--doctor` text, ending in a newline.
    pub fn table(&self) -> String {
        let name_width = self.checks.iter().map(|c| c.name.width()).max().unwrap_or(0) + 2;
        let mut out = format!("jetl {} dependency check\n\n", env!("CARGO_PKG_VERSION"));
        for group in Group::ALL {
            let rows: Vec<&Check> = self.checks.iter().filter(|c| c.group == group).collect();
            if rows.is_empty() {
                continue;
            }
            out.push_str(group.label());
            out.push('\n');
            for c in rows {
                let pad = " ".repeat(name_width.saturating_sub(c.name.width()));
                let line = format!("  {:<9}{}{}{}", c.status.label(), c.name, pad, c.detail);
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
        match self.missing() {
            0 => out.push_str("\nEverything found.\n"),
            n => out.push_str(&format!("\n{n} missing.\n")),
        }
        out
    }
}

/// Terminal capabilities, measured by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermCaps {
    /// Kitty keyboard protocol: `Some(true)` supported, `Some(false)` not, `None` unknown.
    pub kitty: Option<bool>,
    /// `$COLORTERM` as seen by jetl.
    pub colorterm: Option<String>,
}

impl TermCaps {
    /// For `--doctor`: asks the terminal (crossterm, up to 2 s) when stdin and
    /// stdout are both terminals, else `kitty: None`; a query error is also `None`.
    pub fn query() -> Self {
        let kitty = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            crossterm::terminal::supports_keyboard_enhancement().ok()
        } else {
            None
        };
        TermCaps { kitty, colorterm: std::env::var("COLORTERM").ok() }
    }

    /// Inside the running app. No terminal I/O.
    pub fn running() -> Self {
        TermCaps { kitty: Some(crate::tui::keyboard_enhanced()), colorterm: crate::config::color::colorterm_env() }
    }
}

/// Every `lang-*` cargo feature and whether this build has it.
const GRAMMARS: [(&str, bool); 21] = [
    ("rust", cfg!(feature = "lang-rust")),
    ("js", cfg!(feature = "lang-js")),
    ("ts", cfg!(feature = "lang-ts")),
    ("python", cfg!(feature = "lang-python")),
    ("json", cfg!(feature = "lang-json")),
    ("toml", cfg!(feature = "lang-toml")),
    ("bash", cfg!(feature = "lang-bash")),
    ("md", cfg!(feature = "lang-md")),
    ("csharp", cfg!(feature = "lang-csharp")),
    ("yaml", cfg!(feature = "lang-yaml")),
    ("html", cfg!(feature = "lang-html")),
    ("css", cfg!(feature = "lang-css")),
    ("scss", cfg!(feature = "lang-scss")),
    ("xml", cfg!(feature = "lang-xml")),
    ("go", cfg!(feature = "lang-go")),
    ("c", cfg!(feature = "lang-c")),
    ("cpp", cfg!(feature = "lang-cpp")),
    ("java", cfg!(feature = "lang-java")),
    ("php", cfg!(feature = "lang-php")),
    ("lua", cfg!(feature = "lang-lua")),
    ("dockerfile", cfg!(feature = "lang-dockerfile")),
];

type Find<'a> = &'a dyn Fn(&str) -> Option<PathBuf>;

/// Every check for `cfg`, resolving relative commands (those containing `/`) against `root`.
/// Synchronous; walks PATH and stats files, so the app calls it in `spawn_blocking`.
pub fn probe(cfg: &Config, root: &Path, term: &TermCaps) -> Report {
    probe_with(cfg, root, term, &|k| std::env::var(k).ok(), &crate::util::fs::which)
}

fn resolve(cmd: &str, root: &Path, find: Find) -> Option<PathBuf> {
    if cmd.is_empty() {
        None
    } else if cmd.contains('/') {
        Some(root.join(cmd)).filter(|p| crate::util::fs::is_executable(p))
    } else {
        find(cmd)
    }
}

/// `Ok` with the found path, else `Missing` with `"{cmd} not found"`.
fn found(cmd: &str, root: &Path, find: Find) -> (Status, String) {
    match resolve(cmd, root, find) {
        Some(p) => (Status::Ok, p.display().to_string()),
        None => (Status::Missing, format!("{cmd} not found")),
    }
}

fn probe_with(cfg: &Config, root: &Path, term: &TermCaps, env: &dyn Fn(&str) -> Option<String>, find: Find) -> Report {
    let mut checks = Vec::new();
    let mut add = |group: Group, name: &str, (status, detail): (Status, String)| {
        checks.push(Check { group, name: name.to_string(), status, detail });
    };

    for s in cfg.lsp.servers() {
        let langs = s.languages.join(", ");
        let row = if !cfg.lsp.enabled {
            (Status::Off, format!("lsp.enabled = false ({langs})"))
        } else {
            match resolve(&s.command[0], root, find) {
                Some(p) => (Status::Ok, format!("{} ({langs})", p.display())),
                None => (Status::Missing, format!("{} not found ({langs})", s.command[0])),
            }
        };
        add(Group::Lsp, &s.name, row);
    }

    let mut formatters: Vec<_> = cfg.format.commands.iter().filter(|(_, argv)| argv.first().is_some_and(|a| !a.is_empty())).collect();
    formatters.sort_by(|a, b| a.0.cmp(b.0));
    if formatters.is_empty() {
        add(Group::Formatter, "none", (Status::Off, "no formatter commands in [format]".into()));
    }
    for (lang, argv) in formatters {
        add(Group::Formatter, lang, found(&argv[0], root, find));
    }

    let tools = crate::util::clipboard::select(&cfg.clipboard, env, cfg!(target_os = "macos"), |n| find(n).is_some());
    let paste = match &tools.paste {
        Paste::Command(argv) => found(&argv[0], root, find),
        Paste::Disabled => (Status::Off, "clipboard.paste_command = \"none\": paste uses jetl's last copy".into()),
        Paste::Missing => (Status::Missing, "no wl-paste, xclip, xsel or pbpaste for this session: paste uses jetl's last copy".into()),
    };
    add(Group::Clipboard, "paste", paste);
    let copy = match &tools.copy {
        Some(argv) => match resolve(&argv[0], root, find) {
            Some(p) => (Status::Ok, format!("{} and OSC 52", p.display())),
            None => (Status::Missing, format!("{} not found: OSC 52 only", argv[0])),
        },
        None => (Status::Ok, "OSC 52 only (the terminal must support it)".into()),
    };
    add(Group::Clipboard, "copy", copy);

    for (name, compiled) in GRAMMARS {
        let row = if compiled {
            (Status::Ok, "compiled".to_string())
        } else {
            (Status::Missing, format!("not compiled in (cargo feature lang-{name})"))
        };
        add(Group::Grammar, name, row);
    }

    let kitty = match term.kitty {
        Some(true) => (Status::Ok, "supported".to_string()),
        Some(false) => (Status::Missing, "not supported: ctrl+shift+letter, ctrl+tab and double-shift bindings unavailable".into()),
        None => (Status::Unknown, "could not ask the terminal".into()),
    };
    add(Group::Terminal, "kitty keyboard", kitty);
    let color = match cfg.ui.colors {
        ColorMode::TrueColor => (Status::Ok, "ui.colors = \"truecolor\"".to_string()),
        ColorMode::Ansi256 => (Status::Off, "ui.colors = \"256\": themes use the 256-color palette".into()),
        ColorMode::Auto if crate::config::color::truecolor_from_env(term.colorterm.as_deref()) => {
            (Status::Ok, format!("$COLORTERM = {}", term.colorterm.as_deref().unwrap_or_default()))
        }
        ColorMode::Auto => {
            (Status::Missing, format!("$COLORTERM is {}: themes use the 256-color palette", term.colorterm.as_deref().unwrap_or("unset")))
        }
    };
    add(Group::Terminal, "true color", color);

    let git = match find("git") {
        Some(p) => (Status::Ok, p.display().to_string()),
        None => (Status::Missing, "not found: the Git window, worktrees, push and pull need it".into()),
    };
    add(Group::Tool, "git", git);
    let yman = match find("yman") {
        Some(p) => (Status::Ok, p.display().to_string()),
        None => (Status::Missing, "not found: needed for yman task tracking".into()),
    };
    add(Group::Tool, "yman", yman);

    Report { checks }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finder(known: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |n| known.contains(&n).then(|| PathBuf::from(format!("/usr/bin/{n}")))
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn cfg(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    fn caps() -> TermCaps {
        TermCaps { kitty: Some(true), colorterm: Some("truecolor".into()) }
    }

    fn run(c: &Config, known: &'static [&'static str]) -> Report {
        probe_with(c, Path::new("/nonexistent"), &caps(), &no_env, &finder(known))
    }

    fn row<'a>(r: &'a Report, group: Group, name: &str) -> &'a Check {
        r.checks.iter().find(|c| c.group == group && c.name == name).unwrap_or_else(|| panic!("no row {name}"))
    }

    #[test]
    fn grammar_list_matches_cargo_features() {
        let table: toml::Table = toml::from_str(include_str!("../Cargo.toml")).unwrap();
        let mut features: Vec<String> = table["features"].as_table().unwrap().keys().filter(|k| k.starts_with("lang-")).cloned().collect();
        features.sort();
        let mut listed: Vec<String> = GRAMMARS.iter().map(|g| format!("lang-{}", g.0)).collect();
        listed.sort();
        assert_eq!(features, listed);
    }

    #[test]
    fn lsp_rows_found_missing_and_disabled() {
        let r = run(&Config::default(), &["rust-analyzer"]);
        let ra = row(&r, Group::Lsp, "rust-analyzer");
        assert_eq!((ra.status, ra.detail.as_str()), (Status::Ok, "/usr/bin/rust-analyzer (rust)"));
        let go = row(&r, Group::Lsp, "gopls");
        assert_eq!((go.status, go.detail.as_str()), (Status::Missing, "gopls not found (go)"));

        let r = run(&cfg("[lsp]\nenabled = false\n"), &["rust-analyzer"]);
        let lsp: Vec<_> = r.checks.iter().filter(|c| c.group == Group::Lsp).collect();
        assert!(!lsp.is_empty() && lsp.iter().all(|c| c.status == Status::Off));

        let r = run(&cfg("[lsp.servers.clangd]\ncommand = []\n"), &[]);
        assert!(!r.checks.iter().any(|c| c.group == Group::Lsp && c.name == "clangd"));
    }

    #[test]
    fn formatters_sorted_and_none() {
        let r = run(&Config::default(), &[]);
        let fm: Vec<_> = r.checks.iter().filter(|c| c.group == Group::Formatter).collect();
        assert_eq!(fm.len(), 1);
        assert_eq!((fm[0].name.as_str(), fm[0].status), ("none", Status::Off));

        let r = run(&cfg("[format]\nrust = [\"rustfmt\"]\ngo = [\"gofmt\"]\n"), &["gofmt"]);
        let fm: Vec<_> = r.checks.iter().filter(|c| c.group == Group::Formatter).collect();
        assert_eq!(fm.len(), 2);
        assert_eq!((fm[0].name.as_str(), fm[0].status), ("go", Status::Ok));
        assert_eq!((fm[1].name.as_str(), fm[1].status, fm[1].detail.as_str()), ("rust", Status::Missing, "rustfmt not found"));
    }

    #[cfg(unix)]
    #[test]
    fn relative_formatter_resolves_against_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("jetl-test-doctor-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let fmt = dir.join("bin/fmt");
        std::fs::write(&fmt, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fmt, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = probe_with(&cfg("[format]\nrust = [\"./bin/fmt\"]\n"), &dir, &caps(), &no_env, &finder(&[]));
        let status = row(&r, Group::Formatter, "rust").status;
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(status, Status::Ok);
    }

    #[test]
    fn clipboard_rows() {
        let wayland = |k: &str| (k == "WAYLAND_DISPLAY").then(|| "wayland-0".to_string());
        let r = probe_with(&Config::default(), Path::new("/"), &caps(), &wayland, &finder(&["wl-paste", "wl-copy"]));
        let paste = row(&r, Group::Clipboard, "paste");
        assert_eq!((paste.status, paste.detail.as_str()), (Status::Ok, "/usr/bin/wl-paste"));
        let copy = row(&r, Group::Clipboard, "copy");
        assert_eq!((copy.status, copy.detail.as_str()), (Status::Ok, "/usr/bin/wl-copy and OSC 52"));

        let r = run(&Config::default(), &[]);
        assert_eq!(row(&r, Group::Clipboard, "paste").status, Status::Missing);
        let copy = row(&r, Group::Clipboard, "copy");
        assert_eq!((copy.status, copy.detail.as_str()), (Status::Ok, "OSC 52 only (the terminal must support it)"));

        let r = run(&cfg("[clipboard]\npaste_command = \"none\"\n"), &[]);
        assert_eq!(row(&r, Group::Clipboard, "paste").status, Status::Off);
    }

    #[test]
    fn terminal_rows() {
        let none = TermCaps { kitty: None, colorterm: None };
        let r = probe_with(&Config::default(), Path::new("/"), &none, &no_env, &finder(&[]));
        assert_eq!(row(&r, Group::Terminal, "kitty keyboard").status, Status::Unknown);
        let tc = row(&r, Group::Terminal, "true color");
        assert_eq!((tc.status, tc.detail.as_str()), (Status::Missing, "$COLORTERM is unset: themes use the 256-color palette"));

        let full = TermCaps { kitty: Some(true), colorterm: Some("24bit".into()) };
        let r = probe_with(&Config::default(), Path::new("/"), &full, &no_env, &finder(&[]));
        assert_eq!(row(&r, Group::Terminal, "kitty keyboard").status, Status::Ok);
        let tc = row(&r, Group::Terminal, "true color");
        assert_eq!((tc.status, tc.detail.as_str()), (Status::Ok, "$COLORTERM = 24bit"));

        let r = probe_with(&cfg("[ui]\ncolors = \"256\"\n"), Path::new("/"), &full, &no_env, &finder(&[]));
        assert_eq!(row(&r, Group::Terminal, "true color").status, Status::Off);
    }

    #[test]
    fn tools_rows() {
        let r = run(&Config::default(), &["git"]);
        assert_eq!(row(&r, Group::Tool, "git").status, Status::Ok);
        assert_eq!(row(&r, Group::Tool, "yman").status, Status::Missing);
    }

    #[test]
    fn group_order_is_fixed() {
        let r = run(&Config::default(), &[]);
        let mut seen: Vec<Group> = Vec::new();
        for c in &r.checks {
            if seen.last() != Some(&c.group) {
                seen.push(c.group);
            }
        }
        assert_eq!(seen, Group::ALL);
    }

    #[test]
    fn table_format() {
        let check = |group, name: &str, status, detail: &str| Check { group, name: name.into(), status, detail: detail.into() };
        let mut r = Report {
            checks: vec![
                check(Group::Lsp, "gopls", Status::Missing, "gopls not found (go)"),
                check(Group::Lsp, "rust-analyzer", Status::Ok, "/usr/bin/rust-analyzer (rust)"),
                check(Group::Tool, "git", Status::Off, ""),
            ],
        };
        let v = env!("CARGO_PKG_VERSION");
        let expected = format!(
            "jetl {v} dependency check\n\nLanguage servers\n  missing  gopls          gopls not found (go)\n  ok       rust-analyzer  /usr/bin/rust-analyzer (rust)\nTools\n  off      git\n\n1 missing.\n"
        );
        assert_eq!(r.table(), expected);
        r.checks.remove(0);
        assert!(r.table().ends_with("\nEverything found.\n"));
    }
}
