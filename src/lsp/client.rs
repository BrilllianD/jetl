//! One language server: the process (own session, killed with its process
//! group), a driver task that owns the connection and the pending requests,
//! and the [`Client`] handle the app keeps.
//!
//! The driver sends `initialize` first and holds everything else back until
//! the answer arrives (then `initialized` and the held messages go out, and
//! the app gets `LspMsg::Started`). Responses to the app's requests come back
//! with the request's [`PendingTag`]; server requests and notifications are
//! forwarded as they are. [`Client::shutdown`] sends `shutdown` and `exit`
//! and waits up to two seconds before killing the group.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use lsp_types::{
    ChangeAnnotationWorkspaceEditClientCapabilities, ClientCapabilities, ClientInfo, CodeActionCapabilityResolveSupport,
    CodeActionClientCapabilities, CodeActionKindLiteralSupport, CodeActionLiteralSupport, CompletionClientCapabilities,
    CompletionItemCapability, CompletionItemCapabilityResolveSupport, CompletionItemKind, CompletionItemKindCapability, DiagnosticTag,
    DidChangeWatchedFilesClientCapabilities, DocumentFormattingClientCapabilities, DocumentRangeFormattingClientCapabilities,
    DocumentSymbolClientCapabilities, ExecuteCommandClientCapabilities, FailureHandlingKind, GeneralClientCapabilities, GotoCapability,
    HoverClientCapabilities, InitializeParams, InitializeResult, MarkupKind, MessageActionItemCapabilities, ParameterInformationSettings,
    PositionEncodingKind, PublishDiagnosticsClientCapabilities, ReferenceClientCapabilities, RenameClientCapabilities,
    ResourceOperationKind, ShowMessageRequestClientCapabilities, SignatureHelpClientCapabilities, SignatureInformationSettings, TagSupport,
    TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities, WindowClientCapabilities, WorkspaceClientCapabilities,
    WorkspaceEditClientCapabilities, WorkspaceFileOperationsClientCapabilities, WorkspaceFolder, WorkspaceSymbolClientCapabilities,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use super::transport::{self, Incoming, Outgoing, RequestId, ResponseError};
use super::{LspMsg, PendingTag, ServerId, sync};
use crate::event::AppMsg;
use crate::run::process::kill_group;

/// How long [`Client::shutdown`] waits for the server to exit.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Messages held back while the server initializes; older ones go first.
const MAX_QUEUED: usize = 64;
/// The ids of the driver's own requests (the app's start at 1).
const INITIALIZE_ID: i64 = 0;
const SHUTDOWN_ID: i64 = -1;

/// A connection to a server: its stdout and stdin, and the process.
pub struct Conn {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// `None` for an in-process server (tests).
    pub child: Option<tokio::process::Child>,
}

/// Start `argv` in `cwd` as a server: a new session (no controlling
/// terminal; [`kill_group`] stops it with everything it started), stdin and
/// stdout for the protocol, stderr to the log.
pub fn spawn_process(name: &str, argv: &[String], cwd: &Path) -> std::io::Result<Conn> {
    let program = argv.first().filter(|p| !p.is_empty()).ok_or_else(|| std::io::Error::other("empty command"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        return Err(std::io::Error::other("no pipes to the server"));
    };
    let name = name.to_string();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!("{name} stderr: {line}");
        }
    });
    Ok(Conn { reader: Box::new(stdout), writer: Box::new(stdin), child: Some(child) })
}

/// What the driver needs to start a server.
#[derive(Debug, Clone)]
pub struct StartSpec {
    pub id: ServerId,
    pub name: String,
    pub root: PathBuf,
    /// Workspace generation, carried by every `LspMsg`.
    pub generation: u64,
    pub init_options: Option<Value>,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    /// Whether `initialize` offers `workspace.didChangeWatchedFiles`.
    pub watch_files: bool,
}

/// Where a server is in its life.
#[derive(Debug)]
pub enum State {
    /// Waiting for the answer to `initialize`.
    Starting,
    Running(Box<lsp_types::ServerCapabilities>),
    /// Failed to start, crashed or exited; not restarted until
    /// `lsp.restart`.
    Failed(String),
}

