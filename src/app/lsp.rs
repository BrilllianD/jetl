//! Language servers in the app (`crate::lsp`): one per configured server,
//! started for the first open buffer of its languages; documents kept in
//! sync by [`App::lsp_sync`], which the run loop calls after each batch of
//! input and messages; `LspMsg` routing (generation and server id checked
//! first); server requests answered; the status bar segment; restart, root
//! switch and quit. Extra servers (`[lsp.servers.<name>] extra`) get the
//! same documents next to the main server's (`LspService::extra_docs`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{
    ConfigurationParams, ProgressParams, ProgressParamsValue, PublishDiagnosticsParams, ShowMessageParams, TextDocumentSyncKind, Uri,
    WorkDoneProgress,
};
use serde_json::{Value, json};

use super::App;
use crate::editor::buffer::{Buffer, BufferId};
use crate::event::Level;
use crate::lsp::caps::{Caps, INLAY_HINT, SEMANTIC_TOKENS};
use crate::lsp::client::{Client, StartSpec, State};
use crate::lsp::diagnostics::{Counts, Diagnostic, DiagnosticStore, MAX_TOTAL, Severity};
use crate::lsp::registry::DocRef;
use crate::lsp::sync::{self, DocEdit, SyncCaps};
use crate::lsp::transport::{RequestId, ResponseError};
use crate::lsp::{LspConfig, LspMsg, LspRequest, ServerConfig, ServerId};
use crate::ui::dialog::{Dialog, DialogId, DialogResult};

/// How servers are reached: processes, or in-process fakes in tests.
#[derive(Default)]
enum Connector {
    #[default]
    Process,
    #[cfg(test)]
    #[cfg_attr(not(feature = "lang-rust"), allow(dead_code))]
    Fake(Box<FakeConnect>),
}

/// Connects to an in-process server, given the server's name.
#[cfg(test)]
type FakeConnect = dyn FnMut(&str) -> std::io::Result<crate::lsp::client::Conn> + Send;

#[derive(Default)]
pub struct LspService {
    /// The current root's servers by name; a failed one stays (in
    /// `State::Failed`) so it is not started again until `lsp.restart`.
    pub(super) clients: HashMap<String, Client>,
    /// Servers whose command could not start; not tried again until
    /// `lsp.restart`.
    missing: HashSet<String>,
    /// The `detachedFiles` each rust-analyzer scratch client was started
    /// with, by client key.
    detached: HashMap<String, Vec<PathBuf>>,
    /// Buffers open on a server.
    pub(super) docs: HashMap<BufferId, Doc>,
    /// Buffers open on extra servers (`[lsp.servers.<name>] extra`), by buffer and client name.
    pub(super) extra_docs: HashMap<(BufferId, String), Doc>,
    /// `[lsp]` servers, resolved on first use.
    servers: Option<Arc<Vec<ServerConfig>>>,
    /// The last `ServerId` handed out.
    last_id: ServerId,
    /// Work done progress, oldest first: server, token, what it reports.
    progress: Vec<(ServerId, String, Progress)>,
    /// What the servers published (`textDocument/publishDiagnostics`).
    pub(crate) diagnostics: DiagnosticStore,
    /// The request in flight per method (`app/lsp_nav.rs`): only its
    /// answer is used, and a newer request of the method cancels it.
    pub(super) inflight: HashMap<&'static str, Inflight>,
    /// The last seq handed to a request (`DocTag::seq`).
    pub(super) last_seq: u64,
    /// `workspace/symbol` requests of the Symbols palette (`app/symbols.rs`).
    pub(super) ws_symbols: super::symbols::WsSymbols,
    /// The seq of the newest definition or usages request: the one whose
    /// locations may still open the palette.
    pub(super) nav_seq: u64,
    /// Whether the newest signature help request came from Ctrl+P: only
    /// then an empty answer or a failure is told to the user.
    pub(super) signature_explicit: bool,
    /// Completion requests and the item being accepted
    /// (`app/completion.rs`).
    pub(super) completion: super::completion::CompletionReqs,
    /// Rename, context actions (`app/lsp_edit.rs`).
    pub(super) edit: super::lsp_edit::EditReqs,
    /// The lightbulb's request and debounce (`app/lightbulb.rs`).
    pub(super) lightbulb: super::lightbulb::Lightbulb,
    /// The document highlight's request and debounce (`app/doc_highlight.rs`).
    pub(super) doc_highlight: super::doc_highlight::DocHighlightReqs,
    /// The inlay hints' request and debounce (`app/inlay.rs`).
    pub(super) inlay: super::inlay::InlayReqs,
    /// Whether inlay hints are asked for and drawn: `[lsp] inlay_hints`,
    /// switched for the session by the View menu.
    pub(super) inlays_shown: bool,
    /// The semantic tokens' request and debounce (`app/semantic.rs`).
    pub(super) semantic: super::semantic::SemanticReqs,
    /// Tree file operations waiting for `will*Files` answers (`app/files.rs`).
    pub(super) file_ops: Vec<super::files::PendingFileOp>,
    /// Offer servers `didChangeWatchedFiles` (the root watcher runs).
    pub(super) watch_files: bool,
    /// The outside watcher failed for this root: servers are not offered
    /// `didChangeWatchedFiles`, so they watch by themselves.
    pub(super) outside_failed: bool,
    /// Folders outside the root that servers' watchers need, with whether
    /// they are watched recursively (`app/lsp_watch.rs`).
    pub(super) outside: Vec<(PathBuf, bool)>,
    pub(super) outside_watcher: Option<crate::files::watch::Watcher>,
    connector: Connector,
}

/// A request in flight: the server, its id there (for `$/cancelRequest`)
/// and its seq.
#[derive(Debug, Clone)]
pub(super) struct Inflight {
    pub server: String,
    pub id: i64,
    pub seq: u64,
}

/// `didChange`s remembered per document (`Doc::sent`).
const SENT_KEEP: usize = 64;

/// A buffer as its server knows it.
#[derive(Debug)]
pub(super) struct Doc {
    pub server: String,
    path: PathBuf,
    language: &'static str,
    pub uri: Uri,
    pub version: i32,
    /// What each `didChange` sent, newest last, for mapping a publish made
    /// for an older version: the version and its edits, `None` when the
    /// whole text went (the edits are unknown). At most `SENT_KEEP`.
    sent: std::collections::VecDeque<(i32, Option<Vec<DocEdit>>)>,
    /// `didOpen` sent and not closed since.
    opened: bool,
    /// `didClose` sent after a `didOpen` unregistration: no changes until it is opened again.
    closed: bool,
    /// The change kind of the last sync; from `NONE` to another the next change sends the whole text.
    kind: TextDocumentSyncKind,
    /// Edits of a pass could not be sent (no capabilities then): the next change sends the whole text.
    resync: bool,
}

/// How `client` wants `doc` synced, once it runs.
fn sync_caps(client: &Client, doc: DocRef<'_>) -> Option<SyncCaps> {
    Some(Caps { fixed: client.capabilities()?, registry: &client.registry, doc: Some(doc) }.sync())
}

impl Doc {
    /// The edits sent after `version`, in order; `None` when they are not
    /// all known (older than `sent` reaches, a whole-text change between,
    /// or a version this document never had).
    fn edits_since(&self, version: i32) -> Option<impl Iterator<Item = &DocEdit>> {
        if version > self.version {
            return None;
        }
        let later = self.sent.iter().filter(move |(v, _)| *v > version);
        if version < self.version && self.sent.front().is_none_or(|(v, _)| *v > version + 1) {
            return None;
        }
        if later.clone().any(|(_, e)| e.is_none()) {
            return None;
        }
        Some(later.flat_map(|(_, e)| e.iter().flatten()))
    }

    /// The document as a registration's document selector sees it.
    pub(super) fn doc_ref(&self) -> DocRef<'_> {
        DocRef { language_id: sync::language_id(self.language), path: &self.path }
    }
}

#[derive(Debug, Clone, Default)]
struct Progress {
    title: String,
    message: Option<String>,
    percentage: Option<u32>,
}

/// The status bar segment for the active buffer's server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspStatus {
    Idle(String),
    /// Starting or reporting progress; drawn with the spinner.
    Busy(String),
    /// Off, missing or stopped.
    Down(String),
}

impl LspService {
    /// Stop every server and wait (at most `SHUTDOWN_TIMEOUT`) for them to
    /// exit (quit).
    pub(crate) async fn shutdown_all(&mut self) {
        let stops: Vec<_> = self.clients.drain().map(|(_, c)| c.shutdown()).collect();
        futures::future::join_all(stops).await;
    }

    /// Stop every server in the background and forget the documents (root
    /// switch, restart).
    fn reset(&mut self) {
        for (_, c) in self.clients.drain() {
            tokio::spawn(c.shutdown());
        }
        self.docs.clear();
        self.extra_docs.clear();
        self.detached.clear();
        self.progress.clear();
        self.diagnostics.clear();
        self.inflight.clear();
        self.ws_symbols = Default::default();
        self.signature_explicit = false;
        self.completion.request = None;
        self.completion.accepting = None;
        self.completion.docs = None;
        // The rename dialog's option is a session choice, not server state.
        let rename_in_text = self.edit.rename_in_text;
        self.edit = Default::default();
        self.edit.rename_in_text = rename_in_text;
        self.lightbulb = Default::default();
        self.doc_highlight = Default::default();
        self.inlay = Default::default();
        self.semantic = Default::default();
        self.file_ops.clear();
        self.outside.clear();
        self.outside_watcher = None;
    }

    /// A service with nothing running and the inlay hints switched `shown`.
    pub(super) fn with_inlays(shown: bool) -> Self {
        Self { inlays_shown: shown, ..Default::default() }
    }

    /// Whether a spinner is shown (a server starting or busy).
    pub(crate) fn busy(&self) -> bool {
        !self.progress.is_empty() || self.clients.values().any(|c| matches!(c.state, State::Starting))
    }

    fn name_of(&self, server: ServerId) -> Option<String> {
        self.clients.values().find(|c| c.id == server).map(|c| c.name.clone())
    }

    /// Start `server` as client `key` (its name, or its scratch client's)
    /// rooted at `root`. A command that cannot start is remembered as
    /// missing and reported once.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        server: &ServerConfig,
        cfg: &LspConfig,
        root: &Path,
        generation: u64,
        bus: &tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
        key: String,
        init_options: Option<Value>,
    ) -> Option<(Level, String)> {
        let conn = match &mut self.connector {
            Connector::Process => crate::lsp::client::spawn_process(&key, &server.command, root),
            #[cfg(test)]
            Connector::Fake(connect) => connect(&key),
        };
        let name = &server.name;
        let conn = match conn {
            Ok(conn) => conn,
            Err(e) => {
                self.missing.insert(name.clone());
                return Some(if e.kind() == std::io::ErrorKind::NotFound {
                    let program = &server.command[0];
                    let which = if program == name { String::new() } else { format!(" ({program})") };
                    (
                        Level::Info,
                        format!("Language server {name} not found{which}: install it, or set its command in [lsp.servers.{name}]"),
                    )
                } else {
                    (Level::Warn, format!("Language server {name} could not start: {e}"))
                });
            }
        };
        self.last_id += 1;
        tracing::info!("starting language server {name} ({:?}) in {}", server.command, root.display());
        let spec = StartSpec {
            id: self.last_id,
            name: key.clone(),
            root: root.to_path_buf(),
            generation,
            init_options,
            startup_timeout: std::time::Duration::from_millis(cfg.startup_timeout_ms),
            request_timeout: std::time::Duration::from_millis(cfg.request_timeout_ms),
            watch_files: self.watch_files && !self.outside_failed,
        };
        self.clients.insert(key, Client::start(spec, conn, bus.clone()));
        None
    }

    /// Close buffer `id`'s documents: on its main server and on the extra ones.
    fn close_doc(&mut self, id: BufferId) {
        if let Some(doc) = self.docs.remove(&id) {
            close_on(&self.clients, &doc);
        }
        let keys: Vec<_> = self.extra_docs.keys().filter(|(b, _)| *b == id).cloned().collect();
        for key in keys {
            if let Some(doc) = self.extra_docs.remove(&key) {
                close_on(&self.clients, &doc);
            }
        }
    }

    /// Whether client `client` is an extra server's.
    pub(super) fn is_extra(&self, client: &str) -> bool {
        let name = crate::lsp::server_name(client);
        self.servers.as_ref().is_some_and(|s| s.iter().any(|s| s.extra && s.name == name))
    }

    /// Whether buffer `id` is open on any server.
    pub(super) fn has_doc(&self, id: BufferId) -> bool {
        self.docs.contains_key(&id) || self.extra_docs.keys().any(|(b, _)| *b == id)
    }

    /// Stop client `key` and forget its documents (the caller also calls
    /// `App::format_server_gone`).
    fn stop_client(&mut self, key: &str, buffers: &mut [Buffer]) {
        let Some(client) = self.clients.remove(key) else { return };
        let id = client.id;
        tokio::spawn(client.shutdown());
        self.server_gone(key, id, buffers);
        self.detached.remove(key);
    }

    /// Forget the documents and progress of a server that went away.
    fn server_gone(&mut self, name: &str, server: ServerId, buffers: &mut [Buffer]) {
        self.docs.retain(|_, d| d.server != name);
        self.extra_docs.retain(|(_, n), _| n != name);
        self.progress.retain(|p| p.0 != server);
        self.diagnostics.clear_server(name);
        for b in buffers.iter_mut() {
            if !self.has_doc(b.id) {
                untrack(b);
            } else if !self.docs.contains_key(&b.id) {
                drop_server_state(b);
            }
        }
    }
}

