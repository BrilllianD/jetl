//! Call and type hierarchy in the app: the prepare request at the caret,
//! the tree in `App::hierarchy` and its lazily asked children
//! (`lsp/hierarchy.rs` holds the model).
//!
//! Without a server for the buffer, the call hierarchy is computed from the
//! symbol index (`symbols/graph.rs`): an index tree
//! (`HierarchyTree::from_index`) whose children are filled synchronously.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{Position, Range};
use serde_json::{Value, json};

use super::App;
use super::lsp_nav::word_at;
use crate::actions::HierarchyAction;
use crate::editor::buffer::BufferId;
use crate::lsp::PendingTag;
use crate::lsp::caps::{PREPARE_CALL_HIERARCHY, PREPARE_TYPE_HIERARCHY};
use crate::lsp::hierarchy::HierarchyItem;
use crate::lsp::hierarchy::{Direction, HierarchyTree, parse_calls, parse_items, parse_types};
use crate::lsp::transport::ResponseError;
use crate::symbols::graph;
use crate::ui::focus::{PanelId, ToolWindowId};

impl App {
    /// Ask the server for the call (`call`) or type hierarchy of the symbol at the caret;
    /// without a server, the call hierarchy comes from the symbol index.
    pub(crate) fn hierarchy_start(&mut self, call: bool) {
        let Some((id, pos)) = self.caret_position() else { return };
        let (method, tag): (_, fn(_) -> _) =
            if call { (PREPARE_CALL_HIERARCHY, PendingTag::CallHierarchy) } else { (PREPARE_TYPE_HIERARCHY, PendingTag::TypeHierarchy) };
        if let Err(why) = self.lsp_doc_request(id, pos, method, json!({}), tag) {
            if call && self.hierarchy_without_server(id) {
                return;
            }
            if !why.is_empty() {
                self.lsp_hint(why);
            }
        }
    }

    /// The call hierarchy of the word at the caret of buffer `id` from the
    /// symbol index, when no server document serves it (the Find Usages rule)
    /// and its language has a tags query. True when handled.
    fn hierarchy_without_server(&mut self, id: BufferId) -> bool {
        if self.lsp.docs.contains_key(&id) {
            return false;
        }
        let Some(b) = self.editor.buffers.iter().find(|b| b.id == id) else { return false };
        let (Some(path), Some(def)) = (b.path.clone(), b.language()) else { return false };
        let head = b.view.sel().head;
        let word = word_at(b, head);
        if def.tags.is_none() || word.is_empty() {
            return false;
        }
        let Some(rel) = self.rel_path(&path) else { return false };
        let line = b.rope.char_to_line(head);
        let roots = graph::roots(&mut self.symbols, &word, &rel, line);
        if roots.is_empty() {
            self.lsp_hint(if self.symbols.indexing() {
                "No call hierarchy found yet; the project is still being indexed"
            } else {
                "No call hierarchy here"
            });
            return true;
        }
        let items = self.hierarchy_items(roots);
        let id = self.hierarchy_next_id();
        self.hierarchy_show(HierarchyTree::new(id, String::new(), Direction::Callers, items));
        true
    }

    fn rel_path(&self, p: &Path) -> Option<String> {
        p.strip_prefix(&self.workspace.root).ok().map(|r| r.to_string_lossy().replace('\\', "/"))
    }

    /// The answer to a prepare request: with items they become the tree's
    /// roots and the first one's children are asked; without, the tree stays.
    pub(super) fn hierarchy_prepared(&mut self, name: &str, call: bool, value: Value) {
        let items = parse_items(value);
        if items.is_empty() {
            return self.lsp_hint(if call { "No call hierarchy here" } else { "No type hierarchy here" });
        }
        let direction = if call { Direction::Callers } else { Direction::Subtypes };
        let id = self.hierarchy_next_id();
        self.hierarchy_show(HierarchyTree::new(id, name.to_string(), direction, items));
    }

    /// Make `tree` the Hierarchy window's, ask its first root's children, show and focus the window.
    pub(super) fn hierarchy_show(&mut self, tree: HierarchyTree) {
        self.hierarchy = Some(tree);
        self.hierarchy_ask(0);
        self.layout.show(ToolWindowId::Hierarchy);
        self.focus.set(PanelId::Hierarchy);
        self.dirty_frame = true;
    }

