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
use super::inlay::Inlay;
use super::rows::{Row, Rows};
use super::selection::{MAX_CARETS, Selection, Selections, Virt, line_len_no_eol, pos_to_line_col};
use crate::config::theme::Theme;

/// What a [`Cell`] is: text of the buffer, or an inlay hint's virtual text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellKind {
    #[default]
    Text,
    /// A space around a hint's label, drawn as plain background.
    InlayPad,
    /// A hint's label.
    InlayLabel,
}

/// One rendered unit: a grapheme (or the expansion of a tab, a control char
/// or a U+2028/U+2029 separator, drawn as an escape), or a piece of an
/// inlay hint, whose `char_idx` is its anchor.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cell {
    pub char_idx: usize,
    pub col: usize,
    pub width: usize,
    pub kind: CellKind,
    /// A piece of a parameter-name hint: a caret at its anchor stands after it
    /// (before any other hint).
    pub param: bool,
}

impl Cell {
    /// A piece of an inlay hint, not a char of the buffer.
    pub fn is_inlay(&self) -> bool {
        self.kind != CellKind::Text
    }
}

/// Walk the graphemes of `line` (without its line break) as text columns,
/// without inlay hints. The callback gets the text to draw and the cell
/// geometry; return `false` to stop early.
pub fn layout_line(rope: &Rope, line: usize, tab_width: usize, max_chars: usize, f: &mut dyn FnMut(&str, Cell) -> bool) {
    layout_line_inlays(rope, line, tab_width, max_chars, &[], None, f);
}

/// [`layout_line`] with the hints `inlays` (the line's, sorted) drawn before
/// the grapheme at their anchor, and after the last one when anchored at the
/// line end (only when the walk reaches it). Cells carry their kind; a hint's
/// cells (left pad, label, right pad) have the anchor as `char_idx`.
pub fn layout_line_inlays(
    rope: &Rope,
    line: usize,
    tab_width: usize,
    max_chars: usize,
    inlays: &[Inlay],
    tail: Option<&Inlay>,
    f: &mut dyn FnMut(&str, Cell) -> bool,
) {
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
    let mut k = 0usize;
    let tab_width = tab_width.max(1);
    for g in text.graphemes(true) {
        let nchars = g.chars().count();
        while k < inlays.len() && inlays[k].at < idx + nchars {
            if !emit_inlay(&inlays[k], idx, &mut col, f) {
                return;
            }
            k += 1;
        }
        if g == "\t" {
            let w = tab_width - (col % tab_width);
            if !f("", Cell { char_idx: idx, col, width: w, kind: CellKind::Text, param: false }) {
                return;
            }
            col += w;
        } else if let Some(c) = g.chars().next().filter(|&c| crate::util::text::shown_escaped(c)) {
            let shown = control_repr(c);
            let w = shown.len();
            if !f(&shown, Cell { char_idx: idx, col, width: w, kind: CellKind::Text, param: false }) {
                return;
            }
            col += w;
        } else {
            let w = g.width();
            if !f(g, Cell { char_idx: idx, col, width: w, kind: CellKind::Text, param: false }) {
                return;
            }
            col += w;
        }
        idx += nchars;
    }
    if take == n {
        for h in &inlays[k..] {
            if !emit_inlay(h, idx, &mut col, f) {
                return;
            }
        }
        if let Some(t) = tail {
            emit_inlay(t, idx, &mut col, f);
        }
    }
}

/// The cells of one hint at `col`; false when `f` stopped the walk.
fn emit_inlay(h: &Inlay, idx: usize, col: &mut usize, f: &mut dyn FnMut(&str, Cell) -> bool) -> bool {
    let pad = |col: &mut usize, f: &mut dyn FnMut(&str, Cell) -> bool| {
        let ok = f(" ", Cell { char_idx: idx, col: *col, width: 1, kind: CellKind::InlayPad, param: h.param });
        *col += 1;
        ok
    };
    if h.pad_left && !pad(col, f) {
        return false;
    }
    let ok = f(&h.label, Cell { char_idx: idx, col: *col, width: h.width, kind: CellKind::InlayLabel, param: h.param });
    *col += h.width;
    if !ok {
        return false;
    }
    !h.pad_right || pad(col, f)
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
    if rows.is_filler(row) {
        let e = rows.seg_range(rope, row).end;
        return RowGeom { start: e, end: e, origin: 0, indent: 0, last: false };
    }
    let r = rows.seg_range(rope, row);
    match rows.line_wrap(row.line) {
        Some(w) => {
            let s = u16::try_from(row.seg).unwrap_or_default();
            RowGeom { start: r.start, end: r.end, origin: w.start(s).1, indent: w.indent_of(s), last: s + 1 >= w.segs() }
        }
        None => RowGeom { start: r.start, end: r.end, origin: 0, indent: 0, last: true },
    }
}

/// [`layout_line_inlays`] for one screen row: the callback also gets each
/// cell's row x. Without wrap the row is the whole line, walked up to about
/// `cols` cells.
pub fn layout_row(rope: &Rope, rows: &Rows, row: Row, tab_width: usize, cols: usize, f: &mut dyn FnMut(&str, Cell, usize) -> bool) {
    if rows.is_filler(row) {
        return;
    }
    let g = row_geom(rope, rows, row);
    let max_chars =
        if rows.wraps() && row.line < rope.len_lines() { g.end - rope.line_to_char(row.line) } else { cols.saturating_add(256) };
    layout_line_inlays(rope, row.line, tab_width, max_chars, rows.inlays_on(rope, row.line), rows.value_on(row.line), &mut |s, c| {
        if c.char_idx < g.start {
            return true;
        }
        // Hints at the line end belong to the last row.
        if c.char_idx >= g.end && !(c.is_inlay() && g.last && c.char_idx == g.end) {
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
        // A caret at an anchor stands before its hints, parameter hints
        // excepted (they belong to the char after them).
        if c.char_idx >= pos && !c.param {
            x = rx;
            return false;
        }
        x = rx + c.width;
        true
    });
    (row, x)
}

/// Row x after the last cell of `line`'s last row, its line-end hints included:
/// where the line-break selection cell, virtual space and a fold placeholder start.
pub fn text_end_x(rope: &Rope, rows: &Rows, line: usize, tab_width: usize) -> usize {
    let line = line.min(rope.len_lines().saturating_sub(1));
    let end = rope.line_to_char(line) + line_len_no_eol(rope, line);
    let row = rows.row_of(rope, end);
    let mut x = row_geom(rope, rows, row).indent;
    layout_row(rope, rows, row, tab_width, usize::MAX, &mut |_, c, rx| {
        x = rx + c.width;
        true
    });
    x
}

/// Char position and affinity for a click at row x `x` on screen row
/// `row`: the grapheme there (its right half too); left of a continuation
/// row's text, the row's first char; past the text, the line end on the
/// line's last row, else the row's end with the upstream affinity (the
/// caret stays on the row). A cell of an inlay hint is its anchor's.
pub fn row_x_to_char(rope: &Rope, rows: &Rows, row: Row, x: usize, tab_width: usize) -> (usize, bool) {
    if row.line >= rope.len_lines() {
        return (rope.len_chars(), false);
    }
    if rows.is_filler(row) {
        return (rows.seg_range(rope, row).end, false);
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

/// The column (row x on an unwrapped line) the caret `sel` stands at: hints
/// before it included, and past the line end the cells the virtual column
/// goes beyond the line's text: the x after the line's hints plus those cells.
pub fn caret_col(rope: &Rope, rows: &Rows, sel: &Selection, tab_width: usize) -> usize {
    let x = char_to_row_x(rope, rows, sel.head, sel.upstream, tab_width).1;
    match sel.virt.head {
        Some(c) if !rows.wraps() && c > visual_col(rope, sel.head, tab_width) => {
            text_end_x(rope, rows, rope.char_to_line(sel.head), tab_width) + (c - visual_col(rope, sel.head, tab_width))
        }
        _ => x,
    }
}

/// The text column under row x `rx` of `line` (soft wrap off): screen
/// columns count the line's hints, text columns (column blocks, virtual
/// space) do not. Past the line end the extra cells carry over.
pub fn text_col_at(rope: &Rope, rows: &Rows, line: usize, rx: usize, tab_width: usize) -> usize {
    if rows.wraps() || line >= rope.len_lines() || (rows.inlays_on(rope, line).is_empty() && rows.value_on(line).is_none()) {
        return rx;
    }
    let end = rope.line_to_char(line) + line_len_no_eol(rope, line);
    let ex = text_end_x(rope, rows, line, tab_width);
    if rx >= ex {
        return visual_col(rope, end, tab_width) + (rx - ex);
    }
    visual_col(rope, row_x_to_char(rope, rows, Row::line(line), rx, tab_width).0, tab_width)
}

/// The carets of a column block from `anchor` to `head`, each a (line,
/// visual column). Every line between gets the cells between the two
/// columns, a grapheme cut by either edge included; a line too short to
/// reach an edge gets that end at its line end with the edge's column as
/// its virtual column (`Selection::virt`; typing pads to it, `render` draws
/// there), and a zero-width block is a caret on every line. Each selection points the
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
            let w = visual_col(rope, rope.line_to_char(line) + line_len_no_eol(rope, line), tab_width);
            if lo == hi {
                return if lo > w { Selection::virtual_point(start, lo) } else { Selection::point(start) };
            }
            let end = col_to_char_ceil(rope, line, hi, tab_width).max(start);
            let mut sel = if forward { Selection::new(start, end) } else { Selection::new(end, start) };
            let (l, r) = ((lo > w).then_some(lo), (hi > w).then_some(hi));
            sel.virt = if forward { Virt { anchor: l, head: r } } else { Virt { anchor: r, head: l } };
            sel
        })
        .collect();
    Selections::new(ranges, primary)
}

