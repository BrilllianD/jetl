//! LSP positions: a line and a UTF-16 code unit offset in it (jetl asks
//! servers for `utf-16`, the encoding every server supports). Buffers use
//! char indices; these convert at the boundary, so LSP positions never
//! enter `Buffer`. Positions past the end of a line or of the document
//! clamp to it, as the protocol asks.

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
}
