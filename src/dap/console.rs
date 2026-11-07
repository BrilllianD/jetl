//! The Debug window's Console, off the main thread. A debuggee can print 100 000 lines in a second, each an
//! `output` event, so the App only forwards events into a channel; one thread per session splits them into
//! lines, matches file locations (blocking `stat`s) and hands the window batches of at most one per `BATCH`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::time::Instant;

use super::protocol::OutputEvent;
use super::{DapBody, DapMsg};
use crate::run::matcher::{Matcher, RunLine, Severity};
use crate::run::process::{BATCH, LineSplitter};

/// Splitters and matcher of one session. A category keeps its own partial line: stdout and stderr interleave
/// by event, and a half line of one must not glue to the next line of the other.
pub(crate) struct ConsoleFeed {
    out: LineSplitter,
    err: LineSplitter,
    other: LineSplitter,
    matcher: Matcher,
}

impl ConsoleFeed {
    pub(crate) fn new(matcher: Matcher) -> Self {
        Self { out: LineSplitter::default(), err: LineSplitter::default(), other: LineSplitter::default(), matcher }
    }

    /// The lines `e` completes. `stderr` lines are errors and `important` ones warnings; a held partial line takes
    /// the style of the event that completes it.
    pub(crate) fn feed(&mut self, e: &OutputEvent) -> Vec<RunLine> {
        let category = e.category.as_deref();
        let splitter = match category {
            Some("stderr") => &mut self.err,
            Some("stdout") => &mut self.out,
            _ => &mut self.other,
        };
        let lines = splitter.push(e.output.as_bytes());
        lines.into_iter().map(|text| style(self.matcher.line(text), category)).collect()
    }

    /// The partial lines still held, in the order `out`, `err`, `other`.
    pub(crate) fn finish(&mut self) -> Vec<RunLine> {
        let mut lines = Vec::new();
        if let Some(t) = self.out.finish() {
            lines.push(self.matcher.line(t));
        }
        if let Some(t) = self.err.finish() {
            lines.push(style(self.matcher.line(t), Some("stderr")));
        }
        if let Some(t) = self.other.finish() {
            lines.push(self.matcher.line(t));
        }
        lines
    }
}

fn style(mut line: RunLine, category: Option<&str>) -> RunLine {
    match category {
        Some("stderr") => line.severity = Some(Severity::Error),
        Some("important") => {
            line.severity.get_or_insert(Severity::Warning);
        }
        _ => {}
    }
    line
}

/// The bytes of `output` events that may wait between the adapter and the Console thread. Over it, `admit` drops.
pub(crate) const MAX_QUEUED: usize = 32 * 1024 * 1024;
/// What an event is charged on top of its text: the message, the channel slot and the allocations (measured about 430).
const EVENT_COST: usize = 512;

/// A byte gauge over the events on their way to the Console thread, or held on the app bus while the main thread is
/// stalled. The adapter's driver charges each event and drops it, counting its bytes, when the gauge is over its
/// limit; the Console thread gives the charge back once it has fed the event, and the App for a line it hides. It
/// never blocks: an adapter whose pipe is not read just grows itself (gdb does, and keeps the program running).
pub(crate) struct ConsoleGate {
    limit: usize,
    queued: AtomicUsize,
    /// Dropping since the gauge reached `limit`, until it falls to half of it: the notice would repeat per event otherwise.
    dropping: AtomicBool,
    dropped: AtomicUsize,
}

impl ConsoleGate {
    pub(crate) fn new(limit: usize) -> Self {
        Self { limit, queued: AtomicUsize::new(0), dropping: AtomicBool::new(false), dropped: AtomicUsize::new(0) }
    }

