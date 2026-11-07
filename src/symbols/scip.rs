//! A SCIP code index (`index.scip`, for example from `rust-analyzer scip .`)
//! turned into a compact lookup structure: documents keyed by root-relative
//! path, occurrences sorted by position, symbols interned, and an inverted
//! list per symbol. A document's `SymbolInformation` is read for the relationships that link an implementation to what
//! it implements and for the kinds of abstract types and members ([`ScipIndex::rename_set`]).
//!
//! Whether a document may still be trusted is the app's decision (it needs
//! buffers and mtimes); [`file_fresh`] is the rule it applies. Columns stay in the document's own units
//! ([`Encoding`]); callers convert with the line text ([`to_chars`],
//! [`to_units`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use protobuf::Message;
use scip::types::symbol_information::Kind;
use scip::types::{Index, PositionEncoding, SymbolRole, occurrence};

/// Larger files are refused unread: loading builds the whole protobuf message first, about 7x the file size at its peak
/// (jetl's own index, 34 MiB, 2026-11-05: peak 6.8x, retained 3.6x before `release_freed_memory`). Keeps that peak near 2 GiB.
pub(crate) const MAX_SCIP_BYTES: u64 = 256 * 1024 * 1024;

/// The parsed protobuf is many small allocations, freed in the middle of the heap, where glibc keeps them
/// resident. Hand those pages back; this is off the main thread and runs once per load.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn release_freed_memory() {
    // SAFETY: `malloc_trim` takes no pointers and is safe to call from any thread.
    unsafe { libc::malloc_trim(0) };
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn release_freed_memory() {}

/// Whether an index written at `index_mtime` may be trusted for the file at `abs`: not edited in an open buffer, and
/// not newer on disk than the index. The one rule behind `App::scip_fresh` and the SCIP rename.
pub fn file_fresh(abs: &Path, dirty: bool, index_mtime: SystemTime) -> bool {
    !dirty && std::fs::metadata(abs).and_then(|m| m.modified()).is_ok_and(|t| t <= index_mtime)
}

/// The unit of the columns in one document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    fn width(self, c: char) -> u32 {
        match self {
            Encoding::Utf8 => c.len_utf8() as u32,
            Encoding::Utf16 => c.len_utf16() as u32,
            Encoding::Utf32 => 1,
        }
    }
}

/// The char column of a column in `enc` units on `line`; past the end, the
/// line's char count.
pub fn to_chars(line: &str, units: u32, enc: Encoding) -> usize {
    let mut total = 0;
    let mut n = 0;
    for c in line.chars() {
        if total >= units {
            return n;
        }
        total += enc.width(c);
        n += 1;
    }
    n
}

/// The column in `enc` units of the char column `chars` on `line`.
pub fn to_units(line: &str, chars: usize, enc: Encoding) -> u32 {
    line.chars().take(chars).map(|c| enc.width(c)).sum()
}

fn encoding_of(e: protobuf::EnumOrUnknown<PositionEncoding>) -> Encoding {
    match e.enum_value() {
        Ok(PositionEncoding::UTF8CodeUnitOffsetFromLineStart) => Encoding::Utf8,
        Ok(PositionEncoding::UTF32CodeUnitOffsetFromLineStart) => Encoding::Utf32,
        // LSP's unit; older indexers leave the encoding unspecified.
        _ => Encoding::Utf16,
    }
}

/// A half-open range in document units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub start: u32,
    pub end_line: u32,
    pub end: u32,
}

impl Span {
    pub fn single_line(&self) -> bool {
        self.line == self.end_line
    }
}

fn span(line: i32, start: i32, end_line: i32, end: i32) -> Option<Span> {
    Some(Span {
        line: u32::try_from(line).ok()?,
        start: u32::try_from(start).ok()?,
        end_line: u32::try_from(end_line).ok()?,
        end: u32::try_from(end).ok()?,
    })
}

fn legacy_span(r: &[i32]) -> Option<Span> {
    match r {
        [l, s, e] => span(*l, *s, *l, *e),
        [l, s, el, e] => span(*l, *s, *el, *e),
        _ => None,
    }
}

fn span_of(typed: Option<&occurrence::Typed_range>, legacy: &[i32]) -> Option<Span> {
    match typed {
        Some(occurrence::Typed_range::SingleLineRange(r)) => span(r.line, r.start_character, r.line, r.end_character),
        Some(occurrence::Typed_range::MultiLineRange(r)) => span(r.start_line, r.start_character, r.end_line, r.end_character),
        _ => legacy_span(legacy),
    }
}

