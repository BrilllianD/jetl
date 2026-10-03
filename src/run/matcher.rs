//! Problem matchers: file locations in task output, as links the Run
//! window opens through `Effect::OpenFileAt`. Formats, tried in order:
//!
//! - rustc/cargo: `error[E0308]: …` or `warning: …` sets the severity of the
//!   ` --> src/main.rs:3:5` line that follows.
//! - tsc: `src/a.ts(12,5): error TS2322: …` and `src/a.ts:12:5 - error TS…`.
//! - Python: `tests/test_a.py:42: AssertionError` (pytest) and
//!   `File "x.py", line 42` (tracebacks).
//! - Anything else with `path:line[:col]`; "error" or "warning" in the line
//!   gives it a severity.
//!
//! Paths are tried against the task's directory, then each parent up to the
//! project root (cargo prints paths relative to its workspace root). Only
//! existing files become links.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A location in a file, 0-based like `Effect::OpenFileAt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    /// Byte range of the location in the line's text.
    pub span: Range<usize>,
}

/// One line of task output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLine {
    pub text: String,
    pub severity: Option<Severity>,
    pub link: Option<Link>,
}

impl RunLine {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), severity: None, link: None }
    }
}

macro_rules! re {
    ($name:ident, $re:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($re).unwrap());
    };
}

