//! The project symbol index behind the Go to Symbol palette. It follows the
//! palette's file index: every file with a tags query (up to `MAX_FILES`,
//! each at most `MAX_FILE_BYTES`) is parsed in `spawn_blocking` and comes
//! back in `AppMsg::Symbols` batches. Each file also keeps its tree-sitter
//! references (`refs_named`) and imports, which resolve to file edges on
//! demand (`imports_of`, `imported_by`); the job reads the nearest `go.mod`
//! of each Go file to turn its in-module imports into directory specs. Later runs skip files whose size
//! and mtime are unchanged; the first run after a root opens takes such files from the on-disk cache
//! (`cache.rs`), and a finished run that changed anything is written back by the app. Watcher paths are
//! dropped and parsed again. Runs go through
//! `util::Coalesce` (one at a time) and carry a seq: a batch of an older
//! run is dropped.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::{Import, Reference, Symbol};
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
    /// File length in bytes when read; with `mtime` decides reuse.
    pub size: u64,
    pub symbols: Vec<Symbol>,
    /// References in the file, in document order (`symbols::extract_text_all`).
    pub refs: Vec<Reference>,
    /// Imports in the file, in document order (`symbols::imports`).
    pub imports: Vec<Import>,
}

/// Files parsed since the previous batch; `None` drops a file (gone, too
/// large, unreadable).
#[derive(Debug, Default)]
pub struct SymbolBatch {
    pub files: Vec<(String, Option<FileSymbols>)>,
    /// The last batch of this run.
    pub done: bool,
    /// A file was parsed, or one the index or the cache file held was dropped; not set for files taken from the
    /// cache or for a file that again gives nothing.
    pub changed: bool,
}

/// One palette row: a symbol and its file (relative to the root).
#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub path: Arc<str>,
    pub symbol: Symbol,
    /// `symbol.col` is a UTF-16 column from a language server (converted once the file is loaded).
    pub utf16: bool,
}

/// Name to its definitions: path and index into that file's symbols.
type ByName = HashMap<Arc<str>, Vec<(Arc<str>, usize)>>;

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
    /// File import edges, rebuilt on demand after a change.
    edges: Option<Edges>,
    /// Definitions by name, `impl` blocks left out: path and index into that file's symbols. Rebuilt on demand after a change.
    by_name: Option<ByName>,
    /// The cache of this root was handed to a run.
    cache_read: bool,
    /// Files changed since the cache was last taken.
    dirty: bool,
}

/// Resolved imports between indexed files, both directions.
#[derive(Debug, Default)]
struct Edges {
    out: HashMap<Arc<str>, Vec<Arc<str>>>,
    inc: HashMap<Arc<str>, Vec<Arc<str>>>,
}

