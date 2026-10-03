//! The diff pane of the Commit, Log and Stash tabs: side-by-side or unified
//! rows sharing one vertical scroll, unchanged runs folded into one row
//! (a click unfolds it), the current change (what hunk staging and F7 work
//! on), syntax colors, and the changed characters of modified lines.

use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;

use ratatui::buffer::Buffer as Screen;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use unicode_width::UnicodeWidthChar;

use super::{Btn, put};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::Effect;
use crate::git::GitRequest;
use crate::git::diff::{DiffDoc, DiffRow, RowKind, strip_eol};
use crate::git::model::DiffKey;

/// Equal lines kept around a change when unchanged runs are folded.
const CONTEXT: usize = 3;

/// Files above this size are not syntax highlighted in the diff.
const SYNTAX_MAX_BYTES: usize = 512 * 1024;

/// How every diff pane is laid out (Git window settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DiffOpts {
    /// Forced unified (`Some(true)`) or side-by-side; `None` picks by width.
    pub unified: Option<bool>,
    /// Fold unchanged runs.
    pub collapse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewRow {
    /// A row of `DiffDoc::rows` or `DiffDoc::unified`.
    Row(usize),
    /// Hidden equal rows `start..end`.
    Fold { start: usize, end: usize },
}

/// Per-line syntax spans (line-relative char ranges) of one side.
type LineSpans = Vec<Vec<(Range<usize>, Style)>>;

/// The diff view: what is shown, the computed rows, and the shared
/// vertical scroll of both sides.
#[derive(Debug, Default)]
pub(super) struct DiffPane {
    pub key: Option<DiffKey>,
    pub doc: Option<DiffDoc>,
    pub error: Option<String>,
    pub loading: bool,
    /// First visible view row.
    pub scroll: usize,
    pub hscroll: usize,
    /// Rendered unified last frame; the view rows follow that mode.
    pub unified: bool,
    collapse: bool,
    pub page: usize,
    /// The current changed block (`DiffDoc::blocks`): what hunk staging,
    /// discarding and F7 work on.
    pub block: usize,
    /// Where the rows were drawn last frame (mouse mapping).
    rows_rect: Rect,
    view: Vec<ViewRow>,
    /// Row index to view index (a folded row maps to its fold).
    row_view: Vec<usize>,
    /// Folds the user opened, by first hidden row (current mode).
    unfolded: HashSet<usize>,
    /// Syntax spans of the old and new side, computed on first render.
    syntax: Option<(LineSpans, LineSpans)>,
}

impl DiffPane {
    pub fn new(collapse: bool) -> Self {
        Self { collapse, ..Default::default() }
    }

    pub fn request(&mut self, key: DiffKey, cx: &mut Ctx) {
        if self.key.as_ref() != Some(&key) {
            self.doc = None;
            self.scroll = 0;
            self.hscroll = 0;
            self.unfolded.clear();
            self.view.clear();
            self.row_view.clear();
            self.syntax = None;
        }
        self.key = Some(key.clone());
        self.loading = true;
        self.error = None;
        cx.effects.push(Effect::Git(GitRequest::LoadDiff(key)));
    }

    pub fn clear(&mut self) {
        *self = DiffPane { unified: self.unified, collapse: self.collapse, page: self.page, ..Default::default() };
    }

