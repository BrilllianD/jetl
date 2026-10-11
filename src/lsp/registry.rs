//! Dynamic capability registration (`client/registerCapability` and
//! `client/unregisterCapability`): the registrations of one server, kept by
//! its [`super::client::Client`], with their document selectors compiled.
//!
//! Params are read from JSON rather than `lsp_types`: lsp-types 0.97 has a
//! string-only `DocumentFilter::pattern` (no 3.17 `RelativePattern`), and the
//! misspelled `unregistrations` is accepted beside the spec's
//! `unregisterations`. A malformed request changes nothing; a filter jetl
//! cannot match (a notebook filter, an empty one, a bad glob) matches nothing.

use std::path::{Path, PathBuf};

use lsp_types::Uri;
use serde_json::Value;

use super::caps::DID_CHANGE_WATCHED_FILES;
use super::sync;
use crate::files::watch::FileChange;

/// Dynamic registrations of one server (`client/registerCapability`),
/// in arrival order. Ids are unique: a repeated id replaces the old entry.
#[derive(Debug, Default)]
pub struct Registry {
    regs: Vec<Registration>,
}

#[derive(Debug)]
pub struct Registration {
    pub id: String,
    pub method: String,
    /// `registerOptions` as sent (`Value::Null` when absent). Tasks 139/141
    /// read what they need from here (`filters`, `codeActionKinds`, ...).
    pub options: Value,
    /// `registerOptions.documentSelector`, compiled. `None`: null or absent,
    /// which means the client's own selector, i.e. every document jetl opens
    /// on this server.
    selector: Option<Vec<Filter>>,
    /// `registerOptions.watchers` of a `workspace/didChangeWatchedFiles`
    /// registration, compiled (empty for any other method).
    watchers: Vec<FileWatcher>,
}

/// One `FileSystemWatcher`: a glob and the `WatchKind` bits (1 create, 2 change, 4 delete).
#[derive(Debug)]
struct FileWatcher {
    pattern: Pattern,
    kind: u8,
}

/// One `DocumentFilter`. Matches when every field that is set matches.
#[derive(Debug)]
struct Filter {
    language: Option<String>,
    scheme: Option<String>,
    pattern: Option<Pattern>,
    /// A notebook filter, an empty filter `{}` or an uncompilable glob:
    /// matches nothing.
    never: bool,
}

#[derive(Debug)]
enum Pattern {
    /// A string `GlobPattern`: matched against the absolute path, or equal to it.
    Absolute { text: String, glob: Glob },
    /// `RelativePattern { baseUri, pattern }`: matched against the path relative
    /// to the base. `baseUri` is a URI string or a `WorkspaceFolder` `{uri, name}`.
    Relative { base: PathBuf, glob: Glob },
}

/// An LSP glob (`*`, `**`, `?`, `{a,b}`, `[0-9]`, `[!0-9]`), `*` and `?` not
/// crossing `/`. Public so that file operation filters can be compiled too.
#[derive(Debug, Clone)]
pub struct Glob(globset::GlobMatcher);

impl Glob {
    /// `None` when the pattern does not compile.
    pub fn new(pattern: &str, ignore_case: bool) -> Option<Self> {
        let glob = globset::GlobBuilder::new(pattern).literal_separator(true).case_insensitive(ignore_case).build().ok()?;
        Some(Self(glob.compile_matcher()))
    }

    pub fn is_match(&self, path: &Path) -> bool {
        self.0.is_match(path)
    }
}

/// A document as a selector sees it. The scheme is always `file`, because jetl
/// opens only files on servers (untitled buffers and scratches never).
#[derive(Debug, Clone, Copy)]
pub struct DocRef<'a> {
    pub language_id: &'a str,
    pub path: &'a Path,
}

