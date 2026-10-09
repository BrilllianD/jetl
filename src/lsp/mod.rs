//! Language servers. One [`client::Client`] per configured server and
//! project root, started when the first buffer of one of its languages
//! opens, speaking JSON-RPC over the server's stdin and stdout
//! ([`transport`]). Results come back to the app as [`LspMsg`], tagged with
//! the workspace generation and the server's id; `app/lsp.rs` routes them
//! and keeps open buffers in sync ([`sync`], positions in [`pos`]).
//! Dynamic registrations live in [`registry`], combined with the static
//! capabilities in [`caps`].

pub mod caps;
pub mod client;
pub mod completion;
pub mod diagnostics;
pub mod edits;
#[cfg(test)]
pub mod fake;
pub mod locations;
pub mod panel;
pub mod pos;
pub mod registry;
pub mod symbols;
pub mod sync;
pub mod transport;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use transport::{RequestId, ResponseError};

/// Identifies one started server process for the whole session; a
/// restarted server gets a new one, so messages of the old are dropped.
pub type ServerId = u64;

/// What a request was for, carried from the request to its response
/// (`LspMsg::Response`). Variants carry what the response is checked
/// against: the buffer, its version, the request's seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingTag {
    /// `initialize`, answered inside the client.
    Initialize,
    /// `shutdown`, answered inside the client.
    Shutdown,
    /// `textDocument/definition`.
    Definition(DocTag),
    /// `textDocument/references`.
    References(DocTag),
    /// `textDocument/hover`.
    Hover(DocTag),
    /// `textDocument/completion`.
    Completion(DocTag),
    /// `completionItem/resolve` of the item being accepted.
    CompletionResolve(DocTag),
    /// `completionItem/resolve` for the documentation panel of the selected item.
    CompletionDocs(DocTag),
    /// `textDocument/prepareRename`.
    PrepareRename(DocTag),
    /// `textDocument/rename`.
    Rename(DocTag),
    /// `textDocument/codeAction`.
    CodeAction(DocTag),
    /// `codeAction/resolve` of the chosen action.
    CodeActionResolve(DocTag),
    /// `workspace/executeCommand` of a chosen action.
    ExecuteCommand(DocTag),
    /// `textDocument/documentSymbol`.
    DocumentSymbols(DocTag),
    /// `textDocument/formatting`; checked against the format job of the
    /// buffer (`app/format.rs`), not the caret.
    Formatting(DocTag),
    /// `textDocument/codeAction` for the lightbulb; checked against
    /// `LspService::lightbulb` (`app/lightbulb.rs`), not `inflight`.
    Lightbulb(DocTag),
}

impl PendingTag {
    /// The document request's tag, for the requests the app makes.
    pub fn doc(&self) -> Option<&DocTag> {
        match self {
            PendingTag::Initialize | PendingTag::Shutdown => None,
            PendingTag::Definition(t)
            | PendingTag::References(t)
            | PendingTag::Hover(t)
            | PendingTag::Completion(t)
            | PendingTag::CompletionResolve(t)
            | PendingTag::CompletionDocs(t)
            | PendingTag::PrepareRename(t)
            | PendingTag::Rename(t)
            | PendingTag::CodeAction(t)
            | PendingTag::CodeActionResolve(t)
            | PendingTag::ExecuteCommand(t)
            | PendingTag::DocumentSymbols(t)
            | PendingTag::Formatting(t)
            | PendingTag::Lightbulb(t) => Some(t),
        }
    }
}

/// What a request about a position in a buffer is checked against when
/// its answer arrives: the buffer, its `Buffer::version` and the position
/// (UTF-16) when it was sent, and the request's seq (the newest request of
/// a kind wins; `app/lsp.rs` keeps it per method).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocTag {
    pub buffer: crate::editor::buffer::BufferId,
    pub version: u64,
    pub pos: lsp_types::Position,
    pub seq: u64,
}

