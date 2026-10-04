//! What one view of a buffer owns: carets, selections, scroll, find bar.
//! `Buffer::view` holds the live view's state; other views of the same
//! buffer keep theirs parked ([`Parked`]) and catch up on the edits made
//! meanwhile through the buffer's [`EditLog`].

use std::collections::VecDeque;

use ropey::Rope;

use super::buffer::map_one;
use super::fold::Folds;
use super::rows::{Row, Rows};
use super::search::SearchState;
use super::selection::{Selection, Selections};
use super::wrap::Wrap;

#[derive(Debug, Default)]
pub struct ViewState {
    /// The carets and their selections (with their desired columns); most
    /// code reads the primary one through [`ViewState::sel`].
    pub sels: Selections,
    /// The top row: `scroll_line` and the wrap segment of it shown first.
    pub scroll_line: usize,
    pub scroll_seg: u16,
    pub scroll_col: usize,
    /// Caret, version and text area size when the view last followed the
    /// caret. The view only scrolls to the caret when one of them changes,
    /// so wheel scrolling is not undone on the next frame.
    pub follow_key: Option<(usize, u64, u16, u16)>,
    /// Expand-selection history: the selection before each step and the
    /// one the step produced. Shrink only pops while the current selection
    /// is still the produced one, so selection changes made elsewhere (mouse,
    /// undo) cannot restore a stale range. Cleared on every edit.
    pub expand_stack: Vec<(Selections, Selections)>,
    /// The selections Add Next Occurrence added, in order, for Unselect
    /// Occurrence. Cleared with the carets ([`ViewState::set_sel`]) and on
    /// every edit.
    pub occ_stack: Vec<Selection>,
    /// Add Next Occurrence started from a bare caret, so it matches whole
    /// words. Cleared with `occ_stack`.
    pub occ_whole_word: bool,
    /// The column block the carets were last made from, so extending it
    /// keeps its columns where short lines lost them. Only meant while the
    /// carets are still its selections (`Buffer::current_block`); cleared
    /// with the carets and on every edit.
    pub block: Option<Block>,
    pub search: Option<SearchState>,
    /// The collapsed folds; `scroll_line` is never a line they hide.
    pub folds: Folds,
    /// Soft wrap: on or off for this view, and its breaks.
    pub wrap: Wrap,
}

/// A column block: two corners, each a (line, visual column). The head is
/// the corner that moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl ViewState {
    /// A new view of the same buffer: the primary caret, its selection,
    /// the scroll, the folds (its own from then on) and whether it wraps,
    /// but no other carets, find bar or expand history.
    pub fn fork(&self) -> Self {
        let mut sels = self.sels.clone();
        sels.collapse_to_primary();
        Self {
            sels,
            scroll_line: self.scroll_line,
            scroll_seg: self.scroll_seg,
            scroll_col: self.scroll_col,
            folds: self.folds.clone(),
            wrap: Wrap::new(self.wrap.on),
            ..Default::default()
        }
    }

    /// The top row.
    pub fn top(&self) -> Row {
        Row { line: self.scroll_line, seg: self.scroll_seg }
    }

    pub fn set_top(&mut self, r: Row) {
        (self.scroll_line, self.scroll_seg) = (r.line, r.seg);
    }

    /// The primary caret and its selection.
    pub fn sel(&self) -> Selection {
        self.sels.primary()
    }

    /// Make `s` the only caret, with no desired column.
    pub fn set_sel(&mut self, s: Selection) {
        self.sels = s.into();
        self.block = None;
        self.occ_stack.clear();
        self.occ_whole_word = false;
    }
}

/// At most this many edits are kept between frames; a view parked longer
/// ago clamps its positions instead.
const LOG_CAP: usize = 4096;

/// One change of a buffer's text, in chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogEdit {
    /// `old_len` chars at `at` became `new_len` chars.
    Edit { at: usize, old_len: usize, new_len: usize },
    /// The whole text was replaced (a reload from disk): positions clamp.
    Reset,
}

/// The text changes of a buffer since the views were last caught up
/// (`Editor::sync_parked`, at the start of every frame).
#[derive(Debug, Default)]
pub struct EditLog {
    /// Sequence number of `entries[0]`.
    base: u64,
    entries: VecDeque<LogEdit>,
}

