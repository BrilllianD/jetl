//! Cursor/selection model and movement over a rope. Positions are char
//! indices; `head` is the caret, `anchor` the other end of the selection.

use std::ops::Range;

use ropey::Rope;

use super::rows::Rows;
use crate::actions::Motion;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn point(p: usize) -> Self {
        Self { anchor: p, head: p }
    }
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
    pub fn collapse(&mut self) {
        self.anchor = self.head;
    }
    pub fn set_head(&mut self, head: usize, extend: bool) {
        self.head = head;
        if !extend {
            self.anchor = head;
        }
    }
    pub fn clamp(&mut self, len: usize) {
        self.head = self.head.min(len);
        self.anchor = self.anchor.min(len);
    }
    /// `range` with this selection's direction: the head at its start when
    /// this one points backwards.
    pub fn with_range(&self, r: Range<usize>) -> Self {
        if self.anchor <= self.head { Self::new(r.start, r.end) } else { Self::new(r.end, r.start) }
    }
}

/// At most this many carets; adding more is refused.
pub const MAX_CARETS: usize = 1000;

/// The carets of a view: never empty, sorted by start, never overlapping.
/// The primary one (the last added, as in JetBrains) is the one the view
/// scrolls to and the one language server features act on. Each caret
/// keeps its own desired column for Up/Down.
#[derive(Debug, Clone)]
pub struct Selections {
    ranges: Vec<Selection>,
    primary: usize,
    desired: Vec<Option<usize>>,
}

impl Default for Selections {
    fn default() -> Self {
        Self::single(Selection::default())
    }
}

impl PartialEq for Selections {
    fn eq(&self, other: &Self) -> bool {
        self.ranges == other.ranges && self.primary == other.primary
    }
}

impl Eq for Selections {}

impl From<Selection> for Selections {
    fn from(s: Selection) -> Self {
        Self::single(s)
    }
}

