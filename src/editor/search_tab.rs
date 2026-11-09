//! The Find in Files results tab: a pathless, read-only, always clean
//! `Buffer` whose rope is the results text (`layout`), painted through the
//! same highlight slot as the Markdown preview. Enter and a double click on
//! a result open it; F5 searches again (`App::search_tab_rerun`).
//!
//! The tab runs its own `Search` with its own seq and cancel handle. The
//! rows shown stay until a run's last batch arrives, then the text is
//! replaced in one go (`Buffer::set_preview_text`), so the tab is never
//! dirty and is never in the session, autosave, recovery or hot exit. The
//! results are a snapshot: file changes do not touch them until a re-run.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::layout::Position;
use ratatui::style::{Modifier, Style};

use super::buffer::BufferId;
use super::syntax::Highlights;
use super::{Buffer, Editor};
use crate::actions::EditorAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::files::find::{Batch, Hit, MAX_HITS, Search, SearchSpec};

/// Chars before a preview on a hit line: the right-aligned line number, a
/// colon and a space.
const PREFIX_CHARS: usize = 8;

/// Where a tab's search stands.
#[derive(Debug)]
pub(crate) enum RunState {
    Searching,
    Done { files_searched: usize, capped: bool },
    Failed(String),
}

/// What a span of the results text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Paint {
    Header,
    Status(Option<Level>),
    File,
    LineNo,
    Match,
}

/// What Enter opens on a line of the results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowTarget {
    /// A file line: the file's first hit.
    File { hit: usize },
    /// A hit line: its matches as char ranges from the line start (prefix
    /// included) and the index of each one's hit.
    Hit { matches: Vec<(Range<usize>, usize)> },
}

