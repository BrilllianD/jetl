//! Screen rows of a view: the buffer lines a fold leaves visible, each one
//! or, with soft wrap on, several rows (`Row::seg`, breaks from
//! [`super::wrap`]). Every vertical walk over the text (rendering,
//! scrolling, Up/Down, mouse rows, column blocks) goes through [`Rows`], so
//! hidden lines and wrapped segments are handled in one place.
//!
//! A view may also carry [`Filler`]s: blank rows that no text backs, which
//! the conflict resolver adds after a line to keep its panes aligned. They
//! are extra segments after the line's text segments (`Row::seg >=
//! text_segs`), so every walk counts them; [`Rows::text_row`] moves a caret
//! off one. `top` fillers come before line 0's text, as negative segments.

use std::ops::Range;
use std::rc::Rc;

use ratatui::style::Color;
use ropey::Rope;

use super::fold::Folds;
use super::selection::line_len_no_eol;
use super::wrap::{LineWrap, Wrap};

/// One screen row: a buffer line and, once lines wrap, which segment of it.
/// Rows order as they appear on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Row {
    pub line: usize,
    /// Wrap segment: 0 for the line's first text row; negative for the
    /// filler rows before line 0.
    pub seg: i32,
}

impl Row {
    /// The first row of `line`.
    pub fn line(line: usize) -> Self {
        Self { line, seg: 0 }
    }
}

/// Blank rows after a line that no text backs: the conflict resolver
/// pads its panes with them so they stay aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filler {
    /// The line the rows follow.
    pub line: usize,
    pub rows: u16,
    /// The resolver's block they pad (`editor::merge`), for the linked
    /// scroll.
    pub block: usize,
    /// Their background.
    pub bg: Option<Color>,
    /// Rows before line 0 (the resolver's empty block at the top of a
    /// pane) instead of after `line`; `line` is 0 then.
    pub top: bool,
}

/// The visible rows of a buffer of `last + 1` lines under its folds and,
/// when given one that is on, its wrap.
#[derive(Debug, Clone, Copy)]
pub struct Rows<'a> {
    /// Sorted, disjoint, merged; never contains line 0.
    hidden: &'a [Range<usize>],
    last: usize,
    /// The wrap (active), the text and its version.
    wrap: Option<(&'a Wrap, &'a Rope, u64)>,
    /// Sorted by line, the `top` ones first, stable within one.
    fill: &'a [Filler],
}

impl<'a> Rows<'a> {
    /// One row per visible line, whatever the wrap.
    pub fn new(folds: &'a Folds, len_lines: usize) -> Self {
        Self { hidden: folds.hidden(), last: len_lines.saturating_sub(1), wrap: None, fill: &[] }
    }

    /// The rows of a view: its folds and its wrap over `rope` at buffer
    /// `version`. Takes the fields, not the view, so the caller can borrow
    /// the carets mutably alongside (`Buffer::rows` is the shorthand).
    pub fn view(folds: &'a Folds, wrap: &'a Wrap, fill: &'a [Filler], rope: &'a Rope, version: u64) -> Self {
        let wrap = wrap.active().then_some((wrap, rope, version));
        Self { wrap, fill, ..Self::new(folds, rope.len_lines()) }
    }

    /// Whether lines wrap.
    pub fn wraps(&self) -> bool {
        self.wrap.is_some()
    }

    /// The tab width lines are broken with, when they wrap.
    pub fn wrap_tab(&self) -> Option<usize> {
        self.wrap.map(|(w, _, _)| w.tab())
    }

    /// How `line` wraps; `None` when lines do not.
    pub fn line_wrap(&self, line: usize) -> Option<Rc<LineWrap>> {
        self.wrap.map(|(w, rope, version)| w.line(rope, version, line))
    }

    /// The hidden range holding `line`.
    fn range_of(&self, line: usize) -> Option<&Range<usize>> {
        let k = self.hidden.partition_point(|r| r.end <= line);
        self.hidden.get(k).filter(|r| r.start <= line)
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        self.range_of(line).is_some()
    }