impl Selections {
    pub fn single(s: Selection) -> Self {
        Self { ranges: vec![s], primary: 0, desired: vec![None] }
    }
    pub fn point(p: usize) -> Self {
        Self::single(Selection::point(p))
    }
    /// `ranges` (at least one) with `primary` indexing into them, normalized.
    pub fn new(ranges: Vec<Selection>, primary: usize) -> Self {
        assert!(!ranges.is_empty(), "a view has at least one caret");
        let desired = vec![None; ranges.len()];
        let mut s = Self { primary: primary.min(ranges.len() - 1), ranges, desired };
        s.normalize();
        s
    }
    pub fn primary(&self) -> Selection {
        self.ranges[self.primary]
    }
    /// The primary selection to change in place; call [`Selections::normalize`]
    /// after moving it while there are others.
    pub fn primary_mut(&mut self) -> &mut Selection {
        &mut self.ranges[self.primary]
    }
    pub fn primary_index(&self) -> usize {
        self.primary
    }
    /// The number of carets; never 0.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.ranges.len()
    }
    pub fn is_multi(&self) -> bool {
        self.ranges.len() > 1
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Selection> {
        self.ranges.iter()
    }
    pub fn as_slice(&self) -> &[Selection] {
        &self.ranges
    }
    /// Each selection with its desired column, for motions; normalize after.
    pub fn iter_mut_desired(&mut self) -> impl Iterator<Item = (&mut Selection, &mut Option<usize>)> {
        self.ranges.iter_mut().zip(self.desired.iter_mut())
    }
    /// The primary caret's desired column.
    pub fn desired(&self) -> Option<usize> {
        self.desired[self.primary]
    }
    /// Caret `i`'s desired column.
    pub fn desired_at(&self, i: usize) -> Option<usize> {
        self.desired.get(i).copied().flatten()
    }
    pub fn set_primary_desired(&mut self, d: Option<usize>) {
        self.desired[self.primary] = d;
    }
    /// Make selection `i` the primary one.
    pub fn set_primary(&mut self, i: usize) {
        if i < self.ranges.len() {
            self.primary = i;
        }
    }
    pub fn heads(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().map(|s| s.head)
    }

    /// Add `sel`, merging it with any it overlaps. Refused (false) at
    /// [`MAX_CARETS`].
    pub fn push(&mut self, sel: Selection, make_primary: bool) -> bool {
        if self.ranges.len() >= MAX_CARETS {
            return false;
        }
        self.ranges.push(sel);
        self.desired.push(None);
        if make_primary {
            self.primary = self.ranges.len() - 1;
        }
        self.normalize();
        true
    }

    /// Drop selection `i`; the last one is kept. The primary moves to the
    /// one before when it goes.
    pub fn remove(&mut self, i: usize) {
        if self.ranges.len() <= 1 || i >= self.ranges.len() {
            return;
        }
        self.ranges.remove(i);
        self.desired.remove(i);
        if self.primary > i || (self.primary == i && i > 0) {
            self.primary -= 1;
        }
    }

    /// Keep only the primary selection.
    pub fn collapse_to_primary(&mut self) {
        let (s, d) = (self.ranges[self.primary], self.desired[self.primary]);
        *self = Self { ranges: vec![s], primary: 0, desired: vec![d] };
    }

    /// Sort by start and merge overlapping selections (and touching ones
    /// where one side is a bare caret). A merged selection keeps the
    /// direction of the member holding the primary (else of its first
    /// non-empty member), and the primary follows into it.
    pub fn normalize(&mut self) {
        if self.ranges.len() == 1 {
            return;
        }
        let mut order: Vec<usize> = (0..self.ranges.len()).collect();
        order.sort_by_key(|&i| (self.ranges[i].range().start, self.ranges[i].range().end, i != self.primary));
        let mut out: Vec<(Selection, Option<usize>, bool)> = Vec::with_capacity(order.len());
        for i in order {
            let (s, d, p) = (self.ranges[i], self.desired[i], i == self.primary);
            if let Some(last) = out.last_mut() {
                let (a, b) = (last.0.range(), s.range());
                if b.start < a.end || (b.start == a.end && (a.is_empty() || b.is_empty())) {
                    let r = a.start..a.end.max(b.end);
                    // Direction: the primary member's, else the first non-empty one's.
                    let lead = if (p && !s.is_empty()) || last.0.is_empty() { s } else { last.0 };
                    let d = if p { d } else { last.1 };
                    *last = (lead.with_range(r), d, last.2 || p);
                    continue;
                }
            }
            out.push((s, d, p));
        }
        self.primary = out.iter().position(|e| e.2).unwrap_or(0);
        self.ranges = out.iter().map(|e| e.0).collect();
        self.desired = out.iter().map(|e| e.1).collect();
    }

    /// Whether the ranges are sorted and apart, as [`Selections::normalize`]
    /// leaves them.
    pub fn is_normalized(&self) -> bool {
        self.desired.len() == self.ranges.len()
            && self.primary < self.ranges.len()
            && self.ranges.windows(2).all(|w| {
                let (a, b) = (w[0].range(), w[1].range());
                b.start > a.end || (b.start == a.end && !a.is_empty() && !b.is_empty())
            })
    }

    /// Map every anchor and head through `f`, then normalize.
    pub fn map(&mut self, mut f: impl FnMut(usize) -> usize) {
        for s in &mut self.ranges {
            *s = Selection::new(f(s.anchor), f(s.head));
        }
        self.normalize();
    }

    /// Map through `changes` as [`super::buffer::map_through`] does.
    pub fn map_changes(&mut self, changes: &[super::undo::Change], stay_at_insert: bool) {
        self.map(|p| super::buffer::map_through(changes, p, stay_at_insert));
    }

    pub fn clamp(&mut self, len: usize) {
        for s in &mut self.ranges {
            s.clamp(len);
        }
        self.normalize();
    }

    pub fn reset_desired(&mut self) {
        self.desired.iter_mut().for_each(|d| *d = None);
    }

    /// Whether char `idx` is inside any selection.
    pub fn contains(&self, idx: usize) -> bool {
        let k = self.ranges.partition_point(|s| s.range().end <= idx);
        self.ranges.get(k).is_some_and(|s| s.range().contains(&idx))
    }

    /// The index of the selection holding `pos` (a caret at it, or a range
    /// around or ending at it).
    pub fn index_at(&self, pos: usize) -> Option<usize> {
        self.ranges.iter().position(|s| {
            let r = s.range();
            r.start <= pos && pos <= r.end
        })
    }
}

