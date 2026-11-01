//! The capture form of the Settings Keymap Add/Edit Key dialog: a first-stroke field that records
//! a key press, a "Second" toggle enabling a second-stroke field, and a text field as the
//! fallback for keys the terminal cannot deliver. Hosted as `DialogKind::KeyCapture`; answers
//! `DialogResult::Text`. The text field is the single source of truth: a capture rewrites it, and
//! editing it re-derives the shown strokes.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::keymap::{KeyCombo, chord_key, chord_text, parse_chord};
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::dialog::{DialogResult, cut_rows, message_rows};
use crate::ui::widgets::text_input::TextInput;
use crate::util::text::fit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    First,
    Toggle,
    Second,
    Text,
    Ok,
    Cancel,
}

const HINT_CAPTURING: &str = "Press a key to record it · Esc: cancel";
const HINT_IDLE: &str = "Tab: next · Space: toggle · Enter: OK · Esc: cancel";

#[derive(Debug, Clone)]
pub struct KeyCaptureForm {
    strokes: [Option<KeyCombo>; 2],
    second: bool,
    focus: Part,
    text: TextInput,
    rect: Rect,
    first_rect: Rect,
    toggle_rect: Rect,
    second_rect: Rect,
    text_rect: Rect,
    button_rects: [Rect; 2],
}

impl KeyCaptureForm {
    pub fn new(initial: &str) -> Self {
        let mut f = Self {
            strokes: [None, None],
            second: false,
            focus: Part::First,
            text: TextInput::new(false).with_text(initial),
            rect: Rect::default(),
            first_rect: Rect::default(),
            toggle_rect: Rect::default(),
            second_rect: Rect::default(),
            text_rect: Rect::default(),
            button_rects: [Rect::default(); 2],
        };
        f.sync_from_text();
        f
    }

    pub fn text(&self) -> &str {
        &self.text.text
    }

    pub fn focus(&self) -> Part {
        self.focus
    }

    /// An armed capture field has focus: the next press is recorded.
    pub fn capturing(&self) -> bool {
        matches!(self.focus, Part::First | Part::Second)
    }

    fn parts(&self) -> Vec<Part> {
        let mut v = vec![Part::First, Part::Toggle];
        if self.second {
            v.push(Part::Second);
        }
        v.extend([Part::Text, Part::Ok, Part::Cancel]);
        v
    }

    fn step(&mut self, back: bool) {
        let parts = self.parts();
        let n = parts.len();
        let from = parts.iter().position(|p| *p == self.focus).unwrap_or(1);
        self.focus = parts[if back { (from + n - 1) % n } else { (from + 1) % n }];
    }

    fn write_text(&mut self) {
        let mut chord: Vec<KeyCombo> = self.strokes[0].into_iter().collect();
        if self.second {
            chord.extend(self.strokes[1]);
        }
        if !chord.is_empty() {
            self.text.set_text(&chord_key(&chord));
        }
    }

    fn sync_from_text(&mut self) {
        match parse_chord(&self.text.text) {
            Ok(c) if c.len() == 1 => self.strokes = [Some(c[0]), None],
            Ok(c) if c.len() == 2 => {
                self.strokes = [Some(c[0]), Some(c[1])];
                self.second = true;
            }
            _ => self.strokes = [None, None],
        }
    }

    fn toggle(&mut self) {
        self.second = !self.second;
        if !self.second {
            self.strokes[1] = None;
        }
        if self.strokes[0].is_some() {
            self.write_text();
        }
    }

