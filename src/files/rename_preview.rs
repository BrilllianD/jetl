//! Rename preview: a language server's rename answer (`Vec<Step>`) turned
//! into Find results, one per text edit, so the user reviews it before
//! anything is written. `build` runs in `spawn_blocking`; the Find panel
//! shows the hits through its own batch path, and Replace All rebuilds the
//! steps from the rows left included (`included_steps`) for the usual
//! workspace-edit apply (`App::apply_steps_now`). File operations are never
//! rows: they are counted in the Find status line and listed in the confirm
//! dialog.
//!
//! Positions in a row come from the text the edit will meet: an open
//! document's rope, else the file as read (and statted) when the preview was
//! built. Each file with a row has a `FileStamp`; `first_stale` compares them
//! with the world at apply time, so a rename is all included edits or none.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use lsp_types::TextEdit;
use ropey::Rope;

use crate::editor::buffer::BufferId;
use crate::editor::nav::moved_path;
use crate::editor::selection::line_len_no_eol;
use crate::files::find::{Hit, preview};
use crate::lsp::edits::{FileEdit, FileOp, Step};
use crate::lsp::pos;

/// Where a row came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameSource {
    /// An edit of the server's answer.
    Exact,
    /// A textual match of the old name (not part of the answer).
    Textual,
}

/// What a rename row carries beside the `Hit` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameEdit {
    /// What replaces the hit's range. Per hit: a server may write `new: x` for a shorthand field.
    pub new_text: String,
    /// The edit in LSP positions (UTF-16), whatever the source.
    pub edit: TextEdit,
    /// Step and edit index in `RenamePreview::steps`; `None` for a textual row.
    pub at: Option<(u32, u32)>,
    pub source: RenameSource,
}

/// What a file was when the preview was built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStamp {
    OpenBuffer { id: BufferId, version: u64 },
    Disk { mtime: SystemTime, size: u64 },
}

/// An open buffer with a path, as the preview sees it.
#[derive(Debug, Clone)]
pub struct OpenText {
    pub path: PathBuf,
    pub id: BufferId,
    pub rope: Rope,
    pub version: u64,
    pub dirty: bool,
}

#[derive(Debug)]
pub struct RenamePreview {
    pub old: String,
    pub new: String,
    /// The server's or SCIP's steps, in order, file operations included.
    pub steps: Vec<Step>,
    /// One per file with a row, taken when the preview was built; also one per file that the positions of an
    /// edit without a row refer to (the source of a rename, an existing file a create leaves alone), when it
    /// has text.
    pub stamps: HashMap<PathBuf, FileStamp>,
    /// The `(step, edit)` of every edit that has no row (its file is made by an earlier operation of the
    /// answer, or has no text to show); it always applies.
    pub unlisted: HashSet<(u32, u32)>,
}

pub struct RenameRequest {
    pub old: String,
    pub new: String,
    pub steps: Vec<Step>,
    pub open: Vec<OpenText>,
    pub root: PathBuf,
}

/// The text of one file as the rows see it, with its stamp.
struct Text {
    rope: Rope,
    stamp: FileStamp,
}

/// `path` relative to `root`, for messages.
fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// The text and stamp of `path`: an open document, else the file (statted first, so a write in between shows
/// as stale at apply). `None` for a file that is missing or a folder; `Err` for anything else.
fn text_of(path: &Path, open: &[OpenText], root: &Path) -> Result<Option<Text>, String> {
    if let Some(o) = open.iter().find(|o| o.path == path) {
        return Ok(Some(Text { rope: o.rope.clone(), stamp: FileStamp::OpenBuffer { id: o.id, version: o.version } }));
    }
    let gone = |e: &std::io::Error| matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory);
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => return Ok(None),
        Ok(m) => m,
        Err(e) if gone(&e) => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", rel(path, root))),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if gone(&e) => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", rel(path, root))),
    };
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", rel(path, root)))?;
    let mtime = meta.modified().map_err(|e| format!("{}: {e}", rel(path, root)))?;
    Ok(Some(Text { rope: Rope::from_str(&text), stamp: FileStamp::Disk { mtime, size: meta.len() } }))
}

