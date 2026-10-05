//! The shell on a pseudo terminal (portable-pty), leading its own session
//! with the terminal as its controlling one.
//!
//! Three threads per shell: a reader moves output into an [`Inbox`] and
//! wakes the app with `AppMsg::TerminalOutput` only when the app has not
//! been woken yet, so one message carries everything read since the app
//! last looked, and it stops reading once `INBOX_MAX` bytes wait (the shell
//! then blocks on its writes, as in any terminal). A writer feeds input, so
//! a program that does not read cannot block the UI. A waiter reaps the
//! shell and reports its exit after the last output.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize};
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;

/// The reader stops while this much output waits for the app.
const INBOX_MAX: usize = 1 << 20;
/// How long the exit waits for the reader to see the end of the output.
const DRAIN: Duration = Duration::from_millis(300);

#[derive(Debug, Clone)]
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Added to jetl's environment (or to an empty one with `clear_env`).
    pub env: Vec<(String, String)>,
    /// Removed from jetl's environment.
    pub env_remove: Vec<String>,
    pub clear_env: bool,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Default)]
struct InboxState {
    bytes: Vec<u8>,
    /// A wake is on the bus and not yet drained.
    notified: bool,
    /// The reader saw the end of the output.
    eof: bool,
    exit: Option<ExitStatus>,
    /// The session is gone; the threads stop.
    closed: bool,
}

/// Output and the exit, shared by the threads and the app.
#[derive(Debug, Default)]
struct Inbox {
    state: Mutex<InboxState>,
    cv: Condvar,
}

impl Inbox {
    fn lock(&self) -> MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What arrived since the last [`Session::drain`].
#[derive(Debug, Default)]
pub struct Drained {
    pub bytes: Vec<u8>,
    /// The shell exited; all of its output came before.
    pub exit: Option<ExitStatus>,
}

pub struct Session {
    /// Messages of other sessions (an earlier shell) are dropped.
    pub id: u64,
    /// The shell's pid, which is also its session and process group id.
    pid: u32,
    /// The waiter reaped the shell, so `pid` may name another process.
    reaped: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
    inbox: Arc<Inbox>,
    killed: bool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("id", &self.id).field("pid", &self.pid).finish()
    }
}

