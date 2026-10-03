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
    pub open_close: bool,
    pub change: TextDocumentSyncKind,
    /// `didSave` wanted, and whether with the text.
    pub save: Option<bool>,
}

impl SyncCaps {
    pub fn of(caps: &ServerCapabilities) -> Self {
        match &caps.text_document_sync {
            Some(TextDocumentSyncCapability::Kind(kind)) => Self { open_close: true, change: *kind, save: Some(false) },
            Some(TextDocumentSyncCapability::Options(o)) => Self {
                open_close: o.open_close.unwrap_or(false),
                change: o.change.unwrap_or(TextDocumentSyncKind::NONE),
                save: match &o.save {
                    Some(TextDocumentSyncSaveOptions::Supported(true)) => Some(false),
                    Some(TextDocumentSyncSaveOptions::SaveOptions(s)) => Some(s.include_text.unwrap_or(false)),
                    _ => None,
                },
            },
            None => Self { open_close: false, change: TextDocumentSyncKind::NONE, save: None },
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
    pub(crate) fn apply_changes(text: &str, changes: &[TextDocumentContentChangeEvent]) -> String {
        let mut rope = Rope::from_str(text);
        for c in changes {
            match c.range {
                Some(r) => {
                    let (s, e) = (pos::char_index(&rope, r.start), pos::char_index(&rope, r.end));
                    rope.remove(s..e);
                    rope.insert(s, &c.text);
                }
                None => rope = Rope::from_str(&c.text),
            }
        }
        rope.to_string()
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
}
