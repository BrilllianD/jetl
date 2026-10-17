//! LSP inlay hints: the server's types and parameter names, drawn as virtual
//! text in the focused group's active buffer.
//!
//! Driven by the tick. Once the buffer, its version and its visible lines
//! have stayed the same for [`INLAY_DELAY`], one `textDocument/inlayHint` is
//! sent for the visible lines plus one screen above and below, unless the
//! last answer (`InlayHints::fetched`) is for this version and already
//! covers the visible lines (scrolling inside the margin asks nothing). The
//! request has its own [`super::lsp::Inflight`] (`lsp_send` with
//! `newest = false`); a change of the key cancels it. An answer counts only
//! for its seq and the buffer version it was sent at. Until a newer answer
//! arrives the old hints stay, shifted by the edits (no flicker). An error
//! answer is logged; a timeout or content-modified answer for the current key
//! makes the next ticks ask again (a server still loading answers that way). `workspace/inlayHint/refresh` forgets what was
//! fetched, so the next ticks ask again. Nothing is asked while
//! `LspService::inlays_shown` is off (`[lsp] inlay_hints`, the View menu).

use std::ops::Range;
use std::time::{Duration, Instant};

use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel};
use ropey::Rope;
use serde_json::{Value, json};

use super::App;
use super::lsp::Inflight;
use crate::editor::buffer::BufferId;
use crate::editor::inlay::Inlay;
use crate::lsp::caps::INLAY_HINT;
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag, pos};

/// How long the buffer, version and visible lines must stay put before the
/// server is asked.
pub(super) const INLAY_DELAY: Duration = Duration::from_millis(300);

/// The error code jetl gives a request that got no answer in time
/// (`lsp::client`).
const TIMED_OUT: i64 = -32803;

/// The hints' debounce and its request in flight (in `LspService`).
#[derive(Debug, Default)]
pub(super) struct InlayReqs {
    /// Buffer, `Buffer::version`, visible lines.
    key: Option<(BufferId, u64, Range<usize>)>,
    /// When `key` last changed.
    since: Option<Instant>,
    /// The request for `key` was made (or found pointless).
    done: bool,
    /// The request in flight and the lines it asks for.
    req: Option<(Inflight, Range<usize>)>,
}

impl App {
    /// The tick: restart the debounce when the key changed, else ask the
    /// server once it has held for [`INLAY_DELAY`].
    pub(super) fn inlay_tick(&mut self, now: Instant) {
        let visible = self.editor.visible_lines();
        let Some(b) = self.editor.active_buffer().filter(|_| self.lsp.inlays_shown && self.config.lsp.enabled && !visible.is_empty())
        else {
            self.inlay_cancel();
            self.lsp.inlay.key = None;
            return;
        };
        let id = b.id;
        let key = (id, b.version, visible.clone());
        let state = &mut self.lsp.inlay;
        if state.key.as_ref() != Some(&key) {
            self.inlay_cancel();
            let state = &mut self.lsp.inlay;
            state.key = Some(key);
            state.since = Some(now);
            state.done = false;
            return;
        }
        if state.done || state.since.is_none_or(|since| now < since + INLAY_DELAY) {
            return;
        }
        if b.inlays.fetched().is_some_and(|(v, r)| v == b.version && r.start <= visible.start && visible.end <= r.end) {
            state.done = true;
            return;
        }
        // Not tracked yet, or the server is still starting: try again next tick.
        let Some(caps) = self.lsp_caps(id) else { return };
        let supported = caps.has(INLAY_HINT);
        let state = &mut self.lsp.inlay;
        state.done = true;
        if !supported {
            return;
        }
        let rope = &b.rope;
        let h = visible.len();
        let lo = visible.start.saturating_sub(h);
        let hi = (visible.end + h).min(rope.len_lines());
        let start = pos::position(rope, rope.line_to_char(lo));
        let end = pos::position(rope, if hi < rope.len_lines() { rope.line_to_char(hi) } else { rope.len_chars() });
        let params = json!({"range": {"start": start, "end": end}});
        if let Ok(req) = self.lsp_send(id, start, INLAY_HINT, params, PendingTag::InlayHints, false) {
            self.lsp.inlay.req = Some((req, lo..hi));
        }
    }

