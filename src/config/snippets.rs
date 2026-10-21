//! Live templates: snippets with a prefix that Tab expands.
//!
//! Built-ins ship in `assets/snippets/<language>.toml` ([`BUILTIN`]). The user's
//! files are `<config dir>/snippets/<name>.toml`, where `<name>` is a
//! language name (`rust`, `python`, `typescript`, `tsx`, ...) or [`ALL`] for
//! every buffer. A file holds `[[snippet]]` tables: `prefix` (required, word
//! characters only), `body` (required, LSP snippet syntax) and `description`
//! (optional). Unknown keys are ignored.
//!
//! Lookup for a buffer of language `L`: the file `L` (a user snippet replaces
//! the built-in of the same prefix, a later duplicate replaces an earlier
//! one), then `typescript` for `tsx`, then `all`; the first snippet with a
//! given prefix wins.
//!
//! Error policy: a file that does not parse, cannot be read, or a snippet
//! with a bad prefix is reported and contributes nothing; the other files and
//! the built-ins stay.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::FileText;
use crate::config::theme::error_summary;
use crate::editor::selection::is_word_char;

/// The folder under the config dir that holds the user's snippet files.
pub const SNIPPETS_DIR: &str = "snippets";
/// The file stem of the snippets that apply to every buffer.
pub const ALL: &str = "all";
/// (file stem, TOML text) of the shipped snippets.
pub const BUILTIN: &[(&str, &str)] = &[
    ("rust", include_str!("../../assets/snippets/rust.toml")),
    ("python", include_str!("../../assets/snippets/python.toml")),
    ("typescript", include_str!("../../assets/snippets/typescript.toml")),
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Snippet {
    pub prefix: String,
    pub body: String,
    #[serde(default)]
    pub description: String,
}

/// The user's snippet files as last read: path and text, sorted by path.
pub type SnippetInputs = Vec<(PathBuf, FileText)>;

/// Every snippet, per file stem.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnippetSet {
    files: BTreeMap<String, Vec<Snippet>>,
}

#[derive(Deserialize)]
struct SnippetFile {
    #[serde(default)]
    snippet: Vec<Snippet>,
}

/// A prefix is non-empty and made of word characters.
pub fn is_prefix(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_word_char)
}

/// The snippets of one file and the errors it gave; `name` is the file name.
fn parse(name: &str, text: &str) -> (Vec<Snippet>, Vec<String>) {
    let file: SnippetFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            return (Vec::new(), vec![format!("snippets/{name} is invalid: {}; its snippets are ignored", error_summary(&e.to_string()))]);
        }
    };
    let mut snippets = Vec::new();
    let mut errors = Vec::new();
    for s in file.snippet {
        if is_prefix(&s.prefix) {
            snippets.push(s);
        } else {
            errors.push(format!("snippets/{name}: prefix \"{}\" must be letters, digits or _; that snippet is ignored", s.prefix));
        }
    }
    (snippets, errors)
}

/// Replaces the entry with the same prefix in place, else appends.
fn merge(into: &mut Vec<Snippet>, s: Snippet) {
    match into.iter_mut().find(|e| e.prefix == s.prefix) {
        Some(e) => *e = s,
        None => into.push(s),
    }
}

/// The file stems to look in, in order.
fn stems(lang: Option<&str>) -> Vec<&str> {
    match lang {
        Some("tsx") => vec!["tsx", "typescript", ALL],
        Some(l) => vec![l, ALL],
        None => vec![ALL],
    }
}

