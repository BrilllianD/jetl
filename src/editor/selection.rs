//! Cursor/selection model and movement over a rope. Positions are char
//! indices; `head` is the caret, `anchor` the other end of the selection.

use std::ops::Range;

use ropey::Rope;

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

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
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

pub fn apply_motion(rope: &Rope, sel: &mut Selection, m: Motion, extend: bool, desired_col: &mut Option<usize>, page: usize) {
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
            let target = match m {
                Motion::Up => line.saturating_sub(1),
                Motion::Down => (line + 1).min(rope.len_lines().saturating_sub(1)),
                Motion::PageUp => line.saturating_sub(page.max(1)),
                _ => (line + page.max(1)).min(rope.len_lines().saturating_sub(1)),
            };
            if target == line && matches!(m, Motion::Up | Motion::PageUp) {
                0
            } else if target == line {
                len
            } else {
                line_col_to_pos(rope, target, col)
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn up_down_keep_desired_column() {
        let r = Rope::from_str("longer line\nab\nlonger line");
        let mut sel = Selection::point(8);
        let mut d = None;
        apply_motion(&r, &mut sel, Motion::Down, false, &mut d, 10);
        assert_eq!(sel.head, 12 + 2);
        apply_motion(&r, &mut sel, Motion::Down, false, &mut d, 10);
        assert_eq!(sel.head, 15 + 8);
    }
}
