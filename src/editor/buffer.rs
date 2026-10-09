//! A text buffer: rope, selection, undo history, file metadata.
//! Every mutation goes through [`Buffer::apply`].

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::Instant;

use ropey::Rope;

use super::fold::Fold;
use super::indent::{FileSettings, IndentStyle};
use super::selection::{self, Selection, Selections, line_len_no_eol, line_range};
use super::undo::{Change, Transaction, TxKind, UndoStack};
use super::view_state::{EditLog, LogEdit, ViewState};
use crate::workspace::SessionFold;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

pub type BufferId = u64;

/// One caret's part of an edit made at every caret
/// ([`Buffer::edit_carets`]): `range` of the text before the edit becomes
/// `text`, and the caret lands as `after` says.
#[derive(Debug, Clone)]
pub struct CaretEdit {
    pub range: Range<usize>,
    pub text: String,
    pub after: After,
    /// How undo groups the edit: see [`TxKind`].
    pub kind: TxKind,
}

/// Where a caret lands after its [`CaretEdit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    /// The selection stays on its text (moved along by the edits before it).
    Keep,
    /// Anchor and head as offsets from `range.start` in the edited text.
    At { anchor: usize, head: usize },
}

impl CaretEdit {
    /// No change; the caret stays.
    pub fn keep(sel: Selection) -> Self {
        Self { range: sel.head..sel.head, text: String::new(), after: After::Keep, kind: TxKind::Other }
    }

    /// `range` becomes `text`, the caret after it.
    pub fn replace(range: Range<usize>, text: impl Into<String>, kind: TxKind) -> Self {
        let text = text.into();
        let n = text.chars().count();
        Self { range, text, after: After::At { anchor: n, head: n }, kind }
    }

    /// The caret `off` chars after the start of the range instead.
    pub fn caret_at(self, off: usize) -> Self {
        Self { after: After::At { anchor: off, head: off }, ..self }
    }

    /// Changes no text (it may still move the caret).
    fn is_noop(&self) -> bool {
        self.range.is_empty() && self.text.is_empty()
    }
}

/// The buffer's language as picked by hand (status bar, `editor.set_language`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LanguageChoice {
    /// Detected from the path.
    #[default]
    Auto,
    /// No language, even where the path suggests one.
    Plain,
    Named(&'static str),
}

#[derive(Debug)]
pub struct Buffer {
    pub id: BufferId,
    pub path: Option<PathBuf>,
    pub name: String,
    pub rope: Rope,
    /// Bumped on every text change (edits, undo, redo); caches such as
    /// syntax, search and gutter marks compare against it.
    pub version: u64,
    /// `undo.state_id()` when the buffer was last saved or loaded; dirty
    /// means the undo stack is somewhere else.
    pub saved_state: u64,
    /// Caret, selection, scroll and find bar of the live view (the one
    /// drawn or acted on now); other views of this buffer are parked.
    pub view: ViewState,
    /// Text changes since the last frame, for the parked views.
    pub edit_log: EditLog,
    pub undo: UndoStack,
    pub line_ending: LineEnding,
    pub lossy: bool,
    /// Set after each apply; cleared by whoever consumes it (syntax, gutter).
    pub changed_since_render: bool,
    pub syntax: Option<super::syntax::Syntax>,
    pub language_choice: LanguageChoice,
    /// Language found from the path, shebang or modeline when opened; kept
    /// for files too large to get a syntax attached.
    pub detected_language: Option<&'static super::syntax::LanguageDef>,
    pub gutter: Vec<super::gutter::GutterMark>,
    /// Buffer version the gutter marks were computed for.
    pub gutter_version: u64,
    /// HEAD version of the file for gutter marks; `None` for untracked
    /// files, files outside a repository, or before the blob arrives.
    pub diff_base: Option<String>,
    /// Blob request that produced `diff_base`.
    pub diff_base_seq: u64,
    /// The blame column, when shown (`editor.toggle_blame`).
    pub blame: Option<super::blame::BlameState>,
    /// Modification time of the file when it was loaded or last saved;
    /// autosave refuses to write over a file that changed since.
    pub disk_mtime: Option<std::time::SystemTime>,
    /// Version the editor's idle autosave timer last saw.
    pub autosave_seen: u64,
    /// Version of the last snapshot of unsaved edits taken
    /// (`Editor::unsaved_captures`).
    pub history_seen: u64,
    /// Version at which an autosave failed; no retry until the next edit.
    pub autosave_failed: Option<u64>,
    /// The file changed on disk under unsaved edits; no autosave until an
    /// explicit save.
    pub autosave_conflict: bool,
    /// Bookmarked lines (F11) and their mnemonic digit, if any (Ctrl+F11;
    /// the editor keeps each digit unique across buffers). Every applied
    /// change (edits, undo, redo) carries them along with their text; lines
    /// deleted outright drop out, and of two marks merged into one line a
    /// digit wins over none.
    pub bookmarks: BTreeMap<usize, Option<u8>>,
    /// What Tab, Enter and indent/unindent insert; set by the editor on
    /// open from `.editorconfig`, the text or the config.
    pub indent: IndentStyle,
    /// The file's `.editorconfig` settings (all unset without one).
    pub settings: FileSettings,
    /// Open on a language server: edits are recorded in `lsp_pending`.
    pub lsp_tracked: bool,
    /// Edits since the last `didChange`, in order, as `contentChanges`
    /// (`lsp::sync`). Taken by `App::lsp_sync`.
    pub lsp_pending: Vec<crate::lsp::sync::DocEdit>,
    /// The next `didChange` sends the whole text: too many edits piled up,
    /// or one could not be put in LSP terms.
    pub lsp_full_resync: bool,
    /// Language server diagnostics placed in the text (gutter glyphs,
    /// underlines, F2); rebuilt by `App::sync_diag_marks` when the store,
    /// the text or the path changed.
    pub diag_marks: crate::lsp::diagnostics::DiagMarks,
    /// The language server has quick fixes on this line at this version
    /// (`app/lightbulb.rs`); drawn on the caret line while both still hold.
    pub lightbulb: Option<(u64, usize)>,
    /// The last `fold_candidates` answer: version, lines and the folds.
    pub fold_cache: Option<(u64, std::ops::Range<usize>, Vec<super::fold::Fold>)>,
    /// No edit changes the text (the conflict resolver's yours and theirs
    /// panes): `apply` only moves the carets, undo and redo do nothing.
    pub read_only: bool,
    /// Text restored after a crash (`Editor::recover_text`): no autosave
    /// until an explicit save writes it.
    pub recovered: bool,
}

