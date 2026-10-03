//! Fixed JetBrains-style layout: tool strip on the left, optional left tool
//! window, editor (tabs + text) in the centre, optional bottom tool window
//! spanning the full width, status bar at the bottom. Zen mode drops all
//! of it but the editor text.

use ratatui::layout::Rect;

use crate::ui::focus::{PanelId, Side, ToolWindowId};

pub const STRIP_W: u16 = 3;
pub const MIN_LEFT_W: u16 = 12;
pub const MIN_BOTTOM_H: u16 = 4;
pub const MIN_CENTER_W: u16 = 30;
pub const MIN_CENTER_H: u16 = 5;
/// Upper bound for a restored tool window size; `compute` then fits it to
/// the terminal.
pub const MAX_RESTORED: u16 = 500;

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

/// What zen mode hid, put back when it ends. Sizes need no copy: nothing
/// changes them while no tool window is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZenSaved {
    pub left: Option<ToolWindowId>,
    pub bottom: Option<ToolWindowId>,
    pub focus: PanelId,
}

/// A button of the tool strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripButton {
    /// The top button, which opens the main menu.
    Menu,
    Tool(ToolWindowId),
}

#[derive(Debug, Clone)]
pub struct LayoutState {
    pub left: Option<ToolWindowId>,
    pub left_w: u16,
    pub bottom: Option<ToolWindowId>,
    pub bottom_h: u16,
    pub rects: Rects,
    /// Clickable areas of the tool strip, filled by the strip renderer.
    pub strip_hits: Vec<(StripButton, Rect)>,
    /// Set while in zen mode: the editor text fills the screen.
    pub zen: Option<ZenSaved>,
}

impl LayoutState {
    pub fn new(left_w: u16, bottom_h: u16) -> Self {
        Self {
            left: Some(ToolWindowId::Project),
            left_w,
            bottom: None,
            bottom_h,
            rects: Rects::default(),
            strip_hits: Vec::new(),
            zen: None,
        }
    }

    pub fn is_visible(&self, tw: ToolWindowId) -> bool {
        self.left == Some(tw) || self.bottom == Some(tw)
    }

