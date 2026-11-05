//! Language server edits and structure: rename (Shift+F6), context actions
//! (Alt+Enter), Refactor This (Alt+Shift+T) and Optimize Imports (Ctrl+Alt+O), the same request
//! limited to a kind, File Structure (Ctrl+F12), and applying workspace edits,
//! the servers' answers and their `workspace/applyEdit` requests alike.
//! Formatting through a server is in `app/format.rs`; the lightbulb (quick
//! fixes on the caret line) in `app/lightbulb.rs`. Context actions list the
//! server's preferred ones first, so Enter runs the preferred fix.
//!
//! A workspace edit is checked and planned against the open buffers and
//! the disk first (`lsp::edits::plan_workspace`, its file operations
//! simulated in order): an open buffer the server saw at another version,
//! overlapping edits, or a file operation its options, its path (outside
//! the project, inside `.git`) or an open tab (unsaved, or edited and then
//! deleted or replaced) do not allow refuse the whole edit, so nothing
//! changes. Then clean tabs of deleted or replaced files close, each open
//! buffer gets one transaction (one undo step, left unsaved), and the disk
//! steps (files that are not open edited, files created, renamed and
//! deleted) run in order off the main thread, stopping at the first that
//! fails (`AppMsg::EditsApplied`). Renamed files' tabs follow them, with
//! their local history; created files are not opened.
//!
//! A server's rename is not applied at once (`[lsp] rename_preview`, on by
//! default): its answer is built into Find results, one per text edit
//! (`files::rename_preview`), and Replace All (Alt+A) asks and applies the
//! included edits and every file operation through the same plan and apply
//! (`apply_steps_now`). The rename dialog's option "Search in comments and
//! strings" adds guesses to the preview, excluded and marked `text`; the
//! option is kept for the session (`EditReqs::rename_in_text`). A file that changed since the preview refuses the
//! whole rename (`RenamePreview::first_stale`). With the key off the answer
//! applies at once.
//!
//! Without a server document for the buffer (and no server starting),
//! rename is textual: the name goes to Find in files (case and whole word)
//! with the new name as replacement, the uses and definitions the symbol
//! index knows included and other results excluded; nothing changes until
//! Replace All.
//!
//! Undo reverts the open buffers only: file operations and edits of files
//! that were not open cannot be undone. Local History keeps a snapshot of
//! deleted and replaced files, and carries a renamed file's history.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_types::{CodeAction, CodeActionOrCommand, Command, PrepareRenameResponse, WorkspaceEdit};
use serde_json::{Value, json};

use super::lsp_nav::{CODE_ACTION, CODE_ACTION_RESOLVE, DOCUMENT_SYMBOL, EXECUTE_COMMAND, PREPARE_RENAME, RENAME, word_at};
use super::{App, Ctx};
use crate::editor::buffer::{Buffer, BufferId, LineEnding};
use crate::editor::undo::TxKind;
use crate::event::{AppMsg, Effect, Level};
use crate::files::find::{Hit, MAX_HITS};
use crate::files::ops::FsOp;
use crate::files::rename_preview::{self, OpenText, RenamePreview, RenameRequest, TextualScope, TextualStage};
use crate::history::Ack;
use crate::lsp::edits::{self, DiskStep, EditCounts, OpenDoc};
use crate::lsp::locations::LocationItem;
use crate::lsp::{DocTag, EditOrigin, PendingTag, pos, symbols};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::{PanelId, ToolWindowId};

/// The confirmation dialog's text: the annotations (at most 8) and the number of changes.
fn confirm_message(title: &str, rows: &[edits::AnnotationRow], total: usize) -> String {
    let mut out = format!("{title} asks to confirm these changes:");
    for r in rows.iter().take(8) {
        let mark = if r.needs_confirmation { '*' } else { ' ' };
        out.push_str(&format!("\n{mark} {}", r.label));
        if let Some(d) = r.description.as_deref().filter(|d| !d.is_empty()) {
            out.push_str(&format!(" - {d}"));
        }
    }
    if rows.len() > 8 {
        out.push_str(&format!("\n  ... and {} more", rows.len() - 8));
    }
    out.push_str(&format!("\n{total} change{} in all. Apply all of them?", if total == 1 { "" } else { "s" }));
    out
}

/// Put the server's preferred code actions first, keeping the order otherwise.
fn preferred_first(items: &mut [CodeActionOrCommand]) {
    items.sort_by_key(|a| !matches!(a, CodeActionOrCommand::CodeAction(c) if c.is_preferred == Some(true)));
}

/// Which code-action request an answer belongs to: Alt+Enter's (everything),
/// Refactor This's or Optimize Imports'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionsFor {
    Context,
    Refactor,
    OrganizeImports,
}

impl ActionsFor {
    /// The kind sent as `context.only` and kept from the answer; `None`: all.
    fn only(self) -> Option<&'static str> {
        match self {
            ActionsFor::Context => None,
            ActionsFor::Refactor => Some("refactor"),
            ActionsFor::OrganizeImports => Some("source.organizeImports"),
        }
    }

    fn title(self) -> &'static str {
        match self {
            ActionsFor::Context => "Context Actions",
            ActionsFor::Refactor => "Refactor This",
            ActionsFor::OrganizeImports => "Optimize Imports",
        }
    }

    fn none_hint(self) -> &'static str {
        match self {
            ActionsFor::Context => "No context actions here",
            ActionsFor::Refactor => "No refactorings here",
            ActionsFor::OrganizeImports => "Nothing to optimize",
        }
    }

    fn tag(self) -> fn(DocTag) -> PendingTag {
        match self {
            ActionsFor::Context => PendingTag::CodeAction,
            ActionsFor::Refactor => PendingTag::Refactor,
            ActionsFor::OrganizeImports => PendingTag::OrganizeImports,
        }
    }

    /// Whether `item` is of this request's kind: always for `Context`; else a
    /// `CodeAction` whose `kind` equals `only` or starts with `only` + "."
    /// (a bare `Command` has no kind and is dropped).
    fn keeps(self, item: &CodeActionOrCommand) -> bool {
        let Some(only) = self.only() else { return true };
        let CodeActionOrCommand::CodeAction(a) = item else { return false };
        a.kind.as_ref().is_some_and(|k| {
            let k = k.as_str();
            k == only || k.strip_prefix(only).is_some_and(|rest| rest.starts_with('.'))
        })
    }
}

/// Rename and context action requests (in `LspService`).
#[derive(Default)]
pub(super) struct EditReqs {
    /// The rename in flight: its seq, the old name and the new name.
    rename: Option<(u64, String, String)>,
    /// The name in the rename dialog when it opened.
    rename_old: String,
    /// The rename dialog's "Search in comments and strings", as last answered. Kept for the session, not persisted.
    rename_in_text: bool,
    /// The context actions in the dialog, with the seq of their request.
    actions: Option<(u64, Vec<CodeActionOrCommand>)>,
    /// The edit waiting for the user's confirmation.
    pub(super) confirm: Option<PendingConfirm>,
}

/// A workspace edit with changes that need confirmation, behind `DialogId::LspConfirmEdit(seq)`.
pub(super) struct PendingConfirm {
    seq: u64,
    edit: WorkspaceEdit,
    origin: EditOrigin,
    /// A code action's command, run after the edit.
    then: Option<Command>,
}

