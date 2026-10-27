//! The Markdown preview's renderer: one plain text plus styled char ranges,
//! line for line with the source (only the fence lines go). Headings, lists
//! with task boxes, quotes, inline emphasis, code, links and images, fences
//! highlighted by their language, aligned GFM tables and rules are drawn as
//! text; nothing is reflowed. [`crate::ui::hover::markdown`] is the separate,
//! popup-oriented cleaner and is not used here.

use std::ops::Range;

use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::ui::hover::{code_lines, fence_language, link_span};

/// Rendered Markdown.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rendered {
    /// The rendered lines joined by `\n` (no trailing `\n`).
    pub text: String,
    /// Sorted, non-overlapping char ranges of `text`, none empty, none over a `\n`.
    pub spans: Vec<(Range<usize>, Style)>,
}

type Segs = Vec<(String, Style)>;

/// Builds the text line by line and records a span per styled segment.
#[derive(Default)]
struct Out {
    text: String,
    chars: usize,
    spans: Vec<(Range<usize>, Style)>,
    first: bool,
}

impl Out {
    fn new() -> Self {
        Out { first: true, ..Out::default() }
    }

    fn line(&mut self, segs: &[(String, Style)]) {
        if !self.first {
            self.text.push('\n');
            self.chars += 1;
        }
        self.first = false;
        for (s, style) in segs {
            let n = s.chars().count();
            if n == 0 {
                continue;
            }
            let start = self.chars;
            self.text.push_str(s);
            self.chars += n;
            if *style == Style::default() {
                continue;
            }
            match self.spans.last_mut() {
                Some((r, st)) if r.end == start && st == style => r.end = self.chars,
                _ => self.spans.push((start..self.chars, *style)),
            }
        }
    }

    fn finish(self) -> Rendered {
        Rendered { text: self.text, spans: self.spans }
    }
}

/// Render `source` as a preview text with styles from `theme`.
pub fn render(source: &str, theme: &Theme) -> Rendered {
    let src: Vec<&str> = source.lines().collect();
    let mut out = Out::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let line = src[i];
        let t = line.trim_start();
        let te = t.trim_end();
        let indent = indent_of(line);
        let item = list_item(line);
        if item.is_none() && !te.is_empty() && indent == 0 {
            stack.clear();
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            let marker = &t[..3];
            let lang = fence_language(&t[3..], None);
            let mut block = Vec::new();
            i += 1;
            while i < src.len() && !src[i].trim_start().starts_with(marker) {
                block.push(src[i].trim_end());
                i += 1;
            }
            fence(&mut out, &block, lang, theme);
        } else if is_rule(te) {
            out.line(&[("─".repeat(40), theme.dim)]);
        } else if let Some(text) = heading(te) {
            let style = theme.capture_style("text.title").unwrap_or_else(|| Style::default().fg(theme.fg).add_modifier(Modifier::BOLD));
            out.line(&inline(text, style, theme));
        } else if is_table_start(&src, i) {
            let mut end = i + 2;
            while end < src.len() && src[end].contains('|') && !src[end].trim().is_empty() {
                end += 1;
            }
            table(&mut out, &src[i..end], theme);
            i = end - 1;
        } else if t.starts_with('>') {
            let mut rest = t;
            let mut segs: Segs = Vec::new();
            while let Some(r) = rest.strip_prefix('>') {
                segs.push(("│ ".to_string(), theme.dim));
                rest = r.strip_prefix(' ').unwrap_or(r).trim_start();
            }
            segs.extend(inline(rest.trim_end(), theme.dim, theme));
            out.line(&segs);
        } else if let Some((marker, rest)) = item {
            while stack.last().is_some_and(|&top| top > indent) {
                stack.pop();
            }
            if stack.last().is_none_or(|&top| top < indent) {
                stack.push(indent);
            }
            let mut segs: Segs = vec![("  ".repeat(stack.len() - 1), Style::default())];
            let (boxed, rest) = task_box(rest);
            match (marker, boxed) {
                (Marker::Bullet, Some(b)) => segs.push((format!("{b} "), Style::default())),
                (Marker::Bullet, None) => segs.push(("• ".to_string(), Style::default())),
                (Marker::Number(n), Some(b)) => segs.push((format!("{n} {b} "), Style::default())),
                (Marker::Number(n), None) => segs.push((format!("{n} "), Style::default())),
            }
            segs.extend(inline(rest.trim_end(), Style::default(), theme));
            out.line(&segs);
        } else {
            out.line(&inline(line.trim_end(), Style::default(), theme));
        }
        i += 1;
    }
    out.finish()
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

