//! The Debug window's Console, off the main thread. A debuggee can print 100 000 lines in a second, each an
//! `output` event, so the App only forwards events into a channel; one thread per session splits them into
//! lines, matches file locations (blocking `stat`s) and hands the window batches of at most one per `BATCH`.

use std::path::PathBuf;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::time::Instant;

use super::protocol::OutputEvent;
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

/// Starts the Console thread: events sent to the returned sender come out of `emit` as lines, in batches. The
/// thread ends when the sender is dropped, after flushing the held partial lines. `cwd` and `root` are where
/// relative locations are looked for.
pub(crate) fn spawn_console(cwd: PathBuf, root: PathBuf, emit: impl Fn(Vec<RunLine>) + Send + 'static) -> Sender<OutputEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<OutputEvent>();
    // A thread, not a runtime task: the matcher stats files.
    std::thread::spawn(move || {
        let mut feed = ConsoleFeed::new(Matcher::new(&cwd, &root));
        let mut pending: Vec<RunLine> = Vec::new();
        let mut last = Instant::now();
        loop {
            match rx.recv_timeout(BATCH) {
                Ok(e) => pending.extend(feed.feed(&e)),
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
        let tx = spawn_console("/nonexistent".into(), "/nonexistent".into(), move |l| {
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
}
