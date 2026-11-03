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

/// A view over the indexed paths (relative to the root, `/`-separated).
pub struct Project<'a> {
    files: &'a [&'a str],
}

impl<'a> Project<'a> {
    /// `sorted` must be sorted ascending.
    pub fn new(sorted: &'a [&'a str]) -> Self {
        Self { files: sorted }
    }

    pub fn exists(&self, p: &str) -> bool {
        self.files.binary_search(&p).is_ok()
    }

    /// The files directly in `dir` (`""` is the root), sorted.
    pub fn files_in(&self, dir: &str) -> Vec<&'a str> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let start = self.files.partition_point(|f| *f < prefix.as_str());
        self.files[start..].iter().take_while(|f| f.starts_with(&prefix)).filter(|f| !f[prefix.len()..].contains('/')).copied().collect()
    }
}

/// The indexed files `imp` (found in `from`) refers to, sorted and without
/// duplicates; a Go package or a Java `a.b.*` gives every file of its
/// directory, other imports at most one. Pure: paths are relative to the
/// project root with `/`, the root itself is `""`. Never returns `from`.
pub fn resolve(lang: &str, from: &str, imp: &Import, project: &Project) -> Vec<String> {
    let exists = |p: &str| project.exists(p);
    let mut out: Vec<String> = match lang {
        "rust" => resolve_rust(from, imp, &exists).into_iter().collect(),
        "python" => resolve_python(from, imp, &exists).into_iter().collect(),
        "javascript" | "typescript" | "tsx" => resolve_js(from, imp, &exists).into_iter().collect(),
        "go" => resolve_go(imp, project),
        "java" => resolve_java(from, imp, project),
        "c" | "cpp" => resolve_c(from, imp, &exists).into_iter().collect(),
        _ => Vec::new(),
    };
    out.retain(|t| t != from);
    out.sort();
    out.dedup();
    out
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

/// `p` without `.` and empty segments and with `..` applied; `None` when a `..` leaves the root.
fn normalize(p: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

const JS_EXTS: [&str; 9] = [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

fn resolve_js(from: &str, imp: &Import, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let spec = imp.spec.as_str();
    if !(spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let base = normalize(&join(parent(from).unwrap_or(""), spec))?;
    let mut cands: Vec<String> = Vec::new();
    if !base.is_empty() {
        cands.push(base.clone());
        // TypeScript writes the emitted extension: `./b.js` names `b.ts`.
        for (from_ext, to) in [(".js", [".ts", ".tsx"].as_slice()), (".jsx", &[".ts", ".tsx"]), (".mjs", &[".mts"]), (".cjs", &[".cts"])] {
            if let Some(stem) = base.strip_suffix(from_ext) {
                cands.extend(to.iter().map(|e| format!("{stem}{e}")));
            }
        }
        cands.extend(JS_EXTS.iter().map(|e| format!("{base}{e}")));
    }
    cands.extend(JS_EXTS.iter().map(|e| join(&base, &format!("index{e}"))));
    first(cands, exists)
}

fn resolve_go(imp: &Import, project: &Project) -> Vec<String> {
    let Some(rest) = imp.spec.strip_prefix('/') else { return Vec::new() };
    let dir = rest.trim_end_matches('/');
    project.files_in(dir).into_iter().filter(|f| f.ends_with(".go") && !f.ends_with("_test.go")).map(str::to_string).collect()
}

fn resolve_java(from: &str, imp: &Import, project: &Project) -> Vec<String> {
    let wild = imp.spec.ends_with(".*");
    let name = imp.spec.strip_suffix(".*").unwrap_or(&imp.spec);
    let segs: Vec<&str> = name.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return Vec::new();
    }
    let mut seen: Vec<String> = Vec::new();
    for a in ancestors(parent(from).unwrap_or("")) {
        for base in [a.to_string(), join(a, "src/main/java"), join(a, "src/test/java")] {
            if seen.contains(&base) {
                continue;
            }
            seen.push(base.clone());
            if wild {
                let files: Vec<String> = project
                    .files_in(&join(&base, &segs.join("/")))
                    .into_iter()
                    .filter(|f| f.ends_with(".java"))
                    .map(str::to_string)
                    .collect();
                if !files.is_empty() {
                    return files;
                }
            } else if let Some(f) =
                (1..=segs.len()).rev().map(|k| format!("{}.java", join(&base, &segs[..k].join("/")))).find(|f| project.exists(f))
            {
                return vec![f];
            }
        }
    }
    Vec::new()
}

fn resolve_c(from: &str, imp: &Import, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    if imp.spec.is_empty() || imp.spec.starts_with('/') {
        return None;
    }
    let dir = parent(from).unwrap_or("");
    first([normalize(&join(dir, &imp.spec)), normalize(&imp.spec)].into_iter().flatten(), exists)
}

/// The module path a `go.mod` declares.
pub fn go_module_path(go_mod: &str) -> Option<String> {
    for line in go_mod.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("module") else { continue };
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '"' || c == '`') {
            continue;
        }
        let rest = rest.split("//").next().unwrap_or("").trim();
        let m = unquote(rest);
        return if m.is_empty() { None } else { Some(m.to_string()) };
    }
    None
}

/// Rewrite the Go imports of `from` that lie inside its own module (the
/// nearest directory up from `from` whose `module_at` gives a module path)
/// to `/` + the package directory relative to the root. Other imports stay.
pub fn localize_go(from: &str, imports: &mut [Import], module_at: &mut dyn FnMut(&str) -> Option<String>) {
    let Some((d, m)) = ancestors(parent(from).unwrap_or("")).into_iter().find_map(|d| module_at(d).map(|m| (d, m))) else {
        return;
    };
    for imp in imports {
        let rest = if imp.spec == m { Some("") } else { imp.spec.strip_prefix(&format!("{m}/")) };
        if let Some(rest) = rest {
            imp.spec = format!("/{}", join(d, rest).trim_end_matches('/'));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proj(files: &[&'static str]) -> Vec<&'static str> {
        let mut v = files.to_vec();
        v.sort_unstable();
        v
    }

    fn rs(lang: &str, from: &str, spec: &str, decl: bool, files: &[&'static str]) -> Vec<String> {
        let sorted = proj(files);
        resolve(lang, from, &Import { spec: spec.into(), line: 0, decl }, &Project::new(&sorted))
    }

    fn r(lang: &str, from: &str, spec: &str, decl: bool, files: &[&'static str]) -> Option<String> {
        let v = rs(lang, from, spec, decl, files);
        assert!(v.len() <= 1, "{v:?}");
        v.into_iter().next()
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
        assert_eq!(r("lua", "a.lua", "x", false, &["a.lua", "x.lua"]), None);
        assert!(rs("go", "a.go", "fmt", false, &["a.go", "fmt.go"]).is_empty());
    }

    #[test]
    fn js_resolves_relative_specs() {
        let f = ["src/main.ts", "src/a.ts", "src/lib/b.ts", "src/c/index.tsx", "src/d.js", "src/e.d.ts", "index.js"];
        let at = |lang: &str, spec: &str| r(lang, "src/main.ts", spec, false, &f);
        assert_eq!(at("typescript", "./a").as_deref(), Some("src/a.ts"));
        assert_eq!(at("typescript", "./lib/b.js").as_deref(), Some("src/lib/b.ts"));
        assert_eq!(at("typescript", "./c").as_deref(), Some("src/c/index.tsx"));
        assert_eq!(at("typescript", "./d").as_deref(), Some("src/d.js"));
        assert_eq!(at("tsx", "./d.js").as_deref(), Some("src/d.js"));
        assert_eq!(at("javascript", "./e").as_deref(), Some("src/e.d.ts"));
        assert_eq!(at("typescript", "..").as_deref(), Some("index.js"));
        for spec in ["react", "../x", "../../x"] {
            assert_eq!(at("typescript", spec), None, "{spec}");
        }
    }

    #[test]
    fn go_resolves_packages() {
        let f = ["main.go", "internal/foo/foo.go", "internal/foo/bar.go", "internal/foo/foo_test.go", "internal/foo/sub/s.go"];
        assert_eq!(rs("go", "main.go", "/internal/foo", false, &f), ["internal/foo/bar.go", "internal/foo/foo.go"]);
        assert_eq!(rs("go", "internal/foo/foo.go", "/", false, &f), ["main.go"]);
        assert!(rs("go", "main.go", "/nope", false, &f).is_empty());
        assert!(rs("go", "main.go", "fmt", false, &f).is_empty());
        assert_eq!(rs("go", "internal/foo/foo.go", "/internal/foo", false, &f), ["internal/foo/bar.go"]);
    }

    #[test]
    fn go_module_paths_localize() {
        assert_eq!(go_module_path("// c\nmodule example.com/app // x\n\ngo 1.22\n").as_deref(), Some("example.com/app"));
        assert_eq!(go_module_path("module \"q/x\"\n").as_deref(), Some("q/x"));
        assert_eq!(go_module_path("go 1.22\n"), None);
        let imp = |s: &str| Import { spec: s.into(), line: 0, decl: false };
        let specs = |v: &[Import]| v.iter().map(|i| i.spec.clone()).collect::<Vec<_>>();
        let mut v = vec![imp("example.com/app/internal/foo"), imp("example.com/app"), imp("example.com/apple/x"), imp("fmt")];
        localize_go("cmd/app/main.go", &mut v, &mut |d| (d.is_empty()).then(|| "example.com/app".to_string()));
        assert_eq!(specs(&v), ["/internal/foo", "/", "example.com/apple/x", "fmt"]);
        let mut v = vec![imp("example.com/tools/lib"), imp("example.com/app/x")];
        localize_go("tools/gen/g.go", &mut v, &mut |d| match d {
            "" => Some("example.com/app".to_string()),
            "tools" => Some("example.com/tools".to_string()),
            _ => None,
        });
        assert_eq!(specs(&v), ["/tools/lib", "example.com/app/x"]);
    }

    #[test]
    fn java_resolves_classes() {
        let f = [
            "src/main/java/com/acme/App.java",
            "src/main/java/com/acme/Util.java",
            "src/main/java/com/acme/model/A.java",
            "src/main/java/com/acme/model/B.java",
            "src/test/java/com/acme/UtilTest.java",
        ];
        let app = "src/main/java/com/acme/App.java";
        let util = "src/main/java/com/acme/Util.java";
        assert_eq!(rs("java", app, "com.acme.Util", false, &f), [util]);
        assert_eq!(rs("java", app, "com.acme.Util.max", false, &f), [util]);
        assert_eq!(
            rs("java", app, "com.acme.model.*", false, &f),
            ["src/main/java/com/acme/model/A.java", "src/main/java/com/acme/model/B.java"]
        );
        assert!(rs("java", app, "com.acme.model", false, &f).is_empty());
        assert!(rs("java", app, "java.util.List", false, &f).is_empty());
        assert_eq!(rs("java", "src/test/java/com/acme/UtilTest.java", "com.acme.Util", false, &f), [util]);
    }

    #[test]
    fn c_resolves_includes() {
        let f = ["src/x.c", "src/util.h", "inc/a.h"];
        assert_eq!(r("c", "src/x.c", "util.h", false, &f).as_deref(), Some("src/util.h"));
        assert_eq!(r("c", "src/x.c", "../inc/a.h", false, &f).as_deref(), Some("inc/a.h"));
        assert_eq!(r("c", "src/x.c", "inc/a.h", false, &f).as_deref(), Some("inc/a.h"));
        for spec in ["stdio.h", "../../a.h", "/usr/a.h"] {
            assert_eq!(r("c", "src/x.c", spec, false, &f), None, "{spec}");
        }
        assert_eq!(r("cpp", "src/x.c", "util.h", false, &f).as_deref(), Some("src/util.h"));
    }

    #[cfg(any(
        feature = "lang-rust",
        feature = "lang-python",
        feature = "lang-js",
        feature = "lang-ts",
        feature = "lang-go",
        feature = "lang-c",
        feature = "lang-cpp",
        feature = "lang-java"
    ))]
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

    #[cfg(feature = "lang-js")]
    #[test]
    fn js_imports_extracted() {
        let src = "import a from \"./a\";\nimport { b } from '../lib/b.js';\nexport * from \"./c\";\nconst d = require(\"./d\");\nconst e = import(\"./e\");\nimport \"side\";\nrequire(x);\nfoo(\"./z\");\n";
        let want: Vec<(String, usize, bool)> = [("./a", 0), ("../lib/b.js", 1), ("./c", 2), ("./d", 3), ("./e", 4), ("side", 5)]
            .map(|(s, l)| (s.to_string(), l, false))
            .to_vec();
        assert_eq!(imports("javascript", src), want);
    }

    #[cfg(feature = "lang-ts")]
    #[test]
    fn ts_imports_extracted() {
        let src = "import type { T } from \"./t\";\nimport fs = require(\"fs\");\nexport { u } from \"./u\";\n";
        let want: Vec<(String, usize, bool)> = [("./t", 0), ("fs", 1), ("./u", 2)].map(|(s, l)| (s.to_string(), l, false)).to_vec();
        assert_eq!(imports("typescript", src), want);
        assert_eq!(imports("tsx", "import React from \"react\";\n"), [("react".to_string(), 0, false)]);
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn go_imports_extracted() {
        let src =
            "package main\n\nimport (\n\t\"fmt\"\n\tfoo \"example.com/app/internal/foo\"\n\t_ `example.com/app/x`\n)\nimport \"os\"\n";
        let want: Vec<(String, usize, bool)> = [("fmt", 3), ("example.com/app/internal/foo", 4), ("example.com/app/x", 5), ("os", 7)]
            .map(|(s, l)| (s.to_string(), l, false))
            .to_vec();
        assert_eq!(imports("go", src), want);
    }

    #[cfg(feature = "lang-java")]
    #[test]
    fn java_imports_extracted() {
        let src = "package a;\nimport java.util.List;\nimport static com.acme.Util.max;\nimport com.acme.model.*;\n";
        let want: Vec<(String, usize, bool)> =
            [("java.util.List", 1), ("com.acme.Util.max", 2), ("com.acme.model", 3), ("com.acme.model.*", 3)]
                .map(|(s, l)| (s.to_string(), l, false))
                .to_vec();
        assert_eq!(imports("java", src), want);
    }

    #[cfg(any(feature = "lang-c", feature = "lang-cpp"))]
    #[test]
    fn c_imports_extracted() {
        let src = "#include <stdio.h>\n#include \"util.h\"\n#include \"../inc/a.h\"\n";
        let want: Vec<(String, usize, bool)> = [("util.h", 1), ("../inc/a.h", 2)].map(|(s, l)| (s.to_string(), l, false)).to_vec();
        #[cfg(feature = "lang-c")]
        assert_eq!(imports("c", src), want);
        #[cfg(feature = "lang-cpp")]
        assert_eq!(imports("cpp", src), want);
    }
}
