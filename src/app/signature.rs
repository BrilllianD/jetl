//! Parameter Info: signature help at the caret (`ui/signature.rs` draws it).
//! Ctrl+P (`lsp.parameter_info`) asks `textDocument/signatureHelp`
//! (triggerKind 1); typing one of the server's trigger characters asks by
//! itself (triggerKind 2). While the popup is open every editor action asks
//! again (`isRetrigger`, triggerKind 2 for a trigger or retrigger character,
//! else 3, with `activeSignatureHelp`) and a null or empty answer closes it.
//! Esc closes it; other keys go on. It closes when the editor loses focus,
//! an overlay opens, or the focused group or active buffer changes. Only an
//! explicit Ctrl+P (`LspService::signature_explicit`) tells the user when
//! there is nothing to show. Several carets ask nothing and close it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use lsp_types::SignatureHelp;
use serde_json::{Value, json};

use super::App;
use crate::dispatch::Mode;
use crate::lsp::caps::SIGNATURE_HELP;
use crate::lsp::{DocTag, PendingTag};
use crate::ui::focus::PanelId;
use crate::ui::signature::SignaturePopup;

/// `SignatureHelpTriggerKind`.
const INVOKED: u8 = 1;
const TRIGGER_CHARACTER: u8 = 2;
const CONTENT_CHANGE: u8 = 3;

impl App {
    /// `lsp.parameter_info` (Ctrl+P).
    pub(super) fn lsp_parameter_info(&mut self) {
        if !self.focus.is(PanelId::Editor) || self.editor.find_focused() {
            return;
        }
        self.signature_request(INVOKED, None, true);
    }

    /// Ask at the caret; `trigger` is the character typed.
    fn signature_request(&mut self, kind: u8, trigger: Option<char>, explicit: bool) {
        let Some((id, pos)) = self.caret_position() else { return };
        let mut context = json!({"triggerKind": kind, "isRetrigger": self.signature.is_some()});
        if let Some(c) = trigger {
            context["triggerCharacter"] = json!(c.to_string());
        }
        if let Some(p) = &self.signature {
            context["activeSignatureHelp"] = serde_json::to_value(&p.help).unwrap_or(Value::Null);
        }
        match self.lsp_doc_request(id, pos, SIGNATURE_HELP, json!({"context": context}), PendingTag::SignatureHelp) {
            Ok(_) => self.lsp.signature_explicit = explicit,
            Err(why) if explicit && !why.is_empty() => self.lsp_hint(why),
            Err(_) => {}
        }
    }

    /// After an editor action (`typed`: the character inserted): ask when a
    /// trigger character was typed, and again while the popup is open.
    pub(super) fn signature_after_edit(&mut self, typed: Option<char>) {
        if !self.focus.is(PanelId::Editor) || self.editor.find_focused() || self.mode != Mode::Insert {
            return;
        }
        let Some(b) = self.editor.active_buffer() else { return };
        if b.view.sels.is_multi() {
            return self.signature_close();
        }
        let Some(caps) = self.lsp_caps(b.id) else { return };
        if !caps.has(SIGNATURE_HELP) {
            return;
        }
        let is_in = |list: Vec<String>, c: char| list.iter().any(|t| t.ends_with(c));
        let trigger = typed.filter(|&c| is_in(caps.signature_triggers(), c));
        let retrigger = typed.filter(|&c| is_in(caps.signature_retriggers(), c));
        if let Some(c) = trigger {
            self.signature_request(TRIGGER_CHARACTER, Some(c), false);
        } else if self.signature.is_some() {
            match retrigger {
                Some(c) => self.signature_request(TRIGGER_CHARACTER, Some(c), false),
                None => self.signature_request(CONTENT_CHANGE, None, false),
            }
        }
    }

    /// The answer: the popup replaces the old one, an empty answer closes it.
    pub(super) fn signature_answer(&mut self, t: DocTag, value: Value) {
        let help = serde_json::from_value::<Option<SignatureHelp>>(value).ok().flatten();
        match help.and_then(|h| SignaturePopup::new(self.editor.focused_group().id, t.buffer, h)) {
            Some(p) => self.signature = Some(p),
            None => {
                self.signature = None;
                if self.lsp.signature_explicit {
                    self.lsp_hint("No parameter info");
                }
            }
        }
        self.dirty_frame = true;
    }

