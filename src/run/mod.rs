//! Run tasks: `[[run.tasks]]` in config.toml, `[[tasks]]` in the project's
//! `.jetl/run.toml`, or defaults detected from the project files when
//! neither has any. A task runs in its own process group with stdout and
//! stderr merged (`process`); its output is matched for file locations
//! (`matcher`) and shown in the Run tool window (`panel`).

pub mod caret;
pub mod edit;
pub mod matcher;
pub mod panel;
pub mod process;
pub mod testing;

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// `[run]` in config.toml, and the whole of `.jetl/run.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RunConfig {
    pub tasks: Vec<RunTask>,
    /// `[[run.configs]]` / `[[configs]]`: the documented name; `tasks` is the older one. Both are read.
    pub configs: Vec<RunTask>,
    /// `[run.debug.<name>]` / `[debug.<name>]`: how a configuration of that name is debugged.
    pub debug: BTreeMap<String, crate::dap::config::DebugBlock>,
}

impl RunConfig {
    /// `tasks` then `configs`.
    pub fn all(&self) -> Vec<RunTask> {
        self.tasks.iter().chain(&self.configs).cloned().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunTask {
    pub name: String,
    pub command: TaskCommand,
    /// Relative to the project root; the root when unset.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Added to jetl's environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Appended to the command's words (to a string command as `"$@"`).
    #[serde(default)]
    pub args: Vec<String>,
    /// How the output is read; unset picks one from the command.
    #[serde(default)]
    pub matcher: Option<MatcherKind>,
    #[serde(skip)]
    pub origin: Origin,
}

/// How a run's output is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatcherKind {
    /// File locations become links (`matcher.rs`).
    Problems,
    /// Test results as a tree (`testing.rs`), file locations linked too.
    Tests,
    /// Plain text, no links.
    #[serde(rename = "none")]
    Plain,
}

impl MatcherKind {
    /// Every kind, in the order the Edit Configurations dialog lists them.
    pub const ALL: &'static [MatcherKind] = &[MatcherKind::Problems, MatcherKind::Tests, MatcherKind::Plain];

    /// The TOML spelling: "problems", "tests", "none".
    pub fn label(self) -> &'static str {
        match self {
            MatcherKind::Problems => "problems",
            MatcherKind::Tests => "tests",
            MatcherKind::Plain => "none",
        }
    }
}

/// A string runs through `sh -c`; a list of words runs directly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum TaskCommand {
    Shell(String),
    Argv(Vec<String>),
}

/// Where a task comes from, shown in the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Origin {
    #[default]
    Config,
    Project,
    Detected,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Config => "config.toml",
            Origin::Project => ".jetl/run.toml",
            Origin::Detected => "detected",
        }
    }
}

impl TaskCommand {
    pub fn argv(&self) -> Vec<String> {
        match self {
            TaskCommand::Shell(s) => vec!["sh".into(), "-c".into(), s.clone()],
            TaskCommand::Argv(v) => v.clone(),
        }
    }

    /// The command as typed in a shell.
    pub fn display(&self) -> String {
        match self {
            TaskCommand::Shell(s) => s.clone(),
            TaskCommand::Argv(v) => v.iter().map(|w| quote_word(w)).collect::<Vec<_>>().join(" "),
        }
    }
}

/// `w` as one shell word: bare when it is plain, else single-quoted.
pub fn quote_word(w: &str) -> String {
    let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:+,@%".contains(c));
    if plain { w.to_string() } else { format!("'{}'", w.replace('\'', r"'\''")) }
}

/// Split `s` into words like a POSIX shell: whitespace separates, `'..'` is
/// literal, `".."` honours `\"` and `\\`, a backslash outside quotes escapes
/// the next character, `''` and `""` give an empty word.
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("unclosed quote".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\')) => cur.push(e),
                            Some(e) => {
                                cur.push('\\');
                                cur.push(e);
                            }
                            None => return Err("unclosed quote".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("unclosed quote".into()),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(e) = chars.next() {
                    cur.push(e);
                }
            }
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        words.push(cur);
    }
    Ok(words)
}

impl RunTask {
    /// The process to start: the command's words, then `args`. A string
    /// command gets them as positional parameters (`"$@"`), unsplit.
    pub fn argv(&self) -> Vec<String> {
        match &self.command {
            TaskCommand::Argv(v) => v.iter().chain(&self.args).cloned().collect(),
            TaskCommand::Shell(s) if self.args.is_empty() => vec!["sh".into(), "-c".into(), s.clone()],
            TaskCommand::Shell(s) => {
                let mut v = vec!["sh".into(), "-c".into(), format!("{s} \"$@\""), "sh".into()];
                v.extend(self.args.iter().cloned());
                v
            }
        }
    }

    /// The command as typed in a shell.
    pub fn display(&self) -> String {
        let mut s = self.command.display();
        for a in &self.args {
            s.push(' ');
            s.push_str(&quote_word(a));
        }
        s
    }