fn is_rule(te: &str) -> bool {
    let s: String = te.chars().filter(|c| *c != ' ').collect();
    s.chars().count() >= 3 && ["-", "*", "_"].iter().any(|m| s.chars().all(|c| m.starts_with(c)))
}

/// The text of an ATX heading line (`te` is trimmed), `#`s and closing `#`s gone.
fn heading(te: &str) -> Option<&str> {
    let hashes = te.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &te[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let rest = rest.trim();
    let stripped = rest.trim_end_matches('#');
    Some(if stripped.is_empty() || stripped.ends_with(' ') { stripped.trim() } else { rest })
}

enum Marker {
    Bullet,
    Number(String),
}

/// A list item line: its marker and the text after it.
fn list_item(line: &str) -> Option<(Marker, &str)> {
    let t = line.trim_start();
    if let Some(rest) = ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m)) {
        return Some((Marker::Bullet, rest));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let after = &t[digits..];
        let punct = after.chars().next().filter(|c| matches!(c, '.' | ')'))?;
        let rest = after[1..].strip_prefix(' ')?;
        return Some((Marker::Number(format!("{}{punct}", &t[..digits])), rest));
    }
    None
}

/// `[ ]` / `[x]` at the start of an item: the box and the rest.
fn task_box(rest: &str) -> (Option<char>, &str) {
    for (mark, b) in [("[ ]", '☐'), ("[x]", '☑'), ("[X]", '☑')] {
        if let Some(r) = rest.strip_prefix(mark) {
            if r.is_empty() {
                return (Some(b), r);
            }
            if let Some(r) = r.strip_prefix(' ') {
                return (Some(b), r);
            }
        }
    }
    (None, rest)
}

/// A fenced block: code lines indented by two cells, highlighted by `lang`.
fn fence(out: &mut Out, block: &[&str], lang: Option<&'static crate::editor::syntax::LanguageDef>, theme: &Theme) {
    if block.is_empty() {
        return;
    }
    let code = code_lines(&block.join("\n"), lang, theme);
    // `code_lines` drops a trailing empty line; every block line still gets one.
    for n in 0..block.len() {
        let Some(dl) = code.get(n).filter(|dl| !dl.text.is_empty()) else {
            out.line(&[]);
            continue;
        };
        let chars: Vec<char> = dl.text.chars().collect();
        let mut segs: Segs = vec![("  ".to_string(), Style::default())];
        let mut pos = 0;
        for (r, style) in &dl.spans {
            let (a, b) = (r.start.max(pos), r.end.min(chars.len()));
            if a >= b {
                continue;
            }
            segs.push((chars[pos..a].iter().collect(), Style::default()));
            segs.push((chars[a..b].iter().collect(), *style));
            pos = b;
        }
        segs.push((chars[pos..].iter().collect(), Style::default()));
        out.line(&segs);
    }
}

fn is_delimiter_row(line: &str) -> bool {
    let cells = split_row(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.strip_prefix(':').unwrap_or(c);
            let c = c.strip_suffix(':').unwrap_or(c);
            !c.is_empty() && c.chars().all(|ch| ch == '-')
        })
}

fn is_table_start(src: &[&str], i: usize) -> bool {
    src[i].contains('|') && src.get(i + 1).is_some_and(|n| n.contains('-') && is_delimiter_row(n))
}

/// The cells of a table row: edge pipes gone, split on unescaped `|`, trimmed.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") { &t[..t.len() - 1] } else { t };
    let mut cells = vec![String::new()];
    let mut prev = ' ';
    for c in t.chars() {
        if c == '|' && prev != '\\' {
            cells.push(String::new());
        } else if let Some(last) = cells.last_mut() {
            last.push(c);
        }
        prev = c;
    }
    cells.into_iter().map(|c| c.trim().replace("\\|", "|")).collect()
}

