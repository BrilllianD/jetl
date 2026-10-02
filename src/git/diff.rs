//! Line diffs for the diff view (side-by-side and unified rows) and the
//! editor gutter, computed with imara-diff's histogram algorithm.

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

/// Number of lines the way imara-diff tokenizes them (a trailing newline
/// does not start a new line).
fn line_count(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.split_inclusive('\n').count() }
}

/// Split into display lines without line terminators, tabs expanded.
pub fn display_lines(s: &str, tab_width: usize) -> Vec<String> {
    s.split_inclusive('\n')
        .map(|l| {
            let l = l.strip_suffix('\n').unwrap_or(l);
            let l = l.strip_suffix('\r').unwrap_or(l);
            expand_tabs(l, tab_width)
        })
        .collect()
}

fn expand_tabs(s: &str, tab_width: usize) -> String {
    if !s.contains('\t') && !s.chars().any(|c| c.is_control()) {
        return s.to_string();
    }
    let tw = tab_width.max(1);
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for c in s.chars() {
        if c == '\t' {
            let n = tw - col % tw;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else if c.is_control() {
            out.push('?');
            col += 1;
        } else {
            out.push(c);
            col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    out
}

/// A computed diff ready for the view.
#[derive(Debug, Clone, Default)]
pub struct DiffDoc {
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub rows: Vec<DiffRow>,
    pub unified: Vec<DiffRow>,
    pub binary: bool,
}

impl DiffDoc {
    pub fn build(old: &[u8], new: &[u8], tab_width: usize) -> Self {
        if is_binary(old) || is_binary(new) {
            return Self { binary: true, ..Default::default() };
        }
        let old_s = String::from_utf8_lossy(old);
        let new_s = String::from_utf8_lossy(new);
        let rows = side_by_side(&old_s, &new_s);
        let unified = unified(&rows);
        Self { old: display_lines(&old_s, tab_width), new: display_lines(&new_s, tab_width), rows, unified, binary: false }
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
    fn binary_and_display_lines() {
        assert!(DiffDoc::build(b"a\0b", b"", 4).binary);
        let d = DiffDoc::build(b"\tx\r\n", b"\tx\r\ny\r\n", 4);
        assert_eq!(d.old, vec!["    x"]);
        assert_eq!(d.new, vec!["    x", "y"]);
        assert_eq!(d.first_change(false), Some(1));
        assert!(DiffDoc::build(b"same\n", b"same\n", 4).is_identical());
    }
}