/// What a workspace edit was for, carried until it is applied (the files
/// that are not open are edited off the main thread,
/// `AppMsg::EditsApplied`).
#[derive(Debug, Clone)]
pub enum EditOrigin {
    /// A rename to this name.
    Rename(String),
    /// The code action with this title.
    CodeAction(String),
    /// `workspace/applyEdit` from `server`, answered once applied.
    ServerRequest { server: String, id: RequestId, label: Option<String> },
}

/// A language server event for the app, tagged with the workspace
/// generation the server was started in and its id.
#[derive(Debug)]
pub enum LspMsg {
    /// `initialize` answered; the server takes documents and requests.
    Started {
        generation: u64,
        server: ServerId,
        caps: Box<lsp_types::ServerCapabilities>,
    },
    /// The server failed to initialize (an error or no answer in time);
    /// it was killed.
    Failed {
        generation: u64,
        server: ServerId,
        error: String,
    },
    /// The server's process ended (or its output closed) without being
    /// asked to.
    Exited {
        generation: u64,
        server: ServerId,
        status: String,
    },
    Notification {
        generation: u64,
        server: ServerId,
        method: String,
        params: Value,
    },
    /// A request from the server; the app must answer it
    /// (`Client::respond`).
    ServerRequest {
        generation: u64,
        server: ServerId,
        id: RequestId,
        method: String,
        params: Value,
    },
    /// The answer to a request the app made; an error also when the server
    /// took longer than `request_timeout_ms`.
    Response {
        generation: u64,
        server: ServerId,
        tag: PendingTag,
        result: Result<Value, ResponseError>,
    },
}

impl LspMsg {
    pub fn generation(&self) -> u64 {
        match self {
            LspMsg::Started { generation, .. }
            | LspMsg::Failed { generation, .. }
            | LspMsg::Exited { generation, .. }
            | LspMsg::Notification { generation, .. }
            | LspMsg::ServerRequest { generation, .. }
            | LspMsg::Response { generation, .. } => *generation,
        }
    }

    pub fn server(&self) -> ServerId {
        match self {
            LspMsg::Started { server, .. }
            | LspMsg::Failed { server, .. }
            | LspMsg::Exited { server, .. }
            | LspMsg::Notification { server, .. }
            | LspMsg::ServerRequest { server, .. }
            | LspMsg::Response { server, .. } => *server,
        }
    }
}

/// What panels ask of the language service (`Effect::Lsp`).
#[derive(Debug)]
pub enum LspRequest {
    /// Stop every server; they start again for the open buffers.
    Restart,
    /// Go to the definition of what is at the active buffer's caret (a
    /// Ctrl+click puts the caret on the clicked char first).
    Definition,
}

/// `[lsp]` in config.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    pub enabled: bool,
    /// A server that has not answered `initialize` by then is killed.
    pub startup_timeout_ms: u64,
    /// A request unanswered by then fails.
    pub request_timeout_ms: u64,
    /// Diagnostics kept per file (the most severe first).
    pub max_diagnostics_per_file: usize,
    /// Open the completion popup while typing (else only Ctrl+Space).
    pub completion_auto: bool,
    /// Identifier chars typed before the popup opens by itself (a server's
    /// trigger character such as `.` opens it at once).
    pub completion_min_chars: usize,
    /// `[lsp.servers.<name>]`: changes to the built-in servers, or new ones.
    pub servers: BTreeMap<String, ServerOverride>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            startup_timeout_ms: 15_000,
            request_timeout_ms: 5_000,
            max_diagnostics_per_file: 1000,
            completion_auto: true,
            completion_min_chars: 1,
            servers: BTreeMap::new(),
        }
    }
}

/// A `[lsp.servers.<name>]` table; omitted keys keep the built-in values.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerOverride {
    /// The command and its arguments; empty turns the server off.
    pub command: Option<Vec<String>>,
    /// jetl language ids (`LanguageDef::name`) the server is used for.
    pub languages: Option<Vec<String>>,
    /// `initializationOptions` of `initialize`.
    pub init_options: Option<Value>,
    /// Answers `workspace/configuration`, looked up by section.
    pub settings: Option<Value>,
}

/// A server as started: the built-in values with the user's changes.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub name: String,
    pub command: Vec<String>,
    pub languages: Vec<String>,
    pub init_options: Option<Value>,
    pub settings: Option<Value>,
}

