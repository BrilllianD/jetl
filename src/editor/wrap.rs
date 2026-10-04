//! Soft wrap: where a line too long for its view breaks into screen rows.
//! The breaks come from the same [`layout_line`] walk that draws the text,
//! so rows, rendering and mouse mapping agree. Each view owns a [`Wrap`]
//! (its toggle and a cache of breaks for its width); [`super::rows::Rows`]
//! is what turns those breaks into rows.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ropey::Rope;

use super::selection::line_len_no_eol;
use super::view::layout_line;

/// Lines longer than this (in chars) are not wrapped.
pub const MAX_WRAP_CHARS: usize = 100_000;

/// The cache is dropped when it holds more lines than this.
const CACHE_LINES: usize = 50_000;

/// How one line wraps.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineWrap {
    /// Where each row after the first starts: its first char (counted from
    /// the line start) and that char's visual column in the unwrapped line.
    pub starts: Vec<(u32, u32)>,
    /// Cells the rows after the first are shifted right by: the line's
    /// leading indent, at most half the view.
    pub indent: u16,
}

impl LineWrap {
    /// Rows the line takes.
    pub fn segs(&self) -> u16 {
        self.starts.len() as u16 + 1
    }

    /// First char (from the line start) and its visual column of segment
    /// `seg`.
    pub fn start(&self, seg: u16) -> (usize, usize) {
        match seg.checked_sub(1).and_then(|k| self.starts.get(k as usize)) {
            Some(&(c, col)) => (c as usize, col as usize),
            None => (0, 0),
        }
    }

    /// The segment holding char offset `off` (from the line start); the
    /// line end is on the last one.
    pub fn seg_of(&self, off: usize) -> u16 {
        self.starts.partition_point(|&(c, _)| c as usize <= off) as u16
    }

    /// Cells segment `seg` is shifted right by.
    pub fn indent_of(&self, seg: u16) -> usize {
        if seg == 0 { 0 } else { self.indent as usize }
    }
}

/// Break `line` into rows for a text area `text_w` cells wide. The last
/// column stays free for the caret or selection cell at a row's end.
/// Breaks go before the first grapheme after a run of whitespace (the
/// whitespace hangs at the row's end); a word longer than the row breaks
/// before the grapheme that does not fit. Graphemes are never split, and
/// one wider than the row still gets a row of its own.
pub fn wrap_line(rope: &Rope, line: usize, tab_width: usize, text_w: u16) -> LineWrap {
    if text_w < 2 || line >= rope.len_lines() || line_len_no_eol(rope, line) > MAX_WRAP_CHARS {
        return LineWrap::default();
    }
    let avail = text_w as usize - 1;
    let line_start = rope.line_to_char(line);
    let mut out = LineWrap::default();
    // The leading indent: the column of the first non-blank grapheme.
    let mut lead = None;
    layout_line(rope, line, tab_width, MAX_WRAP_CHARS, &mut |g, c| {
        if !is_blank(g) {
            lead = Some(c.col);
            return false;
        }
        true
    });
    let indent = lead.unwrap_or(0).min(avail / 2);
    out.indent = indent as u16;
    let cont = (avail - indent).max(1);
    // The current segment: first char, its column and its width.
    let (mut seg_start, mut seg_col, mut seg_w) = (line_start, 0usize, avail);
    // The last break candidate in the current segment.
    // (The leading indent is no break: it would leave a blank first row.)
    let mut cand: Option<(usize, usize)> = None;
    let (mut prev_blank, mut text_seen) = (false, false);
    layout_line(rope, line, tab_width, MAX_WRAP_CHARS, &mut |g, c| {
        let blank = is_blank(g);
        if !blank && prev_blank && text_seen && c.char_idx > seg_start {
            cand = Some((c.char_idx, c.col));
        }
        prev_blank = blank;
        text_seen |= !blank;
        // Whitespace never breaks: it hangs past the row's end.
        while !blank && c.char_idx > seg_start && c.col + c.width - seg_col > seg_w {
            let (at, col) = cand.filter(|&(at, _)| at > seg_start).unwrap_or((c.char_idx, c.col));
            out.starts.push(((at - line_start) as u32, col as u32));
            (seg_start, seg_col, seg_w) = (at, col, cont);
            cand = None;
        }
        true
    });
    out
}