re!(RUST_HEADER, r"^(error|warning)(?:\[[A-Za-z0-9]+\])?:");
re!(RUST_NOTE, r"^(?:note|help)(?:\[[A-Za-z0-9]+\])?:");
re!(RUST_ARROW, r"^\s*--> (.+?):(\d+):(\d+)\s*$");
re!(TSC_PAREN, r"^(.+?)\((\d+),(\d+)\): (error|warning) TS\d+");
re!(TSC_PRETTY, r"^(.+?):(\d+):(\d+) - (error|warning) TS\d+");
re!(PY_SHORT, r"^(\S+\.py):(\d+): (.*)$");
re!(PY_FILE, r#"^\s*File "([^"]+)", line (\d+)"#);
re!(PY_ERROR, r"\w*(?:Error|Exception)\b");
re!(GENERIC, r#"(?:^|[\s(\[{"'=,])((?:[^\s:()\[\]{}"',]*[./\\])?[^\s:()\[\]{}"',]*\.[A-Za-z0-9]+):(\d+)(?::(\d+))?"#);
re!(WORD_ERROR, r"(?i)\berror\b");
re!(WORD_WARNING, r"(?i)\bwarn(?:ing)?\b");

/// Matches the lines of one run in order (rustc locations depend on the line
/// before). Caches which paths exist.
#[derive(Debug)]
pub struct Matcher {
    /// The task directory, then its parents up to the root.
    dirs: Vec<PathBuf>,
    /// Severity of the last rustc header, until a blank line.
    pending: Option<Severity>,
    resolved: HashMap<String, Option<PathBuf>>,
}

impl Matcher {
    pub fn new(cwd: &Path, root: &Path) -> Self {
        let mut dirs = vec![cwd.to_path_buf()];
        if cwd.starts_with(root) {
            let mut d = cwd;
            while d != root {
                let Some(p) = d.parent() else { break };
                dirs.push(p.to_path_buf());
                d = p;
            }
        } else {
            dirs.push(root.to_path_buf());
        }
        Self { dirs, pending: None, resolved: HashMap::new() }
    }

    pub fn line(&mut self, text: String) -> RunLine {
        let (severity, link) = self.scan(&text);
        RunLine { text, severity, link }
    }

    fn scan(&mut self, text: &str) -> (Option<Severity>, Option<Link>) {
        if text.trim().is_empty() {
            self.pending = None;
            return (None, None);
        }
        if let Some(c) = RUST_HEADER.captures(text) {
            self.pending = Some(severity(&c[1]));
            return (self.pending, None);
        }
        if RUST_NOTE.is_match(text) {
            self.pending = None;
            return (None, None);
        }
        if let Some(c) = RUST_ARROW.captures(text) {
            return (self.pending, self.link(&c, 1, Some(2), Some(3)));
        }
        for re in [&*TSC_PAREN, &*TSC_PRETTY] {
            if let Some(c) = re.captures(text) {
                let link = self.link(&c, 1, Some(2), Some(3));
                if link.is_some() {
                    return (Some(severity(&c[4])), link);
                }
            }
        }
        if let Some(c) = PY_SHORT.captures(text) {
            let link = self.link(&c, 1, Some(2), None);
            if link.is_some() {
                return (PY_ERROR.is_match(&c[3]).then_some(Severity::Error), link);
            }
        }
        if let Some(c) = PY_FILE.captures(text) {
            let link = self.link(&c, 1, Some(2), None);
            if link.is_some() {
                return (None, link);
            }
        }
        for c in GENERIC.captures_iter(text) {
            if let Some(link) = self.link(&c, 1, Some(2), Some(3)) {
                let sev = if WORD_ERROR.is_match(text) {
                    Some(Severity::Error)
                } else if WORD_WARNING.is_match(text) {
                    Some(Severity::Warning)
                } else {
                    None
                };
                return (sev, Some(link));
            }
        }
        (None, None)
    }

    /// A link from capture groups: the path, the 1-based line and column.
    /// The span covers the path and the numbers written right after it
    /// (`a.rs:3:5`, `a.ts(12,5)`), not a separate `line 7`.
    fn link(&mut self, c: &regex::Captures, path: usize, line: Option<usize>, col: Option<usize>) -> Option<Link> {
        let p = c.get(path)?;
        let resolved = self.resolve(p.as_str())?;
        let num = |i: Option<usize>| i.and_then(|i| c.get(i));
        let (l, k) = (num(line), num(col));
        let mut end = p.end();
        for m in [l, k].into_iter().flatten() {
            if m.start() == end + 1 {
                end = m.end();
            }
        }
        // tsc's `file(12,5)` closes with a parenthesis.
        let whole = c.get(0)?;
        if end > p.end() && whole.as_str()[end - whole.start()..].starts_with(')') {
            end += 1;
        }
        let parse = |m: Option<regex::Match>| m.and_then(|m| m.as_str().parse::<usize>().ok()).map_or(0, |n| n.saturating_sub(1));
        Some(Link { path: resolved, line: parse(l), col: parse(k), span: p.start()..end })
    }

    fn resolve(&mut self, raw: &str) -> Option<PathBuf> {
        if let Some(r) = self.resolved.get(raw) {
            return r.clone();
        }
        let p = Path::new(raw);
        let found = if p.is_absolute() {
            p.is_file().then(|| normalize(p))
        } else {
            self.dirs.iter().map(|d| d.join(p)).find(|f| f.is_file()).map(|f| normalize(&f))
        };
        self.resolved.insert(raw.to_string(), found.clone());
        found
    }
}

fn severity(word: &str) -> Severity {
    if word == "error" { Severity::Error } else { Severity::Warning }
}

/// `p` without `.` components and with `..` applied, so a file reached as
/// `./src/a.rs` opens the same tab as `src/a.rs`.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project with `files` under a fresh temp root.
    fn project(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-matcher-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in files {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        dir
    }

    fn scan(m: &mut Matcher, text: &str) -> RunLine {
        m.line(text.to_string())
    }

    fn at(l: &RunLine) -> Option<(String, usize, usize, &str)> {
        let k = l.link.as_ref()?;
        Some((k.path.file_name()?.to_string_lossy().into_owned(), k.line, k.col, &l.text[k.span.clone()]))
    }

    #[test]
    fn rustc_locations_take_the_header_severity() {
        let root = project("rust", &["src/main.rs", "src/lib.rs"]);
        let mut m = Matcher::new(&root, &root);
        let h = scan(&mut m, "error[E0308]: mismatched types");
        assert_eq!((h.severity, h.link.is_none()), (Some(Severity::Error), true));
        let l = scan(&mut m, "  --> src/main.rs:3:5");
        assert_eq!(l.severity, Some(Severity::Error));
        assert_eq!(at(&l), Some(("main.rs".into(), 2, 4, "src/main.rs:3:5")));
        assert_eq!(l.link.unwrap().path, root.join("src/main.rs"));
        scan(&mut m, "");
        assert_eq!(scan(&mut m, "warning: unused variable: `x`").severity, Some(Severity::Warning));
        assert_eq!(scan(&mut m, " --> src/lib.rs:10:9").severity, Some(Severity::Warning));
        scan(&mut m, "");
        // A note's location is a link without a severity.
        scan(&mut m, "note: the lint level is defined here");
        let n = scan(&mut m, " --> src/lib.rs:1:9");
        assert_eq!((n.severity, at(&n).map(|a| a.1)), (None, Some(0)));
        // Files that do not exist are no links.
        assert_eq!(scan(&mut m, " --> src/gone.rs:1:1").link, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_paths_resolve_from_a_member_directory() {
        let root = project("ws", &["crates/core/src/lib.rs"]);
        let mut m = Matcher::new(&root.join("crates/core"), &root);
        scan(&mut m, "error: x");
        assert_eq!(scan(&mut m, " --> crates/core/src/lib.rs:2:1").link.unwrap().path, root.join("crates/core/src/lib.rs"));
        assert_eq!(scan(&mut m, "src/lib.rs:4: here").link.unwrap().path, root.join("crates/core/src/lib.rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tsc_both_styles() {
        let root = project("tsc", &["src/app.ts"]);
        let mut m = Matcher::new(&root, &root);
        let l = scan(&mut m, "src/app.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.");
        assert_eq!(l.severity, Some(Severity::Error));
        assert_eq!(at(&l), Some(("app.ts".into(), 11, 4, "src/app.ts(12,5)")));
        let l = scan(&mut m, "src/app.ts:3:1 - warning TS6133: 'x' is declared but never used.");
        assert_eq!(l.severity, Some(Severity::Warning));
        assert_eq!(at(&l), Some(("app.ts".into(), 2, 0, "src/app.ts:3:1")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pytest_and_tracebacks() {
        let root = project("py", &["tests/test_a.py", "pkg/x.py"]);
        let mut m = Matcher::new(&root, &root);
        let l = scan(&mut m, "tests/test_a.py:42: AssertionError");
        assert_eq!(l.severity, Some(Severity::Error));
        assert_eq!(at(&l), Some(("test_a.py".into(), 41, 0, "tests/test_a.py:42")));
        assert_eq!(scan(&mut m, "tests/test_a.py:10: in test_sum").severity, None);
        let l = scan(&mut m, r#"  File "pkg/x.py", line 7, in run"#);
        assert_eq!(at(&l), Some(("x.py".into(), 6, 0, "pkg/x.py")));
        let abs = root.join("pkg/x.py");
        let l = scan(&mut m, &format!(r#"  File "{}", line 3, in <module>"#, abs.display()));
        assert_eq!(l.link.unwrap().path, abs);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn generic_locations() {
        let root = project("generic", &["main.go", "src/a.c", "lib/util.js"]);
        let mut m = Matcher::new(&root, &root);
        let l = scan(&mut m, "./main.go:3:5: undefined: foo");
        assert_eq!((l.severity, at(&l)), (None, Some(("main.go".into(), 2, 4, "./main.go:3:5"))));
        assert_eq!(l.link.unwrap().path, root.join("main.go"), "normalized");
        let l = scan(&mut m, "src/a.c:10:2: error: expected ';'");
        assert_eq!((l.severity, at(&l).map(|a| a.1)), (Some(Severity::Error), Some(9)));
        let l = scan(&mut m, "    at run (lib/util.js:20:11)");
        assert_eq!(at(&l), Some(("util.js".into(), 19, 10, "lib/util.js:20:11")));
        let l = scan(&mut m, "thread 'main' panicked at src/a.c:4:9:");
        assert_eq!(at(&l).map(|a| (a.1, a.2)), Some((3, 8)));
        // Times, ports and missing files are not links.
        for text in ["12:30:45 started", "listening on 127.0.0.1:8080", "nope.rs:1:1 error", "error: 3 failed"] {
            assert_eq!(scan(&mut m, text).link, None, "{text}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