impl Buffer {
    /// The live view's rows: its folds and its wrap.
    pub fn rows(&self) -> super::rows::Rows<'_> {
        super::rows::Rows::view(&self.view.folds, &self.view.wrap, &self.rope, self.version)
    }

    pub fn from_text(id: BufferId, path: Option<PathBuf>, text: &str, lossy: bool) -> Self {
        let line_ending = if text.contains("\r\n") { LineEnding::CrLf } else { LineEnding::Lf };
        let name = path.as_ref().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "untitled".into());
        Self {
            id,
            path,
            name,
            rope: Rope::from_str(text),
            version: 0,
            saved_state: 0,
            view: ViewState::default(),
            edit_log: EditLog::default(),
            undo: UndoStack::new(),
            line_ending,
            lossy,
            changed_since_render: true,
            syntax: None,
            language_choice: LanguageChoice::Auto,
            detected_language: None,
            gutter: Vec::new(),
            gutter_version: u64::MAX,
            diff_base: None,
            diff_base_seq: 0,
            blame: None,
            disk_mtime: None,
            autosave_seen: 0,
            history_seen: 0,
            autosave_failed: None,
            autosave_conflict: false,
            bookmarks: BTreeMap::new(),
            indent: IndentStyle::default(),
            settings: FileSettings::default(),
            lsp_tracked: false,
            lsp_pending: Vec::new(),
            lsp_full_resync: false,
            diag_marks: Default::default(),
            lightbulb: None,
            fold_cache: None,
            read_only: false,
            recovered: false,
        }
    }

    pub fn dirty(&self) -> bool {
        self.undo.state_id() != self.saved_state
    }

    /// Modified, has a file, and nothing holds autosave back. Files that
    /// were not valid UTF-8 are left to an explicit save, which re-encodes;
    /// so is text recovered after a crash.
    pub fn autosave_candidate(&self) -> bool {
        self.path.is_some()
            && self.dirty()
            && !self.lossy
            && !self.recovered
            && !self.autosave_conflict
            && self.autosave_failed != Some(self.version)
    }

    /// Whether the file's modification time differs from the one recorded at
    /// load or the last save (a vanished file counts as changed).
    pub fn changed_on_disk(&self) -> bool {
        match (&self.path, self.disk_mtime) {
            (Some(path), Some(m)) => file_mtime(path) != Some(m),
            _ => false,
        }
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// The text of the primary selection.
    pub fn selected_text(&self) -> String {
        let r = self.view.sel().range();
        self.rope.slice(r).to_string()
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        selection::pos_to_line_col(&self.rope, self.view.sel().head)
    }

    /// Apply a transaction. Change positions refer to the buffer state after
    /// the previous change in the same transaction. A single selection for
    /// `sel_after` leaves one caret.
    pub fn apply(&mut self, changes: Vec<Change>, sel_after: impl Into<Selections>, kind: TxKind) {
        let sel_after = sel_after.into();
        self.apply_then(changes, kind, |_| sel_after);
    }

    /// Like [`Buffer::apply`], but the selections after the transaction are
    /// computed from the edited rope, so redo restores the same carets.
    pub fn apply_then<S: Into<Selections>>(&mut self, changes: Vec<Change>, kind: TxKind, sel_after: impl FnOnce(&Rope) -> S) {
        self.view.expand_stack.clear();
        self.view.occ_stack.clear();
        self.view.occ_whole_word = false;
        self.view.block = None;
        if changes.is_empty() {
            self.view.sels = sel_after(&self.rope).into();
            return;
        }
        if self.read_only {
            return;
        }
        let sel_before = self.view.sels.clone();
        for c in &changes {
            self.apply_change(c);
        }
        let mut sel_after: Selections = sel_after(&self.rope).into();
        sel_after.clamp(self.rope.len_chars());
        sel_after.reset_desired();
        debug_assert!(sel_after.is_normalized());
        self.view.sels = sel_after.clone();
        self.version += 1;
        self.changed_since_render = true;
        self.undo.push(Transaction { id: 0, changes, sel_before, sel_after, at: Instant::now(), kind, line_ending: None });
    }

    fn apply_change(&mut self, c: &Change) {
        let old_len = c.old.chars().count();
        let at = c.at.min(self.rope.len_chars());
        let new_len = c.new.chars().count();
        // Parked views follow through the log; a whole-text replacement
        // (a reload) leaves them nothing to map.
        let whole = at == 0 && old_len > 0 && old_len == self.rope.len_chars();
        self.edit_log.push(if whole { LogEdit::Reset } else { LogEdit::Edit { at, old_len, new_len } });
        let marks: Vec<(MarkTarget, Option<u8>)> =
            self.bookmarks.iter().map(|(&l, &d)| (mark_target(&self.rope, l, at, old_len, c), d)).collect();
        let end = (at + old_len).min(self.rope.len_chars());
        self.record_lsp_edit(at, end, &c.new);
        if old_len > 0 {
            self.rope.remove(at..end);
        }
        if !c.new.is_empty() {
            self.rope.insert(at, &c.new);
        }
        if !marks.is_empty() {
            let (len, last) = (self.rope.len_chars(), self.rope.len_lines() - 1);
            let mut moved = BTreeMap::new();
            for (t, digit) in marks {
                let line = match t {
                    MarkTarget::Pos(p) => self.rope.char_to_line(p.min(len)),
                    MarkTarget::Line(l) => l.min(last),
                    MarkTarget::Drop => continue,
                };
                let slot = moved.entry(line).or_insert(None);
                if slot.is_none() {
                    *slot = digit;
                }
            }
            self.bookmarks = moved;
        }
        if let Some(s) = &mut self.syntax {
            s.note_change(&self.rope, at, old_len, new_len, &c.old, &c.new);
        }
        // The live view's folds follow like its carets; a reload drops them.
        if whole {
            self.view.folds.clear();
        } else {
            self.view.folds.map_edit(at, old_len, new_len);
        }
        self.view.folds.rebuild(&self.rope);
    }

    /// Record the replacement of `start..end` by `text` for the language
    /// server, before the rope changes.
    fn record_lsp_edit(&mut self, start: usize, end: usize, text: &str) {
        if !self.lsp_tracked || self.lsp_full_resync {
            return;
        }
        match crate::lsp::sync::DocEdit::new(&self.rope, start, end, text) {
            Some(edit) if self.lsp_pending.len() < crate::lsp::sync::MAX_PENDING => self.lsp_pending.push(edit),
            _ => {
                self.lsp_pending.clear();
                self.lsp_full_resync = true;
            }
        }
    }

    pub fn undo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        let Some(tx) = self.undo.pop_undo() else { return false };
        for c in tx.changes.iter().rev() {
            let inv = c.inverse();
            self.apply_change(&inv);
        }
        self.view.sels = tx.sel_before;
        self.view.sels.clamp(self.rope.len_chars());
        self.view.occ_stack.clear();
        self.view.block = None;
        if let Some((before, _)) = tx.line_ending {
            self.line_ending = before;
        }
        self.version += 1;
        self.changed_since_render = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        let Some(tx) = self.undo.pop_redo() else { return false };
        for c in &tx.changes {
            self.apply_change(c);
        }
        self.view.sels = tx.sel_after;
        self.view.sels.clamp(self.rope.len_chars());
        self.view.occ_stack.clear();
        self.view.block = None;
        if let Some((_, after)) = tx.line_ending {
            self.line_ending = after;
        }
        self.version += 1;
        self.changed_since_render = true;
        true
    }

    /// Make one edit at every caret: `f` plans each caret's part against
    /// the text as it is now (called for each caret in order), and they
    /// become one transaction. Parts are
    /// applied back to front, so every position stays one of the text
    /// before the edit; parts that overlap (or insert at the same place)
    /// collapse into the first one, and carets that meet afterwards merge.
    /// The transaction is `Typing` or `Deleting` only when every caret's
    /// part is, so undo groups it like the single-caret edit.
    pub fn edit_carets(&mut self, mut f: impl FnMut(&Buffer, Selection) -> CaretEdit) {
        let primary = self.view.sels.primary_index();
        let mut parts: Vec<(CaretEdit, Selection, bool)> =
            self.view.sels.iter().enumerate().map(|(i, s)| (f(self, *s), *s, i == primary)).collect();
        parts.sort_by_key(|(e, _, _)| (e.range.start, e.range.end));
        // Coalesce: (edit, member selections, holds the primary).
        let mut groups: Vec<(CaretEdit, Vec<Selection>, bool)> = Vec::with_capacity(parts.len());
        for (e, s, p) in parts {
            if let Some(last) = groups.last_mut() {
                let a = &last.0;
                let together = !e.is_noop() && !a.is_noop() && e.range.start == a.range.start;
                if e.range.start < a.range.end || together {
                    // The first part's text stands for the union. Text a
                    // later part reaches past its end goes, unless that part
                    // keeps it as it is.
                    if e.range.end > a.range.end {
                        let tail = self.rope.slice(a.range.end..e.range.end).to_string();
                        if e.text.ends_with(&tail) {
                            last.0.text.push_str(&tail);
                        }
                        last.0.range.end = e.range.end;
                    }
                    last.1.push(s);
                    last.2 |= p;
                    continue;
                }
            }
            groups.push((e, vec![s], p));
        }
        let mut sels = Vec::with_capacity(groups.len());
        let mut primary = 0;
        let mut delta = 0isize;
        let mut kind = None;
        for (e, members, p) in &groups {
            let (r, n) = (e.range.clone(), e.text.chars().count());
            let shift = |x: usize| (x as isize + delta) as usize;
            if *p {
                primary = sels.len();
            }
            match e.after {
                After::Keep => {
                    let map = |x: usize| {
                        if x < r.start {
                            shift(x)
                        } else if x >= r.end {
                            shift(x - r.len() + n)
                        } else {
                            shift(r.start)
                        }
                    };
                    if *p {
                        primary = sels.len() + members.iter().position(|s| self.view.sels.primary() == *s).unwrap_or(0);
                    }
                    sels.extend(members.iter().map(|s| Selection::new(map(s.anchor), map(s.head))));
                }
                After::At { anchor, head } => sels.push(Selection::new(shift(r.start + anchor), shift(r.start + head))),
            }
            if !e.is_noop() {
                kind = match kind {
                    None => Some(e.kind),
                    Some(k) if k == e.kind => Some(k),
                    Some(_) => Some(TxKind::Other),
                };
            }
            delta += n as isize - r.len() as isize;
        }
        let changes: Vec<Change> = groups
            .iter()
            .rev()
            .flat_map(|(e, _, _)| minimal_changes(e.range.start, &self.rope.slice(e.range.clone()).to_string(), &e.text))
            .collect();
        let sels = Selections::new(sels, primary);
        self.apply(changes, sels, kind.unwrap_or(TxKind::Other));
    }

    /// Replace each selection (or insert at each caret) with `s`.
    pub fn insert(&mut self, s: &str, kind: TxKind) {
        self.edit_carets(|_, sel| CaretEdit::replace(sel.range(), s, kind));
    }

    /// Insert `pieces[i]` at the i-th caret (replacing its selection); there
    /// is one piece per caret.
    pub fn insert_pieces(&mut self, pieces: &[String]) {
        let mut next = pieces.iter();
        self.edit_carets(|_, sel| CaretEdit::replace(sel.range(), next.next().map_or("", String::as_str), TxKind::Other));
    }

    /// What a copy takes at each caret: its selection, or its whole line
    /// with a line break (once per line).
    pub fn clip_pieces(&self) -> Vec<String> {
        let mut last_line = None;
        let mut pieces = Vec::new();
        for s in self.view.sels.iter() {
            if !s.is_empty() {
                pieces.push(self.rope.slice(s.range()).to_string());
                continue;
            }
            let line = self.rope.char_to_line(s.head);
            if last_line != Some(line) {
                pieces.push(self.line_text(line) + self.line_ending.as_str());
                last_line = Some(line);
            }
        }
        pieces
    }

    /// Cut at every caret: its selection, or its whole line.
    pub fn cut_carets(&mut self) {
        self.edit_carets(|b, sel| {
            let r = if sel.is_empty() { line_range(&b.rope, b.rope.char_to_line(sel.head)) } else { sel.range() };
            CaretEdit::replace(r, "", TxKind::Other)
        });
    }

    /// Delete every non-empty selection; false when there is none.
    pub fn delete_selection(&mut self) -> bool {
        if self.view.sels.iter().all(Selection::is_empty) {
            return false;
        }
        self.edit_carets(|_, sel| if sel.is_empty() { CaretEdit::keep(sel) } else { CaretEdit::replace(sel.range(), "", TxKind::Other) });
        true
    }

    pub fn backspace(&mut self) {
        self.backspace_pairs(&[]);
    }

    /// Backspace at every caret: a selection goes, else the char before the
    /// caret, or both halves of an auto-close pair around it such as `(|)`.
    pub fn backspace_pairs(&mut self, pairs: &[(char, char)]) {
        self.edit_carets(|b, sel| b.backspace_edit(sel, pairs));
    }

    fn backspace_edit(&self, sel: Selection, pairs: &[(char, char)]) -> CaretEdit {
        if !sel.is_empty() {
            return CaretEdit::replace(sel.range(), "", TxKind::Other);
        }
        let p = sel.head;
        if p == 0 {
            return CaretEdit::keep(sel);
        }
        if p < self.rope.len_chars() && pairs.contains(&(self.rope.char(p - 1), self.rope.char(p))) {
            return CaretEdit::replace(p - 1..p + 1, "", TxKind::Deleting);
        }
        let mut start = p - 1;
        if start > 0 && self.rope.char(start) == '\n' && self.rope.char(start - 1) == '\r' {
            start -= 1;
        }
        CaretEdit::replace(start..p, "", TxKind::Deleting)
    }

    /// Type `c` at every caret, with auto-close ([`Buffer::char_edit`];
    /// never inside a string or comment) and a closer on a blank line
    /// dedented to its opener's line ([`Buffer::closer_indent_at`]).
    pub fn insert_char(&mut self, c: char, pairs: &[(char, char)]) {
        if let Some(s) = &mut self.syntax {
            s.reparse(&self.rope);
        }
        self.edit_carets(|b, sel| {
            let tree = b.syntax.as_ref().and_then(|s| s.tree());
            let p = sel.head;
            let may_pair = !tree.is_some_and(|t| super::brackets::in_literal_at(&b.rope, t, p));
            // A closer that auto-close steps over keeps the line as is.
            let steps_over = pairs.iter().any(|(_, cl)| *cl == c) && p < b.rope.len_chars() && b.rope.char(p) == c;
            match b.closer_indent_at(sel, c, tree).filter(|_| !steps_over) {
                Some(indent) => b.dedent_edit(p, c, &indent),
                None => b.char_edit(sel, c, pairs, may_pair),
            }
        });
    }

    /// Type `c` at every caret with auto-close ([`Buffer::char_edit`]).
    #[cfg(test)]
    pub fn type_char(&mut self, c: char, pairs: &[(char, char)], may_pair: bool) {
        self.edit_carets(|b, sel| b.char_edit(sel, c, pairs, may_pair));
    }

    /// `c` typed at `sel` with auto-close: wrap a selection in an opener
    /// and its closer, step over a closer that is already at the caret, or
    /// insert the pair when `may_pair` and the caret is at a word boundary.
    /// An empty `pairs` table types `c` as is.
    fn char_edit(&self, sel: Selection, c: char, pairs: &[(char, char)], may_pair: bool) -> CaretEdit {
        let kind = if c.is_whitespace() { TxKind::Other } else { TxKind::Typing };
        let opener = pairs.iter().find(|(o, _)| *o == c).map(|(_, close)| *close);
        let is_closer = pairs.iter().any(|(_, close)| *close == c);
        let r = sel.range();
        if let (Some(close), false) = (opener, r.is_empty()) {
            let inner = self.rope.slice(r.clone()).to_string();
            let n = inner.chars().count();
            let (anchor, head) = if sel.anchor <= sel.head { (1, n + 1) } else { (n + 1, 1) };
            return CaretEdit { range: r, text: format!("{c}{inner}{close}"), after: After::At { anchor, head }, kind: TxKind::Other };
        }
        let p = sel.head;
        let next = (p < self.rope.len_chars()).then(|| self.rope.char(p));
        if r.is_empty() && is_closer && next == Some(c) {
            return CaretEdit::keep(sel).caret_at(1);
        }
        if let (Some(close), true, true) = (opener, r.is_empty(), may_pair) {
            let next_ok = next.is_none_or(|n| n.is_whitespace() || pairs.iter().any(|(_, cl)| *cl == n));
            let prev_word = p > 0 && selection::is_word_char(self.rope.char(p - 1));
            if next_ok && (close != c || !prev_word) {
                return CaretEdit::replace(p..p, format!("{c}{close}"), kind).caret_at(1);
            }
        }
        CaretEdit::replace(r, c.to_string(), kind)
    }

    /// Backspace between an auto-close pair such as `(|)` deletes both, at
    /// the primary caret. Returns false when the caret is not inside a pair.
    #[cfg(test)]
    pub fn backspace_pair(&mut self, pairs: &[(char, char)]) -> bool {
        let sel = self.view.sel();
        let p = sel.head;
        if !sel.is_empty() || p == 0 || p >= self.rope.len_chars() || !pairs.contains(&(self.rope.char(p - 1), self.rope.char(p))) {
            return false;
        }
        self.backspace_pairs(pairs);
        true
    }

    pub fn delete_forward(&mut self) {
        self.edit_carets(|b, sel| {
            if !sel.is_empty() {
                return CaretEdit::replace(sel.range(), "", TxKind::Other);
            }
            let p = sel.head;
            if p >= b.rope.len_chars() {
                return CaretEdit::keep(sel);
            }
            let mut end = p + 1;
            if b.rope.char(p) == '\r' && end < b.rope.len_chars() && b.rope.char(end) == '\n' {
                end += 1;
            }
            CaretEdit::replace(p..end, "", TxKind::Deleting)
        });
    }

    pub fn delete_word_back(&mut self) {
        self.edit_carets(|b, sel| {
            let p = sel.head;
            let r = if sel.is_empty() { selection::prev_word_boundary(&b.rope, p)..p } else { sel.range() };
            if r.is_empty() { CaretEdit::keep(sel) } else { CaretEdit::replace(r, "", TxKind::Other) }
        });
    }

    pub fn delete_word_forward(&mut self) {
        self.edit_carets(|b, sel| {
            let p = sel.head;
            let r = if sel.is_empty() { p..selection::next_word_boundary(&b.rope, p) } else { sel.range() };
            if r.is_empty() { CaretEdit::keep(sel) } else { CaretEdit::replace(r, "", TxKind::Other) }
        });
    }

    /// Newline at every caret, keeping the line's indentation, one level
    /// deeper after an opening bracket (or a `:` when `colon`). Between a
    /// bracket pair such as `{|}` the closer moves to its own line at the
    /// base indent. Trailing whitespace before the caret is dropped.
    pub fn newline_smart(&mut self, colon: bool) {
        self.edit_carets(|b, sel| b.newline_edit(sel, colon));
    }

    fn newline_edit(&self, sel: Selection, colon: bool) -> CaretEdit {
        let r = sel.range();
        let (line, col) = selection::pos_to_line_col(&self.rope, r.start);
        let ls = self.rope.line_to_char(line);
        let indent_n = selection::indent_of(&self.rope, line).min(col);
        let base: String = self.rope.line(line).chars().take(indent_n).collect();
        let before = self.rope.slice(ls..r.start).to_string();
        let kept = before.trim_end_matches([' ', '\t']);
        let start = ls + kept.chars().count();
        let last = kept.chars().last();
        let closer = match last {
            Some('{') => Some('}'),
            Some('[') => Some(']'),
            Some('(') => Some(')'),
            _ => None,
        };
        let deeper = closer.is_some() || (colon && last == Some(':'));
        let unit = self.indent.unit();
        let eol = self.line_ending.as_str();
        let indent = if deeper { format!("{base}{unit}") } else { base.clone() };
        let mut new = format!("{eol}{indent}");
        let caret = new.chars().count();
        if closer.is_some() && r.end < self.rope.len_chars() && Some(self.rope.char(r.end)) == closer {
            new.push_str(&format!("{eol}{base}"));
        }
        CaretEdit::replace(start..r.end, new, TxKind::Other).caret_at(caret)
    }

    /// Indent for closer `c` typed at the primary caret; see
    /// [`Buffer::closer_indent_at`].
    #[cfg(test)]
    pub fn closer_indent(&self, c: char, tree: Option<&tree_sitter::Tree>) -> Option<String> {
        self.closer_indent_at(self.view.sel(), c, tree)
    }

    /// Indent for closer `c` typed at caret `sel` on a line that is blank up
    /// to the caret: that of the line holding the matching opener. None
    /// when that does not apply or the line already has that indent.
    fn closer_indent_at(&self, sel: Selection, c: char, tree: Option<&tree_sitter::Tree>) -> Option<String> {
        if !matches!(c, '}' | ']' | ')') || !sel.is_empty() {
            return None;
        }
        let p = sel.head;
        let (line, col) = selection::pos_to_line_col(&self.rope, p);
        if selection::indent_of(&self.rope, line) < col {
            return None;
        }
        let open = super::brackets::opener_before(&self.rope, tree, p, c)?;
        let ol = self.rope.char_to_line(open);
        let indent: String = self.rope.line(ol).chars().take(selection::indent_of(&self.rope, ol)).collect();
        let current: String = self.rope.line(line).chars().take(col).collect();
        (indent != current).then_some(indent)
    }

    /// Closer `c` typed at `p` after replacing the line's leading
    /// whitespace with `indent` (from [`Buffer::closer_indent_at`]).
    fn dedent_edit(&self, p: usize, c: char, indent: &str) -> CaretEdit {
        let ls = self.rope.line_to_char(self.rope.char_to_line(p));
        CaretEdit::replace(ls..p, format!("{indent}{c}"), TxKind::Typing)
    }

    /// Type closer `c` at the primary caret after replacing its line's
    /// leading whitespace with `indent`, as one transaction.
    #[cfg(test)]
    pub fn type_dedented(&mut self, c: char, indent: &str) {
        let p = self.view.sel().head;
        self.view.set_sel(Selection::point(p));
        self.edit_carets(|b, sel| b.dedent_edit(sel.head, c, indent));
    }

    /// A tab at every caret with hard tabs, else spaces up to the next
    /// indent stop of the caret's column.
    pub fn insert_tab(&mut self) {
        let (hard, w) = (self.indent.hard_tabs, self.indent.width.max(1));
        self.edit_carets(|b, sel| {
            if hard {
                return CaretEdit::replace(sel.range(), "\t", TxKind::Typing);
            }
            let (_, col) = selection::pos_to_line_col(&b.rope, sel.head);
            CaretEdit::replace(sel.range(), " ".repeat(w - col % w), TxKind::Typing)
        });
    }

    /// Delete the lines of every selection (or caret line); each caret
    /// lands at its column on the line that took their place.
    pub fn delete_line(&mut self) {
        let sels: Vec<Selection> = self.view.sels.iter().copied().collect();
        let primary = self.view.sels.primary_index();
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for s in &sels {
            let r = s.range();
            let (l0, l1) = self.fold_block(self.rope.char_to_line(r.start), self.rope.char_to_line(r.end));
            match blocks.last_mut() {
                Some(b) if l0 <= b.1 + 1 => b.1 = b.1.max(l1),
                _ => blocks.push((l0, l1)),
            }
        }
        let ranges: Vec<Range<usize>> = blocks
            .iter()
            .map(|&(l0, l1)| {
                let mut range = line_range(&self.rope, l0).start..line_range(&self.rope, l1).end;
                if range.end == self.rope.len_chars() && range.start > 0 && self.rope.char(range.start - 1) == '\n' {
                    // Last line: also remove the preceding line break.
                    let mut s = range.start - 1;
                    if s > 0 && self.rope.char(s - 1) == '\r' {
                        s -= 1;
                    }
                    range.start = s;
                }
                range
            })
            .collect();
        let changes: Vec<Change> = ranges.iter().rev().map(|r| Change::delete(r.start, self.rope.slice(r.clone()).to_string())).collect();
        // Each caret: where its block was, after the edit, and its column.
        let targets: Vec<(usize, usize)> = sels
            .iter()
            .map(|s| {
                let line = self.rope.char_to_line(s.range().start);
                let k = blocks.iter().position(|b| b.0 <= line && line <= b.1).unwrap_or(0);
                (map_through(&changes, ranges[k].start, false), selection::pos_to_line_col(&self.rope, s.head).1)
            })
            .collect();
        self.apply_then(changes, TxKind::Other, |rope| {
            let carets = targets
                .iter()
                .map(|&(at, col)| Selection::point(selection::line_col_to_pos(rope, rope.char_to_line(at.min(rope.len_chars())), col)))
                .collect();
            Selections::new(carets, primary)
        });
    }

    /// Duplicate each selection after itself, or each caret line below it
    /// (once per line).
    pub fn duplicate_line(&mut self) {
        let eol = self.line_ending.as_str();
        let copies = self.duplicated_folds();
        let version = self.version;
        self.edit_carets(|b, sel| {
            let r = sel.range();
            if !r.is_empty() {
                let text = b.rope.slice(r.clone()).to_string();
                let n = text.chars().count();
                return CaretEdit { range: r.end..r.end, text, after: After::At { anchor: 0, head: n }, kind: TxKind::Other };
            }
            let (line, col) = selection::pos_to_line_col(&b.rope, sel.head);
            let (first, last) = b.fold_block(line, line);
            let lr = line_range(&b.rope, first).start..line_range(&b.rope, last).end;
            let mut text = b.rope.slice(lr.clone()).to_string();
            let mut caret = if line == first { col } else { 0 };
            if !text.ends_with('\n') {
                text = format!("{eol}{text}");
                caret += eol.chars().count();
            }
            CaretEdit::replace(lr.end..lr.end, text, TxKind::Other).caret_at(caret)
        });
        if self.version != version {
            self.view.folds.collapse_saved(&copies, &self.rope);
        }
    }

    /// The collapsed folds in the lines [`Buffer::duplicate_line`] copies,
    /// as they will sit in the copies (once per block).
    fn duplicated_folds(&self) -> Vec<SessionFold> {
        if self.view.folds.is_empty() {
            return Vec::new();
        }
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for s in self.view.sels.iter().filter(|s| s.is_empty()) {
            let line = self.rope.char_to_line(s.head);
            let b = self.fold_block(line, line);
            if !blocks.contains(&b) {
                blocks.push(b);
            }
        }
        blocks.sort_unstable();
        let saved = self.view.folds.to_session(&self.rope);
        let mut earlier = 0;
        let mut out = Vec::new();
        for (first, last) in blocks {
            let n = last - first + 1;
            for f in saved.iter().filter(|f| first <= f.start_line && f.start_line <= last) {
                out.push(shifted(f, (earlier + n) as isize));
            }
            earlier += n;
        }
        out
    }

    /// The first and last line of the block holding lines `l0..=l1`: a
    /// collapsed fold's header with its hidden lines count as one line.
    fn fold_block(&self, l0: usize, l1: usize) -> (usize, usize) {
        let folds = &self.view.folds;
        let first = super::rows::Rows::new(folds, self.rope.len_lines()).visible(l0);
        let last = folds.hidden().iter().find(|r| r.start <= l1 + 1 && l1 < r.end).map_or(l1, |r| r.end - 1);
        (first, last)
    }

    pub fn select_all(&mut self) {
        self.view.set_sel(Selection::new(0, self.rope.len_chars()));
    }

    pub fn select_line(&mut self) {
        let (line, _) = self.cursor_line_col();
        let (first, last) = self.fold_block(line, line);
        self.view.set_sel(Selection::new(line_range(&self.rope, first).start, line_range(&self.rope, last).end));
    }

    /// Select the smallest syntax node (or bracket contents) around each
    /// selection; word, line, all without a syntax tree.
    pub fn expand_selection(&mut self) {
        if let Some(s) = &mut self.syntax {
            s.reparse(&self.rope);
        }
        if self.view.expand_stack.last().is_some_and(|(_, after)| *after != self.view.sels) {
            self.view.expand_stack.clear();
        }
        let tree = self.syntax.as_ref().and_then(|s| s.tree());
        let grown: Vec<Option<std::ops::Range<usize>>> =
            self.view.sels.iter().map(|s| super::expand::expand_range(&self.rope, tree, s.range())).collect();
        if grown.iter().all(Option::is_none) {
            return;
        }
        let ranges = self.view.sels.iter().zip(grown).map(|(s, r)| r.map_or(*s, |r| Selection::new(r.start, r.end))).collect();
        let after = Selections::new(ranges, self.view.sels.primary_index());
        self.view.expand_stack.push((self.view.sels.clone(), after.clone()));
        self.view.sels = after;
        self.undo.break_group = true;
    }

    /// Undo the last expand step, if the selections are still what it made.
    pub fn shrink_selection(&mut self) {
        match self.view.expand_stack.pop() {
            Some((before, after)) if after == self.view.sels => {
                self.view.sels = before;
                self.view.sels.reset_desired();
                self.undo.break_group = true;
            }
            _ => self.view.expand_stack.clear(),
        }
    }

    /// Put the caret at char `pos` (clamped to the text).
    pub fn goto_pos(&mut self, pos: usize) {
        self.view.set_sel(Selection::point(pos.min(self.rope.len_chars())));
        self.undo.break_group = true;
    }

    pub fn goto_line(&mut self, line: usize, col: usize) {
        let line = line.min(self.rope.len_lines().saturating_sub(1));
        let p = selection::line_col_to_pos(&self.rope, line, col);
        self.view.set_sel(Selection::point(p));
        self.undo.break_group = true;
    }

    /// Comment or uncomment the selected lines (or the caret lines) of
    /// every caret with a line comment `token`, as one transaction: each
    /// run of lines at its own smallest indent. Uncomments only when every
    /// non-blank line is already commented.
    pub fn toggle_line_comment(&mut self, token: &str) {
        let blocks = self.line_blocks();
        // (line, indent, blank) for each covered line, by block.
        let lines: Vec<Vec<(usize, usize, bool)>> = blocks
            .iter()
            .map(|&(l0, l1)| {
                (l0..=l1)
                    .map(|l| {
                        let indent = selection::indent_of(&self.rope, l);
                        (l, indent, indent >= line_len_no_eol(&self.rope, l))
                    })
                    .collect()
            })
            .collect();
        // Comment blank lines too when there is nothing else to comment.
        let skip_blank = lines.iter().flatten().any(|(_, _, blank)| !blank);
        let targets: Vec<Vec<(usize, usize)>> =
            lines.iter().map(|b| b.iter().filter(|(_, _, blank)| !(skip_blank && *blank)).map(|(l, i, _)| (*l, *i)).collect()).collect();
        let commented = |l: usize, indent: usize| {
            let s = self.rope.line_to_char(l) + indent;
            let n = token.chars().count();
            s + n <= self.rope.len_chars() && self.rope.slice(s..s + n) == token
        };
        let uncomment = skip_blank && targets.iter().flatten().all(|&(l, i)| commented(l, i));
        let mut changes = Vec::new();
        for block in targets.iter().rev() {
            let min_indent = block.iter().map(|(_, i)| *i).min().unwrap_or(0);
            for &(l, indent) in block.iter().rev() {
                let ls = self.rope.line_to_char(l);
                if uncomment {
                    let at = ls + indent;
                    let mut old = token.to_string();
                    let after = at + token.chars().count();
                    if after < self.rope.len_chars() && self.rope.char(after) == ' ' {
                        old.push(' ');
                    }
                    changes.push(Change::delete(at, old));
                } else {
                    changes.push(Change::insert(ls + min_indent, format!("{token} ")));
                }
            }
        }
        let sel_after = self.sels_through(&changes);
        // JetBrains moves a lone caret to the next line after commenting.
        let next_line = !self.view.sels.is_multi() && self.view.sel().is_empty();
        // Past a folded block, not just its header.
        let below = blocks.last().map_or(0, |b| b.1) + 1;
        self.apply_then(changes, TxKind::Other, |rope| {
            let head = sel_after.primary().head;
            let (_, col) = selection::pos_to_line_col(rope, head);
            if next_line && below < rope.len_lines() {
                Selection::point(selection::line_col_to_pos(rope, below, col)).into()
            } else {
                sel_after
            }
        });
    }

    /// Wrap each selection, or each caret line from its indent to the end,
    /// in a block comment, or unwrap it when its trimmed text already is one.
    pub fn toggle_block_comment(&mut self, open: &str, close: &str) {
        self.edit_carets(|b, sel| {
            let r = if sel.is_empty() {
                let line = b.rope.char_to_line(sel.head);
                b.line_content(line, line)
            } else {
                sel.range()
            };
            b.block_comment_edit(sel, r, open, close)
        });
    }

    /// Block comment toggle on whole lines: the selected lines (or the caret
    /// line) of each caret from the first one's indent to the last one's
    /// end. Ctrl+/ uses it in languages without line comments.
    pub fn toggle_block_comment_lines(&mut self, open: &str, close: &str) {
        self.edit_carets(|b, sel| {
            let (l0, l1) = selected_lines(&b.rope, sel);
            b.block_comment_edit(sel, b.line_content(l0, l1), open, close)
        });
    }

    /// From the indent of `l0` to the end of `l1`, without the line break.
    fn line_content(&self, l0: usize, l1: usize) -> Range<usize> {
        self.rope.line_to_char(l0) + selection::indent_of(&self.rope, l0)..self.rope.line_to_char(l1) + line_len_no_eol(&self.rope, l1)
    }

    /// Toggle a block comment around the trimmed text of `r` for caret
    /// `sel`. Unwrapping also drops one space inside each delimiter. A
    /// selection keeps covering the text (and the added delimiters); a caret
    /// stays on the same character.
    fn block_comment_edit(&self, sel: Selection, r: Range<usize>, open: &str, close: &str) -> CaretEdit {
        let text = self.rope.slice(r.clone()).to_string();
        let trimmed = text.trim();
        let start = r.start + text.chars().take_while(|c| c.is_whitespace()).count();
        let end = start + trimmed.chars().count();
        let n = |s: &str| s.chars().count();
        let changes = if trimmed.len() >= open.len() + close.len() && trimmed.starts_with(open) && trimmed.ends_with(close) {
            let inner = &trimmed[open.len()..trimmed.len() - close.len()];
            let lead = inner.starts_with(' ');
            let trail = inner[lead as usize..].ends_with(' ');
            let close_old = if trail { format!(" {close}") } else { close.to_string() };
            let open_old = if lead { format!("{open} ") } else { open.to_string() };
            vec![Change::delete(end - n(&close_old), close_old), Change::delete(start, open_old)]
        } else if trimmed.is_empty() {
            vec![Change::insert(start, format!("{open}{close}"))]
        } else {
            vec![Change::insert(end, format!(" {close}")), Change::insert(start, format!("{open} "))]
        };
        let after = if sel.is_empty() {
            // On a blank line the caret lands between the delimiters.
            let p = if trimmed.is_empty() { start + n(open) } else { map_through(&changes, sel.head, false) };
            Selection::point(p)
        } else {
            sel_through(sel, &changes)
        };
        self.caret_edit_from(&changes, sel, after)
    }

    /// One caret's `changes` (back to front, positions in the text before
    /// them) and the selection they leave, as a [`CaretEdit`] over the text
    /// they and the caret's selection span.
    fn caret_edit_from(&self, changes: &[Change], sel: Selection, after: Selection) -> CaretEdit {
        let lo = changes.iter().map(|c| c.at).chain([sel.range().start, after.range().start]).min().unwrap_or(0);
        let hi = changes.iter().map(|c| c.at + c.old.chars().count()).chain([sel.range().end, lo]).max().unwrap_or(lo);
        let mut text: Vec<char> = self.rope.slice(lo..hi).chars().collect();
        for c in changes {
            let at = c.at - lo;
            text.splice(at..at + c.old.chars().count(), c.new.chars());
        }
        let after = After::At { anchor: after.anchor - lo, head: after.head - lo };
        CaretEdit { range: lo..hi, text: text.into_iter().collect(), after, kind: TxKind::Other }
    }

    /// The buffer's language: the one picked by hand, else the attached
    /// syntax's, else the one detected on open (files too large for syntax
    /// have none attached), else the one its path suggests.
    pub fn language(&self) -> Option<&'static super::syntax::LanguageDef> {
        match self.language_choice {
            LanguageChoice::Plain => None,
            LanguageChoice::Named(name) => super::syntax::language_for_name(name),
            LanguageChoice::Auto => self
                .syntax
                .as_ref()
                .map(|s| s.def)
                .or(self.detected_language)
                .or_else(|| self.path.as_deref().and_then(super::syntax::detect)),
        }
    }

    /// Override the language (`None` is plain text) and its highlighter.
    pub fn set_language(&mut self, def: Option<&'static super::syntax::LanguageDef>, syntax: Option<super::syntax::Syntax>) {
        self.language_choice = def.map_or(LanguageChoice::Plain, |d| LanguageChoice::Named(d.name));
        self.syntax = syntax;
        self.fold_cache = None;
        self.changed_since_render = true;
    }

    /// Convert every line break to `le` (lone `\r` is left alone) as one
    /// undoable transaction that also carries the old and new `line_ending`,
    /// so undo and redo switch it back and forth with the text. Text without
    /// a break to convert only changes the setting.
    pub fn convert_line_endings(&mut self, le: LineEnding) {
        if self.read_only {
            return;
        }
        let before = self.line_ending;
        let mut changes = Vec::new();
        let mut prev = '\0';
        for (i, ch) in self.rope.chars().enumerate() {
            if ch == '\n' {
                match (le, prev == '\r') {
                    (LineEnding::CrLf, false) => changes.push(Change::insert(i, "\r")),
                    (LineEnding::Lf, true) => changes.push(Change::delete(i - 1, "\r")),
                    _ => {}
                }
            }
            prev = ch;
        }
        // Back to front: each position is still valid after the later ones.
        changes.reverse();
        self.line_ending = le;
        if changes.is_empty() {
            return;
        }
        // A caret at a line end stays before the inserted `\r`.
        let mut sels = self.view.sels.clone();
        sels.map_changes(&changes, true);
        self.apply(changes, sels, TxKind::Other);
        if let Some(tx) = self.undo.last_mut() {
            tx.line_ending = Some((before, le));
        }
    }

    /// The selected lines of every caret ([`selected_lines`]) widened over
    /// collapsed folds ([`Buffer::fold_block`]), with runs that overlap or
    /// touch merged; sorted.
    fn line_blocks(&self) -> Vec<(usize, usize)> {
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for s in self.view.sels.iter() {
            let (l0, l1) = selected_lines(&self.rope, *s);
            let (l0, l1) = self.fold_block(l0, l1);
            match blocks.last_mut() {
                Some(b) if l0 <= b.1 + 1 => b.1 = b.1.max(l1),
                _ => blocks.push((l0, l1)),
            }
        }
        blocks
    }

    /// Every selection mapped through `changes` ([`sel_through`]).
    fn sels_through(&self, changes: &[Change]) -> Selections {
        let mapped = self.view.sels.iter().map(|s| sel_through(*s, changes)).collect();
        Selections::new(mapped, self.view.sels.primary_index())
    }

    /// Indent every non-blank selected line of every caret by one level.
    pub fn indent_lines(&mut self) {
        let pad = self.indent.unit();
        let changes: Vec<Change> = self
            .line_blocks()
            .into_iter()
            .rev()
            .flat_map(|(l0, l1)| (l0..=l1).rev())
            .filter(|&l| selection::indent_of(&self.rope, l) < line_len_no_eol(&self.rope, l))
            .map(|l| Change::insert(self.rope.line_to_char(l), pad.clone()))
            .collect();
        let sel_after = self.sels_through(&changes);
        self.apply(changes, sel_after, TxKind::Other);
    }

    /// Remove one level of indentation (a tab or up to the indent width in
    /// spaces) from the selected lines, or the caret lines.
    pub fn unindent_lines(&mut self) {
        let width = self.indent.width.max(1);
        let mut changes = Vec::new();
        for l in self.line_blocks().into_iter().rev().flat_map(|(l0, l1)| (l0..=l1).rev()) {
            let ls = self.rope.line_to_char(l);
            let lead: String = self.rope.line(l).chars().take_while(|c| *c == ' ' || *c == '\t').take(width).collect();
            let old: String = if lead.starts_with('\t') { "\t".into() } else { lead.chars().take_while(|c| *c == ' ').collect() };
            if !old.is_empty() {
                changes.push(Change::delete(ls, old));
            }
        }
        let sel_after = self.sels_through(&changes);
        self.apply(changes, sel_after, TxKind::Other);
    }

    /// Index of the last line, not counting the empty line after a final
    /// line break.
    fn last_line(&self) -> usize {
        let last = self.rope.len_lines() - 1;
        if last > 0 && line_len_no_eol(&self.rope, last) == 0 { last - 1 } else { last }
    }

    /// Join each caret line with the next one, or all lines of a multi-line
    /// selection: the line break, trailing whitespace before it and the
    /// next line's indentation become one space (none next to an empty
    /// line). A caret lands at its join point.
    pub fn join_lines(&mut self) {
        let last = self.last_line();
        let mut joins = BTreeSet::new();
        for s in self.view.sels.iter() {
            let (l0, l1) = selected_lines(&self.rope, *s);
            let l1 = if l1 == l0 { l0 + 1 } else { l1 };
            joins.extend(l0..l1.min(last));
        }
        if joins.is_empty() {
            return;
        }
        let join_at = |l: usize| self.rope.line_to_char(l) + self.line_text(l).trim_end().chars().count();
        let mut changes = Vec::new();
        for &l in joins.iter().rev() {
            let at = join_at(l);
            let kept = at - self.rope.line_to_char(l);
            let next = self.rope.line_to_char(l + 1);
            let next_indent = selection::indent_of(&self.rope, l + 1);
            let next_empty = next_indent >= line_len_no_eol(&self.rope, l + 1);
            let old = self.rope.slice(at..next + next_indent).to_string();
            let new = if kept == 0 || next_empty { "" } else { " " };
            changes.push(Change::replace(at, old, new));
        }
        let mapped = self
            .view
            .sels
            .iter()
            .map(|s| {
                let line = self.rope.char_to_line(s.head);
                if s.is_empty() && joins.contains(&line) {
                    Selection::point(map_through(&changes, join_at(line), false))
                } else {
                    sel_through(*s, &changes)
                }
            })
            .collect();
        let sel_after = Selections::new(mapped, self.view.sels.primary_index());
        self.apply(changes, sel_after, TxKind::Other);
    }

    /// Uppercase each selection (or the word at each caret) if it has any
    /// lowercase char, otherwise lowercase it.
    pub fn toggle_case(&mut self) {
        self.edit_carets(|b, sel| {
            let r = if sel.is_empty() { selection::word_at(&b.rope, sel.head) } else { sel.range() };
            let old = b.rope.slice(r.clone()).to_string();
            let new = if old.chars().any(char::is_lowercase) { old.to_uppercase() } else { old.to_lowercase() };
            if r.is_empty() || new == old {
                return CaretEdit::keep(sel);
            }
            // Case mapping can change the length (ß -> SS).
            let n = new.chars().count();
            let after = if sel.is_empty() {
                After::At { anchor: (sel.head - r.start).min(n), head: (sel.head - r.start).min(n) }
            } else {
                After::At { anchor: 0, head: n }
            };
            CaretEdit { range: r, text: new, after, kind: TxKind::Other }
        });
    }

    /// Swap the selected lines (or the caret line) of every caret with the
    /// line above or below; the selections move with them. No-op when any
    /// of them is at the document edge; the empty line after a final line
    /// break does not count as a line.
    pub fn move_lines(&mut self, up: bool) {
        let blocks = self.line_blocks();
        let last = self.last_line();
        if blocks.iter().any(|&(l0, l1)| (up && l0 == 0) || (!up && l1 >= last)) {
            return;
        }
        let eol = self.line_ending.as_str();
        let mut changes = Vec::with_capacity(blocks.len());
        let mut kept = Vec::new();
        // How far each block's text moves, including the length change of
        // the blocks before it.
        let mut moves = Vec::with_capacity(blocks.len());
        let mut delta = 0isize;
        for &(l0, l1) in &blocks {
            // The neighbour is a whole folded block too.
            let (first, end) = if up { (self.fold_block(l0 - 1, l0 - 1).0, l1) } else { (l0, self.fold_block(l1 + 1, l1 + 1).1) };
            let nb_lines = if up { l0 - first } else { end - l1 };
            let range = line_range(&self.rope, first).start..line_range(&self.rope, end).end;
            let old = self.rope.slice(range.clone()).to_string();
            let mut lines: Vec<String> = (first..=end).map(|l| self.line_text(l)).collect();
            let neighbour_len: usize =
                if up { &lines[..nb_lines] } else { &lines[lines.len() - nb_lines..] }.iter().map(|l| l.chars().count()).sum();
            if up {
                lines.rotate_left(nb_lines);
            } else {
                lines.rotate_right(nb_lines);
            }
            let mut new = lines.join(eol);
            if old.ends_with('\n') {
                new.push_str(eol);
            }
            if !self.view.folds.is_empty() && !self.read_only {
                // Folds in the span: the block's go by the neighbour's size,
                // the neighbour's by the block's.
                let own = l1 - l0 + 1;
                for f in self.view.folds.to_session(&self.rope).iter().filter(|f| first <= f.start_line && f.start_line <= end) {
                    let in_block = l0 <= f.start_line && f.start_line <= l1;
                    let by = match (in_block, up) {
                        (true, true) => -(nb_lines as isize),
                        (true, false) => nb_lines as isize,
                        (false, true) => own as isize,
                        (false, false) => -(own as isize),
                    };
                    kept.push(shifted(f, by));
                }
                let span = Fold { start: range.start, end: range.end };
                self.view.folds.expand_within(span, &self.rope);
            }
            let shift = (neighbour_len + nb_lines * eol.chars().count()) as isize;
            moves.push(delta + if up { -shift } else { shift });
            delta += new.chars().count() as isize - old.chars().count() as isize;
            changes.push(Change::replace(range.start, old, new));
        }
        changes.reverse();
        let mapped = self
            .view
            .sels
            .iter()
            .map(|s| {
                let l0 = selected_lines(&self.rope, *s).0;
                let m = blocks.iter().position(|b| b.0 <= l0 && l0 <= b.1).map_or(0, |k| moves[k]);
                let moved = |p: usize| (p as isize + m) as usize;
                Selection::new(moved(s.anchor), moved(s.head))
            })
            .collect();
        let sel_after = Selections::new(mapped, self.view.sels.primary_index());
        self.apply(changes, sel_after, TxKind::Other);
        self.view.folds.collapse_saved(&kept, &self.rope);
    }

    /// Add or remove the bookmark on the caret line.
    pub fn toggle_bookmark(&mut self) {
        let line = self.cursor_line_col().0;
        if self.bookmarks.remove(&line).is_none() {
            self.bookmarks.insert(line, None);
        }
    }

    /// Replace the bookmarks (from a session), dropping lines past the end.
    pub fn set_bookmarks(&mut self, lines: &[usize]) {
        let n = self.rope.len_lines();
        self.bookmarks = lines.iter().copied().filter(|l| *l < n).map(|l| (l, None)).collect();
    }

    /// The line of the bookmark with mnemonic `digit`.
    pub fn digit_line(&self, digit: u8) -> Option<usize> {
        self.bookmarks.iter().find(|(_, d)| **d == Some(digit)).map(|(l, _)| *l)
    }

    /// The numbered bookmarks as (digit, line), by line.
    pub fn digits(&self) -> Vec<(u8, usize)> {
        self.bookmarks.iter().filter_map(|(l, d)| d.map(|d| (d, *l))).collect()
    }

    pub fn line_text(&self, line: usize) -> String {
        let n = line_len_no_eol(&self.rope, line);
        let s = self.rope.line_to_char(line);
        self.rope.slice(s..s + n).to_string()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn mark_saved(&mut self) {
        self.mark_saved_at(self.undo.state_id());
    }

    /// The file now holds the text of undo state `state` (the current one,
    /// or an earlier one a formatted save wrote).
    pub fn mark_saved_at(&mut self, state: u64) {
        self.saved_state = state;
        self.undo.break_group = true;
    }

    /// Clean up before writing: strip trailing spaces and tabs (`trim`) and
    /// end a non-empty buffer with a line break (`final_newline`), as one
    /// undoable transaction. The caret lines keep their whitespace, as in
    /// JetBrains, and Markdown is never trimmed (two trailing spaces are a
    /// hard break). Nothing to fix means no transaction, so `dirty()` and
    /// undo stay as they were.
    pub fn prepare_for_save(&mut self, trim: bool, final_newline: bool) {
        let len = self.rope.len_chars();
        let mut changes = Vec::new();
        if final_newline && len > 0 && !matches!(self.rope.char(len - 1), '\n' | '\r') {
            changes.push(Change::insert(len, self.line_ending.as_str()));
        }
        if trim && !self.is_markdown() {
            let caret_lines: BTreeSet<usize> = self.view.sels.heads().map(|h| self.rope.char_to_line(h.min(len))).collect();
            for l in (0..self.rope.len_lines()).rev().filter(|l| !caret_lines.contains(l)) {
                let text = self.line_text(l);
                let kept = text.trim_end_matches([' ', '\t']);
                if kept.len() < text.len() {
                    let at = self.rope.line_to_char(l) + kept.chars().count();
                    changes.push(Change::delete(at, &text[kept.len()..]));
                }
            }
        }
        if changes.is_empty() {
            return;
        }
        // Positions at an insertion stay before it: a caret at the end of
        // the file does not jump onto the added empty line.
        let mut sels = self.view.sels.clone();
        sels.map_changes(&changes, true);
        self.apply(changes, sels, TxKind::Other);
    }

    fn is_markdown(&self) -> bool {
        let ext = self.path.as_deref().and_then(|p| p.extension()).and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        self.language().is_some_and(|d| d.name == "markdown") || matches!(ext.as_deref(), Some("md" | "markdown"))
    }
}

