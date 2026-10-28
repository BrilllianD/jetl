//! File-type icons before file names: one glyph table by file name and
//! extension, coloured by syntax captures (no theme key of its own).

use ratatui::style::Style;

use crate::config::FileIcons;
use crate::config::theme::Theme;

/// One file kind: its Nerd Font glyph, its ASCII stand-in and the syntax
/// capture whose foreground colours it.
#[derive(Debug, Clone, Copy)]
struct Kind {
    nerd: char,
    ascii: char,
    capture: &'static str,
}

const fn kind(nerd: char, ascii: char, capture: &'static str) -> Kind {
    Kind { nerd, ascii, capture }
}

const FOLDER: Kind = kind('\u{f07b}', '/', "module");
const FILE: Kind = kind('\u{f15b}', '-', "comment");
const RUST: Kind = kind('\u{e7a8}', 'R', "keyword");
const PYTHON: Kind = kind('\u{e606}', 'P', "function");
const JAVASCRIPT: Kind = kind('\u{e60c}', 'J', "function");
const TYPESCRIPT: Kind = kind('\u{e628}', 'T', "module");
const GO: Kind = kind('\u{e626}', 'G', "label");
const C: Kind = kind('\u{e61e}', 'C', "module");
const CPP: Kind = kind('\u{e61d}', 'C', "module");
const JAVA: Kind = kind('\u{e738}', 'j', "keyword");
const KOTLIN: Kind = kind('\u{e634}', 'K', "variable.parameter");
const SWIFT: Kind = kind('\u{e755}', 'S', "keyword");
const RUBY: Kind = kind('\u{e739}', 'r', "variable.builtin");
const LUA: Kind = kind('\u{e620}', 'L', "module");
const ZIG: Kind = kind('\u{e6a9}', 'Z', "attribute");
const NIX: Kind = kind('\u{f313}', 'N', "module");
const SHELL: Kind = kind('\u{e795}', '$', "string");
const MAKE: Kind = kind('\u{e673}', 'M', "attribute");
const DOCKER: Kind = kind('\u{f308}', 'D', "module");
const MARKDOWN: Kind = kind('\u{f48a}', '#', "text.title");
const JSON: Kind = kind('\u{e60b}', '{', "function");
const CONFIG: Kind = kind('\u{e615}', '=', "property");
const HTML: Kind = kind('\u{e736}', '<', "tag");
const XML: Kind = kind('\u{e619}', '<', "tag");
const CSS: Kind = kind('\u{e749}', '*', "type.parameter");
const SQL: Kind = kind('\u{e706}', 'Q', "constant");
const GIT: Kind = kind('\u{f1d3}', '@', "keyword");
const LOCK: Kind = kind('\u{f023}', '!', "comment");
const IMAGE: Kind = kind('\u{f1c5}', '%', "constant");
const DIFF: Kind = kind('\u{f440}', '+', "string");

/// Exact, case-sensitive file names; checked before the extension.
const BY_NAME: &[(&str, Kind)] = &[
    ("Cargo.toml", RUST),
    ("pyproject.toml", PYTHON),
    ("go.mod", GO),
    ("go.sum", GO),
    ("Gemfile", RUBY),
    ("Rakefile", RUBY),
    ("Vagrantfile", RUBY),
    ("requirements.txt", PYTHON),
    (".bashrc", SHELL),
    (".zshrc", SHELL),
    (".profile", SHELL),
    (".bash_profile", SHELL),
    (".envrc", SHELL),
    ("Makefile", MAKE),
    ("makefile", MAKE),
    ("GNUmakefile", MAKE),
    ("CMakeLists.txt", MAKE),
    ("Dockerfile", DOCKER),
    ("Containerfile", DOCKER),
    (".env", CONFIG),
    (".editorconfig", CONFIG),
    (".gitignore", GIT),
    (".gitattributes", GIT),
    (".gitmodules", GIT),
    (".gitconfig", GIT),
    ("package-lock.json", LOCK),
];

/// Lowercase extensions without the dot.
const BY_EXT: &[(&str, Kind)] = &[
    ("rs", RUST),
    ("py", PYTHON),
    ("pyi", PYTHON),
    ("pyw", PYTHON),
    ("js", JAVASCRIPT),
    ("mjs", JAVASCRIPT),
    ("cjs", JAVASCRIPT),
    ("jsx", JAVASCRIPT),
    ("ts", TYPESCRIPT),
    ("tsx", TYPESCRIPT),
    ("mts", TYPESCRIPT),
    ("cts", TYPESCRIPT),
    ("go", GO),
    ("c", C),
    ("h", C),
    ("cpp", CPP),
    ("cc", CPP),
    ("cxx", CPP),
    ("hpp", CPP),
    ("hh", CPP),
    ("hxx", CPP),
    ("java", JAVA),
    ("kt", KOTLIN),
    ("kts", KOTLIN),
    ("swift", SWIFT),
    ("rb", RUBY),
    ("rake", RUBY),
    ("ru", RUBY),
    ("gemspec", RUBY),
    ("lua", LUA),
    ("zig", ZIG),
    ("zon", ZIG),
    ("nix", NIX),
    ("sh", SHELL),
    ("bash", SHELL),
    ("zsh", SHELL),
    ("fish", SHELL),
    ("mk", MAKE),
    ("mak", MAKE),
    ("cmake", MAKE),
    ("dockerfile", DOCKER),
    ("md", MARKDOWN),
    ("markdown", MARKDOWN),
    ("json", JSON),
    ("jsonc", JSON),
    ("json5", JSON),
    ("jsonl", JSON),
    ("code-workspace", JSON),
    ("toml", CONFIG),
    ("yaml", CONFIG),
    ("yml", CONFIG),
    ("ini", CONFIG),
    ("cfg", CONFIG),
    ("conf", CONFIG),
    ("properties", CONFIG),
    ("env", CONFIG),
    ("html", HTML),
    ("htm", HTML),
    ("xml", XML),
    ("iml", XML),
    ("wsdl", XML),
    ("kml", XML),
    ("css", CSS),
    ("scss", CSS),
    ("sass", CSS),
    ("less", CSS),
    ("sql", SQL),
    ("lock", LOCK),
    ("png", IMAGE),
    ("jpg", IMAGE),
    ("jpeg", IMAGE),
    ("gif", IMAGE),
    ("svg", IMAGE),
    ("webp", IMAGE),
    ("ico", IMAGE),
    ("bmp", IMAGE),
    ("diff", DIFF),
    ("patch", DIFF),
];

