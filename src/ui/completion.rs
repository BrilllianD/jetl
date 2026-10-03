//! The code completion popup: the filtered list of a language server's
//! completions, drawn under the caret line (above it when there is no
//! room), its rows lined up with the word being completed. Each row has a
//! kind glyph in its syntax color, the label (struck through when
//! deprecated) and the detail, dimmed and cut to fit.
//!
//! The list and its logic are `lsp::completion::CompletionState`; the app
//! routes keys (`App::completion_key`): Up/Down (wrapping), PgUp/PgDn,
//! Enter/Tab accept, Esc closes, other keys go on to the editor.

use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemTag};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::lsp::completion::{CompletionState, kind_glyph};

/// Rows shown at most; longer lists scroll.
pub const MAX_ROWS: usize = 10;
/// Widest the popup gets (with its border), in cells.
const MAX_WIDTH: u16 = 72;
/// Narrowest, when the screen allows.
const MIN_WIDTH: u16 = 24;

#[derive(Debug)]
pub struct CompletionPopup {
    pub state: CompletionState,
    /// First row shown.
    scroll: usize,
    /// The popup and its list rows as last drawn (empty when not drawn).
    rect: Rect,
    rows: Rect,
}

impl CompletionPopup {
    pub fn new(state: CompletionState) -> Self {
        Self { state, scroll: 0, rect: Rect::default(), rows: Rect::default() }
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
    }

    /// Draw for the caret cell `anchor`, inside `area` (the screen above the
    /// status bar); `word` is the cells of the typed prefix left of the
    /// caret, so labels start under the word. Nothing when the list is
    /// empty.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, anchor: (u16, u16), word: u16, theme: &Theme) {
        self.rect = Rect::default();
        self.rows = Rect::default();
        let n = self.state.order.len();
        let (ax, ay) = anchor;
        if n == 0 || ay < area.y || ay >= area.bottom() || area.width < 12 {
            return;
        }
        let items: Vec<&CompletionItem> = self.state.order.iter().map(|&i| &self.state.items[i]).collect();
        let label_w = items.iter().map(|it| it.label.width()).max().unwrap_or(0) as u16;
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
            let (lx, _) = buf.set_stringn(inner.x + 2, ry, &item.label, room, label_style);
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
    }
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
