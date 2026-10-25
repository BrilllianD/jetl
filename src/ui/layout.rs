//! Fixed JetBrains-style layout: tool strip on the left, optional left tool
//! window, editor (tabs + text) in the centre, optional right tool window,
//! optional bottom tool window spanning the full width, status bar at the
//! bottom. Each tool window has a side (its default, or one the user chose);
//! a second strip on the right edge exists while some window sits on the
//! right. Zen mode drops all
//! of it but the editor text. A maximized tool window takes the whole area
//! between the strip and the right edge instead of the editor.

use ratatui::layout::Rect;

use crate::ui::focus::{PanelId, Side, ToolWindowId};

pub const STRIP_W: u16 = 3;
/// Minimum width of a side (left or right) tool window.
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
    Right,
}

#[derive(Debug, Clone, Default)]
pub struct Rects {
    pub area: Rect,
    pub strip: Rect,
    /// Empty unless a window's side is Right.
    pub strip_right: Rect,
    pub left: Rect,
    pub tabs: Rect,
    pub editor: Rect,
    pub bottom: Rect,
    pub right: Rect,
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
    pub right: Option<ToolWindowId>,
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
    pub right: Option<ToolWindowId>,
    pub right_w: u16,
    /// Windows moved off their default side.
    sides: std::collections::HashMap<ToolWindowId, Side>,
    pub rects: Rects,
    /// Clickable areas of the tool strip, filled by the strip renderer.
    pub strip_hits: Vec<(StripButton, Rect)>,
    /// Set while in zen mode: the editor text fills the screen.
    pub zen: Option<ZenSaved>,
    /// A tool window filling the content area; cleared when it loses focus
    /// (`keep_maximized`), is hidden, or zen starts. Not saved in sessions.
    pub maximized: Option<ToolWindowId>,
}

impl LayoutState {
    pub fn new(left_w: u16, bottom_h: u16) -> Self {
        Self {
            left: Some(ToolWindowId::Project),
            left_w,
            bottom: None,
            bottom_h,
            right: None,
            right_w: left_w,
            sides: std::collections::HashMap::new(),
            rects: Rects::default(),
            strip_hits: Vec::new(),
            zen: None,
            maximized: None,
        }
    }

    pub fn is_visible(&self, tw: ToolWindowId) -> bool {
        self.left == Some(tw) || self.bottom == Some(tw) || self.right == Some(tw)
    }

    /// The side `tw` sits on now.
    pub fn side_of(&self, tw: ToolWindowId) -> Side {
        self.sides.get(&tw).copied().unwrap_or(tw.default_side())
    }

    /// Windows off their default side, in strip order.
    pub fn side_overrides(&self) -> Vec<(ToolWindowId, Side)> {
        ToolWindowId::ALL.into_iter().filter_map(|tw| self.sides.get(&tw).map(|s| (tw, *s))).collect()
    }

    /// Replace all side overrides; entries equal to the default are dropped.
    /// Call before [`LayoutState::restore`].
    pub fn set_sides(&mut self, sides: &[(ToolWindowId, Side)]) {
        self.sides = sides.iter().copied().filter(|(tw, s)| *s != tw.default_side()).collect();
    }

    /// Whether some window's side is Right, which shows the right strip.
    pub fn has_right_strip(&self) -> bool {
        self.sides.values().any(|s| *s == Side::Right)
    }

    /// The slot of a side; while zen is on, the one zen puts back.
    fn slot_mut(&mut self, side: Side) -> &mut Option<ToolWindowId> {
        match (&mut self.zen, side) {
            (Some(z), Side::Left) => &mut z.left,
            (Some(z), Side::Bottom) => &mut z.bottom,
            (Some(z), Side::Right) => &mut z.right,
            (None, Side::Left) => &mut self.left,
            (None, Side::Bottom) => &mut self.bottom,
            (None, Side::Right) => &mut self.right,
        }
    }

    /// Put `tw` on `side`. A shown window stays shown there and hides the
    /// window that side showed; a hidden one only changes side. In zen mode
    /// the windows zen puts back are moved.
    pub fn move_to(&mut self, tw: ToolWindowId, side: Side) {
        let from = self.side_of(tw);
        if from == side {
            return;
        }
        let slot = self.slot_mut(from);
        let shown = *slot == Some(tw);
        if shown {
            *slot = None;
        }
        if side == tw.default_side() {
            self.sides.remove(&tw);
        } else {
            self.sides.insert(tw, side);
        }
        if shown {
            *self.slot_mut(side) = Some(tw);
        }
    }

    /// The maximized window, if it is still shown.
    pub fn maximized_window(&self) -> Option<ToolWindowId> {
        self.maximized.filter(|tw| self.is_visible(*tw))
    }