fn kind_of(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        return FOLDER;
    }
    if let Some((_, k)) = BY_NAME.iter().find(|(n, _)| *n == name) {
        return *k;
    }
    if let Some((stem, ext)) = name.rsplit_once('.')
        && !stem.is_empty()
    {
        let ext = ext.to_ascii_lowercase();
        if let Some((_, k)) = BY_EXT.iter().find(|(e, _)| *e == ext) {
            return *k;
        }
    }
    FILE
}

/// The icon for `name` and its style: `base` with the foreground of the
/// kind's syntax capture. `None` exactly when `mode` is `FileIcons::None`;
/// otherwise the string is one glyph and one space, 2 cells wide. `name` is
/// the plain file name, not a path.
pub fn span(name: &str, is_dir: bool, mode: FileIcons, base: Style, theme: &Theme) -> Option<(String, Style)> {
    let k = kind_of(name, is_dir);
    let glyph = match mode {
        FileIcons::None => return None,
        FileIcons::Ascii => k.ascii,
        FileIcons::Nerd => k.nerd,
    };
    let style = match theme.capture_style(k.capture).and_then(|s| s.fg) {
        Some(fg) => base.fg(fg),
        None => base,
    };
    Some((format!("{glyph} "), style))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ratatui::style::Color;
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    use super::*;
    use crate::config::theme::BUILTIN_CAPTURES;

    #[test]
    fn every_glyph_is_one_cell_and_every_capture_is_builtin() {
        let kinds = [FOLDER, FILE].into_iter().chain(BY_NAME.iter().map(|e| e.1)).chain(BY_EXT.iter().map(|e| e.1));
        for k in kinds {
            assert!(('\u{e000}'..='\u{f8ff}').contains(&k.nerd), "{:?} outside the BMP PUA", k.nerd);
            assert_eq!(UnicodeWidthChar::width(k.nerd), Some(1), "{:?}", k.nerd);
            assert!(k.ascii.is_ascii_graphic(), "{:?}", k.ascii);
            assert!(BUILTIN_CAPTURES.contains(&k.capture), "{}", k.capture);
        }
        for (e, _) in BY_EXT {
            assert_eq!(*e, e.to_ascii_lowercase());
            assert!(!e.contains('.'), "{e}");
        }
        let names: HashSet<_> = BY_NAME.iter().map(|e| e.0).collect();
        assert_eq!(names.len(), BY_NAME.len(), "duplicate name");
        let exts: HashSet<_> = BY_EXT.iter().map(|e| e.0).collect();
        assert_eq!(exts.len(), BY_EXT.len(), "duplicate extension");
    }

    #[test]
    fn lookup_order() {
        let a = |n: &str, d: bool| kind_of(n, d).ascii;
        assert_eq!(a("Cargo.toml", false), RUST.ascii);
        assert_eq!(a("x.TOML", false), CONFIG.ascii);
        assert_eq!(a("main.rs", false), RUST.ascii);
        assert_eq!(a("src", true), FOLDER.ascii);
        assert_eq!(a("main.rs", true), FOLDER.ascii);
        for n in ["README", ".hidden", "notes.txt"] {
            assert_eq!(a(n, false), FILE.ascii, "{n}");
        }
    }

    #[test]
    fn span_modes() {
        let theme = Theme::default();
        let base = Style::default().fg(theme.fg).bg(Color::Blue);
        assert_eq!(span("main.rs", false, FileIcons::None, base, &theme), None);
        let fg = theme.capture_style("keyword").unwrap().fg.unwrap();
        assert_eq!(span("main.rs", false, FileIcons::Ascii, base, &theme), Some(("R ".into(), base.fg(fg))));
        let (nerd, style) = span("main.rs", false, FileIcons::Nerd, base, &theme).unwrap();
        assert_eq!(nerd, "\u{e7a8} ");
        assert_eq!(UnicodeWidthStr::width(nerd.as_str()), 2);
        assert_eq!(style.bg, base.bg);
    }
}
