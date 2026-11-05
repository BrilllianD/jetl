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

use crate::ui::focus::{Dock, Focus, PanelId, Side, ToolWindowId};

pub const STRIP_W: u16 = 3;
/// Minimum width of a side (left or right) tool window.
pub const MIN_LEFT_W: u16 = 12;
pub const MIN_BOTTOM_H: u16 = 4;
pub const MIN_CENTER_W: u16 = 30;
pub const MIN_CENTER_H: u16 = 5;
/// Upper bound for a restored tool window size; `compute` then fits it to
/// the terminal.
pub const MAX_RESTORED: u16 = 500;
/// Minimum rows of either pane of a left or right split (a bottom split uses
/// `MIN_LEFT_W` columns).
pub const MIN_SPLIT_H: u16 = 3;
/// Terminal rows needed to show the toolbar row.
pub const TOOLBAR_MIN_H: u16 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitterId {
    Left,
    Bottom,
    Right,
    LeftSplit,
    BottomSplit,
    RightSplit,
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
    /// The secondary panes (Left Bottom, Bottom Right, Right Bottom); empty
    /// unless shown and drawn.
    pub left2: Rect,
    pub bottom2: Rect,
    pub right2: Rect,
    pub status: Rect,
    /// The Undocked window's rect over the editor area; empty when none is
    /// shown, in zen and while a window is maximized.
    pub overlay: Rect,
    pub splitters: Vec<(SplitterId, Rect)>,
}

impl Rects {
    /// The rect of a dock's pane.
    pub fn of(&self, dock: Dock) -> Rect {
        match (dock.side, dock.split) {
            (Side::Left, false) => self.left,
            (Side::Left, true) => self.left2,
            (Side::Bottom, false) => self.bottom,
            (Side::Bottom, true) => self.bottom2,
            (Side::Right, false) => self.right,
            (Side::Right, true) => self.right2,
        }
    }

    fn of_mut(&mut self, dock: Dock) -> &mut Rect {
        match (dock.side, dock.split) {
            (Side::Left, false) => &mut self.left,
            (Side::Left, true) => &mut self.left2,
            (Side::Bottom, false) => &mut self.bottom,
            (Side::Bottom, true) => &mut self.bottom2,
            (Side::Right, false) => &mut self.right,
            (Side::Right, true) => &mut self.right2,
        }
    }
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
    pub left2: Option<ToolWindowId>,
    pub bottom2: Option<ToolWindowId>,
    pub right2: Option<ToolWindowId>,
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
    /// The run configuration selector (`run.pick`).
    RunConfig,
    /// Runs the selected configuration (`run.run_selected`).
    Run,
    /// Runs the last task again (`run.rerun`).
    Rerun,
    /// Stops the running task (`run.stop`).
    Stop,
    /// Opens the branches popup.
    Branch,
    /// Pulls (`git.pull`).
    Update,
    /// Opens the Commit tab and focuses the message (`git.commit_box`).
    Commit,
    /// Pushes (`git.push`).
    Push,
    /// Opens the Recent Projects palette.
    Project,
    /// Opens Search Everywhere.
    Search,
    /// Opens the Settings window.
    Settings,
}

/// How a tool window docks: a Pinned one stays, an Unpinned one hides when
/// focus leaves it, an Undocked one slides over the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ViewMode {
    #[default]
    Pinned,
    Unpinned,
    /// Draws over the editor at its side (slide-over) and hides when Esc
    /// leaves it or another tool window takes focus.
    Undocked,
}

/// Where a dragged strip button lands: in the place of a button, or at
/// the end of a group (a dock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripDrop {
    Onto(ToolWindowId),
    End(Dock),
}

