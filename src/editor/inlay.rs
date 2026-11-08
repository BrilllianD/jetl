//! LSP inlay hints: virtual text drawn before a char of the buffer. Hints
//! are kept per buffer in char offsets and shifted by every edit
//! ([`InlayHints::map_edit`]) until the next answer replaces them. The
//! layout walk (`view::layout_line_inlays`) emits them as cells that no char
//! backs.

use std::collections::BTreeMap;
use std::ops::Range;

use ropey::Rope;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::selection::line_len_no_eol;

/// Cells of a label; a longer one is cut and ends with `…`.
pub const MAX_LABEL_WIDTH: usize = 40;

/// Cells of a whole inline-values label.
pub const MAX_VALUES_WIDTH: usize = 120;

/// Lines of one function that get inline values, the ones nearest the frame.
pub const MAX_VALUE_LINES: usize = 200;

/// Hints kept per buffer, the first by position.
pub const MAX_INLAYS: usize = 2000;

/// One hint: `label` drawn before the char at `at` (after the last char
/// when `at` is a line end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inlay {
    pub at: usize,
    pub label: String,
    /// Cells of `label`.
    pub width: usize,
    pub pad_left: bool,
    pub pad_right: bool,
    /// A parameter-name hint (LSP kind 2).
    pub param: bool,
}

/// `label` with control chars and escaped chars turned into spaces, cut to
/// `max` cells with `…` when wider.
fn clean_label(label: &str, max: usize) -> String {
    let mut label: String = label.chars().map(|c| if c.is_control() || crate::util::text::shown_escaped(c) { ' ' } else { c }).collect();
    if label.width() > max {
        let mut w = 0;
        let mut cut = String::new();
        for g in label.graphemes(true) {
            let gw = g.width();
            if w + gw > max - 1 {
                break;
            }
            w += gw;
            cut.push_str(g);
        }
        cut.push('…');
        label = cut;
    }
    label
}

impl Inlay {
    /// A hint with a sanitised label: control chars and escaped chars become
    /// spaces, and a label wider than [`MAX_LABEL_WIDTH`] is cut with `…`.
    /// `None` when nothing visible is left.
    pub fn new(at: usize, label: &str, pad_left: bool, pad_right: bool, param: bool) -> Option<Inlay> {
        let label = clean_label(label, MAX_LABEL_WIDTH);
        let width = label.width();
        (width > 0).then_some(Inlay { at, label, width, pad_left, pad_right, param })
    }

    /// Cells the hint takes, paddings included.
    pub fn cells(&self) -> usize {
        self.width + usize::from(self.pad_left) + usize::from(self.pad_right)
    }
}

/// A buffer's hints, sorted by position.
#[derive(Debug, Default, Clone)]
pub struct InlayHints {
    hints: Vec<Inlay>,
    /// Changes whenever the hints (or their places) change; part of the
    /// wrap cache key.
    generation: u64,
    /// The buffer version and lines the last answer covered.
    fetched: Option<(u64, Range<usize>)>,
}

/// No hints, for rows that draw none.
pub static NO_INLAYS: InlayHints = InlayHints::new();

impl InlayHints {
    pub const fn new() -> Self {
        Self { hints: Vec::new(), generation: 0, fetched: None }
    }

    /// Replace the hints with an answer for `lines` at buffer `version`.
    pub fn set(&mut self, mut hints: Vec<Inlay>, version: u64, lines: Range<usize>) {
        hints.sort_by_key(|h| h.at);
        hints.truncate(MAX_INLAYS);
        self.hints = hints;
        self.generation += 1;
        self.fetched = Some((version, lines));
    }

    /// Drop the hints and what was fetched.
    pub fn clear(&mut self) {
        if !self.hints.is_empty() {
            self.generation += 1;
        }
        self.hints.clear();
        self.fetched = None;
    }

    /// Forget what was fetched, keeping the hints (they show until the next
    /// answer).
    pub fn forget_fetch(&mut self) {
        self.fetched = None;
    }

