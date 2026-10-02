//! Colors: Darcula-like defaults plus overrides from `theme.toml`.
//!
//! `theme.toml` entries are `name = "#rrggbb"` or `name = { fg, bg, bold,
//! italic, underline }`. Top-level names are `Theme` fields; the `[syntax]`
//! table holds tree-sitter capture names (`keyword`, `"string.escape"`). A
//! color string on a style sets its foreground; a table changes only the
//! attributes it lists. See `docs/CONFIG.md`.

use std::collections::HashMap;
use std::path::Path;

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub panel_bg: Color,
    pub border: Style,
    pub border_focused: Style,
    pub title: Style,
    pub title_focused: Style,
    pub strip_bg: Color,
    pub strip_item: Style,
    pub strip_item_active: Style,
    pub status_bg: Color,
    pub status_fg: Color,
    pub status_accent: Style,
    pub tab: Style,
    pub tab_active: Style,
    pub tab_dirty: Style,
    pub line_number: Style,
    pub line_number_current: Style,
    pub current_line_bg: Color,
    pub selection_bg: Color,
    pub search_match_bg: Color,
    pub search_current_bg: Color,
    pub occurrence_bg: Color,
    pub bracket_match: Style,
    /// Gutter glyph of a bookmarked line.
    pub bookmark: Style,
    pub gutter_added: Color,
    pub gutter_modified: Color,
    pub gutter_deleted: Color,
    pub git_added: Style,
    pub git_modified: Style,
    pub git_deleted: Style,
    pub git_untracked: Style,
    pub git_conflicted: Style,
    pub git_ignored: Style,
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
    pub diff_mod_bg: Color,
    pub popup_bg: Color,
    pub popup_border: Style,
    pub list_selected: Style,
    pub list_selected_unfocused: Style,
    pub dim: Style,
    pub error: Style,
    pub warn: Style,
    pub info: Style,
    pub splitter: Style,
    /// Tree-sitter capture name -> style, looked up by longest dotted prefix.
    pub syntax: HashMap<String, Style>,
}

impl Theme {
    pub fn darcula() -> Self {
        let bg = Color::Rgb(43, 43, 43);
        let panel_bg = Color::Rgb(60, 63, 65);
        let fg = Color::Rgb(169, 183, 198);
        let accent = Color::Rgb(75, 110, 175);
        Self {
            bg,
            fg,
            panel_bg,
            border: Style::default().fg(Color::Rgb(85, 85, 85)),
            border_focused: Style::default().fg(accent),
            title: Style::default().fg(Color::Rgb(187, 187, 187)),
            title_focused: Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            strip_bg: Color::Rgb(49, 51, 53),
            strip_item: Style::default().fg(Color::Rgb(150, 150, 150)).bg(Color::Rgb(49, 51, 53)),
            strip_item_active: Style::default().fg(Color::White).bg(accent).add_modifier(Modifier::BOLD),
            status_bg: Color::Rgb(49, 51, 53),
            status_fg: Color::Rgb(187, 187, 187),
            status_accent: Style::default().fg(Color::Rgb(104, 151, 187)),
            tab: Style::default().fg(Color::Rgb(187, 187, 187)).bg(panel_bg),
            tab_active: Style::default().fg(Color::White).bg(Color::Rgb(78, 82, 84)).add_modifier(Modifier::BOLD),
            tab_dirty: Style::default().fg(Color::Rgb(255, 198, 109)),
            line_number: Style::default().fg(Color::Rgb(96, 99, 102)),
            line_number_current: Style::default().fg(Color::Rgb(164, 167, 171)),
            current_line_bg: Color::Rgb(50, 50, 50),
            selection_bg: Color::Rgb(33, 66, 131),
            search_match_bg: Color::Rgb(50, 89, 60),
            search_current_bg: Color::Rgb(155, 110, 30),
            occurrence_bg: Color::Rgb(52, 65, 52),
            bracket_match: Style::default().bg(Color::Rgb(59, 81, 77)).add_modifier(Modifier::BOLD),
            bookmark: Style::default().fg(Color::Rgb(255, 198, 109)),
            gutter_added: Color::Rgb(98, 150, 85),
            gutter_modified: Color::Rgb(53, 146, 196),
            gutter_deleted: Color::Rgb(140, 140, 140),
            git_added: Style::default().fg(Color::Rgb(98, 150, 85)),
            git_modified: Style::default().fg(Color::Rgb(104, 151, 187)),
            git_deleted: Style::default().fg(Color::Rgb(140, 140, 140)),
            git_untracked: Style::default().fg(Color::Rgb(204, 120, 50)),
            git_conflicted: Style::default().fg(Color::Rgb(255, 100, 100)),
            git_ignored: Style::default().fg(Color::Rgb(110, 110, 110)),
            diff_add_bg: Color::Rgb(41, 66, 54),
            diff_del_bg: Color::Rgb(72, 44, 44),
            diff_mod_bg: Color::Rgb(44, 55, 72),
            popup_bg: Color::Rgb(53, 53, 56),
            popup_border: Style::default().fg(accent),
            list_selected: Style::default().fg(Color::White).bg(Color::Rgb(44, 83, 148)),
            list_selected_unfocused: Style::default().fg(Color::White).bg(Color::Rgb(13, 41, 62)),
            dim: Style::default().fg(Color::Rgb(120, 120, 120)),
            error: Style::default().fg(Color::Rgb(255, 107, 104)),
            warn: Style::default().fg(Color::Rgb(255, 198, 109)),
            info: Style::default().fg(Color::Rgb(104, 151, 187)),
            splitter: Style::default().fg(Color::Rgb(85, 85, 85)).bg(bg),
            syntax: darcula_syntax(fg),
        }
    }