    /// Result for `key`; ignored when the pane shows something else now.
    pub fn set(&mut self, key: &DiffKey, result: anyhow::Result<DiffDoc>) {
        if self.key.as_ref() != Some(key) {
            return;
        }
        self.loading = false;
        match result {
            Ok(doc) => {
                let fresh = self.doc.is_none();
                let blocks = doc.blocks(false).len();
                self.doc = Some(doc);
                self.syntax = None;
                self.error = None;
                self.rebuild();
                // Jump to the first change of a newly opened diff only; a
                // reload keeps the place (and the block, if it still exists).
                if fresh {
                    self.block = 0;
                    self.scroll_to_block();
                }
                self.block = self.block.min(blocks.saturating_sub(1));
            }
            Err(e) => {
                self.doc = None;
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    fn rows(&self) -> &[DiffRow] {
        match &self.doc {
            Some(d) if self.unified => &d.unified,
            Some(d) => &d.rows,
            None => &[],
        }
    }

    /// Recompute the view rows for the current mode, folds and doc.
    fn rebuild(&mut self) {
        let rows = self.rows();
        let mut view = Vec::with_capacity(rows.len());
        let mut row_view = vec![0; rows.len()];
        let any_change = rows.iter().any(|r| r.kind != RowKind::Equal);
        let mut i = 0;
        while i < rows.len() {
            if rows[i].kind != RowKind::Equal {
                row_view[i] = view.len();
                view.push(ViewRow::Row(i));
                i += 1;
                continue;
            }
            let start = i;
            while i < rows.len() && rows[i].kind == RowKind::Equal {
                i += 1;
            }
            // Keep context next to changes; fold the middle of the run.
            let hide_from = if start == 0 { 0 } else { start + CONTEXT };
            let hide_to = if i == rows.len() { i } else { i.saturating_sub(CONTEXT) };
            let fold = self.collapse && any_change && hide_to >= hide_from + 2 && !self.unfolded.contains(&hide_from);
            for (r, slot) in row_view.iter_mut().enumerate().take(i).skip(start) {
                if fold && (hide_from..hide_to).contains(&r) {
                    if r == hide_from {
                        view.push(ViewRow::Fold { start: hide_from, end: hide_to });
                    }
                    *slot = view.len() - 1;
                } else {
                    *slot = view.len();
                    view.push(ViewRow::Row(r));
                }
            }
        }
        self.view = view;
        self.row_view = row_view;
    }

    /// The doc row shown at view row `v` (a fold: its first hidden row).
    fn row_at(&self, v: usize) -> Option<usize> {
        self.view.get(v).map(|r| match *r {
            ViewRow::Row(i) => i,
            ViewRow::Fold { start, .. } => start,
        })
    }

    /// Switch between unified and side-by-side, keeping the line in view.
    fn set_unified(&mut self, unified: bool) {
        if unified == self.unified {
            return;
        }
        let row = self.row_at(self.scroll);
        let target = match (&self.doc, row) {
            (Some(doc), Some(row)) => map_row(doc, row, self.unified),
            _ => 0,
        };
        self.unified = unified;
        self.unfolded.clear();
        self.rebuild();
        self.scroll = self.row_view.get(target).copied().unwrap_or(0);
    }

    /// Fold or unfold unchanged runs, keeping the line in view.
    pub fn set_collapse(&mut self, collapse: bool) {
        if collapse == self.collapse {
            return;
        }
        let row = self.row_at(self.scroll);
        self.collapse = collapse;
        self.unfolded.clear();
        self.rebuild();
        self.scroll = row.and_then(|r| self.row_view.get(r).copied()).unwrap_or(0);
    }

    fn scroll_to_block(&mut self) {
        let Some(doc) = &self.doc else { return };
        if let Some(b) = doc.blocks(self.unified).get(self.block) {
            self.scroll = self.row_view.get(b.start).copied().unwrap_or(0).saturating_sub(2);
        }
    }

    /// Move to the next or previous changed block and scroll to it.
    /// Returns false when there is none in that direction.
    pub fn step_block(&mut self, forward: bool) -> bool {
        let Some(doc) = &self.doc else { return false };
        let n = doc.blocks(self.unified).len();
        let next = if forward { self.block + 1 } else { self.block.wrapping_sub(1) };
        if next >= n {
            return false;
        }
        self.block = next;
        self.scroll_to_block();
        true
    }

    /// A click at screen row `y`: unfold a fold, or make the block there
    /// current.
    pub fn click(&mut self, y: u16) {
        if self.doc.is_none() || !(self.rows_rect.y..self.rows_rect.bottom()).contains(&y) {
            return;
        }
        let v = self.scroll + (y - self.rows_rect.y) as usize;
        match self.view.get(v).copied() {
            Some(ViewRow::Fold { start, .. }) => {
                self.unfolded.insert(start);
                self.rebuild();
            }
            Some(ViewRow::Row(row)) => {
                if let Some(i) = self.doc.as_ref().and_then(|d| d.blocks(self.unified).iter().position(|b| b.contains(&row))) {
                    self.block = i;
                }
            }
            None => {}
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.view.len().saturating_sub(self.page.max(1));
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    /// Syntax spans per line of both sides, from the file's language.
    fn ensure_syntax(&mut self, theme: &Theme) {
        if self.syntax.is_some() {
            return;
        }
        let (Some(doc), Some(key)) = (&self.doc, &self.key) else { return };
        let old_path = match key {
            DiffKey::Change { old: Some(old), .. } | DiffKey::Commit { old: Some(old), .. } => old.as_str(),
            _ => key.rel(),
        };
        self.syntax = Some((side_spans(&doc.old, Path::new(old_path), theme), side_spans(&doc.new, Path::new(key.rel()), theme)));
    }
}

/// Syntax spans of `lines`, split per line (line-relative char ranges).
fn side_spans(lines: &[String], path: &Path, theme: &Theme) -> LineSpans {
    let mut out = vec![Vec::new(); lines.len()];
    if lines.iter().map(String::len).sum::<usize>() > SYNTAX_MAX_BYTES {
        return out;
    }
    let Some(mut syntax) = crate::editor::syntax::detect(path).and_then(|def| crate::editor::syntax::Syntax::new(def, theme)) else {
        return out;
    };
    let rope = ropey::Rope::from_str(&lines.concat());
    let spans = syntax.highlights(&rope, 0, rope.len_lines()).spans;
    // Char offset where each line starts.
    let mut starts = Vec::with_capacity(lines.len());
    let mut at = 0;
    for l in lines {
        starts.push(at);
        at += l.chars().count();
    }
    for (range, style) in spans {
        let mut line = starts.partition_point(|&s| s <= range.start).saturating_sub(1);
        while line < lines.len() && starts[line] < range.end {
            let (s, e) = (range.start.max(starts[line]), range.end.min(starts[line] + lines[line].chars().count()));
            if e > s {
                out[line].push((s - starts[line]..e - starts[line], style));
            }
            line += 1;
        }
    }
    out
}

fn line_spans(s: &LineSpans, l: usize) -> &[(Range<usize>, Style)] {
    s.get(l).map(Vec::as_slice).unwrap_or(&[])
}

/// The row of the other mode showing the same old or new line as `row`.
fn map_row(doc: &DiffDoc, row: usize, from_unified: bool) -> usize {
    let (from, to) = if from_unified { (&doc.unified, &doc.rows) } else { (&doc.rows, &doc.unified) };
    let Some(r) = from.get(row) else { return 0 };
    to.iter().position(|t| (r.right.is_some() && t.right == r.right) || (r.left.is_some() && t.left == r.left)).unwrap_or(0)
}

/// The layout toggles at the right end of a diff header row; returns their
/// buttons.
pub(super) fn render_toggles(
    buf: &mut Screen,
    y: u16,
    left: u16,
    right: u16,
    unified: bool,
    opts: DiffOpts,
    theme: &Theme,
) -> Vec<(Btn, Rect)> {
    let layout = if unified { "[Unified]" } else { "[Side-by-side]" };
    let collapse = if opts.collapse { "[x] Collapse" } else { "[ ] Collapse" };
    let w = (layout.len() + collapse.len() + 2) as u16;
    if right < left + w + 10 {
        return Vec::new();
    }
    let mut x = right - w;
    let mut out = Vec::new();
    buf.set_stringn(x, y, " ", 1, theme.title);
    x += 1;
    out.push((Btn::DiffLayout, put(buf, &mut x, y, right, layout, theme.title)));
    x += 1;
    out.push((Btn::DiffCollapse, put(buf, &mut x, y, right, collapse, theme.title)));
    out
}

/// One line of text from display column `skip` into `width` cells at
/// (x, y): tabs expanded, control characters as `?`, syntax colors and the
/// changed characters (`word_bg`) on top of `base`.
#[allow(clippy::too_many_arguments)]
fn draw_line(
    buf: &mut Screen,
    (x, y, width): (u16, u16, u16),
    line: &str,
    skip: usize,
    tab_width: usize,
    base: Style,
    syntax: &[(Range<usize>, Style)],
    inline: &[Range<usize>],
    word_bg: Color,
) {
    let tw = tab_width.max(1);
    let right = x + width;
    let (mut col, mut cx) = (0usize, x);
    let (mut si, mut ii) = (0usize, 0usize);
    for (ci, ch) in strip_eol(line).chars().enumerate() {
        while si < syntax.len() && syntax[si].0.end <= ci {
            si += 1;
        }
        while ii < inline.len() && inline[ii].end <= ci {
            ii += 1;
        }
        let mut style = base;
        if let Some((r, s)) = syntax.get(si)
            && r.contains(&ci)
        {
            style = style.patch(*s);
            style.bg = base.bg;
        }
        if inline.get(ii).is_some_and(|r| r.contains(&ci)) {
            style.bg = Some(word_bg);
        }
        let (glyph, w) = match ch {
            '\t' => (' ', tw - col % tw),
            c if c.is_control() => ('?', 1),
            c => (c, c.width().unwrap_or(0)),
        };
        if w == 0 {
            continue;
        }
        // Tabs draw as `w` spaces, one cell at a time; a wide char is one
        // glyph over `w` cells.
        let (cells, step) = if ch == '\t' { (w, 1) } else { (1, w) };
        for _ in 0..cells {
            if col < skip {
                // A wide char cut by the scroll edge leaves a blank.
                if col + step > skip {
                    cx += (col + step - skip) as u16;
                }
                col += step;
                continue;
            }
            if cx + step as u16 > right {
                return;
            }
            let mut tmp = [0u8; 4];
            buf[(cx, y)].set_symbol(glyph.encode_utf8(&mut tmp)).set_style(style);
            for i in 1..step as u16 {
                buf[(cx + i, y)].reset();
                buf[(cx + i, y)].set_style(style);
            }
            cx += step as u16;
            col += step;
        }
    }
}

/// Render a diff pane: optional header row, then the view rows,
/// side-by-side or unified (by width unless `opts` forces one).
pub(super) fn render_diff(
    buf: &mut Screen,
    area: Rect,
    pane: &mut DiffPane,
    header: Option<&str>,
    focused: bool,
    theme: &Theme,
    opts: DiffOpts,
) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let mut r = area;
    if let Some(h) = header {
        let style = if focused { theme.title_focused } else { theme.title };
        buf.set_stringn(r.x + 1, r.y, crate::util::text::fit(h, r.width.saturating_sub(1) as usize), r.width as usize, style);
        r = Rect::new(r.x, r.y + 1, r.width, r.height - 1);
    }
    pane.page = r.height as usize;
    pane.rows_rect = r;
    let msg = |buf: &mut Screen, text: &str, style: Style| buf.set_stringn(r.x + 1, r.y, text, r.width.saturating_sub(1) as usize, style);
    if r.height == 0 {
        return;
    }
    if pane.key.is_none() {
        msg(buf, "Select a file to see its changes", theme.dim);
        return;
    }
    if let Some(e) = &pane.error {
        msg(buf, e, theme.error);
        return;
    }
    let Some(doc) = &pane.doc else {
        if pane.loading {
            msg(buf, "Loading…", theme.dim);
        }
        return;
    };
    if doc.binary {
        msg(buf, "Binary file; no text diff", theme.dim);
        return;
    }
    if doc.rows.is_empty() {
        msg(buf, "Empty file", theme.dim);
        return;
    }
    pane.set_unified(opts.unified.unwrap_or(r.width < 100));
    pane.set_collapse(opts.collapse);
    pane.ensure_syntax(theme);
    let Some(doc) = &pane.doc else { return };
    let unified = pane.unified;
    let rows: &[DiffRow] = if unified { &doc.unified } else { &doc.rows };
    pane.scroll = pane.scroll.min(pane.view.len().saturating_sub(r.height as usize));
    let digits = doc.old.len().max(doc.new.len()).max(1).to_string().len().max(3) as u16;
    // Line numbers of the current block stand out.
    let current = doc.blocks(unified).into_iter().nth(pane.block).unwrap_or_default();
    let no_spans = (Vec::new(), Vec::new());
    let (syn_old, syn_new) = pane.syntax.as_ref().unwrap_or(&no_spans);
    let spans = line_spans;
    let add = Style::default().fg(theme.fg).bg(theme.diff_add_bg);
    let del = Style::default().fg(theme.fg).bg(theme.diff_del_bg);
    let modi = Style::default().fg(theme.fg).bg(theme.diff_mod_bg);
    let plain = Style::default().fg(theme.fg).bg(theme.bg);
    let filler = Style::default().bg(theme.panel_bg);
    for (vi, vrow) in pane.view.iter().skip(pane.scroll).take(r.height as usize).enumerate() {
        let y = r.y + vi as u16;
        let ri = match *vrow {
            ViewRow::Row(i) => i,
            ViewRow::Fold { start, end } => {
                buf.set_style(Rect::new(r.x, y, r.width, 1), filler);
                let n = end - start;
                let label = format!("⋯ {n} unchanged line{} (click to show)", if n == 1 { "" } else { "s" });
                buf.set_stringn(r.x + digits + 1, y, label, r.width.saturating_sub(digits + 1) as usize, theme.dim.bg(theme.panel_bg));
                continue;
            }
        };
        let row = rows[ri];
        let num_style = if current.contains(&ri) { theme.warn } else { theme.line_number };
        let num = |l: usize| format!("{:>w$}", l + 1, w = digits as usize);
        if unified {
            // old# new# sign text
            let (style, sign) = match row.kind {
                RowKind::Removed => (del, "-"),
                RowKind::Added => (add, "+"),
                _ => (plain, " "),
            };
            let nums_w = digits * 2 + 2;
            buf.set_style(Rect::new(r.x + nums_w, y, r.width.saturating_sub(nums_w), 1), style);
            if let Some(l) = row.left {
                buf.set_stringn(r.x, y, num(l), digits as usize, num_style);
            }
            if let Some(l) = row.right {
                buf.set_stringn(r.x + digits + 1, y, num(l), digits as usize, num_style);
            }
            buf.set_stringn(r.x + nums_w, y, sign, 1, style);
            let tx = r.x + nums_w + 1;
            let cells = (tx, y, r.right().saturating_sub(tx));
            match (row.right, row.left) {
                (Some(l), _) => draw_line(
                    buf,
                    cells,
                    &doc.new[l],
                    pane.hscroll,
                    doc.tab_width,
                    style,
                    spans(syn_new, l),
                    &doc.inline_new[l],
                    theme.diff_add_word_bg,
                ),
                (None, Some(l)) => draw_line(
                    buf,
                    cells,
                    &doc.old[l],
                    pane.hscroll,
                    doc.tab_width,
                    style,
                    spans(syn_old, l),
                    &doc.inline_old[l],
                    theme.diff_del_word_bg,
                ),
                (None, None) => {}
            }
        } else {
            let half = (r.width - 1) / 2;
            let sides = [
                (r.x, row.left, &doc.old, syn_old, &doc.inline_old, theme.diff_del_word_bg),
                (r.x + half + 1, row.right, &doc.new, syn_new, &doc.inline_new, theme.diff_add_word_bg),
            ];
            for (side, (sx, line, lines, syn, inline, word_bg)) in sides.into_iter().enumerate() {
                let w = if side == 0 { half } else { r.right() - sx };
                let style = match (row.kind, line) {
                    (_, None) => filler,
                    (RowKind::Equal, _) => plain,
                    (RowKind::Modified, _) => modi,
                    (RowKind::Removed, _) => del,
                    (RowKind::Added, _) => add,
                };
                let text_x = sx + digits + 1;
                buf.set_style(Rect::new(text_x, y, w.saturating_sub(digits + 1), 1), style);
                if let Some(l) = line {
                    buf.set_stringn(sx, y, num(l), digits as usize, num_style);
                    let cells = (text_x, y, (sx + w).saturating_sub(text_x));
                    draw_line(buf, cells, &lines[l], pane.hscroll, doc.tab_width, style, spans(syn, l), &inline[l], word_bg);
                }
            }
            buf[(r.x + half, y)].set_symbol("│").set_style(theme.splitter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(old: &str, new: &str, collapse: bool) -> DiffPane {
        let mut p = DiffPane::new(collapse);
        let key = DiffKey::Commit { sha: "x".into(), rel: "f.txt".into(), old: None };
        p.key = Some(key.clone());
        p.set(&key, Ok(DiffDoc::build(old.as_bytes(), new.as_bytes(), 4)));
        p
    }

    #[test]
    fn unchanged_runs_fold_and_unfold() {
        let old: String = (0..30).map(|i| format!("{i}\n")).collect();
        let new = old.replace("15\n", "fifteen\n");
        let mut p = pane(&old, &new, true);
        // 0..12 folded, 12..15 context, the change, 16..19 context, 19..30 folded.
        assert_eq!(p.view.len(), 1 + 3 + 1 + 3 + 1);
        assert_eq!(p.view[0], ViewRow::Fold { start: 0, end: 12 });
        assert_eq!(p.view[4], ViewRow::Row(15));
        assert_eq!(p.view[8], ViewRow::Fold { start: 19, end: 30 });
        assert_eq!(p.scroll, 2, "opens two rows above the first change");
        // A click on the fold shows its lines.
        p.rows_rect = Rect::new(0, 0, 80, 20);
        p.scroll = 0;
        p.click(0);
        assert_eq!(p.view.len(), 12 + 3 + 1 + 3 + 1);
        p.set_collapse(false);
        assert_eq!(p.view.len(), 30);
        // Switching modes keeps the line at the top of the view.
        p.scroll = 15;
        p.set_unified(true);
        assert_eq!(p.row_at(p.scroll), Some(15));
        assert!(matches!(p.view[p.scroll], ViewRow::Row(15)));
    }

    #[test]
    fn short_runs_and_identical_files_stay_open() {
        let p = pane("a\nb\nc\n", "a\nB\nc\n", true);
        assert_eq!(p.view.len(), 3);
        let p = pane("a\nb\nc\nd\ne\nf\n", "a\nb\nc\nd\ne\nf\n", true);
        assert_eq!(p.view.len(), 6, "nothing to fold around");
    }

    #[test]
    fn lines_draw_with_tabs_syntax_and_changed_characters() {
        let mut buf = Screen::empty(Rect::new(0, 0, 12, 1));
        let base = Style::default().bg(Color::Black);
        let red = Style::default().fg(Color::Red);
        draw_line(&mut buf, (0, 0, 12), "\tab\n", 0, 4, base, &[(1..2, red)], &[std::ops::Range { start: 2, end: 3 }], Color::Green);
        let cells: String = (0..7).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(cells, "    ab ");
        assert_eq!(buf[(4, 0)].fg, Color::Red);
        assert_eq!(buf[(4, 0)].bg, Color::Black);
        assert_eq!(buf[(5, 0)].bg, Color::Green);
        // Scrolled past the tab.
        let mut buf = Screen::empty(Rect::new(0, 0, 4, 1));
        draw_line(&mut buf, (0, 0, 4), "\tab", 3, 4, base, &[], &[], Color::Green);
        let cells: String = (0..3).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(cells, " ab");
    }
}