impl App {
    /// `lsp.rename` (Shift+F6): ask the server what can be renamed at the
    /// caret when it says (`prepareRename`), else take the word there; then
    /// ask for the new name.
    pub(super) fn lsp_rename(&mut self) {
        self.hover = None;
        self.completion_close();
        let Some((id, pos)) = self.caret_position() else { return };
        self.lsp_sync();
        if !self.lsp.docs.contains_key(&id) && !matches!(self.lsp_status(), Some(super::lsp::LspStatus::Busy(_))) {
            return self.text_rename_dialog(id, pos);
        }
        let result = if self.lsp_check(id, PREPARE_RENAME).is_ok() {
            self.lsp_doc_request(id, pos, PREPARE_RENAME, json!({}), PendingTag::PrepareRename).map(|_| ())
        } else {
            self.lsp_check(id, RENAME).map(|_| {
                let Some(b) = self.editor.active_buffer() else { return };
                let (word, version) = (word_at(b, b.view.sel().head), b.version);
                if word.is_empty() {
                    return self.lsp_hint("Cannot rename this element");
                }
                self.lsp_rename_dialog(DocTag { buffer: id, version, pos, seq: 0, group: self.editor.focused_group().id }, &word);
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
        self.lsp.edit.rename_old = name.to_string();
        let button = if self.config.lsp.rename_preview { "Preview" } else { "Refactor" };
        let mut dialog = Dialog::input(DialogId::LspRename(t), "Rename", &msg, name, button);
        if self.config.lsp.rename_preview {
            dialog = dialog.with_check("Search in comments and strings", self.lsp.edit.rename_in_text);
        }
        self.dialog = Some(dialog);
        self.dispatcher.reset();
    }

    /// The rename dialog's answer with its option: remember the option, then as `lsp_rename_to`.
    pub(super) fn lsp_rename_checked(&mut self, t: DocTag, name: String, in_text: bool) {
        self.lsp.edit.rename_in_text = in_text;
        self.lsp_rename_to(t, name);
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
            Ok(seq) => self.lsp.edit.rename = Some((seq, std::mem::take(&mut self.lsp.edit.rename_old), name)),
            Err(why) if !why.is_empty() => self.lsp_hint(why),
            Err(_) => {}
        }
    }

    /// No server document serves the buffer: ask for the new name of the
    /// word at the caret; the answer previews the rename in Find.
    fn text_rename_dialog(&mut self, id: BufferId, pos: lsp_types::Position) {
        let Some(b) = self.editor.active_buffer() else { return };
        let word = word_at(b, b.view.sel().head);
        if b.path.is_none() || word.is_empty() {
            return self.lsp_hint("Cannot rename this element");
        }
        if self.dialog.is_some() {
            return;
        }
        let t = DocTag { buffer: id, version: b.version, pos, seq: 0, group: self.editor.focused_group().id };
        let msg = format!("Rename '{word}' as text to:");
        self.dialog = Some(Dialog::input(DialogId::TextRename(t), "Rename", &msg, &word, "Preview"));
        self.dispatcher.reset();
    }

    /// The textual rename dialog's answer: open Find with the old name as
    /// query and `name` as replacement, hits outside the index's uses and
    /// definitions excluded. Nothing is written here.
    pub(super) fn text_rename_to(&mut self, t: DocTag, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let same = self.editor.active_buffer().is_some_and(|b| b.version == t.version);
        if !same || self.caret_position() != Some((t.buffer, t.pos)) {
            return self.lsp_hint("The file changed; rename again");
        }
        let Some(b) = self.editor.active_buffer() else { return };
        let (old, path) = (word_at(b, b.view.sel().head), b.path.clone());
        let Some(path) = path.filter(|_| !old.is_empty() && old != name) else { return };
        let keep = self.rename_candidates(&old, &path);
        self.layout.show(ToolWindowId::Find);
        self.focus.set(PanelId::Find);
        let mut cx = ctx!(self);
        self.find.open_rename(&old, &name, keep, &mut cx);
        self.notify(Level::Info, format!("Rename {old} to {name}: review the results, then Replace All (Alt+A)"));
        if self.symbols.indexing() {
            self.notify(Level::Info, "The project is still being indexed; some usages may start excluded");
        }
    }

    /// The positions that start included in a textual rename of `name`: the
    /// uses (`fallback_usages`), the definitions of the name in the index and
    /// the caret's own word in `from`. Absolute paths and char columns, the
    /// same shape `Search` gives `Hit`s (`root.join(rel)`).
    fn rename_candidates(&mut self, name: &str, from: &Path) -> HashMap<PathBuf, HashSet<(usize, usize)>> {
        let mut keep: HashMap<PathBuf, HashSet<(usize, usize)>> = HashMap::new();
        for item in self.fallback_usages(name, from) {
            keep.entry(item.path).or_default().insert((item.line, item.col));
        }
        let root = self.workspace.root.clone();
        for (rel, s) in self.symbols.defs_named(name) {
            keep.entry(root.join(rel)).or_default().insert((s.line, s.col));
        }
        if let Some(b) = self.editor.active_buffer() {
            let start = crate::editor::selection::word_at(&b.rope, b.view.sel().head).start;
            let line = b.rope.char_to_line(start);
            keep.entry(from.to_path_buf()).or_default().insert((line, start - b.rope.line_to_char(line)));
        }
        keep
    }

    pub(super) fn lsp_rename_answer(&mut self, t: DocTag, value: Value) {
        let Some((_, old, name)) = self.lsp.edit.rename.take_if(|r| r.0 == t.seq) else { return };
        let edit = if value.is_null() { Ok(None) } else { edits::read_workspace_edit(value).map(Some) };
        match edit {
            Ok(Some(edit)) if self.config.lsp.rename_preview => match edits::workspace_steps(edit) {
                Ok(steps) => {
                    let (open, root) = (self.open_texts(), self.workspace.root.clone());
                    let textual = self.lsp.edit.rename_in_text.then_some(TextualStage {
                        scope: TextualScope::CommentsAndStrings,
                        show_hidden: self.config.files.show_hidden,
                        show_ignored: self.config.files.show_ignored,
                    });
                    self.rename_preview_start(RenameRequest { old, new: name, steps, open, root, textual });
                }
                Err(why) => self.edits_refused(EditOrigin::Rename(name), why),
            },
            Ok(Some(edit)) => {
                self.apply_workspace_edit(edit, EditOrigin::Rename(name));
            }
            Ok(None) => self.lsp_hint("Nothing to rename"),
            Err(e) => self.lsp_hint(format!("Rename: unexpected answer ({e})")),
        }
    }

    /// Every open buffer with a path, as the rename preview sees it.
    fn open_texts(&self) -> Vec<OpenText> {
        self.editor
            .buffers
            .iter()
            .filter_map(|b| Some(OpenText { path: b.path.clone()?, id: b.id, rope: b.rope.clone(), version: b.version, dirty: b.dirty() }))
            .collect()
    }

    /// Build the preview of `req` off the main thread; the result opens in Find (`rename_preview_done`).
    pub(super) fn rename_preview_start(&mut self, req: RenameRequest) {
        self.find.reset();
        self.close_replace_dialog();
        if req.textual.is_some() {
            // Find is not shown until the build ends.
            self.notify(Level::Info, format!("Rename {}: searching the project for text matches…", req.old));
        }
        let (seq, generation, tx) = (self.find.current_seq(), self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let result = Box::new(rename_preview::build(req));
            let _ = tx.send(AppMsg::RenamePreview { generation, seq, result });
        });
    }

    /// Close a Replace in Files confirmation: it must not outlive a change of what it confirms. Other dialogs stay.
    fn close_replace_dialog(&mut self) {
        self.dialog.take_if(|d| d.id == DialogId::ReplaceInFiles);
    }

    /// The preview built by `rename_preview_start`: show it in Find, unless the root or the Find results moved on.
    pub(super) fn rename_preview_done(&mut self, generation: u64, seq: u64, result: Result<(RenamePreview, Vec<Hit>), String>) {
        if generation != self.workspace.generation || seq != self.find.current_seq() {
            tracing::debug!("dropping stale rename preview");
            return;
        }
        self.close_replace_dialog();
        let (preview, hits) = match result {
            Ok(r) => r,
            Err(e) => return self.notify(Level::Warn, format!("Rename: {e}; nothing was changed")),
        };
        if hits.is_empty() && preview.op_count() == 0 {
            return self.lsp_hint("Nothing to rename");
        }
        let (old, new, rows, capped) = (preview.old.clone(), preview.new.clone(), preview.textual.rows, preview.textual.capped);
        self.layout.show(ToolWindowId::Find);
        self.focus.set(PanelId::Find);
        let mut cx = ctx!(self);
        self.find.open_preview(preview, hits, &mut cx);
        let guesses = match rows {
            0 => String::new(),
            1 => "; 1 text match starts excluded".to_string(),
            n => format!("; {n} text matches start excluded"),
        };
        self.notify(Level::Info, format!("Rename {old} to {new}: review the results, then Replace All (Alt+A){guesses}"));
        if capped {
            self.notify(Level::Warn, format!("Text search stopped at {MAX_HITS} matches; some files were not searched"));
        }
    }

    /// Replace All in a rename preview: refuse when a file changed since, else apply the included edits and
    /// every file operation. A refusal keeps the preview.
    pub(super) fn rename_preview_apply(&mut self) {
        let open = self.open_texts();
        let Some((preview, hits)) = self.find.take_preview() else { return };
        if let Some(path) = preview.first_stale(&open).or_else(|| preview.first_stale_textual(&open, &hits)) {
            self.find.restore_preview(preview);
            let rel = path.strip_prefix(&self.workspace.root).unwrap_or(&path).display().to_string();
            return self.notify(Level::Error, format!("{rel} changed since the preview; rename again"));
        }
        let steps = rename_preview::included_steps(&preview, &hits);
        self.lsp_sync();
        if self.apply_steps_now(steps, EditOrigin::Rename(preview.new.clone())) {
            self.find.finish_preview();
        } else {
            self.find.restore_preview(preview);
        }
    }

    /// `lsp.context_actions` (Alt+Enter): the server's quick fixes and
    /// refactorings for the selection (or the caret), with the diagnostics
    /// there. `lsp.refactor_this` and `lsp.optimize_imports` send the same
    /// request with `context.only` (the latter over the whole document).
    pub(super) fn lsp_code_actions(&mut self, what: ActionsFor) {
        self.hover = None;
        self.completion_close();
        let Some(b) = self.editor.active_buffer() else { return };
        let r = b.view.sel().range();
        let range = match what {
            ActionsFor::OrganizeImports => {
                lsp_types::Range::new(lsp_types::Position::new(0, 0), pos::position(&b.rope, b.rope.len_chars()))
            }
            _ => lsp_types::Range::new(pos::position(&b.rope, r.start), pos::position(&b.rope, r.end)),
        };
        let (id, head) = (b.id, pos::position(&b.rope, b.view.sel().head));
        let diagnostics = self.diagnostics_overlapping(b, range);
        let mut context = json!({"diagnostics": diagnostics, "triggerKind": 1});
        if let Some(k) = what.only() {
            context["only"] = json!([k]);
        }
        let params = json!({"range": range, "context": context});
        if let Err(why) = self.lsp_tagged_request(id, head, CODE_ACTION, params, what.tag())
            && !why.is_empty()
        {
            self.lsp_hint(why);
        }
    }

    /// The diagnostics of `b`'s file whose range overlaps `range`, as LSP sends them.
    pub(super) fn diagnostics_overlapping(&self, b: &Buffer, range: lsp_types::Range) -> Vec<lsp_types::Diagnostic> {
        let key = |p: lsp_types::Position| (p.line, p.character);
        b.path
            .as_deref()
            .map(|p| self.lsp.diagnostics.get(p))
            .unwrap_or_default()
            .iter()
            .filter(|d| key(d.range.start) <= key(range.end) && key(range.start) <= key(d.range.end))
            .map(|d| d.to_lsp())
            .collect()
    }

    pub(super) fn lsp_code_actions_answer(&mut self, t: DocTag, value: Value, what: ActionsFor) {
        let mut items: Vec<CodeActionOrCommand> = serde_json::from_value::<Option<Vec<Value>>>(value)
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|raw| {
                let mut item = serde_json::from_value::<CodeActionOrCommand>(raw.clone()).ok()?;
                if let CodeActionOrCommand::CodeAction(a) = &mut item
                    && let (Some(edit), Some(raw_edit)) = (a.edit.as_mut(), raw.get("edit"))
                {
                    edits::prune_annotations(raw_edit, edit);
                }
                Some(item)
            })
            .filter(|a| !matches!(a, CodeActionOrCommand::CodeAction(c) if c.disabled.is_some()))
            .filter(|a| what.keeps(a))
            .collect();
        preferred_first(&mut items);
        if items.is_empty() {
            return self.lsp_hint(what.none_hint());
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
        let single = what == ActionsFor::OrganizeImports && items.len() == 1;
        self.lsp.edit.actions = Some((t.seq, items));
        if single {
            return self.lsp_code_action_chosen(t, 0);
        }
        self.dialog = Some(Dialog::choice(DialogId::LspCodeAction(t), what.title(), "", titles));
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
        let action = serde_json::from_value::<CodeAction>(value.clone()).map(|mut a| {
            if let (Some(edit), Some(raw)) = (a.edit.as_mut(), value.get("edit")) {
                edits::prune_annotations(raw, edit);
            }
            a
        });
        match action {
            Ok(a) if a.edit.is_some() || a.command.is_some() => self.lsp_run_code_action(a),
            Ok(a) => self.lsp_hint(format!("{}: nothing to do", a.title)),
            Err(e) => self.lsp_hint(format!("Context action: unexpected answer ({e})")),
        }
    }

    /// Make a code action's edit, then run its command.
    fn lsp_run_code_action(&mut self, a: CodeAction) {
        if let Some(edit) = a.edit {
            self.apply_or_confirm(edit, EditOrigin::CodeAction(a.title.clone()), a.command);
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
                LocationItem { path: path.clone(), line, col, text: shown, mark: None }
            })
            .collect();
        let title = format!("Structure of {}", b.name);
        self.show_locations(title, items);
    }

    /// Apply a server's workspace edit, asking first when it has changes
    /// that need confirmation. Returns false when it was refused and nothing
    /// changed; an edit waiting for the user returns false too.
    pub(super) fn apply_workspace_edit(&mut self, edit: WorkspaceEdit, origin: EditOrigin) -> bool {
        self.apply_or_confirm(edit, origin, None)
    }

    /// `apply_workspace_edit`; `then` (a code action's command) runs after the edit was made.
    fn apply_or_confirm(&mut self, edit: WorkspaceEdit, origin: EditOrigin, then: Option<Command>) -> bool {
        let Some((rows, total)) = edits::confirmation(&edit) else {
            let ok = self.apply_workspace_edit_now(edit, origin);
            if ok && let Some(c) = then {
                self.lsp_execute(c);
            }
            return ok;
        };
        if self.dialog.is_some() {
            self.edits_refused(origin, "another dialog is open".into());
            return false;
        }
        if let Some(old) = self.lsp.edit.confirm.take() {
            self.edits_refused(old.origin, "replaced by a newer edit".into());
        }
        self.lsp.last_seq += 1;
        let seq = self.lsp.last_seq;
        let msg = confirm_message(&Self::edit_title(&origin), &rows, total);
        self.lsp.edit.confirm = Some(PendingConfirm { seq, edit, origin, then });
        self.dialog = Some(Dialog::confirm(DialogId::LspConfirmEdit(seq), "Confirm Changes", &msg, &["Apply", "Cancel"]));
        self.dispatcher.reset();
        false
    }

