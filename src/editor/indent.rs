//! Per-buffer indentation and `.editorconfig` settings: what Tab, Enter
//! and indent/unindent insert, and the save options a project overrides.
//!
//! On open the indent comes from `.editorconfig`, else from the file's own
//! indentation, else from `editor.tab_width` (spaces).

use std::path::Path;

use ropey::Rope;

use super::buffer::LineEnding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndentStyle {
    pub hard_tabs: bool,
    /// Columns per level: spaces inserted, or the width of a tab.
    pub width: usize,
}

impl Default for IndentStyle {
    fn default() -> Self {
        Self { hard_tabs: false, width: 4 }
    }
}

impl IndentStyle {
    pub fn spaces(width: usize) -> Self {
        Self { hard_tabs: false, width: width.max(1) }
    }

    pub fn tabs(width: usize) -> Self {
        Self { hard_tabs: true, width: width.max(1) }
    }

    /// One level of indentation as text.
    pub fn unit(&self) -> String {
        if self.hard_tabs { "\t".into() } else { " ".repeat(self.width.max(1)) }
    }

    /// Status bar text: "Tab" or "N spaces".
    pub fn label(&self) -> String {
        if self.hard_tabs { "Tab".into() } else { format!("{} spaces", self.width) }
    }
}

/// The `.editorconfig` properties jetl uses for one file; `None` where no
/// applicable section sets the key (or sets it to `unset` or nonsense).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSettings {
    /// `indent_style`: tab (true) or space (false).
    pub indent_tabs: Option<bool>,
    /// `indent_size` as a number (`indent_size = tab` defers to `tab_width`).
    pub indent_size: Option<usize>,
    pub tab_width: Option<usize>,
    /// Only applied to files that do not exist on disk yet.
    pub end_of_line: Option<LineEnding>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
}

impl FileSettings {
    /// Settings for the file at `path` (absolute; it need not exist) from the
    /// `.editorconfig` files in its ancestors up to the filesystem root or
    /// the first one with `root = true`; closer files and later sections
    /// win. A malformed file is logged and ignored.
    pub fn for_path(path: &Path) -> Self {
        use ec4rs::property as p;
        let props = match ec4rs::properties_of(path) {
            Ok(props) => props,
            Err(e) => {
                tracing::warn!("editorconfig for {}: {e}", path.display());
                return Self::default();
            }
        };
        let positive = |n: usize| (n > 0).then_some(n);
        Self {
            indent_tabs: props.get::<p::IndentStyle>().ok().map(|s| s == p::IndentStyle::Tabs),
            indent_size: match props.get::<p::IndentSize>() {
                Ok(p::IndentSize::Value(n)) => positive(n),
                _ => None,
            },
            tab_width: match props.get::<p::TabWidth>() {
                Ok(p::TabWidth::Value(n)) => positive(n),
                _ => None,
            },
            end_of_line: match props.get::<p::EndOfLine>() {
                Ok(p::EndOfLine::Lf) => Some(LineEnding::Lf),
                Ok(p::EndOfLine::CrLf) => Some(LineEnding::CrLf),
                // Lone CR files are not supported.
                _ => None,
            },
            trim_trailing_whitespace: match props.get::<p::TrimTrailingWs>() {
                Ok(p::TrimTrailingWs::Value(v)) => Some(v),
                _ => None,
            },
            insert_final_newline: match props.get::<p::FinalNewline>() {
                Ok(p::FinalNewline::Value(v)) => Some(v),
                _ => None,
            },
        }
    }

    /// The buffer's indent: keys set here win, the rest comes from the
    /// text's own indentation, then from `default_width` spaces. A tab level
    /// is `tab_width` wide (else `indent_size`), a space level `indent_size`
    /// (else `tab_width`), as the EditorConfig spec defaults them.
    pub fn indent(&self, rope: &Rope, default_width: usize) -> IndentStyle {
        let tab_level = self.tab_width.or(self.indent_size).unwrap_or(default_width);
        let space_level = self.indent_size.or(self.tab_width);
        let detected = || detect(rope);
        match self.indent_tabs {
            Some(true) => IndentStyle::tabs(tab_level),
            Some(false) => IndentStyle::spaces(
                space_level
                    .or_else(|| match detected() {
                        Some(Detected::Spaces(n)) => Some(n),
                        _ => None,
                    })
                    .unwrap_or(default_width),
            ),
            None => match detected() {
                Some(Detected::Tabs) => IndentStyle::tabs(tab_level),
                Some(Detected::Spaces(n)) => IndentStyle::spaces(space_level.unwrap_or(n)),
                None => IndentStyle::spaces(space_level.unwrap_or(default_width)),
            },
        }
    }