    pub fn fetched(&self) -> Option<(u64, Range<usize>)> {
        self.fetched.clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_empty(&self) -> bool {
        self.hints.is_empty()
    }

    pub fn all(&self) -> &[Inlay] {
        &self.hints
    }

    /// The hints anchored on `line`: its chars and its end, not the line
    /// break slot (that is the next line's start).
    pub fn on_line(&self, rope: &Rope, line: usize) -> &[Inlay] {
        if self.hints.is_empty() || line >= rope.len_lines() {
            return &[];
        }
        let s = rope.line_to_char(line);
        let e = s + line_len_no_eol(rope, line);
        let lo = self.hints.partition_point(|h| h.at < s);
        let hi = self.hints.partition_point(|h| h.at <= e);
        &self.hints[lo..hi]
    }

    /// Cells the hints of `line` take.
    pub fn width_on_line(&self, rope: &Rope, line: usize) -> usize {
        self.on_line(rope, line).iter().map(Inlay::cells).sum()
    }

    /// Shift the hints through the replacement of `old_len` chars at `at` by
    /// `new_len` chars. Hints strictly inside the replaced text go. At an
    /// insertion point a parameter hint stays and any other hint moves after
    /// the inserted text; at the start of replaced text a hint stays; at its
    /// end a hint moves to the end of the new text.
    pub fn map_edit(&mut self, at: usize, old_len: usize, new_len: usize) {
        if self.hints.is_empty() {
            return;
        }
        let before = self.hints.len();
        let mut moved = false;
        self.hints.retain_mut(|h| {
            let new_at = if h.at < at {
                h.at
            } else if h.at == at {
                if old_len == 0 && !h.param { at + new_len } else { at }
            } else if h.at < at + old_len {
                return false;
            } else {
                h.at + new_len - old_len
            };
            moved |= new_at != h.at;
            h.at = new_at;
            true
        });
        if moved || self.hints.len() != before {
            self.hints.sort_by_key(|h| h.at);
            self.generation += 1;
        }
    }
}

/// The debugger's inline values: at most one line-end label per line, kept
/// apart from the LSP hints ([`InlayHints`]). The layout walk draws it after
/// any hint at the line end; the wrap never sees it.
#[derive(Debug, Default, Clone)]
pub struct InlineValues {
    lines: BTreeMap<usize, Inlay>,
}

/// No values, for rows that draw none.
pub static NO_VALUES: InlineValues = InlineValues::new();

impl InlineValues {
    pub const fn new() -> Self {
        Self { lines: BTreeMap::new() }
    }

