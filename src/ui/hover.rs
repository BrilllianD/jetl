//! The Quick Documentation popup (Alt+Q): a language server's hover text
//! at the caret, drawn next to the caret cell, under it when there is room
//! and above otherwise. Markdown is shown plainly: fences dropped, code
//! lines on the editor background and highlighted by their fence's language
//! (the buffer's language for a bare fence), headings bold, emphasis, inline
//! code marks, escapes and link targets stripped; prose is wrapped to the
//! popup.
//!
//! The app drops the popup once the buffer's version or caret differs from
//! the request's. While it is up, Up/Down/PgUp/PgDn scroll it and Esc closes
//! it; any other key closes it and goes on to the editor
//! (`App::hover_key`), the pattern the completion popup follows too.
//! Quick Definition (`lsp.quick_definition`) shows a heading and code lines
//! through [`HoverPopup::from_lines`].

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
use crate::editor::syntax::{self, LanguageDef};

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

/// A documentation line as drawn: its kind, its text (tabs already expanded to four
/// spaces in code), and for code its syntax styles as sorted, non-overlapping char
/// ranges of `text`.
#[derive(Debug, Clone, PartialEq)]
pub struct DocLine {
    pub kind: Kind,
    pub text: String,
    pub spans: Vec<(std::ops::Range<usize>, Style)>,
}

impl DocLine {
    fn plain(kind: Kind, text: String) -> Self {
        Self { kind, text, spans: Vec::new() }
    }
}

/// What code is highlighted with: the theme, and the language of a fence with no
/// info string (the buffer's language).
#[derive(Clone, Copy)]
pub struct Highlight<'a> {
    pub theme: &'a Theme,
    pub fallback: Option<&'static LanguageDef>,
}

#[derive(Debug)]
pub struct HoverPopup {
    /// What the popup was asked for: dropped when any of these change.
    pub group: GroupId,
    pub buffer: BufferId,
    pub version: u64,
    pub head: usize,
    lines: Vec<DocLine>,
    scroll: usize,
    /// Text rows that fit, as last drawn (a page for PgUp/PgDn).
    page: usize,
    rect: Rect,
}

impl HoverPopup {
    /// The popup for a hover answer; `None` when it has no text.
    pub fn new(group: GroupId, buffer: BufferId, version: u64, head: usize, contents: HoverContents, hl: Highlight<'_>) -> Option<Self> {
        Self::from_lines(group, buffer, version, head, contents_lines(contents, hl))
    }

    /// The popup for ready lines; `None` when there are none.
    pub fn from_lines(group: GroupId, buffer: BufferId, version: u64, head: usize, lines: Vec<DocLine>) -> Option<Self> {
        (!lines.is_empty()).then_some(Self { group, buffer, version, head, lines, scroll: 0, page: 1, rect: Rect::default() })
    }

    #[cfg(test)]
    pub fn lines(&self) -> &[DocLine] {
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
        let natural = self.lines.iter().map(|l| l.text.width()).max().unwrap_or(0) as u16;
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
        draw_lines(frame.buffer_mut(), inner, &rows, self.scroll, theme);
    }
}

/// Draw `rows` from `scroll` into `inner` (a block's inner rect): one row per line,
/// text from `inner.x + 1`, at most `inner.width - 2` cells wide.
pub(crate) fn draw_lines(buf: &mut ratatui::buffer::Buffer, inner: Rect, rows: &[DocLine], scroll: usize, theme: &Theme) {
    let bg = Style::default().bg(theme.popup_bg).fg(theme.fg);
    for (i, line) in rows.iter().skip(scroll).take(inner.height as usize).enumerate() {
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let style = match line.kind {
            Kind::Text => bg,
            Kind::Code => Style::default().bg(theme.bg).fg(theme.fg),
            Kind::Heading => bg.add_modifier(Modifier::BOLD),
            Kind::Rule => theme.dim.bg(theme.popup_bg),
        };
        buf.set_style(row, style);
        if line.kind == Kind::Rule {
            buf.set_stringn(row.x, row.y, "─".repeat(row.width as usize), row.width as usize, style);
            continue;
        }
        let limit = row.x + row.width.saturating_sub(1);
        let chars: Vec<char> = line.text.chars().collect();
        let mut x = row.x + 1;
        let mut at = 0;
        let run = |buf: &mut ratatui::buffer::Buffer, x: &mut u16, from: usize, to: usize, st: Style| {
            if from < to && *x < limit {
                let seg: String = chars[from..to.min(chars.len())].iter().collect();
                *x = buf.set_stringn(*x, row.y, seg, (limit - *x) as usize, st).0;
            }
        };
        for (r, span_style) in &line.spans {
            run(buf, &mut x, at, r.start, style);
            run(buf, &mut x, r.start.max(at), r.end, style.patch(*span_style));
            at = r.end.max(at);
        }
        run(buf, &mut x, at, chars.len(), style);
    }
}