/// Line numbers, bookmark or breakpoint, diff mark (or diagnostic glyph, or lightbulb),
/// fold marker, space; the blame column before them when shown, and first
/// `action_w` cells of [`LineDecor`] glyphs.
pub fn gutter_width(buf: &Buffer, action_w: u16) -> u16 {
    let digits = buf.rope.len_lines().max(1).to_string().len().max(3) as u16;
    let blame = if buf.blame.as_ref().is_some_and(|b| b.column) { super::blame::WIDTH } else { 0 };
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
    /// Draw indent guides (`editor.indent_guides`).
    pub indent_guides: bool,
    /// Columns after which a ruler is drawn, on 0-based cell n, where the text leaves it empty (`editor.rulers`, cleaned: no 0, ascending).
    pub rulers: &'a [usize],
    /// Colour bracket tokens by nesting depth (`editor.bracket_pair_colours`); off for previews and patch tabs.
    pub bracket_colours: bool,
    /// Draw the fold arrows in the gutter.
    pub fold_marks: bool,
    /// Paint the caret line's blame after its text (`editor.inline_blame`).
    pub inline_blame: bool,
    pub theme: &'a Theme,
    /// Scroll to the caret when it moved (else the scroll stays, as the
    /// resolver's linked panes set it).
    pub follow: bool,
    pub decor: Option<&'a LineDecor>,
    /// The 0-based line a paused debug session is on, when it is in this buffer's file.
    pub execution_line: Option<usize>,
    /// The 0-based line of the stack frame chosen in the Debug window, when it is in this buffer's file.
    pub frame_line: Option<usize>,
}

/// How far a blank line looks up and down for the non-blank lines that set
/// its indent guides.
const GUIDE_SCAN: usize = 100;

/// The indent of `line` in display columns (tabs to the next multiple of
/// `tab`); `None` when the line is blank (empty or whitespace only).
fn indent_cols(rope: &Rope, line: usize, tab: usize) -> Option<usize> {
    let tab = tab.max(1);
    let n = line_len_no_eol(rope, line);
    let mut cols = 0;
    for c in rope.line(line).chars().take(n) {
        match c {
            ' ' => cols += 1,
            '\t' => cols = (cols / tab + 1) * tab,
            _ => return Some(cols),
        }
    }
    None
}

/// One entry per line of `window` (clamped to the rope): `None` for a
/// non-blank line, and for a blank one without non-blank neighbours within
/// `GUIDE_SCAN` lines both ways; else `Some(min(previous, next indent))`.
fn blank_guide_levels(rope: &Rope, window: Range<usize>, tab: usize) -> Vec<Option<usize>> {
    let end = window.end.min(rope.len_lines());
    let start = window.start.min(end);
    // The nearest non-blank line above the window, within the scan.
    let mut prev = (start.saturating_sub(GUIDE_SCAN)..start).rev().find_map(|l| indent_cols(rope, l, tab).map(|c| (l, c)));
    let mut out = vec![None; end - start];
    let mut cols = vec![None; end - start];
    for (i, line) in (start..end).enumerate() {
        match indent_cols(rope, line, tab) {
            Some(c) => prev = Some((line, c)),
            None => cols[i] = prev.filter(|&(l, _)| line - l <= GUIDE_SCAN).map(|(_, c)| c),
        }
    }
    // The nearest non-blank line below the window, within the scan.
    let mut next = (end..(end + GUIDE_SCAN).min(rope.len_lines())).find_map(|l| indent_cols(rope, l, tab).map(|c| (l, c)));
    for (i, line) in (start..end).enumerate().rev() {
        match indent_cols(rope, line, tab) {
            Some(c) => next = Some((line, c)),
            None => {
                let below = next.filter(|&(l, _)| l - line <= GUIDE_SCAN).map(|(_, c)| c);
                out[i] = cols[i].zip(below).map(|(a, b)| a.min(b));
            }
        }
    }
    out
}

/// A line with nothing but whitespace.
fn blank_line(rope: &Rope, line: usize) -> bool {
    super::selection::indent_of(rope, line) == line_len_no_eol(rope, line)
}

