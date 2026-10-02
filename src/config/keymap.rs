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
            if let Err(e) = km.bind_str(ctx, keys, id) {
                tracing::error!("keymap {e}");
            }
        }
        km
    }

    /// Bind (or with `"none"` unbind) one entry. The error names the entry.
    pub fn bind_str(&mut self, ctx: &str, keys: &str, id: &str) -> Result<(), String> {
        let chord = parse_chord(keys).map_err(|e| format!("[{ctx}] {keys:?}: {e}"))?;
        let table = self.tables.entry(ctx.to_string()).or_default();
        if id == "none" || id.is_empty() {
            table.unbind(&chord);
            return Ok(());
        }
        match Action::from_id(id) {
            Some(a) => {
                table.bind(chord, a);
                Ok(())
            }
            None => Err(format!("[{ctx}] {keys:?}: unknown action {id:?}")),
        }
    }

    /// Merge a TOML document on top of this keymap. Nested tables such as
    /// `[git.changes]` and `[mode.normal]` map to dotted context names. A
    /// TOML syntax error rejects the whole document (nothing is merged);
    /// invalid entries are skipped, logged, and returned.
    pub fn merge_toml(&mut self, text: &str) -> Result<Vec<String>> {
        let raw: toml::Table = toml::from_str(text)?;
        fn walk(km: &mut Keymap, prefix: &str, table: &toml::Table, errors: &mut Vec<String>) {
            for (k, v) in table {
                match v {
                    toml::Value::String(s) => {
                        if let Err(e) = km.bind_str(prefix, k, s) {
                            errors.push(e);
                        }
                    }
                    toml::Value::Table(t) => {
                        let ctx = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                        walk(km, &ctx, t, errors);
                    }
                    other => errors.push(format!("[{prefix}] {k:?}: expected an action id string, got {}", other.type_str())),
                }
            }
        }
        let mut errors = Vec::new();
        walk(self, "", &raw, &mut errors);
        for e in &errors {
            tracing::error!("keymap {e}");
        }
        Ok(errors)
    }

    /// The default keymap with the user's `keymap.toml` merged on top, plus
    /// messages for a startup notification. A file that does not parse is
    /// ignored as a whole (defaults only).
    pub fn load(file: &std::path::Path) -> (Keymap, Vec<String>) {
        let mut km = default_keymap();
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (km, Vec::new()),
            Err(e) => {
                tracing::error!("cannot read {}: {e}", file.display());
                return (km, vec![format!("{}: {e}; using the default keymap", file.display())]);
            }
        };
        match km.merge_toml(&text) {
            Ok(errors) => {
                let msgs = errors.iter().map(|e| format!("{}: {e} (entry ignored)", file.display())).collect();
                (km, msgs)
            }
            Err(e) => {
                tracing::error!("invalid {}: {e}; using the default keymap", file.display());
                let msg = format!("{}: {}; using the default keymap", file.display(), crate::config::theme::error_summary(&e.to_string()));
                (default_keymap(), vec![msg])
            }
        }
    }

    pub fn table(&self, ctx: &str) -> Option<&Table> {
        self.tables.get(ctx)
    }

    /// Every binding of an action in the given contexts as display strings
    /// (shortest chords first, then alphabetical, duplicates removed).
    pub fn bindings_for(&self, action: &Action, contexts: &[&str]) -> Vec<String> {
        let mut found: Vec<(usize, String)> = Vec::new();
        for ctx in contexts {
            let Some(t) = self.tables.get(*ctx) else { continue };
            for (keys, a) in &t.bindings {
                if a == action {
                    found.push((keys.len(), keys.iter().map(|c| c.display()).collect::<Vec<_>>().join(" ")));
                }
            }
        }
        found.sort();
        found.dedup();
        found.into_iter().map(|(_, s)| s).collect()
    }

    /// The shortcut shown next to an action (palette, docs): all of its
    /// bindings in `contexts`, joined, or `None` when unbound.
    pub fn shortcut_for(&self, action: &Action, contexts: &[&str]) -> Option<String> {
        let all = self.bindings_for(action, contexts);
        (!all.is_empty()).then(|| all.join(", "))
    }
}

