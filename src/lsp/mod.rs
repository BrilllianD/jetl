//! Language servers. One [`client::Client`] per configured server and
//! project root, started when the first buffer of one of its languages
//! opens, speaking JSON-RPC over the server's stdin and stdout
//! ([`transport`]). Results come back to the app as [`LspMsg`], tagged with
//! the workspace generation and the server's id; `app/lsp.rs` routes them
//! and keeps open buffers in sync ([`sync`], positions in [`pos`]).
//! Dynamic registrations live in [`registry`], combined with the static
//! capabilities in [`caps`]. A server marked `extra` runs next to the
//! language's main server and only keeps documents in sync (spell checkers).

pub mod caps;
pub mod client;
pub mod completion;
pub mod diagnostics;
pub mod edits;
#[cfg(test)]
pub mod fake;
pub mod hierarchy;
pub mod locations;
pub mod panel;
pub mod pos;
pub mod registry;
pub mod semantic;
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
    /// `textDocument/typeDefinition`.
    TypeDefinition(DocTag),
    /// `textDocument/implementation`.
    Implementation(DocTag),
    /// `textDocument/declaration`.
    Declaration(DocTag),
    /// `textDocument/references`.
    References(DocTag),
    /// `textDocument/hover`.
    Hover(DocTag),
    /// `textDocument/definition` for the Quick Definition popup.
    QuickDefinition(DocTag),
    /// `textDocument/signatureHelp`.
    SignatureHelp(DocTag),
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
    /// `textDocument/codeAction` limited to refactorings (Refactor This).
    Refactor(DocTag),
    /// `textDocument/codeAction` limited to organize imports (Optimize Imports).
    OrganizeImports(DocTag),
    /// `textDocument/codeAction` limited to one refactoring (Extract Method, Inline, ...).
    RefactorKind(DocTag, Refactoring),
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
    /// `textDocument/documentHighlight` at the caret; checked against
    /// `LspService::doc_highlight` (`app/doc_highlight.rs`), not `inflight`.
    DocumentHighlight(DocTag),
    /// `textDocument/inlayHint` for the visible lines; checked against
    /// `LspService::inlay` (`app/inlay.rs`), not `inflight`.
    InlayHints(DocTag),
    /// `textDocument/semanticTokens/full` or `/range`; checked against `LspService::semantic` (`app/semantic.rs`), not `inflight`.
    SemanticTokens(DocTag),
    /// workspace/symbol for the palette query with this seq.
    WorkspaceSymbols(u64),
    /// textDocument/prepareCallHierarchy at the caret.
    CallHierarchy(DocTag),
    /// textDocument/prepareTypeHierarchy at the caret.
    TypeHierarchy(DocTag),
    /// The children of node `node` of the hierarchy tree `tree` (`App::hierarchy`); checked against the tree's id, not the caret.
    HierarchyChildren { tree: u64, node: usize },
    /// `workspace/will*Files` of a tree file operation; the seq of its `PendingFileOp`.
    WillFileOp(u64),
    /// workspace/willRenameFiles for the file renames of a server's rename answer; the seq of EditReqs::rename_files.
    RenameFileEdits(u64),
}

