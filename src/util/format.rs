//! Running an external formatter (`[format]` in config.toml): the text goes
//! in on stdin, the formatted text comes back on stdout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Why a formatter run produced no text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// One line: the command and what went wrong (the first stderr line).
    pub summary: String,
    /// The formatter's whole stderr, possibly empty.
    pub stderr: String,
}

impl Failure {
    fn new(summary: impl Into<String>) -> Self {
        Self { summary: summary.into(), stderr: String::new() }
    }
}

/// `argv` with `{path}` in each argument replaced by `path`.
pub fn expand(argv: &[String], path: &str) -> Vec<String> {
    argv.iter().map(|a| a.replace("{path}", path)).collect()
}

/// Run `argv` in `cwd` with `input` on stdin and return its stdout. A
/// non-zero exit, output that is not UTF-8, or no output at all for a
/// non-empty input (a formatter that writes the file itself, or nothing) is
/// a failure, so a misconfigured command can never empty the buffer.
///
/// The formatter runs in its own session (no controlling terminal, so it
/// cannot draw over the TUI or wait on a prompt). When it outlives `timeout`
/// or the future is dropped (root switch, quit), its whole process group is
/// killed, so helpers it started (`npx` starting `node`) do not linger.
pub async fn run(argv: &[String], cwd: &Path, input: String, timeout: Duration) -> Result<String, Failure> {
    run_with(argv, cwd, input, timeout, FORMAT).await
}

/// Most bytes of input, and of each run's stdout, Pipe Through Command handles.
pub const PIPE_MAX_BYTES: usize = 16 << 20;
/// Most bytes of stderr a pipe run keeps (the rest is drained and dropped).
pub const PIPE_MAX_STDERR: usize = 64 << 10;
/// Most ranges one Pipe Through Command replaces.
pub const PIPE_MAX_RANGES: usize = 100;

/// Run `sh -c command` like [`run`], for Pipe Through Command: empty output
/// is fine (it deletes the text), stdout is capped at [`PIPE_MAX_BYTES`] and
/// stderr keeps its first [`PIPE_MAX_STDERR`] bytes.
pub async fn pipe(command: &str, cwd: &Path, input: String, timeout: Duration) -> Result<String, Failure> {
    run_with(&["sh".into(), "-c".into(), command.into()], cwd, input, timeout, PIPE).await
}

/// What a run accepts: the `[format]` and the pipe path differ only here.
#[derive(Clone, Copy)]
struct Limits {
    allow_empty: bool,
    max_stdout: usize,
    max_stderr: usize,
}

const FORMAT: Limits = Limits { allow_empty: false, max_stdout: usize::MAX, max_stderr: usize::MAX };
const PIPE: Limits = Limits { allow_empty: true, max_stdout: PIPE_MAX_BYTES, max_stderr: PIPE_MAX_STDERR };

async fn run_with(argv: &[String], cwd: &Path, input: String, timeout: Duration, limits: Limits) -> Result<String, Failure> {
    use tokio::io::AsyncWriteExt;
    let name = argv.first().filter(|n| !n.is_empty()).ok_or_else(|| Failure::new("empty formatter command"))?;
    let mut cmd = tokio::process::Command::new(name);
    cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| Failure::new(format!("cannot run {name}: {e}")))?;
    // Declared after `child`, so it is dropped first: the group is killed
    // while its leader is not reaped yet and the id cannot be reused.
    let group = Group(child.id());
    let had_input = !input.is_empty();
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let feed = async move {
        if let Some(mut stdin) = stdin {
            // A formatter that exits early (bad flags) closes the pipe; its
            // exit status and stderr tell more than the write error.
            let _ = stdin.write_all(input.as_bytes()).await;
        }
    };
    let work = async {
        let (_, out, err, status) =
            tokio::join!(feed, read_capped(stdout, limits.max_stdout), read_capped(stderr, limits.max_stderr), child.wait());
        (out, err, status)
    };
    let Ok((out, err, status)) = tokio::time::timeout(timeout, work).await else {
        return Err(Failure::new(format!("{name} did not finish within {} ms", timeout.as_millis())));
    };
    group.disarm();
    let io = |e: std::io::Error| Failure::new(format!("{name}: {e}"));
    let ((out, out_cut), (err, _), status) = (out.map_err(io)?, err.map_err(io)?, status.map_err(io)?);
    let stderr = String::from_utf8_lossy(&err).trim_end().to_string();
    if !status.success() {
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let summary = if first.is_empty() { format!("{name} failed ({status})") } else { format!("{name}: {first}") };
        return Err(Failure { summary, stderr });
    }
    let fail = |summary: String| Failure { summary, stderr: stderr.clone() };
    if out_cut {
        return Err(fail(format!("{name} printed more than {} MiB", limits.max_stdout >> 20)));
    }
    let text = String::from_utf8(out).map_err(|_| fail(format!("{name} printed text that is not UTF-8")))?;
    if text.is_empty() && had_input && !limits.allow_empty {
        return Err(fail(format!("{name} printed nothing (is it set to write to stdout?)")));
    }
    Ok(text)
}

