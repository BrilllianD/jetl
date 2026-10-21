//! Test results from a run's output: a parser sniffing each line for
//! libtest (`cargo test`), `pytest -v` (also the xdist form) and flat TAP
//! (`vitest --reporter=tap-flat`), `jest --verbose` (and the failure sections
//! of plain jest), the runners that need a flag for it, and
//! the arguments that rerun only the failed tests.
//!
//! The parser is incremental and order-independent: a result line and a
//! failure section reach the same case whichever comes first, and a section
//! may be split over any number of batches.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::matcher::{Link, RunLine, Severity};
use super::{MatcherKind, RunTask, TaskCommand};

/// Most output lines kept per case.
pub const MAX_CASE_LINES: usize = 200;

macro_rules! re {
    ($name:ident, $re:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($re).unwrap());
    };
}

re!(LT_RUNNING, r"^\s*Running (?:unittests )?(\S+) \(");
re!(LT_DOC, r"^\s*Doc-tests (\S+)");
re!(LT_RESULT, r"^test (.+?) \.\.\. (ok|FAILED|ignored)(?:, .*)?$");
re!(LT_SECTION, r"^---- (.+?) stdout ----$");
re!(LT_END, r"^(?:failures:|test result: )");
re!(PY_RESULT, r"^(.+?::.+?) (PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS)\b");
re!(PY_XDIST, r"^\[gw\d+\] \[\s*\d+%\] (PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS) (.+::.+?)\s*$");
re!(PY_SECTION, r"^_{3,} (.+?) _{3,}$");
re!(PY_RULE, r"^={3,}");
re!(TAP_RESULT, r"^(not ok|ok) \d+ - (.*)$");
re!(JEST_HEADER, r"^ ?(PASS|FAIL) +(\S.*?)(?: \(\d[^()]*\))?$");
re!(JEST_RESULT, r"^( +)(✓|✕|○|✎|√|×) (.+?)(?: \(\d+(?:\.\d+)? m?s\))?$");
re!(JEST_FAILURE, r"^\s*● (.+?)\s*$");
re!(JEST_CONSOLE, r"^\s+console\.[a-z]+$");
re!(JEST_END, r"^(?:Test Suites:|Summary of all failing tests)");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStatus {
    Passed,
    Failed,
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Libtest,
    Pytest,
    Tap,
    Jest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    /// libtest: the binary ("src/lib.rs", "tests/cli.rs", "Doc-tests jtc"); pytest, TAP and jest: the file.
    pub suite: String,
    /// Module path / classes / describe blocks between suite and test ("" when none).
    pub group: String,
    /// The leaf name.
    pub name: String,
    /// What the runner's filter takes: libtest's full name, pytest's node id, TAP's name after the file,
    /// jest's describe titles and name joined by ` › ` (empty for a file that failed to load).
    pub id: String,
    pub status: TestStatus,
    pub format: Format,
    /// The failure section, at most `MAX_CASE_LINES` lines.
    pub output: Vec<RunLine>,
}

