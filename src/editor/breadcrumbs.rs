//! The breadcrumb row (`editor.breadcrumbs`): one row at the bottom of each
//! editor pane whose buffer has scopes (a tags query), showing the caret's
//! enclosing definitions as `{kind} {name}` crumbs joined by ` › `, cut from
//! the left with `…`. An empty path leaves the row blank, so the row height
//! never jumps with the caret. `Editor::render_group` takes the bottom row of
//! the text rect before `view::render`, so `text_area` excludes it.

use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use super::buffer::Buffer;
use crate::config::theme::Theme;
use crate::symbols::Symbol;

const SEP: &str = " › ";

/// Whether `b` has scopes: its language has a tags query. Independent of
/// whether the symbol list is empty, so the row never comes and goes while
/// typing.
pub(crate) fn has_scopes(b: &Buffer) -> bool {
    b.syntax.as_ref().is_some_and(|s| s.def.tags_query().is_some())
}

/// The crumb texts of an `enclosing` path.
pub(super) fn crumbs(path: &[&Symbol]) -> Vec<String> {
    path.iter().map(|s| format!("{} {}", s.kind, s.name)).collect()
}

fn width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// `crumbs` joined by the separator as runs of `(text, is_separator)`; when
/// wider than `max` cells, cut from the left with a leading `…`.
pub(super) fn fit_tail(crumbs: &[String], max: usize) -> Vec<(String, bool)> {
    let mut chars: Vec<(char, bool)> = Vec::new();
    for (i, c) in crumbs.iter().enumerate() {
        if i > 0 {
            chars.extend(SEP.chars().map(|ch| (ch, true)));
        }
        chars.extend(c.chars().map(|ch| (ch, false)));
    }
    if max == 0 {
        return Vec::new();
    }
    let total: usize = chars.iter().map(|(c, _)| width(*c)).sum();
    if total > max {
        let mut used = 1; // the ellipsis
        let mut from = chars.len();
        while from > 0 {
            let w = width(chars[from - 1].0);
            if used + w > max {
                break;
            }
            used += w;
            from -= 1;
        }
        chars.drain(..from);
        chars.insert(0, ('…', false));
    }
    let mut runs: Vec<(String, bool)> = Vec::new();
    for (c, sep) in chars {
        match runs.last_mut() {
            Some((t, s)) if *s == sep => t.push(c),
            _ => runs.push((c.to_string(), sep)),
        }
    }
    runs
}

/// Paint the row: the style over all of `row`, one blank cell, then the crumbs
/// (separators in `dim`'s foreground).
pub(super) fn paint(screen: &mut ratatui::buffer::Buffer, row: Rect, crumbs: &[String], theme: &Theme) {
    screen.set_style(row, theme.breadcrumbs);
    if row.width < 2 {
        return;
    }
    let sep = theme.breadcrumbs.patch(theme.dim);
    let mut x = row.x + 1;
    let end = row.right();
    for (text, is_sep) in fit_tail(crumbs, row.width as usize - 1) {
        let style = if is_sep { sep } else { theme.breadcrumbs };
        screen.set_stringn(x, row.y, &text, (end - x) as usize, style);
        x += text.chars().map(width).sum::<usize>() as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(runs: &[(String, bool)]) -> String {
        runs.iter().map(|(t, _)| t.as_str()).collect()
    }

    fn cells(runs: &[(String, bool)]) -> usize {
        joined(runs).chars().map(width).sum()
    }

    #[test]
    fn fit_tail_joins_and_cuts_from_the_left() {
        let c = ["impl Foo".to_string(), "method bar".to_string()];
        let runs = fit_tail(&c, 40);
        assert_eq!(joined(&runs), "impl Foo › method bar");
        assert_eq!(runs, [("impl Foo".to_string(), false), (" › ".to_string(), true), ("method bar".to_string(), false)]);
        let runs = fit_tail(&c, 12);
        assert_eq!(joined(&runs), "… method bar");
        assert_eq!(cells(&runs), 12);
        assert!(fit_tail(&c, 0).is_empty());
        assert!(fit_tail(&[], 10).is_empty());
    }

    #[test]
    fn fit_tail_never_exceeds_wide_chars() {
        let c = ["fn 名前".to_string(), "fn 名前名前".to_string()];
        for max in 1..30 {
            assert!(cells(&fit_tail(&c, max)) <= max, "max {max}");
        }
    }
}
