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
            // Short enough for a one-line toast; yman's own line (a conflict
            // count, or "fix the files yman status lists") stays out of it.
            YmanError::MergeUnresolved(_) => "Sync merge unresolved: run yman sync --continue or --abort".to_string(),
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
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
    let group = Group(child.id());
    let stdin = child.stdin.take();
    // The input is written while the output is read: a child that stops
    // reading cannot stall the task, and one that exits early only breaks
    // the pipe, which its exit code and stderr then explain. Dropping the
    // future on timeout drops the child, which kills it, and `group` kills
    // the git processes it started.
    let work = async {
        let write = async {
            match (input, stdin) {
                (Some(input), Some(mut stdin)) => match stdin.write_all(&input).await {
                    Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e),
                    _ => Ok(()),
                },
                _ => Ok(()),
            }
            // Closing stdin (the drop) ends the input.
        };
        let (written, out) = tokio::join!(write, child.wait_with_output());
        let out = out?;
        written?;
        Ok::<_, std::io::Error>(out)
    };
    match tokio::time::timeout(TIMEOUT, work).await {
        Ok(out) => {
            group.disarm();
            let out = out.map_err(|e| YmanError::Io(e.to_string()))?;
            Ok(YmanOutput { code: out.status.code(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
        }
        Err(_) => Err(YmanError::Timeout(format!("yman {}", args.join(" ")))),
    }
}

/// Kills yman's process group when dropped before [`Group::disarm`]: on a
/// timeout, and when the task running [`run`] is aborted.
struct Group(Option<u32>);

impl Group {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|p| libc::pid_t::try_from(p).ok()) {
            // SAFETY: killpg takes plain integers. The group leader is not
            // reaped yet (its `Child` is dropped before this guard and only
            // a finished wait disarms it), so the id is still the group's.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
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

/// The default `ls`: yman hides the closed tasks itself.
pub fn open_args() -> Vec<String> {
    strings(&["ls", "--json"])
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

/// A status change of one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusChange {
    Start,
    Done,
    Cancel,
    Reopen,
    /// `yman move` to this status.
    Move(String),
}

/// `start|done|cancel|reopen [--message=NOTE] ID` or `move [--message=NOTE] ID STATUS`; a blank
/// note is left out.
pub fn status_change_args(id: &str, change: &StatusChange, note: Option<&str>) -> Vec<String> {
    let verb = match change {
        StatusChange::Start => "start",
        StatusChange::Done => "done",
        StatusChange::Cancel => "cancel",
        StatusChange::Reopen => "reopen",
        StatusChange::Move(_) => "move",
    };
    let mut a = vec![verb.to_string()];
    if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
        a.push(format!("--message={n}"));
    }
    a.push(id.to_string());
    if let StatusChange::Move(s) = change {
        a.push(s.clone());
    }
    a
}

/// `prio ID N`.
pub fn priority_args(id: &str, priority: u8) -> Vec<String> {
    vec!["prio".to_string(), id.to_string(), priority.to_string()]
}

/// `set --assignee=WHO ID`, or `set --no-assignee ID` for `None`.
pub fn assign_args(id: &str, who: Option<&str>) -> Vec<String> {
    match who {
        Some(w) => vec!["set".to_string(), format!("--assignee={w}"), id.to_string()],
        None => strings(&["set", "--no-assignee", id]),
    }
}

/// `set --tag=A... --untag=R... ID`.
pub fn tag_change_args(id: &str, add: &[String], remove: &[String]) -> Vec<String> {
    let mut a = vec!["set".to_string()];
    a.extend(add.iter().map(|t| format!("--tag={t}")));
    a.extend(remove.iter().map(|t| format!("--untag={t}")));
    a.push(id.to_string());
    a
}

/// `comment ID`; the text goes on stdin.
pub fn comment_args(id: &str) -> Vec<String> {
    strings(&["comment", id])
}

/// The answers of the New Task dialog, validated (`ui::new_task`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewTask {
    pub title: String,
    pub tags: Vec<String>,
    /// `None`: yman's configured default.
    pub priority: Option<u8>,
    pub assignee: Option<String>,
    /// Task ids, without `#`.
    pub related: Vec<String>,
    /// Markdown; blank means no body.
    pub body: String,
}

/// `add [--priority=N] [--tag=T]... [--assignee=W] [--relate=ID]... [--body-file=-] -- TITLE`.
pub fn add_args(t: &NewTask) -> Vec<String> {
    let mut a = vec!["add".to_string()];
    if let Some(p) = t.priority {
        a.push(format!("--priority={p}"));
    }
    a.extend(t.tags.iter().map(|g| format!("--tag={g}")));
    if let Some(w) = &t.assignee {
        a.push(format!("--assignee={w}"));
    }
    a.extend(t.related.iter().map(|r| format!("--relate={r}")));
    if add_input(t).is_some() {
        a.push("--body-file=-".to_string());
    }
    a.push("--".to_string());
    a.push(t.title.clone());
    a
}

/// The body for `add_args`' `--body-file=-`: `None` when blank, else the body with trailing
/// whitespace trimmed plus one `\n`.
pub fn add_input(t: &NewTask) -> Option<Vec<u8>> {
    let b = t.body.trim_end();
    if b.trim().is_empty() {
        return None;
    }
    Some(format!("{b}\n").into_bytes())
}

/// The id from `yman add`'s stdout line `added <id>  <folder>`.
pub fn added_id(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("added "))
        .and_then(|l| l.split_whitespace().nth(1))
        .map(str::to_string)
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
        assert_eq!(open_args(), args(&["ls", "--json"]));
        assert_eq!(show_args("7"), args(&["show", "7", "--json"]));
        assert_eq!(tags_args(), args(&["tags", "--json"]));
        assert_eq!(status_args(), args(&["status", "--json"]));
    }

    #[test]
    fn mutation_args() {
        use StatusChange::*;
        assert_eq!(status_change_args("7", &Done, Some("-x shipped")), args(&["done", "--message=-x shipped", "7"]));
        assert_eq!(status_change_args("7", &Move("blocked".into()), None), args(&["move", "7", "blocked"]));
        assert_eq!(status_change_args("7", &Move("blocked".into()), Some(" n ")), args(&["move", "--message=n", "7", "blocked"]));
        assert_eq!(status_change_args("7", &Start, Some("  ")), args(&["start", "7"]));
        assert_eq!(status_change_args("7", &Reopen, None), args(&["reopen", "7"]));
        assert_eq!(status_change_args("7", &Cancel, None), args(&["cancel", "7"]));
        assert_eq!(priority_args("7", 3), args(&["prio", "7", "3"]));
        assert_eq!(assign_args("7", None), args(&["set", "--no-assignee", "7"]));
        assert_eq!(assign_args("7", Some("-bob")), args(&["set", "--assignee=-bob", "7"]));
        assert_eq!(tag_change_args("7", &["a".into()], &["b".into()]), args(&["set", "--tag=a", "--untag=b", "7"]));
        assert_eq!(comment_args("7"), args(&["comment", "7"]));
    }

    #[test]
    fn add_args_and_id() {
        let t = NewTask { title: "-x".into(), ..Default::default() };
        assert_eq!(add_args(&t), args(&["add", "--", "-x"]));
        assert_eq!(add_input(&t), None);
        let t = NewTask {
            title: "T".into(),
            tags: vec!["a".into(), "b".into()],
            priority: Some(0),
            assignee: Some("bob".into()),
            related: vec!["2".into(), "10".into()],
            body: "l1\nl2\n\n".into(),
        };
        assert_eq!(
            add_args(&t),
            args(&["add", "--priority=0", "--tag=a", "--tag=b", "--assignee=bob", "--relate=2", "--relate=10", "--body-file=-", "--", "T"])
        );
        assert_eq!(add_input(&t), Some(b"l1\nl2\n".to_vec()));
        let t = NewTask { title: "T".into(), body: "  \n".into(), ..Default::default() };
        assert_eq!(add_args(&t), args(&["add", "--", "T"]));
        assert_eq!(add_input(&t), None);
        assert_eq!(added_id(b"added 12  .yman/5.12.fix-it\n"), Some("12".into()));
        assert_eq!(added_id(b""), None);
        assert_eq!(added_id(b"nothing\n"), None);
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
    async fn early_exit_with_large_stdin_reports_the_exit() {
        let big = vec![b'x'; 4 << 20];
        let err = run_ok(
            OsStr::new("sh"),
            &std::env::temp_dir(),
            &args(&["-c", "echo 'error: .yman is not initialized' >&2; exit 1"]),
            Some(big),
        )
        .await
        .unwrap_err();
        assert_eq!(err, YmanError::NotInitialized);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn large_stdin_to_a_reader_does_not_deadlock() {
        let big = vec![b'x'; 4 << 20];
        let out = run(OsStr::new("sh"), &std::env::temp_dir(), &args(&["-c", "cat"]), Some(big.clone())).await.unwrap();
        assert_eq!(out.stdout, big);
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