    /// The confirmation dialog's answer for the edit parked as `seq`.
    pub(super) fn lsp_confirm_edit(&mut self, seq: u64, apply: bool) {
        let Some(p) = self.lsp.edit.confirm.take_if(|p| p.seq == seq) else { return };
        if !apply {
            return self.edits_refused(p.origin, "declined".into());
        }
        let ok = self.apply_workspace_edit_now(p.edit, p.origin);
        if ok && let Some(c) = p.then {
            self.lsp_execute(c);
        }
    }

    /// Refuses a parked edit whose dialog was replaced, so its file operation or server answer is not lost.
    pub(super) fn lsp_confirm_orphaned(&mut self) {
        let Some(seq) = self.lsp.edit.confirm.as_ref().map(|p| p.seq) else { return };
        if self.dialog.as_ref().is_some_and(|d| d.id == DialogId::LspConfirmEdit(seq)) {
            return;
        }
        if let Some(p) = self.lsp.edit.confirm.take() {
            self.edits_refused(p.origin, "declined".into());
        }
    }

    /// Apply a server's workspace edit (see the module docs). Returns false
    /// when it was refused and nothing changed.
    fn apply_workspace_edit_now(&mut self, edit: WorkspaceEdit, origin: EditOrigin) -> bool {
        // The documents' versions as the server knows them.
        self.lsp_sync();
        let steps = match edits::workspace_steps(edit) {
            Ok(steps) => steps,
            Err(why) => {
                self.edits_refused(origin, why);
                return false;
            }
        };
        self.apply_steps_now(steps, origin)
    }

    /// Plan and apply `steps` (see the module docs). Returns false when they were refused and nothing changed.
    /// The caller has run `lsp_sync`, so the documents' versions are current.
    pub(super) fn apply_steps_now(&mut self, steps: Vec<edits::Step>, origin: EditOrigin) -> bool {
        // Every buffer with a path: operations on folders touch tabs under them.
        let open: Vec<OpenDoc> = self
            .editor
            .buffers
            .iter()
            .filter_map(|b| {
                Some(OpenDoc {
                    id: b.id,
                    path: b.path.clone()?,
                    name: b.name.clone(),
                    rope: b.rope.clone(),
                    crlf: b.line_ending == LineEnding::CrLf,
                    version: self.lsp.docs.get(&b.id).map(|d| d.version),
                    dirty: b.dirty(),
                })
            })
            .collect();
        let plan = match edits::plan_workspace(steps, &open, &self.workspace.root, &edits::RealDisk) {
            Ok(plan) => plan,
            Err(why) => {
                self.edits_refused(origin, why);
                return false;
            }
        };
        // Clean tabs of files deleted or replaced: closed now, before they
        // can become dirty.
        self.editor.force_close_ids(&plan.close);
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
        if plan.disk.is_empty() {
            self.edits_applied(origin, done, Vec::new(), Vec::new(), None);
            return true;
        }
        // Local history snapshots of what is deleted or replaced, queued in
        // step order; each destructive step waits for its own.
        let acks: Vec<Option<Ack>> = plan
            .disk
            .iter()
            .map(|step| match step {
                DiskStep::Op { op: FsOp::Delete { path, .. } | FsOp::Create { path, overwrite: true, .. }, .. } => {
                    self.history.before_delete(path)
                }
                DiskStep::Op { op: FsOp::Rename { to, overwrite: true, .. }, .. } => self.history.before_delete(to),
                _ => None,
            })
            .collect();
        if let EditOrigin::FileOp { seq, .. } = &origin {
            self.mark_file_op_writing(*seq);
        }
        let (tx, generation, root) = (self.bus_tx.clone(), self.workspace.generation, self.workspace.root.clone());
        tokio::task::spawn_blocking(move || {
            let (mut errors, mut ops, mut failed) = (Vec::new(), Vec::new(), None);
            let total = plan.disk.len();
            for (i, (step, ack)) in plan.disk.into_iter().zip(acks).enumerate() {
                if let Some(ack) = ack {
                    ack.wait();
                }
                let error = match step {
                    DiskStep::Edit(f) => match edits::edit_file(&f.path, &f.edits) {
                        Ok(0) => None,
                        Ok(n) => {
                            done.files += 1;
                            done.edits += n;
                            None
                        }
                        Err(e) => Some((e, f.index)),
                    },
                    DiskStep::Op { index, op } => match op.run() {
                        Ok(()) => {
                            ops.push(op);
                            None
                        }
                        Err(e) => Some((format!("Cannot {}: {e}", op.describe(&root)), Some(index))),
                    },
                };
                // Abort: the steps after a failed one are not made.
                if let Some((e, index)) = error {
                    errors.push(e);
                    failed = index;
                    done.skipped = total - i - 1;
                    break;
                }
            }
            let _ = tx.send(AppMsg::EditsApplied { generation, origin, done, errors, ops, failed });
        });
        true
    }

    /// What a workspace edit is called in messages.
    fn edit_title(origin: &EditOrigin) -> String {
        match origin {
            EditOrigin::Rename(_) => "Rename".into(),
            EditOrigin::CodeAction(title) | EditOrigin::FileOp { title, .. } => title.clone(),
            EditOrigin::ServerRequest { server, label, .. } => label.clone().unwrap_or_else(|| format!("Edit from {server}")),
        }
    }

    fn edits_refused(&mut self, origin: EditOrigin, why: String) {
        self.notify(Level::Warn, format!("{}: {why}; nothing was changed", Self::edit_title(&origin)));
        if let EditOrigin::FileOp { seq, .. } = origin {
            // The operation still runs: the server's edit was an extra.
            return self.file_op_continue(seq);
        }
        if let EditOrigin::ServerRequest { server, id, .. } = origin
            && let Some(c) = self.lsp.clients.get(&server)
        {
            c.respond(id, Ok(json!({"applied": false, "failureReason": why})));
        }
    }

    /// A workspace edit was made (`AppMsg::EditsApplied` for one with disk
    /// steps): tabs, local history and the language servers follow its file
    /// operations; report it, answer the server that asked for it.
    pub(super) fn edits_applied(
        &mut self,
        origin: EditOrigin,
        done: EditCounts,
        mut errors: Vec<String>,
        ops: Vec<FsOp>,
        failed: Option<u32>,
    ) {
        for op in &ops {
            match op {
                FsOp::Rename { from, to, .. } => {
                    // History first: snapshots of the old path queued until now are carried.
                    self.history_renamed(from, to);
                    self.editor.retarget(from, to, &self.theme);
                }
                FsOp::Delete { path, .. } => {
                    self.history_files_changed(path);
                    self.editor.close_under(path);
                }
                FsOp::Create { .. } => {}
            }
        }
        let title = Self::edit_title(&origin);
        let s = |n: usize| if n == 1 { "" } else { "s" };
        if done.skipped > 0
            && let Some(first) = errors.first_mut()
        {
            first.push_str(&format!("; {} later change{} not made", done.skipped, s(done.skipped)));
        }
        for e in errors.iter().take(3) {
            self.notify(Level::Error, format!("{title}: {e}"));
        }
        if errors.len() > 3 {
            self.notify(Level::Error, format!("{title}: {} more errors (see the log)", errors.len() - 3));
            for e in &errors[3..] {
                tracing::error!("{title}: {e}");
            }
        }
        if !ops.is_empty() {
            // The renamed documents: didClose of the old URI, didOpen of the new.
            self.lsp_sync();
        }
        let ops_text = describe_ops(&ops, &self.workspace.root);
        let file_op_seq = if let EditOrigin::FileOp { seq, .. } = &origin { Some(*seq) } else { None };
        match origin {
            EditOrigin::Rename(name) if done.files > 0 => {
                let mut text =
                    format!("Renamed to {name}: {} occurrence{} in {} file{}", done.edits, s(done.edits), done.files, s(done.files));
                if let Some(ops) = &ops_text {
                    text.push_str(&format!("; {ops}"));
                }
                self.notify(Level::Info, text);
            }
            EditOrigin::Rename(name) if ops_text.is_some() => {
                self.notify(Level::Info, format!("Renamed to {name}: {}", ops_text.unwrap_or_default()));
            }
            EditOrigin::Rename(_) if errors.is_empty() => self.lsp_hint("Nothing to rename"),
            EditOrigin::Rename(_) => {}
            EditOrigin::CodeAction(_) => {
                if let Some(ops) = ops_text {
                    self.notify(Level::Info, format!("{title}: {ops}"));
                }
            }
            EditOrigin::FileOp { .. } => {
                if done.files > 0 {
                    self.notify(
                        Level::Info,
                        format!("{title}: updated {} occurrence{} in {} file{}", done.edits, s(done.edits), done.files, s(done.files)),
                    );
                }
                if let Some(ops) = ops_text {
                    self.notify(Level::Info, format!("{title}: {ops}"));
                }
            }
            EditOrigin::ServerRequest { server, id, .. } => {
                if let Some(ops) = ops_text {
                    self.notify(Level::Info, format!("{title}: {ops}"));
                }
                if let Some(c) = self.lsp.clients.get(&server) {
                    let mut answer = json!({"applied": errors.is_empty()});
                    if let Some(e) = errors.first() {
                        answer["failureReason"] = e.clone().into();
                    }
                    if let Some(i) = failed {
                        answer["failedChange"] = i.into();
                    }
                    c.respond(id, Ok(answer));
                }
            }
        }
        if done.files > 0 || !ops.is_empty() {
            self.apply_effects(vec![Effect::GitRefresh]);
        }
        if let Some(seq) = file_op_seq {
            self.file_op_continue(seq);
        }
    }
}