/// The results text, its spans and the targets of its lines.
#[derive(Debug)]
pub(crate) struct Layout {
    pub text: String,
    /// Sorted, non-overlapping char ranges of `text`.
    pub spans: Vec<(Range<usize>, Paint)>,
    /// Rows with a target, sorted by rope line; indexes point into the hits given to `layout`.
    pub rows: Vec<(usize, RowTarget)>,
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// A layout under construction: the char and line counts of its text so far.
struct Builder {
    out: Layout,
    chars: usize,
    line: usize,
}

impl Builder {
    /// Append one line; returns its char range without the `\n`.
    fn push(&mut self, s: &str) -> Range<usize> {
        let start = self.chars;
        let n = s.chars().count();
        self.out.text.push_str(s);
        self.out.text.push('\n');
        self.chars += n + 1;
        self.line += 1;
        start..start + n
    }
}

/// The results text for `hits` (in path order) of a run of `spec` from `root`.
pub(crate) fn layout(spec: &SearchSpec, root: &Path, hits: &[Hit], state: &RunState) -> Layout {
    let mut b = Builder { out: Layout { text: String::new(), spans: Vec::new(), rows: Vec::new() }, chars: 0, line: 0 };

    let mut header = format!("Find in Files: {}", spec.text);
    let options = spec.options();
    if !options.is_empty() {
        header.push_str(&format!("   [{options}]"));
    }
    let r = b.push(&header);
    b.out.spans.push((r, Paint::Header));

    let files = hits.iter().enumerate().filter(|(i, h)| *i == 0 || hits[i - 1].path != h.path).count();
    let n = hits.len();
    let (status, level) = match state {
        RunState::Searching => ("Searching…".to_string(), None),
        RunState::Failed(e) => (format!("Invalid pattern: {e}"), Some(Level::Error)),
        RunState::Done { files_searched, .. } if n == 0 => {
            (format!("No matches ({files_searched} files searched); F5 searches again"), Some(Level::Warn))
        }
        RunState::Done { capped: true, .. } => (
            format!(
                "{n}+ matches in {files} {}; stopped at {MAX_HITS}; Enter opens a result, F5 searches again",
                plural(files, "file", "files")
            ),
            Some(Level::Warn),
        ),
        RunState::Done { .. } => (
            format!(
                "{n} {} in {files} {}; Enter opens a result, F5 searches again",
                plural(n, "match", "matches"),
                plural(files, "file", "files")
            ),
            None,
        ),
    };
    let r = b.push(&status);
    b.out.spans.push((r, Paint::Status(level)));
    b.push("");

    let mut i = 0;
    while i < hits.len() {
        // One file block: the run of hits with this path.
        let path = &hits[i].path;
        let end = (i..hits.len()).find(|&j| hits[j].path != *path).unwrap_or(hits.len());
        if i > 0 {
            b.push("");
        }
        let name = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
        b.out.rows.push((b.line, RowTarget::File { hit: i }));
        let r = b.push(&name);
        b.out.spans.push((r, Paint::File));
        let mut j = i;
        while j < end {
            // One hit line: the hits that share a source line.
            let h = &hits[j];
            let same = |k: usize| hits[k].line == h.line && hits[k].preview == h.preview;
            let last = (j..end).take_while(|&k| same(k)).last().unwrap_or(j);
            let preview: String = h.preview.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
            let preview_len = preview.chars().count();
            let text = format!("{:>6}: {preview}", h.line + 1);
            let start = b.chars;
            b.out.rows.push((b.line, RowTarget::Hit { matches: Vec::new() }));
            let mut matches = Vec::new();
            let mut merged: Vec<Range<usize>> = Vec::new();
            for (k, hit) in hits.iter().enumerate().take(last + 1).skip(j) {
                let s = (hit.preview_col + PREFIX_CHARS).min(PREFIX_CHARS + preview_len);
                let e = (hit.preview_col + hit.len + PREFIX_CHARS).min(PREFIX_CHARS + preview_len);
                matches.push((s..e, k));
                if s < e {
                    match merged.last_mut() {
                        Some(m) if s <= m.end => m.end = m.end.max(e),
                        _ => merged.push(s..e),
                    }
                }
            }
            b.out.spans.push((start..start + PREFIX_CHARS, Paint::LineNo));
            b.out.spans.extend(merged.into_iter().map(|m| (start + m.start..start + m.end, Paint::Match)));
            if let Some((_, RowTarget::Hit { matches: slot })) = b.out.rows.last_mut() {
                *slot = matches;
            }
            b.push(&text);
            j = last + 1;
        }
        i = end;
    }
    b.out
}

/// What a results tab holds: its search and the rows it shows.
#[derive(Debug)]
pub(crate) struct SearchTab {
    pub(crate) spec: SearchSpec,
    root: PathBuf,
    /// The run in progress (or last); batches of any other seq are stale.
    seq: u64,
    /// Holds the newest seq; a walk whose seq differs stops.
    cancel: Arc<AtomicU64>,
    /// The hits on screen: the last finished run's.
    shown: Vec<Hit>,
    /// The hits of the running search, shown when it is done.
    pending: Vec<Hit>,
    state: RunState,
    layout: Layout,
}

impl SearchTab {
    fn new(spec: SearchSpec, root: PathBuf) -> SearchTab {
        let layout = layout(&spec, &root, &[], &RunState::Searching);
        SearchTab {
            spec,
            root,
            seq: 0,
            cancel: Arc::new(AtomicU64::new(0)),
            shown: Vec::new(),
            pending: Vec::new(),
            state: RunState::Searching,
            layout,
        }
    }

    fn relayout(&mut self) {
        self.layout = layout(&self.spec, &self.root, &self.shown, &self.state);
    }

    /// The text the tab shows now.
    pub(crate) fn text(&self) -> &str {
        &self.layout.text
    }

    /// Start a run: bump the seq (which stops the older walk) and drop its
    /// pending hits; the rows on screen stay. `None` when the pattern does
    /// not build (the status says so).
    pub(crate) fn begin(&mut self, root: PathBuf, show_hidden: bool, show_ignored: bool) -> Option<Search> {
        self.seq += 1;
        self.cancel.store(self.seq, Ordering::Relaxed);
        self.pending.clear();
        self.root = root;
        let search = match self.spec.query() {
            Ok(query) => {
                self.state = RunState::Searching;
                Some(Search { root: self.root.clone(), show_hidden, show_ignored, query, seq: self.seq, cancel: self.cancel.clone() })
            }
            Err(e) => {
                self.state = RunState::Failed(e.lines().last().unwrap_or("invalid pattern").trim().to_string());
                None
            }
        };
        self.relayout();
        search
    }

