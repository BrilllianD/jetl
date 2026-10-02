//! A text buffer: rope, selection, undo history, file metadata.
//! Every mutation goes through [`Buffer::apply`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use ropey::Rope;

use super::indent::{FileSettings, IndentStyle};
use super::selection::{self, Selection, line_len_no_eol, line_range};
use super::undo::{Change, Transaction, TxKind, UndoStack};

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
    pub sel: Selection,
    pub desired_col: Option<usize>,
    pub undo: UndoStack,
    pub line_ending: LineEnding,
    pub lossy: bool,
    pub scroll_line: usize,
    pub scroll_col: usize,
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
    pub search: Option<super::search::SearchState>,
    /// Caret, version and text area size when the view last followed the
    /// caret. The view only scrolls to the caret when one of them changes,
    /// so wheel scrolling is not undone on the next frame.
    pub follow_key: Option<(usize, u64, u16, u16)>,
    /// Expand-selection history: the selection before each step and the
    /// one the step produced. Shrink only pops while the current selection
    /// is still the produced one, so selection changes made elsewhere (mouse,
    /// undo) cannot restore a stale range. Cleared on every edit.
    pub expand_stack: Vec<(Selection, Selection)>,
    /// Modification time of the file when it was loaded or last saved;
    /// autosave refuses to write over a file that changed since.
    pub disk_mtime: Option<std::time::SystemTime>,
    /// Version the editor's idle autosave timer last saw.
    pub autosave_seen: u64,
    /// Version at which an autosave failed; no retry until the next edit.
    pub autosave_failed: Option<u64>,
    /// The file changed on disk under unsaved edits; no autosave until an
    /// explicit save.
    pub autosave_conflict: bool,
    /// Bookmarked lines (F11). Every applied change (edits, undo, redo)
    /// carries them along with their text; lines deleted outright drop out.
    pub bookmarks: BTreeSet<usize>,
    /// What Tab, Enter and indent/unindent insert; set by the editor on
    /// open from `.editorconfig`, the text or the config.
    pub indent: IndentStyle,
    /// The file's `.editorconfig` settings (all unset without one).
    pub settings: FileSettings,
}