impl PendingTag {
    /// The document request's tag, for the requests the app makes.
    pub fn doc(&self) -> Option<&DocTag> {
        match self {
            PendingTag::Initialize
            | PendingTag::Shutdown
            | PendingTag::WillFileOp(_)
            | PendingTag::RenameFileEdits(_)
            | PendingTag::WorkspaceSymbols(_)
            | PendingTag::HierarchyChildren { .. } => None,
            PendingTag::Definition(t)
            | PendingTag::TypeDefinition(t)
            | PendingTag::Implementation(t)
            | PendingTag::Declaration(t)
            | PendingTag::References(t)
            | PendingTag::Hover(t)
            | PendingTag::QuickDefinition(t)
            | PendingTag::SignatureHelp(t)
            | PendingTag::Completion(t)
            | PendingTag::CompletionResolve(t)
            | PendingTag::CompletionDocs(t)
            | PendingTag::PrepareRename(t)
            | PendingTag::Rename(t)
            | PendingTag::CodeAction(t)
            | PendingTag::CodeActionResolve(t)
            | PendingTag::Refactor(t)
            | PendingTag::OrganizeImports(t)
            | PendingTag::RefactorKind(t, _)
            | PendingTag::ExecuteCommand(t)
            | PendingTag::DocumentSymbols(t)
            | PendingTag::Formatting(t)
            | PendingTag::Lightbulb(t)
            | PendingTag::DocumentHighlight(t)
            | PendingTag::InlayHints(t)
            | PendingTag::SemanticTokens(t)
            | PendingTag::CallHierarchy(t)
            | PendingTag::TypeHierarchy(t) => Some(t),
        }
    }
}

/// One of the refactoring keys (Extract Method, Inline, ...): the server's code
/// action of that kind at the caret or selection. jetl implements none of them;
/// `matches` picks the server's actions that fit, by kind and by title because
/// servers use kinds differently (clangd and rust-analyzer often send the bare
/// `refactor` or `refactor.extract`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refactoring {
    ExtractMethod,
    ExtractVariable,
    ExtractConstant,
    Inline,
    ChangeSignature,
    Move,
    SafeDelete,
}

/// Whether `kind` is `base` or below it (`base.` prefix).
fn under(kind: &str, base: &str) -> bool {
    kind == base || kind.strip_prefix(base).is_some_and(|rest| rest.starts_with('.'))
}

/// The lowercased words of `title`: runs of alphanumerics and `_`.
fn words(title: &str) -> Vec<String> {
    title.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

impl Refactoring {
    /// The kinds sent as `context.only`.
    pub fn only(self) -> &'static [&'static str] {
        match self {
            Refactoring::ExtractMethod
            | Refactoring::ExtractVariable
            | Refactoring::ExtractConstant
            | Refactoring::ChangeSignature
            | Refactoring::Move => &["refactor"],
            Refactoring::Inline => &["refactor.inline"],
            Refactoring::SafeDelete => &["refactor", "quickfix"],
        }
    }

    /// The menu label and the title of the choice dialog.
    pub fn title(self) -> &'static str {
        match self {
            Refactoring::ExtractMethod => "Extract Method",
            Refactoring::ExtractVariable => "Extract Variable",
            Refactoring::ExtractConstant => "Extract Constant",
            Refactoring::Inline => "Inline",
            Refactoring::ChangeSignature => "Change Signature",
            Refactoring::Move => "Move",
            Refactoring::SafeDelete => "Safe Delete",
        }
    }

    /// Whether the server's action of `kind` and `title` is this refactoring.
    pub fn matches(self, kind: &str, title: &str) -> bool {
        let w = words(title);
        let has = |names: &[&str]| w.iter().any(|x| names.contains(&x.as_str()));
        let first = w.first().map(String::as_str);
        let generic = kind == "refactor.extract" || kind == "refactor";
        let extract = |base: &str, word: &[&str]| under(kind, base) || (generic && first == Some("extract") && has(word));
        match self {
            Refactoring::ExtractMethod => extract("refactor.extract.function", &["function", "method"]),
            Refactoring::ExtractVariable => extract("refactor.extract.variable", &["variable"]),
            Refactoring::ExtractConstant => extract("refactor.extract.constant", &["constant"]),
            Refactoring::Inline => under(kind, "refactor.inline"),
            Refactoring::ChangeSignature => {
                (kind == "refactor" || under(kind, "refactor.rewrite")) && has(&["signature", "parameter", "parameters"])
            }
            Refactoring::Move => {
                let lower = title.to_lowercase();
                // Titles about parameters belong to Change Signature.
                !has(&["parameter", "parameters"])
                    && (under(kind, "refactor.move")
                        || (under(kind, "refactor") && first == Some("move"))
                        || (under(kind, "refactor.extract") && (lower.ends_with(" to file") || lower.ends_with(" to new file"))))
            }
            Refactoring::SafeDelete => {
                (under(kind, "refactor") || under(kind, "quickfix"))
                    && !has(&["all"])
                    && (first == Some("delete") || (first == Some("remove") && has(&["unused", "import", "imports", "include"])))
            }
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
    /// The editor group that was focused when it was sent.
    pub group: crate::editor::groups::GroupId,
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
    /// A server's edit before a tree file operation (`will*Files`); `title` names the
    /// operation and the server.
    FileOp { seq: u64, title: String },
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
    /// Show the server's inlay hints.
    pub inlay_hints: bool,
    /// Colour code by the server's semantic tokens.
    pub semantic_tokens: bool,
    /// Review a server's rename in the Find window before it is applied (else it applies at once).
    pub rename_preview: bool,
    /// Start language servers for scratch files too: separate clients rooted
    /// at the scratches directory (rust-analyzer gets them as
    /// `detachedFiles`).
    pub scratches: bool,
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
            inlay_hints: true,
            semantic_tokens: true,
            rename_preview: true,
            scratches: false,
            servers: BTreeMap::new(),
        }
    }
}

