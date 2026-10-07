//! Language server edits and structure: rename (Shift+F6), context actions
//! (Alt+Enter), File Structure (Ctrl+F12), and applying workspace edits,
//! the servers' answers and their `workspace/applyEdit` requests alike.
//! Formatting through a server is in `app/format.rs`.
//!
//! A workspace edit is checked and planned against the open buffers first
//! (`lsp::edits::plan_workspace`): an open buffer the server saw at another
//! version, overlapping edits or a file operation refuse the whole edit, so
//! nothing changes. Then each open buffer gets one transaction (one undo
//! step, left unsaved), and the files that are not open are edited on disk
//! off the main thread (`AppMsg::EditsApplied`), as replace in files does.
//! Undo reverts the open buffers only.

use lsp_types::{CodeAction, CodeActionOrCommand, Command, PrepareRenameResponse, WorkspaceEdit};
use serde_json::{Value, json};

use super::App;
use super::lsp_nav::{CODE_ACTION, CODE_ACTION_RESOLVE, DOCUMENT_SYMBOL, EXECUTE_COMMAND, PREPARE_RENAME, RENAME, word_at};
use crate::editor::buffer::LineEnding;
use crate::editor::undo::TxKind;
use crate::event::{AppMsg, Effect, Level};
use crate::lsp::edits::{self, EditCounts, OpenDoc};
use crate::lsp::locations::LocationItem;
use crate::lsp::{DocTag, EditOrigin, PendingTag, pos, symbols};
use crate::ui::dialog::{Dialog, DialogId};

/// Rename and context action requests (in `LspService`).
#[derive(Default)]
pub(super) struct EditReqs {
    /// The rename in flight: its seq and the new name.
    rename: Option<(u64, String)>,
    /// The context actions in the dialog, with the seq of their request.
    actions: Option<(u64, Vec<CodeActionOrCommand>)>,
}

impl App {
    /// `lsp.rename` (Shift+F6): ask the server what can be renamed at the
    /// caret when it says (`prepareRename`), else take the word there; then
    /// ask for the new name.
    pub(super) fn lsp_rename(&mut self) {
        self.hover = None;
        self.completion_close();
        let Some((id, pos)) = self.caret_position() else { return };
        let result = if self.lsp_check(id, PREPARE_RENAME).is_ok() {
            self.lsp_doc_request(id, pos, PREPARE_RENAME, json!({}), PendingTag::PrepareRename).map(|_| ())
        } else {
            self.lsp_check(id, RENAME).map(|_| {
                let Some(b) = self.editor.active_buffer() else { return };
                let (word, version) = (word_at(b, b.view.sel().head), b.version);
                if word.is_empty() {
                    return self.lsp_hint("Cannot rename this element");
                }
                self.lsp_rename_dialog(DocTag { buffer: id, version, pos, seq: 0 }, &word);
            })
        };
        if let Err(why) = result
            && !why.is_empty()
        {
            self.lsp_hint(why);
        }
    }