/// A tab (drawn from an empty string) or whitespace.
fn is_blank(g: &str) -> bool {
    g.chars().all(char::is_whitespace)
}

/// A view's soft wrap: whether it is on, the geometry it breaks for (set
/// by each render) and the breaks found so far for that geometry and
/// buffer version.
#[derive(Debug, Default)]
pub struct Wrap {
    pub on: bool,
    width: u16,
    tab: usize,
    cache: RefCell<WrapCache>,
}

#[derive(Debug, Default)]
struct WrapCache {
    /// Buffer version, width and tab width the lines were broken for.
    key: (u64, u16, usize),
    lines: HashMap<usize, Rc<LineWrap>>,
}

impl Wrap {
    pub fn new(on: bool) -> Self {
        Self { on, ..Default::default() }
    }

    /// The text area width and tab width to break lines for.
    pub fn set_geometry(&mut self, width: u16, tab: usize) {
        (self.width, self.tab) = (width, tab.max(1));
    }

    /// On, and drawn at least once at a usable width.
    pub fn active(&self) -> bool {
        self.on && self.width >= 2
    }

    pub fn tab(&self) -> usize {
        self.tab
    }

    /// How `line` of `rope` (at buffer `version`) wraps.
    pub fn line(&self, rope: &Rope, version: u64, line: usize) -> Rc<LineWrap> {
        let key = (version, self.width, self.tab);
        let mut c = self.cache.borrow_mut();
        if c.key != key || c.lines.len() > CACHE_LINES {
            c.lines.clear();
            c.key = key;
        }
        c.lines.entry(line).or_insert_with(|| Rc::new(wrap_line(rope, line, self.tab, self.width))).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows of the first line of `text` at `w` cells, as strings.
    fn rows(text: &str, w: u16) -> Vec<String> {
        let rope = Rope::from_str(text);
        let lw = wrap_line(&rope, 0, 4, w);
        let line: Vec<char> = rope.line(0).chars().filter(|c| *c != '\n').collect();
        let mut bounds: Vec<usize> = vec![0];
        bounds.extend(lw.starts.iter().map(|&(c, _)| c as usize));
        bounds.push(line.len());
        bounds.windows(2).map(|b| line[b[0]..b[1]].iter().collect()).collect()
    }

    #[test]
    fn words_break_after_whitespace_which_hangs() {
        // 10 cells: 9 for text, the last kept free.
        assert_eq!(rows("the quick brown fox jumps", 10), ["the quick ", "brown fox ", "jumps"]);
        assert_eq!(rows("short", 10), ["short"]);
        // Spaces never break, however many.
        assert_eq!(rows("ab          cd", 6), ["ab          ", "cd"]);
    }

    #[test]
    fn long_words_hard_break() {
        assert_eq!(rows("abcdefghijklmnopqrstuvwxyz", 10), ["abcdefghi", "jklmnopqr", "stuvwxyz"]);
        assert_eq!(rows("ab cdefghijklmnop", 6), ["ab ", "cdefg", "hijkl", "mnop"]);
    }

    #[test]
    fn tabs_keep_stops_from_the_line_start() {
        // a(0) tab(1-3) b(4) tab(5-7) c(8): c is past 8 cells of text.
        assert_eq!(rows("a\tb\tc\td", 9), ["a\tb\t", "c\td"]);
        let rope = Rope::from_str("a\tb\tc\td");
        let lw = wrap_line(&rope, 0, 4, 9);
        assert_eq!(lw.starts, [(4, 8)], "the column is the unwrapped one");
    }

    #[test]
    fn wide_graphemes_stay_whole() {
        // 日本語 are two cells each: 5 cells of text fit two of them.
        assert_eq!(rows("日本語日本", 6), ["日本", "語日", "本"]);
        // A ZWJ family emoji is one grapheme.
        let fam = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(rows(&format!("ab {fam} c"), 5), ["ab ", &format!("{fam} c")]);
        assert_eq!(rows(&format!("abc{fam}"), 4), ["abc", fam]);
    }

    #[test]
    fn continuation_rows_take_the_indent_capped_at_half() {
        let rope = Rope::from_str("    aaa bbb ccc ddd");
        let lw = wrap_line(&rope, 0, 4, 13);
        assert_eq!(lw.indent, 4);
        // 12 cells: the first row fits "    aaa bbb ", the rest 8 each.
        assert_eq!(rows("    aaa bbb ccc ddd", 13), ["    aaa bbb ", "ccc ddd"]);
        assert_eq!(rows("    aaaaaaaaaaaaaaaaaaaa", 13), ["    aaaaaaaa", "aaaaaaaa", "aaaa"]);
        let deep = format!("{}x y", " ".repeat(30));
        let lw = wrap_line(&Rope::from_str(&deep), 0, 4, 11);
        assert_eq!(lw.indent, 5, "half of 10");
    }

    #[test]
    fn tiny_widths_still_make_progress() {
        assert_eq!(rows("abc", 2), ["a", "b", "c"]);
        assert_eq!(rows("日本", 2), ["日", "本"], "too wide still gets a row");
        assert_eq!(rows("a b", 3), ["a ", "b"]);
        assert_eq!(rows("abc", 1), ["abc"], "no room: no wrap");
        assert_eq!(rows("", 10), [""]);
    }

    #[test]
    fn segments_and_cache() {
        let rope = Rope::from_str("aaa bbb ccc\nabc");
        let lw = wrap_line(&rope, 0, 4, 5);
        assert_eq!((lw.segs(), lw.start(1), lw.start(2)), (3, (4, 4), (8, 8)));
        assert_eq!((lw.seg_of(0), lw.seg_of(3), lw.seg_of(4), lw.seg_of(11)), (0, 0, 1, 2));
        let mut w = Wrap::new(true);
        assert!(!w.active(), "not drawn yet");
        w.set_geometry(5, 4);
        assert!(w.active());
        assert_eq!(w.line(&rope, 0, 0).segs(), 3);
        assert_eq!(w.line(&rope, 0, 1).segs(), 1);
        w.set_geometry(40, 4);
        assert_eq!(w.line(&rope, 0, 0).segs(), 1, "a new width breaks again");
    }
}

/// Editor-level tests: rendering, scrolling, mouse and motions with wrap on.
#[cfg(all(test, feature = "lang-rust"))]
mod editor_tests {
    use std::path::PathBuf;

