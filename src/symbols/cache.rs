//! The symbol index of one root on disk, so a start parses only the files
//! that changed since the last run.
//!
//! One JSON file per root: `<cache_dir>/index/<fnv1a64 of root as 016x>.json`
//! (the key of the session file). Header `{"format", "queries", "root",
//! "files"}`: `queries` is `fnv1a64` over every language's name, tags and
//! imports query texts and the versions of the tree-sitter packages in
//! `Cargo.lock`, so a query change or a grammar bump invalidates the file; a header
//! mismatch or a parse error discards it (debug log only). A file entry is
//! reused only when its size and mtime still match (`index.rs` compares).
//! `kind: &'static str` is interned back on load against the capture names
//! of the loaded tags queries and `NODE_KINDS`; an unknown kind drops that
//! file's entry.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::index::FileSymbols;
use super::{Import, Reference, Symbol};
use crate::editor::syntax::LANGUAGES;
use crate::util::fs::write_atomic;
use crate::workspace::fnv1a64;

/// Bump when extraction output changes without a query text change
/// (`collect`, `kind_label`, `clean_name`, `imports::extract`, `localize_go`).
/// A query text or tree-sitter package version change needs no bump.
/// 2: Rust calls inside macro arguments (yman 446).
pub const FORMAT: u32 = 2;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    queries: String,
    root: String,
    files: BTreeMap<String, CachedFile>,
}

#[derive(Serialize, Deserialize)]
struct CachedFile {
    size: u64,
    secs: u64,
    nanos: u32,
    symbols: Vec<CachedSymbol>,
    refs: Vec<CachedRef>,
    imports: Vec<Import>,
}

#[derive(Serialize, Deserialize)]
struct CachedSymbol {
    name: String,
    kind: String,
    line: usize,
    col: usize,
    start_line: usize,
    end_line: usize,
    depth: usize,
}

#[derive(Serialize, Deserialize)]
struct CachedRef {
    name: String,
    kind: String,
    line: usize,
    col: usize,
    enclosing: Option<usize>,
}

/// The cache file of `root`.
pub fn file(cache_dir: &Path, root: &Path) -> PathBuf {
    cache_dir.join("index").join(format!("{:016x}.json", fnv1a64(root.as_os_str().as_encoded_bytes())))
}

/// The lock file this build was compiled from.
const LOCK: &str = include_str!("../../Cargo.lock");

/// `name version`, one per line, of the tree-sitter runtime and every `tree-sitter-*` package in `lock`, in file order.
fn grammar_versions(lock: &str) -> String {
    let mut out = String::new();
    let mut name = None;
    for line in lock.lines() {
        if let Some(n) = line.strip_prefix("name = ") {
            name = Some(n.trim_matches('"'));
        } else if let Some(v) = line.strip_prefix("version = ")
            && let Some(n) = name.take()
            && (n == "tree-sitter" || n.starts_with("tree-sitter-"))
        {
            out.push_str(n);
            out.push(' ');
            out.push_str(v.trim_matches('"'));
            out.push('\n');
        }
    }
    out
}

/// Hash of every language's name and query texts, in `LANGUAGES` order, and of the tree-sitter package versions in `Cargo.lock`.
pub fn queries_hash() -> u64 {
    hash_with(LOCK)
}