    /// A key of the Hierarchy window; moves are the panel's, the rest needs the server.
    pub(crate) fn perform_hierarchy(&mut self, a: HierarchyAction) {
        self.dirty_frame = true;
        if self.hierarchy_panel.step(&a, self.hierarchy.as_ref()) {
            return;
        }
        if matches!(a, HierarchyAction::ExportMermaid | HierarchyAction::ExportDot) {
            return self.hierarchy_export(a == HierarchyAction::ExportDot);
        }
        let Some(node) = self.hierarchy_panel.selected(self.hierarchy.as_ref()) else { return };
        match a {
            HierarchyAction::Expand => {
                let Some(n) = self.hierarchy.as_ref().and_then(|t| t.nodes.get(node)) else { return };
                match n.children.as_ref().filter(|c| n.expanded && !c.is_empty()) {
                    Some(children) => {
                        let first = children[0];
                        if let Some(t) = &self.hierarchy {
                            self.hierarchy_panel.select_node(t, first);
                        }
                    }
                    None => self.hierarchy_expand(node),
                }
            }
            HierarchyAction::Collapse => {
                let Some(n) = self.hierarchy.as_ref().and_then(|t| t.nodes.get(node)) else { return };
                if n.expanded && n.children.as_ref().is_some_and(|c| !c.is_empty()) {
                    self.hierarchy_collapse(node);
                } else if let (Some(parent), Some(t)) = (n.parent, &self.hierarchy) {
                    self.hierarchy_panel.select_node(t, parent);
                }
            }
            HierarchyAction::Open => self.hierarchy_open(node),
            HierarchyAction::ToggleDirection => self.hierarchy_flip(),
            HierarchyAction::Refresh => self.hierarchy_refresh(),
            HierarchyAction::Up
            | HierarchyAction::Down
            | HierarchyAction::PageUp
            | HierarchyAction::PageDown
            | HierarchyAction::Home
            | HierarchyAction::End
            | HierarchyAction::ExportMermaid
            | HierarchyAction::ExportDot => {}
        }
    }

    /// Write the loaded tree to a new scratch file: Mermaid in a markdown fence, or DOT.
    pub(crate) fn hierarchy_export(&mut self, dot: bool) {
        let Some(t) = self.hierarchy.as_ref().filter(|t| !t.roots.is_empty()) else {
            return self.lsp_hint("No hierarchy to export");
        };
        let (ext, content) =
            if dot { ("dot", t.to_dot()) } else { ("md", format!("# {}\n\n```mermaid\n{}```\n", t.title(), t.to_mermaid())) };
        self.open_new_scratch(ext, &content);
    }

    /// The server `name` is gone: its answers will not come, so nothing keeps loading.
    pub(super) fn hierarchy_server_gone(&mut self, name: &str) {
        let Some(t) = self.hierarchy.as_mut().filter(|t| t.server == name) else { return };
        t.nodes.iter_mut().for_each(|n| n.loading = false);
        self.dirty_frame = true;
    }

    /// Expand `node`: its children are asked the first time, later they are shown again.
    pub(crate) fn hierarchy_expand(&mut self, node: usize) {
        let Some(n) = self.hierarchy.as_mut().and_then(|t| t.nodes.get_mut(node)) else { return };
        if n.children.is_some() {
            n.expanded = true;
            self.dirty_frame = true;
        } else if !n.loading {
            self.hierarchy_ask(node);
        }
    }

    pub(crate) fn hierarchy_collapse(&mut self, node: usize) {
        if let Some(t) = &mut self.hierarchy {
            t.collapse(node);
            self.dirty_frame = true;
        }
    }

    /// The same roots the other way (callers or callees, supertypes or subtypes).
    pub(crate) fn hierarchy_flip(&mut self) {
        self.hierarchy_restart(|t| t.direction.flipped());
    }

    /// The same roots and direction, every child asked again.
    pub(crate) fn hierarchy_refresh(&mut self) {
        self.hierarchy_restart(|t| t.direction);
    }

    /// Open the item at the start of its selection, like a go-to-definition jump.
    pub(crate) fn hierarchy_open(&mut self, node: usize) {
        let Some(item) = self.hierarchy.as_ref().and_then(|t| t.nodes.get(node)).map(|n| &n.item) else { return };
        let (path, start) = (item.path.clone(), item.selection.start);
        self.editor.push_nav_here();
        self.open_file_at_utf16(path, start);
    }

    /// A new tree id from the LSP seq counter; answers for an older tree are dropped.
    fn hierarchy_next_id(&mut self) -> u64 {
        self.lsp.last_seq += 1;
        self.lsp.last_seq
    }

    fn hierarchy_restart(&mut self, direction: impl FnOnce(&HierarchyTree) -> Direction) {
        let Some(tree) = &self.hierarchy else { return };
        let direction = direction(tree);
        let id = self.hierarchy_next_id();
        let restarted = self.hierarchy.as_ref().map(|t| t.restarted(id, direction));
        let has_root = restarted.as_ref().is_some_and(|t| !t.roots.is_empty());
        self.hierarchy = restarted;
        if has_root {
            self.hierarchy_ask(0);
        }
        self.dirty_frame = true;
    }

    /// Ask the tree's server for `node`'s children, or compute them from the index.
    fn hierarchy_ask(&mut self, node: usize) {
        let Some(t) = &self.hierarchy else { return };
        let Some(n) = t.nodes.get(node) else { return };
        if t.from_index() {
            return self.hierarchy_index_children(node);
        }
        let (tree, server, method, raw) = (t.id, t.server.clone(), t.direction.method(), n.item.raw.clone());
        let client = self.lsp.clients.get_mut(&server).filter(|c| c.capabilities().is_some());
        let Some(client) = client else { return self.lsp_hint(format!("{server} is not running")) };
        client.request(method, json!({"item": raw}), PendingTag::HierarchyChildren { tree, node });
        if let Some(n) = self.hierarchy.as_mut().and_then(|t| t.nodes.get_mut(node)) {
            n.loading = true;
        }
        self.dirty_frame = true;
    }

