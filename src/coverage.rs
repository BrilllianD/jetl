//! Line coverage from an lcov report (`[coverage] lcov`; cargo llvm-cov,
//! pytest-cov, c8). Only `SF:` and `DA:` records are read. `parse` is pure;
//! `load` reads the file (blocking, run it in `spawn_blocking`) and marks a
//! source file stale when it is newer than the report. The gutter paints
//! the state of a line span in the line numbers (`editor::view::render`).

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use ratatui::style::Style;

use crate::config::theme::Theme;
use crate::util::fs::normalize;

/// A report above this size is not read.
pub const MAX_LCOV_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCoverage {
    Covered,
    Uncovered,
}

impl LineCoverage {
    /// The line-number style of this state: `line_number` with only the
    /// foreground of the theme's coverage key (no bg, bold or other modifier).
    pub fn number_style(self, theme: &Theme) -> Style {
        let key = match self {
            LineCoverage::Covered => theme.coverage_covered,
            LineCoverage::Uncovered => theme.coverage_uncovered,
        };
        key.fg.map_or(theme.line_number, |fg| theme.line_number.fg(fg))
    }
}

/// The hits of one source file; `stale` once the file changed after the report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileCoverage {
    /// 0-based line to hit count.
    hits: BTreeMap<usize, u64>,
    pub stale: bool,
}

impl FileCoverage {
    pub fn hits(&self, line: usize) -> Option<u64> {
        self.hits.get(&line).copied()
    }

    /// `Uncovered` if a listed line in `lines` has 0 hits, else `Covered`
    /// if one has hits, else `None`.
    pub fn state(&self, lines: RangeInclusive<usize>) -> Option<LineCoverage> {
        let mut any = false;
        for (_, &h) in self.hits.range(lines) {
            if h == 0 {
                return Some(LineCoverage::Uncovered);
            }
            any = true;
        }
        any.then_some(LineCoverage::Covered)
    }

    fn merge(&mut self, other: FileCoverage) {
        for (line, h) in other.hits {
            let e = self.hits.entry(line).or_insert(0);
            *e = e.saturating_add(h);
        }
    }
}

/// The files of one report, keyed by normalised (after `load`: canonical) path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    files: HashMap<PathBuf, FileCoverage>,
}

impl Coverage {
    /// Read the `SF:` and `DA:` records of `text`; a relative `SF:` path is
    /// joined to `root`. Malformed records are skipped.
    pub fn parse(text: &str, root: &Path) -> Coverage {
        let mut files: HashMap<PathBuf, FileCoverage> = HashMap::new();
        let mut current: Option<PathBuf> = None;
        for l in text.lines() {
            let l = l.trim();
            if let Some(rest) = l.strip_prefix("SF:") {
                let p = Path::new(rest);
                let full = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
                let key = normalize(&full);
                files.entry(key.clone()).or_default();
                current = Some(key);
            } else if let Some(rest) = l.strip_prefix("DA:") {
                let Some(key) = &current else { continue };
                let mut fields = rest.split(',');
                let (Some(line), Some(hits)) = (fields.next(), fields.next()) else { continue };
                let (Ok(line), Ok(hits)) = (line.trim().parse::<usize>(), hits.trim().parse::<u64>()) else { continue };
                if line == 0 {
                    continue;
                }
                if let Some(f) = files.get_mut(key) {
                    let e = f.hits.entry(line - 1).or_insert(0);
                    *e = e.saturating_add(hits);
                }
            } else if l == "end_of_record" {
                current = None;
            }
        }
        Coverage { files }
    }

    /// The coverage of `path`; `None` when the report does not list it or it is stale.
    pub fn file(&self, path: &Path) -> Option<&FileCoverage> {
        self.files.get(path).filter(|f| !f.stale)
    }

    /// Hide `path` until the report is loaded again; true when the report listed it.
    pub fn mark_stale(&mut self, path: &Path) -> bool {
        match self.files.get_mut(path) {
            Some(f) => {
                f.stale = true;
                true
            }
            None => false,
        }
    }

