//! LSP document highlight: the language server's spans for the symbol at
//! the caret, drawn instead of the textual occurrence highlight with read
//! and write accesses in their own colours.
//!
//! Driven by the tick, while `[editor] highlight_occurrences` and
//! `[lsp] enabled` are on: once the active buffer, its version and the caret
//! word have stayed the same for [`DOC_HIGHLIGHT_DELAY`], one
//! `textDocument/documentHighlight` is sent at the caret, if the buffer's
//! server offers it. The request has its own [`super::lsp::Inflight`]
//! (`lsp_send` with `newest = false`); a change of the key cancels it. The
//! answer counts only for its seq and the buffer version it was sent at, and
//! only when it lists a span: an error, `null` or `[]` leaves the textual
//! highlight. It lands in `Buffer::doc_highlights`, which the view draws
//! while [`DocHighlights::holds`].

use std::ops::Range;
use std::time::{Duration, Instant};

use lsp_types::{DocumentHighlight, DocumentHighlightKind};
use serde_json::{Value, json};

use super::App;
use super::lsp::Inflight;
use super::lsp_nav::DOCUMENT_HIGHLIGHT;
use crate::editor::buffer::BufferId;
use crate::editor::search::{self, DocHighlights, HighlightKind};
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag, pos};

/// How long the caret word must stay put before the server is asked.
pub(super) const DOC_HIGHLIGHT_DELAY: Duration = Duration::from_millis(200);

/// The document highlight's debounce and its request in flight (in `LspService`).
#[derive(Debug, Default)]
pub(super) struct DocHighlightReqs {
    /// Buffer, `Buffer::version`, caret word.
    key: Option<(BufferId, u64, Option<Range<usize>>)>,
    /// When `key` last changed.
    since: Option<Instant>,
    /// The request for `key` was made (or found pointless).
    done: bool,
    /// The request in flight and the word it asks about.
    req: Option<(Inflight, Range<usize>)>,
}

impl App {
    /// The tick: restart the debounce when the key changed, else ask the
    /// server once it has held for [`DOC_HIGHLIGHT_DELAY`].
    pub(super) fn doc_highlight_tick(&mut self, now: Instant) {
        let on = self.config.editor.highlight_occurrences && self.config.lsp.enabled;
        let Some(b) = self.editor.active_buffer().filter(|_| on) else {
            self.doc_highlight_cancel();
            self.lsp.doc_highlight.key = None;
            self.doc_highlight_clear();
            return;
        };
        let id = b.id;
        let word = search::caret_word(&b.rope, &b.view.sel());
        let key = (id, b.version, word.clone());
        let state = &mut self.lsp.doc_highlight;
        if state.key.as_ref() != Some(&key) {
            self.doc_highlight_cancel();
            let state = &mut self.lsp.doc_highlight;
            state.key = Some(key);
            state.since = Some(now);
            state.done = false;
            return;
        }
        if state.done || state.since.is_none_or(|since| now < since + DOC_HIGHLIGHT_DELAY) {
            return;
        }
        let holds = b.doc_highlights.as_ref().is_some_and(|h| h.holds(&b.rope, b.version, &b.view.sel()));
        let Some(word) = word.filter(|_| !holds) else {
            state.done = true;
            return;
        };
        // Not tracked yet, or the server is still starting: try again next tick.
        let Some(caps) = self.lsp_caps(id) else { return };
        let supported = caps.has(DOCUMENT_HIGHLIGHT);
        self.lsp.doc_highlight.done = true;
        if !supported {
            self.doc_highlight_clear();
            return;
        }
        let Some(b) = self.editor.active_buffer() else { return };
        let at = pos::position(&b.rope, b.view.sel().head);
        let params = json!({"position": at});
        if let Ok(req) = self.lsp_send(id, at, DOCUMENT_HIGHLIGHT, params, PendingTag::DocumentHighlight, false) {
            self.lsp.doc_highlight.req = Some((req, word));
        }
    }

    /// Cancel the request in flight, if any.
    fn doc_highlight_cancel(&mut self) {
        if let Some((req, _)) = self.lsp.doc_highlight.req.take()
            && let Some(c) = self.lsp.clients.get(&req.server)
        {
            c.cancel(req.id);
        }
    }

    /// Drop the active buffer's stored highlights (the textual ones show).
    fn doc_highlight_clear(&mut self) {
        if let Some(b) = self.editor.active_buffer_mut()
            && b.doc_highlights.take().is_some()
        {
            self.dirty_frame = true;
        }
    }

