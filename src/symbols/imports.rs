//! Import declarations found by a language's imports query, and the
//! resolver from one import to a project file (file import edges of the
//! symbol index).

use std::collections::HashSet;

use ropey::Rope;
use tree_sitter::{Node, QueryCursor, StreamingIterator, Tree};

use crate::editor::syntax::{LanguageDef, RopeProvider};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// Rust: a flat path (`crate::a::B`) or, for `mod name;`, the name. Python: the dotted
    /// name with its leading dots, a from-import's name joined (`pkg.a`, `.b`).
    pub spec: String,
    /// 0-based line of the import.
    pub line: usize,
    /// A declaration that defines the module's file (Rust `mod name;`), not a use of it.
    pub decl: bool,
}

/// The imports in `tree` (parsed from `rope`) found by the language's
/// imports query, in document order without exact duplicates. Empty without
/// an imports query.
pub fn extract(def: &'static LanguageDef, tree: &Tree, rope: &Rope) -> Vec<Import> {
    let Some(query) = def.imports_query() else { return Vec::new() };
    let path_idx = query.capture_index_for_name("import.path");
    let name_idx = query.capture_index_for_name("import.name");
    let mod_idx = query.capture_index_for_name("import.mod");
    let text = |n: Node| {
        let r = n.byte_range();
        let end = r.end.min(rope.len_bytes());
        rope.byte_slice(r.start.min(end)..end).to_string()
    };
    let line = |n: Node| rope.byte_to_line(n.start_byte().min(rope.len_bytes()));
    // (sort key: start byte, import)
    let mut raw: Vec<(usize, Import)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
    while let Some(m) = matches.next() {
        let (mut path, mut module, mut names): (Option<Node>, Option<Node>, Vec<Node>) = (None, None, Vec::new());
        for c in m.captures() {
            if Some(c.index) == path_idx {
                path = Some(c.node);
            } else if Some(c.index) == mod_idx {
                module = Some(c.node);
            } else if Some(c.index) == name_idx {
                names.push(c.node);
            }
        }
        if let Some(n) = module {
            let spec = text(n).trim().to_string();
            if !spec.is_empty() {
                raw.push((n.start_byte(), Import { spec, line: line(n), decl: true }));
            }
        }
        let Some(n) = path else { continue };
        let p = unquote(&text(n)).to_string();
        let l = line(n);
        let mut push = |key: usize, spec: String| {
            if !spec.is_empty() {
                raw.push((key, Import { spec, line: l, decl: false }));
            }
        };
        if def.name == "rust" {
            for spec in expand_rust_use(&p) {
                push(n.start_byte(), spec);
            }
        } else if names.is_empty() {
            push(n.start_byte(), compact(&p));
        } else {
            for name in names {
                push(name.start_byte(), join_name(&compact(&p), &compact(&text(name))));
            }
        }
    }
    raw.sort_by_key(|r| r.0);
    let mut seen: HashSet<(String, usize, bool)> = HashSet::new();
    let mut out: Vec<Import> = raw.into_iter().map(|r| r.1).collect();
    out.retain(|i| seen.insert((i.spec.clone(), i.line, i.decl)));
    out
}

/// `s` trimmed, without one matching pair of quotes around it.
fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\'', '`'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// `s` without any whitespace.
fn compact(s: &str) -> String {
    s.split_whitespace().collect()
}

/// `name` joined to `path` with a `.` (none when `path` already ends with one).
fn join_name(path: &str, name: &str) -> String {
    if path.ends_with('.') { format!("{path}{name}") } else { format!("{path}.{name}") }
}

/// The flat paths of a Rust use tree: `a::{b, c::{self, D as E}}` gives `a::b`, `a::c`, `a::c::D`.
fn expand_rust_use(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    expand_into("", text, &mut out);
    out
}

/// Flatten `s` under `prefix` into `out`.
fn expand_into(prefix: &str, s: &str, out: &mut Vec<String>) {
    let s = s.trim();
    if let Some(open) = s.find('{') {
        let close = s.rfind('}').unwrap_or(s.len());
        let head = s[..open].trim().trim_end_matches("::").trim();
        let p = join_rust(prefix, head);
        for item in split_top(&s[open + 1..close.max(open + 1)]) {
            expand_into(&p, item, out);
        }
        return;
    }
    let leaf: String = s.split_whitespace().take_while(|t| *t != "as").collect();
    let leaf = leaf.trim_end_matches('*').trim_end_matches("::");
    let full = if leaf == "self" || leaf.is_empty() { prefix.to_string() } else { join_rust(prefix, leaf) };
    let full = full.trim_start_matches("::").to_string();
    if !full.is_empty() && !out.contains(&full) {
        out.push(full);
    }
}

/// `s` split on commas outside braces, empty items skipped.
fn split_top(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0usize, 0);
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out.retain(|i| !i.trim().is_empty());
    out
}

/// `a::b`, or the non-empty one of the two.
fn join_rust(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}::{b}")
    }
}

