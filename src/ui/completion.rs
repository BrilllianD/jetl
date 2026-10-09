//! The code completion popup: the filtered list of a language server's
//! completions, drawn under the caret line (above it when there is no
//! room), its rows lined up with the word being completed. Each row has a
//! kind glyph in its syntax color, the label (struck through when
//! deprecated; the chars the typed word matched in the accent color, bold,
//! and underlined on the selected row), the item's `labelDetails.detail`
//! dimmed right after it, and the detail, dimmed and cut to fit.
//!
//! The list and its logic are `lsp::completion::CompletionState`; the app
//! routes keys (`App::completion_key`): Up/Down (wrapping), PgUp/PgDn,
//! Enter/Tab accept, Esc closes, other keys go on to the editor. A
//! commit character (the item's, else the server's `allCommitCharacters`)
//! accepts the item and then goes on to the editor, once the popup was
//! asked for with Ctrl+Space or the selection was moved.
//!
//! Documentation panel: when the selected item has a `detail` (drawn as
//! code) or `documentation` (markdown), a bordered panel shows it, resolved
//! lazily by the app 150 ms after the selection settles. It sits right of
//! the list when 30 or more columns are free there, else left of it under
//! the same rule, else stacked on the side of the list away from the caret
//! line (skipped under 3 rows); at most 16 rows. Alt+Up and Alt+Down and
//! the mouse wheel over it scroll it.

use std::time::Instant;

use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemTag, Documentation, MarkupKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::editor::groups::GroupId;
use crate::lsp::completion::{CompletionState, kind_glyph};
use crate::ui::hover::{DocLine, Highlight, Kind, code_lines, draw_lines, join_parts, markdown, wrap_lines};
use crate::ui::widgets::highlight;

/// Rows shown at most; longer lists scroll.
pub const MAX_ROWS: usize = 10;
/// Widest the popup gets (with its border), in cells.
const MAX_WIDTH: u16 = 72;
/// Narrowest, when the screen allows.
const MIN_WIDTH: u16 = 24;
/// The documentation panel: widest (with its border), narrowest free
/// width to put it beside the list, and tallest.
const DOCS_MAX_WIDTH: u16 = 60;
const DOCS_MIN_BESIDE: u16 = 30;
const DOCS_MAX_HEIGHT: u16 = 16;

/// The documentation panel's content: for item `item` (index into `state.items`).
#[derive(Debug)]
pub struct Docs {
    pub item: usize,
    pub lines: Vec<DocLine>,
    pub scroll: usize,
}

/// A fuzzy matcher that `Debug` can print.
pub struct LabelMatcher(nucleo_matcher::Matcher);

impl std::fmt::Debug for LabelMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Matcher")
    }
}

#[derive(Debug)]
pub struct CompletionPopup {
    pub state: CompletionState,
    /// The editor group it was opened in; it closes when focus leaves it.
    pub group: GroupId,
    /// First row shown.
    scroll: usize,
    /// The popup and its list rows as last drawn (empty when not drawn).
    rect: Rect,
    rows: Rect,
    /// The documentation panel shown, if any.
    pub docs: Option<Docs>,
    /// The item the panel is for or scheduled for.
    pub docs_want: Option<usize>,
    pub docs_due: Option<Instant>,
    /// Items already resolved (merged into `state.items`).
    pub resolved: Vec<usize>,
    /// Commit characters apply: opened by Ctrl+Space, or the user moved the selection.
    pub armed: bool,
    matcher: LabelMatcher,
    /// The panel as last drawn (default when not drawn).
    docs_rect: Rect,
}

impl CompletionPopup {
    pub fn new(state: CompletionState, group: GroupId) -> Self {
        Self {
            state,
            group,
            scroll: 0,
            rect: Rect::default(),
            rows: Rect::default(),
            docs: None,
            docs_want: None,
            docs_due: None,
            resolved: Vec::new(),
            armed: false,
            matcher: LabelMatcher(nucleo_matcher::Matcher::default()),
            docs_rect: Rect::default(),
        }
    }

