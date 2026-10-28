//! Rows of file-grouped result lists (Find, Problems): a file header with
//! the name, its directory and a count, and the spans of an item row.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::config::FileIcons;
use crate::config::theme::Theme;

/// `path` relative to `root` with `/` separators, or the full path.
pub fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// A file header on row `r`: the file name in bold, its directory
/// (relative to `root`) and `count` dimmed.
#[allow(clippy::too_many_arguments)]
pub fn file_header(buf: &mut Buffer, r: Rect, root: &Path, path: &Path, count: &str, base: Style, theme: &Theme, icons: FileIcons) {
    let rel = rel_path(root, path);
    let (dir, name) = match rel.rfind('/') {
        Some(k) => (&rel[..=k], &rel[k + 1..]),
        None => ("", rel.as_str()),
    };
    let icon = crate::ui::file_icons::span(name, false, icons, base, theme);
    let mut parts: Vec<(&str, Style)> = Vec::with_capacity(6);
    if let Some((text, st)) = &icon {
        parts.push((text.as_str(), *st));
    }
    parts.extend([
        (name, base.add_modifier(Modifier::BOLD)),
        ("  ", base),
        (dir, base.patch(theme.dim)),
        ("  ", base),
        (count, base.patch(theme.dim)),
    ]);
    spans(buf, r, &parts);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn header(icons: FileIcons, theme: &Theme) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
        let base = Style::default().fg(theme.fg);
        file_header(&mut buf, Rect::new(0, 0, 40, 1), Path::new("/w"), Path::new("/w/src/main.rs"), "3", base, theme, icons);
        buf
    }

    fn text(buf: &Buffer) -> String {
        (0..40).map(|x| buf[(x, 0)].symbol()).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn file_header_draws_icon() {
        let theme = Theme::default();
        assert_eq!(text(&header(FileIcons::None, &theme)), " main.rs  src/  3");
        let buf = header(FileIcons::Ascii, &theme);
        assert_eq!(text(&buf), " R main.rs  src/  3");
        assert_eq!(buf[(1, 0)].fg, theme.capture_style("keyword").unwrap().fg.unwrap());
        assert!(buf[(3, 0)].modifier.contains(Modifier::BOLD));
    }
}
