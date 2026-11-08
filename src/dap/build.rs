//! The build step of a Debug start: its output as Console lines, and the executables it names.
//!
//! A build may print Cargo's `--message-format=json` (one JSON object per line on stdout, text on stderr
//! sharing the same pipe) or anything else. [`BuildFeed`] turns the bytes into [`BuildItem`]s: a JSON
//! `compiler-artifact` with an `executable` is an [`Artifact`], a `compiler-message` shows its `rendered`
//! text, the other Cargo reasons known to be noise show nothing, and everything else, JSON or not, shows as
//! written. [`drive_build`] runs that over a spawned process and reports to the App.
//!
//! Debug Test at Caret asks `drive_build` for one more step: [`find_tests`] narrows the test-profile
//! executables to the ones that hold the test, listing each with libtest's `--list --format terse` when
//! two or more candidates remain.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::session::SessionId;
use crate::event::AppMsg;
use crate::run::caret::RustTest;
use crate::run::matcher::{Matcher, RunLine};
use crate::run::process::{LineSplitter, RunEnd, Spawned, Spec, clean, kill_group, pump, spawn};

/// A line longer than this with no newline stops being collected for parsing and is shown as text.
const MAX_JSON_LINE: usize = 4 * 1024 * 1024;
/// More executables than this are not collected.
const MAX_ARTIFACTS: usize = 1000;
/// How long one test executable gets to list its tests.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// An executable a build made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    pub name: String,
    /// The target's first kind (`bin`, `example`, `test`, ...).
    pub kind: String,
    /// Built with the test profile (`cargo test --no-run`).
    pub test: bool,
}

impl Artifact {
    /// `name (kind) path`, or `name (kind test) path`; the path is relative to `root` when under it.
    pub fn label(&self, root: &Path) -> String {
        let path = self.path.strip_prefix(root).unwrap_or(&self.path);
        let test = if self.test { " test" } else { "" };
        clean(format!("{} ({}{}) {}", self.name, self.kind, test, path.display()).as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildItem {
    Line(RunLine),
    Artifact(Artifact),
}

/// Bytes of a build's output to items; see the module doc.
pub(crate) struct BuildFeed {
    /// The line being collected, for a JSON parse.
    partial: Vec<u8>,
    /// The current line outgrew `MAX_JSON_LINE`: the rest of it is text.
    overlong: bool,
    split: LineSplitter,
    matcher: Matcher,
    cwd: PathBuf,
}

impl BuildFeed {
    pub(crate) fn new(cwd: &Path, root: &Path) -> Self {
        Self {
            partial: Vec::new(),
            overlong: false,
            split: LineSplitter::default(),
            matcher: Matcher::new(cwd, root),
            cwd: cwd.to_path_buf(),
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<BuildItem> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                self.end_line(&mut out);
                continue;
            }
            if self.overlong {
                self.text(&[b], &mut out);
                continue;
            }
            self.partial.push(b);
            if self.partial.len() >= MAX_JSON_LINE {
                self.overlong = true;
                let part = std::mem::take(&mut self.partial);
                self.text(&part, &mut out);
            }
        }
        out
    }

    pub(crate) fn finish(&mut self) -> Vec<BuildItem> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            self.end_line(&mut out);
        }
        let last = self.split.finish();
        out.extend(last.map(|l| BuildItem::Line(self.matcher.line(l))));
        out
    }

    /// A newline arrived.
    fn end_line(&mut self, out: &mut Vec<BuildItem>) {
        let raw = std::mem::take(&mut self.partial);
        if std::mem::take(&mut self.overlong) {
            self.text(b"\n", out);
        } else {
            self.line(&raw, out);
        }
    }

    fn line(&mut self, raw: &[u8], out: &mut Vec<BuildItem>) {
        let is_object = raw.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
        if is_object && let Ok(v) = serde_json::from_slice::<Value>(raw) {
            match v["reason"].as_str() {
                Some("compiler-artifact") => {
                    out.extend(self.artifact(&v).map(BuildItem::Artifact));
                    return;
                }
                Some("compiler-message") => {
                    if let Some(text) = v["message"]["rendered"].as_str() {
                        self.rendered(text, out);
                    }
                    return;
                }
                Some("build-script-executed" | "build-finished") => return,
                _ => {}
            }
        }
        self.text(raw, out);
        self.text(b"\n", out);
    }