    /// Maximize `tw`, or restore when it already is. Does nothing for a
    /// hidden window or in zen mode.
    pub fn toggle_maximize(&mut self, tw: ToolWindowId) {
        if self.maximized == Some(tw) {
            self.maximized = None;
        } else if self.zen.is_none() && self.is_visible(tw) {
            self.maximized = Some(tw);
        }
    }

    /// Restore the layout once focus has left the maximized window.
    pub fn keep_maximized(&mut self, focus: PanelId) {
        if self.maximized.is_some_and(|tw| focus != tw.panel() || !self.is_visible(tw)) {
            self.maximized = None;
        }
    }

    /// Showing a tool window ends zen mode first, so the window comes back
    /// with the rest of the pre-zen layout.
    pub fn show(&mut self, tw: ToolWindowId) {
        self.leave_zen();
        match self.side_of(tw) {
            Side::Left => self.left = Some(tw),
            Side::Bottom => self.bottom = Some(tw),
            Side::Right => self.right = Some(tw),
        }
    }

    pub fn hide(&mut self, tw: ToolWindowId) {
        match self.side_of(tw) {
            Side::Left if self.left == Some(tw) => self.left = None,
            Side::Bottom if self.bottom == Some(tw) => self.bottom = None,
            Side::Right if self.right == Some(tw) => self.right = None,
            _ => {}
        }
        if self.maximized == Some(tw) {
            self.maximized = None;
        }
    }

    /// Apply saved tool windows and sizes (session restore). A window saved
    /// on the wrong side is dropped and sizes are clamped to sane bounds.
    /// In zen mode the windows become the ones zen puts back.
    pub fn restore(
        &mut self,
        left: Option<ToolWindowId>,
        left_w: u16,
        bottom: Option<ToolWindowId>,
        bottom_h: u16,
        right: Option<ToolWindowId>,
        right_w: Option<u16>,
    ) {
        let left = left.filter(|tw| self.side_of(*tw) == Side::Left);
        let bottom = bottom.filter(|tw| self.side_of(*tw) == Side::Bottom);
        let right = right.filter(|tw| self.side_of(*tw) == Side::Right);
        self.maximized = None;
        match &mut self.zen {
            Some(z) => (z.left, z.bottom, z.right) = (left, bottom, right),
            None => (self.left, self.bottom, self.right) = (left, bottom, right),
        }
        if let Some(w) = right_w {
            self.right_w = w.clamp(MIN_LEFT_W, MAX_RESTORED);
        }
        self.left_w = left_w.clamp(MIN_LEFT_W, MAX_RESTORED);
        self.bottom_h = bottom_h.clamp(MIN_BOTTOM_H, MAX_RESTORED);
    }

    pub fn hide_all(&mut self) {
        self.left = None;
        self.bottom = None;
        self.right = None;
        self.maximized = None;
    }

    /// Hide everything but the editor text, remembering the tool windows
    /// and `focus` for [`LayoutState::leave_zen`].
    pub fn enter_zen(&mut self, focus: PanelId) {
        if self.zen.is_none() {
            self.zen = Some(ZenSaved { left: self.left, bottom: self.bottom, right: self.right, focus });
            self.hide_all();
        }
    }

    /// Put the pre-zen tool windows back; returns the focus to restore, or
    /// `None` when not in zen mode.
    pub fn leave_zen(&mut self) -> Option<PanelId> {
        let z = self.zen.take()?;
        (self.left, self.bottom, self.right) = (z.left, z.bottom, z.right);
        Some(z.focus)
    }

