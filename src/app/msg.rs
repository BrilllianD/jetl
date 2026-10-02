//! Background results: `AppMsg` dispatch, git results, file loads.

use std::path::PathBuf;

use super::{App, Ctx};
use crate::event::{AppMsg, Level};
use crate::git::{BlobWhich, GitMsg, GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::PanelId;
use crate::ui::palette::Source;

impl App {
    pub(super) fn handle_msg(&mut self, msg: AppMsg) {
        self.dirty_frame = true;
        match msg {
            AppMsg::FileLoaded { generation, path, result, focus } => {
                if generation == self.workspace.generation {
                    self.file_loaded(path, result, focus);
                } else {
                    tracing::debug!("dropping stale load of {}", path.display());
                }
            }
            AppMsg::Git(m) => self.on_git_msg(m),
            AppMsg::Worktrees { generation, result } => {
                if generation == self.workspace.generation {
                    self.worktrees.set_list(result, &self.workspace.root);
                } else {
                    tracing::debug!("dropping stale worktree list");
                }
            }
            AppMsg::SessionLoaded { generation, session, files } => self.session_loaded(generation, session, files),
            AppMsg::TreeLoaded { generation, seq, dirs } => {
                if generation == self.workspace.generation {
                    if let Some(req) = self.project.finish_refresh(seq, dirs) {
                        self.spawn_tree_refresh(req);
                    }
                } else {
                    tracing::debug!("dropping stale tree listing");
                }
            }
            AppMsg::FileIndex { generation, seq, files } => self.file_index_loaded(generation, seq, files),
            AppMsg::Find { generation, batch } if generation == self.workspace.generation => self.find.on_batch(batch),
            AppMsg::ReplaceDone { generation, files, matches, errors } if generation == self.workspace.generation => {
                self.replace_done(files, matches, errors)
            }
            AppMsg::Find { .. } | AppMsg::ReplaceDone { .. } => tracing::debug!("dropping stale find result"),
            // Messages of a previous root are dropped; a stale watcher stops
            // when its message is dropped.
            AppMsg::FsChanged { generation, changes } if generation == self.workspace.generation => self.fs_changed(changes),
            AppMsg::WatcherStarted { generation, result } if generation == self.workspace.generation => self.watcher_started(result),
            AppMsg::DiskText { generation, path, result } if generation == self.workspace.generation => self.disk_text(path, result),
            AppMsg::FsChanged { .. } | AppMsg::WatcherStarted { .. } | AppMsg::DiskText { .. } => {
                tracing::debug!("dropping stale watcher message");
            }
            AppMsg::Lsp(_) => {}
        }
    }

    pub(super) fn on_git_msg(&mut self, msg: GitMsg) {
        if let GitMsg::OpDone { id, .. } = &msg {
            self.git.finish(*id);
        }
        if msg.generation() != self.workspace.generation {
            tracing::debug!("dropping stale git result");
            return;
        }
        match msg {
            GitMsg::Status { seq, result, .. } => {
                self.git.status_done();
                let prev_head = self.git_panel.snapshot().map(|s| s.head.clone());
                let mut cx = ctx!(self);
                let applied = self.git_panel.set_status(seq, result, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
                if !applied {
                    return;
                }
                let snap = self.git_panel.snapshot();
                self.project.set_git_status(snap.map(|s| s.tree.clone()).unwrap_or_default());
                // A new HEAD changes every gutter base.
                if snap.map(|s| s.head.clone()) != prev_head {
                    for p in self.editor.open_paths() {
                        self.git.load_blob(p, BlobWhich::Head);
                    }
                }
            }
            GitMsg::Blob { seq, path, which: BlobWhich::Head, text, .. } => {
                self.editor.set_diff_base(&path, seq, text);
                self.editor.update_gutters();
            }
            GitMsg::Blob { .. } => {}
            GitMsg::Diff { key, result, .. } => self.git_panel.set_diff(&key, result),
            GitMsg::Branches { result, .. } => {
                let worktree = std::mem::take(&mut self.worktrees.pending_existing);
                let palette = self.palette.as_mut().filter(|p| p.wants(Source::Branches));
                match result {
                    Ok(list) => {
                        if let Some(p) = palette {
                            p.set_branches(list.clone());
                        }
                        self.git_panel.branches = list;
                        self.branches_error_shown = false;
                        if worktree {
                            self.open_existing_branch_dialog();
                        }
                    }
                    // The worktree flow asked for this list; an open palette
                    // reloads it on every refresh, so it reports once.
                    Err(e) if worktree || (palette.is_some() && !self.branches_error_shown) => {
                        self.branches_error_shown = true;
                        self.notify(Level::Error, format!("Branches: {e:#}"));
                    }
                    Err(_) => {}
                }
            }
            GitMsg::Log { skip, result, .. } => {
                let mut cx = ctx!(self);
                self.git_panel.set_log(skip, result, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            GitMsg::CommitDetails { sha, result, .. } => self.git_panel.set_details(&sha, result),
            GitMsg::OpDone { op, label, ok, output, .. } => self.git_op_done(op, label, ok, output),
        }
    }

    pub(super) fn git_op_done(&mut self, op: GitOp, label: String, ok: bool, output: String) {
        self.git_panel.log_op(&label, ok, &output);
        if self.worktree_op_done(&op, ok, &output) {
            // Reported (or a follow-up dialog opened) by the worktree flow.
        } else if ok {
            match &op {
                GitOp::Commit { push } => {
                    self.git_panel.message.clear();
                    self.notify(Level::Info, format!("Committed {output}"));
                    if *push {
                        self.git_request(GitRequest::Push);
                    }
                }
                GitOp::Push | GitOp::PushUpstream(_) => self.notify(Level::Info, "Pushed"),
                GitOp::Pull => self.notify(Level::Info, "Pulled"),
                GitOp::Fetch => self.notify(Level::Info, "Fetched"),
                GitOp::Switch(b) => self.notify(Level::Info, format!("Switched to {b}")),
                GitOp::NewBranch(b) => self.notify(Level::Info, format!("Created and switched to {b}")),
                GitOp::Stage | GitOp::Unstage => {}
                GitOp::WorktreeAdd { .. } | GitOp::WorktreeRemove { .. } | GitOp::WorktreePrune => {}
            }
            if op.touches_worktree() {
                let n = self.editor.reload_clean_from_disk();
                if n > 0 {
                    tracing::info!("reloaded {n} buffer(s) after {label}");
                }
            }
        } else if op == GitOp::Push && output.contains("has no upstream branch") {
            let branch = self.branch_label().unwrap_or_default();
            let msg = format!("Branch '{branch}' has no upstream.\nPush it and set the upstream to origin/{branch}?");
            self.dialog = Some(Dialog::confirm(DialogId::PushSetUpstream(branch), "Push", &msg, &["Push", "Cancel"]));
        } else if matches!(op, GitOp::Commit { .. } | GitOp::Stage | GitOp::Unstage) {
            self.notify(Level::Error, error_line(&output));
        } else {
            self.notify(Level::Error, format!("{label}: {}", error_line(&output)));
        }
        self.refresh_tree();
        self.git.refresh_status();
        self.git.load_branches();
        // Branch switches and worktree operations both change the list.
        self.git.load_worktrees();
        if self.git_panel.log_loaded() && !matches!(op, GitOp::Stage | GitOp::Unstage) {
            let mut cx = ctx!(self);
            self.git_panel.request_log(&mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
        }
    }

    /// Point git at a new project root (worktree switch): bump the
    /// generation so in-flight results are dropped, forget the old state,
    /// and load the new one.
    pub fn git_root_changed(&mut self, root: PathBuf) {
        self.git.set_root(root, self.workspace.generation);
        self.git_panel.reset();
        self.project.set_git_status(Default::default());
        for b in &mut self.editor.buffers {
            b.diff_base = None;
            b.gutter.clear();
        }
        self.git.refresh_status();
        self.git.load_branches();
        for p in self.editor.open_paths() {
            self.git.load_blob(p, BlobWhich::Head);
        }
        if self.git_panel.tab == crate::git::panel::GitTab::Log {
            let mut cx = ctx!(self);
            self.git_panel.request_log(&mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
        }
    }

    /// Open a file with the caret at a 0-based line and char column.
    pub fn open_file_at(&mut self, path: PathBuf, line: usize, col: usize) {
        if self.editor.index_of(&path).is_some() {
            self.open_file(path, true);
            self.editor.goto_line(line + 1, col + 1);
        } else {
            self.pending_goto = Some((path.clone(), line, col));
            self.open_file(path, true);
        }
    }

    /// Open a file: reuse the tab when already open, otherwise read it off
    /// the main loop and finish in `file_loaded`.
    pub fn open_file(&mut self, path: PathBuf, focus: bool) {
        if let Some(i) = self.editor.index_of(&path) {
            let mut cx = ctx!(self);
            self.editor.activate(i, &mut cx);
            if focus {
                self.focus.set(PanelId::Editor);
            }
            return;
        }
        let (tx, generation) = (self.bus_tx.clone(), self.workspace.generation);
        tokio::task::spawn_blocking(move || {
            let result = std::fs::read(&path);
            let _ = tx.send(AppMsg::FileLoaded { generation, path, result, focus });
        });
    }

    pub(super) fn file_loaded(&mut self, path: PathBuf, result: std::io::Result<Vec<u8>>, focus: bool) {
        let bytes = match result {
            Ok(b) => b,
            Err(e) => {
                self.notify(Level::Error, format!("open {}: {e}", path.display()));
                return;
            }
        };
        let Some((text, lossy)) = self.decode_file(&path, bytes) else { return };
        self.editor.open_text(path.clone(), &text, lossy, &self.theme);
        if let Some((_, line, col)) = self.pending_goto.take_if(|(p, ..)| *p == path) {
            self.editor.goto_line(line + 1, col + 1);
        }
        self.git.load_blob(path, BlobWhich::Head);
        if focus {
            self.focus.set(PanelId::Editor);
        }
    }

    /// File bytes as text and whether decoding was lossy; `None` (with a
    /// notification) for binary files.
    pub fn decode_file(&mut self, path: &std::path::Path, bytes: Vec<u8>) -> Option<(String, bool)> {
        let head = &bytes[..bytes.len().min(8192)];
        if head.contains(&0) {
            self.notify(Level::Warn, format!("{} looks binary; not opening", path.display()));
            return None;
        }
        let (text, lossy) = match String::from_utf8(bytes) {
            Ok(s) => (s, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
        if lossy {
            self.notify(Level::Warn, format!("{} is not valid UTF-8; shown with replacement characters", path.display()));
        }
        Some((text, lossy))
    }
}

/// The most telling line of a failed command's output: the first `fatal:`
/// or `error:` line, else the first non-empty one.
fn error_line(output: &str) -> String {
    let lines = || output.lines().map(str::trim).filter(|l| !l.is_empty());
    lines().find(|l| l.starts_with("fatal:") || l.starts_with("error:")).or_else(|| lines().next()).unwrap_or("failed").to_string()
}