impl TestCase {
    /// The last Error-severity link of `output`, else its first link.
    pub fn link(&self) -> Option<&Link> {
        let errors = self.output.iter().rev().find(|l| l.severity == Some(Severity::Error)).and_then(|l| l.link.as_ref());
        errors.or_else(|| self.output.iter().find_map(|l| l.link.as_ref()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
}

/// Where the lines being read belong.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Capture {
    #[default]
    None,
    Libtest(usize),
    Pytest(usize),
    Tap(usize),
    /// A jest `●` section; `None` swallows one that has its output already, or `● Console`.
    Jest(Option<usize>),
}

#[derive(Debug, Default)]
pub struct TestRun {
    cases: Vec<TestCase>,
    index: HashMap<(String, String), usize>,
    suites: Vec<String>,
    /// The libtest binary being read ("tests" before any `Running` line).
    lt_suite: Option<String>,
    /// The jest file of the last `PASS`/`FAIL` header, until the summary.
    jest_suite: Option<String>,
    /// Indent and title of the open jest describe lines, indents strictly increasing.
    jest_groups: Vec<(usize, String)>,
    capture: Capture,
    /// Blank lines read in the capture, kept only when more text follows.
    blanks: usize,
}

impl TestRun {
    pub fn feed(&mut self, lines: &[RunLine]) {
        for l in lines {
            self.line(l);
        }
    }

    pub fn cases(&self) -> &[TestCase] {
        &self.cases
    }

    /// Suites in first-seen order.
    pub fn suites(&self) -> &[String] {
        &self.suites
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for case in &self.cases {
            match case.status {
                TestStatus::Passed => c.passed += 1,
                TestStatus::Failed => c.failed += 1,
                TestStatus::Ignored => c.ignored += 1,
            }
        }
        c
    }

    pub fn failed(&self) -> impl Iterator<Item = &TestCase> {
        self.cases.iter().filter(|c| c.status == TestStatus::Failed)
    }

    fn line(&mut self, l: &RunLine) {
        let t = l.text.as_str();
        match self.capture {
            Capture::Libtest(i) => {
                if !(LT_SECTION.is_match(t) || LT_END.is_match(t) || LT_RUNNING.is_match(t) || LT_DOC.is_match(t)) {
                    return self.push(i, l);
                }
                self.stop();
            }
            Capture::Pytest(i) => {
                if !(PY_SECTION.is_match(t) || PY_RULE.is_match(t)) {
                    return self.push(i, l);
                }
                self.stop();
            }
            Capture::Tap(i) => {
                if t.starts_with(char::is_whitespace) {
                    return self.push(i, l);
                }
                self.stop();
            }
            Capture::Jest(i) => {
                let ends =
                    (!t.trim().is_empty() && !t.starts_with(char::is_whitespace)) || JEST_FAILURE.is_match(t) || JEST_CONSOLE.is_match(t);
                if !ends {
                    if let Some(i) = i {
                        self.push(i, l);
                    }
                    return;
                }
                self.stop();
            }
            Capture::None => {}
        }
        if let Some(c) = LT_RUNNING.captures(t) {
            self.lt_suite = Some(c[1].trim_matches('`').to_string());
        } else if let Some(c) = LT_DOC.captures(t) {
            self.lt_suite = Some(format!("Doc-tests {}", &c[1]));
        } else if let Some(c) = LT_RESULT.captures(t) {
            let status = match &c[2] {
                "ok" => TestStatus::Passed,
                "FAILED" => TestStatus::Failed,
                _ => TestStatus::Ignored,
            };
            let id = c[1].strip_suffix(" - should panic").unwrap_or(&c[1]);
            self.libtest(id, Some(status));
        } else if let Some(c) = LT_SECTION.captures(t) {
            let i = self.libtest(&c[1], None);
            self.start(Capture::Libtest(i));
        } else if let Some(c) = PY_SECTION.captures(t) {
            let header = c[1].strip_prefix("ERROR at setup of ").or_else(|| c[1].strip_prefix("ERROR at teardown of ")).unwrap_or(&c[1]);
            if let Some(i) = self.cases.iter().position(|k| {
                k.format == Format::Pytest && k.status == TestStatus::Failed && k.output.is_empty() && pytest_header(k) == header
            }) {
                self.start(Capture::Pytest(i));
            }
        } else if let Some(c) = PY_XDIST.captures(t) {
            self.pytest(&c[2], &c[1]);
        } else if let Some(c) = PY_RESULT.captures(t) {
            self.pytest(&c[1], &c[2]);
        } else if let Some(c) = TAP_RESULT.captures(t) {
            let i = self.tap(&c[2], &c[1] == "ok");
            if self.cases[i].status == TestStatus::Failed {
                self.start(Capture::Tap(i));
            }
        } else if let Some(c) = JEST_HEADER.captures(t) {
            self.jest_suite = Some(c[2].to_string());
            self.jest_groups.clear();
        } else if JEST_END.is_match(t) {
            self.jest_suite = None;
            self.jest_groups.clear();
        } else if let Some(suite) = self.jest_suite.clone() {
            self.jest_line(suite, t);
        }
    }

    fn jest_line(&mut self, suite: String, t: &str) {
        if let Some(c) = JEST_FAILURE.captures(t) {
            let title = &c[1];
            if title == "Console" {
                return self.start(Capture::Jest(None));
            }
            let case = if title == "Test suite failed to run" {
                TestCase {
                    suite,
                    group: String::new(),
                    name: title.to_string(),
                    id: String::new(),
                    status: TestStatus::Failed,
                    format: Format::Jest,
                    output: Vec::new(),
                }
            } else {
                let (group, name) = title.rsplit_once(" › ").unwrap_or(("", title));
                TestCase {
                    suite,
                    group: group.to_string(),
                    name: name.to_string(),
                    id: title.to_string(),
                    status: TestStatus::Failed,
                    format: Format::Jest,
                    output: Vec::new(),
                }
            };
            let i = self.upsert(case, None);
            let capture = if self.cases[i].output.is_empty() { Some(i) } else { None };
            self.start(Capture::Jest(capture));
        } else if let Some(c) = JEST_RESULT.captures(t) {
            let indent = c[1].len();
            self.jest_groups.retain(|(n, _)| *n < indent);
            let rest = &c[3];
            let (status, name) = match &c[2] {
                "✓" | "√" => (TestStatus::Passed, rest),
                "✕" | "×" => (TestStatus::Failed, rest),
                "○" => (TestStatus::Ignored, rest.strip_prefix("skipped ").unwrap_or(rest)),
                _ => (TestStatus::Ignored, rest.strip_prefix("todo ").unwrap_or(rest)),
            };
            let group = self.jest_groups.iter().map(|(_, g)| g.as_str()).collect::<Vec<_>>().join(" › ");
            let id = if group.is_empty() { name.to_string() } else { format!("{group} › {name}") };
            let case = TestCase { suite, group, name: name.to_string(), id, status, format: Format::Jest, output: Vec::new() };
            self.upsert(case, Some(status));
        } else if t.starts_with(' ') && !t.trim().is_empty() {
            let indent = t.len() - t.trim_start_matches(' ').len();
            self.jest_groups.retain(|(n, _)| *n < indent);
            self.jest_groups.push((indent, t.trim().to_string()));
        }
    }

    fn start(&mut self, c: Capture) {
        self.capture = c;
        self.blanks = 0;
    }

    fn stop(&mut self) {
        self.capture = Capture::None;
        self.blanks = 0;
    }

    /// Add `l` to the output of case `i`; blank lines wait for text after them.
    fn push(&mut self, i: usize, l: &RunLine) {
        if l.text.trim().is_empty() {
            if !self.cases[i].output.is_empty() {
                self.blanks += 1;
            }
            return;
        }
        let out = &mut self.cases[i].output;
        for _ in 0..std::mem::take(&mut self.blanks) {
            if out.len() < MAX_CASE_LINES {
                out.push(RunLine::plain(""));
            }
        }
        if out.len() < MAX_CASE_LINES {
            out.push(l.clone());
        }
    }

    /// The case `(suite, id)`, created Failed or with `status`; an existing one takes `status`.
    fn upsert(&mut self, mut case: TestCase, status: Option<TestStatus>) -> usize {
        let key = (case.suite.clone(), case.id.clone());
        if let Some(&i) = self.index.get(&key) {
            if let Some(s) = status {
                self.cases[i].status = s;
            }
            return i;
        }
        if !self.suites.contains(&case.suite) {
            self.suites.push(case.suite.clone());
        }
        case.status = status.unwrap_or(TestStatus::Failed);
        self.cases.push(case);
        self.index.insert(key, self.cases.len() - 1);
        self.cases.len() - 1
    }

    fn libtest(&mut self, id: &str, status: Option<TestStatus>) -> usize {
        let suite = self.lt_suite.clone().unwrap_or_else(|| "tests".into());
        let (group, name) = match id.split_once(" - ") {
            Some((g, n)) => (g.to_string(), n.to_string()),
            None => match id.rsplit_once("::") {
                Some((g, n)) => (g.to_string(), n.to_string()),
                None => (String::new(), id.to_string()),
            },
        };
        let case =
            TestCase { suite, group, name, id: id.to_string(), status: TestStatus::Failed, format: Format::Libtest, output: Vec::new() };
        self.upsert(case, status)
    }

    fn pytest(&mut self, node: &str, word: &str) {
        let status = match word {
            "PASSED" | "XPASS" => TestStatus::Passed,
            "FAILED" | "ERROR" => TestStatus::Failed,
            _ => TestStatus::Ignored,
        };
        let (suite, rest) = node.split_once("::").unwrap_or(("", node));
        // Parameters may hold `::`: split only the part before the first `[`.
        let (path, params) = rest.split_at(rest.find('[').unwrap_or(rest.len()));
        let mut parts: Vec<&str> = path.split("::").collect();
        let last = parts.pop().unwrap_or("");
        let case = TestCase {
            suite: suite.to_string(),
            group: parts.join("::"),
            name: format!("{last}{params}"),
            id: node.to_string(),
            status,
            format: Format::Pytest,
            output: Vec::new(),
        };
        self.upsert(case, Some(status));
    }

    fn tap(&mut self, rest: &str, ok: bool) -> usize {
        let (name, directive) = split_tap(rest);
        let skipped =
            directive.map(|d| d.trim_start().to_ascii_uppercase()).is_some_and(|d| d.starts_with("SKIP") || d.starts_with("TODO"));
        let status = match (ok, skipped) {
            (_, true) => TestStatus::Ignored,
            (true, false) => TestStatus::Passed,
            (false, false) => TestStatus::Failed,
        };
        let parts: Vec<&str> = name.split(" > ").collect();
        let (suite, group, leaf, id) = match parts.as_slice() {
            [only] => ("tests".to_string(), String::new(), only.to_string(), only.to_string()),
            [file, mid @ .., leaf] => (file.to_string(), mid.join(" > "), leaf.to_string(), parts[1..].join(" > ")),
            [] => unreachable!("split yields one part"),
        };
        let case = TestCase { suite, group, name: leaf, id, status, format: Format::Tap, output: Vec::new() };
        self.upsert(case, Some(status))
    }
}

/// The header pytest prints over a failure: classes and test joined by dots.
fn pytest_header(c: &TestCase) -> String {
    if c.group.is_empty() { c.name.clone() } else { format!("{}.{}", c.group.replace("::", "."), c.name) }
}

/// A TAP result's text after the number: the unescaped name and the directive after the first unescaped ` # `.
fn split_tap(rest: &str) -> (String, Option<&str>) {
    let b = rest.as_bytes();
    let mut i = 0;
    let mut cut = None;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
        } else if rest[i..].starts_with(" # ") {
            cut = Some(i);
            break;
        } else {
            i += 1;
        }
    }
    let (raw, directive) = match cut {
        Some(i) => (&rest[..i], Some(&rest[i + 3..])),
        None => (rest, None),
    };
    let mut name = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(e @ ('#' | '\\')) => name.push(e),
                Some(e) => {
                    name.push('\\');
                    name.push(e);
                }
                None => name.push('\\'),
            }
        } else {
            name.push(c);
        }
    }
    (name, directive)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    Cargo,
    Pytest,
    Vitest,
    Jest,
}

