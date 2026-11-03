//! Symbols from tree-sitter tags queries: the extraction shared by the
//! Structure window (`panel.rs`, the active buffer's live tree) and the
//! project symbol index (`index.rs`, files parsed off the main thread).

pub mod imports;
pub mod index;
pub mod panel;

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use ropey::Rope;
use tree_sitter::{QueryCursor, StreamingIterator, Tree};

pub use imports::Import;

use crate::editor::syntax::{LanguageDef, RopeProvider, Syntax};

/// Names longer than this (in chars) are cut.
const MAX_NAME: usize = 120;

/// One definition: a function, type, module, ...
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: Arc<str>,
    /// `function`, `method`, `struct`, `class`, `impl`, ...
    pub kind: &'static str,
    /// 0-based line and char column of the name.
    pub line: usize,
    pub col: usize,
    /// Lines the whole definition spans (first and last, 0-based).
    pub start_line: usize,
    pub end_line: usize,
    /// How many other symbols enclose this one.
    pub depth: usize,
}

/// One use of a name found by a tags query's `@reference.*` capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: Arc<str>,
    /// The capture suffix after `reference.`: `call`, `implementation`, `class`, `type`, ...
    pub kind: &'static str,
    /// 0-based line and char column of the name.
    pub line: usize,
    pub col: usize,
    /// Index into the same file's symbols: the innermost definition containing the name.
    pub enclosing: Option<usize>,
}

/// 0-based line and char column of `byte` (clamped to the rope).
fn pos(rope: &Rope, byte: usize) -> (usize, usize) {
    let at = byte.min(rope.len_bytes());
    let line = rope.byte_to_line(at);
    (line, rope.byte_to_char(at) - rope.line_to_char(line))
}

/// The definitions in `tree` (parsed from `rope`) found by the language's
/// tags query, in document order with their nesting depth. References are
/// left out; see `extract_text_all`. A node matched by several patterns
/// keeps the first. Markdown headings inside a block quote or list item are
/// skipped.
pub fn extract(def: &'static LanguageDef, tree: &Tree, rope: &Rope) -> Vec<Symbol> {
    collect(def, tree, rope, false).0
}