    /// `line` (clamped to the text) when visible, else the header of the
    /// fold hiding it.
    pub fn visible(&self, line: usize) -> usize {
        let line = line.min(self.last);
        self.range_of(line).map_or(line, |r| r.start - 1)
    }

    /// The next visible line after `line`.
    pub fn next(&self, line: usize) -> Option<usize> {
        let n = line + 1;
        let n = self.range_of(n).map_or(n, |r| r.end);
        (n <= self.last).then_some(n)
    }

    /// The visible line before `line`.
    pub fn prev(&self, line: usize) -> Option<usize> {
        let p = line.checked_sub(1)?;
        Some(self.range_of(p).map_or(p, |r| r.start - 1))
    }

    /// `delta` visible lines from `line` (made visible first), stopping at
    /// either end of the text.
    pub fn step(&self, line: usize, delta: isize) -> usize {
        let mut l = self.visible(line);
        for _ in 0..delta.unsigned_abs() {
            match if delta < 0 { self.prev(l) } else { self.next(l) } {
                Some(n) => l = n,
                None => break,
            }
        }
        l
    }

    /// Visible lines from `from` to `to` (in either order): the visible
    /// lines in between, counting one end.
    pub fn distance(&self, from: usize, to: usize) -> usize {
        let (lo, hi) = (from.min(to), from.max(to));
        let k = self.hidden.partition_point(|r| r.end <= lo);
        let hidden: usize = self.hidden[k..].iter().take_while(|r| r.start < hi).map(|r| r.end.min(hi) - r.start.max(lo)).sum();
        hi - lo - hidden
    }

