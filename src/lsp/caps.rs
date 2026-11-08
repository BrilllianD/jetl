//! What a running server offers for one document: the capabilities of its
//! `initialize` answer combined with the dynamic registrations
//! ([`super::registry`]) whose document selector matches the document.
//!
//! The two combine as a union: a feature exists if either has it, a boolean
//! sub-option is true if any source sets it, and lists (trigger characters,
//! commands) are joined. Nothing is cached: callers ask when they decide.

use std::path::Path;

use lsp_types::{
    CallHierarchyServerCapability, CodeActionProviderCapability, DeclarationCapability, FileOperationFilter, FileOperationPatternKind,
    FileOperationRegistrationOptions, HoverProviderCapability, ImplementationProviderCapability, OneOf, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentSyncKind,
    TypeDefinitionProviderCapability,
};
use serde_json::Value;

use super::registry::{DocRef, Glob, Registry};
use super::sync::SyncCaps;

pub const DEFINITION: &str = "textDocument/definition";
pub const TYPE_DEFINITION: &str = "textDocument/typeDefinition";
pub const IMPLEMENTATION: &str = "textDocument/implementation";
pub const DECLARATION: &str = "textDocument/declaration";
pub const WORKSPACE_SYMBOL: &str = "workspace/symbol";
pub const REFERENCES: &str = "textDocument/references";
pub const HOVER: &str = "textDocument/hover";
pub const SIGNATURE_HELP: &str = "textDocument/signatureHelp";
pub const COMPLETION: &str = "textDocument/completion";
pub const RESOLVE: &str = "completionItem/resolve";
pub const PREPARE_RENAME: &str = "textDocument/prepareRename";
pub const RENAME: &str = "textDocument/rename";
pub const CODE_ACTION: &str = "textDocument/codeAction";
pub const CODE_ACTION_RESOLVE: &str = "codeAction/resolve";
pub const INLAY_HINT: &str = "textDocument/inlayHint";
pub const DOCUMENT_HIGHLIGHT: &str = "textDocument/documentHighlight";
/// The dynamic-registration method of semantic tokens; never sent as a request.
pub const SEMANTIC_TOKENS: &str = "textDocument/semanticTokens";
pub const SEMANTIC_TOKENS_FULL: &str = "textDocument/semanticTokens/full";
pub const SEMANTIC_TOKENS_RANGE: &str = "textDocument/semanticTokens/range";
pub const EXECUTE_COMMAND: &str = "workspace/executeCommand";
pub const DOCUMENT_SYMBOL: &str = "textDocument/documentSymbol";
pub const PREPARE_CALL_HIERARCHY: &str = "textDocument/prepareCallHierarchy";
pub const INCOMING_CALLS: &str = "callHierarchy/incomingCalls";
pub const OUTGOING_CALLS: &str = "callHierarchy/outgoingCalls";
pub const PREPARE_TYPE_HIERARCHY: &str = "textDocument/prepareTypeHierarchy";
pub const SUPERTYPES: &str = "typeHierarchy/supertypes";
pub const SUBTYPES: &str = "typeHierarchy/subtypes";
pub const FORMATTING: &str = "textDocument/formatting";
pub const RANGE_FORMATTING: &str = "textDocument/rangeFormatting";
pub const WILL_CREATE_FILES: &str = "workspace/willCreateFiles";
pub const DID_CREATE_FILES: &str = "workspace/didCreateFiles";
pub const WILL_RENAME_FILES: &str = "workspace/willRenameFiles";
pub const DID_RENAME_FILES: &str = "workspace/didRenameFiles";
pub const WILL_DELETE_FILES: &str = "workspace/willDeleteFiles";
pub const DID_DELETE_FILES: &str = "workspace/didDeleteFiles";
pub const DID_OPEN: &str = "textDocument/didOpen";
pub const DID_CHANGE: &str = "textDocument/didChange";
pub const DID_SAVE: &str = "textDocument/didSave";
pub const DID_CLOSE: &str = "textDocument/didClose";
pub const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";

/// What a running server offers for one document: its `initialize`
/// capabilities plus the registrations that apply to the document.
#[derive(Clone, Copy)]
pub struct Caps<'a> {
    pub fixed: &'a ServerCapabilities,
    pub registry: &'a Registry,
    pub doc: Option<DocRef<'a>>,
}

fn on<T>(p: &Option<OneOf<bool, T>>) -> bool {
    matches!(p, Some(OneOf::Left(true) | OneOf::Right(_)))
}

