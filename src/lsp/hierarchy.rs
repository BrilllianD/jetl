//! The call and type hierarchy of a symbol: the items a server answers
//! (`prepareCallHierarchy`, `incomingCalls`, `outgoingCalls`,
//! `prepareTypeHierarchy`, `supertypes`, `subtypes`), and the tree of them
//! the Hierarchy window shows, loaded one level at a time.
//!
//! Items are read through [`Wire`], not `lsp_types::CallHierarchyItem` or
//! `TypeHierarchyItem` (the latter declares `tags` as one tag, so a real
//! answer would not parse). The server's own item is kept in
//! [`HierarchyItem::raw`] and sent back verbatim when its children are asked.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_types::{Range, SymbolKind};
use serde::Deserialize;
use serde_json::Value;

use super::caps::{INCOMING_CALLS, OUTGOING_CALLS, SUBTYPES, SUPERTYPES};
use super::sync;

/// Which way a hierarchy is walked from its roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Callers,
    Callees,
    Supertypes,
    Subtypes,
}

impl Direction {
    /// The other way of the same kind: callers and callees, supertypes and subtypes.
    pub fn flipped(self) -> Self {
        match self {
            Direction::Callers => Direction::Callees,
            Direction::Callees => Direction::Callers,
            Direction::Supertypes => Direction::Subtypes,
            Direction::Subtypes => Direction::Supertypes,
        }
    }

    /// Whether this is a call hierarchy (else a type hierarchy).
    pub fn is_call(self) -> bool {
        matches!(self, Direction::Callers | Direction::Callees)
    }

    /// The request that asks an item's children in this direction.
    pub fn method(self) -> &'static str {
        match self {
            Direction::Callers => INCOMING_CALLS,
            Direction::Callees => OUTGOING_CALLS,
            Direction::Supertypes => SUPERTYPES,
            Direction::Subtypes => SUBTYPES,
        }
    }
}

/// One function or type of a hierarchy.
#[derive(Debug, Clone, PartialEq)]
pub struct HierarchyItem {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub path: PathBuf,
    /// The item's `selectionRange` (UTF-16): where to put the caret.
    pub selection: Range,
    /// The server's item as received, sent back in child requests.
    pub raw: Value,
}

/// The fields of an item jetl reads; the same for call and type items.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    name: String,
    kind: SymbolKind,
    #[serde(default)]
    detail: Option<String>,
    uri: lsp_types::Uri,
    selection_range: Range,
}

/// An item from the server's JSON; `None` when it is malformed or not a file.
fn item(raw: Value) -> Option<HierarchyItem> {
    let w = match serde_json::from_value::<Wire>(raw.clone()) {
        Ok(w) => w,
        Err(e) => {
            tracing::debug!("skipping a malformed hierarchy item: {e}");
            return None;
        }
    };
    let path = sync::path(&w.uri)?;
    Some(HierarchyItem { name: w.name, detail: w.detail, kind: w.kind, path, selection: w.selection_range, raw })
}

/// The items of a prepare answer: null, `[]` or items. Non-file URIs and
/// malformed items are left out.
pub fn parse_items(value: Value) -> Vec<HierarchyItem> {
    match value {
        Value::Array(a) => a.into_iter().filter_map(item).collect(),
        _ => Vec::new(),
    }
}