fn hash_with(lock: &str) -> u64 {
    let mut bytes = Vec::new();
    for def in LANGUAGES.iter() {
        bytes.extend_from_slice(def.name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(def.tags.unwrap_or("").as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(def.imports.unwrap_or("").as_bytes());
        bytes.push(0);
    }
    bytes.extend_from_slice(grammar_versions(lock).as_bytes());
    fnv1a64(&bytes)
}

fn queries_key() -> String {
    format!("{:016x}", queries_hash())
}

/// Every kind a symbol or reference can have.
fn kinds() -> HashSet<&'static str> {
    let mut out: HashSet<&'static str> = super::NODE_KINDS.into_iter().collect();
    for def in LANGUAGES.iter() {
        let Some(query) = def.tags_query() else { continue };
        for name in query.capture_names() {
            if let Some(k) = name.strip_prefix("definition.").or_else(|| name.strip_prefix("reference.")) {
                out.insert(k);
            }
        }
    }
    out
}

/// Writes `files` (entries without an mtime skipped); creates the parent dir.
pub fn save(file: &Path, root: &Path, files: &[(String, FileSymbols)]) -> std::io::Result<()> {
    let mut map = BTreeMap::new();
    for (rel, f) in files {
        let Some(d) = f.mtime.and_then(|t| t.duration_since(UNIX_EPOCH).ok()) else { continue };
        let symbols = f
            .symbols
            .iter()
            .map(|s| CachedSymbol {
                name: s.name.to_string(),
                kind: s.kind.to_string(),
                line: s.line,
                col: s.col,
                start_line: s.start_line,
                end_line: s.end_line,
                depth: s.depth,
            })
            .collect();
        let refs = f
            .refs
            .iter()
            .map(|r| CachedRef { name: r.name.to_string(), kind: r.kind.to_string(), line: r.line, col: r.col, enclosing: r.enclosing })
            .collect();
        map.insert(
            rel.clone(),
            CachedFile { size: f.size, secs: d.as_secs(), nanos: d.subsec_nanos(), symbols, refs, imports: f.imports.clone() },
        );
    }
    let cache = CacheFile { format: FORMAT, queries: queries_key(), root: root.to_string_lossy().into_owned(), files: map };
    let text = serde_json::to_string(&cache).map_err(std::io::Error::other)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(file, &text)?;
    Ok(())
}

/// The cached entries, empty (with a `tracing::debug!`) when the file is missing, unparsable,
/// or its format, queries or root differ. A file entry with an unknown kind is left out.
pub fn load(file: &Path, root: &Path) -> HashMap<String, FileSymbols> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("symbol cache {}: {e}", file.display());
            return HashMap::new();
        }
    };
    let cache: CacheFile = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("symbol cache {}: unparsable: {e}", file.display());
            return HashMap::new();
        }
    };
    if cache.format != FORMAT {
        tracing::debug!("symbol cache: format {} differs", cache.format);
        return HashMap::new();
    }
    if cache.queries != queries_key() {
        tracing::debug!("symbol cache: queries differ");
        return HashMap::new();
    }
    if cache.root != root.to_string_lossy() {
        tracing::debug!("symbol cache: root differs");
        return HashMap::new();
    }
    let kinds = kinds();
    let mut out = HashMap::new();
    'files: for (rel, f) in cache.files {
        let mut symbols = Vec::with_capacity(f.symbols.len());
        for s in f.symbols {
            let Some(kind) = kinds.get(s.kind.as_str()).copied() else {
                tracing::debug!("symbol cache: {rel}: unknown kind {}", s.kind);
                continue 'files;
            };
            symbols.push(Symbol {
                name: s.name.into(),
                kind,
                line: s.line,
                col: s.col,
                start_line: s.start_line,
                end_line: s.end_line,
                depth: s.depth,
            });
        }
        let mut refs = Vec::with_capacity(f.refs.len());
        for r in f.refs {
            let Some(kind) = kinds.get(r.kind.as_str()).copied() else {
                tracing::debug!("symbol cache: {rel}: unknown kind {}", r.kind);
                continue 'files;
            };
            refs.push(Reference { name: r.name.into(), kind, line: r.line, col: r.col, enclosing: r.enclosing });
        }
        let mtime: SystemTime = UNIX_EPOCH + Duration::new(f.secs, f.nanos);
        out.insert(rel, FileSymbols { mtime: Some(mtime), size: f.size, symbols, refs, imports: f.imports });
    }
    out
}

