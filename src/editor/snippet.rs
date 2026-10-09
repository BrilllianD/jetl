//! The tabstops of an accepted completion snippet, walked with Tab and
//! Shift+Tab (the `editor.snippet` keymap context) while they last.
//!
//! The session keeps the stops as char ranges and follows the buffer's
//! edits through its [`EditLog`](super::view_state::EditLog), like the
//! conflict resolver's anchors: the stops of the current group grow with
//! text typed at their ends, the others keep out of it. A stop whose
//! enclosing placeholder was edited is dead (overwriting `${1:a ${2:b}}`
//! drops `$2`). Same-number stops (mirrors) are selected together, so
//! typing edits them all as several carets.

use std::ops::Range;

use super::buffer::{Buffer, BufferId, map_one};
use super::groups::GroupId;
use super::selection::{Selection, Selections};
use super::view_state::LogEdit;

/// A tabstop of an inserted snippet: `$n`, `${n}` (empty), `${n:placeholder}`
/// (its text) or `${n|a,b|}` (the first option), as a char range. `parent`
/// indexes the placeholder it is nested in, in the same list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tabstop {
    pub n: u32,
    pub range: Range<usize>,
    pub parent: Option<usize>,
}

#[derive(Debug, Clone)]
struct Stop {
    n: u32,
    range: Range<usize>,
    parent: Option<usize>,
    /// An edit touched it while its group was current.
    edited: bool,
}

/// A snippet's tabstops in a buffer, while Tab walks them.
#[derive(Debug)]
pub struct SnippetSession {
    pub buffer: BufferId,
    pub group: GroupId,
    stops: Vec<Stop>,
    /// Distinct tabstop numbers ascending, 0 last (always present).
    order: Vec<u32>,
    /// Index into `order`; never the `0` group.
    current: usize,
    /// `EditLog::end` and `Buffer::version` when last caught up.
    seq: u64,
    version: u64,
    /// `b.undo.len()` right after the insertion: fewer means it was undone.
    /// This relies on the undo stack having no length cap; with one, the
    /// insertion's transaction id would have to be the anchor instead.
    depth: usize,
}

/// What a Tab or Shift+Tab does.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Select a group's stops; the session goes on.
    Select(Selections),
    /// Select `$0`; the session is over.
    Finish(Selections),
}

impl SnippetSession {
    /// None when there is no tabstop but $0. `stops` are absolute
    /// (`Accept::stops`); without a `$0` one is put after the last stop.
    pub fn new(buffer: BufferId, group: GroupId, stops: Vec<Tabstop>) -> Option<Self> {
        let mut order: Vec<u32> = stops.iter().map(|s| s.n).filter(|&n| n != 0).collect();
        order.sort_unstable();
        order.dedup();
        if order.is_empty() {
            return None;
        }
        order.push(0);
        let mut stops: Vec<Stop> = stops.into_iter().map(|t| Stop { n: t.n, range: t.range, parent: t.parent, edited: false }).collect();
        if !stops.iter().any(|s| s.n == 0) {
            let end = stops.iter().map(|s| s.range.end).max().unwrap_or(0);
            stops.push(Stop { n: 0, range: end..end, parent: None, edited: false });
        }
        Some(Self { buffer, group, stops, order, current: 0, seq: 0, version: 0, depth: 0 })
    }

    /// Call right after the insertion transaction: seq, version, depth from `b`.
    pub fn attach(&mut self, b: &Buffer) {
        self.seq = b.edit_log.end();
        self.version = b.version;
        self.depth = b.undo.len();
    }

    /// No placeholder enclosing stop `i` was edited.
    fn live(&self, i: usize) -> bool {
        let mut p = self.stops[i].parent;
        while let Some(j) = p {
            if self.stops[j].edited {
                return false;
            }
            p = self.stops[j].parent;
        }
        true
    }

    /// The live stops of group `g` (an index into `order`) as selections,
    /// anchor at the start and head at the end, primary the first.
    fn group_sels(&self, g: usize) -> Option<Selections> {
        let n = self.order[g];
        let ranges: Vec<Selection> = (0..self.stops.len())
            .filter(|&i| self.stops[i].n == n && self.live(i))
            .map(|i| Selection::new(self.stops[i].range.start, self.stops[i].range.end))
            .collect();
        (!ranges.is_empty()).then(|| Selections::new(ranges, 0))
    }

    /// `$0`'s range (live or not: the snippet ends there).
    fn finish(&self) -> Selections {
        let r = self.stops.iter().find(|s| s.n == 0).map(|s| s.range.clone()).unwrap_or(0..0);
        Selections::single(Selection::new(r.start, r.end))
    }

    /// The current group's live ranges as selections (anchor = start, head
    /// = end), primary = the first one in `stops` order; `$0` when none is
    /// left.
    pub fn selections(&self) -> Selections {
        self.group_sels(self.current).unwrap_or_else(|| self.finish())
    }

