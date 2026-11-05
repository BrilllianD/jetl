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
use super::scip::{Occ, ScipIndex, Span, SymId};
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

/// The files on an import edge of `from`, either way.
fn near_of(index: &mut SymbolIndex, from: &str) -> HashSet<Arc<str>> {
    index.imports_of(from).into_iter().chain(index.imported_by(from)).collect()
}

/// `resolve` with the near files of `from` already known.
fn pick(index: &mut SymbolIndex, name: &str, from: &str, near: &HashSet<Arc<str>>) -> Vec<Def> {
    let found = index.defs_of(name);
    let group = |path: &str| {
        if path == from {
            0u8
        } else if near.contains(path) {
            1
        } else {
            2
        }
    };
    let Some(best) = found.iter().map(|(p, _)| group(p)).min() else { return Vec::new() };
    let mut defs: Vec<Def> = found.into_iter().filter(|(p, _)| group(p) == best).map(|(path, symbol)| Def { path, symbol }).collect();
    defs.sort_by(|a, b| a.path.cmp(&b.path).then(a.symbol.line.cmp(&b.symbol.line)).then(a.symbol.col.cmp(&b.symbol.col)));
    defs
}

/// Definitions named `name` as seen from file `from` (best group only), by path, line, col; never `impl`.
pub fn resolve(index: &mut SymbolIndex, name: &str, from: &str) -> Vec<Def> {
    let near = near_of(index, from);
    pick(index, name, from, &near)
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
    sort_defs(&mut out);
    out
}

/// Sorts by path, line, col and drops repeats of the same definition (what `callers` did inline).
pub fn sort_defs(defs: &mut Vec<Def>) {
    defs.sort_by(|a, b| a.path.cmp(&b.path).then(a.symbol.line.cmp(&b.symbol.line)).then(a.symbol.col.cmp(&b.symbol.col)));
    defs.dedup_by(|a, b| a.path == b.path && a.symbol.line == b.symbol.line && a.symbol.col == b.symbol.col);
}

/// The tags symbol (index into `rel`'s symbols) that SCIP definition `def` of `rel` names: the one whose
/// `line` is the definition's line, never `impl`; when several, the one named `scip.name(def.symbol)`, else the first.
pub fn scip_def_symbol(index: &SymbolIndex, scip: &ScipIndex, rel: &str, def: &Occ) -> Option<usize> {
    let f = index.file(rel)?;
    let line = def.span.line as usize;
    let mut on_line = f.symbols.iter().enumerate().filter(|(_, s)| s.line == line && s.kind != "impl").map(|(i, _)| i);
    let first = on_line.next()?;
    let Some(name) = scip.name(def.symbol) else { return Some(first) };
    if *f.symbols[first].name == *name {
        return Some(first);
    }
    Some(on_line.find(|&i| *f.symbols[i].name == *name).unwrap_or(first))
}

/// `(enclosing range, tags symbol)` of every SCIP definition of `rel` that has an enclosing range and maps
/// through `scip_def_symbol`. Computed once per document.
fn bodies(index: &SymbolIndex, scip: &ScipIndex, rel: &str) -> Vec<(Span, usize)> {
    let Some(doc) = scip.doc(rel) else { return Vec::new() };
    doc.occurrences.iter().filter(|o| o.definition).filter_map(|o| Some((o.enclosing?, scip_def_symbol(index, scip, rel, o)?))).collect()
}

/// The tags symbol whose definition holds the occurrence at `span`: the innermost body (latest start; on a tie the
/// earliest end) with `(b.line, b.start) <= (span.line, span.start) < (b.end_line, b.end)`; without one, `caller_of`
/// on a synthetic `Reference { name: Arc::from(""), kind: "call", line: span.line as usize, col: 0, enclosing: None }`.
fn container(bodies: &[(Span, usize)], lang: Option<&str>, symbols: &[Symbol], span: &Span) -> Option<usize> {
    let at = (span.line, span.start);
    let inner = bodies
        .iter()
        .filter(|(b, _)| (b.line, b.start) <= at && at < (b.end_line, b.end))
        .max_by_key(|(b, _)| ((b.line, b.start), std::cmp::Reverse((b.end_line, b.end))));
    if let Some(&(_, i)) = inner {
        return Some(i);
    }
    let r = Reference { name: Arc::from(""), kind: "call", line: span.line as usize, col: 0, enclosing: None };
    caller_of(lang, symbols, &r)
}