    /// Showing a tool window ends zen mode first, so the window comes back
    /// with the rest of the pre-zen layout.
    pub fn show(&mut self, tw: ToolWindowId) {
        self.leave_zen();
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

    /// Apply saved tool windows and sizes (session restore). A window saved
    /// on the wrong side is dropped and sizes are clamped to sane bounds.
    /// In zen mode the windows become the ones zen puts back.
    pub fn restore(&mut self, left: Option<ToolWindowId>, left_w: u16, bottom: Option<ToolWindowId>, bottom_h: u16) {
        let left = left.filter(|tw| tw.side() == Side::Left);
        let bottom = bottom.filter(|tw| tw.side() == Side::Bottom);
        match &mut self.zen {
            Some(z) => (z.left, z.bottom) = (left, bottom),
            None => (self.left, self.bottom) = (left, bottom),
        }
        self.left_w = left_w.clamp(MIN_LEFT_W, MAX_RESTORED);
        self.bottom_h = bottom_h.clamp(MIN_BOTTOM_H, MAX_RESTORED);
    }

    pub fn hide_all(&mut self) {
        self.left = None;
        self.bottom = None;
    }

    /// Hide everything but the editor text, remembering the tool windows
    /// and `focus` for [`LayoutState::leave_zen`].
    pub fn enter_zen(&mut self, focus: PanelId) {
        if self.zen.is_none() {
            self.zen = Some(ZenSaved { left: self.left, bottom: self.bottom, focus });
            self.hide_all();
        }
    }

    /// Put the pre-zen tool windows back; returns the focus to restore, or
    /// `None` when not in zen mode.
    pub fn leave_zen(&mut self) -> Option<PanelId> {
        let z = self.zen.take()?;
        (self.left, self.bottom) = (z.left, z.bottom);
        Some(z.focus)
    }

    /// The tool windows a session saves: the pre-zen ones while in zen mode,
    /// which is not saved itself.
    pub fn saved_windows(&self) -> (Option<ToolWindowId>, Option<ToolWindowId>) {
        match self.zen {
            Some(z) => (z.left, z.bottom),
            None => (self.left, self.bottom),
        }
    }

    pub fn compute(&mut self, area: Rect) {
        let mut r = Rects { area, ..Default::default() };
        if self.zen.is_some() {
            // No strip, tabs, status bar or splitters: every other rect stays
            // empty, so hit testing finds only the editor.
            r.tabs = Rect::new(area.x, area.y, area.width, 0);
            r.editor = area;
            self.rects = r;
            return;
        }
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

    /// Keyboard resize: move a splitter by `delta` cells, bounded below like
    /// a drag and then fitted to the terminal the way a frame would.
    pub fn resize(&mut self, id: SplitterId, delta: i16) {
        match id {
            SplitterId::Left => self.left_w = self.left_w.saturating_add_signed(delta).max(MIN_LEFT_W),
            SplitterId::Bottom => self.bottom_h = self.bottom_h.saturating_add_signed(delta).max(MIN_BOTTOM_H),
        }
        if !self.rects.area.is_empty() {
            self.compute(self.rects.area);
        }
    }

    /// The strip button under a point, with its rect.
    pub fn strip_hit(&self, x: u16, y: u16) -> Option<(StripButton, Rect)> {
        let p = ratatui::layout::Position::new(x, y);
        self.strip_hits.iter().find(|(_, r)| r.contains(p)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_hide_per_side() {
        let mut l = LayoutState::new(30, 10);
        assert!(l.is_visible(ToolWindowId::Project));
        l.show(ToolWindowId::Git);
        assert!(l.is_visible(ToolWindowId::Project) && l.is_visible(ToolWindowId::Git), "sides are independent");
        // Hiding a window that is not shown leaves its side alone.
        l.hide(ToolWindowId::Worktrees);
        assert!(l.is_visible(ToolWindowId::Project));
        l.hide_all();
        assert_eq!((l.left, l.bottom), (None, None));
    }

    #[test]
    fn compute_and_hit_test_agree() {
        let mut l = LayoutState::new(30, 10);
        l.show(ToolWindowId::Git);
        l.compute(Rect::new(0, 0, 120, 40));
        let r = l.rects.clone();
        assert_eq!(r.status, Rect::new(0, 39, 120, 1));
        assert_eq!(r.left.width, 30);
        assert_eq!(r.bottom.height, 10);
        assert_eq!(l.hit_test(1, 5), Hit::Strip);
        assert_eq!(l.hit_test(5, 39), Hit::Status);
        assert_eq!(l.hit_test(r.left.x, r.left.y), Hit::Panel(PanelId::Project, r.left));
        assert_eq!(l.hit_test(r.left.right(), 5), Hit::Splitter(SplitterId::Left));
        assert_eq!(l.hit_test(50, r.bottom.y - 1), Hit::Splitter(SplitterId::Bottom));
        assert_eq!(l.hit_test(50, r.bottom.y), Hit::Panel(PanelId::Git, r.bottom));
        assert_eq!(l.hit_test(r.tabs.x, r.tabs.y), Hit::Tabs(r.tabs));
        assert_eq!(l.hit_test(r.editor.x, r.editor.y), Hit::Panel(PanelId::Editor, r.editor));
    }

    #[test]
    fn splitter_drag_respects_minimums_and_compute_clamps() {
        let mut l = LayoutState::new(30, 10);
        l.show(ToolWindowId::Git);
        l.compute(Rect::new(0, 0, 100, 30));
        l.drag_splitter(SplitterId::Left, 4, 0);
        assert_eq!(l.left_w, MIN_LEFT_W);
        l.drag_splitter(SplitterId::Left, 99, 0);
        l.drag_splitter(SplitterId::Bottom, 0, 0);
        l.compute(Rect::new(0, 0, 100, 30));
        assert!(l.rects.editor.width >= MIN_CENTER_W, "left window cannot squeeze the editor away");
        assert!(l.rects.editor.height + l.rects.tabs.height >= MIN_CENTER_H);
    }

    #[test]
    fn zen_fills_the_screen_and_puts_the_layout_back() {
        let mut l = LayoutState::new(30, 10);
        l.show(ToolWindowId::Git);
        l.enter_zen(PanelId::Git);
        l.compute(Rect::new(0, 0, 100, 30));
        let r = l.rects.clone();
        assert_eq!(r.editor, Rect::new(0, 0, 100, 30));
        assert!(r.strip.is_empty() && r.status.is_empty() && r.tabs.is_empty() && r.splitters.is_empty());
        assert_eq!(l.hit_test(0, 0), Hit::Panel(PanelId::Editor, r.editor));
        assert_eq!(l.hit_test(99, 29), Hit::Panel(PanelId::Editor, r.editor));
        assert_eq!(l.saved_windows(), (Some(ToolWindowId::Project), Some(ToolWindowId::Git)));
        // A restored session while in zen replaces what zen puts back.
        l.restore(None, 40, Some(ToolWindowId::Find), 8);
        assert_eq!((l.left, l.bottom), (None, None));
        assert_eq!(l.leave_zen(), Some(PanelId::Git));
        assert_eq!((l.left, l.bottom, l.left_w, l.bottom_h), (None, Some(ToolWindowId::Find), 40, 8));
        assert_eq!(l.leave_zen(), None);
        // Showing a window ends zen with the rest of the layout.
        l.enter_zen(PanelId::Editor);
        l.show(ToolWindowId::Project);
        assert!(l.zen.is_none());
        assert_eq!((l.left, l.bottom), (Some(ToolWindowId::Project), Some(ToolWindowId::Find)));
    }

    #[test]
    fn keyboard_resize_clamps_like_a_drag() {
        let mut l = LayoutState::new(30, 10);
        l.show(ToolWindowId::Git);
        l.compute(Rect::new(0, 0, 100, 30));
        l.resize(SplitterId::Left, 2);
        l.resize(SplitterId::Bottom, -1);
        assert_eq!((l.left_w, l.bottom_h), (32, 9));
        l.resize(SplitterId::Left, -100);
        l.resize(SplitterId::Bottom, -100);
        assert_eq!((l.left_w, l.bottom_h), (MIN_LEFT_W, MIN_BOTTOM_H));
        // Growing stops where the editor keeps its minimum size.
        l.resize(SplitterId::Left, 500);
        l.resize(SplitterId::Bottom, 500);
        assert_eq!(l.left_w, 100 - STRIP_W - MIN_CENTER_W - 1);
        assert_eq!(l.bottom_h, 30 - 1 - MIN_CENTER_H - 1);
        assert_eq!(l.rects.left.width, l.left_w, "rects follow at once");
    }
}
