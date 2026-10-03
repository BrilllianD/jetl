//! The Log tab's commit graph, laid out from each commit's parents
//! (`%P`) instead of `git log --graph`, so pages fetched with `--skip`
//! continue the lanes of the page before.
//!
//! Commits arrive newest first (`--date-order`: no parent before its
//! children). Each lane waits for one commit; a commit takes the lane that
//! waits for it, other lanes waiting for it end there, and its parents
//! take its lane (the first parent) or other lanes. A row is two columns
//! per lane: the lane's glyph, then a gap that carries horizontal lines.

/// Lanes carried from row to row.
#[derive(Debug, Clone, Default)]
pub struct LogGraph {
    /// The commit each lane waits for; `None` is a free lane.
    lanes: Vec<Option<String>>,
}

impl LogGraph {
    /// The graph cells of the next commit, and the lanes after it.
    pub fn row(&mut self, sha: &str, parents: &[String]) -> String {
        let before = self.lanes.clone();
        let waiting: Vec<usize> = (0..before.len()).filter(|&i| before[i].as_deref() == Some(sha)).collect();
        let c = match waiting.first() {
            Some(&c) => c,
            None => self.free_lane(&before),
        };
        // Other lanes waiting for this commit end here.
        let mut ends: Vec<usize> = waiting.iter().copied().filter(|&j| j != c).collect();
        for &j in &ends {
            self.lanes[j] = None;
        }
        self.lanes[c] = None;
        // Parents: the first continues below the commit; a lane to the
        // right that waits for it already ends here instead, which keeps
        // lines to the left. Other parents join a waiting lane or open one.
        let mut forks = Vec::new();
        for (k, p) in parents.iter().enumerate() {
            match self.lanes.iter().position(|l| l.as_deref() == Some(p.as_str())) {
                Some(j) if k == 0 && j > c => {
                    self.lanes[j] = None;
                    self.lanes[c] = Some(p.clone());
                    ends.push(j);
                }
                Some(j) => forks.push(j),
                None if k == 0 => self.lanes[c] = Some(p.clone()),
                None => {
                    let j = self.free_lane(&before);
                    self.lanes[j] = Some(p.clone());
                    forks.push(j);
                }
            }
        }
        let cells = render(&before, c, &ends, &forks, &self.lanes);
        while self.lanes.last().is_some_and(Option::is_none) {
            self.lanes.pop();
        }
        cells
    }

    /// A lane free now and before this row (so no line runs through it),
    /// or a new one.
    fn free_lane(&mut self, before: &[Option<String>]) -> usize {
        let free = (0..self.lanes.len()).find(|&i| self.lanes[i].is_none() && before.get(i).is_none_or(Option::is_none));
        free.unwrap_or_else(|| {
            self.lanes.push(None);
            self.lanes.len() - 1
        })
    }
}

/// Draw one row: `c` is the commit's lane, `ends` lanes that end in it
/// (from above), `forks` lanes its parents continue in (below).
fn render(before: &[Option<String>], c: usize, ends: &[usize], forks: &[usize], after: &[Option<String>]) -> String {
    let width = before.len().max(after.len()).max(c + 1);
    let lo = ends.iter().chain(forks).copied().chain([c]).min().unwrap_or(c);
    let hi = ends.iter().chain(forks).copied().chain([c]).max().unwrap_or(c);
    let up = |i: usize| before.get(i).is_some_and(Option::is_some);
    let down = |i: usize| after.get(i).is_some_and(Option::is_some);
    let mut out = String::new();
    for i in 0..width {
        // Lines meet the horizontal from above (ends), below (forks) or
        // both; at the span's ends the horizontal stops.
        let at = usize::from(i > lo) + usize::from(i == hi);
        let glyph = if i == c {
            '●'
        } else if ends.contains(&i) {
            ['└', '┴', '┘'][at]
        } else if forks.contains(&i) {
            if up(i) && down(i) { ['├', '┼', '┤'][at] } else { ['┌', '┬', '┐'][at] }
        } else if (lo..=hi).contains(&i) {
            if up(i) && down(i) { '┼' } else { '─' }
        } else if up(i) || down(i) {
            '│'
        } else {
            ' '
        };
        out.push(glyph);
        out.push(if i >= lo && i < hi { '─' } else { ' ' });
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(commits: &[(&str, &[&str])]) -> Vec<String> {
        let mut g = LogGraph::default();
        commits
            .iter()
            .map(|(sha, parents)| {
                let parents: Vec<String> = parents.iter().map(|p| p.to_string()).collect();
                g.row(sha, &parents)
            })
            .collect()
    }

    #[test]
    fn linear_history_is_one_lane() {
        assert_eq!(rows(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]), ["●", "●", "●"]);
    }

    #[test]
    fn merge_opens_a_lane_and_the_fork_point_closes_it() {
        // m merges a (first parent) and b; both come from base.
        let r = rows(&[("m", &["a", "b"]), ("b", &["base"]), ("a", &["base"]), ("base", &[])]);
        assert_eq!(r, ["●─┐", "│ ●", "●─┘", "●"]);
    }

    #[test]
    fn a_first_parent_waited_for_elsewhere_joins_that_lane() {
        // Two tips (x, y) on top of base; x's lane joins y's lane at base.
        let r = rows(&[("y", &["base"]), ("x", &["base"]), ("base", &[])]);
        assert_eq!(r, ["●", "├─●", "●"]);
    }

    #[test]
    fn pages_continue_the_lanes() {
        // The same commits in two calls give the same rows as one.
        let mut g = LogGraph::default();
        let s = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let first = [g.row("m", &s(&["a", "b"])), g.row("b", &s(&["base"]))];
        let second = [g.row("a", &s(&["base"])), g.row("base", &s(&[]))];
        assert_eq!([first.as_slice(), second.as_slice()].concat(), ["●─┐", "│ ●", "●─┘", "●"]);
    }

    #[test]
    fn a_merge_closing_one_lane_and_opening_another() {
        // Lane 1 waits for m too (a branch forked from it); m's second
        // parent opens lane 2.
        let mut g = LogGraph { lanes: vec![Some("m".into()), Some("m".into())] };
        assert_eq!(g.row("m", &["a".into(), "b".into()]), "●─┴─┐");
    }

    #[test]
    fn lines_cross_lanes_they_pass() {
        // Lane 1 waits for "p" while m (lane 0) opens lane 2 for "q".
        let mut g = LogGraph { lanes: vec![Some("m".into()), Some("p".into())] };
        assert_eq!(g.row("m", &["x".into(), "q".into()]), "●─┼─┐");
    }
}