    /// The server answered the document highlight request: stored iff it
    /// lists at least one span; an error or an odd answer clears.
    pub(super) fn doc_highlight_answer(&mut self, t: DocTag, result: Result<Value, ResponseError>) {
        let Some((_, word)) = self.lsp.doc_highlight.req.take_if(|(r, _)| r.seq == t.seq) else {
            return tracing::debug!("dropping a superseded document highlight answer");
        };
        let Some(b) = self.editor.index_of_id(t.buffer).map(|i| &mut self.editor.buffers[i]) else { return };
        if b.version != t.version {
            return tracing::debug!("dropping a stale document highlight answer");
        }
        if let Err(e) = &result {
            tracing::debug!("document highlight: {}", e.message);
        }
        let list = result.ok().and_then(|v| serde_json::from_value::<Option<Vec<DocumentHighlight>>>(v).ok().flatten()).unwrap_or_default();
        let new = (!list.is_empty()).then(|| {
            let spans = list
                .iter()
                .map(|h| {
                    let kind = match h.kind {
                        Some(DocumentHighlightKind::READ) => HighlightKind::Read,
                        Some(DocumentHighlightKind::WRITE) => HighlightKind::Write,
                        _ => HighlightKind::Text,
                    };
                    (pos::char_index(&b.rope, h.range.start)..pos::char_index(&b.rope, h.range.end), kind)
                })
                .collect();
            DocHighlights::new(t.version, word, spans)
        });
        if b.doc_highlights != new {
            b.doc_highlights = new;
            self.dirty_frame = true;
        }
    }
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::actions::{Action, EditorAction};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::editor::selection::Selection;
    use crate::lsp::fake::FakeServer;
    use crate::lsp::sync;

    const A: &str = "fn main() {\n    let mut x = 1;\n    x += 2;\n    let y = x;\n}\n";
    /// The `x` of `let mut x`.
    const X: Range<usize> = 24..25;

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    fn hl(r: Value, kind: Option<u8>) -> Value {
        match kind {
            Some(k) => json!({"range": r, "kind": k}),
            None => json!({"range": r}),
        }
    }

