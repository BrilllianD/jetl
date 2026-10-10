//! Changes made outside jetl: starting the watcher and acting on its
//! batches (tree, git state, open buffers).

use std::path::PathBuf;

use super::App;
use crate::editor::DiskChange;
use crate::event::{AppMsg, Level};
use crate::files::watch::{self, Changes, Watcher};

impl App {
    /// (Re)start watching the current root, off the main thread; the
    /// previous watch stops. Without a watcher, focus gain refreshes instead.
    pub fn start_watcher(&mut self) {
        self.watcher = None;
        if !self.config.files.watch {
            return;
        }
        let (root, generation, tx) = (self.workspace.root.clone(), self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let result = watch::start(&root, generation, tx.clone());
            let _ = tx.send(AppMsg::WatcherStarted { generation, result });
        });
    }

    pub(super) fn watcher_started(&mut self, result: anyhow::Result<Watcher>) {
        match result {
            Ok(w) => self.watcher = Some(w),
            Err(e) => self.notify(Level::Warn, format!("File watching unavailable ({e:#}); refreshing when the terminal regains focus")),
        }
    }

    pub(super) fn fs_changed(&mut self, mut changes: Changes) {
        let renames = std::mem::take(&mut changes.renames);
        self.history_moved(renames);
        if changes.git || !changes.paths.is_empty() {
            self.git.refresh_status();
        }
        if changes.git {
            self.git.load_branches();
            self.git.load_worktrees();
        }
        if changes.paths.iter().any(|p| p.parent().is_some_and(|d| self.project.lists(d))) {
            self.refresh_tree();
        }
        self.symbols_changed(&changes.paths);
        for path in changes.paths {
            match self.editor.on_disk_change(&path) {
                DiskChange::None => {}
                DiskChange::Reload => self.read_changed(path),
                DiskChange::Conflict(i) => {
                    // Never replace a dialog or palette the user is working in.
                    if self.dialog.is_none() && self.palette.is_none() {
                        self.dialog = self.editor.conflict_dialog(i);
                    } else {
                        let name = &self.editor.buffers[i].name;
                        self.notify(Level::Warn, format!("{name} changed on disk; saving it will ask what to do"));
                    }
                }
            }
        }
    }

    /// Read a clean tab's file again; `disk_text` swaps it in.
    fn read_changed(&mut self, path: PathBuf) {
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let result = std::fs::read(&path);
            let _ = tx.send(AppMsg::DiskText { generation, path, result });
        });
    }

    pub(super) fn disk_text(&mut self, path: PathBuf, result: std::io::Result<Vec<u8>>) {
        let text = match result.map(String::from_utf8) {
            Ok(Ok(text)) => text,
            Ok(Err(_)) => return tracing::debug!("{} is no longer UTF-8; not reloaded", path.display()),
            Err(e) => return tracing::debug!("reload {}: {e}", path.display()),
        };
        let mut sink = Vec::new();
        if self.editor.reload_clean(&path, text, &mut sink) {
            tracing::info!("reloaded {} after an outside change", path.display());
        }
        for c in sink {
            self.history.capture(c);
        }
    }
}
