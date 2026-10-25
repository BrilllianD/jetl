//! The Switcher popup (Ctrl+Tab): two columns, tool windows on the left and
//! the recent files on the right. Tab and the arrows move the selection,
//! releasing Ctrl (or Enter, or a click) opens it. It is plain data: the app
//! builds the rows (`app/switcher.rs`) and carries out the result.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::keymap::KeyCombo;
use crate::config::theme::Theme;
use crate::editor::buffer::BufferId;
use crate::event::Mouse;
use crate::ui::focus::ToolWindowId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    File(BufferId),
    Tool(ToolWindowId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitcherResult {
    Cancel,
    Open(Pick),
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: BufferId,
    pub name: String,
    /// Shown on the bottom row while selected: the path.
    pub detail: String,
    pub dirty: bool,
}

#[derive(Debug, Clone)]
pub struct ToolRow {
    pub id: ToolWindowId,
    pub title: &'static str,
    pub shortcut: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Tools,
    Files,
}

impl Column {
    fn index(self) -> usize {
        match self {
            Column::Tools => 0,
            Column::Files => 1,
        }
    }
}

pub struct Switcher {
    files: Vec<FileRow>,
    tools: Vec<ToolRow>,
    column: Column,
    /// Selected row per column (Tools, Files); the other column keeps its row.
    sel: [usize; 2],
    scroll: [usize; 2],
    /// Ctrl is down, so releasing it opens the selection.
    hold: bool,
    rect: Rect,
    hits: Vec<(Rect, Pick)>,
}

impl Switcher {
    /// `back`: start on the last file instead of the second; `hold`: Ctrl is down.
    pub fn new(files: Vec<FileRow>, tools: Vec<ToolRow>, back: bool, hold: bool) -> Self {
        let column = if files.is_empty() { Column::Tools } else { Column::Files };
        let file_sel = if back {
            files.len().saturating_sub(1)
        } else if files.len() > 1 {
            1
        } else {
            0
        };
        Self { files, tools, column, sel: [0, file_sel], scroll: [0, 0], hold, rect: Rect::default(), hits: Vec::new() }
    }

    pub fn selected(&self) -> Option<Pick> {
        match self.column {
            Column::Tools => self.tools.get(self.sel[0]).map(|t| Pick::Tool(t.id)),
            Column::Files => self.files.get(self.sel[1]).map(|f| Pick::File(f.id)),
        }
    }

    pub fn column(&self) -> Column {
        self.column
    }

    fn len(&self, column: Column) -> usize {
        match column {
            Column::Tools => self.tools.len(),
            Column::Files => self.files.len(),
        }
    }

    fn step(&mut self, forward: bool) {
        let n = self.len(self.column);
        if n == 0 {
            return;
        }
        let s = &mut self.sel[self.column.index()];
        *s = if forward { (*s + 1) % n } else { (*s + n - 1) % n };
    }

    fn switch_to(&mut self, column: Column) {
        if self.len(column) > 0 {
            self.column = column;
        }
    }

    fn open_selected(&self) -> SwitcherResult {
        self.selected().map(SwitcherResult::Open).unwrap_or(SwitcherResult::Cancel)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<SwitcherResult> {
        if key.kind == KeyEventKind::Release {
            let ctrl = matches!(key.code, KeyCode::Modifier(ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl));
            return (self.hold && ctrl).then(|| self.open_selected());
        }
        if matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        self.hold |= key.modifiers.contains(KeyModifiers::CONTROL);
        let combo = KeyCombo::normalize(key.code, key.modifiers);
        let shift = combo.mods.contains(KeyModifiers::SHIFT);
        match combo.code {
            KeyCode::Tab if !shift => self.step(true),
            KeyCode::Tab => self.step(false),
            KeyCode::Down => self.step(true),
            KeyCode::Up => self.step(false),
            KeyCode::Left => self.switch_to(Column::Tools),
            KeyCode::Right => self.switch_to(Column::Files),
            KeyCode::Enter => return Some(self.open_selected()),
            KeyCode::Esc => return Some(SwitcherResult::Cancel),
            _ => {}
        }
        None
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<SwitcherResult> {
        match m.kind {
            MouseEventKind::Down(_) if !self.contains(m.x, m.y) => Some(SwitcherResult::Cancel),
            MouseEventKind::Down(MouseButton::Left) => {
                let pick = self.hits.iter().find(|(r, _)| r.contains((m.x, m.y).into())).map(|&(_, p)| p)?;
                match pick {
                    Pick::File(id) => {
                        self.column = Column::Files;
                        self.sel[1] = self.files.iter().position(|f| f.id == id).unwrap_or(self.sel[1]);
                    }
                    Pick::Tool(id) => {
                        self.column = Column::Tools;
                        self.sel[0] = self.tools.iter().position(|t| t.id == id).unwrap_or(self.sel[0]);
                    }
                }
                Some(SwitcherResult::Open(pick))
            }
            MouseEventKind::ScrollDown if self.contains(m.x, m.y) => {
                self.step(true);
                None
            }
            MouseEventKind::ScrollUp if self.contains(m.x, m.y) => {
                self.step(false);
                None
            }
            _ => None,
        }
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains((x, y).into())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.hits.clear();
        let tools_w = self.tools.iter().map(|t| t.title.width() + t.shortcut.width() + 4).max().unwrap_or(0).max(16);
        let files_w = self.files.iter().map(|f| f.name.width() + 4 + if f.dirty { 2 } else { 0 }).max().unwrap_or(0).max(24);
        let w = (tools_w + 1 + files_w + 2).min(area.width.saturating_sub(4) as usize).min(100) as u16;
        let rows = self.tools.len().max(self.files.len()).max(1);
        let h = (4 + rows).min(area.height.saturating_sub(2) as usize) as u16;
        if w < 5 || h < 5 {
            self.rect = Rect::default();
            return;
        }
        let x = area.x + (area.width - w) / 2;
        let y = area.y + (area.height - h) / 2;
        self.rect = Rect::new(x, y, w, h);
        frame.render_widget(Clear, self.rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.popup_border)
            .title(" Switcher ")
            .style(Style::default().bg(theme.popup_bg).fg(theme.fg));
        let inner = block.inner(self.rect);
        frame.render_widget(block, self.rect);

        let iw = inner.width as usize;
        let tw = if tools_w + 1 + files_w <= iw { tools_w } else { tools_w.min(iw / 2) };
        let tw = tw.min(iw.saturating_sub(1)) as u16;
        let fw = inner.width.saturating_sub(tw + 1);
        let fx = inner.x + tw + 1;
        let list_rows = inner.height.saturating_sub(2) as usize;
        let bg = Style::default().fg(theme.fg).bg(theme.popup_bg);
        let dim = theme.dim.bg(theme.popup_bg);
        let line = theme.popup_border.bg(theme.popup_bg);

        for (c, n) in [self.tools.len(), self.files.len()].into_iter().enumerate() {
            let s = &mut self.scroll[c];
            if list_rows > 0 {
                if self.sel[c] < *s {
                    *s = self.sel[c];
                } else if self.sel[c] >= *s + list_rows {
                    *s = self.sel[c] + 1 - list_rows;
                }
            }
            *s = (*s).min(n.saturating_sub(list_rows));
        }

        let buf = frame.buffer_mut();
        buf.set_stringn(inner.x + 1, inner.y, "Tool Windows", tw.saturating_sub(1) as usize, dim);
        buf.set_stringn(fx + 1, inner.y, "Recent Files", fw.saturating_sub(1) as usize, dim);
        for dy in 0..inner.height.saturating_sub(1) {
            buf.set_string(inner.x + tw, inner.y + dy, "│", line);
        }
        let style_of = |column: Column, i: usize, sel: &[usize; 2], cur: Column| {
            if column == cur && i == sel[column.index()] { theme.list_selected } else { bg }
        };
        for (i, t) in self.tools.iter().enumerate().skip(self.scroll[0]).take(list_rows) {
            let ry = inner.y + 1 + (i - self.scroll[0]) as u16;
            let st = style_of(Column::Tools, i, &self.sel, self.column);
            let r = Rect::new(inner.x, ry, tw, 1);
            buf.set_style(r, st);
            buf.set_stringn(inner.x + 1, ry, t.title, tw.saturating_sub(1) as usize, st);
            if !t.shortcut.is_empty() {
                let sw = t.shortcut.width() as u16;
                let kx = (inner.x + tw).saturating_sub(sw + 1).max(inner.x + 1 + t.title.width() as u16 + 1);
                if kx < inner.x + tw {
                    buf.set_stringn(kx, ry, &t.shortcut, (inner.x + tw - kx) as usize, st.patch(theme.dim));
                }
            }
            self.hits.push((r, Pick::Tool(t.id)));
        }
        for (i, f) in self.files.iter().enumerate().skip(self.scroll[1]).take(list_rows) {
            let ry = inner.y + 1 + (i - self.scroll[1]) as u16;
            let st = style_of(Column::Files, i, &self.sel, self.column);
            let r = Rect::new(fx, ry, fw, 1);
            buf.set_style(r, st);
            let label = if f.dirty { format!("{} *", f.name) } else { f.name.clone() };
            buf.set_stringn(fx + 1, ry, label, fw.saturating_sub(1) as usize, st);
            self.hits.push((r, Pick::File(f.id)));
        }
        let detail = match self.selected() {
            Some(Pick::File(id)) => self.files.iter().find(|f| f.id == id).map(|f| f.detail.as_str()),
            Some(Pick::Tool(id)) => self.tools.iter().find(|t| t.id == id).map(|t| t.title),
            None => None,
        };
        if let Some(d) = detail {
            buf.set_stringn(inner.x + 1, inner.bottom() - 1, d, iw.saturating_sub(2), dim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn files(n: u64) -> Vec<FileRow> {
        (1..=n).map(|id| FileRow { id, name: format!("f{id}.rs"), detail: format!("src/f{id}.rs"), dirty: id == 2 }).collect()
    }

    fn tools() -> Vec<ToolRow> {
        [ToolWindowId::Project, ToolWindowId::Find, ToolWindowId::Git]
            .into_iter()
            .map(|id| ToolRow { id, title: id.title(), shortcut: "Alt+1".into() })
            .collect()
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn release(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release)
    }

    const CTRL_L: KeyCode = KeyCode::Modifier(ModifierKeyCode::LeftControl);

    #[test]
    fn starts_on_the_previous_file() {
        assert_eq!(Switcher::new(files(3), tools(), false, true).selected(), Some(Pick::File(2)));
        assert_eq!(Switcher::new(files(3), tools(), true, true).selected(), Some(Pick::File(3)));
        assert_eq!(Switcher::new(files(1), tools(), false, true).selected(), Some(Pick::File(1)));
        let s = Switcher::new(Vec::new(), tools(), false, true);
        assert_eq!(s.column(), Column::Tools);
        assert_eq!(s.selected(), Some(Pick::Tool(ToolWindowId::Project)));
    }

    #[test]
    fn tab_and_arrows_cycle_and_wrap() {
        let mut s = Switcher::new(files(3), tools(), false, true);
        s.handle_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.selected(), Some(Pick::File(3)));
        s.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(s.selected(), Some(Pick::File(1)), "wraps forward");
        s.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(s.selected(), Some(Pick::File(3)), "wraps backward");
        s.handle_key(key(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(s.selected(), Some(Pick::File(2)));
        s.handle_key(key(KeyCode::Tab, KeyModifiers::CONTROL));
        assert_eq!(s.selected(), Some(Pick::File(3)));
        s.handle_key(key(KeyCode::BackTab, KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert_eq!(s.selected(), Some(Pick::File(2)));
    }

    #[test]
    fn left_right_switch_columns() {
        let mut s = Switcher::new(files(3), tools(), false, true);
        s.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(s.selected(), Some(Pick::Tool(ToolWindowId::Project)));
        s.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(s.selected(), Some(Pick::File(2)), "the files row is kept");
        let mut s = Switcher::new(Vec::new(), tools(), false, true);
        s.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(s.column(), Column::Tools);
    }

    #[test]
    fn ctrl_release_opens_only_when_held() {
        let mut s = Switcher::new(files(3), tools(), false, false);
        assert_eq!(s.handle_key(release(CTRL_L)), None, "opened without Ctrl");
        assert_eq!(s.handle_key(key(CTRL_L, KeyModifiers::CONTROL)), None, "a bare Ctrl press does not arm it");
        assert_eq!(s.handle_key(release(CTRL_L)), None);
        s.handle_key(key(KeyCode::Tab, KeyModifiers::CONTROL));
        assert_eq!(s.handle_key(release(KeyCode::Tab)), None);
        assert_eq!(s.handle_key(release(CTRL_L)), Some(SwitcherResult::Open(Pick::File(3))));
    }

    #[test]
    fn enter_opens_esc_cancels_other_keys_swallowed() {
        let mut s = Switcher::new(files(3), tools(), false, false);
        assert_eq!(s.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)), None);
        assert_eq!(s.selected(), Some(Pick::File(2)));
        assert_eq!(s.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)), Some(SwitcherResult::Open(Pick::File(2))));
        assert_eq!(s.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)), Some(SwitcherResult::Cancel));
    }

    #[test]
    fn mouse_click_opens_row_press_outside_cancels() {
        let mut s = Switcher::new(files(3), tools(), false, false);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let theme = Theme::default();
        term.draw(|f| s.render(f, f.area(), &theme)).unwrap();
        let at = |kind, x, y| Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE };
        assert!(s.rect().width > 0 && s.contains(s.rect().x + 1, s.rect().y + 1));
        let (r, pick) = *s.hits.iter().find(|(_, p)| *p == Pick::File(3)).unwrap();
        assert_eq!(s.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), r.x + 1, r.y)), Some(SwitcherResult::Open(pick)));
        let (r, pick) = *s.hits.iter().find(|(_, p)| *p == Pick::Tool(ToolWindowId::Find)).unwrap();
        assert_eq!(s.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), r.x, r.y)), Some(SwitcherResult::Open(pick)));
        assert_eq!(s.handle_mouse(&at(MouseEventKind::ScrollDown, r.x, r.y)), None);
        assert_eq!(s.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 0, 0)), Some(SwitcherResult::Cancel));
    }
}
