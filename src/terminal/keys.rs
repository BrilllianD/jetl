//! Input for the shell, encoded as xterm sends it (`TERM=xterm-256color`):
//! keys (with the application cursor mode the program asked for), mouse
//! reports in the protocol and encoding it enabled, and paste.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// The bytes for `key`, or `None` for keys a terminal does not send
/// (modifier presses, media keys). Alt prefixes ESC; modified cursor and
/// function keys carry xterm's modifier parameter instead.
pub fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let m = key.modifiers;
    let (shift, alt, ctrl) = (m.contains(KeyModifiers::SHIFT), m.contains(KeyModifiers::ALT), m.contains(KeyModifiers::CONTROL));
    let param = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    // Cursor keys, Home and End: `CSI 1;m X` when modified, SS3 in
    // application cursor mode, else CSI.
    let cursor = |c: char| match (param > 1, app_cursor) {
        (true, _) => format!("\x1b[1;{param}{c}"),
        (false, true) => format!("\x1bO{c}"),
        (false, false) => format!("\x1b[{c}"),
    };
    let tilde = |n: u8| if param > 1 { format!("\x1b[{n};{param}~") } else { format!("\x1b[{n}~") };
    let with_alt = |b: &[u8]| if alt { [b"\x1b", b].concat() } else { b.to_vec() };
    let s = match key.code {
        KeyCode::Char(c) => return Some(char_bytes(c, ctrl, alt)),
        KeyCode::Enter => return Some(with_alt(b"\r")),
        KeyCode::Tab if shift => return Some(b"\x1b[Z".to_vec()),
        KeyCode::Tab => return Some(with_alt(b"\t")),
        KeyCode::BackTab => return Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => return Some(with_alt(if ctrl { b"\x08" } else { b"\x7f" })),
        KeyCode::Esc => return Some(with_alt(b"\x1b")),
        KeyCode::Null => return Some(vec![0]),
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let c = char::from(b'P' + n - 1);
            if param > 1 { format!("\x1b[1;{param}{c}") } else { format!("\x1bO{c}") }
        }
        KeyCode::F(n) => tilde(match n {
            5 => 15,
            6 => 17,
            7 => 18,
            8 => 19,
            9 => 20,
            10 => 21,
            11 => 23,
            12 => 24,
            _ => return None,
        }),
        _ => return None,
    };
    Some(s.into_bytes())
}

/// A character: Ctrl makes it a control code where one exists (Ctrl+A is
/// 0x01, Ctrl+Space NUL, Ctrl+[ ESC, Ctrl+/ 0x1f, Ctrl+? DEL), Alt prefixes
/// ESC.
fn char_bytes(c: char, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    if alt {
        out.push(0x1b);
    }
    let control = match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '~' | '6' => Some(0x1e),
        '_' | '/' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    };
    match control.filter(|_| ctrl) {
        Some(b) => out.push(b),
        None => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
    }
    out
}

