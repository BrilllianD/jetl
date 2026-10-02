//! Cell-level rendering of a buffer and the inverse mapping from screen
//! cells back to char positions. Both go through [`layout_line`] so they
//! can never disagree.

use std::borrow::Cow;

use ratatui::buffer::Buffer as Screen;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ropey::Rope;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::buffer::Buffer;
use super::selection::{line_len_no_eol, pos_to_line_col};
use crate::config::theme::Theme;

/// One rendered unit: a grapheme (or tab / control char expansion).
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub char_idx: usize,
    pub col: usize,
    pub width: usize,
}

/// Walk the graphemes of `line` (without its line break). The callback gets
/// the text to draw and the cell geometry; return `false` to stop early.
pub fn layout_line(rope: &Rope, line: usize, tab_width: usize, max_chars: usize, f: &mut dyn FnMut(&str, Cell) -> bool) {
    if line >= rope.len_lines() {
        return;
    }
    let n = line_len_no_eol(rope, line);
    let start = rope.line_to_char(line);
    let take = n.min(max_chars);
    let slice = rope.slice(start..start + take);
    let text: Cow<str> = slice.into();
    let mut col = 0usize;
    let mut idx = start;
    let tab_width = tab_width.max(1);
    for g in text.graphemes(true) {
        let nchars = g.chars().count();
        if g == "\t" {
            let w = tab_width - (col % tab_width);
            if !f("", Cell { char_idx: idx, col, width: w }) {
                return;
            }
            col += w;
        } else if let Some(c) = g.chars().next().filter(|c| c.is_control()) {
            let shown = control_repr(c);
            let w = shown.len();
            if !f(&shown, Cell { char_idx: idx, col, width: w }) {
                return;
            }
            col += w;
        } else {
            let w = g.width();
            if !f(g, Cell { char_idx: idx, col, width: w }) {
                return;
            }
            col += w;
        }
        idx += nchars;
    }
}

fn control_repr(c: char) -> String {
    let code = c as u32;
    if code < 32 {
        format!("^{}", char::from_u32(code + 64).unwrap_or('?'))
    } else if code == 127 {
        "^?".into()
    } else {
        format!("\\u{{{code:x}}}")
    }
}

/// Visual column (in cells) of char position `pos`.
pub fn visual_col(rope: &Rope, pos: usize, tab_width: usize) -> usize {
    let (line, _) = pos_to_line_col(rope, pos);
    let mut result = 0;
    let mut last_end = 0;
    layout_line(rope, line, tab_width, usize::MAX, &mut |_, c| {
        if c.char_idx >= pos {
            result = c.col;
            return false;
        }
        last_end = c.col + c.width;
        result = last_end;
        true
    });
    result
}

/// Char position for a click at visual column `target` on `line`.
pub fn col_to_char(rope: &Rope, line: usize, target: usize, tab_width: usize) -> usize {
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    let mut result = rope.line_to_char(line) + line_len_no_eol(rope, line);
    layout_line(rope, line, tab_width, usize::MAX, &mut |_, c| {
        if c.col + c.width > target {
            // Clicking the right half of a wide char still lands on it.
            result = c.char_idx;
            return false;
        }
        true
    });
    result
}

pub fn gutter_width(rope: &Rope) -> u16 {
    let digits = rope.len_lines().max(1).to_string().len().max(3) as u16;
    digits + 3 // space, diff mark, space
}

pub struct RenderOut {
    pub cursor: Option<(u16, u16)>,
    pub text_area: Rect,
}

pub struct RenderOpts<'a> {
    pub focused: bool,
    pub tab_width: usize,
    pub scrolloff: usize,
    pub hscrolloff: usize,
    /// Highlight occurrences of the word or selected text at the caret.
    pub occurrences: bool,
    pub theme: &'a Theme,
}

