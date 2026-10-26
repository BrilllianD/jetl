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

/// Hunks of a line diff with lines as the editor counts them: a lone `\r`
/// ends a line, as `\n` and `\r\n` do.
pub fn rope_hunks(old: &str, new: &str) -> Vec<Hunk> {
    use crate::util::text::lines_inclusive;
    let mut input = InternedInput::default();
    input.update_before(lines_inclusive(old));
    input.update_after(lines_inclusive(new));
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

/// One patch line: `sign`, the text, and the no-newline marker when the text
/// has no terminator.
fn push_line(out: &mut String, sign: char, text: &str) {
    out.push(sign);
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
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
        blocks(&self.rows).into_iter().nth(block).map(|b| self.row_lines(false, b))
    }

    /// The old and new line ranges covered by `rows` of the side-by-side or
    /// unified rows. A side without lines there gets an empty range at the
    /// count of lines before.
    pub fn row_lines(&self, unified: bool, rows: Range<usize>) -> (Range<usize>, Range<usize>) {
        let all = if unified { &self.unified } else { &self.rows };
        let count = |rows: &[DiffRow], left: bool| rows.iter().filter(|r| if left { r.left.is_some() } else { r.right.is_some() }).count();
        let side = |left: bool| {
            let start = count(&all[..rows.start], left);
            start..start + count(&all[rows.clone()], left)
        };
        (side(true), side(false))
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
        let mut out = format!("diff --git a/{old_path} b/{new_path}\n--- a/{old_path}\n+++ b/{new_path}\n");
        self.write_hunk(&mut out, start..end);
        Some(out)
    }

    /// A unified patch of the selected lines for `git apply`: one hunk over
    /// the changed blocks the selection touches. `old_sel` / `new_sel` are
    /// old and new line indices (as `row_lines` returns them); a selected old
    /// line is `-`, a selected new line `+`. Unselected lines of the side the
    /// patch applies to (old, or new when `reverse`) stay as context, the
    /// other side's are left out. A touched block with an unterminated last
    /// line is taken whole.
    pub fn lines_patch(
        &self,
        old_sel: Range<usize>,
        new_sel: Range<usize>,
        old_path: &str,
        new_path: &str,
        context: usize,
        reverse: bool,
    ) -> Option<String> {
        let rows = &self.rows;
        let hit = |r: &DiffRow| {
            r.kind != RowKind::Equal && (r.left.is_some_and(|l| old_sel.contains(&l)) || r.right.is_some_and(|n| new_sel.contains(&n)))
        };
        let touched: Vec<Range<usize>> = blocks(rows).into_iter().filter(|b| rows[b.clone()].iter().any(hit)).collect();
        let (first, last) = (touched.first()?, touched.last()?);
        let unterminated =
            |r: &DiffRow| r.left.is_some_and(|l| !self.old[l].ends_with('\n')) || r.right.is_some_and(|n| !self.new[n].ends_with('\n'));
        let whole: Vec<bool> = touched.iter().map(|b| rows[b.clone()].iter().any(unterminated)).collect();
        let is_whole = |i: usize| touched.iter().zip(&whole).any(|(b, &w)| w && b.contains(&i));
        let equal = |i: usize| rows[i].kind == RowKind::Equal;
        let mut start = first.start;
        while start > 0 && first.start - start < context && equal(start - 1) {
            start -= 1;
        }
        let mut end = last.end;
        while end < rows.len() && end - last.end < context && equal(end) {
            end += 1;
        }
        let pos = rows[..start].iter().filter(|r| if reverse { r.right.is_some() } else { r.left.is_some() }).count();
        let mut body = String::new();
        let (mut n_old, mut n_new) = (0, 0);
        for (i, r) in rows.iter().enumerate().take(end).skip(start) {
            if r.kind == RowKind::Equal {
                if let Some(l) = r.left {
                    push_line(&mut body, ' ', &self.old[l]);
                    n_old += 1;
                    n_new += 1;
                }
                continue;
            }
            let whole = is_whole(i);
            if let Some(l) = r.left {
                if whole || old_sel.contains(&l) {
                    push_line(&mut body, '-', &self.old[l]);
                    n_old += 1;
                } else if !reverse {
                    push_line(&mut body, ' ', &self.old[l]);
                    n_old += 1;
                    n_new += 1;
                }
            }
            if let Some(n) = r.right {
                if whole || new_sel.contains(&n) {
                    push_line(&mut body, '+', &self.new[n]);
                    n_new += 1;
                } else if reverse {
                    push_line(&mut body, ' ', &self.new[n]);
                    n_old += 1;
                    n_new += 1;
                }
            }
        }
        let from = |n: usize| if n == 0 { pos } else { pos + 1 };
        Some(format!(
            "diff --git a/{old_path} b/{new_path}\n--- a/{old_path}\n+++ b/{new_path}\n@@ -{},{n_old} +{},{n_new} @@\n{body}",
            from(n_old),
            from(n_new)
        ))
    }

    /// One `@@` hunk (header and lines) of side-by-side rows `rows`: equal
    /// rows as context, each run of changed rows as its `-` then its `+` lines.
    fn write_hunk(&self, out: &mut String, rows: Range<usize>) {
        let all = &self.rows;
        let count =
            |range: Range<usize>, left: bool| all[range].iter().filter(|r| if left { r.left.is_some() } else { r.right.is_some() }).count();
        // A side without lines starts at the line before (git's convention).
        let header = |left: bool| {
            let (before, n) = (count(0..rows.start, left), count(rows.clone(), left));
            format!("{},{n}", if n == 0 { before } else { before + 1 })
        };
        out.push_str(&format!("@@ -{} +{} @@\n", header(true), header(false)));
        let mut line = |sign: char, text: &str| push_line(out, sign, text);
        let mut i = rows.start;
        while i < rows.end {
            if all[i].kind == RowKind::Equal {
                if let Some(l) = all[i].left {
                    line(' ', &self.old[l]);
                }
                i += 1;
                continue;
            }
            let run_end = (i..rows.end).find(|&j| all[j].kind == RowKind::Equal).unwrap_or(rows.end);
            for l in all[i..run_end].iter().filter_map(|r| r.left) {
                line('-', &self.old[l]);
            }
            for n in all[i..run_end].iter().filter_map(|r| r.right) {
                line('+', &self.new[n]);
            }
            i = run_end;
        }
    }

    /// A whole-file unified patch (`old_name` to `new_name`, no git
    /// prefixes) with `context` equal lines around each change; hunks that
    /// touch are merged. The header alone when nothing changed.
    pub fn patch(&self, old_name: &str, new_name: &str, context: usize) -> String {
        let mut out = format!("diff -u {old_name} {new_name}\n--- {old_name}\n+++ {new_name}\n");
        let mut groups: Vec<Range<usize>> = Vec::new();
        for b in blocks(&self.rows) {
            let s = b.start.saturating_sub(context);
            let e = (b.end + context).min(self.rows.len());
            match groups.last_mut() {
                Some(last) if s <= last.end => last.end = e,
                _ => groups.push(s..e),
            }
        }
        for g in groups {
            self.write_hunk(&mut out, g);
        }
        out
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

/// Context lines around each change of a whole-file compare patch.
pub const COMPARE_CONTEXT: usize = 3;

/// The result of comparing two whole texts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compared {
    /// A unified patch of all changes (old = `-`, new = `+`).
    Patch(String),
    Identical,
    /// A side looks binary (`is_binary`).
    Binary,
}

/// Compare `old` with `new` as whole files named `old_name` and `new_name`
/// (shown in the header only).
pub fn compare(old: &[u8], new: &[u8], old_name: &str, new_name: &str) -> Compared {
    let d = DiffDoc::build(old, new, 4);
    if d.binary {
        Compared::Binary
    } else if d.is_identical() {
        Compared::Identical
    } else {
        Compared::Patch(d.patch(old_name, new_name, COMPARE_CONTEXT))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rope_hunks_count_lone_cr_as_a_break() {
        let h = super::rope_hunks("a\rb\rc", "a\rB\rc");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].before.clone(), h[0].after.clone()), (1..2, 1..2));
    }

    use super::*;

    /// Builds the patch of `rows` (of the unified or side-by-side mode), applies it
    /// with real `git apply` to `old` (or `new` when `reverse`), returns the
    /// resulting file text and the patch.
    fn apply(case: &str, old: &str, new: &str, unified: bool, rows: Range<usize>, reverse: bool) -> (String, String) {
        use std::io::Write;
        let d = DiffDoc::build(old.as_bytes(), new.as_bytes(), 4);
        let (o, n) = d.row_lines(unified, rows);
        let patch = d.lines_patch(o, n, "f.txt", "f.txt", 3, reverse).unwrap();
        let dir = std::env::temp_dir().join(format!("jetl-lines-patch-{case}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), if reverse { new } else { old }).unwrap();
        let mut cmd = std::process::Command::new("git");
        cmd.args(["apply", "--whitespace=nowarn"]);
        if reverse {
            cmd.arg("--reverse");
        }
        let mut child = cmd
            .arg("-")
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(patch.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git apply failed: {}\npatch:\n{patch}", String::from_utf8_lossy(&out.stderr));
        let text = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        (text, patch)
    }

    #[test]
    fn lines_patches_apply_for_real() {
        let hunk = |p: &str| p[p.find("@@").unwrap()..].to_string();
        // Two of three added lines.
        let (old, new) = ("1\n2\n3\n4\n5\n6\n", "1\n2\n3\na\nb\nc\n4\n5\n6\n");
        let (text, patch) = apply("two-of-three", old, new, false, 3..5, false);
        assert_eq!(text, "1\n2\n3\na\nb\n4\n5\n6\n");
        assert_eq!(hunk(&patch), "@@ -1,6 +1,8 @@\n 1\n 2\n 3\n+a\n+b\n 4\n 5\n 6\n");
        assert_eq!(apply("two-of-three-b", old, new, false, 4..6, false).0, "1\n2\n3\nb\nc\n4\n5\n6\n");
        // One of two removed lines.
        let (text, patch) = apply("removed", "1\nx\ny\n2\n", "1\n2\n", false, 2..3, false);
        assert_eq!(text, "1\nx\n2\n");
        assert_eq!(hunk(&patch), "@@ -1,4 +1,3 @@\n 1\n x\n-y\n 2\n");
        // Half of a modified pair, unified.
        assert_eq!(apply("half-new", "a\nb\nc\n", "a\nB\nc\n", true, 2..3, false).0, "a\nb\nB\nc\n");
        assert_eq!(apply("half-old", "a\nb\nc\n", "a\nB\nc\n", true, 1..2, false).0, "a\nc\n");
        // Across two blocks.
        let (old, new) = ("a\n1\n2\nb\n", "A\n1\n2\nB\n");
        assert_eq!(apply("across-u", old, new, true, 1..5, false).0, "a\nA\n1\n2\n");
        assert_eq!(apply("across-s", old, new, false, 0..4, false).0, "A\n1\n2\nB\n");
        // Reverse (unstage).
        let (text, patch) = apply("rev-add", "1\n2\n3\n4\n", "1\n2\na\nb\n3\n4\n", false, 3..4, true);
        assert_eq!(text, "1\n2\na\n3\n4\n");
        assert_eq!(hunk(&patch), "@@ -1,5 +1,6 @@\n 1\n 2\n a\n+b\n 3\n 4\n");
        assert_eq!(apply("rev-rm", "1\nx\ny\n2\n", "1\n2\n", false, 2..3, true).0, "1\ny\n2\n");
        let (text, patch) = apply("rev-start", "1\n2\n3\n4\n", "0\n1\n2\nX\n3\n4\n", false, 3..4, true);
        assert_eq!(text, "0\n1\n2\n3\n4\n");
        assert!(hunk(&patch).starts_with("@@ -2,") && hunk(&patch).contains(" +2,"), "{patch}");
        // A last line without a newline: the block is taken whole.
        let (text, patch) = apply("no-eol", "a\nb", "a\nb\nc", false, 2..3, false);
        assert_eq!(text, "a\nb\nc");
        assert_eq!(hunk(&patch), "@@ -1,2 +1,3 @@\n a\n-b\n\\ No newline at end of file\n+b\n+c\n\\ No newline at end of file\n");
        // Only an equal row selected.
        let d = DiffDoc::build(b"a\nb\n", b"a\nB\n", 4);
        assert!(d.lines_patch(0..1, 0..1, "f", "f", 3, false).is_none());
    }

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
    fn row_lines_count_the_lines_of_some_rows() {
        let d = DiffDoc::build(b"x\na\nb\nc\ny\n", b"x\nA\nB\nC\ny\n", 4);
        // The middle row of a three-row modified block, one line before it.
        assert_eq!(d.row_lines(false, 2..3), (2..3, 2..3));
        assert_eq!(d.row_lines(false, 1..4), d.block_lines(0).unwrap());
        // Unified: a deletion row alone has an empty new range at the
        // block's new start; an addition row an empty old one.
        assert_eq!(d.row_lines(true, 1..2), (1..2, 1..1));
        assert_eq!(d.row_lines(true, 4..5), (4..4, 1..2));
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

    #[test]
    fn compare_patches_whole_files() {
        const P: &str = "diff -u o n\n--- o\n+++ n\n";
        let c = |o: &[u8], n: &[u8]| compare(o, n, "o", "n");
        let lines = |n: usize| (1..=n).map(|i| format!("{i}\n")).collect::<String>();
        let far_new = lines(20).replace("\n2\n", "\ntwo\n").replace("\n19\n", "\nnineteen\n");
        assert_eq!(
            c(lines(20).as_bytes(), far_new.as_bytes()),
            Compared::Patch(format!(
                "{P}@@ -1,5 +1,5 @@\n 1\n-2\n+two\n 3\n 4\n 5\n@@ -16,5 +16,5 @@\n 16\n 17\n 18\n-19\n+nineteen\n 20\n"
            ))
        );
        let near_new = lines(10).replace("\n2\n", "\ntwo\n").replace("\n8\n", "\neight\n");
        let Compared::Patch(p) = c(lines(10).as_bytes(), near_new.as_bytes()) else { panic!("patch") };
        assert_eq!(p.matches("@@ -").count(), 1);
        assert!(p.contains("@@ -1,10 +1,10 @@"));
        assert_eq!(c(b"a\n", b"new\na\n"), Compared::Patch(format!("{P}@@ -1,1 +1,2 @@\n+new\n a\n")));
        assert_eq!(c(b"", b"x\n"), Compared::Patch(format!("{P}@@ -0,0 +1,1 @@\n+x\n")));
        assert_eq!(c(b"a\nb\n", b"a\nb\nc"), Compared::Patch(format!("{P}@@ -1,2 +1,3 @@\n a\n b\n+c\n\\ No newline at end of file\n")));
        assert_eq!(c(b"a\nb", b"a\nb\n"), Compared::Patch(format!("{P}@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n")));
        assert_eq!(c(b"a\r\nb\r\n", b"a\r\nB\r\n"), Compared::Patch(format!("{P}@@ -1,2 +1,2 @@\n a\r\n-b\r\n+B\r\n")));
        assert_eq!(c(b"x\n", b"x\n"), Compared::Identical);
        assert_eq!(c(b"a\0b", b"a"), Compared::Binary);
        assert_eq!(c(b"", b""), Compared::Identical);
    }
}
