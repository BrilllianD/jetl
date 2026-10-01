//! Modal dialogs: confirmation with buttons, text input, and a choice list.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

/// Identifies what the app should do with the dialog's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogId {
    QuitDirty,
    CloseTabDirty(usize),
    CloseAllDirty,
    SwitchRootDirty(std::path::PathBuf),
    WorktreeRemove { path: std::path::PathBuf, force: bool },
    WorktreeNewBranchName { start: Option<String> },
    WorktreeExistingBranch,
    WorktreePath { branch: String, new_branch: bool, start: Option<String> },
    WorktreeSwitchAfterCreate(std::path::PathBuf),
    NewBranchName,
    /// Branch popup; item 0 is "New branch", item `i` is `names[i - 1]`.
    Branches(Vec<String>),
    PushSetUpstream(String),
    Info,
    GotoLine,
}

#[derive(Debug, Clone)]
pub enum DialogKind {
    Confirm { buttons: Vec<String>, selected: usize },
    Input { input: TextInput, ok: String },
    Choice { items: Vec<String>, list: ListState, filter: TextInput },
}

#[derive(Debug, Clone)]
pub enum DialogResult {
    Button(usize),
    Text(String),
    Choice(usize),
    Cancel,
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub id: DialogId,
    pub title: String,
    pub message: String,
    pub kind: DialogKind,
    button_rects: Vec<Rect>,
    body_rect: Rect,
    list_rect: Rect,
}

impl Dialog {
    pub fn confirm(id: DialogId, title: &str, message: &str, buttons: &[&str]) -> Self {
        Self {
            id,
            title: title.into(),
            message: message.into(),
            kind: DialogKind::Confirm { buttons: buttons.iter().map(|s| s.to_string()).collect(), selected: 0 },
            button_rects: vec![],
            body_rect: Rect::default(),
            list_rect: Rect::default(),
        }
    }
    pub fn info(title: &str, message: &str) -> Self {
        Self::confirm(DialogId::Info, title, message, &["OK"])
    }
    pub fn input(id: DialogId, title: &str, message: &str, initial: &str, ok: &str) -> Self {
        Self {
            id,
            title: title.into(),
            message: message.into(),
            kind: DialogKind::Input { input: TextInput::new(false).with_text(initial), ok: ok.into() },
            button_rects: vec![],
            body_rect: Rect::default(),
            list_rect: Rect::default(),
        }
    }
    pub fn choice(id: DialogId, title: &str, message: &str, items: Vec<String>) -> Self {
        let mut list = ListState::default();
        list.set_len(items.len());
        Self {
            id,
            title: title.into(),
            message: message.into(),
            kind: DialogKind::Choice { items, list, filter: TextInput::new(false) },
            button_rects: vec![],
            body_rect: Rect::default(),
            list_rect: Rect::default(),
        }
    }

    fn filtered(&self) -> Vec<usize> {
        match &self.kind {
            DialogKind::Choice { items, filter, .. } => {
                let f = filter.text.to_lowercase();
                items.iter().enumerate().filter(|(_, s)| f.is_empty() || s.to_lowercase().contains(&f)).map(|(i, _)| i).collect()
            }
            _ => vec![],
        }
    }