/// What a scratch client's name ends with (`rust-analyzer (scratches)`).
pub const SCRATCH_SUFFIX: &str = " (scratches)";

/// The client name of server `name` for scratch files.
pub fn scratch_client(name: &str) -> String {
    format!("{name}{SCRATCH_SUFFIX}")
}

/// The server a client name stands for (the scratch suffix removed).
pub fn server_name(client: &str) -> &str {
    client.strip_suffix(SCRATCH_SUFFIX).unwrap_or(client)
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
    /// Runs next to the language's main server: document sync, file notifications and diagnostics only; no requests.
    pub extra: bool,
}

/// A server as started: the built-in values with the user's changes.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub name: String,
    pub command: Vec<String>,
    pub languages: Vec<String>,
    pub init_options: Option<Value>,
    pub settings: Option<Value>,
    pub extra: bool,
}

/// The language entry that stands for every language (only on an `extra` server).
pub const ANY_LANGUAGE: &str = "*";

impl ServerConfig {
    /// Whether the server takes buffers of `language`: listed, or [`ANY_LANGUAGE`] on an extra server.
    pub fn serves(&self, language: &str) -> bool {
        self.languages.iter().any(|l| l == language || (self.extra && l == ANY_LANGUAGE))
    }
}

/// The first non-extra server of `language`.
pub fn main_server<'a>(servers: &'a [ServerConfig], language: &str) -> Option<&'a ServerConfig> {
    servers.iter().find(|s| !s.extra && s.serves(language))
}

/// Every extra server of `language`, in `servers()` order.
pub fn extra_servers<'a>(servers: &'a [ServerConfig], language: &'a str) -> impl Iterator<Item = &'a ServerConfig> + 'a {
    servers.iter().filter(move |s| s.extra && s.serves(language))
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
        self.all().into_iter().filter(|s| s.extra || !s.languages.iter().any(|l| l == ANY_LANGUAGE)).collect()
    }

    /// Names of the servers `servers()` leaves out because `languages = ["*"]` needs `extra = true`.
    pub fn misplaced_wildcards(&self) -> Vec<String> {
        self.all().into_iter().filter(|s| !s.extra && s.languages.iter().any(|l| l == ANY_LANGUAGE)).map(|s| s.name).collect()
    }

    fn all(&self) -> Vec<ServerConfig> {
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
                extra: o.extra,
            })
            .filter(|s| s.command.first().is_some_and(|c| !c.is_empty()))
            .collect()
    }

    /// The server for a jetl language, if any.
    pub fn server_for(&self, language: &str) -> Option<ServerConfig> {
        main_server(&self.servers(), language).cloned()
    }
}

