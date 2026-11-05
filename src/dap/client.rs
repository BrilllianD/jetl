//! The driver of one debug session: it owns the process, the frames and the clock, and feeds `session::Session`, which
//! decides everything about order. It reports through a closure, so it needs no App. One `select!` over commands, incoming
//! frames, child exit and a tick. `Group` makes every way out of it kill the process group; EOF in particular
//! has no `Action::Kill`.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::DapMsg;
use super::protocol::Incoming;
use super::session::{Action, Command, Input, STOP_STAGE, Session, SessionId, SessionSpec};
use crate::lsp::client::Conn;
use crate::lsp::transport::{read_frame, write_frame};
use crate::run::process::kill_group;

/// What moves the machine's deadlines; they can be this late.
const TICK: Duration = Duration::from_millis(250);
/// How long frames are awaited after the adapter exited, when something outside its group holds its stdout open.
const CHILD_DRAIN: Duration = Duration::from_millis(500);
/// The most [`DapClient::shutdown`] waits: both stop stages, each up to a `TICK` late, and a margin.
pub const SHUTDOWN_LIMIT: Duration = Duration::from_secs(STOP_STAGE.as_secs() * 2 + 2);

/// The process group, while it is ours to signal; 0 when none, or once killed, so a reused id is never signalled.
/// Owned by the driver's future, so that a panic, an abort or the runtime's shutdown kill the group as well.
struct Group(Arc<AtomicU32>);

impl Group {
    fn kill(&self) {
        kill_group(self.0.swap(0, Ordering::SeqCst));
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The App's handle on a session. Dropping it kills the process group at once and reports nothing more;
/// [`DapClient::shutdown`] asks the adapter to stop first.
#[derive(Debug)]
pub struct DapClient {
    pid: Arc<AtomicU32>,
    cmds: UnboundedSender<Command>,
    driver: Option<JoinHandle<()>>,
}

impl DapClient {
    /// Start the driver over `conn`. `emit` runs on a runtime thread and must not block.
    pub fn start(spec: SessionSpec, conn: Conn, emit: impl Fn(DapMsg) + Send + 'static) -> Self {
        let pid = Arc::new(AtomicU32::new(conn.child.as_ref().and_then(|c| c.id()).unwrap_or(0)));
        let (cmds, rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(drive(spec, conn, rx, emit, Group(Arc::clone(&pid))));
        Self { pid, cmds, driver: Some(driver) }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.cmds.send(cmd);
    }

    /// The process group; `None` for an in-process adapter and once the group is killed.
    pub fn pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::SeqCst)).filter(|p| *p != 0)
    }

    /// Stop, then resolves when the driver ended ([`SHUTDOWN_LIMIT`] at most), and the group is killed either way.
    /// Do not send `Command::Stop` first: a second one skips a stage.
    pub fn shutdown(mut self) -> impl Future<Output = ()> + Send + 'static {
        let _ = self.cmds.send(Command::Stop);
        // Dropping `self` must neither kill the group nor close `cmds`, which ends the driver, before the adapter could stop.
        let pid = std::mem::take(&mut self.pid);
        let driver = self.driver.take();
        let cmds = self.cmds.clone();
        async move {
            if let Some(driver) = driver
                && tokio::time::timeout(SHUTDOWN_LIMIT, driver).await.is_err()
            {
                tracing::info!("dap: the adapter did not stop in time; killing it");
            }
            drop(cmds);
            kill_group(pid.swap(0, Ordering::SeqCst));
        }
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        kill_group(self.pid.swap(0, Ordering::SeqCst));
    }
}

/// Everything the loop acts on between awaits. `feed` and `apply` are synchronous on purpose: `emit` is not
/// `Sync`, so no reference to it may be held across an `.await`.
struct Driver<F> {
    session: Session,
    out: UnboundedSender<Value>,
    group: Group,
    emit: F,
    generation: u64,
    id: SessionId,
}

