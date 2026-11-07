//! The model of the Debug window's Variables tab: the scopes of one frame as a tree whose children are read one
//! level at a time. No drawing and no IO. It is `HierarchyTree`'s shape (an append-only arena, `children: None`
//! until asked), with these differences: a leaf is known from `variablesReference <= 0` and never asked; an answer
//! does not force `expanded`, so a collapse while the request is out holds; there is no tree id, since the panel
//! replaces the whole view when its selection seq rises; a node can hold a failure and a count of children left
//! out; children are capped; a node's children can come in pages, asked by a load-more row that is a node of its
//! own and not one of the children; and asks are queued here for the App to send, as the panel has no sender.

use super::protocol::Scope;
use crate::ui::widgets::list::ListState;

/// Rows kept per answer, and the size of a page asked with start and count.
pub const MAX_CHILDREN: usize = 1000;
/// More children than this, announced by `indexedVariables` plus `namedVariables`, are not asked for unless the
/// adapter is known to honour `start` and `count` (`Live::trusted_paging`, gdb only): an adapter that ignores them
/// answers whole, gdb takes about 73 bytes an element, and a frame over `MAX_BODY` ends the session.
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
    /// 0 for a first ask, `children.len()` for a later page.
    pub start: usize,
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
    /// Children the adapter has and the tree did not keep: asked for or not.
    pub hidden: usize,
    /// The load-more row, while the adapter has more and honours pages. Not in `children`.
    pub more: Option<usize>,
}

