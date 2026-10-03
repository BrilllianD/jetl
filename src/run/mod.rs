//! Run tasks: `[[run.tasks]]` in config.toml, `[[tasks]]` in the project's
//! `.jetl/run.toml`, or defaults detected from the project files when
//! neither has any. A task runs in its own process group with stdout and
//! stderr merged (`process`); its output is matched for file locations
//! (`matcher`) and shown in the Run tool window (`panel`).

pub mod matcher;
pub mod panel;
pub mod process;

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// `[run]` in config.toml, and the whole of `.jetl/run.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RunConfig {
    pub tasks: Vec<RunTask>,
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
    #[serde(skip)]
    pub origin: Origin,
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
            TaskCommand::Argv(v) => v
                .iter()
                .map(|w| {
                    let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:+,@%".contains(c));
                    if plain { w.clone() } else { format!("'{}'", w.replace('\'', r"'\''")) }
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

impl RunTask {
    fn detected(name: &str, argv: &[&str]) -> Self {
        Self {
            name: name.into(),
            command: TaskCommand::Argv(argv.iter().map(|s| s.to_string()).collect()),
            cwd: None,
            env: BTreeMap::new(),
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
            Ok(c) => tasks.extend(c.tasks.into_iter().map(|t| RunTask { origin: Origin::Project, ..t })),
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
        assert_eq!((tasks[0].origin, tasks[1].origin), (Origin::Config, Origin::Project));
        std::fs::write(dir.join(".jetl/run.toml"), "[[tasks]]\nname = 3\n").unwrap();
        let (tasks, error) = load_tasks(&dir, &configured);
        assert_eq!(names(&tasks), ["b"]);
        assert!(error.unwrap().starts_with(".jetl/run.toml: "));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
