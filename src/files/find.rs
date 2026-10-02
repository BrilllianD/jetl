//! Find in files: the search that walks the project off the main thread and
//! streams hits back in batches, and the text replacement used by Replace
//! in files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use regex::Regex;

/// A search stops after this many hits.
pub const MAX_HITS: usize = 10_000;
/// Files larger than this are skipped.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// A batch goes out at this many hits or after this long, whichever first.
const BATCH_HITS: usize = 200;
const BATCH_EVERY: Duration = Duration::from_millis(100);
/// Preview of a long line: this many chars, starting a little before the
/// match.
const PREVIEW_CHARS: usize = 300;
const PREVIEW_LEAD: usize = 40;

/// One match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    /// 0-based line and char column of the match start.
    pub line: usize,
    pub col: usize,
    /// Match length in chars, cut at the end of the line.
    pub len: usize,
    /// The line (or a window of a long line) and the match's char column
    /// in it.
    pub preview: String,
    pub preview_col: usize,
}

/// Hits found since the previous batch.
#[derive(Debug)]
pub struct Batch {
    pub seq: u64,
    pub hits: Vec<Hit>,
    pub files_searched: usize,
    /// The last batch of this search.
    pub done: bool,
    /// Stopped at `MAX_HITS`.
    pub capped: bool,
}

/// A search request, detached from the panel so it can run in
/// `spawn_blocking`. `cancel` holds the seq of the newest search; this one
/// stops once it no longer matches.
pub struct Search {
    pub root: PathBuf,
    pub show_hidden: bool,
    pub show_ignored: bool,
    pub re: Regex,
    pub seq: u64,
    pub cancel: Arc<AtomicU64>,
}

impl Search {
    /// Walk the root in path order and search every text file; `send` gets
    /// the batches, the last with `done`. Nothing is sent after a cancel.
    pub fn run(self, mut send: impl FnMut(Batch)) {
        let cancelled = || self.cancel.load(Ordering::Relaxed) != self.seq;
        let walk = crate::files::walk::builder(&self.root, self.show_hidden, self.show_ignored).sort_by_file_path(|a, b| a.cmp(b)).build();
        let (mut pending, mut total, mut files) = (Vec::new(), 0usize, 0usize);
        let mut last_send = Instant::now();
        let mut capped = false;
        for entry in walk.flatten() {
            if cancelled() {
                return;
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else { continue };
            files += 1;
            let found = search_text(entry.path(), &bytes, &self.re, MAX_HITS - total);
            total += found.len();
            pending.extend(found);
            if total >= MAX_HITS {
                capped = true;
                break;
            }
            if pending.len() >= BATCH_HITS || (!pending.is_empty() && last_send.elapsed() >= BATCH_EVERY) {
                send(Batch { seq: self.seq, hits: std::mem::take(&mut pending), files_searched: files, done: false, capped: false });
                last_send = Instant::now();
            }
        }
        if !cancelled() {
            send(Batch { seq: self.seq, hits: pending, files_searched: files, done: true, capped });
        }
    }
}

/// Up to `limit` hits of `re` in a file's bytes; none for binary files (a
/// NUL byte in the first 8 KiB). Invalid UTF-8 is searched lossily.
pub fn search_text(path: &Path, bytes: &[u8], re: &Regex, limit: usize) -> Vec<Hit> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let mut hits = Vec::new();
    // Line number and start of the line holding the current match, advanced
    // incrementally so the text is scanned once.
    let (mut line, mut line_start, mut scanned) = (0usize, 0usize, 0usize);
    for m in re.find_iter(&text) {
        if m.start() == m.end() {
            continue;
        }
        if hits.len() >= limit {
            break;
        }
        for (i, _) in text[scanned..m.start()].match_indices('\n') {
            line += 1;
            line_start = scanned + i + 1;
        }
        scanned = m.start();
        let line_end = text[line_start..].find('\n').map_or(text.len(), |i| line_start + i);
        let line_text = text[line_start..line_end].trim_end_matches('\r');
        let col = text[line_start..m.start()].chars().count();
        let len = text[m.start()..m.end().min(line_end)].chars().count().max(1);
        let (preview, preview_col) = preview(line_text, col);
        hits.push(Hit { path: path.to_path_buf(), line, col, len, preview, preview_col });
    }
    hits
}

/// The line, or a window of it around `col` when it is long; tabs become
/// spaces so the preview lines up.
fn preview(line: &str, col: usize) -> (String, usize) {
    let start = if line.chars().count() > PREVIEW_CHARS { col.saturating_sub(PREVIEW_LEAD) } else { 0 };
    let text: String = line.chars().skip(start).take(PREVIEW_CHARS).map(|c| if c == '\t' { ' ' } else { c }).collect();
    (text, col - start)
}

