//! The Settings Keymap page: a read-only tree of context > action > keys over
//! the keymap given by [`KeymapPage::set_keymap`], with a search over action
//! descriptions, ids and key text.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, REGISTRY};
use crate::config::keymap::{KeyCombo, Keymap, chord_text};
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
    /// The keymap the page shows.
    shown: Keymap,
    /// Contexts the user expanded; `None` is the Unbound group.
    expanded: HashSet<Option<String>>,
    query: TextInput,
    rows: Vec<KeymapRow>,
    list: ListState,
    rows_rect: Rect,
    /// The whole page body from the last draw: the search line, the rows and the help.
    area: Rect,
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

    /// Show `km`, keeping the selected row when it still exists.
    pub fn set_keymap(&mut self, km: &Keymap) {
        self.shown = km.clone();
        self.rebuild(true);
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

    /// Select the row at the cell; whether one was hit. The marker cell or a
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
            Some(KeymapRow::Key { ctx, action, .. }) => {
                let id = REGISTRY.iter().find(|r| &r.action == action).map_or("?", |r| r.id);
                help.extend(wrap(&format!("Action: {id}"), w, 2));
                help.extend(wrap(&format!("Context: {ctx}"), w, 2));
            }
            None => {}
        }
        help.extend(wrap("Type to search. Enter, Right or Left expands or collapses a context.", w, 2));
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
}
