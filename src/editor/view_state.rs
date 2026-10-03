//! What one view of a buffer owns: caret, selection, scroll, find bar.
//! `Buffer::view` holds the live view's state; other views of the same
//! buffer keep theirs parked ([`Parked`]) and catch up on the edits made
//! meanwhile through the buffer's [`EditLog`].

use std::collections::VecDeque;

use ropey::Rope;

use super::buffer::map_one;
use super::search::SearchState;
use super::selection::Selection;

#[derive(Debug, Default)]
pub struct ViewState {
    pub sel: Selection,
    pub desired_col: Option<usize>,
    pub scroll_line: usize,
    pub scroll_col: usize,
    /// Caret, version and text area size when the view last followed the
    /// caret. The view only scrolls to the caret when one of them changes,
    /// so wheel scrolling is not undone on the next frame.
    pub follow_key: Option<(usize, u64, u16, u16)>,
    /// Expand-selection history: the selection before each step and the
    /// one the step produced. Shrink only pops while the current selection
    /// is still the produced one, so selection changes made elsewhere (mouse,
    /// undo) cannot restore a stale range. Cleared on every edit.
    pub expand_stack: Vec<(Selection, Selection)>,
    pub search: Option<SearchState>,
}

impl ViewState {
    /// A new view of the same buffer: the caret, selection and scroll, but
    /// no find bar or expand history.
    pub fn fork(&self) -> Self {
        Self {
            sel: self.sel,
            desired_col: self.desired_col,
            scroll_line: self.scroll_line,
            scroll_col: self.scroll_col,
            ..Default::default()
        }
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

    /// Map the caret, selection, top line and find bar origin through the
    /// edits since the last call; a reload or a dropped edit clamps them
    /// instead. The expand history is dropped. A view that was following
    /// its caret keeps following it without scrolling.
    pub fn catch_up(&mut self, rope: &Rope, log: &EditLog, version: u64) {
        if self.version == version {
            self.seq = log.end();
            return;
        }
        let s = &mut self.state;
        let following = s.follow_key.is_some_and(|k| (k.0, k.1) == (s.sel.head, self.version));
        let origin = s.search.as_ref().map_or(0, |q| q.origin);
        let mut ps = [s.sel.anchor, s.sel.head, self.top, origin];
        let edits = log.since(self.seq).map(|it| it.copied().collect::<Vec<_>>()).filter(|v| !v.contains(&LogEdit::Reset));
        for e in edits.iter().flatten() {
            if let LogEdit::Edit { at, old_len, new_len } = *e {
                ps = ps.map(|p| map_one(p, at, old_len, new_len, false));
            }
        }
        let len = rope.len_chars();
        let [anchor, head, top, origin] = ps.map(|p| p.min(len));
        s.sel = Selection::new(anchor, head);
        s.scroll_line = rope.char_to_line(top);
        self.top = rope.line_to_char(s.scroll_line);
        if let Some(q) = &mut s.search {
            q.origin = origin;
        }
        s.expand_stack.clear();
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
        let state = ViewState { sel: Selection::point(caret), ..Default::default() };
        let p = Parked::park(state, &b.rope, &b.edit_log, b.version);
        (b, p)
    }

    fn caught_up(b: &Buffer, p: &mut Parked) -> Selection {
        p.catch_up(&b.rope, &b.edit_log, b.version);
        p.state.sel
    }

    #[test]
    fn caret_follows_inserts_before_at_and_across() {
        let (mut b, mut p) = parked("hello world", 6);
        b.view.sel = Selection::point(0);
        b.insert("> ", TxKind::Other);
        assert_eq!(caught_up(&b, &mut p).head, 8, "insert before shifts");
        b.view.sel = Selection::point(8);
        b.insert("big ", TxKind::Other);
        assert_eq!(caught_up(&b, &mut p).head, 12, "insert at the caret pushes it");
        b.view.sel = Selection::new(14, 16);
        b.delete_selection();
        assert_eq!(caught_up(&b, &mut p).head, 12, "a later edit leaves it");
        // A deletion across the caret moves it to the start.
        b.view.sel = Selection::new(6, 14);
        b.delete_selection();
        assert_eq!(caught_up(&b, &mut p).head, 6);
        assert_eq!(b.text(), "> helld");
    }

    #[test]
    fn selection_undo_and_redo_map_too() {
        let (mut b, mut p) = parked("one two three", 0);
        p.state.sel = Selection::new(4, 7);
        b.view.sel = Selection::point(0);
        b.insert("zero ", TxKind::Other);
        assert_eq!(caught_up(&b, &mut p), Selection::new(9, 12));
        assert!(b.undo());
        assert_eq!(caught_up(&b, &mut p), Selection::new(4, 7));
        assert!(b.redo());
        assert_eq!(caught_up(&b, &mut p), Selection::new(9, 12));
        // Several edits between catch-ups map in order.
        b.view.sel = Selection::point(b.len_chars());
        b.insert(" four", TxKind::Other);
        b.view.sel = Selection::point(0);
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
    fn dropped_edits_clamp() {
        let (mut b, mut p) = parked("xyz", 3);
        for _ in 0..LOG_CAP + 1 {
            b.view.sel = Selection::point(0);
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
        p.state.follow_key = Some((p.state.sel.head, b.version, 80, 10));
        p.state.expand_stack.push((Selection::point(0), Selection::new(0, 4)));
        p = Parked::park(std::mem::take(&mut p.state), &b.rope, &b.edit_log, b.version);
        b.view.sel = Selection::point(0);
        b.insert("new 1\nnew 2\n", TxKind::Other);
        let head = caught_up(&b, &mut p).head;
        assert_eq!(b.rope.char_to_line(head), 32);
        assert_eq!(p.state.scroll_line, 22, "the same text stays at the top");
        assert_eq!(p.state.follow_key, Some((head, b.version, 80, 10)), "still following, without a scroll");
        assert!(p.state.expand_stack.is_empty());
        // A view whose caret moved since it last followed it is left to
        // scroll to it when shown.
        p.state.sel = Selection::point(0);
        b.view.sel = Selection::point(0);
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
        let mut v = ViewState { sel: Selection::new(1, 3), scroll_line: 7, scroll_col: 2, ..Default::default() };
        v.search = Some(SearchState::default());
        v.expand_stack.push((Selection::point(0), Selection::point(1)));
        v.follow_key = Some((3, 0, 1, 1));
        let f = v.fork();
        assert_eq!((f.sel, f.scroll_line, f.scroll_col), (Selection::new(1, 3), 7, 2));
        assert!(f.search.is_none() && f.expand_stack.is_empty() && f.follow_key.is_none());
    }
}
