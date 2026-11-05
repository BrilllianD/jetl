//! Running a task: one child in its own session and process group, stdout
//! and stderr on the same pipe (so they interleave as written), stdin
//! closed. A reader thread splits the output into lines, strips ANSI escape
//! codes and matches problems; [`drive`] sends the lines to the app in
//! batches and reports the exit.

use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use super::matcher::{Matcher, RunLine};
use crate::event::AppMsg;

/// Lines are sent at most this often.
pub(crate) const BATCH: Duration = Duration::from_millis(50);
/// How long the output may stay open after the task exits (a background
/// child still holding the pipe) before the exit is reported anyway.
const DRAIN: Duration = Duration::from_millis(500);
/// A longer line without a newline is cut here.
const MAX_LINE: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    Code(i32),
    /// Killed by a signal (a stop sends SIGKILL to the group).
    Signal(i32),
    /// Waiting for the process failed.
    Failed(String),
}

/// A started task, handed to [`drive`].
#[derive(Debug)]
pub struct Spawned {
    pub pid: u32,
    child: tokio::process::Child,
    output: std::io::PipeReader,
}

/// Start `spec`. The child leads a new session (no controlling terminal,
/// so it cannot draw over the TUI or wait on a prompt) whose process group
/// [`kill_group`] stops.
pub fn spawn(spec: &Spec) -> std::io::Result<Spawned> {
    let name = spec.argv.first().filter(|n| !n.is_empty()).ok_or_else(|| std::io::Error::other("empty command"))?;
    let (output, writer) = std::io::pipe()?;
    let mut cmd = tokio::process::Command::new(name);
    cmd.args(&spec.argv[1..])
        .current_dir(&spec.cwd)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .kill_on_drop(true);
    #[cfg(unix)]
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // `cmd` holds the pipe's write ends; dropping it lets the reader see
    // the end of the output once the child and its children close theirs.
    drop(cmd);
    let pid = child.id().ok_or_else(|| std::io::Error::other("the process exited at once"))?;
    Ok(Spawned { pid, child, output })
}

/// SIGKILL to the process group `pid` leads (0 is no process).
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(pid)
        && pid > 0
    {
        // SAFETY: killpg takes plain integers.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

/// Stream `task`'s output as `AppMsg::RunOutput` batches tagged with
/// `generation` and `run`, then send `AppMsg::RunExited`. `pid` holds the
/// group id while the leader is unreaped and is cleared when it is, so a
/// later stop cannot signal a reused id.
pub async fn drive(
    task: Spawned,
    mut matcher: Option<Matcher>,
    generation: u64,
    run: u64,
    tx: UnboundedSender<AppMsg>,
    pid: Arc<AtomicU32>,
) {
    let Spawned { mut child, mut output, .. } = task;
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<Vec<RunLine>>();
    // A thread, not `spawn_blocking`: a background child that keeps the pipe
    // open must not hold up the runtime's shutdown.
    std::thread::spawn(move || {
        let mut split = LineSplitter::default();
        let mut buf = [0u8; 8192];
        loop {
            let n = match output.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let batch: Vec<RunLine> = split
                .push(&buf[..n])
                .into_iter()
                .map(|l| match matcher.as_mut() {
                    Some(m) => m.line(l),
                    None => RunLine::plain(l),
                })
                .collect();
            if !batch.is_empty() && line_tx.send(batch).is_err() {
                return;
            }
        }
        if let Some(last) = split.finish() {
            let _ = line_tx.send(vec![match matcher.as_mut() {
                Some(m) => m.line(last),
                None => RunLine::plain(last),
            }]);
        }
    });
    let mut pending: Vec<RunLine> = Vec::new();
    let flush = |pending: &mut Vec<RunLine>| {
        if !pending.is_empty() {
            let _ = tx.send(AppMsg::RunOutput { generation, run, lines: std::mem::take(pending) });
        }
    };
    let mut tick = tokio::time::interval(BATCH);
    let mut end: Option<RunEnd> = None;
    let mut eof = false;
    let drain = tokio::time::sleep(Duration::MAX / 4);
    tokio::pin!(drain);
    loop {
        tokio::select! {
            batch = lines.recv(), if !eof => match batch {
                Some(batch) => pending.extend(batch),
                None => eof = true,
            },
            status = child.wait(), if end.is_none() => {
                pid.store(0, Ordering::SeqCst);
                end = Some(match status {
                    Ok(s) => exit_of(s),
                    Err(e) => RunEnd::Failed(e.to_string()),
                });
                drain.as_mut().reset(tokio::time::Instant::now() + DRAIN);
            }
            _ = tick.tick() => flush(&mut pending),
            _ = &mut drain, if end.is_some() => break,
        }
        if eof && end.is_some() {
            break;
        }
    }
    flush(&mut pending);
    let _ = tx.send(AppMsg::RunExited { generation, run, end: end.unwrap_or(RunEnd::Failed("no exit status".into())) });
}

fn exit_of(s: std::process::ExitStatus) -> RunEnd {
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&s) {
        return RunEnd::Signal(sig);
    }
    RunEnd::Code(s.code().unwrap_or(-1))
}

/// Bytes to lines: `\n` ends a line, a trailing `\r` is dropped, and a `\r`
/// inside a line (a progress bar redrawing itself) keeps only what follows
/// it. ANSI escape sequences go; tabs become spaces.
#[derive(Debug, Default)]
pub(crate) struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                out.push(clean(&std::mem::take(&mut self.partial)));
            } else {
                self.partial.push(b);
                if self.partial.len() >= MAX_LINE {
                    out.push(clean(&std::mem::take(&mut self.partial)));
                }
            }
        }
        out
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        (!self.partial.is_empty()).then(|| clean(&std::mem::take(&mut self.partial)))
    }
}

/// One raw output line as display text.
pub(crate) fn clean(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    let text: &str = text.rsplit('\r').find(|s| !s.is_empty()).unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters and intermediates up to a final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC (hyperlinks, titles): up to BEL or ESC \.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => {
                let n = 4 - out.chars().count() % 4;
                out.extend(std::iter::repeat_n(' ', n));
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_split_and_cleaned() {
        let mut s = LineSplitter::default();
        assert_eq!(s.push(b"one\r\ntw"), ["one"]);
        assert_eq!(s.push(b"o\n\x1b[1m\x1b[31merror\x1b[0m: x\n"), ["two", "error: x"]);
        assert_eq!(s.push(b"10%\r50%\r100%\r\n"), ["100%"]);
        assert_eq!(s.push(b"\x1b]8;;file:///a\x1b\\link\x1b]8;;\x07 a\tb\n"), ["link a  b"]);
        assert_eq!(s.push(b"no newline"), Vec::<String>::new());
        assert_eq!(s.finish().as_deref(), Some("no newline"));
        assert_eq!(s.finish(), None);
        // Bytes that are not UTF-8 do not stop the output.
        assert_eq!(s.push(b"caf\xc3\xa9 \xff\n"), ["café \u{fffd}"]);
    }
}
