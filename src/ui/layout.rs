//! Fixed JetBrains-style layout: tool strip on the left, optional left tool
//! window, editor (tabs + text) in the centre, optional bottom tool window
//! spanning the full width, status bar at the bottom.

use ratatui::layout::Rect;

use crate::ui::focus::{PanelId, Side, ToolWindowId};

pub const STRIP_W: u16 = 3;
pub const MIN_LEFT_W: u16 = 12;
pub const MIN_BOTTOM_H: u16 = 4;
pub const MIN_CENTER_W: u16 = 30;
pub const MIN_CENTER_H: u16 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitterId {
    Left,
    Bottom,
}

#[derive(Debug, Clone, Default)]
pub struct Rects {
    pub area: Rect,
    pub strip: Rect,
    pub left: Rect,
    pub tabs: Rect,
    pub editor: Rect,
    pub bottom: Rect,
    pub status: Rect,
    pub splitters: Vec<(SplitterId, Rect)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Splitter(SplitterId),
    Panel(PanelId, Rect),
    Tabs(Rect),
    Strip,
    Status,
    None,
}

#[derive(Debug, Clone)]
pub struct LayoutState {
    pub left: Option<ToolWindowId>,
    pub left_w: u16,
    pub bottom: Option<ToolWindowId>,
    pub bottom_h: u16,
    pub rects: Rects,
    /// Clickable areas of the tool strip, filled by the strip renderer.
    pub strip_hits: Vec<(ToolWindowId, Rect)>,
}

impl LayoutState {
    pub fn new(left_w: u16, bottom_h: u16) -> Self {
        Self { left: Some(ToolWindowId::Project), left_w, bottom: None, bottom_h, rects: Rects::default(), strip_hits: Vec::new() }
    }

    pub fn is_visible(&self, tw: ToolWindowId) -> bool {
        self.left == Some(tw) || self.bottom == Some(tw)
    }

    pub fn show(&mut self, tw: ToolWindowId) {
        match tw.side() {
            Side::Left => self.left = Some(tw),
            Side::Bottom => self.bottom = Some(tw),
        }
    }

    pub fn hide(&mut self, tw: ToolWindowId) {
        match tw.side() {
            Side::Left if self.left == Some(tw) => self.left = None,
            Side::Bottom if self.bottom == Some(tw) => self.bottom = None,
            _ => {}
        }
    }

    pub fn hide_all(&mut self) {
        self.left = None;
        self.bottom = None;
    }

    pub fn compute(&mut self, area: Rect) {
        let mut r = Rects { area, ..Default::default() };
        if area.height < 2 || area.width < STRIP_W + 2 {
            self.rects = r;
            return;
        }
        r.status = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let body = Rect::new(area.x, area.y, area.width, area.height - 1);
        r.strip = Rect::new(body.x, body.y, STRIP_W, body.height);
        let content = Rect::new(body.x + STRIP_W, body.y, body.width - STRIP_W, body.height);

        // Bottom tool window across the full content width.
        let mut upper = content;
        if self.bottom.is_some() {
            let max_h = content.height.saturating_sub(MIN_CENTER_H + 1);
            self.bottom_h = self.bottom_h.clamp(MIN_BOTTOM_H.min(max_h.max(1)), max_h.max(1));
            if content.height > self.bottom_h + 1 {
                r.bottom = Rect::new(content.x, content.bottom() - self.bottom_h, content.width, self.bottom_h);
                let split = Rect::new(content.x, r.bottom.y - 1, content.width, 1);
                r.splitters.push((SplitterId::Bottom, split));
                upper = Rect::new(content.x, content.y, content.width, content.height - self.bottom_h - 1);
            }
        }

        let mut center = upper;
        if self.left.is_some() {
            let max_w = upper.width.saturating_sub(MIN_CENTER_W + 1);
            self.left_w = self.left_w.clamp(MIN_LEFT_W.min(max_w.max(1)), max_w.max(1));
            if upper.width > self.left_w + 1 {
                r.left = Rect::new(upper.x, upper.y, self.left_w, upper.height);
                let split = Rect::new(upper.x + self.left_w, upper.y, 1, upper.height);
                r.splitters.push((SplitterId::Left, split));
                center = Rect::new(split.x + 1, upper.y, upper.width - self.left_w - 1, upper.height);
            }
        }
        r.tabs = Rect::new(center.x, center.y, center.width, 1.min(center.height));
        r.editor = Rect::new(center.x, center.y + r.tabs.height, center.width, center.height.saturating_sub(r.tabs.height));
        self.rects = r;
    }

    pub fn hit_test(&self, x: u16, y: u16) -> Hit {
        let p = ratatui::layout::Position::new(x, y);
        for (id, r) in &self.rects.splitters {
            if r.contains(p) {
                return Hit::Splitter(*id);
            }
        }
        if self.rects.strip.contains(p) {
            return Hit::Strip;
        }
        if self.rects.status.contains(p) {
            return Hit::Status;
        }
        if let Some(tw) = self.left
            && self.rects.left.contains(p)
        {
            return Hit::Panel(tw.panel(), self.rects.left);
        }
        if let Some(tw) = self.bottom
            && self.rects.bottom.contains(p)
        {
            return Hit::Panel(tw.panel(), self.rects.bottom);
        }
        if self.rects.tabs.contains(p) {
            return Hit::Tabs(self.rects.tabs);
        }
        if self.rects.editor.contains(p) {
            return Hit::Panel(PanelId::Editor, self.rects.editor);
        }
        Hit::None
    }

    /// Resize the splitter being dragged so that it sits at the pointer.
    pub fn drag_splitter(&mut self, id: SplitterId, x: u16, y: u16) {
        match id {
            SplitterId::Left => {
                let base = self.rects.strip.right();
                self.left_w = x.saturating_sub(base).max(MIN_LEFT_W);
            }
            SplitterId::Bottom => {
                let bottom = self.rects.status.y;
                self.bottom_h = bottom.saturating_sub(y + 1).max(MIN_BOTTOM_H);
            }
        }
    }

    pub fn strip_hit(&self, x: u16, y: u16) -> Option<ToolWindowId> {
        let p = ratatui::layout::Position::new(x, y);
        self.strip_hits.iter().find(|(_, r)| r.contains(p)).map(|(tw, _)| *tw)
    }
}