/// The indexed file `imp` (found in `from`) refers to, if any. Pure: paths are
/// relative to the project root with `/`, the root itself is `""`, and
/// `exists` says whether a path is an indexed file. Never returns `from`.
pub fn resolve(lang: &str, from: &str, imp: &Import, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    match lang {
        "rust" => resolve_rust(from, imp, exists),
        "python" => resolve_python(from, imp, exists),
        _ => None,
    }
    .filter(|t| t != from)
}

/// The directory part of `p`: `Some("")` for a top-level name, `None` for `""`.
fn parent(p: &str) -> Option<&str> {
    if p.is_empty() { None } else { Some(p.rsplit_once('/').map_or("", |(h, _)| h)) }
}

/// `base/rest`, or `rest` when `base` is the root.
fn join(base: &str, rest: &str) -> String {
    if base.is_empty() { rest.to_string() } else { format!("{base}/{rest}") }
}

/// The first of `cands` that exists.
fn first(cands: impl IntoIterator<Item = String>, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    cands.into_iter().find(|c| exists(c))
}

/// `from` and then each ancestor directory up to the root (`""`), nearest first.
fn ancestors(dir: &str) -> Vec<&str> {
    let mut out = vec![dir];
    let mut d = dir;
    while let Some(p) = parent(d) {
        out.push(p);
        d = p;
    }
    out
}

fn resolve_rust(from: &str, imp: &Import, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let dir = parent(from).unwrap_or("");
    let name = from.rsplit('/').next().unwrap_or(from);
    let module_dir =
        if matches!(name, "mod.rs" | "lib.rs" | "main.rs") { dir.to_string() } else { join(dir, name.trim_end_matches(".rs")) };
    if imp.decl {
        return first([join(&module_dir, &format!("{}.rs", imp.spec)), join(&module_dir, &format!("{}/mod.rs", imp.spec))], exists);
    }
    let crate_root = || ancestors(dir).into_iter().find(|d| exists(&join(d, "lib.rs")) || exists(&join(d, "main.rs"))).map(str::to_string);
    let descend = |base: &str, segs: &[&str]| {
        (1..=segs.len()).rev().find_map(|k| {
            let p = join(base, &segs[..k].join("/"));
            first([format!("{p}.rs"), format!("{p}/mod.rs")], exists)
        })
    };
    let module_file = |base: &str| {
        let by_name = if base.is_empty() { None } else { Some(format!("{base}.rs")) };
        first([join(base, "mod.rs")].into_iter().chain(by_name).chain([join(base, "lib.rs"), join(base, "main.rs")]), exists)
    };
    let segs: Vec<&str> = imp.spec.split("::").filter(|s| !s.is_empty()).collect();
    let (&head, rest) = segs.split_first()?;
    match head {
        "crate" => {
            let base = crate_root()?;
            descend(&base, rest).or_else(|| module_file(&base))
        }
        "self" => descend(&module_dir, rest).or_else(|| module_file(&module_dir)),
        "super" => {
            let mut base = module_dir.as_str();
            let mut rest = &segs[..];
            while let Some((&"super", tail)) = rest.split_first() {
                base = parent(base)?;
                rest = tail;
            }
            descend(base, rest).or_else(|| module_file(base))
        }
        _ => descend(&module_dir, &segs).or_else(|| descend(&crate_root()?, &segs)),
    }
}