/// The row of edit `e` (number `at.1` of step `at.0`) in `rope`.
fn row(path: &Path, rope: &Rope, e: &TextEdit, at: (u32, u32)) -> Hit {
    let start = pos::char_index(rope, e.range.start);
    let end = pos::char_index(rope, e.range.end).max(start);
    let line = rope.char_to_line(start);
    let line_start = rope.line_to_char(line);
    let col = start - line_start;
    let line_len = line_len_no_eol(rope, line);
    let len = end.min(line_start + line_len).saturating_sub(start);
    let line_text = rope.slice(line_start..line_start + line_len).to_string();
    let (preview, preview_col) = preview(&line_text, col);
    Hit {
        path: path.to_path_buf(),
        line,
        col,
        len,
        preview,
        preview_col,
        matched: rope.slice(start..end).to_string(),
        excluded: false,
        rename: Some(Box::new(RenameEdit { new_text: e.new_text.clone(), edit: e.clone(), at: Some(at), source: RenameSource::Exact })),
    }
}

/// The file whose text the positions of an edit of `path` refer to, given the steps before it: walk them
/// backward, a rename that `path` is at or under maps it back to where it was; a create of `path` that
/// overwrites ends the walk (the file starts empty: `None`), any other create leaves it as it is.
fn source_of(path: &Path, before: &[Step]) -> Option<PathBuf> {
    let mut p = path.to_path_buf();
    for step in before.iter().rev() {
        match step {
            Step::Op { op: FileOp::Rename { from, to, .. }, .. } => {
                if let Some(back) = moved_path(&p, to, from) {
                    p = back;
                }
            }
            Step::Op { op: FileOp::Create { path: made, overwrite: true, .. }, .. } if *made == p => return None,
            _ => {}
        }
    }
    Some(p)
}

/// The preview of `req.steps` and its rows (sorted by path, line, column). Runs off the main thread.
pub fn build(req: RenameRequest) -> Result<(RenamePreview, Vec<Hit>), String> {
    let RenameRequest { old, new, steps, open, root } = req;
    let mut made: Vec<PathBuf> = Vec::new();
    let mut texts: HashMap<PathBuf, Option<Text>> = HashMap::new();
    let (mut hits, mut stamps, mut unlisted) = (Vec::new(), HashMap::new(), HashSet::new());
    for (s, step) in steps.iter().enumerate() {
        let s = s as u32;
        match step {
            Step::Op { op: FileOp::Create { path, .. } | FileOp::Rename { to: path, .. }, .. } => made.push(path.clone()),
            Step::Op { .. } => {}
            Step::Edit(f) if f.edits.is_empty() => {}
            Step::Edit(f) => {
                let under_made = made.iter().any(|m| f.path.starts_with(m));
                if !under_made && !texts.contains_key(&f.path) {
                    texts.insert(f.path.clone(), text_of(&f.path, &open, &root)?);
                }
                let Some(text) = texts.get(&f.path).and_then(Option::as_ref).filter(|_| !under_made) else {
                    unlisted.extend((0..f.edits.len() as u32).map(|e| (s, e)));
                    if let Some(src) = source_of(&f.path, &steps[..s as usize])
                        && !stamps.contains_key(&src)
                        && let Some(t) = text_of(&src, &open, &root)?
                    {
                        stamps.insert(src, t.stamp);
                    }
                    continue;
                };
                stamps.insert(f.path.clone(), text.stamp.clone());
                hits.extend(f.edits.iter().enumerate().map(|(e, edit)| row(&f.path, &text.rope, edit, (s, e as u32))));
            }
        }
    }
    hits.sort_by(|a: &Hit, b: &Hit| {
        let at = |h: &Hit| h.rename.as_ref().and_then(|r| r.at);
        (&a.path, a.line, a.col, at(a)).cmp(&(&b.path, b.line, b.col, at(b)))
    });
    Ok((RenamePreview { old, new, steps, stamps, unlisted }, hits))
}

