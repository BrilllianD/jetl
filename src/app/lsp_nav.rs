//! Language server navigation: go to definition (Ctrl+B, Ctrl+click), find
//! usages (Alt+F7) and quick documentation (Alt+Q), all about the active
//! buffer's caret.
//!
//! A request is sent after a sync, so the server has the text it refers
//! to, and carries a [`DocTag`]: its answer is used only while the buffer,
//! its version and the caret are what they were, and only for the newest
//! request of its method ([`super::lsp::Inflight`]; a newer one cancels the
//! older with `$/cancelRequest`). Locations open in the Locations palette
//! with a preview of their line (from the open buffer, else read from disk
//! off the main thread); a single definition is jumped to, and a definition
//! that is where the caret already is lists the usages instead.

use lsp_types::{Position, Uri};
use serde_json::{Value, json};

use super::App;
use crate::actions::PaletteKind;
use crate::editor::buffer::BufferId;
use crate::event::{AppMsg, Level};
use crate::lsp::locations::{self, LocationItem, Target};
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag, pos};
use crate::ui::focus::PanelId;
use crate::ui::hover::HoverPopup;
use crate::ui::palette::Palette;

pub(super) use crate::lsp::caps::{
    CODE_ACTION, CODE_ACTION_RESOLVE, COMPLETION, DOCUMENT_SYMBOL, EXECUTE_COMMAND, FORMATTING, INLAY_HINT, PREPARE_RENAME,
    RANGE_FORMATTING, RENAME, RESOLVE,
};
use crate::lsp::caps::{
    Caps, DECLARATION, DEFINITION, HOVER, IMPLEMENTATION, REFERENCES, SIGNATURE_HELP, TYPE_DEFINITION, WORKSPACE_SYMBOL,
};

/// The method a tag was sent with.
fn method_of(tag: &PendingTag) -> &'static str {
    match tag {
        PendingTag::Definition(_) => DEFINITION,
        PendingTag::TypeDefinition(_) => TYPE_DEFINITION,
        PendingTag::Implementation(_) => IMPLEMENTATION,
        PendingTag::Declaration(_) => DECLARATION,
        PendingTag::References(_) => REFERENCES,
        PendingTag::Hover(_) => HOVER,
        PendingTag::SignatureHelp(_) => SIGNATURE_HELP,
        PendingTag::Completion(_) => COMPLETION,
        PendingTag::CompletionResolve(_) | PendingTag::CompletionDocs(_) => RESOLVE,
        PendingTag::PrepareRename(_) => PREPARE_RENAME,
        PendingTag::Rename(_) => RENAME,
        PendingTag::CodeAction(_) => CODE_ACTION,
        PendingTag::CodeActionResolve(_) => CODE_ACTION_RESOLVE,
        PendingTag::ExecuteCommand(_) => EXECUTE_COMMAND,
        PendingTag::DocumentSymbols(_) => DOCUMENT_SYMBOL,
        PendingTag::Formatting(_) => FORMATTING,
        PendingTag::Lightbulb(_) => CODE_ACTION,
        PendingTag::InlayHints(_) => INLAY_HINT,
        PendingTag::Initialize => "initialize",
        PendingTag::Shutdown => "shutdown",
        PendingTag::WillFileOp(_) => "workspace/will*Files",
        PendingTag::WorkspaceSymbols(_) => WORKSPACE_SYMBOL,
    }
}

/// Whether `method`'s params are about a document (`textDocument` is
/// added); a resolve or a command carries what the server gave instead.
fn about_document(method: &str) -> bool {
    ![RESOLVE, CODE_ACTION_RESOLVE, EXECUTE_COMMAND].contains(&method)
}

/// What to call `method` when the server lacks it.
fn what(method: &str) -> &'static str {
    match method {
        DEFINITION => "go to definition",
        TYPE_DEFINITION => "go to type declaration",
        IMPLEMENTATION => "go to implementation",
        DECLARATION => "go to declaration",
        REFERENCES => "find usages",
        HOVER => "documentation",
        SIGNATURE_HELP => "parameter info",
        WORKSPACE_SYMBOL => "workspace symbols",
        COMPLETION => "code completion",
        RESOLVE => "resolving completions",
        PREPARE_RENAME => "checking renames",
        RENAME => "rename",
        CODE_ACTION => "context actions",
        CODE_ACTION_RESOLVE => "resolving context actions",
        EXECUTE_COMMAND => "commands",
        DOCUMENT_SYMBOL => "file structure",
        FORMATTING => "formatting",
        INLAY_HINT => "inlay hints",
        RANGE_FORMATTING => "formatting a selection",
        _ => "this request",
    }
}