/// The items of an `incomingCalls` (`from`) or `outgoingCalls` (`to`) answer.
pub fn parse_calls(value: Value, dir: Direction) -> Vec<HierarchyItem> {
    let key = if dir == Direction::Callees { "to" } else { "from" };
    match value {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|mut call| match call.get_mut(key).map(Value::take) {
                Some(raw) => item(raw),
                None => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The items of a `supertypes` or `subtypes` answer.
pub fn parse_types(value: Value) -> Vec<HierarchyItem> {
    parse_items(value)
}

/// A row of the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub item: HierarchyItem,
    pub depth: usize,
    pub parent: Option<usize>,
    /// `None`: not asked yet; `Some(vec![])`: a leaf.
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    pub loading: bool,
}

/// The tree of one prepare answer, in one direction. `id` tells the
/// answers meant for it from those meant for an older tree.
#[derive(Debug, Clone, PartialEq)]
pub struct HierarchyTree {
    pub id: u64,
    pub server: String,
    pub direction: Direction,
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
}

impl HierarchyTree {
    /// A tree of `roots` (depth 0), none of them asked for children yet.
    pub fn new(id: u64, server: String, direction: Direction, roots: Vec<HierarchyItem>) -> Self {
        let nodes: Vec<Node> =
            roots.into_iter().map(|item| Node { item, depth: 0, parent: None, children: None, expanded: false, loading: false }).collect();
        let roots = (0..nodes.len()).collect();
        Self { id, server, direction, nodes, roots }
    }

    /// The same roots in `direction` under a new `id`; every child is asked again.
    pub fn restarted(&self, id: u64, direction: Direction) -> Self {
        let roots = self.roots.iter().map(|&r| self.nodes[r].item.clone()).collect();
        Self::new(id, self.server.clone(), direction, roots)
    }

    /// The visible nodes in display order: the roots, each followed by its
    /// children when expanded, depth first.
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

    /// The window title: the direction and the first root's name.
    pub fn title(&self) -> String {
        let what = match self.direction {
            Direction::Callers => "Callers",
            Direction::Callees => "Callees",
            Direction::Supertypes => "Supertypes",
            Direction::Subtypes => "Subtypes",
        };
        let root = self.roots.first().map(|&r| self.nodes[r].item.name.as_str()).unwrap_or_default();
        format!("{what} of {root}")
    }

    /// Give `node` its children (replacing an earlier list) and expand it.
    /// An old child stays in `nodes`, out of reach of `rows`.
    pub fn set_children(&mut self, node: usize, items: Vec<HierarchyItem>) {
        let Some(depth) = self.nodes.get(node).map(|n| n.depth + 1) else { return };
        let first = self.nodes.len();
        self.nodes.extend(items.into_iter().map(|item| Node {
            item,
            depth,
            parent: Some(node),
            children: None,
            expanded: false,
            loading: false,
        }));
        let children = (first..self.nodes.len()).collect();
        let n = &mut self.nodes[node];
        n.children = Some(children);
        n.expanded = true;
        n.loading = false;
    }

    pub fn collapse(&mut self, node: usize) {
        if let Some(n) = self.nodes.get_mut(node) {
            n.expanded = false;
        }
    }

    /// The loaded part of the tree as graph nodes `(name, file label, children never asked)`
    /// in id order, and the deduplicated edges `(from, to)` in walk order. One graph node
    /// per distinct item; a node is walked once, which bounds cycles.
    fn graph(&self) -> Graph {
        type Key<'a> = (&'a str, &'a Path, u32, u32);
        let mut ids: HashMap<Key, usize> = HashMap::new();
        let mut labels: Vec<(String, String)> = Vec::new();
        let mut loaded: HashSet<usize> = HashSet::new();
        let mut walked: HashSet<usize> = HashSet::new();
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let mut edges = Vec::new();
        let mut id_of = |n: usize| -> usize {
            let it = &self.nodes[n].item;
            let key = (it.name.as_str(), it.path.as_path(), it.selection.start.line, it.selection.start.character);
            *ids.entry(key).or_insert_with(|| {
                let file = it.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                labels.push((it.name.clone(), format!("{file}:{}", it.selection.start.line + 1)));
                labels.len() - 1
            })
        };
        let to_parent = matches!(self.direction, Direction::Callers | Direction::Subtypes);
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            let id = id_of(n);
            let Some(children) = self.nodes[n].children.as_ref() else { continue };
            loaded.insert(id);
            if !walked.insert(id) {
                continue;
            }
            for &c in children {
                let cid = id_of(c);
                let edge = if to_parent { (cid, id) } else { (id, cid) };
                if seen.insert(edge) {
                    edges.push(edge);
                }
            }
            stack.extend(children.iter().rev());
        }
        let nodes = labels.into_iter().enumerate().map(|(i, (name, file))| (name, file, !loaded.contains(&i))).collect();
        (nodes, edges)
    }

    /// The loaded part of the tree as a Mermaid flowchart; edges point from caller to
    /// callee and from subtype to supertype.
    pub fn to_mermaid(&self) -> String {
        let (nodes, edges) = self.graph();
        let mut out = String::from("flowchart LR\n");
        for (i, (name, file, unloaded)) in nodes.iter().enumerate() {
            let tail = if *unloaded { " …" } else { "" };
            out.push_str(&format!("    n{i}[\"{}<br/>{}{tail}\"]\n", mermaid_escape(name), mermaid_escape(file)));
        }
        for (a, b) in edges {
            out.push_str(&format!("    n{a} --> n{b}\n"));
        }
        out
    }

    /// The loaded part of the tree as a Graphviz DOT digraph.
    pub fn to_dot(&self) -> String {
        let (nodes, edges) = self.graph();
        let mut out = format!("digraph \"{}\" {{\n    rankdir=LR;\n    node [shape=box];\n", dot_escape(&self.title()));
        for (i, (name, file, unloaded)) in nodes.iter().enumerate() {
            let tail = if *unloaded { " …" } else { "" };
            out.push_str(&format!("    n{i} [label=\"{}\\n{}{tail}\"];\n", dot_escape(name), dot_escape(file)));
        }
        for (a, b) in edges {
            out.push_str(&format!("    n{a} -> n{b};\n"));
        }
        out.push_str("}\n");
        out
    }
}

/// Graph nodes `(name, file label, children never asked)` and edges `(from, to)`.
type Graph = (Vec<(String, String, bool)>, Vec<(usize, usize)>);

fn mermaid_escape(s: &str) -> String {
    s.replace('#', "#35;").replace('"', "#quot;").replace('<', "#lt;").replace('>', "#gt;")
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use lsp_types::{Position, Range, SymbolKind};
    use serde_json::{Value, json};

    use super::*;

    fn raw(name: &str, uri: &str) -> Value {
        json!({
            "name": name, "kind": 12, "detail": "fn()", "uri": uri,
            "range": {"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 1}},
            "selectionRange": {"start": {"line": 1, "character": 3}, "end": {"line": 1, "character": 7}},
            "data": {"id": 7}, "tags": [1],
        })
    }

    fn item(name: &str) -> HierarchyItem {
        parse_items(json!([raw(name, "file:///w/a.rs")])).remove(0)
    }

    #[test]
    fn prepare_answers_parse_in_order_and_keep_the_raw_item() {
        assert!(parse_items(Value::Null).is_empty());
        assert!(parse_items(json!([])).is_empty());
        let a = raw("main", "file:///w/a.rs");
        let b = raw("helper", "file:///w/b.rs");
        let items = parse_items(json!([a, b]));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "main");
        assert_eq!(items[0].detail.as_deref(), Some("fn()"));
        assert_eq!(items[0].kind, SymbolKind::FUNCTION);
        assert_eq!(items[0].path, PathBuf::from("/w/a.rs"));
        assert_eq!(items[0].selection, Range::new(Position::new(1, 3), Position::new(1, 7)));
        assert_eq!(items[0].raw, a);
        assert_eq!(items[0].raw["data"], json!({"id": 7}));
        assert_eq!(items[1].name, "helper");
        assert_eq!(items[1].raw, b);
    }

    #[test]
    fn non_file_and_malformed_items_are_left_out() {
        let mut bad = raw("x", "file:///w/a.rs");
        bad.as_object_mut().unwrap().remove("name");
        let items = parse_items(json!([raw("u", "untitled:x"), bad, raw("ok", "file:///w/a.rs")]));
        assert_eq!(items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["ok"]);
    }

    #[test]
    fn calls_read_from_or_to() {
        let incoming = json!([{"from": raw("caller", "file:///w/a.rs"), "fromRanges": []}]);
        assert_eq!(parse_calls(incoming, Direction::Callers)[0].name, "caller");
        let outgoing = json!([{"to": raw("callee", "file:///w/a.rs"), "fromRanges": []}]);
        assert_eq!(parse_calls(outgoing, Direction::Callees)[0].name, "callee");
        assert!(parse_calls(Value::Null, Direction::Callers).is_empty());
    }

    #[test]
    fn types_parse_like_prepare_answers() {
        let items = parse_types(json!([raw("A", "file:///w/a.rs"), raw("B", "file:///w/a.rs")]));
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].name, "B");
    }

    #[test]
    fn direction_flips_and_names_its_request() {
        use Direction::*;
        assert_eq!((Callers.flipped(), Callees.flipped()), (Callees, Callers));
        assert_eq!((Supertypes.flipped(), Subtypes.flipped()), (Subtypes, Supertypes));
        assert!(Callers.is_call() && Callees.is_call() && !Supertypes.is_call() && !Subtypes.is_call());
        assert_eq!(Callers.method(), crate::lsp::caps::INCOMING_CALLS);
        assert_eq!(Callees.method(), crate::lsp::caps::OUTGOING_CALLS);
        assert_eq!(Supertypes.method(), crate::lsp::caps::SUPERTYPES);
        assert_eq!(Subtypes.method(), crate::lsp::caps::SUBTYPES);
    }

    fn tree() -> HierarchyTree {
        HierarchyTree::new(1, "ra".into(), Direction::Callers, vec![item("main"), item("other")])
    }

    #[test]
    fn rows_follow_expanded_nodes_depth_first() {
        let mut t = tree();
        assert_eq!(t.rows(), [0, 1]);
        t.set_children(0, vec![item("a"), item("b")]);
        assert_eq!(t.rows(), [0, 2, 3, 1]);
        assert!(t.nodes[2].depth == 1 && t.nodes[3].depth == 1);
        assert_eq!((t.nodes[2].parent, t.nodes[3].parent), (Some(0), Some(0)));
        assert!(t.nodes[0].expanded && !t.nodes[0].loading);
        t.set_children(2, vec![]);
        assert_eq!(t.nodes[2].children, Some(vec![]));
        assert!(t.nodes[2].expanded);
        t.collapse(0);
        assert_eq!(t.rows(), [0, 1]);
    }

    #[test]
    fn export_walks_loaded_nodes_and_marks_unasked_ones() {
        let mut t = tree();
        t.set_children(0, vec![item("a"), item("b")]);
        t.set_children(2, vec![]);
        t.collapse(0);
        assert_eq!(
            t.to_mermaid(),
            "flowchart LR\n    n0[\"main<br/>a.rs:2\"]\n    n1[\"a<br/>a.rs:2\"]\n    n2[\"b<br/>a.rs:2 …\"]\n    n3[\"other<br/>a.rs:2 …\"]\n    n1 --> n0\n    n2 --> n0\n"
        );
        assert_eq!(
            t.to_dot(),
            "digraph \"Callers of main\" {\n    rankdir=LR;\n    node [shape=box];\n    n0 [label=\"main\\na.rs:2\"];\n    n1 [label=\"a\\na.rs:2\"];\n    n2 [label=\"b\\na.rs:2 …\"];\n    n3 [label=\"other\\na.rs:2 …\"];\n    n1 -> n0;\n    n2 -> n0;\n}\n"
        );
    }

    #[test]
    fn export_edges_point_from_caller_and_from_subtype() {
        let mut t = tree();
        t.set_children(0, vec![item("a"), item("b")]);
        for d in [Direction::Callees, Direction::Supertypes] {
            t.direction = d;
            assert!(t.to_mermaid().ends_with("    n0 --> n1\n    n0 --> n2\n"));
        }
        t.direction = Direction::Subtypes;
        assert!(t.to_mermaid().ends_with("    n1 --> n0\n    n2 --> n0\n"));
    }

    #[test]
    fn export_merges_repeated_items_and_ends_on_cycles() {
        let mut t = tree();
        t.set_children(0, vec![item("main")]);
        t.set_children(2, vec![item("main")]);
        let m = t.to_mermaid();
        assert_eq!(m.matches("<br/>").count(), 2);
        assert!(m.contains("n0[\"main<br/>a.rs:2\"]"));
        assert!(m.contains("n1[\"other<br/>a.rs:2 …\"]"));
        assert_eq!(m.matches("-->").count(), 1);
        assert!(m.ends_with("    n0 --> n0\n"));
    }

    #[test]
    fn export_skips_replaced_children() {
        let mut t = tree();
        t.set_children(0, vec![item("x")]);
        t.set_children(0, vec![item("y")]);
        let m = t.to_mermaid();
        assert!(m.contains("y<br/>") && !m.contains("x<br/>"));
    }

    #[test]
    fn export_escapes_labels() {
        let mut it = item("z");
        it.name = "Vec<\"a#b\">".into();
        let t = HierarchyTree::new(1, "s".into(), Direction::Callers, vec![it.clone()]);
        assert!(t.to_mermaid().contains("Vec#lt;#quot;a#35;b#quot;#gt;"));
        assert!(t.to_dot().contains("Vec<\\\"a#b\\\">"));
        it.name = "a\\b".into();
        let t = HierarchyTree::new(1, "s".into(), Direction::Callers, vec![it]);
        assert!(t.to_dot().contains("a\\\\b"));
    }

    #[test]
    fn export_of_an_empty_tree_is_just_the_header() {
        let t = HierarchyTree::new(1, "s".into(), Direction::Callers, vec![]);
        assert_eq!(t.to_mermaid(), "flowchart LR\n");
    }

    #[test]
    fn titles_name_the_first_root() {
        let mut t = tree();
        for (d, want) in [
            (Direction::Callers, "Callers of main"),
            (Direction::Callees, "Callees of main"),
            (Direction::Supertypes, "Supertypes of main"),
            (Direction::Subtypes, "Subtypes of main"),
        ] {
            t.direction = d;
            assert_eq!(t.title(), want);
        }
    }

    #[test]
    fn restarted_keeps_only_the_roots() {
        let mut t = tree();
        t.set_children(0, vec![item("a")]);
        let r = t.restarted(9, Direction::Subtypes);
        assert_eq!((r.id, r.direction, r.nodes.len(), r.roots.clone()), (9, Direction::Subtypes, 2, vec![0, 1]));
        assert!(r.nodes.iter().all(|n| n.children.is_none() && !n.expanded && !n.loading));
        assert_eq!(r.server, "ra");
    }

    #[test]
    fn out_of_range_nodes_are_ignored() {
        let mut t = tree();
        let before = t.clone();
        t.set_children(99, vec![item("a")]);
        t.collapse(99);
        assert_eq!(t, before);
    }
}