impl SnippetSet {
    /// The `*.toml` files of `dir`, sorted by path. A missing directory is empty.
    pub fn read(dir: &Path) -> SnippetInputs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!("snippets dir {}: {e}", dir.display());
                return Vec::new();
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml") && p.is_file())
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|p| {
                let text = FileText::read(&p);
                (p, text)
            })
            .collect()
    }

    /// The built-ins with the user's files merged over them, and the
    /// errors the user's files gave, each with its path.
    pub fn build(inputs: &[(PathBuf, FileText)]) -> (Self, Vec<(PathBuf, String)>) {
        let mut files: BTreeMap<String, Vec<Snippet>> = BTreeMap::new();
        for (stem, text) in BUILTIN {
            let (snippets, errors) = parse(&format!("{stem}.toml"), text);
            for e in errors {
                tracing::error!("built-in {e}");
            }
            let list = files.entry((*stem).to_string()).or_default();
            for s in snippets {
                merge(list, s);
            }
        }
        let mut errors = Vec::new();
        for (path, text) in inputs {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let name = format!("{stem}.toml");
            match text {
                FileText::Text(t) => {
                    let (snippets, errs) = parse(&name, t);
                    let list = files.entry(stem.to_string()).or_default();
                    for s in snippets {
                        merge(list, s);
                    }
                    errors.extend(errs.into_iter().map(|e| (path.clone(), e)));
                }
                FileText::Unreadable(e) => {
                    errors.push((path.clone(), format!("Cannot read snippets/{name}: {e}; its snippets are ignored")))
                }
                FileText::Missing => {}
            }
        }
        (Self { files }, errors)
    }

    /// The snippets that apply to a buffer of `lang`, sorted by prefix.
    pub fn for_language(&self, lang: Option<&str>) -> Vec<&Snippet> {
        let mut out: Vec<&Snippet> = Vec::new();
        for stem in stems(lang) {
            let Some(list) = self.files.get(stem) else { continue };
            let seen = out.len();
            for s in list {
                if !out[..seen].iter().any(|o| o.prefix == s.prefix) {
                    out.push(s);
                }
            }
        }
        out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
        out
    }

    /// The snippet whose prefix is exactly `prefix` (case-sensitive).
    pub fn exact(&self, lang: Option<&str>, prefix: &str) -> Option<&Snippet> {
        stems(lang).into_iter().find_map(|stem| self.files.get(stem)?.iter().find(|s| s.prefix == prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(case: &str, files: &[(&str, &str)]) -> Self {
            let d = std::env::temp_dir().join(format!("jetl-test-snippets-{case}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            for (n, t) in files {
                std::fs::write(d.join(n), t).unwrap();
            }
            Dir(d)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn snip(prefix: &str, body: &str) -> String {
        format!("[[snippet]]\nprefix = \"{prefix}\"\nbody = \"{body}\"\n")
    }

    fn prefixes(set: &SnippetSet, lang: Option<&str>) -> Vec<String> {
        set.for_language(lang).iter().map(|s| s.prefix.clone()).collect()
    }

    fn body<'a>(set: &'a SnippetSet, lang: Option<&str>, prefix: &str) -> &'a str {
        &set.exact(lang, prefix).unwrap().body
    }

    #[test]
    fn parses_snippet_tables() {
        let (s, e) = parse(
            "x.toml",
            "[[snippet]]\nprefix = \"a\"\nbody = \"A\"\ndescription = \"d\"\n\n[[snippet]]\nprefix = \"b\"\nbody = \"B\"\n",
        );
        assert!(e.is_empty());
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].prefix.as_str(), s[0].description.as_str()), ("a", "d"));
        assert_eq!((s[1].prefix.as_str(), s[1].description.as_str()), ("b", ""));
        let (s, e) = parse("x.toml", "# nothing\n");
        assert!(s.is_empty() && e.is_empty());
    }

    #[test]
    fn builtins_parse_and_cover_the_listed_prefixes() {
        for (stem, text) in BUILTIN {
            let (s, e) = parse(&format!("{stem}.toml"), text);
            assert!(e.is_empty(), "{stem}: {e:?}");
            assert!(!s.is_empty());
        }
        let (set, errors) = SnippetSet::build(&[]);
        assert!(errors.is_empty());
        assert_eq!(prefixes(&set, Some("rust")), ["fn", "if", "impl", "match", "test", "while"]);
        assert_eq!(prefixes(&set, Some("python")), ["class", "def", "if", "ifmain", "try"]);
        assert_eq!(prefixes(&set, Some("typescript")), ["fn", "if", "log", "try"]);
        for (stem, _) in BUILTIN {
            for s in set.for_language(Some(stem)) {
                let (_, stops) = crate::lsp::completion::parse_snippet(&s.body);
                assert!(!stops.is_empty(), "{stem} {}: no tabstop", s.prefix);
            }
        }
    }

    #[test]
    fn user_file_overrides_a_builtin_by_prefix() {
        let d = Dir::new("override", &[("rust.toml", &(snip("fn", "FN") + &snip("pf", "PF")))]);
        let (set, errors) = SnippetSet::build(&SnippetSet::read(&d.0));
        assert!(errors.is_empty());
        assert_eq!(body(&set, Some("rust"), "fn"), "FN");
        assert_eq!(body(&set, Some("rust"), "pf"), "PF");
        assert_eq!(prefixes(&set, Some("rust")), ["fn", "if", "impl", "match", "pf", "test", "while"]);
        assert_eq!(prefixes(&set, Some("python")), ["class", "def", "if", "ifmain", "try"]);
    }

    #[test]
    fn all_toml_applies_everywhere_and_loses_to_the_language() {
        let d = Dir::new("all", &[("all.toml", &(snip("fn", "ALL") + &snip("todo", "T")))]);
        let (set, errors) = SnippetSet::build(&SnippetSet::read(&d.0));
        assert!(errors.is_empty());
        assert!(body(&set, Some("rust"), "fn").starts_with("fn ${1:name}"));
        assert!(prefixes(&set, Some("rust")).contains(&"todo".to_string()));
        for lang in [None, Some("go")] {
            assert_eq!(prefixes(&set, lang), ["fn", "todo"]);
            assert_eq!(body(&set, lang, "fn"), "ALL");
        }
        assert!(set.exact(Some("rust"), "todo").is_some());
        assert!(set.exact(Some("rust"), "Fn").is_none());
    }

    #[test]
    fn tsx_uses_typescript_snippets() {
        let (set, _) = SnippetSet::build(&[]);
        assert!(prefixes(&set, Some("tsx")).contains(&"log".to_string()));
        let d = Dir::new("tsx", &[("tsx.toml", &snip("log", "TSX"))]);
        let (set, errors) = SnippetSet::build(&SnippetSet::read(&d.0));
        assert!(errors.is_empty());
        assert_eq!(body(&set, Some("tsx"), "log"), "TSX");
        assert_eq!(body(&set, Some("typescript"), "log"), "console.log(${1});$0");
    }

    #[test]
    fn bad_file_keeps_the_builtins_and_the_other_files() {
        let d = Dir::new("bad", &[("rust.toml", "[[snippet]\nprefix = 1\n"), ("all.toml", &snip("todo", "T"))]);
        let (set, errors) = SnippetSet::build(&SnippetSet::read(&d.0));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].0.ends_with("rust.toml"));
        assert!(errors[0].1.starts_with("snippets/rust.toml is invalid: "), "{}", errors[0].1);
        assert!(errors[0].1.ends_with("; its snippets are ignored"));
        assert_eq!(prefixes(&set, Some("rust")), ["fn", "if", "impl", "match", "test", "todo", "while"]);
    }

    #[test]
    fn bad_prefixes_are_skipped() {
        let text = snip("", "E") + &snip("a b", "S") + &snip("ok_1", "O");
        let d = Dir::new("prefix", &[("all.toml", &text)]);
        let (set, errors) = SnippetSet::build(&SnippetSet::read(&d.0));
        assert_eq!(prefixes(&set, None), ["ok_1"]);
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|(_, e)| e.contains("prefix \"a b\" must be")));
        assert!(errors.iter().any(|(_, e)| e.contains("prefix \"\" must be")));
    }

    #[test]
    fn missing_dir_reads_nothing() {
        let missing = std::env::temp_dir().join(format!("jetl-test-snippets-missing-{}", std::process::id()));
        assert!(SnippetSet::read(&missing).is_empty());
        let (set, errors) = SnippetSet::build(&[]);
        assert!(errors.is_empty());
        assert_eq!(set.for_language(Some("rust")).len(), 6);
    }

    #[test]
    fn non_toml_files_are_ignored() {
        let d = Dir::new("nontoml", &[("rust.toml.swp", "x"), ("notes.txt", "y")]);
        assert!(SnippetSet::read(&d.0).is_empty());
    }
}
