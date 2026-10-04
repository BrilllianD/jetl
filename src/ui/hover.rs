//! The Quick Documentation popup (Alt+Q): a language server's hover text
//! at the caret, drawn next to the caret cell, under it when there is room
//! and above otherwise. Markdown is shown plainly: fences dropped, code
//! lines on the editor background, headings bold, emphasis, inline code
//! marks, escapes and link targets stripped; prose is wrapped to the popup.
//!
//! The app drops the popup once the buffer's version or caret differs from
//! the request's. While it is up, Up/Down/PgUp/PgDn scroll it and Esc closes
//! it; any other key closes it and goes on to the editor
//! (`App::hover_key`), the pattern the completion popup follows too.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lsp_types::{HoverContents, MarkedString, MarkupKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::theme::Theme;
use crate::editor::buffer::BufferId;
use crate::editor::groups::GroupId;

/// Widest the popup gets (with its border), in cells.
const MAX_WIDTH: u16 = 82;
/// Tallest the popup gets (with its border); longer text scrolls.
const MAX_HEIGHT: u16 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Code,
    Heading,
    Rule,
}

#[derive(Debug)]
pub struct HoverPopup {
    /// What the popup was asked for: dropped when any of these change.
    pub group: GroupId,
    pub buffer: BufferId,
    pub version: u64,
    pub head: usize,
    lines: Vec<(Kind, String)>,
    scroll: usize,
    /// Text rows that fit, as last drawn (a page for PgUp/PgDn).
    page: usize,
    rect: Rect,
}

impl HoverPopup {
    /// The popup for a hover answer; `None` when it has no text.
    pub fn new(group: GroupId, buffer: BufferId, version: u64, head: usize, contents: HoverContents) -> Option<Self> {
        let lines = contents_lines(contents);
        (!lines.is_empty()).then_some(Self { group, buffer, version, head, lines, scroll: 0, page: 1, rect: Rect::default() })
    }

    #[cfg(test)]
    pub fn lines(&self) -> &[(Kind, String)] {
        &self.lines
    }

    /// Scroll keys; `false` for any other key.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT) {
            return false;
        }
        let page = self.page.max(1);
        match key.code {
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll += 1,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::PageDown => self.scroll += page,
            _ => return false,
        }
        true
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }

    /// Whether a screen cell is inside the popup as last drawn.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains(ratatui::layout::Position::new(x, y))
    }

    /// Draw next to the caret cell `anchor`, inside `area` (the screen
    /// above the status bar).
    pub fn render(&mut self, frame: &mut Frame, area: Rect, anchor: (u16, u16), theme: &Theme) {
        self.rect = Rect::default();
        let (ax, ay) = anchor;
        let max_w = MAX_WIDTH.min(area.width);
        if max_w < 12 || ay < area.y || ay >= area.bottom() {
            return;
        }
        let natural = self.lines.iter().map(|(_, s)| s.width()).max().unwrap_or(0) as u16;
        // Text width: the border and a column of padding each side.
        let iw = natural.clamp(20.min(max_w - 4), max_w - 4);
        let rows = wrap_lines(&self.lines, iw as usize);
        let want = (rows.len() as u16 + 2).min(MAX_HEIGHT);
        let below = area.bottom().saturating_sub(ay + 1);
        let above = ay - area.y;
        let (y, h) = if below >= want || below >= above { (ay + 1, want.min(below)) } else { (ay - want.min(above), want.min(above)) };
        if h < 3 {
            return;
        }
        let w = iw + 4;
        let x = ax.min(area.right() - w).max(area.x);
        let rect = Rect::new(x, y, w, h);
        self.rect = rect;
        let view = (h - 2) as usize;
        self.page = view;
        self.scroll = self.scroll.min(rows.len().saturating_sub(view));
        frame.render_widget(Clear, rect);
        let bg = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let mut block = Block::default().borders(Borders::ALL).border_style(theme.popup_border).style(bg);
        if rows.len() > view {
            let last = (self.scroll + view).min(rows.len());
            block = block.title_bottom(Line::from(format!(" {last}/{} ", rows.len())).right_aligned());
        }
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let buf = frame.buffer_mut();
        for (i, (kind, text)) in rows.iter().skip(self.scroll).take(view).enumerate() {
            let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
            let style = match kind {
                Kind::Text => bg,
                Kind::Code => Style::default().bg(theme.bg).fg(theme.fg),
                Kind::Heading => bg.add_modifier(Modifier::BOLD),
                Kind::Rule => theme.dim.bg(theme.popup_bg),
            };
            buf.set_style(row, style);
            match kind {
                Kind::Rule => buf.set_stringn(row.x, row.y, "─".repeat(row.width as usize), row.width as usize, style),
                _ => buf.set_stringn(row.x + 1, row.y, text, iw as usize, style),
            };
        }
    }
}

/// Lines of a hover answer: markdown parsed, plain text as it is, code in
/// `MarkedString`s as code; several parts separated by a rule.
pub fn contents_lines(contents: HoverContents) -> Vec<(Kind, String)> {
    let marked = |m: MarkedString| match m {
        MarkedString::String(s) => markdown(&s),
        MarkedString::LanguageString(l) => l.value.lines().map(|l| (Kind::Code, l.to_string())).collect(),
    };
    let parts: Vec<Vec<(Kind, String)>> = match contents {
        HoverContents::Scalar(m) => vec![marked(m)],
        HoverContents::Array(ms) => ms.into_iter().map(marked).collect(),
        HoverContents::Markup(m) if m.kind == MarkupKind::Markdown => vec![markdown(&m.value)],
        HoverContents::Markup(m) => vec![m.value.lines().map(|l| (Kind::Text, l.to_string())).collect()],
    };
    let mut out: Vec<(Kind, String)> = Vec::new();
    for part in parts.into_iter().map(tidy).filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push((Kind::Rule, String::new()));
        }
        out.extend(part);
    }
    out
}

