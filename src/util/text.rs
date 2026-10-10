//! Text measurement helpers shared by the editor renderer and widgets.

use unicode_width::UnicodeWidthStr;

/// Display width of a grapheme cluster in terminal cells.
/// Tabs are handled by the caller because their width depends on the column.
pub fn grapheme_width(g: &str) -> usize {
    if let Some(c) = g.chars().next()
        && shown_escaped(c)
    {
        return 2; // rendered as ^X
    }
    g.width()
}

/// Chars the editor and diff panes draw as an escape instead of the char
/// itself: controls, and the line and paragraph separators, which are no
/// line breaks in jetl and which terminals draw unpredictably.
pub fn shown_escaped(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

/// Truncate a string to fit `max` cells, appending `…` when cut.
pub fn fit(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Break `phrases` into lines of at most `max` cells: whole phrases joined
/// by spaces while they fit, a phrase wider than `max` broken between its
/// words, and a word wider than `max` cut with `…`.
pub fn wrap_phrases(phrases: &[&str], max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for p in phrases {
        let units: Vec<&str> = if p.width() <= max { vec![p] } else { p.split_whitespace().collect() };
        for u in units {
            if !cur.is_empty() && cur.width() + 1 + u.width() <= max {
                cur.push(' ');
                cur.push_str(u);
                continue;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            cur = fit(u, max);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Lines of `s` that keep their terminators, split the way the editor's
/// rope counts them: after `\n`, after `\r\n` (one break) and after a lone
/// `\r`. Empty text has no lines; a trailing break starts no extra line.
pub fn lines_inclusive(s: &str) -> impl Iterator<Item = &str> {
    let b = s.as_bytes();
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= b.len() {
            return None;
        }
        let mut i = start;
        while i < b.len() {
            match b[i] {
                b'\n' => {
                    i += 1;
                    break;
                }
                b'\r' => {
                    i += 1;
                    if b.get(i) == Some(&b'\n') {
                        i += 1;
                    }
                    break;
                }
                _ => i += 1,
            }
        }
        let line = &s[start..i];
        start = i;
        Some(line)
    })
}

/// Number of [`lines_inclusive`] lines.
pub fn line_count_inclusive(s: &str) -> usize {
    lines_inclusive(s).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_inclusive_splits_like_the_rope() {
        let v = |s| lines_inclusive(s).collect::<Vec<_>>();
        assert_eq!(v("a\rb\r\nc\nd"), ["a\r", "b\r\n", "c\n", "d"]);
        assert!(v("").is_empty());
        assert_eq!(v("a\n"), ["a\n"]);
        assert_eq!(v("\r\r"), ["\r", "\r"]);
        for s in ["a\rb\r\nc\nd", "a\n", "\r\r", "x\r", "", "abc", "a\n\nb\r\n"] {
            let n = line_count_inclusive(s);
            let len = ropey::Rope::from_str(s).len_lines();
            let ends_with_break = s.ends_with(['\n', '\r']);
            assert_eq!(if ends_with_break { n + 1 } else { n.max(1) }, len, "{s:?}");
        }
    }

    #[test]
    fn wrap_phrases_keeps_phrases_whole_when_they_fit() {
        let p = ["No files open.", "Alt+1 opens the Project view;", "Ctrl+Shift+N finds a file."];
        assert_eq!(wrap_phrases(&p, 80), vec!["No files open. Alt+1 opens the Project view; Ctrl+Shift+N finds a file."]);
        assert_eq!(wrap_phrases(&p, 44), vec!["No files open. Alt+1 opens the Project view;", "Ctrl+Shift+N finds a file."]);
        assert_eq!(wrap_phrases(&p, 42), vec!["No files open.", "Alt+1 opens the Project view;", "Ctrl+Shift+N finds a file."]);
        // A phrase too wide breaks between words; a word too wide is cut
        // with an ellipsis, never silently.
        assert_eq!(wrap_phrases(&p, 16), vec!["No files open.", "Alt+1 opens the", "Project view;", "Ctrl+Shift+N", "finds a file."]);
        assert_eq!(wrap_phrases(&["Ctrl+Shift+N"], 8), vec!["Ctrl+Sh…"]);
        assert!(wrap_phrases(&p, 10).iter().all(|l| l.width() <= 10));
    }
}
