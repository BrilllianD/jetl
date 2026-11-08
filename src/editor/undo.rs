//! Edits are transactions of replacements; the undo stack stores them with
//! the selection before and after so undo restores the caret too.

use std::time::{Duration, Instant};

use super::selection::Selections;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Char index at which `old` is replaced by `new`.
    pub at: usize,
    pub old: String,
    pub new: String,
}

impl Change {
    pub fn insert(at: usize, new: impl Into<String>) -> Self {
        Self { at, old: String::new(), new: new.into() }
    }
    pub fn delete(at: usize, old: impl Into<String>) -> Self {
        Self { at, old: old.into(), new: String::new() }
    }
    pub fn replace(at: usize, old: impl Into<String>, new: impl Into<String>) -> Self {
        Self { at, old: old.into(), new: new.into() }
    }
    pub fn inverse(&self) -> Change {
        Change { at: self.at, old: self.new.clone(), new: self.old.clone() }
    }
    pub fn end_after(&self) -> usize {
        self.at + self.new.chars().count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxKind {
    /// Consecutive typing merges into one transaction.
    Typing,
    /// Consecutive single-char deletions merge.
    Deleting,
    Other,
}

#[derive(Debug, Clone)]
pub struct Transaction {
    /// Unique per stack; assigned by [`UndoStack::push`] and renewed when
    /// a later edit merges into it, so a saved state id never matches text
    /// it did not describe.
    pub id: u64,
    pub changes: Vec<Change>,
    pub sel_before: Selections,
    pub sel_after: Selections,
    pub at: Instant,
    pub kind: TxKind,
    /// Line ending before and after, for a line ending conversion; undo and
    /// redo restore it along with the text.
    pub line_ending: Option<(super::buffer::LineEnding, super::buffer::LineEnding)>,
    /// Collapsed folds the edits dropped, as (index into `changes`, fold in
    /// the coordinates just before that change); undo collapses them again.
    pub folds: Vec<(usize, super::fold::Fold)>,
}

/// A place in the undo stack ([`UndoStack::mark`]): its length and the id of
/// the transaction on top then (0 for an empty stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UndoMark {
    pub len: usize,
    pub id: u64,
}

#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    pub group_window: Option<Duration>,
    /// Set by movement/save to stop the next edit from merging.
    pub break_group: bool,
    next_id: u64,
}

