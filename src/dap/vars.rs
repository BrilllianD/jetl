//! The model of the Debug window's Variables tab: the scopes of one frame as a tree whose children are read one
//! level at a time. No drawing and no IO. It is `HierarchyTree`'s shape (an append-only arena, `children: None`
//! until asked), with these differences: a leaf is known from `variablesReference <= 0` and never asked; an answer
//! does not force `expanded`, so a collapse while the request is out holds; there is no tree id, since the panel
//! replaces the whole view when its selection seq rises; a node can hold a failure and a count of children left
//! out; children are capped; a node's children can come in pages, asked by a load-more row that is a node of its
//! own and not one of the children; and asks are queued here for the App to send, as the panel has no sender. What
//! the user opened survives the tree: `KeptPaths` keeps name paths per function, and answers reopen them one level at
//! a time within a budget. Watches are roots above the scopes, flagged `watch`: `set_roots` leaves them in place, a
//! watch's name is its expression, and its value comes from its own `evaluate`, not from a scope's `variables`.

use super::protocol::Scope;
use crate::ui::widgets::list::ListState;

/// Rows kept per answer, and the size of a page asked with start and count.
pub const MAX_CHILDREN: usize = 1000;
/// More children than this, announced by `indexedVariables` plus `namedVariables`, are not asked for unless the
/// adapter is known to honour `start` and `count` (`Live::trusted_paging`, gdb only): an adapter that ignores them
/// answers whole, gdb takes about 73 bytes an element, and a frame over `MAX_BODY` ends the session.
pub const UNPAGED_LIMIT: i64 = 100_000;
/// What one stop may spend on reopening, in about milliseconds of gdb's time. A step waits behind what is asked.
pub const REOPEN_BUDGET: usize = 200;
/// Paths kept per function, oldest dropped.
pub const MAX_KEPT_PATHS: usize = 64;
/// Functions kept, least recently changed dropped.
pub const MAX_KEPT_FRAMES: usize = 32;

/// What the user opened, closed and selected in one function's tree. Changed only by the user's actions, so a
/// stop mid-reopen cannot shrink it. Holds names, never a reference: gdb reuses those after a stop.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeptFrame {
    /// Name paths the user opened, oldest first.
    pub open: Vec<Vec<String>>,
    /// Scopes the user closed, which would open by themselves.
    pub shut: Vec<String>,
    pub selected: Option<Vec<String>>,
}

impl KeptFrame {
    /// The user opened or closed `path`.
    pub fn record(&mut self, path: Vec<String>, open: bool) {
        self.open.retain(|p| *p != path);
        if let [scope] = path.as_slice() {
            self.shut.retain(|s| s != scope);
            if !open {
                self.shut.push(scope.clone());
            }
        }
        if open {
            self.open.push(path);
            if self.open.len() > MAX_KEPT_PATHS {
                self.open.remove(0);
            }
        }
    }
}

/// One `KeptFrame` per function name, for one session.
#[derive(Debug, Default)]
pub struct KeptPaths {
    /// Least recently changed first.
    frames: Vec<(String, KeptFrame)>,
}

impl KeptPaths {
    pub fn frame(&self, name: &str) -> Option<&KeptFrame> {
        self.frames.iter().find(|(n, _)| n == name).map(|(_, f)| f)
    }

    pub fn frame_mut(&mut self, name: &str) -> &mut KeptFrame {
        let kept = match self.frames.iter().position(|(n, _)| n == name) {
            Some(i) => self.frames.remove(i).1,
            None => KeptFrame::default(),
        };
        self.frames.push((name.to_owned(), kept));
        if self.frames.len() > MAX_KEPT_FRAMES {
            self.frames.remove(0);
        }
        &mut self.frames.last_mut().expect("just pushed").1
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }
}

/// A name without the ` #2` gdb adds to a name that repeats in one container.
fn base_name(name: &str) -> &str {
    match name.rsplit_once(" #") {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => name,
    }
}

