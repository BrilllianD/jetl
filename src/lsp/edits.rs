//! Several text edits made at once, in char indices of one text, turned
//! into the changes of one `Buffer::apply` transaction (one undo step).
//!
//! The changes of a transaction apply one after another, each at a position
//! in the text the previous ones left. Applying the edits from the last to
//! the first keeps every position as it was in the original text, so
//! [`plan`] sorts them by position, descending, and refuses overlapping
//! ones. [`Plan::starts`] says where each edit's new text ends up, for the
//! caret. Completion (`lsp::completion`) uses it for an item's edit plus
//! its `additionalTextEdits`; workspace edits build on it too.

use lsp_types::TextEdit;
use ropey::Rope;

use super::pos;
use crate::editor::undo::Change;

/// Replace chars `start..end` of the original text with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl CharEdit {
    pub fn new(start: usize, end: usize, text: impl Into<String>) -> Self {
        Self { start, end: end.max(start), text: text.into() }
    }

    /// The edit of an LSP `TextEdit` (UTF-16 positions, clamped) in `rope`.
    pub fn from_lsp(rope: &Rope, e: &TextEdit) -> Self {
        Self::new(pos::char_index(rope, e.range.start), pos::char_index(rope, e.range.end), e.new_text.clone())
    }
}

/// The changes for `Buffer::apply`, and where each edit's new text starts
/// in the edited text (by the edits' input order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub starts: Vec<usize>,
}

/// Two edits of a plan replace the same text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlap {
    /// Input indices of the two edits.
    pub first: usize,
    pub second: usize,
}

/// Plan `edits` (positions in `rope`, clamped to it) as one transaction:
/// changes sorted by position descending, each with the text it replaces.
/// Insertions at the same position keep their input order in the result
/// (as the LSP asks), and an insertion at the start of a replacement goes
/// before it. Edits that change nothing are left out of the changes.
pub fn plan(rope: &Rope, edits: &[CharEdit]) -> Result<Plan, Overlap> {
    let len = rope.len_chars();
    let clamped: Vec<(usize, usize)> = edits
        .iter()
        .map(|e| {
            let start = e.start.min(len);
            (start, e.end.clamp(start, len))
        })
        .collect();
    // Text order: by start, insertions before replacements at the same
    // start, then input order.
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| (clamped[i].0, clamped[i].1 > clamped[i].0, i));
    for w in order.windows(2) {
        if clamped[w[0]].1 > clamped[w[1]].0 {
            return Err(Overlap { first: w[0].min(w[1]), second: w[0].max(w[1]) });
        }
    }
    let mut starts = vec![0; edits.len()];
    let mut shift: isize = 0;
    for &i in &order {
        let (s, e) = clamped[i];
        starts[i] = (s as isize + shift) as usize;
        shift += edits[i].text.chars().count() as isize - (e - s) as isize;
    }
    let changes = order
        .iter()
        .rev()
        .filter_map(|&i| {
            let (s, e) = clamped[i];
            let old = rope.slice(s..e).to_string();
            (old != edits[i].text).then(|| Change::replace(s, old, edits[i].text.clone()))
        })
        .collect();
    Ok(Plan { changes, starts })
}

/// `text` with `\n` line breaks as `\r\n` when `crlf` (a server's new text
/// for a CRLF buffer), and the char offset `at` in it moved along.
pub fn line_breaks(text: &str, at: usize, crlf: bool) -> (String, usize) {
    let text = text.replace("\r\n", "\n");
    if !crlf {
        return (text, at);
    }
    let before = text.chars().take(at).filter(|&c| c == '\n').count();
    (text.replace('\n', "\r\n"), at + before)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::Buffer;
    use crate::editor::selection::Selection;
    use crate::editor::undo::TxKind;

    fn run(text: &str, edits: &[CharEdit]) -> (String, Plan) {
        let mut b = Buffer::from_text(1, None, text, false);
        let plan = plan(&b.rope, edits).unwrap();
        b.apply(plan.changes.clone(), Selection::point(0), TxKind::Other);
        (b.text(), plan)
    }

    #[test]
    fn edits_apply_from_the_last_and_report_their_new_starts() {
        // An import at the top, the item in the middle, a tail at the end.
        let text = "fn a() {}\nfoo\nbar\n";
        let edits =
            [CharEdit::new(10, 13, "food()"), CharEdit::new(0, 0, "use x;\n"), CharEdit::new(14, 17, "baz"), CharEdit::new(18, 18, "!")];
        let (out, plan) = run(text, &edits);
        assert_eq!(out, "use x;\nfn a() {}\nfood()\nbaz\n!");
        assert_eq!(plan.starts, [17, 0, 24, 28]);
        assert_eq!(&out[17..23], "food()");
        // Descending positions.
        let ats: Vec<usize> = plan.changes.iter().map(|c| c.at).collect();
        assert_eq!(ats, [18, 14, 10, 0]);
        // One undo step reverts all of it.
        let mut b = Buffer::from_text(1, None, text, false);
        b.apply(plan.changes, Selection::point(0), TxKind::Other);
        assert!(b.undo());
        assert_eq!(b.text(), text);
    }

    #[test]
    fn same_position_inserts_keep_their_order_and_overlaps_fail() {
        let (out, plan) = run("ab", &[CharEdit::new(1, 1, "1"), CharEdit::new(1, 1, "2"), CharEdit::new(1, 2, "B")]);
        assert_eq!(out, "a12B");
        assert_eq!(plan.starts, [1, 2, 3]);
        let rope = Rope::from_str("abcdef");
        assert_eq!(plan_err(&rope, &[CharEdit::new(3, 5, "x"), CharEdit::new(0, 4, "y")]), Some(Overlap { first: 0, second: 1 }));
        // Touching ranges are fine; out of range positions clamp.
        assert!(super::plan(&rope, &[CharEdit::new(0, 3, "x"), CharEdit::new(3, 6, "y"), CharEdit::new(99, 120, "z")]).is_ok());
        // An edit that changes nothing has no change but a start.
        let (_, plan) = run("abc", &[CharEdit::new(0, 1, "a"), CharEdit::new(2, 3, "C")]);
        assert_eq!((plan.changes.len(), plan.starts), (1, vec![0, 2]));
    }

    fn plan_err(rope: &Rope, edits: &[CharEdit]) -> Option<Overlap> {
        super::plan(rope, edits).err()
    }

    #[test]
    fn line_breaks_follow_the_buffer() {
        assert_eq!(line_breaks("a\nb\r\nc", 4, false), ("a\nb\nc".to_string(), 4));
        assert_eq!(line_breaks("a\nb\nc", 4, true), ("a\r\nb\r\nc".to_string(), 6));
    }
}