    /// Returns Some when the dialog is finished.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<DialogResult> {
        match &mut self.kind {
            DialogKind::Confirm { buttons, selected } => match key.code {
                KeyCode::Esc => Some(DialogResult::Cancel),
                KeyCode::Enter => Some(DialogResult::Button(*selected)),
                KeyCode::Left | KeyCode::BackTab => {
                    *selected = (*selected + buttons.len() - 1) % buttons.len();
                    None
                }
                KeyCode::Right | KeyCode::Tab => {
                    *selected = (*selected + 1) % buttons.len();
                    None
                }
                KeyCode::Char(c) => {
                    // First-letter accelerators, e.g. Y/N/C.
                    let lc = c.to_ascii_lowercase();
                    buttons.iter().position(|b| b.chars().next().map(|f| f.to_ascii_lowercase()) == Some(lc)).map(DialogResult::Button)
                }
                _ => None,
            },
            DialogKind::Input { input, .. } => match key.code {
                KeyCode::Esc => Some(DialogResult::Cancel),
                KeyCode::Enter => Some(DialogResult::Text(input.text.clone())),
                KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.delete_word_back();
                    None
                }
                KeyCode::Backspace => {
                    input.backspace();
                    None
                }
                KeyCode::Delete => {
                    input.delete();
                    None
                }
                KeyCode::Left => {
                    input.left();
                    None
                }
                KeyCode::Right => {
                    input.right();
                    None
                }
                KeyCode::Home => {
                    input.home();
                    None
                }
                KeyCode::End => {
                    input.end();
                    None
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    input.insert(c);
                    None
                }
                _ => None,
            },
            DialogKind::Choice { .. } => {
                if key.code == KeyCode::Esc {
                    return Some(DialogResult::Cancel);
                }
                if key.code == KeyCode::Enter {
                    let f = self.filtered();
                    let DialogKind::Choice { list, .. } = &self.kind else { unreachable!() };
                    return f.get(list.selected).map(|i| DialogResult::Choice(*i));
                }
                let DialogKind::Choice { list, filter, .. } = &mut self.kind else { unreachable!() };
                match key.code {
                    KeyCode::Up => list.up(1),
                    KeyCode::Down => list.down(1),
                    KeyCode::PageUp => list.up(list.viewport.max(1)),
                    KeyCode::PageDown => list.down(list.viewport.max(1)),
                    KeyCode::Home => list.home(),
                    KeyCode::End => list.end(),
                    KeyCode::Backspace => {
                        filter.backspace();
                        list.select(0);
                    }
                    KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                        filter.insert(c);
                        list.select(0);
                    }
                    _ => {}
                }
                None
            }
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<DialogResult> {
        use crossterm::event::{MouseButton, MouseEventKind};
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let p = ratatui::layout::Position::new(m.x, m.y);
                if let Some(i) = self.button_rects.iter().position(|r| r.contains(p)) {
                    return match &self.kind {
                        DialogKind::Confirm { .. } => Some(DialogResult::Button(i)),
                        DialogKind::Input { input, .. } => {
                            if i == 0 { Some(DialogResult::Text(input.text.clone())) } else { Some(DialogResult::Cancel) }
                        }
                        DialogKind::Choice { .. } => Some(DialogResult::Cancel),
                    };
                }
                if let DialogKind::Choice { list, .. } = &mut self.kind {
                    if let Some(row) = list.hit(self.list_rect, m.x, m.y) {
                        let f = self.filtered();
                        if m.clicks >= 2 {
                            return f.get(row).map(|i| DialogResult::Choice(*i));
                        }
                        let DialogKind::Choice { list, .. } = &mut self.kind else { unreachable!() };
                        list.select(row);
                        return None;
                    }
                }
                if let DialogKind::Input { input, .. } = &mut self.kind {
                    input.click(self.body_rect, m.x, m.y);
                }
                if !self.body_rect.contains(p) && !self.outer_rect().contains(p) {
                    return Some(DialogResult::Cancel);
                }
                None
            }
            MouseEventKind::ScrollUp => {
                if let DialogKind::Choice { list, .. } = &mut self.kind {
                    list.scroll(-3);
                }
                None
            }
            MouseEventKind::ScrollDown => {
                if let DialogKind::Choice { list, .. } = &mut self.kind {
                    list.scroll(3);
                }
                None
            }
            _ => None,
        }
    }

    fn outer_rect(&self) -> Rect {
        self.body_rect
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let msg_lines: Vec<&str> = self.message.lines().collect();
        let msg_w = msg_lines.iter().map(|l| l.width()).max().unwrap_or(0);
        let (w, h) = match &self.kind {
            DialogKind::Confirm { buttons, .. } => {
                let bw: usize = buttons.iter().map(|b| b.width() + 4).sum::<usize>() + 2;
                ((msg_w.max(bw) + 4).max(30), msg_lines.len() + 4)
            }
            DialogKind::Input { .. } => ((msg_w + 4).max(50), msg_lines.len() + 5),
            DialogKind::Choice { .. } => ((msg_w + 4).max(50), (msg_lines.len() + 3 + 12).min(area.height as usize)),
        };
        let w = (w as u16).min(area.width.saturating_sub(2)).max(10);
        let h = (h as u16).min(area.height.saturating_sub(1)).max(5);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 3;
        let rect = Rect::new(x, y, w, h);
        self.body_rect = rect;
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.popup_border)
            .title(format!(" {} ", self.title))
            .title_style(theme.title_focused)
            .style(Style::default().bg(theme.popup_bg).fg(theme.fg));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let buf = frame.buffer_mut();
        let mut row = inner.y;
        for line in msg_lines.iter().take(inner.height as usize) {
            buf.set_stringn(inner.x + 1, row, line, inner.width.saturating_sub(2) as usize, Style::default().fg(theme.fg));
            row += 1;
        }
        self.button_rects.clear();
        match &mut self.kind {
            DialogKind::Confirm { buttons, selected } => {
                let by = inner.bottom().saturating_sub(1);
                let total: u16 = buttons.iter().map(|b| b.width() as u16 + 4).sum::<u16>() + (buttons.len() as u16).saturating_sub(1);
                let mut bx = inner.x + inner.width.saturating_sub(total) / 2;
                for (i, b) in buttons.iter().enumerate() {
                    let label = format!("[ {b} ]");
                    let bw = label.width() as u16;
                    let st = if i == *selected { theme.list_selected.add_modifier(Modifier::BOLD) } else { Style::default().fg(theme.fg).bg(theme.popup_bg) };
                    buf.set_stringn(bx, by, &label, bw as usize, st);
                    self.button_rects.push(Rect::new(bx, by, bw, 1));
                    bx += bw + 1;
                }
            }
            DialogKind::Input { input, ok } => {
                let iy = row + 1;
                let irect = Rect::new(inner.x + 1, iy, inner.width.saturating_sub(2), 1);
                self.body_rect = irect;
                let by = inner.bottom().saturating_sub(1);
                let labels = [format!("[ {ok} ]"), "[ Cancel ]".to_string()];
                let total: u16 = labels.iter().map(|l| l.width() as u16).sum::<u16>() + 1;
                let mut bx = inner.x + inner.width.saturating_sub(total) / 2;
                for l in &labels {
                    let bw = l.width() as u16;
                    buf.set_stringn(bx, by, l, bw as usize, Style::default().fg(theme.fg).bg(theme.popup_bg));
                    self.button_rects.push(Rect::new(bx, by, bw, 1));
                    bx += bw + 1;
                }
                input.render(frame, irect, Style::default().bg(theme.bg).fg(theme.fg), "", theme.dim, true);
                self.body_rect = rect;
            }
            DialogKind::Choice { items, list, filter } => {
                let fy = row;
                let frect = Rect::new(inner.x + 1, fy, inner.width.saturating_sub(2), 1);
                let lrect = Rect::new(inner.x + 1, fy + 1, inner.width.saturating_sub(2), inner.bottom().saturating_sub(fy + 1));
                self.list_rect = lrect;
                let f = filter.text.to_lowercase();
                let filtered: Vec<usize> = items.iter().enumerate().filter(|(_, s)| f.is_empty() || s.to_lowercase().contains(&f)).map(|(i, _)| i).collect();
                list.set_len(filtered.len());
                list.set_viewport(lrect.height as usize);
                for (vi, idx) in filtered.iter().enumerate().skip(list.offset).take(lrect.height as usize) {
                    let y = lrect.y + (vi - list.offset) as u16;
                    let st = if vi == list.selected { theme.list_selected } else { Style::default().fg(theme.fg).bg(theme.popup_bg) };
                    let buf = frame.buffer_mut();
                    buf.set_style(Rect::new(lrect.x, y, lrect.width, 1), st);
                    buf.set_stringn(lrect.x + 1, y, &items[*idx], lrect.width.saturating_sub(1) as usize, st);
                }
                filter.render(frame, frect, Style::default().bg(theme.bg).fg(theme.fg), "type to filter", theme.dim, true);
            }
        }
    }
}