    /// Rows that fit, as last drawn (a page for PgUp/PgDn).
    pub fn page(&self) -> usize {
        (self.rows.height as usize).max(1)
    }

    /// Whether a screen cell is inside the popup as last drawn.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains(ratatui::layout::Position::new(x, y))
    }

    /// The list row (index into `state.order`) at a screen cell.
    pub fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        let in_rows = self.rows.contains(ratatui::layout::Position::new(x, y));
        let i = self.scroll + (y.saturating_sub(self.rows.y)) as usize;
        (in_rows && i < self.state.order.len()).then_some(i)
    }

    /// Wheel scrolling moves the selection.
    pub fn scroll_by(&mut self, delta: isize) {
        self.state.step(delta, false);
        self.armed = true;
    }

    /// Whether a screen cell is inside the documentation panel as last drawn.
    pub fn docs_contains(&self, x: u16, y: u16) -> bool {
        self.docs_rect.contains(ratatui::layout::Position::new(x, y))
    }

    /// Whether the documentation panel was drawn.
    pub fn docs_shown(&self) -> bool {
        self.docs_rect != Rect::default()
    }

    /// Scroll the documentation panel (clamped when drawn).
    pub fn scroll_docs(&mut self, delta: isize) {
        if let Some(d) = &mut self.docs {
            d.scroll = d.scroll.saturating_add_signed(delta);
        }
    }

    /// Draw for the caret cell `anchor`, inside `area` (the screen above the
    /// status bar); `word` is the cells of the typed prefix left of the
    /// caret, so labels start under the word. Nothing when the list is
    /// empty.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, anchor: (u16, u16), word: u16, theme: &Theme) {
        self.rect = Rect::default();
        self.rows = Rect::default();
        self.docs_rect = Rect::default();
        let n = self.state.order.len();
        let (ax, ay) = anchor;
        if n == 0 || ay < area.y || ay >= area.bottom() || area.width < 12 {
            return;
        }
        let items: Vec<&CompletionItem> = self.state.order.iter().map(|&i| &self.state.items[i]).collect();
        let label_w = items.iter().map(|it| it.label.width() + label_detail(it).width()).max().unwrap_or(0) as u16;
        let detail_w = items.iter().map(|it| detail(it).width()).max().unwrap_or(0) as u16;
        // Border, glyph and a space, label, two spaces, detail, a space, border.
        let natural = 1 + 2 + label_w + if detail_w > 0 { 2 + detail_w } else { 0 } + 1 + 1;
        let w = natural.clamp(MIN_WIDTH.min(area.width), MAX_WIDTH.min(area.width));
        let want = (n.min(MAX_ROWS) as u16) + 2;
        let below = area.bottom().saturating_sub(ay + 1);
        let above = ay - area.y;
        let (y, h) = if below >= want || below >= above { (ay + 1, want.min(below)) } else { (ay - want.min(above), want.min(above)) };
        if h < 3 {
            return;
        }
        // The label column under the word's first cell.
        let x = ax.saturating_sub(word + 3).min(area.right() - w).max(area.x);
        let rect = Rect::new(x, y, w, h);
        self.rect = rect;
        let view = (h - 2) as usize;
        self.rows = Rect::new(x + 1, y + 1, w - 2, h - 2);
        let sel = self.state.selected.min(n - 1);
        if sel < self.scroll {
            self.scroll = sel;
        } else if sel >= self.scroll + view {
            self.scroll = sel + 1 - view;
        }
        self.scroll = self.scroll.min(n.saturating_sub(view));
        frame.render_widget(Clear, rect);
        let bg = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let mut block = Block::default().borders(Borders::ALL).border_style(theme.popup_border).style(bg);
        if n > view {
            block = block.title_bottom(Line::from(format!(" {}/{n} ", sel + 1)).right_aligned());
        }
        frame.render_widget(block, rect);
        let inner = self.rows;
        let marks: Vec<Vec<u32>> = (self.scroll..(self.scroll + view).min(n))
            .map(|row| self.state.match_indices(self.state.order[row], &mut self.matcher.0))
            .collect();
        let buf = frame.buffer_mut();
        for (row, item) in items.iter().enumerate().skip(self.scroll).take(view) {
            let ry = inner.y + (row - self.scroll) as u16;
            let base = if row == sel { theme.list_selected } else { bg };
            buf.set_style(Rect::new(inner.x, ry, inner.width, 1), base);
            buf.set_stringn(inner.x, ry, kind_glyph(item.kind), 1, base.patch(kind_style(item.kind, theme)));
            let room = inner.width.saturating_sub(3) as usize;
            let mut label_style = base;
            if deprecated(item) {
                label_style = label_style.add_modifier(Modifier::CROSSED_OUT);
            }
            let hl = if row == sel {
                label_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                label_style.fg(theme.status_accent.fg.unwrap_or(theme.fg)).add_modifier(Modifier::BOLD)
            };
            let mut lx = inner.x + 2;
            let end = lx + room as u16;
            for (run, st) in highlight(&item.label, |ci| marks[row - self.scroll].contains(&(ci as u32)), label_style, hl) {
                lx = buf.set_stringn(lx, ry, run, (end - lx) as usize, st).0;
            }
            let ld = label_detail(item);
            if !ld.is_empty() && lx < end {
                lx = buf.set_stringn(lx, ry, ld, (end - lx) as usize, base.patch(theme.dim)).0;
            }
            let d = detail(item);
            let used = lx.saturating_sub(inner.x);
            let left = inner.width.saturating_sub(used + 3);
            if !d.is_empty() && left >= 4 {
                let dw = (d.width() as u16).min(left);
                let dx = inner.right() - 1 - dw;
                let text = if d.width() as u16 > left { cut(&d, left as usize) } else { d };
                buf.set_stringn(dx, ry, text, dw as usize, base.patch(theme.dim));
            }
        }
        self.render_docs(frame, area, ay, theme);
    }

    /// The documentation panel of the selected item, beside the list, else
    /// stacked on the side away from the caret line `ay`.
    fn render_docs(&mut self, frame: &mut Frame, area: Rect, ay: u16, theme: &Theme) {
        let sel = self.state.order.get(self.state.selected).copied();
        let Some(d) = self.docs.as_mut().filter(|d| Some(d.item) == sel && !d.lines.is_empty()) else { return };
        let rect = self.rect;
        let natural = d.lines.iter().map(|l| l.text.width()).max().unwrap_or(0) as u16;
        let iw = natural.clamp(20, DOCS_MAX_WIDTH - 4);
        let below_caret = rect.y > ay;
        let right = area.right().saturating_sub(rect.right());
        let left = rect.x.saturating_sub(area.x);
        let beside = |x: u16, w: u16, rows: usize| {
            let want = (rows as u16 + 2).min(DOCS_MAX_HEIGHT);
            if below_caret {
                (x, rect.y, w, want.min(area.bottom().saturating_sub(rect.y)))
            } else {
                let h = want.min(rect.bottom().saturating_sub(area.y));
                (x, rect.bottom() - h, w, h)
            }
        };
        let (x, y, w, h, rows) = if right >= DOCS_MIN_BESIDE {
            let w = (iw + 4).min(right);
            let rows = wrap_lines(&d.lines, (w - 4) as usize);
            let (x, y, w, h) = beside(rect.right(), w, rows.len());
            (x, y, w, h, rows)
        } else if left >= DOCS_MIN_BESIDE {
            let w = (iw + 4).min(left);
            let rows = wrap_lines(&d.lines, (w - 4) as usize);
            let (x, y, w, h) = beside(rect.x - w, w, rows.len());
            (x, y, w, h, rows)
        } else {
            let w = rect.width.max((iw + 4).min(area.width));
            let x = rect.x.min(area.right() - w);
            let rows = wrap_lines(&d.lines, w.saturating_sub(4) as usize);
            let want = (rows.len() as u16 + 2).min(DOCS_MAX_HEIGHT);
            if below_caret {
                (x, rect.bottom(), w, want.min(area.bottom().saturating_sub(rect.bottom())), rows)
            } else {
                let h = want.min(rect.y.saturating_sub(area.y));
                (x, rect.y - h, w, h, rows)
            }
        };
        if h < 3 {
            return;
        }
        let r = Rect::new(x, y, w, h);
        let view = (h - 2) as usize;
        d.scroll = d.scroll.min(rows.len().saturating_sub(view));
        frame.render_widget(Clear, r);
        let bg = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let mut block = Block::default().borders(Borders::ALL).border_style(theme.popup_border).style(bg);
        if rows.len() > view {
            let last = (d.scroll + view).min(rows.len());
            block = block.title_bottom(Line::from(format!(" {last}/{} ", rows.len())).right_aligned());
        }
        let inner = block.inner(r);
        frame.render_widget(block, r);
        draw_lines(frame.buffer_mut(), inner, &rows, d.scroll, theme);
        self.docs_rect = r;
    }
}

