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
use super::selection::{MAX_CARETS, Selection, Selections, line_len_no_eol, pos_to_line_col};
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

/// Char position of the first grapheme on `line` that starts at visual
/// column `target` or later, else the line end. With [`col_to_char`] it
/// bounds a column block: a grapheme either edge cuts through is inside.
pub fn col_to_char_ceil(rope: &Rope, line: usize, target: usize, tab_width: usize) -> usize {
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    let mut result = rope.line_to_char(line) + line_len_no_eol(rope, line);
    layout_line(rope, line, tab_width, usize::MAX, &mut |_, c| {
        if c.col >= target {
            result = c.char_idx;
            return false;
        }
        true
    });
    result
}

/// The carets of a column block from `anchor` to `head`, each a (line,
/// visual column). Every line between gets the cells between the two
/// columns, a grapheme cut by either edge included; a line too short to
/// reach the block gets a caret at its end (there is no virtual space), and
/// a zero-width block is a caret on every line. Each selection points the
/// way the block does and the head's line holds the primary. At most
/// [`MAX_CARETS`] lines, the ones nearest the head.
///
/// Rows are buffer lines. Word wrap (yman 106) would make a line several
/// rows; `Buffer::blocks_supported` is where it refuses column selection
/// until blocks are made of visual rows here.
pub fn block_selections(rope: &Rope, anchor: (usize, usize), head: (usize, usize), tab_width: usize) -> Selections {
    let last = rope.len_lines().saturating_sub(1);
    let hl = head.0.min(last);
    let al = anchor.0.min(last).clamp(hl.saturating_sub(MAX_CARETS - 1), hl + MAX_CARETS - 1);
    let (lo, hi) = (anchor.1.min(head.1), anchor.1.max(head.1));
    let forward = anchor.1 <= head.1;
    let ranges = (al.min(hl)..=al.max(hl))
        .map(|line| {
            let start = col_to_char(rope, line, lo, tab_width);
            if lo == hi {
                return Selection::point(start);
            }
            let end = col_to_char_ceil(rope, line, hi, tab_width).max(start);
            if forward { Selection::new(start, end) } else { Selection::new(end, start) }
        })
        .collect();
    Selections::new(ranges, hl - al.min(hl))
}

