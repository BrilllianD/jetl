//! The model of the Debug window's Variables tab: the scopes of one frame as a tree whose children are read one
//! level at a time. No drawing and no IO. It is `HierarchyTree`'s shape (an append-only arena, `children: None`
//! until asked), with these differences: a leaf is known from `variablesReference <= 0` and never asked; an answer
//! does not force `expanded`, so a collapse while the request is out holds; there is no tree id, since the panel
//! replaces the whole view when its selection seq rises; a node can hold a failure and a count of children left
//! out; children are capped; and asks are queued here for the App to send, as the panel has no sender.

use super::protocol::Scope;
use crate::ui::widgets::list::ListState;

/// Rows kept per answer.
pub const MAX_CHILDREN: usize = 1000;
/// Without paging, more children than this announced by `indexedVariables` plus `namedVariables` are not asked for:
/// gdb takes about 73 bytes an element, and a frame over `MAX_BODY` ends the session. gdb takes `start` and `count`
/// only from a client that declares `supportsVariablePaging` in `initialize`; jetl does not, so against gdb the
/// request goes without them and only the fake adapter exercises the paged branch.
pub const UNPAGED_LIMIT: i64 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VarItem {
    pub name: String,
    pub value: String,
    pub ty: String,
    /// 0 or less: a leaf.
    pub reference: i64,
    /// The adapter's hint for the number of children: indexed plus named.
    pub children: i64,
}

/// A request for the children of `node`, to be sent by the App.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarAsk {
    pub node: usize,
    pub reference: i64,
    pub children: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarNode {
    pub item: VarItem,
    pub depth: usize,
    pub parent: Option<usize>,
    /// `None`: not asked, or the ask failed. `Some(vec![])`: known empty.
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    pub loading: bool,
    /// Why the last ask failed.
    pub failed: Option<String>,
    /// Children the answer had and the tree did not keep.
    pub hidden: usize,
}

impl VarNode {
    fn new(item: VarItem, depth: usize, parent: Option<usize>) -> Self {
        Self { item, depth, parent, children: None, expanded: false, loading: false, failed: None, hidden: 0 }
    }
}

#[derive(Debug, Default)]
pub struct VarTree {
    nodes: Vec<VarNode>,
    roots: Vec<usize>,
    asks: Vec<VarAsk>,
}

impl VarTree {
    /// The scopes, as roots. Only the first `MAX_CHILDREN` are kept.
    pub fn set_roots(&mut self, items: Vec<VarItem>) {
        self.nodes.clear();
        self.asks.clear();
        self.nodes.extend(items.into_iter().take(MAX_CHILDREN).map(|item| VarNode::new(item, 0, None)));
        self.roots = (0..self.nodes.len()).collect();
    }

    /// The visible nodes in display order: the roots, each followed by its children when expanded and known.
    pub fn rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            rows.push(n);
            if let Some(children) = self.nodes[n].children.as_ref().filter(|_| self.nodes[n].expanded) {
                stack.extend(children.iter().rev());
            }
        }
        rows
    }

    pub fn node(&self, i: usize) -> Option<&VarNode> {
        self.nodes.get(i)
    }

    /// Blank for a leaf or a known empty node, `▾` open, `▸` otherwise.
    pub fn marker(&self, i: usize) -> char {
        match self.nodes.get(i) {
            Some(n) if n.item.reference <= 0 || n.children.as_ref().is_some_and(Vec::is_empty) => ' ',
            Some(n) if n.expanded => '▾',
            Some(_) => '▸',
            None => ' ',
        }
    }

    /// Opens a node. Its children are asked once: not while an ask is out, and not when they are known.
    pub fn expand(&mut self, i: usize) {
        let Some(n) = self.nodes.get_mut(i) else { return };
        if n.item.reference <= 0 {
            return;
        }
        n.expanded = true;
        n.failed = None;
        if n.children.is_none() && !n.loading {
            n.loading = true;
            self.asks.push(VarAsk { node: i, reference: n.item.reference, children: n.item.children });
        }
    }

    /// Closes a node. An ask in flight stays in flight, and its answer is kept.
    pub fn collapse(&mut self, i: usize) {
        if let Some(n) = self.nodes.get_mut(i) {
            n.expanded = false;
        }
    }

    /// The answer to an ask: the children are stored (at most `MAX_CHILDREN`), whether the node is open or not.
    /// A node that is not waiting for one ignores it.
    pub fn set_children(&mut self, i: usize, mut items: Vec<VarItem>) {
        let Some(n) = self.nodes.get_mut(i).filter(|n| n.loading) else { return };
        let total = (n.item.children.max(0) as usize).max(items.len());
        items.truncate(MAX_CHILDREN);
        n.hidden = total - items.len();
        n.loading = false;
        n.failed = None;
        let depth = n.depth + 1;
        let first = self.nodes.len();
        self.nodes.extend(items.into_iter().map(|item| VarNode::new(item, depth, Some(i))));
        self.nodes[i].children = Some((first..self.nodes.len()).collect());
    }

    /// The ask failed: the node closes, keeps no children, and Right asks again.
    pub fn set_failed(&mut self, i: usize, message: String) {
        let Some(n) = self.nodes.get_mut(i).filter(|n| n.loading) else { return };
        n.loading = false;
        n.expanded = false;
        n.failed = Some(message);
    }

    pub fn take_asks(&mut self) -> Vec<VarAsk> {
        std::mem::take(&mut self.asks)
    }
}