/// Whether registration options set `key` to true.
fn flag(o: &Value, key: &str) -> bool {
    o.get(key).and_then(Value::as_bool) == Some(true)
}

/// Whether `filter` accepts the absolute `path` (a folder when `is_dir`).
fn filter_matches(filter: &FileOperationFilter, ignore_case: bool, path: &Path, is_dir: bool) -> bool {
    let p = &filter.pattern;
    filter.scheme.as_deref().is_none_or(|s| s == "file")
        && match p.matches {
            None => true,
            Some(FileOperationPatternKind::File) => !is_dir,
            Some(FileOperationPatternKind::Folder) => is_dir,
        }
        && Glob::new(&p.glob, ignore_case || p.options.as_ref().and_then(|o| o.ignore_case) == Some(true)).is_some_and(|g| g.is_match(path))
}

/// The strings of the array `o[key]`.
fn strings<'v>(o: &'v Value, key: &str) -> impl Iterator<Item = &'v str> + use<'v> {
    o.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str)
}

/// `items` without repeats, in order.
fn dedup<'v>(items: impl Iterator<Item = &'v str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in items {
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

impl<'a> Caps<'a> {
    /// `registerOptions` of `method`'s registrations that apply to the document.
    pub fn dynamic(&self, method: &'a str) -> impl Iterator<Item = &'a Value> + use<'a> {
        self.registry.matching(method, self.doc).map(|r| &r.options)
    }

    /// Whether the server answers `method` (static OR any applicable registration).
    pub fn has(&self, method: &str) -> bool {
        let fixed = self.fixed;
        match method {
            DEFINITION => on(&fixed.definition_provider) || self.any(DEFINITION),
            TYPE_DEFINITION => {
                matches!(
                    fixed.type_definition_provider,
                    Some(TypeDefinitionProviderCapability::Simple(true) | TypeDefinitionProviderCapability::Options(_))
                ) || self.any(TYPE_DEFINITION)
            }
            IMPLEMENTATION => {
                matches!(
                    fixed.implementation_provider,
                    Some(ImplementationProviderCapability::Simple(true) | ImplementationProviderCapability::Options(_))
                ) || self.any(IMPLEMENTATION)
            }
            DECLARATION => {
                matches!(
                    fixed.declaration_provider,
                    Some(
                        DeclarationCapability::Simple(true)
                            | DeclarationCapability::RegistrationOptions(_)
                            | DeclarationCapability::Options(_)
                    )
                ) || self.any(DECLARATION)
            }
            WORKSPACE_SYMBOL => on(&fixed.workspace_symbol_provider) || self.any(WORKSPACE_SYMBOL),
            REFERENCES => on(&fixed.references_provider) || self.any(REFERENCES),
            HOVER => {
                matches!(fixed.hover_provider, Some(HoverProviderCapability::Simple(true) | HoverProviderCapability::Options(_)))
                    || self.any(HOVER)
            }
            SIGNATURE_HELP => fixed.signature_help_provider.is_some() || self.any(SIGNATURE_HELP),
            COMPLETION => fixed.completion_provider.is_some() || self.any(COMPLETION),
            RESOLVE => {
                fixed.completion_provider.as_ref().and_then(|c| c.resolve_provider) == Some(true)
                    || self.dynamic(COMPLETION).any(|o| flag(o, "resolveProvider"))
            }
            PREPARE_RENAME => {
                matches!(&fixed.rename_provider, Some(OneOf::Right(o)) if o.prepare_provider == Some(true))
                    || self.dynamic(RENAME).any(|o| flag(o, "prepareProvider"))
            }
            RENAME => on(&fixed.rename_provider) || self.any(RENAME),
            CODE_ACTION => {
                matches!(
                    fixed.code_action_provider,
                    Some(CodeActionProviderCapability::Simple(true) | CodeActionProviderCapability::Options(_))
                ) || self.any(CODE_ACTION)
            }
            CODE_ACTION_RESOLVE => {
                matches!(&fixed.code_action_provider, Some(CodeActionProviderCapability::Options(o)) if o.resolve_provider == Some(true))
                    || self.dynamic(CODE_ACTION).any(|o| flag(o, "resolveProvider"))
            }
            EXECUTE_COMMAND => fixed.execute_command_provider.is_some() || self.any(EXECUTE_COMMAND),
            DOCUMENT_SYMBOL => on(&fixed.document_symbol_provider) || self.any(DOCUMENT_SYMBOL),
            FORMATTING => on(&fixed.document_formatting_provider) || self.any(FORMATTING),
            RANGE_FORMATTING => on(&fixed.document_range_formatting_provider) || self.any(RANGE_FORMATTING),
            INLAY_HINT => on(&fixed.inlay_hint_provider) || self.any(INLAY_HINT),
            DOCUMENT_HIGHLIGHT => on(&fixed.document_highlight_provider) || self.any(DOCUMENT_HIGHLIGHT),
            SEMANTIC_TOKENS_FULL => {
                matches!(
                    self.semantic_options().and_then(|o| o.full.as_ref()),
                    Some(SemanticTokensFullOptions::Bool(true) | SemanticTokensFullOptions::Delta { .. })
                ) || self.dynamic(SEMANTIC_TOKENS).any(|o| flag(o, "full") || o.get("full").is_some_and(Value::is_object))
            }
            SEMANTIC_TOKENS_RANGE => {
                self.semantic_options().is_some_and(|o| o.range == Some(true)) || self.dynamic(SEMANTIC_TOKENS).any(|o| flag(o, "range"))
            }
            PREPARE_CALL_HIERARCHY => {
                matches!(
                    fixed.call_hierarchy_provider,
                    Some(CallHierarchyServerCapability::Simple(true) | CallHierarchyServerCapability::Options(_))
                ) || self.any(PREPARE_CALL_HIERARCHY)
            }
            // lsp-types 0.97 drops typeHierarchyProvider: any running server is asked, a -32601 answer says it has none.
            PREPARE_TYPE_HIERARCHY => true,
            _ => false,
        }
    }

    /// The static semantic tokens options, whichever variant carries them.
    fn semantic_options(&self) -> Option<&'a SemanticTokensOptions> {
        match self.fixed.semantic_tokens_provider.as_ref()? {
            SemanticTokensServerCapabilities::SemanticTokensOptions(o) => Some(o),
            SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(r) => Some(&r.semantic_tokens_options),
        }
    }

    /// The semantic tokens legend: the static one, else the first applicable
    /// registration's. `None` when neither has a readable legend.
    pub fn semantic_legend(&self) -> Option<SemanticTokensLegend> {
        if let Some(o) = self.semantic_options() {
            return Some(o.legend.clone());
        }
        self.dynamic(SEMANTIC_TOKENS).next().and_then(|o| serde_json::from_value(o.get("legend")?.clone()).ok())
    }

    /// Whether any registration of `method` applies.
    fn any(&self, method: &'a str) -> bool {
        self.dynamic(method).next().is_some()
    }

    /// Completion trigger characters: static ones, then registered ones, deduplicated in order.
    pub fn completion_triggers(&self) -> Vec<String> {
        let fixed = self.fixed.completion_provider.as_ref().and_then(|c| c.trigger_characters.as_ref()).into_iter().flatten();
        let registered = self.dynamic(COMPLETION).flat_map(|o| strings(o, "triggerCharacters"));
        dedup(fixed.map(String::as_str).chain(registered))
    }

    /// Signature help trigger characters: static ones, then registered ones, deduplicated in order.
    pub fn signature_triggers(&self) -> Vec<String> {
        let fixed = self.fixed.signature_help_provider.as_ref().and_then(|c| c.trigger_characters.as_ref()).into_iter().flatten();
        let registered = self.dynamic(SIGNATURE_HELP).flat_map(|o| strings(o, "triggerCharacters"));
        dedup(fixed.map(String::as_str).chain(registered))
    }

    /// Signature help retrigger characters: static ones, then registered ones, deduplicated in order.
    pub fn signature_retriggers(&self) -> Vec<String> {
        let fixed = self.fixed.signature_help_provider.as_ref().and_then(|c| c.retrigger_characters.as_ref()).into_iter().flatten();
        let registered = self.dynamic(SIGNATURE_HELP).flat_map(|o| strings(o, "retriggerCharacters"));
        dedup(fixed.map(String::as_str).chain(registered))
    }

    /// Completion commit characters: static `allCommitCharacters`, then registered ones, deduplicated in order.
    pub fn completion_commit_chars(&self) -> Vec<String> {
        let fixed = self.fixed.completion_provider.as_ref().and_then(|c| c.all_commit_characters.as_ref()).into_iter().flatten();
        let registered = self.dynamic(COMPLETION).flat_map(|o| strings(o, "allCommitCharacters"));
        dedup(fixed.map(String::as_str).chain(registered))
    }

    /// Code action kinds the server says it returns: `None` means any kind (static
    /// `Simple(true)`, Options without `code_action_kinds`, or any applicable registration
    /// without `codeActionKinds`); otherwise the union, deduplicated in order.
    pub fn code_action_kinds(&self) -> Option<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |k: &str| {
            if !out.iter().any(|o| o == k) {
                out.push(k.to_string());
            }
        };
        match &self.fixed.code_action_provider {
            Some(CodeActionProviderCapability::Simple(true)) => return None,
            Some(CodeActionProviderCapability::Options(o)) => o.code_action_kinds.as_ref()?.iter().for_each(|k| push(k.as_str())),
            Some(CodeActionProviderCapability::Simple(false)) | None => {}
        }
        for o in self.dynamic(CODE_ACTION) {
            // Without a list it may return any kind.
            o.get("codeActionKinds").and_then(Value::as_array)?;
            strings(o, "codeActionKinds").for_each(&mut push);
        }
        if out.is_empty() { None } else { Some(out) }
    }

    /// Whether `method` (`workspace/will*Files` or `did*Files`) is wanted for `path`: static
    /// `workspace.fileOperations` filters or any registration's `filters` match it.
    pub fn file_operation(&self, method: &'a str, path: &Path, is_dir: bool) -> bool {
        let ops = self.fixed.workspace.as_ref().and_then(|w| w.file_operations.as_ref());
        let fixed = ops.and_then(|o| match method {
            WILL_CREATE_FILES => o.will_create.as_ref(),
            DID_CREATE_FILES => o.did_create.as_ref(),
            WILL_RENAME_FILES => o.will_rename.as_ref(),
            DID_RENAME_FILES => o.did_rename.as_ref(),
            WILL_DELETE_FILES => o.will_delete.as_ref(),
            DID_DELETE_FILES => o.did_delete.as_ref(),
            _ => None,
        });
        let matches = |o: &FileOperationRegistrationOptions| o.filters.iter().any(|f| filter_matches(f, false, path, is_dir));
        fixed.is_some_and(matches)
            || self
                .dynamic(method)
                .filter_map(|o| serde_json::from_value::<FileOperationRegistrationOptions>(o.clone()).ok())
                .any(|o| matches(&o))
    }

    /// How the document is synced: the static sync capability, plus the applicable
    /// `didOpen`/`didClose`/`didChange`/`didSave` registrations (the last `didChange` kind wins).
    pub fn sync(&self) -> SyncCaps {
        let mut s = SyncCaps::of(self.fixed);
        s.open |= self.any(DID_OPEN);
        s.close |= self.any(DID_CLOSE);
        let kind = self.dynamic(DID_CHANGE).filter_map(|o| match o.get("syncKind").and_then(Value::as_u64) {
            Some(0) => Some(TextDocumentSyncKind::NONE),
            Some(1) => Some(TextDocumentSyncKind::FULL),
            Some(2) => Some(TextDocumentSyncKind::INCREMENTAL),
            _ => None,
        });
        if let Some(k) = kind.last() {
            s.change = k;
        }
        if self.any(DID_SAVE) {
            s.save = Some(s.save == Some(true) || self.dynamic(DID_SAVE).any(|o| flag(o, "includeText")));
        }
        s
    }

    /// Whether `workspace/executeCommand` runs `command` (static `commands` ∪ registered `commands`).
    pub fn runs_command(&self, command: &str) -> bool {
        self.fixed.execute_command_provider.as_ref().is_some_and(|p| p.commands.iter().any(|c| c == command))
            || self.dynamic(EXECUTE_COMMAND).any(|o| strings(o, "commands").any(|c| c == command))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::lsp::client::client_capabilities;

    fn rust() -> DocRef<'static> {
        DocRef { language_id: "rust", path: Path::new("/p/src/a.rs") }
    }
    fn python() -> DocRef<'static> {
        DocRef { language_id: "python", path: Path::new("/p/a.py") }
    }

    fn fixed(v: Value) -> ServerCapabilities {
        serde_json::from_value(v).unwrap()
    }

    fn caps<'a>(fixed: &'a ServerCapabilities, registry: &'a Registry, doc: DocRef<'a>) -> Caps<'a> {
        Caps { fixed, registry, doc: Some(doc) }
    }

    fn register(r: &mut Registry, id: &str, method: &str, options: Value) {
        r.register(&json!({"registrations": [{"id": id, "method": method, "registerOptions": options}]})).unwrap();
    }

    #[test]
    fn static_capabilities_and_registrations_combine() {
        let f = fixed(json!({"definitionProvider": true}));
        let mut r = Registry::default();
        let c = caps(&f, &r, rust());
        assert!(c.has(DEFINITION));
        assert!(!c.has(FORMATTING) && !c.has(RESOLVE) && !c.has("x/y"));
        register(&mut r, "fmt", FORMATTING, json!({"documentSelector": [{"language": "rust"}]}));
        assert!(caps(&f, &r, rust()).has(FORMATTING));
        assert!(!caps(&f, &r, python()).has(FORMATTING));
    }

    #[test]
    fn workspace_symbol_is_static_or_registered() {
        let none = Registry::default();
        for v in [json!({"workspaceSymbolProvider": true}), json!({"workspaceSymbolProvider": {}})] {
            let f = fixed(v);
            let c = Caps { fixed: &f, registry: &none, doc: None };
            assert!(c.has(WORKSPACE_SYMBOL));
        }
        let f = fixed(json!({}));
        assert!(!Caps { fixed: &f, registry: &none, doc: None }.has(WORKSPACE_SYMBOL));
        let mut r = Registry::default();
        register(&mut r, "w", WORKSPACE_SYMBOL, json!({}));
        assert!(Caps { fixed: &f, registry: &r, doc: None }.has(WORKSPACE_SYMBOL));
    }

    #[test]
    fn goto_variants_are_static_or_registered() {
        let f = fixed(json!({"typeDefinitionProvider": true, "implementationProvider": {}}));
        let mut r = Registry::default();
        let c = caps(&f, &r, rust());
        assert!(c.has(TYPE_DEFINITION) && c.has(IMPLEMENTATION) && !c.has(DECLARATION));
        register(&mut r, "d", DECLARATION, json!({"documentSelector": [{"language": "rust"}]}));
        assert!(caps(&f, &r, rust()).has(DECLARATION));
        assert!(!caps(&f, &r, python()).has(DECLARATION));
        let f = fixed(json!({"typeDefinitionProvider": false}));
        assert!(!caps(&f, &Registry::default(), rust()).has(TYPE_DEFINITION));
    }

    #[test]
    fn call_hierarchy_is_static_or_registered_and_type_hierarchy_is_always_asked() {
        let r = Registry::default();
        for (v, want) in [
            (json!({"callHierarchyProvider": true}), true),
            (json!({"callHierarchyProvider": {}}), true),
            (json!({}), false),
            (json!({"callHierarchyProvider": false}), false),
        ] {
            let f = fixed(v.clone());
            assert_eq!(caps(&f, &r, rust()).has(PREPARE_CALL_HIERARCHY), want, "{v}");
        }
        let f = fixed(json!({}));
        let mut r = Registry::default();
        register(&mut r, "c", PREPARE_CALL_HIERARCHY, json!({"documentSelector": [{"language": "rust"}]}));
        assert!(caps(&f, &r, rust()).has(PREPARE_CALL_HIERARCHY));
        assert!(!caps(&f, &r, python()).has(PREPARE_CALL_HIERARCHY));
        assert!(caps(&f, &Registry::default(), rust()).has(PREPARE_TYPE_HIERARCHY));
    }

    #[test]
    fn inlay_hint_from_static_or_registered_capability() {
        let r = Registry::default();
        for (v, want) in [
            (json!({"inlayHintProvider": true}), true),
            (json!({"inlayHintProvider": {"resolveProvider": false}}), true),
            (json!({}), false),
            (json!({"inlayHintProvider": false}), false),
        ] {
            let f = fixed(v.clone());
            assert_eq!(caps(&f, &r, rust()).has(INLAY_HINT), want, "{v}");
        }
        let f = fixed(json!({}));
        let mut r = Registry::default();
        register(&mut r, "i", INLAY_HINT, json!({}));
        assert!(caps(&f, &r, rust()).has(INLAY_HINT));
    }

    #[test]
    fn document_highlight_from_static_or_registered_capability() {
        let r = Registry::default();
        for (v, want) in [
            (json!({"documentHighlightProvider": true}), true),
            (json!({"documentHighlightProvider": {}}), true),
            (json!({}), false),
            (json!({"documentHighlightProvider": false}), false),
        ] {
            let f = fixed(v.clone());
            assert_eq!(caps(&f, &r, rust()).has(DOCUMENT_HIGHLIGHT), want, "{v}");
        }
        let f = fixed(json!({}));
        let mut r = Registry::default();
        register(&mut r, "d", DOCUMENT_HIGHLIGHT, json!({}));
        assert!(caps(&f, &r, rust()).has(DOCUMENT_HIGHLIGHT));
    }

    #[test]
    fn registration_options_add_triggers_resolve_and_commands() {
        let f = fixed(
            json!({"completionProvider": {"triggerCharacters": [".", ":"], "allCommitCharacters": ["("]}, "executeCommandProvider": {"commands": ["a"]}}),
        );
        let mut r = Registry::default();
        register(
            &mut r,
            "c",
            COMPLETION,
            json!({"triggerCharacters": [":", ">"], "allCommitCharacters": [";", "("], "resolveProvider": true}),
        );
        register(&mut r, "r", RENAME, json!({"prepareProvider": true}));
        register(&mut r, "a", CODE_ACTION, json!({"resolveProvider": true}));
        register(&mut r, "x", EXECUTE_COMMAND, json!({"commands": ["b"]}));
        let c = caps(&f, &r, rust());
        assert_eq!(c.completion_triggers(), [".", ":", ">"]);
        assert_eq!(c.completion_commit_chars(), ["(", ";"]);
        assert!(c.has(RESOLVE) && c.has(PREPARE_RENAME) && c.has(CODE_ACTION_RESOLVE));
        assert!(c.runs_command("a") && c.runs_command("b") && !c.runs_command("c"));
    }

    #[test]
    fn signature_help_triggers_combine() {
        let f = fixed(json!({"signatureHelpProvider": {"triggerCharacters": ["(", ","], "retriggerCharacters": [")"]}}));
        let mut r = Registry::default();
        register(&mut r, "s", SIGNATURE_HELP, json!({"triggerCharacters": ["<", "("]}));
        let c = caps(&f, &r, rust());
        assert!(c.has(SIGNATURE_HELP));
        assert_eq!(c.signature_triggers(), ["(", ",", "<"]);
        assert_eq!(c.signature_retriggers(), [")"]);
        let f = fixed(json!({}));
        assert!(!caps(&f, &Registry::default(), rust()).has(SIGNATURE_HELP));
    }

    #[test]
    fn range_formatting_is_static_or_registered() {
        let f = fixed(json!({"documentRangeFormattingProvider": true}));
        let r = Registry::default();
        assert!(caps(&f, &r, rust()).has(RANGE_FORMATTING));
        let f = fixed(json!({}));
        let mut r = Registry::default();
        assert!(!caps(&f, &r, rust()).has(RANGE_FORMATTING));
        register(&mut r, "rf", RANGE_FORMATTING, json!({"documentSelector": [{"language": "rust"}]}));
        assert!(caps(&f, &r, rust()).has(RANGE_FORMATTING));
        assert!(!caps(&f, &r, python()).has(RANGE_FORMATTING));
    }

    #[test]
    fn code_action_kinds_are_the_union_or_any() {
        let r = Registry::default();
        let f = fixed(json!({"codeActionProvider": {}}));
        assert_eq!(caps(&f, &r, rust()).code_action_kinds(), None);
        let f = fixed(json!({"codeActionProvider": true}));
        assert_eq!(caps(&f, &r, rust()).code_action_kinds(), None);
        let f = fixed(json!({"codeActionProvider": {"codeActionKinds": ["quickfix", "refactor"]}}));
        let mut r = Registry::default();
        register(&mut r, "a", CODE_ACTION, json!({"codeActionKinds": ["source", "quickfix"]}));
        assert_eq!(caps(&f, &r, rust()).code_action_kinds().unwrap(), ["quickfix", "refactor", "source"]);
        register(&mut r, "b", CODE_ACTION, json!({"resolveProvider": true}));
        assert_eq!(caps(&f, &r, rust()).code_action_kinds(), None);
    }

    #[test]
    fn dynamic_registration_is_advertised_where_honoured() {
        let c = serde_json::to_value(client_capabilities(true)).unwrap();
        for k in [
            "definition",
            "typeDefinition",
            "implementation",
            "declaration",
            "references",
            "hover",
            "signatureHelp",
            "completion",
            "rename",
            "codeAction",
            "documentSymbol",
            "formatting",
            "rangeFormatting",
            "inlayHint",
            "documentHighlight",
            "semanticTokens",
        ] {
            assert_eq!(c["textDocument"][k]["dynamicRegistration"], true, "{k}");
        }
        assert_eq!(c["workspace"]["executeCommand"]["dynamicRegistration"], true);
        assert_eq!(c["workspace"]["symbol"]["dynamicRegistration"], true);
        assert_eq!(c["window"]["showMessage"]["messageActionItem"]["additionalPropertiesSupport"], true);
        assert_eq!(c["textDocument"]["synchronization"]["dynamicRegistration"], true);
        assert_eq!(c["workspace"]["fileOperations"]["dynamicRegistration"], true);
        assert_eq!(c["workspace"]["fileOperations"]["willRename"], true);
        assert_eq!(c["workspace"]["didChangeWatchedFiles"], json!({"dynamicRegistration": true, "relativePatternSupport": true}));
        assert!(serde_json::to_value(client_capabilities(false)).unwrap()["workspace"]["didChangeWatchedFiles"].is_null());
        assert!(c["workspace"]["didChangeConfiguration"].is_null());
        assert_eq!(c["workspace"]["workspaceEdit"]["changeAnnotationSupport"]["groupsOnLabel"], false);
        assert_eq!(c["textDocument"]["rename"]["honorsChangeAnnotations"], true);
        assert_eq!(c["textDocument"]["codeAction"]["honorsChangeAnnotations"], true);
    }

    const LEGEND: &str = r#"{"tokenTypes": ["variable"], "tokenModifiers": []}"#;

    fn legend() -> Value {
        serde_json::from_str(LEGEND).unwrap()
    }

    #[test]
    fn semantic_tokens_full_and_range_from_static_or_registered() {
        let r = Registry::default();
        let l = legend();
        let both = |v: Value| {
            let f = fixed(json!({ "semanticTokensProvider": v }));
            let c = caps(&f, &r, rust());
            (c.has(SEMANTIC_TOKENS_FULL), c.has(SEMANTIC_TOKENS_RANGE))
        };
        assert_eq!(both(json!({"legend": l, "full": true})), (true, false));
        assert_eq!(both(json!({"legend": l, "full": {"delta": true}})), (true, false));
        assert_eq!(both(json!({"legend": l, "range": true})), (false, true));
        assert_eq!(both(json!({"legend": l, "full": false, "range": false})), (false, false));
        let f = fixed(json!({}));
        assert!(!caps(&f, &r, rust()).has(SEMANTIC_TOKENS_FULL) && !caps(&f, &r, rust()).has(SEMANTIC_TOKENS_RANGE));
        let mut r = Registry::default();
        register(
            &mut r,
            "s",
            SEMANTIC_TOKENS,
            json!({"documentSelector": [{"language": "rust"}], "legend": l, "full": {"delta": false}, "range": true}),
        );
        let c = caps(&f, &r, rust());
        assert!(c.has(SEMANTIC_TOKENS_FULL) && c.has(SEMANTIC_TOKENS_RANGE));
        let c = caps(&f, &r, python());
        assert!(!c.has(SEMANTIC_TOKENS_FULL) && !c.has(SEMANTIC_TOKENS_RANGE));
    }

    #[test]
    fn semantic_legend_from_static_or_registration() {
        let mut r = Registry::default();
        let f = fixed(json!({}));
        assert!(caps(&f, &r, rust()).semantic_legend().is_none());
        register(&mut r, "n", SEMANTIC_TOKENS, json!({"documentSelector": [{"language": "rust"}], "full": true}));
        assert!(caps(&f, &r, rust()).semantic_legend().is_none(), "a registration without a legend");
        let mut r = Registry::default();
        let l = json!({"tokenTypes": ["function"], "tokenModifiers": ["mutable"]});
        register(&mut r, "s", SEMANTIC_TOKENS, json!({"documentSelector": [{"language": "rust"}], "legend": l, "full": true}));
        let got = caps(&f, &r, rust()).semantic_legend().unwrap();
        assert_eq!((got.token_types[0].as_str(), got.token_modifiers[0].as_str()), ("function", "mutable"));
        let f = fixed(json!({"semanticTokensProvider": {"legend": legend(), "full": true}}));
        let got = caps(&f, &r, rust()).semantic_legend().unwrap();
        assert_eq!((got.token_types.len(), got.token_types[0].as_str(), got.token_modifiers.len()), (1, "variable", 0));
    }

    #[test]
    fn sync_registrations_combine_with_static_sync() {
        let f = fixed(json!({}));
        let mut r = Registry::default();
        let sel = json!([{"language": "rust"}]);
        register(&mut r, "o", DID_OPEN, json!({"documentSelector": sel}));
        register(&mut r, "c", DID_CHANGE, json!({"documentSelector": sel, "syncKind": 2}));
        register(&mut r, "s", DID_SAVE, json!({"documentSelector": sel, "includeText": true}));
        let s = caps(&f, &r, rust()).sync();
        assert!(s.open && !s.close);
        assert_eq!(s.change, TextDocumentSyncKind::INCREMENTAL);
        assert_eq!(s.save, Some(true));
        let s = caps(&f, &r, python()).sync();
        assert!(!s.open && !s.close);
        assert_eq!(s.change, TextDocumentSyncKind::NONE);
        assert_eq!(s.save, None);
        let f = fixed(json!({"textDocumentSync": 1}));
        let mut r = Registry::default();
        register(&mut r, "c", DID_CHANGE, json!({"documentSelector": sel, "syncKind": 2}));
        assert_eq!(caps(&f, &r, rust()).sync().change, TextDocumentSyncKind::INCREMENTAL);
    }

    #[test]
    fn file_operation_filters_match_kind_glob_and_scheme() {
        let f = fixed(json!({"workspace": {"fileOperations": {"willRename": {"filters": [
            {"scheme": "file", "pattern": {"glob": "**/*.rs", "matches": "file"}},
            {"pattern": {"glob": "**", "matches": "folder"}}
        ]}}}}));
        let mut r = Registry::default();
        let c = Caps { fixed: &f, registry: &r, doc: None };
        assert!(c.file_operation(WILL_RENAME_FILES, Path::new("/p/src/a.rs"), false));
        assert!(!c.file_operation(WILL_RENAME_FILES, Path::new("/p/a.txt"), false));
        assert!(c.file_operation(WILL_RENAME_FILES, Path::new("/p/src"), true));
        assert!(!c.file_operation(DID_RENAME_FILES, Path::new("/p/src/a.rs"), false));
        assert!(!c.file_operation(DID_DELETE_FILES, Path::new("/p/a.txt"), false));
        register(&mut r, "d", DID_DELETE_FILES, json!({"filters": [{"pattern": {"glob": "**/*.TXT", "options": {"ignoreCase": true}}}]}));
        let c = Caps { fixed: &f, registry: &r, doc: None };
        assert!(c.file_operation(DID_DELETE_FILES, Path::new("/p/a.txt"), false));
        let f = fixed(
            json!({"workspace": {"fileOperations": {"didCreate": {"filters": [{"scheme": "untitled", "pattern": {"glob": "**"}}]}}}}),
        );
        let c = Caps { fixed: &f, registry: &r, doc: None };
        assert!(!c.file_operation(DID_CREATE_FILES, Path::new("/p/a.rs"), false));
    }

    #[test]
    fn diagnostic_tags_related_and_versions_are_advertised() {
        let c = serde_json::to_value(client_capabilities(true)).unwrap();
        let d = &c["textDocument"]["publishDiagnostics"];
        assert_eq!(d["relatedInformation"], true);
        assert_eq!(d["tagSupport"]["valueSet"], serde_json::json!([1, 2]));
        assert_eq!(d["versionSupport"], true);
    }

    #[test]
    fn completion_item_details_are_advertised() {
        let c = serde_json::to_value(client_capabilities(true)).unwrap();
        let i = &c["textDocument"]["completion"]["completionItem"];
        assert_eq!(i["labelDetailsSupport"], true);
        assert_eq!(i["commitCharactersSupport"], true);
    }

    #[test]
    fn malformed_legend_is_no_legend() {
        let f = fixed(json!({}));
        let mut r = Registry::default();
        register(&mut r, "s", SEMANTIC_TOKENS, json!({"documentSelector": [{"language": "rust"}], "legend": 5, "full": true}));
        let c = caps(&f, &r, rust());
        assert!(c.semantic_legend().is_none());
        assert!(c.has(SEMANTIC_TOKENS_FULL), "the capability stays; the tick finds no legend and asks nothing");
        // A static one never reaches `Caps`: the initialize answer does not parse, so the server fails to start.
        let init = json!({"capabilities": {"semanticTokensProvider": {"legend": 5, "full": true}}});
        assert!(serde_json::from_value::<lsp_types::InitializeResult>(init).is_err());
    }
}