    /// Style for a capture such as `function.method`: tries the full name,
    /// then `function`.
    pub fn capture_style(&self, name: &str) -> Option<Style> {
        let mut n = name;
        loop {
            if let Some(s) = self.syntax.get(n) {
                return Some(*s);
            }
            match n.rfind('.') {
                Some(i) => n = &n[..i],
                None => return None,
            }
        }
    }
}

fn darcula_syntax(fg: Color) -> HashMap<String, Style> {
    let c = |r, g, b| Style::default().fg(Color::Rgb(r, g, b));
    let keyword = c(204, 120, 50);
    let string = c(106, 135, 89);
    let comment = c(128, 128, 128);
    let number = c(104, 151, 187);
    let function = c(255, 198, 109);
    let constant = c(152, 118, 170);
    let meta = c(187, 181, 41);
    let plain = Style::default().fg(fg);
    // Identifier-level captures drawn in the default color (variables,
    // types) are left out so they never shadow a more specific capture of
    // the same node (e.g. `@variable` listed before `@function`).
    let pairs: Vec<(&str, Style)> = vec![
        ("keyword", keyword),
        ("keyword.directive", meta),
        ("string", string),
        ("string.special", c(106, 135, 89)),
        ("string.special.key", c(152, 118, 170)),
        ("string.escape", keyword),
        ("escape", keyword),
        ("comment", comment.add_modifier(Modifier::ITALIC)),
        ("comment.documentation", c(98, 151, 85).add_modifier(Modifier::ITALIC)),
        ("number", number),
        ("boolean", keyword),
        ("constant", constant),
        ("constant.builtin", keyword),
        ("function", function),
        ("function.builtin", c(136, 136, 198)),
        ("function.macro", meta),
        ("type.builtin", keyword),
        ("property", constant),
        ("attribute", meta),
        ("label", c(32, 153, 157)),
        ("operator", plain),
        ("punctuation", plain),
        ("punctuation.special", keyword),
        ("variable.builtin", c(148, 85, 141)),
        ("tag", c(232, 191, 106)),
        // CSS and SCSS queries capture each at-rule under its own name.
        ("media", keyword),
        ("import", keyword),
        ("keyframes", keyword),
        ("charset", keyword),
        ("namespace", keyword),
        ("supports", keyword),
        ("at", keyword),
        ("use", keyword),
        ("forward", keyword),
        ("include", keyword),
        ("mixin", keyword),
        ("extend", keyword),
        ("each", keyword),
        ("for", keyword),
        ("while", keyword),
        ("return", keyword),
        ("debug", keyword),
        ("warn", keyword),
        // Lua query names for control flow, methods and the shebang line.
        ("conditional", keyword),
        ("repeat", keyword),
        ("method", function),
        ("preproc", meta),
        ("embedded", plain),
        ("text.title", c(255, 198, 109).add_modifier(Modifier::BOLD)),
        ("text.literal", c(106, 135, 89)),
        ("text.reference", c(104, 151, 187)),
        ("text.uri", c(104, 151, 187).add_modifier(Modifier::UNDERLINED)),
        ("none", plain),
    ];
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// Declares which `Theme` fields `theme.toml` can set, by type.
macro_rules! theme_fields {
    (colors: [$($c:ident),* $(,)?], styles: [$($s:ident),* $(,)?] $(,)?) => {
        impl Theme {
            /// Every field name `theme.toml` accepts at the top level.
            pub const FIELDS: &'static [&'static str] = &[$(stringify!($c),)* $(stringify!($s),)*];

            fn set_field(&mut self, name: &str, spec: &StyleSpec) -> Result<(), String> {
                match name {
                    $(stringify!($c) => self.$c = spec.color()?,)*
                    $(stringify!($s) => self.$s = spec.patch(self.$s),)*
                    _ => return Err("unknown theme field".into()),
                }
                Ok(())
            }
        }
    };
}

