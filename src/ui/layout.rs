//! Fixed JetBrains-style layout: tool strip on the left, optional left tool
//! window, editor (tabs + text) in the centre, optional right tool window,
//! optional bottom tool window spanning the full width, status bar at the
//! bottom. A toolbar row above all of it (`[ui] toolbar`) holds the main menu
//! button. Each tool window has a side (its default, or one the user chose);
//! a second strip on the right edge exists while some window sits on the
//! right. Zen mode drops all
//! of it but the editor text. A maximized tool window takes the whole area
//! between the strip and the right edge instead of the editor.

use ratatui::layout::Rect;

use crate::ui::focus::{Focus, PanelId, Side, ToolWindowId};

pub const STRIP_W: u16 = 3;
/// Minimum width of a side (left or right) tool window.
pub const MIN_LEFT_W: u16 = 12;
pub const MIN_BOTTOM_H: u16 = 4;
pub const MIN_CENTER_W: u16 = 30;
pub const MIN_CENTER_H: u16 = 5;
/// Upper bound for a restored tool window size; `compute` then fits it to
/// the terminal.
pub const MAX_RESTORED: u16 = 500;
/// Terminal rows needed to show the toolbar row.
pub const TOOLBAR_MIN_H: u16 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitterId {
    Left,
    Bottom,
    Right,
}

#[derive(Debug, Clone, Default)]
pub struct Rects {
    pub area: Rect,
    /// The top row; empty when hidden.
    pub toolbar: Rect,
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
    Toolbar,
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
    /// Opens a menu of the strip's windows that got no button.
    More,
}

/// A button of the toolbar row; later tasks add variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarButton {
    /// Opens the main menu.
    Menu,
}

/// How a tool window docks: an Unpinned one hides when focus leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Pinned,
    Unpinned,
}

