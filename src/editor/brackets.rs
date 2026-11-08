//! Bracket matching for the pair highlight and "move caret to matching
//! bracket". With a syntax tree only real bracket tokens match, so brackets
//! inside strings and comments are ignored; without one a nesting scan over
//! the text is used.

use std::ops::Range;

use ropey::Rope;
use tree_sitter::{Node, Tree};

/// How far the plain-text scan looks in each direction, in chars.
const SCAN_LIMIT: usize = 200_000;

/// The partner of a bracket char and whether `c` opens.
fn partner(c: char) -> Option<(char, bool)> {
    match c {
        '(' => Some((')', true)),
        '[' => Some((']', true)),
        '{' => Some(('}', true)),
        ')' => Some(('(', false)),
        ']' => Some(('[', false)),
        '}' => Some(('{', false)),
        _ => None,
    }
}

/// The bracket next to the caret and its partner, as char positions. The
/// bracket at `pos` wins over the one before it, so a caret between `)(`
/// matches the opener.
pub fn pair_at(rope: &Rope, tree: Option<&Tree>, pos: usize) -> Option<(usize, usize)> {
    for p in [Some(pos), pos.checked_sub(1)].into_iter().flatten() {
        if p >= rope.len_chars() {
            continue;
        }
        let c = rope.char(p);
        let Some((other, opens)) = partner(c) else { continue };
        let found = match tree {
            Some(tree) => match_in_tree(rope, tree, p, c, other, opens),
            None => scan(rope, p, c, other, opens),
        };
        return found.map(|q| (p, q));
    }
    None
}

/// The opener matching a closer `closer` typed at `pos`, by a backwards
/// nesting scan. With a syntax tree, brackets inside strings and comments
/// are skipped.
pub fn opener_before(rope: &Rope, tree: Option<&Tree>, pos: usize, closer: char) -> Option<usize> {
    let (opener, false) = partner(closer)? else { return None };
    let mut depth = 0usize;
    let mut it = rope.chars_at(pos.min(rope.len_chars()));
    let mut q = pos.min(rope.len_chars());
    while let Some(ch) = it.prev() {
        q -= 1;
        if pos - q > SCAN_LIMIT {
            break;
        }
        if (ch != closer && ch != opener) || tree.is_some_and(|t| in_literal_at(rope, t, q)) {
            continue;
        }
        if ch == closer {
            depth += 1;
        } else if depth == 0 {
            return Some(q);
        } else {
            depth -= 1;
        }
    }
    None
}

/// Match among the bracket token's siblings. A bracket that is not a token
/// of its own is ignored inside strings and comments and scanned for
/// elsewhere (plain text such as Markdown prose).
fn match_in_tree(rope: &Rope, tree: &Tree, p: usize, c: char, other: char, opens: bool) -> Option<usize> {
    let b = rope.char_to_byte(p);
    let (kind, other_kind) = (c.to_string(), other.to_string());
    let node = match tree.root_node().descendant_for_byte_range(b, b + 1) {
        Some(n) if n.kind() == kind && n.start_byte() == b => n,
        Some(n) if in_literal(n) => return None,
        _ => return scan(rope, p, c, other, opens),
    };
    let parent = node.parent()?;
    let mut cursor = parent.walk();
    let kids: Vec<Node> = parent.children(&mut cursor).collect();
    let i = kids.iter().position(|k| k.id() == node.id())?;
    let candidates: Box<dyn Iterator<Item = &Node>> = if opens { Box::new(kids[i + 1..].iter()) } else { Box::new(kids[..i].iter().rev()) };
    let mut depth = 0usize;
    for k in candidates {
        if k.kind() == kind {
            depth += 1;
        } else if k.kind() == other_kind {
            if depth == 0 {
                return Some(rope.byte_to_char(k.start_byte()));
            }
            depth -= 1;
        }
    }
    None
}