    /// The fillers after `line`.
    fn fill_on(&self, line: usize) -> &'a [Filler] {
        let lo = self.fill.partition_point(|f| f.top || f.line < line);
        let hi = self.fill.partition_point(|f| f.line <= line);
        &self.fill[lo..hi]
    }

    /// The fillers before line 0.
    fn top_fill(&self) -> &'a [Filler] {
        &self.fill[..self.fill.partition_point(|f| f.top)]
    }

    /// The filler rows before line 0.
    pub fn top_rows(&self) -> i32 {
        self.top_fill().iter().map(|f| i32::from(f.rows)).sum()
    }

    pub fn fillers(&self) -> &'a [Filler] {
        self.fill
    }

    /// The rows the text of `line` takes.
    pub fn text_segs(&self, line: usize) -> u16 {
        self.line_wrap(line).map_or(1, |w| w.segs())
    }

    /// The first segment of `line`: it starts with the top fillers on line 0.
    pub fn first_seg(&self, line: usize) -> i32 {
        if line == 0 { -self.top_rows() } else { 0 }
    }

    /// The last segment of `line`: its text and the fillers after it.
    pub fn last_seg(&self, line: usize) -> i32 {
        let after: i32 = self.fill_on(line).iter().map(|f| i32::from(f.rows)).sum();
        i32::from(self.text_segs(line)) + after - 1
    }

    /// The rows `line` takes on screen: its text and its fillers.
    pub fn segs(&self, line: usize) -> u16 {
        let n = self.last_seg(line) - self.first_seg(line) + 1;
        u16::try_from(n).unwrap_or(u16::MAX)
    }

    /// Whether `row` is a filler row.
    pub fn is_filler(&self, row: Row) -> bool {
        row.seg < 0 || row.seg >= i32::from(self.text_segs(row.line))
    }

    /// The filler `row` is in and its index inside it.
    pub fn filler(&self, row: Row) -> Option<(&'a Filler, u16)> {
        let (k, fill) = if row.seg < 0 {
            (row.seg + self.top_rows(), self.top_fill())
        } else {
            (row.seg - i32::from(self.text_segs(row.line)), self.fill_on(row.line))
        };
        let mut k = u16::try_from(k).ok()?;
        for f in fill {
            if k < f.rows {
                return Some((f, k));
            }
            k -= f.rows;
        }
        None
    }

    /// Row `i` (clamped to the filler) of `fillers()[k]`.
    pub fn filler_row(&self, k: usize, i: u16) -> Row {
        let f = &self.fill[k];
        let i = i32::from(i.min(f.rows.saturating_sub(1)));
        let before: i32 = self.fill[..k].iter().filter(|g| g.line == f.line && g.top == f.top).map(|g| i32::from(g.rows)).sum();
        let seg = if f.top { -self.top_rows() + before + i } else { i32::from(self.text_segs(f.line)) + before + i };
        Row { line: f.line, seg }
    }

    /// `row` when it holds text; a filler row gives the next visible
    /// line's first row (`down`) or the line's last text row.
    pub fn text_row(&self, row: Row, down: bool) -> Row {
        if !self.is_filler(row) {
            return row;
        }
        if row.seg < 0 {
            return Row::line(0);
        }
        if down && let Some(n) = self.next(row.line) {
            return Row::line(n);
        }
        Row { line: row.line, seg: i32::from(self.text_segs(row.line)) - 1 }
    }

    /// The chars of `row`: from its first char to the next row's (the line
    /// end, without the line break, on its last row).
    pub fn seg_range(&self, rope: &Rope, row: Row) -> Range<usize> {
        if row.line >= rope.len_lines() {
            return rope.len_chars()..rope.len_chars();
        }
        let start = rope.line_to_char(row.line);
        let end = start + line_len_no_eol(rope, row.line);
        if row.seg < 0 {
            return start..start;
        }
        if self.is_filler(row) {
            return end..end;
        }
        let seg = u16::try_from(row.seg).unwrap_or_default();
        match self.line_wrap(row.line) {
            Some(w) if seg + 1 < w.segs() => start + w.start(seg).0..start + w.start(seg + 1).0,
            Some(w) => (start + w.start(seg).0).min(end)..end,
            None => start..end,
        }
    }

    /// The row char position `pos` is on (the line end is on the last row).
    pub fn row_of(&self, rope: &Rope, pos: usize) -> Row {
        let pos = pos.min(rope.len_chars());
        let line = rope.char_to_line(pos);
        let seg = self.line_wrap(line).map_or(0, |w| i32::from(w.seg_of(pos - rope.line_to_char(line))));
        Row { line, seg }
    }

    /// `row` on a visible line with its segment in range; a hidden line
    /// gives the first row of its fold's header.
    pub fn clamp(&self, row: Row) -> Row {
        let line = self.visible(row.line);
        let seg = if line == row.line { row.seg.clamp(self.first_seg(line), self.last_seg(line)) } else { 0 };
        Row { line, seg }
    }

    /// The row below `row`.
    pub fn next_row(&self, row: Row) -> Option<Row> {
        if row.seg < self.last_seg(row.line) {
            return Some(Row { line: row.line, seg: row.seg + 1 });
        }
        self.next(row.line).map(Row::line)
    }

    /// The row above `row`.
    pub fn prev_row(&self, row: Row) -> Option<Row> {
        if row.seg > self.first_seg(row.line) {
            return Some(Row { line: row.line, seg: row.seg - 1 });
        }
        self.prev(row.line).map(|line| Row { line, seg: self.last_seg(line) })
    }

    /// `delta` rows from `row` (clamped first), stopping at either end of
    /// the text.
    pub fn step_row(&self, row: Row, delta: isize) -> Row {
        if !self.wraps() && self.fill.is_empty() {
            return Row::line(self.step(row.line, delta));
        }
        let mut r = self.clamp(row);
        for _ in 0..delta.unsigned_abs() {
            match if delta < 0 { self.prev_row(r) } else { self.next_row(r) } {
                Some(n) => r = n,
                None => break,
            }
        }
        r
    }

    /// Rows from `from` to `to` (in either order, both visible), counting
    /// one end. Wrapped rows are walked, so the count stops at `cap`.
    pub fn row_distance(&self, from: Row, to: Row, cap: usize) -> usize {
        if !self.wraps() && self.fill.is_empty() {
            return self.distance(from.line, to.line);
        }
        let (mut r, hi) = (from.min(to), from.max(to));
        let mut n = 0;
        while r < hi && n < cap {
            match self.next_row(r) {
                Some(next) => (r, n) = (next, n + 1),
                None => break,
            }
        }
        n
    }

    /// The row `n` rows below `top`; `None` past the text.
    pub fn at_offset(&self, top: Row, n: usize) -> Option<Row> {
        self.iter_from(top).nth(n)
    }

    /// The rows from `top` (clamped) to the end of the text.
    pub fn iter_from(&self, top: Row) -> impl Iterator<Item = Row> + 'a {
        let rows = *self;
        std::iter::successors(Some(rows.clamp(top)), move |&r| rows.next_row(r))
    }

    pub fn last_visible(&self) -> usize {
        self.visible(self.last)
    }

    /// The last row of the text.
    pub fn last_row(&self) -> Row {
        let line = self.last_visible();
        Row { line, seg: self.last_seg(line) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::fold::Fold;
    use ropey::Rope;

    /// 12 lines `l0`..`l11`, with folds hiding lines 2..=4 (header 1),
    /// 6..=7 (header 5, nested 7 inside it is a fold too) and 10..=11.
    fn setup() -> (Rope, Folds) {
        let text: String = (0..12).map(|i| format!("l{i:02}\n")).collect::<String>().trim_end().to_string();
        let rope = Rope::from_str(&text);
        let at = |line: usize| rope.line_to_char(line);
        let mut f = Folds::default();
        for (h, last) in [(1, 4), (5, 7), (6, 7), (9, 11)] {
            f.collapse(Fold { start: at(h), end: at(last) + 3 }, &rope);
        }
        (rope, f)
    }

    #[test]
    fn hidden_ranges_merge_nested_folds() {
        let (rope, f) = setup();
        assert_eq!(f.hidden(), [2..5, 6..8, 10..12]);
        let r = Rows::new(&f, rope.len_lines());
        let visible: Vec<usize> = r.iter_from(Row::line(0)).map(|r| r.line).collect();
        assert_eq!(visible, [0, 1, 5, 8, 9]);
        assert!(r.is_hidden(3) && !r.is_hidden(5) && r.is_hidden(11));
        assert_eq!((r.visible(3), r.visible(7), r.visible(11), r.visible(99)), (1, 5, 9, 9));
        assert_eq!(r.last_visible(), 9);
    }

    #[test]
    fn step_next_prev_and_distance() {
        let (rope, f) = setup();
        let r = Rows::new(&f, rope.len_lines());
        assert_eq!((r.next(1), r.next(5), r.next(9)), (Some(5), Some(8), None));
        assert_eq!((r.prev(5), r.prev(8), r.prev(0)), (Some(1), Some(5), None));
        assert_eq!(r.step(0, 3), 8);
        assert_eq!(r.step(8, -2), 1);
        assert_eq!(r.step(0, 100), 9, "clamps at the end");
        assert_eq!(r.step(9, -100), 0);
        assert_eq!(r.step(3, 1), 5, "a hidden start counts from its header");
        assert_eq!((r.distance(0, 9), r.distance(9, 0), r.distance(1, 5), r.distance(5, 5)), (4, 4, 1, 0));
        assert_eq!(r.at_offset(Row::line(1), 2), Some(Row { line: 8, seg: 0 }));
        assert_eq!(r.at_offset(Row::line(1), 4), None, "past the text");
        // No folds: plain lines.
        let none = Folds::default();
        let r = Rows::new(&none, 5);
        assert_eq!((r.step(1, 2), r.distance(0, 4), r.next(4), r.last_visible()), (3, 4, None, 4));
    }

    /// Lines 0..=6: line 1 folds 2..=3 away; lines 0 and 4 wrap at 10
    /// cells into 3 and 2 rows.
    fn wrapped() -> (Rope, Folds, Wrap) {
        let text = "aaaa bbbb cccc dddd eeee\nfn x() {\n  y\n}\nffff gggg hhhh\nz\nlast";
        let rope = Rope::from_str(text);
        let mut f = Folds::default();
        let start = rope.line_to_char(1) + 7;
        f.collapse(Fold { start, end: rope.line_to_char(3) + 1 }, &rope);
        let mut w = Wrap::new(true);
        w.set_geometry(10, 4, 0);
        (rope, f, w)
    }

    fn row(line: usize, seg: i32) -> Row {
        Row { line, seg }
    }

    #[test]
    fn wrapped_rows_next_to_a_fold() {
        let (rope, f, w) = wrapped();
        assert_eq!(f.hidden(), std::slice::from_ref(&(2..4)));
        let r = Rows::view(&f, &w, &[], &rope, 0);
        assert!(r.wraps() && (r.segs(0), r.segs(1), r.segs(4), r.segs(5)) == (3, 1, 2, 1));
        let all: Vec<Row> = r.iter_from(row(0, 0)).collect();
        assert_eq!(all, [row(0, 0), row(0, 1), row(0, 2), row(1, 0), row(4, 0), row(4, 1), row(5, 0), row(6, 0)]);
        assert_eq!(r.step_row(row(0, 1), 3), row(4, 0));
        assert_eq!(r.step_row(row(4, 1), -2), row(1, 0));
        assert_eq!(r.step_row(row(4, 0), -3), row(0, 1));
        assert_eq!(r.step_row(row(3, 0), 1), row(4, 0), "a hidden line counts from its header");
        assert_eq!(r.step_row(row(0, 9), 0), row(0, 2), "a segment past the line clamps");
        assert_eq!((r.row_distance(row(0, 0), row(5, 0), 99), r.row_distance(row(5, 0), row(0, 1), 99)), (6, 5));
        assert_eq!(r.row_distance(row(0, 0), row(6, 0), 3), 3, "the cap stops the walk");
        assert_eq!(r.at_offset(row(0, 2), 2), Some(row(4, 0)));
        assert_eq!(r.at_offset(row(4, 1), 3), None);
        assert_eq!(r.last_row(), row(6, 0));
        // Chars of each row; the line end is on the last row.
        assert_eq!(r.seg_range(&rope, row(0, 1)), 10..20);
        assert_eq!(r.seg_range(&rope, row(0, 2)), 20..24);
        assert_eq!((r.row_of(&rope, 9), r.row_of(&rope, 10), r.row_of(&rope, 24)), (row(0, 0), row(0, 1), row(0, 2)));
        // Unwrapped rows of the same view: one per line.
        let plain = Rows::new(&f, rope.len_lines());
        assert!(!plain.wraps());
        assert_eq!((plain.segs(0), plain.row_of(&rope, 24), plain.seg_range(&rope, row(0, 0))), (1, row(0, 0), 0..24));
        // Off, or not drawn yet: no wrap.
        let mut off = Wrap::new(true);
        assert!(!Rows::view(&f, &off, &[], &rope, 0).wraps());
        off.set_geometry(10, 4, 0);
        off.on = false;
        assert!(!Rows::view(&f, &off, &[], &rope, 0).wraps());
    }

    #[test]
    fn filler_rows_follow_their_line() {
        let rope = Rope::from_str("l0\nl1\nl2\nl3\nl4\nl5");
        let f = Folds::default();
        let w = Wrap::new(false);
        let fill =
            [Filler { line: 1, rows: 2, block: 1, bg: None, top: false }, Filler { line: 5, rows: 1, block: 3, bg: None, top: false }];
        let r = Rows::view(&f, &w, &fill, &rope, 0);
        let all: Vec<Row> = r.iter_from(Row::line(0)).collect();
        assert_eq!(all, [row(0, 0), row(1, 0), row(1, 1), row(1, 2), row(2, 0), row(3, 0), row(4, 0), row(5, 0), row(5, 1)]);
        assert_eq!(r.step_row(Row::line(0), 3), row(1, 2));
        assert_eq!(r.row_distance(row(0, 0), row(2, 0), 99), 4);
        assert_eq!(r.last_row(), row(5, 1));
        assert_eq!(r.filler(row(1, 1)), Some((&fill[0], 0)));
        assert_eq!(r.filler(row(1, 2)), Some((&fill[0], 1)));
        assert!(!r.is_filler(row(1, 0)) && r.filler(row(1, 0)).is_none());
        assert_eq!(r.seg_range(&rope, row(1, 2)), 5..5, "the end of l1, empty");
        assert_eq!(r.text_row(row(1, 1), true), row(2, 0));
        assert_eq!(r.text_row(row(1, 2), false), row(1, 0));
        assert_eq!(r.text_row(row(5, 1), true), row(5, 0), "no line below");
        assert_eq!(r.text_row(row(2, 0), true), row(2, 0));
        // With wrap: the fillers come after the text segments.
        let (rope, f, w) = wrapped();
        let fill = [Filler { line: 0, rows: 1, block: 0, bg: None, top: false }];
        let r = Rows::view(&f, &w, &fill, &rope, 0);
        assert_eq!(r.segs(0), 4);
        assert!(r.filler(row(0, 3)).is_some() && r.is_filler(row(0, 3)) && !r.is_filler(row(0, 2)));
        assert_eq!(r.seg_range(&rope, row(0, 3)), 24..24);
        assert_eq!(r.prev_row(row(1, 0)), Some(row(0, 3)));
    }

    #[test]
    fn top_filler_rows_come_before_line_0() {
        let rope = Rope::from_str("l0\nl1\nl2");
        let f = Folds::default();
        let w = Wrap::new(false);
        let fill =
            [Filler { line: 0, rows: 2, block: 1, bg: None, top: true }, Filler { line: 0, rows: 1, block: 2, bg: None, top: false }];
        let r = Rows::view(&f, &w, &fill, &rope, 0);
        let all: Vec<Row> = r.iter_from(row(0, -2)).collect();
        assert_eq!(all, [row(0, -2), row(0, -1), row(0, 0), row(0, 1), row(1, 0), row(2, 0)]);
        assert_eq!(r.iter_from(Row::line(0)).next(), Some(row(0, 0)));
        assert_eq!(r.clamp(row(0, -9)), row(0, -2));
        assert_eq!(r.prev_row(Row::line(0)), Some(row(0, -1)));
        assert_eq!(r.prev_row(row(0, -2)), None);
        assert_eq!(r.step_row(Row::line(2), -10), row(0, -2));
        assert_eq!(r.filler(row(0, -1)), Some((&fill[0], 1)));
        assert_eq!(r.filler(row(0, 1)), Some((&fill[1], 0)));
        assert!(r.is_filler(row(0, -1)) && !r.is_filler(row(0, 0)));
        assert_eq!(r.text_row(row(0, -1), true), Row::line(0));
        assert_eq!(r.text_row(row(0, -1), false), Row::line(0));
        assert_eq!(r.seg_range(&rope, row(0, -1)), 0..0);
        assert_eq!((r.filler_row(0, 0), r.filler_row(0, 5), r.filler_row(1, 0)), (row(0, -2), row(0, -1), row(0, 1)));
        assert_eq!(r.segs(0), 4);
        assert_eq!(r.row_distance(row(0, -2), row(1, 0), 99), 4);
        // With wrap.
        let (rope, f, w) = wrapped();
        let fill = [Filler { line: 0, rows: 1, block: 1, bg: None, top: true }];
        let r = Rows::view(&f, &w, &fill, &rope, 0);
        assert_eq!(r.prev_row(Row::line(0)), Some(row(0, -1)));
        assert_eq!(r.row_of(&rope, 10), row(0, 1));
    }
}