    /// Cancel the request in flight, if any.
    fn inlay_cancel(&mut self) {
        if let Some((req, _)) = self.lsp.inlay.req.take()
            && let Some(c) = self.lsp.clients.get(&req.server)
        {
            c.cancel(req.id);
        }
    }

    /// The server answered the hints request: replace the buffer's hints
    /// when it is still at the version asked about.
    pub(super) fn inlay_answer(&mut self, t: DocTag, result: Result<Value, ResponseError>) {
        let Some((_, lines)) = self.lsp.inlay.req.take_if(|(r, _)| r.seq == t.seq) else {
            return tracing::debug!("dropping a superseded inlay hints answer");
        };
        let shown = self.lsp.inlays_shown;
        let Some(b) = self.editor.index_of_id(t.buffer).map(|i| &mut self.editor.buffers[i]) else { return };
        if b.version != t.version {
            return tracing::debug!("dropping a stale inlay hints answer");
        }
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("inlay hints: {e}");
                // Still the current key and the server only was not ready (timeout,
                // content modified): ask again once the delay has passed.
                let state = &mut self.lsp.inlay;
                let retry = [TIMED_OUT, ResponseError::CONTENT_MODIFIED].contains(&e.code);
                if retry && state.key.as_ref().is_some_and(|(id, v, _)| *id == t.buffer && *v == t.version) {
                    state.done = false;
                    state.since = Some(Instant::now());
                }
                return;
            }
        };
        // Switched off while the request was in flight.
        if !shown {
            return;
        }
        b.inlays.set(parse_hints(&b.rope, value), t.version, lines);
        self.dirty_frame = true;
    }

    /// Switch hints on or off for the session. Off: the request is
    /// cancelled and every buffer's hints go; on: the next ticks ask.
    pub(super) fn set_inlays_shown(&mut self, on: bool) {
        self.lsp.inlays_shown = on;
        self.lsp.inlay.key = None;
        if !on {
            self.inlay_cancel();
            for b in &mut self.editor.buffers {
                b.inlays.clear();
            }
            self.dirty_frame = true;
        }
    }

    /// View > Inlay Hints: flip the session state and say which way.
    pub(super) fn toggle_inlay_hints(&mut self) {
        let on = !self.lsp.inlays_shown;
        self.set_inlays_shown(on);
        self.lsp_hint(if on { "Inlay hints: on" } else { "Inlay hints: off" });
    }

    /// `workspace/inlayHint/refresh`: ask again for what is on screen. The
    /// old hints stay until the new answer.
    pub(super) fn inlay_refresh(&mut self) {
        for b in &mut self.editor.buffers {
            b.inlays.forget_fetch();
        }
        self.lsp.inlay.key = None;
    }
}

