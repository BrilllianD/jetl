//! Back/forward navigation history (Ctrl+Alt+Left/Right): caret positions
//! left by jumps and tab switches.

use std::path::PathBuf;

use super::buffer::BufferId;

/// Most positions each direction keeps; the oldest fall off.
pub const NAV_CAP: usize = 100;

/// A caret position in a buffer. The path lets a closed file be reopened;
/// the id tells apart untitled buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavPos {
    pub id: BufferId,
    pub path: Option<PathBuf>,
    pub pos: usize,
}

#[derive(Debug, Default)]
pub struct NavRing {
    back: Vec<NavPos>,
    forward: Vec<NavPos>,
}

fn push_capped(v: &mut Vec<NavPos>, p: NavPos) {
    if v.last() != Some(&p) {
        v.push(p);
    }
    if v.len() > NAV_CAP {
        v.remove(0);
    }
}

impl NavRing {
    /// Remember the position being left. A new navigation forgets the
    /// positions back went through.
    pub fn push(&mut self, p: NavPos) {
        self.forward.clear();
        push_capped(&mut self.back, p);
    }

    /// The previous position, skipping ones `usable` rejects and the
    /// current one; `current` goes onto the forward list.
    pub fn back(&mut self, current: Option<NavPos>, usable: impl Fn(&NavPos) -> bool) -> Option<NavPos> {
        step(&mut self.back, &mut self.forward, current, usable)
    }

    /// The opposite of [`NavRing::back`].
    pub fn forward(&mut self, current: Option<NavPos>, usable: impl Fn(&NavPos) -> bool) -> Option<NavPos> {
        step(&mut self.forward, &mut self.back, current, usable)
    }

    pub fn clear(&mut self) {
        self.back.clear();
        self.forward.clear();
    }

    pub fn sizes(&self) -> (usize, usize) {
        (self.back.len(), self.forward.len())
    }
}

fn step(from: &mut Vec<NavPos>, to: &mut Vec<NavPos>, current: Option<NavPos>, usable: impl Fn(&NavPos) -> bool) -> Option<NavPos> {
    while let Some(p) = from.pop() {
        if current.as_ref() == Some(&p) || !usable(&p) {
            continue;
        }
        if let Some(c) = current {
            push_capped(to, c);
        }
        return Some(p);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(pos: usize) -> NavPos {
        NavPos { id: 1, path: None, pos }
    }

    #[test]
    fn back_forward_and_a_new_push_clears_forward() {
        let mut r = NavRing::default();
        r.push(at(1));
        r.push(at(2));
        r.push(at(2));
        assert_eq!(r.sizes(), (2, 0), "a repeated position is kept once");
        // At 3: back goes to 2, then 1; 3 and 2 wait on the forward list.
        assert_eq!(r.back(Some(at(3)), |_| true), Some(at(2)));
        assert_eq!(r.back(Some(at(2)), |_| true), Some(at(1)));
        assert_eq!(r.back(Some(at(1)), |_| true), None);
        assert_eq!(r.forward(Some(at(1)), |_| true), Some(at(2)));
        assert_eq!(r.sizes(), (1, 1));
        // A new jump from 2 forgets 3.
        r.push(at(2));
        assert_eq!(r.sizes(), (2, 0));
        assert_eq!(r.forward(Some(at(5)), |_| true), None);
        // Unusable positions and the current one are skipped.
        r.push(at(7));
        assert_eq!(r.back(Some(at(7)), |p| p.pos != 2), Some(at(1)));
        assert_eq!(r.sizes(), (0, 1));
    }

    #[test]
    fn capped_at_100() {
        let mut r = NavRing::default();
        for i in 0..150 {
            r.push(at(i));
        }
        assert_eq!(r.sizes(), (NAV_CAP, 0));
        let mut seen = Vec::new();
        let mut cur = at(1000);
        while let Some(p) = r.back(Some(cur), |_| true) {
            seen.push(p.pos);
            cur = p;
        }
        assert_eq!(seen.first(), Some(&149));
        assert_eq!(seen.last(), Some(&50), "the oldest fell off");
        assert_eq!(r.sizes(), (0, NAV_CAP));
    }
}