/// Context order in the generated key reference; other contexts follow
/// alphabetically.
const DOC_CONTEXTS: [&str; 10] =
    ["global", "editor", "editor.find", "tree", "git", "worktrees", "palette", "mode.normal", "mode.insert", "mode.visual"];

impl Keymap {
    /// Markdown reference of every binding, grouped by context, plus the
    /// actions without one. `docs/KEYMAP.md` is this output for the default
    /// keymap (`jetl --dump-keymap`); a test keeps it current.
    pub fn markdown(&self) -> String {
        use std::fmt::Write;
        let registry = &*crate::actions::REGISTRY;
        let index = |a: &Action| registry.iter().position(|r| &r.action == a).unwrap_or(usize::MAX);
        let code = |s: &str| if s.contains('`') { format!("`` {s} ``") } else { format!("`{s}`") };
        let mut out = String::new();
        out.push_str("# Key bindings\n\n");
        out.push_str("Generated from the action registry (`src/actions.rs`) and the default keymap\n");
        out.push_str("(`assets/keymap.default.toml`) by `cargo run -q -- --dump-keymap > docs/KEYMAP.md`.\n");
        out.push_str("Do not edit by hand; `cargo test` fails when it is out of date.\n\n");
        out.push_str("Contexts are searched innermost first (for example `editor.find`, then `editor`,\n");
        out.push_str("then `global`). Bindings marked * need the kitty keyboard protocol (foot, kitty,\n");
        out.push_str("WezTerm, Ghostty); legacy terminals and tmux have the listed fallbacks instead.\n");
        out.push_str("Override any of them in `~/.config/jetl/keymap.toml` (see [CONFIG.md](CONFIG.md)).\n");
        let mut contexts: Vec<&str> =
            DOC_CONTEXTS.iter().copied().filter(|c| self.tables.get(*c).is_some_and(|t| !t.bindings.is_empty())).collect();
        let mut rest: Vec<&str> = self
            .tables
            .iter()
            .filter(|(k, t)| !DOC_CONTEXTS.contains(&k.as_str()) && !t.bindings.is_empty())
            .map(|(k, _)| k.as_str())
            .collect();
        rest.sort();
        contexts.extend(rest);
        for ctx in contexts {
            let t = &self.tables[ctx];
            let mut rows: Vec<(usize, usize, String, &Action)> = t
                .bindings
                .iter()
                .map(|(keys, a)| (index(a), keys.len(), keys.iter().map(|c| c.display()).collect::<Vec<_>>().join(" "), a))
                .collect();
            rows.sort_by(|x, y| (x.0, x.1, &x.2).cmp(&(y.0, y.1, &y.2)));
            let _ = write!(out, "\n## {ctx}\n\n| Keys | Action | Description |\n|---|---|---|\n");
            for (_, _, keys, a) in rows {
                let info = registry.iter().find(|r| &r.action == a);
                let kitty = if needs_kitty(&keys) { " *" } else { "" };
                let _ = writeln!(
                    out,
                    "| {}{kitty} | `{}` | {} |",
                    code(&keys),
                    info.map(|i| i.id).unwrap_or("?"),
                    info.map(|i| i.desc).unwrap_or("")
                );
            }
        }
        out.push_str("\n## Without a default binding\n\nReachable from Find Action (F1) or by binding them in `keymap.toml`.\n\n| Action | Description |\n|---|---|\n");
        for r in registry {
            let bound = self.tables.values().any(|t| t.bindings.values().any(|a| a == &r.action));
            if !bound && !matches!(r.action, Action::Palette(_)) {
                let _ = writeln!(out, "| `{}` | {} |", r.id, r.desc);
            }
        }
        out
    }
}

/// Whether a displayed binding only arrives with the kitty keyboard
/// protocol: Ctrl+Shift+letter, Ctrl+? (Ctrl+Shift+/), Ctrl+Tab, Ctrl+Enter,
/// Shift+Enter, Shift+F3, Shift+Esc and Shift Shift.
fn needs_kitty(keys: &str) -> bool {
    keys.split(' ').any(|k| {
        let letter = k.rsplit('+').next().is_some_and(|l| l.len() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()));
        (k.starts_with("Ctrl+") && k.contains("Shift+") && letter)
            || k == "Ctrl+?"
            || k == "Ctrl+Tab"
            || k == "Ctrl+Enter"
            || k == "Shift+Enter"
            || k == "Shift+F3"
            || k == "Shift+Esc"
    }) || keys == "Shift Shift"
}

