//! Language server navigation: go to definition (Ctrl+B, Ctrl+click), find
//! usages (Alt+F7), quick documentation (Alt+Q) and quick definition
//! (Ctrl+Shift+I), all about the active buffer's caret.
//!
//! A request is sent after a sync, so the server has the text it refers
//! to, and carries a [`DocTag`]: its answer is used only while the buffer,
//! its version and the caret are what they were, and only for the newest
//! request of its method ([`super::lsp::Inflight`]; a newer one cancels the
//! older with `$/cancelRequest`). Locations open in the Locations palette
//! with a preview of their line (from the open buffer, else read from disk
//! off the main thread); a single definition is jumped to, and a definition
//! that is where the caret already is lists the usages instead.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{Position, Uri};
use serde_json::{Value, json};

use super::App;
use super::lsp_edit::ActionsFor;
use crate::actions::PaletteKind;
use crate::config::theme::Theme;
use crate::editor::buffer::BufferId;
use crate::editor::syntax::{self, LanguageDef};
use crate::event::{AppMsg, Level};
use crate::lsp::locations::{self, LocationItem, Target};
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag, pos};
use crate::symbols::scip::{self, Occ, SymId};
use crate::ui::focus::PanelId;
use crate::ui::hover::{DocLine, HoverPopup, Kind, code_lines};
use crate::ui::palette::Palette;