fn segs_width(segs: &[(String, Style)]) -> usize {
    segs.iter().map(|(s, _)| s.width()).sum()
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// A table block (`rows[0]` header, `rows[1]` delimiter row): aligned columns.
fn table(out: &mut Out, rows: &[&str], theme: &Theme) {
    let header = split_row(rows[0]);
    let cols = header.len();
    let aligns: Vec<Align> = split_row(rows[1])
        .iter()
        .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
        .chain(std::iter::repeat(Align::Left))
        .take(cols)
        .collect();
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let cells: Vec<Vec<Segs>> = std::iter::once((0, bold))
        .chain((2..rows.len()).map(|r| (r, Style::default())))
        .map(|(r, base)| {
            let mut row = split_row(rows[r]);
            row.resize(cols, String::new());
            row.iter().map(|c| inline(c, base, theme)).collect()
        })
        .collect();
    let widths: Vec<usize> = (0..cols).map(|c| cells.iter().map(|row| segs_width(&row[c])).max().unwrap_or(0)).collect();
    let sep = (" │ ".to_string(), theme.dim);
    for (n, row) in cells.into_iter().enumerate() {
        if n == 1 {
            let mut segs: Segs = Vec::new();
            for (c, w) in widths.iter().enumerate() {
                if c > 0 {
                    segs.push(("─┼─".to_string(), theme.dim));
                }
                segs.push(("─".repeat(*w), theme.dim));
            }
            out.line(&segs);
        }
        let mut segs: Segs = Vec::new();
        for (c, cell) in row.into_iter().enumerate() {
            if c > 0 {
                segs.push(sep.clone());
            }
            let gap = widths[c] - segs_width(&cell);
            let left = match aligns[c] {
                Align::Left => 0,
                Align::Right => gap,
                Align::Center => gap / 2,
            };
            segs.push((" ".repeat(left), Style::default()));
            segs.extend(cell);
            segs.push((" ".repeat(gap - left), Style::default()));
        }
        out.line(&segs);
    }
}

/// Inline Markdown as styled segments over `base`.
fn inline(s: &str, base: Style, theme: &Theme) -> Segs {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Segs::new();
    scan(&chars, base, theme, &mut out);
    out
}

fn push(out: &mut Segs, c: char, style: Style) {
    match out.last_mut() {
        Some((s, st)) if *st == style => s.push(c),
        _ => out.push((c.to_string(), style)),
    }
}

fn push_all(out: &mut Segs, chars: &[char], style: Style) {
    for &c in chars {
        push(out, c, style);
    }
}

/// For a backtick run at `i`: its length and the start of the next run of
/// exactly that length.
fn code_close(chars: &[char], i: usize) -> (usize, Option<usize>) {
    let n = chars[i..].iter().take_while(|&&c| c == '`').count();
    let mut j = i + n;
    while j < chars.len() {
        if chars[j] == '`' {
            let m = chars[j..].iter().take_while(|&&c| c == '`').count();
            if m == n {
                return (n, Some(j));
            }
            j += m;
        } else {
            j += 1;
        }
    }
    (n, None)
}

/// The closer of an emphasis marker `c` (`len` 1 or 2) opened before `from`.
fn find_closer(chars: &[char], from: usize, c: char, len: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        let ch = chars[j];
        if ch == '\\' {
            j += 2;
        } else if ch == '`' {
            let (n, close) = code_close(chars, j);
            j = close.map_or(j + n, |k| k + n);
        } else if ch == c {
            let pair = chars.get(j + 1) == Some(&c);
            if len == 1 && pair {
                j += 2;
                continue;
            }
            let after = if len == 2 { j + 2 } else { j + 1 };
            let fits = (len == 1 || pair)
                && j > from
                && !chars[j - 1].is_whitespace()
                && (c != '_' || !chars.get(after).is_some_and(|a| a.is_alphanumeric()));
            if fits {
                return Some(j);
            }
            j += 1;
        } else {
            j += 1;
        }
    }
    None
}