fn enclosing_of(typed: Option<&occurrence::Typed_enclosing_range>, legacy: &[i32]) -> Option<Span> {
    match typed {
        Some(occurrence::Typed_enclosing_range::SingleLineEnclosingRange(r)) => span(r.line, r.start_character, r.line, r.end_character),
        Some(occurrence::Typed_enclosing_range::MultiLineEnclosingRange(r)) => {
            span(r.start_line, r.start_character, r.end_line, r.end_character)
        }
        _ => legacy_span(legacy),
    }
}

pub type SymId = u32;

/// One occurrence of a symbol in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occ {
    pub span: Span,
    pub symbol: SymId,
    pub definition: bool,
    /// The whole definition (body included), when the indexer gave it.
    pub enclosing: Option<Span>,
}

#[derive(Debug)]
pub struct ScipDoc {
    pub encoding: Encoding,
    /// Sorted by `(span.line, span.start, span.end_line, span.end)`.
    pub occurrences: Vec<Occ>,
}

#[derive(Debug)]
pub struct ScipIndex {
    file: PathBuf,
    mtime: SystemTime,
    docs: HashMap<String, ScipDoc>,
    names: Vec<Arc<str>>,
    ids: HashMap<Arc<str>, SymId>,
    /// Per symbol: (index into `doc_paths`, index into that document's occurrences).
    by_symbol: Vec<Vec<(u32, u32)>>,
    doc_paths: Vec<String>,
    /// Undirected links between global symbols from relationships with `is_implementation` or `is_reference`, both
    /// ends interned, each list sorted.
    related: HashMap<SymId, Vec<SymId>>,
    /// Global symbols of kind Trait, Interface, Protocol or TypeClass.
    abstract_types: HashSet<SymId>,
    /// Global symbols of kind TraitMethod, AbstractMethod, ProtocolMethod or TypeClassMethod.
    abstract_members: HashSet<SymId>,
    /// Set when the index has documents but none lands under the root.
    warning: Option<String>,
}

impl ScipIndex {
    /// Reads and converts `file`. Blocking: call it off the main thread.
    pub fn load(file: &Path, root: &Path) -> Result<ScipIndex, String> {
        let meta = std::fs::metadata(file).map_err(|e| e.to_string())?;
        let mtime = meta.modified().map_err(|e| e.to_string())?;
        if meta.len() > MAX_SCIP_BYTES {
            return Err(format!("{} MiB is above the limit of {} MiB", meta.len() >> 20, MAX_SCIP_BYTES >> 20));
        }
        let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
        let index = Index::parse_from_bytes(&bytes).map_err(|e| format!("not a SCIP index: {e}"))?;
        let ix = ScipIndex::from_proto(index, root, file.to_path_buf(), mtime);
        drop(bytes);
        release_freed_memory();
        Ok(ix)
    }

