//! The DAP session as a pure state machine. Input and a caller-supplied `Instant` go in, `Action`s come out:
//! no IO, no async, no clock of its own. Every timer is a deadline compared with the `now` of `on_input`.
//!
//! Adapters disagree about when `initialized` and the launch response arrive. The one order that works
//! everywhere: `launch` (or `attach`) goes out right after the initialize response without waiting for its reply, and the
//! configuration requests run whenever `initialized` arrives.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::DapBody;
use super::config::StartRequest;
use super::protocol::{
    Breakpoint, BreakpointEvent, Capabilities, Event, ExceptionFilter, ExitedEvent, Incoming, Outgoing, OutputEvent, Response,
    ReverseRequest, StoppedEvent, body_of,
};

/// Applies to `initialize` alone; the session ends when it passes.
pub const INIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Every request but `initialize`, `launch` and the stop requests.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long each stage of a stop (`terminate`, then `disconnect`) waits before going on.
pub const STOP_STAGE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSpec {
    pub id: SessionId,
    pub generation: u64,
    /// `initialize`'s `adapterID`.
    pub adapter_id: String,
    /// Which request starts the session.
    pub request: StartRequest,
    /// The `launch` or `attach` arguments, passed through.
    pub launch: Value,
    /// Absolute paths.
    pub breakpoints: BTreeMap<PathBuf, SourceBreakpoints>,
    /// The user's saved exception filter choices for this adapter, by filter id; a declared id with none takes its default.
    pub exception_filters: BTreeMap<String, bool>,
}

/// The filter ids to send: each declared filter (in declared order, first of a repeated id, none empty) that `choices` turns on,
/// or with no choice its `default`. Never an id the adapter did not declare.
pub fn chosen_filters(declared: &[ExceptionFilter], choices: &BTreeMap<String, bool>) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for f in declared {
        if f.filter.is_empty() || seen.contains(&f.filter.as_str()) {
            continue;
        }
        seen.push(&f.filter);
        if choices.get(&f.filter).copied().unwrap_or(f.default) {
            out.push(f.filter.clone());
        }
    }
    out
}

/// One `setBreakpoints` entry: the 0-based line as jetl holds it, and what is sent with it. No field is `Some("")`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceBreakpoint {
    pub line: usize,
    pub condition: Option<String>,
    pub log_message: Option<String>,
    pub hit_condition: Option<String>,
}

impl SourceBreakpoint {
    pub fn at(line: usize) -> Self {
        Self { line, ..Self::default() }
    }
}