/// The lines of the panel for `item`: its `detail` as code, then its
/// `documentation`; empty when it has neither.
pub fn docs_lines(item: &CompletionItem, hl: Highlight<'_>) -> Vec<DocLine> {
    let text =
        |s: &str| -> Vec<DocLine> { s.lines().map(|l| DocLine { kind: Kind::Text, text: l.to_string(), spans: Vec::new() }).collect() };
    let mut parts = Vec::new();
    if let Some(d) = item.detail.as_deref() {
        parts.push(code_lines(d, hl.fallback, hl.theme));
    }
    match &item.documentation {
        Some(Documentation::String(s)) => parts.push(text(s)),
        Some(Documentation::MarkupContent(m)) if m.kind == MarkupKind::Markdown => parts.push(markdown(&m.value, hl)),
        Some(Documentation::MarkupContent(m)) => parts.push(text(&m.value)),
        None => {}
    }
    join_parts(parts)
}

/// `labelDetails.detail`, drawn dimmed right after the label.
fn label_detail(item: &CompletionItem) -> &str {
    item.label_details.as_ref().and_then(|l| l.detail.as_deref()).unwrap_or("")
}

/// The detail shown right of the label: its first line.
fn detail(item: &CompletionItem) -> String {
    let d = item.label_details.as_ref().and_then(|l| l.description.as_deref()).or(item.detail.as_deref()).unwrap_or("");
    d.lines().next().unwrap_or("").trim().to_string()
}

fn deprecated(item: &CompletionItem) -> bool {
    #[allow(deprecated)]
    let old = item.deprecated == Some(true);
    old || item.tags.as_ref().is_some_and(|t| t.contains(&CompletionItemTag::DEPRECATED))
}

/// `s` cut to `width` cells, ending in `…`.
fn cut(s: &str, width: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 1 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// The kind glyph's color: the syntax color of what the kind names.
fn kind_style(kind: Option<CompletionItemKind>, theme: &Theme) -> Style {
    let capture = match kind {
        Some(CompletionItemKind::METHOD | CompletionItemKind::FUNCTION | CompletionItemKind::CONSTRUCTOR) => "function",
        Some(CompletionItemKind::FIELD | CompletionItemKind::PROPERTY) => "property",
        Some(CompletionItemKind::CONSTANT | CompletionItemKind::ENUM_MEMBER | CompletionItemKind::VALUE) => "constant",
        Some(CompletionItemKind::KEYWORD | CompletionItemKind::MODULE | CompletionItemKind::SNIPPET) => "keyword",
        _ => "",
    };
    theme.syntax.get(capture).copied().unwrap_or(theme.dim)
}
