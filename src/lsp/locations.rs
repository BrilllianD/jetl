//! Locations answered by `textDocument/definition` and
//! `textDocument/references`, and the rows the Locations palette shows for
//! them: the path, the line and char column, and the line's text as a
//! preview (from the open buffer when there is one, else from disk, read
//! off the main thread by [`read_items`]), and the chars of the preview the
//! usage's range covers (`mark`).

use std::collections::HashMap;
use std::path::PathBuf;

use lsp_types::{GotoDefinitionResponse, Location, Position, Range};
use serde_json::Value;

use super::{pos, sync};

/// A place in a file a server pointed at; `range` is what to select or
/// put the caret at the start of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub range: Range,
}

/// The targets of a definition or references answer: null, one
/// `Location`, an array of them, or an array of `LocationLink`s (their
/// `targetSelectionRange`). Non-file URIs are left out; duplicates go,
/// the order is kept.
pub fn parse(v: Value) -> Vec<Target> {
    if v.is_null() {
        return Vec::new();
    }
    let locations: Vec<(lsp_types::Uri, Range)> = match serde_json::from_value::<GotoDefinitionResponse>(v) {
        Ok(GotoDefinitionResponse::Scalar(l)) => vec![(l.uri, l.range)],
        Ok(GotoDefinitionResponse::Array(ls)) => ls.into_iter().map(|l: Location| (l.uri, l.range)).collect(),
        Ok(GotoDefinitionResponse::Link(links)) => links.into_iter().map(|l| (l.target_uri, l.target_selection_range)).collect(),
        Err(e) => {
            tracing::debug!("unexpected locations: {e}");
            Vec::new()
        }
    };
    let mut out: Vec<Target> = Vec::new();
    for (uri, range) in locations {
        let Some(path) = sync::path(&uri) else { continue };
        let t = Target { path, range };
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Whether `pos` lies within `range` (both ends included).
pub fn contains(range: &Range, pos: Position) -> bool {
    let key = |p: Position| (p.line, p.character);
    key(range.start) <= key(pos) && key(pos) <= key(range.end)
}

/// Longest preview kept of a line, in chars.
const PREVIEW_MAX: usize = 200;

/// A Locations palette row: a 0-based line and char column in `path`, the
/// text shown (a line preview, or a symbol's name), and `mark`, the chars of
/// `text` the usage covers (cut at the line end; `None` for a point range or
/// a symbol).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationItem {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub text: String,
    pub mark: Option<std::ops::Range<usize>>,
}

impl LocationItem {
    /// The row for `range` on a line whose text is `line_text` (without its
    /// break, or `None` past the end of the file).
    pub fn at(path: PathBuf, range: Range, line_text: Option<&str>) -> Self {
        let line = line_text.unwrap_or("");
        let col = pos::line_char_col(line, range.start.character);
        let lead = line.chars().take_while(|c| c.is_whitespace()).count();
        let end = if range.end.line == range.start.line { pos::line_char_col(line, range.end.character) } else { line.chars().count() };
        let text: String = line.trim().chars().take(PREVIEW_MAX).collect();
        let n = text.chars().count();
        let mark = Some(col.saturating_sub(lead).min(n)..end.saturating_sub(lead).min(n)).filter(|r| !r.is_empty());
        Self { path, line: range.start.line as usize, col, text, mark }
    }
}

/// Rows for targets in files that are not open: each file read once (in
/// `spawn_blocking`). An unreadable file still gets rows, without text.
pub fn read_items(targets: Vec<Target>) -> Vec<LocationItem> {
    let mut files: HashMap<PathBuf, Option<String>> = HashMap::new();
    targets
        .into_iter()
        .map(|t| {
            let text = files
                .entry(t.path.clone())
                .or_insert_with(|| std::fs::read(&t.path).ok().map(|b| String::from_utf8_lossy(&b).into_owned()));
            let line = text
                .as_deref()
                .and_then(|s| crate::util::text::lines_inclusive(s).nth(t.range.start.line as usize).map(crate::git::diff::strip_eol));
            LocationItem::at(t.path, t.range, line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
        json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
    }

    #[test]
    fn location_link_and_array_answers() {
        let a = "file:///w/a.rs";
        let r = |l0, c0, l1, c1| Range::new(Position::new(l0, c0), Position::new(l1, c1));
        assert!(parse(Value::Null).is_empty());
        assert!(parse(json!([])).is_empty());
        let one = parse(json!({"uri": a, "range": range(1, 2, 1, 5)}));
        assert_eq!(one, [Target { path: "/w/a.rs".into(), range: r(1, 2, 1, 5) }]);
        let many = parse(json!([
            {"uri": a, "range": range(1, 2, 1, 5)},
            {"uri": "file:///w/b.rs", "range": range(0, 0, 0, 1)},
            {"uri": a, "range": range(1, 2, 1, 5)},
            {"uri": "untitled:x", "range": range(0, 0, 0, 1)},
        ]));
        assert_eq!(
            many.iter().map(|t| t.path.to_str().unwrap()).collect::<Vec<_>>(),
            ["/w/a.rs", "/w/b.rs"],
            "deduplicated, non-files left out"
        );
        // A link points at its selection range, not the whole item.
        let link = parse(json!([{
            "targetUri": a, "targetRange": range(3, 0, 9, 1), "targetSelectionRange": range(3, 7, 3, 11),
            "originSelectionRange": range(0, 0, 0, 3)
        }]));
        assert_eq!(link, [Target { path: "/w/a.rs".into(), range: r(3, 7, 3, 11) }]);
        assert!(parse(json!({"nonsense": 1})).is_empty());
        assert!(contains(&r(3, 7, 3, 11), Position::new(3, 11)) && !contains(&r(3, 7, 3, 11), Position::new(3, 12)));
    }

    #[test]
    fn items_split_lines_at_a_lone_cr() {
        let dir = std::env::temp_dir().join(format!("jetl-test-locations-cr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "x\rfoo\r\nbar\n").unwrap();
        let t = |line| Target { path: path.clone(), range: Range::new(Position::new(line, 0), Position::new(line, 1)) };
        let items = read_items(vec![t(1), t(2)]);
        assert_eq!((items[0].text.as_str(), items[1].text.as_str()), ("foo", "bar"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn items_read_their_line_from_disk_and_convert_the_column() {
        let dir = std::env::temp_dir().join(format!("jetl-test-locations-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\n    let s = \"日本😀\"; s.len();\n").unwrap();
        let t = |line, character| Target {
            path: path.clone(),
            range: Range::new(Position::new(line, character), Position::new(line, character + 1)),
        };
        let missing = Target { path: dir.join("gone.rs"), range: Range::default() };
        // `    let s = "日本😀"; ` is 20 UTF-16 units before `s.len`: the
        // emoji counts two, so it is char 19. Unit 16, inside the emoji,
        // is the emoji (char 15).
        let items = read_items(vec![t(1, 20), t(0, 3), t(7, 0), missing, t(1, 16)]);
        assert_eq!((items[0].line, items[0].col, items[0].text.as_str()), (1, 19, "let s = \"日本😀\"; s.len();"));
        assert_eq!(items[4].col, 15);
        // The `s` of `s.len()` in the trimmed text.
        assert_eq!(items[0].mark, Some(15..16));
        let point = Target { path: path.clone(), range: Range::new(Position::new(1, 20), Position::new(1, 20)) };
        assert_eq!(read_items(vec![point])[0].mark, None, "a point range marks nothing");
        assert_eq!((items[1].line, items[1].col, items[1].text.as_str()), (0, 3, "fn a() {}"));
        assert_eq!((items[2].col, items[2].text.as_str()), (0, ""), "past the end of the file");
        assert_eq!(items[3].text, "", "an unreadable file has no preview");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