/// The built-in default keymap (JetBrains-like). Embedded TOML so that the
/// user file uses exactly the same format.
pub const DEFAULT_KEYMAP_TOML: &str = include_str!("../../assets/keymap.default.toml");

pub fn default_keymap() -> Keymap {
    let mut km = Keymap::default();
    match km.merge_toml(DEFAULT_KEYMAP_TOML) {
        Ok(errors) if errors.is_empty() => {}
        Ok(errors) => tracing::error!("built-in keymap has invalid entries: {errors:?}"),
        Err(e) => tracing::error!("built-in keymap failed to parse: {e}"),
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
    fn kitty_ctrl_shift_slash_is_ctrl_question_mark() {
        // With REPORT_ALTERNATE_KEYS crossterm replaces the key with its
        // shifted form and drops SHIFT; spelling the binding ctrl-shift-/
        // would collide with Ctrl+/.
        let km = default_keymap();
        let got = km.table("editor").unwrap().bindings.get(&vec![KeyCombo::normalize(KeyCode::Char('?'), KeyModifiers::CONTROL)]);
        assert_eq!(got, Some(&Action::Editor(crate::actions::EditorAction::ToggleBlockComment)));
        assert_eq!(parse_combo("ctrl-shift-/").unwrap(), parse_combo("ctrl-/").unwrap());
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
    fn keymap_md_is_current() {
        let expected = default_keymap().markdown();
        let actual = include_str!("../../docs/KEYMAP.md");
        assert!(expected == actual, "docs/KEYMAP.md is out of date; run: cargo run -q -- --dump-keymap > docs/KEYMAP.md");
    }

    #[test]
    fn default_keymap_has_no_invalid_entries() {
        let errors = Keymap::default().merge_toml(DEFAULT_KEYMAP_TOML).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn merge_unbinds_with_none() {
        let mut km = default_keymap();
        let quit = parse_chord("ctrl-q").unwrap();
        let errors = km.merge_toml("[global]\n\"ctrl-q\" = \"none\"\n").unwrap();
        assert!(errors.is_empty());
        assert!(!km.table("global").unwrap().bindings.contains_key(&quit));
        // Nothing else lost its binding, and the dispatcher no longer quits.
        assert!(km.shortcut_for(&Action::App(crate::actions::AppAction::Quit), &["global"]).is_none());
        assert!(km.table("global").unwrap().bindings.contains_key(&parse_chord("ctrl-s").unwrap()));
        let mut d = crate::dispatch::Dispatcher::new(false);
        let key = crossterm::event::KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let r = d.dispatch(key, &["editor", "global"], None, &km);
        assert!(matches!(r, crate::dispatch::Dispatch::Unmapped(_)));
    }

    #[test]
    fn user_keymap_errors_are_reported() {
        let mut km = default_keymap();
        let errors =
            km.merge_toml("[global]\n\"ctrl-q\" = \"app.nope\"\n\"ctrl-\" = \"app.quit\"\n\"f2\" = \"app.quit\"\n\"f3\" = 1\n").unwrap();
        assert_eq!(errors.len(), 3, "{errors:?}");
        // Valid entries still apply; the bad one keeps the default.
        assert_eq!(
            km.table("global").unwrap().bindings.get(&parse_chord("f2").unwrap()),
            Some(&Action::App(crate::actions::AppAction::Quit))
        );
        assert!(km.table("global").unwrap().bindings.contains_key(&parse_chord("ctrl-q").unwrap()));
        assert!(km.merge_toml("[global\n").is_err());
    }

    #[test]
    fn load_falls_back_on_broken_file() {
        let dir = std::env::temp_dir().join(format!("jetl-test-keymap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keymap.toml");
        std::fs::write(&file, "[global]\n\"ctrl-q\" = \"none\"\n\"oops").unwrap();
        let (km, msgs) = Keymap::load(&file);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("using the default keymap"));
        assert!(km.table("global").unwrap().bindings.contains_key(&parse_chord("ctrl-q").unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
