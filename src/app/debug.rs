//! The debug session in the App, as `app/run.rs` is for tasks: `debug.start` chooses a configuration and an
//! adapter off the main thread and starts a `DapClient`; `debug.stop` stops it; a replacing start, a root
//! switch and a quit end it gracefully. The machine (`dap::session`) and the driver (`dap::client`) own every
//! ordering rule of the protocol, so this file decides only which session is current.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::App;
use crate::actions::DebugAction;
use crate::dap::build::{Artifact, BuildTags, drive_build};
use crate::dap::client::{DapClient, SHUTDOWN_LIMIT};
use crate::dap::config::{
    AdapterConfig, DebugBlock, DebugConfig, LaunchVars, choose_adapter, default_block, fill_artifact, load_debug_blocks, needs_artifact,
    process_env, resolve_build, resolve_launch,
};
use crate::dap::console::spawn_console;
use crate::dap::panel::BuildStage;
use crate::dap::protocol::{Breakpoint, BreakpointEvent, OutputEvent, StackFrame, StackTraceBody, StoppedEvent, ThreadsBody, body_of};
use crate::dap::session::{Command, DapTag, Phase, ResumeKind, SessionId, SessionSpec, SourceBreakpoints, TagKind};
use crate::dap::{DapBody, DapMsg};
use crate::editor::buffer::{BpState, BreakpointMark};
use crate::editor::{Editor, ExecutionPoint};
use crate::event::{AppMsg, Level};
#[cfg(test)]
use crate::lsp::client::Conn;
use crate::lsp::client::spawn_process;
use crate::run::matcher::{Link, RunLine, Severity};
use crate::run::process::{RunEnd, Spec, clean, kill_group};
use crate::run::{RunTask, load_tasks, quote_word};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::{PanelId, ToolWindowId};

/// How adapters are reached: processes, or in-process fakes in tests.
#[derive(Default)]
enum Connector {
    #[default]
    Process,
    #[cfg(test)]
    Fake(Box<dyn FnMut() -> std::io::Result<Conn> + Send>),
}

/// The live session. Gone from `DebugState` as soon as it ends or is replaced, so its later messages are dropped.
pub(super) struct Live {
    pub(super) id: SessionId,
    pub(super) client: DapClient,
    pub(super) phase: Phase,
    pub(super) exit_code: Option<i64>,
    /// A user Stop: its end is reported as a stop, without the exit code gdb makes up for a killed program.
    pub(super) stop_requested: bool,
    /// What the adapter was last told, per source: never an empty list, a cleared source leaves it.
    pub(super) sent: BTreeMap<PathBuf, Vec<usize>>,
    /// The sources in `sent` that had an open buffer at the last comparison.
    pub(super) open: BTreeSet<PathBuf>,
    /// The epoch of the last `Stopped` or `Running`; the tag of the requests the App makes.
    pub(super) epoch: u64,
    /// The thread the last stop was on, kept through `Running` for `debug.pause`.
    pub(super) thread: Option<i64>,
    /// The program's output goes to the Console thread; dropping this ends it after it flushed what it holds.
    pub(super) console: std::sync::mpsc::Sender<OutputEvent>,
}

/// Kills the build's process group when dropped: a replace, a Stop, a root switch, a quit, or the app going.
/// The id is 0 once the leader is reaped, so a reused id is never signalled.
struct BuildGroup(Arc<AtomicU32>);

impl Drop for BuildGroup {
    fn drop(&mut self) {
        kill_group(self.0.load(Ordering::SeqCst));
    }
}

/// A build step that is running; its `DebugBuilt` is what ends the stage.
struct Building {
    _group: BuildGroup,
    prepared: Prepared,
    session: SessionId,
    /// The build words ask for Cargo's JSON stream, so a build without an executable is not a missing flag.
    json: bool,
}

/// A finished build that made several executables, waiting for the user to pick one.
struct Choice {
    prepared: Prepared,
    session: SessionId,
    artifacts: Vec<Artifact>,
    /// The dialog was opened.
    offered: bool,
}

#[derive(Default)]
pub struct DebugState {
    session: Option<Live>,
    /// The last `SessionId` handed out.
    next_id: u64,
    /// `debug.start`s so far; only the newest one's `DebugReady` is used.
    seq: u64,
    /// A `DebugReady` of the current seq is on its way.
    starting: bool,
    /// The end of what is stopping or preparing: a new start waits for it, so two adapters never overlap.
    retiring: Option<JoinHandle<()>>,
    connector: Connector,
    /// The Debug window was shown by a first start in this run; a later start leaves it as the user has it.
    window_shown: bool,
    building: Option<Building>,
    choice: Option<Choice>,
}

impl DebugState {
    /// A session is live and not over.
    pub fn active(&self) -> bool {
        self.phase().is_some_and(|p| p != Phase::Ended)
    }

    /// `debug.start` waits for its choice of adapter.
    pub fn starting(&self) -> bool {
        self.starting
    }

    pub fn suspended(&self) -> bool {
        self.phase() == Some(Phase::Suspended)
    }

    pub fn phase(&self) -> Option<Phase> {
        self.session.as_ref().map(|l| l.phase)
    }

    pub fn send(&self, cmd: Command) {
        if let Some(live) = &self.session {
            live.client.send(cmd);
        }
    }

    /// Stops the live session gracefully and returns what to wait for. `DapClient::shutdown` sends its own `Stop`.
    fn retire(&mut self) -> Option<JoinHandle<()>> {
        // Dropping the build kills its process group.
        self.building = None;
        self.choice = None;
        let before = self.retiring.take();
        let Some(live) = self.session.take() else { return before };
        let stop = live.client.shutdown();
        Some(tokio::spawn(async move {
            if let Some(b) = before {
                let _ = b.await;
            }
            stop.await;
        }))
    }

    /// A build step runs, or its executables wait for a choice.
    fn pending_build(&self) -> bool {
        self.building.is_some() || self.choice.is_some()
    }

    fn new_session_id(&mut self) -> SessionId {
        self.next_id += 1;
        SessionId(self.next_id)
    }
}

/// What `debug.start` chose, ready to launch.
#[derive(Debug)]
pub struct Prepared {
    pub name: String,
    pub adapter: AdapterConfig,
    pub launch: Value,
    /// The step to run before the adapter; taken when it starts.
    pub build: Option<Spec>,
}

struct Job {
    root: PathBuf,
    debug: DebugConfig,
    tasks: Vec<RunTask>,
    blocks: BTreeMap<String, DebugBlock>,
    selected: Option<String>,
    language: Option<String>,
    file: Option<String>,
}

/// Whether the file's text differs from what is on disk: an open buffer's dirty state; a closed file has none.
fn source_modified(editor: &Editor, path: &Path) -> bool {
    editor.index_of(path).is_some_and(|i| editor.buffers[i].dirty())
}

/// The sources in `lines` with an open buffer.
fn open_sources(editor: &Editor, lines: &BTreeMap<PathBuf, Vec<usize>>) -> BTreeSet<PathBuf> {
    lines.keys().filter(|p| editor.index_of(p).is_some()).cloned().collect()
}

/// What to send: the sources gone from `now` with an empty list, since `all_breakpoints` has no empty ones and an
/// empty list is what clears; those whose lines changed; those that became open, because a closed file's lines hold
/// no state, so a reopened one has to be told again what the adapter thinks of them.
fn breakpoint_changes(
    sent: &BTreeMap<PathBuf, Vec<usize>>,
    was_open: &BTreeSet<PathBuf>,
    now: &BTreeMap<PathBuf, Vec<usize>>,
    open: &BTreeSet<PathBuf>,
) -> BTreeMap<PathBuf, Vec<usize>> {
    let mut changes: BTreeMap<PathBuf, Vec<usize>> =
        sent.keys().filter(|p| !now.contains_key(*p)).map(|p| (p.clone(), Vec::new())).collect();
    for (path, lines) in now {
        if sent.get(path) != Some(lines) || (open.contains(path) && !was_open.contains(path)) {
            changes.insert(path.clone(), lines.clone());
        }
    }
    changes
}

/// What the adapter says of a mark on `line`. The key is the user's line and never moves: another line goes into `adapter_line`.
fn apply_breakpoint(line: usize, mark: &mut BreakpointMark, bp: &Breakpoint) {
    mark.state = if bp.reason.as_deref() == Some("failed") {
        BpState::Failed
    } else if bp.verified {
        BpState::Verified
    } else {
        BpState::Pending
    };
    if bp.id.is_some() {
        mark.id = bp.id;
    }
    // On the wire lines are 1-based; 0 or absent says nothing about where it is.
    if let Some(l) = bp.line.filter(|l| *l > 0) {
        let adapter = (l - 1) as usize;
        mark.adapter_line = (adapter != line).then_some(adapter);
    }
    mark.message = bp.message.clone();
}

/// The Console line for a verdict the user should read: a failed breakpoint, or one the adapter put on another
/// line. A verified breakpoint on its own line, or a pending one, says nothing. The location is the user's line.
/// An adapter's message as one display line: each of its lines cleaned of escape sequences and control
/// characters, the non-empty ones joined by a space.
fn one_line(message: &str) -> String {
    let lines: Vec<String> = message.split(['\n', '\r']).map(|l| clean(l.as_bytes())).filter(|l| !l.is_empty()).collect();
    lines.join(" ")
}

fn verdict_line(source: &Path, line: usize, mark: &BreakpointMark) -> Option<RunLine> {
    let file = source.file_name()?.to_string_lossy();
    let place = format!("{file}:{}", line + 1);
    let (rest, severity, target) = if mark.state == BpState::Failed {
        let rest = match &mark.message {
            Some(m) => format!(" failed: {}", one_line(m)),
            None => " failed".to_string(),
        };
        (rest, Some(Severity::Warning), line)
    } else {
        let moved = mark.adapter_line?;
        (format!(" moved to line {}", moved + 1), None, moved)
    };
    let link = Link { path: source.to_path_buf(), line: target, col: 0, span: 11..11 + place.len() };
    Some(RunLine { text: format!("Breakpoint {place}{rest}"), severity, link: Some(link) })
}

/// The file and 0-based line a frame is at, when it has a source file: not when `line` is 0, the source is absent
/// or has a `sourceReference` (not a file), or its path is empty. A relative path is joined to the root. The frame
/// id and column are not read, so 0 is no special value.
fn frame_location(frame: &StackFrame, root: &Path) -> Option<(PathBuf, usize)> {
    if frame.line <= 0 {
        return None;
    }
    let source = frame.source.as_ref()?;
    if source.source_reference.is_some_and(|r| r > 0) {
        return None;
    }
    let path = source.path.as_deref().filter(|p| !p.is_empty())?;
    Some((root.join(path), (frame.line - 1) as usize))
}

/// Reads the project files, picks the configuration and the adapter, and resolves the launch arguments.
fn prepare(job: Job) -> Result<Prepared, String> {
    let (tasks, tasks_error) = load_tasks(&job.root, &job.tasks);
    let (blocks, blocks_error) = load_debug_blocks(&job.root, &job.blocks);
    // `load_debug_blocks` is the only reporter of a bad block: Run does not show it.
    if let Some(e) = blocks_error.or(tasks_error) {
        return Err(e);
    }
    let block_of = |t: &RunTask| blocks.get(&t.name).cloned().or_else(|| default_block(t));
    let (task, block) = match &job.selected {
        Some(name) => {
            let task = tasks.iter().find(|t| &t.name == name).ok_or_else(|| format!("Run configuration '{name}' no longer exists"))?;
            let block =
                block_of(task).ok_or_else(|| format!("Configuration '{name}' has no [debug.\"{name}\"] table in .jetl/run.toml"))?;
            (task, block)
        }
        // A written block wins over a default, whatever the order of the tasks.
        None => tasks
            .iter()
            .find_map(|t| blocks.get(&t.name).map(|b| (t, b.clone())))
            .or_else(|| tasks.iter().find_map(|t| default_block(t).map(|b| (t, b))))
            .ok_or_else(|| "No debug configuration: add [debug.<name>] to .jetl/run.toml".to_string())?,
    };
    let root = job.root.as_path();
    let adapter =
        choose_adapter(&job.debug, &block, job.language.as_deref(), &|cmd| crate::doctor::resolve(cmd, root, &crate::util::fs::which))?;
    let vars = LaunchVars { workspace_folder: root.to_string_lossy().into_owned(), file: job.file, artifact: None };
    let launch = resolve_launch(&adapter, &block, task, &vars, &process_env);
    let build = resolve_build(&block, task, &vars);
    if build.is_none() && needs_artifact(&launch) {
        return Err(format!("Cannot debug '{}': ${{artifact}} needs a build step: add build = [...] to its debug table", task.name));
    }
    check_line_breaks(&task.name, &launch)?;
    Ok(Prepared { name: task.name.clone(), adapter, launch, build })
}

/// gdb runs `cd <cwd>` and `file <program>`, and `gdb.execute` runs each line of a string as a command.
fn check_line_breaks(name: &str, launch: &Value) -> Result<(), String> {
    for (key, what) in [("cwd", "its working directory"), ("program", "its program")] {
        if launch[key].as_str().is_some_and(|v| v.contains(['\n', '\r'])) {
            return Err(format!("Cannot debug '{name}': line break in {what}"));
        }
    }
    Ok(())
}

/// Whether the build words ask Cargo for its JSON stream: `--message-format=json...` or `--message-format json...`.
fn asks_for_json(argv: &[String]) -> bool {
    argv.iter().any(|w| w.starts_with("--message-format=json"))
        || argv.windows(2).any(|w| w[0] == "--message-format" && w[1].starts_with("json"))
}

impl App {
    pub(super) fn perform_debug(&mut self, a: DebugAction) {
        match a {
            DebugAction::Start => self.debug_start(),
            DebugAction::Stop => self.debug_stop(),
            DebugAction::Resume => self.debug_resume(ResumeKind::Continue),
            DebugAction::StepOver => self.debug_resume(ResumeKind::Next),
            DebugAction::StepInto => self.debug_resume(ResumeKind::StepIn),
            DebugAction::StepOut => self.debug_resume(ResumeKind::StepOut),
            DebugAction::Pause => self.debug_pause(),
            DebugAction::ShowExecutionPoint => self.debug_show_execution_point(),
            // The Console's list keys: `perform` hands them to the panel.
            DebugAction::Up
            | DebugAction::Down
            | DebugAction::PageUp
            | DebugAction::PageDown
            | DebugAction::Home
            | DebugAction::End
            | DebugAction::Open => {}
        }
    }

    fn debug_start(&mut self) {
        // The build the press replaces is gone below; a start that is refused must not leave `building` on screen.
        if self.debug.pending_build() {
            self.debug_panel.set_stage(Some(BuildStage::NotStarted));
        }
        let before = self.debug.retire();
        // The live session's `Ended` will be dropped, so its marks' states go now.
        self.editor.reset_breakpoint_states();
        self.editor.set_execution_point(None);
        self.debug.seq += 1;
        self.debug.starting = true;
        let (seq, generation, tx) = (self.debug.seq, self.workspace.generation, self.bus_tx.clone());
        let job = Job {
            root: self.workspace.root.clone(),
            debug: self.config.debug.clone(),
            tasks: self.config.run.all(),
            blocks: self.config.run.debug.clone(),
            selected: self.run.selected.clone(),
            language: self.editor.active_buffer().and_then(|b| b.language()).map(|l| l.name.to_string()),
            file: self.editor.active_path().map(|p| p.to_string_lossy().into_owned()),
        };
        self.debug.retiring = Some(tokio::spawn(async move {
            if let Some(b) = before {
                let _ = b.await;
            }
            let result = tokio::task::spawn_blocking(move || prepare(job)).await.unwrap_or_else(|e| Err(format!("Debug: {e}")));
            let _ = tx.send(AppMsg::DebugReady { generation, seq, result: Box::new(result) });
        }));
    }

    fn debug_stop(&mut self) {
        if let Some(live) = self.debug.session.as_mut() {
            live.stop_requested = true;
            live.client.send(Command::Stop);
        } else if self.debug.starting {
            if self.debug.pending_build() {
                self.debug_panel.set_stage(Some(BuildStage::NotStarted));
            }
            self.debug.building = None;
            self.debug.choice = None;
            self.debug.starting = false;
            self.debug.seq += 1;
            self.notify(Level::Info, "Debug start cancelled");
        } else {
            self.notify(Level::Info, "No debug session is running");
        }
    }

    pub(super) fn debug_ready(&mut self, generation: u64, seq: u64, result: Result<Prepared, String>) {
        if generation != self.workspace.generation || seq != self.debug.seq {
            tracing::debug!("dropping the start of an older debug session");
            return;
        }
        let mut p = match result {
            Ok(p) => p,
            Err(e) => {
                self.debug.starting = false;
                return self.notify(Level::Warn, e);
            }
        };
        match p.build.take() {
            Some(spec) => self.debug_begin_build(p, spec),
            None => self.debug_launch(p, None),
        }
    }

    /// The build step is a stage of the start: the session id is allocated now, so its lines are an ordinary
    /// `DebugOutput` and a newer start's id drops what an older build prints late. `starting` stays true.
    fn debug_begin_build(&mut self, p: Prepared, spec: Spec) {
        let id = self.debug.new_session_id();
        self.debug_panel.start(&p.name, id);
        self.debug_panel.set_stage(Some(BuildStage::Building));
        let words: Vec<String> = spec.argv.iter().map(|w| quote_word(w)).collect();
        self.debug_panel.push(vec![RunLine::plain(format!("$ {}", words.join(" ")))]);
        self.debug_show_window();
        self.notify(Level::Info, format!("Building {}", p.name));
        if !spec.cwd.is_dir() {
            return self.debug_build_failed(format!("Cannot build '{}': {} is not a directory", p.name, spec.cwd.display()));
        }
        let json = asks_for_json(&spec.argv);
        let task = match crate::run::process::spawn(&spec) {
            Ok(t) => t,
            Err(e) => {
                let word = spec.argv.first().map_or("", String::as_str);
                return self.debug_build_failed(format!("Cannot build '{}': cannot run {word}: {e}", p.name));
            }
        };
        let pid = Arc::new(AtomicU32::new(task.pid));
        let tags = BuildTags { generation: self.workspace.generation, seq: self.debug.seq, session: id };
        let drive = drive_build(task, spec.cwd, self.workspace.root.clone(), tags, self.bus_tx.clone(), pid.clone());
        // The prepare task that held this has finished: it sent the message being handled.
        self.debug.retiring = Some(tokio::spawn(drive));
        self.debug.building = Some(Building { _group: BuildGroup(pid), prepared: p, session: id, json });
    }