impl UndoStack {
    pub fn new() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), group_window: Some(Duration::from_millis(500)), break_group: false, next_id: 1 }
    }

    /// Identifies the text state the stack is at: the id of the last applied
    /// transaction, or 0 at the bottom. Undoing back to a saved state yields
    /// the same id again.
    pub fn state_id(&self) -> u64 {
        self.undo.last().map(|t| t.id).unwrap_or(0)
    }

    fn fresh_id(&mut self) -> u64 {
        // Ids start at 1 so they never collide with the empty stack.
        self.next_id = self.next_id.max(1);
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn push(&mut self, mut tx: Transaction) {
        self.redo.clear();
        let id = self.fresh_id();
        if !self.break_group
            && tx.kind != TxKind::Other
            && let Some(last) = self.undo.last_mut()
        {
            let within = self.group_window.map(|w| tx.at.duration_since(last.at) <= w).unwrap_or(true);
            // Several carets emit their changes back to front, so the change
            // positions say nothing; the edit continues the last one when it
            // starts from the carets that one left.
            let multi = tx.changes.len() > 1 || tx.sel_before.is_multi() || last.sel_after.is_multi();
            let contiguous = match tx.kind {
                TxKind::Typing | TxKind::Deleting if multi => last.sel_after == tx.sel_before,
                TxKind::Typing => last.changes.last().map(|c| c.end_after()) == tx.changes.first().map(|c| c.at),
                TxKind::Deleting => {
                    let prev = last.changes.last();
                    let cur = tx.changes.first();
                    match (prev, cur) {
                        (Some(p), Some(c)) => c.at == p.at || c.end_after() == p.at || c.at + c.old.chars().count() == p.at,
                        _ => false,
                    }
                }
                TxKind::Other => false,
            };
            if last.kind == tx.kind && within && contiguous {
                last.id = id;
                let offset = last.changes.len();
                last.folds.extend(tx.folds.into_iter().map(|(i, f)| (i + offset, f)));
                last.changes.extend(tx.changes);
                last.sel_after = tx.sel_after;
                last.at = tx.at;
                return;
            }
        }
        self.break_group = false;
        tx.id = id;
        self.undo.push(tx);
    }

    /// Mark the top of the stack and stop the next edit from merging into it,
    /// so [`Self::squash_since`] can fold everything after it.
    pub fn mark(&mut self) -> UndoMark {
        self.break_group = true;
        UndoMark { len: self.undo.len(), id: self.undo.last().map(|t| t.id).unwrap_or(0) }
    }

    /// Fold the transactions pushed since `m` into one, so a single undo
    /// reverts them all. Leaves the stack as it is (false) when fewer than two
    /// are new, when the transaction below them is no longer the marked one
    /// (an undo went below the mark), or when one converts the line ending.
    /// The folded transaction keeps the last id, so a saved state id stays
    /// valid. The redo stack is not touched.
    pub fn squash_since(&mut self, m: UndoMark) -> bool {
        if self.undo.len() <= m.len + 1 {
            return false;
        }
        if m.len > 0 && self.undo[m.len - 1].id != m.id {
            return false;
        }
        if self.undo[m.len..].iter().any(|t| t.line_ending.is_some()) {
            return false;
        }
        let mut tail = self.undo.drain(m.len..);
        let mut first = tail.next().expect("two or more new transactions");
        for t in tail {
            let offset = first.changes.len();
            first.folds.extend(t.folds.into_iter().map(|(i, f)| (i + offset, f)));
            first.changes.extend(t.changes);
            first.sel_after = t.sel_after;
            first.at = t.at;
            first.id = t.id;
        }
        first.kind = TxKind::Other;
        self.undo.push(first);
        self.break_group = true;
        true
    }

    pub fn pop_undo(&mut self) -> Option<Transaction> {
        let tx = self.undo.pop()?;
        self.redo.push(tx.clone());
        self.break_group = true;
        Some(tx)
    }

    pub fn pop_redo(&mut self) -> Option<Transaction> {
        let tx = self.redo.pop()?;
        self.undo.push(tx.clone());
        self.break_group = true;
        Some(tx)
    }

    /// The transaction undo would revert next.
    pub fn last_mut(&mut self) -> Option<&mut Transaction> {
        self.undo.last_mut()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn len(&self) -> usize {
        self.undo.len()
    }
    pub fn is_empty(&self) -> bool {
        self.undo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::super::selection::Selection;
    use super::*;

    fn tx(changes: Vec<Change>, before: Selections, after: Selections, kind: TxKind) -> Transaction {
        Transaction { id: 0, changes, sel_before: before, sel_after: after, at: Instant::now(), kind, line_ending: None, folds: Vec::new() }
    }

    fn sels(ps: &[usize]) -> Selections {
        Selections::new(ps.iter().map(|&p| Selection::point(p)).collect(), 0)
    }

    #[test]
    fn single_caret_typing_merges_by_position() {
        let mut u = UndoStack::new();
        u.push(tx(vec![Change::insert(0, "a")], sels(&[0]), sels(&[1]), TxKind::Typing));
        u.push(tx(vec![Change::insert(1, "b")], sels(&[1]), sels(&[2]), TxKind::Typing));
        assert_eq!(u.len(), 1);
        // Somewhere else: a new step, even with the same selections.
        u.push(tx(vec![Change::insert(9, "c")], sels(&[2]), sels(&[10]), TxKind::Typing));
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn multi_caret_typing_merges_when_it_starts_from_the_last_carets() {
        let mut u = UndoStack::new();
        let typed = |at: [usize; 2]| vec![Change::insert(at[1], "x"), Change::insert(at[0], "x")];
        u.push(tx(typed([0, 5]), sels(&[0, 5]), sels(&[1, 7]), TxKind::Typing));
        u.push(tx(typed([1, 6]), sels(&[1, 7]), sels(&[2, 9]), TxKind::Typing));
        assert_eq!(u.len(), 1);
        assert_eq!(u.last_mut().unwrap().sel_after, sels(&[2, 9]));
        // Carets that moved in between: not merged.
        u.push(tx(typed([3, 8]), sels(&[3, 10]), sels(&[4, 12]), TxKind::Typing));
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn squash_since_folds_the_new_transactions_into_one() {
        let mut u = UndoStack::new();
        u.push(tx(vec![Change::insert(0, "z")], sels(&[0]), sels(&[1]), TxKind::Other));
        let m = u.mark();
        u.push(tx(vec![Change::insert(1, "a")], sels(&[1]), sels(&[2]), TxKind::Other));
        u.push(tx(vec![Change::insert(2, "b")], sels(&[2]), sels(&[3]), TxKind::Other));
        u.push(tx(vec![Change::insert(3, "c")], sels(&[3]), sels(&[4]), TxKind::Other));
        let id = u.state_id();
        assert!(u.squash_since(m));
        assert_eq!(u.len(), 2);
        assert_eq!(u.state_id(), id);
        let t = u.last_mut().unwrap();
        let news: Vec<&str> = t.changes.iter().map(|c| c.new.as_str()).collect();
        assert_eq!(news, ["a", "b", "c"]);
        assert_eq!(t.sel_before, sels(&[1]));
        assert_eq!(t.sel_after, sels(&[4]));
        assert_eq!(t.kind, TxKind::Other);
    }

    #[test]
    fn squash_since_keeps_a_stack_that_went_below_the_mark() {
        let mut u = UndoStack::new();
        u.push(tx(vec![Change::insert(0, "z")], sels(&[0]), sels(&[1]), TxKind::Other));
        u.push(tx(vec![Change::insert(1, "y")], sels(&[1]), sels(&[2]), TxKind::Other));
        let m = u.mark();
        u.pop_undo();
        u.push(tx(vec![Change::insert(1, "a")], sels(&[1]), sels(&[2]), TxKind::Other));
        u.push(tx(vec![Change::insert(2, "b")], sels(&[2]), sels(&[3]), TxKind::Other));
        u.push(tx(vec![Change::insert(3, "c")], sels(&[3]), sels(&[4]), TxKind::Other));
        // Long enough to pass the length check: only the marked id tells.
        assert_eq!(u.len(), 4);
        assert!(!u.squash_since(m));
        assert_eq!(u.len(), 4);
    }

    #[test]
    fn squash_since_keeps_line_ending_conversions() {
        use super::super::buffer::LineEnding;
        let mut u = UndoStack::new();
        let m = u.mark();
        u.push(tx(vec![Change::insert(0, "a")], sels(&[0]), sels(&[1]), TxKind::Other));
        let mut conv = tx(vec![Change::insert(1, "b")], sels(&[1]), sels(&[2]), TxKind::Other);
        conv.line_ending = Some((LineEnding::Lf, LineEnding::CrLf));
        u.push(conv);
        assert!(!u.squash_since(m));
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn squash_since_needs_two_new_transactions() {
        let mut u = UndoStack::new();
        let m = u.mark();
        assert!(!u.squash_since(m));
        u.push(tx(vec![Change::insert(0, "a")], sels(&[0]), sels(&[1]), TxKind::Other));
        assert!(!u.squash_since(m));
        assert_eq!(u.len(), 1);
    }
}