    /// The tool windows a session saves: the pre-zen ones while in zen mode,
    /// which is not saved itself.
    pub fn saved_windows(&self) -> (Option<ToolWindowId>, Option<ToolWindowId>, Option<ToolWindowId>) {
        match self.zen {
            Some(z) => (z.left, z.bottom, z.right),
            None => (self.left, self.bottom, self.right),
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
        let rs_w = if self.has_right_strip() { STRIP_W } else { 0 };
        if area.height < 2 || area.width < STRIP_W + rs_w + 2 {
            self.rects = r;
            return;
        }
        r.status = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let body = Rect::new(area.x, area.y, area.width, area.height - 1);
        r.strip = Rect::new(body.x, body.y, STRIP_W, body.height);
        if rs_w > 0 {
            r.strip_right = Rect::new(body.right() - STRIP_W, body.y, STRIP_W, body.height);
        }
        let content = Rect::new(body.x + STRIP_W, body.y, body.width - STRIP_W - rs_w, body.height);

        if let Some(tw) = self.maximized_window() {
            // The window takes the whole content area; sizes stay for the restore.
            match self.side_of(tw) {
                Side::Left => r.left = content,
                Side::Bottom => r.bottom = content,
                Side::Right => r.right = content,
            }
            r.tabs = Rect::new(content.x, content.y, 0, 0);
            r.editor = Rect::new(content.x, content.y, 0, 0);
            self.rects = r;
            return;
        }

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
        if self.right.is_some() {
            // No room beside the left window and the editor's minimum: not drawn.
            let max_w = center.width.saturating_sub(MIN_CENTER_W + 1);
            if max_w > 0 {
                self.right_w = self.right_w.clamp(MIN_LEFT_W.min(max_w), max_w);
            }
            if max_w > 0 && center.width > self.right_w + 1 {
                r.right = Rect::new(center.right() - self.right_w, center.y, self.right_w, center.height);
                let split = Rect::new(r.right.x - 1, center.y, 1, center.height);
                r.splitters.push((SplitterId::Right, split));
                center = Rect::new(center.x, center.y, center.width - self.right_w - 1, center.height);
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
        if self.rects.strip.contains(p) || self.rects.strip_right.contains(p) {
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
        if let Some(tw) = self.right
            && self.rects.right.contains(p)
        {
            return Hit::Panel(tw.panel(), self.rects.right);
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
            SplitterId::Right => {
                let edge = if self.rects.strip_right.is_empty() { self.rects.area.right() } else { self.rects.strip_right.x };
                self.right_w = edge.saturating_sub(x + 1).max(MIN_LEFT_W);
            }
        }
    }

    /// Keyboard resize: move a splitter by `delta` cells, bounded below like
    /// a drag and then fitted to the terminal the way a frame would.
    pub fn resize(&mut self, id: SplitterId, delta: i16) {
        match id {
            SplitterId::Left => self.left_w = self.left_w.saturating_add_signed(delta).max(MIN_LEFT_W),
            SplitterId::Bottom => self.bottom_h = self.bottom_h.saturating_add_signed(delta).max(MIN_BOTTOM_H),
            SplitterId::Right => self.right_w = self.right_w.saturating_add_signed(delta).max(MIN_LEFT_W),
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
        assert!(r.strip_right.is_empty() && r.right.is_empty());
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
        assert_eq!(l.saved_windows(), (Some(ToolWindowId::Project), Some(ToolWindowId::Git), None));
        // A restored session while in zen replaces what zen puts back.
        l.restore(None, 40, Some(ToolWindowId::Find), 8, None, None);
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

    #[test]
    fn maximize_fills_the_content_and_restores() {
        let mut l = LayoutState::new(30, 10);
        l.show(ToolWindowId::Git);
        l.compute(Rect::new(0, 0, 100, 30));
        let (strip, status) = (l.rects.strip, l.rects.status);
        l.toggle_maximize(ToolWindowId::Git);
        l.compute(Rect::new(0, 0, 100, 30));
        let r = l.rects.clone();
        assert_eq!(r.bottom, Rect::new(STRIP_W, 0, 100 - STRIP_W, 29));
        assert!(r.left.is_empty() && r.editor.is_empty() && r.tabs.is_empty() && r.splitters.is_empty());
        assert_eq!((r.strip, r.status), (strip, status));
        assert_eq!(l.hit_test(50, 5), Hit::Panel(PanelId::Git, r.bottom));
        assert_eq!((l.left_w, l.bottom_h), (30, 10));
        l.toggle_maximize(ToolWindowId::Git);
        l.compute(Rect::new(0, 0, 100, 30));
        assert_eq!((l.rects.left.width, l.rects.bottom.height), (30, 10));

        l.toggle_maximize(ToolWindowId::Project);
        l.compute(Rect::new(0, 0, 100, 30));
        assert_eq!(l.rects.left, Rect::new(STRIP_W, 0, 97, 29));
        assert!(l.rects.bottom.is_empty());

        let mut l = LayoutState::new(30, 10);
        l.toggle_maximize(ToolWindowId::Find);
        assert_eq!(l.maximized, None);
    }

    #[test]
    fn maximize_ends_on_focus_change_hide_and_zen() {
        let mut l = LayoutState::new(30, 10);
        l.show(ToolWindowId::Git);
        l.toggle_maximize(ToolWindowId::Git);
        l.keep_maximized(PanelId::Git);
        assert_eq!(l.maximized, Some(ToolWindowId::Git));
        l.keep_maximized(PanelId::Editor);
        assert_eq!(l.maximized, None);

        l.toggle_maximize(ToolWindowId::Git);
        l.hide(ToolWindowId::Git);
        assert_eq!(l.maximized, None);

        l.show(ToolWindowId::Git);
        l.toggle_maximize(ToolWindowId::Git);
        l.hide(ToolWindowId::Project);
        assert_eq!(l.maximized, Some(ToolWindowId::Git));
        l.enter_zen(PanelId::Git);
        assert_eq!(l.maximized, None);

        let mut l = LayoutState::new(30, 10);
        l.show(ToolWindowId::Git);
        l.toggle_maximize(ToolWindowId::Git);
        l.show(ToolWindowId::Find);
        assert_eq!(l.maximized_window(), None);
    }

    use ToolWindowId::{Find, Git, Project};

    #[test]
    fn move_to_changes_side_and_slot() {
        let mut l = LayoutState::new(30, 10);
        l.show(Git);
        l.move_to(Git, Side::Right);
        assert_eq!((l.bottom, l.right), (None, Some(Git)));
        assert_eq!(l.side_of(Git), Side::Right);
        assert_eq!(l.side_overrides(), vec![(Git, Side::Right)]);
        assert!(l.has_right_strip());
        l.move_to(Find, Side::Right);
        assert_eq!(l.right, Some(Git));
        assert_eq!(l.side_overrides(), vec![(Find, Side::Right), (Git, Side::Right)]);
        l.move_to(Git, Side::Bottom);
        assert_eq!(l.bottom, Some(Git));
        assert_eq!(l.side_overrides(), vec![(Find, Side::Right)]);
        l.move_to(Find, Side::Left);
        assert_eq!(l.left, Some(Project));
        l.show(Find);
        assert_eq!(l.left, Some(Find));
        l.move_to(Project, Side::Left);
        l.set_sides(&[(Project, Side::Left), (Git, Side::Right)]);
        assert_eq!(l.side_overrides(), vec![(Git, Side::Right)]);
    }

    #[test]
    fn right_window_compute_hit_and_drag() {
        let mut l = LayoutState::new(30, 10);
        l.hide_all();
        l.move_to(Git, Side::Right);
        l.show(Git);
        l.compute(Rect::new(0, 0, 120, 40));
        let r = l.rects.clone();
        assert_eq!(r.strip_right, Rect::new(117, 0, 3, 39));
        assert_eq!(r.right, Rect::new(117 - 30, 0, 30, 39));
        let split = r.splitters.iter().find(|(id, _)| *id == SplitterId::Right).unwrap().1;
        assert_eq!(split.x, r.right.x - 1);
        assert_eq!(l.hit_test(118, 5), Hit::Strip);
        assert_eq!(l.hit_test(split.x, 5), Hit::Splitter(SplitterId::Right));
        assert_eq!(l.hit_test(r.right.x + 2, 5), Hit::Panel(PanelId::Git, r.right));
        assert_eq!(r.editor.right(), r.right.x - 1);
        l.drag_splitter(SplitterId::Right, 80, 0);
        assert_eq!(l.right_w, 117 - 81);
        l.resize(SplitterId::Right, 2);
        assert_eq!(l.right_w, 117 - 81 + 2);
        l.resize(SplitterId::Right, -500);
        assert_eq!(l.right_w, MIN_LEFT_W);
        l.show(Project);
        l.resize(SplitterId::Left, 500);
        l.resize(SplitterId::Right, 500);
        assert!(l.rects.editor.width >= MIN_CENTER_W);

        let mut l = LayoutState::new(30, 10);
        l.move_to(Git, Side::Right);
        l.show(Git);
        l.show(Find);
        l.compute(Rect::new(0, 0, 120, 40));
        assert!(l.rects.right.bottom() < l.rects.bottom.y);
    }

    #[test]
    fn maximize_and_zen_on_the_right() {
        let mut l = LayoutState::new(30, 10);
        l.move_to(Git, Side::Right);
        l.show(Git);
        l.toggle_maximize(Git);
        l.compute(Rect::new(0, 0, 100, 30));
        assert_eq!(l.rects.right, Rect::new(STRIP_W, 0, 100 - 2 * STRIP_W, 29));
        assert!(l.rects.left.is_empty() && l.rects.bottom.is_empty() && l.rects.editor.is_empty());
        l.toggle_maximize(Git);
        l.enter_zen(PanelId::Git);
        assert_eq!(l.saved_windows(), (Some(Project), None, Some(Git)));
        l.move_to(Git, Side::Bottom);
        assert_eq!(l.saved_windows(), (Some(Project), Some(Git), None));
        l.leave_zen();
        assert_eq!((l.bottom, l.right), (Some(Git), None));

        l.move_to(Git, Side::Right);
        l.hide(Git);
        l.set_sides(&[]);
        l.restore(None, 30, None, 10, Some(Git), Some(5));
        assert_eq!(l.right, None);
        assert_eq!(l.right_w, MIN_LEFT_W);
    }
}
