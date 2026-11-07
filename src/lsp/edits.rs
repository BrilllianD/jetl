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
//! planned here too: [`workspace_steps`] reads a `WorkspaceEdit` into its
//! ordered steps (text edits and file operations), and [`plan_workspace`]
//! checks and plans them against the open buffers (one transaction each)
//! and the disk before anything is changed; file operations are simulated
//! in order and checked with the text edits. The files that are not open
//! are edited on disk ([`edit_text`]), in order with the file operations.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp, TextEdit, WorkspaceEdit};
use ropey::Rope;
use serde_json::Value;

use super::{pos, sync};
use crate::editor::buffer::BufferId;
use crate::editor::nav::moved_path;
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

/// One file's text edits from one entry of a workspace edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    /// The version of the document the edits were made for, when the
    /// server said (`documentChanges` with a versioned identifier).
    pub version: Option<i32>,
    pub edits: Vec<TextEdit>,
    /// Index in `documentChanges` (for `failedChange`); `None` for `changes`.
    pub index: Option<u32>,
}

/// A file operation of `documentChanges`, options read (absent = false).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    Create { path: PathBuf, overwrite: bool, ignore_if_exists: bool },
    Rename { from: PathBuf, to: PathBuf, overwrite: bool, ignore_if_exists: bool },
    Delete { path: PathBuf, recursive: bool, ignore_if_not_exists: bool },
}

/// One step of a workspace edit, in the order the server gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Edit(FileEdit),
    Op { index: u32, op: FileOp },
}

/// A workspace edit's steps. `documentChanges` wins over `changes` when
/// both are there, as the protocol asks; `changes` (an unordered map) is
/// sorted by path. A URI that is not `file:` refuses it. Several entries of
/// one file are not merged here ([`plan_workspace`] does).
pub fn workspace_steps(edit: WorkspaceEdit) -> Result<Vec<Step>, String> {
    let path = |uri: &lsp_types::Uri| sync::path(uri).ok_or_else(|| format!("{} is not a file", uri.as_str()));
    let text_edits = |edits: Vec<OneOf<TextEdit, lsp_types::AnnotatedTextEdit>>| -> Vec<TextEdit> {
        edits
            .into_iter()
            .map(|e| match e {
                OneOf::Left(e) => e,
                OneOf::Right(a) => a.text_edit,
            })
            .collect()
    };
    let doc_edit = |d: lsp_types::TextDocumentEdit, index: u32| -> Result<Step, String> {
        let path = path(&d.text_document.uri)?;
        Ok(Step::Edit(FileEdit { path, version: d.text_document.version, edits: text_edits(d.edits), index: Some(index) }))
    };
    let mut out = Vec::new();
    match edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => {
            for (i, d) in edits.into_iter().enumerate() {
                out.push(doc_edit(d, i as u32)?);
            }
        }
        Some(DocumentChanges::Operations(ops)) => {
            for (i, op) in ops.into_iter().enumerate() {
                let index = i as u32;
                let op = match op {
                    DocumentChangeOperation::Edit(d) => {
                        out.push(doc_edit(d, index)?);
                        continue;
                    }
                    DocumentChangeOperation::Op(ResourceOp::Create(c)) => {
                        let o = c.options.as_ref();
                        FileOp::Create {
                            path: path(&c.uri)?,
                            overwrite: o.and_then(|o| o.overwrite).unwrap_or(false),
                            ignore_if_exists: o.and_then(|o| o.ignore_if_exists).unwrap_or(false),
                        }
                    }
                    DocumentChangeOperation::Op(ResourceOp::Rename(r)) => {
                        let o = r.options.as_ref();
                        FileOp::Rename {
                            from: path(&r.old_uri)?,
                            to: path(&r.new_uri)?,
                            overwrite: o.and_then(|o| o.overwrite).unwrap_or(false),
                            ignore_if_exists: o.and_then(|o| o.ignore_if_exists).unwrap_or(false),
                        }
                    }
                    DocumentChangeOperation::Op(ResourceOp::Delete(d)) => {
                        let o = d.options.as_ref();
                        FileOp::Delete {
                            path: path(&d.uri)?,
                            recursive: o.and_then(|o| o.recursive).unwrap_or(false),
                            ignore_if_not_exists: o.and_then(|o| o.ignore_if_not_exists).unwrap_or(false),
                        }
                    }
                };
                out.push(Step::Op { index, op });
            }
        }
        None => {
            let mut changes: Vec<_> = edit.changes.unwrap_or_default().into_iter().collect();
            changes.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            for (uri, edits) in changes {
                out.push(Step::Edit(FileEdit { path: path(&uri)?, version: None, edits, index: None }));
            }
        }
    }
    Ok(out)
}

/// `first`'s changes, then `then`'s, as one edit with `documentChanges`, each in its own order. A `changes`
/// map (read only when its edit has no `documentChanges`) becomes unversioned `TextDocumentEdit`s sorted by
/// URI, as [`workspace_steps`] orders it. The annotations of both are kept; `then`'s wins on an id clash.
pub fn chain(first: WorkspaceEdit, then: WorkspaceEdit) -> WorkspaceEdit {
    let mut ops = Vec::new();
    let mut annotations = std::collections::HashMap::new();
    for edit in [first, then] {
        match edit.document_changes {
            Some(DocumentChanges::Edits(edits)) => ops.extend(edits.into_iter().map(DocumentChangeOperation::Edit)),
            Some(DocumentChanges::Operations(more)) => ops.extend(more),
            None => {
                let mut changes: Vec<_> = edit.changes.unwrap_or_default().into_iter().collect();
                changes.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                ops.extend(changes.into_iter().map(|(uri, edits)| {
                    DocumentChangeOperation::Edit(lsp_types::TextDocumentEdit {
                        text_document: lsp_types::OptionalVersionedTextDocumentIdentifier { uri, version: None },
                        edits: edits.into_iter().map(OneOf::Left).collect(),
                    })
                }));
            }
        }
        annotations.extend(edit.change_annotations.unwrap_or_default());
    }
    WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(ops)),
        change_annotations: (!annotations.is_empty()).then_some(annotations),
    }
}

/// What a workspace edit changed: files, and edits in them; and the steps
/// not made after one that failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditCounts {
    pub files: usize,
    pub edits: usize,
    pub skipped: usize,
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
    /// Unsaved edits.
    pub dirty: bool,
}