theme_fields! {
    colors: [
        bg, fg, panel_bg, strip_bg, status_bg, status_fg, current_line_bg, selection_bg, search_match_bg,
        search_current_bg, occurrence_bg, gutter_added, gutter_modified, gutter_deleted, diff_add_bg, diff_del_bg,
        diff_mod_bg, popup_bg,
    ],
    styles: [
        border, border_focused, title, title_focused, strip_item, strip_item_active, status_accent, tab, tab_active,
        tab_dirty, line_number, line_number_current, bracket_match, bookmark, git_added, git_modified, git_deleted, git_untracked,
        git_conflicted, git_ignored, popup_border, list_selected, list_selected_unfocused, dim, error, warn, info,
        splitter,
    ],
}

/// One `theme.toml` value: a color string (foreground) or a table.
#[derive(Debug, Default, Clone, PartialEq)]
struct StyleSpec {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
}

const SPEC_KEYS: [&str; 5] = ["fg", "bg", "bold", "italic", "underline"];

impl StyleSpec {
    fn parse(v: &toml::Value) -> Result<Self, String> {
        match v {
            toml::Value::String(s) => Ok(Self { fg: Some(parse_color(s)?), ..Default::default() }),
            toml::Value::Table(t) => {
                let mut spec = Self::default();
                for (k, v) in t {
                    let flag = || v.as_bool().ok_or_else(|| format!("{k}: expected true or false"));
                    match k.as_str() {
                        "fg" | "bg" => {
                            let c = v.as_str().ok_or_else(|| format!("{k}: expected a color string")).and_then(parse_color)?;
                            if k == "fg" { spec.fg = Some(c) } else { spec.bg = Some(c) }
                        }
                        "bold" => spec.bold = Some(flag()?),
                        "italic" => spec.italic = Some(flag()?),
                        "underline" => spec.underline = Some(flag()?),
                        other => return Err(format!("unknown key {other:?} (expected fg, bg, bold, italic, underline)")),
                    }
                }
                Ok(spec)
            }
            other => Err(format!("expected a color string or a table, got {}", other.type_str())),
        }
    }

    /// The value for a plain color field: the string, or the table's `fg`
    /// (else `bg`).
    fn color(&self) -> Result<Color, String> {
        if self.bold.is_some() || self.italic.is_some() || self.underline.is_some() {
            return Err("a color field takes no bold/italic/underline".into());
        }
        self.fg.or(self.bg).ok_or_else(|| "expected a color".into())
    }

    /// `base` with the listed attributes changed.
    fn patch(&self, mut base: Style) -> Style {
        if let Some(c) = self.fg {
            base = base.fg(c);
        }
        if let Some(c) = self.bg {
            base = base.bg(c);
        }
        for (flag, m) in [(self.bold, Modifier::BOLD), (self.italic, Modifier::ITALIC), (self.underline, Modifier::UNDERLINED)] {
            match flag {
                Some(true) => base = base.add_modifier(m),
                Some(false) => base = base.remove_modifier(m),
                None => {}
            }
        }
        base
    }
}

/// `#rrggbb`, or a ratatui color name (`red`, `lightblue`, `reset`) or
/// palette index (`208`).
fn parse_color(s: &str) -> Result<Color, String> {
    let bad = || format!("invalid color {s:?} (expected \"#rrggbb\" or a color name)");
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let n = u32::from_str_radix(hex, 16).map_err(|_| bad())?;
        return Ok(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8));
    }
    s.parse::<Color>().map_err(|_| bad())
}

impl Theme {
    /// Apply a `theme.toml` document. A TOML syntax error rejects the whole
    /// file (`Err`, nothing applied); invalid entries are skipped and
    /// returned as messages while the rest apply.
    pub fn apply_toml(&mut self, text: &str) -> anyhow::Result<Vec<String>> {
        let raw: toml::Table = toml::from_str(text)?;
        let mut errors = Vec::new();
        for (k, v) in &raw {
            if k == "syntax" {
                match v {
                    toml::Value::Table(t) => self.apply_syntax("", t, &mut errors),
                    _ => errors.push("syntax: expected a table".into()),
                }
                continue;
            }
            if let Err(e) = StyleSpec::parse(v).and_then(|spec| self.set_field(k, &spec)) {
                errors.push(format!("{k}: {e}"));
            }
        }
        Ok(errors)
    }

    /// `[syntax]` entries; nested tables that are not style tables are
    /// dotted capture prefixes (`[syntax.string] escape = ...`).
    fn apply_syntax(&mut self, prefix: &str, table: &toml::Table, errors: &mut Vec<String>) {
        for (k, v) in table {
            let name = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            if let toml::Value::Table(t) = v {
                if !t.is_empty() && !t.keys().all(|key| SPEC_KEYS.contains(&key.as_str())) {
                    self.apply_syntax(&name, t, errors);
                    continue;
                }
            }
            match StyleSpec::parse(v) {
                Ok(spec) => {
                    let base = self.syntax.get(&name).copied().unwrap_or_default();
                    self.syntax.insert(name, spec.patch(base));
                }
                Err(e) => errors.push(format!("syntax.{name}: {e}")),
            }
        }
    }