/// A run of the index, detached so it can go to `spawn_blocking`.
pub struct IndexJob {
    pub root: PathBuf,
    /// Relative paths with the size and mtime already indexed (skipped when
    /// the file still has them); `None` parses the file regardless.
    pub files: Vec<(String, Option<(u64, SystemTime)>)>,
    pub seq: u64,
    cancel: Arc<AtomicU64>,
    /// This run may take files from the on-disk cache.
    warm: bool,
    cache: Option<PathBuf>,
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
            self.edges = None;
            self.by_name = None;
            self.dirty = true;
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
                self.dirty |= self.files.remove(&f).is_some();
                self.queue.insert(f);
                queued = true;
            }
        }
        if queued {
            self.entries = None;
            self.edges = None;
            self.by_name = None;
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
        let mut files: Vec<(String, Option<(u64, SystemTime)>)> = Vec::new();
        let warm = self.rescan && !self.cache_read;
        self.cache_read |= warm;
        if std::mem::take(&mut self.rescan) {
            for f in self.candidates.iter().filter(|f| !queue.contains(*f)) {
                files.push((f.clone(), self.files.get(f).and_then(|s| s.mtime.map(|m| (s.size, m)))));
            }
        }
        files.extend(queue.into_iter().map(|f| (f, None)));
        IndexJob { root, files, seq: self.seq, cancel: self.cancel.clone(), warm, cache: None }
    }

    /// The entries to write to the cache file, once a run is done and
    /// something changed since the last call; sorted by path, without files
    /// that have no mtime.
    pub fn take_cache(&mut self) -> Option<Vec<(String, FileSymbols)>> {
        if self.running || !self.dirty {
            return None;
        }
        self.dirty = false;
        let mut out: Vec<(String, FileSymbols)> =
            self.files.iter().filter(|(_, f)| f.mtime.is_some()).map(|(p, f)| (p.clone(), f.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Some(out)
    }

    /// Apply a batch of run `seq`. Returns `None` when it is stale (dropped),
    /// else whether a held-back run should start now.
    pub fn on_batch(&mut self, seq: u64, batch: SymbolBatch) -> Option<bool> {
        if seq != self.seq {
            return None;
        }
        self.run.touch();
        self.dirty |= batch.changed;
        if !batch.files.is_empty() {
            self.entries = None;
            self.edges = None;
            self.by_name = None;
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

    fn edges(&mut self) -> &Edges {
        if self.edges.is_none() {
            let interned: HashMap<&str, Arc<str>> = self.files.keys().map(|k| (k.as_str(), Arc::from(k.as_str()))).collect();
            let mut keys: Vec<&str> = self.files.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let project = super::imports::Project::new(&keys);
            let mut e = Edges::default();
            for (path, f) in &self.files {
                let Some(lang) = syntax::detect(Path::new(path)).map(|d| d.name) else { continue };
                for imp in &f.imports {
                    for t in super::imports::resolve(lang, path, imp, &project) {
                        let Some(target) = interned.get(t.as_str()) else { continue };
                        e.out.entry(interned[path.as_str()].clone()).or_default().push(target.clone());
                        e.inc.entry(target.clone()).or_default().push(interned[path.as_str()].clone());
                    }
                }
            }
            for v in e.out.values_mut().chain(e.inc.values_mut()) {
                v.sort();
                v.dedup();
            }
            self.edges = Some(e);
        }
        self.edges.as_ref().expect("built above")
    }

    /// The indexed files `rel` imports, sorted, empty when unknown. The edge
    /// cache is rebuilt after any change.
    pub fn imports_of(&mut self, rel: &str) -> Vec<Arc<str>> {
        self.edges().out.get(rel).cloned().unwrap_or_default()
    }

    /// The indexed files that import `rel`, sorted, empty when unknown. The
    /// edge cache is rebuilt after any change.
    pub fn imported_by(&mut self, rel: &str) -> Vec<Arc<str>> {
        self.edges().inc.get(rel).cloned().unwrap_or_default()
    }

    /// A run is in flight or waiting.
    pub fn indexing(&self) -> bool {
        self.running
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The indexed data of `rel` (relative to the root).
    pub fn file(&self, rel: &str) -> Option<&FileSymbols> {
        self.files.get(rel)
    }

    /// Every definition named `name` (exact), `impl` blocks left out, in no particular order across files
    /// and in symbol order within one. The map is rebuilt on demand after any change.
    pub fn defs_of(&mut self, name: &str) -> Vec<(Arc<str>, Symbol)> {
        let files = &self.files;
        let map = self.by_name.get_or_insert_with(|| {
            let mut map = ByName::new();
            for (path, f) in files {
                let path: Arc<str> = Arc::from(path.as_str());
                for (i, s) in f.symbols.iter().enumerate().filter(|(_, s)| s.kind != "impl") {
                    map.entry(s.name.clone()).or_default().push((path.clone(), i));
                }
            }
            map
        });
        let Some(found) = map.get(name) else { return Vec::new() };
        found.iter().map(|(p, i)| (p.clone(), files[&**p].symbols[*i].clone())).collect()
    }

    /// Every indexed reference named `name` (exact), by path, then line and
    /// column. A linear scan over all files.
    pub fn refs_named(&self, name: &str) -> Vec<(&str, &Reference)> {
        let mut out: Vec<(&str, &Reference)> =
            self.files.iter().flat_map(|(p, f)| f.refs.iter().filter(|r| &*r.name == name).map(move |r| (p.as_str(), r))).collect();
        out.sort_by(|a, b| a.0.cmp(b.0).then(a.1.line.cmp(&b.1.line)).then(a.1.col.cmp(&b.1.col)));
        out
    }

    /// Every indexed definition named `name` (exact), `impl` blocks included,
    /// by path, then line and column. A linear scan over all files.
    pub fn defs_named(&self, name: &str) -> Vec<(&str, &Symbol)> {
        let mut out: Vec<(&str, &Symbol)> =
            self.files.iter().flat_map(|(p, f)| f.symbols.iter().filter(|s| &*s.name == name).map(move |s| (p.as_str(), s))).collect();
        out.sort_by(|a, b| a.0.cmp(b.0).then(a.1.line.cmp(&b.1.line)).then(a.1.col.cmp(&b.1.col)));
        out
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
                out.push(SymbolEntry { path: path.clone(), symbol: s.clone(), utf16: false });
            }
        }
        let e = Arc::new(out);
        self.entries = Some(e.clone());
        e
    }
}

impl IndexJob {
    /// Take unchanged files from the cache file (first run of a root only).
    pub fn with_cache(mut self, file: PathBuf) -> Self {
        self.cache = self.warm.then_some(file);
        self
    }

    /// Parse the files in order; `send` gets the batches, the last with
    /// `done`. Nothing is sent after a cancel.
    pub fn run(self, mut send: impl FnMut(SymbolBatch)) {
        let cancelled = || self.cancel.load(Ordering::Relaxed) != self.seq;
        let mut parser = tree_sitter::Parser::new();
        let mut batch = SymbolBatch::default();
        let mut last_send = Instant::now();
        let mut go_mods: HashMap<String, Option<String>> = HashMap::new();
        let mut cached = self.cache.as_deref().map(|f| super::cache::load(f, &self.root)).unwrap_or_default();
        let mut reused = 0usize;
        for (rel, known) in &self.files {
            if cancelled() {
                return;
            }
            let path = self.root.join(rel);
            let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file() && m.len() <= MAX_FILE_BYTES);
            let mtime = meta.as_ref().and_then(|m| m.modified().ok());
            let size = meta.as_ref().map(|m| m.len());
            let stamp = size.zip(mtime);
            if known.is_some() && *known == stamp {
                continue;
            }
            let was_cached = cached.remove(rel);
            // Held with a stamp, so it is (or may be) on disk: losing it must reach the cache file.
            let held = known.is_some() || was_cached.is_some();
            if let Some(c) = was_cached
                && stamp.is_some()
                && c.mtime.map(|m| (c.size, m)) == stamp
            {
                batch.files.push((rel.clone(), Some(c)));
                reused += 1;
                if batch.files.len() >= BATCH_FILES || last_send.elapsed() >= BATCH_EVERY {
                    send(std::mem::take(&mut batch));
                    last_send = Instant::now();
                }
                continue;
            }
            let syms = meta.and_then(|_| parse(&path, &mut parser)).map(|(symbols, refs, mut imports)| {
                if rel.ends_with(".go") {
                    super::imports::localize_go(rel, &mut imports, &mut |d| {
                        go_mods
                            .entry(d.to_string())
                            .or_insert_with(|| {
                                let p = if d.is_empty() { self.root.join("go.mod") } else { self.root.join(d).join("go.mod") };
                                std::fs::read_to_string(p).ok().and_then(|t| super::imports::go_module_path(&t))
                            })
                            .clone()
                    });
                }
                FileSymbols { mtime, size: size.unwrap_or(0), symbols, refs, imports }
            });
            batch.changed |= syms.is_some() || held;
            batch.files.push((rel.clone(), syms));
            if batch.files.len() >= BATCH_FILES || last_send.elapsed() >= BATCH_EVERY {
                send(std::mem::take(&mut batch));
                last_send = Instant::now();
            }
        }
        if self.cache.is_some() {
            tracing::debug!("symbol cache: reused {reused} of {} files", self.files.len());
        }
        if !cancelled() {
            batch.done = true;
            send(batch);
        }
    }
}

/// The symbols, references and imports of one file; `None` for binary or unreadable files.
fn parse(path: &Path, parser: &mut tree_sitter::Parser) -> Option<(Vec<Symbol>, Vec<Reference>, Vec<Import>)> {
    let def = syntax::detect(path).filter(|d| d.tags.is_some())?;
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    Some(super::extract_index(def, parser, &text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, line: usize) -> Reference {
        Reference { name: name.into(), kind: "call", line, col: 0, enclosing: None }
    }

    fn file(refs: Vec<Reference>) -> Option<FileSymbols> {
        Some(FileSymbols { mtime: None, size: 0, symbols: vec![], refs, imports: vec![] })
    }

    #[test]
    fn refs_named_sorts_by_path_then_line() {
        let mut idx = SymbolIndex::default();
        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        let batch = SymbolBatch {
            files: vec![("b.rs".into(), file(vec![r("f", 3), r("g", 1)])), ("a.rs".into(), file(vec![r("f", 9), r("f", 2)]))],
            done: true,
            changed: true,
        };
        assert!(idx.on_batch(seq, batch).is_some());
        let got: Vec<(&str, usize)> = idx.refs_named("f").into_iter().map(|(p, r)| (p, r.line)).collect();
        assert_eq!(got, [("a.rs", 2), ("a.rs", 9), ("b.rs", 3)]);
        assert!(idx.refs_named("F").is_empty());
        assert_eq!(idx.refs_named("g").len(), 1);
    }

    #[test]
    fn defs_named_sorts_by_path_then_line() {
        let sym =
            |name: &str, line| Symbol { name: name.into(), kind: "function", line, col: 3, start_line: line, end_line: line, depth: 0 };
        let f = |symbols| Some(FileSymbols { mtime: None, size: 0, symbols, refs: vec![], imports: vec![] });
        let mut idx = SymbolIndex::default();
        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        let batch = SymbolBatch {
            files: vec![("b.rs".into(), f(vec![sym("f", 3), sym("g", 1)])), ("a.rs".into(), f(vec![sym("f", 9), sym("f", 2)]))],
            done: true,
            changed: true,
        };
        assert!(idx.on_batch(seq, batch).is_some());
        let got: Vec<(&str, usize, usize)> = idx.defs_named("f").into_iter().map(|(p, s)| (p, s.line, s.col)).collect();
        assert_eq!(got, [("a.rs", 2, 3), ("a.rs", 9, 3), ("b.rs", 3, 3)]);
        assert!(idx.defs_named("F").is_empty());
        assert_eq!(idx.defs_named("g").len(), 1);
    }

    #[test]
    fn defs_of_follows_every_change() {
        let sym = |name: &str, kind, line| Symbol { name: name.into(), kind, line, col: 0, start_line: line, end_line: line, depth: 0 };
        let f = |symbols| Some(FileSymbols { mtime: None, size: 0, symbols, refs: vec![], imports: vec![] });
        let lines = |idx: &mut SymbolIndex, name: &str| -> Vec<(String, usize, &'static str)> {
            idx.defs_of(name).into_iter().map(|(p, s)| (p.to_string(), s.line, s.kind)).collect()
        };
        let mut idx = SymbolIndex::default();
        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        let first = SymbolBatch {
            files: vec![("a.rs".into(), f(vec![sym("f", "function", 5), sym("g", "function", 1), sym("f", "function", 2)]))],
            done: true,
            changed: true,
        };
        assert!(idx.on_batch(seq, first).is_some());
        assert_eq!(lines(&mut idx, "f"), [("a.rs".to_string(), 5, "function"), ("a.rs".to_string(), 2, "function")]);

        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        let second = SymbolBatch {
            files: vec![
                ("a.rs".into(), f(vec![sym("g", "function", 7)])),
                ("b.rs".into(), f(vec![sym("f", "function", 3), sym("f", "impl", 1)])),
            ],
            done: true,
            changed: true,
        };
        assert!(idx.on_batch(seq, second).is_some());
        assert_eq!(lines(&mut idx, "f"), [("b.rs".to_string(), 3, "function")]);
        assert_eq!(lines(&mut idx, "g"), [("a.rs".to_string(), 7, "function")]);

        idx.invalidate(&["b.rs".to_string()]);
        assert!(idx.defs_of("f").is_empty());
        idx.reset();
        assert!(idx.defs_of("g").is_empty());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn defs_of_forgets_files_set_files_drops() {
        let sym = Symbol { name: "f".into(), kind: "function", line: 0, col: 0, start_line: 0, end_line: 0, depth: 0 };
        let f = || Some(FileSymbols { mtime: None, size: 0, symbols: vec![sym.clone()], refs: vec![], imports: vec![] });
        let mut idx = SymbolIndex::default();
        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        let batch = SymbolBatch { files: vec![("a.rs".into(), f()), ("b.rs".into(), f())], done: true, changed: true };
        assert!(idx.on_batch(seq, batch).is_some());
        assert_eq!(idx.defs_of("f").len(), 2);
        idx.set_files(&["a.rs".to_string()]);
        let got: Vec<String> = idx.defs_of("f").into_iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(got, ["a.rs"]);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn index_run_keeps_references() {
        let dir = std::env::temp_dir().join(format!("jetl-symrefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() { b(); }\n").unwrap();
        let mut idx = SymbolIndex::default();
        idx.set_files(&["a.rs".to_string()]);
        let job = idx.start(dir.clone());
        let seq = job.seq;
        let mut batches = Vec::new();
        job.run(|b| batches.push(b));
        for b in batches {
            idx.on_batch(seq, b);
        }
        let want = Reference { name: "b".into(), kind: "call", line: 0, col: 9, enclosing: Some(0) };
        assert_eq!(idx.refs_named("b"), [("a.rs", &want)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fs(imports: &[(&str, bool)]) -> Option<FileSymbols> {
        let imports = imports.iter().map(|(spec, decl)| Import { spec: (*spec).into(), line: 0, decl: *decl }).collect();
        Some(FileSymbols { mtime: None, size: 0, symbols: vec![], refs: vec![], imports })
    }

    fn strs(v: Vec<Arc<str>>) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn import_edges_follow_batches() {
        let mut idx = SymbolIndex::default();
        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        let files = vec![
            ("src/main.rs".to_string(), fs(&[("app", true), ("crate::config::Config", false), ("std::fs", false)])),
            ("src/app/mod.rs".to_string(), fs(&[("super::config", false), ("crate::{x}", false)])),
            ("src/config.rs".to_string(), fs(&[])),
            ("main.py".to_string(), fs(&[("pkg.a", false), ("pkg.b", false)])),
            ("pkg/__init__.py".to_string(), fs(&[])),
            ("pkg/a.py".to_string(), fs(&[(".b", false)])),
            ("pkg/b.py".to_string(), fs(&[])),
        ];
        assert!(idx.on_batch(seq, SymbolBatch { files, done: true, changed: true }).is_some());
        #[cfg(feature = "lang-rust")]
        {
            assert_eq!(strs(idx.imports_of("src/main.rs")), ["src/app/mod.rs", "src/config.rs"]);
            assert_eq!(strs(idx.imported_by("src/config.rs")), ["src/app/mod.rs", "src/main.rs"]);
        }
        #[cfg(feature = "lang-python")]
        {
            assert_eq!(strs(idx.imports_of("main.py")), ["pkg/a.py", "pkg/b.py"]);
            assert_eq!(strs(idx.imported_by("pkg/b.py")), ["main.py", "pkg/a.py"]);
        }
        assert!(idx.imports_of("nope.rs").is_empty());
        let seq = idx.start(PathBuf::from("/nonexistent")).seq;
        let batch = SymbolBatch { files: vec![("src/config.rs".to_string(), None)], done: true, changed: true };
        assert!(idx.on_batch(seq, batch).is_some());
        assert!(idx.imported_by("src/config.rs").is_empty());
        #[cfg(feature = "lang-rust")]
        assert_eq!(strs(idx.imports_of("src/main.rs")), ["src/app/mod.rs"]);
    }

    #[cfg(all(feature = "lang-rust", feature = "lang-python"))]
    #[test]
    fn index_run_builds_import_edges() {
        let dir = std::env::temp_dir().join(format!("jetl-symimports-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        let files = [
            ("src/main.rs", "mod a;\nuse crate::b::B;\nfn main() {}\n"),
            ("src/a.rs", "use super::b;\n"),
            ("src/b.rs", "pub struct B;\n"),
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "from . import b\n"),
            ("pkg/b.py", "x = 1\n"),
        ];
        for (p, c) in files {
            std::fs::write(dir.join(p), c).unwrap();
        }
        let mut idx = SymbolIndex::default();
        idx.set_files(&files.iter().map(|(p, _)| p.to_string()).collect::<Vec<_>>());
        let job = idx.start(dir.clone());
        let seq = job.seq;
        let mut batches = Vec::new();
        job.run(|b| batches.push(b));
        for b in batches {
            idx.on_batch(seq, b);
        }
        assert_eq!(strs(idx.imports_of("src/main.rs")), ["src/a.rs", "src/b.rs"]);
        assert_eq!(strs(idx.imported_by("src/b.rs")), ["src/a.rs", "src/main.rs"]);
        assert_eq!(strs(idx.imports_of("pkg/a.py")), ["pkg/b.py"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn index_run_resolves_go_packages() {
        let dir = std::env::temp_dir().join(format!("jetl-symgo-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("internal/foo/sub")).unwrap();
        let files = [
            (
                "main.go",
                "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/app/internal/foo\"\n)\n\nfunc main() { fmt.Println(foo.A) }\n",
            ),
            ("internal/foo/foo.go", "package foo\n\nvar A = 1\n"),
            ("internal/foo/bar.go", "package foo\n"),
            ("internal/foo/foo_test.go", "package foo\n"),
            ("internal/foo/sub/s.go", "package sub\n"),
        ];
        for (p, c) in files {
            std::fs::write(dir.join(p), c).unwrap();
        }
        std::fs::write(dir.join("go.mod"), "module example.com/app\n\ngo 1.22\n").unwrap();
        let mut idx = SymbolIndex::default();
        idx.set_files(&files.iter().map(|(p, _)| p.to_string()).collect::<Vec<_>>());
        let job = idx.start(dir.clone());
        let seq = job.seq;
        let mut batches = Vec::new();
        job.run(|b| batches.push(b));
        for b in batches {
            idx.on_batch(seq, b);
        }
        assert_eq!(strs(idx.imports_of("main.go")), ["internal/foo/bar.go", "internal/foo/foo.go"]);
        assert_eq!(strs(idx.imported_by("internal/foo/foo.go")), ["main.go"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
