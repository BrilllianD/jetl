//! Find / replace bar state for one buffer.

use std::borrow::Cow;
use std::ops::Range;

use regex::Regex;
use ropey::Rope;

use super::selection::{Selection, is_word_char, word_at};
use crate::ui::widgets::text_input::TextInput;

/// Live search keeps at most this many matches.
const MAX_MATCHES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Query,
    Replace,
}

#[derive(Debug)]
pub struct SearchState {
    pub query: TextInput,
    pub replace: TextInput,
    pub show_replace: bool,
    pub field: Field,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub matches: Vec<Range<usize>>,
    /// More than `MAX_MATCHES` matches exist; only the first are kept.
    pub capped: bool,
    pub current: Option<usize>,
    pub error: Option<String>,
    /// The bar has keyboard focus (as opposed to the text).
    pub focused: bool,
    /// Caret position when the bar opened; live search starts here.
    pub origin: usize,
    computed_for: Option<(u64, String, bool, bool, bool)>,
    compiled: Option<Regex>,
    /// Files above this many bytes are not searched live.
    pub max_bytes: usize,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: TextInput::new(false),
            replace: TextInput::new(false),
            show_replace: false,
            field: Field::Query,
            regex: false,
            case_sensitive: false,
            whole_word: false,
            matches: Vec::new(),
            capped: false,
            current: None,
            error: None,
            focused: false,
            origin: 0,
            computed_for: None,
            compiled: None,
            max_bytes: 16 * 1024 * 1024,
        }
    }
}

impl SearchState {
    pub fn new(initial: &str, show_replace: bool) -> Self {
        let mut s = Self { show_replace, ..Default::default() };
        s.query.set_text(initial);
        s
    }

    /// Some(true) when inside the current match, Some(false) in another match.
    pub fn match_at(&self, char_idx: usize) -> Option<bool> {
        let i = self.matches.partition_point(|r| r.end <= char_idx);
        let r = self.matches.get(i)?;
        if r.contains(&char_idx) { Some(self.current == Some(i)) } else { None }
    }

    pub fn pattern(&self) -> Result<Regex, String> {
        let q = &self.query.text;
        let mut pat = if self.regex { q.clone() } else { regex::escape(q) };
        if self.whole_word {
            pat = format!(r"\b(?:{pat})\b");
        }
        let flags = if self.case_sensitive { "" } else { "(?i)" };
        Regex::new(&format!("{flags}{pat}")).map_err(|e| e.to_string())
    }

