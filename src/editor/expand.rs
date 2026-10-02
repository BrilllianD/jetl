//! Expand selection (JetBrains "Extend Selection"). With a syntax tree each
//! step selects the smallest node strictly containing the selection, trying
//! the contents between the node's brackets first; without one it steps
//! word, line, whole text. Positions are char indices; the tree uses bytes.

use std::ops::Range;

use ropey::Rope;
use tree_sitter::{Node, Tree};

use super::selection::{is_word_char, line_range, word_at};

/// The next larger range around `r`, or None when `r` already covers the
/// whole text.
pub fn expand_range(rope: &Rope, tree: Option<&Tree>, r: Range<usize>) -> Option<Range<usize>> {
    let found = match tree {
        Some(t) => node_step(rope, t, &r),
        None => plain_step(rope, &r),
    };
    let all = 0..rope.len_chars();
    found.or_else(|| grows(&all, &r).then_some(all))
}

/// `c` contains `r` and is larger.
fn grows(c: &Range<usize>, r: &Range<usize>) -> bool {
    c.start <= r.start && c.end >= r.end && c != r
}

/// The char an empty selection at `p` expands from: a word char next to the
/// caret wins (so `foo|(` starts at `foo`), then any non-blank one.
fn char_at_caret(rope: &Rope, p: usize) -> Option<usize> {
    let len = rope.len_chars();
    let before = p.checked_sub(1);
    let fits = |i: usize, word: bool| i < len && if word { is_word_char(rope.char(i)) } else { !rope.char(i).is_whitespace() };
    let tries = [(Some(p), true), (before, true), (Some(p), false), (before, false)];
    tries.into_iter().find_map(|(i, word)| i.filter(|&i| fits(i, word))).or((p < len).then_some(p)).or(before)
}

fn node_step(rope: &Rope, tree: &Tree, r: &Range<usize>) -> Option<Range<usize>> {
    let q = if r.is_empty() { char_at_caret(rope, r.start).map(|c| c..c + 1)? } else { r.clone() };
    let mut node = tree.root_node().descendant_for_byte_range(rope.char_to_byte(q.start), rope.char_to_byte(q.end))?;
    loop {
        let nr = rope.byte_to_char(node.start_byte())..rope.byte_to_char(node.end_byte());
        if grows(&nr, r) {
            // The inner range lies strictly inside the node, so it only
            // needs checking once the node itself grows the selection.
            return Some(inner_range(rope, node).filter(|i| grows(i, r)).unwrap_or(nr));
        }
        node = node.parent()?;
    }
}

/// The contents between a node's first and last child when those are a
/// bracket or quote pair, trimmed of surrounding whitespace.
fn inner_range(rope: &Rope, node: Node) -> Option<Range<usize>> {
    let n = node.child_count();
    if n < 2 {
        return None;
    }
    let (first, last) = (node.child(0)?, node.child(n - 1)?);
    if !matches!((first.kind(), last.kind()), ("(", ")") | ("[", "]") | ("{", "}") | ("<", ">") | ("\"", "\"") | ("'", "'")) {
        return None;
    }
    let mut s = rope.byte_to_char(first.end_byte());
    let mut e = rope.byte_to_char(last.start_byte());
    while s < e && rope.char(s).is_whitespace() {
        s += 1;
    }
    while e > s && rope.char(e - 1).is_whitespace() {
        e -= 1;
    }
    (s < e).then_some(s..e)
}

fn plain_step(rope: &Rope, r: &Range<usize>) -> Option<Range<usize>> {
    let from = if r.is_empty() { char_at_caret(rope, r.start) } else { Some(r.start) };
    if let Some(w) = from.filter(|&p| is_word_char(rope.char(p))).map(|p| word_at(rope, p)).filter(|w| grows(w, r)) {
        return Some(w);
    }
    let l0 = rope.char_to_line(r.start);
    let l1 = if r.end > r.start { rope.char_to_line(r.end - 1) } else { l0 };
    let lines = line_range(rope, l0).start..line_range(rope, l1).end;
    grows(&lines, r).then_some(lines)
}
