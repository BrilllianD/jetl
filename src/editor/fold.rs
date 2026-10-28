//! Code folding: the collapsed regions of a view ([`Folds`], one per
//! `ViewState`, so split panes fold independently) and the regions that can
//! fold (`fold_candidates`, from the syntax tree and the trees of embedded
//! languages, in patch tabs from the diff's sections, or else from the
//! indentation).
//!
//! A fold is two char anchors mapped through every edit like carets. Its
//! header line (the one holding `start`) stays visible; the lines after it
//! are hidden up to the line holding `end`, which is hidden too unless
//! something other than closing punctuation follows `end` there (so
//! `} else {` stays visible).

use std::ops::Range;

use ropey::Rope;
use tree_sitter::{Node, Tree};

use super::buffer::{Buffer, map_one};
use super::rows::Rows;
use super::selection::{Selections, indent_of, line_len_no_eol};
use super::syntax::Syntax;
use crate::workspace::SessionFold;

/// A region of the text, in chars: a collapsed fold or one that can fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub start: usize,
    pub end: usize,
}

impl Fold {
    /// Its header line and the last line it hides; `None` when it would
    /// hide nothing (or is out of the text).
    pub fn lines(&self, rope: &Rope) -> Option<(usize, usize)> {
        if self.start >= self.end || self.end > rope.len_chars() {
            return None;
        }
        let header = rope.char_to_line(self.start);
        let last = rope.char_to_line(self.end);
        let last_start = rope.line_to_char(last);
        // Ending at a line start, the line before is the last one.
        let last_hidden = if self.end == last_start {
            last.checked_sub(1)?
        } else {
            let after = rope.slice(self.end..last_start + line_len_no_eol(rope, last));
            let closing = after.chars().all(|c| c.is_whitespace() || ";,)]}".contains(c));
            if closing { last } else { last.checked_sub(1)? }
        };
        (last_hidden > header).then_some((header, last_hidden))
    }
}

/// The collapsed folds of one view.
#[derive(Debug, Default, Clone)]
pub struct Folds {
    /// Sorted by start, each hiding at least one line.
    list: Vec<Fold>,
    /// The header and last hidden line of each fold in `list`.
    lines: Vec<(usize, usize)>,
    /// The hidden lines: sorted, disjoint, merged ranges.
    hidden: Vec<Range<usize>>,
}

impl Folds {
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn hidden(&self) -> &[Range<usize>] {
        &self.hidden
    }

    /// Each collapsed fold with its header and last hidden line.
    pub fn iter(&self) -> impl Iterator<Item = (Fold, (usize, usize))> + '_ {
        self.list.iter().copied().zip(self.lines.iter().copied())
    }

    /// Map the anchors through the replacement of `old_len` chars at `at`
    /// by `new_len` chars. A fold loses an anchor, and goes, when the
    /// replaced text strictly contains it. Text inserted right at the end
    /// stays outside. Call [`Folds::rebuild`] after.
    pub fn map_edit(&mut self, at: usize, old_len: usize, new_len: usize) {
        self.list.retain_mut(|f| match map_fold(*f, at, old_len, new_len) {
            Some(m) => {
                *f = m;
                true
            }
            None => false,
        });
    }

    /// The collapsed folds, sorted by start.
    pub fn list(&self) -> &[Fold] {
        &self.list
    }

    /// Recompute the lines from the text, dropping folds that no longer
    /// hide anything and duplicates.
    pub fn rebuild(&mut self, rope: &Rope) {
        self.list.sort_by_key(|f| (f.start, f.end));
        self.list.dedup();
        let mut lines = Vec::with_capacity(self.list.len());
        self.list.retain(|f| match f.lines(rope) {
            Some(l) => {
                lines.push(l);
                true
            }
            None => false,
        });
        self.lines = lines;
        let mut spans: Vec<Range<usize>> = self.lines.iter().map(|&(h, l)| h + 1..l + 1).collect();
        spans.sort_by_key(|r| r.start);
        self.hidden.clear();
        for r in spans {
            match self.hidden.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => self.hidden.push(r),
            }
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Expand every fold hiding a caret (its head), unless a selection
    /// covers all of the fold's hidden text. True when one went.
    pub fn reveal(&mut self, rope: &Rope, sels: &Selections) -> bool {
        if self.list.is_empty() {
            return false;
        }
        // Heads of sorted, apart selections are sorted too.
        let heads: Vec<usize> = sels.heads().collect();
        let before = self.list.len();
        let mut i = 0;
        self.list.retain(|_| {
            let (h, last) = self.lines[i];
            i += 1;
            let lo = rope.line_to_char(h + 1);
            let hi = rope.line_to_char(last) + line_len_no_eol(rope, last);
            let k = heads.partition_point(|&p| p < lo);
            if !heads.get(k).is_some_and(|&p| p <= hi) {
                return true;
            }
            // A selection over the whole hidden text (Select All, Select
            // Line on the header) leaves the fold alone.
            sels.iter().any(|s| {
                let r = s.range();
                r.start <= lo && hi <= r.end
            })
        });
        if self.list.len() == before {
            return false;
        }
        self.rebuild(rope);
        true
    }

    /// The fold whose header is `line` and that hides the most, with its
    /// last hidden line.
    pub fn header_at(&self, line: usize) -> Option<(Fold, usize)> {
        self.iter().filter(|(_, (h, _))| *h == line).max_by_key(|(_, (_, l))| *l).map(|(f, (_, l))| (f, l))
    }

    /// Whether a collapsed fold has the same lines as `f`.
    pub fn has(&self, rope: &Rope, f: &Fold) -> bool {
        f.lines(rope).is_some_and(|l| self.lines.contains(&l))
    }

    /// Collapse `f`; false when it hides nothing or is collapsed already.
    pub fn collapse(&mut self, f: Fold, rope: &Rope) -> bool {
        if f.lines(rope).is_none() || self.has(rope, &f) {
            return false;
        }
        self.list.push(f);
        self.rebuild(rope);
        true
    }

    /// Collapse each of `folds` that hides something and is not collapsed.
    pub fn collapse_all(&mut self, folds: Vec<Fold>, rope: &Rope) {
        let new: Vec<Fold> = folds.into_iter().filter(|f| f.lines(rope).is_some() && !self.has(rope, f)).collect();
        if !new.is_empty() {
            self.list.extend(new);
            self.rebuild(rope);
        }
    }

    /// Expand every fold inside `outer`.
    pub fn expand_within(&mut self, outer: Fold, rope: &Rope) {
        self.list.retain(|f| !(outer.start <= f.start && f.end <= outer.end));
        self.rebuild(rope);
    }

    /// Expand the folds whose header is `line`, and with `recursive` every
    /// fold inside them too. False when there was none.
    pub fn expand_at(&mut self, line: usize, rope: &Rope, recursive: bool) -> bool {
        let outer: Vec<Fold> = self.iter().filter(|(_, (h, _))| *h == line).map(|(f, _)| f).collect();
        if outer.is_empty() {
            return false;
        }
        let gone = |f: &Fold| outer.contains(f) || (recursive && outer.iter().any(|o| o.start <= f.start && f.end <= o.end));
        self.list.retain(|f| !gone(f));
        self.rebuild(rope);
        true
    }

    /// The folds as lines and columns, for the session.
    pub fn to_session(&self, rope: &Rope) -> Vec<SessionFold> {
        let lc = |p: usize| {
            let line = rope.char_to_line(p);
            (line, p - rope.line_to_char(line))
        };
        self.list
            .iter()
            .map(|f| {
                let ((start_line, start_col), (end_line, end_col)) = (lc(f.start), lc(f.end));
                SessionFold { start_line, start_col, end_line, end_col }
            })
            .collect()
    }

    /// Folds from a session, clamped to the text as it is now; those past
    /// its end or hiding nothing are dropped.
    pub fn from_session(saved: &[SessionFold], rope: &Rope) -> Self {
        let mut f = Self { list: session_folds(saved, rope), ..Default::default() };
        f.rebuild(rope);
        f
    }

    /// Collapse the folds of a session form (clamped like
    /// [`Folds::from_session`]) that hide something and are not collapsed.
    pub fn collapse_saved(&mut self, saved: &[SessionFold], rope: &Rope) {
        self.collapse_all(session_folds(saved, rope), rope);
    }
}