    fn capture(&mut self, k: KeyCombo) {
        let i = usize::from(self.focus == Part::Second);
        self.strokes[i] = Some(k);
        self.write_text();
        self.focus = if i == 0 && self.second && self.strokes[1].is_none() { Part::Second } else { Part::Ok };
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<DialogResult> {
        if key.code == KeyCode::Esc {
            return Some(DialogResult::Cancel);
        }
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        if self.capturing() {
            self.capture(KeyCombo::normalize(key.code, key.modifiers));
            return None;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::BackTab | KeyCode::Up => {
                self.step(true);
                return None;
            }
            KeyCode::Tab if shift => {
                self.step(true);
                return None;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.step(false);
                return None;
            }
            _ => {}
        }
        let answer = || Some(DialogResult::Text(self.text.text.clone()));
        match self.focus {
            Part::Text => {
                if key.code == KeyCode::Enter {
                    return answer();
                }
                if crate::ui::new_task::edit(&mut self.text, key) {
                    self.sync_from_text();
                }
                None
            }
            Part::Toggle => {
                if matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
                    self.toggle();
                }
                None
            }
            Part::Ok => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => answer(),
                KeyCode::Left | KeyCode::Right => {
                    self.focus = Part::Cancel;
                    None
                }
                _ => None,
            },
            Part::Cancel => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => Some(DialogResult::Cancel),
                KeyCode::Left | KeyCode::Right => {
                    self.focus = Part::Ok;
                    None
                }
                _ => None,
            },
            Part::First | Part::Second => None,
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.focus = Part::Text;
        self.text.insert_str(&text.replace("\r\n", " ").replace(['\n', '\r'], " "));
        self.sync_from_text();
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<DialogResult> {
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        let p = Position::new(m.x, m.y);
        if let Some(i) = self.button_rects.iter().position(|r| r.contains(p)) {
            return Some(if i == 0 { DialogResult::Text(self.text.text.clone()) } else { DialogResult::Cancel });
        }
        if self.first_rect.contains(p) {
            self.focus = Part::First;
        } else if self.toggle_rect.contains(p) {
            self.toggle();
            self.focus = Part::Toggle;
        } else if self.second_rect.contains(p) {
            if self.second {
                self.focus = Part::Second;
            }
        } else if self.text_rect.contains(p) {
            self.focus = Part::Text;
            self.text.click(self.text_rect, m.x, m.y);
        } else if !self.rect.contains(p) {
            return Some(DialogResult::Cancel);
        }
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, title: &str, message: &str) -> Rect {
        let msg_w = message.lines().map(|l| l.width()).max().unwrap_or(0);
        let w = (msg_w + 4).clamp(50, 72).max(title.width() + 4);
        let w = (w as u16).min(area.width.saturating_sub(2)).max(10).min(area.width);
        let msg_rows = message_rows(message, (w as usize).saturating_sub(4));
        let h = (msg_rows.len() + 5 + 2) as u16;
        let h = h.min(area.height.saturating_sub(1)).max(5).min(area.height);
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 3;
        let rect = Rect::new(x, y, w, h);
        self.rect = rect;
        frame.render_widget(Clear, rect);
        let base = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.popup_border)
            .title(format!(" {title} "))
            .title_style(theme.title_focused)
            .style(base);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        self.first_rect = Rect::default();
        self.toggle_rect = Rect::default();
        self.second_rect = Rect::default();
        self.text_rect = Rect::default();
        self.button_rects = [Rect::default(); 2];
        if inner.height < 4 || inner.width < 20 {
            return rect;
        }
        let hint = u16::from(inner.height >= 5);
        let buttons_y = inner.bottom() - 1;
        let text_y = inner.bottom() - 2 - hint;
        let second_y = text_y - 1;
        let first_y = second_y - 1;
        let mut msg_rows = msg_rows;
        let room = (inner.height - 4 - hint) as usize;
        if msg_rows.len() > room {
            cut_rows(&mut msg_rows, room, inner.width.saturating_sub(2) as usize);
        }
        let label_x = inner.x + 1;
        let field_x = inner.x + 15;
        let field_w = inner.width - 16;
        self.first_rect = Rect::new(field_x, first_y, field_w, 1);
        self.toggle_rect = Rect::new(label_x, second_y, 10, 1);
        if self.second {
            self.second_rect = Rect::new(field_x, second_y, field_w, 1);
        }
        self.text_rect = Rect::new(field_x, text_y, field_w, 1);
        let field_base = Style::default().bg(theme.bg).fg(theme.fg);
        let bold = |on: bool| if on { base.add_modifier(Modifier::BOLD) } else { base };
        let buf = frame.buffer_mut();
        for (i, line) in msg_rows.iter().enumerate() {
            buf.set_stringn(label_x, inner.y + i as u16, line, (inner.width - 2) as usize, Style::default().fg(theme.fg));
        }
        buf.set_stringn(label_x, first_y, "First stroke", 13, bold(self.focus == Part::First));
        let toggle_label = if self.second { "[x] Second" } else { "[ ] Second" };
        let toggle_style = if self.focus == Part::Toggle { theme.list_selected } else { base };
        buf.set_stringn(label_x, second_y, toggle_label, 13, toggle_style);
        buf.set_stringn(label_x, text_y, "Or type", 13, bold(self.focus == Part::Text));
        for (idx, r, enabled) in [(0usize, self.first_rect, true), (1, self.second_rect, self.second)] {
            if r.is_empty() && enabled {
                continue;
            }
            let rr = if enabled { r } else { Rect::new(field_x, second_y, field_w, 1) };
            let armed = enabled && self.capturing() && self.focus == if idx == 0 { Part::First } else { Part::Second };
            let st = if !enabled {
                field_base.patch(theme.dim)
            } else if armed {
                theme.list_selected
            } else {
                field_base
            };
            buf.set_style(rr, st);
            let content = match self.strokes[idx] {
                Some(k) if enabled => chord_text(&[k]),
                None if armed => "Press a key".to_string(),
                _ => String::new(),
            };
            buf.set_stringn(rr.x, rr.y, &content, rr.width as usize, st);
        }
        let hint_text = if self.capturing() { HINT_CAPTURING } else { HINT_IDLE };
        if hint == 1 {
            buf.set_stringn(
                label_x,
                inner.bottom() - 2,
                fit(hint_text, (inner.width - 2) as usize),
                (inner.width - 2) as usize,
                base.patch(theme.dim),
            );
        }
        let labels = ["[ OK ]", "[ Cancel ]"];
        let total: u16 = labels.iter().map(|l| l.width() as u16).sum::<u16>() + 1;
        let mut bx = inner.x + inner.width.saturating_sub(total) / 2;
        for (i, l) in labels.iter().enumerate() {
            let bw = l.width() as u16;
            let r = Rect::new(bx, buttons_y, bw, 1).intersection(inner);
            let focused = self.focus == if i == 0 { Part::Ok } else { Part::Cancel };
            let st = if focused { theme.list_selected.add_modifier(Modifier::BOLD) } else { base };
            buf.set_stringn(r.x, buttons_y, l, r.width as usize, st);
            self.button_rects[i] = r;
            bx += bw + 1;
        }
        self.text.render(frame, self.text_rect, field_base, "", theme.dim, self.focus == Part::Text);
        rect
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, ModifierKeyCode};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn plain(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn click(x: u16, y: u16) -> Mouse {
        Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks: 1, mods: KeyModifiers::NONE }
    }

    fn center(r: Rect) -> (u16, u16) {
        (r.x + r.width / 2, r.y)
    }

    fn draw(f: &mut KeyCaptureForm, w: u16, h: u16, message: &str) -> Vec<String> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| {
            f.render(fr, Rect::new(0, 0, w, h), &theme, "Add Key", message);
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    fn screen(f: &mut KeyCaptureForm, w: u16, h: u16) -> Vec<String> {
        draw(f, w, h, "")
    }

    fn back(f: &mut KeyCaptureForm, n: usize) {
        for _ in 0..n {
            f.handle_key(plain(KeyCode::BackTab));
        }
    }

    #[test]
    fn opens_armed_on_the_first_stroke() {
        let f = KeyCaptureForm::new("");
        assert!(f.capturing());
        assert_eq!(f.focus(), Part::First);
        assert_eq!(f.text(), "");
    }

    #[test]
    fn a_press_is_recorded_and_enter_on_ok_answers() {
        let mut f = KeyCaptureForm::new("");
        let ev = key(KeyCode::Char('j'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert!(f.handle_key(ev).is_none());
        assert_eq!(f.text(), "Ctrl+Alt+J");
        assert_eq!(f.focus(), Part::Ok);
        assert!(!f.capturing());
        assert!(matches!(f.handle_key(plain(KeyCode::Enter)), Some(DialogResult::Text(t)) if t == "Ctrl+Alt+J"));
    }

    #[test]
    fn letters_keep_the_written_spelling() {
        for (ev, want) in
            [(plain(KeyCode::Char('g')), "g"), (key(KeyCode::Char('J'), KeyModifiers::SHIFT), "Shift+J"), (plain(KeyCode::F(5)), "F5")]
        {
            let mut f = KeyCaptureForm::new("");
            f.handle_key(ev);
            assert_eq!(f.text(), want);
            assert_eq!(parse_chord(f.text()).unwrap(), vec![KeyCombo::normalize(ev.code, ev.modifiers)]);
        }
    }

    #[test]
    fn tab_and_enter_are_captured_while_armed() {
        let mut f = KeyCaptureForm::new("");
        assert!(f.handle_key(plain(KeyCode::Tab)).is_none());
        assert_eq!(f.text(), "Tab");
        back(&mut f, 3);
        assert_eq!(f.focus(), Part::First);
        assert!(f.handle_key(plain(KeyCode::Enter)).is_none());
        assert_eq!(f.text(), "Enter");
    }

    #[test]
    fn esc_cancels_even_while_armed() {
        let mut f = KeyCaptureForm::new("ctrl-k");
        assert!(f.capturing());
        assert!(matches!(f.handle_key(plain(KeyCode::Esc)), Some(DialogResult::Cancel)));
        assert_eq!(f.text(), "ctrl-k");
    }

    #[test]
    fn modifier_presses_and_releases_are_ignored() {
        let mut f = KeyCaptureForm::new("");
        assert!(f.handle_key(plain(KeyCode::Modifier(ModifierKeyCode::LeftControl))).is_none());
        let mut rel = plain(KeyCode::Char('a'));
        rel.kind = KeyEventKind::Release;
        rel.state = KeyEventState::NONE;
        assert!(f.handle_key(rel).is_none());
        assert!(f.capturing());
        assert_eq!(f.text(), "");
    }

    #[test]
    fn two_strokes_with_the_toggle() {
        let mut f = KeyCaptureForm::new("");
        f.handle_key(ctrl('k'));
        back(&mut f, 2);
        assert_eq!(f.focus(), Part::Toggle);
        f.handle_key(plain(KeyCode::Char(' ')));
        assert_eq!(f.text(), "Ctrl+K");
        f.handle_key(plain(KeyCode::Tab));
        assert_eq!(f.focus(), Part::Second);
        assert!(f.capturing());
        f.handle_key(ctrl('c'));
        assert_eq!(f.text(), "Ctrl+K Ctrl+C");
        assert_eq!(f.focus(), Part::Ok);
        assert!(matches!(f.handle_key(plain(KeyCode::Enter)), Some(DialogResult::Text(t)) if t == "Ctrl+K Ctrl+C"));
        // BackTab would land in the armed second field and be captured: click the toggle.
        screen(&mut f, 80, 24);
        let (x, y) = center(f.toggle_rect);
        f.handle_mouse(&click(x, y));
        assert_eq!(f.focus(), Part::Toggle);
        assert_eq!(f.text(), "Ctrl+K");
    }

    #[test]
    fn first_capture_moves_to_an_empty_second() {
        let mut f = KeyCaptureForm::new("");
        screen(&mut f, 80, 24);
        let (x, y) = (f.toggle_rect.x, f.toggle_rect.y);
        f.handle_mouse(&click(x, y));
        assert_eq!(f.focus(), Part::Toggle);
        assert!(f.second);
        let (x, y) = center(f.first_rect);
        f.handle_mouse(&click(x, y));
        f.handle_key(ctrl('k'));
        assert_eq!(f.focus(), Part::Second);
        assert!(f.capturing());
        f.handle_key(ctrl('c'));
        assert_eq!(f.focus(), Part::Ok);

        let mut f = KeyCaptureForm::new("ctrl-k ctrl-c");
        assert!(f.second);
        assert_eq!(f.focus(), Part::First);
        f.handle_key(ctrl('x'));
        assert_eq!(f.text(), "Ctrl+X Ctrl+C");
        assert_eq!(f.focus(), Part::Ok);
    }

    #[test]
    fn initial_text_fills_the_fields() {
        let f = KeyCaptureForm::new("g g");
        assert!(f.second);
        assert_eq!(f.text(), "g g");
        let mut f = KeyCaptureForm::new("Ctrl+Shift+F");
        assert!(screen(&mut f, 80, 24).iter().any(|r| r.contains("Ctrl+Shift+F")));
        let f = KeyCaptureForm::new("not a key");
        assert_eq!(f.text(), "not a key");
        assert_eq!(f.strokes, [None, None]);
        assert!(!f.second);
    }

    #[test]
    fn the_text_field_is_the_fallback() {
        let mut f = KeyCaptureForm::new("");
        f.handle_key(plain(KeyCode::Char('a')));
        back(&mut f, 1);
        assert_eq!(f.focus(), Part::Text);
        f.handle_key(plain(KeyCode::Backspace));
        for c in "f13".chars() {
            f.handle_key(plain(KeyCode::Char(c)));
        }
        assert_eq!(f.text(), "f13");
        assert!(!f.second);
        assert!(screen(&mut f, 80, 24).iter().any(|r| r.contains("F13")));
        assert!(matches!(f.handle_key(plain(KeyCode::Enter)), Some(DialogResult::Text(t)) if t == "f13"));
    }

    #[test]
    fn space_in_the_text_field_is_typed() {
        let mut f = KeyCaptureForm::new("");
        f.handle_key(plain(KeyCode::Char('a')));
        back(&mut f, 1);
        f.handle_key(plain(KeyCode::Char(' ')));
        assert_eq!(f.text(), "a ");
        assert!(!f.second);
    }

    #[test]
    fn paste_goes_to_the_text_field() {
        let mut f = KeyCaptureForm::new("");
        f.paste("ctrl-a\nctrl-b");
        assert_eq!(f.focus(), Part::Text);
        assert_eq!(f.text(), "ctrl-a ctrl-b");
        assert!(!f.capturing());
        assert!(f.second);
    }

    #[test]
    fn mouse_parts_and_buttons() {
        let mut f = KeyCaptureForm::new("");
        screen(&mut f, 80, 24);
        let (x, y) = center(f.toggle_rect);
        f.handle_mouse(&click(x, y));
        f.handle_mouse(&click(x, y));
        assert!(!f.second);
        assert_eq!(f.second_rect, Rect::default());
        let before = f.focus();
        f.handle_mouse(&click(f.first_rect.x, f.first_rect.y + 1));
        assert_eq!(f.focus(), before);
        let (x, y) = center(f.text_rect);
        assert!(f.handle_mouse(&click(x, y)).is_none());
        assert_eq!(f.focus(), Part::Text);
        let ok = f.button_rects[0];
        assert!(matches!(f.handle_mouse(&click(ok.x, ok.y)), Some(DialogResult::Text(_))));
        let cancel = f.button_rects[1];
        assert!(matches!(f.handle_mouse(&click(cancel.x, cancel.y)), Some(DialogResult::Cancel)));
        assert!(matches!(f.handle_mouse(&click(0, 0)), Some(DialogResult::Cancel)));
    }

    #[test]
    fn draws_the_parts() {
        let msg = "Run in editor. Keys as in ctrl-shift-f:";
        let mut f = KeyCaptureForm::new("");
        let rows = draw(&mut f, 80, 24, msg);
        assert!(rows.iter().any(|r| r.contains("Press a key")));
        f.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        let rows = draw(&mut f, 80, 24, msg);
        let has = |s: &str| rows.iter().any(|r| r.contains(s));
        for s in ["Add Key", msg, "First stroke", "[ ] Second", "Or type", "[ OK ]", "[ Cancel ]"] {
            assert!(has(s), "{s} missing in {rows:#?}");
        }
        assert_eq!(rows.iter().map(|r| r.matches("Ctrl+Alt+J").count()).sum::<usize>(), 2);
    }

    #[test]
    fn fits_tiny_screens() {
        let msg = "Run in editor. Keys as in ctrl-shift-f, written as the keymap file spells them, please:";
        let inside = |r: Rect, outer: Rect| {
            r.is_empty() || outer.contains(r.as_position()) && r.right() <= outer.right() && r.bottom() <= outer.bottom()
        };
        let mut bad = Vec::new();
        for w in 1..=60u16 {
            for h in 1..=14u16 {
                let area = Rect::new(0, 0, w, h);
                let mut f = KeyCaptureForm::new("ctrl-k ctrl-c");
                let theme = Theme::default();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                    let mut out = Rect::default();
                    term.draw(|fr| out = f.render(fr, area, &theme, "Add Key", msg)).unwrap();
                    out
                }));
                let Ok(rect) = r else {
                    bad.push(format!("{w}x{h}: panic"));
                    continue;
                };
                let parts = [f.first_rect, f.toggle_rect, f.second_rect, f.text_rect, f.button_rects[0], f.button_rects[1]];
                if !inside(rect, area) || !parts.into_iter().all(|p| inside(p, rect)) {
                    bad.push(format!("{w}x{h}: outside {rect:?}"));
                }
            }
        }
        assert!(bad.is_empty(), "{} bad sizes, first: {:?}", bad.len(), &bad[..bad.len().min(12)]);
    }
}
