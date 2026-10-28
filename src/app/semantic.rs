//! LSP semantic tokens: the server's colour hints for the focused group's
//! active buffer, kept in `Buffer::semantic` for the editor view to draw
//! over tree-sitter.
//!
//! Driven by the tick, like inlay hints (`app/inlay.rs`). Once the buffer and
//! its version have stayed the same for [`SEMANTIC_DELAY`], one
//! `textDocument/semanticTokens/full` is sent, unless the last answer
//! (`SemanticTokens::fetched`) is for this version. A server with only
//! `/range` gets the visible lines plus one screen above and below, and the
//! visible lines are part of the debounce key then (scrolling out of the
//! fetched lines asks again); with `/full` they are not, so scrolling never
//! cancels or repeats the request. No delta requests. The server's legend is
//! read when the request goes out and kept with it: the answer is decoded
//! against that legend and the rope of the version sent, and counts only for
//! its seq and that version. The request has its own [`Inflight`]
//! (`lsp_send` with `newest = false`); a change of the key cancels it. Until
//! a newer answer arrives the old tokens stay, shifted by the edits. A
//! `/range` answer replaces all stored tokens. An error answer is logged; a
//! timeout or content-modified answer for the current key makes the next
//! ticks ask again, and so does the end of the server's last `$/progress`.
//! `workspace/semanticTokens/refresh` forgets what was fetched. So does a
//! `client/registerCapability` of `textDocument/semanticTokens`. Nothing is
//! asked while `[lsp] semantic_tokens` is off, and turning it off clears
//! every buffer's tokens.

use std::ops::Range;
use std::time::{Duration, Instant};

use lsp_types::{Position, SemanticTokensLegend};
use serde_json::{Value, json};

use super::App;
use super::inlay::TIMED_OUT;
use super::lsp::Inflight;
use crate::editor::buffer::BufferId;
use crate::lsp::caps::{SEMANTIC_TOKENS_FULL, SEMANTIC_TOKENS_RANGE};
use crate::lsp::semantic::{decode, parse};
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag, pos};

/// How long the buffer and version (and the visible lines for a range-only
/// server) must stay put before the server is asked.
pub(super) const SEMANTIC_DELAY: Duration = Duration::from_millis(300);

/// The tokens' debounce and its request in flight (in `LspService`).
#[derive(Debug, Default)]
pub(super) struct SemanticReqs {
    /// Buffer, `Buffer::version`, and the visible lines for a range-only server (else `0..0`).
    key: Option<(BufferId, u64, Range<usize>)>,
    /// When `key` last changed.
    since: Option<Instant>,
    /// The request for `key` was made (or found pointless).
    done: bool,
    /// The request in flight, the legend it decodes with, and the lines it asks for (`None` = `/full`).
    req: Option<(Inflight, SemanticTokensLegend, Option<Range<usize>>)>,
}