pub(super) use crate::lsp::caps::{
    CODE_ACTION, CODE_ACTION_RESOLVE, COMPLETION, DOCUMENT_SYMBOL, EXECUTE_COMMAND, FORMATTING, INLAY_HINT, PREPARE_RENAME,
    RANGE_FORMATTING, RENAME, RESOLVE, SEMANTIC_TOKENS_FULL, SEMANTIC_TOKENS_RANGE,
};
use crate::lsp::caps::{
    Caps, DECLARATION, DEFINITION, HOVER, IMPLEMENTATION, PREPARE_CALL_HIERARCHY, PREPARE_TYPE_HIERARCHY, REFERENCES, SIGNATURE_HELP,
    TYPE_DEFINITION, WORKSPACE_SYMBOL,
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
        PendingTag::QuickDefinition(_) => DEFINITION,
        PendingTag::SignatureHelp(_) => SIGNATURE_HELP,
        PendingTag::Completion(_) => COMPLETION,
        PendingTag::CompletionResolve(_) | PendingTag::CompletionDocs(_) => RESOLVE,
        PendingTag::PrepareRename(_) => PREPARE_RENAME,
        PendingTag::Rename(_) => RENAME,
        PendingTag::CodeAction(_) | PendingTag::Refactor(_) | PendingTag::OrganizeImports(_) => CODE_ACTION,
        PendingTag::CodeActionResolve(_) => CODE_ACTION_RESOLVE,
        PendingTag::ExecuteCommand(_) => EXECUTE_COMMAND,
        PendingTag::DocumentSymbols(_) => DOCUMENT_SYMBOL,
        PendingTag::Formatting(_) => FORMATTING,
        PendingTag::Lightbulb(_) => CODE_ACTION,
        PendingTag::InlayHints(_) => INLAY_HINT,
        // Never looked up: routed before the inflight check.
        PendingTag::SemanticTokens(_) => SEMANTIC_TOKENS_FULL,
        PendingTag::Initialize => "initialize",
        PendingTag::Shutdown => "shutdown",
        PendingTag::WillFileOp(_) => "workspace/will*Files",
        PendingTag::WorkspaceSymbols(_) => WORKSPACE_SYMBOL,
        PendingTag::CallHierarchy(_) => PREPARE_CALL_HIERARCHY,
        PendingTag::TypeHierarchy(_) => PREPARE_TYPE_HIERARCHY,
        // Never looked up: routed before `method_of`.
        PendingTag::HierarchyChildren { .. } => "hierarchy children",
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
        SEMANTIC_TOKENS_FULL | SEMANTIC_TOKENS_RANGE => "semantic highlighting",
        RANGE_FORMATTING => "formatting a selection",
        PREPARE_CALL_HIERARCHY => "call hierarchy",
        PREPARE_TYPE_HIERARCHY => "type hierarchy",
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
    /// the file, the SCIP index answers when it is fresh for the file; else
    /// Go to Symbol opens on the word at the caret.
    pub(super) fn lsp_goto_definition(&mut self) {
        let Some((id, pos)) = self.caret_position() else { return };
        match self.lsp_doc_request(id, pos, DEFINITION, json!({}), PendingTag::Definition) {
            Ok(seq) => self.lsp.nav_seq = seq,
            Err(why) => {
                if !self.lsp.docs.contains_key(&id) && self.scip_definition(id) {
                    return;
                }
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

    /// `lsp.find_usages` (Alt+F7); without a server, unverified candidates
    /// from the symbol index.
    pub(super) fn lsp_find_usages(&mut self) {
        let Some((id, pos)) = self.caret_position() else { return };
        self.lsp_request_usages(id, pos);
    }

    fn lsp_request_usages(&mut self, id: BufferId, pos: Position) {
        let extra = json!({"context": {"includeDeclaration": false}});
        match self.lsp_doc_request(id, pos, REFERENCES, extra, PendingTag::References) {
            Ok(seq) => self.lsp.nav_seq = seq,
            Err(why) => {
                if !self.lsp_usages_without_server(id) && !why.is_empty() {
                    self.lsp_hint(why);
                }
            }
        }
    }

    /// Find Usages when no server document serves buffer `id`: the SCIP
    /// index when it knows the caret's symbol, else (if the language has a
    /// tags query and the caret is on a word) unverified candidates from the
    /// symbol index. True when handled.
    fn lsp_usages_without_server(&mut self, id: BufferId) -> bool {
        if self.lsp.docs.contains_key(&id) {
            return false;
        }
        if self.scip_usages(id) {
            return true;
        }
        let Some(b) = self.editor.buffers.iter().find(|b| b.id == id) else { return false };
        let (Some(path), Some(def)) = (b.path.clone(), b.language()) else { return false };
        let word = word_at(b, b.view.sel().head);
        if def.tags.is_none() || word.is_empty() {
            return false;
        }
        let mut items = self.fallback_usages(&word, &path);
        match items.len() {
            0 if self.symbols.indexing() => self.lsp_hint("No usages found yet; the project is still being indexed"),
            0 => self.lsp_hint("No usages found"),
            1 => {
                let one = items.remove(0);
                self.editor.push_nav_here();
                self.open_file_at(one.path, one.line, one.col);
                self.lsp_hint("Only one usage (unverified)");
            }
            _ => self.open_locations(format!("Usages of {word} (unverified)"), items, true),
        }
        true
    }

    /// Uses of `name` found by the symbol index, for Find Usages without a
    /// server. Ranked: the current file `from`, files it imports or that
    /// import it, its directory, the rest; within a group by path, line,
    /// column. Definitions are not listed. Dirty open buffers are parsed
    /// live (the index knows the saved text only). No cap; opens nothing.
    pub(crate) fn fallback_usages(&mut self, name: &str, from: &Path) -> Vec<LocationItem> {
        let root = self.workspace.root.clone();
        let rel_of = |p: &Path| p.strip_prefix(&root).ok().map(|r| r.to_string_lossy().replace('\\', "/"));
        let from_rel = rel_of(from);
        let mut near: HashSet<Arc<str>> = HashSet::new();
        if let Some(r) = &from_rel {
            near.extend(self.symbols.imports_of(r));
            near.extend(self.symbols.imported_by(r));
        }
        let mut live: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        let mut parser = tree_sitter::Parser::new();
        for b in &self.editor.buffers {
            let (Some(path), Some(def)) = (&b.path, b.language()) else { continue };
            let Some(rel) = rel_of(path) else { continue };
            if !b.dirty() || def.tags.is_none() || b.rope.len_bytes() as u64 > crate::symbols::index::MAX_FILE_BYTES {
                continue;
            }
            let refs = crate::symbols::extract_text_all(def, &mut parser, &b.rope.to_string()).1;
            live.insert(rel, refs.iter().filter(|r| &*r.name == name).map(|r| (r.line, r.col)).collect());
        }
        let mut hits: Vec<(u8, String, usize, usize)> = Vec::new();
        for (rel, r) in self.symbols.refs_named(name) {
            if !live.contains_key(rel) {
                hits.push((usage_group(rel, from_rel.as_deref(), &near), rel.to_string(), r.line, r.col));
            }
        }
        for (rel, at) in &live {
            let group = usage_group(rel, from_rel.as_deref(), &near);
            hits.extend(at.iter().map(|&(line, col)| (group, rel.clone(), line, col)));
        }
        hits.sort();
        let len = name.chars().count();
        let mut disk: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut items = Vec::with_capacity(hits.len());
        for (_, rel, line, col) in hits {
            let abs: PathBuf = root.join(&rel);
            let text = self.line_of(&abs, line, &mut disk);
            items.push(LocationItem::at_name(abs, line, col, len, text.as_deref()));
        }
        items
    }

    /// Text of line `line` of `abs` (no line break): the open buffer's, else
    /// the file's on disk, read once per file into `disk`.
    fn line_of(&self, abs: &Path, line: usize, disk: &mut HashMap<PathBuf, Option<String>>) -> Option<String> {
        let open = self.editor.index_of(abs).map(|i| &self.editor.buffers[i]).filter(|b| line < b.rope.len_lines());
        if let Some(b) = open {
            return Some(b.line_text(line));
        }
        disk.entry(abs.to_path_buf())
            .or_insert_with(|| std::fs::read(abs).ok().map(|b| String::from_utf8_lossy(&b).into_owned()))
            .as_deref()
            .and_then(|s| crate::util::text::lines_inclusive(s).nth(line).map(crate::git::diff::strip_eol))
            .map(str::to_string)
    }

    /// The SCIP occurrence under the caret of buffer `id`: its path relative
    /// to the root, symbol and whether it is a definition. None without an
    /// index, when the buffer's file is not fresh or the caret is on no
    /// occurrence. Starts a reload when the index file changed on disk (this
    /// lookup still uses the loaded index).
    fn scip_caret(&mut self, id: BufferId) -> Option<(String, SymId, bool)> {
        self.scip_check_reload();
        self.scip.index.as_ref()?;
        let b = self.editor.buffers.iter().find(|b| b.id == id)?;
        let rel = b.path.as_ref()?.strip_prefix(&self.workspace.root).ok()?.to_string_lossy().replace('\\', "/");
        if !self.scip_fresh(&rel) {
            return None;
        }
        let ix = self.scip.index.as_ref()?;
        let doc = ix.doc(&rel)?;
        let (line, col) = crate::editor::selection::pos_to_line_col(&b.rope, b.view.sel().head);
        let unit = scip::to_units(&b.line_text(line), col, doc.encoding);
        let occ = ix.occurrence_at(&rel, line as u32, unit)?;
        let (symbol, definition) = (occ.symbol, occ.definition);
        Some((rel, symbol, definition))
    }

    /// The row for a SCIP occurrence in `rel`, columns converted to chars with
    /// the line's text; `word_len` marks a multi-line span.
    fn scip_item(&self, rel: &str, occ: &Occ, word_len: usize, disk: &mut HashMap<PathBuf, Option<String>>) -> LocationItem {
        let abs = self.workspace.root.join(rel);
        let line = occ.span.line as usize;
        let text = self.line_of(&abs, line, disk);
        let enc = self.scip.index.as_ref().and_then(|ix| ix.doc(rel)).map_or(scip::Encoding::Utf16, |d| d.encoding);
        let (col, len) = match &text {
            Some(t) => {
                let col = scip::to_chars(t, occ.span.start, enc);
                let len = if occ.span.single_line() { scip::to_chars(t, occ.span.end, enc).saturating_sub(col) } else { word_len };
                (col, len)
            }
            None => (occ.span.start as usize, word_len),
        };
        LocationItem::at_name(abs, line, col, len, text.as_deref())
    }

    /// Go to Definition without a server document, from the SCIP index. On a
    /// definition: its usages. Definitions in files that are not fresh come
    /// from the symbol index instead. True when handled (the caller falls
    /// back to the Go to Symbol palette otherwise).
    fn scip_definition(&mut self, id: BufferId) -> bool {
        let Some((_, sym, definition)) = self.scip_caret(id) else { return false };
        if definition {
            return self.lsp_usages_without_server(id);
        }
        let word = self.editor.buffers.iter().find(|b| b.id == id).map(|b| word_at(b, b.view.sel().head)).unwrap_or_default();
        let wlen = word.chars().count();
        let root = self.workspace.root.clone();
        let defs: Vec<(String, Occ)> = match &self.scip.index {
            Some(ix) => ix.occurrences_of(sym).into_iter().filter(|(_, o)| o.definition).map(|(r, o)| (r.to_string(), o.clone())).collect(),
            None => return false,
        };
        let mut disk = HashMap::new();
        let mut seen_stale: HashSet<String> = HashSet::new();
        let mut items: Vec<LocationItem> = Vec::new();
        for (rel, occ) in &defs {
            if self.scip_fresh(rel) {
                items.push(self.scip_item(rel, occ, wlen, &mut disk));
            } else if seen_stale.insert(rel.clone()) {
                for (r, s) in self.symbols.defs_named(&word) {
                    if r == rel {
                        let abs = root.join(rel);
                        let text = self.line_of(&abs, s.line, &mut disk);
                        items.push(LocationItem::at_name(abs, s.line, s.col, wlen, text.as_deref()));
                    }
                }
            }
        }
        let mut keys = HashSet::new();
        items.retain(|i| keys.insert((i.path.clone(), i.line, i.col)));
        match items.len() {
            0 => return false,
            1 => {
                let one = items.remove(0);
                self.editor.push_nav_here();
                self.open_file_at(one.path, one.line, one.col);
            }
            _ => self.show_locations("Choose Declaration".into(), items),
        }
        true
    }

    /// Find Usages from the SCIP index: exact occurrences in fresh files plus
    /// the unverified symbol-index candidates of files that are not fresh,
    /// ranked as `fallback_usages` does. True when handled.
    fn scip_usages(&mut self, id: BufferId) -> bool {
        let Some((from_rel, sym, _)) = self.scip_caret(id) else { return false };
        let Some(b) = self.editor.buffers.iter().find(|b| b.id == id) else { return false };
        let Some(path) = b.path.clone() else { return false };
        let word = word_at(b, b.view.sel().head);
        let has_tags = b.language().is_some_and(|d| d.tags.is_some());
        let wlen = word.chars().count();
        let root = self.workspace.root.clone();
        let occs: Vec<(String, Occ)> = match &self.scip.index {
            Some(ix) => {
                ix.occurrences_of(sym).into_iter().filter(|(_, o)| !o.definition).map(|(r, o)| (r.to_string(), o.clone())).collect()
            }
            None => return false,
        };
        let mut near: HashSet<Arc<str>> = HashSet::new();
        near.extend(self.symbols.imports_of(&from_rel));
        near.extend(self.symbols.imported_by(&from_rel));
        let mut fresh: HashMap<String, bool> = HashMap::new();
        let mut disk = HashMap::new();
        let mut items: Vec<(String, LocationItem)> = Vec::new();
        for (rel, occ) in &occs {
            let ok = *fresh.entry(rel.clone()).or_insert_with(|| self.scip_fresh(rel));
            if ok {
                items.push((rel.clone(), self.scip_item(rel, occ, wlen, &mut disk)));
            }
        }
        if has_tags && !word.is_empty() {
            for item in self.fallback_usages(&word, &path) {
                let Ok(rel) = item.path.strip_prefix(&root) else { continue };
                let rel = rel.to_string_lossy().replace('\\', "/");
                if !*fresh.entry(rel.clone()).or_insert_with(|| self.scip_fresh(&rel)) {
                    items.push((rel, item));
                }
            }
        }
        let mut keys = HashSet::new();
        items.retain(|(_, i)| keys.insert((i.path.clone(), i.line, i.col)));
        items.sort_by_cached_key(|(rel, i)| (usage_group(rel, Some(&from_rel), &near), i.path.clone(), i.line, i.col));
        let mut items: Vec<LocationItem> = items.into_iter().map(|(_, i)| i).collect();
        match items.len() {
            0 => self.lsp_hint("No usages found"),
            1 => {
                let one = items.remove(0);
                self.editor.push_nav_here();
                self.open_file_at(one.path, one.line, one.col);
                self.lsp_hint("Only one usage");
            }
            _ => {
                let title = if word.is_empty() { "Usages (SCIP)".to_string() } else { format!("Usages of {word} (SCIP)") };
                self.open_locations(title, items, true);
            }
        }
        true
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

    /// `lsp.quick_definition` (Ctrl+Shift+I, Alt+Shift+I).
    pub(super) fn lsp_quick_definition(&mut self) {
        self.hover = None;
        self.completion_close();
        let Some((id, pos)) = self.caret_position() else { return };
        match self.lsp_doc_request(id, pos, DEFINITION, json!({}), PendingTag::QuickDefinition) {
            Ok(seq) => self.lsp.nav_seq = seq,
            Err(why) if !why.is_empty() => self.lsp_hint(why),
            Err(_) => {}
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
        if let PendingTag::HierarchyChildren { tree, node } = tag {
            return self.hierarchy_children_answer(name, tree, node, result);
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
        // And the semantic tokens.
        if let PendingTag::SemanticTokens(t) = tag {
            return self.semantic_answer(t, result);
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
            Err(e) if e.code == ResponseError::METHOD_NOT_FOUND && matches!(tag, PendingTag::TypeHierarchy(_)) => {
                return self.lsp_hint(format!("{name} has no type hierarchy"));
            }
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
            PendingTag::QuickDefinition(t) => self.lsp_quick_definition_answer(t, value),
            PendingTag::SignatureHelp(t) => self.signature_answer(t, value),
            PendingTag::PrepareRename(t) => self.lsp_prepare_rename_answer(t, value),
            PendingTag::Rename(t) => self.lsp_rename_answer(t, value),
            PendingTag::CodeAction(t) => self.lsp_code_actions_answer(t, value, ActionsFor::Context),
            PendingTag::Refactor(t) => self.lsp_code_actions_answer(t, value, ActionsFor::Refactor),
            PendingTag::OrganizeImports(t) => self.lsp_code_actions_answer(t, value, ActionsFor::OrganizeImports),
            PendingTag::CodeActionResolve(t) => self.lsp_code_action_resolved(t, value),
            PendingTag::DocumentSymbols(t) => self.lsp_document_symbols_answer(t, value),
            PendingTag::CallHierarchy(_) => self.hierarchy_prepared(name, true, value),
            PendingTag::TypeHierarchy(_) => self.hierarchy_prepared(name, false, value),
            PendingTag::Completion(_)
            | PendingTag::CompletionResolve(_)
            | PendingTag::CompletionDocs(_)
            | PendingTag::ExecuteCommand(_)
            | PendingTag::Formatting(_)
            | PendingTag::Lightbulb(_)
            | PendingTag::InlayHints(_)
            | PendingTag::SemanticTokens(_)
            | PendingTag::WillFileOp(_)
            | PendingTag::WorkspaceSymbols(_)
            | PendingTag::HierarchyChildren { .. }
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

    /// The first target's text (the open buffer's, else the file's) is cut
    /// off the main thread and comes back as `AppMsg::QuickDefinition`.
    fn lsp_quick_definition_answer(&mut self, t: DocTag, value: Value) {
        let Some(target) = locations::parse(value).into_iter().next() else { return self.lsp_hint("No definition found") };
        let line = target.range.start.line as usize;
        let shown = target.path.strip_prefix(&self.workspace.root).unwrap_or(&target.path).display().to_string();
        let heading = format!("{shown}:{}", line + 1);
        let (rope, def) = match self.editor.index_of(&target.path).map(|i| &self.editor.buffers[i]) {
            Some(b) => (Some(b.rope.clone()), b.language()),
            None => (None, None),
        };
        let (tx, generation) = (self.bus_tx.clone(), self.workspace.generation);
        tokio::task::spawn_blocking(move || {
            let text = match rope {
                Some(r) => Ok(r.to_string()),
                None => std::fs::read_to_string(&target.path).map_err(|e| format!("Cannot read {shown}: {e}")),
            };
            let (code, lang) = match text {
                Ok(text) => {
                    let def = def.or_else(|| syntax::detect_text(&target.path, &text));
                    (Ok(definition_snippet(&text, def, line)), def.map(|d| d.name))
                }
                Err(e) => (Err(e), None),
            };
            let _ = tx.send(AppMsg::QuickDefinition { generation, tag: t, heading, code, lang });
        });
    }

    /// `AppMsg::QuickDefinition`: shown unless the root, the newest
    /// definition request or the caret, version or group changed.
    pub(super) fn quick_definition_loaded(
        &mut self,
        generation: u64,
        tag: DocTag,
        heading: String,
        code: Result<String, String>,
        lang: Option<&'static str>,
    ) {
        if generation != self.workspace.generation
            || tag.seq != self.lsp.nav_seq
            || self.caret_position() != Some((tag.buffer, tag.pos))
            || self.editor.active_buffer().is_none_or(|b| b.version != tag.version)
            || self.editor.focused_group().id != tag.group
            || self.dialog.is_some()
            || self.palette.is_some()
        {
            return tracing::debug!("dropping a stale quick definition");
        }
        let code = match code {
            Ok(c) => c,
            Err(why) => return self.lsp_hint(why),
        };
        let Some(head) = self.editor.active_buffer().map(|b| b.view.sel().head) else { return };
        let lines = definition_lines(&heading, &code, lang.and_then(syntax::language_for_name), &self.theme);
        self.hover = HoverPopup::from_lines(tag.group, tag.buffer, tag.version, head, lines);
        self.dirty_frame = true;
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
        items.sort_by(|a, b| (&a.path, a.line, a.col).cmp(&(&b.path, b.line, b.col)));
        self.open_locations(title, items, false);
    }

    /// Open the Locations palette on `items` as given: with `in_order` flat
    /// in that order, else grouped by file. A dialog that came up meanwhile
    /// keeps the input; nothing opens.
    pub(crate) fn open_locations(&mut self, title: String, items: Vec<LocationItem>, in_order: bool) {
        if self.dialog.is_some() {
            return;
        }
        let mut p = Palette::new(PaletteKind::Locations, self.workspace.root.clone());
        p.file_icons = self.config.ui.file_icons;
        if in_order {
            p.set_locations_in_order(title, items);
        } else {
            p.set_locations(title, items);
        }
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
        let overlay = self.editor_overlay_open();
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

/// Rank group of a fallback usage in `rel`: 0 the current file `from`, 1 a file
/// `from` imports or that imports it (`near`), 2 `from`'s directory, 3 the rest.
fn usage_group(rel: &str, from: Option<&str>, near: &HashSet<Arc<str>>) -> u8 {
    let Some(from) = from else { return 3 };
    let dir = |p: &str| p.rsplit_once('/').map_or("", |(d, _)| d).to_string();
    if rel == from {
        0
    } else if near.contains(rel) {
        1
    } else if dir(rel) == dir(from) {
        2
    } else {
        3
    }
}

/// The identifier at char `at` of `b` (empty when there is none).
pub(super) fn word_at(b: &crate::editor::buffer::Buffer, at: usize) -> String {
    let r = crate::editor::selection::word_at(&b.rope, at);
    let w = b.rope.slice(r).to_string();
    if w.chars().all(|c| c.is_alphanumeric() || c == '_') { w } else { String::new() }
}

/// The popup's rows: a heading with the target's place, then its code.
pub(super) fn definition_lines(heading: &str, code: &str, def: Option<&'static LanguageDef>, theme: &Theme) -> Vec<DocLine> {
    let mut lines = vec![DocLine { kind: Kind::Heading, text: heading.to_string(), spans: Vec::new() }];
    lines.extend(code_lines(code, def, theme));
    lines
}

/// The lines of `text` from `line` to the end of the definition named there
/// (its tags symbol's last line), else 15 lines; at most 200.
fn definition_snippet(text: &str, def: Option<&'static LanguageDef>, line: usize) -> String {
    let symbols = match def {
        Some(def) => crate::symbols::extract_text(def, &mut tree_sitter::Parser::new(), text),
        None => Vec::new(),
    };
    let symbol = symbols.iter().find(|s| s.line == line).or_else(|| symbols.iter().find(|s| s.start_line == line));
    let end = symbol.map_or(line + 14, |s| s.end_line).max(line).min(line + 199);
    text.lines().skip(line).take(end - line + 1).collect::<Vec<_>>().join("\n")
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

    fn popup(app: &App) -> Vec<String> {
        app.hover.as_ref().unwrap().lines().iter().map(|l| format!("{:?}:{}", l.kind, l.text)).collect()
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

    #[test]
    fn usage_groups_rank_current_edges_directory_rest() {
        let near: HashSet<Arc<str>> = ["src/util.rs", "lib/in.rs"].into_iter().map(Arc::from).collect();
        let g = |rel, from| usage_group(rel, from, &near);
        let from = Some("src/main.rs");
        assert_eq!(g("src/main.rs", from), 0);
        assert_eq!((g("src/util.rs", from), g("lib/in.rs", from)), (1, 1));
        assert_eq!(g("src/same.rs", from), 2);
        assert_eq!(g("lib/far.rs", from), 3);
        assert_eq!(g("top.rs", from), 3);
        assert_eq!(g("top.rs", Some("other.rs")), 2, "top-level directory");
        assert_eq!(g("src/main.rs", None), 3);
    }

    #[tokio::test]
    async fn find_usages_without_a_server_ranks_index_candidates() {
        use crate::event::AppMsg;
        const MAIN: &str = "mod util;\nfn helper() {}\nfn main() {\n    helper();\n}\n";
        let (mut app, dir) = test_app("usages-fallback");
        let (root, g) = (app.workspace.root.clone(), app.workspace.generation);
        for d in ["src", "lib"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let call = "fn f() {\n    helper();\n}\n";
        std::fs::write(root.join("src/util.rs"), call).unwrap();
        std::fs::write(root.join("src/same.rs"), call).unwrap();
        std::fs::write(root.join("lib/far.rs"), call).unwrap();
        let files = ["lib/far.rs", "src/main.rs", "src/same.rs", "src/util.rs"];
        std::fs::write(root.join("src/main.rs"), MAIN).unwrap();
        app.handle_msg(AppMsg::FileIndex { generation: g, seq: 1, files: files.iter().map(|s| s.to_string()).collect() });
        let job = app.symbols.start(root.clone());
        let (seq, mut batches) = (job.seq, Vec::new());
        job.run(|b| batches.push(b));
        for batch in batches {
            app.handle_msg(AppMsg::Symbols { generation: g, seq, batch });
        }
        let main = crate::app::tests::load(&mut app, "src/main.rs", MAIN);
        goto(&mut app, 3, 6);
        act(&mut app, LspAction::FindUsages);
        let at = |f: &str, l| (root.join(f), l);
        let p = app.palette.as_ref().expect("Locations");
        assert_eq!(p.kind, PaletteKind::Locations);
        assert_eq!(p.shown_locations(), [at("src/main.rs", 3), at("src/util.rs", 1), at("src/same.rs", 1), at("lib/far.rs", 1)]);
        let buf = draw(&mut app, 100, 30);
        assert!((0..30).any(|y| row(&buf, y).contains("Usages of helper (unverified)")));
        // Unsaved text is parsed live.
        app.palette = None;
        goto(&mut app, 4, 0);
        app.perform(Action::Editor(EditorAction::InsertText("    helper();\n".into())));
        goto(&mut app, 3, 6);
        act(&mut app, LspAction::FindUsages);
        let shown = app.palette.as_ref().unwrap().shown_locations();
        assert_eq!(shown[..2], [(main.clone(), 3), (main, 4)]);
        // No candidates, then one.
        app.palette = None;
        app.toasts = Default::default();
        crate::app::tests::load(&mut app, "src/two.rs", "fn k() {\n    only_here();\n}\n");
        goto(&mut app, 1, 6);
        act(&mut app, LspAction::FindUsages);
        assert!(toasts(&app).contains(&"No usages found".to_string()), "{:?}", toasts(&app));
        goto(&mut app, 2, 0);
        app.perform(Action::Editor(EditorAction::InsertText("\n".into())));
        goto(&mut app, 1, 6);
        app.toasts = Default::default();
        act(&mut app, LspAction::FindUsages);
        assert!(toasts(&app).contains(&"Only one usage (unverified)".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    const SCIP_MAIN: &str = "mod util;\nfn main() {\n    let s = \"日本😀\"; util::helper(s);\n    util::helper(s);\n}\n";
    const SCIP_UTIL: &str = "/* 😀 */ pub fn helper(_s: &str) {}\n";

    fn set_mtime(p: &Path, t: std::time::SystemTime) {
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    fn run_tags_job(app: &mut App) {
        let (root, g) = (app.workspace.root.clone(), app.workspace.generation);
        let job = app.symbols.start(root);
        let (seq, mut batches) = (job.seq, Vec::new());
        job.run(|b| batches.push(b));
        for batch in batches {
            app.handle_msg(AppMsg::Symbols { generation: g, seq, batch });
        }
    }

    /// A project with `src/main.rs` (open, caret file) and `src/util.rs`, a
    /// loaded SCIP index in between them in age, and the tags index built.
    async fn scip_app(name: &str) -> (App, PathBuf, PathBuf) {
        let (mut app, dir) = test_app(name);
        let (root, g) = (app.workspace.root.clone(), app.workspace.generation);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), SCIP_MAIN).unwrap();
        std::fs::write(root.join("src/util.rs"), SCIP_UTIL).unwrap();
        app.handle_msg(AppMsg::FileIndex { generation: g, seq: 1, files: vec!["src/main.rs".into(), "src/util.rs".into()] });
        run_tags_job(&mut app);
        let helper: &[(&[i32], &str, bool)] = &[(&[0, 18, 24], "t util/helper().", true)];
        let main: &[(&[i32], &str, bool)] = &[
            (&[1, 3, 7], "t main().", true),
            (&[2, 8, 9], "local 0", true),
            (&[2, 32, 38], "t util/helper().", false),
            (&[2, 39, 40], "local 0", false),
            (&[3, 10, 16], "t util/helper().", false),
            (&[0, 0, 5, 0], "t crate/", true),
        ];
        let index = root.join("index.scip");
        crate::symbols::scip::write_test_index(
            &index,
            "",
            &[("src/main.rs", scip::Encoding::Utf8, main), ("src/util.rs", scip::Encoding::Utf8, helper)],
        );
        let now = std::time::SystemTime::now();
        let ago = |s| now - std::time::Duration::from_secs(s);
        set_mtime(&root.join("src/util.rs"), ago(100));
        set_mtime(&index, ago(50));
        let mut c = app.config.clone();
        c.index.scip = "index.scip".into();
        app.apply_config(c);
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert!(app.scip.index.is_some(), "{:?}", toasts(&app));
        crate::app::tests::load(&mut app, "src/main.rs", SCIP_MAIN);
        set_mtime(&root.join("src/main.rs"), ago(100));
        (app, dir, root)
    }

    #[tokio::test]
    async fn scip_definition_and_usages_without_a_server() {
        let (mut app, dir, root) = scip_app("scip-nav").await;
        let at = |f: &str, l| (root.join(f), l);
        goto(&mut app, 3, 12);
        act(&mut app, LspAction::FindUsages);
        let p = app.palette.as_ref().expect("Locations");
        assert_eq!(p.kind, PaletteKind::Locations);
        assert_eq!(p.shown_locations(), [at("src/main.rs", 2), at("src/main.rs", 3)]);
        let buf = draw(&mut app, 100, 30);
        assert!((0..30).any(|y| row(&buf, y).contains("Usages of helper (SCIP)")));
        app.palette = None;
        goto(&mut app, 2, 26);
        act(&mut app, LspAction::GotoDefinition);
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_buffer().unwrap().path, Some(root.join("src/util.rs")));
        assert_eq!(caret(&app), (0, 15));
        // On the declaration: its usages.
        goto(&mut app, 0, 16);
        act(&mut app, LspAction::GotoDefinition);
        let p = app.palette.as_ref().expect("Locations");
        assert_eq!(p.shown_locations(), [at("src/main.rs", 2), at("src/main.rs", 3)]);
        let buf = draw(&mut app, 100, 30);
        assert!((0..30).any(|y| row(&buf, y).contains("Usages of helper (SCIP)")));
        // The index file changed on disk: the next lookup reloads it.
        app.palette = None;
        let newer = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        set_mtime(&root.join("index.scip"), newer);
        act(&mut app, LspAction::FindUsages);
        assert!(app.scip.loading);
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert!(!app.scip.loading);
        let loaded = app.scip.index.as_ref().unwrap().mtime();
        assert!(loaded > newer - std::time::Duration::from_secs(1) && loaded < newer + std::time::Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_files_fall_back_to_the_tags_index() {
        let (mut app, dir, root) = scip_app("scip-stale").await;
        let util = root.join("src/util.rs");
        std::fs::write(&util, format!("// moved\n{SCIP_UTIL}")).unwrap();
        set_mtime(&util, std::time::SystemTime::now());
        app.symbols.invalidate(&["src/util.rs".to_string()]);
        run_tags_job(&mut app);
        goto(&mut app, 2, 26);
        act(&mut app, LspAction::GotoDefinition);
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_buffer().unwrap().path, Some(util));
        assert_eq!(caret(&app), (1, 15));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unsaved_caret_file_does_not_ask_scip() {
        let (mut app, dir, _) = scip_app("scip-dirty").await;
        goto(&mut app, 4, 1);
        app.perform(Action::Editor(EditorAction::InsertText("\n".into())));
        goto(&mut app, 2, 26);
        act(&mut app, LspAction::FindUsages);
        // SCIP would list the two calls; the tags index has no refs for scoped
        // calls like `util::helper(..)`, so today's path finds nothing.
        assert!(app.palette.is_none());
        assert!(toasts(&app).contains(&"No usages found".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_reload_keeps_the_index_and_retries_on_the_next_change() {
        let (mut app, dir, root) = scip_app("scip-reload").await;
        let file = root.join("index.scip");
        let old = app.scip.index.as_ref().unwrap().mtime();
        let secs = |s| std::time::SystemTime::now() - std::time::Duration::from_secs(s);
        std::fs::write(&file, b"half written \xff").unwrap();
        set_mtime(&file, secs(20));
        goto(&mut app, 3, 12);
        act(&mut app, LspAction::FindUsages);
        assert!(app.scip.loading);
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert_eq!(app.scip.index.as_ref().map(|ix| ix.mtime()), Some(old), "the old index stays");
        assert!(toasts(&app).iter().any(|t| t.starts_with("SCIP index index.scip:")), "{:?}", toasts(&app));
        // The same failed mtime: no second load.
        app.palette = None;
        act(&mut app, LspAction::FindUsages);
        assert!(!app.scip.loading);
        // The file changes again and is good: the next lookup retries.
        let main: &[(&[i32], &str, bool)] = &[(&[1, 3, 7], "t main().", true)];
        crate::symbols::scip::write_test_index(&file, "", &[("src/main.rs", scip::Encoding::Utf8, main)]);
        let newer = secs(10);
        set_mtime(&file, newer);
        app.palette = None;
        act(&mut app, LspAction::FindUsages);
        assert!(app.scip.loading);
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert_eq!(app.scip.index.as_ref().map(|ix| ix.mtime()), Some(newer));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_first_load_retries_when_the_file_changes() {
        let (mut app, dir) = test_app("scip-first");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("index.scip"), b"half written \xff").unwrap();
        let mut c = app.config.clone();
        c.index.scip = "index.scip".into();
        app.apply_config(c);
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert!(app.scip.index.is_none());
        app.scip_check_reload();
        assert!(!app.scip.loading, "the failed mtime is not retried");
        let main: &[(&[i32], &str, bool)] = &[(&[1, 3, 7], "t main().", true)];
        crate::symbols::scip::write_test_index(&root.join("index.scip"), "", &[("src/main.rs", scip::Encoding::Utf8, main)]);
        set_mtime(&root.join("index.scip"), std::time::SystemTime::now() - std::time::Duration::from_secs(5));
        app.scip_check_reload();
        assert!(app.scip.loading);
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert!(app.scip.index.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_root_switch_drops_the_old_roots_index() {
        let (mut app, dir, _) = scip_app("scip-switch").await;
        let other = dir.join("other-root");
        std::fs::create_dir_all(other.join("src")).unwrap();
        std::fs::write(other.join("src/main.rs"), SCIP_MAIN).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(app.scip.index.is_none(), "before the new root's load lands");
        assert!(!app.scip_fresh("src/main.rs"));
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert!(app.scip.index.is_none(), "the new root has no index file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_scip_index_warns() {
        let (mut app, dir) = test_app("scip-missing");
        let mut c = app.config.clone();
        c.index.scip = "nope.scip".into();
        app.apply_config(c);
        handle_next(&mut app, |m| matches!(m, AppMsg::Scip { .. })).await;
        assert!(app.scip.index.is_none());
        assert!(toasts(&app).iter().any(|t| t.starts_with("SCIP index nope.scip:")), "{:?}", toasts(&app));
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

    #[tokio::test]
    async fn quick_definition_shows_an_open_buffer_target() {
        let (mut app, dir) = test_app("lsp-quickdef");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        app.perform(Action::Editor(EditorAction::Move(Motion::DocEnd, false)));
        app.perform(Action::Editor(EditorAction::InsertText("fn helper(s: &str) {\n    s.len();\n}\n// after\n".into())));
        app.lsp_sync();
        goto(&mut app, 1, 21);
        app.handle_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::ALT | KeyModifiers::SHIFT));
        let req = answer(&mut app, &mut server, "textDocument/definition", loc(&a, range(3, 3, 3, 9))).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 22}));
        handle_next(&mut app, |m| matches!(m, AppMsg::QuickDefinition { .. })).await;
        assert_eq!(popup(&app), ["Heading:a.rs:4", "Code:fn helper(s: &str) {", "Code:    s.len();", "Code:}"]);
        assert!(app.hover.as_ref().unwrap().lines().iter().any(|l| !l.spans.is_empty()));
        assert_eq!(caret(&app), (1, 21));
        assert_eq!(app.editor.buffers.len(), 1);
        key(&mut app, KeyCode::Esc);
        assert!(app.hover.is_none());
        assert_eq!(caret(&app), (1, 21));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quick_definition_reads_a_closed_file_from_disk() {
        let (mut app, dir) = test_app("lsp-quickdef-disk");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let a = app.workspace.root.join("a.rs");
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        let notes = app.workspace.root.join("notes.txt");
        std::fs::write(&notes, (0..40).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        goto(&mut app, 1, 21);
        act(&mut app, LspAction::QuickDefinition);
        let links = json!([
            {"targetUri": crate::lsp::sync::uri(&b).unwrap().as_str(), "targetRange": range(1, 0, 1, 37), "targetSelectionRange": range(1, 16, 1, 22)},
            {"targetUri": crate::lsp::sync::uri(&a).unwrap().as_str(), "targetRange": range(0, 0, 2, 1), "targetSelectionRange": range(0, 3, 0, 7)},
        ]);
        answer(&mut app, &mut server, "textDocument/definition", links).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::QuickDefinition { .. })).await;
        assert_eq!(popup(&app), ["Heading:b.rs:2", "Code:/* 😀 */ pub fn helper(s: &str) {}"]);
        assert_eq!(app.editor.buffers.len(), 1);
        assert_eq!(app.editor.active_path(), Some(a.as_path()));

        key(&mut app, KeyCode::Esc);
        act(&mut app, LspAction::QuickDefinition);
        answer(&mut app, &mut server, "textDocument/definition", loc(&notes, range(2, 0, 2, 4))).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::QuickDefinition { .. })).await;
        let rows = popup(&app);
        assert_eq!(rows.len(), 16, "{rows:?}");
        assert_eq!((rows[0].as_str(), rows[1].as_str(), rows[15].as_str()), ("Heading:notes.txt:3", "Code:line 2", "Code:line 16"));
        assert!(app.hover.as_ref().unwrap().lines().iter().all(|l| l.spans.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quick_definition_of_an_unreadable_file_says_why() {
        let (mut app, dir) = test_app("lsp-quickdef-unreadable");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let gone = app.workspace.root.join("gone.rs");
        let bad = app.workspace.root.join("bad.rs");
        std::fs::write(&bad, b"fn \xff() {}\n").unwrap();
        goto(&mut app, 1, 21);
        for (target, shown) in [(&gone, "gone.rs"), (&bad, "bad.rs")] {
            app.toasts = Default::default();
            act(&mut app, LspAction::QuickDefinition);
            answer(&mut app, &mut server, "textDocument/definition", loc(target, range(0, 0, 0, 1))).await;
            handle_next(&mut app, |m| matches!(m, AppMsg::QuickDefinition { .. })).await;
            let prefix = format!("Cannot read {shown}: ");
            assert!(toasts(&app).iter().any(|t| t.starts_with(&prefix)), "{:?}", toasts(&app));
            assert!(app.hover.is_none());
            assert_eq!(app.editor.buffers.len(), 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quick_definition_without_a_location_says_so_and_stale_ones_are_dropped() {
        let (mut app, dir) = test_app("lsp-quickdef-stale");
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        let b = app.workspace.root.join("b.rs");
        std::fs::write(&b, B).unwrap();
        goto(&mut app, 1, 21);
        for none in [Value::Null, json!([])] {
            app.toasts = Default::default();
            act(&mut app, LspAction::QuickDefinition);
            answer(&mut app, &mut server, "textDocument/definition", none).await;
            assert!(toasts(&app).contains(&"No definition found".to_string()), "{:?}", toasts(&app));
            assert!(app.hover.is_none());
        }
        act(&mut app, LspAction::QuickDefinition);
        answer(&mut app, &mut server, "textDocument/definition", loc(&b, range(1, 16, 1, 22))).await;
        app.perform(Action::Editor(EditorAction::Move(Motion::Left, false)));
        handle_next(&mut app, |m| matches!(m, AppMsg::QuickDefinition { .. })).await;
        assert!(app.hover.is_none());

        let _ = std::fs::remove_dir_all(&dir);

        let (mut app, dir) = test_app("lsp-quickdef-nocap");
        let mut servers = with_fake_servers(&mut app, incremental());
        let _server = open_on_server(&mut app, &mut servers, "a.rs", A).await;
        app.toasts = Default::default();
        act(&mut app, LspAction::QuickDefinition);
        assert!(toasts(&app).contains(&"rust-analyzer does not support go to definition".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn definition_snippet_cuts_at_the_symbol_end_or_15_lines_capped_at_200() {
        let rust = syntax::language_for_name("rust");
        let text = "// c\nfn a() {\n    1;\n}\nfn b() {}\n";
        assert_eq!(definition_snippet(text, rust, 1), "fn a() {\n    1;\n}");
        assert_eq!(definition_snippet(text, None, 0).lines().count(), 5);
        let long = format!("fn big() {{\n{}}}\n", "    1;\n".repeat(300));
        assert_eq!(definition_snippet(&long, rust, 0).lines().count(), 200);
        assert_eq!(definition_snippet("a\nb\nc\nd\ne\n", None, 99), "");
        assert_eq!(definition_snippet(text, rust, 2), "    1;\n}\nfn b() {}");
    }
}
