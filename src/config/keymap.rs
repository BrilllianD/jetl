//! Key combination parsing and the keymap tables.
//!
//! Key string grammar: `[ctrl-][alt-][shift-]<key>` where `<key>` is a single
//! character, `f1`..`f12`, or one of `enter tab esc space backspace delete
//! insert up down left right home end pageup pagedown`. Space-separated
//! combos form a chord. The special token `shift shift` binds a double tap
//! of the Shift key.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{KeyCode, KeyModifiers, MediaKeyCode, ModifierKeyCode};

use crate::actions::Action;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeyCombo {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }
    }

    /// Normalize a key event so that keymap lookups do not depend on the
    /// terminal's keyboard protocol: uppercase letters become lowercase with
    /// SHIFT set, and irrelevant modifier bits are stripped.
    pub fn normalize(code: KeyCode, mods: KeyModifiers) -> Self {
        let mut mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER);
        let code = match code {
            KeyCode::Char(c) if c.is_uppercase() => {
                mods |= KeyModifiers::SHIFT;
                KeyCode::Char(c.to_lowercase().next().unwrap_or(c))
            }
            KeyCode::Char(c) if c != ' ' && !c.is_alphabetic() => {
                // Shift+1 arrives as '!' already; the shift flag is redundant.
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::Char(c)
            }
            KeyCode::BackTab => {
                mods |= KeyModifiers::SHIFT;
                KeyCode::Tab
            }
            other => other,
        };
        Self { code, mods }
    }

    /// Double tap of Shift is represented by this pseudo combo.
    pub fn double_shift() -> Self {
        Self { code: KeyCode::Modifier(ModifierKeyCode::LeftShift), mods: KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL }
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push_str("Ctrl+");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("Alt+");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("Shift+");
        }
        if *self == Self::double_shift() {
            return "Shift Shift".into();
        }
        let k = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_uppercase().to_string(),
            KeyCode::F(n) => format!("F{n}"),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Tab => "Tab".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Insert => "Insert".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            other => format!("{other:?}"),
        };
        s.push_str(&k);
        s
    }
}

/// Parse one combo like `ctrl-shift-n`.
pub fn parse_combo(s: &str) -> Result<KeyCombo> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty key");
    }
    let mut mods = KeyModifiers::NONE;
    let mut rest = s;
    loop {
        let lower = rest.to_ascii_lowercase();
        if let Some(r) = lower.strip_prefix("ctrl-").or_else(|| lower.strip_prefix("control-")).or_else(|| lower.strip_prefix("c-")) {
            mods |= KeyModifiers::CONTROL;
            rest = &rest[rest.len() - r.len()..];
        } else if let Some(r) = lower.strip_prefix("alt-").or_else(|| lower.strip_prefix("meta-")).or_else(|| lower.strip_prefix("a-")) {
            mods |= KeyModifiers::ALT;
            rest = &rest[rest.len() - r.len()..];
        } else if let Some(r) = lower.strip_prefix("shift-").or_else(|| lower.strip_prefix("s-")) {
            mods |= KeyModifiers::SHIFT;
            rest = &rest[rest.len() - r.len()..];
        } else if let Some(r) = lower.strip_prefix("super-").or_else(|| lower.strip_prefix("cmd-")) {
            mods |= KeyModifiers::SUPER;
            rest = &rest[rest.len() - r.len()..];
        } else {
            break;
        }
        if rest.is_empty() {
            bail!("missing key after modifier in {s:?}");
        }
    }
    let key = rest.to_ascii_lowercase();
    let code = match key.as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "backtick" | "grave" => KeyCode::Char('`'),
        "minus" => KeyCode::Char('-'),
        "plus" => KeyCode::Char('+'),
        "slash" => KeyCode::Char('/'),
        "backslash" => KeyCode::Char('\\'),
        "play" => KeyCode::Media(MediaKeyCode::Play),
        k if k.len() > 1 && k.starts_with('f') && k[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u8 = k[1..].parse().map_err(|_| anyhow!("bad function key {k:?}"))?;
            if n == 0 || n > 24 {
                bail!("bad function key {k:?}");
            }
            KeyCode::F(n)
        }
        k => {
            let mut chars = rest.chars();
            let c = chars.next().unwrap();
            if chars.next().is_some() {
                bail!("unknown key {k:?}");
            }
            if c.is_uppercase() {
                mods |= KeyModifiers::SHIFT;
                KeyCode::Char(c.to_lowercase().next().unwrap_or(c))
            } else {
                KeyCode::Char(c)
            }
        }
    };
    Ok(KeyCombo::normalize(code, mods))
}

/// Parse a chord string (`"ctrl-k ctrl-x"`, `"g g"`, `"shift shift"`).
pub fn parse_chord(s: &str) -> Result<Vec<KeyCombo>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("shift shift") {
        return Ok(vec![KeyCombo::double_shift()]);
    }
    let mut out = Vec::new();
    for part in s.split_whitespace() {
        out.push(parse_combo(part)?);
    }
    if out.is_empty() {
        bail!("empty chord");
    }
    Ok(out)
}

/// One context's bindings: full chords plus the set of proper prefixes.
#[derive(Debug, Default, Clone)]
pub struct Table {
    pub bindings: HashMap<Vec<KeyCombo>, Action>,
    pub prefixes: HashSet<Vec<KeyCombo>>,
}