/// Without blank lines at the ends, next to rules or doubled.
fn tidy(lines: Vec<(Kind, String)>) -> Vec<(Kind, String)> {
    let blank = |l: &(Kind, String)| l.0 == Kind::Text && l.1.trim().is_empty();
    let mut out: Vec<(Kind, String)> = Vec::new();
    for l in lines {
        if blank(&l) && out.last().is_none_or(|p| blank(p) || p.0 == Kind::Rule) {
            continue;
        }
        if l.0 == Kind::Rule && out.last().is_some_and(blank) {
            out.pop();
        }
        out.push(l);
    }
    while out.last().is_some_and(blank) {
        out.pop();
    }
    out
}

/// Markdown as lines to show: fenced blocks become code lines (the fences
/// go), `#` headings and `---` rules are marked, link reference
/// definitions dropped, the rest cleaned by [`inline`].
pub fn markdown(text: &str) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            } else {
                out.push((Kind::Code, line.trim_end().to_string()));
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
            continue;
        }
        let t = t.trim_end();
        if t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_')) {
            out.push((Kind::Rule, String::new()));
        } else if let Some(h) = t.strip_prefix('#') {
            out.push((Kind::Heading, inline(h.trim_start_matches('#').trim())));
        } else if t.starts_with('[') && t.split_once("]: ").is_some_and(|(_, target)| !target.contains(' ')) {
            // `[label]: target`, a link reference definition.
            continue;
        } else {
            out.push((Kind::Text, inline(line.trim_end())));
        }
    }
    out
}

/// Inline markdown cleaned for plain display: code span marks, `**`
/// emphasis and backslash escapes go; `[text](target)` shows its text.
fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_code = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '`' {
            in_code = !in_code;
            i += 1;
            continue;
        }
        if in_code {
            out.push(c);
            i += 1;
            continue;
        }
        if c == '['
            && let Some((close, end)) = link_span(&chars, i)
        {
            out.push_str(&inline(&chars[i + 1..close].iter().collect::<String>()));
            i = end + 1;
            continue;
        }
        match c {
            '\\' if next.is_some_and(|n| n.is_ascii_punctuation()) => {
                out.push(next.unwrap_or_default());
                i += 2;
            }
            '*' if next == Some('*') => i += 2,
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `[text](url)` starting at `open`: indexes of `]` and `)`.
fn link_span(chars: &[char], open: usize) -> Option<(usize, usize)> {
    let close = chars[open + 1..].iter().position(|&c| c == ']')? + open + 1;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = chars[close + 2..].iter().position(|&c| c == ')')? + close + 2;
    Some((close, end))
}

/// Rows of `width` cells: prose wrapped at spaces, code cut by cells.
fn wrap_lines(lines: &[(Kind, String)], width: usize) -> Vec<(Kind, String)> {
    let mut rows = Vec::new();
    for (kind, text) in lines {
        match kind {
            Kind::Rule => rows.push((Kind::Rule, "─".repeat(width))),
            Kind::Code => {
                let mut cur = String::new();
                for c in text.replace('\t', "    ").chars() {
                    if cur.width() + c.width().unwrap_or(0) > width {
                        rows.push((Kind::Code, std::mem::take(&mut cur)));
                    }
                    cur.push(c);
                }
                rows.push((Kind::Code, cur));
            }
            _ if text.is_empty() => rows.push((*kind, String::new())),
            _ => rows.extend(crate::ui::toasts::wrap(text, width, usize::MAX).into_iter().map(|l| (*kind, l))),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{LanguageString, MarkupContent};

    fn texts(lines: &[(Kind, String)]) -> Vec<String> {
        lines.iter().map(|(k, s)| format!("{k:?}:{s}")).collect()
    }

    #[test]
    fn markdown_is_shown_plainly() {
        let md = "\n```rust\nfn len(&self) -> usize\n```\n\n---\n\n# Returns\n\nThe **length** of `self`, see [`Vec`](https://x/y) \\[1\\] a[0].\n\n\n[`Vec`]: https://x/y\nlast";
        let lines = contents_lines(HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value: md.into() }));
        assert_eq!(
            texts(&lines),
            [
                "Code:fn len(&self) -> usize",
                "Rule:",
                "Heading:Returns",
                "Text:",
                "Text:The length of self, see Vec [1] a[0].",
                "Text:",
                "Text:last",
            ]
        );
        // Plain text is not parsed; several parts get a rule between them.
        let plain = HoverContents::Markup(MarkupContent { kind: MarkupKind::PlainText, value: "a **b**\n".into() });
        assert_eq!(texts(&contents_lines(plain)), ["Text:a **b**"]);
        let parts = HoverContents::Array(vec![
            MarkedString::LanguageString(LanguageString { language: "rust".into(), value: "struct S".into() }),
            MarkedString::String("".into()),
            MarkedString::String("doc".into()),
        ]);
        assert_eq!(texts(&contents_lines(parts)), ["Code:struct S", "Rule:", "Text:doc"]);
        assert!(contents_lines(HoverContents::Array(vec![])).is_empty());
    }

    #[test]
    fn rows_wrap_prose_and_cut_code() {
        let lines = vec![
            (Kind::Text, "one two three".to_string()),
            (Kind::Text, String::new()),
            (Kind::Code, "abcdefgh".to_string()),
            (Kind::Rule, String::new()),
        ];
        assert_eq!(texts(&wrap_lines(&lines, 5)), ["Text:one", "Text:two", "Text:three", "Text:", "Code:abcde", "Code:fgh", "Rule:─────"]);
    }
}
