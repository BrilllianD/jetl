//! Line diffs for the diff view (side-by-side and unified rows) and the
//! editor gutter, computed with imara-diff's histogram algorithm, plus a
//! second pass on the characters of each modified line pair.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, Hunk, InternedInput};

/// Bytes that look like a binary file (NUL in the first 8000 bytes, as git
/// decides).
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// Hunks of a line diff between `old` and `new`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    diff.hunks().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Equal,
    /// Only on the left.
    Removed,
    /// Only on the right.
    Added,
    /// Both sides, changed.
    Modified,
}

/// One side-by-side row: line indexes into the old and new texts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub left: Option<usize>,
    pub right: Option<usize>,
}

/// Align two texts into side-by-side rows. Inside a hunk the first
/// `min(removed, added)` lines pair up as modified; the rest stay on their
/// own side with a blank filler opposite.
pub fn side_by_side(old: &str, new: &str) -> Vec<DiffRow> {
    let n_old = line_count(old);
    let n_new = line_count(new);
    let mut rows = Vec::with_capacity(n_old.max(n_new));
    let (mut o, mut n) = (0usize, 0usize);
    let equal_until = |rows: &mut Vec<DiffRow>, o: &mut usize, n: &mut usize, o_end: usize| {
        while *o < o_end {
            rows.push(DiffRow { kind: RowKind::Equal, left: Some(*o), right: Some(*n) });
            *o += 1;
            *n += 1;
        }
    };
    for h in hunks(old, new) {
        equal_until(&mut rows, &mut o, &mut n, h.before.start as usize);
        let removed = (h.before.end - h.before.start) as usize;
        let added = (h.after.end - h.after.start) as usize;
        for i in 0..removed.max(added) {
            let left = (i < removed).then_some(h.before.start as usize + i);
            let right = (i < added).then_some(h.after.start as usize + i);
            let kind = match (left, right) {
                (Some(_), Some(_)) => RowKind::Modified,
                (Some(_), None) => RowKind::Removed,
                _ => RowKind::Added,
            };
            rows.push(DiffRow { kind, left, right });
        }
        o = h.before.end as usize;
        n = h.after.end as usize;
    }
    equal_until(&mut rows, &mut o, &mut n, n_old);
    rows
}