fn resolve_python(from: &str, imp: &Import, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let dots = imp.spec.chars().take_while(|c| *c == '.').count();
    let segs: Vec<&str> = imp.spec[dots..].split('.').filter(|s| !s.is_empty()).collect();
    let descend = |base: &str| {
        (1..=segs.len()).rev().find_map(|k| {
            let p = join(base, &segs[..k].join("/"));
            first([format!("{p}.py"), format!("{p}.pyi"), format!("{p}/__init__.py")], exists)
        })
    };
    if dots > 0 {
        let mut base = parent(from)?;
        for _ in 1..dots {
            base = parent(base)?;
        }
        return descend(base).or_else(|| first([join(base, "__init__.py")], exists));
    }
    ancestors(parent(from).unwrap_or("")).into_iter().find_map(descend)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(files: &[&str]) -> impl Fn(&str) -> bool {
        let s: HashSet<String> = files.iter().map(|f| f.to_string()).collect();
        move |p: &str| s.contains(p)
    }

    fn r(lang: &str, from: &str, spec: &str, decl: bool, files: &[&str]) -> Option<String> {
        resolve(lang, from, &Import { spec: spec.into(), line: 0, decl }, &set(files))
    }

    #[test]
    fn rust_use_trees_flatten() {
        assert_eq!(
            expand_rust_use("crate::{config::Config, app::{self, view::View as V}, util::*}"),
            ["crate::config::Config", "crate::app", "crate::app::view::View", "crate::util"]
        );
        assert_eq!(expand_rust_use("std :: fs"), ["std::fs"]);
        assert_eq!(expand_rust_use("::std::io::{self, Write,}"), ["std::io", "std::io::Write"]);
        assert_eq!(expand_rust_use("super::*"), ["super"]);
    }

    #[test]
    fn rust_resolves_modules() {
        let f = ["src/main.rs", "src/app/mod.rs", "src/app/view.rs", "src/config.rs", "src/util/mod.rs", "src/util/text.rs"];
        assert_eq!(r("rust", "src/main.rs", "app", true, &f).as_deref(), Some("src/app/mod.rs"));
        assert_eq!(r("rust", "src/app/mod.rs", "view", true, &f).as_deref(), Some("src/app/view.rs"));
        assert_eq!(r("rust", "src/app/view.rs", "inner", true, &f), None);
        assert_eq!(r("rust", "src/app/view.rs", "crate::config::Config", false, &f).as_deref(), Some("src/config.rs"));
        assert_eq!(r("rust", "src/app/view.rs", "super", false, &f).as_deref(), Some("src/app/mod.rs"));
        assert_eq!(r("rust", "src/app/mod.rs", "super::util::text::f", false, &f).as_deref(), Some("src/util/text.rs"));
        assert_eq!(r("rust", "src/app/mod.rs", "self::view::View", false, &f).as_deref(), Some("src/app/view.rs"));
        assert_eq!(r("rust", "src/config.rs", "crate::App", false, &f).as_deref(), Some("src/main.rs"));
        assert_eq!(r("rust", "src/main.rs", "crate::App", false, &f), None);
        assert_eq!(r("rust", "src/app/mod.rs", "util::text", false, &f).as_deref(), Some("src/util/text.rs"));
        assert_eq!(r("rust", "src/main.rs", "std::fs", false, &f), None);
        assert_eq!(r("rust", "src/main.rs", "super::x", false, &f), None);
    }

    #[test]
    fn python_resolves_modules() {
        let f = ["main.py", "pkg/__init__.py", "pkg/a.py", "pkg/b.py", "pkg/sub/__init__.py", "pkg/sub/x.py", "src/lib2/m.py"];
        assert_eq!(r("python", "main.py", "pkg.a", false, &f).as_deref(), Some("pkg/a.py"));
        assert_eq!(r("python", "main.py", "pkg.sub.x", false, &f).as_deref(), Some("pkg/sub/x.py"));
        assert_eq!(r("python", "main.py", "pkg.sub.y", false, &f).as_deref(), Some("pkg/sub/__init__.py"));
        assert_eq!(r("python", "pkg/a.py", ".b", false, &f).as_deref(), Some("pkg/b.py"));
        assert_eq!(r("python", "pkg/a.py", ".helper", false, &f).as_deref(), Some("pkg/__init__.py"));
        assert_eq!(r("python", "pkg/sub/x.py", "..a.f", false, &f).as_deref(), Some("pkg/a.py"));
        assert_eq!(r("python", "pkg/sub/x.py", "....x", false, &f), None);
        assert_eq!(r("python", "src/tool.py", "lib2.m", false, &f).as_deref(), Some("src/lib2/m.py"));
        assert_eq!(r("python", "main.py", "os", false, &f), None);
    }

    #[test]
    fn other_languages_resolve_nothing() {
        assert_eq!(r("go", "a.go", "fmt", false, &["a.go", "fmt.go"]), None);
        let imp = Import { spec: "fmt".into(), line: 0, decl: false };
        assert_eq!(resolve("go", "a.go", &imp, &|_| true), None);
    }

    #[cfg(any(feature = "lang-rust", feature = "lang-python"))]
    fn imports(lang: &str, src: &str) -> Vec<(String, usize, bool)> {
        let def = crate::editor::syntax::language_for_name(lang).unwrap();
        let got = crate::symbols::extract_index(def, &mut tree_sitter::Parser::new(), src).2;
        got.into_iter().map(|i| (i.spec, i.line, i.decl)).collect()
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_imports_extracted() {
        let src = "mod app;\nmod inline {}\npub(crate) mod util;\nuse std::fs;\nuse crate::{config::Config, app::{self, view::View as V}, util::*};\nuse super::x;\nfn f() { use self::inner::Y; }\n";
        let want: Vec<(String, usize, bool)> = [
            ("app", 0, true),
            ("util", 2, true),
            ("std::fs", 3, false),
            ("crate::config::Config", 4, false),
            ("crate::app", 4, false),
            ("crate::app::view::View", 4, false),
            ("crate::util", 4, false),
            ("super::x", 5, false),
            ("self::inner::Y", 6, false),
        ]
        .into_iter()
        .map(|(s, l, d)| (s.to_string(), l, d))
        .collect();
        assert_eq!(imports("rust", src), want);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_imports_extracted() {
        let src =
            "import os, pkg.a as pa\nfrom . import b, c as cc\nfrom ..up import thing\nfrom .sib import *\nfrom pkg.sub import (x, y)\n";
        let want: Vec<(String, usize, bool)> =
            [("os", 0), ("pkg.a", 0), (".b", 1), (".c", 1), ("..up.thing", 2), (".sib", 3), ("pkg.sub.x", 4), ("pkg.sub.y", 4)]
                .into_iter()
                .map(|(s, l)| (s.to_string(), l, false))
                .collect();
        assert_eq!(imports("python", src), want);
    }
}