/// `f` after the replacement of `old_len` chars at `at` by `new_len`
/// chars; `None` when the replaced text strictly contains an anchor.
pub fn map_fold(f: Fold, at: usize, old_len: usize, new_len: usize) -> Option<Fold> {
    let inside = |p: usize| at < p && p < at + old_len;
    if inside(f.start) || inside(f.end) {
        return None;
    }
    Some(Fold { start: map_one(f.start, at, old_len, new_len, false), end: map_one(f.end, at, old_len, new_len, true) })
}

/// `saved` as char folds in `rope`; those past its end are dropped.
fn session_folds(saved: &[SessionFold], rope: &Rope) -> Vec<Fold> {
    let pos = |line: usize, col: usize| (line < rope.len_lines()).then(|| rope.line_to_char(line) + col.min(line_len_no_eol(rope, line)));
    saved.iter().filter_map(|s| Some(Fold { start: pos(s.start_line, s.start_col)?, end: pos(s.end_line, s.end_col)? })).collect()
}

/// Tokens that close a bracketed node.
const CLOSERS: &[&str] = &["}", "]", ")", "end", "fi", "done", "esac"];

/// Sibling statements that fold together when they follow each other.
const IMPORT_KINDS: &[&str] =
    &["use_declaration", "import_statement", "import_from_statement", "import_declaration", "preproc_include", "using_directive"];

/// Bodies of a statement: when one starts its own line (a Python block,
/// an Allman brace) its header is the line before.
fn is_statement_body(kind: &str) -> bool {
    matches!(kind, "block" | "suite" | "compound_statement" | "declaration_list")
        || kind.ends_with("_body")
        || kind.ends_with("field_declaration_list")
}

/// Kinds that fold whatever they end with.
fn is_body(kind: &str) -> bool {
    is_statement_body(kind) || matches!(kind, "object" | "array" | "element" | "section" | "table" | "block_mapping" | "block_sequence")
}

/// Whether only blanks come before char `pos` on its line.
fn starts_line(rope: &Rope, pos: usize) -> bool {
    let line_start = rope.line_to_char(rope.char_to_line(pos));
    rope.slice(line_start..pos).chars().all(|c| c == ' ' || c == '\t')
}

/// The last row `node` covers: its end row, or the one before when it
/// ends with its line break.
fn last_row(node: &Node, rope: &Rope) -> usize {
    let end = rope.byte_to_line(node.end_byte());
    if rope.line_to_byte(end) == node.end_byte() && node.end_byte() > node.start_byte() { end - 1 } else { end }
}

/// The line (as the rope counts) `node` starts on. Tree-sitter rows count
/// only `\n`, so a lone `\r` break would put them off.
fn first_row(node: &Node, rope: &Rope) -> usize {
    rope.byte_to_line(node.start_byte())
}

/// The fold of `node` when it can fold: a named node hiding a line, not a
/// string, that is bracketed (its last child a closing token), a
/// body or a comment.
fn node_fold(node: &Node, rope: &Rope) -> Option<Fold> {
    let kind = node.kind();
    // A body on one line can still hide it under the header above.
    let one_line = last_row(node, rope) == first_row(node, rope) && !is_statement_body(kind);
    if !node.is_named() || node.is_error() || node.is_missing() || one_line || kind.contains("string") {
        return None;
    }
    let closed = node.child(node.child_count().checked_sub(1)?).is_some_and(|c| !c.is_named() && CLOSERS.contains(&c.kind()));
    if !closed && !is_body(kind) && !kind.contains("comment") {
        return None;
    }
    let mut start = rope.byte_to_char(node.start_byte());
    if is_statement_body(kind) && starts_line(rope, start) {
        let mut prev = node.prev_sibling();
        while let Some(p) = prev.filter(|p| p.is_extra()) {
            prev = p.prev_sibling();
        }
        // The last char of what comes before: an insert after it (Enter at
        // the end of the header line) leaves the header where it is.
        if let Some(p) = prev {
            start = rope.byte_to_char(p.end_byte()).saturating_sub(1);
        }
    }
    let f = Fold { start, end: rope.byte_to_char(node.end_byte().min(rope.len_bytes())) };
    f.lines(rope).map(|_| f)
}

/// Runs among `kids` (siblings in order) that fold as one: two or more
/// line comments on consecutive lines, each first on its line, and
/// consecutive imports (blank lines between allowed).
fn runs(kids: &[Node], rope: &Rope, out: &mut Vec<Fold>) {
    let line_comment = |n: &Node| {
        n.kind().contains("comment") && last_row(n, rope) == first_row(n, rope) && starts_line(rope, rope.byte_to_char(n.start_byte()))
    };
    let import = |n: &Node| IMPORT_KINDS.contains(&n.kind());
    let mut push = |run: &[Node]| {
        if run.len() >= 2 {
            let f = Fold { start: rope.byte_to_char(run[0].start_byte()), end: rope.byte_to_char(run[run.len() - 1].end_byte()) };
            if f.lines(rope).is_some() {
                out.push(f);
            }
        }
    };
    let mut i = 0;
    while i < kids.len() {
        let mut j = i + 1;
        if line_comment(&kids[i]) {
            while j < kids.len() && line_comment(&kids[j]) && first_row(&kids[j], rope) == last_row(&kids[j - 1], rope) + 1 {
                j += 1;
            }
        } else if import(&kids[i]) {
            while j < kids.len() && import(&kids[j]) {
                j += 1;
            }
        }
        push(&kids[i..j]);
        i = j;
    }
}

/// The folds of the nodes of `tree` that `visit` lets through (it gets the
/// node's first and last row); subtrees it refuses are skipped.
fn tree_folds(tree: &Tree, rope: &Rope, visit: impl Fn(usize, usize) -> bool) -> Vec<Fold> {
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    let mut cursor = tree.walk();
    while let Some(node) = stack.pop() {
        out.extend(node_fold(&node, rope));
        let kids: Vec<Node> = node.children(&mut cursor).collect();
        runs(&kids, rope, &mut out);
        stack.extend(kids.into_iter().rev().filter(|k| visit(first_row(k, rope), rope.byte_to_line(k.end_byte()))));
    }
    out
}

/// Whether `line` is blank.
fn blank(rope: &Rope, line: usize) -> bool {
    rope.line(line).chars().all(char::is_whitespace)
}

/// Without a syntax tree: a line folds when the next non-blank line is
/// indented deeper, down to the last line before one indented no deeper
/// (blank lines at the end left out).
fn indent_fold(rope: &Rope, line: usize) -> Option<Fold> {
    if line >= rope.len_lines() || blank(rope, line) {
        return None;
    }
    let indent = indent_of(rope, line);
    let mut last = None;
    for l in line + 1..rope.len_lines() {
        if blank(rope, l) {
            continue;
        }
        if indent_of(rope, l) <= indent {
            break;
        }
        last = Some(l);
    }
    let last = last?;
    let f = Fold { start: rope.line_to_char(line) + indent, end: rope.line_to_char(last) + line_len_no_eol(rope, last) };
    f.lines(rope).map(|_| f)
}