impl EditLog {
    pub fn push(&mut self, e: LogEdit) {
        if self.entries.len() == LOG_CAP {
            self.entries.pop_front();
            self.base += 1;
        }
        self.entries.push_back(e);
    }

    /// The sequence number of the next edit.
    pub fn end(&self) -> u64 {
        self.base + self.entries.len() as u64
    }

    /// Forget the edits; views parked from now on start at `end`.
    pub fn clear(&mut self) {
        self.base = self.end();
        self.entries.clear();
    }

    /// The edits from `seq` on, or `None` when some of them were dropped.
    fn since(&self, seq: u64) -> Option<impl Iterator<Item = &LogEdit>> {
        (seq >= self.base).then(|| self.entries.iter().skip((seq - self.base) as usize))
    }
}

/// The state of a view that is not the live one.
#[derive(Debug)]
pub struct Parked {
    pub state: ViewState,
    /// `EditLog::end` when parked or last caught up.
    seq: u64,
    /// `Buffer::version` when parked or last caught up.
    version: u64,
    /// The first char of the top line, mapped like the caret so the view
    /// keeps showing the same text.
    top: usize,
}

impl Parked {
    pub fn park(state: ViewState, rope: &Rope, log: &EditLog, version: u64) -> Self {
        let top = rope.line_to_char(state.scroll_line.min(rope.len_lines() - 1));
        Self { state, seq: log.end(), version, top }
    }

    /// Map the carets, selections, folds, top line and find bar origin
    /// through the edits since the last call; a reload or a dropped edit
    /// clamps them instead (and drops the folds). Carets that meet merge. The expand and occurrence history
    /// is dropped. A view that was following its primary caret keeps
    /// following it without scrolling.
    pub fn catch_up(&mut self, rope: &Rope, log: &EditLog, version: u64) {
        if self.version == version {
            self.seq = log.end();
            return;
        }
        let s = &mut self.state;
        let following = s.follow_key.is_some_and(|k| (k.0, k.1) == (s.sel().head, self.version));
        let origin = s.search.as_ref().map_or(0, |q| q.origin);
        let edits = log.since(self.seq).map(|it| it.copied().collect::<Vec<_>>()).filter(|v| !v.contains(&LogEdit::Reset));
        match &edits {
            Some(edits) => {
                for e in edits {
                    if let LogEdit::Edit { at, old_len, new_len } = *e {
                        s.folds.map_edit(at, old_len, new_len);
                    }
                }
            }
            None => s.folds.clear(),
        }
        s.folds.rebuild(rope);
        let edits = edits.unwrap_or_default();
        let len = rope.len_chars();
        let map = |p: usize| {
            let mapped = edits.iter().fold(p, |p, e| match *e {
                LogEdit::Edit { at, old_len, new_len } => map_one(p, at, old_len, new_len, false),
                LogEdit::Reset => p,
            });
            mapped.min(len)
        };
        let (top, origin) = (map(self.top), map(origin));
        s.sels.map(map);
        let head = s.sel().head;
        s.scroll_line = Rows::new(&s.folds, rope.len_lines()).visible(rope.char_to_line(top));
        self.top = rope.line_to_char(s.scroll_line);
        if let Some(q) = &mut s.search {
            q.origin = origin;
        }
        s.expand_stack.clear();
        s.occ_stack.clear();
        s.occ_whole_word = false;
        s.block = None;
        if following && let Some(k) = &mut s.follow_key {
            (k.0, k.1) = (head, version);
        }
        self.seq = log.end();
        self.version = version;
    }