    use ratatui::layout::Rect;

    use crate::config::Config;
    use crate::config::theme::Theme;
    use crate::editor::Editor;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

    use crate::actions::{EditorAction as E, Motion};
    use crate::editor::fold::tests::editor::{act, collapse, draw, mouse};
    use crate::editor::rows::Row;
    use crate::editor::selection::Selection;

    /// A plain text file with wrap on.
    pub(super) fn wrapped(text: &str) -> Editor {
        let mut ed = Editor::new(&Config::default());
        ed.open_text(PathBuf::from("/tmp/a.txt"), text, false, &Theme::default());
        ed.active_buffer_mut().unwrap().view.wrap.on = true;
        ed
    }

    /// Draw like `draw`, keeping the cells.
    fn screen(ed: &mut Editor, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let theme = Theme::default();
        term.draw(|f| ed.render(f, Rect::new(0, 0, w, 1), Rect::new(0, 1, w, h - 1), true, &theme)).unwrap().buffer.clone()
    }

    fn trimmed(rows: &[String]) -> Vec<&str> {
        rows.iter().map(|r| r.trim_end()).collect()
    }

    /// 100 words `w00`..`w99`, then a short line: at 20 columns (13 cells
    /// of text) three words fit a row, so the first line takes 34 rows.
    pub(super) fn words() -> String {
        let w: Vec<String> = (0..100).map(|i| format!("w{i:02}")).collect();
        format!("{}\nend\n", w.join(" "))
    }

    fn top(ed: &Editor) -> Row {
        ed.active_buffer().unwrap().view.top()
    }

