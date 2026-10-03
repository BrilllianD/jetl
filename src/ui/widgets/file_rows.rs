//! Rows of file-grouped result lists (Find, Problems): a file header with
//! the name, its directory and a count, and the spans of an item row.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;

/// `path` relative to `root` with `/` separators, or the full path.
pub fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// A file header on row `r`: the file name in bold, its directory
/// (relative to `root`) and `count` dimmed.
pub fn file_header(buf: &mut Buffer, r: Rect, root: &Path, path: &Path, count: &str, base: Style, theme: &Theme) {
    let rel = rel_path(root, path);
    let (dir, name) = match rel.rfind('/') {
        Some(k) => (&rel[..=k], &rel[k + 1..]),
        None => ("", rel.as_str()),
    };
    spans(
        buf,
        r,
        &[
            (name, base.add_modifier(Modifier::BOLD)),
            ("  ", base),
            (dir, base.patch(theme.dim)),
            ("  ", base),
            (count, base.patch(theme.dim)),
        ],
    );
}

/// Draw `parts` one after another from `r.x + 1`, cut at the right edge.
/// Returns the column after the last one drawn.
pub fn spans(buf: &mut Buffer, r: Rect, parts: &[(&str, Style)]) -> u16 {
    let mut x = r.x + 1;
    for (text, style) in parts {
        if x >= r.right() {
            break;
        }
        let w = (text.width() as u16).min(r.right() - x);
        (x, _) = buf.set_stringn(x, r.y, text, w as usize, *style);
    }
    x
}