    /// The explicit matcher, else `Tests` for a cargo test, pytest, vitest or
    /// jest command, else `Problems`.
    pub fn matcher_kind(&self) -> MatcherKind {
        self.matcher.unwrap_or_else(|| if testing::Runner::detect(self).is_some() { MatcherKind::Tests } else { MatcherKind::Problems })
    }

    fn detected(name: &str, argv: &[&str]) -> Self {
        Self {
            name: name.into(),
            command: TaskCommand::Argv(argv.iter().map(|s| s.to_string()).collect()),
            cwd: None,
            env: BTreeMap::new(),
            args: Vec::new(),
            matcher: None,
            origin: Origin::Detected,
        }
    }
}

/// The picker's tasks for `root`: the configured ones, then those of
/// `.jetl/run.toml`; detected defaults when there are none. The error is an
/// unreadable or invalid project file. Reads files, so it runs off the main
/// thread.
pub fn load_tasks(root: &Path, configured: &[RunTask]) -> (Vec<RunTask>, Option<String>) {
    let mut tasks: Vec<RunTask> = configured.iter().cloned().map(|t| RunTask { origin: Origin::Config, ..t }).collect();
    let file = root.join(".jetl").join("run.toml");
    let mut error = None;
    match std::fs::read_to_string(&file) {
        Ok(text) => match toml::from_str::<RunConfig>(&text) {
            Ok(c) => tasks.extend(c.tasks.into_iter().chain(c.configs).map(|t| RunTask { origin: Origin::Project, ..t })),
            Err(e) => error = Some(format!(".jetl/run.toml: {}", crate::config::theme::error_summary(&e.to_string()))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => error = Some(format!(".jetl/run.toml: {e}")),
    }
    if tasks.is_empty() {
        tasks = detect(root);
    }
    (tasks, error)
}

/// Defaults from the project files in `root`: Cargo, npm scripts (at most
/// 20), make, Go.
pub fn detect(root: &Path) -> Vec<RunTask> {
    let mut v = Vec::new();
    if root.join("Cargo.toml").is_file() {
        v.push(RunTask::detected("cargo build", &["cargo", "build"]));
        v.push(RunTask::detected("cargo test", &["cargo", "test"]));
        v.push(RunTask::detected("cargo run", &["cargo", "run"]));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(scripts) = json.get("scripts").and_then(|s| s.as_object())
    {
        for name in scripts.keys().take(20) {
            v.push(RunTask::detected(&format!("npm run {name}"), &["npm", "run", name]));
        }
    }
    if ["Makefile", "makefile", "GNUmakefile"].iter().any(|f| root.join(f).is_file()) {
        v.push(RunTask::detected("make", &["make"]));
    }
    if root.join("go.mod").is_file() {
        v.push(RunTask::detected("go build", &["go", "build", "./..."]));
        v.push(RunTask::detected("go test", &["go", "test", "./..."]));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_strings_or_words() {
        let text = "[[run.tasks]]\nname = \"build\"\ncommand = \"cargo build 2>&1\"\n\
                    [[run.tasks]]\nname = \"web\"\ncommand = [\"npm\", \"run\", \"dev server\"]\ncwd = \"web\"\nenv = { PORT = \"8080\" }\n";
        let cfg = crate::config::Config::parse(text).unwrap().run;
        assert_eq!(cfg.tasks.len(), 2);
        assert_eq!(cfg.tasks[0].command.argv(), ["sh", "-c", "cargo build 2>&1"]);
        assert_eq!(cfg.tasks[0].cwd, None);
        assert_eq!(cfg.tasks[1].command.argv(), ["npm", "run", "dev server"]);
        assert_eq!(cfg.tasks[1].command.display(), "npm run 'dev server'");
        assert_eq!(cfg.tasks[1].cwd.as_deref(), Some("web"));
        assert_eq!(cfg.tasks[1].env["PORT"], "8080");
        assert!(crate::config::Config::parse("[[run.tasks]]\nname = \"x\"\n").is_err(), "a command is required");
        let text = "[[run.tasks]]\nname = \"old\"\ncommand = \"make\"\n\
                    [[run.configs]]\nname = \"t\"\ncommand = [\"cargo\", \"test\"]\nargs = [\"--release\", \"a b\"]\nmatcher = \"none\"\n";
        let cfg = crate::config::Config::parse(text).unwrap().run;
        let all = cfg.all();
        assert_eq!(all.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["old", "t"]);
        assert_eq!(all[1].args, ["--release", "a b"]);
        assert_eq!(all[1].matcher, Some(MatcherKind::Plain));
        assert_eq!(all[1].matcher_kind(), MatcherKind::Plain);
        assert_eq!(all[0].matcher_kind(), MatcherKind::Problems);
        assert_eq!(all[1].display(), "cargo test --release 'a b'");
        assert_eq!(all[1].argv(), ["cargo", "test", "--release", "a b"]);
        assert!(crate::config::Config::parse("[[run.configs]]\nname = \"x\"\ncommand = \"y\"\nmatcher = \"nope\"\n").is_err());
    }

    #[test]
    fn auto_matcher_follows_the_command() {
        let argv = |w: &[&str]| RunTask::detected("t", w);
        let shell = |s: &str| RunTask { command: TaskCommand::Shell(s.into()), ..argv(&[]) };
        for t in [
            argv(&["cargo", "test"]),
            shell("cargo test 2>&1"),
            argv(&["cargo", "+nightly", "test", "--lib"]),
            argv(&["python3", "-m", "pytest", "-q"]),
            argv(&["pytest"]),
            argv(&["npx", "vitest", "run"]),
            argv(&["npx", "jest"]),
            shell("yarn jest --ci"),
        ] {
            assert_eq!(t.matcher_kind(), MatcherKind::Tests, "{}", t.display());
        }
        for t in [argv(&["cargo", "build"]), argv(&["npm", "test"])] {
            assert_eq!(t.matcher_kind(), MatcherKind::Problems, "{}", t.display());
        }
        let explicit = RunTask { matcher: Some(MatcherKind::Problems), ..argv(&["cargo", "test"]) };
        assert_eq!(explicit.matcher_kind(), MatcherKind::Problems);
        let text = "[[run.configs]]\nname = \"t\"\ncommand = \"x\"\nmatcher = \"tests\"\n";
        assert_eq!(crate::config::Config::parse(text).unwrap().run.configs[0].matcher, Some(MatcherKind::Tests));
    }

    #[test]
    fn shell_args_are_positional_parameters() {
        let t = RunTask { args: vec!["a b".into(), "c".into()], ..RunTask::detected("e", &[]) };
        let t = RunTask { command: TaskCommand::Shell("echo".into()), ..t };
        assert_eq!(t.argv(), ["sh", "-c", "echo \"$@\"", "sh", "a b", "c"]);
        let none = RunTask { args: Vec::new(), ..t };
        assert_eq!(none.argv(), ["sh", "-c", "echo"]);
    }

    #[test]
    fn split_words_quotes_and_escapes() {
        assert_eq!(split_words(r#"a 'b c' "d \"e\"" f\ g """#).unwrap(), ["a", "b c", "d \"e\"", "f g", ""]);
        assert_eq!(split_words("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_words("\"x").unwrap_err(), "unclosed quote");
        assert!(split_words("'x").is_err());
    }

    #[test]
    fn configured_tasks_win_over_detected_ones() {
        let dir = std::env::temp_dir().join(format!("jetl-test-run-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_tasks(&dir, &[]).0.is_empty());
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.join("Makefile"), "").unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts": {"test": "jest", "build": "tsc"}}"#).unwrap();
        let names = |t: &[RunTask]| t.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        let (tasks, error) = load_tasks(&dir, &[]);
        assert_eq!(names(&tasks), ["cargo build", "cargo test", "cargo run", "npm run build", "npm run test", "make"]);
        assert!(error.is_none() && tasks.iter().all(|t| t.origin == Origin::Detected));
        std::fs::create_dir_all(dir.join(".jetl")).unwrap();
        std::fs::write(dir.join(".jetl/run.toml"), "[[tasks]]\nname = \"lint\"\ncommand = [\"cargo\", \"clippy\"]\n").unwrap();
        let configured = crate::config::Config::parse("[[run.tasks]]\nname = \"b\"\ncommand = \"make b\"\n").unwrap().run.tasks;
        let (tasks, _) = load_tasks(&dir, &configured);
        assert_eq!(names(&tasks), ["b", "lint"]);
        std::fs::write(
            dir.join(".jetl/run.toml"),
            "[[configs]]\nname = \"c\"\ncommand = \"x\"\n[[tasks]]\nname = \"t\"\ncommand = \"y\"\n",
        )
        .unwrap();
        let (tasks, _) = load_tasks(&dir, &configured);
        assert_eq!(names(&tasks), ["b", "t", "c"]);
        assert!(tasks[1..].iter().all(|t| t.origin == Origin::Project));
        std::fs::write(dir.join(".jetl/run.toml"), "[[tasks]]\nname = \"lint\"\ncommand = [\"cargo\", \"clippy\"]\n").unwrap();
        let (tasks, _) = load_tasks(&dir, &configured);
        assert_eq!((tasks[0].origin, tasks[1].origin), (Origin::Config, Origin::Project));
        std::fs::write(dir.join(".jetl/run.toml"), "[[tasks]]\nname = 3\n").unwrap();
        let (tasks, error) = load_tasks(&dir, &configured);
        assert_eq!(names(&tasks), ["b"]);
        assert!(error.unwrap().starts_with(".jetl/run.toml: "));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_debug_block_is_reported_by_load_tasks() {
        let dir = std::env::temp_dir().join(format!("jetl-test-run-bad-debug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".jetl")).unwrap();
        std::fs::write(dir.join(".jetl/run.toml"), "[[configs]]\nname = \"c\"\ncommand = \"x\"\n[debug.a]\nlaunch = \"s\"\n").unwrap();
        let (_, error) = load_tasks(&dir, &[]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.is_some_and(|e| e.contains(".jetl/run.toml") && e.contains("launch must be a table")));
    }
}
