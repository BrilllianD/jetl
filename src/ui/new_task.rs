//! The New Task dialog of the Tasks window: a form over `yman add`. Hosted as
//! `DialogKind::NewTask`, so it gets the dialog's key, mouse and paste routing; answers
//! `DialogResult::NewTask` once `validate` passes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::dialog::DialogResult;
use crate::ui::widgets::text_input::TextInput;
use crate::util::text::fit;
use crate::yman::cli::NewTask;
use crate::yman::panel::parse_tags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Tags,
    Priority,
    Assignee,
    Related,
    Body,
}

impl Field {
    pub const ALL: [Field; 6] = [Field::Title, Field::Tags, Field::Priority, Field::Assignee, Field::Related, Field::Body];

    fn label(self) -> &'static str {
        match self {
            Field::Title => "Title",
            Field::Tags => "Tags",
            Field::Priority => "Priority",
            Field::Assignee => "Assignee",
            Field::Related => "Related",
            Field::Body => "Body",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Field::Title => "",
            Field::Tags => "space or comma separated",
            Field::Priority => "default",
            Field::Assignee => "nobody",
            Field::Related => "task ids, e.g. 12 40",
            Field::Body => "Markdown",
        }
    }

    fn next(self) -> Field {
        Field::ALL[(self as usize + 1) % Field::ALL.len()]
    }

    fn prev(self) -> Field {
        Field::ALL[(self as usize + Field::ALL.len() - 1) % Field::ALL.len()]
    }
}

const HINT: &str = "Tab: next field · Enter: create (in Body: Alt+Enter) · Esc: cancel";

#[derive(Debug, Clone)]
pub struct NewTaskForm {
    inputs: [TextInput; 6],
    focus: Field,
    /// Known tags, most used first.
    known: Vec<String>,
    error: Option<String>,
    field_rects: [Rect; 6],
    suggestion_rects: Vec<(Rect, String)>,
    button_rects: [Rect; 2],
}

/// The plain editing keys of a single input; whether the key was used.
fn edit(input: &mut TextInput, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => input.delete_word_back(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => input.insert(c),
        _ => return false,
    }
    true
}

impl NewTaskForm {
    pub fn new(known_tags: Vec<String>) -> Self {
        Self {
            inputs: std::array::from_fn(|i| TextInput::new(i == Field::Body as usize)),
            focus: Field::Title,
            known: known_tags,
            error: None,
            field_rects: [Rect::default(); 6],
            suggestion_rects: Vec::new(),
            button_rects: [Rect::default(); 2],
        }
    }

    pub fn set_known_tags(&mut self, tags: Vec<String>) {
        self.known = tags;
    }

    pub fn focus(&self) -> Field {
        self.focus
    }

    #[cfg(test)]
    pub fn input_mut(&mut self, f: Field) -> &mut TextInput {
        &mut self.inputs[f as usize]
    }