/// The largest of `folds` per header line in `lines`, sorted by start.
fn per_header(folds: Vec<Fold>, rope: &Rope, lines: &Range<usize>) -> Vec<Fold> {
    let mut keyed: Vec<((usize, usize), Fold)> =
        folds.into_iter().filter_map(|f| f.lines(rope).filter(|(h, _)| lines.contains(h)).map(|l| (l, f))).collect();
    keyed.sort_by_key(|((h, l), f)| (*h, std::cmp::Reverse(*l), std::cmp::Reverse(f.end)));
    keyed.dedup_by_key(|((h, _), _)| *h);
    let mut out: Vec<Fold> = keyed.into_iter().map(|(_, f)| f).collect();
    out.sort_by_key(|f| f.start);
    out
}

/// The syntax, parsed up to date, to read the trees of (host first, then
/// injections); `None` without a syntax or a host tree.
fn fold_syntax<'a>(syntax: &'a mut Option<Syntax>, rope: &Rope) -> Option<&'a Syntax> {
    let s = syntax.as_mut()?;
    s.reparse(rope);
    s.tree()?;
    Some(&*s)
}

impl Buffer {
    /// The regions that can fold with their header on `lines`, one per
    /// header line (the largest), sorted. From the syntax tree when there
    /// is one, else from the indentation. Cached per version and lines.
    pub fn fold_candidates(&mut self, lines: Range<usize>) -> Vec<Fold> {
        if let Some((v, l, folds)) = &self.fold_cache
            && *v == self.version
            && *l == lines
        {
            return folds.clone();
        }
        let folds = self.find_candidates(&lines);
        self.fold_cache = Some((self.version, lines, folds.clone()));
        folds
    }

    /// Patch tabs fold by `== ` commit, `diff ` file and `@@` hunk sections
    /// (`Patch::folds`); their `+`/`-`/space prefixes make indent folds
    /// meaningless.
    fn find_candidates(&mut self, lines: &Range<usize>) -> Vec<Fold> {
        if let Some(p) = &self.patch {
            return per_header(p.folds(&self.rope), &self.rope, lines);
        }
        match fold_syntax(&mut self.syntax, &self.rope) {
            Some(syn) => {
                let visit = |first: usize, last: usize| last >= lines.start && first <= lines.end;
                let all = syn.trees().flat_map(|t| tree_folds(t, &self.rope, visit)).collect();
                per_header(all, &self.rope, lines)
            }
            None => lines.clone().filter_map(|l| indent_fold(&self.rope, l)).collect(),
        }
    }

    /// The lines of every caret, each once.
    fn caret_lines(&self) -> Vec<usize> {
        let mut lines: Vec<usize> = self.view.sels.heads().map(|h| self.rope.char_to_line(h.min(self.rope.len_chars()))).collect();
        lines.dedup();
        lines
    }

    /// Collapse the innermost region at each caret's line that is not
    /// collapsed yet (on a folded line, the one around it), and with
    /// `recursive` every region inside it too.
    pub fn fold_collapse(&mut self, recursive: bool) {
        for line in self.caret_lines() {
            let chain = self.fold_chain(line);
            let Some(f) = chain.into_iter().find(|f| !self.view.folds.has(&self.rope, f)) else { continue };
            let mut add = vec![f];
            if recursive && let Some((h, last)) = f.lines(&self.rope) {
                add.extend(self.find_candidates(&(h..last + 1)).into_iter().filter(|g| f.start <= g.start && g.end <= f.end));
            }
            self.view.folds.collapse_all(add, &self.rope);
        }
        self.carets_out_of_folds();
    }

    /// Expand the fold on each caret's line; with `recursive` the folds
    /// inside it too (or, on a line without one, every fold inside the
    /// innermost region there).
    pub fn fold_expand(&mut self, recursive: bool) {
        for line in self.caret_lines() {
            if self.view.folds.expand_at(line, &self.rope, recursive) || !recursive {
                continue;
            }
            if let Some(&f) = self.fold_chain(line).first() {
                self.view.folds.expand_within(f, &self.rope);
            }
        }
    }

    /// Collapse every region of the text that can fold.
    pub fn fold_collapse_all(&mut self) {
        let all = self.find_candidates(&(0..self.rope.len_lines()));
        self.view.folds.collapse_all(all, &self.rope);
        self.carets_out_of_folds();
    }

    /// Fold each caret's selection over several lines; a caret without one
    /// expands the fold on its line.
    pub fn fold_selection(&mut self) {
        let sels: Vec<_> = self.view.sels.iter().copied().collect();
        for s in sels {
            let r = s.range();
            if s.is_empty() {
                self.view.folds.expand_at(self.rope.char_to_line(r.start), &self.rope, false);
            } else {
                self.view.folds.collapse(Fold { start: r.start, end: r.end }, &self.rope);
            }
        }
        self.carets_out_of_folds();
    }

    /// The regions that can fold holding `line` (as header or hidden
    /// line), innermost first.
    pub fn fold_chain(&mut self, line: usize) -> Vec<Fold> {
        let patch = self.patch.as_ref().map(|p| p.folds(&self.rope));
        let syn = if patch.is_some() { None } else { fold_syntax(&mut self.syntax, &self.rope) };
        let rope = &self.rope;
        let mut folds = match (patch, syn) {
            (Some(folds), _) => folds,
            (None, Some(syn)) => {
                let visit = |first: usize, last: usize| last >= line && first <= line + 1;
                syn.trees().flat_map(|t| tree_folds(t, rope, visit)).collect()
            }
            (None, None) => {
                // Headers above, each indented less than the one below it.
                let mut out = Vec::new();
                let mut indent = usize::MAX;
                for h in (0..=line.min(rope.len_lines().saturating_sub(1))).rev() {
                    if blank(rope, h) || indent_of(rope, h) >= indent {
                        continue;
                    }
                    indent = indent_of(rope, h);
                    out.extend(indent_fold(rope, h));
                    if indent == 0 {
                        break;
                    }
                }
                out
            }
        };
        let mut keyed: Vec<((usize, usize), Fold)> =
            folds.drain(..).filter_map(|f| f.lines(rope).filter(|(h, l)| *h <= line && line <= *l).map(|l| (l, f))).collect();
        keyed.sort_by_key(|((h, l), f)| (l - h, *h, f.end - f.start));
        keyed.dedup_by_key(|(l, _)| *l);
        keyed.into_iter().map(|(_, f)| f).collect()
    }

    /// Collapse `f` in the live view; carets it hides move to the end of
    /// its header line. False when it hides nothing or is collapsed.
    pub fn collapse_fold(&mut self, f: Fold) -> bool {
        if !self.view.folds.collapse(f, &self.rope) {
            return false;
        }
        self.carets_out_of_folds();
        true
    }

    /// Move every caret end a fold hides to the end of the fold's header
    /// line.
    pub fn carets_out_of_folds(&mut self) {
        let rows = Rows::new(&self.view.folds, self.rope.len_lines());
        let rope = &self.rope;
        let out = |p: usize| {
            let line = rope.char_to_line(p.min(rope.len_chars()));
            if !rows.is_hidden(line) {
                return p;
            }
            let header = rows.visible(line);
            rope.line_to_char(header) + line_len_no_eol(rope, header)
        };
        if self.view.sels.iter().any(|s| out(s.head) != s.head || out(s.anchor) != s.anchor) {
            self.view.sels.map(out);
            self.view.block = None;
            self.undo.break_group = true;
        }
    }