    /// `bytes` as output text, through the splitter and the matcher.
    fn text(&mut self, bytes: &[u8], out: &mut Vec<BuildItem>) {
        let lines = self.split.push(bytes);
        out.extend(lines.into_iter().map(|l| BuildItem::Line(self.matcher.line(l))));
    }

    fn rendered(&mut self, text: &str, out: &mut Vec<BuildItem>) {
        let mut split = LineSplitter::default();
        let mut lines = split.push(text.as_bytes());
        lines.extend(split.finish());
        out.extend(lines.into_iter().map(|l| BuildItem::Line(self.matcher.line(l))));
    }

    fn artifact(&self, v: &Value) -> Option<Artifact> {
        let exe = v["executable"].as_str()?;
        let path = self.cwd.join(exe);
        let name = match v["target"]["name"].as_str() {
            Some(n) => n.to_string(),
            None => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        let kind = v["target"]["kind"][0].as_str().unwrap_or("bin").to_string();
        Some(Artifact { path, name, kind, test: v["profile"]["test"].as_bool() == Some(true) })
    }
}

/// By path, with a repeated path once.
pub(crate) fn sorted_artifacts(mut v: Vec<Artifact>) -> Vec<Artifact> {
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v.dedup_by(|a, b| a.path == b.path);
    v
}

/// The executables that can hold the test `t`: the test profile's, and for a file under `src/` (no target flag
/// given) only the library's and binaries', not integration tests, examples or benches. Order kept.
pub(crate) fn test_candidates(artifacts: Vec<Artifact>, t: &RustTest) -> Vec<Artifact> {
    artifacts.into_iter().filter(|a| a.test && !(t.in_src && matches!(a.kind.as_str(), "test" | "example" | "bench"))).collect()
}

/// Whether a line of libtest's `--list --format terse` names the test: the whole name for an exact filter,
/// else a substring, as libtest's own filter matches.
pub(crate) fn lists_test(line: &str, t: &RustTest) -> bool {
    line.strip_suffix(": test").is_some_and(|name| if t.exact { name == t.filter } else { name.contains(&t.filter) })
}

/// The candidates of `artifacts` that hold the test `t`. With no candidate nothing is run.
/// Otherwise `note` gets one line and each candidate runs `--list --format terse` in `cwd`, at most `timeout`
/// (then its process group is killed and it counts as not listing). `pid` is the build's shared handle: when
/// it has no other holder, the `BuildGroup` that stops the build is gone (Stop, a new start, a root switch,
/// quit), so no more lists start and the result is empty.
pub(crate) async fn find_tests(
    artifacts: Vec<Artifact>,
    t: &RustTest,
    cwd: &Path,
    pid: &Arc<AtomicU32>,
    timeout: Duration,
    mut note: impl FnMut(String),
) -> Vec<Artifact> {
    let candidates = test_candidates(artifacts, t);
    if candidates.is_empty() {
        return candidates;
    }
    let noun = if candidates.len() == 1 { "test executable" } else { "test executables" };
    note(format!("Looking for {} in {} {noun}", t.filter, candidates.len()));
    let mut keep = Vec::new();
    for a in candidates {
        if Arc::strong_count(pid) == 1 {
            return Vec::new();
        }
        let argv = vec![a.path.to_string_lossy().into_owned(), "--list".into(), "--format".into(), "terse".into()];
        let task = match spawn(&Spec { argv, cwd: cwd.to_path_buf(), env: Vec::new() }) {
            Ok(task) => task,
            Err(e) => {
                tracing::debug!("listing the tests of {}: {e}", a.path.display());
                continue;
            }
        };
        pid.store(task.pid, Ordering::SeqCst);
        let found = AtomicBool::new(false);
        let mut split = LineSplitter::default();
        let want = t.clone();
        let list = pump(
            task,
            move |bytes| {
                let lines = match bytes {
                    Some(b) => split.push(b),
                    None => split.finish().into_iter().collect(),
                };
                if lines.iter().any(|l| lists_test(l, &want)) { vec![true] } else { Vec::new() }
            },
            |hit: Vec<bool>| {
                if !hit.is_empty() {
                    found.store(true, Ordering::SeqCst);
                }
            },
            pid.clone(),
        );
        tokio::pin!(list);
        tokio::select! {
            _ = &mut list => {}
            _ = tokio::time::sleep(timeout) => {
                tracing::warn!("{} did not list its tests in {timeout:?}", a.path.display());
                kill_group(pid.load(Ordering::SeqCst));
                list.await;
                continue;
            }
        };
        if found.load(Ordering::SeqCst) {
            keep.push(a);
        }
    }
    keep
}

/// What a build's messages carry back.
pub(crate) struct BuildTags {
    pub generation: u64,
    pub seq: u64,
    pub session: SessionId,
}

/// Run `task` to its end: its lines go out as `AppMsg::DebugOutput` for `tags.session`, then one
/// `AppMsg::DebugBuilt` with how it ended and its executables, sorted. `pid` is cleared when it is reaped.
/// With a `test` (Debug Test at Caret) and a build that succeeded, the executables are narrowed by
/// [`find_tests`] first.
pub(crate) async fn drive_build(
    task: Spawned,
    cwd: PathBuf,
    root: PathBuf,
    tags: BuildTags,
    tx: UnboundedSender<AppMsg>,
    pid: Arc<AtomicU32>,
    test: Option<RustTest>,
) {
    let mut feed = BuildFeed::new(&cwd, &root);
    let mut artifacts = Vec::new();
    let end = pump(
        task,
        move |bytes| match bytes {
            Some(b) => feed.push(b),
            None => feed.finish(),
        },
        |items| {
            let mut lines = Vec::new();
            for item in items {
                match item {
                    BuildItem::Line(l) => lines.push(l),
                    BuildItem::Artifact(a) if artifacts.len() < MAX_ARTIFACTS => artifacts.push(a),
                    BuildItem::Artifact(_) => {}
                }
            }
            if !lines.is_empty() {
                let _ = tx.send(AppMsg::DebugOutput { generation: tags.generation, session: tags.session, lines });
            }
        },
        pid.clone(),
    )
    .await;
    let mut artifacts = sorted_artifacts(artifacts);
    if end == RunEnd::Code(0)
        && let Some(t) = &test
    {
        artifacts = find_tests(artifacts, t, &cwd, &pid, LIST_TIMEOUT, |text| {
            let _ = tx.send(AppMsg::DebugOutput { generation: tags.generation, session: tags.session, lines: vec![RunLine::plain(text)] });
        })
        .await;
    }
    let _ = tx.send(AppMsg::DebugBuilt { generation: tags.generation, seq: tags.seq, end, artifacts });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::matcher::Severity;

    const LIB: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///tmp/jetl-d3b-probe/demo#0.1.0","manifest_path":"/tmp/jetl-d3b-probe/demo/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"demo_lib","src_path":"/tmp/jetl-d3b-probe/demo/src/lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/tmp/jetl-d3b-probe/target/debug/libdemo_lib.rlib","/tmp/jetl-d3b-probe/target/debug/deps/libdemo_lib-86a6942af6bb7240.rmeta"],"executable":null,"fresh":false}"#;
    const BIN: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///tmp/jetl-d3b-probe/demo#0.1.0","manifest_path":"/tmp/jetl-d3b-probe/demo/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"demo","src_path":"/tmp/jetl-d3b-probe/demo/src/main.rs","edition":"2024","doc":true,"doctest":false,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/tmp/jetl-d3b-probe/target/debug/demo"],"executable":"/tmp/jetl-d3b-probe/target/debug/demo","fresh":false}"#;
    const TEST: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///tmp/jetl-d3b-probe/demo#0.1.0","manifest_path":"/tmp/jetl-d3b-probe/demo/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"demo_lib","src_path":"/tmp/jetl-d3b-probe/demo/src/lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":true},"features":[],"filenames":["/tmp/jetl-d3b-probe/target/debug/deps/demo_lib-94f2f90e8d7eaf43"],"executable":"/tmp/jetl-d3b-probe/target/debug/deps/demo_lib-94f2f90e8d7eaf43","fresh":true}"#;
    const CUSTOM: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///tmp/jetl-d3b-probe/bs#0.1.0","manifest_path":"/tmp/jetl-d3b-probe/bs/Cargo.toml","target":{"kind":["custom-build"],"crate_types":["bin"],"name":"build-script-build","src_path":"/tmp/jetl-d3b-probe/bs/build.rs","edition":"2024","doc":false,"doctest":false,"test":false},"profile":{"opt_level":"0","debuginfo":0,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/tmp/jetl-d3b-probe/t2/debug/build/bs-ed6c979e5f5679d1/build-script-build"],"executable":null,"fresh":false}"#;
    const SCRIPT: &str = r#"{"reason":"build-script-executed","package_id":"path+file:///tmp/jetl-d3b-probe/bs#0.1.0","linked_libs":[],"linked_paths":[],"cfgs":[],"env":[],"out_dir":"/tmp/jetl-d3b-probe/t2/debug/build/bs-23032b385df74c58/out"}"#;
    const FINISHED: &str = r#"{"reason":"build-finished","success":true}"#;
    const MESSAGE: &str = r#"{"reason":"compiler-message","package_id":"path+file:///tmp/jetl-d3b-probe/demo#0.1.0","manifest_path":"/tmp/jetl-d3b-probe/demo/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"other","src_path":"/tmp/jetl-d3b-probe/demo/src/bin/other.rs","edition":"2024","doc":true,"doctest":false,"test":true},"message":{"rendered":"error[E0308]: mismatched types\n --> src/bin/other.rs:1:25\n  |\n1 | fn main(){ let x: i32 = \"a\"; }\n  |                   ---   ^^^ expected `i32`, found `&str`\n  |                   |\n  |                   expected due to this\n\n","$message_type":"diagnostic","children":[],"level":"error","message":"mismatched types","spans":[],"code":{"code":"E0308"}}}"#;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-test-build-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn feed(cwd: &str) -> BuildFeed {
        BuildFeed::new(Path::new(cwd), Path::new(cwd))
    }