    /// The start ends here: the title, the Console and a toast say why.
    fn debug_build_failed(&mut self, text: String) {
        self.debug.starting = false;
        self.debug_panel.set_stage(Some(BuildStage::Failed));
        let mut line = RunLine::plain(text.clone());
        line.severity = Some(Severity::Error);
        self.debug_panel.push(vec![line]);
        self.notify(Level::Warn, text);
    }

    /// A start refused after its build: nothing is launched.
    fn debug_refuse(&mut self, text: String) {
        self.debug.starting = false;
        self.debug_panel.set_stage(Some(BuildStage::NotStarted));
        self.notify(Level::Warn, text);
    }

    pub(super) fn debug_built(&mut self, generation: u64, seq: u64, end: RunEnd, artifacts: Vec<Artifact>) {
        if generation != self.workspace.generation || seq != self.debug.seq {
            tracing::debug!("dropping the end of an older debug build");
            return;
        }
        let Some(Building { prepared: p, session, json, .. }) = self.debug.building.take() else { return };
        let name = &p.name;
        match end {
            RunEnd::Code(0) => {}
            RunEnd::Code(c) => return self.debug_build_failed(format!("Build of '{name}' failed with exit code {c}")),
            RunEnd::Signal(s) => return self.debug_build_failed(format!("Build of '{name}' was killed by signal {s}")),
            RunEnd::Failed(e) => return self.debug_build_failed(format!("Build of '{name}' failed: {e}")),
        }
        if !needs_artifact(&p.launch) {
            return self.debug_launch(p, Some(session));
        }
        match artifacts.len() {
            0 => {
                let advice = if json { "" } else { ". A Cargo build needs --message-format=json" };
                self.debug_refuse(format!("Cannot debug '{name}': its build named no executable, so ${{artifact}} has no value{advice}"))
            }
            1 => self.debug_launch_artifact(p, session, &artifacts[0]),
            _ => {
                self.debug_panel.set_stage(Some(BuildStage::Choosing));
                self.debug.choice = Some(Choice { prepared: p, session, artifacts, offered: false });
                self.debug_offer_artifacts();
            }
        }
    }

    /// Also on the tick: nothing is offered over a dialog, and the open one keeps its text.
    pub(super) fn debug_offer_artifacts(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        let root = self.workspace.root.clone();
        let seq = self.debug.seq;
        let Some(c) = self.debug.choice.as_mut().filter(|c| !c.offered) else { return };
        c.offered = true;
        let items: Vec<String> = c.artifacts.iter().map(|a| a.label(&root)).collect();
        let message = format!("The build made {} executables. Which one?", items.len());
        self.dialog = Some(Dialog::choice(DialogId::DebugArtifact { seq }, "Debug", &message, items));
    }

    /// The dialog's answer: `None` is a cancel, which starts nothing and stops nothing else.
    pub(super) fn debug_artifact_chosen(&mut self, seq: u64, i: Option<usize>) {
        if seq != self.debug.seq {
            return;
        }
        let Some(c) = self.debug.choice.take() else { return };
        match i.and_then(|i| c.artifacts.get(i).cloned()) {
            Some(a) => self.debug_launch_artifact(c.prepared, c.session, &a),
            None => {
                self.debug.starting = false;
                self.debug_panel.set_stage(Some(BuildStage::NotStarted));
                self.notify(Level::Info, "Debug start cancelled");
            }
        }
    }

    fn debug_launch_artifact(&mut self, mut p: Prepared, session: SessionId, artifact: &Artifact) {
        let path = artifact.path.to_string_lossy();
        // A line break would run as a gdb command; see `check_line_breaks`.
        if path.contains(['\n', '\r']) {
            return self.debug_refuse(format!("Cannot debug '{}': line break in the path of its executable", p.name));
        }
        fill_artifact(&mut p.launch, &path);
        self.debug_launch(p, Some(session));
    }