    #[test]
    fn continuation_rows_get_a_wrap_mark_and_the_indent() {
        let mut ed = wrapped("    aaaa bbbb cccc dddd\nnext\n");
        let rows = draw(&mut ed, 20, 7);
        assert_eq!(
            trimmed(&rows[1..7]),
            ["  1        aaaa", "     ↪     bbbb", "     ↪     cccc", "     ↪     dddd", "  2    next", "  3"]
        );
        // Off: one row, cut at the edge.
        ed.active_buffer_mut().unwrap().view.wrap.on = false;
        let rows = draw(&mut ed, 20, 7);
        assert_eq!(trimmed(&rows[1..3]), ["  1        aaaa bbbb", "  2    next"]);
    }

    #[test]
    fn line_end_cells_and_placeholder_go_on_the_last_row() {
        let mut ed = wrapped("    aaaa bbbb cccc dddd\nnext\n");
        let b = ed.active_buffer_mut().unwrap();
        b.view.set_sel(Selection::new(0, 24));
        let s = screen(&mut ed, 20, 9);
        let sel = Theme::default().selection_bg;
        // Row 4 ends "dddd" at x 7 + 4 + 4; the line break cell follows.
        assert_eq!(s[(15, 4)].bg, sel, "the line break is selected on the last row");
        assert_eq!(s[(15, 2)].bg, sel, "the hanging space is text");
        assert_ne!(s[(16, 2)].bg, sel, "nothing past it on the other rows");
        // A folded header that wraps shows its placeholder after its last row.
        let mut ed = wrapped("aaaa bbbb cccc\n  x\n  y\nz\n");
        collapse(&mut ed, 1);
        let rows = draw(&mut ed, 20, 7);
        assert_eq!(trimmed(&rows[1..4]), ["  1  ▸ aaaa bbbb", "     ↪ cccc…", "  4    z"]);
    }

    #[test]
    fn the_caret_scrolls_into_view_by_rows() {
        let mut ed = wrapped(&words());
        // w15 starts row 5 of line 0.
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(60));
        draw(&mut ed, 20, 6);
        // 5 rows of text keep 2 rows of context: the caret is on the third.
        assert_eq!(top(&ed), Row { line: 0, seg: 3 });
        let ta = ed.text_area();
        assert_eq!(ed.focused_group().caret_cell, Some((ta.x, ta.y + 2)));
        // Far away and back: the walk is bounded by the view.
        ed.active_buffer_mut().unwrap().goto_line(1, 2);
        draw(&mut ed, 20, 6);
        assert_eq!(top(&ed), Row { line: 0, seg: 32 });
        assert_eq!(ed.focused_group().caret_cell, Some((ta.x + 2, ta.y + 2)));
        ed.active_buffer_mut().unwrap().goto_line(0, 0);
        draw(&mut ed, 20, 6);
        assert_eq!(top(&ed), Row { line: 0, seg: 0 });
        assert_eq!(ed.active_buffer().unwrap().view.scroll_col, 0);
    }

    fn head(ed: &Editor) -> usize {
        ed.active_buffer().unwrap().view.sel().head
    }

    fn click(ed: &mut Editor, x: u16, y: u16) {
        mouse(ed, MouseEventKind::Down(MouseButton::Left), (x, y), KeyModifiers::NONE);
        mouse(ed, MouseEventKind::Up(MouseButton::Left), (x, y), KeyModifiers::NONE);
    }

    #[test]
    fn clicks_round_trip_on_every_cell() {
        let text =
            "Lorem ipsum\tdolor 日本語 sit amet, consectetur_adipiscing_elit_sed_do eiusmod 😀 tempor  incididunt\n  ut labore et dolore\n";
        for tw in (10..=40u16).step_by(3) {
            let (w, h) = (tw + 7, 30);
            let mut ed = wrapped(text);
            draw(&mut ed, w, h);
            let ta = ed.text_area();
            assert_eq!(ta.width, tw);
            let used = ed.active_buffer().unwrap().rows().iter_from(Row::line(0)).count() as u16;
            assert!(used + 4 <= ta.height, "width {tw}: {used} rows do not fit without scrolling");
            for y in ta.y..ta.y + used {
                for x in ta.x - 1..ta.right() {
                    click(&mut ed, x, y);
                    draw(&mut ed, w, h);
                    let p = head(&ed);
                    let (cx, cy) = ed.focused_group().caret_cell.unwrap();
                    assert_eq!(cy, y, "width {tw}, cell ({x}, {y}): caret {p} drawn on another row");
                    click(&mut ed, cx, cy);
                    assert_eq!(head(&ed), p, "width {tw}, cell ({x}, {y}): clicking the caret's cell moves it");
                }
            }
        }
    }