enum Command {
    Send(Outgoing, Option<PendingTag>),
    Shutdown(oneshot::Sender<()>),
}

/// The app's handle on a server. Dropping it kills the server's process
/// group; [`Client::shutdown`] asks it to exit first.
pub struct Client {
    pub id: ServerId,
    pub name: String,
    pub state: State,
    next_id: i64,
    cmds: UnboundedSender<Command>,
    /// The process group, shared with the driver, which clears it once the
    /// group is killed and the server reaped (so a reused id is never
    /// signalled); 0 for an in-process server.
    pid: Arc<AtomicU32>,
    /// The server's dynamic registrations (`client/registerCapability`),
    /// kept by the app on the main thread; gone with the `Client`.
    pub registry: super::registry::Registry,
    /// Whether this server was offered `didChangeWatchedFiles`.
    pub watch_files: bool,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("id", &self.id).field("name", &self.name).field("state", &self.state).finish()
    }
}

impl Client {
    /// Start the driver over `conn`; it reports on `bus`.
    pub fn start(spec: StartSpec, conn: Conn, bus: UnboundedSender<AppMsg>) -> Self {
        let pid = Arc::new(AtomicU32::new(conn.child.as_ref().and_then(|c| c.id()).unwrap_or(0)));
        let (cmds, rx) = mpsc::unbounded_channel();
        let (id, name, watch_files) = (spec.id, spec.name.clone(), spec.watch_files);
        tokio::spawn(drive(spec, conn, rx, bus, Arc::clone(&pid)));
        Self { id, name, state: State::Starting, next_id: 1, cmds, pid, registry: Default::default(), watch_files }
    }

    pub fn capabilities(&self) -> Option<&lsp_types::ServerCapabilities> {
        match &self.state {
            State::Running(caps) => Some(caps),
            _ => None,
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.cmds.send(Command::Send(Outgoing::notification(method, params), None));
    }

    /// Send a request; its answer comes back as `LspMsg::Response` with
    /// `tag`. Returns the id, for `$/cancelRequest`.
    pub fn request(&mut self, method: &str, params: Value, tag: PendingTag) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.cmds.send(Command::Send(Outgoing::Request { id, method: method.into(), params }, Some(tag)));
        id
    }

    /// Tell the server request `id` is no longer wanted
    /// (`$/cancelRequest`). Its answer (often a `RequestCancelled` error)
    /// still arrives and is dropped by its seq.
    pub fn cancel(&self, id: i64) {
        self.notify("$/cancelRequest", json!({ "id": id }));
    }

    /// Answer a server request.
    pub fn respond(&self, id: RequestId, result: Result<Value, ResponseError>) {
        let _ = self.cmds.send(Command::Send(Outgoing::Response { id, result }, None));
    }

    /// Ask the server to shut down and exit; the future ends when it has,
    /// or after [`SHUTDOWN_TIMEOUT`], and kills the process group either way.
    pub fn shutdown(mut self) -> impl Future<Output = ()> + Send + 'static {
        let (done, wait) = oneshot::channel();
        let _ = self.cmds.send(Command::Shutdown(done));
        // Dropping `self` must not kill the server before it could exit.
        let pid = std::mem::take(&mut self.pid);
        let name = self.name.clone();
        async move {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, wait).await.is_err() {
                tracing::info!("{name} did not exit in time; killing it");
            }
            kill_group(pid.load(Ordering::SeqCst));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        kill_group(self.pid.load(Ordering::SeqCst));
    }
}

