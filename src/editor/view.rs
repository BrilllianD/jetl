//! Cell-level rendering of a buffer and the inverse mapping from screen
//! cells back to char positions. Both go through [`layout_line`] so they
//! can never disagree.

use std::borrow::Cow;
use std::ops::Range;

use ratatui::buffer::Buffer as Screen;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ropey::Rope;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::buffer::Buffer;
use super::fold::Fold;
use super::rows::{Row, Rows};
use super::selection::{MAX_CARETS, Selection, Selections, line_len_no_eol, pos_to_line_col};
use crate::config::theme::Theme;

/// One rendered unit: a grapheme (or the expansion of a tab, a control char
/// or a U+2028/U+2029 separator, drawn as an escape).
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
        } else if let Some(c) = g.chars().next().filter(|&c| crate::util::text::shown_escaped(c)) {
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

/// The escape drawn for a char `shown_escaped` picks: `^X` for C0 controls
/// and DEL, `\u{…}` for the rest (C1 controls such as NEL, U+2028, U+2029).
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

/// Where the cells of a screen row go. A cell's row x (its column on the
/// row before any sideways scroll) is `indent + col - origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowGeom {
    /// The row's chars: `start..end`.
    pub start: usize,
    pub end: usize,
    /// Visual column of `start` in the unwrapped line.
    pub origin: usize,
    /// Cells the row is shifted right by (wrap indent).
    pub indent: usize,
    /// The line's last row: the line end is on it.
    pub last: bool,
}

pub fn row_geom(rope: &Rope, rows: &Rows, row: Row) -> RowGeom {
    let r = rows.seg_range(rope, row);
    match rows.line_wrap(row.line) {
        Some(w) => {
            RowGeom { start: r.start, end: r.end, origin: w.start(row.seg).1, indent: w.indent_of(row.seg), last: row.seg + 1 >= w.segs() }
        }
        None => RowGeom { start: r.start, end: r.end, origin: 0, indent: 0, last: true },
    }
}

/// [`layout_line`] for one screen row: the callback also gets each cell's
/// row x. Without wrap the row is the whole line, walked up to about
/// `cols` cells.
pub fn layout_row(rope: &Rope, rows: &Rows, row: Row, tab_width: usize, cols: usize, f: &mut dyn FnMut(&str, Cell, usize) -> bool) {
    let g = row_geom(rope, rows, row);
    let max_chars =
        if rows.wraps() && row.line < rope.len_lines() { g.end - rope.line_to_char(row.line) } else { cols.saturating_add(256) };
    layout_line(rope, row.line, tab_width, max_chars, &mut |s, c| {
        if c.char_idx < g.start {
            return true;
        }
        if c.char_idx >= g.end {
            return false;
        }
        f(s, c, g.indent + c.col - g.origin)
    });
}

/// The screen row char position `pos` is on and its row x (where the caret
/// there is drawn, before sideways scroll).
///
/// With `upstream` (soft wrap, `pos` at the start of a continuation row) it
/// is the previous row and the x after its last grapheme, unclamped.
pub fn char_to_row_x(rope: &Rope, rows: &Rows, pos: usize, upstream: bool, tab_width: usize) -> (Row, usize) {
    let mut row = rows.row_of(rope, pos);
    if upstream && rows.wraps() && row.seg > 0 && pos == row_geom(rope, rows, row).start {
        row.seg -= 1;
        let mut x = row_geom(rope, rows, row).indent;
        layout_row(rope, rows, row, tab_width, usize::MAX, &mut |_, c, rx| {
            x = rx + c.width;
            true
        });
        return (row, x);
    }
    let mut x = row_geom(rope, rows, row).indent;
    layout_row(rope, rows, row, tab_width, usize::MAX, &mut |_, c, rx| {
        if c.char_idx >= pos {
            x = rx;
            return false;
        }
        x = rx + c.width;
        true
    });
    (row, x)
}