/// Scroll so that the caret is visible, then render.
pub fn render(buf: &mut Buffer, screen: &mut Screen, area: Rect, o: &RenderOpts) -> RenderOut {
    let gutter_w = gutter_width(&buf.rope);
    let text_area = Rect::new(area.x + gutter_w, area.y, area.width.saturating_sub(gutter_w), area.height);
    if text_area.width == 0 || area.height == 0 {
        return RenderOut { cursor: None, text_area };
    }
    let follow_key = (buf.sel.head, buf.version, text_area.width, text_area.height);
    if buf.follow_key != Some(follow_key) {
        ensure_visible(buf, text_area, o);
        buf.follow_key = Some(follow_key);
    }

    let theme = o.theme;
    let rope = &buf.rope;
    let sel = buf.sel.range();
    let (cur_line, _) = pos_to_line_col(rope, buf.sel.head);
    let cur_vcol = visual_col(rope, buf.sel.head, o.tab_width);
    let hl = buf.syntax.as_mut().map(|s| s.highlights(rope, buf.scroll_line, area.height as usize)).unwrap_or_default();
    let search = buf.search.as_ref();
    let pair = super::brackets::pair_at(rope, buf.syntax.as_ref().and_then(|s| s.tree()), buf.sel.head);
    // Occurrences of the word or selection at the caret, visible lines only.
    // An open find bar owns the match highlighting, so they step aside.
    let occ = match (o.occurrences && search.is_none()).then(|| super::search::occurrence_target(rope, &buf.sel)).flatten() {
        Some((target, whole_word)) => super::search::occurrences(rope, buf.scroll_line..buf.scroll_line + area.height as usize, &target, whole_word),
        None => Vec::new(),
    };
    let digits = (gutter_w - 3) as usize;
    let mut cursor = None;

    for row in 0..area.height {
        let y = area.y + row;
        let line = buf.scroll_line + row as usize;
        if line >= rope.len_lines() {
            break;
        }
        let is_cur = line == cur_line;
        // Gutter: line number and diff mark.
        let num_style = if is_cur { theme.line_number_current } else { theme.line_number };
        let num = format!("{:>width$}", line + 1, width = digits);
        screen.set_stringn(area.x, y, &num, digits, num_style);
        if let Some(mark) = super::gutter::mark_at(&buf.gutter, line) {
            let (glyph, color) = mark.glyph(theme);
            screen.set_string(area.x + digits as u16 + 1, y, glyph, Style::default().fg(color));
        }
        if is_cur && o.focused {
            screen.set_style(Rect::new(text_area.x, y, text_area.width, 1), Style::default().bg(theme.current_line_bg));
        }
        let line_start = rope.line_to_char(line);
        let line_end = line_start + line_len_no_eol(rope, line);
        let max_chars = buf.scroll_col + text_area.width as usize + 256;
        let scroll_col = buf.scroll_col;
        let right = text_area.right();
        layout_line(rope, line, o.tab_width, max_chars, &mut |g, c| {
            if c.col + c.width <= scroll_col {
                return true;
            }
            let x = text_area.x as usize + c.col.saturating_sub(scroll_col);
            if x >= right as usize {
                return false;
            }
            let x = x as u16;
            let mut style = hl.style_at(c.char_idx).unwrap_or(Style::default().fg(theme.fg));
            if sel.contains(&c.char_idx) {
                style = style.bg(theme.selection_bg);
            } else if let Some(s) = search {
                if let Some(cur) = s.match_at(c.char_idx) {
                    style = style.bg(if cur { theme.search_current_bg } else { theme.search_match_bg });
                }
            } else if in_ranges(&occ, c.char_idx) {
                style = style.bg(theme.occurrence_bg);
            }
            if pair.is_some_and(|(a, b)| c.char_idx == a || c.char_idx == b) {
                style = style.patch(theme.bracket_match);
            }
            if is_cur && o.focused && style.bg.is_none() {
                style = style.bg(theme.current_line_bg);
            }
            if c.width == 0 {
                return true;
            }
            if g.is_empty() {
                // Tab: fill with spaces.
                for i in 0..c.width {
                    let xi = x + i as u16;
                    if xi >= right {
                        break;
                    }
                    screen[(xi, y)].set_symbol(" ").set_style(style);
                }
            } else if g.starts_with('^') && c.width == 2 && g.len() == 2 {
                screen.set_stringn(x, y, g, (right - x) as usize, style.add_modifier(Modifier::DIM));
            } else {
                if x + c.width as u16 > right {
                    return false;
                }
                screen[(x, y)].set_symbol(g).set_style(style);
                for i in 1..c.width as u16 {
                    screen[(x + i, y)].reset();
                }
            }
            true
        });
        // Selection that spans the line break: mark one cell past the text.
        if !sel.is_empty() && sel.start <= line_end && sel.end > line_end && line + 1 < rope.len_lines() {
            let vc = visual_col(rope, line_end, o.tab_width);
            if vc >= scroll_col {
                let x = text_area.x + (vc - scroll_col) as u16;
                if x < right {
                    screen[(x, y)].set_style(Style::default().bg(theme.selection_bg));
                }
            }
        }
        if is_cur && cur_vcol >= scroll_col {
            let x = text_area.x + (cur_vcol - scroll_col) as u16;
            if x < right {
                cursor = Some((x, y));
            }
        }
    }
    RenderOut { cursor, text_area }
}

