//! Colors: the built-in themes (`assets/themes/*.toml`), theme files and
//! `theme.toml` overrides, and the 256-color fallback (`Theme::load`).
//!
//! Theme documents have entries `name = "#rrggbb"` or `name = { fg, bg,
//! bold, italic, underline, reverse }`. Top-level names are `Theme` fields;
//! the `[syntax]` table holds tree-sitter capture names (`keyword`,
//! `"string.escape"`), and `[colors256]` holds entries used only with 256
//! colors. A color string on a style sets its foreground; a table changes
//! only the attributes it lists. See `docs/CONFIG.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};

use crate::config::omarchy::{self, Palette};
use crate::config::{Config, color};

impl Theme {
    /// Darcula, the default theme (parsed once).
    pub fn darcula() -> Self {
        static DARCULA_THEME: LazyLock<Theme> = LazyLock::new(|| Theme::builtin(&DARCULA, false));
        DARCULA_THEME.clone()
    }

    /// A built-in theme, with 256-color values when `ansi256`. Built-in
    /// files set every key (checked by a test), so they start from a blank
    /// theme.
    pub fn builtin(b: &BuiltinTheme, ansi256: bool) -> Theme {
        let mut t = Theme::blank();
        let errors = t.apply_toml(b.toml, ansi256).expect("built-in theme is valid TOML");
        debug_assert!(errors.is_empty(), "built-in theme {}: {errors:?}", b.name);
        if ansi256 {
            t.map_colors(color::to_ansi256);
        }
        t
    }

    /// Style for a capture such as `function.method`: tries the full name,
    /// then `function`.
    pub fn capture_style(&self, name: &str) -> Option<Style> {
        let mut n = name;
        loop {
            if let Some(s) = self.syntax.get(n) {
                return Some(*s);
            }
            {
                let i = n.rfind('.')?;
                n = &n[..i]
            }
        }
    }
}

/// A theme shipped with jetl, embedded from `assets/themes/`.
#[derive(Debug, PartialEq)]
pub struct BuiltinTheme {
    pub name: &'static str,
    pub toml: &'static str,
}

/// Name of the table holding a theme's 256-color values.
const COLORS256: &str = "colors256";

pub const DARCULA: BuiltinTheme = BuiltinTheme { name: "darcula", toml: include_str!("../../assets/themes/darcula.toml") };

pub const LIGHT: BuiltinTheme = BuiltinTheme { name: "light", toml: include_str!("../../assets/themes/light.toml") };

pub const GITHUB_DARK: BuiltinTheme = BuiltinTheme { name: "github-dark", toml: include_str!("../../assets/themes/github-dark.toml") };

/// Every built-in theme, selectable by name with `ui.theme`.
pub const BUILTIN_THEMES: &[BuiltinTheme] = &[DARCULA, LIGHT, GITHUB_DARK];

/// The `[syntax]` captures every built-in theme sets.
pub const BUILTIN_CAPTURES: &[&str] = &[
    "keyword",
    "keyword.directive",
    "string",
    "string.special",
    "string.special.key",
    "string.escape",
    "escape",
    "comment",
    "comment.documentation",
    "number",
    "boolean",
    "constant",
    "constant.builtin",
    "function",
    "function.builtin",
    "function.macro",
    "type.builtin",
    "property",
    "attribute",
    "label",
    "operator",
    "punctuation",
    "punctuation.special",
    "variable.builtin",
    "tag",
    "embedded",
    "text.title",
    "text.literal",
    "text.reference",
    "text.uri",
    "none",
    "media",
    "import",
    "keyframes",
    "charset",
    "namespace",
    "supports",
    "at",
    "use",
    "forward",
    "include",
    "mixin",
    "extend",
    "each",
    "for",
    "while",
    "return",
    "debug",
    "warn",
    "conditional",
    "repeat",
    "method",
    "preproc",
];