    pub(super) fn lsp_prepare_rename_answer(&mut self, t: DocTag, value: Value) {
        let Some(b) = self.editor.active_buffer() else { return };
        let name = match serde_json::from_value::<Option<PrepareRenameResponse>>(value).ok().flatten() {
            None => return self.lsp_hint("Cannot rename this element"),
            Some(PrepareRenameResponse::Range(r)) => {
                let (start, end) = (pos::char_index(&b.rope, r.start), pos::char_index(&b.rope, r.end));
                b.rope.slice(start..end.max(start)).to_string()
            }
            Some(PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. }) => placeholder,
            Some(PrepareRenameResponse::DefaultBehavior { .. }) => word_at(b, b.view.sel().head),
        };
        self.lsp_rename_dialog(t, &name);
    }

    fn lsp_rename_dialog(&mut self, t: DocTag, name: &str) {
        if self.dialog.is_some() {
            return;
        }
        let msg = format!("Rename '{name}' and its usages to:");
        self.dialog = Some(Dialog::input(DialogId::LspRename(t), "Rename", &msg, name, "Refactor"));
        self.dispatcher.reset();
    }

    /// The rename dialog's answer: rename what is at `t` to `name`, unless
    /// the text or the caret moved meanwhile.
    pub(super) fn lsp_rename_to(&mut self, t: DocTag, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let same = self.editor.active_buffer().is_some_and(|b| b.version == t.version);
        if !same || self.caret_position() != Some((t.buffer, t.pos)) {
            return self.lsp_hint("The file changed; rename again");
        }
        match self.lsp_doc_request(t.buffer, t.pos, RENAME, json!({"newName": name}), PendingTag::Rename) {
            Ok(seq) => self.lsp.edit.rename = Some((seq, name)),
            Err(why) if !why.is_empty() => self.lsp_hint(why),
            Err(_) => {}
        }
    }

    pub(super) fn lsp_rename_answer(&mut self, t: DocTag, value: Value) {
        let Some((_, name)) = self.lsp.edit.rename.take_if(|r| r.0 == t.seq) else { return };
        match serde_json::from_value::<Option<WorkspaceEdit>>(value) {
            Ok(Some(edit)) => {
                self.apply_workspace_edit(edit, EditOrigin::Rename(name));
            }
            Ok(None) => self.lsp_hint("Nothing to rename"),
            Err(e) => self.lsp_hint(format!("Rename: unexpected answer ({e})")),
        }
    }

    /// `lsp.context_actions` (Alt+Enter): the server's quick fixes and
    /// refactorings for the selection (or the caret), with the diagnostics
    /// there.
    pub(super) fn lsp_context_actions(&mut self) {
        self.hover = None;
        self.completion_close();
        let Some(b) = self.editor.active_buffer() else { return };
        let r = b.view.sel().range();
        let range = lsp_types::Range::new(pos::position(&b.rope, r.start), pos::position(&b.rope, r.end));
        let (id, head) = (b.id, pos::position(&b.rope, b.view.sel().head));
        let key = |p: lsp_types::Position| (p.line, p.character);
        let diagnostics: Vec<lsp_types::Diagnostic> = b
            .path
            .as_deref()
            .map(|p| self.lsp.diagnostics.get(p))
            .unwrap_or_default()
            .iter()
            .filter(|d| key(d.range.start) <= key(range.end) && key(range.start) <= key(d.range.end))
            .map(|d| d.to_lsp())
            .collect();
        let params = json!({"range": range, "context": {"diagnostics": diagnostics, "triggerKind": 1}});
        if let Err(why) = self.lsp_tagged_request(id, head, CODE_ACTION, params, PendingTag::CodeAction)
            && !why.is_empty()
        {
            self.lsp_hint(why);
        }
    }

    pub(super) fn lsp_code_actions_answer(&mut self, t: DocTag, value: Value) {
        let items: Vec<CodeActionOrCommand> = serde_json::from_value::<Option<Vec<CodeActionOrCommand>>>(value)
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !matches!(a, CodeActionOrCommand::CodeAction(c) if c.disabled.is_some()))
            .collect();
        if items.is_empty() {
            return self.lsp_hint("No context actions here");
        }
        if self.dialog.is_some() {
            return;
        }
        let titles = items
            .iter()
            .map(|a| match a {
                CodeActionOrCommand::Command(c) => c.title.clone(),
                CodeActionOrCommand::CodeAction(a) => a.title.clone(),
            })
            .collect();
        self.lsp.edit.actions = Some((t.seq, items));
        self.dialog = Some(Dialog::choice(DialogId::LspCodeAction(t), "Context Actions", "", titles));
        self.dispatcher.reset();
    }

    /// Item `i` of the context actions dialog was chosen: its edit is made
    /// and its command run (resolving it first when the server left both
    /// out and resolves).
    pub(super) fn lsp_code_action_chosen(&mut self, t: DocTag, i: usize) {
        let Some((_, items)) = self.lsp.edit.actions.take_if(|a| a.0 == t.seq) else { return };
        match items.into_iter().nth(i) {
            Some(CodeActionOrCommand::Command(c)) => self.lsp_execute(c),
            Some(CodeActionOrCommand::CodeAction(a)) if a.edit.is_none() && a.command.is_none() => {
                if self.lsp_check(t.buffer, CODE_ACTION_RESOLVE).is_err() {
                    return self.lsp_hint(format!("{}: nothing to do", a.title));
                }
                let Some((id, pos)) = self.caret_position() else { return };
                let params = serde_json::to_value(&a).unwrap_or_default();
                if let Err(why) = self.lsp_tagged_request(id, pos, CODE_ACTION_RESOLVE, params, PendingTag::CodeActionResolve)
                    && !why.is_empty()
                {
                    self.lsp_hint(why);
                }
            }
            Some(CodeActionOrCommand::CodeAction(a)) => self.lsp_run_code_action(a),
            None => {}
        }
    }

    pub(super) fn lsp_code_action_resolved(&mut self, _t: DocTag, value: Value) {
        match serde_json::from_value::<CodeAction>(value) {
            Ok(a) if a.edit.is_some() || a.command.is_some() => self.lsp_run_code_action(a),
            Ok(a) => self.lsp_hint(format!("{}: nothing to do", a.title)),
            Err(e) => self.lsp_hint(format!("Context action: unexpected answer ({e})")),
        }
    }

    /// Make a code action's edit, then run its command.
    fn lsp_run_code_action(&mut self, a: CodeAction) {
        if let Some(edit) = a.edit
            && !self.apply_workspace_edit(edit, EditOrigin::CodeAction(a.title.clone()))
        {
            return;
        }
        if let Some(c) = a.command {
            self.lsp_execute(c);
        }
    }

    /// `workspace/executeCommand` for a command the active buffer's server
    /// runs; what it changes arrives as `workspace/applyEdit`.
    fn lsp_execute(&mut self, c: Command) {
        let Some((id, pos)) = self.caret_position() else { return };
        let runs = self.lsp_caps(id).is_some_and(|caps| caps.runs_command(&c.command));
        if !runs {
            return self.lsp_hint(format!("{}: the command {} is not supported", c.title, c.command));
        }
        let mut params = json!({"command": c.command});
        if let Some(args) = c.arguments {
            params["arguments"] = Value::Array(args);
        }
        if let Err(why) = self.lsp_tagged_request(id, pos, EXECUTE_COMMAND, params, PendingTag::ExecuteCommand)
            && !why.is_empty()
        {
            self.lsp_hint(why);
        }
    }

    /// `lsp.file_structure` (Ctrl+F12): the file's symbols from its server
    /// in the Locations palette, nested by indentation.
    pub(super) fn lsp_file_structure(&mut self) {
        self.hover = None;
        self.completion_close();
        let Some((id, pos)) = self.caret_position() else { return };
        if let Err(why) = self.lsp_tagged_request(id, pos, DOCUMENT_SYMBOL, json!({}), PendingTag::DocumentSymbols)
            && !why.is_empty()
        {
            self.lsp_hint(why);
        }
    }

    pub(super) fn lsp_document_symbols_answer(&mut self, _t: DocTag, value: Value) {
        let rows = symbols::rows(value);
        let Some(b) = self.editor.active_buffer() else { return };
        let Some(path) = b.path.clone() else { return };
        if rows.is_empty() {
            return self.lsp_hint("No symbols in this file");
        }
        let items = rows
            .into_iter()
            .map(|r| {
                let line = r.pos.line as usize;
                let text = if line < b.rope.len_lines() { b.line_text(line) } else { String::new() };
                let col = pos::line_char_col(&text, r.pos.character);
                let shown = format!("{}{} {}", "  ".repeat(r.depth), symbols::kind_glyph(r.kind), r.name);
                LocationItem { path: path.clone(), line, col, text: shown }
            })
            .collect();
        let title = format!("Structure of {}", b.name);
        self.show_locations(title, items);
    }

    /// Apply a server's workspace edit (see the module docs). Returns false
    /// when it was refused and nothing changed.
    pub(super) fn apply_workspace_edit(&mut self, edit: WorkspaceEdit, origin: EditOrigin) -> bool {
        // The documents' versions as the server knows them.
        self.lsp_sync();
        let files = match edits::workspace_files(edit) {
            Ok(files) => files,
            Err(why) => {
                self.edits_refused(origin, why);
                return false;
            }
        };
        let open: Vec<OpenDoc> = files
            .iter()
            .filter_map(|f| {
                let b = &self.editor.buffers[self.editor.index_of(&f.path)?];
                Some(OpenDoc {
                    id: b.id,
                    path: f.path.clone(),
                    name: b.name.clone(),
                    rope: b.rope.clone(),
                    crlf: b.line_ending == LineEnding::CrLf,
                    version: self.lsp.docs.get(&b.id).map(|d| d.version),
                })
            })
            .collect();
        let plan = match edits::plan_workspace(files, &open) {
            Ok(plan) => plan,
            Err(why) => {
                self.edits_refused(origin, why);
                return false;
            }
        };
        let mut done = EditCounts::default();
        for (id, plan, n) in plan.open {
            let Some(i) = self.editor.index_of_id(id) else { continue };
            if n == 0 {
                continue;
            }
            let b = &mut self.editor.buffers[i];
            let mut sels = b.view.sels.clone();
            sels.map(|p| edits::map_pos(&plan.changes, p));
            b.apply(plan.changes, sels, TxKind::Other);
            done.files += 1;
            done.edits += n;
        }
        self.dirty_frame = true;
        if plan.closed.is_empty() {
            self.edits_applied(origin, done, Vec::new());
            return true;
        }
        let (tx, generation) = (self.bus_tx.clone(), self.workspace.generation);
        tokio::task::spawn_blocking(move || {
            let mut errors = Vec::new();
            for f in plan.closed {
                match edits::edit_file(&f.path, &f.edits) {
                    Ok(0) => {}
                    Ok(n) => {
                        done.files += 1;
                        done.edits += n;
                    }
                    Err(e) => errors.push(e),
                }
            }
            let _ = tx.send(AppMsg::EditsApplied { generation, origin, done, errors });
        });
        true
    }

    /// What a workspace edit is called in messages.
    fn edit_title(origin: &EditOrigin) -> String {
        match origin {
            EditOrigin::Rename(_) => "Rename".into(),
            EditOrigin::CodeAction(title) => title.clone(),
            EditOrigin::ServerRequest { server, label, .. } => label.clone().unwrap_or_else(|| format!("Edit from {server}")),
        }
    }

    fn edits_refused(&mut self, origin: EditOrigin, why: String) {
        self.notify(Level::Warn, format!("{}: {why}; nothing was changed", Self::edit_title(&origin)));
        if let EditOrigin::ServerRequest { server, id, .. } = origin
            && let Some(c) = self.lsp.clients.get(&server)
        {
            c.respond(id, Ok(json!({"applied": false, "failureReason": why})));
        }
    }

    /// A workspace edit was made (`AppMsg::EditsApplied` for one with files
    /// that were not open): report it, answer the server that asked for it.
    pub(super) fn edits_applied(&mut self, origin: EditOrigin, done: EditCounts, errors: Vec<String>) {
        let title = Self::edit_title(&origin);
        for e in errors.iter().take(3) {
            self.notify(Level::Error, format!("{title}: {e}"));
        }
        if errors.len() > 3 {
            self.notify(Level::Error, format!("{title}: {} more errors (see the log)", errors.len() - 3));
            for e in &errors[3..] {
                tracing::error!("{title}: {e}");
            }
        }
        let s = |n: usize| if n == 1 { "" } else { "s" };
        match origin {
            EditOrigin::Rename(name) if done.files > 0 => self.notify(
                Level::Info,
                format!("Renamed to {name}: {} occurrence{} in {} file{}", done.edits, s(done.edits), done.files, s(done.files)),
            ),
            EditOrigin::Rename(_) if errors.is_empty() => self.lsp_hint("Nothing to rename"),
            EditOrigin::Rename(_) | EditOrigin::CodeAction(_) => {}
            EditOrigin::ServerRequest { server, id, .. } => {
                if let Some(c) = self.lsp.clients.get(&server) {
                    let mut answer = json!({"applied": errors.is_empty()});
                    if let Some(e) = errors.first() {
                        answer["failureReason"] = e.clone().into();
                    }
                    c.respond(id, Ok(answer));
                }
            }
        }
        if done.files > 0 {
            self.apply_effects(vec![Effect::GitRefresh]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, LspAction, PaletteKind};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::fake::FakeServer;
    use crate::lsp::sync;
    use crate::ui::dialog::{DialogKind, DialogResult};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::Path;

    /// Rename (with prepare), context actions (resolved lazily), one
    /// command, file structure and formatting on top of incremental sync.
    fn caps() -> Value {
        let mut c = incremental();
        c["renameProvider"] = json!({"prepareProvider": true});
        c["codeActionProvider"] = json!({"resolveProvider": true});
        c["executeCommandProvider"] = json!({"commands": ["fix.all"]});
        c["documentSymbolProvider"] = json!(true);
        c["documentFormattingProvider"] = json!(true);
        c
    }

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    fn edit(l0: u32, c0: u32, l1: u32, c1: u32, text: &str) -> Value {
        json!({"range": range(l0, c0, l1, c1), "newText": text})
    }

    fn uri(path: &Path) -> String {
        sync::uri(path).unwrap().as_str().to_string()
    }

    fn text(app: &App) -> String {
        app.editor.active_buffer().unwrap().text()
    }

    fn caret(app: &App) -> (usize, usize) {
        app.editor.active_buffer().unwrap().cursor_line_col()
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    async fn answer(app: &mut App, server: &mut FakeServer, method: &str, result: Value) -> Value {
        let req = server.recv(method).await;
        server.respond(req["id"].as_i64().unwrap(), result);
        handle_next(app, is_lsp).await;
        req
    }

    fn applied(m: &AppMsg) -> bool {
        matches!(m, AppMsg::EditsApplied { .. })
    }

    /// The open file: `old` called after an emoji (two UTF-16 units).
    const A: &str = "fn main() {\n    let s = \"😀\"; old(s);\n}\n";
    /// The file that is not open: the definition and a comment.
    const B: &str = "pub fn old(s: &str) {}\n// 日本 old\n";

    #[test]
    fn capabilities_advertise_edits() {
        let c = serde_json::to_value(crate::lsp::client::client_capabilities()).unwrap();
        let td = &c["textDocument"];
        assert_eq!(td["rename"]["prepareSupport"], true);
        assert_eq!(td["codeAction"]["resolveSupport"]["properties"], json!(["edit"]));
        assert!(
            td["codeAction"]["codeActionLiteralSupport"]["codeActionKind"]["valueSet"].as_array().unwrap().contains(&json!("quickfix"))
        );
        assert_eq!(td["documentSymbol"]["hierarchicalDocumentSymbolSupport"], true);
        assert!(td["formatting"].is_object());
        assert_eq!(c["workspace"]["applyEdit"], true);
        assert_eq!(c["workspace"]["workspaceEdit"]["documentChanges"], true);
        assert!(c["workspace"]["workspaceEdit"].get("resourceOperations").is_none(), "file operations are not supported");
    }

    #[tokio::test]
    async fn rename_edits_the_open_buffer_in_one_step_and_the_closed_file_on_disk() {
        let (mut app, dir) = test_app("lsp-rename");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let (a, b) = (app.workspace.root.join("a.rs"), app.workspace.root.join("b.rs"));
        std::fs::write(&b, B).unwrap();
        // On `old` after the emoji: char 18, UTF-16 unit 19.
        app.editor.active_buffer_mut().unwrap().goto_line(1, 18);
        app.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::SHIFT));
        let req =
            answer(&mut app, &mut server, "textDocument/prepareRename", json!({"range": range(1, 18, 1, 21), "placeholder": "old"})).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 19}));
        let d = app.dialog.take().expect("the rename dialog");
        match &d.kind {
            DialogKind::Input { input, .. } => assert_eq!(input.text, "old"),
            k => panic!("{k:?}"),
        }
        app.finish_dialog(d.id, DialogResult::Text(" renamed ".into()));
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"textDocument": {"uri": uri(&b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed"), edit(1, 6, 1, 9, "renamed")]},
        ]});
        let req = answer(&mut app, &mut server, "textDocument/rename", we).await;
        assert_eq!(req["params"]["newName"], "renamed");
        // The open buffer changed at once, unsaved, the caret on the name.
        assert_eq!(text(&app), "fn main() {\n    let s = \"😀\"; renamed(s);\n}\n");
        assert!(app.editor.active_buffer().unwrap().dirty());
        assert_eq!(caret(&app), (1, 18));
        // The closed one on disk, off the main thread.
        handle_next(&mut app, applied).await;
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 renamed\n");
        assert!(toasts(&app).contains(&"Renamed to renamed: 3 occurrences in 2 files".to_string()), "{:?}", toasts(&app));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), A, "the open file is not saved");
        // One undo step reverts the open buffer; the other file stays.
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&app), A);
        assert!(std::fs::read_to_string(&b).unwrap().contains("renamed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rename_rejections_and_a_stale_version_change_nothing() {
        let (mut app, dir) = test_app("lsp-rename-no");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let (a, b) = (app.workspace.root.join("a.rs"), app.workspace.root.join("b.rs"));
        std::fs::write(&b, B).unwrap();
        app.toasts = Default::default();
        app.editor.active_buffer_mut().unwrap().goto_line(1, 18);
        // The server refuses: its reason as a toast, no dialog.
        app.perform(Action::Lsp(LspAction::Rename));
        let req = server.recv("textDocument/prepareRename").await;
        let id = req["id"].as_i64().unwrap();
        server.respond_error(id, -32602, "Cannot rename a built-in");
        handle_next(&mut app, is_lsp).await;
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"rust-analyzer: Cannot rename a built-in".to_string()), "{:?}", toasts(&app));
        // Null: nothing to rename here.
        app.perform(Action::Lsp(LspAction::Rename));
        answer(&mut app, &mut server, "textDocument/prepareRename", Value::Null).await;
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"Cannot rename this element".to_string()));
        // A range answer: its text is the name offered.
        app.perform(Action::Lsp(LspAction::Rename));
        answer(&mut app, &mut server, "textDocument/prepareRename", range(1, 18, 1, 21)).await;
        let d = app.dialog.take().unwrap();
        assert!(matches!(&d.kind, DialogKind::Input { input, .. } if input.text == "old"));
        app.finish_dialog(d.id, DialogResult::Text("renamed".into()));
        // An edit for another version of the open file changes no file.
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed")]},
            {"textDocument": {"uri": uri(&a), "version": 7}, "edits": [edit(1, 18, 1, 21, "renamed")]},
        ]});
        answer(&mut app, &mut server, "textDocument/rename", we).await;
        assert_eq!(text(&app), A);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), B);
        let warn = app.notifications.last().unwrap();
        assert_eq!(
            (warn.level, warn.text.as_str()),
            (Level::Warn, "Rename: a.rs changed since the language server made the edit; nothing was changed")
        );
        // Creating a file is not supported: nothing changes either.
        app.perform(Action::Lsp(LspAction::Rename));
        answer(&mut app, &mut server, "textDocument/prepareRename", range(1, 18, 1, 21)).await;
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Text("renamed".into()));
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"kind": "create", "uri": uri(&app.workspace.root.join("c.rs"))},
        ]});
        answer(&mut app, &mut server, "textDocument/rename", we).await;
        assert_eq!(text(&app), A);
        assert!(app.notifications.last().unwrap().text.contains("creating a file"), "{}", app.notifications.last().unwrap().text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn without_prepare_the_word_at_the_caret_is_offered() {
        let (mut app, dir) = test_app("lsp-rename-word");
        let mut c = incremental();
        c["renameProvider"] = json!(true);
        let mut servers = with_fake_servers(&mut app, c);
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 19);
        app.perform(Action::Lsp(LspAction::Rename));
        let d = app.dialog.take().expect("asked at once");
        assert!(matches!(&d.kind, DialogKind::Input { input, .. } if input.text == "old"));
        app.finish_dialog(d.id, DialogResult::Text("x".into()));
        // A `changes` answer for the open file only: applied at once.
        let a = app.workspace.root.join("a.rs");
        let mut changes = serde_json::Map::new();
        changes.insert(uri(&a), json!([edit(1, 18, 1, 21, "x")]));
        answer(&mut app, &mut server, "textDocument/rename", json!({"changes": changes})).await;
        assert_eq!(text(&app), "fn main() {\n    let s = \"😀\"; x(s);\n}\n");
        assert!(toasts(&app).contains(&"Renamed to x: 1 occurrence in 1 file".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn context_actions_apply_edits_resolve_and_run_commands() {
        let (mut app, dir) = test_app("lsp-actions");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        // A diagnostic on `old`, and one elsewhere.
        let diag = |l: u32, c0: u32, c1: u32, code: Value| json!({"range": range(l, c0, l, c1), "severity": 1, "message": "unresolved", "code": code, "data": {"fix": l}});
        server.notify(
            "textDocument/publishDiagnostics",
            json!({"uri": uri(&a), "diagnostics": [diag(1, 18, 21, json!(425)), diag(0, 3, 7, json!("x"))]}),
        );
        handle_next(&mut app, is_lsp).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 19);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        let fix = json!({"title": "Import old", "kind": "quickfix", "edit": {"changes": {uri(&a): [edit(0, 0, 0, 0, "use m::old;\n")]}}});
        let lazy = json!({"title": "Inline", "kind": "refactor.inline", "data": {"id": 3}});
        let cmd = json!({"title": "Fix all", "command": "fix.all", "arguments": [1]});
        let off = json!({"title": "Disabled", "disabled": {"reason": "no"}});
        let req = answer(&mut app, &mut server, "textDocument/codeAction", json!([fix, lazy, cmd, off])).await;
        assert_eq!(req["params"]["range"], range(1, 20, 1, 20));
        let sent = &req["params"]["context"]["diagnostics"];
        assert_eq!(sent.as_array().unwrap().len(), 1, "only the one at the caret: {sent}");
        assert_eq!((sent[0]["code"].clone(), sent[0]["data"].clone()), (json!(425), json!({"fix": 1})));
        let d = app.dialog.take().expect("the actions");
        match &d.kind {
            DialogKind::Choice { items, .. } => assert_eq!(items, &["Import old", "Inline", "Fix all"]),
            k => panic!("{k:?}"),
        }
        // An action with an edit: made as one undo step.
        app.finish_dialog(d.id, DialogResult::Choice(0));
        assert_eq!(text(&app), format!("use m::old;\n{A}"));
        assert_eq!(caret(&app), (2, 19), "the caret stays on its code");
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&app), A);

        // One without its edit is resolved first.
        app.perform(Action::Lsp(LspAction::ContextActions));
        answer(&mut app, &mut server, "textDocument/codeAction", json!([lazy])).await;
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Choice(0));
        let mut resolved = lazy.clone();
        resolved["edit"] = json!({"changes": {uri(&a): [edit(1, 18, 1, 24, "\"😀\"")]}});
        let req = answer(&mut app, &mut server, "codeAction/resolve", resolved).await;
        assert_eq!(req["params"]["data"], json!({"id": 3}));
        assert!(req["params"].get("textDocument").is_none());
        assert_eq!(text(&app), "fn main() {\n    let s = \"😀\"; \"😀\";\n}\n");
        app.perform(Action::Editor(EditorAction::Undo));

        // A command runs on the server, which edits through applyEdit and
        // is answered once the edit is made.
        app.perform(Action::Lsp(LspAction::ContextActions));
        answer(&mut app, &mut server, "textDocument/codeAction", json!([cmd])).await;
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Choice(0));
        let exec = server.recv("workspace/executeCommand").await;
        assert_eq!(exec["params"], json!({"command": "fix.all", "arguments": [1]}));
        server.request(
            41,
            "workspace/applyEdit",
            json!({"label": "Fix all", "edit": {"changes": {uri(&a): [edit(2, 0, 2, 1, "} // fixed")]}}}),
        );
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(41).await["result"], json!({"applied": true}));
        assert!(text(&app).ends_with("} // fixed\n"));
        server.respond(exec["id"].as_i64().unwrap(), Value::Null);
        handle_next(&mut app, is_lsp).await;
        // An edit it cannot make is answered with the reason.
        server.request(42, "workspace/applyEdit", json!({"edit": {"documentChanges": [{"kind": "delete", "uri": uri(&a)}]}}));
        handle_next(&mut app, is_lsp).await;
        let r = server.response(42).await["result"].clone();
        assert_eq!(r["applied"], false);
        assert!(r["failureReason"].as_str().unwrap().contains("deleting a file"), "{r}");
        // Nothing offered: a toast.
        app.perform(Action::Lsp(LspAction::ContextActions));
        answer(&mut app, &mut server, "textDocument/codeAction", json!([])).await;
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"No context actions here".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_structure_lists_both_symbol_forms_nested() {
        let (mut app, dir) = test_app("lsp-symbols");
        let mut servers = with_fake_servers(&mut app, caps());
        let text = "struct S {\n    日本: u8,\n}\nfn main() {}\n";
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", text).await;
        let a = app.workspace.root.join("a.rs");
        app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::CONTROL));
        let tree = json!([
            {"name": "main", "kind": 12, "range": range(3, 0, 3, 12), "selectionRange": range(3, 3, 3, 7)},
            {"name": "S", "kind": 23, "range": range(0, 0, 2, 1), "selectionRange": range(0, 7, 0, 8), "children": [
                {"name": "日本", "kind": 8, "range": range(1, 4, 1, 12), "selectionRange": range(1, 4, 1, 6)},
            ]},
        ]);
        answer(&mut app, &mut server, "textDocument/documentSymbol", tree).await;
        let p = app.palette.as_ref().expect("the palette");
        assert_eq!(p.kind, PaletteKind::Locations);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let buf = term.draw(|f| crate::ui::render(&mut app, f)).unwrap().buffer.clone();
        let rows: Vec<String> = (0..24).map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect()).collect();
        assert!(rows.iter().any(|r| r.contains("Structure of a.rs")), "{rows:#?}");
        let at = |needle: &str| rows.iter().find_map(|r| r.find(needle).map(|i| r[..i].chars().count()));
        let (s, field, main) = (at("S S").unwrap(), at("d 日").unwrap(), at("f main").unwrap());
        assert_eq!((field - s, main), (2, s), "the field is indented under its struct: {rows:#?}");
        assert!(rows.iter().any(|r| r.contains("f main") && r.contains("a.rs:4")), "{rows:#?}");
        // Down to the field; Enter jumps to its name.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.palette.is_none());
        assert_eq!(caret(&app), (1, 4));
        // The flat form nests by ranges.
        app.perform(Action::Lsp(LspAction::FileStructure));
        let loc = |r: Value| json!({"uri": uri(&a), "range": r});
        let flat = json!([
            {"name": "main", "kind": 12, "location": loc(range(3, 0, 3, 12))},
            {"name": "日本", "kind": 8, "location": loc(range(1, 4, 1, 12)), "containerName": "S"},
            {"name": "S", "kind": 23, "location": loc(range(0, 0, 2, 1))},
        ]);
        answer(&mut app, &mut server, "textDocument/documentSymbol", flat).await;
        let buf = term.draw(|f| crate::ui::render(&mut app, f)).unwrap().buffer.clone();
        let rows: Vec<String> = (0..24).map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect()).collect();
        let at = |needle: &str| rows.iter().find_map(|r| r.find(needle).map(|i| r[..i].chars().count()));
        assert_eq!(at("d 日").unwrap() - at("S S").unwrap(), 2, "{rows:#?}");
        // No symbols: a toast.
        app.palette = None;
        app.perform(Action::Lsp(LspAction::FileStructure));
        answer(&mut app, &mut server, "textDocument/documentSymbol", json!([])).await;
        assert!(app.palette.is_none());
        assert!(toasts(&app).contains(&"No symbols in this file".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
