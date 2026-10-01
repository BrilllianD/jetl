//! Keyboard/mouse scrollable list state shared by the tree, git and worktree panels.

use ratatui::layout::Rect;

#[derive(Debug, Default, Clone)]
pub struct ListState {
    pub selected: usize,
    pub offset: usize,
    pub len: usize,
    pub viewport: usize,
}

impl ListState {
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        if len == 0 {
            self.selected = 0;
            self.offset = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
        self.clamp();
    }
    pub fn select(&mut self, i: usize) {
        if self.len == 0 {
            return;
        }
        self.selected = i.min(self.len - 1);
        self.clamp();
    }
    pub fn up(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
        self.clamp();
    }
    pub fn down(&mut self, n: usize) {
        if self.len > 0 {
            self.selected = (self.selected + n).min(self.len - 1);
        }
        self.clamp();
    }
    pub fn home(&mut self) {
        self.selected = 0;
        self.clamp();
    }
    pub fn end(&mut self) {
        self.selected = self.len.saturating_sub(1);
        self.clamp();
    }
    pub fn scroll(&mut self, delta: i32) {
        let max = self.len.saturating_sub(self.viewport);
        self.offset = (self.offset as i64 + delta as i64).clamp(0, max as i64) as usize;
    }
    /// Keep the selection inside the viewport.
    pub fn clamp(&mut self) {
        if self.viewport == 0 {
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + self.viewport {
            self.offset = self.selected + 1 - self.viewport;
        }
        let max = self.len.saturating_sub(self.viewport);
        if self.offset > max {
            self.offset = max;
        }
    }
    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows;
        self.clamp();
    }
    /// Row index under a screen position, if any.
    pub fn hit(&self, area: Rect, x: u16, y: u16) -> Option<usize> {
        if !area.contains((x, y).into()) {
            return None;
        }
        let i = self.offset + (y - area.y) as usize;
        (i < self.len).then_some(i)
    }
    pub fn visible(&self) -> std::ops::Range<usize> {
        let end = (self.offset + self.viewport).min(self.len);
        self.offset..end
    }
}