impl Buffer {
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
            sel: Selection::default(),
            desired_col: None,
            undo: UndoStack::new(),
            line_ending,
            lossy,
            scroll_line: 0,
            scroll_col: 0,
            changed_since_render: true,
            syntax: None,
            language_choice: LanguageChoice::Auto,
            detected_language: None,
            gutter: Vec::new(),
            gutter_version: u64::MAX,
            diff_base: None,
            diff_base_seq: 0,
            search: None,
            follow_key: None,
            expand_stack: Vec::new(),
            disk_mtime: None,
            autosave_seen: 0,
            autosave_failed: None,
            autosave_conflict: false,
            bookmarks: BTreeSet::new(),
            indent: IndentStyle::default(),
            settings: FileSettings::default(),
        }
    }

    pub fn dirty(&self) -> bool {
        self.undo.state_id() != self.saved_state
    }

    /// Modified, has a file, and nothing holds autosave back. Files that
    /// were not valid UTF-8 are left to an explicit save, which re-encodes.
    pub fn autosave_candidate(&self) -> bool {
        self.path.is_some() && self.dirty() && !self.lossy && !self.autosave_conflict && self.autosave_failed != Some(self.version)
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

    pub fn selected_text(&self) -> String {
        let r = self.sel.range();
        self.rope.slice(r).to_string()
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        selection::pos_to_line_col(&self.rope, self.sel.head)
    }

    /// Apply a transaction. Change positions refer to the buffer state after
    /// the previous change in the same transaction.
    pub fn apply(&mut self, changes: Vec<Change>, sel_after: Selection, kind: TxKind) {
        self.apply_then(changes, kind, |_| sel_after);
    }

    /// Like [`Buffer::apply`], but the selection after the transaction is
    /// computed from the edited rope, so redo restores the same caret.
    pub fn apply_then(&mut self, changes: Vec<Change>, kind: TxKind, sel_after: impl FnOnce(&Rope) -> Selection) {
        self.expand_stack.clear();
        if changes.is_empty() {
            self.sel = sel_after(&self.rope);
            return;
        }
        let sel_before = self.sel;
        for c in &changes {
            self.apply_change(c);
        }
        let mut sel_after = sel_after(&self.rope);
        sel_after.clamp(self.rope.len_chars());
        self.sel = sel_after;
        self.desired_col = None;
        self.version += 1;
        self.changed_since_render = true;
        self.undo.push(Transaction { id: 0, changes, sel_before, sel_after, at: Instant::now(), kind, line_ending: None });
    }

    fn apply_change(&mut self, c: &Change) {
        let old_len = c.old.chars().count();
        let at = c.at.min(self.rope.len_chars());
        let marks: Vec<MarkTarget> = self.bookmarks.iter().map(|&l| mark_target(&self.rope, l, at, old_len, c)).collect();
        if old_len > 0 {
            let end = (at + old_len).min(self.rope.len_chars());
            self.rope.remove(at..end);
        }
        if !c.new.is_empty() {
            self.rope.insert(at, &c.new);
        }
        if !marks.is_empty() {
            let (len, last) = (self.rope.len_chars(), self.rope.len_lines() - 1);
            self.bookmarks = marks
                .into_iter()
                .filter_map(|t| match t {
                    MarkTarget::Pos(p) => Some(self.rope.char_to_line(p.min(len))),
                    MarkTarget::Line(l) => Some(l.min(last)),
                    MarkTarget::Drop => None,
                })
                .collect();
        }
        if let Some(s) = &mut self.syntax {
            s.note_change(&self.rope, at, old_len, c.new.chars().count(), &c.old, &c.new);
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(tx) = self.undo.pop_undo() else { return false };
        for c in tx.changes.iter().rev() {
            let inv = c.inverse();
            self.apply_change(&inv);
        }
        self.sel = tx.sel_before;
        self.sel.clamp(self.rope.len_chars());
        if let Some((before, _)) = tx.line_ending {
            self.line_ending = before;
        }
        self.version += 1;
        self.changed_since_render = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(tx) = self.undo.pop_redo() else { return false };
        for c in &tx.changes {
            self.apply_change(c);
        }
        self.sel = tx.sel_after;
        self.sel.clamp(self.rope.len_chars());
        if let Some((_, after)) = tx.line_ending {
            self.line_ending = after;
        }
        self.version += 1;
        self.changed_since_render = true;
        true
    }

    /// Replace the selection (or insert at the caret) with `s`.
    pub fn insert(&mut self, s: &str, kind: TxKind) {
        let r = self.sel.range();
        let old = self.rope.slice(r.clone()).to_string();
        let after = r.start + s.chars().count();
        self.apply(vec![Change::replace(r.start, old, s)], Selection::point(after), kind);
    }

    pub fn delete_selection(&mut self) -> bool {
        let r = self.sel.range();
        if r.is_empty() {
            return false;
        }
        let old = self.rope.slice(r.clone()).to_string();
        self.apply(vec![Change::delete(r.start, old)], Selection::point(r.start), TxKind::Other);
        true
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let p = self.sel.head;
        if p == 0 {
            return;
        }
        let mut start = p - 1;
        if start > 0 && self.rope.char(start) == '\n' && self.rope.char(start - 1) == '\r' {
            start -= 1;
        }
        let old = self.rope.slice(start..p).to_string();
        self.apply(vec![Change::delete(start, old)], Selection::point(start), TxKind::Deleting);
    }

    /// Type `c` with auto-close: wrap a selection in an opener and its
    /// closer, step over a closer that is already at the caret, or insert
    /// the pair when `may_pair` and the caret is at a word boundary. An
    /// empty `pairs` table types `c` as is.
    pub fn type_char(&mut self, c: char, pairs: &[(char, char)], may_pair: bool) {
        let kind = if c.is_whitespace() { TxKind::Other } else { TxKind::Typing };
        let opener = pairs.iter().find(|(o, _)| *o == c).map(|(_, close)| *close);
        let is_closer = pairs.iter().any(|(_, close)| *close == c);
        let r = self.sel.range();
        let len = self.rope.len_chars();
        if let (Some(close), false) = (opener, r.is_empty()) {
            let changes = vec![Change::insert(r.end, close.to_string()), Change::insert(r.start, c.to_string())];
            let sel_after = Selection::new(self.sel.anchor + 1, self.sel.head + 1);
            self.apply(changes, sel_after, TxKind::Other);
            return;
        }
        let p = self.sel.head;
        let next = (p < len).then(|| self.rope.char(p));
        if r.is_empty() && is_closer && next == Some(c) {
            self.sel = Selection::point(p + 1);
            return;
        }
        if let (Some(close), true, true) = (opener, r.is_empty(), may_pair) {
            let next_ok = next.is_none_or(|n| n.is_whitespace() || pairs.iter().any(|(_, cl)| *cl == n));
            let prev_word = p > 0 && selection::is_word_char(self.rope.char(p - 1));
            if next_ok && (close != c || !prev_word) {
                self.apply(vec![Change::insert(p, format!("{c}{close}"))], Selection::point(p + 1), kind);
                return;
            }
        }
        self.insert(&c.to_string(), kind);
    }

    /// Backspace between an auto-close pair such as `(|)` deletes both.
    /// Returns false when the caret is not inside a pair.
    pub fn backspace_pair(&mut self, pairs: &[(char, char)]) -> bool {
        let p = self.sel.head;
        if !self.sel.is_empty() || p == 0 || p >= self.rope.len_chars() {
            return false;
        }
        let around = (self.rope.char(p - 1), self.rope.char(p));
        if !pairs.contains(&around) {
            return false;
        }
        let old = format!("{}{}", around.0, around.1);
        self.apply(vec![Change::delete(p - 1, old)], Selection::point(p - 1), TxKind::Deleting);
        true
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let p = self.sel.head;
        if p >= self.rope.len_chars() {
            return;
        }
        let mut end = p + 1;
        if self.rope.char(p) == '\r' && end < self.rope.len_chars() && self.rope.char(end) == '\n' {
            end += 1;
        }
        let old = self.rope.slice(p..end).to_string();
        self.apply(vec![Change::delete(p, old)], Selection::point(p), TxKind::Deleting);
    }

    pub fn delete_word_back(&mut self) {
        if self.delete_selection() {
            return;
        }
        let p = self.sel.head;
        let s = selection::prev_word_boundary(&self.rope, p);
        if s == p {
            return;
        }
        let old = self.rope.slice(s..p).to_string();
        self.apply(vec![Change::delete(s, old)], Selection::point(s), TxKind::Other);
    }

    pub fn delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let p = self.sel.head;
        let e = selection::next_word_boundary(&self.rope, p);
        if e == p {
            return;
        }
        let old = self.rope.slice(p..e).to_string();
        self.apply(vec![Change::delete(p, old)], Selection::point(p), TxKind::Other);
    }

    /// Newline keeping the current line's indentation, one level deeper
    /// after an opening bracket (or a `:` when `colon`). Between a bracket
    /// pair such as `{|}` the closer moves to its own line at the base
    /// indent. Trailing whitespace before the caret is dropped.
    pub fn newline_smart(&mut self, colon: bool) {
        let r = self.sel.range();
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
        let caret = start + new.chars().count();
        if closer.is_some() && r.end < self.rope.len_chars() && Some(self.rope.char(r.end)) == closer {
            new.push_str(&format!("{eol}{base}"));
        }
        let old = self.rope.slice(start..r.end).to_string();
        self.apply(vec![Change::replace(start, old, new)], Selection::point(caret), TxKind::Other);
    }

    /// Indent for closer `c` typed at the caret on a line that is blank up
    /// to the caret: that of the line holding the matching opener. None
    /// when that does not apply or the line already has that indent.
    pub fn closer_indent(&self, c: char, tree: Option<&tree_sitter::Tree>) -> Option<String> {
        if !matches!(c, '}' | ']' | ')') || !self.sel.is_empty() {
            return None;
        }
        let p = self.sel.head;
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

    /// Type closer `c` after replacing the caret line's leading whitespace
    /// with `indent` (from [`Buffer::closer_indent`]), as one transaction.
    pub fn type_dedented(&mut self, c: char, indent: &str) {
        let p = self.sel.head;
        let ls = self.rope.line_to_char(self.rope.char_to_line(p));
        let old = self.rope.slice(ls..p).to_string();
        let caret = ls + indent.chars().count() + 1;
        self.apply(vec![Change::replace(ls, old, format!("{indent}{c}"))], Selection::point(caret), TxKind::Typing);
    }

    /// A tab with hard tabs, else spaces up to the next indent stop.
    pub fn insert_tab(&mut self) {
        if self.indent.hard_tabs {
            self.insert("\t", TxKind::Typing);
            return;
        }
        let w = self.indent.width.max(1);
        let (_, col) = self.cursor_line_col();
        self.insert(&" ".repeat(w - col % w), TxKind::Typing);
    }

    pub fn delete_line(&mut self) {
        let r = self.sel.range();
        let (l0, _) = selection::pos_to_line_col(&self.rope, r.start);
        let (l1, _) = selection::pos_to_line_col(&self.rope, r.end);
        let mut range = line_range(&self.rope, l0).start..line_range(&self.rope, l1).end;
        if range.end == self.rope.len_chars() && range.start > 0 && self.rope.char(range.start - 1) == '\n' {
            // Last line: also remove the preceding line break.
            let mut s = range.start - 1;
            if s > 0 && self.rope.char(s - 1) == '\r' {
                s -= 1;
            }
            range.start = s;
        }
        let (_, col) = self.cursor_line_col();
        let old = self.rope.slice(range.clone()).to_string();
        let new_line = selection::pos_to_line_col(&self.rope, range.start).0;
        self.apply_then(vec![Change::delete(range.start, old)], TxKind::Other, |rope| {
            Selection::point(selection::line_col_to_pos(rope, new_line, col))
        });
    }

    pub fn duplicate_line(&mut self) {
        let r = self.sel.range();
        if !r.is_empty() {
            let text = self.rope.slice(r.clone()).to_string();
            let n = text.chars().count();
            self.apply(vec![Change::insert(r.end, text)], Selection::new(r.end, r.end + n), TxKind::Other);
            return;
        }
        let (line, col) = self.cursor_line_col();
        let lr = line_range(&self.rope, line);
        let mut text = self.rope.slice(lr.clone()).to_string();
        if !text.ends_with('\n') {
            text = format!("{}{}", self.line_ending.as_str(), text);
        }
        self.apply_then(vec![Change::insert(lr.end, text)], TxKind::Other, |rope| {
            Selection::point(selection::line_col_to_pos(rope, line + 1, col))
        });
    }

    pub fn select_all(&mut self) {
        self.sel = Selection::new(0, self.rope.len_chars());
    }

    pub fn select_line(&mut self) {
        let (line, _) = self.cursor_line_col();
        let r = line_range(&self.rope, line);
        self.sel = Selection::new(r.start, r.end);
    }

    /// Select the smallest syntax node (or bracket contents) around the
    /// selection; word, line, all without a syntax tree.
    pub fn expand_selection(&mut self) {
        if let Some(s) = &mut self.syntax {
            s.reparse(&self.rope);
        }
        if self.expand_stack.last().is_some_and(|(_, after)| *after != self.sel) {
            self.expand_stack.clear();
        }
        let tree = self.syntax.as_ref().and_then(|s| s.tree());
        let Some(r) = super::expand::expand_range(&self.rope, tree, self.sel.range()) else { return };
        let after = Selection::new(r.start, r.end);
        self.expand_stack.push((self.sel, after));
        self.sel = after;
        self.desired_col = None;
        self.undo.break_group = true;
    }

    /// Undo the last expand step, if the selection is still what it made.
    pub fn shrink_selection(&mut self) {
        match self.expand_stack.pop() {
            Some((before, after)) if after == self.sel => {
                self.sel = before;
                self.desired_col = None;
                self.undo.break_group = true;
            }
            _ => self.expand_stack.clear(),
        }
    }

    /// Put the caret at char `pos` (clamped to the text).
    pub fn goto_pos(&mut self, pos: usize) {
        self.sel = Selection::point(pos.min(self.rope.len_chars()));
        self.desired_col = None;
        self.undo.break_group = true;
    }

    pub fn goto_line(&mut self, line: usize, col: usize) {
        let line = line.min(self.rope.len_lines().saturating_sub(1));
        let p = selection::line_col_to_pos(&self.rope, line, col);
        self.sel = Selection::point(p);
        self.desired_col = None;
        self.undo.break_group = true;
    }

    /// Comment or uncomment the selected lines (or the caret line) with a
    /// line comment `token`, as one transaction. Uncomments only when every
    /// non-blank line is already commented.
    pub fn toggle_line_comment(&mut self, token: &str) {
        let r = self.sel.range();
        let (l0, l1) = self.selected_lines();
        // (line, indent, blank) for each covered line.
        let lines: Vec<(usize, usize, bool)> = (l0..=l1)
            .map(|l| {
                let indent = selection::indent_of(&self.rope, l);
                (l, indent, indent >= line_len_no_eol(&self.rope, l))
            })
            .collect();
        // Comment blank lines too when there is nothing else to comment.
        let skip_blank = lines.iter().any(|(_, _, blank)| !blank);
        let targets: Vec<(usize, usize)> = lines.iter().filter(|(_, _, blank)| !(skip_blank && *blank)).map(|(l, i, _)| (*l, *i)).collect();
        let commented = |l: usize, indent: usize| {
            let s = self.rope.line_to_char(l) + indent;
            let n = token.chars().count();
            s + n <= self.rope.len_chars() && self.rope.slice(s..s + n) == token
        };
        let uncomment = skip_blank && targets.iter().all(|&(l, i)| commented(l, i));
        let min_indent = targets.iter().map(|(_, i)| *i).min().unwrap_or(0);
        let mut changes = Vec::with_capacity(targets.len());
        for &(l, indent) in targets.iter().rev() {
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
        let sel_after = self.sel_through(&changes);
        let was_empty = r.is_empty();
        self.apply_then(changes, TxKind::Other, |rope| {
            if !was_empty {
                return sel_after;
            }
            // JetBrains moves the caret to the next line after commenting.
            let (line, col) = selection::pos_to_line_col(rope, sel_after.head);
            if line + 1 < rope.len_lines() { Selection::point(selection::line_col_to_pos(rope, line + 1, col)) } else { sel_after }
        });
    }

    /// Wrap the selection, or the caret line from its indent to the end, in
    /// a block comment, or unwrap it when its trimmed text already is one.
    pub fn toggle_block_comment(&mut self, open: &str, close: &str) {
        let r = if self.sel.is_empty() {
            self.line_content(self.rope.char_to_line(self.sel.head), self.rope.char_to_line(self.sel.head))
        } else {
            self.sel.range()
        };
        self.toggle_block_comment_in(r, open, close);
    }

    /// Block comment toggle on whole lines: the selected lines (or the caret
    /// line) from the first one's indent to the last one's end. Ctrl+/ uses
    /// it in languages without line comments.
    pub fn toggle_block_comment_lines(&mut self, open: &str, close: &str) {
        let (l0, l1) = self.selected_lines();
        let r = self.line_content(l0, l1);
        self.toggle_block_comment_in(r, open, close);
    }

    /// From the indent of `l0` to the end of `l1`, without the line break.
    fn line_content(&self, l0: usize, l1: usize) -> std::ops::Range<usize> {
        self.rope.line_to_char(l0) + selection::indent_of(&self.rope, l0)..self.rope.line_to_char(l1) + line_len_no_eol(&self.rope, l1)
    }

    /// Toggle a block comment around the trimmed text of `r`, as one
    /// transaction. Unwrapping also drops one space inside each delimiter.
    /// A selection keeps covering the text (and the added delimiters); a
    /// caret stays on the same character.
    fn toggle_block_comment_in(&mut self, r: std::ops::Range<usize>, open: &str, close: &str) {
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
        let sel_after = if self.sel.is_empty() {
            // On a blank line the caret lands between the delimiters.
            let p = if trimmed.is_empty() { start + n(open) } else { map_through(&changes, self.sel.head, false) };
            Selection::point(p)
        } else {
            self.sel_through(&changes)
        };
        self.apply(changes, sel_after, TxKind::Other);
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
        self.changed_since_render = true;
    }

    /// Convert every line break to `le` (lone `\r` is left alone) as one
    /// undoable transaction that also carries the old and new `line_ending`,
    /// so undo and redo switch it back and forth with the text. Text without
    /// a break to convert only changes the setting.
    pub fn convert_line_endings(&mut self, le: LineEnding) {
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
        let sel = Selection::new(map_through(&changes, self.sel.anchor, true), map_through(&changes, self.sel.head, true));
        self.apply(changes, sel, TxKind::Other);
        if let Some(tx) = self.undo.last_mut() {
            tx.line_ending = Some((before, le));
        }
    }

    /// First and last line covered by the selection. A selection ending at
    /// column 0 does not include that line.
    fn selected_lines(&self) -> (usize, usize) {
        let r = self.sel.range();
        let (l0, _) = selection::pos_to_line_col(&self.rope, r.start);
        let (mut l1, c1) = selection::pos_to_line_col(&self.rope, r.end);
        if l1 > l0 && c1 == 0 {
            l1 -= 1;
        }
        (l0, l1)
    }

    /// The selection mapped through `changes` (applied in order, each
    /// relative to the text after the previous one). A non-empty selection
    /// starting where text is inserted grows to include it.
    fn sel_through(&self, changes: &[Change]) -> Selection {
        let r = self.sel.range();
        let (start, end) = (map_through(changes, r.start, !r.is_empty()), map_through(changes, r.end, false));
        if self.sel.anchor <= self.sel.head { Selection::new(start, end) } else { Selection::new(end, start) }
    }

    /// Indent every non-blank selected line by one level.
    pub fn indent_lines(&mut self) {
        let (l0, l1) = self.selected_lines();
        let pad = self.indent.unit();
        let changes: Vec<Change> = (l0..=l1)
            .rev()
            .filter(|&l| selection::indent_of(&self.rope, l) < line_len_no_eol(&self.rope, l))
            .map(|l| Change::insert(self.rope.line_to_char(l), pad.clone()))
            .collect();
        let sel_after = self.sel_through(&changes);
        self.apply(changes, sel_after, TxKind::Other);
    }

    /// Remove one level of indentation (a tab or up to the indent width in
    /// spaces) from the selected lines, or the caret line.
    pub fn unindent_lines(&mut self) {
        let width = self.indent.width.max(1);
        let (l0, l1) = self.selected_lines();
        let mut changes = Vec::new();
        for l in (l0..=l1).rev() {
            let ls = self.rope.line_to_char(l);
            let lead: String = self.rope.line(l).chars().take_while(|c| *c == ' ' || *c == '\t').take(width).collect();
            let old: String = if lead.starts_with('\t') { "\t".into() } else { lead.chars().take_while(|c| *c == ' ').collect() };
            if !old.is_empty() {
                changes.push(Change::delete(ls, old));
            }
        }
        let sel_after = self.sel_through(&changes);
        self.apply(changes, sel_after, TxKind::Other);
    }

    /// Index of the last line, not counting the empty line after a final
    /// line break.
    fn last_line(&self) -> usize {
        let last = self.rope.len_lines() - 1;
        if last > 0 && line_len_no_eol(&self.rope, last) == 0 { last - 1 } else { last }
    }

    /// Join the caret line with the next one, or all lines of a multi-line
    /// selection: the line break, trailing whitespace before it and the
    /// next line's indentation become one space (none next to an empty
    /// line). The caret lands at the join point.
    pub fn join_lines(&mut self) {
        let (l0, l1) = self.selected_lines();
        let l1 = if l1 == l0 { l0 + 1 } else { l1 };
        let l1 = l1.min(self.last_line());
        if l1 <= l0 {
            return;
        }
        let mut changes = Vec::new();
        for l in (l0..l1).rev() {
            let text = self.line_text(l);
            let kept = text.trim_end().chars().count();
            let at = self.rope.line_to_char(l) + kept;
            let next = self.rope.line_to_char(l + 1);
            let next_indent = selection::indent_of(&self.rope, l + 1);
            let next_empty = next_indent >= line_len_no_eol(&self.rope, l + 1);
            let old = self.rope.slice(at..next + next_indent).to_string();
            let new = if kept == 0 || next_empty { "" } else { " " };
            changes.push(Change::replace(at, old, new));
        }
        let sel_after = if self.sel.is_empty() { Selection::point(changes[0].at) } else { self.sel_through(&changes) };
        self.apply(changes, sel_after, TxKind::Other);
    }

    /// Uppercase the selection (or the word at the caret) if it has any
    /// lowercase char, otherwise lowercase it.
    pub fn toggle_case(&mut self) {
        let r = if self.sel.is_empty() { selection::word_at(&self.rope, self.sel.head) } else { self.sel.range() };
        if r.is_empty() {
            return;
        }
        let old = self.rope.slice(r.clone()).to_string();
        let new = if old.chars().any(char::is_lowercase) { old.to_uppercase() } else { old.to_lowercase() };
        if new == old {
            return;
        }
        // Case mapping can change the length (ß -> SS).
        let n = new.chars().count();
        let sel_after =
            if self.sel.is_empty() { Selection::point(self.sel.head.min(r.start + n)) } else { Selection::new(r.start, r.start + n) };
        self.apply(vec![Change::replace(r.start, old, new)], sel_after, TxKind::Other);
    }

    /// Swap the selected lines (or the caret line) with the line above or
    /// below; the selection moves with them. No-op at the document edge;
    /// the empty line after a final line break does not count as a line.
    pub fn move_lines(&mut self, up: bool) {
        let (l0, l1) = self.selected_lines();
        let last = self.last_line();
        if (up && l0 == 0) || (!up && l1 >= last) {
            return;
        }
        let (first, end) = if up { (l0 - 1, l1) } else { (l0, l1 + 1) };
        let range = line_range(&self.rope, first).start..line_range(&self.rope, end).end;
        let old = self.rope.slice(range.clone()).to_string();
        let mut lines: Vec<String> = (first..=end).map(|l| self.line_text(l)).collect();
        let neighbour_len = if up { lines[0].chars().count() } else { lines[lines.len() - 1].chars().count() };
        if up {
            lines.rotate_left(1);
        } else {
            lines.rotate_right(1);
        }
        let eol = self.line_ending.as_str();
        let mut new = lines.join(eol);
        if old.ends_with('\n') {
            new.push_str(eol);
        }
        let shift = neighbour_len + eol.chars().count();
        let moved = |p: usize| if up { p - shift } else { p + shift };
        let sel_after = Selection::new(moved(self.sel.anchor), moved(self.sel.head));
        self.apply(vec![Change::replace(range.start, old, new)], sel_after, TxKind::Other);
    }

    /// Add or remove the bookmark on the caret line.
    pub fn toggle_bookmark(&mut self) {
        let line = self.cursor_line_col().0;
        if !self.bookmarks.remove(&line) {
            self.bookmarks.insert(line);
        }
    }

    /// Replace the bookmarks (from a session), dropping lines past the end.
    pub fn set_bookmarks(&mut self, lines: &[usize]) {
        let n = self.rope.len_lines();
        self.bookmarks = lines.iter().copied().filter(|l| *l < n).collect();
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
        self.saved_state = self.undo.state_id();
        self.undo.break_group = true;
    }

    /// Clean up before writing: strip trailing spaces and tabs (`trim`) and
    /// end a non-empty buffer with a line break (`final_newline`), as one
    /// undoable transaction. The caret line keeps its whitespace, as in
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
            let caret_line = self.cursor_line_col().0;
            for l in (0..self.rope.len_lines()).rev().filter(|&l| l != caret_line) {
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
        let sel = Selection::new(map_through(&changes, self.sel.anchor, true), map_through(&changes, self.sel.head, true));
        self.apply(changes, sel, TxKind::Other);
    }

    fn is_markdown(&self) -> bool {
        let ext = self.path.as_deref().and_then(|p| p.extension()).and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        self.language().is_some_and(|d| d.name == "markdown") || matches!(ext.as_deref(), Some("md" | "markdown"))
    }
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

/// Map position `p` through `changes`. With `stay_at_insert`, a position
/// exactly where text is inserted stays before the insertion.
fn map_through(changes: &[Change], mut p: usize, stay_at_insert: bool) -> usize {
    for c in changes {
        let old_n = c.old.chars().count();
        if p < c.at || (p == c.at && stay_at_insert && old_n == 0) {
            continue;
        }
        p = if p >= c.at + old_n { p - old_n + c.new.chars().count() } else { c.at };
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(text: &str) -> Buffer {
        Buffer::from_text(1, None, text, false)
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
        assert_eq!(buf.sel.head, 5);
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
            buf.sel = Selection::point(pos);
            buf.undo.break_group = true;
            let n = buf.undo.len();
            match i % 4 {
                0 => buf.insert(&format!("w{i} "), TxKind::Other),
                1 => buf.backspace(),
                2 => {
                    buf.sel = Selection::new(pos, (pos + 3).min(len));
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
        buf.sel = Selection::point(3);
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
            let (text, sel) = (buf.text(), buf.sel);
            assert!(buf.undo(), "{name}");
            assert!(buf.redo(), "{name}");
            assert_eq!(buf.text(), text, "{name}");
            assert_eq!(buf.sel, sel, "{name}");
        }
    }

    const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];

    #[test]
    fn auto_close_wraps_selection() {
        let mut buf = b("let x = a + b;");
        buf.sel = Selection::new(13, 8);
        buf.type_char('(', PAIRS, true);
        assert_eq!(buf.text(), "let x = (a + b);");
        assert_eq!(buf.sel, Selection::new(14, 9), "selection stays on the wrapped text");
        assert!(buf.undo());
        assert_eq!(buf.text(), "let x = a + b;");
    }

    #[test]
    fn auto_close_pairs_and_steps_over() {
        let mut buf = b("f\n");
        buf.sel = Selection::point(1);
        buf.type_char('(', PAIRS, true);
        assert_eq!(buf.text(), "f()\n");
        assert_eq!(buf.sel.head, 2);
        buf.type_char('x', PAIRS, true);
        buf.type_char(')', PAIRS, true);
        assert_eq!(buf.text(), "f(x)\n", "closer at the caret is stepped over");
        assert_eq!(buf.sel.head, 4);
        // Before a word char the opener is typed alone.
        buf.sel = Selection::point(0);
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
        assert_eq!(buf.sel.head, 3);
        let mut buf = b("don");
        buf.sel = Selection::point(3);
        buf.type_char('\'', PAIRS, true);
        assert_eq!(buf.text(), "don'", "apostrophe after a word char");
        // Rust has no '' pair, so lifetimes type plainly.
        let rust = crate::editor::brackets::pairs(Some("rust"));
        let mut buf = b("&");
        buf.sel = Selection::point(1);
        buf.type_char('\'', rust, true);
        assert_eq!(buf.text(), "&'");
    }

    #[test]
    fn backspace_deletes_pair_and_disabled_table_types_plainly() {
        let mut buf = b("");
        buf.type_char('(', PAIRS, true);
        assert!(buf.backspace_pair(PAIRS));
        assert_eq!(buf.text(), "");
        let mut buf = b("()");
        buf.sel = Selection::point(1);
        assert!(!buf.backspace_pair(&[]));
        buf.type_char(')', &[], true);
        assert_eq!(buf.text(), "())", "auto_close = false: no step-over");
        buf.sel = Selection::point(0);
        buf.type_char('(', &[], true);
        assert_eq!(buf.text(), "(())", "auto_close = false: no pair");
    }

    #[test]
    fn indent_and_unindent_selection() {
        let original = "fn a() {\n    x;\n\n\ty;\n}\n";
        let mut buf = b(original);
        // Lines 1-3; the selection ends at column 0 of line 4.
        buf.sel = Selection::new(buf.rope.line_to_char(1), buf.rope.line_to_char(4));
        buf.indent_lines();
        assert_eq!(buf.text(), "fn a() {\n        x;\n\n    \ty;\n}\n", "blank line untouched");
        assert_eq!(buf.sel.range(), buf.rope.line_to_char(1)..buf.rope.line_to_char(4), "selection covers the same lines");
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
        buf.sel = Selection::new(0, buf.rope.line_to_char(2));
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
        buf.sel = Selection::new(buf.rope.line_to_char(3), buf.rope.line_to_char(1));
        buf.move_lines(false);
        assert_eq!(buf.text(), "x\ny\na\nb\n");
        assert_eq!(buf.sel, Selection::new(buf.rope.line_to_char(4), buf.rope.line_to_char(2)));
        buf.move_lines(true);
        buf.move_lines(true);
        assert_eq!(buf.text(), "a\nb\nx\ny\n");
        assert_eq!(buf.sel.range(), 0..4);
    }

    #[test]
    fn join_lines_caret_and_selection() {
        let mut buf = b("let a = foo(  \n    bar);\n\n  x\n");
        buf.goto_line(0, 3);
        buf.join_lines();
        assert_eq!(buf.text(), "let a = foo( bar);\n\n  x\n");
        assert_eq!(buf.sel.head, 12, "caret at the join point");
        // Next to an empty line no space is added.
        buf.join_lines();
        assert_eq!(buf.text(), "let a = foo( bar);\n  x\n");
        let mut buf = b("a\n b\n  c\nd\n");
        buf.sel = Selection::new(0, buf.rope.line_to_char(2) + 1);
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
        assert_eq!(buf.sel.head, 2);
        buf.toggle_case();
        assert_eq!(buf.text(), "helloworld foo");
        let mut buf = b("straße");
        buf.select_all();
        buf.toggle_case();
        assert_eq!(buf.text(), "STRASSE");
        assert_eq!(buf.sel.range(), 0..7, "selection covers the longer text");
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
        buf.sel = Selection::new(0, buf.rope.line_to_char(4));
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), "  //   a\n\n  // b\n  //     c\nd\n");
        assert_eq!(buf.undo.len(), 1);
        assert_eq!(buf.sel.range(), 0..buf.rope.line_to_char(4));
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
        buf.sel = Selection::new(0, buf.len_chars());
        buf.toggle_line_comment("#");
        assert_eq!(buf.text(), "# # let a;\n# let b;\n");
    }

    #[test]
    fn block_comment_wraps_and_unwraps_selection() {
        let mut buf = b("let a = f(x, y);\n");
        // Select "f(x, y)" backwards; the selection grows to the delimiters.
        buf.sel = Selection::new(15, 8);
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "let a = /* f(x, y) */;\n");
        assert_eq!(buf.selected_text(), "/* f(x, y) */");
        assert_eq!(buf.sel.head, 8, "direction kept");
        assert_eq!(buf.undo.len(), 1);
        buf.toggle_block_comment("/*", "*/");
        assert_eq!(buf.text(), "let a = f(x, y);\n");
        assert_eq!(buf.selected_text(), "f(x, y)");
        // Surrounding whitespace in the selection is left outside; no inner
        // spaces to drop is fine too.
        let mut buf = b("<!--x--> y");
        buf.sel = Selection::new(0, 9);
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
        buf.sel = Selection::point(10);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "    foo(\n        ", "trailing whitespace dropped, one level deeper");
        assert_eq!(buf.cursor_line_col(), (1, 8));
        let mut buf = b("\tx = [");
        buf.indent = IndentStyle::tabs(4);
        buf.sel = Selection::point(6);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "\tx = [\n\t\t", "tab indent adds a tab");
    }

    #[test]
    fn newline_between_pair_splits_it() {
        let mut buf = b("  if x {}");
        buf.sel = Selection::point(8);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "  if x {\n      \n  }");
        assert_eq!(buf.cursor_line_col(), (1, 6));
        assert_eq!(buf.undo.len(), 1);
        assert!(buf.undo());
        assert_eq!(buf.text(), "  if x {}");
        // A non-matching closer is left on the caret line.
        let mut buf = b("f(]");
        buf.sel = Selection::point(2);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "f(\n    ]");
    }

    #[test]
    fn newline_colon_only_when_asked() {
        let mut buf = b("def f():");
        buf.sel = Selection::point(8);
        buf.newline_smart(true);
        assert_eq!(buf.text(), "def f():\n    ");
        let mut buf = b("label:");
        buf.sel = Selection::point(6);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "label:\n");
    }

    #[test]
    fn newline_copies_indent() {
        let mut buf = b("    foo\nbar");
        buf.sel = Selection::point(7);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "    foo\n    \nbar");
        assert_eq!(buf.cursor_line_col(), (1, 4));
        // Caret inside the indent: the rest of the line keeps its column.
        let mut buf = b("    foo");
        buf.sel = Selection::point(2);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "\n    foo");
        assert_eq!(buf.cursor_line_col(), (1, 2));
        // A selection is replaced.
        let mut buf = b("  ab cd");
        buf.sel = Selection::new(4, 5);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "  ab\n  cd");
    }

    #[test]
    fn closer_on_blank_line_dedents() {
        let mut buf = b("fn f() {\n    if x {\n        a();\n        ");
        let end = buf.len_chars();
        buf.sel = Selection::point(end);
        let indent = buf.closer_indent('}', None).expect("dedent applies");
        assert_eq!(indent, "    ");
        buf.type_dedented('}', &indent);
        assert_eq!(buf.text(), "fn f() {\n    if x {\n        a();\n    }");
        assert_eq!(buf.cursor_line_col(), (3, 5));
        assert!(buf.undo());
        assert_eq!(buf.text(), "fn f() {\n    if x {\n        a();\n        ");
        // Not on a line with text before the caret, nor at the right indent already.
        let mut buf = b("{\n  a");
        buf.sel = Selection::point(5);
        assert_eq!(buf.closer_indent('}', None), None);
        let mut buf = b("  {\n  ");
        buf.sel = Selection::point(6);
        assert_eq!(buf.closer_indent('}', None), None);
        assert_eq!(buf.closer_indent('x', None), None);
    }

    #[test]
    fn crlf_newline_smart() {
        let mut buf = b("a {}\r\nb");
        assert_eq!(buf.line_ending, LineEnding::CrLf);
        buf.sel = Selection::point(3);
        buf.indent = IndentStyle::spaces(2);
        buf.newline_smart(false);
        assert_eq!(buf.text(), "a {\r\n  \r\n}\r\nb");
        assert_eq!(buf.cursor_line_col(), (1, 2));
        let mut buf = b("x {\r\n    \r\ny");
        buf.sel = Selection::point(9);
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
        buf.sel = Selection::point(caret);
        assert_eq!(grow(&mut buf), "foo");
        assert_eq!(grow(&mut buf), "foo(bar)");
        assert_eq!(grow(&mut buf), "foo(bar);");
        // Block contents before the block, as in JetBrains.
        assert_eq!(grow(&mut buf), "let a = 1;\n    foo(bar);");
        assert_eq!(grow(&mut buf), "{\n    let a = 1;\n    foo(bar);\n}");
        assert_eq!(grow(&mut buf), "fn main() {\n    let a = 1;\n    foo(bar);\n}");
        assert_eq!(grow(&mut buf), src);
        let top = buf.sel;
        buf.expand_selection();
        assert_eq!(buf.sel, top, "nothing larger than the whole text");
        assert_eq!(buf.sel.anchor, 0, "anchor at start, head at end");
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
        assert_eq!(buf.sel, Selection::point(caret));
        buf.shrink_selection();
        assert_eq!(buf.sel, Selection::point(caret), "empty stack is a no-op");
        // Caret right after an identifier starts from it; argument parens
        // get their contents first only when that is larger.
        buf.sel = Selection::point(src.find("bar").unwrap() + 3);
        assert_eq!(grow(&mut buf), "bar");
        assert_eq!(grow(&mut buf), "(bar)");
    }

    #[test]
    fn expand_plain_text_and_stale_stack() {
        let mut buf = b("one two\nthree\n");
        buf.sel = Selection::point(5);
        assert_eq!(grow(&mut buf), "two");
        assert_eq!(grow(&mut buf), "one two\n");
        assert_eq!(grow(&mut buf), "one two\nthree\n");
        buf.shrink_selection();
        assert_eq!(buf.selected_text(), "one two\n");
        // A selection changed outside expand/shrink (e.g. a mouse drag)
        // invalidates the stack instead of jumping back.
        buf.sel = Selection::new(8, 10);
        buf.shrink_selection();
        assert_eq!(buf.sel, Selection::new(8, 10));
        assert!(buf.expand_stack.is_empty());
        // Edits clear it too.
        buf.sel = Selection::point(1);
        buf.expand_selection();
        buf.insert("x", TxKind::Other);
        assert!(buf.expand_stack.is_empty());
        let mut buf = b("");
        buf.expand_selection();
        assert!(buf.expand_stack.is_empty(), "nothing to expand in an empty buffer");
    }

    #[test]
    fn prepare_for_save_trims_except_caret_line() {
        let mut buf = b("a  \nb\t \nc  \nd");
        buf.sel = Selection::point(7);
        buf.prepare_for_save(true, false);
        assert_eq!(buf.text(), "a\nb\t \nc\nd", "caret line 1 keeps its whitespace");
        assert_eq!(buf.sel, Selection::point(5), "caret mapped past the trim on line 0");
        assert!(buf.undo());
        assert_eq!(buf.text(), "a  \nb\t \nc  \nd", "one transaction restores all of it");
        // CRLF: the `\r` of a break is not trailing whitespace.
        let mut buf = b("x \r\ny\r\n");
        buf.sel = Selection::point(8);
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "x\r\ny\r\n");
    }

    #[test]
    fn prepare_for_save_adds_final_newline_once() {
        let mut buf = b("a\nb");
        buf.sel = Selection::point(3);
        buf.prepare_for_save(true, true);
        assert_eq!(buf.text(), "a\nb\n");
        assert_eq!(buf.sel, Selection::point(3), "caret stays before the added break");
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
        buf.bookmarks.iter().copied().collect()
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
    fn line_ending_conversion_is_one_undo_step() {
        let src = "fn a() {\n\n    x\r\n}\rtail\n";
        let mut buf = b(src);
        assert_eq!(buf.line_ending, LineEnding::CrLf);
        buf.set_bookmarks(&[2, 3]);
        // Caret at the end of line 2 ("    x").
        buf.goto_line(2, 5);
        let caret = buf.sel.head;
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
        assert_eq!(buf.sel.head, caret);
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
