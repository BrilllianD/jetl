//! The `yman` command line for reads and writes of the project's task
//! tracker. Never prompts: no controlling terminal, stdin closed unless input
//! is given, `EDITOR`/`VISUAL`/`GIT_EDITOR` set to `true`,
//! `GIT_TERMINAL_PROMPT=0`; killed after [`TIMEOUT`].

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

pub const PROGRAM: &str = "yman";
pub const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct YmanOutput {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YmanError {
    NoBinary,
    NotGitRepo,
    NotInitialized,
    Usage(String),
    MergeUnresolved(String),
    NoSuchTask(String),
    Failed(String),
    Timeout(String),
    Io(String),
    BadOutput(String),
}

impl YmanError {
    /// The error for a non-zero exit: yman's exit codes are 1 error, 2 usage,
    /// 3 sync merge unresolved, 4 no such task; errors are `error: ...` lines
    /// on stderr.
    pub fn from_exit(code: Option<i32>, stderr: &str) -> YmanError {
        let lines = || stderr.lines().map(str::trim).filter(|l| !l.is_empty());
        let line = lines()
            .find_map(|l| l.strip_prefix("error:").map(|r| r.trim_start().to_string()))
            .or_else(|| lines().next().map(str::to_string))
            .unwrap_or_else(|| match code {
                Some(n) => format!("yman exited with code {n}"),
                None => "yman was killed by a signal".to_string(),
            });
        match code {
            Some(1) if stderr.contains(".yman is not initialized") => YmanError::NotInitialized,
            Some(1) if stderr.contains("not inside a git repository") => YmanError::NotGitRepo,
            Some(2) => YmanError::Usage(line),
            Some(3) => YmanError::MergeUnresolved(line),
            Some(4) => YmanError::NoSuchTask(line),
            _ => YmanError::Failed(line),
        }
    }

    pub fn message(&self) -> String {
        match self {
            YmanError::NoBinary => "yman is not installed (no yman on PATH)".to_string(),
            YmanError::NotGitRepo => "The project is not inside a git repository, which yman needs".to_string(),
            YmanError::NotInitialized => "This project has no yman tracker; run yman init in a terminal to create one".to_string(),
            YmanError::Usage(m) => format!("yman rejected the arguments: {m}"),
            YmanError::MergeUnresolved(m) => format!(
                "yman: {m}. A sync merge is unresolved: fix the files yman status lists, then run yman sync --continue (or --abort) in a terminal"
            ),
            YmanError::NoSuchTask(m) | YmanError::Failed(m) => format!("yman: {m}"),
            YmanError::Timeout(cmd) => format!("{cmd} timed out after {} s", TIMEOUT.as_secs()),
            YmanError::Io(m) => format!("Cannot run yman: {m}"),
            YmanError::BadOutput(m) => format!("Cannot read yman's output ({m}); jetl needs yman 0.9 or later"),
        }
    }
}

impl std::fmt::Display for YmanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Run `program args` in `root`. Errors only when yman cannot be started,
/// stdin cannot be written, or it times out; a non-zero exit is reported in
/// `YmanOutput::code`.
pub async fn run(program: &OsStr, root: &Path, args: &[String], input: Option<Vec<u8>>) -> Result<YmanOutput, YmanError> {
    use tokio::io::AsyncWriteExt;
    // A missing current_dir also fails with NotFound, which must not read as
    // "no binary".
    if !root.is_dir() {
        return Err(YmanError::Failed(format!("{} is not a directory", root.display())));
    }
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("EDITOR", "true")
        .env("VISUAL", "true")
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A new session has no controlling terminal, so nothing yman or git runs
    // can open /dev/tty to prompt over the TUI or wait for an answer.
    #[cfg(unix)]
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => YmanError::NoBinary,
        _ => YmanError::Io(e.to_string()),
    })?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // Closing stdin (the drop) ends the input.
        stdin.write_all(&input).await.map_err(|e| YmanError::Io(e.to_string()))?;
    }
    // Dropping the future on timeout drops the child, which kills it.
    match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(out) => {
            let out = out.map_err(|e| YmanError::Io(e.to_string()))?;
            Ok(YmanOutput { code: out.status.code(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
        }
        Err(_) => Err(YmanError::Timeout(format!("yman {}", args.join(" ")))),
    }
}

/// [`run`], a non-zero exit mapped through [`YmanError::from_exit`].
pub async fn run_ok(program: &OsStr, root: &Path, args: &[String], input: Option<Vec<u8>>) -> Result<YmanOutput, YmanError> {
    let out = run(program, root, args, input).await?;
    if out.code == Some(0) { Ok(out) } else { Err(YmanError::from_exit(out.code, &out.stderr)) }
}

/// [`run_ok`] without input, stdout parsed as JSON.
pub async fn json<T: serde::de::DeserializeOwned>(program: &OsStr, root: &Path, args: &[String]) -> Result<T, YmanError> {
    let out = run_ok(program, root, args, None).await?;
    serde_json::from_slice(&out.stdout).map_err(|e| YmanError::BadOutput(e.to_string()))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

pub fn list_args() -> Vec<String> {
    strings(&["ls", "--json", "-a"])
}

pub fn show_args(id: &str) -> Vec<String> {
    strings(&["show", id, "--json"])
}

pub fn tags_args() -> Vec<String> {
    strings(&["tags", "--json"])
}

pub fn status_args() -> Vec<String> {
    strings(&["status", "--json"])
}

/// Test skip check: whether `yman` can be started. Spawns the binary like the
/// service does, so the two cannot disagree.
#[cfg(test)]
pub(crate) fn yman_available() -> bool {
    std::process::Command::new(PROGRAM).arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        strings(a)
    }

    #[test]
    fn from_exit_maps_codes() {
        use YmanError::*;
        assert_eq!(YmanError::from_exit(Some(1), "error: .yman is not initialized; run: yman init\n"), NotInitialized);
        assert_eq!(YmanError::from_exit(Some(1), "error: not inside a git repository\n  fatal: x\n"), NotGitRepo);
        assert_eq!(YmanError::from_exit(Some(4), "error: task 1 not found\n"), NoSuchTask("task 1 not found".into()));
        assert_eq!(YmanError::from_exit(Some(3), "error: sync merge in progress\n"), MergeUnresolved("sync merge in progress".into()));
        assert!(matches!(YmanError::from_exit(Some(2), "error: unexpected argument\n"), Usage(_)));
        assert_eq!(YmanError::from_exit(Some(1), "warning: w\nerror: boom\n"), Failed("boom".into()));
        assert_eq!(YmanError::from_exit(Some(1), ""), Failed("yman exited with code 1".into()));
        assert_eq!(YmanError::from_exit(None, ""), Failed("yman was killed by a signal".into()));
        assert_eq!(NoSuchTask("task 1 not found".into()).message(), "yman: task 1 not found");
        assert_eq!(NoBinary.to_string(), NoBinary.message());
    }

    #[test]
    fn args_are_json_reads() {
        assert_eq!(list_args(), args(&["ls", "--json", "-a"]));
        assert_eq!(show_args("7"), args(&["show", "7", "--json"]));
        assert_eq!(tags_args(), args(&["tags", "--json"]));
        assert_eq!(status_args(), args(&["status", "--json"]));
    }

    #[tokio::test]
    async fn missing_binary_is_no_binary() {
        let r = run(OsStr::new("jetl-no-such-yman-xyz"), &std::env::temp_dir(), &list_args(), None).await;
        assert_eq!(r.unwrap_err(), YmanError::NoBinary);
    }

    #[tokio::test]
    async fn missing_root_is_not_no_binary() {
        let r = run(OsStr::new("sh"), Path::new("/nonexistent-jetl-dir"), &[], None).await;
        assert!(matches!(r, Err(YmanError::Failed(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_reaches_the_child() {
        let tmp = std::env::temp_dir();
        let out = run(OsStr::new("sh"), &tmp, &args(&["-c", "cat"]), Some(b"hi".to_vec())).await.unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(out.stdout, b"hi");
        let err = run_ok(OsStr::new("sh"), &tmp, &args(&["-c", "echo 'error: task 9 not found' >&2; exit 4"]), None).await.unwrap_err();
        assert_eq!(err, YmanError::NoSuchTask("task 9 not found".into()));
    }
}
