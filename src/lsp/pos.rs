//! LSP positions: a line and a UTF-16 code unit offset in it (jetl asks
//! servers for `utf-16`, the encoding every server supports). Buffers use
//! char indices; these convert at the boundary, so LSP positions never
//! enter `Buffer`. Positions past the end of a line or of the document
//! clamp to it, as the protocol asks. Lines are the rope's: jetl builds
//! ropey with `cr_lines` only, so they break at `\n`, `\r\n` and `\r`, as
//! LSP does (see Cargo.toml).

use lsp_types::Position;
use ropey::Rope;

use crate::editor::selection::line_len_no_eol;

/// The position of char index `idx` (clamped to the end of the text).
pub fn position(rope: &Rope, idx: usize) -> Position {
    let idx = idx.min(rope.len_chars());
    let line = rope.char_to_line(idx);
    let start = rope.line_to_char(line);
    let col = rope.char_to_utf16_cu(idx) - rope.char_to_utf16_cu(start);
    Position::new(line as u32, col as u32)
}

/// The char index of `pos`. A line past the end is the end of the text; a
/// column past the end of its line is the end of the line (before the line
/// break); a column inside a surrogate pair is the char it belongs to.
pub fn char_index(rope: &Rope, pos: Position) -> usize {
    let line = pos.line as usize;
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    let start = rope.line_to_char(line);
    let end = start + line_len_no_eol(rope, line);
    let start16 = rope.char_to_utf16_cu(start);
    let end16 = rope.char_to_utf16_cu(end);
    let at16 = (start16 + pos.character as usize).min(end16);
    rope.utf16_cu_to_char(at16)
}

/// The char column of UTF-16 column `character` in `line` (one line's
/// text, without its break), clamped to the end of the line; a column
/// inside a surrogate pair is the char it belongs to. For text that is not
/// in a rope (a line read from disk).
pub fn line_char_col(line: &str, character: u32) -> usize {
    let mut units = 0;
    for (i, c) in line.chars().enumerate() {
        units += c.len_utf16();
        if units > character as usize {
            return i;
        }
    }
    line.chars().count()
}