    fn all(input: &str, cwd: &str) -> Vec<BuildItem> {
        let mut f = feed(cwd);
        let mut v = f.push(input.as_bytes());
        v.extend(f.finish());
        v
    }

    fn texts(items: &[BuildItem]) -> Vec<&str> {
        items.iter().filter_map(|i| if let BuildItem::Line(l) = i { Some(l.text.as_str()) } else { None }).collect()
    }

    fn artifacts(items: &[BuildItem]) -> Vec<&Artifact> {
        items.iter().filter_map(|i| if let BuildItem::Artifact(a) = i { Some(a) } else { None }).collect()
    }

    #[test]
    fn artifact_lines_give_their_executable() {
        let items = all(&format!("{LIB}\n{BIN}\n{TEST}\n{CUSTOM}\n"), "/tmp/jetl-d3b-probe");
        let a = artifacts(&items);
        assert_eq!(a.len(), 2, "{items:?}");
        assert_eq!(
            a[0],
            &Artifact { path: "/tmp/jetl-d3b-probe/target/debug/demo".into(), name: "demo".into(), kind: "bin".into(), test: false }
        );
        assert_eq!(a[1].path, Path::new("/tmp/jetl-d3b-probe/target/debug/deps/demo_lib-94f2f90e8d7eaf43"));
        assert_eq!((a[1].name.as_str(), a[1].kind.as_str(), a[1].test), ("demo_lib", "lib", true));
        assert!(texts(&items).is_empty());
    }

