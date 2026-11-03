//! Hierarchy tool window (bottom): the call or type hierarchy in
//! `App::hierarchy` as a tree; this is its view state (`app/hierarchy.rs`
//! asks the server).

use crossterm::event::{MouseButton, MouseEventKind};
use lsp_types::SymbolKind;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, HierarchyAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Mouse};
use crate::lsp::hierarchy::{HierarchyTree, Node};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::util::text::fit;

const HINT: &str = "No hierarchy yet: Navigate > Call Hierarchy (Ctrl+Alt+H) or Type Hierarchy";

#[derive(Default)]
pub struct HierarchyPanel {
    pub list: ListState,
    list_rect: Rect,
    /// The id of the tree the selection belongs to.
    tree: Option<u64>,
    /// The node of the selected row; the selection follows it when rows appear above.
    node: usize,
}

/// The marker cell of a row: open, closed, or blank for a known leaf.
fn marker(n: &Node) -> char {
    match &n.children {
        Some(c) if c.is_empty() => ' ',
        Some(_) if n.expanded => '▾',
        _ => '▸',
    }
}

impl HierarchyPanel {
    /// Line the list up with `tree`'s visible rows and return them (node indices).
    pub fn sync(&mut self, tree: Option<&HierarchyTree>) -> Vec<usize> {
        let Some(t) = tree else {
            self.tree = None;
            self.list.set_len(0);
            return Vec::new();
        };
        let rows = t.rows();
        if self.tree != Some(t.id) {
            self.tree = Some(t.id);
            self.list = ListState::default();
            self.node = rows.first().copied().unwrap_or(0);
        }
        // The selected node, or its nearest visible ancestor (a collapse hid it).
        let mut at = Some(self.node);
        let i = loop {
            match at {
                Some(n) => match rows.iter().position(|&r| r == n) {
                    Some(i) => break i,
                    None => at = t.nodes.get(n).and_then(|n| n.parent),
                },
                None => break 0,
            }
        };
        if self.list.len != rows.len() || self.list.selected != i {
            self.list.len = rows.len();
            self.list.select(i);
        }
        if let Some(&n) = rows.get(self.list.selected) {
            self.node = n;
        }
        rows
    }

    /// The selected node after `sync`; `None` without a tree.
    pub fn selected(&mut self, tree: Option<&HierarchyTree>) -> Option<usize> {
        let rows = self.sync(tree);
        rows.get(self.list.selected).copied()
    }

    /// Select `node`'s row (it must be visible).
    pub fn select_node(&mut self, tree: &HierarchyTree, node: usize) {
        self.sync(Some(tree));
        if let Some(i) = tree.rows().iter().position(|&r| r == node) {
            self.list.select(i);
            self.node = node;
        }
    }