#[derive(Debug, Clone)]
pub struct LayoutState {
    pub left: Option<ToolWindowId>,
    pub left_w: u16,
    pub bottom: Option<ToolWindowId>,
    pub bottom_h: u16,
    pub right: Option<ToolWindowId>,
    pub right_w: u16,
    /// The secondary slots: Left Bottom, Bottom Right, Right Bottom.
    pub left2: Option<ToolWindowId>,
    pub bottom2: Option<ToolWindowId>,
    pub right2: Option<ToolWindowId>,
    /// Cells of the secondary pane (rows for left and right, columns for
    /// bottom); `0` is half the side.
    pub left2_h: u16,
    pub bottom2_w: u16,
    pub right2_h: u16,
    /// Windows moved off their default side.
    sides: std::collections::HashMap<ToolWindowId, Side>,
    /// Windows docked in their side's second pane.
    split: std::collections::HashSet<ToolWindowId>,
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
            left2: None,
            bottom2: None,
            right2: None,
            left2_h: 0,
            bottom2_w: 0,
            right2_h: 0,
            sides: std::collections::HashMap::new(),
            split: std::collections::HashSet::new(),
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
        Dock::ALL.iter().any(|d| self.slot(*d) == Some(tw))
    }

    /// The live slot of a dock (not the zen copy).
    pub fn slot(&self, dock: Dock) -> Option<ToolWindowId> {
        match (dock.side, dock.split) {
            (Side::Left, false) => self.left,
            (Side::Left, true) => self.left2,
            (Side::Bottom, false) => self.bottom,
            (Side::Bottom, true) => self.bottom2,
            (Side::Right, false) => self.right,
            (Side::Right, true) => self.right2,
        }
    }

    /// The shown windows, in `Dock::ALL` order.
    pub fn shown(&self) -> Vec<ToolWindowId> {
        Dock::ALL.iter().filter_map(|d| self.slot(*d)).collect()
    }

    /// The side `tw` sits on now.
    pub fn side_of(&self, tw: ToolWindowId) -> Side {
        self.dock_of(tw).side
    }

    /// The dock `tw` sits in now.
    pub fn dock_of(&self, tw: ToolWindowId) -> Dock {
        Dock { side: self.sides.get(&tw).copied().unwrap_or(tw.default_side()), split: self.split.contains(&tw) }
    }

    /// Windows docked in their side's second pane, in `ALL` order.
    pub fn split_windows(&self) -> Vec<ToolWindowId> {
        ToolWindowId::ALL.into_iter().filter(|tw| self.split.contains(tw)).collect()
    }

    /// Replace the set of second-pane windows. Call before [`LayoutState::restore`].
    pub fn set_split(&mut self, tws: &[ToolWindowId]) {
        self.split = tws.iter().copied().collect();
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
        let dock = self.dock_of(tw);
        let i = self.order.iter().position(|t| *t == tw)?;
        let same = |k: &usize| self.dock_of(self.order[*k]) == dock;
        // Bottom buttons are drawn bottom to top, so up is the next one in `order`.
        let k = if up == (dock.side != Side::Bottom) { (0..i).rev().find(same) } else { (i + 1..self.order.len()).find(same) };
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

    /// Reorder for a dropped strip button and return the dock `tw` must move
    /// to (`None`: nothing to do). Dropping onto a button takes its place and
    /// its dock, so a reorder inside a split group keeps the split.
    pub fn drop_button(&mut self, tw: ToolWindowId, drop: StripDrop) -> Option<Dock> {
        match drop {
            StripDrop::Onto(b) if b == tw => None,
            StripDrop::Onto(b) => {
                let pos = |t: ToolWindowId| self.order.iter().position(|x| *x == t);
                let after = self.dock_of(b) == self.dock_of(tw) && pos(b) > pos(tw);
                self.put(tw, b, after);
                Some(self.dock_of(b))
            }
            StripDrop::End(dock) => {
                let last = self.order.iter().rev().copied().find(|t| *t != tw && self.dock_of(*t) == dock);
                if let Some(last) = last {
                    self.put(tw, last, true);
                }
                Some(dock)
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

    /// Whether some window's button is on the right strip (Right Top, Right
    /// Bottom, Bottom Right).
    pub fn has_right_strip(&self) -> bool {
        ToolWindowId::ALL.iter().any(|tw| self.on_right_strip(*tw))
    }

    /// Whether `tw`'s button sits on the right strip.
    pub fn on_right_strip(&self, tw: ToolWindowId) -> bool {
        let d = self.dock_of(tw);
        d.side == Side::Right || d == (Dock { side: Side::Bottom, split: true })
    }

    /// A strip's groups: top, split and foot.
    pub fn strip_docks(right: bool) -> [Dock; 3] {
        let d = |side, split| Dock { side, split };
        if right {
            [d(Side::Right, false), d(Side::Right, true), d(Side::Bottom, true)]
        } else {
            [d(Side::Left, false), d(Side::Left, true), d(Side::Bottom, false)]
        }
    }

    /// The first row below the lowest button of the strip's top and split
    /// groups in the last frame.
    pub fn strip_upper_end(&self, right: bool) -> Option<u16> {
        let strip = if right { self.rects.strip_right } else { self.rects.strip };
        let [top, split, _] = Self::strip_docks(right);
        self.strip_hits
            .iter()
            .filter_map(|(b, r)| match b {
                StripButton::Tool(t) if strip.contains(r.as_position()) => {
                    let d = self.dock_of(*t);
                    (d == top || d == split).then(|| r.bottom())
                }
                _ => None,
            })
            .max()
    }

    /// The live slot of a dock.
    fn field(&mut self, dock: Dock) -> &mut Option<ToolWindowId> {
        match (dock.side, dock.split) {
            (Side::Left, false) => &mut self.left,
            (Side::Left, true) => &mut self.left2,
            (Side::Bottom, false) => &mut self.bottom,
            (Side::Bottom, true) => &mut self.bottom2,
            (Side::Right, false) => &mut self.right,
            (Side::Right, true) => &mut self.right2,
        }
    }

    /// The slot of a dock; while zen is on, the one zen puts back.
    fn slot_mut(&mut self, dock: Dock) -> &mut Option<ToolWindowId> {
        if self.zen.is_none() {
            return self.field(dock);
        }
        let z = self.zen.as_mut().expect("zen is on");
        match (dock.side, dock.split) {
            (Side::Left, false) => &mut z.left,
            (Side::Left, true) => &mut z.left2,
            (Side::Bottom, false) => &mut z.bottom,
            (Side::Bottom, true) => &mut z.bottom2,
            (Side::Right, false) => &mut z.right,
            (Side::Right, true) => &mut z.right2,
        }
    }

    /// Put `tw` on the first pane of `side`; see [`LayoutState::move_to_dock`].
    pub fn move_to(&mut self, tw: ToolWindowId, side: Side) {
        self.move_to_dock(tw, Dock { side, split: false });
    }

    /// Put `tw` in `dock`. A shown window stays shown there and hides the
    /// window that slot showed; a hidden one only changes dock. In zen mode
    /// the windows zen puts back are moved.
    pub fn move_to_dock(&mut self, tw: ToolWindowId, dock: Dock) {
        let from = self.dock_of(tw);
        if from == dock {
            return;
        }
        let slot = self.slot_mut(from);
        let shown = *slot == Some(tw);
        if shown {
            *slot = None;
        }
        if dock.side == tw.default_side() {
            self.sides.remove(&tw);
        } else {
            self.sides.insert(tw, dock.side);
        }
        if dock.split {
            self.split.insert(tw);
        } else {
            self.split.remove(&tw);
        }
        if shown {
            *self.slot_mut(dock) = Some(tw);
        }
    }

    /// Restore Default Layout: every window back on its default dock (a
    /// shown one stays shown there, as `move_to`), the default strip order,
    /// nothing maximized. Sizes and zen are kept.
    pub fn restore_default(&mut self) {
        for tw in ToolWindowId::ALL {
            if self.dock_of(tw) != tw.default_dock() {
                self.move_to_dock(tw, tw.default_dock());
            }
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
        if mode == ViewMode::Undocked && self.is_visible(tw) {
            self.keep_one_overlay(tw);
        }
    }

    /// The shown Undocked window, if any (the first in `shown()` order).
    pub fn overlay_window(&self) -> Option<ToolWindowId> {
        self.shown().into_iter().find(|tw| self.view_mode(*tw) == ViewMode::Undocked)
    }

    /// Hide every shown Undocked window except `keep`.
    fn keep_one_overlay(&mut self, keep: ToolWindowId) {
        for tw in self.shown() {
            if tw != keep && self.view_mode(tw) == ViewMode::Undocked {
                self.hide(tw);
            }
        }
    }

    /// Unpinned windows in `ToolWindowId::ALL` order (for the session).
    pub fn unpinned(&self) -> Vec<ToolWindowId> {
        ToolWindowId::ALL.into_iter().filter(|tw| self.view_mode(*tw) == ViewMode::Unpinned).collect()
    }

    /// Replace the Unpinned windows (session restore); other modes stay.
    pub fn set_unpinned(&mut self, tws: &[ToolWindowId]) {
        self.mode.retain(|_, m| *m != ViewMode::Unpinned);
        self.mode.extend(tws.iter().map(|tw| (*tw, ViewMode::Unpinned)));
    }

    /// Undocked windows in `ToolWindowId::ALL` order (for the session).
    pub fn undocked(&self) -> Vec<ToolWindowId> {
        ToolWindowId::ALL.into_iter().filter(|tw| self.view_mode(*tw) == ViewMode::Undocked).collect()
    }

    /// Replace the Undocked windows (session restore); other modes stay.
    /// Call after `set_unpinned` and the slot restore.
    pub fn set_undocked(&mut self, tws: &[ToolWindowId]) {
        self.mode.retain(|_, m| *m != ViewMode::Undocked);
        self.mode.extend(tws.iter().map(|tw| (*tw, ViewMode::Undocked)));
        if let Some(keep) = self.overlay_window() {
            self.keep_one_overlay(keep);
        }
    }

    /// Hide the shown Unpinned windows once focus has moved since the last
    /// frame and is not on them, and the shown Undocked window when focus
    /// moved to another tool window (moving to the editor keeps it). Overlays
    /// do not move focus, so they keep them.
    pub fn keep_unpinned(&mut self, focus: &Focus) {
        if focus.moves == self.seen_moves {
            return;
        }
        self.seen_moves = focus.moves;
        for tw in self.shown() {
            let away = match self.view_mode(tw) {
                ViewMode::Pinned => false,
                ViewMode::Unpinned => tw.panel() != focus.current,
                ViewMode::Undocked => focus.current.tool_window().is_some_and(|f| f != tw),
            };
            if away {
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
        *self.field(self.dock_of(tw)) = Some(tw);
        if self.view_mode(tw) == ViewMode::Undocked {
            self.keep_one_overlay(tw);
        }
    }

    pub fn hide(&mut self, tw: ToolWindowId) {
        let slot = self.field(self.dock_of(tw));
        if *slot == Some(tw) {
            *slot = None;
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
        let primary = |side| Dock { side, split: false };
        let left = left.filter(|tw| self.dock_of(*tw) == primary(Side::Left));
        let bottom = bottom.filter(|tw| self.dock_of(*tw) == primary(Side::Bottom));
        let right = right.filter(|tw| self.dock_of(*tw) == primary(Side::Right));
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

    /// Apply the saved secondary slots and sizes, right after
    /// [`LayoutState::restore`]. A window whose dock is not that slot's is
    /// dropped; a missing size is half the side (`0`), any other is clamped.
    /// In zen mode the windows become the ones zen puts back.
    pub fn restore_split(
        &mut self,
        left2: Option<ToolWindowId>,
        bottom2: Option<ToolWindowId>,
        right2: Option<ToolWindowId>,
        left2_h: Option<u16>,
        bottom2_w: Option<u16>,
        right2_h: Option<u16>,
    ) {
        let secondary = |side| Dock { side, split: true };
        let left2 = left2.filter(|tw| self.dock_of(*tw) == secondary(Side::Left));
        let bottom2 = bottom2.filter(|tw| self.dock_of(*tw) == secondary(Side::Bottom));
        let right2 = right2.filter(|tw| self.dock_of(*tw) == secondary(Side::Right));
        match &mut self.zen {
            Some(z) => (z.left2, z.bottom2, z.right2) = (left2, bottom2, right2),
            None => (self.left2, self.bottom2, self.right2) = (left2, bottom2, right2),
        }
        let size = |v: Option<u16>, min| match v {
            None | Some(0) => 0,
            Some(v) => v.clamp(min, MAX_RESTORED),
        };
        self.left2_h = size(left2_h, MIN_SPLIT_H);
        self.bottom2_w = size(bottom2_w, MIN_LEFT_W);
        self.right2_h = size(right2_h, MIN_SPLIT_H);
    }

    pub fn hide_all(&mut self) {
        for dock in Dock::ALL {
            *self.field(dock) = None;
        }
        self.maximized = None;
    }

    /// Hide everything but the editor text, remembering the tool windows
    /// and `focus` for [`LayoutState::leave_zen`].
    pub fn enter_zen(&mut self, focus: PanelId) {
        if self.zen.is_none() {
            self.zen = Some(ZenSaved {
                left: self.left,
                bottom: self.bottom,
                right: self.right,
                left2: self.left2,
                bottom2: self.bottom2,
                right2: self.right2,
                focus,
            });
            self.hide_all();
        }
    }

    /// Put the pre-zen tool windows back; returns the focus to restore, or
    /// `None` when not in zen mode.
    pub fn leave_zen(&mut self) -> Option<PanelId> {
        let z = self.zen.take()?;
        (self.left, self.bottom, self.right) = (z.left, z.bottom, z.right);
        (self.left2, self.bottom2, self.right2) = (z.left2, z.bottom2, z.right2);
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

    /// `(left2, bottom2, right2)` of [`LayoutState::saved_windows`].
    pub fn saved_split_windows(&self) -> (Option<ToolWindowId>, Option<ToolWindowId>, Option<ToolWindowId>) {
        match self.zen {
            Some(z) => (z.left2, z.bottom2, z.right2),
            None => (self.left2, self.bottom2, self.right2),
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
            *r.of_mut(self.dock_of(tw)) = content;
            r.tabs = Rect::new(content.x, content.y, 0, 0);
            r.editor = Rect::new(content.x, content.y, 0, 0);
            self.rects = r;
            return;
        }

        // The Undocked window's slot is not carved; it slides over the editor.
        let ov = self.overlay_window();
        let carved = |s: Option<ToolWindowId>| s.filter(|t| Some(*t) != ov);
        let (left, left2) = (carved(self.left), carved(self.left2));
        let (bottom, bottom2) = (carved(self.bottom), carved(self.bottom2));
        let (right, right2) = (carved(self.right), carved(self.right2));

        // Bottom tool window across the full content width.
        let mut upper = content;
        if bottom.is_some() || bottom2.is_some() {
            let max_h = content.height.saturating_sub(MIN_CENTER_H + 1);
            self.bottom_h = self.bottom_h.clamp(MIN_BOTTOM_H.min(max_h.max(1)), max_h.max(1));
            if content.height > self.bottom_h + 1 {
                r.bottom = Rect::new(content.x, content.bottom() - self.bottom_h, content.width, self.bottom_h);
                let split = Rect::new(content.x, r.bottom.y - 1, content.width, 1);
                r.splitters.push((SplitterId::Bottom, split));
                let (a, b, sp) = split_side(r.bottom, bottom.is_some(), bottom2.is_some(), self.bottom2_w, true);
                (r.bottom, r.bottom2) = (a, b);
                r.splitters.extend(sp.map(|sp| (SplitterId::BottomSplit, sp)));
                upper = Rect::new(content.x, content.y, content.width, content.height - self.bottom_h - 1);
            }
        }

        let mut center = upper;
        if left.is_some() || left2.is_some() {
            let max_w = upper.width.saturating_sub(MIN_CENTER_W + 1);
            self.left_w = self.left_w.clamp(MIN_LEFT_W.min(max_w.max(1)), max_w.max(1));
            if upper.width > self.left_w + 1 {
                r.left = Rect::new(upper.x, upper.y, self.left_w, upper.height);
                let split = Rect::new(upper.x + self.left_w, upper.y, 1, upper.height);
                r.splitters.push((SplitterId::Left, split));
                let (a, b, sp) = split_side(r.left, left.is_some(), left2.is_some(), self.left2_h, false);
                (r.left, r.left2) = (a, b);
                r.splitters.extend(sp.map(|sp| (SplitterId::LeftSplit, sp)));
                center = Rect::new(split.x + 1, upper.y, upper.width - self.left_w - 1, upper.height);
            }
        }
        if right.is_some() || right2.is_some() {
            // No room beside the left window and the editor's minimum: not drawn.
            let max_w = center.width.saturating_sub(MIN_CENTER_W + 1);
            if max_w > 0 {
                self.right_w = self.right_w.clamp(MIN_LEFT_W.min(max_w), max_w);
            }
            if max_w > 0 && center.width > self.right_w + 1 {
                r.right = Rect::new(center.right() - self.right_w, center.y, self.right_w, center.height);
                let split = Rect::new(r.right.x - 1, center.y, 1, center.height);
                r.splitters.push((SplitterId::Right, split));
                let (a, b, sp) = split_side(r.right, right.is_some(), right2.is_some(), self.right2_h, false);
                (r.right, r.right2) = (a, b);
                r.splitters.extend(sp.map(|sp| (SplitterId::RightSplit, sp)));
                center = Rect::new(center.x, center.y, center.width - self.right_w - 1, center.height);
            }
        }
        r.tabs = Rect::new(center.x, center.y, center.width, 1.min(center.height));
        r.editor = Rect::new(center.x, center.y + r.tabs.height, center.width, center.height.saturating_sub(r.tabs.height));
        if let Some(tw) = ov {
            r.overlay = overlay_rect(self.dock_of(tw).side, center, self.left_w, self.bottom_h, self.right_w);
        }
        self.rects = r;
    }

    pub fn hit_test(&self, x: u16, y: u16) -> Hit {
        let p = ratatui::layout::Position::new(x, y);
        if self.rects.overlay.contains(p)
            && let Some(tw) = self.overlay_window()
        {
            return Hit::Panel(tw.panel(), self.rects.overlay);
        }
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
        for dock in Dock::ALL {
            if let Some(tw) = self.slot(dock)
                && self.rects.of(dock).contains(p)
            {
                return Hit::Panel(tw.panel(), self.rects.of(dock));
            }
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
            SplitterId::LeftSplit => self.left2_h = self.rects.left2.bottom().saturating_sub(y + 1).max(MIN_SPLIT_H),
            SplitterId::RightSplit => self.right2_h = self.rects.right2.bottom().saturating_sub(y + 1).max(MIN_SPLIT_H),
            SplitterId::BottomSplit => self.bottom2_w = self.rects.bottom2.right().saturating_sub(x + 1).max(MIN_LEFT_W),
        }
    }

    /// Keyboard resize: move a splitter by `delta` cells, bounded below like
    /// a drag and then fitted to the terminal the way a frame would.
    pub fn resize(&mut self, id: SplitterId, delta: i16) {
        match id {
            SplitterId::Left => self.left_w = self.left_w.saturating_add_signed(delta).max(MIN_LEFT_W),
            SplitterId::Bottom => self.bottom_h = self.bottom_h.saturating_add_signed(delta).max(MIN_BOTTOM_H),
            SplitterId::Right => self.right_w = self.right_w.saturating_add_signed(delta).max(MIN_LEFT_W),
            SplitterId::LeftSplit => {
                let cur = if self.left2_h == 0 { self.rects.left2.height } else { self.left2_h };
                self.left2_h = cur.saturating_add_signed(delta).max(MIN_SPLIT_H);
            }
            SplitterId::RightSplit => {
                let cur = if self.right2_h == 0 { self.rects.right2.height } else { self.right2_h };
                self.right2_h = cur.saturating_add_signed(delta).max(MIN_SPLIT_H);
            }
            SplitterId::BottomSplit => {
                let cur = if self.bottom2_w == 0 { self.rects.bottom2.width } else { self.bottom2_w };
                self.bottom2_w = cur.saturating_add_signed(delta).max(MIN_LEFT_W);
            }
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
    /// frame's strip: a button is `Onto`. A row among the top and split
    /// groups' buttons drops onto the nearest of them, a row among the foot
    /// group's buttons onto the nearest foot button. The free gap between
    /// them is halved: its upper half is the end of the split group (of the
    /// top group when those have no button), its lower half the end of the
    /// foot group. A row equally far from two buttons, such as the separator
    /// between the top and split groups, goes to the one in the dragged
    /// window's dock, else to the upper one.
    pub fn strip_drop_at(&self, x: u16, y: u16, dragged: Option<ToolWindowId>) -> Option<StripDrop> {
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
        let [top, split, foot] = Self::strip_docks(in_right);
        let tools: Vec<(ToolWindowId, Rect)> = self
            .strip_hits
            .iter()
            .filter_map(|(b, r)| match b {
                StripButton::Tool(t) if strip.contains(r.as_position()) => Some((*t, *r)),
                _ => None,
            })
            .collect();
        let upper: Vec<(ToolWindowId, Rect)> = tools.iter().copied().filter(|(t, _)| [top, split].contains(&self.dock_of(*t))).collect();
        let lower: Vec<(ToolWindowId, Rect)> = tools.iter().copied().filter(|(t, _)| self.dock_of(*t) == foot).collect();
        let u_end = self.strip_upper_end(in_right).unwrap_or(strip.y);
        let f_top = lower.iter().map(|(_, r)| r.y).min().unwrap_or_else(|| strip.bottom());
        let dist = |r: &Rect| if y < r.y { r.y - y } else { y.saturating_sub(r.bottom() - 1) };
        let own = dragged.map(|t| self.dock_of(t));
        let nearest = |list: &[(ToolWindowId, Rect)]| {
            list.iter().min_by_key(|(t, r)| (dist(r), Some(self.dock_of(*t)) != own, r.y)).map(|(t, _)| StripDrop::Onto(*t))
        };
        if y < u_end {
            return nearest(&upper);
        }
        if y >= f_top {
            return nearest(&lower);
        }
        let g_mid = u_end + (f_top - u_end) / 2;
        Some(StripDrop::End(if y >= g_mid {
            foot
        } else if upper.is_empty() {
            top
        } else {
            split
        }))
    }

    /// The windows of the left strip (`right == false`) or the right strip
    /// that got no button this frame, in strip order: the More menu's rows.
    pub fn strip_overflow(&self, right: bool) -> Vec<ToolWindowId> {
        self.order
            .iter()
            .copied()
            .filter(|tw| self.on_right_strip(*tw) == right)
            .filter(|tw| !self.strip_hits.iter().any(|(b, _)| *b == StripButton::Tool(*tw)))
            .collect()
    }
}

/// The slide-over rect of a side inside the editor area (tabs row included);
/// the sizes are the docked ones, cut to the area.
fn overlay_rect(side: Side, center: Rect, left_w: u16, bottom_h: u16, right_w: u16) -> Rect {
    match side {
        Side::Left => Rect::new(center.x, center.y, left_w.min(center.width), center.height),
        Side::Right => {
            let w = right_w.min(center.width);
            Rect::new(center.right() - w, center.y, w, center.height)
        }
        Side::Bottom => {
            let h = bottom_h.min(center.height);
            Rect::new(center.x, center.bottom() - h, center.width, h)
        }
    }
}

/// Divide a side's rect between its two panes: `(primary, secondary,
/// splitter)`. A lone pane takes the whole rect. A secondary under the
/// minimum is not drawn. `size` is the secondary's cells, `0` half.
fn split_side(r: Rect, primary: bool, secondary: bool, size: u16, across: bool) -> (Rect, Rect, Option<Rect>) {
    if !secondary {
        return (r, Rect::default(), None);
    }
    if !primary {
        return (Rect::default(), r, None);
    }
    let (len, min) = if across { (r.width, MIN_LEFT_W) } else { (r.height, MIN_SPLIT_H) };
    let max = len.saturating_sub(1 + min);
    let s = if size == 0 { len.saturating_sub(1) / 2 } else { size }.min(max);
    if s < min {
        return (r, Rect::default(), None);
    }
    if across {
        (
            Rect::new(r.x, r.y, len - s - 1, r.height),
            Rect::new(r.right() - s, r.y, s, r.height),
            Some(Rect::new(r.x + len - s - 1, r.y, 1, r.height)),
        )
    } else {
        (
            Rect::new(r.x, r.y, r.width, len - s - 1),
            Rect::new(r.x, r.bottom() - s, r.width, s),
            Some(Rect::new(r.x, r.y + len - s - 1, r.width, 1)),
        )
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

    use ToolWindowId::{Find, Git, Project, Structure};

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
        let want =
            [Git, Project, Worktrees, Structure, Find, Run, Problems, Terminal, EventLog, LocalHistory, Tasks, Todo, Hierarchy, Debug];
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
        assert!(!l.can_move_button(Debug, true), "Debug is the highest bottom button");
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
        assert_eq!(l.drop_button(Find, StripDrop::Onto(Problems)), Some(dock(Side::Bottom, false)));
        assert_eq!(&bottom(&l)[..3], [Run, Problems, Find]);
        assert_eq!(l.drop_button(Terminal, StripDrop::Onto(Find)), Some(dock(Side::Bottom, false)));
        assert_eq!(&bottom(&l)[..4], [Run, Problems, Terminal, Find]);
        assert_eq!(l.drop_button(Git, StripDrop::Onto(Worktrees)), Some(dock(Side::Left, false)));
        let o = l.order();
        assert_eq!(o.iter().position(|t| *t == Git).unwrap() + 1, o.iter().position(|t| *t == Worktrees).unwrap());
        assert_eq!(l.side_of(Git), Side::Bottom, "drop_button only reorders");
        let before = l.order().to_vec();
        assert_eq!(l.drop_button(Run, StripDrop::End(dock(Side::Right, false))), Some(dock(Side::Right, false)));
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
        assert_eq!(l.strip_drop_at(1, 2, None), Some(StripDrop::Onto(Project)));
        assert_eq!(l.strip_drop_at(1, 0, None), None);
        assert_eq!(l.strip_drop_at(1, 10, None), Some(StripDrop::End(dock(Side::Left, true))));
        assert_eq!(l.strip_drop_at(1, 30, None), Some(StripDrop::End(dock(Side::Bottom, false))));
        assert_eq!(l.strip_drop_at(1, 38, None), Some(StripDrop::Onto(Find)));
        assert_eq!(l.strip_drop_at(60, 10, None), None);
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1)), (StripButton::Tool(Find), Rect::new(0, 37, 3, 2))];
        assert_eq!(l.strip_drop_at(1, 5, None), Some(StripDrop::End(dock(Side::Left, false))));
        assert_eq!(l.strip_drop_at(1, 30, None), Some(StripDrop::End(dock(Side::Bottom, false))));
        // No Bottom button: the lower half of the gap is the Bottom end.
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1)), (StripButton::Tool(Project), Rect::new(0, 2, 3, 2))];
        assert_eq!(l.strip_drop_at(1, 30, None), Some(StripDrop::End(dock(Side::Bottom, false))));
        // The right strip's free row is the Right end.
        l.move_to(Hierarchy, Side::Right);
        l.compute(Rect::new(0, 0, 120, 40));
        let sr = l.rects.strip_right;
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1))];
        assert_eq!(l.strip_drop_at(sr.x + 1, sr.y + 5, None), Some(StripDrop::End(dock(Side::Right, false))));
        // The right strip is drawn only while a window is on the right, so a
        // hidden one never yields the Right end, whatever the point.
        let mut l = LayoutState::new(30, 10);
        l.compute(Rect::new(0, 0, 120, 40));
        l.strip_hits = vec![(StripButton::Menu, Rect::new(0, 0, 3, 1))];
        assert!(!l.has_right_strip());
        assert!(
            (0..120).all(|x| (0..40).all(|y| !matches!(l.strip_drop_at(x, y, None), Some(StripDrop::End(d)) if d.side == Side::Right)))
        );
        // A More cell is not a drop target.
        l.strip_hits = vec![(StripButton::More, Rect::new(0, 38, 3, 1))];
        assert_eq!(l.strip_drop_at(1, 38, None), None);
    }

    fn dock(side: Side, split: bool) -> Dock {
        Dock { side, split }
    }

    #[test]
    fn drop_button_keeps_a_split_group() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.move_to_dock(Project, dock(Side::Left, true));
        l.show(Structure);
        assert_eq!(l.drop_button(Structure, StripDrop::Onto(Project)), Some(dock(Side::Left, true)));
        l.move_to_dock(Structure, dock(Side::Left, true));
        assert_eq!(l.left2, Some(Structure));
        assert!(l.split_windows().contains(&Structure));
        assert_eq!(l.drop_button(Worktrees, StripDrop::End(dock(Side::Left, true))), Some(dock(Side::Left, true)));
        let pos = |t| l.order().iter().position(|x| *x == t).unwrap();
        assert!(pos(Worktrees) > pos(Project) && pos(Worktrees) > pos(Structure));
    }

    #[test]
    fn strip_drop_at_uses_groups() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.compute(Rect::new(0, 0, 120, 40));
        l.strip_hits = vec![
            (StripButton::Menu, Rect::new(0, 0, 3, 1)),
            (StripButton::Tool(Project), Rect::new(0, 2, 3, 2)),
            (StripButton::Tool(Structure), Rect::new(0, 6, 3, 2)),
            (StripButton::Tool(Find), Rect::new(0, 37, 3, 2)),
        ];
        assert_eq!(l.strip_drop_at(1, 5, None), Some(StripDrop::Onto(Structure)));
        assert_eq!(l.strip_drop_at(1, 9, None), Some(StripDrop::End(dock(Side::Left, true))));
        assert_eq!(l.strip_drop_at(1, 30, None), Some(StripDrop::End(dock(Side::Bottom, false))));
        assert_eq!(l.strip_drop_at(1, 38, None), Some(StripDrop::Onto(Find)));
        l.move_to_dock(Git, dock(Side::Right, false));
        l.move_to_dock(Problems, dock(Side::Bottom, true));
        l.compute(Rect::new(0, 0, 120, 40));
        let sr = l.rects.strip_right;
        l.strip_hits = vec![
            (StripButton::Tool(Git), Rect::new(sr.x, sr.y + 2, 3, 2)),
            (StripButton::Tool(Problems), Rect::new(sr.x, sr.bottom() - 2, 3, 2)),
        ];
        assert_eq!(l.strip_drop_at(sr.x + 1, sr.y + 6, None), Some(StripDrop::End(dock(Side::Right, true))));
        assert_eq!(l.strip_drop_at(sr.x + 1, sr.bottom() - 6, None), Some(StripDrop::End(dock(Side::Bottom, true))));
    }

    #[test]
    fn strip_drop_at_breaks_a_tie_toward_the_dragged_dock() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.compute(Rect::new(0, 0, 120, 40));
        l.strip_hits = vec![
            (StripButton::Tool(Project), Rect::new(0, 2, 3, 2)),
            (StripButton::Tool(Structure), Rect::new(0, 5, 3, 2)),
            (StripButton::Tool(Find), Rect::new(0, 37, 3, 2)),
        ];
        // Row 4 is the separator, one row from each neighbour.
        assert_eq!(l.strip_drop_at(1, 4, None), Some(StripDrop::Onto(Project)));
        assert_eq!(l.strip_drop_at(1, 4, Some(Structure)), Some(StripDrop::Onto(Structure)));
        assert_eq!(l.strip_drop_at(1, 4, Some(Project)), Some(StripDrop::Onto(Project)));
        assert_eq!(l.strip_drop_at(1, 4, Some(Find)), Some(StripDrop::Onto(Project)));
        // Not a tie: the nearer button wins whatever is dragged.
        l.strip_hits[1].1 = Rect::new(0, 8, 3, 2);
        assert_eq!(l.strip_drop_at(1, 5, Some(Structure)), Some(StripDrop::Onto(Project)));
        // The right strip.
        l.move_to_dock(Git, dock(Side::Right, false));
        l.move_to_dock(Problems, dock(Side::Right, true));
        l.compute(Rect::new(0, 0, 120, 40));
        let sr = l.rects.strip_right;
        l.strip_hits =
            vec![(StripButton::Tool(Git), Rect::new(sr.x, sr.y + 2, 3, 2)), (StripButton::Tool(Problems), Rect::new(sr.x, sr.y + 5, 3, 2))];
        assert_eq!(l.strip_drop_at(sr.x + 1, sr.y + 4, Some(Problems)), Some(StripDrop::Onto(Problems)));
        assert_eq!(l.strip_drop_at(sr.x + 1, sr.y + 4, Some(Git)), Some(StripDrop::Onto(Git)));
    }

    #[test]
    fn right_strip_follows_bottom_right() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        assert!(!l.has_right_strip());
        l.move_to_dock(Problems, dock(Side::Bottom, true));
        assert!(l.has_right_strip() && l.on_right_strip(Problems));
        assert!(l.strip_overflow(true).contains(&Problems));
        assert!(!l.strip_overflow(false).contains(&Problems));
    }

    #[test]
    fn move_button_stays_in_its_group() {
        use ToolWindowId::*;
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        assert!(!l.can_move_button(Structure, true));
        assert!(!l.can_move_button(Worktrees, false));
    }

    /// Project/Structure left, Find/Problems bottom, Git/EventLog right.
    fn split_everywhere() -> LayoutState {
        use ToolWindowId::{EventLog, Find, Git, Problems, Structure};
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.show(Structure);
        l.show(Find);
        l.move_to_dock(Problems, dock(Side::Bottom, true));
        l.show(Problems);
        l.move_to(Git, Side::Right);
        l.show(Git);
        l.move_to_dock(EventLog, dock(Side::Right, true));
        l.show(EventLog);
        l
    }

    #[test]
    fn move_to_dock_tracks_side_and_split() {
        use ToolWindowId::{Git, Project, Structure};
        let mut l = LayoutState::new(30, 10);
        assert_eq!(l.dock_of(Git), Git.default_dock());
        l.show(Git);
        l.move_to_dock(Git, dock(Side::Bottom, true));
        assert_eq!((l.bottom, l.bottom2), (None, Some(Git)));
        assert_eq!(l.side_of(Git), Side::Bottom);
        assert!(l.side_overrides().is_empty());
        assert_eq!(l.split_windows(), [Git]);
        assert!(l.is_visible(Git));
        l.move_to(Git, Side::Bottom);
        assert_eq!((l.bottom, l.bottom2), (Some(Git), None));
        assert!(l.split_windows().is_empty());
        l.move_to_dock(Git, dock(Side::Right, true));
        assert_eq!(l.right2, Some(Git));
        assert_eq!(l.side_overrides(), [(Git, Side::Right)]);
        assert!(l.has_right_strip());
        l.hide(Git);
        assert_eq!(l.right2, None);
        l.move_to_dock(Structure, dock(Side::Left, true));
        assert_eq!(l.dock_of(Structure), dock(Side::Left, true));
        assert_eq!(l.shown(), [Project]);
    }

    #[test]
    fn compute_splits_each_side() {
        use ToolWindowId::{EventLog, Find, Git, Problems, Structure};
        let mut l = split_everywhere();
        let area = Rect::new(0, 0, 120, 40);
        l.compute(area);
        let r = l.rects.clone();
        assert_eq!(r.left, Rect::new(3, 0, 30, 14));
        assert_eq!(r.left2, Rect::new(3, 15, 30, 13));
        assert_eq!(r.bottom, Rect::new(3, 29, 57, 10));
        assert_eq!(r.bottom2, Rect::new(61, 29, 56, 10));
        assert_eq!(r.right, Rect::new(87, 0, 30, 14));
        assert_eq!(r.right2, Rect::new(87, 15, 30, 13));
        for (id, rect) in [
            (SplitterId::LeftSplit, Rect::new(3, 14, 30, 1)),
            (SplitterId::BottomSplit, Rect::new(60, 29, 1, 10)),
            (SplitterId::RightSplit, Rect::new(87, 14, 30, 1)),
        ] {
            assert!(r.splitters.contains(&(id, rect)), "{id:?}");
            assert_eq!(l.hit_test(rect.x, rect.y), Hit::Splitter(id));
        }
        for (tw, rect) in [
            (ToolWindowId::Project, r.left),
            (Structure, r.left2),
            (Find, r.bottom),
            (Problems, r.bottom2),
            (Git, r.right),
            (EventLog, r.right2),
        ] {
            assert_eq!(l.hit_test(rect.x, rect.y), Hit::Panel(tw.panel(), rect));
        }
        let mut plain = LayoutState::new(30, 10);
        plain.show(Find);
        plain.move_to(Git, Side::Right);
        plain.show(Git);
        plain.compute(area);
        assert_eq!(plain.rects.editor, r.editor);
    }

    #[test]
    fn secondary_pane_drops_when_too_small_and_fills_alone() {
        use ToolWindowId::{Git, Project, Structure};
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.show(Structure);
        l.show(Git);
        l.compute(Rect::new(0, 0, 80, 12));
        assert!(l.rects.left2.is_empty());
        assert_eq!(l.rects.left.height, 5);
        assert!(l.rects.splitters.iter().all(|(id, _)| *id != SplitterId::LeftSplit));
        l.hide(Project);
        l.compute(Rect::new(0, 0, 80, 12));
        assert!(l.rects.left.is_empty());
        assert_eq!(l.rects.left2, Rect::new(3, 0, 30, 5));
        assert!(matches!(l.hit_test(4, 1), Hit::Panel(PanelId::Structure, _)));
    }

    #[test]
    fn split_splitters_drag_and_resize() {
        let mut l = split_everywhere();
        let area = Rect::new(0, 0, 120, 40);
        l.compute(area);
        l.drag_splitter(SplitterId::LeftSplit, 5, 10);
        assert_eq!(l.left2_h, 17);
        l.compute(area);
        assert_eq!(l.rects.left2.height, 17);
        l.drag_splitter(SplitterId::BottomSplit, 80, 30);
        assert_eq!(l.bottom2_w, 36);
        l.drag_splitter(SplitterId::RightSplit, 90, 27);
        assert_eq!(l.right2_h, MIN_SPLIT_H);
        l.resize(SplitterId::LeftSplit, -100);
        assert_eq!(l.left2_h, MIN_SPLIT_H);
        l.bottom2_w = 0;
        l.compute(area);
        assert_eq!(l.rects.bottom2.width, 56);
        l.resize(SplitterId::BottomSplit, 2);
        assert_eq!(l.bottom2_w, 58);
    }

    #[test]
    fn zen_maximize_hide_all_and_unpinned_cover_secondaries() {
        use ToolWindowId::Structure;
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.show(Structure);
        l.hide(ToolWindowId::Project);
        l.enter_zen(PanelId::Editor);
        assert_eq!(l.left2, None);
        assert_eq!(l.saved_split_windows(), (Some(Structure), None, None));
        l.move_to_dock(Structure, dock(Side::Bottom, true));
        l.leave_zen();
        assert_eq!(l.bottom2, Some(Structure));
        l.toggle_maximize(Structure);
        l.compute(Rect::new(0, 0, 100, 30));
        // Bottom Right puts its button on a right strip, which takes 3 columns.
        assert_eq!(l.rects.bottom2, Rect::new(STRIP_W, 0, 94, 29));
        l.maximized = None;
        l.set_view_mode(Structure, ViewMode::Unpinned);
        let mut focus = Focus::new();
        focus.set(PanelId::Structure);
        l.keep_unpinned(&focus);
        assert!(l.is_visible(Structure));
        focus.set(PanelId::Editor);
        l.keep_unpinned(&focus);
        assert!(!l.is_visible(Structure));
        l.show(Structure);
        l.show(ToolWindowId::Project);
        l.hide_all();
        assert!(l.shown().is_empty());
    }

    #[test]
    fn restore_default_and_restore_split_use_docks() {
        use ToolWindowId::{Git, Problems, Structure};
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.show(Structure);
        l.move_to_dock(Problems, dock(Side::Bottom, true));
        l.restore_default();
        assert!(l.split_windows().is_empty());
        assert_eq!(l.left, Some(Structure));
        assert_eq!(l.dock_of(Problems), Problems.default_dock());
        l.set_split(&[Structure]);
        l.restore(Some(Structure), 30, None, 10, None, None);
        assert_eq!(l.left, None);
        l.restore_split(Some(Structure), Some(Git), None, Some(0), Some(1), None);
        assert_eq!(l.left2, Some(Structure));
        assert_eq!(l.bottom2, None);
        assert_eq!((l.left2_h, l.bottom2_w, l.right2_h), (0, MIN_LEFT_W, 0));
    }

    #[test]
    fn undocked_window_overlays_the_editor() {
        let area = Rect::new(0, 0, 120, 40);
        let mut l = LayoutState::new(30, 10);
        l.move_to(Git, Side::Right);
        l.hide(Git);
        l.compute(area);
        let before = l.rects.clone();
        assert!(before.right.is_empty());
        l.set_view_mode(Git, ViewMode::Undocked);
        l.show(Git);
        l.compute(area);
        assert_eq!((l.rects.editor, l.rects.tabs), (before.editor, before.tabs));
        assert!(l.rects.right.is_empty());
        assert!(l.rects.splitters.iter().all(|(id, _)| *id != SplitterId::Right));
        let ov = Rect::new(before.tabs.right() - 30, before.tabs.y, 30, before.tabs.height + before.editor.height);
        assert_eq!(l.rects.overlay, ov);
        assert_eq!(l.hit_test(ov.x, ov.y + 1), Hit::Panel(PanelId::Git, ov));
        assert!(matches!(l.hit_test(ov.x - 1, ov.y + 1), Hit::Panel(PanelId::Editor, _)));

        // Bottom: the last rows of the editor area, full width.
        let mut l = LayoutState::new(30, 10);
        l.set_view_mode(Find, ViewMode::Undocked);
        l.show(Find);
        l.compute(area);
        let ov = l.rects.overlay;
        assert_eq!((ov.height, ov.width, ov.bottom()), (10, l.rects.editor.width, l.rects.editor.bottom()));
        assert_eq!(ov.x, l.rects.tabs.x);

        // Left: starts at the edge of the editor area.
        let mut l = LayoutState::new(30, 10);
        l.set_view_mode(Project, ViewMode::Undocked);
        l.hide(Git);
        l.compute(area);
        assert_eq!(l.rects.overlay, Rect::new(l.rects.strip.right(), l.rects.tabs.y, 30, l.rects.tabs.height + l.rects.editor.height));
    }

    #[test]
    fn undocked_split_window_takes_the_whole_side_overlay() {
        let area = Rect::new(0, 0, 120, 40);
        let mut l = LayoutState::new(30, 10);
        l.move_to_dock(Structure, dock(Side::Left, true));
        l.set_view_mode(Structure, ViewMode::Undocked);
        l.show(Structure);
        l.compute(area);
        assert!(l.rects.left2.is_empty());
        assert!(l.rects.splitters.iter().all(|(id, _)| *id != SplitterId::LeftSplit));
        assert_eq!(l.rects.left.width, 30);
        assert_eq!(l.rects.overlay.x, l.rects.strip.right() + 30 + 1);
        assert_eq!(l.rects.overlay.width, 30);
    }

    #[test]
    fn one_undocked_window_at_a_time() {
        let mut l = LayoutState::new(30, 10);
        l.move_to(Git, Side::Right);
        l.set_view_mode(Git, ViewMode::Undocked);
        l.set_view_mode(Find, ViewMode::Undocked);
        l.show(Git);
        l.show(Find);
        assert!(!l.is_visible(Git) && l.is_visible(Find));
        l.show(ToolWindowId::Structure);
        l.set_view_mode(ToolWindowId::Structure, ViewMode::Undocked);
        assert!(!l.is_visible(Find) && l.is_visible(ToolWindowId::Structure));
    }

    #[test]
    fn undocked_hides_when_another_tool_window_takes_focus() {
        let mut l = LayoutState::new(30, 10);
        let mut f = Focus::new();
        l.move_to(Git, Side::Right);
        l.show(Git);
        l.set_view_mode(Git, ViewMode::Undocked);
        f.set(PanelId::Git);
        l.keep_unpinned(&f);
        assert!(l.is_visible(Git));
        f.set(PanelId::Editor);
        l.keep_unpinned(&f);
        assert!(l.is_visible(Git));
        f.set(PanelId::Project);
        l.keep_unpinned(&f);
        assert!(!l.is_visible(Git));
    }

    #[test]
    fn maximized_undocked_window_has_no_overlay() {
        let area = Rect::new(0, 0, 120, 40);
        let mut l = LayoutState::new(30, 10);
        l.move_to(Git, Side::Right);
        l.show(Git);
        l.set_view_mode(Git, ViewMode::Undocked);
        l.toggle_maximize(Git);
        l.compute(area);
        assert!(l.rects.overlay.is_empty());
        assert!(!l.rects.of(l.dock_of(Git)).is_empty());
        l.toggle_maximize(Git);
        l.enter_zen(PanelId::Editor);
        l.compute(area);
        assert!(l.rects.overlay.is_empty());
    }

    #[test]
    fn undocked_and_unpinned_lists_are_separate() {
        use ToolWindowId::Problems;
        let mut l = LayoutState::new(30, 10);
        l.set_unpinned(&[Find]);
        l.set_undocked(&[Git]);
        l.set_unpinned(&[Problems]);
        assert_eq!(l.undocked(), vec![Git]);
        assert_eq!(l.unpinned(), vec![Problems]);
        l.restore_default();
        assert!(l.undocked().is_empty() && l.unpinned().is_empty());
    }
}
