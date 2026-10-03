//! Diagnostics from `textDocument/publishDiagnostics`: the store (per file,
//! each publish replacing that file's list, capped and sorted, with a
//! `version` that every change bumps) and the marks a buffer draws from it
//! (char ranges in the current text, the worst severity per line, and
//! non-overlapping underline spans).
//!
//! The store keeps LSP ranges (UTF-16). They are converted against the
//! buffer's text when its marks are built, so they drift with edits made
//! after the publish until the server publishes again.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use lsp_types::{NumberOrString, Position};
use ratatui::style::Style;
use ropey::Rope;

use super::pos;
use crate::config::theme::Theme;
use crate::editor::selection::line_len_no_eol;

/// Diagnostics kept over all files; past this a publish keeps only what
/// fits (most severe first) and one note goes to the Event Log.
pub const MAX_TOTAL: usize = 20_000;

/// Most severe first, so sorting puts errors on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    /// A missing severity counts as an error, as in VS Code.
    pub fn of(s: Option<lsp_types::DiagnosticSeverity>) -> Self {
        match s {
            Some(lsp_types::DiagnosticSeverity::WARNING) => Severity::Warning,
            Some(lsp_types::DiagnosticSeverity::INFORMATION) => Severity::Info,
            Some(lsp_types::DiagnosticSeverity::HINT) => Severity::Hint,
            _ => Severity::Error,
        }
    }

    /// One cell wide: the gutter, the Problems window, the status bar.
    pub fn glyph(self) -> &'static str {
        match self {
            Severity::Error => "✖",
            Severity::Warning => "⚠",
            Severity::Info => "ℹ",
            Severity::Hint => "·",
        }
    }

    /// `diag_error`, `diag_warn`, `diag_info` or `diag_hint`.
    pub fn style(self, theme: &Theme) -> Style {
        match self {
            Severity::Error => theme.diag_error,
            Severity::Warning => theme.diag_warn,
            Severity::Info => theme.diag_info,
            Severity::Hint => theme.diag_hint,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        }
    }
}

/// One diagnostic as published, in LSP positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: lsp_types::Range,
    pub severity: Severity,
    pub message: String,
    /// `rustc`, `clippy`, `eslint`…
    pub source: Option<String>,
    pub code: Option<String>,
    /// What the server sent that is only given back to it (code actions
    /// ask with the diagnostics at the caret): the code as sent, and
    /// `data`.
    pub raw_code: Option<NumberOrString>,
    pub data: Option<serde_json::Value>,
}

impl Diagnostic {
    pub fn from_lsp(d: lsp_types::Diagnostic) -> Self {
        let code = d.code.as_ref().map(|c| match c {
            NumberOrString::Number(n) => n.to_string(),
            NumberOrString::String(s) => s.clone(),
        });
        Self {
            range: d.range,
            severity: Severity::of(d.severity),
            message: d.message,
            source: d.source,
            code,
            raw_code: d.code,
            data: d.data,
        }
    }

    /// The diagnostic in LSP terms again, for the server that sent it.
    pub fn to_lsp(&self) -> lsp_types::Diagnostic {
        let severity = match self.severity {
            Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
            Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
            Severity::Info => lsp_types::DiagnosticSeverity::INFORMATION,
            Severity::Hint => lsp_types::DiagnosticSeverity::HINT,
        };
        lsp_types::Diagnostic {
            range: self.range,
            severity: Some(severity),
            code: self.raw_code.clone(),
            source: self.source.clone(),
            message: self.message.clone(),
            data: self.data.clone(),
            ..Default::default()
        }
    }

    /// The first line of the message (they can span several).
    pub fn summary(&self) -> &str {
        self.message.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_end()
    }

    /// `rustc E0308`, `clippy`, or empty.
    pub fn origin(&self) -> String {
        match (&self.source, &self.code) {
            (Some(s), Some(c)) => format!("{s} {c}"),
            (Some(s), None) => s.clone(),
            (None, Some(c)) => c.clone(),
            (None, None) => String::new(),
        }
    }

