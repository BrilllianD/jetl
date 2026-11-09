//! Increment and decrement the number at or after each caret
//! (Ctrl+Alt+A, Ctrl+Alt+X). `step_number` is the pure grammar on one line;
//! `Buffer::step_numbers` applies it at every caret as one undo step.
//!
//! Grammar, in char offsets of one line, ASCII digits only: a `0x`/`0X` hex
//! token (unsigned); otherwise a run of digits, negative when a `-` starts
//! the line or follows a char outside GLUE, unsigned after a GLUE char or a
//! joining `-` (`a-1`, `2026-11-08`), signed otherwise. The target is the
//! first token ending at or after the caret. Leading zeros keep the width;
//! overflow and a result below zero for an unsigned token change nothing.

use std::ops::Range;

use super::buffer::{Buffer, CaretEdit};
use super::undo::TxKind;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Hex,
    Negative,
    Signed,
    Unsigned,
}

/// A char that glues a following `-` or digit to the word before it.
fn is_glue(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ')' | ']')
}

/// The number at or after char column `col` of `line` stepped by `delta`:
/// the range to replace (char offsets of `line`) and its new text. `None`
/// when there is no such number or it cannot change.
pub(crate) fn step_number(line: &str, col: usize, delta: i64) -> Option<(Range<usize>, String)> {
    let c: Vec<char> = line.chars().collect();
    let n = c.len();
    let mut i = 0;
    while i < n {
        // (start, digits_start, end, kind)
        let tok = if c[i] == '0'
            && (i == 0 || !c[i - 1].is_ascii_digit())
            && i + 2 < n
            && matches!(c[i + 1], 'x' | 'X')
            && c[i + 2].is_ascii_hexdigit()
        {
            let mut e = i + 2;
            while e < n && c[e].is_ascii_hexdigit() {
                e += 1;
            }
            (i, i + 2, e, Kind::Hex)
        } else if c[i].is_ascii_digit() {
            let mut e = i;
            while e < n && c[e].is_ascii_digit() {
                e += 1;
            }
            match i.checked_sub(1).map(|p| c[p]) {
                Some('-') if i == 1 || !is_glue(c[i - 2]) => (i - 1, i, e, Kind::Negative),
                Some(p) if is_glue(p) || p == '-' => (i, i, e, Kind::Unsigned),
                _ => (i, i, e, Kind::Signed),
            }
        } else {
            i += 1;
            continue;
        };
        let (start, ds, end, kind) = tok;
        if end >= col {
            let digits: String = c[ds..end].iter().collect();
            let text = if kind == Kind::Hex {
                let new = u64::from_str_radix(&digits, 16).ok()?.checked_add_signed(delta)?;
                let upper = digits.chars().rev().find(char::is_ascii_alphabetic).is_some_and(|l| l.is_ascii_uppercase());
                let prefix: String = c[start..ds].iter().collect();
                let w = digits.len();
                if upper { format!("{prefix}{new:0w$X}") } else { format!("{prefix}{new:0w$x}") }
            } else {
                let sign = if kind == Kind::Negative { "-" } else { "" };
                let new = format!("{sign}{digits}").parse::<i64>().ok()?.checked_add(delta)?;
                if new < 0 && kind == Kind::Unsigned {
                    return None;
                }
                let w = if digits.len() > 1 && digits.starts_with('0') { digits.len() } else { 0 };
                let neg = if new < 0 { "-" } else { "" };
                format!("{neg}{:0w$}", new.unsigned_abs())
            };
            let old: String = c[start..end].iter().collect();
            return (text != old).then_some((start..end, text));
        }
        i = end;
    }
    None
}