impl<F: Fn(DapMsg)> Driver<F> {
    /// True once the session is over.
    fn feed(&mut self, input: Input) -> bool {
        if let Input::Incoming(msg) = &input {
            log_incoming(self.id, msg);
        }
        let actions = self.session.on_input(input, Instant::now());
        self.apply(actions)
    }

    /// Carries out the machine's actions in order. True when they hold `Finish`, always the last.
    fn apply(&mut self, actions: Vec<Action>) -> bool {
        let mut finished = false;
        for action in actions {
            match action {
                Action::Send(msg) => {
                    tracing::debug!("dap {}: -> {}", self.id.0, msg.command());
                    let _ = self.out.send(msg.to_json());
                }
                Action::Emit(body) => (self.emit)(DapMsg { generation: self.generation, session: self.id, body }),
                Action::Kill => self.group.kill(),
                Action::Finish => finished = true,
            }
        }
        finished
    }
}

/// Names only: a body can hold what the launch arguments carried.
fn log_incoming(id: SessionId, msg: &Incoming) {
    match msg {
        Incoming::Response(r) => tracing::debug!("dap {}: <- response {} success {}", id.0, r.command, r.success),
        Incoming::Event(e) => tracing::debug!("dap {}: <- event {}", id.0, e.event),
        Incoming::Request(r) => tracing::debug!("dap {}: <- request {}", id.0, r.command),
    }
}

async fn drive<F: Fn(DapMsg)>(spec: SessionSpec, conn: Conn, mut cmds: UnboundedReceiver<Command>, emit: F, group: Group) {
    let Conn { reader, writer, mut child } = conn;
    let (in_tx, mut incoming) = mpsc::unbounded_channel();
    let (out, out_rx) = mpsc::unbounded_channel();
    let reader = spawn_reader(reader, in_tx);
    spawn_writer(writer, out_rx);
    let (generation, id) = (spec.generation, spec.id);
    let mut session = Session::new(spec, Instant::now());
    let first = session.start();
    let mut d = Driver { session, out, group, emit, generation, id };
    d.apply(first);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut drain: Option<Instant> = None;
    loop {
        // Not `biased`: a debuggee printing without pause must not starve a Stop.
        let finished = tokio::select! {
            msg = incoming.recv() => match msg {
                Some(msg) => d.feed(Input::Incoming(msg)),
                None => d.feed(Input::Eof),
            },
            cmd = cmds.recv() => match cmd {
                Some(cmd) => d.feed(Input::Command(cmd)),
                // The handle was dropped; nobody is listening.
                None => true,
            },
            _ = child_exit(&mut child), if child.is_some() => {
                child = None;
                // Its frames may still be in the pipe, so this is not EOF yet. What it left running goes with its
                // group, which closes the stdout those hold, and the reader gets to the real end.
                d.group.kill();
                drain = Some(Instant::now() + CHILD_DRAIN);
                false
            }
            _ = tick.tick() => {
                let due = drain.is_some_and(|at| Instant::now() >= at);
                d.feed(if due { Input::Eof } else { Input::Tick })
            }
        };
        if finished {
            break;
        }
    }
    // Kills the group first (EOF has no `Action::Kill`), since waiting for a live leader would hang, and closes the writer.
    drop(d);
    reader.abort();
    if let Some(mut c) = child {
        let _ = c.wait().await;
    }
}

async fn child_exit(child: &mut Option<Child>) {
    match child {
        Some(c) => match c.wait().await {
            Ok(status) => tracing::debug!("dap: the adapter exited: {status}"),
            Err(e) => tracing::warn!("dap: waiting for the adapter: {e}"),
        },
        None => std::future::pending().await,
    }
}

/// Frames until the stream ends or breaks; a body that does not parse is logged and skipped. Dropping `tx` is the EOF.
fn spawn_reader(reader: Box<dyn AsyncRead + Send + Unpin>, tx: UnboundedSender<Incoming>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = BufReader::new(reader);
        loop {
            match read_frame(&mut r).await {
                Ok(Some(body)) => match Incoming::parse(&body) {
                    Ok(msg) => {
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!("dap: unreadable message: {e}"),
                },
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!("dap: reading from the adapter: {e}");
                    return;
                }
            }
        }
    })
}