/// Whether the server answers `method`, and what to call it otherwise.
fn supports(caps: &Caps<'_>, method: &str) -> Result<(), &'static str> {
    if caps.has(method) { Ok(()) } else { Err(what(method)) }
}

impl App {
    /// A short note for the user: a toast, not kept in the Event Log.
    pub(super) fn lsp_hint(&mut self, text: impl Into<String>) {
        self.toasts.push(Level::Info, &text.into(), std::time::Instant::now());
        self.dirty_frame = true;
    }

    /// The server and URI of buffer `id`'s document, or why it has none.
    fn lsp_doc_of(&self, id: BufferId) -> Result<(String, Uri), String> {
        if !self.config.lsp.enabled {
            return Err("Language servers are off ([lsp] enabled = false)".into());
        }
        if let Some(doc) = self.lsp.docs.get(&id) {
            return Ok((doc.server.clone(), doc.uri.clone()));
        }
        Err(match self.lsp_status() {
            Some(super::lsp::LspStatus::Busy(s)) => format!("{s}; try again in a moment"),
            Some(super::lsp::LspStatus::Down(s)) => s,
            _ => "No language server for this file".into(),
        })
    }

    /// Send `method` about `pos` in buffer `id` (`extra` adds parameters)
    /// tagged by `tag`, cancelling the one in flight. Returns the seq, or
    /// why it was not sent.
    pub(super) fn lsp_doc_request(
        &mut self,
        id: BufferId,
        pos: Position,
        method: &'static str,
        extra: Value,
        tag: fn(DocTag) -> PendingTag,
    ) -> Result<u64, String> {
        let mut params = json!({"position": pos});
        if let (Some(p), Value::Object(extra)) = (params.as_object_mut(), extra) {
            p.extend(extra);
        }
        self.lsp_tagged_request(id, pos, method, params, tag)
    }

    /// Send `method` with `params` (`textDocument` added when it is an
    /// object) about buffer `id`, tagged with `pos`, cancelling the one in
    /// flight. Returns the seq, or why it was not sent.
    pub(super) fn lsp_tagged_request(
        &mut self,
        id: BufferId,
        pos: Position,
        method: &'static str,
        params: Value,
        tag: fn(DocTag) -> PendingTag,
    ) -> Result<u64, String> {
        self.lsp_send(id, pos, method, params, tag, true).map(|s| s.seq)
    }

    /// Whether buffer `id`'s server takes `method` now: its server's name
    /// and the document's URI, or why not (empty: no such buffer). Syncs
    /// first, so a request sent next is about the current text.
    pub(super) fn lsp_check(&mut self, id: BufferId, method: &str) -> Result<(String, Uri), String> {
        self.lsp_sync();
        self.editor.index_of_id(id).ok_or_else(String::new)?;
        let (server, uri) = self.lsp_doc_of(id)?;
        let Some(caps) = self.lsp_caps(id) else {
            return Err(format!("{server} is not running"));
        };
        supports(&caps, method).map_err(|what| format!("{server} does not support {what}"))?;
        Ok((server, uri))
    }

    /// What buffer `id`'s server offers for it: static capabilities and
    /// the registrations that apply to the document. `None`: not open on a
    /// running server.
    pub(super) fn lsp_caps(&self, id: BufferId) -> Option<Caps<'_>> {
        let doc = self.lsp.docs.get(&id)?;
        let client = self.lsp.clients.get(&doc.server)?;
        Some(Caps { fixed: client.capabilities()?, registry: &client.registry, doc: Some(doc.doc_ref()) })
    }

    /// [`App::lsp_tagged_request`]; `newest` makes it the request of its
    /// method in flight (cancelling the one before), else it is left to
    /// the caller to track (formatting, one per buffer).
    pub(super) fn lsp_send(
        &mut self,
        id: BufferId,
        pos: Position,
        method: &'static str,
        mut params: Value,
        tag: fn(DocTag) -> PendingTag,
        newest: bool,
    ) -> Result<super::lsp::Inflight, String> {
        let (server, uri) = self.lsp_check(id, method)?;
        let version = self.editor.index_of_id(id).map(|i| self.editor.buffers[i].version).ok_or_else(String::new)?;
        if newest
            && let Some(old) = self.lsp.inflight.remove(method)
            && let Some(c) = self.lsp.clients.get(&old.server)
        {
            c.cancel(old.id);
        }
        self.lsp.last_seq += 1;
        let seq = self.lsp.last_seq;
        if about_document(method)
            && let Some(p) = params.as_object_mut()
        {
            p.insert("textDocument".into(), json!({"uri": uri}));
        }
        let group = self.editor.focused_group().id;
        let client = self.lsp.clients.get_mut(&server).expect("checked above");
        let rid = client.request(method, params, tag(DocTag { buffer: id, version, pos, seq, group }));
        let sent = super::lsp::Inflight { server, id: rid, seq };
        if newest {
            self.lsp.inflight.insert(method, sent.clone());
        }
        Ok(sent)
    }

    /// The active buffer's id and caret as an LSP position.
    pub(super) fn caret_position(&self) -> Option<(BufferId, Position)> {
        let b = self.editor.active_buffer()?;
        Some((b.id, pos::position(&b.rope, b.view.sel().head)))
    }

    /// `lsp.goto_definition` (Ctrl+B, Ctrl+click). Without a server for
    /// the file, Go to Symbol opens on the word at the caret instead.
    pub(super) fn lsp_goto_definition(&mut self) {
        let Some((id, pos)) = self.caret_position() else { return };
        match self.lsp_doc_request(id, pos, DEFINITION, json!({}), PendingTag::Definition) {
            Ok(seq) => self.lsp.nav_seq = seq,
            Err(why) => {
                let word = self.editor.active_buffer().map(|b| word_at(b, b.view.sel().head)).unwrap_or_default();
                if !self.lsp.docs.contains_key(&id) && !word.is_empty() {
                    self.open_palette(PaletteKind::Symbols);
                    if let Some(p) = &mut self.palette {
                        p.set_query(&word);
                    }
                } else if !why.is_empty() {
                    self.lsp_hint(why);
                }
            }
        }
    }

    /// `lsp.goto_type_definition` (Ctrl+Shift+B), `lsp.goto_implementation`
    /// (Ctrl+Alt+B), `lsp.goto_declaration`: `method` at the caret.
    pub(super) fn lsp_goto(&mut self, method: &'static str, tag: fn(DocTag) -> PendingTag) {
        let Some((id, pos)) = self.caret_position() else { return };
        match self.lsp_doc_request(id, pos, method, json!({}), tag) {
            Ok(seq) => self.lsp.nav_seq = seq,
            Err(why) if !why.is_empty() => self.lsp_hint(why),
            Err(_) => {}
        }
    }

    /// `lsp.find_usages` (Alt+F7).
    pub(super) fn lsp_find_usages(&mut self) {
        let Some((id, pos)) = self.caret_position() else { return };
        self.lsp_request_usages(id, pos);
    }

    fn lsp_request_usages(&mut self, id: BufferId, pos: Position) {
        let extra = json!({"context": {"includeDeclaration": false}});
        match self.lsp_doc_request(id, pos, REFERENCES, extra, PendingTag::References) {
            Ok(seq) => self.lsp.nav_seq = seq,
            Err(why) if !why.is_empty() => self.lsp_hint(why),
            Err(_) => {}
        }
    }

    /// `lsp.quick_doc` (Alt+Q).
    pub(super) fn lsp_quick_doc(&mut self) {
        self.hover = None;
        self.completion_close();
        let Some((id, pos)) = self.caret_position() else { return };
        if let Err(why) = self.lsp_doc_request(id, pos, HOVER, json!({}), PendingTag::Hover)
            && !why.is_empty()
        {
            self.lsp_hint(why);
        }
    }

    /// The answer to one of the requests above: dropped unless it is the
    /// newest of its method and the buffer, its version and the caret are
    /// as they were when it was sent.
    pub(super) fn lsp_response(&mut self, name: &str, tag: PendingTag, result: Result<Value, ResponseError>) {
        if let PendingTag::WillFileOp(seq) = tag {
            return self.will_file_op_answer(name, seq, result);
        }
        if let PendingTag::WorkspaceSymbols(seq) = tag {
            return self.workspace_symbols_answer(name, seq, result);
        }
        let Some(t) = tag.doc().cloned() else { return };
        // Formatting is checked against its buffer's format job.
        if let PendingTag::Formatting(t) = tag {
            return self.lsp_format_answer(name, t, result);
        }
        // So is the lightbulb, against its own request.
        if let PendingTag::Lightbulb(t) = tag {
            return self.lightbulb_answer(t, result);
        }
        // And the inlay hints.
        if let PendingTag::InlayHints(t) = tag {
            return self.inlay_answer(t, result);
        }
        let method = method_of(&tag);
        match self.lsp.inflight.get(method) {
            Some(f) if f.seq == t.seq => {
                self.lsp.inflight.remove(method);
            }
            _ => return tracing::debug!("dropping a superseded {method} answer"),
        }
        // Completion checks its answers itself: they stay good while the
        // word at the caret is typed.
        if let PendingTag::Completion(_) | PendingTag::CompletionResolve(_) | PendingTag::CompletionDocs(_) = tag {
            return self.completion_response(name, tag, result);
        }
        // A command's answer says nothing but whether it failed; what it
        // does comes as `workspace/applyEdit`.
        if let PendingTag::ExecuteCommand(_) = tag {
            if let Err(e) = result
                && ![ResponseError::REQUEST_CANCELLED, ResponseError::CONTENT_MODIFIED].contains(&e.code)
            {
                self.lsp_hint(format!("{name}: {}", e.message));
            }
            return;
        }
        if self.caret_position() != Some((t.buffer, t.pos))
            || self.editor.active_buffer().is_none_or(|b| b.version != t.version)
            || self.editor.focused_group().id != t.group
        {
            return tracing::debug!("dropping a stale {method} answer");
        }
        let value = match result {
            Ok(v) => v,
            Err(e) if [ResponseError::REQUEST_CANCELLED, ResponseError::CONTENT_MODIFIED].contains(&e.code) => return,
            Err(e) => return self.lsp_hint(format!("{name}: {}", e.message)),
        };
        self.dirty_frame = true;
        match tag {
            PendingTag::Definition(t) => self.lsp_definition_answer(t, value),
            PendingTag::TypeDefinition(t) => self.lsp_goto_answer(t, value, TYPE_DEFINITION),
            PendingTag::Implementation(t) => self.lsp_goto_answer(t, value, IMPLEMENTATION),
            PendingTag::Declaration(t) => self.lsp_goto_answer(t, value, DECLARATION),
            PendingTag::References(t) => self.lsp_usages_answer(t, value),
            PendingTag::Hover(t) => self.lsp_hover_answer(t, value),
            PendingTag::SignatureHelp(t) => self.signature_answer(t, value),
            PendingTag::PrepareRename(t) => self.lsp_prepare_rename_answer(t, value),
            PendingTag::Rename(t) => self.lsp_rename_answer(t, value),
            PendingTag::CodeAction(t) => self.lsp_code_actions_answer(t, value),
            PendingTag::CodeActionResolve(t) => self.lsp_code_action_resolved(t, value),
            PendingTag::DocumentSymbols(t) => self.lsp_document_symbols_answer(t, value),
            PendingTag::Completion(_)
            | PendingTag::CompletionResolve(_)
            | PendingTag::CompletionDocs(_)
            | PendingTag::ExecuteCommand(_)
            | PendingTag::Formatting(_)
            | PendingTag::Lightbulb(_)
            | PendingTag::InlayHints(_)
            | PendingTag::WillFileOp(_)
            | PendingTag::WorkspaceSymbols(_)
            | PendingTag::Initialize
            | PendingTag::Shutdown => {}
        }
    }

    fn lsp_definition_answer(&mut self, t: DocTag, value: Value) {
        let targets = locations::parse(value);
        let here = self.editor.active_buffer().and_then(|b| b.path.clone());
        match targets.as_slice() {
            [] => self.lsp_hint("Cannot find declaration to go to"),
            // Already on the declaration: its usages, as in JetBrains.
            [one] if here.as_ref() == Some(&one.path) && locations::contains(&one.range, t.pos) => self.lsp_request_usages(t.buffer, t.pos),
            [one] => {
                self.editor.push_nav_here();
                self.open_file_at_utf16(one.path.clone(), one.range.start);
            }
            _ => self.show_location_targets("Choose Declaration".into(), t.seq, targets),
        }
    }

    /// The answer to `lsp_goto`: one target is jumped to, several are listed.
    fn lsp_goto_answer(&mut self, t: DocTag, value: Value, method: &'static str) {
        let targets = locations::parse(value);
        match targets.as_slice() {
            [] => self.lsp_hint(match method {
                TYPE_DEFINITION => "Cannot find type declaration",
                IMPLEMENTATION => "No implementations found",
                _ => "Cannot find declaration to go to",
            }),
            [one] => {
                self.editor.push_nav_here();
                self.open_file_at_utf16(one.path.clone(), one.range.start);
            }
            _ => {
                let title = match method {
                    TYPE_DEFINITION => "Choose Type Declaration".to_string(),
                    IMPLEMENTATION => {
                        let word = self.editor.active_buffer().map(|b| word_at(b, b.view.sel().head)).unwrap_or_default();
                        if word.is_empty() { "Implementations".to_string() } else { format!("Implementations of {word}") }
                    }
                    _ => "Choose Declaration".to_string(),
                };
                self.show_location_targets(title, t.seq, targets);
            }
        }
    }

    fn lsp_usages_answer(&mut self, t: DocTag, value: Value) {
        let targets = locations::parse(value);
        if targets.is_empty() {
            return self.lsp_hint("No usages found");
        }
        if let [one] = targets.as_slice() {
            self.editor.push_nav_here();
            self.open_file_at_utf16(one.path.clone(), one.range.start);
            return self.lsp_hint("Only one usage");
        }
        let word = self.editor.active_buffer().map(|b| word_at(b, b.view.sel().head)).unwrap_or_default();
        let title = if word.is_empty() { "Usages".to_string() } else { format!("Usages of {word}") };
        self.show_location_targets(title, t.seq, targets);
    }

    fn lsp_hover_answer(&mut self, t: DocTag, value: Value) {
        let hover = serde_json::from_value::<Option<lsp_types::Hover>>(value).ok().flatten();
        let Some(head) = self.editor.active_buffer().map(|b| b.view.sel().head) else { return };
        let fallback = self.editor.index_of_id(t.buffer).and_then(|i| self.editor.buffers[i].language());
        let hl = crate::ui::hover::Highlight { theme: &self.theme, fallback };
        match hover.and_then(|h| HoverPopup::new(self.editor.focused_group().id, t.buffer, t.version, head, h.contents, hl)) {
            Some(popup) => self.hover = Some(popup),
            None => self.lsp_hint("No documentation found"),
        }
    }

    /// Rows for `targets` with their line text: open buffers' lines now,
    /// the rest read from disk off the main thread (`AppMsg::Locations`).
    fn show_location_targets(&mut self, title: String, seq: u64, targets: Vec<Target>) {
        let mut items = Vec::new();
        let mut disk = Vec::new();
        for t in targets {
            match self.editor.index_of(&t.path).map(|i| &self.editor.buffers[i]) {
                Some(b) => {
                    let line = t.range.start.line as usize;
                    let text = (line < b.rope.len_lines()).then(|| b.line_text(line));
                    items.push(LocationItem::at(t.path, t.range, text.as_deref()));
                }
                None => disk.push(t),
            }
        }
        if disk.is_empty() {
            return self.show_locations(title, items);
        }
        let (tx, generation) = (self.bus_tx.clone(), self.workspace.generation);
        tokio::task::spawn_blocking(move || {
            items.extend(locations::read_items(disk));
            let _ = tx.send(AppMsg::Locations { generation, seq, title, items });
        });
    }

    /// `AppMsg::Locations`: shown unless the root changed or a newer
    /// definition or usages request was made meanwhile.
    pub(super) fn locations_loaded(&mut self, generation: u64, seq: u64, title: String, items: Vec<LocationItem>) {
        if generation != self.workspace.generation || seq != self.lsp.nav_seq {
            return tracing::debug!("dropping stale locations");
        }
        self.show_locations(title, items);
    }

    /// Open the Locations palette on `items` (sorted by file and position).
    /// A dialog that came up meanwhile keeps the input; nothing opens.
    pub(crate) fn show_locations(&mut self, title: String, mut items: Vec<LocationItem>) {
        if self.dialog.is_some() {
            return;
        }
        items.sort_by(|a, b| (&a.path, a.line, a.col).cmp(&(&b.path, b.line, b.col)));
        let mut p = Palette::new(PaletteKind::Locations, self.workspace.root.clone());
        p.set_locations(title, items);
        self.palette = Some(p);
        self.hover = None;
        self.dispatcher.reset();
        self.dirty_frame = true;
    }

    /// Drop the hover popup once it no longer applies: the editor lost
    /// focus, an overlay opened, or the buffer, its version or the caret
    /// changed.
    pub(crate) fn hover_check(&mut self) {
        let Some(h) = &self.hover else { return };
        let overlay = self.palette.is_some()
            || self.dialog.is_some()
            || self.menu.is_some()
            || self.menu_bar.is_some()
            || self.settings.is_some()
            || self.run_configs.is_some();
        let same = self.editor.focused_group().id == h.group
            && self.editor.active_buffer().is_some_and(|b| b.id == h.buffer && b.version == h.version && b.view.sel().head == h.head);
        if overlay || !same || !self.focus.is(PanelId::Editor) {
            self.hover = None;
            self.dirty_frame = true;
        }
    }

    /// Keys while the hover popup is up: Esc closes it, Up/Down/PgUp/PgDn
    /// scroll it; any other key closes it and is not consumed (it goes on
    /// to the editor). Returns whether the key was consumed.
    pub(super) fn hover_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyEventKind};
        self.hover_check();
        let Some(h) = &mut self.hover else { return false };
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return false;
        }
        self.dirty_frame = true;
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            self.hover = None;
            self.dispatcher.reset();
            return true;
        }
        if h.handle_key(key) {
            self.dispatcher.reset();
            return true;
        }
        self.hover = None;
        false
    }
}

