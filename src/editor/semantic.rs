//! LSP semantic tokens kept per buffer: capture names over char ranges,
//! sorted and non-overlapping, in the buffer's current coordinates. Every
//! edit shifts them ([`SemanticTokens::map_edit`]) until the next answer
//! replaces them. The editor view draws them over tree-sitter's spans.

use std::ops::Range;

/// A buffer's semantic tokens.
#[derive(Debug, Clone, Default)]
pub struct SemanticTokens {
    /// Char range and index into `names`, sorted by start.
    tokens: Vec<(Range<usize>, u32)>,
    /// The distinct capture names.
    names: Vec<String>,
    /// The buffer version the last answer was for, and the lines it covered
    /// (`None` = the whole document).
    fetched: Option<(u64, Option<Range<usize>>)>,
}

impl SemanticTokens {
    /// Replace the tokens: sorted by start, empty ranges and tokens that
    /// start inside the previous one dropped. Leaves `fetched` alone.
    pub fn set(&mut self, mut tokens: Vec<(Range<usize>, String)>) {
        tokens.sort_by_key(|(r, _)| r.start);
        self.tokens.clear();
        self.names.clear();
        let mut end = 0;
        for (r, name) in tokens {
            if r.start >= r.end || r.start < end {
                continue;
            }
            end = r.end;
            let i = match self.names.iter().position(|n| *n == name) {
                Some(i) => i,
                None => {
                    self.names.push(name);
                    self.names.len() - 1
                }
            };
            self.tokens.push((r, i as u32));
        }
    }

    /// Drop the tokens and what was fetched.
    pub fn clear(&mut self) {
        self.tokens.clear();
        self.names.clear();
        self.fetched = None;
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Record the buffer `version` an answer was for; `lines` is what a
    /// `/range` answer covered, `None` for the whole document.
    pub fn set_fetched(&mut self, version: u64, lines: Option<Range<usize>>) {
        self.fetched = Some((version, lines));
    }

    pub fn fetched(&self) -> Option<(u64, Option<Range<usize>>)> {
        self.fetched.clone()
    }

    /// Forget what was fetched, keeping the tokens (they show until the next
    /// answer).
    pub fn forget_fetch(&mut self) {
        self.fetched = None;
    }

    /// Follow an edit that replaced `old_len` chars at `at` with `new_len`.
    /// Per token, the first rule that applies: before the edit, unchanged;
    /// after it, shifted; around it (typing in or at the end of a token,
    /// replacing its text), its end moves; a partial overlap drops it.
    pub fn map_edit(&mut self, at: usize, old_len: usize, new_len: usize) {
        self.tokens.retain_mut(|(r, _)| {
            let (start, end) = (r.start, r.end);
            if end < at || (end == at && old_len > 0) {
                true
            } else if start >= at + old_len {
                r.start = (start + new_len) - old_len;
                r.end = (end + new_len) - old_len;
                true
            } else if start <= at && at + old_len <= end {
                r.end = (end + new_len) - old_len;
                r.start < r.end
            } else {
                false
            }
        });
    }

    /// The tokens meeting `chars`, in order, with their capture names.
    pub fn in_range(&self, chars: Range<usize>) -> impl Iterator<Item = (Range<usize>, &str)> {
        let first = self.tokens.partition_point(|(r, _)| r.end <= chars.start);
        self.tokens[first..]
            .iter()
            .take_while(move |(r, _)| r.start < chars.end)
            .map(|(r, i)| (r.clone(), self.names[*i as usize].as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SemanticTokens {
        let mut t = SemanticTokens::default();
        t.set(vec![(4..7, "variable".into()), (10..13, "function".into())]);
        t
    }

    fn all(t: &SemanticTokens) -> Vec<(Range<usize>, String)> {
        t.in_range(0..usize::MAX).map(|(r, n)| (r, n.to_string())).collect()
    }

    fn ranges(t: &SemanticTokens) -> Vec<Range<usize>> {
        all(t).into_iter().map(|(r, _)| r).collect()
    }

    #[test]
    fn set_sorts_and_drops_overlaps_and_empties() {
        let mut t = SemanticTokens::default();
        assert!(t.is_empty());
        t.set(vec![
            (10..13, "function".into()),
            (5..8, "x".into()),
            (4..7, "variable".into()),
            (20..20, "e".into()),
            (30..33, "variable".into()),
        ]);
        assert!(!t.is_empty());
        assert_eq!(all(&t), [(4..7, "variable".to_string()), (10..13, "function".to_string()), (30..33, "variable".to_string())]);
    }

    #[test]
    fn clear_and_fetched() {
        let mut t = base();
        assert_eq!(t.fetched(), None);
        t.set_fetched(3, Some(1..9));
        assert_eq!(t.fetched(), Some((3, Some(1..9))));
        t.set(vec![(1..2, "a".into())]);
        assert_eq!(t.fetched(), Some((3, Some(1..9))), "set leaves fetched alone");
        t.set_fetched(4, None);
        t.forget_fetch();
        assert_eq!(t.fetched(), None);
        assert!(!t.is_empty(), "forget_fetch keeps the tokens");
        t.set_fetched(4, None);
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.fetched(), None);
    }

    #[test]
    fn in_range_meets_the_window() {
        let t = base();
        let names = |r: Range<usize>| t.in_range(r).map(|(_, n)| n.to_string()).collect::<Vec<_>>();
        assert_eq!(names(5..11), ["variable", "function"]);
        assert!(names(7..10).is_empty());
        assert!(names(0..4).is_empty());
        assert_eq!(names(12..100), ["function"]);
    }

    #[test]
    fn map_edit_rules() {
        // (at, old_len, new_len, the tokens left as (start, end) pairs)
        type Case = (usize, usize, usize, &'static [(usize, usize)]);
        let cases: [Case; 9] = [
            (20, 0, 2, &[(4, 7), (10, 13)]),
            (0, 0, 2, &[(6, 9), (12, 15)]),
            (4, 0, 2, &[(6, 9), (12, 15)]),
            (5, 0, 1, &[(4, 8), (11, 14)]),
            (7, 0, 1, &[(4, 8), (11, 14)]),
            (4, 3, 5, &[(4, 9), (12, 15)]),
            (4, 3, 0, &[(7, 10)]),
            (6, 5, 0, &[]),
            (8, 2, 0, &[(4, 7), (8, 11)]),
        ];
        for (at, old, new, want) in cases {
            let mut t = base();
            t.map_edit(at, old, new);
            let got: Vec<(usize, usize)> = ranges(&t).into_iter().map(|r| (r.start, r.end)).collect();
            assert_eq!(got, want, "edit at {at} old {old} new {new}");
        }
    }
}
