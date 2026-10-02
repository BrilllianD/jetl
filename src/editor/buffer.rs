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
        let (l0, _) = selection::pos_to_line_col(&self.rope, r.start);
        let (mut l1, c1) = selection::pos_to_line_col(&self.rope, r.end);
        // A selection ending at column 0 does not include that line.
        if l1 > l0 && c1 == 0 {
            l1 -= 1;
        }
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
        // Map the selection through the changes (applied last line first).
        let map = |mut p: usize, stay_at_insert: bool| {
            for c in &changes {
                let old_n = c.old.chars().count();
                if p < c.at || (p == c.at && stay_at_insert && old_n == 0) {
                    continue;
                }
                p = if p >= c.at + old_n { p - old_n + c.new.chars().count() } else { c.at };
            }
            p
        };
        let forward = self.sel.anchor <= self.sel.head;
        let (start, end) = (map(r.start, !r.is_empty()), map(r.end, false));
        let sel_after = if forward { Selection::new(start, end) } else { Selection::new(end, start) };
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