impl Table {
    pub fn bind(&mut self, chord: Vec<KeyCombo>, action: Action) {
        for n in 1..chord.len() {
            self.prefixes.insert(chord[..n].to_vec());
        }
        self.bindings.insert(chord, action);
    }
    pub fn unbind(&mut self, chord: &[KeyCombo]) {
        self.bindings.remove(chord);
    }
}

/// All keymap tables keyed by context name (`global`, `editor`, `tree`,
/// `git.changes`, `mode.normal`, ...).
#[derive(Debug, Default, Clone)]
pub struct Keymap {
    pub tables: HashMap<String, Table>,
}

impl Keymap {
    /// Build from (context, chord, action-id) triples. Unknown action ids and
    /// unparsable keys are logged and skipped so one typo cannot break startup.
    pub fn from_triples<'a>(triples: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> Self {
        let mut km = Keymap::default();
        for (ctx, keys, id) in triples {
            km.bind_str(ctx, keys, id);
        }
        km
    }

    pub fn bind_str(&mut self, ctx: &str, keys: &str, id: &str) {
        let chord = match parse_chord(keys) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("keymap [{ctx}] {keys:?}: {e}");
                return;
            }
        };
        let table = self.tables.entry(ctx.to_string()).or_default();
        if id == "none" || id.is_empty() {
            table.unbind(&chord);
            return;
        }
        match Action::from_id(id) {
            Some(a) => table.bind(chord, a),
            None => tracing::error!("keymap [{ctx}] {keys:?}: unknown action {id:?}"),
        }
    }

    /// Merge a TOML document on top of this keymap. Nested tables such as
    /// `[git.changes]` and `[mode.normal]` map to dotted context names.
    pub fn merge_toml(&mut self, text: &str) -> Result<()> {
        let raw: toml::Table = toml::from_str(text)?;
        fn walk(km: &mut Keymap, prefix: &str, table: &toml::Table) {
            for (k, v) in table {
                match v {
                    toml::Value::String(s) => km.bind_str(prefix, k, s),
                    toml::Value::Table(t) => {
                        let ctx = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                        walk(km, &ctx, t);
                    }
                    other => tracing::error!("keymap [{prefix}] {k}: expected string, got {}", other.type_str()),
                }
            }
        }
        walk(self, "", &raw);
        Ok(())
    }

    pub fn table(&self, ctx: &str) -> Option<&Table> {
        self.tables.get(ctx)
    }

    /// First binding (as display string) for an action, searching the given
    /// contexts in order. Used by the palette to show shortcuts.
    pub fn shortcut_for(&self, action: &Action, contexts: &[&str]) -> Option<String> {
        for ctx in contexts {
            if let Some(t) = self.tables.get(*ctx) {
                let mut found: Vec<&Vec<KeyCombo>> = t.bindings.iter().filter(|(_, a)| *a == action).map(|(k, _)| k).collect();
                found.sort_by_key(|k| k.len());
                if let Some(k) = found.first() {
                    return Some(k.iter().map(|c| c.display()).collect::<Vec<_>>().join(" "));
                }
            }
        }
        None
    }
}

/// The built-in default keymap (JetBrains-like). Embedded TOML so that the
/// user file uses exactly the same format.
pub const DEFAULT_KEYMAP_TOML: &str = include_str!("../../assets/keymap.default.toml");

pub fn default_keymap() -> Keymap {
    let mut km = Keymap::default();
    if let Err(e) = km.merge_toml(DEFAULT_KEYMAP_TOML) {
        tracing::error!("built-in keymap failed to parse: {e}");
    }
    km
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_keys() {
        let c = parse_combo("ctrl-shift-n").unwrap();
        assert_eq!(c.code, KeyCode::Char('n'));
        assert!(c.mods.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        let c = parse_combo("alt-1").unwrap();
        assert_eq!(c.code, KeyCode::Char('1'));
        assert_eq!(c.mods, KeyModifiers::ALT);
        let c = parse_combo("F12").unwrap();
        assert_eq!(c.code, KeyCode::F(12));
        let c = parse_combo("ctrl-`").unwrap();
        assert_eq!(c.code, KeyCode::Char('`'));
        assert!(parse_combo("ctrl-").is_err());
        assert!(parse_combo("bogus").is_err());
    }

    #[test]
    fn normalizes_uppercase_and_backtab() {
        let a = KeyCombo::normalize(KeyCode::Char('Z'), KeyModifiers::CONTROL);
        let b = parse_combo("ctrl-shift-z").unwrap();
        assert_eq!(a, b);
        let t = KeyCombo::normalize(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(t, parse_combo("shift-tab").unwrap());
    }

    #[test]
    fn chords_and_double_shift() {
        let ch = parse_chord("ctrl-k ctrl-x").unwrap();
        assert_eq!(ch.len(), 2);
        assert_eq!(parse_chord("shift shift").unwrap(), vec![KeyCombo::double_shift()]);
    }

    #[test]
    fn default_keymap_parses_cleanly() {
        let km = default_keymap();
        assert!(km.table("global").is_some());
        assert!(km.table("editor").is_some());
        let quit = km.table("global").unwrap().bindings.get(&parse_chord("ctrl-q").unwrap());
        assert_eq!(quit, Some(&Action::App(crate::actions::AppAction::Quit)));
    }

    #[test]
    fn merge_unbinds_with_none() {
        let mut km = default_keymap();
        km.merge_toml("[global]\n\"ctrl-q\" = \"none\"\n").unwrap();
        assert!(km.table("global").unwrap().bindings.get(&parse_chord("ctrl-q").unwrap()).is_none());
    }
}