    /// Take a batch of the current run. True when the text must be replaced:
    /// the done batch, which swaps the pending hits in.
    pub(crate) fn on_batch(&mut self, batch: Batch) -> bool {
        if batch.seq != self.seq {
            return false;
        }
        self.pending.extend(batch.hits);
        if !batch.done {
            return false;
        }
        self.shown = std::mem::take(&mut self.pending);
        self.state = RunState::Done { files_searched: batch.files_searched, capped: batch.capped };
        self.relayout();
        true
    }

    /// The spans that meet the lines `lines` of `rope`, styled from `theme`.
    pub(crate) fn highlights(&self, rope: &ropey::Rope, lines: Range<usize>, theme: &Theme) -> Highlights {
        let end_line = lines.end.min(rope.len_lines());
        let start_line = lines.start.min(end_line);
        let start = rope.line_to_char(start_line);
        let end = if end_line >= rope.len_lines() { rope.len_chars() } else { rope.line_to_char(end_line) };
        let spans = &self.layout.spans;
        let first = spans.partition_point(|(r, _)| r.end <= start);
        let style = |p: Paint| match p {
            Paint::Header => theme.title,
            Paint::Status(Some(Level::Warn)) => theme.warn,
            Paint::Status(Some(Level::Error)) => theme.error,
            Paint::Status(_) => theme.dim,
            Paint::File => Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            Paint::LineNo => theme.line_number,
            Paint::Match => Style::default().fg(theme.fg).bg(theme.search_match_bg),
        };
        Highlights { spans: spans[first..].iter().take_while(|(r, _)| r.start < end).map(|(r, p)| (r.clone(), style(*p))).collect() }
    }

    /// The hit that Enter on char column `col` of rope line `line` opens.
    pub(crate) fn target_at(&self, line: usize, col: usize) -> Option<&Hit> {
        let at = self.layout.rows.binary_search_by_key(&line, |(l, _)| *l).ok()?;
        let hit = match &self.layout.rows[at].1 {
            RowTarget::File { hit } => *hit,
            RowTarget::Hit { matches } => matches.iter().find(|(r, _)| r.start <= col && col <= r.end).or(matches.first())?.1,
        };
        self.shown.get(hit)
    }
}

impl Drop for SearchTab {
    fn drop(&mut self) {
        self.cancel.store(self.seq.wrapping_add(1), Ordering::Relaxed);
    }
}

impl Editor {
    /// Show the results tab of `spec`, opening it if no tab holds that
    /// search, and search again. Returns the tab's index and the search to
    /// spawn (`None` when the pattern does not build).
    pub fn open_search_tab(&mut self, spec: SearchSpec, cx: &Ctx) -> (usize, Option<(BufferId, Search)>) {
        let open = self.buffers.iter().position(|b| b.search_tab.as_ref().is_some_and(|t| t.spec == spec));
        let at = match open {
            Some(at) => {
                self.activate_index(at);
                at
            }
            None => {
                let id = self.next_id;
                self.next_id += 1;
                let tab = SearchTab::new(spec.clone(), cx.workspace.root.clone());
                let mut buf = Buffer::from_text(id, None, tab.text(), false);
                buf.name = crate::util::text::fit(&format!("Find: {}", spec.text), 30);
                buf.read_only = true;
                buf.indent = super::indent::FileSettings::default().indent(&buf.rope, self.tab_width);
                buf.view.wrap.on = false;
                buf.search_tab = Some(Box::new(tab));
                self.buffers.push(buf);
                let at = self.buffers.len() - 1;
                self.activate_index(at);
                at
            }
        };
        (at, self.start_search_tab(at, cx))
    }

    /// Search again in the active buffer, when it is a results tab.
    pub fn rerun_search_tab(&mut self, cx: &Ctx) -> Option<(BufferId, Search)> {
        let at = self.active()?;
        self.start_search_tab(at, cx)
    }

