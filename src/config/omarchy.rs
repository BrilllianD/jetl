//! The Omarchy desktop theme: its current palette (`colors.toml`) and the
//! jetl theme rendered from it (`assets/themes/omarchy.toml`).
//!
//! `omarchy-theme-set` replaces `current/theme/` (remove, then rename) and
//! then writes `current/theme.name`, so the stable `current/` directory is
//! the one to watch. The palette is resolved like Omarchy's own
//! `omarchy-theme-color`: legacy `color0..color15` names, then shades
//! derived from the base colors.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The theme directory inside Omarchy's `current/`.
pub const THEME_DIR: &str = "theme";
/// Written by `omarchy-theme-set` after each switch.
pub const NAME_FILE: &str = "theme.name";
/// The palette inside the theme directory.
pub const COLORS_FILE: &str = "colors.toml";
/// A theme's own jetl overrides, layered on the rendered palette.
pub const JETL_FILE: &str = "jetl.toml";

/// The template rendered by [`Palette::render`].
pub const TEMPLATE: &str = include_str!("../../assets/themes/omarchy.toml");

/// Omarchy's `current/` directory: `$XDG_STATE_HOME/omarchy/current`
/// (`~/.local/state/...`), or `~/.config/omarchy/current` of older
/// releases. `None` without Omarchy, and always in tests, so they never
/// read the desktop theme.
pub fn current_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".local/state")));
    [state.map(|s| s.join("omarchy/current")), home.map(|h| h.join(".config/omarchy/current"))].into_iter().flatten().find(|d| d.is_dir())
}

/// The palette file of the Omarchy `current/` directory `dir`, if the
/// current theme has one.
pub fn colors_file(dir: &Path) -> Option<PathBuf> {
    Some(dir.join(THEME_DIR).join(COLORS_FILE)).filter(|p| p.is_file())
}

type Rgb = (u8, u8, u8);

/// Palette keys read from `colors.toml`; others are ignored.
const KEYS: &[&str] = &[
    "accent",
    "selection",
    "selection_background",
    "muted",
    "background",
    "dark_background",
    "darker_background",
    "lighter_background",
    "foreground",
    "dark_foreground",
    "light_foreground",
    "bright_foreground",
    "red",
    "yellow",
    "orange",
    "green",
    "cyan",
    "blue",
    "magenta",
    "purple",
    "brown",
    "bright_red",
    "bright_yellow",
    "bright_green",
    "bright_cyan",
    "bright_blue",
    "bright_magenta",
    "bright_purple",
];

/// The ANSI names each semantic color falls back to.
const ANSI: &[(&str, &str)] = &[
    ("background", "color0"),
    ("red", "color1"),
    ("green", "color2"),
    ("yellow", "color3"),
    ("blue", "color4"),
    ("magenta", "color5"),
    ("cyan", "color6"),
    ("foreground", "color7"),
    ("muted", "color8"),
    ("bright_red", "color9"),
    ("bright_green", "color10"),
    ("bright_yellow", "color11"),
    ("bright_blue", "color12"),
    ("bright_magenta", "color13"),
    ("bright_cyan", "color14"),
    ("bright_foreground", "color15"),
];

/// Colors a palette must give (directly or by their ANSI name).
const REQUIRED: &[&str] = &["background", "foreground", "red", "green", "yellow", "blue", "magenta", "cyan"];

/// A resolved Omarchy palette.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    colors: HashMap<String, Rgb>,
    pub light: bool,
}

