//! Text measurement helpers shared by the editor renderer and widgets.

use unicode_width::UnicodeWidthStr;

/// Display width of a grapheme cluster in terminal cells.
/// Tabs are handled by the caller because their width depends on the column.
pub fn grapheme_width(g: &str) -> usize {
    if let Some(c) = g.chars().next()
        && c.is_control()
    {
        return 2; // rendered as ^X
    }
    g.width()
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
