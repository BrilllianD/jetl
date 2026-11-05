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
//!
//! The textual stage (`RenameRequest::textual`, the rename dialog's "Search in
//! comments and strings") adds guesses: whole-word matches of the old name in
//! comments and strings (or whole files with no grammar, and Markdown). `Search`
//! only names the files; positions come from the text the stamp belongs to, a
//! match touching an exact edit is dropped, and a file a file operation could
//! shift gets no guesses. Rows are excluded and `Textual`; a file with only
//! such rows is stamped apart (`TextualOutcome::stamps`) and checked only when
//! one of its rows is included.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use lsp_types::{Range, TextEdit};
use ropey::Rope;

use crate::editor::brackets::in_literal_at;
use crate::editor::buffer::BufferId;
use crate::editor::nav::moved_path;
use crate::editor::search::build_pattern;
use crate::editor::selection::line_len_no_eol;
use crate::editor::syntax::detect_text;
use crate::files::find::{Hit, MAX_HITS, Query, Search, preview};
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
    /// One per file with an exact row, taken when the preview was built; also one per file that the positions of
    /// an edit without a row refer to (the source of a rename, the target of a rename that may be skipped, an
    /// existing file a create leaves alone), when it has text. Always checked. A file with only textual rows is in
    /// `textual.stamps` instead.
    pub stamps: HashMap<PathBuf, FileStamp>,
    /// The `(step, edit)` of every edit that has no row (its file is made by an earlier operation of the
    /// answer, or has no text to show); it always applies.
    pub unlisted: HashSet<(u32, u32)>,
    /// What the textual stage did; all default without one.
    pub textual: TextualOutcome,
}

/// The textual stage of a rename: guesses beside the server's edits.
#[derive(Debug, Clone)]
pub struct TextualStage {
    pub scope: TextualScope,
    pub show_hidden: bool,
    pub show_ignored: bool,
}

#[derive(Debug, Clone)]
pub enum TextualScope {
    /// Matches inside comments and strings; a file with no grammar in this build, or Markdown, counts whole.
    CommentsAndStrings,
}

/// What the textual stage did. All default without one.
#[derive(Debug, Default)]
pub struct TextualOutcome {
    /// Rows it made.
    pub rows: usize,
    /// The search stopped at `MAX_HITS`: some files were not searched.
    pub capped: bool,
    /// Files with the name that got no rows: a file operation touches them, or they cannot be read as text.
    pub skipped_files: usize,
    /// One per file that has only textual rows; checked only when one of its rows is included.
    pub stamps: HashMap<PathBuf, FileStamp>,
}

pub struct RenameRequest {
    pub old: String,
    pub new: String,
    pub steps: Vec<Step>,
    pub open: Vec<OpenText>,
    pub root: PathBuf,
    pub textual: Option<TextualStage>,
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

/// The row of edit `e` (number `at.1` of step `at.0`, none for a textual row) in `rope`. A textual row starts
/// excluded.
fn row(path: &Path, rope: &Rope, e: &TextEdit, at: Option<(u32, u32)>, source: RenameSource) -> Hit {
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
        excluded: source == RenameSource::Textual,
        rename: Some(Box::new(RenameEdit { new_text: e.new_text.clone(), edit: e.clone(), at, source })),
    }
}

/// The files whose text the positions of an edit of `path` may refer to, given the steps before it: walk them
/// backward, a rename that a candidate is at or under maps it back to where it was. A rename with
/// `ignore_if_exists` is skipped when its target exists (decided on the disk at apply), and then the edit meets
/// the target itself, so the candidate is kept as it was and the mapped one is added. A create of a candidate
/// that overwrites ends the walk for that candidate (the file starts empty); any other create leaves it as it is.
fn source_of(path: &Path, before: &[Step]) -> Vec<PathBuf> {
    let mut paths = vec![path.to_path_buf()];
    for step in before.iter().rev() {
        match step {
            Step::Op { op: FileOp::Rename { from, to, ignore_if_exists, .. }, .. } => {
                let mut next = Vec::new();
                for p in paths {
                    let back = moved_path(&p, to, from);
                    if (back.is_none() || *ignore_if_exists) && !next.contains(&p) {
                        next.push(p);
                    }
                    if let Some(back) = back
                        && !next.contains(&back)
                    {
                        next.push(back);
                    }
                }
                paths = next;
            }
            Step::Op { op: FileOp::Create { path: made, overwrite: true, .. }, .. } => paths.retain(|p| p != made),
            _ => {}
        }
    }
    paths
}

