//! Key combination parsing and the keymap tables.
//!
//! Key string grammar: `[ctrl-][alt-][shift-]<key>` (also `Ctrl+Shift+N`, as
//! keys are displayed; there the case of a letter does not imply Shift) where `<key>` is a single
//! character, `f1`..`f12`, or one of `enter tab esc space backspace delete
//! insert up down left right home end pageup pagedown`. Space-separated
//! combos form a chord. The special token `shift shift` binds a double tap
//! of the Shift key.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use crossterm::event::{KeyCode, KeyModifiers, MediaKeyCode, ModifierKeyCode};

use crate::actions::Action;
use crate::config::FileText;
use crate::config::edit::KeymapEdit;

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
            // NUL, which crossterm reports as Ctrl+Space on Unix.
            KeyCode::Null => {
                mods |= KeyModifiers::CONTROL;
                KeyCode::Char(' ')
            }
            other => other,
        };
        Self { code, mods }
    }

    /// A key as the keymap should see it. Without the kitty keyboard
    /// protocol (`enhanced` false) Ctrl+/ arrives as 0x1F, which crossterm
    /// reports as Ctrl+7 (a real Ctrl+7 sends the same byte), so it is read
    /// as Ctrl+/. With the protocol the two keys are distinct and Ctrl+7
    /// stays itself (numbered bookmark 7).
    pub fn legacy_input(key: crossterm::event::KeyEvent, enhanced: bool) -> crossterm::event::KeyEvent {
        if !enhanced && key.code == KeyCode::Char('7') && key.modifiers == KeyModifiers::CONTROL {
            return crossterm::event::KeyEvent { code: KeyCode::Char('/'), ..key };
        }
        key
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

/// Modifier prefixes of a key string, dash spelling and displayed `+` spelling.
const MODIFIER_PREFIXES: [(&str, KeyModifiers); 17] = [
    ("ctrl-", KeyModifiers::CONTROL),
    ("control-", KeyModifiers::CONTROL),
    ("c-", KeyModifiers::CONTROL),
    ("alt-", KeyModifiers::ALT),
    ("meta-", KeyModifiers::ALT),
    ("a-", KeyModifiers::ALT),
    ("shift-", KeyModifiers::SHIFT),
    ("s-", KeyModifiers::SHIFT),
    ("super-", KeyModifiers::SUPER),
    ("cmd-", KeyModifiers::SUPER),
    ("ctrl+", KeyModifiers::CONTROL),
    ("control+", KeyModifiers::CONTROL),
    ("alt+", KeyModifiers::ALT),
    ("meta+", KeyModifiers::ALT),
    ("shift+", KeyModifiers::SHIFT),
    ("super+", KeyModifiers::SUPER),
    ("cmd+", KeyModifiers::SUPER),
];

/// Parse one combo like `ctrl-shift-n` or, as keys are displayed, `Ctrl+Shift+N`
/// (there the letter's case does not imply Shift).
pub fn parse_combo(s: &str) -> Result<KeyCombo> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty key");
    }
    let mut mods = KeyModifiers::NONE;
    let mut rest = s;
    // A `+` prefix is the displayed spelling (`Ctrl+Shift+N`): there the case of
    // the letter does not imply Shift.
    let mut plus = false;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(&(prefix, m)) = MODIFIER_PREFIXES.iter().find(|(p, _)| lower.starts_with(p)) else { break };
        mods |= m;
        plus |= prefix.ends_with('+');
        rest = &rest[prefix.len()..];
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
                if !plus {
                    mods |= KeyModifiers::SHIFT;
                }
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

    /// The in-memory twin of `config::edit::apply_keymap_edits` for one edit.
    pub fn apply_edit(&mut self, e: &KeymapEdit) {
        match e {
            KeymapEdit::Bind { ctx, chord, id } => {
                if let Some(a) = Action::from_id(id) {
                    self.tables.entry(ctx.clone()).or_default().bind(chord.clone(), a);
                }
            }
            KeymapEdit::Unbind { ctx, chord } => {
                if let Some(t) = self.tables.get_mut(ctx) {
                    t.unbind(chord);
                }
            }
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

    /// The default keymap with `text` (read from `file`) merged on top, and
    /// the messages of skipped entries ("(entry ignored)"). A missing file
    /// gives the defaults. A file that cannot be read or does not parse is
    /// an `Err` starting with the path, with no fallback wording.
    pub fn from_file_text(text: &FileText, file: &Path) -> Result<(Keymap, Vec<String>), String> {
        let mut km = default_keymap();
        let text = match text {
            FileText::Missing => return Ok((km, Vec::new())),
            FileText::Unreadable(e) => return Err(format!("{}: {e}", file.display())),
            FileText::Text(t) => t,
        };
        match km.merge_toml(text) {
            Ok(errors) => {
                let msgs = errors.iter().map(|e| format!("{}: {e} (entry ignored)", file.display())).collect();
                Ok((km, msgs))
            }
            Err(e) => Err(format!("{}: {}", file.display(), crate::config::theme::error_summary(&e.to_string()))),
        }
    }

    /// [`Keymap::from_file_text`] for startup: a whole-file failure gives
    /// the default keymap and a message for a notification.
    pub fn load_text(text: &FileText, file: &Path) -> (Keymap, Vec<String>) {
        match Self::from_file_text(text, file) {
            Ok(loaded) => loaded,
            Err(e) => {
                tracing::error!("{e}; using the default keymap");
                (default_keymap(), vec![format!("{e}; using the default keymap")])
            }
        }
    }

    /// The default keymap with the user's `keymap.toml` merged on top, plus
    /// messages for a startup notification. A file that does not parse is
    /// ignored as a whole (defaults only).
    pub fn load(file: &Path) -> (Keymap, Vec<String>) {
        Self::load_text(&FileText::read(file), file)
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

/// How a binding conflicts with another one; see [`Keymap::conflicts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// The chord is bound to a different action in an ancestor context.
    Shadows,
    /// The chord is a proper prefix of a longer chord bound in the same
    /// context, which therefore never fires.
    Prefix,
}

/// One conflict of a binding (`ctx`, `chord`) with `other` in `other_ctx`.
/// For `Prefix`, `chord` is the shorter chord, `other_ctx == ctx` and
/// `other` the longer chord's action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub ctx: String,
    pub chord: Vec<KeyCombo>,
    pub other_ctx: String,
    pub other: Action,
    pub kind: ConflictKind,
}

/// Contexts whose bindings `ctx` overrides: dotted prefixes from the
/// longest, then `global`; `terminal` and `mode.*` only `global`.
fn ancestors(ctx: &str) -> Vec<&str> {
    if ctx == "global" {
        return Vec::new();
    }
    let mut out: Vec<&str> = Vec::new();
    if ctx != "terminal" && !ctx.starts_with("mode.") {
        out.extend(ctx.rmatch_indices('.').map(|(i, _)| &ctx[..i]));
    }
    out.push("global");
    out
}

impl Keymap {
    /// Bindings that hide or are hidden by another one, computed from
    /// `bindings` only, never from `Table::prefixes`, which may be stale
    /// after `Table::unbind`. Contexts sorted by name, chords by
    /// `(len, text)`; per chord its `Shadows` (nearest ancestor first), then
    /// its `Prefix` entries.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut ctxs: Vec<&String> = self.tables.keys().collect();
        ctxs.sort();
        let mut out = Vec::new();
        for ctx in ctxs {
            let mut list: Vec<(&Vec<KeyCombo>, &Action)> = self.tables[ctx].bindings.iter().collect();
            list.sort_by_cached_key(|(c, _)| (c.len(), chord_text(c)));
            for (chord, action) in &list {
                for anc in ancestors(ctx) {
                    if let Some(o) = self.table(anc).and_then(|t| t.bindings.get(*chord))
                        && o != *action
                    {
                        out.push(Conflict {
                            ctx: ctx.clone(),
                            chord: (*chord).clone(),
                            other_ctx: anc.to_string(),
                            other: o.clone(),
                            kind: ConflictKind::Shadows,
                        });
                    }
                }
                for (long, la) in &list {
                    if long.len() > chord.len() && long.starts_with(chord) {
                        out.push(Conflict {
                            ctx: ctx.clone(),
                            chord: (*chord).clone(),
                            other_ctx: ctx.clone(),
                            other: (*la).clone(),
                            kind: ConflictKind::Prefix,
                        });
                    }
                }
            }
        }
        out
    }
}

