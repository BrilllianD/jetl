//! The breadcrumb row (`editor.breadcrumbs`): one row at the bottom of each
//! editor pane whose buffer has scopes (a tags query), showing the caret's
//! enclosing definitions as `{label} {name}` crumbs (short kind labels such as
//! `fn`, `impl`, `class`; Markdown headings bare) joined by ` › `, cut from
//! the left with `…`. An empty path leaves the row blank, so the row height
//! never jumps with the caret. `Editor::render_group` takes the bottom row of
//! the text rect before `view::render`, so `text_area` excludes it. A left press
//! on a crumb's cells (`CrumbHit`, returned by `paint` and kept in
//! `TabGroup::crumb_hits`) goes to its symbol's name, as a sticky pin does.

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

/// The short label shown before a crumb's name; `None` shows the name alone.
pub(super) fn short_kind(kind: &'static str) -> Option<&'static str> {
    match kind {
        "function" | "method" => Some("fn"),
        "mod" | "module" => Some("mod"),
        "constant" => Some("const"),
        "heading" => None,
        // impl, struct, enum, union, trait, macro, type, class, interface, and any other kind
        other => Some(other),
    }
}

/// The crumb texts of an `enclosing` path, one per symbol, same order.
pub(super) fn crumbs(path: &[&Symbol]) -> Vec<String> {
    path.iter()
        .map(|s| match short_kind(s.kind) {
            Some(k) => format!("{k} {}", s.name),
            None => s.name.to_string(),
        })
        .collect()
}

fn width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// What a run of `fit_tail` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    /// Text of crumb `i` (its index in `crumbs`).
    Crumb(usize),
    /// A ` › ` separator (or what is left of it after a cut).
    Sep,
    /// The leading `…` of a cut row.
    Ellipsis,
}

/// One crumb's cells in the row as last drawn (`x0..x1`, `y` is the row's) and
/// where its symbol's name is (0-based line, char column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CrumbHit {
    pub(super) x0: u16,
    pub(super) x1: u16,
    pub(super) line: usize,
    pub(super) col: usize,
}

/// `crumbs` joined by the separator as runs of `(text, part)`; when wider
/// than `max` cells, cut from the left with a leading `…`. The status bar's
/// path crumbs use it too.
pub(crate) fn fit_tail(crumbs: &[String], max: usize) -> Vec<(String, Part)> {
    let mut chars: Vec<(char, Part)> = Vec::new();
    for (i, c) in crumbs.iter().enumerate() {
        if i > 0 {
            chars.extend(SEP.chars().map(|ch| (ch, Part::Sep)));
        }
        chars.extend(c.chars().map(|ch| (ch, Part::Crumb(i))));
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
        chars.insert(0, ('…', Part::Ellipsis));
    }
    let mut runs: Vec<(String, Part)> = Vec::new();
    for (c, part) in chars {
        match runs.last_mut() {
            Some((t, p)) if *p == part => t.push(c),
            _ => runs.push((c.to_string(), part)),
        }
    }
    runs
}

