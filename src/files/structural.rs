//! Structural search for Find in files (feature `structural`): an ast-grep
//! pattern, parsed per file language from `LANGUAGES`. `$X` matches one node,
//! `$$$X` any number of them. Files without a grammar, binary files and files
//! where the pattern does not parse as code are skipped.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use ast_grep_core::matcher::PatternBuilder;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use ast_grep_core::{Language, Pattern, PatternError};

use crate::editor::syntax::{self, LanguageDef};
use crate::files::find::{Hit, hits_at, is_binary};

/// A jetl grammar as an ast-grep language. `expando` stands in for `$` where
/// `$` cannot start an identifier.
#[derive(Clone, Copy)]
struct Lang {
    def: &'static LanguageDef,
    expando: char,
}

impl Language for Lang {
    fn pre_process_pattern<'q>(&self, q: &'q str) -> Cow<'q, str> {
        if self.expando == '$' { Cow::Borrowed(q) } else { Cow::Owned(pre_process(self.expando, q)) }
    }

    fn expando_char(&self) -> char {
        self.expando
    }

    fn kind_to_id(&self, kind: &str) -> u16 {
        self.def.language.id_for_node_kind(kind, true)
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.def.language.field_id_for_name(field).map(|f| f.get())
    }

    fn build_pattern(&self, b: &PatternBuilder) -> Result<Pattern, PatternError> {
        b.build(|src| StrDoc::try_new(src, *self))
    }
}

impl LanguageExt for Lang {
    fn get_ts_language(&self) -> TSLanguage {
        self.def.language.clone()
    }
}

/// The `$` of metavariables (`$A`, `$$A`, `$_`, `$$$A`, `$$$`) replaced by
/// `expando`; other `$` are kept (ast-grep-language's rule).
fn pre_process(expando: char, q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    let mut dollars = 0;
    for c in q.chars() {
        if c == '$' {
            dollars += 1;
            continue;
        }
        let sigil = if c.is_ascii_uppercase() || c == '_' || dollars == 3 { expando } else { '$' };
        out.extend(std::iter::repeat_n(sigil, dollars));
        dollars = 0;
        out.push(c);
    }
    let sigil = if dollars == 3 { expando } else { '$' };
    out.extend(std::iter::repeat_n(sigil, dollars));
    out
}

/// The metavariable stand-in of a language (ast-grep-language's table).
fn expando(name: &str) -> char {
    match name {
        "rust" | "python" | "c" | "cpp" | "csharp" | "go" | "kotlin" | "php" | "ruby" | "swift" => 'µ',
        "css" | "scss" => '_',
        _ => '$',
    }
}

/// The pattern compiled once per language as files of that language turn up;
/// `None` means it does not parse there.
pub struct Patterns {
    src: String,
    by_lang: HashMap<&'static str, Option<Pattern>>,
}

impl Patterns {
    pub fn new(src: &str) -> Self {
        Self { src: src.to_string(), by_lang: HashMap::new() }
    }

    fn get(&mut self, def: &'static LanguageDef) -> Option<(Lang, &Pattern)> {
        let lang = Lang { def, expando: expando(def.name) };
        let src = &self.src;
        let entry = self.by_lang.entry(def.name).or_insert_with(|| {
            let p = Pattern::try_new(src, lang).ok().filter(|p| !p.has_error());
            if p.is_none() {
                tracing::debug!("structural pattern does not parse as {}", def.name);
            }
            p
        });
        entry.as_ref().map(|p| (lang, p))
    }
}

/// Hits of the pattern in a file; none when the file is binary, has no known
/// language, or the pattern does not build for that language.
pub fn search_file(path: &Path, bytes: &[u8], patterns: &mut Patterns, limit: usize) -> Vec<Hit> {
    if is_binary(bytes) {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let Some(def) = syntax::detect_text(path, &text) else { return Vec::new() };
    let Some((lang, pattern)) = patterns.get(def) else { return Vec::new() };
    let root = lang.ast_grep(&*text);
    let mut ranges: Vec<_> = root.root().find_all(pattern).take(limit).map(|m| m.get_node().range()).collect();
    // Pre-order is non-decreasing already; sort to guard `hits_at`'s
    // precondition, and drop a node and its same-span child both matching.
    ranges.sort_by_key(|r| r.start);
    ranges.dedup();
    hits_at(path, &text, ranges, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(path: &str, text: &str, pattern: &str, limit: usize) -> Vec<(usize, usize, String)> {
        search_file(Path::new(path), text.as_bytes(), &mut Patterns::new(pattern), limit)
            .into_iter()
            .map(|h| (h.line, h.col, h.matched))
            .collect()
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_metavariables_match_nodes() {
        let src = "fn main() {\n  let x = foo().unwrap();\n  bar(y.unwrap());\n}\n";
        assert_eq!(find("a.rs", src, "$A.unwrap()", 100), [(1, 10, "foo().unwrap()".to_string()), (2, 6, "y.unwrap()".to_string())]);
        let calls = find("a.rs", "fn f() { foo(1, 2); foo(); food(3); }", "foo($$$ARGS)", 100);
        let matched: Vec<_> = calls.iter().map(|c| c.2.as_str()).collect();
        assert_eq!(matched, ["foo(1, 2)", "foo()"]);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn multi_line_match_is_one_hit() {
        let src = "fn f() {\n    a.b(1)\n        .unwrap();\n}\n";
        let hits = search_file(Path::new("a.rs"), src.as_bytes(), &mut Patterns::new("$A.unwrap()"), 100);
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!((h.line, h.col, h.len), (1, 4, "a.b(1)".len()));
        assert!(h.matched.contains('\n') && h.matched.ends_with(".unwrap()"));
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_uses_the_expando() {
        assert_eq!(find("a.py", "print(1)\nprint (2)\n", "print($X)", 100).len(), 2);
    }

    #[cfg(feature = "lang-js")]
    #[test]
    fn javascript_keeps_the_dollar() {
        assert_eq!(find("a.js", "console.log(1);\nconsole.log(a, b);\n", "console.log($X)", 100).len(), 1);
    }

    #[test]
    fn pre_process_rewrites_only_metavariables() {
        assert_eq!(pre_process('µ', "$A + $$B + $$$C + $$$ + $_ + $a + $"), "µA + µµB + µµµC + µµµ + µ_ + $a + $");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn unparsable_pattern_and_unknown_language_give_nothing() {
        assert!(find("a.rs", "fn f() {}\n", "fn (", 100).is_empty());
        assert!(find("notes.txt", "x.unwrap()\n", "$A.unwrap()", 100).is_empty());
        assert!(search_file(Path::new("a.rs"), b"x.unwrap()\0", &mut Patterns::new("$A.unwrap()"), 100).is_empty());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn limit_caps_hits() {
        assert_eq!(find("a.rs", "fn f() { foo(); foo(); foo(); }", "foo()", 2).len(), 2);
    }
}