/// The steps Replace All makes: an edit of `preview.steps` keeps the edits whose `(step, edit)` is in an included
/// row or in `unlisted`, and is dropped when none is left; operations are kept in order. Included rows without
/// an `at` (textual) become one `Step::Edit` per file (`version: None`), placed first so their positions refer
/// to the original text.
pub fn included_steps(preview: &RenamePreview, included: &[Hit]) -> Vec<Step> {
    let mut kept: HashSet<(u32, u32)> = preview.unlisted.clone();
    let mut textual: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();
    for h in included {
        let Some(r) = h.rename.as_deref() else { continue };
        match r.at {
            Some(at) => {
                kept.insert(at);
            }
            None => textual.entry(h.path.clone()).or_default().push(r.edit.clone()),
        }
    }
    let mut out: Vec<Step> =
        textual.into_iter().map(|(path, edits)| Step::Edit(FileEdit { path, version: None, edits, index: None })).collect();
    for (s, step) in preview.steps.iter().enumerate() {
        match step {
            Step::Op { .. } => out.push(step.clone()),
            Step::Edit(f) => {
                let edits: Vec<TextEdit> =
                    f.edits.iter().enumerate().filter(|(e, _)| kept.contains(&(s as u32, *e as u32))).map(|(_, e)| e.clone()).collect();
                if !edits.is_empty() {
                    out.push(Step::Edit(FileEdit { path: f.path.clone(), version: f.version, edits, index: f.index }));
                }
            }
        }
    }
    out
}

/// "rename a.rs to c.rs", paths relative to `root`.
fn describe(op: &FileOp, root: &Path) -> String {
    match op {
        FileOp::Create { path, .. } => format!("create {}", rel(path, root)),
        FileOp::Rename { from, to, .. } => format!("rename {} to {}", rel(from, root), rel(to, root)),
        FileOp::Delete { path, .. } => format!("delete {}", rel(path, root)),
    }
}

impl RenamePreview {
    /// The number of file operations in the answer.
    pub fn op_count(&self) -> usize {
        self.steps.iter().filter(|s| matches!(s, Step::Op { .. })).count()
    }

    /// The smallest stamped path that has moved on (stable for tests), against the open buffers now.
    pub fn first_stale(&self, open: &[OpenText]) -> Option<PathBuf> {
        let mut paths: Vec<&PathBuf> = self.stamps.keys().collect();
        paths.sort();
        paths
            .into_iter()
            .find(|path| match &self.stamps[*path] {
                FileStamp::OpenBuffer { id, version } => !open.iter().any(|o| o.id == *id && o.version == *version),
                FileStamp::Disk { mtime, size } => {
                    open.iter().any(|o| &o.path == *path && o.dirty)
                        || std::fs::metadata(path).ok().is_none_or(|m| m.modified().ok() != Some(*mtime) || m.len() != *size)
                }
            })
            .cloned()
    }