    /// Starts the adapter. `session` is the id the build's Console already has; without a build the id and the
    /// Console are new, and a failed adapter start leaves the old Console as it was.
    fn debug_launch(&mut self, p: Prepared, session: Option<SessionId>) {
        self.debug.starting = false;
        let generation = self.workspace.generation;
        let root = self.workspace.root.clone();
        let conn = match &mut self.debug.connector {
            Connector::Process => spawn_process(&p.adapter.name, &p.adapter.command, &root),
            #[cfg(test)]
            Connector::Fake(connect) => connect(),
        };
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                if session.is_some() {
                    self.debug_panel.set_stage(Some(BuildStage::NotStarted));
                }
                return self.notify(Level::Error, format!("Debug adapter '{}' could not start: {e}", p.adapter.name));
            }
        };
        let id = session.unwrap_or_else(|| self.debug.new_session_id());
        let sent = self.editor.all_breakpoints();
        let open = open_sources(&self.editor, &sent);
        tracing::debug!("dap: breakpoints at start: {sent:?}");
        let breakpoints = sent
            .iter()
            .map(|(p, lines)| (p.clone(), SourceBreakpoints { lines: lines.clone(), modified: source_modified(&self.editor, p) }))
            .collect();
        let cwd = p.launch["cwd"].as_str().map_or_else(|| root.clone(), PathBuf::from);
        let spec = SessionSpec { id, generation, adapter_id: p.adapter.name.clone(), launch: p.launch, breakpoints };
        let output_tx = self.bus_tx.clone();
        let console = spawn_console(cwd, root.clone(), move |lines| {
            let _ = output_tx.send(AppMsg::DebugOutput { generation, session: id, lines });
        });
        if session.is_none() {
            self.debug_panel.start(&p.name, id);
        } else {
            self.debug_panel.set_stage(None);
        }
        let tx = self.bus_tx.clone();
        let client = DapClient::start(spec, conn, move |m| {
            let _ = tx.send(AppMsg::Dap(m));
        });
        self.debug.session = Some(Live {
            id,
            client,
            phase: Phase::Initializing,
            exit_code: None,
            stop_requested: false,
            sent,
            open,
            epoch: 0,
            thread: None,
            console,
        });
        self.notify(Level::Info, format!("Debugging {} with {}", p.name, p.adapter.name));
        self.debug_show_window();
    }

    /// The first start in this run shows the Debug window without taking focus; a later one leaves it as it is.
    fn debug_show_window(&mut self) {
        if self.debug.window_shown {
            return;
        }
        self.debug.window_shown = true;
        self.layout.show(ToolWindowId::Debug);
        // `show` replaces the slot's occupant and leaves focus alone.
        if let Some(cur) = self.focus.current.tool_window()
            && !self.layout.is_visible(cur)
        {
            self.focus.set(PanelId::Editor);
        }
    }

    pub(super) fn dap_msg(&mut self, m: DapMsg) {
        let current = m.generation == self.workspace.generation && self.debug.session.as_ref().is_some_and(|l| l.id == m.session);
        if !current {
            tracing::debug!("dropping a message of an old debug session");
            return;
        }
        match m.body {
            DapBody::Phase(p) => {
                if let Some(l) = self.debug.session.as_mut() {
                    l.phase = p;
                }
            }
            DapBody::Exited(c) => {
                if let Some(l) = self.debug.session.as_mut() {
                    l.exit_code = Some(c);
                }
            }
            DapBody::Error(e) => self.notify(Level::Error, e),
            DapBody::Stopped { epoch, event } => self.debug_stopped(epoch, event),
            DapBody::Running { epoch } => self.debug_running(epoch),
            DapBody::Response { tag, result } => self.debug_response(tag, result),
            DapBody::Ended { reason } => self.debug_ended(reason),
            DapBody::Breakpoints { source, requested, result } => self.debug_breakpoints_answered(&source, &requested, &result),
            DapBody::BreakpointChanged(e) => self.debug_breakpoint_changed(&e),
            // Split and matched on the Console thread: a program can print 100 000 lines a second.
            DapBody::Output(e) => {
                if let Some(l) = &self.debug.session {
                    let _ = l.console.send(e);
                }
            }
            DapBody::Started { .. } => {}
        }
    }

    /// A batch from the Console thread. Judged by the panel's session, not by `Live`: the thread flushes what it
    /// holds after the session ended.
    pub(super) fn debug_output(&mut self, generation: u64, session: SessionId, lines: Vec<RunLine>) {
        if generation != self.workspace.generation || self.debug_panel.session() != Some(session) {
            tracing::debug!("dropping the output of an old debug session");
            return;
        }
        self.debug_panel.push(lines);
    }

    /// A stop: asks for the stack of the stopped thread, or for the threads when the event names none.
    fn debug_stopped(&mut self, epoch: u64, event: StoppedEvent) {
        self.debug_panel.set_reason(Some(event.reason.clone()));
        // A shown window has the reason in its title; otherwise nothing says the program stopped. Another maximized
        // window covers it and the editor, and a stop does not move focus, so that one stays.
        let shown =
            self.layout.is_visible(ToolWindowId::Debug) && self.layout.maximized_window().is_none_or(|tw| tw == ToolWindowId::Debug);
        if !shown {
            self.notify(Level::Info, format!("Paused ({})", event.reason));
        }
        let Some(live) = self.debug.session.as_mut() else { return };
        live.epoch = epoch;
        live.thread = event.thread_id;
        match event.thread_id {
            Some(t) => self.debug_ask_stack(epoch, t),
            None => self.debug_ask_threads(epoch),
        }
    }

    fn debug_ask_threads(&mut self, epoch: u64) {
        let tag = DapTag { epoch, kind: TagKind::Threads };
        self.debug.send(Command::Request { command: "threads".into(), arguments: json!({}), tag });
    }

    fn debug_ask_stack(&mut self, epoch: u64, thread: i64) {
        let tag = DapTag { epoch, kind: TagKind::StackTrace { thread } };
        let arguments = json!({"threadId": thread, "startFrame": 0, "levels": 20});
        self.debug.send(Command::Request { command: "stackTrace".into(), arguments, tag });
    }

    fn debug_running(&mut self, epoch: u64) {
        if let Some(live) = self.debug.session.as_mut() {
            live.epoch = epoch;
        }
        self.debug_panel.set_reason(None);
        self.editor.set_execution_point(None);
    }

    /// An answer to `threads` or `stackTrace`. No epoch check here: the machine drops an answer older than its
    /// epoch (rule 7). `Stop` does not bump the epoch, so a frame answer is applied only while `Suspended`. One already
    /// on the bus when a step is sent is applied too, and the step's `Running` then clears the point.
    fn debug_response(&mut self, tag: DapTag, result: Result<Value, String>) {
        let phase = self.debug.phase();
        let body = match result {
            Ok(b) => b,
            Err(e) => {
                // Shutdown answers what is still outstanding with `not available`.
                if matches!(phase, Some(Phase::Stopping | Phase::Ended)) {
                    return;
                }
                let what = match tag.kind {
                    TagKind::Threads => "threads",
                    TagKind::StackTrace { .. } => "stack",
                };
                return self.notify(Level::Warn, format!("Could not read the {what}: {e}"));
            }
        };
        match tag.kind {
            TagKind::Threads => {
                let threads = body_of::<ThreadsBody>("threads", body).threads;
                let Some(first) = threads.first().map(|t| t.id) else {
                    return self.notify(Level::Warn, "The adapter reports no threads");
                };
                // Each resume or pause bumps the epoch, so a `Threads` answer that gets here while `Running`
                // can only come from `debug_pause`: no flag and no tag kind of its own is needed.
                match phase {
                    Some(Phase::Suspended) => {
                        if let Some(live) = self.debug.session.as_mut() {
                            live.thread = Some(first);
                        }
                        self.debug_ask_stack(tag.epoch, first);
                    }
                    Some(Phase::Running) => {
                        if let Some(live) = self.debug.session.as_mut() {
                            live.thread = Some(first);
                        }
                        self.debug.send(Command::Pause { thread: first });
                    }
                    _ => {}
                }
            }
            TagKind::StackTrace { .. } => {
                // `Stop` does not bump the epoch, so an answer can still arrive while `Stopping`.
                if phase != Some(Phase::Suspended) {
                    return;
                }
                let frames = body_of::<StackTraceBody>("stackTrace", body).stack_frames;
                match frames.first() {
                    Some(frame) => self.debug_show_frame(frame),
                    None => {
                        self.editor.set_execution_point(None);
                        self.notify(Level::Warn, "Paused, the adapter reports no stack");
                    }
                }
            }
        }
    }

    /// Opens the frame's file at its line without taking focus, and marks the line.
    fn debug_show_frame(&mut self, frame: &StackFrame) {
        match frame_location(frame, &self.workspace.root) {
            Some((path, line)) => {
                tracing::debug!("dap: execution point {}:{line}", path.display());
                self.editor.set_execution_point(Some(ExecutionPoint { path: path.clone(), line }));
                self.open_file_at_unfocused(path, line, 0);
            }
            None => {
                self.editor.set_execution_point(None);
                self.notify(Level::Info, format!("Paused in {}, no source", frame.name));
            }
        }
    }

    fn debug_resume(&mut self, kind: ResumeKind) {
        if !self.debug.suspended() {
            return self.notify(Level::Info, "No paused debug session");
        }
        let Some(live) = self.debug.session.as_ref() else { return };
        let Some(thread) = live.thread else {
            // The stop's `threads` may have failed or come back empty: ask again, so the next key can work.
            let epoch = live.epoch;
            self.debug_ask_threads(epoch);
            return self.notify(Level::Info, "The paused thread is not known yet");
        };
        // A breakpoint cleared since the last tick has to reach the adapter before the program runs.
        self.debug_sync_breakpoints();
        self.debug.send(Command::Resume { kind, thread });
    }

    fn debug_pause(&mut self) {
        if self.debug.phase() != Some(Phase::Running) {
            return self.notify(Level::Info, "No running debug session");
        }
        let Some(live) = self.debug.session.as_ref() else { return };
        match live.thread {
            Some(thread) => self.debug.send(Command::Pause { thread }),
            None => {
                let epoch = live.epoch;
                self.debug_ask_threads(epoch);
            }
        }
    }

    fn debug_show_execution_point(&mut self) {
        let Some(p) = self.editor.execution_point().cloned() else {
            return self.notify(Level::Info, "No execution point");
        };
        self.open_file_at(p.path, p.line, 0);
    }

    /// An answer to the newest `setBreakpoints` of a source; results are positional. A closed file has no marks to
    /// update, and a mark gone since the request is not brought back.
    fn debug_breakpoints_answered(&mut self, source: &Path, requested: &[usize], result: &[Breakpoint]) {
        tracing::debug!("dap: breakpoints of {}: {requested:?} answered {result:?}", source.display());
        let Some(marks) = self.editor.breakpoint_marks_mut(source) else { return };
        let mut lines = Vec::new();
        for (line, bp) in requested.iter().zip(result) {
            if let Some(mark) = marks.get_mut(line) {
                let before = (mark.state, mark.adapter_line, mark.message.clone());
                *mark = BreakpointMark::default();
                apply_breakpoint(*line, mark, bp);
                tracing::debug!("dap: mark {}:{line} is {:?}, adapter line {:?}", source.display(), mark.state, mark.adapter_line);
                if before != (mark.state, mark.adapter_line, mark.message.clone()) {
                    lines.extend(verdict_line(source, *line, mark));
                }
            }
        }
        if !lines.is_empty() {
            self.debug_panel.push(lines);
        }
    }

    fn debug_breakpoint_changed(&mut self, e: &BreakpointEvent) {
        tracing::debug!("dap: breakpoint {}: {:?}", e.reason, e.breakpoint);
        let Some(id) = e.breakpoint.id else { return };
        for (line, mark) in self.editor.breakpoint_marks_by_id_mut(id) {
            if e.reason == "removed" {
                // gdb's `removed` carries the old descriptor, so `verified` says nothing here.
                *mark =
                    BreakpointMark { state: BpState::Failed, message: Some("removed by the debug adapter".into()), ..Default::default() };
            } else {
                apply_breakpoint(line, mark, &e.breakpoint);
            }
            tracing::debug!("dap: mark with id {id} on line {line} is {:?}, adapter line {:?}", mark.state, mark.adapter_line);
        }
    }

    /// On the tick: tells the adapter of each source that changed since the last time. The machine holds a change that
    /// comes before its configuration in the snapshot.
    pub(super) fn debug_sync_breakpoints(&mut self) {
        if !self.debug.active() {
            return;
        }
        let now = self.editor.all_breakpoints();
        let open = open_sources(&self.editor, &now);
        let Some(live) = self.debug.session.as_mut() else { return };
        let changes = breakpoint_changes(&live.sent, &live.open, &now, &open);
        live.sent = now;
        live.open = open;
        for (source, lines) in changes {
            let modified = source_modified(&self.editor, &source);
            tracing::debug!("dap: sending breakpoints of {}: {lines:?}, modified {modified}", source.display());
            self.debug.send(Command::SetBreakpoints { source, lines, modified });
        }
    }

    fn debug_ended(&mut self, reason: String) {
        let Some(live) = self.debug.session.take() else { return };
        self.editor.reset_breakpoint_states();
        self.editor.set_execution_point(None);
        let (level, text) = match (live.stop_requested, reason.is_empty(), live.exit_code) {
            (_, false, Some(c)) => (Level::Warn, format!("Debug session ended: {reason} (exit code {c})")),
            (_, false, None) => (Level::Warn, format!("Debug session ended: {reason}")),
            (true, true, _) => (Level::Info, "Debug session stopped".to_string()),
            (false, true, Some(0)) => (Level::Info, "Debug session ended, exit code 0".to_string()),
            (false, true, Some(c)) => (Level::Warn, format!("Debug session ended, exit code {c}")),
            (false, true, None) => (Level::Info, "Debug session ended".to_string()),
        };
        self.notify(level, text);
    }

    /// A root switch: the session belongs to the old root. Called before the generation changes.
    pub(crate) fn debug_root_changed(&mut self) {
        if self.dialog.as_ref().is_some_and(|d| matches!(d.id, DialogId::DebugArtifact { .. })) {
            self.dialog = None;
        }
        self.debug.retiring = self.debug.retire();
        self.debug_panel.clear();
        self.editor.reset_breakpoint_states();
        self.editor.set_execution_point(None);
        self.debug.starting = false;
        self.debug.seq += 1;
    }

    /// For `App::shutdown`: ends the session and whatever is still stopping, within a bound.
    // `use<>`: edition 2024 would otherwise capture `&mut self`, and `App::shutdown` joins this with `self.lsp`.
    pub(super) fn debug_finish(&mut self) -> impl Future<Output = ()> + Send + use<> {
        let before = self.debug.retire();
        self.debug.starting = false;
        self.debug.seq += 1;
        async move {
            if let Some(b) = before {
                // What this abandons: the adapter and its process group are killed with the runtime (the driver's `Group` kills on drop).
                // A debuggee in a group of its own goes only if the adapter's death takes it; see ARCHITECTURE.
                let _ = tokio::time::timeout(SHUTDOWN_LIMIT + Duration::from_secs(1), b).await;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::actions::Action;
    use crate::app::tests::{handle_next, load, test_app};
    use crate::dap::config::AdapterOverride;
    use crate::dap::fake::{self, FakeAdapter, Order};
    use crate::dap::session::STOP_STAGE;
    use crate::editor::undo::TxKind;
    use crate::ui::focus::PanelId;

    pub(crate) const DEMO: &str = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nadapter = \"fake\"\nlaunch = { program = \"${workspaceFolder}/demo\" }\n";

    pub(crate) fn caps() -> Value {
        json!({"supportsConfigurationDoneRequest": true, "supportsTerminateRequest": true, "supportTerminateDebuggee": true})
    }

    pub(crate) fn with_fake_adapters(app: &mut App, caps: Value, order: Order) -> UnboundedReceiver<FakeAdapter> {
        app.config.debug.adapters.insert(
            "fake".into(),
            AdapterOverride { command: Some(vec!["sh".into()]), languages: Some(vec!["c".into(), "rust".into()]), launch: None },
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.debug.connector = Connector::Fake(Box::new(move || {
            let (conn, fake) = fake::connect(caps.clone(), order);
            let _ = tx.send(fake);
            Ok(conn)
        }));
        rx
    }

    /// Handles debug messages until `done`.
    pub(crate) async fn dap_until(app: &mut App, done: impl Fn(&App) -> bool) {
        while !done(app) {
            handle_next(app, |m| matches!(m, AppMsg::Dap(_) | AppMsg::DebugReady { .. } | AppMsg::DebugBuilt { .. })).await;
        }
    }

    /// Like `dap_until`, and the Console thread's batches are handled too.
    pub(crate) async fn console_until(app: &mut App, done: impl Fn(&App) -> bool) {
        while !done(app) {
            handle_next(app, |m| {
                matches!(m, AppMsg::Dap(_) | AppMsg::DebugReady { .. } | AppMsg::DebugBuilt { .. } | AppMsg::DebugOutput { .. })
            })
            .await;
        }
    }

    pub(crate) fn write_run_toml(app: &App, text: &str) {
        let dir = app.workspace.root.join(".jetl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.toml"), text).unwrap();
    }

    /// The session is over and reported: `Phase(Ended)` makes `active()` false one message before `Ended` removes the `Live`.
    fn gone(app: &App) -> bool {
        app.debug.session.is_none()
    }

    fn shift_f9(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::SHIFT));
    }

    fn ctrl_f2(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::CONTROL));
    }

    fn texts(app: &App) -> Vec<String> {
        app.notifications.iter().map(|n| n.text.clone()).collect()
    }

    fn last(app: &App) -> (Level, String) {
        let n = app.notifications.last().unwrap();
        (n.level, n.text.clone())
    }

    fn inject(app: &mut App, body: DapBody) {
        let (generation, session) = (app.workspace.generation, app.debug.session.as_ref().unwrap().id);
        app.dap_msg(DapMsg { generation, session, body });
    }

    /// Presses Debug and handles the start; the adapter the session talks to.
    async fn start(app: &mut App, fakes: &mut UnboundedReceiver<FakeAdapter>) -> FakeAdapter {
        shift_f9(app);
        dap_until(app, |a| a.debug.session.is_some()).await;
        fakes.try_recv().expect("an adapter was connected")
    }

    /// A fresh app with `DEMO` and a live session, past its `launch`.
    async fn live(name: &str, order: Order) -> (App, PathBuf, UnboundedReceiver<FakeAdapter>, FakeAdapter) {
        let (mut app, dir) = test_app(name);
        let mut fakes = with_fake_adapters(&mut app, caps(), order);
        write_run_toml(&app, DEMO);
        let fake = start(&mut app, &mut fakes).await;
        dap_until(&mut app, |a| a.debug.phase().is_some_and(|p| p != Phase::Initializing)).await;
        (app, dir, fakes, fake)
    }

    /// The adapter answers a stop: `terminate` ends with `terminated`, `disconnect` with its response.
    async fn cooperate(fake: &mut FakeAdapter) {
        fake.recv("terminate").await;
        fake.event("terminated", Value::Null);
        let disconnect = fake.recv("disconnect").await;
        fake.respond(&disconnect, Value::Null);
    }

    /// `debug.start` with the files as they are; the toast it ends with, no adapter connected.
    async fn refused(name: &str, toml: &str, setup: impl FnOnce(&mut App)) -> (Level, String) {
        let (mut app, dir) = test_app(name);
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, toml);
        setup(&mut app);
        shift_f9(&mut app);
        dap_until(&mut app, |a| !a.debug.starting()).await;
        assert!(fakes.try_recv().is_err(), "{name}: an adapter was connected");
        assert!(!app.debug.active(), "{name}: a session is live");
        let out = last(&app);
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[tokio::test]
    async fn start_runs_in_every_order() {
        for (i, order) in fake::ORDERS.into_iter().enumerate() {
            let (mut app, dir) = test_app(&format!("debug-start-{i}"));
            let mut fakes = with_fake_adapters(&mut app, caps(), order);
            write_run_toml(&app, DEMO);
            let root = app.workspace.root.clone();
            let mut fake = start(&mut app, &mut fakes).await;
            let launch = fake.recv("launch").await;
            assert_eq!(launch["arguments"]["program"], json!(format!("{}/demo", root.display())), "{order:?}");
            assert_eq!(launch["arguments"]["cwd"], json!(root.to_string_lossy()), "{order:?}");
            dap_until(&mut app, |a| a.debug.phase() == Some(Phase::Running)).await;
            assert!(texts(&app).contains(&"Debugging demo with fake".to_string()), "{order:?}");
            assert!(app.debug.active() && !app.debug.suspended(), "{order:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn selected_configuration_wins() {
        let (mut app, dir) = test_app("debug-selected");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let block = |n: &str| {
            format!(
                "[[configs]]\nname = \"{n}\"\ncommand = [\"x\"]\n\n[debug.{n}]\nadapter = \"fake\"\nlaunch = {{ program = \"/p/{n}\" }}\n\n"
            )
        };
        write_run_toml(&app, &(block("a") + &block("b")));
        app.run.selected = Some("b".into());
        let mut fake = start(&mut app, &mut fakes).await;
        assert_eq!(fake.recv("launch").await["arguments"]["program"], "/p/b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn first_configuration_with_a_block_when_none_selected() {
        let (mut app, dir) = test_app("debug-first-block");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let text = "[[configs]]\nname = \"a\"\ncommand = [\"x\"]\n\n[[configs]]\nname = \"b\"\ncommand = [\"x\"]\n\n[debug.b]\nadapter = \"fake\"\nlaunch = { program = \"/p/b\" }\n";
        write_run_toml(&app, text);
        let mut fake = start(&mut app, &mut fakes).await;
        assert_eq!(fake.recv("launch").await["arguments"]["program"], "/p/b");
        assert!(texts(&app).contains(&"Debugging b with fake".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn each_refusal_has_its_toast() {
        let no_block = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n";
        let with = |debug: &str| format!("{no_block}\n[debug.demo]\n{debug}\n");
        let warn = |s: &str| (Level::Warn, s.to_string());
        let missing = refused("debug-refuse-gone", DEMO, |a| a.run.selected = Some("gone".into())).await;
        assert_eq!(missing, warn("Run configuration 'gone' no longer exists"));
        let no_table = refused("debug-refuse-table", no_block, |a| a.run.selected = Some("demo".into())).await;
        assert_eq!(no_table, warn("Configuration 'demo' has no [debug.\"demo\"] table in .jetl/run.toml"));
        let none = refused("debug-refuse-none", no_block, |_| {}).await;
        assert_eq!(none, warn("No debug configuration: add [debug.<name>] to .jetl/run.toml"));
        let ghost = refused("debug-refuse-ghost", &with("adapter = \"ghost\""), |_| {}).await;
        assert_eq!(ghost, warn("Debug adapter 'ghost' is not configured, or its command is empty: see [debug.adapters] in config.toml"));
        let off = |a: &mut App| {
            let o = AdapterOverride { command: Some(vec!["nonexistent-dap-xyz".into()]), languages: Some(vec!["c".into()]), launch: None };
            a.config.debug.adapters.insert("nope".into(), o);
        };
        let nope = refused("debug-refuse-nope", &with("adapter = \"nope\""), off).await;
        assert_eq!(nope, warn("Debug adapter 'nope': nonexistent-dap-xyz not found"));
        let py = |a: &mut App| {
            let o = AdapterOverride { command: Some(vec!["nonexistent-dap-xyz".into()]), ..Default::default() };
            a.config.debug.adapters.insert("debugpy".into(), o);
        };
        let python = refused("debug-refuse-python", &with("language = \"python\""), py).await;
        assert_eq!(python.0, Level::Warn);
        assert!(python.1.starts_with("No debug adapter found for python. Install one of: debugpy ("), "{}", python.1);
        let needs_build = warn("Cannot debug 'demo': ${artifact} needs a build step: add build = [...] to its debug table");
        for (i, launch) in ["launch = { program = \"${artifact}\" }", "launch = { program = \"x\", args = [\"-a\", \"${artifact}\"] }"]
            .into_iter()
            .enumerate()
        {
            let text = with(&format!("adapter = \"fake\"\n{launch}"));
            assert_eq!(refused(&format!("debug-refuse-artifact-{i}"), &text, |_| {}).await, needs_build, "{launch}");
        }
        let lang = refused("debug-refuse-language", &with(""), |_| {}).await;
        assert_eq!(lang.0, Level::Warn);
        assert!(lang.1.starts_with("No language for this debug configuration"), "{}", lang.1);
    }

    #[tokio::test]
    async fn a_bad_debug_block_is_shown() {
        let text = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nlaunch = \"x\"\n";
        let (level, toast) = refused("debug-bad-block", text, |_| {}).await;
        assert_eq!(level, Level::Warn);
        assert!(toast.contains("launch must be a table"), "{toast}");
    }

    #[tokio::test]
    async fn a_cwd_with_a_newline_is_refused() {
        let exact = "Cannot debug 'demo': line break in its working directory";
        let in_block = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nadapter = \"fake\"\nlaunch = { program = \"x\", cwd = \"/tmp/a\\nshell touch x\" }\n";
        assert_eq!(refused("debug-newline-block", in_block, |_| {}).await, (Level::Warn, exact.to_string()));
        let in_config = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\ncwd = \"sub\\nshell touch x\"\n\n[debug.demo]\nadapter = \"fake\"\nlaunch = { program = \"x\" }\n";
        assert_eq!(refused("debug-newline-config", in_config, |_| {}).await, (Level::Warn, exact.to_string()));
    }

    #[tokio::test]
    async fn a_program_with_a_line_break_is_refused() {
        let exact = "Cannot debug 'demo': line break in its program";
        let toml = |program: &str| {
            format!(
                "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nadapter = \"fake\"\nlaunch = {{ program = \"{program}\" }}\n"
            )
        };
        assert_eq!(refused("debug-program-lf", &toml("/tmp/a\\nshell touch x"), |_| {}).await, (Level::Warn, exact.to_string()));
        assert_eq!(refused("debug-program-cr", &toml("/tmp/a\\rshell touch x"), |_| {}).await, (Level::Warn, exact.to_string()));
        // The name of the open file is the only untrusted input of a block in the user's own config.
        let open = |app: &mut App| {
            load(app, "a\nshell touch x.c", "int main;\n");
        };
        assert_eq!(refused("debug-program-file", &toml("${file}"), open).await, (Level::Warn, exact.to_string()));
    }

    #[tokio::test]
    async fn stop_during_the_choice_ends_the_start() {
        let (mut app, dir, mut fakes, _) = choosing("build-stop-choosing").await;
        // Another dialog overwrote the offer, so Stop is the way out.
        app.dialog = None;
        app.perform(Action::Debug(DebugAction::Stop));
        assert_eq!(last(&app), (Level::Info, "Debug start cancelled".to_string()));
        assert_eq!(title(&app), " Debug: demo · not started ");
        assert!(!app.debug.starting() && app.debug.choice.is_none());
        assert!(fakes.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_cwd_with_spaces_is_not_refused() {
        let (mut app, dir) = test_app("debug-cwd-spaces");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let text = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nadapter = \"fake\"\nlaunch = { program = \"x\", cwd = \"/tmp/a b\" }\n";
        write_run_toml(&app, text);
        let mut fake = start(&mut app, &mut fakes).await;
        assert_eq!(fake.recv("launch").await["arguments"]["cwd"], "/tmp/a b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn language_and_file_come_from_the_active_buffer() {
        let (mut app, dir) = test_app("debug-language");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let text = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nlaunch = { program = \"${file}\" }\n";
        write_run_toml(&app, text);
        let path = load(&mut app, "a.rs", "fn main() {}\n");
        let mut fake = start(&mut app, &mut fakes).await;
        assert!(texts(&app).contains(&"Debugging demo with fake".to_string()), "{:?}", texts(&app));
        assert_eq!(fake.recv("launch").await["arguments"]["program"], json!(path.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gdb_gets_the_whole_environment() {
        let (mut app, dir) = test_app("debug-gdb-env");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        // No gdb runs: its command is `sh`, and the connector is the fake.
        app.config.debug.adapters.insert("gdb".into(), AdapterOverride { command: Some(vec!["sh".into()]), ..Default::default() });
        write_run_toml(&app, &DEMO.replace("\"fake\"", "\"gdb\""));
        let mut fake = start(&mut app, &mut fakes).await;
        let launch = fake.recv("launch").await;
        assert!(launch["arguments"]["env"]["PATH"].is_string(), "{launch}");
        drop(fake);
        write_run_toml(&app, DEMO);
        let mut fake = start(&mut app, &mut fakes).await;
        let launch = fake.recv("launch").await;
        assert!(launch["arguments"].get("env").is_none(), "{launch}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_adapter_that_cannot_start() {
        let (mut app, dir) = test_app("debug-cannot-start");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        app.debug.connector = Connector::Fake(Box::new(|| Err(std::io::Error::other("boom"))));
        write_run_toml(&app, DEMO);
        shift_f9(&mut app);
        dap_until(&mut app, |a| !a.debug.starting()).await;
        assert_eq!(last(&app), (Level::Error, "Debug adapter 'fake' could not start: boom".to_string()));
        assert!(!app.debug.active() && !app.debug.starting());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_real_process_that_exits_ends_the_session() {
        let (mut app, dir) = test_app("debug-real-exit");
        let o = AdapterOverride {
            command: Some(vec!["sh".into(), "-c".into(), "exit 3".into()]),
            languages: Some(vec!["c".into()]),
            launch: None,
        };
        app.config.debug.adapters.insert("quits".into(), o);
        write_run_toml(&app, &DEMO.replace("\"fake\"", "\"quits\""));
        shift_f9(&mut app);
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugReady { .. })).await;
        assert!(app.debug.active());
        dap_until(&mut app, gone).await;
        assert_eq!(last(&app), (Level::Warn, "Debug session ended: adapter exited".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn old_session_and_generation_are_dropped() {
        let (mut app, dir, _fakes, _fake) = live("debug-old", Order::LaunchDeferred).await;
        let (generation, id) = (app.workspace.generation, app.debug.session.as_ref().unwrap().id);
        for body in [DapBody::Error("old".into()), DapBody::Ended { reason: "old".into() }] {
            app.dap_msg(DapMsg { generation: generation + 1, session: id, body: body.clone() });
            assert!(!texts(&app).iter().any(|t| t.contains("old")), "generation: {:?}", texts(&app));
            app.dap_msg(DapMsg { generation, session: SessionId(id.0 + 1), body });
            assert!(!texts(&app).iter().any(|t| t.contains("old")), "session: {:?}", texts(&app));
            assert!(app.debug.active());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ready_of_another_generation_is_dropped() {
        let (mut app, dir) = test_app("debug-ready-generation");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, DEMO);
        shift_f9(&mut app);
        app.workspace.generation += 1;
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugReady { .. })).await;
        assert!(fakes.try_recv().is_err());
        assert!(!app.debug.active());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_newer_start_wins() {
        let (mut app, dir) = test_app("debug-newer-start");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, DEMO);
        shift_f9(&mut app);
        shift_f9(&mut app);
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugReady { .. })).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugReady { .. })).await;
        assert!(fakes.try_recv().is_ok());
        assert!(fakes.try_recv().is_err(), "a second adapter was connected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stop_ends_with_no_adapter_reply() {
        let (mut app, dir, _fakes, mut fake) = live("debug-stop", Order::LaunchEarly).await;
        app.perform(Action::Debug(crate::actions::DebugAction::Stop));
        fake.recv("terminate").await;
        fake.recv("disconnect").await;
        dap_until(&mut app, gone).await;
        assert_eq!(last(&app), (Level::Info, "Debug session stopped".to_string()));
        assert!(app.notifications.iter().all(|n| n.level != Level::Error));
        app.perform(Action::Debug(crate::actions::DebugAction::Stop));
        assert_eq!(last(&app), (Level::Info, "No debug session is running".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_second_stop_skips_a_stage() {
        let (mut app, dir, _fakes, mut fake) = live("debug-stop-twice", Order::LaunchEarly).await;
        app.perform(Action::Debug(crate::actions::DebugAction::Stop));
        fake.recv("terminate").await;
        let began = Instant::now();
        app.perform(Action::Debug(crate::actions::DebugAction::Stop));
        fake.recv("disconnect").await;
        assert!(began.elapsed() < STOP_STAGE / 2, "disconnect took {:?}", began.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stop_during_a_start_cancels_it() {
        let (mut app, dir) = test_app("debug-stop-start");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, DEMO);
        shift_f9(&mut app);
        app.perform(Action::Debug(crate::actions::DebugAction::Stop));
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugReady { .. })).await;
        assert!(fakes.try_recv().is_err());
        assert!(!app.debug.active() && !app.debug.starting());
        assert_eq!(last(&app), (Level::Info, "Debug start cancelled".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exit_code_toasts() {
        for (code, level, text) in
            [(3, Level::Warn, "Debug session ended, exit code 3"), (0, Level::Info, "Debug session ended, exit code 0")]
        {
            let (mut app, dir, _fakes, mut fake) = live(&format!("debug-exit-{code}"), Order::LaunchDeferred).await;
            fake.event("stopped", json!({"reason": "breakpoint", "threadId": 1}));
            dap_until(&mut app, |a| a.debug_panel.title(a.debug.phase()).contains("paused (breakpoint)")).await;
            fake.event("exited", json!({"exitCode": code}));
            fake.event("terminated", Value::Null);
            let disconnect = fake.recv("disconnect").await;
            fake.respond(&disconnect, Value::Null);
            dap_until(&mut app, gone).await;
            assert_eq!(last(&app), (level, text.to_string()));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn error_toast() {
        let (mut app, dir, _fakes, _fake) = live("debug-error", Order::LaunchDeferred).await;
        inject(&mut app, DapBody::Error("no program".into()));
        assert_eq!(last(&app), (Level::Error, "no program".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ctrl_f2_stops_the_session_then_the_task() {
        let (mut app, dir, _fakes, mut fake) = live("debug-ctrl-f2", Order::LaunchDeferred).await;
        app.run.fake_running("t");
        ctrl_f2(&mut app);
        assert!(app.debug.session.as_ref().unwrap().stop_requested);
        assert!(!app.run.stop_requested());
        cooperate(&mut fake).await;
        dap_until(&mut app, gone).await;
        ctrl_f2(&mut app);
        assert!(app.run.stop_requested());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn contexts_hold_debug_active_only_while_it_applies() {
        let (mut app, dir) = test_app("debug-contexts");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, DEMO);
        assert!(!app.contexts().contains(&"debug.active"));
        shift_f9(&mut app);
        assert!(app.contexts().contains(&"debug.active"), "during a start");
        dap_until(&mut app, |a| a.debug.phase().is_some_and(|p| p != Phase::Initializing)).await;
        let mut fake = fakes.try_recv().unwrap();
        let v = app.contexts();
        assert_eq!(v.last(), Some(&"global"));
        assert_eq!(v[v.len() - 2], "debug.active", "{v:?}");
        app.dialog = Some(crate::ui::dialog::Dialog::info("x", "y"));
        assert!(!app.contexts().contains(&"debug.active"), "under a dialog");
        app.dialog = None;
        app.perform(Action::Debug(crate::actions::DebugAction::Stop));
        cooperate(&mut fake).await;
        dap_until(&mut app, gone).await;
        assert!(!app.contexts().contains(&"debug.active"), "after Ended");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stop_shows_its_key() {
        let (app, dir) = test_app("debug-keys");
        for (a, key) in [(crate::actions::DebugAction::Stop, "Ctrl+F2"), (crate::actions::DebugAction::Start, "Shift+F9")] {
            let action = Action::Debug(a);
            assert_eq!(app.keymap.shortcut_for(&action, action.contexts()).as_deref(), Some(key));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_root_switch_stops_gracefully_and_quietly() {
        let (mut app, dir, _fakes, mut fake) = live("debug-root-switch", Order::LaunchDeferred).await;
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(!app.debug.active());
        cooperate(&mut fake).await;
        fake.closed().await;
        while let Ok(m) = app.bus_rx.try_recv() {
            app.handle_msg(m);
        }
        assert!(!texts(&app).iter().any(|t| t.starts_with("Debug session")), "{:?}", texts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_root_switch_during_a_start_connects_nothing() {
        let (mut app, dir) = test_app("debug-root-switch-start");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, DEMO);
        shift_f9(&mut app);
        assert!(app.debug.starting());
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(!app.debug.starting());
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugReady { result, .. } if result.is_ok())).await;
        assert!(fakes.try_recv().is_err(), "an adapter was connected for the old root");
        assert!(!app.debug.active() && !app.debug.starting());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_second_start_during_a_stop_waits_and_replaces() {
        let (mut app, dir, mut fakes, mut fake1) = live("debug-start-in-stop", Order::LaunchDeferred).await;
        app.perform(Action::Debug(crate::actions::DebugAction::Stop));
        fake1.recv("terminate").await;
        shift_f9(&mut app);
        tokio::time::sleep(Duration::from_millis(200)).await;
        while let Ok(m) = app.bus_rx.try_recv() {
            assert!(!matches!(m, AppMsg::DebugReady { .. }), "the start did not wait");
        }
        fake1.event("terminated", Value::Null);
        let disconnect = fake1.recv("disconnect").await;
        fake1.respond(&disconnect, Value::Null);
        dap_until(&mut app, |a| a.debug.active()).await;
        let mut fake2 = fakes.try_recv().expect("the second adapter");
        fake2.recv("launch").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_second_start_waits_and_replaces() {
        let (mut app, dir, mut fakes, mut fake1) = live("debug-replace", Order::LaunchDeferred).await;
        shift_f9(&mut app);
        fake1.recv("terminate").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        while let Ok(m) = app.bus_rx.try_recv() {
            assert!(!matches!(m, AppMsg::DebugReady { .. }), "the start did not wait");
        }
        fake1.event("terminated", Value::Null);
        let disconnect = fake1.recv("disconnect").await;
        fake1.respond(&disconnect, Value::Null);
        dap_until(&mut app, |a| a.debug.active()).await;
        let mut fake2 = fakes.try_recv().expect("the second adapter");
        fake2.recv("launch").await;
        assert!(!texts(&app).contains(&"Debug session stopped".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn app_shutdown_ends_the_session() {
        let (mut app, dir, _fakes, mut fake) = live("debug-shutdown", Order::LaunchDeferred).await;
        let task = tokio::spawn(async move {
            cooperate(&mut fake).await;
            fake
        });
        app.shutdown().await;
        tokio::time::timeout(Duration::from_secs(1), task).await.expect("in time").expect("cooperated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- breakpoints (yman 413) ----

    pub(crate) fn setup(name: &str, order: Order) -> (App, PathBuf, UnboundedReceiver<FakeAdapter>) {
        let (mut app, dir) = test_app(name);
        let fakes = with_fake_adapters(&mut app, caps(), order);
        write_run_toml(&app, DEMO);
        (app, dir, fakes)
    }

    /// A live session in `Running`, its snapshot sent; every setBreakpoints so far is taken off the fake and returned.
    pub(crate) async fn running(app: &mut App, fakes: &mut UnboundedReceiver<FakeAdapter>) -> (FakeAdapter, Vec<Value>) {
        app.perform(Action::Debug(DebugAction::Start));
        dap_until(app, |a| a.debug.active()).await;
        let mut fake = fakes.try_recv().expect("connected");
        let mut sets = Vec::new();
        loop {
            let m = fake.next().await;
            if m["command"] == "setBreakpoints" {
                sets.push(m);
            } else if m["command"] == "configurationDone" {
                break;
            }
        }
        dap_until(app, |a| a.debug.phase() == Some(Phase::Running)).await;
        (fake, sets)
    }

    fn lines(m: &Value) -> Vec<i64> {
        m["arguments"]["breakpoints"].as_array().expect("breakpoints").iter().map(|b| b["line"].as_i64().expect("line")).collect()
    }

    fn source(m: &Value) -> String {
        m["arguments"]["source"]["path"].as_str().expect("path").to_string()
    }

    fn toggle(app: &mut App, path: &Path, line: usize) {
        let i = app.editor.index_of(path).unwrap();
        app.editor.buffers[i].toggle_breakpoint_at(line);
    }

    fn state(app: &App, path: &Path, line: usize) -> BreakpointMark {
        let i = app.editor.index_of(path).unwrap();
        app.editor.buffers[i].breakpoints[&line].clone()
    }

    fn keys(app: &App, path: &Path) -> Vec<usize> {
        let i = app.editor.index_of(path).unwrap();
        app.editor.buffers[i].breakpoints.keys().copied().collect()
    }

    /// No request was sent since the last one taken: a mark in a new file `name` makes the very next message that file's.
    async fn assert_quiet(app: &mut App, fake: &mut FakeAdapter, name: &str) {
        let p = load(app, name, "x\n");
        toggle(app, &p, 0);
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m)), (Some("setBreakpoints"), p.to_string_lossy().into_owned()));
    }

    fn answer(fake: &FakeAdapter, set: &Value, bps: Value) {
        fake.respond(set, json!({ "breakpoints": bps }));
    }

    pub(crate) fn six(app: &mut App) -> PathBuf {
        load(app, "a.log", "l0\nl1\nl2\nl3\nl4\nl5\n")
    }

    fn answered(app: &mut App, path: &Path, requested: Vec<usize>, result: Vec<crate::dap::protocol::Breakpoint>) {
        inject(app, DapBody::Breakpoints { source: path.to_path_buf(), requested, result });
    }

    fn bp(v: Value) -> crate::dap::protocol::Breakpoint {
        serde_json::from_value(v).unwrap()
    }

    fn changed(app: &mut App, reason: &str, v: Value) {
        inject(app, DapBody::BreakpointChanged(BreakpointEvent { reason: reason.into(), breakpoint: bp(v) }));
    }

    #[tokio::test]
    async fn the_snapshot_goes_before_configuration_in_every_order() {
        for (n, order) in fake::ORDERS.into_iter().enumerate() {
            let (mut app, dir, mut fakes) = setup(&format!("bp-snapshot-{n}"), order);
            let root = app.workspace.root.clone();
            let z = root.join("z.log");
            app.editor.restore_closed_breakpoints(&[(z.clone(), vec![0, 2])]);
            let a = six(&mut app);
            toggle(&mut app, &a, 1);
            toggle(&mut app, &a, 3);
            let b = load(&mut app, "b.log", "x0\nx1\n");
            toggle(&mut app, &b, 0);
            let i = app.editor.index_of(&b).unwrap();
            app.editor.buffers[i].insert("x", TxKind::Typing);
            let (_fake, sets) = running(&mut app, &mut fakes).await;
            let got: Vec<(String, Vec<i64>, bool)> =
                sets.iter().map(|m| (source(m), lines(m), m["arguments"]["sourceModified"].as_bool().unwrap())).collect();
            let s = |p: &Path| p.to_string_lossy().into_owned();
            assert_eq!(got, vec![(s(&a), vec![2, 4], false), (s(&b), vec![1], true), (s(&z), vec![1, 3], false)], "{order:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn no_marks_send_no_breakpoints() {
        let (mut app, dir, mut fakes) = setup("bp-none", Order::LaunchEarly);
        let (mut fake, sets) = running(&mut app, &mut fakes).await;
        assert!(sets.is_empty(), "{sets:?}");
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_tick_after_the_start_sends_nothing() {
        let (mut app, dir, mut fakes) = setup("bp-tick", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, sets) = running(&mut app, &mut fakes).await;
        assert_eq!(sets.len(), 1);
        for _ in 0..3 {
            app.on_tick();
        }
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_toggle_sends_the_whole_list_once() {
        let (mut app, dir, mut fakes) = setup("bp-toggle", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        toggle(&mut app, &a, 3);
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), a.to_string_lossy().into_owned(), vec![2, 4]));
        assert_eq!(m["arguments"]["sourceModified"], json!(false));
        app.on_tick();
        app.on_tick();
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_moving_edit_sends_new_lines_marked_modified() {
        let (mut app, dir, mut fakes) = setup("bp-edit", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.focus.set(PanelId::Editor);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((source(&m), lines(&m)), (a.to_string_lossy().into_owned(), vec![3]));
        assert_eq!(m["arguments"]["sourceModified"], json!(true));
        // A save (dirty goes false) and then typing on a line (dirty again): the lines are the same, so nothing goes.
        let i = app.editor.index_of(&a).unwrap();
        app.editor.buffers[i].mark_saved();
        app.on_tick();
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        app.editor.buffers[i].insert("x", TxKind::Typing);
        app.on_tick();
        app.on_tick();
        assert_quiet(&mut app, &mut fake, "q2.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn closing_a_tab_with_marks_sends_nothing() {
        let (mut app, dir, mut fakes) = setup("bp-close-tab", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        let i = app.editor.index_of(&a).unwrap();
        app.editor.force_close(i);
        app.on_tick();
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        assert_eq!(app.editor.closed_breakpoints(), vec![(a, vec![1])]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_reopened_file_is_asked_again_once() {
        let (mut app, dir, mut fakes) = setup("bp-reopen", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        let i = app.editor.index_of(&a).unwrap();
        app.editor.force_close(i);
        app.on_tick();
        load(&mut app, "a.log", "l0\nl1\nl2\nl3\nl4\nl5\n");
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), a.to_string_lossy().into_owned(), vec![2]));
        answer(&fake, &m, json!([{"id": 5, "verified": false, "reason": "pending"}]));
        dap_until(&mut app, |a2| state(a2, &a, 1).state == BpState::Pending).await;
        assert_eq!(state(&app, &a, 1).id, Some(5));
        app.on_tick();
        app.on_tick();
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_file_closed_at_the_start_is_asked_on_opening() {
        let (mut app, dir, mut fakes) = setup("bp-open-closed", Order::LaunchEarly);
        let z = app.workspace.root.join("z.log");
        app.editor.restore_closed_breakpoints(&[(z.clone(), vec![1])]);
        let (mut fake, sets) = running(&mut app, &mut fakes).await;
        assert_eq!(sets.len(), 1);
        load(&mut app, "z.log", "a\nb\n");
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), z.to_string_lossy().into_owned(), vec![2]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_file_reopened_shorter_sends_where_its_marks_landed() {
        let (mut app, dir, mut fakes) = setup("bp-shorter", Order::LaunchEarly);
        let z = app.workspace.root.join("z.log");
        app.editor.restore_closed_breakpoints(&[(z.clone(), vec![1, 9])]);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        load(&mut app, "z.log", "a\nb\nc");
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((source(&m), lines(&m)), (z.to_string_lossy().into_owned(), vec![2, 3]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_emptied_file_gets_an_empty_list_once() {
        let (mut app, dir, mut fakes) = setup("bp-emptied", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        toggle(&mut app, &a, 1);
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), a.to_string_lossy().into_owned(), vec![]));
        assert!(!app.debug.session.as_ref().unwrap().sent.contains_key(&a));
        app.on_tick();
        app.on_tick();
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        toggle(&mut app, &a, 2);
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((source(&m), lines(&m)), (a.to_string_lossy().into_owned(), vec![3]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_closed_file_dropped_gets_an_empty_list() {
        let (mut app, dir, mut fakes) = setup("bp-dropped", Order::LaunchEarly);
        let z = app.workspace.root.join("z.log");
        app.editor.restore_closed_breakpoints(&[(z.clone(), vec![1])]);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.editor.close_under(&z);
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), z.to_string_lossy().into_owned(), vec![]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_closed_tab_whose_marks_went_gets_an_empty_list() {
        let (mut app, dir, mut fakes) = setup("bp-tab-emptied", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        toggle(&mut app, &a, 1);
        let i = app.editor.index_of(&a).unwrap();
        app.editor.force_close(i);
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), a.to_string_lossy().into_owned(), vec![]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_rename_clears_the_old_path_and_sends_the_new() {
        let (mut app, dir, mut fakes) = setup("bp-rename", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        let m_path = app.workspace.root.join("m.log");
        let theme = app.theme.clone();
        app.editor.retarget(&a, &m_path, &theme);
        app.on_tick();
        let first = fake.next().await;
        assert_eq!((source(&first), lines(&first)), (a.to_string_lossy().into_owned(), vec![]));
        let second = fake.next().await;
        assert_eq!((source(&second), lines(&second)), (m_path.to_string_lossy().into_owned(), vec![2]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_open_buffer_with_no_marks_outranks_parked_lines_it_was_sent_for() {
        let (mut app, dir, mut fakes) = setup("bp-outrank", Order::LaunchEarly);
        let z = app.workspace.root.join("z.log");
        app.editor.restore_closed_breakpoints(&[(z.clone(), vec![1])]);
        let u = load(&mut app, "u.log", "one\ntwo\n");
        let (mut fake, sets) = running(&mut app, &mut fakes).await;
        assert_eq!(sets.len(), 1);
        assert_eq!((source(&sets[0]), lines(&sets[0])), (z.to_string_lossy().into_owned(), vec![2]));
        let i = app.editor.index_of(&u).unwrap();
        app.editor.buffers[i].path = Some(z.clone());
        app.on_tick();
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), z.to_string_lossy().into_owned(), vec![]));
        app.on_tick();
        app.on_tick();
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_toggle_before_initialized_replaces_the_snapshot() {
        let (mut app, dir, mut fakes) = setup("bp-before-init", Order::LaunchDeferred);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        app.perform(Action::Debug(DebugAction::Start));
        dap_until(&mut app, |x| x.debug.active()).await;
        toggle(&mut app, &a, 3);
        app.on_tick();
        let mut fake = fakes.try_recv().expect("connected");
        let m = fake.recv("setBreakpoints").await;
        assert_eq!(lines(&m), vec![2, 4]);
        assert_eq!(fake.next().await["command"], "configurationDone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn results_map_onto_the_requested_lines() {
        let (mut app, dir, mut fakes) = setup("bp-map", Order::LaunchEarly);
        let a = six(&mut app);
        for l in 1..=4 {
            toggle(&mut app, &a, l);
        }
        let (fake, sets) = running(&mut app, &mut fakes).await;
        answer(
            &fake,
            &sets[0],
            json!([
                {"id": 1, "verified": true, "line": 2},
                {"id": 2, "verified": false, "reason": "pending"},
                {"verified": false, "reason": "failed", "message": "no code"},
                {"id": 4, "verified": true, "line": 7}
            ]),
        );
        dap_until(&mut app, |x| state(x, &a, 4).state == BpState::Verified).await;
        let m =
            |state, id, adapter_line, message: Option<&str>| BreakpointMark { state, id, adapter_line, message: message.map(Into::into) };
        assert_eq!(state(&app, &a, 1), m(BpState::Verified, Some(1), None, None));
        assert_eq!(state(&app, &a, 2), m(BpState::Pending, Some(2), None, None));
        assert_eq!(state(&app, &a, 3), m(BpState::Failed, None, None, Some("no code")));
        assert_eq!(state(&app, &a, 4), m(BpState::Verified, Some(4), Some(6), None));
        assert_eq!(keys(&app, &a), vec![1, 2, 3, 4]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_wins_over_verified_and_line_zero_says_nothing() {
        let (mut app, dir, _fakes, _fake) = live("bp-failed-wins", Order::LaunchEarly).await;
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        toggle(&mut app, &a, 2);
        answered(
            &mut app,
            &a,
            vec![1, 2],
            vec![bp(json!({"verified": true, "reason": "failed"})), bp(json!({"verified": true, "line": 0}))],
        );
        assert_eq!(state(&app, &a, 1).state, BpState::Failed);
        assert_eq!((state(&app, &a, 2).state, state(&app, &a, 2).adapter_line), (BpState::Verified, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_answer_replaces_the_mark_whole() {
        let (mut app, dir, _fakes, _fake) = live("bp-whole", Order::LaunchEarly).await;
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let i = app.editor.index_of(&a).unwrap();
        app.editor.buffers[i]
            .breakpoints
            .insert(1, BreakpointMark { state: BpState::Failed, id: Some(9), adapter_line: Some(7), message: Some("old".into()) });
        answered(&mut app, &a, vec![1], vec![bp(json!({"verified": true}))]);
        assert_eq!(state(&app, &a, 1), BreakpointMark { state: BpState::Verified, ..Default::default() });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_short_answer_leaves_the_rest() {
        let (mut app, dir, _fakes, _fake) = live("bp-short", Order::LaunchEarly).await;
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        toggle(&mut app, &a, 2);
        answered(&mut app, &a, vec![1, 2], vec![bp(json!({"id": 1, "verified": true}))]);
        assert_eq!(state(&app, &a, 1).state, BpState::Verified);
        assert_eq!(state(&app, &a, 2), BreakpointMark::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_answer_touches_neither_a_closed_file_nor_a_gone_mark() {
        let (mut app, dir, _fakes, _fake) = live("bp-gone", Order::LaunchEarly).await;
        let root = app.workspace.root.clone();
        answered(&mut app, &root.join("nope.log"), vec![1], vec![bp(json!({"verified": true}))]);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        toggle(&mut app, &a, 2);
        // Mark 1 went after the request for [1, 2]: the second result is still mark 2's.
        toggle(&mut app, &a, 1);
        answered(
            &mut app,
            &a,
            vec![1, 2],
            vec![bp(json!({"id": 1, "verified": true})), bp(json!({"id": 2, "verified": false, "reason": "pending"}))],
        );
        assert_eq!(keys(&app, &a), vec![2]);
        let mark = state(&app, &a, 2);
        assert_eq!((mark.state, mark.id), (BpState::Pending, Some(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_request_leaves_the_marks_and_shows_its_toast() {
        let (mut app, dir, mut fakes) = setup("bp-request-fails", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (fake, sets) = running(&mut app, &mut fakes).await;
        fake.respond_error(&sets[0], "boom");
        dap_until(&mut app, |x| texts(x).contains(&"setBreakpoints: boom".to_string())).await;
        assert_eq!(app.notifications.iter().find(|n| n.text == "setBreakpoints: boom").unwrap().level, Level::Error);
        assert_eq!(state(&app, &a, 1), BreakpointMark::default());
        assert!(app.debug.active());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gdb_pending_then_three_changes() {
        let (mut app, dir, mut fakes) = setup("bp-gdb", Order::LaunchDeferred);
        let a = six(&mut app);
        toggle(&mut app, &a, 2);
        let (mut fake, sets) = running(&mut app, &mut fakes).await;
        answer(&fake, &sets[0], json!([{"id": 1, "verified": false, "reason": "pending"}]));
        dap_until(&mut app, |x| state(x, &a, 2).state == BpState::Pending).await;
        assert_eq!(state(&app, &a, 2).adapter_line, None);
        for _ in 0..3 {
            fake.event("breakpoint", json!({"reason": "changed", "breakpoint": {"id": 1, "verified": true, "line": 3}}));
        }
        dap_until(&mut app, |x| state(x, &a, 2).state == BpState::Verified).await;
        assert_eq!(state(&app, &a, 2).adapter_line, None);
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_changed_event_finds_the_mark_by_id_in_any_buffer() {
        let (mut app, dir, _fakes, _fake) = live("bp-by-id", Order::LaunchEarly).await;
        let a = six(&mut app);
        let b = load(&mut app, "b.log", "b0\nb1\nb2\nb3\n");
        toggle(&mut app, &a, 1);
        toggle(&mut app, &b, 2);
        answered(&mut app, &a, vec![1], vec![bp(json!({"id": 1, "verified": false, "reason": "pending"}))]);
        answered(&mut app, &b, vec![2], vec![bp(json!({"id": 2, "verified": false, "reason": "pending"}))]);
        let a_before = state(&app, &a, 1);
        changed(&mut app, "changed", json!({"id": 2, "verified": true, "line": 9}));
        let m = state(&app, &b, 2);
        assert_eq!((m.state, m.adapter_line), (BpState::Verified, Some(8)));
        assert_eq!(keys(&app, &b), vec![2]);
        assert_eq!(state(&app, &a, 1), a_before);
        changed(&mut app, "changed", json!({"id": 2, "verified": true}));
        assert_eq!(state(&app, &b, 2).adapter_line, Some(8));
        changed(&mut app, "changed", json!({"id": 2, "verified": false, "reason": "pending"}));
        assert_eq!(state(&app, &b, 2).state, BpState::Pending);
        let (a_now, b_now) = (state(&app, &a, 1), state(&app, &b, 2));
        changed(&mut app, "changed", json!({"id": 99, "verified": true, "line": 3}));
        changed(&mut app, "changed", json!({"verified": true, "line": 3}));
        assert_eq!((state(&app, &a, 1), state(&app, &b, 2)), (a_now, b_now));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_removed_event_leaves_the_mark_failed_on_its_line() {
        let (mut app, dir, mut fakes) = setup("bp-removed", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        answered(&mut app, &a, vec![1], vec![bp(json!({"id": 1, "verified": true}))]);
        assert_eq!(state(&app, &a, 1).state, BpState::Verified);
        changed(&mut app, "removed", json!({"id": 1, "verified": true, "line": 2}));
        let want =
            BreakpointMark { state: BpState::Failed, id: None, adapter_line: None, message: Some("removed by the debug adapter".into()) };
        assert_eq!(state(&app, &a, 1), want);
        assert_eq!(keys(&app, &a), vec![1]);
        changed(&mut app, "changed", json!({"id": 1, "verified": true, "line": 2}));
        assert_eq!(state(&app, &a, 1), want);
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ended_resets_the_states() {
        let (mut app, dir, mut fakes) = setup("bp-ended", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake, sets) = running(&mut app, &mut fakes).await;
        answer(&fake, &sets[0], json!([{"id": 3, "verified": true}]));
        dap_until(&mut app, |x| state(x, &a, 1).state == BpState::Verified).await;
        fake.event("terminated", Value::Null);
        let disconnect = fake.recv("disconnect").await;
        fake.respond(&disconnect, Value::Null);
        dap_until(&mut app, gone).await;
        assert_eq!(state(&app, &a, 1), BreakpointMark::default());
        assert_eq!(keys(&app, &a), vec![1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_replace_resets_the_states_at_once() {
        let (mut app, dir, mut fakes) = setup("bp-replace", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake1, sets) = running(&mut app, &mut fakes).await;
        answer(&fake1, &sets[0], json!([{"id": 3, "verified": true}]));
        dap_until(&mut app, |x| state(x, &a, 1).state == BpState::Verified).await;
        app.perform(Action::Debug(DebugAction::Start));
        assert_eq!(state(&app, &a, 1), BreakpointMark::default());
        cooperate(&mut fake1).await;
        dap_until(&mut app, |x| x.debug.active()).await;
        let mut fake2 = fakes.try_recv().expect("the second adapter");
        assert_eq!(lines(&fake2.recv("setBreakpoints").await), vec![2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_replace_whose_start_is_refused_leaves_no_old_state() {
        let (mut app, dir, mut fakes) = setup("bp-replace-refused", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (mut fake1, sets) = running(&mut app, &mut fakes).await;
        answer(&fake1, &sets[0], json!([{"id": 3, "verified": true}]));
        dap_until(&mut app, |x| state(x, &a, 1).state == BpState::Verified).await;
        write_run_toml(&app, "");
        app.perform(Action::Debug(DebugAction::Start));
        cooperate(&mut fake1).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugReady { .. })).await;
        assert_eq!(state(&app, &a, 1), BreakpointMark::default());
        assert!(!app.debug.active());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_root_switch_resets_the_states() {
        let (mut app, dir, mut fakes) = setup("bp-root", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (fake, sets) = running(&mut app, &mut fakes).await;
        answer(&fake, &sets[0], json!([{"id": 3, "verified": true}]));
        dap_until(&mut app, |x| state(x, &a, 1).state == BpState::Verified).await;
        app.debug_root_changed();
        assert_eq!(state(&app, &a, 1), BreakpointMark::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_old_sessions_answer_is_dropped() {
        let (mut app, dir, mut fakes) = setup("bp-old-answer", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let (_fake, _) = running(&mut app, &mut fakes).await;
        let (generation, id) = (app.workspace.generation, app.debug.session.as_ref().unwrap().id);
        let body = DapBody::Breakpoints { source: a.clone(), requested: vec![1], result: vec![bp(json!({"id": 1, "verified": true}))] };
        app.dap_msg(DapMsg { generation, session: SessionId(id.0 + 1), body });
        assert_eq!(state(&app, &a, 1), BreakpointMark::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- stepping and the execution point (yman 414) ----

    use crate::actions::{AppAction, EditorAction, GitAction, HistoryAction, MergeAction};
    use crate::dispatch::Dispatch;
    use crate::ui::focus::ToolWindowId;

    const F7: KeyCode = KeyCode::F(7);
    const F8: KeyCode = KeyCode::F(8);
    const F9: KeyCode = KeyCode::F(9);

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(key(code, KeyModifiers::NONE));
    }

    /// What `key` is in the current focus and session state.
    fn action_of(app: &mut App, key: KeyEvent) -> Option<Action> {
        let c = app.contexts();
        match app.dispatcher.dispatch(key, &c, Some(app.mode), &app.keymap) {
            Dispatch::Action(a) => Some(a),
            _ => None,
        }
    }

    fn frame(path: &Path, line: i64) -> Value {
        json!({"id": 0, "name": "main", "line": line, "column": 0, "source": {"path": path.to_string_lossy()}})
    }

    fn epoch_of(app: &App) -> u64 {
        app.debug.session.as_ref().unwrap().epoch
    }

    fn thread_of(app: &App) -> Option<i64> {
        app.debug.session.as_ref().and_then(|l| l.thread)
    }

    fn point(app: &App) -> Option<(PathBuf, usize)> {
        app.editor.execution_point().map(|p| (p.path.clone(), p.line))
    }

    /// Like `dap_until`, and a file read for a stop is handled too.
    async fn pump_until(app: &mut App, done: impl Fn(&App) -> bool) {
        while !done(app) {
            handle_next(app, |m| matches!(m, AppMsg::Dap(_) | AppMsg::DebugReady { .. } | AppMsg::FileLoaded { .. })).await;
        }
    }

    /// The adapter reports a stop on `thread` and answers the stack with one `frame`.
    async fn stop(app: &mut App, fake: &mut FakeAdapter, thread: i64, frame: Value) {
        let before = epoch_of(app);
        fake.event("stopped", json!({"reason": "breakpoint", "threadId": thread}));
        pump_until(app, |a| epoch_of(a) > before).await;
        let req = fake.recv("stackTrace").await;
        assert_eq!(req["arguments"]["threadId"], thread);
        fake.respond(&req, json!({"stackFrames": [frame]}));
    }

    /// A stop at `path` on the 1-based `line`, handled up to the point being set.
    async fn stop_at(app: &mut App, fake: &mut FakeAdapter, thread: i64, path: &Path, line: i64) {
        stop(app, fake, thread, frame(path, line)).await;
        let want = (path.to_path_buf(), (line - 1) as usize);
        pump_until(app, |a| point(a).as_ref() == Some(&want)).await;
    }

    /// A running session stopped on thread 7 at `path`:`line`.
    pub(crate) async fn paused(app: &mut App, fakes: &mut UnboundedReceiver<FakeAdapter>, path: &Path, line: i64) -> FakeAdapter {
        let (mut fake, _) = running(app, fakes).await;
        stop_at(app, &mut fake, 7, path, line).await;
        fake
    }

    #[tokio::test]
    async fn suspended_keys_by_focus() {
        let (mut app, dir, mut fakes) = setup("step-keys", Order::LaunchEarly);
        let a = six(&mut app);
        let _fake = paused(&mut app, &mut fakes, &a, 3).await;
        let dbg = |d| Some(Action::Debug(d));
        let shift = KeyModifiers::SHIFT;
        let steps = |app: &mut App, f7: Option<Action>, what: &str| {
            assert_eq!(action_of(app, key(F7, KeyModifiers::NONE)), f7, "{what}: F7");
            assert_eq!(action_of(app, key(F8, KeyModifiers::NONE)), dbg(DebugAction::StepOver), "{what}: F8");
        };
        app.focus.set(PanelId::Editor);
        steps(&mut app, dbg(DebugAction::StepInto), "editor");
        assert_eq!(action_of(&mut app, key(F9, KeyModifiers::NONE)), dbg(DebugAction::Resume));
        assert_eq!(action_of(&mut app, key(F8, shift)), dbg(DebugAction::StepOut));
        assert_eq!(action_of(&mut app, key(KeyCode::F(10), KeyModifiers::ALT)), dbg(DebugAction::ShowExecutionPoint));
        for (panel, f7, name) in [
            (PanelId::Git, Some(Action::Git(GitAction::NextHunk)), "git"),
            (PanelId::LocalHistory, Some(Action::History(HistoryAction::NextChange)), "local history"),
            (PanelId::Project, dbg(DebugAction::StepInto), "project"),
        ] {
            app.focus.set(panel);
            steps(&mut app, f7, name);
            if panel != PanelId::Project {
                assert_eq!(action_of(&mut app, key(F9, KeyModifiers::NONE)), dbg(DebugAction::Resume), "{name}: F9");
                assert_eq!(action_of(&mut app, key(F8, shift)), dbg(DebugAction::StepOut), "{name}: Shift+F8");
            }
        }
        app.focus.set(PanelId::Editor);
        let texts = crate::git::merge::MergeTexts { base: "x\n".into(), ours: "y\n".into(), theirs: "z\n".into() };
        app.editor.open_merge(a.clone(), "a.log".into(), texts, ["main".into(), "dev".into()], "x\n".into(), &app.theme);
        assert!(app.editor.merge_active());
        steps(&mut app, Some(Action::Editor(EditorAction::Merge(MergeAction::NextConflict))), "merge");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn running_keys_are_unchanged() {
        let (mut app, dir, mut fakes) = setup("step-keys-running", Order::LaunchEarly);
        let a = six(&mut app);
        let _ = running(&mut app, &mut fakes).await;
        assert_eq!(app.debug.phase(), Some(Phase::Running));
        app.focus.set(PanelId::Editor);
        assert_eq!(action_of(&mut app, key(F7, KeyModifiers::NONE)), Some(Action::Editor(EditorAction::NextChange)));
        for (code, mods) in
            [(F8, KeyModifiers::NONE), (F9, KeyModifiers::NONE), (F8, KeyModifiers::SHIFT), (KeyCode::F(10), KeyModifiers::ALT)]
        {
            assert_eq!(action_of(&mut app, key(code, mods)), None, "{code:?} {mods:?}");
        }
        app.focus.set(PanelId::Git);
        assert_eq!(action_of(&mut app, key(F7, KeyModifiers::NONE)), Some(Action::Git(GitAction::NextHunk)));
        assert_eq!(action_of(&mut app, key(F8, KeyModifiers::NONE)), None);
        let _ = (a, std::fs::remove_dir_all(&dir));
    }

    #[tokio::test]
    async fn a_dialog_or_palette_stack_has_no_debug_suspended() {
        let (mut app, dir, mut fakes) = setup("step-keys-overlay", Order::LaunchEarly);
        let a = six(&mut app);
        let _fake = paused(&mut app, &mut fakes, &a, 3).await;
        assert!(app.contexts().contains(&"debug.suspended"));
        app.dialog = Some(crate::ui::dialog::Dialog::info("x", "y"));
        assert!(!app.contexts().contains(&"debug.suspended"));
        assert_eq!(action_of(&mut app, key(F8, KeyModifiers::NONE)), None);
        app.dialog = None;
        app.perform(Action::App(AppAction::OpenPalette(crate::actions::PaletteKind::Actions)));
        assert!(app.palette.is_some());
        assert!(!app.contexts().contains(&"debug.suspended"));
        assert_eq!(action_of(&mut app, key(F8, KeyModifiers::NONE)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_focused_terminal_keeps_the_step_keys() {
        let (mut app, dir, mut fakes) = setup("step-terminal", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 3).await;
        app.perform(Action::App(AppAction::ToggleToolWindow(ToolWindowId::Terminal)));
        app.start_terminal(10, 60);
        assert!(app.focus.is(PanelId::Terminal));
        press(&mut app, F8);
        assert_eq!(app.debug.phase(), Some(Phase::Suspended));
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_asks_the_stack_of_its_thread() {
        for (n, order) in fake::ORDERS.into_iter().enumerate() {
            let (mut app, dir, mut fakes) = setup(&format!("step-ask-{n}"), order);
            let a = six(&mut app);
            let (mut fake, _) = running(&mut app, &mut fakes).await;
            fake.event("stopped", json!({"reason": "breakpoint", "threadId": 7}));
            pump_until(&mut app, |x| thread_of(x) == Some(7)).await;
            let req = fake.recv("stackTrace").await;
            assert_eq!(req["arguments"], json!({"threadId": 7, "startFrame": 0, "levels": 20}), "{order:?}");
            let _ = (a, std::fs::remove_dir_all(&dir));
        }
    }

    #[tokio::test]
    async fn frame_id_zero_and_column_zero_set_the_point() {
        let (mut app, dir, mut fakes) = setup("step-frame-zero", Order::LaunchEarly);
        let a = six(&mut app);
        let _fake = paused(&mut app, &mut fakes, &a, 3).await;
        assert_eq!(point(&app), Some((a.clone(), 2)));
        assert_eq!(app.editor.active_path(), Some(a.as_path()));
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col().0, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_relative_source_path_joins_the_root() {
        let (mut app, dir, mut fakes) = setup("step-relative", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        let f = json!({"id": 1, "name": "main", "line": 2, "column": 1, "source": {"path": "a.log"}});
        stop(&mut app, &mut fake, 7, f).await;
        pump_until(&mut app, |x| point(x).is_some()).await;
        assert_eq!(point(&app), Some((app.workspace.root.join("a.log"), 1)));
        assert_eq!(a, app.workspace.root.join("a.log"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_without_a_thread_asks_threads_first() {
        let (mut app, dir, mut fakes) = setup("step-no-thread", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        fake.event("stopped", json!({"reason": "pause"}));
        pump_until(&mut app, |x| epoch_of(x) > 0).await;
        let threads = fake.recv("threads").await;
        fake.respond(&threads, json!({"threads": [{"id": 4}, {"id": 9}]}));
        pump_until(&mut app, |x| thread_of(x) == Some(4)).await;
        let req = fake.recv("stackTrace").await;
        assert_eq!(req["arguments"]["threadId"], 4);
        fake.respond(&req, json!({"stackFrames": [frame(&a, 2)]}));
        pump_until(&mut app, |x| point(x).is_some()).await;
        press(&mut app, F8);
        assert_eq!(fake.recv("next").await["arguments"]["threadId"], 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_without_a_thread_refuses_to_step_until_known() {
        let (mut app, dir, mut fakes) = setup("step-unknown-thread", Order::LaunchEarly);
        let _ = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        fake.event("stopped", json!({"reason": "pause"}));
        pump_until(&mut app, |x| x.debug.suspended() && epoch_of(x) > 0).await;
        let _threads = fake.recv("threads").await;
        press(&mut app, F8);
        assert_eq!(last(&app), (Level::Info, "The paused thread is not known yet".to_string()));
        // Only the new `threads` request goes out, no `next`.
        let _again = fake.recv("threads").await;
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_threads_answer_is_asked_again_by_a_step_key() {
        let (mut app, dir, mut fakes) = setup("step-threads-retry", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        fake.event("stopped", json!({"reason": "pause"}));
        pump_until(&mut app, |x| x.debug.suspended() && epoch_of(x) > 0).await;
        let first = fake.recv("threads").await;
        fake.respond_error(&first, "boom");
        pump_until(&mut app, |x| texts(x).iter().any(|t| t.starts_with("Could not read the threads"))).await;
        press(&mut app, F8);
        assert_eq!(last(&app), (Level::Info, "The paused thread is not known yet".to_string()));
        let again = fake.recv("threads").await;
        fake.respond(&again, json!({"threads": [{"id": 4}]}));
        pump_until(&mut app, |x| thread_of(x) == Some(4)).await;
        let stack = fake.recv("stackTrace").await;
        assert_eq!(stack["arguments"]["threadId"], 4);
        fake.respond(&stack, json!({"stackFrames": [frame(&a, 2)]}));
        pump_until(&mut app, |x| point(x).is_some()).await;
        press(&mut app, F8);
        // `fake.recv` skips other messages, so `next` as the very next one is read with `next()`.
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), m["arguments"]["threadId"].as_i64()), (Some("next"), Some(4)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_threads_warns() {
        let (mut app, dir, mut fakes) = setup("step-no-threads", Order::LaunchEarly);
        let _ = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        fake.event("stopped", json!({"reason": "pause"}));
        pump_until(&mut app, |x| epoch_of(x) > 0).await;
        let threads = fake.recv("threads").await;
        fake.respond(&threads, json!({"threads": []}));
        pump_until(&mut app, |x| texts(x).contains(&"The adapter reports no threads".to_string())).await;
        assert_eq!(last(&app), (Level::Warn, "The adapter reports no threads".to_string()));
        assert_eq!(point(&app), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_source_path_toasts_and_sets_no_point() {
        let (mut app, dir, mut fakes) = setup("step-no-source", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        let p = a.to_string_lossy().into_owned();
        let cases = [
            json!({"id": 0, "name": "main", "line": 3, "column": 0}),
            json!({"id": 0, "name": "main", "line": 3, "column": 0, "source": {"name": "a.log"}}),
            json!({"id": 0, "name": "main", "line": 0, "column": 0, "source": {"path": p}}),
            json!({"id": 0, "name": "main", "line": 3, "column": 0, "source": {"path": p, "sourceReference": 5}}),
            json!({"id": 0, "name": "main", "line": 3, "column": 0, "source": {"path": ""}}),
        ];
        let toast = "Paused in main, no source".to_string();
        for (n, case) in cases.into_iter().enumerate() {
            // A point from an earlier stop has to go.
            stop_at(&mut app, &mut fake, 7, &a, 2).await;
            let before = texts(&app).iter().filter(|t| **t == toast).count();
            stop(&mut app, &mut fake, 7, case).await;
            pump_until(&mut app, |x| texts(x).iter().filter(|t| **t == toast).count() > before).await;
            assert_eq!(point(&app), None, "case {n}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_frames_warns() {
        let (mut app, dir, mut fakes) = setup("step-no-frames", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        stop_at(&mut app, &mut fake, 7, &a, 2).await;
        let before = epoch_of(&app);
        fake.event("stopped", json!({"reason": "step", "threadId": 7}));
        pump_until(&mut app, |x| epoch_of(x) > before).await;
        let req = fake.recv("stackTrace").await;
        fake.respond(&req, json!({"stackFrames": []}));
        pump_until(&mut app, |x| point(x).is_none()).await;
        assert_eq!(last(&app), (Level::Warn, "Paused, the adapter reports no stack".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unopened_file_opens_at_the_frame() {
        let (mut app, dir, mut fakes) = setup("step-unopened", Order::LaunchEarly);
        let _ = six(&mut app);
        let b = app.workspace.root.join("b.log");
        std::fs::write(&b, "b0\nb1\nb2\nb3\n").unwrap();
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        assert!(app.editor.index_of(&b).is_none());
        stop(&mut app, &mut fake, 7, frame(&b, 3)).await;
        pump_until(&mut app, |x| x.editor.index_of(&b).is_some()).await;
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col().0, 2);
        assert_eq!(point(&app), Some((b, 2)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_stack_answer_is_dropped() {
        let (mut app, dir, mut fakes) = setup("step-stale", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        fake.event("stopped", json!({"reason": "breakpoint", "threadId": 7}));
        pump_until(&mut app, |x| thread_of(x) == Some(7)).await;
        let old = fake.recv("stackTrace").await;
        press(&mut app, F8);
        let _next = fake.recv("next").await;
        fake.respond(&old, json!({"stackFrames": [frame(&a, 3)]}));
        fake.event("stopped", json!({"reason": "breakpoint", "threadId": 8}));
        pump_until(&mut app, |x| thread_of(x) == Some(8)).await;
        assert_eq!(point(&app), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_stack_error_gives_no_toast() {
        let (mut app, dir, mut fakes) = setup("step-stale-error", Order::LaunchEarly);
        let _ = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        fake.event("stopped", json!({"reason": "breakpoint", "threadId": 7}));
        pump_until(&mut app, |x| thread_of(x) == Some(7)).await;
        let old = fake.recv("stackTrace").await;
        press(&mut app, F8);
        let _next = fake.recv("next").await;
        // gdb's answer to a stack request for a thread that has run on.
        fake.respond_error(&old, "list index out of range");
        fake.event("stopped", json!({"reason": "breakpoint", "threadId": 8}));
        pump_until(&mut app, |x| thread_of(x) == Some(8)).await;
        assert!(texts(&app).iter().all(|t| !t.starts_with("Could not")), "{:?}", texts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stack_answer_outside_suspended_is_ignored() {
        let (mut app, dir, mut fakes) = setup("step-answer-stopping", Order::LaunchEarly);
        let a = six(&mut app);
        let b = app.workspace.root.join("b.log");
        std::fs::write(&b, "b0\nb1\nb2\nb3\n").unwrap();
        let _ = running(&mut app, &mut fakes).await;
        let line = app.editor.active_buffer().unwrap().cursor_line_col().0;
        let tabs = app.editor.buffers.len();
        inject(&mut app, DapBody::Phase(Phase::Stopping));
        let tag = DapTag { epoch: epoch_of(&app), kind: TagKind::StackTrace { thread: 7 } };
        inject(&mut app, DapBody::Response { tag, result: Ok(json!({"stackFrames": [frame(&b, 3)]})) });
        // A file read, if one was started, lands here.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(point(&app), None);
        assert!(app.editor.index_of(&b).is_none());
        assert_eq!(app.editor.buffers.len(), tabs);
        assert_eq!(app.editor.active_path(), Some(a.as_path()));
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col().0, line);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_point_clears_on_running() {
        let (mut app, dir, mut fakes) = setup("step-clear-running", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 3).await;
        press(&mut app, F8);
        pump_until(&mut app, |x| point(x).is_none()).await;
        let next = fake.recv("next").await;
        fake.respond(&next, json!({}));
        stop_at(&mut app, &mut fake, 7, &a, 4).await;
        fake.event("continued", json!({"threadId": 7}));
        pump_until(&mut app, |x| point(x).is_none()).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_point_clears_on_end() {
        let (mut app, dir, mut fakes) = setup("step-clear-end", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 3).await;
        fake.event("terminated", Value::Null);
        let disconnect = fake.recv("disconnect").await;
        fake.respond(&disconnect, Value::Null);
        dap_until(&mut app, gone).await;
        assert_eq!(point(&app), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_point_clears_on_a_new_start() {
        let (mut app, dir, mut fakes) = setup("step-clear-start", Order::LaunchEarly);
        let a = six(&mut app);
        let _fake = paused(&mut app, &mut fakes, &a, 3).await;
        app.perform(Action::Debug(DebugAction::Start));
        assert_eq!(point(&app), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_point_clears_on_a_root_switch() {
        let (mut app, dir, mut fakes) = setup("step-clear-root", Order::LaunchEarly);
        let a = six(&mut app);
        let _fake = paused(&mut app, &mut fakes, &a, 3).await;
        app.debug_root_changed();
        assert_eq!(point(&app), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_step_keys_send_their_command_with_the_thread() {
        let (mut app, dir, mut fakes) = setup("step-commands", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 2).await;
        let keys = [
            (key(F8, KeyModifiers::NONE), "next"),
            (key(F7, KeyModifiers::NONE), "stepIn"),
            (key(F8, KeyModifiers::SHIFT), "stepOut"),
            (key(F9, KeyModifiers::NONE), "continue"),
        ];
        for (n, (k, command)) in keys.into_iter().enumerate() {
            app.handle_key(k);
            let req = fake.recv(command).await;
            assert_eq!(req["arguments"]["threadId"], 7, "{command}");
            fake.respond(&req, json!({}));
            pump_until(&mut app, |x| point(x).is_none()).await;
            stop_at(&mut app, &mut fake, 7, &a, 3 + n as i64).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_second_stops_thread_is_used() {
        let (mut app, dir, mut fakes) = setup("step-second-thread", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 2).await;
        stop_at(&mut app, &mut fake, 8, &a, 4).await;
        press(&mut app, F8);
        assert_eq!(fake.recv("next").await["arguments"]["threadId"], 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_step_key_needs_a_pause() {
        let (mut app, dir, mut fakes) = setup("step-needs-pause", Order::LaunchEarly);
        let _ = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.perform(Action::Debug(DebugAction::StepOver));
        assert_eq!(last(&app), (Level::Info, "No paused debug session".to_string()));
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pause_with_a_known_thread() {
        let (mut app, dir, mut fakes) = setup("pause-known", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 2).await;
        press(&mut app, F9);
        dap_until(&mut app, |x| x.debug.phase() == Some(Phase::Running)).await;
        app.perform(Action::Debug(DebugAction::Pause));
        assert_eq!(fake.recv("pause").await["arguments"]["threadId"], 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pause_with_no_known_thread_asks_threads() {
        let (mut app, dir, mut fakes) = setup("pause-unknown", Order::LaunchEarly);
        let _ = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.perform(Action::Debug(DebugAction::Pause));
        let threads = fake.recv("threads").await;
        fake.respond(&threads, json!({"threads": [{"id": 3}]}));
        dap_until(&mut app, |x| thread_of(x) == Some(3)).await;
        assert_eq!(fake.recv("pause").await["arguments"]["threadId"], 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pause_when_not_running_refuses() {
        let (mut app, dir, mut fakes) = setup("pause-refuses", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 2).await;
        app.perform(Action::Debug(DebugAction::Pause));
        assert_eq!(last(&app), (Level::Info, "No running debug session".to_string()));
        assert_quiet(&mut app, &mut fake, "q1.log").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_stack_answer_warns() {
        let (mut app, dir, mut fakes) = setup("step-failed-stack", Order::LaunchEarly);
        let _ = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        fake.event("stopped", json!({"reason": "breakpoint", "threadId": 7}));
        pump_until(&mut app, |x| thread_of(x) == Some(7)).await;
        let req = fake.recv("stackTrace").await;
        fake.respond_error(&req, "boom");
        pump_until(&mut app, |x| texts(x).iter().any(|t| t.starts_with("Could not read the stack"))).await;
        assert_eq!(last(&app), (Level::Warn, "Could not read the stack: boom".to_string()));
        assert_eq!(point(&app), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_answer_refused_at_shutdown_is_silent() {
        let (mut app, dir, _fakes, _fake) = live("step-shutdown-answer", Order::LaunchEarly).await;
        inject(&mut app, DapBody::Phase(Phase::Stopping));
        let before = texts(&app);
        let tag = DapTag { epoch: 0, kind: TagKind::StackTrace { thread: 7 } };
        inject(&mut app, DapBody::Response { tag, result: Err("not available".into()) });
        assert_eq!(texts(&app), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn show_execution_point_returns_and_focuses_the_editor() {
        let (mut app, dir, mut fakes) = setup("step-show", Order::LaunchEarly);
        let a = six(&mut app);
        let _fake = paused(&mut app, &mut fakes, &a, 3).await;
        app.editor.goto_line(1, 1);
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col().0, 0);
        app.focus.set(PanelId::Git);
        app.handle_key(key(KeyCode::F(10), KeyModifiers::ALT));
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col().0, 2);
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn show_execution_point_with_none_toasts() {
        let (mut app, dir) = test_app("step-show-none");
        app.perform(Action::Debug(DebugAction::ShowExecutionPoint));
        assert_eq!(last(&app), (Level::Info, "No execution point".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_leaves_focus_in_the_panel() {
        let (mut app, dir, mut fakes) = setup("step-focus", Order::LaunchEarly);
        let a = six(&mut app);
        let b = app.workspace.root.join("b.log");
        std::fs::write(&b, "b0\nb1\nb2\nb3\n").unwrap();
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.editor.goto_line(1, 1);
        app.focus.set(PanelId::Git);
        stop_at(&mut app, &mut fake, 7, &a, 3).await;
        assert!(app.focus.is(PanelId::Git));
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col().0, 2);
        // The same with a file that is not open yet.
        stop(&mut app, &mut fake, 7, frame(&b, 2)).await;
        pump_until(&mut app, |x| x.editor.index_of(&b).is_some() && point(x) == Some((b.clone(), 1))).await;
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        assert!(app.focus.is(PanelId::Git));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_breakpoint_cleared_while_paused_goes_out_before_continue() {
        let (mut app, dir, mut fakes) = setup("step-cleared-bp", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let mut fake = paused(&mut app, &mut fakes, &a, 2).await;
        toggle(&mut app, &a, 1);
        // No tick between the toggle and the key.
        press(&mut app, F9);
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), a.to_string_lossy().into_owned(), vec![]));
        assert_eq!(fake.next().await["command"], "continue");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_breakpoint_cleared_while_paused_goes_out_before_a_step() {
        let (mut app, dir, mut fakes) = setup("step-cleared-bp-step", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let mut fake = paused(&mut app, &mut fakes, &a, 2).await;
        toggle(&mut app, &a, 1);
        press(&mut app, F8);
        let m = fake.next().await;
        assert_eq!((m["command"].as_str(), source(&m), lines(&m)), (Some("setBreakpoints"), a.to_string_lossy().into_owned(), vec![]));
        assert_eq!(fake.next().await["command"], "next");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_sync_after_the_phase_ended() {
        let (mut app, dir, mut fakes) = setup("step-no-sync-ended", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let _ = running(&mut app, &mut fakes).await;
        inject(&mut app, DapBody::Phase(Phase::Ended));
        let before = app.debug.session.as_ref().unwrap().sent.clone();
        toggle(&mut app, &a, 3);
        app.on_tick();
        assert_eq!(app.debug.session.as_ref().unwrap().sent, before);
        let _ = std::fs::remove_dir_all(&dir);
    }
    // ---- the Debug window and its Console (yman 415) ----

    fn console(app: &App) -> Vec<String> {
        app.debug_panel.texts().into_iter().map(String::from).collect()
    }

    fn output(fake: &FakeAdapter, category: &str, text: &str) {
        fake.event("output", json!({"category": category, "output": text}));
    }

    #[tokio::test]
    async fn program_output_reaches_the_console_without_telemetry() {
        let (mut app, dir, _fakes, fake) = live("console-output", Order::LaunchDeferred).await;
        output(&fake, "telemetry", "secret\n");
        output(&fake, "stdout", "x=3 \u{1F600}\n");
        console_until(&mut app, |a| console(a).contains(&"x=3 \u{1F600}".to_string())).await;
        assert_eq!(console(&app), ["x=3 \u{1F600}"], "telemetry is not shown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_tail_is_kept_after_the_session_ended() {
        let (mut app, dir, _fakes, mut fake) = live("console-tail", Order::LaunchDeferred).await;
        output(&fake, "stdout", "tail");
        fake.event("terminated", Value::Null);
        let disconnect = fake.recv("disconnect").await;
        fake.respond(&disconnect, Value::Null);
        console_until(&mut app, gone).await;
        console_until(&mut app, |a| console(a).contains(&"tail".to_string())).await;
        assert!(app.debug.session.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_console_lines_are_dropped() {
        let (mut app, dir, _fakes, _fake) = live("console-stale", Order::LaunchDeferred).await;
        let (generation, session) = (app.workspace.generation, app.debug_panel.session().expect("a session"));
        let batch = |generation, session, text: &str| AppMsg::DebugOutput { generation, session, lines: vec![RunLine::plain(text)] };
        app.handle_msg(batch(generation.wrapping_sub(1), session, "old generation"));
        app.handle_msg(batch(generation, SessionId(session.0 + 1), "other session"));
        assert!(console(&app).is_empty(), "{:?}", console(&app));
        app.handle_msg(batch(generation, session, "current"));
        assert_eq!(console(&app), ["current"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_new_start_and_a_root_switch_empty_the_console() {
        let (mut app, dir, mut fakes, mut fake1) = live("console-replace", Order::LaunchDeferred).await;
        let first = app.debug_panel.session().unwrap();
        output(&fake1, "stdout", "first run\n");
        console_until(&mut app, |a| !console(a).is_empty()).await;
        shift_f9(&mut app);
        cooperate(&mut fake1).await;
        console_until(&mut app, |a| a.debug.active()).await;
        let _fake2 = fakes.try_recv().expect("the second adapter");
        assert_ne!(app.debug_panel.session(), Some(first));
        assert!(console(&app).is_empty(), "{:?}", console(&app));
        assert_eq!(app.debug_panel.title(app.debug.phase()), " Debug: demo · starting ");
        app.debug_panel.push(vec![RunLine::plain("second run")]);
        app.debug_root_changed();
        assert!(console(&app).is_empty());
        assert_eq!(app.debug_panel.title(None), " Debug ");
        assert_eq!(app.debug_panel.session(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_first_start_shows_the_window_and_leaves_focus() {
        let (mut app, dir, mut fakes) = setup("window-first", Order::LaunchDeferred);
        assert!(!app.layout.is_visible(ToolWindowId::Debug));
        app.focus.set(PanelId::Editor);
        let mut fake = start(&mut app, &mut fakes).await;
        assert!(app.layout.is_visible(ToolWindowId::Debug));
        assert!(app.focus.is(PanelId::Editor), "the window takes no focus");
        // Hidden by the user, a later start leaves it hidden.
        dap_until(&mut app, |a| a.debug.phase().is_some_and(|p| p != Phase::Initializing)).await;
        app.layout.hide(ToolWindowId::Debug);
        app.perform(Action::Debug(DebugAction::Stop));
        cooperate(&mut fake).await;
        dap_until(&mut app, gone).await;
        let _fake2 = start(&mut app, &mut fakes).await;
        assert!(!app.layout.is_visible(ToolWindowId::Debug));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_window_the_debug_window_replaces_gives_up_focus() {
        let (mut app, dir, mut fakes) = setup("window-replaces", Order::LaunchDeferred);
        let dock = app.layout.dock_of(ToolWindowId::Run);
        app.layout.move_to_dock(ToolWindowId::Debug, dock);
        app.layout.show(ToolWindowId::Run);
        app.focus.set(PanelId::Run);
        let _fake = start(&mut app, &mut fakes).await;
        assert!(app.layout.is_visible(ToolWindowId::Debug));
        assert!(!app.layout.is_visible(ToolWindowId::Run));
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_f5_shows_focuses_and_hides() {
        let (mut app, dir) = test_app("window-alt-f5");
        let alt_f5 = key(KeyCode::F(5), KeyModifiers::ALT);
        assert!(!app.layout.is_visible(ToolWindowId::Debug));
        app.handle_key(alt_f5);
        assert!(app.layout.is_visible(ToolWindowId::Debug));
        assert!(app.focus.is(PanelId::Debug));
        app.handle_key(alt_f5);
        assert!(!app.layout.is_visible(ToolWindowId::Debug));
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn f_keys_step_with_the_window_focused() {
        let (mut app, dir, mut fakes) = setup("window-f-keys", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 3).await;
        app.focus.set(PanelId::Debug);
        let ctx = app.contexts();
        let at = |name: &str| ctx.iter().position(|c| *c == name).unwrap_or_else(|| panic!("{name} not in {ctx:?}"));
        assert!(at("debug.suspended") < at("debug"), "{ctx:?}");
        for (n, (code, mods, command)) in [
            (F8, KeyModifiers::NONE, "next"),
            (F7, KeyModifiers::NONE, "stepIn"),
            (F8, KeyModifiers::SHIFT, "stepOut"),
            (F9, KeyModifiers::NONE, "continue"),
        ]
        .into_iter()
        .enumerate()
        {
            app.handle_key(key(code, mods));
            let req = fake.recv(command).await;
            fake.respond(&req, json!({}));
            pump_until(&mut app, |x| point(x).is_none()).await;
            stop_at(&mut app, &mut fake, 7, &a, 4 + n as i64).await;
            assert!(app.focus.is(PanelId::Debug), "{command}: focus stays");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_shows_in_the_title_and_not_as_a_toast() {
        let (mut app, dir, mut fakes) = setup("window-title", Order::LaunchEarly);
        let a = six(&mut app);
        let mut fake = paused(&mut app, &mut fakes, &a, 3).await;
        assert!(app.debug_panel.title(app.debug.phase()).contains("paused (breakpoint)"));
        assert!(app.layout.is_visible(ToolWindowId::Debug));
        assert!(texts(&app).iter().all(|t| !t.starts_with("Paused (")), "{:?}", texts(&app));
        press(&mut app, F9);
        let req = fake.recv("continue").await;
        fake.respond(&req, json!({}));
        pump_until(&mut app, |x| point(x).is_none() && x.debug.phase() == Some(Phase::Running)).await;
        assert_eq!(app.debug_panel.title(app.debug.phase()), " Debug: demo · running ");
        assert_eq!(app.debug_panel.title(Some(Phase::Suspended)), " Debug: demo · paused ", "the reason went with the pause");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_with_the_window_hidden_is_a_toast() {
        let (mut app, dir, mut fakes) = setup("window-hidden-stop", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.layout.hide(ToolWindowId::Debug);
        stop_at(&mut app, &mut fake, 7, &a, 3).await;
        assert!(texts(&app).contains(&"Paused (breakpoint)".to_string()), "{:?}", texts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_under_another_maximized_window_is_a_toast() {
        let (mut app, dir, mut fakes) = setup("window-covered-stop", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.layout.toggle_maximize(ToolWindowId::Project);
        assert_eq!(app.layout.maximized_window(), Some(ToolWindowId::Project));
        assert!(app.layout.is_visible(ToolWindowId::Debug));
        stop_at(&mut app, &mut fake, 7, &a, 3).await;
        assert!(texts(&app).contains(&"Paused (breakpoint)".to_string()), "{:?}", texts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_with_the_window_itself_maximized_is_no_toast() {
        let (mut app, dir, mut fakes) = setup("window-maximized-stop", Order::LaunchEarly);
        let a = six(&mut app);
        let (mut fake, _) = running(&mut app, &mut fakes).await;
        app.layout.toggle_maximize(ToolWindowId::Debug);
        assert_eq!(app.layout.maximized_window(), Some(ToolWindowId::Debug));
        stop_at(&mut app, &mut fake, 7, &a, 3).await;
        assert!(texts(&app).iter().all(|t| !t.starts_with("Paused (")), "{:?}", texts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_breakpoint_verdict_is_written_once() {
        let (mut app, dir, mut fakes) = setup("console-verdict", Order::LaunchEarly);
        let a = six(&mut app);
        toggle(&mut app, &a, 1);
        let _ = running(&mut app, &mut fakes).await;
        // The adapter put it on its 1-based line 4.
        answered(&mut app, &a, vec![1], vec![bp(json!({"id": 1, "verified": true, "line": 4}))]);
        assert_eq!(console(&app), ["Breakpoint a.log:2 moved to line 4"]);
        // Whole-list answers repeat: nothing new.
        answered(&mut app, &a, vec![1], vec![bp(json!({"id": 1, "verified": true, "line": 4}))]);
        assert_eq!(console(&app).len(), 1);
        // Verified where it was asked, and pending: nothing to say.
        answered(&mut app, &a, vec![1], vec![bp(json!({"id": 1, "verified": true, "line": 2}))]);
        answered(&mut app, &a, vec![1], vec![bp(json!({"id": 1, "verified": false}))]);
        assert_eq!(console(&app).len(), 1);
        // A failure carries the adapter's words.
        answered(&mut app, &a, vec![1], vec![bp(json!({"verified": false, "reason": "failed", "message": "no code"}))]);
        assert_eq!(console(&app)[1], "Breakpoint a.log:2 failed: no code");
        // A file with no open buffer has no marks and writes nothing.
        let closed = app.workspace.root.join("closed.log");
        answered(&mut app, &closed, vec![0], vec![bp(json!({"verified": false, "reason": "failed", "message": "x"}))]);
        assert_eq!(console(&app).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_verdict_line_names_the_place_and_links_it() {
        let path = Path::new("/p/a.log");
        let mark =
            |state, adapter_line, message: Option<&str>| BreakpointMark { state, id: None, adapter_line, message: message.map(Into::into) };
        let moved = verdict_line(path, 1, &mark(BpState::Verified, Some(3), None)).unwrap();
        assert_eq!((moved.text.as_str(), moved.severity), ("Breakpoint a.log:2 moved to line 4", None));
        let link = moved.link.unwrap();
        assert_eq!((link.path.as_path(), link.line, &moved.text[link.span]), (path, 3, "a.log:2"));
        let failed = verdict_line(path, 1, &mark(BpState::Failed, None, None)).unwrap();
        assert_eq!((failed.text.as_str(), failed.severity), ("Breakpoint a.log:2 failed", Some(Severity::Warning)));
        assert_eq!(failed.link.unwrap().line, 1, "a failed one links the user's line");
        assert!(verdict_line(path, 1, &mark(BpState::Verified, None, None)).is_none());
        assert!(verdict_line(path, 1, &mark(BpState::Pending, None, None)).is_none());
        // An adapter's message is escape codes and lines: the verdict is one clean line.
        let noisy = verdict_line(path, 1, &mark(BpState::Failed, None, Some("\x1b[31mno code\x1b[0m\nat line 2"))).unwrap();
        assert_eq!(noisy.text, "Breakpoint a.log:2 failed: no code at line 2");
    }

    // ---- the build step (yman 428) ----

    /// A run.toml whose `demo` has this `build` (TOML array text) and this `launch` body.
    fn build_toml(build: &str, launch: &str) -> String {
        format!(
            "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nadapter = \"fake\"\nbuild = {build}\nlaunch = {{ {launch} }}\n"
        )
    }

    const SLEEPER: &str = r#"["sh", "-c", "echo building; sleep 30 & echo $! > sleeper.pid; wait"]"#;
    const ARTIFACT_LAUNCH: &str = r#"program = "${artifact}""#;

    /// `one.jsonl` in the root: a compact artifact line per executable.
    fn write_artifacts(app: &App, exes: &[&Path]) {
        let lines: Vec<String> = exes
            .iter()
            .map(|e| {
                let name = e.file_name().unwrap().to_string_lossy().into_owned();
                json!({"reason": "compiler-artifact", "target": {"name": name, "kind": ["bin"]}, "profile": {"test": false}, "executable": e})
                    .to_string()
            })
            .collect();
        std::fs::write(app.workspace.root.join("one.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    async fn read_pid(file: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_to_string(file).ok().and_then(|t| t.trim().parse().ok()) {
                return pid;
            }
            assert!(Instant::now() < deadline, "no pid in {}", file.display());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rsplit(')').next().is_some_and(|r| !r.trim_start().starts_with('Z')))
    }

    async fn dies(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    /// Presses Debug on a build that sleeps; the sleeper's pid, once the build runs.
    async fn building(app: &mut App) -> u32 {
        shift_f9(app);
        console_until(app, |a| a.debug.building.is_some()).await;
        read_pid(&app.workspace.root.join("sleeper.pid")).await
    }

    fn title(app: &App) -> String {
        app.debug_panel.title(app.debug.phase())
    }

    fn dialog_items(app: &App) -> Vec<String> {
        match &app.dialog.as_ref().expect("a dialog").kind {
            crate::ui::dialog::DialogKind::Choice { items, .. } => items.clone(),
            k => panic!("not a choice: {k:?}"),
        }
    }

    fn answer_dialog(app: &mut App, res: crate::ui::dialog::DialogResult) {
        let d = app.dialog.take().expect("a dialog");
        app.finish_dialog(d.id, res);
    }

    /// Two executables in reverse order, a choice dialog open.
    async fn choosing(name: &str) -> (App, PathBuf, UnboundedReceiver<FakeAdapter>, Vec<PathBuf>) {
        let (mut app, dir) = test_app(name);
        let fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let root = app.workspace.root.clone();
        let exes = vec![root.join("bin/zeta"), root.join("bin/alpha")];
        write_artifacts(&app, &[&exes[0], &exes[1]]);
        write_run_toml(&app, &build_toml(r#"["cat", "one.jsonl"]"#, ARTIFACT_LAUNCH));
        shift_f9(&mut app);
        console_until(&mut app, |a| a.dialog.is_some()).await;
        (app, dir, fakes, exes)
    }

    #[tokio::test]
    async fn a_build_fills_the_artifact_and_launches() {
        let (mut app, dir) = test_app("build-fill");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let exe = app.workspace.root.join("bin/prog");
        write_artifacts(&app, &[&exe]);
        write_run_toml(&app, &build_toml(r#"["cat", "one.jsonl"]"#, ARTIFACT_LAUNCH));
        shift_f9(&mut app);
        console_until(&mut app, |a| a.debug.building.is_some()).await;
        let before = app.debug_panel.session().expect("the build has a session");
        assert_eq!(title(&app), " Debug: demo · building ");
        console_until(&mut app, |a| a.debug.session.is_some()).await;
        let mut fake = fakes.try_recv().expect("an adapter");
        assert_eq!(fake.recv("launch").await["arguments"]["program"], json!(exe.to_string_lossy()));
        assert_eq!(app.debug_panel.session(), Some(before), "the build's Console is the session's");
        assert_eq!(app.debug.session.as_ref().unwrap().id, before);
        assert_eq!(title(&app), " Debug: demo · starting ");
        // The build's artifact lines are not raw JSON in the Console; the command line is first.
        let lines = console(&app);
        assert_eq!(lines[0], "$ cat one.jsonl");
        assert!(lines.iter().all(|l| !l.contains("reason")), "{lines:?}");
        assert!(texts(&app).contains(&"Building demo".to_string()), "{:?}", texts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_build_starts_nothing() {
        for layout in ["shown", "hidden", "covered"] {
            let (mut app, dir) = test_app(&format!("build-failed-{layout}"));
            let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
            match layout {
                "hidden" => app.debug.window_shown = true,
                "covered" => {
                    app.debug.window_shown = true;
                    app.layout.show(ToolWindowId::Debug);
                    app.layout.toggle_maximize(ToolWindowId::Project);
                }
                _ => {}
            }
            write_run_toml(&app, &build_toml(r#"["sh", "-c", "echo broken; exit 3"]"#, ARTIFACT_LAUNCH));
            shift_f9(&mut app);
            console_until(&mut app, |a| !a.debug.starting()).await;
            assert!(fakes.try_recv().is_err(), "{layout}: an adapter was connected");
            assert_eq!(last(&app), (Level::Warn, "Build of 'demo' failed with exit code 3".to_string()), "{layout}");
            assert!(!app.debug.starting() && !app.debug.active() && app.debug.building.is_none(), "{layout}");
            assert_eq!(title(&app), " Debug: demo · build failed ", "{layout}");
            let lines = console(&app);
            assert_eq!(lines, ["$ sh -c 'echo broken; exit 3'", "broken", "Build of 'demo' failed with exit code 3"], "{layout}");
            let failure = app.debug_panel.severities().last().copied().flatten();
            assert_eq!(failure, Some(Severity::Error), "{layout}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn a_killed_build_says_the_signal() {
        let (level, toast) = refused("build-signal", &build_toml(r#"["sh", "-c", "kill -9 $$"]"#, ARTIFACT_LAUNCH), |_| {}).await;
        assert_eq!((level, toast.as_str()), (Level::Warn, "Build of 'demo' was killed by signal 9"));
    }

    #[tokio::test]
    async fn a_missing_build_program_and_cwd_are_reported() {
        let missing = refused("build-missing", &build_toml(r#"["nonexistent-xyz"]"#, ARTIFACT_LAUNCH), |_| {}).await;
        assert_eq!(missing.0, Level::Warn);
        assert!(missing.1.starts_with("Cannot build 'demo': cannot run nonexistent-xyz: "), "{}", missing.1);
        let text = build_toml(r#"["true"]"#, ARTIFACT_LAUNCH).replace("command = [\"./demo\"]", "command = [\"./demo\"]\ncwd = \"nodir\"");
        let (mut app, dir) = test_app("build-no-dir");
        let root = app.workspace.root.clone();
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &text);
        shift_f9(&mut app);
        dap_until(&mut app, |a| !a.debug.starting()).await;
        assert!(fakes.try_recv().is_err());
        assert_eq!(last(&app), (Level::Warn, format!("Cannot build 'demo': {} is not a directory", root.join("nodir").display())));
        assert_eq!(title(&app), " Debug: demo · build failed ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_build_naming_no_executable_is_a_refusal() {
        let exact =
            "Cannot debug 'demo': its build named no executable, so ${artifact} has no value. A Cargo build needs --message-format=json";
        let (mut app, dir) = test_app("build-none");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &build_toml(r#"["true"]"#, ARTIFACT_LAUNCH));
        shift_f9(&mut app);
        console_until(&mut app, |a| !a.debug.starting()).await;
        assert!(fakes.try_recv().is_err());
        assert_eq!(last(&app), (Level::Warn, exact.to_string()));
        assert_eq!(title(&app), " Debug: demo · not started ");
        // A build that already asks for the JSON stream has no flag to add: it made no executable (a library-only crate).
        let none = "Cannot debug 'demo': its build named no executable, so ${artifact} has no value";
        for build in [
            r#"["true", "--message-format=json"]"#,
            r#"["true", "--message-format=json-render-diagnostics"]"#,
            r#"["true", "--message-format", "json"]"#,
        ] {
            write_run_toml(&app, &build_toml(build, ARTIFACT_LAUNCH));
            shift_f9(&mut app);
            console_until(&mut app, |a| !a.debug.starting()).await;
            assert_eq!(last(&app), (Level::Warn, none.to_string()), "{build}");
            assert_eq!(title(&app), " Debug: demo · not started ", "{build}");
        }
        // The same build without `${artifact}` launches.
        write_run_toml(&app, &build_toml(r#"["true"]"#, r#"program = "/p/x""#));
        let mut fake = start(&mut app, &mut fakes).await;
        assert_eq!(fake.recv("launch").await["arguments"]["program"], "/p/x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn several_executables_ask_and_the_choice_launches() {
        let (mut app, dir, mut fakes, exes) = choosing("build-choice").await;
        assert_eq!(dialog_items(&app), ["alpha (bin) bin/alpha", "zeta (bin) bin/zeta"], "sorted by path, relative to the root");
        let d = app.dialog.as_ref().unwrap();
        assert_eq!((d.title.as_str(), d.message.as_str()), ("Debug", "The build made 2 executables. Which one?"));
        assert!(app.debug.starting(), "the start waits for the answer");
        assert_eq!(title(&app), " Debug: demo · choosing an executable ", "the build is over");
        answer_dialog(&mut app, crate::ui::dialog::DialogResult::Choice(1));
        dap_until(&mut app, |a| a.debug.session.is_some()).await;
        let mut fake = fakes.try_recv().expect("an adapter");
        assert_eq!(fake.recv("launch").await["arguments"]["program"], json!(exes[0].to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancelling_the_choice_starts_and_stops_nothing() {
        let (mut app, dir, mut fakes, _) = choosing("build-choice-cancel").await;
        app.run.fake_running("task");
        answer_dialog(&mut app, crate::ui::dialog::DialogResult::Cancel);
        assert!(fakes.try_recv().is_err());
        assert!(!app.debug.starting() && app.debug.choice.is_none() && !app.debug.active());
        assert!(app.run.running() && !app.run.stop_requested(), "the Run task stays");
        assert_eq!(last(&app), (Level::Info, "Debug start cancelled".to_string()));
        assert_eq!(title(&app), " Debug: demo · not started ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_choice_waits_for_an_open_dialog() {
        let (mut app, dir) = test_app("build-choice-waits");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let root = app.workspace.root.clone();
        write_artifacts(&app, &[&root.join("a"), &root.join("b")]);
        write_run_toml(&app, &build_toml(r#"["cat", "one.jsonl"]"#, ARTIFACT_LAUNCH));
        app.dialog = Some(crate::ui::dialog::Dialog::info("Other", "text"));
        app.perform(Action::Debug(DebugAction::Start));
        console_until(&mut app, |a| a.debug.choice.is_some()).await;
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::Info, "the open dialog keeps its place");
        app.on_tick();
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::Info);
        app.dialog = None;
        app.on_tick();
        assert!(matches!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::DebugArtifact { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_choice_is_dropped() {
        let (mut app, dir, mut fakes, _) = choosing("build-choice-stale").await;
        let old = app.debug.seq;
        app.dialog = None;
        app.perform(Action::Debug(DebugAction::Start));
        console_until(&mut app, |a| a.dialog.is_some()).await;
        assert_eq!(app.debug.seq, old + 1);
        app.debug_artifact_chosen(old, Some(0));
        assert!(fakes.try_recv().is_err(), "an old answer launched");
        assert!(app.debug.choice.is_some() && app.debug.starting(), "the new start's choice is still waiting");
        answer_dialog(&mut app, crate::ui::dialog::DialogResult::Choice(0));
        dap_until(&mut app, |a| a.debug.session.is_some()).await;
        assert!(fakes.try_recv().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ctrl_f2_kills_the_build_not_the_run_task() {
        let (mut app, dir) = test_app("build-ctrl-f2");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &build_toml(SLEEPER, ARTIFACT_LAUNCH));
        let sleeper = building(&mut app).await;
        assert!(alive(sleeper));
        app.run.fake_running("task");
        ctrl_f2(&mut app);
        assert!(!app.debug.starting() && app.debug.building.is_none());
        assert!(dies(sleeper).await, "the build's process group survived");
        assert!(app.run.running() && !app.run.stop_requested(), "Ctrl+F2 went to the Run task");
        assert_eq!(last(&app), (Level::Info, "Debug start cancelled".to_string()));
        assert_eq!(title(&app), " Debug: demo · not started ");
        // The build's end arrives late and changes nothing.
        handle_next(&mut app, |m| matches!(m, AppMsg::DebugBuilt { .. })).await;
        assert!(fakes.try_recv().is_err() && !app.debug.active());
        assert_eq!(title(&app), " Debug: demo · not started ");
        assert!(console(&app).iter().all(|l| !l.contains("failed")), "{:?}", console(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_stop_button_cancels_a_build() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("build-stop-button");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &build_toml(SLEEPER, ARTIFACT_LAUNCH));
        let sleeper = building(&mut app).await;
        assert!(app.layout.is_visible(ToolWindowId::Debug));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let r = app.debug_panel.hit_rect("Stop").expect("the Stop button is drawn");
        let click = |kind| MouseEvent { kind, column: r.x, row: r.y, modifiers: KeyModifiers::NONE };
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)));
        app.handle_mouse(click(MouseEventKind::Up(MouseButton::Left)));
        assert!(!app.debug.starting() && app.debug.building.is_none());
        assert!(dies(sleeper).await, "the build's process group survived");
        assert_eq!(last(&app), (Level::Info, "Debug start cancelled".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_start_during_a_build_replaces_it() {
        let (mut app, dir) = test_app("build-replace");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &build_toml(SLEEPER, ARTIFACT_LAUNCH));
        let sleeper = building(&mut app).await;
        let first = app.debug_panel.session().unwrap();
        let exe = app.workspace.root.join("bin/second");
        write_artifacts(&app, &[&exe]);
        write_run_toml(&app, &build_toml(r#"["cat", "one.jsonl"]"#, ARTIFACT_LAUNCH));
        shift_f9(&mut app);
        console_until(&mut app, |a| a.debug.session.is_some()).await;
        assert!(dies(sleeper).await, "the first build survived");
        let mut fake = fakes.try_recv().expect("an adapter");
        assert!(fakes.try_recv().is_err(), "exactly one adapter");
        assert_eq!(fake.recv("launch").await["arguments"]["program"], json!(exe.to_string_lossy()));
        assert!(console(&app).iter().all(|l| !l.contains("building")), "{:?}", console(&app));
        let (generation, n) = (app.workspace.generation, console(&app).len());
        app.debug_output(generation, first, vec![RunLine::plain("late from the first build")]);
        assert_eq!(console(&app).len(), n);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_built_is_dropped() {
        let (mut app, dir) = test_app("build-stale-built");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &build_toml(SLEEPER, ARTIFACT_LAUNCH));
        let _sleeper = building(&mut app).await;
        let (generation, seq) = (app.workspace.generation, app.debug.seq);
        let n = console(&app).len();
        for (g, q) in [(generation, seq - 1), (generation - 1, seq)] {
            app.debug_built(g, q, RunEnd::Code(1), Vec::new());
            assert!(app.debug.building.is_some() && app.debug.starting(), "generation {g}, seq {q}");
            assert_eq!(console(&app).len(), n);
            assert_eq!(title(&app), " Debug: demo · building ");
        }
        app.debug.building = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_root_switch_kills_the_build() {
        let (mut app, dir) = test_app("build-root-switch");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &build_toml(SLEEPER, ARTIFACT_LAUNCH));
        let sleeper = building(&mut app).await;
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(dies(sleeper).await, "the build survived the root switch");
        assert!(app.debug.building.is_none() && !app.debug.starting());
        assert_eq!(app.debug_panel.session(), None);
        // An open artifact dialog belongs to the old root.
        let (mut app, dir2, _fakes, _) = choosing("build-root-switch-dialog").await;
        let other = dir2.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(app.dialog.is_none() && app.debug.choice.is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[tokio::test]
    async fn quitting_kills_the_build() {
        let (mut app, dir) = test_app("build-quit");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        write_run_toml(&app, &build_toml(SLEEPER, ARTIFACT_LAUNCH));
        let sleeper = building(&mut app).await;
        app.debug_finish().await;
        assert!(dies(sleeper).await, "the build survived the quit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_line_break_in_the_artifact_path_is_refused() {
        let (mut app, dir) = test_app("build-newline");
        let mut fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        let refusal = "Cannot debug 'demo': line break in the path of its executable";
        // Whichever string of `launch` the path reaches, and either kind of line break.
        for (path, launch) in [
            ("/tmp/a\nshell touch x", ARTIFACT_LAUNCH),
            ("/tmp/a\rshell touch x", ARTIFACT_LAUNCH),
            ("/tmp/a\nshell touch x", r#"program = "x", cwd = "${artifact}""#),
            ("/tmp/a\nshell touch x", r#"program = "x", args = ["${artifact}"]"#),
        ] {
            write_artifacts(&app, &[Path::new(path)]);
            write_run_toml(&app, &build_toml(r#"["cat", "one.jsonl"]"#, launch));
            shift_f9(&mut app);
            console_until(&mut app, |a| !a.debug.starting()).await;
            assert!(fakes.try_recv().is_err(), "{path:?} in {launch}: an adapter started");
            assert!(!app.debug.active(), "{path:?} in {launch}");
            assert_eq!(last(&app), (Level::Warn, refusal.to_string()), "{path:?} in {launch}");
            assert_eq!(title(&app), " Debug: demo · not started ", "{path:?} in {launch}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_adapter_that_cannot_start_after_a_build_is_not_started() {
        let (mut app, dir) = test_app("build-cannot-start");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        app.debug.connector = Connector::Fake(Box::new(|| Err(std::io::Error::other("boom"))));
        write_run_toml(&app, &build_toml(r#"["echo", "built"]"#, r#"program = "/p/x""#));
        shift_f9(&mut app);
        console_until(&mut app, |a| !a.debug.starting()).await;
        assert_eq!(last(&app), (Level::Error, "Debug adapter 'fake' could not start: boom".to_string()));
        assert!(!app.debug.active());
        assert_eq!(console(&app), ["$ echo built", "built"], "the build's Console stays");
        assert_eq!(title(&app), " Debug: demo · not started ", "the program never ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prepare_gives_a_detected_cargo_config_the_default() {
        use crate::run::{Origin, TaskCommand};
        let dir = std::env::temp_dir().join(format!("jetl-test-prepare-cargo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".jetl")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
        let mut debug = DebugConfig::default();
        let o = AdapterOverride { command: Some(vec!["sh".into()]), languages: Some(vec!["rust".into()]), launch: None };
        debug.adapters.insert("fake".into(), o);
        let job = |selected: Option<&str>, blocks: BTreeMap<String, DebugBlock>, tasks: Vec<RunTask>| Job {
            root: dir.clone(),
            debug: debug.clone(),
            tasks,
            blocks,
            selected: selected.map(String::from),
            language: None,
            file: None,
        };
        let argv = |p: &Prepared| p.build.as_ref().map(|b| b.argv.join(" "));
        let none = prepare(job(None, BTreeMap::new(), Vec::new())).unwrap();
        assert_eq!(none.name, "cargo build");
        assert_eq!(argv(&none).as_deref(), Some("cargo build --message-format=json"));
        assert_eq!(none.launch["program"], "${artifact}");
        assert_eq!(none.build.as_ref().unwrap().cwd, dir);
        let test = prepare(job(Some("cargo test"), BTreeMap::new(), Vec::new())).unwrap();
        assert_eq!(argv(&test).as_deref(), Some("cargo test --no-run --message-format=json"));
        let run = prepare(job(Some("cargo run"), BTreeMap::new(), Vec::new())).unwrap();
        assert_eq!(argv(&run).as_deref(), Some("cargo build --message-format=json"));
        // A written block wins, whatever the order of the tasks.
        let written =
            DebugBlock { adapter: Some("fake".into()), language: Some("rust".into()), build: None, launch: json!({"program": "/p/mine"}) };
        let blocks = BTreeMap::from([("cargo run".to_string(), written)]);
        let wins = prepare(job(None, blocks.clone(), Vec::new())).unwrap();
        assert_eq!((wins.name.as_str(), wins.launch["program"].as_str(), argv(&wins)), ("cargo run", Some("/p/mine"), None));
        let selected = prepare(job(Some("cargo run"), blocks, Vec::new())).unwrap();
        assert_eq!(argv(&selected), None);
        // Neither a task that is not Cargo nor a written Cargo task has a default.
        let make = prepare(job(Some("make"), BTreeMap::new(), Vec::new())).unwrap_err();
        assert_eq!(make, "Configuration 'make' has no [debug.\"make\"] table in .jetl/run.toml");
        let task = RunTask {
            name: "cargo build".into(),
            command: TaskCommand::Argv(vec!["cargo".into(), "build".into()]),
            cwd: None,
            env: BTreeMap::new(),
            args: Vec::new(),
            matcher: None,
            origin: Origin::Config,
        };
        let configured = prepare(job(Some("cargo build"), BTreeMap::new(), vec![task])).unwrap_err();
        assert_eq!(configured, "Configuration 'cargo build' has no [debug.\"cargo build\"] table in .jetl/run.toml");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_first_start_with_a_build_shows_the_window() {
        let (mut app, dir) = test_app("build-window");
        let _fakes = with_fake_adapters(&mut app, caps(), Order::LaunchDeferred);
        app.focus.set(PanelId::Editor);
        assert!(!app.layout.is_visible(ToolWindowId::Debug));
        write_run_toml(&app, &build_toml(SLEEPER, ARTIFACT_LAUNCH));
        let _sleeper = building(&mut app).await;
        assert!(app.layout.is_visible(ToolWindowId::Debug));
        assert!(app.focus.is(PanelId::Editor), "the window takes no focus");
        ctrl_f2(&mut app);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