/// A reference match before sorting: name range, pattern, kind, name.
type RawRef = (Range<usize>, usize, &'static str, String);

fn collect(def: &'static LanguageDef, tree: &Tree, rope: &Rope, want_refs: bool) -> (Vec<Symbol>, Vec<Reference>) {
    let Some(query) = def.tags_query() else { return Default::default() };
    let names = query.capture_names();
    let name_idx = query.capture_index_for_name("name");
    let trait_idx = query.capture_index_for_name("trait");
    let text = |n: tree_sitter::Node| {
        let r = n.byte_range();
        let end = r.end.min(rope.len_bytes());
        clean_name(&rope.byte_slice(r.start.min(end)..end).to_string())
    };
    // (definition byte range, pattern, symbol)
    let mut raw: Vec<(Range<usize>, usize, Symbol)> = Vec::new();
    // Name range and final kind of every definition match.
    let mut def_names: Vec<(Range<usize>, &'static str)> = Vec::new();
    let mut raw_refs: Vec<RawRef> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
    while let Some(m) = matches.next() {
        let (mut node, mut kind, mut name, mut tr, mut ref_kind) = (None, "", None, None, None);
        for c in m.captures() {
            if Some(c.index) == name_idx {
                name = Some(c.node);
            } else if Some(c.index) == trait_idx {
                tr = Some(c.node);
            } else if let Some(k) = names[c.index as usize].strip_prefix("definition.") {
                (node, kind) = (Some(c.node), k);
            } else if let Some(k) = names[c.index as usize].strip_prefix("reference.") {
                ref_kind = Some(k);
            }
        }
        let Some(name) = name else { continue };
        let Some(node) = node else {
            if let (true, Some(k)) = (want_refs, ref_kind) {
                let t = text(name);
                if !t.is_empty() {
                    raw_refs.push((name.byte_range(), m.pattern_index, k, t));
                }
            }
            continue;
        };
        if def.name == "markdown" && in_md_container(node) {
            continue;
        }
        let mut label = text(name);
        if let Some(t) = tr {
            label = format!("{} for {label}", text(t));
        }
        if label.is_empty() {
            continue;
        }
        let (line, col) = pos(rope, name.start_byte());
        let kind = kind_label(kind, node.kind());
        if want_refs {
            def_names.push((name.byte_range(), kind));
        }
        let sym = Symbol {
            name: label.into(),
            kind,
            line,
            col,
            start_line: rope.byte_to_line(node.start_byte().min(rope.len_bytes())),
            end_line: rope.byte_to_line(node.end_byte().min(rope.len_bytes())),
            depth: 0,
        };
        raw.push((node.byte_range(), m.pattern_index, sym));
    }
    // Outer definitions first; the first pattern matching a node wins.
    raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(b.0.end.cmp(&a.0.end)).then(a.1.cmp(&b.1)));
    raw.dedup_by(|b, a| a.0 == b.0);
    let mut open: Vec<usize> = Vec::new();
    let mut symbols = Vec::with_capacity(raw.len());
    let mut def_ranges: Vec<Range<usize>> = Vec::with_capacity(raw.len());
    for (r, _, mut sym) in raw {
        while open.last().is_some_and(|&end| end <= r.start) {
            open.pop();
        }
        sym.depth = open.len();
        open.push(r.end);
        symbols.push(sym);
        def_ranges.push(r);
    }
    if !want_refs {
        return (symbols, Vec::new());
    }
    // A definition's own name is no reference to it (an `impl` is a use of its type).
    raw_refs.retain(|r| !def_names.iter().any(|(range, kind)| *range == r.0 && *kind != "impl"));
    raw_refs.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.1.cmp(&b.1)));
    raw_refs.dedup_by(|b, a| a.0 == b.0);
    let mut interned: HashSet<Arc<str>> = HashSet::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut next = 0;
    let refs = raw_refs
        .into_iter()
        .map(|(range, _, kind, name)| {
            let b = range.start;
            while next < def_ranges.len() && def_ranges[next].start <= b {
                while stack.last().is_some_and(|e| e.0 <= def_ranges[next].start) {
                    stack.pop();
                }
                stack.push((def_ranges[next].end, next));
                next += 1;
            }
            while stack.last().is_some_and(|e| e.0 <= b) {
                stack.pop();
            }
            let name = match interned.get(name.as_str()) {
                Some(a) => a.clone(),
                None => {
                    let a: Arc<str> = Arc::from(name);
                    interned.insert(a.clone());
                    a
                }
            };
            let (line, col) = pos(rope, b);
            Reference { name, kind, line, col, enclosing: stack.last().map(|e| e.1) }
        })
        .collect();
    (symbols, refs)
}

/// The symbols whose `start_line..=end_line` contains `line`, outermost
/// first and strictly nested (each next one lies inside the previous), at
/// most one per depth; when two siblings both span `line` the later one in
/// document order wins. `symbols` is in `extract`'s order. Pure.
pub fn enclosing(symbols: &[Symbol], line: usize) -> Vec<&Symbol> {
    let mut out: Vec<&Symbol> = Vec::new();
    for s in symbols {
        if s.start_line <= line && line <= s.end_line {
            let keep = out.iter().position(|o| o.depth >= s.depth).unwrap_or(out.len());
            out.truncate(keep);
            out.push(s);
        }
    }
    out
}

