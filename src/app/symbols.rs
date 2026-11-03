//! The project symbol index (runs off the main thread, fed to the Go to
//! Symbol palette) and the Structure window's sync with the active buffer.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::App;
use crate::event::AppMsg;
use crate::lsp::caps::{Caps, WORKSPACE_SYMBOL};
use crate::lsp::transport::ResponseError;
use crate::lsp::{PendingTag, SCRATCH_SUFFIX};
use crate::symbols::index::{SymbolBatch, SymbolEntry};
use crate::ui::focus::{PanelId, ToolWindowId};
use crate::ui::palette::Source;

/// An open palette gets the growing index at most this often.
const SYMBOLS_FEED: Duration = Duration::from_millis(300);

/// `workspace/symbol` requests for the Symbols palette (in `LspService`).
#[derive(Default)]
pub(super) struct WsSymbols {
    /// The query last sent (trimmed); empty when none.
    pub query: String,
    /// Seq of that query (from `LspService::last_seq`).
    pub seq: u64,
    /// Its requests not answered yet: client name and request id.
    pub pending: Vec<(String, i64)>,
    /// The seq whose answers `entries` hold.
    pub entries_seq: u64,
    pub entries: Vec<SymbolEntry>,
}

impl App {
    /// Cancel the requests of the last query.
    pub(super) fn cancel_workspace_symbols(&mut self) {
        for (name, id) in std::mem::take(&mut self.lsp.ws_symbols.pending) {
            if let Some(c) = self.lsp.clients.get(&name) {
                c.cancel(id);
            }
        }
    }

    /// Cancel the requests and forget the rows and the query, under a fresh
    /// seq that no earlier request carries, so no late answer is kept.
    pub(super) fn reset_workspace_symbols(&mut self) {
        self.cancel_workspace_symbols();
        self.lsp.last_seq += 1;
        self.lsp.ws_symbols = WsSymbols { seq: self.lsp.last_seq, ..Default::default() };
    }

    /// Close the palette, ending its workspace symbol requests.
    pub(crate) fn close_palette(&mut self) {
        self.palette = None;
        self.reset_workspace_symbols();
    }

    /// Ask the servers for the symbols matching the Symbols palette's query
    /// when it changed; each query cancels the previous one's requests.
    pub(super) fn workspace_symbols_sync(&mut self) {
        let Some(p) = self.palette.as_ref().filter(|p| p.wants(Source::Symbols) && p.source() == Source::Symbols) else { return };
        let query = p.input.text.trim().to_string();
        if query == self.lsp.ws_symbols.query {
            return;
        }
        // A new seq even when nothing is asked: answers to the old query
        // must not refill the rows. The old rows stay until the new
        // query's first answer replaces them (`entries_seq`).
        self.cancel_workspace_symbols();
        self.lsp.last_seq += 1;
        let seq = self.lsp.last_seq;
        self.lsp.ws_symbols.seq = seq;
        self.lsp.ws_symbols.query = query.clone();
        if query.is_empty() || !self.config.lsp.enabled {
            self.lsp.ws_symbols.entries.clear();
            return self.feed_palette_symbols();
        }
        let mut names: Vec<String> = self.lsp.clients.keys().filter(|n| !n.ends_with(SCRATCH_SUFFIX)).cloned().collect();
        names.sort();
        for name in names {
            let Some(c) = self.lsp.clients.get_mut(&name) else { continue };
            let has = c.capabilities().is_some_and(|fixed| Caps { fixed, registry: &c.registry, doc: None }.has(WORKSPACE_SYMBOL));
            if has {
                let id = c.request(WORKSPACE_SYMBOL, json!({"query": query}), PendingTag::WorkspaceSymbols(seq));
                self.lsp.ws_symbols.pending.push((name, id));
            }
        }
        self.feed_palette_symbols();
    }

    /// A server's `workspace/symbol` answer: kept only for the newest query.
    pub(super) fn workspace_symbols_answer(&mut self, name: &str, seq: u64, result: Result<Value, ResponseError>) {
        if seq != self.lsp.ws_symbols.seq {
            return tracing::debug!("dropping a superseded workspace/symbol answer");
        }
        let ws = &mut self.lsp.ws_symbols;
        ws.pending.retain(|(n, _)| n != name);
        if ws.entries_seq != seq {
            ws.entries.clear();
            ws.entries_seq = seq;
        }
        match result {
            Ok(v) => {
                for e in crate::lsp::symbols::workspace_entries(v, &self.workspace.root) {
                    let same = |o: &SymbolEntry| {
                        o.path == e.path && o.symbol.line == e.symbol.line && o.symbol.col == e.symbol.col && o.symbol.name == e.symbol.name
                    };
                    if !ws.entries.iter().any(same) {
                        ws.entries.push(e);
                    }
                }
            }
            Err(e) => {
                tracing::debug!("{name}: workspace/symbol failed: {e:?}");
                // The next key asks again (a server still indexing times out).
                ws.query.clear();
            }
        }
        self.feed_palette_symbols();
    }

