//! Several text edits made at once, in char indices of one text, turned
//! into the changes of one `Buffer::apply` transaction (one undo step).
//!
//! The changes of a transaction apply one after another, each at a position
//! in the text the previous ones left. Applying the edits from the last to
//! the first keeps every position as it was in the original text, so
//! [`plan`] sorts them by position, descending, and refuses overlapping
//! ones. [`Plan::starts`] says where each edit's new text ends up, for the
//! caret. Completion (`lsp::completion`) uses it for an item's edit plus
//! its `additionalTextEdits`.
//!
//! Workspace edits (rename, code actions, `workspace/applyEdit`) are
//! planned here too: [`workspace_files`] reads a `WorkspaceEdit` into the
//! edits of each file, and [`plan_workspace`] checks and plans them against
//! the open buffers (one transaction each) before anything is changed; the
//! files that are not open are edited on disk ([`edit_text`]).

use std::path::{Path, PathBuf};

use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, TextEdit, WorkspaceEdit};
use ropey::Rope;

use super::{pos, sync};
use crate::editor::buffer::BufferId;
use crate::editor::undo::Change;

/// Replace chars `start..end` of the original text with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl CharEdit {
    pub fn new(start: usize, end: usize, text: impl Into<String>) -> Self {
        Self { start, end: end.max(start), text: text.into() }
    }

    /// The edit of an LSP `TextEdit` (UTF-16 positions, clamped) in `rope`.
    pub fn from_lsp(rope: &Rope, e: &TextEdit) -> Self {
        Self::new(pos::char_index(rope, e.range.start), pos::char_index(rope, e.range.end), e.new_text.clone())
    }
}

/// The changes for `Buffer::apply`, and where each edit's new text starts
/// in the edited text (by the edits' input order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub starts: Vec<usize>,
}

/// Two edits of a plan replace the same text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlap {
    /// Input indices of the two edits.
    pub first: usize,
    pub second: usize,
}

/// Plan `edits` (positions in `rope`, clamped to it) as one transaction:
/// changes sorted by position descending, each with the text it replaces.
/// Insertions at the same position keep their input order in the result
/// (as the LSP asks), and an insertion at the start of a replacement goes
/// before it. Edits that change nothing are left out of the changes.
pub fn plan(rope: &Rope, edits: &[CharEdit]) -> Result<Plan, Overlap> {
    let len = rope.len_chars();
    let clamped: Vec<(usize, usize)> = edits
        .iter()
        .map(|e| {
            let start = e.start.min(len);
            (start, e.end.clamp(start, len))
        })
        .collect();
    // Text order: by start, insertions before replacements at the same
    // start, then input order.
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| (clamped[i].0, clamped[i].1 > clamped[i].0, i));
    for w in order.windows(2) {
        if clamped[w[0]].1 > clamped[w[1]].0 {
            return Err(Overlap { first: w[0].min(w[1]), second: w[0].max(w[1]) });
        }
    }
    let mut starts = vec![0; edits.len()];
    let mut shift: isize = 0;
    for &i in &order {
        let (s, e) = clamped[i];
        starts[i] = (s as isize + shift) as usize;
        shift += edits[i].text.chars().count() as isize - (e - s) as isize;
    }
    let changes = order
        .iter()
        .rev()
        .filter_map(|&i| {
            let (s, e) = clamped[i];
            let old = rope.slice(s..e).to_string();
            (old != edits[i].text).then(|| Change::replace(s, old, edits[i].text.clone()))
        })
        .collect();
    Ok(Plan { changes, starts })
}

/// `text` with `\n` line breaks as `\r\n` when `crlf` (a server's new text
/// for a CRLF buffer), and the char offset `at` in it moved along.
pub fn line_breaks(text: &str, at: usize, crlf: bool) -> (String, usize) {
    let text = text.replace("\r\n", "\n");
    if !crlf {
        return (text, at);
    }
    let before = text.chars().take(at).filter(|&c| c == '\n').count();
    (text.replace('\n', "\r\n"), at + before)
}