impl Buffer {
    /// Add `delta` to the number at or after every caret (the start of a
    /// non-empty selection), one undo step. False when no caret found a
    /// number it could change. A read-only buffer stays and returns true.
    pub fn step_numbers(&mut self, delta: i64) -> bool {
        if self.read_only {
            return true;
        }
        let mut found = false;
        self.edit_carets(|b, sel| {
            let pos = if sel.is_empty() { sel.head } else { sel.range().start };
            let line = b.rope.char_to_line(pos);
            let ls = b.rope.line_to_char(line);
            let text = b.line_text(line);
            let col = (pos - ls).min(text.chars().count());
            match step_number(&text, col, delta) {
                Some((r, new)) => {
                    found = true;
                    CaretEdit::replace(ls + r.start..ls + r.end, new, TxKind::Other)
                }
                None => CaretEdit::keep(sel),
            }
        });
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::selection::{Selection, Selections};

    fn s(r: Range<usize>, t: &str) -> Option<(Range<usize>, String)> {
        Some((r, t.to_string()))
    }

    type Case = (&'static str, usize, i64, Option<(Range<usize>, String)>);

    fn check(cases: &[Case]) {
        for (line, col, delta, want) in cases {
            assert_eq!(step_number(line, *col, *delta), *want, "{line:?} col {col} delta {delta}");
        }
    }

    #[test]
    fn position() {
        check(&[
            ("a 42 b", 0, 1, s(2..4, "43")),
            ("a 42 b", 3, 1, s(2..4, "43")),
            ("a 42 b", 4, 1, s(2..4, "43")),
            ("a 42 b", 5, 1, None),
            ("1 2", 1, 1, s(0..1, "2")),
            ("1 2", 2, 1, s(2..3, "3")),
            ("abc", 0, 1, None),
            ("", 0, 1, None),
        ]);
    }

    #[test]
    fn carry_and_width() {
        check(&[
            ("9", 0, 1, s(0..1, "10")),
            ("999", 0, 1, s(0..3, "1000")),
            ("100", 0, -1, s(0..3, "99")),
            ("007", 0, 1, s(0..3, "008")),
            ("0099", 0, 1, s(0..4, "0100")),
            ("010", 0, -1, s(0..3, "009")),
        ]);
    }

    #[test]
    fn sign() {
        check(&[
            ("x = -1", 0, 1, s(4..6, "0")),
            ("x = 0", 0, -1, s(4..5, "-1")),
            ("f(-5)", 0, 1, s(2..4, "-4")),
            ("-001", 0, 1, s(0..4, "000")),
            ("000", 0, -1, s(0..3, "-001")),
            ("a-1", 0, 1, s(2..3, "2")),
            ("a-0", 0, -1, None),
            ("v0", 0, -1, None),
            ("2026-11-08", 5, 1, s(5..7, "12")),
            ("2026-11-00", 8, -1, None),
            ("1.0", 2, -1, None),
            ("a - 1", 0, -1, s(4..5, "0")),
            ("x1", 0, 1, s(1..2, "2")),
        ]);
    }

    #[test]
    fn hex() {
        check(&[
            ("0xff", 0, 1, s(0..4, "0x100")),
            ("0x0F", 0, 1, s(0..4, "0x10")),
            ("0x10", 0, -1, s(0..4, "0x0f")),
            ("0XFF", 0, -1, s(0..4, "0XFE")),
            ("0xAb", 0, 1, s(0..4, "0xac")),
            ("0x00", 0, -1, None),
            ("-0x10", 0, 1, s(1..5, "0x11")),
            ("0x", 0, 1, s(0..1, "1")),
            ("10x5", 0, 1, s(0..2, "11")),
            ("0xffffffffffffffff", 0, 1, None),
            ("0x10000000000000000", 0, 1, None),
        ]);
    }

    #[test]
    fn overflow() {
        check(&[
            ("9223372036854775807", 0, 1, None),
            ("-9223372036854775808", 0, -1, None),
            ("-9223372036854775808", 0, 1, s(0..20, "-9223372036854775807")),
            ("99999999999999999999", 0, 1, None),
        ]);
    }

    #[test]
    fn non_ascii() {
        check(&[("é 5", 0, 1, s(2..3, "6")), ("٣", 0, 1, None)]);
    }

    fn buf(text: &str, carets: &[usize]) -> Buffer {
        let mut b = Buffer::from_text(1, None, text, false);
        b.view.sels = Selections::new(carets.iter().map(|&c| Selection::point(c)).collect(), 0);
        b
    }

    fn heads(b: &Buffer) -> Vec<usize> {
        b.view.sels.iter().map(|s| s.head).collect()
    }

    #[test]
    fn steps_every_caret_in_one_undo_step() {
        let mut b = buf("a 1\nb 9\nc\n", &[0, 4, 8]);
        assert!(b.step_numbers(1));
        assert_eq!(b.text(), "a 2\nb 10\nc\n");
        assert_eq!(heads(&b), [3, 8, 9]);
        assert_eq!(b.undo.len(), 1);
        b.undo();
        assert_eq!(b.text(), "a 1\nb 9\nc\n");
    }

    #[test]
    fn two_carets_on_one_number_change_it_once() {
        let mut b = buf("x 41 y", &[0, 3]);
        assert!(b.step_numbers(1));
        assert_eq!(b.text(), "x 42 y");
        assert_eq!(heads(&b), [4]);
    }

    #[test]
    fn nothing_to_change_pushes_no_undo_step() {
        let mut b = buf("abc", &[0]);
        let n = b.undo.len();
        assert!(!b.step_numbers(1));
        assert_eq!(b.text(), "abc");
        assert_eq!(b.undo.len(), n);
    }

    #[test]
    fn selection_counts_from_its_start() {
        let mut b = Buffer::from_text(1, None, "1 2", false);
        b.view.set_sel(Selection::new(2, 3));
        assert!(b.step_numbers(1));
        assert_eq!(b.text(), "1 3");
        assert_eq!(b.view.sel(), Selection::point(3));
    }

    #[test]
    fn read_only_text_stays() {
        let mut b = buf("5", &[0]);
        b.read_only = true;
        assert!(b.step_numbers(1));
        assert_eq!(b.text(), "5");
    }

    #[test]
    fn crlf_line() {
        let mut b = buf("a 9\r\nb\r\n", &[0]);
        assert!(b.step_numbers(1));
        assert_eq!(b.text(), "a 10\r\nb\r\n");
    }
}
