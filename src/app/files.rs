//! File operations from the Project tree: run off the main thread, then
//! the tree, tabs and git status follow the outcome.

use std::path::Path;

use super::App;
use crate::event::{AppMsg, Level};
use crate::files::ops::FsOp;

impl App {
    pub(super) fn run_fs_op(&mut self, op: FsOp) {
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        // A delete waits for the local history snapshot of what it removes.
        let pre = if let FsOp::Delete { path } = &op { self.history.before_delete(path) } else { None };
        tokio::task::spawn_blocking(move || {
            if let Some(ack) = pre {
                ack.wait();
            }
            let result = op.run().map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::FsOpDone { generation, op, result });
        });
    }

    pub(super) fn fs_op_done(&mut self, op: FsOp, result: Result<(), String>) {
        let what = op.describe(&self.workspace.root);
        if let Err(e) = result {
            return self.notify(Level::Error, format!("Cannot {what}: {e}"));
        }
        match &op {
            FsOp::Create { path, is_dir } => {
                self.reload_parents(path);
                self.project.select_path(path);
                if !is_dir {
                    self.open_file(path.clone(), true);
                }
            }
            FsOp::Rename { from, to } => {
                // Before the tabs follow: snapshots of the old path queued until now are carried.
                self.history_renamed(from, to);
                self.reload_parents(from);
                self.reload_parents(to);
                self.project.select_path(to);
                self.editor.retarget(from, to, &self.theme);
                let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.notify(Level::Info, format!("Renamed to {name}"));
            }
            FsOp::Delete { path } => {
                self.reload_parents(path);
                let closed = self.editor.close_under(path);
                let tabs = match closed {
                    0 => String::new(),
                    1 => " (closed 1 tab)".into(),
                    n => format!(" (closed {n} tabs)"),
                };
                let rel = path.strip_prefix(&self.workspace.root).unwrap_or(path).display().to_string();
                self.notify(Level::Info, format!("Deleted {rel}{tabs}"));
            }
        }
        self.git.refresh_status();
    }

    /// Re-read the tree directories from the root down to `path`'s parent,
    /// so folders created on the way show too.
    pub(super) fn reload_parents(&mut self, path: &Path) {
        let root = self.workspace.root.clone();
        let mut dirs: Vec<&Path> = path.ancestors().skip(1).take_while(|d| d.starts_with(&root)).collect();
        dirs.reverse();
        for d in dirs {
            self.project.reload_dir(d);
        }
    }
}
