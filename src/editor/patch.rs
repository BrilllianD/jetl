//! Patch tabs (Git Log Show Diff): the added and removed lines of a unified
//! diff text, for line backgrounds and next/previous change.

use std::ops::Range;

use super::view::{DecorLine, LineDecor};
use crate::config::theme::Theme;
use crate::util::text::lines_inclusive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchLine {
    Added,
    Removed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patch {
    /// Runs of consecutive added or removed lines, sorted.
    runs: Vec<(Range<usize>, PatchLine)>,
    /// First line of every block of consecutive changed lines.
    starts: Vec<usize>,
    /// The diff spans the text was built from: the identity of the tab.
    pub spans: Vec<crate::git::model::DiffSpan>,
}

impl Patch {
    /// Lines inside a hunk (after `@@`, until the next `diff ` or `== `
    /// line) starting with `+` or `-` are changes.
    pub fn parse(text: &str) -> Patch {
        let mut p = Patch::default();
        let mut in_hunk = false;
        let mut prev_changed = false;
        for (i, line) in lines_inclusive(text).enumerate() {
            if line.starts_with("@@") {
                in_hunk = true;
            } else if line.starts_with("diff ") || line.starts_with("== ") {
                in_hunk = false;
            }
            let kind = match line.as_bytes().first() {
                Some(b'+') if in_hunk => Some(PatchLine::Added),
                Some(b'-') if in_hunk => Some(PatchLine::Removed),
                _ => None,
            };
            let Some(kind) = kind else {
                prev_changed = false;
                continue;
            };
            if !prev_changed {
                p.starts.push(i);
            }
            prev_changed = true;
            match p.runs.last_mut() {
                Some((r, k)) if *k == kind && r.end == i => r.end = i + 1,
                _ => p.runs.push((i..i + 1, kind)),
            }
        }
        p
    }

    pub fn change_starts(&self) -> &[usize] {
        &self.starts
    }

    pub fn decor(&self, theme: &Theme) -> LineDecor {
        let lines = self
            .runs
            .iter()
            .map(|(r, k)| DecorLine {
                lines: r.clone(),
                bg: Some(match k {
                    PatchLine::Added => theme.diff_add_bg,
                    PatchLine::Removed => theme.diff_del_bg,
                }),
                glyphs: String::new(),
            })
            .collect();
        LineDecor { action_w: 0, lines }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_tabs_are_reused_by_spans_not_by_title() {
        use crate::git::model::DiffSpan;
        let span = |top: &str| vec![DiffSpan { base: None, top: top.into(), header: format!("== {top}") }];
        let mut ed = crate::editor::Editor::new(&crate::config::Config::default());
        let a = ed.open_patch("Diff of 2 commits", span("aaaa"), "@@ -1 +1 @@\n-a\n+a2\n");
        let b = ed.open_patch("Diff of 2 commits", span("bbbb"), "@@ -1 +1 @@\n-b\n+b2\n");
        assert_ne!(a, b);
        assert_eq!(ed.open_patch("Diff of 2 commits", span("aaaa"), "ignored"), a);
        assert_eq!(ed.open_patch("Diff of 2 commits", span("bbbb"), "ignored"), b);
        assert!(ed.buffers[a].text().contains("+a2") && ed.buffers[b].text().contains("+b2"));
        assert_eq!(ed.buffers.len(), 2);
    }

    #[test]
    fn file_headers_are_not_changes_but_dashes_in_a_hunk_are() {
        let p = Patch::parse("== a1 s\ndiff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,1 @@\n ctx\n-- comment\n");
        assert_eq!(p.change_starts(), [6]);
        assert_eq!(p.runs, [(6..7, PatchLine::Removed)]);
    }

    #[test]
    fn two_hunks_give_two_starts() {
        let p = Patch::parse("diff x\n@@ -1 +1 @@\n-a\n+b\n c\n@@ -9 +9 @@\n+d\n");
        assert_eq!(p.change_starts(), [2, 6]);
    }

    #[test]
    fn a_replaced_line_is_one_block_of_two_runs() {
        let p = Patch::parse("@@ -1 +1 @@\n-x\n+y\n");
        assert_eq!(p.change_starts(), [1]);
        assert_eq!(p.runs, [(1..2, PatchLine::Removed), (2..3, PatchLine::Added)]);
    }

    #[test]
    fn a_new_section_ends_the_hunk() {
        let p = Patch::parse("@@ -1 +1 @@\n+a\n\n== b\n--- not a change\n");
        assert_eq!(p.change_starts(), [1]);
    }
}