    pub fn set(&mut self, lines: BTreeMap<usize, Inlay>) {
        self.lines = lines;
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn on_line(&self, line: usize) -> Option<&Inlay> {
        self.lines.get(&line)
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, &Inlay)> {
        self.lines.iter().map(|(l, i)| (*l, i))
    }
}

/// Whether a tree-sitter leaf kind names a variable.
fn is_name_kind(kind: &str) -> bool {
    matches!(kind, "identifier" | "shorthand_field_identifier" | "shorthand_property_identifier" | "self" | "this")
}

/// The names written on `line`, in first-occurrence order, without repeats.
fn names_on_line(rope: &Rope, tree: Option<&tree_sitter::Tree>, line: usize) -> Vec<String> {
    let s = rope.line_to_char(line);
    let e = s + line_len_no_eol(rope, line);
    let mut names: Vec<String> = Vec::new();
    let mut push = |n: String| {
        if !names.contains(&n) {
            names.push(n);
        }
    };
    match tree {
        Some(tree) => {
            let (sb, eb) = (rope.char_to_byte(s), rope.char_to_byte(e));
            let mut cur = tree.walk();
            'walk: loop {
                let n = cur.node();
                let skip = n.end_byte() <= sb || n.start_byte() >= eb;
                if !skip && n.child_count() == 0 && n.start_byte() >= sb && is_name_kind(n.kind()) {
                    push(rope.byte_slice(n.start_byte()..n.end_byte()).to_string());
                }
                if !skip && cur.goto_first_child() {
                    continue;
                }
                loop {
                    if cur.goto_next_sibling() {
                        continue 'walk;
                    }
                    if !cur.goto_parent() {
                        break 'walk;
                    }
                }
            }
        }
        None => {
            let text = rope.slice(s..e).to_string();
            let mut word = String::new();
            for c in text.chars().chain(std::iter::once(' ')) {
                if super::selection::is_word_char(c) {
                    word.push(c);
                } else if !word.is_empty() {
                    if !word.starts_with(|c: char| c.is_ascii_digit()) {
                        push(std::mem::take(&mut word));
                    } else {
                        word.clear();
                    }
                }
            }
        }
    }
    names
}

/// The labels of `lines`: per line, `name = value` for each of `values`
/// (name, shown value, shadowed; in scope order) whose name is written there.
/// A shadowed name counts on `frame_line` only.
pub fn value_labels(
    rope: &Rope,
    tree: Option<&tree_sitter::Tree>,
    lines: Range<usize>,
    frame_line: usize,
    values: &[(&str, &str, bool)],
) -> BTreeMap<usize, Inlay> {
    let mut out = BTreeMap::new();
    for line in lines {
        if line >= rope.len_lines() {
            break;
        }
        let parts: Vec<String> = names_on_line(rope, tree, line)
            .iter()
            .filter_map(|n| {
                let (name, v, shadowed) = values.iter().find(|(vn, _, _)| vn == n)?;
                if *shadowed && line != frame_line {
                    return None;
                }
                Some(format!("{name} = {}", clean_label(v, MAX_LABEL_WIDTH)))
            })
            .collect();
        if parts.is_empty() {
            continue;
        }
        let label = clean_label(&parts.join(", "), MAX_VALUES_WIDTH);
        let width = label.width();
        let at = rope.line_to_char(line) + line_len_no_eol(rope, line);
        out.insert(line, Inlay { at, label, width, pad_left: true, pad_right: false, param: false });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_sanitised_and_truncated() {
        let h = Inlay::new(0, "a\nb\tc", false, false, false).unwrap();
        assert_eq!((h.label.as_str(), h.width), ("a b c", 5));
        let long = Inlay::new(0, &"x".repeat(60), false, false, false).unwrap();
        assert_eq!(long.label, format!("{}…", "x".repeat(39)));
        assert_eq!(long.width, 40);
        let cjk = Inlay::new(0, "日本語", false, false, false).unwrap();
        assert_eq!(cjk.width, 6);
        let cut = Inlay::new(0, &"日".repeat(30), false, false, false).unwrap();
        assert!(cut.width <= MAX_LABEL_WIDTH && cut.label.ends_with('…'));
        assert!(Inlay::new(0, "", false, false, false).is_none());
        assert!(Inlay::new(0, "\u{200b}", false, false, false).is_none());
        assert_eq!(Inlay::new(0, "ab", true, true, false).unwrap().cells(), 4);
    }

    fn hint(at: usize, param: bool) -> Inlay {
        Inlay::new(at, "h", false, false, param).unwrap()
    }

    #[test]
    fn map_edit_follows_the_rules() {
        // (at, old, new, param, hint before, hint after)
        let table: [(usize, usize, usize, bool, usize, Option<usize>); 11] = [
            (5, 0, 1, false, 3, Some(3)),  // before the edit: kept
            (5, 0, 1, false, 5, Some(6)),  // insertion at a type hint: it moves after
            (5, 0, 1, true, 5, Some(5)),   // insertion at a param hint: it stays
            (5, 2, 1, false, 5, Some(5)),  // replacement start: stays
            (5, 2, 1, true, 5, Some(5)),   // replacement start: stays
            (5, 3, 1, false, 6, None),     // strictly inside: dropped
            (5, 3, 1, false, 8, Some(6)),  // at the end of the replaced text
            (5, 3, 4, true, 8, Some(9)),   // at the end, param too
            (5, 2, 4, false, 9, Some(11)), // after: shifted
            (5, 2, 0, false, 9, Some(7)),  // after a deletion
            (5, 0, 0, false, 5, Some(5)),  // empty edit
        ];
        for (at, old, new, param, before, after) in table {
            let mut hs = InlayHints::new();
            hs.set(vec![hint(before, param)], 0, 0..1);
            hs.map_edit(at, old, new);
            let got = hs.all().first().map(|h| h.at);
            assert_eq!(got, after, "edit ({at}, {old}, {new}) param={param} hint at {before}");
        }
    }

    #[test]
    fn gen_changes_only_when_hints_change() {
        let mut hs = InlayHints::new();
        hs.map_edit(0, 0, 3);
        hs.clear();
        assert_eq!(hs.generation(), 0, "no hints: nothing to bump");
        hs.set(vec![hint(2, false)], 4, 0..2);
        let g = hs.generation();
        assert_eq!(g, 1, "set bumps");
        hs.map_edit(10, 0, 3);
        assert_eq!(hs.generation(), g, "an edit after the last hint moves nothing");
        hs.map_edit(0, 0, 3);
        assert_eq!(hs.generation(), g + 1, "a shift bumps");
        hs.forget_fetch();
        assert_eq!((hs.generation(), hs.fetched(), hs.is_empty()), (g + 1, None, false));
        hs.set(Vec::new(), 4, 0..2);
        assert_eq!(hs.generation(), g + 2, "set always bumps");
        hs.set(vec![hint(0, false)], 5, 0..1);
        assert_eq!(hs.fetched(), Some((5, 0..1)));
        hs.clear();
        assert_eq!((hs.generation(), hs.fetched()), (g + 4, None), "clear of hints bumps");
        hs.clear();
        assert_eq!(hs.generation(), g + 4);
    }

    #[test]
    fn on_line_includes_the_line_end() {
        let rope = Rope::from_str("ab\ncd\n");
        let mut hs = InlayHints::new();
        hs.set(vec![hint(3, false), hint(0, false), hint(2, false)], 0, 0..2);
        assert_eq!(hs.on_line(&rope, 0).iter().map(|h| h.at).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(hs.on_line(&rope, 1).iter().map(|h| h.at).collect::<Vec<_>>(), [3]);
        assert!(hs.on_line(&rope, 9).is_empty());
        assert_eq!(hs.width_on_line(&rope, 0), 2);
    }

    const ADD_RS: &str = "fn add(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum\n}\nfn main() {\n    add(1, 2);\n}\n";

    fn labels(src: &str, lang: Option<&str>, lines: Range<usize>, frame: usize, values: &[(&str, &str, bool)]) -> Vec<(usize, String)> {
        let rope = Rope::from_str(src);
        let mut syn = lang.map(|l| {
            let def = crate::editor::syntax::language_for_name(l).unwrap();
            crate::editor::syntax::Syntax::new(def, &crate::config::theme::Theme::default()).unwrap()
        });
        if let Some(s) = &mut syn {
            s.reparse(&rope);
        }
        let tree = syn.as_ref().and_then(|s| s.tree());
        value_labels(&rope, tree, lines, frame, values).into_iter().map(|(l, i)| (l, i.label)).collect()
    }

    fn pair(l: usize, s: &str) -> (usize, String) {
        (l, s.to_string())
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn values_follow_the_names_on_each_line() {
        let got = labels(ADD_RS, Some("rust"), 0..3, 2, &[("a", "1", false), ("b", "2", false), ("sum", "3", false)]);
        assert_eq!(got, [pair(0, "a = 1, b = 2"), pair(1, "sum = 3, a = 1, b = 2"), pair(2, "sum = 3")]);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn shadowed_names_only_on_the_frame_line() {
        let got = labels(ADD_RS, Some("rust"), 0..3, 2, &[("a", "1", false), ("b", "2", false), ("sum", "3", true)]);
        assert_eq!(got, [pair(0, "a = 1, b = 2"), pair(1, "a = 1, b = 2"), pair(2, "sum = 3")]);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn fields_and_types_do_not_match() {
        let src = "let p = Point { x: a, y: b };\n";
        let got = labels(src, Some("rust"), 0..1, 0, &[("x", "10", false), ("Point", "11", false), ("a", "1", false), ("p", "5", false)]);
        assert_eq!(got, [pair(0, "p = 5, a = 1")]);
    }

    #[test]
    fn no_tree_scans_words() {
        let got = labels("l3 x+y 9z\n", None, 0..1, 0, &[("x", "1", false), ("y", "2", false), ("9z", "3", false)]);
        assert_eq!(got, [pair(0, "x = 1, y = 2")]);
    }

    #[test]
    fn long_values_are_cut() {
        let long = "v".repeat(60);
        let got = labels("x\n", None, 0..1, 0, &[("x", &long, false)]);
        assert_eq!(got, [pair(0, &format!("x = {}…", "v".repeat(39)))]);
        // "x = " is 4 cells, the value is cut to 40 cells ending in `…`.
        assert_eq!(got[0].1.width(), 4 + 40);
        let names: Vec<String> = (0..10).map(|i| format!("n{i}")).collect();
        let vals: Vec<String> = (0..10).map(|_| "w".repeat(30)).collect();
        let src = format!("{}\n", names.join(" "));
        let v: Vec<(&str, &str, bool)> = names.iter().zip(&vals).map(|(n, v)| (n.as_str(), v.as_str(), false)).collect();
        let got = labels(&src, None, 0..1, 0, &v);
        assert_eq!(got[0].1.width(), MAX_VALUES_WIDTH);
        assert!(got[0].1.ends_with('…'));
    }

    #[test]
    fn the_label_sits_at_the_line_end_with_a_left_pad() {
        let rope = Rope::from_str("ab\ncd\n");
        let m = value_labels(&rope, None, 0..2, 1, &[("ab", "1", false)]);
        let i = &m[&0];
        assert_eq!((i.at, i.pad_left, i.pad_right, i.param, i.width), (2, true, false, false, 6));
        assert!(!m.contains_key(&1));
    }
}