/// Built-in servers: name, command, languages.
const BUILTIN: &[(&str, &[&str], &[&str])] = &[
    ("rust-analyzer", &["rust-analyzer"], &["rust"]),
    ("typescript-language-server", &["typescript-language-server", "--stdio"], &["javascript", "typescript", "tsx"]),
    ("pyright", &["pyright-langserver", "--stdio"], &["python"]),
    ("gopls", &["gopls"], &["go"]),
    ("clangd", &["clangd"], &["c", "cpp"]),
    ("lua-language-server", &["lua-language-server"], &["lua"]),
    ("bash-language-server", &["bash-language-server", "start"], &["bash"]),
];

impl LspConfig {
    /// Every server, user-defined ones first (by name), then the built-in
    /// ones with the user's changes; servers turned off are left out.
    pub fn servers(&self) -> Vec<ServerConfig> {
        let builtin = |name: &str| BUILTIN.iter().find(|b| b.0 == name);
        let user = self.servers.iter().filter(|(name, _)| builtin(name).is_none()).map(|(name, o)| (name.as_str(), o, None));
        let known = BUILTIN.iter().map(|b| (b.0, self.servers.get(b.0).unwrap_or(&NO_OVERRIDE), Some(b)));
        user.chain(known)
            .map(|(name, o, b)| ServerConfig {
                name: name.to_string(),
                command: o.command.clone().unwrap_or_else(|| b.map(|b| b.1.iter().map(|s| s.to_string()).collect()).unwrap_or_default()),
                languages: o
                    .languages
                    .clone()
                    .unwrap_or_else(|| b.map(|b| b.2.iter().map(|s| s.to_string()).collect()).unwrap_or_default()),
                init_options: o.init_options.clone(),
                settings: o.settings.clone(),
            })
            .filter(|s| s.command.first().is_some_and(|c| !c.is_empty()))
            .collect()
    }

    /// The server for a jetl language, if any.
    pub fn server_for(&self, language: &str) -> Option<ServerConfig> {
        self.servers().into_iter().find(|s| s.languages.iter().any(|l| l == language))
    }
}

static NO_OVERRIDE: ServerOverride = ServerOverride { command: None, languages: None, init_options: None, settings: None };

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn builtin_servers_and_overrides() {
        let cfg = Config::default().lsp;
        assert!(cfg.enabled);
        assert_eq!(cfg.server_for("rust").unwrap().command, ["rust-analyzer"]);
        assert_eq!(cfg.server_for("tsx").unwrap().name, "typescript-language-server");
        assert!(cfg.server_for("json").is_none());
        assert_eq!(cfg.max_diagnostics_per_file, 1000);
        assert!(cfg.completion_auto);
        assert_eq!(cfg.completion_min_chars, 1);
        let text = "[lsp]\nstartup_timeout_ms = 100\nmax_diagnostics_per_file = 50\ncompletion_min_chars = 3\n\
                    [lsp.servers.rust-analyzer]\nsettings = { rust-analyzer = { checkOnSave = false } }\n\
                    [lsp.servers.clangd]\ncommand = []\n\
                    [lsp.servers.pylsp]\ncommand = [\"pylsp\"]\nlanguages = [\"python\"]\n";
        let cfg = Config::parse(text).unwrap().lsp;
        assert_eq!((cfg.startup_timeout_ms, cfg.max_diagnostics_per_file, cfg.completion_min_chars), (100, 50, 3));
        let ra = cfg.server_for("rust").unwrap();
        assert_eq!(ra.command, ["rust-analyzer"], "a partial table keeps the built-in command");
        assert_eq!(ra.settings.unwrap()["rust-analyzer"]["checkOnSave"], false);
        assert!(cfg.server_for("cpp").is_none(), "an empty command turns the server off");
        assert_eq!(cfg.server_for("python").unwrap().name, "pylsp", "user servers come first");
        assert!(Config::parse("[lsp.servers.x]\ncommand = \"x\"\n").is_err(), "a command is a list of words");
    }
}