    fn start_search_tab(&mut self, at: usize, cx: &Ctx) -> Option<(BufferId, Search)> {
        let b = &mut self.buffers[at];
        let tab = b.search_tab.as_mut()?;
        let run = tab.begin(cx.workspace.root.clone(), cx.config.files.show_hidden, cx.config.files.show_ignored);
        let text = tab.text().to_string();
        b.set_preview_text(&text);
        run.map(|s| (b.id, s))
    }

    /// Whether the active tab is a Find results tab.
    pub fn active_is_search_tab(&self) -> bool {
        self.active_buffer().is_some_and(|b| b.search_tab.is_some())
    }

    /// A batch of tab `id`'s search. True when it ended the run and the
    /// tab's text was replaced; false for a closed tab, an older run's
    /// batch and a batch that only adds pending hits.
    pub fn search_tab_batch(&mut self, id: BufferId, batch: Batch) -> bool {
        let Some(at) = self.index_of_id(id) else { return false };
        let b = &mut self.buffers[at];
        let Some(tab) = b.search_tab.as_mut() else { return false };
        if !tab.on_batch(batch) {
            return false;
        }
        let text = tab.text().to_string();
        b.set_preview_text(&text);
        true
    }

    /// Open the hit under `(line, col)` of the active results tab.
    fn open_search_hit(&self, line: usize, col: usize, cx: &mut Ctx) {
        let Some(hit) = self.active_buffer().and_then(|b| b.search_tab.as_ref()).and_then(|t| t.target_at(line, col)) else { return };
        cx.effects.push(Effect::OpenFileAt { path: hit.path.clone(), line: hit.line, col: hit.col });
    }

    /// Enter in a results tab opens the result under the caret; every other
    /// action is the ordinary read-only tab's.
    pub(super) fn search_tab_action(&mut self, a: &EditorAction, cx: &mut Ctx) -> Option<bool> {
        if *a != EditorAction::InsertNewline || !self.active_is_search_tab() {
            return None;
        }
        let (line, col) = self.active_buffer()?.cursor_line_col();
        self.open_search_hit(line, col, cx);
        Some(true)
    }

    /// A double click on the text of a results tab opens the result under
    /// the caret, which the click just placed.
    pub(super) fn search_tab_click(&mut self, m: &Mouse, cx: &mut Ctx) {
        if m.kind != MouseEventKind::Down(MouseButton::Left) || m.clicks != 2 || !self.active_is_search_tab() {
            return;
        }
        if !self.focused_group().text_area.contains(Position::new(m.x, m.y)) {
            return;
        }
        let Some((line, col)) = self.active_buffer().map(|b| b.cursor_line_col()) else { return };
        self.open_search_hit(line, col, cx);
    }