/// Key text of a chord (rows, written TOML keys, input prefill): the
/// [`KeyCombo::display`] of each step joined by spaces.
pub fn chord_text(chord: &[KeyCombo]) -> String {
    chord.iter().map(KeyCombo::display).collect::<Vec<_>>().join(" ")
}

/// The spelling written to `keymap.toml` and prefilled in the key input:
/// [`chord_text`], except that a letter without modifiers stays lower-case
/// (a bare capital letter would read back as Shift+letter). Always parses back
/// with [`parse_chord`].
pub fn chord_key(chord: &[KeyCombo]) -> String {
    chord
        .iter()
        .map(|c| match c.code {
            KeyCode::Char(ch) if c.mods.is_empty() && ch.is_alphabetic() => ch.to_string(),
            _ => c.display(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Context order in the generated key reference; other contexts follow
/// alphabetically.
const DOC_CONTEXTS: [&str; 13] = [
    "global",
    "editor",
    "editor.find",
    "editor.merge",
    "editor.snippet",
    "editor.snippet_prefix",
    "tree",
    "git",
    "worktrees",
    "palette",
    "mode.normal",
    "mode.insert",
    "mode.visual",
];

impl Keymap {
    /// Contexts with at least one binding in the order of the key reference:
    /// [`DOC_CONTEXTS`] first, then the others alphabetically. Shared by
    /// [`Keymap::markdown`] and the Settings Keymap page.
    pub fn context_order(&self) -> Vec<&str> {
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
        contexts
    }

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
        out.push_str("WezTerm, Ghostty); legacy terminals and tmux have the listed fallbacks instead\n");
        out.push_str("(where Alt+Shift+Tab does not arrive, Alt+Tab cycles the tab groups).\n");
        out.push_str("Override any of them in `~/.config/jetl/keymap.toml` (see [CONFIG.md](CONFIG.md)).\n");
        out.push_str("While the Terminal window has focus only its `terminal` table is searched: every other\n");
        out.push_str("key goes to the shell, and Esc Esc at the shell prompt focuses the editor.\n");
        for ctx in self.context_order() {
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
/// protocol: Ctrl+Shift+letter, Ctrl+? (Ctrl+Shift+/), Ctrl+" (Ctrl+Shift+'),
/// Ctrl with `-`, `=`, `+`, `_` or `.`, Ctrl with a digit other than 4-6 (legacy terminals send
/// those as Ctrl+\, Ctrl+] and Ctrl+^; Ctrl+7 is Ctrl+/ there, which jetl
/// reads as Ctrl+/: [`KeyCombo::legacy_input`]), Ctrl+Tab, Ctrl+Enter,
/// Shift+Enter, Shift+F3, Shift+Esc, Alt+Shift+Tab, Ctrl+Shift+Tab and Shift Shift.
fn needs_kitty(keys: &str) -> bool {
    keys.split(' ').any(|k| {
        let key = k.rsplit('+').next().unwrap_or("");
        let letter = key.len() == 1 && key.chars().all(|c| c.is_ascii_alphabetic());
        // `Ctrl++` splits into an empty last part.
        let punct = matches!(key, "-" | "=" | "_" | ".") || k.ends_with("++");
        (k.starts_with("Ctrl+") && k.contains("Shift+") && letter)
            || (k.starts_with("Ctrl+") && punct)
            || k == "Ctrl+Shift+Tab"
            || matches!(k, "Ctrl+0" | "Ctrl+1" | "Ctrl+2" | "Ctrl+3" | "Ctrl+7" | "Ctrl+8" | "Ctrl+9")
            || matches!(k, "Ctrl+?" | "Ctrl+\"")
            || k == "Ctrl+Tab"
            || k == "Ctrl+Enter"
            || k == "Shift+Enter"
            || k == "Shift+F3"
            || k == "Shift+Esc"
            || k == "Alt+Shift+Tab"
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
    fn conflicts_shadowing_prefix_and_ancestors() {
        let ch = |s: &str| parse_chord(s).unwrap();
        let a = |id: &str| Action::from_id(id).unwrap();
        let mut km = Keymap::from_triples([
            ("global", "f5", "app.refresh"),
            ("global", "ctrl-k", "app.quit"),
            ("global", "ctrl-k ctrl-c", "app.settings"),
            ("global", "f6", "app.about"),
            ("tree", "f5", "tree.refresh"),
            ("tree", "f6", "app.about"),
            ("git", "f5", "git.refresh"),
            ("git.changes", "f5", "git.refresh"),
            ("mode", "f7", "app.about"),
            ("mode.normal", "f7", "app.quit"),
        ]);
        let c = |ctx: &str, chord: &str, other_ctx: &str, id: &str, kind| Conflict {
            ctx: ctx.into(),
            chord: ch(chord),
            other_ctx: other_ctx.into(),
            other: a(id),
            kind,
        };
        let want = vec![
            c("git", "f5", "global", "app.refresh", ConflictKind::Shadows),
            c("git.changes", "f5", "global", "app.refresh", ConflictKind::Shadows),
            c("global", "ctrl-k", "global", "app.settings", ConflictKind::Prefix),
            c("tree", "f5", "global", "app.refresh", ConflictKind::Shadows),
        ];
        assert_eq!(km.conflicts(), want);

        km.bind_str("global", "f7", "app.settings").unwrap();
        let all = km.conflicts();
        let normal: Vec<_> = all.iter().filter(|c| c.ctx == "mode.normal").collect();
        assert_eq!(normal.len(), 1);
        assert_eq!((normal[0].other_ctx.as_str(), normal[0].kind), ("global", ConflictKind::Shadows));
        let mode: Vec<_> = all.iter().filter(|c| c.ctx == "mode").collect();
        assert_eq!(mode.len(), 1);
        assert_eq!(mode[0].other_ctx, "global");
    }

    #[test]
    fn conflicts_ignore_stale_prefixes() {
        let ch = |s: &str| parse_chord(s).unwrap();
        let mut km = Keymap::from_triples([("global", "ctrl-k", "app.quit"), ("global", "ctrl-k ctrl-c", "app.settings")]);
        assert_eq!(km.conflicts().len(), 1);
        km.tables.get_mut("global").unwrap().unbind(&ch("ctrl-k ctrl-c"));
        assert!(km.conflicts().is_empty());
    }

    #[test]
    fn default_keymap_has_no_prefix_conflicts() {
        let ch = |s: &str| parse_chord(s).unwrap();
        let all = default_keymap().conflicts();
        assert!(all.iter().all(|c| c.kind != ConflictKind::Prefix));
        assert!(
            all.iter().any(|c| c.kind == ConflictKind::Shadows
                && c.ctx == "editor.merge"
                && c.chord == ch("alt-enter")
                && c.other_ctx == "editor")
        );
    }

    #[test]
    fn context_order_lists_doc_contexts_first() {
        let km = default_keymap();
        let order = km.context_order();
        assert_eq!(order[0], "global");
        let mut seen = std::collections::HashSet::new();
        for c in &order {
            assert!(!km.tables[*c].bindings.is_empty(), "{c}");
            assert!(seen.insert(*c), "{c} repeats");
        }
        let last_doc = order.iter().rposition(|c| DOC_CONTEXTS.contains(c)).unwrap();
        let rest = &order[last_doc + 1..];
        assert!(rest.iter().all(|c| !DOC_CONTEXTS.contains(c)));
        let mut sorted = rest.to_vec();
        sorted.sort();
        assert_eq!(rest, sorted);
    }

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
    fn legacy_ctrl_7_is_ctrl_slash_only_without_kitty() {
        use crossterm::event::KeyEvent;
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(KeyCombo::legacy_input(ctrl('7'), false), ctrl('/'));
        assert_eq!(KeyCombo::legacy_input(ctrl('7'), true), ctrl('7'));
        assert_eq!(KeyCombo::legacy_input(ctrl('/'), false), ctrl('/'));
        let alt7 = KeyEvent::new(KeyCode::Char('7'), KeyModifiers::ALT);
        assert_eq!(KeyCombo::legacy_input(alt7, false), alt7);
        let plain = KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE);
        assert_eq!(KeyCombo::legacy_input(plain, false), plain);
        // The editor table no longer binds Ctrl+7: with kitty it reaches
        // the global numbered bookmark.
        let km = default_keymap();
        let ctrl7 = parse_chord("ctrl-7").unwrap();
        assert_eq!(km.table("editor").unwrap().bindings.get(&ctrl7), None);
        assert_eq!(
            km.table("global").unwrap().bindings.get(&ctrl7),
            Some(&Action::Editor(crate::actions::EditorAction::GotoNumberedBookmark(7)))
        );
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

    /// Actions whose primary key needs the kitty protocol must also have a key
    /// that legacy terminals and tmux deliver: (action id, contexts).
    const LEGACY_REACHABLE: &[(&str, &[&str])] = &[
        ("editor.redo", &["editor"]),
        ("editor.fold_selection", &["editor"]),
        ("editor.fold_collapse_recursive", &["editor"]),
        ("editor.fold_expand_recursive", &["editor"]),
        ("editor.save_all", &["global"]),
        ("git.push", &["global"]),
        ("lsp.goto_type_definition", &["editor"]),
        ("tool_window.hide_focused", &["global"]),
        ("terminal.new_tab", &["terminal"]),
    ];

    #[test]
    fn kitty_only_actions_have_a_legacy_key() {
        let km = default_keymap();
        for (id, contexts) in LEGACY_REACHABLE {
            let info = crate::actions::REGISTRY.iter().find(|a| a.id == *id).unwrap_or_else(|| panic!("unknown action {id}"));
            let keys = km.bindings_for(&info.action, contexts);
            assert!(keys.iter().any(|k| !needs_kitty(k)), "{id} has no legacy-terminal key, only {keys:?}");
        }
    }

    #[test]
    fn kitty_only_fold_keys_are_marked() {
        for brace in ["{", "}"] {
            let combo = parse_combo(&format!("alt-{brace}")).unwrap();
            assert_eq!(combo.code, KeyCode::Char(brace.chars().next().unwrap()));
            assert!(combo.mods.contains(KeyModifiers::ALT));
            assert_eq!(combo.display(), format!("Alt+{brace}"));
        }
        for k in ["Ctrl+-", "Ctrl+=", "Ctrl++", "Ctrl+Alt+-", "Ctrl+Alt+=", "Ctrl+_", "Ctrl+."] {
            assert!(needs_kitty(k), "{k}");
        }
        for k in ["Alt+Shift+F", "Alt+{", "Alt+}"] {
            assert!(!needs_kitty(k), "{k}");
        }
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
    fn keymap_from_file_text() {
        let file = Path::new("/cfg/keymap.toml");
        let e = Keymap::from_file_text(&FileText::Text("[global\n".into()), file).unwrap_err();
        assert!(e.starts_with("/cfg/keymap.toml: ") && !e.contains("default keymap"), "{e}");
        assert!(Keymap::from_file_text(&FileText::Unreadable("denied".into()), file).is_err());
        let (km, msgs) =
            Keymap::from_file_text(&FileText::Text("[global]\n\"f3\" = \"no.such\"\n\"f4\" = \"app.about\"\n".into()), file).unwrap();
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].ends_with("(entry ignored)"), "{msgs:?}");
        assert_eq!(
            km.table("global").unwrap().bindings.get(&parse_chord("f4").unwrap()),
            Some(&Action::App(crate::actions::AppAction::About))
        );
        let (km, msgs) = Keymap::from_file_text(&FileText::Missing, file).unwrap();
        assert!(msgs.is_empty());
        assert!(km.table("global").unwrap().bindings.contains_key(&parse_chord("ctrl-q").unwrap()));
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

    #[test]
    fn display_spelling_parses_back() {
        for (shown, dash) in [
            ("Ctrl+Q", "ctrl-q"),
            ("ctrl+q", "ctrl-q"),
            ("Ctrl+Shift+F", "ctrl-shift-f"),
            ("Ctrl++", "ctrl-plus"),
            ("Alt+Shift+Tab", "alt-shift-tab"),
            ("Ctrl+Space", "ctrl-space"),
            ("Ctrl+Alt+-", "ctrl-alt-minus"),
        ] {
            assert_eq!(parse_combo(shown).unwrap(), parse_combo(dash).unwrap(), "{shown}");
        }
        assert!(parse_combo("Q").unwrap().mods.contains(KeyModifiers::SHIFT));
        assert!(parse_combo("ctrl-Q").unwrap().mods.contains(KeyModifiers::SHIFT));
        assert_eq!(parse_chord("Shift Shift").unwrap(), vec![KeyCombo::double_shift()]);
        assert!(parse_combo("Ctrl+").is_err());
    }

    #[test]
    fn chord_key_round_trips_every_default_binding() {
        let bare_letter = |c: &KeyCombo| c.mods.is_empty() && matches!(c.code, KeyCode::Char(ch) if ch.is_alphabetic());
        for (ctx, table) in &default_keymap().tables {
            for chord in table.bindings.keys() {
                assert_eq!(&parse_chord(&chord_key(chord)).unwrap(), chord, "[{ctx}] {}", chord_key(chord));
                if !chord.iter().any(bare_letter) {
                    assert_eq!(&parse_chord(&chord_text(chord)).unwrap(), chord, "[{ctx}] {}", chord_text(chord));
                }
            }
        }
        assert_eq!(chord_key(&parse_chord("g g").unwrap()), "g g");
        assert_eq!(chord_key(&parse_chord("ctrl-k ctrl-c").unwrap()), "Ctrl+K Ctrl+C");
    }

    #[test]
    fn apply_edit_binds_and_unbinds() {
        let ch = |s| parse_chord(s).unwrap();
        let mut km = Keymap::from_triples([("global", "ctrl-q", "app.quit")]);
        km.apply_edit(&KeymapEdit::Bind { ctx: "git.changes".into(), chord: ch("f2"), id: "app.about" });
        assert_eq!(km.table("git.changes").unwrap().bindings.get(&ch("f2")), Action::from_id("app.about").as_ref());
        km.apply_edit(&KeymapEdit::Unbind { ctx: "global".into(), chord: ch("ctrl-q") });
        assert!(km.table("global").unwrap().bindings.is_empty());
        km.apply_edit(&KeymapEdit::Unbind { ctx: "nowhere".into(), chord: ch("f2") });
        km.apply_edit(&KeymapEdit::Bind { ctx: "global".into(), chord: ch("f3"), id: "no.such" });
        assert!(km.table("nowhere").is_none());
        assert!(km.table("global").unwrap().bindings.is_empty());
    }
}