/// The words of the command, then `args`; a string command is split on whitespace.
fn words(task: &RunTask) -> Vec<String> {
    let mut w: Vec<String> = match &task.command {
        TaskCommand::Argv(v) => v.clone(),
        TaskCommand::Shell(s) => s.split_whitespace().map(String::from).collect(),
    };
    w.extend(task.args.iter().cloned());
    w
}

fn file_name(w: &str) -> &str {
    std::path::Path::new(w).file_name().and_then(|n| n.to_str()).unwrap_or(w)
}

/// The index of the word after which pytest's own flags go: the `pytest` word or the one after `-m pytest`.
fn pytest_word(words: &[String]) -> Option<usize> {
    words.iter().enumerate().find_map(|(i, w)| {
        let direct = matches!(file_name(w), "pytest" | "py.test");
        let module = w == "-m" && words.get(i + 1).is_some_and(|n| n == "pytest");
        if direct {
            Some(i)
        } else if module {
            Some(i + 1)
        } else {
            None
        }
    })
}

impl Runner {
    /// The runner a task's command is, first match in the order cargo, pytest, vitest, jest.
    pub fn detect(task: &RunTask) -> Option<Runner> {
        let w = words(task);
        let cargo = w.iter().enumerate().any(|(i, x)| {
            file_name(x) == "cargo" && w[i + 1..].iter().find(|n| !n.starts_with(['+', '-'])).is_some_and(|n| n == "test" || n == "t")
        });
        if cargo {
            Some(Runner::Cargo)
        } else if pytest_word(&w).is_some() {
            Some(Runner::Pytest)
        } else if w.iter().any(|x| file_name(x) == "vitest") {
            Some(Runner::Vitest)
        } else if w.iter().any(|x| file_name(x) == "jest") {
            Some(Runner::Jest)
        } else {
            None
        }
    }