    /// The number of files the report lists.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Read and parse the report `file` (relative `SF:` paths are joined to
/// `root`). Keys become canonical paths; a source file whose mtime is newer
/// than the report's (or unreadable) is stale. Blocking. `Ok(None)`: no such file.
pub fn load(file: &Path, root: &Path) -> Result<Option<Coverage>, String> {
    let meta = match std::fs::metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if meta.len() > MAX_LCOV_BYTES {
        return Err(format!("{} MiB is above the limit of {} MiB", meta.len() >> 20, MAX_LCOV_BYTES >> 20));
    }
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let lcov_mtime = meta.modified().ok();
    let parsed = Coverage::parse(&String::from_utf8_lossy(&bytes), root);
    let mut files: HashMap<PathBuf, FileCoverage> = HashMap::new();
    for (path, fc) in parsed.files {
        let key = std::fs::canonicalize(&path).unwrap_or(path);
        files.entry(key).or_default().merge(fc);
    }
    for (key, f) in files.iter_mut() {
        let src = std::fs::metadata(key).and_then(|m| m.modified());
        f.stale = match (src, lcov_mtime) {
            (Ok(t), Some(l)) => t > l,
            _ => true,
        };
    }
    Ok(Some(Coverage { files }))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    const FIXTURE: &str = "TN:\nSF:src/a.rs\nFN:1,main\nFNDA:1,main\nDA:1,3\nDA:2,0\nDA:4,1,abcd\nBRDA:4,0,0,1\nLF:3\nLH:2\nend_of_record\nSF:/abs/b.rs\nDA:10,0\nend_of_record\n";

    fn hits_of(c: &Coverage, path: &str) -> Vec<(usize, u64)> {
        c.files[Path::new(path)].hits.iter().map(|(l, h)| (*l, *h)).collect()
    }

    fn set_mtime(p: &Path, t: SystemTime) {
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-coverage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn parse_reads_sf_and_da_records() {
        let c = Coverage::parse(FIXTURE, Path::new("/r"));
        assert_eq!(c.len(), 2);
        assert_eq!(hits_of(&c, "/r/src/a.rs"), vec![(0, 3), (1, 0), (3, 1)]);
        let a = c.file(Path::new("/r/src/a.rs")).unwrap();
        assert_eq!(a.hits(2), None);
        assert_eq!(a.hits(0), Some(3));
        assert_eq!(hits_of(&c, "/abs/b.rs"), vec![(9, 0)]);
    }

    #[test]
    fn parse_sums_repeated_files_and_lines() {
        let text =
            "SF:src/a.rs\nDA:2,0\nDA:1,1\nDA:1,1\nend_of_record\nSF:src/a.rs\nDA:2,5\nDA:3,18446744073709551615\nDA:3,1\nend_of_record\n";
        let c = Coverage::parse(text, Path::new("/r"));
        assert_eq!(c.len(), 1);
        assert_eq!(hits_of(&c, "/r/src/a.rs"), vec![(0, 2), (1, 5), (2, u64::MAX)]);
        let a = c.file(Path::new("/r/src/a.rs")).unwrap();
        assert_eq!(a.state(1..=1), Some(LineCoverage::Covered));
    }

    #[test]
    fn parse_skips_malformed_records() {
        let text = "DA:5,1\nSF:./src/../src/a.rs\nDA:x,1\nDA:0,1\nDA:3,-1\nDA:4\nDA:2,7\nend_of_record\nDA:9,9\n";
        let c = Coverage::parse(text, Path::new("/r"));
        assert_eq!(c.len(), 1);
        assert_eq!(hits_of(&c, "/r/src/a.rs"), vec![(1, 7)]);
        let crlf = Coverage::parse(&FIXTURE.replace('\n', "\r\n"), Path::new("/r"));
        assert_eq!(crlf, Coverage::parse(FIXTURE, Path::new("/r")));
    }

    #[test]
    fn parse_lists_a_file_without_lines() {
        let c = Coverage::parse("SF:/x.rs\nend_of_record\n", Path::new("/r"));
        assert_eq!(c.len(), 1);
        assert!(!c.is_empty());
        assert_eq!(c.file(Path::new("/x.rs")).unwrap().state(0..=100), None);
    }

    #[test]
    fn state_over_a_span() {
        let c = Coverage::parse(FIXTURE, Path::new("/r"));
        let a = c.file(Path::new("/r/src/a.rs")).unwrap();
        assert_eq!(a.state(0..=0), Some(LineCoverage::Covered));
        assert_eq!(a.state(0..=1), Some(LineCoverage::Uncovered));
        assert_eq!(a.state(2..=2), None);
        assert_eq!(a.state(2..=3), Some(LineCoverage::Covered));
        assert_eq!(a.state(5..=9), None);
    }

    #[test]
    fn file_hides_stale_entries() {
        let mut c = Coverage::parse(FIXTURE, Path::new("/r"));
        assert!(c.file(Path::new("/r/src/a.rs")).is_some());
        assert!(c.mark_stale(Path::new("/r/src/a.rs")));
        assert!(c.file(Path::new("/r/src/a.rs")).is_none());
        assert!(c.file(Path::new("/abs/b.rs")).is_some());
        assert!(!c.mark_stale(Path::new("/nope.rs")));
    }

    #[test]
    fn load_missing_is_none_and_too_big_errs() {
        let dir = temp("missing");
        assert_eq!(load(&dir.join("nope.info"), &dir), Ok(None));
        let big = dir.join("big.info");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_LCOV_BYTES + 1).unwrap();
        assert_eq!(load(&big, &dir), Err("256 MiB is above the limit of 256 MiB".to_string()));
        assert!(load(&dir, &dir).is_err(), "a directory is not a report");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_marks_newer_sources_stale() {
        let dir = temp("stale");
        std::fs::write(dir.join("a.rs"), "a\n").unwrap();
        std::fs::write(dir.join("b.rs"), "b\n").unwrap();
        let lcov = dir.join("lcov.info");
        std::fs::write(&lcov, "SF:a.rs\nDA:1,1\nend_of_record\nSF:b.rs\nDA:1,1\nend_of_record\nSF:gone.rs\nDA:1,1\nend_of_record\n")
            .unwrap();
        let t = SystemTime::now() - Duration::from_secs(100);
        set_mtime(&dir.join("a.rs"), t);
        set_mtime(&lcov, t);
        set_mtime(&dir.join("b.rs"), t + Duration::from_secs(10));
        let c = load(&lcov, &dir).unwrap().unwrap();
        assert_eq!(c.len(), 3);
        assert!(c.file(&dir.join("a.rs")).is_some(), "equal mtime is fresh");
        assert!(c.file(&dir.join("b.rs")).is_none(), "newer than the report");
        assert!(c.file(&dir.join("gone.rs")).is_none(), "a listed file that does not exist");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_canonicalises_and_merges_keys() {
        let dir = temp("canon");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/a.rs"), "a\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let lcov = dir.join("lcov.info");
        std::fs::write(&lcov, "SF:link/a.rs\nDA:1,1\nend_of_record\nSF:./real/a.rs\nDA:1,2\nDA:2,0\nend_of_record\n").unwrap();
        let t = SystemTime::now();
        set_mtime(&dir.join("real/a.rs"), t - Duration::from_secs(10));
        set_mtime(&lcov, t);
        let c = load(&lcov, &dir).unwrap().unwrap();
        assert_eq!(c.len(), 1);
        let a = c.file(&dir.join("real/a.rs")).expect("keyed by the canonical path");
        assert_eq!((a.hits(0), a.hits(1)), (Some(3), Some(0)));
        assert!(c.file(&dir.join("link/a.rs")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn number_style_takes_only_the_fg() {
        use ratatui::style::{Color, Modifier};
        let theme = Theme {
            coverage_covered: Style::default().fg(Color::Rgb(1, 2, 3)).bg(Color::Rgb(9, 9, 9)).add_modifier(Modifier::BOLD),
            coverage_uncovered: Style::default().bg(Color::Rgb(9, 9, 9)),
            ..Theme::default()
        };
        assert_eq!(LineCoverage::Covered.number_style(&theme), theme.line_number.fg(Color::Rgb(1, 2, 3)));
        assert_eq!(LineCoverage::Uncovered.number_style(&theme), theme.line_number);
    }
}
