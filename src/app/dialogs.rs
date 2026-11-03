//! Dialog answers.

use super::{AfterSave, App, Ctx};
use crate::event::Level;
use crate::files::ops::{FsOp, check_name};
use crate::git::model::BranchOp;
use crate::git::{BranchInfo, GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId, DialogKind, DialogResult, PathHint};
use crate::ui::focus::PanelId;

impl App {
    fn edit_log_filter(&mut self, edit: impl FnOnce(&mut crate::git::LogFilter)) {
        let mut cx = ctx!(self);
        self.git_panel.edit_log_filter(edit, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// The Ctrl+F choice in the Log tab: open that part's dialog, or clear.
    fn log_filter_kind(&mut self, i: usize) {
        if i == 4 {
            return self.edit_log_filter(|f| *f = Default::default());
        }
        let mut cx = ctx!(self);
        self.git_panel.open_log_filter_dialog(i, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// A branch or tag name typed in a dialog, or `None` (with a warning)
    /// when git would take it for an option or split it.
    fn ref_name(&mut self, text: &str, what: &str) -> Option<String> {
        let name = text.trim().to_string();
        if name.is_empty() || name.starts_with('-') || name.contains(char::is_whitespace) {
            self.notify(Level::Warn, format!("Invalid {what} name {name:?}"));
            return None;
        }
        Some(name)
    }

    /// Run one branch operation (the branches popup, a branch submenu).
    pub(super) fn branch_op(&mut self, b: BranchInfo, op: BranchOp) {
        let name = b.name.clone();
        let current = self.branch_label().unwrap_or_else(|| "HEAD".into());
        match op {
            BranchOp::Checkout => self.switch_branch(name),
            BranchOp::CheckoutTracking => self.git_request(GitRequest::CheckoutTracking(name)),
            BranchOp::NewFrom => {
                let msg = format!("Create and switch to a new branch from '{name}':");
                self.dialog = Some(Dialog::input(DialogId::NewBranchAt { rev: name }, "New Branch", &msg, "", "Create"));
            }
            BranchOp::Merge => {
                let msg = format!("Merge '{name}' into '{current}'?\nConflicts stop it halfway.");
                self.dialog = Some(Dialog::confirm(DialogId::MergeBranch { name }, "Merge", &msg, &["Merge", "Cancel"]));
            }
            BranchOp::Compare => {
                self.layout.show(crate::ui::focus::ToolWindowId::Git);
                self.focus.set(crate::ui::focus::PanelId::Git);
                let mut cx = ctx!(self);
                self.git_panel.show_log_rev(format!("HEAD..{name}"), &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            BranchOp::Push => self.git_request(GitRequest::Push),
            BranchOp::Rebase => {
                let msg =
                    format!("Rebase '{current}' onto '{name}'?\nThis rewrites the commits of '{current}'; conflicts stop it halfway.");
                self.dialog = Some(Dialog::confirm(DialogId::RebaseOnto { onto: name }, "Rebase", &msg, &["Rebase", "Cancel"]));
            }
            BranchOp::Rename => {
                let msg = format!("Rename branch '{name}' to:");
                self.dialog = Some(Dialog::input(DialogId::RenameBranch { from: name.clone() }, "Rename Branch", &msg, &name, "Rename"));
            }
            BranchOp::Delete => {
                let msg = format!("Delete branch '{name}'?");
                self.dialog =
                    Some(Dialog::confirm(DialogId::DeleteBranch { name, force: false }, "Delete Branch", &msg, &["Delete", "Cancel"]));
            }
        }
    }

    pub(super) fn confirm_delete_tag(&mut self, tag: String) {
        let msg = format!("Delete tag '{tag}'?\nThis cannot be undone.");
        self.dialog = Some(Dialog::confirm(DialogId::DeleteTag { tags: vec![tag] }, "Delete Tag", &msg, &["Delete", "Cancel"]));
    }

    pub(super) fn request_quit(&mut self) {
        if !self.quit_merge() {
            return;
        }
        if self.editor.has_dirty() {
            self.dialog = Some(Dialog::confirm(
                DialogId::QuitDirty,
                "Unsaved changes",
                "Some files have unsaved changes.",
                &["Save all and exit", "Discard and exit", "Cancel"],
            ));
        } else {
            self.should_quit = true;
        }
    }

    /// Tab in an input dialog with path completion (Save As, the worktree path): complete the
    /// path before the cursor against its folder, read here on the main
    /// thread (one `read_dir`; Save As touches the same path synchronously
    /// at Enter). The first Tab that adds nothing to an ambiguous path says
    /// how many match; the next one lists them.
    pub(super) fn complete_dialog_path(&mut self) {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let Some(d) = &mut self.dialog else { return };
        let (Some(pc), DialogKind::Input { input, .. }) = (&mut d.paths, &mut d.kind) else { return };
        let before: String = input.text.chars().take(input.cursor).collect();
        let again = std::mem::replace(&mut pc.tabbed, true);
        pc.hint = match crate::files::complete::complete(&before, &pc.base, home.as_deref(), pc.folders_only) {
            Err(e) => Some(PathHint::Note(format!("Cannot read the folder: {e}"))),
            Ok(c) if c.matches.is_empty() => Some(PathHint::Note("No match".into())),
            Ok(c) => {
                input.insert_str(&c.insert);
                match c.matches.len() {
                    1 => None,
                    _ if again && c.insert.is_empty() => Some(PathHint::List(c.matches)),
                    n => Some(PathHint::Note(format!("{n} matches; Tab again lists them"))),
                }
            }
        };
    }

    pub(crate) fn finish_dialog(&mut self, id: DialogId, res: DialogResult) {
        self.dirty_frame = true;
        if let DialogId::LspMessageRequest { server, name, id, actions } = &id {
            return self.finish_lsp_message(*server, name, id.clone(), actions, &res);
        }
        if self.finish_merge_dialog(&id, &res)
            || self.finish_recovery_dialog(&id, &res)
            || self.finish_settings_dialog(&id, &res)
            || self.finish_run_configs_dialog(&id, &res)
            || self.finish_git_ops_dialog(&id, &res)
            || self.finish_tasks_dialog(&id, &res)
        {
            return;
        }
        match (id, res) {
            // A failed save or a disk conflict keeps the app open; untitled
            // buffers are asked about one at a time (`save_as.rs`).
            (DialogId::QuitDirty, DialogResult::Button(0)) => self.save_then(AfterSave::Quit),
            (DialogId::QuitDirty, DialogResult::Button(1)) => {
                self.snapshot_discarded(None);
                self.should_quit = true;
            }
            (DialogId::CloseTabDirty(id), DialogResult::Button(0)) => self.save_then(AfterSave::CloseTab(id)),
            (DialogId::CloseTabDirty(id), DialogResult::Button(1)) => {
                self.snapshot_discarded(Some(&[id]));
                if let Some(i) = self.editor.index_of_id(id) {
                    self.editor.force_close(i);
                }
            }
            (DialogId::CloseAllDirty, DialogResult::Button(0)) => self.save_then(AfterSave::CloseAll),
            (DialogId::CloseAllDirty, DialogResult::Button(1)) => {
                self.snapshot_discarded(None);
                self.editor.force_close_all();
            }
            (DialogId::CloseTabsDirty(ids), DialogResult::Button(0)) => self.save_then(AfterSave::CloseTabs(ids)),
            (DialogId::CloseTabsDirty(ids), DialogResult::Button(1)) => {
                self.snapshot_discarded(Some(&ids));
                self.editor.force_close_ids(&ids);
            }
            (DialogId::SaveAs { id, then }, DialogResult::Text(t)) => self.finish_save_as(id, t, then),
            (DialogId::SaveAsOverwrite { id, path, then }, DialogResult::Button(0)) => self.save_as_write(id, path, then),
            (DialogId::ReplaceInFiles, DialogResult::Button(0)) => self.replace_in_files(),
            (DialogId::OverwriteExternal(id), DialogResult::Button(0)) => {
                // The outside change about to be overwritten.
                if let Some(path) = self.editor.index_of_id(id).and_then(|i| self.editor.buffers[i].path.clone()) {
                    self.capture_disk(&path, crate::history::Kind::BeforeOverwrite);
                }
                let mut cx = ctx!(self);
                self.editor.overwrite_file(id, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::OverwriteExternal(id), DialogResult::Button(1)) => {
                let mut cx = ctx!(self);
                self.editor.reload_from_disk(id, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::GotoLine, DialogResult::Text(t)) => {
                let mut it = t.trim().split(':');
                let line = it.next().and_then(|s| s.trim().parse::<usize>().ok());
                let col = it.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                match line {
                    Some(l) => self.editor.goto_line(l, col),
                    None => self.notify(Level::Warn, "Go to line: expected [line][:column]"),
                }
            }
            (DialogId::Bookmarks(targets), DialogResult::Choice(i)) => {
                if let Some(t) = targets.get(i) {
                    let mut cx = ctx!(self);
                    self.editor.goto_bookmark_target(t, &mut cx);
                    let fx = cx.effects;
                    self.apply_effects(fx);
                    self.focus.set(PanelId::Editor);
                }
            }
            (DialogId::BookmarkDigit(id, line), DialogResult::Button(d)) => {
                if let Ok(d) = u8::try_from(d) {
                    self.editor.set_bookmark_digit(id, line, d);
                }
            }
            (DialogId::LineEnding(id), DialogResult::Choice(i)) => {
                let le = if i == 0 { crate::editor::buffer::LineEnding::Lf } else { crate::editor::buffer::LineEnding::CrLf };
                self.editor.set_line_ending(id, le);
            }
            (DialogId::Indent(id), DialogResult::Choice(i)) => self.editor.set_indent(id, i),
            (DialogId::LspRename(t), DialogResult::Text(name)) => self.lsp_rename_to(t, name),
            (DialogId::TextRename(t), DialogResult::Text(name)) => self.text_rename_to(t, name),
            (DialogId::LspCodeAction(t), DialogResult::Choice(i)) => self.lsp_code_action_chosen(t, i),
            (DialogId::SurroundWith { buffer, version, prefixes }, DialogResult::Choice(i)) => {
                self.surround_chosen(buffer, version, prefixes.get(i).cloned())
            }
            (DialogId::Language(id), DialogResult::Choice(i)) => {
                let def = i.checked_sub(1).and_then(|i| crate::editor::syntax::LANGUAGES.get(i));
                self.editor.set_language(id, def, &self.theme);
            }
            (DialogId::NewBranchName, DialogResult::Text(t)) => {
                let name = t.trim().to_string();
                if name.is_empty() || name.starts_with('-') || name.contains(char::is_whitespace) {
                    self.notify(Level::Warn, format!("Invalid branch name {name:?}"));
                } else {
                    self.git.run_cli(GitOp::NewBranch(name.clone()), vec!["switch".into(), "-c".into(), name]);
                }
            }
            (DialogId::LogFilterKind, DialogResult::Choice(i)) => self.log_filter_kind(i),
            (DialogId::LogFilterText, DialogResult::Text(t)) => self.edit_log_filter(|f| f.text = t.trim().to_string()),
            (DialogId::LogFilterAuthor, DialogResult::Text(t)) => self.edit_log_filter(|f| f.author = t.trim().to_string()),
            (DialogId::LogFilterPath, DialogResult::Text(t)) => {
                let path = t.trim().trim_start_matches("./").trim_end_matches('/').to_string();
                self.edit_log_filter(|f| f.path = if path == "." { String::new() } else { path })
            }
            (DialogId::LogBranch(revs), DialogResult::Choice(i)) => {
                if let Some(rev) = revs.get(i).cloned() {
                    self.edit_log_filter(|f| f.rev = rev);
                }
            }
            (DialogId::NewBranchAt { rev }, DialogResult::Text(t)) => {
                if let Some(name) = self.ref_name(&t, "branch") {
                    self.git_request(GitRequest::NewBranchAt { name, rev });
                }
            }
            (DialogId::BranchOps { branch, ops }, DialogResult::Choice(i)) => {
                if let Some(op) = ops.get(i).copied() {
                    self.branch_op(branch, op);
                }
            }
            (DialogId::RenameBranch { from }, DialogResult::Text(t)) => {
                if let Some(to) = self.ref_name(&t, "branch")
                    && to != from
                {
                    self.git_request(GitRequest::RenameBranch { from, to });
                }
            }
            (DialogId::DeleteBranch { name, force }, DialogResult::Button(0)) => self.git_request(GitRequest::DeleteBranch { name, force }),
            (DialogId::MergeBranch { name }, DialogResult::Button(0)) => self.git_request(GitRequest::Merge(name)),
            (DialogId::RebaseOnto { onto }, DialogResult::Button(0)) => self.git_request(GitRequest::Rebase(onto)),
            (DialogId::InteractiveRebase, DialogResult::Rebase(plan)) => self.git_request(GitRequest::InteractiveRebase(plan)),
            (DialogId::NewTag { sha }, DialogResult::Text(t)) => {
                if let Some(name) = self.ref_name(&t, "tag") {
                    self.git_request(GitRequest::NewTag { name, sha });
                }
            }
            (DialogId::DeleteTag { tags }, DialogResult::Choice(i)) => {
                if let Some(tag) = tags.get(i) {
                    self.confirm_delete_tag(tag.clone());
                }
            }
            (DialogId::DeleteTag { tags }, DialogResult::Button(0)) => {
                if let Some(tag) = tags.into_iter().next() {
                    self.git_request(GitRequest::DeleteTag(tag));
                }
            }
            (DialogId::RevertCommit { sha }, DialogResult::Button(0)) => self.git_request(crate::git::GitRequest::Revert(sha)),
            (DialogId::StashMessage { untracked }, DialogResult::Text(message)) => {
                self.git_request(crate::git::GitRequest::Stash(crate::git::repo::StashOp::Push { message, untracked }))
            }
            (DialogId::StashDrop { oid }, DialogResult::Button(0)) => {
                self.git_request(crate::git::GitRequest::Stash(crate::git::repo::StashOp::Drop { oid }))
            }
            (DialogId::DiscardHunk { patch }, DialogResult::Button(0)) => {
                let op = crate::git::GitOp::Discard;
                self.git_request(crate::git::GitRequest::ApplyPatch { patch, cached: false, reverse: true, op })
            }
            (DialogId::GitDiscard(items), DialogResult::Button(0)) => self.git_request(crate::git::GitRequest::Discard(items)),
            (DialogId::PushSetUpstream { branch, remote }, DialogResult::Button(0)) => self.push_upstream(branch, remote),
            (DialogId::PushUpstreamRemote { branch, remotes }, DialogResult::Choice(i)) => {
                if let Some(remote) = remotes.get(i) {
                    self.push_upstream(branch, remote.clone());
                }
            }
            (DialogId::SwitchRootDirty(root), DialogResult::Button(0)) => self.save_then(AfterSave::SwitchRoot(root)),
            (DialogId::SwitchRootDirty(root), DialogResult::Button(1)) => {
                self.snapshot_discarded(None);
                if !self.config.worktree.switch_closes_tabs {
                    self.close_dirty_tabs();
                }
                self.do_switch_root(root);
            }
            (DialogId::TreeNewKind(dir), DialogResult::Choice(i)) => {
                self.dialog = Some(crate::files::tree::new_entry_dialog(&dir, &self.workspace.root, i == 1));
            }
            (DialogId::TreeNew { dir, is_dir }, DialogResult::Text(t)) => match check_name(&t, true) {
                Ok(()) => self.run_fs_op(FsOp::Create { path: dir.join(t.trim()), is_dir, overwrite: false }),
                Err(e) => self.notify(Level::Warn, format!("New {}: {e}", if is_dir { "directory" } else { "file" })),
            },
            (DialogId::TreeRename(path), DialogResult::Text(t)) => match check_name(&t, false) {
                Ok(()) if path.file_name().is_some_and(|n| n == t.trim()) => {}
                Ok(()) => self.run_fs_op(FsOp::Rename { to: path.with_file_name(t.trim()), from: path, overwrite: false }),
                Err(e) => self.notify(Level::Warn, format!("Rename: {e}")),
            },
            (DialogId::TreeDelete(path), DialogResult::Button(0)) => self.run_fs_op(FsOp::Delete { path, recursive: true }),
            (DialogId::NewScratch, DialogResult::Choice(i)) => self.create_scratch(i),
            (DialogId::DeleteScratch(path), DialogResult::Button(b)) => self.finish_delete_scratch(path, b == 0),
            (DialogId::DeleteScratch(path), DialogResult::Cancel) => self.finish_delete_scratch(path, false),
            (DialogId::PutLabel(path), DialogResult::Text(t)) => self.finish_put_label(path, &t),
            (DialogId::RemoveLabel { name, entries }, DialogResult::Button(b)) => self.finish_remove_label(name, entries, b == 0),
            (DialogId::RemoveLabel { name, entries }, DialogResult::Cancel) => self.finish_remove_label(name, entries, false),
            (DialogId::RevertLabel { at, name, members }, DialogResult::Button(0)) => {
                let items = members.into_iter().map(|(path, e)| (path, e.blob)).collect();
                self.history.blobs(at, name, items);
            }
            (DialogId::Info, _) => {}
            (DialogId::LspConfirmEdit(seq), DialogResult::Button(b)) => self.lsp_confirm_edit(seq, b == 0),
            (DialogId::LspConfirmEdit(seq), DialogResult::Cancel) => self.lsp_confirm_edit(seq, false),
            (_, DialogResult::Cancel) => {}
            (
                id @ (DialogId::WorktreeCreateKind
                | DialogId::WorktreeNewBranchName
                | DialogId::WorktreeExistingBranch(_)
                | DialogId::WorktreePath { .. }
                | DialogId::WorktreeSwitchAfterCreate(_)
                | DialogId::WorktreeRemove { .. }),
                res,
            ) => self.finish_worktree_dialog(id, res),
            (id, res) => tracing::debug!("dialog {id:?} -> {res:?} (no handler yet)"),
        }
    }
}
