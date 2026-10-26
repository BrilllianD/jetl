//! Sticky scroll (`editor.sticky_scroll`): the header line of each definition
//! the view is scrolled into stays pinned over the top text rows.
//!
//! Which headers (constraint of the feature): the headers of the scopes from
//! `symbols::enclosing` at the line of the first row the pins do not cover,
//! outermost first, only those above the viewport top, at most 5 and a third
//! of the text height, and never down to the caret's row (see [`pins`]).
//!
//! Layout: the pins are painted over the top text rows after `view::render`;
//! `text_area` is unchanged, so `layout_line()` and every mouse mapping stay
//! as they were. `TabGroup::sticky` keeps the drawn rects, which take the
//! click (`Editor::mouse`) before the text does.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::buffer::{Buffer, BufferId};
use super::rows::{Row, Rows};
use super::syntax::Highlights;
use super::view::{self, CellKind};
use crate::config::theme::Theme;
use crate::symbols::{Symbol, enclosing};

const MAX_PINS: usize = 5;

/// One pinned header: its line and where its definition's name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) line: usize,
    pub(crate) name_line: usize,
    pub(crate) name_col: usize,
}

/// A group's sticky scroll state.
#[derive(Debug, Default)]
pub(crate) struct Sticky {
    /// The pinned rows as last drawn (whole pane width) and their scope.
    pub(crate) rows: Vec<(Rect, Pin)>,
    /// Syntax colours of pinned lines, valid for one buffer id and version.
    hl_key: Option<(BufferId, u64)>,
    hl: Vec<(usize, Highlights)>,
}

impl Sticky {
    /// The pin drawn at screen cell `(x, y)`.
    pub(crate) fn hit(&self, x: u16, y: u16) -> Option<Pin> {
        let at = ratatui::layout::Position::new(x, y);
        self.rows.iter().find(|(r, _)| r.contains(at)).map(|(_, p)| *p)
    }

    /// Drop the cached colours (the theme changed).
    pub(crate) fn forget_styles(&mut self) {
        self.hl_key = None;
        self.hl.clear();
    }
}

/// The caret's row index below `top` when it is on screen (`0..height`).
pub(super) fn caret_row(b: &Buffer, top: Row, height: u16, tab_width: usize) -> Option<usize> {
    let r = b.rows();
    let sel = b.view.sel();
    let caret = r.clamp(view::char_to_row_x(&b.rope, &r, sel.head, sel.upstream, tab_width).0);
    let top = r.clamp(top);
    if caret < top {
        return None;
    }
    let d = r.row_distance(top, caret, height as usize);
    (d < height as usize).then_some(d)
}

/// The headers to pin over a view whose first row is `top`, outermost first.
/// For `n` pins the row `n` below `top` is the first uncovered one: its
/// enclosing scopes, filtered to headers above it and not folded away, must
/// number at least `n` and start above the viewport top.
pub(super) fn pins(syms: &[Symbol], rows: &Rows, top: Row, caret: Option<usize>, height: u16) -> Vec<Pin> {
    let limit = MAX_PINS.min(height as usize / 3).min(caret.unwrap_or(usize::MAX));
    let mut out = Vec::new();
    for n in 1..=limit {
        let Some(r) = rows.at_offset(top, n) else { break };
        let chain: Vec<&Symbol> =
            enclosing(syms, r.line).into_iter().filter(|s| Row::line(s.start_line) < r && !rows.is_hidden(s.start_line)).collect();
        if chain.len() < n || Row::line(chain[0].start_line) >= top {
            break;
        }
        out = chain[..n].iter().map(|s| Pin { line: s.start_line, name_line: s.line, name_col: s.col }).collect();
    }
    out
}