impl Registry {
    /// `client/registerCapability` params. All or nothing: a malformed entry
    /// refuses the whole request (Err text goes into InvalidParams).
    pub fn register(&mut self, params: &Value) -> Result<(), String> {
        let entries = params.get("registrations").and_then(Value::as_array).ok_or("registrations: expected an array")?;
        let mut parsed = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = entry.get("id").and_then(Value::as_str).ok_or("registration: expected an id string")?;
            let method = entry.get("method").and_then(Value::as_str).ok_or("registration: expected a method string")?;
            let options = entry.get("registerOptions").cloned().unwrap_or(Value::Null);
            let selector = parse_selector(options.get("documentSelector").unwrap_or(&Value::Null))?;
            let watchers = if method == DID_CHANGE_WATCHED_FILES { parse_watchers(&options)? } else { Vec::new() };
            parsed.push(Registration { id: id.to_string(), method: method.to_string(), options, selector, watchers });
        }
        for reg in parsed {
            self.regs.retain(|r| r.id != reg.id);
            self.regs.push(reg);
        }
        Ok(())
    }

    /// `client/unregisterCapability` params: `unregisterations` (the spec's
    /// spelling, also lsp-types') or `unregistrations`. Unknown ids are ignored.
    pub fn unregister(&mut self, params: &Value) -> Result<(), String> {
        let entries = ["unregisterations", "unregistrations"]
            .iter()
            .find_map(|k| params.get(*k).and_then(Value::as_array))
            .ok_or("unregisterations: expected an array")?;
        let ids = entries
            .iter()
            .map(|e| e.get("id").and_then(Value::as_str).ok_or("unregistration: expected an id string"))
            .collect::<Result<Vec<_>, _>>()?;
        self.regs.retain(|r| !ids.contains(&r.id.as_str()));
        Ok(())
    }

    /// Registrations of `method` that apply to `doc`. A registration with a
    /// selector never applies when `doc` is `None` (workspace-level queries).
    pub fn matching<'a>(&'a self, method: &'a str, doc: Option<DocRef<'a>>) -> impl Iterator<Item = &'a Registration> + 'a {
        self.regs.iter().filter(move |r| r.method == method && r.applies_to(doc))
    }

    /// Whether some `workspace/didChangeWatchedFiles` registration wants `change` on `path`.
    pub fn watches(&self, path: &Path, change: FileChange) -> bool {
        let bit = match change {
            FileChange::Created => 1,
            FileChange::Changed => 2,
            FileChange::Deleted => 4,
        };
        self.regs
            .iter()
            .filter(|r| r.method == DID_CHANGE_WATCHED_FILES)
            .flat_map(|r| &r.watchers)
            .any(|w| w.kind & bit != 0 && w.pattern.matches(path))
    }

    /// Folders the registered watchers are rooted at, sorted and deduplicated.
    /// A relative pattern gives its base, an absolute glob its literal prefix
    /// (the parent when it has no glob characters); a glob that is not
    /// absolute (`**/*.rs`) is not tied to a folder.
    pub fn watch_bases(&self) -> Vec<PathBuf> {
        let mut bases: Vec<PathBuf> = self
            .regs
            .iter()
            .filter(|r| r.method == DID_CHANGE_WATCHED_FILES)
            .flat_map(|r| &r.watchers)
            .filter_map(|w| match &w.pattern {
                Pattern::Relative { base, .. } => Some(base.clone()),
                Pattern::Absolute { text, .. } => literal_prefix(text),
            })
            .collect();
        bases.sort();
        bases.dedup();
        bases
    }

    pub fn clear(&mut self) {
        self.regs.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.regs.is_empty()
    }
}

impl Registration {
    pub fn applies_to(&self, doc: Option<DocRef<'_>>) -> bool {
        match (&self.selector, doc) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(filters), Some(doc)) => filters.iter().any(|f| f.matches(doc)),
        }
    }
}

impl Filter {
    fn matches(&self, doc: DocRef<'_>) -> bool {
        !self.never
            && self.language.as_deref().is_none_or(|l| l == doc.language_id)
            && self.scheme.as_deref().is_none_or(|s| s == "file")
            && self.pattern.as_ref().is_none_or(|p| p.matches(doc.path))
    }
}

impl Pattern {
    fn matches(&self, path: &Path) -> bool {
        match self {
            Pattern::Absolute { text, glob } => *text == path.to_string_lossy() || glob.is_match(path),
            Pattern::Relative { base, glob } => path.strip_prefix(base).is_ok_and(|rest| glob.is_match(rest)),
        }
    }
}

/// The components of an absolute glob before the first one with a glob
/// character; the parent when all are literal.
fn literal_prefix(text: &str) -> Option<PathBuf> {
    if !text.starts_with('/') {
        return None;
    }
    let comps: Vec<&str> = text.split('/').filter(|c| !c.is_empty()).collect();
    let literal = comps.iter().take_while(|c| !c.contains(['*', '?', '[', '{'])).count();
    let keep = if literal == comps.len() { literal.saturating_sub(1) } else { literal };
    let mut base = PathBuf::from("/");
    base.extend(&comps[..keep]);
    Some(base)
}

fn parse_watchers(options: &Value) -> Result<Vec<FileWatcher>, String> {
    let entries = options.get("watchers").and_then(Value::as_array).ok_or("watchers: expected an array")?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(pattern) = entry.get("globPattern").and_then(parse_pattern) else {
            tracing::warn!("ignoring a file watcher with an unusable glob: {entry}");
            continue;
        };
        out.push(FileWatcher { pattern, kind: (entry["kind"].as_u64().unwrap_or(7) & 7) as u8 });
    }
    Ok(out)
}

