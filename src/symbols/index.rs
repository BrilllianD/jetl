//! The project symbol index behind the Go to Symbol palette. It follows the
//! palette's file index: every file with a tags query (up to `MAX_FILES`,
//! each at most `MAX_FILE_BYTES`) is parsed in `spawn_blocking` and comes
//! back in `AppMsg::Symbols` batches. Later runs skip files whose mtime is
//! unchanged; watcher paths are dropped and parsed again. Runs go through
//! `util::Coalesce` (one at a time) and carry a seq: a batch of an older
//! run is dropped.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::Symbol;
use crate::editor::syntax;
use crate::util::coalesce::Coalesce;

/// Files larger than this are not indexed.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
/// At most this many files are indexed.
pub const MAX_FILES: usize = 5000;
/// A batch goes out after this many files or this long, whichever first.
const BATCH_FILES: usize = 64;
const BATCH_EVERY: Duration = Duration::from_millis(200);

/// The symbols of one file and the mtime they were read at.
#[derive(Debug, Clone)]
pub struct FileSymbols {
    pub mtime: Option<SystemTime>,
    pub symbols: Vec<Symbol>,
}

/// Files parsed since the previous batch; `None` drops a file (gone, too
/// large, unreadable).
#[derive(Debug, Default)]
pub struct SymbolBatch {
    pub files: Vec<(String, Option<FileSymbols>)>,
    /// The last batch of this run.
    pub done: bool,
}

/// One palette row: a symbol and its file (relative to the root).
#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub path: Arc<str>,
    pub symbol: Symbol,
}

#[derive(Debug, Default)]
pub struct SymbolIndex {
    files: HashMap<String, FileSymbols>,
    /// Files of the last file index that have a tags query.
    candidates: Arc<Vec<String>>,
    /// The next run checks every candidate (after a file index load).
    rescan: bool,
    /// Files the next run parses whatever their mtime (watcher paths).
    queue: BTreeSet<String>,
    run: Coalesce,
    /// Seq of the newest run; older runs stop and their batches are dropped.
    seq: u64,
    cancel: Arc<AtomicU64>,
    /// The newest run has not sent its last batch.
    running: bool,
    /// Palette rows, rebuilt on demand after a change.
    entries: Option<Arc<Vec<SymbolEntry>>>,
}

/// A run of the index, detached so it can go to `spawn_blocking`.
pub struct IndexJob {
    pub root: PathBuf,
    /// Relative paths with the mtime already indexed (skipped when the file
    /// still has it); `None` parses the file regardless.
    pub files: Vec<(String, Option<SystemTime>)>,
    pub seq: u64,
    cancel: Arc<AtomicU64>,
}

/// Whether the index parses files of this path.
pub fn indexable(path: &Path) -> bool {
    syntax::detect(path).is_some_and(|d| d.tags.is_some())
}

impl SymbolIndex {
    /// Forget everything (root switch); a run in flight stops.
    pub fn reset(&mut self) {
        let seq = self.seq + 1;
        self.cancel.store(seq, Ordering::Relaxed);
        *self = Self { seq, cancel: self.cancel.clone(), ..Self::default() };
    }

    /// The palette's file list loaded: index what it lists (only files with
    /// a tags query, at most `MAX_FILES`) and drop what it no longer does.
    pub fn set_files(&mut self, files: &[String]) {
        let candidates: Vec<String> = files.iter().filter(|f| indexable(Path::new(f))).take(MAX_FILES).cloned().collect();
        let keep: HashSet<&str> = candidates.iter().map(String::as_str).collect();
        let before = self.files.len();
        self.files.retain(|f, _| keep.contains(f.as_str()));
        if self.files.len() != before {
            self.entries = None;
        }
        self.candidates = Arc::new(candidates);
        self.rescan = true;
    }

    /// Files changed on disk: drop the symbols of those the index knows
    /// (and of files under a changed directory) and queue them to be parsed
    /// again. Returns whether anything was queued, and whether an indexable
    /// file it does not know appeared (the file index is out of date).
    pub fn invalidate(&mut self, rels: &[String]) -> (bool, bool) {
        let (mut queued, mut unknown) = (false, false);
        for rel in rels {
            let prefix = format!("{rel}/");
            let mut drop: Vec<String> = self.files.keys().filter(|f| f.starts_with(&prefix)).cloned().collect();
            if self.knows(rel) {
                drop.push(rel.clone());
            } else if indexable(Path::new(rel)) {
                unknown = true;
            }
            for f in drop {
                self.files.remove(&f);
                self.queue.insert(f);
                queued = true;
            }
        }
        if queued {
            self.entries = None;
        }
        (queued, unknown)
    }

