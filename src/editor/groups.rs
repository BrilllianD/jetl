//! Tab groups: each pane of the split editor shows its own tabs, each tab
//! a view of a buffer in `Editor::buffers`. One buffer can have a view in
//! several groups (at most one per group).

use ratatui::layout::Rect;

use super::buffer::BufferId;
use super::find::FindHit;
use super::selection::{Selection, line_col_to_pos};
use super::split::{Axis, MIN_GROUP_H, MIN_GROUP_W, SplitNode};
use super::view_state::{Parked, ViewState};
use super::{Editor, nav_pos};
use crate::actions::{Action, EditorAction};
use crate::app::Ctx;
use crate::event::{Effect, Level};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::menu::{ContextMenu, MenuItem};
use crate::ui::tabs::{TabHit, TabHits};
use crate::workspace::{SessionNode, SessionSplits};

pub type GroupId = u32;

/// A tab: a buffer shown in a group.
#[derive(Debug)]
pub struct View {
    pub id: BufferId,
    /// The view's caret, scroll and find bar while another view of the
    /// buffer is live; `None` while this one is (its state is then
    /// `Buffer::view`).
    pub parked: Option<Parked>,
    /// Pinned tabs come first in their group's tab bar and survive close
    /// others, close all but pinned and the tab limit.
    pub pinned: bool,
}

/// One pane of the editor: a tab bar over a text view.
#[derive(Debug)]
pub struct TabGroup {
    pub id: GroupId,
    /// The tabs in bar order; pinned ones first.
    pub views: Vec<View>,
    /// Slot of the shown tab in `views`.
    pub active: Option<usize>,
    /// Buffer ids of this group's tabs, most recently shown first; closing
    /// the shown tab shows the next one here.
    pub mru: Vec<BufferId>,
    /// Tab bar hit rects by slot, as last drawn.
    pub tab_hits: TabHits,
    pub tab_scroll: usize,
    /// The tab bar as last drawn (no rows in zen mode).
    pub tab_area: Rect,
    /// The text cells of the view as last drawn (mouse mapping, page size).
    pub text_area: Rect,
    /// The whole group (tab bar, find bar and text view) as last drawn.
    pub last_area: Rect,
    pub find_area: Rect,
    pub(super) find_hits: Vec<(FindHit, Rect)>,
    /// The screen cell of the caret as last drawn (`None` when it is
    /// scrolled out of view); popups at the caret anchor there.
    pub caret_cell: Option<(u16, u16)>,
}

impl TabGroup {
    pub fn new(id: GroupId) -> Self {
        Self {
            id,
            views: Vec::new(),
            active: None,
            mru: Vec::new(),
            tab_hits: TabHits::default(),
            tab_scroll: 0,
            tab_area: Rect::default(),
            text_area: Rect::default(),
            last_area: Rect::default(),
            find_area: Rect::default(),
            find_hits: Vec::new(),
            caret_cell: None,
        }
    }

    pub fn slot_of(&self, id: BufferId) -> Option<usize> {
        self.views.iter().position(|v| v.id == id)
    }

    /// The buffer of the shown tab.
    pub fn active_id(&self) -> Option<BufferId> {
        self.views.get(self.active?).map(|v| v.id)
    }

    /// Pinned tabs are always the first ones.
    pub fn pinned_count(&self) -> usize {
        self.views.iter().take_while(|v| v.pinned).count()
    }

    /// The slots tab `slot` may move to: pinned tabs stay among the pinned
    /// ones, the others after them.
    pub fn pin_range(&self, slot: usize) -> std::ops::Range<usize> {
        let n = self.pinned_count();
        if self.views.get(slot).is_some_and(|v| v.pinned) { 0..n } else { n..self.views.len() }
    }

    /// Note that buffer `id` was shown here.
    pub fn touch(&mut self, id: BufferId) {
        self.mru.retain(|x| *x != id);
        self.mru.insert(0, id);
    }

    /// Where tab `slot` goes when closed: the most recently shown other
    /// tab, else the one that takes its place.
    pub fn next_after_close(&self, slot: usize) -> Option<usize> {
        let id = self.views.get(slot)?.id;
        let n = self.views.len() - 1;
        if n == 0 {
            return None;
        }
        let pick =
            self.mru.iter().filter(|x| **x != id).find_map(|x| self.slot_of(*x)).unwrap_or(if slot < n { slot + 1 } else { slot - 1 });
        // Slots after the closed one shift down.
        Some(if pick > slot { pick - 1 } else { pick })
    }

    /// Move tab `from` to slot `to` (clamped), keeping the shown tab.
    /// Returns the new slot.
    pub fn move_tab(&mut self, from: usize, to: usize) -> usize {
        let to = to.min(self.views.len().saturating_sub(1));
        if from == to || from >= self.views.len() {
            return from;
        }
        let v = self.views.remove(from);
        self.views.insert(to, v);
        self.active = self.active.map(|a| match a {
            a if a == from => to,
            a if from < a && a <= to => a - 1,
            a if to <= a && a < from => a + 1,
            a => a,
        });
        to
    }
}

/// What is under the pointer in the editor area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorHit {
    /// The bar of the split with this preorder index.
    Splitter(usize),
    Tab(GroupId, TabHit),
    /// A tab bar off the tabs.
    Strip(GroupId),
    /// A group's find bar or text.
    Body(GroupId),
    /// The conflict resolver's header row, on a button or not.
    MergeHeader(Option<crate::actions::MergeAction>),
}

/// Take tab `slot` out of `group`; a shown tab hands over to the next one
/// ([`TabGroup::next_after_close`]).
pub(super) fn remove_view(group: &mut TabGroup, slot: usize) -> View {
    let was_active = group.active == Some(slot);
    let next = group.next_after_close(slot);
    let v = group.views.remove(slot);
    group.mru.retain(|x| *x != v.id);
    group.active = match group.active {
        _ if was_active => next,
        Some(a) if a > slot => Some(a - 1),
        a => a,
    };
    v
}