    pub fn from_proto(index: Index, root: &Path, file: PathBuf, mtime: SystemTime) -> ScipIndex {
        let base = url::Url::parse(&index.metadata.project_root)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| root.to_path_buf());
        let mut ix = ScipIndex {
            file,
            mtime,
            docs: HashMap::new(),
            names: Vec::new(),
            ids: HashMap::new(),
            by_symbol: Vec::new(),
            doc_paths: Vec::new(),
            related: HashMap::new(),
            abstract_types: HashSet::new(),
            abstract_members: HashSet::new(),
            warning: None,
        };
        let total = index.documents.len();
        for doc in index.documents {
            let abs = base.join(&doc.relative_path);
            let Ok(rel) = abs.strip_prefix(root) else { continue };
            // `..` components survive `join`; `strip_prefix` is lexical, so a path
            // that climbs out of the root must not count as inside it.
            if rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                continue;
            }
            let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            if rel.is_empty() || ix.docs.contains_key(&rel) {
                continue;
            }
            let mut occurrences = Vec::with_capacity(doc.occurrences.len());
            for o in doc.occurrences {
                if o.symbol.is_empty() {
                    continue;
                }
                let Some(span) = span_of(o.typed_range.as_ref(), &o.range) else { continue };
                let key: Arc<str> = if scip::symbol::is_local_symbol(&o.symbol) {
                    format!("{rel}\u{0}{}", o.symbol).into()
                } else {
                    o.symbol.as_str().into()
                };
                let symbol = ix.intern(key);
                occurrences.push(Occ {
                    span,
                    symbol,
                    definition: o.symbol_roles & (SymbolRole::Definition as i32) != 0,
                    enclosing: enclosing_of(o.typed_enclosing_range.as_ref(), &o.enclosing_range),
                });
            }
            occurrences.sort_by_key(|o| (o.span.line, o.span.start, o.span.end_line, o.span.end));
            let d = ix.doc_paths.len() as u32;
            ix.doc_paths.push(rel.clone());
            for (i, o) in occurrences.iter().enumerate() {
                ix.by_symbol[o.symbol as usize].push((d, i as u32));
            }
            for info in doc.symbols {
                if info.symbol.is_empty() || scip::symbol::is_local_symbol(&info.symbol) {
                    continue;
                }
                let class = match info.kind.enum_value() {
                    Ok(Kind::Trait | Kind::Interface | Kind::Protocol | Kind::TypeClass) => Some(true),
                    Ok(Kind::TraitMethod | Kind::AbstractMethod | Kind::ProtocolMethod | Kind::TypeClassMethod) => Some(false),
                    _ => None,
                };
                let links: Vec<&str> = info
                    .relationships
                    .iter()
                    .filter(|r| r.is_implementation || r.is_reference)
                    .map(|r| r.symbol.as_str())
                    .filter(|t| !t.is_empty() && !scip::symbol::is_local_symbol(t) && *t != info.symbol)
                    .collect();
                if class.is_none() && links.is_empty() {
                    continue;
                }
                let id = ix.intern(info.symbol.as_str().into());
                match class {
                    Some(true) => {
                        ix.abstract_types.insert(id);
                    }
                    Some(false) => {
                        ix.abstract_members.insert(id);
                    }
                    None => {}
                }
                for target in links {
                    let to = ix.intern(target.into());
                    ix.related.entry(id).or_default().push(to);
                    ix.related.entry(to).or_default().push(id);
                }
            }
            let encoding = encoding_of(doc.position_encoding);
            ix.docs.insert(rel, ScipDoc { encoding, occurrences });
        }
        for list in ix.related.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        if total > 0 && ix.docs.is_empty() {
            ix.warning = Some(format!(
                "{}: no document under {} (project_root {})",
                ix.file.file_name().unwrap_or_default().to_string_lossy(),
                root.display(),
                index.metadata.project_root
            ));
        }
        ix
    }

    fn intern(&mut self, key: Arc<str>) -> SymId {
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        let id = self.names.len() as SymId;
        self.names.push(key.clone());
        self.ids.insert(key, id);
        self.by_symbol.push(Vec::new());
        id
    }

    /// The index file this was read from.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The index file's mtime when it was read.
    pub fn mtime(&self) -> SystemTime {
        self.mtime
    }

    /// Why the index is useless for this root, when it is.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// The root-relative path of every document, in the order the index lists them.
    pub fn doc_paths(&self) -> impl Iterator<Item = &str> {
        self.doc_paths.iter().map(String::as_str)
    }

    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    pub fn doc(&self, rel: &str) -> Option<&ScipDoc> {
        self.docs.get(rel)
    }

    /// The interned key; for a document-local symbol it carries the document's
    /// path, so callers never show it.
    pub fn symbol(&self, id: SymId) -> &str {
        &self.names[id as usize]
    }

    /// Whether `id` is a document-local symbol (`local N`; its key carries the document path and a NUL).
    pub fn is_local(&self, id: SymId) -> bool {
        self.names[id as usize].contains('\0')
    }

    /// The name a global symbol defines: the name of its last descriptor (`helper` for
    /// `rust-analyzer cargo demo 0.1.0 util/helper().`). None for a local symbol or one that does not parse.
    pub fn name(&self, id: SymId) -> Option<String> {
        if self.is_local(id) {
            return None;
        }
        scip::symbol::parse_symbol(self.symbol(id)).ok()?.descriptors.last().map(|d| d.name.clone()).filter(|n| !n.is_empty())
    }

    /// The id of a global symbol.
    pub fn symbol_id(&self, symbol: &str) -> Option<SymId> {
        self.ids.get(symbol).copied()
    }

    /// The narrowest single-line occurrence on `line` that holds `unit`; when
    /// the caret sits between two names, the one that starts there.
    pub fn occurrence_at(&self, rel: &str, line: u32, unit: u32) -> Option<&Occ> {
        let occs = &self.docs.get(rel)?.occurrences;
        let from = occs.partition_point(|o| o.span.line < line);
        occs[from..]
            .iter()
            .take_while(|o| o.span.line == line)
            .filter(|o| o.span.single_line() && o.span.start <= unit && unit <= o.span.end)
            .min_by_key(|o| (o.span.end - o.span.start, unit == o.span.end))
    }

    /// Whether some occurrence of `id` has the exact span of an occurrence of a different symbol (a shorthand
    /// `S { x }` is both the local and the field). Two occurrences of one symbol on one span do not count.
    pub fn shares_span(&self, id: SymId) -> bool {
        let Some(list) = self.by_symbol.get(id as usize) else { return false };
        list.iter().any(|&(d, i)| {
            let occs = &self.docs[self.doc_paths[d as usize].as_str()].occurrences;
            let span = occs[i as usize].span;
            let at = (span.line, span.start);
            let from = occs.partition_point(|o| (o.span.line, o.span.start) < at);
            occs[from..].iter().take_while(|o| (o.span.line, o.span.start) == at).any(|o| o.span == span && o.symbol != id)
        })
    }

    /// Every occurrence of a symbol, sorted by path, then position.
    pub fn occurrences_of(&self, id: SymId) -> Vec<(&str, &Occ)> {
        let Some(list) = self.by_symbol.get(id as usize) else { return Vec::new() };
        let mut out: Vec<(&str, u32, &Occ)> = list
            .iter()
            .map(|&(d, i)| {
                let rel = self.doc_paths[d as usize].as_str();
                (rel, i, &self.docs[rel].occurrences[i as usize])
            })
            .collect();
        out.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        out.into_iter().map(|(rel, _, o)| (rel, o)).collect()
    }

    /// Whether some document states a relationship the index links symbols by.
    pub fn has_relationships(&self) -> bool {
        !self.related.is_empty()
    }

    /// The symbols the index links to `id` by relationships, sorted.
    pub fn related(&self, id: SymId) -> &[SymId] {
        self.related.get(&id).map_or(&[], Vec::as_slice)
    }

    /// May stand for several definitions in the code: a member of a trait, interface, protocol or type class, or of
    /// rust-analyzer's `impl Trait for Type` block.
    pub fn polymorphic(&self, id: SymId) -> bool {
        use scip::types::descriptor::Suffix;
        if self.is_local(id) {
            return false;
        }
        if self.abstract_members.contains(&id) {
            return true;
        }
        let Ok(mut sym) = scip::symbol::parse_symbol(self.symbol(id)) else { return false };
        let is = |d: &scip::types::Descriptor, suffix: Suffix| d.suffix.enum_value() == Ok(suffix);
        // rust-analyzer names the members of `impl Trait for Type` `impl#[Type][Trait]name().`; an inherent block has one bracket.
        if sym
            .descriptors
            .windows(3)
            .any(|w| is(&w[0], Suffix::Type) && w[0].name == "impl" && is(&w[1], Suffix::TypeParameter) && is(&w[2], Suffix::TypeParameter))
        {
            return true;
        }
        if sym.descriptors.len() < 2 {
            return false;
        }
        sym.descriptors.pop();
        self.symbol_id(&scip::symbol::format_symbol(sym)).is_some_and(|parent| self.abstract_types.contains(&parent))
    }

    /// The symbols a rename of `id` must change together, `id` included, sorted: the closure of `id` over the
    /// relationships. None (the rename is left to the text path) when a linked symbol has no definition in the index
    /// (an implementation of a `std` trait), or when the index states no relationship at all and `id` is
    /// [`polymorphic`](Self::polymorphic): an indexer that says nothing cannot vouch that a trait member has no
    /// implementation. A missing definition of `id` itself is left to the rename build's fallback only when `id` links
    /// nothing: in a larger set a related definition would stand in for it, so `id` must be defined too.
    pub fn rename_set(&self, id: SymId) -> Option<Vec<SymId>> {
        if self.is_local(id) {
            return Some(vec![id]);
        }
        let mut set = vec![id];
        let mut seen: HashSet<SymId> = set.iter().copied().collect();
        let mut at = 0;
        while at < set.len() {
            for &next in self.related(set[at]) {
                if seen.insert(next) {
                    set.push(next);
                }
            }
            at += 1;
        }
        if !self.has_relationships() && self.polymorphic(id) {
            return None;
        }
        if set.iter().any(|&s| (s != id || set.len() > 1) && !self.has_definition(s)) {
            return None;
        }
        set.sort_unstable();
        Some(set)
    }

    fn has_definition(&self, id: SymId) -> bool {
        let Some(list) = self.by_symbol.get(id as usize) else { return false };
        list.iter().any(|&(d, i)| self.docs[self.doc_paths[d as usize].as_str()].occurrences[i as usize].definition)
    }
}