/// What asking this node's first page costs gdb, in about milliseconds, from what the hint and the value say:
/// a page is 20 for 1000 rows. A printer (a non-empty value on a container) above one page builds all its
/// children before it answers the first page, about 12.5 per 1000 rows for a `std::map`; a C array does not.
/// Measured under gdb 17.2. A hint of 0 is priced as small, which no adapter is obliged to make true.
fn reopen_price(item: &VarItem) -> usize {
    let rows = item.children.max(0) as usize;
    let page = 1 + rows.min(MAX_CHILDREN) / 50;
    let printer = rows > MAX_CHILDREN && !item.value.is_empty();
    page + if printer { rows / 80 } else { 0 }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VarItem {
    pub name: String,
    pub value: String,
    pub ty: String,
    /// 0 or less: a leaf.
    pub reference: i64,
    /// The adapter's hint for the number of children: indexed plus named.
    pub children: i64,
    /// A scope the adapter said is expensive to read, so it is never reopened by itself.
    pub expensive: bool,
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
    /// A watch root: `item.name` is the expression, and the value comes from an `evaluate`.
    pub watch: bool,
}

impl VarNode {
    fn new(item: VarItem, depth: usize, parent: Option<usize>) -> Self {
        Self { item, depth, parent, children: None, expanded: false, loading: false, failed: None, hidden: 0, more: None, watch: false }
    }
}

#[derive(Debug, Default)]
pub struct VarTree {
    nodes: Vec<VarNode>,
    roots: Vec<usize>,
    asks: Vec<VarAsk>,
    /// Watch roots whose `evaluate` the App has not taken yet.
    watch_asks: Vec<usize>,
}

impl VarTree {
    /// The scopes, as roots after the watch roots; returns the node of the first scope. With no watch root the
    /// arena and the asks are cleared first. Only the first `MAX_CHILDREN` scopes are kept.
    pub fn set_roots(&mut self, items: Vec<VarItem>) -> usize {
        let nodes = &self.nodes;
        self.roots.retain(|&r| nodes[r].watch);
        if self.roots.is_empty() {
            self.nodes.clear();
            self.asks.clear();
            self.watch_asks.clear();
        }
        let first = self.nodes.len();
        self.nodes.extend(items.into_iter().take(MAX_CHILDREN).map(|item| VarNode::new(item, 0, None)));
        self.roots.extend(first..self.nodes.len());
        first
    }

    /// A watch root for `expression`, after the last watch root and before the scopes: its evaluate is queued, and
    /// it is ` …` (`loading`) until `set_watch`.
    pub fn add_watch(&mut self, expression: String) -> usize {
        let mut node = VarNode::new(VarItem { name: expression, ..VarItem::default() }, 0, None);
        node.watch = true;
        node.loading = true;
        self.nodes.push(node);
        let i = self.nodes.len() - 1;
        let at = self.roots.iter().rposition(|&r| self.nodes[r].watch).map_or(0, |p| p + 1);
        self.roots.insert(at, i);
        self.watch_asks.push(i);
        i
    }

    /// The watch roots whose evaluate is queued, oldest first.
    pub fn take_watch_asks(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.watch_asks)
    }

    /// The evaluate answer of watch root `i`: the item's value, type, reference and hint, the name staying the
    /// expression; or the failure. False, and nothing changes, unless `i` is a watch root still in `roots` and loading.
    pub fn set_watch(&mut self, i: usize, result: Result<VarItem, String>) -> bool {
        if !self.is_watch(i) || !self.nodes[i].loading {
            return false;
        }
        let n = &mut self.nodes[i];
        n.loading = false;
        match result {
            Ok(item) => {
                n.item = VarItem { name: std::mem::take(&mut n.item.name), ..item };
                n.failed = None;
            }
            Err(m) => {
                n.item.reference = 0;
                n.failed = Some(m);
            }
        }
        true
    }

    /// Whether `i` is a watch root still in `roots`. Never a child or a load-more row.
    pub fn is_watch(&self, i: usize) -> bool {
        self.nodes.get(i).is_some_and(|n| n.watch) && self.roots.contains(&i)
    }

    /// Takes watch root `i` out of `roots`; its nodes stay in the arena, unreachable. False when `i` is not one.
    pub fn remove_watch(&mut self, i: usize) -> bool {
        if !self.is_watch(i) {
            return false;
        }
        self.roots.retain(|&r| r != i);
        self.watch_asks.retain(|&r| r != i);
        true
    }

    /// The visible nodes in display order: the roots, each followed by its children when expanded and known, then by
    /// its load-more row if it has one. That row is a node with no item of its own: `more_of` tells it apart.
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

    /// Loads a node's children without opening it. Like `expand`, nothing is asked twice.
    pub fn fetch(&mut self, i: usize) {
        let Some(n) = self.nodes.get_mut(i) else { return };
        if n.item.reference <= 0 || n.children.is_some() || n.loading {
            return;
        }
        n.loading = true;
        self.asks.push(VarAsk { node: i, reference: n.item.reference, children: n.item.children, start: 0 });
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

    /// The ask failed. For a first page the node closes, keeps no children, and Right asks again; for a later page it
    /// stays open with what it has, and its load-more row carries the message.
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

    /// The names from the root down to `i`. Empty when `i` is no node. A load-more row has an empty name, so its path
    /// ends in `""`: ask `more_of` first, and take the container's.
    pub fn path_of(&self, i: usize) -> Vec<String> {
        let mut path = Vec::new();
        let mut at = Some(i);
        while let Some(n) = at.and_then(|i| self.nodes.get(i)) {
            path.push(n.item.name.clone());
            at = n.parent;
        }
        path.reverse();
        path
    }

    /// `i`'s path, or `None` when a name on it is ambiguous, so the path cannot be told from another across stops, or
    /// when `i` is a load-more row, which names nothing.
    pub fn stable_path_of(&self, i: usize) -> Option<Vec<String>> {
        if self.more_of(i).is_some() {
            return None;
        }
        let mut at = Some(i);
        while let Some(n) = at {
            if self.ambiguous(n) {
                return None;
            }
            at = self.nodes.get(n)?.parent;
        }
        Some(self.path_of(i))
    }

    /// Whether `i`'s name means more than one thing at its level. A literal repeat always does. Directly under a
    /// scope, gdb's ` #2` does too: the innermost variable takes the bare name, so it changes with the block. Deeper,
    /// a container's shape comes from its type, and ` #2` is stable.
    fn ambiguous(&self, i: usize) -> bool {
        let Some(n) = self.nodes.get(i) else { return true };
        let siblings = match n.parent {
            Some(p) => self.nodes[p].children.as_deref().unwrap_or_default(),
            None => self.roots.as_slice(),
        };
        let (name, base) = (&n.item.name, base_name(&n.item.name));
        let same = |s: &&usize| {
            let other = &self.nodes[**s].item.name;
            other == name || (n.depth == 1 && base_name(other) == base)
        };
        siblings.iter().filter(same).count() > 1
    }

    /// The first of `among` called `name`.
    fn named_in(&self, among: &[usize], name: &str) -> Option<usize> {
        among.iter().copied().find(|&c| self.nodes[c].item.name == name)
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
    /// What this tree spent on reopening.
    pub spent: usize,
    /// The root nodes of the scopes that open by themselves, shut ones too.
    pub auto: Vec<usize>,
}

/// One top-level variable of an auto scope, as the editor shows it at line ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopValue {
    pub name: String,
    pub value: String,
    /// The name means more than one variable in its scope (gdb's shadowing).
    pub shadowed: bool,
}

impl VarsView {
    /// How many variables the auto scopes hold so far.
    pub fn auto_loaded(&self) -> usize {
        self.auto.iter().filter_map(|&i| self.tree.node(i)).map(|n| n.children.as_ref().map_or(0, Vec::len)).sum()
    }

    /// The variables of the auto scopes, in scope order. A name with gdb's ` #N` is left out, and so is a leaf with
    /// no value. A container with no value shows `{…}`.
    pub fn top_values(&self) -> Vec<TopValue> {
        let mut out = Vec::new();
        for &root in &self.auto {
            let Some(children) = self.tree.node(root).and_then(|n| n.children.as_ref()) else { continue };
            for &c in children {
                let Some(n) = self.tree.node(c) else { continue };
                let item = &n.item;
                if base_name(&item.name) != item.name {
                    continue;
                }
                let value = if !item.value.is_empty() {
                    item.value.clone()
                } else if item.reference > 0 {
                    "{…}".to_string()
                } else {
                    continue;
                };
                out.push(TopValue { name: item.name.clone(), value, shadowed: self.tree.ambiguous(c) });
            }
        }
        out
    }

    /// The scopes arrived. `open` are the ones that open by themselves, for nothing from the budget, unless the
    /// user closed them; then the saved root paths, and the selection if it is a root.
    pub fn reopen_roots(&mut self, open: &[usize], kept: Option<&KeptFrame>) {
        for &i in open {
            let shut = kept.is_some_and(|k| self.tree.node(i).is_some_and(|n| k.shut.contains(&n.item.name)));
            if !shut {
                self.tree.expand(i);
            }
        }
        let Some(kept) = kept else { return };
        // A watch root reopens when its own answer lands.
        let roots: Vec<usize> = self.tree.roots.iter().copied().filter(|&r| !self.tree.nodes[r].watch).collect();
        self.reopen_among(&roots, &[], kept);
    }

    /// An answer for `i` was stored, and its children from `before` on are new: the saved paths one level under
    /// `i` open, and the selection moves if its path runs through one of them. A closed node is left alone.
    pub fn reopen_under(&mut self, i: usize, before: usize, kept: &KeptFrame) {
        let Some(n) = self.tree.node(i).filter(|n| n.expanded) else { return };
        let Some(children) = n.children.as_ref() else { return };
        // Under a shadowed name nothing opens, whoever opened `i`.
        if self.tree.stable_path_of(i).is_none() {
            return;
        }
        let new = children[before.min(children.len())..].to_vec();
        let prefix = self.tree.path_of(i);
        self.reopen_among(&new, &prefix, kept);
    }

    /// Watch root `i` was answered: the selection moves to it and it reopens, from the kept paths, as a root would.
    pub fn reopen_watch(&mut self, i: usize, kept: &KeptFrame) {
        self.reopen_among(&[i], &[], kept);
    }

    fn reopen_among(&mut self, new: &[usize], prefix: &[String], kept: &KeptFrame) {
        let depth = prefix.len();
        if let Some(selected) = kept.selected.as_ref().filter(|s| s.len() > depth && s.starts_with(prefix))
            && let Some(c) = self.tree.named_in(new, &selected[depth])
        {
            self.node = c;
        }
        // Newest first, so the budget goes to what the user opened last.
        for path in kept.open.iter().rev().filter(|p| p.len() == depth + 1 && p.starts_with(prefix)) {
            if let Some(c) = self.tree.named_in(new, &path[depth]) {
                self.reopen_node(c, kept);
            }
        }
    }

    fn reopen_node(&mut self, c: usize, kept: &KeptFrame) {
        let Some(n) = self.tree.node(c) else { return };
        if n.item.reference <= 0 || n.item.expensive || self.tree.ambiguous(c) {
            return;
        }
        if n.children.is_none() && !n.loading {
            let price = reopen_price(&n.item);
            if self.spent + price > REOPEN_BUDGET {
                return;
            }
            self.spent += price;
        }
        self.tree.expand(c);
        // Known children only when the user opened a node whose answer had already landed.
        self.reopen_under(c, 0, kept);
    }

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
    fn fetch_asks_once_without_opening() {
        let mut t = tree();
        t.fetch(1);
        t.fetch(1);
        let asks = t.take_asks();
        assert_eq!(asks.iter().map(|a| (a.node, a.reference)).collect::<Vec<_>>(), [(1, 11)], "one ask");
        assert!(!t.node(1).unwrap().expanded);
        t.set_children(1, vec![item("b1", 0)]);
        assert_eq!(t.node(1).unwrap().children.as_ref().map(Vec::len), Some(1));
        assert!(!t.node(1).unwrap().expanded, "stored but still shut");
        t.fetch(1);
        t.fetch(0);
        t.fetch(3);
        assert!(t.take_asks().is_empty(), "known children, a leaf and a missing node ask nothing");
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

    fn view(names: &[&str]) -> VarsView {
        let mut v = VarsView::default();
        v.tree.set_roots(names.iter().enumerate().map(|(i, n)| item(n, 10 + i as i64)).collect());
        v
    }

    fn path(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| (*s).to_owned()).collect()
    }

    fn open(paths: &[&[&str]]) -> KeptFrame {
        KeptFrame { open: paths.iter().map(|p| path(p)).collect(), ..KeptFrame::default() }
    }

    /// The answer for `node` is stored, and the paths under it reopen, as the panel does.
    fn stored(v: &mut VarsView, node: usize, items: Vec<VarItem>, kept: &KeptFrame) {
        let before = v.tree.node(node).and_then(|n| n.children.as_ref()).map_or(0, Vec::len);
        v.show_children(node, items);
        v.reopen_under(node, before, kept);
    }

    /// `(node, reference, start)` of each ask since the last call.
    fn asked(v: &mut VarsView) -> Vec<(usize, i64, usize)> {
        v.tree.take_asks().into_iter().map(|a| (a.node, a.reference, a.start)).collect()
    }

    /// Node `i` of `v`, by name under the root `0`'s children: the child of `parent` called `name`.
    fn child(v: &VarsView, parent: usize, name: &str) -> usize {
        let children = v.tree.node(parent).unwrap().children.as_ref().unwrap();
        v.tree.named_in(children, name).unwrap_or_else(|| panic!("no child {name}"))
    }

    #[test]
    fn base_name_takes_only_a_counter() {
        let table = [
            ("x #2", "x"),
            ("x #12", "x"),
            ("x", "x"),
            ("x #", "x #"),
            ("x #a", "x #a"),
            ("#2", "#2"),
            ("a # 2", "a # 2"),
            ("x #2 #3", "x #2"),
        ];
        let wrong: Vec<_> = table.iter().filter(|(name, want)| base_name(name) != *want).map(|(n, w)| (*n, *w, base_name(n))).collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn a_path_runs_from_the_scope() {
        let mut v = view(&["Locals"]);
        v.tree.expand(0);
        v.show_children(0, vec![item("p", 20), item("q", 0)]);
        v.tree.expand(1);
        v.show_children(1, vec![item("x", 30)]);
        let x = child(&v, 1, "x");
        assert_eq!(v.tree.path_of(x), path(&["Locals", "p", "x"]));
        assert_eq!(v.tree.path_of(0), path(&["Locals"]));
        assert!(v.tree.path_of(99).is_empty());
        let mut t = arr(2500);
        t.set_children(0, from(0, 1000));
        assert_eq!(t.path_of(row(&t)), path(&["r", ""]), "a load-more row's last name is empty");
    }

    #[test]
    fn a_repeat_under_a_scope_is_ambiguous() {
        let mut v = view(&["Locals"]);
        v.tree.expand(0);
        v.show_children(0, vec![item("p", 20), item("p #2", 21), item("q", 22)]);
        let (p, p2, q) = (child(&v, 0, "p"), child(&v, 0, "p #2"), child(&v, 0, "q"));
        let got = [v.tree.ambiguous(p), v.tree.ambiguous(p2), v.tree.ambiguous(q)];
        assert_eq!(got, [true, true, false], "p, p #2, q");
        v.tree.expand(p);
        v.show_children(p, vec![item("x", 30)]);
        let x = child(&v, p, "x");
        assert_eq!(v.tree.stable_path_of(x), None, "under a name that means two things");
        assert_eq!(v.tree.stable_path_of(q), Some(path(&["Locals", "q"])));
    }

    #[test]
    fn a_repeat_in_a_container_is_not() {
        let mut v = view(&["Locals"]);
        v.tree.expand(0);
        v.show_children(0, vec![item("der", 20)]);
        v.tree.expand(1);
        v.show_children(1, vec![item("Base", 30), item("d", 0), item("Base #2", 0)]);
        let (a, b) = (child(&v, 1, "Base"), child(&v, 1, "Base #2"));
        assert!(!v.tree.ambiguous(a) && !v.tree.ambiguous(b));
        assert_eq!(v.tree.stable_path_of(b), Some(path(&["Locals", "der", "Base #2"])));
        v.tree.expand(a);
        v.show_children(a, vec![item("<anonymous union>", 40), item("<anonymous union>", 41)]);
        let u = child(&v, a, "<anonymous union>");
        assert_eq!(v.tree.node(u).unwrap().depth, 3);
        assert!(v.tree.ambiguous(u), "a literal repeat");
        assert_eq!(v.tree.stable_path_of(u), None);
        let twice = view(&["Locals", "Locals"]);
        assert!(twice.tree.ambiguous(0) && twice.tree.ambiguous(1), "two literal roots");
    }

    #[test]
    fn the_price_is_the_measured_one() {
        let table = [
            (0, "", 1),
            (2, "", 1),
            (2, "0x1", 1),
            (1000, "", 21),
            (1000, "s", 21),
            (3000, "", 21),
            (2_000_000, "", 21),
            (1001, "s", 33),
            (5000, "s", 83),
            (10_000, "s", 146),
            (200_000, "s", 2521),
            (400_000, "s", 5021),
            (-5, "", 1),
            (-5, "s", 1),
        ];
        let wrong: Vec<_> = table
            .iter()
            .filter_map(|&(hint, value, want)| {
                let got = reopen_price(&VarItem { children: hint, value: value.into(), ..item("m", 5) });
                (got != want).then_some((hint, value, want, got))
            })
            .collect();
        assert!(wrong.is_empty(), "(hint, value, want, got): {wrong:?}");
    }

    #[test]
    fn record_keeps_the_newest_and_forgets_the_oldest() {
        let mut k = KeptFrame::default();
        k.record(path(&["L", "a"]), true);
        k.record(path(&["L", "b"]), true);
        k.record(path(&["L", "a"]), true);
        assert_eq!(k.open, [path(&["L", "b"]), path(&["L", "a"])], "once, as the newest");
        k.record(path(&["L", "b"]), false);
        assert_eq!(k.open, [path(&["L", "a"])]);
        let mut k = KeptFrame::default();
        for i in 0..=MAX_KEPT_PATHS {
            k.record(path(&["L", &i.to_string()]), true);
        }
        assert_eq!(k.open.len(), MAX_KEPT_PATHS);
        assert_eq!(k.open.first(), Some(&path(&["L", "1"])), "the first one went");
        assert_eq!(k.open.last(), Some(&path(&["L", &MAX_KEPT_PATHS.to_string()])));
    }

    #[test]
    fn a_scope_closed_is_shut_until_opened() {
        let mut k = KeptFrame::default();
        k.record(path(&["Locals"]), false);
        k.record(path(&["Locals"]), false);
        assert_eq!(k.shut, ["Locals"], "once");
        assert!(k.open.is_empty());
        k.record(path(&["Locals"]), true);
        assert!(k.shut.is_empty());
        assert_eq!(k.open, [path(&["Locals"])]);
        k.record(path(&["Locals", "p"]), false);
        assert!(k.shut.is_empty(), "a variable is not a scope");
    }

    #[test]
    fn kept_frames_are_bounded_by_the_last_change() {
        let mut kp = KeptPaths::default();
        for i in 0..=MAX_KEPT_FRAMES {
            kp.frame_mut(&format!("f{i}")).selected = Some(path(&["L"]));
        }
        assert!(kp.frame("f0").is_none() && kp.frame("f1").is_some(), "the first dropped");
        let mut kp = KeptPaths::default();
        for i in 0..MAX_KEPT_FRAMES {
            kp.frame_mut(&format!("f{i}")).selected = Some(path(&["L"]));
        }
        kp.frame_mut("f0");
        kp.frame_mut("new");
        assert!(kp.frame("f0").is_some(), "changed again, so kept");
        assert!(kp.frame("f1").is_none(), "the second went instead");
        kp.frame("f2");
        kp.frame_mut("newer");
        assert!(kp.frame("f2").is_none(), "a read reorders nothing: f2 was the oldest");
        kp.clear();
        assert!(kp.frame("f0").is_none() && kp.frame("newer").is_none());
    }

    #[test]
    fn a_chain_reopens_as_answers_arrive() {
        let k = open(&[&["L"], &["L", "p"], &["L", "p", "x"]]);
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        assert_eq!(asked(&mut v), [(0, 10, 0)], "L");
        stored(&mut v, 0, vec![item("p", 20), item("q", 21)], &k);
        assert_eq!(asked(&mut v), [(1, 20, 0)], "p, with its new reference");
        stored(&mut v, 1, vec![item("x", 30), item("y", 31)], &k);
        assert_eq!(asked(&mut v), [(3, 30, 0)], "x");
        stored(&mut v, 3, vec![item("a", 0)], &k);
        assert!(asked(&mut v).is_empty());
        let (q, y) = (child(&v, 0, "q"), child(&v, 1, "y"));
        assert!(!v.tree.node(q).unwrap().expanded && !v.tree.node(y).unwrap().expanded);
        assert_eq!(v.spent, 2, "p and x; the scope L is a default and costs nothing");
    }

    #[test]
    fn a_vanished_name_does_nothing() {
        let k = open(&[&["L"], &["L", "gone"]]);
        let before = k.clone();
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        stored(&mut v, 0, vec![item("p", 20)], &k);
        assert!(asked(&mut v).is_empty());
        assert_eq!((v.spent, k), (0, before), "nothing spent, nothing forgotten");
    }

    #[test]
    fn a_shadowed_name_stays_closed_and_a_later_stop_opens_it() {
        let k = open(&[&["L"], &["L", "p"]]);
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        stored(&mut v, 0, vec![item("p", 20), item("p #2", 21)], &k);
        assert!(asked(&mut v).is_empty(), "p means the inner one now");
        assert!(v.tree.rows().len() == 3);
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        stored(&mut v, 0, vec![item("p", 22)], &k);
        assert_eq!(asked(&mut v), [(1, 22, 0)]);
    }

    #[test]
    fn a_leaf_and_an_expensive_scope_are_not_opened() {
        let k = open(&[&["L"], &["L", "leaf"]]);
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        stored(&mut v, 0, vec![item("leaf", 0), item("neg", -1)], &k);
        assert!(asked(&mut v).is_empty());
        assert_eq!(v.spent, 0);
        let k = open(&[&["E"]]);
        let mut v = VarsView::default();
        v.tree.set_roots(vec![VarItem { expensive: true, ..item("E", 10) }]);
        v.reopen_roots(&[], Some(&k));
        assert!(asked(&mut v).is_empty(), "an expensive scope is opened by hand each stop");
        assert!(!v.tree.node(0).unwrap().expanded);
    }

    /// `L` with one child `m` of the hint and value, kept open; returns the asks and `spent`.
    fn one_child(hint: i64, value: &str) -> (Vec<(usize, i64, usize)>, usize) {
        let k = open(&[&["L"], &["L", "m"]]);
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        stored(&mut v, 0, vec![VarItem { children: hint, value: value.into(), ..item("m", 20) }], &k);
        (asked(&mut v), v.spent)
    }

    #[test]
    fn the_budget_stops_a_big_printer_and_not_an_array() {
        // (label, hint, value, asks, spent)
        let table = [
            ("a map of 200000", 400_000, "s", 0, 0),
            ("an array of 400000, no value", 400_000, "", 1, 21),
            ("the last that fits", 14_399, "s", 1, 200),
            ("one over", 14_400, "s", 0, 0),
        ];
        let wrong: Vec<_> = table
            .iter()
            .filter_map(|&(label, hint, value, asks, spent)| {
                let (got, got_spent) = one_child(hint, value);
                (got.len() != asks || got_spent != spent).then_some((label, got, got_spent))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn the_budget_runs_out_for_what_is_opened_first() {
        // Oldest first: a small struct, then ten arrays.
        let mut paths: Vec<Vec<String>> = vec![path(&["L"]), path(&["L", "small"])];
        paths.extend((0..10).map(|i| path(&["L", &format!("a{i}")])));
        let k = KeptFrame { open: paths, ..KeptFrame::default() };
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        let mut items = vec![item("small", 19)];
        items.extend((0..10).map(|i| VarItem { children: 3000, ..item(&format!("a{i}"), 20 + i) }));
        stored(&mut v, 0, items, &k);
        let got = asked(&mut v);
        let names: Vec<&str> = got.iter().map(|a| v.tree.node(a.0).unwrap().item.name.as_str()).collect();
        let want = ["a9", "a8", "a7", "a6", "a5", "a4", "a3", "a2", "a1", "small"];
        assert_eq!(names, want, "the nine newest arrays, then the small one still fits");
        assert_eq!(v.spent, 190);
    }

    #[test]
    fn the_default_scopes_cost_nothing() {
        let mut v = view(&["Arguments", "Locals"]);
        v.reopen_roots(&[0, 1], None);
        assert_eq!(asked(&mut v), [(0, 10, 0), (1, 11, 0)]);
        assert_eq!(v.spent, 0);
    }

    #[test]
    fn a_closed_scope_stays_closed() {
        let k = KeptFrame { shut: vec!["Arguments".into()], ..KeptFrame::default() };
        let mut v = view(&["Arguments", "Locals"]);
        v.reopen_roots(&[0, 1], Some(&k));
        assert_eq!(asked(&mut v), [(1, 11, 0)], "only Locals");
    }

    #[test]
    fn a_collapsed_node_opens_nothing_under_it() {
        let k = open(&[&["L"], &["L", "p"], &["L", "p", "x"]]);
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        stored(&mut v, 0, vec![item("p", 20)], &k);
        asked(&mut v);
        let p = child(&v, 0, "p");
        v.tree.collapse(p);
        v.node = p;
        stored(&mut v, p, vec![item("x", 30)], &k);
        assert!(asked(&mut v).is_empty());
        assert_eq!(v.node, p, "the selection does not move into a closed node");
    }

    #[test]
    fn the_selection_follows_to_the_deepest_row_there_is() {
        let mut k = open(&[&["L"], &["L", "p"], &["L", "p", "x"]]);
        k.selected = Some(path(&["L", "p", "x", "a"]));
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        assert_eq!(v.node, 0);
        stored(&mut v, 0, vec![item("p", 20), item("q", 21)], &k);
        let p = child(&v, 0, "p");
        assert_eq!(v.node, p);
        stored(&mut v, p, vec![item("x", 30)], &k);
        let x = child(&v, p, "x");
        assert_eq!(v.node, x);
        stored(&mut v, x, vec![item("a", 0), item("b", 0)], &k);
        assert_eq!(v.node, child(&v, x, "a"));
        // x has no a.
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        stored(&mut v, 0, vec![item("p", 20)], &k);
        stored(&mut v, 1, vec![item("x", 30)], &k);
        stored(&mut v, 2, vec![item("b", 0)], &k);
        assert_eq!(v.node, 2, "stays on x");
        // p is gone.
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        stored(&mut v, 0, vec![item("q", 21)], &k);
        assert_eq!(v.node, 0, "stays where it was");
    }

    #[test]
    fn a_moved_selection_is_not_moved_back() {
        let mut k = open(&[&["L"], &["L", "p"], &["L", "p", "x"]]);
        k.selected = Some(path(&["L", "q"]));
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        stored(&mut v, 0, vec![item("p", 20), item("q", 21)], &k);
        let q = child(&v, 0, "q");
        assert_eq!(v.node, q);
        stored(&mut v, 1, vec![item("x", 30)], &k);
        assert_eq!(v.node, q, "the reopen under p leaves it on q");
    }

    /// `L` holding `arr` (hint 2500) reopened, its first page answered with no `1200`.
    fn paged_reopen(k: &KeptFrame) -> VarsView {
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(k));
        asked(&mut v);
        stored(&mut v, 0, vec![VarItem { children: 2500, ..item("arr", 20) }], k);
        v
    }

    #[test]
    fn a_later_page_opens_what_it_brings() {
        let k = open(&[&["L"], &["L", "arr"], &["L", "arr", "1200"]]);
        let mut v = paged_reopen(&k);
        assert_eq!(asked(&mut v), [(1, 20, 0)]);
        stored(&mut v, 1, from(0, 1000), &k);
        assert!(asked(&mut v).is_empty(), "no 1200 on the first page");
        let r = v.tree.node(1).unwrap().more.expect("a load-more row");
        assert!(v.tree.load_more(r));
        assert_eq!(asked(&mut v), [(1, 20, 1000)]);
        let mut page = from(1000, 1000);
        page[200] = item("1200", 40);
        stored(&mut v, 1, page, &k);
        assert_eq!(asked(&mut v), [(child(&v, 1, "1200"), 40, 0)], "1200 opens, from its start");
        let seven = child(&v, 1, "7");
        assert!(!v.tree.node(seven).unwrap().expanded, "a row of page one the user never opened stays closed");
    }

    #[test]
    fn a_stop_asks_no_later_page() {
        let k = open(&[&["L"], &["L", "arr"], &["L", "arr", "1200"]]);
        let mut v = paged_reopen(&k);
        stored(&mut v, 1, from(0, 1000), &k);
        assert_eq!(v.tree.node(1).unwrap().children.as_ref().map(Vec::len), Some(1000));
        let all = v.tree.take_asks();
        assert!(all.iter().all(|a| a.start == 0), "{all:?}");
    }

    /// `L` holding `arr` (hint 2500) and its first page of 1000, whose row `5` has a reference (40) and was asked
    /// for; `5` is the child returned.
    fn page_one_with_a_child(k: &KeptFrame) -> (VarsView, usize) {
        let mut v = paged_reopen(k);
        asked(&mut v);
        let mut page = from(0, 1000);
        page[5] = item("5", 40);
        stored(&mut v, 1, page, k);
        let five = child(&v, 1, "5");
        (v, five)
    }

    /// Presses the load-more row of node 1 and stores `page` as its answer.
    fn later_page(v: &mut VarsView, page: Vec<VarItem>, k: &KeptFrame) {
        let r = v.tree.node(1).unwrap().more.expect("a load-more row");
        assert!(v.tree.load_more(r));
        let start = v.tree.node(1).unwrap().children.as_ref().map_or(0, Vec::len);
        assert_eq!(asked(v), [(1, 20, start)]);
        stored(v, 1, page, k);
    }

    #[test]
    fn a_failed_child_of_page_one_stays_as_it_is_when_a_later_page_lands() {
        let k = open(&[&["L"], &["L", "arr"], &["L", "arr", "5"]]);
        let (mut v, five) = page_one_with_a_child(&k);
        assert_eq!(asked(&mut v), [(five, 40, 0)], "5 reopens on page one");
        v.tree.set_failed(five, "could not read".into());
        let spent = v.spent;
        later_page(&mut v, from(1000, 1000), &k);
        assert!(asked(&mut v).is_empty(), "no second ask for 5");
        let n = v.tree.node(five).unwrap();
        assert_eq!(n.failed.as_deref(), Some("could not read"), "the note stays");
        assert!(!n.expanded && n.children.is_none());
        assert_eq!(v.spent, spent, "and costs nothing");
    }

    #[test]
    fn a_selection_on_a_nested_load_more_row_stays_when_the_outer_page_lands() {
        let mut k = open(&[&["L"], &["L", "arr"]]);
        k.selected = Some(path(&["L", "arr", "c"]));
        let mut v = paged_reopen(&k);
        asked(&mut v);
        let mut page = from(0, 999);
        page.push(VarItem { children: 2500, ..item("c", 41) });
        stored(&mut v, 1, page, &k);
        let c = child(&v, 1, "c");
        assert_eq!(v.node, c, "the saved selection finds c on page one");
        v.tree.expand(c);
        asked(&mut v);
        stored(&mut v, c, from(0, 1000), &k);
        let row = v.tree.node(c).unwrap().more.expect("c has a load-more row");
        v.node = row;
        later_page(&mut v, from(1000, 1000), &k);
        assert_eq!(v.node, row, "the page of arr leaves the selection on the row of c");
    }

    #[test]
    fn a_load_more_row_has_no_stable_path() {
        let k = open(&[&["L"], &["L", "arr"]]);
        let mut v = paged_reopen(&k);
        asked(&mut v);
        stored(&mut v, 1, from(0, 1000), &k);
        let r = v.tree.node(1).unwrap().more.expect("a load-more row");
        assert_eq!(v.tree.stable_path_of(1), Some(path(&["L", "arr"])));
        assert_eq!(v.tree.stable_path_of(r), None);
    }

    #[test]
    fn nothing_opens_under_a_shadowed_node() {
        // `p` and `p #2` under a scope: whichever the user opens, a saved child does not follow.
        let k = open(&[&["L"], &["L", "p"], &["L", "p", "x"]]);
        let mut v = view(&["L"]);
        v.reopen_roots(&[0], Some(&k));
        asked(&mut v);
        stored(&mut v, 0, vec![item("p", 20), item("p #2", 21)], &k);
        assert!(asked(&mut v).is_empty(), "p is ambiguous");
        let p = child(&v, 0, "p");
        v.tree.expand(p);
        asked(&mut v);
        stored(&mut v, p, vec![item("x", 30)], &k);
        assert!(asked(&mut v).is_empty(), "x stays closed under a shadow");
        let x = child(&v, p, "x");
        assert!(!v.tree.node(x).unwrap().expanded);
    }

    #[test]
    fn set_roots_keeps_the_watches() {
        let mut t = VarTree::default();
        let w = t.add_watch("w".into());
        assert!(t.set_watch(w, Ok(VarItem { children: 2, ..item("ignored", 9) })));
        t.expand(w);
        t.take_asks();
        t.set_children(w, vec![item("c", 5), item("d", 0)]);
        t.expand(1);
        let first = t.set_roots(vec![item("A", 10), item("B", 11)]);
        assert_eq!(first, 3, "after the watch and its two children");
        assert_eq!(names(&t), ["w", "c", "d", "A", "B"], "the watch rows come first");
        assert_eq!(t.take_asks(), vec![VarAsk { node: 1, reference: 5, children: 0, start: 0 }], "the pending ask stays");
        assert_eq!(t.take_watch_asks(), vec![w]);
        assert!(t.is_watch(w) && !t.is_watch(first));
    }

    #[test]
    fn set_roots_without_watches_is_unchanged() {
        let mut t = VarTree::default();
        assert_eq!(t.set_roots(vec![item("a", 10), item("b", 11)]), 0);
        t.expand(0);
        assert_eq!(t.set_roots(vec![item("c", 12)]), 0, "a second call clears");
        assert_eq!(names(&t), ["c"]);
        assert!(t.take_asks().is_empty());
    }

    #[test]
    fn a_watch_answer_keeps_its_expression() {
        let mut t = VarTree::default();
        let w = t.add_watch("a + b".into());
        assert!(t.node(w).unwrap().loading && t.node(w).unwrap().watch);
        let answer = VarItem { value: "3".into(), ty: "int".into(), ..item("other", 0) };
        assert!(t.set_watch(w, Ok(answer)));
        let n = t.node(w).unwrap();
        assert_eq!((n.item.name.as_str(), n.item.value.as_str(), n.item.ty.as_str()), ("a + b", "3", "int"));
        assert!(!n.loading && n.failed.is_none());
        assert!(!t.set_watch(w, Ok(item("again", 0))), "a second answer changes nothing");
        assert_eq!(t.node(w).unwrap().item.value, "3");
        let x = t.add_watch("nosuch".into());
        assert!(t.set_watch(x, Err("No symbol".into())));
        let n = t.node(x).unwrap();
        assert_eq!((n.failed.as_deref(), n.item.reference, n.loading), (Some("No symbol"), 0, false));
    }

    #[test]
    fn watches_are_ordered_and_asked_oldest_first() {
        let mut t = VarTree::default();
        let (a, b) = (t.add_watch("a".into()), t.add_watch("b".into()));
        t.set_roots(vec![item("L", 10)]);
        let c = t.add_watch("c".into());
        assert_eq!(names(&t), ["a", "b", "c", "L"], "a new watch goes after the last watch, before the scopes");
        assert_eq!(t.take_watch_asks(), vec![a, b, c]);
        assert!(t.take_watch_asks().is_empty());
    }

    #[test]
    fn a_removed_watch_ignores_its_answer() {
        let mut t = VarTree::default();
        let w = t.add_watch("w".into());
        assert!(t.remove_watch(w));
        assert!(!t.is_watch(w) && !t.remove_watch(w));
        assert!(!t.set_watch(w, Ok(VarItem { value: "1".into(), ..item("w", 0) })));
        assert!(t.node(w).unwrap().item.value.is_empty());
        assert!(t.rows().is_empty());
        assert!(t.take_watch_asks().is_empty(), "its ask is gone too");
    }

    #[test]
    fn a_load_more_row_under_a_watch_is_no_watch() {
        let mut t = VarTree::default();
        let w = t.add_watch("w".into());
        assert!(t.set_watch(w, Ok(VarItem { children: 2500, ..item("w", 9) })));
        t.expand(w);
        t.set_children(w, from(0, 1000));
        let r = t.node(w).unwrap().more.expect("a load-more row");
        let c = t.node(w).unwrap().children.as_ref().unwrap()[0];
        assert!(!t.is_watch(r) && !t.remove_watch(r));
        assert!(!t.is_watch(c) && !t.remove_watch(c));
        assert_eq!(t.stable_path_of(r), None);
        assert_eq!(t.stable_path_of(c), Some(path(&["w", "0"])));
        assert!(t.is_watch(w), "the watch is still there");
    }

    #[test]
    fn nothing_opens_under_a_watch_named_like_a_scope() {
        let k = open(&[&["Locals"], &["Locals", "x"]]);
        let mut v = VarsView::default();
        let w = v.tree.add_watch("Locals".into());
        let first = v.tree.set_roots(vec![item("Locals", 10)]);
        v.reopen_roots(&[], Some(&k));
        assert!(v.tree.set_watch(w, Ok(item("Locals", 9))));
        v.reopen_watch(w, &k);
        assert!(asked(&mut v).is_empty(), "both are ambiguous");
        assert!(!v.tree.node(w).unwrap().expanded && !v.tree.node(first).unwrap().expanded);
        // The user opens the scope: nothing below it follows, whoever opened it.
        v.tree.expand(first);
        asked(&mut v);
        stored(&mut v, first, vec![item("x", 30)], &k);
        assert!(asked(&mut v).is_empty());
        assert_eq!(v.spent, 0);
    }

    #[test]
    fn the_scopes_leave_the_watch_roots_alone() {
        let mut k = open(&[&["w"], &["L"]]);
        k.selected = Some(path(&["w"]));
        let mut v = VarsView::default();
        let w = v.tree.add_watch("w".into());
        assert!(v.tree.set_watch(w, Ok(item("w", 9))));
        v.node = w + 5;
        let first = v.tree.set_roots(vec![item("L", 10)]);
        v.reopen_roots(&[], Some(&k));
        assert!(!v.tree.node(w).unwrap().expanded, "a watch opens when its own answer lands");
        assert_eq!(v.spent, 1, "only the scope L was charged");
        assert_ne!(v.node, w, "and the selection stays");
        assert!(v.tree.node(first).unwrap().expanded);
    }

    #[test]
    fn a_watch_reopens_by_its_name() {
        let mut k = open(&[&["w"], &["w", "x"]]);
        k.selected = Some(path(&["w", "x"]));
        let mut v = VarsView::default();
        let w = v.tree.add_watch("w".into());
        assert!(v.tree.set_watch(w, Ok(VarItem { children: 2, ..item("w", 9) })));
        v.reopen_watch(w, &k);
        assert_eq!(asked(&mut v), [(w, 9, 0)]);
        assert_eq!(v.spent, reopen_price(&VarItem { children: 2, ..item("w", 9) }));
        assert!(v.spent > 0);
        assert_eq!(v.node, w);
        stored(&mut v, w, vec![item("x", 30), item("y", 0)], &k);
        let x = child(&v, w, "x");
        assert_eq!(asked(&mut v), [(x, 30, 0)]);
        assert_eq!(v.node, x, "the selection follows");
    }
}