impl Palette {
    /// The palette of the Omarchy theme directory `theme_dir`. The error
    /// starts with the file's path.
    pub fn read(theme_dir: &Path) -> Result<Palette, String> {
        let path = theme_dir.join(COLORS_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Palette::parse(&text, theme_dir.join("light.mode").exists()).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Resolve a `colors.toml` document; `light_mode_file` is whether a
    /// `light.mode` file sits beside it.
    pub fn parse(text: &str, light_mode_file: bool) -> Result<Palette, String> {
        let raw: toml::Table = toml::from_str(text).map_err(|e| super::theme::error_summary(&e.to_string()))?;
        let mut c: HashMap<String, Rgb> = HashMap::new();
        for (k, v) in &raw {
            let ansi = k.strip_prefix("color").and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| n < 16);
            if !ansi && !KEYS.contains(&k.as_str()) {
                continue;
            }
            let s = v.as_str().ok_or_else(|| format!("{k}: expected a color string"))?;
            c.insert(k.clone(), parse_hex(s).ok_or_else(|| format!("{k}: invalid color {s:?} (expected \"#rrggbb\")"))?);
        }
        for (name, ansi) in ANSI {
            if !c.contains_key(*name)
                && let Some(v) = c.get(*ansi).copied()
            {
                c.insert(name.to_string(), v);
            }
        }
        for (name, alias) in [("magenta", "purple"), ("bright_magenta", "bright_purple")] {
            if !c.contains_key(name)
                && let Some(v) = c.get(alias).copied()
            {
                c.insert(name.into(), v);
            }
        }
        if let Some(missing) = REQUIRED.iter().find(|k| !c.contains_key(**k)) {
            return Err(format!("no {missing} color"));
        }
        let get = |c: &HashMap<String, Rgb>, k: &str| c[k];
        let derive = |c: &mut HashMap<String, Rgb>, k: &str, f: &dyn Fn(&HashMap<String, Rgb>) -> Rgb| {
            if !c.contains_key(k) {
                let v = f(c);
                c.insert(k.into(), v);
            }
        };
        const BLACK: Rgb = (0, 0, 0);
        const WHITE: Rgb = (255, 255, 255);
        derive(&mut c, "accent", &|c| get(c, "blue"));
        derive(&mut c, "orange", &|c| get(c, "yellow"));
        derive(&mut c, "brown", &|c| mix(get(c, "orange"), BLACK, 50));
        derive(&mut c, "light_foreground", &|c| get(c, "foreground"));
        derive(&mut c, "bright_foreground", &|c| get(c, "foreground"));
        derive(&mut c, "muted", &|c| {
            c.get("dark_foreground").copied().unwrap_or_else(|| mix(get(c, "foreground"), get(c, "background"), 45))
        });
        derive(&mut c, "dark_foreground", &|c| get(c, "muted"));
        derive(&mut c, "dark_background", &|c| mix(get(c, "background"), BLACK, 25));
        derive(&mut c, "darker_background", &|c| mix(get(c, "background"), BLACK, 50));
        derive(&mut c, "lighter_background", &|c| mix(get(c, "background"), get(c, "foreground"), 10));
        derive(&mut c, "selection", &|c| {
            c.get("selection_background").copied().unwrap_or_else(|| mix(get(c, "background"), get(c, "blue"), 30))
        });
        for base in ["red", "yellow", "green", "cyan", "blue", "magenta"] {
            derive(&mut c, &format!("bright_{base}"), &|c| mix(get(c, base), WHITE, 20));
        }
        let mode = ["mode", "theme_type"].iter().find_map(|k| raw.get(*k).and_then(|v| v.as_str()));
        let light = match mode {
            Some(m) if m.eq_ignore_ascii_case("light") => true,
            Some(m) if m.eq_ignore_ascii_case("dark") => false,
            _ => light_mode_file || luminance(c["background"]) > 0.5,
        };
        Ok(Palette { colors: c, light })
    }

    /// A palette color by name.
    pub fn get(&self, name: &str) -> Option<Rgb> {
        self.colors.get(name).copied()
    }

    /// `template` with each `{{name}}` replaced by that palette color and
    /// each `{{mix a b pct}}` by `pct` percent of `b` blended into `a`, as
    /// `#rrggbb`; comment lines are copied as they are. A light palette
    /// drops the `[colors256]` table, whose
    /// fixed palette indices suit dark backgrounds only.
    pub fn render(&self, template: &str) -> Result<String, String> {
        let mut out = String::with_capacity(template.len());
        let mut in_colors256 = false;
        for line in template.split_inclusive('\n') {
            let header = line.trim_start();
            if header.starts_with('[') {
                in_colors256 = header.starts_with("[colors256");
            }
            if in_colors256 && self.light {
                continue;
            }
            if header.starts_with('#') {
                out.push_str(line);
                continue;
            }
            let mut rest = line;
            while let Some(start) = rest.find("{{") {
                let end = rest[start..].find("}}").ok_or_else(|| format!("unclosed placeholder in {:?}", line.trim()))? + start;
                out.push_str(&rest[..start]);
                let (r, g, b) = self.placeholder(&rest[start + 2..end])?;
                out.push_str(&format!("#{r:02x}{g:02x}{b:02x}"));
                rest = &rest[end + 2..];
            }
            out.push_str(rest);
        }
        Ok(out)
    }

    fn placeholder(&self, expr: &str) -> Result<Rgb, String> {
        let color = |name: &str| self.get(name).ok_or_else(|| format!("unknown palette color {name:?}"));
        match expr.split_whitespace().collect::<Vec<_>>().as_slice() {
            [name] => color(name),
            ["mix", a, b, pct] => {
                let pct: u8 = pct.parse().ok().filter(|p| *p <= 100).ok_or_else(|| format!("bad mix percentage {pct:?}"))?;
                Ok(mix(color(a)?, color(b)?, pct))
            }
            _ => Err(format!("bad placeholder {{{{{expr}}}}}")),
        }
    }
}

/// `#rrggbb` (the `#` optional).
fn parse_hex(s: &str) -> Option<Rgb> {
    let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// `pct` percent of `b` blended into `a`.
fn mix(a: Rgb, b: Rgb, pct: u8) -> Rgb {
    let ch = |x: u8, y: u8| ((x as u32 * (100 - pct as u32) + y as u32 * pct as u32 + 50) / 100) as u8;
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// Relative luminance in 0..=1 (sRGB weights, no gamma).
fn luminance((r, g, b): Rgb) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// Palettes for tests.
#[cfg(test)]
pub(crate) mod fixtures {
    /// Catppuccin Mocha, as Omarchy ships it.
    pub const DARK: &str = r##"
mode = "dark"
accent = "#89b4fa"
selection = "#45475a"
muted = "#585b70"
background = "#1e1e2e"
dark_background = "#161622"
darker_background = "#101019"
lighter_background = "#313244"
foreground = "#cdd6f4"
dark_foreground = "#6c7086"
light_foreground = "#bac2de"
bright_foreground = "#cdd6f4"
red = "#f38ba8"
yellow = "#f9e2af"
orange = "#f6b6ab"
green = "#a6e3a1"
cyan = "#94e2d5"
blue = "#89b4fa"
magenta = "#f5c2e7"
brown = "#7b5b55"
"##;

    /// A light palette with only the required colors.
    pub const LIGHT: &str = r##"
background = "#fafafa"
foreground = "#383a42"
red = "#e45649"
green = "#50a14f"
yellow = "#c18401"
blue = "#4078f2"
magenta = "#a626a4"
cyan = "#0184bc"
"##;
}

#[cfg(test)]
mod tests {
    use super::fixtures::{DARK, LIGHT};
    use super::*;

    #[test]
    fn reads_semantic_names() {
        let p = Palette::parse(DARK, false).unwrap();
        assert!(!p.light);
        assert_eq!(p.get("background"), Some((0x1e, 0x1e, 0x2e)));
        assert_eq!(p.get("accent"), Some((0x89, 0xb4, 0xfa)));
        assert_eq!(p.get("bright_red"), Some(mix((0xf3, 0x8b, 0xa8), (255, 255, 255), 20)), "derived");
    }

    #[test]
    fn legacy_ansi_names_fill_semantic_ones() {
        let mut text = String::new();
        for i in 0..16u8 {
            text.push_str(&format!("color{i} = \"#{i:02x}{i:02x}{i:02x}\"\n"));
        }
        text.push_str("cursor = \"not a color\"\n");
        let p = Palette::parse(&text, false).unwrap();
        assert_eq!(p.get("background"), Some((0, 0, 0)));
        assert_eq!(p.get("foreground"), Some((7, 7, 7)));
        assert_eq!(p.get("muted"), Some((8, 8, 8)));
        assert_eq!(p.get("bright_blue"), Some((12, 12, 12)));
        assert_eq!(p.get("bright_foreground"), Some((15, 15, 15)));
        assert_eq!(p.get("accent"), p.get("blue"));
    }

    #[test]
    fn missing_or_bad_colors_are_errors() {
        let e = Palette::parse("foreground = \"#ffffff\"", false).unwrap_err();
        assert_eq!(e, "no background color");
        let e = Palette::parse(&LIGHT.replace("#e45649", "#zz"), false).unwrap_err();
        assert!(e.starts_with("red: invalid color"), "{e}");
        assert!(Palette::parse("background = ", false).is_err());
    }

    #[test]
    fn mode_from_key_file_or_luminance() {
        assert!(Palette::parse(LIGHT, false).unwrap().light, "luminance");
        assert!(!Palette::parse(&format!("mode = \"dark\"\n{LIGHT}"), false).unwrap().light);
        assert!(Palette::parse(&format!("theme_type = \"light\"\n{DARK}").replace("mode = \"dark\"\n", ""), false).unwrap().light);
        assert!(Palette::parse(&DARK.replace("mode = \"dark\"\n", ""), true).unwrap().light, "light.mode file");
        assert!(!Palette::parse(&DARK.replace("mode = \"dark\"\n", ""), false).unwrap().light);
    }

    #[test]
    fn render_placeholders() {
        let p = Palette::parse(LIGHT, false).unwrap();
        let out = p.render("bg = \"{{background}}\" x = \"{{ mix background foreground 50 }}\"\n").unwrap();
        assert_eq!(out, "bg = \"#fafafa\" x = \"#999a9e\"\n");
        assert!(p.render("x = \"{{nope}}\"").unwrap_err().contains("nope"));
        assert!(p.render("x = \"{{mix background red 101}}\"").is_err());
        assert!(p.render("x = \"{{background\"").is_err());
    }

    #[test]
    fn light_palettes_drop_colors256() {
        let template = "bg = \"{{background}}\"\n[syntax]\nkeyword = \"{{red}}\"\n[colors256]\nbg = \"22\"\n";
        let dark = Palette::parse(DARK, false).unwrap().render(template).unwrap();
        assert!(dark.contains("[colors256]"));
        let light = Palette::parse(LIGHT, false).unwrap().render(template).unwrap();
        assert_eq!(light, "bg = \"#fafafa\"\n[syntax]\nkeyword = \"#e45649\"\n");
    }

    #[test]
    fn read_reports_the_path() {
        let dir = std::env::temp_dir().join(format!("jetl-test-omarchy-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Palette::read(&dir).unwrap_err().contains(COLORS_FILE));
        std::fs::write(dir.join(COLORS_FILE), DARK.replace("mode = \"dark\"\n", "")).unwrap();
        std::fs::write(dir.join("light.mode"), "").unwrap();
        assert!(Palette::read(&dir).unwrap().light);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
