//! Code completion, the parts that do not need the app: where a request
//! was made ([`Origin`]) and whether its answer still applies, the list
//! filtered by what was typed since ([`CompletionState`], fuzzy matched by
//! nucleo), snippets parsed to text and tabstops ([`parse_snippet`]), and
//! the edit accepting an item makes ([`edit_for`]) with the snippet's
//! tabstops where it put them (walked by `editor::snippet`).
//!
//! A list is used while only identifier chars are typed or deleted at the
//! caret: the caret stays in the word the list was asked for, and the text
//! after it keeps its length. Then every position the server gave is still
//! good: those before the request position are unchanged, those after it
//! moved with the caret (`map`), so an answer that arrives a few keystrokes
//! late is filtered by the longer prefix instead of being thrown away.

use std::cmp::Reverse;

use lsp_types::{CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, InsertTextFormat, Position};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use ropey::Rope;
use serde_json::Value;

use super::edits::{self, CharEdit};
use super::pos;
use crate::editor::buffer::BufferId;
use crate::editor::selection::is_word_char;
use crate::editor::snippet::Tabstop;
use crate::editor::undo::Change;

/// Where a completion request was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub buffer: BufferId,
    /// Char index where the word being completed starts (the caret, after
    /// a trigger char such as `.`).
    pub start: usize,
    /// The caret at the request, as a char index and an LSP position.
    pub caret: usize,
    pub pos: Position,
    /// Chars after the caret at the request.
    pub tail: usize,
}

impl Origin {
    pub fn new(buffer: BufferId, rope: &Rope, caret: usize) -> Self {
        let caret = caret.min(rope.len_chars());
        Self { buffer, start: word_start(rope, caret), caret, pos: pos::position(rope, caret), tail: rope.len_chars() - caret }
    }

    /// The word typed from `start` to `caret` in `rope`, when a list asked
    /// for here still applies: the caret is in the same word, at or after
    /// its start, and the text after the caret has its length.
    pub fn prefix(&self, rope: &Rope, caret: usize) -> Option<String> {
        let len = rope.len_chars();
        if caret < self.start || caret > len || len - caret != self.tail || word_start(rope, caret) != self.start {
            return None;
        }
        Some(rope.slice(self.start..caret).to_string())
    }
}

/// Where the word that ends at `at` starts (`at` itself after a non-word
/// char).
pub fn word_start(rope: &Rope, at: usize) -> usize {
    let mut s = at.min(rope.len_chars());
    while s > 0 && is_word_char(rope.char(s - 1)) {
        s -= 1;
    }
    s
}

/// The items of a `textDocument/completion` answer and whether the list is
/// incomplete (asked again as typing goes on). Null and junk are empty.
pub fn parse(value: Value) -> (Vec<CompletionItem>, bool) {
    match serde_json::from_value::<Option<CompletionResponse>>(value) {
        Ok(Some(CompletionResponse::Array(items))) => (items, false),
        Ok(Some(CompletionResponse::List(l))) => (l.items, l.is_incomplete),
        Ok(None) => (Vec::new(), false),
        Err(e) => {
            tracing::debug!("bad completion answer: {e}");
            (Vec::new(), false)
        }
    }
}

/// The text an item is matched against.
fn filter_text(item: &CompletionItem) -> &str {
    item.filter_text.as_deref().unwrap_or(&item.label)
}

/// The server's order for an item.
fn sort_text(item: &CompletionItem) -> &str {
    item.sort_text.as_deref().unwrap_or(&item.label)
}

