//! Approximate call edges from the symbol index, for the Hierarchy window
//! without a language server: a call is a `@reference.call` capture, its
//! caller the definition around it, its callee every definition with its
//! name, preferring the caller's file, then files on an import edge. Names
//! are not resolved; results are candidates (titles say approximate).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use lsp_types::SymbolKind;

use super::index::SymbolIndex;
use super::{Reference, Symbol};
use crate::editor::syntax;

/// Tags kinds that are functions for the C/C++ caller rule.
pub const CALLABLE: [&str; 3] = ["function", "method", "constructor"];

/// A definition found in the index: its file (relative to the root) and its symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub path: Arc<str>,
    pub symbol: Symbol,
}

/// The innermost symbol whose lines hold `line` (the deepest, the later on a tie).
fn innermost(symbols: &[Symbol], line: usize) -> Option<usize> {
    symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| s.start_line <= line && line <= s.end_line)
        .max_by_key(|&(i, s)| (s.depth, i))
        .map(|(i, _)| i)
}

fn language_of(path: &str) -> Option<&'static str> {
    syntax::detect(Path::new(path)).map(|d| d.name)
}

/// The index in `symbols` of the definition that calls through reference `r` (same file).
/// In C and C++ a function symbol spans only its declarator, so a call that no
/// function encloses belongs to the last function at or before its line.
pub fn caller_of(lang: Option<&str>, symbols: &[Symbol], r: &Reference) -> Option<usize> {
    let container = r.enclosing.filter(|&i| i < symbols.len()).or_else(|| innermost(symbols, r.line));
    if matches!(lang, Some("c" | "cpp")) && container.is_none_or(|c| !CALLABLE.contains(&symbols[c].kind)) {
        let from = container.map_or(0, |c| c + 1);
        if let Some(j) = (from..symbols.len()).rev().find(|&j| CALLABLE.contains(&symbols[j].kind) && symbols[j].line <= r.line) {
            return Some(j);
        }
    }
    container
}

/// The symbol of file `path` named `name` whose name is on `line` (the first such).
pub fn find(index: &SymbolIndex, path: &str, name: &str, line: usize) -> Option<usize> {
    index.file(path)?.symbols.iter().position(|s| &*s.name == name && s.line == line)
}

/// Definitions named `name` as seen from file `from` (best group only), by path, line, col; never `impl`.
pub fn resolve(index: &mut SymbolIndex, name: &str, from: &str) -> Vec<Def> {
    let near: HashSet<Arc<str>> = index.imports_of(from).into_iter().chain(index.imported_by(from)).collect();
    let mut found: Vec<(u8, Def)> = Vec::new();
    for (path, f) in index.files() {
        let group = if path == from {
            0
        } else if near.contains(path) {
            1
        } else {
            2
        };
        for s in f.symbols.iter().filter(|s| &*s.name == name && s.kind != "impl") {
            found.push((group, Def { path: Arc::from(path), symbol: s.clone() }));
        }
    }
    let Some(best) = found.iter().map(|(g, _)| *g).min() else { return Vec::new() };
    let mut defs: Vec<Def> = found.into_iter().filter(|(g, _)| *g == best).map(|(_, d)| d).collect();
    defs.sort_by(|a, b| a.path.cmp(&b.path).then(a.symbol.line.cmp(&b.symbol.line)).then(a.symbol.col.cmp(&b.symbol.col)));
    defs
}

/// Hierarchy roots for `name` at `line` of `from`.
pub fn roots(index: &mut SymbolIndex, name: &str, from: &str, line: usize) -> Vec<Def> {
    let defs = resolve(index, name, from);
    if defs.len() > 1 && defs.first().is_some_and(|d| &*d.path == from) {
        let around = defs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.symbol.start_line <= line && line <= d.symbol.end_line)
            .max_by_key(|&(i, d)| (d.symbol.depth, i))
            .map(|(i, _)| i);
        if let Some(i) = around {
            return vec![defs[i].clone()];
        }
    }
    defs
}