    /// `argv` with the flag that makes the runner print per-test results, when missing.
    pub fn prepare(self, mut argv: Vec<String>) -> Vec<String> {
        match self {
            Runner::Cargo => {}
            Runner::Pytest => {
                // -q cancels -v, and no PASSED lines would be printed.
                argv.retain(|w| !matches!(w.as_str(), "-q" | "-qq" | "-qqq" | "--quiet"));
                let verbose = argv.iter().any(|w| matches!(w.as_str(), "-v" | "-vv" | "-vvv" | "--verbose"));
                if !verbose && let Some(i) = pytest_word(&argv) {
                    argv.insert(i + 1, "-v".into());
                }
            }
            Runner::Vitest => {
                if !argv.iter().any(|w| w.starts_with("--reporter")) {
                    argv.push("--reporter=tap-flat".into());
                }
            }
            Runner::Jest => {
                if !argv.iter().any(|w| w == "--verbose" || w.starts_with("--verbose=")) {
                    argv.push("--verbose".into());
                }
            }
        }
        argv
    }
}

/// Removes `-t X`, `--testNamePattern X` and `--testNamePattern=X` from jest words.
fn drop_jest_name_pattern(words: &mut Vec<String>) {
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        if w == "-t" || w == "--testNamePattern" {
            words.drain(i..(i + 2).min(words.len()));
        } else if w.starts_with("--testNamePattern=") {
            words.remove(i);
        } else {
            i += 1;
        }
    }
}