/// Char position and affinity for a click at row x `x` on screen row
/// `row`: the grapheme there (its right half too); left of a continuation
/// row's text, the row's first char; past the text, the line end on the
/// line's last row, else the row's end with the upstream affinity (the
/// caret stays on the row).
pub fn row_x_to_char(rope: &Rope, rows: &Rows, row: Row, x: usize, tab_width: usize) -> (usize, bool) {
    if row.line >= rope.len_lines() {
        return (rope.len_chars(), false);
    }
    let g = row_geom(rope, rows, row);
    if x < g.indent {
        return (g.start, false);
    }
    let mut hit = None;
    layout_row(rope, rows, row, tab_width, usize::MAX, &mut |_, c, rx| {
        if rx + c.width > x {
            hit = Some(c.char_idx);
            return false;
        }
        true
    });
    match hit {
        Some(i) => (i, false),
        None => (g.end, !g.last),
    }
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
/// Rows are buffer lines. Soft wrap makes a line several rows;
/// `Buffer::blocks_supported` is where it refuses column selection until
/// blocks are made of visual rows here.
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

/// Line numbers, bookmark, diff mark (or diagnostic glyph, or lightbulb),
/// fold marker, space; the blame column before them when shown, and first
/// `action_w` cells of [`LineDecor`] glyphs.
pub fn gutter_width(buf: &Buffer, action_w: u16) -> u16 {
    let digits = buf.rope.len_lines().max(1).to_string().len().max(3) as u16;
    let blame = if buf.blame.is_some() { super::blame::WIDTH } else { 0 };
    action_w + blame + digits + 4
}

/// What the conflict resolver adds to a view: line backgrounds and a
/// column of clickable glyphs at the left edge of the gutter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineDecor {
    /// Cells of the glyph column (0: none).
    pub action_w: u16,
    /// Sorted by line.
    pub lines: Vec<DecorLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecorLine {
    /// Lines with the background; may be empty (a chunk with no lines in
    /// this text), the glyphs still go on `lines.start`.
    pub lines: Range<usize>,
    pub bg: Option<Color>,
    /// One cell per char on the first line (spaces keep the places).
    pub glyphs: String,
}

impl LineDecor {
    /// The background of `line`.
    pub fn bg_at(&self, line: usize) -> Option<Color> {
        let k = self.lines.partition_point(|d| d.lines.end <= line);
        self.lines[k..].iter().take_while(|d| d.lines.start <= line).find(|d| d.lines.contains(&line)).and_then(|d| d.bg)
    }

    /// The glyphs shown on `line`.
    pub fn glyphs_at(&self, line: usize) -> Option<&str> {
        self.lines.iter().find(|d| d.lines.start == line && !d.glyphs.trim().is_empty()).map(|d| d.glyphs.as_str())
    }
}

/// At most this many cells of fold placeholder.
const PLACEHOLDER_MAX: usize = 20;

/// The lightbulb glyph: U+2731, one cell in every terminal (the emoji bulb
/// is two and would break the gutter).
const LIGHTBULB: &str = "✱";

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
    /// `editor.soft_wrap_extra_indent`.
    pub wrap_extra_indent: usize,
    pub scrolloff: usize,
    pub hscrolloff: usize,
    /// Highlight occurrences of the word or selected text at the caret.
    pub occurrences: bool,
    pub theme: &'a Theme,
    /// Scroll to the caret when it moved (else the scroll stays, as the
    /// resolver's linked panes set it).
    pub follow: bool,
    pub decor: Option<&'a LineDecor>,
}