/// Declares the `Theme` struct: every color and style field, each of which
/// is also a `theme.toml` key, mapped by the 256-color fallback.
macro_rules! theme_fields {
    (
        colors: [$($(#[$cm:meta])* $c:ident),* $(,)?],
        styles: [$($(#[$sm:meta])* $s:ident),* $(,)?] $(,)?
    ) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct Theme {
            $($(#[$cm])* pub $c: Color,)*
            $($(#[$sm])* pub $s: Style,)*
            /// Tree-sitter capture name -> style, looked up by longest dotted prefix.
            pub syntax: HashMap<String, Style>,
        }

        impl Theme {
            /// Every field name `theme.toml` accepts at the top level.
            pub const FIELDS: &'static [&'static str] = &[$(stringify!($c),)* $(stringify!($s),)*];

            /// All colors `Reset`, all styles empty, no captures.
            fn blank() -> Self {
                Self {
                    $($c: Color::Reset,)*
                    $($s: Style::default(),)*
                    syntax: HashMap::new(),
                }
            }

            /// Replace every color in the theme (fields, styles, captures) with `f(color)`.
            pub fn map_colors(&mut self, f: impl Fn(Color) -> Color) {
                $(self.$c = f(self.$c);)*
                $(self.$s = map_style(self.$s, &f);)*
                for s in self.syntax.values_mut() {
                    *s = map_style(*s, &f);
                }
            }

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
        bg,
        fg,
        panel_bg,
        strip_bg,
        status_bg,
        status_fg,
        current_line_bg,
        selection_bg,
        search_match_bg,
        search_current_bg,
        occurrence_bg,
        gutter_added,
        gutter_modified,
        gutter_deleted,
        diff_add_bg,
        diff_del_bg,
        diff_mod_bg,
        /// Changed characters inside a modified line (diff view).
        diff_add_word_bg,
        diff_del_word_bg,
        /// Conflict resolver: an unresolved conflict's lines (one-sided changes
        /// take `diff_add_bg`, `diff_del_bg` or `diff_mod_bg`), and the lines of
        /// a resolved chunk.
        merge_conflict_bg,
        merge_resolved_bg,
        popup_bg,
    ],
    styles: [
        border,
        border_focused,
        title,
        title_focused,
        strip_item,
        strip_item_active,
        status_accent,
        tab,
        tab_active,
        tab_dirty,
        line_number,
        line_number_current,
        bracket_match,
        /// The carets other than the primary one (which is the terminal cursor).
        secondary_caret,
        /// Gutter glyph of a bookmarked line (◆, or a numbered one's digit
        /// drawn reversed).
        bookmark,
        /// What a collapsed fold shows after its header line (`…}`).
        fold_placeholder,
        git_added,
        git_modified,
        git_deleted,
        git_untracked,
        git_conflicted,
        git_ignored,
        popup_border,
        list_selected,
        list_selected_unfocused,
        dim,
        error,
        warn,
        info,
        /// Diagnostics: the gutter glyph and Problems window icon take the
        /// foreground; text in the range gets the modifiers (underlined in the
        /// foreground color) and the background, if set.
        diag_error,
        diag_warn,
        diag_info,
        diag_hint,
        /// Gutter glyph (✱) on the caret line when the language server has
        /// quick fixes there.
        lightbulb,
        /// Indent guides (│ in leading whitespace): only the foreground is used.
        indent_guide,
        splitter,
    ],
}

/// `s` with its foreground, background and underline colors mapped by `f`.
fn map_style(mut s: Style, f: &impl Fn(Color) -> Color) -> Style {
    s.fg = s.fg.map(f);
    s.bg = s.bg.map(f);
    s.underline_color = s.underline_color.map(f);
    s
}

/// One `theme.toml` value: a color string (foreground) or a table.
#[derive(Debug, Default, Clone, PartialEq)]
struct StyleSpec {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    reverse: Option<bool>,
}

const SPEC_KEYS: [&str; 6] = ["fg", "bg", "bold", "italic", "underline", "reverse"];

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
                        "reverse" => spec.reverse = Some(flag()?),
                        other => return Err(format!("unknown key {other:?} (expected fg, bg, bold, italic, underline, reverse)")),
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
        if self.bold.is_some() || self.italic.is_some() || self.underline.is_some() || self.reverse.is_some() {
            return Err("a color field takes no bold/italic/underline/reverse".into());
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
        for (flag, m) in [
            (self.bold, Modifier::BOLD),
            (self.italic, Modifier::ITALIC),
            (self.underline, Modifier::UNDERLINED),
            (self.reverse, Modifier::REVERSED),
        ] {
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
    /// Apply a theme document (a built-in, a theme file or `theme.toml`).
    /// A TOML syntax error rejects the whole file (`Err`, nothing applied);
    /// invalid entries are skipped and returned as messages while the rest
    /// apply. The `[colors256]` table applies only when `ansi256`, right
    /// after the top level; otherwise it is only checked for errors.
    pub fn apply_toml(&mut self, text: &str, ansi256: bool) -> anyhow::Result<Vec<String>> {
        let raw: toml::Table = toml::from_str(text)?;
        let mut errors = Vec::new();
        self.apply_table(&raw, "", &mut errors);
        match raw.get(COLORS256) {
            None => {}
            Some(toml::Value::Table(t)) if ansi256 => self.apply_table(t, "colors256.", &mut errors),
            Some(toml::Value::Table(t)) => self.clone().apply_table(t, "colors256.", &mut errors),
            Some(_) => errors.push(format!("{COLORS256}: expected a table")),
        }
        Ok(errors)
    }

    /// Top-level entries (fields and `[syntax]`) of a document or of its
    /// `[colors256]` table; `prefix` starts each error message.
    fn apply_table(&mut self, raw: &toml::Table, prefix: &str, errors: &mut Vec<String>) {
        for (k, v) in raw {
            if k == "syntax" {
                match v {
                    toml::Value::Table(t) => self.apply_syntax(prefix, "", t, errors),
                    _ => errors.push(format!("{prefix}syntax: expected a table")),
                }
                continue;
            }
            if k == COLORS256 && prefix.is_empty() {
                continue;
            }
            if let Err(e) = StyleSpec::parse(v).and_then(|spec| self.set_field(k, &spec)) {
                errors.push(format!("{prefix}{k}: {e}"));
            }
        }
    }

    /// `[syntax]` entries; nested tables that are not style tables are
    /// dotted capture prefixes (`[syntax.string] escape = ...`).
    fn apply_syntax(&mut self, err_prefix: &str, prefix: &str, table: &toml::Table, errors: &mut Vec<String>) {
        for (k, v) in table {
            let name = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            if let toml::Value::Table(t) = v
                && !t.is_empty()
                && !t.keys().all(|key| SPEC_KEYS.contains(&key.as_str()))
            {
                self.apply_syntax(err_prefix, &name, t, errors);
                continue;
            }
            match StyleSpec::parse(v) {
                Ok(spec) => {
                    let base = self.syntax.get(&name).copied().unwrap_or_default();
                    self.syntax.insert(name, spec.patch(base));
                }
                Err(e) => errors.push(format!("{err_prefix}syntax.{name}: {e}")),
            }
        }
    }

    /// The theme `config` asks for: by default (`auto`) the palette of the
    /// current Omarchy theme when `omarchy` (Omarchy's `current/`
    /// directory) has one, else GitHub Dark; or the built-in or theme file
    /// named by `ui.theme` (a theme file is layered on Darcula). Then
    /// `config_dir/theme.toml` on top, then the 256-color mapping unless
    /// `ui.colors` and `colorterm` (the `$COLORTERM` value) give true color.
    /// Problems are logged and returned for notifications; a missing or
    /// broken theme gives GitHub Dark, a broken `theme.toml` is skipped.
    pub fn load(config: &Config, config_dir: &Path, omarchy: Option<&Path>, colorterm: Option<&str>) -> (Theme, Vec<String>) {
        let r = Theme::load_report(config, config_dir, omarchy, colorterm, OnBadFile::Fallback);
        (r.theme, r.messages)
    }

    /// [`Theme::load`] with a choice of what a whole-document failure (an
    /// unknown `ui.theme` name, a theme file that is missing, unreadable
    /// or not valid TOML, a bad Omarchy palette, a broken `theme.toml`)
    /// does: `Fallback` as at startup, or `Reject` for hot reload, which
    /// sets `rejected` so the caller keeps its current theme. Every problem
    /// is reported either way; skipped entries ("(entry ignored)") never
    /// reject.
    pub fn load_report(
        config: &Config,
        config_dir: &Path,
        omarchy: Option<&Path>,
        colorterm: Option<&str>,
        on_bad: OnBadFile,
    ) -> ThemeLoad {
        const KEEP: &str = "; keeping the current theme";
        const FALLBACK: &str = "; using github-dark";
        let ansi256 = !config.ui.colors.truecolor(colorterm);
        let mut messages = Vec::new();
        let mut rejected = false;
        let mut whole = |m: String, fallback: &str, messages: &mut Vec<String>| match on_bad {
            OnBadFile::Fallback => messages.push(format!("{m}{fallback}")),
            OnBadFile::Reject => {
                rejected = true;
                messages.push(format!("{m}{KEEP}"));
            }
        };
        let source = resolve_theme(&config.ui.theme, config_dir, omarchy);
        let mut theme = match &source {
            ThemeSource::Builtin(b) => Theme::builtin(b, ansi256),
            ThemeSource::File(path) => {
                let mut t = Theme::builtin(&DARCULA, ansi256);
                match apply_file(&mut t, path, ansi256) {
                    Ok(entries) => {
                        messages.extend(entries);
                        t
                    }
                    Err(FileError::Missing(m) | FileError::Rejected(m)) => {
                        whole(m, FALLBACK, &mut messages);
                        Theme::builtin(&GITHUB_DARK, ansi256)
                    }
                }
            }
            ThemeSource::Omarchy(dir) => match Theme::omarchy(&dir.join(omarchy::THEME_DIR), ansi256) {
                Ok(t) => t,
                Err(m) => {
                    whole(format!("Omarchy theme: {m}"), FALLBACK, &mut messages);
                    Theme::builtin(&GITHUB_DARK, ansi256)
                }
            },
            ThemeSource::Unknown(name) => {
                let m = if name.eq_ignore_ascii_case(OMARCHY) {
                    "ui.theme: no Omarchy theme found".to_string()
                } else {
                    let names: Vec<&str> = BUILTIN_THEMES.iter().map(|b| b.name).collect();
                    format!(
                        "ui.theme: unknown theme {name:?} ({AUTO}, {OMARCHY}, built-in: {}; a theme file is a path ending in .toml)",
                        names.join(", ")
                    )
                };
                whole(m, FALLBACK, &mut messages);
                Theme::builtin(&GITHUB_DARK, ansi256)
            }
        };
        let mut overrides = Vec::new();
        if let ThemeSource::Omarchy(dir) = &source {
            overrides.push(dir.join(omarchy::THEME_DIR).join(omarchy::JETL_FILE));
        }
        overrides.push(config_dir.join("theme.toml"));
        for path in overrides {
            match apply_file(&mut theme, &path, ansi256) {
                Ok(entries) => messages.extend(entries),
                Err(FileError::Missing(_)) => {}
                Err(FileError::Rejected(m)) => whole(m, "; its overrides are not applied", &mut messages),
            }
        }
        if ansi256 {
            // Catches the RGB colors of theme files and theme.toml.
            theme.map_colors(color::to_ansi256);
        }
        for m in &messages {
            tracing::error!("{m}");
        }
        ThemeLoad { theme, messages, rejected }
    }

    /// The theme rendered from the palette of the Omarchy theme directory
    /// `theme_dir`, before its `jetl.toml`.
    fn omarchy(theme_dir: &Path, ansi256: bool) -> Result<Theme, String> {
        Theme::from_palette(&Palette::read(theme_dir)?, ansi256)
    }

    /// The Omarchy template rendered with `palette`.
    fn from_palette(palette: &Palette, ansi256: bool) -> Result<Theme, String> {
        let text = palette.render(omarchy::TEMPLATE)?;
        let mut t = Theme::blank();
        let errors = t.apply_toml(&text, ansi256).map_err(|e| error_summary(&e.to_string()))?;
        debug_assert!(errors.is_empty(), "omarchy template: {errors:?}");
        t.separate_highlights(ansi256);
        Ok(t)
    }

    /// Make the highlights of each [`HIGHLIGHT_GROUPS`] group differ as
    /// drawn (after the 256-color mapping when `ansi256`): a color equal to
    /// an earlier one of its group moves toward `fg` in small steps. Any
    /// palette can give equal mixes, and the coarse 256-color gray ramp
    /// often does.
    fn separate_highlights(&mut self, ansi256: bool) {
        let Color::Rgb(fr, fg, fb) = self.fg else { return };
        let shown = |c: Color| if ansi256 { color::to_ansi256(c) } else { c };
        for group in HIGHLIGHT_GROUPS {
            for j in 1..group.len() {
                for _ in 0..20 {
                    let c = *group[j](self);
                    if !(0..j).any(|i| shown(*group[i](self)) == shown(c)) {
                        break;
                    }
                    // Fixed palette indices ([colors256]) stay.
                    let Color::Rgb(r, g, b) = c else { break };
                    let step = |x: u8, y: u8| (x as i32 + (y as i32 - x as i32).signum() * ((y as i32 - x as i32).abs() / 16).max(1)) as u8;
                    *group[j](self) = Color::Rgb(step(r, fr), step(g, fg), step(b, fb));
                }
            }
        }
    }

    /// The files the theme is read from, in layer order: the `ui.theme`
    /// file (when it names one) or the Omarchy files (`theme.name`, which
    /// changes on every theme switch, the palette and the theme's
    /// `jetl.toml`), then `config_dir/theme.toml`; no path twice.
    pub fn sources(config: &Config, config_dir: &Path, omarchy: Option<&Path>) -> Vec<PathBuf> {
        let mut out = Vec::new();
        match resolve_theme(&config.ui.theme, config_dir, omarchy) {
            ThemeSource::File(p) => out.push(p),
            ThemeSource::Omarchy(dir) => {
                let theme_dir = dir.join(omarchy::THEME_DIR);
                out.extend([dir.join(omarchy::NAME_FILE), theme_dir.join(omarchy::COLORS_FILE), theme_dir.join(omarchy::JETL_FILE)]);
            }
            ThemeSource::Builtin(_) | ThemeSource::Unknown(_) => {}
        }
        let toml = config_dir.join("theme.toml");
        if !out.contains(&toml) {
            out.push(toml);
        }
        out
    }
}

/// What [`Theme::load_report`] does with a document that cannot be applied
/// as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnBadFile {
    /// Startup: GitHub Dark for a bad `ui.theme`, no overrides for a bad
    /// `theme.toml` or `jetl.toml`.
    Fallback,
    /// Hot reload: report it and set [`ThemeLoad::rejected`].
    Reject,
}

/// The result of [`Theme::load_report`].
#[derive(Debug)]
pub struct ThemeLoad {
    pub theme: Theme,
    /// Problems for notifications, already logged.
    pub messages: Vec<String>,
    /// With [`OnBadFile::Reject`]: a document failed as a whole, so `theme`
    /// is not what the files ask for.
    pub rejected: bool,
}

/// Why a theme document was not applied at all; the message starts with
/// its path.
enum FileError {
    /// The file does not exist.
    Missing(String),
    /// It cannot be read or is not valid TOML.
    Rejected(String),
}

/// Apply the theme document at `path`. Returns the messages of skipped
/// entries; on `Err` the theme is unchanged.
fn apply_file(theme: &mut Theme, path: &Path, ansi256: bool) -> Result<Vec<String>, FileError> {
    let p = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(FileError::Missing(format!("{p}: {e}"))),
        Err(e) => return Err(FileError::Rejected(format!("{p}: {e}"))),
    };
    match theme.apply_toml(&text, ansi256) {
        Ok(errors) => Ok(errors.iter().map(|e| format!("{p}: {e} (entry ignored)")).collect()),
        Err(e) => Err(FileError::Rejected(format!("{p}: {}", error_summary(&e.to_string())))),
    }
}

/// A color of a theme, for [`HIGHLIGHT_GROUPS`].
type Slot = fn(&mut Theme) -> &mut Color;

/// The background of a style (set to `Reset` when it has none).
fn style_bg(s: &mut Style) -> &mut Color {
    s.bg.get_or_insert(Color::Reset)
}

/// Backgrounds drawn over or beside one another, which must differ: each
/// group starts with the background they sit on.
const HIGHLIGHT_GROUPS: &[&[Slot]] = &[
    &[
        |t| &mut t.bg,
        |t| &mut t.current_line_bg,
        |t| &mut t.selection_bg,
        |t| &mut t.search_match_bg,
        |t| &mut t.search_current_bg,
        |t| &mut t.occurrence_bg,
    ],
    &[
        |t| &mut t.bg,
        |t| &mut t.merge_conflict_bg,
        |t| &mut t.diff_add_bg,
        |t| &mut t.diff_del_bg,
        |t| &mut t.diff_mod_bg,
        |t| &mut t.merge_resolved_bg,
    ],
    &[
        |t| &mut t.bg,
        |t| &mut t.diff_add_bg,
        |t| &mut t.diff_del_bg,
        |t| &mut t.diff_mod_bg,
        |t| &mut t.diff_add_word_bg,
        |t| &mut t.diff_del_word_bg,
    ],
    &[|t| &mut t.panel_bg, |t| style_bg(&mut t.list_selected), |t| style_bg(&mut t.list_selected_unfocused)],
    &[|t| &mut t.bg, |t| style_bg(&mut t.list_selected), |t| style_bg(&mut t.list_selected_unfocused)],
];

/// Where `ui.theme` points.
#[derive(Debug, PartialEq)]
pub enum ThemeSource {
    Builtin(&'static BuiltinTheme),
    File(PathBuf),
    /// The palette of the current Omarchy theme; holds Omarchy's
    /// `current/` directory.
    Omarchy(PathBuf),
    Unknown(String),
}

/// The default `ui.theme`: Omarchy's palette when there is one, else
/// GitHub Dark.
pub const AUTO: &str = "auto";
/// `ui.theme` for Omarchy's palette only.
pub const OMARCHY: &str = "omarchy";

/// Resolve a `ui.theme` value: empty or `auto` is the Omarchy palette when
/// `omarchy` (Omarchy's `current/` directory) has one, else GitHub Dark;
/// `omarchy` is that palette or unknown; a built-in name matches
/// case-insensitively, and a value with a `/`, a leading `~` or a `.toml`
/// ending is a theme file (`~/` expands; relative to `config_dir`).
pub fn resolve_theme(value: &str, config_dir: &Path, omarchy: Option<&Path>) -> ThemeSource {
    let v = value.trim();
    let palette = || omarchy.filter(|d| omarchy::colors_file(d).is_some()).map(|d| ThemeSource::Omarchy(d.to_path_buf()));
    if v.is_empty() || v.eq_ignore_ascii_case(AUTO) {
        return palette().unwrap_or(ThemeSource::Builtin(&GITHUB_DARK));
    }
    if v.eq_ignore_ascii_case(OMARCHY) {
        return palette().unwrap_or_else(|| ThemeSource::Unknown(v.to_string()));
    }
    if let Some(b) = BUILTIN_THEMES.iter().find(|b| b.name.eq_ignore_ascii_case(v)) {
        return ThemeSource::Builtin(b);
    }
    if v.contains('/') || v.starts_with('~') || v.ends_with(".toml") {
        let p = crate::worktree::expand_tilde(v);
        return ThemeSource::File(if p.is_relative() { config_dir.join(p) } else { p });
    }
    ThemeSource::Unknown(v.to_string())
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
        let errors = t.apply_toml("bg = \"#102030\"\nfg = \"red\"\ndim = \"#ffffff\"\nmerge_conflict_bg = \"#400000\"\n", false).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(t.merge_conflict_bg, Color::Rgb(0x40, 0, 0));
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
        let errors = t.apply_toml(text, false).unwrap();
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

    /// A complete theme document: every field, every built-in capture, no
    /// bad entry in either color mode.
    fn assert_sets_every_key(name: &str, text: &str) {
        use std::collections::BTreeSet;
        let fields: BTreeSet<&str> = Theme::FIELDS.iter().copied().collect();
        let captures: BTreeSet<&str> = BUILTIN_CAPTURES.iter().copied().collect();
        assert_eq!(captures.len(), BUILTIN_CAPTURES.len(), "duplicate capture");
        let raw: toml::Table = toml::from_str(text).unwrap();
        let top: BTreeSet<&str> = raw.keys().map(String::as_str).filter(|k| *k != "syntax" && *k != "colors256").collect();
        assert_eq!(top, fields, "{name}");
        let mut t = Theme::blank();
        let errors = t.apply_toml(text, false).unwrap();
        assert!(errors.is_empty(), "{name}: {errors:?}");
        let errors = Theme::blank().apply_toml(text, true).unwrap();
        assert!(errors.is_empty(), "{name} (256 colors): {errors:?}");
        let set: BTreeSet<&str> = t.syntax.keys().map(String::as_str).collect();
        assert_eq!(set, captures, "{name}");
    }

    #[test]
    fn builtin_themes_set_every_key() {
        for b in BUILTIN_THEMES {
            assert_sets_every_key(b.name, b.toml);
        }
    }

    /// Omarchy fixture palettes and the theme each renders to.
    fn omarchy_renders() -> Vec<(&'static str, String)> {
        use crate::config::omarchy::fixtures;
        [("omarchy dark", fixtures::DARK), ("omarchy light", fixtures::LIGHT)]
            .into_iter()
            .map(|(name, text)| (name, Palette::parse(text, false).unwrap().render(omarchy::TEMPLATE).unwrap()))
            .collect()
    }

    #[test]
    fn omarchy_template_sets_every_key() {
        for (name, text) in omarchy_renders() {
            assert!(!text.lines().any(|l| !l.trim_start().starts_with('#') && l.contains("{{")), "{name}");
            assert_sets_every_key(name, &text);
        }
    }

    #[test]
    fn colors256_applies_only_in_256_mode() {
        assert_eq!(Theme::builtin(&DARCULA, false).diff_add_bg, Color::Rgb(41, 66, 54));
        let mut t = Theme::builtin(&DARCULA, true);
        assert_eq!(t.diff_add_bg, Color::Indexed(22));
        // A later layer's top level beats an earlier layer's [colors256].
        assert!(t.apply_toml("diff_add_bg = \"#00ff00\"", true).unwrap().is_empty());
        t.map_colors(color::to_ansi256);
        assert_eq!(t.diff_add_bg, Color::Indexed(46));
    }

    #[test]
    fn colors256_errors_reported_in_truecolor_mode() {
        let mut t = Theme::darcula();
        let errors = t.apply_toml("[colors256]\nbg = \"#zz\"\nfg = \"22\"\n[colors256.syntax]\nkeyword = 5\n", false).unwrap();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with("colors256.bg:")), "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with("colors256.syntax.keyword:")), "{errors:?}");
        assert_eq!(t, Theme::darcula());
        let errors = t.apply_toml("colors256 = 3\n[colors256x]\n", false).unwrap();
        assert_eq!(errors.len(), 2, "{errors:?}");
        let errors = t.apply_toml("[colors256.colors256]\nbg = \"1\"\n", true).unwrap();
        assert!(errors[0].starts_with("colors256.colors256: unknown theme field"), "{errors:?}");
    }

    /// Highlights drawn over one another or side by side differ, so that
    /// none disappears.
    fn assert_highlights_apart(name: &str, t: &Theme, ansi256: bool) {
        let sel = t.list_selected.bg.unwrap();
        let unf = t.list_selected_unfocused.bg.unwrap();
        let groups: [(&str, Vec<Color>); 5] = [
            ("editor", vec![t.bg, t.current_line_bg, t.selection_bg, t.search_match_bg, t.search_current_bg, t.occurrence_bg]),
            ("merge", vec![t.bg, t.merge_conflict_bg, t.diff_add_bg, t.diff_del_bg, t.diff_mod_bg, t.merge_resolved_bg]),
            ("diff", vec![t.bg, t.diff_add_bg, t.diff_del_bg, t.diff_mod_bg, t.diff_add_word_bg, t.diff_del_word_bg]),
            ("panel lists", vec![t.panel_bg, sel, unf]),
            ("editor lists", vec![t.bg, sel, unf]),
        ];
        for (group, colors) in groups {
            for (i, a) in colors.iter().enumerate() {
                for c in &colors[i + 1..] {
                    assert_ne!(a, c, "{name} ansi256={ansi256} {group}: {colors:?}");
                }
            }
        }
    }

    #[test]
    fn builtin_themes_keep_highlights_apart() {
        for b in BUILTIN_THEMES {
            for ansi256 in [false, true] {
                assert_highlights_apart(b.name, &Theme::builtin(b, ansi256), ansi256);
            }
        }
    }

    #[test]
    fn indent_guide_differs_from_the_backgrounds() {
        use crate::config::omarchy::fixtures;
        let mut themes = Vec::new();
        for b in BUILTIN_THEMES {
            for ansi256 in [false, true] {
                themes.push((b.name, ansi256, Theme::builtin(b, ansi256)));
            }
        }
        for (name, text) in [("omarchy dark", fixtures::DARK), ("omarchy light", fixtures::LIGHT)] {
            let palette = Palette::parse(text, false).unwrap();
            for ansi256 in [false, true] {
                let mut t = Theme::from_palette(&palette, ansi256).unwrap();
                if ansi256 {
                    t.map_colors(color::to_ansi256);
                }
                themes.push((name, ansi256, t));
            }
        }
        for (name, ansi256, t) in themes {
            let fg = t.indent_guide.fg.unwrap_or_else(|| panic!("{name} ansi256={ansi256}: no indent_guide fg"));
            assert_ne!(fg, t.bg, "{name} ansi256={ansi256}");
            assert_ne!(fg, t.current_line_bg, "{name} ansi256={ansi256}");
        }
    }

    #[test]
    fn omarchy_template_keeps_highlights_apart() {
        use crate::config::omarchy::fixtures;
        for (name, text) in [("omarchy dark", fixtures::DARK), ("omarchy light", fixtures::LIGHT)] {
            let palette = Palette::parse(text, false).unwrap();
            for ansi256 in [false, true] {
                let mut t = Theme::from_palette(&palette, ansi256).unwrap();
                if ansi256 {
                    t.map_colors(color::to_ansi256);
                }
                assert_highlights_apart(name, &t, ansi256);
            }
        }
    }

    #[test]
    fn map_colors_reaches_every_color() {
        let mut t = Theme::darcula();
        t.border.underline_color = Some(Color::Rgb(1, 2, 3));
        t.map_colors(|_| Color::Indexed(200));
        t.map_colors(|c| {
            assert_eq!(c, Color::Indexed(200));
            c
        });
        assert_eq!(t.bg, Color::Indexed(200));
        assert_eq!(t.tab.fg, Some(Color::Indexed(200)));
        assert_eq!(t.tab.bg, Some(Color::Indexed(200)));
        assert_eq!(t.border.underline_color, Some(Color::Indexed(200)));
        assert_eq!(t.capture_style("keyword").unwrap().fg, Some(Color::Indexed(200)));
        // Styles without a color stay without one.
        assert_eq!(t.secondary_caret.fg, None);
    }

    #[test]
    fn reverse_flag_adds_and_removes() {
        let mut t = Theme::default();
        assert!(t.secondary_caret.add_modifier.contains(Modifier::REVERSED));
        assert!(t.apply_toml("secondary_caret = { reverse = false }", false).unwrap().is_empty());
        assert!(!t.secondary_caret.add_modifier.contains(Modifier::REVERSED));
        t.dim = Style::default();
        assert!(t.apply_toml("dim = { reverse = true }", false).unwrap().is_empty());
        assert!(t.dim.add_modifier.contains(Modifier::REVERSED));
        let errors = t.apply_toml("bg = { fg = \"#000000\", reverse = true }", false).unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn invalid_values_are_reported_and_skipped() {
        let mut t = Theme::default();
        let text = "bg = \"#zzzzzz\"\nfg = \"#123456\"\nnope = \"#000000\"\ndim = 5\nborder = { fg = \"#000000\", blink = true }\nselection_bg = { fg = \"#000000\", bold = true }\n";
        let errors = t.apply_toml(text, false).unwrap();
        assert_eq!(errors.len(), 5, "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with("bg: invalid color \"#zzzzzz\"")), "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with("nope: unknown theme field")));
        assert_eq!(t.bg, Theme::default().bg);
        assert_eq!(t.fg, Color::Rgb(0x12, 0x34, 0x56));
        // A syntax error rejects the file.
        assert!(Theme::default().apply_toml("bg = ", false).is_err());
    }

    /// A fresh config directory for one test case.
    fn test_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-theme-{case}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn theme_config(theme: &str) -> Config {
        Config::parse(&format!("[ui]\ntheme = {theme:?}\n")).unwrap()
    }

    const TRUE: Option<&str> = Some("truecolor");

    #[test]
    fn resolve_theme_values() {
        let dir = Path::new("/cfg");
        assert_eq!(resolve_theme("", dir, None), ThemeSource::Builtin(&GITHUB_DARK));
        assert_eq!(resolve_theme("Auto", dir, None), ThemeSource::Builtin(&GITHUB_DARK));
        assert_eq!(resolve_theme("omarchy", dir, None), ThemeSource::Unknown("omarchy".into()));
        assert_eq!(resolve_theme("github-dark", dir, None), ThemeSource::Builtin(&GITHUB_DARK));
        assert_eq!(resolve_theme(" darcula ", dir, None), ThemeSource::Builtin(&DARCULA));
        assert_eq!(resolve_theme("LIGHT", dir, None), ThemeSource::Builtin(&LIGHT));
        assert_eq!(resolve_theme("x", dir, None), ThemeSource::Unknown("x".into()));
        assert_eq!(resolve_theme("x.toml", dir, None), ThemeSource::File("/cfg/x.toml".into()));
        assert_eq!(resolve_theme("sub/x", dir, None), ThemeSource::File("/cfg/sub/x".into()));
        assert_eq!(resolve_theme("/abs/x", dir, None), ThemeSource::File("/abs/x".into()));
        if std::env::var_os("HOME").is_some() {
            match resolve_theme("~/x.toml", dir, None) {
                ThemeSource::File(p) => assert!(!p.starts_with("~") && p.ends_with("x.toml"), "{p:?}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn load_default_without_omarchy_is_github_dark() {
        let dir = test_dir("default");
        let (t, msgs) = Theme::load(&Config::default(), &dir, None, TRUE);
        assert!(msgs.is_empty(), "{msgs:?}");
        assert_eq!(t, Theme::builtin(&GITHUB_DARK, false));
        assert_eq!(t.bg, Color::Rgb(0x0d, 0x11, 0x17));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_light_by_name() {
        let dir = test_dir("light");
        for name in ["light", "Light"] {
            let (t, msgs) = Theme::load(&theme_config(name), &dir, None, TRUE);
            assert!(msgs.is_empty(), "{msgs:?}");
            assert_eq!(t, Theme::builtin(&LIGHT, false));
            assert_eq!(t.bg, Color::Rgb(255, 255, 255));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_theme_file_layers_on_darcula() {
        let dir = test_dir("file");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for f in ["mine.toml", "sub/mine.toml"] {
            std::fs::write(dir.join(f), "bg = \"#010203\"\n").unwrap();
        }
        let abs = dir.join("mine.toml").display().to_string();
        for name in ["mine.toml", "sub/mine.toml", abs.as_str()] {
            let (t, msgs) = Theme::load(&theme_config(name), &dir, None, TRUE);
            assert!(msgs.is_empty(), "{name}: {msgs:?}");
            assert_eq!(t.bg, Color::Rgb(1, 2, 3), "{name}");
            assert_eq!(t.fg, Theme::darcula().fg);
        }
        // A bad entry is skipped; the rest applies.
        std::fs::write(dir.join("mine.toml"), "bg = \"#010203\"\nnope = \"red\"\n").unwrap();
        let (t, msgs) = Theme::load(&theme_config("mine.toml"), &dir, None, TRUE);
        assert_eq!(t.bg, Color::Rgb(1, 2, 3));
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("nope: unknown theme field (entry ignored)"), "{msgs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_unknown_name_falls_back() {
        let dir = test_dir("unknown");
        let (t, msgs) = Theme::load(&theme_config("solarized"), &dir, None, TRUE);
        assert_eq!(t, Theme::builtin(&GITHUB_DARK, false));
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("unknown theme") && msgs[0].contains("darcula") && msgs[0].ends_with("using github-dark"), "{msgs:?}");
        let (_, msgs) = Theme::load(&theme_config("omarchy"), &dir, None, TRUE);
        assert_eq!(msgs, ["ui.theme: no Omarchy theme found; using github-dark"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_bad_theme_file_falls_back() {
        let dir = test_dir("badfile");
        std::fs::write(dir.join("broken.toml"), "bg = \"#000000\"\n[[[").unwrap();
        for name in ["gone.toml", "broken.toml"] {
            let (t, msgs) = Theme::load(&theme_config(name), &dir, None, TRUE);
            assert_eq!(t, Theme::builtin(&GITHUB_DARK, false), "{name}");
            assert_eq!(msgs.len(), 1, "{msgs:?}");
            assert!(msgs[0].contains("using github-dark"), "{msgs:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_toml_overrides_light() {
        let dir = test_dir("override");
        std::fs::write(dir.join("theme.toml"), "selection_bg = \"#123456\"\n").unwrap();
        let (t, msgs) = Theme::load(&theme_config("light"), &dir, None, TRUE);
        assert!(msgs.is_empty(), "{msgs:?}");
        assert_eq!(t.selection_bg, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(t.bg, Theme::builtin(&LIGHT, false).bg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_theme_toml_keeps_chosen_theme() {
        let dir = test_dir("broken");
        std::fs::write(dir.join("theme.toml"), "bg = \"#000000\"\n[[[").unwrap();
        let (t, msgs) = Theme::load(&theme_config("light"), &dir, None, TRUE);
        assert_eq!(t, Theme::builtin(&LIGHT, false));
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("its overrides are not applied"), "{msgs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_mode_flags_whole_file_failures() {
        let dir = test_dir("reject");
        let reject = |config: &Config| Theme::load_report(config, &dir, None, TRUE, OnBadFile::Reject);
        std::fs::write(dir.join("theme.toml"), "bg = \"#000000\"\n[[[").unwrap();
        let r = reject(&Config::default());
        assert!(r.rejected);
        assert_eq!(r.messages.len(), 1, "{:?}", r.messages);
        assert!(r.messages[0].contains("theme.toml") && r.messages[0].ends_with("keeping the current theme"), "{:?}", r.messages);
        std::fs::remove_file(dir.join("theme.toml")).unwrap();
        for name in ["mine.toml", "nope"] {
            let r = reject(&theme_config(name));
            assert!(r.rejected, "{name}");
            assert_eq!(r.messages.len(), 1, "{:?}", r.messages);
            assert!(r.messages[0].ends_with("keeping the current theme"), "{:?}", r.messages);
            assert!(!r.messages[0].contains("using github-dark"), "{:?}", r.messages);
        }
        assert!(reject(&theme_config("nope")).messages[0].contains("unknown theme \"nope\""));
        let r = reject(&Config::default());
        assert!(!r.rejected && r.messages.is_empty(), "a missing theme.toml is fine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_mode_applies_entry_errors() {
        let dir = test_dir("reject-entry");
        std::fs::write(dir.join("theme.toml"), "bg = \"#zz\"\nfg = \"#010203\"\n").unwrap();
        let r = Theme::load_report(&Config::default(), &dir, None, TRUE, OnBadFile::Reject);
        assert!(!r.rejected);
        assert_eq!(r.theme.fg, Color::Rgb(1, 2, 3));
        assert_eq!(r.theme.bg, Theme::builtin(&GITHUB_DARK, false).bg);
        assert_eq!(r.messages.len(), 1, "{:?}", r.messages);
        assert!(r.messages[0].ends_with("(entry ignored)"), "{:?}", r.messages);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sources_lists_theme_file_then_theme_toml() {
        let dir = Path::new("/cfg");
        assert_eq!(Theme::sources(&Config::default(), dir, None), [dir.join("theme.toml")]);
        assert_eq!(Theme::sources(&theme_config("light"), dir, None), [dir.join("theme.toml")]);
        assert_eq!(Theme::sources(&theme_config("mine.toml"), dir, None), [dir.join("mine.toml"), dir.join("theme.toml")]);
        assert_eq!(Theme::sources(&theme_config("/abs/x.toml"), dir, None), [PathBuf::from("/abs/x.toml"), dir.join("theme.toml")]);
        assert_eq!(Theme::sources(&theme_config("theme.toml"), dir, None), [dir.join("theme.toml")], "no path twice");
    }

    #[test]
    fn load_maps_to_256_without_truecolor() {
        let dir = test_dir("ansi256");
        std::fs::write(dir.join("theme.toml"), "selection_bg = \"#123456\"\n").unwrap();
        let has_rgb = |t: &mut Theme| {
            let found = std::cell::Cell::new(false);
            t.map_colors(|c| {
                if matches!(c, Color::Rgb(..)) {
                    found.set(true);
                }
                c
            });
            found.get()
        };
        let (mut t, msgs) = Theme::load(&Config::default(), &dir, None, None);
        assert!(msgs.is_empty(), "{msgs:?}");
        assert!(!has_rgb(&mut t));
        assert_eq!(t.diff_add_bg, Color::Indexed(22), "github-dark's [colors256]");
        let (mut t, _) = Theme::load(&Config::parse("[ui]\ncolors = \"truecolor\"\n").unwrap(), &dir, None, None);
        assert!(has_rgb(&mut t));
        let (mut t, _) = Theme::load(&Config::parse("[ui]\ncolors = \"256\"\ntheme = \"light\"\n").unwrap(), &dir, None, TRUE);
        assert!(!has_rgb(&mut t));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An Omarchy `current/` directory in `dir` whose theme has `colors`.
    fn omarchy_dir(dir: &Path, colors: &str) -> PathBuf {
        let current = dir.join("omarchy-current");
        std::fs::create_dir_all(current.join(omarchy::THEME_DIR)).unwrap();
        std::fs::write(current.join(omarchy::THEME_DIR).join(omarchy::COLORS_FILE), colors).unwrap();
        std::fs::write(current.join(omarchy::NAME_FILE), "test\n").unwrap();
        current
    }

    #[test]
    fn auto_follows_omarchy() {
        use crate::config::omarchy::fixtures;
        let dir = test_dir("omarchy-auto");
        let current = omarchy_dir(&dir, fixtures::DARK);
        let cfg = Path::new("/cfg");
        assert_eq!(resolve_theme("auto", cfg, Some(&current)), ThemeSource::Omarchy(current.clone()));
        assert_eq!(resolve_theme("OMARCHY", cfg, Some(&current)), ThemeSource::Omarchy(current.clone()));
        assert_eq!(resolve_theme("darcula", cfg, Some(&current)), ThemeSource::Builtin(&DARCULA));
        assert_eq!(resolve_theme("auto", cfg, Some(&dir)), ThemeSource::Builtin(&GITHUB_DARK), "no palette there");
        let (t, msgs) = Theme::load(&Config::default(), &dir, Some(&current), TRUE);
        assert!(msgs.is_empty(), "{msgs:?}");
        assert_eq!(t.bg, Color::Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(t.capture_style("keyword").unwrap().fg, Some(Color::Rgb(0xf5, 0xc2, 0xe7)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn omarchy_jetl_toml_then_theme_toml_override() {
        use crate::config::omarchy::fixtures;
        let dir = test_dir("omarchy-layers");
        let current = omarchy_dir(&dir, fixtures::DARK);
        let jetl = current.join(omarchy::THEME_DIR).join(omarchy::JETL_FILE);
        std::fs::write(&jetl, "bg = \"#010203\"\nfg = \"#040506\"\n").unwrap();
        std::fs::write(dir.join("theme.toml"), "fg = \"#070809\"\n").unwrap();
        let (t, msgs) = Theme::load(&Config::default(), &dir, Some(&current), TRUE);
        assert!(msgs.is_empty(), "{msgs:?}");
        assert_eq!((t.bg, t.fg), (Color::Rgb(1, 2, 3), Color::Rgb(7, 8, 9)));
        std::fs::write(&jetl, "[[[").unwrap();
        let (t, msgs) = Theme::load(&Config::default(), &dir, Some(&current), TRUE);
        assert_eq!(t.bg, Color::Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("jetl.toml") && msgs[0].ends_with("its overrides are not applied"), "{msgs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_omarchy_palette_falls_back_or_rejects() {
        let dir = test_dir("omarchy-bad");
        let current = omarchy_dir(&dir, "background = \"#000000\"\n");
        let (t, msgs) = Theme::load(&Config::default(), &dir, Some(&current), TRUE);
        assert_eq!(t, Theme::builtin(&GITHUB_DARK, false));
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].starts_with("Omarchy theme: ") && msgs[0].contains("no foreground color"), "{msgs:?}");
        assert!(msgs[0].ends_with("using github-dark"), "{msgs:?}");
        let r = Theme::load_report(&Config::default(), &dir, Some(&current), TRUE, OnBadFile::Reject);
        assert!(r.rejected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sources_of_omarchy_watch_the_theme_name() {
        use crate::config::omarchy::fixtures;
        let dir = test_dir("omarchy-sources");
        let current = omarchy_dir(&dir, fixtures::DARK);
        let theme_dir = current.join(omarchy::THEME_DIR);
        assert_eq!(
            Theme::sources(&Config::default(), &dir, Some(&current)),
            [current.join("theme.name"), theme_dir.join("colors.toml"), theme_dir.join("jetl.toml"), dir.join("theme.toml")]
        );
        assert_eq!(Theme::sources(&theme_config("light"), &dir, Some(&current)), [dir.join("theme.toml")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
