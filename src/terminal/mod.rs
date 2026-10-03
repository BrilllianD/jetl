//! The Terminal tool window (Alt+F12): one shell on a pseudo terminal
//! (`pty`), its output parsed by `vt100` and drawn cell by cell (`panel`),
//! keys, mouse and paste encoded the way xterm sends them (`keys`).

pub mod keys;
pub mod panel;
pub mod pty;

use serde::Deserialize;

/// `[terminal]` in config.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// The program to run; `$SHELL`, then `/bin/sh`, when empty.
    pub shell: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Lines kept above the screen for scrolling back.
    pub scrollback: usize,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { shell: String::new(), args: Vec::new(), scrollback: 5000 }
    }
}

impl TerminalConfig {
    /// The shell's argv: the configured one, else `$SHELL`, else `/bin/sh`.
    pub fn argv(&self) -> Vec<String> {
        let shell = Some(self.shell.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "/bin/sh".into());
        std::iter::once(shell).chain(self.args.iter().cloned()).collect()
    }
}