/// Which of the matches starting at char offsets `starts` of `text` (`rope` is the same text) are in scope.
fn in_scope(scope: &TextualScope, path: &Path, text: &str, rope: &Rope, starts: &[usize]) -> Vec<bool> {
    match scope {
        TextualScope::CommentsAndStrings => {
            // Markdown has no comments or strings for prose: it counts whole, as a file with no grammar does.
            let tree = detect_text(path, text).filter(|d| d.name != "markdown").and_then(|def| {
                let mut parser = tree_sitter::Parser::new();
                parser.set_language(&def.language).ok()?;
                parser.parse(text, None)
            });
            match tree {
                Some(tree) => starts.iter().map(|&s| in_literal_at(rope, &tree, s)).collect(),
                None => vec![true; starts.len()],
            }
        }
    }
}

/// The textual rows: matches of `old` that no exact edit touches, in scope, in files no operation can shift.
/// At most `MAX_HITS` rows are made. Returns the rows, whether the stage was capped and the number of files with the name that got no rows.
fn textual_stage(
    stage: &TextualStage,
    (old, new): (&str, &str),
    steps: &[Step],
    open: &[OpenText],
    root: &Path,
    texts: &mut HashMap<PathBuf, Option<Text>>,
) -> Result<(Vec<Hit>, bool, usize), String> {
    if old.trim().is_empty() || old.contains(['\n', '\r']) || old == new {
        return Ok((Vec::new(), false, 0));
    }
    let re = build_pattern(old, false, true, true)?;
    // `Search` only names the files that hold the name.
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    let mut capped = false;
    Search {
        root: root.to_path_buf(),
        show_hidden: stage.show_hidden,
        show_ignored: stage.show_ignored,
        query: Query::Regex(re.clone()),
        seq: 0,
        cancel: Arc::new(AtomicU64::new(0)),
    }
    .run(|b| {
        found.extend(b.hits.into_iter().map(|h| h.path));
        capped |= b.capped;
    });
    let mut candidates: Vec<(PathBuf, bool)> = found.iter().map(|p| (p.clone(), true)).collect();
    for o in open.iter().filter(|o| o.dirty && o.path.starts_with(root)) {
        if !found.contains(&o.path) {
            candidates.push((o.path.clone(), false));
        }
    }
    candidates.sort();

    // Rule 3: files a textual edit could shift.
    let first_op = steps.iter().position(|s| matches!(s, Step::Op { .. }));
    let mut after_op: HashSet<PathBuf> = HashSet::new();
    if let Some(first_op) = first_op {
        for (s, step) in steps.iter().enumerate().skip(first_op + 1) {
            if let Step::Edit(f) = step
                && !f.edits.is_empty()
            {
                after_op.extend(source_of(&f.path, &steps[..s]));
            }
        }
    }
    let prefixes: Vec<&Path> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Op { op: FileOp::Create { path, .. } | FileOp::Delete { path, .. } | FileOp::Rename { to: path, .. }, .. } => {
                Some(path.as_path())
            }
            Step::Edit(_) => None,
        })
        .collect();
    // Every literal spelling of each real path an edit names.
    let mut real: HashMap<PathBuf, Vec<&Path>> = HashMap::new();
    for s in steps {
        if let Step::Edit(f) = s
            && !f.edits.is_empty()
            && let Ok(r) = std::fs::canonicalize(&f.path)
        {
            real.entry(r).or_default().push(f.path.as_path());
        }
    }

    let (mut rows, mut skipped) = (Vec::new(), 0usize);
    'files: for (path, from_search) in candidates {
        let refused = after_op.contains(&path)
            || prefixes.iter().any(|p| path.starts_with(p))
            || std::fs::canonicalize(&path)
                .ok()
                .is_some_and(|r| real.get(&r).is_some_and(|literals| literals.iter().any(|l| **l != *path)));
        if refused {
            skipped += usize::from(from_search);
            continue;
        }
        if !texts.contains_key(&path) {
            match text_of(&path, open, root) {
                Ok(t) => texts.insert(path.clone(), t),
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
        }
        let Some(Some(text)) = texts.get(&path) else { continue };
        let rope = &text.rope;
        let s = rope.to_string();
        // Rule 2: a match touching an exact edit is not offered.
        let exact: Vec<(usize, usize)> = steps
            .iter()
            .filter_map(|st| if let Step::Edit(f) = st { Some(f) } else { None })
            .filter(|f| f.path == path)
            .flat_map(|f| f.edits.iter())
            .map(|e| {
                let start = pos::char_index(rope, e.range.start);
                (start, pos::char_index(rope, e.range.end).max(start))
            })
            .collect();
        let matches: Vec<(usize, usize)> = re
            .find_iter(&s)
            .map(|m| (rope.byte_to_char(m.start()), rope.byte_to_char(m.end())))
            .filter(|&(ms, me)| !exact.iter().any(|&(start, end)| ms <= end && start <= me))
            .collect();
        if matches.is_empty() {
            continue;
        }
        let starts: Vec<usize> = matches.iter().map(|m| m.0).collect();
        let keep = in_scope(&stage.scope, &path, &s, rope, &starts);
        for ((ms, me), _) in matches.into_iter().zip(keep).filter(|(_, k)| *k) {
            if rows.len() >= MAX_HITS {
                capped = true;
                break 'files;
            }
            let edit = TextEdit { range: Range::new(pos::position(rope, ms), pos::position(rope, me)), new_text: new.into() };
            rows.push(row(&path, rope, &edit, None, RenameSource::Textual));
        }
    }
    Ok((rows, capped, skipped))
}