/// Scroll so that the caret is visible, then render.
pub fn render(buf: &mut Buffer, screen: &mut Screen, area: Rect, o: &RenderOpts) -> RenderOut {
    let action_w = o.decor.map_or(0, |d| d.action_w);
    let gutter_w = gutter_width(buf, action_w);
    let blame_w = if buf.blame.as_ref().is_some_and(|b| b.column) { super::blame::WIDTH } else { 0 };
    let text_area = Rect::new(area.x + gutter_w, area.y, area.width.saturating_sub(gutter_w), area.height);
    if text_area.width == 0 || area.height == 0 {
        return RenderOut { cursor: None, text_area };
    }
    // A caret moved into a fold from outside the editor (go to definition,
    // F2, find in files, bookmarks, back/forward) unfolds it; the top line
    // is never hidden.
    let v = &mut buf.view;
    v.folds.reveal(&buf.rope, &v.sels);
    // Measured before the rows are re-broken for the new width.
    let follow_key = (buf.view.sel().head, buf.version, text_area.width, text_area.height);
    let was_visible = buf.view.follow_key != Some(follow_key) && buf.view.follow_key.is_some_and(|k| caret_on_screen(buf, k.2, k.3, o));
    let v = &mut buf.view;
    v.wrap.set_geometry(text_area.width, o.tab_width, o.wrap_extra_indent);
    let top = Rows::view(&v.folds, &v.wrap, &v.fillers, &buf.rope, buf.version).with_inlays(&buf.inlays, &buf.inline_values).clamp(v.top());
    v.set_top(top);
    if buf.view.follow_key != Some(follow_key) {
        // A resize alone re-scrolls only a caret that was on screen and is
        // not any more; one already off screen stays where it was.
        let resized_only = buf.view.follow_key.is_some_and(|k| (k.0, k.1) == (follow_key.0, follow_key.1));
        if o.follow && (!resized_only || (was_visible && !caret_on_screen(buf, text_area.width, text_area.height, o))) {
            ensure_visible(buf, text_area, o);
        }
        buf.view.follow_key = Some(follow_key);
    }
    let shown: Vec<Row> = buf.rows().iter_from(buf.view.top()).take(area.height as usize).collect();
    // The lines from the top to the bottom row, hidden ones included.
    let window = buf.view.scroll_line..shown.last().map_or(buf.view.scroll_line, |r| r.line + 1);
    // Headers that can fold get a marker.
    let foldable: Vec<usize> =
        buf.fold_candidates(window.clone()).iter().filter(|_| o.fold_marks).map(|f| buf.rope.char_to_line(f.start)).collect();

    let theme = o.theme;
    let rope = &buf.rope;
    let rows =
        Rows::view(&buf.view.folds, &buf.view.wrap, &buf.view.fillers, rope, buf.version).with_inlays(&buf.inlays, &buf.inline_values);
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
    let cur_row = char_to_row_x(rope, &rows, buf.view.sel().head, buf.view.sel().upstream, o.tab_width).0;
    // Virtual space (soft wrap off): selections reach past their line ends to their
    // virtual columns, and carets stand there.
    let virt_on = !rows.wraps();
    let cur_x = caret_col(rope, &rows, &buf.view.sel(), o.tab_width);
    let mut hl = if let Some(p) = &buf.preview {
        p.highlights(rope, window.clone())
    } else if let Some(t) = &buf.search_tab {
        t.highlights(rope, window.clone(), theme)
    } else {
        buf.syntax.as_mut().map(|s| s.highlights(rope, window.start, window.len())).unwrap_or_default()
    };
    // LSP semantic tokens over tree-sitter's (or no) styles; a capture the
    // theme does not style keeps what is there.
    if !buf.semantic.is_empty() {
        let last = window.end.min(rope.len_lines());
        let chars = rope.line_to_char(window.start.min(last))..rope.line_to_char(last);
        hl.overlay(buf.semantic.in_range(chars).filter_map(|(r, name)| theme.capture_style(name).map(|s| (r, s))));
    }
    // Bracket pair colours: the window's bracket tokens and their depths.
    let depths = if o.bracket_colours && buf.preview.is_none() {
        buf.syntax.as_mut().map(|s| s.bracket_depths(rope, window.start, window.len())).unwrap_or_default()
    } else {
        Vec::new()
    };
    let bracket_fg = [theme.bracket_1.fg, theme.bracket_2.fg, theme.bracket_3.fg];
    let search = buf.view.search.as_ref();
    let pair =
        super::brackets::pair_at(rope, buf.syntax.as_ref().and_then(|s| s.tree_at_char(rope, buf.view.sel().head)), buf.view.sel().head);
    // Occurrences of the word or selection at the caret, visible lines only.
    // An open find bar owns the match highlighting, so they step aside. A
    // language server's document highlight of the caret word, while it
    // holds, replaces the textual occurrences, with its own colours.
    let mut occ: Vec<(Range<usize>, Color)> = Vec::new();
    if o.occurrences && search.is_none() {
        match buf.doc_highlights.as_ref().filter(|h| h.holds(rope, buf.version, &buf.view.sel())) {
            Some(h) => {
                let last = window.end.min(rope.len_lines());
                let lo = rope.line_to_char(window.start.min(last));
                let hi = if window.end >= rope.len_lines() { rope.len_chars() } else { rope.line_to_char(window.end) };
                occ.extend(h.spans().iter().filter(|(r, _)| r.end > lo && r.start < hi).map(|(r, k)| {
                    let bg = match k {
                        super::search::HighlightKind::Text => theme.occurrence_bg,
                        super::search::HighlightKind::Read => theme.occurrence_read,
                        super::search::HighlightKind::Write => theme.occurrence_write,
                    };
                    (r.clone(), bg)
                }));
            }
            None => {
                if let Some((target, whole_word)) = super::search::occurrence_target(rope, &buf.view.sel()) {
                    let found = super::search::occurrences(rope, window.clone(), &target, whole_word);
                    occ.extend(found.into_iter().map(|r| (r, theme.occurrence_bg)));
                }
            }
        }
    }
    let marks = &buf.diag_marks;
    let underlines = !marks.spans.is_empty();
    let digits = (gutter_w - 4 - blame_w - action_w) as usize;
    let bx = area.x + action_w;
    let gx = bx + blame_w;
    // Wrapped views never scroll sideways.
    let scroll_col = if rows.wraps() { 0 } else { buf.view.scroll_col };
    let right = text_area.right();
    let mut cursor = None;
    let guide_step = if buf.indent.hard_tabs { o.tab_width } else { buf.indent.width }.max(1);
    let guide_fg = theme.indent_guide.fg.unwrap_or(theme.fg);
    let ruler_fg = theme.ruler.fg.unwrap_or(theme.fg);
    let guides = o.indent_guides && buf.patch.is_none();
    let levels = if guides { blank_guide_levels(rope, window.clone(), o.tab_width) } else { Vec::new() };

    for (row_i, &row) in shown.iter().enumerate() {
        let line = row.line;
        let row_i = row_i as u16;
        let y = area.y + row_i;
        if rows.is_filler(row) {
            if let Some(bg) = rows.filler(row).and_then(|(f, _)| f.bg) {
                screen.set_style(Rect::new(text_area.x, y, text_area.width, 1), Style::default().bg(bg));
            }
            continue;
        }
        let line_start = rope.line_to_char(line);
        let line_end = line_start + line_len_no_eol(rope, line);
        // Blank lines get their guides after the walk, from `levels`.
        let blank_level = levels.get(line - window.start).copied().flatten();
        let lead_end = if guides && row.seg == 0 && blank_level.is_none() && !blank_line(rope, line) {
            line_start + super::selection::indent_of(rope, line)
        } else {
            line_start
        };
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
        // A fold's header shows the arrow and the background of the line it hides.
        let on_execution = o.execution_line.is_some_and(|l| span.contains(&l));
        let num_style = if is_cur { theme.line_number_current } else { theme.line_number };
        if let Some(b) = buf.blame.as_ref().filter(|b| b.column) {
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
            // One cell, one mark: the execution arrow hides a breakpoint, a breakpoint hides a bookmark
            // while it is there (on a fold's header, the first one it hides).
            if on_execution {
                screen.set_string(gx + digits as u16, y, "▶", theme.debug_execution_arrow);
            } else if let Some((_, mark)) = buf.breakpoints.range(span.clone()).next() {
                let (glyph, style) = super::breakpoints::breakpoint_glyph(mark.state, &mark.spec, theme);
                screen.set_string(gx + digits as u16, y, glyph, style);
            } else {
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
        let exec_bg = on_execution.then_some(theme.debug_execution_line_bg);
        let on_frame = o.frame_line.is_some_and(|l| span.contains(&l));
        let frame_bg = on_frame.then_some(theme.debug_frame_line_bg);
        let row_bg = decor_bg.or(exec_bg).or(frame_bg).or((is_cur && o.focused).then_some(theme.current_line_bg));
        if let Some(bg) = row_bg {
            screen.set_style(Rect::new(text_area.x, y, text_area.width, 1), Style::default().bg(bg));
        }
        if let Some(g) = o.decor.and_then(|d| d.glyphs_at(line)).filter(|_| row.seg == 0) {
            let style = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
            screen.set_stringn(area.x, y, g, action_w as usize, style);
        }
        // The caret of a type hint's anchor is drawn on the hint's first cell.
        let virt_end = virt_on
            && sels.as_slice().iter().any(|s| s.head == line_end && s.virt.head.is_some_and(|c| c > visual_col(rope, s.head, o.tab_width)));
        let mut hint_caret: Option<usize> = None;
        let mut row_end = 0usize;
        layout_row(rope, &rows, row, o.tab_width, scroll_col + text_area.width as usize, &mut |g, c, rx| {
            row_end = row_end.max(rx + c.width);
            if c.is_inlay() {
                // Hint cells get no highlight, selection or search styling,
                // only the secondary caret of their anchor. Pads keep the row
                // background.
                let first = !c.param
                    && here.binary_search(&c.char_idx).is_ok()
                    && hint_caret != Some(c.char_idx)
                    && !(virt_end && c.char_idx == line_end);
                if first {
                    hint_caret = Some(c.char_idx);
                }
                let mut caret = first;
                if c.kind == CellKind::InlayPad {
                    let sx = text_area.x as usize + rx.saturating_sub(scroll_col);
                    if caret && rx >= scroll_col && sx < right as usize {
                        screen[(sx as u16, y)].set_style(theme.secondary_caret);
                    }
                } else if c.kind == CellKind::InlayLabel {
                    let mut gx = rx;
                    for gr in g.graphemes(true) {
                        let gw = gr.width();
                        let sx = text_area.x as usize + gx.saturating_sub(scroll_col);
                        let first_gr = gw > 0 && std::mem::take(&mut caret);
                        if gw > 0 && gx >= scroll_col {
                            if sx + gw > right as usize {
                                return false;
                            }
                            screen[(sx as u16, y)].set_symbol(gr).set_style(theme.inlay_hint);
                            if first_gr {
                                screen[(sx as u16, y)].set_style(theme.secondary_caret);
                            }
                            for i in 1..gw as u16 {
                                screen[(sx as u16 + i, y)].reset();
                            }
                        }
                        gx += gw;
                    }
                }
                return text_area.x as usize + rx.saturating_sub(scroll_col) < right as usize;
            }
            if rx + c.width <= scroll_col {
                return true;
            }
            let x = text_area.x as usize + rx.saturating_sub(scroll_col);
            if x >= right as usize {
                return false;
            }
            let x = x as u16;
            let mut style = hl.style_at(c.char_idx).unwrap_or(Style::default().fg(theme.fg));
            if let Ok(i) = depths.binary_search_by_key(&c.char_idx, |&(p, _)| p)
                && let Some(fg) = bracket_fg[depths[i].1 % 3]
            {
                style = style.fg(fg);
            }
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
            } else if let Some(bg) = bg_at(&occ, c.char_idx) {
                style = style.bg(bg);
            }
            if pair.is_some_and(|(a, b)| c.char_idx == a || c.char_idx == b) {
                style = style.patch(theme.bracket_match);
            }
            if let Some(bg) = row_bg.filter(|_| style.bg.is_none()) {
                style = style.bg(bg);
            }
            if here.binary_search(&c.char_idx).is_ok() && hint_caret != Some(c.char_idx) {
                style = style.patch(theme.secondary_caret);
            }
            if c.width == 0 {
                return true;
            }
            let guide = c.char_idx < lead_end && c.col % guide_step == 0;
            if g.is_empty() {
                // Tab: fill with spaces, a guide in the first cell.
                for i in 0..c.width {
                    let xi = x + i as u16;
                    if xi >= right {
                        break;
                    }
                    if i == 0 && guide {
                        screen[(xi, y)].set_symbol("│").set_style(style.fg(guide_fg));
                    } else {
                        screen[(xi, y)].set_symbol(" ").set_style(style);
                    }
                }
            } else if crate::util::text::shown_escaped(rope.char(c.char_idx)) {
                screen.set_stringn(x, y, g, (right - x) as usize, style.add_modifier(Modifier::DIM));
            } else {
                if x + c.width as u16 > right {
                    return false;
                }
                if guide && g == " " {
                    screen[(x, y)].set_symbol("│").set_style(style.fg(guide_fg));
                } else {
                    screen[(x, y)].set_symbol(g).set_style(style);
                }
                for i in 1..c.width as u16 {
                    screen[(x + i, y)].reset();
                }
            }
            true
        });
        // Guides of a blank line, in its real and virtual cells; pure paint,
        // under the selection and caret styles below.
        if let Some(l) = blank_level.filter(|_| guides && row.seg == 0) {
            for c in (0..l).step_by(guide_step).filter(|&c| c >= scroll_col) {
                let x = text_area.x as usize + c - scroll_col;
                if x >= right as usize {
                    break;
                }
                screen[(x as u16, y)].set_symbol("│").set_fg(guide_fg);
            }
        }
        // Rulers: the cells past this row's text (hints included), pure paint
        // under the line-end selection, carets, placeholder and inline blame.
        for &n in o.rulers {
            if n < row_end || n < scroll_col {
                continue;
            }
            let x = text_area.x as usize + n - scroll_col;
            if x >= right as usize {
                break;
            }
            screen[(x as u16, y)].set_symbol("│").set_fg(ruler_fg);
        }
        // Selection that spans the line break: mark one cell past the text;
        // a caret at the line end is drawn there too. Both on the line's
        // last row, as is a fold's placeholder.
        let end_x = last_seg.then(|| text_end_x(rope, &rows, line, o.tab_width));
        let at_end = here.last() == Some(&line_end);
        // The one selection touching the line end, and the columns of its two
        // ends there (virtual space included).
        let vspan = end_x.filter(|_| virt_on).and_then(|w| {
            let all = sels.as_slice();
            let s = all.get(all.partition_point(|s| s.range().end < line_end)).filter(|s| s.range().start <= line_end)?;
            let end_col = visual_col(rope, line_end, o.tab_width);
            let col = |p: usize, v: Option<usize>| if p == line_end { v.map_or(w, |c| w + c.saturating_sub(end_col)) } else { w };
            Some((s.head == line_end, col(s.anchor, s.virt.anchor), col(s.head, s.virt.head)))
        });
        // Its virtual cells past the line end.
        if let Some((_, a, h)) = vspan
            && folded.is_none()
        {
            let (from, to) = (a.min(h).max(scroll_col), a.max(h));
            if from < to {
                let x = text_area.x as usize + from - scroll_col;
                if x < right as usize {
                    let w = (to - from).min(right as usize - x);
                    screen.set_style(Rect::new(x as u16, y, w as u16, 1), Style::default().bg(theme.selection_bg));
                }
            }
        }
        // The caret cell: the line end, or the virtual head column past it.
        let caret_x = end_x.map(|vc| match vspan {
            Some((true, _, h)) if at_end => h,
            _ => vc,
        });
        if let Some(vc) = caret_x
            && ((sels.contains(line_end) && line + 1 < rope.len_lines()) || at_end)
            && vc >= scroll_col
        {
            let x = text_area.x as usize + vc - scroll_col;
            if x < right as usize {
                let x = x as u16;
                if sels.contains(line_end) && line + 1 < rope.len_lines() {
                    screen[(x, y)].set_style(Style::default().bg(theme.selection_bg));
                }
                if at_end && (hint_caret != Some(line_end) || virt_end) {
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
        // Inline blame: the primary caret's line, four cells after its text.
        if o.inline_blame
            && o.focused
            && line == cur_line
            && let Some(vc) = end_x
            && buf.view.sel().is_empty()
            && let Some(c) = buf.blame.as_ref().and_then(|b| b.commit_at(line))
        {
            let mut end = vc;
            if let Some((f, last)) = folded {
                end += placeholder(rope, f, last).width();
            }
            if virt_on {
                end = end.max(cur_x);
            }
            let gc = end + 4;
            if gc >= scroll_col {
                let x = text_area.x as usize + gc - scroll_col;
                if x < right as usize && right as usize - x >= 10 {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                    let text = super::blame::fit_inline(&super::blame::inline_text(c, now), right as usize - x);
                    screen.set_stringn(x as u16, y, text, right as usize - x, theme.inline_blame);
                }
            }
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

/// The colour of the range holding `i`; `ranges` is sorted and non-overlapping.
fn bg_at(ranges: &[(Range<usize>, Color)], i: usize) -> Option<Color> {
    let k = ranges.partition_point(|(r, _)| r.end <= i);
    ranges.get(k).filter(|(r, _)| r.contains(&i)).map(|&(_, c)| c)
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
    let vcol = caret_col(&buf.rope, &r, &sel, o.tab_width);
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
    let vcol = caret_col(&buf.rope, &r, &buf.view.sel(), o.tab_width);
    buf.view.set_top(top);
    if wraps {
        buf.view.scroll_col = 0;
        return;
    }

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
    use crate::editor::indent::IndentStyle;

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
            indent_guides: false,
            rulers: &[],
            bracket_colours: false,
            fold_marks: true,
            inline_blame: false,
            theme: &theme,
            follow: true,
            decor: Some(&decor),
            execution_line: None,
            frame_line: None,
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
    fn decor_bg_wins_over_execution() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "a\nb\nc\n", false);
        let decor = LineDecor { action_w: 0, lines: vec![DecorLine { lines: 1..2, bg: Some(Color::Red), glyphs: String::new() }] };
        let area = Rect::new(0, 0, 20, 5);
        let mut cells = Vec::new();
        for line in [1, 2] {
            let o = RenderOpts {
                focused: false,
                tab_width: 4,
                wrap_extra_indent: 0,
                scrolloff: 0,
                hscrolloff: 0,
                occurrences: false,
                indent_guides: false,
                rulers: &[],
                bracket_colours: false,
                fold_marks: true,
                inline_blame: false,
                theme: &theme,
                follow: true,
                decor: Some(&decor),
                execution_line: Some(line),
                frame_line: None,
            };
            let mut screen = Screen::empty(area);
            let out = render(&mut buf, &mut screen, area, &o);
            cells.push((screen[(out.text_area.x, line as u16)].bg, screen[(out.text_area.x - 4, line as u16)].symbol().to_string()));
        }
        assert_eq!(cells[0].0, Color::Red, "the resolver's chunk keeps its background");
        assert_eq!(cells[1].0, theme.debug_execution_line_bg, "positive control: the same row without a chunk");
        assert_eq!((cells[0].1.as_str(), cells[1].1.as_str()), ("▶", "▶"), "the arrow shows either way");
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
            indent_guides: false,
            rulers: &[],
            bracket_colours: false,
            fold_marks: true,
            inline_blame: false,
            theme: &theme,
            follow: true,
            decor: None,
            execution_line: None,
            frame_line: None,
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
            indent_guides: false,
            rulers: &[],
            bracket_colours: false,
            fold_marks: true,
            inline_blame: false,
            theme,
            follow: true,
            decor: None,
            execution_line: None,
            frame_line: None,
        }
    }

    fn blamed(text: &str) -> Buffer {
        use crate::editor::blame::BlameState;
        use crate::git::model::{Blame, BlameCommit};
        let mut buf = Buffer::from_text(1, None, text, false);
        let commit = BlameCommit { sha: "s".into(), author: "Ann".into(), time: 0, offset_minutes: 0, summary: "msg".into() };
        let data = Blame { commits: vec![commit], lines: vec![0; 8] };
        buf.blame = Some(BlameState { data: Some(data), ..BlameState::new() });
        buf
    }

    /// Chars of row 0 and the screen column where the blame text starts.
    fn blame_col(buf: &mut Buffer, theme: &Theme, w: u16, follow: bool) -> (Vec<String>, Option<usize>, u16) {
        let area = Rect::new(0, 0, w, 3);
        let mut screen = Screen::empty(area);
        let o = RenderOpts { inline_blame: true, follow, ..resize_opts(theme) };
        let out = render(buf, &mut screen, area, &o);
        let row: Vec<String> = (0..w).map(|x| screen[(x, 0)].symbol().to_string()).collect();
        let at = row.iter().position(|c| c == "A").filter(|&i| row[i..].concat().starts_with("Ann,"));
        (row, at, out.text_area.x)
    }

    #[test]
    fn inline_blame_follows_the_fold_placeholder() {
        let theme = Theme::default();
        let mut buf = blamed("fn x() {\n    a;\n}\nz\n");
        buf.fold_collapse(false);
        let (f, last) = buf.view.folds.header_at(0).expect("folded");
        let ph = placeholder(&buf.rope, f, last).width();
        let (_, at, tx) = blame_col(&mut buf, &theme, 60, true);
        assert_eq!(at, Some(tx as usize + 8 + ph + 4));
    }

    #[test]
    fn inline_blame_starts_after_a_virtual_caret() {
        let theme = Theme::default();
        let mut buf = blamed("ab\n");
        buf.view.sels = Selections::single(Selection::virtual_point(2, 20));
        let (_, at, tx) = blame_col(&mut buf, &theme, 80, true);
        assert_eq!(at, Some(tx as usize + 20 + 4));
    }

    #[test]
    fn inline_blame_follows_and_hides_under_horizontal_scroll() {
        let theme = Theme::default();
        let mut buf = blamed(&format!("{}\n", "x".repeat(60)));
        buf.view.sels = Selections::single(Selection::point(60));
        let (_, at, _) = blame_col(&mut buf, &theme, 50, true);
        assert!(buf.view.scroll_col > 0);
        assert_eq!(at, None, "fewer than 10 cells left after the scrolled text");
        // A short line scrolled out of view leaves the blame left of the viewport.
        let mut short = blamed("ab\n");
        short.view.scroll_col = 50;
        assert_eq!(blame_col(&mut short, &theme, 80, false).1, None);
        // Partly scrolled: the text starts `gc - scroll_col` cells in.
        let mut mid = blamed("ab\n");
        mid.view.scroll_col = 3;
        let (_, at, tx) = blame_col(&mut mid, &theme, 80, false);
        assert_eq!(at, Some(tx as usize + 2 + 4 - 3));
    }

    fn render_at(buf: &mut Buffer, theme: &Theme, w: u16, h: u16) {
        let area = Rect::new(0, 0, w, h);
        render(buf, &mut Screen::empty(area), area, &resize_opts(theme));
    }

    #[test]
    fn a_virtual_caret_is_drawn_and_followed_on_its_column() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "ab\n", false);
        buf.view.sels = Selections::single(Selection::virtual_point(2, 200));
        let area = Rect::new(0, 0, 40, 5);
        let out = render(&mut buf, &mut Screen::empty(area), area, &resize_opts(&theme));
        let cursor = out.cursor.expect("cursor");
        assert!(buf.view.scroll_col > 0);
        assert_eq!(cursor.0, out.text_area.x + (200 - buf.view.scroll_col) as u16);
    }

    #[test]
    fn virtual_columns_are_ignored_under_soft_wrap() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "ab\n", false);
        buf.view.sels = Selections::single(Selection::virtual_point(2, 200));
        buf.view.wrap.on = true;
        let area = Rect::new(0, 0, 40, 5);
        let out = render(&mut buf, &mut Screen::empty(area), area, &resize_opts(&theme));
        assert_eq!(out.cursor.map(|c| c.0), Some(out.text_area.x + 2));
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

    #[test]
    fn narrowing_a_wrapped_pane_keeps_a_bottom_row_caret_on_screen() {
        let theme = Theme::default();
        let text: String = (0..12).map(|_| "aaaa bbbb cccc dddd eeee\n").collect();
        let mut buf = Buffer::from_text(1, None, &text, false);
        buf.view.wrap.on = true;
        buf.goto_line(4, 0);
        render_at(&mut buf, &theme, 40, 5);
        assert_eq!(buf.view.scroll_line, 0, "the caret is on the bottom row");
        // Narrower, each line takes more rows: line 4 is past the old height.
        render_at(&mut buf, &theme, 14, 5);
        assert!(buf.view.scroll_line >= 3, "scrolled to the caret, top line {}", buf.view.scroll_line);
    }

    #[test]
    fn filler_rows_have_no_cells_and_click_to_the_line_end() {
        let rope = Rope::from_str("ab\ncd\n");
        let folds = crate::editor::fold::Folds::default();
        let wrap = crate::editor::wrap::Wrap::new(false);
        let fill = [crate::editor::rows::Filler { line: 0, rows: 2, block: 1, bg: None, top: false }];
        let rows = Rows::view(&folds, &wrap, &fill, &rope, 0);
        let filler = Row { line: 0, seg: 1 };
        assert_eq!(row_x_to_char(&rope, &rows, filler, 0, 4), (2, false));
        assert_eq!(row_x_to_char(&rope, &rows, filler, 30, 4), (2, false));
        let mut cells = 0;
        layout_row(&rope, &rows, filler, 4, 80, &mut |_, _, _| {
            cells += 1;
            true
        });
        assert_eq!(cells, 0);
        let g = row_geom(&rope, &rows, Row { line: 0, seg: 2 });
        assert_eq!((g.start, g.end), (2, 2));
        assert_eq!(row_x_to_char(&rope, &rows, Row::line(1), 0, 4), (3, false), "text rows are unchanged");
    }

    fn guide_opts(theme: &Theme, on: bool) -> RenderOpts<'_> {
        RenderOpts { indent_guides: on, ..resize_opts(theme) }
    }

    /// Render `buf` at `w` x `h`; the screen and the text column.
    fn draw_guides(buf: &mut Buffer, theme: &Theme, on: bool, w: u16, h: u16) -> (Screen, u16) {
        let area = Rect::new(0, 0, w, h);
        let mut screen = Screen::empty(area);
        let out = render(buf, &mut screen, area, &guide_opts(theme, on));
        (screen, out.text_area.x)
    }

    fn bars(screen: &Screen, y: u16, w: u16) -> Vec<u16> {
        (0..w).filter(|&x| screen[(x, y)].symbol() == "│").collect()
    }

    #[test]
    fn indent_guides_only_in_leading_whitespace() {
        let theme = Theme::default();
        let text = "fn a() {\n    if x {\n        y();\n    }\n\n}\n";
        let mut buf = Buffer::from_text(1, None, text, false);
        buf.indent = IndentStyle::spaces(4);
        let (screen, x0) = draw_guides(&mut buf, &theme, true, 40, 8);
        assert!(bars(&screen, 0, 40).is_empty());
        assert_eq!(bars(&screen, 1, 40), [x0], "the space at column 8 of `if x {{` stays a space");
        assert_eq!(bars(&screen, 2, 40), [x0, x0 + 4]);
        assert_eq!(screen[(x0 + 8, 2)].symbol(), "y");
        assert_eq!(bars(&screen, 3, 40), [x0]);
        assert!(bars(&screen, 4, 40).is_empty(), "min(4, 0) = 0");
        assert_eq!(screen[(x0, 2)].fg, theme.indent_guide.fg.unwrap());
        // A selection keeps its background under the guides.
        let (a, b) = (buf.rope.line_to_char(2), buf.rope.line_to_char(3));
        buf.view.sels = Selections::single(Selection::new(a, b));
        let (screen, x0) = draw_guides(&mut buf, &theme, true, 40, 8);
        assert_eq!(bars(&screen, 2, 40), [x0, x0 + 4]);
        assert_eq!(screen[(x0, 2)].bg, theme.selection_bg);
        assert_eq!(screen[(x0 + 4, 2)].bg, theme.selection_bg);
        // Off: none.
        let (screen, _) = draw_guides(&mut buf, &theme, false, 40, 8);
        assert!((0..8).all(|y| bars(&screen, y, 40).is_empty()));
        // Hard tabs: a guide in the first cell of each tab.
        let mut buf = Buffer::from_text(1, None, "a\n\t\tb\n", false);
        buf.indent = IndentStyle::tabs(4);
        let (screen, x0) = draw_guides(&mut buf, &theme, true, 40, 4);
        assert_eq!(bars(&screen, 1, 40), [x0, x0 + 4]);
        assert_eq!(screen[(x0 + 1, 1)].symbol(), " ");
        assert_eq!(screen[(x0 + 8, 1)].symbol(), "b");
    }

    #[test]
    fn indent_guides_continue_through_blank_lines() {
        let theme = Theme::default();
        let text = "fn a() {\n    if x {\n        y();\n\n  \n        z();\n    }\n}\n";
        let mut buf = Buffer::from_text(1, None, text, false);
        buf.indent = IndentStyle::spaces(4);
        buf.view.sels = Selections::point(buf.rope.line_to_char(3));
        let (screen, x0) = draw_guides(&mut buf, &theme, true, 40, 10);
        assert_eq!(bars(&screen, 3, 40), [x0, x0 + 4], "virtual cells of an empty line");
        assert_eq!(bars(&screen, 4, 40), [x0, x0 + 4]);
        assert_eq!(screen[(x0 + 1, 4)].symbol(), " ");
        for x in [x0, x0 + 4] {
            assert_eq!(screen[(x, 3)].bg, theme.current_line_bg);
        }
        // Lines 2-4 selected whole.
        let (a, b) = (buf.rope.line_to_char(2), buf.rope.line_to_char(5));
        buf.view.sels = Selections::single(Selection::new(a, b));
        let (screen, x0) = draw_guides(&mut buf, &theme, true, 40, 10);
        for y in [3, 4] {
            assert_eq!(bars(&screen, y, 40), [x0, x0 + 4]);
            // The line end and the real spaces are selected.
            assert_eq!(screen[(x0, y)].bg, theme.selection_bg, "row {y}");
        }
        let (screen, _) = draw_guides(&mut buf, &theme, false, 40, 10);
        assert!(bars(&screen, 3, 40).is_empty() && bars(&screen, 4, 40).is_empty());
    }

    #[test]
    fn blank_line_guides_sit_under_a_secondary_caret() {
        let theme = Theme { secondary_caret: Style::default().fg(Color::Red).bg(Color::Blue), ..Theme::default() };
        let text = "fn a() {\n    if x {\n        y();\n\n        z();\n    }\n}\n";
        let mut buf = Buffer::from_text(1, None, text, false);
        buf.indent = IndentStyle::spaces(4);
        let (a, b) = (buf.rope.line_to_char(0), buf.rope.line_to_char(3));
        buf.view.sels = Selections::new(vec![Selection::point(a), Selection::point(b)], 0);
        let (screen, x0) = draw_guides(&mut buf, &theme, true, 40, 10);
        assert_eq!(bars(&screen, 3, 40), [x0, x0 + 4]);
        assert_eq!(screen[(x0, 3)].fg, Color::Red, "the caret style wins over the guide");
        assert_eq!(screen[(x0, 3)].bg, Color::Blue);
        assert_eq!(screen[(x0 + 4, 3)].fg, theme.indent_guide.fg.unwrap());
    }

    #[test]
    fn whitespace_only_last_line_without_level_gets_no_guides() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "a\n    ", false);
        buf.indent = IndentStyle::spaces(4);
        let (screen, _) = draw_guides(&mut buf, &theme, true, 40, 4);
        assert!(bars(&screen, 1, 40).is_empty());
    }

    #[test]
    fn patch_buffers_have_no_indent_guides() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "@@ -1 +1 @@\n     deep\n same\n", false);
        buf.patch = Some(crate::editor::patch::Patch::parse(&buf.text()));
        buf.indent = IndentStyle::spaces(4);
        let (screen, _) = draw_guides(&mut buf, &theme, true, 40, 6);
        assert!((0..6).all(|y| bars(&screen, y, 40).is_empty()));
    }

    fn ruler_opts<'a>(theme: &'a Theme, rulers: &'a [usize]) -> RenderOpts<'a> {
        RenderOpts { rulers, ..resize_opts(theme) }
    }

    /// Render `buf` at `w` x `h` with `o`; the screen and the text area.
    fn draw_with(buf: &mut Buffer, o: &RenderOpts, w: u16, h: u16) -> (Screen, Rect) {
        let area = Rect::new(0, 0, w, h);
        let mut screen = Screen::empty(area);
        let out = render(buf, &mut screen, area, o);
        (screen, out.text_area)
    }

    #[test]
    fn rulers_fill_the_cells_past_the_text() {
        let theme = Theme::default();
        let text = format!("ab\n{}\n\tc\n", "x".repeat(12));
        let mut buf = Buffer::from_text(1, None, &text, false);
        buf.view.set_sel(Selection::point(0));
        let rulers = [4, 10];
        let (screen, ta) = draw_with(&mut buf, &ruler_opts(&theme, &rulers), 40, 6);
        let x0 = ta.x;
        assert_eq!(bars(&screen, 0, 40), [x0 + 4, x0 + 10]);
        assert_eq!(screen[(x0 + 4, 0)].fg, theme.ruler.fg.unwrap());
        assert_eq!(screen[(x0 + 4, 0)].bg, theme.current_line_bg, "the caret line's background stays under the ruler");
        assert!(bars(&screen, 1, 40).is_empty(), "12 chars reach both rulers");
        assert_eq!(screen[(x0 + 4, 1)].symbol(), "x");
        assert_eq!(screen[(x0 + 10, 1)].symbol(), "x");
        assert_eq!(screen[(x0 + 4, 2)].symbol(), "c", "the tab ends at cell 4, c is on cell 4");
        assert_eq!(bars(&screen, 2, 40), [x0 + 10]);
        let (screen, _) = draw_with(&mut buf, &ruler_opts(&theme, &[]), 40, 6);
        assert!((0..3).all(|y| bars(&screen, y, 40).is_empty()));
    }

    #[test]
    fn rulers_follow_horizontal_scroll() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "ab\n0123456789\n", false);
        buf.view.scroll_col = 5;
        let rulers = [2, 8, 11];
        let o = RenderOpts { follow: false, ..ruler_opts(&theme, &rulers) };
        let (screen, ta) = draw_with(&mut buf, &o, 40, 3);
        let x0 = ta.x;
        assert_eq!(bars(&screen, 0, 40), [x0 + 3, x0 + 6], "cells 8 and 11 minus the scroll; cell 2 is scrolled off");
        assert_eq!(bars(&screen, 1, 40), [x0 + 6], "the 10-char line reaches cell 8, not 11");
        assert_eq!(screen[(x0 + 3, 1)].symbol(), "8");
    }

    #[test]
    fn rulers_fit_the_wrapped_view_only() {
        let theme = Theme::default();
        let text = format!("ab\n{}\n", "y".repeat(25));
        let mut buf = Buffer::from_text(1, None, &text, false);
        buf.view.wrap.on = true;
        let rulers = [9, 24];
        let o = ruler_opts(&theme, &rulers);
        let (_, probe) = draw_with(&mut buf, &o, 40, 6);
        let w = probe.x + 20;
        let (screen, ta) = draw_with(&mut buf, &o, w, 6);
        assert_eq!(ta.width, 20);
        let x0 = ta.x;
        assert_eq!(bars(&screen, 0, w), [x0 + 9], "cell 24 is past the view");
        assert!(bars(&screen, 1, w).is_empty(), "the first row of the y line (19 cells) passes the ruler");
        assert_eq!(screen[(x0, 2)].symbol(), "y");
        assert_eq!(screen[(x0 + 5, 2)].symbol(), "y");
        assert_eq!(screen[(x0 + 6, 2)].symbol(), " ", "the first row took 19 cells, the continuation has 6");
        assert_eq!(bars(&screen, 2, w), [x0 + 9], "the continuation row ends before the ruler");
        for y in 0..6 {
            assert!(bars(&screen, y, w).iter().all(|&x| x < ta.right()), "row {y}");
        }
    }

    #[test]
    fn rulers_never_cover_hints() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, HINTED, false);
        buf.inlays = hinted_line();
        buf.view.set_sel(Selection::new(18, 5));
        let rulers = [26, 32];
        let (screen, ta) = draw_with(&mut buf, &ruler_opts(&theme, &rulers), 60, 3);
        let x0 = ta.x;
        assert_eq!(screen[(x0 + 26, 0)].symbol(), " ");
        assert_eq!(screen[(x0 + 26, 0)].bg, theme.current_line_bg);
        assert_eq!(screen[(x0 + 28, 0)].fg, theme.inlay_hint.fg.unwrap());
        assert_eq!(bars(&screen, 0, 60), [x0 + 32]);
    }

    #[test]
    fn rulers_go_under_the_fold_placeholder() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "fn x() {\n    a;\n}\nz\n", false);
        buf.fold_collapse(false);
        let (f, last) = buf.view.folds.header_at(0).expect("folded");
        let w = 8 + placeholder(&buf.rope, f, last).width();
        let rulers = [w - 1];
        let (screen, ta) = draw_with(&mut buf, &ruler_opts(&theme, &rulers), 60, 4);
        let x0 = ta.x;
        let (plain, _) = draw_with(&mut buf, &ruler_opts(&theme, &[]), 60, 4);
        assert_ne!(screen[(x0 + w as u16 - 1, 0)].symbol(), "│");
        assert_eq!(screen[(x0 + w as u16 - 1, 0)].symbol(), plain[(x0 + w as u16 - 1, 0)].symbol());
        assert_eq!(bars(&screen, 1, 60), [x0 + w as u16 - 1]);
    }

    #[test]
    fn blank_guide_levels_scan_is_bounded() {
        let text = format!("        a\n{}        b\n", "\n".repeat(150));
        let rope = Rope::from_str(&text);
        let all = blank_guide_levels(&rope, 0..152, 4);
        assert_eq!(all.len(), 152);
        assert_eq!(all[0], None);
        assert_eq!(all[50], None, "101 lines down");
        assert_eq!(all[51], Some(8));
        assert_eq!(all[100], Some(8));
        assert_eq!(all[101], None, "101 lines up");
        assert_eq!(all[151], None);
        assert_eq!(blank_guide_levels(&rope, 60..70, 4), all[60..70]);
        let blanks = Rope::from_str("\n  \n\n");
        assert!(blank_guide_levels(&blanks, 0..4, 4).iter().all(Option::is_none));
    }

    use crate::editor::fold::Folds;
    use crate::editor::inlay::{Inlay, InlayHints, InlineValues, NO_VALUES};

    const HINTED: &str = "let x = foo(1, 2);";

    fn hinted_line() -> InlayHints {
        let mut h = InlayHints::new();
        h.set(
            vec![
                Inlay::new(5, ": i32", false, false, false).unwrap(),
                Inlay::new(12, "a:", false, true, true).unwrap(),
                Inlay::new(18, "-> u8", true, false, false).unwrap(),
            ],
            0,
            0..1,
        );
        h
    }

    #[test]
    fn layout_walk_puts_hints_before_their_char() {
        let rope = Rope::from_str(HINTED);
        let h = hinted_line();
        let mut cells = Vec::new();
        layout_line_inlays(&rope, 0, 4, usize::MAX, h.all(), None, &mut |g, c| {
            cells.push((g.to_string(), c.char_idx, c.col, c.width, c.kind));
            true
        });
        let at = |i: usize| cells.iter().position(|c| c.1 == i && c.4 == CellKind::Text).unwrap();
        let k = at(5);
        assert_eq!(cells[k - 1], (": i32".into(), 5, 5, 5, CellKind::InlayLabel));
        assert_eq!(cells[k], (" ".into(), 5, 10, 1, CellKind::Text));
        let k = at(12);
        assert_eq!(cells[k - 2], ("a:".into(), 12, 17, 2, CellKind::InlayLabel));
        assert_eq!(cells[k - 1], (" ".into(), 12, 19, 1, CellKind::InlayPad));
        assert_eq!(cells[k], ("1".into(), 12, 20, 1, CellKind::Text));
        let n = cells.len();
        assert_eq!(cells[n - 2], (" ".into(), 18, 26, 1, CellKind::InlayPad));
        assert_eq!(cells[n - 1], ("-> u8".into(), 18, 27, 5, CellKind::InlayLabel));
        let last = &cells[n - 1];
        assert_eq!(last.2 + last.3, 18 + 5 + 3 + 6);
        // Without hints the walk is today's: columns 0..18.
        let mut plain = Vec::new();
        layout_line(&rope, 0, 4, usize::MAX, &mut |_, c| {
            plain.push((c.char_idx, c.col, c.kind));
            true
        });
        assert_eq!(plain.len(), 18);
        assert!(plain.iter().enumerate().all(|(i, &(idx, col, kind))| (idx, col, kind) == (i, i, CellKind::Text)));
        // A walk cut short never reaches the line-end hint.
        let mut cut = 0;
        layout_line_inlays(&rope, 0, 4, 10, h.all(), None, &mut |_, c| {
            cut += usize::from(c.char_idx == 18);
            true
        });
        assert_eq!(cut, 0);
    }

    #[test]
    fn hint_cells_map_clicks_to_anchor_and_caret_before_type_hints() {
        let rope = Rope::from_str(HINTED);
        let h = hinted_line();
        let folds = Folds::default();
        let rows = Rows::new(&folds, rope.len_lines()).with_inlays(&h, &NO_VALUES);
        let row = Row::line(0);
        for x in 5..10 {
            assert_eq!(row_x_to_char(&rope, &rows, row, x, 4), (5, false), "x {x}");
        }
        assert_eq!(row_x_to_char(&rope, &rows, row, 10, 4), (5, false));
        assert_eq!(char_to_row_x(&rope, &rows, 5, false, 4).1, 5, "before the type hint");
        assert_eq!(char_to_row_x(&rope, &rows, 12, false, 4).1, 20, "after `a: `");
        assert_eq!(char_to_row_x(&rope, &rows, 18, false, 4).1, 26, "line end before the line-end hint's pad");
        assert_eq!(text_end_x(&rope, &rows, 0, 4), 32);
        for i in 0..18 {
            let (_, x) = char_to_row_x(&rope, &rows, i, false, 4);
            assert_eq!(row_x_to_char(&rope, &rows, row, x, 4).0, i, "round trip of char {i}");
        }
        // Past the end, the line end.
        assert_eq!(row_x_to_char(&rope, &rows, row, 60, 4), (18, false));
        // Text columns under row x.
        assert_eq!(text_col_at(&rope, &rows, 0, 3, 4), 3);
        assert_eq!(text_col_at(&rope, &rows, 0, 10, 4), 5, "the char after the first hint");
        assert_eq!(text_col_at(&rope, &rows, 0, 20, 4), 12);
        assert_eq!(text_col_at(&rope, &rows, 0, 32 + 4, 4), 18 + 4, "past the end");
    }

    #[test]
    fn virtual_caret_past_a_line_end_hint() {
        let rope = Rope::from_str(HINTED);
        let h = hinted_line();
        let folds = Folds::default();
        let rows = Rows::new(&folds, rope.len_lines()).with_inlays(&h, &NO_VALUES);
        assert_eq!(caret_col(&rope, &rows, &Selection::point(18), 4), 26);
        assert_eq!(caret_col(&rope, &rows, &Selection::virtual_point(18, 22), 4), 36);
        assert_eq!(text_col_at(&rope, &rows, 0, 36, 4), 22);
    }

    #[test]
    fn typing_at_a_type_hint_anchor_keeps_the_caret_before_it() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, HINTED, false);
        buf.inlays = hinted_line();
        buf.view.set_sel(Selection::point(5));
        buf.insert_char('y', &[]);
        assert_eq!(buf.inlays.all()[0].at, 6, "the type hint moved after the typed char");
        let head = buf.view.sel().head;
        assert_eq!(head, 6);
        assert_eq!(char_to_row_x(&buf.rope, &buf.rows(), head, false, 4).1, 6);
        let area = Rect::new(0, 0, 60, 3);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &resize_opts(&theme));
        let x0 = out.text_area.x;
        let row: String = (x0..x0 + 12).map(|x| screen[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "let xy: i32 ");
        assert_eq!(out.cursor, Some((x0 + 6, 0)));
        buf.view.set_sel(Selection::point(5));
        assert_eq!(char_to_row_x(&buf.rope, &buf.rows(), 5, false, 4).1, 5);

        // A parameter hint stays put: the typed char goes after it.
        let mut buf = Buffer::from_text(1, None, HINTED, false);
        buf.inlays = hinted_line();
        buf.view.set_sel(Selection::point(12));
        buf.insert_char('z', &[]);
        assert_eq!(buf.inlays.all()[1].at, 12);
        let head = buf.view.sel().head;
        assert_eq!(head, 13);
        // `let x` + `: i32`, 7 chars to the argument, `a: `, then `z`.
        assert_eq!(char_to_row_x(&buf.rope, &buf.rows(), head, false, 4).1, 12 + 5 + 3 + 1);
    }

    #[test]
    fn virtual_col_past_hinted_tab_line_round_trips() {
        let rope = Rope::from_str("a\tb\n");
        let mut h = InlayHints::new();
        h.set(vec![Inlay::new(1, "xyz", false, false, false).unwrap()], 0, 0..1);
        let folds = Folds::default();
        let rows = Rows::new(&folds, rope.len_lines()).with_inlays(&h, &NO_VALUES);
        // `a`, 3 hint cells, the tab from column 4 to 8, `b`: the end is at x 9.
        assert_eq!(char_to_row_x(&rope, &rows, 3, false, 4).1, 9);
        for click in 9..14 {
            let col = text_col_at(&rope, &rows, 0, click, 4);
            let sel = Selection::virtual_point(3, col);
            assert_eq!(caret_col(&rope, &rows, &sel, 4), click, "click {click} is virtual column {col}");
        }
    }

    fn one_value(line: usize, at: usize, label: &str) -> InlineValues {
        let mut v = InlineValues::new();
        let i = Inlay { at, label: label.to_string(), width: label.chars().count(), pad_left: true, pad_right: false, param: false };
        v.set(std::collections::BTreeMap::from([(line, i)]));
        v
    }

    #[test]
    fn the_value_comes_after_the_line_end_hints() {
        let rope = Rope::from_str("ab\n");
        let mut h = InlayHints::new();
        h.set(vec![Inlay::new(2, ": T", false, false, false).unwrap()], 0, 0..1);
        let v = one_value(0, 2, "x = 1");
        let tail = v.on_line(0);
        let mut cells = Vec::new();
        layout_line_inlays(&rope, 0, 4, usize::MAX, h.all(), tail, &mut |g, c| {
            cells.push((g.to_string(), c.col, c.kind));
            true
        });
        let want = [
            ("a", 0, CellKind::Text),
            ("b", 1, CellKind::Text),
            (": T", 2, CellKind::InlayLabel),
            (" ", 5, CellKind::InlayPad),
            ("x = 1", 6, CellKind::InlayLabel),
        ];
        assert_eq!(cells, want.map(|(g, c, k)| (g.to_string(), c, k)));
    }

    #[test]
    fn a_cut_line_draws_no_value() {
        let rope = Rope::from_str("abcd\n");
        let v = one_value(0, 4, "x = 1");
        let mut n = 0;
        layout_line_inlays(&rope, 0, 4, 2, &[], v.on_line(0), &mut |_, c| {
            n += usize::from(c.is_inlay());
            true
        });
        assert_eq!(n, 0);
    }

    #[test]
    fn text_col_at_counts_the_value() {
        let rope = Rope::from_str("ab\n");
        let folds = Folds::default();
        let v = one_value(0, 2, "x = 12");
        let h = InlayHints::new();
        let rows = Rows::new(&folds, rope.len_lines()).with_inlays(&h, &v);
        // The value is 6 cells and a left pad: the text ends at row x 2 + 7 = 9.
        // Its cells are not text columns, so a click at row x 10 is one column
        // past the line end: 2 + (10 - 9) = 3. Without the value it is 10.
        assert_eq!(text_end_x(&rope, &rows, 0, 4), 9);
        assert_eq!(text_col_at(&rope, &rows, 0, 10, 4), 3);
        let plain = Rows::new(&folds, rope.len_lines());
        assert_eq!(text_col_at(&rope, &plain, 0, 10, 4), 10);
    }

    #[test]
    fn render_draws_values_in_the_hint_style() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "ab\ncd\n", false);
        buf.inline_values = one_value(0, 2, "x = 1");
        buf.view.set_sel(Selection::point(3));
        let area = Rect::new(0, 0, 30, 3);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &resize_opts(&theme));
        let x0 = out.text_area.x;
        let row: String = (x0..x0 + 8).map(|x| screen[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "ab x = 1");
        let hint = theme.inlay_hint;
        for x in x0 + 3..x0 + 8 {
            assert_eq!((screen[(x, 0)].fg, screen[(x, 0)].bg), (hint.fg.unwrap(), hint.bg.unwrap()), "x {x}");
        }
        assert_ne!(screen[(x0 + 2, 0)].fg, hint.fg.unwrap(), "the pad is plain background");
        let row1: String = (x0..x0 + 8).map(|x| screen[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row1.trim_end(), "cd");
    }

    #[test]
    fn a_wrapped_line_has_its_value_on_the_last_row() {
        let rope = Rope::from_str("abcdefgh\n");
        let folds = Folds::default();
        let mut wrap = crate::editor::wrap::Wrap::new(true);
        wrap.set_geometry(5, 4, 0);
        let v = one_value(0, 8, "v");
        let h = InlayHints::new();
        let rows = Rows::view(&folds, &wrap, &[], &rope, 0).with_inlays(&h, &v);
        let walk = |seg: i32| {
            let mut cells = Vec::new();
            layout_row(&rope, &rows, Row { line: 0, seg }, 4, 80, &mut |g, c, rx| {
                cells.push((g.to_string(), rx, c.is_inlay()));
                true
            });
            cells
        };
        assert!(rows.line_wrap(0).unwrap().segs() >= 2);
        let last = i32::from(rows.line_wrap(0).unwrap().segs()) - 1;
        for seg in 0..last {
            assert!(walk(seg).iter().all(|c| !c.2), "seg {seg} has no value");
        }
        let cells = walk(last);
        let tail: Vec<_> = cells.iter().filter(|c| c.2).map(|c| c.0.as_str()).collect();
        assert_eq!(tail, [" ", "v"]);
    }

    #[test]
    fn no_hints_no_change() {
        let rope = Rope::from_str("\ta\tb日本x\u{1}y\n  tail 😀\nlast");
        let folds = Folds::default();
        let none = InlayHints::default();
        let plain = Rows::new(&folds, rope.len_lines());
        let with = Rows::new(&folds, rope.len_lines()).with_inlays(&none, &NO_VALUES);
        for line in 0..rope.len_lines() {
            let walk = |rows: &Rows| {
                let mut out = Vec::new();
                layout_row(&rope, rows, Row::line(line), 4, 80, &mut |g, c, rx| {
                    out.push((g.to_string(), c.char_idx, c.col, c.width, c.kind, rx));
                    true
                });
                out
            };
            assert_eq!(walk(&plain), walk(&with), "line {line}");
            for x in 0..30 {
                assert_eq!(row_x_to_char(&rope, &plain, Row::line(line), x, 4), row_x_to_char(&rope, &with, Row::line(line), x, 4));
                assert_eq!(text_col_at(&rope, &with, line, x, 4), x);
            }
        }
    }

    #[test]
    fn render_draws_hints_in_their_style() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, HINTED, false);
        buf.inlays = hinted_line();
        buf.view.set_sel(Selection::new(18, 5));
        let area = Rect::new(0, 0, 60, 3);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &resize_opts(&theme));
        let x0 = out.text_area.x;
        let row: String = (x0..x0 + 32).map(|x| screen[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "let x: i32 = foo(a: 1, 2); -> u8");
        let hint = theme.inlay_hint;
        for x in (x0 + 5..x0 + 10).chain(x0 + 17..x0 + 19).chain(x0 + 27..x0 + 32) {
            assert_eq!((screen[(x, 0)].fg, screen[(x, 0)].bg), (hint.fg.unwrap(), hint.bg.unwrap()), "x {x}");
        }
        assert_eq!(screen[(x0 + 10, 0)].bg, theme.selection_bg, "the char after the hint is selected");
        assert_eq!(screen[(x0 + 19, 0)].bg, theme.current_line_bg, "the pad is plain background");
        assert_eq!(screen[(x0 + 26, 0)].bg, theme.current_line_bg, "so is the left pad of the line-end hint");
        assert_eq!(out.cursor, Some((x0 + 5, 0)), "the caret at a type hint's anchor stands before the hint");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn semantic_tokens_draw_over_tree_sitter() {
        use ratatui::style::{Color, Modifier};
        let mut theme = Theme::default();
        let sem = Style::default().fg(Color::Magenta).add_modifier(Modifier::UNDERLINED);
        theme.syntax.insert("test.semantic".into(), sem);
        let path = std::path::PathBuf::from("/tmp/sem.rs");
        let mut buf = Buffer::from_text(1, Some(path.clone()), "fn main() {}\n", false);
        buf.syntax = crate::editor::syntax::Syntax::for_path(&path, &theme);
        buf.view.set_sel(Selection::point(13));
        let area = Rect::new(0, 0, 40, 3);
        let o = resize_opts(&theme);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &o);
        let x0 = out.text_area.x;
        assert_eq!(screen[(x0 + 3, 0)].fg, theme.capture_style("function").unwrap().fg.unwrap(), "tree-sitter styles main");
        buf.semantic.set(vec![(3..7, "test.semantic".into()), (0..2, "nothing.known".into())]);
        let mut screen = Screen::empty(area);
        render(&mut buf, &mut screen, area, &o);
        for x in x0 + 3..x0 + 7 {
            assert_eq!(screen[(x, 0)].fg, Color::Magenta, "x {x}");
            assert!(screen[(x, 0)].modifier.contains(Modifier::UNDERLINED), "x {x}");
        }
        assert_eq!(
            screen[(x0, 0)].fg,
            theme.capture_style("keyword").unwrap().fg.unwrap(),
            "an unresolved capture keeps tree-sitter's style"
        );
        assert_ne!(screen[(x0 + 7, 0)].fg, Color::Magenta);
    }

    #[cfg(feature = "lang-rust")]
    fn rust_buf(text: &str, theme: &Theme) -> Buffer {
        let path = std::path::PathBuf::from("/tmp/brk.rs");
        let mut buf = Buffer::from_text(1, Some(path.clone()), text, false);
        buf.syntax = crate::editor::syntax::Syntax::for_path(&path, theme);
        buf
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn bracket_colours_paint_by_depth() {
        use ratatui::style::Modifier;
        let theme = Theme::default();
        let text = "fn f() { g(h([1])) }\n";
        let mut buf = rust_buf(text, &theme);
        buf.view.set_sel(Selection::point(text.chars().count() - 1));
        let on = RenderOpts { bracket_colours: true, ..resize_opts(&theme) };
        let (screen, ta) = draw_with(&mut buf, &on, 40, 3);
        let x0 = ta.x;
        let rainbow = [theme.bracket_1.fg.unwrap(), theme.bracket_2.fg.unwrap(), theme.bracket_3.fg.unwrap()];
        // (char index, depth) of every bracket, from the depth rule.
        let brackets = [(4, 0), (5, 0), (7, 0), (10, 1), (12, 2), (13, 3), (15, 3), (16, 2), (17, 1), (19, 0)];
        for (i, d) in brackets {
            let cell = &screen[(x0 + i as u16, 0)];
            assert_eq!(cell.fg, rainbow[d % 3], "char {i} depth {d}");
        }
        let braces = [7u16, 19];
        for x in braces {
            let cell = &screen[(x0 + x, 0)];
            assert_eq!(cell.bg, theme.bracket_match.bg.unwrap(), "pair background at {x}");
            assert!(cell.modifier.contains(Modifier::BOLD), "pair bold at {x}");
        }
        let off = RenderOpts { bracket_colours: false, ..resize_opts(&theme) };
        let (plain, _) = draw_with(&mut buf, &off, 40, 3);
        for (i, _) in brackets {
            assert!(!rainbow.contains(&plain[(x0 + i as u16, 0)].fg), "off: char {i}");
        }
        for x in [3u16, 9, 11] {
            assert_eq!(screen[(x0 + x, 0)].fg, plain[(x0 + x, 0)].fg, "non-bracket char {x} keeps its syntax fg");
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn bracket_colours_follow_an_edit() {
        use crate::editor::undo::TxKind;
        let theme = Theme::default();
        let mut buf = rust_buf("fn f() { g(1) }\n", &theme);
        buf.view.set_sel(Selection::point(0));
        let on = RenderOpts { bracket_colours: true, follow: false, ..resize_opts(&theme) };
        let (before, ta) = draw_with(&mut buf, &on, 40, 3);
        let x0 = ta.x;
        let rainbow = [theme.bracket_1.fg.unwrap(), theme.bracket_2.fg.unwrap(), theme.bracket_3.fg.unwrap()];
        assert_eq!(before[(x0 + 10, 0)].fg, rainbow[1], "g's ( is at depth 1");
        buf.view.set_sel(Selection::point(9));
        buf.insert("let _ = (); ", TxKind::Typing);
        let (after, _) = draw_with(&mut buf, &on, 40, 3);
        // The same text in a fresh buffer: nothing stale from the first frame.
        let mut fresh = rust_buf("fn f() { let _ = (); g(1) }\n", &theme);
        fresh.view.set_sel(Selection::point(9));
        let (want, _) = draw_with(&mut fresh, &on, 40, 3);
        for x in 0..40 - x0 {
            assert_eq!(after[(x0 + x, 0)].fg, want[(x0 + x, 0)].fg, "cell {x}");
        }
        assert_eq!(after[(x0 + 22, 0)].symbol(), "(", "g's ( moved to cell 22");
        assert_eq!(after[(x0 + 22, 0)].fg, rainbow[1]);
        assert_eq!(after[(x0 + 17, 0)].fg, rainbow[1], "the new ( is a paren at depth 1");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn bracket_colours_keep_selection_and_diagnostics() {
        use crate::lsp::diagnostics::{DiagMarks, Severity, Tags, tests::diag};
        let theme = Theme::default();
        let text = "fn f() { g(1) }\n";
        let mut buf = rust_buf(text, &theme);
        buf.view.set_sel(Selection { anchor: 0, head: text.chars().count(), ..Selection::point(0) });
        let on = RenderOpts { bracket_colours: true, ..resize_opts(&theme) };
        let (screen, ta) = draw_with(&mut buf, &on, 40, 3);
        let cell = &screen[(ta.x + 10, 0)];
        assert_eq!(cell.fg, theme.bracket_2.fg.unwrap(), "the rainbow fg stays under the selection");
        assert_eq!(cell.bg, theme.selection_bg);
        // Unnecessary code is faded: the dim fg wins over the bracket fg.
        let mut d = diag(0, 9, 0, 13, Severity::Hint, "unused");
        d.tags = Tags { unnecessary: true, deprecated: false };
        buf.diag_marks = DiagMarks::build(&[d], &buf.rope);
        buf.view.set_sel(Selection::point(0));
        let (screen, ta) = draw_with(&mut buf, &on, 40, 3);
        assert_eq!(screen[(ta.x + 10, 0)].fg, theme.dim.fg.unwrap());
    }

    #[test]
    fn semantic_tokens_colour_a_buffer_without_a_grammar() {
        use ratatui::style::{Color, Modifier};
        let mut theme = Theme::default();
        let sem = Style::default().fg(Color::Magenta).add_modifier(Modifier::UNDERLINED);
        theme.syntax.insert("test.semantic".into(), sem);
        let mut buf = Buffer::from_text(1, None, "alpha beta\n", false);
        buf.semantic.set(vec![(6..10, "test.semantic".into())]);
        buf.view.set_sel(Selection::point(11));
        let area = Rect::new(0, 0, 40, 3);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &resize_opts(&theme));
        let x0 = out.text_area.x;
        for x in x0 + 6..x0 + 10 {
            assert_eq!(screen[(x, 0)].fg, Color::Magenta, "x {x}");
            assert!(screen[(x, 0)].modifier.contains(Modifier::UNDERLINED), "x {x}");
        }
        for x in x0..x0 + 5 {
            assert_eq!(screen[(x, 0)].fg, theme.fg, "x {x}");
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn doc_comment_tokens_keep_the_doc_comment_colour() {
        let theme = Theme::default();
        let path = std::path::PathBuf::from("/tmp/doc.rs");
        let mut buf = Buffer::from_text(1, Some(path.clone()), "/// doc\nfn main() {}\n", false);
        buf.syntax = crate::editor::syntax::Syntax::for_path(&path, &theme);
        buf.view.set_sel(Selection::point(21));
        buf.semantic.set(vec![(0..7, crate::lsp::semantic::capture_for("comment", &["documentation"]).unwrap())]);
        let area = Rect::new(0, 0, 40, 4);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &resize_opts(&theme));
        let x0 = out.text_area.x;
        let doc = theme.capture_style("comment.documentation").unwrap().fg.unwrap();
        assert_ne!(doc, theme.capture_style("comment").unwrap().fg.unwrap(), "the test can fail");
        for x in x0..x0 + 7 {
            assert_eq!(screen[(x, 0)].fg, doc, "x {x}");
        }
    }

    #[test]
    fn horizontal_scroll_counts_hints() {
        let theme = Theme::default();
        let mut buf = Buffer::from_text(1, None, "0123456789abcde", false);
        buf.inlays.set(vec![Inlay::new(2, "123456789a", false, false, false).unwrap()], 0, 0..1);
        buf.view.set_sel(Selection::point(15));
        let o = resize_opts(&theme);
        let area = Rect::new(0, 0, 27, 3);
        let out = render(&mut buf, &mut Screen::empty(area), area, &o);
        assert_eq!(out.text_area.width, 20);
        let rows = buf.rows();
        let col = caret_col(&buf.rope, &rows, &buf.view.sel(), 4);
        assert_eq!(col, 25);
        assert!(buf.view.scroll_col > 0, "15 chars fit, 25 cells do not");
        assert!((buf.view.scroll_col..buf.view.scroll_col + 20).contains(&col));
        assert_eq!(out.cursor.map(|c| c.0), Some(out.text_area.x + (col - buf.view.scroll_col) as u16));
    }

    #[test]
    fn virtual_head_on_the_end_column_draws_one_caret() {
        let theme = Theme { secondary_caret: Style::default().bg(Color::Blue), ..Theme::default() };
        let mut buf = Buffer::from_text(1, None, HINTED, false);
        buf.inlays = hinted_line();
        buf.view.sels = Selections::new(vec![Selection::point(0), Selection::virtual_point(18, 18)], 0);
        let area = Rect::new(0, 0, 60, 3);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &resize_opts(&theme));
        let x0 = out.text_area.x;
        let blue: Vec<u16> = (x0..x0 + 40).filter(|&x| screen[(x, 0)].bg == Color::Blue).map(|x| x - x0).collect();
        assert_eq!(blue, [26], "the cell before the line-end hint, as caret_col puts it");
    }

    #[test]
    fn scrolled_off_label_start_leaves_no_caret_mid_label() {
        let theme = Theme { secondary_caret: Style::default().bg(Color::Blue), ..Theme::default() };
        let mut buf = Buffer::from_text(1, None, HINTED, false);
        buf.inlays = hinted_line();
        buf.view.sels = Selections::new(vec![Selection::point(5), Selection::point(12)], 1);
        buf.view.scroll_col = 7;
        let area = Rect::new(0, 0, 60, 3);
        let mut screen = Screen::empty(area);
        let out = render(&mut buf, &mut screen, area, &resize_opts(&theme));
        assert_eq!(buf.view.scroll_col, 7);
        let x0 = out.text_area.x;
        let row: String = (x0..x0 + 5).map(|x| screen[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "i32 =", "the label's `: ` is scrolled off");
        assert!((x0..x0 + 20).all(|x| screen[(x, 0)].bg != Color::Blue), "no secondary caret on the visible rest of the label");
    }
}
