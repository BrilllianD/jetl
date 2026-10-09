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
    ClientCapabilities, ClientInfo, CodeActionCapabilityResolveSupport, CodeActionClientCapabilities, CodeActionKindLiteralSupport,
    CodeActionLiteralSupport, CompletionClientCapabilities, CompletionItemCapability, CompletionItemCapabilityResolveSupport,
    CompletionItemKind, CompletionItemKindCapability, DocumentFormattingClientCapabilities, DocumentSymbolClientCapabilities,
    ExecuteCommandClientCapabilities, FailureHandlingKind, GeneralClientCapabilities, GotoCapability, HoverClientCapabilities,
    InitializeParams, InitializeResult, MarkupKind, PositionEncodingKind, PublishDiagnosticsClientCapabilities,
    ReferenceClientCapabilities, RenameClientCapabilities, ResourceOperationKind, ShowMessageRequestClientCapabilities,
    TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities, WindowClientCapabilities, WorkspaceClientCapabilities,
    WorkspaceEditClientCapabilities, WorkspaceFolder,
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
        let (id, name) = (spec.id, spec.name.clone());
        tokio::spawn(drive(spec, conn, rx, bus, Arc::clone(&pid)));
        Self { id, name, state: State::Starting, next_id: 1, cmds, pid, registry: Default::default() }
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
/// dynamic registration where `Caps::has` honours it (`lsp::caps`).
pub fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        general: Some(GeneralClientCapabilities { position_encodings: Some(vec![PositionEncodingKind::UTF16]), ..Default::default() }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities { did_save: Some(true), ..Default::default() }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities::default()),
            definition: Some(GotoCapability { dynamic_registration: Some(true), link_support: Some(true) }),
            references: Some(ReferenceClientCapabilities { dynamic_registration: Some(true) }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(true),
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            }),
            completion: Some(completion_capabilities()),
            rename: Some(RenameClientCapabilities { dynamic_registration: Some(true), prepare_support: Some(true), ..Default::default() }),
            code_action: Some(code_action_capabilities()),
            document_symbol: Some(DocumentSymbolClientCapabilities {
                dynamic_registration: Some(true),
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            formatting: Some(DocumentFormattingClientCapabilities { dynamic_registration: Some(true) }),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            show_message: Some(ShowMessageRequestClientCapabilities::default()),
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
                ..Default::default()
            }),
            execute_command: Some(ExecuteCommandClientCapabilities { dynamic_registration: Some(true) }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Completion: snippets (flattened on accept), `textEdit` and
/// `additionalTextEdits`, resolved lazily when accepting; every item kind.
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
        resolve_support: Some(CodeActionCapabilityResolveSupport { properties: vec!["edit".into()] }),
        ..Default::default()
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
        capabilities: client_capabilities(),
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