/// A mouse event at the 0-based cell (`col`, `row`) as the program asked for
/// it, or `None` when its mode does not report it (or the default encoding
/// cannot reach the cell).
pub fn encode_mouse(
    kind: MouseEventKind,
    mods: KeyModifiers,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use MouseProtocolMode as P;
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0u16,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let motion = matches!(mode, P::ButtonMotion | P::AnyMotion);
    let (mut code, release) = match kind {
        _ if mode == P::None => return None,
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(_) if mode == P::Press => return None,
        MouseEventKind::Up(b) => (button(b), true),
        MouseEventKind::Drag(b) if motion => (button(b) + 32, false),
        MouseEventKind::Moved if mode == P::AnyMotion => (3 + 32, false),
        MouseEventKind::Drag(_) | MouseEventKind::Moved => return None,
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if mode == P::Press {
        // X10 reports presses without modifiers.
    } else {
        code += 4 * u16::from(mods.contains(KeyModifiers::SHIFT))
            + 8 * u16::from(mods.contains(KeyModifiers::ALT))
            + 16 * u16::from(mods.contains(KeyModifiers::CONTROL));
    }
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    match encoding {
        MouseProtocolEncoding::Sgr => Some(format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes()),
        // The old encodings report a release as button 3.
        legacy => {
            let code = if release { (code & !3) | 3 } else { code };
            let mut out = b"\x1b[M".to_vec();
            for v in [u32::from(code), x, y] {
                let v = v + 32;
                match legacy {
                    MouseProtocolEncoding::Utf8 => out.extend_from_slice(char::from_u32(v)?.encode_utf8(&mut [0; 4]).as_bytes()),
                    _ => out.push(u8::try_from(v).ok()?),
                }
            }
            Some(out)
        }
    }
}

/// Pasted text: line breaks become CR, as a terminal sends Enter, and the
/// text is wrapped in bracketed paste markers when the program enabled
/// them (with any end marker inside removed, so the paste cannot end early).
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed { format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", "")).into_bytes() } else { text.into_bytes() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode_key(&KeyEvent::new(code, mods), false).unwrap()
    }

    #[test]
    fn printable_control_and_alt_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(key(KeyCode::Char('a'), none), b"a");
        assert_eq!(key(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(key(KeyCode::Char('é'), none), "é".as_bytes());
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), b"\x03");
        assert_eq!(key(KeyCode::Char('D'), KeyModifiers::CONTROL | KeyModifiers::SHIFT), b"\x04");
        assert_eq!(key(KeyCode::Char(' '), KeyModifiers::CONTROL), b"\x00");
        assert_eq!(key(KeyCode::Char('['), KeyModifiers::CONTROL), b"\x1b");
        assert_eq!(key(KeyCode::Char('/'), KeyModifiers::CONTROL), b"\x1f");
        assert_eq!(key(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(key(KeyCode::Char('x'), KeyModifiers::ALT | KeyModifiers::CONTROL), b"\x1b\x18");
        assert_eq!(key(KeyCode::Enter, none), b"\r");
        assert_eq!(key(KeyCode::Enter, KeyModifiers::ALT), b"\x1b\r");
        assert_eq!(key(KeyCode::Tab, none), b"\t");
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(key(KeyCode::Backspace, none), b"\x7f");
        assert_eq!(key(KeyCode::Backspace, KeyModifiers::ALT), b"\x1b\x7f");
        assert_eq!(key(KeyCode::Backspace, KeyModifiers::CONTROL), b"\x08");
        assert_eq!(key(KeyCode::Esc, none), b"\x1b");
        assert!(encode_key(&KeyEvent::new(KeyCode::CapsLock, none), false).is_none());
    }

    #[test]
    fn cursor_editing_and_function_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(key(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, none), b"\x1b[D");
        assert_eq!(key(KeyCode::Home, none), b"\x1b[H");
        assert_eq!(key(KeyCode::End, none), b"\x1b[F");
        assert_eq!(key(KeyCode::Right, KeyModifiers::CONTROL), b"\x1b[1;5C");
        assert_eq!(key(KeyCode::Down, KeyModifiers::SHIFT | KeyModifiers::ALT), b"\x1b[1;4B");
        // Application cursor mode (vim, less) uses SS3, unless modified.
        let app = |code| encode_key(&KeyEvent::new(code, none), true).unwrap();
        assert_eq!(app(KeyCode::Up), b"\x1bOA");
        assert_eq!(app(KeyCode::End), b"\x1bOF");
        assert_eq!(encode_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), true).unwrap(), b"\x1b[1;5A");
        assert_eq!(key(KeyCode::Insert, none), b"\x1b[2~");
        assert_eq!(key(KeyCode::Delete, none), b"\x1b[3~");
        assert_eq!(key(KeyCode::Delete, KeyModifiers::CONTROL), b"\x1b[3;5~");
        assert_eq!(key(KeyCode::PageUp, none), b"\x1b[5~");
        assert_eq!(key(KeyCode::PageDown, none), b"\x1b[6~");
        assert_eq!(key(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(key(KeyCode::F(4), none), b"\x1bOS");
        assert_eq!(key(KeyCode::F(2), KeyModifiers::SHIFT), b"\x1b[1;2Q");
        assert_eq!(key(KeyCode::F(5), none), b"\x1b[15~");
        assert_eq!(key(KeyCode::F(10), none), b"\x1b[21~");
        assert_eq!(key(KeyCode::F(12), KeyModifiers::CONTROL), b"\x1b[24;5~");
        assert!(encode_key(&KeyEvent::new(KeyCode::F(13), none), false).is_none());
    }

    #[test]
    fn mouse_reports_follow_mode_and_encoding() {
        use MouseProtocolEncoding as E;
        use MouseProtocolMode as P;
        let none = KeyModifiers::NONE;
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(encode_mouse(down, none, 0, 0, P::None, E::Sgr), None);
        assert_eq!(encode_mouse(down, none, 4, 2, P::PressRelease, E::Sgr).unwrap(), b"\x1b[<0;5;3M");
        let up = MouseEventKind::Up(MouseButton::Left);
        assert_eq!(encode_mouse(up, none, 4, 2, P::PressRelease, E::Sgr).unwrap(), b"\x1b[<0;5;3m");
        assert_eq!(encode_mouse(up, none, 4, 2, P::Press, E::Sgr), None);
        assert_eq!(
            encode_mouse(MouseEventKind::ScrollDown, KeyModifiers::CONTROL, 0, 0, P::PressRelease, E::Sgr).unwrap(),
            b"\x1b[<81;1;1M"
        );
        let drag = MouseEventKind::Drag(MouseButton::Left);
        assert_eq!(encode_mouse(drag, none, 1, 1, P::PressRelease, E::Sgr), None);
        assert_eq!(encode_mouse(drag, none, 1, 1, P::ButtonMotion, E::Sgr).unwrap(), b"\x1b[<32;2;2M");
        // The default encoding: 32 + value per byte, a release is button 3.
        assert_eq!(encode_mouse(down, none, 0, 0, P::PressRelease, E::Default).unwrap(), b"\x1b[M\x20\x21\x21");
        assert_eq!(encode_mouse(up, none, 0, 0, P::PressRelease, E::Default).unwrap(), b"\x1b[M\x23\x21\x21");
        assert_eq!(encode_mouse(down, none, 300, 0, P::PressRelease, E::Default), None);
        assert_eq!(encode_mouse(down, none, 300, 0, P::PressRelease, E::Utf8).unwrap(), "\x1b[M\x20\u{14d}\x21".as_bytes());
    }

    #[test]
    fn paste_uses_cr_and_brackets_when_asked() {
        assert_eq!(encode_paste("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(encode_paste("ls\n", true), b"\x1b[200~ls\r\x1b[201~");
        assert_eq!(encode_paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
    }
}
