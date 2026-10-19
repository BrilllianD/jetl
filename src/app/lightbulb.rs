//! The lightbulb: a `✱` in the gutter of the caret line when the language
//! server has quick fixes for a diagnostic there (Alt+Enter lists them).
//!
//! Driven by the tick, while the editor has focus: once the active buffer,
//! its version, the caret line and the diagnostics store have stayed the
//! same for [`LIGHTBULB_DELAY`], one `textDocument/codeAction` (`only: ["quickfix"]`, automatic) is sent
//! for the line, and only when a diagnostic overlaps it. The request has
//! its own [`super::lsp::Inflight`] (`lsp_send` with `newest = false`), so
//! it and Alt+Enter never cancel each other; a change of the key cancels
//! it. The answer counts only for its seq and the buffer version it was
//! sent at, and lands in `Buffer::lightbulb`, which the view draws while
//! the buffer is still at that version with the caret on that line.

use std::time::{Duration, Instant};

use lsp_types::CodeActionOrCommand;
use serde_json::{Value, json};

use super::App;
use super::lsp::Inflight;
use super::lsp_nav::CODE_ACTION;
use crate::editor::buffer::BufferId;
use crate::editor::selection::line_len_no_eol;
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag, pos};
use crate::ui::focus::PanelId;

/// How long the caret line must stay put before the server is asked.
pub(super) const LIGHTBULB_DELAY: Duration = Duration::from_millis(250);

/// The lightbulb's debounce and its request in flight (in `LspService`).
#[derive(Debug, Default)]
pub(super) struct Lightbulb {
    /// Buffer, `Buffer::version`, caret line, `DiagnosticStore::version`.
    key: Option<(BufferId, u64, usize, u64)>,
    /// When `key` last changed.
    since: Option<Instant>,
    /// The request for `key` was made (or found pointless).
    done: bool,
    /// The request in flight and the line it asks about.
    req: Option<(Inflight, usize)>,
}

/// Whether `a` is an enabled quick fix (kind `quickfix` or `quickfix.*`).
fn is_quickfix(a: &CodeActionOrCommand) -> bool {
    match a {
        CodeActionOrCommand::CodeAction(c) => {
            c.disabled.is_none() && c.kind.as_ref().is_some_and(|k| k.as_str() == "quickfix" || k.as_str().starts_with("quickfix."))
        }
        CodeActionOrCommand::Command(_) => false,
    }
}

impl App {
    /// The tick: restart the debounce when the key changed, else ask the
    /// server once it has held for [`LIGHTBULB_DELAY`].
    /// Nothing is asked while the editor is not focused (the view draws
    /// the bulb only then); focusing it again restarts the debounce.
    pub(super) fn lightbulb_tick(&mut self, now: Instant) {
        let focused = self.focus.is(PanelId::Editor);
        let Some(b) = self.editor.active_buffer().filter(|_| focused) else {
            self.lightbulb_cancel();
            self.lsp.lightbulb.key = None;
            return;
        };
        let line = b.cursor_line_col().0;
        let key = (b.id, b.version, line, self.lsp.diagnostics.version());
        let state = &mut self.lsp.lightbulb;
        if state.key != Some(key) {
            self.lightbulb_cancel();
            let state = &mut self.lsp.lightbulb;
            state.key = Some(key);
            state.since = Some(now);
            state.done = false;
            return;
        }
        if state.done || state.since.is_none_or(|since| now < since + LIGHTBULB_DELAY) {
            return;
        }
        state.done = true;
        let start = b.rope.line_to_char(line);
        let range = lsp_types::Range::new(pos::position(&b.rope, start), pos::position(&b.rope, start + line_len_no_eol(&b.rope, line)));
        let diags = self.diagnostics_overlapping(b, range);
        let id = b.id;
        let quickfixes = |k: Vec<String>| k.iter().any(|k| k == "quickfix" || k.starts_with("quickfix."));
        let asks =
            !diags.is_empty() && self.lsp_caps(id).is_some_and(|c| c.has(CODE_ACTION) && c.code_action_kinds().is_none_or(quickfixes));
        if !asks {
            if let Some(b) = self.editor.active_buffer_mut()
                && b.lightbulb.take().is_some()
            {
                self.dirty_frame = true;
            }
            return;
        }
        let params = json!({"range": range, "context": {"diagnostics": diags, "only": ["quickfix"], "triggerKind": 2}});
        if let Ok(req) = self.lsp_send(id, range.start, CODE_ACTION, params, PendingTag::Lightbulb, false) {
            self.lsp.lightbulb.req = Some((req, line));
        }
    }