    fn sort_key(&self) -> (u32, u32, Severity) {
        (self.range.start.line, self.range.start.character, self.severity)
    }
}

/// Counts by severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub errors: usize,
    pub warnings: usize,
    pub infos: usize,
    pub hints: usize,
}

impl Counts {
    pub fn of(items: &[Diagnostic]) -> Self {
        let mut c = Counts::default();
        for d in items {
            match d.severity {
                Severity::Error => c.errors += 1,
                Severity::Warning => c.warnings += 1,
                Severity::Info => c.infos += 1,
                Severity::Hint => c.hints += 1,
            }
        }
        c
    }
}

/// One file's diagnostics and the server that published them.
#[derive(Debug, Clone)]
struct FileDiagnostics {
    server: String,
    items: Vec<Diagnostic>,
}

/// Diagnostics of every file, by path.
#[derive(Debug, Default)]
pub struct DiagnosticStore {
    files: BTreeMap<PathBuf, FileDiagnostics>,
    total: usize,
    version: u64,
    /// The total cap was hit and reported; reset by `clear`.
    overflow_noted: bool,
}

impl DiagnosticStore {
    /// Bumped by every change; caches (buffer marks, the Problems rows)
    /// compare against it.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// `path`'s diagnostics, sorted by position.
    pub fn get(&self, path: &Path) -> &[Diagnostic] {
        self.files.get(path).map(|f| f.items.as_slice()).unwrap_or_default()
    }

    /// Files with diagnostics, by path, each with its sorted list.
    pub fn files(&self) -> impl Iterator<Item = (&Path, &[Diagnostic])> {
        self.files.iter().map(|(p, f)| (p.as_path(), f.items.as_slice()))
    }

    pub fn counts(&self, path: &Path) -> Counts {
        Counts::of(self.get(path))
    }

    pub fn total_counts(&self) -> Counts {
        let mut c = Counts::default();
        for f in self.files.values() {
            let n = Counts::of(&f.items);
            c.errors += n.errors;
            c.warnings += n.warnings;
            c.infos += n.infos;
            c.hints += n.hints;
        }
        c
    }

    /// Replace `path`'s diagnostics with what `server` published: at most
    /// `per_file` (and what is left of `MAX_TOTAL`), the most severe kept,
    /// sorted by position. True the first time the total cap cuts a list.
    pub fn publish(&mut self, server: &str, path: PathBuf, mut items: Vec<Diagnostic>, per_file: usize) -> bool {
        let old = self.files.remove(&path).map_or(0, |f| f.items.len());
        self.total -= old;
        let room = MAX_TOTAL.saturating_sub(self.total);
        let cap = per_file.min(room);
        let over_total = items.len() > room && room < per_file;
        if items.len() > cap {
            items.sort_by_key(|d| (d.severity, d.range.start.line, d.range.start.character));
            items.truncate(cap);
        }
        items.sort_by_key(Diagnostic::sort_key);
        if old > 0 || !items.is_empty() {
            self.version += 1;
        }
        self.total += items.len();
        if !items.is_empty() {
            self.files.insert(path, FileDiagnostics { server: server.to_string(), items });
        }
        let note = over_total && !self.overflow_noted;
        self.overflow_noted |= over_total;
        note
    }

    /// Forget what `server` published (it stopped or exited).
    pub fn clear_server(&mut self, server: &str) {
        let before = self.files.len();
        self.files.retain(|_, f| f.server != server);
        if self.files.len() != before {
            self.total = self.files.values().map(|f| f.items.len()).sum();
            self.version += 1;
        }
    }

    /// Forget everything (root switch, restart).
    pub fn clear(&mut self) {
        if !self.files.is_empty() {
            self.version += 1;
        }
        self.files.clear();
        self.total = 0;
        self.overflow_noted = false;
    }
}