/// What is at a path, as `symlink_metadata` sees it (links not followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Missing,
    File,
    Link,
    Dir { empty: bool },
}

/// The disk as the planner may look at it. [`RealDisk`] in the app, a map
/// in tests.
pub trait Disk {
    fn entry(&self, path: &Path) -> Entry;
    /// Whether the nearest existing ancestor of `path` (symlinks followed)
    /// is `root` or under it.
    fn inside(&self, root: &Path, path: &Path) -> bool;
}

/// The file system, looked at with `lstat`s (no file is read).
pub struct RealDisk;

impl Disk for RealDisk {
    fn entry(&self, path: &Path) -> Entry {
        use std::io::ErrorKind;
        match path.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() => Entry::Link,
            Ok(m) if m.is_dir() => Entry::Dir { empty: std::fs::read_dir(path).is_ok_and(|mut r| r.next().is_none()) },
            Ok(_) => Entry::File,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Entry::Missing,
            // Something is there: creates are refused, and running the
            // step reports the real error.
            Err(_) => Entry::File,
        }
    }

    fn inside(&self, root: &Path, path: &Path) -> bool {
        // `root` is canonical (`Workspace::switch_root`).
        path.ancestors().find_map(|a| a.canonicalize().ok()).is_some_and(|c| c.starts_with(root))
    }
}

/// A step that changes the disk, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskStep {
    Edit(FileEdit),
    Op { index: u32, op: crate::files::ops::FsOp },
}

/// A workspace edit checked and planned.
#[derive(Debug, Clone, Default)]
pub struct WorkspacePlan {
    /// One transaction per open buffer, with its number of changes.
    pub open: Vec<(BufferId, Plan, usize)>,
    /// Clean tabs of files the edit deletes or replaces: closed before the
    /// disk steps.
    pub close: Vec<BufferId>,
    /// Closed-file edits and file operations, in order.
    pub disk: Vec<DiskStep>,
}

/// An open buffer as the simulation sees it.
struct VTab<'a> {
    doc: &'a OpenDoc,
    /// Where its file is after the operations so far.
    path: PathBuf,
    moved: bool,
    closed: bool,
    edits: Vec<TextEdit>,
}

/// What the operations so far did to the disk.
enum Over {
    /// Created or overwritten.
    File(PathBuf),
    /// A parent folder a create or rename makes.
    Dir(PathBuf),
    /// Deleted, with everything under it.
    Gone(PathBuf),
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
}

struct Sim<'a> {
    root: &'a Path,
    disk: &'a dyn Disk,
    tabs: Vec<VTab<'a>>,
    overlay: Vec<Over>,
    plan: WorkspacePlan,
}

