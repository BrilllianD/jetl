//! The blame column and inline blame: author and date of the commit that last
//! touched each line, from `git blame` of the HEAD version (inline blame shows
//! the caret line's author, relative date and summary after its text). Lines are matched to HEAD
//! through the same diff as the gutter marks, so edits move the column
//! along with the text and edited lines show no commit.

use crate::git::model::{Blame, BlameCommit};
use crate::util::text::{line_count_inclusive, lines_inclusive};

/// Width of the column, separator included.
pub const WIDTH: u16 = 24;

/// Blame shown for a buffer.
#[derive(Debug, Default)]
pub struct BlameState {
    /// `None` until the blame arrives.
    pub data: Option<Blame>,
    /// Request that produced `data`; older requests are ignored.
    pub seq: u64,
    /// Buffer line -> HEAD line (`None`: changed since HEAD).
    pub map: Vec<Option<u32>>,
    /// Buffer version `map` was computed for.
    pub map_version: u64,
    /// The blame column is shown; `false` when only inline blame wanted the data.
    pub column: bool,
}

impl BlameState {
    pub fn new() -> Self {
        Self { map_version: u64::MAX, ..Default::default() }
    }

    /// The commit of buffer line `line`, if it is unchanged since HEAD.
    pub fn commit_at(&self, line: usize) -> Option<&BlameCommit> {
        let data = self.data.as_ref()?;
        // Before the map exists (no HEAD text yet) lines are taken as is.
        let head = if self.map.is_empty() { Some(line as u32) } else { *self.map.get(line)? };
        data.commits.get(*data.lines.get(head? as usize)? as usize)
    }
}

/// Buffer line -> HEAD line for `text` against its HEAD version `base`.
/// Buffer lines are the editor's (a lone `\r` breaks a line); HEAD lines
/// are git's, which count only `\n`.
pub fn line_map(base: &str, text: &str) -> Vec<Option<u32>> {
    let n = line_count_inclusive(text) + 1;
    // Git line of each base rope line: the `\n`s before its start.
    let mut git_line = Vec::new();
    let mut breaks = 0u32;
    for l in lines_inclusive(base) {
        git_line.push(breaks);
        breaks += l.matches('\n').count() as u32;
    }
    // Past the last line (the empty line after a final break) count on from
    // git's line total.
    let git_total = base.split_inclusive('\n').count() as u32;
    let git_of = |rope_line: u32| match git_line.get(rope_line as usize) {
        Some(&g) => g,
        None => git_total + rope_line - git_line.len() as u32,
    };
    let mut map = Vec::with_capacity(n);
    let (mut old, mut new) = (0u32, 0usize);
    for h in crate::git::diff::rope_hunks(base, text) {
        while new < h.after.start as usize {
            map.push(Some(git_of(old)));
            old += 1;
            new += 1;
        }
        map.extend(std::iter::repeat_n(None, h.after.len()));
        new = h.after.end as usize;
        old = h.before.end;
    }
    while new < n {
        map.push(Some(git_of(old)));
        old += 1;
        new += 1;
    }
    map
}

/// `author date` padded to the column width.
pub fn label(c: &BlameCommit) -> String {
    let w = WIDTH as usize - 1;
    let date = date(c.time, c.offset_minutes);
    let author_w = w - date.len() - 1;
    let author: String = c.author.chars().take(author_w).collect();
    let pad = author_w.saturating_sub(unicode_width::UnicodeWidthStr::width(author.as_str()));
    format!("{author}{} {date}", " ".repeat(pad))
}

