//! Column (block) selection: a rectangle of lines by visual columns made
//! into one caret per line ([`block_selections`]), by Alt+drag or, in
//! column selection mode, by a plain drag and Shift+arrows.

use super::buffer::{Buffer, CaretEdit};
use super::rows::{Row, Rows};
use super::selection::{Selection, apply_motion, indent_of, line_len_no_eol, next_word_boundary, prev_word_boundary, vertical_target};
use super::view::{block_selections, col_to_char, layout_line, visual_col};
use super::view_state::Block;
use crate::actions::Motion;

impl Buffer {
    /// Whether column blocks can be made here. Blocks are buffer lines by
    /// visual columns, which soft wrap breaks: this is the one place that
    /// refuses column selection (or would let it through once
    /// [`block_selections`] works on visual rows) when wrap is on.
    pub fn blocks_supported(&self) -> bool {
        !self.view.wrap.on
    }

    /// Make the carets the selections of `block`.
    pub fn set_block(&mut self, block: Block, tab_width: usize) {
        let rows = Rows::new(&self.view.folds, self.rope.len_lines());
        self.view.sels = block_selections(&self.rope, &rows, block.anchor, block.head, tab_width);
        self.view.block = Some(block);
        self.view.occ_stack.clear();
        self.view.occ_whole_word = false;
        self.undo.break_group = true;
    }

    /// The block made by [`Buffer::set_block`] while the carets are still
    /// its selections.
    pub fn live_block(&self, tab_width: usize) -> Option<Block> {
        let b = self.view.block?;
        let rows = Rows::new(&self.view.folds, self.rope.len_lines());
        (block_selections(&self.rope, &rows, b.anchor, b.head, tab_width) == self.view.sels).then_some(b)
    }

    /// The caret a click on cell (line, visual column) puts in virtual
    /// space: at its line's end on that column, when the cell is past the
    /// end (soft wrap off).
    pub fn virtual_caret(&self, (line, col): (usize, usize), tab_width: usize) -> Option<Selection> {
        if !self.blocks_supported() || line >= self.rope.len_lines() {
            return None;
        }
        let end = self.rope.line_to_char(line) + line_len_no_eol(&self.rope, line);
        (col > visual_col(&self.rope, end, tab_width)).then(|| Selection::virtual_point(end, col))
    }

    /// A motion in column selection mode (without Shift). Right at or past
    /// a line end steps one column into virtual space, Left in virtual space
    /// one column back down to the line end, and Up/Down/PageUp/PageDown
    /// keep each caret's column on lines too short for it. Other motions
    /// act as usual.
    pub fn move_virtual(&mut self, m: Motion, page: usize, tab_width: usize) {
        let rows = Rows::view(&self.view.folds, &self.view.wrap, &self.view.fillers, &self.rope, self.version)
            .with_inlays(&self.inlays, &self.inline_values);
        let rope = &self.rope;
        for (s, d) in self.view.sels.iter_mut_desired() {
            let line = rope.char_to_line(s.head.min(rope.len_chars()));
            let end = rope.line_to_char(line) + line_len_no_eol(rope, line);
            let w = visual_col(rope, end, tab_width);
            let col = s.head_col(rope, tab_width);
            if m == Motion::Left && s.is_empty() && col > w {
                *s = if col - 1 > w { Selection::virtual_point(end, col - 1) } else { Selection::point(end) };
                *d = None;
            } else if m == Motion::Right && s.is_empty() && s.head == end {
                *s = Selection::virtual_point(end, col + 1);
                *d = None;
            } else {
                apply_motion(rope, &rows, s, m, false, d, page, tab_width);
                if matches!(m, Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown)
                    && let Some(c) = *d
                {
                    let head = s.head.min(rope.len_chars());
                    let hl = rope.char_to_line(head);
                    if hl != line && head == rope.line_to_char(hl) + line_len_no_eol(rope, hl) && c > visual_col(rope, head, tab_width) {
                        *s = Selection::virtual_point(head, c);
                    }
                }
            }
        }
        self.view.sels.normalize();
        self.view.follow_key = None;
    }