    /// Fill `node`'s children of an index tree at once.
    fn hierarchy_index_children(&mut self, node: usize) {
        let Some((direction, item)) = self.hierarchy.as_ref().and_then(|t| t.nodes.get(node).map(|n| (t.direction, n.item.clone()))) else {
            return;
        };
        let defs = match direction {
            Direction::Callers => graph::callers(&self.symbols, &item.name),
            Direction::Callees => match self.rel_path(&item.path) {
                Some(rel) => match graph::find(&self.symbols, &rel, &item.name, item.selection.start.line as usize) {
                    Some(sym) => graph::callees(&mut self.symbols, &rel, sym),
                    None => Vec::new(),
                },
                None => Vec::new(),
            },
            Direction::Supertypes | Direction::Subtypes => Vec::new(),
        };
        let items = self.hierarchy_items(defs);
        if let Some(t) = &mut self.hierarchy {
            t.set_children(node, items);
        }
        self.dirty_frame = true;
    }

    /// Index definitions as items: `raw` is null and the selection is the name
    /// in UTF-16 columns, from the open buffer's line else the file on disk.
    pub(super) fn hierarchy_items(&mut self, defs: Vec<graph::Def>) -> Vec<HierarchyItem> {
        let root = self.workspace.root.clone();
        let mut disk: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut items = Vec::with_capacity(defs.len());
        for d in defs {
            let (line, col) = (d.symbol.line, d.symbol.col);
            let abs = root.join(&*d.path);
            let open = self.editor.index_of(&abs).map(|i| &self.editor.buffers[i]).filter(|b| line < b.rope.len_lines());
            let text = match open {
                Some(b) => Some(b.line_text(line)),
                None => disk
                    .entry(abs.clone())
                    .or_insert_with(|| std::fs::read(&abs).ok().map(|b| String::from_utf8_lossy(&b).into_owned()))
                    .as_deref()
                    .and_then(|s| crate::util::text::lines_inclusive(s).nth(line).map(crate::git::diff::strip_eol))
                    .map(str::to_string),
            };
            let ch = text.map_or(col, |t| t.chars().take(col).map(char::len_utf16).sum());
            let len = d.symbol.name.encode_utf16().count();
            items.push(HierarchyItem {
                name: d.symbol.name.to_string(),
                detail: None,
                kind: graph::lsp_kind(d.symbol.kind),
                path: abs,
                selection: Range::new(Position::new(line as u32, ch as u32), Position::new(line as u32, (ch + len) as u32)),
                raw: Value::Null,
            });
        }
        items
    }

    /// The answer to a children request: dropped unless it is for the
    /// current tree and a node waiting for it.
    pub(super) fn hierarchy_children_answer(&mut self, name: &str, tree: u64, node: usize, result: Result<Value, ResponseError>) {
        let waiting = self.hierarchy.as_ref().is_some_and(|t| t.id == tree && t.nodes.get(node).is_some_and(|n| n.loading));
        if !waiting {
            return tracing::debug!("dropping a hierarchy answer for an old tree");
        }
        let Some(t) = &mut self.hierarchy else { return };
        self.dirty_frame = true;
        match result {
            Ok(v) => {
                let items = if t.direction.is_call() { parse_calls(v, t.direction) } else { parse_types(v) };
                t.set_children(node, items);
            }
            Err(e) => {
                t.nodes[node].loading = false;
                if ![ResponseError::REQUEST_CANCELLED, ResponseError::CONTENT_MODIFIED].contains(&e.code) {
                    self.lsp_hint(format!("{name}: {}", e.message));
                }
            }
        }
    }
}

