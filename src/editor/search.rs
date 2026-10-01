//! Find/replace state (filled in phase 2).

#[derive(Debug, Default)]
pub struct SearchState {
    pub matches: Vec<std::ops::Range<usize>>,
    pub current: Option<usize>,
}

impl SearchState {
    /// Some(true) when inside the current match, Some(false) in another match.
    pub fn match_at(&self, char_idx: usize) -> Option<bool> {
        let i = self.matches.partition_point(|r| r.end <= char_idx);
        let r = self.matches.get(i)?;
        if r.contains(&char_idx) { Some(self.current == Some(i)) } else { None }
    }
}