/// In its own task, so that an adapter not reading its stdin cannot block the loop.
fn spawn_writer(mut writer: Box<dyn AsyncWrite + Send + Unpin>, mut rx: UnboundedReceiver<Value>) {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_frame(&mut writer, &msg).await {
                tracing::warn!("dap: writing to the adapter: {e}");
                return;
            }
        }
        let _ = writer.shutdown().await;
    });
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::dap::DapBody;
    use crate::dap::fake::{self, FakeAdapter, Order};
    use crate::dap::session::{DapTag, Phase, ResumeKind, SourceBreakpoints, TagKind};

    const GEN: u64 = 3;
    const SESSION: SessionId = SessionId(7);
    const WAIT: Duration = Duration::from_secs(5);

    fn caps() -> Value {
        json!({
            "supportsConfigurationDoneRequest": true, "supportsTerminateRequest": true, "supportTerminateDebuggee": true,
            "exceptionBreakpointFilters": [{"filter": "on", "label": "On", "default": true}],
        })
    }

    fn spec(breakpoints: &[(&str, &[usize])]) -> SessionSpec {
        SessionSpec {
            id: SESSION,
            generation: GEN,
            adapter_id: "fake".into(),
            launch: json!({"program": "/p"}),
            breakpoints: breakpoints
                .iter()
                .map(|(p, l)| (PathBuf::from(p), SourceBreakpoints { lines: l.to_vec(), modified: false }))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn collector() -> (impl Fn(DapMsg) + Send + 'static, UnboundedReceiver<DapMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            move |m| {
                let _ = tx.send(m);
            },
            rx,
        )
    }

    async fn next_body(rx: &mut UnboundedReceiver<DapMsg>) -> DapBody {
        let msg = tokio::time::timeout(WAIT, rx.recv()).await.expect("a report in time").expect("the session is open");
        assert_eq!((msg.generation, msg.session), (GEN, SESSION));
        msg.body
    }

    /// Every body up to and including the first match.
    async fn until(rx: &mut UnboundedReceiver<DapMsg>, want: impl Fn(&DapBody) -> bool) -> Vec<DapBody> {
        let mut seen = Vec::new();
        loop {
            let body = next_body(rx).await;
            let hit = want(&body);
            seen.push(body);
            if hit {
                return seen;
            }
        }
    }

    /// Every body until the channel closes, 5 s between each.
    async fn rest(rx: &mut UnboundedReceiver<DapMsg>) -> Vec<DapBody> {
        let mut seen = Vec::new();
        while let Some(msg) = tokio::time::timeout(WAIT, rx.recv()).await.expect("the session to end") {
            assert_eq!((msg.generation, msg.session), (GEN, SESSION));
            seen.push(msg.body);
        }
        seen
    }

    fn commands(seen: &[Value]) -> Vec<String> {
        seen.iter().map(|m| m["command"].as_str().unwrap_or("").to_string()).collect()
    }

    /// Answers configuration requests as they come; returns each request up to and including `configurationDone`.
    async fn answer_configuration(adapter: &mut FakeAdapter) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let msg = adapter.next().await;
            match msg["command"].as_str() {
                Some("setBreakpoints") => {
                    let n = msg["arguments"]["breakpoints"].as_array().map_or(0, Vec::len);
                    let list: Vec<Value> = (0..n).map(|i| json!({"id": i + 1, "verified": true})).collect();
                    adapter.respond(&msg, json!({"breakpoints": list}));
                }
                Some("setExceptionBreakpoints") => adapter.respond(&msg, Value::Null),
                _ => {}
            }
            let done = msg["command"] == "configurationDone";
            seen.push(msg);
            if done {
                return seen;
            }
        }
    }

    async fn running(order: Order, c: Value) -> (DapClient, FakeAdapter, UnboundedReceiver<DapMsg>) {
        let (conn, mut adapter) = fake::connect(c, order);
        let (emit, mut rx) = collector();
        let client = DapClient::start(spec(&[]), conn, emit);
        answer_configuration(&mut adapter).await;
        until(&mut rx, |b| *b == DapBody::Phase(Phase::Running)).await;
        (client, adapter, rx)
    }

    #[tokio::test]
    async fn start_in_every_order() {
        for order in fake::ORDERS {
            let (conn, mut adapter) = fake::connect(caps(), order);
            let (emit, mut rx) = collector();
            let client = DapClient::start(spec(&[("/a.c", &[2, 4]), ("/b.c", &[0])]), conn, emit);

            // `launch` comes right after the initialize response. Under `LaunchDeferred` the fake has sent no
            // `initialized`, and under the first two orders no launch reply, so a client that waits for either
            // never gets this far and `next` panics after 5 s.
            let seen = answer_configuration(&mut adapter).await;
            assert_eq!(
                commands(&seen),
                ["initialize", "launch", "setBreakpoints", "setBreakpoints", "setExceptionBreakpoints", "configurationDone"],
                "{order:?}"
            );
            let seqs: Vec<i64> = seen.iter().map(|m| m["seq"].as_i64().unwrap()).collect();
            assert_eq!(seqs, [1, 2, 3, 4, 5, 6], "{order:?}");
            assert_eq!(seen[1]["arguments"], json!({"program": "/p"}), "{order:?}");
            assert_eq!(seen[2]["arguments"]["source"]["path"], "/a.c", "{order:?}");
            assert_eq!(seen[2]["arguments"]["breakpoints"], json!([{"line": 3}, {"line": 5}]), "{order:?}");
            assert_eq!(seen[3]["arguments"]["source"]["path"], "/b.c", "{order:?}");

            // The fake refuses a `configurationDone` it gets before `initialized`; that would be an `Error`, a stop, and
            // never `Running`, so reaching `Running` with no `Error` is the proof that none preceded it.
            let mut bodies = Vec::new();
            let is_bps = |b: &DapBody| matches!(b, DapBody::Breakpoints { .. });
            while !(bodies.contains(&DapBody::Phase(Phase::Running)) && bodies.iter().filter(|b| is_bps(b)).count() == 2) {
                bodies.push(next_body(&mut rx).await);
            }
            assert!(matches!(bodies[0], DapBody::Started { .. }), "{order:?}");

            client.send(Command::Stop);
            let terminate = adapter.recv("terminate").await;
            adapter.respond(&terminate, Value::Null);
            adapter.event("terminated", Value::Null);
            let disconnect = adapter.recv("disconnect").await;
            assert_eq!(disconnect["arguments"], json!({"terminateDebuggee": true}), "{order:?}");
            adapter.respond(&disconnect, Value::Null);
            bodies.extend(rest(&mut rx).await);

            let phases: Vec<Phase> = bodies.iter().filter_map(|b| if let DapBody::Phase(p) = b { Some(*p) } else { None }).collect();
            assert_eq!(phases, [Phase::Starting, Phase::Running, Phase::Stopping, Phase::Ended], "{order:?}");
            assert!(!bodies.iter().any(|b| matches!(b, DapBody::Error(_))), "{order:?}: {bodies:?}");
            assert_eq!(bodies.last(), Some(&DapBody::Ended { reason: String::new() }), "{order:?}");
            adapter.closed().await;
        }
    }

    #[tokio::test]
    async fn not_capable_of_configuration_done_runs_in_every_order() {
        for order in fake::ORDERS {
            let (conn, mut adapter) = fake::connect(json!({}), order);
            let (emit, mut rx) = collector();
            let _client = DapClient::start(spec(&[]), conn, emit);
            let mut seen = Vec::new();
            loop {
                let msg = adapter.next().await;
                if msg["command"] == "setExceptionBreakpoints" {
                    assert_eq!(msg["arguments"], json!({"filters": []}), "{order:?}");
                    adapter.respond(&msg, Value::Null);
                    seen.push(msg);
                    break;
                }
                seen.push(msg);
            }
            until(&mut rx, |b| *b == DapBody::Phase(Phase::Running)).await;
            assert!(!commands(&seen).contains(&"configurationDone".to_string()), "{order:?}");
            assert_eq!(commands(&seen).last().map(String::as_str), Some("setExceptionBreakpoints"), "{order:?}");
        }
    }

    #[tokio::test]
    async fn commands_and_events_go_through() {
        let (client, mut adapter, mut rx) = running(Order::LaunchDeferred, caps()).await;
        adapter.event("stopped", json!({"reason": "breakpoint", "threadId": 1}));
        let stopped = until(&mut rx, |b| matches!(b, DapBody::Stopped { .. })).await.pop().unwrap();
        assert!(matches!(&stopped, DapBody::Stopped { epoch: 1, event } if event.reason == "breakpoint"), "{stopped:?}");

        let tag = DapTag { epoch: 1, kind: TagKind::Threads };
        client.send(Command::Request { command: "threads".into(), arguments: Value::Null, tag: tag.clone() });
        let threads = adapter.recv("threads").await;
        adapter.respond(&threads, json!({"threads": [{"id": 1, "name": "t"}]}));
        let answer = until(&mut rx, |b| matches!(b, DapBody::Response { .. })).await.pop().unwrap();
        assert_eq!(answer, DapBody::Response { tag, result: Ok(json!({"threads": [{"id": 1, "name": "t"}]})) });

        client.send(Command::SetBreakpoints { source: PathBuf::from("/x.c"), lines: vec![4], modified: false });
        let bps = adapter.recv("setBreakpoints").await;
        assert_eq!(bps["arguments"]["breakpoints"], json!([{"line": 5}]));
        assert_eq!(bps["arguments"]["source"]["path"], "/x.c");

        client.send(Command::Resume { kind: ResumeKind::Next, thread: 1 });
        let next = adapter.recv("next").await;
        assert_eq!(next["arguments"]["threadId"], 1);
        assert_eq!(until(&mut rx, |b| matches!(b, DapBody::Running { .. })).await.pop(), Some(DapBody::Running { epoch: 2 }));

        adapter.event("output", json!({"category": "stdout", "output": "hi\n"}));
        let out = until(&mut rx, |b| matches!(b, DapBody::Output(_))).await.pop();
        assert!(matches!(out, Some(DapBody::Output(o)) if o.output == "hi\n"));
        adapter.event("exited", json!({"exitCode": 3}));
        assert_eq!(until(&mut rx, |b| matches!(b, DapBody::Exited(_))).await.pop(), Some(DapBody::Exited(3)));
    }

    #[tokio::test]
    async fn reverse_request_is_answered() {
        let (_client, mut adapter, _rx) = running(Order::InitializedFirst, caps()).await;
        let seq = adapter.request("runInTerminal", json!({}));
        let answer = adapter.response_to(seq).await;
        assert_eq!((answer["success"].as_bool(), answer["message"].as_str()), (Some(false), Some("not supported")));
        // One counter serves requests and answers: `running` sent four requests, so this answer is the fifth.
        assert_eq!(answer["seq"], 5, "{answer}");
    }

    #[tokio::test]
    async fn unparsable_frames_are_skipped() {
        let (_client, adapter, mut rx) = running(Order::InitializedFirst, caps()).await;
        adapter.raw(json!({"type": "bogus"}));
        adapter.raw(json!({"seq": 1}));
        adapter.event("output", json!({"output": "still here"}));
        assert!(matches!(next_body(&mut rx).await, DapBody::Output(_)));
    }

    #[tokio::test]
    async fn broken_frame_ends_the_session() {
        let (client_end, adapter_end) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_end);
        let (_adapter_read, mut adapter_write) = tokio::io::split(adapter_end);
        let conn = Conn { reader: Box::new(client_read), writer: Box::new(client_write), child: None };
        let (emit, mut rx) = collector();
        let _client = DapClient::start(spec(&[]), conn, emit);
        adapter_write.write_all(b"Content-Length: x\r\n\r\n").await.unwrap();
        assert_eq!(rest(&mut rx).await, [DapBody::Phase(Phase::Ended), DapBody::Ended { reason: "adapter exited".into() }]);
    }

    #[tokio::test]
    async fn eof_ends_the_session() {
        let (_client, mut adapter, mut rx) = running(Order::LaunchEarly, caps()).await;
        adapter.close();
        assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: "adapter exited".into() }));
        adapter.closed().await;
    }

    #[tokio::test]
    async fn eof_after_stop_is_a_normal_end() {
        let (client, mut adapter, mut rx) = running(Order::LaunchEarly, caps()).await;
        client.send(Command::Stop);
        adapter.recv("terminate").await;
        adapter.close();
        assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: String::new() }));
    }

    #[tokio::test]
    async fn shutdown_against_a_silent_adapter_resolves_within_its_bound() {
        let (client, mut adapter, mut rx) = running(Order::InitializedFirst, caps()).await;
        let started = std::time::Instant::now();
        let stop = tokio::spawn(client.shutdown());
        adapter.recv("terminate").await;
        adapter.recv("disconnect").await;
        stop.await.unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= 2 * STOP_STAGE, "{elapsed:?}");
        assert!(elapsed < SHUTDOWN_LIMIT + Duration::from_secs(1), "{elapsed:?}");
        // The machine ended it, not `SHUTDOWN_LIMIT`: that path closes the driver with no `Ended`.
        assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: String::new() }));
    }

    #[tokio::test]
    async fn shutdown_with_a_cooperating_adapter_is_quick() {
        let (client, mut adapter, mut rx) = running(Order::LaunchDeferred, caps()).await;
        let started = std::time::Instant::now();
        let stop = tokio::spawn(client.shutdown());
        let terminate = adapter.recv("terminate").await;
        adapter.respond(&terminate, Value::Null);
        adapter.event("terminated", Value::Null);
        let disconnect = adapter.recv("disconnect").await;
        adapter.respond(&disconnect, Value::Null);
        stop.await.unwrap();
        assert!(started.elapsed() < STOP_STAGE, "{:?}", started.elapsed());
        assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: String::new() }));
    }

    #[tokio::test]
    async fn shutdown_after_the_end_resolves_at_once() {
        let (client, adapter, mut rx) = running(Order::LaunchDeferred, caps()).await;
        adapter.close();
        rest(&mut rx).await;
        assert!(tokio::time::timeout(Duration::from_secs(1), client.shutdown()).await.is_ok());
    }

    #[tokio::test]
    async fn dropping_an_unpolled_shutdown_ends_the_driver() {
        let (client, mut adapter, mut rx) = running(Order::LaunchDeferred, caps()).await;
        drop(client.shutdown());
        rest(&mut rx).await;
        adapter.closed().await;
    }

    #[tokio::test]
    async fn drop_ends_the_driver_silently() {
        let (client, mut adapter, mut rx) = running(Order::LaunchDeferred, caps()).await;
        drop(client);
        assert_eq!(rest(&mut rx).await, []);
        adapter.closed().await;
    }

    #[tokio::test]
    async fn pid_is_none_in_process() {
        let (client, _adapter, _rx) = running(Order::LaunchDeferred, caps()).await;
        assert_eq!(client.pid(), None);
    }

    #[tokio::test]
    async fn emit_needs_only_send() {
        let (conn, _adapter) = fake::connect(caps(), Order::LaunchDeferred);
        let n = Cell::new(0);
        let _client = DapClient::start(spec(&[]), conn, move |_| n.set(n.get() + 1));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[cfg(target_os = "linux")]
    mod process {
        use std::path::Path;

        use super::*;
        use crate::lsp::client::spawn_process;

        fn sh(script: &str, args: &[&str]) -> Conn {
            let mut argv: Vec<String> = ["sh", "-c", script].map(String::from).to_vec();
            argv.extend(args.iter().map(|a| a.to_string()));
            spawn_process("test", &argv, Path::new("/tmp")).expect("sh starts")
        }

        fn frame(msg: &Value) -> String {
            let body = msg.to_string();
            format!("Content-Length: {}\r\n\r\n{body}", body.len())
        }

        /// Processes of group `pgid` that still run. A zombie does not count: it only waits for a reaper.
        fn group_members(pgid: u32) -> usize {
            let Ok(dir) = std::fs::read_dir("/proc") else { return 0 };
            dir.flatten()
                .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
                .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
                .filter(|stat| {
                    // `pid (comm) state ppid pgrp ...`; comm can hold spaces and parentheses.
                    let Some((_, rest)) = stat.rsplit_once(')') else { return false };
                    let mut f = rest.split_whitespace();
                    let (state, _ppid, pgrp) = (f.next(), f.next(), f.next());
                    !matches!(state, Some("Z" | "X")) && pgrp.and_then(|p| p.parse::<u32>().ok()) == Some(pgid)
                })
                .count()
        }

        async fn members_at_least(pgid: u32, n: usize) {
            let deadline = std::time::Instant::now() + WAIT;
            while group_members(pgid) < n {
                assert!(std::time::Instant::now() < deadline, "group {pgid} never had {n} members");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        async fn group_gone(pgid: u32) {
            let deadline = std::time::Instant::now() + WAIT;
            while group_members(pgid) > 0 {
                assert!(std::time::Instant::now() < deadline, "group {pgid} still runs");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        fn members_at_least_blocking(pgid: u32, n: usize) {
            let deadline = std::time::Instant::now() + WAIT;
            while group_members(pgid) < n {
                assert!(std::time::Instant::now() < deadline, "group {pgid} never had {n} members");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn group_gone_blocking(pgid: u32) {
            let deadline = std::time::Instant::now() + WAIT;
            while group_members(pgid) > 0 {
                assert!(std::time::Instant::now() < deadline, "group {pgid} still runs");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
        }

        fn ended(reason: &str) -> [DapBody; 2] {
            [DapBody::Phase(Phase::Ended), DapBody::Ended { reason: reason.into() }]
        }

        #[tokio::test]
        async fn child_exit_ends_the_session() {
            let (emit, mut rx) = collector();
            let client = DapClient::start(spec(&[]), sh("exit 3", &[]), emit);
            let bodies = rest(&mut rx).await;
            assert_eq!(bodies, ended("adapter exited"));
            assert_eq!(client.pid(), None);
        }

        #[tokio::test]
        async fn frames_written_before_the_exit_are_not_lost() {
            let mut frames = String::new();
            for i in 0..200 {
                frames += &frame(&json!({"seq": i + 1, "type": "event", "event": "output", "body": {"output": i.to_string()}}));
            }
            frames += &frame(&json!({"seq": 201, "type": "event", "event": "exited", "body": {"exitCode": 7}}));
            let (emit, mut rx) = collector();
            let _client = DapClient::start(spec(&[]), sh(r#"printf %s "$0"; exit 0"#, &[&frames]), emit);
            let bodies = rest(&mut rx).await;
            let outputs: Vec<String> =
                bodies.iter().filter_map(|b| if let DapBody::Output(o) = b { Some(o.output.clone()) } else { None }).collect();
            let want: Vec<String> = (0..200).map(|i| i.to_string()).collect();
            assert_eq!(outputs, want);
            let n = bodies.len();
            assert_eq!(bodies[n - 3], DapBody::Exited(7), "{:?}", &bodies[n.saturating_sub(5)..]);
            assert_eq!(bodies[n - 2..], ended("adapter exited"));
        }

        #[tokio::test]
        async fn child_exit_kills_a_survivor_holding_stdout() {
            let (emit, mut rx) = collector();
            let client = DapClient::start(spec(&[]), sh("sleep 100 & sleep 0.2; exit 0", &[]), emit);
            let pgid = client.pid().unwrap();
            assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: "adapter exited".into() }));
            group_gone(pgid).await;
        }

        #[tokio::test]
        async fn child_exit_with_an_outsider_holding_stdout_ends_by_the_deadline() {
            let (emit, mut rx) = collector();
            let _client = DapClient::start(spec(&[]), sh("setsid sleep 20 & sleep 0.2; exit 0", &[]), emit);
            assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: "adapter exited".into() }));
        }

        #[tokio::test]
        async fn eof_with_a_live_adapter_kills_the_group() {
            let (emit, mut rx) = collector();
            let client = DapClient::start(spec(&[]), sh("exec 1>&-; sleep 100 & sleep 100", &[]), emit);
            let pgid = client.pid().unwrap();
            assert_eq!(rest(&mut rx).await, ended("adapter exited"));
            group_gone(pgid).await;
            assert_eq!(client.pid(), None);
        }

        #[tokio::test]
        async fn drop_kills_the_group() {
            let (emit, _rx) = collector();
            let client = DapClient::start(spec(&[]), sh("sleep 100 & sleep 100", &[]), emit);
            let pgid = client.pid().unwrap();
            members_at_least(pgid, 2).await;
            drop(client);
            group_gone(pgid).await;
        }

        #[tokio::test]
        async fn stop_in_initializing_kills_the_group() {
            let (emit, mut rx) = collector();
            let client = DapClient::start(spec(&[]), sh("sleep 100 & sleep 100", &[]), emit);
            let pgid = client.pid().unwrap();
            members_at_least(pgid, 2).await;
            client.send(Command::Stop);
            assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: String::new() }));
            group_gone(pgid).await;
        }

        #[tokio::test]
        async fn shutdown_kills_a_process_that_never_answers() {
            let reply =
                frame(&json!({"seq": 1, "type": "response", "request_seq": 1, "success": true, "command": "initialize", "body": {}}));
            let (emit, mut rx) = collector();
            let client = DapClient::start(spec(&[]), sh(r#"printf %s "$0"; sleep 100 & sleep 100"#, &[&reply]), emit);
            let pgid = client.pid().unwrap();
            until(&mut rx, |b| *b == DapBody::Phase(Phase::Starting)).await;
            members_at_least(pgid, 2).await;
            let started = std::time::Instant::now();
            client.shutdown().await;
            assert!(started.elapsed() >= STOP_STAGE, "{:?}", started.elapsed());
            group_gone(pgid).await;
            assert_eq!(rest(&mut rx).await.last(), Some(&DapBody::Ended { reason: String::new() }));
        }

        /// The handle's `Drop` kills on the caller's thread, with nothing driving the runtime: an App dropped just before
        /// the process exits must not depend on the driver being scheduled.
        #[test]
        fn drop_kills_at_once_with_no_driver_running() {
            let rt = runtime();
            let client = rt.block_on(async { DapClient::start(spec(&[]), sh("sleep 100 & sleep 100", &[]), |_| {}) });
            let pgid = client.pid().unwrap();
            members_at_least_blocking(pgid, 2);
            drop(client); // `rt` is not running here
            group_gone_blocking(pgid);
        }

        /// The driver's own `Drop`: the runtime going away cancels the task, and `kill_on_drop` reaches the leader only.
        #[test]
        fn runtime_shutdown_kills_the_group() {
            let rt = runtime();
            let client = rt.block_on(async {
                let client = DapClient::start(spec(&[]), sh("sleep 100 & sleep 100", &[]), |_| {});
                tokio::time::sleep(Duration::from_millis(50)).await;
                client
            });
            let pgid = client.pid().unwrap();
            members_at_least_blocking(pgid, 2);
            drop(rt); // `client` is still alive
            group_gone_blocking(pgid);
            assert_eq!(client.pid(), None);
        }
    }
}
