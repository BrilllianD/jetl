//! Messages flowing from background services into the app loop.

use std::path::PathBuf;

use crate::ui::focus::PanelId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug)]
pub enum AppMsg {
    /// A file read for a new tab, tagged with the workspace generation at
    /// the time of the request so loads from a previous root are dropped.
    FileLoaded {
        generation: u64,
        path: PathBuf,
        result: std::io::Result<Vec<u8>>,
        focus: bool,
    },
    Git(crate::git::GitMsg),
    /// `git worktree list`, tagged with the workspace generation like `GitMsg`.
    Worktrees {
        generation: u64,
        result: anyhow::Result<Vec<crate::worktree::Worktree>>,
    },
    /// A saved session and its files (`files[i]` belongs to
    /// `session.files[i]`), tagged with the workspace generation.
    SessionLoaded {
        generation: u64,
        session: crate::workspace::Session,
        files: Vec<std::io::Result<Vec<u8>>>,
    },
    /// The palette's file walk of the root, tagged with the workspace
    /// generation and its request order.
    FileIndex {
        generation: u64,
        seq: u64,
        files: Vec<String>,
    },
    /// Reserved for the language server integration.
    Lsp(LspMsg),
}

#[derive(Debug)]
pub enum LspMsg {}

/// Side effects a panel asks the app to perform after handling input.
#[derive(Debug)]
pub enum Effect {
    OpenFile(PathBuf),
    Focus(PanelId),
    Notify(Level, String),
    SwitchRoot(PathBuf),
    GitRefresh,
    /// A git operation or query for the app to run through `GitService`.
    Git(crate::git::GitRequest),
    Dialog(crate::ui::dialog::Dialog),
    /// `quiet` (autosave) skips the "Saved" toast.
    FileSaved {
        path: PathBuf,
        quiet: bool,
    },
    Clipboard(String),
    Quit,
}

/// A mouse event with click count already resolved by the app.
#[derive(Debug, Clone, Copy)]
pub struct Mouse {
    pub kind: crossterm::event::MouseEventKind,
    pub x: u16,
    pub y: u16,
    /// 1 = single, 2 = double, 3 = triple (only for button presses).
    pub clicks: u8,
    pub mods: crossterm::event::KeyModifiers,
}
