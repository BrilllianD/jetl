//! Running a task: one child in its own session and process group, stdout
//! and stderr on the same pipe (so they interleave as written), stdin
//! closed. A reader thread splits the output into lines, strips ANSI escape
//! codes and matches problems; [`drive`] sends the lines to the app in
//! batches and reports the exit.

use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use super::matcher::{Matcher, RunLine, Severity};
use crate::event::AppMsg;

/// Lines are sent at most this often.
pub(crate) const BATCH: Duration = Duration::from_millis(50);
/// How long the output may stay open after the task exits (a background
/// child still holding the pipe) before the exit is reported anyway.
const DRAIN: Duration = Duration::from_millis(500);
/// A longer line without a newline is cut here.
const MAX_LINE: usize = 16 * 1024;
/// The bytes of output lines that may wait between the reader thread and the Run window. Over it, `admit` drops.
pub(crate) const MAX_QUEUED: usize = 32 * 1024 * 1024;
/// What a line is charged on top of its text: the `RunLine` struct, its batch slot and the text allocation's header.
pub(crate) const LINE_COST: usize = 128;

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

/// A byte gauge over the lines on their way from the reader thread to the Run window, or held on the app bus while
/// the main thread is stalled. The reader thread charges each line and drops it, counting its bytes, when the gauge
/// is over its limit; `App::run_output` gives the charge back. It never blocks: the child keeps running.
#[derive(Debug)]
pub struct RunGate {
    limit: usize,
    queued: AtomicUsize,
    /// Dropping since the gauge reached `limit`, until it falls to half of it: the notice would repeat per line otherwise.
    dropping: AtomicBool,
    dropped: AtomicUsize,
}

impl RunGate {
    pub(crate) fn new(limit: usize) -> Self {
        Self { limit, queued: AtomicUsize::new(0), dropping: AtomicBool::new(false), dropped: AtomicUsize::new(0) }
    }

    /// Whether `l` may go on. Only the reader thread calls this. The check comes before the charge, so one line can
    /// pass the limit by its own size.
    pub(crate) fn admit(&self, l: &RunLine) -> bool {
        let q = self.queued.load(Ordering::SeqCst);
        let closed = if self.dropping.load(Ordering::SeqCst) { q > self.limit / 2 } else { q >= self.limit };
        self.dropping.store(closed, Ordering::SeqCst);
        if closed {
            self.dropped.fetch_add(l.text.len() + 1, Ordering::SeqCst);
            false
        } else {
            self.queued.fetch_add(cost(l), Ordering::SeqCst);
            true
        }
    }

    /// Gives back what `admit` and `notice` charged for `lines`.
    pub(crate) fn release(&self, lines: &[RunLine]) {
        self.queued.fetch_sub(lines.iter().map(cost).sum(), Ordering::SeqCst);
    }

    /// The warning line that tells of the bytes dropped since the last one, charged like any line.
    pub(crate) fn notice(&self) -> Option<RunLine> {
        let n = self.dropped.swap(0, Ordering::SeqCst);
        if n == 0 {
            return None;
        }
        let l = RunLine {
            text: format!("{n} bytes of output dropped: the program prints faster than the Run window can read"),
            severity: Some(Severity::Warning),
            link: None,
        };
        self.queued.fetch_add(cost(&l), Ordering::SeqCst);
        Some(l)
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }
}

fn cost(l: &RunLine) -> usize {
    l.text.len() + LINE_COST
}

/// What the reader thread sends on: the lines the gate refuses go nowhere, and the notice of what was dropped goes
/// ahead of the next admitted line and, when `end`, after the last one.
pub(crate) fn gated(gate: &RunGate, lines: Vec<RunLine>, end: bool) -> Vec<RunLine> {
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        if gate.admit(&l) {
            out.extend(gate.notice());
            out.push(l);
        }
    }
    if end {
        out.extend(gate.notice());
    }
    out
}

