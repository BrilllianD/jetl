//! `textDocument/documentSymbol` answers as rows for the File Structure
//! palette: a tree of `DocumentSymbol`s walked depth first, or a flat list
//! of `SymbolInformation`s nested by their ranges.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use lsp_types::{DocumentSymbol, DocumentSymbolResponse, OneOf, Position, Range, SymbolKind, WorkspaceSymbolResponse};
use serde_json::Value;

use crate::symbols::Symbol;
use crate::symbols::index::SymbolEntry;

/// Rows of a `workspace/symbol` answer kept per server.
const MAX_WORKSPACE_ENTRIES: usize = 500;

/// A symbol in a file: how deep it is nested, its kind and name, and where
/// its name is (UTF-16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub depth: usize,
    pub kind: SymbolKind,
    pub name: String,
    pub pos: Position,
}

/// The rows of an answer, in text order (parents before their children).
pub fn rows(v: Value) -> Vec<SymbolRow> {
    let mut out = Vec::new();
    match serde_json::from_value::<Option<DocumentSymbolResponse>>(v) {
        Ok(Some(DocumentSymbolResponse::Nested(symbols))) => walk(&symbols, 0, &mut out),
        Ok(Some(DocumentSymbolResponse::Flat(mut symbols))) => {
            let key = |p: Position| (p.line, p.character);
            symbols.sort_by_key(|s| (key(s.location.range.start), std::cmp::Reverse(key(s.location.range.end))));
            // The ranges of the symbols the current one may be inside.
            let mut open: Vec<Range> = Vec::new();
            for s in symbols {
                let r = s.location.range;
                while open.last().is_some_and(|o| key(o.end) < key(r.end) || key(o.start) > key(r.start)) {
                    open.pop();
                }
                out.push(SymbolRow { depth: open.len(), kind: s.kind, name: s.name, pos: r.start });
                open.push(r);
            }
        }
        Ok(None) => {}
        Err(e) => tracing::debug!("unexpected document symbols: {e}"),
    }
    out
}

fn walk(symbols: &[DocumentSymbol], depth: usize, out: &mut Vec<SymbolRow>) {
    let mut sorted: Vec<&DocumentSymbol> = symbols.iter().collect();
    sorted.sort_by_key(|s| (s.selection_range.start.line, s.selection_range.start.character));
    for s in sorted {
        out.push(SymbolRow { depth, kind: s.kind, name: s.name.clone(), pos: s.selection_range.start });
        walk(s.children.as_deref().unwrap_or_default(), depth + 1, out);
    }
}

/// One-cell glyph for a symbol's kind (as `completion::kind_glyph`).
pub fn kind_glyph(kind: SymbolKind) -> &'static str {
    const GLYPHS: &[(SymbolKind, &str)] = &[
        (SymbolKind::FILE, "/"),
        (SymbolKind::MODULE, "M"),
        (SymbolKind::NAMESPACE, "M"),
        (SymbolKind::PACKAGE, "M"),
        (SymbolKind::CLASS, "C"),
        (SymbolKind::METHOD, "m"),
        (SymbolKind::PROPERTY, "p"),
        (SymbolKind::FIELD, "d"),
        (SymbolKind::CONSTRUCTOR, "c"),
        (SymbolKind::ENUM, "E"),
        (SymbolKind::INTERFACE, "I"),
        (SymbolKind::FUNCTION, "f"),
        (SymbolKind::VARIABLE, "v"),
        (SymbolKind::CONSTANT, "K"),
        (SymbolKind::STRING, "s"),
        (SymbolKind::NUMBER, "n"),
        (SymbolKind::BOOLEAN, "b"),
        (SymbolKind::ARRAY, "a"),
        (SymbolKind::OBJECT, "o"),
        (SymbolKind::KEY, "k"),
        (SymbolKind::NULL, "0"),
        (SymbolKind::ENUM_MEMBER, "e"),
        (SymbolKind::STRUCT, "S"),
        (SymbolKind::EVENT, "!"),
        (SymbolKind::OPERATOR, "o"),
        (SymbolKind::TYPE_PARAMETER, "T"),
    ];
    GLYPHS.iter().find(|(k, _)| *k == kind).map(|(_, g)| *g).unwrap_or(" ")
}

