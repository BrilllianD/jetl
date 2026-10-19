//! File operations from the Project tree: run off the main thread, then
//! the tree, tabs and git status follow the outcome. Language servers
//! interested in the path get `workspace/will*Files` first (their edits are
//! applied before the operation) and `did*Files` after it.

use std::collections::VecDeque;
use std::path::Path;

use lsp_types::WorkspaceEdit;
use serde_json::{Value, json};

use super::App;
use crate::event::{AppMsg, Level};
use crate::files::ops::FsOp;
use crate::lsp::caps::{
    Caps, DID_CREATE_FILES, DID_DELETE_FILES, DID_RENAME_FILES, WILL_CREATE_FILES, WILL_DELETE_FILES, WILL_RENAME_FILES,
};
use crate::lsp::transport::ResponseError;
use crate::lsp::{EditOrigin, PendingTag, SCRATCH_SUFFIX, sync};

/// A tree file operation waiting for the servers' `will*Files` answers, then applying their edits.
pub(super) struct PendingFileOp {
    pub(super) seq: u64,
    pub(super) op: FsOp,
    pub(super) is_dir: bool,
    /// Servers (client names) that have not answered yet.
    waiting: Vec<String>,
    /// Their edits with the server's name, in answer order; applied front first.
    edits: VecDeque<(String, WorkspaceEdit)>,
    /// An edit's disk steps are being written (`EditsApplied` pending).
    writing: bool,
}

/// The (`will`, `did`) methods of an operation.
fn file_op_methods(op: &FsOp) -> (&'static str, &'static str) {
    match op {
        FsOp::Create { .. } => (WILL_CREATE_FILES, DID_CREATE_FILES),
        FsOp::Rename { .. } => (WILL_RENAME_FILES, DID_RENAME_FILES),
        FsOp::Delete { .. } => (WILL_DELETE_FILES, DID_DELETE_FILES),
    }
}

/// Params of `will*Files` and `did*Files`; `None` when a path has no URI.
fn file_op_params(op: &FsOp) -> Option<Value> {
    let uri = |p: &Path| sync::uri(p).map(|u| u.to_string());
    Some(match op {
        FsOp::Create { path, .. } | FsOp::Delete { path, .. } => json!({"files": [{"uri": uri(path)?}]}),
        FsOp::Rename { from, to, .. } => json!({"files": [{"oldUri": uri(from)?, "newUri": uri(to)?}]}),
    })
}

impl App {
    pub(super) fn run_fs_op(&mut self, op: FsOp) {
        let is_dir = match &op {
            FsOp::Create { is_dir, .. } => *is_dir,
            FsOp::Rename { from: p, .. } | FsOp::Delete { path: p, .. } => p.symlink_metadata().is_ok_and(|m| m.is_dir()),
        };
        // The servers' versions of open documents must be current for their edits.
        self.lsp_sync();
        let (will, _) = file_op_methods(&op);
        let servers = self.file_op_servers(will, &op, is_dir);
        let Some(params) = file_op_params(&op).filter(|_| !servers.is_empty()) else {
            return self.run_fs_op_now(op, is_dir);
        };
        self.lsp.last_seq += 1;
        let seq = self.lsp.last_seq;
        for name in &servers {
            if let Some(c) = self.lsp.clients.get_mut(name) {
                c.request(will, params.clone(), PendingTag::WillFileOp(seq));
            }
        }
        self.lsp.file_ops.push(PendingFileOp { seq, op, is_dir, waiting: servers, edits: VecDeque::new(), writing: false });
    }