/// `time` as a distance from `now`: `3 days ago`.
pub fn relative(secs: i64, now: i64) -> String {
    let d = (now - secs).max(0);
    let (n, unit) = match d {
        0..60 => return "just now".into(),
        60..3600 => (d / 60, "minute"),
        3600..86_400 => (d / 3600, "hour"),
        86_400..604_800 => (d / 86_400, "day"),
        604_800..2_592_000 => (d / 604_800, "week"),
        2_592_000..31_536_000 => (d / 2_592_000, "month"),
        _ => (d / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// The inline blame text: `author, relative date · summary`.
pub fn inline_text(c: &BlameCommit, now: i64) -> String {
    let head = format!("{}, {}", c.author, relative(c.time, now));
    if c.summary.is_empty() { head } else { format!("{head} · {}", c.summary) }
}

/// `text` cut to `width` cells; a cut that leaves a dangling ` ·` separator drops it.
pub fn fit_inline(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut used = 0;
    let mut end = text.len();
    for (i, ch) in text.char_indices() {
        used += ch.width().unwrap_or(0);
        if used > width {
            end = i;
            break;
        }
    }
    if end == text.len() {
        return text.to_string();
    }
    let cut = text[..end].trim_end();
    cut.strip_suffix('·').unwrap_or(cut).trim_end().to_string()
}

/// `YYYY-MM-DD` of a git time in its own time zone.
pub fn date(secs: i64, offset_minutes: i32) -> String {
    let (year, month, day) = civil((secs + i64::from(offset_minutes) * 60).div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// (year, month 1-12, day 1-31) of `days` since 1970-01-01.
pub fn civil(days: i64) -> (i64, i64, i64) {
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_inline_drops_a_dangling_separator() {
        let t = "Alice, just now · Fix it";
        assert_eq!(fit_inline(t, 80), t);
        assert_eq!(fit_inline(t, 17), "Alice, just now");
        assert_eq!(fit_inline(t, 16), "Alice, just now");
        assert_eq!(fit_inline(t, 15), "Alice, just now");
        assert_eq!(fit_inline(t, 19), "Alice, just now · F");
        assert_eq!(fit_inline(t, 8), "Alice, j");
    }

    #[test]
    fn edits_shift_lines_and_changed_lines_have_no_commit() {
        let base = "a\nb\nc\nd\n";
        // A line inserted at the top, "c" changed.
        let map = line_map(base, "new\na\nb\nC\nd\n");
        assert_eq!(&map[..5], &[None, Some(0), Some(1), None, Some(3)]);
        assert_eq!(line_map(base, base)[..4], [Some(0), Some(1), Some(2), Some(3)]);
        let blame = Blame {
            commits: vec![BlameCommit { sha: "s".into(), author: "Ann".into(), time: 0, offset_minutes: 0, summary: String::new() }],
            lines: vec![0; 4],
        };
        let state = BlameState { data: Some(blame), map, ..BlameState::new() };
        assert!(state.commit_at(0).is_none());
        assert_eq!(state.commit_at(1).map(|c| c.author.as_str()), Some("Ann"));
        assert!(state.commit_at(3).is_none());
    }

    #[test]
    fn relative_dates_and_inline_text() {
        assert_eq!(relative(100, 100), "just now");
        assert_eq!(relative(200, 100), "just now");
        assert_eq!(relative(0, 59), "just now");
        assert_eq!(relative(0, 60), "1 minute ago");
        assert_eq!(relative(0, 7200), "2 hours ago");
        assert_eq!(relative(0, 86_400), "1 day ago");
        assert_eq!(relative(0, 6 * 86_400), "6 days ago");
        assert_eq!(relative(0, 14 * 86_400), "2 weeks ago");
        assert_eq!(relative(0, 60 * 86_400), "2 months ago");
        assert_eq!(relative(0, 400 * 86_400), "1 year ago");
        let mut c = BlameCommit { sha: "s".into(), author: "Ann".into(), time: 0, offset_minutes: 0, summary: "Fix it".into() };
        assert_eq!(inline_text(&c, 86_400), "Ann, 1 day ago · Fix it");
        c.summary.clear();
        assert_eq!(inline_text(&c, 86_400), "Ann, 1 day ago");
    }

    #[test]
    fn dates_and_labels() {
        assert_eq!(date(0, 0), "1970-01-01");
        assert_eq!(date(1_700_000_000, 0), "2023-11-14");
        // 23:30 UTC on Nov 14 is already Nov 15 at +01:00.
        assert_eq!(date(1_700_004_600, 60), "2023-11-15");
        assert_eq!(date(951_782_400, 0), "2000-02-29");
        let c = BlameCommit { sha: "s".into(), author: "Alexander Somebody".into(), time: 0, offset_minutes: 0, summary: String::new() };
        let l = label(&c);
        assert_eq!(l, "Alexander So 1970-01-01");
        assert_eq!(l.chars().count(), WIDTH as usize - 1);
    }

    #[test]
    fn lone_cr_rope_lines_map_to_git_lines() {
        // "a\r" and "b\n" are both git line 0; "c\n" is git line 1.
        assert_eq!(line_map("a\rb\nc\n", "a\rb\nc\n")[..3], [Some(0), Some(0), Some(1)]);
    }

    #[test]
    fn line_map_counts_lines_like_the_rope() {
        let text = "new\na\u{c}b\nc\n";
        let map = line_map("a\u{c}b\nc\n", text);
        assert_eq!(&map[..3], &[None, Some(0), Some(1)]);
        let r = ropey::Rope::from_str(text);
        assert_eq!(r.line(1).to_string(), "a\u{c}b\n", "blame line 0");
        assert_eq!(r.len_lines(), 4);
        assert_eq!(r.len_lines(), line_count_inclusive(text) + 1);
        assert_eq!(map.len(), r.len_lines());
    }
}