    /// a.rs open on a server with `provider` as its `documentHighlightProvider`
    /// (absent for `null`) and the caret on the `x` of `let mut x`.
    async fn setup(name: &str, provider: Value) -> (App, FakeServer, PathBuf) {
        let (mut app, dir) = test_app(name);
        let mut caps = incremental();
        if !provider.is_null() {
            caps["documentHighlightProvider"] = provider;
        }
        let mut servers = with_fake_servers(&mut app, caps);
        let server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 12);
        (app, server, dir)
    }

    /// Two ticks [`DOC_HIGHLIGHT_DELAY`] apart: the key settles, then the
    /// request goes out if there is one to make.
    fn settle(app: &mut App) {
        let t0 = Instant::now();
        app.doc_highlight_tick(t0);
        app.doc_highlight_tick(t0 + DOC_HIGHLIGHT_DELAY);
    }

    fn stored(app: &App) -> Option<&DocHighlights> {
        app.editor.active_buffer().unwrap().doc_highlights.as_ref()
    }

    fn version(app: &App) -> u64 {
        app.editor.active_buffer().unwrap().version
    }

    /// The server's answer for `x` as rust-analyzer gave it: unsorted.
    fn answer() -> Value {
        json!([
            hl(range(1, 12, 1, 13), Some(3)),
            hl(range(3, 12, 3, 13), Some(2)),
            hl(range(2, 4, 2, 5), Some(3)),
            hl(range(4, 0, 4, 1), None),
        ])
    }

    #[tokio::test]
    async fn asks_after_the_delay_and_keeps_the_kinds() {
        let (mut app, mut server, dir) = setup("dochl-ask", json!(true)).await;
        let t0 = Instant::now();
        app.doc_highlight_tick(t0);
        app.doc_highlight_tick(t0 + DOC_HIGHLIGHT_DELAY / 2);
        assert!(app.lsp.doc_highlight.req.is_none(), "not before the delay");
        app.doc_highlight_tick(t0 + DOC_HIGHLIGHT_DELAY);
        let req = server.recv("textDocument/documentHighlight").await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 12}));
        let a = app.workspace.root.join("a.rs");
        assert_eq!(req["params"]["textDocument"]["uri"], sync::uri(&a).unwrap().as_str());
        server.respond(req["id"].as_i64().unwrap(), answer());
        handle_next(&mut app, is_lsp).await;
        let h = stored(&app).expect("stored");
        assert_eq!(h.version, version(&app));
        assert_eq!(h.word, X);
        use HighlightKind::{Read, Text, Write};
        assert_eq!(h.spans(), &[(24..25, Write), (35..36, Write), (55..56, Read), (58..59, Text)]);
        // The same key asks once.
        app.doc_highlight_tick(t0 + DOC_HIGHLIGHT_DELAY * 4);
        assert!(app.lsp.doc_highlight.req.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_edit_cancels_and_drops_the_answer() {
        let (mut app, mut server, dir) = setup("dochl-edit", json!(true)).await;
        settle(&mut app);
        let req = server.recv("textDocument/documentHighlight").await;
        app.perform(Action::Editor(EditorAction::InsertChar('y')));
        app.doc_highlight_tick(Instant::now());
        assert_eq!(server.recv("$/cancelRequest").await["params"]["id"], req["id"]);
        server.respond(req["id"].as_i64().unwrap(), answer());
        handle_next(&mut app, is_lsp).await;
        assert!(stored(&app).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_answer_for_an_older_version_is_dropped() {
        let (mut app, mut server, dir) = setup("dochl-stale", json!(true)).await;
        settle(&mut app);
        let req = server.recv("textDocument/documentHighlight").await;
        // Typed after the request, before the next tick could cancel it.
        app.perform(Action::Editor(EditorAction::InsertChar('y')));
        server.respond(req["id"].as_i64().unwrap(), answer());
        handle_next(&mut app, is_lsp).await;
        assert!(stored(&app).is_none(), "the buffer moved on");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn another_word_asks_again() {
        let (mut app, mut server, dir) = setup("dochl-moves", json!(true)).await;
        settle(&mut app);
        let req = server.recv("textDocument/documentHighlight").await;
        server.respond(req["id"].as_i64().unwrap(), answer());
        handle_next(&mut app, is_lsp).await;
        assert!(stored(&app).is_some());
        // The `y` of `let y = x`.
        app.editor.active_buffer_mut().unwrap().goto_line(3, 8);
        let b = app.editor.active_buffer().unwrap();
        assert!(!stored(&app).unwrap().holds(&b.rope, b.version, &b.view.sel()), "the answer is for x");
        settle(&mut app);
        let req = server.recv("textDocument/documentHighlight").await;
        assert_eq!(req["params"]["position"], json!({"line": 3, "character": 8}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn nothing_or_an_error_clears() {
        let (mut app, mut server, dir) = setup("dochl-clear", json!(true)).await;
        for (n, answer_) in [Some(json!(null)), Some(json!([])), None, Some(json!("junk"))].into_iter().enumerate() {
            // Store an answer, then re-ask with a new version (a no-op edit
            // is not possible, so type and delete).
            app.perform(Action::Editor(EditorAction::InsertChar('x')));
            app.perform(Action::Editor(EditorAction::Backspace));
            settle(&mut app);
            let req = server.recv("textDocument/documentHighlight").await;
            server.respond(req["id"].as_i64().unwrap(), answer());
            handle_next(&mut app, is_lsp).await;
            assert!(stored(&app).is_some(), "stored before case {n}");
            app.perform(Action::Editor(EditorAction::InsertChar('x')));
            app.perform(Action::Editor(EditorAction::Backspace));
            settle(&mut app);
            let req = server.recv("textDocument/documentHighlight").await;
            let id = req["id"].as_i64().unwrap();
            match answer_ {
                Some(v) => server.respond(id, v),
                None => server.respond_error(id, -32603, "boom"),
            }
            handle_next(&mut app, is_lsp).await;
            assert!(stored(&app).is_none(), "case {n}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_capability_asks_nothing() {
        let (mut app, _server, dir) = setup("dochl-nocap", json!(null)).await;
        let v = version(&app);
        // Answers some other word, so it does not hold.
        app.editor.active_buffer_mut().unwrap().doc_highlights = Some(DocHighlights::new(v, 35..36, vec![(35..36, HighlightKind::Write)]));
        settle(&mut app);
        assert!(app.lsp.doc_highlight.req.is_none());
        assert!(stored(&app).is_none(), "cleared");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_word_or_a_selection_asks_nothing() {
        let (mut app, _server, dir) = setup("dochl-noword", json!(true)).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 1);
        settle(&mut app);
        assert!(app.lsp.doc_highlight.req.is_none(), "whitespace");
        app.editor.active_buffer_mut().unwrap().view.set_sel(Selection::new(24, 25));
        settle(&mut app);
        assert!(app.lsp.doc_highlight.req.is_none(), "a selection");
        app.editor.active_buffer_mut().unwrap().goto_line(1, 12);
        settle(&mut app);
        assert!(app.lsp.doc_highlight.req.is_some(), "and a word asks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn off_with_highlight_occurrences_or_lsp_off() {
        let (mut app, _server, dir) = setup("dochl-off", json!(true)).await;
        app.config.editor.highlight_occurrences = false;
        settle(&mut app);
        assert!(app.lsp.doc_highlight.req.is_none(), "highlight_occurrences off");
        app.config.editor.highlight_occurrences = true;
        app.config.lsp.enabled = false;
        settle(&mut app);
        assert!(app.lsp.doc_highlight.req.is_none(), "lsp off");
        app.config.lsp.enabled = true;
        settle(&mut app);
        assert!(app.lsp.doc_highlight.req.is_some(), "and on again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
