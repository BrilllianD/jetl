//! Document synchronization helpers: the edits a buffer records for
//! `textDocument/didChange`, language ids and file URIs.
//!
//! `Buffer::apply_change` is the single funnel for edits, undo, redo and
//! reloads, and its change order is the `contentChanges` order: each edit's
//! range refers to the text after the previous one. It records a [`DocEdit`]
//! before mutating the rope while the buffer is open on a server.

use std::path::{Path, PathBuf};

use lsp_types::{
    Position, Range, ServerCapabilities, TextDocumentContentChangeEvent, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncSaveOptions, Uri,
};
use ropey::Rope;

use super::pos;

/// Edits kept per buffer between syncs; past this the next sync sends the
/// whole text instead (`Buffer::lsp_full_resync`).
pub const MAX_PENDING: usize = 512;

/// One replacement in LSP terms: the UTF-16 range before the edit and the
/// new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocEdit {
    pub start: Position,
    pub end: Position,
    pub text: String,
}

impl DocEdit {
    /// The edit replacing chars `start..end` of `rope` with `text`, or
    /// `None` when an end falls inside a CRLF break (the caller then sends
    /// the whole text).
    pub fn new(rope: &Rope, start: usize, end: usize, text: &str) -> Option<Self> {
        if pos::inside_crlf(rope, start) || pos::inside_crlf(rope, end) {
            return None;
        }
        Some(Self { start: pos::position(rope, start), end: pos::position(rope, end), text: text.to_string() })
    }

    pub fn change_event(&self) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent { range: Some(Range::new(self.start, self.end)), range_length: None, text: self.text.clone() }
    }
}

/// How a server wants documents synced, from its capabilities. The
/// number form means open/close, that kind of change, and saves without
/// the text (what VS Code assumes too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncCaps {
    /// `didOpen` wanted.
    pub open: bool,
    /// `didClose` wanted.
    pub close: bool,
    pub change: TextDocumentSyncKind,
    /// `didSave` wanted, and whether with the text.
    pub save: Option<bool>,
}

impl SyncCaps {
    pub fn of(caps: &ServerCapabilities) -> Self {
        match &caps.text_document_sync {
            Some(TextDocumentSyncCapability::Kind(kind)) => Self { open: true, close: true, change: *kind, save: Some(false) },
            Some(TextDocumentSyncCapability::Options(o)) => Self {
                open: o.open_close.unwrap_or(false),
                close: o.open_close.unwrap_or(false),
                change: o.change.unwrap_or(TextDocumentSyncKind::NONE),
                save: match &o.save {
                    Some(TextDocumentSyncSaveOptions::Supported(true)) => Some(false),
                    Some(TextDocumentSyncSaveOptions::SaveOptions(s)) => Some(s.include_text.unwrap_or(false)),
                    _ => None,
                },
            },
            None => Self { open: false, close: false, change: TextDocumentSyncKind::NONE, save: None },
        }
    }
}

/// The LSP `languageId` of a jetl language (`LanguageDef::name`).
pub fn language_id(name: &str) -> &str {
    match name {
        "tsx" => "typescriptreact",
        "bash" => "shellscript",
        other => other,
    }
}

/// The `file://` URI of an absolute path.
pub fn uri(path: &Path) -> Option<Uri> {
    let url = url::Url::from_file_path(path).ok()?;
    url.as_str().parse().ok()
}

/// The path of a `file://` URI.
pub fn path(uri: &Uri) -> Option<PathBuf> {
    url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Apply `changes` to `text` the way a server does: ranges in UTF-16
    /// against the text after the previous change; no range replaces all.
    /// Positions go through `lsp_offset`, a model of the protocol that does
    /// not use ropey, so tests check jetl's line counting against it.
    pub(crate) fn apply_changes(text: &str, changes: &[TextDocumentContentChangeEvent]) -> String {
        let mut text = text.to_string();
        for c in changes {
            match c.range {
                Some(r) => {
                    let (s, e) = (lsp_offset(&text, r.start), lsp_offset(&text, r.end));
                    text.replace_range(s..e, &c.text);
                }
                None => text = c.text.clone(),
            }
        }
        text
    }

    /// The byte offset of `p` in `text` as LSP defines it: lines break at
    /// `\n`, `\r\n` (one break) and a lone `\r`; the column counts UTF-16
    /// units and clamps to the end of its line (before the break), a column
    /// inside a surrogate pair naming the char it belongs to; a line past
    /// the end is the end of the text.
    pub(crate) fn lsp_offset(text: &str, p: Position) -> usize {
        let b = text.as_bytes();
        let mut i = 0;
        for _ in 0..p.line {
            match b[i..].iter().position(|&c| c == b'\n' || c == b'\r') {
                None => return text.len(),
                Some(k) => {
                    i += k;
                    i += if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                }
            }
        }
        let mut units = 0;
        for (k, c) in text[i..].char_indices() {
            if c == '\n' || c == '\r' || units + c.len_utf16() > p.character as usize {
                return i + k;
            }
            units += c.len_utf16();
        }
        text.len()
    }

    #[test]
    fn uris_round_trip_with_spaces_and_non_ascii() {
        let p = Path::new("/tmp/a dir/日本.rs");
        let u = uri(p).unwrap();
        assert_eq!(u.as_str(), "file:///tmp/a%20dir/%E6%97%A5%E6%9C%AC.rs");
        assert_eq!(path(&u).unwrap(), p);
        assert!(uri(Path::new("relative.rs")).is_none());
    }

    #[test]
    fn edits_next_to_crlf_breaks() {
        let r = Rope::from_str("ab\r\ncd");
        assert!(DocEdit::new(&r, 3, 4, "").is_none(), "between \\r and \\n");
        let e = DocEdit::new(&r, 2, 4, "").unwrap();
        assert_eq!((e.start, e.end), (Position::new(0, 2), Position::new(1, 0)));
        assert_eq!(apply_changes("ab\r\ncd", &[e.change_event()]), "abcd");
    }

    #[test]
    fn edits_around_unicode_separators() {
        let t = "a\u{b}b\u{c}c\n\u{85}d\u{2028}e\u{2029}f";
        let n = t.chars().count();
        let r = Rope::from_str(t);
        for start in 0..=n {
            for end in start..=n {
                for text in ["", "X", "\n"] {
                    let e = DocEdit::new(&r, start, end, text).unwrap();
                    let mut want = r.clone();
                    want.remove(start..end);
                    want.insert(start, text);
                    assert_eq!(apply_changes(t, &[e.change_event()]), want.to_string(), "{start}..{end} {text:?}");
                }
            }
        }
        // The char before `e` is U+2028, column 2 of line 1.
        let at = t.chars().position(|c| c == 'e').unwrap() - 1;
        let e = DocEdit::new(&r, at, at, "X").unwrap();
        assert_eq!((e.start, e.end), (Position::new(1, 2), Position::new(1, 2)));
    }

    #[test]
    fn lsp_offset_counts_every_break_form() {
        let t = "a\rb\r\nc\nd😀e";
        let p = Position::new;
        assert_eq!(lsp_offset(t, p(1, 0)), 2, "after a lone \\r");
        assert_eq!(lsp_offset(t, p(1, 9)), 3, "clamped before \\r\\n");
        assert_eq!(lsp_offset(t, p(2, 0)), 5);
        assert_eq!(lsp_offset(t, p(3, 2)), t.find('😀').unwrap(), "inside the surrogate pair");
        assert_eq!(lsp_offset(t, p(3, 3)), t.find('e').unwrap());
        assert_eq!(lsp_offset(t, p(4, 0)), t.len());
    }
}