/// Where a dragged strip button lands: in the place of a button, or at
/// the end of a side that has no button to drop onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripDrop {
    Onto(ToolWindowId),
    End(Side),
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
    /// Strip order, a permutation of ToolWindowId::ALL; groups are its windows per side.
    order: Vec<ToolWindowId>,
    pub rects: Rects,
    /// Clickable areas of the tool strip, filled by the strip renderer.
    pub strip_hits: Vec<(StripButton, Rect)>,
    /// The `[ui] toolbar` setting, copied in by `ui::render` before `compute`.
    pub toolbar_enabled: bool,
    /// Clickable areas of the toolbar, filled by `ui::toolbar::render`.
    pub toolbar_hits: Vec<(ToolbarButton, Rect)>,
    /// Set while in zen mode: the editor text fills the screen.
    pub zen: Option<ZenSaved>,
    /// A tool window filling the content area; cleared when it loses focus
    /// (`keep_maximized`), is hidden, or zen starts. Not saved in sessions.
    pub maximized: Option<ToolWindowId>,
    /// Windows that are not Pinned (never holds `Pinned`).
    mode: std::collections::HashMap<ToolWindowId, ViewMode>,
    /// `Focus::moves` as of the last `keep_unpinned`.
    seen_moves: u64,
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
            order: ToolWindowId::ALL.to_vec(),
            rects: Rects::default(),
            strip_hits: Vec::new(),
            toolbar_enabled: false,
            toolbar_hits: Vec::new(),
            zen: None,
            maximized: None,
            mode: std::collections::HashMap::new(),
            seen_moves: 0,
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

    /// The strip order: each side's buttons follow it.
    pub fn order(&self) -> &[ToolWindowId] {
        &self.order
    }

    /// Session restore: duplicates keep the first, missing windows follow in `ALL` order.
    pub fn set_order(&mut self, order: &[ToolWindowId]) {
        let mut v: Vec<ToolWindowId> = Vec::with_capacity(ToolWindowId::ALL.len());
        for tw in order.iter().copied().chain(ToolWindowId::ALL) {
            if !v.contains(&tw) {
                v.push(tw);
            }
        }
        self.order = v;
    }

    /// The order to save: empty when it is the default.
    pub fn order_overrides(&self) -> Vec<ToolWindowId> {
        if self.order == ToolWindowId::ALL { Vec::new() } else { self.order.clone() }
    }

    /// The indexes in `order` of `tw` and of its visual neighbour in its group.
    fn button_neighbour(&self, tw: ToolWindowId, up: bool) -> Option<(usize, usize)> {
        let side = self.side_of(tw);
        let i = self.order.iter().position(|t| *t == tw)?;
        let same = |k: &usize| self.side_of(self.order[*k]) == side;
        // Bottom buttons are drawn bottom to top, so up is the next one in `order`.
        let k = if up == (side != Side::Bottom) { (0..i).rev().find(same) } else { (i + 1..self.order.len()).find(same) };
        k.map(|k| (i, k))
    }

    /// Whether `tw`'s button has a neighbour above (`up`) or below it.
    pub fn can_move_button(&self, tw: ToolWindowId, up: bool) -> bool {
        self.button_neighbour(tw, up).is_some()
    }

    /// Swap `tw`'s button with its visual neighbour in the same group.
    pub fn move_button(&mut self, tw: ToolWindowId, up: bool) {
        if let Some((i, k)) = self.button_neighbour(tw, up) {
            self.order.swap(i, k);
        }
    }

    /// Reorder for a dropped strip button and return the side `tw` must move
    /// to (`None`: nothing to do). Dropping onto a button takes its place.
    pub fn drop_button(&mut self, tw: ToolWindowId, drop: StripDrop) -> Option<Side> {
        match drop {
            StripDrop::Onto(b) if b == tw => None,
            StripDrop::Onto(b) => {
                let pos = |t: ToolWindowId| self.order.iter().position(|x| *x == t);
                let after = self.side_of(b) == self.side_of(tw) && pos(b) > pos(tw);
                self.put(tw, b, after);
                Some(self.side_of(b))
            }
            StripDrop::End(side) => {
                let last = self.order.iter().rev().copied().find(|t| *t != tw && self.side_of(*t) == side);
                if let Some(last) = last {
                    self.put(tw, last, true);
                }
                Some(side)
            }
        }
    }

    /// Move `tw` right after or before `next_to` in `order`.
    fn put(&mut self, tw: ToolWindowId, next_to: ToolWindowId, after: bool) {
        self.order.retain(|t| *t != tw);
        let at = self.order.iter().position(|t| *t == next_to).map_or(self.order.len(), |j| j + usize::from(after));
        self.order.insert(at, tw);
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

    /// Restore Default Layout: every window back on its default side (a
    /// shown one stays shown there, as `move_to`), the default strip order,
    /// nothing maximized. Sizes and zen are kept.
    pub fn restore_default(&mut self) {
        for (tw, _) in self.side_overrides() {
            self.move_to(tw, tw.default_side());
        }
        self.order = ToolWindowId::ALL.to_vec();
        self.maximized = None;
        self.mode.clear();
    }

    pub fn view_mode(&self, tw: ToolWindowId) -> ViewMode {
        self.mode.get(&tw).copied().unwrap_or_default()
    }

    pub fn set_view_mode(&mut self, tw: ToolWindowId, mode: ViewMode) {
        if mode == ViewMode::Pinned {
            self.mode.remove(&tw);
        } else {
            self.mode.insert(tw, mode);
        }
    }

    /// Unpinned windows in `ToolWindowId::ALL` order (for the session).
    pub fn unpinned(&self) -> Vec<ToolWindowId> {
        ToolWindowId::ALL.into_iter().filter(|tw| self.view_mode(*tw) == ViewMode::Unpinned).collect()
    }

    /// Replace the whole mode map (session restore).
    pub fn set_unpinned(&mut self, tws: &[ToolWindowId]) {
        self.mode = tws.iter().map(|tw| (*tw, ViewMode::Unpinned)).collect();
    }

    /// Hide the shown Unpinned windows once focus has moved since the last
    /// frame and is not on them. Overlays do not move focus, so they keep them.
    pub fn keep_unpinned(&mut self, focus: &Focus) {
        if focus.moves == self.seen_moves {
            return;
        }
        self.seen_moves = focus.moves;
        for tw in [self.left, self.bottom, self.right].into_iter().flatten() {
            if self.view_mode(tw) == ViewMode::Unpinned && tw.panel() != focus.current {
                self.hide(tw);
            }
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
        let top = u16::from(self.toolbar_enabled && area.height >= TOOLBAR_MIN_H);
        if top == 1 {
            r.toolbar = Rect::new(area.x, area.y, area.width, 1);
        }
        r.status = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let body = Rect::new(area.x, area.y + top, area.width, area.height - 1 - top);
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
        if self.rects.toolbar.contains(p) {
            return Hit::Toolbar;
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

    /// The toolbar button under a point, with its rect.
    pub fn toolbar_hit(&self, x: u16, y: u16) -> Option<(ToolbarButton, Rect)> {
        let p = ratatui::layout::Position::new(x, y);
        self.toolbar_hits.iter().find(|(_, r)| r.contains(p)).copied()
    }

    /// What dropping a dragged strip button at a point does, over the last
    /// frame's strip: a button, or the free rows of a group without buttons.
    pub fn strip_drop_at(&self, x: u16, y: u16) -> Option<StripDrop> {
        let p = ratatui::layout::Position::new(x, y);
        match self.strip_hit(x, y) {
            Some((StripButton::Tool(b), _)) => return Some(StripDrop::Onto(b)),
            Some(_) => return None,
            None => {}
        }
        let in_right = self.rects.strip_right.contains(p);
        let strip = if in_right {
            self.rects.strip_right
        } else if self.rects.strip.contains(p) {
            self.rects.strip
        } else {
            return None;
        };
        let tools: Vec<(ToolWindowId, Rect)> = self
            .strip_hits
            .iter()
            .filter_map(|(b, r)| match b {
                StripButton::Tool(t) if strip.contains(r.as_position()) => Some((*t, *r)),
                _ => None,
            })
            .collect();
        let has = |s: Side| tools.iter().any(|(t, _)| self.side_of(*t) == s);
        let mid = strip.y + strip.height / 2;
        if in_right {
            if !has(Side::Right) {
                return Some(StripDrop::End(Side::Right));
            }
        } else if y < mid {
            if !has(Side::Left) {
                return Some(StripDrop::End(Side::Left));
            }
        } else if !has(Side::Bottom) {
            return Some(StripDrop::End(Side::Bottom));
        }
        let dist = |r: &Rect| if y < r.y { r.y - y } else { y.saturating_sub(r.bottom() - 1) };
        tools.iter().min_by_key(|(_, r)| (dist(r), r.y)).map(|(t, _)| StripDrop::Onto(*t))
    }

    /// The windows of the left strip (`right == false`) or the right strip
    /// that got no button this frame, in strip order: the More menu's rows.
    pub fn strip_overflow(&self, right: bool) -> Vec<ToolWindowId> {
        self.order
            .iter()
            .copied()
            .filter(|tw| (self.side_of(*tw) == Side::Right) == right)
            .filter(|tw| !self.strip_hits.iter().any(|(b, _)| *b == StripButton::Tool(*tw)))
            .collect()
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
    fn toolbar_row_moves_everything_down() {
        let mut l = LayoutState::new(30, 10);
        l.toolbar_enabled = true;
        l.compute(Rect::new(0, 0, 120, 40));
        assert_eq!(l.rects.toolbar, Rect::new(0, 0, 120, 1));
        assert_eq!(l.rects.strip, Rect::new(0, 1, 3, 38));
        assert_eq!(l.rects.status, Rect::new(0, 39, 120, 1));
        assert_eq!(l.rects.tabs.y, 1);
        assert_eq!(l.hit_test(60, 0), Hit::Toolbar);
        l.compute(Rect::new(0, 0, 80, 11));
        assert!(l.rects.toolbar.is_empty());
        assert_eq!(l.rects.strip.y, 0);
        l.compute(Rect::new(0, 0, 80, 12));
        assert_eq!(l.rects.toolbar, Rect::new(0, 0, 80, 1));
        l.compute(Rect::new(0, 0, 120, 40));
        l.toggle_maximize(ToolWindowId::Project);
        l.compute(Rect::new(0, 0, 120, 40));
        assert_eq!(l.rects.toolbar, Rect::new(0, 0, 120, 1));
        l.toggle_maximize(ToolWindowId::Project);
        l.enter_zen(PanelId::Editor);
        l.compute(Rect::new(0, 0, 120, 40));
        assert!(l.rects.toolbar.is_empty());
        let mut l = LayoutState::new(30, 10);
        l.compute(Rect::new(0, 0, 120, 40));
        assert!(l.rects.toolbar.is_empty());
        assert_eq!(l.rects.strip, Rect::new(0, 0, 3, 39));
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
    fn restore_default_clears_sides_order_and_maximized() {
        let mut l = LayoutState::new(30, 10);
        l.show(Git);
        l.move_to(Git, Side::Right);
        l.move_to(Find, Side::Left);
        l.move_button(ToolWindowId::Worktrees, true);
        l.toggle_maximize(Git);
        assert!(!l.order_overrides().is_empty() && l.maximized.is_some());
        let sizes = (l.left_w, l.bottom_h, l.right_w);
        l.restore_default();
        assert!(l.side_overrides().is_empty());
        assert!(l.order_overrides().is_empty());
        assert_eq!(l.maximized, None);
        assert!(!l.has_right_strip());
        assert_eq!((l.left, l.bottom, l.right), (Some(Project), Some(Git), None));
        assert_eq!((l.left_w, l.bottom_h, l.right_w), sizes);

        let mut l = LayoutState::new(30, 10);
        l.show(Git);
        l.move_to(Git, Side::Right);
        l.enter_zen(PanelId::Editor);
        l.restore_default();
        assert!(l.zen.is_some());
        l.leave_zen();
        assert_eq!(l.bottom, Some(Git));
        assert_eq!(l.right, None);
    }

    #[test]
    fn unpinned_hides_after_a_focus_move_only() {
        let mut l = LayoutState::new(30, 10);
        let mut f = Focus::new();
        l.show(Git);
        l.set_view_mode(Git, ViewMode::Unpinned);
        f.set(PanelId::Git);
        l.keep_unpinned(&f);
        assert_eq!(l.bottom, Some(Git));
        f.set(PanelId::Git);
        l.keep_unpinned(&f);
        assert_eq!(l.bottom, Some(Git));
        f.set(PanelId::Editor);
        l.keep_unpinned(&f);
        assert_eq!(l.bottom, None);
        assert_eq!(l.left, Some(Project));
        l.show(Git);
        l.keep_unpinned(&f);
        assert_eq!(l.bottom, Some(Git));
    }

    #[test]
    fn restore_default_pins_every_window() {
        let mut l = LayoutState::new(30, 10);
        l.set_view_mode(Git, ViewMode::Unpinned);
        l.set_view_mode(Find, ViewMode::Unpinned);
        assert_eq!(l.unpinned().len(), 2);
        l.restore_default();
        assert!(l.unpinned().is_empty());
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

    #[test]
    fn order_set_and_overrides() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        assert_eq!(l.order(), ToolWindowId::ALL);
        assert!(l.order_overrides().is_empty());
        l.set_order(&[Git, Project, Git]);
        let want = [Git, Project, Worktrees, Structure, Find, Run, Problems, Terminal, EventLog, LocalHistory, Tasks, Todo, Hierarchy];
        assert_eq!(l.order(), want);
        assert_eq!(l.order_overrides(), want);
        l.set_order(&[]);
        assert_eq!(l.order(), ToolWindowId::ALL);
    }

    #[test]
    fn move_button_follows_the_visual_order() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        l.move_button(ToolWindowId::Worktrees, true);
        assert_eq!(&l.order()[..3], [Worktrees, Project, Structure]);
        l.move_button(Project, false);
        assert_eq!(&l.order()[..3], [Worktrees, Structure, Project]);
        assert!(!l.can_move_button(Worktrees, true));
        assert!(!l.can_move_button(Find, false), "Find is the lowest bottom button");
        l.move_button(Find, true);
        assert_eq!(&l.order()[3..5], [Run, Find]);
        assert!(!l.can_move_button(Hierarchy, true));
        l.move_to(Terminal, Side::Right);
        assert!(!l.can_move_button(Terminal, true) && !l.can_move_button(Terminal, false));
        let before = l.order().to_vec();
        l.move_button(Terminal, true);
        assert_eq!(l.order(), before);
    }

    #[test]
    fn drop_button_takes_the_target_place() {
        use ToolWindowId::*;
        let bottom = |l: &LayoutState| l.order().iter().copied().filter(|t| l.side_of(*t) == Side::Bottom).collect::<Vec<_>>();
        let mut l = LayoutState::new(30, 10);
        assert_eq!(l.drop_button(Find, StripDrop::Onto(Problems)), Some(Side::Bottom));
        assert_eq!(&bottom(&l)[..3], [Run, Problems, Find]);
        assert_eq!(l.drop_button(Terminal, StripDrop::Onto(Find)), Some(Side::Bottom));
        assert_eq!(&bottom(&l)[..4], [Run, Problems, Terminal, Find]);
        assert_eq!(l.drop_button(Git, StripDrop::Onto(Worktrees)), Some(Side::Left));
        let o = l.order();
        assert_eq!(o.iter().position(|t| *t == Git).unwrap() + 1, o.iter().position(|t| *t == Worktrees).unwrap());
        assert_eq!(l.side_of(Git), Side::Bottom, "drop_button only reorders");
        let before = l.order().to_vec();
        assert_eq!(l.drop_button(Run, StripDrop::End(Side::Right)), Some(Side::Right));
        assert_eq!(l.order(), before);
        assert_eq!(l.drop_button(Run, StripDrop::Onto(Run)), None);
    }

    #[test]
    fn strip_drop_at_maps_rows() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        l.compute(Rect::new(0, 0, 120, 40));
        l.strip_hits = vec![
            (StripButton::Menu, Rect::new(0, 0, 3, 1)),
            (StripButton::Tool(Project), Rect::new(0, 2, 3, 2)),
            (StripButton::Tool(Find), Rect::new(0, 37, 3, 2)),
        ];
        assert_eq!(l.strip_drop_at(1, 2), Some(StripDrop::Onto(Project)));
        assert_eq!(l.strip_drop_at(1, 0), None);
        assert_eq!(l.strip_drop_at(1, 10), Some(StripDrop::Onto(Project)));
        assert_eq!(l.strip_drop_at(1, 30), Some(StripDrop::Onto(Find)));
        assert_eq!(l.strip_drop_at(60, 10), None);
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1)), (StripButton::Tool(Find), Rect::new(0, 37, 3, 2))];
        assert_eq!(l.strip_drop_at(1, 5), Some(StripDrop::End(Side::Left)));
        assert_eq!(l.strip_drop_at(1, 30), Some(StripDrop::Onto(Find)));
        // No Bottom button: a free row at or below mid is the Bottom end.
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1)), (StripButton::Tool(Project), Rect::new(0, 2, 3, 2))];
        assert_eq!(l.strip_drop_at(1, 30), Some(StripDrop::End(Side::Bottom)));
        // The right strip's free row is the Right end.
        l.move_to(Hierarchy, Side::Right);
        l.compute(Rect::new(0, 0, 120, 40));
        let sr = l.rects.strip_right;
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1))];
        assert_eq!(l.strip_drop_at(sr.x + 1, sr.y + 5), Some(StripDrop::End(Side::Right)));
        // The right strip is drawn only while a window is on the right, so a
        // hidden one never yields the Right end, whatever the point.
        let mut l = LayoutState::new(30, 10);
        l.compute(Rect::new(0, 0, 120, 40));
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1))];
        assert!(!l.has_right_strip());
        assert!((0..120).all(|x| (0..40).all(|y| l.strip_drop_at(x, y) != Some(StripDrop::End(Side::Right)))));
        // A More cell is not a drop target.
        l.strip_hits = vec![(StripButton::More, Rect::new(0, 38, 3, 1))];
        assert_eq!(l.strip_drop_at(1, 38), None);
    }
}