#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;
    use crate::symbols::index::SymbolIndex;

    fn tmp(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-symcache-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, mtime: Option<SystemTime>) -> FileSymbols {
        FileSymbols {
            mtime,
            size: 12,
            symbols: vec![Symbol { name: name.into(), kind: "function", line: 1, col: 3, start_line: 1, end_line: 4, depth: 0 }],
            refs: vec![Reference { name: "g".into(), kind: "call", line: 2, col: 5, enclosing: Some(0) }],
            imports: vec![Import { spec: "crate::a".into(), line: 0, decl: false }],
        }
    }

    fn stamp() -> Option<SystemTime> {
        Some(UNIX_EPOCH + Duration::new(1_700_000_000, 5))
    }

    #[test]
    fn round_trip_keeps_entries() {
        let dir = tmp("rt");
        let (f, root) = (dir.join("c.json"), dir.join("root"));
        save(&f, &root, &[("a.rs".into(), entry("f", stamp())), ("b.rs".into(), entry("h", None))]).unwrap();
        let got = load(&f, &root);
        assert_eq!(got.len(), 1);
        let want = entry("f", stamp());
        let a = &got["a.rs"];
        assert_eq!((a.symbols.as_slice(), a.refs.as_slice(), a.imports.as_slice()), (&*want.symbols, &*want.refs, &*want.imports));
        assert_eq!((a.size, a.mtime), (12, stamp()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn edit(f: &Path, change: impl FnOnce(&mut serde_json::Value)) {
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        change(&mut v);
        std::fs::write(f, v.to_string()).unwrap();
    }

    #[test]
    fn header_mismatch_discards() {
        let dir = tmp("hdr");
        let (f, root) = (dir.join("c.json"), dir.join("root"));
        let write = || save(&f, &root, &[("a.rs".into(), entry("f", stamp()))]).unwrap();
        write();
        assert_eq!(load(&f, &root).len(), 1);
        assert!(load(&f, &dir.join("other")).is_empty());
        edit(&f, |v| v["queries"] = "0000000000000000".into());
        assert!(load(&f, &root).is_empty());
        write();
        edit(&f, |v| v["format"] = (FORMAT - 1).into());
        assert!(load(&f, &root).is_empty());
        write();
        edit(&f, |v| v["format"] = (FORMAT + 1).into());
        assert!(load(&f, &root).is_empty());
        std::fs::write(&f, "not json").unwrap();
        assert!(load(&f, &root).is_empty());
        assert!(load(&dir.join("missing.json"), &root).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_kind_drops_that_file() {
        let dir = tmp("kind");
        let (f, root) = (dir.join("c.json"), dir.join("root"));
        save(&f, &root, &[("a.rs".into(), entry("f", stamp())), ("b.rs".into(), entry("h", stamp()))]).unwrap();
        edit(&f, |v| v["files"]["a.rs"]["symbols"][0]["kind"] = "no-such-kind".into());
        let got = load(&f, &root);
        assert_eq!(got.keys().collect::<Vec<_>>(), ["b.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn drain(idx: &mut SymbolIndex, job: crate::symbols::index::IndexJob) -> Vec<bool> {
        let seq = job.seq;
        let mut batches = Vec::new();
        job.run(|b| batches.push(b));
        let changed = batches.iter().map(|b| b.changed).collect();
        for b in batches {
            idx.on_batch(seq, b);
        }
        changed
    }

    fn run(root: &Path, cache: &Path) -> (SymbolIndex, Vec<bool>) {
        let mut idx = SymbolIndex::default();
        idx.set_files(&["a.rs".to_string(), "b.rs".to_string()]);
        let job = idx.start(root.to_path_buf()).with_cache(cache.to_path_buf());
        let changed = drain(&mut idx, job);
        (idx, changed)
    }

    #[test]
    fn warm_run_parses_no_unchanged_file() {
        let dir = tmp("warm");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let cache = dir.join("index").join("c.json");
        std::fs::write(root.join("a.rs"), "fn alpha() { beta(); }\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn beta() {}\n").unwrap();
        let (mut cold, changed) = run(&root, &cache);
        assert!(changed.contains(&true));
        let files = cold.take_cache().expect("a cold run changed things");
        save(&cache, &root, &files).unwrap();
        let mtime = std::fs::metadata(root.join("a.rs")).unwrap().modified().unwrap();
        std::fs::write(root.join("a.rs"), "fn gamma() { beta(); }\n").unwrap();
        std::fs::File::options().write(true).open(root.join("a.rs")).unwrap().set_modified(mtime).unwrap();

        let (mut warm, changed) = run(&root, &cache);
        assert!(!changed.contains(&true), "nothing parsed");
        let names: Vec<String> = warm.entries().iter().map(|e| e.symbol.name.to_string()).collect();
        assert!(names.contains(&"alpha".to_string()) && !names.contains(&"gamma".to_string()), "{names:?}");
        assert_eq!(warm.refs_named("beta").iter().filter(|(p, _)| *p == "a.rs").count(), 1);
        assert!(warm.take_cache().is_none());

        std::fs::write(root.join("b.rs"), "fn beta() {}\nfn more() {}\n").unwrap();
        let (mut third, changed) = run(&root, &cache);
        assert!(changed.contains(&true));
        assert!(third.take_cache().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh root with `b.rs` and the cache path; `a.rs` is the caller's.
    fn project(what: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = tmp(what);
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("b.rs"), "fn beta() {}\n").unwrap();
        let cache = dir.join("index").join("c.json");
        (dir, root, cache)
    }

    #[test]
    fn unparseable_file_leaves_the_cache_untouched() {
        let big = vec![b'/'; crate::symbols::index::MAX_FILE_BYTES as usize + 1];
        let cases: [(&str, Option<&[u8]>); 3] = [("nul", Some(b"fn a() {}\0\n")), ("big", Some(&big)), ("gone", None)];
        for (what, content) in cases {
            let (dir, root, cache) = project(what);
            if let Some(c) = content {
                std::fs::write(root.join("a.rs"), c).unwrap();
            }
            let (mut cold, changed) = run(&root, &cache);
            assert!(changed.contains(&true), "{what}: cold run");
            save(&cache, &root, &cold.take_cache().unwrap()).unwrap();
            let (mut warm, changed) = run(&root, &cache);
            assert!(!changed.contains(&true), "{what}: second run");
            assert!(warm.take_cache().is_none(), "{what}");
            assert!(warm.file("a.rs").is_none() && warm.file("b.rs").is_some(), "{what}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn cached_file_that_stops_parsing_rewrites_the_cache() {
        let (dir, root, cache) = project("stops");
        std::fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();
        let (mut cold, _) = run(&root, &cache);
        save(&cache, &root, &cold.take_cache().unwrap()).unwrap();
        std::fs::write(root.join("a.rs"), "fn alpha() {}\0\n").unwrap();
        let (mut warm, changed) = run(&root, &cache);
        assert!(changed.contains(&true));
        let files = warm.take_cache().unwrap();
        assert!(files.iter().all(|(p, _)| p != "a.rs") && files.iter().any(|(p, _)| p == "b.rs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn held_file_that_stops_parsing_marks_the_index_dirty() {
        let (dir, root, _) = project("held");
        std::fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();
        let mut idx = SymbolIndex::default();
        let files = ["a.rs".to_string(), "b.rs".to_string()];
        idx.set_files(&files);
        let job = idx.start(root.clone());
        drain(&mut idx, job);
        idx.take_cache().unwrap();
        std::fs::write(root.join("a.rs"), "fn alpha() {}\0\n").unwrap();
        idx.set_files(&files);
        let job = idx.start(root.clone());
        assert!(drain(&mut idx, job).contains(&true));
        let files = idx.take_cache().unwrap();
        assert!(files.iter().all(|(p, _)| p != "a.rs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalidated_file_that_stops_parsing_marks_the_index_dirty() {
        let (dir, root, _) = project("inval");
        std::fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();
        let mut idx = SymbolIndex::default();
        idx.set_files(&["a.rs".to_string(), "b.rs".to_string()]);
        let job = idx.start(root.clone());
        drain(&mut idx, job);
        idx.take_cache().unwrap();
        std::fs::write(root.join("a.rs"), "fn alpha() {}\0\n").unwrap();
        idx.invalidate(&["a.rs".to_string()]);
        let job = idx.start(root.clone());
        assert!(!drain(&mut idx, job).contains(&true));
        let files = idx.take_cache().unwrap();
        assert!(files.iter().all(|(p, _)| p != "a.rs"));
        idx.invalidate(&["a.rs".to_string()]);
        let job = idx.start(root.clone());
        drain(&mut idx, job);
        assert!(idx.take_cache().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn lock(ts: &str, rust: &str, serde: &str, sum: &str) -> String {
        format!(
            "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"{serde}\"\nsource = \"registry+x\"\nchecksum = \"{sum}\"\n\n\
             [[package]]\nname = \"tree-sitter\"\nversion = \"{ts}\"\nsource = \"registry+x\"\nchecksum = \"{sum}\"\ndependencies = [\n \"streaming-iterator\",\n \"tree-sitter-language\",\n]\n\n\
             [[package]]\nname = \"tree-sitter-language\"\nversion = \"0.1.5\"\n\n\
             [[package]]\nname = \"tree-sitter-rust\"\nversion = \"{rust}\"\nsource = \"registry+x\"\n\n\
             [[package]]\nname = \"streaming-iterator\"\nversion = \"0.1.9\"\n"
        )
    }

    #[test]
    fn grammar_versions_keeps_only_tree_sitter_packages() {
        let text = lock("0.27.0", "0.24.0", "1.0.0", "abc");
        let want = "tree-sitter 0.27.0\ntree-sitter-language 0.1.5\ntree-sitter-rust 0.24.0\n";
        assert_eq!(grammar_versions(&text), want);
        assert_eq!(grammar_versions(&text.replace('\n', "\r\n")), want);
    }

    #[test]
    fn grammar_bump_changes_the_hash() {
        let base = hash_with(&lock("0.27.0", "0.24.0", "1.0.0", "abc"));
        assert_ne!(base, hash_with(&lock("0.27.0", "0.24.1", "1.0.0", "abc")));
        assert_ne!(base, hash_with(&lock("0.28.0", "0.24.0", "1.0.0", "abc")));
        assert_eq!(base, hash_with(&lock("0.27.0", "0.24.0", "1.0.1", "abc")));
        assert_eq!(base, hash_with(&lock("0.27.0", "0.24.0", "1.0.0", "def")));
    }

    #[test]
    fn real_lock_lists_the_grammars() {
        let got = grammar_versions(LOCK);
        assert!(got.lines().any(|l| l.starts_with("tree-sitter ")), "{got}");
        assert!(got.lines().any(|l| l.starts_with("tree-sitter-rust ")), "{got}");
        assert!(!got.lines().any(|l| l.starts_with("serde ")), "{got}");
    }
}
