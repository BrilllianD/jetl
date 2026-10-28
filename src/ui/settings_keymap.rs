//! The Settings Keymap page: a tree of context > action > keys over the keymap
//! given by [`KeymapPage::set_staged`] (the base with the staged edits
//! applied), with a search over action descriptions, ids and key text.
//! Changed rows carry the pending marker. Key rows that conflict with another
//! binding carry a mark ([`Keymap::conflicts`]).

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, REGISTRY};
use crate::config::edit::KeymapEdit;
use crate::config::keymap::{Conflict, ConflictKind, KeyCombo, Keymap, chord_text};
use crate::config::theme::Theme;
use crate::ui::settings::{MARK_W, MAX_HELP_ROWS, move_list};
use crate::ui::toasts::wrap;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;
use crate::util::text::fit;

/// Title of the last group: registry actions bound in no context.
pub const UNBOUND: &str = "Unbound";

const SEARCH_LABEL: &str = "Search: ";

/// One visible row of the Keymap page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapRow {
    /// A context heading; `ctx` is `None` for the Unbound group.
    Context { ctx: Option<String>, label: String, expanded: bool, actions: usize },
    /// An action of the context above. `ctx` is that context, or for the
    /// Unbound group `action.contexts()[0]` (where a new key would bind it).
    Action { ctx: String, action: Action, id: &'static str, desc: &'static str },
    /// One chord of the action above.
    Key { ctx: String, action: Action, chord: Vec<KeyCombo> },
}

/// What the selected row stands for; `None` on a context row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapSelection {
    pub ctx: String,
    pub action: Action,
    /// `Some` on a key row; `None` on an action row.
    pub chord: Option<Vec<KeyCombo>>,
}

#[derive(Default)]
pub struct KeymapPage {
    /// The keymap before the staged edits; `shown` is it with them applied.
    base: Keymap,
    /// The keymap the page shows.
    shown: Keymap,
    /// Conflicts of the shown keymap, refreshed by [`Self::restage`].
    conflicts: Vec<Conflict>,
    /// Contexts the user expanded; `None` is the Unbound group.
    expanded: HashSet<Option<String>>,
    query: TextInput,
    rows: Vec<KeymapRow>,
    list: ListState,
    rows_rect: Rect,
    /// The whole page body from the last draw: the search line, the rows and the help.
    area: Rect,
}

/// Registry id and description of an action (`"?"` when missing).
fn action_info(a: &Action) -> (&'static str, &'static str) {
    REGISTRY.iter().find(|r| &r.action == a).map_or(("?", "?"), |r| (r.id, r.desc))
}

/// The non-empty bindings of context `ctx`.
fn bindings<'a>(km: &'a Keymap, ctx: &str) -> Option<&'a HashMap<Vec<KeyCombo>, Action>> {
    km.table(ctx).map(|t| &t.bindings).filter(|b| !b.is_empty())
}