/// Number of chars in `line` excluding its line break.
pub fn line_len_no_eol(rope: &Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        return 0;
    }
    let l = rope.line(line);
    let n = l.len_chars();
    if n == 0 {
        return 0;
    }
    let last = l.char(n - 1);
    if last == '\n' {
        if n >= 2 && l.char(n - 2) == '\r' { n - 2 } else { n - 1 }
    } else if last == '\r' {
        n - 1
    } else {
        n
    }
}

pub fn pos_to_line_col(rope: &Rope, pos: usize) -> (usize, usize) {
    let pos = pos.min(rope.len_chars());
    let line = rope.char_to_line(pos);
    (line, pos - rope.line_to_char(line))
}

pub fn line_col_to_pos(rope: &Rope, line: usize, col: usize) -> usize {
    let line = line.min(rope.len_lines().saturating_sub(1));
    rope.line_to_char(line) + col.min(line_len_no_eol(rope, line))
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Word,
    Space,
    Punct,
}

pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn class(c: char) -> Class {
    if is_word_char(c) {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Punct
    }
}

pub fn next_word_boundary(rope: &Rope, pos: usize) -> usize {
    let len = rope.len_chars();
    if pos >= len {
        return len;
    }
    let mut i = pos;
    let c = rope.char(i);
    if c == '\n' {
        return i + 1;
    }
    let k = class(c);
    while i < len && class(rope.char(i)) == k && rope.char(i) != '\n' {
        i += 1;
    }
    if k != Class::Space {
        while i < len && class(rope.char(i)) == Class::Space && rope.char(i) != '\n' {
            i += 1;
        }
    }
    i
}