    /// Up, Down, PageUp, PageDown, Home, End; false for any other action.
    pub fn step(&mut self, a: &HierarchyAction, tree: Option<&HierarchyTree>) -> bool {
        let rows = self.sync(tree);
        let page = self.list.viewport.max(1);
        match a {
            HierarchyAction::Up => self.list.up(1),
            HierarchyAction::Down => self.list.down(1),
            HierarchyAction::PageUp => self.list.up(page),
            HierarchyAction::PageDown => self.list.down(page),
            HierarchyAction::Home => self.list.home(),
            HierarchyAction::End => self.list.end(),
            _ => return false,
        }
        if let Some(&n) = rows.get(self.list.selected) {
            self.node = n;
        }
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, tree: Option<&HierarchyTree>, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.list.hit(self.list_rect, m.x, m.y) {
                    let rows = self.sync(tree);
                    self.list.select(i);
                    let Some((&node, t)) = rows.get(i).zip(tree) else { return true };
                    self.node = node;
                    let n = &t.nodes[node];
                    let at = self.list_rect.x + 1 + 2 * n.depth as u16;
                    let toggle = (m.x == at || m.x == at + 1) && marker(n) != ' ';
                    let action = if toggle {
                        Some(if marker(n) == '▾' { HierarchyAction::Collapse } else { HierarchyAction::Expand })
                    } else if m.clicks >= 2 {
                        Some(HierarchyAction::Open)
                    } else {
                        None
                    };
                    if let Some(a) = action {
                        cx.effects.push(Effect::Perform(Action::Hierarchy(a)));
                    }
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.list.scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.list.scroll(3);
                true
            }
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, tree: Option<&HierarchyTree>) {
        let title = tree.map_or_else(|| " Hierarchy ".to_string(), |t| format!(" {} ", t.title()));
        let block = tool_window_block(&title, focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.list_rect = Rect::default();
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let buf = frame.buffer_mut();
        let Some(t) = tree else {
            self.sync(None);
            buf.set_stringn(inner.x + 1, inner.y, HINT, inner.width.saturating_sub(1) as usize, theme.dim);
            return;
        };
        let rows = self.sync(tree);
        let lr = inner;
        self.list_rect = lr;
        self.list.set_viewport(lr.height as usize);
        for (vi, i) in self.list.visible().enumerate() {
            let y = lr.y + vi as u16;
            let selected = i == self.list.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(lr.x, y, lr.width, 1), base);
            }
            let n = &t.nodes[rows[i]];
            let room = |x: u16| lr.right().saturating_sub(x) as usize;
            let mut x = lr.x + 1 + 2 * n.depth as u16;
            if x >= lr.right() {
                continue;
            }
            buf.set_stringn(x, y, marker(n).to_string(), room(x), base);
            x += 2;
            (x, _) = buf.set_stringn(x, y, &n.item.name, room(x), base);
            let file = n.item.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let loading = if n.loading { " …" } else { "" };
            let place = if n.item.kind == SymbolKind::FILE {
                loading.to_string()
            } else {
                format!("  {file}:{}{loading}", n.item.selection.start.line + 1)
            };
            let detail = n.item.detail.as_deref().map(|d| d.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|d| !d.is_empty());
            let mut tail = String::new();
            if let Some(d) = detail {
                let budget = room(x).saturating_sub(place.width() + 2);
                if budget > 0 {
                    tail = format!("  {}", fit(&d, budget));
                }
            }
            tail.push_str(&place);
            buf.set_stringn(x, y, &tail, room(x), base.patch(theme.dim));
        }
    }

    #[cfg(test)]
    pub(crate) fn list_rect(&self) -> Rect {
        self.list_rect
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::*;
    use crate::lsp::hierarchy::{Direction, parse_items};
    use crate::ui::snapshot::row_text;

    fn items(names: &[&str], detail: Option<&str>) -> Vec<crate::lsp::hierarchy::HierarchyItem> {
        let range = json!({"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}});
        let raw: Vec<_> = names
            .iter()
            .map(|n| json!({"name": n, "kind": 12, "detail": detail, "uri": "file:///w/b.rs", "range": range, "selectionRange": range}))
            .collect();
        parse_items(json!(raw))
    }

    fn tree(id: u64, names: &[&str]) -> HierarchyTree {
        HierarchyTree::new(id, "s".into(), Direction::Callers, items(names, None))
    }

    fn draw(p: &mut HierarchyPanel, t: Option<&HierarchyTree>, w: u16) -> ratatui::buffer::Buffer {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(w, 5)).unwrap();
        term.draw(|f| p.render(f, f.area(), true, &theme, t)).unwrap();
        term.backend().buffer().clone()
    }

    #[test]
    fn sync_follows_the_node() {
        let mut t = tree(1, &["a", "b"]);
        let mut p = HierarchyPanel::default();
        p.sync(Some(&t));
        p.select_node(&t, 1);
        t.set_children(0, items(&["c", "d"], None));
        assert_eq!(p.selected(Some(&t)), Some(1));
        assert_eq!(p.list.selected, 3);
    }

    #[test]
    fn a_collapsed_selection_moves_to_its_parent() {
        let mut t = tree(1, &["a"]);
        t.set_children(0, items(&["c"], None));
        let mut p = HierarchyPanel::default();
        p.select_node(&t, 1);
        assert_eq!(p.selected(Some(&t)), Some(1));
        t.collapse(0);
        assert_eq!(p.selected(Some(&t)), Some(0));
    }

    #[test]
    fn a_new_tree_starts_at_the_first_row() {
        let mut p = HierarchyPanel::default();
        let a = tree(1, &["a", "b"]);
        p.select_node(&a, 1);
        assert_eq!(p.selected(Some(&a)), Some(1));
        assert_eq!(p.selected(Some(&tree(2, &["a", "b"]))), Some(0));
        assert_eq!(p.selected(None), None);
    }

    #[test]
    fn the_hint_without_a_tree() {
        let mut p = HierarchyPanel::default();
        let buf = draw(&mut p, None, 70);
        assert!(row_text(&buf, 1, 70).contains("No hierarchy yet"), "{}", row_text(&buf, 1, 70));
        assert_eq!(p.list_rect(), Rect::default());
    }

    #[test]
    fn a_row_shows_marker_name_and_location() {
        let mut t = HierarchyTree::new(1, "s".into(), Direction::Callers, items(&["helper"], Some("fn helper(s:\n    &str)")));
        let mut p = HierarchyPanel::default();
        let buf = draw(&mut p, Some(&t), 70);
        assert!(row_text(&buf, 1, 70).contains("│ ▸ helper  fn helper(s: &str)  b.rs:2"), "{}", row_text(&buf, 1, 70));
        t.nodes[0].loading = true;
        let buf = draw(&mut p, Some(&t), 70);
        assert!(row_text(&buf, 1, 70).contains("b.rs:2 …"), "{}", row_text(&buf, 1, 70));
        let buf = draw(&mut p, Some(&t), 32);
        let row = row_text(&buf, 1, 32);
        assert!(row.contains("helper  fn helpe…  b.rs:2 …"), "{row}");
    }
}