    /// The current run of tab `id` again (hidden and ignored files off), for
    /// tests that run it synchronously.
    #[cfg(test)]
    pub(crate) fn search_tab_search(&self, id: BufferId) -> Option<Search> {
        let tab = self.buffers.iter().find(|b| b.id == id)?.search_tab.as_ref()?;
        Some(Search {
            root: tab.root.clone(),
            show_hidden: false,
            show_ignored: false,
            query: tab.spec.query().ok()?,
            seq: tab.seq,
            cancel: tab.cancel.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;
    use crate::config::Config;
    use crate::editor::selection::Selection;
    use crate::workspace::Workspace;
    use crossterm::event::KeyModifiers;

    fn spec(text: &str) -> SearchSpec {
        SearchSpec { text: text.into(), ..SearchSpec::default() }
    }

    fn hit(path: &str, line: usize, col: usize, preview: &str) -> Hit {
        Hit {
            path: PathBuf::from(path),
            line,
            col,
            len: 3,
            preview: preview.into(),
            preview_col: col,
            matched: "foo".into(),
            excluded: false,
            rename: None,
        }
    }

    fn done(files_searched: usize) -> RunState {
        RunState::Done { files_searched, capped: false }
    }

    fn sample() -> Vec<Hit> {
        vec![
            hit("/r/a.txt", 1, 0, "foo here"),
            hit("/r/src/b.rs", 0, 4, "let foo = 1;"),
            hit("/r/src/b.rs", 1, 0, "foo(foo);"),
            hit("/r/src/b.rs", 1, 4, "foo(foo);"),
        ]
    }

    const SAMPLE_TEXT: &str = "Find in Files: foo\n4 matches in 2 files; Enter opens a result, F5 searches again\n\na.txt\n     2: foo here\n\nsrc/b.rs\n     1: let foo = 1;\n     2: foo(foo);\n";

    fn sample_layout() -> Layout {
        layout(&spec("foo"), Path::new("/r"), &sample(), &done(3))
    }

    fn status(spec: &SearchSpec, hits: &[Hit], state: &RunState) -> (String, Paint) {
        let l = layout(spec, Path::new("/r"), hits, state);
        let line = l.text.lines().nth(1).unwrap().to_string();
        (line, l.spans[1].1)
    }

    #[test]
    fn layout_groups_files_and_merges_a_line() {
        let l = sample_layout();
        assert_eq!(l.text, SAMPLE_TEXT);
        assert_eq!(
            l.rows,
            [
                (3, RowTarget::File { hit: 0 }),
                (4, RowTarget::Hit { matches: vec![(8..11, 0)] }),
                (6, RowTarget::File { hit: 1 }),
                (7, RowTarget::Hit { matches: vec![(12..15, 1)] }),
                (8, RowTarget::Hit { matches: vec![(8..11, 2), (12..15, 3)] }),
            ]
        );
    }

    #[test]
    fn layout_spans() {
        let l = sample_layout();
        let rope = ropey::Rope::from_str(&l.text);
        let text = |r: &Range<usize>| rope.slice(r.clone()).to_string();
        let line_of = |n: usize| rope.line_to_char(n)..rope.line_to_char(n + 1);
        let on = |n: usize| -> Vec<(String, Paint)> {
            let r = line_of(n);
            l.spans.iter().filter(|(s, _)| s.start >= r.start && s.end <= r.end).map(|(s, p)| (text(s), *p)).collect()
        };
        assert_eq!(on(0), [("Find in Files: foo".to_string(), Paint::Header)]);
        assert_eq!(on(1)[0].1, Paint::Status(None));
        assert_eq!(on(3), [("a.txt".to_string(), Paint::File)]);
        assert_eq!(on(4), [("     2: ".to_string(), Paint::LineNo), ("foo".to_string(), Paint::Match)]);
        assert_eq!(on(8), [("     2: ".to_string(), Paint::LineNo), ("foo".to_string(), Paint::Match), ("foo".to_string(), Paint::Match)]);
        assert!(l.spans.windows(2).all(|w| w[0].0.end <= w[1].0.start), "sorted and not overlapping: {:?}", l.spans);
        assert!(l.spans.iter().all(|(r, _)| r.start < r.end && r.end <= rope.len_chars()));
    }

    #[test]
    fn layout_statuses() {
        let s = spec("foo");
        let one = [hit("/r/a.txt", 0, 0, "foo")];
        assert_eq!(status(&s, &sample(), &RunState::Searching), ("Searching…".into(), Paint::Status(None)));
        // The rows given stay while a run is on.
        let l = layout(&s, Path::new("/r"), &sample(), &RunState::Searching);
        assert!(l.text.ends_with("     2: foo(foo);\n") && l.rows.len() == 5);
        assert_eq!(
            status(&s, &[], &done(7)),
            ("No matches (7 files searched); F5 searches again".into(), Paint::Status(Some(Level::Warn)))
        );
        assert_eq!(layout(&s, Path::new("/r"), &[], &done(7)).rows, []);
        assert_eq!(status(&s, &one, &done(1)), ("1 match in 1 file; Enter opens a result, F5 searches again".into(), Paint::Status(None)));
        let two = [hit("/r/a.txt", 0, 0, "foo"), hit("/r/a.txt", 1, 0, "foo")];
        assert_eq!(
            status(&s, &two, &RunState::Done { files_searched: 1, capped: true }),
            ("2+ matches in 1 file; stopped at 10000; Enter opens a result, F5 searches again".into(), Paint::Status(Some(Level::Warn)))
        );
        assert_eq!(status(&s, &[], &RunState::Failed("bad".into())), ("Invalid pattern: bad".into(), Paint::Status(Some(Level::Error))));
        let opts = SearchSpec { text: "x".into(), case_sensitive: true, regex: true, ..SearchSpec::default() };
        assert_eq!(layout(&opts, Path::new("/r"), &[], &done(1)).text.lines().next().unwrap(), "Find in Files: x   [match case, regex]");
    }

    #[test]
    fn layout_keeps_columns_over_line_breaks() {
        let mut h = hit("/r/a.txt", 0, 2, "a\rfoo");
        h.preview_col = 2;
        let l = layout(&spec("foo"), Path::new("/r"), &[h], &done(1));
        assert_eq!(l.text.lines().nth(4).unwrap(), "     1: a foo");
        assert_eq!(l.rows[1].1, RowTarget::Hit { matches: vec![(10..13, 0)] });
    }

    fn tab_with_sample() -> SearchTab {
        let mut t = SearchTab::new(spec("foo"), PathBuf::from("/r"));
        t.begin(PathBuf::from("/r"), false, false);
        let seq = t.seq;
        assert!(t.on_batch(Batch { seq, hits: sample(), files_searched: 3, done: true, capped: false }));
        t
    }

    #[test]
    fn target_at_prefers_the_match_under_the_caret() {
        let t = tab_with_sample();
        assert_eq!(t.layout.text, SAMPLE_TEXT);
        let at = |line, col| t.target_at(line, col).map(|h| (h.line, h.col));
        assert_eq!(at(8, 13), Some((1, 4)));
        assert_eq!(at(8, 2), Some((1, 0)), "before the first match falls back to it");
        assert_eq!(at(8, 11), Some((1, 0)), "the end of a match counts");
        assert_eq!(at(8, 15), Some((1, 4)));
        assert_eq!(at(6, 3), Some((0, 4)), "a file line opens its first hit");
        assert_eq!(at(3, 0), Some((1, 0)));
        for line in [0, 1, 2, 5, 9] {
            assert_eq!(at(line, 0), None, "line {line}");
        }
    }

    #[test]
    fn a_batch_of_an_older_run_is_ignored() {
        let mut t = SearchTab::new(spec("foo"), PathBuf::from("/r"));
        t.begin(PathBuf::from("/r"), false, false);
        let first = t.seq;
        t.begin(PathBuf::from("/r"), false, false);
        assert!(!t.on_batch(Batch { seq: first, hits: sample(), files_searched: 3, done: true, capped: false }));
        assert!(t.shown.is_empty() && t.pending.is_empty());
        assert!(matches!(t.state, RunState::Searching));
    }

    #[test]
    fn rows_stay_until_the_new_run_is_done() {
        let mut t = tab_with_sample();
        t.begin(PathBuf::from("/r"), false, false);
        assert_eq!(t.text().lines().nth(1), Some("Searching…"));
        assert!(t.text().ends_with("     2: foo(foo);\n"), "old rows stay");
        let seq = t.seq;
        let before = t.text().to_string();
        let batch = |hits, files_searched, done| Batch { seq, hits, files_searched, done, capped: false };
        assert!(!t.on_batch(batch(vec![hit("/r/z.txt", 0, 0, "foo")], 1, false)));
        assert_eq!(t.text(), before, "pending hits are not shown yet");
        assert!(t.on_batch(batch(Vec::new(), 5, true)));
        assert!(t.text().contains("z.txt") && !t.text().contains("a.txt"), "the done batch swaps the rows: {}", t.text());
        // A run with no hits at all empties the rows.
        t.begin(PathBuf::from("/r"), false, false);
        let seq = t.seq;
        assert!(t.on_batch(Batch { seq, hits: Vec::new(), files_searched: 5, done: true, capped: false }));
        assert!(t.shown.is_empty());
        assert_eq!(t.text(), "Find in Files: foo\nNo matches (5 files searched); F5 searches again\n\n");
    }

    #[test]
    fn dropping_the_tab_cancels_its_walk() {
        let mut t = SearchTab::new(spec("foo"), PathBuf::from("/r"));
        t.begin(PathBuf::from("/r"), false, false);
        let (cancel, seq) = (t.cancel.clone(), t.seq);
        assert_eq!(cancel.load(Ordering::Relaxed), seq);
        drop(t);
        assert_ne!(cancel.load(Ordering::Relaxed), seq);
    }

    #[test]
    fn a_new_run_cancels_the_older_walk() {
        let mut t = SearchTab::new(spec("foo"), PathBuf::from("/r"));
        let search = t.begin(PathBuf::from("/r"), false, false).unwrap();
        t.begin(PathBuf::from("/r"), false, false);
        assert_ne!(search.cancel.load(Ordering::Relaxed), search.seq);
    }

    #[test]
    fn a_bad_pattern_shows_in_the_status() {
        let mut t = SearchTab::new(SearchSpec { text: "a(".into(), regex: true, ..SearchSpec::default() }, PathBuf::from("/r"));
        assert!(t.begin(PathBuf::from("/r"), false, false).is_none());
        assert!(t.text().lines().nth(1).unwrap().starts_with("Invalid pattern: "));
    }

    #[test]
    fn highlights_keep_only_the_spans_of_the_window() {
        let t = tab_with_sample();
        let theme = Theme::default();
        let rope = ropey::Rope::from_str(t.text());
        let h = t.highlights(&rope, 3..4, &theme);
        assert_eq!(h.spans.len(), 1);
        assert_eq!(h.spans[0].1, Style::default().fg(theme.fg).add_modifier(Modifier::BOLD));
        assert_eq!(t.highlights(&rope, 0..1, &theme).spans[0].1, theme.title);
        assert_eq!(t.highlights(&rope, 4..5, &theme).spans[1].1, Style::default().fg(theme.fg).bg(theme.search_match_bg));
        assert!(t.highlights(&rope, 2..3, &theme).spans.is_empty());
    }

    // ---- the editor side ----

    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Root {
            let dir = std::env::temp_dir().join(format!("jetl-search-tab-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("a.txt"), "no\nfoo here\n").unwrap();
            std::fs::write(dir.join("src/b.rs"), "let foo = 1;\nfoo(foo);\n").unwrap();
            Root(dir)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn with_root_cx<R>(root: &Path, f: impl FnOnce(&mut Ctx) -> R) -> (R, Vec<Effect>) {
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(root.to_path_buf());
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        let r = f(&mut cx);
        (r, cx.effects)
    }

    /// Run a search to its end and feed the batches to the tab `id`.
    fn run(ed: &mut Editor, id: BufferId, search: Search) {
        let mut batches = Vec::new();
        search.run(|b| batches.push(b));
        for b in batches {
            ed.search_tab_batch(id, b);
        }
    }

    fn open(ed: &mut Editor, root: &Path, spec: SearchSpec) -> (usize, BufferId) {
        let (at, run_) = with_root_cx(root, |cx| ed.open_search_tab(spec, cx)).0;
        let (id, search) = run_.unwrap();
        run(ed, id, search);
        (at, id)
    }

    fn opens(fx: &[Effect]) -> Vec<(PathBuf, usize, usize)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::OpenFileAt { path, line, col } => Some((path.clone(), *line, *col)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn open_runs_reuses_and_opens_targets() {
        let root = Root::new("open");
        let mut ed = Editor::new(&Config::default());
        let (at, id) = open(&mut ed, &root.0, spec("foo"));
        let b = &ed.buffers[at];
        assert!(b.read_only && b.path.is_none() && !b.dirty());
        assert_eq!(b.name, "Find: foo");
        // Hits in the walk's path order: a.txt, src/b.rs.
        assert_eq!(b.text(), SAMPLE_TEXT);
        // Caret on line 8 (`     2: foo(foo);`), col 12: the second match.
        let p = ed.buffers[at].rope.line_to_char(8) + 12;
        ed.buffers[at].view.sels = Selection::point(p).into();
        let (_, fx) = with_root_cx(&root.0, |cx| assert!(ed.handle_action(&EditorAction::InsertNewline, cx)));
        assert_eq!(opens(&fx), [(root.0.join("src/b.rs"), 1, 4)]);
        for a in [EditorAction::InsertChar('x'), EditorAction::Backspace] {
            with_root_cx(&root.0, |cx| ed.handle_action(&a, cx));
        }
        assert_eq!(ed.buffers[at].text(), SAMPLE_TEXT);
        assert!(!ed.buffers[at].dirty());
        // The same search is the same tab, run again with a newer seq.
        let before = ed.search_tab_search(id).unwrap().seq;
        let n = ed.buffers.len();
        let (at2, run2) = with_root_cx(&root.0, |cx| ed.open_search_tab(spec("foo"), cx)).0;
        assert_eq!((at2, ed.buffers.len()), (at, n));
        assert!(run2.unwrap().1.seq > before);
        // Another option set is another search.
        let (at3, _) = open(&mut ed, &root.0, SearchSpec { case_sensitive: true, ..spec("foo") });
        assert_ne!(at3, at);
        assert_eq!(ed.buffers.len(), n + 1);
    }

    #[test]
    fn a_closed_tab_drops_its_batches() {
        let root = Root::new("closed");
        let mut ed = Editor::new(&Config::default());
        let (_, id) = open(&mut ed, &root.0, spec("foo"));
        let search = ed.search_tab_search(id).unwrap();
        with_root_cx(&root.0, |cx| ed.handle_action(&EditorAction::CloseTab, cx));
        assert!(ed.index_of_id(id).is_none());
        assert_ne!(search.cancel.load(Ordering::Relaxed), search.seq, "closing stops the walk");
        let batch = Batch { seq: search.seq, hits: Vec::new(), files_searched: 0, done: true, capped: false };
        assert!(!ed.search_tab_batch(id, batch));
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16, clicks: u8) -> Mouse {
        Mouse { kind, x, y, clicks, mods: KeyModifiers::NONE }
    }

    fn draw(ed: &mut Editor, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let theme = Theme::default();
        term.draw(|f| ed.render(f, Rect::new(0, 0, w, 1), Rect::new(0, 1, w, h - 1), true, &theme)).unwrap();
        term.backend().buffer().clone()
    }

    #[test]
    fn a_double_click_on_a_hit_opens_it() {
        let root = Root::new("click");
        let mut ed = Editor::new(&Config::default());
        open(&mut ed, &root.0, spec("foo"));
        draw(&mut ed, 80, 20);
        let ta = ed.focused_group().text_area;
        let (x, y) = (ta.x + 10, ta.y + 4);
        let down = |ed: &mut Editor, x, y, clicks| {
            let m = mouse(MouseEventKind::Down(MouseButton::Left), x, y, clicks);
            with_root_cx(&root.0, |cx| ed.handle_mouse(&m, Rect::new(0, 1, 80, 19), cx)).1
        };
        assert!(opens(&down(&mut ed, x, y, 1)).is_empty(), "a single click only places the caret");
        assert_eq!(ed.active_buffer().unwrap().cursor_line_col().0, 4);
        assert_eq!(opens(&down(&mut ed, x, y, 2)), [(root.0.join("a.txt"), 1, 0)]);
        assert!(opens(&down(&mut ed, 3, 0, 2)).is_empty(), "the tab bar opens nothing");
    }

    #[test]
    fn results_tabs_draw_no_guides_or_rulers() {
        let root = Root::new("guides");
        let mut ed = Editor::new(&Config::default());
        ed.indent_guides = true;
        ed.rulers = vec![4];
        open(&mut ed, &root.0, spec("foo"));
        let screen = draw(&mut ed, 80, 20);
        let ta = ed.focused_group().text_area;
        let row: String = (ta.x..ta.x + 12).map(|x| screen[(x, ta.y + 4)].symbol().to_string()).collect();
        assert_eq!(row, "     2: foo ", "no guide in the leading spaces and no ruler: {row:?}");
        let rows: String = (0..20).flat_map(|y| (0..80).map(move |x| (x, y))).map(|(x, y)| screen[(x, y)].symbol().to_string()).collect();
        assert!(!rows.contains('│'), "no guide glyph anywhere");
    }
}