impl Editor {
    pub(super) fn group_index(&self, g: GroupId) -> Option<usize> {
        self.groups.iter().position(|x| x.id == g)
    }

    pub fn group(&self, g: GroupId) -> Option<&TabGroup> {
        self.groups.iter().find(|x| x.id == g)
    }

    pub(super) fn group_mut(&mut self, g: GroupId) -> Option<&mut TabGroup> {
        self.groups.iter_mut().find(|x| x.id == g)
    }

    /// The group keys go to; it always exists.
    pub fn focused_group(&self) -> &TabGroup {
        self.group(self.focused).expect("focused group exists")
    }

    pub(super) fn focused_group_mut(&mut self) -> &mut TabGroup {
        let g = self.focused;
        self.group_mut(g).expect("focused group exists")
    }

    /// Index in `buffers` of the focused group's shown tab.
    pub fn active(&self) -> Option<usize> {
        self.focused_group().active_id().and_then(|id| self.index_of_id(id))
    }

    /// The focused group's text cells as last drawn.
    pub fn text_area(&self) -> Rect {
        self.focused_group().text_area
    }

    /// The focused group's caret cell as last drawn; popups at the caret
    /// anchor there.
    pub fn caret_cell(&self) -> Option<(u16, u16)> {
        self.focused_group().caret_cell
    }

    /// Number of tabs showing buffer `id`, over all groups.
    pub(super) fn view_count(&self, id: BufferId) -> usize {
        self.groups.iter().filter(|g| g.slot_of(id).is_some()).count()
    }

    /// Buffers with a pinned tab in some group.
    pub fn pinned_ids(&self) -> Vec<BufferId> {
        self.session_layout().0.iter().flat_map(|g| g.views.iter().filter(|v| v.pinned).map(|v| v.id)).collect()
    }