/// Lines of a hover answer: markdown parsed, plain text as it is, code in
/// `MarkedString`s as code; several parts separated by a rule.
pub fn contents_lines(contents: HoverContents, hl: Highlight<'_>) -> Vec<DocLine> {
    let text_lines = |s: &str| s.lines().map(|l| DocLine::plain(Kind::Text, l.to_string())).collect();
    let marked = |m: MarkedString| match m {
        MarkedString::String(s) => markdown(&s, hl),
        MarkedString::LanguageString(l) => code_lines(&l.value, fence_language(&l.language, hl.fallback), hl.theme),
    };
    let parts: Vec<Vec<DocLine>> = match contents {
        HoverContents::Scalar(m) => vec![marked(m)],
        HoverContents::Array(ms) => ms.into_iter().map(marked).collect(),
        HoverContents::Markup(m) if m.kind == MarkupKind::Markdown => vec![markdown(&m.value, hl)],
        HoverContents::Markup(m) => vec![text_lines(&m.value)],
    };
    join_parts(parts)
}

/// Parts joined with a rule between them; blank lines at the ends, next to rules or doubled go.
pub(crate) fn join_parts(parts: Vec<Vec<DocLine>>) -> Vec<DocLine> {
    let mut out: Vec<DocLine> = Vec::new();
    for part in parts.into_iter().map(tidy).filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push(DocLine::plain(Kind::Rule, String::new()));
        }
        out.extend(part);
    }
    out
}

/// Without blank lines at the ends, next to rules or doubled.
fn tidy(lines: Vec<DocLine>) -> Vec<DocLine> {
    let blank = |l: &DocLine| l.kind == Kind::Text && l.text.trim().is_empty();
    let mut out: Vec<DocLine> = Vec::new();
    for l in lines {
        if blank(&l) && out.last().is_none_or(|p| blank(p) || p.kind == Kind::Rule) {
            continue;
        }
        if l.kind == Kind::Rule && out.last().is_some_and(blank) {
            out.pop();
        }
        out.push(l);
    }
    while out.last().is_some_and(blank) {
        out.pop();
    }
    out
}

/// Code lines of `code`, highlighted with `def` (plain when `None` or when the
/// code is huge). Tabs become four spaces.
pub fn code_lines(code: &str, def: Option<&'static LanguageDef>, theme: &Theme) -> Vec<DocLine> {
    let text = code.replace('\t', "    ");
    let mut spans = match def {
        Some(def) if text.len() <= 100_000 => syntax::highlight_text(def, &text, theme),
        _ => Vec::new(),
    };
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            let line = line.trim_end();
            let len = line.chars().count();
            let spans = if i < spans.len() { std::mem::take(&mut spans[i]) } else { Vec::new() };
            let spans = spans.into_iter().filter(|(r, _)| r.start < len).map(|(r, s)| (r.start..r.end.min(len), s)).collect();
            DocLine { kind: Kind::Code, text: line.to_string(), spans }
        })
        .collect()
}

