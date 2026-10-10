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
}
