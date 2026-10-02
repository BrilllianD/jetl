//! A text buffer: rope, selection, undo history, file metadata.
//! Every mutation goes through [`Buffer::apply`].

use std::path::PathBuf;
use std::time::Instant;

use ropey::Rope;

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
    pub gutter: Vec<super::gutter::GutterMark>,
    /// Buffer version the gutter marks were computed for.
    pub gutter_version: u64,
    /// HEAD version of the file for gutter marks; `None` for untracked
    /// files, files outside a repository, or before the blob arrives.
    pub diff_base: Option<String>,
    pub search: Option<super::search::SearchState>,
    /// Caret, version and text area size when the view last followed the
    /// caret. The view only scrolls to the caret when one of them changes,
    /// so wheel scrolling is not undone on the next frame.
    pub follow_key: Option<(usize, u64, u16, u16)>,
}

impl Buffer {
    pub fn from_text(id: BufferId, path: Option<PathBuf>, text: &str, lossy: bool) -> Self {
        let line_ending = if text.contains("\r\n") { LineEnding::CrLf } else { LineEnding::Lf };
        let name = path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "untitled".into());
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
            gutter: Vec::new(),
            gutter_version: u64::MAX,
            diff_base: None,
            search: None,
            follow_key: None,
        }
    }

    pub fn dirty(&self) -> bool {
        self.undo.state_id() != self.saved_state
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
        self.undo.push(Transaction { id: 0, changes, sel_before, sel_after, at: Instant::now(), kind });
    }

    fn apply_change(&mut self, c: &Change) {
        let old_len = c.old.chars().count();
        let at = c.at.min(self.rope.len_chars());
        if old_len > 0 {
            let end = (at + old_len).min(self.rope.len_chars());
            self.rope.remove(at..end);
        }
        if !c.new.is_empty() {
            self.rope.insert(at, &c.new);
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

    /// Newline keeping the current line's indentation.
    pub fn newline(&mut self) {
        let (line, col) = self.cursor_line_col();
        let indent_n = selection::indent_of(&self.rope, line).min(col);
        let indent: String = self.rope.line(line).chars().take(indent_n).collect();
        let s = format!("{}{}", self.line_ending.as_str(), indent);
        self.insert(&s, TxKind::Other);
    }

    pub fn insert_tab(&mut self, tab_width: usize) {
        let (_, col) = self.cursor_line_col();
        let n = tab_width - (col % tab_width);
        self.insert(&" ".repeat(n), TxKind::Typing);
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
            if line + 1 < rope.len_lines() {
                Selection::point(selection::line_col_to_pos(rope, line + 1, col))
            } else {
                sel_after
            }
        });
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

    /// Indent every non-blank selected line by `tab_width` spaces.
    pub fn indent_lines(&mut self, tab_width: usize) {
        let (l0, l1) = self.selected_lines();
        let pad = " ".repeat(tab_width);
        let changes: Vec<Change> = (l0..=l1)
            .rev()
            .filter(|&l| selection::indent_of(&self.rope, l) < line_len_no_eol(&self.rope, l))
            .map(|l| Change::insert(self.rope.line_to_char(l), pad.clone()))
            .collect();
        let sel_after = self.sel_through(&changes);
        self.apply(changes, sel_after, TxKind::Other);
    }

    /// Remove one level of indentation (a tab or up to `tab_width` spaces)
    /// from the selected lines, or the caret line.
    pub fn unindent_lines(&mut self, tab_width: usize) {
        let (l0, l1) = self.selected_lines();
        let mut changes = Vec::new();
        for l in (l0..=l1).rev() {
            let ls = self.rope.line_to_char(l);
            let lead: String = self.rope.line(l).chars().take_while(|c| *c == ' ' || *c == '\t').take(tab_width.max(1)).collect();
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
        let sel_after = if self.sel.is_empty() { Selection::point(self.sel.head.min(r.start + n)) } else { Selection::new(r.start, r.start + n) };
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
                _ => buf.newline(),
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
        for i in 1..=steps {
            assert!(buf.redo());
            assert_eq!(buf.text(), snapshots[i], "redo step {i}");
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
        let ops: [(&str, Op); 4] = [
            ("delete_line", |b| b.delete_line()),
            ("duplicate_line", |b| b.duplicate_line()),
            ("duplicate_last_line", |b| {
                b.goto_line(2, 1);
                b.duplicate_line();
            }),
            ("toggle_comment", |b| b.toggle_line_comment("//")),
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
        buf.indent_lines(4);
        assert_eq!(buf.text(), "fn a() {\n        x;\n\n    \ty;\n}\n", "blank line untouched");
        assert_eq!(buf.sel.range(), buf.rope.line_to_char(1)..buf.rope.line_to_char(4), "selection covers the same lines");
        assert_eq!(buf.undo.len(), 1);
        buf.unindent_lines(4);
        assert_eq!(buf.text(), original);
        buf.unindent_lines(4);
        assert_eq!(buf.text(), "fn a() {\nx;\n\ny;\n}\n", "a leading tab is one level");
        assert_eq!(buf.undo.len(), 3);
        assert!(buf.undo());
        assert_eq!(buf.text(), original);
    }

    #[test]
    fn unindent_caret_line_keeps_caret_on_text() {
        let mut buf = b("      abc\n");
        buf.goto_line(0, 7);
        buf.unindent_lines(4);
        assert_eq!(buf.text(), "  abc\n");
        assert_eq!(buf.cursor_line_col(), (0, 3));
        buf.unindent_lines(4);
        assert_eq!(buf.text(), "abc\n", "fewer spaces than a level");
        assert_eq!(buf.cursor_line_col(), (0, 1));
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
}