impl App {
    /// The tick: restart the debounce when the key changed, else ask the
    /// server once it has held for [`SEMANTIC_DELAY`].
    pub(super) fn semantic_tick(&mut self, now: Instant) {
        let visible = self.editor.visible_lines();
        let on = self.config.lsp.semantic_tokens && self.config.lsp.enabled && !visible.is_empty();
        let Some(b) = self.editor.active_buffer().filter(|_| on) else {
            self.semantic_cancel();
            self.lsp.semantic.key = None;
            return;
        };
        let id = b.id;
        let range_only = self.lsp_caps(id).is_some_and(|c| !c.has(SEMANTIC_TOKENS_FULL) && c.has(SEMANTIC_TOKENS_RANGE));
        let key = (id, b.version, if range_only { visible.clone() } else { 0..0 });
        let state = &mut self.lsp.semantic;
        if state.key.as_ref() != Some(&key) {
            self.semantic_cancel();
            let state = &mut self.lsp.semantic;
            state.key = Some(key);
            state.since = Some(now);
            state.done = false;
            return;
        }
        if state.done || state.since.is_none_or(|since| now < since + SEMANTIC_DELAY) {
            return;
        }
        let covered = match b.semantic.fetched() {
            Some((v, None)) => v == b.version,
            Some((v, Some(r))) => v == b.version && r.start <= visible.start && visible.end <= r.end,
            None => false,
        };
        if covered {
            state.done = true;
            return;
        }
        // Not tracked yet, or the server is still starting: try again next tick.
        let Some(caps) = self.lsp_caps(id) else { return };
        let (full, range, legend) = (caps.has(SEMANTIC_TOKENS_FULL), caps.has(SEMANTIC_TOKENS_RANGE), caps.semantic_legend());
        self.lsp.semantic.done = true;
        let Some(legend) = legend.filter(|_| full || range) else { return };
        let sent = if full {
            self.lsp_send(id, Position::default(), SEMANTIC_TOKENS_FULL, json!({}), PendingTag::SemanticTokens, false).map(|r| (r, None))
        } else {
            let rope = &b.rope;
            let h = visible.len();
            let lo = visible.start.saturating_sub(h);
            let hi = (visible.end + h).min(rope.len_lines());
            let start = pos::position(rope, rope.line_to_char(lo));
            let end = pos::position(rope, if hi < rope.len_lines() { rope.line_to_char(hi) } else { rope.len_chars() });
            let params = json!({"range": {"start": start, "end": end}});
            self.lsp_send(id, start, SEMANTIC_TOKENS_RANGE, params, PendingTag::SemanticTokens, false).map(|r| (r, Some(lo..hi)))
        };
        if let Ok((req, lines)) = sent {
            self.lsp.semantic.req = Some((req, legend, lines));
        }
    }

    /// Cancel the request in flight, if any.
    fn semantic_cancel(&mut self) {
        if let Some((req, ..)) = self.lsp.semantic.req.take()
            && let Some(c) = self.lsp.clients.get(&req.server)
        {
            c.cancel(req.id);
        }
    }