/// Pairs typed together when `editor.auto_close` is on, for the language
/// named `lang`. Rust has no `''` (lifetimes); JavaScript, TypeScript, shell
/// and Markdown also pair backticks.
pub fn pairs(lang: Option<&str>) -> &'static [(char, char)] {
    const BASE: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"')];
    const QUOTES: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];
    const TICKS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\''), ('`', '`')];
    match lang {
        Some("rust") => BASE,
        Some("javascript" | "typescript" | "tsx" | "bash" | "markdown") => TICKS,
        _ => QUOTES,
    }
}

/// The char at `pos` lies inside a string or comment.
pub fn in_literal_at(rope: &Rope, tree: &Tree, pos: usize) -> bool {
    if pos >= rope.len_chars() {
        return false;
    }
    let b = rope.char_to_byte(pos);
    tree.root_node().descendant_for_byte_range(b, b + 1).is_some_and(in_literal)
}

fn literal_kind(kind: &str) -> bool {
    kind.contains("string") || kind.contains("comment") || kind == "char_literal"
}

fn in_literal(node: Node) -> bool {
    let mut n = Some(node);
    while let Some(x) = n {
        if literal_kind(x.kind()) {
            return true;
        }
        n = x.parent();
    }
    false
}

/// `Some(true)` for an opening and `Some(false)` for a closing bracket token
/// (a one-byte leaf whose kind is `(`, `)`, `[`, `]`, `{` or `}`).
fn bracket_token(node: Node) -> Option<bool> {
    if node.child_count() != 0 || node.end_byte() != node.start_byte() + 1 {
        return None;
    }
    let mut k = node.kind().chars();
    match (k.next(), k.next()) {
        (Some(c), None) => partner(c).map(|(_, opens)| opens),
        _ => None,
    }
}

/// Appends the bracket tokens of `tree` that start in `bytes` to `out`, as
/// (char, nesting depth), in text order. Each tree level counts the
/// brackets opened and not yet closed among its own children, starting at
/// its parent's count where it begins: a pair's two brackets share a depth,
/// and a stray bracket inside a node never shifts the colours after it.
/// Strings and comments are skipped. Visits the nodes that meet `bytes` and
/// the siblings before them on the path from the root, never the text.
pub fn depths(rope: &Rope, tree: &Tree, bytes: Range<usize>, out: &mut Vec<(usize, usize)>) {
    let mut cur = tree.walk();
    // Running count per level, from the root's children down to the cursor's.
    let mut level = vec![0usize];
    if !cur.goto_first_child() {
        return;
    }
    loop {
        let node = cur.node();
        if node.start_byte() >= bytes.end {
            return; // pre-order: every later node starts at or after this one
        }
        let d = level.last_mut().expect("a count per cursor level");
        let mut down = false;
        match bracket_token(node) {
            Some(true) => {
                if node.start_byte() >= bytes.start {
                    out.push((rope.byte_to_char(node.start_byte()), *d));
                }
                *d += 1;
            }
            Some(false) => {
                *d = d.saturating_sub(1);
                if node.start_byte() >= bytes.start {
                    out.push((rope.byte_to_char(node.start_byte()), *d));
                }
            }
            None if node.end_byte() > bytes.start && !literal_kind(node.kind()) => {
                let base = *d;
                if cur.goto_first_child() {
                    level.push(base);
                    down = true;
                }
            }
            None => {}
        }
        if down {
            continue;
        }
        while !cur.goto_next_sibling() {
            if !cur.goto_parent() {
                return;
            }
            level.pop();
        }
    }
}

