//! Git diff marks shown in the editor gutter: the buffer text against its
//! HEAD version.

use ratatui::style::Color;

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
}