/// The preview of `req.steps` and its rows (sorted by path, line, column). Runs off the main thread.
pub fn build(req: RenameRequest) -> Result<(RenamePreview, Vec<Hit>), String> {
    let RenameRequest { old, new, steps, open, root, textual } = req;
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
                    for src in source_of(&f.path, &steps[..s as usize]) {
                        if !stamps.contains_key(&src)
                            && let Some(t) = text_of(&src, &open, &root)?
                        {
                            stamps.insert(src, t.stamp);
                        }
                    }
                    continue;
                };
                stamps.insert(f.path.clone(), text.stamp.clone());
                hits.extend(
                    f.edits.iter().enumerate().map(|(e, edit)| row(&f.path, &text.rope, edit, Some((s, e as u32)), RenameSource::Exact)),
                );
            }
        }
    }
    let mut outcome = TextualOutcome::default();
    if let Some(stage) = &textual {
        let (rows, capped, skipped) = textual_stage(stage, (&old, &new), &steps, &open, &root, &mut texts)?;
        for h in &rows {
            if !stamps.contains_key(&h.path)
                && let Some(Some(t)) = texts.get(&h.path)
            {
                outcome.stamps.entry(h.path.clone()).or_insert_with(|| t.stamp.clone());
            }
        }
        outcome.rows = rows.len();
        (outcome.capped, outcome.skipped_files) = (capped, skipped);
        hits.extend(rows);
    }
    hits.sort_by(|a: &Hit, b: &Hit| {
        let at = |h: &Hit| h.rename.as_ref().and_then(|r| r.at);
        (&a.path, a.line, a.col, at(a)).cmp(&(&b.path, b.line, b.col, at(b)))
    });
    Ok((RenamePreview { old, new, steps, stamps, unlisted, textual: outcome }, hits))
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