fn parse_selector(v: &Value) -> Result<Option<Vec<Filter>>, String> {
    match v {
        Value::Null => Ok(None),
        Value::Array(items) => Ok(Some(items.iter().map(parse_filter).collect())),
        _ => Err("documentSelector: expected an array or null".into()),
    }
}

fn parse_filter(v: &Value) -> Filter {
    let never = Filter { language: None, scheme: None, pattern: None, never: true };
    // A bare string is a language id, as VS Code takes it.
    if let Some(language) = v.as_str() {
        return Filter { language: Some(language.to_string()), scheme: None, pattern: None, never: false };
    }
    let Some(o) = v.as_object() else { return never };
    if o.contains_key("notebook") {
        return never;
    }
    let text = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
    let (language, scheme) = (text("language"), text("scheme"));
    let pattern = match o.get("pattern") {
        None | Some(Value::Null) => None,
        Some(p) => match parse_pattern(p) {
            Some(p) => Some(p),
            None => {
                tracing::warn!("ignoring a document filter with an unusable pattern: {p}");
                return never;
            }
        },
    };
    if language.is_none() && scheme.is_none() && pattern.is_none() {
        return never;
    }
    Filter { language, scheme, pattern, never: false }
}

fn parse_pattern(v: &Value) -> Option<Pattern> {
    if let Some(text) = v.as_str() {
        return Some(Pattern::Absolute { text: text.to_string(), glob: Glob::new(text, false)? });
    }
    let base = v.get("baseUri")?;
    let base = base.as_str().or_else(|| base.get("uri").and_then(Value::as_str))?;
    let base = sync::path(&base.parse::<Uri>().ok()?)?;
    let glob = Glob::new(v.get("pattern")?.as_str()?, false)?;
    Some(Pattern::Relative { base, glob })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn glob(p: &str) -> Glob {
        Glob::new(p, false).unwrap()
    }

    fn m(g: &Glob, p: &str) -> bool {
        g.is_match(Path::new(p))
    }

    fn rust() -> DocRef<'static> {
        DocRef { language_id: "rust", path: Path::new("/p/src/a.rs") }
    }

    /// A registry holding one registration of method `m` with `selector`.
    fn with_selector(selector: Value) -> Registry {
        let mut r = Registry::default();
        let params = json!({"registrations": [{"id": "x", "method": "m", "registerOptions": {"documentSelector": selector}}]});
        r.register(&params).unwrap();
        r
    }

    fn applies(r: &Registry, doc: Option<DocRef<'_>>) -> bool {
        r.matching("m", doc).next().is_some()
    }

    #[test]
    fn glob_patterns_follow_lsp_syntax() {
        let g = glob("**/*.rs");
        assert!(m(&g, "/home/u/p/src/a.rs") && m(&g, "a.rs"));
        assert!(!m(&glob("*.rs"), "src/a.rs"));
        let g = glob("src/?.rs");
        assert!(m(&g, "src/a.rs") && !m(&g, "src/ab.rs"));
        assert!(m(&glob("**/*.{ts,tsx}"), "x/a.tsx"));
        assert!(m(&glob("v[0-9].txt"), "v1.txt"));
        let g = glob("v[!0-9].txt");
        assert!(m(&g, "va.txt") && !m(&g, "v1.txt"));
        assert!(m(&Glob::new("**/*.RS", true).unwrap(), "x/a.rs"));
        assert!(Glob::new("{", false).is_none());
    }

    #[test]
    fn filters_match_language_scheme_and_pattern() {
        for selector in [
            json!([{"language": "rust"}]),
            json!([{"scheme": "file"}]),
            json!([{"pattern": "**/*.rs"}]),
            json!([{"language": "rust", "pattern": "**/src/**"}]),
            json!(["rust"]),
            json!([{"language": "python"}, {"language": "rust"}]),
        ] {
            let r = with_selector(selector.clone());
            assert!(r.regs[0].applies_to(Some(rust())), "{selector}");
        }
        for selector in
            [json!([{"language": "python"}]), json!([{"scheme": "untitled"}]), json!([{"language": "rust", "pattern": "**/*.toml"}])]
        {
            let r = with_selector(selector.clone());
            assert!(!r.regs[0].applies_to(Some(rust())), "{selector}");
        }
    }

    #[test]
    fn relative_patterns_match_under_their_base() {
        let r = with_selector(json!([{"pattern": {"baseUri": "file:///p", "pattern": "src/*.rs"}}]));
        assert!(applies(&r, Some(rust())));
        assert!(!applies(&r, Some(DocRef { language_id: "rust", path: Path::new("/q/src/a.rs") })));
        let r = with_selector(json!([{"pattern": {"baseUri": {"uri": "file:///p", "name": "p"}, "pattern": "**/*.rs"}}]));
        assert!(applies(&r, Some(DocRef { language_id: "rust", path: Path::new("/p/a.rs") })));
    }

    #[test]
    fn selectors_null_empty_notebook_and_workspace_queries() {
        let mut r = Registry::default();
        r.register(&json!({"registrations": [{"id": "a", "method": "m"}]})).unwrap();
        assert!(applies(&r, Some(rust())) && applies(&r, None));
        let r = with_selector(Value::Null);
        assert!(applies(&r, Some(rust())) && applies(&r, None));
        assert!(!applies(&with_selector(json!([])), Some(rust())));
        assert!(!applies(&with_selector(json!([{"notebook": "jupyter"}])), Some(rust())));
        assert!(!applies(&with_selector(json!([{}])), Some(rust())));
        assert!(!applies(&with_selector(json!([{"language": "rust"}])), None));
        assert!(!applies(&with_selector(json!([{"pattern": "{"}])), Some(rust())));
    }

    #[test]
    fn register_replaces_by_id_and_refuses_malformed_params_whole() {
        let mut r = Registry::default();
        r.register(&json!({"registrations": [{"id": "a", "method": "m", "registerOptions": {"n": 1}}]})).unwrap();
        r.register(&json!({"registrations": [{"id": "a", "method": "m", "registerOptions": {"n": 2}}]})).unwrap();
        assert_eq!(r.regs.len(), 1);
        assert_eq!(r.regs[0].options, json!({"n": 2}));
        assert!(r.register(&json!({"registrations": [{"id": "b", "method": "m"}, {"method": "m"}]})).is_err());
        assert!(r.regs.iter().all(|x| x.id != "b"));
        assert!(r.register(&json!({"registrations": "x"})).is_err());
        let bad = json!({"registrations": [{"id": "c", "method": "m", "registerOptions": {"documentSelector": 5}}]});
        assert!(r.register(&bad).is_err());
        assert_eq!(r.regs.len(), 1);
    }

    #[test]
    fn watchers_match_kind_and_pattern() {
        use FileChange::{Changed, Created, Deleted};
        let mut r = Registry::default();
        let params = json!({"registrations": [{"id": "w", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": [
            {"globPattern": "**/*.rs"},
            {"globPattern": {"baseUri": "file:///p", "pattern": "Cargo.toml"}, "kind": 4},
            {"globPattern": "{"},
        ]}}]});
        r.register(&params).unwrap();
        let rs = Path::new("/p/src/a.rs");
        assert!(r.watches(rs, Created) && r.watches(rs, Changed) && r.watches(rs, Deleted));
        let cargo = Path::new("/p/Cargo.toml");
        assert!(r.watches(cargo, Deleted));
        assert!(!r.watches(cargo, Changed) && !r.watches(cargo, Created));
        assert!(!r.watches(Path::new("/p/notes.txt"), Changed));
        assert_eq!(r.regs[0].watchers.len(), 2, "the bad glob is skipped");
        assert!(
            r.register(
                &json!({"registrations": [{"id": "x", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": 5}}]})
            )
            .is_err()
        );
        assert_eq!(r.regs.len(), 1);
    }

    #[test]
    fn watch_bases_are_literal_prefixes() {
        let mut r = Registry::default();
        let params = json!({"registrations": [{"id": "w", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": [
            {"globPattern": "/q/dep/**/*.rs"},
            {"globPattern": "/q/x/Cargo.toml"},
            {"globPattern": "**/*.rs"},
            {"globPattern": {"baseUri": "file:///p", "pattern": "**/*.rs"}},
            {"globPattern": {"baseUri": "file:///p", "pattern": "Cargo.toml"}},
        ]}}]});
        r.register(&params).unwrap();
        assert_eq!(r.watch_bases(), [PathBuf::from("/p"), PathBuf::from("/q/dep"), PathBuf::from("/q/x")]);
    }

    #[test]
    fn unregister_takes_both_spellings() {
        let mut r = Registry::default();
        r.register(&json!({"registrations": [{"id": "a", "method": "m"}, {"id": "b", "method": "m"}, {"id": "c", "method": "m"}]}))
            .unwrap();
        r.unregister(&json!({"unregisterations": [{"id": "a", "method": "m"}]})).unwrap();
        r.unregister(&json!({"unregistrations": [{"id": "b", "method": "m"}]})).unwrap();
        let ids = |r: &Registry| r.regs.iter().map(|x| x.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&r), ["c"]);
        r.unregister(&json!({"unregisterations": [{"id": "zzz", "method": "m"}]})).unwrap();
        assert_eq!(ids(&r), ["c"]);
        assert!(r.unregister(&json!({})).is_err());
        assert_eq!(ids(&r), ["c"]);
    }
}