/// The hints of an `InlayHint[] | null` answer, in `rope`'s char offsets.
/// An item that does not parse is skipped, not the rest.
fn parse_hints(rope: &Rope, v: Value) -> Vec<Inlay> {
    let items = serde_json::from_value::<Option<Vec<Value>>>(v).ok().flatten().unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| serde_json::from_value::<InlayHint>(item).ok())
        .filter_map(|h| {
            let label = match &h.label {
                InlayHintLabel::String(s) => s.clone(),
                InlayHintLabel::LabelParts(parts) => parts.iter().map(|p| p.value.as_str()).collect(),
            };
            let param = h.kind == Some(InlayHintKind::PARAMETER);
            Inlay::new(pos::char_index(rope, h.position), &label, h.padding_left == Some(true), h.padding_right == Some(true), param)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, LspAction, Motion};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::fake::FakeServer;

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

    /// a.rs open on a server (with `inlayHintProvider` unless `caps` says
    /// otherwise), drawn once so the text area is known.
    async fn setup(name: &str, provider: bool) -> (App, FakeServer, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        let mut caps = incremental();
        if provider {
            caps["inlayHintProvider"] = json!(true);
        }
        let mut servers = with_fake_servers(&mut app, caps);
        let server = open_on_server(&mut app, &mut servers, "a.rs", &source()).await;
        draw(&mut app, 80, 24);
        (app, server, dir)
    }

    /// Two ticks [`INLAY_DELAY`] apart: the key settles, then the request
    /// goes out if there is one to make.
    fn settle(app: &mut App) {
        let t0 = Instant::now();
        app.inlay_tick(t0);
        app.inlay_tick(t0 + INLAY_DELAY);
    }

    fn pos_json(line: u32, character: u32) -> Value {
        json!({"line": line, "character": character})
    }

    fn hint(line: u32, character: u32, label: Value, kind: u8) -> Value {
        json!({"position": pos_json(line, character), "label": label, "kind": kind})
    }

    fn all(app: &App) -> Vec<Inlay> {
        app.editor.active_buffer().unwrap().inlays.all().to_vec()
    }

    fn char_at(app: &App, line: usize, col: usize) -> usize {
        app.editor.active_buffer().unwrap().rope.line_to_char(line) + col
    }

    /// Settle, take the request and answer it with `hints`.
    async fn ask_and_answer(app: &mut App, server: &mut FakeServer, hints: Value) -> Value {
        settle(app);
        let req = server.recv("textDocument/inlayHint").await;
        server.respond(req["id"].as_i64().unwrap(), hints);
        handle_next(app, is_lsp).await;
        req
    }

    #[tokio::test]
    async fn asks_for_the_visible_range_after_the_delay() {
        let (mut app, mut server, dir) = setup("inlay-ask", true).await;
        let visible = app.editor.visible_lines();
        let rows = visible.len();
        assert!(visible.start == 0 && 2 * rows < 60, "{visible:?}");
        let t0 = Instant::now();
        app.inlay_tick(t0);
        app.inlay_tick(t0 + INLAY_DELAY / 2);
        assert!(app.lsp.inlay.req.is_none(), "not before the delay");
        app.inlay_tick(t0 + INLAY_DELAY);
        let req = server.recv("textDocument/inlayHint").await;
        assert_eq!(req["params"]["range"], json!({"start": pos_json(0, 0), "end": pos_json(2 * rows as u32, 0)}));
        let hints = json!([
            hint(1, 9, json!(": i32"), 1),
            {"position": pos_json(2, 13), "label": [{"value": "a"}, {"value": ":"}], "kind": 2, "paddingRight": true},
        ]);
        server.respond(req["id"].as_i64().unwrap(), hints);
        handle_next(&mut app, is_lsp).await;
        let got = all(&app);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].at, got[0].label.as_str(), got[0].param), (char_at(&app, 1, 9), ": i32", false));
        assert_eq!((got[1].at, got[1].label.as_str(), got[1].param, got[1].pad_right), (char_at(&app, 2, 13), "a:", true, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_request_without_the_capability_or_when_off() {
        let (mut app, mut server, dir) = setup("inlay-nocap", false).await;
        settle(&mut app);
        settle(&mut app);
        let client = app.lsp.clients.get("rust-analyzer").unwrap();
        assert!(server.sent_so_far(client).await.iter().all(|m| m["method"] != "textDocument/inlayHint"));
        let _ = std::fs::remove_dir_all(&dir);

        let (mut app, mut server, dir) = setup("inlay-off", true).await;
        app.set_inlays_shown(false);
        settle(&mut app);
        let client = app.lsp.clients.get("rust-analyzer").unwrap();
        assert!(server.sent_so_far(client).await.iter().all(|m| m["method"] != "textDocument/inlayHint"));
        app.set_inlays_shown(true);
        settle(&mut app);
        server.recv("textDocument/inlayHint").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edits_shift_hints_until_the_new_answer() {
        let (mut app, mut server, dir) = setup("inlay-shift", true).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        let before = all(&app)[0].at;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 0);
        for c in ['x', 'x'] {
            app.perform(Action::Editor(EditorAction::InsertChar(c)));
        }
        app.lsp_sync();
        assert_eq!(all(&app)[0].at, before + 2);
        app.inlay_tick(Instant::now());
        assert!(!app.editor.active_buffer().unwrap().inlays.is_empty(), "no flicker");
        assert_eq!(all(&app)[0].at, before + 2);
        settle(&mut app);
        server.recv("textDocument/inlayHint").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_answer_is_dropped_and_old_hints_stay() {
        let (mut app, mut server, dir) = setup("inlay-stale", true).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        let old = all(&app);
        // Edit away from line 1 so the hint does not move.
        app.editor.active_buffer_mut().unwrap().goto_line(5, 0);
        app.perform(Action::Editor(EditorAction::InsertChar('x')));
        app.lsp_sync();
        settle(&mut app);
        let b = server.recv("textDocument/inlayHint").await;
        app.perform(Action::Editor(EditorAction::InsertChar('y')));
        app.lsp_sync();
        app.inlay_tick(Instant::now());
        let client = app.lsp.clients.get("rust-analyzer").unwrap();
        let sent = server.sent_so_far(client).await;
        assert!(sent.iter().any(|m| m["method"] == "$/cancelRequest" && m["params"]["id"] == b["id"]), "{sent:?}");
        server.respond(b["id"].as_i64().unwrap(), json!([hint(3, 4, json!(": u8"), 1)]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(all(&app), old, "the cancelled request is superseded");

        // Edited after the request, before any tick could cancel it: the version differs.
        settle(&mut app);
        let c = server.recv("textDocument/inlayHint").await;
        app.perform(Action::Editor(EditorAction::InsertChar('z')));
        app.lsp_sync();
        let kept = all(&app);
        server.respond(c["id"].as_i64().unwrap(), json!([hint(3, 4, json!(": u8"), 1)]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(all(&app), kept, "the stale version is dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scroll_within_fetched_range_asks_nothing() {
        let (mut app, mut server, dir) = setup("inlay-scroll", true).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        for _ in 0..2 {
            app.perform(Action::Editor(EditorAction::ScrollDown(1)));
        }
        draw(&mut app, 80, 24);
        assert_eq!(app.editor.visible_lines().start, 2);
        settle(&mut app);
        let client = app.lsp.clients.get("rust-analyzer").unwrap();
        assert!(server.sent_so_far(client).await.iter().all(|m| m["method"] != "textDocument/inlayHint"));
        app.perform(Action::Editor(EditorAction::Move(Motion::DocEnd, false)));
        draw(&mut app, 80, 24);
        settle(&mut app);
        server.recv("textDocument/inlayHint").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refresh_request_refetches() {
        let (mut app, mut server, dir) = setup("inlay-refresh", true).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        server.request(7, "workspace/inlayHint/refresh", json!(null));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(7).await["result"], Value::Null);
        assert_eq!(all(&app).len(), 1, "the old hints stay");
        settle(&mut app);
        server.recv("textDocument/inlayHint").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    /// Settle, take the request and answer it with an error.
    async fn ask_and_fail(app: &mut App, server: &mut FakeServer, code: i64) {
        settle(app);
        let req = server.recv("textDocument/inlayHint").await;
        server.respond_error(req["id"].as_i64().unwrap(), code, "no");
        handle_next(app, is_lsp).await;
    }

    async fn inlay_requests(app: &App, server: &mut FakeServer) -> usize {
        let client = app.lsp.clients.get("rust-analyzer").unwrap();
        server.sent_so_far(client).await.iter().filter(|m| m["method"] == "textDocument/inlayHint").count()
    }

    #[tokio::test]
    async fn timeout_answer_asks_again_on_the_next_settle() {
        let (mut app, mut server, dir) = setup("inlay-timeout", true).await;
        ask_and_fail(&mut app, &mut server, TIMED_OUT).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        assert_eq!(all(&app).len(), 1);
        app.inlay_refresh();
        ask_and_fail(&mut app, &mut server, ResponseError::CONTENT_MODIFIED).await;
        settle(&mut app);
        server.recv("textDocument/inlayHint").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn other_error_does_not_retry() {
        let (mut app, mut server, dir) = setup("inlay-error", true).await;
        ask_and_fail(&mut app, &mut server, -32603).await;
        settle(&mut app);
        settle(&mut app);
        assert_eq!(inlay_requests(&app, &mut server).await, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_key_error_does_not_retry() {
        let (mut app, mut server, dir) = setup("inlay-error-stale", true).await;
        settle(&mut app);
        let req = server.recv("textDocument/inlayHint").await;
        // The key moves on (an edit) before the timeout answer arrives.
        app.editor.active_buffer_mut().unwrap().goto_line(5, 0);
        app.perform(Action::Editor(EditorAction::InsertChar('x')));
        app.lsp_sync();
        let t0 = Instant::now();
        app.inlay_tick(t0);
        server.respond_error(req["id"].as_i64().unwrap(), TIMED_OUT, "late");
        handle_next(&mut app, is_lsp).await;
        // The new key still waits for its delay: nothing extra goes out.
        app.inlay_tick(t0 + INLAY_DELAY / 2);
        assert_eq!(inlay_requests(&app, &mut server).await, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn progress_end_refreshes_hints() {
        let (mut app, mut server, dir) = setup("inlay-progress", true).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        let progress = |value: Value| json!({"token": "rustAnalyzer/Indexing", "value": value});
        server.notify("$/progress", progress(json!({"kind": "begin", "title": "Indexing"})));
        handle_next(&mut app, is_lsp).await;
        settle(&mut app);
        assert_eq!(inlay_requests(&app, &mut server).await, 0, "covered, nothing asked");
        server.notify("$/progress", progress(json!({"kind": "end"})));
        handle_next(&mut app, is_lsp).await;
        settle(&mut app);
        server.recv("textDocument/inlayHint").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn toggle_action_hides_and_brings_back_hints() {
        let (mut app, mut server, dir) = setup("inlay-toggle", true).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        assert_eq!(all(&app).len(), 1);
        app.perform(Action::Lsp(LspAction::ToggleInlayHints));
        assert!(all(&app).is_empty());
        assert!(!app.lsp.inlays_shown);
        assert!(toasts(&app).iter().any(|t| t == "Inlay hints: off"), "{:?}", toasts(&app));
        settle(&mut app);
        let client = app.lsp.clients.get("rust-analyzer").unwrap();
        assert!(server.sent_so_far(client).await.iter().all(|m| m["method"] != "textDocument/inlayHint"));
        app.perform(Action::Lsp(LspAction::ToggleInlayHints));
        assert!(toasts(&app).iter().any(|t| t == "Inlay hints: on"), "{:?}", toasts(&app));
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        assert_eq!(all(&app).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn server_gone_clears_hints() {
        let (mut app, mut server, dir) = setup("inlay-gone", true).await;
        ask_and_answer(&mut app, &mut server, json!([hint(1, 9, json!(": i32"), 1)])).await;
        assert!(!app.editor.active_buffer().unwrap().inlays.is_empty());
        app.perform(Action::Lsp(LspAction::Restart));
        assert!(app.editor.active_buffer().unwrap().inlays.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_from_parts_and_padding() {
        let rope = Rope::from_str("let a = 1;\nlet b = 2;\n");
        let v = json!([
            {"position": pos_json(0, 4), "label": [{"value": "x"}, {"value": ": "}, {"value": "T"}], "paddingLeft": true, "paddingRight": true},
            {"position": pos_json(1, 99), "label": "end", "kind": 2},
            {"position": pos_json(0, 1), "label": 5},
            {"position": pos_json(0, 2), "label": "ok", "kind": 1},
        ]);
        let got = parse_hints(&rope, v);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!((got[0].at, got[0].label.as_str(), got[0].pad_left, got[0].pad_right, got[0].param), (4, "x: T", true, true, false));
        assert_eq!((got[1].at, got[1].label.as_str(), got[1].param), (11 + 10, "end", true));
        assert_eq!((got[2].at, got[2].label.as_str()), (2, "ok"));
        assert!(parse_hints(&rope, json!({"not": "an array"})).is_empty());
        assert!(parse_hints(&rope, Value::Null).is_empty());
    }
}