/// `path` is not what `stamp` says any more.
fn moved_on(path: &Path, stamp: &FileStamp, open: &[OpenText]) -> bool {
    match stamp {
        FileStamp::OpenBuffer { id, version } => !open.iter().any(|o| o.id == *id && o.version == *version),
        FileStamp::Disk { mtime, size } => {
            open.iter().any(|o| o.path == path && o.dirty)
                || std::fs::metadata(path).ok().is_none_or(|m| m.modified().ok() != Some(*mtime) || m.len() != *size)
        }
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
        paths.into_iter().find(|path| moved_on(path, &self.stamps[*path], open)).cloned()
    }

    /// The smallest path of an included textual row, in a file with only textual rows, that has moved on.
    pub fn first_stale_textual(&self, open: &[OpenText], included: &[Hit]) -> Option<PathBuf> {
        let paths: BTreeSet<&PathBuf> = included
            .iter()
            .filter(|h| h.rename.as_deref().is_some_and(|r| r.source == RenameSource::Textual))
            .map(|h| &h.path)
            .filter(|p| self.textual.stamps.contains_key(*p))
            .collect();
        paths.into_iter().find(|path| moved_on(path, &self.textual.stamps[*path], open)).cloned()
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
        let t = &self.textual;
        if t.rows > 0 {
            let on = included.iter().filter(|h| h.rename.as_deref().is_some_and(|r| r.source == RenameSource::Textual)).count();
            out.push_str(&format!("\nText matches (guesses): {on} included, {} left out.", t.rows.saturating_sub(on)));
        }
        if t.capped {
            out.push_str(&format!("\nText search stopped at {MAX_HITS} matches; others are not listed."));
        }
        match t.skipped_files {
            0 => {}
            1 => out.push_str("\nNo text matches offered in 1 file: a file operation touches it or it cannot be read."),
            n => out.push_str(&format!("\nNo text matches offered in {n} files: file operations touch them or they cannot be read.")),
        }
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
        RenameRequest { old: "old".into(), new: "new".into(), steps, open, root: dir.to_path_buf(), textual: None }
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
    fn a_rename_that_may_be_skipped_stamps_both_files() {
        let dir = tmp("ignore");
        let (a, c) = (dir.join("a.rs"), dir.join("c.rs"));
        std::fs::write(&a, "old\n").unwrap();
        std::fs::write(&c, "old\n").unwrap();
        let skip = Step::Op { index: 0, op: FileOp::Rename { from: a.clone(), to: c.clone(), overwrite: false, ignore_if_exists: true } };
        let steps = vec![skip, file(&c, None, vec![te(0, 0, 0, 3, "new")])];
        let (preview, hits) = build(req(&dir, steps, Vec::new())).unwrap();
        assert!(hits.is_empty());
        assert_eq!(stamped(&preview), [a.clone(), c.clone()]);
        assert_eq!(preview.first_stale(&[]), None);
        std::fs::write(&c, "old and more\n").unwrap();
        assert_eq!(preview.first_stale(&[]), Some(c));
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
        let preview = RenamePreview {
            old: "o".into(),
            new: "n".into(),
            steps,
            stamps: HashMap::new(),
            unlisted: HashSet::from([(3, 1)]),
            textual: Default::default(),
        };
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
            textual: Default::default(),
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

    fn stage() -> TextualStage {
        TextualStage { scope: TextualScope::CommentsAndStrings, show_hidden: false, show_ignored: false }
    }

    fn treq(dir: &Path, steps: Vec<Step>, open: Vec<OpenText>) -> RenameRequest {
        RenameRequest { textual: Some(stage()), ..req(dir, steps, open) }
    }

    /// `(file name, line, col)` of the textual rows.
    fn textual_at(hits: &[Hit]) -> Vec<(String, usize, usize)> {
        hits.iter()
            .filter(|h| h.rename.as_ref().is_some_and(|r| r.source == RenameSource::Textual))
            .map(|h| (h.path.file_name().unwrap().to_str().unwrap().to_string(), h.line, h.col))
            .collect()
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn textual_rows_are_comments_and_strings_excluded_and_marked() {
        let dir = tmp("textual-rust");
        let d = dir.join("d.rs");
        std::fs::write(&d, "let x = \"old\"; old(); // old\n").unwrap();
        let (preview, hits) = build(treq(&dir, Vec::new(), Vec::new())).unwrap();
        assert_eq!(textual_at(&hits), [("d.rs".to_string(), 0, 9), ("d.rs".to_string(), 0, 25)]);
        for h in &hits {
            let r = h.rename.as_ref().unwrap();
            assert!(h.excluded);
            assert_eq!((&r.source, r.at, r.new_text.as_str()), (&RenameSource::Textual, None, "new"));
        }
        assert_eq!(preview.textual.rows, 2);
        assert_eq!(preview.textual.stamps[&d], stamp_of(&d));
        assert!(preview.stamps.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn textual_stage_off_searches_nothing() {
        let dir = tmp("textual-off");
        std::fs::write(dir.join("n.txt"), "old\n").unwrap();
        let (preview, hits) = build(req(&dir, Vec::new(), Vec::new())).unwrap();
        assert!(hits.is_empty());
        let t = &preview.textual;
        assert_eq!((t.rows, t.capped, t.skipped_files, t.stamps.len()), (0, false, 0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_no_grammar_and_markdown_count_whole() {
        let dir = tmp("textual-whole");
        std::fs::write(dir.join("notes.txt"), "an old note\n").unwrap();
        std::fs::write(dir.join("README.md"), "# old\nthe old way\n").unwrap();
        let (_, hits) = build(treq(&dir, Vec::new(), Vec::new())).unwrap();
        assert_eq!(textual_at(&hits), [("README.md".to_string(), 0, 2), ("README.md".to_string(), 1, 4), ("notes.txt".to_string(), 0, 3)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_match_on_or_touching_an_exact_edit_gets_no_row() {
        // `old x`: the match is 0..3; a second `old` on line 1 always keeps its row.
        let cases = [
            ("replacement on the match", te(0, 0, 0, 3, "new")),
            ("insertion at the start", te(0, 0, 0, 0, "new: ")),
            ("insertion at the end", te(0, 3, 0, 3, "!")),
            ("clamped past the line end", te(0, 1, 0, 100, "z")),
        ];
        for (name, edit) in cases {
            let dir = tmp("textual-overlap");
            let n = dir.join("notes.txt");
            std::fs::write(&n, "old x\nold\n").unwrap();
            let (_, hits) = build(treq(&dir, vec![file(&n, None, vec![edit])], Vec::new())).unwrap();
            assert_eq!(textual_at(&hits), [("notes.txt".to_string(), 1, 0)], "{name}");
            assert_eq!(hits.iter().filter(|h| at(h).is_some()).count(), 1, "{name}: the exact row stays");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn textual_positions_round_trip_utf16() {
        let dir = tmp("textual-utf16");
        let n = dir.join("notes.txt");
        std::fs::write(&n, "// 😀 old 日本 old\n").unwrap();
        let (_, hits) = build(treq(&dir, Vec::new(), Vec::new())).unwrap();
        let rope = Rope::from_str("// 😀 old 日本 old\n");
        assert_eq!(hits.len(), 2);
        let want_cols = [5, 12];
        for (h, col) in hits.iter().zip(want_cols) {
            let e = &h.rename.as_ref().unwrap().edit;
            assert_eq!((h.col, h.matched.as_str(), h.len), (col, "old", 3));
            assert_eq!(pos::char_index(&rope, e.range.start), col);
            assert_eq!(pos::char_index(&rope, e.range.end), col + 3);
        }
        // The emoji is two UTF-16 units: the first range starts at unit 6.
        assert_eq!(hits[0].rename.as_ref().unwrap().edit.range.start.character, 6);
        let edits: Vec<TextEdit> = hits.iter().map(|h| h.rename.as_ref().unwrap().edit.clone()).collect();
        let got = crate::lsp::edits::edit_text("// 😀 old 日本 old\n", &edits);
        assert_eq!(got.unwrap().0, "// 😀 new 日本 new\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_come_from_the_open_text_not_the_disk() {
        let dir = tmp("textual-open");
        let n = dir.join("notes.txt");
        std::fs::write(&n, "old\nb\nc\n").unwrap();
        let open = OpenText { path: n.clone(), id: 3, rope: Rope::from_str("a\nb\nold\n"), version: 9, dirty: true };
        let (preview, hits) = build(treq(&dir, Vec::new(), vec![open])).unwrap();
        assert_eq!(textual_at(&hits), [("notes.txt".to_string(), 2, 0)]);
        assert_eq!(preview.textual.stamps[&n], FileStamp::OpenBuffer { id: 3, version: 9 });
        // A dirty buffer holding the name where the disk has none (a file Search never matched).
        let m = dir.join("m.txt");
        std::fs::write(&m, "nothing\n").unwrap();
        let open = OpenText { path: m.clone(), id: 4, rope: Rope::from_str("old\n"), version: 1, dirty: true };
        let (_, hits) = build(treq(&dir, Vec::new(), vec![open])).unwrap();
        assert!(textual_at(&hits).contains(&("m.txt".to_string(), 0, 0)));
        // A clean open document of a hidden file Search does not walk: no row.
        let h = dir.join(".hidden.txt");
        std::fs::write(&h, "old\n").unwrap();
        let open = OpenText { path: h, id: 5, rope: Rope::from_str("old\n"), version: 1, dirty: false };
        let (_, hits) = build(treq(&dir, Vec::new(), vec![open])).unwrap();
        assert!(!textual_at(&hits).iter().any(|r| r.0 == ".hidden.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn op_create(index: u32, path: &Path, overwrite: bool) -> Step {
        Step::Op { index, op: FileOp::Create { path: path.to_path_buf(), overwrite, ignore_if_exists: false } }
    }

    /// Files named in the textual rows, and `skipped_files`, of a build over `names` (each holding `old`).
    fn rule_3(dir: &Path, steps: Vec<Step>) -> (Vec<String>, usize) {
        let (preview, hits) = build(treq(dir, steps, Vec::new())).unwrap();
        let mut files: Vec<String> = textual_at(&hits).into_iter().map(|r| r.0).collect::<BTreeSet<_>>().into_iter().collect();
        files.sort();
        (files, preview.textual.skipped_files)
    }

    fn plain(dir: &Path, names: &[&str]) {
        for n in names {
            let p = dir.join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "an old name\n").unwrap();
        }
    }

    #[test]
    fn rule_3_files_get_no_textual_rows() {
        let e1 = te(0, 3, 0, 6, "new");
        // [rename a to c, edit c]: a and c refused; b keeps rows.
        let dir = tmp("rule3-a");
        plain(&dir, &["a.txt", "b.txt"]);
        let (a, c) = (dir.join("a.txt"), dir.join("c.txt"));
        let steps = vec![rename_op(0, &a, &c), file(&c, None, vec![te(0, 3, 0, 6, "new")])];
        assert_eq!(rule_3(&dir, steps), (vec!["b.txt".to_string()], 1));
        // [create o, edit b]: the first-op rule refuses b.
        let dir = tmp("rule3-b");
        plain(&dir, &["b.txt", "z.txt"]);
        let steps = vec![op_create(0, &dir.join("o.txt"), false), file(&dir.join("b.txt"), None, vec![e1.clone()])];
        assert_eq!(rule_3(&dir, steps), (vec!["z.txt".to_string()], 1));
        // [edit a, create o, edit a]: refused.
        let dir = tmp("rule3-c");
        plain(&dir, &["a.txt", "z.txt"]);
        let a = dir.join("a.txt");
        let steps = vec![file(&a, None, vec![e1.clone()]), op_create(1, &dir.join("o.txt"), false), file(&a, None, vec![e1.clone()])];
        assert_eq!(rule_3(&dir, steps), (vec!["z.txt".to_string()], 1));
        // [delete e]
        let dir = tmp("rule3-d");
        plain(&dir, &["e.txt", "z.txt"]);
        let del = Step::Op { index: 0, op: FileOp::Delete { path: dir.join("e.txt"), recursive: false, ignore_if_not_exists: false } };
        assert_eq!(rule_3(&dir, vec![del]), (vec!["z.txt".to_string()], 1));
        // [create over (overwrite)]
        let dir = tmp("rule3-e");
        plain(&dir, &["over.txt", "z.txt"]);
        assert_eq!(rule_3(&dir, vec![op_create(0, &dir.join("over.txt"), true)]), (vec!["z.txt".to_string()], 1));
        // [rename x to y] with y on disk: no row in y; x keeps its rows.
        let dir = tmp("rule3-f");
        plain(&dir, &["x.txt", "y.txt"]);
        let steps = vec![rename_op(0, &dir.join("x.txt"), &dir.join("y.txt"))];
        assert_eq!(rule_3(&dir, steps), (vec!["x.txt".to_string()], 1));
        // [edit a, rename a to c]: a keeps its rows (the moved-file case).
        let dir = tmp("rule3-g");
        let a = dir.join("a.txt");
        std::fs::write(&a, "an old name old\n").unwrap();
        let steps = vec![file(&a, None, vec![e1.clone()]), rename_op(1, &a, &dir.join("c.txt"))];
        assert_eq!(rule_3(&dir, steps), (vec!["a.txt".to_string()], 0));
        // A folder: [rename d1 to d2, edit d2/x]: no row in d1/x, a row in d1/y.
        let dir = tmp("rule3-h");
        plain(&dir, &["d1/x.txt", "d1/y.txt"]);
        let steps = vec![rename_op(0, &dir.join("d1"), &dir.join("d2")), file(&dir.join("d2/x.txt"), None, vec![e1])];
        let (preview, hits) = build(treq(&dir, steps, Vec::new())).unwrap();
        assert_eq!(textual_at(&hits), [("y.txt".to_string(), 0, 3)]);
        assert!(hits.iter().filter_map(|h| h.path.parent()).all(|p| p.ends_with("d1") || p.ends_with("d2")));
        assert_eq!(preview.textual.skipped_files, 1);
        for d in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            let _ =
                std::fs::remove_dir_all(std::env::temp_dir().join(format!("jetl-test-rename-preview-rule3-{d}-{}", std::process::id())));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_exact_path_refuses_textual_rows_on_the_real_file() {
        let dir = tmp("textual-link");
        std::fs::write(dir.join("real.txt"), "an old name\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();
        let link = dir.join("link.txt");
        let (preview, hits) = build(treq(&dir, vec![file(&link, None, vec![te(0, 3, 0, 6, "new")])], Vec::new())).unwrap();
        assert!(!textual_at(&hits).iter().any(|r| r.0 == "real.txt"), "{:?}", textual_at(&hits));
        assert_eq!(preview.textual.skipped_files, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn two_spellings_of_one_file_refuse_textual_rows_in_either_order() {
        let dir = tmp("textual-two");
        std::fs::write(dir.join("real.txt"), "an old name\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();
        let (link, real) = (dir.join("link.txt"), dir.join("real.txt"));
        let edit = || vec![te(0, 3, 0, 6, "new")];
        for steps in
            [vec![file(&link, None, edit()), file(&real, None, edit())], vec![file(&real, None, edit()), file(&link, None, edit())]]
        {
            let (preview, hits) = build(treq(&dir, steps, Vec::new())).unwrap();
            assert!(textual_at(&hits).is_empty(), "{:?}", textual_at(&hits));
            assert_eq!(preview.textual.skipped_files, 1, "the search finds real.txt only; the refusal counts it once");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_utf8_with_the_name_is_skipped_not_an_error() {
        let dir = tmp("textual-utf8");
        std::fs::write(dir.join("bad.txt"), b"old \xff\n").unwrap();
        let (preview, hits) = build(treq(&dir, Vec::new(), Vec::new())).unwrap();
        assert!(hits.is_empty());
        assert_eq!(preview.textual.skipped_files, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capped_search_says_so() {
        let dir = tmp("textual-cap");
        std::fs::write(dir.join("notes.txt"), "old\n".repeat(MAX_HITS + 1)).unwrap();
        let (preview, hits) = build(treq(&dir, Vec::new(), Vec::new())).unwrap();
        assert!(preview.textual.capped);
        assert!(hits.len() <= MAX_HITS, "{}", hits.len());
        let msg = preview.confirm_message(&[], 0, &dir);
        assert!(msg.contains(&format!("Text search stopped at {MAX_HITS} matches; others are not listed.")), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dirty_buffer_over_the_cap_is_capped() {
        let dir = tmp("textual-cap-dirty");
        let n = dir.join("notes.txt");
        std::fs::write(&n, "old\n").unwrap();
        let text = "old\n".repeat(MAX_HITS + 5);
        let open = OpenText { path: n, id: 1, rope: Rope::from_str(&text), version: 2, dirty: true };
        let (preview, hits) = build(treq(&dir, Vec::new(), vec![open])).unwrap();
        assert_eq!((hits.len(), preview.textual.rows, preview.textual.capped), (MAX_HITS, MAX_HITS, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_an_included_textual_row_makes_its_file_stale() {
        let dir = tmp("textual-stale");
        let n = dir.join("notes.txt");
        std::fs::write(&n, "old\n").unwrap();
        let (preview, hits) = build(treq(&dir, Vec::new(), Vec::new())).unwrap();
        std::fs::write(&n, "old and more\n").unwrap();
        assert_eq!(preview.first_stale(&[]), None);
        assert_eq!(preview.first_stale_textual(&[], &[]), None);
        assert_eq!(preview.first_stale_textual(&[], &hits), Some(n.clone()), "the row is passed as included");
        // An exact row of the same file keeps it in `stamps`, checked with nothing included.
        let dir = tmp("textual-stale-both");
        let n = dir.join("notes.txt");
        std::fs::write(&n, "old x\nold\n").unwrap();
        let (preview, _) = build(treq(&dir, vec![file(&n, None, vec![te(0, 0, 0, 3, "new")])], Vec::new())).unwrap();
        assert!(preview.stamps.contains_key(&n) && preview.textual.stamps.is_empty());
        std::fs::write(&n, "old x\nold!\n").unwrap();
        assert_eq!(preview.first_stale(&[]), Some(n));
        for d in ["textual-stale", "textual-stale-both"] {
            let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("jetl-test-rename-preview-{d}-{}", std::process::id())));
        }
    }

    #[test]
    fn confirm_message_counts_text_matches() {
        let root = PathBuf::from("/p");
        let exact = hand_hit(&root.join("a"), Some((0, 0)), te(0, 0, 0, 1, "n"), RenameSource::Exact);
        let text = hand_hit(&root.join("b"), None, te(0, 0, 0, 1, "n"), RenameSource::Textual);
        let mut p = preview_with(Vec::new());
        p.old = "old".into();
        p.new = "new".into();
        let plain = p.confirm_message(&[exact.clone(), text.clone()], 2, &root);
        p.textual = TextualOutcome { rows: 3, capped: true, skipped_files: 1, stamps: HashMap::new() };
        let got = p.confirm_message(&[exact.clone(), text.clone()], 2, &root);
        let want = "Rename old to new: 2 occurrences in 2 files; 2 excluded.\n\
                    Text matches (guesses): 1 included, 2 left out.\n\
                    Text search stopped at 10000 matches; others are not listed.\n\
                    No text matches offered in 1 file: a file operation touches it or it cannot be read.\n\
                    Open files change in the editor (undoable); others are written to disk.";
        assert_eq!(got, want);
        // Each line is absent when it does not apply.
        p.textual = TextualOutcome { rows: 0, capped: false, skipped_files: 2, stamps: HashMap::new() };
        let got = p.confirm_message(std::slice::from_ref(&exact), 0, &root);
        assert!(!got.contains("Text matches") && !got.contains("stopped"), "{got}");
        assert!(got.contains("No text matches offered in 2 files: file operations touch them or they cannot be read.\n"), "{got}");
        p.textual = TextualOutcome { rows: 1, ..Default::default() };
        let got = p.confirm_message(std::slice::from_ref(&exact), 0, &root);
        assert!(got.contains("\nText matches (guesses): 0 included, 1 left out.\n") && !got.contains("No text"), "{got}");
        // No stage: today's message.
        p.textual = TextualOutcome::default();
        assert_eq!(p.confirm_message(&[exact, text], 2, &root), plain);
        assert!(!plain.contains("Text"), "{plain}");
    }
}