/// Lower-case name of a symbol's kind, as the index labels its own.
pub fn kind_name(kind: SymbolKind) -> &'static str {
    const NAMES: &[(SymbolKind, &str)] = &[
        (SymbolKind::FILE, "file"),
        (SymbolKind::MODULE, "module"),
        (SymbolKind::NAMESPACE, "namespace"),
        (SymbolKind::PACKAGE, "package"),
        (SymbolKind::CLASS, "class"),
        (SymbolKind::METHOD, "method"),
        (SymbolKind::PROPERTY, "property"),
        (SymbolKind::FIELD, "field"),
        (SymbolKind::CONSTRUCTOR, "constructor"),
        (SymbolKind::ENUM, "enum"),
        (SymbolKind::INTERFACE, "interface"),
        (SymbolKind::FUNCTION, "function"),
        (SymbolKind::VARIABLE, "variable"),
        (SymbolKind::CONSTANT, "constant"),
        (SymbolKind::OBJECT, "object"),
        (SymbolKind::KEY, "key"),
        (SymbolKind::ENUM_MEMBER, "enum member"),
        (SymbolKind::STRUCT, "struct"),
        (SymbolKind::EVENT, "event"),
        (SymbolKind::OPERATOR, "operator"),
        (SymbolKind::TYPE_PARAMETER, "type parameter"),
    ];
    NAMES.iter().find(|(k, _)| *k == kind).map(|(_, n)| *n).unwrap_or("symbol")
}

