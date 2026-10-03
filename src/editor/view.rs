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
use super::fold::Fold;
use super::rows::Rows;
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
/// [`MAX_CARETS`] lines, the ones nearest the head. Lines a fold hides
/// get none (but the head's line always has one).
///
/// Rows are buffer lines. Word wrap (yman 106) would make a line several
/// rows; `Buffer::blocks_supported` is where it refuses column selection
/// until blocks are made of visual rows here.
pub fn block_selections(rope: &Rope, rows: &Rows, anchor: (usize, usize), head: (usize, usize), tab_width: usize) -> Selections {
    let last = rope.len_lines().saturating_sub(1);
    let hl = head.0.min(last);
    let al = anchor.0.min(last).clamp(hl.saturating_sub(MAX_CARETS - 1), hl + MAX_CARETS - 1);
    let (lo, hi) = (anchor.1.min(head.1), anchor.1.max(head.1));
    let forward = anchor.1 <= head.1;
    let lines: Vec<usize> = (al.min(hl)..=al.max(hl)).filter(|&l| l == hl || !rows.is_hidden(l)).collect();
    let primary = lines.iter().position(|&l| l == hl).unwrap_or(0);
    let ranges = lines
        .into_iter()
        .map(|line| {
            let start = col_to_char(rope, line, lo, tab_width);
            if lo == hi {
                return Selection::point(start);
            }
            let end = col_to_char_ceil(rope, line, hi, tab_width).max(start);
            if forward { Selection::new(start, end) } else { Selection::new(end, start) }
        })
        .collect();
    Selections::new(ranges, primary)
}

/// Line numbers, bookmark, diff mark, fold marker, space; and the blame
/// column first when shown.
pub fn gutter_width(buf: &Buffer) -> u16 {
    let digits = buf.rope.len_lines().max(1).to_string().len().max(3) as u16;
    let blame = if buf.blame.is_some() { super::blame::WIDTH } else { 0 };
    blame + digits + 4
}

/// At most this many cells of fold placeholder.
const PLACEHOLDER_MAX: usize = 20;