    /// Returns Some when the form is finished.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<DialogResult> {
        if key.code != KeyCode::Esc {
            self.error = None;
        }
        let mods = key.modifiers;
        let in_body = self.focus == Field::Body;
        match key.code {
            KeyCode::Esc => Some(DialogResult::Cancel),
            KeyCode::Enter if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) || !in_body => self.submit(),
            KeyCode::Enter => {
                self.inputs[Field::Body as usize].insert('\n');
                None
            }
            KeyCode::Tab => {
                if self.focus == Field::Tags && self.complete() {
                    return None;
                }
                self.focus = self.focus.next();
                None
            }
            KeyCode::BackTab => {
                self.focus = self.focus.prev();
                None
            }
            KeyCode::Down => {
                if in_body {
                    self.inputs[Field::Body as usize].down();
                } else {
                    self.focus = self.focus.next();
                }
                None
            }
            KeyCode::Up => {
                if in_body {
                    let body = &mut self.inputs[Field::Body as usize];
                    if body.line_col().0 > 0 {
                        body.up();
                    } else {
                        self.focus = Field::Related;
                    }
                } else if self.focus != Field::Title {
                    self.focus = self.focus.prev();
                }
                None
            }
            _ => {
                edit(&mut self.inputs[self.focus as usize], key);
                None
            }
        }
    }

    fn submit(&mut self) -> Option<DialogResult> {
        match self.validate() {
            Ok(t) => Some(DialogResult::NewTask(t)),
            Err((f, msg)) => {
                self.focus = f;
                self.error = Some(msg);
                None
            }
        }
    }

    fn tags_text(&self) -> &str {
        &self.inputs[Field::Tags as usize].text
    }

    /// The text after the last whitespace or comma of the Tags input.
    fn current_word(&self) -> &str {
        let t = self.tags_text();
        t.rsplit(|c: char| c.is_whitespace() || c == ',').next().unwrap_or("")
    }

    /// Known tags that are not entered yet and complete the word being typed; all unentered ones
    /// while no word is being typed.
    pub fn suggestions(&self) -> Vec<String> {
        let word = self.current_word();
        let head = &self.tags_text()[..self.tags_text().len() - word.len()];
        let typed: Vec<String> = parse_tags(head).iter().map(|t| t.to_lowercase()).collect();
        let w = word.to_lowercase();
        self.known
            .iter()
            .filter(|k| {
                let l = k.to_lowercase();
                !typed.contains(&l) && l.starts_with(&w) && l != w
            })
            .cloned()
            .collect()
    }

    /// Replaces the word being typed with the first suggestion; whether it did.
    fn complete(&mut self) -> bool {
        if self.current_word().is_empty() {
            return false;
        }
        match self.suggestions().into_iter().next() {
            Some(tag) => {
                self.apply_suggestion(&tag);
                true
            }
            None => false,
        }
    }

    /// The word being typed becomes `tag` and a space; with no word, `tag` is appended.
    fn apply_suggestion(&mut self, tag: &str) {
        let word_len = self.current_word().len();
        let input = &mut self.inputs[Field::Tags as usize];
        let keep = input.text.len() - word_len;
        input.text.truncate(keep);
        if word_len == 0 && !input.text.is_empty() && !input.text.ends_with(|c: char| c.is_whitespace() || c == ',') {
            input.text.push(' ');
        }
        input.text.push_str(tag);
        input.text.push(' ');
        input.cursor = input.text.chars().count();
    }

    pub fn validate(&self) -> Result<NewTask, (Field, String)> {
        let text = |f: Field| self.inputs[f as usize].text.as_str();
        let title = text(Field::Title).trim();
        if title.is_empty() {
            return Err((Field::Title, "Enter a title".into()));
        }
        let tags = parse_tags(text(Field::Tags));
        if let Some(c) = tags.iter().flat_map(|t| t.chars()).find(|c| c.is_control()) {
            return Err((Field::Tags, format!("Tags cannot contain {c:?}")));
        }
        let priority = match text(Field::Priority).trim() {
            "" => None,
            p => {
                let mut cs = p.chars();
                match (cs.next().and_then(|c| c.to_digit(10)), cs.next()) {
                    (Some(d), None) => Some(d as u8),
                    _ => return Err((Field::Priority, "Priority is a digit, 0 (highest) to 9".into())),
                }
            }
        };
        let assignee = Some(text(Field::Assignee).trim()).filter(|a| !a.is_empty()).map(str::to_string);
        let mut related: Vec<String> = Vec::new();
        for part in parse_tags(text(Field::Related)) {
            let id = part.strip_prefix('#').unwrap_or(&part);
            if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
                return Err((Field::Related, "Related ids are task numbers, e.g. 12 40".into()));
            }
            if !related.iter().any(|r| r == id) {
                related.push(id.to_string());
            }
        }
        Ok(NewTask { title: title.to_string(), tags, priority, assignee, related, body: text(Field::Body).to_string() })
    }

    pub fn paste(&mut self, text: &str) {
        let input = &mut self.inputs[self.focus as usize];
        if self.focus == Field::Body {
            // A lone CR is a line break too: tmux and xterm paste newlines as CR.
            input.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        } else {
            input.insert_str(&text.replace("\r\n", " ").replace(['\n', '\r'], " "));
        }
        self.error = None;
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<DialogResult> {
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        let p = Position::new(m.x, m.y);
        if let Some(i) = self.button_rects.iter().position(|r| r.contains(p)) {
            return if i == 0 { self.submit() } else { Some(DialogResult::Cancel) };
        }
        if let Some((_, tag)) = self.suggestion_rects.iter().find(|(r, _)| r.contains(p)).cloned() {
            self.apply_suggestion(&tag);
            self.focus = Field::Tags;
            self.error = None;
            return None;
        }
        if let Some(i) = self.field_rects.iter().position(|r| r.contains(p)) {
            self.focus = Field::ALL[i];
            self.inputs[i].click(self.field_rects[i], m.x, m.y);
        }
        None
    }

    /// Draws the form centred in `area`; returns its outer rect.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, title: &str) -> Rect {
        let w = area.width.saturating_sub(2).min(72);
        let h = area.height.saturating_sub(2).min(20);
        let x = area.x + (area.width - w) / 2;
        let y = area.y + (area.height - h) / 3;
        let rect = Rect::new(x, y, w, h);
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
        self.field_rects = [Rect::default(); 6];
        self.suggestion_rects.clear();
        self.button_rects = [Rect::default(); 2];
        if inner.height < 11 || inner.width < 24 {
            return rect;
        }
        let label_x = inner.x + 1;
        let input_x = inner.x + 11;
        let input_w = inner.width - 12;
        let input_style = Style::default().bg(theme.bg).fg(theme.fg);
        let rows: [u16; 6] = [0, 1, 3, 4, 5, 6];
        let body_h = inner.height - 8;
        for f in Field::ALL {
            let row = inner.y + rows[f as usize];
            let r = Rect::new(input_x, row, input_w, if f == Field::Body { body_h } else { 1 });
            self.field_rects[f as usize] = r;
            let label_style = if f == self.focus { base.add_modifier(Modifier::BOLD) } else { base };
            frame.buffer_mut().set_stringn(label_x, row, f.label(), 9, label_style);
            let focused = f == self.focus;
            self.inputs[f as usize].render(frame, r, input_style, f.placeholder(), theme.dim, focused);
        }
        if self.focus == Field::Tags {
            let row = inner.y + 2;
            let end = input_x + input_w;
            let prefix = "Tab: ";
            let mut cx = input_x;
            frame.buffer_mut().set_stringn(cx, row, prefix, (end - cx) as usize, base.patch(theme.dim));
            cx += prefix.width() as u16;
            for (i, tag) in self.suggestions().into_iter().enumerate() {
                if cx >= end {
                    break;
                }
                let tw = (tag.width() as u16).min(end - cx);
                let st = if i == 0 { theme.list_selected } else { base.patch(theme.dim) };
                frame.buffer_mut().set_stringn(cx, row, &tag, tw as usize, st);
                self.suggestion_rects.push((Rect::new(cx, row, tw, 1), tag));
                cx += tw + 2;
            }
        }
        let msg_row = inner.bottom() - 2;
        let msg_w = inner.width.saturating_sub(2) as usize;
        let (msg, st) = match &self.error {
            Some(e) => (fit(e, msg_w), base.patch(theme.error)),
            None => (fit(HINT, msg_w), base.patch(theme.dim)),
        };
        frame.buffer_mut().set_stringn(label_x, msg_row, &msg, msg_w, st);
        let by = inner.bottom() - 1;
        let labels = ["[ Create ]", "[ Cancel ]"];
        let total: u16 = labels.iter().map(|l| l.width() as u16).sum::<u16>() + 1;
        let mut bx = inner.x + inner.width.saturating_sub(total) / 2;
        for (i, l) in labels.iter().enumerate() {
            let bw = l.width() as u16;
            let r = Rect::new(bx, by, bw, 1).intersection(inner);
            frame.buffer_mut().set_stringn(r.x, by, l, r.width as usize, base);
            self.button_rects[i] = r;
            bx += bw + 1;
        }
        rect
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(f: &mut NewTaskForm, s: &str) {
        for c in s.chars() {
            f.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn form() -> NewTaskForm {
        NewTaskForm::new(vec!["ui".into(), "bug".into(), "build".into()])
    }

    #[test]
    fn validate_rules() {
        let mut f = form();
        assert_eq!(f.validate().unwrap_err(), (Field::Title, "Enter a title".to_string()));
        f.input_mut(Field::Title).set_text(" T ");
        for bad in ["12", "x"] {
            f.input_mut(Field::Priority).set_text(bad);
            assert_eq!(f.validate().unwrap_err().0, Field::Priority, "{bad}");
        }
        f.input_mut(Field::Priority).set_text("");
        f.input_mut(Field::Related).set_text("#4, 4 7");
        assert_eq!(f.validate().unwrap().related, ["4", "7"]);
        f.input_mut(Field::Related).set_text("4a");
        assert_eq!(f.validate().unwrap_err().0, Field::Related);
        f.input_mut(Field::Related).set_text("");
        f.input_mut(Field::Tags).set_text("UI, bug");
        let t = f.validate().unwrap();
        assert_eq!(t.tags, ["UI", "bug"]);
        assert_eq!(t.title, "T");
        assert_eq!(t.priority, None);
        f.input_mut(Field::Tags).set_text("a\u{1}b");
        assert_eq!(f.validate().unwrap_err(), (Field::Tags, "Tags cannot contain '\\u{1}'".to_string()));
        f.input_mut(Field::Tags).set_text("");
        f.input_mut(Field::Priority).set_text("0");
        assert_eq!(f.validate().unwrap().priority, Some(0));
    }

    #[test]
    fn tab_completes_tags_then_moves_on() {
        let mut f = form();
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.focus(), Field::Tags);
        type_str(&mut f, "b");
        assert_eq!(f.suggestions(), ["bug", "build"]);
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.input_mut(Field::Tags).text, "bug ");
        assert_eq!(f.focus(), Field::Tags);
        assert_eq!(f.suggestions(), ["ui", "build"]);
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.focus(), Field::Priority);
    }

    #[test]
    fn enter_in_body_adds_a_line_and_alt_enter_submits() {
        let mut f = form();
        type_str(&mut f, "t");
        for _ in 0..5 {
            f.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(f.focus(), Field::Body);
        type_str(&mut f, "a");
        assert!(f.handle_key(key(KeyCode::Enter)).is_none());
        type_str(&mut f, "b");
        match f.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)) {
            Some(DialogResult::NewTask(t)) => assert_eq!(t.body, "a\nb"),
            other => panic!("{other:?}"),
        }
        f.handle_key(key(KeyCode::Up));
        f.handle_key(key(KeyCode::Up));
        assert_eq!(f.focus(), Field::Related);
    }

    fn screen(f: &mut NewTaskForm, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| {
            f.render(fr, Rect::new(0, 0, w, h), &theme, "New Task");
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    #[test]
    fn invalid_submit_keeps_the_form() {
        let mut f = form();
        assert!(f.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(f.focus(), Field::Title);
        let rows = screen(&mut f, 80, 24);
        assert!(rows.iter().any(|r| r.contains("Enter a title")), "{rows:#?}");
        // Any key clears the message.
        type_str(&mut f, "x");
        let rows = screen(&mut f, 80, 24);
        assert!(!rows.iter().any(|r| r.contains("Enter a title")));
        assert!(rows.iter().any(|r| r.contains("[ Create ]") && r.contains("[ Cancel ]")));
    }

    #[test]
    fn paste_keeps_newlines_in_body_only() {
        let mut f = form();
        f.paste("a\r\nb\nc");
        assert_eq!(f.input_mut(Field::Title).text, "a b c");
        for _ in 0..5 {
            f.handle_key(key(KeyCode::Tab));
        }
        f.paste("l1\r\nl2\rl3\n");
        assert_eq!(f.input_mut(Field::Body).text, "l1\nl2\nl3\n");
    }

    #[test]
    fn mouse_clicks_buttons_suggestions_and_fields() {
        let mut f = form();
        f.handle_key(key(KeyCode::Tab));
        let _ = screen(&mut f, 80, 24);
        let click = |x: u16, y: u16| Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks: 1, mods: KeyModifiers::NONE };
        let (r, tag) = f.suggestion_rects[0].clone();
        assert_eq!(tag, "ui");
        assert!(f.handle_mouse(&click(r.x, r.y)).is_none());
        assert_eq!(f.input_mut(Field::Tags).text, "ui ");
        let a = f.field_rects[Field::Assignee as usize];
        f.handle_mouse(&click(a.x, a.y));
        assert_eq!(f.focus(), Field::Assignee);
        // A click outside does nothing; Cancel cancels.
        assert!(f.handle_mouse(&click(0, 0)).is_none());
        let c = f.button_rects[1];
        assert!(matches!(f.handle_mouse(&click(c.x, c.y)), Some(DialogResult::Cancel)));
        let c = f.button_rects[0];
        assert!(f.handle_mouse(&click(c.x, c.y)).is_none(), "empty title");
        assert_eq!(f.focus(), Field::Title);
    }

    #[test]
    fn small_screens_draw_an_empty_frame() {
        let mut f = form();
        for (w, h) in [(80, 12), (20, 24), (5, 3), (1, 1)] {
            let _ = screen(&mut f, w, h);
        }
        assert!(f.button_rects[0].is_empty());
    }
}