/// Where char position `p` is after `changes` (as [`plan`] makes them:
/// descending, each in the text the previous ones left). A position inside
/// a replaced range keeps its offset into the new text, up to its end.
pub fn map_pos(changes: &[Change], mut p: usize) -> usize {
    for c in changes {
        let (old, new) = (c.old.chars().count(), c.new.chars().count());
        if p >= c.at + old {
            p = p + new - old;
        } else if p > c.at {
            p = c.at + (p - c.at).min(new);
        }
    }
    p
}

/// One file's part of a workspace edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    /// The version of the document the edits were made for, when the
    /// server said (`documentChanges` with a versioned identifier).
    pub version: Option<i32>,
    pub edits: Vec<TextEdit>,
}

/// The files a `WorkspaceEdit` changes and their edits, in the order
/// given (`changes`, an unordered map, by path). `documentChanges` wins
/// over `changes` when both are there, as the protocol asks. Several text
/// document edits of one file are merged. File operations (create,
/// rename, delete) are not supported: the whole edit is refused.
pub fn workspace_files(edit: WorkspaceEdit) -> Result<Vec<FileEdit>, String> {
    let mut docs: Vec<(lsp_types::Uri, Option<i32>, Vec<TextEdit>)> = Vec::new();
    let text_edits = |edits: Vec<OneOf<TextEdit, lsp_types::AnnotatedTextEdit>>| -> Vec<TextEdit> {
        edits
            .into_iter()
            .map(|e| match e {
                OneOf::Left(e) => e,
                OneOf::Right(a) => a.text_edit,
            })
            .collect()
    };
    match edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => {
            docs.extend(edits.into_iter().map(|d| (d.text_document.uri, d.text_document.version, text_edits(d.edits))));
        }
        Some(DocumentChanges::Operations(ops)) => {
            for op in ops {
                match op {
                    DocumentChangeOperation::Edit(d) => docs.push((d.text_document.uri, d.text_document.version, text_edits(d.edits))),
                    DocumentChangeOperation::Op(op) => {
                        let what = match op {
                            lsp_types::ResourceOp::Create(_) => "creating",
                            lsp_types::ResourceOp::Rename(_) => "renaming",
                            lsp_types::ResourceOp::Delete(_) => "deleting",
                        };
                        return Err(format!("the edit needs {what} a file, which jetl does not support"));
                    }
                }
            }
        }
        None => {
            let mut changes: Vec<_> = edit.changes.unwrap_or_default().into_iter().collect();
            changes.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            docs.extend(changes.into_iter().map(|(uri, edits)| (uri, None, edits)));
        }
    }
    let mut out: Vec<FileEdit> = Vec::new();
    for (uri, version, edits) in docs {
        let path = sync::path(&uri).ok_or_else(|| format!("{} is not a file", uri.as_str()))?;
        match out.iter_mut().find(|f| f.path == path) {
            Some(f) => {
                f.version = f.version.or(version);
                f.edits.extend(edits);
            }
            None => out.push(FileEdit { path, version, edits }),
        }
    }
    Ok(out)
}

/// What a workspace edit changed: files, and edits in them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditCounts {
    pub files: usize,
    pub edits: usize,
}

/// An open buffer a workspace edit may change.
#[derive(Debug, Clone)]
pub struct OpenDoc {
    pub id: BufferId,
    pub path: PathBuf,
    pub name: String,
    pub rope: Rope,
    pub crlf: bool,
    /// The document's version on its language server, if it is open there.
    pub version: Option<i32>,
}

/// A workspace edit checked and planned: one transaction per open buffer
/// (with the number of edits it makes), the files that are not open with
/// their edits.
#[derive(Debug, Clone, Default)]
pub struct WorkspacePlan {
    pub open: Vec<(BufferId, Plan, usize)>,
    pub closed: Vec<FileEdit>,
}