/// What a collapsed fold shows after its header line: `…`, then the
/// closing line when the fold hides it (`fn main() {…}`).
pub fn placeholder(rope: &Rope, f: Fold, last: usize) -> String {
    let mut s = String::from("…");
    if rope.char_to_line(f.end.min(rope.len_chars())) == last {
        let text = rope.line(last).to_string();
        let t = text.trim();
        let word = |w: &str| t.strip_prefix(w).is_some_and(|r| !r.starts_with(|c: char| c.is_alphanumeric() || c == '_'));
        if t.starts_with(['}', ']', ')']) || ["end", "fi", "done", "esac"].into_iter().any(word) {
            s.push_str(t);
        }
    }
    let mut w = 0;
    s.chars()
        .take_while(|c| {
            w += c.to_string().width();
            w <= PLACEHOLDER_MAX
        })
        .collect()
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
    // A caret moved into a fold from outside the editor (go to definition,
    // F2, find in files, bookmarks, back/forward) unfolds it; the top line
    // is never hidden.
    let v = &mut buf.view;
    v.folds.reveal(&buf.rope, &v.sels);
    v.scroll_line = Rows::new(&v.folds, buf.rope.len_lines()).visible(v.scroll_line);
    let follow_key = (buf.view.sel().head, buf.version, text_area.width, text_area.height);
    if buf.view.follow_key != Some(follow_key) {
        ensure_visible(buf, text_area, o);
        buf.view.follow_key = Some(follow_key);
    }
    let shown: Vec<usize> = {
        let rows = Rows::new(&buf.view.folds, buf.rope.len_lines());
        rows.iter_from(buf.view.scroll_line).take(area.height as usize).map(|r| r.line).collect()
    };
    // The lines from the top to the bottom row, hidden ones included.
    let window = buf.view.scroll_line..shown.last().map_or(buf.view.scroll_line, |l| l + 1);
    // Headers that can fold get a marker.
    let foldable: Vec<usize> = buf.fold_candidates(window.clone()).iter().map(|f| buf.rope.char_to_line(f.start)).collect();

    let theme = o.theme;
    let rope = &buf.rope;
    let rows = Rows::new(&buf.view.folds, rope.len_lines());
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
            let line = rope.char_to_line(h.min(rope.len_chars()));
            window.contains(&line) && !rows.is_hidden(line)
        })
        .collect();
    let cur_vcol = visual_col(rope, buf.view.sel().head, o.tab_width);
    let hl = buf.syntax.as_mut().map(|s| s.highlights(rope, window.start, window.len())).unwrap_or_default();
    let search = buf.view.search.as_ref();
    let pair = super::brackets::pair_at(rope, buf.syntax.as_ref().and_then(|s| s.tree()), buf.view.sel().head);
    // Occurrences of the word or selection at the caret, visible lines only.
    // An open find bar owns the match highlighting, so they step aside.
    let occ = match (o.occurrences && search.is_none()).then(|| super::search::occurrence_target(rope, &buf.view.sel())).flatten() {
        Some((target, whole_word)) => super::search::occurrences(rope, window.clone(), &target, whole_word),
        None => Vec::new(),
    };
    let marks = &buf.diag_marks;
    let underlines = !marks.spans.is_empty();
    let digits = (gutter_w - 4 - blame_w) as usize;
    let gx = area.x + blame_w;
    let mut cursor = None;

    for (row, &line) in shown.iter().enumerate() {
        let row = row as u16;
        let y = area.y + row;
        let line_start = rope.line_to_char(line);
        let line_end = line_start + line_len_no_eol(rope, line);
        let here = &drawn[drawn.partition_point(|&h| h < line_start)..drawn.partition_point(|&h| h <= line_end)];
        // Every caret line of the focused view counts as current.
        let is_cur = line == cur_line || (o.focused && !here.is_empty());
        // Gutter: line number, bookmark, diff mark, fold marker. A folded
        // line shows the marks of the lines it hides too.
        let folded = buf.view.folds.header_at(line);
        let span = line..=folded.map_or(line, |(_, last)| last);
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
        if buf.bookmarks.range(span.clone()).next().is_some() {
            screen.set_string(gx + digits as u16, y, "◆", theme.bookmark);
        }
        // A diagnostic's glyph takes the diff mark's column over it.
        let git_mark = buf.gutter.get(buf.gutter.partition_point(|m| m.line < line)).filter(|m| m.line <= *span.end());
        if let Some(sev) = marks.lines.range(span.clone()).map(|(_, s)| *s).min() {
            let st = sev.style(theme);
            screen.set_string(gx + digits as u16 + 1, y, sev.glyph(), Style::default().fg(st.fg.unwrap_or(theme.fg)));
        } else if let Some(mark) = git_mark {
            let (glyph, color) = mark.glyph(theme);
            screen.set_string(gx + digits as u16 + 1, y, glyph, Style::default().fg(color));
        }
        if folded.is_some() {
            screen.set_string(gx + digits as u16 + 2, y, "▸", theme.line_number);
        } else if foldable.binary_search(&line).is_ok() {
            screen.set_string(gx + digits as u16 + 2, y, "▾", theme.line_number);
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
        if let Some((f, last)) = folded {
            draw_placeholder(screen, buf, (f, line, last), text_area, y, o.tab_width, theme);
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

/// The placeholder of collapsed fold `f` (header `line`, hiding up to
/// `last`) on screen row `y` of `text_area`, from the header line's end.
/// It takes the selection's background when the selection reaches into the
/// fold, else a hidden find match's, and the worst hidden diagnostic's mark.
fn draw_placeholder(
    screen: &mut Screen,
    buf: &Buffer,
    (f, line, last): (Fold, usize, usize),
    text_area: Rect,
    y: u16,
    tab_width: usize,
    theme: &Theme,
) {
    let rope = &buf.rope;
    let (lo, hi) = (rope.line_to_char(line + 1), rope.line_to_char(last) + line_len_no_eol(rope, last));
    let mut style = theme.fold_placeholder;
    if buf.view.sels.contains(lo) {
        style = style.bg(theme.selection_bg);
    } else if let Some(s) = &buf.view.search {
        let k = s.matches.partition_point(|r| r.end <= lo);
        if s.matches.get(k).is_some_and(|r| r.start <= hi) {
            style = style.bg(theme.search_match_bg);
        }
    }
    if let Some(sev) = buf.diag_marks.lines.range(line + 1..=last).map(|(_, s)| *s).min() {
        style = diag_overlay(style, sev.style(theme));
    }
    let mut col = visual_col(rope, rope.line_to_char(line) + line_len_no_eol(rope, line), tab_width);
    for g in placeholder(rope, f, last).graphemes(true) {
        let w = g.width();
        if col >= buf.view.scroll_col {
            let x = text_area.x as usize + col - buf.view.scroll_col;
            if x + w > text_area.right() as usize {
                break;
            }
            screen.set_stringn(x as u16, y, g, w, style);
        }
        col += w;
    }
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
    // Distances are visible rows: hidden lines take none.
    let r = Rows::new(&buf.view.folds, buf.rope.len_lines());
    let (line, top) = (r.visible(line), r.visible(buf.view.scroll_line));
    if line < top || r.distance(top, line) < so {
        buf.view.scroll_line = r.step(line, -(so as isize));
    } else if r.distance(top, line) + so >= rows {
        buf.view.scroll_line = r.step(line, -((rows - 1 - so) as isize));
    }
    buf.view.scroll_line = r.visible(buf.view.scroll_line);

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
