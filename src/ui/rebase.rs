//! The Interactive Rebase dialog: a todo form over `git::rebase::RebasePlan`. Hosted as
//! `DialogKind::Rebase`; answers `DialogResult::Rebase` once `check_plan` passes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::git::rebase::{RebasePlan, TodoAction, TodoEntry, check_plan};
use crate::ui::dialog::DialogResult;
use crate::ui::new_task::edit;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;
use crate::util::text::fit;

const LIST_HINT: &str = "↑↓ select · Alt+↑↓ move · p/r/s/f/d · Tab msg · Enter start · Esc cancel";
const MESSAGE_HINT: &str = "Tab: list · Enter: new line (reword) · Alt+Enter: start · Esc: cancel";
const READ_ONLY: &str = "  read-only: r rewords it";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Message,
}

#[derive(Debug, Clone)]
pub struct RebaseForm {
    plan: RebasePlan,
    list: ListState,
    /// The selected row's message.
    input: TextInput,
    focus: Focus,
    error: Option<String>,
    list_rect: Rect,
    message_rect: Rect,
    button_rects: [Rect; 2],
}

pub(crate) fn row_text(e: &TodoEntry) -> String {
    format!("{:<6} {} {}", e.action.word(), &e.sha[..e.sha.len().min(8)], e.subject)
}

impl RebaseForm {
    pub fn new(plan: RebasePlan) -> Self {
        let mut list = ListState::default();
        list.set_len(plan.entries.len());
        let mut f = Self {
            plan,
            list,
            input: TextInput::new(true),
            focus: Focus::List,
            error: None,
            list_rect: Rect::default(),
            message_rect: Rect::default(),
            button_rects: [Rect::default(); 2],
        };
        f.load_message();
        f
    }

    pub fn entries(&self) -> &[TodoEntry] {
        &self.plan.entries
    }

    pub fn selected(&self) -> usize {
        self.list.selected
    }

    pub fn message_focused(&self) -> bool {
        self.focus == Focus::Message
    }

    fn is_reword(&self) -> bool {
        self.plan.entries.get(self.list.selected).is_some_and(|e| e.action == TodoAction::Reword)
    }

    /// Shows the selected row's message in the box, the cursor at the end.
    fn load_message(&mut self) {
        let text = match self.plan.entries.get(self.list.selected) {
            Some(e) if e.action == TodoAction::Reword => e.new_message.as_deref().unwrap_or(&e.message),
            Some(e) => &e.message,
            None => "",
        };
        self.input.set_text(text);
    }

    fn select(&mut self, f: impl FnOnce(&mut ListState)) {
        f(&mut self.list);
        self.load_message();
    }

    fn set_action(&mut self, action: TodoAction) {
        let Some(e) = self.plan.entries.get_mut(self.list.selected) else { return };
        e.action = action;
        if action == TodoAction::Reword && e.new_message.is_none() {
            e.new_message = Some(e.message.clone());
        }
        self.load_message();
        if action == TodoAction::Reword {
            self.focus = Focus::Message;
        }
    }