    /// Display width of a tab when `.editorconfig` sets one: `tab_width`, or
    /// `indent_size` in a tab-indented buffer.
    pub fn tab_display(&self, indent: IndentStyle) -> Option<usize> {
        self.tab_width.or(self.indent_size.filter(|_| indent.hard_tabs))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    Tabs,
    Spaces(usize),
}

/// Lines looked at by [`detect`].
const DETECT_LINES: usize = 1000;

/// Guess the indentation from the first [`DETECT_LINES`] lines: tabs when
/// more lines start with a tab than with two or more spaces (a single space
/// is usually a block comment's ` *`), else the most common step of 2, 4
/// or 8 columns between a line and a more indented next one. `None` without
/// enough evidence: no indented lines, fewer than two steps of the winning
/// size, or a tie.
pub fn detect(rope: &Rope) -> Option<Detected> {
    let (mut tab_lines, mut space_lines) = (0usize, 0usize);
    // Steps of 2, 4 and 8 columns.
    let mut steps = [0usize; 3];
    let mut prev: Option<usize> = None;
    for line in rope.lines().take(DETECT_LINES) {
        let mut spaces = 0;
        let mut lead_tab = false;
        let mut mixed = false;
        let mut blank = true;
        for c in line.chars() {
            match c {
                ' ' => spaces += 1,
                '\t' if spaces == 0 => lead_tab = true,
                '\t' => mixed = true,
                '\n' | '\r' => break,
                _ => {
                    blank = false;
                    break;
                }
            }
        }
        if blank {
            continue;
        }
        if lead_tab || mixed {
            tab_lines += lead_tab as usize;
            prev = None;
            continue;
        }
        if spaces >= 2 {
            space_lines += 1;
        }
        if let Some(p) = prev
            && spaces > p
        {
            match spaces - p {
                2 => steps[0] += 1,
                4 => steps[1] += 1,
                8 => steps[2] += 1,
                _ => {}
            }
        }
        prev = Some(spaces);
    }
    if tab_lines > space_lines {
        return Some(Detected::Tabs);
    }
    let (best, &count) = steps.iter().enumerate().max_by_key(|(_, n)| **n)?;
    let tie = steps.iter().enumerate().any(|(i, n)| i != best && *n == count);
    (count >= 2 && !tie).then_some(Detected::Spaces([2, 4, 8][best]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn detects_tabs() {
        let text = "fn a() {\n\tif x {\n\t\ty();\n\t}\n}\n/**\n * doc\n */\n";
        assert_eq!(detect(&rope(text)), Some(Detected::Tabs));
        assert_eq!(FileSettings::default().indent(&rope(text), 4), IndentStyle::tabs(4));
    }

    #[test]
    fn detects_two_spaces() {
        let text = "a:\n  b:\n    c: 1\n    d: 2\n  e:\n    f: 3\ng: 4\n";
        assert_eq!(detect(&rope(text)), Some(Detected::Spaces(2)));
        assert_eq!(FileSettings::default().indent(&rope(text), 4), IndentStyle::spaces(2));
    }

    #[test]
    fn detects_four_spaces() {
        let text = "def f():\n    if x:\n        y()\n            # odd\n    return [\n        1,\n    ]\n\n\nclass C:\n    pass\n";
        assert_eq!(detect(&rope(text)), Some(Detected::Spaces(4)));
        assert_eq!(FileSettings::default().indent(&rope(text), 2), IndentStyle::spaces(4));
    }

    #[test]
    fn ambiguous_text_falls_back_to_config() {
        // One indented line is not enough evidence.
        assert_eq!(detect(&rope("a\n  b\nc\n")), None);
        // As many 2-column steps as 4-column ones.
        assert_eq!(detect(&rope("a\n  b\nc\n  d\ne\n    f\ng\n    h\n")), None);
        assert_eq!(detect(&rope("no\nindent\n")), None);
        assert_eq!(FileSettings::default().indent(&rope("a\n  b\n"), 4), IndentStyle::spaces(4));
    }

    #[test]
    fn editorconfig_keys_combine_with_detection() {
        let two = rope("a\n  b\n    c\n  d\n    e\n");
        let tabbed = rope("a\n\tb\n");
        let s = FileSettings { indent_tabs: Some(true), ..Default::default() };
        assert_eq!(s.indent(&two, 4), IndentStyle::tabs(4), "style from editorconfig, width from config");
        assert_eq!(s.tab_display(s.indent(&two, 4)), None);
        let s = FileSettings { indent_tabs: Some(true), indent_size: Some(8), ..Default::default() };
        assert_eq!(s.indent(&two, 4), IndentStyle::tabs(8));
        assert_eq!(s.tab_display(s.indent(&two, 4)), Some(8), "indent_size sets the tab width with tabs");
        let s = FileSettings { indent_tabs: Some(false), ..Default::default() };
        assert_eq!(s.indent(&two, 4), IndentStyle::spaces(2), "size from detection");
        assert_eq!(s.indent(&tabbed, 4), IndentStyle::spaces(4));
        let s = FileSettings { indent_size: Some(3), ..Default::default() };
        assert_eq!(s.indent(&two, 4), IndentStyle::spaces(3));
        assert_eq!(s.indent(&tabbed, 4), IndentStyle::tabs(3), "style from detection");
        let s = FileSettings { tab_width: Some(8), indent_size: Some(2), ..Default::default() };
        assert_eq!(s.indent(&tabbed, 4), IndentStyle::tabs(8));
        assert_eq!(s.tab_display(s.indent(&two, 4)), Some(8));
    }

    /// A fresh directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-test-editorconfig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn editorconfig_section_globs() {
        let dir = temp_dir("globs");
        write(
            &dir.join(".editorconfig"),
            "root = true\n\n[*]\nindent_style = space\nindent_size = 4\nend_of_line = lf\n\n[*.go]\nindent_style = tab\n\n[*.{yml,yaml}]\nindent_size = 2\n\n[lib/**.rs]\nindent_size = 3\ntrim_trailing_whitespace = false\ninsert_final_newline = false\n\n[Makefile]\nindent_style = tab\ntab_width = 8\nend_of_line = crlf\n",
        );
        let get = |rel: &str| FileSettings::for_path(&dir.join(rel));
        let txt = get("notes.txt");
        assert_eq!((txt.indent_tabs, txt.indent_size, txt.end_of_line), (Some(false), Some(4), Some(LineEnding::Lf)), "[*]");
        assert_eq!(get("sub/main.go").indent_tabs, Some(true), "[*.ext] in a subdirectory");
        assert_eq!(get("a.yml").indent_size, Some(2), "[*.{{a,b}}]");
        assert_eq!(get("b.yaml").indent_size, Some(2));
        let rs = get("lib/x/y.rs");
        assert_eq!(
            (rs.indent_size, rs.trim_trailing_whitespace, rs.insert_final_newline),
            (Some(3), Some(false), Some(false)),
            "[path/**.ext]"
        );
        assert_eq!(get("src/lib/y.rs").indent_size, Some(4), "a path glob is relative to its file");
        let mk = get("Makefile");
        assert_eq!((mk.indent_tabs, mk.tab_width, mk.end_of_line), (Some(true), Some(8), Some(LineEnding::CrLf)));
        assert_eq!(get("a.txt").trim_trailing_whitespace, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editorconfig_nested_files() {
        let dir = temp_dir("nested");
        write(&dir.join(".editorconfig"), "root = true\n[*]\nindent_style = tab\ntab_width = 8\n[*.rs]\nindent_size = 4\n");
        write(&dir.join("sub/.editorconfig"), "[*.rs]\nindent_style = space\n");
        write(&dir.join("sub/deep/.editorconfig"), "root = true\n[*]\ninsert_final_newline = true\n");
        let get = |rel: &str| FileSettings::for_path(&dir.join(rel));
        let top = get("a.rs");
        assert_eq!((top.indent_tabs, top.indent_size, top.tab_width), (Some(true), Some(4), Some(8)));
        let sub = get("sub/a.rs");
        assert_eq!(
            (sub.indent_tabs, sub.indent_size, sub.tab_width),
            (Some(false), Some(4), Some(8)),
            "the closer file wins, the rest is inherited"
        );
        assert_eq!(get("sub/a.txt").indent_tabs, Some(true));
        let deep = get("sub/deep/a.rs");
        assert_eq!(deep, FileSettings { insert_final_newline: Some(true), ..Default::default() }, "root = true stops the walk");
        assert_eq!(get("sub/deep/a.rs").indent(&rope("x\n"), 4), IndentStyle::spaces(4));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