    /// The confirm dialog's text; `excluded` is the number of rows the user left out.
    pub fn confirm_message(&self, included: &[Hit], excluded: usize, root: &Path) -> String {
        let s = |n: usize| if n == 1 { "" } else { "s" };
        let files: HashSet<&PathBuf> = included.iter().map(|h| &h.path).collect();
        let mut out = format!(
            "Rename {} to {}: {} occurrence{} in {} file{}{}.",
            self.old,
            self.new,
            included.len(),
            s(included.len()),
            files.len(),
            s(files.len()),
            if excluded > 0 { format!("; {excluded} excluded") } else { String::new() }
        );
        if !self.unlisted.is_empty() {
            let k = self.unlisted.len();
            out.push_str(&format!("\n{k} more edit{} in files not listed.", s(k)));
        }
        let ops: Vec<String> =
            self.steps.iter().filter_map(|st| if let Step::Op { op, .. } = st { Some(describe(op, root)) } else { None }).collect();
        if !ops.is_empty() {
            out.push_str("\nFile operations:");
            for o in ops.iter().take(8) {
                out.push_str(&format!("\n  {o}"));
            }
            if ops.len() > 8 {
                out.push_str(&format!("\n  ... and {} more", ops.len() - 8));
            }
        }
        out.push_str("\nOpen files change in the editor (undoable); others are written to disk.");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn te(l0: u32, c0: u32, l1: u32, c1: u32, text: &str) -> TextEdit {
        TextEdit { range: Range::new(Position::new(l0, c0), Position::new(l1, c1)), new_text: text.into() }
    }

    fn file(path: &Path, version: Option<i32>, edits: Vec<TextEdit>) -> Step {
        Step::Edit(FileEdit { path: path.to_path_buf(), version, edits, index: None })
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-rename-preview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn req(dir: &Path, steps: Vec<Step>, open: Vec<OpenText>) -> RenameRequest {
        RenameRequest { old: "old".into(), new: "new".into(), steps, open, root: dir.to_path_buf() }
    }

    fn at(h: &Hit) -> Option<(u32, u32)> {
        h.rename.as_ref().unwrap().at
    }

    #[test]
    fn build_rows_use_utf16_positions_and_sort_by_path_line_col() {
        let dir = tmp("utf16");
        let (a, b) = (dir.join("a.rs"), dir.join("b.rs"));
        std::fs::write(&a, "fn f() {}\nlet s = \"😀\"; old(s);\n").unwrap();
        std::fs::write(&b, "// 日本 old\nold old\n").unwrap();
        // b first, then a; b's edits out of order.
        let steps = vec![
            file(&b, None, vec![te(1, 4, 1, 7, "new"), te(0, 6, 0, 9, "new"), te(1, 0, 1, 3, "new")]),
            file(&a, None, vec![te(1, 14, 1, 17, "new")]),
        ];
        let (preview, hits) = build(req(&dir, steps, Vec::new())).unwrap();
        let got: Vec<_> = hits
            .iter()
            .map(|h| (h.path.file_name().unwrap().to_str().unwrap(), h.line, h.col, h.len, h.matched.as_str(), h.preview_col))
            .collect();
        assert_eq!(
            got,
            [("a.rs", 1, 13, 3, "old", 13), ("b.rs", 0, 6, 3, "old", 6), ("b.rs", 1, 0, 3, "old", 0), ("b.rs", 1, 4, 3, "old", 4)]
        );
        assert_eq!(hits[0].preview, "let s = \"😀\"; old(s);");
        assert_eq!((at(&hits[2]), at(&hits[3]), at(&hits[1])), (Some((0, 2)), Some((0, 0)), Some((0, 1))));
        assert!(hits.iter().all(|h| !h.excluded && h.rename.as_ref().unwrap().source == RenameSource::Exact));
        assert_eq!(preview.stamps.len(), 2);
        assert!(matches!(preview.stamps[&a], FileStamp::Disk { size, .. } if size == 34));
        assert!(preview.unlisted.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_takes_open_text_over_the_disk() {
        let dir = tmp("open");
        let a = dir.join("a.rs");
        std::fs::write(&a, "disk disk disk\n").unwrap();
        let open = OpenText { path: a.clone(), id: 7, rope: Rope::from_str("x old\n"), version: 3, dirty: true };
        let (preview, hits) = build(req(&dir, vec![file(&a, Some(1), vec![te(0, 2, 0, 5, "new")])], vec![open])).unwrap();
        assert_eq!((hits[0].col, hits[0].matched.as_str(), hits[0].preview.as_str()), (2, "old", "x old"));
        assert_eq!(preview.stamps[&a], FileStamp::OpenBuffer { id: 7, version: 3 });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_length_edit_is_a_row_with_its_text() {
        let dir = tmp("zero");
        let a = dir.join("a.rs");
        std::fs::write(&a, "S { old }\n").unwrap();
        let (_, hits) = build(req(&dir, vec![file(&a, None, vec![te(0, 4, 0, 4, "new: ")])], Vec::new())).unwrap();
        let r = hits[0].rename.as_ref().unwrap();
        assert_eq!((hits[0].len, hits[0].matched.as_str(), r.new_text.as_str(), hits[0].col), (0, "", "new: ", 4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_line_edit_cuts_len_at_the_line_end() {
        let dir = tmp("multi");
        let a = dir.join("a.rs");
        std::fs::write(&a, "ab cd\nef gh\n").unwrap();
        let (_, hits) = build(req(&dir, vec![file(&a, None, vec![te(0, 3, 1, 2, "x")])], Vec::new())).unwrap();
        assert_eq!((hits[0].line, hits[0].col, hits[0].len, hits[0].matched.as_str()), (0, 3, 2, "cd\nef"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_row_after_a_create_or_rename_onto_the_path() {
        let dir = tmp("made");
        let (a, b, c, d, e) = (dir.join("a.rs"), dir.join("b.rs"), dir.join("c.rs"), dir.join("d.rs"), dir.join("gone.rs"));
        for p in [&a, &b, &c, &d] {
            std::fs::write(p, "old\n").unwrap();
        }
        let steps = vec![
            file(&a, None, vec![te(0, 0, 0, 3, "new")]),
            Step::Op { index: 1, op: FileOp::Rename { from: a.clone(), to: c.clone(), overwrite: true, ignore_if_exists: false } },
            file(&c, None, vec![te(0, 0, 0, 3, "new")]),
            Step::Op { index: 3, op: FileOp::Create { path: d.clone(), overwrite: true, ignore_if_exists: false } },
            file(&d, None, vec![te(0, 0, 0, 3, "new"), te(0, 1, 0, 2, "z")]),
            file(&e, None, vec![te(0, 0, 0, 3, "new")]),
            file(&b, None, vec![te(0, 0, 0, 3, "new")]),
        ];
        let (preview, hits) = build(req(&dir, steps, Vec::new())).unwrap();
        let paths: Vec<_> = hits.iter().map(|h| h.path.file_name().unwrap().to_str().unwrap()).collect();
        assert_eq!(paths, ["a.rs", "b.rs"]);
        let mut unlisted: Vec<_> = preview.unlisted.iter().copied().collect();
        unlisted.sort();
        assert_eq!(unlisted, [(2, 0), (4, 0), (4, 1), (5, 0)], "after the operations and for the missing file");
        let mut stamped: Vec<_> = preview.stamps.keys().cloned().collect();
        stamped.sort();
        // a.rs: its own row; b.rs: its row; d.rs is made by an overwriting create, no stamp; c.rs maps back to
        // a.rs, which has one already; gone.rs is missing.
        assert_eq!(stamped, [a, b]);
        assert_eq!(preview.op_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stamped paths of a preview, sorted.
    fn stamped(preview: &RenamePreview) -> Vec<PathBuf> {
        let mut v: Vec<_> = preview.stamps.keys().cloned().collect();
        v.sort();
        v
    }

    fn rename_op(index: u32, from: &Path, to: &Path) -> Step {
        Step::Op { index, op: FileOp::Rename { from: from.to_path_buf(), to: to.to_path_buf(), overwrite: false, ignore_if_exists: false } }
    }

    #[test]
    fn an_edit_without_a_row_stamps_the_file_it_refers_to() {
        let dir = tmp("source");
        let (a, c) = (dir.join("a.rs"), dir.join("c.rs"));
        std::fs::write(&a, "old\n").unwrap();
        // rename a.rs to c.rs, then an edit of c.rs; a.rs has no row of its own.
        let steps = vec![rename_op(0, &a, &c), file(&c, None, vec![te(0, 0, 0, 3, "new")])];
        let (preview, hits) = build(req(&dir, steps.clone(), Vec::new())).unwrap();
        assert!(hits.is_empty());
        assert_eq!(preview.unlisted, HashSet::from([(1, 0)]));
        assert_eq!(stamped(&preview), std::slice::from_ref(&a));
        assert_eq!(preview.stamps[&a], stamp_of(&a));
        assert_eq!(preview.first_stale(&[]), None);
        // The file changed on disk: refused.
        std::fs::write(&a, "old and more\n").unwrap();
        assert_eq!(preview.first_stale(&[]), Some(a.clone()));
        // Open: the stamp is the buffer's version, and a later edit of the buffer is stale.
        let open = |version| OpenText { path: a.clone(), id: 4, rope: Rope::from_str("old\n"), version, dirty: true };
        let (preview, _) = build(req(&dir, steps, vec![open(2)])).unwrap();
        assert_eq!(preview.stamps[&a], FileStamp::OpenBuffer { id: 4, version: 2 });
        assert_eq!(preview.first_stale(&[open(2)]), None);
        assert_eq!(preview.first_stale(&[open(3)]), Some(a));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_rename_maps_the_edited_path_back() {
        let dir = tmp("folder");
        let (d1, d2) = (dir.join("d1"), dir.join("d2"));
        std::fs::create_dir(&d1).unwrap();
        std::fs::write(d1.join("x.rs"), "old\n").unwrap();
        let steps = vec![rename_op(0, &d1, &d2), file(&d2.join("x.rs"), None, vec![te(0, 0, 0, 3, "new")])];
        let (preview, _) = build(req(&dir, steps, Vec::new())).unwrap();
        assert_eq!(stamped(&preview), [d1.join("x.rs")]);
        std::fs::write(d1.join("x.rs"), "old!\n").unwrap();
        assert_eq!(preview.first_stale(&[]), Some(d1.join("x.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_create_that_ignores_an_existing_file_stamps_it_and_an_overwriting_one_does_not() {
        let dir = tmp("create");
        let (keep, over, new) = (dir.join("keep.rs"), dir.join("over.rs"), dir.join("new.rs"));
        std::fs::write(&keep, "old\n").unwrap();
        std::fs::write(&over, "old\n").unwrap();
        let create = |index, path: &Path, overwrite, ignore_if_exists| Step::Op {
            index,
            op: FileOp::Create { path: path.to_path_buf(), overwrite, ignore_if_exists },
        };
        let steps = vec![
            create(0, &keep, false, true),
            file(&keep, None, vec![te(0, 0, 0, 3, "new")]),
            create(2, &over, true, false),
            file(&over, None, vec![te(0, 0, 0, 3, "new")]),
            create(4, &new, false, false),
            file(&new, None, vec![te(0, 0, 0, 0, "new")]),
        ];
        let (preview, hits) = build(req(&dir, steps, Vec::new())).unwrap();
        assert!(hits.is_empty());
        assert_eq!(preview.unlisted, HashSet::from([(1, 0), (3, 0), (5, 0)]));
        assert_eq!(stamped(&preview), std::slice::from_ref(&keep), "the existing file only: one starts empty, one is missing");
        std::fs::write(&keep, "old and more\n").unwrap();
        assert_eq!(preview.first_stale(&[]), Some(keep));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_utf8_is_an_error_naming_the_file() {
        let dir = tmp("utf8");
        let a = dir.join("a.rs");
        std::fs::write(&a, [0xff, 0xfe, b'o']).unwrap();
        let err = build(req(&dir, vec![file(&a, None, vec![te(0, 0, 0, 1, "x")])], Vec::new())).err().unwrap();
        assert_eq!(err, "a.rs is not valid UTF-8");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hand_hit(path: &Path, at: Option<(u32, u32)>, edit: TextEdit, source: RenameSource) -> Hit {
        Hit {
            path: path.to_path_buf(),
            line: 0,
            col: 0,
            len: 1,
            preview: String::new(),
            preview_col: 0,
            matched: String::new(),
            excluded: false,
            rename: Some(Box::new(RenameEdit { new_text: edit.new_text.clone(), edit, at, source })),
        }
    }

    #[test]
    fn included_steps_rebuild_rule() {
        let dir = PathBuf::from("/p");
        let (a, b, c, t1, t2) = (dir.join("a"), dir.join("b"), dir.join("c"), dir.join("t1"), dir.join("t2"));
        let op = Step::Op { index: 1, op: FileOp::Delete { path: c.clone(), recursive: false, ignore_if_not_exists: true } };
        let steps = vec![
            Step::Edit(FileEdit {
                path: a.clone(),
                version: Some(4),
                edits: vec![te(0, 0, 0, 1, "1"), te(1, 0, 1, 1, "2")],
                index: Some(0),
            }),
            op.clone(),
            Step::Edit(FileEdit { path: b.clone(), version: None, edits: vec![te(0, 0, 0, 1, "3")], index: Some(2) }),
            Step::Edit(FileEdit { path: c.clone(), version: None, edits: vec![te(0, 0, 0, 1, "4"), te(0, 2, 0, 3, "5")], index: Some(3) }),
        ];
        let preview = RenamePreview { old: "o".into(), new: "n".into(), steps, stamps: HashMap::new(), unlisted: HashSet::from([(3, 1)]) };
        let row = |p: &Path, at, e: &TextEdit| hand_hit(p, at, e.clone(), RenameSource::Exact);
        let included = vec![
            row(&a, Some((0, 1)), &te(1, 0, 1, 1, "2")),
            hand_hit(&t2, None, te(2, 0, 2, 1, "n"), RenameSource::Textual),
            hand_hit(&t1, None, te(0, 0, 0, 1, "n"), RenameSource::Textual),
            hand_hit(&t1, None, te(1, 0, 1, 1, "n"), RenameSource::Textual),
        ];
        let got = included_steps(&preview, &included);
        // b's only edit is excluded: its step is dropped. c keeps its unlisted edit. Textual files first.
        let want = vec![
            Step::Edit(FileEdit { path: t1, version: None, edits: vec![te(0, 0, 0, 1, "n"), te(1, 0, 1, 1, "n")], index: None }),
            Step::Edit(FileEdit { path: t2, version: None, edits: vec![te(2, 0, 2, 1, "n")], index: None }),
            Step::Edit(FileEdit { path: a, version: Some(4), edits: vec![te(1, 0, 1, 1, "2")], index: Some(0) }),
            op,
            Step::Edit(FileEdit { path: c, version: None, edits: vec![te(0, 2, 0, 3, "5")], index: Some(3) }),
        ];
        assert_eq!(got, want);
        let _ = b;
    }

    fn stamp_of(path: &Path) -> FileStamp {
        let m = std::fs::metadata(path).unwrap();
        FileStamp::Disk { mtime: m.modified().unwrap(), size: m.len() }
    }

    fn preview_with(stamps: Vec<(PathBuf, FileStamp)>) -> RenamePreview {
        RenamePreview {
            old: "o".into(),
            new: "n".into(),
            steps: Vec::new(),
            stamps: stamps.into_iter().collect(),
            unlisted: HashSet::new(),
        }
    }

    #[test]
    fn first_stale_cases() {
        let dir = tmp("stale");
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        for p in [&a, &b, &c] {
            std::fs::write(p, "abc").unwrap();
        }
        let open_a = |version, dirty| OpenText { path: a.clone(), id: 1, rope: Rope::new(), version, dirty };
        let buf = preview_with(vec![(a.clone(), FileStamp::OpenBuffer { id: 1, version: 5 })]);
        assert_eq!(buf.first_stale(&[open_a(5, true)]), None);
        assert_eq!(buf.first_stale(&[]), Some(a.clone()), "buffer gone");
        assert_eq!(buf.first_stale(&[open_a(6, true)]), Some(a.clone()), "other version");
        let disk = preview_with(vec![(b.clone(), stamp_of(&b))]);
        assert_eq!(disk.first_stale(&[]), None, "all fresh");
        let open_b = |dirty| OpenText { path: b.clone(), id: 2, rope: Rope::new(), version: 1, dirty };
        assert_eq!(disk.first_stale(&[open_b(false)]), None, "open and clean with the same stat");
        assert_eq!(disk.first_stale(&[open_b(true)]), Some(b.clone()), "open and dirty");
        // Another size.
        std::fs::write(&b, "abcd").unwrap();
        assert_eq!(disk.first_stale(&[]), Some(b.clone()), "size");
        // The same size, another mtime.
        std::fs::write(&c, "abc").unwrap();
        let FileStamp::Disk { mtime, size } = stamp_of(&c) else { unreachable!() };
        let old_mtime = mtime - std::time::Duration::from_secs(5);
        let by_time = preview_with(vec![(c.clone(), FileStamp::Disk { mtime: old_mtime, size })]);
        assert_eq!(by_time.first_stale(&[]), Some(c.clone()), "mtime");
        std::fs::remove_file(&c).unwrap();
        assert_eq!(preview_with(vec![(c.clone(), FileStamp::Disk { mtime, size })]).first_stale(&[]), Some(c.clone()), "gone");
        // Two stale: the smaller path.
        let both = preview_with(vec![(c.clone(), stamp_of(&b)), (b.clone(), FileStamp::Disk { mtime, size: 99 })]);
        assert_eq!(both.first_stale(&[]), Some(b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_message_lists_at_most_eight_operations() {
        let root = PathBuf::from("/p");
        let (a, c) = (root.join("a.rs"), root.join("c.rs"));
        let row = |p: &Path| hand_hit(p, Some((0, 0)), te(0, 0, 0, 1, "n"), RenameSource::Exact);
        let steps = vec![
            Step::Op { index: 0, op: FileOp::Rename { from: a.clone(), to: c.clone(), overwrite: false, ignore_if_exists: false } },
            Step::Op { index: 1, op: FileOp::Create { path: root.join("d.rs"), overwrite: false, ignore_if_exists: false } },
            Step::Op { index: 2, op: FileOp::Delete { path: root.join("e.rs"), recursive: false, ignore_if_not_exists: false } },
        ];
        let mut p = preview_with(Vec::new());
        p.old = "old".into();
        p.new = "new".into();
        p.steps = steps.clone();
        p.unlisted = HashSet::from([(1, 0), (1, 1)]);
        let included = [row(&a), row(&a), row(&root.join("b.rs"))];
        let want = "Rename old to new: 3 occurrences in 2 files.\n\
                    2 more edits in files not listed.\n\
                    File operations:\n  rename a.rs to c.rs\n  create d.rs\n  delete e.rs\n\
                    Open files change in the editor (undoable); others are written to disk.";
        assert_eq!(p.confirm_message(&included, 0, &root), want);
        let with_excluded = p.confirm_message(&included, 1, &root);
        assert_eq!(with_excluded.lines().next().unwrap(), "Rename old to new: 3 occurrences in 2 files; 1 excluded.");
        assert!(p.confirm_message(&included, 12, &root).starts_with("Rename old to new: 3 occurrences in 2 files; 12 excluded.\n"));
        // Singular, no unlisted line, no operations.
        p.unlisted = HashSet::new();
        p.steps = Vec::new();
        let want = "Rename old to new: 1 occurrence in 1 file.\nOpen files change in the editor (undoable); others are written to disk.";
        assert_eq!(p.confirm_message(&included[..1], 0, &root), want);
        // Nine operations: eight lines and the rest counted.
        p.steps = (0..9)
            .map(|i| Step::Op {
                index: i,
                op: FileOp::Delete { path: root.join(format!("f{i}")), recursive: false, ignore_if_not_exists: false },
            })
            .collect();
        let msg = p.confirm_message(&included, 0, &root);
        assert_eq!(msg.lines().filter(|l| l.starts_with("  delete")).count(), 8);
        assert!(msg.contains("\n  ... and 1 more\n"), "{msg}");
        p.unlisted = HashSet::from([(0, 0)]);
        assert!(p.confirm_message(&included, 0, &root).contains("1 more edit in files"));
    }
}