/// The server that gets `detachedFiles` for its scratch client.
const RUST_ANALYZER: &str = "rust-analyzer";

/// The scratch files `server` serves: those in `dir` whose language is one
/// of its languages, and the open ones given, sorted and deduplicated. A
/// small directory read on the main thread, once per scratch client start.
fn scratch_files<'a>(dir: &Path, server: &ServerConfig, open: impl Iterator<Item = &'a Path>) -> Vec<PathBuf> {
    let on_disk = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| crate::editor::syntax::detect(p).is_some_and(|def| server.languages.iter().any(|l| l == def.name)));
    let mut files: Vec<PathBuf> = on_disk.chain(open.map(Path::to_path_buf)).collect();
    files.sort();
    files.dedup();
    files
}

/// `initializationOptions` of a scratch client: rust-analyzer's own with
/// `detachedFiles` set to `files`; none for other servers.
fn scratch_init_options(server: &ServerConfig, files: &[PathBuf]) -> Option<Value> {
    if server.name != RUST_ANALYZER {
        return None;
    }
    let mut options = server.init_options.clone().filter(Value::is_object).unwrap_or_else(|| json!({}));
    options["detachedFiles"] = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    Some(options)
}

/// Stop recording edits for the language server.
fn untrack(b: &mut Buffer) {
    b.lsp_tracked = false;
    b.lsp_pending.clear();
    b.lsp_full_resync = false;
    drop_server_state(b);
}

/// Forget what only the main server put on a buffer.
fn drop_server_state(b: &mut Buffer) {
    b.inlays.clear();
    b.semantic.clear();
}

/// `didClose` of `doc` on its client, when it is open there and the client wants it.
fn close_on(clients: &HashMap<String, Client>, doc: &Doc) {
    if doc.opened
        && let Some(c) = clients.get(&doc.server)
        && sync_caps(c, doc.doc_ref()).is_some_and(|caps| caps.close)
    {
        c.notify("textDocument/didClose", json!({"textDocument": {"uri": doc.uri}}));
    }
}

/// `didOpen` of `buf` on `client` (as client `key`); `None` while the client has no capabilities.
fn open_doc(client: &Client, buf: &Buffer, language: &'static str, path: &Path, key: String) -> Option<Doc> {
    let doc_ref = DocRef { language_id: sync::language_id(language), path };
    let (Some(caps), Some(uri)) = (sync_caps(client, doc_ref), sync::uri(path)) else { return None };
    if caps.open {
        let item = json!({"uri": uri, "languageId": sync::language_id(language), "version": 1, "text": buf.text()});
        client.notify("textDocument/didOpen", json!({"textDocument": item}));
    }
    Some(Doc {
        server: key,
        path: path.to_path_buf(),
        language,
        uri,
        version: 1,
        sent: Default::default(),
        opened: caps.open,
        closed: false,
        kind: caps.change,
        resync: false,
    })
}

/// Bring `doc` up to date with `buf`: `didOpen` when it is due, else `didChange` with the `edits`
/// recorded since the last pass (the whole text when `full`).
fn sync_doc(client: &Client, doc: &mut Doc, buf: &Buffer, edits: &[DocEdit], full: bool) {
    let Some(caps) = sync_caps(client, doc.doc_ref()) else {
        doc.resync |= full || !edits.is_empty();
        return;
    };
    if caps.open && !doc.opened {
        doc.version += 1;
        let item = json!({"uri": doc.uri, "languageId": sync::language_id(doc.language), "version": doc.version, "text": buf.text()});
        client.notify("textDocument/didOpen", json!({"textDocument": item}));
        doc.sent.push_back((doc.version, None));
        if doc.sent.len() > SENT_KEEP {
            doc.sent.pop_front();
        }
        doc.opened = true;
        doc.closed = false;
        doc.kind = caps.change;
        doc.resync = false;
        return;
    }
    if !caps.open && doc.opened {
        if caps.close {
            client.notify("textDocument/didClose", json!({"textDocument": {"uri": doc.uri}}));
        }
        doc.opened = false;
        doc.closed = caps.close;
    }
    if doc.closed {
        doc.kind = caps.change;
        return;
    }
    let switched = doc.kind == TextDocumentSyncKind::NONE && caps.change != TextDocumentSyncKind::NONE;
    let full = full || doc.resync || switched;
    if edits.is_empty() && !full {
        doc.kind = caps.change;
        return;
    }
    doc.version += 1;
    let sent = (!full).then(|| edits.to_vec());
    doc.sent.push_back((doc.version, sent));
    if doc.sent.len() > SENT_KEEP {
        doc.sent.pop_front();
    }
    let changes = match caps.change {
        TextDocumentSyncKind::INCREMENTAL if !full => edits.iter().map(|e| e.change_event()).collect(),
        TextDocumentSyncKind::NONE => Vec::new(),
        _ => vec![lsp_types::TextDocumentContentChangeEvent { range: None, range_length: None, text: buf.text() }],
    };
    if !changes.is_empty() {
        let params = json!({"textDocument": {"uri": doc.uri, "version": doc.version}, "contentChanges": changes});
        client.notify("textDocument/didChange", params);
    }
    doc.kind = caps.change;
    doc.resync = false;
}