    /// Recompute matches when the query, options or buffer changed.
    /// Returns true when the match list was rebuilt.
    pub fn update(&mut self, rope: &Rope, version: u64) -> bool {
        let key = (version, self.query.text.clone(), self.regex, self.case_sensitive, self.whole_word);
        if self.computed_for.as_ref() == Some(&key) {
            return false;
        }
        self.computed_for = Some(key);
        self.matches.clear();
        self.capped = false;
        self.error = None;
        self.compiled = None;
        if self.query.text.is_empty() {
            self.current = None;
            return true;
        }
        if rope.len_bytes() > self.max_bytes {
            self.error = Some("file too large for live search".into());
            return true;
        }
        let re = match self.pattern() {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                self.current = None;
                return true;
            }
        };
        let text = rope.to_string();
        // Byte offsets to char offsets incrementally: one linear pass over
        // the text instead of a rope lookup per match.
        let (mut byte, mut ch) = (0usize, 0usize);
        for m in re.find_iter(&text) {
            if m.start() == m.end() {
                continue;
            }
            if self.matches.len() >= MAX_MATCHES {
                self.capped = true;
                break;
            }
            ch += text[byte..m.start()].chars().count();
            let len = m.as_str().chars().count();
            self.matches.push(ch..ch + len);
            byte = m.end();
            ch += len;
        }
        self.compiled = Some(re);
        if let Some(c) = self.current {
            if c >= self.matches.len() {
                self.current = None;
            }
        }
        true
    }

    /// Index of the first match starting at or after `pos` (wrapping).
    pub fn first_from(&self, pos: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let i = self.matches.partition_point(|r| r.start < pos);
        Some(if i < self.matches.len() { i } else { 0 })
    }

    /// Index of the last match starting before `pos` (wrapping).
    pub fn last_before(&self, pos: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let i = self.matches.partition_point(|r| r.start < pos);
        Some(if i == 0 { self.matches.len() - 1 } else { i - 1 })
    }

    /// Make match `idx` current and return its char range.
    pub fn select(&mut self, idx: usize) -> Range<usize> {
        self.current = Some(idx);
        self.matches[idx].clone()
    }

    /// Replacement for the match at `bytes` in `text` (the whole buffer).
    /// In regex mode `$1` groups are expanded; the regex runs on the full
    /// text so anchors and `\b` see the real context.
    pub fn replacement_at(&self, text: &str, bytes: Range<usize>) -> String {
        let (Some(re), true) = (&self.compiled, self.regex) else { return self.replace.text.clone() };
        match re.captures_at(text, bytes.start) {
            Some(caps) if caps.get(0).map(|m| m.range()) == Some(bytes) => {
                let mut out = String::new();
                caps.expand(&self.replace.text, &mut out);
                out
            }
            _ => self.replace.text.clone(),
        }
    }

    pub fn active_input(&mut self) -> &mut TextInput {
        match self.field {
            Field::Query => &mut self.query,
            Field::Replace => &mut self.replace,
        }
    }

    pub fn status(&self) -> String {
        if let Some(e) = &self.error {
            return e.clone();
        }
        if self.query.text.is_empty() {
            return String::new();
        }
        let more = if self.capped { "+" } else { "" };
        match (self.current, self.matches.len()) {
            (_, 0) => "No matches".into(),
            (Some(c), n) => format!("{}/{n}{more}", c + 1),
            (None, n) => format!("{n}{more} matches"),
        }
    }

    pub fn invalidate(&mut self) {
        self.computed_for = None;
    }
}

/// What the editor highlights occurrences of: the selected text when the
/// selection is on one line and not just whitespace, otherwise the word at
/// the caret (or the one ending at it). The bool is the whole-word rule:
/// words match only as whole words, selected text as plain substrings.
pub fn occurrence_target(rope: &Rope, sel: &Selection) -> Option<(String, bool)> {
    if !sel.is_empty() {
        let text = rope.slice(sel.range()).to_string();
        let skip = text.contains(['\n', '\r']) || text.trim().is_empty();
        return (!skip).then_some((text, false));
    }
    let is_word = |p: usize| p < rope.len_chars() && is_word_char(rope.char(p));
    let at = if is_word(sel.head) { sel.head } else if sel.head > 0 && is_word(sel.head - 1) { sel.head - 1 } else { return None };
    Some((rope.slice(word_at(rope, at)).to_string(), true))
}