    /// The default theme with `file` applied when it exists. Problems are
    /// logged and returned for a startup notification; a file that does not
    /// parse leaves the defaults untouched.
    pub fn load(file: &Path) -> (Theme, Vec<String>) {
        let mut theme = Theme::default();
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (theme, Vec::new()),
            Err(e) => {
                tracing::error!("cannot read {}: {e}", file.display());
                return (theme, vec![format!("{}: {e}; using the default theme", file.display())]);
            }
        };
        match theme.apply_toml(&text) {
            Ok(errors) => {
                for e in &errors {
                    tracing::error!("{}: {e}", file.display());
                }
                let msgs = errors.iter().map(|e| format!("{}: {e} (entry ignored)", file.display())).collect();
                (theme, msgs)
            }
            Err(e) => {
                tracing::error!("invalid {}: {e}; using the default theme", file.display());
                (Theme::default(), vec![format!("{}: {}; using the default theme", file.display(), error_summary(&e.to_string()))])
            }
        }
    }
}

/// A one-line summary of a (possibly multi-line) parse error: TOML errors
/// put the position on the first line and the reason on the last.
pub fn error_summary(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    match lines.as_slice() {
        [] => s.to_string(),
        [one] => one.to_string(),
        [first, .., last] => format!("{}: {last}", first.trim_start_matches("TOML parse error at ")),
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::darcula()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_named_colors() {
        let mut t = Theme::default();
        let errors = t.apply_toml("bg = \"#102030\"\nfg = \"red\"\ndim = \"#ffffff\"\n").unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(t.bg, Color::Rgb(0x10, 0x20, 0x30));
        assert_eq!(t.fg, Color::Red);
        assert_eq!(t.dim.fg, Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn table_form_patches_attributes() {
        let mut t = Theme::default();
        let text = r##"
            title_focused = { fg = "#000000", bold = false, underline = true }
            selection_bg = { bg = "#334455" }
            [syntax]
            keyword = { fg = "#aabbcc", bold = true }
            "string.escape" = "#010203"
            comment = { italic = false }
            [syntax.function]
            macro = "#0000ff"
        "##;
        let errors = t.apply_toml(text).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        let tf = t.title_focused;
        assert_eq!(tf.fg, Some(Color::Rgb(0, 0, 0)));
        assert!(!tf.add_modifier.contains(Modifier::BOLD));
        assert!(tf.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(t.selection_bg, Color::Rgb(0x33, 0x44, 0x55));
        let kw = t.capture_style("keyword.function").unwrap();
        assert_eq!(kw.fg, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert!(kw.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.capture_style("string.escape").unwrap().fg, Some(Color::Rgb(1, 2, 3)));
        // Only the italic flag changed; the default comment color stays.
        let c = t.capture_style("comment").unwrap();
        assert_eq!(c.fg, Theme::default().capture_style("comment").unwrap().fg);
        assert!(c.sub_modifier.contains(Modifier::ITALIC));
        assert_eq!(t.capture_style("function.macro").unwrap().fg, Some(Color::Rgb(0, 0, 255)));
    }

    #[test]
    fn invalid_values_are_reported_and_skipped() {
        let mut t = Theme::default();
        let text = "bg = \"#zzzzzz\"\nfg = \"#123456\"\nnope = \"#000000\"\ndim = 5\nborder = { fg = \"#000000\", blink = true }\nselection_bg = { fg = \"#000000\", bold = true }\n";
        let errors = t.apply_toml(text).unwrap();
        assert_eq!(errors.len(), 5, "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with("bg: invalid color \"#zzzzzz\"")), "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with("nope: unknown theme field")));
        assert_eq!(t.bg, Theme::default().bg);
        assert_eq!(t.fg, Color::Rgb(0x12, 0x34, 0x56));
        // A syntax error rejects the file.
        assert!(Theme::default().apply_toml("bg = ").is_err());
    }

    #[test]
    fn load_falls_back_on_broken_file() {
        let dir = std::env::temp_dir().join(format!("jetl-test-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("theme.toml");
        let (t, msgs) = Theme::load(&file);
        assert!(msgs.is_empty());
        assert_eq!(t.bg, Theme::default().bg);
        std::fs::write(&file, "bg = \"#000000\"\n[[[").unwrap();
        let (t, msgs) = Theme::load(&file);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("using the default theme"), "{msgs:?}");
        assert_eq!(t.bg, Theme::default().bg);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