    /// Whether `e` may go on. Only the driver's thread calls this. The check comes before the charge, so one event
    /// can pass the limit by its own size.
    pub(crate) fn admit(&self, e: &OutputEvent) -> bool {
        let q = self.queued.load(Ordering::SeqCst);
        let closed = if self.dropping.load(Ordering::SeqCst) { q > self.limit / 2 } else { q >= self.limit };
        self.dropping.store(closed, Ordering::SeqCst);
        if closed {
            self.dropped.fetch_add(e.output.len(), Ordering::SeqCst);
            false
        } else {
            self.queued.fetch_add(cost(e), Ordering::SeqCst);
            true
        }
    }

    /// Gives back what `admit` charged for `e`.
    pub(crate) fn release(&self, e: &OutputEvent) {
        self.queued.fetch_sub(cost(e), Ordering::SeqCst);
    }

    /// The `important` line that tells of the bytes dropped since the last one, charged like any event.
    pub(crate) fn notice(&self) -> Option<OutputEvent> {
        let n = self.dropped.swap(0, Ordering::SeqCst);
        if n == 0 {
            return None;
        }
        let e = OutputEvent {
            category: Some("important".into()),
            output: format!("{n} bytes of output dropped: the program prints faster than the Console can read\n"),
        };
        self.queued.fetch_add(cost(&e), Ordering::SeqCst);
        Some(e)
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }
}

fn cost(e: &OutputEvent) -> usize {
    e.output.len() + EVENT_COST
}

/// What the `emit` closure of `DapClient::start` does with a message: an `output` event the gate refuses goes
/// nowhere, and the notice of what was dropped goes ahead of the next admitted event or the end of the session.
/// Runs on the driver's thread, so `send` must not block.
pub(crate) fn forward_output(gate: &ConsoleGate, m: DapMsg, send: impl Fn(DapMsg)) {
    match &m.body {
        DapBody::Output(e) if !gate.admit(e) => return,
        DapBody::Output(_) | DapBody::Ended { .. } => {
            if let Some(n) = gate.notice() {
                send(DapMsg { generation: m.generation, session: m.session, body: DapBody::Output(n) });
            }
        }
        _ => {}
    }
    send(m);
}