    #[test]
    fn a_cargo_stream_shows_only_rendered_and_text() {
        let input = format!("   Compiling demo v0.1.0 (/tmp/x)\n{LIB}\n{BIN}\n{SCRIPT}\n{FINISHED}\n    Finished `dev` profile\n");
        let items = all(&input, "/tmp/jetl-d3b-probe");
        assert_eq!(texts(&items), ["   Compiling demo v0.1.0 (/tmp/x)", "    Finished `dev` profile"]);
        assert!(texts(&items).iter().all(|t| !t.contains("\"reason\"")));
    }

    #[test]
    fn a_compiler_message_is_cleaned_and_linked() {
        let root = scratch("link");
        std::fs::create_dir_all(root.join("src/bin")).unwrap();
        std::fs::write(root.join("src/bin/other.rs"), "fn main(){}\n").unwrap();
        let items = all(&format!("{MESSAGE}\n"), root.to_str().unwrap());
        let lines: Vec<&RunLine> = items.iter().filter_map(|i| if let BuildItem::Line(l) = i { Some(l) } else { None }).collect();
        assert_eq!(lines[0].text, "error[E0308]: mismatched types");
        assert_eq!(lines[0].severity, Some(Severity::Error));
        assert_eq!(lines[1].text, " --> src/bin/other.rs:1:25");
        assert_eq!(lines[1].severity, Some(Severity::Error));
        let link = lines[1].link.as_ref().expect("the arrow line links");
        assert_eq!((link.path.as_path(), link.line, link.col), (root.join("src/bin/other.rs").as_path(), 0, 24));
        assert_eq!(lines.last().unwrap().text, "", "the blank line ending the diagnostic stays");
        assert_eq!(lines.len(), 8);
        // Colour codes in `rendered` are cleaned.
        let coloured = r#"{"reason":"compiler-message","message":{"rendered":"\u001b[31merror\u001b[0m: boom\n"}}"#;
        assert_eq!(texts(&all(&format!("{coloured}\n"), "/tmp")), ["error: boom"]);
        // A message with no `rendered` shows nothing.
        assert!(all("{\"reason\":\"compiler-message\",\"message\":{}}\n", "/tmp").is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn other_json_and_other_tools_stay_as_written() {
        let mut f = feed("/tmp");
        let mut items = f.push(b"{\"a\":1}\n{\"reason\":\"future\"}\n{broken\nplain\ncaf\xc3\xa9 \xff\n");
        items.extend(f.finish());
        assert_eq!(texts(&items), ["{\"a\":1}", "{\"reason\":\"future\"}", "{broken", "plain", "café \u{fffd}"]);
    }

    #[test]
    fn a_message_longer_than_the_run_cut_still_parses() {
        let rendered = "x".repeat(20 * 1024);
        let line = format!(r#"{{"reason":"compiler-message","message":{{"rendered":"{rendered}\n"}}}}"#);
        assert!(line.len() > 16 * 1024);
        let items = all(&format!("{line}\n"), "/tmp");
        let t = texts(&items);
        assert_eq!(t.concat(), rendered);
        assert!(t.iter().all(|l| !l.contains("reason")));
    }

    #[test]
    fn a_line_without_end_is_bounded() {
        let mut f = feed("/tmp");
        let mut items = Vec::new();
        let chunk = vec![b'y'; 64 * 1024];
        for _ in 0..80 {
            items.extend(f.push(&chunk));
            assert!(f.partial.len() < MAX_JSON_LINE);
        }
        items.extend(f.finish());
        let t = texts(&items);
        assert!(t.iter().all(|l| l.len() <= 16 * 1024), "{}", t.iter().map(|l| l.len()).max().unwrap());
        assert_eq!(t.iter().map(|l| l.len()).sum::<usize>(), 80 * 64 * 1024);
        // A JSON-looking line that outgrows the cap is text, not an artifact.
        let mut g = feed("/tmp");
        let mut out = g.push(b"{\"reason\":\"compiler-artifact\",\"executable\":\"/x\",\"pad\":\"");
        for _ in 0..80 {
            out.extend(g.push(&chunk));
        }
        out.extend(g.push(b"\"}\n"));
        out.extend(g.finish());
        assert!(artifacts(&out).is_empty());
        // The next line is classified again.
        assert!(g.push(format!("{BIN}\n").as_bytes()).iter().any(|i| matches!(i, BuildItem::Artifact(_))));
    }

    #[test]
    fn chunks_split_anywhere_give_the_same_items() {
        let input = format!("   Compiling demo\n{BIN}\n{MESSAGE}\n{FINISHED}\n    Finished");
        let whole = all(&input, "/tmp/jetl-d3b-probe");
        assert!(!artifacts(&whole).is_empty() && texts(&whole).len() > 3);
        let mut f = feed("/tmp/jetl-d3b-probe");
        let mut bytes = Vec::new();
        for b in input.bytes() {
            bytes.extend(f.push(&[b]));
        }
        bytes.extend(f.finish());
        assert_eq!(bytes, whole);
        // The end of the output with no newline still parses.
        let mut g = feed("/tmp/jetl-d3b-probe");
        let mut v = g.push(BIN.as_bytes());
        assert!(v.is_empty());
        v.extend(g.finish());
        assert_eq!(artifacts(&v).len(), 1);
    }

    #[test]
    fn relative_executable_joins_the_cwd() {
        let line =
            r#"{"reason":"compiler-artifact","target":{"name":"r","kind":["bin"]},"profile":{"test":false},"executable":"target/debug/r"}"#;
        let items = all(&format!("{line}\n"), "/w/proj");
        assert_eq!(artifacts(&items)[0].path, Path::new("/w/proj/target/debug/r"));
        // No target name: the file name; no kind: bin.
        let bare = r#"{"reason":"compiler-artifact","executable":"/a/b/prog"}"#;
        let items = all(&format!("{bare}\n"), "/w");
        assert_eq!((artifacts(&items)[0].name.as_str(), artifacts(&items)[0].kind.as_str()), ("prog", "bin"));
    }

    fn art(path: &str) -> Artifact {
        Artifact { path: path.into(), name: "n".into(), kind: "bin".into(), test: false }
    }

    #[test]
    fn sorted_artifacts_by_path_without_duplicates() {
        let v = sorted_artifacts(vec![art("/b"), art("/a"), art("/b"), art("/c")]);
        assert_eq!(v.iter().map(|a| a.path.to_str().unwrap()).collect::<Vec<_>>(), ["/a", "/b", "/c"]);
    }

    #[test]
    fn label_is_relative_and_marks_a_test() {
        let a = Artifact { path: "/w/target/debug/demo".into(), name: "demo".into(), kind: "bin".into(), test: false };
        assert_eq!(a.label(Path::new("/w")), "demo (bin) target/debug/demo");
        let t = Artifact { test: true, kind: "lib".into(), ..a.clone() };
        assert_eq!(t.label(Path::new("/w")), "demo (lib test) target/debug/demo");
        assert_eq!(a.label(Path::new("/other")), "demo (bin) /w/target/debug/demo");
        let evil = Artifact { name: "a\x1b[31mb\nc".into(), ..a };
        assert!(!evil.label(Path::new("/w")).contains(['\x1b', '\n']));
    }

    fn rust_test(filter: &str, exact: bool, in_src: bool) -> RustTest {
        RustTest { dir: "/w".into(), target: Vec::new(), filter: filter.into(), exact, in_src }
    }

    fn kind(path: &str, kind: &str, test: bool) -> Artifact {
        Artifact { path: path.into(), name: "probe".into(), kind: kind.into(), test }
    }

    #[test]
    fn test_candidates_keep_the_test_profile_and_the_target_kinds() {
        // The four executables a build with no target flag names (design probe, build-all.json).
        let all =
            vec![kind("/t/bin", "bin", false), kind("/t/lib", "lib", true), kind("/t/it", "test", true), kind("/t/bint", "bin", true)];
        let paths = |v: Vec<Artifact>| v.into_iter().map(|a| a.path.to_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(paths(test_candidates(all.clone(), &rust_test("x", true, true))), ["/t/lib", "/t/bint"]);
        assert_eq!(paths(test_candidates(all.clone(), &rust_test("x", true, false))), ["/t/lib", "/t/it", "/t/bint"]);
        let more = vec![kind("/t/ex", "example", true), kind("/t/be", "bench", true), kind("/t/lib", "lib", true)];
        assert_eq!(paths(test_candidates(more, &rust_test("x", true, true))), ["/t/lib"]);
    }

    #[test]
    fn lists_test_matches_libtests_terse_lines() {
        let exact = rust_test("util::tests::twice_works", true, true);
        assert!(lists_test("util::tests::twice_works: test", &exact));
        assert!(!lists_test("util::tests::twice_works_too: test", &exact));
        let sub = rust_test("util::tests::twice_works", false, true);
        assert!(lists_test("util::tests::twice_works_too: test", &sub));
        assert!(!lists_test("a::b: bench", &rust_test("a::b", true, true)));
        assert!(!lists_test("util::tests::twice_works", &exact), "no suffix");
        let module = rust_test("util::tests::", false, true);
        assert!(lists_test("util::tests::twice_works: test", &module) && lists_test("util::tests::twice_works_too: test", &module));
        assert!(!lists_test("tests::main_test: test", &module));
    }

    /// An executable script that prints `body` when asked to `--list`.
    fn script(dir: &Path, name: &str, body: &str) -> Artifact {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Artifact { path, name: name.into(), kind: "lib".into(), test: true }
    }

    #[tokio::test]
    async fn find_tests_lists_a_single_candidate_too() {
        let dir = scratch("single");
        let pid = Arc::new(AtomicU32::new(0));
        let _hold = pid.clone();
        let t = rust_test("a::b", true, true);

        // One test candidate that lists the test: kept, after a singular note.
        let yes = script(&dir, "yes", "[ \"$1\" = --list ] && echo 'a::b: test'");
        let mut notes = Vec::new();
        let got = find_tests(vec![yes.clone(), art("/other")], &t, &dir, &pid, LIST_TIMEOUT, |n| notes.push(n)).await;
        assert_eq!(got, vec![yes]);
        assert_eq!(notes, ["Looking for a::b in 1 test executable"]);

        // One candidate that lists only another test: dropped, the note is still there.
        let no = script(&dir, "no", "[ \"$1\" = --list ] && echo 'other: test'");
        let mut notes = Vec::new();
        let got = find_tests(vec![no, art("/other")], &t, &dir, &pid, LIST_TIMEOUT, |n| notes.push(n)).await;
        assert!(got.is_empty());
        assert_eq!(notes, ["Looking for a::b in 1 test executable"]);

        // No test candidate: nothing is run and there is no note.
        let mut notes = Vec::new();
        let got = find_tests(vec![art("/other")], &t, &dir, &pid, LIST_TIMEOUT, |n| notes.push(n)).await;
        assert!(got.is_empty());
        assert!(notes.is_empty(), "{notes:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn find_tests_keeps_the_executables_that_list_the_test() {
        let dir = scratch("list");
        let yes = script(&dir, "yes", "[ \"$1\" = --list ] && echo 'a::b: test'");
        let no = script(&dir, "no", "[ \"$1\" = --list ] && echo 'other: test'");
        let pid = Arc::new(AtomicU32::new(0));
        let _hold = pid.clone();
        let mut notes = Vec::new();
        let got = find_tests(vec![yes.clone(), no], &rust_test("a::b", true, true), &dir, &pid, LIST_TIMEOUT, |n| notes.push(n)).await;
        assert_eq!(got, vec![yes]);
        assert_eq!(notes, ["Looking for a::b in 2 test executables"]);
        assert_eq!(pid.load(Ordering::SeqCst), 0, "reaped");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_list_that_hangs_is_killed_and_dropped() {
        let dir = scratch("hang");
        let pid_file = dir.join("sleeper.pid");
        let hang = script(&dir, "hang", &format!("echo $$ > {}\nexec sleep 30", pid_file.display()));
        let fast = script(&dir, "fast", "echo 'a::b: test'");
        let pid = Arc::new(AtomicU32::new(0));
        let _hold = pid.clone();
        let got =
            find_tests(vec![fast.clone(), hang], &rust_test("a::b", true, true), &dir, &pid, Duration::from_millis(200), |_| {}).await;
        assert_eq!(got, vec![fast]);
        let sleeper: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let alive = |p: u32| {
            std::fs::read_to_string(format!("/proc/{p}/stat"))
                .is_ok_and(|s| s.rsplit(')').next().is_some_and(|r| !r.trim_start().starts_with('Z')))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(sleeper) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(sleeper), "the hung list was killed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn find_tests_stops_when_the_start_is_gone() {
        let dir = scratch("gone");
        let marker = dir.join("ran");
        let a = script(&dir, "a", &format!("touch {}\necho 'a::b: test'", marker.display()));
        let b = script(&dir, "b", &format!("touch {}\necho 'a::b: test'", marker.display()));
        let pid = Arc::new(AtomicU32::new(0));
        let got = find_tests(vec![a, b], &rust_test("a::b", true, true), &dir, &pid, LIST_TIMEOUT, |_| {}).await;
        assert!(got.is_empty());
        assert!(!marker.exists(), "no list ran");
        let _ = std::fs::remove_dir_all(dir);
    }
}