/// The entries of one source, lines ascending, one per line.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceBreakpoints {
    pub breakpoints: Vec<SourceBreakpoint>,
    pub modified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Initializing,
    Starting,
    Running,
    Suspended,
    Stopping,
    Ended,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Incoming(Incoming),
    Command(Command),
    Tick,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send(Outgoing),
    Emit(DapBody),
    /// Kill the process group now.
    Kill,
    /// The session is over: the driver ends. Comes after `Emit(Ended)`.
    Finish,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    SetBreakpoints {
        source: PathBuf,
        breakpoints: Vec<SourceBreakpoint>,
        modified: bool,
    },
    Request {
        command: String,
        arguments: Value,
        tag: DapTag,
    },
    Resume {
        kind: ResumeKind,
        thread: i64,
    },
    Pause {
        thread: i64,
    },
    /// The user's exception filter choices changed: replaces the stored ones, and tells the adapter once configured.
    SetExceptionFilters {
        choices: BTreeMap<String, bool>,
    },
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    Continue,
    Next,
    StepIn,
    StepOut,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DapTag {
    pub epoch: u64,
    pub kind: TagKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TagKind {
    Threads,
    /// gdb's start, proven or not: see `App::debug_start_answer`. `confirm` is the second ask.
    StartCheck {
        confirm: bool,
    },
    StackTrace {
        thread: i64,
    },
    /// The stack of a thread the user chose; `seq` is the panel's selection seq when it was asked.
    Frames {
        thread: i64,
        seq: u64,
    },
    /// The scopes of the selected frame; `seq` is the panel's selection seq when it was asked.
    Scopes {
        seq: u64,
    },
    /// The children of tree node `node`; `seq` is the panel's selection seq when it was asked.
    Variables {
        node: usize,
        seq: u64,
    },
    /// A watch's value: watch root `node` of the tree of selection seq `seq`, evaluated in the selected frame.
    Watch {
        node: usize,
        seq: u64,
    },
    /// The Evaluate dialog's one-shot, shown in the Console; `seq` is the selection seq when it was asked.
    Evaluate {
        seq: u64,
        expression: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum PendingKind {
    Initialize,
    /// The `launch` or `attach` request.
    Launch,
    SetBreakpoints {
        source: PathBuf,
        requested: Vec<usize>,
    },
    SetExceptionBreakpoints,
    ConfigurationDone,
    /// A step, `continue` or `pause`; the epoch after its own bump. `resumed` is the thread of a step or `continue`,
    /// `None` for a `pause`.
    Step {
        epoch: u64,
        resumed: Option<i64>,
    },
    /// Whether the program is stopped after a refused step or `continue`: a refusal does not say so.
    /// Under gdb, `finish` in the outermost frame leaves it stopped, and `Command aborted.` leaves it `notStopped`.
    StopCheck {
        epoch: u64,
        thread: i64,
    },
    Tagged(DapTag),
    Terminate,
    Disconnect,
}

#[derive(Debug)]
struct Pending {
    command: String,
    kind: PendingKind,
    sent: Instant,
}

impl Pending {
    fn timeout(&self) -> Option<Duration> {
        match self.kind {
            PendingKind::Initialize => Some(INIT_TIMEOUT),
            // `launch` waits for a build or for `configurationDone`; the stop stages have their own deadlines.
            PendingKind::Launch | PendingKind::Terminate | PendingKind::Disconnect => None,
            _ => Some(REQUEST_TIMEOUT),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StopStage {
    Terminate { deadline: Instant },
    Disconnect { deadline: Instant },
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SetBreakpointsBody {
    breakpoints: Vec<Breakpoint>,
}

#[derive(Debug)]
pub struct Session {
    spec: SessionSpec,
    created: Instant,
    phase: Phase,
    epoch: u64,
    /// Starts at 1; requests and reverse answers share it.
    next_seq: i64,
    caps: Capabilities,
    /// The first `initialized` arrived.
    initialized: bool,
    /// The configuration requests went out.
    configured: bool,
    /// `configurationDone` answered or timed out, or not capable and past the point it would go out.
    config_done: bool,
    launch_answered: bool,
    terminated: bool,
    /// The user's `Stop` or an earlier failure began or joined this stop, so a failure of a request it cancelled
    /// is not shown again. An adapter's `terminated` alone does not set it.
    stop_silent: bool,
    stop: Option<StopStage>,
    /// Expired in `seq` order, deterministic.
    pending: BTreeMap<i64, Pending>,
    newest_breakpoints: BTreeMap<PathBuf, i64>,
    /// Seq of the newest `setExceptionBreakpoints`; an older one's failure or timeout is dropped.
    newest_exception_filters: Option<i64>,
    /// The warning that a condition or log message was left out has been shown.
    spec_warned: bool,
    /// The newest stop's event, given again when a refused resume returns to it.
    last_stop: StoppedEvent,
}

impl Session {
    pub fn new(spec: SessionSpec, now: Instant) -> Self {
        Self {
            spec,
            created: now,
            phase: Phase::Initializing,
            epoch: 0,
            next_seq: 1,
            caps: Capabilities::default(),
            initialized: false,
            configured: false,
            config_done: false,
            launch_answered: false,
            terminated: false,
            stop_silent: false,
            stop: None,
            pending: BTreeMap::new(),
            newest_breakpoints: BTreeMap::new(),
            newest_exception_filters: None,
            spec_warned: false,
            last_stop: StoppedEvent::default(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Sends `initialize`. `Initializing` is the initial state, so no `Phase` is emitted.
    pub fn start(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        let args = json!({
            "adapterID": self.spec.adapter_id,
            "clientID": "jetl",
            "clientName": "jetl",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "pathFormat": "path",
            "supportsVariableType": true,
            "supportsVariablePaging": true,
        });
        self.send("initialize", args, PendingKind::Initialize, self.created, &mut out);
        out
    }

    pub fn on_input(&mut self, input: Input, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if self.phase == Phase::Ended {
            return out;
        }
        match input {
            Input::Incoming(Incoming::Response(r)) => self.on_response(r, now, &mut out),
            Input::Incoming(Incoming::Event(e)) => self.on_event(e, now, &mut out),
            Input::Incoming(Incoming::Request(r)) => self.answer_reverse(r, &mut out),
            Input::Command(c) => self.on_command(c, now, &mut out),
            Input::Tick => {}
            Input::Eof => {
                let reason = if self.terminated || self.stop.is_some() { "" } else { "adapter exited" };
                self.end(reason, false, &mut out);
            }
        }
        if self.phase != Phase::Ended {
            self.expire(now, &mut out);
        }
        out
    }

    fn send(&mut self, command: &str, arguments: Value, kind: PendingKind, now: Instant, out: &mut Vec<Action>) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.insert(seq, Pending { command: command.to_string(), kind, sent: now });
        out.push(Action::Send(Outgoing::Request { seq, command: command.to_string(), arguments }));
        seq
    }

    fn set_phase(&mut self, phase: Phase, out: &mut Vec<Action>) {
        if self.phase != phase {
            self.phase = phase;
            out.push(Action::Emit(DapBody::Phase(phase)));
        }
    }

    fn end(&mut self, reason: &str, kill: bool, out: &mut Vec<Action>) {
        if kill {
            out.push(Action::Kill);
        }
        self.phase = Phase::Ended;
        self.pending.clear();
        self.stop = None;
        out.push(Action::Emit(DapBody::Phase(Phase::Ended)));
        out.push(Action::Emit(DapBody::Ended { reason: reason.to_string() }));
        out.push(Action::Finish);
    }

    /// Moves to `Running` once both `configurationDone` and `launch` are answered. Only `Starting` moves: an
    /// early `stopped` has left it.
    fn maybe_running(&mut self, out: &mut Vec<Action>) {
        if self.phase == Phase::Starting && self.config_done && self.launch_answered {
            self.set_phase(Phase::Running, out);
        }
    }

    fn configure(&mut self, now: Instant, out: &mut Vec<Action>) {
        self.configured = true;
        for (path, sb) in std::mem::take(&mut self.spec.breakpoints) {
            self.send_set_breakpoints(path, sb.breakpoints, sb.modified, now, out);
        }
        let capable = self.caps.supports_configuration_done_request;
        if !self.caps.exception_breakpoint_filters.is_empty() || !capable {
            let filters = chosen_filters(&self.caps.exception_breakpoint_filters, &self.spec.exception_filters);
            self.send_set_exception_breakpoints(filters, now, out);
        }
        if capable {
            self.send("configurationDone", Value::Null, PendingKind::ConfigurationDone, now, out);
        } else {
            self.config_done = true;
            self.maybe_running(out);
        }
    }

    fn send_set_breakpoints(
        &mut self,
        source: PathBuf,
        breakpoints: Vec<SourceBreakpoint>,
        modified: bool,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut left_out = false;
        let entries: Vec<Value> = breakpoints
            .iter()
            .map(|b| {
                let mut e = json!({ "line": b.line + 1 });
                if let Some(c) = &b.condition {
                    if self.caps.supports_conditional_breakpoints {
                        e["condition"] = json!(c);
                    } else {
                        left_out = true;
                    }
                }
                if let Some(m) = &b.log_message {
                    if self.caps.supports_log_points {
                        e["logMessage"] = json!(m);
                    } else {
                        left_out = true;
                    }
                }
                if let Some(h) = &b.hit_condition {
                    if self.caps.supports_hit_conditional_breakpoints {
                        e["hitCondition"] = json!(h);
                    } else {
                        left_out = true;
                    }
                }
                e
            })
            .collect();
        if left_out && !self.spec_warned {
            self.spec_warned = true;
            out.push(Action::Emit(DapBody::Error(
                "The debug adapter does not support breakpoint conditions, hit counts or log messages: those breakpoints stop every time"
                    .into(),
            )));
        }
        let lines: Vec<usize> = breakpoints.iter().map(|b| b.line).collect();
        let args = json!({
            "source": { "name": name, "path": source.to_string_lossy() },
            "breakpoints": entries,
            "sourceModified": modified,
        });
        let kind = PendingKind::SetBreakpoints { source: source.clone(), requested: lines };
        let seq = self.send("setBreakpoints", args, kind, now, out);
        self.newest_breakpoints.insert(source, seq);
    }

    fn send_set_exception_breakpoints(&mut self, filters: Vec<String>, now: Instant, out: &mut Vec<Action>) {
        let seq = self.send("setExceptionBreakpoints", json!({ "filters": filters }), PendingKind::SetExceptionBreakpoints, now, out);
        self.newest_exception_filters = Some(seq);
    }

    fn is_newest(&self, source: &PathBuf, seq: i64) -> bool {
        self.newest_breakpoints.get(source) == Some(&seq)
    }

    fn begin_stop(&mut self, now: Instant, out: &mut Vec<Action>) {
        // `terminate` makes gdb kill what it is attached to, so an attach goes straight to `disconnect`.
        if self.caps.supports_terminate_request && self.spec.request == StartRequest::Launch {
            self.send("terminate", Value::Null, PendingKind::Terminate, now, out);
            self.stop = Some(StopStage::Terminate { deadline: now + STOP_STAGE });
            self.set_phase(Phase::Stopping, out);
        } else {
            self.disconnect(now, out);
        }
    }

    /// `process <pid>` of the attach, or `the process` when it attaches by something else.
    fn attach_target(&self) -> String {
        match self.spec.launch["pid"].as_i64() {
            Some(pid) => format!("process {pid}"),
            None => "the process".into(),
        }
    }

    /// The only place `disconnect` is built.
    fn disconnect(&mut self, now: Instant, out: &mut Vec<Action>) {
        let args = match self.spec.request {
            StartRequest::Attach => json!({ "terminateDebuggee": false }),
            StartRequest::Launch if self.caps.support_terminate_debuggee => json!({ "terminateDebuggee": true }),
            StartRequest::Launch => json!({}),
        };
        self.send("disconnect", args, PendingKind::Disconnect, now, out);
        self.stop = Some(StopStage::Disconnect { deadline: now + STOP_STAGE });
        self.set_phase(Phase::Stopping, out);
    }

    /// Emits `Error(msg)` unless the stop in progress is already explained: the user's stop, or a failure that began
    /// it. Returns whether it emitted. A stop the adapter began with `terminated` explains nothing.
    fn report(&mut self, msg: String, out: &mut Vec<Action>) -> bool {
        let stopping = self.phase == Phase::Stopping;
        if stopping && self.stop_silent {
            return false;
        }
        out.push(Action::Emit(DapBody::Error(msg)));
        true
    }

    /// A failed `launch` or `configurationDone`: shown, then the session stops unless it already is.
    fn fail_start(&mut self, msg: String, now: Instant, out: &mut Vec<Action>) {
        let stopping = self.phase == Phase::Stopping;
        if self.report(msg, out) && !stopping {
            self.stop_silent = true;
            self.begin_stop(now, out);
        }
    }

    fn answer_reverse(&mut self, r: ReverseRequest, out: &mut Vec<Action>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        out.push(Action::Send(Outgoing::Response {
            seq,
            request_seq: r.seq,
            success: false,
            command: r.command,
            message: Some("not supported".into()),
        }));
    }

    fn on_response(&mut self, r: Response, now: Instant, out: &mut Vec<Action>) {
        let Some(p) = self.pending.remove(&r.request_seq) else { return };
        match p.kind {
            PendingKind::Initialize => {
                if !r.success {
                    let reason = format!("initialize failed: {}", r.error_text());
                    self.end(&reason, true, out);
                    return;
                }
                self.caps = body_of("initialize", r.body);
                out.push(Action::Emit(DapBody::Started { capabilities: Box::new(self.caps.clone()) }));
                self.set_phase(Phase::Starting, out);
                let launch = std::mem::take(&mut self.spec.launch);
                self.send(self.spec.request.command(), launch, PendingKind::Launch, now, out);
                if self.initialized {
                    self.configure(now, out);
                }
            }
            PendingKind::Launch | PendingKind::ConfigurationDone if !r.success => self.fail_start(r.error_text(), now, out),
            PendingKind::Launch => {
                self.launch_answered = true;
                self.maybe_running(out);
            }
            PendingKind::ConfigurationDone => {
                self.config_done = true;
                self.maybe_running(out);
            }
            PendingKind::SetBreakpoints { source, requested } => {
                if self.phase == Phase::Stopping || !self.is_newest(&source, r.request_seq) {
                    return;
                }
                if !r.success {
                    out.push(Action::Emit(DapBody::Error(format!("setBreakpoints: {}", r.error_text()))));
                    return;
                }
                let body: SetBreakpointsBody = body_of("setBreakpoints", r.body);
                out.push(Action::Emit(DapBody::Breakpoints { source, requested, result: body.breakpoints }));
            }
            PendingKind::SetExceptionBreakpoints => {
                if !r.success && self.newest_exception_filters == Some(r.request_seq) {
                    self.report(format!("setExceptionBreakpoints: {}", r.error_text()), out);
                }
            }
            PendingKind::Step { epoch, resumed } => {
                if !r.success && epoch == self.epoch {
                    self.report(format!("{}: {}", p.command, r.error_text()), out);
                    if let Some(thread) = resumed
                        && self.phase == Phase::Running
                    {
                        let arguments = json!({"threadId": thread, "startFrame": 0, "levels": 1});
                        self.send("stackTrace", arguments, PendingKind::StopCheck { epoch, thread }, now, out);
                    }
                }
            }
            PendingKind::StopCheck { epoch, thread } => {
                if r.success && epoch == self.epoch && self.phase == Phase::Running {
                    self.epoch += 1;
                    self.set_phase(Phase::Suspended, out);
                    let event = StoppedEvent { thread_id: Some(thread), ..self.last_stop.clone() };
                    out.push(Action::Emit(DapBody::Stopped { epoch: self.epoch, event }));
                }
            }
            PendingKind::Tagged(tag) => {
                if tag.epoch < self.epoch {
                    return;
                }
                let result = if r.success { Ok(r.body) } else { Err(r.error_text()) };
                out.push(Action::Emit(DapBody::Response { tag, result }));
            }
            PendingKind::Terminate => {}
            PendingKind::Disconnect => self.end("", true, out),
        }
    }

    fn on_event(&mut self, e: Event, now: Instant, out: &mut Vec<Action>) {
        let live = !matches!(self.phase, Phase::Initializing | Phase::Stopping);
        match e.event.as_str() {
            "initialized" => {
                if self.initialized {
                    return;
                }
                self.initialized = true;
                if live {
                    self.configure(now, out);
                }
            }
            "stopped" if live => {
                self.epoch += 1;
                self.set_phase(Phase::Suspended, out);
                let event: StoppedEvent = body_of("stopped", e.body);
                self.last_stop = event.clone();
                out.push(Action::Emit(DapBody::Stopped { epoch: self.epoch, event }));
            }
            "continued" if live => {
                self.epoch += 1;
                if self.phase == Phase::Suspended {
                    self.set_phase(Phase::Running, out);
                }
                out.push(Action::Emit(DapBody::Running { epoch: self.epoch }));
            }
            "terminated" => self.on_terminated(now, out),
            "exited" => {
                let ev: ExitedEvent = body_of("exited", e.body);
                out.push(Action::Emit(DapBody::Exited(ev.exit_code)));
            }
            "process" => out.push(Action::Emit(DapBody::Process(body_of("process", e.body)))),
            "output" => {
                let ev: OutputEvent = body_of("output", e.body);
                if ev.category.as_deref() != Some("telemetry") {
                    out.push(Action::Emit(DapBody::Output(ev)));
                }
            }
            "breakpoint" => {
                let ev: BreakpointEvent = body_of("breakpoint", e.body);
                out.push(Action::Emit(DapBody::BreakpointChanged(ev)));
            }
            _ => {}
        }
    }

    fn on_terminated(&mut self, now: Instant, out: &mut Vec<Action>) {
        self.terminated = true;
        match (self.phase, self.stop) {
            (Phase::Initializing, _) => self.end("", true, out),
            (Phase::Stopping, Some(StopStage::Terminate { .. })) => self.disconnect(now, out),
            // Already disconnecting: only recorded.
            (Phase::Stopping, _) => {}
            _ => self.disconnect(now, out),
        }
    }

    fn on_command(&mut self, c: Command, now: Instant, out: &mut Vec<Action>) {
        match c {
            Command::SetBreakpoints { source, breakpoints, modified } => {
                if self.phase == Phase::Stopping {
                    return;
                }
                if self.configured {
                    self.send_set_breakpoints(source, breakpoints, modified, now, out);
                } else {
                    self.spec.breakpoints.insert(source, SourceBreakpoints { breakpoints, modified });
                }
            }
            Command::Request { command, arguments, tag } => {
                if tag.epoch < self.epoch {
                    return;
                }
                if matches!(self.phase, Phase::Initializing | Phase::Stopping) {
                    out.push(Action::Emit(DapBody::Response { tag, result: Err("not available".into()) }));
                    return;
                }
                self.send(&command, arguments, PendingKind::Tagged(tag), now, out);
            }
            Command::Resume { kind, thread } => {
                if self.phase != Phase::Suspended {
                    return;
                }
                self.epoch += 1;
                let command = match kind {
                    ResumeKind::Continue => "continue",
                    ResumeKind::Next => "next",
                    ResumeKind::StepIn => "stepIn",
                    ResumeKind::StepOut => "stepOut",
                };
                let kind = PendingKind::Step { epoch: self.epoch, resumed: Some(thread) };
                self.send(command, json!({ "threadId": thread }), kind, now, out);
                self.set_phase(Phase::Running, out);
                out.push(Action::Emit(DapBody::Running { epoch: self.epoch }));
            }
            Command::Pause { thread } => {
                if self.phase != Phase::Running {
                    return;
                }
                self.epoch += 1;
                let kind = PendingKind::Step { epoch: self.epoch, resumed: None };
                self.send("pause", json!({ "threadId": thread }), kind, now, out);
            }
            Command::SetExceptionFilters { choices } => {
                if self.phase == Phase::Stopping {
                    return;
                }
                self.spec.exception_filters = choices;
                if self.configured && !self.caps.exception_breakpoint_filters.is_empty() {
                    let filters = chosen_filters(&self.caps.exception_breakpoint_filters, &self.spec.exception_filters);
                    self.send_set_exception_breakpoints(filters, now, out);
                }
            }
            Command::Stop => {
                self.stop_silent = true;
                match (self.phase, self.stop) {
                    (Phase::Initializing, _) => self.end("", true, out),
                    (Phase::Stopping, Some(StopStage::Terminate { .. })) => self.disconnect(now, out),
                    // Killing gdb while it is attached can leave a breakpoint's `int3` in the user's process.
                    (Phase::Stopping, Some(StopStage::Disconnect { .. })) if self.spec.request == StartRequest::Attach => {
                        out.push(Action::Emit(DapBody::Info(format!("Detaching from {}...", self.attach_target()))));
                    }
                    (Phase::Stopping, _) => self.end("", true, out),
                    _ => self.begin_stop(now, out),
                }
            }
        }
    }

    /// Runs after every input, so an answer that arrives with a late `now` beats its own timeout.
    fn expire(&mut self, now: Instant, out: &mut Vec<Action>) {
        match self.stop {
            Some(StopStage::Terminate { deadline }) if now >= deadline => self.disconnect(now, out),
            Some(StopStage::Disconnect { deadline }) if now >= deadline => {
                if self.spec.request == StartRequest::Attach {
                    let target = self.attach_target();
                    out.push(Action::Emit(DapBody::Warning(format!(
                        "gdb did not detach in time and was stopped; {target} may have been stopped with it"
                    ))));
                }
                self.end("", true, out);
                return;
            }
            _ => {}
        }
        let due: Vec<i64> =
            self.pending.iter().filter(|(_, p)| p.timeout().is_some_and(|d| now >= p.sent + d)).map(|(seq, _)| *seq).collect();
        for seq in due {
            let Some(p) = self.pending.remove(&seq) else { continue };
            let stopping = self.phase == Phase::Stopping;
            match p.kind {
                PendingKind::Initialize => {
                    self.end("initialize timed out", true, out);
                    return;
                }
                PendingKind::SetBreakpoints { source, .. } => {
                    if !stopping && self.is_newest(&source, seq) {
                        out.push(Action::Emit(DapBody::Error("setBreakpoints timed out".into())));
                    }
                }
                PendingKind::SetExceptionBreakpoints => {
                    if self.newest_exception_filters == Some(seq) {
                        self.report("setExceptionBreakpoints timed out".into(), out);
                    }
                }
                PendingKind::ConfigurationDone => {
                    self.report("configurationDone timed out".into(), out);
                    self.config_done = true;
                    self.maybe_running(out);
                }
                PendingKind::Step { epoch, .. } => {
                    if epoch == self.epoch {
                        self.report(format!("{} timed out", p.command), out);
                    }
                }
                // The refusal was already shown and the state is unknown, so it stays `Running`.
                PendingKind::StopCheck { .. } => {}
                PendingKind::Tagged(tag) => {
                    if tag.epoch >= self.epoch {
                        out.push(Action::Emit(DapBody::Response { tag, result: Err("timed out".into()) }));
                    }
                }
                // Unreachable: `timeout` is `None` for these.
                PendingKind::Launch | PendingKind::Terminate | PendingKind::Disconnect => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::dap::protocol::ProcessEvent;

    fn caps() -> Value {
        json!({
            "supportsConfigurationDoneRequest": true, "supportsTerminateRequest": true, "supportTerminateDebuggee": true,
            "exceptionBreakpointFilters": [{"filter": "on", "label": "On", "default": true}, {"filter": "off", "label": "Off"}],
        })
    }

    fn at(lines: &[usize]) -> Vec<SourceBreakpoint> {
        lines.iter().map(|l| SourceBreakpoint::at(*l)).collect()
    }

    fn spec_caps(conditions: bool, log_points: bool) -> Value {
        let mut c = caps();
        c["supportsConditionalBreakpoints"] = json!(conditions);
        c["supportsLogPoints"] = json!(log_points);
        c
    }

    fn with_specs() -> Vec<SourceBreakpoint> {
        vec![
            SourceBreakpoint { line: 2, condition: Some("i == 3".into()), ..Default::default() },
            SourceBreakpoint { line: 4, log_message: Some("i={i}".into()), ..Default::default() },
        ]
    }

    const NO_SPEC_SUPPORT: &str =
        "The debug adapter does not support breakpoint conditions, hit counts or log messages: those breakpoints stop every time";

    #[test]
    fn a_spec_goes_on_the_wire_when_declared() {
        let mut h = H::new(&[]);
        h.answer("initialize", spec_caps(true, true));
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: with_specs(), modified: false });
        assert_eq!(
            h.requests("setBreakpoints")[0]["arguments"]["breakpoints"],
            json!([{"line": 3, "condition": "i == 3"}, {"line": 5, "logMessage": "i={i}"}])
        );
        assert!(h.errors().is_empty());
        h.answer("setBreakpoints", json!({"breakpoints": [{"verified": true}, {"verified": true}]}));
        let Some(DapBody::Breakpoints { requested, .. }) = h.emitted.iter().find(|b| matches!(b, DapBody::Breakpoints { .. })) else {
            panic!("no answer emitted")
        };
        assert_eq!(requested, &vec![2, 4]);
    }

    #[test]
    fn an_undeclared_spec_is_left_out_and_said_once() {
        let mut h = H::new(&[]);
        h.answer("initialize", spec_caps(false, false));
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: with_specs(), modified: false });
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([{"line": 3}, {"line": 5}]));
        assert_eq!(h.errors(), vec![NO_SPEC_SUPPORT.to_string()]);
        h.cmd(Command::SetBreakpoints { source: "/b.c".into(), breakpoints: with_specs(), modified: false });
        assert_eq!(h.requests("setBreakpoints")[1]["arguments"]["breakpoints"], json!([{"line": 3}, {"line": 5}]));
        assert_eq!(h.errors().len(), 1, "said once per session");
        // One capability declared: only the other field is left out.
        let mut h = H::new(&[]);
        h.answer("initialize", spec_caps(true, false));
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: with_specs(), modified: false });
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([{"line": 3, "condition": "i == 3"}, {"line": 5}]));
        assert_eq!(h.errors(), vec![NO_SPEC_SUPPORT.to_string()]);
    }

    fn hit_count_entry() -> Vec<SourceBreakpoint> {
        vec![SourceBreakpoint { line: 2, hit_condition: Some("3".into()), ..Default::default() }]
    }

    #[test]
    fn a_hit_count_goes_on_the_wire_when_declared() {
        let mut h = H::new(&[]);
        let mut c = caps();
        c["supportsHitConditionalBreakpoints"] = json!(true);
        h.answer("initialize", c);
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: hit_count_entry(), modified: false });
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([{"line": 3, "hitCondition": "3"}]));
        assert!(h.errors().is_empty());
    }

    #[test]
    fn an_undeclared_hit_count_is_left_out_and_said_once() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: hit_count_entry(), modified: false });
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([{"line": 3}]));
        assert_eq!(h.errors(), vec![NO_SPEC_SUPPORT.to_string()]);
        h.cmd(Command::SetBreakpoints { source: "/b.c".into(), breakpoints: hit_count_entry(), modified: false });
        assert_eq!(h.requests("setBreakpoints")[1]["arguments"]["breakpoints"], json!([{"line": 3}]));
        assert_eq!(h.errors().len(), 1, "said once per session");
    }

    #[test]
    fn a_plain_entry_has_only_its_line() {
        let mut h = H::new(&[("/a.c", &[1, 6])]);
        h.answer("initialize", spec_caps(true, true));
        h.initialized();
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([{"line": 2}, {"line": 7}]));
        assert!(h.errors().is_empty(), "nothing was left out");
    }

    const MS: Duration = Duration::from_millis(1);
    const HOUR: Duration = Duration::from_secs(3600);

    struct H {
        s: Session,
        now: Instant,
        sent: Vec<Value>,
        emitted: Vec<DapBody>,
        last: Vec<Action>,
        answered: BTreeSet<i64>,
    }

    impl H {
        fn new(breakpoints: &[(&str, &[usize])]) -> Self {
            Self::with(breakpoints, StartRequest::Launch)
        }

        fn attach(breakpoints: &[(&str, &[usize])]) -> Self {
            Self::with(breakpoints, StartRequest::Attach)
        }

        fn with(breakpoints: &[(&str, &[usize])], request: StartRequest) -> Self {
            let now = Instant::now();
            let spec = SessionSpec {
                id: SessionId(1),
                generation: 1,
                adapter_id: "dbg".into(),
                request,
                launch: json!({"program": "/p"}),
                breakpoints: breakpoints
                    .iter()
                    .map(|(p, l)| (PathBuf::from(p), SourceBreakpoints { breakpoints: at(l), modified: false }))
                    .collect(),
                exception_filters: BTreeMap::new(),
            };
            let mut h = H { s: Session::new(spec, now), now, sent: vec![], emitted: vec![], last: vec![], answered: BTreeSet::new() };
            let out = h.s.start();
            h.record(out);
            h
        }

        fn record(&mut self, out: Vec<Action>) {
            for a in &out {
                match a {
                    Action::Send(o) => self.sent.push(o.to_json()),
                    Action::Emit(b) => self.emitted.push(b.clone()),
                    _ => {}
                }
            }
            self.last = out;
        }

        fn feed(&mut self, input: Input) {
            let out = self.s.on_input(input, self.now);
            self.record(out);
        }

        fn cmd(&mut self, c: Command) {
            self.feed(Input::Command(c));
        }

        fn raw(&mut self, v: Value) {
            let msg = Incoming::parse(v.to_string().as_bytes()).unwrap();
            self.feed(Input::Incoming(msg));
        }

        fn event(&mut self, name: &str, body: Value) {
            self.raw(json!({"type": "event", "seq": 0, "event": name, "body": body}));
        }

        fn reverse(&mut self, command: &str, seq: i64) {
            self.raw(json!({"type": "request", "seq": seq, "command": command, "arguments": {}}));
        }

        /// The `n`th unanswered request of that command.
        fn reply_nth(&mut self, command: &str, n: usize, success: bool, message: Option<&str>, body: Value) {
            let seq = self
                .sent
                .iter()
                .filter(|v| v["type"] == "request" && v["command"] == command)
                .map(|v| v["seq"].as_i64().unwrap())
                .filter(|s| !self.answered.contains(s))
                .nth(n)
                .unwrap_or_else(|| panic!("no unanswered {command} #{n}"));
            self.answered.insert(seq);
            let mut m =
                json!({"type": "response", "seq": 500 + seq, "request_seq": seq, "success": success, "command": command, "body": body});
            if let Some(message) = message {
                m["message"] = json!(message);
            }
            self.raw(m);
        }

        fn answer(&mut self, command: &str, body: Value) {
            self.reply_nth(command, 0, true, None, body);
        }

        fn fail(&mut self, command: &str, message: &str) {
            self.reply_nth(command, 0, false, Some(message), Value::Null);
        }

        fn advance(&mut self, d: Duration) {
            self.now += d;
            self.feed(Input::Tick);
        }

        fn commands(&self) -> Vec<String> {
            self.sent.iter().map(|v| v["command"].as_str().unwrap().to_string()).collect()
        }

        fn requests(&self, command: &str) -> Vec<&Value> {
            self.sent.iter().filter(|v| v["type"] == "request" && v["command"] == command).collect()
        }

        fn phases(&self) -> Vec<Phase> {
            self.emitted.iter().filter_map(|b| if let DapBody::Phase(p) = b { Some(*p) } else { None }).collect()
        }

        fn take_emitted(&mut self) -> Vec<DapBody> {
            std::mem::take(&mut self.emitted)
        }

        fn phase(&self) -> Phase {
            self.s.phase()
        }

        fn count(&self, command: &str) -> usize {
            self.commands().iter().filter(|c| *c == command).count()
        }

        fn errors(&self) -> Vec<String> {
            self.emitted.iter().filter_map(|b| if let DapBody::Error(e) = b { Some(e.clone()) } else { None }).collect()
        }

        fn has_ended(&self) -> bool {
            self.emitted.iter().any(|b| matches!(b, DapBody::Ended { .. }))
        }
    }

    const BPS: &[(&str, &[usize])] = &[("/a.c", &[2, 4]), ("/b.c", &[0])];

    fn end_actions(reason: &str) -> Vec<Action> {
        vec![
            Action::Kill,
            Action::Emit(DapBody::Phase(Phase::Ended)),
            Action::Emit(DapBody::Ended { reason: reason.into() }),
            Action::Finish,
        ]
    }

    fn sets_ok(h: &mut H) {
        let n = h.requests("setBreakpoints").len() - h.answered_count("setBreakpoints");
        for _ in 0..n {
            h.answer("setBreakpoints", json!({"breakpoints": [{"verified": true}]}));
        }
        if h.requests("setExceptionBreakpoints").len() > h.answered_count("setExceptionBreakpoints") {
            h.answer("setExceptionBreakpoints", Value::Null);
        }
    }

    impl H {
        fn answered_count(&self, command: &str) -> usize {
            self.requests(command).iter().filter(|v| self.answered.contains(&v["seq"].as_i64().unwrap())).count()
        }

        /// `initialized` with the rule-3 assertions: no `configurationDone` before, one after.
        fn initialized(&mut self) {
            assert_eq!(self.count("configurationDone"), 0, "configurationDone before initialized");
            self.event("initialized", json!({}));
            assert_eq!(self.count("configurationDone"), 1, "no configurationDone after initialized");
        }
    }

    fn initialized_first_with(h: &mut H, c: Value) {
        h.answer("initialize", c);
        assert_eq!(h.phase(), Phase::Starting);
        h.initialized();
        sets_ok(h);
        h.answer("configurationDone", Value::Null);
        assert_eq!(h.phase(), Phase::Starting, "launch not answered yet");
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
    }

    fn initialized_first(h: &mut H) {
        initialized_first_with(h, caps());
    }

    fn launch_deferred_with(h: &mut H, c: Value) {
        h.answer("initialize", c);
        h.event("output", json!({"category": "console", "output": "hi"}));
        h.initialized();
        sets_ok(h);
        h.event("output", json!({"category": "stdout", "output": "x"}));
        h.answer("configurationDone", Value::Null);
        assert_eq!(h.phase(), Phase::Starting, "launch not answered yet");
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
    }

    fn launch_deferred(h: &mut H) {
        launch_deferred_with(h, caps());
    }

    fn launch_early(h: &mut H) {
        h.answer("initialize", caps());
        h.initialized();
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Starting, "configurationDone not answered yet");
        sets_ok(h);
        h.answer("configurationDone", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
    }

    fn launch_early_reversed(h: &mut H) {
        h.answer("initialize", caps());
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Starting);
        h.initialized();
        sets_ok(h);
        assert_eq!(h.phase(), Phase::Starting);
        h.answer("configurationDone", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
    }

    fn every_order(check: impl Fn(&mut H)) {
        let orders: [fn(&mut H); 4] = [initialized_first, launch_deferred, launch_early, launch_early_reversed];
        for order in orders {
            let mut h = H::new(BPS);
            order(&mut h);
            check(&mut h);
        }
    }

    fn running() -> H {
        let mut h = H::new(&[]);
        launch_deferred(&mut h);
        h.take_emitted();
        h
    }

    fn starting() -> H {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h
    }

    /// Suspended at epoch 1, thread 7.
    fn suspended() -> H {
        let mut h = running();
        h.event("stopped", json!({"threadId": 7, "reason": "breakpoint"}));
        h
    }

    fn tag(epoch: u64) -> DapTag {
        DapTag { epoch, kind: TagKind::Threads }
    }

    fn request(h: &mut H, command: &str, epoch: u64) {
        h.cmd(Command::Request { command: command.into(), arguments: Value::Null, tag: tag(epoch) });
    }

    fn sent_command(a: &Action) -> Option<&str> {
        if let Action::Send(o) = a { Some(o.command()) } else { None }
    }

    // ---- rules 1 to 3

    #[test]
    fn start_sends_only_initialize() {
        let h = H::new(&[]);
        assert_eq!(h.last.len(), 1);
        let Action::Send(Outgoing::Request { seq, command, arguments }) = &h.last[0] else { panic!("not a request") };
        assert_eq!((*seq, command.as_str()), (1, "initialize"));
        assert_eq!(
            *arguments,
            json!({
                "adapterID": "dbg", "clientID": "jetl", "clientName": "jetl", "linesStartAt1": true,
                "columnsStartAt1": true, "pathFormat": "path", "supportsVariableType": true,
                "supportsVariablePaging": true,
            })
        );
        assert_eq!(h.phase(), Phase::Initializing);
        assert!(h.emitted.is_empty());
    }

    #[test]
    fn nothing_else_sent_before_the_initialize_response() {
        let mut h = H::new(BPS);
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[1]), modified: false });
        request(&mut h, "threads", 0);
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 1 });
        h.cmd(Command::Pause { thread: 1 });
        h.event("initialized", json!({}));
        h.feed(Input::Tick);
        assert_eq!(h.commands(), ["initialize"]);
    }

    #[test]
    fn initialize_response_emits_started_and_sends_launch() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        let expected_caps = body_of::<Capabilities>("t", caps());
        assert_eq!(
            h.last,
            vec![
                Action::Emit(DapBody::Started { capabilities: Box::new(expected_caps) }),
                Action::Emit(DapBody::Phase(Phase::Starting)),
                Action::Send(Outgoing::Request { seq: 2, command: "launch".into(), arguments: json!({"program": "/p"}) }),
            ]
        );
    }

    #[test]
    fn launch_goes_out_without_waiting_for_initialized() {
        let mut h = H::new(BPS);
        h.answer("initialize", caps());
        assert_eq!(h.commands(), ["initialize", "launch"]);
        assert_eq!(h.phase(), Phase::Starting);
    }

    #[test]
    fn every_order_sends_configuration_in_order() {
        every_order(|h| {
            assert_eq!(
                h.commands(),
                ["initialize", "launch", "setBreakpoints", "setBreakpoints", "setExceptionBreakpoints", "configurationDone"]
            );
            let sets = h.requests("setBreakpoints");
            assert_eq!(sets[0]["arguments"]["source"]["path"], "/a.c");
            assert_eq!(sets[1]["arguments"]["source"]["path"], "/b.c");
        });
    }

    #[test]
    fn configuration_done_never_precedes_initialized() {
        // Each sequence asserts it in `H::initialized`; here the order of the whole run.
        every_order(|h| {
            let cmds = h.commands();
            let done = cmds.iter().position(|c| c == "configurationDone").unwrap();
            assert!(done > cmds.iter().position(|c| c == "setBreakpoints").unwrap());
            assert_eq!(cmds.iter().filter(|c| *c == "configurationDone").count(), 1);
        });
    }

    #[test]
    fn second_initialized_does_nothing() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h.initialized();
        h.event("initialized", json!({}));
        assert!(h.last.is_empty(), "after configuration");
        sets_ok(&mut h);
        h.answer("configurationDone", Value::Null);
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
        h.event("initialized", json!({}));
        assert!(h.last.is_empty(), "after Running");
    }

    #[test]
    fn initialized_before_response_runs_after_launch() {
        let mut h = H::new(BPS);
        h.event("initialized", json!({}));
        assert!(h.last.is_empty());
        h.answer("initialize", caps());
        assert_eq!(
            h.commands(),
            ["initialize", "launch", "setBreakpoints", "setBreakpoints", "setExceptionBreakpoints", "configurationDone"]
        );
    }

    #[test]
    fn exception_filters_only_when_rule_says() {
        let run = |c: Value| {
            let mut h = H::new(&[]);
            h.answer("initialize", c);
            h.event("initialized", json!({}));
            h
        };
        let h = run(caps());
        assert_eq!(h.requests("setExceptionBreakpoints")[0]["arguments"], json!({"filters": ["on"]}));
        let h = run(json!({"supportsConfigurationDoneRequest": true}));
        assert!(h.requests("setExceptionBreakpoints").is_empty());
        assert_eq!(h.count("configurationDone"), 1);
        let h = run(json!({}));
        assert_eq!(h.requests("setExceptionBreakpoints")[0]["arguments"], json!({"filters": []}));
        assert_eq!(h.count("configurationDone"), 0);
    }

    fn choices(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn saved_choices_pick_the_filters_sent() {
        let mut h = H::new(&[]);
        h.s.spec.exception_filters = choices(&[("on", false), ("off", true), ("gone", true)]);
        h.answer("initialize", caps());
        h.event("initialized", json!({}));
        assert_eq!(h.requests("setExceptionBreakpoints")[0]["arguments"], json!({"filters": ["off"]}));
    }

    #[test]
    fn chosen_filters_keeps_declared_order_and_drops_repeats() {
        let f = |name: &str, default: bool| ExceptionFilter { filter: name.into(), label: String::new(), default };
        let declared = [f("b", false), f("a", true), f("b", true), f("", true)];
        assert_eq!(chosen_filters(&declared, &BTreeMap::new()), vec!["a".to_string()]);
        assert_eq!(chosen_filters(&declared, &choices(&[("b", true)])), vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn a_filter_change_sends_the_full_set_once() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h.event("initialized", json!({}));
        h.answer("setExceptionBreakpoints", Value::Null);
        h.cmd(Command::SetExceptionFilters { choices: choices(&[("on", true), ("off", true)]) });
        assert_eq!(h.requests("setExceptionBreakpoints").len(), 2);
        assert_eq!(h.requests("setExceptionBreakpoints")[1]["arguments"], json!({"filters": ["on", "off"]}));
        h.answer("setExceptionBreakpoints", Value::Null);
        h.cmd(Command::SetExceptionFilters { choices: choices(&[("off", false), ("x", true)]) });
        assert_eq!(h.requests("setExceptionBreakpoints").len(), 3);
        assert_eq!(h.requests("setExceptionBreakpoints")[2]["arguments"], json!({"filters": ["on"]}));
    }

    #[test]
    fn a_filter_change_before_configuration_is_used_by_it() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h.cmd(Command::SetExceptionFilters { choices: choices(&[("on", false), ("off", true)]) });
        assert!(h.requests("setExceptionBreakpoints").is_empty());
        h.event("initialized", json!({}));
        assert_eq!(h.requests("setExceptionBreakpoints").len(), 1);
        assert_eq!(h.requests("setExceptionBreakpoints")[0]["arguments"], json!({"filters": ["off"]}));
    }

    #[test]
    fn a_filter_change_while_stopping_or_with_no_filters_sends_nothing() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h.event("initialized", json!({}));
        h.answer("setExceptionBreakpoints", Value::Null);
        h.cmd(Command::Stop);
        assert_eq!(h.phase(), Phase::Stopping);
        h.cmd(Command::SetExceptionFilters { choices: choices(&[("off", true)]) });
        assert_eq!(h.requests("setExceptionBreakpoints").len(), 1);

        let mut h = H::new(&[]);
        h.answer("initialize", json!({"supportsConfigurationDoneRequest": true}));
        h.event("initialized", json!({}));
        h.cmd(Command::SetExceptionFilters { choices: choices(&[("off", true)]) });
        assert!(h.requests("setExceptionBreakpoints").is_empty());
    }

    #[test]
    fn a_failed_filter_change_is_reported() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h.event("initialized", json!({}));
        h.answer("setExceptionBreakpoints", Value::Null);
        h.cmd(Command::SetExceptionFilters { choices: choices(&[("off", true)]) });
        h.fail("setExceptionBreakpoints", "nope");
        assert!(h.errors().contains(&"setExceptionBreakpoints: nope".to_string()), "{:?}", h.errors());
    }

    #[test]
    fn a_superseded_filter_change_fails_silently() {
        // Timeout: both unanswered requests expire together; only the newest reports.
        for timeout in [false, true] {
            let mut h = H::new(&[]);
            h.answer("initialize", caps());
            h.event("initialized", json!({}));
            h.answer("setExceptionBreakpoints", Value::Null);
            h.cmd(Command::SetExceptionFilters { choices: choices(&[("on", true)]) });
            h.cmd(Command::SetExceptionFilters { choices: choices(&[("off", true)]) });
            assert_eq!(h.requests("setExceptionBreakpoints").len(), 3);
            h.answer("configurationDone", Value::Null);
            if timeout {
                h.advance(REQUEST_TIMEOUT);
                assert_eq!(h.errors(), ["setExceptionBreakpoints timed out"]);
            } else {
                h.fail("setExceptionBreakpoints", "old");
                assert!(h.errors().is_empty(), "{:?}", h.errors());
                h.fail("setExceptionBreakpoints", "new");
                assert_eq!(h.errors(), ["setExceptionBreakpoints: new"]);
            }
        }
    }

    #[test]
    fn no_sources_still_configures() {
        let mut h = H::new(&[]);
        h.answer("initialize", caps());
        h.event("initialized", json!({}));
        assert_eq!(h.count("setBreakpoints"), 0);
        assert_eq!(h.count("configurationDone"), 1);
    }

    // ---- rule 4

    #[test]
    fn init_timeout_ends_the_session() {
        let mut h = H::new(&[]);
        h.advance(INIT_TIMEOUT - MS);
        assert!(h.last.is_empty());
        h.advance(MS);
        assert_eq!(h.last, end_actions("initialize timed out"));
    }

    #[test]
    fn an_hour_before_initialized_times_nothing_out_and_stop_works() {
        let mut h = starting();
        h.advance(HOUR);
        for _ in 0..20 {
            h.advance(Duration::from_millis(250));
        }
        assert!(h.errors().is_empty());
        assert!(!h.has_ended());
        assert_eq!(h.phase(), Phase::Starting);
        assert!(h.s.pending.values().any(|p| p.command == "launch"), "launch must outlive any hour");
        h.cmd(Command::Stop);
        assert_eq!(h.commands().last().unwrap(), "terminate");
        assert_eq!(h.phase(), Phase::Stopping);
    }

    #[test]
    fn an_hour_before_initialized_then_the_session_runs() {
        let mut h = starting();
        h.advance(HOUR);
        for _ in 0..20 {
            h.advance(Duration::from_millis(250));
        }
        h.initialized();
        sets_ok(&mut h);
        h.answer("configurationDone", Value::Null);
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
        assert!(h.errors().is_empty());
    }

    #[test]
    fn launch_is_exempt_even_after_configuration() {
        let mut h = starting();
        h.initialized();
        sets_ok(&mut h);
        h.answer("configurationDone", Value::Null);
        h.take_emitted();
        h.advance(HOUR);
        assert_eq!(h.phase(), Phase::Starting);
        assert!(h.emitted.is_empty());
        assert!(h.last.is_empty());
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
    }

    #[test]
    fn tagged_request_times_out_and_the_session_continues() {
        let mut h = suspended();
        request(&mut h, "threads", 1);
        h.take_emitted();
        h.advance(REQUEST_TIMEOUT - MS);
        assert!(h.last.is_empty());
        h.advance(MS);
        assert_eq!(h.last, vec![Action::Emit(DapBody::Response { tag: tag(1), result: Err("timed out".into()) })]);
        h.advance(Duration::from_secs(1));
        assert!(h.last.is_empty());
        assert_eq!(h.phase(), Phase::Suspended);
        h.answer("threads", json!({"threads": []}));
        assert!(h.last.is_empty());
    }

    #[test]
    fn a_timed_out_tag_of_an_older_epoch_is_dropped() {
        let mut h = suspended();
        request(&mut h, "threads", 1);
        h.cmd(Command::Resume { kind: ResumeKind::Continue, thread: 7 });
        h.answer("continue", json!({}));
        h.take_emitted();
        h.advance(REQUEST_TIMEOUT);
        assert!(h.emitted.is_empty(), "{:?}", h.emitted);
    }

    #[test]
    fn configuration_done_timeout_counts_as_answered() {
        let mut h = starting();
        h.initialized();
        sets_ok(&mut h);
        h.advance(REQUEST_TIMEOUT);
        assert_eq!(h.errors(), ["configurationDone timed out"]);
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
    }

    #[test]
    fn set_breakpoints_timeout_reports_and_continues() {
        let mut h = H::new(&[("/a.c", &[1])]);
        h.answer("initialize", json!({"supportsConfigurationDoneRequest": true}));
        h.event("initialized", json!({}));
        h.advance(REQUEST_TIMEOUT);
        assert_eq!(h.errors().iter().filter(|e| *e == "setBreakpoints timed out").count(), 1);
        assert_eq!(h.phase(), Phase::Starting);
    }

    // ---- rule 5

    #[test]
    fn failed_launch_emits_error_then_stops() {
        let mut h = starting();
        h.fail("launch", "no program");
        assert_eq!(h.last.len(), 3);
        assert_eq!(h.last[0], Action::Emit(DapBody::Error("no program".into())));
        assert_eq!(sent_command(&h.last[1]), Some("terminate"));
        assert_eq!(h.last[2], Action::Emit(DapBody::Phase(Phase::Stopping)));
        h.event("terminated", json!({}));
        assert_eq!(h.commands().last().unwrap(), "disconnect");
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn failed_configuration_done_emits_error_then_stops() {
        let mut h = starting();
        h.initialized();
        sets_ok(&mut h);
        h.fail("configurationDone", "bad");
        assert_eq!(h.last[0], Action::Emit(DapBody::Error("bad".into())));
        assert_eq!(sent_command(&h.last[1]), Some("terminate"));
        assert_eq!(h.last[2], Action::Emit(DapBody::Phase(Phase::Stopping)));
        h.event("terminated", json!({}));
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn initialize_failure_ends_with_reason() {
        let mut h = H::new(&[]);
        h.fail("initialize", "nope");
        assert_eq!(h.last, end_actions("initialize failed: nope"));
        assert!(h.errors().is_empty());
    }

    // ---- rule 6

    #[test]
    fn phases_in_every_order() {
        every_order(|h| assert_eq!(h.phases(), [Phase::Starting, Phase::Running]));
    }

    #[test]
    fn not_capable_of_configuration_done_runs_on_launch_answer() {
        let mut h = H::new(&[]);
        h.answer("initialize", json!({}));
        h.event("initialized", json!({}));
        assert_eq!(h.phase(), Phase::Starting);
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);

        let mut h = H::new(&[]);
        h.answer("initialize", json!({}));
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Starting);
        h.event("initialized", json!({}));
        assert_eq!(h.phase(), Phase::Running);
    }

    fn early_stop() -> H {
        let mut h = starting();
        h.initialized();
        sets_ok(&mut h);
        h.event("stopped", json!({"reason": "entry"}));
        h
    }

    #[test]
    fn stopped_before_configuration_done_response() {
        let h = early_stop();
        assert_eq!(h.phase(), Phase::Suspended);
        assert_eq!(h.last.len(), 2);
        assert_eq!(h.last[0], Action::Emit(DapBody::Phase(Phase::Suspended)));
        assert!(matches!(&h.last[1], Action::Emit(DapBody::Stopped { epoch: 1, event }) if event.reason == "entry"));
    }

    #[test]
    fn answers_after_an_early_stop_keep_suspended() {
        let mut h = early_stop();
        h.answer("configurationDone", Value::Null);
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Suspended);
        assert_eq!(h.phases(), [Phase::Starting, Phase::Suspended]);
        h.cmd(Command::Resume { kind: ResumeKind::Continue, thread: 1 });
        assert_eq!(h.phase(), Phase::Running);
    }

    #[test]
    fn stopped_during_initializing_is_ignored() {
        let mut h = H::new(&[]);
        h.event("stopped", json!({"reason": "entry"}));
        assert!(h.last.is_empty());
        assert_eq!(h.s.epoch(), 0);
        assert_eq!(h.phase(), Phase::Initializing);
    }

    #[test]
    fn stopped_while_stopping_is_ignored() {
        let mut h = running();
        h.cmd(Command::Stop);
        h.event("stopped", json!({"reason": "x"}));
        assert!(h.last.is_empty());
        h.event("continued", json!({}));
        assert!(h.last.is_empty());
        assert_eq!(h.s.epoch(), 0);
        assert_eq!(h.phase(), Phase::Stopping);
    }

    // ---- rule 7

    #[test]
    fn epoch_bumps_on_stopped_continued_resume_pause() {
        let mut h = running();
        h.event("stopped", json!({"threadId": 1, "reason": "pause"}));
        assert_eq!(h.s.epoch(), 1);
        h.cmd(Command::Resume { kind: ResumeKind::Continue, thread: 1 });
        assert_eq!(h.s.epoch(), 2);
        assert!(h.last.contains(&Action::Emit(DapBody::Running { epoch: 2 })));
        h.event("stopped", json!({"threadId": 1, "reason": "pause"}));
        assert_eq!(h.s.epoch(), 3);
        h.event("continued", json!({"threadId": 1}));
        assert_eq!(h.s.epoch(), 4);
        assert_eq!(h.last, vec![Action::Emit(DapBody::Phase(Phase::Running)), Action::Emit(DapBody::Running { epoch: 4 })]);
        assert_eq!(h.phase(), Phase::Running);
        h.cmd(Command::Pause { thread: 1 });
        assert_eq!(h.s.epoch(), 5);
        assert_eq!(h.last.len(), 1);
        assert_eq!(sent_command(&h.last[0]), Some("pause"));
        assert_eq!(h.phase(), Phase::Running);
        h.event("stopped", json!({"threadId": 1, "reason": "pause"}));
        assert_eq!(h.s.epoch(), 6);
    }

    #[test]
    fn resume_sends_the_right_command_with_the_thread() {
        for (kind, name) in [
            (ResumeKind::Continue, "continue"),
            (ResumeKind::Next, "next"),
            (ResumeKind::StepIn, "stepIn"),
            (ResumeKind::StepOut, "stepOut"),
        ] {
            let mut h = suspended();
            h.cmd(Command::Resume { kind, thread: 7 });
            let Action::Send(Outgoing::Request { command, arguments, .. }) = &h.last[0] else { panic!("no request") };
            assert_eq!(command, name);
            assert_eq!(*arguments, json!({"threadId": 7}));
            if kind == ResumeKind::Continue {
                assert_eq!(h.last.len(), 3);
                assert_eq!(h.last[1], Action::Emit(DapBody::Phase(Phase::Running)));
                assert_eq!(h.last[2], Action::Emit(DapBody::Running { epoch: 2 }));
            }
        }
    }

    #[test]
    fn resume_outside_suspended_is_ignored() {
        let mut h = running();
        let n = h.sent.len();
        h.cmd(Command::Resume { kind: ResumeKind::Continue, thread: 1 });
        assert!(h.last.is_empty());
        assert_eq!((h.s.epoch(), h.sent.len()), (0, n));
        let mut h = suspended();
        let n = h.sent.len();
        h.cmd(Command::Pause { thread: 1 });
        assert!(h.last.is_empty());
        assert_eq!((h.s.epoch(), h.sent.len()), (1, n));
    }

    #[test]
    fn current_tag_answers_come_back() {
        let mut h = suspended();
        request(&mut h, "threads", 1);
        h.answer("threads", json!({"threads": [{"id": 1, "name": "t"}]}));
        assert_eq!(h.last, vec![Action::Emit(DapBody::Response { tag: tag(1), result: Ok(json!({"threads": [{"id": 1, "name": "t"}]})) })]);
        request(&mut h, "threads", 1);
        h.fail("threads", "boom");
        assert_eq!(h.last, vec![Action::Emit(DapBody::Response { tag: tag(1), result: Err("boom".into()) })]);
    }

    #[test]
    fn stale_answer_is_dropped_errors_included() {
        let mut h = suspended();
        request(&mut h, "variables", 1);
        request(&mut h, "variables", 1);
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.fail("variables", "list index out of range");
        assert!(h.last.is_empty());
        h.answer("variables", json!({"variables": []}));
        assert!(h.last.is_empty());
    }

    #[test]
    fn request_with_an_older_tag_is_not_sent() {
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        let cmds = h.commands();
        request(&mut h, "threads", 1);
        assert_eq!(h.commands(), cmds);
        assert!(h.last.is_empty());
    }

    #[test]
    fn an_old_variables_error_is_dropped() {
        let mut h = suspended();
        let tagged = |kind| Command::Request { command: "variables".into(), arguments: Value::Null, tag: DapTag { epoch: 1, kind } };
        h.cmd(tagged(TagKind::Variables { node: 3, seq: 5 }));
        h.cmd(tagged(TagKind::Scopes { seq: 5 }));
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.fail("variables", "list index out of range");
        assert!(h.last.is_empty(), "{:?}", h.last);
        h.fail("variables", "list index out of range");
        assert!(h.last.is_empty(), "{:?}", h.last);
    }

    #[test]
    fn an_old_variables_request_is_not_sent() {
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        let cmds = h.commands();
        for kind in [TagKind::Variables { node: 0, seq: 1 }, TagKind::Scopes { seq: 1 }] {
            h.cmd(Command::Request { command: "variables".into(), arguments: Value::Null, tag: DapTag { epoch: 1, kind } });
            assert_eq!(h.commands(), cmds);
            assert!(h.last.is_empty());
        }
    }

    #[test]
    fn request_when_not_available() {
        let mut h = H::new(&[]);
        request(&mut h, "threads", 0);
        assert_eq!(h.last, vec![Action::Emit(DapBody::Response { tag: tag(0), result: Err("not available".into()) })]);
        assert_eq!(h.commands(), ["initialize"]);
        let mut h = running();
        h.cmd(Command::Stop);
        let cmds = h.commands();
        request(&mut h, "threads", 0);
        assert_eq!(h.last, vec![Action::Emit(DapBody::Response { tag: tag(0), result: Err("not available".into()) })]);
        assert_eq!(h.commands(), cmds);
    }

    #[test]
    fn failed_step_reports_while_current() {
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.fail("next", "notStopped");
        assert_eq!(h.errors(), ["next: notStopped"]);
        let checks = h.requests("stackTrace");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0]["arguments"], json!({"threadId": 7, "startFrame": 0, "levels": 1}));
        assert_eq!(h.phase(), Phase::Running);
        h.take_emitted();
        h.fail("stackTrace", "notStopped");
        assert_eq!(h.phase(), Phase::Running);
        assert!(!h.take_emitted().iter().any(|b| matches!(b, DapBody::Stopped { .. })));

        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.event("stopped", json!({"threadId": 7, "reason": "step"}));
        h.fail("next", "notStopped");
        assert!(h.last.is_empty());
    }

    #[test]
    fn refused_resume_returns_to_the_stop_when_a_stack_answers() {
        for (kind, name) in [
            (ResumeKind::Next, "next"),
            (ResumeKind::StepIn, "stepIn"),
            (ResumeKind::StepOut, "stepOut"),
            (ResumeKind::Continue, "continue"),
        ] {
            let mut h = suspended();
            h.cmd(Command::Resume { kind, thread: 7 });
            h.fail(name, "no");
            h.take_emitted();
            h.answer("stackTrace", json!({"stackFrames": []}));
            let emitted = h.take_emitted();
            let [DapBody::Phase(Phase::Suspended), DapBody::Stopped { epoch: 3, event }] = &emitted[..] else {
                panic!("{name}: {emitted:?}");
            };
            assert_eq!(event.reason, "breakpoint", "{name}");
            assert_eq!(event.thread_id, Some(7), "{name}");
            assert_eq!(h.phase(), Phase::Suspended);
            h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
            assert_eq!(h.requests("next").len(), usize::from(name == "next") + 1, "{name}");
            assert_eq!(h.take_emitted(), vec![DapBody::Phase(Phase::Running), DapBody::Running { epoch: 4 }], "{name}");
        }
    }

    #[test]
    fn refused_resume_keeps_the_last_stops_event_and_the_chosen_thread() {
        let mut h = running();
        h.event("stopped", json!({"threadId": 7, "reason": "step", "hitBreakpointIds": [2]}));
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 4 });
        h.fail("next", "no");
        h.take_emitted();
        h.answer("stackTrace", json!({"stackFrames": []}));
        let emitted = h.take_emitted();
        let Some(DapBody::Stopped { event, .. }) = emitted.last() else { panic!("{emitted:?}") };
        assert_eq!(event.thread_id, Some(4));
        assert_eq!(event.reason, "step");
        assert_eq!(event.hit_breakpoint_ids, [2]);
    }

    #[test]
    fn a_stop_check_goes_stale() {
        for row in ["stopped", "continued", "pause", "stop"] {
            let mut h = suspended();
            h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
            h.fail("next", "no");
            match row {
                "stopped" => h.event("stopped", json!({"threadId": 7, "reason": "step"})),
                "continued" => h.event("continued", json!({"threadId": 7})),
                "pause" => h.cmd(Command::Pause { thread: 7 }),
                _ => h.cmd(Command::Stop),
            }
            h.take_emitted();
            h.answer("stackTrace", json!({"stackFrames": []}));
            let emitted = h.take_emitted();
            assert!(emitted.is_empty(), "{row}: the stale check emitted {emitted:?}");
            if row == "stop" {
                assert_eq!(h.phase(), Phase::Stopping);
            }
        }
        // The `stopped` row: exactly one `Stopped` since the resume, the event's own.
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.fail("next", "no");
        h.take_emitted();
        h.event("stopped", json!({"threadId": 7, "reason": "step"}));
        h.answer("stackTrace", json!({"stackFrames": []}));
        let stops = h.take_emitted().iter().filter(|b| matches!(b, DapBody::Stopped { .. })).count();
        assert_eq!(stops, 1);
    }

    #[test]
    fn a_stop_check_that_times_out_stays_running() {
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.fail("next", "no");
        h.advance(REQUEST_TIMEOUT);
        assert_eq!(h.phase(), Phase::Running);
        assert_eq!(h.errors(), ["next: no"]);
    }

    #[test]
    fn a_refused_pause_asks_nothing() {
        let mut h = running();
        h.cmd(Command::Pause { thread: 7 });
        h.fail("pause", "no");
        assert_eq!(h.errors(), ["pause: no"]);
        assert!(h.requests("stackTrace").is_empty());
        assert_eq!(h.phase(), Phase::Running);
    }

    #[test]
    fn a_refusal_during_a_terminated_stop_reports_and_asks_nothing() {
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.event("terminated", json!({}));
        h.fail("next", "no");
        assert!(h.errors().contains(&"next: no".to_string()));
        assert!(h.requests("stackTrace").is_empty());
        assert_eq!(h.phase(), Phase::Stopping);
    }

    #[test]
    fn a_step_timeout_asks_nothing() {
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.advance(REQUEST_TIMEOUT);
        assert_eq!(h.errors(), ["next timed out"]);
        assert!(h.requests("stackTrace").is_empty());
        assert_eq!(h.phase(), Phase::Running);
    }

    // ---- rule 8

    #[test]
    fn set_breakpoints_before_initialized_replaces_the_snapshot() {
        let mut h = H::new(BPS);
        h.answer("initialize", caps());
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[7]), modified: false });
        assert!(h.last.is_empty());
        h.cmd(Command::SetBreakpoints { source: "/c.c".into(), breakpoints: at(&[1]), modified: false });
        assert!(h.last.is_empty());
        h.cmd(Command::SetBreakpoints { source: "/b.c".into(), breakpoints: at(&[]), modified: true });
        assert!(h.last.is_empty());
        assert_eq!(h.commands(), ["initialize", "launch"]);
        h.event("initialized", json!({}));
        let args: Vec<Value> = h.requests("setBreakpoints").iter().map(|v| v["arguments"].clone()).collect();
        assert_eq!(
            args,
            vec![
                json!({"source": {"name": "a.c", "path": "/a.c"}, "breakpoints": [{"line": 8}], "sourceModified": false}),
                json!({"source": {"name": "b.c", "path": "/b.c"}, "breakpoints": [], "sourceModified": true}),
                json!({"source": {"name": "c.c", "path": "/c.c"}, "breakpoints": [{"line": 2}], "sourceModified": false}),
            ]
        );
    }

    #[test]
    fn set_breakpoints_after_initialized_sends_at_once() {
        let mut h = starting();
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[2, 4]), modified: false });
        assert_eq!(h.last.len(), 1);
        let Action::Send(o) = &h.last[0] else { panic!("no send") };
        assert_eq!(
            o.to_json()["arguments"],
            json!({"source": {"name": "a.c", "path": "/a.c"}, "breakpoints": [{"line": 3}, {"line": 5}], "sourceModified": false})
        );
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[2, 4]), modified: true });
        assert_eq!(h.requests("setBreakpoints")[1]["arguments"]["sourceModified"], json!(true));
    }

    #[test]
    fn empty_list_is_sent() {
        let mut h = starting();
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[]), modified: false });
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([]));
    }

    fn two_for_a() -> H {
        let mut h = starting();
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[1]), modified: false });
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[2, 3]), modified: false });
        h.take_emitted();
        h
    }

    fn breakpoint_emits(h: &H) -> Vec<(Vec<usize>, Vec<Breakpoint>)> {
        h.emitted
            .iter()
            .filter_map(|b| {
                if let DapBody::Breakpoints { requested, result, .. } = b { Some((requested.clone(), result.clone())) } else { None }
            })
            .collect()
    }

    #[test]
    fn only_the_newest_answer_of_a_source_is_emitted() {
        let older = json!({"breakpoints": [{"id": 1, "verified": true, "line": 2}]});
        let newer = json!({"breakpoints": [{"id": 2, "verified": true, "line": 3}, {"id": 3, "verified": false}]});
        let mut h = two_for_a();
        h.reply_nth("setBreakpoints", 0, true, None, older.clone());
        h.reply_nth("setBreakpoints", 0, true, None, newer.clone());
        let got = breakpoint_emits(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, [2, 3]);
        assert_eq!(got[0].1.len(), 2);
        assert_eq!(got[0].1[0].id, Some(2));

        let mut h = two_for_a();
        h.reply_nth("setBreakpoints", 1, true, None, newer);
        h.reply_nth("setBreakpoints", 0, true, None, older);
        let got = breakpoint_emits(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, [2, 3]);
    }

    #[test]
    fn newest_tracking_is_per_source() {
        let mut h = starting();
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[1]), modified: false });
        h.cmd(Command::SetBreakpoints { source: "/b.c".into(), breakpoints: at(&[1]), modified: false });
        h.take_emitted();
        h.answer("setBreakpoints", json!({"breakpoints": [{"verified": true}]}));
        h.answer("setBreakpoints", json!({"breakpoints": [{"verified": true}]}));
        assert_eq!(breakpoint_emits(&h).len(), 2);
    }

    #[test]
    fn failed_set_breakpoints_reports_and_continues() {
        let mut h = two_for_a();
        h.reply_nth("setBreakpoints", 1, false, Some("x"), Value::Null);
        assert_eq!(h.last, vec![Action::Emit(DapBody::Error("setBreakpoints: x".into()))]);
        assert!(breakpoint_emits(&h).is_empty());
        assert_eq!(h.phase(), Phase::Starting);
        h.reply_nth("setBreakpoints", 0, false, Some("old"), Value::Null);
        assert!(h.last.is_empty());
    }

    #[test]
    fn breakpoint_result_is_passed_as_on_the_wire() {
        let mut h = starting();
        h.initialized();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[2]), modified: false });
        h.answer("setBreakpoints", json!({"breakpoints": [{"id": 1, "verified": true, "line": 3}]}));
        let got = breakpoint_emits(&h);
        assert_eq!(got[0].0, [2]);
        assert_eq!(got[0].1[0].line, Some(3));
    }

    // ---- rule 9

    #[test]
    fn reverse_request_is_answered_from_the_shared_counter() {
        let mut h = H::new(&[]);
        h.reverse("runInTerminal", 77);
        let Action::Send(o) = &h.last[0] else { panic!("no answer") };
        assert_eq!(
            *o,
            Outgoing::Response {
                seq: 2,
                request_seq: 77,
                success: false,
                command: "runInTerminal".into(),
                message: Some("not supported".into())
            }
        );
        h.answer("initialize", caps());
        h.reverse("startDebugging", 3);
        h.initialized();
        let seqs: Vec<i64> = h.sent.iter().map(|v| v["seq"].as_i64().unwrap()).collect();
        assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>());
    }

    #[test]
    fn reverse_request_answered_in_every_phase() {
        let phases: [(Phase, fn() -> H); 5] = [
            (Phase::Initializing, || H::new(&[])),
            (Phase::Starting, starting),
            (Phase::Running, running),
            (Phase::Suspended, suspended),
            (Phase::Stopping, || {
                let mut h = running();
                h.cmd(Command::Stop);
                h
            }),
        ];
        for (phase, make) in phases {
            let mut h = make();
            assert_eq!(h.phase(), phase);
            h.reverse("runInTerminal", 5);
            assert!(matches!(&h.last[..], [Action::Send(Outgoing::Response { request_seq: 5, success: false, .. })]), "{phase:?}");
        }
        let mut h = H::new(&[]);
        h.cmd(Command::Stop);
        h.reverse("runInTerminal", 5);
        assert!(h.last.is_empty());
    }

    // ---- rules 10, 11

    #[test]
    fn stop_sequence_with_terminate() {
        let mut h = running();
        h.cmd(Command::Stop);
        assert_eq!(h.last.len(), 2);
        assert_eq!(sent_command(&h.last[0]), Some("terminate"));
        assert_eq!(h.last[1], Action::Emit(DapBody::Phase(Phase::Stopping)));
        h.event("terminated", json!({}));
        assert_eq!(h.last.len(), 1);
        let Action::Send(o) = &h.last[0] else { panic!("no send") };
        assert_eq!(o.command(), "disconnect");
        assert_eq!(o.to_json()["arguments"], json!({"terminateDebuggee": true}));
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
    }

    /// An attach session that is running: initialize answered with `c`, configured, `attach` answered.
    fn attached_with(c: Value) -> H {
        let mut h = H::attach(&[]);
        h.answer("initialize", c);
        h.initialized();
        sets_ok(&mut h);
        h.answer("configurationDone", Value::Null);
        h.answer("attach", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
        h.take_emitted();
        h
    }

    fn detach_args(h: &H) -> Value {
        h.requests("disconnect")[0]["arguments"].clone()
    }

    #[test]
    fn attach_is_sent_in_place_of_launch() {
        let mut h = H::attach(&[]);
        h.answer("initialize", caps());
        assert_eq!(h.commands(), ["initialize", "attach"]);
        assert_eq!(h.requests("attach")[0]["arguments"], json!({"program": "/p"}));
    }

    #[test]
    fn stopping_an_attach_detaches_without_terminate() {
        let mut h = attached_with(caps());
        h.cmd(Command::Stop);
        assert_eq!(h.commands().last().unwrap(), "disconnect");
        assert_eq!(detach_args(&h), json!({"terminateDebuggee": false}));
        assert_eq!(h.count("terminate"), 0);
        assert_eq!(h.phase(), Phase::Stopping);
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
        assert_eq!(h.count("terminate"), 0);
    }

    #[test]
    fn an_attach_without_terminate_debuggee_still_says_false() {
        let mut h = attached_with(json!({"supportsConfigurationDoneRequest": true, "supportsTerminateRequest": true}));
        h.cmd(Command::Stop);
        assert_eq!(detach_args(&h), json!({"terminateDebuggee": false}));
    }

    #[test]
    fn a_failed_attach_detaches() {
        let mut h = H::attach(&[]);
        h.answer("initialize", caps());
        h.fail("attach", "no");
        assert_eq!(h.errors().len(), 1);
        assert_eq!(h.commands().last().unwrap(), "disconnect");
        assert_eq!(detach_args(&h), json!({"terminateDebuggee": false}));
        assert_eq!(h.count("terminate"), 0);
    }

    #[test]
    fn the_attached_program_ending_detaches() {
        let mut h = attached_with(caps());
        h.event("terminated", json!({}));
        assert_eq!(h.commands().last().unwrap(), "disconnect");
        assert_eq!(detach_args(&h), json!({"terminateDebuggee": false}));
        assert_eq!(h.count("terminate"), 0);
    }

    #[test]
    fn stop_without_terminate_capability_goes_to_disconnect() {
        let mut h = H::new(&[]);
        launch_deferred_with(&mut h, json!({"supportsConfigurationDoneRequest": true}));
        h.cmd(Command::Stop);
        assert_eq!(h.count("terminate"), 0);
        assert_eq!(h.commands().last().unwrap(), "disconnect");
        assert_eq!(h.phase(), Phase::Stopping);
    }

    #[test]
    fn disconnect_without_debuggee_capability() {
        let mut h = H::new(&[]);
        launch_deferred_with(&mut h, json!({"supportsConfigurationDoneRequest": true, "supportsTerminateRequest": true}));
        h.cmd(Command::Stop);
        h.event("terminated", json!({}));
        assert_eq!(h.requests("disconnect")[0]["arguments"], json!({}));
    }

    #[test]
    fn terminate_stage_times_out() {
        let mut h = running();
        h.cmd(Command::Stop);
        h.advance(STOP_STAGE - MS);
        assert!(h.last.is_empty());
        h.advance(MS);
        assert_eq!(h.last.len(), 1);
        assert_eq!(sent_command(&h.last[0]), Some("disconnect"));
        h.advance(STOP_STAGE);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn silent_adapter_ends_in_two_stages() {
        let mut h = running();
        h.cmd(Command::Stop);
        h.advance(STOP_STAGE);
        assert!(!h.has_ended());
        h.advance(STOP_STAGE);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn second_stop_skips_a_stage() {
        let mut h = running();
        h.cmd(Command::Stop);
        h.cmd(Command::Stop);
        assert_eq!(h.last.len(), 1);
        assert_eq!(sent_command(&h.last[0]), Some("disconnect"));
        h.cmd(Command::Stop);
        assert_eq!(h.last, end_actions(""));
    }

    fn notes(h: &H) -> Vec<String> {
        h.emitted
            .iter()
            .filter_map(|b| match b {
                DapBody::Info(t) | DapBody::Warning(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_second_stop_press_does_not_kill_an_attach() {
        let mut h = attached_with(caps());
        h.s.spec.launch = json!({"pid": 4242});
        h.cmd(Command::Stop);
        h.take_emitted();
        h.cmd(Command::Stop);
        assert!(!h.has_ended(), "gdb was killed by the second press");
        assert!(!h.last.iter().any(|a| matches!(a, Action::Kill)), "{:?}", h.last);
        assert_eq!(h.phase(), Phase::Stopping);
        assert_eq!(notes(&h), ["Detaching from process 4242..."]);
        assert_eq!(h.count("disconnect"), 1);
        // The stage and its deadline are kept: the detach can still be answered.
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn an_attach_that_does_not_detach_in_time_is_killed_with_a_warning() {
        let mut h = attached_with(caps());
        h.s.spec.launch = json!({"pid": 4242});
        h.cmd(Command::Stop);
        h.take_emitted();
        h.cmd(Command::Stop);
        h.advance(STOP_STAGE);
        assert!(h.has_ended());
        assert_eq!(
            notes(&h),
            ["Detaching from process 4242...", "gdb did not detach in time and was stopped; process 4242 may have been stopped with it"]
        );
    }

    #[test]
    fn a_launch_second_stop_press_still_kills_without_a_note() {
        let mut h = running();
        h.cmd(Command::Stop);
        h.cmd(Command::Stop);
        h.cmd(Command::Stop);
        assert!(h.has_ended());
        assert!(notes(&h).is_empty(), "{:?}", notes(&h));
        let mut h = running();
        h.cmd(Command::Stop);
        h.advance(STOP_STAGE);
        h.advance(STOP_STAGE);
        assert!(h.has_ended());
        assert!(notes(&h).is_empty(), "{:?}", notes(&h));
    }

    #[test]
    fn stop_in_initializing_kills_straight() {
        let mut h = H::new(&[]);
        h.cmd(Command::Stop);
        assert_eq!(h.last, end_actions(""));
        assert_eq!(h.commands(), ["initialize"]);
    }

    #[test]
    fn stop_works_in_each_phase() {
        for make in [starting as fn() -> H, running, suspended] {
            let mut h = make();
            h.cmd(Command::Stop);
            assert_eq!(h.commands().last().unwrap(), "terminate");
            assert_eq!(h.phase(), Phase::Stopping);
        }
        let mut h = H::new(&[]);
        h.cmd(Command::Stop);
        h.cmd(Command::Stop);
        assert!(h.last.is_empty());
        assert_eq!(h.phase(), Phase::Ended);
    }

    #[test]
    fn initialized_while_stopping_sends_no_configuration() {
        let mut h = starting();
        h.cmd(Command::Stop);
        h.event("initialized", json!({}));
        assert!(h.last.is_empty());
        assert_eq!(h.count("setBreakpoints") + h.count("configurationDone"), 0);
    }

    #[test]
    fn stale_failures_are_silent_while_stopping() {
        let mut h = starting();
        h.cmd(Command::Stop);
        h.fail("launch", "cancelled");
        assert!(h.last.is_empty());
        assert!(h.errors().is_empty());
        assert_eq!(h.count("terminate"), 1);
    }

    #[test]
    fn terminated_outside_stop_gets_one_disconnect() {
        let mut h = running();
        h.event("terminated", json!({}));
        assert_eq!(h.last.len(), 2);
        assert_eq!(sent_command(&h.last[0]), Some("disconnect"));
        assert_eq!(h.last[1], Action::Emit(DapBody::Phase(Phase::Stopping)));
        assert_eq!(h.count("terminate"), 0);
        h.event("terminated", json!({}));
        assert!(h.last.is_empty());
        assert_eq!(h.count("disconnect"), 1);
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn terminated_then_silence_ends_after_stop_stage() {
        let mut h = running();
        h.event("terminated", json!({}));
        h.advance(STOP_STAGE);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn second_stop_after_terminated_kills() {
        let mut h = running();
        h.event("terminated", json!({}));
        h.cmd(Command::Stop);
        assert_eq!(h.last, end_actions(""));
    }

    #[test]
    fn eof_without_terminated() {
        let makes: [fn() -> H; 4] = [|| H::new(&[]), starting, running, suspended];
        for make in makes {
            let mut h = make();
            h.feed(Input::Eof);
            assert_eq!(
                h.last,
                vec![
                    Action::Emit(DapBody::Phase(Phase::Ended)),
                    Action::Emit(DapBody::Ended { reason: "adapter exited".into() }),
                    Action::Finish
                ]
            );
        }
    }

    #[test]
    fn eof_after_terminated_or_stop_is_normal() {
        let normal = vec![Action::Emit(DapBody::Phase(Phase::Ended)), Action::Emit(DapBody::Ended { reason: "".into() }), Action::Finish];
        let mut h = running();
        h.event("terminated", json!({}));
        h.feed(Input::Eof);
        assert_eq!(h.last, normal);
        let mut h = running();
        h.cmd(Command::Stop);
        h.feed(Input::Eof);
        assert_eq!(h.last, normal);
    }

    #[test]
    fn exited_is_optional() {
        let mut h = running();
        h.event("exited", json!({"exitCode": 3}));
        assert_eq!(h.last, vec![Action::Emit(DapBody::Exited(3))]);
        assert_eq!(h.phase(), Phase::Running);
        let mut h = running();
        h.event("terminated", json!({}));
        h.answer("disconnect", Value::Null);
        assert!(h.has_ended());
        assert!(!h.emitted.iter().any(|b| matches!(b, DapBody::Exited(_))));
    }

    // ---- rule 12 and events

    #[test]
    fn stopped_without_thread_id_is_passed_on() {
        let mut h = running();
        h.event("stopped", json!({"reason": "pause"}));
        assert!(matches!(&h.last[1], Action::Emit(DapBody::Stopped { event, .. }) if event.thread_id.is_none()));
        assert_eq!(h.phase(), Phase::Suspended);
    }

    #[test]
    fn odd_stack_frame_body_passes_through() {
        let mut h = suspended();
        h.cmd(Command::Request {
            command: "stackTrace".into(),
            arguments: json!({"threadId": 7}),
            tag: DapTag { epoch: 1, kind: TagKind::StackTrace { thread: 7 } },
        });
        let body = json!({"stackFrames": [{"id": 0, "name": "f", "line": 0, "column": 0, "source": {"name": "x"}}]});
        h.answer("stackTrace", body.clone());
        assert!(matches!(&h.last[..], [Action::Emit(DapBody::Response { result: Ok(v), .. })] if *v == body));
    }

    #[test]
    fn breakpoint_changed_event_is_passed_on() {
        let mut h = running();
        h.event("breakpoint", json!({"reason": "changed", "breakpoint": {"id": 1, "verified": true, "line": 3}}));
        let [Action::Emit(DapBody::BreakpointChanged(e))] = &h.last[..] else { panic!("{:?}", h.last) };
        assert!(e.breakpoint.verified);
        assert_eq!(e.breakpoint.line, Some(3));
    }

    #[test]
    fn output_passes_except_telemetry() {
        for make in [(|| H::new(&[])) as fn() -> H, running] {
            let mut h = make();
            h.event("output", json!({"category": "stdout", "output": "a"}));
            assert_eq!(h.last.len(), 1);
            h.event("output", json!({"output": "b"}));
            assert!(matches!(&h.last[..], [Action::Emit(DapBody::Output(o))] if o.category.is_none()));
            h.event("output", json!({"category": "important", "output": "c"}));
            assert_eq!(h.last.len(), 1);
            h.event("output", json!({"category": "telemetry", "output": "d"}));
            assert!(h.last.is_empty());
        }
    }

    #[test]
    fn unknown_events_are_ignored() {
        let mut h = running();
        for name in ["module", "thread", "debugpyAttach", "capabilities"] {
            h.event(name, json!({"x": 1}));
            assert!(h.last.is_empty(), "{name}");
        }
    }

    #[test]
    fn process_event_reaches_the_app() {
        let mut h = running();
        h.event("process", json!({"name": "/p/demo", "systemProcessId": 42, "isLocalProcess": true, "startMethod": "process"}));
        assert_eq!(h.last, [Action::Emit(DapBody::Process(ProcessEvent { name: "/p/demo".into(), system_process_id: Some(42) }))]);
        h.event("process", json!({"startMethod": "attach"}));
        assert_eq!(h.last, [Action::Emit(DapBody::Process(ProcessEvent::default()))]);
        h.event("process", json!({"name": null, "systemProcessId": null}));
        assert_eq!(h.last, [Action::Emit(DapBody::Process(ProcessEvent::default()))]);
    }

    #[test]
    fn unknown_request_seq_is_ignored() {
        let mut h = running();
        h.raw(json!({"type": "response", "seq": 1, "request_seq": 9999, "success": false, "command": "x"}));
        assert!(h.last.is_empty());
    }

    // ---- fix wave: why a stop began, and the decision-table rows that had no test

    /// `Starting` with `initialized` seen, configuration sent, nothing answered.
    fn configuring() -> H {
        let mut h = starting();
        h.initialized();
        h
    }

    #[test]
    fn terminated_then_failed_launch_reports_then_ends() {
        let mut h = starting();
        h.event("terminated", json!({}));
        assert_eq!(h.phase(), Phase::Stopping);
        h.fail("launch", "no program");
        assert_eq!(h.errors(), ["no program"]);
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
        let at = |f: fn(&DapBody) -> bool| h.emitted.iter().position(f).unwrap();
        assert!(at(|b| matches!(b, DapBody::Error(_))) < at(|b| matches!(b, DapBody::Ended { .. })));
    }

    #[test]
    fn terminated_does_not_hide_a_second_failure() {
        let mut h = configuring();
        sets_ok(&mut h);
        h.event("terminated", json!({}));
        h.fail("configurationDone", "bad");
        h.fail("launch", "worse");
        assert_eq!(h.errors(), ["bad", "worse"]);
    }

    #[test]
    fn terminated_then_a_collateral_failure_does_not_hide_the_launch_failure() {
        let mut h = configuring();
        h.event("terminated", json!({}));
        h.fail("setExceptionBreakpoints", "cancelled");
        h.fail("launch", "no program");
        assert_eq!(h.errors(), ["setExceptionBreakpoints: cancelled", "no program"]);
    }

    #[test]
    fn a_failure_that_began_the_stop_keeps_later_ones_silent_past_terminated() {
        let mut h = configuring();
        sets_ok(&mut h);
        h.fail("configurationDone", "bad");
        h.event("terminated", json!({}));
        h.fail("launch", "worse");
        assert_eq!(h.errors(), ["bad"]);
    }

    #[test]
    fn stop_stays_silent_past_terminated() {
        let mut h = starting();
        h.cmd(Command::Stop);
        h.event("terminated", json!({}));
        h.fail("launch", "cancelled");
        assert!(h.errors().is_empty(), "{:?}", h.errors());
    }

    #[test]
    fn terminated_then_configuration_done_timeout_reports() {
        let mut h = configuring();
        sets_ok(&mut h);
        let second = Duration::from_secs(1);
        h.advance(REQUEST_TIMEOUT - second);
        h.event("terminated", json!({}));
        h.advance(second - MS);
        assert!(h.errors().is_empty());
        h.advance(MS);
        assert_eq!(h.errors(), ["configurationDone timed out"]);
    }

    #[test]
    fn stop_then_cancelled_set_exception_breakpoints_is_silent() {
        let mut h = configuring();
        h.answer("setExceptionBreakpoints", Value::Null);
        h.cmd(Command::Stop);
        h.fail("configurationDone", "cancelled");
        let mut h2 = configuring();
        h2.cmd(Command::Stop);
        h2.fail("setExceptionBreakpoints", "cancelled");
        assert!(h.errors().is_empty() && h2.errors().is_empty(), "{:?} {:?}", h.errors(), h2.errors());
    }

    #[test]
    fn stop_then_set_exception_breakpoints_timeout_is_silent() {
        let mut h = configuring();
        h.cmd(Command::Stop);
        h.advance(REQUEST_TIMEOUT);
        assert!(h.errors().is_empty(), "{:?}", h.errors());
    }

    #[test]
    fn stop_then_cancelled_step_is_silent() {
        for pause in [false, true] {
            let mut h = if pause { running() } else { suspended() };
            if pause {
                h.cmd(Command::Pause { thread: 7 });
            } else {
                h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
            }
            h.cmd(Command::Stop);
            h.fail(if pause { "pause" } else { "next" }, "cancelled");
            assert!(h.errors().is_empty(), "pause={pause}: {:?}", h.errors());
        }
    }

    #[test]
    fn stop_then_step_timeout_is_silent() {
        for pause in [false, true] {
            let mut h = if pause { running() } else { suspended() };
            if pause {
                h.cmd(Command::Pause { thread: 7 });
            } else {
                h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
            }
            h.cmd(Command::Stop);
            h.advance(REQUEST_TIMEOUT);
            assert!(h.errors().is_empty(), "pause={pause}: {:?}", h.errors());
        }
    }

    #[test]
    fn stop_then_configuration_done_failure_or_timeout_is_silent() {
        let mut h = configuring();
        sets_ok(&mut h);
        h.cmd(Command::Stop);
        h.fail("configurationDone", "cancelled");
        assert!(h.errors().is_empty());
        let mut h = configuring();
        sets_ok(&mut h);
        h.cmd(Command::Stop);
        h.advance(REQUEST_TIMEOUT);
        assert!(h.errors().is_empty());
    }

    #[test]
    fn failure_after_a_failed_launch_stop_is_silent() {
        let mut h = configuring();
        h.fail("launch", "first");
        h.fail("configurationDone", "second");
        h.fail("setExceptionBreakpoints", "third");
        assert_eq!(h.errors(), ["first"]);
        assert_eq!(h.count("terminate"), 1);
    }

    #[test]
    fn continued_in_starting_bumps_the_epoch_and_keeps_the_phase() {
        let mut h = starting();
        h.event("continued", json!({"threadId": 1}));
        assert_eq!(h.s.epoch(), 1);
        assert_eq!(h.phase(), Phase::Starting);
        assert_eq!(h.last, vec![Action::Emit(DapBody::Running { epoch: 1 })]);
    }

    #[test]
    fn set_exception_breakpoints_failure_and_timeout_report_and_continue() {
        let mut h = configuring();
        h.fail("setExceptionBreakpoints", "nope");
        assert_eq!(h.last, vec![Action::Emit(DapBody::Error("setExceptionBreakpoints: nope".into()))]);
        assert_eq!(h.phase(), Phase::Starting);
        assert_eq!(h.count("terminate"), 0);

        let mut h = configuring();
        h.advance(REQUEST_TIMEOUT);
        assert_eq!(h.errors().iter().filter(|e| *e == "setExceptionBreakpoints timed out").count(), 1);
        assert_eq!(h.phase(), Phase::Starting);
        assert_eq!(h.count("terminate"), 0);
    }

    #[test]
    fn step_and_pause_timeouts_report_while_current_only() {
        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.take_emitted();
        h.advance(REQUEST_TIMEOUT - MS);
        assert!(h.errors().is_empty());
        h.advance(MS);
        assert_eq!(h.errors(), ["next timed out"]);

        let mut h = running();
        h.cmd(Command::Pause { thread: 7 });
        h.advance(REQUEST_TIMEOUT);
        assert_eq!(h.errors(), ["pause timed out"]);

        let mut h = suspended();
        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 7 });
        h.event("stopped", json!({"threadId": 7, "reason": "step"}));
        h.advance(REQUEST_TIMEOUT);
        assert!(h.errors().is_empty(), "stale step timeout: {:?}", h.errors());

        let mut h = running();
        h.cmd(Command::Pause { thread: 7 });
        h.event("stopped", json!({"threadId": 7, "reason": "pause"}));
        h.advance(REQUEST_TIMEOUT);
        assert!(h.errors().is_empty(), "stale pause timeout: {:?}", h.errors());
    }

    #[test]
    fn terminate_answered_either_way_changes_nothing() {
        for ok in [true, false] {
            let mut h = running();
            h.cmd(Command::Stop);
            let n = h.sent.len();
            if ok {
                h.answer("terminate", Value::Null);
            } else {
                h.fail("terminate", "unsupported");
            }
            assert!(h.last.is_empty(), "ok={ok}: {:?}", h.last);
            assert_eq!(h.sent.len(), n);
            assert_eq!(h.phase(), Phase::Stopping);
            assert!(h.errors().is_empty());
            h.advance(STOP_STAGE);
            assert_eq!(h.commands().last().unwrap(), "disconnect", "the stage deadline still runs");
        }
    }

    #[test]
    fn terminated_in_initializing_kills() {
        let mut h = H::new(&[]);
        h.event("terminated", json!({}));
        assert_eq!(h.last, end_actions(""));
        assert_eq!(h.commands(), ["initialize"]);
    }

    #[test]
    fn disconnect_failure_ends_like_success() {
        let mut h = running();
        h.cmd(Command::Stop);
        h.event("terminated", json!({}));
        h.fail("disconnect", "no");
        assert_eq!(h.last, end_actions(""));
        assert!(h.errors().is_empty());
    }

    #[test]
    fn set_breakpoints_command_in_stopping_is_dropped() {
        let mut h = running();
        h.cmd(Command::Stop);
        let n = h.sent.len();
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[1]), modified: false });
        assert!(h.last.is_empty());
        assert_eq!(h.sent.len(), n);
    }

    #[test]
    fn set_breakpoints_answer_in_stopping_is_dropped() {
        for ok in [true, false] {
            let mut h = configuring();
            h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[1]), modified: false });
            h.take_emitted();
            h.cmd(Command::Stop);
            if ok {
                h.answer("setBreakpoints", json!({"breakpoints": [{"verified": true}]}));
            } else {
                h.fail("setBreakpoints", "cancelled");
            }
            assert!(h.last.is_empty(), "ok={ok}: {:?}", h.last);
            assert!(breakpoint_emits(&h).is_empty() && h.errors().is_empty());
        }
    }

    #[test]
    fn set_breakpoints_between_initialized_and_the_initialize_response_goes_with_the_configuration() {
        let mut h = H::new(&[]);
        h.event("initialized", json!({}));
        h.cmd(Command::SetBreakpoints { source: "/a.c".into(), breakpoints: at(&[4]), modified: false });
        assert!(h.last.is_empty());
        assert_eq!(h.commands(), ["initialize"]);
        h.answer("initialize", caps());
        assert_eq!(h.commands(), ["initialize", "launch", "setBreakpoints", "setExceptionBreakpoints", "configurationDone"]);
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([{"line": 5}]));
    }

    #[test]
    fn request_command_in_starting_is_sent() {
        let mut h = starting();
        request(&mut h, "threads", 0);
        assert_eq!(h.commands().last().unwrap(), "threads");
        h.answer("threads", json!({"threads": []}));
        assert_eq!(h.last, vec![Action::Emit(DapBody::Response { tag: tag(0), result: Ok(json!({"threads": []})) })]);
    }

    // ---- the end

    fn ends() -> Vec<H> {
        let mut v = vec![];
        let mut h = H::new(&[]);
        h.cmd(Command::Stop);
        v.push(h);
        let mut h = running();
        h.feed(Input::Eof);
        v.push(h);
        let mut h = running();
        h.event("terminated", json!({}));
        h.answer("disconnect", Value::Null);
        v.push(h);
        let mut h = H::new(&[]);
        h.fail("initialize", "x");
        v.push(h);
        v
    }

    #[test]
    fn nothing_after_ended() {
        for mut h in ends() {
            assert_eq!(h.phase(), Phase::Ended);
            h.feed(Input::Tick);
            assert!(h.last.is_empty());
            h.feed(Input::Eof);
            assert!(h.last.is_empty());
            h.cmd(Command::Stop);
            assert!(h.last.is_empty());
            h.event("stopped", json!({"reason": "x"}));
            assert!(h.last.is_empty());
            h.raw(json!({"type": "response", "seq": 1, "request_seq": 1, "success": true, "command": "x"}));
            assert!(h.last.is_empty());
            h.reverse("runInTerminal", 3);
            assert!(h.last.is_empty());
            assert_eq!(h.phase(), Phase::Ended);
        }
    }

    #[test]
    fn finish_is_last_and_ended_is_just_before() {
        for h in ends() {
            let n = h.last.len();
            assert_eq!(h.last[n - 1], Action::Finish);
            assert!(matches!(&h.last[n - 2], Action::Emit(DapBody::Ended { .. })));
            assert_eq!(h.last[n - 3], Action::Emit(DapBody::Phase(Phase::Ended)));
            assert_eq!(h.emitted.last().map(|b| matches!(b, DapBody::Ended { .. })), Some(true));
        }
    }

    // ---- replay

    #[test]
    fn gdb_capture_replays() {
        let mut h = H::new(&[("/t.c", &[2])]);
        let gdb_caps =
            json!({"supportsConfigurationDoneRequest": true, "supportsTerminateRequest": true, "supportTerminateDebuggee": true});
        h.answer("initialize", gdb_caps);
        h.event("initialized", json!({}));
        assert_eq!(h.requests("setBreakpoints")[0]["arguments"]["breakpoints"], json!([{"line": 3}]));
        h.answer("setBreakpoints", json!({"breakpoints": [{"id": 1, "verified": false, "reason": "pending"}]}));
        assert!(
            matches!(&h.last[..], [Action::Emit(DapBody::Breakpoints { requested, result, .. })] if *requested == [2] && result.len() == 1)
        );
        h.event("output", json!({"category": "console", "output": "Reading symbols\n"}));
        h.answer("configurationDone", Value::Null);
        h.answer("launch", Value::Null);
        assert_eq!(h.phase(), Phase::Running);
        h.event("module", json!({"reason": "new", "module": {"id": 1, "name": "x"}}));
        h.event("breakpoint", json!({"reason": "changed", "breakpoint": {"id": 1, "verified": true, "line": 3}}));
        h.event("process", json!({"name": "/t"}));
        h.event("thread", json!({"reason": "started", "threadId": 1}));
        h.event("stopped", json!({"threadId": 1, "allThreadsStopped": true, "hitBreakpointIds": [1], "reason": "breakpoint"}));
        assert!(matches!(&h.last[1], Action::Emit(DapBody::Stopped { epoch: 1, .. })));

        let threads = DapTag { epoch: 1, kind: TagKind::Threads };
        h.cmd(Command::Request { command: "threads".into(), arguments: Value::Null, tag: threads.clone() });
        h.answer("threads", json!({"threads": [{"id": 1, "name": "t"}]}));
        assert!(matches!(&h.last[..], [Action::Emit(DapBody::Response { result: Ok(_), .. })]));
        let stack = DapTag { epoch: 1, kind: TagKind::StackTrace { thread: 1 } };
        h.cmd(Command::Request { command: "stackTrace".into(), arguments: json!({"threadId": 1, "levels": 20}), tag: stack });
        h.answer("stackTrace", json!({"stackFrames": [{"id": 0, "name": "f", "line": 3, "column": 0}]}));
        assert!(matches!(&h.last[..], [Action::Emit(DapBody::Response { result: Ok(_), .. })]));
        h.cmd(Command::Request { command: "variables".into(), arguments: json!({"variablesReference": 1}), tag: threads });

        h.cmd(Command::Resume { kind: ResumeKind::Next, thread: 1 });
        h.answer("next", Value::Null);
        h.event("stopped", json!({"threadId": 1, "reason": "step"}));
        assert!(matches!(&h.last[1], Action::Emit(DapBody::Stopped { epoch: 3, .. })));
        h.fail("variables", "list index out of range");
        assert!(h.last.is_empty());

        h.cmd(Command::Resume { kind: ResumeKind::StepOut, thread: 1 });
        h.answer("stepOut", Value::Null);
        h.event("stopped", json!({"threadId": 1, "reason": "step"}));
        h.cmd(Command::Resume { kind: ResumeKind::Continue, thread: 1 });
        h.answer("continue", json!({"allThreadsContinued": true}));
        h.event("output", json!({"category": "stdout", "output": "x=3 \u{1F600}\n"}));
        h.event("thread", json!({"reason": "exited", "threadId": 1}));
        h.event("exited", json!({"exitCode": 0}));
        assert_eq!(h.last, vec![Action::Emit(DapBody::Exited(0))]);
        h.event("terminated", json!({}));
        assert_eq!(h.commands().last().unwrap(), "disconnect");
        assert_eq!(h.requests("disconnect")[0]["arguments"], json!({"terminateDebuggee": true}));
        h.answer("disconnect", Value::Null);
        assert_eq!(h.last, end_actions(""));
        h.feed(Input::Eof);
        assert!(h.last.is_empty());

        assert_eq!(
            h.commands(),
            [
                "initialize",
                "launch",
                "setBreakpoints",
                "configurationDone",
                "threads",
                "stackTrace",
                "variables",
                "next",
                "stepOut",
                "continue",
                "disconnect"
            ]
        );
    }
}