/// What a child pipe gives until it closes: at most `cap` bytes, the rest is
/// read and dropped (the child must not block on a full pipe). The bool says
/// whether anything was dropped.
async fn read_capped(pipe: Option<impl tokio::io::AsyncRead + Unpin>, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut cut = false;
    if let Some(mut pipe) = pipe {
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let n = pipe.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            let room = cap.saturating_sub(buf.len()).min(n);
            buf.extend_from_slice(&chunk[..room]);
            cut |= room < n;
        }
    }
    Ok((buf, cut))
}

/// Kills the formatter's process group when dropped before [`Group::disarm`]:
/// on a timeout, and when the task running [`run`] is aborted.
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
            // reaped yet (its `Child` is dropped after this guard and only
            // a finished wait disarms it), so the id is still the group's.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    async fn run_sh(script: &str, input: &str) -> Result<String, Failure> {
        run(&sh(script), &std::env::temp_dir(), input.into(), Duration::from_secs(5)).await
    }

    async fn pipe_sh(script: &str, input: &str) -> Result<String, Failure> {
        pipe(script, &std::env::temp_dir(), input.into(), Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn pipe_runs_sh_and_allows_empty_output() {
        assert_eq!(pipe_sh("sort", "c\nb\na\n").await.unwrap(), "a\nb\nc\n");
        assert_eq!(pipe_sh("tr a-z A-Z", "ab").await.unwrap(), "AB");
        assert_eq!(pipe_sh("cat >/dev/null", "x").await.unwrap(), "");
        assert_eq!(pipe_sh("false", "x").await.unwrap_err().summary, "sh failed (exit status: 1)");
        assert_eq!(pipe_sh("echo boom >&2; exit 3", "").await.unwrap_err().summary, "sh: boom");
        assert!(run_sh("cat >/dev/null", "x").await.unwrap_err().summary.contains("printed nothing"));
    }

    #[tokio::test]
    async fn pipe_caps_stdout_and_stderr() {
        let f = pipe_sh("head -c 17000000 /dev/zero | tr '\\0' a", "").await.unwrap_err();
        assert_eq!(f.summary, "sh printed more than 16 MiB");
        let f = pipe_sh("head -c 100000 /dev/zero | tr '\\0' e >&2; exit 1", "").await.unwrap_err();
        assert!(!f.stderr.is_empty() && f.stderr.len() <= PIPE_MAX_STDERR, "{}", f.stderr.len());
    }

    #[tokio::test]
    async fn pipe_timeout_kills_the_command() {
        let start = std::time::Instant::now();
        let f = pipe("sleep 5", &std::env::temp_dir(), String::new(), Duration::from_millis(200)).await.unwrap_err();
        assert!(f.summary.contains("did not finish within 200 ms"), "{}", f.summary);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn path_placeholder_expands_in_every_argument() {
        let argv: Vec<String> = ["prettier", "--stdin-filepath", "{path}", "--x={path}"].map(String::from).into();
        assert_eq!(expand(&argv, "/p/a b.ts"), ["prettier", "--stdin-filepath", "/p/a b.ts", "--x=/p/a b.ts"]);
    }

    #[tokio::test]
    async fn output_and_failures() {
        assert_eq!(run_sh("tr a-z A-Z", "fn x\n").await.unwrap(), "FN X\n");
        // More than a pipe buffer each way: stdin is fed while stdout drains.
        let big = "line\n".repeat(100_000);
        assert_eq!(run_sh("cat", &big).await.unwrap(), big);
        let f = run_sh("cat >/dev/null; echo 'error: expected `;`' >&2; echo ' --> <stdin>:1:5' >&2; exit 1", "x").await.unwrap_err();
        assert_eq!(f.summary, "sh: error: expected `;`");
        assert_eq!(f.stderr, "error: expected `;`\n --> <stdin>:1:5");
        assert_eq!(run_sh("exit 3", "").await.unwrap_err().summary, "sh failed (exit status: 3)");
        // A formatter that prints nothing would empty the buffer.
        assert!(run_sh("cat >/dev/null", "x").await.unwrap_err().summary.contains("printed nothing"));
        assert_eq!(run_sh("cat", "").await.unwrap(), "");
        let tmp = std::env::temp_dir();
        let missing = run(&["jetl-no-such-formatter".into()], &tmp, "x".into(), Duration::from_secs(1)).await;
        assert!(missing.unwrap_err().summary.starts_with("cannot run jetl-no-such-formatter"));
    }

    #[tokio::test]
    async fn timeout_kills_the_formatter_and_its_children() {
        let dir = std::env::temp_dir().join(format!("jetl-test-format-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let start = std::time::Instant::now();
        let f = run(&sh(&script), &dir, "x".into(), Duration::from_millis(300)).await.unwrap_err();
        assert!(f.summary.contains("did not finish within 300 ms"), "{}", f.summary);
        assert!(start.elapsed() < Duration::from_secs(3));
        #[cfg(target_os = "linux")]
        {
            let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();
            // The background sleep (a grandchild) is gone too, or at most a
            // zombie waiting for its new parent to reap it.
            assert!(crate::util::process_gone(pid.parse().unwrap()).await, "sleep {pid} still runs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