/// `git.changes` becomes `Git › Changes`, `local_history` `Local History`.
pub fn context_label(ctx: &str) -> String {
    ctx.split('.')
        .map(|seg| {
            seg.split('_')
                .map(|w| {
                    let mut cs = w.chars();
                    cs.next().map(|c| c.to_uppercase().chain(cs).collect::<String>()).unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" › ")
}

/// The rows for `km` under `query` (case-insensitive substring over the
/// description, the id and the key text). A search shows every matching
/// context expanded.
pub fn build_rows(km: &Keymap, query: &str, expanded: &HashSet<Option<String>>) -> Vec<KeymapRow> {
    let q = query.trim().to_lowercase();
    let searching = !q.is_empty();
    let mut position: HashMap<&Action, usize> = HashMap::new();
    for (i, r) in REGISTRY.iter().enumerate() {
        position.entry(&r.action).or_insert(i);
    }
    let mut rows = Vec::new();
    for ctx in km.context_order() {
        let mut by_action: HashMap<usize, Vec<Vec<KeyCombo>>> = HashMap::new();
        for (chord, action) in &km.tables[ctx].bindings {
            if let Some(&i) = position.get(action) {
                by_action.entry(i).or_default().push(chord.clone());
            }
        }
        let mut actions: Vec<(usize, Vec<Vec<KeyCombo>>)> = by_action.into_iter().collect();
        actions.sort_by_key(|(i, _)| *i);
        for (_, chords) in &mut actions {
            chords.sort_by_cached_key(|c| (c.len(), chord_text(c)));
        }
        if searching {
            actions.retain(|(i, chords)| {
                let info = &REGISTRY[*i];
                info.desc.to_lowercase().contains(&q)
                    || info.id.to_lowercase().contains(&q)
                    || chords.iter().any(|c| chord_text(c).to_lowercase().contains(&q))
            });
        }
        if actions.is_empty() {
            continue;
        }
        let open = searching || expanded.contains(&Some(ctx.to_string()));
        rows.push(KeymapRow::Context { ctx: Some(ctx.to_string()), label: context_label(ctx), expanded: open, actions: actions.len() });
        if open {
            for (i, chords) in actions {
                let info = &REGISTRY[i];
                rows.push(KeymapRow::Action { ctx: ctx.to_string(), action: info.action.clone(), id: info.id, desc: info.desc });
                for chord in chords {
                    rows.push(KeymapRow::Key { ctx: ctx.to_string(), action: info.action.clone(), chord });
                }
            }
        }
    }
    let unbound: Vec<_> = REGISTRY
        .iter()
        .filter(|r| !km.tables.values().any(|t| t.bindings.values().any(|a| a == &r.action)))
        .filter(|r| !searching || r.desc.to_lowercase().contains(&q) || r.id.to_lowercase().contains(&q))
        .collect();
    if !unbound.is_empty() {
        let open = searching || expanded.contains(&None);
        rows.push(KeymapRow::Context { ctx: None, label: UNBOUND.into(), expanded: open, actions: unbound.len() });
        if open {
            for r in unbound {
                let ctx = r.action.contexts().first().copied().unwrap_or("global").to_string();
                rows.push(KeymapRow::Action { ctx, action: r.action.clone(), id: r.id, desc: r.desc });
            }
        }
    }
    rows
}

fn same(a: &KeymapRow, b: &KeymapRow) -> bool {
    use KeymapRow::*;
    match (a, b) {
        (Context { ctx: x, .. }, Context { ctx: y, .. }) => x == y,
        (Action { ctx: c1, action: a1, .. }, Action { ctx: c2, action: a2, .. }) => c1 == c2 && a1 == a2,
        (Key { ctx: c1, action: a1, chord: k1 }, Key { ctx: c2, action: a2, chord: k2 }) => c1 == c2 && a1 == a2 && k1 == k2,
        _ => false,
    }
}

impl KeymapPage {
    fn rebuild(&mut self, keep: bool) {
        let previous = if keep { self.selected_row().cloned() } else { None };
        self.rows = build_rows(&self.shown, &self.query.text, &self.expanded);
        self.list.set_len(self.rows.len());
        if keep {
            if let Some(i) = previous.and_then(|p| self.rows.iter().position(|r| same(r, &p))) {
                self.list.select(i);
            }
        } else {
            let first = self.rows.iter().position(|r| matches!(r, KeymapRow::Action { .. })).unwrap_or(0);
            self.list.select(first);
            self.list.offset = 0;
        }
    }

    /// Show `km` with nothing staged, keeping the selected row when it still exists.
    pub fn set_keymap(&mut self, km: &Keymap) {
        self.set_staged(km, &[]);
    }

    /// Show `base` with `edits` applied, keeping the selected row when it still exists.
    pub fn set_staged(&mut self, base: &Keymap, edits: &[KeymapEdit]) {
        self.base = base.clone();
        self.restage(edits);
    }

    /// Show the base with `edits` applied in order. The only place `shown` changes.
    pub fn restage(&mut self, edits: &[KeymapEdit]) {
        self.shown = self.base.clone();
        for e in edits {
            self.shown.apply_edit(e);
        }
        self.rebuild(true);
        self.conflicts = self.shown.conflicts();
    }

    /// The keymap before the staged edits.
    pub fn base(&self) -> &Keymap {
        &self.base
    }

    /// Whether the shown keymap binds exactly what the base binds.
    pub fn is_unchanged(&self) -> bool {
        self.base.tables.keys().chain(self.shown.tables.keys()).all(|c| bindings(&self.base, c) == bindings(&self.shown, c))
    }

    /// The conflict a key row is part of, and whether the row is the longer
    /// key a `Prefix` conflict blocks. A `Prefix` conflict wins over `Shadows`.
    fn row_conflict(&self, row: &KeymapRow) -> Option<(&Conflict, bool)> {
        let KeymapRow::Key { ctx, action, chord } = row else { return None };
        self.conflicts
            .iter()
            .filter_map(|c| {
                if c.ctx != *ctx {
                    return None;
                }
                if c.chord == *chord {
                    Some((c, false))
                } else if c.kind == ConflictKind::Prefix && chord.len() > c.chord.len() && chord.starts_with(&c.chord) && c.other == *action
                {
                    Some((c, true))
                } else {
                    None
                }
            })
            .min_by_key(|(c, _)| c.kind != ConflictKind::Prefix)
    }

    /// The help line naming the binding a key row conflicts with.
    fn conflict_line(&self, row: &KeymapRow) -> Option<String> {
        let (c, longer) = self.row_conflict(row)?;
        let (id, desc) = action_info(&c.other);
        Some(match (c.kind, longer) {
            (ConflictKind::Shadows, _) => format!("Conflict: overrides {desc} ({id}) in {}.", context_label(&c.other_ctx)),
            (ConflictKind::Prefix, false) => format!("Conflict: also starts a key of {id} in this context, which never fires."),
            (ConflictKind::Prefix, true) => format!("Conflict: never fires; {} is bound in this context.", chord_text(&c.chord)),
        })
    }

    /// Whether the row differs from the base (drawn with the pending marker).
    pub fn row_changed(&self, row: &KeymapRow) -> bool {
        let chords = |km: &Keymap, ctx: &str, action: &Action| -> HashSet<Vec<KeyCombo>> {
            km.table(ctx).map(|t| t.bindings.iter().filter(|(_, a)| *a == action).map(|(c, _)| c.clone()).collect()).unwrap_or_default()
        };
        match row {
            KeymapRow::Context { ctx: Some(c), .. } => bindings(&self.base, c) != bindings(&self.shown, c),
            KeymapRow::Context { ctx: None, .. } => false,
            KeymapRow::Action { ctx, action, .. } => chords(&self.base, ctx, action) != chords(&self.shown, ctx, action),
            KeymapRow::Key { ctx, action, chord } => self.base.table(ctx).and_then(|t| t.bindings.get(chord)) != Some(action),
        }
    }

    pub fn keymap(&self) -> &Keymap {
        &self.shown
    }

    pub fn rows(&self) -> &[KeymapRow] {
        &self.rows
    }

    pub fn selected_row(&self) -> Option<&KeymapRow> {
        self.rows.get(self.list.selected)
    }

    pub fn selection(&self) -> Option<KeymapSelection> {
        match self.selected_row()? {
            KeymapRow::Context { .. } => None,
            KeymapRow::Action { ctx, action, .. } => Some(KeymapSelection { ctx: ctx.clone(), action: action.clone(), chord: None }),
            KeymapRow::Key { ctx, action, chord } => {
                Some(KeymapSelection { ctx: ctx.clone(), action: action.clone(), chord: Some(chord.clone()) })
            }
        }
    }

    pub fn query(&self) -> &str {
        &self.query.text
    }

    pub fn set_query(&mut self, q: &str) {
        self.query.set_text(q);
        self.rebuild(false);
    }

    /// Empty the query; whether it had text.
    pub fn clear_query(&mut self) -> bool {
        if self.query.text.is_empty() {
            return false;
        }
        self.query.clear();
        self.rebuild(false);
        true
    }

    fn toggle(&mut self, ctx: Option<String>, open: bool) {
        if !self.query.text.is_empty() {
            return;
        }
        if open {
            self.expanded.insert(ctx);
        } else {
            self.expanded.remove(&ctx);
        }
        self.rebuild(true);
    }

    /// Whether the key was used; `false` lets the window handle it (Left
    /// then returns to the page tree).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if move_list(&mut self.list, key.code) {
            return true;
        }
        let ctx_row = match self.selected_row() {
            Some(KeymapRow::Context { ctx, expanded, .. }) => Some((ctx.clone(), *expanded)),
            _ => None,
        };
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => {
                if let Some((ctx, open)) = ctx_row {
                    self.toggle(ctx, !open);
                }
                true
            }
            KeyCode::Right => {
                if let Some((ctx, false)) = ctx_row {
                    self.toggle(ctx, true);
                }
                true
            }
            KeyCode::Left => match ctx_row {
                Some((ctx, true)) if self.query.text.is_empty() => {
                    self.toggle(ctx, false);
                    true
                }
                Some(_) => false,
                None if self.rows.is_empty() => false,
                None => {
                    if let Some(i) = self.rows[..self.list.selected].iter().rposition(|r| matches!(r, KeymapRow::Context { .. })) {
                        self.list.select(i);
                    }
                    true
                }
            },
            KeyCode::Backspace => {
                let before = self.query.text.clone();
                if plain {
                    self.query.backspace();
                } else {
                    self.query.delete_word_back();
                }
                if self.query.text != before {
                    self.rebuild(false);
                }
                true
            }
            KeyCode::Char(c) if plain => {
                self.query.insert(c);
                self.rebuild(false);
                true
            }
            _ => false,
        }
    }

    /// Select the row at the cell; whether the cell is inside the page body
    /// (a click outside the rows focuses the page). The marker cell or a
    /// double click toggles a context.
    pub fn click(&mut self, x: u16, y: u16, clicks: u8) -> bool {
        if !self.area.contains((x, y).into()) {
            return false;
        }
        let Some(i) = self.list.hit(self.rows_rect, x, y) else { return true };
        self.list.select(i);
        if let Some(KeymapRow::Context { ctx, expanded, .. }) = self.rows.get(i) {
            let (ctx, open) = (ctx.clone(), *expanded);
            let on_marker = x == self.rows_rect.x + MARK_W as u16;
            if (on_marker && clicks <= 1) || (!on_marker && clicks >= 2) {
                self.toggle(ctx, !open);
            }
        }
        true
    }

    pub fn scroll(&mut self, delta: i32) {
        self.list.scroll(delta);
    }

    /// Whether the cell is on a row (not on the blank space below the last one).
    pub fn hits_row(&self, x: u16, y: u16) -> bool {
        self.list.hit(self.rows_rect, x, y).is_some()
    }

    pub fn rows_rect(&self) -> Rect {
        self.rows_rect
    }

    /// Draw into `area`, the page body below the crumb.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, base: Style, focused: bool, theme: &Theme) {
        let w = area.width as usize;
        let bottom = area.bottom();
        if area.height == 0 || w == 0 {
            self.rows_rect = Rect::default();
            self.area = Rect::default();
            return;
        }
        self.area = area;
        let dim = base.patch(theme.dim);
        {
            let buf = frame.buffer_mut();
            buf.set_string(area.x, area.y, fit(SEARCH_LABEL, w), dim);
        }
        if w > SEARCH_LABEL.len() {
            let input = Rect::new(area.x + SEARCH_LABEL.len() as u16, area.y, (w - SEARCH_LABEL.len()) as u16, 1);
            self.query.render(frame, input, base, "action, id or key", dim, focused);
        }

        let top = area.y + 2;
        let free = bottom.saturating_sub(top) as usize;
        let mut help: Vec<String> = Vec::new();
        match self.selected_row() {
            Some(KeymapRow::Context { ctx: Some(c), .. }) => help.extend(wrap(&format!("Context: {c}"), w, 2)),
            Some(KeymapRow::Context { ctx: None, .. }) => help.push("Actions without a key in any context.".into()),
            Some(KeymapRow::Action { ctx, id, .. }) => {
                help.extend(wrap(&format!("Action: {id}"), w, 2));
                help.extend(wrap(&format!("Context: {ctx}"), w, 2));
            }
            Some(row @ KeymapRow::Key { ctx, action, .. }) => {
                help.extend(wrap(&format!("Action: {}", action_info(action).0), w, 2));
                help.extend(wrap(&format!("Context: {ctx}"), w, 2));
                if let Some(l) = self.conflict_line(row) {
                    help.extend(wrap(&l, w, 2));
                }
            }
            None => {}
        }
        let tip = match self.selected_row() {
            Some(KeymapRow::Action { .. }) => "Type to search. Enter or Insert adds a key.",
            Some(KeymapRow::Key { .. }) => "Type to search. Enter edits the key, Insert adds one, Delete removes it.",
            _ => "Type to search. Enter, Right or Left expands or collapses a context.",
        };
        help.extend(wrap(tip, w, 2));
        help.truncate(MAX_HELP_ROWS.min(free.saturating_sub(2)));
        let help_h = help.len() as u16;
        let sep = u16::from(help_h > 0);
        let rows_h = bottom.saturating_sub(top).saturating_sub(help_h + sep);
        // The help height follows the selection; keep the selected row in view
        // when the viewport shrinks (wheel scrolling leaves it alone).
        let resized = self.list.viewport != rows_h as usize;
        self.list.fit(self.rows.len(), rows_h as usize);
        if resized {
            self.list.clamp();
        }
        self.rows_rect = Rect::new(area.x, top, area.width, rows_h);

        let changed: Vec<bool> = self.list.visible().map(|i| self.row_changed(&self.rows[i])).collect();
        let marks: Vec<Option<ConflictKind>> = self.list.visible().map(|i| self.row_conflict(&self.rows[i]).map(|(c, _)| c.kind)).collect();
        let buf = frame.buffer_mut();
        if self.rows.is_empty() {
            if rows_h > 0 {
                let msg = if self.query.text.is_empty() { "No key bindings." } else { "No action or key matches." };
                buf.set_string(area.x, top, fit(msg, w), dim);
            }
        } else {
            let sel = if focused { theme.list_selected } else { theme.list_selected_unfocused };
            let text_w = w.saturating_sub(MARK_W);
            let tx = area.x + MARK_W as u16;
            for (row, i) in self.list.visible().enumerate() {
                let ry = top + row as u16;
                let style = if i == self.list.selected { sel } else { base };
                buf.set_style(Rect::new(area.x, ry, area.width, 1), style);
                if changed[row] {
                    buf.set_string(area.x, ry, "*", style.patch(theme.git_modified));
                }
                if let Some(kind) = marks[row].filter(|_| w >= 2) {
                    let (glyph, mark) = if kind == ConflictKind::Prefix { ("!", theme.warn) } else { ("^", theme.info) };
                    buf.set_string(area.x + 1, ry, glyph, style.patch(mark));
                }
                if text_w == 0 {
                    continue;
                }
                match &self.rows[i] {
                    KeymapRow::Context { label, expanded, actions, .. } => {
                        let mark = if *expanded { '▾' } else { '▸' };
                        buf.set_string(tx, ry, fit(&format!("{mark} {label} ({actions})"), text_w), style.patch(theme.title));
                    }
                    KeymapRow::Action { id, desc, .. } => {
                        buf.set_string(tx, ry, fit(&format!("  {desc}"), text_w), style);
                        if desc.width() + id.width() + 4 <= text_w {
                            buf.set_string(tx + (text_w - id.width()) as u16, ry, id, style.patch(theme.dim));
                        }
                    }
                    KeymapRow::Key { chord, .. } => {
                        buf.set_string(tx, ry, fit(&format!("      {}", chord_text(chord)), text_w), style);
                    }
                }
            }
        }
        for (i, l) in help.iter().enumerate() {
            buf.set_string(area.x, bottom - help_h + i as u16, fit(l, w), dim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::AppAction;
    use crate::config::keymap::{default_keymap, parse_chord};

    fn press(p: &mut KeymapPage, code: KeyCode) -> bool {
        p.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn contexts(rows: &[KeymapRow]) -> Vec<(Option<&str>, bool)> {
        rows.iter()
            .filter_map(|r| match r {
                KeymapRow::Context { ctx, expanded, .. } => Some((ctx.as_deref(), *expanded)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn labels_title_case_each_segment() {
        assert_eq!(context_label("global"), "Global");
        assert_eq!(context_label("git.changes"), "Git › Changes");
        assert_eq!(context_label("mode.normal"), "Mode › Normal");
        assert_eq!(context_label("editor.snippet_prefix"), "Editor › Snippet Prefix");
    }

    #[test]
    fn contexts_follow_context_order_then_unbound() {
        let km = default_keymap();
        let rows = build_rows(&km, "", &HashSet::new());
        assert!(rows.iter().all(|r| matches!(r, KeymapRow::Context { .. })));
        let mut want: Vec<Option<&str>> = km.context_order().into_iter().map(Some).collect();
        want.push(None);
        assert_eq!(contexts(&rows).into_iter().map(|c| c.0).collect::<Vec<_>>(), want);
        let KeymapRow::Context { label, .. } = rows.last().unwrap() else { panic!() };
        assert_eq!(label, UNBOUND);
    }

    #[test]
    fn expanded_context_lists_actions_in_registry_order_and_chords_shortest_first() {
        let km = Keymap::from_triples([
            ("global", "ctrl-k ctrl-x", "app.quit"),
            ("global", "ctrl-x", "app.quit"),
            ("global", "ctrl-y", "app.settings"),
        ]);
        let first_quit = REGISTRY.iter().position(|r| r.id == "app.quit").unwrap();
        let first_settings = REGISTRY.iter().position(|r| r.id == "app.settings").unwrap();
        let (a, b) = if first_quit < first_settings { ("app.quit", "app.settings") } else { ("app.settings", "app.quit") };
        let rows = build_rows(&km, "", &HashSet::from([Some("global".to_string())]));
        let shape: Vec<String> = rows
            .iter()
            .map(|r| match r {
                KeymapRow::Context { .. } => "ctx".into(),
                KeymapRow::Action { id, .. } => id.to_string(),
                KeymapRow::Key { chord, .. } => chord_text(chord),
            })
            .collect();
        let keys_of = |id: &str| match id {
            "app.quit" => vec!["Ctrl+X".to_string(), "Ctrl+K Ctrl+X".to_string()],
            _ => vec!["Ctrl+Y".to_string()],
        };
        let mut want = vec!["ctx".to_string(), a.to_string()];
        want.extend(keys_of(a));
        want.push(b.to_string());
        want.extend(keys_of(b));
        want.push("ctx".to_string()); // the collapsed Unbound group
        assert_eq!(shape, want);
    }

    #[test]
    fn search_matches_description_id_and_key_text() {
        let km = default_keymap();
        for q in ["exit jetl", "app.quit", "ctrl+q"] {
            let rows = build_rows(&km, q, &HashSet::new());
            assert!(rows.iter().any(|r| matches!(r, KeymapRow::Context { ctx: Some(c), expanded: true, .. } if c == "global")), "{q}");
            assert!(rows.iter().any(|r| matches!(r, KeymapRow::Action { ctx, id: "app.quit", .. } if ctx == "global")), "{q}");
            assert!(
                rows.iter().any(|r| matches!(r, KeymapRow::Key { action, chord, .. }
                    if *action == Action::App(AppAction::Quit) && chord_text(chord) == "Ctrl+Q")),
                "{q}"
            );
        }
        assert!(build_rows(&km, "zzzqqqxxx", &HashSet::new()).is_empty());
    }

    #[test]
    fn unbound_actions_bind_in_their_first_context() {
        let km = Keymap::from_triples([("global", "ctrl-q", "app.quit")]);
        let rows = build_rows(&km, "", &HashSet::from([None]));
        let mut n = 0;
        for r in rows.iter().skip_while(|r| !matches!(r, KeymapRow::Context { ctx: None, .. })) {
            if let KeymapRow::Action { ctx, action, id, .. } = r {
                n += 1;
                assert_eq!(ctx, action.contexts().first().copied().unwrap_or("global"));
                assert_ne!(*id, "app.quit");
            }
        }
        assert!(n > 0);
    }

    #[test]
    fn selection_survives_set_keymap_and_query_change_selects_first_action() {
        let km = default_keymap();
        let mut p = KeymapPage::default();
        p.set_keymap(&km);
        p.set_query("ctrl+q");
        let i = p.rows().iter().position(|r| matches!(r, KeymapRow::Key { .. })).unwrap();
        p.list.select(i);
        let before = p.selected_row().cloned();
        p.set_keymap(&km);
        assert_eq!(p.selected_row().cloned(), before);
        assert_eq!(p.selection().unwrap().chord, Some(parse_chord("ctrl-q").unwrap()));
        p.set_query("quit");
        assert!(matches!(p.selected_row(), Some(KeymapRow::Action { .. })));
        let first = p.rows().iter().position(|r| matches!(r, KeymapRow::Action { .. })).unwrap();
        assert_eq!(p.list.selected, first);
        assert!(p.clear_query());
        assert!(!p.clear_query());
    }

    #[test]
    fn left_goes_to_the_context_then_collapses() {
        let km = Keymap::from_triples([("global", "ctrl-q", "app.quit")]);
        let mut p = KeymapPage::default();
        p.set_keymap(&km);
        p.toggle(Some("global".into()), true);
        p.list.select(2);
        assert!(matches!(p.selected_row(), Some(KeymapRow::Key { .. })));
        assert!(press(&mut p, KeyCode::Left));
        assert_eq!(p.list.selected, 0);
        assert!(press(&mut p, KeyCode::Left));
        assert!(matches!(p.rows()[0], KeymapRow::Context { expanded: false, .. }));
        assert!(!press(&mut p, KeyCode::Left));
    }

    fn bind(ctx: &str, key: &str, id: &'static str) -> KeymapEdit {
        KeymapEdit::Bind { ctx: ctx.into(), chord: parse_chord(key).unwrap(), id }
    }

    fn unbind(ctx: &str, key: &str) -> KeymapEdit {
        KeymapEdit::Unbind { ctx: ctx.into(), chord: parse_chord(key).unwrap() }
    }

    #[test]
    fn staged_rows_are_marked() {
        let base = Keymap::from_triples([("global", "ctrl-q", "app.quit"), ("global", "ctrl-s", "editor.save")]);
        let mut p = KeymapPage::default();
        p.set_keymap(&base);
        p.toggle(Some("global".into()), true);
        p.restage(&[unbind("global", "ctrl-q"), bind("global", "f2", "app.quit")]);
        let changed = |p: &KeymapPage| -> Vec<String> {
            p.rows()
                .iter()
                .filter(|r| p.row_changed(r))
                .map(|r| match r {
                    KeymapRow::Context { label, .. } => label.clone(),
                    KeymapRow::Action { id, .. } => id.to_string(),
                    KeymapRow::Key { chord, .. } => chord_text(chord),
                })
                .collect()
        };
        assert_eq!(changed(&p), ["Global", "app.quit", "F2"]);
        assert!(!p.is_unchanged());
        p.restage(&[]);
        assert!(p.is_unchanged());
        assert!(changed(&p).is_empty());
    }

    #[test]
    fn set_staged_reapplies_edits_on_a_new_base() {
        let base1 = Keymap::from_triples([("global", "ctrl-q", "app.quit")]);
        let base2 = Keymap::from_triples([("global", "ctrl-q", "app.quit"), ("global", "ctrl-alt-y", "app.about")]);
        let edits = [bind("global", "f2", "app.quit")];
        let mut p = KeymapPage::default();
        p.set_staged(&base1, &edits);
        p.set_staged(&base2, &edits);
        let bound = |k: &str| p.keymap().table("global").unwrap().bindings.contains_key(&parse_chord(k).unwrap());
        assert!(bound("f2") && bound("ctrl-alt-y"));
        assert!(!p.base().table("global").unwrap().bindings.contains_key(&parse_chord("f2").unwrap()));
    }

    #[test]
    fn staged_edit_shows_its_conflict() {
        let ch = |s: &str| parse_chord(s).unwrap();
        let base = Keymap::from_triples([("global", "f5", "app.refresh"), ("tree", "f6", "tree.refresh")]);
        let mut p = KeymapPage::default();
        p.set_keymap(&base);
        assert!(p.conflicts.is_empty());
        p.restage(&[bind("tree", "f5", "tree.refresh")]);
        assert_eq!(
            p.conflicts,
            vec![Conflict {
                ctx: "tree".into(),
                chord: ch("f5"),
                other_ctx: "global".into(),
                other: Action::from_id("app.refresh").unwrap(),
                kind: ConflictKind::Shadows,
            }]
        );
        let row = KeymapRow::Key { ctx: "tree".into(), action: Action::from_id("tree.refresh").unwrap(), chord: ch("f5") };
        assert!(matches!(p.row_conflict(&row), Some((_, false))));
        let line = p.conflict_line(&row).unwrap();
        assert!(line.contains("overrides") && line.contains("app.refresh") && line.contains("Global"), "{line}");
        p.restage(&[]);
        assert!(p.conflicts.is_empty());
    }

    #[test]
    fn prefix_conflict_marks_both_keys() {
        let ch = |s: &str| parse_chord(s).unwrap();
        let a = |id: &str| Action::from_id(id).unwrap();
        let mut p = KeymapPage::default();
        p.set_keymap(&Keymap::from_triples([("global", "ctrl-k", "app.quit"), ("global", "ctrl-k ctrl-c", "app.settings")]));
        let key = |chord: &str, id: &str| KeymapRow::Key { ctx: "global".into(), action: a(id), chord: ch(chord) };
        let (c, longer) = p.row_conflict(&key("ctrl-k", "app.quit")).unwrap();
        assert!(!longer && c.kind == ConflictKind::Prefix);
        let long = key("ctrl-k ctrl-c", "app.settings");
        assert!(matches!(p.row_conflict(&long), Some((_, true))));
        let line = p.conflict_line(&long).unwrap();
        assert!(line.contains("never fires") && line.contains("Ctrl+K"), "{line}");
        assert!(p.row_conflict(&key("f9", "app.about")).is_none());
        let action = KeymapRow::Action { ctx: "global".into(), action: a("app.quit"), id: "app.quit", desc: "x" };
        assert!(p.row_conflict(&action).is_none());
    }
}