    #[test]
    fn gutter_and_placeholder_clicks_need_the_right_row() {
        let mut ed = wrapped("aaaa bbbb cccc\n  x\n  y\nz\n");
        collapse(&mut ed, 1);
        draw(&mut ed, 20, 7);
        let ta = ed.text_area();
        // The fold marker column of a continuation row does nothing.
        click(&mut ed, ta.x - 2, ta.y + 1);
        assert!(!ed.active_buffer().unwrap().view.folds.is_empty());
        // Past the text of the header's first row: a caret, still folded.
        click(&mut ed, ta.x + 11, ta.y);
        assert_eq!(head(&ed), 9, "the start of the row's last grapheme");
        assert!(!ed.active_buffer().unwrap().view.folds.is_empty());
        // On the placeholder after the last row: expanded.
        click(&mut ed, ta.x + 5, ta.y + 1);
        assert!(ed.active_buffer().unwrap().view.folds.is_empty());
        assert_eq!(head(&ed), 14);
        // A gutter click picks the row's first char.
        click(&mut ed, ta.x - 1, ta.y + 1);
        assert_eq!(head(&ed), 10);
    }

    /// At 13 cells of text: "alpha beta " (0..11), "gamma delta "
    /// (11..23), "x epsilon " (23..33), "zeta" (33..37); then "next".
    const ROWS: &str = "alpha beta gamma delta x epsilon zeta\nnext\n";

    fn moved(ed: &mut Editor, m: Motion) -> usize {
        act(ed, E::Move(m, false));
        head(ed)
    }