/// Definitions that reference SCIP symbol `sym`, from documents `covered` accepts: per non-definition occurrence its
/// container (see below), one per definition, sorted with `sort_defs`.
pub fn scip_callers(index: &SymbolIndex, scip: &ScipIndex, sym: SymId, covered: &mut dyn FnMut(&str) -> bool) -> Vec<Def> {
    let mut out: Vec<Def> = Vec::new();
    let mut cache: Option<(&str, Vec<(Span, usize)>)> = None;
    for (rel, o) in scip.occurrences_of(sym) {
        if o.definition || !covered(rel) {
            continue;
        }
        let Some(f) = index.file(rel) else { continue };
        if cache.as_ref().is_none_or(|(p, _)| *p != rel) {
            cache = Some((rel, bodies(index, scip, rel)));
        }
        let b = &cache.as_ref().expect("set above").1;
        if let Some(i) = container(b, language_of(rel), &f.symbols, &o.span) {
            out.push(Def { path: Arc::from(rel), symbol: f.symbols[i].clone() });
        }
    }
    sort_defs(&mut out);
    out
}

/// The non-local SCIP symbols referenced (non-definition occurrences) inside tags symbol `sym` of `rel`, in
/// position order, each once. The caller checks that `rel` is covered.
pub fn scip_calls_in(index: &SymbolIndex, scip: &ScipIndex, rel: &str, sym: usize) -> Vec<SymId> {
    let Some(f) = index.file(rel) else { return Vec::new() };
    let b = bodies(index, scip, rel);
    let lang = language_of(rel);
    let Some(doc) = scip.doc(rel) else { return Vec::new() };
    // Lines `container` can map to `sym`: its tags range and its SCIP bodies; C and C++ fall back to the last
    // callable above the line, so only the lower bound holds there.
    let s = &f.symbols[sym];
    let mut lo = s.start_line.min(s.line);
    let mut hi = s.end_line;
    for (span, _) in b.iter().filter(|(_, i)| *i == sym) {
        lo = lo.min(span.line as usize);
        hi = hi.max(span.end_line as usize);
    }
    if matches!(lang, Some("c" | "cpp")) {
        hi = usize::MAX;
    }
    let mut out: Vec<SymId> = Vec::new();
    for o in &doc.occurrences {
        let line = o.span.line as usize;
        if line < lo || line > hi || o.definition || scip.is_local(o.symbol) || out.contains(&o.symbol) {
            continue;
        }
        if container(&b, lang, &f.symbols, &o.span) == Some(sym) {
            out.push(o.symbol);
        }
    }
    out
}

