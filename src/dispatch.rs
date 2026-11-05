//! Turns key events into actions using the keymap, the current input mode
//! and the focus context stack.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};

use crate::actions::Action;
use crate::config::keymap::{KeyCombo, Keymap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Insert,
    Normal,
    Visual,
}

impl Mode {
    pub fn table(self) -> &'static str {
        match self {
            Mode::Insert => "mode.insert",
            Mode::Normal => "mode.normal",
            Mode::Visual => "mode.visual",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Mode::Insert => "INSERT",
            Mode::Normal => "NORMAL",
            Mode::Visual => "VISUAL",
        }
    }
}

#[derive(Debug)]
pub enum Dispatch {
    /// A full chord matched.
    Action(Action),
    /// The keys so far are a prefix of a longer chord; wait for more.
    Pending,
    /// No binding; the caller may treat printable chars as text input.
    Unmapped(KeyEvent),
    /// Key release or other event with no meaning.
    Ignore,
}

pub struct Dispatcher {
    pending: Vec<KeyCombo>,
    pending_at: Option<Instant>,
    last_shift: Option<Instant>,
    pub chord_timeout: Duration,
    pub double_tap: Duration,
    /// Whether vim-style mode tables are consulted.
    pub modal: bool,
}

impl Dispatcher {
    pub fn new(modal: bool) -> Self {
        Self {
            pending: Vec::new(),
            pending_at: None,
            last_shift: None,
            chord_timeout: Duration::from_millis(1000),
            double_tap: Duration::from_millis(400),
            modal,
        }
    }

    pub fn pending_display(&self) -> Option<String> {
        if self.pending.is_empty() { None } else { Some(self.pending.iter().map(|c| c.display()).collect::<Vec<_>>().join(" ")) }
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.pending_at = None;
    }

    /// `contexts` is the focus context stack, innermost first, and must end
    /// with `"global"`. `mode` selects the vim mode table (consulted first
    /// when modal); overlays such as the palette pass `None` to skip it.
    pub fn dispatch(&mut self, key: KeyEvent, contexts: &[&str], mode: Option<Mode>, keymap: &Keymap) -> Dispatch {
        // Double-shift detection uses modifier press events (kitty protocol).
        if let KeyCode::Modifier(m) = key.code {
            if matches!(m, ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift) && key.kind == KeyEventKind::Press {
                let now = Instant::now();
                if let Some(prev) = self.last_shift
                    && now.duration_since(prev) <= self.double_tap
                {
                    self.last_shift = None;
                    return self.lookup(KeyCombo::double_shift(), contexts, mode, keymap, key);
                }
                self.last_shift = Some(now);
            }
            return Dispatch::Ignore;
        }
        if key.kind == KeyEventKind::Release {
            return Dispatch::Ignore;
        }
        // Any non-shift key cancels a pending double tap.
        self.last_shift = None;

        if let Some(at) = self.pending_at
            && at.elapsed() > self.chord_timeout
        {
            self.reset();
        }
        let combo = KeyCombo::normalize(key.code, key.modifiers);
        self.lookup(combo, contexts, mode, keymap, key)
    }

    fn lookup(&mut self, combo: KeyCombo, contexts: &[&str], mode: Option<Mode>, keymap: &Keymap, key: KeyEvent) -> Dispatch {
        let mut chord = self.pending.clone();
        chord.push(combo);

        let mut tables: Vec<&str> = Vec::with_capacity(contexts.len() + 1);
        if let Some(mode) = mode.filter(|_| self.modal) {
            tables.push(mode.table());
        }
        tables.extend_from_slice(contexts);

        let mut is_prefix = false;
        for ctx in &tables {
            let Some(t) = keymap.table(ctx) else { continue };
            if let Some(a) = t.bindings.get(&chord) {
                self.reset();
                return Dispatch::Action(a.clone());
            }
            if t.prefixes.contains(&chord) {
                is_prefix = true;
            }
        }
        if is_prefix {
            self.pending = chord;
            self.pending_at = Some(Instant::now());
            return Dispatch::Pending;
        }
        let had_pending = !self.pending.is_empty();
        self.reset();
        if had_pending {
            // An unfinished chord followed by an unrelated key: swallow it,
            // JetBrains-style, rather than inserting text unexpectedly.
            return Dispatch::Ignore;
        }
        Dispatch::Unmapped(key)
    }
}