impl Sim<'_> {
    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(self.root).unwrap_or(p).display().to_string()
    }

    /// What is at `p` after the operations so far.
    fn resolve(&self, p: &Path) -> Entry {
        let mut p = p.to_path_buf();
        for o in self.overlay.iter().rev() {
            match o {
                Over::File(q) if p == *q => return Entry::File,
                Over::File(q) if p.starts_with(q) => return Entry::Missing,
                Over::Dir(q) if p == *q => return Entry::Dir { empty: false },
                Over::Gone(q) if p.starts_with(q) => return Entry::Missing,
                Over::Moved { from, to } => {
                    if let Some(before) = moved_path(&p, to, from) {
                        p = before;
                    } else if p.starts_with(from) {
                        return Entry::Missing;
                    }
                }
                _ => {}
            }
        }
        self.disk.entry(&p)
    }

    /// Whether `p` is an empty folder; anything unsure is not.
    fn dir_empty(&self, p: &Path) -> bool {
        self.resolve(p) == Entry::Dir { empty: true }
            && !self.overlay.iter().any(|o| match o {
                Over::File(q) | Over::Dir(q) | Over::Moved { to: q, .. } => q.starts_with(p) && q != p,
                Over::Gone(_) => false,
            })
    }

    /// Open tabs of `p` or under it.
    fn tabs_at(&self, p: &Path) -> Vec<usize> {
        (0..self.tabs.len()).filter(|&i| !self.tabs[i].closed && self.tabs[i].path.starts_with(p)).collect()
    }

    /// A file operation may change `p`.
    fn check_op_path(&self, p: &Path) -> Result<(), String> {
        use std::path::Component;
        if p.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir)) {
            return Err(format!("{} is not a valid path", p.display()));
        }
        if !p.starts_with(self.root) || !self.disk.inside(self.root, p) {
            return Err(format!("{} is outside the project", p.display()));
        }
        if p == self.root {
            return Err("the edit would change the project folder itself".into());
        }
        if p.strip_prefix(self.root).is_ok_and(|r| r.components().any(|c| c.as_os_str() == ".git")) {
            return Err(format!("{} is inside .git", self.rel(p)));
        }
        Ok(())
    }

    /// The folders between the root and `p` that are missing (nearest the
    /// root first); one that is a file refuses it.
    fn missing_parents(&self, p: &Path) -> Result<Vec<PathBuf>, String> {
        let mut missing = Vec::new();
        for a in p.ancestors().skip(1).take_while(|a| a.starts_with(self.root) && *a != self.root) {
            match self.resolve(a) {
                Entry::Missing => missing.push(a.to_path_buf()),
                Entry::Dir { .. } => {}
                Entry::File | Entry::Link => return Err(format!("{}: {} is a file", self.rel(p), self.rel(a))),
            }
        }
        missing.reverse();
        Ok(missing)
    }

    /// `p` is replaced: its clean tabs close; dirty or edited ones refuse.
    fn replace_tabs(&mut self, p: &Path, verb: &str) -> Result<(), String> {
        for i in self.tabs_at(p) {
            let t = &self.tabs[i];
            if t.doc.dirty {
                return Err(format!("{} has unsaved changes", t.doc.name));
            }
            if !t.edits.is_empty() {
                return Err(format!("{} is edited and {verb} by the same edit", t.doc.name));
            }
            let id = t.doc.id;
            self.tabs[i].closed = true;
            self.plan.close.push(id);
        }
        Ok(())
    }

    fn emit(&mut self, index: u32, op: crate::files::ops::FsOp) {
        self.plan.disk.push(DiskStep::Op { index, op });
    }

    fn op(&mut self, index: u32, op: FileOp) -> Result<(), String> {
        use crate::files::ops::FsOp;
        match op {
            FileOp::Create { path, overwrite, ignore_if_exists } => {
                self.check_op_path(&path)?;
                let rel = self.rel(&path);
                match self.resolve(&path) {
                    Entry::Missing => {
                        let parents = self.missing_parents(&path)?;
                        self.overlay.extend(parents.into_iter().map(Over::Dir));
                        self.overlay.push(Over::File(path.clone()));
                        self.emit(index, FsOp::Create { path, is_dir: false, overwrite: false });
                    }
                    Entry::Link if overwrite => return Err(format!("{rel} is a symbolic link")),
                    Entry::File | Entry::Link if overwrite => {
                        self.replace_tabs(&path, "replaced")?;
                        self.overlay.push(Over::File(path.clone()));
                        self.emit(index, FsOp::Create { path, is_dir: false, overwrite: true });
                    }
                    Entry::Dir { .. } if overwrite => return Err(format!("{rel} is a folder")),
                    _ if ignore_if_exists => {}
                    _ => return Err(format!("{rel} already exists")),
                }
            }
            FileOp::Rename { from, to, overwrite, ignore_if_exists } => {
                self.check_op_path(&from)?;
                self.check_op_path(&to)?;
                if from == to {
                    return Ok(());
                }
                if to.starts_with(&from) {
                    return Err(format!("cannot move {} into itself", self.rel(&from)));
                }
                let source = self.resolve(&from);
                if source == Entry::Missing {
                    return Err(format!("{} does not exist", self.rel(&from)));
                }
                let parents = self.missing_parents(&to)?;
                match self.resolve(&to) {
                    Entry::Missing => {}
                    Entry::Dir { .. } if overwrite => return Err(format!("{} is a folder", self.rel(&to))),
                    _ if overwrite && matches!(source, Entry::Dir { .. }) => {
                        return Err(format!("cannot replace the file {} with a folder", self.rel(&to)));
                    }
                    _ if overwrite => self.replace_tabs(&to, "replaced")?,
                    _ if ignore_if_exists => return Ok(()),
                    _ => return Err(format!("{} already exists", self.rel(&to))),
                }
                // Tabs follow, dirty ones too: nothing is lost.
                for i in self.tabs_at(&from) {
                    let t = &mut self.tabs[i];
                    if let Some(moved) = moved_path(&t.path, &from, &to) {
                        t.path = moved;
                        t.moved = true;
                    }
                }
                self.overlay.extend(parents.into_iter().map(Over::Dir));
                self.overlay.push(Over::Moved { from: from.clone(), to: to.clone() });
                self.emit(index, FsOp::Rename { from, to, overwrite });
            }
            FileOp::Delete { path, recursive, ignore_if_not_exists } => {
                self.check_op_path(&path)?;
                let rel = self.rel(&path);
                match self.resolve(&path) {
                    Entry::Missing if ignore_if_not_exists => return Ok(()),
                    Entry::Missing => return Err(format!("{rel} does not exist")),
                    Entry::Dir { .. } if !recursive && !self.dir_empty(&path) => {
                        return Err(format!("{rel} is a folder that is not empty"));
                    }
                    _ => {}
                }
                self.replace_tabs(&path, "deleted")?;
                self.overlay.push(Over::Gone(path.clone()));
                self.emit(index, FsOp::Delete { path, recursive });
            }
        }
        Ok(())
    }

    fn edit(&mut self, f: FileEdit) -> Result<(), String> {
        if let Some(t) = self.tabs.iter_mut().find(|t| !t.closed && t.path == f.path) {
            // After a rename in this edit the server's version is of a
            // document it has not seen opened.
            if !t.moved
                && let (Some(want), Some(have)) = (f.version, t.doc.version)
                && want != have
            {
                return Err(format!("{} changed since the language server made the edit", t.doc.name));
            }
            t.edits.extend(f.edits);
            return Ok(());
        }
        match self.resolve(&f.path) {
            Entry::Missing => return Err(format!("{} does not exist", self.rel(&f.path))),
            Entry::Dir { .. } => return Err(format!("{} is a folder", self.rel(&f.path))),
            Entry::File | Entry::Link => {}
        }
        // Merged with the file's last edit unless an operation came after it.
        let last = self.plan.disk.iter().rposition(|s| matches!(s, DiskStep::Edit(e) if e.path == f.path));
        if let Some(i) = last
            && !self.plan.disk[i + 1..].iter().any(|s| matches!(s, DiskStep::Op { .. }))
            && let DiskStep::Edit(e) = &mut self.plan.disk[i]
        {
            e.version = e.version.or(f.version);
            e.edits.extend(f.edits);
        } else {
            self.plan.disk.push(DiskStep::Edit(f));
        }
        Ok(())
    }
}

/// Whether two of `edits` overlap by LSP position: ordered by start, insertions before replacements at the same
/// start, then input order, as `plan` orders chars.
fn overlap_in_lsp(edits: &[TextEdit]) -> bool {
    let key = |e: &TextEdit| ((e.range.start.line, e.range.start.character), (e.range.end.line, e.range.end.character));
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| {
        let (start, end) = key(&edits[i]);
        (start, end > start, i)
    });
    order.windows(2).any(|w| key(&edits[w[0]]).1 > key(&edits[w[1]]).0)
}