    /// The server answered the tokens request: replace the buffer's tokens
    /// when it is still at the version asked about.
    pub(super) fn semantic_answer(&mut self, t: DocTag, result: Result<Value, ResponseError>) {
        let Some((_, legend, lines)) = self.lsp.semantic.req.take_if(|(r, ..)| r.seq == t.seq) else {
            return tracing::debug!("dropping a superseded semantic tokens answer");
        };
        let Some(b) = self.editor.index_of_id(t.buffer).map(|i| &mut self.editor.buffers[i]) else { return };
        if b.version != t.version {
            return tracing::debug!("dropping a stale semantic tokens answer");
        }
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("semantic tokens: {e}");
                // Still the current key and the server only was not ready (timeout,
                // content modified): ask again once the delay has passed.
                let state = &mut self.lsp.semantic;
                let retry = [TIMED_OUT, ResponseError::CONTENT_MODIFIED].contains(&e.code);
                if retry && state.key.as_ref().is_some_and(|(id, v, _)| *id == t.buffer && *v == t.version) {
                    state.done = false;
                    state.since = Some(Instant::now());
                }
                return;
            }
        };
        // Switched off while the request was in flight.
        if !self.config.lsp.semantic_tokens {
            return;
        }
        let tokens = decode(&b.rope, &parse(value), &legend);
        b.semantic.set(tokens);
        b.semantic.set_fetched(t.version, lines);
        self.dirty_frame = true;
    }

    /// Apply `[lsp] semantic_tokens` (the caller has written the config).
    /// Off: the request is cancelled and every buffer's tokens go; on: the
    /// next ticks ask.
    pub(super) fn set_semantic_tokens(&mut self, on: bool) {
        self.lsp.semantic.key = None;
        if !on {
            self.semantic_cancel();
            for b in &mut self.editor.buffers {
                b.semantic.clear();
            }
            self.dirty_frame = true;
        }
    }

    /// `workspace/semanticTokens/refresh`: ask again. The old tokens stay
    /// until the new answer.
    pub(super) fn semantic_refresh(&mut self) {
        for b in &mut self.editor.buffers {
            b.semantic.forget_fetch();
        }
        self.lsp.semantic.key = None;
    }
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, LspAction, Motion};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::fake::FakeServer;

    const FULL: &str = "textDocument/semanticTokens/full";
    const RANGE: &str = "textDocument/semanticTokens/range";

    /// 60 lines: `fn main() {`, 58 `    let vN = N;` lines, `}`.
    fn source() -> String {
        let mut s = String::from("fn main() {\n");
        for n in 1..=58 {
            s.push_str(&format!("    let v{n} = {n};\n"));
        }
        s.push_str("}\n");
        s
    }

    fn draw(app: &mut App, w: u16, h: u16) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::render(app, f)).unwrap();
    }

    fn legend() -> Value {
        json!({"tokenTypes": ["function", "variable", "generic"], "tokenModifiers": ["declaration", "mutable"]})
    }

    /// `main` at line 0 col 3, `v1` at line 1 col 8 as a mutable variable, `v2` on line 2 as `generic`.
    fn data() -> Value {
        json!({"data": [0,3,4,0,0, 1,8,2,1,2, 1,8,2,2,0]})
    }

    /// a.rs open on a server (with `semanticTokensProvider` = `provider`
    /// unless that is null), drawn once so the text area is known.
    async fn setup(name: &str, provider: Value) -> (App, FakeServer, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        let mut caps = incremental();
        if !provider.is_null() {
            caps["semanticTokensProvider"] = provider;
        }
        let mut servers = with_fake_servers(&mut app, caps);
        let server = open_on_server(&mut app, &mut servers, "a.rs", &source()).await;
        draw(&mut app, 80, 24);
        (app, server, dir)
    }

    async fn setup_full(name: &str) -> (App, FakeServer, std::path::PathBuf) {
        setup(name, json!({"legend": legend(), "full": true, "range": true})).await
    }

    /// Two ticks [`SEMANTIC_DELAY`] apart: the key settles, then the request
    /// goes out if there is one to make.
    fn settle(app: &mut App) {
        let t0 = Instant::now();
        app.semantic_tick(t0);
        app.semantic_tick(t0 + SEMANTIC_DELAY);
    }

    fn tokens(app: &App) -> Vec<(Range<usize>, String)> {
        app.editor.active_buffer().unwrap().semantic.in_range(0..usize::MAX).map(|(r, n)| (r, n.to_string())).collect()
    }

    fn char_at(app: &App, line: usize, col: usize) -> usize {
        app.editor.active_buffer().unwrap().rope.line_to_char(line) + col
    }

    /// Settle, take the `/full` request and answer it with `data`.
    async fn ask_and_answer(app: &mut App, server: &mut FakeServer, data: Value) -> Value {
        settle(app);
        let req = server.recv(FULL).await;
        server.respond(req["id"].as_i64().unwrap(), data);
        handle_next(app, is_lsp).await;
        req
    }

    /// Settle, take the `/full` request and answer it with an error.
    async fn ask_and_fail(app: &mut App, server: &mut FakeServer, code: i64) {
        settle(app);
        let req = server.recv(FULL).await;
        server.respond_error(req["id"].as_i64().unwrap(), code, "no");
        handle_next(app, is_lsp).await;
    }

    async fn requests(app: &App, server: &mut FakeServer, method: &str) -> usize {
        let client = app.lsp.clients.get("rust-analyzer").unwrap();
        server.sent_so_far(client).await.iter().filter(|m| m["method"] == method).count()
    }

    #[tokio::test]
    async fn asks_full_after_the_delay_and_stores_tokens() {
        let (mut app, mut server, dir) = setup_full("sem-full").await;
        let t0 = Instant::now();
        app.semantic_tick(t0);
        app.semantic_tick(t0 + SEMANTIC_DELAY / 2);
        assert!(app.lsp.semantic.req.is_none(), "not before the delay");
        app.semantic_tick(t0 + SEMANTIC_DELAY);
        let req = server.recv(FULL).await;
        assert!(req["params"]["textDocument"].is_object() && req["params"].get("range").is_none(), "{req}");
        server.respond(req["id"].as_i64().unwrap(), data());
        handle_next(&mut app, is_lsp).await;
        let want = vec![
            (char_at(&app, 0, 3)..char_at(&app, 0, 7), "function".to_string()),
            (char_at(&app, 1, 8)..char_at(&app, 1, 10), "variable.mutable".to_string()),
        ];
        assert_eq!(tokens(&app), want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scrolling_asks_nothing_more_in_full_mode() {
        let (mut app, mut server, dir) = setup_full("sem-scroll").await;
        ask_and_answer(&mut app, &mut server, data()).await;
        for _ in 0..2 {
            app.perform(Action::Editor(EditorAction::ScrollDown(1)));
        }
        draw(&mut app, 80, 24);
        assert_eq!(app.editor.visible_lines().start, 2);
        settle(&mut app);
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn range_only_server_asks_the_visible_range() {
        let (mut app, mut server, dir) = setup("sem-range", json!({"legend": legend(), "range": true})).await;
        let rows = app.editor.visible_lines().len();
        assert!(2 * rows < 60);
        settle(&mut app);
        let req = server.recv(RANGE).await;
        assert_eq!(req["params"]["range"], json!({"start": {"line": 0, "character": 0}, "end": {"line": 2 * rows as u32, "character": 0}}));
        server.respond(req["id"].as_i64().unwrap(), data());
        handle_next(&mut app, is_lsp).await;
        assert_eq!(tokens(&app).len(), 2);
        app.perform(Action::Editor(EditorAction::Move(Motion::DocEnd, false)));
        draw(&mut app, 80, 24);
        settle(&mut app);
        server.recv(RANGE).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_request_without_the_capability_or_legend() {
        let (mut app, mut server, dir) = setup("sem-nocap", Value::Null).await;
        settle(&mut app);
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await + requests(&app, &mut server, RANGE).await, 0);
        // A registration with `full` but no legend cannot be used.
        server.request(
            9,
            "client/registerCapability",
            json!({"registrations": [{"id": "s", "method": "textDocument/semanticTokens",
                "registerOptions": {"documentSelector": [{"language": "rust"}], "full": true}}]}),
        );
        handle_next(&mut app, is_lsp).await;
        settle(&mut app);
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await + requests(&app, &mut server, RANGE).await, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_version_is_dropped() {
        let (mut app, mut server, dir) = setup_full("sem-stale").await;
        ask_and_answer(&mut app, &mut server, data()).await;
        let old = tokens(&app);
        // Edit away from the tokens so nothing moves.
        app.editor.active_buffer_mut().unwrap().goto_line(5, 0);
        app.perform(Action::Editor(EditorAction::InsertChar('x')));
        app.lsp_sync();
        settle(&mut app);
        let req = server.recv(FULL).await;
        // Edited after the request, before any tick could cancel it: the version differs.
        app.perform(Action::Editor(EditorAction::InsertChar('y')));
        app.lsp_sync();
        server.respond(req["id"].as_i64().unwrap(), json!({"data": [0, 0, 2, 1, 0]}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(tokens(&app), old);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn content_modified_and_timeout_ask_again() {
        let (mut app, mut server, dir) = setup_full("sem-retry").await;
        ask_and_fail(&mut app, &mut server, TIMED_OUT).await;
        ask_and_answer(&mut app, &mut server, data()).await;
        assert_eq!(tokens(&app).len(), 2);
        app.semantic_refresh();
        ask_and_fail(&mut app, &mut server, ResponseError::CONTENT_MODIFIED).await;
        settle(&mut app);
        server.recv(FULL).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn other_error_does_not_retry() {
        let (mut app, mut server, dir) = setup_full("sem-error").await;
        ask_and_fail(&mut app, &mut server, -32603).await;
        settle(&mut app);
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refresh_request_asks_again() {
        let (mut app, mut server, dir) = setup_full("sem-refresh").await;
        ask_and_answer(&mut app, &mut server, data()).await;
        server.request(7, "workspace/semanticTokens/refresh", json!(null));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(7).await["result"], Value::Null);
        assert_eq!(tokens(&app).len(), 2, "the old tokens stay");
        settle(&mut app);
        server.recv(FULL).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn progress_end_asks_again() {
        let (mut app, mut server, dir) = setup_full("sem-progress").await;
        ask_and_answer(&mut app, &mut server, data()).await;
        let progress = |value: Value| json!({"token": "rustAnalyzer/Indexing", "value": value});
        server.notify("$/progress", progress(json!({"kind": "begin", "title": "Indexing"})));
        handle_next(&mut app, is_lsp).await;
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await, 0, "covered, nothing asked");
        server.notify("$/progress", progress(json!({"kind": "end"})));
        handle_next(&mut app, is_lsp).await;
        settle(&mut app);
        server.recv(FULL).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn config_off_clears_and_asks_nothing_and_on_asks_again() {
        let (mut app, mut server, dir) = setup_full("sem-config").await;
        ask_and_answer(&mut app, &mut server, data()).await;
        app.config.lsp.semantic_tokens = false;
        app.set_semantic_tokens(false);
        assert!(tokens(&app).is_empty());
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await, 0);
        app.config.lsp.semantic_tokens = true;
        app.set_semantic_tokens(true);
        ask_and_answer(&mut app, &mut server, data()).await;
        assert_eq!(tokens(&app).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn answer_after_switching_off_is_dropped() {
        let (mut app, mut server, dir) = setup_full("sem-off-late").await;
        settle(&mut app);
        let req = server.recv(FULL).await;
        app.config.lsp.semantic_tokens = false;
        app.set_semantic_tokens(false);
        server.respond(req["id"].as_i64().unwrap(), data());
        handle_next(&mut app, is_lsp).await;
        assert!(tokens(&app).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn server_gone_clears_tokens() {
        let (mut app, mut server, dir) = setup_full("sem-gone").await;
        ask_and_answer(&mut app, &mut server, data()).await;
        assert!(!tokens(&app).is_empty());
        app.perform(Action::Lsp(LspAction::Restart));
        assert!(tokens(&app).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dynamic_registration_asks_without_an_edit() {
        let (mut app, mut server, dir) = setup("sem-dynreg", Value::Null).await;
        settle(&mut app);
        assert!(app.lsp.semantic.done, "the tick looked and found no capability");
        assert_eq!(requests(&app, &mut server, FULL).await, 0);
        server.request(
            9,
            "client/registerCapability",
            json!({"registrations": [{"id": "s", "method": "textDocument/semanticTokens",
                "registerOptions": {"documentSelector": [{"language": "rust"}], "legend": legend(), "full": true}}]}),
        );
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(9).await["result"], Value::Null);
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await, 1, "asked right after the registration");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unrelated_registration_asks_nothing() {
        let (mut app, mut server, dir) = setup_full("sem-otherreg").await;
        ask_and_answer(&mut app, &mut server, data()).await;
        server.request(
            9,
            "client/registerCapability",
            json!({"registrations": [{"id": "h", "method": "textDocument/hover",
                "registerOptions": {"documentSelector": [{"language": "rust"}]}}]}),
        );
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(9).await["result"], Value::Null);
        settle(&mut app);
        settle(&mut app);
        assert_eq!(requests(&app, &mut server, FULL).await, 0, "tokens already fetched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
