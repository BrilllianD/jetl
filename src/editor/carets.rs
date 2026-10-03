//! Commands that add and remove carets: occurrences of the selected text
//! (Alt+J, Alt+Shift+J, Ctrl+Alt+Shift+J) and clones above and below.

use super::buffer::Buffer;
use super::search::{occurrence_target, occurrences};
use super::selection::{self, MAX_CARETS, Selection, Selections, word_at};

impl Buffer {
    /// Alt+J. With an empty primary caret: select the word at every caret
    /// (later presses then match whole words only). Otherwise select the
    /// next occurrence of the primary selection's text after it, wrapping
    /// at the end and skipping text already selected; it becomes the
    /// primary. False when there is none left.
    pub fn add_next_occurrence(&mut self) -> bool {
        let primary = self.view.sel();
        if primary.is_empty() {
            let mut any = false;
            let ranges = self
                .view
                .sels
                .iter()
                .map(|s| match occurrence_target(&self.rope, s).filter(|_| s.is_empty()) {
                    Some(_) => {
                        any = true;
                        let at = if s.head < self.rope.len_chars() && selection::is_word_char(self.rope.char(s.head)) {
                            s.head
                        } else {
                            s.head - 1
                        };
                        let r = word_at(&self.rope, at);
                        Selection::new(r.start, r.end)
                    }
                    None => *s,
                })
                .collect();
            if any {
                self.view.sels = Selections::new(ranges, self.view.sels.primary_index());
                self.view.occ_whole_word = true;
                self.undo.break_group = true;
            }
            return true;
        }
        let target = self.rope.slice(primary.range()).to_string();
        let all = occurrences(&self.rope, 0..self.rope.len_lines(), &target, self.view.occ_whole_word);
        let taken = |r: &std::ops::Range<usize>| self.view.sels.iter().any(|s| s.range().start < r.end && r.start < s.range().end);
        let from = all.partition_point(|r| r.start < primary.range().end);
        let Some(next) = all[from..].iter().chain(&all[..from]).find(|r| !taken(r)) else { return false };
        let sel = Selection::new(next.start, next.end);
        if !self.view.sels.push(sel, true) {
            return false;
        }
        self.view.occ_stack.push(sel);
        self.undo.break_group = true;
        true
    }

    /// Alt+Shift+J: drop the selection the last Alt+J added; the one added
    /// before it becomes the primary again.
    pub fn unselect_occurrence(&mut self) {
        let Some(last) = self.view.occ_stack.pop() else { return };
        let Some(i) = self.view.sels.iter().position(|s| *s == last) else {
            self.view.occ_stack.clear();
            return;
        };
        self.view.sels.remove(i);
        let before = self.view.occ_stack.last().copied();
        if let Some(j) = before.and_then(|b| self.view.sels.iter().position(|s| *s == b)) {
            self.view.sels.set_primary(j);
        }
        self.undo.break_group = true;
    }

    /// Ctrl+Alt+Shift+J: select every occurrence of the primary selection's
    /// text (or the word at the caret, whole words only), or every match of
    /// the open find bar. Returns how many there were when more than
    /// [`MAX_CARETS`] (only that many are selected).
    pub fn select_all_occurrences(&mut self) -> Option<usize> {
        let primary = self.view.sel();
        let all = match self.view.search.as_mut() {
            Some(s) => {
                s.update(&self.rope, self.version);
                s.matches.clone()
            }
            None => {
                let whole = self.view.occ_whole_word || primary.is_empty();
                let (target, _) = occurrence_target(&self.rope, &primary)?;
                occurrences(&self.rope, 0..self.rope.len_lines(), &target, whole)
            }
        };
        if all.is_empty() {
            return None;
        }
        let total = all.len();
        // The one at the primary caret stays primary.
        let at = all.partition_point(|r| r.end < primary.head).min(total - 1);
        let start = at.saturating_sub(MAX_CARETS / 2).min(total.saturating_sub(MAX_CARETS));
        let kept = &all[start..(start + MAX_CARETS).min(total)];
        let ranges = kept.iter().map(|r| Selection::new(r.start, r.end)).collect();
        self.view.sels = Selections::new(ranges, at - start);
        self.view.occ_stack.clear();
        self.view.occ_whole_word = primary.is_empty();
        self.undo.break_group = true;
        (total > MAX_CARETS).then_some(total)
    }