/// Paint the pins over the top rows of `text_area` (inside `pane`) and record
/// their rects in `st`.
pub(super) fn draw(
    screen: &mut ratatui::buffer::Buffer,
    b: &mut Buffer,
    pane: Rect,
    text_area: Rect,
    tab_width: usize,
    theme: &Theme,
    st: &mut Sticky,
) {
    st.rows.clear();
    if text_area.height < 3 || text_area.width == 0 {
        return;
    }
    let top = b.view.top();
    let caret = caret_row(b, top, text_area.height, tab_width);
    let syms = b.scopes.symbols(b.syntax.as_mut(), &b.rope, b.version);
    let rows = Rows::view(&b.view.folds, &b.view.wrap, &b.view.fillers, &b.rope, b.version).with_inlays(&b.inlays);
    let pins = pins(syms, &rows, top, caret, text_area.height);
    if pins.is_empty() {
        return;
    }
    // Colours of the pinned lines, kept apart from the view's one-window
    // highlight cache so that is not evicted every frame.
    if st.hl_key != Some((b.id, b.version)) {
        st.hl_key = Some((b.id, b.version));
        st.hl.clear();
    }
    for p in &pins {
        if !st.hl.iter().any(|(l, _)| *l == p.line) {
            let h = b.syntax.as_mut().map(|s| s.highlights(&b.rope, p.line, 1)).unwrap_or_default();
            st.hl.push((p.line, h));
        }
    }
    let rows = b.rows();
    let scroll_col = if rows.wraps() { 0 } else { b.view.scroll_col };
    let right = text_area.right() as usize;
    let sticky_bg = theme.sticky_scroll.bg.unwrap_or(theme.bg);
    let base = Style::default().bg(theme.bg).patch(theme.sticky_scroll);
    let num_style = theme.line_number.fg.map(|c| Style::default().fg(c)).unwrap_or_default();
    let mut drawn = Vec::new();
    for (k, p) in pins.iter().enumerate() {
        let y = text_area.y + k as u16;
        let rect = Rect::new(pane.x, y, pane.width, 1);
        for x in rect.left()..rect.right() {
            screen[(x, y)].reset();
        }
        screen.set_style(rect, base);
        let num = (p.line + 1).to_string();
        if (text_area.x - pane.x) as usize >= num.len() + 4 {
            screen.set_string(text_area.x - 4 - num.len() as u16, y, &num, num_style);
        }
        let hl = st.hl.iter().find(|(l, _)| *l == p.line).map(|(_, h)| h);
        view::layout_row(&b.rope, &rows, Row::line(p.line), tab_width, scroll_col + text_area.width as usize, &mut |g, c, rx| {
            if rx + c.width <= scroll_col {
                return true;
            }
            let x = text_area.x as usize + rx.saturating_sub(scroll_col);
            if x >= right {
                return false;
            }
            if c.kind == CellKind::InlayLabel {
                screen.set_stringn(x as u16, y, g, right - x, theme.inlay_hint);
                return true;
            }
            if c.width == 0 || c.kind == CellKind::InlayPad || g.is_empty() {
                return true;
            }
            let style = hl.and_then(|h| h.style_at(c.char_idx)).unwrap_or(Style::default().fg(theme.fg)).bg(sticky_bg);
            if crate::util::text::shown_escaped(b.rope.char(c.char_idx)) {
                screen.set_stringn(x as u16, y, g, right - x, style.add_modifier(Modifier::DIM));
            } else {
                if x + c.width > right {
                    return false;
                }
                screen.set_stringn(x as u16, y, g, c.width, style);
            }
            true
        });
        drawn.push((rect, *p));
    }
    st.rows = drawn;
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;
    use ropey::Rope;

    use super::*;
    use crate::editor::fold::{Fold, Folds};

    fn sym(name: &str, start: usize, end: usize, depth: usize) -> Symbol {
        Symbol { name: name.into(), kind: "function", line: start, col: depth * 4 + 3, start_line: start, end_line: end, depth }
    }

    fn scopes() -> Vec<Symbol> {
        vec![sym("I", 0, 30, 0), sym("f", 2, 10, 1), sym("g", 12, 20, 1)]
    }

    fn rope32() -> Rope {
        let text: String = (0..32).map(|i| if i == 10 { "}\n" } else { "x\n" }).collect();
        Rope::from_str(&text)
    }

    fn lines(p: &[Pin]) -> Vec<usize> {
        p.iter().map(|p| p.line).collect()
    }

    #[test]
    fn pins_nest_outermost_first() {
        let rope = rope32();
        let folds = Folds::default();
        let rows = Rows::new(&folds, rope.len_lines());
        let p = pins(&scopes(), &rows, Row::line(4), None, 12);
        assert_eq!(lines(&p), [0, 2]);
        assert_eq!((p[1].name_line, p[1].name_col), (2, 7));
    }

    #[test]
    fn pins_leave_out_a_scope_that_ends_under_them() {
        let rope = rope32();
        let folds = Folds::default();
        let rows = Rows::new(&folds, rope.len_lines());
        assert_eq!(lines(&pins(&scopes(), &rows, Row::line(9), None, 12)), [0]);
    }

    #[test]
    fn pins_need_a_header_above_the_top() {
        let rope = rope32();
        let folds = Folds::default();
        let rows = Rows::new(&folds, rope.len_lines());
        assert!(pins(&scopes(), &rows, Row::line(0), None, 12).is_empty());
    }

    #[test]
    fn pins_respect_the_caps() {
        let rope = rope32();
        let folds = Folds::default();
        let rows = Rows::new(&folds, rope.len_lines());
        let s = scopes();
        assert_eq!(lines(&pins(&s, &rows, Row::line(4), None, 5)), [0]);
        assert_eq!(lines(&pins(&s, &rows, Row::line(4), Some(1), 12)), [0]);
        assert!(pins(&s, &rows, Row::line(4), Some(0), 12).is_empty());
        assert!(pins(&s, &rows, Row::line(4), None, 2).is_empty());

        let deep: Vec<Symbol> = (0..7).map(|k| sym(&format!("s{k}"), k, 100 - k, k)).collect();
        let rope = Rope::from_str(&"x\n".repeat(102));
        let rows = Rows::new(&folds, rope.len_lines());
        assert_eq!(lines(&pins(&deep, &rows, Row::line(10), None, 30)), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn pins_walk_folded_rows() {
        let rope = rope32();
        let mut folds = Folds::default();
        folds.collapse(Fold { start: rope.line_to_char(2), end: rope.line_to_char(10) + 1 }, &rope);
        let rows = Rows::new(&folds, rope.len_lines());
        assert_eq!(rows.next(2), Some(11));
        assert_eq!(lines(&pins(&scopes(), &rows, Row::line(1), None, 12)), [0]);
    }

    #[test]
    fn sticky_hit_finds_the_row() {
        let a = Pin { line: 0, name_line: 0, name_col: 0 };
        let b = Pin { line: 2, name_line: 2, name_col: 7 };
        let st = Sticky { rows: vec![(Rect::new(0, 1, 40, 1), a), (Rect::new(0, 2, 40, 1), b)], ..Default::default() };
        assert_eq!(st.hit(5, 2), Some(b));
        assert_eq!(st.hit(5, 3), None);
        assert_eq!(st.hit(40, 1), None);
    }
}