    /// Whether `rel` is indexed or listed for indexing.
    pub fn knows(&self, rel: &str) -> bool {
        self.files.contains_key(rel) || self.candidates.binary_search_by(|c| c.as_str().cmp(rel)).is_ok()
    }

    /// Ask for a run; true means start one now ([`SymbolIndex::start`]).
    pub fn request(&mut self) -> bool {
        self.run.request()
    }

    /// The next run: the queued files, plus every candidate after a file
    /// index load.
    pub fn start(&mut self, root: PathBuf) -> IndexJob {
        if self.running {
            // The previous run was given up on; check everything again.
            self.rescan = true;
        }
        self.seq += 1;
        self.cancel.store(self.seq, Ordering::Relaxed);
        self.running = true;
        let queue = std::mem::take(&mut self.queue);
        let mut files: Vec<(String, Option<SystemTime>)> = Vec::new();
        if std::mem::take(&mut self.rescan) {
            for f in self.candidates.iter().filter(|f| !queue.contains(*f)) {
                files.push((f.clone(), self.files.get(f).and_then(|s| s.mtime)));
            }
        }
        files.extend(queue.into_iter().map(|f| (f, None)));
        IndexJob { root, files, seq: self.seq, cancel: self.cancel.clone() }
    }

    /// Apply a batch of run `seq`. Returns `None` when it is stale (dropped),
    /// else whether a held-back run should start now.
    pub fn on_batch(&mut self, seq: u64, batch: SymbolBatch) -> Option<bool> {
        if seq != self.seq {
            return None;
        }
        self.run.touch();
        if !batch.files.is_empty() {
            self.entries = None;
        }
        for (rel, syms) in batch.files {
            match syms {
                Some(s) => {
                    self.files.insert(rel, s);
                }
                None => {
                    self.files.remove(&rel);
                }
            }
        }
        if !batch.done {
            return Some(false);
        }
        self.running = false;
        Some(self.run.done())
    }

    /// A run is in flight or waiting.
    pub fn indexing(&self) -> bool {
        self.running
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Palette rows: every symbol except `impl` blocks, by path then line.
    pub fn entries(&mut self) -> Arc<Vec<SymbolEntry>> {
        if let Some(e) = &self.entries {
            return e.clone();
        }
        let mut paths: Vec<&String> = self.files.keys().collect();
        paths.sort();
        let mut out = Vec::new();
        for p in paths {
            let path: Arc<str> = p.as_str().into();
            for s in self.files[p].symbols.iter().filter(|s| s.kind != "impl") {
                out.push(SymbolEntry { path: path.clone(), symbol: s.clone() });
            }
        }
        let e = Arc::new(out);
        self.entries = Some(e.clone());
        e
    }
}

impl IndexJob {
    /// Parse the files in order; `send` gets the batches, the last with
    /// `done`. Nothing is sent after a cancel.
    pub fn run(self, mut send: impl FnMut(SymbolBatch)) {
        let cancelled = || self.cancel.load(Ordering::Relaxed) != self.seq;
        let mut parser = tree_sitter::Parser::new();
        let mut batch = SymbolBatch::default();
        let mut last_send = Instant::now();
        for (rel, known) in &self.files {
            if cancelled() {
                return;
            }
            let path = self.root.join(rel);
            let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file() && m.len() <= MAX_FILE_BYTES);
            let mtime = meta.as_ref().and_then(|m| m.modified().ok());
            if known.is_some() && *known == mtime {
                continue;
            }
            let syms = meta.and_then(|_| parse(&path, &mut parser)).map(|symbols| FileSymbols { mtime, symbols });
            batch.files.push((rel.clone(), syms));
            if batch.files.len() >= BATCH_FILES || last_send.elapsed() >= BATCH_EVERY {
                send(std::mem::take(&mut batch));
                last_send = Instant::now();
            }
        }
        if !cancelled() {
            batch.done = true;
            send(batch);
        }
    }
}

/// The symbols of one file; `None` for binary or unreadable files.
fn parse(path: &Path, parser: &mut tree_sitter::Parser) -> Option<Vec<Symbol>> {
    let def = syntax::detect(path).filter(|d| d.tags.is_some())?;
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    Some(super::extract_text(def, parser, &text))
}