impl Session {
    /// Start `spec` on a new pseudo terminal of its size. Wakes are tagged
    /// with `generation` and `id`.
    pub fn spawn(spec: &Spec, id: u64, generation: u64, tx: UnboundedSender<AppMsg>) -> Result<Session> {
        let name = spec.argv.first().filter(|n| !n.is_empty()).context("no shell")?;
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            rows: spec.rows.max(1),
            cols: spec.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(name);
        cmd.args(&spec.argv[1..]);
        cmd.cwd(&spec.cwd);
        if spec.clear_env {
            cmd.env_clear();
        }
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = pair.slave.spawn_command(cmd)?;
        // Only the shell holds the terminal's slave side, so its end (and
        // its children's) is the end of the output.
        drop(pair.slave);
        let master = pair.master;
        let pid = child.process_id().context("the shell exited at once")?;
        let mut reader = master.try_clone_reader()?;
        let mut writer = master.take_writer()?;
        let inbox = Arc::new(Inbox::default());
        let reaped = Arc::new(AtomicBool::new(false));
        let wake = move |st: &mut InboxState, tx: &UnboundedSender<AppMsg>| {
            if !st.notified {
                st.notified = true;
                let _ = tx.send(AppMsg::TerminalOutput { generation, id });
            }
        };

        let (inbox_r, tx_r) = (inbox.clone(), tx.clone());
        std::thread::Builder::new().name("jetl-term-read".into()).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // EIO once every process holding the terminal is gone.
                    Err(_) => break,
                };
                let mut st = inbox_r.lock();
                while st.bytes.len() >= INBOX_MAX && !st.closed {
                    st = inbox_r.cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                if st.closed {
                    return;
                }
                st.bytes.extend_from_slice(&buf[..n]);
                wake(&mut st, &tx_r);
            }
            inbox_r.lock().eof = true;
            inbox_r.cv.notify_all();
        })?;

        let (input, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name("jetl-term-write".into()).spawn(move || {
            for bytes in input_rx {
                if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })?;

        let (inbox_w, reaped_w) = (inbox.clone(), reaped.clone());
        std::thread::Builder::new().name("jetl-term-wait".into()).spawn(move || {
            let status = child.wait().unwrap_or_else(|_| ExitStatus::with_exit_code(1));
            reaped_w.store(true, Ordering::SeqCst);
            // Output still in the terminal comes first.
            let deadline = Instant::now() + DRAIN;
            let mut st = inbox_w.lock();
            while !st.eof && !st.closed {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
                st = inbox_w.cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
            }
            if st.closed {
                return;
            }
            st.exit = Some(status);
            wake(&mut st, &tx);
        })?;

        Ok(Session { id, pid, reaped, master, input, inbox, killed: false })
    }

    /// The shell's pid.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Queue `bytes` for the shell.
    pub fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.input.send(bytes);
        }
    }

    /// Resize the terminal; the kernel tells the foreground program
    /// (SIGWINCH).
    pub fn resize(&self, rows: u16, cols: u16) {
        if let Err(e) = self.master.resize(PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 }) {
            tracing::debug!("terminal resize: {e}");
        }
    }

    /// Take the output that arrived (and the exit, once all output is in),
    /// letting the reader go on and the next output wake the app again.
    pub fn drain(&self) -> Drained {
        let mut st = self.inbox.lock();
        st.notified = false;
        let d = Drained { bytes: std::mem::take(&mut st.bytes), exit: st.exit.take() };
        drop(st);
        self.inbox.cv.notify_all();
        d
    }

    /// SIGKILL every process of the shell's session: the shell, its jobs
    /// (each in its own process group under job control) and what they
    /// started, except programs that left for a new session (daemons,
    /// `setsid`). The threads end once the terminal closes.
    pub fn kill(&mut self) {
        if std::mem::replace(&mut self.killed, true) {
            return;
        }
        self.inbox.lock().closed = true;
        self.inbox.cv.notify_all();
        #[cfg(unix)]
        {
            let foreground = self.master.process_group_leader();
            kill_session(self.pid, !self.reaped.load(Ordering::SeqCst), foreground);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}

/// SIGKILL to the session `sid` leads: on Linux every process whose
/// session it is (from `/proc`), elsewhere the shell's process group and
/// the terminal's foreground one.
#[cfg(unix)]
fn kill_session(sid: u32, leader_alive: bool, foreground: Option<libc::pid_t>) {
    let Ok(sid) = libc::pid_t::try_from(sid) else { return };
    if sid <= 0 {
        return;
    }
    // SAFETY: kill and killpg take plain integers.
    unsafe {
        if leader_alive {
            libc::killpg(sid, libc::SIGKILL);
        }
        if let Some(fg) = foreground.filter(|p| *p > 0) {
            libc::killpg(fg, libc::SIGKILL);
        }
    }
    #[cfg(target_os = "linux")]
    for pid in session_members(sid) {
        // SAFETY: as above.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Processes whose session id is `sid`.
#[cfg(target_os = "linux")]
fn session_members(sid: libc::pid_t) -> Vec<libc::pid_t> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|s| stat_session(&s)) == Some(sid))
        .collect()
}

/// The session field of a `/proc/<pid>/stat` line: `pid (comm) state ppid
/// pgrp session ...`, where comm may hold spaces and parentheses.
#[cfg(target_os = "linux")]
fn stat_session(stat: &str) -> Option<libc::pid_t> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(3)?.parse().ok()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn stat_session_skips_odd_command_names() {
        assert_eq!(stat_session("42 (a) b (c) S 1 40 41 0 -1"), Some(41));
        assert_eq!(stat_session("42 (zsh) S 1 42 42 34816"), Some(42));
        assert_eq!(stat_session("garbage"), None);
    }
}
