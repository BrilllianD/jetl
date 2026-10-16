//! Symbols from tree-sitter tags queries: the extraction shared by the
//! Structure window (`panel.rs`, the active buffer's live tree) and the
//! project symbol index (`index.rs`, files parsed off the main thread).

pub mod index;
pub mod panel;

use std::sync::Arc;

use ropey::Rope;
use tree_sitter::{QueryCursor, StreamingIterator, Tree};

use crate::editor::syntax::{LanguageDef, RopeProvider};

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

/// The definitions in `tree` (parsed from `rope`) found by the language's
/// tags query, in document order with their nesting depth. References are
/// skipped; a node matched by several patterns keeps the first.
pub fn extract(def: &'static LanguageDef, tree: &Tree, rope: &Rope) -> Vec<Symbol> {
    let Some(query) = def.tags_query() else { return Vec::new() };
    let names = query.capture_names();
    let name_idx = query.capture_index_for_name("name");
    let trait_idx = query.capture_index_for_name("trait");
    // (definition byte range, pattern, symbol)
    let mut raw: Vec<(std::ops::Range<usize>, usize, Symbol)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
    while let Some(m) = matches.next() {
        let (mut node, mut kind, mut name, mut tr) = (None, "", None, None);
        for c in m.captures() {
            if Some(c.index) == name_idx {
                name = Some(c.node);
            } else if Some(c.index) == trait_idx {
                tr = Some(c.node);
            } else if let Some(k) = names[c.index as usize].strip_prefix("definition.") {
                (node, kind) = (Some(c.node), k);
            }
        }
        let (Some(node), Some(name)) = (node, name) else { continue };
        let text = |n: tree_sitter::Node| {
            let r = n.byte_range();
            let end = r.end.min(rope.len_bytes());
            clean_name(&rope.byte_slice(r.start.min(end)..end).to_string())
        };
        let mut label = text(name);
        if let Some(t) = tr {
            label = format!("{} for {label}", text(t));
        }
        if label.is_empty() {
            continue;
        }
        let at = name.start_byte().min(rope.len_bytes());
        let line = rope.byte_to_line(at);
        let col = rope.byte_to_char(at) - rope.line_to_char(line);
        let sym = Symbol {
            name: label.into(),
            kind: kind_label(kind, node.kind()),
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
    raw.into_iter()
        .map(|(r, _, mut sym)| {
            while open.last().is_some_and(|&end| end <= r.start) {
                open.pop();
            }
            sym.depth = open.len();
            open.push(r.end);
            sym
        })
        .collect()
}

/// Parse `text` with `def`'s grammar and extract its symbols (off the main
/// thread; the parser is reused across files of one language).
pub fn extract_text(def: &'static LanguageDef, parser: &mut tree_sitter::Parser, text: &str) -> Vec<Symbol> {
    if def.tags_query().is_none() || parser.set_language(&def.language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else { return Vec::new() };
    extract(def, &tree, &Rope::from_str(text))
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

    /// Names and depths of `src` parsed as `file`'s language, when enabled.
    fn names(file: &str, src: &str) -> Option<Vec<(String, &'static str, usize)>> {
        let def = crate::editor::syntax::detect(std::path::Path::new(file))?;
        let mut parser = tree_sitter::Parser::new();
        Some(extract_text(def, &mut parser, src).into_iter().map(|s| (s.name.to_string(), s.kind, s.depth)).collect())
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
