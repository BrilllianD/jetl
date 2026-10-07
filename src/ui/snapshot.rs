//! Text snapshots of a drawn frame for render regression tests
//! (`app/snapshot_tests.rs`, compared with `insta`).
//!
//! A snapshot is a header, `WxH cursor=(x,y)` or `WxH cursor=hidden`, one
//! line per screen row (`NN ` and the row's text, right-trimmed), then a
//! marks grid: a legend (`-- marks: C=caret S=selection`) and, for each row
//! with a marked cell, the same columns with each cell's mark letter.
//! Colors are never printed; a mark names a theme field instead, so a
//! theme tweak changes no snapshot and a misplaced highlight still shows.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

/// A letter for the cells `hit` accepts in the marks grid.
pub(crate) struct Mark {
    pub ch: char,
    pub name: &'static str,
    hit: Box<dyn Fn(&Cell) -> bool>,
}

impl Mark {
    /// Cells with background `color`.
    pub(crate) fn bg(ch: char, name: &'static str, color: Color) -> Self {
        Self { ch, name, hit: Box::new(move |c| c.bg == color) }
    }

    /// Cells that carry every part `style` sets: its foreground, its
    /// background and the modifiers it adds.
    pub(crate) fn style(ch: char, name: &'static str, style: Style) -> Self {
        let hit = move |c: &Cell| {
            style.fg.is_none_or(|fg| c.fg == fg) && style.bg.is_none_or(|bg| c.bg == bg) && c.modifier.contains(style.add_modifier)
        };
        Self { ch, name, hit: Box::new(hit) }
    }

    /// Cells with `modifier` set.
    pub(crate) fn modifier(ch: char, name: &'static str, modifier: Modifier) -> Self {
        Self { ch, name, hit: Box::new(move |c| c.modifier.contains(modifier)) }
    }

    /// Underlined cells whose underline has `color` (diagnostics).
    pub(crate) fn underline(ch: char, name: &'static str, color: Color) -> Self {
        Self { ch, name, hit: Box::new(move |c| c.modifier.contains(Modifier::UNDERLINED) && c.underline_color == color) }
    }
}

/// The first `w` columns of row `y` as they show: a wide symbol's
/// trailing cell is hidden behind it.
pub(crate) fn row_text(buf: &Buffer, y: u16, w: u16) -> String {
    let mut text = String::new();
    let mut x = 0;
    while x < w {
        let symbol = buf[(x, y)].symbol();
        text.push_str(symbol);
        x += symbol.width().max(1) as u16;
    }
    text
}

/// The marks of row `y` between columns `x0` and `x1` (from the row's
/// start, so the letters line up with [`row_text`]): the first mark that
/// accepts a cell wins, a wide symbol's letter repeats over its width.
fn mark_row(buf: &Buffer, y: u16, (x0, x1): (u16, u16), marks: &[Mark]) -> String {
    let mut out = String::new();
    let mut x = 0;
    while x < buf.area.width {
        let cell = &buf[(x, y)];
        let w = cell.symbol().width().max(1) as u16;
        let ch = if (x0..x1).contains(&x) { marks.iter().find(|m| (m.hit)(cell)).map_or(' ', |m| m.ch) } else { ' ' };
        for _ in 0..w {
            out.push(ch);
        }
        x += w;
    }
    out.trim_end().to_string()
}

/// The snapshot of the frame last drawn on `term` (see the module docs).
/// Marks are looked for only inside `mark_area` when given.
pub(crate) fn snapshot(term: &Terminal<TestBackend>, marks: &[Mark], mark_area: Option<Rect>) -> String {
    let backend = term.backend();
    let buf = backend.buffer();
    let (w, h) = (buf.area.width, buf.area.height);
    let cursor = if backend.cursor_visible() {
        let p = backend.cursor_position();
        format!("({},{})", p.x, p.y)
    } else {
        "hidden".to_string()
    };
    let mut out = format!("{w}x{h} cursor={cursor}\n");
    for y in 0..h {
        out.push_str(format!("{y:02} {}", row_text(buf, y, w)).trim_end());
        out.push('\n');
    }
    if !marks.is_empty() {
        let legend: Vec<String> = marks.iter().map(|m| format!("{}={}", m.ch, m.name)).collect();
        out.push_str(&format!("-- marks: {}\n", legend.join(" ")));
        let area = mark_area.unwrap_or(buf.area).intersection(buf.area);
        for y in area.y..area.bottom() {
            let row = mark_row(buf, y, (area.x, area.right()), marks);
            if !row.is_empty() {
                out.push_str(&format!("{y:02} {row}\n"));
            }
        }
    }
    let tmp = std::env::temp_dir();
    assert!(!out.contains(tmp.to_string_lossy().as_ref()), "a snapshot must not show a temp path:\n{out}");
    out
}

mod tests {
    use ratatui::layout::Position;

    use super::*;

    fn draw(f: impl FnOnce(&mut ratatui::Frame)) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(12, 3)).unwrap();
        term.draw(f).unwrap();
        term
    }

    #[test]
    fn rows_cursor_and_marks() {
        let term = draw(|f| {
            let buf = f.buffer_mut();
            buf.set_string(0, 0, "ab世c  ", Style::default());
            buf[(1, 0)].set_style(Style::default().bg(Color::Blue));
            buf[(2, 0)].set_style(Style::default().bg(Color::Blue));
            buf[(4, 0)].set_style(Style::default().add_modifier(Modifier::REVERSED));
            buf.set_string(1, 2, "x", Style::default().fg(Color::Red).bg(Color::Green));
            f.set_cursor_position(Position::new(3, 2));
        });
        let marks = [
            Mark::modifier('C', "caret", Modifier::REVERSED),
            Mark::bg('S', "selection", Color::Blue),
            Mark::style('F', "fold", Style::default().fg(Color::Red).bg(Color::Green)),
        ];
        let s = snapshot(&term, &marks, None);
        assert_eq!(s, "12x3 cursor=(3,2)\n00 ab世c\n01\n02  x\n-- marks: C=caret S=selection F=fold\n00  SSSC\n02  F\n");
        // Clipped to the second column on: the wide glyph's mark repeats.
        let s = snapshot(&term, &marks[1..2], Some(Rect::new(2, 0, 10, 3)));
        assert!(s.ends_with("-- marks: S=selection\n00   SS\n"), "{s}");
        // A style mark needs every part it sets.
        let s = snapshot(&term, &[Mark::style('F', "fold", Style::default().fg(Color::Red).bg(Color::Blue))], None);
        assert!(s.ends_with("-- marks: F=fold\n"), "{s}");
    }

    #[test]
    fn hidden_cursor_and_no_marks() {
        let term = draw(|f| f.buffer_mut().set_string(0, 1, "hi", Style::default()));
        assert_eq!(snapshot(&term, &[], None), "12x3 cursor=hidden\n00\n01 hi\n02\n");
    }

    #[test]
    #[should_panic(expected = "temp path")]
    fn temp_paths_are_refused() {
        let tmp = std::env::temp_dir().join("x").to_string_lossy().into_owned();
        let mut term = Terminal::new(TestBackend::new(tmp.width() as u16, 1)).unwrap();
        term.draw(|f| f.buffer_mut().set_string(0, 0, &tmp, Style::default())).unwrap();
        snapshot(&term, &[], None);
    }
}
