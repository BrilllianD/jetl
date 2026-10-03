//! Screen rows of a view: the buffer lines a fold leaves visible. Every
//! vertical walk over the text (rendering, scrolling, Up/Down, mouse rows,
//! column blocks) goes through [`Rows`], so hidden lines are skipped in one
//! place. Word wrap (yman 106) will split a line into several rows here
//! (`Row::seg`).

use std::ops::Range;

use super::fold::Folds;

/// One screen row: a buffer line and, once lines wrap, which segment of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    /// Wrap segment; always 0 until word wrap exists.
    pub seg: u16,
}

/// The visible lines of a buffer of `last + 1` lines under its folds.
#[derive(Debug, Clone, Copy)]
pub struct Rows<'a> {
    /// Sorted, disjoint, merged; never contains line 0.
    hidden: &'a [Range<usize>],
    last: usize,
}

impl<'a> Rows<'a> {
    pub fn new(folds: &'a Folds, len_lines: usize) -> Self {
        Self { hidden: folds.hidden(), last: len_lines.saturating_sub(1) }
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

    /// Visible rows from `from` to `to` (in either order): the visible
    /// lines in between, counting one end.
    pub fn distance(&self, from: usize, to: usize) -> usize {
        let (lo, hi) = (from.min(to), from.max(to));
        let k = self.hidden.partition_point(|r| r.end <= lo);
        let hidden: usize = self.hidden[k..].iter().take_while(|r| r.start < hi).map(|r| r.end.min(hi) - r.start.max(lo)).sum();
        hi - lo - hidden
    }

    /// The row `row` rows below the top line `top`; `None` past the text.
    pub fn at_offset(&self, top: usize, row: usize) -> Option<Row> {
        self.iter_from(top).nth(row)
    }

    /// The rows from `top` (made visible) to the end of the text.
    pub fn iter_from(&self, top: usize) -> impl Iterator<Item = Row> + 'a {
        let rows = *self;
        std::iter::successors(Some(rows.visible(top)), move |&l| rows.next(l)).map(|line| Row { line, seg: 0 })
    }

    pub fn last_visible(&self) -> usize {
        self.visible(self.last)
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
        let visible: Vec<usize> = r.iter_from(0).map(|r| r.line).collect();
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
        assert_eq!(r.at_offset(1, 2), Some(Row { line: 8, seg: 0 }));
        assert_eq!(r.at_offset(1, 4), None, "past the text");
        // No folds: plain lines.
        let none = Folds::default();
        let r = Rows::new(&none, 5);
        assert_eq!((r.step(1, 2), r.distance(0, 4), r.next(4), r.last_visible()), (3, 4, None, 4));
    }
}