/// The callable definitions of SCIP symbol `id`: from covered documents mapped with `scip_def_symbol` and kept when
/// the tags kind is in `CALLABLE`; and the paths (each once, in order) of definitions in documents `covered` rejects.
pub fn scip_defs(index: &SymbolIndex, scip: &ScipIndex, id: SymId, covered: &mut dyn FnMut(&str) -> bool) -> (Vec<Def>, Vec<String>) {
    let mut defs = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    for (rel, o) in scip.occurrences_of(id) {
        if !o.definition {
            continue;
        }
        if !covered(rel) {
            if !stale.iter().any(|p| p == rel) {
                stale.push(rel.to_string());
            }
            continue;
        }
        let Some(f) = index.file(rel) else { continue };
        if let Some(i) = scip_def_symbol(index, scip, rel, o)
            && CALLABLE.contains(&f.symbols[i].kind)
        {
            defs.push(Def { path: Arc::from(rel), symbol: f.symbols[i].clone() });
        }
    }
    (defs, stale)
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
    if names.is_empty() {
        return Vec::new();
    }
    let near = near_of(index, path);
    let mut seen: HashSet<(Arc<str>, usize, usize)> = HashSet::new();
    let mut out = Vec::new();
    for name in names {
        for d in pick(index, &name, path, &near) {
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
    fn resolve_skips_impl_and_sorts_within_the_best_group() {
        let mut idx = indexed(&[("src/a.rs", "struct A;\nimpl A {}\n"), ("src/z.rs", "struct A;\n"), ("src/m.rs", "fn other() {}\n")]);
        assert_eq!(names(&resolve(&mut idx, "A", "src/m.rs")), pairs(&[("src/a.rs", "A"), ("src/z.rs", "A")]));
        assert_eq!(names(&resolve(&mut idx, "A", "src/z.rs")), pairs(&[("src/z.rs", "A")]));
        assert!(resolve(&mut idx, "nothing", "src/m.rs").is_empty());
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

    /// `(range, symbol, is definition, enclosing range)` per occurrence, per document.
    type Occs<'a> = &'a [(&'a [i32], &'a str, bool, &'a [i32])];

    fn scip_of(docs: &[(&str, Occs)]) -> ScipIndex {
        use ::scip::types::{Document, Index, Occurrence, SymbolRole};
        let mut index = Index::new();
        for (path, occs) in docs {
            let mut d = Document::new();
            d.relative_path = (*path).to_string();
            for (range, symbol, def, enclosing) in *occs {
                let mut o = Occurrence::new();
                o.range = range.to_vec();
                o.symbol = (*symbol).to_string();
                o.symbol_roles = if *def { SymbolRole::Definition as i32 } else { 0 };
                o.enclosing_range = enclosing.to_vec();
                d.occurrences.push(o);
            }
            index.documents.push(d);
        }
        ScipIndex::from_proto(index, Path::new("/nonexistent"), PathBuf::from("/nonexistent/index.scip"), std::time::SystemTime::UNIX_EPOCH)
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn scip_callers_follow_the_symbol_not_the_name() {
        let main = "mod a;\nmod b;\nuse b::helper;\n\nfn main() {\n    helper();\n}\n\nfn run() {\n    other();\n}\n";
        let idx = indexed(&[
            ("src/main.rs", main),
            ("src/a.rs", "pub fn helper() {}\n"),
            ("src/b.rs", "pub fn helper() {}\npub fn other() {}\n"),
        ]);
        let scip = scip_of(&[
            (
                "src/main.rs",
                &[
                    (&[4, 3, 7], "t . . . main().", true, &[]),
                    (&[8, 3, 6], "t . . . run().", true, &[]),
                    (&[2, 7, 13], "t . . . b/helper().", false, &[]),
                    (&[5, 4, 10], "t . . . b/helper().", false, &[]),
                    (&[9, 4, 9], "t . . . b/other().", false, &[]),
                ],
            ),
            ("src/a.rs", &[(&[0, 7, 13], "t . . . a/helper().", true, &[])]),
            ("src/b.rs", &[(&[0, 7, 13], "t . . . b/helper().", true, &[]), (&[1, 7, 12], "t . . . b/other().", true, &[])]),
        ]);
        let id = |s: &str| scip.symbol_id(s).unwrap();
        let all = &mut |_: &str| true;
        assert_eq!(names(&scip_callers(&idx, &scip, id("t . . . b/helper()."), all)), pairs(&[("src/main.rs", "main")]));
        assert!(scip_callers(&idx, &scip, id("t . . . a/helper()."), all).is_empty());
        assert_eq!(names(&callers(&idx, "helper")), pairs(&[("src/main.rs", "main")]));
        assert_eq!(names(&scip_callers(&idx, &scip, id("t . . . b/other()."), all)), pairs(&[("src/main.rs", "run")]));
        let not_main = &mut |r: &str| r != "src/main.rs";
        assert!(scip_callers(&idx, &scip, id("t . . . b/helper()."), not_main).is_empty());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn scip_enclosing_ranges_win_over_tags_lines() {
        let text = "fn a() {}\nfn b() {\n    helper();\n}\nfn helper() {}\n";
        let idx = indexed(&[("src/m.rs", text)]);
        let docs = |a_enclosing: &'static [i32]| {
            scip_of(&[(
                "src/m.rs",
                &[
                    (&[0, 3, 4], "t . . . a().", true, a_enclosing),
                    (&[1, 3, 4], "t . . . b().", true, &[]),
                    (&[4, 3, 9], "t . . . helper().", true, &[]),
                    (&[2, 4, 10], "t . . . helper().", false, &[]),
                ],
            )])
        };
        let all = &mut |_: &str| true;
        let wide = docs(&[0, 0, 3, 1]);
        let id = wide.symbol_id("t . . . helper().").unwrap();
        assert_eq!(names(&scip_callers(&idx, &wide, id, all)), pairs(&[("src/m.rs", "a")]));
        let plain = docs(&[]);
        let id = plain.symbol_id("t . . . helper().").unwrap();
        assert_eq!(names(&scip_callers(&idx, &plain, id, all)), pairs(&[("src/m.rs", "b")]));
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn scip_calls_and_defs_keep_callables_in_order() {
        let main = "mod b;\nstruct S;\nfn m() {\n    let x = S;\n    b::y();\n    b::z();\n    b::y();\n}\n";
        let idx = indexed(&[("src/main.rs", main), ("src/b.rs", "pub fn y() {}\npub fn z() {}\n"), ("src/c.rs", "pub fn z() {}\n")]);
        let scip = scip_of(&[
            (
                "src/main.rs",
                &[
                    (&[1, 7, 8], "t . . . S#", true, &[]),
                    (&[2, 3, 4], "t . . . m().", true, &[]),
                    (&[3, 8, 9], "local 0", true, &[]),
                    (&[3, 12, 13], "t . . . S#", false, &[]),
                    (&[4, 7, 8], "t . . . b/y().", false, &[]),
                    (&[5, 7, 8], "t . . . z().", false, &[]),
                    (&[6, 7, 8], "t . . . b/y().", false, &[]),
                ],
            ),
            ("src/b.rs", &[(&[0, 7, 8], "t . . . b/y().", true, &[]), (&[1, 7, 8], "t . . . z().", true, &[])]),
            ("src/c.rs", &[(&[0, 7, 8], "t . . . z().", true, &[])]),
        ]);
        let id = |s: &str| scip.symbol_id(s).unwrap();
        let (s, y, z) = (id("t . . . S#"), id("t . . . b/y()."), id("t . . . z()."));
        let m = find(&idx, "src/main.rs", "m", 2).unwrap();
        assert_eq!(scip_calls_in(&idx, &scip, "src/main.rs", m), [s, y, z]);
        let all = &mut |_: &str| true;
        assert_eq!(scip_defs(&idx, &scip, s, all), (Vec::new(), Vec::new()));
        let (defs, stale) = scip_defs(&idx, &scip, y, all);
        assert_eq!((names(&defs), stale), (pairs(&[("src/b.rs", "y")]), Vec::new()));
        let not_c = &mut |r: &str| r != "src/c.rs";
        let (defs, stale) = scip_defs(&idx, &scip, z, not_c);
        assert_eq!((names(&defs), stale), (pairs(&[("src/b.rs", "z")]), vec!["src/c.rs".to_string()]));
    }

    #[test]
    fn lsp_kind_maps_tags_kinds() {
        assert_eq!(lsp_kind("method"), SymbolKind::METHOD);
        assert_eq!(lsp_kind("struct"), SymbolKind::STRUCT);
        assert_eq!(lsp_kind("trait"), SymbolKind::INTERFACE);
        assert_eq!(lsp_kind("unknown"), SymbolKind::FUNCTION);
    }
}
