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

#[cfg(test)]
mod tests {
    use super::*;

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