/// Stream `task`'s output as `AppMsg::RunOutput` batches tagged with
/// `generation` and `run`, then send `AppMsg::RunExited`. `pid` holds the
/// group id while the leader is unreaped and is cleared when it is, so a
/// later stop cannot signal a reused id.
pub async fn drive(
    task: Spawned,
    mut matcher: Option<Matcher>,
    gate: Arc<RunGate>,
    generation: u64,
    run: u64,
    tx: UnboundedSender<AppMsg>,
    pid: Arc<AtomicU32>,
) {
    let mut split = LineSplitter::default();
    let mut classify = move |l: String| match matcher.as_mut() {
        Some(m) => m.line(l),
        None => RunLine::plain(l),
    };
    let end_gate = gate.clone();
    let feed = move |bytes: Option<&[u8]>| -> Vec<RunLine> {
        // Classified before the gate, so the matcher's lookahead sees every line.
        match bytes {
            Some(b) => gated(&gate, split.push(b).into_iter().map(&mut classify).collect(), false),
            None => gated(&gate, split.finish().into_iter().map(&mut classify).collect(), true),
        }
    };
    let end = pump(
        task,
        feed,
        |lines| {
            let _ = tx.send(AppMsg::RunOutput { generation, run, lines });
        },
        pid,
    )
    .await;
    // `pump` gave up on a reader still blocked on a pipe a grandchild holds open, so `feed(None)` never ran:
    // the end notice goes out from here. After a normal end `feed(None)` took the count and this is `None`.
    if let Some(notice) = end_gate.notice() {
        let _ = tx.send(AppMsg::RunOutput { generation, run, lines: vec![notice] });
    }
    let _ = tx.send(AppMsg::RunExited { generation, run, end });
}