    /// Map through `b.edit_log.since(self.seq)`; false when the session cannot
    /// go on (undo below depth, Reset, lost edits).
    pub fn catch_up(&mut self, b: &Buffer) -> bool {
        if b.undo.len() < self.depth {
            return false;
        }
        if b.version == self.version {
            self.seq = b.edit_log.end();
            return true;
        }
        let Some(edits) = b.edit_log.since(self.seq).map(|e| e.copied().collect::<Vec<_>>()) else { return false };
        for e in edits {
            match e {
                LogEdit::Reset => return false,
                LogEdit::Edit { at, old_len, new_len } => self.map_edit(at, old_len, new_len),
            }
        }
        self.seq = b.edit_log.end();
        self.version = b.version;
        true
    }

    /// `old_len` chars at `at` became `new_len`: the current group's stops
    /// grow with text inserted at their ends (and are marked edited), the
    /// others keep out of it.
    fn map_edit(&mut self, at: usize, old_len: usize, new_len: usize) {
        let cur = self.order[self.current];
        for s in &mut self.stops {
            let active = s.n == cur;
            if active && at <= s.range.end && s.range.start <= at + old_len {
                s.edited = true;
            }
            let start = map_one(s.range.start, at, old_len, new_len, active);
            let end = map_one(s.range.end, at, old_len, new_len, !active);
            s.range = start..end.max(start);
        }
    }

    /// Whether `head` is in a live numbered stop (inclusive); switches `current`
    /// to that stop's group when it is not in the current one.
    pub fn follow(&mut self, head: usize) -> bool {
        let inside = |s: &Stop| s.n != 0 && s.range.start <= head && head <= s.range.end;
        let cur = self.order[self.current];
        if (0..self.stops.len()).any(|i| self.stops[i].n == cur && inside(&self.stops[i]) && self.live(i)) {
            return true;
        }
        let Some(i) = (0..self.stops.len()).find(|&i| inside(&self.stops[i]) && self.live(i)) else { return false };
        let n = self.stops[i].n;
        self.current = self.order.iter().position(|&o| o == n).unwrap_or(self.current);
        true
    }

    /// Next / previous group with a live stop. Forward from the last numbered
    /// group gives Finish($0's range). Backward from the first stays put.
    pub fn step(&mut self, forward: bool) -> Step {
        let last = self.order.len() - 1;
        if forward {
            match (self.current + 1..last).find(|&g| self.group_sels(g).is_some()) {
                Some(g) => self.current = g,
                None => return Step::Finish(self.finish()),
            }
        } else if let Some(g) = (0..self.current).rev().find(|&g| self.group_sels(g).is_some()) {
            self.current = g;
        }
        Step::Select(self.selections())
    }
}

impl super::Editor {
    /// Start a session (replacing a running one), or end it with None.
    pub fn set_snippet(&mut self, s: Option<SnippetSession>) {
        self.snippet = s;
    }

    /// Whether Tab walks a snippet's tabstops (`editor.snippet`).
    pub fn snippet_active(&self) -> bool {
        self.snippet.is_some()
    }

    /// Follow the buffer's edits since the last call (from `sync_parked`,
    /// before the logs are cleared); end the session when it cannot.
    pub(super) fn snippet_catch_up(&mut self) {
        let Some(s) = &mut self.snippet else { return };
        if !self.buffers.iter().find(|b| b.id == s.buffer).is_some_and(|b| s.catch_up(b)) {
            self.snippet = None;
        }
    }

    /// Keep the session only while it still applies: `here` (the app's
    /// check: editor focused, no overlay, Insert mode), its group focused
    /// and its buffer shown, its edits followed, and the caret in one of
    /// its stops. Returns whether it stays.
    pub fn snippet_check(&mut self, here: bool) -> bool {
        let Some(mut s) = self.snippet.take() else { return false };
        let keep = here
            && self.focused == s.group
            && self.active_buffer().is_some_and(|b| b.id == s.buffer && s.catch_up(b) && s.follow(b.view.sel().head));
        if keep {
            self.snippet = Some(s);
        }
        keep
    }