/// First and last line covered by `sel`. A selection ending at column 0
/// does not include that line.
fn selected_lines(rope: &Rope, sel: Selection) -> (usize, usize) {
    let r = sel.range();
    let (l0, _) = selection::pos_to_line_col(rope, r.start);
    let (mut l1, c1) = selection::pos_to_line_col(rope, r.end);
    if l1 > l0 && c1 == 0 {
        l1 -= 1;
    }
    (l0, l1)
}

/// `f` moved `by` lines.
fn shifted(f: &SessionFold, by: isize) -> SessionFold {
    let line = |l: usize| (l as isize + by) as usize;
    SessionFold { start_line: line(f.start_line), end_line: line(f.end_line), ..*f }
}

/// `sel` mapped through `changes` (applied in order, each relative to the
/// text after the previous one). A non-empty selection starting where text
/// is inserted grows to include it.
fn sel_through(sel: Selection, changes: &[Change]) -> Selection {
    let r = sel.range();
    let (start, end) = (map_through(changes, r.start, !r.is_empty()), map_through(changes, r.end, false));
    sel.with_range(start..end)
}

pub fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Where a bookmark goes when a change is applied: the line holding a
/// position of the edited text, a line number, or nowhere.
enum MarkTarget {
    Pos(usize),
    Line(usize),
    Drop,
}

