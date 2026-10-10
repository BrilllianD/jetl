//! Git diff marks shown in the editor gutter: the buffer text against its
//! HEAD version, and the change commands built on them (revert the change
//! at the caret, go to the next or previous change).

use std::ops::Range;

use imara_diff::Hunk;
use ratatui::style::Color;

use super::buffer::Buffer;
use super::selection::Selection;
use super::undo::{Change, TxKind};
use crate::config::theme::Theme;
use crate::util::text::{line_count_inclusive, lines_inclusive};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GutterMark {
    pub line: usize,
    pub kind: MarkKind,
}

impl GutterMark {
    pub fn glyph(&self, theme: &Theme) -> (&'static str, Color) {
        match self.kind {
            MarkKind::Added => ("▎", theme.gutter_added),
            MarkKind::Modified => ("▎", theme.gutter_modified),
            MarkKind::Deleted => ("▔", theme.gutter_deleted),
        }
    }
}

/// Marks for `text` compared with its HEAD version `base`, sorted by line.
/// Pure insertions mark their lines added; replacements mark the new lines
/// modified; pure removals put one deleted mark on the line that follows the
/// removed block (the last line when the block was at the end).
pub fn compute(base: &str, text: &str) -> Vec<GutterMark> {
    let last_line = line_count_inclusive(text).saturating_sub(1);
    let mut out = Vec::new();
    for h in crate::git::diff::rope_hunks(base, text) {
        let (a0, a1) = (h.after.start as usize, h.after.end as usize);
        if h.before.is_empty() {
            out.extend((a0..a1).map(|line| GutterMark { line, kind: MarkKind::Added }));
        } else if h.after.is_empty() {
            let line = a0.min(last_line);
            if out.last().is_none_or(|m: &GutterMark| m.line != line) {
                out.push(GutterMark { line, kind: MarkKind::Deleted });
            }
        } else {
            out.extend((a0..a1).map(|line| GutterMark { line, kind: MarkKind::Modified }));
        }
    }
    out
}

/// The line a hunk's marks start on (see [`compute`]).
fn mark_line(h: &Hunk, last_line: usize) -> usize {
    if h.after.is_empty() { (h.after.start as usize).min(last_line) } else { h.after.start as usize }
}

/// The hunk whose marks cover `line`.
pub fn hunk_at(base: &str, text: &str, line: usize) -> Option<Hunk> {
    let last_line = line_count_inclusive(text).saturating_sub(1);
    crate::git::diff::rope_hunks(base, text).into_iter().find(|h| {
        let start = mark_line(h, last_line);
        line == start || (h.after.start as usize..h.after.end as usize).contains(&line)
    })
}

/// First mark line of every hunk, in order.
pub fn change_lines(base: &str, text: &str) -> Vec<usize> {
    let last_line = line_count_inclusive(text).saturating_sub(1);
    let mut lines: Vec<usize> = crate::git::diff::rope_hunks(base, text).iter().map(|h| mark_line(h, last_line)).collect();
    lines.dedup();
    lines
}

/// The edit that puts the base lines of hunk `h` back into `text`.
pub fn revert(base: &str, text: &str, h: &Hunk) -> Change {
    let lines: Vec<&str> = lines_inclusive(text).collect();
    let base_lines: Vec<&str> = lines_inclusive(base).collect();
    let span = (h.before.start as usize..h.before.end as usize, h.after.start as usize..h.after.end as usize);
    revert_spans(&base_lines, &lines, &[span]).remove(0)
}

/// The edits that put old lines back over new ones: for each `(old, new)`
/// pair of line ranges (ascending, not overlapping; lines keep their
/// terminators) the new lines are replaced by the old ones. Returned last
/// first, so as one transaction each change applies to text the ones
/// before it left untouched above it.
pub fn revert_spans<S: AsRef<str>>(old: &[S], new: &[S], spans: &[(Range<usize>, Range<usize>)]) -> Vec<Change> {
    let concat = |lines: &[S]| lines.iter().map(AsRef::as_ref).collect::<String>();
    let (mut at, mut line) = (0, 0);
    let mut out: Vec<Change> = spans
        .iter()
        .map(|(o, n)| {
            at += new[line..n.start].iter().map(|l| l.as_ref().chars().count()).sum::<usize>();
            line = n.start;
            Change::replace(at, concat(&new[n.clone()]), concat(&old[o.clone()]))
        })
        .collect();
    out.reverse();
    out
}

impl Buffer {
    /// Revert the change at the caret line to the HEAD text, as one undoable
    /// edit. False when the caret is on no change (or there is no base).
    pub fn revert_change_at_caret(&mut self) -> bool {
        let Some(base) = self.diff_base.clone() else { return false };
        let text = self.text();
        let line = self.cursor_line_col().0;
        let Some(h) = hunk_at(&base, &text, line) else { return false };
        let change = revert(&base, &text, &h);
        let at = change.at;
        self.apply(vec![change], Selection::point(at), TxKind::Other);
        true
    }

    /// Move the caret to the start of the next (or previous) change. False
    /// when there is none in that direction.
    pub fn goto_change(&mut self, forward: bool) -> bool {
        let Some(base) = &self.diff_base else { return false };
        let lines = change_lines(base, &self.text());
        let cur = self.cursor_line_col().0;
        let target = if forward { lines.into_iter().find(|&l| l > cur) } else { lines.into_iter().rev().find(|&l| l < cur) };
        let Some(line) = target else { return false };
        self.view.set_sel(Selection::point(self.rope.line_to_char(line.min(self.rope.len_lines().saturating_sub(1)))));
        self.undo.break_group = true;
        true
    }
}