/// A replacement of one match, in char offsets of the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub start: usize,
    pub old: String,
    pub new: String,
}

/// Every non-empty match of `re` in `text` and its replacement: `with`
/// literally, or with `$1`/`${name}` expanded when `expand` (regex mode).
pub fn replacements(text: &str, re: &Regex, with: &str, expand: bool) -> Vec<Replacement> {
    let (mut out, mut byte, mut ch) = (Vec::new(), 0usize, 0usize);
    for caps in re.captures_iter(text) {
        let m = caps.get(0).expect("group 0");
        if m.start() == m.end() {
            continue;
        }
        ch += text[byte..m.start()].chars().count();
        let new = if expand {
            let mut s = String::new();
            caps.expand(with, &mut s);
            s
        } else {
            with.to_string()
        };
        out.push(Replacement { start: ch, old: m.as_str().to_string(), new });
        ch += m.as_str().chars().count();
        byte = m.end();
    }
    out
}

/// `text` with every replacement applied.
pub fn replace_text(text: &str, reps: &[Replacement]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    let mut at = 0;
    for r in reps {
        out.extend(chars.by_ref().take(r.start - at));
        out.push_str(&r.new);
        chars.by_ref().take(r.old.chars().count()).for_each(drop);
        at = r.start + r.old.chars().count();
    }
    out.extend(chars);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::search::build_pattern;

    #[test]
    fn hits_carry_line_column_and_preview() {
        let re = build_pattern("needle", false, false, false).unwrap();
        let text = "first\n\tа needle here\r\nnone\nNEEDLE and needle\n";
        let hits = search_text(Path::new("f"), text.as_bytes(), &re, 100);
        let pos: Vec<_> = hits.iter().map(|h| (h.line, h.col, h.len)).collect();
        assert_eq!(pos, [(1, 3, 6), (3, 0, 6), (3, 11, 6)]);
        assert_eq!(hits[0].preview, " а needle here", "tab shown as a space, CR dropped");
        assert_eq!(hits[0].preview_col, 3);
        assert_eq!(search_text(Path::new("f"), text.as_bytes(), &re, 2).len(), 2, "limit");
        assert!(search_text(Path::new("f"), b"needle\0", &re, 100).is_empty(), "binary");
    }

    #[test]
    fn long_line_preview_is_a_window_around_the_match() {
        let re = build_pattern("x", false, true, false).unwrap();
        let line = format!("{}x{}", "a".repeat(1000), "b".repeat(1000));
        let h = &search_text(Path::new("f"), line.as_bytes(), &re, 10)[0];
        assert_eq!((h.col, h.preview_col, h.preview.chars().count()), (1000, PREVIEW_LEAD, PREVIEW_CHARS));
        assert_eq!(h.preview.chars().nth(h.preview_col), Some('x'));
    }

    #[test]
    fn replacements_literal_and_with_groups() {
        let re = build_pattern(r"(\w+)@(\w+)", true, false, false).unwrap();
        let text = "мы a@b, c@d";
        let reps = replacements(text, &re, "$2@$1", true);
        assert_eq!(reps.iter().map(|r| (r.start, r.new.as_str())).collect::<Vec<_>>(), [(3, "b@a"), (8, "d@c")]);
        assert_eq!(replace_text(text, &reps), "мы b@a, d@c");
        let lit = replacements(text, &re, "$2", false);
        assert_eq!(replace_text(text, &lit), "мы $2, $2");
    }

    #[test]
    fn search_streams_sorted_files_and_honours_cancel() {
        let dir = std::env::temp_dir().join(format!("jetl-test-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("b/two.txt"), "hit\n").unwrap();
        std::fs::write(dir.join("a.txt"), "hit hit\n").unwrap();
        std::fs::write(dir.join("c.bin"), b"hit\0").unwrap();
        let cancel = Arc::new(AtomicU64::new(1));
        let search = |seq| Search {
            root: dir.clone(),
            show_hidden: false,
            show_ignored: false,
            re: build_pattern("hit", false, false, false).unwrap(),
            seq,
            cancel: cancel.clone(),
        };
        let mut batches = Vec::new();
        search(1).run(|b| batches.push(b));
        let hits: Vec<_> = batches.iter().flat_map(|b| &b.hits).map(|h| h.path.strip_prefix(&dir).unwrap().to_owned()).collect();
        assert_eq!(hits, [PathBuf::from("a.txt"), "a.txt".into(), "b/two.txt".into()]);
        assert!(batches.last().unwrap().done);
        // A newer search started: this one sends nothing.
        let mut late = Vec::new();
        search(0).run(|b| late.push(b));
        assert!(late.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
