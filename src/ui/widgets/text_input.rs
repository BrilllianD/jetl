//! Single and multi-line text input used by dialogs, the palette and the
//! commit message box.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    /// Cursor as a char index into `text`.
    pub cursor: usize,
    pub multiline: bool,
    pub scroll: usize,
}

impl TextInput {
    pub fn new(multiline: bool) -> Self {
        Self { multiline, ..Default::default() }
    }
    pub fn with_text(mut self, s: &str) -> Self {
        self.text = s.to_string();
        self.cursor = self.text.chars().count();
        self
    }
    pub fn set_text(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.chars().count();
    }
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
    fn byte_at(&self, ci: usize) -> usize {
        self.text.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(self.text.len())
    }
    pub fn insert(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }
    pub fn insert_str(&mut self, s: &str) {
        let b = self.byte_at(self.cursor);
        self.text.insert_str(b, s);
        self.cursor += s.chars().count();
    }
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let b = self.byte_at(self.cursor - 1);
        self.text.remove(b);
        self.cursor -= 1;
    }
    pub fn delete(&mut self) {
        if self.cursor >= self.text.chars().count() {
            return;
        }
        let b = self.byte_at(self.cursor);
        self.text.remove(b);
    }
    pub fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = self.byte_at(i);
        let end = self.byte_at(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor = i;
    }
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }
    pub fn home(&mut self) {
        if self.multiline {
            let (line, _) = self.line_col();
            self.cursor = self.line_start(line);
        } else {
            self.cursor = 0;
        }
    }
    pub fn end(&mut self) {
        if self.multiline {
            let (line, _) = self.line_col();
            let lines: Vec<&str> = self.text.split('\n').collect();
            self.cursor = self.line_start(line) + lines[line].chars().count();
        } else {
            self.cursor = self.text.chars().count();
        }
    }
    pub fn up(&mut self) {
        let (line, col) = self.line_col();
        if line == 0 {
            self.cursor = 0;
            return;
        }
        let lines: Vec<&str> = self.text.split('\n').collect();
        let len = lines[line - 1].chars().count();
        self.cursor = self.line_start(line - 1) + col.min(len);
    }
    pub fn down(&mut self) {
        let (line, col) = self.line_col();
        let lines: Vec<&str> = self.text.split('\n').collect();
        if line + 1 >= lines.len() {
            self.cursor = self.text.chars().count();
            return;
        }
        let len = lines[line + 1].chars().count();
        self.cursor = self.line_start(line + 1) + col.min(len);
    }
    fn line_start(&self, line: usize) -> usize {
        self.text.split('\n').take(line).map(|l| l.chars().count() + 1).sum()
    }
    pub fn line_col(&self) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        for (i, c) in self.text.chars().enumerate() {
            if i == self.cursor {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Set the cursor from a click at screen position within `area`.
    pub fn click(&mut self, area: Rect, x: u16, y: u16) {
        if !area.contains((x, y).into()) {
            return;
        }
        let row = (y - area.y) as usize + self.scroll;
        let col = (x - area.x) as usize;
        let lines: Vec<&str> = self.text.split('\n').collect();
        let row = row.min(lines.len().saturating_sub(1));
        let mut w = 0;
        let mut ci = 0;
        for c in lines[row].chars() {
            let cw = c.width().unwrap_or(1);
            if w + cw > col {
                break;
            }
            w += cw;
            ci += 1;
        }
        self.cursor = self.line_start(row) + ci;
    }

    /// Render and, when `focused`, position the terminal cursor.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, style: Style, placeholder: &str, placeholder_style: Style, focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let buf = frame.buffer_mut();
        buf.set_style(area, style);
        let (cline, ccol) = self.line_col();
        // Keep the cursor row visible.
        if cline < self.scroll {
            self.scroll = cline;
        } else if cline >= self.scroll + area.height as usize {
            self.scroll = cline + 1 - area.height as usize;
        }
        if self.text.is_empty() {
            buf.set_stringn(area.x, area.y, placeholder, area.width as usize, placeholder_style);
        } else {
            let lines: Vec<&str> = self.text.split('\n').collect();
            for (i, line) in lines.iter().skip(self.scroll).take(area.height as usize).enumerate() {
                buf.set_stringn(area.x, area.y + i as u16, line, area.width as usize, style);
            }
        }
        if focused {
            let lines: Vec<&str> = self.text.split('\n').collect();
            let cw: usize = lines.get(cline).map(|l| l.chars().take(ccol).map(|c| c.width().unwrap_or(1)).sum()).unwrap_or(0);
            let cx = area.x + (cw as u16).min(area.width.saturating_sub(1));
            let cy = area.y + (cline - self.scroll) as u16;
            frame.set_cursor_position((cx, cy));
        }
    }
}