/// Paint the row (the style over all of `row`, one blank cell, then the crumbs,
/// separators in `dim`'s foreground) and return the cells of each drawn crumb;
/// `targets[i]` is crumb `i`'s `(line, col)`.
pub(super) fn paint(
    screen: &mut ratatui::buffer::Buffer,
    row: Rect,
    crumbs: &[String],
    targets: &[(usize, usize)],
    theme: &Theme,
) -> Vec<CrumbHit> {
    debug_assert_eq!(crumbs.len(), targets.len());
    screen.set_style(row, theme.breadcrumbs);
    let mut hits = Vec::new();
    if row.width < 2 {
        return hits;
    }
    let sep = theme.breadcrumbs.patch(theme.dim);
    let mut x = row.x + 1;
    let end = row.right();
    for (text, part) in fit_tail(crumbs, row.width as usize - 1) {
        let style = if part == Part::Sep { sep } else { theme.breadcrumbs };
        screen.set_stringn(x, row.y, &text, (end - x) as usize, style);
        let w = text.chars().map(width).sum::<usize>() as u16;
        if let Part::Crumb(i) = part
            && let Some(&(line, col)) = targets.get(i)
        {
            let x1 = (x + w).min(end);
            if x < x1 {
                hits.push(CrumbHit { x0: x, x1, line, col });
            }
        }
        x += w;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, kind: &'static str) -> Symbol {
        Symbol { name: name.into(), kind, line: 0, col: 0, start_line: 0, end_line: 0, depth: 0 }
    }

    #[test]
    fn short_kind_maps_every_listed_kind() {
        assert_eq!(short_kind("function"), Some("fn"));
        assert_eq!(short_kind("method"), Some("fn"));
        assert_eq!(short_kind("impl"), Some("impl"));
        for k in ["struct", "enum", "union", "trait", "macro", "type", "class", "interface"] {
            assert_eq!(short_kind(k), Some(k));
        }
        assert_eq!(short_kind("mod"), Some("mod"));
        assert_eq!(short_kind("module"), Some("mod"));
        assert_eq!(short_kind("constant"), Some("const"));
        assert_eq!(short_kind("heading"), None);
        assert_eq!(short_kind("field"), Some("field"));
    }

    #[test]
    fn crumbs_use_short_labels() {
        let (a, b) = (sym("Point", "impl"), sym("new", "method"));
        assert_eq!(crumbs(&[&a, &b]), ["impl Point", "fn new"]);
        let (a, b) = (sym("A", "class"), sym("m", "method"));
        assert_eq!(crumbs(&[&a, &b]), ["class A", "fn m"]);
        let (a, b) = (sym("Top", "heading"), sym("Sub", "heading"));
        assert_eq!(crumbs(&[&a, &b]), ["Top", "Sub"]);
        let a = sym("x", "field");
        assert_eq!(crumbs(&[&a]), ["field x"]);
        assert!(crumbs(&[]).is_empty());
    }

    fn joined(runs: &[(String, Part)]) -> String {
        runs.iter().map(|(t, _)| t.as_str()).collect()
    }

    fn cells(runs: &[(String, Part)]) -> usize {
        joined(runs).chars().map(width).sum()
    }

    #[test]
    fn fit_tail_joins_and_cuts_from_the_left() {
        let c = ["impl Foo".to_string(), "method bar".to_string()];
        let runs = fit_tail(&c, 40);
        assert_eq!(joined(&runs), "impl Foo › method bar");
        assert_eq!(
            runs,
            [("impl Foo".to_string(), Part::Crumb(0)), (" › ".to_string(), Part::Sep), ("method bar".to_string(), Part::Crumb(1))]
        );
        let runs = fit_tail(&c, 12);
        assert_eq!(joined(&runs), "… method bar");
        assert_eq!(runs, [("…".to_string(), Part::Ellipsis), (" ".to_string(), Part::Sep), ("method bar".to_string(), Part::Crumb(1))]);
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

    fn row_text(screen: &ratatui::buffer::Buffer, w: u16) -> String {
        (0..w).map(|x| screen[(x, 0)].symbol().to_string()).collect()
    }

    #[test]
    fn paint_records_crumb_cells() {
        let theme = Theme::default();
        let c = ["impl A".to_string(), "fn b".to_string()];
        let t = [(0, 5), (1, 7)];
        let mut screen = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 30, 1));
        let hits = paint(&mut screen, Rect::new(0, 0, 30, 1), &c, &t, &theme);
        assert_eq!(hits, [CrumbHit { x0: 1, x1: 7, line: 0, col: 5 }, CrumbHit { x0: 10, x1: 14, line: 1, col: 7 }]);
        let mut screen = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 1, 1));
        assert!(paint(&mut screen, Rect::new(0, 0, 1, 1), &c, &t, &theme).is_empty());
    }

    #[test]
    fn paint_cut_crumb_keeps_its_visible_cells() {
        let theme = Theme::default();
        let c = ["impl A".to_string(), "fn b".to_string()];
        let t = [(0, 5), (1, 7)];
        let mut screen = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 10, 1));
        let hits = paint(&mut screen, Rect::new(0, 0, 10, 1), &c, &t, &theme);
        assert_eq!(row_text(&screen, 10), " …A › fn b");
        assert_eq!(hits, [CrumbHit { x0: 2, x1: 3, line: 0, col: 5 }, CrumbHit { x0: 6, x1: 10, line: 1, col: 7 }]);
        assert!(hits.iter().all(|h| !(h.x0 <= 1 && 1 < h.x1)));
    }
}
