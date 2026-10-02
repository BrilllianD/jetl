//! Bracket matching for the pair highlight and "move caret to matching
//! bracket". With a syntax tree only real bracket tokens match, so brackets
//! inside strings and comments are ignored; without one a nesting scan over
//! the text is used.

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

fn in_literal(node: Node) -> bool {
    let mut n = Some(node);
    while let Some(x) = n {
        let k = x.kind();
        if k.contains("string") || k.contains("comment") || k == "char_literal" {
            return true;
        }
        n = x.parent();
    }
    false
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
}