/// Definitions that call `name`, one per definition, by path, line, col.
pub fn callers(index: &SymbolIndex, name: &str) -> Vec<Def> {
    let mut out: Vec<Def> = Vec::new();
    for (path, r) in index.refs_named(name) {
        if r.kind != "call" {
            continue;
        }
        let Some(f) = index.file(path) else { continue };
        if let Some(i) = caller_of(language_of(path), &f.symbols, r) {
            out.push(Def { path: Arc::from(path), symbol: f.symbols[i].clone() });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.symbol.line.cmp(&b.symbol.line)).then(a.symbol.col.cmp(&b.symbol.col)));
    out.dedup_by(|a, b| a.path == b.path && a.symbol.line == b.symbol.line && a.symbol.col == b.symbol.col);
    out
}

/// Definitions called from symbol `sym` of file `path`, in call order, one per definition.
pub fn callees(index: &mut SymbolIndex, path: &str, sym: usize) -> Vec<Def> {
    let mut names: Vec<Arc<str>> = Vec::new();
    if let Some(f) = index.file(path) {
        let lang = language_of(path);
        for r in f.refs.iter().filter(|r| r.kind == "call" && caller_of(lang, &f.symbols, r) == Some(sym)) {
            if !names.contains(&r.name) {
                names.push(r.name.clone());
            }
        }
    }
    let mut seen: HashSet<(Arc<str>, usize, usize)> = HashSet::new();
    let mut out = Vec::new();
    for name in names {
        for d in resolve(index, &name, path) {
            if seen.insert((d.path.clone(), d.symbol.line, d.symbol.col)) {
                out.push(d);
            }
        }
    }
    out
}