/// Follow bookmarked `line` through change `c` (at `at`, removing `old_len`
/// chars of `rope`), by its line start. A line whose start is inside the
/// replaced text and that a pure deletion removes with one of its line
/// breaks is dropped; one that a deletion only merges into the line above
/// lands there. Inside a replacement the line is looked up by its text among
/// the new lines (so moved lines and reloads keep their marks), else keeps
/// its offset clamped to the new text.
fn mark_target(rope: &Rope, line: usize, at: usize, old_len: usize, c: &Change) -> MarkTarget {
    if line >= rope.len_lines() {
        return MarkTarget::Drop;
    }
    let s = rope.line_to_char(line);
    let del_end = at + old_len;
    let new_len = c.new.chars().count();
    if s < at {
        return MarkTarget::Pos(s);
    }
    if old_len == 0 || s >= del_end {
        // Text inserted at the line start pushes the line down.
        return MarkTarget::Pos(s - old_len + new_len);
    }
    let e = s + line_len_no_eol(rope, line);
    let has_eol = line + 1 < rope.len_lines();
    if new_len == 0 {
        let own_break_gone = s == at && has_eol && rope.line_to_char(line + 1) <= del_end;
        let prev_break_gone = s > at && e <= del_end;
        return if own_break_gone || prev_break_gone { MarkTarget::Drop } else { MarkTarget::Pos(at) };
    }
    let l0 = rope.char_to_line(at);
    let k = line - l0;
    let old_lines: Vec<&str> = c.old.split('\n').collect();
    let new_lines: Vec<&str> = c.new.split('\n').collect();
    let Some(text) = old_lines.get(k) else { return MarkTarget::Line(l0) };
    // The n-th line with this text among the old lines is the n-th one among
    // the new lines.
    let nth = old_lines[..k].iter().filter(|l| *l == text).count();
    let found = new_lines.iter().enumerate().filter(|(_, l)| *l == text).nth(nth).map(|(i, _)| i);
    MarkTarget::Line(l0 + found.unwrap_or(k.min(new_lines.len() - 1)))
}