/// The file operations made, for messages: "created a.rs, renamed b.rs to
/// c.rs" (three at most, then how many more); `None` when there are none.
fn describe_ops(ops: &[FsOp], root: &Path) -> Option<String> {
    if ops.is_empty() {
        return None;
    }
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let mut text = ops
        .iter()
        .take(3)
        .map(|op| match op {
            FsOp::Create { path, .. } => format!("created {}", rel(path)),
            FsOp::Rename { from, to, .. } => format!("renamed {} to {}", rel(from), rel(to)),
            FsOp::Delete { path, .. } => format!("deleted {}", rel(path)),
        })
        .collect::<Vec<_>>()
        .join(", ");
    if ops.len() > 3 {
        let k = ops.len() - 3;
        text.push_str(&format!(" and {k} more file operation{}", if k == 1 { "" } else { "s" }));
    }
    Some(text)
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, LspAction, PaletteKind};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, sent, with_fake_servers};
    use crate::app::tests::{handle_next, load, test_app};
    use crate::lsp::fake::FakeServer;
    use crate::lsp::sync;
    use crate::ui::dialog::{DialogKind, DialogResult};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        let c = serde_json::to_value(crate::lsp::client::client_capabilities(true)).unwrap();
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
        assert_eq!(c["workspace"]["workspaceEdit"]["resourceOperations"], json!(["create", "rename", "delete"]));
        assert_eq!(c["workspace"]["workspaceEdit"]["failureHandling"], "abort");
    }

    #[tokio::test]
    async fn rename_edits_the_open_buffer_in_one_step_and_the_closed_file_on_disk() {
        let (mut app, dir) = test_app("lsp-rename");
        let mut servers = with_fake_servers(&mut app, caps());
        app.config.lsp.rename_preview = false;
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
        ],
        // As rust-analyzer: an annotation that needs confirmation, that no edit refers to.
        "changeAnnotations": {"OutsideWorkspace": {"label": "Edit outside of the workspace", "needsConfirmation": true}}});
        let req = answer(&mut app, &mut server, "textDocument/rename", we).await;
        assert_eq!(req["params"]["newName"], "renamed");
        assert!(app.dialog.is_none(), "no confirmation for an unreferenced annotation");
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
        app.config.lsp.rename_preview = false;
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
        // Creating a file that exists (no options): nothing changes either.
        app.perform(Action::Lsp(LspAction::Rename));
        answer(&mut app, &mut server, "textDocument/prepareRename", range(1, 18, 1, 21)).await;
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Text("renamed".into()));
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"kind": "create", "uri": uri(&b)},
        ]});
        answer(&mut app, &mut server, "textDocument/rename", we).await;
        assert_eq!(text(&app), A);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), B);
        assert!(app.notifications.last().unwrap().text.contains("b.rs already exists"), "{}", app.notifications.last().unwrap().text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn without_prepare_the_word_at_the_caret_is_offered() {
        let (mut app, dir) = test_app("lsp-rename-word");
        let mut c = incremental();
        c["renameProvider"] = json!(true);
        let mut servers = with_fake_servers(&mut app, c);
        app.config.lsp.rename_preview = false;
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
    async fn rename_without_a_server_previews_in_find_with_index_candidates() {
        use crate::ui::focus::{PanelId, ToolWindowId};
        const MAIN: &str = "fn helper() {}\nfn main() {\n    helper(); // helper\n}\n";
        let (mut app, dir) = test_app("rename-text");
        let (root, g) = (app.workspace.root.clone(), app.workspace.generation);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), MAIN).unwrap();
        std::fs::write(root.join("src/util.rs"), "fn f() {\n    helper();\n}\n").unwrap();
        std::fs::write(root.join("notes.txt"), "helper here\n").unwrap();
        let files = ["src/main.rs", "src/util.rs"];
        app.handle_msg(AppMsg::FileIndex { generation: g, seq: 1, files: files.iter().map(|s| s.to_string()).collect() });
        let job = app.symbols.start(root.clone());
        let (seq, mut batches) = (job.seq, Vec::new());
        job.run(|b| batches.push(b));
        for batch in batches {
            app.handle_msg(AppMsg::Symbols { generation: g, seq, batch });
        }
        load(&mut app, "src/main.rs", MAIN);
        app.editor.active_buffer_mut().unwrap().goto_line(2, 6);
        app.perform(Action::Lsp(LspAction::Rename));
        let d = app.dialog.take().expect("asked at once");
        assert!(matches!(d.id, DialogId::TextRename(_)));
        assert!(matches!(&d.kind, DialogKind::Input { input, .. } if input.text == "helper"));
        app.finish_dialog(d.id, DialogResult::Text(" assist ".into()));
        assert!(app.layout.is_visible(ToolWindowId::Find) && app.focus.is(PanelId::Find));
        let f = &app.find;
        assert_eq!((f.query.text.as_str(), f.replace.text.as_str()), ("helper", "assist"));
        assert!(f.show_replace && f.case_sensitive && f.whole_word && !f.regex);
        let want = "Rename helper to assist: review the results, then Replace All (Alt+A)";
        assert!(toasts(&app).contains(&want.to_string()), "{:?}", toasts(&app));
        crate::app::tests::run_find(&mut app);
        let got: Vec<(PathBuf, Vec<(usize, usize)>)> =
            app.find.included_by_file().into_iter().map(|(p, h)| (p, h.iter().map(|h| (h.line, h.col)).collect())).collect();
        assert_eq!(got, [(root.join("src/main.rs"), vec![(0, 3), (2, 4)]), (root.join("src/util.rs"), vec![(1, 4)])]);
        assert_eq!(app.find.excluded_count(), 2, "the comment and notes.txt");
        app.finish_dialog(DialogId::ReplaceInFiles, DialogResult::Button(0));
        handle_next(&mut app, |m| matches!(m, AppMsg::ReplaceDone { .. })).await;
        assert_eq!(text(&app), "fn assist() {}\nfn main() {\n    assist(); // helper\n}\n");
        assert!(app.editor.active_buffer().unwrap().dirty());
        assert_eq!(std::fs::read_to_string(root.join("src/util.rs")).unwrap(), "fn f() {\n    assist();\n}\n");
        assert_eq!(std::fs::read_to_string(root.join("notes.txt")).unwrap(), "helper here\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rename_without_a_server_on_no_word_or_unchanged_name_does_nothing() {
        let (mut app, dir) = test_app("rename-text-noop");
        load(&mut app, "a.rs", "fn old() {}\n");
        app.editor.active_buffer_mut().unwrap().goto_line(0, 2);
        app.perform(Action::Lsp(LspAction::Rename));
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"Cannot rename this element".to_string()), "{:?}", toasts(&app));
        app.editor.active_buffer_mut().unwrap().goto_line(0, 4);
        app.perform(Action::Lsp(LspAction::Rename));
        let d = app.dialog.take().expect("asked");
        app.finish_dialog(d.id, DialogResult::Text("old".into()));
        assert!(!app.layout.is_visible(crate::ui::focus::ToolWindowId::Find));
        app.perform(Action::Lsp(LspAction::Rename));
        let d = app.dialog.take().expect("asked");
        app.perform(Action::Editor(EditorAction::InsertText("x".into())));
        app.finish_dialog(d.id, DialogResult::Text("new".into()));
        assert!(toasts(&app).contains(&"The file changed; rename again".to_string()), "{:?}", toasts(&app));
        assert!(!app.layout.is_visible(crate::ui::focus::ToolWindowId::Find));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preferred_code_actions_come_first() {
        let item = |v: Value| serde_json::from_value::<CodeActionOrCommand>(v).unwrap();
        let title = |a: &CodeActionOrCommand| match a {
            CodeActionOrCommand::Command(c) => c.title.clone(),
            CodeActionOrCommand::CodeAction(a) => a.title.clone(),
        };
        let mut items = vec![
            item(json!({"title": "cmd", "command": "c"})),
            item(json!({"title": "a"})),
            item(json!({"title": "p1", "isPreferred": true})),
            item(json!({"title": "b", "isPreferred": false})),
            item(json!({"title": "p2", "isPreferred": true})),
        ];
        preferred_first(&mut items);
        assert_eq!(items.iter().map(title).collect::<Vec<_>>(), ["p1", "p2", "cmd", "a", "b"]);
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
        assert!(req["params"]["context"].get("only").is_none(), "Alt+Enter asks for every kind");
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

        // A preferred one comes first.
        let mut preferred = lazy.clone();
        preferred["isPreferred"] = json!(true);
        app.perform(Action::Lsp(LspAction::ContextActions));
        answer(&mut app, &mut server, "textDocument/codeAction", json!([fix, preferred, cmd])).await;
        let d = app.dialog.take().unwrap();
        match &d.kind {
            DialogKind::Choice { items, .. } => assert_eq!(items, &["Inline", "Import old", "Fix all"]),
            k => panic!("{k:?}"),
        }
        app.finish_dialog(d.id, DialogResult::Cancel);

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
        // An edit it cannot make (deleting the file with unsaved changes)
        // is answered with the reason.
        server.request(42, "workspace/applyEdit", json!({"edit": {"documentChanges": [{"kind": "delete", "uri": uri(&a)}]}}));
        handle_next(&mut app, is_lsp).await;
        let r = server.response(42).await["result"].clone();
        assert_eq!(r["applied"], false);
        assert!(r["failureReason"].as_str().unwrap().contains("unsaved changes"), "{r}");
        assert!(a.exists());
        // Nothing offered: a toast.
        app.perform(Action::Lsp(LspAction::ContextActions));
        answer(&mut app, &mut server, "textDocument/codeAction", json!([])).await;
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"No context actions here".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refactor_this_lists_only_refactorings() {
        let (mut app, dir) = test_app("lsp-refactor");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        let diag = |l: u32, c0: u32, c1: u32, code: Value| json!({"range": range(l, c0, l, c1), "severity": 1, "message": "unresolved", "code": code});
        server.notify(
            "textDocument/publishDiagnostics",
            json!({"uri": uri(&a), "diagnostics": [diag(1, 18, 21, json!(425)), diag(0, 3, 7, json!("x"))]}),
        );
        handle_next(&mut app, is_lsp).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 19);
        app.perform(Action::Lsp(LspAction::RefactorThis));
        let one_edit = json!({"changes": {uri(&a): [edit(0, 0, 0, 0, "use m::old;\n")]}});
        let fix = json!({"title": "Import old", "kind": "quickfix", "edit": one_edit});
        let extract = json!({"title": "Extract variable", "kind": "refactor.extract", "edit": one_edit});
        let plain = json!({"title": "Rewrite", "kind": "refactor", "isPreferred": true, "edit": one_edit});
        let cmd = json!({"title": "Fix all", "command": "fix.all"});
        let organize = json!({"title": "Organize", "kind": "source.organizeImports", "edit": one_edit});
        let off = json!({"title": "Inline", "kind": "refactor.inline", "disabled": {"reason": "no"}});
        let lookalike = json!({"title": "Refactoring-ish", "kind": "refactoring", "edit": one_edit});
        let req =
            answer(&mut app, &mut server, "textDocument/codeAction", json!([fix, extract, plain, cmd, organize, off, lookalike])).await;
        assert_eq!(req["params"]["context"]["only"], json!(["refactor"]));
        assert_eq!(req["params"]["range"], range(1, 20, 1, 20));
        let sent = &req["params"]["context"]["diagnostics"];
        assert_eq!(sent.as_array().unwrap().len(), 1, "only the one at the caret: {sent}");
        let d = app.dialog.take().expect("the refactorings");
        match &d.kind {
            DialogKind::Choice { items, .. } => {
                assert_eq!(d.title, "Refactor This");
                assert_eq!(items, &["Rewrite", "Extract variable"]);
            }
            k => panic!("{k:?}"),
        }
        app.finish_dialog(d.id, DialogResult::Choice(1));
        assert_eq!(text(&app), format!("use m::old;\n{A}"));
        app.perform(Action::Editor(EditorAction::Undo));
        app.perform(Action::Lsp(LspAction::RefactorThis));
        answer(&mut app, &mut server, "textDocument/codeAction", json!([fix])).await;
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"No refactorings here".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn optimize_imports_runs_a_single_action_at_once() {
        let (mut app, dir) = test_app("lsp-organize");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        app.editor.active_buffer_mut().unwrap().goto_line(1, 19);
        app.perform(Action::Lsp(LspAction::OptimizeImports));
        let edit_of = || json!({"changes": {uri(&a): [edit(0, 0, 0, 0, "use m::old;\n")]}});
        let organize = json!({"title": "Organize", "kind": "source.organizeImports", "edit": edit_of()});
        let fix = json!({"title": "Import old", "kind": "quickfix", "edit": edit_of()});
        let req = answer(&mut app, &mut server, "textDocument/codeAction", json!([organize, fix])).await;
        assert_eq!(req["params"]["context"]["only"], json!(["source.organizeImports"]));
        assert_eq!(req["params"]["range"], range(0, 0, 3, 0));
        assert!(app.dialog.is_none());
        assert_eq!(text(&app), format!("use m::old;\n{A}"));
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&app), A);

        // One without its edit is resolved first.
        app.perform(Action::Lsp(LspAction::OptimizeImports));
        let lazy = json!({"title": "Organize", "kind": "source.organizeImports.rust", "data": {"id": 7}});
        answer(&mut app, &mut server, "textDocument/codeAction", json!([lazy])).await;
        assert!(app.dialog.is_none());
        let mut resolved = lazy.clone();
        resolved["edit"] = edit_of();
        let req = answer(&mut app, &mut server, "codeAction/resolve", resolved).await;
        assert_eq!(req["params"]["data"], json!({"id": 7}));
        assert_eq!(text(&app), format!("use m::old;\n{A}"));
        app.perform(Action::Editor(EditorAction::Undo));

        // Several: the choice.
        app.perform(Action::Lsp(LspAction::OptimizeImports));
        let second = json!({"title": "Organize (rust)", "kind": "source.organizeImports.rust", "edit": edit_of()});
        answer(&mut app, &mut server, "textDocument/codeAction", json!([organize, second])).await;
        let d = app.dialog.take().expect("the choice");
        match &d.kind {
            DialogKind::Choice { items, .. } => {
                assert_eq!(d.title, "Optimize Imports");
                assert_eq!(items, &["Organize", "Organize (rust)"]);
            }
            k => panic!("{k:?}"),
        }
        app.finish_dialog(d.id, DialogResult::Cancel);
        assert_eq!(text(&app), A);

        // None of its kind: a toast.
        app.perform(Action::Lsp(LspAction::OptimizeImports));
        answer(&mut app, &mut server, "textDocument/codeAction", json!([fix])).await;
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"Nothing to optimize".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rename_answer_moves_the_open_file_and_its_tab() {
        let (mut app, dir) = test_app("lsp-rename-file");
        let mut servers = with_fake_servers(&mut app, caps());
        app.config.lsp.rename_preview = false;
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let root = app.workspace.root.clone();
        let (a, b, c) = (root.join("a.rs"), root.join("b.rs"), root.join("c.rs"));
        std::fs::write(&b, B).unwrap();
        app.editor.active_buffer_mut().unwrap().goto_line(1, 18);
        app.perform(Action::Lsp(LspAction::Rename));
        answer(&mut app, &mut server, "textDocument/prepareRename", range(1, 18, 1, 21)).await;
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Text("renamed".into()));
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"kind": "rename", "oldUri": uri(&a), "newUri": uri(&c)},
            {"textDocument": {"uri": uri(&b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed"), edit(1, 6, 1, 9, "renamed")]},
        ]});
        answer(&mut app, &mut server, "textDocument/rename", we).await;
        let edited = "fn main() {\n    let s = \"😀\"; renamed(s);\n}\n";
        assert_eq!(text(&app), edited, "the open buffer is edited at once");
        handle_next(&mut app, applied).await;
        // On disk: moved with its saved text, the other file edited.
        assert!(!a.exists());
        assert_eq!(std::fs::read_to_string(&c).unwrap(), A, "unsaved edits are not written");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 renamed\n");
        // The tab follows, still dirty.
        let buf = app.editor.active_buffer().unwrap();
        assert_eq!((buf.path.as_deref(), buf.name.as_str(), buf.dirty()), (Some(c.as_path()), "c.rs", true));
        // The server closes the old document and opens the new one.
        app.lsp_sync();
        let closed = server.recv("textDocument/didClose").await;
        assert_eq!(closed["params"]["textDocument"]["uri"], uri(&a));
        let opened = server.recv("textDocument/didOpen").await;
        assert_eq!(opened["params"]["textDocument"]["uri"], uri(&c));
        assert_eq!(opened["params"]["textDocument"]["text"], edited);
        let want = "Renamed to renamed: 3 occurrences in 2 files; renamed a.rs to c.rs";
        assert!(toasts(&app).contains(&want.to_string()), "{:?}", toasts(&app));
        // Undo reverts the text in the moved tab; the file stays moved.
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&app), A);
        assert!(c.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn apply_edit_creates_edits_and_deletes_files() {
        let (mut app, dir) = test_app("lsp-file-ops");
        let mut servers = with_fake_servers(&mut app, caps());
        let server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let mut server = server;
        let root = app.workspace.root.clone();
        let (n, old, d) = (root.join("n.rs"), root.join("old.txt"), root.join("d"));
        std::fs::write(&old, "old\n").unwrap();
        std::fs::create_dir(&d).unwrap();
        load(&mut app, "d/x.txt", "x\n");
        assert!(app.editor.index_of(&d.join("x.txt")).is_some());
        server.request(
            50,
            "workspace/applyEdit",
            json!({"label": "Move", "edit": {"documentChanges": [
                {"kind": "create", "uri": uri(&n)},
                {"textDocument": {"uri": uri(&n), "version": null}, "edits": [edit(0, 0, 0, 0, "fn n() {}\n")]},
                {"kind": "delete", "uri": uri(&old)},
                {"kind": "delete", "uri": uri(&d), "options": {"recursive": true}},
            ]}}),
        );
        handle_next(&mut app, is_lsp).await;
        handle_next(&mut app, applied).await;
        assert_eq!(server.response(50).await["result"], json!({"applied": true}));
        assert_eq!(std::fs::read_to_string(&n).unwrap(), "fn n() {}\n");
        assert!(app.editor.index_of(&n).is_none(), "a created file is not opened");
        assert!(!old.exists() && !d.exists());
        assert!(app.editor.buffers.iter().all(|b| !b.path.as_deref().is_some_and(|p| p.starts_with(&d))), "no tab under d");
        let want = "Move: created n.rs, deleted old.txt, deleted d";
        assert!(toasts(&app).contains(&want.to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_operations_that_cannot_be_made_change_nothing() {
        let (mut app, dir) = test_app("lsp-file-ops-no");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let root = app.workspace.root.clone();
        let (a, b, d) = (root.join("a.rs"), root.join("b.rs"), root.join("d"));
        std::fs::write(&b, B).unwrap();
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("x.txt"), "x").unwrap();
        let outside = dir.join("outside.rs");
        let cases = [
            (json!({"kind": "create", "uri": uri(&b)}), "already exists"),
            (json!({"kind": "rename", "oldUri": uri(&b), "newUri": uri(&outside)}), "outside the project"),
            (json!({"kind": "delete", "uri": uri(&d)}), "not empty"),
            (
                json!({"textDocument": {"uri": uri(&root.join("z.rs")), "version": null}, "edits": [edit(0, 0, 0, 0, "z")]}),
                "z.rs does not exist",
            ),
            (json!({"kind": "delete", "uri": uri(&a)}), "unsaved changes"),
        ];
        for (id, (op, why)) in (60..).zip(cases) {
            if why == "unsaved changes" {
                app.perform(Action::Editor(EditorAction::InsertText("x".into())));
            }
            let before = text(&app);
            let changes = json!([{"textDocument": {"uri": uri(&a), "version": null}, "edits": [edit(0, 0, 0, 0, "// x\n")]}, op]);
            server.request(id, "workspace/applyEdit", json!({"edit": {"documentChanges": changes}}));
            handle_next(&mut app, is_lsp).await;
            let r = server.response(id).await["result"].clone();
            assert_eq!(r["applied"], false, "{why}");
            assert!(r["failureReason"].as_str().unwrap().contains(why), "{r} should say {why}");
            assert_eq!(text(&app), before, "{why}");
            assert_eq!(std::fs::read_to_string(&b).unwrap(), B);
            assert!(d.join("x.txt").exists() && a.exists() && !outside.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failing_step_stops_the_rest() {
        let (mut app, dir) = test_app("lsp-file-ops-abort");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let root = app.workspace.root.clone();
        let (x, bin, y) = (root.join("x.rs"), root.join("bin.rs"), root.join("y.txt"));
        std::fs::write(&bin, b"\xff\xfe not text").unwrap();
        std::fs::write(&y, "y").unwrap();
        server.request(
            70,
            "workspace/applyEdit",
            json!({"edit": {"documentChanges": [
                {"kind": "create", "uri": uri(&x)},
                {"textDocument": {"uri": uri(&bin), "version": null}, "edits": [edit(0, 0, 0, 0, "x")]},
                {"kind": "delete", "uri": uri(&y)},
            ]}}),
        );
        handle_next(&mut app, is_lsp).await;
        handle_next(&mut app, applied).await;
        let r = server.response(70).await["result"].clone();
        assert_eq!((r["applied"].clone(), r["failedChange"].clone()), (json!(false), json!(1)), "{r}");
        assert!(r["failureReason"].as_str().unwrap().contains("not valid UTF-8"), "{r}");
        assert!(x.exists(), "the step before stays");
        assert!(y.exists(), "the step after is not made");
        let errors: Vec<&str> = app.notifications.iter().filter(|n| n.level == Level::Error).map(|n| n.text.as_str()).collect();
        assert!(errors.iter().any(|e| e.ends_with("; 1 later change not made")), "{errors:?}");
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

    /// A server `workspace/applyEdit` (request 7) renaming `a` to `b` in `a.rs`, annotated `x`.
    fn annotated_request(server: &FakeServer, a: &Path, needs: bool) {
        server.request(
            7,
            "workspace/applyEdit",
            json!({"label": "Fix", "edit": {
                "documentChanges": [{
                    "textDocument": {"uri": uri(a), "version": 1},
                    "edits": [{"range": range(0, 3, 0, 4), "newText": "b", "annotationId": "x"}]
                }],
                "changeAnnotations": {"x": {"label": "Risky", "needsConfirmation": needs}}
            }}),
        );
    }

    #[tokio::test]
    async fn a_server_edit_needing_confirmation_waits_for_the_user() {
        let (mut app, dir) = test_app("lsp-confirm-apply");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        let a = app.workspace.root.join("a.rs");
        annotated_request(&server, &a, true);
        handle_next(&mut app, is_lsp).await;
        let d = app.dialog.take().expect("the confirm dialog");
        assert!(matches!(d.id, DialogId::LspConfirmEdit(_)));
        assert!(d.message.contains("* Risky") && d.message.contains("1 change in all"), "{}", d.message);
        assert_eq!(text(&app), "fn a() {}\n");
        assert!(sent(&app, &mut server).await.iter().all(|m| m["id"] != 7 || m.get("method").is_some()));
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert_eq!(text(&app), "fn b() {}\n");
        assert_eq!(server.response(7).await["result"]["applied"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn declining_changes_nothing_and_answers_not_applied() {
        let (mut app, dir) = test_app("lsp-confirm-decline");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        let a = app.workspace.root.join("a.rs");
        annotated_request(&server, &a, true);
        handle_next(&mut app, is_lsp).await;
        let d = app.dialog.take().expect("the confirm dialog");
        app.finish_dialog(d.id, DialogResult::Cancel);
        assert_eq!(text(&app), "fn a() {}\n");
        assert_eq!(server.response(7).await["result"], json!({"applied": false, "failureReason": "declined"}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn annotations_without_needs_confirmation_apply_at_once() {
        let (mut app, dir) = test_app("lsp-confirm-not-needed");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        let a = app.workspace.root.join("a.rs");
        annotated_request(&server, &a, false);
        handle_next(&mut app, is_lsp).await;
        assert!(app.dialog.is_none());
        assert_eq!(text(&app), "fn b() {}\n");
        assert_eq!(server.response(7).await["result"]["applied"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Rename preview: the Find window shows the answer first.

    fn preview_msg(m: &AppMsg) -> bool {
        matches!(m, AppMsg::RenamePreview { .. })
    }

    struct Rename {
        app: App,
        dir: PathBuf,
        _servers: tokio::sync::mpsc::UnboundedReceiver<FakeServer>,
        server: FakeServer,
        a: PathBuf,
        b: PathBuf,
    }

    async fn rename_fixture(name: &str) -> Rename {
        let (mut app, dir) = test_app(name);
        let mut servers = with_fake_servers(&mut app, caps());
        let server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let (a, b) = (app.workspace.root.join("a.rs"), app.workspace.root.join("b.rs"));
        std::fs::write(&b, B).unwrap();
        app.editor.active_buffer_mut().unwrap().goto_line(1, 18);
        Rename { app, dir, _servers: servers, server, a, b }
    }

    impl Rename {
        /// Ask for the rename of `old` to `renamed` and answer it with `we`; the dialog's button is returned.
        async fn ask(&mut self, we: Value) -> String {
            let app = &mut self.app;
            app.perform(Action::Lsp(LspAction::Rename));
            answer(app, &mut self.server, "textDocument/prepareRename", range(1, 18, 1, 21)).await;
            let d = app.dialog.take().expect("the rename dialog");
            let DialogKind::Input { ok, .. } = &d.kind else { panic!("{:?}", d.kind) };
            let ok = ok.clone();
            app.finish_dialog(d.id, DialogResult::Text("renamed".into()));
            answer(app, &mut self.server, "textDocument/rename", we).await;
            ok
        }

        async fn preview(&mut self, we: Value) {
            self.ask(we).await;
            handle_next(&mut self.app, preview_msg).await;
        }

        /// Replace All, then the dialog's first button (`Rename`).
        fn replace_all(&mut self) -> crate::ui::dialog::Dialog {
            self.app.perform(Action::Find(crate::actions::FindAction::ReplaceAll));
            let d = self.app.dialog.take().expect("the rename dialog");
            self.app.finish_dialog(d.id.clone(), DialogResult::Button(0));
            d
        }
    }

    /// The edit of a.rs (open, version 1) and b.rs (closed): three edits, two files.
    fn rename_edit(a: &Path, b: &Path) -> Value {
        json!({"documentChanges": [
            {"textDocument": {"uri": uri(a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"textDocument": {"uri": uri(b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed"), edit(1, 6, 1, 9, "renamed")]},
        ],
        "changeAnnotations": {"OutsideWorkspace": {"label": "Edit outside of the workspace", "needsConfirmation": true}}})
    }

    fn rows_of(app: &App) -> Vec<(String, usize, usize)> {
        let name = |p: &Path| p.file_name().unwrap().to_str().unwrap().to_string();
        app.find.included_by_file().into_iter().flat_map(|(p, hits)| hits.into_iter().map(move |h| (name(&p), h.line, h.col))).collect()
    }

    #[tokio::test]
    async fn rename_answer_opens_the_preview_and_writes_nothing() {
        use crate::ui::focus::{PanelId, ToolWindowId};
        let mut fx = rename_fixture("rename-preview-open").await;
        let we = rename_edit(&fx.a, &fx.b);
        let ok = fx.ask(we).await;
        assert_eq!(ok, "Preview");
        handle_next(&mut fx.app, preview_msg).await;
        let app = &fx.app;
        assert!(app.dialog.is_none(), "a referenced needsConfirmation annotation opens no confirmation");
        assert_eq!(text(app), A);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        assert!(app.layout.is_visible(ToolWindowId::Find) && app.focus.is(PanelId::Find));
        let f = &app.find;
        assert_eq!((f.query.text.as_str(), f.replace.text.as_str()), ("old", "renamed"));
        assert!(f.show_replace && f.case_sensitive && f.whole_word && !f.regex && !f.structural);
        assert_eq!(rows_of(app), [("a.rs".to_string(), 1, 17), ("b.rs".to_string(), 0, 7), ("b.rs".to_string(), 1, 6)]);
        let want = "Rename old to renamed: review the results, then Replace All (Alt+A)";
        assert!(toasts(app).contains(&want.to_string()), "{:?}", toasts(app));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn excluded_edit_is_not_applied_and_the_rest_apply_as_one_undo_step() {
        let mut fx = rename_fixture("rename-preview-exclude").await;
        let we = rename_edit(&fx.a, &fx.b);
        fx.preview(we).await;
        // Rows: 0 a.rs, 1 its edit, 2 b.rs, 3 the definition, 4 the comment.
        fx.app.find.list.select(4);
        fx.app.find.toggle_exclude();
        let d = fx.replace_all();
        assert_eq!(d.title, "Rename");
        assert!(d.message.starts_with("Rename old to renamed: 2 occurrences in 2 files; 1 excluded.\n"), "{}", d.message);
        assert_eq!(text(&fx.app), "fn main() {\n    let s = \"😀\"; renamed(s);\n}\n");
        handle_next(&mut fx.app, applied).await;
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 old\n");
        let want = "Renamed to renamed: 2 occurrences in 2 files";
        assert!(toasts(&fx.app).contains(&want.to_string()), "{:?}", toasts(&fx.app));
        let f = &fx.app.find;
        assert!((f.hit_count(), f.rename_preview().is_none(), f.query.text.as_str(), f.replace.text.as_str()) == (0, true, "", ""));
        assert!(!toasts(&fx.app).iter().any(|t| t.starts_with("Replaced")), "the regex path did not run");
        fx.app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&fx.app), A, "one undo step");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn shorthand_edit_shows_and_applies_its_own_text() {
        let mut fx = rename_fixture("rename-preview-shorthand").await;
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&fx.a), "version": 1}, "edits": [edit(1, 18, 1, 18, "old: "), edit(1, 18, 1, 21, "renamed")]},
        ]});
        fx.preview(we).await;
        let hits = fx.app.find.included_by_file().remove(0).1;
        let shown: Vec<_> = hits.iter().map(|h| (h.len, h.matched.as_str(), h.rename.as_ref().unwrap().new_text.as_str())).collect();
        assert_eq!(shown, [(0, "", "old: "), (3, "old", "renamed")]);
        fx.replace_all();
        assert_eq!(text(&fx.app), "fn main() {\n    let s = \"😀\"; old: renamed(s);\n}\n");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    /// a.rs edited, then moved to c.rs, c.rs edited (no row), b.rs edited.
    fn operation_edit(a: &Path, b: &Path, c: &Path) -> Value {
        json!({"documentChanges": [
            {"textDocument": {"uri": uri(a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"kind": "rename", "oldUri": uri(a), "newUri": uri(c)},
            {"textDocument": {"uri": uri(c), "version": null}, "edits": [edit(0, 3, 0, 7, "mn")]},
            {"textDocument": {"uri": uri(b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed")]},
        ]})
    }

    #[tokio::test]
    async fn file_operations_are_listed_and_carried_out_in_order() {
        let mut fx = rename_fixture("rename-preview-ops").await;
        let c = fx.app.workspace.root.join("c.rs");
        let we = operation_edit(&fx.a, &fx.b, &c);
        fx.preview(we).await;
        assert_eq!(rows_of(&fx.app), [("a.rs".to_string(), 1, 17), ("b.rs".to_string(), 0, 7)], "the edit of c.rs has no row");
        let d = fx.replace_all();
        assert_eq!(d.title, "Rename");
        assert!(d.message.contains("rename a.rs to c.rs") && d.message.contains("1 more edit"), "{}", d.message);
        handle_next(&mut fx.app, applied).await;
        assert!(!fx.a.exists());
        assert_eq!(std::fs::read_to_string(&c).unwrap(), A, "unsaved edits are not written");
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 old\n");
        let buf = fx.app.editor.active_buffer().unwrap();
        assert_eq!((buf.path.as_deref(), buf.dirty()), (Some(c.as_path()), true));
        assert_eq!(text(&fx.app), "fn mn() {\n    let s = \"😀\"; renamed(s);\n}\n");
        assert!(toasts(&fx.app).iter().any(|t| t.contains("renamed a.rs to c.rs")), "{:?}", toasts(&fx.app));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn excluding_every_row_still_runs_the_file_operations() {
        let mut fx = rename_fixture("rename-preview-ops-only").await;
        let c = fx.app.workspace.root.join("c.rs");
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&fx.a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"kind": "rename", "oldUri": uri(&fx.a), "newUri": uri(&c)},
            {"textDocument": {"uri": uri(&fx.b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed")]},
        ]});
        fx.preview(we).await;
        for row in [2, 0] {
            fx.app.find.list.select(row);
            fx.app.find.toggle_exclude();
        }
        assert_eq!(fx.app.find.included_count(), 0);
        fx.replace_all();
        handle_next(&mut fx.app, applied).await;
        assert!(!fx.a.exists() && c.exists());
        assert_eq!(text(&fx.app), A);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn stale_buffer_refuses_the_whole_rename() {
        let mut fx = rename_fixture("rename-preview-stale-buffer").await;
        let we = rename_edit(&fx.a, &fx.b);
        fx.preview(we).await;
        fx.app.perform(Action::Editor(EditorAction::InsertText("// ".into())));
        let edited = text(&fx.app);
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Error, "a.rs changed since the preview; rename again"));
        assert_eq!(text(&fx.app), edited);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        assert!(fx.app.find.rename_preview().is_some());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn stale_disk_file_refuses_the_whole_rename() {
        let mut fx = rename_fixture("rename-preview-stale-disk").await;
        let we = rename_edit(&fx.a, &fx.b);
        fx.preview(we).await;
        std::fs::write(&fx.b, format!("{B}// longer\n")).unwrap();
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Error, "b.rs changed since the preview; rename again"));
        assert_eq!(text(&fx.app), A);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), format!("{B}// longer\n"));
        assert!(fx.app.find.rename_preview().is_some());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_rename_that_may_be_skipped_refuses_when_its_target_changed() {
        let mut fx = rename_fixture("rename-preview-ignore-if-exists").await;
        let c = fx.app.workspace.root.join("c.rs");
        std::fs::write(&c, B).unwrap();
        let we = json!({"documentChanges": [
            {"kind": "rename", "oldUri": uri(&fx.b), "newUri": uri(&c), "options": {"ignoreIfExists": true}},
            {"textDocument": {"uri": uri(&c), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed")]},
        ]});
        fx.preview(we).await;
        assert!(rows_of(&fx.app).is_empty(), "the edit of c.rs has no row");
        let changed = format!("{B}// longer\n");
        std::fs::write(&c, &changed).unwrap();
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Error, "c.rs changed since the preview; rename again"));
        assert_eq!(std::fs::read_to_string(&c).unwrap(), changed);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        assert!(fx.app.find.rename_preview().is_some());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn plan_refusal_keeps_the_preview() {
        let mut fx = rename_fixture("rename-preview-plan-refusal").await;
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&fx.a), "version": 7}, "edits": [edit(1, 18, 1, 21, "renamed")]},
        ]});
        fx.preview(we).await;
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!(
            (n.level, n.text.as_str()),
            (Level::Warn, "Rename: a.rs changed since the language server made the edit; nothing was changed")
        );
        assert!(fx.app.find.rename_preview().is_some());
        assert_eq!(text(&fx.app), A);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn key_off_applies_at_once_and_the_button_says_refactor() {
        let mut fx = rename_fixture("rename-preview-off").await;
        fx.app.config.lsp.rename_preview = false;
        let we = rename_edit(&fx.a, &fx.b);
        let ok = fx.ask(we).await;
        assert_eq!(ok, "Refactor");
        assert_eq!(text(&fx.app), "fn main() {\n    let s = \"😀\"; renamed(s);\n}\n");
        assert!(fx.app.find.rename_preview().is_none());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn empty_answer_hints_nothing_to_rename() {
        use crate::ui::focus::ToolWindowId;
        let mut fx = rename_fixture("rename-preview-empty").await;
        fx.preview(json!({"changes": {}})).await;
        assert!(toasts(&fx.app).contains(&"Nothing to rename".to_string()), "{:?}", toasts(&fx.app));
        assert!(!fx.app.layout.is_visible(ToolWindowId::Find) && fx.app.find.rename_preview().is_none());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn an_older_result_is_dropped() {
        use crate::ui::focus::ToolWindowId;
        let mut fx = rename_fixture("rename-preview-older").await;
        let we = rename_edit(&fx.a, &fx.b);
        fx.ask(we).await;
        fx.app.find.reset();
        handle_next(&mut fx.app, preview_msg).await;
        assert!(!fx.app.layout.is_visible(ToolWindowId::Find) && fx.app.find.rename_preview().is_none());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn new_search_after_a_cancelled_dialog_applies_nothing() {
        let mut fx = rename_fixture("rename-preview-cancel").await;
        let we = rename_edit(&fx.a, &fx.b);
        fx.preview(we).await;
        fx.app.perform(Action::Find(crate::actions::FindAction::ReplaceAll));
        let d = fx.app.dialog.take().unwrap();
        fx.app.finish_dialog(d.id, DialogResult::Button(1));
        assert!(fx.app.find.rename_preview().is_some(), "Cancel keeps the preview");
        fx.app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(fx.app.find.rename_preview().is_none());
        assert_eq!(text(&fx.app), A);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    fn stale_dialog() -> Dialog {
        Dialog::confirm(DialogId::ReplaceInFiles, "Replace in Files", "Replace 1 match?", &["Replace", "Cancel"])
    }

    #[tokio::test]
    async fn a_replace_dialog_open_when_the_rename_starts_is_closed() {
        let mut fx = rename_fixture("rename-preview-dialog-start").await;
        fx.app.dialog = Some(stale_dialog());
        fx.app.rename_preview_start(RenameRequest {
            old: "old".into(),
            new: "renamed".into(),
            steps: Vec::new(),
            open: Vec::new(),
            root: fx.app.workspace.root.clone(),
            textual: None,
        });
        assert!(fx.app.dialog.is_none());
        handle_next(&mut fx.app, preview_msg).await;
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_replace_dialog_opened_while_the_preview_builds_is_closed_and_applies_nothing() {
        let mut fx = rename_fixture("rename-preview-dialog-done").await;
        let we = rename_edit(&fx.a, &fx.b);
        fx.ask(we).await;
        fx.app.dialog = Some(stale_dialog());
        handle_next(&mut fx.app, preview_msg).await;
        assert!(fx.app.dialog.is_none(), "the dialog confirmed something else");
        assert!(fx.app.find.rename_preview().is_some());
        assert_eq!(text(&fx.app), A);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        // The preview still asks for its own confirmation.
        fx.replace_all();
        assert_ne!(text(&fx.app), A);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_dialog_of_another_id_survives_the_preview() {
        let mut fx = rename_fixture("rename-preview-dialog-other").await;
        let we = rename_edit(&fx.a, &fx.b);
        fx.ask(we).await;
        fx.app.dialog = Some(Dialog::confirm(DialogId::CloseAllDirty, "Reload", "Reload?", &["Reload", "Cancel"]));
        handle_next(&mut fx.app, preview_msg).await;
        assert!(fx.app.dialog.as_ref().is_some_and(|d| d.id == DialogId::CloseAllDirty));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn an_edit_of_a_renamed_file_without_a_row_refuses_when_the_source_changed() {
        let mut fx = rename_fixture("rename-preview-source-open").await;
        let c = fx.app.workspace.root.join("c.rs");
        let we = json!({"documentChanges": [
            {"kind": "rename", "oldUri": uri(&fx.a), "newUri": uri(&c)},
            {"textDocument": {"uri": uri(&c), "version": null}, "edits": [edit(1, 18, 1, 21, "renamed")]},
        ]});
        fx.preview(we).await;
        assert!(fx.app.find.rename_preview().is_some());
        fx.app.perform(Action::Editor(EditorAction::InsertText("// ".into())));
        let edited = text(&fx.app);
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Error, "a.rs changed since the preview; rename again"));
        assert_eq!(text(&fx.app), edited);
        assert!(fx.a.exists() && !c.exists());
        assert!(fx.app.find.rename_preview().is_some());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn an_edit_of_a_renamed_file_without_a_row_refuses_when_the_source_changed_on_disk() {
        let mut fx = rename_fixture("rename-preview-source-disk").await;
        // The source is closed: b.rs becomes c.rs.
        let c = fx.app.workspace.root.join("c.rs");
        let we = json!({"documentChanges": [
            {"kind": "rename", "oldUri": uri(&fx.b), "newUri": uri(&c)},
            {"textDocument": {"uri": uri(&c), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed")]},
        ]});
        fx.preview(we).await;
        std::fs::write(&fx.b, format!("{B}// longer\n")).unwrap();
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Error, "b.rs changed since the preview; rename again"));
        assert!(fx.b.exists() && !c.exists());
        assert!(fx.app.find.rename_preview().is_some());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn overlapping_edits_of_a_closed_file_are_refused_untouched_and_excluding_one_applies() {
        let mut fx = rename_fixture("rename-preview-overlap").await;
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&fx.a), "version": 1}, "edits": [edit(1, 18, 1, 21, "renamed")]},
            {"textDocument": {"uri": uri(&fx.b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed"), edit(0, 8, 0, 9, "x")]},
        ]});
        fx.preview(we).await;
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Warn, "Rename: the edits of b.rs overlap; nothing was changed"));
        assert_eq!(text(&fx.app), A, "the open file is not edited either");
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        assert!(fx.app.find.rename_preview().is_some());
        // Rows: 0 a.rs, 1 its edit, 2 b.rs, 3 and 4 the pair. Excluding one applies the rest.
        fx.app.find.list.select(4);
        fx.app.find.toggle_exclude();
        fx.replace_all();
        handle_next(&mut fx.app, applied).await;
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B.replacen("old", "renamed", 1));
        assert_eq!(text(&fx.app), "fn main() {\n    let s = \"😀\"; renamed(s);\n}\n");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    // Rename preview: the dialog option "Search in comments and strings" and the textual rows.

    /// `let x = "old"; old(); // old`: textual matches at columns 9 and 25, a code use at 15.
    const D: &str = "let x = \"old\"; old(); // old\n";

    /// Every row: file name, line, column, whether it is a text guess, whether it is excluded.
    fn all_rows(app: &App) -> Vec<(String, usize, usize, bool, bool)> {
        use crate::files::rename_preview::RenameSource;
        app.find
            .hits()
            .iter()
            .map(|h| {
                let text = h.rename.as_ref().is_some_and(|r| r.source == RenameSource::Textual);
                (h.path.file_name().unwrap().to_str().unwrap().to_string(), h.line, h.col, text, h.excluded)
            })
            .collect()
    }

    /// Select the row of file `name`, line 0, column `col` in the Find list (a header row precedes each file).
    fn select_row(app: &mut App, name: &str, line: usize, col: usize) {
        let mut files: Vec<&Path> = Vec::new();
        let mut idx = None;
        for (i, h) in app.find.hits().iter().enumerate() {
            if files.last() != Some(&h.path.as_path()) {
                files.push(&h.path);
            }
            if h.path.file_name().unwrap() == name && (h.line, h.col) == (line, col) {
                idx = Some(i + files.len());
            }
        }
        app.find.list.select(idx.expect("the row"));
    }

    fn write_d(fx: &Rename, text: &str) -> PathBuf {
        let d = fx.app.workspace.root.join("d.rs");
        std::fs::write(&d, text).unwrap();
        d
    }

    #[tokio::test]
    async fn the_rename_dialog_offers_the_option_and_remembers_it() {
        let mut fx = rename_fixture("rename-text-option").await;
        let app = &mut fx.app;
        app.perform(Action::Lsp(LspAction::Rename));
        answer(app, &mut fx.server, "textDocument/prepareRename", range(1, 18, 1, 21)).await;
        let mut d = app.dialog.take().expect("the rename dialog");
        let on = |d: &Dialog| matches!(&d.kind, DialogKind::Input { check: Some(c), .. } if c.on);
        assert!(matches!(&d.kind, DialogKind::Input { check: Some(_), .. }) && !on(&d));
        let alt_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT);
        assert!(d.handle_key(alt_c).is_none() && on(&d));
        let result = d.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).expect("the answer");
        assert!(matches!(&result, DialogResult::TextChecked(t, true) if t == "old"));
        app.finish_dialog(d.id, result);
        assert!(app.lsp.edit.rename_in_text);
        answer(app, &mut fx.server, "textDocument/rename", rename_edit(&fx.a, &fx.b)).await;
        handle_next(app, preview_msg).await;
        // The next dialog opens with the option on; a cancelled toggle stores nothing.
        app.focus.set(crate::ui::focus::PanelId::Editor);
        app.perform(Action::Lsp(LspAction::Rename));
        answer(app, &mut fx.server, "textDocument/prepareRename", range(1, 18, 1, 21)).await;
        let mut d = app.dialog.take().expect("the second dialog");
        let DialogId::LspRename(tag) = d.id.clone() else { panic!("{:?}", d.id) };
        assert!(on(&d));
        assert!(d.handle_key(alt_c).is_none() && !on(&d));
        assert!(matches!(d.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(DialogResult::Cancel)));
        assert!(app.lsp.edit.rename_in_text, "Esc leaves it as it was");
        // With the key off the row is absent.
        app.config.lsp.rename_preview = false;
        app.lsp_rename_dialog(tag, "old");
        assert!(matches!(&app.dialog.as_ref().unwrap().kind, DialogKind::Input { check: None, .. }));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn comment_and_string_matches_start_excluded_and_apply_when_included() {
        let mut fx = rename_fixture("rename-text-apply").await;
        let d = write_d(&fx, D);
        fx.app.lsp.edit.rename_in_text = true;
        let we = rename_edit(&fx.a, &fx.b);
        fx.ask(we).await;
        assert!(toasts(&fx.app).iter().any(|t| t.contains("searching the project for text matches")), "{:?}", toasts(&fx.app));
        assert_eq!(fx.app.find.hit_count(), 0, "no rows before the message");
        handle_next(&mut fx.app, preview_msg).await;
        let rows = all_rows(&fx.app);
        let want = [
            ("a.rs", 1, 17, false, false),
            ("b.rs", 0, 7, false, false),
            ("b.rs", 1, 6, false, false),
            ("d.rs", 0, 9, true, true),
            ("d.rs", 0, 25, true, true),
        ];
        let want: Vec<_> = want.iter().map(|(n, l, c, t, e)| (n.to_string(), *l, *c, *t, *e)).collect();
        assert_eq!(rows, want, "no row at d.rs col 15; B's comment is exact only");
        let ts = toasts(&fx.app);
        assert!(ts.iter().any(|t| t.contains("2 text matches start excluded")), "{ts:?}");
        select_row(&mut fx.app, "d.rs", 0, 25);
        fx.app.find.toggle_exclude();
        let dialog = fx.replace_all();
        assert!(dialog.message.contains("Text matches (guesses): 1 included, 1 left out."), "{}", dialog.message);
        handle_next(&mut fx.app, applied).await;
        assert_eq!(std::fs::read_to_string(&d).unwrap(), "let x = \"old\"; old(); // renamed\n");
        assert_eq!(text(&fx.app), "fn main() {\n    let s = \"😀\"; renamed(s);\n}\n");
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 renamed\n");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn with_the_option_off_only_the_servers_edits_are_listed() {
        let mut fx = rename_fixture("rename-text-off").await;
        write_d(&fx, D);
        assert!(!fx.app.lsp.edit.rename_in_text);
        let we = rename_edit(&fx.a, &fx.b);
        fx.preview(we).await;
        assert!(all_rows(&fx.app).iter().all(|r| !r.3 && r.0 != "d.rs"));
        assert_eq!(fx.app.find.rename_preview().unwrap().textual.rows, 0);
        assert!(!toasts(&fx.app).iter().any(|t| t.contains("text match")), "{:?}", toasts(&fx.app));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn exact_and_text_edits_on_one_line_of_a_closed_file() {
        let mut fx = rename_fixture("rename-text-same-line").await;
        let d = write_d(&fx, D);
        fx.app.lsp.edit.rename_in_text = true;
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&d), "version": null}, "edits": [edit(0, 15, 0, 18, "renamed")]},
        ]});
        fx.preview(we).await;
        for col in [9, 25] {
            select_row(&mut fx.app, "d.rs", 0, col);
            fx.app.find.toggle_exclude();
        }
        fx.replace_all();
        handle_next(&mut fx.app, applied).await;
        assert_eq!(std::fs::read_to_string(&d).unwrap(), "let x = \"renamed\"; renamed(); // renamed\n");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    /// Make the active buffer (a.rs) hold `D`, as unsaved text.
    fn type_d(app: &mut App) {
        app.perform(Action::Editor(EditorAction::SelectAll));
        app.perform(Action::Editor(EditorAction::InsertText(D.into())));
        assert_eq!(text(app), D);
    }

    fn start_in_buffer(app: &mut App, steps: Vec<edits::Step>) {
        let (open, root) = (app.open_texts(), app.workspace.root.clone());
        let textual = Some(TextualStage { scope: TextualScope::CommentsAndStrings, show_hidden: false, show_ignored: false });
        app.rename_preview_start(RenameRequest { old: "old".into(), new: "renamed".into(), steps, open, root, textual });
    }

    #[tokio::test]
    async fn exact_and_text_edits_on_one_line_of_an_open_buffer_are_one_undo_step() {
        let mut fx = rename_fixture("rename-text-open-buffer").await;
        type_d(&mut fx.app);
        let a = fx.a.clone();
        let steps = vec![edits::Step::Edit(edits::FileEdit {
            path: a,
            version: None,
            edits: vec![lsp_types::TextEdit {
                range: lsp_types::Range::new(lsp_types::Position::new(0, 15), lsp_types::Position::new(0, 18)),
                new_text: "renamed".into(),
            }],
            index: None,
        })];
        start_in_buffer(&mut fx.app, steps);
        handle_next(&mut fx.app, preview_msg).await;
        for col in [9, 25] {
            select_row(&mut fx.app, "a.rs", 0, col);
            fx.app.find.toggle_exclude();
        }
        fx.replace_all();
        assert_eq!(text(&fx.app), "let x = \"renamed\"; renamed(); // renamed\n");
        fx.app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&fx.app), D, "one undo step");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_moved_file_takes_its_text_edits_first() {
        let mut fx = rename_fixture("rename-text-moved").await;
        fx.app.lsp.edit.rename_in_text = true;
        let c = fx.app.workspace.root.join("c.rs");
        let we = json!({"documentChanges": [
            {"textDocument": {"uri": uri(&fx.b), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed")]},
            {"kind": "rename", "oldUri": uri(&fx.b), "newUri": uri(&c)},
        ]});
        fx.preview(we).await;
        select_row(&mut fx.app, "b.rs", 1, 6);
        assert!(all_rows(&fx.app).contains(&("b.rs".to_string(), 1, 6, true, true)), "{:?}", all_rows(&fx.app));
        fx.app.find.toggle_exclude();
        fx.replace_all();
        handle_next(&mut fx.app, applied).await;
        assert_eq!(std::fs::read_to_string(&c).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 renamed\n");
        assert!(!fx.b.exists());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn an_edit_after_an_operation_keeps_its_file_out_of_the_text_rows() {
        let mut fx = rename_fixture("rename-text-after-op").await;
        fx.app.lsp.edit.rename_in_text = true;
        let c = fx.app.workspace.root.join("c.rs");
        let we = json!({"documentChanges": [
            {"kind": "rename", "oldUri": uri(&fx.b), "newUri": uri(&c)},
            {"textDocument": {"uri": uri(&c), "version": null}, "edits": [edit(0, 7, 0, 10, "renamed")]},
        ]});
        fx.preview(we).await;
        assert!(all_rows(&fx.app).iter().all(|r| r.0 != "b.rs" && r.0 != "c.rs"), "{:?}", all_rows(&fx.app));
        let d = fx.replace_all();
        assert!(d.message.contains("No text matches offered in 1 file"), "{}", d.message);
        handle_next(&mut fx.app, applied).await;
        assert_eq!(std::fs::read_to_string(&c).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 old\n");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn an_included_text_row_in_a_changed_file_refuses_and_an_excluded_one_does_not() {
        // Excluded: the file changed, everything else applies.
        let mut fx = rename_fixture("rename-text-stale-excluded").await;
        let d = write_d(&fx, D);
        fx.app.lsp.edit.rename_in_text = true;
        let we = rename_edit(&fx.a, &fx.b);
        fx.preview(we.clone()).await;
        std::fs::write(&d, format!("// longer\n{D}")).unwrap();
        fx.replace_all();
        handle_next(&mut fx.app, applied).await;
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), "pub fn renamed(s: &str) {}\n// 日本 renamed\n");
        assert_eq!(std::fs::read_to_string(&d).unwrap(), format!("// longer\n{D}"), "the guess was not applied");
        let _ = std::fs::remove_dir_all(&fx.dir);
        // Included: the whole rename is refused, the preview and its exclusions stay.
        let mut fx = rename_fixture("rename-text-stale-included").await;
        let d = write_d(&fx, D);
        fx.app.lsp.edit.rename_in_text = true;
        fx.preview(we).await;
        select_row(&mut fx.app, "d.rs", 0, 9);
        fx.app.find.toggle_exclude();
        std::fs::write(&d, format!("// longer\n{D}")).unwrap();
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Error, "d.rs changed since the preview; rename again"));
        assert!(fx.app.find.rename_preview().is_some());
        let rows = all_rows(&fx.app);
        assert!(rows.contains(&("d.rs".to_string(), 0, 9, true, false)) && rows.contains(&("d.rs".to_string(), 0, 25, true, true)));
        assert_eq!(text(&fx.app), A);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn an_open_dirty_file_with_a_text_row_refuses_when_typed_in() {
        let mut fx = rename_fixture("rename-text-dirty").await;
        type_d(&mut fx.app);
        start_in_buffer(&mut fx.app, Vec::new());
        handle_next(&mut fx.app, preview_msg).await;
        select_row(&mut fx.app, "a.rs", 0, 9);
        fx.app.find.toggle_exclude();
        fx.app.focus.set(crate::ui::focus::PanelId::Editor);
        fx.app.perform(Action::Editor(EditorAction::InsertText("// ".into())));
        let typed = text(&fx.app);
        fx.replace_all();
        let n = fx.app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (Level::Error, "a.rs changed since the preview; rename again"));
        assert_eq!(text(&fx.app), typed);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_replace_dialog_opened_while_the_text_search_runs_is_closed() {
        let mut fx = rename_fixture("rename-text-dialog-done").await;
        write_d(&fx, D);
        fx.app.lsp.edit.rename_in_text = true;
        let we = rename_edit(&fx.a, &fx.b);
        fx.ask(we).await;
        assert_eq!(fx.app.find.hit_count(), 0);
        fx.app.dialog = Some(stale_dialog());
        handle_next(&mut fx.app, preview_msg).await;
        assert!(fx.app.dialog.is_none());
        assert_eq!(text(&fx.app), A);
        assert_eq!(std::fs::read_to_string(&fx.b).unwrap(), B);
        assert!(fx.app.find.hit_count() > 0);
        assert!(fx.app.find.rename_preview().is_some(), "the preview still needs its own confirmation");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_capped_result_warns() {
        let mut fx = rename_fixture("rename-text-capped").await;
        let n = fx.app.workspace.root.join("n.txt");
        let edit = lsp_types::TextEdit { range: lsp_types::Range::default(), new_text: "renamed".into() };
        let hit = Hit {
            path: n,
            line: 0,
            col: 0,
            len: 3,
            preview: "old".into(),
            preview_col: 0,
            matched: "old".into(),
            excluded: true,
            rename: Some(Box::new(rename_preview::RenameEdit {
                new_text: "renamed".into(),
                edit,
                at: None,
                source: rename_preview::RenameSource::Textual,
            })),
        };
        let mut preview = RenamePreview {
            old: "old".into(),
            new: "renamed".into(),
            steps: Vec::new(),
            stamps: HashMap::new(),
            unlisted: HashSet::new(),
            textual: Default::default(),
        };
        preview.textual.capped = true;
        preview.textual.rows = 1;
        let (generation, seq) = (fx.app.workspace.generation, fx.app.find.current_seq());
        fx.app.rename_preview_done(generation, seq, Ok((preview, vec![hit])));
        let last = fx.app.notifications.last().unwrap();
        assert_eq!((last.level, last.text.as_str()), (Level::Warn, "Text search stopped at 10000 matches; some files were not searched"));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }
}