/// The scopes that open by themselves: those hinted `arguments` or `locals` that are not `expensive` and have a
/// reference; if there are none, the first that is not `expensive` and has a reference. An `expensive` scope, or a
/// big one such as gdb's Registers, stays closed until the user opens it.
pub fn scopes_to_open(scopes: &[Scope]) -> Vec<usize> {
    let usable = |s: &Scope| !s.expensive && s.variables_reference > 0;
    let hinted: Vec<usize> = scopes
        .iter()
        .enumerate()
        .filter(|(_, s)| usable(s) && matches!(s.presentation_hint.as_str(), "arguments" | "locals"))
        .map(|(i, _)| i)
        .collect();
    if !hinted.is_empty() {
        return hinted;
    }
    scopes.iter().position(usable).into_iter().collect()
}

/// What the Variables tab holds for one selection seq.
#[derive(Debug, Default)]
pub enum VarsLoad {
    /// `scopes` was not asked.
    #[default]
    Idle,
    /// `scopes` is out.
    Asked,
    Shown,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct VarsView {
    pub tree: VarTree,
    pub list: ListState,
    /// The node of the selected row; the selection follows it when rows appear above.
    pub node: usize,
    pub load: VarsLoad,
}

impl VarsView {
    /// Lines the list up with the tree's visible rows and returns them (node indices). The selection stays on
    /// `node`, or on its nearest visible ancestor when a collapse hid it.
    pub fn sync(&mut self) -> Vec<usize> {
        let rows = self.tree.rows();
        if self.tree.node(self.node).is_none() {
            self.node = rows.first().copied().unwrap_or(0);
        }
        let mut at = Some(self.node);
        let i = loop {
            match at {
                Some(n) => match rows.iter().position(|&r| r == n) {
                    Some(i) => break i,
                    None => at = self.tree.node(n).and_then(|n| n.parent),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, reference: i64) -> VarItem {
        VarItem { name: name.into(), reference, ..VarItem::default() }
    }

    fn many(n: usize) -> Vec<VarItem> {
        (0..n).map(|i| item(&i.to_string(), 0)).collect()
    }

    /// Roots `a` (reference 10) and `b` (reference 11), `a` open with children `a1` (reference 12) and `a2`.
    fn tree() -> VarTree {
        let mut t = VarTree::default();
        t.set_roots(vec![item("a", 10), item("b", 11)]);
        t.expand(0);
        t.set_children(0, vec![item("a1", 12), item("a2", 0)]);
        t.take_asks();
        t
    }

    fn names(t: &VarTree) -> Vec<&str> {
        t.rows().into_iter().map(|i| t.node(i).unwrap().item.name.as_str()).collect()
    }

    #[test]
    fn rows_are_depth_first_and_skip_closed_nodes() {
        let mut t = tree();
        assert_eq!(names(&t), ["a", "a1", "a2", "b"]);
        t.expand(1);
        t.set_children(1, vec![item("b1", 0)]);
        t.expand(2);
        t.set_children(2, vec![item("a1x", 0)]);
        assert_eq!(names(&t), ["a", "a1", "a1x", "a2", "b", "b1"]);
        t.collapse(0);
        assert_eq!(names(&t), ["a", "b", "b1"], "a collapsed node hides its whole subtree");
        assert_eq!(t.node(2).unwrap().depth, 1);
        assert_eq!(t.node(t.node(2).unwrap().children.as_ref().unwrap()[0]).unwrap().depth, 2);
    }

    #[test]
    fn the_marker_says_leaf_open_closed() {
        let mut t = VarTree::default();
        t.set_roots(vec![item("leaf", 0), item("neg", -1), item("node", 5), item("empty", 6)]);
        assert_eq!(t.marker(0), ' ', "reference 0");
        assert_eq!(t.marker(1), ' ', "a negative reference");
        assert_eq!(t.marker(2), '▸', "closed, not asked");
        t.expand(2);
        assert_eq!(t.marker(2), '▾', "open and loading");
        t.collapse(2);
        assert_eq!(t.marker(2), '▸', "closed and loading");
        t.expand(2);
        t.set_children(2, vec![item("c", 0)]);
        assert_eq!(t.marker(2), '▾', "open with children");
        t.expand(3);
        t.set_children(3, Vec::new());
        assert_eq!(t.marker(3), ' ', "known empty");
        assert_eq!(t.marker(99), ' ');
    }

    #[test]
    fn a_leaf_is_never_asked() {
        let mut t = VarTree::default();
        t.set_roots(vec![item("zero", 0), item("neg", -3)]);
        t.expand(0);
        t.expand(1);
        assert!(t.take_asks().is_empty());
        for i in 0..2 {
            let n = t.node(i).unwrap();
            assert!(!n.expanded && !n.loading, "{n:?}");
        }
    }

    #[test]
    fn a_second_expand_does_not_ask_again() {
        let mut t = VarTree::default();
        t.set_roots(vec![VarItem { children: 7, ..item("a", 10) }]);
        t.expand(0);
        t.expand(0);
        assert_eq!(t.take_asks(), vec![VarAsk { node: 0, reference: 10, children: 7 }]);
        t.set_children(0, vec![item("c", 0)]);
        t.collapse(0);
        t.expand(0);
        assert!(t.take_asks().is_empty(), "known children are not asked again");
        assert_eq!(names(&t), ["a", "c"]);
    }

    #[test]
    fn a_node_collapsed_in_flight_stays_collapsed() {
        let mut t = VarTree::default();
        t.set_roots(vec![item("a", 10)]);
        t.expand(0);
        t.collapse(0);
        t.set_children(0, vec![item("c", 0)]);
        let n = t.node(0).unwrap();
        assert!(!n.expanded && !n.loading, "{n:?}");
        assert_eq!(n.children.as_ref().map(Vec::len), Some(1), "the answer is kept");
        assert_eq!(names(&t), ["a"]);
        assert_eq!(t.marker(0), '▸');
    }

    #[test]
    fn expand_collapse_expand_in_flight_asks_once_and_shows_the_answer() {
        let mut t = VarTree::default();
        t.set_roots(vec![item("a", 10)]);
        t.expand(0);
        t.collapse(0);
        assert!(t.node(0).unwrap().loading, "collapse leaves the request alone");
        t.expand(0);
        assert_eq!(t.take_asks().len(), 1);
        t.set_children(0, vec![item("c", 0)]);
        assert_eq!(names(&t), ["a", "c"]);
    }

    #[test]
    fn a_failure_can_be_asked_again() {
        let mut t = VarTree::default();
        t.set_roots(vec![item("a", 10)]);
        t.expand(0);
        t.take_asks();
        t.set_failed(0, "timed out".into());
        let n = t.node(0).unwrap();
        assert!(!n.expanded && !n.loading && n.children.is_none(), "{n:?}");
        assert_eq!(n.failed.as_deref(), Some("timed out"));
        assert_eq!(t.marker(0), '▸');
        t.expand(0);
        assert_eq!(t.take_asks().len(), 1, "Right asks again");
        let n = t.node(0).unwrap();
        assert!(n.failed.is_none() && n.loading && n.expanded, "{n:?}");
        // A failure answering nothing asked is ignored.
        t.set_children(0, Vec::new());
        t.set_failed(0, "late".into());
        assert!(t.node(0).unwrap().failed.is_none());
    }

    #[test]
    fn children_are_capped_and_counted() {
        let mut t = VarTree::default();
        t.set_roots(vec![VarItem { children: 5000, ..item("arr", 10) }, item("long", 11)]);
        t.expand(0);
        t.set_children(0, many(1500));
        assert_eq!(t.node(0).unwrap().children.as_ref().map(Vec::len), Some(MAX_CHILDREN));
        assert_eq!(t.node(0).unwrap().hidden, 4000, "counted from the hint");
        assert_eq!(t.rows().len(), 2 + MAX_CHILDREN);
        t.expand(1);
        t.set_children(1, many(1500));
        assert_eq!(t.node(1).unwrap().hidden, 500, "no hint: counted from the answer");
        let mut t = VarTree::default();
        t.set_roots(vec![VarItem { children: 3, ..item("arr", 10) }]);
        t.expand(0);
        t.set_children(0, many(3));
        assert_eq!(t.node(0).unwrap().hidden, 0);
        let mut t = VarTree::default();
        t.set_roots(Vec::from_iter((0..1500).map(|i| item(&i.to_string(), 0))));
        assert_eq!(t.rows().len(), MAX_CHILDREN, "roots are capped too");
    }

    fn scope(name: &str, hint: &str, reference: i64, expensive: bool) -> Scope {
        Scope { name: name.into(), presentation_hint: hint.into(), variables_reference: reference, expensive }
    }

    #[test]
    fn scopes_to_open_by_hint() {
        let gdb =
            [scope("Arguments", "arguments", 1, false), scope("Locals", "locals", 2, false), scope("Registers", "registers", 3, false)];
        assert_eq!(scopes_to_open(&gdb), [0, 1], "registers stay closed");
        let expensive_locals = [scope("Arguments", "arguments", 1, false), scope("Locals", "locals", 2, true)];
        assert_eq!(scopes_to_open(&expensive_locals), [0]);
        let no_hints = [scope("A", "", 1, false), scope("B", "", 2, false)];
        assert_eq!(scopes_to_open(&no_hints), [0], "the first one");
        let first_cheap = [scope("A", "", 1, true), scope("B", "", 2, false)];
        assert_eq!(scopes_to_open(&first_cheap), [1]);
        let all_expensive = [scope("A", "locals", 1, true), scope("B", "", 2, true)];
        assert!(scopes_to_open(&all_expensive).is_empty());
        let no_reference = [scope("A", "locals", 0, false), scope("B", "arguments", -1, false), scope("C", "", 4, false)];
        assert_eq!(scopes_to_open(&no_reference), [2], "a hint without a reference does not count");
        assert!(scopes_to_open(&[]).is_empty());
        let only_registers = [scope("Registers", "registers", 3, false)];
        assert_eq!(scopes_to_open(&only_registers), [0], "the fallback opens the first usable one");
    }

    #[test]
    fn the_selection_keeps_its_node_when_rows_change() {
        let mut v = VarsView::default();
        v.tree.set_roots(vec![item("a", 10), item("b", 11)]);
        v.sync();
        v.list.select(1);
        v.node = 1; // what a key does after it moves the list
        assert_eq!(v.sync(), [0, 1]);
        assert_eq!(v.node, 1);
        v.tree.expand(0);
        v.tree.set_children(0, vec![item("a1", 0), item("a2", 0)]);
        let rows = v.sync();
        assert_eq!(rows, [0, 2, 3, 1]);
        assert_eq!((v.list.selected, v.node, v.list.len), (3, 1, 4), "still on b");
    }

    #[test]
    fn a_collapse_moves_the_selection_to_the_nearest_shown_ancestor() {
        let mut t = tree();
        t.expand(2);
        assert_eq!(t.node(2).unwrap().item.name, "a1");
        t.set_children(2, vec![item("deep", 0)]);
        let mut v = VarsView { tree: t, ..VarsView::default() };
        let rows = v.sync();
        let deep = *rows.last().unwrap();
        assert_eq!(v.tree.node(deep).unwrap().item.name, "b", "b is last");
        let deep = rows.iter().copied().find(|&r| v.tree.node(r).unwrap().item.name == "deep").unwrap();
        v.node = deep;
        v.sync();
        assert_eq!(v.node, deep);
        v.tree.collapse(2);
        v.sync();
        assert_eq!(v.tree.node(v.node).unwrap().item.name, "a1", "one level up");
        v.node = deep;
        v.tree.expand(2);
        v.sync();
        v.node = deep;
        v.tree.collapse(2);
        v.tree.collapse(0);
        v.sync();
        assert_eq!(v.tree.node(v.node).unwrap().item.name, "a", "two levels up");
        assert_eq!(v.list.selected, 0);
    }

    #[test]
    fn an_unknown_node_is_ignored() {
        let mut t = tree();
        let before = names(&t).len();
        t.expand(99);
        t.collapse(99);
        t.set_children(99, vec![item("x", 0)]);
        t.set_failed(99, "x".into());
        assert!(t.take_asks().is_empty());
        assert!(t.node(99).is_none());
        assert_eq!(names(&t).len(), before);
        let mut v = VarsView { node: 42, ..VarsView::default() };
        assert!(v.sync().is_empty());
        let mut v = VarsView { tree: tree(), node: 42, ..VarsView::default() };
        v.sync();
        assert_eq!(v.node, 0, "a node that is not in the tree falls back to the first row");
    }
}
