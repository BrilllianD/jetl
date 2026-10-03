//! The split tree of the editor area: leaves are tab groups, inner nodes
//! split their rect in two along an axis with a one-cell bar between.

use ratatui::layout::Rect;

use super::groups::GroupId;

/// Narrowest and lowest a group gets while its split has room for both.
pub const MIN_GROUP_W: u16 = 20;
pub const MIN_GROUP_H: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Axis {
    /// Side by side, a bar column between (split right).
    Horizontal,
    /// One above the other, a bar row between (split down).
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitNode {
    Leaf(GroupId),
    Split {
        axis: Axis,
        /// Share of `a` in permille of the room left by the bar.
        ratio: u16,
        a: Box<SplitNode>,
        b: Box<SplitNode>,
    },
}

/// A splitter bar as last laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitterHit {
    /// The split's index in preorder (see [`SplitNode::set_ratio`]).
    pub node: usize,
    pub axis: Axis,
    pub rect: Rect,
    /// The split's whole rect, which a drag measures against.
    pub area: Rect,
}

impl SplitterHit {
    /// The ratio that puts the bar at `x`, `y`.
    pub fn ratio_at(&self, x: u16, y: u16) -> u16 {
        let (pos, start, len) = match self.axis {
            Axis::Horizontal => (x, self.area.x, self.area.width),
            Axis::Vertical => (y, self.area.y, self.area.height),
        };
        let room = len.saturating_sub(1).max(1) as u32;
        // Rounded up, so the layout's rounding down lands on `pos`.
        (pos.saturating_sub(start) as u32 * 1000).div_ceil(room).min(1000) as u16
    }
}

/// Split `len` cells at `ratio` permille around a one-cell bar: the sizes
/// of `a` and `b`. Each side keeps `min` while there is room for both;
/// with no room for the bar `a` takes everything.
fn sizes(len: u16, ratio: u16, min: u16) -> Option<(u16, u16)> {
    if len < 3 {
        return None;
    }
    let room = len - 1;
    let mut a = (room as u32 * ratio.min(1000) as u32 / 1000) as u16;
    if room >= 2 * min {
        a = a.clamp(min, room - min);
    } else {
        a = a.clamp(1, room - 1);
    }
    Some((a, room - a))
}

impl SplitNode {
    /// Tile `area`: the rect of every leaf, in order, and every bar.
    pub fn layout(&self, area: Rect, leaves: &mut Vec<(GroupId, Rect)>, bars: &mut Vec<SplitterHit>) {
        let mut n = 0;
        self.layout_at(area, &mut n, leaves, bars);
    }

    fn layout_at(&self, area: Rect, n: &mut usize, leaves: &mut Vec<(GroupId, Rect)>, bars: &mut Vec<SplitterHit>) {
        let node = *n;
        *n += 1;
        let (axis, ratio, a, b) = match self {
            SplitNode::Leaf(g) => return leaves.push((*g, area)),
            SplitNode::Split { axis, ratio, a, b } => (*axis, *ratio, a, b),
        };
        let (ra, bar, rb) = match axis {
            Axis::Horizontal => match sizes(area.width, ratio, MIN_GROUP_W) {
                Some((wa, wb)) => (
                    Rect::new(area.x, area.y, wa, area.height),
                    Rect::new(area.x + wa, area.y, 1, area.height),
                    Rect::new(area.x + wa + 1, area.y, wb, area.height),
                ),
                None => (area, Rect::default(), Rect::new(area.right(), area.y, 0, area.height)),
            },
            Axis::Vertical => match sizes(area.height, ratio, MIN_GROUP_H) {
                Some((ha, hb)) => (
                    Rect::new(area.x, area.y, area.width, ha),
                    Rect::new(area.x, area.y + ha, area.width, 1),
                    Rect::new(area.x, area.y + ha + 1, area.width, hb),
                ),
                None => (area, Rect::default(), Rect::new(area.x, area.bottom(), area.width, 0)),
            },
        };
        if !bar.is_empty() {
            bars.push(SplitterHit { node, axis, rect: bar, area });
        }
        a.layout_at(ra, n, leaves, bars);
        b.layout_at(rb, n, leaves, bars);
    }

    /// The groups in layout order.
    pub fn leaves(&self) -> Vec<GroupId> {
        match self {
            SplitNode::Leaf(g) => vec![*g],
            SplitNode::Split { a, b, .. } => {
                let mut v = a.leaves();
                v.extend(b.leaves());
                v
            }
        }
    }

    /// Split leaf `g` in half along `axis`, `new` to the right or below.
    pub fn split(&mut self, g: GroupId, new: GroupId, axis: Axis) -> bool {
        match self {
            SplitNode::Leaf(x) if *x == g => {
                *self = SplitNode::Split { axis, ratio: 500, a: Box::new(SplitNode::Leaf(g)), b: Box::new(SplitNode::Leaf(new)) };
                true
            }
            SplitNode::Leaf(_) => false,
            SplitNode::Split { a, b, .. } => a.split(g, new, axis) || b.split(g, new, axis),
        }
    }

    /// Remove leaf `g`; its sibling takes the place of their split. The
    /// last leaf is never removed.
    pub fn remove(&mut self, g: GroupId) -> bool {
        let SplitNode::Split { a, b, .. } = self else { return false };
        let keep = match (&**a, &**b) {
            (SplitNode::Leaf(x), _) if *x == g => Some(std::mem::replace(&mut **b, SplitNode::Leaf(g))),
            (_, SplitNode::Leaf(x)) if *x == g => Some(std::mem::replace(&mut **a, SplitNode::Leaf(g))),
            _ => None,
        };
        match keep {
            Some(k) => {
                *self = k;
                true
            }
            None => a.remove(g) || b.remove(g),
        }
    }