/// The LSP kind shown for a tags kind.
pub fn lsp_kind(kind: &str) -> SymbolKind {
    match kind {
        "method" => SymbolKind::METHOD,
        "constructor" => SymbolKind::CONSTRUCTOR,
        "class" | "impl" => SymbolKind::CLASS,
        "struct" | "union" => SymbolKind::STRUCT,
        "enum" => SymbolKind::ENUM,
        "interface" | "trait" => SymbolKind::INTERFACE,
        "module" | "mod" => SymbolKind::MODULE,
        "namespace" => SymbolKind::NAMESPACE,
        "constant" => SymbolKind::CONSTANT,
        "field" => SymbolKind::FIELD,
        "property" => SymbolKind::PROPERTY,
        "type" => SymbolKind::TYPE_PARAMETER,
        _ => SymbolKind::FUNCTION,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::symbols::index::{FileSymbols, SymbolBatch};

    fn indexed(files: &[(&str, &str)]) -> SymbolIndex {
        let mut parser = tree_sitter::Parser::new();
        let files = files
            .iter()
            .map(|(rel, text)| {
                let def = syntax::detect(Path::new(rel)).unwrap();
                let (symbols, refs, imports) = crate::symbols::extract_index(def, &mut parser, text);
                (rel.to_string(), Some(FileSymbols { mtime: None, size: 0, symbols, refs, imports }))
            })
            .collect();
        let mut idx = SymbolIndex::default();
        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        idx.on_batch(seq, SymbolBatch { files, done: true, ..Default::default() });
        idx
    }

    fn names(defs: &[Def]) -> Vec<(String, String)> {
        defs.iter().map(|d| (d.path.to_string(), d.symbol.name.to_string())).collect()
    }

    fn pairs(want: &[(&str, &str)]) -> Vec<(String, String)> {
        want.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn callers_are_the_enclosing_definitions() {
        let main = "mod b;\nuse b::helper;\n\nfn main() {\n    helper();\n    run();\n    helper();\n}\n\nfn run() {\n    fn inner() { helper(); }\n    inner();\n}\n";
        let idx = indexed(&[("src/main.rs", main), ("src/b.rs", "pub fn helper() {}\n")]);
        assert_eq!(names(&callers(&idx, "helper")), pairs(&[("src/main.rs", "main"), ("src/main.rs", "inner")]));
        assert!(callers(&idx, "nothing").is_empty());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn callees_resolve_same_file_then_imports_then_rest() {
        let main = "mod b;\nfn x() {}\nfn m() {\n    x();\n    y();\n    z();\n    x();\n    println!(\"\");\n}\n";
        let mut idx = indexed(&[
            ("src/main.rs", main),
            ("src/b.rs", "pub fn y() {}\n"),
            ("src/c.rs", "pub fn x() {}\npub fn y() {}\npub fn z() {}\n"),
            ("src/d.rs", "pub fn z() {}\n"),
        ]);
        let imports: Vec<String> = idx.imports_of("src/main.rs").iter().map(|p| p.to_string()).collect();
        assert_eq!(imports.first().map(String::as_str), Some("src/b.rs"), "import resolution");
        let m = find(&idx, "src/main.rs", "m", 2).unwrap();
        assert_eq!(
            names(&callees(&mut idx, "src/main.rs", m)),
            pairs(&[("src/main.rs", "x"), ("src/b.rs", "y"), ("src/c.rs", "z"), ("src/d.rs", "z")])
        );
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn roots_prefer_the_definition_around_the_caret() {
        let a = "fn f() {}\nstruct A;\nimpl A {\n    fn f(&self) {\n        f();\n    }\n}\n";
        let mut idx = indexed(&[("src/a.rs", a), ("src/b.rs", "fn f() {}\n")]);
        let lines = |defs: &[Def]| defs.iter().map(|d| (d.path.to_string(), d.symbol.line)).collect::<Vec<_>>();
        assert_eq!(lines(&roots(&mut idx, "f", "src/a.rs", 4)), [("src/a.rs".to_string(), 3)]);
        assert_eq!(lines(&roots(&mut idx, "f", "src/a.rs", 1)), [("src/a.rs".to_string(), 0), ("src/a.rs".to_string(), 3)]);
        assert_eq!(
            lines(&roots(&mut idx, "f", "src/c.rs", 0)),
            [("src/a.rs".to_string(), 0), ("src/a.rs".to_string(), 3), ("src/b.rs".to_string(), 0)]
        );
        let callers = callers(&idx, "f");
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].symbol.line, 3);
    }

    #[cfg(feature = "lang-c")]
    #[test]
    fn c_bodies_belong_to_the_declarator_before_them() {
        let src = "int helper(int x)\n{\n    return x;\n}\n\nint main(void)\n{\n    int y = 2;\n    return helper(y);\n}\n";
        let mut idx = indexed(&[("a.c", src)]);
        let call = idx.file("a.c").unwrap().refs.iter().find(|r| &*r.name == "helper").unwrap();
        assert_eq!(call.enclosing, None);
        assert_eq!(names(&callers(&idx, "helper")), pairs(&[("a.c", "main")]));
        let main = find(&idx, "a.c", "main", 5).unwrap();
        assert_eq!(names(&callees(&mut idx, "a.c", main)), pairs(&[("a.c", "helper")]));
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn calls_outside_definitions_have_no_caller() {
        let idx = indexed(&[("m.py", "def g():\n    pass\n\ndef f():\n    g()\n\ng()\n")]);
        assert_eq!(names(&callers(&idx, "g")), pairs(&[("m.py", "f")]));
    }

    #[test]
    fn lsp_kind_maps_tags_kinds() {
        assert_eq!(lsp_kind("method"), SymbolKind::METHOD);
        assert_eq!(lsp_kind("struct"), SymbolKind::STRUCT);
        assert_eq!(lsp_kind("trait"), SymbolKind::INTERFACE);
        assert_eq!(lsp_kind("unknown"), SymbolKind::FUNCTION);
    }
}
