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

use super::sync;

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
            parsed.push(Registration { id: id.to_string(), method: method.to_string(), options, selector });
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