    /// The group that takes over from leaf `g` when it goes: the nearest
    /// leaf of its sibling (the first one after it, else the last before).
    pub fn sibling(&self, g: GroupId) -> Option<GroupId> {
        let SplitNode::Split { a, b, .. } = self else { return None };
        match (&**a, &**b) {
            (SplitNode::Leaf(x), other) if *x == g => other.leaves().first().copied(),
            (other, SplitNode::Leaf(x)) if *x == g => other.leaves().last().copied(),
            _ => a.sibling(g).or_else(|| b.sibling(g)),
        }
    }

    /// The axis of the split right above leaf `g`.
    pub fn parent_axis(&self, g: GroupId) -> Option<Axis> {
        let SplitNode::Split { axis, a, b, .. } = self else { return None };
        if matches!(**a, SplitNode::Leaf(x) if x == g) || matches!(**b, SplitNode::Leaf(x) if x == g) {
            return Some(*axis);
        }
        a.parent_axis(g).or_else(|| b.parent_axis(g))
    }

    /// Set the ratio of the split with preorder index `node` (the index
    /// [`SplitNode::layout`] gives its bar).
    pub fn set_ratio(&mut self, node: usize, ratio: u16) {
        let mut n = 0;
        self.set_ratio_at(node, ratio.min(1000), &mut n);
    }

    fn set_ratio_at(&mut self, node: usize, value: u16, n: &mut usize) -> bool {
        let here = *n;
        *n += 1;
        match self {
            SplitNode::Leaf(_) => false,
            SplitNode::Split { ratio, a, b, .. } => {
                if here == node {
                    *ratio = value;
                    return true;
                }
                a.set_ratio_at(node, value, n) || b.set_ratio_at(node, value, n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(t: &SplitNode, area: Rect) -> (Vec<(GroupId, Rect)>, Vec<SplitterHit>) {
        let (mut leaves, mut bars) = (Vec::new(), Vec::new());
        t.layout(area, &mut leaves, &mut bars);
        (leaves, bars)
    }

    #[test]
    fn split_halves_and_ratio_moves_the_bar() {
        let mut t = SplitNode::Leaf(0);
        assert!(t.split(0, 1, Axis::Horizontal));
        let (leaves, bars) = tile(&t, Rect::new(0, 0, 81, 20));
        assert_eq!(leaves, [(0, Rect::new(0, 0, 40, 20)), (1, Rect::new(41, 0, 40, 20))]);
        assert_eq!(bars[0].rect, Rect::new(40, 0, 1, 20));
        assert_eq!(bars[0].ratio_at(60, 3), 750);
        t.set_ratio(bars[0].node, 750);
        let (leaves, _) = tile(&t, Rect::new(0, 0, 81, 20));
        assert_eq!(leaves[0].1.width, 60);
    }

    #[test]
    fn minimum_sizes_and_tiny_rects() {
        let mut t = SplitNode::Leaf(0);
        t.split(0, 1, Axis::Vertical);
        t.set_ratio(0, 0);
        let (leaves, _) = tile(&t, Rect::new(0, 0, 50, 20));
        assert_eq!((leaves[0].1.height, leaves[1].1.height), (MIN_GROUP_H, 15));
        t.set_ratio(0, 1000);
        let (leaves, _) = tile(&t, Rect::new(0, 0, 50, 20));
        assert_eq!((leaves[0].1.height, leaves[1].1.height), (15, MIN_GROUP_H));
        // Too small for both minimums: proportional, one cell each at least.
        let (leaves, bars) = tile(&t, Rect::new(0, 0, 50, 6));
        assert_eq!((leaves[0].1.height, leaves[1].1.height, bars.len()), (4, 1, 1));
        // No room for a bar: the first side takes it all.
        let (leaves, bars) = tile(&t, Rect::new(0, 0, 50, 2));
        assert_eq!((leaves[0].1, leaves[1].1.height, bars.len()), (Rect::new(0, 0, 50, 2), 0, 0));
    }

    #[test]
    fn nested_splits_tile_and_number_in_preorder() {
        let mut t = SplitNode::Leaf(0);
        t.split(0, 1, Axis::Horizontal);
        t.split(1, 2, Axis::Vertical);
        t.split(0, 3, Axis::Vertical);
        assert_eq!(t.leaves(), [0, 3, 1, 2]);
        let area = Rect::new(2, 1, 101, 41);
        let (leaves, bars) = tile(&t, area);
        // Every cell is a leaf or a bar, once.
        let cells: u32 = leaves.iter().map(|(_, r)| r.area()).chain(bars.iter().map(|b| b.rect.area())).sum();
        assert_eq!(cells, area.area());
        assert_eq!(bars.iter().map(|b| b.node).collect::<Vec<_>>(), [0, 1, 4]);
        t.set_ratio(4, 250);
        let (leaves, _) = tile(&t, area);
        assert_eq!(leaves[2].1.height, 10);
    }

    #[test]
    fn remove_collapses_into_the_sibling() {
        let mut t = SplitNode::Leaf(0);
        t.split(0, 1, Axis::Horizontal);
        t.split(1, 2, Axis::Vertical);
        assert_eq!(t.sibling(1), Some(2));
        assert_eq!(t.sibling(2), Some(1));
        assert_eq!(t.sibling(0), Some(1));
        assert_eq!(t.parent_axis(2), Some(Axis::Vertical));
        assert!(t.remove(1));
        assert_eq!(t.leaves(), [0, 2]);
        assert_eq!(t.parent_axis(2), Some(Axis::Horizontal));
        assert!(t.remove(0));
        assert_eq!(t, SplitNode::Leaf(2));
        assert!(!t.remove(2), "the last leaf stays");
    }
}