    /// The largest region that can fold with its header on `line`.
    pub fn fold_at_header(&mut self, line: usize) -> Option<Fold> {
        let chain = self.fold_chain(line);
        chain.into_iter().rfind(|f| f.lines(&self.rope).is_some_and(|(h, _)| h == line))
    }

    /// The gutter marker: expand the fold on `line`, else collapse the one
    /// that can fold there.
    pub fn toggle_fold(&mut self, line: usize) -> bool {
        if self.view.folds.expand_at(line, &self.rope, false) {
            return true;
        }
        self.fold_at_header(line).is_some_and(|f| self.collapse_fold(f))
    }

    /// Expand the folds of the live view that hide one of its carets.
    pub fn reveal_carets(&mut self) -> bool {
        self.view.folds.reveal(&self.rope, &self.view.sels)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::editor::selection::Selection;

    fn spans(f: &Folds) -> Vec<(usize, usize)> {
        f.hidden().iter().map(|r| (r.start, r.end)).collect()
    }

    const SRC: &str = "fn main() {\n    let x = 1;\n    if x {\n        a();\n    } else {\n        b();\n    }\n}\nfn other() {}\n";

    fn fold_of(rope: &Rope, open: &str, close_line: usize, close: &str) -> Fold {
        let text = rope.to_string();
        let start = rope.byte_to_char(text.find(open).unwrap());
        let line = rope.line(close_line).to_string();
        let end = rope.line_to_char(close_line) + line.find(close).unwrap() + close.chars().count();
        Fold { start, end }
    }

    #[test]
    fn lines_keep_an_end_line_with_more_code() {
        let rope = Rope::from_str(SRC);
        // The whole body: the closing line goes too.
        assert_eq!(fold_of(&rope, "{\n    let", 7, "}").lines(&rope), Some((0, 7)));
        // `} else {` stays visible.
        assert_eq!(fold_of(&rope, "{\n        a()", 4, "}").lines(&rope), Some((2, 3)));
        // On one line, or two with the second kept: nothing to hide.
        assert_eq!(Fold { start: 0, end: 5 }.lines(&rope), None);
        let r = Rope::from_str("a {\n} b\n");
        assert_eq!(Fold { start: 2, end: 5 }.lines(&r), None);
        // Ending at a line start: the line before is the last.
        let r = Rope::from_str("// a\n// b\nx\n");
        assert_eq!(Fold { start: 0, end: 10 }.lines(&r), Some((0, 1)));
    }

    #[test]
    fn edits_map_anchors_and_drop_broken_folds() {
        let rope = Rope::from_str(SRC);
        let body = fold_of(&rope, "{\n    let", 7, "}");
        let mut f = Folds::default();
        assert!(f.collapse(body, &rope));
        assert!(!f.collapse(body, &rope), "already collapsed");
        assert_eq!(spans(&f), [(1, 8)]);
        let edit = |f: &mut Folds, at: usize, old: usize, new: &str| {
            let mut r = rope.clone();
            r.remove(at..at + old);
            r.insert(at, new);
            f.map_edit(at, old, new.chars().count());
            f.rebuild(&r);
            r
        };
        // Before: both anchors shift.
        let mut g = f.clone();
        edit(&mut g, 0, 0, "// hi\n");
        assert_eq!(spans(&g), [(2, 9)]);
        // Inside: the end moves; at the end: it stays.
        let mut g = f.clone();
        edit(&mut g, 20, 0, "\n\n");
        assert_eq!(spans(&g), [(1, 10)]);
        let mut g = f.clone();
        let r = edit(&mut g, body.end, 0, "\nfn x() {}");
        assert_eq!((g.iter().next().unwrap().0.end, spans(&g)), (body.end, vec![(1, 8)]));
        assert_eq!(r.line(8).to_string(), "fn x() {}\n");
        // A deletion swallowing the header's `{` drops it.
        let mut g = f.clone();
        edit(&mut g, body.start - 1, 3, "");
        assert!(g.is_empty());
        // Joining every hidden line into the header drops it.
        let mut g = f.clone();
        edit(&mut g, body.start + 1, body.end - body.start - 2, "");
        assert!(g.is_empty());
        // An insert at the header start pushes the header along.
        let mut g = f.clone();
        edit(&mut g, body.start, 0, "\n");
        assert_eq!(spans(&g), [(2, 9)]);
    }

    #[test]
    fn reveal_expand_and_session_round_trip() {
        let rope = Rope::from_str(SRC);
        let outer = fold_of(&rope, "{\n    let", 7, "}");
        let inner = fold_of(&rope, "{\n        a()", 4, "}");
        let mut f = Folds::default();
        f.collapse(inner, &rope);
        f.collapse(outer, &rope);
        assert_eq!(spans(&f), [(1, 8)]);
        assert_eq!(f.header_at(0).map(|(_, l)| l), Some(7));
        // A caret on the header line hides nothing.
        assert!(!f.reveal(&rope, &Selections::point(rope.line_to_char(1) - 1)));
        let saved = f.to_session(&rope);
        assert_eq!(Folds::from_session(&saved, &rope).hidden(), f.hidden());
        // Expanding the outer keeps the inner collapsed.
        let mut g = f.clone();
        assert!(g.expand_at(0, &rope, false));
        assert_eq!(spans(&g), [(3, 4)]);
        let mut g = f.clone();
        assert!(g.expand_at(0, &rope, true));
        assert!(g.is_empty());
        assert!(!g.expand_at(0, &rope, true));
        // A caret inside the inner reveals both; at the end of the last
        // hidden line too.
        let mut g = f.clone();
        assert!(g.reveal(&rope, &Selection::point(rope.line_to_char(3) + 2).into()));
        assert!(g.is_empty());
        let mut g = f.clone();
        assert!(g.reveal(&rope, &Selection::point(rope.line_to_char(7) + 1).into()));
        assert_eq!(spans(&g), [(3, 4)]);
        // Sessions clamp: a shorter text drops what no longer fits.
        let short = Rope::from_str("fn main() {\n}\n");
        assert!(Folds::from_session(&saved, &short).is_empty());
    }

    #[test]
    fn live_folds_follow_line_endings_undo_and_drop_on_reload() {
        use crate::editor::buffer::LineEnding;
        use crate::editor::undo::{Change, TxKind};
        let mut b = Buffer::from_text(1, None, SRC, false);
        let body = fold_of(&b.rope, "{\n    let", 7, "}");
        assert!(b.collapse_fold(body));
        b.convert_line_endings(LineEnding::CrLf);
        assert_eq!(spans(&b.view.folds), [(1, 8)]);
        b.convert_line_endings(LineEnding::Lf);
        assert!(b.undo());
        assert_eq!(spans(&b.view.folds), [(1, 8)]);
        let old = b.text();
        b.apply(vec![Change::replace(0, old, "x\n")], Selection::point(0), TxKind::Other);
        assert!(b.view.folds.is_empty(), "a reload drops them");
    }

    /// A buffer of `text` named `name`, with the syntax its name implies.
    fn buf(name: &str, text: &str) -> Buffer {
        let path = std::path::PathBuf::from(format!("/tmp/{name}"));
        let mut b = Buffer::from_text(1, Some(path.clone()), text, false);
        b.syntax = crate::editor::syntax::Syntax::for_path(&path, &crate::config::theme::Theme::default());
        b
    }

    /// The (header, last hidden line) of every candidate.
    fn cands(b: &mut Buffer) -> Vec<(usize, usize)> {
        let n = b.rope.len_lines();
        let rope = b.rope.clone();
        b.fold_candidates(0..n).iter().map(|f| f.lines(&rope).unwrap()).collect()
    }

    #[cfg(all(feature = "lang-md", feature = "lang-rust"))]
    #[test]
    fn injected_code_folds() {
        let mut b = buf("a.md", "# T\n\n```rust\nfn a() {\n    x();\n}\n```\n");
        assert!(cands(&mut b).iter().any(|(header, _)| *header == 3), "fold of `fn a() {{` from the inner tree");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_candidates() {
        let text = "\
use std::fmt;

use std::io;
/// Doc one.
/// Doc two.
struct S {
    a: u8,
}
impl S {
    fn f(&self) {
        match self.a {
            0 => {}
            _ => {}
        }
        if true {
            g();
        } else {
            h();
        }
    }
}
// lone
fn one() {}
/*
 * c
 */
";
        let mut b = buf("a.rs", text);
        assert_eq!(cands(&mut b), [(0, 2), (3, 4), (5, 7), (8, 20), (9, 19), (10, 13), (14, 15), (16, 18), (23, 25)]);
        // The chain at `g();`: the if block, the fn body, the impl.
        let rope = b.rope.clone();
        let chain: Vec<_> = b.fold_chain(15).iter().map(|f| f.lines(&rope).unwrap()).collect();
        assert_eq!(chain, [(14, 15), (9, 19), (8, 20)]);
        // A window only lists headers in it.
        let some: Vec<_> = b.fold_candidates(9..11).iter().map(|f| f.lines(&rope).unwrap()).collect();
        assert_eq!(some, [(9, 19), (10, 13)]);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn lone_cr_breaks_count_as_lines() {
        let lf = cands(&mut buf("a.rs", "fn a() {\n    x();\n}\n"));
        let cr = cands(&mut buf("a.rs", "fn a() {\r    x();\r}\r"));
        assert_eq!(cr, lf);
        assert_eq!(cr[0].0, 0);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_candidates() {
        let text = "\
import os
from sys import argv

class A:
    def f(self):
        if x:
            a()
        else:
            b()
";
        let mut b = buf("a.py", text);
        assert_eq!(cands(&mut b), [(0, 1), (3, 8), (4, 8), (5, 6), (7, 8)]);
    }

    #[cfg(feature = "lang-json")]
    #[test]
    fn json_candidates() {
        let mut b = buf("a.json", "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {\"c\": 1}\n}\n");
        assert_eq!(cands(&mut b), [(0, 6), (1, 4)]);
    }

    #[test]
    fn indentation_candidates_without_syntax() {
        let text = "a:\n  b\n\n  c:\n    d\n\ne\n  f\n";
        let mut b = buf("a.txt", text);
        assert!(b.syntax.is_none());
        assert_eq!(cands(&mut b), [(0, 4), (3, 4), (6, 7)]);
        let rope = b.rope.clone();
        let chain: Vec<_> = b.fold_chain(4).iter().map(|f| f.lines(&rope).unwrap()).collect();
        assert_eq!(chain, [(3, 4), (0, 4)]);
    }

    const PATCH_TEXT: &str = "== a1 one\ndiff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n same\n@@ -9 +9 @@\n+more\ndiff --git a/y b/y\n@@ -1 +1 @@\n-y\n+y2\n\n== b2 two\n(no changes)\n";

    fn patch_buf() -> Buffer {
        let mut b = buf("a.txt", PATCH_TEXT);
        b.patch = Some(crate::editor::patch::Patch::parse(PATCH_TEXT));
        b.fold_cache = None;
        b
    }

    #[test]
    fn patch_buffers_fold_by_section() {
        let mut b = patch_buf();
        assert_eq!(cands(&mut b), [(0, 13), (1, 9), (4, 7), (8, 9), (10, 13), (11, 13), (15, 16)]);
        let rope = b.rope.clone();
        let chain: Vec<_> = b.fold_chain(6).iter().map(|f| f.lines(&rope).unwrap()).collect();
        assert_eq!(chain, [(4, 7), (1, 9), (0, 13)]);
        assert!(b.fold_chain(14).is_empty());
    }

    #[test]
    fn patch_buffers_fold_recursively_and_all() {
        let mut b = patch_buf();
        b.view.set_sel(Selection::point(b.rope.line_to_char(1)));
        b.fold_collapse(true);
        assert_eq!(spans(&b.view.folds), [(2, 10)]);
        b.fold_expand(false);
        assert_eq!(spans(&b.view.folds), [(5, 8), (9, 10)]);
        b.view.folds.clear();
        b.fold_collapse_all();
        assert_eq!(spans(&b.view.folds), [(1, 14), (16, 17)]);
    }

    #[test]
    fn patch_buffers_get_no_indent_folds() {
        let text = "@@ -1,2 +1,2 @@\n-old\n+new\n same\n+more\n  deeper\n";
        let mut b = buf("a.txt", text);
        b.patch = Some(crate::editor::patch::Patch::parse(text));
        b.fold_cache = None;
        assert_eq!(cands(&mut b), [(0, 5)]);
    }

    /// Editor-level tests: rendering, motions, mouse and actions over folds.
    #[cfg(feature = "lang-rust")]
    pub(crate) mod editor {
        use std::path::PathBuf;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
        use ratatui::layout::Rect;

        use crate::actions::{EditorAction as E, Motion};
        use crate::app::Ctx;
        use crate::config::Config;
        use crate::config::theme::Theme;
        use crate::editor::Editor;
        use crate::editor::selection::Selection;
        use crate::event::{Effect, Mouse};
        use crate::workspace::Workspace;

        pub(crate) const TEXT: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n}\nfn two() {\n    x();\n}\nlast\n";

        pub(crate) fn editor(text: &str) -> Editor {
            let mut ed = Editor::new(&Config::default());
            ed.open_text(PathBuf::from("/tmp/a.rs"), text, false, &Theme::default());
            ed
        }

        pub(crate) fn act(ed: &mut Editor, a: E) -> Vec<Effect> {
            let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/tmp")));
            let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
            ed.handle_action(&a, &mut cx);
            cx.effects
        }

        pub(crate) fn mouse(ed: &mut Editor, kind: MouseEventKind, (x, y): (u16, u16), mods: KeyModifiers) {
            let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/tmp")));
            let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
            ed.handle_mouse(&Mouse { kind, x, y, clicks: 1, mods }, Rect::default(), &mut cx);
        }

        /// Draw at `w` x `h` (a tab row, then the text); the screen rows as
        /// strings.
        pub(crate) fn draw(ed: &mut Editor, w: u16, h: u16) -> Vec<String> {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            let theme = Theme::default();
            let buf = term.draw(|f| ed.render(f, Rect::new(0, 0, w, 1), Rect::new(0, 1, w, h - 1), true, &theme)).unwrap().buffer.clone();
            (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect()
        }

        /// Collapse the innermost fold holding `line`.
        pub(crate) fn collapse(ed: &mut Editor, line: usize) {
            let b = ed.active_buffer_mut().unwrap();
            let f = b.fold_chain(line)[0];
            assert!(b.view.folds.collapse(f, &b.rope), "{f:?} {:?} {:?}", f.lines(&b.rope), b.text());
        }

        fn head(ed: &Editor) -> (usize, usize) {
            ed.active_buffer().unwrap().cursor_line_col()
        }

        fn folded(ed: &Editor) -> bool {
            !ed.active_buffer().unwrap().view.folds.is_empty()
        }

        #[test]
        fn folded_body_renders_its_placeholder_and_skips_lines() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            let rows = draw(&mut ed, 40, 6);
            let x0 = ed.text_area().x as usize;
            assert_eq!((x0, rows[1].trim_end()), (7, "  1  ▸ fn main() {…}"));
            assert_eq!(rows[2].trim_end(), "  5  ▾ fn two() {");
        }

        #[test]
        fn vertical_motions_skip_hidden_lines_and_keep_the_column() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(0, 5);
            act(&mut ed, E::Move(Motion::Down, false));
            assert_eq!(head(&ed), (4, 5));
            act(&mut ed, E::Move(Motion::Down, false));
            act(&mut ed, E::Move(Motion::Up, false));
            assert_eq!(head(&ed), (4, 5), "the desired column survives the short line");
            act(&mut ed, E::Move(Motion::Up, false));
            assert_eq!(head(&ed), (0, 5));
            assert!(folded(&ed));
            // Five rows of text: a page is four visible lines.
            draw(&mut ed, 40, 6);
            act(&mut ed, E::Move(Motion::PageDown, false));
            assert_eq!(head(&ed).0, 7);
            assert!(folded(&ed));
        }

        #[test]
        fn landing_in_a_fold_unfolds_it() {
            // Right from the header's end.
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(0, 11);
            act(&mut ed, E::Move(Motion::Right, false));
            assert_eq!((head(&ed), folded(&ed)), ((1, 0), false));
            // Enter at the header's end.
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(0, 11);
            act(&mut ed, E::InsertNewline);
            assert_eq!((head(&ed).0, folded(&ed)), (1, false));
            // Undo puts the caret back inside.
            let mut ed = editor(TEXT);
            ed.active_buffer_mut().unwrap().goto_line(2, 4);
            act(&mut ed, E::InsertChar('z'));
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(7, 0);
            act(&mut ed, E::Undo);
            assert_eq!((head(&ed), folded(&ed)), ((2, 4), false));
            // A jump from elsewhere (go to line, F2) unfolds on the next frame.
            collapse(&mut ed, 1);
            ed.goto_line(3, 1);
            assert!(folded(&ed));
            draw(&mut ed, 40, 6);
            assert!(!folded(&ed));
            // Find next.
            let mut ed = editor(TEXT);
            act(&mut ed, E::Find);
            let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/tmp")));
            let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
            for k in "let b".chars().map(KeyCode::Char).chain([KeyCode::Esc]) {
                ed.handle_find_key(KeyEvent::new(k, KeyModifiers::NONE), &mut cx);
            }
            ed.active_buffer_mut().unwrap().goto_line(0, 0);
            collapse(&mut ed, 1);
            act(&mut ed, E::FindNext);
            assert_eq!((head(&ed).0, folded(&ed)), (2, false));
        }

        #[test]
        fn mouse_rows_map_past_folds() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            draw(&mut ed, 40, 6);
            let (x0, y0) = (ed.text_area().x, ed.text_area().y);
            let down = MouseEventKind::Down(MouseButton::Left);
            mouse(&mut ed, down, (x0 + 3, y0 + 1), KeyModifiers::NONE);
            assert_eq!(head(&ed), (4, 3), "the row below the fold is the line after it");
            mouse(&mut ed, down, (x0, y0 + 5), KeyModifiers::NONE);
            assert_eq!(head(&ed).0, 8, "below the text: the last line");
            // A drag across the fold selects through it and keeps it.
            mouse(&mut ed, down, (x0 + 3, y0), KeyModifiers::NONE);
            mouse(&mut ed, MouseEventKind::Drag(MouseButton::Left), (x0 + 2, y0 + 2), KeyModifiers::NONE);
            let b = ed.active_buffer().unwrap();
            assert_eq!(b.view.sel(), Selection::new(3, b.rope.line_to_char(5) + 2));
            assert!(folded(&ed));
            // An Alt+drag block skips the hidden lines.
            mouse(&mut ed, down, (x0 + 1, y0), KeyModifiers::ALT);
            mouse(&mut ed, MouseEventKind::Drag(MouseButton::Left), (x0 + 2, y0 + 2), KeyModifiers::ALT);
            let b = ed.active_buffer().unwrap();
            let lines: Vec<usize> = b.view.sels.iter().map(|s| b.rope.char_to_line(s.head)).collect();
            assert_eq!(lines, [0, 4, 5]);
            assert!(folded(&ed));
        }

        #[test]
        fn gutter_markers_aggregate_and_clicks_toggle() {
            let mut ed = editor(TEXT);
            let b = ed.active_buffer_mut().unwrap();
            b.diag_marks.lines.insert(2, crate::lsp::diagnostics::Severity::Error);
            b.diag_marks.lines.insert(5, crate::lsp::diagnostics::Severity::Warning);
            b.bookmarks.insert(1, None);
            let rows = draw(&mut ed, 40, 8);
            let col = |rows: &[String], y: usize, x: u16| rows[y].chars().nth(x as usize).unwrap();
            let (x0, y0) = (ed.text_area().x, ed.text_area().y as usize);
            assert_eq!((col(&rows, y0, x0 - 2), col(&rows, y0 + 4, x0 - 2), col(&rows, y0 + 1, x0 - 2)), ('▾', '▾', ' '));
            // A marker click collapses; the header shows what the fold hides.
            mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (x0 - 2, y0 as u16), KeyModifiers::NONE);
            let rows = draw(&mut ed, 40, 8);
            assert_eq!((col(&rows, y0, x0 - 2), col(&rows, y0, x0 - 3), col(&rows, y0, x0 - 4)), ('▸', '✖', '◆'));
            assert_eq!(col(&rows, y0 + 2, x0 - 3), '⚠', "line 6 is the second row below the fold");
            // Again expands.
            mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (x0 - 2, y0 as u16), KeyModifiers::NONE);
            assert!(!folded(&ed));
            // A click on the placeholder expands, the caret at the header end.
            collapse(&mut ed, 1);
            draw(&mut ed, 40, 8);
            mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (x0 + 12, y0 as u16), KeyModifiers::NONE);
            assert_eq!((folded(&ed), head(&ed)), (false, (0, 11)));
            // A click on the header text does not.
            collapse(&mut ed, 1);
            draw(&mut ed, 40, 8);
            mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (x0 + 3, y0 as u16), KeyModifiers::NONE);
            assert_eq!((folded(&ed), head(&ed)), (true, (0, 3)));
        }

        #[test]
        fn preview_tabs_do_not_fold_on_a_marker_click() {
            let mut ed = editor(TEXT);
            let source = ed.active_buffer().unwrap().id;
            let rendered = crate::ui::markdown::Rendered { text: "Head\n  body one\n  body two\nTail".into(), spans: Vec::new() };
            ed.open_markdown_preview(source, rendered, &Theme::default());
            assert!(ed.active_buffer().unwrap().preview.is_some());
            assert!(ed.active_buffer_mut().unwrap().fold_at_header(0).is_some(), "the text has a foldable block");
            draw(&mut ed, 40, 8);
            let (x0, y0) = (ed.text_area().x, ed.text_area().y);
            let before = head(&ed);
            mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (x0 - 2, y0), KeyModifiers::NONE);
            assert_eq!((folded(&ed), head(&ed)), (false, before));
        }

        #[test]
        fn hints_on_a_folded_header_come_before_the_placeholder() {
            use crate::editor::inlay::Inlay;
            let mut ed = editor(TEXT);
            // Line 0 is `fn main() {` (11 chars); line 1 is hidden by the fold.
            let hint = |at: usize, l: &str| Inlay::new(at, l, false, false, false).unwrap();
            let line1_end = TEXT.find('\n').unwrap() + 1 + "    let a = 1;".len();
            ed.active_buffer_mut().unwrap().inlays.set(vec![hint(11, "-> T"), hint(line1_end, "HIDDEN")], 0, 0..8);
            collapse(&mut ed, 1);
            let rows = draw(&mut ed, 40, 6);
            assert_eq!(rows[1].trim_end(), "  1  ▸ fn main() {-> T…}");
            assert!(rows.iter().all(|r| !r.contains("HIDDEN")), "{rows:?}");
            // A click on the placeholder (after the hint) still expands.
            let (x0, y0) = (ed.text_area().x, ed.text_area().y);
            mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (x0 + 11 + 4, y0), KeyModifiers::NONE);
            assert!(!folded(&ed));
            assert_eq!(head(&ed), (0, 11));
            // A click on the hint itself does not.
            collapse(&mut ed, 1);
            draw(&mut ed, 40, 6);
            mouse(&mut ed, MouseEventKind::Down(MouseButton::Left), (x0 + 12, y0), KeyModifiers::NONE);
            assert!(folded(&ed));
            assert_eq!(head(&ed), (0, 11));
        }

        fn hidden(ed: &Editor) -> Vec<(usize, usize)> {
            ed.active_buffer().unwrap().view.folds.hidden().iter().map(|r| (r.start, r.end)).collect()
        }

        #[test]
        fn collapse_expand_actions() {
            let text = "impl A {\n    fn f() {\n        if x {\n            y();\n        }\n    }\n}\nfn g() {\n    z();\n}\n";
            let mut ed = editor(text);
            ed.active_buffer_mut().unwrap().goto_line(3, 4);
            act(&mut ed, E::FoldCollapse);
            assert_eq!(hidden(&ed), [(3, 5)]);
            assert_eq!(head(&ed), (2, 14), "the caret leaves the fold for its header's end");
            // On a folded line the next one out goes.
            act(&mut ed, E::FoldCollapse);
            assert_eq!(hidden(&ed), [(2, 6)]);
            act(&mut ed, E::FoldExpand);
            assert_eq!(hidden(&ed), [(3, 5)], "expanding the outer keeps the inner folded");
            act(&mut ed, E::FoldExpandAll);
            assert!(!folded(&ed));
            // Recursively: the region and those inside.
            ed.active_buffer_mut().unwrap().goto_line(1, 0);
            act(&mut ed, E::FoldCollapseRecursive);
            assert_eq!(hidden(&ed), [(2, 6)]);
            assert_eq!(ed.active_buffer().unwrap().view.folds.iter().count(), 2);
            act(&mut ed, E::FoldExpandRecursive);
            assert!(!folded(&ed));
            act(&mut ed, E::FoldCollapseAll);
            assert_eq!(hidden(&ed), [(1, 7), (8, 10)]);
            act(&mut ed, E::FoldExpand);
            act(&mut ed, E::FoldExpandAll);
            assert!(!folded(&ed));
            // Fold Selection: the selected lines; again on the header expands.
            ed.active_buffer_mut().unwrap().view.set_sel(Selection::new(0, 30));
            act(&mut ed, E::FoldSelection);
            assert_eq!(hidden(&ed), [(1, 2)], "line 2 has more after the selection");
            ed.active_buffer_mut().unwrap().goto_line(0, 0);
            act(&mut ed, E::FoldSelection);
            assert!(!folded(&ed));
        }

        #[test]
        fn split_views_fold_on_their_own() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            act(&mut ed, E::SplitRight);
            assert_eq!(hidden(&ed), [(1, 4)], "the new view starts with the same folds");
            act(&mut ed, E::FoldExpandAll);
            draw(&mut ed, 80, 8);
            act(&mut ed, E::NextGroup);
            draw(&mut ed, 80, 8);
            assert_eq!(hidden(&ed), [(1, 4)], "the first view keeps its fold");
            // Edits in this view move the other one's folds along.
            ed.active_buffer_mut().unwrap().goto_line(7, 0);
            act(&mut ed, E::NextGroup);
            ed.active_buffer_mut().unwrap().goto_line(0, 0);
            act(&mut ed, E::InsertNewline);
            draw(&mut ed, 80, 8);
            act(&mut ed, E::NextGroup);
            draw(&mut ed, 80, 8);
            assert_eq!(hidden(&ed), [(2, 5)]);
        }

        #[test]
        fn expanding_keeps_the_caret_in_view() {
            let text = format!("fn a() {{\n{}}}\n{}", "    x;\n".repeat(40), "y\n".repeat(20));
            let mut ed = editor(&text);
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(42, 0);
            draw(&mut ed, 40, 8);
            assert_eq!(ed.active_buffer().unwrap().view.scroll_line, 0, "right below the fold");
            act(&mut ed, E::FoldExpandAll);
            draw(&mut ed, 40, 8);
            assert!(ed.focused_group().caret_cell.is_some(), "the caret did not drop off the screen");
        }

        #[test]
        fn scrolling_counts_visible_rows() {
            let text = format!("fn a() {{\n{}}}\n{}", "    x;\n".repeat(40), "y\n".repeat(20));
            let mut ed = editor(&text);
            collapse(&mut ed, 1);
            draw(&mut ed, 40, 8);
            // Down from the header lands right below it: no scroll.
            act(&mut ed, E::Move(Motion::Down, false));
            draw(&mut ed, 40, 8);
            let b = ed.active_buffer().unwrap();
            assert_eq!((b.cursor_line_col().0, b.view.scroll_line), (42, 0));
            assert_eq!(ed.focused_group().caret_cell.map(|c| c.1), Some(ed.text_area().y + 1));
            // The wheel steps over the fold, and never stops inside it.
            mouse(&mut ed, MouseEventKind::ScrollDown, (10, 3), KeyModifiers::NONE);
            assert_eq!(ed.active_buffer().unwrap().view.scroll_line, 44);
            mouse(&mut ed, MouseEventKind::ScrollUp, (10, 3), KeyModifiers::NONE);
            mouse(&mut ed, MouseEventKind::ScrollUp, (10, 3), KeyModifiers::NONE);
            assert_eq!(ed.active_buffer().unwrap().view.scroll_line, 0);
            // A caret far below scrolls by visible rows too.
            ed.active_buffer_mut().unwrap().goto_line(60, 0);
            draw(&mut ed, 40, 8);
            let (top, ta) = (ed.active_buffer().unwrap().view.scroll_line, ed.text_area());
            assert!(top > 42);
            assert_eq!(ed.focused_group().caret_cell.map(|c| c.1 - ta.y), Some((60 - top) as u16));
        }

        fn text(ed: &Editor) -> String {
            ed.active_buffer().unwrap().text()
        }

        #[test]
        fn delete_line_on_a_folded_header_deletes_the_block() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(0, 3);
            act(&mut ed, E::DeleteLine);
            assert_eq!(text(&ed), "fn two() {\n    x();\n}\nlast\n");
            assert_eq!(head(&ed), (0, 3));
            assert!(!folded(&ed));
        }

        #[test]
        fn duplicate_line_on_a_folded_header_copies_the_block_and_keeps_both_folded() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(0, 3);
            act(&mut ed, E::DuplicateLine);
            let main = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
            assert_eq!(text(&ed), format!("{main}{main}fn two() {{\n    x();\n}}\nlast\n"));
            assert_eq!(hidden(&ed), [(1, 4), (5, 8)]);
            assert_eq!(head(&ed), (4, 3), "the caret is on the copy's header");
        }

        #[test]
        fn duplicate_line_counts_the_lines_an_earlier_selection_copy_adds() {
            use crate::editor::selection::{Selection, Selections};
            let mut ed = editor(&format!("a\nb\n{TEXT}"));
            collapse(&mut ed, 2);
            let b = ed.active_buffer_mut().unwrap();
            let caret = b.rope.line_to_char(2) + 3;
            b.view.sels = Selections::new(vec![Selection::new(0, 3), Selection::point(caret)], 1);
            act(&mut ed, E::DuplicateLine);
            let main = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
            assert_eq!(text(&ed), format!("a\nba\nb\n{main}{main}fn two() {{\n    x();\n}}\nlast\n"));
            assert_eq!(hidden(&ed), [(4, 7), (8, 11)], "both fold the main block, no other text");
        }

        #[test]
        fn duplicate_line_leaves_a_selection_copy_inside_a_block_expanded() {
            use crate::editor::selection::{Selection, Selections};
            let mut ed = editor(&format!("a\n{TEXT}"));
            collapse(&mut ed, 1);
            let b = ed.active_buffer_mut().unwrap();
            let (header, inner) = (b.rope.line_to_char(1) + 3, b.rope.line_to_char(2) + 4);
            b.view.sels = Selections::new(vec![Selection::point(header), Selection::new(inner, inner + 3)], 0);
            act(&mut ed, E::DuplicateLine);
            let main = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
            assert_eq!(
                text(&ed),
                format!("a\nfn main() {{\n    letlet a = 1;\n    let b = 2;\n}}\n{main}fn two() {{\n    x();\n}}\nlast\n")
            );
            assert_eq!(hidden(&ed), [], "the copy beside the selection copy stays expanded, no fold moves");
        }

        /// Select `from..to` in the first buffer line range and type a char.
        fn type_over(ed: &mut Editor, from: usize, to: usize, c: char) {
            use crate::editor::selection::Selection;
            ed.active_buffer_mut().unwrap().view.sels = Selection::new(from, to).into();
            act(ed, E::InsertChar(c));
        }

        #[test]
        fn undo_restores_a_fold_the_edit_dropped_and_redo_drops_it_again() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 0);
            assert_eq!(hidden(&ed), [(1, 4)]);
            type_over(&mut ed, 5, 11, 'x');
            assert_eq!(hidden(&ed), [], "the edit drops the fold");
            act(&mut ed, E::Undo);
            assert_eq!(text(&ed), TEXT);
            assert_eq!(hidden(&ed), [(1, 4)], "undo collapses it again");
            act(&mut ed, E::Redo);
            assert_eq!(hidden(&ed), [], "redo drops it again");
        }

        #[test]
        fn one_undo_restores_a_fold_dropped_by_the_second_of_two_merged_typing_edits() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 0);
            type_over(&mut ed, 0, 0, 'q');
            assert_eq!(hidden(&ed), [(1, 4)]);
            type_over(&mut ed, 1, 15, 'x');
            assert_eq!(hidden(&ed), []);
            act(&mut ed, E::Undo);
            assert_eq!(text(&ed), TEXT, "both edits are one transaction");
            assert_eq!(hidden(&ed), [(1, 4)]);
        }

        #[test]
        fn undo_of_a_reload_restores_the_cleared_folds() {
            use crate::editor::selection::Selection;
            use crate::editor::undo::Change;
            use crate::editor::undo::TxKind;
            let mut ed = editor(TEXT);
            collapse(&mut ed, 0);
            let b = ed.active_buffer_mut().unwrap();
            b.apply(vec![Change { at: 0, old: TEXT.into(), new: TEXT.replace("two", "three") }], Selection::point(0), TxKind::Other);
            assert_eq!(hidden(&ed), [], "the reload clears the folds");
            act(&mut ed, E::Undo);
            assert_eq!(text(&ed), TEXT);
            assert_eq!(hidden(&ed), [(1, 4)]);
        }

        #[test]
        fn move_line_down_jumps_over_a_folded_neighbour() {
            let mut ed = editor("a\nfn main() {\n    let a = 1;\n    let b = 2;\n}\nz\n");
            collapse(&mut ed, 2);
            ed.active_buffer_mut().unwrap().goto_line(0, 1);
            act(&mut ed, E::MoveLineDown);
            assert_eq!(text(&ed), "fn main() {\n    let a = 1;\n    let b = 2;\n}\na\nz\n");
            assert_eq!(head(&ed), (4, 1));
            assert_eq!(hidden(&ed), [(1, 4)], "the fold stays collapsed");
        }

        #[test]
        fn move_folded_block_up_keeps_it_collapsed() {
            let mut ed = editor("a\nb\nfn main() {\n    let a = 1;\n    let b = 2;\n}\nz\n");
            collapse(&mut ed, 3);
            ed.active_buffer_mut().unwrap().goto_line(2, 2);
            act(&mut ed, E::MoveLineUp);
            assert_eq!(text(&ed), "a\nfn main() {\n    let a = 1;\n    let b = 2;\n}\nb\nz\n");
            assert_eq!(head(&ed), (1, 2));
            assert_eq!(hidden(&ed), [(2, 5)]);
            // Past a folded block upwards, the neighbour moves down whole.
            ed.active_buffer_mut().unwrap().goto_line(5, 0);
            act(&mut ed, E::MoveLineUp);
            assert_eq!(text(&ed), "a\nb\nfn main() {\n    let a = 1;\n    let b = 2;\n}\nz\n");
            assert_eq!(hidden(&ed), [(3, 6)]);
        }

        #[test]
        fn select_line_on_a_folded_header_selects_the_block_without_expanding() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(0, 3);
            act(&mut ed, E::SelectLine);
            let b = ed.active_buffer().unwrap();
            assert_eq!(b.rope.slice(b.view.sel().range()).to_string(), "fn main() {\n    let a = 1;\n    let b = 2;\n}\n");
            assert!(folded(&ed));
            draw(&mut ed, 40, 6);
            assert!(folded(&ed), "drawing does not reveal it either");
        }

        #[test]
        fn select_all_keeps_folds_collapsed() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            collapse(&mut ed, 5);
            act(&mut ed, E::SelectAll);
            assert_eq!(hidden(&ed), [(1, 4), (5, 7)]);
            // A caret inside hidden text still expands.
            ed.active_buffer_mut().unwrap().goto_line(2, 0);
            act(&mut ed, E::Move(Motion::Right, false));
            assert_eq!(hidden(&ed), [(5, 7)]);
        }

        #[test]
        fn comment_lone_caret_on_a_header_steps_past_the_fold() {
            let mut ed = editor(TEXT);
            collapse(&mut ed, 1);
            ed.active_buffer_mut().unwrap().goto_line(0, 2);
            act(&mut ed, E::ToggleComment);
            assert_eq!(text(&ed), "// fn main() {\n//     let a = 1;\n//     let b = 2;\n// }\nfn two() {\n    x();\n}\nlast\n");
            assert_eq!(head(&ed), (4, 5));
            assert!(folded(&ed));
        }
    }
}