/// Indices of the items matching `prefix`, best first. Items that start
/// with the prefix (exact case first) come before other fuzzy matches; in
/// each group the better nucleo score, then the server's `sortText`, then
/// the server's order. An empty prefix keeps every item in `sortText` order.
pub fn filter(items: &[CompletionItem], prefix: &str, matcher: &mut Matcher) -> Vec<usize> {
    if prefix.is_empty() {
        let mut all: Vec<usize> = (0..items.len()).collect();
        all.sort_by(|&a, &b| sort_text(&items[a]).cmp(sort_text(&items[b])).then(a.cmp(&b)));
        return all;
    }
    matcher.config = nucleo_matcher::Config::DEFAULT;
    let pattern = Pattern::new(prefix, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy);
    let lower = prefix.to_lowercase();
    let mut buf = Vec::new();
    let mut scored: Vec<(u8, u32, usize)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let text = filter_text(item);
        let Some(score) = pattern.score(Utf32Str::new(text, &mut buf), matcher) else { continue };
        let group = if text.starts_with(prefix) {
            0
        } else if text.to_lowercase().starts_with(&lower) {
            1
        } else {
            2
        };
        scored.push((group, score, i));
    }
    scored.sort_by(|a, b| {
        (a.0, Reverse(a.1)).cmp(&(b.0, Reverse(b.1))).then_with(|| sort_text(&items[a.2]).cmp(sort_text(&items[b.2]))).then(a.2.cmp(&b.2))
    });
    scored.into_iter().map(|(_, _, i)| i).collect()
}

/// A completion list: the items of the answer for `origin`, filtered by
/// the word typed so far, and the selected row.
#[derive(Debug, Clone)]
pub struct CompletionState {
    pub origin: Origin,
    pub items: Vec<CompletionItem>,
    /// The server said further typing changes the list: it is asked again
    /// whenever the prefix changes.
    pub incomplete: bool,
    /// What the list is filtered by (`origin.start` to the caret).
    pub prefix: String,
    /// Indices into `items` that match, best first.
    pub order: Vec<usize>,
    /// Index into `order`.
    pub selected: usize,
}

impl CompletionState {
    pub fn new(origin: Origin, items: Vec<CompletionItem>, incomplete: bool, prefix: &str, matcher: &mut Matcher) -> Self {
        let order = filter(&items, prefix, matcher);
        let selected = order.iter().position(|&i| items[i].preselect == Some(true)).unwrap_or(0);
        Self { origin, items, incomplete, prefix: prefix.to_string(), order, selected }
    }

    /// Filter by a new prefix, keeping the selected item when it still
    /// matches.
    pub fn refilter(&mut self, prefix: &str, matcher: &mut Matcher) {
        let was = self.order.get(self.selected).copied();
        self.prefix = prefix.to_string();
        self.order = filter(&self.items, prefix, matcher);
        self.selected = was.and_then(|w| self.order.iter().position(|&i| i == w)).unwrap_or(0);
    }

    /// Char indices of `items[i]`'s label that the prefix matched (for
    /// highlighting); empty without a prefix or a match. Always against the
    /// label, whatever `filterText` is.
    pub fn match_indices(&self, i: usize, matcher: &mut Matcher) -> Vec<u32> {
        let mut idx = Vec::new();
        if self.prefix.is_empty() {
            return idx;
        }
        matcher.config = nucleo_matcher::Config::DEFAULT;
        let pattern = Pattern::new(&self.prefix, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy);
        let mut buf = Vec::new();
        if pattern.indices(Utf32Str::new(&self.items[i].label, &mut buf), matcher, &mut idx).is_none() {
            idx.clear();
        }
        idx.sort_unstable();
        idx.dedup();
        idx
    }

    pub fn selected_item(&self) -> Option<&CompletionItem> {
        self.order.get(self.selected).map(|&i| &self.items[i])
    }

    /// Move the selection by `delta` rows; `wrap` goes around the ends
    /// (Up/Down), else it stops there (PgUp/PgDn).
    pub fn step(&mut self, delta: isize, wrap: bool) {
        let n = self.order.len() as isize;
        if n == 0 {
            return;
        }
        let to = self.selected as isize + delta;
        self.selected = if wrap { to.rem_euclid(n) } else { to.clamp(0, n - 1) } as usize;
    }
}