    /// Tab (`forward`) or Shift+Tab: select the next or previous tabstop;
    /// past the last one, `$0`, which ends the session.
    pub(super) fn snippet_step(&mut self, forward: bool) {
        if !self.snippet_check(true) {
            return;
        }
        let Some(s) = &mut self.snippet else { return };
        let (sels, finish) = match s.step(forward) {
            Step::Select(sels) => (sels, false),
            Step::Finish(sels) => (sels, true),
        };
        if finish {
            self.snippet = None;
        }
        let Some(b) = self.active_buffer_mut() else { return };
        b.view.sels = sels;
        b.view.block = None;
        b.view.occ_stack.clear();
        b.view.occ_whole_word = false;
        b.undo.break_group = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::undo::TxKind;

    fn stop(n: u32, range: Range<usize>, parent: Option<usize>) -> Tabstop {
        Tabstop { n, range, parent }
    }

    fn ranges(s: &Selections) -> Vec<(usize, usize)> {
        s.iter().map(|r| (r.anchor, r.head)).collect()
    }

    /// `text` with a session over `stops`, attached and its first group
    /// selected, as accepting the snippet leaves it.
    fn start(text: &str, stops: Vec<Tabstop>) -> (Buffer, SnippetSession) {
        let mut b = Buffer::from_text(1, None, "", false);
        let mut s = SnippetSession::new(1, 0, stops).expect("numbered stops");
        b.apply(vec![crate::editor::undo::Change::insert(0, text)], s.selections(), TxKind::Other);
        s.attach(&b);
        (b, s)
    }

    fn select(b: &mut Buffer, step: Step) -> Selections {
        let (Step::Select(sels) | Step::Finish(sels)) = step;
        b.view.sels = sels.clone();
        b.undo.break_group = true;
        sels
    }

    #[test]
    fn typing_grows_the_current_stop_and_moves_the_next() {
        let (mut b, mut s) = start("foo(a, b)", vec![stop(1, 4..5, None), stop(2, 7..8, None), stop(0, 9..9, None)]);
        assert_eq!(ranges(&s.selections()), [(4, 5)]);
        b.insert("xy", TxKind::Typing);
        assert!(s.catch_up(&b));
        assert_eq!(b.text(), "foo(xy, b)");
        assert!(s.follow(b.view.sel().head));
        let Step::Select(sels) = s.step(true) else { panic!("a next stop") };
        assert_eq!(ranges(&sels), [(8, 9)]);
        assert_eq!(s.step(true), Step::Finish(Selections::point(10)));
    }

    #[test]
    fn adjacent_empty_stops_stay_apart() {
        let (mut b, mut s) = start("f()", vec![stop(1, 2..2, None), stop(2, 2..2, None)]);
        b.insert("ab", TxKind::Typing);
        assert!(s.catch_up(&b) && s.follow(b.view.sel().head));
        let Step::Select(sels) = s.step(true) else { panic!("a next stop") };
        assert_eq!(ranges(&sels), [(4, 4)], "stop 2 after the typed text");
        // The implicit $0 is after the last stop.
        assert_eq!(s.step(true), Step::Finish(Selections::point(4)));
    }

    #[test]
    fn overwriting_a_placeholder_drops_its_nested_stops() {
        // `${1:outer ${2:inner}} ${3|one,two|}`
        let (mut b, mut s) = start("outer inner one", vec![stop(1, 0..11, None), stop(2, 6..11, Some(0)), stop(3, 12..15, None)]);
        b.insert("z", TxKind::Typing);
        assert!(s.catch_up(&b) && s.follow(b.view.sel().head));
        let Step::Select(sels) = s.step(true) else { panic!("a next stop") };
        assert_eq!(ranges(&sels), [(2, 5)], "$2 is skipped");
        // Not overwritten, a nested stop stays.
        let (_, mut s) = start("outer inner one", vec![stop(1, 0..11, None), stop(2, 6..11, Some(0)), stop(3, 12..15, None)]);
        let Step::Select(sels) = s.step(true) else { panic!("a next stop") };
        assert_eq!(ranges(&sels), [(6, 11)]);
    }

    #[test]
    fn mirrors_select_every_occurrence() {
        let (mut b, mut s) = start("let v = 1; v", vec![stop(1, 4..5, None), stop(1, 11..12, None)]);
        let sels = s.selections();
        assert_eq!(ranges(&sels), [(4, 5), (11, 12)]);
        assert_eq!(sels.primary_index(), 0);
        b.insert("w", TxKind::Typing);
        assert_eq!(b.text(), "let w = 1; w");
        assert!(s.catch_up(&b) && s.follow(b.view.sel().head));
        assert_eq!(s.step(true), Step::Finish(Selections::point(12)));
    }

    #[test]
    fn undo_below_the_insertion_or_a_reload_ends_it() {
        let (mut b, mut s) = start("foo(a)", vec![stop(1, 4..5, None)]);
        b.insert("x", TxKind::Typing);
        assert!(b.undo());
        assert!(s.catch_up(&b), "undoing the typing keeps it");
        assert!(b.undo());
        assert!(!s.catch_up(&b), "undoing the insertion ends it");
        // A whole-text replacement.
        let (mut b, mut s) = start("foo(a)", vec![stop(1, 4..5, None)]);
        let all = b.text();
        b.apply(vec![crate::editor::undo::Change::replace(0, all, "bar")], Selections::point(0), TxKind::Other);
        assert!(!s.catch_up(&b));
    }

    #[test]
    fn shift_tab_on_the_first_stays() {
        let (mut b, mut s) = start("foo(a, b)", vec![stop(1, 4..5, None), stop(2, 7..8, None)]);
        let first = s.step(false);
        assert_eq!(first, Step::Select(s.selections()));
        assert_eq!(ranges(&select(&mut b, first)), [(4, 5)]);
        select(&mut b, s.step(true));
        assert_eq!(ranges(&select(&mut b, s.step(false))), [(4, 5)]);
        // A click into the second stop makes it current.
        assert!(s.follow(8) && !s.follow(6));
        assert_eq!(ranges(&s.selections()), [(7, 8)]);
    }
}