/// Read `task`'s output through `feed` on a reader thread (`None` once, at
/// the end of the output) and hand what it returns to `emit` at most every
/// [`BATCH`], never as an empty batch. Returns how the task ended. `pid` is
/// cleared when the leader is reaped.
pub(crate) async fn pump<T: Send + 'static>(
    task: Spawned,
    mut feed: impl FnMut(Option<&[u8]>) -> Vec<T> + Send + 'static,
    mut emit: impl FnMut(Vec<T>),
    pid: Arc<AtomicU32>,
) -> RunEnd {
    let Spawned { mut child, mut output, .. } = task;
    let (item_tx, mut items) = tokio::sync::mpsc::unbounded_channel::<Vec<T>>();
    // A thread, not `spawn_blocking`: a background child that keeps the pipe
    // open must not hold up the runtime's shutdown.
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match output.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let batch = feed(Some(&buf[..n]));
            if !batch.is_empty() && item_tx.send(batch).is_err() {
                return;
            }
        }
        let last = feed(None);
        if !last.is_empty() {
            let _ = item_tx.send(last);
        }
    });
    let mut pending: Vec<T> = Vec::new();
    let mut flush = |pending: &mut Vec<T>| {
        if !pending.is_empty() {
            emit(std::mem::take(pending));
        }
    };
    let mut tick = tokio::time::interval(BATCH);
    let mut end: Option<RunEnd> = None;
    let mut eof = false;
    let drain = tokio::time::sleep(Duration::MAX / 4);
    tokio::pin!(drain);
    loop {
        tokio::select! {
            batch = items.recv(), if !eof => match batch {
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
    end.unwrap_or(RunEnd::Failed("no exit status".into()))
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

    const NOTICE: &str = "bytes of output dropped: the program prints faster than the Run window can read";

    fn line(n: usize) -> RunLine {
        RunLine::plain("x".repeat(n))
    }

    #[test]
    fn the_run_gate_drops_at_its_limit_and_counts() {
        assert!(std::mem::size_of::<RunLine>() <= LINE_COST);
        let g = RunGate::new(1000);
        assert!(g.admit(&line(900)));
        assert_eq!(g.queued(), 900 + LINE_COST);
        assert!(!g.admit(&line(10)));
        assert_eq!(g.queued(), 900 + LINE_COST);
        let n = g.notice().unwrap();
        assert_eq!(n.text, format!("11 {NOTICE}"));
        assert_eq!(n.severity, Some(Severity::Warning));
        assert!(g.notice().is_none());
    }

    #[test]
    fn the_run_gate_reopens_at_half() {
        let g = RunGate::new(1000);
        let big = line(900);
        assert!(g.admit(&big));
        assert!(!g.admit(&line(1)));
        // 900 + LINE_COST = 1028 charged, half of the limit is 500.
        g.release(&[line(300)]);
        assert_eq!(g.queued(), 1028 - (300 + LINE_COST));
        assert!(g.queued() > 500);
        assert!(!g.admit(&line(1)), "still closed above half");
        g.release(&[line(0)]);
        assert_eq!(g.queued(), 1028 - (300 + LINE_COST) - LINE_COST);
        assert!(g.queued() <= 500);
        assert!(g.admit(&line(1)), "open at half or below");
    }

    #[test]
    fn the_notice_is_charged_and_given_back() {
        let g = RunGate::new(10);
        assert!(g.admit(&line(20)));
        assert!(!g.admit(&line(5)));
        let before = g.queued();
        let n = g.notice().unwrap();
        assert_eq!(g.queued(), before + n.text.len() + LINE_COST);
        g.release(&[n, line(20)]);
        assert_eq!(g.queued(), 0);
    }

    #[test]
    fn gated_puts_the_notice_before_the_next_admitted_line_and_at_the_end() {
        let texts = |v: Vec<RunLine>| v.into_iter().map(|l| l.text).collect::<Vec<_>>();
        let g = RunGate::new(200);
        // `a` is admitted (queued 129), `b` too (129 is under 200 at the check), `c` is refused.
        assert_eq!(texts(gated(&g, vec![line(1), line(1), line(1)], false)), ["x", "x"]);
        // Nothing admitted while closed, and no notice without an admitted line or the end.
        assert!(gated(&g, vec![line(7)], false).is_empty());
        // The end alone carries the pending count.
        assert_eq!(texts(gated(&g, vec![line(3)], true)), [format!("14 {NOTICE}")]);
        // No drops pending: the end adds nothing.
        assert!(gated(&g, vec![], true).is_empty());
        // Reopen, then the notice goes ahead of the next admitted line.
        let h = RunGate::new(200);
        assert_eq!(gated(&h, vec![line(1), line(1)], false).len(), 2);
        assert!(gated(&h, vec![line(4)], false).is_empty());
        h.release(&[line(1), line(1)]);
        assert_eq!(texts(gated(&h, vec![line(2)], false)), [format!("5 {NOTICE}"), "xx".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drive_drops_over_the_limit_and_says_so() {
        let spec = Spec { argv: vec!["sh".into(), "-c".into(), "printf 'x\\ny\\n'".into()], cwd: std::env::temp_dir(), env: vec![] };
        let spawned = spawn(&spec).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(drive(spawned, None, Arc::new(RunGate::new(1)), 1, 1, tx, Arc::new(AtomicU32::new(0))));
        let mut texts = Vec::new();
        loop {
            match rx.recv().await.unwrap() {
                AppMsg::RunOutput { lines, .. } => texts.extend(lines.into_iter().map(|l| l.text)),
                AppMsg::RunExited { end, .. } => {
                    assert_eq!(end, RunEnd::Code(0));
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(texts, ["x".to_string(), format!("2 {NOTICE}")]);
    }

    #[test]
    fn a_refused_empty_line_still_counts() {
        let g = RunGate::new(1);
        assert!(g.admit(&line(1)));
        assert!(!g.admit(&line(0)));
        assert!(!g.admit(&line(0)));
        assert_eq!(gated(&g, vec![], true).into_iter().map(|l| l.text).collect::<Vec<_>>(), [format!("2 {NOTICE}")]);
    }

    #[test]
    fn the_run_gate_flips_exactly_at_limit_and_half() {
        // line(872) costs 1000: the gauge sits exactly at the limit.
        let g = RunGate::new(1000);
        assert!(g.admit(&line(871)));
        assert_eq!(g.queued(), 999);
        let g = RunGate::new(1000);
        assert!(g.admit(&line(872)));
        assert_eq!(g.queued(), 1000);
        assert!(!g.admit(&line(1)), "at the limit it closes");
        // line(372) costs 500: the gauge falls exactly to half of the limit.
        g.release(&[line(372)]);
        assert_eq!(g.queued(), 500);
        assert!(g.admit(&line(1)), "at half it reopens");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drive_sends_the_end_notice_when_a_grandchild_holds_the_pipe() {
        let argv = ["sh", "-c", "sleep 2 & printf 'x\\ny\\n'"].map(String::from).to_vec();
        let spawned = spawn(&Spec { argv, cwd: std::env::temp_dir(), env: vec![] }).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(drive(spawned, None, Arc::new(RunGate::new(1)), 1, 1, tx, Arc::new(AtomicU32::new(0))));
        let mut texts = Vec::new();
        loop {
            match rx.recv().await.unwrap() {
                AppMsg::RunOutput { lines, .. } => texts.extend(lines.into_iter().map(|l| l.text)),
                AppMsg::RunExited { .. } => break,
                _ => {}
            }
        }
        assert_eq!(texts, ["x".to_string(), format!("2 {NOTICE}")]);
    }
}
