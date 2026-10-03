//! Git diff marks shown in the editor gutter: the buffer text against its
//! HEAD version, and the change commands built on them (revert the change
//! at the caret, go to the next or previous change).

use imara_diff::Hunk;
use ratatui::style::Color;

use super::buffer::Buffer;
use super::selection::Selection;
use super::undo::{Change, TxKind};
use crate::config::theme::Theme;

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
            MarkKind::Deleted => ("▸", theme.gutter_deleted),
        }
    }
}

/// Marks for `text` compared with its HEAD version `base`, sorted by line.
/// Pure insertions mark their lines added; replacements mark the new lines
/// modified; pure removals put one deleted mark on the line that follows the
/// removed block (the last line when the block was at the end).
pub fn compute(base: &str, text: &str) -> Vec<GutterMark> {
    let last_line = text.split_inclusive('\n').count().saturating_sub(1);
    let mut out = Vec::new();
    for h in crate::git::diff::hunks(base, text) {
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
    let last_line = text.split_inclusive('\n').count().saturating_sub(1);
    crate::git::diff::hunks(base, text).into_iter().find(|h| {
        let start = mark_line(h, last_line);
        line == start || (h.after.start as usize..h.after.end as usize).contains(&line)
    })
}

/// First mark line of every hunk, in order.
pub fn change_lines(base: &str, text: &str) -> Vec<usize> {
    let last_line = text.split_inclusive('\n').count().saturating_sub(1);
    let mut lines: Vec<usize> = crate::git::diff::hunks(base, text).iter().map(|h| mark_line(h, last_line)).collect();
    lines.dedup();
    lines
}

/// The edit that puts the base lines of hunk `h` back into `text`.
pub fn revert(base: &str, text: &str, h: &Hunk) -> Change {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let base_lines: Vec<&str> = base.split_inclusive('\n').collect();
    let (a0, a1) = (h.after.start as usize, h.after.end as usize);
    let at = lines[..a0].iter().map(|l| l.chars().count()).sum();
    Change::replace(at, lines[a0..a1].concat(), base_lines[h.before.start as usize..h.before.end as usize].concat())
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
        self.view.sel = Selection::point(self.rope.line_to_char(line.min(self.rope.len_lines().saturating_sub(1))));
        self.view.desired_col = None;
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
}
