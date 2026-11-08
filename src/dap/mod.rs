//! Debug Adapter Protocol client. `protocol` holds the wire types, `session` the pure state machine that
//! decides every ordering rule of a session (no IO, no clock of its own). `client` is the driver that owns
//! the process and the clock and feeds the machine; `fake`, tests only, is an in-process adapter speaking each
//! start order real adapters use. `config` is the adapter table and the debug blocks that choose and launch one. `console`
//! turns the program's `output` events into Console lines on a thread of its own; `panel` is the Debug tool window, `vars` the model of its Variables tab and `disasm` that of its Disassembly tab. `procs` lists the processes Attach to Process offers and words a refused attach. `build` reads the output of a Debug build step and the executables it names. `launch_json` reads `.vscode/launch.json` into run configurations and debug blocks.

pub(crate) mod build;
pub mod client;
pub mod config;
pub(crate) mod console;
pub mod disasm;
#[cfg(test)]
pub mod fake;
pub mod launch_json;
pub mod panel;
pub(crate) mod procs;
pub mod protocol;
pub mod session;
pub mod vars;

use std::path::PathBuf;

use serde_json::Value;

use protocol::{Breakpoint, BreakpointEvent, Capabilities, OutputEvent, ProcessEvent, StoppedEvent};
use session::{DapTag, Phase, SessionId};

/// One report of a session to the App. The App drops it when `generation` or `session` is not the current one.
#[derive(Debug, Clone, PartialEq)]
pub struct DapMsg {
    pub generation: u64,
    pub session: SessionId,
    pub body: DapBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DapBody {
    Started {
        capabilities: Box<Capabilities>,
    },
    /// Every transition.
    Phase(Phase),
    Breakpoints {
        source: PathBuf,
        requested: Vec<usize>,
        result: Vec<Breakpoint>,
    },
    BreakpointChanged(BreakpointEvent),
    Stopped {
        epoch: u64,
        event: StoppedEvent,
    },
    Running {
        epoch: u64,
    },
    /// Never `telemetry`.
    Output(OutputEvent),
    Response {
        tag: DapTag,
        result: Result<Value, String>,
    },
    /// A failed launch or configuration step.
    Error(String),
    /// Something the user should know that is not a failure: an attach's Stop press that waits.
    Info(String),
    /// A stop that may have cost the user's process something.
    Warning(String),
    Exited(i64),
    /// The adapter has a program: it can arrive just after an empty threads answer.
    Process(ProcessEvent),
    /// Always last; empty for a normal end.
    Ended {
        reason: String,
    },
}