/// Palette rows of a `workspace/symbol` answer: files only, paths relative
/// to `root` when under it, columns still UTF-16.
pub fn workspace_entries(v: Value, root: &Path) -> Vec<SymbolEntry> {
    // (name, kind, uri, range)
    let items: Vec<(String, SymbolKind, lsp_types::Uri, Range)> = match serde_json::from_value::<Option<WorkspaceSymbolResponse>>(v) {
        Ok(Some(WorkspaceSymbolResponse::Flat(list))) => {
            list.into_iter().map(|s| (s.name, s.kind, s.location.uri, s.location.range)).collect()
        }
        Ok(Some(WorkspaceSymbolResponse::Nested(list))) => list
            .into_iter()
            .map(|s| match s.location {
                OneOf::Left(l) => (s.name, s.kind, l.uri, l.range),
                OneOf::Right(w) => (s.name, s.kind, w.uri, Range::default()),
            })
            .collect(),
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::debug!("unexpected workspace symbols: {e}");
            return Vec::new();
        }
    };
    let mut seen: HashSet<(std::path::PathBuf, u32, u32, String)> = HashSet::new();
    let mut out = Vec::new();
    for (name, kind, uri, range) in items {
        let Some(path) = super::sync::path(&uri) else { continue };
        let name = crate::symbols::clean_name(&name);
        let (start, end) = (range.start, range.end);
        if !seen.insert((path.clone(), start.line, start.character, name.clone())) {
            continue;
        }
        let shown = match path.strip_prefix(root) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => path.to_string_lossy().into_owned(),
        };
        out.push(SymbolEntry {
            path: Arc::from(shown.as_str()),
            symbol: Symbol {
                name: Arc::from(name.as_str()),
                kind: kind_name(kind),
                line: start.line as usize,
                col: start.character as usize,
                start_line: start.line as usize,
                end_line: end.line as usize,
                depth: 0,
            },
            utf16: true,
        });
        if out.len() >= MAX_WORKSPACE_ENTRIES {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    fn short(rows: &[SymbolRow]) -> Vec<(usize, &str, u32)> {
        rows.iter().map(|r| (r.depth, r.name.as_str(), r.pos.line)).collect()
    }

    #[test]
    fn nested_symbols_are_walked_in_text_order() {
        let v = json!([
            {"name": "b", "kind": 12, "range": range(9, 0, 9, 10), "selectionRange": range(9, 3, 9, 4)},
            {"name": "S", "kind": 23, "range": range(0, 0, 5, 1), "selectionRange": range(0, 7, 0, 8), "children": [
                {"name": "y", "kind": 8, "range": range(2, 4, 2, 10), "selectionRange": range(2, 4, 2, 5)},
                {"name": "x", "kind": 8, "range": range(1, 4, 1, 10), "selectionRange": range(1, 4, 1, 5), "children": []},
            ]},
        ]);
        let rows = rows(v);
        assert_eq!(short(&rows), [(0, "S", 0), (1, "x", 1), (1, "y", 2), (0, "b", 9)]);
        assert_eq!(rows[0].pos, Position::new(0, 7), "the name, not the whole item");
        assert_eq!(kind_glyph(rows[0].kind), "S");
        assert_eq!(kind_glyph(rows[1].kind), "d");
    }

    #[test]
    fn flat_symbols_nest_by_their_ranges() {
        let loc = |r: Value| json!({"uri": "file:///w/a.py", "range": r});
        let v = json!([
            {"name": "helper", "kind": 12, "location": loc(range(8, 0, 9, 8))},
            {"name": "method", "kind": 6, "location": loc(range(2, 4, 3, 12)), "containerName": "A"},
            {"name": "A", "kind": 5, "location": loc(range(0, 0, 6, 0))},
            {"name": "x", "kind": 13, "location": loc(range(3, 8, 3, 9))},
        ]);
        assert_eq!(short(&rows(v)), [(0, "A", 0), (1, "method", 2), (2, "x", 3), (0, "helper", 8)]);
        assert!(rows(Value::Null).is_empty());
        assert!(rows(json!({"bad": 1})).is_empty());
    }

    #[test]
    fn workspace_entries_flat_nested_and_outside_root() {
        let root = Path::new("/w");
        let loc = |uri: &str, r: Value| json!({"uri": uri, "range": r});
        let flat = json!([
            {"name": "helper", "kind": 12, "location": loc("file:///w/src/b.rs", range(3, 7, 5, 1))},
            {"name": "helper", "kind": 12, "location": loc("file:///w/src/b.rs", range(3, 7, 5, 1))},
            {"name": "untitled", "kind": 12, "location": loc("untitled:Untitled-1", range(0, 0, 0, 1))},
        ]);
        let e = workspace_entries(flat, root);
        assert_eq!(e.len(), 1, "a repeat and a non-file URI are dropped");
        assert_eq!((&*e[0].path, &*e[0].symbol.name, e[0].symbol.kind), ("src/b.rs", "helper", "function"));
        assert_eq!((e[0].symbol.line, e[0].symbol.col, e[0].symbol.start_line, e[0].symbol.end_line), (3, 7, 3, 5));
        assert!(e[0].utf16);
        let nested = json!([
            {"name": "Thing", "kind": 23, "location": {"uri": "file:///w/lib.rs"}},
            {"name": "Far", "kind": 5, "location": loc("file:///elsewhere/x.rs", range(1, 2, 1, 5))},
        ]);
        let e = workspace_entries(nested, root);
        assert_eq!((&*e[0].path, e[0].symbol.line, e[0].symbol.col, e[0].symbol.kind), ("lib.rs", 0, 0, "struct"));
        assert_eq!((&*e[1].path, e[1].symbol.line, e[1].symbol.kind), ("/elsewhere/x.rs", 1, "class"));
        assert!(workspace_entries(Value::Null, root).is_empty());
        assert!(workspace_entries(json!({"bad": 1}), root).is_empty());
    }
}