pub fn prev_word_boundary(rope: &Rope, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut i = pos;
    if rope.char(i - 1) == '\n' {
        return i - 1;
    }
    while i > 0 && class(rope.char(i - 1)) == Class::Space && rope.char(i - 1) != '\n' {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    if rope.char(i - 1) == '\n' {
        return i;
    }
    let k = class(rope.char(i - 1));
    while i > 0 && class(rope.char(i - 1)) == k && rope.char(i - 1) != '\n' {
        i -= 1;
    }
    i
}

/// Word under `pos` for double-click selection.
pub fn word_at(rope: &Rope, pos: usize) -> Range<usize> {
    let len = rope.len_chars();
    if len == 0 {
        return 0..0;
    }
    let p = pos.min(len - 1);
    let k = class(rope.char(p));
    let mut s = p;
    while s > 0 && class(rope.char(s - 1)) == k && rope.char(s - 1) != '\n' {
        s -= 1;
    }
    let mut e = p;
    while e < len && class(rope.char(e)) == k && rope.char(e) != '\n' {
        e += 1;
    }
    s..e
}

/// Whole line including its line break.
pub fn line_range(rope: &Rope, line: usize) -> Range<usize> {
    let line = line.min(rope.len_lines().saturating_sub(1));
    let s = rope.line_to_char(line);
    let e = if line + 1 < rope.len_lines() { rope.line_to_char(line + 1) } else { rope.len_chars() };
    s..e
}

/// First non-blank column of a line.
pub fn indent_of(rope: &Rope, line: usize) -> usize {
    let l = rope.line(line);
    l.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// The line a vertical motion (Up, Down, PageUp, PageDown) from `line`
/// lands on, counting visible rows (folded lines are skipped); `None` when
/// there is no line further that way, so the caret goes to the start or
/// end of the text instead.
pub fn vertical_target(rows: &Rows, line: usize, m: Motion, page: usize) -> Option<usize> {
    let page = page.max(1) as isize;
    let target = match m {
        Motion::Up => rows.step(line, -1),
        Motion::Down => rows.step(line, 1),
        Motion::PageUp => rows.step(line, -page),
        _ => rows.step(line, page),
    };
    (target != line).then_some(target)
}

pub fn apply_motion(rope: &Rope, rows: &Rows, sel: &mut Selection, m: Motion, extend: bool, desired_col: &mut Option<usize>, page: usize) {
    let len = rope.len_chars();
    let head = sel.head.min(len);
    let keep_desired = matches!(m, Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown);
    let new_head = match m {
        Motion::Left => {
            if !extend && !sel.is_empty() {
                sel.range().start
            } else if head > 0 {
                let mut p = head - 1;
                if p > 0 && rope.char(p) == '\n' && rope.char(p - 1) == '\r' {
                    p -= 1;
                }
                p
            } else {
                0
            }
        }
        Motion::Right => {
            if !extend && !sel.is_empty() {
                sel.range().end
            } else if head < len {
                let mut p = head + 1;
                if p < len && rope.char(head) == '\r' && rope.char(p) == '\n' {
                    p += 1;
                }
                p
            } else {
                len
            }
        }
        Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown => {
            let (line, col) = pos_to_line_col(rope, head);
            let col = desired_col.unwrap_or(col);
            *desired_col = Some(col);
            match vertical_target(rows, line, m, page) {
                Some(target) => line_col_to_pos(rope, target, col),
                None if matches!(m, Motion::Up | Motion::PageUp) => 0,
                None => len,
            }
        }
        Motion::WordLeft => prev_word_boundary(rope, head),
        Motion::WordRight => next_word_boundary(rope, head),
        Motion::LineStart => {
            let (line, col) = pos_to_line_col(rope, head);
            let indent = indent_of(rope, line);
            let start = rope.line_to_char(line);
            // JetBrains: Home toggles between first non-blank and column 0.
            if col == indent { start } else { start + indent }
        }
        Motion::LineEnd => {
            let (line, _) = pos_to_line_col(rope, head);
            rope.line_to_char(line) + line_len_no_eol(rope, line)
        }
        Motion::DocStart => 0,
        Motion::DocEnd => len,
    };
    if !keep_desired {
        *desired_col = None;
    }
    sel.set_head(new_head, extend);
}

/// [`apply_motion`] for every caret, each with its own desired column;
/// carets that meet merge.
pub fn apply_motion_all(rope: &Rope, rows: &Rows, sels: &mut Selections, m: Motion, extend: bool, page: usize) {
    for (s, d) in sels.iter_mut_desired() {
        apply_motion(rope, rows, s, m, extend, d, page);
    }
    sels.normalize();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::fold::Folds;

    static NO_FOLDS: std::sync::LazyLock<Folds> = std::sync::LazyLock::new(Folds::default);

    fn rows(r: &Rope) -> Rows<'static> {
        Rows::new(&NO_FOLDS, r.len_lines())
    }

    #[test]
    fn line_lengths_ignore_breaks() {
        let r = Rope::from_str("ab\r\ncd\nef");
        assert_eq!(line_len_no_eol(&r, 0), 2);
        assert_eq!(line_len_no_eol(&r, 1), 2);
        assert_eq!(line_len_no_eol(&r, 2), 2);
    }

    #[test]
    fn word_motion() {
        let r = Rope::from_str("foo bar_baz  (qux)\nnext");
        assert_eq!(next_word_boundary(&r, 0), 4);
        assert_eq!(next_word_boundary(&r, 4), 13);
        assert_eq!(next_word_boundary(&r, 13), 14);
        assert_eq!(prev_word_boundary(&r, 13), 4);
        assert_eq!(prev_word_boundary(&r, 4), 0);
        assert_eq!(word_at(&r, 5), 4..11);
    }

    fn ranges(s: &Selections) -> Vec<(usize, usize)> {
        s.iter().map(|x| (x.anchor, x.head)).collect()
    }

    #[test]
    fn normalize_sorts_and_merges() {
        // Overlapping ranges merge; touching non-empty ones stay apart.
        let s = Selections::new(vec![Selection::new(8, 10), Selection::new(0, 3), Selection::new(3, 5), Selection::new(9, 12)], 0);
        assert_eq!(ranges(&s), [(0, 3), (3, 5), (8, 12)]);
        assert!(s.is_normalized());
        assert_eq!(s.primary_index(), 2, "the primary follows into the merge");
        // A caret touching a range, and two carets at one place, merge.
        let s = Selections::new(vec![Selection::point(3), Selection::new(0, 3), Selection::point(7), Selection::point(7)], 3);
        assert_eq!(ranges(&s), [(0, 3), (7, 7)]);
        assert_eq!(s.primary(), Selection::point(7));
    }

    #[test]
    fn merged_selection_keeps_the_primary_direction() {
        // The primary points backwards: so does the union.
        let s = Selections::new(vec![Selection::new(0, 4), Selection::new(6, 2)], 1);
        assert_eq!(ranges(&s), [(6, 0)]);
        // Neither is primary: the first non-empty member's direction.
        let s = Selections::new(vec![Selection::point(20), Selection::new(5, 1), Selection::new(2, 6)], 0);
        assert_eq!(ranges(&s), [(6, 1), (20, 20)]);
        assert_eq!(s.primary(), Selection::point(20));
        // A primary caret inside a range takes the range's direction.
        let s = Selections::new(vec![Selection::new(9, 3), Selection::point(5)], 1);
        assert_eq!(ranges(&s), [(9, 3)]);
        assert_eq!(s.primary_index(), 0);
    }

    #[test]
    fn push_remove_and_cap() {
        let mut s = Selections::point(5);
        assert!(s.push(Selection::point(1), true));
        assert_eq!((ranges(&s), s.primary_index()), (vec![(1, 1), (5, 5)], 0));
        s.push(Selection::point(9), false);
        assert_eq!(s.primary(), Selection::point(1));
        s.remove(0);
        assert_eq!((ranges(&s), s.primary_index()), (vec![(5, 5), (9, 9)], 0));
        s.remove(0);
        s.remove(0);
        assert_eq!(ranges(&s), [(9, 9)], "the last caret stays");
        let mut s = Selections::point(0);
        for i in 1..MAX_CARETS + 5 {
            s.push(Selection::point(i * 2), false);
        }
        assert_eq!(s.len(), MAX_CARETS);
        assert!(!s.contains(1) && s.index_at(4) == Some(2));
    }

    #[test]
    fn map_changes_and_contains() {
        use super::super::undo::Change;
        let mut s = Selections::new(vec![Selection::new(2, 4), Selection::point(6), Selection::point(10)], 2);
        // Delete 3..7: the caret at 6 lands at 3, inside the shrunk range.
        s.map_changes(&[Change::delete(3, "abcd")], false);
        assert_eq!(ranges(&s), [(2, 3), (6, 6)]);
        assert_eq!(s.primary(), Selection::point(6));
        assert!(s.contains(2) && !s.contains(3) && !s.contains(6));
        s.clamp(4);
        assert_eq!(ranges(&s), [(2, 3), (4, 4)]);
        s.collapse_to_primary();
        assert_eq!(ranges(&s), [(4, 4)]);
    }

    #[test]
    fn motions_move_every_caret_with_its_own_column() {
        let r = Rope::from_str("abcdef\nab\nabcdef\n");
        let mut s = Selections::new(vec![Selection::point(1), Selection::point(5)], 1);
        apply_motion_all(&r, &rows(&r), &mut s, Motion::Down, false, 10);
        assert_eq!(s.heads().collect::<Vec<_>>(), [8, 9], "both clamp to the short line");
        apply_motion_all(&r, &rows(&r), &mut s, Motion::Down, false, 10);
        assert_eq!(s.heads().collect::<Vec<_>>(), [10 + 1, 10 + 5], "each keeps its column");
        // Carets that meet merge, and the primary survives.
        apply_motion_all(&r, &rows(&r), &mut s, Motion::LineStart, false, 10);
        assert_eq!((s.len(), s.primary()), (1, Selection::point(10)));
        // Extending keeps the anchors.
        let mut s = Selections::new(vec![Selection::point(0), Selection::point(7)], 0);
        apply_motion_all(&r, &rows(&r), &mut s, Motion::LineEnd, true, 10);
        assert_eq!(s.as_slice(), [Selection::new(0, 6), Selection::new(7, 9)]);
    }

    #[test]
    fn up_down_keep_desired_column() {
        let r = Rope::from_str("longer line\nab\nlonger line");
        let mut sel = Selection::point(8);
        let mut d = None;
        apply_motion(&r, &rows(&r), &mut sel, Motion::Down, false, &mut d, 10);
        assert_eq!(sel.head, 12 + 2);
        apply_motion(&r, &rows(&r), &mut sel, Motion::Down, false, &mut d, 10);
        assert_eq!(sel.head, 15 + 8);
    }
}