    /// Cancel the request in flight, if any.
    fn lightbulb_cancel(&mut self) {
        if let Some((req, _)) = self.lsp.lightbulb.req.take()
            && let Some(c) = self.lsp.clients.get(&req.server)
        {
            c.cancel(req.id);
        }
    }

    /// The server answered the lightbulb request: shown iff it offers an
    /// enabled quick fix; an error or an odd answer hides it.
    pub(super) fn lightbulb_answer(&mut self, t: DocTag, result: Result<Value, ResponseError>) {
        let Some((_, line)) = self.lsp.lightbulb.req.take_if(|(r, _)| r.seq == t.seq) else {
            return tracing::debug!("dropping a superseded lightbulb answer");
        };
        let Some(b) = self.editor.index_of_id(t.buffer).map(|i| &mut self.editor.buffers[i]) else { return };
        if b.version != t.version {
            return tracing::debug!("dropping a stale lightbulb answer");
        }
        if let Err(e) = &result {
            tracing::debug!("lightbulb: {}", e.message);
        }
        let fix = result
            .ok()
            .and_then(|v| serde_json::from_value::<Option<Vec<CodeActionOrCommand>>>(v).ok().flatten())
            .is_some_and(|v| v.iter().any(is_quickfix));
        let new = fix.then_some((t.version, line));
        if b.lightbulb != new {
            b.lightbulb = new;
            self.dirty_frame = true;
        }
    }
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::actions::{Action, EditorAction, LspAction};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::fake::FakeServer;
    use crate::lsp::sync;
    use crate::ui::dialog::DialogKind;

    const A: &str = "fn main() {\n    let x: u8 = \"one\";\n    unused();\n}\n";

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    fn uri(path: &Path) -> String {
        sync::uri(path).unwrap().as_str().to_string()
    }

    fn fix(title: &str, kind: &str) -> Value {
        json!({"title": title, "kind": kind, "edit": {"changes": {}}})
    }

    /// a.rs open on a server with `code_action` as its `codeActionProvider`,
    /// an error published on line 1 and the caret there.
    async fn setup(name: &str, code_action: Value) -> (App, FakeServer, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        let mut caps = incremental();
        caps["codeActionProvider"] = code_action;
        let mut servers = with_fake_servers(&mut app, caps);
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        publish(&mut app, &mut server, 1).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        (app, server, dir)
    }

    async fn publish(app: &mut App, server: &mut FakeServer, line: u32) {
        let a = app.workspace.root.join("a.rs");
        let d = json!({"range": range(line, 16, line, 21), "severity": 1, "message": "mismatched types"});
        server.notify("textDocument/publishDiagnostics", json!({"uri": uri(&a), "diagnostics": [d]}));
        handle_next(app, is_lsp).await;
    }

    /// Two ticks [`LIGHTBULB_DELAY`] apart: the key settles, then the
    /// request goes out if there is one to make.
    fn settle(app: &mut App) {
        let t0 = Instant::now();
        app.lightbulb_tick(t0);
        app.lightbulb_tick(t0 + LIGHTBULB_DELAY);
    }