/// Check `steps` in order against the open buffers `open` and `disk`,
/// simulating the file operations, and plan them. Any problem refuses the
/// whole edit, so that nothing is changed: an open buffer not at the
/// version the server made its edits for, overlapping edits, a file
/// operation its options do not allow, outside `root` or inside `.git`, or
/// one that would delete or replace a file with unsaved (or planned) edits.
pub fn plan_workspace(steps: Vec<Step>, open: &[OpenDoc], root: &Path, disk: &dyn Disk) -> Result<WorkspacePlan, String> {
    let tabs = open.iter().map(|doc| VTab { doc, path: doc.path.clone(), moved: false, closed: false, edits: Vec::new() }).collect();
    let mut sim = Sim { root, disk, tabs, overlay: Vec::new(), plan: WorkspacePlan::default() };
    for step in steps {
        match step {
            Step::Edit(f) => sim.edit(f)?,
            Step::Op { index, op } => sim.op(index, op)?,
        }
    }
    // Closed files: overlap is found here, before anything changes, from the positions alone. They are not
    // clamped, so a pair that overlaps only after clamping still fails when the file is written, and a pair past
    // a line end or the end of the file that clamping would separate is refused here (it only refuses; nothing
    // else changes).
    for step in &sim.plan.disk {
        if let DiskStep::Edit(f) = step
            && overlap_in_lsp(&f.edits)
        {
            return Err(format!("the edits of {} overlap", sim.rel(&f.path)));
        }
    }
    let mut plan = sim.plan;
    for t in sim.tabs.iter().filter(|t| !t.edits.is_empty()) {
        let doc = t.doc;
        let edits: Vec<CharEdit> = t
            .edits
            .iter()
            .map(|e| {
                let mut c = CharEdit::from_lsp(&doc.rope, e);
                c.text = line_breaks(&c.text, 0, doc.crlf).0;
                c
            })
            .collect();
        let p = self::plan(&doc.rope, &edits).map_err(|_| format!("the edits of {} overlap", doc.name))?;
        let n = p.changes.len();
        plan.open.push((doc.id, p, n));
    }
    Ok(plan)
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

/// A change annotation of a workspace edit, for the confirmation dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationRow {
    pub label: String,
    pub description: Option<String>,
    pub needs_confirmation: bool,
}

/// The ids of the change annotations that the changes of the raw `WorkspaceEdit` `raw` refer to:
/// `documentChanges[*].edits[*].annotationId` and `documentChanges[*].annotationId`.
fn referenced_annotations(raw: &Value) -> HashSet<&str> {
    let mut ids = HashSet::new();
    let Some(changes) = raw.get("documentChanges").and_then(Value::as_array) else { return ids };
    for change in changes {
        ids.extend(change.get("annotationId").and_then(Value::as_str));
        for edit in change.get("edits").and_then(Value::as_array).into_iter().flatten() {
            ids.extend(edit.get("annotationId").and_then(Value::as_str));
        }
    }
    ids
}

/// Drop the `changeAnnotations` of `edit` that no change of its raw JSON `raw` refers to.
pub fn prune_annotations(raw: &Value, edit: &mut WorkspaceEdit) {
    let Some(annotations) = edit.change_annotations.as_mut() else { return };
    let used = referenced_annotations(raw);
    annotations.retain(|id, _| used.contains(id.as_str()));
    if annotations.is_empty() {
        edit.change_annotations = None;
    }
}

/// A `WorkspaceEdit` read from JSON, with only the annotations that its changes refer to.
pub fn read_workspace_edit(raw: Value) -> Result<WorkspaceEdit, serde_json::Error> {
    let mut edit = serde_json::from_value::<WorkspaceEdit>(raw.clone())?;
    prune_annotations(&raw, &mut edit);
    Ok(edit)
}