    /// Per caret, the spaces its line lacks to reach the left column of its
    /// virtual cells (an empty caret in virtual space), else 0. Empty while
    /// no caret is in virtual space or the view soft-wraps. Typing pads by
    /// these.
    pub fn virtual_pads(&self, tab_width: usize) -> Vec<usize> {
        if !self.blocks_supported() || !self.view.sels.iter().any(Selection::is_virtual) {
            return Vec::new();
        }
        self.view
            .sels
            .iter()
            .map(|s| {
                if s.is_empty() {
                    let lo = s.anchor_col(&self.rope, tab_width).min(s.head_col(&self.rope, tab_width));
                    lo.saturating_sub(visual_col(&self.rope, s.head, tab_width))
                } else {
                    0
                }
            })
            .collect()
    }

    /// Backspace (`back`) or Delete, by char or by `word`, at every caret.
    /// A caret that is empty in chars but has virtual cells changes no text.
    /// If its cells have width, it goes to their left column. Delete and
    /// Ctrl+Delete leave it where it is. Backspace moves it one column left.
    /// Ctrl+Backspace moves it to the line end. Every other caret edits as
    /// usual, all in one transaction. With a live block it is made live
    /// again at its new column, as Backspace and Delete always did. False
    /// when nothing here is virtual (no virtual caret and no live block, or
    /// a word delete without a virtual caret) or the view soft-wraps.
    pub fn virtual_delete(&mut self, back: bool, word: bool, pairs: &[(char, char)], tab_width: usize) -> bool {
        if !self.blocks_supported() {
            return false;
        }
        let block = self.live_block(tab_width);
        let any = self.view.sels.iter().any(Selection::is_virtual);
        if !any && (word || block.is_none()) {
            return false;
        }
        self.edit_carets(|buf, sel| {
            let w = visual_col(&buf.rope, sel.head, tab_width);
            let (a, h) = (sel.anchor_col(&buf.rope, tab_width), sel.head_col(&buf.rope, tab_width));
            if sel.is_empty() && a.max(h) > w {
                let (lo, hi) = (a.min(h), a.max(h));
                let col = if lo < hi || !back {
                    lo
                } else if word {
                    w
                } else {
                    lo - 1
                };
                return CaretEdit::keep(sel).virtual_at((col > w).then_some(col));
            }
            match (back, word) {
                (true, false) => buf.backspace_edit(sel, pairs),
                (false, false) => buf.delete_edit(sel),
                (true, true) => buf.word_back_edit(sel),
                (false, true) => buf.word_forward_edit(sel),
            }
        });
        if !word && let Some(b) = block {
            let (lo, hi) = (b.anchor.1.min(b.head.1), b.anchor.1.max(b.head.1));
            let col = if back && lo == hi { lo.saturating_sub(1) } else { lo };
            self.keep_block(Block { anchor: (b.anchor.0, col), head: (b.head.0, col) }, tab_width);
        }
        self.view.follow_key = None;
        true
    }

    /// After an edit (which dropped the block): `b`, a zero-width block, is
    /// live again when the carets are its selections and each sits on its
    /// column or at the end of a shorter line; else there is none.
    fn keep_block(&mut self, b: Block, tab_width: usize) {
        let col = b.head.1;
        let fits = self.view.sels.iter().all(|s| {
            let line = self.rope.char_to_line(s.head);
            let end = self.rope.line_to_char(line) + line_len_no_eol(&self.rope, line);
            let vc = visual_col(&self.rope, s.head, tab_width);
            vc == col || (vc < col && s.head == end)
        });
        let rows = Rows::new(&self.view.folds, self.rope.len_lines());
        if fits && block_selections(&self.rope, &rows, b.anchor, b.head, tab_width) == self.view.sels {
            self.view.block = Some(b);
        }
    }