/// Line numbers plus space, diff mark, space; and the blame column first
/// when shown.
pub fn gutter_width(buf: &Buffer) -> u16 {
    let digits = buf.rope.len_lines().max(1).to_string().len().max(3) as u16;
    let blame = if buf.blame.is_some() { super::blame::WIDTH } else { 0 };
    blame + digits + 3
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
    let gutter_w = gutter_width(buf);
    let blame_w = if buf.blame.is_some() { super::blame::WIDTH } else { 0 };
    let text_area = Rect::new(area.x + gutter_w, area.y, area.width.saturating_sub(gutter_w), area.height);
    if text_area.width == 0 || area.height == 0 {
        return RenderOut { cursor: None, text_area };
    }
    let follow_key = (buf.view.sel().head, buf.version, text_area.width, text_area.height);
    if buf.view.follow_key != Some(follow_key) {
        ensure_visible(buf, text_area, o);
        buf.view.follow_key = Some(follow_key);
    }

    let theme = o.theme;
    let rope = &buf.rope;
    let sels = &buf.view.sels;
    let (cur_line, _) = pos_to_line_col(rope, buf.view.sel().head);
    // The carets drawn as cells: the others, and the primary too in a view
    // without the terminal cursor. Sorted, like the selections.
    let paint_primary = !o.focused && sels.is_multi();
    let drawn: Vec<usize> = sels
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != sels.primary_index() || paint_primary)
        .map(|(_, s)| s.head)
        .filter(|&h| {
            (buf.view.scroll_line..buf.view.scroll_line + area.height as usize).contains(&rope.char_to_line(h.min(rope.len_chars())))
        })
        .collect();
    let cur_vcol = visual_col(rope, buf.view.sel().head, o.tab_width);
    let hl = buf.syntax.as_mut().map(|s| s.highlights(rope, buf.view.scroll_line, area.height as usize)).unwrap_or_default();
    let search = buf.view.search.as_ref();
    let pair = super::brackets::pair_at(rope, buf.syntax.as_ref().and_then(|s| s.tree()), buf.view.sel().head);
    // Occurrences of the word or selection at the caret, visible lines only.
    // An open find bar owns the match highlighting, so they step aside.
    let occ = match (o.occurrences && search.is_none()).then(|| super::search::occurrence_target(rope, &buf.view.sel())).flatten() {
        Some((target, whole_word)) => {
            super::search::occurrences(rope, buf.view.scroll_line..buf.view.scroll_line + area.height as usize, &target, whole_word)
        }
        None => Vec::new(),
    };
    let marks = &buf.diag_marks;
    let underlines = !marks.spans.is_empty();
    let digits = (gutter_w - 3 - blame_w) as usize;
    let gx = area.x + blame_w;
    let mut cursor = None;

    for row in 0..area.height {
        let y = area.y + row;
        let line = buf.view.scroll_line + row as usize;
        if line >= rope.len_lines() {
            break;
        }
        let line_start = rope.line_to_char(line);
        let line_end = line_start + line_len_no_eol(rope, line);
        let here = &drawn[drawn.partition_point(|&h| h < line_start)..drawn.partition_point(|&h| h <= line_end)];
        // Every caret line of the focused view counts as current.
        let is_cur = line == cur_line || (o.focused && !here.is_empty());
        // Gutter: line number and diff mark.
        let num_style = if is_cur { theme.line_number_current } else { theme.line_number };
        if let Some(b) = &buf.blame {
            let label = match b.commit_at(line) {
                Some(c) => Some(super::blame::label(c)),
                None if b.data.is_none() && row == 0 => Some("blame…".to_string()),
                None => None,
            };
            if let Some(label) = label {
                screen.set_stringn(area.x, y, label, blame_w as usize - 1, theme.dim);
            }
        }
        let num = format!("{:>width$}", line + 1, width = digits);
        screen.set_stringn(gx, y, &num, digits, num_style);
        if buf.bookmarks.contains(&line) {
            screen.set_string(gx + digits as u16, y, "◆", theme.bookmark);
        }
        // A diagnostic's glyph takes the diff mark's column over it.
        if let Some(sev) = marks.lines.get(&line) {
            let st = sev.style(theme);
            screen.set_string(gx + digits as u16 + 1, y, sev.glyph(), Style::default().fg(st.fg.unwrap_or(theme.fg)));
        } else if let Some(mark) = super::gutter::mark_at(&buf.gutter, line) {
            let (glyph, color) = mark.glyph(theme);
            screen.set_string(gx + digits as u16 + 1, y, glyph, Style::default().fg(color));
        }
        if is_cur && o.focused {
            screen.set_style(Rect::new(text_area.x, y, text_area.width, 1), Style::default().bg(theme.current_line_bg));
        }
        let max_chars = buf.view.scroll_col + text_area.width as usize + 256;
        let scroll_col = buf.view.scroll_col;
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
            if underlines && let Some(sev) = marks.severity_at(c.char_idx) {
                style = diag_overlay(style, sev.style(theme));
            }
            if sels.contains(c.char_idx) {
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
            if here.binary_search(&c.char_idx).is_ok() {
                style = style.patch(theme.secondary_caret);
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
        // Selection that spans the line break: mark one cell past the text;
        // a caret at the line end is drawn there too.
        let at_end = here.last() == Some(&line_end);
        if (sels.contains(line_end) && line + 1 < rope.len_lines()) || at_end {
            let vc = visual_col(rope, line_end, o.tab_width);
            if vc >= scroll_col {
                let x = text_area.x + (vc - scroll_col) as u16;
                if x < right {
                    if sels.contains(line_end) && line + 1 < rope.len_lines() {
                        screen[(x, y)].set_style(Style::default().bg(theme.selection_bg));
                    }
                    if at_end {
                        screen[(x, y)].set_style(theme.secondary_caret);
                    }
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

/// Text under a diagnostic: the severity style's modifiers (an underline
/// in its foreground color by default) and its background, if any.
fn diag_overlay(style: Style, d: Style) -> Style {
    let mut s = style.add_modifier(d.add_modifier).remove_modifier(d.sub_modifier);
    if d.add_modifier.contains(Modifier::UNDERLINED)
        && let Some(fg) = d.fg
    {
        s = s.underline_color(fg);
    }
    if let Some(bg) = d.bg {
        s = s.bg(bg);
    }
    s
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
    let (line, _) = pos_to_line_col(&buf.rope, buf.view.sel().head);
    let so = o.scrolloff.min(rows.saturating_sub(1) / 2);
    if line < buf.view.scroll_line + so {
        buf.view.scroll_line = line.saturating_sub(so);
    } else if line + so >= buf.view.scroll_line + rows {
        buf.view.scroll_line = line + so + 1 - rows;
    }
    let max_scroll = buf.rope.len_lines().saturating_sub(1);
    buf.view.scroll_line = buf.view.scroll_line.min(max_scroll);

    let vcol = visual_col(&buf.rope, buf.view.sel().head, o.tab_width);
    let ho = o.hscrolloff.min(cols.saturating_sub(1) / 2);
    if vcol < buf.view.scroll_col + ho {
        buf.view.scroll_col = vcol.saturating_sub(ho);
    } else if vcol + ho >= buf.view.scroll_col + cols {
        buf.view.scroll_col = vcol + ho + 1 - cols;
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