/// When an annotation of `edit` has `needsConfirmation: true`: every annotation (those needing
/// confirmation first, then by label, then by id) and the number of changes of the edit (text
/// edits and file operations). `None` otherwise.
///
/// Decided on `changeAnnotations` alone, and the changes are not counted per annotation:
/// lsp-types reads an annotated text edit as a plain one (`OneOf<TextEdit, AnnotatedTextEdit>`
/// tries `TextEdit` first) and drops a `DeleteFile`'s top-level `annotationId`, so which change
/// refers to which annotation is lost. Asking for the whole edit is the safe side. An edit read
/// with [`read_workspace_edit`] or pruned with [`prune_annotations`] only has the annotations
/// some change refers to (rust-analyzer lists an unreferenced one in every edit).
pub fn confirmation(edit: &WorkspaceEdit) -> Option<(Vec<AnnotationRow>, usize)> {
    let annotations = edit.change_annotations.as_ref()?;
    if !annotations.values().any(|a| a.needs_confirmation == Some(true)) {
        return None;
    }
    let total = match &edit.document_changes {
        Some(DocumentChanges::Edits(docs)) => docs.iter().map(|d| d.edits.len()).sum(),
        Some(DocumentChanges::Operations(ops)) => ops
            .iter()
            .map(|op| match op {
                DocumentChangeOperation::Edit(d) => d.edits.len(),
                DocumentChangeOperation::Op(_) => 1,
            })
            .sum(),
        None => edit.changes.iter().flat_map(|c| c.values()).map(Vec::len).sum(),
    };
    let mut rows: Vec<(&String, AnnotationRow)> = annotations
        .iter()
        .map(|(id, a)| {
            let row = AnnotationRow {
                label: a.label.clone(),
                description: a.description.clone(),
                needs_confirmation: a.needs_confirmation == Some(true),
            };
            (id, row)
        })
        .collect();
    rows.sort_by(|(ia, a), (ib, b)| (!a.needs_confirmation, &a.label, ia).cmp(&(!b.needs_confirmation, &b.label, ib)));
    Some((rows.into_iter().map(|(_, r)| r).collect(), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::Buffer;
    use crate::editor::selection::Selection;
    use crate::editor::undo::TxKind;
    use crate::files::ops::FsOp;
    use std::collections::HashMap;

    fn run(text: &str, edits: &[CharEdit]) -> (String, Plan) {
        let mut b = Buffer::from_text(1, None, text, false);
        let plan = plan(&b.rope, edits).unwrap();
        b.apply(plan.changes.clone(), Selection::point(0), TxKind::Other);
        (b.text(), plan)
    }

    fn line_edit(l: u32, text: &str) -> lsp_types::TextEdit {
        serde_json::from_value(
            serde_json::json!({"range": {"start": {"line": l, "character": 0}, "end": {"line": l, "character": 1}}, "newText": text}),
        )
        .unwrap()
    }

    #[test]
    fn chain_puts_the_first_edits_changes_before_the_seconds() {
        let uri = |s: &str| s.parse::<lsp_types::Uri>().unwrap();
        let first = WorkspaceEdit {
            changes: Some(HashMap::from([
                (uri("file:///p/b.rs"), vec![line_edit(1, "b")]),
                (uri("file:///p/a.rs"), vec![line_edit(0, "a")]),
            ])),
            ..Default::default()
        };
        let then: WorkspaceEdit = serde_json::from_value(serde_json::json!({"documentChanges": [
            {"kind": "rename", "oldUri": "file:///p/x.rs", "newUri": "file:///p/y.rs"}]}))
        .unwrap();
        let merged = chain(first, then);
        assert!(merged.changes.is_none());
        let steps = workspace_steps(merged).unwrap();
        let shape: Vec<String> = steps
            .iter()
            .map(|s| match s {
                Step::Edit(e) => format!("edit {}", e.path.display()),
                Step::Op { op: FileOp::Rename { from, to, .. }, .. } => format!("rename {} {}", from.display(), to.display()),
                s => panic!("{s:?}"),
            })
            .collect();
        assert_eq!(shape, ["edit /p/a.rs", "edit /p/b.rs", "rename /p/x.rs /p/y.rs"]);
    }

    #[test]
    fn chain_keeps_the_annotations_of_both() {
        let ann = |id: &str, label: &str| serde_json::json!({id: {"label": label}});
        let first: WorkspaceEdit = serde_json::from_value(serde_json::json!({"changeAnnotations": ann("one", "first")})).unwrap();
        let then: WorkspaceEdit = serde_json::from_value(serde_json::json!({"changeAnnotations": ann("two", "second")})).unwrap();
        let both = chain(first, then).change_annotations.unwrap();
        assert_eq!(both.len(), 2);
        assert_eq!(both["one"].label, "first");
        assert_eq!(both["two"].label, "second");
        let clash_a: WorkspaceEdit = serde_json::from_value(serde_json::json!({"changeAnnotations": ann("x", "old")})).unwrap();
        let clash_b: WorkspaceEdit = serde_json::from_value(serde_json::json!({"changeAnnotations": ann("x", "new")})).unwrap();
        assert_eq!(chain(clash_a, clash_b).change_annotations.unwrap()["x"].label, "new");
    }

    fn annotated_edit(needs_a: Option<bool>) -> WorkspaceEdit {
        let uri = "file:///p/a.rs";
        let te = |id: Option<&str>| {
            let t =
                serde_json::json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "x"});
            match id {
                Some(id) => {
                    let mut t = t;
                    t["annotationId"] = id.into();
                    t
                }
                None => t,
            }
        };
        let mut a = serde_json::json!({"label": "Rename in tests"});
        if let Some(n) = needs_a {
            a["needsConfirmation"] = n.into();
        }
        serde_json::from_value(serde_json::json!({
            "documentChanges": [
                {"textDocument": {"uri": uri, "version": 1}, "edits": [te(Some("a")), te(Some("a")), te(Some("b")), te(None)]},
                {"kind": "rename", "oldUri": "file:///p/b.rs", "newUri": "file:///p/c.rs", "annotationId": "b"}
            ],
            "changeAnnotations": {"a": a, "b": {"label": "Outside", "description": "Edit outside of the workspace"}}
        }))
        .unwrap()
    }

    #[test]
    fn annotations_needing_confirmation_are_listed() {
        let (rows, total) = confirmation(&annotated_edit(Some(true))).unwrap();
        let row = |label: &str, description: Option<&str>, needs| AnnotationRow {
            label: label.into(),
            description: description.map(String::from),
            needs_confirmation: needs,
        };
        assert_eq!(rows, [row("Rename in tests", None, true), row("Outside", Some("Edit outside of the workspace"), false)]);
        // Four text edits and a rename.
        assert_eq!(total, 5);
    }

    fn raw_edit(change: Value) -> Value {
        serde_json::json!({
            "documentChanges": [change],
            "changeAnnotations": {"Out": {"label": "Outside", "needsConfirmation": true}}
        })
    }

    fn text_change(annotation: Option<&str>) -> Value {
        let mut e =
            serde_json::json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "x"});
        if let Some(id) = annotation {
            e["annotationId"] = id.into();
        }
        serde_json::json!({"textDocument": {"uri": "file:///p/a.rs", "version": null}, "edits": [e]})
    }

    #[test]
    fn an_unreferenced_annotation_is_pruned() {
        let edit = read_workspace_edit(raw_edit(text_change(None))).unwrap();
        assert_eq!(edit.change_annotations, None);
        assert_eq!(confirmation(&edit), None);
    }

    #[test]
    fn an_annotation_of_a_text_edit_is_kept() {
        let edit = read_workspace_edit(raw_edit(text_change(Some("Out")))).unwrap();
        let (rows, total) = confirmation(&edit).unwrap();
        assert_eq!((rows.len(), total), (1, 1));
        // A reference to an unknown id keeps nothing.
        let edit = read_workspace_edit(raw_edit(text_change(Some("Other")))).unwrap();
        assert_eq!(confirmation(&edit), None);
    }

    #[test]
    fn an_annotation_of_a_resource_op_is_kept() {
        for change in [
            serde_json::json!({"kind": "rename", "oldUri": "file:///p/b.rs", "newUri": "file:///p/c.rs", "annotationId": "Out"}),
            serde_json::json!({"kind": "delete", "uri": "file:///p/b.rs", "annotationId": "Out"}),
        ] {
            let edit = read_workspace_edit(raw_edit(change)).unwrap();
            assert_eq!(confirmation(&edit).map(|(rows, total)| (rows.len(), total)), Some((1, 1)));
        }
    }

    #[test]
    fn no_annotation_needing_confirmation_is_none() {
        assert_eq!(confirmation(&annotated_edit(Some(false))), None);
        assert_eq!(confirmation(&annotated_edit(None)), None);
        assert_eq!(confirmation(&WorkspaceEdit::default()), None);
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
            dirty: false,
        }
    }

    fn dirty_doc(id: BufferId, path: &str) -> OpenDoc {
        OpenDoc { dirty: true, ..open_doc(id, path, "x\n", None) }
    }

    struct FakeDisk {
        entries: HashMap<PathBuf, Entry>,
        outside: Vec<PathBuf>,
    }

    impl Disk for FakeDisk {
        fn entry(&self, p: &Path) -> Entry {
            self.entries.get(p).copied().unwrap_or(Entry::Missing)
        }
        fn inside(&self, root: &Path, p: &Path) -> bool {
            p.starts_with(root) && !self.outside.iter().any(|o| p.starts_with(o))
        }
    }

    /// Root `/w` with `entries` in it.
    fn disk(entries: &[(&str, Entry)]) -> FakeDisk {
        let mut map: HashMap<PathBuf, Entry> = entries.iter().map(|(p, e)| (PathBuf::from(p), *e)).collect();
        map.insert("/w".into(), Entry::Dir { empty: false });
        FakeDisk { entries: map, outside: Vec::new() }
    }

    fn create(path: &str, overwrite: bool, ignore_if_exists: bool) -> Step {
        Step::Op { index: 0, op: FileOp::Create { path: path.into(), overwrite, ignore_if_exists } }
    }

    fn rename(from: &str, to: &str, overwrite: bool, ignore_if_exists: bool) -> Step {
        Step::Op { index: 0, op: FileOp::Rename { from: from.into(), to: to.into(), overwrite, ignore_if_exists } }
    }

    fn delete(path: &str, recursive: bool, ignore_if_not_exists: bool) -> Step {
        Step::Op { index: 0, op: FileOp::Delete { path: path.into(), recursive, ignore_if_not_exists } }
    }

    fn text_edit(path: &str, version: Option<i32>, text: &str) -> Step {
        Step::Edit(FileEdit { path: path.into(), version, edits: vec![te(0, 0, 0, 0, text)], index: None })
    }

    fn plan_with(steps: Vec<Step>, open: &[OpenDoc], d: &FakeDisk) -> Result<WorkspacePlan, String> {
        plan_workspace(steps, open, Path::new("/w"), d)
    }

    /// The file operations planned.
    fn ops(plan: &WorkspacePlan) -> Vec<FsOp> {
        plan.disk
            .iter()
            .filter_map(|s| match s {
                DiskStep::Op { op, .. } => Some(op.clone()),
                DiskStep::Edit(_) => None,
            })
            .collect()
    }

    fn err(steps: Vec<Step>, open: &[OpenDoc], d: &FakeDisk, want: &str) {
        let e = plan_with(steps, open, d).unwrap_err();
        assert!(e.contains(want), "{e:?} should contain {want:?}");
    }

    #[test]
    fn workspace_steps_keep_order_options_and_indices() {
        // `changes`: by path, no index.
        let we: WorkspaceEdit = serde_json::from_value(serde_json::json!({"changes": {
            "file:///w/b.rs": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "B"}],
            "file:///w/a.rs": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "A"}],
        }}))
        .unwrap();
        let steps = workspace_steps(we).unwrap();
        let paths: Vec<_> = steps
            .iter()
            .map(|s| match s {
                Step::Edit(f) => (f.path.to_str().unwrap().to_string(), f.version, f.index),
                Step::Op { .. } => panic!("{s:?}"),
            })
            .collect();
        assert_eq!(paths, [("/w/a.rs".to_string(), None, None), ("/w/b.rs".to_string(), None, None)]);
        // `documentChanges` wins over `changes`; order, versions, options
        // (absent = false) and indices kept; annotated edits are edits.
        let edit = |text: &str| serde_json::json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": text});
        let we: WorkspaceEdit = serde_json::from_value(serde_json::json!({
            "changes": {"file:///w/c.rs": [edit("ignored")]},
            "documentChanges": [
                {"textDocument": {"uri": "file:///w/a.rs", "version": 3}, "edits": [edit("1"), {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "x", "annotationId": "a"}]},
                {"kind": "create", "uri": "file:///w/new.rs", "options": {"overwrite": true, "ignoreIfExists": true}},
                {"kind": "rename", "oldUri": "file:///w/a.rs", "newUri": "file:///w/z.rs"},
                {"kind": "delete", "uri": "file:///w/d", "options": {"recursive": true}},
            ],
        }))
        .unwrap();
        let steps = workspace_steps(we).unwrap();
        assert_eq!(steps.len(), 4);
        match &steps[0] {
            Step::Edit(f) => {
                assert_eq!((f.path.to_str().unwrap(), f.version, f.index), ("/w/a.rs", Some(3), Some(0)));
                assert_eq!(f.edits.iter().map(|e| e.new_text.as_str()).collect::<Vec<_>>(), ["1", "x"]);
            }
            s => panic!("{s:?}"),
        }
        assert_eq!(
            steps[1..],
            [
                Step::Op { index: 1, op: FileOp::Create { path: "/w/new.rs".into(), overwrite: true, ignore_if_exists: true } },
                Step::Op {
                    index: 2,
                    op: FileOp::Rename { from: "/w/a.rs".into(), to: "/w/z.rs".into(), overwrite: false, ignore_if_exists: false }
                },
                Step::Op { index: 3, op: FileOp::Delete { path: "/w/d".into(), recursive: true, ignore_if_not_exists: false } },
            ]
        );
        let we = serde_json::json!({"changes": {"untitled:x": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "x"}]}});
        assert!(workspace_steps(serde_json::from_value(we).unwrap()).unwrap_err().contains("not a file"));
    }

    #[test]
    fn overlapping_edits_of_a_closed_file_are_refused_before_anything_is_planned() {
        let d = disk(&[("/w/a.rs", Entry::File), ("/w/b.rs", Entry::File)]);
        let two = |a: TextEdit, b: TextEdit| Step::Edit(FileEdit { path: "/w/a.rs".into(), version: None, edits: vec![a, b], index: None });
        let other = || text_edit("/w/b.rs", None, "x");
        // Overlap: refused, naming the file relative to the root, nothing planned.
        err(vec![other(), two(te(0, 0, 0, 5, "x"), te(0, 3, 0, 8, "y"))], &[], &d, "the edits of a.rs overlap");
        // Input order does not matter, nor does a replacement inside another one.
        err(vec![two(te(1, 0, 1, 2, "x"), te(0, 0, 3, 0, "y"))], &[], &d, "the edits of a.rs overlap");
        // Touching, equal-position inserts and an insert at a replacement's start are fine.
        for (a, b) in [
            (te(0, 0, 0, 3, "x"), te(0, 3, 0, 5, "y")),
            (te(0, 2, 0, 2, "x"), te(0, 2, 0, 2, "y")),
            (te(0, 2, 0, 5, "x"), te(0, 2, 0, 2, "y")),
        ] {
            let plan = plan_with(vec![two(a, b)], &[], &d).unwrap();
            assert_eq!(plan.disk.len(), 1);
        }
        // Edits merged from two steps of the same file are checked together.
        err(
            vec![
                text_edit("/w/a.rs", None, "x"),
                two(te(0, 0, 0, 0, "i"), te(0, 0, 0, 4, "r")),
                two(te(0, 2, 0, 3, "z"), te(0, 9, 0, 9, "q")),
            ],
            &[],
            &d,
            "the edits of a.rs overlap",
        );
    }

    #[test]
    fn create_options_follow_the_spec() {
        let d = disk(&[("/w/a.rs", Entry::File), ("/w/l.rs", Entry::Link), ("/w/dir", Entry::Dir { empty: true }), ("/w/f", Entry::File)]);
        let plan = plan_with(vec![create("/w/new/n.rs", false, false)], &[], &d).unwrap();
        assert_eq!(ops(&plan), [FsOp::Create { path: "/w/new/n.rs".into(), is_dir: false, overwrite: false }]);
        err(vec![create("/w/a.rs", false, false)], &[], &d, "a.rs already exists");
        assert!(plan_with(vec![create("/w/a.rs", false, true)], &[], &d).unwrap().disk.is_empty());
        let plan = plan_with(vec![create("/w/a.rs", true, true)], &[], &d).unwrap();
        assert_eq!(ops(&plan), [FsOp::Create { path: "/w/a.rs".into(), is_dir: false, overwrite: true }]);
        err(vec![create("/w/dir", true, false)], &[], &d, "dir is a folder");
        assert!(plan_with(vec![create("/w/dir", false, true)], &[], &d).unwrap().disk.is_empty());
        err(vec![create("/w/l.rs", true, false)], &[], &d, "l.rs is a symbolic link");
        err(vec![create("/w/f/x.rs", false, false)], &[], &d, "f/x.rs: f is a file");
        // Creating twice in one edit: the second sees the first.
        err(vec![create("/w/n.rs", false, false), create("/w/n.rs", false, false)], &[], &d, "n.rs already exists");
        // Overwriting an open file: a clean tab closes, a dirty one refuses.
        let plan = plan_with(vec![create("/w/a.rs", true, false)], &[open_doc(1, "/w/a.rs", "a", None)], &d).unwrap();
        assert_eq!(plan.close, [1]);
        err(vec![create("/w/a.rs", true, false)], &[dirty_doc(1, "/w/a.rs")], &d, "a.rs has unsaved changes");
    }

    #[test]
    fn rename_options_and_tabs() {
        let d = disk(&[("/w/a.rs", Entry::File), ("/w/taken.rs", Entry::File), ("/w/d", Entry::Dir { empty: false })]);
        err(vec![rename("/w/gone.rs", "/w/x.rs", false, false)], &[], &d, "gone.rs does not exist");
        err(vec![rename("/w/a.rs", "/w/taken.rs", false, false)], &[], &d, "taken.rs already exists");
        assert!(plan_with(vec![rename("/w/a.rs", "/w/taken.rs", false, true)], &[], &d).unwrap().disk.is_empty());
        let plan = plan_with(vec![rename("/w/a.rs", "/w/taken.rs", true, false)], &[], &d).unwrap();
        assert_eq!(ops(&plan), [FsOp::Rename { from: "/w/a.rs".into(), to: "/w/taken.rs".into(), overwrite: true }]);
        err(vec![rename("/w/d", "/w/d/e", false, false)], &[], &d, "cannot move d into itself");
        err(vec![rename("/w/d", "/w/taken.rs", true, false)], &[], &d, "cannot replace the file taken.rs with a folder");
        err(vec![rename("/w/a.rs", "/w/d", true, false)], &[], &d, "d is a folder");
        assert!(plan_with(vec![rename("/w/a.rs", "/w/a.rs", false, false)], &[], &d).unwrap().disk.is_empty());
        // A dirty tab at the source moves with it.
        let plan = plan_with(vec![rename("/w/a.rs", "/w/b.rs", false, false)], &[dirty_doc(1, "/w/a.rs")], &d).unwrap();
        assert!(plan.close.is_empty());
        assert_eq!(ops(&plan).len(), 1);
        // The tab of an overwritten target: clean closes, dirty refuses.
        let plan = plan_with(vec![rename("/w/a.rs", "/w/taken.rs", true, false)], &[open_doc(2, "/w/taken.rs", "t", None)], &d).unwrap();
        assert_eq!(plan.close, [2]);
        err(vec![rename("/w/a.rs", "/w/taken.rs", true, false)], &[dirty_doc(2, "/w/taken.rs")], &d, "taken.rs has unsaved changes");
        // A renamed file is no longer at its old path.
        err(vec![rename("/w/a.rs", "/w/b.rs", false, false), rename("/w/a.rs", "/w/c.rs", false, false)], &[], &d, "a.rs does not exist");
    }

    #[test]
    fn edits_follow_renames_and_creates() {
        let d = disk(&[("/w/a.rs", Entry::File), ("/w/c.rs", Entry::File)]);
        // The open tab follows its rename; no version check after the move.
        let steps =
            vec![text_edit("/w/a.rs", Some(4), "1"), rename("/w/a.rs", "/w/b.rs", false, false), text_edit("/w/b.rs", Some(9), "2")];
        let plan = plan_with(steps, &[open_doc(1, "/w/a.rs", "x\n", Some(4))], &d).unwrap();
        assert_eq!(plan.open.len(), 1);
        let (id, p, n) = &plan.open[0];
        assert_eq!((*id, *n), (1, 2));
        assert_eq!(p.changes.len(), 2);
        assert_eq!(ops(&plan).len(), 1);
        // A version mismatch before the move still refuses.
        let steps = vec![text_edit("/w/a.rs", Some(3), "1"), rename("/w/a.rs", "/w/b.rs", false, false)];
        err(steps, &[open_doc(1, "/w/a.rs", "x\n", Some(4))], &d, "a.rs changed since");
        // A closed file renamed, then edited at its new path.
        let plan = plan_with(vec![rename("/w/c.rs", "/w/d.rs", false, false), text_edit("/w/d.rs", None, "x")], &[], &d).unwrap();
        assert!(
            matches!(&plan.disk[..], [DiskStep::Op { op: FsOp::Rename { .. }, .. }, DiskStep::Edit(f)] if f.path == Path::new("/w/d.rs"))
        );
        // Edited at the old path after the rename: gone.
        err(vec![rename("/w/c.rs", "/w/d.rs", false, false), text_edit("/w/c.rs", None, "x")], &[], &d, "c.rs does not exist");
        // A created file, then edited.
        let plan = plan_with(vec![create("/w/n.rs", false, false), text_edit("/w/n.rs", None, "x")], &[], &d).unwrap();
        assert!(
            matches!(&plan.disk[..], [DiskStep::Op { op: FsOp::Create { .. }, .. }, DiskStep::Edit(f)] if f.path == Path::new("/w/n.rs"))
        );
        err(vec![text_edit("/w/missing.rs", None, "x")], &[], &d, "missing.rs does not exist");
        // Two edits of a closed file merge unless an operation is between.
        let plan = plan_with(vec![text_edit("/w/c.rs", None, "1"), text_edit("/w/c.rs", Some(2), "2")], &[], &d).unwrap();
        assert!(matches!(&plan.disk[..], [DiskStep::Edit(f)] if f.edits.len() == 2 && f.version == Some(2)));
        let steps = vec![text_edit("/w/c.rs", None, "1"), create("/w/n.rs", false, false), text_edit("/w/c.rs", None, "2")];
        let plan = plan_with(steps, &[], &d).unwrap();
        assert_eq!(plan.disk.len(), 3);
    }

    #[test]
    fn edits_of_a_closed_file_on_either_side_of_an_operation_are_not_merged_for_overlap() {
        let d = disk(&[("/w/c.rs", Entry::File)]);
        let at = |path: &str, text: &str| {
            Step::Edit(FileEdit { path: path.into(), version: None, edits: vec![te(0, 0, 0, 5, text)], index: None })
        };
        let plan = plan_with(vec![at("/w/c.rs", "1"), create("/w/n.rs", false, false), at("/w/c.rs", "2")], &[], &d).unwrap();
        assert_eq!(plan.disk.len(), 3);
        // Merged, the same two overlap.
        err(vec![at("/w/c.rs", "1"), at("/w/c.rs", "2")], &[], &d, "the edits of c.rs overlap");
    }

    #[test]
    fn delete_rules() {
        let d = disk(&[
            ("/w/a.rs", Entry::File),
            ("/w/full", Entry::Dir { empty: false }),
            ("/w/full/x.rs", Entry::File),
            ("/w/empty", Entry::Dir { empty: true }),
        ]);
        err(vec![delete("/w/gone.rs", false, false)], &[], &d, "gone.rs does not exist");
        assert!(plan_with(vec![delete("/w/gone.rs", false, true)], &[], &d).unwrap().disk.is_empty());
        err(vec![delete("/w/full", false, false)], &[], &d, "full is a folder that is not empty");
        let plan = plan_with(vec![delete("/w/empty", false, false)], &[], &d).unwrap();
        assert_eq!(ops(&plan), [FsOp::Delete { path: "/w/empty".into(), recursive: false }]);
        err(vec![create("/w/empty/n.rs", false, false), delete("/w/empty", false, false)], &[], &d, "not empty");
        let plan = plan_with(vec![delete("/w/full", true, false)], &[open_doc(3, "/w/full/x.rs", "x", None)], &d).unwrap();
        assert_eq!(plan.close, [3]);
        err(vec![delete("/w/full", true, false)], &[dirty_doc(3, "/w/full/x.rs")], &d, "x.rs has unsaved changes");
        let steps = vec![text_edit("/w/a.rs", None, "x"), delete("/w/a.rs", false, false)];
        err(steps, &[open_doc(1, "/w/a.rs", "a", None)], &d, "a.rs is edited and deleted by the same edit");
        // Deleted twice: the second sees the first.
        err(vec![delete("/w/a.rs", false, false), delete("/w/a.rs", false, false)], &[], &d, "a.rs does not exist");
    }

    #[test]
    fn paths_outside_the_project_are_refused() {
        let mut d = disk(&[("/w/a.rs", Entry::File), ("/etc/x", Entry::File), ("/w/link", Entry::Link)]);
        d.outside = vec!["/w/link".into()];
        err(vec![create("/etc/x", false, true)], &[], &d, "/etc/x is outside the project");
        err(vec![rename("/w/a.rs", "/etc/y", false, false)], &[], &d, "/etc/y is outside the project");
        err(vec![delete("/w", true, false)], &[], &d, "project folder itself");
        err(vec![rename("/w", "/w2", false, false)], &[], &d, "project folder itself");
        err(vec![create("/w/.git/config", true, false)], &[], &d, ".git/config is inside .git");
        err(vec![create("/w/link/x.rs", false, false)], &[], &d, "/w/link/x.rs is outside the project");
        err(vec![create("/w/../etc/x", false, false)], &[], &d, "is not a valid path");
        // A text edit alone of a file outside the project is still made.
        let plan = plan_with(vec![text_edit("/etc/x", None, "x")], &[], &d).unwrap();
        assert_eq!(plan.disk.len(), 1);
    }

    #[test]
    fn workspace_plan_edits_open_buffers_and_leaves_closed_files() {
        // The open buffer is dirty (not what is on disk): its text is what
        // the positions refer to. 😀 is two UTF-16 units.
        let a = "let s = \"😀\"; old(s);\n// old\n";
        let files = [
            FileEdit {
                path: "/w/a.rs".into(),
                version: Some(4),
                edits: vec![te(0, 14, 0, 17, "renamed"), te(1, 3, 1, 6, "new")],
                index: None,
            },
            FileEdit { path: "/w/b.rs".into(), version: None, edits: vec![te(0, 0, 0, 3, "new")], index: None },
        ];
        let steps = || files.iter().cloned().map(Step::Edit).collect::<Vec<_>>();
        let d = disk(&[("/w/a.rs", Entry::File), ("/w/b.rs", Entry::File)]);
        let open = [open_doc(1, "/w/a.rs", a, Some(4))];
        let plan = plan_with(steps(), &open, &d).unwrap();
        assert_eq!(plan.disk, [DiskStep::Edit(files[1].clone())]);
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
        let e = plan_with(steps(), &[open_doc(1, "/w/a.rs", a, Some(5))], &d).unwrap_err();
        assert!(e.contains("a.rs changed"), "{e}");
        // Not open on a server, or no version given: no check.
        assert!(plan_with(steps(), &[open_doc(1, "/w/a.rs", a, None)], &d).is_ok());
        // Overlapping edits refuse it too.
        let bad = vec![Step::Edit(FileEdit {
            path: "/w/a.rs".into(),
            version: None,
            edits: vec![te(0, 0, 0, 5, "x"), te(0, 2, 0, 8, "y")],
            index: None,
        })];
        assert!(plan_with(bad, &open, &d).unwrap_err().contains("overlap"));
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
