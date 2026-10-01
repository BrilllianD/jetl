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
    Notify { level: Level, text: String },
    FileLoaded { path: PathBuf, result: std::io::Result<Vec<u8>>, focus: bool },
    FileSaved { path: PathBuf, result: std::io::Result<()> },
    Git(crate::git::GitMsg),
    Worktrees(anyhow::Result<Vec<crate::worktree::Worktree>>),
    SwitchRoot(PathBuf),
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