/// True when an unmapped key event should insert text.
pub fn is_text_input(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(_) => !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{AppAction, EditorAction};
    use crate::config::keymap::default_keymap;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn global_and_context_lookup() {
        let km = default_keymap();
        let mut d = Dispatcher::new(false);
        let r = d.dispatch(key(KeyCode::Char('q'), KeyModifiers::CONTROL), &["editor", "global"], Some(Mode::Insert), &km);
        assert!(matches!(r, Dispatch::Action(Action::App(AppAction::Quit))));
        let r = d.dispatch(key(KeyCode::Char('z'), KeyModifiers::CONTROL), &["editor", "global"], Some(Mode::Insert), &km);
        assert!(matches!(r, Dispatch::Action(Action::Editor(EditorAction::Undo))));
        let r = d.dispatch(key(KeyCode::Char('x'), KeyModifiers::NONE), &["tree", "global"], Some(Mode::Insert), &km);
        assert!(matches!(r, Dispatch::Unmapped(_)));
    }

    #[test]
    fn refactoring_keys_and_their_neighbours() {
        use crate::actions::{LspAction, REGISTRY};
        use crate::lsp::Refactoring::*;
        let km = default_keymap();
        let mut d = Dispatcher::new(false);
        let (ctrl_alt, none) = (KeyModifiers::CONTROL | KeyModifiers::ALT, KeyModifiers::NONE);
        let rows = [
            (key(KeyCode::Char('m'), ctrl_alt), ExtractMethod),
            (key(KeyCode::Char('v'), ctrl_alt), ExtractVariable),
            (key(KeyCode::Char('c'), ctrl_alt), ExtractConstant),
            (key(KeyCode::Char('n'), ctrl_alt), Inline),
            (key(KeyCode::F(6), KeyModifiers::CONTROL), ChangeSignature),
            (key(KeyCode::F(6), none), Move),
            (key(KeyCode::Delete, KeyModifiers::ALT), SafeDelete),
        ];
        for (k, want) in rows {
            let r = d.dispatch(k, &["editor", "global"], Some(Mode::Insert), &km);
            assert!(matches!(r, Dispatch::Action(Action::Lsp(LspAction::Refactoring(got))) if got == want), "{k:?}: {r:?}");
        }
        let id_of = |r: Dispatch| match r {
            Dispatch::Action(a) => REGISTRY.iter().find(|i| i.action == a).map(|i| i.id),
            _ => None,
        };
        let r = d.dispatch(key(KeyCode::F(6), KeyModifiers::SHIFT), &["editor", "global"], Some(Mode::Insert), &km);
        assert_eq!(id_of(r), Some("lsp.rename"));
        let r = d.dispatch(key(KeyCode::Delete, KeyModifiers::ALT), &["find", "global"], Some(Mode::Insert), &km);
        assert_eq!(id_of(r), Some("find.toggle_exclude"));
        let r = d.dispatch(key(KeyCode::Char('n'), ctrl_alt), &["git", "global"], Some(Mode::Insert), &km);
        assert_eq!(id_of(r), Some("git.new_branch"));
        let r = d.dispatch(key(KeyCode::F(6), KeyModifiers::SHIFT), &["tree", "global"], Some(Mode::Insert), &km);
        assert_eq!(id_of(r), Some("tree.rename"));
    }

    #[test]
    fn chords_wait_then_fire() {
        let mut km = default_keymap();
        km.merge_toml("[editor]\n\"ctrl-k ctrl-x\" = \"app.quit\"\n").unwrap();
        let mut d = Dispatcher::new(false);
        let r = d.dispatch(key(KeyCode::Char('k'), KeyModifiers::CONTROL), &["editor", "global"], Some(Mode::Insert), &km);
        // ctrl-k is also bound globally to git.commit_box, but the editor
        // table is searched first and reports a prefix only if no full match
        // exists there. Full matches in outer tables still win over prefixes.
        assert!(matches!(r, Dispatch::Action(_)));
        let mut km = Keymap::default();
        km.merge_toml("[editor]\n\"ctrl-k ctrl-x\" = \"app.quit\"\n").unwrap();
        let mut d = Dispatcher::new(false);
        let r = d.dispatch(key(KeyCode::Char('k'), KeyModifiers::CONTROL), &["editor", "global"], Some(Mode::Insert), &km);
        assert!(matches!(r, Dispatch::Pending));
        let r = d.dispatch(key(KeyCode::Char('x'), KeyModifiers::CONTROL), &["editor", "global"], Some(Mode::Insert), &km);
        assert!(matches!(r, Dispatch::Action(Action::App(AppAction::Quit))));
    }

    #[test]
    fn modal_tables_only_when_enabled() {
        let km = default_keymap();
        let mut d = Dispatcher::new(false);
        let r = d.dispatch(key(KeyCode::Char('j'), KeyModifiers::NONE), &["editor", "global"], Some(Mode::Normal), &km);
        assert!(matches!(r, Dispatch::Unmapped(_)));
        let mut d = Dispatcher::new(true);
        let r = d.dispatch(key(KeyCode::Char('j'), KeyModifiers::NONE), &["editor", "global"], Some(Mode::Normal), &km);
        assert!(matches!(r, Dispatch::Action(Action::Editor(EditorAction::Move(_, false)))));
    }
}