    /// The splitter, tab, tab bar or group body at `x`, `y`, as last drawn.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<EditorHit> {
        let p = ratatui::layout::Position::new(x, y);
        if let Some(a) = self.merge_header_hit(x, y) {
            return Some(EditorHit::MergeHeader(a));
        }
        if let Some(s) = self.splitters.iter().find(|s| s.rect.contains(p)) {
            return Some(EditorHit::Splitter(s.node));
        }
        let g = self.groups.iter().find(|g| g.last_area.contains(p))?;
        Some(match g.tab_hits.hit(x, y) {
            Some(h) => EditorHit::Tab(g.id, h),
            None if g.tab_area.contains(p) => EditorHit::Strip(g.id),
            None => EditorHit::Body(g.id),
        })
    }

    /// A drag of the bar of split `node` to `x`, `y`.
    pub fn drag_splitter(&mut self, node: usize, x: u16, y: u16) {
        if let Some(s) = self.splitters.iter().find(|s| s.node == node) {
            self.tree.set_ratio(node, s.ratio_at(x, y));
        }
    }

    /// Buffer indices in tab order: the groups in layout order, each buffer
    /// once (the layout the resolver put aside while it is open).
    pub fn tab_order(&self) -> Vec<usize> {
        let (groups, tree, _) = self.session_layout();
        let mut out = Vec::new();
        for g in tree.leaves().into_iter().filter_map(|g| groups.iter().find(|x| x.id == g)) {
            for v in &g.views {
                if let Some(i) = self.index_of_id(v.id).filter(|i| !out.contains(i)) {
                    out.push(i);
                }
            }
        }
        out
    }

    /// Show buffer `i` in the focused group, remembering the position left
    /// for back/forward.
    pub(super) fn activate_index(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        if self.active() != Some(i)
            && let Some(here) = self.active_buffer().map(nav_pos)
        {
            self.nav.push(here);
        }
        self.set_active(i);
    }

    pub fn activate(&mut self, i: usize, _cx: &mut Ctx) {
        self.activate_index(i);
    }

    /// [`Editor::activate_index`] without touching the history: its tab in
    /// the focused group, else a new tab there.
    pub(super) fn set_active(&mut self, i: usize) {
        let Some(id) = self.buffers.get(i).map(|b| b.id) else { return };
        let g = self.focused;
        let slot = match self.focused_group().slot_of(id) {
            Some(s) => s,
            None => self.add_view(g, id),
        };
        self.show(slot);
    }

    /// Add a tab of buffer `id` to group `g` right after its shown tab, as
    /// JetBrains does, but never among the pinned tabs. Returns its slot.
    /// A buffer shown elsewhere gets a parked copy of its live caret and
    /// scroll.
    fn add_view(&mut self, g: GroupId, id: BufferId) -> usize {
        let parked = match self.index_of_id(id) {
            Some(i) if self.view_count(id) > 0 => {
                let b = &self.buffers[i];
                Some(Parked::park(b.view.fork(), &b.rope, &b.edit_log, b.version))
            }
            _ => None,
        };
        let Some(group) = self.group_mut(g) else { return 0 };
        let at = group.active.map_or(group.views.len(), |a| a + 1).min(group.views.len()).max(group.pinned_count());
        group.views.insert(at, View { id, parked, pinned: false });
        if let Some(a) = group.active.filter(|a| *a >= at) {
            group.active = Some(a + 1);
        }
        at
    }

    /// Make tab `slot` of group `g` its buffer's live view: its parked state
    /// goes into `Buffer::view` and the state there is parked in the tab
    /// it belonged to.
    pub(super) fn make_live(&mut self, g: GroupId, slot: usize) {
        let Some(gi) = self.group_index(g) else { return };
        let Some(v) = self.groups[gi].views.get_mut(slot) else { return };
        let (id, Some(incoming)) = (v.id, v.parked.take()) else { return };
        let Some(b) = self.buffers.iter_mut().find(|b| b.id == id) else { return };
        let state = incoming.unpark(&b.rope, &b.edit_log, b.version);
        let out = Parked::park(std::mem::replace(&mut b.view, state), &b.rope, &b.edit_log, b.version);
        let owner = self.groups.iter_mut().enumerate().filter(|(i, _)| *i != gi).find_map(|(_, x)| {
            let s = x.slot_of(id)?;
            x.views[s].parked.is_none().then_some(&mut x.views[s])
        });
        match owner {
            Some(v) => v.parked = Some(out),
            None => debug_assert!(false, "buffer {id} had no live view"),
        }
    }

    /// Bring every parked view up to the edits made since the last frame,
    /// then forget them. Run at the start of each frame.
    pub(super) fn sync_parked(&mut self) {
        self.merge_catch_up();
        self.snippet_catch_up();
        let saved = self.merge.as_mut().map(|m| &mut m.saved.groups);
        for g in self.groups.iter_mut().chain(saved.into_iter().flatten()) {
            for v in &mut g.views {
                if let Some(p) = &mut v.parked
                    && let Some(b) = self.buffers.iter().find(|b| b.id == v.id)
                {
                    p.catch_up(&b.rope, &b.edit_log, b.version);
                }
            }
        }
        for b in &mut self.buffers {
            b.edit_log.clear();
        }
    }

    /// Show tab `slot` of the focused group.
    pub(super) fn show(&mut self, slot: usize) {
        self.make_live(self.focused, slot);
        let group = self.focused_group_mut();
        let Some(id) = group.views.get(slot).map(|v| v.id) else { return };
        group.active = Some(slot);
        group.touch(id);
        self.mru.retain(|x| *x != id);
        self.mru.insert(0, id);
        if let Some(i) = self.index_of_id(id) {
            self.buffers[i].undo.break_group = true;
        }
    }

    /// Move keyboard focus to group `g`; its shown tab becomes live.
    pub fn focus_group(&mut self, g: GroupId) {
        if g == self.focused || self.group(g).is_none() {
            return;
        }
        self.focused = g;
        if let Some(slot) = self.focused_group().active {
            self.show(slot);
        }
    }

    /// The groups in layout order.
    pub fn group_ids(&self) -> Vec<GroupId> {
        self.tree.leaves()
    }

    /// Alt+Tab / Alt+Shift+Tab: focus the next (previous) group in layout
    /// order.
    pub(super) fn cycle_group(&mut self, forward: bool) {
        let ids = self.tree.leaves();
        let Some(i) = ids.iter().position(|g| *g == self.focused) else { return };
        let n = ids.len();
        self.focus_group(ids[if forward { (i + 1) % n } else { (i + n - 1) % n }]);
    }

    /// Split the focused group along `axis`: a new group to the right or
    /// below shows the same tab, with its caret and scroll, and takes focus.
    /// Refused when the group is too small to halve.
    pub(super) fn split(&mut self, axis: Axis, cx: &mut Ctx) {
        let group = self.focused_group();
        let Some(id) = group.active_id() else {
            cx.notify(Level::Info, "Nothing to split: no tab open");
            return;
        };
        let r = group.last_area;
        let fits = match axis {
            Axis::Horizontal => r.width > 2 * MIN_GROUP_W,
            Axis::Vertical => r.height > 2 * MIN_GROUP_H,
        };
        // A group not drawn yet has no size to check.
        if !r.is_empty() && !fits {
            cx.notify(Level::Info, "Not enough room to split");
            return;
        }
        let new = self.next_group;
        self.next_group += 1;
        self.tree.split(self.focused, new, axis);
        // Right after its parent in `groups`, which is in no particular order.
        let at = self.group_index(self.focused).map_or(self.groups.len(), |i| i + 1);
        self.groups.insert(at, TabGroup::new(new));
        let slot = self.add_view(new, id);
        self.focused = new;
        self.show(slot);
    }

    /// Merge group `g` into the group beside it: its tabs move there (a
    /// buffer shown in both keeps the tab already there) and focus follows
    /// with the tab `g` showed. Nothing happens with one group.
    pub(super) fn unsplit(&mut self, g: GroupId) {
        let Some(target) = self.tree.sibling(g) else { return };
        let Some(gi) = self.group_index(g) else { return };
        let shown = self.groups[gi].active_id();
        let views = std::mem::take(&mut self.groups[gi].views);
        self.tree.remove(g);
        self.groups.remove(gi);
        let ti = self.group_index(target).expect("sibling group exists");
        for v in views {
            let t = &mut self.groups[ti];
            match t.slot_of(v.id) {
                // The duplicate goes; if it was the live view, the one kept
                // takes over.
                Some(s) => {
                    if v.parked.is_none()
                        && let Some(p) = t.views[s].parked.take()
                        && let Some(b) = self.buffers.iter_mut().find(|b| b.id == v.id)
                    {
                        b.view = p.unpark(&b.rope, &b.edit_log, b.version);
                    }
                }
                None => {
                    let at = if v.pinned { t.pinned_count() } else { t.views.len() };
                    if t.active.is_some_and(|a| a >= at) {
                        t.active = t.active.map(|a| a + 1);
                    }
                    t.views.insert(at, v);
                    if t.active.is_none() {
                        t.active = Some(0);
                    }
                }
            }
        }
        if self.focused == g || !self.groups.iter().any(|x| x.id == self.focused) {
            self.focused = target;
            let slot = shown.and_then(|id| self.focused_group().slot_of(id)).or(self.focused_group().active);
            if let Some(slot) = slot {
                self.show(slot);
            }
        }
    }

    /// Move the focused group's shown tab to the group beside it (a new
    /// one to the right when there is one group); focus goes with it. The
    /// last tab of a group only moves into an existing group.
    pub(super) fn move_to_opposite(&mut self, cx: &mut Ctx) {
        let from = self.focused;
        let group = self.focused_group();
        let (Some(slot), n) = (group.active, group.views.len()) else { return };
        let id = group.views[slot].id;
        match self.tree.sibling(from) {
            Some(to) => {
                let s = match self.group(to).and_then(|g| g.slot_of(id)) {
                    Some(s) => s,
                    None => self.add_view(to, id),
                };
                self.focused = to;
                self.show(s);
            }
            None => {
                self.split(Axis::Horizontal, cx);
                if self.focused == from || n == 1 {
                    return;
                }
            }
        }
        if let Some(s) = self.group(from).and_then(|g| g.slot_of(id)) {
            self.drop_view(from, s);
        }
    }

    /// The split tree.
    pub fn split_tree(&self) -> &SplitNode {
        &self.tree
    }

    /// Index in `buffers` of the tab a session restores as active: the
    /// focused group's shown tab, of the layout the resolver put aside
    /// while it is open.
    pub fn session_active(&self) -> Option<usize> {
        let (groups, _, focused) = self.session_layout();
        groups.iter().find(|g| g.id == focused).and_then(TabGroup::active_id).and_then(|id| self.index_of_id(id))
    }

    /// Lay the tabs out in a session's groups (`buffers` maps its files to
    /// buffer indices, `None` for files that did not open). Only a single
    /// group is replaced; tabs it had that the session does not place stay
    /// in the first group. Groups left without tabs collapse.
    pub fn restore_splits(&mut self, splits: &SessionSplits, buffers: &[Option<usize>]) {
        if self.groups.len() != 1 {
            return;
        }
        let old = self.groups.remove(0);
        let mut groups = Vec::new();
        let tree = self.session_node(&splits.tree, buffers, &mut groups);
        for v in old.views {
            if !groups.iter().any(|g: &TabGroup| g.slot_of(v.id).is_some()) {
                let g = &mut groups[0];
                g.active = g.active.or(Some(g.views.len()));
                g.views.push(v);
            }
        }
        // One live view per buffer: the first one in layout order.
        for b in &mut self.buffers {
            if groups.iter().any(|g| g.slot_of(b.id).is_some_and(|s| g.views[s].parked.is_none())) {
                continue;
            }
            if let Some(p) = groups.iter_mut().find_map(|g| g.slot_of(b.id).and_then(|s| g.views[s].parked.take())) {
                b.view = p.unpark(&b.rope, &b.edit_log, b.version);
            }
        }
        let leaves = tree.leaves();
        self.focused = leaves.get(splits.focused).copied().unwrap_or(leaves[0]);
        self.groups = groups;
        self.tree = tree;
        self.collapse_empty();
    }

    /// Build the groups of session node `n` into `groups`, every view
    /// parked with its saved caret and scroll.
    fn session_node(&mut self, n: &SessionNode, buffers: &[Option<usize>], groups: &mut Vec<TabGroup>) -> SplitNode {
        let sg = match n {
            SessionNode::Split { axis, ratio, a, b } => {
                let a = Box::new(self.session_node(a, buffers, groups));
                let b = Box::new(self.session_node(b, buffers, groups));
                return SplitNode::Split { axis: *axis, ratio: (*ratio).min(1000), a, b };
            }
            SessionNode::Leaf(sg) => sg,
        };
        let mut g = TabGroup::new(self.next_group);
        self.next_group += 1;
        for (k, sv) in sg.views.iter().enumerate() {
            let Some(b) = buffers.get(sv.file).copied().flatten().and_then(|i| self.buffers.get(i)) else { continue };
            if g.slot_of(b.id).is_some() {
                continue;
            }
            if sg.active == Some(k) {
                g.active = Some(g.views.len());
            }
            let sel = Selection::point(line_col_to_pos(&b.rope, sv.line, sv.col));
            let folds = super::fold::Folds::from_session(&sv.folds, &b.rope);
            let scroll_line = super::rows::Rows::new(&folds, b.rope.len_lines()).visible(sv.scroll_line);
            let wrap = super::wrap::Wrap::new(self.soft_wrap_for(b.path.as_deref()));
            let state = ViewState { sels: sel.into(), scroll_line, scroll_col: sv.scroll_col, folds, wrap, ..Default::default() };
            let parked = Some(Parked::park(state, &b.rope, &b.edit_log, b.version));
            g.views.push(View { id: b.id, parked, pinned: sv.pinned });
        }
        if g.active.is_none() && !g.views.is_empty() {
            g.active = Some(0);
        }
        let id = g.id;
        groups.push(g);
        SplitNode::Leaf(id)
    }

    /// Merge every group into one, keeping the focused tab shown.
    pub(super) fn unsplit_all(&mut self) {
        let shown = self.focused_group().active_id();
        while self.groups.len() > 1 {
            self.unsplit(self.focused);
        }
        if let Some(slot) = shown.and_then(|id| self.focused_group().slot_of(id)) {
            self.show(slot);
        }
    }

    /// Remove the groups left without tabs (never the last one); focus
    /// moves on from a removed focused group.
    pub(super) fn collapse_empty(&mut self) {
        while self.groups.len() > 1 {
            let Some(g) = self.groups.iter().find(|x| x.views.is_empty()).map(|x| x.id) else { break };
            let next = self.tree.sibling(g);
            self.tree.remove(g);
            self.groups.retain(|x| x.id != g);
            if self.focused == g
                && let Some(next) = next
            {
                self.focused = next;
            }
        }
        if let Some(slot) = self.focused_group().active {
            self.show(slot);
        }
    }

    /// A click on tab `slot` of group `g`: focus the group and show the tab,
    /// remembering the position left for back/forward.
    pub fn activate_slot(&mut self, g: GroupId, slot: usize) {
        let Some(id) = self.group(g).and_then(|x| x.views.get(slot)).map(|v| v.id) else { return };
        if self.active_buffer().is_some_and(|b| b.id != id)
            && let Some(here) = self.active_buffer().map(nav_pos)
        {
            self.nav.push(here);
        }
        self.focused = g;
        self.show(slot);
    }

    /// Close tab `slot` of group `g`. The buffer stays open while another
    /// group shows it; its last tab asks first when it has unsaved changes.
    /// While the conflict resolver is open this cancels it.
    pub fn close_slot(&mut self, g: GroupId, slot: usize, cx: &mut Ctx) {
        // A resolver pane's tab closes the resolver.
        if self.merge.is_some() {
            return self.merge_cancel(false, cx);
        }
        let Some(id) = self.group(g).and_then(|x| x.views.get(slot)).map(|v| v.id) else { return };
        if self.view_count(id) > 1 {
            self.drop_view(g, slot);
            return;
        }
        let Some(i) = self.index_of_id(id) else { return };
        let b = &self.buffers[i];
        if b.dirty() {
            let msg = format!("{} has unsaved changes.", b.name);
            cx.effects.push(Effect::Dialog(Dialog::confirm(
                DialogId::CloseTabDirty(b.id),
                "Unsaved changes",
                &msg,
                &["Save", "Don't save", "Cancel"],
            )));
            return;
        }
        self.force_close(i);
    }

    /// Remove tab `slot` from group `g`, leaving its buffer open in the
    /// other groups that show it. A live tab hands over to one of those.
    fn drop_view(&mut self, g: GroupId, slot: usize) {
        let Some(gi) = self.group_index(g) else { return };
        let Some(id) = self.groups[gi].views.get(slot).map(|v| v.id) else { return };
        if g == self.focused
            && self.groups[gi].active == Some(slot)
            && let Some(here) = self.active_buffer().map(nav_pos)
        {
            self.nav.push(here);
        }
        let v = remove_view(&mut self.groups[gi], slot);
        if v.parked.is_none()
            && let Some(p) = self.groups.iter_mut().find_map(|x| x.slot_of(id).and_then(|s| x.views[s].parked.take()))
            && let Some(b) = self.buffers.iter_mut().find(|b| b.id == id)
        {
            b.view = p.unpark(&b.rope, &b.edit_log, b.version);
        }
        self.collapse_empty();
    }

    pub fn force_close(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        let id = self.buffers[i].id;
        self.park_bookmarks(i);
        // Back from the next tab reopens the closed one.
        if self.active() == Some(i) {
            self.nav.push(nav_pos(&self.buffers[i]));
        }
        let saved = self.merge.as_mut().map(|m| &mut m.saved.groups);
        for g in self.groups.iter_mut().chain(saved.into_iter().flatten()) {
            if let Some(slot) = g.slot_of(id) {
                remove_view(g, slot);
            }
        }
        self.buffers.remove(i);
        self.mru.retain(|x| *x != id);
        self.collapse_empty();
    }

    pub fn close_all(&mut self, cx: &mut Ctx) {
        if self.has_dirty() {
            cx.effects.push(Effect::Dialog(Dialog::confirm(
                DialogId::CloseAllDirty,
                "Unsaved changes",
                "Some files have unsaved changes.",
                &["Save all", "Don't save", "Cancel"],
            )));
            return;
        }
        self.force_close_all();
    }

    /// Close every buffer and go back to one empty group.
    pub fn force_close_all(&mut self) {
        for i in 0..self.buffers.len() {
            self.park_bookmarks(i);
        }
        self.buffers.clear();
        self.mru.clear();
        self.groups = vec![TabGroup::new(self.next_group)];
        self.tree = SplitNode::Leaf(self.next_group);
        self.focused = self.next_group;
        self.next_group += 1;
    }

    /// Close the tabs of `ids` in the focused group. Buffers another group
    /// still shows only lose this tab; for the others this asks first when
    /// any has unsaved changes.
    pub(super) fn close_tabs(&mut self, ids: Vec<BufferId>, cx: &mut Ctx) {
        let g = self.focused;
        let (shared, last): (Vec<BufferId>, Vec<BufferId>) = ids.into_iter().partition(|id| self.view_count(*id) > 1);
        for id in shared {
            if let Some(slot) = self.focused_group().slot_of(id) {
                self.drop_view(g, slot);
            }
        }
        self.close_ids(last, cx);
    }

    /// Close the buffers `ids`, asking first when any has unsaved changes.
    fn close_ids(&mut self, ids: Vec<BufferId>, cx: &mut Ctx) {
        if ids.is_empty() {
            return;
        }
        if self.buffers.iter().any(|b| b.dirty() && ids.contains(&b.id)) {
            cx.effects.push(Effect::Dialog(Dialog::confirm(
                DialogId::CloseTabsDirty(ids),
                "Unsaved changes",
                "Some of these files have unsaved changes.",
                &["Save", "Don't save", "Cancel"],
            )));
            return;
        }
        self.force_close_ids(&ids);
    }

    /// Close the buffers `ids` that are still open, unsaved edits included.
    pub fn force_close_ids(&mut self, ids: &[BufferId]) {
        for id in ids {
            if let Some(i) = self.index_of_id(*id) {
                self.force_close(i);
            }
        }
    }

    /// Save the unsaved buffers among `ids`, then close those that are
    /// clean (a save that failed or needs an answer keeps its tab, and so
    /// does an untitled buffer, left to Save As).
    pub fn save_and_close_ids(&mut self, ids: &[BufferId], cx: &mut Ctx) {
        for id in ids {
            let Some(i) = self.index_of_id(*id) else { continue };
            let b = &self.buffers[i];
            if b.dirty() && b.path.is_none() {
                continue;
            }
            if !b.dirty() || self.save(i, cx) {
                self.force_close(i);
            }
        }
    }

    pub fn is_pinned_slot(&self, g: GroupId, slot: usize) -> bool {
        self.group(g).and_then(|x| x.views.get(slot)).is_some_and(|v| v.pinned)
    }

    /// Pin or unpin tab `slot` of group `g`. A pinned tab moves to the end
    /// of the pinned ones, an unpinned one to the start of the rest.
    /// Returns its new slot.
    pub fn set_pinned_slot(&mut self, g: GroupId, slot: usize, pinned: bool) -> usize {
        let Some(group) = self.group_mut(g) else { return slot };
        if slot >= group.views.len() || group.views[slot].pinned == pinned {
            return slot;
        }
        let n = group.pinned_count();
        group.views[slot].pinned = pinned;
        group.move_tab(slot, if pinned { n } else { n - 1 })
    }

    /// [`Editor::set_pinned_slot`] for buffer `i`'s tab in the focused group.
    pub fn set_pinned(&mut self, i: usize, pinned: bool) {
        let Some(id) = self.buffers.get(i).map(|b| b.id) else { return };
        if let Some(slot) = self.focused_group().slot_of(id) {
            self.set_pinned_slot(self.focused, slot, pinned);
        }
    }

    /// Alt+Shift+Left / Alt+Shift+Right: move the shown tab one place
    /// within its pin range.
    pub(super) fn step_tab(&mut self, right: bool) {
        let group = self.focused_group_mut();
        let Some(i) = group.active else { return };
        let range = group.pin_range(i);
        let to = if right { i + 1 } else { i.wrapping_sub(1) };
        if range.contains(&to) {
            group.move_tab(i, to);
        }
    }

    /// A tab drag over column `x`: move tab `id` of group `g` to the tab
    /// under the pointer, within its pin range. It moves only once the
    /// pointer is over the place it would take, so tabs of different widths
    /// do not swap back and forth. The hit rects are stale after a move
    /// until the next frame, so they are cleared.
    pub fn drag_tab(&mut self, g: GroupId, id: BufferId, x: u16) {
        let Some(group) = self.group_mut(g) else { return };
        let Some(cur) = group.slot_of(id) else { return };
        let Some(over) = group.tab_hits.tab_at_x(x) else { return };
        // `cur` is in its range, so the range is not empty.
        let range = group.pin_range(cur);
        let to = over.clamp(range.start, range.end - 1);
        if to == cur {
            return;
        }
        let (Some(from_r), Some(to_r)) = (group.tab_hits.rect_of(cur), group.tab_hits.rect_of(to)) else { return };
        let reached = if to > cur { x >= to_r.right().saturating_sub(from_r.width) } else { x < to_r.x + from_r.width };
        if reached {
            group.move_tab(cur, to);
            group.tab_hits.clear();
        }
    }

    /// Close the least recently used buffers while more than
    /// `editor.max_tabs` are open (0: no limit). Only clean buffers without
    /// a pinned tab that no group shows are closed; when none is left the
    /// limit is exceeded. Called by the app after a file opens and after a
    /// session restore.
    pub fn enforce_tab_limit(&mut self) -> usize {
        let mut closed = 0;
        while self.max_tabs > 0 && self.buffers.len() > self.max_tabs {
            let shown: Vec<BufferId> = self.groups.iter().filter_map(TabGroup::active_id).collect();
            let pinned = |id: BufferId| self.groups.iter().any(|g| g.views.iter().any(|v| v.id == id && v.pinned));
            let victim = self.mru.iter().rev().filter_map(|id| self.index_of_id(*id)).find(|&i| {
                let b = &self.buffers[i];
                !shown.contains(&b.id) && !pinned(b.id) && !b.dirty()
            });
            let Some(i) = victim else { break };
            tracing::debug!("tab limit {}: closing {}", self.max_tabs, self.buffers[i].name);
            self.force_close(i);
            closed += 1;
        }
        closed
    }

    /// Right-click on a tab (already shown): pin and close actions.
    pub fn tab_menu(&self, x: u16, y: u16, cx: &mut Ctx) {
        let group = self.focused_group();
        let Some(slot) = group.active else { return };
        let e = Action::Editor;
        let pinned = group.views[slot].pinned;
        let others = group.views.iter().enumerate().any(|(i, v)| !v.pinned && i != slot);
        let unpinned = group.views.iter().any(|v| !v.pinned);
        let split = self.groups.len() > 1;
        let items = vec![
            MenuItem::new(if pinned { "Unpin Tab" } else { "Pin Tab" }, e(EditorAction::TogglePinTab)),
            MenuItem::new("Close", e(EditorAction::CloseTab)),
            MenuItem::new("Close Other Tabs", e(EditorAction::CloseOtherTabs)).enabled(others),
            MenuItem::new("Close All but Pinned", e(EditorAction::CloseUnpinnedTabs)).enabled(unpinned),
            MenuItem::new("Close All Tabs", e(EditorAction::CloseAllTabs)),
            MenuItem::separator(),
            MenuItem::new("Split Right", e(EditorAction::SplitRight)),
            MenuItem::new("Split Down", e(EditorAction::SplitDown)),
            MenuItem::new("Move to Opposite Group", e(EditorAction::MoveTabToOppositeGroup)),
            MenuItem::new("Unsplit", e(EditorAction::Unsplit)).enabled(split),
        ];
        cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, x, y)));
    }

    /// Panics when the groups and buffers disagree.
    #[cfg(test)]
    pub fn check_invariants(&self) {
        assert!(self.group(self.focused).is_some(), "focused group exists");
        let mut leaves = self.tree.leaves();
        let mut ids: Vec<GroupId> = self.groups.iter().map(|g| g.id).collect();
        leaves.sort_unstable();
        ids.sort_unstable();
        assert_eq!(leaves, ids, "the tree's leaves are the groups");
        assert!(self.groups.len() == 1 || self.groups.iter().all(|g| !g.views.is_empty()), "no empty group beside others");
        let saved = self.merge.as_ref().map_or(&[][..], |m| &m.saved.groups[..]);
        let all = || self.groups.iter().chain(saved);
        for b in &self.buffers {
            assert!(all().any(|g| g.slot_of(b.id).is_some()), "{} has a tab", b.name);
            let live = all().filter(|g| g.slot_of(b.id).is_some_and(|s| g.views[s].parked.is_none())).count();
            assert_eq!(live, 1, "{} has one live view", b.name);
            assert!(b.view.sels.is_normalized(), "{}: carets sorted and apart", b.name);
        }
        let f = self.focused_group();
        assert!(f.active.is_none_or(|a| f.views[a].parked.is_none()), "the focused tab is live");
        if let Some(m) = &self.merge {
            assert!(self.groups.iter().flat_map(|g| &g.views).all(|v| m.ids.contains(&v.id)), "only the resolver's panes");
        }
        for g in all() {
            for (i, v) in g.views.iter().enumerate() {
                assert!(self.index_of_id(v.id).is_some(), "tab of a closed buffer");
                assert!(g.views[..i].iter().all(|w| w.id != v.id), "one tab per buffer and group");
                assert!(v.parked.as_ref().is_none_or(|p| p.state.sels.is_normalized()), "parked carets sorted and apart");
            }
            assert_eq!(g.active.is_some(), !g.views.is_empty(), "a group with tabs shows one");
            assert!(g.active.is_none_or(|a| a < g.views.len()));
            assert!(g.views.iter().skip(g.pinned_count()).all(|v| !v.pinned), "pinned tabs first");
        }
    }

    /// Show tab `slot` of the focused group, as a click on it does.
    #[cfg(test)]
    pub fn show_slot(&mut self, slot: usize) {
        self.activate_slot(self.focused, slot);
    }

    /// The tab names of the focused group, in bar order.
    #[cfg(test)]
    pub fn tab_names(&self) -> Vec<String> {
        let names = self.focused_group().views.iter().filter_map(|v| self.index_of_id(v.id));
        names.map(|i| self.buffers[i].name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::Config;
    use crate::config::theme::Theme;
    use crate::editor::selection::Selection;
    use crate::editor::undo::{Change, TxKind};
    use crate::workspace::Workspace;

    fn act(ed: &mut Editor, a: EditorAction) -> Vec<Effect> {
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/tmp")));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        ed.handle_action(&a, &mut cx);
        ed.check_invariants();
        cx.effects
    }

    fn open(ed: &mut Editor, name: &str, text: &str) {
        ed.open_text(PathBuf::from(format!("/tmp/{name}")), text, false, &Theme::default());
        ed.check_invariants();
    }

    fn draw(ed: &mut Editor, w: u16, h: u16, zen: bool) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        let tabs = Rect::new(0, 0, w, if zen { 0 } else { 1 });
        let area = Rect::new(0, tabs.height, w, h - tabs.height);
        let frame = term.draw(|f| ed.render(f, tabs, area, true, &Theme::default())).unwrap();
        ed.check_invariants();
        frame.buffer.clone()
    }

    fn row(screen: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..screen.area.width).map(|x| screen[(x, y)].symbol().to_string()).collect()
    }

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn split_forks_the_view_and_edits_move_the_other_caret() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed, "a.txt", "hello world\n");
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(6));
        let left = ed.focused;
        act(&mut ed, EditorAction::SplitRight);
        assert_ne!(ed.focused, left);
        assert_eq!(ed.group_ids(), [left, ed.focused]);
        assert_eq!(ed.active_buffer().unwrap().view.sel().head, 6, "the new view starts at the same caret");
        // Typing in the right group moves the left group's caret.
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(0));
        act(&mut ed, EditorAction::InsertText(">> ".into()));
        act(&mut ed, EditorAction::PrevGroup);
        assert_eq!(ed.focused, left);
        assert_eq!(ed.active_buffer().unwrap().view.sel().head, 9);
        // Undo in the left group puts the right group's caret back too.
        act(&mut ed, EditorAction::Undo);
        act(&mut ed, EditorAction::NextGroup);
        assert_eq!(ed.active_buffer().unwrap().view.sel().head, 0);
        assert_eq!(ed.buffers.len(), 1, "one buffer, two views");
    }

    #[test]
    fn an_edit_of_a_buffer_shown_elsewhere_maps_its_parked_views() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed, "a.txt", "one two\n");
        act(&mut ed, EditorAction::SplitDown);
        open(&mut ed, "b.txt", "other\n");
        // a.txt's live view is in the first group now; the second shows b.
        let a = ed.index_of(&PathBuf::from("/tmp/a.txt")).unwrap();
        ed.buffers[a].view.set_sel(Selection::point(4));
        act(&mut ed, EditorAction::PrevGroup);
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(4));
        act(&mut ed, EditorAction::NextGroup);
        // A language server edit lands in a.txt, live in the first group.
        let b = &mut ed.buffers[a];
        b.apply(vec![Change::insert(0, "zero ")], b.view.sel(), TxKind::Other);
        act(&mut ed, EditorAction::PrevGroup);
        assert_eq!(ed.active_buffer().unwrap().view.sel().head, 4, "the live view keeps what the edit set");
        // The second group's view of a.txt was parked: shown again, it maps.
        let g = ed.focused;
        act(&mut ed, EditorAction::NextGroup);
        let slot = ed.focused_group().slot_of(ed.buffers[a].id).unwrap();
        ed.show(slot);
        ed.check_invariants();
        assert_eq!(ed.active_buffer().unwrap().view.sel().head, 9);
        assert_ne!(ed.focused, g);
    }

    #[test]
    fn closing_tabs_with_views_elsewhere_and_unsplit() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed, "a.txt", "a\n");
        open(&mut ed, "b.txt", "b\n");
        act(&mut ed, EditorAction::InsertText("dirty ".into()));
        act(&mut ed, EditorAction::SplitRight);
        open(&mut ed, "c.txt", "c\n");
        assert_eq!(ed.tab_names(), ["b.txt", "c.txt"]);
        // Closing the dirty buffer's tab here asks nothing: the left group
        // still shows it.
        ed.show_slot(0);
        assert!(act(&mut ed, EditorAction::CloseTab).is_empty());
        assert_eq!(ed.tab_names(), ["c.txt"]);
        // Its last tab asks.
        act(&mut ed, EditorAction::PrevGroup);
        ed.show_slot(1);
        let fx = act(&mut ed, EditorAction::CloseTab);
        assert!(matches!(fx.as_slice(), [Effect::Dialog(d)] if matches!(d.id, DialogId::CloseTabDirty(_))));
        // Unsplit merges the right group's tabs into the left one, once each.
        act(&mut ed, EditorAction::NextGroup);
        open(&mut ed, "a.txt", "a\n");
        act(&mut ed, EditorAction::Unsplit);
        assert_eq!(ed.groups.len(), 1);
        assert_eq!(ed.tab_names(), ["a.txt", "b.txt", "c.txt"]);
        assert_eq!(ed.active_buffer().unwrap().name, "a.txt", "the merged group's tab stays shown");
        // Closing a group's last tab removes the group.
        act(&mut ed, EditorAction::SplitDown);
        act(&mut ed, EditorAction::CloseTab);
        assert_eq!(ed.groups.len(), 1);
        assert_eq!(ed.buffers.len(), 3);
    }

    #[test]
    fn tab_limit_counts_buffers_and_spares_shown_tabs() {
        let mut config = Config::default();
        config.editor.max_tabs = 2;
        let mut ed = Editor::new(&config);
        open(&mut ed, "a.txt", "a\n");
        act(&mut ed, EditorAction::SplitRight);
        open(&mut ed, "b.txt", "b\n");
        assert_eq!(ed.enforce_tab_limit(), 0, "two buffers in three tabs");
        open(&mut ed, "c.txt", "c\n");
        // a is shown in the left group, c on the right: b goes.
        assert_eq!(ed.enforce_tab_limit(), 1);
        assert!(ed.index_of(&PathBuf::from("/tmp/b.txt")).is_none());
        ed.check_invariants();
    }

    #[test]
    fn split_is_refused_when_too_small_and_unsplit_all_keeps_the_tab() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed, "a.txt", "a\n");
        draw(&mut ed, 40, 10, false);
        let fx = act(&mut ed, EditorAction::SplitRight);
        assert!(matches!(fx.as_slice(), [Effect::Notify(_, t)] if t == "Not enough room to split"));
        act(&mut ed, EditorAction::SplitDown);
        draw(&mut ed, 40, 10, false);
        act(&mut ed, EditorAction::SplitDown);
        assert_eq!(ed.groups.len(), 2, "a 4-row group does not split again");
        open(&mut ed, "b.txt", "b\n");
        act(&mut ed, EditorAction::UnsplitAll);
        assert_eq!((ed.groups.len(), ed.active_buffer().unwrap().name.as_str()), (1, "b.txt"));
    }

    #[test]
    fn groups_render_side_by_side_with_their_own_scroll_and_cursor() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed, "a.txt", &lines(100));
        open(&mut ed, "b.txt", "b\n");
        ed.show_slot(0);
        act(&mut ed, EditorAction::SplitRight);
        draw(&mut ed, 100, 30, false);
        act(&mut ed, EditorAction::ScrollDown(50));
        let screen = draw(&mut ed, 100, 30, false);
        let top = row(&screen, 0);
        assert!(top.starts_with("  a.txt ×   b.txt × "), "{top}");
        assert_eq!(ed.focused_group().last_area, Rect::new(50, 0, 50, 30));
        assert!(top.chars().skip(50).collect::<String>().starts_with("  a.txt × "), "{top}");
        assert_eq!(screen[(49, 0)].symbol(), "│");
        // Left: from line 1; right: scrolled to line 51.
        let body = row(&screen, 1);
        assert!(body.contains("line 0") && body.contains("line 50"), "{body}");
        // The cursor is in the focused (right) group only.
        let left = ed.groups[0].caret_cell.unwrap();
        let right = ed.focused_group().caret_cell;
        assert!(left.0 < 49 && right.is_none(), "the right caret is scrolled out");
        assert_ne!(screen[(left.0, left.1)].bg, Theme::default().current_line_bg);
        // With its caret in view the focused group gets the terminal cursor.
        ed.goto_line(60, 1);
        let mut term = ratatui::Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| ed.render(f, Rect::new(0, 0, 100, 1), Rect::new(0, 1, 100, 29), true, &Theme::default())).unwrap();
        use ratatui::backend::Backend;
        let cursor = term.backend_mut().get_cursor_position().unwrap();
        assert_eq!(Some((cursor.x, cursor.y)), ed.caret_cell());
        assert!(cursor.x > 50);
        // Zen mode: no tab bars, the split stays.
        let screen = draw(&mut ed, 100, 30, true);
        assert_eq!(screen[(49, 0)].symbol(), "│");
        assert!(row(&screen, 0).contains("line 0"));
    }

    #[test]
    fn move_to_opposite_group_splits_or_moves() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed, "a.txt", "a\n");
        // A group's only tab: the split keeps a copy.
        act(&mut ed, EditorAction::MoveTabToOppositeGroup);
        assert_eq!((ed.groups.len(), ed.buffers.len()), (2, 1));
        act(&mut ed, EditorAction::UnsplitAll);
        open(&mut ed, "b.txt", "b\n");
        act(&mut ed, EditorAction::MoveTabToOppositeGroup);
        assert_eq!(ed.tab_names(), ["b.txt"]);
        act(&mut ed, EditorAction::PrevGroup);
        assert_eq!(ed.tab_names(), ["a.txt"]);
        // Moved back, the right group loses its last tab and goes.
        act(&mut ed, EditorAction::NextGroup);
        act(&mut ed, EditorAction::MoveTabToOppositeGroup);
        assert_eq!(ed.groups.len(), 1);
        assert_eq!((ed.tab_names(), ed.active_buffer().unwrap().name.as_str()), (vec!["a.txt".to_string(), "b.txt".into()], "b.txt"));
    }
}