    /// Close the popup when an overlay is up, the editor is not focused, or
    /// the focused group or active buffer is not the popup's.
    pub(crate) fn signature_check(&mut self) {
        let Some(s) = &self.signature else { return };
        let overlay = self.editor_overlay_open();
        let same = self.editor.focused_group().id == s.group && self.editor.active_buffer().is_some_and(|b| b.id == s.buffer);
        if overlay || !same || !self.focus.is(PanelId::Editor) {
            self.signature_close();
        }
    }

    /// Close the popup and cancel a request in flight, so a late answer
    /// cannot reopen it.
    pub(super) fn signature_close(&mut self) {
        if self.signature.take().is_some() {
            self.dirty_frame = true;
        }
        if let Some(old) = self.lsp.inflight.remove(SIGNATURE_HELP)
            && let Some(c) = self.lsp.clients.get(&old.server)
        {
            c.cancel(old.id);
        }
    }

    /// Esc closes the popup (consumed); other keys go on. Esc before the
    /// first answer cancels the request and goes on.
    pub(super) fn signature_key(&mut self, key: KeyEvent) -> bool {
        self.signature_check();
        if key.kind == KeyEventKind::Release
            || matches!(key.code, KeyCode::Modifier(_))
            || key.code != KeyCode::Esc
            || !key.modifiers.is_empty()
        {
            return false;
        }
        let open = self.signature.is_some();
        self.signature_close();
        if open {
            self.dispatcher.reset();
        }
        open
    }
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::fake::FakeServer;
    use crossterm::event::KeyModifiers;

    const METHOD: &str = "textDocument/signatureHelp";
    const MAIN: &str = "fn main() {\n    \n}\n";