    /// The block the carets are: the last one made while the carets are
    /// still its selections, else one from the anchor of the caret farthest
    /// from the primary (the first or the last) to the primary's head.
    pub fn current_block(&self, tab_width: usize) -> Block {
        if let Some(b) = self.live_block(tab_width) {
            return b;
        }
        let sels = self.view.sels.as_slice();
        let far = if self.view.sels.primary_index() == 0 { sels[sels.len() - 1] } else { sels[0] };
        let line = |p: usize| self.rope.char_to_line(p);
        let head = self.view.sel();
        Block {
            anchor: (line(far.anchor), far.anchor_col(&self.rope, tab_width)),
            head: (line(head.head), head.head_col(&self.rope, tab_width)),
        }
    }

    /// Shift+motion in column selection mode: move the block's head. Left
    /// and Right step over one grapheme of the head's line (one column at a
    /// time past its end, with no limit: virtual space), Up/Down and
    /// PageUp/Down change its line, Home goes to the indent or column 0 like smart Home,
    /// End to the end of the head's line. False for other motions, which
    /// act as usual.
    pub fn extend_block(&mut self, m: Motion, page: usize, tab_width: usize) -> bool {
        let mut b = self.current_block(tab_width);
        let (line, col) = b.head;
        let width = |l: usize| line_width(self, l, tab_width);
        b.head = match m {
            Motion::Left => (line, step_col(self, line, col, tab_width, false)),
            Motion::Right => (line, step_col(self, line, col, tab_width, true)),
            Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown => {
                let rows = Rows::new(&self.view.folds, self.rope.len_lines());
                (vertical_target(&rows, Row::line(line), m, page).map_or(line, |r| r.line), col)
            }
            Motion::LineStart => {
                let start = self.rope.line_to_char(line);
                let indent = visual_col(&self.rope, start + indent_of(&self.rope, line), tab_width);
                (line, if col == indent { 0 } else { indent })
            }
            Motion::LineEnd => (line, width(line)),
            Motion::WordLeft | Motion::WordRight => {
                let right = m == Motion::WordRight;
                let w = width(line);
                if col > w {
                    // Past the line's end: left snaps to the end, right stays.
                    (line, if right { col } else { w })
                } else {
                    let start = self.rope.line_to_char(line);
                    let pos = col_to_char(&self.rope, line, col, tab_width);
                    let to = if right { next_word_boundary(&self.rope, pos) } else { prev_word_boundary(&self.rope, pos) };
                    let to = to.clamp(start, start + line_len_no_eol(&self.rope, line));
                    (line, visual_col(&self.rope, to, tab_width))
                }
            }
            _ => return false,
        };
        self.set_block(b, tab_width);
        // A head moving in virtual space leaves the primary's char position
        // alone, so the view would not follow it otherwise.
        self.view.follow_key = None;
        true
    }
}

/// Cells taken by `line`.
fn line_width(buf: &Buffer, line: usize, tab_width: usize) -> usize {
    visual_col(&buf.rope, buf.rope.line_to_char(line) + line_len_no_eol(&buf.rope, line), tab_width)
}