    fn move_row(&mut self, up: bool) {
        let i = self.list.selected;
        let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|&j| j < self.plan.entries.len()) };
        if let Some(j) = j {
            self.plan.entries.swap(i, j);
            self.list.select(j);
        }
    }

    /// Copies the box into the selected Reword row.
    fn store_message(&mut self) {
        if let Some(e) = self.plan.entries.get_mut(self.list.selected) {
            e.new_message = Some(self.input.text.clone());
        }
    }

    fn answer(&mut self) -> Option<DialogResult> {
        if let Err(e) = check_plan(&self.plan.entries) {
            self.error = Some(e);
            return None;
        }
        let mut plan = self.plan.clone();
        for e in &mut plan.entries {
            if e.action != TodoAction::Reword {
                e.new_message = None;
            }
        }
        Some(DialogResult::Rebase(plan))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<DialogResult> {
        if key.code != KeyCode::Esc {
            self.error = None;
        }
        let mods = key.modifiers;
        if key.code == KeyCode::Esc {
            return Some(DialogResult::Cancel);
        }
        if key.code == KeyCode::Enter && mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return self.answer();
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.focus = if self.focus == Focus::List { Focus::Message } else { Focus::List };
            return None;
        }
        match self.focus {
            Focus::List => self.list_key(key),
            Focus::Message => self.message_key(key),
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Option<DialogResult> {
        let mods = key.modifiers;
        match key.code {
            KeyCode::Enter => return self.answer(),
            KeyCode::Up if mods.contains(KeyModifiers::ALT) => self.move_row(true),
            KeyCode::Down if mods.contains(KeyModifiers::ALT) => self.move_row(false),
            KeyCode::Up => self.select(|l| l.up(1)),
            KeyCode::Down => self.select(|l| l.down(1)),
            KeyCode::Home => self.select(ListState::home),
            KeyCode::End => self.select(ListState::end),
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => match c {
                'p' => self.set_action(TodoAction::Pick),
                'r' => self.set_action(TodoAction::Reword),
                's' => self.set_action(TodoAction::Squash),
                'f' => self.set_action(TodoAction::Fixup),
                'd' => self.set_action(TodoAction::Drop),
                _ => {}
            },
            _ => {}
        }
        None
    }

    fn message_key(&mut self, key: KeyEvent) -> Option<DialogResult> {
        if !self.is_reword() {
            return None;
        }
        let changed = match key.code {
            KeyCode::Enter => {
                self.input.insert('\n');
                true
            }
            KeyCode::Up => {
                self.input.up();
                false
            }
            KeyCode::Down => {
                self.input.down();
                false
            }
            _ => edit(&mut self.input, key),
        };
        if changed {
            self.store_message();
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        if self.focus != Focus::Message || !self.is_reword() {
            return;
        }
        // A lone CR is a line break too: tmux and xterm paste newlines as CR.
        self.input.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        self.store_message();
        self.error = None;
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<DialogResult> {
        let p = Position::new(m.x, m.y);
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.list_rect.contains(p) => {
                self.list.scroll(if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.button_rects.iter().position(|r| r.contains(p)) {
                    return if i == 0 { self.answer() } else { Some(DialogResult::Cancel) };
                }
                if let Some(row) = self.list.hit(self.list_rect, m.x, m.y) {
                    self.focus = Focus::List;
                    self.select(|l| l.select(row));
                } else if self.message_rect.contains(p) {
                    self.focus = Focus::Message;
                    if self.is_reword() {
                        self.input.click(self.message_rect, m.x, m.y);
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// Draws the form centred in `area`; returns its outer rect.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, title: &str) -> Rect {
        let w = area.width.saturating_sub(2).min(76);
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
        self.list_rect = Rect::default();
        self.message_rect = Rect::default();
        self.button_rects = [Rect::default(); 2];
        if inner.height < 9 || inner.width < 30 {
            return rect;
        }
        let list_h = (self.plan.entries.len().max(1) as u16).min((inner.height - 5) / 2);
        self.list_rect = Rect::new(inner.x, inner.y, inner.width, list_h);
        self.list.fit(self.plan.entries.len(), list_h as usize);
        let text_w = inner.width as usize - 2;
        let focused_list = self.focus == Focus::List;
        for (i, e) in self.plan.entries.iter().enumerate().skip(self.list.offset).take(list_h as usize) {
            let row = inner.y + (i - self.list.offset) as u16;
            let st = if i == self.list.selected {
                if focused_list { theme.list_selected } else { theme.list_selected_unfocused }
            } else if e.action == TodoAction::Drop {
                base.patch(theme.dim)
            } else {
                base
            };
            frame.buffer_mut().set_style(Rect::new(inner.x, row, inner.width, 1), st);
            frame.buffer_mut().set_stringn(inner.x + 1, row, fit(&row_text(e), text_w), text_w, st);
        }
        let label_row = inner.y + list_h;
        let label = if focused_list { base } else { base.add_modifier(Modifier::BOLD) };
        frame.buffer_mut().set_stringn(inner.x + 1, label_row, "Message", text_w, label);
        let reword = self.is_reword();
        if !reword {
            let at = inner.x + 1 + "Message".width() as u16;
            let room = (inner.right() - 1).saturating_sub(at) as usize;
            frame.buffer_mut().set_stringn(at, label_row, READ_ONLY, room, base.patch(theme.dim));
        }
        self.message_rect = Rect::new(inner.x + 1, label_row + 1, inner.width - 2, inner.height - list_h - 3);
        let mut input_style = Style::default().bg(theme.bg).fg(theme.fg);
        if !reword {
            input_style = input_style.patch(theme.dim);
        }
        self.input.render(frame, self.message_rect, input_style, "", theme.dim, !focused_list && reword);
        let (msg, st) = match &self.error {
            Some(e) => (fit(e, text_w), base.patch(theme.error)),
            None => (fit(if focused_list { LIST_HINT } else { MESSAGE_HINT }, text_w), base.patch(theme.dim)),
        };
        frame.buffer_mut().set_stringn(inner.x + 1, inner.bottom() - 2, &msg, text_w, st);
        let by = inner.bottom() - 1;
        let labels = ["[ Start Rebasing ]", "[ Cancel ]"];
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

    #[cfg(test)]
    pub(crate) fn list_rect(&self) -> Rect {
        self.list_rect
    }

    #[cfg(test)]
    pub(crate) fn message_rect(&self) -> Rect {
        self.message_rect
    }

    #[cfg(test)]
    pub(crate) fn button_rects(&self) -> [Rect; 2] {
        self.button_rects
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

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn press(f: &mut RebaseForm, s: &str) {
        for c in s.chars() {
            f.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn entry(c: char, subject: &str, message: &str) -> TodoEntry {
        TodoEntry {
            sha: c.to_string().repeat(40),
            subject: subject.into(),
            message: message.into(),
            action: TodoAction::Pick,
            new_message: None,
        }
    }

    fn plan() -> RebasePlan {
        RebasePlan {
            base: Some("b".repeat(40)),
            head: "h".repeat(40),
            entries: vec![
                entry('a', "one", "one\n\nbody one"),
                entry('c', "two", "two\n\nbody two"),
                entry('d', "three", "three"),
                entry('e', "four", "four"),
            ],
        }
    }

    fn form() -> RebaseForm {
        RebaseForm::new(plan())
    }

    fn subjects(f: &RebaseForm) -> Vec<&str> {
        f.entries().iter().map(|e| e.subject.as_str()).collect()
    }

    fn actions(f: &RebaseForm) -> Vec<TodoAction> {
        f.entries().iter().map(|e| e.action).collect()
    }

    fn screen(f: &mut RebaseForm, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| {
            f.render(fr, Rect::new(0, 0, w, h), &theme, "Interactive Rebase from aaaaaaaa");
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    fn click(x: u16, y: u16) -> Mouse {
        Mouse { kind: MouseEventKind::Down(MouseButton::Left), x, y, clicks: 1, mods: KeyModifiers::NONE }
    }

    #[test]
    fn row_text_pads_the_action_and_shortens_the_sha() {
        let mut e = entry('a', "one", "one");
        assert_eq!(row_text(&e), "pick   aaaaaaaa one");
        e.action = TodoAction::Reword;
        assert_eq!(row_text(&e), "reword aaaaaaaa one");
        e.action = TodoAction::Drop;
        assert_eq!(row_text(&e), "drop   aaaaaaaa one");
    }

    #[test]
    fn alt_arrows_move_the_selected_row() {
        let mut f = form();
        f.handle_key(key(KeyCode::Down));
        f.handle_key(alt(KeyCode::Down));
        assert_eq!(subjects(&f), ["one", "three", "two", "four"]);
        assert_eq!(f.selected(), 2);
        f.handle_key(key(KeyCode::Home));
        f.handle_key(alt(KeyCode::Up));
        assert_eq!(subjects(&f), ["one", "three", "two", "four"]);
        assert_eq!(f.selected(), 0);
        f.handle_key(key(KeyCode::End));
        f.handle_key(alt(KeyCode::Down));
        assert_eq!(subjects(&f), ["one", "three", "two", "four"]);
        assert_eq!(f.selected(), 3);
    }

    #[test]
    fn letters_set_the_action() {
        use TodoAction::*;
        let mut f = form();
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "s");
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "f");
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "d");
        assert_eq!(actions(&f), [Pick, Squash, Fixup, Drop]);
        press(&mut f, "p");
        assert_eq!(actions(&f), [Pick, Squash, Fixup, Pick]);
        press(&mut f, "x");
        f.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(actions(&f), [Pick, Squash, Fixup, Pick]);
    }

    #[test]
    fn r_rewords_and_edits_the_message() {
        let mut f = form();
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "r");
        assert_eq!(f.entries()[1].action, TodoAction::Reword);
        assert_eq!(f.entries()[1].new_message.as_deref(), Some("two\n\nbody two"));
        assert!(f.message_focused());
        press(&mut f, "!");
        f.handle_key(key(KeyCode::Enter));
        press(&mut f, "x");
        assert_eq!(f.entries()[1].new_message.as_deref(), Some("two\n\nbody two!\nx"));
        f.handle_key(key(KeyCode::Tab));
        assert!(!f.message_focused());
        f.handle_key(key(KeyCode::Up));
        f.handle_key(key(KeyCode::Down));
        let rows = screen(&mut f, 80, 24);
        assert!(rows.iter().any(|r| r.contains("body two!")), "{rows:#?}");
    }

    #[test]
    fn a_read_only_message_ignores_typing() {
        let mut f = form();
        f.handle_key(key(KeyCode::Tab));
        assert!(f.message_focused());
        press(&mut f, "zz");
        assert!(f.handle_key(key(KeyCode::Enter)).is_none());
        f.handle_key(key(KeyCode::Backspace));
        assert_eq!(f.entries()[0], plan().entries[0]);
        assert_eq!(f.entries()[0].new_message, None);
    }

    #[test]
    fn check_errors_show_and_clear() {
        let mut f = form();
        press(&mut f, "s");
        assert!(f.handle_key(key(KeyCode::Enter)).is_none());
        let rows = screen(&mut f, 80, 24);
        assert!(rows.iter().any(|r| r.contains("The first commit cannot be squashed or fixed up")), "{rows:#?}");
        f.handle_key(key(KeyCode::Down));
        let rows = screen(&mut f, 80, 24);
        assert!(!rows.iter().any(|r| r.contains("cannot be squashed")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("p/r/s/f/d")), "{rows:#?}");

        let mut f = form();
        press(&mut f, "r");
        for _ in 0..16 {
            f.handle_key(key(KeyCode::Backspace));
        }
        assert!(f.handle_key(alt(KeyCode::Enter)).is_none());
        let rows = screen(&mut f, 80, 24);
        assert!(rows.iter().any(|r| r.contains("A reworded commit needs a message")), "{rows:#?}");
    }

    #[test]
    fn the_answer_carries_order_actions_and_messages() {
        use TodoAction::*;
        let mut f = form();
        press(&mut f, "r");
        press(&mut f, " again");
        f.handle_key(key(KeyCode::Tab));
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "s");
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "r");
        f.handle_key(key(KeyCode::Tab));
        press(&mut f, "p");
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "d");
        assert_eq!(actions(&f), [Reword, Squash, Pick, Drop]);
        let Some(DialogResult::Rebase(p)) = f.handle_key(key(KeyCode::Enter)) else { panic!("no answer") };
        assert_eq!(p.base, plan().base);
        assert_eq!(p.head, plan().head);
        assert_eq!(p.entries.iter().map(|e| e.action).collect::<Vec<_>>(), [Reword, Squash, Pick, Drop]);
        assert_eq!(p.entries[0].new_message.as_deref(), Some("one\n\nbody one again"));
        assert!(p.entries[1..].iter().all(|e| e.new_message.is_none()), "{:?}", p.entries);
    }

    #[test]
    fn paste_goes_into_a_reword_message_only() {
        let mut f = form();
        f.handle_key(key(KeyCode::Tab));
        f.paste("zz");
        assert_eq!(f.entries()[0].new_message, None);
        f.handle_key(key(KeyCode::Tab));
        f.handle_key(key(KeyCode::Down));
        press(&mut f, "r");
        f.paste("a\r\nb\rc");
        assert_eq!(f.entries()[1].new_message.as_deref(), Some("two\n\nbody twoa\nb\nc"));
    }

    #[test]
    fn mouse_selects_rows_and_presses_buttons() {
        let mut f = form();
        f.handle_key(key(KeyCode::Tab));
        let _ = screen(&mut f, 80, 24);
        let l = f.list_rect();
        assert!(f.handle_mouse(&click(l.x + 2, l.y + 2)).is_none());
        assert_eq!(f.selected(), 2);
        assert!(!f.message_focused());
        let m = f.message_rect();
        assert!(f.handle_mouse(&click(m.x, m.y)).is_none());
        assert!(f.message_focused());
        let [start, cancel] = f.button_rects();
        assert!(matches!(f.handle_mouse(&click(cancel.x, cancel.y)), Some(DialogResult::Cancel)));
        assert!(matches!(f.handle_mouse(&click(start.x, start.y)), Some(DialogResult::Rebase(_))));
        assert!(f.handle_mouse(&click(0, 0)).is_none());
    }

    #[test]
    fn hints_fit_the_form_text_width() {
        // The form is at most 76 wide; text width is the inner width minus 2.
        for hint in [LIST_HINT, MESSAGE_HINT] {
            assert!(hint.width() <= 72, "{hint:?} is {} cells", hint.width());
        }
    }

    #[test]
    fn small_screens_draw_an_empty_frame() {
        let mut f = form();
        for (w, h) in [(80, 10), (28, 24), (5, 3), (1, 1)] {
            let _ = screen(&mut f, w, h);
        }
        assert!(f.button_rects()[0].is_empty());
        assert!(f.list_rect().is_empty());
    }
}