/// Scroll so that the caret is visible, then render.
pub fn render(buf: &mut Buffer, screen: &mut Screen, area: Rect, o: &RenderOpts) -> RenderOut {
    let action_w = o.decor.map_or(0, |d| d.action_w);
    let gutter_w = gutter_width(buf, action_w);
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
    v.wrap.set_geometry(text_area.width, o.tab_width, o.wrap_extra_indent);
    let top = Rows::view(&v.folds, &v.wrap, &buf.rope, buf.version).clamp(v.top());
    v.set_top(top);
    let follow_key = (buf.view.sel().head, buf.version, text_area.width, text_area.height);
    if buf.view.follow_key != Some(follow_key) {
        // A resize alone re-scrolls only a caret that was on screen and is
        // not any more; one already off screen stays where it was.
        let resized_only = buf.view.follow_key.is_some_and(|k| (k.0, k.1) == (follow_key.0, follow_key.1));
        let was_visible = buf.view.follow_key.is_some_and(|k| caret_on_screen(buf, k.2, k.3, o));
        if o.follow && (!resized_only || (was_visible && !caret_on_screen(buf, text_area.width, text_area.height, o))) {
            ensure_visible(buf, text_area, o);
        }
        buf.view.follow_key = Some(follow_key);
    }
    let shown: Vec<Row> = buf.rows().iter_from(buf.view.top()).take(area.height as usize).collect();
    // The lines from the top to the bottom row, hidden ones included.
    let window = buf.view.scroll_line..shown.last().map_or(buf.view.scroll_line, |r| r.line + 1);
    // Headers that can fold get a marker.
    let foldable: Vec<usize> = buf.fold_candidates(window.clone()).iter().map(|f| buf.rope.char_to_line(f.start)).collect();

    let theme = o.theme;
    let rope = &buf.rope;
    let rows = Rows::view(&buf.view.folds, &buf.view.wrap, rope, buf.version);
    let sels = &buf.view.sels;
    let (cur_line, _) = pos_to_line_col(rope, buf.view.sel().head);
    // The carets drawn as cells: the others, and the primary too in a view
    // without the terminal cursor. Sorted, like the selections.
    let paint_primary = !o.focused && sels.is_multi();
    let visible: Vec<_> = sels
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != sels.primary_index() || paint_primary)
        .map(|(_, s)| *s)
        .filter(|s| {
            let line = rope.char_to_line(s.head.min(rope.len_chars()));
            window.contains(&line) && !rows.is_hidden(line)
        })
        .collect();
    // Upstream heads at a continuation row's start are drawn after the row
    // above's text instead.
    let is_up =
        |s: &Selection| s.upstream && rows.wraps() && char_to_row_x(rope, &rows, s.head, true, o.tab_width).0 != rows.row_of(rope, s.head);
    let drawn_up: Vec<usize> = visible.iter().filter(|s| is_up(s)).map(|s| s.head).collect();
    let drawn: Vec<usize> = visible.iter().filter(|s| !is_up(s)).map(|s| s.head).collect();
    let (cur_row, cur_x) = char_to_row_x(rope, &rows, buf.view.sel().head, buf.view.sel().upstream, o.tab_width);
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
    let digits = (gutter_w - 4 - blame_w - action_w) as usize;
    let bx = area.x + action_w;
    let gx = bx + blame_w;
    // Wrapped views never scroll sideways.
    let scroll_col = if rows.wraps() { 0 } else { buf.view.scroll_col };
    let right = text_area.right();
    let mut cursor = None;

    for (row_i, &row) in shown.iter().enumerate() {
        let line = row.line;
        let row_i = row_i as u16;
        let y = area.y + row_i;
        let line_start = rope.line_to_char(line);
        let line_end = line_start + line_len_no_eol(rope, line);
        let seg = rows.seg_range(rope, row);
        let last_seg = seg.end == line_end;
        let in_line = &drawn[drawn.partition_point(|&h| h < line_start)..drawn.partition_point(|&h| h <= line_end)];
        // The carets on this row: the line end belongs to the last one.
        let here =
            &in_line[in_line.partition_point(|&h| h < seg.start)..in_line.partition_point(|&h| h < seg.end || (last_seg && h == seg.end))];
        // Every caret line of the focused view counts as current, all its
        // rows.
        let is_cur = line == cur_line || (o.focused && !in_line.is_empty());
        // Gutter: line number, bookmark, diff mark, fold marker on a line's
        // first row; a wrap mark on the others. A folded line shows the
        // marks of the lines it hides too.
        let folded = buf.view.folds.header_at(line);
        let span = line..=folded.map_or(line, |(_, last)| last);
        let num_style = if is_cur { theme.line_number_current } else { theme.line_number };
        if let Some(b) = &buf.blame {
            let label = match b.commit_at(line) {
                Some(c) if row.seg == 0 => Some(super::blame::label(c)),
                None if b.data.is_none() && row_i == 0 => Some("blame…".to_string()),
                _ => None,
            };
            if let Some(label) = label {
                screen.set_stringn(bx, y, label, blame_w as usize - 1, theme.dim);
            }
        }
        if row.seg > 0 {
            screen.set_string(gx + digits as u16 + 2, y, "↪", theme.line_number);
        } else {
            let num = format!("{:>width$}", line + 1, width = digits);
            screen.set_stringn(gx, y, &num, digits, num_style);
            // A numbered bookmark shows its digit (on a fold's header the
            // first digit it hides), any other one ◆.
            let mut marked = buf.bookmarks.range(span.clone()).peekable();
            if marked.peek().is_some() {
                // A digit is drawn reversed, as a badge, so that it does not
                // read as one more digit of the line number.
                let (glyph, style) = match marked.find_map(|(_, d)| *d) {
                    Some(d) => (char::from(b'0' + d).to_string(), theme.bookmark.add_modifier(Modifier::REVERSED)),
                    None => ("◆".to_string(), theme.bookmark),
                };
                screen.set_string(gx + digits as u16, y, glyph, style);
            }
            // A diagnostic's glyph takes the diff mark's column over it, and
            // the lightbulb (quick fixes, `app/lightbulb.rs`) both, on the
            // caret line of the focused view.
            let git_mark = buf.gutter.get(buf.gutter.partition_point(|m| m.line < line)).filter(|m| m.line <= *span.end());
            let bulb = o.focused && line == cur_line && buf.lightbulb == Some((buf.version, line));
            if bulb {
                screen.set_string(gx + digits as u16 + 1, y, LIGHTBULB, theme.lightbulb);
            } else if let Some(sev) = marks.lines.range(span.clone()).map(|(_, s)| *s).min() {
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
        }
        // A resolver chunk's background wins over the current line's.
        let decor_bg = o.decor.and_then(|d| d.bg_at(line));
        let row_bg = decor_bg.or((is_cur && o.focused).then_some(theme.current_line_bg));
        if let Some(bg) = row_bg {
            screen.set_style(Rect::new(text_area.x, y, text_area.width, 1), Style::default().bg(bg));
        }
        if let Some(g) = o.decor.and_then(|d| d.glyphs_at(line)).filter(|_| row.seg == 0) {
            let style = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
            screen.set_stringn(area.x, y, g, action_w as usize, style);
        }
        layout_row(rope, &rows, row, o.tab_width, scroll_col + text_area.width as usize, &mut |g, c, rx| {
            if rx + c.width <= scroll_col {
                return true;
            }
            let x = text_area.x as usize + rx.saturating_sub(scroll_col);
            if x >= right as usize {
                return false;
            }
            let x = x as u16;
            let mut style = hl.style_at(c.char_idx).unwrap_or(Style::default().fg(theme.fg));
            if underlines {
                let t = marks.tags_at(c.char_idx);
                if t.unnecessary {
                    style = style.fg(theme.dim.fg.unwrap_or(theme.fg));
                }
                if t.deprecated {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }
            }
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
            if let Some(bg) = row_bg.filter(|_| style.bg.is_none()) {
                style = style.bg(bg);
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
            } else if crate::util::text::shown_escaped(rope.char(c.char_idx)) {
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
        // a caret at the line end is drawn there too. Both on the line's
        // last row, as is a fold's placeholder.
        let end_x = last_seg.then(|| char_to_row_x(rope, &rows, line_end, false, o.tab_width).1);
        let at_end = here.last() == Some(&line_end);
        if let Some(vc) = end_x
            && ((sels.contains(line_end) && line + 1 < rope.len_lines()) || at_end)
            && vc >= scroll_col
        {
            let x = text_area.x as usize + vc - scroll_col;
            if x < right as usize {
                let x = x as u16;
                if sels.contains(line_end) && line + 1 < rope.len_lines() {
                    screen[(x, y)].set_style(Style::default().bg(theme.selection_bg));
                }
                if at_end {
                    screen[(x, y)].set_style(theme.secondary_caret);
                }
            }
        }
        if !last_seg && drawn_up.binary_search(&seg.end).is_ok() {
            let end = char_to_row_x(rope, &rows, seg.end, true, o.tab_width).1;
            if end >= scroll_col {
                let x = (text_area.x as usize + end - scroll_col).min(right as usize - 1);
                screen[(x as u16, y)].set_style(theme.secondary_caret);
            }
        }
        if let (Some((f, last)), Some(vc)) = (folded, end_x) {
            draw_placeholder(screen, buf, (f, line, last), (text_area, y), vc, scroll_col, theme);
        }
        if row == cur_row && cur_x >= scroll_col {
            let x = text_area.x as usize + cur_x - scroll_col;
            if x < right as usize {
                cursor = Some((x as u16, y));
            } else if rows.wraps() {
                // Hanging whitespace past the edge: the caret stays on screen.
                cursor = Some((right - 1, y));
            }
        }
    }
    RenderOut { cursor, text_area }
}

/// The placeholder of collapsed fold `f` (header `line`, hiding up to
/// `last`) on screen row `y` of `text_area`, from row x `start` (the header
/// line's end on its last row; `scroll_col` is the view's sideways scroll).
/// It takes the selection's background when the selection reaches into the
/// fold, else a hidden find match's, and the worst hidden diagnostic's mark.
fn draw_placeholder(
    screen: &mut Screen,
    buf: &Buffer,
    (f, line, last): (Fold, usize, usize),
    (text_area, y): (Rect, u16),
    start: usize,
    scroll_col: usize,
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
    let mut col = start;
    for g in placeholder(rope, f, last).graphemes(true) {
        let w = g.width();
        if col >= scroll_col {
            let x = text_area.x as usize + col - scroll_col;
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

/// Whether the caret is inside a text area of `w` x `h` at the view's scroll.
fn caret_on_screen(buf: &Buffer, w: u16, h: u16, o: &RenderOpts) -> bool {
    let r = buf.rows();
    let sel = buf.view.sel();
    let caret = r.clamp(char_to_row_x(&buf.rope, &r, sel.head, sel.upstream, o.tab_width).0);
    let top = r.clamp(buf.view.top());
    if caret < top || r.row_distance(top, caret, h as usize) >= h as usize {
        return false;
    }
    if r.wraps() {
        return true;
    }
    let vcol = visual_col(&buf.rope, sel.head, o.tab_width);
    (buf.view.scroll_col..buf.view.scroll_col + w as usize).contains(&vcol)
}

pub fn ensure_visible(buf: &mut Buffer, text_area: Rect, o: &RenderOpts) {
    let rows = text_area.height as usize;
    let cols = text_area.width as usize;
    if rows == 0 || cols == 0 {
        return;
    }
    let so = o.scrolloff.min(rows.saturating_sub(1) / 2);
    // Distances are visible rows: hidden lines take none, wrapped ones
    // one per segment.
    let r = buf.rows();
    let sel = buf.view.sel();
    let caret = r.clamp(char_to_row_x(&buf.rope, &r, sel.head, sel.upstream, o.tab_width).0);
    let top = r.clamp(buf.view.top());
    let top = if caret < top || r.row_distance(top, caret, so) < so {
        r.step_row(caret, -(so as isize))
    } else if r.row_distance(top, caret, rows) + so >= rows {
        r.step_row(caret, -((rows - 1 - so) as isize))
    } else {
        top
    };
    let wraps = r.wraps();
    buf.view.set_top(top);
    if wraps {
        buf.view.scroll_col = 0;
        return;
    }

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
    fn decor_paints_lines_and_glyphs_left_of_the_gutter() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "a\nb\nc\n", false);
        let decor = LineDecor {
            action_w: 2,
            lines: vec![
                DecorLine { lines: 1..2, bg: Some(Color::Red), glyphs: "»×".into() },
                DecorLine { lines: 3..3, bg: Some(Color::Blue), glyphs: " ×".into() },
            ],
        };
        let o = RenderOpts {
            focused: false,
            tab_width: 4,
            wrap_extra_indent: 0,
            scrolloff: 0,
            hscrolloff: 0,
            occurrences: false,
            theme: &theme,
            follow: true,
            decor: Some(&decor),
        };
        let area = Rect::new(0, 0, 20, 5);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &o);
        assert_eq!(out.text_area.x, 2 + 3 + 4, "the glyph column, three digits, the marks");
        let row = |y: u16| (0..20).map(|x| screen[(x, y)].symbol().to_string()).collect::<String>();
        assert_eq!(row(1), "»×  2    b          ");
        assert_eq!(row(3), " ×  4               ", "an empty range still shows its glyphs");
        assert_eq!(screen[(9, 1)].bg, Color::Red);
        assert_eq!(screen[(19, 1)].bg, Color::Red, "the whole row");
        assert_ne!(screen[(9, 0)].bg, Color::Red);
        assert_ne!(screen[(9, 3)].bg, Color::Blue, "no lines, no background");
        assert_eq!(decor.glyphs_at(1), Some("»×"));
        assert_eq!(decor.glyphs_at(0), None);
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

    #[test]
    fn separators_are_drawn_escaped_inside_their_line() {
        let r = Rope::from_str("a\u{2028}b\u{c}c\u{85}d\n");
        assert_eq!(r.len_lines(), 2);
        let mut cells = Vec::new();
        layout_line(&r, 0, 4, usize::MAX, &mut |g, c| {
            cells.push((g.to_string(), c.char_idx, c.col, c.width));
            true
        });
        let want = [
            ("a", 0, 0, 1),
            ("\\u{2028}", 1, 1, 8),
            ("b", 2, 9, 1),
            ("^L", 3, 10, 2),
            ("c", 4, 12, 1),
            ("\\u{85}", 5, 13, 6),
            ("d", 6, 19, 1),
        ];
        let want: Vec<_> = want.iter().map(|&(g, i, c, w)| (g.to_string(), i, c, w)).collect();
        assert_eq!(cells, want);
        assert_eq!(col_to_char(&r, 0, 5, 4), 1, "inside the escape");
        assert_eq!(visual_col(&r, 2, 4), 9);

        // Every escape is drawn dim, cell by cell, nothing raw.
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "a\u{2028}b\u{c}c\u{85}d\n", false);
        let o = RenderOpts {
            focused: false,
            tab_width: 4,
            wrap_extra_indent: 0,
            scrolloff: 0,
            hscrolloff: 0,
            occurrences: false,
            theme: &theme,
            follow: true,
            decor: None,
        };
        let area = Rect::new(0, 0, 40, 3);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &o);
        let x0 = out.text_area.x;
        let row: String = (x0..x0 + 20).map(|x| screen[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "a\\u{2028}b^Lc\\u{85}d");
        for col in [1u16, 8, 10, 11, 13, 18] {
            assert!(screen[(x0 + col, 0)].modifier.contains(Modifier::DIM), "col {col}");
        }
        assert!(!screen[(x0, 0)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn render_and_map_agree_on_separators() {
        let r = Rope::from_str("x\u{2028}y\t\u{2029}z\u{b}😀\u{85}end\n");
        let mut cells = Vec::new();
        layout_line(&r, 0, 4, usize::MAX, &mut |_, c| {
            cells.push(c);
            true
        });
        for c in cells {
            assert_eq!(visual_col(&r, c.char_idx, 4), c.col);
            for col in c.col..c.col + c.width {
                assert_eq!(col_to_char(&r, 0, col, 4), c.char_idx, "col {col}");
            }
        }
    }

    fn resize_opts(theme: &Theme) -> RenderOpts<'_> {
        RenderOpts {
            focused: true,
            tab_width: 4,
            wrap_extra_indent: 0,
            scrolloff: 0,
            hscrolloff: 0,
            occurrences: false,
            theme,
            follow: true,
            decor: None,
        }
    }

    fn render_at(buf: &mut Buffer, theme: &Theme, w: u16, h: u16) {
        let area = Rect::new(0, 0, w, h);
        render(buf, &mut Screen::empty(area), area, &resize_opts(theme));
    }

    #[test]
    fn resize_does_not_rescroll_a_caret_already_off_screen() {
        let theme = Theme::default();
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut buf = Buffer::from_text(1, None, &text, false);
        buf.goto_line(50, 0);
        render_at(&mut buf, &theme, 40, 10);
        assert!(buf.view.scroll_line > 40, "scrolled to the caret");
        // Scrolled away by hand: the caret is off screen at both sizes.
        buf.view.scroll_line = 0;
        render_at(&mut buf, &theme, 40, 10);
        assert_eq!(buf.view.scroll_line, 0);
        render_at(&mut buf, &theme, 50, 12);
        assert_eq!(buf.view.scroll_line, 0, "a resize alone leaves the view");
    }

    #[test]
    fn resize_scrolls_when_the_caret_drops_off_screen() {
        let theme = Theme::default();
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut buf = Buffer::from_text(1, None, &text, false);
        buf.goto_line(3, 0);
        render_at(&mut buf, &theme, 40, 10);
        assert_eq!(buf.view.scroll_line, 0);
        render_at(&mut buf, &theme, 40, 2);
        assert_eq!(buf.view.scroll_line, 2, "the caret line is the last row");
    }
}