/// The grapheme edge on `line` next to visual column `col` (`forward` or
/// back); one column over at or past the line end.
fn step_col(buf: &Buffer, line: usize, col: usize, tab_width: usize, forward: bool) -> usize {
    let mut edges = vec![0];
    layout_line(&buf.rope, line, tab_width, usize::MAX, &mut |_, c| {
        edges.push(c.col + c.width);
        true
    });
    let end = edges[edges.len() - 1];
    match forward {
        true if col >= end => col + 1,
        true => edges.into_iter().find(|&e| e > col).unwrap_or(col + 1),
        false if col > end => col - 1,
        false => edges.into_iter().rev().find(|&e| e < col).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::selection::{Selections, Virt};
    use crate::editor::undo::TxKind;
    use crate::editor::view::col_to_char_ceil;
    use ropey::Rope;

    const TEXT: &str = "\tab日本語x\n\tab日本語x\nab\n\tab日本語x\n";

    fn b(text: &str) -> Buffer {
        Buffer::from_text(1, None, text, false)
    }

    fn texts(b: &Buffer) -> Vec<String> {
        b.view.sels.iter().map(|s| b.rope.slice(s.range()).to_string()).collect()
    }

    #[test]
    fn block_over_tabs_wide_chars_and_a_short_line() {
        // tab (cols 0-3), a(4), b(5), 日(6-7), 本(8-9), 語(10-11), x(12)
        let mut buf = b(TEXT);
        buf.set_block(Block { anchor: (0, 5), head: (3, 7) }, 4);
        // 日 is cut by the right edge, so it is in; "ab" is too short.
        assert_eq!(texts(&buf), ["b日", "b日", "", "b日"]);
        let sels = buf.view.sels.as_slice();
        assert_eq!(
            sels[2],
            Selection { virt: Virt { anchor: Some(5), head: Some(7) }, ..Selection::point(buf.rope.line_to_char(2) + 2) },
            "the short line: a caret at its end with the edges as virtual columns"
        );
        assert_eq!(buf.view.sels.primary_index(), 3, "the head's line holds the primary");
        assert!(sels[0].anchor < sels[0].head);
        // Dragged right to left and bottom to top: the carets at the left
        // edge, the primary on the first line; the tab cut at col 2 is in,
        // and "ab" ends right at the edge.
        buf.set_block(Block { anchor: (3, 7), head: (0, 2) }, 4);
        assert_eq!(texts(&buf), ["\tab日", "\tab日", "", "\tab日"]);
        assert_eq!(buf.view.sels.primary_index(), 0);
        assert!(buf.view.sels.iter().filter(|s| !s.is_empty()).all(|s| s.head < s.anchor));
        // Zero width, inside 本: a caret before it on every long line.
        buf.set_block(Block { anchor: (0, 9), head: (3, 9) }, 4);
        assert_eq!(buf.view.sels.heads().collect::<Vec<_>>(), [4, 12, 18, 23]);
        // Past every line end: carets at the line ends.
        buf.set_block(Block { anchor: (1, 40), head: (2, 50) }, 4);
        let v = Virt { anchor: Some(40), head: Some(50) };
        assert_eq!(
            buf.view.sels.as_slice(),
            [Selection { virt: v, ..Selection::point(15) }, Selection { virt: v, ..Selection::point(18) }]
        );
    }

    #[test]
    fn block_edges_agree_with_visual_col() {
        let lines = ["x\ty\t\tz 😀 end", "\tab日本語x", "", "ab", "😀😀\t1"];
        let rope = Rope::from_str(&lines.join("\n"));
        let mut x: u64 = 7;
        for _ in 0..500 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (c0, c1, l0, l1) = ((x >> 33) % 24, (x >> 41) % 24, (x >> 49) % 5, (x >> 57) % 5);
            let (c0, c1, l0, l1) = (c0 as usize, c1 as usize, l0 as usize, l1 as usize);
            let sels = block_selections(&rope, &Rows::new(&Default::default(), rope.len_lines()), (l0, c0), (l1, c1), 4);
            assert_eq!(sels.len(), l0.abs_diff(l1) + 1);
            let (lo, hi) = (c0.min(c1), c0.max(c1));
            for (i, s) in sels.iter().enumerate() {
                let line = l0.min(l1) + i;
                let r = s.range();
                let (start, end) = (visual_col(&rope, r.start, 4), visual_col(&rope, r.end, 4));
                let width = visual_col(&rope, rope.line_to_char(line) + line_len_no_eol(&rope, line), 4);
                assert_eq!(rope.char_to_line(r.start), line);
                if width <= lo {
                    assert_eq!((start, end), (width, width), "short line {line}: a caret at its end");
                    continue;
                }
                // The selection starts at or before the left edge and
                // nothing before it reaches past that edge.
                assert!(start <= lo, "{lo}..{hi} on {line}: starts at {start}");
                assert_eq!(r.start, col_to_char(&rope, line, lo, 4));
                if lo == hi {
                    assert!(s.is_empty());
                } else {
                    // It ends at or after the right edge (or the line end),
                    // on the first grapheme starting there.
                    assert!(end >= hi.min(width), "{lo}..{hi} on {line}: ends at {end}");
                    assert_eq!(r.end, col_to_char_ceil(&rope, line, hi, 4));
                    assert!(r.end == r.start || visual_col(&rope, r.end - 1, 4) < hi);
                }
            }
        }
    }

    #[test]
    fn shift_arrows_extend_the_block() {
        let mut buf = b(TEXT);
        // From a caret after the tab of line 0: Shift+Down x3 is a caret per
        // line (the short one at its end), Shift+Right takes `a` on each long one.
        buf.view.set_sel(Selection::point(1));
        for _ in 0..3 {
            assert!(buf.extend_block(Motion::Down, 10, 4));
        }
        assert_eq!(buf.view.sels.len(), 4);
        assert!(buf.view.sels.iter().all(|s| s.is_empty()));
        assert!(buf.extend_block(Motion::Right, 10, 4));
        assert_eq!(texts(&buf), ["a", "a", "", "a"]);
        // Steps go by graphemes of the head's line: b, then all of 日.
        assert!(buf.extend_block(Motion::Right, 10, 4));
        assert!(buf.extend_block(Motion::Right, 10, 4));
        assert_eq!(texts(&buf), ["ab日", "ab日", "", "ab日"]);
        // The short line keeps the block's columns: back up over it.
        assert!(buf.extend_block(Motion::Up, 10, 4));
        assert!(buf.extend_block(Motion::Up, 10, 4));
        assert_eq!(texts(&buf), ["ab日", "ab日"]);
        assert_eq!(buf.current_block(4), Block { anchor: (0, 4), head: (1, 8) });
        // End, then Right goes on past the line end; Home goes to the indent.
        assert!(buf.extend_block(Motion::LineEnd, 10, 4));
        assert!(buf.extend_block(Motion::Right, 10, 4));
        assert_eq!(buf.current_block(4).head, (1, 14));
        assert!(buf.extend_block(Motion::LineStart, 10, 4));
        assert_eq!(buf.current_block(4).head, (1, 4));
        assert!(!buf.extend_block(Motion::DocStart, 10, 4));
    }

    #[test]
    fn ctrl_shift_word_right_extends_each_row_by_word() {
        let mut buf = b("foo bar baz\nfo\nfoo bar baz\n");
        buf.view.set_sel(Selection::point(0));
        assert!(buf.extend_block(Motion::Down, 10, 4));
        assert!(buf.extend_block(Motion::Down, 10, 4));
        assert!(buf.extend_block(Motion::WordRight, 10, 4));
        assert_eq!(texts(&buf), ["foo ", "fo", "foo "], "the short row stops at its end");
        assert!(buf.extend_block(Motion::WordRight, 10, 4));
        assert_eq!(texts(&buf), ["foo bar ", "fo", "foo bar "]);
        assert_eq!(buf.current_block(4).head, (2, 8));
        assert!(buf.extend_block(Motion::WordLeft, 10, 4));
        assert_eq!(buf.current_block(4).head, (2, 4));
    }

    #[test]
    fn word_left_past_line_width_snaps_to_width() {
        let mut buf = b("foo bar\nab\n");
        buf.set_block(Block { anchor: (0, 0), head: (1, 6) }, 4);
        assert!(buf.extend_block(Motion::WordRight, 10, 4));
        assert_eq!(buf.current_block(4).head, (1, 6), "right past the width leaves it");
        assert!(buf.extend_block(Motion::WordLeft, 10, 4));
        assert_eq!(buf.current_block(4).head, (1, 2));
    }

    #[test]
    fn typing_into_a_block_edits_every_line_and_undoes_at_once() {
        let mut buf = b(TEXT);
        buf.set_block(Block { anchor: (0, 4), head: (3, 6) }, 4);
        buf.insert("Z", TxKind::Typing);
        assert_eq!(buf.text(), "\tZ日本語x\n\tZ日本語x\nabZ\n\tZ日本語x\n");
        assert!(buf.view.block.is_none(), "an edit ends the block");
        // The carets stay a block for Shift+Down.
        assert!(buf.extend_block(Motion::Up, 10, 4));
        assert_eq!(buf.view.sels.len(), 3);
        buf.insert("Y", TxKind::Typing);
        assert_eq!(buf.text(), "\tZY日本語x\n\tZY日本語x\nabZY\n\tZ日本語x\n");
        assert!(buf.undo());
        assert!(buf.undo());
        assert_eq!(buf.text(), TEXT);
        assert_eq!(texts(&buf), ["ab", "ab", "", "ab"], "undo puts the block's selections back");
    }

    #[test]
    fn typing_in_a_block_pads_short_lines() {
        let make = || {
            let mut buf = b("abcdef\nab\nabcdef\n");
            buf.set_block(Block { anchor: (0, 4), head: (2, 4) }, 4);
            buf
        };
        let buf = make();
        assert_eq!(buf.virtual_pads(4), [0, 2, 0]);
        let mut buf = make();
        buf.virtual_pad = buf.virtual_pads(4);
        buf.insert("X", TxKind::Typing);
        assert_eq!(buf.rope.to_string(), "abcdXef\nab  X\nabcdXef\n");
        let heads: Vec<usize> = buf.view.sels.iter().map(|s| s.head).collect();
        assert_eq!(heads, [5, 13, 19]);
        buf.undo();
        assert_eq!(buf.rope.to_string(), "abcdef\nab\nabcdef\n", "one undo takes pad and text");
        let mut buf = make();
        buf.virtual_pad = buf.virtual_pads(4);
        buf.insert_char('X', &[]);
        assert_eq!(buf.rope.to_string(), "abcdXef\nab  X\nabcdXef\n");
        assert!(buf.virtual_pad.is_empty(), "the pad is used up");
        let mut buf = make();
        buf.insert("Y", TxKind::Typing);
        assert_eq!(buf.rope.to_string(), "abcdYef\nabY\nabcdYef\n", "no pad set: no padding");
    }

    #[test]
    fn no_pads_without_virtual_space() {
        let mut buf = b("abcdef\nab\n");
        assert!(buf.virtual_pads(4).is_empty());
        buf.set_block(Block { anchor: (0, 4), head: (1, 4) }, 4);
        buf.view.set_sel(Selection::point(0));
        assert!(buf.virtual_pads(4).is_empty());
    }

    #[test]
    fn backspace_and_delete_in_virtual_space() {
        let make = |text: &str, a: (usize, usize), h: (usize, usize)| {
            let mut buf = b(text);
            buf.set_block(Block { anchor: a, head: h }, 4);
            buf
        };
        let mut buf = make("abcdef\nab\nabcdef\n", (0, 4), (2, 4));
        assert!(buf.virtual_delete(true, false, &[], 4));
        assert_eq!(buf.rope.to_string(), "abcef\nab\nabcef\n");
        assert_eq!(buf.view.block, Some(Block { anchor: (0, 3), head: (2, 3) }));
        assert_eq!(buf.virtual_pads(4), [0, 1, 0]);
        assert!(buf.virtual_delete(true, false, &[], 4));
        assert_eq!(buf.rope.to_string(), "abef\nab\nabef\n");
        assert_eq!(buf.view.block, Some(Block { anchor: (0, 2), head: (2, 2) }));
        assert!(buf.virtual_pads(4).is_empty());
        assert!(buf.virtual_delete(true, false, &[], 4));
        assert_eq!(buf.rope.to_string(), "aef\na\naef\n");
        assert!(buf.undo());
        assert_eq!(buf.rope.to_string(), "abcdef\nab\nabcdef\n");

        let mut buf = make("abcdef\nab\nabcdef\n", (0, 4), (2, 4));
        assert!(buf.virtual_delete(false, false, &[], 4));
        assert_eq!(buf.rope.to_string(), "abcdf\nab\nabcdf\n");
        assert_eq!(buf.view.block, Some(Block { anchor: (0, 4), head: (2, 4) }));

        for back in [true, false] {
            let mut buf = make("abcdef\nab\nabcdef\nabc\n", (0, 2), (3, 4));
            assert!(buf.virtual_delete(back, false, &[], 4));
            assert_eq!(buf.rope.to_string(), "abef\nab\nabef\nab\n");
            assert_eq!(buf.view.block, Some(Block { anchor: (0, 2), head: (3, 2) }));
        }

        let mut buf = make("ab\nab\n", (0, 5), (1, 5));
        assert!(buf.virtual_delete(true, false, &[], 4));
        assert_eq!(buf.rope.to_string(), "ab\nab\n");
        assert!(!buf.undo());
        assert_eq!(buf.view.block, Some(Block { anchor: (0, 4), head: (1, 4) }));

        let mut buf = make(TEXT, (0, 4), (1, 4));
        assert!(buf.virtual_delete(true, false, &[], 4));
        assert_eq!(buf.rope.to_string(), "ab日本語x\nab日本語x\nab\n\tab日本語x\n");
        assert!(buf.view.block.is_none());

        let mut buf = b("ab\n");
        assert!(!buf.virtual_delete(true, false, &[], 4));
        assert_eq!(buf.rope.to_string(), "ab\n");
    }

    #[test]
    fn ctrl_backspace_and_ctrl_delete_in_virtual_space() {
        let mut buf = b("foo bar\nab\nfoo bar\n");
        buf.set_block(Block { anchor: (0, 7), head: (2, 7) }, 4);
        assert!(buf.virtual_delete(true, true, &[], 4));
        assert_eq!(buf.text(), "foo \nab\nfoo \n");
        assert!(buf.view.sels.iter().all(|s| !s.is_virtual()));
        assert_eq!(buf.view.sels.as_slice()[1].head, 7);
        assert!(buf.undo());
        assert_eq!(buf.text(), "foo bar\nab\nfoo bar\n");
        assert_eq!(buf.view.sels.as_slice()[1].virt.head, Some(7));
        buf.set_block(Block { anchor: (0, 3), head: (2, 3) }, 4);
        assert!(buf.virtual_delete(false, true, &[], 4));
        assert_eq!(buf.text(), "foobar\nab\nfoobar\n");
        assert_eq!(buf.view.sels.as_slice()[1].virt.head, Some(3));
    }

    #[test]
    fn virtual_delete_without_a_block() {
        let mut buf = b("abcdef\nab\n");
        buf.view.sels = Selections::new(vec![Selection::point(3), Selection::virtual_point(9, 5)], 1);
        assert!(buf.virtual_delete(true, false, &[], 4));
        assert_eq!(buf.text(), "abdef\nab\n");
        assert_eq!(buf.view.sels.as_slice()[1].virt.head, Some(4));
        // 4 -> 3 -> 2: the second Backspace lands on the line end.
        for _ in 0..2 {
            assert!(buf.virtual_delete(true, false, &[], 4));
        }
        let s = buf.view.sels.as_slice()[1];
        assert_eq!(visual_col(&buf.rope, s.head, 4), 2);
        assert_eq!(s.virt, Virt::default());
        assert_eq!(buf.text(), "def\nab\n", "only line 0 lost chars");
    }

    #[test]
    fn no_virtual_space_under_soft_wrap() {
        let mut buf = b("abcdef\nab\nabcdef\n");
        buf.set_block(Block { anchor: (0, 4), head: (2, 4) }, 4);
        buf.view.wrap.on = true;
        assert!(buf.virtual_pads(4).is_empty());
        assert!(!buf.virtual_delete(true, false, &[], 4));
    }

    #[test]
    fn current_block_starts_at_virtual_columns() {
        let mut buf = b("abcdefgh\nab\nabcdefgh\n");
        buf.view.set_sel(Selection::virtual_point(11, 6));
        assert!(buf.extend_block(Motion::Down, 10, 4));
        assert_eq!(buf.current_block(4), Block { anchor: (1, 6), head: (2, 6) });
    }
}