/// The identifier at char `at` of `b` (empty when there is none).
pub(super) fn word_at(b: &crate::editor::buffer::Buffer, at: usize) -> String {
    let r = crate::editor::selection::word_at(&b.rope, at);
    let w = b.rope.slice(r).to_string();
    if w.chars().all(|c| c.is_alphanumeric() || c == '_') { w } else { String::new() }
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, LspAction, Motion};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::fake::FakeServer;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    /// Navigation and hover on top of incremental sync.
    fn caps() -> Value {
        let mut c = incremental();
        c["definitionProvider"] = json!(true);
        c["referencesProvider"] = json!(true);
        c["hoverProvider"] = json!(true);
        c["typeDefinitionProvider"] = json!(true);
        c["implementationProvider"] = json!(true);
        c["declarationProvider"] = json!(true);
        c
    }

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    fn loc(path: &std::path::Path, r: Value) -> Value {
        json!({"uri": crate::lsp::sync::uri(path).unwrap().as_str(), "range": r})
    }

    fn caret(app: &App) -> (usize, usize) {
        let b = app.editor.active_buffer().unwrap();
        crate::editor::selection::pos_to_line_col(&b.rope, b.view.sel().head)
    }

    fn act(app: &mut App, a: LspAction) {
        app.perform(Action::Lsp(a));
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    /// Put the caret at 0-based `line`, char `col`.
    fn goto(app: &mut App, line: usize, col: usize) {
        app.editor.active_buffer_mut().unwrap().goto_line(line, col);
    }

    async fn answer(app: &mut App, server: &mut FakeServer, method: &str, result: Value) -> Value {
        let req = server.recv(method).await;
        server.respond(req["id"].as_i64().unwrap(), result);
        handle_next(app, is_lsp).await;
        req
    }

    /// `a.rs`: the caret line has a CJK string and an emoji before the call.
    const A: &str = "fn main() {\n    let s = \"日本😀\"; helper(s);\n}\n";
    /// `b.rs`: the definition, after a surrogate pair on its line.
    const B: &str = "// b\n/* 😀 */ pub fn helper(s: &str) {}\n";

    #[tokio::test]
    async fn definition_in_a_closed_file_opens_it_and_back_returns() {
        let (mut app, dir) = test_app("lsp-def");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        // On `helper`: char 21, UTF-16 unit 22 (the emoji counts two).
        goto(&mut app, 1, 21);
        act(&mut app, LspAction::GotoDefinition);
        // `pub fn helper`: `/* 😀 */ pub fn ` is 16 units, 15 chars.
        let req = answer(&mut app, &mut server, "textDocument/definition", loc(&b, range(1, 16, 1, 22))).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 22}));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        assert_eq!(caret(&app), (1, 15));
        // Back returns to the call, forward to the definition.
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!((app.editor.active_buffer().unwrap().name.as_str(), caret(&app)), ("a.rs", (1, 21)));
        app.perform(Action::Editor(EditorAction::NavForward));
        assert_eq!((app.editor.active_buffer().unwrap().name.as_str(), caret(&app)), ("b.rs", (1, 15)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn several_definitions_open_the_palette_and_none_says_so() {
        let (mut app, dir) = test_app("lsp-defs");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        goto(&mut app, 1, 21);
        // LocationLinks: one in the open file, one on disk.
        act(&mut app, LspAction::GotoDefinition);
        let links = json!([
            {"targetUri": crate::lsp::sync::uri(&b).unwrap().as_str(), "targetRange": range(1, 0, 1, 30), "targetSelectionRange": range(1, 16, 1, 22)},
            {"targetUri": crate::lsp::sync::uri(&a).unwrap().as_str(), "targetRange": range(0, 0, 2, 1), "targetSelectionRange": range(0, 3, 0, 7)},
        ]);
        answer(&mut app, &mut server, "textDocument/definition", links).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::Locations { .. })).await;
        let p = app.palette.as_mut().expect("the Locations palette");
        assert_eq!(p.kind, PaletteKind::Locations);
        // Grouped by file: headers carry the name and count, not `path:line`.
        let buf = draw(&mut app, 100, 30);
        let rows: Vec<String> = (0..30).map(|y| row(&buf, y)).collect();
        assert!(rows.iter().any(|r| r.contains("b.rs") && r.contains('1') && !r.contains("b.rs:")), "{rows:#?}");
        // Rows are [a.rs, a item, b.rs, b item], the selection on a's item;
        // Down lands on b.rs's header and Enter opens b's first location.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.palette.is_none());
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!((app.editor.active_path(), caret(&app)), (Some(b.as_path()), (1, 15)));
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!((app.editor.active_path(), caret(&app)), (Some(a.as_path()), (1, 21)));

        // Null: a toast, nothing opens.
        act(&mut app, LspAction::GotoDefinition);
        answer(&mut app, &mut server, "textDocument/definition", Value::Null).await;
        assert!(app.palette.is_none());
        assert!(toasts(&app).contains(&"Cannot find declaration to go to".to_string()));
        // An answer after the caret moved is dropped.
        act(&mut app, LspAction::GotoDefinition);
        app.perform(Action::Editor(EditorAction::Move(Motion::Left, false)));
        answer(&mut app, &mut server, "textDocument/definition", loc(&b, range(1, 16, 1, 22))).await;
        assert_eq!(app.editor.active_path(), Some(a.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn type_definition_jumps_and_none_says_so() {
        let (mut app, dir) = test_app("lsp-typedef");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        goto(&mut app, 1, 21);
        app.handle_key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        let req = answer(&mut app, &mut server, "textDocument/typeDefinition", loc(&b, range(1, 16, 1, 22))).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 22}));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!((app.editor.active_path(), caret(&app)), (Some(b.as_path()), (1, 15)));
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!((app.editor.active_buffer().unwrap().name.as_str(), caret(&app)), ("a.rs", (1, 21)));
        act(&mut app, LspAction::GotoTypeDefinition);
        answer(&mut app, &mut server, "textDocument/typeDefinition", Value::Null).await;
        assert!(toasts(&app).contains(&"Cannot find type declaration".to_string()), "{:?}", toasts(&app));
        // Alt+Shift+B asks the same.
        app.handle_key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::ALT | KeyModifiers::SHIFT));
        answer(&mut app, &mut server, "textDocument/typeDefinition", Value::Null).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn implementations_list_in_the_palette() {
        let (mut app, dir) = test_app("lsp-impls");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        goto(&mut app, 1, 21);
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        answer(&mut app, &mut server, "textDocument/implementation", json!([loc(&a, range(0, 3, 0, 7)), loc(&b, range(1, 16, 1, 22))]))
            .await;
        handle_next(&mut app, |m| matches!(m, AppMsg::Locations { .. })).await;
        assert_eq!(app.palette.as_ref().expect("the Locations palette").kind, PaletteKind::Locations);
        let buf = draw(&mut app, 100, 30);
        let rows: Vec<String> = (0..30).map(|y| row(&buf, y)).collect();
        assert!(rows.iter().any(|r| r.contains("Implementations of helper")), "{rows:#?}");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        // Empty: a toast.
        act(&mut app, LspAction::GotoImplementation);
        answer(&mut app, &mut server, "textDocument/implementation", json!([])).await;
        assert!(app.palette.is_none());
        assert!(toasts(&app).contains(&"No implementations found".to_string()), "{:?}", toasts(&app));
        // A superseded answer is dropped.
        act(&mut app, LspAction::GotoImplementation);
        let first = server.recv("textDocument/implementation").await["id"].as_i64().unwrap();
        act(&mut app, LspAction::GotoImplementation);
        assert_eq!(server.recv("$/cancelRequest").await["params"]["id"], first);
        let second = server.recv("textDocument/implementation").await["id"].as_i64().unwrap();
        server.respond(first, json!([loc(&b, range(1, 16, 1, 22))]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.editor.active_path(), Some(a.as_path()));
        assert!(app.pending_goto.is_none(), "the superseded answer is not followed");
        server.respond(second, Value::Null);
        handle_next(&mut app, is_lsp).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn declaration_and_missing_capabilities() {
        let (mut app, dir) = test_app("lsp-decl");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        goto(&mut app, 1, 21);
        act(&mut app, LspAction::GotoDeclaration);
        answer(&mut app, &mut server, "textDocument/declaration", loc(&b, range(1, 16, 1, 22))).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!((app.editor.active_path(), caret(&app)), (Some(b.as_path()), (1, 15)));
        let _ = std::fs::remove_dir_all(&dir);

        let (mut app, dir) = test_app("lsp-no-goto");
        let mut servers = with_fake_servers(&mut app, incremental());
        let _server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        goto(&mut app, 1, 21);
        for (a, what) in [
            (LspAction::GotoTypeDefinition, "go to type declaration"),
            (LspAction::GotoImplementation, "go to implementation"),
            (LspAction::GotoDeclaration, "go to declaration"),
        ] {
            act(&mut app, a);
            assert!(toasts(&app).contains(&format!("rust-analyzer does not support {what}")), "{what}: {:?}", toasts(&app));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn definition_on_itself_lists_usages_with_previews() {
        let (mut app, dir) = test_app("lsp-usages");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "b.rs", B).await;
        let b = app.workspace.root.join("b.rs");
        let a = app.workspace.root.join("a.rs");
        std::fs::write(&a, A).unwrap();
        // On `helper` in its own declaration.
        goto(&mut app, 1, 17);
        act(&mut app, LspAction::GotoDefinition);
        answer(&mut app, &mut server, "textDocument/definition", json!([loc(&b, range(1, 16, 1, 22))])).await;
        let usages = loc(&a, range(1, 22, 1, 28));
        let req = answer(&mut app, &mut server, "textDocument/references", json!([usages, loc(&b, range(1, 16, 1, 22))])).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 18}));
        assert_eq!(req["params"]["context"], json!({"includeDeclaration": false}));
        // a.rs is read from disk.
        handle_next(&mut app, |m| matches!(m, AppMsg::Locations { .. })).await;
        app.toasts = Default::default();
        let buf = draw(&mut app, 100, 30);
        let rows: Vec<String> = (0..30).map(|y| row(&buf, y)).collect();
        assert!(rows.iter().any(|r| r.contains("Usages of helper")), "{rows:#?}");
        // (Wide chars are followed by a blank cell in the rows.)
        assert!(rows.iter().any(|r| r.contains("a.rs") && !r.contains("a.rs:")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("   2  let s = \"日") && r.contains("\"; helper(s);")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("b.rs") && !r.contains("b.rs:")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("   2  /* 😀") && r.contains("*/ pub fn helper(s: &str) {}")), "{rows:#?}");
        // Accepting the first row jumps to the call's char column.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!((app.editor.active_path(), caret(&app)), (Some(a.as_path()), (1, 21)));

        // Alt+F7 directly; an empty answer is a toast.
        app.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::ALT));
        answer(&mut app, &mut server, "textDocument/references", json!([])).await;
        assert!(toasts(&app).contains(&"No usages found".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn one_usage_jumps_straight_to_it() {
        let (mut app, dir) = test_app("lsp-one-usage");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        goto(&mut app, 1, 21);
        app.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::ALT));
        app.toasts = Default::default();
        answer(&mut app, &mut server, "textDocument/references", json!([loc(&b, range(1, 16, 1, 22))])).await;
        assert!(app.palette.is_none());
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!((app.editor.active_path(), caret(&app)), (Some(b.as_path()), (1, 15)));
        assert!(toasts(&app).contains(&"Only one usage".to_string()), "{:?}", toasts(&app));
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!((app.editor.active_buffer().unwrap().name.as_str(), caret(&app)), ("a.rs", (1, 21)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn draw(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone()
    }

    fn row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn hover(text: &str) -> Value {
        json!({"contents": {"kind": "markdown", "value": text}})
    }

    #[tokio::test]
    async fn hover_popup_shows_scrolls_and_closes() {
        let (mut app, dir) = test_app("lsp-hover");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        app.toasts = Default::default();
        goto(&mut app, 1, 21);
        draw(&mut app, 80, 24);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT));
        let doc = "```rust\nfn helper(s: &str)\n```\n---\nDoes **things**.";
        answer(&mut app, &mut server, "textDocument/hover", hover(doc)).await;
        assert!(app.hover.is_some());
        let buf = draw(&mut app, 80, 24);
        let (cx, cy) = app.editor.caret_cell().unwrap();
        // Under the caret line, from the caret column.
        let popup: Vec<String> = (cy + 1..cy + 6).map(|y| row(&buf, y)).collect();
        assert!(popup[0][..].contains('┌'), "{popup:#?}");
        assert!(popup[1].contains("fn helper(s: &str)"), "{popup:#?}");
        assert!(popup[2].contains("────"), "{popup:#?}");
        assert!(popup[3].contains("Does things."), "{popup:#?}");
        // At the caret column, or moved left to fit the screen.
        let x0 = (0..80).find(|&x| buf[(x, cy + 1)].symbol() == "┌").unwrap();
        assert!(x0 == cx || (x0 < cx && buf[(79, cy + 1)].symbol() == "┐"), "{x0} {cx}");
        // Scroll keys stay with the popup; Esc closes it and does nothing else.
        key(&mut app, KeyCode::Down);
        assert!(app.hover.is_some());
        assert_eq!(caret(&app), (1, 21));
        key(&mut app, KeyCode::Esc);
        assert!(app.hover.is_none());
        let buf = draw(&mut app, 80, 24);
        assert!(!row(&buf, cy + 2).contains("fn helper"));
        // Another key closes it and still reaches the editor.
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT));
        answer(&mut app, &mut server, "textDocument/hover", hover("doc")).await;
        assert!(app.hover.is_some());
        key(&mut app, KeyCode::Right);
        assert!(app.hover.is_none());
        assert_eq!(caret(&app), (1, 22));

        // Near the bottom of the screen it opens above the caret.
        let tall = (0..30).map(|i| format!("line {i}\n")).collect::<String>();
        app.perform(Action::Editor(EditorAction::Move(Motion::DocEnd, false)));
        app.perform(Action::Editor(EditorAction::InsertText(tall)));
        app.lsp_sync();
        draw(&mut app, 80, 24);
        act(&mut app, LspAction::QuickDoc);
        answer(&mut app, &mut server, "textDocument/hover", hover("one\n\ntwo")).await;
        let buf = draw(&mut app, 80, 24);
        let (_, cy) = app.editor.caret_cell().unwrap();
        assert!(row(&buf, cy - 1).contains('└'), "{}", row(&buf, cy - 1));
        assert!(row(&buf, cy - 2).contains("two"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_and_superseded_hovers_are_dropped() {
        let (mut app, dir) = test_app("lsp-hover-stale");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        goto(&mut app, 1, 21);
        // A second request cancels the first; the first's late answer is
        // dropped, the second's shown.
        act(&mut app, LspAction::QuickDoc);
        let first = server.recv("textDocument/hover").await["id"].as_i64().unwrap();
        act(&mut app, LspAction::QuickDoc);
        let cancel = server.recv("$/cancelRequest").await;
        assert_eq!(cancel["params"]["id"], first);
        let second = server.recv("textDocument/hover").await["id"].as_i64().unwrap();
        server.respond(first, hover("old"));
        handle_next(&mut app, is_lsp).await;
        assert!(app.hover.is_none(), "the cancelled request's answer");
        server.respond(second, hover("new"));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.hover.as_ref().unwrap().lines()[0].text, "new");
        // Moving the caret drops the popup.
        app.perform(Action::Editor(EditorAction::Move(Motion::Right, false)));
        app.hover_check();
        assert!(app.hover.is_none());
        // An answer after an edit (another version) is dropped.
        act(&mut app, LspAction::QuickDoc);
        app.perform(Action::Editor(EditorAction::InsertChar('x')));
        app.perform(Action::Editor(EditorAction::Move(Motion::Left, false)));
        answer(&mut app, &mut server, "textDocument/hover", hover("stale")).await;
        assert!(app.hover.is_none());
        // An empty answer is a toast.
        act(&mut app, LspAction::QuickDoc);
        answer(&mut app, &mut server, "textDocument/hover", Value::Null).await;
        assert!(toasts(&app).contains(&"No documentation found".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ctrl_click_goes_to_the_clicked_char() {
        let (mut app, dir) = test_app("lsp-click");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        app.toasts = Default::default();
        draw(&mut app, 100, 24);
        let ta = app.editor.text_area();
        // `helper` on line 2 is at visual column 22 (the emoji and the CJK
        // chars take two cells each): click its `l`, cell 25.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: ta.x + 25,
            row: ta.y + 1,
            modifiers: KeyModifiers::CONTROL,
        });
        assert_eq!(caret(&app), (1, 22));
        let req = server.recv("textDocument/definition").await;
        // Char 22 is UTF-16 unit 23.
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 23}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn without_a_server_there_is_a_hint_or_go_to_symbol() {
        let (mut app, dir) = test_app("lsp-none");
        crate::app::tests::load(&mut app, "notes.txt", "word here\n");
        act(&mut app, LspAction::QuickDoc);
        act(&mut app, LspAction::FindUsages);
        assert!(toasts(&app).iter().any(|t| t.contains("off")), "{:?}", toasts(&app));
        // Ctrl+B falls back to Go to Symbol with the word at the caret.
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        let p = app.palette.as_ref().expect("Go to Symbol");
        assert_eq!((p.kind, p.input.text.as_str()), (PaletteKind::Symbols, "word"));
        // A server without the capability says so.
        app.palette = None;
        let mut servers = with_fake_servers(&mut app, incremental());
        let _server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        act(&mut app, LspAction::QuickDoc);
        assert!(toasts(&app).contains(&"rust-analyzer does not support documentation".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hover_anchors_in_the_focused_group_and_closes_when_it_changes() {
        let (mut app, dir) = test_app("lsp-hover-split");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        app.perform(Action::Editor(EditorAction::SplitRight));
        goto(&mut app, 1, 21);
        draw(&mut app, 100, 24);
        act(&mut app, LspAction::QuickDoc);
        answer(&mut app, &mut server, "textDocument/hover", hover("doc")).await;
        app.toasts = Default::default();
        let buf = draw(&mut app, 100, 24);
        let (cx, cy) = app.editor.caret_cell().unwrap();
        assert!(cx > 66, "the caret of the right group");
        let x0 = (0..100).find(|&x| buf[(x, cy + 1)].symbol() == "┌").unwrap();
        assert!(x0 > 66, "{x0}");
        app.perform(Action::Editor(EditorAction::PrevGroup));
        draw(&mut app, 100, 24);
        assert!(app.hover.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hover_answer_for_another_group_is_dropped() {
        let (mut app, dir) = test_app("lsp-hover-other-group");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        goto(&mut app, 1, 21);
        draw(&mut app, 100, 24);
        act(&mut app, LspAction::QuickDoc);
        // The split focuses a new group showing the same buffer at the same caret.
        app.perform(Action::Editor(EditorAction::SplitRight));
        draw(&mut app, 100, 24);
        answer(&mut app, &mut server, "textDocument/hover", hover("doc")).await;
        assert!(app.hover.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