/// Plan `files` against the open buffers `open`. Fails, so that nothing
/// is changed, when an open buffer is not at the version the server made
/// its edits for, or when edits of a file overlap.
pub fn plan_workspace(files: Vec<FileEdit>, open: &[OpenDoc]) -> Result<WorkspacePlan, String> {
    let mut out = WorkspacePlan::default();
    for f in files {
        let Some(doc) = open.iter().find(|d| d.path == f.path) else {
            out.closed.push(f);
            continue;
        };
        if let (Some(want), Some(have)) = (f.version, doc.version)
            && want != have
        {
            return Err(format!("{} changed since the language server made the edit", doc.name));
        }
        let edits: Vec<CharEdit> = f
            .edits
            .iter()
            .map(|e| {
                let mut c = CharEdit::from_lsp(&doc.rope, e);
                c.text = line_breaks(&c.text, 0, doc.crlf).0;
                c
            })
            .collect();
        let plan = plan(&doc.rope, &edits).map_err(|_| format!("the edits of {} overlap", doc.name))?;
        let n = plan.changes.len();
        out.open.push((doc.id, plan, n));
    }
    Ok(out)
}

/// `text` with LSP `edits` made (a file that is not open; its line breaks
/// kept for the new text), and the number of changes.
pub fn edit_text(text: &str, edits: &[TextEdit]) -> Result<(String, usize), Overlap> {
    let mut rope = Rope::from_str(text);
    let crlf = text.contains("\r\n");
    let edits: Vec<CharEdit> = edits
        .iter()
        .map(|e| {
            let mut c = CharEdit::from_lsp(&rope, e);
            c.text = line_breaks(&c.text, 0, crlf).0;
            c
        })
        .collect();
    let changes = plan(&rope, &edits)?.changes;
    let n = changes.len();
    for c in changes {
        rope.remove(c.at..c.at + c.old.chars().count());
        rope.insert(c.at, &c.new);
    }
    Ok((rope.to_string(), n))
}