/// Case-sensitive, non-overlapping occurrences of `target` within `lines`
/// (line indices, end exclusive), as char ranges. With `whole_word` a match
/// must not touch a word char on either side.
pub fn occurrences(rope: &Rope, lines: Range<usize>, target: &str, whole_word: bool) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let end_line = lines.end.min(rope.len_lines());
    if target.is_empty() || lines.start >= end_line {
        return out;
    }
    let start = rope.line_to_char(lines.start);
    let end = if end_line < rope.len_lines() { rope.line_to_char(end_line) } else { rope.len_chars() };
    // The slice starts and ends at line boundaries, so its edges are word
    // boundaries too and the whole-word check needs no outside context.
    let text: Cow<str> = rope.slice(start..end).into();
    let (mut byte, mut ch) = (0usize, start);
    for (b, m) in text.match_indices(target) {
        if whole_word {
            let before = text[..b].chars().next_back();
            let after = text[b + m.len()..].chars().next();
            if before.is_some_and(is_word_char) || after.is_some_and(is_word_char) {
                continue;
            }
        }
        ch += text[byte..b].chars().count();
        let len = m.chars().count();
        out.push(ch..ch + len);
        byte = b + m.len();
        ch += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_navigates() {
        let rope = Rope::from_str("foo Foo foobar\nfoo");
        let mut s = SearchState::new("foo", false);
        assert!(s.update(&rope, 1));
        assert_eq!(s.matches.len(), 4);
        s.whole_word = true;
        s.update(&rope, 1);
        assert_eq!(s.matches.len(), 3);
        s.case_sensitive = true;
        s.update(&rope, 1);
        assert_eq!(s.matches.len(), 2);
        assert_eq!(s.first_from(1), Some(1));
        assert_eq!(s.first_from(16), Some(0));
        assert_eq!(s.last_before(0), Some(1));
    }

    #[test]
    fn match_offsets_are_chars_with_multibyte_text() {
        let rope = Rope::from_str("日本 foo é foo");
        let mut s = SearchState::new("foo", false);
        s.update(&rope, 1);
        assert_eq!(s.matches, vec![3..6, 9..12]);
    }

    #[test]
    fn regex_groups_in_replacement() {
        let text = "a1 b2";
        let mut s = SearchState::new(r"([a-z])(\d)", false);
        s.regex = true;
        s.replace.set_text("$2$1");
        s.update(&Rope::from_str(text), 1);
        assert_eq!(s.replacement_at(text, 0..2), "1a");
        assert_eq!(s.replacement_at(text, 3..5), "2b");
    }

    #[test]
    fn anchored_regex_replacement_uses_full_text() {
        let text = "a1 a2";
        let mut s = SearchState::new(r"^a(\d)", false);
        s.regex = true;
        s.replace.set_text("x$1");
        s.update(&Rope::from_str(text), 1);
        // Only the match at the start of the text exists.
        assert_eq!(s.matches, vec![0..2]);
        assert_eq!(s.replacement_at(text, 0..2), "x1");
        // Literal mode returns the replacement verbatim.
        s.regex = false;
        s.query.set_text("a1");
        s.update(&Rope::from_str(text), 1);
        assert_eq!(s.replacement_at(text, 0..2), "x$1");
    }

    #[test]
    fn occurrences_respect_word_boundaries() {
        let rope = Rope::from_str("foo foo_bar foo\nbarfoo (foo) foo2");
        assert_eq!(occurrences(&rope, 0..2, "foo", true), vec![0..3, 12..15, 24..27]);
        // Substring mode also finds the ones inside other words.
        assert_eq!(occurrences(&rope, 0..2, "foo", false), vec![0..3, 4..7, 12..15, 19..22, 24..27, 29..32]);
        // Only the requested lines are scanned.
        assert_eq!(occurrences(&rope, 1..9, "foo", true), vec![24..27]);
        assert!(occurrences(&rope, 0..1, "", false).is_empty());
    }

    #[test]
    fn occurrences_with_multibyte_text() {
        let rope = Rope::from_str("日本 é_x é x\nпривет мир привет");
        // `é_x` is one word, so the `é` inside it does not count.
        assert_eq!(occurrences(&rope, 0..2, "é", true), vec![7..8]);
        assert_eq!(occurrences(&rope, 0..2, "привет", true), vec![11..17, 22..28]);
        assert_eq!(occurrences(&rope, 0..2, "日本", false), vec![0..2]);
        // Cyrillic letters are word chars: no match inside `приветы`.
        let rope = Rope::from_str("приветы привет");
        assert_eq!(occurrences(&rope, 0..1, "привет", true), vec![8..14]);
    }

    #[test]
    fn occurrence_target_rules() {
        let rope = Rope::from_str("let foo = a.b;\n  x");
        assert_eq!(occurrence_target(&rope, &Selection::point(5)), Some(("foo".into(), true)));
        assert_eq!(occurrence_target(&rope, &Selection::point(7)), Some(("foo".into(), true)), "caret right after a word");
        assert_eq!(occurrence_target(&rope, &Selection::point(8)), None, "punctuation");
        assert_eq!(occurrence_target(&rope, &Selection::new(10, 13)), Some(("a.b".into(), false)));
        assert_eq!(occurrence_target(&rope, &Selection::new(14, 17)), None, "spans a line break");
        assert_eq!(occurrence_target(&rope, &Selection::new(15, 17)), None, "whitespace only");
    }
}