/// The language of a fence's info string: its first word (ends at whitespace, `,`
/// or `{`) by name, the buffer's language for no word, none for an unknown one.
pub(crate) fn fence_language(info: &str, fallback: Option<&'static LanguageDef>) -> Option<&'static LanguageDef> {
    let word = info.trim().split(|c: char| c.is_whitespace() || c == ',' || c == '{').next().unwrap_or("");
    if word.is_empty() { fallback } else { syntax::language_for_alias(word) }
}

/// Markdown as lines to show: fenced blocks become code lines (the fences
/// go), `#` headings and `---` rules are marked, link reference
/// definitions dropped, the rest cleaned by [`inline`].
pub fn markdown(text: &str, hl: Highlight<'_>) -> Vec<DocLine> {
    let mut out = Vec::new();
    // The open fence's marker, its language, and the code seen so far.
    let mut fence: Option<(&str, Option<&'static LanguageDef>)> = None;
    let mut block: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if let Some((f, lang)) = fence {
            if t.starts_with(f) {
                fence = None;
                if !block.is_empty() {
                    out.extend(code_lines(&block.join("\n"), lang, hl.theme));
                }
                block.clear();
            } else {
                block.push(line.trim_end());
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some((&t[..3], fence_language(&t[3..], hl.fallback)));
            continue;
        }
        let t = t.trim_end();
        if t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_')) {
            out.push(DocLine::plain(Kind::Rule, String::new()));
        } else if let Some(h) = t.strip_prefix('#') {
            out.push(DocLine::plain(Kind::Heading, inline(h.trim_start_matches('#').trim())));
        } else if t.starts_with('[') && t.split_once("]: ").is_some_and(|(_, target)| !target.contains(' ')) {
            // `[label]: target`, a link reference definition.
            continue;
        } else {
            out.push(DocLine::plain(Kind::Text, inline(line.trim_end())));
        }
    }
    // An unclosed fence keeps its lines as code.
    if let Some((_, lang)) = fence
        && !block.is_empty()
    {
        out.extend(code_lines(&block.join("\n"), lang, hl.theme));
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
pub(crate) fn link_span(chars: &[char], open: usize) -> Option<(usize, usize)> {
    let close = chars[open + 1..].iter().position(|&c| c == ']')? + open + 1;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = chars[close + 2..].iter().position(|&c| c == ')')? + close + 2;
    Some((close, end))
}

/// Rows of `width` cells: prose wrapped at spaces, code cut by cells (spans cut with it).
pub(crate) fn wrap_lines(lines: &[DocLine], width: usize) -> Vec<DocLine> {
    let mut rows = Vec::new();
    for line in lines {
        match line.kind {
            Kind::Rule => rows.push(DocLine::plain(Kind::Rule, "─".repeat(width))),
            Kind::Code => {
                let mut cur = String::new();
                let (mut a, mut b) = (0, 0);
                let cut = |cur: String, a: usize, b: usize| DocLine {
                    kind: Kind::Code,
                    text: cur,
                    spans: line
                        .spans
                        .iter()
                        .filter(|(r, _)| r.start < b && r.end > a)
                        .map(|(r, s)| (r.start.max(a) - a..r.end.min(b) - a, *s))
                        .collect(),
                };
                for c in line.text.chars() {
                    if cur.width() + c.width().unwrap_or(0) > width {
                        rows.push(cut(std::mem::take(&mut cur), a, b));
                        a = b;
                    }
                    cur.push(c);
                    b += 1;
                }
                rows.push(cut(cur, a, b));
            }
            _ if line.text.is_empty() => rows.push(DocLine::plain(line.kind, String::new())),
            _ => rows.extend(crate::ui::toasts::wrap(&line.text, width, usize::MAX).into_iter().map(|l| DocLine::plain(line.kind, l))),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{LanguageString, MarkupContent};

    fn texts(lines: &[DocLine]) -> Vec<String> {
        lines.iter().map(|l| format!("{:?}:{}", l.kind, l.text)).collect()
    }

    fn hl(theme: &Theme) -> Highlight<'_> {
        Highlight { theme, fallback: None }
    }

    #[test]
    fn markdown_is_shown_plainly() {
        let theme = Theme::default();
        let md = "\n```rust\nfn len(&self) -> usize\n```\n\n---\n\n# Returns\n\nThe **length** of `self`, see [`Vec`](https://x/y) \\[1\\] a[0].\n\n\n[`Vec`]: https://x/y\nlast";
        let lines = contents_lines(HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value: md.into() }), hl(&theme));
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
        assert_eq!(texts(&contents_lines(plain, hl(&theme))), ["Text:a **b**"]);
        let parts = HoverContents::Array(vec![
            MarkedString::LanguageString(LanguageString { language: "rust".into(), value: "struct S".into() }),
            MarkedString::String("".into()),
            MarkedString::String("doc".into()),
        ]);
        assert_eq!(texts(&contents_lines(parts, hl(&theme))), ["Code:struct S", "Rule:", "Text:doc"]);
        assert!(contents_lines(HoverContents::Array(vec![]), hl(&theme)).is_empty());
    }

    #[test]
    fn rows_wrap_prose_and_cut_code() {
        let line = |k, t: &str| DocLine::plain(k, t.to_string());
        let lines = vec![line(Kind::Text, "one two three"), line(Kind::Text, ""), line(Kind::Code, "abcdefgh"), line(Kind::Rule, "")];
        assert_eq!(texts(&wrap_lines(&lines, 5)), ["Text:one", "Text:two", "Text:three", "Text:", "Code:abcde", "Code:fgh", "Rule:─────"]);
        // Spans are cut with the code.
        let st = Style::default().fg(ratatui::style::Color::Red);
        let code = DocLine { kind: Kind::Code, text: "abcdefgh".into(), spans: vec![(3..6, st)] };
        let rows = wrap_lines(&[code], 5);
        assert_eq!(rows[0].spans, [(3..5, st)]);
        assert_eq!(rows[1].spans, [(0..1, st)]);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn fences_pick_their_language_or_the_buffers() {
        let theme = Theme::default();
        let rust = syntax::language_for_name("rust");
        let styled = |md: &str, fallback| {
            let lines = markdown(md, Highlight { theme: &theme, fallback });
            assert_eq!(lines.len(), 1, "{md}");
            !lines[0].spans.is_empty()
        };
        assert!(styled("```rust\nfn a() {}\n```", None));
        assert!(styled("```\nfn a() {}\n```", rust));
        assert!(!styled("```\nfn a() {}\n```", None));
        assert!(!styled("```text\nfn a() {}\n```", rust));
        assert!(styled("```rust,ignore\nfn a() {}\n```", None));
    }
}