/// Make `edits` in the file at `path` (read, edited, written atomically).
/// Returns the number of changes made.
pub fn edit_file(path: &Path, edits: &[TextEdit]) -> Result<usize, String> {
    let name = path.display();
    let text = match std::fs::read(path).map(String::from_utf8) {
        Ok(Ok(t)) => t,
        Ok(Err(_)) => return Err(format!("{name} is not valid UTF-8")),
        Err(e) => return Err(format!("{name}: {e}")),
    };
    let (new, n) = edit_text(&text, edits).map_err(|_| format!("the edits of {name} overlap"))?;
    if n == 0 {
        return Ok(0);
    }
    crate::util::fs::write_atomic(path, &new).map_err(|e| format!("{name}: {e}"))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::Buffer;
    use crate::editor::selection::Selection;
    use crate::editor::undo::TxKind;

    fn run(text: &str, edits: &[CharEdit]) -> (String, Plan) {
        let mut b = Buffer::from_text(1, None, text, false);
        let plan = plan(&b.rope, edits).unwrap();
        b.apply(plan.changes.clone(), Selection::point(0), TxKind::Other);
        (b.text(), plan)
    }

    #[test]
    fn edits_apply_from_the_last_and_report_their_new_starts() {
        // An import at the top, the item in the middle, a tail at the end.
        let text = "fn a() {}\nfoo\nbar\n";
        let edits =
            [CharEdit::new(10, 13, "food()"), CharEdit::new(0, 0, "use x;\n"), CharEdit::new(14, 17, "baz"), CharEdit::new(18, 18, "!")];
        let (out, plan) = run(text, &edits);
        assert_eq!(out, "use x;\nfn a() {}\nfood()\nbaz\n!");
        assert_eq!(plan.starts, [17, 0, 24, 28]);
        assert_eq!(&out[17..23], "food()");
        // Descending positions.
        let ats: Vec<usize> = plan.changes.iter().map(|c| c.at).collect();
        assert_eq!(ats, [18, 14, 10, 0]);
        // One undo step reverts all of it.
        let mut b = Buffer::from_text(1, None, text, false);
        b.apply(plan.changes, Selection::point(0), TxKind::Other);
        assert!(b.undo());
        assert_eq!(b.text(), text);
    }

    #[test]
    fn same_position_inserts_keep_their_order_and_overlaps_fail() {
        let (out, plan) = run("ab", &[CharEdit::new(1, 1, "1"), CharEdit::new(1, 1, "2"), CharEdit::new(1, 2, "B")]);
        assert_eq!(out, "a12B");
        assert_eq!(plan.starts, [1, 2, 3]);
        let rope = Rope::from_str("abcdef");
        assert_eq!(plan_err(&rope, &[CharEdit::new(3, 5, "x"), CharEdit::new(0, 4, "y")]), Some(Overlap { first: 0, second: 1 }));
        // Touching ranges are fine; out of range positions clamp.
        assert!(super::plan(&rope, &[CharEdit::new(0, 3, "x"), CharEdit::new(3, 6, "y"), CharEdit::new(99, 120, "z")]).is_ok());
        // An edit that changes nothing has no change but a start.
        let (_, plan) = run("abc", &[CharEdit::new(0, 1, "a"), CharEdit::new(2, 3, "C")]);
        assert_eq!((plan.changes.len(), plan.starts), (1, vec![0, 2]));
    }

    fn plan_err(rope: &Rope, edits: &[CharEdit]) -> Option<Overlap> {
        super::plan(rope, edits).err()
    }

    fn te(l0: u32, c0: u32, l1: u32, c1: u32, text: &str) -> TextEdit {
        TextEdit::new(lsp_types::Range::new(lsp_types::Position::new(l0, c0), lsp_types::Position::new(l1, c1)), text.into())
    }

    fn open_doc(id: BufferId, path: &str, text: &str, version: Option<i32>) -> OpenDoc {
        OpenDoc {
            id,
            path: path.into(),
            name: Path::new(path).file_name().unwrap().to_string_lossy().into(),
            rope: Rope::from_str(text),
            crlf: text.contains("\r\n"),
            version,
        }
    }

    #[test]
    fn workspace_files_from_changes_and_document_changes() {
        // `changes`: by path.
        let we: WorkspaceEdit = serde_json::from_value(serde_json::json!({"changes": {
            "file:///w/b.rs": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "B"}],
            "file:///w/a.rs": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "A"}],
        }}))
        .unwrap();
        let files = workspace_files(we).unwrap();
        assert_eq!(files.iter().map(|f| (f.path.to_str().unwrap(), f.version)).collect::<Vec<_>>(), [("/w/a.rs", None), ("/w/b.rs", None)]);
        // `documentChanges` wins over `changes`; versions kept; annotated
        // edits are edits; two edits of one file merge.
        let edit = |text: &str| serde_json::json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": text});
        let we: WorkspaceEdit = serde_json::from_value(serde_json::json!({
            "changes": {"file:///w/c.rs": [edit("ignored")]},
            "documentChanges": [
                {"textDocument": {"uri": "file:///w/a.rs", "version": 3}, "edits": [edit("1")]},
                {"textDocument": {"uri": "file:///w/b.rs", "version": null}, "edits": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "x", "annotationId": "a"}]},
                {"textDocument": {"uri": "file:///w/a.rs", "version": 3}, "edits": [edit("2")]},
            ],
        }))
        .unwrap();
        let files = workspace_files(we).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].path.to_str().unwrap(), files[0].version, files[0].edits.len()), ("/w/a.rs", Some(3), 2));
        assert_eq!((files[1].version, files[1].edits[0].new_text.as_str()), (None, "x"));
        // File operations refuse the whole edit, wherever they are.
        for op in [
            serde_json::json!({"kind": "create", "uri": "file:///w/new.rs"}),
            serde_json::json!({"kind": "rename", "oldUri": "file:///w/a.rs", "newUri": "file:///w/z.rs"}),
            serde_json::json!({"kind": "delete", "uri": "file:///w/a.rs"}),
        ] {
            let we: WorkspaceEdit = serde_json::from_value(serde_json::json!({"documentChanges": [
                {"textDocument": {"uri": "file:///w/a.rs", "version": 1}, "edits": [edit("1")]}, op,
            ]}))
            .unwrap();
            let e = workspace_files(we).unwrap_err();
            assert!(e.contains("does not support"), "{e}");
        }
        let we = serde_json::json!({"changes": {"untitled:x": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "x"}]}});
        assert!(workspace_files(serde_json::from_value(we).unwrap()).unwrap_err().contains("not a file"));
    }

    #[test]
    fn workspace_plan_edits_open_buffers_and_leaves_closed_files() {
        // The open buffer is dirty (not what is on disk): its text is what
        // the positions refer to. 😀 is two UTF-16 units.
        let a = "let s = \"😀\"; old(s);\n// old\n";
        let files = vec![
            FileEdit { path: "/w/a.rs".into(), version: Some(4), edits: vec![te(0, 14, 0, 17, "renamed"), te(1, 3, 1, 6, "new")] },
            FileEdit { path: "/w/b.rs".into(), version: None, edits: vec![te(0, 0, 0, 3, "new")] },
        ];
        let open = [open_doc(1, "/w/a.rs", a, Some(4))];
        let plan = plan_workspace(files.clone(), &open).unwrap();
        assert_eq!(plan.closed, files[1..]);
        let (id, p, n) = plan.open[0].clone();
        assert_eq!((id, n), (1, 2));
        let mut b = crate::editor::buffer::Buffer::from_text(1, None, a, false);
        b.apply(p.changes.clone(), Selection::point(0), TxKind::Other);
        assert_eq!(b.text(), "let s = \"😀\"; renamed(s);\n// new\n");
        // The caret after the call's name moves with it.
        assert_eq!(map_pos(&p.changes, 16), 20, "before the ( after old: before the one after renamed");
        assert_eq!(map_pos(&p.changes, a.chars().count()), a.chars().count() + 4);
        assert_eq!(map_pos(&[Change::replace(2, "abc", "x")], 4), 3, "inside a replacement: up to its end");

        // Another version of the open buffer: nothing is planned.
        let e = plan_workspace(files.clone(), &[open_doc(1, "/w/a.rs", a, Some(5))]).unwrap_err();
        assert!(e.contains("a.rs changed"), "{e}");
        // Not open on a server, or no version given: no check.
        assert!(plan_workspace(files.clone(), &[open_doc(1, "/w/a.rs", a, None)]).is_ok());
        // Overlapping edits refuse it too.
        let bad = vec![FileEdit { path: "/w/a.rs".into(), version: None, edits: vec![te(0, 0, 0, 5, "x"), te(0, 2, 0, 8, "y")] }];
        assert!(plan_workspace(bad, &open).unwrap_err().contains("overlap"));
    }

    #[test]
    fn closed_files_keep_their_line_breaks() {
        let (text, n) = edit_text("a\r\nold\r\n", &[te(1, 0, 1, 3, "new\nline"), te(0, 0, 0, 0, "")]).unwrap();
        assert_eq!((text.as_str(), n), ("a\r\nnew\r\nline\r\n", 1));
        let (text, _) = edit_text("日本 old\n", &[te(0, 3, 0, 6, "new")]).unwrap();
        assert_eq!(text, "日本 new\n");
        assert!(edit_text("abc", &[te(0, 0, 0, 2, "x"), te(0, 1, 0, 3, "y")]).is_err());
        let dir = std::env::temp_dir().join(format!("jetl-test-edits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();
        assert_eq!(edit_file(&path, &[te(0, 3, 0, 6, "new")]), Ok(1));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn new() {}\n");
        assert_eq!(edit_file(&path, &[te(0, 3, 0, 6, "new")]), Ok(0), "unchanged: not written");
        assert!(edit_file(&dir.join("gone.rs"), &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_breaks_follow_the_buffer() {
        assert_eq!(line_breaks("a\nb\r\nc", 4, false), ("a\nb\nc".to_string(), 4));
        assert_eq!(line_breaks("a\nb\nc", 4, true), ("a\r\nb\r\nc".to_string(), 6));
    }
}