/// `task` rerun for its failed tests only.
pub fn rerun_failed(task: &RunTask, run: &TestRun) -> Result<RunTask, String> {
    let failed: Vec<&TestCase> = run.failed().collect();
    if failed.is_empty() {
        return Err("No failed tests".into());
    }
    let runner = Runner::detect(task).ok_or("Rerun Failed needs a cargo test, pytest, vitest or jest command")?;
    let unique = |items: Vec<String>| {
        let mut seen: Vec<String> = Vec::new();
        for s in items {
            if !seen.contains(&s) {
                seen.push(s);
            }
        }
        seen
    };
    let mut out = RunTask { name: format!("{} (failed)", task.name), matcher: Some(MatcherKind::Tests), ..task.clone() };
    let all = words(task);
    let extra = match runner {
        Runner::Cargo => {
            let mut e = Vec::new();
            if !all.iter().any(|w| w == "--") {
                e.push("--".to_string());
            }
            e.push("--exact".into());
            e.extend(unique(failed.iter().map(|c| c.id.clone()).collect()));
            e
        }
        Runner::Pytest => {
            let prefix = pytest_word(&all).map_or(0, |i| i + 1);
            let mut w: Vec<String> = all[..prefix].to_vec();
            w.push("-v".into());
            w.extend(unique(failed.iter().map(|c| c.id.clone()).collect()));
            out.command = TaskCommand::Argv(w);
            out.args.clear();
            return Ok(out);
        }
        Runner::Vitest => {
            let mut e = unique(failed.iter().map(|c| c.suite.clone()).collect());
            let alts = unique(failed.iter().map(|c| regex::escape(&c.id.replace(" > ", " "))).collect());
            e.push("-t".into());
            e.push(format!("^(?:{})$", alts.join("|")));
            e
        }
        Runner::Jest => {
            // Two `-t` options make jest match their patterns joined with a comma, which
            // matches nothing; the one added below replaces any already in the command.
            drop_jest_name_pattern(&mut out.args);
            if let TaskCommand::Argv(w) = &mut out.command {
                drop_jest_name_pattern(w);
            }
            // jest reads file arguments and `-t` as regular expressions; a file that
            // failed to load has no name to filter, so it reruns as a whole.
            let mut e: Vec<String> = unique(failed.iter().map(|c| regex::escape(&c.suite)).collect());
            let alts = unique(failed.iter().filter(|c| !c.id.is_empty()).map(|c| regex::escape(&c.id.replace(" › ", " "))).collect());
            if !alts.is_empty() {
                e.push("-t".into());
                e.push(format!("^(?:{})$", alts.join("|")));
            }
            e
        }
    };
    // A string command runs as `<string> "$@"`, so the words go after the
    // user's args for both kinds of command.
    out.args.extend(extra);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::matcher::Matcher;
    use super::*;

    const LIBTEST: &str = include_str!("fixtures/libtest.txt");
    const TWO_BINARIES: &str = include_str!("fixtures/libtest_two_binaries.txt");
    const PYTEST: &str = include_str!("fixtures/pytest.txt");
    const XDIST: &str = include_str!("fixtures/pytest_xdist.txt");
    const TAP: &str = include_str!("fixtures/vitest_tap_flat.txt");
    const JEST: &str = include_str!("fixtures/jest_verbose.txt");
    const JEST_DEFAULT: &str = include_str!("fixtures/jest_default.txt");

    /// A project holding the files the fixtures mention.
    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-testing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in ["src/lib.rs", "tests/cli.rs", "tests/test_a.py", "src/math.test.ts", "src/math.test.js", "src/broken.test.js"] {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        dir
    }

    fn lines(root: &Path, text: &str) -> Vec<RunLine> {
        let mut m = Matcher::new(root, root);
        text.lines().map(|l| m.line(l.to_string())).collect()
    }

    fn parse(root: &Path, text: &str) -> TestRun {
        let mut run = TestRun::default();
        run.feed(&lines(root, text));
        run
    }

    fn tree(run: &TestRun) -> Vec<String> {
        let mut v: Vec<String> = run.cases().iter().map(|c| format!("{}/{}/{}={:?}", c.suite, c.group, c.name, c.status)).collect();
        v.sort();
        v
    }

    fn case<'a>(run: &'a TestRun, name: &str) -> &'a TestCase {
        run.cases().iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no case {name}"))
    }

    fn at(l: &Link) -> (String, usize, usize) {
        (l.path.file_name().unwrap().to_string_lossy().into_owned(), l.line, l.col)
    }

    #[test]
    fn libtest_cases_status_and_failure_location() {
        let root = project("lt");
        let run = parse(&root, LIBTEST);
        assert_eq!(
            tree(&run),
            [
                "src/lib.rs/tests/adds=Passed",
                "src/lib.rs/tests/panics=Passed",
                "src/lib.rs/tests/slow=Ignored",
                "src/lib.rs/tests::nested/fails=Failed"
            ]
        );
        let fails = case(&run, "fails");
        assert_eq!(fails.id, "tests::nested::fails");
        assert_eq!(fails.output[0].text, "thread 'tests::nested::fails' panicked at src/lib.rs:12:9:");
        assert_eq!(fails.output.last().unwrap().text.split(':').next(), Some("note"), "trailing blanks are not kept");
        assert_eq!(fails.link().map(at), Some(("lib.rs".into(), 11, 8)));
        assert_eq!(run.counts(), Counts { passed: 2, failed: 1, ignored: 1 });
        assert!(run.cases().iter().filter(|c| c.status != TestStatus::Failed).all(|c| c.output.is_empty()));
        assert_eq!(run.cases().iter().map(|c| c.output.len()).sum::<usize>(), fails.output.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn libtest_order_does_not_matter() {
        let root = project("lt-order");
        let base = parse(&root, LIBTEST);
        let reversed: String = {
            let mut v: Vec<&str> = LIBTEST.lines().collect();
            let results: Vec<usize> = (0..v.len()).filter(|&i| v[i].starts_with("test ") && v[i].contains(" ... ")).collect();
            let picked: Vec<&str> = results.iter().rev().map(|&i| v[i]).collect();
            for (&i, l) in results.iter().zip(picked) {
                v[i] = l;
            }
            v.join("\n")
        };
        let all = lines(&root, &reversed);
        let split = all.iter().position(|l| l.text.starts_with("thread ")).unwrap() + 1;
        let mut run = TestRun::default();
        run.feed(&all[..split]);
        run.feed(&all[split..]);
        assert_eq!(tree(&run), tree(&base));
        assert_eq!(case(&run, "fails").output, case(&base, "fails").output);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn libtest_binaries_are_suites() {
        let root = project("lt-bins");
        let mut run = parse(&root, TWO_BINARIES);
        assert_eq!(run.suites(), ["src/lib.rs", "tests/cli.rs", "Doc-tests jtc"]);
        let outside = case(&run, "outside");
        assert_eq!(outside.suite, "tests/cli.rs");
        assert_eq!(outside.link().map(at), Some(("cli.rs".into(), 1, 15)));
        let doc = case(&run, "add (line 3)");
        assert_eq!((doc.suite.as_str(), doc.group.as_str()), ("Doc-tests jtc", "src/lib.rs"));
        assert_eq!(doc.status, TestStatus::Passed);
        // The same name in two binaries stays two cases.
        run.feed(&lines(&root, "     Running tests/other.rs (x)\ntest tests::adds ... ok"));
        assert_eq!(run.cases().iter().filter(|c| c.id == "tests::adds").count(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pytest_verbose_and_xdist() {
        let root = project("py");
        let run = parse(&root, PYTEST);
        assert_eq!(
            tree(&run),
            [
                "tests/test_a.py//test_ok=Passed",
                "tests/test_a.py//test_param[1-2]=Passed",
                "tests/test_a.py//test_skip=Ignored",
                "tests/test_a.py/TestMath/test_fail=Failed"
            ]
        );
        assert_eq!(tree(&parse(&root, XDIST)), tree(&run));
        for r in [&run, &parse(&root, XDIST)] {
            let f = case(r, "test_fail");
            assert_eq!(f.id, "tests/test_a.py::TestMath::test_fail");
            assert_eq!(f.output.last().unwrap().text, "tests/test_a.py:7: AssertionError");
            assert_eq!(f.link().map(at), Some(("test_a.py".into(), 6, 0)));
            assert!(f.output[0].text.starts_with("self = "), "the header and the blank line below it are skipped");
        }
        assert_eq!(run.counts(), Counts { passed: 2, failed: 1, ignored: 1 });
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tap_flat() {
        let root = project("tap");
        let run = parse(&root, TAP);
        assert_eq!(run.suites(), ["src/math.test.ts", "src/other.test.ts"]);
        assert_eq!(
            tree(&run),
            [
                "src/math.test.ts/math > nested/fails=Failed",
                "src/math.test.ts/math/adds=Passed",
                "src/math.test.ts/math/skipped=Ignored",
                "src/other.test.ts//top level=Passed"
            ]
        );
        let fails = case(&run, "fails");
        assert_eq!(fails.id, "math > nested > fails");
        assert_eq!(fails.link().map(at), Some(("math.test.ts".into(), 8, 16)));
        assert_eq!(fails.output.len(), 8, "the indented block, not the next result");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tap_names_unescape_and_directives_split() {
        let mut run = TestRun::default();
        run.feed(&[
            RunLine::plain(r"ok 1 - a.test.ts > has \# hash # time=1ms"),
            RunLine::plain(r"not ok 2 - a.test.ts > todo # TODO later"),
        ]);
        assert_eq!(run.cases()[0].name, "has # hash");
        assert_eq!(run.cases()[1].status, TestStatus::Ignored);
    }

    #[test]
    fn output_is_capped() {
        let mut text = String::from("     Running unittests src/lib.rs (x)\ntest a ... FAILED\n---- a stdout ----\n");
        for i in 0..500 {
            text.push_str(&format!("line {i}\n"));
        }
        let run = parse(&project("cap"), &text);
        assert_eq!(case(&run, "a").output.len(), MAX_CASE_LINES);
        assert_eq!(case(&run, "a").output[0].text, "line 0");
    }

    fn task(cmd: TaskCommand, args: &[&str]) -> RunTask {
        RunTask {
            name: "t".into(),
            command: cmd,
            cwd: None,
            env: Default::default(),
            args: args.iter().map(|s| s.to_string()).collect(),
            matcher: None,
            origin: Default::default(),
        }
    }

    fn argv(w: &[&str]) -> TaskCommand {
        TaskCommand::Argv(w.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn rerun_failed_args_per_runner() {
        let root = project("rerun");
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let lt = parse(&root, LIBTEST);
        let r = rerun_failed(&task(argv(&["cargo", "test"]), &[]), &lt).unwrap();
        assert_eq!(r.args, ["--", "--exact", "tests::nested::fails"]);
        assert_eq!((r.name.as_str(), r.matcher), ("t (failed)", Some(MatcherKind::Tests)));
        let r = rerun_failed(&task(argv(&["cargo", "test"]), &["--", "--nocapture"]), &lt).unwrap();
        assert_eq!(r.args, ["--", "--nocapture", "--exact", "tests::nested::fails"]);
        let r = rerun_failed(&task(TaskCommand::Shell("cargo test".into()), &[]), &lt).unwrap();
        assert_eq!(r.command, TaskCommand::Shell("cargo test".into()));
        assert_eq!(r.args, ["--", "--exact", "tests::nested::fails"]);
        let r = rerun_failed(&task(TaskCommand::Shell("cargo test -- --nocapture".into()), &[]), &lt).unwrap();
        assert_eq!(r.command, TaskCommand::Shell("cargo test -- --nocapture".into()));
        assert_eq!(r.args, ["--exact", "tests::nested::fails"]);
        // User args come after the string: cargo flags stay before `--`, libtest flags after.
        let r = rerun_failed(&task(TaskCommand::Shell("cargo test".into()), &["--release"]), &lt).unwrap();
        assert_eq!(r.argv()[3..], ["sh", "--release", "--", "--exact", "tests::nested::fails"]);
        assert_eq!(r.argv()[2], "cargo test \"$@\"");
        let r = rerun_failed(&task(TaskCommand::Shell("cargo test".into()), &["--", "--nocapture"]), &lt).unwrap();
        assert_eq!(r.args, ["--", "--nocapture", "--exact", "tests::nested::fails"]);

        let py = parse(&root, PYTEST);
        let r = rerun_failed(&task(argv(&["python3", "-m", "pytest", "tests/"]), &["-x"]), &py).unwrap();
        assert_eq!(r.command, argv(&["python3", "-m", "pytest", "-v", "tests/test_a.py::TestMath::test_fail"]));
        assert!(r.args.is_empty());
        let r = rerun_failed(&task(TaskCommand::Shell("pytest -q".into()), &[]), &py).unwrap();
        assert_eq!(r.command, argv(&["pytest", "-v", "tests/test_a.py::TestMath::test_fail"]));

        let tap = parse(&root, TAP);
        let r = rerun_failed(&task(argv(&["npx", "vitest", "run"]), &[]), &tap).unwrap();
        assert_eq!(r.args, strs(&["src/math.test.ts", "-t", "^(?:math nested fails)$"]));
        let r = rerun_failed(&task(TaskCommand::Shell("npx vitest run".into()), &[]), &tap).unwrap();
        assert_eq!(r.command, TaskCommand::Shell("npx vitest run".into()));
        assert_eq!(r.args, strs(&["src/math.test.ts", "-t", "^(?:math nested fails)$"]));

        let jest = parse(&root, JEST);
        let jest_args = strs(&[r"src/math\.test\.js", r"src/broken\.test\.js", "-t", r"^(?:math f\(x\) \[1\] b\.c|math nested fails)$"]);
        let r = rerun_failed(&task(argv(&["npx", "jest"]), &[]), &jest).unwrap();
        assert_eq!(r.args, jest_args);
        let r = rerun_failed(&task(TaskCommand::Shell("npx jest".into()), &[]), &jest).unwrap();
        assert_eq!(r.command, TaskCommand::Shell("npx jest".into()));
        assert_eq!(r.args, jest_args);
        // A caret-run `-t` is replaced, not repeated: two `-t` make jest join the patterns.
        let r = rerun_failed(&task(argv(&["npx", "jest", "src/math.test.js", "-t", "^math adds$", "--verbose"]), &[]), &jest).unwrap();
        assert_eq!(r.command, argv(&["npx", "jest", "src/math.test.js", "--verbose"]));
        let r = rerun_failed(&task(argv(&["npx", "jest"]), &["--testNamePattern=x", "-t", "y", "--testNamePattern", "z"]), &jest).unwrap();
        assert_eq!(r.args, jest_args);
        let mut broken = TestRun::default();
        broken.feed(&[
            RunLine::plain("FAIL src/broken.test.js"),
            RunLine::plain("  ● Test suite failed to run"),
            RunLine::plain("    boom"),
        ]);
        let r = rerun_failed(&task(argv(&["npx", "jest"]), &[]), &broken).unwrap();
        assert_eq!(r.args, strs(&[r"src/broken\.test\.js"]));

        let ok = parse(&root, "     Running unittests src/lib.rs (x)\ntest a ... ok");
        assert_eq!(rerun_failed(&task(argv(&["cargo", "test"]), &[]), &ok).unwrap_err(), "No failed tests");
        assert_eq!(
            rerun_failed(&task(argv(&["make", "test"]), &[]), &lt).unwrap_err(),
            "Rerun Failed needs a cargo test, pytest, vitest or jest command"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn captured_running_line_keeps_the_suite() {
        let root = project("running");
        let text = "     Running unittests src/lib.rs (target/debug/deps/jtc-01)\nrunning 1 test\ntest a::fails ... FAILED\n\nfailures:\n\n---- a::fails stdout ----\n    Running foo\nafter\n\nfailures:\n    a::fails\n\ntest result: FAILED. 0 passed; 1 failed";
        let run = parse(&root, text);
        assert_eq!(run.cases().len(), 1);
        assert_eq!(run.cases()[0].suite, "src/lib.rs");
        assert!(run.cases()[0].output.iter().any(|l| l.text == "after"), "capture goes on past 'Running foo'");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rerun_failed_drops_duplicates_and_escapes_vitest_names() {
        let mut run = TestRun::default();
        run.feed(&[RunLine::plain("not ok 1 - a.test.ts > f(x) [1] > b.c"), RunLine::plain("not ok 2 - a.test.ts > f(x) [1] > b.c")]);
        let r = rerun_failed(&task(argv(&["vitest"]), &[]), &run).unwrap();
        assert_eq!(r.args, ["a.test.ts", "-t", r"^(?:f\(x\) \[1\] b\.c)$"]);
    }

    #[test]
    fn detect_and_prepare() {
        let d = |c: TaskCommand, a: &[&str]| Runner::detect(&task(c, a));
        assert_eq!(d(argv(&["/usr/bin/cargo", "-q", "t"]), &[]), Some(Runner::Cargo));
        assert_eq!(d(argv(&["cargo"]), &["test"]), Some(Runner::Cargo), "args count as words");
        assert_eq!(d(argv(&["cargo", "build"]), &[]), None);
        assert_eq!(d(argv(&["py.test"]), &[]), Some(Runner::Pytest));
        assert_eq!(d(argv(&["python", "-m", "unittest"]), &[]), None);
        assert_eq!(d(TaskCommand::Shell("npx vitest".into()), &[]), Some(Runner::Vitest));
        assert_eq!(d(argv(&["npx", "jest"]), &[]), Some(Runner::Jest));
        assert_eq!(d(argv(&["node_modules/.bin/jest"]), &[]), Some(Runner::Jest));
        assert_eq!(d(TaskCommand::Shell("yarn jest --ci".into()), &[]), Some(Runner::Jest));
        assert_eq!(d(argv(&["npx", "vitest"]), &[]), Some(Runner::Vitest));
        assert_eq!(d(argv(&["jest.config.js"]), &[]), None);
    }

    #[test]
    fn prepare_adds_missing_flags_once() {
        let v = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(Runner::Pytest.prepare(v(&["pytest", "tests"])), v(&["pytest", "-v", "tests"]));
        assert_eq!(Runner::Pytest.prepare(v(&["python3", "-m", "pytest", "-q"])), v(&["python3", "-m", "pytest", "-v"]));
        assert_eq!(Runner::Pytest.prepare(v(&["pytest", "-qq", "-v"])), v(&["pytest", "-v"]));
        let once = Runner::Pytest.prepare(v(&["pytest"]));
        assert_eq!(Runner::Pytest.prepare(once.clone()), once);
        assert_eq!(Runner::Pytest.prepare(v(&["pytest", "-vv"])), v(&["pytest", "-vv"]));
        assert_eq!(Runner::Pytest.prepare(v(&["pytest", "--verbose"])), v(&["pytest", "--verbose"]));
        assert_eq!(Runner::Vitest.prepare(v(&["vitest", "run"])), v(&["vitest", "run", "--reporter=tap-flat"]));
        assert_eq!(Runner::Vitest.prepare(v(&["vitest", "--reporter=verbose"])), v(&["vitest", "--reporter=verbose"]));
        assert_eq!(Runner::Vitest.prepare(v(&["vitest", "--reporter", "tap-flat"])), v(&["vitest", "--reporter", "tap-flat"]));
        assert_eq!(Runner::Jest.prepare(v(&["npx", "jest"])), v(&["npx", "jest", "--verbose"]));
        let once = Runner::Jest.prepare(v(&["npx", "jest"]));
        assert_eq!(Runner::Jest.prepare(once.clone()), once);
        assert_eq!(Runner::Jest.prepare(v(&["jest", "--verbose=false"])), v(&["jest", "--verbose=false"]));
        assert_eq!(Runner::Cargo.prepare(v(&["cargo", "test"])), v(&["cargo", "test"]));
    }

    const JEST_FAILED_ROWS: [&str; 3] = [
        "src/broken.test.js//Test suite failed to run=Failed",
        "src/math.test.js/math › nested/fails=Failed",
        "src/math.test.js/math/f(x) [1] b.c=Failed",
    ];

    #[test]
    fn jest_verbose() {
        let root = project("jest");
        let run = parse(&root, JEST);
        assert_eq!(
            tree(&run),
            [
                "src/broken.test.js//Test suite failed to run=Failed",
                "src/math.test.js//top level=Passed",
                "src/math.test.js/math › nested/fails=Failed",
                "src/math.test.js/math/adds=Passed",
                "src/math.test.js/math/f(x) [1] b.c=Failed",
                "src/math.test.js/math/later=Ignored",
                "src/math.test.js/math/skipped=Ignored",
                "src/other.test.js//other ok=Passed",
                "src/other.test.js/deep › er/leaf=Passed",
            ]
        );
        assert_eq!(run.counts(), Counts { passed: 4, failed: 3, ignored: 2 });
        let fails = case(&run, "fails");
        assert_eq!(fails.id, "math › nested › fails");
        assert_eq!(fails.output[0].text, "    expect(received).toBe(expected) // Object.is equality");
        assert_eq!(fails.output.last().unwrap().text, "      at Object.toBe (src/math.test.js:7:21)");
        assert_eq!(fails.link().map(at), Some(("math.test.js".into(), 6, 20)));
        let f = case(&run, "f(x) [1] b.c");
        assert_eq!(f.link().map(at), Some(("math.test.js".into(), 13, 16)));
        assert!(f.output.iter().all(|l| l.text.trim() != "hello from test"));
        let broken = case(&run, "Test suite failed to run");
        assert_eq!(broken.id, "");
        assert_eq!(broken.link().map(at), Some(("broken.test.js".into(), 0, 23)));
        assert!(run.cases().iter().filter(|c| c.status != TestStatus::Failed).all(|c| c.output.is_empty()));
        assert_eq!(run.suites(), ["src/math.test.js", "src/broken.test.js", "src/other.test.js"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn jest_console_block_ends_the_failure_before_it() {
        // Hand-written input: a console block printed right after a file's failure
        // sections, before the next file's header.
        let root = project("jest-console");
        let run = parse(
            &root,
            "FAIL src/math.test.js\n  math\n    \u{2715} fails (4 ms)\n\n  \u{25cf} math \u{203a} fails\n\n    expect(received).toBe(expected)\n\n      at Object.toBe (src/math.test.js:7:21)\n\n  console.log\n    hello from test\n\n      at Object.log (src/other.test.js:2:9)\n\nPASS src/other.test.js\n  \u{2713} ok (1 ms)\n",
        );
        let fails = case(&run, "fails");
        assert_eq!(fails.output.last().unwrap().text, "      at Object.toBe (src/math.test.js:7:21)");
        assert!(fails.output.iter().all(|l| !l.text.contains("console.log") && !l.text.contains("hello from test")));
        assert_eq!(case(&run, "ok").status, TestStatus::Passed);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn jest_default_output_has_the_failures() {
        let root = project("jest-default");
        let run = parse(&root, JEST_DEFAULT);
        assert_eq!(tree(&run), JEST_FAILED_ROWS);
        assert!(run.cases().iter().all(|c| c.name != "Console"));
        assert_eq!(case(&run, "fails").link().map(at), Some(("math.test.js".into(), 6, 20)));
        assert!(run.cases().iter().all(|c| c.output.iter().all(|l| l.text.trim() != "hello from test")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn jest_order_does_not_matter() {
        let root = project("jest-order");
        let base = parse(&root, JEST);
        let all = lines(&root, JEST);
        let split = all.iter().position(|l| l.text == "  ● math › nested › fails").unwrap() + 1;
        let mut run = TestRun::default();
        run.feed(&all[..split]);
        run.feed(&all[split..]);
        assert_eq!(tree(&run), tree(&base));
        assert_eq!(case(&run, "fails").output, case(&base, "fails").output);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn jest_colour_header_and_prefixes() {
        let mut run = TestRun::default();
        let input = [" PASS  a.test.js (5.2 s)", "  ✓ ok (3 ms)", "  ○ skipped s", "  ✎ todo t", "  d", "    ✕ f"];
        run.feed(&input.map(RunLine::plain));
        assert_eq!(run.suites(), ["a.test.js"]);
        assert_eq!(case(&run, "ok").status, TestStatus::Passed);
        assert_eq!(case(&run, "s").status, TestStatus::Ignored);
        assert_eq!(case(&run, "t").status, TestStatus::Ignored);
        let f = case(&run, "f");
        assert_eq!((f.status, f.group.as_str(), f.id.as_str()), (TestStatus::Failed, "d", "d › f"));
    }
}