impl App {
    /// Bring the servers in line with the open buffers: start the server of
    /// a buffer's language, `didOpen` buffers once it runs, `didChange` with
    /// the edits recorded since the last call (version + 1), `didClose` for
    /// closed buffers, close and open again when a path, language or client
    /// changed. Scratch files open on their own clients, rooted at the
    /// scratches directory, when `[lsp] scratches` is on; else they open on
    /// none.
    pub(crate) fn lsp_sync(&mut self) {
        if !self.config.lsp.enabled {
            return;
        }
        let mut notes = Vec::new();
        if self.lsp.servers.is_none() {
            for name in self.config.lsp.misplaced_wildcards() {
                let text = format!("Language server {name} is ignored: languages = [\"*\"] needs extra = true in [lsp.servers.{name}]");
                notes.push((Level::Warn, text));
            }
        }
        let servers = Arc::clone(self.lsp.servers.get_or_insert_with(|| Arc::new(self.config.lsp.servers())));
        let gone: HashSet<BufferId> = self
            .lsp
            .docs
            .keys()
            .chain(self.lsp.extra_docs.keys().map(|(id, _)| id))
            .filter(|id| self.editor.index_of_id(**id).is_none())
            .copied()
            .collect();
        for id in gone {
            self.lsp.close_doc(id);
        }
        let scratches = self.paths.scratches_dir();
        let scratch_on = self.config.lsp.scratches;
        // Open scratch buffers and their languages, for `detachedFiles`.
        let open_scratches: Vec<(PathBuf, &'static str)> = if scratch_on {
            let under = |p: &PathBuf| p.starts_with(&scratches);
            self.editor.buffers.iter().filter_map(|b| Some((b.path.clone().filter(under)?, b.language()?.name))).collect()
        } else {
            Vec::new()
        };
        let mut restart: Vec<String> = Vec::new();
        let lsp = &mut self.lsp;
        for buf in &mut self.editor.buffers {
            let language = buf.language().map(|l| l.name);
            let is_scratch = buf.path.as_ref().is_some_and(|p| p.starts_with(&scratches));
            let server = language.and_then(|language| crate::lsp::main_server(&servers, language));
            let key = server.map(|server| if is_scratch { crate::lsp::scratch_client(&server.name) } else { server.name.clone() });
            // Extra servers never take scratch files.
            let extras: Vec<&ServerConfig> = match (language, &buf.path) {
                (Some(language), Some(_)) if !is_scratch => crate::lsp::extra_servers(&servers, language).collect(),
                _ => Vec::new(),
            };
            if lsp.docs.get(&buf.id).is_some_and(|d| {
                buf.path.as_ref() != Some(&d.path) || language != Some(d.language) || key.as_deref() != Some(d.server.as_str())
            }) && let Some(doc) = lsp.docs.remove(&buf.id)
            {
                close_on(&lsp.clients, &doc);
                drop_server_state(buf);
            }
            let stale: Vec<(BufferId, String)> = lsp
                .extra_docs
                .iter()
                .filter(|((id, n), d)| {
                    *id == buf.id
                        && (buf.path.as_ref() != Some(&d.path) || language != Some(d.language) || !extras.iter().any(|s| &s.name == n))
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in stale {
                if let Some(doc) = lsp.extra_docs.remove(&k) {
                    close_on(&lsp.clients, &doc);
                }
            }
            if !lsp.has_doc(buf.id) {
                untrack(buf);
            }
            let (Some(language), Some(path)) = (language, buf.path.clone()) else { continue };
            // Taken once for every document of the buffer.
            let edits = std::mem::take(&mut buf.lsp_pending);
            let full = std::mem::replace(&mut buf.lsp_full_resync, false);
            // The store follows the text as the servers now know it: once, since it moves every server's list.
            if !full && !edits.is_empty() && lsp.has_doc(buf.id) {
                lsp.diagnostics.shift(&path, &edits);
            }
            for s in &extras {
                if let Some(doc) = lsp.extra_docs.get_mut(&(buf.id, s.name.clone())) {
                    match lsp.clients.get(&s.name) {
                        Some(client) => sync_doc(client, doc, buf, &edits, full),
                        None => doc.resync = true,
                    }
                } else if !lsp.clients.contains_key(&s.name) {
                    if !lsp.missing.contains(&s.name) {
                        let (root, generation) = (&self.workspace.root, self.workspace.generation);
                        notes.extend(lsp.start(
                            s,
                            &self.config.lsp,
                            root,
                            generation,
                            &self.bus_tx,
                            s.name.clone(),
                            s.init_options.clone(),
                        ));
                    }
                } else if buf.rope.len_bytes() <= self.config.editor.max_highlight_bytes
                    && let Some(doc) = open_doc(&lsp.clients[&s.name], buf, language, &path, s.name.clone())
                {
                    lsp.extra_docs.insert((buf.id, s.name.clone()), doc);
                    buf.lsp_tracked = true;
                }
            }
            let (Some(server), Some(key)) = (server, key) else { continue };
            // Scratch files belong to no project: no server for them unless
            // `[lsp] scratches` is on (one saved into the directory was
            // closed above).
            if is_scratch && !scratch_on {
                continue;
            }
            if let Some(doc) = lsp.docs.get_mut(&buf.id) {
                match lsp.clients.get(&doc.server) {
                    Some(client) => sync_doc(client, doc, buf, &edits, full),
                    None => doc.resync = true,
                }
                continue;
            }
            if !lsp.clients.contains_key(&key) {
                if !lsp.missing.contains(&server.name) {
                    let (root, init_options) = if is_scratch {
                        let open = open_scratches.iter().filter(|(_, l)| server.languages.iter().any(|s| s == l)).map(|(p, _)| p.as_path());
                        let files = scratch_files(&scratches, server, open);
                        let options = scratch_init_options(server, &files);
                        lsp.detached.insert(key.clone(), files);
                        (scratches.as_path(), options)
                    } else {
                        (self.workspace.root.as_path(), server.init_options.clone())
                    };
                    notes.extend(lsp.start(server, &self.config.lsp, root, self.workspace.generation, &self.bus_tx, key, init_options));
                }
                continue;
            }
            // rust-analyzer reads `detachedFiles` once: a scratch it was not
            // started with needs a new start.
            if is_scratch && server.name == RUST_ANALYZER && lsp.detached.get(&key).is_some_and(|files| !files.contains(&path)) {
                if !restart.contains(&key) {
                    restart.push(key);
                }
                continue;
            }
            if let Some(doc) = open_doc(&lsp.clients[&key], buf, language, &path, key) {
                lsp.docs.insert(buf.id, doc);
                drop_server_state(buf);
                buf.lsp_tracked = true;
            }
        }
        for key in restart {
            self.lsp.stop_client(&key, &mut self.editor.buffers);
            self.command_server_gone(&key);
            self.format_server_gone(&key);
        }
        for (level, text) in notes {
            self.notify(level, text);
        }
    }

    /// `Effect::FileSaved`: `didSave` after the edits that led to it.
    pub(super) fn lsp_saved(&mut self, path: &Path) {
        self.lsp_sync();
        // A document closed by a `didOpen` unregistration gets no `didSave` until it is opened again.
        let mut saves = Vec::new();
        for doc in self.lsp.docs.values().chain(self.lsp.extra_docs.values()).filter(|d| d.path == path && !d.closed) {
            let Some(client) = self.lsp.clients.get(&doc.server) else { continue };
            if let Some(include_text) = sync_caps(client, doc.doc_ref()).and_then(|c| c.save) {
                saves.push((client, &doc.uri, include_text));
            }
        }
        let text = saves.iter().any(|s| s.2).then(|| self.editor.index_of(path).map(|i| self.editor.buffers[i].text())).flatten();
        for (client, uri, include_text) in saves {
            let mut params = json!({"textDocument": {"uri": uri}});
            if include_text && let Some(text) = &text {
                params["text"] = text.as_str().into();
            }
            client.notify("textDocument/didSave", params);
        }
    }

    pub(super) fn lsp_request(&mut self, req: LspRequest) {
        match req {
            LspRequest::Restart => self.lsp_restart(),
            LspRequest::Definition => self.lsp_goto_definition(),
        }
    }

    /// `lsp.restart`: stop every server, forget which were missing, and
    /// start them again for the open buffers.
    pub(super) fn lsp_restart(&mut self) {
        // The servers are going away; the user's operations still happen.
        let writing = self.file_ops_for_restart();
        if let Some(s) = self.hierarchy.as_ref().map(|t| t.server.clone()) {
            self.hierarchy_server_gone(&s);
        }
        self.lsp.reset();
        self.lsp.file_ops = writing;
        self.lsp_retarget_watch();
        self.lsp.missing.clear();
        self.lsp.servers = None;
        self.editor.buffers.iter_mut().for_each(untrack);
        if !self.config.lsp.enabled {
            return self.notify(Level::Info, "Language servers are off ([lsp] enabled = false)");
        }
        self.notify(Level::Info, "Restarting language servers");
        self.lsp_sync();
    }

    /// The root changed: the old root's servers stop; the new root's start
    /// for the open buffers on the next sync.
    pub(crate) fn lsp_root_changed(&mut self) {
        for p in std::mem::take(&mut self.lsp.file_ops) {
            let what = p.op.describe(&self.workspace.root);
            self.notify(Level::Warn, format!("Cannot {what}: the project root changed"));
        }
        self.lsp.reset();
        self.hierarchy = None;
        self.lsp.outside_failed = false;
        self.lsp_retarget_watch();
        self.editor.buffers.iter_mut().for_each(untrack);
    }

    pub(super) fn lsp_msg(&mut self, msg: LspMsg) {
        let server = msg.server();
        let name = match self.lsp.name_of(server) {
            Some(name) if msg.generation() == self.workspace.generation => name,
            _ => return tracing::debug!("dropping a stale language server message: {msg:?}"),
        };
        match msg {
            LspMsg::Started { caps, .. } => {
                tracing::info!("{name} started");
                if let Some(c) = self.lsp.clients.get_mut(&name) {
                    c.state = State::Running(caps);
                }
                self.lsp_sync();
                self.format_server_started(&name);
            }
            LspMsg::Failed { error, .. } => {
                self.lsp_server_down(&name, server, error.clone());
                self.notify(Level::Warn, format!("Language server {name} failed to start: {error}"));
            }
            LspMsg::Exited { status, .. } => {
                self.lsp_server_down(&name, server, status.clone());
                self.notify(
                    Level::Warn,
                    format!("Language server {name} exited ({status}); Help > Restart Language Servers starts it again"),
                );
            }
            LspMsg::Notification { method, params, .. } => self.lsp_notification(&name, server, &method, params),
            LspMsg::ServerRequest { id, method, params, .. } => self.lsp_server_request(&name, server, id, &method, params),
            LspMsg::Response { tag, result, .. } => self.lsp_response(&name, tag, result),
        }
    }

    fn lsp_server_down(&mut self, name: &str, server: ServerId, why: String) {
        if let Some(c) = self.lsp.clients.get_mut(name) {
            c.state = State::Failed(why);
            c.registry.clear();
        }
        self.lsp_retarget_watch();
        self.lsp.server_gone(name, server, &mut self.editor.buffers);
        self.command_server_gone(name);
        self.format_server_gone(name);
        self.file_ops_server_gone(name);
        self.hierarchy_server_gone(name);
    }

    fn lsp_notification(&mut self, name: &str, server: ServerId, method: &str, params: Value) {
        match method {
            "window/showMessage" => {
                let Ok(p) = serde_json::from_value::<ShowMessageParams>(params) else { return };
                let level = match p.typ {
                    lsp_types::MessageType::ERROR => Level::Error,
                    lsp_types::MessageType::WARNING => Level::Warn,
                    _ => Level::Info,
                };
                self.notify(level, format!("{name}: {}", p.message));
            }
            "window/logMessage" => tracing::debug!("{name}: {}", params["message"].as_str().unwrap_or_default()),
            "$/progress" => {
                let Ok(ProgressParams { token, value: ProgressParamsValue::WorkDone(work) }) = serde_json::from_value(params) else {
                    return;
                };
                let token = serde_json::to_string(&token).unwrap_or_default();
                let at = self.lsp.progress.iter().position(|p| p.0 == server && p.1 == token);
                match (work, at) {
                    (WorkDoneProgress::Begin(b), _) => {
                        let p = Progress { title: b.title, message: b.message, percentage: b.percentage };
                        self.lsp.progress.retain(|p| !(p.0 == server && p.1 == token));
                        self.lsp.progress.push((server, token, p));
                    }
                    (WorkDoneProgress::Report(r), Some(i)) => {
                        let p = &mut self.lsp.progress[i].2;
                        p.message = r.message.or(p.message.take());
                        p.percentage = r.percentage.or(p.percentage);
                    }
                    (WorkDoneProgress::End(_), Some(i)) => {
                        self.lsp.progress.remove(i);
                        // Hints and semantic tokens asked while the server loaded were empty or failed.
                        if !self.lsp.progress.iter().any(|p| p.0 == server) {
                            self.inlay_refresh();
                            self.semantic_refresh();
                        }
                    }
                    _ => {}
                }
            }
            "textDocument/publishDiagnostics" => {
                let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params) else { return };
                let Some(path) = sync::path(&p.uri) else { return };
                let mut items: Vec<Diagnostic> = p.diagnostics.into_iter().map(Diagnostic::from_lsp).collect();
                // Made for an older text: moved through what was sent since.
                if let Some(doc) = self.lsp.docs.values().chain(self.lsp.extra_docs.values()).find(|d| d.path == path && d.server == name) {
                    let v = p.version.unwrap_or(doc.version);
                    match doc.edits_since(v) {
                        Some(edits) => {
                            crate::lsp::diagnostics::shift_all(&mut items, &path, edits);
                        }
                        None => tracing::debug!("{name}: diagnostics for {path:?} version {v} cannot be mapped to {}", doc.version),
                    }
                }
                if self.lsp.diagnostics.publish(name, path, items, self.config.lsp.max_diagnostics_per_file) {
                    self.notify(
                        Level::Warn,
                        format!("Language servers reported over {MAX_TOTAL} problems; the rest are not shown (the most severe are kept)"),
                    );
                }
            }
            _ => tracing::debug!("{name}: unhandled notification {method}"),
        }
    }

    fn lsp_server_request(&mut self, name: &str, server: ServerId, id: RequestId, method: &str, params: Value) {
        let result = match method {
            // Answered once applied (`App::edits_applied`).
            "workspace/applyEdit" => match serde_json::from_value::<lsp_types::ApplyWorkspaceEditParams>(params.clone()).map(|mut p| {
                if let Some(raw) = params.get("edit") {
                    crate::lsp::edits::prune_annotations(raw, &mut p.edit);
                }
                p
            }) {
                Ok(p) => {
                    let origin = crate::lsp::EditOrigin::ServerRequest { server: name.to_string(), id, label: p.label };
                    self.server_apply_edit(name, p.edit, origin);
                    return;
                }
                Err(e) => Err(ResponseError::new(ResponseError::INVALID_PARAMS, format!("invalid params: {e}"))),
            },
            // The server asks the client to start a rename after an extraction. jetl does not prompt,
            // and an error here would fail the command that already succeeded.
            "_typescript.rename" => Ok(Value::Null),
            "workspace/configuration" => {
                let settings = self
                    .lsp
                    .servers
                    .as_ref()
                    .and_then(|s| s.iter().find(|s| s.name == crate::lsp::server_name(name)))
                    .and_then(|s| s.settings.clone());
                let items = serde_json::from_value::<ConfigurationParams>(params).map(|p| p.items).unwrap_or_default();
                Ok(items.iter().map(|item| section(settings.as_ref(), item.section.as_deref())).collect())
            }
            "client/registerCapability" | "client/unregisterCapability" => {
                let Some(c) = self.lsp.clients.get_mut(name) else { return };
                tracing::debug!("{name}: {method} {params}");
                let done =
                    if method == "client/registerCapability" { c.registry.register(&params) } else { c.registry.unregister(&params) };
                let done = done
                    .map(|()| Value::Null)
                    .map_err(|e| ResponseError::new(ResponseError::INVALID_PARAMS, format!("invalid params: {e}")));
                if done.is_ok() {
                    self.lsp_retarget_watch();
                    // A feature registered after the tick looked: forget what was decided so the next ticks ask.
                    if method == "client/registerCapability" {
                        let registers = |m: &str| {
                            params
                                .get("registrations")
                                .and_then(Value::as_array)
                                .is_some_and(|a| a.iter().any(|r| r.get("method").and_then(Value::as_str) == Some(m)))
                        };
                        if registers(SEMANTIC_TOKENS) {
                            self.semantic_refresh();
                        }
                        if registers(INLAY_HINT) {
                            self.inlay_refresh();
                        }
                    }
                }
                done
            }
            "window/workDoneProgress/create" => Ok(Value::Null),
            "workspace/inlayHint/refresh" => {
                self.inlay_refresh();
                Ok(Value::Null)
            }
            "workspace/semanticTokens/refresh" => {
                self.semantic_refresh();
                Ok(Value::Null)
            }
            "workspace/workspaceFolders" => {
                let scratches = self.paths.scratches_dir();
                let root = if name.ends_with(crate::lsp::SCRATCH_SUFFIX) { &scratches } else { &self.workspace.root };
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                Ok(sync::uri(root).map_or(Value::Null, |uri| json!([{"uri": uri, "name": name}])))
            }
            "window/showMessageRequest" => {
                if let Ok(p) = serde_json::from_value::<lsp_types::ShowMessageRequestParams>(params) {
                    let actions = p.actions.filter(|a| !a.is_empty());
                    if let Some(actions) = actions.filter(|_| self.dialog.is_none()) {
                        let titles = actions.iter().map(|a| a.title.clone()).collect();
                        let json = actions.iter().filter_map(|a| serde_json::to_string(a).ok()).collect();
                        let did = DialogId::LspMessageRequest { server, name: name.into(), id, actions: json };
                        self.dialog = Some(Dialog::choice(did, name, &p.message, titles));
                        self.dirty_frame = true;
                        return;
                    }
                    let level = match p.typ {
                        lsp_types::MessageType::ERROR => Level::Error,
                        lsp_types::MessageType::WARNING => Level::Warn,
                        _ => Level::Info,
                    };
                    self.notify(level, format!("{name}: {}", p.message));
                }
                Ok(Value::Null)
            }
            _ => {
                tracing::debug!("{name}: unhandled request {method}");
                Err(ResponseError::new(ResponseError::METHOD_NOT_FOUND, format!("unhandled method {method}")))
            }
        };
        if let Some(c) = self.lsp.clients.get(name) {
            c.respond(id, result);
        }
    }

    /// The answer to a `window/showMessageRequest` dialog: the picked
    /// action, else null. A server restarted meanwhile gets nothing.
    pub(super) fn finish_lsp_message(&mut self, server: ServerId, name: &str, id: RequestId, actions: &[String], res: &DialogResult) {
        let picked = match res {
            DialogResult::Choice(i) => actions.get(*i).and_then(|a| serde_json::from_str::<Value>(a).ok()),
            _ => None,
        };
        if let Some(c) = self.lsp.clients.get(name).filter(|c| c.id == server) {
            c.respond(id, Ok(picked.unwrap_or(Value::Null)));
        }
    }

    /// Bring every buffer's diagnostic marks up to date with the store
    /// (cheap when nothing changed; `ui::render` calls it each frame).
    pub(crate) fn sync_diag_marks(&mut self) {
        let store = &self.lsp.diagnostics;
        for b in &mut self.editor.buffers {
            b.diag_marks.sync(store, &b.rope, b.version, b.path.as_deref());
        }
    }

    /// `lsp.next_problem` / `lsp.prev_problem` (F2, Shift+F2): the caret to
    /// the next or previous diagnostic in the file, wrapping around.
    pub(super) fn lsp_step_problem(&mut self, forward: bool) {
        self.sync_diag_marks();
        let Some(b) = self.editor.active_buffer_mut() else { return };
        match b.diag_marks.step(b.view.sel().head, forward) {
            Some(pos) => b.goto_pos(pos),
            None => self.notify(Level::Info, "No problems in this file"),
        }
    }

    /// Error and warning counts of the active file for the status bar:
    /// shown while the file is open on a server or has diagnostics.
    pub fn problem_counts(&self) -> Option<Counts> {
        let b = self.editor.active_buffer()?;
        let path = b.path.as_deref()?;
        let counts = self.lsp.diagnostics.counts(path);
        (self.lsp.has_doc(b.id) || counts != Counts::default()).then_some(counts)
    }

    /// The most severe diagnostic at the active buffer's caret, for the
    /// status bar.
    pub fn caret_problem(&self) -> Option<(Severity, &str)> {
        let b = self.editor.active_buffer()?;
        b.diag_marks.at(b.view.sel().head).map(|m| (m.severity, m.message.as_str()))
    }

    /// Show and focus the Problems window on the active file's problems
    /// (a click on the status bar counts).
    pub(super) fn show_problems(&mut self) {
        self.layout.show(crate::ui::focus::ToolWindowId::Problems);
        self.focus.set(crate::ui::focus::PanelId::Problems);
        if let Some(path) = self.editor.active_path() {
            self.problems.select_file(path, &self.lsp.diagnostics, Some(path));
        }
    }

    /// The name of the client that serves buffer `b` (open on it, or the
    /// one its language would start).
    pub(super) fn buffer_server(&self, b: &Buffer) -> Option<String> {
        let language = b.language()?.name;
        Some(match self.lsp.docs.get(&b.id) {
            Some(doc) => doc.server.clone(),
            None => {
                let server = match &self.lsp.servers {
                    Some(servers) => crate::lsp::main_server(servers, language)?.name.clone(),
                    None => self.config.lsp.server_for(language)?.name,
                };
                match b.path.as_ref() {
                    Some(p) if self.config.lsp.scratches && p.starts_with(self.paths.scratches_dir()) => {
                        crate::lsp::scratch_client(&server)
                    }
                    _ => server,
                }
            }
        })
    }

    /// The first extra server of buffer `b`'s language (never for scratch files).
    fn buffer_extra(&self, b: &Buffer) -> Option<String> {
        let language = b.language()?.name;
        if b.path.as_ref().is_some_and(|p| p.starts_with(self.paths.scratches_dir())) {
            return None;
        }
        let owned;
        let servers: &[ServerConfig] = match &self.lsp.servers {
            Some(servers) => servers,
            None => {
                owned = self.config.lsp.servers();
                &owned
            }
        };
        crate::lsp::extra_servers(servers, language).next().map(|s| s.name.clone())
    }

    /// The status bar segment for the active buffer: its server's name,
    /// with what it is busy with, or why it is not there.
    pub fn lsp_status(&self) -> Option<LspStatus> {
        let b = self.editor.active_buffer()?;
        b.path.as_ref()?;
        b.language()?;
        let name = self.buffer_server(b).or_else(|| self.buffer_extra(b))?;
        if !self.config.lsp.enabled {
            return Some(LspStatus::Down("LSP: off".into()));
        }
        if self.lsp.missing.contains(crate::lsp::server_name(&name)) {
            return Some(LspStatus::Down(format!("LSP: {name} missing")));
        }
        let client = self.lsp.clients.get(&name)?;
        Some(match &client.state {
            State::Starting => LspStatus::Busy(format!("{name}: starting")),
            State::Failed(_) => LspStatus::Down(format!("LSP: {name} stopped")),
            State::Running(_) => match self.lsp.progress.iter().rev().find(|p| p.0 == client.id) {
                Some((_, _, p)) => {
                    let mut s = format!("{name}: {}", p.title);
                    if let Some(m) = p.message.as_deref().filter(|m| !m.is_empty()) {
                        s = format!("{s} {m}");
                    }
                    if let Some(pct) = p.percentage {
                        s = format!("{s} {pct}%");
                    }
                    LspStatus::Busy(s)
                }
                None => LspStatus::Idle(name),
            },
        })
    }
}

/// `section` (dotted) of a server's `settings`; null when absent.
fn section(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(mut v) = settings else { return Value::Null };
    for key in section.into_iter().flat_map(|s| s.split('.')).filter(|k| !k.is_empty()) {
        match v.get(key) {
            Some(next) => v = next,
            None => return Value::Null,
        }
    }
    v.clone()
}

// Every test here opens a Rust file (a language server needs a language).
#[cfg(all(test, feature = "lang-rust"))]
pub(crate) mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, LspAction, Motion};
    use crate::app::tests::{handle_next, load, test_app};
    use crate::editor::buffer::LineEnding;
    use crate::event::AppMsg;
    use crate::lsp::fake::{self, FakeServer};
    use crate::lsp::sync::tests::apply_changes;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Turn the language servers of a test app on, served in-process: each
    /// server the app starts answers `initialize` with `caps` and arrives on
    /// the returned channel.
    pub(crate) fn with_fake_servers(app: &mut App, caps: Value) -> UnboundedReceiver<FakeServer> {
        app.config.lsp.enabled = true;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.lsp.connector = Connector::Fake(Box::new(move |_| {
            let (conn, server) = fake::connect(caps.clone());
            let _ = tx.send(server);
            Ok(conn)
        }));
        rx
    }

    /// Like [`with_fake_servers`], with each server arriving as `(client key, server)`; a key in
    /// `missing` fails to start as if its command were not installed.
    pub(crate) fn with_named_fake_servers(
        app: &mut App,
        caps: Value,
        missing: &'static [&'static str],
    ) -> UnboundedReceiver<(String, FakeServer)> {
        app.config.lsp.enabled = true;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.lsp.connector = Connector::Fake(Box::new(move |key| {
            if missing.contains(&key) {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            let (conn, server) = fake::connect(caps.clone());
            let _ = tx.send((key.to_string(), server));
            Ok(conn)
        }));
        rx
    }

    /// Incremental sync with open/close and saves, as rust-analyzer has it.
    pub(crate) fn incremental() -> Value {
        json!({"textDocumentSync": {"openClose": true, "change": 2, "save": {"includeText": false}}})
    }

    pub(crate) fn is_lsp(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Lsp(_))
    }

    /// Everything rust-analyzer's client sent so far, through a `test/ping`
    /// round trip (`FakeServer::sent_so_far`): unlike a `try_recv` loop it
    /// cannot miss a message still on its way.
    pub(crate) async fn sent(app: &App, server: &mut FakeServer) -> Vec<Value> {
        server.sent_so_far(&app.lsp.clients["rust-analyzer"]).await
    }

    /// Open `name` with `text` in `app` (fake servers installed) and wait
    /// until its server runs and has the document.
    pub(crate) async fn open_on_server(app: &mut App, servers: &mut UnboundedReceiver<FakeServer>, name: &str, text: &str) -> FakeServer {
        load(app, name, text);
        app.lsp_sync();
        let mut server = servers.try_recv().expect("a server started");
        handle_next(app, |m| matches!(m, AppMsg::Lsp(LspMsg::Started { .. }))).await;
        server.recv("textDocument/didOpen").await;
        server
    }

    fn act(app: &mut App, a: EditorAction) {
        app.perform(Action::Editor(a));
        app.lsp_sync();
    }

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    #[tokio::test]
    async fn documents_open_change_save_move_and_close() {
        let (mut app, dir) = test_app("lsp-sync");
        let mut servers = with_fake_servers(&mut app, incremental());
        let path = load(&mut app, "a.rs", "a😀b\n日本\n");
        app.lsp_sync();
        let mut server = servers.try_recv().expect("started for the first Rust file");
        assert_eq!(app.lsp_status(), Some(LspStatus::Busy("rust-analyzer: starting".into())));
        let init = server.recv("initialize").await;
        assert_eq!(init["params"]["capabilities"]["general"]["positionEncodings"], json!(["utf-16"]));
        assert!(init["params"]["workspaceFolders"][0]["uri"].as_str().unwrap().starts_with("file:///"));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp_status(), Some(LspStatus::Idle("rust-analyzer".into())));
        server.recv("initialized").await;
        let open = server.recv("textDocument/didOpen").await;
        let doc = &open["params"]["textDocument"];
        let uri = sync::uri(&path).unwrap();
        assert_eq!(doc["uri"], uri.as_str());
        assert_eq!(
            (doc["languageId"].as_str(), doc["version"].as_i64(), doc["text"].as_str()),
            (Some("rust"), Some(1), Some("a😀b\n日本\n"))
        );

        // After the emoji: UTF-16 column 3.
        act(&mut app, EditorAction::Move(Motion::Right, false));
        act(&mut app, EditorAction::Move(Motion::Right, false));
        act(&mut app, EditorAction::InsertChar('x'));
        let change = server.recv("textDocument/didChange").await;
        assert_eq!(change["params"]["textDocument"], json!({"uri": uri.as_str(), "version": 2}));
        assert_eq!(change["params"]["contentChanges"], json!([{"range": range(0, 3, 0, 3), "text": "x"}]));
        act(&mut app, EditorAction::Save);
        assert_eq!(server.recv("textDocument/didSave").await["params"], json!({"textDocument": {"uri": uri.as_str()}}));
        act(&mut app, EditorAction::Undo);
        let change = server.recv("textDocument/didChange").await;
        assert_eq!(change["params"]["textDocument"]["version"], 3);
        assert_eq!(change["params"]["contentChanges"], json!([{"range": range(0, 3, 0, 4), "text": ""}]));
        // Nothing changed, nothing sent.
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.is_empty());

        // A rename closes the old document and opens the new one.
        let moved = app.workspace.root.join("b.rs");
        app.editor.retarget(&path, &moved, &app.theme);
        app.lsp_sync();
        assert_eq!(server.recv("textDocument/didClose").await["params"]["textDocument"]["uri"], uri.as_str());
        let open = server.recv("textDocument/didOpen").await;
        let moved_uri = sync::uri(&moved).unwrap();
        assert_eq!(
            (open["params"]["textDocument"]["uri"].as_str(), open["params"]["textDocument"]["version"].as_i64()),
            (Some(moved_uri.as_str()), Some(1))
        );
        // Closed with its unsaved undo.
        app.editor.force_close(app.editor.active().unwrap());
        app.lsp_sync();
        assert_eq!(server.recv("textDocument/didClose").await["params"]["textDocument"]["uri"], moved_uri.as_str());
        // Plain text has no server.
        load(&mut app, "notes.txt", "x");
        act(&mut app, EditorAction::InsertChar('y'));
        assert!(sent(&app, &mut server).await.is_empty());
        assert_eq!(app.lsp_status(), None);

        // Quitting asks the server to shut down and exit.
        app.lsp.shutdown_all().await;
        server.recv("shutdown").await;
        server.recv("exit").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registered_sync_opens_switches_and_closes_documents() {
        let (mut app, dir) = test_app("lsp-dyn-sync");
        let mut servers = with_fake_servers(&mut app, json!({}));
        load(&mut app, "a.rs", "ab\n");
        app.lsp_sync();
        let mut server = servers.try_recv().expect("started");
        handle_next(&mut app, |m| matches!(m, AppMsg::Lsp(LspMsg::Started { .. }))).await;
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/didOpen"));

        let sel = json!([{"language": "rust"}]);
        let reg = |id: &str, method: &str, mut options: Value| {
            options["documentSelector"] = sel.clone();
            json!({"id": id, "method": method, "registerOptions": options})
        };
        let regs = json!({"registrations": [
            reg("o", "textDocument/didOpen", json!({})),
            reg("x", "textDocument/didClose", json!({})),
            reg("c", "textDocument/didChange", json!({"syncKind": 2})),
            reg("s", "textDocument/didSave", json!({"includeText": true})),
        ]});
        server.request(1, "client/registerCapability", regs);
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(1).await["result"], Value::Null);
        app.lsp_sync();
        let open = server.recv("textDocument/didOpen").await;
        assert_eq!(
            (open["params"]["textDocument"]["version"].as_i64(), open["params"]["textDocument"]["text"].as_str()),
            (Some(2), Some("ab\n"))
        );

        act(&mut app, EditorAction::InsertChar('x'));
        let change = server.recv("textDocument/didChange").await;
        assert_eq!(change["params"]["textDocument"]["version"], 3);
        assert!(change["params"]["contentChanges"][0]["range"].is_object());
        act(&mut app, EditorAction::Save);
        assert_eq!(server.recv("textDocument/didSave").await["params"]["text"], "xab\n");

        // Without a change registration the edits stop.
        server.request(2, "client/unregisterCapability", json!({"unregistrations": [{"id": "c", "method": "textDocument/didChange"}]}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(2).await["result"], Value::Null);
        act(&mut app, EditorAction::InsertChar('y'));
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/didChange"));
        // Back on as Full: the whole text goes.
        server.request(
            3,
            "client/registerCapability",
            json!({"registrations": [reg("c", "textDocument/didChange", json!({"syncKind": 1}))]}),
        );
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(3).await["result"], Value::Null);
        app.lsp_sync();
        let change = server.recv("textDocument/didChange").await;
        let changes = change["params"]["contentChanges"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].get("range").is_none());
        assert_eq!(changes[0]["text"], "xyab\n");

        // No longer wanted open: closed once.
        server.request(4, "client/unregisterCapability", json!({"unregistrations": [{"id": "o", "method": "textDocument/didOpen"}]}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(4).await["result"], Value::Null);
        app.lsp_sync();
        server.recv("textDocument/didClose").await;
        // Closed on the server: no changes until it is opened again.
        act(&mut app, EditorAction::InsertChar('z'));
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/didChange"));
        act(&mut app, EditorAction::Save);
        server.request(5, "client/registerCapability", json!({"registrations": [reg("o", "textDocument/didOpen", json!({}))]}));
        handle_next(&mut app, is_lsp).await;
        // The pipe keeps order and the save came first: a `didSave` would precede the response.
        let mut seen = Vec::new();
        loop {
            let msg = server.next().await;
            if msg.get("method").is_none() && msg["id"] == 5 {
                assert_eq!(msg["result"], Value::Null);
                break;
            }
            seen.push(msg);
        }
        assert!(seen.iter().all(|m| m["method"] != "textDocument/didSave"), "{seen:?}");
        app.lsp_sync();
        let open = server.recv("textDocument/didOpen").await;
        assert_eq!(open["params"]["textDocument"]["text"], app.editor.active_buffer().unwrap().text());
        assert_eq!(open["params"]["textDocument"]["text"], "xyzab\n");
        // Opened again: saves are sent again.
        act(&mut app, EditorAction::InsertChar('w'));
        act(&mut app, EditorAction::Save);
        assert_eq!(server.recv("textDocument/didSave").await["params"]["text"], app.editor.active_buffer().unwrap().text());
        app.editor.force_close(app.editor.active().unwrap());
        app.lsp_sync();
        let sent = sent(&app, &mut server).await;
        assert_eq!(sent.iter().filter(|m| m["method"] == "textDocument/didClose").count(), 1, "{sent:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registrations_are_answered_and_dropped_with_their_server() {
        use crate::app::lsp_nav::DOCUMENT_SYMBOL;
        let (mut app, dir) = test_app("lsp-registrations");
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let id = app.editor.active_buffer().unwrap().id;
        let sym = json!({"registrations": [{"id": "sym", "method": "textDocument/documentSymbol"}]});
        server.request(1, "client/registerCapability", sym.clone());
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(1).await["result"], Value::Null);
        assert!(app.lsp_check(id, DOCUMENT_SYMBOL).is_ok(), "no selector: every document");
        // Malformed: refused whole, nothing changed.
        server.request(2, "client/registerCapability", json!({"registrations": "nope"}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(2).await["error"]["code"], ResponseError::INVALID_PARAMS);
        assert!(app.lsp_check(id, DOCUMENT_SYMBOL).is_ok());
        // An unknown id (and the misspelled key) is fine.
        server.request(3, "client/unregisterCapability", json!({"unregistrations": [{"id": "other", "method": "x"}]}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(3).await["result"], Value::Null);
        assert!(app.lsp_check(id, DOCUMENT_SYMBOL).is_ok());

        // A restarted server starts with no registrations.
        app.lsp_restart();
        let mut server = servers.try_recv().expect("restarted");
        handle_next(&mut app, |m| matches!(m, AppMsg::Lsp(LspMsg::Started { .. }))).await;
        server.recv("textDocument/didOpen").await;
        assert!(app.lsp_check(id, DOCUMENT_SYMBOL).is_err());
        // A server that exits drops them.
        server.request(1, "client/registerCapability", sym);
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(1).await["result"], Value::Null);
        assert!(!app.lsp.clients["rust-analyzer"].registry.is_empty());
        let (generation, server_id) = (app.workspace.generation, app.lsp.clients["rust-analyzer"].id);
        app.handle_msg(AppMsg::Lsp(LspMsg::Exited { generation, server: server_id, status: "test".into() }));
        assert!(app.lsp.clients["rust-analyzer"].registry.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_rename_that_changes_the_language_moves_the_document() {
        let (mut app, dir) = test_app("lsp-rename-language");
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        let notes = load(&mut app, "notes.txt", "fn b() {}\n");
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.is_empty(), "plain text has no server");
        // Renamed to Rust: opened as Rust.
        let rs = app.workspace.root.join("notes.rs");
        app.editor.retarget(&notes, &rs, &app.theme);
        app.lsp_sync();
        let doc = server.recv("textDocument/didOpen").await["params"]["textDocument"].clone();
        let uri = sync::uri(&rs).unwrap();
        assert_eq!((doc["uri"].as_str(), doc["languageId"].as_str()), (Some(uri.as_str()), Some("rust")));
        // Renamed back to text: closed.
        app.editor.retarget(&rs, &notes, &app.theme);
        app.lsp_sync();
        assert_eq!(server.recv("textDocument/didClose").await["params"]["textDocument"]["uri"], uri.as_str());
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/didOpen"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scratch_files_get_no_server() {
        let (mut app, dir) = test_app("lsp-scratch");
        let mut servers = with_fake_servers(&mut app, incremental());
        let rust = crate::editor::syntax::LANGUAGES.iter().position(|l| l.name == "rust").unwrap();
        app.create_scratch(rust + 1);
        let scratch = app.editor.active_path().unwrap().to_path_buf();
        assert!(scratch.starts_with(app.paths.scratches_dir()) && scratch.extension().is_some_and(|e| e == "rs"));
        act(&mut app, EditorAction::InsertChar('x'));
        assert!(servers.try_recv().is_err(), "no server started for a scratch");
        assert!(app.lsp.docs.is_empty());

        // A project file moved into the scratches directory leaves its server.
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        let path = app.workspace.root.join("a.rs");
        let uri = sync::uri(&path).unwrap();
        app.editor.retarget(&path, &app.paths.scratches_dir().join("a.rs"), &app.theme);
        app.lsp_sync();
        assert_eq!(server.recv("textDocument/didClose").await["params"]["textDocument"]["uri"], uri.as_str());
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/didOpen"));
        assert!(app.lsp.docs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scratch_files_open_on_their_own_client_when_enabled() {
        let (mut app, dir) = test_app("lsp-scratch-on");
        let mut servers = with_fake_servers(&mut app, incremental());
        app.config.lsp.scratches = true;
        let scratches = app.paths.scratches_dir();
        let rust = crate::editor::syntax::LANGUAGES.iter().position(|l| l.name == "rust").unwrap();
        app.create_scratch(rust + 1);
        let first = app.editor.active_path().unwrap().to_path_buf();
        let id = app.editor.active_buffer().unwrap().id;
        app.lsp_sync();
        let mut scratch_server = servers.try_recv().expect("a server started for the scratch");
        assert_eq!(app.lsp_status(), Some(LspStatus::Busy("rust-analyzer (scratches): starting".into())));
        let init = scratch_server.recv("initialize").await;
        assert_eq!(init["params"]["rootUri"], sync::uri(&scratches).unwrap().as_str());
        assert_eq!(init["params"]["initializationOptions"]["detachedFiles"], json!([first.to_string_lossy()]));
        handle_next(&mut app, is_lsp).await;
        let open = scratch_server.recv("textDocument/didOpen").await;
        assert_eq!(open["params"]["textDocument"]["uri"], sync::uri(&first).unwrap().as_str());
        assert_eq!(app.lsp.docs[&id].server, "rust-analyzer (scratches)");

        // A project file starts the project client, not the scratch one.
        open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        assert!(servers.try_recv().is_err());
        let a = app.editor.active_buffer().unwrap().id;
        assert_eq!(app.lsp.docs[&a].server, "rust-analyzer");

        // The scratch's problems are stored and marked, but not listed.
        publish(&scratch_server, &first, json!([diag_json(0, 0, 0, 1, 1, "bad")]));
        handle_next(&mut app, is_lsp).await;
        assert!(!app.lsp.diagnostics.get(&first).is_empty());
        app.problems.sync(&app.lsp.diagnostics, None);
        assert_eq!(app.problems.list.len, 0, "the project scope lists no scratch");

        // The server's workspace folder is the scratches directory.
        scratch_server.request(7, "workspace/workspaceFolders", json!({}));
        handle_next(&mut app, is_lsp).await;
        let folders = scratch_server.response(7).await;
        assert_eq!(folders["result"][0]["uri"], sync::uri(&scratches).unwrap().as_str());
        assert_eq!(folders["result"][0]["name"], "scratches");

        // A second Rust scratch restarts the scratch client with both files.
        app.create_scratch(rust + 1);
        let second = app.editor.active_path().unwrap().to_path_buf();
        assert_ne!(first, second);
        app.lsp_sync();
        scratch_server.recv("shutdown").await;
        app.lsp_sync();
        let mut third = servers.try_recv().expect("the scratch client started again");
        let init = third.recv("initialize").await;
        let mut want = vec![first.to_string_lossy().into_owned(), second.to_string_lossy().into_owned()];
        want.sort();
        assert_eq!(init["params"]["initializationOptions"]["detachedFiles"], json!(want));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn changes_follow_multi_change_transactions_and_line_endings() {
        let (mut app, dir) = test_app("lsp-multi");
        let mut servers = with_fake_servers(&mut app, incremental());
        let text = "ab 😀\ncd 日本\n";
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", text).await;
        let mut mirror = text.to_string();
        let mut check = async |app: &mut App, server: &mut FakeServer, what: &str| {
            app.lsp_sync();
            let change = server.recv("textDocument/didChange").await;
            let changes: Vec<lsp_types::TextDocumentContentChangeEvent> =
                serde_json::from_value(change["params"]["contentChanges"].clone()).unwrap();
            mirror = apply_changes(&mirror, &changes);
            assert_eq!(mirror, app.editor.active_buffer().unwrap().text(), "{what}: {changes:?}");
            changes
        };
        // Typing an opener over a selection wraps it: the closer goes in
        // first, at the end, then the opener at the start.
        app.perform(Action::Editor(EditorAction::SelectAll));
        app.perform(Action::Editor(EditorAction::InsertChar('(')));
        let changes = check(&mut app, &mut server, "wrap").await;
        assert_eq!(changes.len(), 2);
        assert_eq!(
            serde_json::to_value(&changes).unwrap(),
            json!([{"range": range(2, 0, 2, 0), "text": ")"}, {"range": range(0, 0, 0, 0), "text": "("}])
        );
        // Several edits between two syncs, undo and redo among them.
        app.perform(Action::Editor(EditorAction::Move(Motion::DocEnd, false)));
        app.perform(Action::Editor(EditorAction::InsertText("\nfn 😀() {}".into())));
        app.perform(Action::Editor(EditorAction::Move(Motion::LineStart, false)));
        app.perform(Action::Editor(EditorAction::DeleteLine));
        app.perform(Action::Editor(EditorAction::Undo));
        app.perform(Action::Editor(EditorAction::Undo));
        app.perform(Action::Editor(EditorAction::Redo));
        check(&mut app, &mut server, "edits, undo, redo").await;
        // Line ending conversions: to CRLF inserts before each \n; back to
        // LF removes a \r inside a CRLF break, which no LSP position names,
        // so the whole text goes.
        let id = app.editor.active_buffer().unwrap().id;
        app.editor.set_line_ending(id, LineEnding::CrLf);
        assert!(check(&mut app, &mut server, "to CRLF").await.iter().all(|c| c.range.is_some()));
        app.editor.set_line_ending(id, LineEnding::Lf);
        assert!(check(&mut app, &mut server, "to LF").await[0].range.is_none());
        let id = app.editor.active_buffer().unwrap().id;
        assert!(!app.editor.buffers.iter().find(|b| b.id == id).unwrap().lsp_full_resync);
        // Past the cap the next change is the whole text too.
        for _ in 0..sync::MAX_PENDING + 1 {
            app.perform(Action::Editor(EditorAction::InsertChar('z')));
        }
        let changes = check(&mut app, &mut server, "many edits").await;
        assert!(changes.len() == 1 && changes[0].range.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unicode_separators_keep_lsp_lines() {
        let (mut app, dir) = test_app("lsp-separators");
        let mut servers = with_fake_servers(&mut app, incremental());
        let text = "a\u{c}b\nc\u{2028}d\n";
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", text).await;
        let path = app.workspace.root.join("a.rs");
        // The form feed and U+2028 are inside their lines, as the server counts them.
        act(&mut app, EditorAction::Move(Motion::Down, false));
        act(&mut app, EditorAction::Move(Motion::LineEnd, false));
        act(&mut app, EditorAction::InsertChar('x'));
        let change = server.recv("textDocument/didChange").await;
        assert_eq!(change["params"]["contentChanges"], json!([{"range": range(1, 3, 1, 3), "text": "x"}]));
        let changes: Vec<lsp_types::TextDocumentContentChangeEvent> =
            serde_json::from_value(change["params"]["contentChanges"].clone()).unwrap();
        let mut mirror = apply_changes(text, &changes);
        assert_eq!(mirror, app.editor.active_buffer().unwrap().text());

        // A diagnostic on the `d` lands on the `d`.
        publish(&server, &path, json!([diag_json(1, 2, 1, 3, 1, "m")]));
        handle_next(&mut app, is_lsp).await;
        app.sync_diag_marks();
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.diag_marks.spans.len(), 1);
        assert_eq!(b.diag_marks.spans[0].0, 6..7);
        assert_eq!(b.diag_marks.lines.keys().copied().collect::<Vec<_>>(), [1]);

        // Delete Line on line 0 removes the whole line, separator and all.
        app.perform(Action::Editor(EditorAction::Move(Motion::DocStart, false)));
        act(&mut app, EditorAction::DeleteLine);
        let change = server.recv("textDocument/didChange").await;
        let changes: Vec<lsp_types::TextDocumentContentChangeEvent> =
            serde_json::from_value(change["params"]["contentChanges"].clone()).unwrap();
        mirror = apply_changes(&mirror, &changes);
        assert_eq!(mirror, app.editor.active_buffer().unwrap().text());
        assert_eq!(mirror, "c\u{2028}dx\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn show_message_request_opens_a_choice_and_answers_the_pick() {
        let (mut app, dir) = test_app("lsp-msg-request");
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let actions = json!([{"title": "Yes", "extra": 1}, {"title": "No"}]);
        server.request(7, "window/showMessageRequest", json!({"type": 3, "message": "Reload?", "actions": actions}));
        handle_next(&mut app, is_lsp).await;
        let d = app.dialog.take().expect("a dialog");
        assert_eq!((d.title.as_str(), d.message.as_str()), ("rust-analyzer", "Reload?"));
        match &d.kind {
            crate::ui::dialog::DialogKind::Choice { items, .. } => assert_eq!(items, &["Yes", "No"]),
            k => panic!("{k:?}"),
        }
        app.finish_dialog(d.id, DialogResult::Choice(0));
        assert_eq!(server.response(7).await["result"], json!({"title": "Yes", "extra": 1}));

        server.request(8, "window/showMessageRequest", json!({"type": 3, "message": "Again?", "actions": actions}));
        handle_next(&mut app, is_lsp).await;
        let d = app.dialog.take().expect("a dialog");
        app.finish_dialog(d.id, DialogResult::Cancel);
        assert_eq!(server.response(8).await["result"], Value::Null);

        // Without actions: a notification and a null answer.
        server.request(9, "window/showMessageRequest", json!({"type": 2, "message": "Heads up"}));
        handle_next(&mut app, is_lsp).await;
        assert!(app.dialog.is_none());
        assert_eq!(server.response(9).await["result"], Value::Null);
        assert!(app.notifications.iter().any(|n| n.text == "rust-analyzer: Heads up"), "{:?}", app.notifications);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn messages_requests_progress_and_stale_results() {
        let (mut app, dir) = test_app("lsp-msgs");
        let settings = json!({"rust-analyzer": {"cargo": {"features": "all"}}});
        let ra = crate::lsp::ServerOverride { settings: Some(settings.clone()), ..Default::default() };
        app.config.lsp.servers.insert("rust-analyzer".into(), ra);
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let (g, id) = (app.workspace.generation, app.lsp.clients["rust-analyzer"].id);
        let show = |generation, server, text: &str| {
            let params = json!({"type": 1, "message": text});
            AppMsg::Lsp(LspMsg::Notification { generation, server, method: "window/showMessage".into(), params })
        };
        let shown = |app: &App, text: &str| app.notifications.iter().filter(|n| n.text.ends_with(text)).count();
        // Another root's or another server's message is dropped.
        app.handle_msg(show(g - 1, id, "old root"));
        app.handle_msg(show(g, id + 1, "old server"));
        assert_eq!(shown(&app, "old root") + shown(&app, "old server"), 0);
        app.handle_msg(show(g, id, "current"));
        assert_eq!(app.notifications.last().map(|n| (n.level, n.text.as_str())), Some((Level::Error, "rust-analyzer: current")));
        server.notify("window/showMessage", json!({"type": 2, "message": "careful"}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.notifications.last().map(|n| (n.level, n.text.as_str())), Some((Level::Warn, "rust-analyzer: careful")));

        // Progress shows in the status bar until it ends.
        let progress = |value: Value| json!({"token": "rustAnalyzer/Indexing", "value": value});
        server.notify("$/progress", progress(json!({"kind": "begin", "title": "Indexing", "percentage": 0})));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp_status(), Some(LspStatus::Busy("rust-analyzer: Indexing 0%".into())));
        assert!(app.lsp.busy());
        server.notify("$/progress", progress(json!({"kind": "report", "message": "3/10", "percentage": 30})));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp_status(), Some(LspStatus::Busy("rust-analyzer: Indexing 3/10 30%".into())));
        server.notify("$/progress", progress(json!({"kind": "end"})));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp_status(), Some(LspStatus::Idle("rust-analyzer".into())));
        assert!(!app.lsp.busy());

        // Server requests: settings by section, null for the ones that
        // need nothing, MethodNotFound for the rest.
        server.request(1, "workspace/configuration", json!({"items": [{"section": "rust-analyzer.cargo"}, {"section": "other"}, {}]}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(1).await["result"], json!([{"features": "all"}, null, settings]));
        server.request(2, "window/workDoneProgress/create", json!({"token": "t"}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(2).await["result"], Value::Null);
        server.request(3, "custom/unknown", json!({}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(server.response(3).await["error"]["code"], ResponseError::METHOD_NOT_FOUND);

        // A root switch stops the server; its late messages are dropped.
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(other);
        server.recv("shutdown").await;
        server.recv("exit").await;
        app.handle_msg(show(g, id, "after the switch"));
        app.handle_msg(show(app.workspace.generation, id, "after the switch"));
        assert_eq!(shown(&app, "after the switch"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_server_is_reported_once_until_restart() {
        let (mut app, dir) = test_app("lsp-missing");
        app.config.lsp.enabled = true;
        let ra = crate::lsp::ServerOverride { command: Some(vec!["/nonexistent/rust-analyzer".into()]), ..Default::default() };
        app.config.lsp.servers.insert("rust-analyzer".into(), ra);
        let reports =
            |app: &App| app.notifications.iter().filter(|n| n.text.starts_with("Language server rust-analyzer not found")).count();
        load(&mut app, "a.rs", "fn a() {}\n");
        app.lsp_sync();
        app.lsp_sync();
        load(&mut app, "b.rs", "fn b() {}\n");
        app.perform(Action::Editor(EditorAction::InsertChar('x')));
        app.lsp_sync();
        assert_eq!(reports(&app), 1);
        let n = app.notifications.iter().find(|n| n.text.contains("not found")).unwrap();
        assert_eq!(n.level, Level::Info);
        assert!(n.text.contains("[lsp.servers.rust-analyzer]"), "{}", n.text);
        assert_eq!(app.lsp_status(), Some(LspStatus::Down("LSP: rust-analyzer missing".into())));
        assert!(app.editor.buffers.iter().all(|b| !b.lsp_tracked && b.lsp_pending.is_empty()));
        // Restart tries again.
        app.perform(Action::Lsp(LspAction::Restart));
        assert_eq!(reports(&app), 2);
        // Off: no server, and the status says so.
        app.config.lsp.enabled = false;
        app.perform(Action::Lsp(LspAction::Restart));
        assert_eq!(reports(&app), 2);
        assert_eq!(app.lsp_status(), Some(LspStatus::Down("LSP: off".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn publish(server: &FakeServer, path: &Path, diagnostics: Value) {
        publish_v(server, path, Some(1), diagnostics);
    }

    /// A publish for document `version` (`None`: the key left out).
    fn publish_v(server: &FakeServer, path: &Path, version: Option<i32>, diagnostics: Value) {
        let uri = sync::uri(path).unwrap();
        let mut params = json!({"uri": uri.as_str(), "diagnostics": diagnostics});
        if let Some(v) = version {
            params["version"] = v.into();
        }
        server.notify("textDocument/publishDiagnostics", params);
    }

    fn diag_json(l0: u32, c0: u32, l1: u32, c1: u32, severity: u8, message: &str) -> Value {
        json!({"range": range(l0, c0, l1, c1), "severity": severity, "message": message, "source": "rustc"})
    }

    fn draw(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone()
    }

    fn row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn key(app: &mut App, code: crossterm::event::KeyCode, mods: crossterm::event::KeyModifiers) {
        app.handle_key(crossterm::event::KeyEvent::new(code, mods));
    }

    fn caret(app: &App) -> (usize, usize) {
        let b = app.editor.active_buffer().unwrap();
        crate::editor::selection::pos_to_line_col(&b.rope, b.view.sel().head)
    }

    /// The second line has an emoji (two UTF-16 units) before the error.
    const TEXT: &str = "fn main() {}\nlet a = \"😀\"; let x: i32 = a;\n";

    #[tokio::test]
    async fn diagnostics_show_in_the_gutter_text_status_bar_and_problems() {
        use crate::ui::focus::{PanelId, ToolWindowId};
        use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("lsp-diags");
        let mut servers = with_fake_servers(&mut app, incremental());
        let server = open_on_server(&mut app, &mut servers, "a.rs", TEXT).await;
        let path = app.workspace.root.join("a.rs");
        // No toast over the text, no notification in the status bar.
        app.toasts = Default::default();
        app.notifications.clear();
        assert_eq!(app.problem_counts(), Some(Counts::default()), "open on a server: counts show, zero");
        // UTF-16 column 18 is the `x` after `let`, char column 17.
        publish(
            &server,
            &path,
            json!([diag_json(1, 18, 1, 19, 1, "mismatched types\nexpected `i32`"), diag_json(0, 3, 0, 7, 2, "unused")]),
        );
        handle_next(&mut app, is_lsp).await;
        let store = &app.lsp.diagnostics;
        assert_eq!(store.get(&path).iter().map(|d| d.severity).collect::<Vec<_>>(), [Severity::Warning, Severity::Error]);
        assert_eq!(app.problem_counts(), Some(Counts { errors: 1, warnings: 1, ..Default::default() }));

        let buf = draw(&mut app, 100, 24);
        let b = app.editor.active_buffer().unwrap();
        let line1 = b.rope.line_to_char(1);
        assert_eq!(b.diag_marks.marks.iter().map(|m| m.range.clone()).collect::<Vec<_>>(), [3..7, line1 + 17..line1 + 18]);
        let ta = app.editor.text_area();
        let (y0, y1) = (ta.y, ta.y + 1);
        // The glyph sits in the diff mark column, left of the fold markers.
        assert_eq!(buf[(ta.x - 3, y0)].symbol(), "⚠");
        assert_eq!(buf[(ta.x - 3, y1)].symbol(), "✖");
        assert_eq!(buf[(ta.x - 3, y1)].fg, app.theme.diag_error.fg.unwrap());
        // `x` is at visual column 18 (the emoji takes two cells).
        let x = buf[(ta.x + 18, y1)].clone();
        assert_eq!(x.symbol(), "x");
        assert!(x.modifier.contains(ratatui::style::Modifier::UNDERLINED));
        assert_eq!(x.underline_color, app.theme.diag_error.fg.unwrap());
        assert!(!buf[(ta.x + 17, y1)].modifier.contains(ratatui::style::Modifier::UNDERLINED));
        assert!(buf[(ta.x + 3, y0)].modifier.contains(ratatui::style::Modifier::UNDERLINED), "the warning too");
        let status = row(&buf, 23);
        assert!(status.contains("✖ 1 ⚠ 1"), "{status}");

        // The diagnostic at the caret shows in the status bar.
        app.perform(Action::Lsp(LspAction::NextProblem));
        assert_eq!(caret(&app), (0, 3));
        app.perform(Action::Lsp(LspAction::NextProblem));
        assert_eq!(caret(&app), (1, 17));
        let status = row(&draw(&mut app, 140, 24), 23);
        assert!(status.contains("✖ mismatched types "), "{status}");
        // F2 wraps; Shift+F2 goes back, wrapping too.
        key(&mut app, KeyCode::F(2), KeyModifiers::NONE);
        assert_eq!(caret(&app), (0, 3));
        key(&mut app, KeyCode::F(2), KeyModifiers::SHIFT);
        assert_eq!(caret(&app), (1, 17));

        // A click on the counts opens Problems on this file.
        let (_, r) = *app.status_hits.iter().find(|(h, _)| *h == crate::ui::status_bar::StatusHit::Problems).unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: r.x + 1,
            row: r.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.layout.is_visible(ToolWindowId::Problems) && app.focus.is(PanelId::Problems));
        app.perform(Action::App(crate::actions::AppAction::FocusEditor));
        app.perform(Action::Editor(EditorAction::Move(Motion::DocStart, false)));

        // Alt+6 focuses the window; the first problem is selected, Down
        // the second, Enter jumps to it.
        key(&mut app, KeyCode::Char('6'), KeyModifiers::ALT);
        assert!(app.focus.is(PanelId::Problems));
        let buf = draw(&mut app, 100, 24);
        let rows: Vec<String> = (0..24).map(|y| row(&buf, y)).collect();
        assert!(rows.iter().any(|r| r.contains("Problems: 1 error, 1 warning")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("a.rs") && r.contains("1 error, 1 warning")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("✖ mismatched types  rustc  2:19")), "{rows:#?}");
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(caret(&app), (1, 17));
        assert!(app.focus.is(PanelId::Editor));

        // A closed file opens at the converted position once loaded.
        let other = app.workspace.root.join("b.rs");
        std::fs::write(&other, "// 日本😀 x\n").unwrap();
        publish(&server, &other, json!([diag_json(0, 8, 0, 9, 1, "in b")]));
        handle_next(&mut app, is_lsp).await;
        app.perform(Action::App(crate::actions::AppAction::FocusPanel(PanelId::Problems)));
        app.perform(Action::Problems(crate::actions::ProblemsAction::End));
        app.perform(Action::Problems(crate::actions::ProblemsAction::Open));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_path(), Some(other.as_path()));
        // `// 日本` is 5 units, 😀 2, then a space: unit 8 is `x`, char 7.
        assert_eq!(caret(&app), (0, 7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_diagnostics_are_dropped_and_cleared_with_their_server() {
        let (mut app, dir) = test_app("lsp-diags-stale");
        let mut servers = with_fake_servers(&mut app, incremental());
        let server = open_on_server(&mut app, &mut servers, "a.rs", TEXT).await;
        let path = app.workspace.root.join("a.rs");
        let (g, id) = (app.workspace.generation, app.lsp.clients["rust-analyzer"].id);
        let uri = sync::uri(&path).unwrap();
        let msg = |generation, server| {
            let params = json!({"uri": uri.as_str(), "diagnostics": [diag_json(0, 0, 0, 1, 1, "stale")]});
            AppMsg::Lsp(LspMsg::Notification { generation, server, method: "textDocument/publishDiagnostics".into(), params })
        };
        app.handle_msg(msg(g - 1, id));
        app.handle_msg(msg(g, id + 1));
        assert_eq!(app.lsp.diagnostics.total(), 0, "another root's or server's publish is dropped");
        publish(&server, &path, json!([diag_json(0, 0, 0, 1, 1, "e")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp.diagnostics.total(), 1);
        // The server exits: its diagnostics go.
        app.handle_msg(AppMsg::Lsp(LspMsg::Exited { generation: g, server: id, status: "killed".into() }));
        assert_eq!(app.lsp.diagnostics.total(), 0);
        assert_eq!(app.problem_counts(), None, "no server, no diagnostics: no counts");
        // A restarted server publishes again; a root switch clears it.
        app.perform(Action::Lsp(LspAction::Restart));
        let server = servers.recv().await.unwrap();
        handle_next(&mut app, |m| matches!(m, AppMsg::Lsp(LspMsg::Started { .. }))).await;
        publish(&server, &path, json!([diag_json(0, 0, 0, 1, 2, "w")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp.diagnostics.total(), 1);
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(other);
        assert_eq!(app.lsp.diagnostics.total(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The active buffer's diagnostic marks as (line, char column) ranges.
    fn mark_cols(app: &mut App) -> Vec<(usize, usize, usize)> {
        app.sync_diag_marks();
        let b = app.editor.active_buffer().unwrap();
        b.diag_marks
            .marks
            .iter()
            .map(|m| {
                let start = b.rope.line_to_char(m.line);
                (m.line, m.range.start - start, m.range.end - start)
            })
            .collect()
    }

    fn stored(app: &App, path: &Path) -> Vec<lsp_types::Range> {
        app.lsp.diagnostics.get(path).iter().map(|d| d.range).collect()
    }

    fn lsp_range(l0: u32, c0: u32, l1: u32, c1: u32) -> lsp_types::Range {
        lsp_types::Range::new(lsp_types::Position::new(l0, c0), lsp_types::Position::new(l1, c1))
    }

    #[tokio::test]
    async fn diagnostics_follow_edits_until_the_next_publish() {
        let (mut app, dir) = test_app("lsp-diags-follow");
        let mut servers = with_fake_servers(&mut app, incremental());
        let server = open_on_server(&mut app, &mut servers, "a.rs", TEXT).await;
        let path = app.workspace.root.join("a.rs");
        publish(&server, &path, json!([diag_json(1, 18, 1, 19, 1, "mismatched types")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(mark_cols(&mut app), [(1, 17, 18)]);
        // A line inserted at the top: the mark moves down a line.
        act(&mut app, EditorAction::InsertText("// top\n".into()));
        draw(&mut app, 100, 24);
        assert_eq!(mark_cols(&mut app), [(2, 17, 18)]);
        assert_eq!(stored(&app, &path), [lsp_range(2, 18, 2, 19)]);
        // A char typed on its line before it: one column right.
        act(&mut app, EditorAction::Move(Motion::Down, false));
        act(&mut app, EditorAction::InsertChar('q'));
        assert_eq!(mark_cols(&mut app), [(2, 18, 19)]);
        // Undo both: back where it was published.
        act(&mut app, EditorAction::Undo);
        act(&mut app, EditorAction::Undo);
        assert_eq!(app.editor.active_buffer().unwrap().text(), TEXT);
        assert_eq!(mark_cols(&mut app), [(1, 17, 18)]);
        assert_eq!(stored(&app, &path), [lsp_range(1, 18, 1, 19)]);
        // The erroneous `x` deleted: the problem goes with it.
        app.perform(Action::Editor(EditorAction::Move(Motion::DocStart, false)));
        app.perform(Action::Editor(EditorAction::Move(Motion::Down, false)));
        for _ in 0..17 {
            app.perform(Action::Editor(EditorAction::Move(Motion::Right, false)));
        }
        app.perform(Action::Editor(EditorAction::Move(Motion::Right, true)));
        act(&mut app, EditorAction::Backspace);
        assert!(mark_cols(&mut app).is_empty());
        assert_eq!(app.problem_counts().map(|c| c.errors), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn older_publish_is_mapped_forward() {
        let (mut app, dir) = test_app("lsp-diags-older");
        let mut servers = with_fake_servers(&mut app, incremental());
        let server = open_on_server(&mut app, &mut servers, "a.rs", TEXT).await;
        let path = app.workspace.root.join("a.rs");
        act(&mut app, EditorAction::InsertText("// top\n".into()));
        assert_eq!(app.lsp.docs.values().next().unwrap().version, 2);
        let error = || json!([diag_json(1, 18, 1, 19, 1, "e")]);
        publish_v(&server, &path, Some(1), error());
        handle_next(&mut app, is_lsp).await;
        assert_eq!(stored(&app, &path), [lsp_range(2, 18, 2, 19)], "made for version 1: moved through version 2's edits");
        publish_v(&server, &path, Some(2), error());
        handle_next(&mut app, is_lsp).await;
        assert_eq!(stored(&app, &path), [lsp_range(1, 18, 1, 19)], "the current version: as sent");
        publish_v(&server, &path, None, error());
        handle_next(&mut app, is_lsp).await;
        assert_eq!(stored(&app, &path), [lsp_range(1, 18, 1, 19)], "no version counts as the current one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unmappable_publish_is_stored_as_sent() {
        let (mut app, dir) = test_app("lsp-diags-unmappable");
        let mut servers = with_fake_servers(&mut app, incremental());
        let server = open_on_server(&mut app, &mut servers, "a.rs", TEXT).await;
        let path = app.workspace.root.join("a.rs");
        app.perform(Action::Editor(EditorAction::InsertText("// top\n".into())));
        app.editor.active_buffer_mut().unwrap().lsp_full_resync = true;
        app.lsp_sync();
        publish_v(&server, &path, Some(1), json!([diag_json(1, 18, 1, 19, 1, "e")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(stored(&app, &path), [lsp_range(1, 18, 1, 19)], "a whole-text change in between: stored as published");
        // A version this document never had: as published too.
        publish_v(&server, &path, Some(7), json!([diag_json(0, 0, 0, 1, 1, "e")]));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(stored(&app, &path), [lsp_range(0, 0, 0, 1)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Make `typos` an extra server for every language.
    fn typos(app: &mut App) {
        let o = crate::lsp::ServerOverride {
            command: Some(vec!["typos-lsp".into()]),
            languages: Some(vec!["*".into()]),
            extra: true,
            ..Default::default()
        };
        app.config.lsp.servers.insert("typos".into(), o);
    }

    /// The servers that `lsp_sync` just started, by client key, once they all run.
    async fn started(app: &mut App, rx: &mut UnboundedReceiver<(String, FakeServer)>, names: &[&str]) -> HashMap<String, FakeServer> {
        let servers: HashMap<_, _> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let mut got: Vec<_> = servers.keys().map(String::as_str).collect();
        got.sort();
        assert_eq!(got, names);
        let ids: Vec<ServerId> = names.iter().map(|n| app.lsp.clients[*n].id).collect();
        for _ in &ids {
            handle_next(app, |m| matches!(m, AppMsg::Lsp(LspMsg::Started { server, .. }) if ids.contains(server))).await;
        }
        servers
    }

    fn definition_caps() -> Value {
        let mut caps = incremental();
        caps["definitionProvider"] = json!(true);
        caps
    }

    #[tokio::test]
    async fn an_extra_server_syncs_next_to_the_main_one() {
        let (mut app, dir) = test_app("lsp-extra-sync");
        let mut rx = with_named_fake_servers(&mut app, definition_caps(), &[]);
        typos(&mut app);
        let path = load(&mut app, "a.rs", "fn main() {}\n");
        app.lsp_sync();
        let mut servers = started(&mut app, &mut rx, &["rust-analyzer", "typos"]).await;
        for s in servers.values_mut() {
            let open = s.recv("textDocument/didOpen").await;
            let doc = &open["params"]["textDocument"];
            assert_eq!((doc["version"].as_i64(), doc["text"].as_str()), (Some(1), Some("fn main() {}\n")));
        }
        act(&mut app, EditorAction::InsertChar('x'));
        for s in servers.values_mut() {
            let change = s.recv("textDocument/didChange").await;
            assert_eq!(change["params"]["textDocument"]["version"], 2);
            assert_eq!(change["params"]["contentChanges"], json!([{"range": range(0, 0, 0, 0), "text": "x"}]));
        }
        act(&mut app, EditorAction::Save);
        for s in servers.values_mut() {
            s.recv("textDocument/didSave").await;
        }
        publish_v(&servers["rust-analyzer"], &path, None, json!([diag_json(0, 3, 0, 7, 1, "from rustc")]));
        publish_v(&servers["typos"], &path, None, json!([diag_json(0, 8, 0, 12, 3, "teh")]));
        handle_next(&mut app, is_lsp).await;
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp.diagnostics.get(&path).len(), 2);
        assert_eq!(app.problem_counts().map(|c| c.errors), Some(1));
        assert_eq!(app.lsp.diagnostics.counts(&path).infos, 1);
        // Requests go to the main server only.
        app.perform(Action::Lsp(LspAction::GotoDefinition));
        servers.get_mut("rust-analyzer").unwrap().recv("textDocument/definition").await;
        let typos_sent = servers.get_mut("typos").unwrap().sent_so_far(&app.lsp.clients["typos"]).await;
        assert!(typos_sent.iter().all(|m| m["method"] != "textDocument/definition"), "{typos_sent:?}");
        assert_eq!(app.lsp_status(), Some(LspStatus::Idle("rust-analyzer".into())));
        app.editor.force_close(app.editor.active().unwrap());
        app.lsp_sync();
        for s in servers.values_mut() {
            s.recv("textDocument/didClose").await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_extra_server_alone_serves_a_language_without_a_main_server() {
        let (mut app, dir) = test_app("lsp-extra-alone");
        let mut rx = with_named_fake_servers(&mut app, incremental(), &[]);
        typos(&mut app);
        let off = crate::lsp::ServerOverride { command: Some(vec![]), ..Default::default() };
        app.config.lsp.servers.insert("rust-analyzer".into(), off);
        load(&mut app, "a.rs", "fn main() {}\n");
        app.lsp_sync();
        let mut servers = started(&mut app, &mut rx, &["typos"]).await;
        let server = servers.get_mut("typos").unwrap();
        server.recv("textDocument/didOpen").await;
        let id = app.editor.active_buffer().unwrap().id;
        assert!(!app.lsp.docs.contains_key(&id) && app.lsp.has_doc(id));
        assert_eq!(app.lsp_status(), Some(LspStatus::Idle("typos".into())));
        for (version, col, c) in [(2, 0, 'x'), (3, 1, 'y')] {
            act(&mut app, EditorAction::InsertChar(c));
            let change = server.recv("textDocument/didChange").await;
            assert_eq!(change["params"]["textDocument"]["version"], version);
            assert_eq!(change["params"]["contentChanges"], json!([{"range": range(0, col, 0, col), "text": c.to_string()}]));
        }
        assert_eq!(app.problem_counts(), Some(Counts::default()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edits_reach_the_extra_server_while_the_main_one_is_missing() {
        let (mut app, dir) = test_app("lsp-extra-main-missing");
        let mut rx = with_named_fake_servers(&mut app, incremental(), &["rust-analyzer"]);
        typos(&mut app);
        load(&mut app, "a.rs", "fn main() {}\n");
        app.lsp_sync();
        let mut servers = started(&mut app, &mut rx, &["typos"]).await;
        let server = servers.get_mut("typos").unwrap();
        server.recv("textDocument/didOpen").await;
        let missing =
            app.notifications.iter().filter(|n| n.text.starts_with("Language server rust-analyzer not found")).collect::<Vec<_>>();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].level, Level::Info);
        for (version, col, c) in [(2, 0, 'x'), (3, 1, 'y')] {
            act(&mut app, EditorAction::InsertChar(c));
            let change = server.recv("textDocument/didChange").await;
            assert_eq!(change["params"]["textDocument"]["version"], version);
            assert_eq!(change["params"]["contentChanges"], json!([{"range": range(0, col, 0, col), "text": c.to_string()}]));
        }
        assert_eq!(app.lsp_status(), Some(LspStatus::Down("LSP: rust-analyzer missing".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_extra_server_going_away_keeps_the_main_one() {
        let (mut app, dir) = test_app("lsp-extra-gone");
        let mut rx = with_named_fake_servers(&mut app, incremental(), &[]);
        typos(&mut app);
        let path = load(&mut app, "a.rs", "fn main() {}\n");
        app.lsp_sync();
        let mut servers = started(&mut app, &mut rx, &["rust-analyzer", "typos"]).await;
        for s in servers.values_mut() {
            s.recv("textDocument/didOpen").await;
        }
        publish_v(&servers["rust-analyzer"], &path, None, json!([diag_json(0, 3, 0, 7, 1, "from rustc")]));
        publish_v(&servers["typos"], &path, None, json!([diag_json(0, 8, 0, 12, 3, "teh")]));
        handle_next(&mut app, is_lsp).await;
        handle_next(&mut app, is_lsp).await;
        assert_eq!(app.lsp.diagnostics.get(&path).len(), 2);
        let id = app.lsp.clients["typos"].id;
        app.lsp_server_down("typos", id, "gone".into());
        assert!(app.lsp.diagnostics.of_server(&path, "typos").is_empty());
        assert_eq!(app.lsp.diagnostics.get(&path).len(), 1);
        let buf = app.editor.active_buffer().unwrap();
        assert!(app.lsp.docs.contains_key(&buf.id) && buf.lsp_tracked);
        act(&mut app, EditorAction::InsertChar('x'));
        let change = servers.get_mut("rust-analyzer").unwrap().recv("textDocument/didChange").await;
        assert_eq!(change["params"]["contentChanges"], json!([{"range": range(0, 0, 0, 0), "text": "x"}]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_main_server_with_the_wildcard_is_ignored_with_a_warning() {
        let (mut app, dir) = test_app("lsp-wildcard");
        let mut rx = with_named_fake_servers(&mut app, incremental(), &[]);
        let o = crate::lsp::ServerOverride { command: Some(vec!["spell".into()]), languages: Some(vec!["*".into()]), ..Default::default() };
        app.config.lsp.servers.insert("spell".into(), o);
        load(&mut app, "a.rs", "fn main() {}\n");
        app.lsp_sync();
        app.lsp_sync();
        let text = "Language server spell is ignored: languages = [\"*\"] needs extra = true in [lsp.servers.spell]";
        let warns = |app: &App| app.notifications.iter().filter(|n| n.level == Level::Warn && n.text == text).count();
        assert_eq!(warns(&app), 1);
        assert!(!app.lsp.clients.contains_key("spell"));
        assert!(rx.try_recv().is_ok_and(|(k, _)| k == "rust-analyzer"));
        app.lsp_restart();
        assert_eq!(warns(&app), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn will_file_ops_and_workspace_symbols_skip_extra_servers() {
        use crate::lsp::caps::{DID_RENAME_FILES, WILL_RENAME_FILES};
        let (mut app, dir) = test_app("lsp-extra-fileops");
        let filters = json!([{"scheme": "file", "pattern": {"glob": "**/*.rs", "matches": "file"}}]);
        let mut caps = incremental();
        caps["workspace"] = json!({"fileOperations": {"willRename": {"filters": filters}, "didRename": {"filters": filters}}});
        caps["workspaceSymbolProvider"] = json!(true);
        let mut rx = with_named_fake_servers(&mut app, caps, &[]);
        typos(&mut app);
        load(&mut app, "a.rs", "fn main() {}\n");
        app.lsp_sync();
        let mut servers = started(&mut app, &mut rx, &["rust-analyzer", "typos"]).await;
        for s in servers.values_mut() {
            s.recv("textDocument/didOpen").await;
        }
        let root = app.workspace.root.clone();
        let op = crate::files::ops::FsOp::Rename { from: root.join("m.rs"), to: root.join("n.rs"), overwrite: false };
        assert_eq!(app.file_op_servers(WILL_RENAME_FILES, &op, false), ["rust-analyzer"]);
        assert_eq!(app.file_op_servers(DID_RENAME_FILES, &op, false), ["rust-analyzer", "typos"]);
        app.open_palette(crate::actions::PaletteKind::Symbols);
        for c in "hel".chars() {
            key(&mut app, crossterm::event::KeyCode::Char(c), crossterm::event::KeyModifiers::NONE);
        }
        servers.get_mut("rust-analyzer").unwrap().recv("workspace/symbol").await;
        let typos_sent = servers.get_mut("typos").unwrap().sent_so_far(&app.lsp.clients["typos"]).await;
        assert!(typos_sent.iter().all(|m| m["method"] != "workspace/symbol"), "{typos_sent:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