    /// Names of the running, non-scratch servers (sorted) that want `method` for the op's path.
    fn file_op_servers(&self, method: &'static str, op: &FsOp, is_dir: bool) -> Vec<String> {
        let paths: Vec<&Path> = match op {
            FsOp::Create { path, .. } | FsOp::Delete { path, .. } => vec![path],
            FsOp::Rename { from, to, .. } => vec![from, to],
        };
        let mut names: Vec<String> = self
            .lsp
            .clients
            .iter()
            .filter(|(name, _)| !name.ends_with(SCRATCH_SUFFIX))
            .filter(|(_, c)| {
                c.capabilities().is_some_and(|fixed| {
                    let caps = Caps { fixed, registry: &c.registry, doc: None };
                    paths.iter().any(|p| caps.file_operation(method, p, is_dir))
                })
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Run the operation off the main thread; `FsOpDone` follows.
    pub(super) fn run_fs_op_now(&mut self, op: FsOp, is_dir: bool) {
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        // A delete waits for the local history snapshot of what it removes.
        let pre = if let FsOp::Delete { path, .. } = &op { self.history.before_delete(path) } else { None };
        tokio::task::spawn_blocking(move || {
            if let Some(ack) = pre {
                ack.wait();
            }
            let result = op.run().map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::FsOpDone { generation, op, is_dir, result });
        });
    }

    /// A server's answer to `will*Files`. A timeout arrives here as an error.
    pub(super) fn will_file_op_answer(&mut self, name: &str, seq: u64, result: Result<Value, ResponseError>) {
        let Some(pending) = self.lsp.file_ops.iter_mut().find(|p| p.seq == seq) else {
            return tracing::debug!("dropping a will*Files answer of {name} for a finished operation");
        };
        pending.waiting.retain(|w| w != name);
        match result {
            Ok(Value::Null) => {}
            Ok(v) => match crate::lsp::edits::read_workspace_edit(v) {
                Ok(edit) => pending.edits.push_back((name.to_string(), edit)),
                Err(e) => tracing::debug!("{name}: unreadable will*Files edit: {e}"),
            },
            Err(e) => tracing::debug!("{name}: will*Files failed: {e:?}"),
        }
        if pending.waiting.is_empty() {
            self.file_op_continue(seq);
        }
    }

    /// Apply the next server edit of the pending operation `seq`, or run the operation when none is left.
    /// Edits call back here once applied or refused.
    pub(super) fn file_op_continue(&mut self, seq: u64) {
        let Some(i) = self.lsp.file_ops.iter().position(|p| p.seq == seq) else { return };
        self.lsp.file_ops[i].writing = false;
        // State first: `apply_workspace_edit` may call back synchronously.
        let Some((server, edit)) = self.lsp.file_ops[i].edits.pop_front() else {
            let PendingFileOp { op, is_dir, .. } = self.lsp.file_ops.remove(i);
            return self.run_fs_op_now(op, is_dir);
        };
        let what = self.lsp.file_ops[i].op.describe(&self.workspace.root);
        let mut chars = what.chars();
        let what = chars.next().map(|c| c.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default();
        self.apply_workspace_edit(edit, EditOrigin::FileOp { seq, title: format!("{what} ({server})") });
    }

    /// The edit of operation `seq` is being written on a blocking thread.
    pub(super) fn mark_file_op_writing(&mut self, seq: u64) {
        if let Some(p) = self.lsp.file_ops.iter_mut().find(|p| p.seq == seq) {
            p.writing = true;
        }
    }

    /// The servers restart: operations still waiting for answers run now; one whose edit is being written waits
    /// for it (returned, its other servers' edits dropped), so the rename or delete cannot race the
    /// write.
    pub(super) fn file_ops_for_restart(&mut self) -> Vec<PendingFileOp> {
        let mut keep = Vec::new();
        for mut p in std::mem::take(&mut self.lsp.file_ops) {
            if p.writing {
                p.waiting.clear();
                p.edits.clear();
                keep.push(p);
            } else {
                self.run_fs_op_now(p.op, p.is_dir);
            }
        }
        keep
    }

    /// A server went away: it is not waited for any more.
    pub(super) fn file_ops_server_gone(&mut self, name: &str) {
        let mut ready = Vec::new();
        for p in &mut self.lsp.file_ops {
            let before = p.waiting.len();
            p.waiting.retain(|w| w != name);
            if p.waiting.is_empty() && before > 0 {
                ready.push(p.seq);
            }
        }
        for seq in ready {
            self.file_op_continue(seq);
        }
    }

    /// `did*Files` for the servers that want it.
    fn lsp_did_file_op(&self, op: &FsOp, is_dir: bool) {
        let (_, did) = file_op_methods(op);
        let Some(params) = file_op_params(op) else { return };
        for name in self.file_op_servers(did, op, is_dir) {
            if let Some(c) = self.lsp.clients.get(&name) {
                c.notify(did, params.clone());
            }
        }
    }

    pub(super) fn fs_op_done(&mut self, op: FsOp, is_dir: bool, result: Result<(), String>) {
        let what = op.describe(&self.workspace.root);
        if let Err(e) = result {
            return self.notify(Level::Error, format!("Cannot {what}: {e}"));
        }
        self.lsp_did_file_op(&op, is_dir);
        match &op {
            FsOp::Create { path, is_dir, .. } => {
                self.reload_parents(path);
                self.project.select_path(path);
                if !is_dir {
                    self.open_file(path.clone(), true);
                }
            }
            FsOp::Rename { from, to, .. } => {
                // Before the tabs follow: snapshots of the old path queued until now are carried.
                self.history_renamed(from, to);
                self.reload_parents(from);
                self.reload_parents(to);
                self.project.select_path(to);
                self.editor.retarget(from, to, &self.theme);
                let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.notify(Level::Info, format!("Renamed to {name}"));
            }
            FsOp::Delete { path, .. } => {
                self.reload_parents(path);
                // Its rows read deleted (a file unchanged since its last
                // snapshot stored nothing new).
                self.history_files_changed(path);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, sent, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::LspMsg;

    fn fs_op_done(m: &AppMsg) -> bool {
        matches!(m, AppMsg::FsOpDone { .. })
    }

    /// `incremental()` plus `.rs` file renames (before and after) and `.rs` creations (after).
    fn caps() -> Value {
        let mut caps = incremental();
        let filters = json!([{"scheme": "file", "pattern": {"glob": "**/*.rs", "matches": "file"}}]);
        caps["workspace"] = json!({"fileOperations": {
            "willRename": {"filters": filters},
            "didRename": {"filters": filters},
            "didCreate": {"filters": [{"pattern": {"glob": "**/*.rs"}}]},
        }});
        caps
    }

    fn rename(from: &Path, to: &Path) -> FsOp {
        FsOp::Rename { from: from.to_path_buf(), to: to.to_path_buf(), overwrite: false }
    }

    #[tokio::test]
    async fn a_tree_rename_applies_the_servers_edit_first() {
        let (mut app, dir) = test_app("fileop-rename");
        let root = app.workspace.root.clone();
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let (lib, m, n) = (root.join("lib.rs"), root.join("m.rs"), root.join("n.rs"));
        std::fs::write(&lib, "mod m;\n").unwrap();
        std::fs::write(&m, "").unwrap();
        app.run_fs_op(rename(&m, &n));
        let req = server.recv("workspace/willRenameFiles").await;
        let (old, new) = (sync::uri(&m).unwrap().to_string(), sync::uri(&n).unwrap().to_string());
        let files = json!([{"oldUri": old, "newUri": new}]);
        assert_eq!(req["params"]["files"], files);
        let edit = json!({"changes": {sync::uri(&lib).unwrap().to_string(): [
            {"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}}, "newText": "n"}
        ]}});
        server.respond(req["id"].as_i64().unwrap(), edit);
        handle_next(&mut app, is_lsp).await;
        handle_next(&mut app, |m| matches!(m, AppMsg::EditsApplied { .. })).await;
        handle_next(&mut app, fs_op_done).await;
        assert!(n.exists() && !m.exists());
        assert_eq!(std::fs::read_to_string(&lib).unwrap(), "mod n;\n");
        assert_eq!(server.recv("workspace/didRenameFiles").await["params"]["files"], files);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_interested_server_runs_the_operation_at_once() {
        let (mut app, dir) = test_app("fileop-none");
        let root = app.workspace.root.clone();
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let (notes, todo) = (root.join("notes.txt"), root.join("todo.txt"));
        std::fs::write(&notes, "x").unwrap();
        app.run_fs_op(rename(&notes, &todo));
        handle_next(&mut app, fs_op_done).await;
        assert!(todo.exists() && !notes.exists());
        let b = root.join("b.rs");
        app.run_fs_op(FsOp::Create { path: b.clone(), is_dir: false, overwrite: false });
        handle_next(&mut app, fs_op_done).await;
        let sent = sent(&app, &mut server).await;
        let created = sent.iter().find(|m| m["method"] == "workspace/didCreateFiles").expect("didCreateFiles");
        assert_eq!(created["params"]["files"], json!([{"uri": sync::uri(&b).unwrap().to_string()}]));
        let methods: Vec<&str> = sent.iter().filter_map(|m| m["method"].as_str()).collect();
        assert!(!methods.iter().any(|m| m.starts_with("workspace/will") || *m == "workspace/didRenameFiles"), "{methods:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_error_or_a_gone_server_does_not_block_the_operation() {
        let (mut app, dir) = test_app("fileop-error");
        let root = app.workspace.root.clone();
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let (m, n, o) = (root.join("m.rs"), root.join("n.rs"), root.join("o.rs"));
        std::fs::write(&m, "").unwrap();
        app.run_fs_op(rename(&m, &n));
        let req = server.recv("workspace/willRenameFiles").await;
        server.respond_error(req["id"].as_i64().unwrap(), -32603, "x");
        handle_next(&mut app, is_lsp).await;
        handle_next(&mut app, fs_op_done).await;
        assert!(n.exists() && !m.exists());
        app.run_fs_op(rename(&n, &o));
        server.recv("workspace/willRenameFiles").await;
        let (generation, id) = (app.workspace.generation, app.lsp.clients["rust-analyzer"].id);
        app.handle_msg(AppMsg::Lsp(LspMsg::Exited { generation, server: id, status: "test".into() }));
        handle_next(&mut app, fs_op_done).await;
        assert!(o.exists() && !n.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_confirmation_lost_behind_another_dialog_still_lets_the_operation_run() {
        use crate::ui::dialog::{Dialog, DialogId};
        let (mut app, dir) = test_app("fileop-orphaned-confirm");
        let root = app.workspace.root.clone();
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let (lib, m, n) = (root.join("lib.rs"), root.join("m.rs"), root.join("n.rs"));
        std::fs::write(&lib, "mod m;\n").unwrap();
        std::fs::write(&m, "").unwrap();
        app.run_fs_op(rename(&m, &n));
        let req = server.recv("workspace/willRenameFiles").await;
        let edit = json!({
            "documentChanges": [{
                "textDocument": {"uri": sync::uri(&lib).unwrap().to_string(), "version": null},
                "edits": [{"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}},
                           "newText": "n", "annotationId": "x"}]
            }],
            "changeAnnotations": {"x": {"label": "Risky", "needsConfirmation": true}}
        });
        server.respond(req["id"].as_i64().unwrap(), edit);
        handle_next(&mut app, is_lsp).await;
        assert!(app.lsp.edit.confirm.is_some());
        assert!(matches!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::LspConfirmEdit(_))));
        // A harmless message leaves a showing confirmation parked.
        app.handle_msg(AppMsg::Lsp(LspMsg::Exited { generation: app.workspace.generation + 1, server: 0, status: "x".into() }));
        assert!(app.lsp.edit.confirm.is_some());
        // Another dialog takes its place; the next message refuses the edit and the rename goes on.
        app.dialog = Some(Dialog::info("Other", "x"));
        app.handle_msg(AppMsg::Lsp(LspMsg::Exited { generation: app.workspace.generation + 1, server: 0, status: "x".into() }));
        assert!(app.lsp.edit.confirm.is_none());
        handle_next(&mut app, fs_op_done).await;
        assert!(n.exists() && !m.exists());
        assert_eq!(std::fs::read_to_string(&lib).unwrap(), "mod m;\n");
        server.recv("workspace/didRenameFiles").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_restart_waits_for_a_file_op_edit_being_written() {
        let (mut app, dir) = test_app("fileop-restart-writing");
        let root = app.workspace.root.clone();
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let (lib, m, n) = (root.join("lib.rs"), root.join("m.rs"), root.join("n.rs"));
        std::fs::write(&lib, "mod m;\n").unwrap();
        std::fs::write(&m, "").unwrap();
        app.run_fs_op(rename(&m, &n));
        let req = server.recv("workspace/willRenameFiles").await;
        let edit = json!({"changes": {sync::uri(&lib).unwrap().to_string(): [
            {"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}}, "newText": "n"}
        ]}});
        server.respond(req["id"].as_i64().unwrap(), edit);
        handle_next(&mut app, is_lsp).await;
        // The edit of lib.rs is being written; the restart must not run the rename yet.
        app.lsp_restart();
        assert_eq!(app.lsp.file_ops.len(), 1);
        handle_next(&mut app, |m| matches!(m, AppMsg::EditsApplied { .. })).await;
        handle_next(&mut app, fs_op_done).await;
        assert!(n.exists() && !m.exists());
        assert_eq!(std::fs::read_to_string(&lib).unwrap(), "mod n;\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_confirmation_replaced_on_a_tick_is_declined_at_once() {
        use crate::ui::dialog::{Dialog, DialogId};
        let (mut app, dir) = test_app("fileop-orphaned-tick");
        let root = app.workspace.root.clone();
        let mut servers = with_fake_servers(&mut app, caps());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn main() {}\n").await;
        let (lib, m, n) = (root.join("lib.rs"), root.join("m.rs"), root.join("n.rs"));
        std::fs::write(&lib, "mod m;\n").unwrap();
        std::fs::write(&m, "").unwrap();
        app.run_fs_op(rename(&m, &n));
        let req = server.recv("workspace/willRenameFiles").await;
        let edit = json!({
            "documentChanges": [{
                "textDocument": {"uri": sync::uri(&lib).unwrap().to_string(), "version": null},
                "edits": [{"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}},
                           "newText": "n", "annotationId": "x"}]
            }],
            "changeAnnotations": {"x": {"label": "Risky", "needsConfirmation": true}}
        });
        server.respond(req["id"].as_i64().unwrap(), edit);
        handle_next(&mut app, is_lsp).await;
        assert!(matches!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::LspConfirmEdit(_))));
        app.dialog = Some(Dialog::info("Other", "x"));
        app.on_tick();
        assert!(app.lsp.edit.confirm.is_none());
        handle_next(&mut app, fs_op_done).await;
        assert!(n.exists() && !m.exists());
        assert_eq!(std::fs::read_to_string(&lib).unwrap(), "mod m;\n");
        server.recv("workspace/didRenameFiles").await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