    /// The lightbulb requests the server got before an Alt+Enter request
    /// (sent as a marker, left unanswered).
    async fn bulb_requests(app: &mut App, server: &mut FakeServer) -> Vec<Value> {
        app.perform(Action::Lsp(LspAction::ContextActions));
        let mut out = Vec::new();
        loop {
            let m = server.next().await;
            if m["method"] == "textDocument/codeAction" {
                if m["params"]["context"]["triggerKind"] == 1 {
                    return out;
                }
                out.push(m);
            }
        }
    }

    fn bulb(app: &App) -> Option<(u64, usize)> {
        app.editor.active_buffer().unwrap().lightbulb
    }

    fn version(app: &App) -> u64 {
        app.editor.active_buffer().unwrap().version
    }

    #[tokio::test]
    async fn quick_fixes_on_the_caret_line_show_the_bulb() {
        let (mut app, mut server, dir) = setup("lightbulb-show", json!(true)).await;
        let t0 = Instant::now();
        app.lightbulb_tick(t0);
        app.lightbulb_tick(t0 + LIGHTBULB_DELAY / 2);
        assert!(app.lsp.lightbulb.req.is_none(), "not before the delay");
        app.lightbulb_tick(t0 + LIGHTBULB_DELAY);
        let req = server.recv("textDocument/codeAction").await;
        assert_eq!(req["params"]["range"], range(1, 0, 1, 22));
        assert_eq!(req["params"]["context"]["only"], json!(["quickfix"]));
        assert_eq!(req["params"]["context"]["triggerKind"], 2);
        assert_eq!(req["params"]["context"]["diagnostics"].as_array().unwrap().len(), 1);
        server.respond(req["id"].as_i64().unwrap(), json!([fix("Fix", "quickfix")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(bulb(&app), Some((version(&app), 1)));
        // The same key asks once.
        app.lightbulb_tick(t0 + LIGHTBULB_DELAY * 4);
        assert!(bulb_requests(&mut app, &mut server).await.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn only_enabled_quick_fixes_count() {
        let (mut app, mut server, dir) = setup("lightbulb-kinds", json!(true)).await;
        let disabled = json!({"title": "Off", "kind": "quickfix", "disabled": {"reason": "no"}});
        let command = json!({"title": "Cmd", "command": "fix"});
        for answer in [json!([fix("R", "refactor")]), json!([disabled]), json!([command]), json!(null)] {
            // A new diagnostics version re-asks.
            publish(&mut app, &mut server, 1).await;
            settle(&mut app);
            let req = server.recv("textDocument/codeAction").await;
            server.respond(req["id"].as_i64().unwrap(), answer.clone());
            handle_next(&mut app, is_lsp).await;
            assert_eq!(bulb(&app), None, "{answer}");
        }
        // A sub-kind of quickfix counts; an error hides it again.
        publish(&mut app, &mut server, 1).await;
        settle(&mut app);
        let req = server.recv("textDocument/codeAction").await;
        server.respond(req["id"].as_i64().unwrap(), json!([fix("Fix", "quickfix.import")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(bulb(&app), Some((version(&app), 1)));
        publish(&mut app, &mut server, 1).await;
        settle(&mut app);
        let req = server.recv("textDocument/codeAction").await;
        server.respond_error(req["id"].as_i64().unwrap(), -32603, "boom");
        handle_next(&mut app, is_lsp).await;
        assert_eq!(bulb(&app), None);
        assert!(app.toasts.items().iter().all(|t| !t.text.contains("boom")), "errors are silent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_request_without_a_diagnostic_or_quick_fixes() {
        let (mut app, mut server, dir) = setup("lightbulb-none", json!(true)).await;
        app.editor.active_buffer_mut().unwrap().goto_line(2, 0);
        settle(&mut app);
        assert!(bulb_requests(&mut app, &mut server).await.is_empty(), "no diagnostic on line 2");
        let _ = std::fs::remove_dir_all(&dir);

        let (mut app, mut server, dir) = setup("lightbulb-refactor-only", json!({"codeActionKinds": ["refactor"]})).await;
        settle(&mut app);
        assert!(bulb_requests(&mut app, &mut server).await.is_empty(), "the server has no quick fixes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edits_and_caret_moves_make_the_answer_stale() {
        let (mut app, mut server, dir) = setup("lightbulb-stale", json!(true)).await;
        settle(&mut app);
        let req = server.recv("textDocument/codeAction").await;
        app.perform(Action::Editor(EditorAction::InsertText("y".into())));
        app.lsp_sync();
        server.respond(req["id"].as_i64().unwrap(), json!([fix("Fix", "quickfix")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(bulb(&app), None, "typed after the request");

        // Moving off the line before the delay: nothing is sent.
        let t0 = Instant::now();
        app.lightbulb_tick(t0);
        app.editor.active_buffer_mut().unwrap().goto_line(2, 0);
        app.lightbulb_tick(t0 + LIGHTBULB_DELAY);
        assert!(bulb_requests(&mut app, &mut server).await.is_empty());

        // Moving off the line after it was sent cancels it.
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        settle(&mut app);
        let req = server.recv("textDocument/codeAction").await;
        app.editor.active_buffer_mut().unwrap().goto_line(2, 0);
        app.lightbulb_tick(Instant::now());
        assert_eq!(server.recv("$/cancelRequest").await["params"]["id"], req["id"]);

        // Focus elsewhere (the bulb is not drawn): nothing is asked; so is
        // a request in flight when the focus leaves.
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        app.focus.set(PanelId::Project);
        settle(&mut app);
        app.focus.set(PanelId::Editor);
        assert!(bulb_requests(&mut app, &mut server).await.is_empty(), "not while unfocused");
        app.editor.active_buffer_mut().unwrap().goto_line(1, 5);
        settle(&mut app);
        let req = server.recv("textDocument/codeAction").await;
        app.focus.set(PanelId::Project);
        app.lightbulb_tick(Instant::now());
        assert_eq!(server.recv("$/cancelRequest").await["params"]["id"], req["id"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_bulb_and_context_actions_do_not_cancel_each_other() {
        let (mut app, mut server, dir) = setup("lightbulb-alt-enter", json!(true)).await;
        settle(&mut app);
        let bulb_req = server.recv("textDocument/codeAction").await;
        app.perform(Action::Lsp(LspAction::ContextActions));
        let mut actions_req = None;
        while actions_req.is_none() {
            let m = server.next().await;
            assert!(!(m["method"] == "$/cancelRequest" && m["params"]["id"] == bulb_req["id"]), "the bulb's request was cancelled");
            if m["method"] == "textDocument/codeAction" {
                actions_req = Some(m);
            }
        }
        let actions_req = actions_req.unwrap();
        server.respond(bulb_req["id"].as_i64().unwrap(), json!([fix("Fix", "quickfix")]));
        handle_next(&mut app, is_lsp).await;
        server.respond(actions_req["id"].as_i64().unwrap(), json!([fix("Fix", "quickfix")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(bulb(&app), Some((version(&app), 1)));
        match &app.dialog.as_ref().expect("the actions").kind {
            DialogKind::Choice { items, .. } => assert_eq!(items, &["Fix"]),
            k => panic!("{k:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn republished_diagnostics_ask_again() {
        let (mut app, mut server, dir) = setup("lightbulb-republish", json!(true)).await;
        settle(&mut app);
        let req = server.recv("textDocument/codeAction").await;
        server.respond(req["id"].as_i64().unwrap(), json!([]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(bulb(&app), None);
        publish(&mut app, &mut server, 1).await;
        settle(&mut app);
        let req = server.recv("textDocument/codeAction").await;
        server.respond(req["id"].as_i64().unwrap(), json!([fix("Fix", "quickfix")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(bulb(&app), Some((version(&app), 1)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