    /// The state, caught up, to become the live one.
    pub fn unpark(mut self, rope: &Rope, log: &EditLog, version: u64) -> ViewState {
        self.catch_up(rope, log, version);
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::Buffer;
    use crate::editor::undo::{Change, TxKind};

    /// A buffer of `text` and a second view of it with the caret at `caret`.
    fn parked(text: &str, caret: usize) -> (Buffer, Parked) {
        let b = Buffer::from_text(1, None, text, false);
        let state = ViewState { sels: Selections::point(caret), ..Default::default() };
        let p = Parked::park(state, &b.rope, &b.edit_log, b.version);
        (b, p)
    }

    fn caught_up(b: &Buffer, p: &mut Parked) -> Selection {
        p.catch_up(&b.rope, &b.edit_log, b.version);
        p.state.sel()
    }

    #[test]
    fn caret_follows_inserts_before_at_and_across() {
        let (mut b, mut p) = parked("hello world", 6);
        b.view.set_sel(Selection::point(0));
        b.insert("> ", TxKind::Other);
        assert_eq!(caught_up(&b, &mut p).head, 8, "insert before shifts");
        b.view.set_sel(Selection::point(8));
        b.insert("big ", TxKind::Other);
        assert_eq!(caught_up(&b, &mut p).head, 12, "insert at the caret pushes it");
        b.view.set_sel(Selection::new(14, 16));
        b.delete_selection();
        assert_eq!(caught_up(&b, &mut p).head, 12, "a later edit leaves it");
        // A deletion across the caret moves it to the start.
        b.view.set_sel(Selection::new(6, 14));
        b.delete_selection();
        assert_eq!(caught_up(&b, &mut p).head, 6);
        assert_eq!(b.text(), "> helld");
    }

    #[test]
    fn selection_undo_and_redo_map_too() {
        let (mut b, mut p) = parked("one two three", 0);
        p.state.set_sel(Selection::new(4, 7));
        b.view.set_sel(Selection::point(0));
        b.insert("zero ", TxKind::Other);
        assert_eq!(caught_up(&b, &mut p), Selection::new(9, 12));
        assert!(b.undo());
        assert_eq!(caught_up(&b, &mut p), Selection::new(4, 7));
        assert!(b.redo());
        assert_eq!(caught_up(&b, &mut p), Selection::new(9, 12));
        // Several edits between catch-ups map in order.
        b.view.set_sel(Selection::point(b.len_chars()));
        b.insert(" four", TxKind::Other);
        b.view.set_sel(Selection::point(0));
        b.insert("[", TxKind::Other);
        assert_eq!(caught_up(&b, &mut p), Selection::new(10, 13));
    }

    #[test]
    fn reload_clamps_and_formatting_maps() {
        let (mut b, mut p) = parked("a long line of text\nsecond\n", 15);
        let old = b.text();
        b.apply(vec![Change::replace(0, old, "short\n")], Selection::point(0), TxKind::Other);
        assert_eq!(caught_up(&b, &mut p).head, 6, "a whole-text replacement clamps");
        let (mut b, mut p) = parked("fn f(){\nlet x=1;\n}\n", 12);
        assert!(b.apply_formatted("fn f(){\n    let x = 1;\n}\n"));
        assert_eq!(caught_up(&b, &mut p).head, 8, "inside a replaced hunk: its start");
        let (mut b, mut p) = parked("a\nb\nc=1\n", 4);
        assert!(b.apply_formatted("a\nb\nc = 1\n"));
        assert_eq!(caught_up(&b, &mut p).head, 4, "before the hunk: unchanged");
    }

    #[test]
    fn parked_carets_map_through_a_multi_caret_edit_and_merge() {
        let (mut b, mut p) = parked("aa bb cc dd", 0);
        p.state.sels = Selections::new(vec![Selection::point(3), Selection::point(9)], 1);
        b.view.sels = Selections::new(vec![Selection::point(0), Selection::point(6), Selection::point(11)], 0);
        b.insert("x", TxKind::Typing);
        assert_eq!(b.text(), "xaa bb xcc ddx");
        p.catch_up(&b.rope, &b.edit_log, b.version);
        assert_eq!(p.state.sels.heads().collect::<Vec<_>>(), [4, 11]);
        assert_eq!(p.state.sels.primary(), Selection::point(11));
        // Deleting the text between them merges them.
        b.view.set_sel(Selection::new(2, 12));
        b.delete_selection();
        p.catch_up(&b.rope, &b.edit_log, b.version);
        assert_eq!(p.state.sels.as_slice(), [Selection::point(2)]);
    }

    #[test]
    fn dropped_edits_clamp() {
        let (mut b, mut p) = parked("xyz", 3);
        for _ in 0..LOG_CAP + 1 {
            b.view.set_sel(Selection::point(0));
            b.insert("a", TxKind::Other);
        }
        // Mapping would give 3 + 4097; the log lost the first edit.
        assert_eq!(caught_up(&b, &mut p).head, 3);
        assert_eq!(b.edit_log.end(), LOG_CAP as u64 + 1);
    }

    #[test]
    fn scroll_top_maps_follow_key_retargets_expand_stack_clears() {
        let text: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let (mut b, mut p) = parked(&text, b_pos(&text, 30));
        p.state.scroll_line = 20;
        p.state.follow_key = Some((p.state.sel().head, b.version, 80, 10));
        p.state.expand_stack.push((Selections::point(0), Selection::new(0, 4).into()));
        p = Parked::park(std::mem::take(&mut p.state), &b.rope, &b.edit_log, b.version);
        b.view.set_sel(Selection::point(0));
        b.insert("new 1\nnew 2\n", TxKind::Other);
        let head = caught_up(&b, &mut p).head;
        assert_eq!(b.rope.char_to_line(head), 32);
        assert_eq!(p.state.scroll_line, 22, "the same text stays at the top");
        assert_eq!(p.state.follow_key, Some((head, b.version, 80, 10)), "still following, without a scroll");
        assert!(p.state.expand_stack.is_empty());
        // A view whose caret moved since it last followed it is left to
        // scroll to it when shown.
        p.state.set_sel(Selection::point(0));
        b.view.set_sel(Selection::point(0));
        b.insert("x", TxKind::Other);
        caught_up(&b, &mut p);
        assert_ne!(p.state.follow_key.map(|k| k.1), Some(b.version));
        let state = p.unpark(&b.rope, &b.edit_log, b.version);
        assert_eq!(state.scroll_line, 22);
    }

    fn b_pos(text: &str, line: usize) -> usize {
        text.lines().take(line).map(|l| l.chars().count() + 1).sum()
    }

    #[test]
    fn fork_keeps_caret_and_scroll_only() {
        let mut v = ViewState { sels: Selection::new(1, 3).into(), scroll_line: 7, scroll_col: 2, ..Default::default() };
        v.sels.push(Selection::point(9), false);
        v.search = Some(SearchState::default());
        v.expand_stack.push((Selections::point(0), Selections::point(1)));
        v.follow_key = Some((3, 0, 1, 1));
        let f = v.fork();
        assert_eq!((f.sels, f.scroll_line, f.scroll_col), (Selection::new(1, 3).into(), 7, 2));
        assert!(f.search.is_none() && f.expand_stack.is_empty() && f.follow_key.is_none());
    }

    #[test]
    fn parked_folds_map_through_edits_fork_copies_and_reload_drops() {
        let text = "fn a() {\n    1;\n    2;\n}\nfn b() {}\n";
        let (mut b, mut p) = parked(text, 0);
        let fold = crate::editor::fold::Fold { start: 7, end: 25 };
        assert!(p.state.folds.collapse(fold, &b.rope));
        p.state.scroll_line = 2;
        p = Parked::park(std::mem::take(&mut p.state), &b.rope, &b.edit_log, b.version);
        let hidden = |p: &Parked| p.state.folds.hidden().iter().map(|r| (r.start, r.end)).collect::<Vec<_>>();
        assert_eq!(hidden(&p), [(1, 4)]);
        // The live view's folds are its own.
        assert!(b.view.folds.is_empty());
        b.view.set_sel(Selection::point(0));
        b.insert("// x\n", TxKind::Other);
        caught_up(&b, &mut p);
        assert_eq!(hidden(&p), [(2, 5)], "an edit above shifts the fold");
        assert_eq!(p.state.scroll_line, 1, "a hidden top line snaps to its header");
        let f = p.state.fork();
        assert_eq!(f.folds.hidden(), p.state.folds.hidden(), "a fork copies the folds");
        let old = b.text();
        b.apply(vec![Change::replace(0, old, "x\n")], Selection::point(0), TxKind::Other);
        caught_up(&b, &mut p);
        assert!(p.state.folds.is_empty(), "a whole-text replacement drops them");
    }
}
