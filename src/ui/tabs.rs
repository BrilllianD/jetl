//! Editor tab bar hit areas. Rendering lives in the editor so it can read
//! buffer state directly.

use ratatui::layout::Rect;

#[derive(Debug, Clone, Default)]
pub struct TabHits {
    pub tabs: Vec<(usize, Rect)>,
    pub close: Vec<(usize, Rect)>,
}

impl TabHits {
    pub fn clear(&mut self) {
        self.tabs.clear();
        self.close.clear();
    }
    pub fn hit(&self, x: u16, y: u16) -> Option<TabHit> {
        let p = ratatui::layout::Position::new(x, y);
        if let Some((i, _)) = self.close.iter().find(|(_, r)| r.contains(p)) {
            return Some(TabHit::Close(*i));
        }
        if let Some((i, _)) = self.tabs.iter().find(|(_, r)| r.contains(p)) {
            return Some(TabHit::Tab(*i));
        }
        None
    }
    /// The tab under column `x`, whatever the row (a tab drag may stray
    /// off the bar).
    pub fn tab_at_x(&self, x: u16) -> Option<usize> {
        self.tabs.iter().find(|(_, r)| r.x <= x && x < r.right()).map(|(i, _)| *i)
    }
    pub fn rect_of(&self, i: usize) -> Option<Rect> {
        self.tabs.iter().find(|(j, _)| *j == i).map(|(_, r)| *r)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabHit {
    Tab(usize),
    Close(usize),
}
