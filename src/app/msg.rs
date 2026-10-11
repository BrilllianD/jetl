//! Background results: `AppMsg` dispatch, git results, file loads.

use std::path::PathBuf;

use super::{App, Ctx};
use crate::event::{AppMsg, Level};
use crate::files::ops::FsOp;
use crate::git::{BlobWhich, GitMsg, GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::PanelId;
use crate::ui::palette::Source;

impl App {
    pub(super) fn handle_msg(&mut self, msg: AppMsg) {
        self.handle_msg_inner(msg);
        self.lsp_confirm_orphaned();
    }

    fn handle_msg_inner(&mut self, msg: AppMsg) {
        self.dirty_frame = true;
        match msg {
            AppMsg::ConfigChanged { files } => self.config_changed(files),
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
            AppMsg::Symbols { generation, seq, batch } => self.symbols_loaded(generation, seq, batch),
            AppMsg::PaletteFiltered { generation, seq, rows } => self.palette_filtered(generation, seq, rows),
            AppMsg::RecentProjects { seq, version, roots, error } => self.recent_projects_loaded(seq, version, roots, error),
            AppMsg::Find { generation, batch } if generation == self.workspace.generation => self.find.on_batch(batch),
            AppMsg::ReplaceDone { generation, files, matches, errors } if generation == self.workspace.generation => {
                self.replace_done(files, matches, errors)
            }
            AppMsg::Find { .. } | AppMsg::ReplaceDone { .. } => tracing::debug!("dropping stale find result"),
            // Messages of a previous root are dropped; a stale watcher stops
            // when its message is dropped.
            AppMsg::FsChanged { generation, changes } if generation == self.workspace.generation => self.fs_changed(changes),
            AppMsg::WatcherStarted { generation, result } if generation == self.workspace.generation => self.watcher_started(result),
            AppMsg::LspWatched { generation, events } if generation == self.workspace.generation => self.lsp_watched(events),
            AppMsg::LspWatcherStarted { generation, dirs, result } if generation == self.workspace.generation => {
                self.lsp_watcher_started(dirs, result)
            }
            AppMsg::DiskText { generation, path, result } if generation == self.workspace.generation => self.disk_text(path, result),
            AppMsg::FsChanged { .. }
            | AppMsg::WatcherStarted { .. }
            | AppMsg::LspWatched { .. }
            | AppMsg::LspWatcherStarted { .. }
            | AppMsg::DiskText { .. } => {
                tracing::debug!("dropping stale watcher message");
            }
            AppMsg::FsOpDone { generation, op, is_dir, result } if generation == self.workspace.generation => {
                self.fs_op_done(op, is_dir, result)
            }
            // The tree and tabs are a previous root's, but the store (and the
            // Local History window) is global.
            AppMsg::FsOpDone { op: FsOp::Rename { from, to, .. }, result: Ok(()), .. } => self.history_renamed(&from, &to),
            AppMsg::FsOpDone { op, .. } => tracing::debug!("dropping file operation result of a previous root: {op:?}"),
            AppMsg::Paste { generation, seq, result } => self.paste_done(generation, seq, result),
            AppMsg::Formatted { generation, id, seq, result } => self.format_done(generation, id, seq, result),
            AppMsg::RunOutput { generation, run, lines } => self.run_output(generation, run, lines),
            AppMsg::RunExited { generation, run, end } => self.run_exited(generation, run, end),
            AppMsg::RunTasks { generation, seq, tasks, error } => self.run_tasks_loaded(generation, seq, tasks, error),
            AppMsg::TerminalOutput { generation, id } => self.terminal_output(generation, id),
            AppMsg::Lsp(m) => self.lsp_msg(m),
            AppMsg::Locations { generation, seq, title, items } => self.locations_loaded(generation, seq, title, items),
            AppMsg::EditsApplied { generation, origin, done, errors, ops, failed } if generation == self.workspace.generation => {
                self.edits_applied(origin, done, errors, ops, failed)
            }
            // As for FsOpDone: only the global history store follows.
            AppMsg::EditsApplied { ops, .. } => {
                for op in ops {
                    if let FsOp::Rename { from, to, .. } = op {
                        self.history_renamed(&from, &to);
                    }
                }
                tracing::debug!("dropping a workspace edit report of a previous root");
            }
            AppMsg::LocalHistory(m) => self.history_msg(m),
        }
    }

    pub(super) fn on_git_msg(&mut self, mut msg: GitMsg) {
        if let GitMsg::OpDone { id, renames, .. } = &mut msg {
            self.git.finish(*id);
            // Absolute paths and a move that happened: carried even when the
            // root switched meanwhile.
            let renames = std::mem::take(renames);
            self.history_moved(renames);
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
                self.offer_conflicts();
                self.offer_next_conflict();
                let snap = self.git_panel.snapshot();
                self.project.set_git_status(snap.map(|s| s.tree.clone()).unwrap_or_default());
                // A new HEAD changes every gutter base.
                if snap.map(|s| s.head.clone()) != prev_head {
                    for (p, base) in self.editor.open_bases() {
                        self.git.load_blob(p, BlobWhich::Head, base);
                    }
                    for (p, base) in self.editor.blame_bases() {
                        self.git.load_blame(p, base);
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
            GitMsg::Log { skip, filter, result, .. } => {
                let mut cx = ctx!(self);
                self.git_panel.set_log(skip, &filter, result, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            GitMsg::CommitDetails { sha, result, .. } => self.git_panel.set_details(&sha, result),
            GitMsg::PushRemote { branch, result, .. } => self.push_remote_known(branch, result),
            GitMsg::HeadMessage { result, .. } => {
                if let Err(e) = self.git_panel.set_head_message(result) {
                    self.notify(Level::Warn, e);
                }
            }
            GitMsg::CommitMessages { messages, .. } => self.git_panel.set_history(messages),
            GitMsg::Blame { seq, path, result, .. } => {
                if let Some(e) = self.editor.set_blame(&path, seq, result) {
                    self.notify(Level::Warn, e);
                }
                self.editor.update_gutters();
            }
            GitMsg::Stashes { result, .. } => {
                let mut cx = ctx!(self);
                self.git_panel.set_stashes(result, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            GitMsg::Conflict { path, result, .. } => self.conflict_loaded(path, result),
            GitMsg::OpDone { op, label, ok, output, .. } => self.git_op_done(op, label, ok, output),
        }
    }

    /// A push failed for lack of an upstream: confirm the configured or only
    /// remote, or let the user pick one of several.
    fn push_remote_known(&mut self, branch: String, result: anyhow::Result<crate::git::repo::PushRemote>) {
        use crate::git::repo::PushRemote;
        match result {
            Ok(PushRemote::One(remote)) => {
                let msg = format!("Branch '{branch}' has no upstream.\nPush it and set the upstream to {remote}/{branch}?");
                self.dialog = Some(Dialog::confirm(DialogId::PushSetUpstream { branch, remote }, "Push", &msg, &["Push", "Cancel"]));
            }
            Ok(PushRemote::Choose(remotes)) => {
                let msg = format!("Branch '{branch}' has no upstream. Push it to:");
                self.dialog =
                    Some(Dialog::choice(DialogId::PushUpstreamRemote { branch, remotes: remotes.clone() }, "Push", &msg, remotes));
            }
            Ok(PushRemote::NoRemote) => {
                self.notify(Level::Error, "No remote configured; nothing to push to. Add one with `git remote add origin <url>`.")
            }
            Err(e) => self.notify(Level::Error, format!("Push: {e:#}")),
        }
    }

    /// `git push -u <remote> <branch>`.
    pub(super) fn push_upstream(&mut self, branch: String, remote: String) {
        self.git.run_cli(GitOp::PushUpstream(branch.clone()), vec!["push".into(), "-u".into(), remote, branch]);
    }

    pub(super) fn git_op_done(&mut self, op: GitOp, label: String, ok: bool, output: String) {
        self.git_panel.log_op(&label, ok, &output);
        if self.worktree_op_done(&op, ok, &output) {
            // Reported (or a follow-up dialog opened) by the worktree flow.
        } else if ok {
            match &op {
                GitOp::Commit { push, amend } => {
                    self.git_panel.committed();
                    self.git.load_messages();
                    self.notify(Level::Info, format!("{} {output}", if *amend { "Amended" } else { "Committed" }));
                    if *push {
                        self.git_request(GitRequest::Push);
                    }
                }
                GitOp::Push | GitOp::PushUpstream(_) => self.notify(Level::Info, "Pushed"),
                GitOp::Pull => self.notify(Level::Info, "Pulled"),
                GitOp::Fetch => self.notify(Level::Info, "Fetched"),
                GitOp::Switch(b) => self.notify(Level::Info, format!("Switched to {b}")),
                GitOp::NewBranch(b) => self.notify(Level::Info, format!("Created and switched to {b}")),
                GitOp::Checkout(sha) => self.notify(Level::Info, format!("Checked out {} (detached HEAD)", short(sha))),
                GitOp::CherryPick(sha) => self.notify(Level::Info, format!("Cherry-picked {}", short(sha))),
                GitOp::Revert(sha) => self.notify(Level::Info, format!("Reverted {}", short(sha))),
                GitOp::Merge(b) => self.notify(Level::Info, format!("Merged {b}")),
                GitOp::Rebase(b) => self.notify(Level::Info, format!("Rebased onto {b}")),
                GitOp::RenameBranch(b) => self.notify(Level::Info, format!("Renamed the branch to {b}")),
                GitOp::DeleteBranch { name, .. } => self.notify(Level::Info, format!("Deleted branch {name}")),
                GitOp::Tag(t) => self.notify(Level::Info, format!("Created tag {t}")),
                GitOp::DeleteTag(t) => self.notify(Level::Info, format!("Deleted tag {t}")),
                GitOp::Discard | GitOp::Stash => self.notify(Level::Info, output.clone()),
                GitOp::Resolve(rel) => {
                    if !output.is_empty() {
                        self.notify(Level::Info, output.clone());
                    }
                    self.resolved_conflict = Some(rel.clone());
                }
                GitOp::Stage | GitOp::Unstage => {}
                GitOp::WorktreeAdd { .. } | GitOp::WorktreeRemove { .. } | GitOp::WorktreePrune => {}
            }
            if op.touches_worktree() {
                let mut sink = Vec::new();
                let n = self.editor.reload_clean_from_disk(&mut sink);
                for c in sink {
                    self.history.capture(c);
                }
                if n > 0 {
                    tracing::info!("reloaded {n} buffer(s) after {label}");
                }
            }
        } else if op == GitOp::Push && (output.contains("has no upstream branch") || output.contains("No configured push destination")) {
            // No upstream (one remote) or no default remote (several): ask
            // where to push once the remote is known (GitMsg::PushRemote).
            self.git.load_push_remote(self.branch_label().unwrap_or_default());
        } else if let GitOp::DeleteBranch { name, force: false } = &op
            && output.contains("not fully merged")
        {
            let msg = format!("Branch '{name}' is not fully merged.\nDelete it anyway? Commits only it has may be lost.");
            let id = DialogId::DeleteBranch { name: name.clone(), force: true };
            self.dialog = Some(Dialog::confirm(id, "Delete Branch", &msg, &["Force Delete", "Cancel"]));
        } else if matches!(op, GitOp::Merge(_) | GitOp::Rebase(_) | GitOp::CherryPick(_) | GitOp::Revert(_) | GitOp::Stash)
            && output.contains("CONFLICT")
        {
            // Conflicted files show as U in the Commit tab; the next status
            // opens Resolve Conflicts.
            let what = match &op {
                GitOp::Rebase(_) => {
                    "Rebase stopped on conflicts: resolve the U files (Git > Resolve Conflicts), then run `git rebase --continue` (or `--abort`)"
                }
                GitOp::Merge(_) => {
                    "Merge stopped on conflicts: resolve the U files (Git > Resolve Conflicts) and commit (or run `git merge --abort`)"
                }
                GitOp::Stash => {
                    "The stash conflicts with your files: resolve the U files (Git > Resolve Conflicts); the stash entry is kept, drop it when done"
                }
                _ => "Stopped on conflicts: resolve the U files (Git > Resolve Conflicts) and commit",
            };
            self.notify(Level::Warn, what);
            self.pending_conflicts = true;
        } else if matches!(op, GitOp::Commit { .. } | GitOp::Stage | GitOp::Unstage | GitOp::Discard | GitOp::Stash) {
            self.notify(Level::Error, error_line(&output));
        } else {
            self.notify(Level::Error, format!("{label}: {}", error_line(&output)));
        }
        self.refresh_tree();
        self.git.refresh_status();
        self.git.load_branches();
        self.git.load_stashes();
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
        self.resolved_conflict = None;
        self.project.set_git_status(Default::default());
        for b in &mut self.editor.buffers {
            b.diff_base = None;
            b.gutter.clear();
        }
        self.git.refresh_status();
        self.git.load_branches();
        self.git.load_messages();
        self.git.load_stashes();
        for (p, base) in self.editor.open_bases() {
            self.git.load_blob(p, BlobWhich::Head, base);
        }
        if self.git_panel.tab == crate::git::panel::GitTab::Log {
            let mut cx = ctx!(self);
            self.git_panel.request_log(&mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
        }
    }

    /// Show a Git window tab (session restore), loading the log if needed.
    pub fn set_git_tab(&mut self, tab: crate::git::panel::GitTab) {
        let mut cx = ctx!(self);
        self.git_panel.set_tab(tab, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// Open a file with the caret at a 0-based line and char column.
    pub fn open_file_at(&mut self, path: PathBuf, line: usize, col: usize) {
        self.open_file_goto(path, GotoTarget::Char { line, col });
    }

    /// Open a file with the caret at an LSP position (0-based line, UTF-16
    /// column), converted against the text once it is loaded.
    pub fn open_file_at_utf16(&mut self, path: PathBuf, pos: lsp_types::Position) {
        self.open_file_goto(path, GotoTarget::Utf16(pos));
    }

    fn open_file_goto(&mut self, path: PathBuf, target: GotoTarget) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        if self.editor.index_of(&path).is_some() {
            self.open_file(path, true);
            self.goto_target(target);
        } else {
            self.pending_goto = Some((path.clone(), target));
            self.open_file(path, true);
        }
    }

    /// Put the active buffer's caret at `target` (remembered for back).
    fn goto_target(&mut self, target: GotoTarget) {
        let (line, col) = self.target_line_col(target);
        self.editor.goto_line(line + 1, col + 1);
    }

    /// `target` in the active buffer as a 0-based line and char column.
    fn target_line_col(&self, target: GotoTarget) -> (usize, usize) {
        match target {
            GotoTarget::Char { line, col } => (line, col),
            GotoTarget::Utf16(pos) => match self.editor.active_buffer() {
                Some(b) => crate::editor::selection::pos_to_line_col(&b.rope, crate::lsp::pos::char_index(&b.rope, pos)),
                None => (0, 0),
            },
        }
    }

    /// Open a file: reuse the tab when already open, otherwise read it off
    /// the main loop and finish in `file_loaded`.
    pub fn open_file(&mut self, path: PathBuf, focus: bool) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
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
        if self.editor.merge_active() {
            return self.notify(Level::Info, format!("{} not opened: {}", path.display(), crate::editor::merge::BUSY));
        }
        let Some((text, lossy)) = self.decode_file(&path, bytes) else { return };
        self.editor.open_text(path.clone(), &text, lossy, &self.theme);
        if self.is_conflicted(&path) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.notify(Level::Info, format!("{name} has conflicts: Git > Resolve Conflicts"));
        }
        self.editor.enforce_tab_limit();
        if let Some(pending) = self.pending_revert.remove(&path) {
            match pending {
                super::history::PendingRevert::Text(text) => self.apply_revert(&path, &text),
                super::history::PendingRevert::Member(text) => self.apply_revert_with(&path, &text, true),
                super::history::PendingRevert::Changes(rev) => self.apply_changes_revert(*rev),
            }
        }
        if let Some((_, target)) = self.pending_goto.take_if(|(p, _)| *p == path) {
            // Opening the tab remembered the position left; the new tab's
            // first caret is not worth going back to.
            let (line, col) = self.target_line_col(target);
            if let Some(b) = self.editor.active_buffer_mut() {
                b.goto_line(line, col);
            }
        }
        self.git.load_blob(path, BlobWhich::Head, None);
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

/// Where `open_file_at` and `open_file_at_utf16` put the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GotoTarget {
    /// 0-based line and char column.
    Char { line: usize, col: usize },
    /// An LSP position: 0-based line and UTF-16 column.
    Utf16(lsp_types::Position),
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// The most telling line of a failed command's output: the first `fatal:`
/// or `error:` line, else the first non-empty one.
fn error_line(output: &str) -> String {
    let lines = || output.lines().map(str::trim).filter(|l| !l.is_empty());
    lines().find(|l| l.starts_with("fatal:") || l.starts_with("error:")).or_else(|| lines().next()).unwrap_or("failed").to_string()
}