/// The loaded index, as the app holds it.
#[derive(Debug, Default)]
pub struct ScipState {
    pub index: Option<std::sync::Arc<ScipIndex>>,
    /// Bumped by every load; a stale result is dropped.
    pub seq: u64,
    pub loading: bool,
    /// The index file's mtime of the last failed load; the same mtime is not
    /// retried.
    pub failed_mtime: Option<SystemTime>,
}

/// Writes a SCIP file with the crate's own types: per document a path, an
/// encoding and `(legacy range, symbol, is definition)` occurrences.
#[cfg(test)]
pub(crate) type TestOccs<'a> = &'a [(&'a [i32], &'a str, bool)];

#[cfg(test)]
pub(crate) fn write_test_index(file: &Path, project_root: &str, docs: &[(&str, Encoding, TestOccs)]) {
    write_test_index_with(file, project_root, docs, &[]);
}

/// Symbol information to write: (document path, symbol, kind, relationships as (target, is implementation, is reference)).
#[cfg(test)]
pub(crate) type TestSyms<'a> = &'a [(&'a str, &'a str, Kind, &'a [(&'a str, bool, bool)])];

/// [`write_test_index`] with symbol information.
#[cfg(test)]
pub(crate) fn write_test_index_with(file: &Path, project_root: &str, docs: &[(&str, Encoding, TestOccs)], syms: TestSyms) {
    use scip::types::{Document, Occurrence, Relationship, SymbolInformation};
    let mut index = Index::new();
    index.metadata.mut_or_insert_default().project_root = project_root.to_string();
    for (path, enc, occs) in docs {
        let mut d = Document::new();
        d.relative_path = (*path).to_string();
        d.position_encoding = protobuf::EnumOrUnknown::new(match enc {
            Encoding::Utf8 => PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            Encoding::Utf16 => PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
            Encoding::Utf32 => PositionEncoding::UTF32CodeUnitOffsetFromLineStart,
        });
        for (range, symbol, def) in *occs {
            let mut o = Occurrence::new();
            o.range = range.to_vec();
            o.symbol = (*symbol).to_string();
            o.symbol_roles = if *def { SymbolRole::Definition as i32 } else { 0 };
            d.occurrences.push(o);
        }
        index.documents.push(d);
    }
    for (doc, symbol, kind, rels) in syms {
        let d =
            index.documents.iter_mut().find(|d| d.relative_path == *doc).expect("symbol information for a document that is not written");
        let mut info = SymbolInformation::new();
        info.symbol = (*symbol).to_string();
        info.kind = protobuf::EnumOrUnknown::new(*kind);
        for (target, is_implementation, is_reference) in *rels {
            let mut r = Relationship::new();
            r.symbol = (*target).to_string();
            r.is_implementation = *is_implementation;
            r.is_reference = *is_reference;
            info.relationships.push(r);
        }
        d.symbols.push(info);
    }
    std::fs::write(file, index.write_to_bytes().unwrap()).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::{MultiLineRange, Occurrence, SingleLineRange};

    #[test]
    fn columns_convert_per_encoding() {
        let line = "let s = \"日本😀\"; x";
        // The `x` is char 16: "日本" is 3 bytes each, "😀" 4 bytes and 2 UTF-16 units.
        for (enc, units) in [(Encoding::Utf8, 23), (Encoding::Utf16, 17), (Encoding::Utf32, 16)] {
            assert_eq!(to_units(line, 16, enc), units, "{enc:?}");
            assert_eq!(to_chars(line, units, enc), 16, "{enc:?}");
        }
        assert_eq!(to_chars(line, 500, Encoding::Utf8), line.chars().count());
    }

    #[test]
    fn spans_from_typed_and_legacy_ranges() {
        let s = |l, a, el, b| Some(Span { line: l, start: a, end_line: el, end: b });
        assert_eq!(span_of(None, &[1, 2, 5]), s(1, 2, 1, 5));
        assert_eq!(span_of(None, &[1, 2, 3, 4]), s(1, 2, 3, 4));
        let mut single = SingleLineRange::new();
        single.line = 7;
        single.start_character = 1;
        single.end_character = 2;
        let single = occurrence::Typed_range::SingleLineRange(single);
        assert_eq!(span_of(Some(&single), &[0, 0, 9]), s(7, 1, 7, 2));
        let mut multi = MultiLineRange::new();
        multi.start_line = 4;
        multi.start_character = 3;
        multi.end_line = 6;
        multi.end_character = 1;
        let multi = occurrence::Typed_range::MultiLineRange(multi);
        assert_eq!(span_of(Some(&multi), &[0, 0, 9]), s(4, 3, 6, 1));
        assert_eq!(span_of(None, &[1, 2]), None);
        assert_eq!(span_of(None, &[1, -2, 3]), None);
    }

    #[test]
    fn file_fresh_is_the_rule() {
        let dir = temp("fresh");
        let f = dir.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let now = SystemTime::now();
        let set = |t: SystemTime| std::fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
        let hour = std::time::Duration::from_secs(3600);
        set(now - hour);
        assert!(file_fresh(&f, false, now), "older than the index");
        set(now);
        assert!(file_fresh(&f, false, now), "equal mtime");
        set(now + hour);
        assert!(!file_fresh(&f, false, now), "newer than the index");
        set(now - hour);
        assert!(!file_fresh(&f, true, now), "dirty in a buffer");
        assert!(!file_fresh(&dir.join("missing.txt"), false, now), "missing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-scip-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_keeps_documents_under_the_root_with_document_locals() {
        let root = temp("load");
        let file = root.join("index.scip");
        let g: &[(&[i32], &str, bool)] = &[
            (&[0, 0, 5, 0], "t crate/", true),
            (&[1, 3, 7], "t main().", true),
            (&[2, 8, 9], "local 0", true),
            (&[2, 32, 38], "t util/helper().", false),
        ];
        let u: &[(&[i32], &str, bool)] = &[(&[0, 4, 10], "t util/helper().", true), (&[1, 0, 1], "local 0", true)];
        let out: &[(&[i32], &str, bool)] = &[(&[0, 0, 1], "t out/", true)];
        let root_url = format!("file://{}", root.display());
        write_test_index(
            &file,
            &root_url,
            &[("src/main.rs", Encoding::Utf8, g), ("src/util.rs", Encoding::Utf8, u), ("../out.rs", Encoding::Utf8, out)],
        );
        let ix = ScipIndex::load(&file, &root).unwrap();
        assert_eq!(ix.doc_count(), 2);
        let hit = ix.occurrence_at("src/main.rs", 2, 33).unwrap();
        assert_eq!(ix.symbol(hit.symbol), "t util/helper().");
        assert!(!hit.definition);
        let a = ix.occurrence_at("src/main.rs", 2, 8).unwrap().symbol;
        let b = ix.occurrence_at("src/util.rs", 1, 0).unwrap().symbol;
        assert_ne!(a, b);
        assert_eq!(ix.symbol_id("local 0"), None);
        let all = ix.occurrences_of(hit.symbol);
        let seen: Vec<(&str, bool)> = all.iter().map(|(r, o)| (*r, o.definition)).collect();
        assert_eq!(seen, [("src/main.rs", false), ("src/util.rs", true)]);

        // A project_root below the jetl root prefixes the paths.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        write_test_index(&file, &format!("{root_url}/sub"), &[("a.rs", Encoding::Utf16, u)]);
        let ix = ScipIndex::load(&file, &root).unwrap();
        assert!(ix.doc("sub/a.rs").is_some());
        assert_eq!(ix.doc("sub/a.rs").unwrap().encoding, Encoding::Utf16);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_index_without_a_document_under_the_root_warns() {
        let root = temp("outside");
        let file = root.join("index.scip");
        let g: &[(&[i32], &str, bool)] = &[(&[0, 0, 1], "t a/", true)];
        write_test_index(&file, "file:///elsewhere/project", &[("a.rs", Encoding::Utf8, g)]);
        // The base directory does not exist, so paths fall back to the root and stay inside.
        assert!(ScipIndex::load(&file, &root).unwrap().warning().is_none());
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let other = temp("outside-other");
        write_test_index(&file, &format!("file://{}", other.display()), &[("a.rs", Encoding::Utf8, g)]);
        let ix = ScipIndex::load(&file, &root).unwrap();
        assert_eq!(ix.doc_count(), 0);
        let w = ix.warning().unwrap();
        assert!(w.starts_with("index.scip: no document under "), "{w}");
        assert!(w.contains(&format!("project_root file://{}", other.display())), "{w}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn load_reports_a_missing_or_corrupt_file() {
        let root = temp("bad");
        assert!(ScipIndex::load(&root.join("nope.scip"), &root).is_err());
        std::fs::write(root.join("bad.scip"), b"not protobuf \xff").unwrap();
        let e = ScipIndex::load(&root.join("bad.scip"), &root).unwrap_err();
        assert!(e.starts_with("not a SCIP index"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_refuses_a_file_above_the_cap() {
        let root = temp("cap");
        let file = root.join("big.scip");
        std::fs::File::create(&file).unwrap().set_len(MAX_SCIP_BYTES + 1).unwrap();
        let e = ScipIndex::load(&file, &root).unwrap_err();
        assert_eq!(e, format!("{} MiB is above the limit of {} MiB", (MAX_SCIP_BYTES + 1) >> 20, MAX_SCIP_BYTES >> 20));
        std::fs::File::create(&file).unwrap().set_len(MAX_SCIP_BYTES).unwrap();
        let e = ScipIndex::load(&file, &root).unwrap_err();
        assert!(e.starts_with("not a SCIP index"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_are_the_last_descriptor() {
        let root = temp("names");
        let file = root.join("index.scip");
        let occs: &[(&[i32], &str, bool)] = &[
            (&[0, 0, 1], "rust-analyzer cargo demo 0.1.0 util/helper().", true),
            (&[1, 0, 1], "t . . . A#new().", true),
            (&[2, 0, 1], "local 0", true),
            (&[3, 0, 1], "t util/helper().", true),
        ];
        write_test_index(&file, "", &[("a.rs", Encoding::Utf8, occs)]);
        let ix = ScipIndex::load(&file, &root).unwrap();
        let ids: Vec<SymId> = ix.doc("a.rs").unwrap().occurrences.iter().map(|o| o.symbol).collect();
        assert_eq!(ids.iter().map(|&i| ix.name(i)).collect::<Vec<_>>(), [Some("helper".to_string()), Some("new".to_string()), None, None]);
        assert_eq!(ids.iter().map(|&i| ix.is_local(i)).collect::<Vec<_>>(), [false, false, true, false]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symbol_sharing_a_span_with_another_is_reported() {
        let root = temp("shares");
        let file = root.join("index.scip");
        let (field, local, other) = ("t S#x.", "local 0", "t f().");
        let occs: &[(&[i32], &str, bool)] = &[
            (&[0, 4, 5], field, true),
            (&[1, 8, 9], local, false),
            (&[1, 8, 9], field, false),
            (&[2, 0, 1], other, true),
            (&[2, 0, 1], other, false),
            (&[3, 0, 1], other, false),
        ];
        write_test_index(&file, "", &[("a.rs", Encoding::Utf8, occs)]);
        let ix = ScipIndex::load(&file, &root).unwrap();
        let sym = |line: u32, n: usize| ix.doc("a.rs").unwrap().occurrences.iter().filter(|o| o.span.line == line).nth(n).unwrap().symbol;
        let (field, local, other) = (sym(0, 0), sym(1, 0), sym(2, 0));
        assert!(ix.shares_span(field), "the field shares the shorthand span");
        assert!(ix.shares_span(local), "the local shares it too");
        assert!(!ix.shares_span(other), "one symbol twice on one span is not a share");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enclosing_ranges_are_kept() {
        let mut o = Occurrence::new();
        o.enclosing_range = vec![1, 0, 4, 1];
        assert_eq!(enclosing_of(None, &o.enclosing_range), Some(Span { line: 1, start: 0, end_line: 4, end: 1 }));
    }

    const P: &str = "rust-analyzer cargo demo 0.1.0 ";

    #[test]
    fn symbol_information_kinds_and_relationships_are_read() {
        let root = temp("syminfo");
        let file = root.join("index.scip");
        let (t, tf, a, b) = (format!("{P}m/T#"), format!("{P}m/T#f()."), format!("{P}m/impl#[A][T]f()."), format!("{P}m/impl#[B][T]f()."));
        let other = format!("{P}m/Other#");
        let occs: TestOccs = &[(&[0, 0, 1], &t, true)];
        let syms: TestSyms = &[
            ("a.rs", &t, Kind::Trait, &[]),
            ("a.rs", &tf, Kind::TraitMethod, &[]),
            ("a.rs", &a, Kind::Method, &[(&tf, true, false), ("local 3", true, false), (&other, false, false)]),
            ("a.rs", &b, Kind::Method, &[(&tf, false, true)]),
        ];
        write_test_index_with(&file, "", &[("a.rs", Encoding::Utf8, occs)], syms);
        let ix = ScipIndex::load(&file, &root).unwrap();
        assert!(ix.has_relationships());
        let id = |s: &str| ix.symbol_id(s).unwrap_or_else(|| panic!("{s} not interned"));
        let mut both = vec![id(&a), id(&b)];
        both.sort_unstable();
        assert_eq!(ix.related(id(&tf)), both);
        assert_eq!(ix.related(id(&a)), [id(&tf)]);
        assert_eq!(ix.related(id(&b)), [id(&tf)]);
        assert_eq!(ix.symbol_id(&other), None, "a relationship that is only a type definition links nothing");
        assert_eq!(ix.symbol_id("local 3"), None, "a local target links nothing");
        assert!(ix.polymorphic(id(&tf)));
        assert!(ix.related(id(&t)).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trait_members_are_polymorphic() {
        let root = temp("poly");
        let file = root.join("index.scip");
        let area_t = format!("{P}shapes/Area#");
        let area = format!("{P}shapes/Area#area().");
        let twice = format!("{P}shapes/Area#twice().");
        let c_area = format!("{P}shapes/impl#[Circle][Area]area().");
        let grow = format!("{P}shapes/impl#[Circle]grow().");
        let circle = format!("{P}shapes/Circle#");
        let helper = "t util/helper().";
        let all = [&area_t, &area, &twice, &c_area, &grow, &circle];
        let mut occs: Vec<(&[i32], &str, bool)> = Vec::new();
        let ranges: Vec<[i32; 3]> = (0..7).map(|i| [i, 0, 1]).collect();
        for (i, s) in all.iter().enumerate() {
            occs.push((&ranges[i], s.as_str(), true));
        }
        occs.push((&ranges[6], helper, true));
        let syms: TestSyms = &[
            ("a.rs", &area_t, Kind::Trait, &[]),
            ("a.rs", &area, Kind::TraitMethod, &[]),
            ("a.rs", &twice, Kind::Method, &[]),
            ("a.rs", &c_area, Kind::Method, &[]),
            ("a.rs", &grow, Kind::Method, &[]),
            ("a.rs", &circle, Kind::Struct, &[]),
        ];
        write_test_index_with(&file, "", &[("a.rs", Encoding::Utf8, &occs)], syms);
        let ix = ScipIndex::load(&file, &root).unwrap();
        let id = |s: &str| ix.symbol_id(s).unwrap();
        assert!(!ix.has_relationships());
        let mut wrong = Vec::new();
        for (s, poly) in [
            (&area, true),
            (&twice, true),
            (&c_area, true),
            (&grow, false),
            (&area_t, false),
            (&circle, false),
            (&helper.to_string(), false),
        ] {
            let want = if poly { None } else { Some(vec![id(s)]) };
            if ix.polymorphic(id(s)) != poly || ix.rename_set(id(s)) != want {
                wrong.push(format!("{s}: polymorphic {}, set {:?}, want polymorphic {poly}", ix.polymorphic(id(s)), ix.rename_set(id(s))));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_set_follows_relationships_transitively() {
        let root = temp("set");
        let file = root.join("index.scip");
        let (t, tf, a, b) = (format!("{P}m/T#"), format!("{P}m/T#f()."), format!("{P}m/impl#[A][T]f()."), format!("{P}m/impl#[B][T]f()."));
        let (display, user) = ("rust-analyzer cargo std 1.0.0 fmt/Display#fmt().".to_string(), format!("{P}m/impl#[A][Display]fmt()."));
        let occs: TestOccs =
            &[(&[0, 0, 1], &t, true), (&[1, 0, 1], &tf, true), (&[2, 0, 1], &a, true), (&[3, 0, 1], &b, true), (&[4, 0, 1], &user, true)];
        let syms: TestSyms = &[
            ("a.rs", &t, Kind::Trait, &[]),
            ("a.rs", &tf, Kind::TraitMethod, &[]),
            ("a.rs", &a, Kind::Method, &[(&tf, true, false)]),
            ("a.rs", &b, Kind::Method, &[(&tf, false, true)]),
            ("a.rs", &user, Kind::Method, &[(&display, true, false)]),
        ];
        write_test_index_with(&file, "", &[("a.rs", Encoding::Utf8, occs)], syms);
        let ix = ScipIndex::load(&file, &root).unwrap();
        let id = |s: &str| ix.symbol_id(s).unwrap();
        let mut three = vec![id(&tf), id(&a), id(&b)];
        three.sort_unstable();
        assert_eq!(ix.rename_set(id(&a)), Some(three.clone()), "from one implementation");
        assert_eq!(ix.rename_set(id(&tf)), Some(three), "from the trait method");
        assert_eq!(ix.rename_set(id(&t)), Some(vec![id(&t)]), "the trait type links nothing");
        assert_eq!(ix.rename_set(id(&user)), None, "the std trait method has no definition in the index");
        assert_eq!(ix.rename_set(id(&display)), None, "from a call of the std trait method, which the index links but does not define");
        let _ = std::fs::remove_dir_all(&root);
    }
}