    fn caps(signature: bool) -> Value {
        let mut c = incremental();
        if signature {
            c["signatureHelpProvider"] = json!({"triggerCharacters": ["(", ","], "retriggerCharacters": [")"]});
        }
        c
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, crossterm::event::KeyModifiers::NONE));
    }

    fn ctrl_p(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    }

    fn typ(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    fn help(active_param: u32) -> Value {
        json!({"signatures": [{"label": "fn add(a: u32, b: u32) -> u32", "parameters": [{"label": [7, 13]}, {"label": [15, 21]}]}],
               "activeSignature": 0, "activeParameter": active_param})
    }

    async fn setup(name: &str, signature: bool) -> (App, FakeServer, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        let mut servers = with_fake_servers(&mut app, caps(signature));
        let server = open_on_server(&mut app, &mut servers, "a.rs", MAIN).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        (app, server, dir)
    }

    async fn reply(app: &mut App, server: &mut FakeServer, req: &Value, result: Value) {
        server.respond(req["id"].as_i64().unwrap(), result);
        handle_next(app, is_lsp).await;
    }

    fn draw(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let buf = term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect()).collect()
    }

    /// Typed `add(` and answered: the popup is open.
    async fn open(app: &mut App, server: &mut FakeServer) {
        typ(app, "add(");
        let req = server.recv(METHOD).await;
        reply(app, server, &req, help(0)).await;
        assert!(app.signature.is_some());
    }

    #[tokio::test]
    async fn typing_a_trigger_opens_and_a_null_retrigger_closes() {
        let (mut app, mut server, dir) = setup("sig-trigger", true).await;
        typ(&mut app, "add(");
        let req = server.recv(METHOD).await;
        assert_eq!(req["params"]["context"], json!({"triggerKind": 2, "triggerCharacter": "(", "isRetrigger": false}));
        reply(&mut app, &mut server, &req, help(0)).await;
        assert!(app.signature.is_some());
        typ(&mut app, "1,");
        let mut req = server.recv(METHOD).await;
        while req["params"]["context"]["triggerCharacter"] != "," {
            req = server.recv(METHOD).await;
        }
        assert_eq!(req["params"]["context"]["isRetrigger"], true);
        assert!(req["params"]["context"]["activeSignatureHelp"]["signatures"].is_array());
        reply(&mut app, &mut server, &req, help(1)).await;
        assert_eq!(app.signature.as_ref().unwrap().active(), (0, Some(1)));
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Down);
        let mut req = server.recv(METHOD).await;
        while req["params"]["position"]["line"] != 2 {
            req = server.recv(METHOD).await;
        }
        assert_eq!(req["params"]["context"]["triggerKind"], 3);
        reply(&mut app, &mut server, &req, Value::Null).await;
        assert!(app.signature.is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn ctrl_p_asks_and_esc_closes() {
        let (mut app, mut server, dir) = setup("sig-ctrl-p", true).await;
        ctrl_p(&mut app);
        let req = server.recv(METHOD).await;
        assert_eq!(req["params"]["context"]["triggerKind"], 1);
        reply(&mut app, &mut server, &req, help(0)).await;
        assert!(app.signature.is_some());
        let head = app.editor.active_buffer().unwrap().view.sel().head;
        key(&mut app, KeyCode::Esc);
        assert!(app.signature.is_none());
        assert_eq!(app.editor.active_buffer().unwrap().view.sel().head, head);
        ctrl_p(&mut app);
        let req = server.recv(METHOD).await;
        reply(&mut app, &mut server, &req, Value::Null).await;
        assert!(toasts(&app).contains(&"No parameter info".to_string()), "{:?}", toasts(&app));
        ctrl_p(&mut app);
        let req = server.recv(METHOD).await;
        key(&mut app, KeyCode::Esc);
        reply(&mut app, &mut server, &req, help(0)).await;
        assert!(app.signature.is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn popup_closes_when_focus_leaves_the_editor() {
        let (mut app, mut server, dir) = setup("sig-focus", true).await;
        open(&mut app, &mut server).await;
        app.focus.set(PanelId::Project);
        draw(&mut app, 80, 24);
        assert!(app.signature.is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn server_without_signature_help_is_not_asked() {
        let (mut app, mut server, dir) = setup("sig-none", false).await;
        typ(&mut app, "f(");
        ctrl_p(&mut app);
        assert!(toasts(&app).contains(&"rust-analyzer does not support parameter info".to_string()), "{:?}", toasts(&app));
        let sent = server.sent_so_far(&app.lsp.clients["rust-analyzer"]).await;
        assert!(sent.iter().all(|m| m["method"] != METHOD), "{sent:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_click_on_the_popup_is_swallowed() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, mut server, dir) = setup("sig-click", true).await;
        for _ in 0..4 {
            key(&mut app, KeyCode::Enter);
        }
        open(&mut app, &mut server).await;
        app.toasts = Default::default();
        draw(&mut app, 80, 24);
        let (cx, cy) = app.editor.caret_cell().unwrap();
        let head = app.editor.active_buffer().unwrap().view.sel().head;
        // Row cy - 2 is the signature's text; a click on another cell of
        // the editor would move the caret there.
        let click =
            MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: cx + 2, row: cy - 2, modifiers: KeyModifiers::NONE };
        assert!(app.signature.as_ref().unwrap().contains(click.column, click.row));
        app.handle_mouse(click);
        assert_eq!(app.editor.active_buffer().unwrap().view.sel().head, head);
        assert!(app.signature.is_some(), "the popup stays");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn the_popup_sits_above_the_caret() {
        let (mut app, mut server, dir) = setup("sig-above", true).await;
        // Down the screen, so there is room above the caret.
        for _ in 0..4 {
            key(&mut app, KeyCode::Enter);
        }
        open(&mut app, &mut server).await;
        app.toasts = Default::default();
        let screen = draw(&mut app, 80, 24);
        let (_, cy) = app.editor.caret_cell().unwrap();
        let cy = cy as usize;
        assert!(screen[cy - 1].contains('└'), "{}", screen[cy - 1]);
        assert!(screen[cy - 2].contains("fn add(a: u32, b: u32) -> u32"), "{}", screen[cy - 2]);
        std::fs::remove_dir_all(dir).ok();
    }
}