/// One-cell glyph for an item's kind.
pub fn kind_glyph(kind: Option<CompletionItemKind>) -> &'static str {
    const GLYPHS: &[(CompletionItemKind, &str)] = &[
        (CompletionItemKind::TEXT, "t"),
        (CompletionItemKind::METHOD, "m"),
        (CompletionItemKind::FUNCTION, "f"),
        (CompletionItemKind::CONSTRUCTOR, "c"),
        (CompletionItemKind::FIELD, "d"),
        (CompletionItemKind::VARIABLE, "v"),
        (CompletionItemKind::CLASS, "C"),
        (CompletionItemKind::INTERFACE, "I"),
        (CompletionItemKind::MODULE, "M"),
        (CompletionItemKind::PROPERTY, "p"),
        (CompletionItemKind::UNIT, "u"),
        (CompletionItemKind::VALUE, "v"),
        (CompletionItemKind::ENUM, "E"),
        (CompletionItemKind::KEYWORD, "k"),
        (CompletionItemKind::SNIPPET, "s"),
        (CompletionItemKind::COLOR, "#"),
        (CompletionItemKind::FILE, "/"),
        (CompletionItemKind::REFERENCE, "&"),
        (CompletionItemKind::FOLDER, "/"),
        (CompletionItemKind::ENUM_MEMBER, "e"),
        (CompletionItemKind::CONSTANT, "K"),
        (CompletionItemKind::STRUCT, "S"),
        (CompletionItemKind::EVENT, "!"),
        (CompletionItemKind::OPERATOR, "o"),
        (CompletionItemKind::TYPE_PARAMETER, "T"),
    ];
    kind.and_then(|k| GLYPHS.iter().find(|(g, _)| *g == k)).map(|(_, s)| *s).unwrap_or(" ")
}

/// Snippet text as plain text and its tabstops: placeholders inlined
/// (nested ones too), the first of a choice, a variable's default (else
/// nothing), escapes resolved; text that is not snippet syntax stays as it
/// is. Each `$n`, `${n}`, `${n:placeholder}` and `${n|choice|}` is a
/// [`Tabstop`] over its text, in chars of the returned text, in the order
/// they appear.
pub fn parse_snippet(s: &str) -> (String, Vec<Tabstop>) {
    let mut p = Snippet { c: s.chars().collect(), i: 0, out: String::new(), n: 0, stops: Vec::new(), open: Vec::new() };
    p.body(false);
    (p.out, p.stops)
}

struct Snippet {
    c: Vec<char>,
    i: usize,
    out: String,
    /// Chars in `out`.
    n: usize,
    stops: Vec<Tabstop>,
    /// Indices into `stops` of the placeholders being parsed, innermost last.
    open: Vec<usize>,
}

impl Snippet {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn push(&mut self, ch: char) {
        self.out.push(ch);
        self.n += 1;
    }

    fn number(&mut self) -> Option<u32> {
        let from = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        (self.i > from).then(|| self.c[from..self.i].iter().collect::<String>().parse().unwrap_or(u32::MAX))
    }