    #[test]
    fn up_and_down_keep_the_row_x_across_short_rows() {
        let mut ed = wrapped(ROWS);
        draw(&mut ed, 20, 10);
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(21));
        assert_eq!(moved(&mut ed, Motion::Down), 32, "past a row's text: its last grapheme");
        assert_eq!(moved(&mut ed, Motion::Down), 37, "past the last row's text: the line end");
        assert_eq!(moved(&mut ed, Motion::Down), 42, "the next line, at its end");
        assert_eq!(moved(&mut ed, Motion::Up), 37);
        assert_eq!(moved(&mut ed, Motion::Up), 32);
        assert_eq!(moved(&mut ed, Motion::Up), 21, "back at row x 10");
        assert_eq!(moved(&mut ed, Motion::Up), 10);
        assert_eq!(moved(&mut ed, Motion::Up), 0, "above the first row: the text start");
        // Clone Caret Below goes by rows too.
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(13));
        act(&mut ed, E::CloneCaretBelow);
        act(&mut ed, E::CloneCaretBelow);
        assert_eq!(ed.active_buffer().unwrap().view.sels.heads().collect::<Vec<_>>(), [13, 25, 35]);
    }

    #[test]
    fn home_and_end_stop_at_the_row_first() {
        let mut ed = wrapped(ROWS);
        draw(&mut ed, 20, 10);
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(19));
        assert_eq!(moved(&mut ed, Motion::LineStart), 11, "the row's start");
        assert_eq!(moved(&mut ed, Motion::LineStart), 0, "then smart Home on the line");
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(19));
        assert_eq!(moved(&mut ed, Motion::LineEnd), 22, "the row's last grapheme");
        assert_eq!(moved(&mut ed, Motion::LineEnd), 37, "then the line end");
        assert_eq!(moved(&mut ed, Motion::LineEnd), 37);
        // Off: plain Home and End.
        ed.active_buffer_mut().unwrap().view.wrap.on = false;
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(19));
        assert_eq!((moved(&mut ed, Motion::LineEnd), moved(&mut ed, Motion::LineStart)), (37, 0));
    }

    fn notes(effects: &[crate::event::Effect]) -> Vec<&str> {
        effects.iter().filter_map(|e| if let crate::event::Effect::Notify(_, t) = e { Some(t.as_str()) } else { None }).collect()
    }

    #[test]
    fn files_open_wrapped_by_mask_and_config() {
        let ed = Editor::new(&Config::default());
        let on = |p: &str| ed.soft_wrap_for(Some(std::path::Path::new(p)));
        assert!(on("/x/README.md") && on("notes.txt") && on("a.rst") && on("b.adoc"));
        assert!(!on("main.rs") && !on("md") && !on(".md.bak") && !ed.soft_wrap_for(None));
        let mut config = Config::default();
        config.editor.soft_wrap_files = vec!["LICENSE".into(), "*.log".into()];
        let ed = Editor::new(&config);
        let on = |p: &str| ed.soft_wrap_for(Some(std::path::Path::new(p)));
        assert!(on("/x/LICENSE") && on("a.log") && !on("a.md") && !on("LICENSE.txt"));
        config.editor.soft_wrap = true;
        let mut ed = Editor::new(&config);
        assert!(ed.soft_wrap_for(Some(std::path::Path::new("main.rs"))) && ed.soft_wrap_for(None));
        ed.open_text(PathBuf::from("/tmp/main.rs"), "x\n", false, &Theme::default());
        assert!(ed.active_buffer().unwrap().view.wrap.on);
    }

    #[test]
    fn toggling_keeps_the_caret_in_view_and_turns_column_mode_off() {
        let mut ed = Editor::new(&Config::default());
        ed.open_text(PathBuf::from("/tmp/a.rs"), &words(), false, &Theme::default());
        assert!(!ed.active_buffer().unwrap().view.wrap.on, "code does not wrap by default");
        // w75: far right without wrap.
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(300));
        draw(&mut ed, 20, 6);
        assert!(ed.active_buffer().unwrap().view.scroll_col > 0);
        act(&mut ed, E::ToggleSoftWrap);
        draw(&mut ed, 20, 6);
        let b = ed.active_buffer().unwrap();
        assert_eq!((b.view.scroll_col, b.view.top()), (0, Row { line: 0, seg: 23 }), "row 25 holds the caret");
        let ta = ed.text_area();
        assert_eq!(ed.focused_group().caret_cell, Some((ta.x, ta.y + 2)));
        // Column selection is refused while wrapped...
        let fx = act(&mut ed, E::ToggleColumnMode);
        assert_eq!(notes(&fx), ["Column selection does not work with soft wrap"]);
        assert!(!ed.column_mode);
        // ...and turned off by wrapping.
        act(&mut ed, E::ToggleSoftWrap);
        act(&mut ed, E::ToggleColumnMode);
        assert!(ed.column_mode);
        let fx = act(&mut ed, E::ToggleSoftWrap);
        assert_eq!(notes(&fx), ["Column selection mode is off: it does not work with soft wrap"]);
        assert!(!ed.column_mode);
        // Alt+drag makes no block either: only the Alt+click's caret.
        let ta = ed.text_area();
        mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (ta.x, ta.y), KeyModifiers::ALT);
        mouse(&mut ed, MouseEventKind::Drag(MouseButton::Left), (ta.x + 3, ta.y + 2), KeyModifiers::ALT);
        assert_eq!(ed.active_buffer().unwrap().view.sels.len(), 2);
    }

    #[test]
    fn split_views_break_at_their_own_width() {
        let mut ed = wrapped(&words());
        act(&mut ed, E::SplitRight);
        act(&mut ed, E::SplitRight);
        let rows = draw(&mut ed, 80, 20);
        let marks = |ed: &Editor, g: usize| {
            let ta = ed.groups[g].text_area;
            (ta.y..ta.bottom()).filter(|&y| rows[y as usize].chars().nth((ta.x - 2) as usize) == Some('↪')).count()
        };
        let (wide, narrow) = (ed.groups[0].text_area.width, ed.groups[1].text_area.width);
        assert!(wide > narrow + 5, "{wide} {narrow}");
        // The narrow view needs more rows for the first line: it fills all
        // 19 with it, the wide one shows the next line too.
        assert_eq!(marks(&ed, 1), 18);
        assert!(marks(&ed, 0) < 18);
        assert!(ed.groups.iter().all(|g| g.views.iter().all(|v| v.parked.as_ref().is_none_or(|p| p.state.wrap.on))));
    }
}