/// `ranges` is sorted and non-overlapping.
fn in_ranges(ranges: &[std::ops::Range<usize>], i: usize) -> bool {
    let k = ranges.partition_point(|r| r.end <= i);
    ranges.get(k).is_some_and(|r| r.contains(&i))
}

pub fn ensure_visible(buf: &mut Buffer, text_area: Rect, o: &RenderOpts) {
    let rows = text_area.height as usize;
    let cols = text_area.width as usize;
    if rows == 0 || cols == 0 {
        return;
    }
    let (line, _) = pos_to_line_col(&buf.rope, buf.sel.head);
    let so = o.scrolloff.min(rows.saturating_sub(1) / 2);
    if line < buf.scroll_line + so {
        buf.scroll_line = line.saturating_sub(so);
    } else if line + so >= buf.scroll_line + rows {
        buf.scroll_line = line + so + 1 - rows;
    }
    let max_scroll = buf.rope.len_lines().saturating_sub(1);
    buf.scroll_line = buf.scroll_line.min(max_scroll);

    let vcol = visual_col(&buf.rope, buf.sel.head, o.tab_width);
    let ho = o.hscrolloff.min(cols.saturating_sub(1) / 2);
    if vcol < buf.scroll_col + ho {
        buf.scroll_col = vcol.saturating_sub(ho);
    } else if vcol + ho >= buf.scroll_col + cols {
        buf.scroll_col = vcol + ho + 1 - cols;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_mapping_with_tabs_and_wide_chars() {
        let r = Rope::from_str("\tab日本語x\n");
        // tab (cols 0-3), a(4), b(5), 日(6-7), 本(8-9), 語(10-11), x(12)
        assert_eq!(col_to_char(&r, 0, 0, 4), 0);
        assert_eq!(col_to_char(&r, 0, 3, 4), 0);
        assert_eq!(col_to_char(&r, 0, 4, 4), 1);
        assert_eq!(col_to_char(&r, 0, 7, 4), 3); // right half of 日
        assert_eq!(col_to_char(&r, 0, 12, 4), 6);
        assert_eq!(col_to_char(&r, 0, 40, 4), 7); // past end -> line end
        assert_eq!(visual_col(&r, 3, 4), 6);
        assert_eq!(visual_col(&r, 7, 4), 13);
    }

    #[test]
    fn render_and_map_agree() {
        let r = Rope::from_str("x\ty\t\tz 😀 end");
        for target in 0..30 {
            let ch = col_to_char(&r, 0, target, 4);
            let vc = visual_col(&r, ch, 4);
            assert!(vc <= target, "col {target}: char {ch} starts at {vc}");
        }
    }
}