    fn name(&mut self) -> bool {
        if !self.peek().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
            return false;
        }
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            self.i += 1;
        }
        true
    }

    /// Record an empty tabstop `n` here; returns its index.
    fn tabstop(&mut self, n: u32) -> usize {
        self.stops.push(Tabstop { n, range: self.n..self.n, parent: self.open.last().copied() });
        self.stops.len() - 1
    }

    /// Text up to the end, or up to an unescaped `}` when `nested` (left
    /// for the caller).
    fn body(&mut self, nested: bool) {
        while let Some(ch) = self.peek() {
            match ch {
                '}' if nested => return,
                '\\' if self.c.get(self.i + 1).is_some_and(|n| matches!(n, '$' | '}' | '\\')) => {
                    self.push(self.c[self.i + 1]);
                    self.i += 2;
                }
                '$' => self.dollar(),
                _ => {
                    self.push(ch);
                    self.i += 1;
                }
            }
        }
    }

    /// `$1`, `$name`, `${1}`, `${1:text}`, `${1|a,b|}`, `${name}`,
    /// `${name:default}`, `${name/re/fmt/}`; anything else is a plain `$`.
    fn dollar(&mut self) {
        let at = Mark { at: self.i, len: self.out.len(), n: self.n, stops: self.stops.len(), open: self.open.len() };
        self.i += 1;
        if let Some(n) = self.number() {
            self.tabstop(n);
            return;
        }
        if self.name() {
            return;
        }
        if self.peek() != Some('{') {
            return self.literal(at);
        }
        self.i += 1;
        if let Some(n) = self.number() {
            match self.peek() {
                Some('}') => {
                    self.i += 1;
                    self.tabstop(n);
                }
                Some(':') => {
                    self.i += 1;
                    let k = self.tabstop(n);
                    self.open.push(k);
                    self.body(true);
                    self.open.pop();
                    if self.peek() != Some('}') {
                        return self.literal(at);
                    }
                    self.i += 1;
                    self.stops[k].range.end = self.n;
                }
                Some('|') => {
                    self.i += 1;
                    let k = self.tabstop(n);
                    if self.choice() {
                        self.stops[k].range.end = self.n;
                    } else {
                        self.literal(at);
                    }
                }
                _ => self.literal(at),
            }
        } else if self.name() {
            match self.peek() {
                Some('}') => self.i += 1,
                Some(':') => {
                    self.i += 1;
                    self.body(true);
                    if self.peek() != Some('}') {
                        return self.literal(at);
                    }
                    self.i += 1;
                }
                Some('/') => {
                    // A transform: nothing to insert for an unknown variable.
                    while let Some(ch) = self.peek() {
                        self.i += if ch == '\\' { 2 } else { 1 };
                        if ch == '}' {
                            return;
                        }
                    }
                    self.literal(at);
                }
                _ => self.literal(at),
            }
        } else {
            self.literal(at);
        }
    }

    /// `a,b|}` after `${1|`: the first option. False when malformed.
    fn choice(&mut self) -> bool {
        let mut first = true;
        while let Some(ch) = self.peek() {
            match ch {
                '\\' if self.c.get(self.i + 1).is_some_and(|n| matches!(n, '$' | '}' | '\\' | ',' | '|')) => {
                    if first {
                        self.push(self.c[self.i + 1]);
                    }
                    self.i += 2;
                }
                ',' => {
                    first = false;
                    self.i += 1;
                }
                '|' if self.c.get(self.i + 1) == Some(&'}') => {
                    self.i += 2;
                    return true;
                }
                _ => {
                    if first {
                        self.push(ch);
                    }
                    self.i += 1;
                }
            }
        }
        false
    }

    /// Not snippet syntax after all: undo what the `$` at `mark` emitted,
    /// put a plain `$` and go on after it.
    fn literal(&mut self, mark: Mark) {
        self.out.truncate(mark.len);
        self.n = mark.n;
        self.stops.truncate(mark.stops);
        self.open.truncate(mark.open);
        self.i = mark.at + 1;
        self.push('$');
    }
}

/// Parser state before a `$`, to go back to.
#[derive(Clone, Copy)]
struct Mark {
    at: usize,
    len: usize,
    n: usize,
    stops: usize,
    open: usize,
}

/// What accepting an item does: the changes of one transaction (sorted for
/// `Buffer::apply`, see [`edits::plan`]), the caret after it, and a
/// snippet's tabstops as char ranges in the text after it (empty for plain
/// text; with an implicit `$0` at the end when the snippet has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accept {
    pub changes: Vec<Change>,
    pub caret: usize,
    pub stops: Vec<Tabstop>,
}

