//! Background results: `AppMsg` dispatch, git results, file loads.

use std::path::PathBuf;

use super::{App, Ctx};
use crate::event::{AppMsg, Level};
use crate::files::ops::FsOp;
use crate::git::model::{InProgressKind, ProgressAction};
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
            AppMsg::FileLoaded { generation, path, result, focus, view } => {
                if generation != self.workspace.generation {
                    // Its queued revert, if any, already went with the root switch
                    // (`App::pending_reverts_dropped`); a `pending_revert` entry for the
                    // same path now belongs to the new root and is left alone.
                    tracing::debug!("dropping stale load of {}", path.display());
                } else if let Some(ticket) = view {
                    self.debug_file_loaded(ticket, path, result, focus);
                } else {
                    self.file_loaded(path, result, focus, None);
                }
            }
            AppMsg::Git(m) => self.on_git_msg(m),
            AppMsg::Yman(m) => {
                if let Some(done) = self.yman.on_msg(m) {
                    self.finish_yman_mutation(done);
                }
                self.refresh_new_task_tags();
            }
            AppMsg::Worktrees { generation, result } => {
                if generation == self.workspace.generation {
                    self.worktrees.set_list(result, &self.workspace.root);
                } else {
                    tracing::debug!("dropping stale worktree list");
                }
            }
            AppMsg::SessionLoaded { generation, session, files } => self.session_loaded(generation, *session, files),
            AppMsg::BranchSetLoaded { generation, seq, set, files } => self.branch_set_loaded(generation, seq, set, files),
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
            AppMsg::ImportEdges { generation, edges } => self.import_edges_loaded(generation, edges),
            AppMsg::HierarchyIndexChildren { generation, tree, node, children } => {
                self.hierarchy_index_answer(generation, tree, node, children)
            }
            AppMsg::Scip { generation, seq, result } => self.scip_loaded(generation, seq, result),
            AppMsg::PaletteFiltered { generation, seq, rows } => self.palette_filtered(generation, seq, rows),
            AppMsg::Dependencies { generation, seq, report } => self.dependencies_checked(generation, seq, report),
            AppMsg::RecentProjects { seq, version, roots, error } => self.recent_projects_loaded(seq, version, roots, error),
            AppMsg::Layouts { version, layouts, error, done } => self.layouts_loaded(version, layouts, error, done),
            AppMsg::RenamePreview { generation, seq, result } => self.rename_preview_done(generation, seq, *result),
            AppMsg::Find { generation, batch } if generation == self.workspace.generation => self.find.on_batch(batch),
            AppMsg::Todo { generation, batch } if generation == self.workspace.generation => self.todo.on_batch(batch),
            AppMsg::ReplaceDone { generation, files, matches, errors } if generation == self.workspace.generation => {
                self.replace_done(files, matches, errors)
            }
            AppMsg::Find { .. } | AppMsg::ReplaceDone { .. } => tracing::debug!("dropping stale find result"),
            AppMsg::Todo { .. } => tracing::debug!("dropping stale TODO batch"),
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
            AppMsg::Compared { generation, title, key, result } => self.compared(generation, title, key, result),
            AppMsg::Formatted { generation, id, seq, result } => self.format_done(generation, id, seq, result),
            AppMsg::RunOutput { generation, run, lines } => self.run_output(generation, run, lines),
            AppMsg::RunExited { generation, run, end } => self.run_exited(generation, run, end),
            AppMsg::RunTasks { generation, seq, tasks, error } => self.run_tasks_loaded(generation, seq, tasks, error),
            AppMsg::TerminalOutput { generation, id } => self.terminal_output(generation, id),
            AppMsg::Lsp(m) => self.lsp_msg(m),
            AppMsg::Dap(m) => self.dap_msg(m),
            AppMsg::DebugOutput { generation, session, lines } => self.debug_output(generation, session, lines),
            AppMsg::DebugAdapterMemory { generation, session, kb } => self.debug_adapter_memory_read(generation, session, kb),
            AppMsg::DebugReady { generation, seq, result } => self.debug_ready(generation, seq, *result),
            AppMsg::DebugProcesses { generation, seq, items, ptrace_scope } => self.debug_processes(generation, seq, items, ptrace_scope),
            AppMsg::DebugBuilt { generation, seq, end, artifacts } => self.debug_built(generation, seq, end, artifacts),
            AppMsg::Locations { generation, seq, title, items } => self.locations_loaded(generation, seq, title, items),
            AppMsg::QuickDefinition { generation, tag, heading, code, lang } => {
                self.quick_definition_loaded(generation, tag, heading, code, lang)
            }
            AppMsg::RecentLocations { generation, items } => self.recent_locations_loaded(generation, items),
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
                // The editor set follows the branch (also a new branch on the same commit).
                self.sync_branch_set();
            }
            GitMsg::Blob { seq, path, which: BlobWhich::Head, text, .. } => {
                self.editor.set_diff_base(&path, seq, text);
                self.editor.update_gutters();
            }
            GitMsg::Blob { .. } => {}
            GitMsg::Diff { key, result, .. } => {
                self.editor.set_diff(&key, &result);
                self.git_panel.set_diff(&key, result)
            }
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
            GitMsg::CommitsDiff { title, spans, result, .. } => match result {
                Ok(text) => self.open_patch(title, spans, text),
                Err(e) => self.notify(Level::Error, format!("Diff: {e:#}")),
            },
            GitMsg::CommitDetails { sha, result, .. } => self.git_panel.set_details(&sha, result, &self.workspace.root),
            GitMsg::CommitsFiles { tab: Some(title), result, .. } => match result {
                Ok(rows) => self.open_diff_tab(title, rows.into_iter().map(|(_, k)| k).collect(), 0),
                Err(e) => self.notify(Level::Error, format!("Diff: {e:#}")),
            },
            GitMsg::CommitsFiles { spans, tab: None, result, .. } => self.git_panel.set_log_files(&spans, result, &self.workspace.root),
            GitMsg::PushRemote { branch, result, .. } => self.push_remote_known(branch, result),
            GitMsg::RebasePlan { result, .. } => self.rebase_plan_loaded(result),
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
                GitOp::Reset { sha, mode } => self.notify(Level::Info, format!("Reset to {} ({})", short(sha), mode.name())),
                GitOp::Merge(b) => self.notify(Level::Info, format!("Merged {b}")),
                GitOp::Rebase(b) => self.notify(Level::Info, format!("Rebased onto {b}")),
                GitOp::InteractiveRebase(_) => self.notify(Level::Info, "Interactive rebase done"),
                GitOp::Progress { kind, action } => self.notify(Level::Info, action.done(*kind)),
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
        } else if matches!(
            op,
            GitOp::Merge(_)
                | GitOp::Rebase(_)
                | GitOp::InteractiveRebase(_)
                | GitOp::CherryPick(_)
                | GitOp::Revert(_)
                | GitOp::Stash
                | GitOp::Progress { action: ProgressAction::Continue | ProgressAction::Skip, .. }
        ) && output.contains("CONFLICT")
        {
            // Conflicted files show as U in the Commit tab; the next status
            // opens Resolve Conflicts.
            let what = match &op {
                GitOp::Rebase(_) | GitOp::InteractiveRebase(_) | GitOp::Progress { kind: InProgressKind::Rebase, .. } => {
                    "Rebase stopped on conflicts: resolve the U files (Git > Resolve Conflicts), then Git > Continue Operation (or Abort Operation)"
                }
                GitOp::Merge(_) => {
                    "Merge stopped on conflicts: resolve the U files (Git > Resolve Conflicts), then Git > Continue Operation (or Abort Operation)"
                }
                GitOp::Stash => {
                    "The stash conflicts with your files: resolve the U files (Git > Resolve Conflicts); the stash entry is kept, drop it when done"
                }
                _ => {
                    "Stopped on conflicts: resolve the U files (Git > Resolve Conflicts), then Git > Continue Operation (or Abort Operation)"
                }
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
    pub(super) fn goto_target(&mut self, target: GotoTarget) {
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

    /// Open a patch tab (Git Log Show Diff as Patch) and focus the editor.
    pub(super) fn open_patch(&mut self, title: String, spans: Vec<crate::git::model::DiffSpan>, text: String) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        self.editor.open_patch(&title, spans, &text);
        self.focus.set(PanelId::Editor);
    }

    /// Open a diff tab (Git window Ctrl+D) with the Git window's layout and focus the editor.
    pub(super) fn open_diff_tab(&mut self, title: String, files: Vec<crate::git::model::DiffKey>, at: usize) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        if files.is_empty() {
            return self.notify(Level::Info, "No changes to show");
        }
        self.editor.diff_opts = self.git_panel.diff_opts();
        let mut cx = ctx!(self);
        self.editor.open_diff_tab(&title, files, at, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
        self.focus.set(PanelId::Editor);
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
        self.read_file(path, focus, None);
    }

    /// Reads a file off the main loop, for `file_loaded` or, with a ticket, `debug_file_loaded`.
    pub(super) fn read_file(&self, path: PathBuf, focus: bool, view: Option<u64>) {
        let (tx, generation) = (self.bus_tx.clone(), self.workspace.generation);
        tokio::task::spawn_blocking(move || {
            let result = std::fs::read(&path);
            let _ = tx.send(AppMsg::FileLoaded { generation, path, result, focus, view });
        });
    }

    /// `target` is where the debugger wants the caret, and `None` takes `pending_goto`.
    pub(super) fn file_loaded(&mut self, path: PathBuf, result: std::io::Result<Vec<u8>>, focus: bool, target: Option<GotoTarget>) {
        let bytes = match result {
            Ok(b) => b,
            Err(e) => {
                self.notify(Level::Error, format!("open {}: {e}", path.display()));
                self.pending_revert_failed(&path);
                return;
            }
        };
        if self.editor.merge_active() {
            self.notify(Level::Info, format!("{} not opened: {}", path.display(), crate::editor::merge::BUSY));
            return self.pending_revert_failed(&path);
        }
        let Some((text, lossy)) = self.decode_file(&path, bytes) else {
            return self.pending_revert_failed(&path);
        };
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
        let target = target.or_else(|| self.pending_goto.take_if(|(p, _)| *p == path).map(|(_, t)| t));
        if let Some(target) = target {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{load, test_app};
    use crate::editor::undo::TxKind;
    use crate::git::model::ResetMode;

    /// Feed `OpDone` for `op` (current generation, id 1, no renames).
    fn done(app: &mut App, op: GitOp, label: &str, ok: bool, output: &str) {
        let generation = app.workspace.generation;
        app.handle_msg(AppMsg::Git(GitMsg::OpDone {
            generation,
            id: 1,
            op,
            label: label.into(),
            ok,
            output: output.into(),
            renames: Vec::new(),
        }));
    }

    #[tokio::test]
    async fn ctrl_d_in_changes_opens_a_diff_tab_fed_with_the_panel() {
        use crate::actions::{Action, GitAction};
        use crate::git::diff::DiffDoc;
        let (mut app, dir) = test_app("diff-tab-changes");
        let root = app.workspace.root.clone();
        crate::app::tests::init_repo(&app, &[("a.txt", "a\n")]);
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        crate::app::tests::feed_status(&mut app, 1);
        app.focus.set(PanelId::Git);
        let want = crate::git::model::DiffKey::Change { section: crate::git::model::Section::Unstaged, rel: "a.txt".into(), old: None };
        if app.git_panel.diff_key() != Some(&want) {
            app.perform(Action::Git(GitAction::Down));
        }
        assert_eq!(app.git_panel.diff_key(), Some(&want));
        app.perform(Action::Git(GitAction::ShowDiff));
        assert_eq!(app.focus.current, PanelId::Editor);
        let b = app.editor.active_buffer().unwrap();
        assert!(b.diff.is_some());
        assert_eq!(b.name, "Diff a.txt");
        let n = app.editor.buffers.len();
        let result = Ok(DiffDoc::build(b"a\n", b"b\n", 4));
        app.handle_msg(AppMsg::Git(GitMsg::Diff { generation: app.git.generation, key: want, result }));
        assert!(app.editor.active_buffer().unwrap().diff.as_ref().unwrap().pane.doc.is_some());
        assert!(app.git_panel.diff_loaded());
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::ShowDiff));
        assert_eq!(app.editor.buffers.len(), n);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_commits_files_tab_notifies_and_opens_no_tab() {
        let (mut app, dir) = test_app("commits-files-tab-err");
        let n = app.editor.buffers.len();
        app.handle_msg(AppMsg::Git(GitMsg::CommitsFiles {
            generation: app.git.generation,
            spans: Vec::new(),
            tab: Some("Diff abc".into()),
            result: Err(anyhow::anyhow!("boom")),
        }));
        assert_eq!(last(&app), (Level::Error, "Diff: boom".to_string()));
        assert_eq!(app.editor.buffers.len(), n);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The newest notification as (level, text).
    fn last(app: &App) -> (Level, String) {
        let n = app.notifications.last().expect("a notification");
        (n.level, n.text.clone())
    }

    #[tokio::test]
    async fn yman_reloads_on_watcher_and_focus() {
        use crate::yman::{TrackerState, YmanMsg, cli::yman_available};
        if !yman_available() {
            eprintln!("skipping: yman not on PATH");
            return;
        }
        // Removes the temp dir also when an assertion fails.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let (mut app, dir) = test_app("yman-reload");
        let _cleanup = Cleanup(dir);
        let root = app.workspace.root.clone();
        let run = |cmd: &str, args: &[&str]| {
            let out = std::process::Command::new(cmd)
                .args(args)
                .current_dir(&root)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run("git", &["init", "-q"]);
        crate::git::repo::tests::isolate(&root);
        run("git", &["config", "user.name", "t"]);
        run("git", &["config", "user.email", "t@t"]);
        run("yman", &["init", "--offline"]);
        let is_snapshot = |m: &AppMsg| matches!(m, AppMsg::Yman(YmanMsg::Snapshot { .. }));
        let count = |app: &App| match &app.yman.state {
            TrackerState::Ready(s) => s.tasks.len(),
            other => panic!("not ready: {other:?}"),
        };
        app.yman.want();
        crate::app::tests::handle_next(&mut app, is_snapshot).await;
        assert_eq!(count(&app), 0);
        run("yman", &["add", "Second"]);
        app.fs_changed(crate::files::watch::Changes { git: true, ..Default::default() });
        crate::app::tests::handle_next(&mut app, is_snapshot).await;
        assert_eq!(count(&app), 1);
        run("yman", &["add", "Third"]);
        app.handle_term_event(crossterm::event::Event::FocusGained);
        crate::app::tests::handle_next(&mut app, is_snapshot).await;
        assert_eq!(count(&app), 2);
    }

    #[tokio::test]
    async fn stale_op_results_only_finish_the_op() {
        let (mut app, dir) = test_app("msg-stale");
        app.git.busy.push((1, "git pull".into()));
        let before = app.notifications.len();
        let generation = app.workspace.generation + 1;
        app.handle_msg(AppMsg::Git(GitMsg::OpDone {
            generation,
            id: 1,
            op: GitOp::Pull,
            label: "git pull".into(),
            ok: false,
            output: "fatal: boom".into(),
            renames: Vec::new(),
        }));
        assert!(app.git.busy.is_empty());
        assert_eq!(app.notifications.len(), before);
        assert!(!app.pending_conflicts);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn successful_ops_toast_what_happened() {
        let (mut app, dir) = test_app("msg-success");
        let progress = |kind, action| GitOp::Progress { kind, action };
        let table = [
            (GitOp::Fetch, "", "Fetched"),
            (GitOp::Pull, "", "Pulled"),
            (GitOp::Push, "", "Pushed"),
            (GitOp::Switch("dev".into()), "", "Switched to dev"),
            (GitOp::Checkout("0123456789abcdef".into()), "", "Checked out 01234567 (detached HEAD)"),
            (GitOp::Reset { sha: "0123456789abcdef".into(), mode: ResetMode::Hard }, "", "Reset to 01234567 (hard)"),
            (GitOp::Rebase("main".into()), "", "Rebased onto main"),
            (GitOp::InteractiveRebase("01234567".into()), "", "Interactive rebase done"),
            (GitOp::Commit { push: false, amend: true }, "abc1234 msg", "Amended abc1234 msg"),
            (progress(InProgressKind::Rebase, ProgressAction::Continue), "", "Rebase finished"),
            (GitOp::Stash, "Saved working directory", "Saved working directory"),
        ];
        for (op, output, expected) in table {
            done(&mut app, op.clone(), "git", true, output);
            assert_eq!(last(&app), (Level::Info, expected.to_string()), "{op:?}");
        }

        let abort = ProgressAction::Abort.done(InProgressKind::Rebase);
        assert!(!abort.is_empty());
        done(&mut app, progress(InProgressKind::Rebase, ProgressAction::Abort), "git rebase --abort", true, "");
        assert_eq!(last(&app), (Level::Info, abort));

        let before = app.notifications.len();
        done(&mut app, GitOp::Resolve("a.txt".into()), "git checkout --ours", true, "");
        assert_eq!(app.notifications.len(), before);
        assert_eq!(app.resolved_conflict.as_deref(), Some("a.txt"));

        done(&mut app, GitOp::WorktreePrune, "git worktree prune", true, "");
        assert_eq!(last(&app), (Level::Info, "Nothing to prune".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conflicts_warn_and_wait_for_the_status() {
        let (mut app, dir) = test_app("msg-conflicts");
        let out = "CONFLICT (content): Merge conflict in a.txt\nerror: could not apply 1234567";
        let progress = |kind, action| GitOp::Progress { kind, action };
        let rebase = "Rebase stopped on conflicts";
        let stopped = "Stopped on conflicts";
        let table = [
            (GitOp::Rebase("main".into()), rebase),
            (GitOp::InteractiveRebase("0123".into()), rebase),
            (progress(InProgressKind::Rebase, ProgressAction::Continue), rebase),
            (progress(InProgressKind::Rebase, ProgressAction::Skip), rebase),
            (GitOp::Merge("dev".into()), "Merge stopped on conflicts"),
            (GitOp::Stash, "The stash conflicts with your files"),
            (GitOp::CherryPick("0123".into()), stopped),
            (GitOp::Revert("0123".into()), stopped),
            (progress(InProgressKind::Merge, ProgressAction::Continue), stopped),
        ];
        for (op, prefix) in table {
            app.pending_conflicts = false;
            let before = app.notifications.len();
            done(&mut app, op.clone(), "git", false, out);
            assert_eq!(app.notifications.len(), before + 1, "{op:?}");
            let (level, text) = last(&app);
            assert_eq!(level, Level::Warn, "{op:?}");
            assert!(text.starts_with(prefix), "{op:?}: {text}");
            assert!(app.pending_conflicts, "{op:?}");
        }

        app.pending_conflicts = false;
        done(&mut app, progress(InProgressKind::Rebase, ProgressAction::Abort), "git rebase --abort", false, out);
        assert!(!app.pending_conflicts);
        assert_eq!(last(&app), (Level::Error, "git rebase --abort: error: could not apply 1234567".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failures_toast_the_telling_line() {
        let (mut app, dir) = test_app("msg-failures");
        let out = "hint: a\nerror: nothing added\nfatal: later";
        let bare = [GitOp::Commit { push: false, amend: false }, GitOp::Stage, GitOp::Unstage, GitOp::Discard];
        for op in bare {
            let before = app.notifications.len();
            done(&mut app, op.clone(), "git x", false, out);
            assert_eq!(app.notifications.len(), before + 1, "{op:?}");
            assert_eq!(last(&app), (Level::Error, "error: nothing added".to_string()), "{op:?}");
        }
        done(&mut app, GitOp::Pull, "git pull", false, out);
        assert_eq!(last(&app), (Level::Error, "git pull: error: nothing added".to_string()));
        done(&mut app, GitOp::Fetch, "git fetch --prune", false, "");
        assert_eq!(last(&app), (Level::Error, "git fetch --prune: failed".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn push_without_upstream_asks_for_the_remote() {
        let (mut app, dir) = test_app("msg-push");
        let before = app.notifications.len();
        for out in ["fatal: The current branch dev has no upstream branch.", "fatal: No configured push destination."] {
            done(&mut app, GitOp::Push, "git push", false, out);
            assert_eq!(app.notifications.len(), before, "{out}");
            assert!(app.dialog.is_none(), "{out}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deleting_an_unmerged_branch_offers_force() {
        let (mut app, dir) = test_app("msg-delete-branch");
        let before = app.notifications.len();
        let out = "error: The branch 'dev' is not fully merged.";
        done(&mut app, GitOp::DeleteBranch { name: "dev".into(), force: false }, "git branch -d dev", false, out);
        let id = app.dialog.as_ref().map(|d| d.id.clone());
        assert_eq!(id, Some(DialogId::DeleteBranch { name: "dev".into(), force: true }));
        assert_eq!(app.notifications.len(), before);

        app.dialog = None;
        done(&mut app, GitOp::DeleteBranch { name: "dev".into(), force: true }, "git branch -D dev", false, out);
        assert!(app.dialog.is_none());
        let (level, text) = last(&app);
        assert_eq!(level, Level::Error);
        assert!(text.starts_with("git branch -D dev"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn clean_tabs_reload_only_after_worktree_ops_that_succeeded() {
        let (mut app, dir) = test_app("msg-reload");
        let path = load(&mut app, "a.txt", "one\n");
        let text = |app: &App| app.editor.buffers[app.editor.index_of(&path).unwrap()].rope.to_string();
        std::fs::write(&path, "two\n").unwrap();

        done(&mut app, GitOp::Fetch, "git fetch", true, "");
        assert_eq!(text(&app), "one\n");
        done(&mut app, GitOp::Pull, "git pull", false, "fatal: boom");
        assert_eq!(text(&app), "one\n");
        done(&mut app, GitOp::Pull, "git pull", true, "");
        assert_eq!(text(&app), "two\n");
        let i = app.editor.index_of(&path).unwrap();
        assert!(!app.editor.buffers[i].dirty());

        app.editor.buffers[i].insert("x", TxKind::Typing);
        std::fs::write(&path, "three\n").unwrap();
        done(&mut app, GitOp::Pull, "git pull", true, "");
        assert_eq!(text(&app), "xtwo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_line_prefers_fatal_and_error_lines() {
        assert_eq!(error_line("  \nhint: x\nfatal: y\nerror: z"), "fatal: y");
        assert_eq!(error_line("a\n\n b "), "a");
        assert_eq!(error_line(""), "failed");
        assert_eq!(error_line("   \n\t"), "failed");
        assert_eq!(short("0123456789"), "01234567");
        assert_eq!(short("abc"), "abc");
    }
}