    /// Add a caret on the line above the topmost caret (`up`) or below the
    /// bottommost one, at the same column (kept across short lines, as Up
    /// and Down do). The new caret is the primary.
    pub fn clone_caret(&mut self, up: bool) {
        let s = if up { self.view.sels.as_slice()[0] } else { self.view.sels.as_slice()[self.view.sels.len() - 1] };
        let i = if up { 0 } else { self.view.sels.len() - 1 };
        let (line, col) = selection::pos_to_line_col(&self.rope, s.head);
        let col = self.view.sels.desired_at(i).unwrap_or(col);
        let target = if up { line.checked_sub(1) } else { (line + 1 < self.rope.len_lines()).then_some(line + 1) };
        let Some(target) = target else { return };
        let p = selection::line_col_to_pos(&self.rope, target, col);
        if self.view.sels.push(Selection::point(p), true) {
            self.view.sels.set_primary_desired(Some(col));
            self.undo.break_group = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(text: &str) -> Buffer {
        Buffer::from_text(1, None, text, false)
    }

    fn texts(b: &Buffer) -> Vec<String> {
        b.view.sels.iter().map(|s| b.rope.slice(s.range()).to_string()).collect()
    }

    #[test]
    fn add_next_occurrence_selects_word_then_next_with_wrap() {
        let mut buf = b("foo bar foo_x foo\nfoo");
        buf.view.set_sel(Selection::point(9));
        assert!(buf.add_next_occurrence());
        assert_eq!(texts(&buf), ["foo_x"], "the word at the caret");
        let mut buf = b("foo bar foo_x foo\nfoo");
        buf.view.set_sel(Selection::point(14));
        assert!(buf.add_next_occurrence());
        assert_eq!(buf.view.sel(), Selection::new(14, 17));
        // Whole words only: foo_x is skipped; after the end it wraps.
        assert!(buf.add_next_occurrence());
        assert_eq!(buf.view.sel(), Selection::new(18, 21));
        assert!(buf.add_next_occurrence());
        assert_eq!(buf.view.sel(), Selection::new(0, 3));
        assert!(!buf.add_next_occurrence(), "no more");
        assert_eq!(buf.view.sels.len(), 3);
        buf.unselect_occurrence();
        assert_eq!(buf.view.sels.len(), 2);
        assert_eq!(buf.view.sel(), Selection::new(18, 21), "the one before is primary again");
        // A selection made by hand matches anywhere, case-sensitively.
        let mut buf = b("ab xab AB ab");
        buf.view.set_sel(Selection::new(0, 2));
        assert!(buf.add_next_occurrence());
        assert!(buf.add_next_occurrence());
        assert_eq!(buf.view.sels.as_slice(), [Selection::new(0, 2), Selection::new(4, 6), Selection::new(10, 12)]);
    }

    #[test]
    fn select_all_occurrences_and_clone_carets() {
        let mut buf = b("x = x + xx;\nx");
        buf.view.set_sel(Selection::point(4));
        assert_eq!(buf.select_all_occurrences(), None);
        assert_eq!(buf.view.sels.heads().collect::<Vec<_>>(), [1, 5, 13]);
        assert_eq!(buf.view.sel(), Selection::new(4, 5), "the caret's occurrence stays primary");
        let mut buf = b("abcdef\nab\nabcdef\n");
        buf.view.set_sel(Selection::point(4));
        buf.clone_caret(false);
        buf.clone_caret(false);
        assert_eq!(buf.view.sels.heads().collect::<Vec<_>>(), [4, 9, 14], "the column survives the short line");
        buf.clone_caret(true);
        assert_eq!(buf.view.sels.len(), 3, "nothing above the first line");
        let many = "a ".repeat(MAX_CARETS + 10);
        let mut buf = b(&many);
        assert_eq!(buf.select_all_occurrences(), Some(MAX_CARETS + 10));
        assert_eq!(buf.view.sels.len(), MAX_CARETS);
    }
}
