//! What a running server offers for one document: the capabilities of its
//! `initialize` answer combined with the dynamic registrations
//! ([`super::registry`]) whose document selector matches the document.
//!
//! The two combine as a union: a feature exists if either has it, a boolean
//! sub-option is true if any source sets it, and lists (trigger characters,
//! commands) are joined. Nothing is cached: callers ask when they decide.

use lsp_types::{CodeActionProviderCapability, HoverProviderCapability, OneOf, ServerCapabilities};
use serde_json::Value;

use super::registry::{DocRef, Registry};

pub const DEFINITION: &str = "textDocument/definition";
pub const REFERENCES: &str = "textDocument/references";
pub const HOVER: &str = "textDocument/hover";
pub const COMPLETION: &str = "textDocument/completion";
pub const RESOLVE: &str = "completionItem/resolve";
pub const PREPARE_RENAME: &str = "textDocument/prepareRename";
pub const RENAME: &str = "textDocument/rename";
pub const CODE_ACTION: &str = "textDocument/codeAction";
pub const CODE_ACTION_RESOLVE: &str = "codeAction/resolve";
pub const EXECUTE_COMMAND: &str = "workspace/executeCommand";
pub const DOCUMENT_SYMBOL: &str = "textDocument/documentSymbol";
pub const FORMATTING: &str = "textDocument/formatting";
pub const RANGE_FORMATTING: &str = "textDocument/rangeFormatting";

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

/// The strings of the array `o[key]`.
fn strings<'v>(o: &'v Value, key: &str) -> impl Iterator<Item = &'v str> + use<'v> {
    o.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str)
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
            REFERENCES => on(&fixed.references_provider) || self.any(REFERENCES),
            HOVER => {
                matches!(fixed.hover_provider, Some(HoverProviderCapability::Simple(true) | HoverProviderCapability::Options(_)))
                    || self.any(HOVER)
            }
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
            _ => false,
        }
    }

    /// Whether any registration of `method` applies.
    fn any(&self, method: &'a str) -> bool {
        self.dynamic(method).next().is_some()
    }

    /// Completion trigger characters: static ones, then registered ones, deduplicated in order.
    pub fn completion_triggers(&self) -> Vec<String> {
        let fixed = self.fixed.completion_provider.as_ref().and_then(|c| c.trigger_characters.as_ref()).into_iter().flatten();
        let mut out: Vec<String> = Vec::new();
        let registered = self.dynamic(COMPLETION).flat_map(|o| strings(o, "triggerCharacters"));
        for t in fixed.map(String::as_str).chain(registered) {
            if !out.iter().any(|o| o == t) {
                out.push(t.to_string());
            }
        }
        out
    }

    /// Completion commit characters: static `allCommitCharacters`, then registered ones, deduplicated in order.
    pub fn completion_commit_chars(&self) -> Vec<String> {
        let fixed = self.fixed.completion_provider.as_ref().and_then(|c| c.all_commit_characters.as_ref()).into_iter().flatten();
        let mut out: Vec<String> = Vec::new();
        let registered = self.dynamic(COMPLETION).flat_map(|o| strings(o, "allCommitCharacters"));
        for t in fixed.map(String::as_str).chain(registered) {
            if !out.iter().any(|o| o == t) {
                out.push(t.to_string());
            }
        }
        out
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
        let c = serde_json::to_value(client_capabilities()).unwrap();
        for k in
            ["definition", "references", "hover", "completion", "rename", "codeAction", "documentSymbol", "formatting", "rangeFormatting"]
        {
            assert_eq!(c["textDocument"][k]["dynamicRegistration"], true, "{k}");
        }
        assert_eq!(c["workspace"]["executeCommand"]["dynamicRegistration"], true);
        assert_ne!(c["textDocument"]["synchronization"]["dynamicRegistration"], true);
        assert!(c["workspace"]["didChangeWatchedFiles"].is_null());
        assert!(c["workspace"]["didChangeConfiguration"].is_null());
    }

    #[test]
    fn diagnostic_tags_related_and_versions_are_advertised() {
        let c = serde_json::to_value(client_capabilities()).unwrap();
        let d = &c["textDocument"]["publishDiagnostics"];
        assert_eq!(d["relatedInformation"], true);
        assert_eq!(d["tagSupport"]["valueSet"], serde_json::json!([1, 2]));
        assert_eq!(d["versionSupport"], true);
    }

    #[test]
    fn completion_item_details_are_advertised() {
        let c = serde_json::to_value(client_capabilities()).unwrap();
        let i = &c["textDocument"]["completion"]["completionItem"];
        assert_eq!(i["labelDetailsSupport"], true);
        assert_eq!(i["commitCharactersSupport"], true);
    }
}