/// The edit accepting `item` makes: its `textEdit` (else its `insertText`,
/// else its label, replacing the word typed so far), snippets flattened
/// (their tabstops in [`Accept::stops`]), with its `additionalTextEdits` (an import, say) in the same
/// transaction; those overlapping the main edit or each other are left
/// out. The list was asked for at `origin`, the caret is now at `caret`
/// in `rope` and `origin.prefix(rope, caret)` holds. The caret ends at the
/// snippet's `$0`, else after the inserted text. `crlf`: the buffer's line
/// breaks.
pub fn edit_for(item: &CompletionItem, origin: &Origin, rope: &Rope, caret: usize, crlf: bool) -> Accept {
    let map = |p: Position, end: bool| map_position(origin, rope, caret, p, end);
    let (start, end, text) = match &item.text_edit {
        Some(CompletionTextEdit::Edit(e)) => (map(e.range.start, false), map(e.range.end, true), e.new_text.as_str()),
        Some(CompletionTextEdit::InsertAndReplace(e)) => (map(e.insert.start, false), map(e.insert.end, true), e.new_text.as_str()),
        None => (origin.start, caret, item.insert_text.as_deref().unwrap_or(&item.label)),
    };
    let (text, at, mut stops) = match item.insert_text_format {
        Some(InsertTextFormat::SNIPPET) => {
            let (t, mut stops) = parse_snippet(&text.replace("\r\n", "\n"));
            if !stops.iter().any(|s| s.n == 0) {
                let len = t.chars().count();
                stops.push(Tabstop { n: 0, range: len..len, parent: None });
            }
            let at = stops.iter().find(|s| s.n == 0).map_or(0, |s| s.range.start);
            (t, at, stops)
        }
        _ => (text.to_string(), text.chars().count(), Vec::new()),
    };
    if crlf {
        // Each '\n' before a bound becomes "\r\n": one char more.
        let breaks: Vec<usize> = text.chars().enumerate().filter(|&(_, c)| c == '\n').map(|(i, _)| i).collect();
        let shift = |p: usize| p + breaks.partition_point(|&b| b < p);
        for s in &mut stops {
            s.range = shift(s.range.start)..shift(s.range.end);
        }
    }
    let (text, at) = edits::line_breaks(&text, at, crlf);
    let main = CharEdit::new(start, end, text);
    let mut all = vec![main.clone()];
    for e in item.additional_text_edits.iter().flatten() {
        let extra = CharEdit::new(map(e.range.start, false), map(e.range.end, true), edits::line_breaks(&e.new_text, 0, crlf).0);
        let overlaps = extra.start < main.end && main.start < extra.end
            || (extra.start == extra.end && main.start < extra.start && extra.start < main.end);
        if overlaps {
            tracing::debug!("completion: an additional edit overlaps the item's edit; left out");
        } else {
            all.push(extra);
        }
    }
    let plan = edits::plan(rope, &all).or_else(|o| {
        tracing::debug!("completion: additional edits overlap ({o:?}); only the item's edit");
        edits::plan(rope, &all[..1])
    });
    match plan {
        Ok(plan) => {
            let base = plan.starts[0];
            for s in &mut stops {
                s.range = base + s.range.start..base + s.range.end;
            }
            Accept { caret: base + at, changes: plan.changes, stops }
        }
        Err(_) => Accept { changes: Vec::new(), caret, stops: Vec::new() },
    }
}