impl VarNode {
    fn new(item: VarItem, depth: usize, parent: Option<usize>) -> Self {
        Self { item, depth, parent, children: None, expanded: false, loading: false, failed: None, hidden: 0, more: None }
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
                stack.extend(self.nodes[n].more);
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
            self.asks.push(VarAsk { node: i, reference: n.item.reference, children: n.item.children, start: 0 });
        }
    }

    /// Closes a node. An ask in flight stays in flight, and its answer is kept.
    pub fn collapse(&mut self, i: usize) {
        if let Some(n) = self.nodes.get_mut(i) {
            n.expanded = false;
        }
    }

    /// The answer to an ask: the children are stored (at most `MAX_CHILDREN` per answer), whether the node is open or
    /// not. A later page is appended. A node that is not waiting for one ignores it.
    pub fn set_children(&mut self, i: usize, mut items: Vec<VarItem>) {
        let Some(n) = self.nodes.get_mut(i).filter(|n| n.loading) else { return };
        let paged = n.item.children > MAX_CHILDREN as i64;
        let hint = n.item.children.max(0) as usize;
        let depth = n.depth + 1;
        let mut children = n.children.take().unwrap_or_default();
        let row = n.more.take();
        n.loading = false;
        n.failed = None;
        // The adapter ignored `start`: this is the first page again.
        let repeated = children.first().zip(items.first()).is_some_and(|(&c, v)| self.nodes[c].item.name == v.name);
        if repeated {
            items.clear();
        }
        let got = items.len();
        // The adapter ignored `count`.
        let over = got > MAX_CHILDREN;
        // The specification: fewer than `count` means no more. The empty answer past the end is one of these.
        let ended = paged && !repeated && got < MAX_CHILDREN;
        let total = if ended { children.len() + got } else { hint.max(children.len() + got) };
        items.truncate(MAX_CHILDREN);
        let first = self.nodes.len();
        self.nodes.extend(items.into_iter().map(|item| VarNode::new(item, depth, Some(i))));
        children.extend(first..self.nodes.len());
        let hidden = total - children.len();
        let more = (paged && !repeated && !over && hidden > 0).then(|| {
            row.unwrap_or_else(|| {
                self.nodes.push(VarNode::new(VarItem::default(), depth, Some(i)));
                self.nodes.len() - 1
            })
        });
        let n = &mut self.nodes[i];
        n.children = Some(children);
        n.hidden = hidden;
        n.more = more;
    }

    /// The ask failed: the node closes, keeps no children, and Right asks again.
    pub fn set_failed(&mut self, i: usize, message: String) {
        let Some(n) = self.nodes.get_mut(i).filter(|n| n.loading) else { return };
        n.loading = false;
        // A later page: what is loaded stays, and the row says why.
        if n.children.is_some() {
            if let Some(row) = n.more {
                self.nodes[row].failed = Some(message);
            }
            return;
        }
        n.expanded = false;
        n.failed = Some(message);
    }

    /// The node whose load-more row is `row`.
    pub fn more_of(&self, row: usize) -> Option<usize> {
        let parent = self.nodes.get(row)?.parent?;
        (self.nodes[parent].more == Some(row)).then_some(parent)
    }

    /// Asks the next page of the node whose load-more row is `row`, unless an ask is out. False when `row` is
    /// not such a row.
    pub fn load_more(&mut self, row: usize) -> bool {
        let Some(i) = self.more_of(row) else { return false };
        let n = &mut self.nodes[i];
        if !n.loading {
            n.loading = true;
            let start = n.children.as_ref().map_or(0, Vec::len);
            self.asks.push(VarAsk { node: i, reference: n.item.reference, children: n.item.children, start });
            self.nodes[row].failed = None;
        }
        true
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
    /// An answer for node `i`. A selection on its load-more row moves to the first new child, which takes the
    /// row's place on screen, or to the last child when none came.
    pub fn show_children(&mut self, i: usize, items: Vec<VarItem>) {
        let on_row = self.tree.node(i).is_some_and(|n| n.loading && n.more == Some(self.node));
        let before = self.tree.node(i).and_then(|n| n.children.as_ref()).map_or(0, Vec::len);
        self.tree.set_children(i, items);
        if on_row {
            let children = self.tree.node(i).and_then(|n| n.children.as_ref());
            self.node = children.and_then(|c| c.get(before).or(c.last()).copied()).unwrap_or(i);
        }
    }

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
        assert_eq!(t.take_asks(), vec![VarAsk { node: 0, reference: 10, children: 7, start: 0 }]);
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

    /// Leaves named `start`, `start + 1`, ... as an adapter lists elements of an array.
    fn from(start: usize, n: usize) -> Vec<VarItem> {
        (start..start + n).map(|i| item(&i.to_string(), 0)).collect()
    }

    /// One root `r` of hint `n`, reference 10, opened and its first ask taken.
    fn arr(n: i64) -> VarTree {
        let mut t = VarTree::default();
        t.set_roots(vec![VarItem { children: n, ..item("r", 10) }]);
        t.expand(0);
        t.take_asks();
        t
    }

    fn row(t: &VarTree) -> usize {
        t.node(0).unwrap().more.expect("a load-more row")
    }

    fn kept(t: &VarTree) -> usize {
        t.node(0).unwrap().children.as_ref().map_or(0, Vec::len)
    }

    #[test]
    fn a_later_page_appends() {
        let mut t = arr(2500);
        t.set_children(0, from(0, 1000));
        assert_eq!(t.node(0).unwrap().hidden, 1500);
        let r = row(&t);
        assert_eq!(t.rows().len(), 1 + 1000 + 1);
        assert_eq!(t.rows().last(), Some(&r), "the row comes after the children");
        assert_eq!(t.marker(r), ' ');
        let n = t.node(r).unwrap();
        assert_eq!((n.depth, n.parent, n.item.name.as_str()), (1, Some(0), ""));
        assert!(!t.node(0).unwrap().children.as_ref().unwrap().contains(&r), "not one of the children");
        assert!(t.load_more(r));
        assert_eq!(t.take_asks(), vec![VarAsk { node: 0, reference: 10, children: 2500, start: 1000 }]);
        t.set_children(0, from(1000, 1000));
        let names: Vec<&str> =
            t.node(0).unwrap().children.as_ref().unwrap().iter().map(|&c| t.node(c).unwrap().item.name.as_str()).collect();
        let want: Vec<String> = (0..2000).map(|i| i.to_string()).collect();
        assert_eq!(names, want, "in order");
        assert_eq!(t.node(0).unwrap().hidden, 500);
        assert_eq!(row(&t), r, "the same row");
        assert!(t.load_more(r));
        assert_eq!(t.take_asks()[0].start, 2000);
        t.set_children(0, from(2000, 500));
        assert_eq!((t.node(0).unwrap().hidden, t.node(0).unwrap().more), (0, None));
        assert_eq!(t.rows().len(), 1 + 2500);
    }

    #[test]
    fn two_presses_ask_once() {
        let mut t = arr(2500);
        t.set_children(0, from(0, 1000));
        let r = row(&t);
        assert!(t.load_more(r) && t.load_more(r), "both are the row");
        assert_eq!(t.take_asks().len(), 1);
        t.set_children(0, from(1000, 1000));
        t.set_children(0, from(2000, 500));
        assert_eq!((kept(&t), t.node(0).unwrap().hidden), (2000, 500), "an answer with nothing asked changes nothing");
    }

    #[test]
    fn the_row_is_not_a_node_to_open() {
        let mut t = arr(2500);
        t.set_children(0, from(0, 1000));
        let r = row(&t);
        t.expand(r);
        assert!(t.take_asks().is_empty());
        assert!(!t.load_more(0), "a node is not a row");
        assert!(!t.load_more(99));
        assert!(!t.load_more(1), "nor is a child");
        assert!(t.take_asks().is_empty());
    }

    #[test]
    fn a_failed_page_keeps_what_is_loaded() {
        let mut t = arr(2500);
        t.set_children(0, from(0, 1000));
        let r = row(&t);
        t.load_more(r);
        t.take_asks();
        t.set_failed(0, "timed out".into());
        let n = t.node(0).unwrap();
        assert!(n.expanded && !n.loading && n.failed.is_none() && n.hidden == 1500, "{n:?}");
        assert_eq!((kept(&t), t.marker(0)), (1000, '▾'));
        assert_eq!(t.node(r).unwrap().failed.as_deref(), Some("timed out"));
        assert!(t.load_more(r));
        assert!(t.node(r).unwrap().failed.is_none(), "asking again clears it");
        assert_eq!(t.take_asks(), vec![VarAsk { node: 0, reference: 10, children: 2500, start: 1000 }]);
        t.set_children(0, from(1000, 1000));
        let all: std::collections::HashSet<&str> =
            t.node(0).unwrap().children.as_ref().unwrap().iter().map(|&c| t.node(c).unwrap().item.name.as_str()).collect();
        assert_eq!((kept(&t), all.len()), (2000, 2000), "no duplicate");
        // A failure with nothing asked is ignored.
        t.set_failed(0, "late".into());
        assert!(t.node(row(&t)).unwrap().failed.is_none() && t.node(0).unwrap().failed.is_none());
        assert!(t.node(0).unwrap().expanded);
    }

    #[test]
    fn past_the_end_ends_the_row() {
        let mut t = arr(3000);
        t.set_children(0, from(0, 1000));
        t.load_more(row(&t));
        t.set_children(0, Vec::new());
        let n = t.node(0).unwrap();
        assert_eq!((n.hidden, n.more, kept(&t), t.marker(0)), (0, None, 1000, '▾'));
        assert_eq!(t.rows().len(), 1 + 1000);
    }

    #[test]
    fn a_short_page_ends_the_row() {
        let mut t = arr(5000);
        t.set_children(0, from(0, 300));
        let n = t.node(0).unwrap();
        assert_eq!((n.hidden, n.more, kept(&t)), (0, None, 300), "a short first page");
        let mut t = arr(5000);
        t.set_children(0, from(0, 1000));
        t.load_more(row(&t));
        t.set_children(0, from(1000, 300));
        let n = t.node(0).unwrap();
        assert_eq!((n.hidden, n.more, kept(&t)), (0, None, 1300), "a short later page");
    }

    #[test]
    fn an_exact_end_asks_nothing_more() {
        let mut t = arr(2000);
        t.set_children(0, from(0, 1000));
        t.load_more(row(&t));
        t.set_children(0, from(1000, 1000));
        let n = t.node(0).unwrap();
        assert_eq!((n.hidden, n.more, kept(&t)), (0, None, 2000), "two full pages");
        let mut t = arr(1001);
        t.set_children(0, from(0, 1000));
        let n = t.node(0).unwrap();
        assert_eq!((n.hidden, kept(&t)), (1, 1000));
        assert!(n.more.is_some(), "one is left");
    }

    #[test]
    fn no_row_without_a_page() {
        // (label, hint, rows answered, kept, hidden)
        let table = [
            ("exactly one page", 1000, 1000, 1000, 0),
            ("no hint", 0, 1500, 1000, 500),
            ("the adapter ignored count", 5000, 1500, 1000, 4000),
            ("a hint under the answer", 7, 1, 1, 6),
        ];
        let mut wrong = Vec::new();
        for (label, hint, got, want_kept, want_hidden) in table {
            let mut t = arr(hint);
            t.set_children(0, from(0, got));
            let n = t.node(0).unwrap();
            if (kept(&t), n.hidden, n.more) != (want_kept, want_hidden, None) {
                wrong.push((label, kept(&t), n.hidden, n.more));
            }
        }
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn a_repeated_page_ends_the_row() {
        let mut t = arr(5000);
        t.set_children(0, from(0, 1000));
        t.load_more(row(&t));
        t.set_children(0, from(0, 1000));
        let n = t.node(0).unwrap();
        assert_eq!((kept(&t), n.hidden, n.more), (1000, 4000, None));
        assert!(!n.loading);
    }

    #[test]
    fn a_later_page_too_long_is_cut_and_ends() {
        let mut t = arr(5000);
        t.set_children(0, from(0, 1000));
        t.load_more(row(&t));
        t.set_children(0, from(1000, 1500));
        let n = t.node(0).unwrap();
        assert_eq!((kept(&t), n.hidden, n.more), (2000, 3000, None));
    }

    #[test]
    fn a_closed_node_keeps_its_pages() {
        let mut t = arr(2500);
        t.set_children(0, from(0, 1000));
        let r = row(&t);
        t.load_more(r);
        t.take_asks();
        t.collapse(0);
        assert!(t.node(0).unwrap().loading, "a collapse leaves the ask alone");
        assert_eq!(t.rows(), [0], "the row goes with the children");
        t.set_children(0, from(1000, 1000));
        let n = t.node(0).unwrap();
        assert!(!n.expanded && !n.loading, "{n:?}");
        assert_eq!(kept(&t), 2000, "the answer is kept");
        t.expand(0);
        assert!(t.take_asks().is_empty(), "known children are not asked again");
        assert_eq!(t.rows().len(), 1 + 2000 + 1);
        assert_eq!(t.rows().last(), Some(&r));
    }

    fn view_on_row() -> (VarsView, usize) {
        let mut v = VarsView { tree: arr(2500), ..VarsView::default() };
        v.show_children(0, from(0, 1000));
        let r = row(&v.tree);
        v.node = r;
        v.sync();
        assert_eq!((v.list.selected, v.node), (1001, r), "the row is last");
        (v, r)
    }

    #[test]
    fn the_selection_leaves_the_row_for_the_first_new_child() {
        let (mut v, r) = view_on_row();
        v.tree.load_more(r);
        v.show_children(0, from(1000, 1000));
        v.sync();
        let first = v.tree.node(0).unwrap().children.as_ref().unwrap()[1000];
        assert_eq!(v.tree.node(v.node).unwrap().item.name, "1000");
        assert_eq!((v.node, v.list.selected), (first, 1001), "the row's old place on screen");
        // An empty page: the last child.
        let (mut v, r) = view_on_row();
        v.tree.load_more(r);
        v.show_children(0, Vec::new());
        v.sync();
        assert_eq!(v.tree.node(v.node).unwrap().item.name, "999");
        assert_eq!(v.list.selected, 1000);
        // The selection is elsewhere: it stays.
        let (mut v, r) = view_on_row();
        v.tree.load_more(r);
        v.node = 5;
        v.sync();
        v.show_children(0, from(1000, 1000));
        v.sync();
        assert_eq!((v.node, v.list.selected), (5, 5));
        // Nothing asked: the answer is ignored, and the selection stays on the row.
        let (mut v, r) = view_on_row();
        v.show_children(0, from(1000, 1000));
        v.sync();
        assert_eq!((v.node, v.list.selected, v.list.len), (r, 1001, 1002));
    }

    #[test]
    fn a_later_page_leaves_a_collapsed_selection_on_the_parent() {
        let (mut v, r) = view_on_row();
        v.tree.load_more(r);
        v.tree.collapse(0);
        v.sync();
        assert_eq!(v.node, 0, "the row is hidden: the parent");
        v.show_children(0, from(1000, 1000));
        v.sync();
        assert_eq!((v.node, v.list.selected), (0, 0));
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
