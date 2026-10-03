//! `textDocument/documentSymbol` answers as rows for the File Structure
//! palette: a tree of `DocumentSymbol`s walked depth first, or a flat list
//! of `SymbolInformation`s nested by their ranges.

use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position, Range, SymbolKind};
use serde_json::Value;

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
}