/// A position from an answer to the request at `origin`, as a char index
/// in `rope` now that the caret is at `caret`. Before the request position
/// the text is as it was (clamped to the caret, which may have gone back);
/// the request position is the request's caret as a start and the current
/// caret as an end (so text typed since is replaced); positions after it
/// on its line moved with the caret.
fn map_position(origin: &Origin, rope: &Rope, caret: usize, p: Position, end: bool) -> usize {
    let req = origin.pos;
    if p.line != req.line || p.character < req.character {
        let at = pos::char_index(rope, p);
        return if p.line > req.line { at } else { at.min(caret) };
    }
    if p.character == req.character {
        return if end { caret } else { origin.caret.min(caret) };
    }
    // UTF-16 units past the request position, counted from the caret.
    let mut units = (p.character - req.character) as usize;
    let mut at = caret;
    let line_end = {
        let line = rope.char_to_line(caret);
        rope.line_to_char(line) + crate::editor::selection::line_len_no_eol(rope, line)
    };
    while units > 0 && at < line_end {
        units = units.saturating_sub(rope.char(at).len_utf16());
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::Buffer;
    use crate::editor::selection::Selection;
    use crate::editor::undo::TxKind;
    use lsp_types::{InsertReplaceEdit, Range, TextEdit};

    fn item(label: &str) -> CompletionItem {
        CompletionItem { label: label.into(), ..Default::default() }
    }

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Range {
        Range::new(Position::new(l0, c0), Position::new(l1, c1))
    }

    /// `text` with `|` marking the caret at the request; then `typed` is
    /// typed (`<` deletes one char back) and `item` accepted. Returns the
    /// text with `|` at the caret after the accept.
    fn accept(text: &str, typed: &str, item: &CompletionItem) -> String {
        let at = text.chars().position(|c| c == '|').unwrap();
        let mut b = Buffer::from_text(1, None, &text.replacen('|', "", 1), false);
        b.view.set_sel(Selection::point(at));
        let origin = Origin::new(1, &b.rope, at);
        for c in typed.chars() {
            if c == '<' {
                b.backspace();
            } else {
                b.insert(&c.to_string(), TxKind::Typing);
            }
        }
        let caret = b.view.sel().head;
        assert!(origin.prefix(&b.rope, caret).is_some(), "the list applies");
        let acc = edit_for(item, &origin, &b.rope, caret, b.line_ending == crate::editor::buffer::LineEnding::CrLf);
        let before = b.text();
        b.apply(acc.changes, Selection::point(acc.caret), TxKind::Other);
        let mut out = b.text();
        let byte = out.char_indices().nth(acc.caret).map(|(i, _)| i).unwrap_or(out.len());
        out.insert(byte, '|');
        // One undo step back to what was typed.
        assert!(b.undo());
        assert_eq!(b.text(), before);
        out
    }

    #[test]
    fn label_or_insert_text_replaces_the_typed_word() {
        assert_eq!(accept("let x = pr|;", "", &item("println")), "let x = println|;");
        assert_eq!(accept("let x = pr|;", "i", &item("println")), "let x = println|;");
        let it = CompletionItem { insert_text: Some("print_it".into()), ..item("print") };
        assert_eq!(accept("pr|", "<", &it), "print_it|");
        // After a trigger char: nothing typed yet, or typed since.
        assert_eq!(accept("v.|", "", &item("len")), "v.len|");
        assert_eq!(accept("v.|", "le", &item("len")), "v.len|");
    }

    #[test]
    fn text_edits_follow_the_typing_since_the_request() {
        // The server's range covers the word up to the request position.
        let it = CompletionItem {
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range(0, 4, 0, 6), "String::new()".into()))),
            ..item("String::new")
        };
        assert_eq!(accept("let St| = 1;", "", &it), "let String::new()| = 1;");
        assert_eq!(accept("let St| = 1;", "ri", &it), "let String::new()| = 1;");
        // Deleted back past the request position.
        assert_eq!(accept("let St| = 1;", "<", &it), "let String::new()| = 1;");
        // A range past the caret (replace mode) moves with it.
        let it =
            CompletionItem { text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range(0, 0, 0, 4), "foo".into()))), ..item("foo") };
        assert_eq!(accept("f|oo2 x", "", &it), "foo| x");
        assert_eq!(accept("f|oo2 x", "o", &it), "foo| x");
        // An insert-only range at the request position.
        let it = CompletionItem {
            text_edit: Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                new_text: "len()".into(),
                insert: range(0, 2, 0, 2),
                replace: range(0, 2, 0, 2),
            })),
            ..item("len")
        };
        assert_eq!(accept("v.|", "le", &it), "v.len()|");
    }

    #[test]
    fn non_ascii_before_the_word_counts_utf16() {
        // 😀 is two UTF-16 units, 日 one: the word starts at unit 6.
        let it = CompletionItem {
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range(1, 6, 1, 8), "helper".into()))),
            ..item("helper")
        };
        assert_eq!(accept("x\n\"😀日\" he|", "l", &it), "x\n\"😀日\" helper|");
    }

    fn stop(n: u32, range: std::ops::Range<usize>, parent: Option<usize>) -> Tabstop {
        Tabstop { n, range, parent }
    }

    /// The flat text of a snippet and its stops as (number, range).
    fn stops_of(s: &str) -> (String, Vec<(u32, std::ops::Range<usize>)>) {
        let (t, stops) = parse_snippet(s);
        (t, stops.into_iter().map(|s| (s.n, s.range)).collect())
    }

    #[test]
    fn snippets_parse_to_text_and_tabstops() {
        assert_eq!(stops_of("foo(${1:x}, ${2:y})$0"), ("foo(x, y)".into(), vec![(1, 4..5), (2, 7..8), (0, 9..9)]));
        assert_eq!(stops_of("println!(\"$1\")$0"), ("println!(\"\")".into(), vec![(1, 10..10), (0, 12..12)]));
        assert_eq!(stops_of("if ${1:cond} {\n\t$0\n}"), ("if cond {\n\t\n}".into(), vec![(1, 3..7), (0, 11..11)]));
        let (t, stops) = parse_snippet("${1:outer ${2:inner}} ${3|one,two|}");
        assert_eq!(t, "outer inner one");
        assert_eq!(stops, [stop(1, 0..11, None), stop(2, 6..11, Some(0)), stop(3, 12..15, None)]);
        assert_eq!(stops_of(r"cost \$5 \} \\ $ ${TM_FILENAME} ${NAME:dflt}"), (r"cost $5 } \ $  dflt".into(), vec![]));
        assert_eq!(stops_of("a ${1:unclosed"), ("a ${1:unclosed".into(), vec![]));
        assert_eq!(stops_of("$1 and ${1}"), (" and ".into(), vec![(1, 0..0), (1, 5..5)]));
        let it = CompletionItem {
            insert_text: Some("vec![${1:x}; ${2:n}]$0".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..item("vec!")
        };
        assert_eq!(accept("let v = ve|", "", &it), "let v = vec![x; n]|");
        let it = CompletionItem {
            insert_text: Some("fn ${1:name}($2) {\n    $0\n}".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..item("fn")
        };
        assert_eq!(accept("f|", "", &it), "fn name() {\n    |\n}");
        // Plain text keeps its dollars.
        let it = CompletionItem { insert_text: Some("$x".into()), ..item("$x") };
        assert_eq!(accept("x|", "", &it), "$x|");
    }

    /// Accept `item` at the `|` in `text`: each stop's number, start and
    /// the text it covers in the buffer after the accept.
    fn accept_stops(text: &str, item: &CompletionItem) -> Vec<(u32, usize, String)> {
        let at = text.chars().position(|c| c == '|').unwrap();
        let mut b = Buffer::from_text(1, None, &text.replacen('|', "", 1), false);
        let origin = Origin::new(1, &b.rope, at);
        let acc = edit_for(item, &origin, &b.rope, at, b.line_ending == crate::editor::buffer::LineEnding::CrLf);
        b.apply(acc.changes, Selection::point(acc.caret), TxKind::Other);
        acc.stops.into_iter().map(|s| (s.n, s.range.start, b.rope.slice(s.range).to_string())).collect()
    }

    #[test]
    fn accept_returns_absolute_tabstops() {
        let snippet = |text: &str| CompletionItem {
            insert_text: Some(text.into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..item("x")
        };
        let it = snippet("vec![${1:x}; ${2:n}]$0");
        assert_eq!(accept_stops("let v = ve|", &it), [(1, 13, "x".into()), (2, 16, "n".into()), (0, 18, String::new())]);
        // No $0: one is put at the end.
        assert_eq!(accept_stops("f|", &snippet("f(${1:a})")), [(1, 2, "a".into()), (0, 4, String::new())]);
        // CRLF: the stop after a line break is one char further.
        assert_eq!(accept_stops("x\r\n|", &snippet("a\n${1:b}")), [(1, 6, "b".into()), (0, 7, String::new())]);
        // Plain text has none.
        assert!(accept_stops("x|", &item("xyz")).is_empty());
        // An import added above shifts them.
        let it = CompletionItem {
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range(1, 4, 1, 6), "HashMap::with_capacity(${1:n})".into()))),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            additional_text_edits: Some(vec![TextEdit::new(range(0, 0, 0, 0), "use std::collections::HashMap;\n".into())]),
            ..item("HashMap")
        };
        let out = accept_stops("fn f() {\n    Ha|\n}\n", &it);
        assert_eq!(out[0].2, "n");
        assert_eq!(out[0].1, 31 + 9 + 4 + 23);
    }

    #[test]
    fn additional_edits_go_in_the_same_transaction() {
        // An auto-import above, the item at the caret.
        let it = CompletionItem {
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range(2, 4, 2, 6), "HashMap::new()".into()))),
            additional_text_edits: Some(vec![TextEdit::new(range(0, 0, 0, 0), "use std::collections::HashMap;\n".into())]),
            ..item("HashMap")
        };
        let out = accept("fn f() {\n    // 日本\n    Ha|\n}\n", "sh", &it);
        assert_eq!(out, "use std::collections::HashMap;\nfn f() {\n    // 日本\n    HashMap::new()|\n}\n");
        // One overlapping the main edit is left out; others stay.
        let it = CompletionItem {
            additional_text_edits: Some(vec![TextEdit::new(range(0, 0, 0, 1), "X".into()), TextEdit::new(range(0, 3, 0, 4), "!".into())]),
            ..item("abc")
        };
        assert_eq!(accept("q ab|c", "", &it), "X abc|c");
        // CRLF buffers get CRLF line breaks, the caret moved along.
        let it = CompletionItem {
            insert_text: Some("a\n$0b".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            additional_text_edits: Some(vec![TextEdit::new(range(0, 0, 0, 0), "u\n".into())]),
            ..item("ab")
        };
        assert_eq!(accept("x\r\na|", "", &it), "u\r\nx\r\na\r\n|b");
    }

    #[test]
    fn origin_applies_while_the_word_is_typed_at_the_caret() {
        let rope = Rope::from_str("let foo.ba = 1;");
        let o = Origin::new(1, &rope, 10);
        assert_eq!((o.start, o.prefix(&rope, 10).as_deref()), (8, Some("ba")));
        assert_eq!(o.prefix(&rope, 9), None, "the text after the caret changed length");
        let typed = Rope::from_str("let foo.bar = 1;");
        assert_eq!(o.prefix(&typed, 11).as_deref(), Some("bar"));
        let back = Rope::from_str("let foo. = 1;");
        assert_eq!(o.prefix(&back, 8).as_deref(), Some(""));
        let gone = Rope::from_str("let foo = 1;");
        assert_eq!(o.prefix(&gone, 7), None, "deleted past the start");
        let spaced = Rope::from_str("let foo.ba  = 1;");
        assert_eq!(o.prefix(&spaced, 11), None, "a space typed");
    }

    #[test]
    fn match_indices_are_the_label_chars_the_prefix_matched() {
        let mut m = Matcher::default();
        let items = vec![item("println!")];
        let mut st = CompletionState::new(Origin::new(1, &Rope::from_str("pln"), 3), items, false, "pln", &mut m);
        assert_eq!(st.match_indices(0, &mut m), [0, 5, 6]);
        st.prefix = "zz".into();
        assert!(st.match_indices(0, &mut m).is_empty());
        st.prefix.clear();
        assert!(st.match_indices(0, &mut m).is_empty());
    }

    #[test]
    fn filter_puts_prefix_matches_first() {
        let items: Vec<CompletionItem> = ["Option", "print", "println", "eprintln", "PartialEq", "ptr"]
            .iter()
            .enumerate()
            .map(|(i, l)| CompletionItem { sort_text: Some(format!("{i}")), ..item(l) })
            .collect();
        let mut m = Matcher::default();
        let names = |order: Vec<usize>| order.into_iter().map(|i| items[i].label.clone()).collect::<Vec<_>>();
        assert_eq!(names(filter(&items, "pr", &mut m)), ["print", "println", "PartialEq", "ptr", "eprintln"]);
        assert_eq!(names(filter(&items, "pln", &mut m)), ["println", "eprintln"]);
        assert_eq!(names(filter(&items, "P", &mut m))[..2], ["PartialEq", "print"]);
        assert_eq!(names(filter(&items, "", &mut m)).len(), 6);
        // filterText wins over the label.
        let odd = vec![CompletionItem { filter_text: Some("zzz".into()), ..item("abc") }];
        assert!(filter(&odd, "ab", &mut m).is_empty());
        let mut st = CompletionState::new(Origin::new(1, &Rope::from_str("p"), 1), items.clone(), false, "p", &mut m);
        st.step(1, false);
        let sel = st.selected_item().unwrap().label.clone();
        st.refilter("pr", &mut m);
        assert_eq!(st.selected_item().unwrap().label, sel, "the selection stays on its item");
        st.step(-5, true);
        st.step(100, false);
        assert_eq!(st.selected, st.order.len() - 1);
    }
}