/// A diagnostic placed in a buffer's current text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// Char range; at least one char unless the line is empty.
    pub range: Range<usize>,
    pub line: usize,
    pub severity: Severity,
    pub message: String,
}

/// A buffer's diagnostics as drawn, cached by store version, buffer
/// version and path.
#[derive(Debug, Clone, Default)]
pub struct DiagMarks {
    key: Option<(u64, u64, Option<PathBuf>)>,
    /// Sorted by start.
    pub marks: Vec<Mark>,
    /// The worst severity starting on each line (hints left out).
    pub lines: BTreeMap<usize, Severity>,
    /// Sorted, non-overlapping; the worst severity wins where they overlap.
    pub spans: Vec<(Range<usize>, Severity)>,
}

impl DiagMarks {
    /// Rebuild from `store` when it, the text or the path changed since
    /// the last call. True when rebuilt.
    pub fn sync(&mut self, store: &DiagnosticStore, rope: &Rope, version: u64, path: Option<&Path>) -> bool {
        let key = (store.version(), version, path.map(Path::to_path_buf));
        if self.key.as_ref() == Some(&key) {
            return false;
        }
        let items = path.map(|p| store.get(p)).unwrap_or_default();
        *self = Self::build(items, rope);
        self.key = Some(key);
        true
    }

    pub fn build(items: &[Diagnostic], rope: &Rope) -> Self {
        let mut marks: Vec<Mark> = items.iter().map(|d| place(d, rope)).collect();
        marks.sort_by_key(|m| (m.range.start, m.severity));
        let mut lines = BTreeMap::new();
        for m in marks.iter().filter(|m| m.severity != Severity::Hint) {
            lines.entry(m.line).and_modify(|s: &mut Severity| *s = (*s).min(m.severity)).or_insert(m.severity);
        }
        let spans = spans(&marks);
        Self { key: None, marks, lines, spans }
    }

    /// The worst severity underlining char `i`.
    pub fn severity_at(&self, i: usize) -> Option<Severity> {
        let k = self.spans.partition_point(|(r, _)| r.end <= i);
        self.spans.get(k).filter(|(r, _)| r.contains(&i)).map(|(_, s)| *s)
    }

    /// The most severe mark at the caret: its range holds `pos`, or ends
    /// right at it.
    pub fn at(&self, pos: usize) -> Option<&Mark> {
        self.marks.iter().filter(|m| m.range.start <= pos && pos <= m.range.end).min_by_key(|m| m.severity)
    }

    /// The start of the next (or previous) mark after (before) `pos`,
    /// wrapping around; hints are skipped.
    pub fn step(&self, pos: usize, forward: bool) -> Option<usize> {
        let mut starts: Vec<usize> = self.marks.iter().filter(|m| m.severity != Severity::Hint).map(|m| m.range.start).collect();
        starts.dedup();
        if forward {
            starts.iter().find(|&&s| s > pos).or(starts.first()).copied()
        } else {
            starts.iter().rev().find(|&&s| s < pos).or(starts.last()).copied()
        }
    }
}

/// `d` in `rope`'s chars. An empty range covers the char after it, or the
/// one before at the end of a line.
fn place(d: &Diagnostic, rope: &Rope) -> Mark {
    let start = pos::char_index(rope, d.range.start);
    let end = pos::char_index(rope, d.range.end).max(start);
    let line = rope.char_to_line(start);
    let mut range = start..end;
    if range.is_empty() {
        let line_start = rope.line_to_char(line);
        let line_end = line_start + line_len_no_eol(rope, line);
        if start < line_end {
            range = start..start + 1;
        } else if start > line_start {
            range = start - 1..start;
        }
    }
    Mark { range, line, severity: d.severity, message: d.summary().to_string() }
}