/// Whether char index `idx` falls between the `\r` and `\n` of a CRLF
/// line break, a place no LSP position can name.
pub fn inside_crlf(rope: &Rope, idx: usize) -> bool {
    idx > 0 && idx < rope.len_chars() && rope.char(idx - 1) == '\r' && rope.char(idx) == '\n'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns_on_astral_and_cjk_text() {
        let r = Rope::from_str("a😀b\n日本");
        let p = |line, character| Position::new(line, character);
        // a, 😀 (two code units), b, \n; then 日 and 本 (one each).
        let expect = [(0, p(0, 0)), (1, p(0, 1)), (2, p(0, 3)), (3, p(0, 4)), (4, p(1, 0)), (5, p(1, 1)), (6, p(1, 2))];
        for (idx, pos) in expect {
            assert_eq!(position(&r, idx), pos, "char {idx}");
            assert_eq!(char_index(&r, pos), idx, "{pos:?}");
        }
        assert_eq!(position(&r, 99), p(1, 2), "past the end clamps");
        // Inside the surrogate pair: the emoji itself.
        assert_eq!(char_index(&r, p(0, 2)), 1);
        // Past the end of a line: its end, before the break.
        assert_eq!(char_index(&r, p(0, 40)), 3);
        assert_eq!(char_index(&r, p(1, 40)), 6);
        // Past the last line: the end of the text.
        assert_eq!(char_index(&r, p(2, 0)), 6);
        assert_eq!(char_index(&r, p(9, 9)), 6);
    }

    #[test]
    fn line_columns_on_cjk_and_surrogate_pairs() {
        let line = "日本😀x𝄞y";
        // 日 本 😀(2) x 𝄞(2) y: units 0 1 2-3 4 5-6 7.
        let expect = [(0, 0), (1, 1), (2, 2), (3, 2), (4, 3), (5, 4), (6, 4), (7, 5), (8, 6), (99, 6)];
        for (unit, ch) in expect {
            assert_eq!(line_char_col(line, unit), ch, "unit {unit}");
        }
        // Agrees with the rope conversion on whole-char columns.
        let r = Rope::from_str(line);
        for unit in [0, 1, 2, 4, 5, 7, 8] {
            assert_eq!(line_char_col(line, unit), char_index(&r, Position::new(0, unit)), "unit {unit}");
        }
    }

    #[test]
    fn crlf_lines() {
        let r = Rope::from_str("ab\r\ncd");
        assert_eq!(position(&r, 2), Position::new(0, 2));
        assert_eq!(position(&r, 4), Position::new(1, 0));
        assert_eq!(char_index(&r, Position::new(0, 9)), 2, "before the \\r");
        assert!(inside_crlf(&r, 3) && !inside_crlf(&r, 2) && !inside_crlf(&r, 4));
        // An empty last line after a final break.
        let r = Rope::from_str("x\n");
        assert_eq!(position(&r, 2), Position::new(1, 0));
        assert_eq!(char_index(&r, Position::new(1, 5)), 2);
    }

    #[test]
    fn ropey_breaks_lines_like_lsp() {
        let hint = "ropey counts a Unicode separator as a line break: check \
                    `cargo tree -i ropey -e features` for `unicode_lines`";
        let r = Rope::from_str("a\u{b}b\u{c}c\u{85}d\u{2028}e\u{2029}f\rg\r\nh\ni");
        assert_eq!(r.len_lines(), 4, "{hint}");
        let at = |c: char| r.chars().position(|x| x == c).unwrap();
        assert_eq!(r.char_to_line(at('f')), 0, "{hint}");
        assert_eq!(r.char_to_line(at('g')), 1);
        assert_eq!(r.char_to_line(at('h')), 2);
        assert_eq!(r.char_to_line(at('i')), 3);
        assert_eq!(Rope::from_str("x\u{2028}").len_lines(), 1, "{hint}");
    }

    #[test]
    fn unicode_separators_are_columns() {
        let r = Rope::from_str("a\u{c}b\u{2028}😀c\nd");
        let p = |line, character| Position::new(line, character);
        // a, FF, b, LS, 😀 (two code units), c, \n; then d and the end.
        let expect =
            [(0, p(0, 0)), (1, p(0, 1)), (2, p(0, 2)), (3, p(0, 3)), (4, p(0, 4)), (5, p(0, 6)), (6, p(0, 7)), (7, p(1, 0)), (8, p(1, 1))];
        for (idx, pos) in expect {
            assert_eq!(position(&r, idx), pos, "char {idx}");
            assert_eq!(char_index(&r, pos), idx, "{pos:?}");
        }
        assert_eq!(char_index(&r, p(0, 99)), 6, "the \\n");
        assert_eq!(position(&r, r.len_chars()), p(1, 1));
    }

    #[test]
    fn utf16_edge_cases_on_empty_text_lone_cr_and_crlf_after_an_emoji() {
        let r = Rope::from_str("");
        assert_eq!(position(&r, 0), Position::new(0, 0));
        assert_eq!(position(&r, 5), Position::new(0, 0));
        assert_eq!(char_index(&r, Position::new(0, 5)), 0);
        assert_eq!(char_index(&r, Position::new(3, 0)), 0);
        assert_eq!(line_char_col("", 4), 0);
        assert!(!inside_crlf(&r, 0));

        // A lone CR breaks the line.
        let r = Rope::from_str("a\rb");
        assert_eq!(position(&r, 2), Position::new(1, 0));
        assert_eq!(char_index(&r, Position::new(0, 9)), 1, "before the \\r");
        assert_eq!(char_index(&r, Position::new(1, 1)), 3);

        let r = Rope::from_str("x😀\r\ny");
        assert_eq!(position(&r, 2), Position::new(0, 3));
        assert_eq!(position(&r, 4), Position::new(1, 0));
        assert_eq!(char_index(&r, Position::new(0, 2)), 1, "inside the pair: the emoji");
        assert_eq!(char_index(&r, Position::new(0, 3)), 2);
        assert_eq!(char_index(&r, Position::new(0, 9)), 2, "end of line, before the \\r");
        assert!(inside_crlf(&r, 3) && !inside_crlf(&r, 2));
    }
}