/// What jetl supports, sent with `initialize`. Positions are UTF-16 only;
/// dynamic registration where `Caps::has` honours it (`lsp::caps`); file
/// operations (`will*Files`, `did*Files`) from the Project tree;
/// `didChangeWatchedFiles` only while jetl's file watcher runs (`watch_files`).
pub fn client_capabilities(watch_files: bool) -> ClientCapabilities {
    ClientCapabilities {
        general: Some(GeneralClientCapabilities { position_encodings: Some(vec![PositionEncodingKind::UTF16]), ..Default::default() }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(true),
                did_save: Some(true),
                ..Default::default()
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                tag_support: Some(TagSupport { value_set: vec![DiagnosticTag::UNNECESSARY, DiagnosticTag::DEPRECATED] }),
                version_support: Some(true),
                ..Default::default()
            }),
            definition: Some(GotoCapability { dynamic_registration: Some(true), link_support: Some(true) }),
            type_definition: Some(GotoCapability { dynamic_registration: Some(true), link_support: Some(true) }),
            implementation: Some(GotoCapability { dynamic_registration: Some(true), link_support: Some(true) }),
            declaration: Some(GotoCapability { dynamic_registration: Some(true), link_support: Some(true) }),
            references: Some(ReferenceClientCapabilities { dynamic_registration: Some(true) }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(true),
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            }),
            signature_help: Some(SignatureHelpClientCapabilities {
                dynamic_registration: Some(true),
                signature_information: Some(SignatureInformationSettings {
                    documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    parameter_information: Some(ParameterInformationSettings { label_offset_support: Some(true) }),
                    active_parameter_support: Some(true),
                }),
                context_support: Some(true),
            }),
            completion: Some(completion_capabilities()),
            rename: Some(RenameClientCapabilities {
                dynamic_registration: Some(true),
                prepare_support: Some(true),
                honors_change_annotations: Some(true),
                ..Default::default()
            }),
            code_action: Some(code_action_capabilities()),
            document_symbol: Some(DocumentSymbolClientCapabilities {
                dynamic_registration: Some(true),
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            formatting: Some(DocumentFormattingClientCapabilities { dynamic_registration: Some(true) }),
            range_formatting: Some(DocumentRangeFormattingClientCapabilities { dynamic_registration: Some(true) }),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            show_message: Some(ShowMessageRequestClientCapabilities {
                message_action_item: Some(MessageActionItemCapabilities { additional_properties_support: Some(true) }),
            }),
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            workspace_folders: Some(true),
            apply_edit: Some(true),
            // File operations are checked with the text edits before anything changes;
            // the disk steps then run in order and stop at the first failure (abort).
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                resource_operations: Some(vec![
                    ResourceOperationKind::Create,
                    ResourceOperationKind::Rename,
                    ResourceOperationKind::Delete,
                ]),
                failure_handling: Some(FailureHandlingKind::Abort),
                change_annotation_support: Some(ChangeAnnotationWorkspaceEditClientCapabilities { groups_on_label: Some(false) }),
                ..Default::default()
            }),
            symbol: Some(WorkspaceSymbolClientCapabilities { dynamic_registration: Some(true), ..Default::default() }),
            execute_command: Some(ExecuteCommandClientCapabilities { dynamic_registration: Some(true) }),
            did_change_watched_files: watch_files.then_some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(true),
            }),
            file_operations: Some(WorkspaceFileOperationsClientCapabilities {
                dynamic_registration: Some(true),
                did_create: Some(true),
                will_create: Some(true),
                did_rename: Some(true),
                will_rename: Some(true),
                did_delete: Some(true),
                will_delete: Some(true),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Completion: snippets (tabstops walked with Tab; choices and variables
/// flattened), `textEdit` and `additionalTextEdits`, resolved lazily when
/// accepting; every item kind; documentation shown beside the popup,
/// resolved lazily; labelDetails; commit characters.
fn completion_capabilities() -> CompletionClientCapabilities {
    let kinds = (1..=25).filter_map(|k| serde_json::from_value::<CompletionItemKind>(json!(k)).ok()).collect();
    CompletionClientCapabilities {
        dynamic_registration: Some(true),
        completion_item: Some(CompletionItemCapability {
            snippet_support: Some(true),
            documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            deprecated_support: Some(true),
            preselect_support: Some(true),
            insert_replace_support: Some(false),
            label_details_support: Some(true),
            commit_characters_support: Some(true),
            resolve_support: Some(CompletionItemCapabilityResolveSupport {
                properties: ["documentation", "detail", "additionalTextEdits"].map(String::from).to_vec(),
            }),
            ..Default::default()
        }),
        completion_item_kind: Some(CompletionItemKindCapability { value_set: Some(kinds) }),
        context_support: Some(true),
        ..Default::default()
    }
}

/// Code actions as literals with their kinds, resolved lazily for their
/// edit when the server supports it.
fn code_action_capabilities() -> CodeActionClientCapabilities {
    let kinds = ["", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports"];
    CodeActionClientCapabilities {
        dynamic_registration: Some(true),
        code_action_literal_support: Some(CodeActionLiteralSupport {
            code_action_kind: CodeActionKindLiteralSupport { value_set: kinds.map(String::from).to_vec() },
        }),
        is_preferred_support: Some(true),
        disabled_support: Some(true),
        data_support: Some(true),
        honors_change_annotations: Some(true),
        resolve_support: Some(CodeActionCapabilityResolveSupport { properties: vec!["edit".into()] }),
    }
}

#[allow(deprecated)] // `root_uri`, for servers that predate workspace folders
fn initialize_params(spec: &StartSpec) -> Value {
    let root = sync::uri(&spec.root);
    let name = spec.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let params = InitializeParams {
        process_id: Some(std::process::id()),
        root_uri: root.clone(),
        initialization_options: spec.init_options.clone(),
        capabilities: client_capabilities(spec.watch_files),
        workspace_folders: root.map(|uri| vec![WorkspaceFolder { uri, name }]),
        client_info: Some(ClientInfo { name: "jetl".into(), version: Some(env!("CARGO_PKG_VERSION").into()) }),
        ..Default::default()
    };
    serde_json::to_value(params).unwrap_or_default()
}

/// Why the driver stopped.
enum End {
    /// Asked to (shutdown, or the handle was dropped).
    Stopped,
    Failed(String),
    Exited(String),
}

async fn wait_child(child: &mut Option<tokio::process::Child>) -> String {
    match child {
        Some(c) => match c.wait().await {
            Ok(s) => match s.code() {
                Some(code) => format!("exit code {code}"),
                None => "killed by a signal".into(),
            },
            Err(e) => e.to_string(),
        },
        None => std::future::pending().await,
    }
}

async fn drive(spec: StartSpec, conn: Conn, mut cmds: UnboundedReceiver<Command>, bus: UnboundedSender<AppMsg>, pid: Arc<AtomicU32>) {
    let Conn { reader, writer, mut child } = conn;
    let (in_tx, mut incoming) = mpsc::unbounded_channel();
    let (out, out_rx) = mpsc::unbounded_channel();
    transport::spawn_reader(spec.name.clone(), reader, in_tx);
    transport::spawn_writer(spec.name.clone(), writer, out_rx);
    let (generation, server) = (spec.generation, spec.id);
    let send = |msg: LspMsg| {
        let _ = bus.send(AppMsg::Lsp(msg));
    };
    let _ = out.send(Outgoing::Request { id: INITIALIZE_ID, method: "initialize".into(), params: initialize_params(&spec) });
    // The app's requests in flight, with when they were sent.
    let mut pending: HashMap<i64, (PendingTag, Instant)> = HashMap::new();
    let mut running = false;
    let mut held: VecDeque<(Outgoing, Option<PendingTag>)> = VecDeque::new();
    let mut shutdown: Option<oneshot::Sender<()>> = None;
    let mut cmds_open = true;
    let startup = tokio::time::sleep(spec.startup_timeout);
    tokio::pin!(startup);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let end = loop {
        tokio::select! {
            msg = incoming.recv() => match msg {
                Some(Incoming::Response { id: RequestId::Num(INITIALIZE_ID), result }) => {
                    let init = result.map_err(|e| e.to_string()).and_then(|v| serde_json::from_value::<InitializeResult>(v).map_err(|e| e.to_string()));
                    match init {
                        Ok(init) => {
                            running = true;
                            let _ = out.send(Outgoing::notification("initialized", json!({})));
                            for (msg, tag) in held.drain(..) {
                                if let (Outgoing::Request { id, .. }, Some(tag)) = (&msg, tag) {
                                    pending.insert(*id, (tag, Instant::now()));
                                }
                                let _ = out.send(msg);
                            }
                            send(LspMsg::Started { generation, server, caps: Box::new(init.capabilities) });
                        }
                        Err(e) => break End::Failed(format!("initialize: {e}")),
                    }
                }
                Some(Incoming::Response { id: RequestId::Num(SHUTDOWN_ID), .. }) => {
                    let _ = out.send(Outgoing::notification("exit", Value::Null));
                    // A process is waited for; an in-process server is done.
                    if child.is_none() {
                        break End::Stopped;
                    }
                }
                Some(Incoming::Response { id: RequestId::Num(id), result }) => match pending.remove(&id) {
                    Some((tag, _)) => send(LspMsg::Response { generation, server, tag, result }),
                    None => tracing::debug!("{}: answer to unknown request {id}", spec.name),
                },
                Some(Incoming::Response { id, .. }) => tracing::debug!("{}: answer to unknown request {id:?}", spec.name),
                Some(Incoming::Request { id, method, params }) => send(LspMsg::ServerRequest { generation, server, id, method, params }),
                Some(Incoming::Notification { method, params }) => send(LspMsg::Notification { generation, server, method, params }),
                None if shutdown.is_some() || !cmds_open => break End::Stopped,
                None => break End::Exited(match child.is_some() {
                    true => tokio::time::timeout(Duration::from_secs(1), wait_child(&mut child)).await.unwrap_or_else(|_| "output closed".into()),
                    false => "output closed".into(),
                }),
            },
            cmd = cmds.recv(), if cmds_open => match cmd {
                Some(Command::Send(msg, tag)) if running => {
                    if let (Outgoing::Request { id, .. }, Some(tag)) = (&msg, tag) {
                        pending.insert(*id, (tag, Instant::now()));
                    }
                    let _ = out.send(msg);
                }
                Some(Command::Send(msg, tag)) => {
                    if held.len() >= MAX_QUEUED {
                        let (old, _) = held.pop_front().expect("full");
                        tracing::warn!("{}: dropping {:?} queued before initialization", spec.name, old.method());
                    }
                    held.push_back((msg, tag));
                }
                Some(Command::Shutdown(done)) => {
                    shutdown = Some(done);
                    cmds_open = false;
                    if !running {
                        break End::Stopped;
                    }
                    let _ = out.send(Outgoing::Request { id: SHUTDOWN_ID, method: "shutdown".into(), params: Value::Null });
                }
                // The handle was dropped: the process group is killed.
                None => break End::Stopped,
            },
            status = wait_child(&mut child) => {
                child = None;
                break if shutdown.is_some() { End::Stopped } else { End::Exited(status) };
            }
            _ = &mut startup, if !running => {
                break End::Failed(format!("no answer to initialize in {} s", spec.startup_timeout.as_secs_f32()));
            }
            _ = tick.tick() => {
                let now = Instant::now();
                let late: Vec<i64> = pending.iter().filter(|(_, (_, at))| now - *at >= spec.request_timeout).map(|(id, _)| *id).collect();
                for id in late {
                    let Some((tag, _)) = pending.remove(&id) else { continue };
                    let error = ResponseError::new(-32803, format!("no answer in {} s", spec.request_timeout.as_secs_f32()));
                    send(LspMsg::Response { generation, server, tag, result: Err(error) });
                }
            }
        }
    };
    // The writer sends what is queued (`exit`), then closes stdin.
    drop(out);
    match end {
        End::Stopped => tracing::debug!("{} stopped", spec.name),
        End::Failed(error) => {
            tracing::warn!("{} failed: {error}", spec.name);
            send(LspMsg::Failed { generation, server, error });
        }
        End::Exited(status) => {
            tracing::warn!("{} exited: {status}", spec.name);
            send(LspMsg::Exited { generation, server, status });
        }
    }
    // Whatever the server left running goes with its group.
    kill_group(pid.swap(0, Ordering::SeqCst));
    if let Some(mut c) = child {
        let _ = c.wait().await;
    }
    if let Some(done) = shutdown {
        let _ = done.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::fake;
    use crate::lsp::transport::{read_frame, write_frame};
    use tokio::io::{BufReader, DuplexStream};

    const GEN: u64 = 3;
    const SERVER: ServerId = 7;

    fn spec() -> StartSpec {
        StartSpec {
            id: SERVER,
            name: "fake".into(),
            root: PathBuf::from("/tmp/jetl-client-test"),
            generation: GEN,
            init_options: Some(json!({"k": 1})),
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            watch_files: false,
        }
    }

    /// The next `LspMsg` on the bus (5 s at most); checks generation and server.
    async fn next_lsp(rx: &mut UnboundedReceiver<AppMsg>) -> LspMsg {
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("an LspMsg in time").expect("the bus is open");
        let AppMsg::Lsp(msg) = msg else { panic!("not an LspMsg") };
        let (generation, server) = match &msg {
            LspMsg::Started { generation, server, .. }
            | LspMsg::Failed { generation, server, .. }
            | LspMsg::Exited { generation, server, .. }
            | LspMsg::Notification { generation, server, .. }
            | LspMsg::ServerRequest { generation, server, .. }
            | LspMsg::Response { generation, server, .. } => (*generation, *server),
        };
        assert_eq!((generation, server), (GEN, SERVER), "{msg:?}");
        msg
    }

    /// A server the test drives frame by frame (the fake answers
    /// `initialize` by itself; this one does not).
    struct Manual {
        io: BufReader<DuplexStream>,
    }

    fn manual() -> (Conn, Manual) {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(client_end);
        (Conn { reader: Box::new(reader), writer: Box::new(writer), child: None }, Manual { io: BufReader::new(server_end) })
    }

    impl Manual {
        /// The next frame as JSON, `None` at end of stream (5 s at most).
        async fn read(&mut self) -> Option<Value> {
            let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.io))
                .await
                .expect("a frame or the end of the stream in time")
                .expect("readable frame");
            frame.map(|body| serde_json::from_slice(&body).expect("client sent JSON"))
        }

        async fn write(&mut self, v: Value) {
            write_frame(self.io.get_mut(), &v).await.expect("writable");
        }

        /// Answer `initialize` (the next frame) with empty capabilities.
        async fn answer_initialize(&mut self) {
            let init = self.read().await.expect("initialize");
            assert_eq!(init["method"], "initialize");
            self.write(json!({"jsonrpc": "2.0", "id": 0, "result": {"capabilities": {}}})).await;
        }
    }

    #[test]
    fn client_capabilities_offer_the_navigation_extras() {
        let c = serde_json::to_value(client_capabilities(false)).unwrap();
        assert_eq!(c["general"]["positionEncodings"], json!(["utf-16"]));
        for kind in ["definition", "typeDefinition", "implementation", "declaration"] {
            let cap = &c["textDocument"][kind];
            assert_eq!(cap["linkSupport"], json!(true), "{kind}");
            assert_eq!(cap["dynamicRegistration"], json!(true), "{kind}");
        }
        let help = &c["textDocument"]["signatureHelp"];
        let info = &help["signatureInformation"];
        assert_eq!(info["parameterInformation"]["labelOffsetSupport"], json!(true));
        assert_eq!(info["activeParameterSupport"], json!(true));
        assert_eq!(info["documentationFormat"], json!(["markdown", "plaintext"]));
        assert_eq!(help["contextSupport"], json!(true));
        assert_eq!(c["workspace"]["symbol"]["dynamicRegistration"], json!(true));
        assert_eq!(c["window"]["showMessage"]["messageActionItem"]["additionalPropertiesSupport"], json!(true));
        assert!(c["workspace"]["didChangeWatchedFiles"].is_null());
        let watching = serde_json::to_value(client_capabilities(true)).unwrap();
        assert_eq!(watching["workspace"]["didChangeWatchedFiles"]["relativePatternSupport"], json!(true));
    }

    #[tokio::test]
    async fn initialize_carries_root_options_and_client_info_then_initialized() {
        let (conn, mut server) = fake::connect(json!({}));
        let (bus, mut rx) = mpsc::unbounded_channel();
        let _client = Client::start(spec(), conn, bus);
        let init = server.recv("initialize").await;
        assert_eq!(init["id"], 0);
        let p = &init["params"];
        let root = serde_json::to_value(sync::uri(&spec().root)).unwrap();
        assert_eq!(p["processId"], json!(std::process::id()));
        assert_eq!(p["rootUri"], root);
        assert_eq!(p["workspaceFolders"], json!([{"uri": root, "name": "jetl-client-test"}]));
        assert_eq!(p["initializationOptions"], json!({"k": 1}));
        assert_eq!(p["clientInfo"], json!({"name": "jetl", "version": env!("CARGO_PKG_VERSION")}));
        assert_eq!(p["capabilities"], serde_json::to_value(client_capabilities(false)).unwrap());
        let next = server.next().await;
        assert_eq!(next["method"], "initialized");
        assert_eq!(next["params"], json!({}));
        assert!(matches!(next_lsp(&mut rx).await, LspMsg::Started { .. }));
    }

    #[tokio::test]
    async fn messages_wait_for_initialize_and_the_oldest_are_dropped_past_the_limit() {
        let (conn, mut server) = manual();
        let (bus, _rx) = mpsc::unbounded_channel();
        let mut client = Client::start(spec(), conn, bus);
        let id = client.request("test/held", json!({}), PendingTag::Initialize);
        assert_eq!(id, 1);
        for i in 0..64 {
            client.notify("test/n", json!({"i": i}));
        }
        let init = server.read().await.expect("initialize");
        assert_eq!((init["method"].as_str(), &init["id"]), (Some("initialize"), &json!(0)));
        server.write(json!({"jsonrpc": "2.0", "id": 0, "result": {"capabilities": {}}})).await;
        assert_eq!(server.read().await.expect("initialized")["method"], "initialized");
        for i in 0..64 {
            let msg = server.read().await.expect("a held notification");
            assert_eq!(msg["method"], "test/n", "frame {i}");
            assert_eq!(msg["params"]["i"], json!(i), "frame {i}");
        }
    }

    #[tokio::test]
    async fn a_held_request_is_answered_with_its_tag() {
        let (conn, mut server) = manual();
        let (bus, mut rx) = mpsc::unbounded_channel();
        let mut client = Client::start(spec(), conn, bus);
        let id = client.request("test/held", json!({}), PendingTag::Initialize);
        server.answer_initialize().await;
        assert_eq!(server.read().await.expect("initialized")["method"], "initialized");
        let req = server.read().await.expect("the held request");
        assert_eq!((req["id"].as_i64(), req["method"].as_str()), (Some(id), Some("test/held")));
        assert_eq!(id, 1);
        server.write(json!({"jsonrpc": "2.0", "id": 1, "result": 42})).await;
        assert!(matches!(next_lsp(&mut rx).await, LspMsg::Started { .. }));
        match next_lsp(&mut rx).await {
            LspMsg::Response { tag: PendingTag::Initialize, result, .. } => assert_eq!(result.unwrap(), json!(42)),
            other => panic!("expected the tagged response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn requests_count_from_one_and_cancel_sends_cancel_request() {
        let (conn, mut server) = fake::connect(json!({}));
        let (bus, mut rx) = mpsc::unbounded_channel();
        let mut client = Client::start(spec(), conn, bus);
        assert!(matches!(next_lsp(&mut rx).await, LspMsg::Started { .. }));
        let a = client.request("textDocument/hover", json!({}), PendingTag::Initialize);
        let b = client.request("textDocument/hover", json!({}), PendingTag::Initialize);
        assert_eq!((a, b), (1, 2));
        client.cancel(a);
        assert_eq!(server.recv("$/cancelRequest").await["params"], json!({"id": 1}));
        server.respond_error(1, ResponseError::REQUEST_CANCELLED, "cancelled");
        server.respond(2, json!("ok"));
        match next_lsp(&mut rx).await {
            LspMsg::Response { result: Err(e), .. } => assert_eq!(e.code, -32800),
            other => panic!("expected the cancelled error, got {other:?}"),
        }
        match next_lsp(&mut rx).await {
            LspMsg::Response { result: Ok(v), .. } => assert_eq!(v, json!("ok")),
            other => panic!("expected the answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn answers_to_unknown_ids_are_dropped_and_server_messages_forwarded() {
        let (conn, mut server) = fake::connect(json!({}));
        let (bus, mut rx) = mpsc::unbounded_channel();
        let client = Client::start(spec(), conn, bus);
        assert!(matches!(next_lsp(&mut rx).await, LspMsg::Started { .. }));
        server.respond(99, json!(1));
        server.notify("window/logMessage", json!({"type": 3, "message": "m"}));
        match next_lsp(&mut rx).await {
            LspMsg::Notification { method, .. } => assert_eq!(method, "window/logMessage"),
            other => panic!("expected the notification, got {other:?}"),
        }
        server.request(5, "workspace/configuration", json!({"items": []}));
        match next_lsp(&mut rx).await {
            LspMsg::ServerRequest { id, method, .. } => {
                assert_eq!(id, RequestId::Num(5));
                assert_eq!(method, "workspace/configuration");
            }
            other => panic!("expected the server request, got {other:?}"),
        }
        client.respond(RequestId::Num(5), Ok(json!([])));
        assert_eq!(server.response(5).await["result"], json!([]));
        client.respond(RequestId::Str("s".into()), Err(ResponseError::new(-32601, "nope")));
        let answer = server.next().await;
        assert_eq!(answer["id"], json!("s"));
        assert_eq!(answer["error"], json!({"code": -32601, "message": "nope"}));
    }

    #[tokio::test]
    async fn a_request_without_an_answer_times_out() {
        let (conn, _server) = fake::connect(json!({}));
        let (bus, mut rx) = mpsc::unbounded_channel();
        let mut client = Client::start(StartSpec { request_timeout: Duration::ZERO, ..spec() }, conn, bus);
        assert!(matches!(next_lsp(&mut rx).await, LspMsg::Started { .. }));
        client.request("test/slow", json!({}), PendingTag::Initialize);
        match next_lsp(&mut rx).await {
            LspMsg::Response { result: Err(e), .. } => {
                assert_eq!(e.code, -32803);
                assert_eq!(e.message, "no answer in 0 s");
            }
            other => panic!("expected the timeout error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn initialize_errors_and_silence_fail_the_server() {
        let (conn, mut server) = manual();
        let (bus, mut rx) = mpsc::unbounded_channel();
        let _client = Client::start(spec(), conn, bus);
        assert_eq!(server.read().await.expect("initialize")["method"], "initialize");
        server.write(json!({"jsonrpc": "2.0", "id": 0, "error": {"code": -32603, "message": "boom"}})).await;
        match next_lsp(&mut rx).await {
            LspMsg::Failed { error, .. } => assert_eq!(error, "initialize: boom (-32603)"),
            other => panic!("expected Failed, got {other:?}"),
        }

        let (conn, _silent) = manual();
        let (bus, mut rx) = mpsc::unbounded_channel();
        let _client = Client::start(StartSpec { startup_timeout: Duration::from_millis(20), ..spec() }, conn, bus);
        match next_lsp(&mut rx).await {
            LspMsg::Failed { error, .. } => assert_eq!(error, "no answer to initialize in 0.02 s"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closed_output_reports_exited() {
        let (conn, mut server) = manual();
        let (bus, mut rx) = mpsc::unbounded_channel();
        let _client = Client::start(spec(), conn, bus);
        server.answer_initialize().await;
        assert!(matches!(next_lsp(&mut rx).await, LspMsg::Started { .. }));
        drop(server);
        match next_lsp(&mut rx).await {
            LspMsg::Exited { status, .. } => assert_eq!(status, "output closed"),
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_asks_then_exits() {
        let (conn, mut server) = fake::connect(json!({}));
        let (bus, mut rx) = mpsc::unbounded_channel();
        let client = Client::start(spec(), conn, bus);
        assert!(matches!(next_lsp(&mut rx).await, LspMsg::Started { .. }));
        // Ends because the driver stopped, not through the 2 s SHUTDOWN_TIMEOUT.
        tokio::time::timeout(Duration::from_millis(1500), client.shutdown()).await.expect("shutdown ends before its timeout");
        assert_eq!(server.recv("shutdown").await["id"], json!(-1));
        server.recv("exit").await;
    }

    #[tokio::test]
    async fn shutdown_while_starting_sends_nothing_more() {
        let (conn, mut server) = manual();
        let (bus, _rx) = mpsc::unbounded_channel();
        let client = Client::start(spec(), conn, bus);
        client.notify("test/n", json!({}));
        assert_eq!(server.read().await.expect("initialize")["method"], "initialize");
        tokio::time::timeout(Duration::from_millis(1500), client.shutdown()).await.expect("shutdown ends before its timeout");
        assert_eq!(server.read().await, None, "no held notification and no shutdown request");
    }
}