/// Unified rows derived from side-by-side rows: each changed block lists
/// its old lines first, then its new lines. A row has exactly one side set
/// unless it is `Equal`.
pub fn unified(rows: &[DiffRow]) -> Vec<DiffRow> {
    let mut out = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind == RowKind::Equal {
            out.push(rows[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && rows[i].kind != RowKind::Equal {
            i += 1;
        }
        let block = &rows[start..i];
        out.extend(block.iter().filter_map(|r| r.left).map(|l| DiffRow { kind: RowKind::Removed, left: Some(l), right: None }));
        out.extend(block.iter().filter_map(|r| r.right).map(|r| DiffRow { kind: RowKind::Added, left: None, right: Some(r) }));
    }
    out
}

/// Runs of consecutive non-equal rows.
pub fn blocks(rows: &[DiffRow]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind == RowKind::Equal {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && rows[i].kind != RowKind::Equal {
            i += 1;
        }
        out.push(start..i);
    }
    out
}

/// Number of lines the way imara-diff tokenizes them (a trailing newline
/// does not start a new line).
fn line_count(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.split_inclusive('\n').count() }
}

/// A line without its terminator.
pub fn strip_eol(line: &str) -> &str {
    let l = line.strip_suffix('\n').unwrap_or(line);
    l.strip_suffix('\r').unwrap_or(l)
}

/// Split into lines that keep their terminators (the way imara-diff and
/// patches count lines).
fn raw_lines(s: &str) -> Vec<String> {
    s.split_inclusive('\n').map(str::to_string).collect()
}

/// Lines longer than this get no character diff.
const INLINE_MAX_CHARS: usize = 2000;

/// Changed char ranges of a modified line pair (old side, new side), from
/// a diff on characters. Empty when the lines share too little for the
/// highlight to help, or are very long.
pub fn inline_changes(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (n_old, n_new) = (old.chars().count(), new.chars().count());
    if n_old > INLINE_MAX_CHARS || n_new > INLINE_MAX_CHARS {
        return Default::default();
    }
    let mut input = InternedInput::default();
    input.update_before(old.chars());
    input.update_after(new.chars());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_no_heuristic(&input);
    let (mut before, mut after) = (Vec::new(), Vec::new());
    for h in diff.hunks() {
        if !h.before.is_empty() {
            before.push(h.before.start as usize..h.before.end as usize);
        }
        if !h.after.is_empty() {
            after.push(h.after.start as usize..h.after.end as usize);
        }
    }
    let removed: usize = before.iter().map(|r| r.len()).sum();
    let common = n_old - removed;
    if common * 3 < n_old.max(n_new) {
        return Default::default();
    }
    (before, after)
}

/// A computed diff ready for the view.
#[derive(Debug, Clone, Default)]
pub struct DiffDoc {
    /// Lines with their terminators, as in the files.
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub rows: Vec<DiffRow>,
    pub unified: Vec<DiffRow>,
    /// Changed char ranges (without the terminator) per old and new line;
    /// empty except on modified lines.
    pub inline_old: Vec<Vec<Range<usize>>>,
    pub inline_new: Vec<Vec<Range<usize>>>,
    pub tab_width: usize,
    pub binary: bool,
    /// A side was not valid UTF-8 (shown with replacement characters), so
    /// its lines are not the file's text.
    pub lossy: bool,
}

impl DiffDoc {
    pub fn build(old: &[u8], new: &[u8], tab_width: usize) -> Self {
        if is_binary(old) || is_binary(new) {
            return Self { binary: true, ..Default::default() };
        }
        let lossy = std::str::from_utf8(old).is_err() || std::str::from_utf8(new).is_err();
        let (old, new) = (raw_lines(&String::from_utf8_lossy(old)), raw_lines(&String::from_utf8_lossy(new)));
        let rows = side_by_side(&old.concat(), &new.concat());
        let unified = unified(&rows);
        let (mut inline_old, mut inline_new) = (vec![Vec::new(); old.len()], vec![Vec::new(); new.len()]);
        for r in rows.iter().filter(|r| r.kind == RowKind::Modified) {
            if let (Some(l), Some(n)) = (r.left, r.right) {
                (inline_old[l], inline_new[n]) = inline_changes(strip_eol(&old[l]), strip_eol(&new[n]));
            }
        }
        Self { old, new, rows, unified, inline_old, inline_new, tab_width, binary: false, lossy }
    }

    /// The old and new line ranges of changed block `block` (the same block
    /// in both layouts). A pure insertion has an empty old range where its
    /// lines would go, a pure deletion an empty new range.
    pub fn block_lines(&self, block: usize) -> Option<(Range<usize>, Range<usize>)> {
        let b = blocks(&self.rows).into_iter().nth(block)?;
        let count = |rows: &[DiffRow], left: bool| rows.iter().filter(|r| if left { r.left.is_some() } else { r.right.is_some() }).count();
        let side = |left: bool| {
            let start = count(&self.rows[..b.start], left);
            start..start + count(&self.rows[b.clone()], left)
        };
        Some((side(true), side(false)))
    }

    /// Changed blocks (runs of non-equal rows) of the side-by-side or
    /// unified rows; both views have the same blocks in the same order.
    pub fn blocks(&self, unified: bool) -> Vec<std::ops::Range<usize>> {
        blocks(if unified { &self.unified } else { &self.rows })
    }

    /// A unified patch of changed block `block` with up to `context` equal
    /// lines around it, for `git apply` (`a/old_path` to `b/new_path`).
    pub fn block_patch(&self, block: usize, old_path: &str, new_path: &str, context: usize) -> Option<String> {
        let rows = &self.rows;
        let b = blocks(rows).into_iter().nth(block)?;
        let equal = |i: usize| rows[i].kind == RowKind::Equal;
        let mut start = b.start;
        while start > 0 && b.start - start < context && equal(start - 1) {
            start -= 1;
        }
        let mut end = b.end;
        while end < rows.len() && end - b.end < context && equal(end) {
            end += 1;
        }
        let count = |range: std::ops::Range<usize>, left: bool| {
            rows[range].iter().filter(|r| if left { r.left.is_some() } else { r.right.is_some() }).count()
        };
        // A side without lines starts at the line before (git's convention).
        let header = |left: bool| {
            let (before, n) = (count(0..start, left), count(start..end, left));
            format!("{},{n}", if n == 0 { before } else { before + 1 })
        };
        let mut out = format!("diff --git a/{old_path} b/{new_path}\n--- a/{old_path}\n+++ b/{new_path}\n");
        out.push_str(&format!("@@ -{} +{} @@\n", header(true), header(false)));
        let mut line = |sign: char, text: &str| {
            out.push(sign);
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        };
        for r in &rows[start..b.start] {
            line(' ', &self.old[r.left?]);
        }
        for r in &rows[b.clone()] {
            if let Some(l) = r.left {
                line('-', &self.old[l]);
            }
        }
        for r in &rows[b.clone()] {
            if let Some(l) = r.right {
                line('+', &self.new[l]);
            }
        }
        for r in &rows[b.end..end] {
            line(' ', &self.old[r.left?]);
        }
        Some(out)
    }

    /// Index of the first changed row (side-by-side or unified).
    pub fn first_change(&self, unified: bool) -> Option<usize> {
        let rows = if unified { &self.unified } else { &self.rows };
        rows.iter().position(|r| r.kind != RowKind::Equal)
    }

    pub fn is_identical(&self) -> bool {
        !self.binary && self.rows.iter().all(|r| r.kind == RowKind::Equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(rows: &[DiffRow]) -> Vec<(RowKind, Option<usize>, Option<usize>)> {
        rows.iter().map(|r| (r.kind, r.left, r.right)).collect()
    }

    #[test]
    fn side_by_side_pairs_and_fills() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nx\ny\nd\n";
        use RowKind::*;
        assert_eq!(
            kinds(&side_by_side(old, new)),
            vec![
                (Equal, Some(0), Some(0)),
                (Modified, Some(1), Some(1)),
                (Equal, Some(2), Some(2)),
                (Added, None, Some(3)),
                (Added, None, Some(4)),
                (Equal, Some(3), Some(5)),
            ]
        );
        // Removal with more old than new lines leaves fillers on the right.
        let rows = side_by_side("a\nb\nc\nd\n", "a\nX\nd\n");
        assert_eq!(
            kinds(&rows),
            vec![(Equal, Some(0), Some(0)), (Modified, Some(1), Some(1)), (Removed, Some(2), None), (Equal, Some(3), Some(2))]
        );
        // Every old and new line appears exactly once, in order.
        let lefts: Vec<usize> = rows.iter().filter_map(|r| r.left).collect();
        let rights: Vec<usize> = rows.iter().filter_map(|r| r.right).collect();
        assert_eq!(lefts, vec![0, 1, 2, 3]);
        assert_eq!(rights, vec![0, 1, 2]);
    }

    #[test]
    fn empty_sides() {
        use RowKind::*;
        assert_eq!(kinds(&side_by_side("", "x\ny")), vec![(Added, None, Some(0)), (Added, None, Some(1))]);
        assert_eq!(kinds(&side_by_side("x\n", "")), vec![(Removed, Some(0), None)]);
        assert!(side_by_side("", "").is_empty());
    }

    #[test]
    fn unified_lists_old_then_new() {
        use RowKind::*;
        let rows = side_by_side("a\nb\nc\n", "a\nB\nC\nD\n");
        assert_eq!(
            kinds(&unified(&rows)),
            vec![
                (Equal, Some(0), Some(0)),
                (Removed, Some(1), None),
                (Removed, Some(2), None),
                (Added, None, Some(1)),
                (Added, None, Some(2)),
                (Added, None, Some(3))
            ]
        );
    }

    #[test]
    fn binary_and_raw_lines() {
        assert!(DiffDoc::build(b"a\0b", b"", 4).binary);
        let d = DiffDoc::build(b"\tx\r\n", b"\tx\r\ny\r\n", 4);
        assert_eq!(d.old, vec!["\tx\r\n"]);
        assert_eq!(d.new, vec!["\tx\r\n", "y\r\n"]);
        assert_eq!(d.first_change(false), Some(1));
        assert!(DiffDoc::build(b"same\n", b"same\n", 4).is_identical());
    }

    #[test]
    fn modified_lines_get_changed_characters() {
        let d = DiffDoc::build(b"let x = foo(1);\nsame\n", b"let y = foo(2);\nsame\n", 4);
        assert_eq!(d.inline_old[0], vec![4..5, 12..13]);
        assert_eq!(d.inline_new[0], vec![4..5, 12..13]);
        assert!(d.inline_old[1].is_empty());
        // Lines with almost nothing in common are not broken up.
        assert_eq!(inline_changes("abcdef", "uvwxyz"), (vec![], vec![]));
        // An insertion only marks the new side.
        let (old, new) = inline_changes("ab", "aXb");
        assert!(old.is_empty());
        assert_eq!(new, vec![Range { start: 1, end: 2 }]);
    }

    #[test]
    fn block_lines_map_changes_to_both_sides() {
        let lines = |old: &str, new: &str| {
            let d = DiffDoc::build(old.as_bytes(), new.as_bytes(), 4);
            (0..d.blocks(false).len()).map(|i| d.block_lines(i).unwrap()).collect::<Vec<_>>()
        };
        // A modified first line.
        assert_eq!(lines("a\nb\n", "A\nb\n"), [(0..1, 0..1)]);
        // A pure insertion at the top: an empty old range at 0.
        assert_eq!(lines("a\n", "x\ny\na\n"), [(0..0, 0..2)]);
        // A pure deletion at the end: an empty new range after the last line.
        assert_eq!(lines("a\nb\nc\n", "a\n"), [(1..3, 1..1)]);
        // The last line without a newline, on either side.
        assert_eq!(lines("x\ny", "x\ny\nz\n"), [(1..2, 1..3)]);
        assert_eq!(lines("x\ny\n", "x\nY"), [(1..2, 1..2)]);
        // Two changes one equal line apart stay two blocks.
        assert_eq!(lines("a\nb\nc\n", "A\nb\nC\nD\n"), [(0..1, 0..1), (2..3, 2..4)]);
        let d = DiffDoc::build(b"a\n", b"b\n", 4);
        assert!(d.block_lines(1).is_none());
        assert!(!d.lossy);
        assert!(DiffDoc::build(b"caf\xe9\n", b"cafe\n", 4).lossy);
    }

    #[test]
    fn block_patches_carry_context_and_counts() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\n2\nthree\n4\n5\n6\n7\n8\n9\n10\neleven";
        let d = DiffDoc::build(old.as_bytes(), new.as_bytes(), 4);
        assert_eq!(d.blocks(false).len(), 2);
        assert_eq!(d.blocks(true).len(), 2);
        let p = d.block_patch(0, "f.txt", "f.txt", 3).unwrap();
        assert_eq!(p, "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,6 +1,6 @@\n 1\n 2\n-3\n+three\n 4\n 5\n 6\n");
        // An addition at the end without a final newline.
        let p = d.block_patch(1, "f.txt", "f.txt", 3).unwrap();
        assert_eq!(
            p,
            "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -8,3 +8,4 @@\n 8\n 9\n 10\n+eleven\n\\ No newline at end of file\n"
        );
        assert!(d.block_patch(2, "f.txt", "f.txt", 3).is_none());
        // Insertion at the very top: the old side starts at line 0.
        let d = DiffDoc::build(b"a\n", b"new\na\n", 4);
        assert!(d.block_patch(0, "f", "f", 0).unwrap().contains("@@ -0,0 +1,1 @@\n+new\n"));
    }
}