/// A buffer's symbols, extracted again only when the buffer version or the
/// language changes (`Buffer::scopes`).
#[derive(Debug, Default)]
pub struct ScopeCache {
    /// Buffer version and language name the symbols were extracted for.
    key: Option<(u64, &'static str)>,
    symbols: Vec<Symbol>,
}

impl ScopeCache {
    /// The symbols for `rope` at `version`. Cheap per frame unless the
    /// version or language changed, then the tree is reparsed and walked.
    /// No syntax, or a language without a tags query, gives an empty slice.
    pub fn symbols(&mut self, syntax: Option<&mut Syntax>, rope: &Rope, version: u64) -> &[Symbol] {
        let Some(syn) = syntax else {
            self.key = None;
            self.symbols.clear();
            return &self.symbols;
        };
        let key = (version, syn.def.name);
        if self.key != Some(key) {
            self.symbols = if syn.def.tags_query().is_some() {
                syn.reparse(rope);
                syn.tree().map(|t| extract(syn.def, t, rope)).unwrap_or_default()
            } else {
                Vec::new()
            };
            self.key = Some(key);
        }
        &self.symbols
    }
}

/// Parse `text` with `def`'s grammar; `None` without a tags query, when the
/// language cannot be set or parsing fails.
fn parse_text(def: &'static LanguageDef, parser: &mut tree_sitter::Parser, text: &str) -> Option<(Tree, Rope)> {
    if def.tags_query().is_none() || parser.set_language(&def.language).is_err() {
        return None;
    }
    Some((parser.parse(text, None)?, Rope::from_str(text)))
}

/// Parse `text` with `def`'s grammar and extract its symbols (off the main
/// thread; the parser is reused across files of one language).
pub fn extract_text(def: &'static LanguageDef, parser: &mut tree_sitter::Parser, text: &str) -> Vec<Symbol> {
    parse_text(def, parser, text).map(|(t, r)| extract(def, &t, &r)).unwrap_or_default()
}

/// Like `extract_text`, with the references too (document order, no two on
/// one name range). A definition's own name is never a reference to it.
pub fn extract_text_all(def: &'static LanguageDef, parser: &mut tree_sitter::Parser, text: &str) -> (Vec<Symbol>, Vec<Reference>) {
    parse_text(def, parser, text).map(|(t, r)| collect(def, &t, &r, true)).unwrap_or_default()
}

/// Symbols, references and imports of `text` from one parse (the symbol index).
pub fn extract_index(
    def: &'static LanguageDef,
    parser: &mut tree_sitter::Parser,
    text: &str,
) -> (Vec<Symbol>, Vec<Reference>, Vec<Import>) {
    parse_text(def, parser, text)
        .map(|(tree, rope)| {
            let (symbols, refs) = collect(def, &tree, &rope, true);
            (symbols, refs, imports::extract(def, &tree, &rope))
        })
        .unwrap_or_default()
}

/// The kind shown for a definition: the capture's (`function`, `class`,
/// ...) unless the node says more (a Rust `struct` is a `class` capture).
fn kind_label(capture: &'static str, node: &str) -> &'static str {
    match node {
        "struct_item" | "struct_specifier" => "struct",
        "enum_item" | "enum_specifier" => "enum",
        "union_item" => "union",
        "trait_item" => "trait",
        "mod_item" => "mod",
        "macro_definition" => "macro",
        "type_item" | "type_spec" | "type_definition" => "type",
        _ => capture,
    }
}

/// A Markdown heading nested in a block quote or list item: the grammar
/// gives those their own sections, but they are no document structure.
fn in_md_container(node: tree_sitter::Node) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "block_quote" | "list_item") {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// One line, single spaces, at most `MAX_NAME` chars.
pub(crate) fn clean_name(s: &str) -> String {
    let mut out = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((i, _)) = out.char_indices().nth(MAX_NAME) {
        out.truncate(i);
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_definitions_with_nesting() {
        let src = "struct App {\n    x: u32,\n}\n\nimpl App {\n    pub fn new() -> Self {\n        fn helper() {}\n        App { x: helper_len() }\n    }\n}\n\nimpl Default for App {\n    fn default() -> Self { App::new() }\n}\n\nfn main() {\n    App::new();\n}\nmod inner {\n    pub const N: usize = 3;\n}\n";
        let def = crate::editor::syntax::language_for_name("rust").unwrap();
        let mut parser = tree_sitter::Parser::new();
        let syms = extract_text(def, &mut parser, src);
        let got: Vec<(&str, &str, usize, usize, usize)> = syms.iter().map(|s| (&*s.name, s.kind, s.line, s.col, s.depth)).collect();
        assert_eq!(
            got,
            [
                ("App", "struct", 0, 7, 0),
                ("App", "impl", 4, 5, 0),
                ("new", "method", 5, 11, 1),
                ("helper", "function", 6, 11, 2),
                ("Default for App", "impl", 11, 17, 0),
                ("default", "method", 12, 7, 1),
                ("main", "function", 15, 3, 0),
                ("inner", "mod", 18, 4, 0),
                ("N", "constant", 19, 14, 1),
            ]
        );
        assert_eq!((syms[1].start_line, syms[1].end_line), (4, 9));
        // References (`App::new()`, `helper_len()`) are not symbols.
        assert!(!syms.iter().any(|s| &*s.name == "helper_len"));
    }

    fn sym(name: &str, start: usize, end: usize, depth: usize) -> Symbol {
        Symbol { name: name.into(), kind: "function", line: start, col: 0, start_line: start, end_line: end, depth }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn enclosing_nests_outermost_first() {
        let src = "struct App {\n    x: u32,\n}\n\nimpl App {\n    pub fn new() -> Self {\n        fn helper() {}\n        App { x: helper_len() }\n    }\n}\n\nimpl Default for App {\n    fn default() -> Self { App::new() }\n}\n\nfn main() {\n    App::new();\n}\nmod inner {\n    pub const N: usize = 3;\n}\n";
        let def = crate::editor::syntax::language_for_name("rust").unwrap();
        let syms = extract_text(def, &mut tree_sitter::Parser::new(), src);
        let at = |l| enclosing(&syms, l).iter().map(|s| (s.kind, &*s.name)).collect::<Vec<_>>();
        assert_eq!(at(6), [("impl", "App"), ("method", "new"), ("function", "helper")]);
        assert_eq!(at(7), [("impl", "App"), ("method", "new")]);
        for blank in [3, 10, 14] {
            assert!(at(blank).is_empty(), "line {blank}");
        }
        assert_eq!(at(19), [("mod", "inner"), ("constant", "N")]);
    }

    #[test]
    fn enclosing_prefers_the_later_sibling() {
        let syms = [sym("a", 0, 5, 0), sym("a1", 4, 5, 1), sym("b", 5, 9, 0), sym("b1", 5, 6, 1)];
        let at = |l| enclosing(&syms, l).iter().map(|s| &*s.name).collect::<Vec<_>>();
        assert_eq!(at(5), ["b", "b1"]);
        assert_eq!(at(4), ["a", "a1"]);
        assert!(at(20).is_empty());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn scope_cache_extracts_once_per_version() {
        let def = crate::editor::syntax::language_for_name("rust").unwrap();
        let theme = crate::config::theme::Theme::default();
        let mut syn = Syntax::new(def, &theme).unwrap();
        let mut cache = ScopeCache::default();
        let names = |s: &[Symbol]| s.iter().map(|s| s.name.to_string()).collect::<Vec<_>>();
        let one = Rope::from_str("fn a() {}\n");
        assert_eq!(names(cache.symbols(Some(&mut syn), &one, 1)), ["a"]);
        // Same version: cached, even for another syntax and text.
        let mut other = Syntax::new(def, &theme).unwrap();
        let two = Rope::from_str("fn a() {}\nfn b() {}\n");
        assert_eq!(names(cache.symbols(Some(&mut other), &two, 1)), ["a"]);
        assert_eq!(names(cache.symbols(Some(&mut other), &two, 2)), ["a", "b"]);
        assert!(cache.symbols(None, &two, 2).is_empty());
        // A language without a tags query has no symbols.
        if let Some(plain) = crate::editor::syntax::language_for_name("toml").filter(|d| d.tags_query().is_none()) {
            let mut syn = Syntax::new(plain, &theme).unwrap();
            assert!(cache.symbols(Some(&mut syn), &two, 3).is_empty());
        }
    }

    type RefRow = (String, &'static str, usize, usize, Option<usize>);

    fn refs(file: &str, src: &str) -> Option<(Vec<Symbol>, Vec<RefRow>)> {
        let def = crate::editor::syntax::detect(std::path::Path::new(file))?;
        let (syms, refs) = extract_text_all(def, &mut tree_sitter::Parser::new(), src);
        Some((syms, refs.into_iter().map(|r| (r.name.to_string(), r.kind, r.line, r.col, r.enclosing)).collect()))
    }

    fn row(n: &str, k: &'static str, l: usize, c: usize, e: Option<usize>) -> RefRow {
        (n.to_string(), k, l, c, e)
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_references_with_enclosing() {
        let src = "struct App;\nimpl App {\n    fn new() -> Self { helper(); App }\n}\nfn main() {\n    let a = App::new();\n    a.run();\n    println!(\"x\");\n}\n";
        let (syms, got) = refs("a.rs", src).unwrap();
        let sk: Vec<(&str, &str)> = syms.iter().map(|s| (&*s.name, s.kind)).collect();
        assert_eq!(sk, [("App", "struct"), ("App", "impl"), ("new", "method"), ("main", "function")]);
        assert_eq!(
            got,
            [
                row("App", "implementation", 1, 5, Some(1)),
                row("helper", "call", 2, 23, Some(2)),
                row("run", "call", 6, 6, Some(3)),
                row("println", "call", 7, 4, Some(3))
            ]
        );
        assert!(!got.iter().any(|r| r.0 == "new"));
        let def = crate::editor::syntax::language_for_name("rust").unwrap();
        assert_eq!(extract_text(def, &mut tree_sitter::Parser::new(), src), syms);
    }

    #[test]
    fn python_references() {
        let Some((_, got)) = refs("a.py", "class A:\n    def m(self):\n        f()\n\ndef f():\n    A().m()\n") else { return };
        assert_eq!(got, [row("f", "call", 2, 8, Some(1)), row("A", "call", 5, 4, Some(2)), row("m", "call", 5, 8, Some(2))]);
    }

    #[test]
    fn c_references_skip_definition_names() {
        let Some((_, got)) = refs("a.c", "typedef int T;\nstruct S { T x; };\nint f(struct S *p) { return g(p->x) + p->cb(); }\n") else {
            return;
        };
        assert_eq!(
            got,
            [
                row("T", "type", 1, 11, Some(1)),
                row("S", "type", 2, 13, Some(2)),
                row("g", "call", 2, 28, None),
                row("cb", "call", 2, 41, None)
            ]
        );
    }

    #[test]
    fn cpp_references() {
        let Some((_, got)) = refs("a.cpp", "struct P {};\nclass A { P p; void m(); };\nvoid A::m() { n::g(); this->m(); t<int>(); }\n")
        else {
            return;
        };
        let short: Vec<(&str, &str, usize, usize)> = got.iter().map(|r| (r.0.as_str(), r.1, r.2, r.3)).collect();
        assert_eq!(short, [("P", "type", 1, 10), ("g", "call", 2, 17), ("m", "call", 2, 28), ("t", "call", 2, 33)]);
    }

    #[test]
    fn go_type_definition_name_is_no_reference() {
        let Some((_, got)) = refs("a.go", "package p\ntype T struct{}\nfunc F(t T) { F(t) }\n") else { return };
        let of = |n: &str| got.iter().filter(|r| r.0 == n).map(|r| (r.1, r.2)).collect::<Vec<_>>();
        assert_eq!(of("T"), [("type", 2)]);
        assert_eq!(of("F"), [("call", 2)]);
    }

    /// Names and depths of `src` parsed as `file`'s language, when enabled.
    fn names(file: &str, src: &str) -> Option<Vec<(String, &'static str, usize)>> {
        let def = crate::editor::syntax::detect(std::path::Path::new(file))?;
        let mut parser = tree_sitter::Parser::new();
        Some(extract_text(def, &mut parser, src).into_iter().map(|s| (s.name.to_string(), s.kind, s.depth)).collect())
    }

    #[test]
    fn markdown_headings_nest_by_level() {
        let src = "intro\n\n# Top\n\ntext\n\n## Sub\n\n### Deep\n\nbody\n\n## Sub2\n\nSetext two\n----------\n\n# Other\n";
        let Some(got) = names("a.md", src) else { return };
        let want = [("Top", 0), ("Sub", 1), ("Deep", 2), ("Sub2", 1), ("Setext two", 2), ("Other", 0)];
        let want: Vec<(String, &str, usize)> = want.iter().map(|(n, d)| (n.to_string(), "heading", *d)).collect();
        assert_eq!(got, want);
        // A setext heading before any ATX heading is top level; a bare `#` has no name.
        let got = names("b.md", "Title\n=====\n\n#\n\ntext\n").unwrap();
        assert_eq!(got, [("Title".to_string(), "heading", 0)]);
        // Headings inside a block quote or list item are no symbols (setext).
        let got = names("c.md", "> T\n> ===\n\n- U\n  ---\n").unwrap();
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn markdown_headings_in_quotes_and_lists_are_skipped() {
        for src in ["> # Q\n", "> # Q\n>\n> T\n> ===\n", "- # Item\n", "- a\n\n  ## Sub\n"] {
            let Some(got) = names("a.md", src) else { return };
            assert!(got.is_empty(), "{src:?}: {got:?}");
        }
        // A heading after the quote, outside it, is still a top-level symbol.
        let got = names("b.md", "> # Q\n\n# Real\n\n## Sub\n").unwrap();
        assert_eq!(got, [("Real".to_string(), "heading", 0), ("Sub".to_string(), "heading", 1)]);
    }

    #[test]
    fn other_languages_find_their_definitions() {
        type Want = &'static [(&'static str, &'static str, usize)];
        let cases: [(&str, &str, Want); 8] = [
            (
                "a.py",
                "class A:\n    def m(self):\n        pass\n\ndef f():\n    m()\n",
                &[("A", "class", 0), ("m", "function", 1), ("f", "function", 0)],
            ),
            ("a.js", "class A { m() {} }\nfunction f() { g(); }\n", &[("A", "class", 0), ("m", "method", 1), ("f", "function", 0)]),
            (
                "a.ts",
                "interface I { x(): void }\nclass A { m() {} }\nfunction f(): void {}\n",
                &[("I", "interface", 0), ("x", "method", 1), ("A", "class", 0), ("m", "method", 1), ("f", "function", 0)],
            ),
            (
                "a.go",
                "package p\ntype T struct{}\nfunc (t T) M() {}\nfunc F() { F() }\n",
                &[("T", "type", 0), ("M", "method", 0), ("F", "function", 0)],
            ),
            ("a.c", "struct S { int x; };\nint f(void) { return g(); }\n", &[("S", "struct", 0), ("f", "function", 0)]),
            ("a.cpp", "class A { void m(); };\nvoid A::m() {}\n", &[("A", "class", 0), ("m", "function", 1), ("m", "method", 0)]),
            ("a.java", "class A { void m() { n(); } }\ninterface I {}\n", &[("A", "class", 0), ("m", "method", 1), ("I", "interface", 0)]),
            ("a.tsx", "function C() { return <div/>; }\n", &[("C", "function", 0)]),
        ];
        for (file, src, want) in cases {
            let Some(got) = names(file, src) else { continue };
            let want: Vec<(String, &str, usize)> = want.iter().map(|(n, k, d)| (n.to_string(), *k, *d)).collect();
            assert_eq!(got, want, "{file}");
        }
    }

    #[test]
    fn names_are_one_line_and_bounded() {
        assert_eq!(clean_name("Foo<\n    T,\n>"), "Foo< T, >");
        assert_eq!(clean_name(&"x".repeat(200)).chars().count(), MAX_NAME + 1);
    }
}