/// Non-overlapping underline spans of `marks`, the worst severity on top.
fn spans(marks: &[Mark]) -> Vec<(Range<usize>, Severity)> {
    let mut cuts: Vec<usize> = marks.iter().flat_map(|m| [m.range.start, m.range.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<(Range<usize>, Severity)> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let worst = marks.iter().filter(|m| m.range.start <= a && b <= m.range.end).map(|m| m.severity).min();
        let Some(s) = worst else { continue };
        match out.last_mut() {
            Some((r, last)) if r.end == a && *last == s => r.end = b,
            _ => out.push((a..b, s)),
        }
    }
    out
}

/// The position of a diagnostic as the Problems window shows it: 1-based
/// line and UTF-16 column.
pub fn display_pos(p: Position) -> String {
    format!("{}:{}", p.line + 1, p.character + 1)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn diag(l0: u32, c0: u32, l1: u32, c1: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            range: lsp_types::Range::new(Position::new(l0, c0), Position::new(l1, c1)),
            severity,
            message: message.into(),
            source: None,
            code: None,
            raw_code: None,
            data: None,
        }
    }

    #[test]
    fn publish_replaces_sorts_and_bumps_the_version() {
        let mut s = DiagnosticStore::default();
        let p = PathBuf::from("/r/a.rs");
        assert_eq!(s.version(), 0);
        let items =
            vec![diag(3, 0, 3, 1, Severity::Warning, "w"), diag(1, 4, 1, 5, Severity::Hint, "h"), diag(1, 4, 1, 5, Severity::Error, "e")];
        assert!(!s.publish("ra", p.clone(), items, 1000));
        assert_eq!(s.version(), 1);
        let msgs: Vec<&str> = s.get(&p).iter().map(|d| d.message.as_str()).collect();
        assert_eq!(msgs, ["e", "h", "w"], "by position, then severity");
        assert_eq!(s.counts(&p), Counts { errors: 1, warnings: 1, infos: 0, hints: 1 });
        // A publish replaces the file's list.
        s.publish("ra", p.clone(), vec![diag(0, 0, 0, 1, Severity::Info, "i")], 1000);
        assert_eq!(s.get(&p).len(), 1);
        assert_eq!((s.version(), s.total()), (2, 1));
        // An empty publish removes the file; publishing nothing for a file
        // without diagnostics changes nothing.
        s.publish("ra", p.clone(), vec![], 1000);
        assert_eq!((s.version(), s.total(), s.files().count()), (3, 0, 0));
        s.publish("ra", p.clone(), vec![], 1000);
        assert_eq!(s.version(), 3);
    }

    #[test]
    fn caps_keep_the_most_severe() {
        let mut s = DiagnosticStore::default();
        let p = PathBuf::from("/r/a.rs");
        let items = (0..10).map(|i| diag(i, 0, i, 1, if i == 9 { Severity::Error } else { Severity::Hint }, "x")).collect();
        s.publish("ra", p.clone(), items, 3);
        let lines: Vec<u32> = s.get(&p).iter().map(|d| d.range.start.line).collect();
        assert_eq!(lines, [0, 1, 9], "the error survives the cap, the list stays sorted");
        // The total cap: one note, the first time only.
        let many = |n: u32| (0..n).map(|i| diag(i, 0, i, 1, Severity::Warning, "x")).collect::<Vec<_>>();
        let mut notes = 0;
        for f in 0..25 {
            notes += usize::from(s.publish("ra", PathBuf::from(format!("/r/{f}.rs")), many(1000), 1000));
        }
        assert_eq!(s.total(), MAX_TOTAL);
        assert_eq!(notes, 1);
        // Republishing a full file reuses its own room.
        s.publish("ra", PathBuf::from("/r/0.rs"), many(1000), 1000);
        assert_eq!((s.total(), s.get(Path::new("/r/0.rs")).len()), (MAX_TOTAL, 1000));
        s.clear();
        assert_eq!((s.total(), s.files().count()), (0, 0));
        s.publish("ra", PathBuf::from("/r/a.rs"), many(1000), 1000);
        assert!(s.publish("ra", PathBuf::from("/r/b.rs"), many(MAX_TOTAL as u32), MAX_TOTAL), "noted again after a clear");
    }

    #[test]
    fn clearing_a_server_keeps_the_others() {
        let mut s = DiagnosticStore::default();
        s.publish("ra", PathBuf::from("/r/a.rs"), vec![diag(0, 0, 0, 1, Severity::Error, "a")], 10);
        s.publish("py", PathBuf::from("/r/b.py"), vec![diag(0, 0, 0, 1, Severity::Error, "b")], 10);
        let v = s.version();
        s.clear_server("ra");
        assert_eq!(s.files().map(|(p, _)| p.to_path_buf()).collect::<Vec<_>>(), [PathBuf::from("/r/b.py")]);
        assert_eq!((s.version(), s.total()), (v + 1, 1));
        s.clear_server("ra");
        assert_eq!(s.version(), v + 1, "nothing to clear, no bump");
        s.clear();
        assert_eq!((s.version(), s.total()), (v + 2, 0));
    }

    #[test]
    fn marks_in_chars_with_lines_spans_and_steps() {
        // 😀 is two UTF-16 units: column 3 is the `x` after it.
        let rope = Rope::from_str("a😀xyz\n\nfn f() {}\n");
        let items = [
            diag(0, 3, 0, 5, Severity::Warning, "warn\nmore"),
            diag(0, 4, 0, 6, Severity::Error, "err"),
            diag(1, 0, 1, 0, Severity::Info, "empty line"),
            diag(2, 9, 2, 9, Severity::Hint, "at the end"),
        ];
        let m = DiagMarks::build(&items, &rope);
        assert_eq!(m.marks[0], Mark { range: 2..4, line: 0, severity: Severity::Warning, message: "warn".into() });
        assert_eq!(m.marks[1].range, 3..5);
        assert_eq!(m.marks[2].range, 6..6, "nothing to underline on an empty line");
        assert_eq!(m.marks[3].range, 15..16, "an empty range at the end of a line takes the char before");
        assert_eq!(m.lines.iter().map(|(l, s)| (*l, *s)).collect::<Vec<_>>(), [(0, Severity::Error), (1, Severity::Info)]);
        assert_eq!(m.spans, [(2..3, Severity::Warning), (3..5, Severity::Error), (15..16, Severity::Hint)]);
        assert_eq!(
            (m.severity_at(1), m.severity_at(2), m.severity_at(4), m.severity_at(5)),
            (None, Some(Severity::Warning), Some(Severity::Error), None)
        );
        assert_eq!(m.at(3).map(|m| m.message.as_str()), Some("err"));
        // Hints are skipped; stepping wraps both ways.
        assert_eq!(m.step(0, true), Some(2));
        assert_eq!(m.step(2, true), Some(3));
        assert_eq!(m.step(6, true), Some(2));
        assert_eq!(m.step(2, false), Some(6));
        assert_eq!(m.step(5, false), Some(3));
        assert_eq!(DiagMarks::default().step(0, true), None);
    }

    #[test]
    fn marks_are_cached_by_store_and_buffer_version() {
        let mut s = DiagnosticStore::default();
        let p = PathBuf::from("/r/a.rs");
        let rope = Rope::from_str("abc\n");
        let mut m = DiagMarks::default();
        assert!(m.sync(&s, &rope, 1, Some(&p)));
        assert!(!m.sync(&s, &rope, 1, Some(&p)));
        s.publish("ra", p.clone(), vec![diag(0, 1, 0, 2, Severity::Error, "e")], 10);
        assert!(m.sync(&s, &rope, 1, Some(&p)));
        assert_eq!(m.marks.len(), 1);
        assert!(m.sync(&s, &rope, 2, Some(&p)), "the text changed");
        assert!(m.sync(&s, &rope, 2, Some(Path::new("/r/b.rs"))), "the path changed");
        assert!(m.marks.is_empty());
    }
}
