//! Debug Adapter Protocol client. `protocol` holds the wire types, `session` the pure state machine that
//! decides every ordering rule of a session (no IO, no clock of its own). `client` is the driver that owns
//! the process and the clock and feeds the machine; `fake`, tests only, is an in-process adapter speaking each
//! start order real adapters use. `config` is the adapter table and the debug blocks that choose and launch one.

pub mod client;
pub mod config;
#[cfg(test)]
pub mod fake;
pub mod protocol;
pub mod session;

use std::path::PathBuf;

use serde_json::Value;

use protocol::{Breakpoint, BreakpointEvent, Capabilities, OutputEvent, StoppedEvent};
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
    Exited(i64),
    /// Always last; empty for a normal end.
    Ended {
        reason: String,
    },
}