#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, LspAction};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::event::{AppMsg, Mouse};
    use crate::lsp::LspMsg;
    use crate::lsp::fake::FakeServer;
    use crate::ui::focus::{PanelId, ToolWindowId};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const A: &str = "fn main() {\n    let s = \"日本😀\"; helper(s);\n}\n";
    const B: &str = "// b\n/* 😀 */ pub fn helper(s: &str) {}\n";

    fn call_caps() -> Value {
        let mut c = incremental();
        c["callHierarchyProvider"] = json!(true);
        c
    }

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    fn item(path: &std::path::Path, name: &str, line: u32, c0: u32, c1: u32) -> Value {
        json!({
            "name": name, "kind": 12, "uri": crate::lsp::sync::uri(path).unwrap().as_str(),
            "range": range(line, 0, line, c1 + 10), "selectionRange": range(line, c0, line, c1),
            "data": {"n": name},
        })
    }

    fn caret(app: &App) -> (usize, usize) {
        let b = app.editor.active_buffer().unwrap();
        crate::editor::selection::pos_to_line_col(&b.rope, b.view.sel().head)
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    fn goto(app: &mut App, line: usize, col: usize) {
        app.editor.active_buffer_mut().unwrap().goto_line(line, col);
    }

    async fn answer(app: &mut App, server: &mut FakeServer, method: &str, result: Value) -> Value {
        let req = server.recv(method).await;
        server.respond(req["id"].as_i64().unwrap(), result);
        handle_next(app, is_lsp).await;
        req
    }

    async fn sent(app: &App, server: &mut FakeServer) -> Vec<String> {
        let msgs = server.sent_so_far(&app.lsp.clients["rust-analyzer"]).await;
        msgs.iter().filter_map(|m| m["method"].as_str().map(str::to_string)).collect()
    }

    struct Setup {
        app: App,
        dir: std::path::PathBuf,
        server: FakeServer,
        a: std::path::PathBuf,
        b: std::path::PathBuf,
    }

    /// `a.rs` open with the caret on `helper`, `b.rs` on disk, call hierarchy offered.
    async fn setup(name: &str, caps: Value) -> (Setup, tokio::sync::mpsc::UnboundedReceiver<FakeServer>) {
        let (mut app, dir) = test_app(name);
        let mut servers = with_fake_servers(&mut app, caps);
        let server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        goto(&mut app, 1, 21);
        // The startup note about the keyboard protocol is not under test.
        app.toasts = Default::default();
        (Setup { app, dir, server, a, b }, servers)
    }

    fn helper(s: &Setup) -> Value {
        item(&s.b, "helper", 1, 16, 22)
    }

    /// Prepare answered with `helper`; the `incomingCalls` request is left pending, its id returned.
    async fn started(s: &mut Setup) -> (Value, i64) {
        s.app.hierarchy_start(true);
        let root = helper(s);
        answer(&mut s.app, &mut s.server, PREPARE_CALL_HIERARCHY, json!([root])).await;
        let req = s.server.recv("callHierarchy/incomingCalls").await;
        let id = req["id"].as_i64().unwrap();
        (req, id)
    }

    #[tokio::test]
    async fn export_writes_the_loaded_tree_to_a_scratch_file() {
        let (mut s, _servers) = setup("hier-export", call_caps()).await;
        let (_, id) = started(&mut s).await;
        let main = item(&s.a, "main", 0, 3, 7);
        s.server.respond(id, json!([{"from": main, "fromRanges": []}]));
        handle_next(&mut s.app, is_lsp).await;
        let before = sent(&s.app, &mut s.server).await.iter().filter(|m| *m == "callHierarchy/incomingCalls").count();
        s.app.perform(Action::Hierarchy(HierarchyAction::ExportMermaid));
        let path = s.app.paths.scratches_dir().join("scratch_1.md");
        assert_eq!(s.app.editor.active_path(), Some(path.as_path()));
        let buf = s.app.editor.active_buffer().unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!buf.dirty());
        assert_eq!(buf.text(), on_disk);
        assert_eq!(
            on_disk,
            "# Callers of helper\n\n```mermaid\nflowchart LR\n    n0[\"helper<br/>b.rs:2\"]\n    n1[\"main<br/>a.rs:1 …\"]\n    n1 --> n0\n```\n"
        );
        assert!(s.app.focus.is(PanelId::Editor));
        s.app.perform(Action::Hierarchy(HierarchyAction::ExportDot));
        let dot = s.app.paths.scratches_dir().join("scratch_1.dot");
        assert_eq!(s.app.editor.active_path(), Some(dot.as_path()));
        assert!(std::fs::read_to_string(&dot).unwrap().starts_with("digraph \"Callers of helper\" {"));
        let after = sent(&s.app, &mut s.server).await.iter().filter(|m| *m == "callHierarchy/incomingCalls").count();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn export_without_a_tree_only_hints() {
        let (mut app, dir) = test_app("hier-export-none");
        app.perform(Action::Hierarchy(HierarchyAction::ExportMermaid));
        assert!(toasts(&app).iter().any(|t| t.contains("No hierarchy to export")));
        assert!(!app.paths.scratches_dir().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn call_hierarchy_prepares_at_the_caret_and_asks_the_callers() {
        let (mut s, _servers) = setup("hier-call", call_caps()).await;
        s.app.hierarchy_start(true);
        let root = helper(&s);
        let req = answer(&mut s.app, &mut s.server, PREPARE_CALL_HIERARCHY, json!([root])).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 22}));
        assert!(req["params"]["textDocument"]["uri"].as_str().unwrap().ends_with("a.rs"));
        let t = s.app.hierarchy.as_ref().unwrap();
        assert_eq!((t.direction, t.title(), t.server.as_str()), (Direction::Callers, "Callers of helper".into(), "rust-analyzer"));
        assert!(t.nodes[0].loading);
        let req = s.server.recv("callHierarchy/incomingCalls").await;
        assert_eq!(req["params"]["item"], helper(&s));
        let main = item(&s.a, "main", 0, 3, 7);
        let calls = json!([
            {"from": main, "fromRanges": [range(1, 21, 1, 27)]},
            {"from": {"name": "u", "kind": 12, "uri": "untitled:x", "range": range(0, 0, 0, 1), "selectionRange": range(0, 0, 0, 1)}, "fromRanges": []},
        ]);
        s.server.respond(req["id"].as_i64().unwrap(), calls);
        handle_next(&mut s.app, is_lsp).await;
        let t = s.app.hierarchy.as_ref().unwrap();
        assert_eq!(t.rows(), [0, 1]);
        assert_eq!((t.nodes[1].item.name.as_str(), t.nodes[1].depth), ("main", 1));
        assert!(t.nodes[0].expanded && !t.nodes[0].loading);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn expand_asks_once_and_collapse_keeps_the_children() {
        let (mut s, _servers) = setup("hier-expand", call_caps()).await;
        let (_, id) = started(&mut s).await;
        let main = item(&s.a, "main", 0, 3, 7);
        s.server.respond(id, json!([{"from": main, "fromRanges": []}]));
        handle_next(&mut s.app, is_lsp).await;
        s.app.hierarchy_expand(1);
        let req = s.server.recv("callHierarchy/incomingCalls").await;
        assert_eq!(req["params"]["item"], main);
        s.app.hierarchy_expand(1);
        assert!(!sent(&s.app, &mut s.server).await.contains(&"callHierarchy/incomingCalls".to_string()));
        s.server.respond(req["id"].as_i64().unwrap(), json!([]));
        handle_next(&mut s.app, is_lsp).await;
        assert_eq!(s.app.hierarchy.as_ref().unwrap().nodes[1].children, Some(vec![]));
        s.app.hierarchy_expand(1);
        s.app.hierarchy_collapse(0);
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0]);
        s.app.hierarchy_expand(0);
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0, 1]);
        s.app.hierarchy_expand(99);
        s.app.hierarchy_collapse(99);
        assert!(!sent(&s.app, &mut s.server).await.contains(&"callHierarchy/incomingCalls".to_string()));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn flip_asks_the_callees_and_drops_answers_for_the_old_tree() {
        let (mut s, _servers) = setup("hier-flip", call_caps()).await;
        let (_, old) = started(&mut s).await;
        let old_tree = s.app.hierarchy.as_ref().unwrap().id;
        s.app.hierarchy_flip();
        let t = s.app.hierarchy.as_ref().unwrap();
        assert_ne!(t.id, old_tree);
        assert_eq!((t.direction, t.title(), t.nodes.len()), (Direction::Callees, "Callees of helper".into(), 1));
        let req = s.server.recv("callHierarchy/outgoingCalls").await;
        assert_eq!(req["params"]["item"], helper(&s));
        let main = item(&s.a, "main", 0, 3, 7);
        s.server.respond(old, json!([{"from": main, "fromRanges": []}]));
        handle_next(&mut s.app, is_lsp).await;
        let t = s.app.hierarchy.as_ref().unwrap();
        assert!(t.nodes[0].loading && t.nodes[0].children.is_none());
        s.server.respond(req["id"].as_i64().unwrap(), json!([{"to": main, "fromRanges": []}]));
        handle_next(&mut s.app, is_lsp).await;
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0, 1]);
        let before = s.app.hierarchy.as_ref().unwrap().id;
        s.app.hierarchy_refresh();
        let t = s.app.hierarchy.as_ref().unwrap();
        assert_ne!(t.id, before);
        assert_eq!((t.direction, t.nodes.len()), (Direction::Callees, 1));
        s.server.recv("callHierarchy/outgoingCalls").await;
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn type_hierarchy_asks_subtypes_then_supertypes() {
        let (mut s, _servers) = setup("hier-type", incremental()).await;
        s.app.hierarchy_start(false);
        let root = helper(&s);
        answer(&mut s.app, &mut s.server, PREPARE_TYPE_HIERARCHY, json!([root])).await;
        let t = s.app.hierarchy.as_ref().unwrap();
        assert_eq!((t.direction, t.title()), (Direction::Subtypes, "Subtypes of helper".into()));
        let req = answer(&mut s.app, &mut s.server, "typeHierarchy/subtypes", json!([item(&s.a, "main", 0, 3, 7)])).await;
        assert_eq!(req["params"]["item"], helper(&s));
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0, 1]);
        s.app.hierarchy_flip();
        let req = s.server.recv("typeHierarchy/supertypes").await;
        assert_eq!(req["params"]["item"], helper(&s));
        assert_eq!(s.app.hierarchy.as_ref().unwrap().title(), "Supertypes of helper");
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn type_hierarchy_method_not_found_says_so() {
        let (mut s, _servers) = setup("hier-type-none", incremental()).await;
        s.app.hierarchy_start(false);
        let req = s.server.recv(PREPARE_TYPE_HIERARCHY).await;
        s.server.respond_error(req["id"].as_i64().unwrap(), -32601, "unknown request");
        handle_next(&mut s.app, is_lsp).await;
        assert!(toasts(&s.app).contains(&"rust-analyzer has no type hierarchy".to_string()), "{:?}", toasts(&s.app));
        assert!(s.app.hierarchy.is_none());
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn empty_prepare_says_so_and_keeps_the_tree() {
        let (mut s, _servers) = setup("hier-empty", call_caps()).await;
        let (_, id) = started(&mut s).await;
        s.server.respond(id, json!([]));
        handle_next(&mut s.app, is_lsp).await;
        let tree_id = s.app.hierarchy.as_ref().unwrap().id;
        for (call, method, empty, want) in [
            (true, PREPARE_CALL_HIERARCHY, Value::Null, "No call hierarchy here"),
            (true, PREPARE_CALL_HIERARCHY, json!([]), "No call hierarchy here"),
            (false, PREPARE_TYPE_HIERARCHY, json!([]), "No type hierarchy here"),
        ] {
            s.app.toasts = Default::default();
            s.app.hierarchy_start(call);
            answer(&mut s.app, &mut s.server, method, empty).await;
            assert!(toasts(&s.app).contains(&want.to_string()), "{:?}", toasts(&s.app));
            assert_eq!(s.app.hierarchy.as_ref().unwrap().id, tree_id);
        }
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn call_hierarchy_without_the_capability_says_so() {
        let (mut s, _servers) = setup("hier-nocap", incremental()).await;
        s.app.hierarchy_start(true);
        assert!(toasts(&s.app).contains(&"rust-analyzer does not support call hierarchy".to_string()), "{:?}", toasts(&s.app));
        assert!(!sent(&s.app, &mut s.server).await.contains(&PREPARE_CALL_HIERARCHY.to_string()));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn open_jumps_to_the_selection_start_and_back_returns() {
        let (mut s, _servers) = setup("hier-open", call_caps()).await;
        s.app.hierarchy = Some(HierarchyTree::new(
            1,
            "rust-analyzer".into(),
            Direction::Callers,
            crate::lsp::hierarchy::parse_items(json!([helper(&s)])),
        ));
        s.app.hierarchy_open(0);
        handle_next(&mut s.app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(s.app.editor.active_path(), Some(s.b.as_path()));
        assert_eq!(caret(&s.app), (1, 15));
        s.app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!((s.app.editor.active_buffer().unwrap().name.as_str(), caret(&s.app)), ("a.rs", (1, 21)));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn a_child_request_to_a_server_that_is_gone_says_so() {
        let (mut s, _servers) = setup("hier-gone", call_caps()).await;
        s.app.hierarchy =
            Some(HierarchyTree::new(1, "nobody".into(), Direction::Callers, crate::lsp::hierarchy::parse_items(json!([helper(&s)]))));
        s.app.hierarchy_expand(0);
        assert!(toasts(&s.app).contains(&"nobody is not running".to_string()), "{:?}", toasts(&s.app));
        assert!(!s.app.hierarchy.as_ref().unwrap().nodes[0].loading);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn a_failed_child_request_can_be_asked_again() {
        let (mut s, _servers) = setup("hier-err", call_caps()).await;
        let (_, id) = started(&mut s).await;
        s.server.respond_error(id, -32800, "cancelled");
        handle_next(&mut s.app, is_lsp).await;
        assert!(toasts(&s.app).is_empty(), "{:?}", toasts(&s.app));
        let t = s.app.hierarchy.as_ref().unwrap();
        assert!(!t.nodes[0].loading && t.nodes[0].children.is_none());
        s.app.hierarchy_expand(0);
        let req = s.server.recv("callHierarchy/incomingCalls").await;
        s.server.respond_error(req["id"].as_i64().unwrap(), -32603, "boom");
        handle_next(&mut s.app, is_lsp).await;
        assert!(toasts(&s.app).contains(&"rust-analyzer: boom".to_string()), "{:?}", toasts(&s.app));
        assert!(s.app.hierarchy.as_ref().unwrap().nodes[0].children.is_none());
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl_alt_h(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL | KeyModifiers::ALT));
    }

    fn selected(app: &mut App) -> Option<usize> {
        app.hierarchy_panel.selected(app.hierarchy.as_ref())
    }

    fn by_hand(s: &mut Setup) {
        s.app.hierarchy =
            Some(HierarchyTree::new(1, "rust-analyzer".into(), Direction::Callers, crate::lsp::hierarchy::parse_items(json!([helper(s)]))));
    }

    #[tokio::test]
    async fn ctrl_alt_h_shows_the_callers_and_keys_walk_the_tree() {
        let (mut s, _servers) = setup("hier-window", call_caps()).await;
        assert!(s.app.focus.is(PanelId::Editor));
        ctrl_alt_h(&mut s.app);
        assert!(!s.app.layout.is_visible(ToolWindowId::Hierarchy));
        let root = helper(&s);
        answer(&mut s.app, &mut s.server, PREPARE_CALL_HIERARCHY, json!([root])).await;
        assert!(s.app.layout.is_visible(ToolWindowId::Hierarchy));
        assert!(s.app.focus.is(PanelId::Hierarchy));
        let req = s.server.recv("callHierarchy/incomingCalls").await;
        let main = item(&s.a, "main", 0, 3, 7);
        s.server.respond(req["id"].as_i64().unwrap(), json!([{"from": main, "fromRanges": []}]));
        handle_next(&mut s.app, is_lsp).await;
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0, 1]);
        assert_eq!(selected(&mut s.app), Some(0));
        key(&mut s.app, KeyCode::Down);
        assert_eq!(selected(&mut s.app), Some(1));
        key(&mut s.app, KeyCode::Right);
        let req = s.server.recv("callHierarchy/incomingCalls").await;
        assert_eq!(req["params"]["item"], main);
        s.server.respond(req["id"].as_i64().unwrap(), json!([]));
        handle_next(&mut s.app, is_lsp).await;
        key(&mut s.app, KeyCode::Left);
        assert_eq!(selected(&mut s.app), Some(0));
        key(&mut s.app, KeyCode::Left);
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0]);
        key(&mut s.app, KeyCode::Right);
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0, 1]);
        key(&mut s.app, KeyCode::Right);
        assert_eq!(selected(&mut s.app), Some(1));
        assert!(!sent(&s.app, &mut s.server).await.contains(&"callHierarchy/incomingCalls".to_string()));
        key(&mut s.app, KeyCode::Tab);
        s.server.recv("callHierarchy/outgoingCalls").await;
        assert_eq!(s.app.hierarchy.as_ref().unwrap().title(), "Callees of helper");
        assert_eq!(selected(&mut s.app), Some(0));
        key(&mut s.app, KeyCode::F(5));
        s.server.recv("callHierarchy/outgoingCalls").await;
        key(&mut s.app, KeyCode::Esc);
        assert!(s.app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn enter_opens_the_selected_function() {
        let (mut s, _servers) = setup("hier-enter", call_caps()).await;
        by_hand(&mut s);
        s.app.toggle_tool_window(ToolWindowId::Hierarchy);
        assert!(s.app.focus.is(PanelId::Hierarchy));
        key(&mut s.app, KeyCode::Enter);
        handle_next(&mut s.app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(s.app.editor.active_path(), Some(s.b.as_path()));
        assert_eq!(caret(&s.app), (1, 15));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn type_hierarchy_action_shows_the_subtypes() {
        let (mut s, _servers) = setup("hier-type-window", incremental()).await;
        s.app.perform(Action::Lsp(LspAction::TypeHierarchy));
        let root = helper(&s);
        answer(&mut s.app, &mut s.server, PREPARE_TYPE_HIERARCHY, json!([root])).await;
        assert!(s.app.layout.is_visible(ToolWindowId::Hierarchy));
        assert!(s.app.focus.is(PanelId::Hierarchy));
        assert_eq!(s.app.hierarchy.as_ref().unwrap().title(), "Subtypes of helper");
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn an_empty_prepare_leaves_the_layout_alone() {
        let (mut s, _servers) = setup("hier-empty-window", call_caps()).await;
        ctrl_alt_h(&mut s.app);
        answer(&mut s.app, &mut s.server, PREPARE_CALL_HIERARCHY, json!([])).await;
        assert!(toasts(&s.app).contains(&"No call hierarchy here".to_string()), "{:?}", toasts(&s.app));
        assert!(!s.app.layout.is_visible(ToolWindowId::Hierarchy));
        assert!(s.app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn clicks_select_open_and_toggle() {
        let (mut s, _servers) = setup("hier-clicks", call_caps()).await;
        by_hand(&mut s);
        let main = crate::lsp::hierarchy::parse_items(json!([item(&s.a, "main", 0, 3, 7)]));
        s.app.hierarchy.as_mut().unwrap().set_children(0, main);
        s.app.toggle_tool_window(ToolWindowId::Hierarchy);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| crate::ui::render(&mut s.app, f)).unwrap();
        let r = s.app.hierarchy_panel.list_rect();
        let click =
            |x: u16, y: u16, clicks: u8| Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks, mods: KeyModifiers::NONE };
        s.app.forward_mouse(PanelId::Hierarchy, r, &click(r.x + 10, r.y + 1, 1));
        assert_eq!(selected(&mut s.app), Some(1));
        assert_eq!(caret(&s.app), (1, 21));
        s.app.forward_mouse(PanelId::Hierarchy, r, &click(r.x + 1, r.y, 1));
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0]);
        s.app.forward_mouse(PanelId::Hierarchy, r, &click(r.x + 1, r.y, 1));
        assert_eq!(s.app.hierarchy.as_ref().unwrap().rows(), [0, 1]);
        s.app.forward_mouse(PanelId::Hierarchy, r, &click(r.x + 10, r.y, 2));
        handle_next(&mut s.app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(s.app.editor.active_path(), Some(s.b.as_path()));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn a_server_exit_stops_the_loading() {
        let (mut s, _servers) = setup("hier-exit", call_caps()).await;
        started(&mut s).await;
        assert!(s.app.hierarchy.as_ref().unwrap().nodes[0].loading);
        let (generation, id) = (s.app.workspace.generation, s.app.lsp.clients["rust-analyzer"].id);
        s.app.handle_msg(AppMsg::Lsp(LspMsg::Exited { generation, server: id, status: "test".into() }));
        assert!(!s.app.hierarchy.as_ref().unwrap().nodes[0].loading);
        s.app.hierarchy_expand(0);
        assert!(toasts(&s.app).contains(&"rust-analyzer is not running".to_string()), "{:?}", toasts(&s.app));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn a_restart_stops_the_loading() {
        let (mut s, _servers) = setup("hier-restart", call_caps()).await;
        started(&mut s).await;
        s.app.perform(Action::Lsp(LspAction::Restart));
        assert!(!s.app.hierarchy.as_ref().unwrap().nodes[0].loading);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    const IDX_B: &str = "pub fn helper() {}\n";
    const IDX_MAIN: &str = "mod b;\nuse b::helper;\n\nfn main() {\n    helper();\n    run();\n}\n\n/* 😀 */ fn run() {\n    helper();\n}\n";

    /// Write `files` under the root and index them as a finished run.
    fn index(app: &mut App, files: &[(&str, &str)]) {
        let root = app.workspace.root.clone();
        for (rel, text) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        app.symbols.set_files(&files.iter().map(|(p, _)| p.to_string()).collect::<Vec<_>>());
        let job = app.symbols.start(root);
        let seq = job.seq;
        let mut batches = Vec::new();
        job.run(|b| batches.push(b));
        for b in batches {
            app.symbols.on_batch(seq, b);
        }
    }

    fn child_names(app: &App, node: usize) -> Vec<String> {
        let t = app.hierarchy.as_ref().unwrap();
        t.nodes[node].children.as_ref().unwrap().iter().map(|&c| t.nodes[c].item.name.clone()).collect()
    }

    #[tokio::test]
    async fn call_hierarchy_without_a_server_uses_the_index() {
        let (mut app, dir) = test_app("hier-index");
        index(&mut app, &[("src/b.rs", IDX_B), ("src/main.rs", IDX_MAIN)]);
        crate::app::tests::load(&mut app, "src/b.rs", IDX_B);
        goto(&mut app, 0, 8);
        app.toasts = Default::default();
        app.perform(Action::Lsp(LspAction::CallHierarchy));
        let t = app.hierarchy.as_ref().expect("an index tree");
        assert!(t.from_index() && t.server.is_empty());
        assert_eq!(t.title(), "Callers of helper (approximate)");
        let root = &t.nodes[0].item;
        assert_eq!(
            (root.path.clone(), root.selection.start, root.kind, root.raw.clone()),
            (dir.join("root/src/b.rs"), Position::new(0, 7), lsp_types::SymbolKind::FUNCTION, Value::Null)
        );
        let kids: Vec<_> = t.nodes[0]
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|&c| (t.nodes[c].item.name.clone(), t.nodes[c].item.selection.start))
            .collect();
        assert_eq!(kids, [("main".to_string(), Position::new(3, 3)), ("run".to_string(), Position::new(8, 12))]);
        assert!(t.nodes.iter().all(|n| !n.loading));
        assert!(app.layout.is_visible(ToolWindowId::Hierarchy));
        assert_eq!(app.focus.current, PanelId::Hierarchy);
        assert!(toasts(&app).is_empty(), "{:?}", toasts(&app));
        app.perform(Action::Hierarchy(HierarchyAction::ToggleDirection));
        assert_eq!(app.hierarchy.as_ref().unwrap().title(), "Callees of helper (approximate)");
        assert_eq!(app.hierarchy.as_ref().unwrap().nodes[0].children, Some(vec![]));
        app.perform(Action::Hierarchy(HierarchyAction::ToggleDirection));
        app.perform(Action::Hierarchy(HierarchyAction::Down));
        app.perform(Action::Hierarchy(HierarchyAction::Expand));
        let t = app.hierarchy.as_ref().unwrap();
        assert_eq!(t.nodes[t.nodes[0].children.as_ref().unwrap()[0]].children, Some(vec![]));
        // Callees of `main`.
        crate::app::tests::load(&mut app, "src/main.rs", IDX_MAIN);
        goto(&mut app, 3, 4);
        app.perform(Action::Lsp(LspAction::CallHierarchy));
        app.perform(Action::Hierarchy(HierarchyAction::ToggleDirection));
        assert_eq!(child_names(&app, 0), ["helper", "run"]);
        let t = app.hierarchy.as_ref().unwrap();
        assert_eq!(t.nodes[t.nodes[0].children.as_ref().unwrap()[0]].item.path, dir.join("root/src/b.rs"));
        app.perform(Action::Hierarchy(HierarchyAction::Refresh));
        assert_eq!(child_names(&app, 0), ["helper", "run"]);
        app.perform(Action::Hierarchy(HierarchyAction::Down));
        app.perform(Action::Hierarchy(HierarchyAction::Down));
        app.perform(Action::Hierarchy(HierarchyAction::Open));
        assert_eq!(app.editor.active_path(), Some(dir.join("root/src/main.rs").as_path()));
        assert_eq!(caret(&app), (8, 11));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn call_hierarchy_without_a_definition_hints() {
        let (mut app, dir) = test_app("hier-index-none");
        let text = "fn k() {\n    nothing();\n}\n";
        index(&mut app, &[("src/x.rs", text)]);
        crate::app::tests::load(&mut app, "src/x.rs", text);
        goto(&mut app, 1, 6);
        app.toasts = Default::default();
        app.perform(Action::Lsp(LspAction::CallHierarchy));
        assert!(toasts(&app).contains(&"No call hierarchy here".to_string()), "{:?}", toasts(&app));
        assert!(app.hierarchy.is_none());
        crate::app::tests::load(&mut app, "notes.txt", "word here\n");
        app.toasts = Default::default();
        app.perform(Action::Lsp(LspAction::CallHierarchy));
        assert!(toasts(&app).iter().any(|t| t.contains("off")), "{:?}", toasts(&app));
        assert!(app.hierarchy.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn root_switch_clears_the_tree() {
        let (mut app, dir) = test_app("hier-root");
        app.hierarchy = Some(HierarchyTree::new(1, "x".into(), Direction::Callers, vec![]));
        app.lsp_root_changed();
        assert!(app.hierarchy.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