/// The mark on `line`, if any (`marks` sorted by line).
pub fn mark_at(marks: &[GutterMark], line: usize) -> Option<&GutterMark> {
    let i = marks.partition_point(|m| m.line < line);
    marks.get(i).filter(|m| m.line == line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(base: &str, text: &str) -> Vec<(usize, MarkKind)> {
        compute(base, text).into_iter().map(|m| (m.line, m.kind)).collect()
    }

    #[test]
    fn lone_cr_lines_count_like_the_editor() {
        let (base, text) = ("a\rb\rc\r", "a\rX\rc\r");
        assert_eq!(marks(base, text), vec![(1, MarkKind::Modified)]);
        let h = hunk_at(base, text, 1).unwrap();
        let c = revert(base, text, &h);
        assert_eq!((c.at, c.old.as_str(), c.new.as_str()), (2, "X\r", "b\r"));
    }

    #[test]
    fn hunks_map_to_marks() {
        let base = "a\nb\nc\nd\ne\n";
        // Line 1 modified, a line added after c, line e deleted.
        let text = "a\nB\nc\nnew\nd\n";
        assert_eq!(marks(base, text), vec![(1, MarkKind::Modified), (3, MarkKind::Added), (4, MarkKind::Deleted)]);
        // Deleting the first line marks the new first line.
        assert_eq!(marks("x\ny\n", "y\n"), vec![(0, MarkKind::Deleted)]);
        // Deleting a middle line marks the line that moved up.
        assert_eq!(marks("x\ny\nz\n", "x\nz\n"), vec![(1, MarkKind::Deleted)]);
        assert!(marks("same\n", "same\n").is_empty());
        let m = compute(base, text);
        assert_eq!(mark_at(&m, 3).map(|m| m.kind), Some(MarkKind::Added));
        assert!(mark_at(&m, 2).is_none());
    }

    /// `new` with the changes of `blocks` (indices into the diff's blocks)
    /// reverted as one transaction; one undo gives `new` back.
    fn revert_blocks(old: &str, new: &str, blocks: &[usize]) -> String {
        let d = crate::git::diff::DiffDoc::build(old.as_bytes(), new.as_bytes(), 4);
        let spans: Vec<_> = blocks.iter().map(|&i| d.block_lines(i).unwrap()).collect();
        let mut b = Buffer::from_text(1, None, new, false);
        b.apply(revert_spans(&d.old, &d.new, &spans), Selection::point(0), TxKind::Other);
        let out = b.text();
        assert!(b.undo());
        assert_eq!(b.text(), new);
        out
    }

    #[test]
    fn reverting_every_change_gives_the_old_text() {
        let pairs = [
            ("a\nb\nc\n", "A\nb\nC\nD\n"),
            ("a\n", "x\ny\na\n"),
            ("a\nb\nc\n", "a\n"),
            ("x\ny", "x\ny\nz\n"),
            ("x\ny\n", "x\nY"),
            ("one\r\ntwo\r\nthree\r\n", "one\r\n2\r\nthree\r\nfour\r\n"),
            ("", "new\n"),
            ("gone\n", ""),
            ("é\nb\nc\nd\n", "é\nB\nc\nd\nü\n"),
        ];
        for (old, new) in pairs {
            let n = crate::git::diff::DiffDoc::build(old.as_bytes(), new.as_bytes(), 4).blocks(false).len();
            assert_eq!(revert_blocks(old, new, &(0..n).collect::<Vec<_>>()), old, "{old:?} <- {new:?}");
        }
        // One of two: the other stays.
        assert_eq!(revert_blocks("a\nb\nc\n", "A\nb\nC\nD\n", &[1]), "A\nb\nc\n");
        assert_eq!(revert_blocks("a\nb\nc\n", "A\nb\nC\nD\n", &[0]), "a\nb\nC\nD\n");
    }

    #[test]
    fn revert_puts_base_lines_back_and_changes_are_found() {
        let base = "a\nb\nc\nd\ne\n";
        let text = "a\nB\nc\nnew\nd\n";
        assert_eq!(change_lines(base, text), vec![1, 3, 4]);
        let mut b = Buffer::from_text(1, None, text, false);
        b.diff_base = Some(base.into());
        assert!(b.goto_change(true));
        assert_eq!(b.cursor_line_col().0, 1);
        assert!(b.goto_change(true) && b.goto_change(true));
        assert_eq!(b.cursor_line_col().0, 4);
        assert!(!b.goto_change(true), "nothing below the last change");
        assert!(b.goto_change(false));
        assert_eq!(b.cursor_line_col().0, 3);
        // Revert the added line, then the removed "e" (marked on line 3,
        // the last line now), then the modification.
        assert!(b.revert_change_at_caret());
        assert_eq!(b.text(), "a\nB\nc\nd\n");
        b.goto_line(3, 0);
        assert!(b.revert_change_at_caret());
        assert_eq!(b.text(), "a\nB\nc\nd\ne\n");
        b.goto_line(1, 0);
        assert!(b.revert_change_at_caret());
        assert_eq!(b.text(), base);
        b.goto_line(0, 0);
        assert!(!b.revert_change_at_caret(), "no change at the caret");
        // Each revert is one undo step.
        b.undo();
        assert_eq!(b.text(), "a\nB\nc\nd\ne\n");
    }

    #[test]
    fn marks_line_up_with_buffer_lines_across_separators() {
        let base = "a\u{c}b\nc\nd\u{2028}e\nf\n";
        let text = "a\u{c}b\nC\nd\u{2028}e\nF\n";
        assert_eq!(marks(base, text), vec![(1, MarkKind::Modified), (3, MarkKind::Modified)]);
        let r = ropey::Rope::from_str(text);
        assert_eq!(r.line(1).to_string(), "C\n");
        assert_eq!(r.line(3).to_string(), "F\n");
    }
}