/// Nesting scan that only counts the same bracket kind.
fn scan(rope: &Rope, p: usize, c: char, other: char, opens: bool) -> Option<usize> {
    let mut depth = 0usize;
    if opens {
        for (i, ch) in rope.chars_at(p + 1).take(SCAN_LIMIT).enumerate() {
            if ch == c {
                depth += 1;
            } else if ch == other {
                if depth == 0 {
                    return Some(p + 1 + i);
                }
                depth -= 1;
            }
        }
    } else {
        let mut it = rope.chars_at(p);
        let mut q = p;
        while let Some(ch) = it.prev() {
            q -= 1;
            if p - q > SCAN_LIMIT {
                break;
            }
            if ch == c {
                depth += 1;
            } else if ch == other {
                if depth == 0 {
                    return Some(q);
                }
                depth -= 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_nesting() {
        let r = Rope::from_str("f(a, [b], (c)) x");
        assert_eq!(pair_at(&r, None, 1), Some((1, 13)));
        assert_eq!(pair_at(&r, None, 13), Some((13, 1)));
        assert_eq!(pair_at(&r, None, 5), Some((5, 7)));
        assert_eq!(pair_at(&r, None, 15), None);
        let r = Rope::from_str("(()");
        assert_eq!(pair_at(&r, None, 0), None, "unmatched opener");
    }

    #[test]
    fn caret_after_bracket_and_priority() {
        let r = Rope::from_str("(a)(b)");
        // Caret between `)(`: the opener at the caret wins.
        assert_eq!(pair_at(&r, None, 3), Some((3, 5)));
        assert_eq!(pair_at(&r, None, 6), Some((5, 3)), "caret at end matches the bracket before it");
        assert_eq!(pair_at(&r, None, 1), Some((0, 2)), "caret after the opener");
    }

    #[test]
    fn opener_before_closer() {
        let r = Rope::from_str("f(a, [b], (c)\n  ");
        assert_eq!(opener_before(&r, None, r.len_chars(), ')'), Some(1));
        assert_eq!(opener_before(&r, None, 7, ']'), Some(5));
        assert_eq!(opener_before(&r, None, r.len_chars(), '}'), None);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn tree_skips_brackets_in_strings() {
        use crate::config::theme::Theme;
        let theme = Theme::default();
        let def = super::super::syntax::language_for_name("rust").expect("rust grammar");
        let mut syn = super::super::syntax::Syntax::new(def, &theme).unwrap();
        let src = "fn f() { g(\"(\", 1) }\n";
        let r = Rope::from_str(src);
        syn.reparse(&r);
        let tree = syn.tree();
        let open = src.find("g(").unwrap() + 1;
        let close = src.find(") }").unwrap();
        assert_eq!(pair_at(&r, tree, open), Some((open, close)));
        assert_eq!(pair_at(&r, tree, close), Some((close, open)));
        let in_string = src.find("\"(").unwrap() + 1;
        assert_eq!(pair_at(&r, tree, in_string), None, "bracket inside a string literal");
        assert_eq!(pair_at(&r, tree, 7), Some((7, src.len() - 2)), "function body braces");
        assert!(in_literal_at(&r, tree.unwrap(), in_string), "inside the string");
        assert!(!in_literal_at(&r, tree.unwrap(), open), "plain code");
    }

    #[cfg(all(feature = "lang-md", feature = "lang-rust"))]
    #[test]
    fn injected_brackets_use_the_inner_tree() {
        let def = super::super::syntax::language_for_name("markdown").expect("markdown grammar");
        let mut syn = super::super::syntax::Syntax::new(def, &crate::config::theme::Theme::default()).unwrap();
        let src = "```rust\nfn f() { g(\"(\", 1) }\n```\n";
        let r = Rope::from_str(src);
        syn.reparse(&r);
        let in_string = src.find("\"(").unwrap() + 1;
        assert!(pair_at(&r, syn.tree(), in_string).is_some(), "host tree: plain scan pairs it");
        assert_eq!(pair_at(&r, syn.tree_at_char(&r, in_string), in_string), None, "inner tree: inside a string literal");
        let open = src.find("g(").unwrap() + 1;
        let close = src.find(") }").unwrap();
        assert_eq!(pair_at(&r, syn.tree_at_char(&r, open), open), Some((open, close)));
    }

    #[cfg(feature = "lang-rust")]
    fn rust_depths(src: &str, bytes: Option<Range<usize>>) -> Vec<(usize, usize)> {
        use crate::config::theme::Theme;
        let def = super::super::syntax::language_for_name("rust").expect("rust grammar");
        let mut syn = super::super::syntax::Syntax::new(def, &Theme::default()).unwrap();
        let r = Rope::from_str(src);
        syn.reparse(&r);
        let mut out = Vec::new();
        depths(&r, syn.tree().unwrap(), bytes.unwrap_or(0..r.len_bytes()), &mut out);
        out
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn depths_follow_nesting() {
        let src = "fn f(a: [u8; 2]) { g(h(1), {2}) }\n";
        let got = rust_depths(src, None);
        let want_pos: Vec<usize> = src.char_indices().filter(|(_, c)| "()[]{}".contains(*c)).map(|(i, _)| i).collect();
        assert_eq!(got.iter().map(|e| e.0).collect::<Vec<_>>(), want_pos);
        assert_eq!(got.iter().map(|e| e.1).collect::<Vec<_>>(), [0, 1, 1, 0, 0, 1, 2, 2, 2, 2, 1, 0]);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn depths_skip_strings_and_comments() {
        let src = "fn f() { g(\"(\", '[', 1) } // )\n/* { */\n";
        let got = rust_depths(src, None);
        let at = |c: &str, n: usize| src.match_indices(c).nth(n).unwrap().0;
        let want = [(at("(", 0), 0), (at(")", 0), 0), (at("{", 0), 0), (at("(", 1), 1), (at(") }", 0), 1), (at("}", 0), 0)];
        assert_eq!(got, want);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn a_window_gets_the_whole_file_depths() {
        let src = "mod m {\n    fn f() {\n        if a {\n            g([1, (2)]);\n        }\n    }\n}\n";
        let r = Rope::from_str(src);
        let all = rust_depths(src, None);
        assert!(all.iter().any(|e| e.1 == 4), "four levels deep somewhere");
        for l in 0..r.len_lines() {
            for n in 1..=3 {
                let last = (l + n).min(r.len_lines());
                let bytes = r.line_to_byte(l)..if last >= r.len_lines() { r.len_bytes() } else { r.line_to_byte(last) };
                let chars = r.line_to_char(l)..r.line_to_char(last);
                let want: Vec<_> = all.iter().copied().filter(|e| chars.contains(&e.0)).collect();
                assert_eq!(rust_depths(src, Some(bytes)), want, "window {l}+{n}");
            }
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn a_stray_bracket_does_not_recolour_later_items() {
        let src = "fn a() { ( }\nfn b() { g(1) }\n";
        let got = rust_depths(src, None);
        let b = src.find("fn b").unwrap();
        let later: Vec<usize> = got.iter().filter(|e| e.0 >= b).map(|e| e.1).collect();
        // `fn b`: parameter parens, body brace, call parens, closing brace.
        assert_eq!(later, [0, 0, 0, 1, 1, 0], "{got:?}");
    }

    /// A string node that holds bracket tokens of its own (an f-string's
    /// interpolation) is skipped whole: Rust's literals hold none, so only
    /// this proves the guard.
    #[cfg(feature = "lang-python")]
    #[test]
    fn depths_skip_bracket_tokens_inside_a_string_node() {
        use crate::config::theme::Theme;
        let def = super::super::syntax::language_for_name("python").expect("python grammar");
        let mut syn = super::super::syntax::Syntax::new(def, &Theme::default()).unwrap();
        let src = "x = f\"{(1)}\"\ny = (2)\n";
        let r = Rope::from_str(src);
        syn.reparse(&r);
        let mut out = Vec::new();
        depths(&r, syn.tree().unwrap(), 0..r.len_bytes(), &mut out);
        let y = src.find("(2").unwrap();
        assert_eq!(out, [(y, 0), (y + 2, 0)], "only the brackets outside the f-string count");
    }

    #[cfg(feature = "lang-md")]
    #[test]
    fn depths_leave_markdown_prose_alone() {
        use crate::config::theme::Theme;
        let def = super::super::syntax::language_for_name("markdown").expect("markdown grammar");
        let mut syn = super::super::syntax::Syntax::new(def, &Theme::default()).unwrap();
        let src = "# T\n\nprose (a) [b](http://c.d)\n\n[foo]: /url (title)\n";
        let r = Rope::from_str(src);
        assert_eq!(syn.bracket_depths(&r, 0, r.len_lines()), [], "no bracket in prose, link text or a link definition");
    }
}