    /// The index's rows followed by the servers' that the index lacks (a
    /// file and line it already has).
    pub(super) fn palette_symbols(&mut self) -> Arc<Vec<SymbolEntry>> {
        let index = self.symbols.entries();
        if self.lsp.ws_symbols.entries.is_empty() {
            return index;
        }
        let known: HashSet<(&str, usize)> = index.iter().map(|e| (&*e.path, e.symbol.line)).collect();
        let extra = self.lsp.ws_symbols.entries.iter().filter(|e| !known.contains(&(&*e.path, e.symbol.line))).cloned();
        let all: Vec<SymbolEntry> = index.iter().cloned().chain(extra).collect();
        Arc::new(all)
    }

    fn feed_palette_symbols(&mut self) {
        if !self.palette.as_ref().is_some_and(|p| p.wants(Source::Symbols)) {
            return;
        }
        let rows = self.palette_symbols();
        let loading = self.symbols.indexing() || !self.lsp.ws_symbols.pending.is_empty();
        if let Some(p) = self.palette.as_mut() {
            p.set_symbols(rows, loading);
        }
    }

    /// Run the index, or once more after the run in flight.
    pub(super) fn request_symbols(&mut self) {
        if self.symbols.request() {
            self.spawn_symbols();
        }
    }

    fn spawn_symbols(&mut self) {
        let root = self.workspace.root.clone();
        let job = self.symbols.start(root.clone()).with_cache(crate::symbols::cache::file(&self.paths.cache_dir, &root));
        let (generation, seq, tx) = (self.workspace.generation, job.seq, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            job.run(|batch| {
                let _ = tx.send(AppMsg::Symbols { generation, seq, batch });
            })
        });
    }

    /// Write the index to its cache file after a run that changed something.
    fn save_symbol_cache(&mut self) {
        let Some(files) = self.symbols.take_cache() else { return };
        let root = self.workspace.root.clone();
        let file = crate::symbols::cache::file(&self.paths.cache_dir, &root);
        tokio::task::spawn_blocking(move || match crate::symbols::cache::save(&file, &root, &files) {
            Ok(()) => tracing::debug!("symbol cache: wrote {} files", files.len()),
            Err(e) => tracing::debug!("symbol cache {}: {e}", file.display()),
        });
    }

    pub(super) fn symbols_loaded(&mut self, generation: u64, seq: u64, batch: SymbolBatch) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping symbols of a previous root");
            return;
        }
        let done = batch.done;
        match self.symbols.on_batch(seq, batch) {
            None => return tracing::debug!("dropping symbols of an older index run"),
            Some(true) => self.spawn_symbols(),
            Some(false) => {}
        }
        if done {
            tracing::debug!("symbol index: {} files", self.symbols.file_count());
            self.save_symbol_cache();
        }
        let due = done || self.symbols_fed.is_none_or(|t| t.elapsed() >= SYMBOLS_FEED);
        if due && self.palette.as_ref().is_some_and(|p| p.wants(Source::Symbols)) {
            self.feed_palette_symbols();
            self.symbols_fed = Some(Instant::now());
        }
    }

    /// Watcher paths: parse changed files again; an unknown source file
    /// means the file index (which the symbol index follows) is out of date.
    pub(super) fn symbols_changed(&mut self, paths: &[PathBuf]) {
        let root = &self.workspace.root;
        let rels: Vec<String> =
            paths.iter().filter_map(|p| p.strip_prefix(root).ok()).map(|r| r.to_string_lossy().replace('\\', "/")).collect();
        let (queued, unknown) = self.symbols.invalidate(&rels);
        if queued {
            self.request_symbols();
        }
        if unknown {
            self.refresh_file_index();
        }
    }

    /// Bring the Structure window up to date with the active buffer (every
    /// frame while it is shown; re-extracts only after a change).
    pub(crate) fn sync_structure(&mut self) {
        if !self.layout.is_visible(ToolWindowId::Structure) {
            return;
        }
        let follow = !self.focus.is(PanelId::Structure);
        self.structure.sync(self.editor.active_buffer_mut(), follow);
    }
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use crate::actions::PaletteKind;
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::event::AppMsg;

    fn type_str(app: &mut crate::app::App, s: &str) {
        for c in s.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn rows(app: &mut crate::app::App) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let buf = term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone();
        (0..30).map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect()).collect()
    }

    fn symbol_info(name: &str, uri: &str, line: u32, ch: u32) -> serde_json::Value {
        let range = json!({"start": {"line": line, "character": ch}, "end": {"line": line, "character": ch + 6}});
        json!({"name": name, "kind": 12, "location": {"uri": uri, "range": range}})
    }

    #[tokio::test]
    async fn workspace_symbols_merge_into_the_palette() {
        let (mut app, dir) = test_app("ws-symbols");
        let caps = {
            let mut c = incremental();
            c["workspaceSymbolProvider"] = json!(true);
            c
        };
        let mut servers = with_fake_servers(&mut app, caps);
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, "pub fn helper() {}\n").unwrap();
        app.open_palette(PaletteKind::Symbols);
        let sent = server.sent_so_far(&app.lsp.clients["rust-analyzer"]).await;
        assert!(sent.iter().all(|m| m["method"] != "workspace/symbol"), "an empty query sends nothing: {sent:?}");
        type_str(&mut app, "hel");
        let mut ids = Vec::new();
        for (i, q) in ["h", "he", "hel"].iter().enumerate() {
            if i > 0 {
                let cancel = server.recv("$/cancelRequest").await;
                assert_eq!(cancel["params"]["id"], ids[i - 1], "the previous request is cancelled first");
            }
            let req = server.recv("workspace/symbol").await;
            assert_eq!(req["params"], json!({"query": q}));
            ids.push(req["id"].clone());
        }
        // An older query answered late is dropped.
        let late = json!([symbol_info("hello", "file:///tmp/late/l.rs", 0, 0)]);
        server.respond(ids[0].as_i64().unwrap(), late);
        handle_next(&mut app, is_lsp).await;
        assert!(app.lsp.ws_symbols.entries.is_empty());
        let uri = crate::lsp::sync::uri(&b).unwrap();
        let answer = json!([symbol_info("helper", uri.as_str(), 0, 7), symbol_info("unhelpful", "file:///tmp/elsewhere/x.rs", 4, 0)]);
        server.respond(ids[2].as_i64().unwrap(), answer);
        handle_next(&mut app, is_lsp).await;
        let names: Vec<_> = app.lsp.ws_symbols.entries.iter().map(|e| e.symbol.name.to_string()).collect();
        assert_eq!(names, ["helper", "unhelpful"]);
        assert!(app.lsp.ws_symbols.pending.is_empty(), "the newest query is answered");
        let screen = rows(&mut app);
        assert!(screen.iter().any(|r| r.contains("helper") && r.contains("b.rs:1")), "{screen:#?}");
        assert!(screen.iter().any(|r| r.contains("unhelpful") && r.contains("/tmp/elsewhere/x.rs:5")), "{screen:#?}");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        let buf = app.editor.active_buffer().unwrap();
        assert_eq!(crate::editor::selection::pos_to_line_col(&buf.rope, buf.view.sel().head), (0, 7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn workspace_symbols_skip_servers_without_it_and_empty_queries() {
        let (mut app, dir) = test_app("ws-symbols-none");
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "ma");
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        let sent = server.sent_so_far(&app.lsp.clients["rust-analyzer"]).await;
        assert!(sent.iter().all(|m| m["method"] != "workspace/symbol"), "{sent:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ws_caps() -> serde_json::Value {
        let mut c = incremental();
        c["workspaceSymbolProvider"] = json!(true);
        c
    }

    fn late_answer() -> serde_json::Value {
        json!([symbol_info("hello", "file:///tmp/late/l.rs", 0, 0)])
    }

    #[tokio::test]
    async fn rows_stay_until_the_next_queries_answer() {
        let (mut app, dir) = test_app("ws-symbols-keep");
        let mut servers = with_fake_servers(&mut app, ws_caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "h");
        let id = server.recv("workspace/symbol").await["id"].as_i64().unwrap();
        server.respond(id, late_answer());
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp.ws_symbols.entries.len(), 1);
        type_str(&mut app, "e");
        let id = server.recv("workspace/symbol").await["id"].as_i64().unwrap();
        assert_eq!(app.lsp.ws_symbols.entries.len(), 1, "the old row stays until the new answer");
        server.respond(id, json!([symbol_info("hex", "file:///tmp/late/x.rs", 0, 0)]));
        handle_next(&mut app, is_lsp).await;
        let names: Vec<_> = app.lsp.ws_symbols.entries.iter().map(|e| e.symbol.name.to_string()).collect();
        assert_eq!(names, ["hex"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_answer_to_an_erased_query_is_dropped() {
        let (mut app, dir) = test_app("ws-symbols-erased");
        let mut servers = with_fake_servers(&mut app, ws_caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "h");
        let id = server.recv("workspace/symbol").await["id"].as_i64().unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        server.respond(id, late_answer());
        handle_next(&mut app, is_lsp).await;
        assert!(app.lsp.ws_symbols.entries.is_empty(), "the query is empty now");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_answer_with_lsp_off_is_dropped() {
        let (mut app, dir) = test_app("ws-symbols-off");
        let mut servers = with_fake_servers(&mut app, ws_caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "h");
        let id = server.recv("workspace/symbol").await["id"].as_i64().unwrap();
        app.config.lsp.enabled = false;
        type_str(&mut app, "e");
        server.respond(id, late_answer());
        handle_next(&mut app, is_lsp).await;
        assert!(app.lsp.ws_symbols.entries.is_empty(), "LSP is off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_answer_of_the_previous_palette_is_dropped() {
        let (mut app, dir) = test_app("ws-symbols-reopen");
        let mut servers = with_fake_servers(&mut app, ws_caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "h");
        let id = server.recv("workspace/symbol").await["id"].as_i64().unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.palette.is_none());
        server.respond(id, late_answer());
        handle_next(&mut app, is_lsp).await;
        assert!(app.lsp.ws_symbols.entries.is_empty(), "the palette is closed");
        assert!(app.lsp.ws_symbols.pending.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn closing_the_palette_cancels_the_request() {
        let (mut app, dir) = test_app("ws-symbols-close");
        let mut servers = with_fake_servers(&mut app, ws_caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "h");
        let id = server.recv("workspace/symbol").await["id"].clone();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(server.recv("$/cancelRequest").await["params"]["id"], id);
        assert!(app.lsp.ws_symbols.pending.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_reopened_palette_drops_the_old_sessions_answer() {
        let (mut app, dir) = test_app("ws-symbols-reopened");
        let mut servers = with_fake_servers(&mut app, ws_caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "h");
        let id = server.recv("workspace/symbol").await["id"].as_i64().unwrap();
        // The answer lands for the first time, then the palette is reopened.
        server.respond(id, late_answer());
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp.ws_symbols.entries.len(), 1);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.open_palette(PaletteKind::Symbols);
        assert!(app.lsp.ws_symbols.entries.is_empty(), "a fresh palette starts without rows");
        // The same answer, late, is for no request of this palette.
        let seq = app.lsp.ws_symbols.seq;
        app.workspace_symbols_answer("rust-analyzer", seq - 1, Ok(late_answer()));
        assert!(app.lsp.ws_symbols.entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_query_is_asked_again_on_the_next_key() {
        let (mut app, dir) = test_app("ws-symbols-retry");
        let mut servers = with_fake_servers(&mut app, ws_caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        app.open_palette(PaletteKind::Symbols);
        type_str(&mut app, "h");
        let id = server.recv("workspace/symbol").await["id"].as_i64().unwrap();
        server.respond_error(id, -32803, "timed out");
        handle_next(&mut app, is_lsp).await;
        assert!(app.lsp.ws_symbols.query.is_empty(), "the failed query is forgotten");
        // The same text asked again by the next palette key (a no-op edit).
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        let again = server.recv("workspace/symbol").await;
        assert_eq!(again["params"], json!({"query": "h"}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn index_cache_written_after_a_run() {
        let (mut app, dir) = test_app("symbols-cache");
        std::fs::write(app.workspace.root.join("a.rs"), "fn alpha() {}\n").unwrap();
        let generation = app.workspace.generation;
        app.handle_msg(AppMsg::FileIndex { generation, seq: 1, files: vec!["a.rs".into()] });
        handle_next(&mut app, |m| matches!(m, AppMsg::Symbols { batch, .. } if batch.done)).await;
        let file = crate::symbols::cache::file(&app.paths.cache_dir, &app.workspace.root);
        for _ in 0..500 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let loaded = crate::symbols::cache::load(&file, &app.workspace.root);
        assert!(loaded.contains_key("a.rs"), "{:?}", loaded.keys().collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