static NO_OVERRIDE: ServerOverride = ServerOverride { command: None, languages: None, init_options: None, settings: None, extra: false };

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
        assert!(!cfg.scratches);
        assert!(cfg.rename_preview);
        assert_eq!(super::scratch_client("gopls"), "gopls (scratches)");
        assert_eq!((super::server_name("gopls (scratches)"), super::server_name("gopls")), ("gopls", "gopls"));
        let text = "[lsp]\nscratches = true\nstartup_timeout_ms = 100\nmax_diagnostics_per_file = 50\ncompletion_min_chars = 3\n\
                    [lsp.servers.rust-analyzer]\nsettings = { rust-analyzer = { checkOnSave = false } }\n\
                    [lsp.servers.clangd]\ncommand = []\n\
                    [lsp.servers.pylsp]\ncommand = [\"pylsp\"]\nlanguages = [\"python\"]\n";
        let cfg = Config::parse(text).unwrap().lsp;
        assert_eq!((cfg.startup_timeout_ms, cfg.max_diagnostics_per_file, cfg.completion_min_chars), (100, 50, 3));
        assert!(cfg.scratches);
        let ra = cfg.server_for("rust").unwrap();
        assert_eq!(ra.command, ["rust-analyzer"], "a partial table keeps the built-in command");
        assert_eq!(ra.settings.unwrap()["rust-analyzer"]["checkOnSave"], false);
        assert!(cfg.server_for("cpp").is_none(), "an empty command turns the server off");
        assert_eq!(cfg.server_for("python").unwrap().name, "pylsp", "user servers come first");
        assert!(Config::parse("[lsp.servers.x]\ncommand = \"x\"\n").is_err(), "a command is a list of words");
    }

    #[test]
    fn extra_servers_and_wildcards() {
        let text = "[lsp.servers.typos]\ncommand = [\"typos-lsp\"]\nlanguages = [\"*\"]\nextra = true\n\
                    [lsp.servers.spell-md]\ncommand = [\"x\"]\nlanguages = [\"markdown\"]\nextra = true\n\
                    [lsp.servers.bad]\ncommand = [\"bad\"]\nlanguages = [\"*\"]\n";
        let cfg = Config::parse(text).unwrap().lsp;
        assert_eq!(cfg.server_for("rust").unwrap().name, "rust-analyzer", "extras are never the main server");
        let servers = cfg.servers();
        assert!(super::main_server(&servers, "markdown").is_none());
        let names = |l: &'static str| super::extra_servers(&servers, l).map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("rust"), ["typos"]);
        assert_eq!(names("markdown"), ["spell-md", "typos"]);
        assert!(servers.iter().all(|s| s.name != "bad"), "a main server with the wildcard is dropped");
        assert_eq!(cfg.misplaced_wildcards(), ["bad"]);
        assert!(Config::default().lsp.servers().iter().all(|s| !s.extra));
        let plain = super::ServerConfig {
            name: "p".into(),
            command: vec!["p".into()],
            languages: vec!["*".into()],
            init_options: None,
            settings: None,
            extra: false,
        };
        assert!(!plain.serves("rust"), "the wildcard needs extra");
        assert!(super::ServerConfig { extra: true, ..plain }.serves("rust"));
    }

    #[test]
    fn refactoring_table_on_probe_titles() {
        use super::Refactoring::*;
        let yes: &[(super::Refactoring, &str, &str)] = &[
            (ExtractMethod, "refactor.extract", "Extract into function"),
            (ExtractMethod, "refactor.extract.function", "Extract to function in module scope"),
            (ExtractMethod, "refactor.extract.function", "Extract function"),
            (ExtractVariable, "refactor.extract", "Extract into variable"),
            (ExtractVariable, "refactor.extract.variable", "Extract variable"),
            (ExtractVariable, "refactor", "Extract subexpression to variable"),
            (ExtractConstant, "refactor.extract", "Extract into constant"),
            (ExtractConstant, "refactor.extract.constant", "Extract to constant in enclosing scope"),
            (Inline, "refactor.inline", "Inline variable"),
            (Inline, "refactor.inline.variable", "Inline variable"),
            (Inline, "refactor.inline.call", "Inline call to add"),
            (ChangeSignature, "refactor", "Remove unused parameter"),
            (ChangeSignature, "refactor.rewrite.moveParamLeft", "Move parameter left"),
            (ChangeSignature, "refactor.rewrite.parameters.toDestructured", "Convert parameters to destructured object"),
            (Move, "refactor.move.newFile", "Move to a new file"),
            (Move, "refactor.extract", "Extract module to file"),
            (Move, "refactor.extract.toNewFile", "Extract declarations to new file"),
            (Move, "refactor.rewrite", "Move guard to arm body"),
            (Move, "refactor.rewrite", "Move to where clause"),
            (SafeDelete, "quickfix", "Delete function \"deadCode\""),
            (SafeDelete, "quickfix", "Remove unused declaration for: 'deadCode'"),
            (SafeDelete, "refactor", "Remove unused parameter"),
            (SafeDelete, "quickfix", "remove #include directive"),
            (SafeDelete, "quickfix", "Delete function \"install\""),
        ];
        for (r, kind, title) in yes {
            assert!(r.matches(kind, title), "{r:?} should match {kind:?} {title:?}");
        }
        let no: &[(super::Refactoring, &str, &str)] = &[
            (Move, "refactor", "Remove unused parameter"),
            (Move, "refactor.rewrite.moveParamLeft", "Move parameter left"),
            (Move, "refactor.inline.call", "Inline call to move_it"),
            (Move, "refactor.extract", "Extract Module"),
            (ExtractConstant, "refactor", "Promote local to constant"),
            (ExtractConstant, "refactor.extract", "Extract into static"),
            (ExtractConstant, "refactor.extract.type", "Extract to type alias"),
            (ExtractMethod, "refactor.extract", "Extract module to file"),
            (ExtractMethod, "refactor.rewrite", "Generate a type alias for function with named params"),
            (ExtractMethod, "refactor.extract.functional", "Extract function"),
            (ExtractVariable, "refactor.extract.constant", "Extract to constant in enclosing scope"),
            (ChangeSignature, "", "Generate a type alias for function with named params"),
            (ChangeSignature, "quickfix", "Remove unused parameter \"unused\""),
            (Inline, "refactoring", "Inline variable"),
            (Inline, "refactor.rewrite", "Inline variable"),
            (SafeDelete, "quickfix", "Delete all unused declarations"),
            (SafeDelete, "quickfix", "Remove all unused imports"),
            (SafeDelete, "refactor", "Remove redundant parentheses"),
            (SafeDelete, "quickfix", "Remove dbg!()"),
            (SafeDelete, "refactor.rewrite.arrow.braces.remove", "Remove braces from arrow function"),
            (SafeDelete, "quickfix", "Bind as `let _ = unused;`"),
            (SafeDelete, "source", "Delete function x"),
        ];
        for (r, kind, title) in no {
            assert!(!r.matches(kind, title), "{r:?} should not match {kind:?} {title:?}");
        }
    }

    #[test]
    fn refactoring_only_and_titles() {
        use super::Refactoring::*;
        let rows: &[(super::Refactoring, &[&str], &str)] = &[
            (ExtractMethod, &["refactor"], "Extract Method"),
            (ExtractVariable, &["refactor"], "Extract Variable"),
            (ExtractConstant, &["refactor"], "Extract Constant"),
            (Inline, &["refactor.inline"], "Inline"),
            (ChangeSignature, &["refactor"], "Change Signature"),
            (Move, &["refactor"], "Move"),
            (SafeDelete, &["refactor", "quickfix"], "Safe Delete"),
        ];
        for (r, only, title) in rows {
            assert_eq!((r.only(), r.title()), (*only, *title));
        }
    }
}