/// `old` at `at` becoming `new` as the smallest changes, the later one
/// first: the common start and end stay, and text added around (or removed
/// from around) a kept middle becomes two inserts (or deletes), so a
/// wrapped selection reaches undo, parked views and the language server as
/// the two delimiters.
fn minimal_changes(at: usize, old: &str, new: &str) -> Vec<Change> {
    let (o, n): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let pre = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    let room = o.len().min(n.len()) - pre;
    let suf = o.iter().rev().zip(n.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    let (om, nm) = (&o[pre..o.len() - suf], &n[pre..n.len() - suf]);
    let at = at + pre;
    let s = |c: &[char]| c.iter().collect::<String>();
    let around = |outer: &[char], inner: &[char]| {
        (!inner.is_empty() && outer.len() > inner.len()).then(|| outer.windows(inner.len()).position(|w| w == inner)).flatten()
    };
    let parts = if let Some(k) = around(nm, om) {
        vec![Change::insert(at + om.len(), s(&nm[k + om.len()..])), Change::insert(at, s(&nm[..k]))]
    } else if let Some(k) = around(om, nm) {
        vec![Change::delete(at + k + nm.len(), s(&om[k + nm.len()..])), Change::delete(at, s(&om[..k]))]
    } else {
        vec![Change::replace(at, s(om), s(nm))]
    };
    parts.into_iter().filter(|c| c.old != c.new).collect()
}

/// Map position `p` through `changes`. With `stay_at_insert`, a position
/// exactly where text is inserted stays before the insertion.
pub fn map_through(changes: &[Change], mut p: usize, stay_at_insert: bool) -> usize {
    for c in changes {
        p = map_one(p, c.at, c.old.chars().count(), c.new.chars().count(), stay_at_insert);
    }
    p
}

/// Map position `p` through the replacement of `old_n` chars at `at` by
/// `new_n` chars, as [`map_through`] does: a position inside the replaced
/// text moves to its start.
pub fn map_one(p: usize, at: usize, old_n: usize, new_n: usize, stay_at_insert: bool) -> usize {
    if p < at || (p == at && stay_at_insert && old_n == 0) {
        return p;
    }
    if p >= at + old_n { p - old_n + new_n } else { at }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(text: &str) -> Buffer {
        Buffer::from_text(1, None, text, false)
    }

    #[test]
    fn a_read_only_buffer_moves_carets_but_keeps_its_text() {
        let mut buf = b("one\ntwo\n");
        buf.read_only = true;
        buf.view.set_sel(Selection::point(1));
        buf.insert("x", TxKind::Typing);
        buf.delete_forward();
        buf.delete_line();
        buf.convert_line_endings(LineEnding::CrLf);
        assert_eq!((buf.text().as_str(), buf.version, buf.dirty()), ("one\ntwo\n", 0, false));
        assert!(!buf.undo() && !buf.redo());
        assert!(buf.edit_log.since(0).is_some_and(|mut e| e.next().is_none()), "nothing for the parked views");
        buf.select_all();
        assert_eq!(buf.view.sel().range(), 0..8);
    }

    #[test]
    fn typing_groups_and_undo_restores() {
        let mut buf = b("");
        for c in "hello".chars() {
            buf.insert(&c.to_string(), TxKind::Typing);
        }
        assert_eq!(buf.text(), "hello");
        assert_eq!(buf.undo.len(), 1);
        assert!(buf.undo());
        assert_eq!(buf.text(), "");
        assert!(buf.redo());
        assert_eq!(buf.text(), "hello");
        assert_eq!(buf.view.sel().head, 5);
    }

    #[test]
    fn undo_redo_roundtrip_random() {
        let mut buf = b("seed text\nsecond line\n");
        // Snapshots of the text after every step that produced a transaction.
        let mut snapshots = vec![buf.text()];
        let mut x: u64 = 12345;
        for i in 0..100 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = buf.len_chars();
            let pos = (x >> 33) as usize % (len + 1);
            buf.view.set_sel(Selection::point(pos));
            buf.undo.break_group = true;
            let n = buf.undo.len();
            match i % 4 {
                0 => buf.insert(&format!("w{i} "), TxKind::Other),
                1 => buf.backspace(),
                2 => {
                    buf.view.set_sel(Selection::new(pos, (pos + 3).min(len)));
                    buf.delete_selection();
                }
                _ => buf.newline_smart(false),
            }
            if buf.undo.len() > n {
                snapshots.push(buf.text());
            }
        }
        let steps = snapshots.len() - 1;
        assert!(steps > 50);
        for i in (0..steps).rev() {
            assert!(buf.undo());
            assert_eq!(buf.text(), snapshots[i], "undo step {i}");
        }
        assert!(!buf.undo());
        for (i, snap) in snapshots.iter().enumerate().skip(1) {
            assert!(buf.redo());
            assert_eq!(buf.text(), *snap, "redo step {i}");
        }
        assert!(!buf.redo());
    }

    #[test]
    fn undo_back_to_saved_text_is_clean() {
        let mut buf = b("abc");
        assert!(!buf.dirty());
        buf.view.set_sel(Selection::point(3));
        buf.insert("d", TxKind::Typing);
        assert!(buf.dirty());
        assert!(buf.undo());
        assert!(!buf.dirty(), "undo to the loaded text");
        assert!(buf.redo());
        assert!(buf.dirty());
        buf.mark_saved();
        assert!(!buf.dirty());
        let v = buf.version;
        // Typing right after a save starts a new transaction.
        buf.insert("e", TxKind::Typing);
        buf.insert("f", TxKind::Typing);
        assert!(buf.dirty());
        assert!(buf.undo());
        assert_eq!(buf.text(), "abcd");
        assert!(!buf.dirty(), "undo to the saved text");
        assert!(buf.version > v, "version still moves for caches");
        assert!(buf.undo());
        assert!(buf.dirty(), "undo past the save");
        assert!(buf.redo());
        assert!(!buf.dirty());
        // A new edit after undoing past the save makes it unreachable.
        assert!(buf.undo());
        buf.insert("x", TxKind::Other);
        assert!(buf.dirty());
        assert!(buf.undo());
        assert!(buf.dirty());
    }

    /// Carets at `ps`, the last one primary.
    fn carets(buf: &mut Buffer, ps: &[usize]) {
        buf.view.sels = Selections::new(ps.iter().map(|&p| Selection::point(p)).collect(), ps.len() - 1);
    }

    fn heads(buf: &Buffer) -> Vec<usize> {
        buf.view.sels.heads().collect()
    }

    #[test]
    fn typing_at_three_carets_is_one_undo_step() {
        let mut buf = b("a
b
c
");
        carets(&mut buf, &[0, 2, 4]);
        buf.insert("x", TxKind::Typing);
        buf.insert("y", TxKind::Typing);
        assert_eq!(
            buf.text(),
            "xya
xyb
xyc
"
        );
        assert_eq!(heads(&buf), [2, 6, 10]);
        assert_eq!(buf.undo.len(), 1);
        assert!(buf.undo());
        assert_eq!(
            buf.text(),
            "a
b
c
"
        );
        assert_eq!(heads(&buf), [0, 2, 4], "undo puts every caret back");
        assert_eq!(buf.view.sels.primary_index(), 2);
        assert!(buf.redo());
        assert_eq!(
            (buf.text().as_str(), heads(&buf)),
            (
                "xya
xyb
xyc
",
                vec![2, 6, 10]
            )
        );
    }

    #[test]
    fn backspace_at_adjacent_carets_merges_them() {
        let mut buf = b("abc");
        carets(&mut buf, &[1, 2]);
        buf.backspace();
        assert_eq!(buf.text(), "c");
        assert_eq!(heads(&buf), [0], "both carets land at 0 and merge");
        assert!(buf.undo());
        assert_eq!((buf.text().as_str(), heads(&buf)), ("abc", vec![1, 2]));
        // A selection goes at one caret, a char at the other.
        let mut buf = b("one two");
        buf.view.sels = Selections::new(vec![Selection::new(0, 3), Selection::point(7)], 1);
        buf.backspace();
        assert_eq!(buf.text(), " tw");
        assert_eq!(heads(&buf), [0, 3]);
    }

    #[test]
    fn pairs_step_over_and_newline_at_two_carets() {
        let mut buf = b("f
g
");
        carets(&mut buf, &[1, 3]);
        buf.type_char('(', PAIRS, true);
        assert_eq!(
            buf.text(),
            "f()
g()
"
        );
        assert_eq!(heads(&buf), [2, 6]);
        buf.type_char(')', PAIRS, true);
        assert_eq!(
            buf.text(),
            "f()
g()
",
            "both closers stepped over"
        );
        assert_eq!(heads(&buf), [3, 7]);
        let mut buf = b("  a {}
  b {}
");
        carets(&mut buf, &[5, 12]);
        buf.newline_smart(false);
        assert_eq!(
            buf.text(),
            "  a {
      
  }
  b {
      
  }
"
        );
        assert_eq!(buf.view.sels.iter().map(|s| buf.rope.char_to_line(s.head)).collect::<Vec<_>>(), [1, 4]);
        assert_eq!(buf.undo.len(), 1);
        // Wrapping two selections keeps both selected.
        let mut buf = b("ab cd");
        buf.view.sels = Selections::new(vec![Selection::new(0, 2), Selection::new(5, 3)], 0);
        buf.type_char('[', PAIRS, true);
        assert_eq!(buf.text(), "[ab] [cd]");
        assert_eq!(buf.view.sels.as_slice(), [Selection::new(1, 3), Selection::new(8, 6)]);
    }

    #[test]
    fn overlapping_caret_edits_coalesce() {
        // Delete forward at 1 and backspace-like reach at 2 hit the same char.
        let mut buf = b("abcd");
        carets(&mut buf, &[1, 2]);
        buf.delete_word_back();
        assert_eq!(buf.text(), "cd", "both reach back to the word start");
        assert_eq!(heads(&buf), [0]);
        assert!(buf.undo());
        assert_eq!(buf.text(), "abcd");
        // Words forward from two carets in one word.
        let mut buf = b("hello world");
        carets(&mut buf, &[1, 3]);
        buf.delete_word_forward();
        assert_eq!(buf.text(), "hworld");
        assert_eq!(heads(&buf), [1]);
    }

    #[test]
    fn line_ops_act_on_the_union_of_caret_lines() {
        let mut buf = b("a\nb\nc\nd\n");
        carets(&mut buf, &[0, 2, 6]);
        buf.indent_lines();
        assert_eq!(buf.text(), "    a\n    b\nc\n    d\n");
        assert_eq!(heads(&buf), [4, 10, 18], "carets stay on their text");
        assert_eq!(buf.undo.len(), 1);
        buf.unindent_lines();
        assert_eq!(buf.text(), "a\nb\nc\nd\n");
        // One comment decision for all lines; a lone caret would move down,
        // several stay.
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), "// a\n// b\nc\n// d\n");
        assert_eq!(heads(&buf), [3, 8, 15]);
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), "a\nb\nc\nd\n");
        // A caret on an uncommented line comments everything.
        let mut buf = b("// a\nb\n");
        carets(&mut buf, &[0, 5]);
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), "// // a\n// b\n");
    }

    #[test]
    fn move_delete_join_with_several_blocks() {
        let mut buf = b("1\n2\n3\n4\n5\n");
        carets(&mut buf, &[2, 8]);
        buf.move_lines(true);
        assert_eq!(buf.text(), "2\n1\n3\n5\n4\n");
        assert_eq!(heads(&buf), [0, 6]);
        buf.move_lines(true);
        assert_eq!(buf.text(), "2\n1\n3\n5\n4\n", "a block at the top stops them all");
        buf.move_lines(false);
        assert_eq!(buf.text(), "1\n2\n3\n4\n5\n");
        assert_eq!(heads(&buf), [2, 8]);
        // Adjacent caret lines move as one block.
        carets(&mut buf, &[2, 4]);
        buf.move_lines(false);
        assert_eq!(buf.text(), "1\n4\n2\n3\n5\n");
        assert_eq!(heads(&buf), [4, 6]);
        let mut buf = b("ab\ncd\nef\ngh\n");
        carets(&mut buf, &[1, 7]);
        buf.delete_line();
        assert_eq!(buf.text(), "cd\ngh\n");
        assert_eq!(heads(&buf), [1, 4], "each at its column on the next line");
        assert!(buf.undo());
        assert_eq!(heads(&buf), [1, 7]);
        let mut buf = b("a\n  b\nc\n  d\n");
        carets(&mut buf, &[0, 7]);
        buf.join_lines();
        assert_eq!(buf.text(), "a b\nc d\n");
        assert_eq!(heads(&buf), [1, 5], "each caret at its join point");
    }

    #[test]
    fn per_caret_case_duplicate_and_block_comment() {
        let mut buf = b("one two\nthree");
        carets(&mut buf, &[1, 9]);
        buf.toggle_case();
        assert_eq!(buf.text(), "ONE two\nTHREE");
        assert_eq!(heads(&buf), [1, 9]);
        // Two carets on one line duplicate it once (and become one).
        let mut buf = b("ab\ncd\n");
        carets(&mut buf, &[0, 1, 4]);
        buf.duplicate_line();
        assert_eq!(buf.text(), "ab\nab\ncd\ncd\n");
        assert_eq!(heads(&buf), [3, 10]);
        let mut buf = b("x = 1;\n  y;\n");
        carets(&mut buf, &[2, 10]);
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "/* x = 1; */\n  /* y; */\n");
        assert_eq!(heads(&buf), [5, 19]);
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "x = 1;\n  y;\n");
    }

    #[test]
    fn whole_text_edits_keep_every_caret() {
        let mut buf = b("a  \nb  \nc  \n");
        carets(&mut buf, &[3, 11]);
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "a  \nb\nc  \n", "every caret line keeps its whitespace");
        assert_eq!(heads(&buf), [3, 9]);
        buf.convert_line_endings(LineEnding::CrLf);
        assert_eq!(heads(&buf), [3, 11]);
        assert_eq!(buf.view.sels.len(), 2);
    }

    #[test]
    fn delete_line_and_duplicate() {
        let mut buf = b("a\nb\nc");
        buf.goto_line(1, 0);
        buf.delete_line();
        assert_eq!(buf.text(), "a\nc");
        buf.goto_line(1, 0);
        buf.delete_line();
        assert_eq!(buf.text(), "a");
        buf.duplicate_line();
        assert_eq!(buf.text(), "a\na");
        assert_eq!(buf.cursor_line_col(), (1, 0));
    }

    #[test]
    fn redo_restores_caret_of_line_ops() {
        type Op = fn(&mut Buffer);
        let ops: [(&str, Op); 5] = [
            ("delete_line", |b| b.delete_line()),
            ("duplicate_line", |b| b.duplicate_line()),
            ("duplicate_last_line", |b| {
                b.goto_line(2, 1);
                b.duplicate_line();
            }),
            ("toggle_comment", |b| b.toggle_line_comment("//")),
            ("toggle_block_comment", |b| b.toggle_block_comment("/*", "*/")),
        ];
        for (name, op) in ops {
            let mut buf = b("alpha\n  beta\ngamma");
            buf.goto_line(1, 3);
            op(&mut buf);
            let (text, sel) = (buf.text(), buf.view.sel());
            assert!(buf.undo(), "{name}");
            assert!(buf.redo(), "{name}");
            assert_eq!(buf.text(), text, "{name}");
            assert_eq!(buf.view.sel(), sel, "{name}");
        }
    }

    const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];

    #[test]
    fn auto_close_wraps_selection() {
        let mut buf = b("let x = a + b;");
        buf.view.set_sel(Selection::new(13, 8));
        buf.type_char('(', PAIRS, true);
        assert_eq!(buf.text(), "let x = (a + b);");
        assert_eq!(buf.view.sel(), Selection::new(14, 9), "selection stays on the wrapped text");
        assert!(buf.undo());
        assert_eq!(buf.text(), "let x = a + b;");
    }

    #[test]
    fn auto_close_pairs_and_steps_over() {
        let mut buf = b("f\n");
        buf.view.set_sel(Selection::point(1));
        buf.type_char('(', PAIRS, true);
        assert_eq!(buf.text(), "f()\n");
        assert_eq!(buf.view.sel().head, 2);
        buf.type_char('x', PAIRS, true);
        buf.type_char(')', PAIRS, true);
        assert_eq!(buf.text(), "f(x)\n", "closer at the caret is stepped over");
        assert_eq!(buf.view.sel().head, 4);
        // Before a word char the opener is typed alone.
        buf.view.set_sel(Selection::point(0));
        buf.type_char('[', PAIRS, true);
        assert_eq!(buf.text(), "[f(x)\n");
        // Not when the caller says the caret is inside a string or comment.
        let mut buf = b("");
        buf.type_char('{', PAIRS, false);
        assert_eq!(buf.text(), "{");
    }

    #[test]
    fn auto_close_quotes_need_a_word_boundary() {
        let mut buf = b("");
        buf.type_char('"', PAIRS, true);
        assert_eq!(buf.text(), "\"\"");
        buf.type_char('a', PAIRS, true);
        buf.type_char('"', PAIRS, true);
        assert_eq!(buf.text(), "\"a\"");
        assert_eq!(buf.view.sel().head, 3);
        let mut buf = b("don");
        buf.view.set_sel(Selection::point(3));
        buf.type_char('\'', PAIRS, true);
        assert_eq!(buf.text(), "don'", "apostrophe after a word char");
        // Rust has no '' pair, so lifetimes type plainly.
        let rust = crate::editor::brackets::pairs(Some("rust"));
        let mut buf = b("&");
        buf.view.set_sel(Selection::point(1));
        buf.type_char('\'', rust, true);
        assert_eq!(buf.text(), "&'");
    }

    #[test]
    fn backspace_deletes_pair_and_disabled_table_types_plainly() {
        let mut buf = b("");
        buf.type_char('(', PAIRS, true);
        assert!(buf.backspace_pair(PAIRS));
        assert_eq!(buf.text(), "");
        let mut buf = b("(x)");
        carets(&mut buf, &[1, 2]);
        buf.backspace_pairs(PAIRS);
        assert_eq!(buf.text(), ")", "no pair around either caret: one char each");
        let mut buf = b("()");
        buf.view.set_sel(Selection::point(1));
        assert!(!buf.backspace_pair(&[]));
        buf.type_char(')', &[], true);
        assert_eq!(buf.text(), "())", "auto_close = false: no step-over");
        buf.view.set_sel(Selection::point(0));
        buf.type_char('(', &[], true);
        assert_eq!(buf.text(), "(())", "auto_close = false: no pair");
    }

    #[test]
    fn indent_and_unindent_selection() {
        let original = "fn a() {\n    x;\n\n\ty;\n}\n";
        let mut buf = b(original);
        // Lines 1-3; the selection ends at column 0 of line 4.
        buf.view.set_sel(Selection::new(buf.rope.line_to_char(1), buf.rope.line_to_char(4)));
        buf.indent_lines();
        assert_eq!(buf.text(), "fn a() {\n        x;\n\n    \ty;\n}\n", "blank line untouched");
        assert_eq!(buf.view.sel().range(), buf.rope.line_to_char(1)..buf.rope.line_to_char(4), "selection covers the same lines");
        assert_eq!(buf.undo.len(), 1);
        buf.unindent_lines();
        assert_eq!(buf.text(), original);
        buf.unindent_lines();
        assert_eq!(buf.text(), "fn a() {\nx;\n\ny;\n}\n", "a leading tab is one level");
        assert_eq!(buf.undo.len(), 3);
        assert!(buf.undo());
        assert_eq!(buf.text(), original);
    }

    #[test]
    fn unindent_caret_line_keeps_caret_on_text() {
        let mut buf = b("      abc\n");
        buf.goto_line(0, 7);
        buf.unindent_lines();
        assert_eq!(buf.text(), "  abc\n");
        assert_eq!(buf.cursor_line_col(), (0, 3));
        buf.unindent_lines();
        assert_eq!(buf.text(), "abc\n", "fewer spaces than a level");
        assert_eq!(buf.cursor_line_col(), (0, 1));
    }

    #[test]
    fn hard_tabs_insert_indent_and_unindent() {
        let mut buf = b("ab\n");
        buf.indent = IndentStyle::tabs(4);
        buf.goto_line(0, 1);
        buf.insert_tab();
        assert_eq!(buf.text(), "a\tb\n", "a tab whatever the column");
        let mut buf = b("x\n  y\n");
        buf.indent = IndentStyle::tabs(4);
        buf.view.set_sel(Selection::new(0, buf.rope.line_to_char(2)));
        buf.indent_lines();
        assert_eq!(buf.text(), "\tx\n\t  y\n");
        buf.unindent_lines();
        assert_eq!(buf.text(), "x\n  y\n", "one tab per line");
        buf.unindent_lines();
        assert_eq!(buf.text(), "x\ny\n", "spaces up to the width");
        let mut buf = b("\t\tz\n");
        buf.indent = IndentStyle::tabs(8);
        buf.unindent_lines();
        assert_eq!(buf.text(), "\tz\n");
        // Spaces: up to the next stop of the buffer's width.
        let mut buf = b("abc\n");
        buf.indent = IndentStyle::spaces(2);
        buf.goto_line(0, 3);
        buf.insert_tab();
        assert_eq!(buf.text(), "abc \n");
        buf.insert_tab();
        assert_eq!(buf.text(), "abc   \n");
    }

    #[test]
    fn move_lines_up_and_down() {
        let mut buf = b("a\nbb\nccc\n");
        buf.goto_line(1, 1);
        buf.move_lines(true);
        assert_eq!(buf.text(), "bb\na\nccc\n");
        assert_eq!(buf.cursor_line_col(), (0, 1));
        buf.move_lines(true);
        assert_eq!(buf.text(), "bb\na\nccc\n", "first line up is a no-op");
        buf.move_lines(false);
        buf.move_lines(false);
        assert_eq!(buf.text(), "a\nccc\nbb\n");
        assert_eq!(buf.cursor_line_col(), (2, 1));
        buf.move_lines(false);
        assert_eq!(buf.text(), "a\nccc\nbb\n", "the empty line after the final break is not a line");
        assert_eq!(buf.undo.len(), 3, "one undo step per move");
    }

    #[test]
    fn move_last_line_without_break_and_back() {
        let original = "one\ntwo\nthree";
        let mut buf = b(original);
        buf.goto_line(2, 5);
        buf.move_lines(true);
        assert_eq!(buf.text(), "one\nthree\ntwo");
        assert_eq!(buf.cursor_line_col(), (1, 5));
        buf.move_lines(false);
        assert_eq!(buf.text(), original);
        assert_eq!(buf.cursor_line_col(), (2, 5));
    }

    #[test]
    fn move_selection_keeps_its_range() {
        let mut buf = b("x\na\nb\ny\n");
        // Lines 1-2, ending at column 0 of line 3.
        buf.view.set_sel(Selection::new(buf.rope.line_to_char(3), buf.rope.line_to_char(1)));
        buf.move_lines(false);
        assert_eq!(buf.text(), "x\ny\na\nb\n");
        assert_eq!(buf.view.sel(), Selection::new(buf.rope.line_to_char(4), buf.rope.line_to_char(2)));
        buf.move_lines(true);
        buf.move_lines(true);
        assert_eq!(buf.text(), "a\nb\nx\ny\n");
        assert_eq!(buf.view.sel().range(), 0..4);
    }

    #[test]
    fn join_lines_caret_and_selection() {
        let mut buf = b("let a = foo(  \n    bar);\n\n  x\n");
        buf.goto_line(0, 3);
        buf.join_lines();
        assert_eq!(buf.text(), "let a = foo( bar);\n\n  x\n");
        assert_eq!(buf.view.sel().head, 12, "caret at the join point");
        // Next to an empty line no space is added.
        buf.join_lines();
        assert_eq!(buf.text(), "let a = foo( bar);\n  x\n");
        let mut buf = b("a\n b\n  c\nd\n");
        buf.view.set_sel(Selection::new(0, buf.rope.line_to_char(2) + 1));
        buf.join_lines();
        assert_eq!(buf.text(), "a b c\nd\n");
        assert_eq!(buf.undo.len(), 1);
        // The last line has nothing to join with.
        buf.goto_line(1, 0);
        buf.join_lines();
        assert_eq!(buf.text(), "a b c\nd\n");
    }

    #[test]
    fn toggle_case_word_and_selection() {
        let mut buf = b("helloWorld foo");
        buf.goto_line(0, 2);
        buf.toggle_case();
        assert_eq!(buf.text(), "HELLOWORLD foo");
        assert_eq!(buf.view.sel().head, 2);
        buf.toggle_case();
        assert_eq!(buf.text(), "helloworld foo");
        let mut buf = b("straße");
        buf.select_all();
        buf.toggle_case();
        assert_eq!(buf.text(), "STRASSE");
        assert_eq!(buf.view.sel().range(), 0..7, "selection covers the longer text");
    }

    #[test]
    fn delete_word_forward_mirrors_back() {
        let mut buf = b("foo bar.baz");
        buf.goto_line(0, 0);
        buf.delete_word_forward();
        assert_eq!(buf.text(), "bar.baz");
        buf.delete_word_forward();
        assert_eq!(buf.text(), ".baz");
        buf.goto_line(0, 4);
        buf.delete_word_forward();
        assert_eq!(buf.text(), ".baz", "nothing after the end");
    }

    #[test]
    fn crlf_backspace_removes_pair() {
        let mut buf = b("a\r\nb");
        buf.goto_line(1, 0);
        buf.backspace();
        assert_eq!(buf.text(), "ab");
    }

    #[test]
    fn toggle_comment_block_at_min_indent() {
        let original = "    a\n\n  b\n      c\nd\n";
        let mut buf = b(original);
        // Lines 0-3; the selection ends at column 0 of line 4, which is left out.
        buf.view.set_sel(Selection::new(0, buf.rope.line_to_char(4)));
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), "  //   a\n\n  // b\n  //     c\nd\n");
        assert_eq!(buf.undo.len(), 1);
        assert_eq!(buf.view.sel().range(), 0..buf.rope.line_to_char(4));
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), original);
        assert!(buf.undo());
        assert!(buf.undo());
        assert_eq!(buf.text(), original);
    }

    #[test]
    fn toggle_comment_caret_line_moves_down() {
        let mut buf = b("let a;\nlet b;\n");
        buf.goto_line(0, 3);
        buf.toggle_line_comment("#");
        assert_eq!(buf.text(), "# let a;\nlet b;\n");
        assert_eq!(buf.cursor_line_col(), (1, 5));
        // Partially commented selection: comment everything again.
        buf.view.set_sel(Selection::new(0, buf.len_chars()));
        buf.toggle_line_comment("#");
        assert_eq!(buf.text(), "# # let a;\n# let b;\n");
    }

    #[test]
    fn block_comment_wraps_and_unwraps_selection() {
        let mut buf = b("let a = f(x, y);\n");
        // Select "f(x, y)" backwards; the selection grows to the delimiters.
        buf.view.set_sel(Selection::new(15, 8));
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "let a = /* f(x, y) */;\n");
        assert_eq!(buf.selected_text(), "/* f(x, y) */");
        assert_eq!(buf.view.sel().head, 8, "direction kept");
        assert_eq!(buf.undo.len(), 1);
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "let a = f(x, y);\n");
        assert_eq!(buf.selected_text(), "f(x, y)");
        // Surrounding whitespace in the selection is left outside; no inner
        // spaces to drop is fine too.
        let mut buf = b("<!--x--> y");
        buf.view.set_sel(Selection::new(0, 9));
        buf.toggle_block_comment("<!--", "-->");
        assert_eq!(buf.text(), "x y");
        assert_eq!(buf.selected_text(), "x ");
        assert!(buf.undo());
        assert_eq!(buf.text(), "<!--x--> y", "one undo step");
    }

    #[test]
    fn block_comment_caret_line() {
        let mut buf = b("fn f() {\n    call();\n}\n");
        buf.goto_line(1, 6);
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "fn f() {\n    /* call(); */\n}\n");
        assert_eq!(buf.cursor_line_col(), (1, 9), "caret stays on the same character");
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "fn f() {\n    call();\n}\n");
        assert_eq!(buf.cursor_line_col(), (1, 6));
        // A blank line gets an empty comment with the caret inside.
        let mut buf = b("a\n  \nb");
        buf.goto_line(1, 0);
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "a\n  /**/\nb");
        assert_eq!(buf.cursor_line_col(), (1, 4));
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "a\n  \nb");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn language_falls_back_to_path_without_syntax() {
        let buf = Buffer::from_text(1, Some(PathBuf::from("/tmp/big.rs")), "", false);
        assert!(buf.syntax.is_none());
        assert_eq!(buf.language().map(|d| d.name), Some("rust"));
        assert_eq!(buf.language().and_then(|d| d.block_comment), Some(("/*", "*/")));
        assert!(b("x").language().is_none());
    }

    #[test]
    fn newline_after_opener_indents() {
        let mut buf = b("    foo(  ");
        buf.view.set_sel(Selection::point(10));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "    foo(\n        ", "trailing whitespace dropped, one level deeper");
        assert_eq!(buf.cursor_line_col(), (1, 8));
        let mut buf = b("\tx = [");
        buf.indent = IndentStyle::tabs(4);
        buf.view.set_sel(Selection::point(6));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "\tx = [\n\t\t", "tab indent adds a tab");
    }

    #[test]
    fn newline_between_pair_splits_it() {
        let mut buf = b("  if x {}");
        buf.view.set_sel(Selection::point(8));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "  if x {\n      \n  }");
        assert_eq!(buf.cursor_line_col(), (1, 6));
        assert_eq!(buf.undo.len(), 1);
        assert!(buf.undo());
        assert_eq!(buf.text(), "  if x {}");
        // A non-matching closer is left on the caret line.
        let mut buf = b("f(]");
        buf.view.set_sel(Selection::point(2));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "f(\n    ]");
    }

    #[test]
    fn newline_colon_only_when_asked() {
        let mut buf = b("def f():");
        buf.view.set_sel(Selection::point(8));
        buf.newline_smart(true);
        assert_eq!(buf.text(), "def f():\n    ");
        let mut buf = b("label:");
        buf.view.set_sel(Selection::point(6));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "label:\n");
    }

    #[test]
    fn newline_copies_indent() {
        let mut buf = b("    foo\nbar");
        buf.view.set_sel(Selection::point(7));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "    foo\n    \nbar");
        assert_eq!(buf.cursor_line_col(), (1, 4));
        // Caret inside the indent: the rest of the line keeps its column.
        let mut buf = b("    foo");
        buf.view.set_sel(Selection::point(2));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "\n    foo");
        assert_eq!(buf.cursor_line_col(), (1, 2));
        // A selection is replaced.
        let mut buf = b("  ab cd");
        buf.view.set_sel(Selection::new(4, 5));
        buf.newline_smart(false);
        assert_eq!(buf.text(), "  ab\n  cd");
    }

    #[test]
    fn closer_on_blank_line_dedents() {
        let mut buf = b("fn f() {\n    if x {\n        a();\n        ");
        let end = buf.len_chars();
        buf.view.set_sel(Selection::point(end));
        let indent = buf.closer_indent('}', None).expect("dedent applies");
        assert_eq!(indent, "    ");
        buf.type_dedented('}', &indent);
        assert_eq!(buf.text(), "fn f() {\n    if x {\n        a();\n    }");
        assert_eq!(buf.cursor_line_col(), (3, 5));
        assert!(buf.undo());
        assert_eq!(buf.text(), "fn f() {\n    if x {\n        a();\n        ");
        // Not on a line with text before the caret, nor at the right indent already.
        let mut buf = b("{\n  a");
        buf.view.set_sel(Selection::point(5));
        assert_eq!(buf.closer_indent('}', None), None);
        let mut buf = b("  {\n  ");
        buf.view.set_sel(Selection::point(6));
        assert_eq!(buf.closer_indent('}', None), None);
        assert_eq!(buf.closer_indent('x', None), None);
    }

    #[test]
    fn crlf_newline_smart() {
        let mut buf = b("a {}\r\nb");
        assert_eq!(buf.line_ending, LineEnding::CrLf);
        buf.view.set_sel(Selection::point(3));
        buf.indent = IndentStyle::spaces(2);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "a {\r\n  \r\n}\r\nb");
        assert_eq!(buf.cursor_line_col(), (1, 2));
        let mut buf = b("x {\r\n    \r\ny");
        buf.view.set_sel(Selection::point(9));
        let indent = buf.closer_indent('}', None).unwrap();
        buf.type_dedented('}', &indent);
        assert_eq!(buf.text(), "x {\r\n}\r\ny");
    }

    /// Expand once and return the selected text.
    fn grow(buf: &mut Buffer) -> String {
        buf.expand_selection();
        buf.selected_text()
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn expand_and_shrink_by_syntax_node() {
        let src = "fn main() {\n    let a = 1;\n    foo(bar);\n}\n";
        let mut buf = b(src);
        let def = super::super::syntax::language_for_name("rust").unwrap();
        buf.syntax = super::super::syntax::Syntax::new(def, &crate::config::theme::Theme::default());
        let caret = src.find("oo(").unwrap();
        buf.view.set_sel(Selection::point(caret));
        assert_eq!(grow(&mut buf), "foo");
        assert_eq!(grow(&mut buf), "foo(bar)");
        assert_eq!(grow(&mut buf), "foo(bar);");
        // Block contents before the block, as in JetBrains.
        assert_eq!(grow(&mut buf), "let a = 1;\n    foo(bar);");
        assert_eq!(grow(&mut buf), "{\n    let a = 1;\n    foo(bar);\n}");
        assert_eq!(grow(&mut buf), "fn main() {\n    let a = 1;\n    foo(bar);\n}");
        assert_eq!(grow(&mut buf), src);
        let top = buf.view.sel();
        buf.expand_selection();
        assert_eq!(buf.view.sel(), top, "nothing larger than the whole text");
        assert_eq!(buf.view.sel().anchor, 0, "anchor at start, head at end");
        for want in [
            "fn main() {\n    let a = 1;\n    foo(bar);\n}",
            "{\n    let a = 1;\n    foo(bar);\n}",
            "let a = 1;\n    foo(bar);",
            "foo(bar);",
            "foo(bar)",
            "foo",
        ] {
            buf.shrink_selection();
            assert_eq!(buf.selected_text(), want);
        }
        buf.shrink_selection();
        assert_eq!(buf.view.sel(), Selection::point(caret));
        buf.shrink_selection();
        assert_eq!(buf.view.sel(), Selection::point(caret), "empty stack is a no-op");
        // Caret right after an identifier starts from it; argument parens
        // get their contents first only when that is larger.
        buf.view.set_sel(Selection::point(src.find("bar").unwrap() + 3));
        assert_eq!(grow(&mut buf), "bar");
        assert_eq!(grow(&mut buf), "(bar)");
    }

    #[test]
    fn expand_and_shrink_every_selection() {
        let mut buf = b("one two\nthree four\n");
        carets(&mut buf, &[1, 10]);
        buf.expand_selection();
        assert_eq!(buf.view.sels.as_slice(), [Selection::new(0, 3), Selection::new(8, 13)]);
        assert_eq!(buf.view.sels.primary_index(), 1);
        buf.shrink_selection();
        assert_eq!(heads(&buf), [1, 10]);
    }

    #[test]
    fn expand_plain_text_and_stale_stack() {
        let mut buf = b("one two\nthree\n");
        buf.view.set_sel(Selection::point(5));
        assert_eq!(grow(&mut buf), "two");
        assert_eq!(grow(&mut buf), "one two\n");
        assert_eq!(grow(&mut buf), "one two\nthree\n");
        buf.shrink_selection();
        assert_eq!(buf.selected_text(), "one two\n");
        // A selection changed outside expand/shrink (e.g. a mouse drag)
        // invalidates the stack instead of jumping back.
        buf.view.set_sel(Selection::new(8, 10));
        buf.shrink_selection();
        assert_eq!(buf.view.sel(), Selection::new(8, 10));
        assert!(buf.view.expand_stack.is_empty());
        // Edits clear it too.
        buf.view.set_sel(Selection::point(1));
        buf.expand_selection();
        buf.insert("x", TxKind::Other);
        assert!(buf.view.expand_stack.is_empty());
        let mut buf = b("");
        buf.expand_selection();
        assert!(buf.view.expand_stack.is_empty(), "nothing to expand in an empty buffer");
    }

    #[test]
    fn prepare_for_save_trims_except_caret_line() {
        let mut buf = b("a  \nb\t \nc  \nd");
        buf.view.set_sel(Selection::point(7));
        buf.prepare_for_save(true, false);
        assert_eq!(buf.text(), "a\nb\t \nc\nd", "caret line 1 keeps its whitespace");
        assert_eq!(buf.view.sel(), Selection::point(5), "caret mapped past the trim on line 0");
        assert!(buf.undo());
        assert_eq!(buf.text(), "a  \nb\t \nc  \nd", "one transaction restores all of it");
        // CRLF: the `\r` of a break is not trailing whitespace.
        let mut buf = b("x \r\ny\r\n");
        buf.view.set_sel(Selection::point(8));
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "x\r\ny\r\n");
    }

    #[test]
    fn prepare_for_save_adds_final_newline_once() {
        let mut buf = b("a\nb");
        buf.view.set_sel(Selection::point(3));
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "a\nb\n");
        assert_eq!(buf.view.sel(), Selection::point(3), "caret stays before the added break");
        let n = buf.undo.len();
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "a\nb\n");
        assert_eq!(buf.undo.len(), n);
        let mut buf = b("a\r\nb");
        buf.prepare_for_save(false, true);
        assert_eq!(buf.text(), "a\r\nb\r\n", "the buffer's line ending");
        let mut buf = b("");
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "", "an empty buffer stays empty");
        let mut buf = b("a");
        buf.prepare_for_save(true, false);
        assert_eq!(buf.text(), "a", "final_newline = false");
    }

    #[test]
    fn prepare_for_save_leaves_markdown_whitespace() {
        let mut buf = Buffer::from_text(1, Some(PathBuf::from("/tmp/notes.md")), "hard  \nbreak  \n", false);
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "hard  \nbreak  \n");
        assert!(!buf.undo.can_undo());
    }

    #[test]
    fn prepare_for_save_clean_buffer_creates_no_transaction() {
        let mut buf = b("");
        buf.insert("clean\n", TxKind::Other);
        buf.mark_saved();
        buf.insert("x", TxKind::Typing);
        buf.undo();
        assert!(!buf.dirty());
        buf.prepare_for_save(true, true);
        assert!(!buf.dirty());
        assert_eq!(buf.undo.len(), 1);
        assert!(buf.undo.can_redo(), "redo history survives");
    }

    fn marks(buf: &Buffer) -> Vec<usize> {
        buf.bookmarks.keys().copied().collect()
    }

    #[test]
    fn bookmarks_shift_with_lines_inserted_above() {
        let mut buf = b("a\nb\nc\nd\n");
        buf.goto_line(2, 0);
        buf.toggle_bookmark();
        buf.goto_line(3, 1);
        buf.toggle_bookmark();
        assert_eq!(marks(&buf), vec![2, 3]);
        // Two lines typed at the start of line 1 push both marks down.
        buf.goto_line(1, 0);
        buf.insert("x\ny\n", TxKind::Other);
        assert_eq!(marks(&buf), vec![4, 5]);
        // Enter at the end of a marked line leaves the mark where it is.
        buf.goto_line(4, 1);
        buf.insert("\n", TxKind::Other);
        assert_eq!(marks(&buf), vec![4, 6]);
        // Undo and redo map the marks through the same changes.
        assert!(buf.undo());
        assert!(buf.undo());
        assert_eq!(marks(&buf), vec![2, 3]);
        assert!(buf.redo());
        assert_eq!(marks(&buf), vec![4, 5]);
        // Toggling again removes the mark.
        buf.goto_line(5, 0);
        buf.toggle_bookmark();
        assert_eq!(marks(&buf), vec![4]);
    }

    #[test]
    fn bookmarks_drop_with_deleted_lines_and_merge_on_join() {
        let mut buf = b("a\nb\nc\nd");
        buf.set_bookmarks(&[1, 2, 3, 9]);
        assert_eq!(marks(&buf), vec![1, 2, 3], "lines past the end are ignored");
        // Ctrl+Y on line 1 drops its mark and moves the ones below up.
        buf.goto_line(1, 0);
        buf.delete_line();
        assert_eq!((buf.text().as_str(), marks(&buf)), ("a\nc\nd", vec![1, 2]));
        // Deleting the last line takes its preceding line break along.
        buf.goto_line(2, 0);
        buf.delete_line();
        assert_eq!((buf.text().as_str(), marks(&buf)), ("a\nc", vec![1]));
        // Backspace at the start of a marked line merges it into the line
        // above, which inherits the mark.
        buf.goto_line(1, 0);
        buf.backspace();
        assert_eq!((buf.text().as_str(), marks(&buf)), ("ac", vec![0]));
        // Undo puts the line back; the mark stays with the merged text.
        assert!(buf.undo());
        assert_eq!(marks(&buf), vec![0]);
    }

    #[test]
    fn bookmarks_follow_moved_lines_and_reloads() {
        let mut buf = b("one\ntwo\nthree\n");
        buf.set_bookmarks(&[2]);
        buf.goto_line(2, 0);
        buf.move_lines(true);
        assert_eq!((buf.text().as_str(), marks(&buf)), ("one\nthree\ntwo\n", vec![1]));
        assert!(buf.undo());
        assert_eq!(marks(&buf), vec![2]);
        // A whole-text replacement (reload from disk) keeps marks on their text.
        let old = buf.text();
        buf.apply(vec![Change::replace(0, old, "zero\none\ntwo\nthree\n")], Selection::point(0), TxKind::Other);
        assert_eq!(marks(&buf), vec![3]);
    }

    #[test]
    fn numbered_bookmarks_follow_edits_and_win_merges() {
        let mut buf = b("a\nb\nc\nd\n");
        buf.set_bookmarks(&[1]);
        buf.bookmarks.insert(2, Some(3));
        buf.goto_line(0, 0);
        buf.insert("x\n", TxKind::Other);
        assert_eq!((marks(&buf), buf.digits(), buf.digit_line(3)), (vec![2, 3], vec![(3, 3)], Some(3)));
        // Backspace at the start of line 3 merges it into marked line 2:
        // the merged mark keeps the digit.
        buf.goto_line(3, 0);
        buf.backspace();
        assert_eq!(buf.bookmarks.iter().map(|(l, d)| (*l, *d)).collect::<Vec<_>>(), vec![(2, Some(3))]);
        assert!(buf.undo());
        assert_eq!(buf.digit_line(3), Some(2));
        assert_eq!(buf.digit_line(4), None);
    }

    #[test]
    fn line_ending_conversion_is_one_undo_step() {
        let src = "fn a() {\n\n    x\r\n}\rtail\n";
        let mut buf = b(src);
        assert_eq!(buf.line_ending, LineEnding::CrLf);
        buf.set_bookmarks(&[2, 3]);
        // Caret at the end of line 2 ("    x").
        buf.goto_line(2, 5);
        let caret = buf.view.sel().head;
        buf.convert_line_endings(LineEnding::Lf);
        assert_eq!(buf.text(), "fn a() {\n\n    x\n}\rtail\n");
        assert_eq!(buf.line_ending, LineEnding::Lf);
        assert!(buf.dirty());
        buf.convert_line_endings(LineEnding::CrLf);
        // Only the breaks changed; the lone `\r` stays.
        assert_eq!(buf.text(), "fn a() {\r\n\r\n    x\r\n}\rtail\r\n");
        assert_eq!(buf.text().replace("\r\n", "\n"), src.replace("\r\n", "\n"));
        assert_eq!(buf.line_ending, LineEnding::CrLf);
        assert_eq!(buf.cursor_line_col(), (2, 5), "caret stays before the break");
        assert_eq!(marks(&buf), vec![2, 3]);
        // Each conversion is a single undo step that restores the setting.
        assert!(buf.undo());
        assert_eq!(buf.text(), "fn a() {\n\n    x\n}\rtail\n");
        assert_eq!(buf.line_ending, LineEnding::Lf);
        assert!(buf.undo());
        assert_eq!(buf.text(), src);
        assert_eq!(buf.line_ending, LineEnding::CrLf);
        assert_eq!(buf.view.sel().head, caret);
        assert!(!buf.dirty());
        assert!(buf.redo());
        assert_eq!(buf.line_ending, LineEnding::Lf);
        // Nothing to convert: only the setting changes, the buffer stays clean.
        let mut buf = b("one line");
        buf.convert_line_endings(LineEnding::CrLf);
        assert_eq!((buf.text().as_str(), buf.line_ending, buf.dirty()), ("one line", LineEnding::CrLf, false));
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn language_override_beats_path_detection() {
        let rust = super::super::syntax::language_for_name("rust").unwrap();
        let theme = crate::config::theme::Theme::default();
        let mut buf = Buffer::from_text(1, Some(PathBuf::from("/tmp/notes.txt")), "fn main() {}\n", false);
        assert!(buf.language().is_none());
        buf.set_language(Some(rust), super::super::syntax::Syntax::new(rust, &theme));
        assert_eq!(buf.language().map(|d| d.name), Some("rust"));
        assert!(buf.syntax.is_some());
        // Plain text wins over the `.rs` the path suggests.
        let mut buf = Buffer::from_text(1, Some(PathBuf::from("/tmp/main.rs")), "fn main() {}\n", false);
        buf.syntax = super::super::syntax::Syntax::new(rust, &theme);
        assert_eq!(buf.language().map(|d| d.name), Some("rust"));
        buf.set_language(None, None);
        assert!(buf.language().is_none());
        assert!(buf.syntax.is_none());
        assert_eq!(buf.language_choice, LanguageChoice::Plain);
        // A named language without a highlighter (too large) still counts.
        buf.set_language(Some(rust), None);
        assert_eq!(buf.language().map(|d| d.name), Some("rust"));
    }
}