/// Starts the Console thread: events sent to the returned sender come out of `emit` as lines, in batches. The
/// thread gives `gate` back each event's charge once it has fed it, and ends when the sender is dropped, after flushing the held partial lines. `cwd` and `root` are where
/// relative locations are looked for.
pub(crate) fn spawn_console(
    cwd: PathBuf,
    root: PathBuf,
    gate: Arc<ConsoleGate>,
    emit: impl Fn(Vec<RunLine>) + Send + 'static,
) -> Sender<OutputEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<OutputEvent>();
    // A thread, not a runtime task: the matcher stats files.
    std::thread::spawn(move || {
        let mut feed = ConsoleFeed::new(Matcher::new(&cwd, &root));
        let mut pending: Vec<RunLine> = Vec::new();
        let mut last = Instant::now();
        loop {
            match rx.recv_timeout(BATCH) {
                Ok(e) => {
                    pending.extend(feed.feed(&e));
                    gate.release(&e);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    pending.extend(feed.finish());
                    if !pending.is_empty() {
                        emit(pending);
                    }
                    return;
                }
            }
            if !pending.is_empty() && last.elapsed() >= BATCH {
                emit(std::mem::take(&mut pending));
                last = Instant::now();
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::dap::session::SessionId;

    fn out(category: Option<&str>, text: &str) -> OutputEvent {
        OutputEvent { category: category.map(Into::into), output: text.into() }
    }

    fn feed() -> ConsoleFeed {
        ConsoleFeed::new(Matcher::new(Path::new("/nonexistent"), Path::new("/nonexistent")))
    }

    fn texts(lines: &[RunLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn partial_lines_wait_for_their_newline() {
        let mut f = feed();
        assert!(f.feed(&out(Some("stdout"), "ab")).is_empty());
        assert_eq!(texts(&f.feed(&out(Some("stdout"), "c\nd"))), ["abc"]);
        assert_eq!(texts(&f.finish()), ["d"]);
        assert!(f.finish().is_empty());
    }

    #[test]
    fn each_category_keeps_its_own_partial() {
        let mut f = feed();
        assert!(f.feed(&out(Some("stdout"), "a")).is_empty());
        let e = f.feed(&out(Some("stderr"), "e\n"));
        assert_eq!(texts(&e), ["e"]);
        assert_eq!(e[0].severity, Some(Severity::Error));
        let b = f.feed(&out(Some("stdout"), "b\n"));
        assert_eq!(texts(&b), ["ab"]);
        assert_eq!(b[0].severity, None);
    }

    #[test]
    fn stderr_is_an_error_and_important_a_warning() {
        let table = [
            (Some("stdout"), None),
            (None, None),
            (Some("console"), None),
            (Some("stderr"), Some(Severity::Error)),
            (Some("important"), Some(Severity::Warning)),
        ];
        for (category, want) in table {
            let lines = feed().feed(&out(category, "plain text\n"));
            assert_eq!(lines.len(), 1, "{category:?}");
            assert_eq!(lines[0].severity, want, "{category:?}");
        }
        // The matcher's own verdict outranks `important`'s default.
        let lines = feed().feed(&out(Some("important"), "error: broken\n"));
        assert_eq!(lines[0].severity, Some(Severity::Error));
    }

    #[test]
    fn a_link_survives_the_styling() {
        let dir = std::env::temp_dir().join(format!("jetl-console-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.c"), "x\ny\n").unwrap();
        let mut f = ConsoleFeed::new(Matcher::new(&dir, &dir));
        let lines = f.feed(&out(Some("stderr"), "see a.c:2 here\n"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(lines[0].severity, Some(Severity::Error));
        let link = lines[0].link.as_ref().expect("a link");
        assert_eq!(link.line, 1);
        assert_eq!(&lines[0].text[link.span.clone()], "a.c:2");
    }

    #[test]
    fn spawn_console_batches_and_flushes_when_dropped() {
        let (tx_out, rx_out) = std::sync::mpsc::channel::<Vec<RunLine>>();
        let tx = spawn_console("/nonexistent".into(), "/nonexistent".into(), Arc::new(ConsoleGate::new(MAX_QUEUED)), move |l| {
            let _ = tx_out.send(l);
        });
        for i in 0..1000 {
            tx.send(out(Some("stdout"), &format!("line {i}\n"))).unwrap();
        }
        tx.send(out(Some("stdout"), "tail")).unwrap();
        drop(tx);
        let mut batches = Vec::new();
        while let Ok(b) = rx_out.recv_timeout(Duration::from_secs(5)) {
            batches.push(b);
        }
        let all: Vec<String> = batches.iter().flatten().map(|l| l.text.clone()).collect();
        let want: Vec<String> = (0..1000).map(|i| format!("line {i}")).chain(["tail".to_string()]).collect();
        assert_eq!(all, want);
        assert!(batches.len() < 100, "{} emits", batches.len());
    }

    /// An event of `n` bytes of text.
    fn big(n: usize) -> OutputEvent {
        out(Some("stdout"), &"x".repeat(n))
    }

    fn notice_text(n: usize) -> String {
        format!("{n} bytes of output dropped: the program prints faster than the Console can read\n")
    }

    #[test]
    fn the_gate_drops_at_its_limit_and_counts() {
        let g = ConsoleGate::new(4096);
        let e = big(1000);
        // 1512 each: 0, 1512 and 3024 are under the limit, 4536 is not.
        for i in 0..3 {
            assert!(g.admit(&e), "event {i}");
        }
        assert_eq!(g.queued(), 3 * (1000 + 512));
        assert!(!g.admit(&e), "over the limit");
        assert_eq!(g.queued(), 3 * (1000 + 512), "a refused event is not charged");
        assert_eq!(g.notice().expect("a notice").output, notice_text(1000));
    }

    #[test]
    fn the_gate_reopens_at_half() {
        let g = ConsoleGate::new(4096);
        let e = big(1000);
        for _ in 0..3 {
            assert!(g.admit(&e));
        }
        assert!(!g.admit(&e));
        // 3024 is under the limit but above half of it (2048): closed until it falls to half.
        g.release(&e);
        assert_eq!(g.queued(), 2 * 1512);
        assert!(!g.admit(&e), "above half");
        g.release(&e);
        assert_eq!(g.queued(), 1512);
        assert!(g.admit(&e), "at most half");
        assert_eq!(g.notice().expect("a notice").output, notice_text(2000), "each refusal counted");
    }

    #[test]
    fn the_notice_is_important_counts_once_and_is_charged() {
        let g = ConsoleGate::new(1000);
        assert!(g.notice().is_none(), "nothing dropped yet");
        assert!(g.admit(&big(600)), "fills the gate");
        assert!(!g.admit(&big(700)));
        assert!(!g.admit(&big(300)));
        let n = g.notice().expect("a notice");
        assert_eq!(n.category.as_deref(), Some("important"));
        assert_eq!(n.output, notice_text(1000));
        assert!(g.notice().is_none(), "counted once");
        assert_eq!(g.queued(), 600 + 512 + n.output.len() + 512);
        g.release(&n);
        g.release(&big(600));
        assert_eq!(g.queued(), 0);
    }

    #[test]
    fn forward_output_table() {
        let msg = |body| DapMsg { generation: 7, session: SessionId(3), body };
        let run = |g: &ConsoleGate, m: DapMsg| {
            let sent = std::cell::RefCell::new(Vec::new());
            forward_output(g, m, |m| sent.borrow_mut().push(m));
            sent.into_inner()
        };
        let notice = |n: usize| msg(DapBody::Output(OutputEvent { category: Some("important".into()), output: notice_text(n) }));

        // A closed gate refuses output and lets everything else through, with no notice for them.
        let closed = ConsoleGate::new(0);
        assert!(run(&closed, msg(DapBody::Output(big(10)))).is_empty());
        assert_eq!(run(&closed, msg(DapBody::Exited(0))), [msg(DapBody::Exited(0))]);
        assert_eq!(closed.dropped.load(Ordering::SeqCst), 10, "the notice waits for a message that asks for it");

        // The end of the session carries the notice ahead of it, and only once.
        assert_eq!(
            run(&closed, msg(DapBody::Ended { reason: String::new() })),
            [notice(10), msg(DapBody::Ended { reason: String::new() })]
        );
        assert_eq!(run(&closed, msg(DapBody::Ended { reason: String::new() })), [msg(DapBody::Ended { reason: String::new() })]);

        // The first output admitted after a drop has the notice ahead of it, with its generation and session.
        let g = ConsoleGate::new(1000);
        let first = msg(DapBody::Output(big(600)));
        assert_eq!(run(&g, first.clone()), vec![first], "nothing dropped: no notice");
        assert!(run(&g, msg(DapBody::Output(big(20)))).is_empty());
        g.release(&big(600));
        let again = msg(DapBody::Output(big(30)));
        let sent = run(&g, again.clone());
        assert_eq!(sent, [notice(20), again]);
        assert_eq!((sent[0].generation, sent[0].session), (7, SessionId(3)));
    }

    #[test]
    fn the_console_thread_gives_every_charge_back() {
        let (tx_out, rx_out) = std::sync::mpsc::channel::<Vec<RunLine>>();
        let gate = Arc::new(ConsoleGate::new(MAX_QUEUED));
        let tx = spawn_console("/nonexistent".into(), "/nonexistent".into(), gate.clone(), move |l| {
            let _ = tx_out.send(l);
        });
        for i in 0..1000 {
            let e = out(Some("stdout"), &format!("line {i}\n"));
            assert!(gate.admit(&e));
            tx.send(e).unwrap();
        }
        drop(tx);
        // The thread has fed everything when it drops its end of this channel.
        while rx_out.recv_timeout(Duration::from_secs(5)).is_ok() {}
        assert_eq!(gate.queued(), 0);
    }
}