fn scan(chars: &[char], base: Style, theme: &Theme, out: &mut Segs) {
    let literal = theme.capture_style("text.literal").unwrap_or_default();
    let reference = theme.capture_style("text.reference").unwrap_or_else(|| Style::default().add_modifier(Modifier::UNDERLINED));
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                push(out, chars[i + 1], base);
                i += 2;
            }
            '`' => {
                let (n, close) = code_close(chars, i);
                match close {
                    Some(j) => {
                        push_all(out, &chars[i + n..j], base.patch(literal));
                        i = j + n;
                    }
                    None => {
                        push_all(out, &chars[i..i + n], base);
                        i += n;
                    }
                }
            }
            '!' if chars.get(i + 1) == Some(&'[') && link_span(chars, i + 1).is_some() => {
                let (close, end) = link_span(chars, i + 1).unwrap_or_default();
                let alt: String = chars[i + 2..close].iter().collect();
                for ch in format!("[image: {alt}]").chars() {
                    push(out, ch, base.patch(theme.dim));
                }
                i = end + 1;
            }
            '[' if link_span(chars, i).is_some() => {
                let (close, end) = link_span(chars, i).unwrap_or_default();
                scan(&chars[i + 1..close], base.patch(reference), theme, out);
                i = end + 1;
            }
            '*' | '_' => {
                let len = if chars.get(i + 1) == Some(&c) { 2 } else { 1 };
                let opens =
                    chars.get(i + len).is_some_and(|n| !n.is_whitespace()) && (c == '*' || i == 0 || !chars[i - 1].is_alphanumeric());
                match opens.then(|| find_closer(chars, i + len, c, len)).flatten() {
                    Some(j) => {
                        let modifier = if len == 2 { Modifier::BOLD } else { Modifier::ITALIC };
                        scan(&chars[i + len..j], base.add_modifier(modifier), theme, out);
                        i = j + len;
                    }
                    None => {
                        push_all(out, &chars[i..i + len], base);
                        i += len;
                    }
                }
            }
            _ => {
                push(out, c, base);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn theme() -> Theme {
        let mut t = Theme::default();
        t.syntax.insert("text.title".into(), Style::default().fg(Color::Indexed(201)));
        t.syntax.insert("text.literal".into(), Style::default().fg(Color::Indexed(202)));
        t.syntax.insert("text.reference".into(), Style::default().fg(Color::Indexed(203)));
        t.dim = Style::default().fg(Color::Indexed(204));
        t
    }

    fn title() -> Style {
        Style::default().fg(Color::Indexed(201))
    }
    fn literal() -> Style {
        Style::default().fg(Color::Indexed(202))
    }
    fn reference() -> Style {
        Style::default().fg(Color::Indexed(203))
    }
    fn dim() -> Style {
        Style::default().fg(Color::Indexed(204))
    }
    fn bold() -> Modifier {
        Modifier::BOLD
    }

    fn lines(r: &Rendered) -> Vec<&str> {
        r.text.split('\n').collect()
    }

    fn at(r: &Rendered, needle: &str) -> Range<usize> {
        let byte = r.text.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {:?}", r.text));
        let start = r.text[..byte].chars().count();
        start..start + needle.chars().count()
    }

    fn style_of(r: &Rendered, needle: &str) -> Style {
        let w = at(r, needle);
        let hit: Vec<_> = r.spans.iter().filter(|(s, _)| s.start < w.end && s.end > w.start).collect();
        match hit.as_slice() {
            [] => Style::default(),
            [(s, st)] if s.start <= w.start && s.end >= w.end => *st,
            other => panic!("{needle:?} {w:?} is split by {other:?}"),
        }
    }

    fn assert_valid(r: &Rendered) {
        let chars: Vec<char> = r.text.chars().collect();
        let mut prev = 0;
        for (range, style) in &r.spans {
            assert!(range.start < range.end, "empty span {range:?}");
            assert!(range.start >= prev, "unsorted or overlapping at {range:?}");
            assert!(range.end <= chars.len(), "span {range:?} past the end ({})", chars.len());
            assert!(!chars[range.clone()].contains(&'\n'), "span {range:?} covers a newline");
            assert_ne!(*style, Style::default(), "default style span {range:?}");
            prev = range.end;
        }
    }

    #[test]
    fn headings_lose_hashes() {
        let r = render("# Title\n### Sub ###\n#nope", &theme());
        assert_eq!(lines(&r), ["Title", "Sub", "#nope"]);
        assert_eq!(style_of(&r, "Title"), title());
        assert_eq!(style_of(&r, "Sub"), title());
        assert_eq!(style_of(&r, "#nope"), Style::default());
        assert_valid(&r);

        let mut t = Theme::default();
        t.syntax.remove("text.title");
        t.syntax.remove("text");
        let r = render("# Title", &t);
        assert_eq!(style_of(&r, "Title"), Style::default().fg(t.fg).add_modifier(Modifier::BOLD));
        assert_valid(&r);
    }

    #[test]
    fn lists_nest_number_and_box() {
        let r = render("- a\n  * b\n    + c\n1. one\n2) two\n- [ ] todo\n- [x] done\n3. [X] n", &theme());
        assert_eq!(lines(&r), ["• a", "  • b", "    • c", "1. one", "2) two", "☐ todo", "☑ done", "3. ☑ n"]);
        assert_valid(&r);
        let r = render("- a\n    - deep\n- b", &theme());
        assert_eq!(lines(&r), ["• a", "  • deep", "• b"]);
        assert_valid(&r);
    }

    #[test]
    fn quotes_nest_in_dim() {
        let r = render("> a\n>> b\n> > *c*", &theme());
        assert_eq!(lines(&r), ["│ a", "│ │ b", "│ │ c"]);
        assert_eq!(style_of(&r, "│ a"), dim());
        assert_eq!(style_of(&r, "c"), dim().add_modifier(Modifier::ITALIC));
        assert_valid(&r);
    }

    #[test]
    fn inline_styles() {
        let r = render("**b** __u__ *i* _j_ `x` [t](http://u) ![alt](p.png) snake_case a * b \\*e\\*", &theme());
        assert_eq!(r.text, "b u i j x t [image: alt] snake_case a * b *e*");
        assert_eq!(style_of(&r, "b"), Style::default().add_modifier(bold()));
        assert_eq!(style_of(&r, "u"), Style::default().add_modifier(bold()));
        assert_eq!(style_of(&r, "i"), Style::default().add_modifier(Modifier::ITALIC));
        assert_eq!(style_of(&r, "j"), Style::default().add_modifier(Modifier::ITALIC));
        assert_eq!(style_of(&r, "x"), literal());
        assert_eq!(style_of(&r, "t"), reference());
        assert_eq!(style_of(&r, "[image: alt]"), dim());
        assert_eq!(style_of(&r, "snake_case a * b *e*"), Style::default());
        assert_valid(&r);

        let r = render("**[bold link](u)**", &theme());
        assert_eq!(r.text, "bold link");
        assert_eq!(style_of(&r, "bold link"), reference().add_modifier(bold()));
        assert_valid(&r);

        let r = render("**open", &theme());
        assert_eq!(r.text, "**open");
        assert!(r.spans.is_empty());

        let r = render("`*a*`", &theme());
        assert_eq!(r.text, "*a*");
        assert_eq!(style_of(&r, "*a*"), literal());
        assert_valid(&r);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn fences_highlight_by_language() {
        let r = render("```rust\nfn a() {}\n\n```\n~~~nosuch\nfn a() {}\n~~~", &theme());
        assert_eq!(lines(&r), ["  fn a() {}", "", "  fn a() {}"]);
        let first = r.text.find('\n').map(|b| r.text[..b].chars().count()).unwrap_or(0);
        let line0: Vec<_> = r.spans.iter().filter(|(s, _)| s.end <= first).collect();
        assert!(!line0.is_empty(), "no span in the rust line");
        assert_eq!(line0[0].0.start, 2, "first span is shifted by the indent");
        let line2_start = r.text.rfind('\n').map(|b| r.text[..b].chars().count()).unwrap_or(0);
        assert!(r.spans.iter().all(|(s, _)| s.end <= first || s.start < first), "span outside line 0");
        assert!(r.spans.iter().all(|(s, _)| s.end <= line2_start), "unknown language is plain");
        assert_valid(&r);

        let r = render("```\nx", &theme());
        assert_eq!(lines(&r), ["  x"]);
        assert_valid(&r);
    }

    #[test]
    fn tables_align() {
        let r = render("| L | C | R |\n|:--|:-:|--:|\n| a | bb | c |\n| long cell | x |", &theme());
        assert_eq!(lines(&r), ["L         │ C  │ R", "──────────┼────┼──", "a         │ bb │ c", "long cell │ x  │  "]);
        assert_eq!(style_of(&r, "L"), Style::default().add_modifier(bold()));
        assert_eq!(style_of(&r, "│"), dim());
        assert_eq!(style_of(&r, "──────────┼────┼──"), dim());
        assert_valid(&r);
    }

    #[test]
    fn rules_are_dim() {
        let r = render("---\n* * *\n___", &theme());
        let rule = "─".repeat(40);
        assert_eq!(lines(&r), [rule.as_str(); 3]);
        assert_eq!(r.spans.len(), 3);
        assert!(r.spans.iter().all(|(_, s)| *s == dim()));
        assert_valid(&r);
        let r = render("--", &theme());
        assert_eq!(r.text, "--");
        assert!(r.spans.is_empty());
    }

    #[test]
    fn paragraphs_and_html_line_for_line() {
        let r = render("one  \n<div>x</div>\n\ntwo", &theme());
        assert_eq!(lines(&r), ["one", "<div>x</div>", "", "two"]);
        assert_valid(&r);
        assert_eq!(render("", &theme()), Rendered::default());
    }

    #[test]
    fn char_ranges_survive_non_ascii() {
        let r = render("# Привет 🚀\n| я | b |\n|---|---|\n| 中文 | **ж** |", &theme());
        assert_valid(&r);
        assert_eq!(style_of(&r, "Привет 🚀"), title());
        assert_eq!(style_of(&r, "ж"), Style::default().add_modifier(bold()));
        let l = lines(&r);
        assert_eq!(l[1], "я    │ b");
        assert_eq!(l[3], "中文 │ ж");
    }
}
