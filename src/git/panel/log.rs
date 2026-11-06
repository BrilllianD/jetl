//! Log tab: `git log --graph` rows, details of the selected commit, and a
//! diff of a file in it.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::file_tree::{FileTree, TreeNode};
use super::{Area, Btn, GitPanel, GitTab, put, render_diff, render_diff_header};
use crate::actions::{Action, GitAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::GitRequest;
use crate::git::graph::LogGraph;
use crate::git::model::{
    BranchInfo, BranchOp, CommitDetails, CommitFile, DiffKey, DiffSpan, LogCommit, LogFileOp, LogFilter, LogRev, LogRow, ResetMode,
};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::menu::{ContextMenu, MenuItem};

/// A part of the log filter (bar buttons, Ctrl+F).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LogFilterPart {
    Text,
    Author,
    Path,
    Branch,
}

/// What to do with the selected commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommitAction {
    CopySha,
    Checkout,
    NewBranch,
    CherryPick,
    Revert,
    Reset,
    NewTag,
    DeleteTag,
    InteractiveRebase,
}
use crate::ui::widgets::list::ListState;

impl GitPanel {
    pub fn set_log(&mut self, skip: usize, filter: &LogFilter, result: anyhow::Result<Vec<LogCommit>>, cx: &mut Ctx) {
        if *filter != self.log_filter {
            // A page of a filter that was replaced meanwhile.
            return;
        }
        self.log_loading = None;
        let rows = match result {
            Ok(r) => r,
            Err(e) => {
                cx.notify(Level::Error, format!("git log: {e:#}"));
                return;
            }
        };
        let commits = rows.len();
        if skip == 0 {
            let prev = self.selected_commit().map(|c| c.to_string());
            // The graph starts over with the first page and continues
            // through the later ones.
            self.log_graph = LogGraph::default();
            self.log = self.graph_rows(rows);
            self.log_list.set_len(self.log.len());
            let again = prev.and_then(|sha| self.log.iter().position(|r| r.commit.sha == sha));
            self.log_list.select(again.unwrap_or(0));
            self.reset_log_sel();
        } else if skip == self.log.len() {
            let more = self.graph_rows(rows);
            self.log.extend(more);
            self.log_list.set_len(self.log.len());
        } else {
            return;
        }
        self.log_loaded = true;
        self.log_end = commits < self.log_page;
        self.sync_details(cx);
    }

    fn graph_rows(&mut self, commits: Vec<LogCommit>) -> Vec<LogRow> {
        commits.into_iter().map(|c| LogRow { graph: self.log_graph.row(&c.sha, &c.parents), commit: c }).collect()
    }

    fn selected_commit(&self) -> Option<&str> {
        self.log.get(self.log_list.selected).map(|r| r.commit.sha.as_str())
    }

    /// Load details of the selected commit when they are not shown yet.
    pub(super) fn sync_details(&mut self, cx: &mut Ctx) {
        let rows = self.log_selection();
        if rows.len() > 1 {
            let commits: Vec<&LogCommit> = rows.iter().map(|&i| &self.log[i].commit).collect();
            let spans = crate::git::model::commits_diff(&commits).1;
            if self.log_union.as_ref() == Some(&spans) && self.details.is_some() {
                return;
            }
            self.details = None;
            self.details_error = None;
            self.files_list = ListState::default();
            self.log_tree = Default::default();
            self.log_keys.clear();
            self.log_union = Some(spans.clone());
            cx.effects.push(Effect::Git(GitRequest::LoadCommitsFiles(spans)));
            return;
        }
        let Some(sha) = self.selected_commit().map(str::to_string) else { return };
        if self.log_union.is_none() && self.details.as_ref().is_some_and(|d| d.sha == sha) {
            return;
        }
        self.details = None;
        self.details_error = None;
        self.files_list = ListState::default();
        self.log_tree = Default::default();
        self.log_union = None;
        self.log_keys.clear();
        cx.effects.push(Effect::Git(GitRequest::LoadCommit(sha)));
    }

    /// The root row's label: the work tree's directory name.
    fn tree_root_name(&self, root: &std::path::Path) -> String {
        let work = self.snapshot.as_ref().map(|s| s.workdir.as_path()).unwrap_or(root);
        work.file_name().map_or_else(|| "/".to_string(), |n| n.to_string_lossy().into_owned())
    }

    /// The union of the files of the selected rows' `spans`; dropped unless
    /// `spans` are the current selection's.
    pub fn set_log_files(&mut self, spans: &[DiffSpan], result: anyhow::Result<Vec<(CommitFile, DiffKey)>>, root: &std::path::Path) {
        if self.log_union.as_deref() != Some(spans) {
            return;
        }
        match result {
            Ok(rows) => {
                let (files, keys): (Vec<CommitFile>, Vec<DiffKey>) = rows.into_iter().unzip();
                let mut tree = FileTree::build(&self.tree_root_name(root), &files);
                if self.details.is_some() {
                    tree.keep_state(&self.log_tree);
                } else {
                    self.files_list = ListState::default();
                }
                self.files_list.set_len(tree.rows().len());
                self.log_tree = tree;
                self.log_keys = keys;
                self.details = Some(CommitDetails { sha: String::new(), body: String::new(), files });
            }
            Err(e) => self.details_error = Some(format!("{e:#}")),
        }
    }

    pub(super) fn set_log_details(&mut self, sha: &str, result: anyhow::Result<CommitDetails>, root: &std::path::Path) {
        if self.log_union.is_some() || self.selected_commit() != Some(sha) {
            return;
        }
        match result {
            Ok(d) => {
                let mut tree = FileTree::build(&self.tree_root_name(root), &d.files);
                if self.details.as_ref().is_some_and(|old| old.sha == d.sha) {
                    tree.keep_state(&self.log_tree);
                } else {
                    self.files_list = ListState::default();
                }
                self.files_list.set_len(tree.rows().len());
                self.log_tree = tree;
                self.details = Some(d);
            }
            Err(e) => self.details_error = Some(format!("{e:#}")),
        }
    }

    /// Load the first log page, or reload it after a change.
    pub fn request_log(&mut self, cx: &mut Ctx) {
        if self.log_loading.as_ref() == Some(&self.log_filter) {
            return;
        }
        self.log_loading = Some(self.log_filter.clone());
        cx.effects.push(Effect::Git(GitRequest::LoadLog { skip: 0, filter: self.log_filter.clone() }));
    }

    /// List the history `filter` selects, from the top.
    pub(super) fn set_log_filter(&mut self, filter: LogFilter, cx: &mut Ctx) {
        if filter == self.log_filter {
            return;
        }
        self.log_filter = filter;
        self.log.clear();
        self.log_list = ListState::default();
        self.reset_log_sel();
        self.log_loaded = false;
        self.details = None;
        self.details_error = None;
        self.log_tree = Default::default();
        self.log_union = None;
        self.log_keys.clear();
        self.files_list = ListState::default();
        self.request_log(cx);
    }

    /// Show the Log tab listing the history `rev` selects.
    pub fn show_log_rev(&mut self, rev: String, cx: &mut Ctx) {
        self.tab = GitTab::Log;
        self.area = Area::Log;
        self.log_diff.clear();
        self.set_log_filter(LogFilter { rev: LogRev::Rev(rev), ..Default::default() }, cx);
    }

    /// Show `sha` in the Log tab: select it when it is listed, else list
    /// the history from it.
    pub fn show_commit(&mut self, sha: &str, cx: &mut Ctx) {
        self.tab = GitTab::Log;
        self.area = Area::Log;
        self.log_diff.clear();
        match self.log.iter().position(|r| r.commit.sha == sha) {
            Some(i) => {
                self.log_list.select(i);
                self.reset_log_sel();
                self.sync_details(cx);
            }
            None => self.set_log_filter(LogFilter { rev: LogRev::Rev(sha.to_string()), ..Default::default() }, cx),
        }
    }

    /// The Log tab with the history of `rel` (a file or directory relative
    /// to the root; empty for everything).
    pub fn show_history(&mut self, rel: &str, cx: &mut Ctx) {
        self.tab = GitTab::Log;
        self.area = Area::Log;
        self.log_diff.clear();
        self.set_log_filter(LogFilter { path: rel.to_string(), ..Default::default() }, cx);
    }

    pub fn log_filter(&self) -> &LogFilter {
        &self.log_filter
    }

    /// Change one part of the log filter (a dialog answer).
    pub fn edit_log_filter(&mut self, edit: impl FnOnce(&mut LogFilter), cx: &mut Ctx) {
        let mut f = self.log_filter.clone();
        edit(&mut f);
        self.set_log_filter(f, cx);
    }

    /// Ask for part `i` of the Ctrl+F choice (Branch, Text, Author, Path).
    pub fn open_log_filter_dialog(&mut self, i: usize, cx: &mut Ctx) {
        let part = match i {
            0 => LogFilterPart::Branch,
            1 => LogFilterPart::Text,
            2 => LogFilterPart::Author,
            _ => LogFilterPart::Path,
        };
        self.log_filter_dialog(part, cx);
    }

    /// Ask for one part of the filter.
    pub(super) fn log_filter_dialog(&mut self, part: LogFilterPart, cx: &mut Ctx) {
        let f = &self.log_filter;
        let d = match part {
            LogFilterPart::Text => {
                Dialog::input(DialogId::LogFilterText, "Filter Log", "Commits whose message contains:", &f.text, "Filter")
            }
            LogFilterPart::Author => {
                Dialog::input(DialogId::LogFilterAuthor, "Filter Log", "Commits whose author contains:", &f.author, "Filter")
            }
            LogFilterPart::Path => {
                Dialog::input(DialogId::LogFilterPath, "Filter Log", "Commits touching (path relative to the project):", &f.path, "Filter")
            }
            LogFilterPart::Branch => {
                let mut revs = vec![LogRev::Head, LogRev::All];
                revs.extend(self.branches.iter().map(|b| LogRev::Rev(b.name.clone())));
                let items = revs
                    .iter()
                    .map(|r| match r {
                        LogRev::Head => "HEAD (current branch)".to_string(),
                        LogRev::All => "All branches".to_string(),
                        r => r.label(false),
                    })
                    .collect();
                Dialog::choice(DialogId::LogBranch(revs), "Log of Branch", "", items)
            }
        };
        cx.effects.push(Effect::Dialog(d));
    }

    /// The commit a commit action works on.
    fn action_commit(&self, cx: &mut Ctx) -> Option<String> {
        let sha = (self.tab == GitTab::Log).then(|| self.selected_commit()).flatten().map(str::to_string);
        if sha.is_none() {
            cx.notify(Level::Info, "Select a commit in the Log tab");
        }
        sha
    }

    pub(super) fn commit_action(&mut self, action: CommitAction, cx: &mut Ctx) {
        let Some(sha) = self.action_commit(cx) else { return };
        let short = sha[..sha.len().min(8)].to_string();
        if matches!(
            action,
            CommitAction::Checkout
                | CommitAction::NewBranch
                | CommitAction::CherryPick
                | CommitAction::Revert
                | CommitAction::InteractiveRebase
        ) && let Some(ip) = self.in_progress().filter(|ip| ip.kind.blocks_switching())
        {
            return cx.notify(Level::Warn, ip.refusal());
        }
        match action {
            CommitAction::CopySha => {
                cx.notify(Level::Info, format!("Copied {sha}"));
                cx.effects.push(Effect::Clipboard(sha));
            }
            CommitAction::Checkout => self.checkout_commit(sha, cx),
            CommitAction::NewBranch => {
                let msg = format!("Create and switch to a new branch at {short}:");
                cx.effects.push(Effect::Dialog(Dialog::input(DialogId::NewBranchAt { rev: sha }, "New Branch", &msg, "", "Create")));
            }
            CommitAction::CherryPick => cx.effects.push(Effect::Git(GitRequest::CherryPick(sha))),
            CommitAction::NewTag => {
                let msg = format!("Tag {short} as:");
                cx.effects.push(Effect::Dialog(Dialog::input(DialogId::NewTag { sha }, "New Tag", &msg, "", "Create")));
            }
            CommitAction::DeleteTag => {
                let refs = self.log.get(self.log_list.selected).map(|r| r.commit.refs.clone()).unwrap_or_default();
                let tags: Vec<String> = refs.split(", ").filter_map(|r| r.strip_prefix("tag: ")).map(str::to_string).collect();
                let d = match tags.as_slice() {
                    [] => return cx.notify(Level::Info, format!("{short} has no tags")),
                    [tag] => {
                        let msg = format!("Delete tag '{tag}'?\nThis cannot be undone.");
                        Dialog::confirm(DialogId::DeleteTag { tags: tags.clone() }, "Delete Tag", &msg, &["Delete", "Cancel"])
                    }
                    _ => Dialog::choice(DialogId::DeleteTag { tags: tags.clone() }, "Delete Tag", "Tag to delete:", tags.clone()),
                };
                cx.effects.push(Effect::Dialog(d));
            }
            CommitAction::Revert => {
                let subject = self.log.get(self.log_list.selected).map(|r| r.commit.subject.clone()).unwrap_or_default();
                let msg = format!("Revert {short} {subject}?\nThis adds a commit that undoes it.");
                cx.effects.push(Effect::Dialog(Dialog::confirm(
                    DialogId::RevertCommit { sha },
                    "Revert Commit",
                    &msg,
                    &["Revert", "Cancel"],
                )));
            }
            CommitAction::InteractiveRebase => cx.effects.push(Effect::Git(GitRequest::LoadRebasePlan(sha))),
            CommitAction::Reset => {
                let subject = self.log.get(self.log_list.selected).map(|r| r.commit.subject.clone()).unwrap_or_default();
                let branch = self.branch_label().unwrap_or_else(|| "HEAD".into());
                let msg = format!("Reset '{branch}' to {short} {subject}.\nThe commits after it leave the branch (the reflog keeps them).");
                let items = ResetMode::ALL.iter().map(|m| m.choice_label().to_string()).collect();
                cx.effects.push(Effect::Dialog(Dialog::choice(DialogId::ResetMode { sha }, "Reset Current Branch", &msg, items)));
            }
        }
    }

    /// The selected rows, ascending: the multi-selection, else the cursor row.
    pub(crate) fn log_selection(&self) -> Vec<usize> {
        if self.log_sel.is_empty() {
            if self.log_list.selected < self.log.len() { vec![self.log_list.selected] } else { Vec::new() }
        } else {
            self.log_sel.iter().copied().filter(|&i| i < self.log.len()).collect()
        }
    }

    #[cfg(test)]
    pub(crate) fn branches_rect(&self) -> Rect {
        self.branches_rect
    }

    #[cfg(test)]
    pub(crate) fn log_rect(&self) -> Rect {
        self.log_rect
    }

    #[cfg(test)]
    pub(crate) fn log_cursor(&self) -> usize {
        self.log_list.selected
    }

    #[cfg(test)]
    pub(crate) fn log_loading(&self) -> bool {
        self.log_loading.is_some()
    }

    /// Select only the cursor row.
    fn reset_log_sel(&mut self) {
        self.log_sel.clear();
        self.log_anchor = self.log_list.selected;
    }

    /// Move the cursor to `to` and select the rows from the anchor to it.
    pub(super) fn extend_log_selection(&mut self, to: usize, cx: &mut Ctx) {
        if self.log.is_empty() {
            return;
        }
        self.log_list.select(to);
        let (lo, hi) = (self.log_anchor.min(to), self.log_anchor.max(to));
        self.log_sel = (lo..=hi.min(self.log.len() - 1)).collect();
        self.sync_details(cx);
        self.maybe_page_log(cx);
    }

    /// Toggle row `i` in the selection. A row added becomes the cursor and
    /// the anchor; a row removed hands both to the nearest selected row, and
    /// the last selected row stays (the cursor row is always selected).
    fn toggle_log_row(&mut self, i: usize, cx: &mut Ctx) {
        if self.log_sel.is_empty() && self.log_list.selected < self.log.len() {
            self.log_sel.insert(self.log_list.selected);
        }
        let to = if self.log_sel.contains(&i) {
            if self.log_sel.len() == 1 {
                return;
            }
            self.log_sel.remove(&i);
            self.log_sel.iter().copied().min_by_key(|&r| r.abs_diff(i)).unwrap_or(i)
        } else {
            self.log_sel.insert(i);
            i
        };
        self.log_list.select(to);
        self.log_anchor = to;
        self.sync_details(cx);
        self.maybe_page_log(cx);
    }

    /// Title and per-commit spans of the selected commits, or a notice when none is selected.
    fn selected_commit_spans(&self, cx: &mut Ctx) -> Option<(String, Vec<crate::git::model::DiffSpan>)> {
        let rows = self.log_selection();
        if self.tab != GitTab::Log || rows.is_empty() {
            cx.notify(Level::Info, "Select a commit in the Log tab");
            return None;
        }
        let commits: Vec<&LogCommit> = rows.iter().map(|&i| &self.log[i].commit).collect();
        Some(crate::git::model::commits_diff(&commits))
    }

    /// Open the files of the selected commits side by side in one editor diff tab.
    pub(super) fn show_commits_diff(&mut self, cx: &mut Ctx) {
        if let Some((title, spans)) = self.selected_commit_spans(cx) {
            cx.effects.push(Effect::Git(GitRequest::CommitsFilesTab { title, spans }));
        }
    }

    /// Open the combined diff of the selected commits as a unified patch in an editor tab.
    pub(super) fn show_commits_patch(&mut self, cx: &mut Ctx) {
        if let Some((title, spans)) = self.selected_commit_spans(cx) {
            cx.effects.push(Effect::Git(GitRequest::CommitsDiff { title: title.replacen("Diff", "Patch", 1), spans }));
        }
    }

    /// Where a menu for the selected Log row opens.
    fn log_row_anchor(&self) -> (u16, u16) {
        let row = self.log_list.selected.saturating_sub(self.log_list.offset) as u16;
        (self.log_rect.x + 4, self.log_rect.y + row)
    }

    /// Checkout Revision: the branch at the selected commit when one can take
    /// it, a short menu for several, else a detached HEAD.
    fn checkout_commit(&self, sha: String, cx: &mut Ctx) {
        let refs = self.log.get(self.log_list.selected).map_or("", |r| r.commit.refs.as_str());
        let (found, clash) = checkout_candidates(refs, &self.branches);
        let op = |b: &BranchInfo| if b.remote { BranchOp::CheckoutTracking } else { BranchOp::Checkout };
        match found.as_slice() {
            [] => {
                if let Some(name) = clash {
                    cx.notify(Level::Info, format!("Local branch '{name}' is at another commit, so HEAD detaches"));
                }
                cx.effects.push(Effect::Git(GitRequest::CheckoutCommit(sha)));
            }
            [b] => cx.effects.push(Effect::Git(GitRequest::BranchOp { branch: (*b).clone(), op: op(b) })),
            several => {
                let mut several = several.to_vec();
                several.sort_by(|a, b| a.name.cmp(&b.name));
                let items = several
                    .iter()
                    .map(|b| {
                        let label = if b.remote {
                            format!("Checkout '{}' as a new local branch", b.name)
                        } else {
                            format!("Checkout '{}'", b.name)
                        };
                        MenuItem::new(label, Action::Git(GitAction::BranchOp { branch: b.name.clone(), op: op(b) }))
                    })
                    .collect();
                let (x, y) = self.log_row_anchor();
                cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, x, y)));
            }
        }
    }

    /// Commit actions for the selected commit, at the pointer or its row.
    pub(super) fn log_context_menu(&mut self, at: Option<(u16, u16)>, cx: &mut Ctx) {
        if self.tab != GitTab::Log || self.selected_commit().is_none() {
            return;
        }
        let (x, y) = at.unwrap_or_else(|| self.log_row_anchor());
        let g = |a| Action::Git(a);
        let busy = self.in_progress().is_some_and(|ip| ip.kind.blocks_switching());
        let mut items = vec![
            MenuItem::new("Copy Revision Number", g(GitAction::CopySha)),
            MenuItem::new("Show Diff", g(GitAction::ShowDiff)),
            MenuItem::new("Show Diff as Patch", g(GitAction::LogShowPatch)),
            MenuItem::new("Checkout Revision", g(GitAction::LogCheckout)).enabled(!busy),
            MenuItem::new("New Branch…", g(GitAction::LogNewBranch)).enabled(!busy),
            MenuItem::new("Cherry-Pick", g(GitAction::LogCherryPick)).enabled(!busy),
            MenuItem::new("Revert Commit…", g(GitAction::LogRevert)).enabled(!busy),
            MenuItem::new("Reset Current Branch to Here…", g(GitAction::LogReset)),
            MenuItem::new("Interactively Rebase from Here…", g(GitAction::LogInteractiveRebase)).enabled(!busy),
            MenuItem::new("New Tag…", g(GitAction::LogNewTag)),
            MenuItem::new("Delete Tag…", g(GitAction::LogDeleteTag)),
            MenuItem::new("Filter Log…", g(GitAction::LogFilter)),
        ];
        let at = self.log.get(self.log_list.selected).map(|r| branches_at(&r.commit.refs)).unwrap_or_default();
        let rows: Vec<MenuItem> = at
            .iter()
            .filter_map(|name| self.branches.iter().find(|b| b.name == *name))
            .map(|b| MenuItem::submenu(format!("Branch '{}'", b.name), self.branch_op_items(b)))
            .collect();
        if !rows.is_empty() {
            items.push(MenuItem::separator());
            items.extend(rows);
        }
        cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, x, y)));
    }

    /// Fetch the next page when the selection or view reached the end.
    pub(super) fn maybe_page_log(&mut self, cx: &mut Ctx) {
        let at_end = self.log_list.len > 0
            && (self.log_list.selected + 1 >= self.log_list.len || self.log_list.offset + self.log_list.viewport >= self.log_list.len);
        if at_end && self.log_loaded && !self.log_end && self.log_loading.is_none() {
            self.log_loading = Some(self.log_filter.clone());
            cx.effects.push(Effect::Git(GitRequest::LoadLog { skip: self.log.len(), filter: self.log_filter.clone() }));
        }
    }

    /// The selected row of the details' file tree.
    pub(super) fn log_node(&self) -> Option<&TreeNode> {
        self.details.as_ref()?;
        self.log_tree.rows().get(self.files_list.selected).map(|(_, n)| n)
    }

    /// The diff key of the selected tree row when it is a file; `None` on the
    /// root or a folder.
    pub(crate) fn log_file_key(&self) -> Option<DiffKey> {
        let d = self.details.as_ref()?;
        let Some(TreeNode::File(i)) = self.log_node() else { return None };
        if self.log_union.is_some() {
            return self.log_keys.get(*i).cloned();
        }
        let f = d.files.get(*i)?;
        Some(DiffKey::Commit { sha: d.sha.clone(), rel: f.path.clone(), old: f.old.clone() })
    }

    /// The work-tree-relative path of the selected file or folder.
    fn log_node_rel(&self) -> Option<String> {
        match self.log_node()? {
            TreeNode::File(i) => self.details.as_ref()?.files.get(*i).map(|f| f.path.clone()),
            TreeNode::Dir { path, .. } => Some(path.clone()),
            TreeNode::Root => None,
        }
    }

    fn workdir(&self, cx: &Ctx) -> PathBuf {
        self.snapshot.as_ref().map(|s| s.workdir.clone()).unwrap_or_else(|| cx.workspace.root.clone())
    }

    pub(super) fn open_commit_file(&mut self, cx: &mut Ctx) {
        let Some(key) = self.log_file_key() else { return };
        self.log_diff.request(key, cx);
        self.area = Area::LogDiff;
    }

    fn toggle_log_node(&mut self, row: usize) {
        self.log_tree.toggle(row);
        self.files_list.set_len(self.log_tree.rows().len());
    }

    pub(super) fn files_right(&mut self, cx: &mut Ctx) {
        let sel = self.files_list.selected;
        match self.log_node() {
            Some(TreeNode::File(_)) => self.open_commit_file(cx),
            Some(TreeNode::Dir { open: false, .. }) => self.toggle_log_node(sel),
            Some(_) => self.files_list.down(1),
            None => {}
        }
    }

    pub(super) fn files_left(&mut self) {
        let sel = self.files_list.selected;
        match self.log_node() {
            Some(TreeNode::Dir { open: true, .. }) => self.toggle_log_node(sel),
            _ => self.area = Area::Log,
        }
    }

    pub(super) fn files_enter(&mut self, cx: &mut Ctx) {
        let sel = self.files_list.selected;
        match self.log_node() {
            Some(TreeNode::File(_)) => self.open_commit_file(cx),
            Some(TreeNode::Dir { .. }) => self.toggle_log_node(sel),
            _ => {}
        }
    }

    /// Actions for the selected node of the details' file tree, at the
    /// pointer or its row.
    pub(super) fn log_file_menu(&mut self, at: Option<(u16, u16)>, cx: &mut Ctx) {
        if self.tab != GitTab::Log {
            return;
        }
        let Some(node) = self.log_node().cloned() else { return };
        let (x, y) = at.unwrap_or_else(|| {
            let row = self.files_list.selected.saturating_sub(self.files_list.offset) as u16;
            (self.files_rect.x + 2, self.files_rect.y + row)
        });
        let busy = self.in_progress().is_some_and(|ip| ip.kind.blocks_switching());
        let f = |op| Action::Git(GitAction::LogFileOp(op));
        let g = |a| Action::Git(a);
        let mut items = match &node {
            TreeNode::File(_) => {
                let present = self.log_node_rel().is_some_and(|rel| self.workdir(cx).join(rel).is_file());
                vec![
                    MenuItem::new("Show Diff", f(LogFileOp::ShowDiff)),
                    MenuItem::new("Open Diff in Editor Tab", f(LogFileOp::OpenDiffTab)),
                    MenuItem::new("Open in Editor", f(LogFileOp::OpenInEditor)).enabled(present),
                    MenuItem::new("Copy Path", f(LogFileOp::CopyPath)),
                    MenuItem::new("Show History for File", f(LogFileOp::ShowHistory)),
                ]
            }
            TreeNode::Dir { .. } => vec![
                MenuItem::new("Expand All", f(LogFileOp::ExpandAll)),
                MenuItem::new("Collapse All", f(LogFileOp::CollapseAll)),
                MenuItem::new("Copy Path", f(LogFileOp::CopyPath)),
                MenuItem::new("Show History for Folder", f(LogFileOp::ShowHistory)),
            ],
            TreeNode::Root => {
                vec![MenuItem::new("Expand All", f(LogFileOp::ExpandAll)), MenuItem::new("Collapse All", f(LogFileOp::CollapseAll))]
            }
        };
        // The commit items act on the cursor commit only.
        if self.log_union.is_none() {
            items.push(MenuItem::separator());
            items.push(MenuItem::new("Copy Revision Number", g(GitAction::CopySha)));
            items.push(MenuItem::new("Cherry-Pick", g(GitAction::LogCherryPick)).enabled(!busy));
            items.push(MenuItem::new("Revert Commit…", g(GitAction::LogRevert)).enabled(!busy));
            items.push(MenuItem::new("Checkout Revision", g(GitAction::LogCheckout)).enabled(!busy));
        }
        cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, x, y)));
    }

    pub(super) fn log_file_op(&mut self, op: LogFileOp, cx: &mut Ctx) {
        match op {
            LogFileOp::ShowDiff => self.open_commit_file(cx),
            LogFileOp::OpenDiffTab => {
                if let Some(key) = self.log_file_key() {
                    self.push_diff_tab(key, cx)
                }
            }
            LogFileOp::ExpandAll | LogFileOp::CollapseAll => {
                self.log_tree.set_all(op == LogFileOp::ExpandAll);
                self.files_list.set_len(self.log_tree.rows().len());
            }
            LogFileOp::OpenInEditor | LogFileOp::CopyPath | LogFileOp::ShowHistory => {
                let Some(rel) = self.log_node_rel() else { return };
                let abs = self.workdir(cx).join(&rel);
                match op {
                    LogFileOp::OpenInEditor if abs.is_file() => cx.open_file(abs),
                    LogFileOp::OpenInEditor => cx.notify(Level::Info, format!("{rel} is not in the working tree")),
                    LogFileOp::CopyPath => {
                        cx.notify(Level::Info, format!("Copied {}", abs.display()));
                        cx.effects.push(Effect::Clipboard(abs.display().to_string()));
                    }
                    _ => match abs.strip_prefix(&cx.workspace.root) {
                        Ok(p) => {
                            let p = p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                            self.show_history(&p, cx);
                        }
                        Err(_) => cx.notify(Level::Warn, format!("{} is outside the project", abs.display())),
                    },
                }
            }
        }
    }

    pub(super) fn close_log_diff(&mut self) {
        self.log_diff.clear();
        self.area = Area::Files;
    }

    pub(super) fn click_log_tab(&mut self, m: &Mouse, p: Position, cx: &mut Ctx) {
        if self.log_diff.key.is_some() {
            if self.diff_rect.contains(p) {
                self.area = Area::LogDiff;
                self.log_diff.click(m.y, false);
            }
            return;
        }
        if self.click_branches(m, cx) {
            return;
        }
        use crossterm::event::{KeyModifiers as K, MouseButton, MouseEventKind};
        let right = m.kind == MouseEventKind::Down(MouseButton::Right);
        if let Some(i) = self.log_list.hit(self.log_rect, m.x, m.y) {
            self.area = Area::Log;
            if !right && m.mods.contains(K::SHIFT) {
                self.extend_log_selection(i, cx);
                return;
            }
            if !right && m.mods.contains(K::CONTROL) {
                self.toggle_log_row(i, cx);
                return;
            }
            if right && !self.log_sel.is_empty() && self.log_selection().contains(&i) {
                self.log_list.select(i);
            } else {
                self.log_list.select(i);
                self.reset_log_sel();
            }
            self.sync_details(cx);
            self.maybe_page_log(cx);
            if right {
                self.log_context_menu(Some((m.x, m.y)), cx);
            }
        } else if let Some(i) = self.files_list.hit(self.files_rect, m.x, m.y) {
            self.area = Area::Files;
            self.files_list.select(i);
            if right {
                self.log_file_menu(Some((m.x, m.y)), cx);
            } else {
                match self.log_node() {
                    Some(TreeNode::File(_)) => self.open_commit_file(cx),
                    Some(TreeNode::Dir { .. }) => self.toggle_log_node(i),
                    _ => {}
                }
            }
        } else if self.log_rect.contains(p) {
            self.area = Area::Log;
        } else if self.files_rect.contains(p) {
            self.area = Area::Files;
        }
    }

    pub(super) fn render_log_tab(&mut self, frame: &mut Frame, body: Rect, focused: bool, theme: &Theme) {
        let buf = frame.buffer_mut();
        if let Some(key) = self.log_diff.key.clone() {
            let mut x = body.x;
            let r = put(buf, &mut x, body.y, body.right(), "[← Back]", theme.title.bg(theme.panel_bg));
            self.hits.push((Btn::Back, r));
            let header = match &key {
                DiffKey::Commit { sha, .. } => format!(" {}  {}", &sha[..sha.len().min(8)], key.label()),
                DiffKey::Range { base, top, .. } => {
                    format!(" {}..{}  {}", base.as_deref().map_or("root", |b| &b[..b.len().min(8)]), &top[..top.len().min(8)], key.label())
                }
                DiffKey::Change { .. } | DiffKey::Local { .. } | DiffKey::Text { .. } => format!(" {}", key.label()),
            };
            let opts = self.diff_opts;
            let rest = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
            self.diff_rect = rest;
            // First: it picks the layout the toggles show.
            render_diff(buf, rest, &mut self.log_diff, theme, opts);
            let toggles = Some((self.log_diff.unified, opts));
            let hits = render_diff_header(buf, (x, body.y, body.right()), &header, theme.title, toggles, theme);
            self.hits.extend(hits.into_iter().map(|(t, r)| (Btn::Diff(t), r)));
            return;
        }
        // The filter bar: each part opens its dialog.
        let f = &self.log_filter;
        let any = |s: &str| if s.is_empty() { "any".to_string() } else { s.to_string() };
        let parts = [
            (Btn::LogFilter(LogFilterPart::Branch), format!("[Branch: {}]", f.rev.label(self.log_all))),
            (Btn::LogFilter(LogFilterPart::Text), format!("[Text: {}]", any(&f.text))),
            (Btn::LogFilter(LogFilterPart::Author), format!("[Author: {}]", any(&f.author))),
            (Btn::LogFilter(LogFilterPart::Path), format!("[Path: {}]", any(&f.path))),
        ];
        let filtered = !f.is_empty();
        let mut x = body.x;
        for (b, label) in parts {
            let r = put(buf, &mut x, body.y, body.right(), &label, theme.title);
            self.hits.push((b, r));
            x += 1;
        }
        if filtered {
            let r = put(buf, &mut x, body.y, body.right(), "[× Clear]", theme.warn);
            self.hits.push((Btn::LogClearFilter, r));
        }
        let body = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
        let pw = super::branches::pane_width(body.width, !self.branches.is_empty());
        let body = if pw > 0 {
            self.render_branches(buf, Rect::new(body.x, body.y, pw, body.height), focused && self.area == Area::Branches, theme);
            for y in body.y..body.bottom() {
                buf[(body.x + pw, y)].set_symbol("│").set_style(theme.splitter);
            }
            Rect::new(body.x + pw + 1, body.y, body.width - pw - 1, body.height)
        } else {
            self.branches_rect = Rect::default();
            if self.area == Area::Branches {
                self.area = Area::Log;
            }
            body
        };
        let left_w = if body.width >= 70 { body.width * 3 / 5 } else { body.width };
        self.log_rect = Rect::new(body.x, body.y, left_w, body.height);
        self.render_log_list(buf, self.log_rect, focused && self.area == Area::Log, theme);
        if body.width > left_w + 1 {
            let sep_x = body.x + left_w;
            for y in body.y..body.bottom() {
                buf[(sep_x, y)].set_symbol("│").set_style(theme.splitter);
            }
            let right = Rect::new(sep_x + 1, body.y, body.right() - sep_x - 1, body.height);
            self.render_details(buf, right, focused && self.area == Area::Files, theme);
        }
    }

    fn render_log_list(&mut self, buf: &mut Screen, r: Rect, active: bool, theme: &Theme) {
        self.log_list.set_viewport(r.height as usize);
        if self.log.is_empty() {
            let msg = match (self.log_loading.is_some(), self.log_filter.is_empty()) {
                (true, _) => "Loading…",
                (false, true) => "No commits yet",
                (false, false) => "No commits match the filter",
            };
            buf.set_stringn(r.x, r.y, msg, r.width as usize, theme.dim);
            return;
        }
        // Graph lane colors, from theme keys so they follow the theme and the
        // 256-color fallback.
        let fg = |s: Style| s.fg.unwrap_or(theme.fg);
        let syn = |n: &str| theme.capture_style(n).and_then(|s| s.fg).unwrap_or(theme.fg);
        let palette =
            [fg(theme.git_modified), fg(theme.git_added), fg(theme.git_untracked), syn("constant"), syn("attribute"), syn("label")];
        let right_text = |c: &LogCommit| if c.author.is_empty() { String::new() } else { format!(" {}, {}", c.author, c.date) };
        let (mut author_col, mut lead) = (0u16, 0u16);
        for i in self.log_list.visible() {
            let row = &self.log[i];
            author_col = author_col.max(right_text(&row.commit).width() as u16);
            lead = lead.max(row.graph.chars().count() as u16 + 1 + row.commit.short.width() as u16 + 1);
        }
        let rest = r.width.saturating_sub(lead);
        let show_author = author_col > 0 && author_fits(rest, author_col);
        let col_right = if show_author { r.right() - author_col } else { r.right() };
        for (vi, i) in self.log_list.visible().enumerate() {
            let row = &self.log[i];
            let y = r.y + vi as u16;
            let cursor = i == self.log_list.selected;
            let marked = self.log_sel.contains(&i);
            let selected = cursor || marked;
            let base = if cursor {
                if active { theme.list_selected } else { theme.list_selected_unfocused }
            } else if marked {
                theme.list_selected_unfocused
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let mut x = r.x;
            // Two columns per lane; each lane keeps its color.
            for (col, ch) in row.graph.chars().enumerate() {
                let color = palette[(col / 2) % palette.len()];
                let st = if selected { base } else { Style::default().fg(color) };
                let mut tmp = [0u8; 4];
                put(buf, &mut x, y, r.right(), ch.encode_utf8(&mut tmp), st);
            }
            x += 1;
            let c = &row.commit;
            put(buf, &mut x, y, r.right(), &c.short, if selected { base } else { theme.warn });
            x += 1;
            let room = col_right.saturating_sub(x);
            // Refs take what the subject leaves, and at least `refs_cap`.
            let subject_w = c.subject.width() as u16;
            let cap = refs_cap(room).max(room.saturating_sub(subject_w + 1));
            let labels = if cap < 4 { String::new() } else { ref_labels(&c.refs, &self.branches, cap as usize) };
            let refs_w = labels.width() as u16;
            let refs_x = col_right - refs_w;
            let subject_end = if refs_w > 0 { refs_x - 1 } else { col_right };
            let subject = crate::util::text::fit(&c.subject, subject_end.saturating_sub(x) as usize);
            put(buf, &mut x, y, subject_end, &subject, base);
            if refs_w > 0 {
                let mut rx = refs_x;
                let st = if selected { base.add_modifier(Modifier::BOLD) } else { theme.status_accent.add_modifier(Modifier::BOLD) };
                put(buf, &mut rx, y, col_right, &labels, st);
            }
            if show_author {
                let right = right_text(c);
                let mut rx = r.right() - right.width() as u16;
                put(buf, &mut rx, y, r.right(), &right, if selected { base } else { theme.dim });
            }
        }
    }

    fn render_details(&mut self, buf: &mut Screen, r: Rect, active: bool, theme: &Theme) {
        if r.height == 0 || r.width < 4 {
            return;
        }
        let x0 = r.x + 1;
        let w = r.width.saturating_sub(1) as usize;
        let icons = self.file_icons;
        if let Some(e) = &self.details_error {
            buf.set_stringn(x0, r.y, crate::util::text::fit(e, w), w, theme.error);
            return;
        }
        let Some(d) = &self.details else {
            if self.selected_commit().is_some() {
                buf.set_stringn(x0, r.y, "Loading…", w, theme.dim);
            }
            return;
        };
        let mut y = r.y;
        // Message (or the selected commits) up to half the height, then the file list.
        let max_body = (r.height as usize / 2).saturating_sub(1).max(1);
        if let Some(spans) = &self.log_union {
            let rows = self.log_selection();
            let gap = if spans.len() > 1 { " (not contiguous)" } else { "" };
            buf.set_stringn(x0, y, format!("{} commits selected{gap}", rows.len()), w, theme.dim);
            y += 1;
            let shown = if rows.len() > max_body { max_body.saturating_sub(1) } else { rows.len() };
            for &i in rows.iter().take(shown) {
                if y >= r.bottom() {
                    return;
                }
                let Some(row) = self.log.get(i) else { continue };
                let mut x = x0;
                let end = x0 + w as u16;
                put(buf, &mut x, y, end, &row.commit.short, theme.warn);
                put(buf, &mut x, y, end, " ", Style::default());
                put(buf, &mut x, y, end, &row.commit.subject, Style::default().fg(theme.fg));
                y += 1;
            }
            if shown < rows.len() && y < r.bottom() {
                buf.set_stringn(x0, y, format!("… {} more", rows.len() - shown), w, theme.dim);
                y += 1;
            }
        } else {
            let commit = self.log.get(self.log_list.selected).map(|row| &row.commit);
            if let Some(c) = commit {
                buf.set_stringn(x0, y, format!("{}  {}, {}", c.short, c.author, c.date), w, theme.title);
                y += 1;
            }
            for line in d.body.lines().take(max_body) {
                if y >= r.bottom() {
                    return;
                }
                buf.set_stringn(x0, y, line, w, Style::default().fg(theme.fg));
                y += 1;
            }
        }
        if y >= r.bottom() {
            return;
        }
        buf.set_stringn(x0, y, format!("{} file{} changed", d.files.len(), if d.files.len() == 1 { "" } else { "s" }), w, theme.dim);
        y += 1;
        self.files_rect = Rect::new(r.x, y, r.width, r.bottom().saturating_sub(y));
        self.files_list.set_viewport(self.files_rect.height as usize);
        for (vi, i) in self.files_list.visible().enumerate() {
            let fy = self.files_rect.y + vi as u16;
            let Some((depth, node)) = self.log_tree.rows().get(i) else { break };
            let label = self.log_tree.label(i);
            let selected = i == self.files_list.selected && active;
            let base = if selected { theme.list_selected } else { Style::default().fg(theme.fg) };
            if selected {
                buf.set_style(Rect::new(r.x, fy, r.width, 1), base);
            }
            let mut x = x0 + depth * 2;
            if x >= r.right() {
                continue;
            }
            let dim = if selected { base } else { theme.dim };
            match node {
                TreeNode::Root | TreeNode::Dir { .. } => {
                    let open = !matches!(node, TreeNode::Dir { open: false, .. });
                    put(buf, &mut x, fy, r.right(), if open { "▾ " } else { "▸ " }, dim);
                    if let Some((icon, st)) = crate::ui::file_icons::span(label, true, icons, base, theme) {
                        put(buf, &mut x, fy, r.right(), &icon, st);
                    }
                    put(buf, &mut x, fy, r.right(), label, base.add_modifier(Modifier::BOLD));
                }
                TreeNode::File(fi) => {
                    let Some(f) = d.files.get(*fi) else { continue };
                    let st_style = match f.status {
                        'A' => theme.git_added,
                        'D' => theme.git_deleted,
                        _ => theme.git_modified,
                    };
                    put(buf, &mut x, fy, r.right(), &f.status.to_string(), if selected { base } else { st_style });
                    x += 1;
                    if let Some((icon, st)) = crate::ui::file_icons::span(label, false, icons, base, theme) {
                        put(buf, &mut x, fy, r.right(), &icon, st);
                    }
                    put(buf, &mut x, fy, r.right(), label, base);
                    if let Some(old) = &f.old {
                        put(buf, &mut x, fy, r.right(), &format!(" ← {old}"), dim);
                    }
                }
            }
        }
    }
}

/// The branch names in a commit's decoration (`LogCommit::refs`): no `HEAD`,
/// tags or remote `HEAD` pointers.
fn branches_at(refs: &str) -> Vec<String> {
    refs.split(", ")
        .map(|r| r.strip_prefix("HEAD -> ").unwrap_or(r))
        .filter(|r| !r.is_empty() && *r != "HEAD" && !r.starts_with("tag: ") && !r.ends_with("/HEAD"))
        .map(str::to_string)
        .collect()
}

/// Cut at this width a subject is not worth squeezing for the refs or the author.
const LOG_SUBJECT_MIN: u16 = 10;
/// The room the refs column keeps before the author and date may take space.
const LOG_REFS_ROOM: u16 = 16;

/// The refs of a commit as the Log column shows them, at most `cap` columns:
/// detached `HEAD`, the current branch (marked `* `), other local branches,
/// remote-only branches, then tags in `%D` order. Local branches and remotes go
/// by name; a remote merged into the local one of the same name at this commit
/// reads `origin & main`; `<remote>/HEAD` is dropped. When the full text is
/// wider than `cap`, the merged remote prefixes go before the end is cut, so
/// the branch Checkout Revision takes survives whenever it fits.
fn ref_labels(refs: &str, branches: &[BranchInfo], cap: usize) -> String {
    let full = ref_labels_with(refs, branches, true);
    let text = if full.width() > cap { ref_labels_with(refs, branches, false) } else { full };
    crate::util::text::fit(&text, cap)
}

fn ref_labels_with(refs: &str, branches: &[BranchInfo], merge_remotes: bool) -> String {
    let items: Vec<&str> = refs.split(", ").filter(|r| !r.is_empty() && !r.ends_with("/HEAD")).collect();
    let is_remote = |r: &str| branches.iter().any(|b| b.remote && b.name == r);
    let local_of = |r: &str| r.split_once('/').map_or_else(|| r.to_string(), |(_, l)| l.to_string());
    let is_tag = |r: &str| r.starts_with("tag: ");
    let bare = |r: &'_ str| r.strip_prefix("HEAD -> ").unwrap_or(r).to_string();
    // A name is a local branch here when it is not a remote one, a tag or a bare HEAD.
    let is_local = |r: &str| r != "HEAD" && !is_tag(r) && !is_remote(r);
    let local_at = |name: &str| items.iter().any(|r| bare(r) == name && is_local(name));
    let merges = |r: &str| is_remote(r) && local_at(&local_of(r));
    let (mut head, mut current, mut locals, mut remotes, mut tags) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for r in &items {
        let name = bare(r);
        if merges(r) {
            continue;
        }
        let is_current = r.starts_with("HEAD -> ");
        let mut label = String::new();
        if is_current {
            label.push_str("* ");
        }
        if merge_remotes && is_local(&name) {
            let mut ms: Vec<&&str> = items.iter().filter(|m| merges(m) && local_of(m) == name).collect();
            ms.sort();
            for m in ms {
                label.push_str(m.split_once('/').map_or(m, |(remote, _)| remote));
                label.push_str(" & ");
            }
        }
        label.push_str(&name);
        if name == "HEAD" {
            head.push((name, label));
        } else if is_tag(&name) {
            tags.push((name, label));
        } else if is_current {
            current.push((name, label));
        } else if is_remote(&name) {
            remotes.push((name, label));
        } else {
            locals.push((name, label));
        }
    }
    locals.sort();
    remotes.sort();
    let all = head.into_iter().chain(current).chain(locals).chain(remotes).chain(tags);
    all.map(|(_, l)| l).collect::<Vec<_>>().join(", ")
}

/// Whether the author and date fit beside a subject and the refs: `rest` is
/// the width left of the widest graph and hash, `author_col` the widest
/// author and date text.
fn author_fits(rest: u16, author_col: u16) -> bool {
    rest >= author_col + LOG_SUBJECT_MIN + LOG_REFS_ROOM
}

/// The width the refs column may take in `room` columns after the hash: the
/// subject keeps `LOG_SUBJECT_MIN`, refs take at most half, 0 for none.
fn refs_cap(room: u16) -> u16 {
    let cap = room.saturating_sub(LOG_SUBJECT_MIN + 1).min((room / 2).max(LOG_REFS_ROOM));
    if cap < 4 { 0 } else { cap }
}

/// The branches Checkout Revision can take at a commit, and the local name
/// that kept a remote out: the current branch, else the local branches there,
/// else the remote ones whose local name does not exist.
fn checkout_candidates<'a>(refs: &str, branches: &'a [BranchInfo]) -> (Vec<&'a BranchInfo>, Option<&'a str>) {
    let at: Vec<&BranchInfo> = branches_at(refs).iter().filter_map(|n| branches.iter().find(|b| b.name == *n)).collect();
    if let Some(cur) = at.iter().find(|b| b.current && !b.remote) {
        return (vec![*cur], None);
    }
    let local: Vec<&BranchInfo> = at.iter().copied().filter(|b| !b.remote).collect();
    if !local.is_empty() {
        return (local, None);
    }
    let local_name = |b: &'a BranchInfo| b.name.split_once('/').map_or(b.name.as_str(), |(_, l)| l);
    let (taken, free): (Vec<&BranchInfo>, Vec<&BranchInfo>) =
        at.into_iter().partition(|b| branches.iter().any(|o| !o.remote && o.name == local_name(b)));
    let clash = if free.is_empty() { taken.first().map(|b| local_name(b)) } else { None };
    (free, clash)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{author_fits, branches_at, checkout_candidates, ref_labels, refs_cap};
    use crate::actions::{Action, GitAction};
    use crate::app::Ctx;
    use crate::config::Config;
    use crate::config::theme::Theme;
    use crate::event::{Effect, Level};
    use crate::git::GitRequest;
    use crate::git::model::{
        BranchInfo, BranchOp, CommitDetails, CommitFile, DiffKey, InProgress, InProgressKind, LogCommit, LogFileOp, LogRow, StatusSnapshot,
    };
    use crate::git::panel::{Area, GitPanel, GitTab};
    use crate::workspace::Workspace;

    #[test]
    fn branches_at_keeps_only_branches() {
        assert_eq!(branches_at("HEAD -> main, origin/main, origin/HEAD, tag: v1, dev"), ["main", "origin/main", "dev"]);
        assert!(branches_at("HEAD").is_empty());
        assert!(branches_at("").is_empty());
    }

    fn with_cx<T>(f: impl FnOnce(&mut Ctx) -> T) -> Vec<Effect> {
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(PathBuf::from("/tmp/root"));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        let _ = f(&mut cx);
        cx.effects
    }

    fn file(status: char, path: &str) -> CommitFile {
        CommitFile { status, path: path.to_string(), old: None }
    }

    /// Rows: 0 root, 1 docs, 2 a.md, 3 src, 4 git/panel, 5 log.rs, 6 mod.rs, 7 main.rs, 8 README.md.
    fn panel() -> GitPanel {
        let mut p = GitPanel::new();
        p.tab = GitTab::Log;
        let sha = "a".repeat(40);
        let commit = LogCommit {
            sha: sha.clone(),
            parents: Vec::new(),
            short: "aaaaaaa".into(),
            author: "me".into(),
            date: "now".into(),
            refs: String::new(),
            subject: "subject".into(),
        };
        p.log.push(LogRow { graph: "●".into(), commit });
        p.log_list.set_len(1);
        let files = vec![
            file('M', "src/git/panel/log.rs"),
            file('M', "src/git/panel/mod.rs"),
            file('M', "src/main.rs"),
            file('A', "README.md"),
            file('A', "docs/a.md"),
        ];
        p.set_log_details(&sha, Ok(CommitDetails { sha: sha.clone(), body: String::new(), files }), Path::new("/tmp/root"));
        p
    }

    fn labels(effects: &[Effect]) -> Vec<(String, bool)> {
        let Some(Effect::ContextMenu(m)) = effects.first() else { panic!("no menu: {effects:?}") };
        m.items.iter().map(|i| (i.label.clone(), i.enabled)).collect()
    }

    const COMMIT_ITEMS: [&str; 4] = ["Copy Revision Number", "Cherry-Pick", "Revert Commit…", "Checkout Revision"];

    fn branch(name: &str, current: bool, remote: bool) -> BranchInfo {
        BranchInfo { name: name.into(), upstream: None, ahead: 0, behind: 0, current, remote }
    }

    fn checkout_branches() -> Vec<BranchInfo> {
        let mut v: Vec<BranchInfo> = ["main", "dev", "left", "right", "clash"].iter().map(|n| branch(n, *n == "main", false)).collect();
        v.extend(["origin/main", "origin/dev", "origin/only", "upstream/only", "origin/clash"].iter().map(|n| branch(n, false, true)));
        v
    }

    #[test]
    fn checkout_candidates_follow_the_rules() {
        let branches = checkout_branches();
        let cases: [(&str, &[&str], Option<&str>); 11] = [
            ("dev, origin/dev", &["dev"], None),
            ("HEAD -> main, origin/main, dev", &["main"], None),
            ("HEAD, dev", &["dev"], None),
            ("dev, origin/only", &["dev"], None),
            ("left, right", &["left", "right"], None),
            ("origin/only", &["origin/only"], None),
            ("origin/only, upstream/only", &["origin/only", "upstream/only"], None),
            ("origin/clash", &[], Some("clash")),
            ("origin/clash, origin/only", &["origin/only"], None),
            ("ghost", &[], None),
            ("HEAD, tag: v1, origin/HEAD", &[], None),
        ];
        for (refs, want, clash) in cases {
            let (got, why) = checkout_candidates(refs, &branches);
            let names: Vec<&str> = got.iter().map(|b| b.name.as_str()).collect();
            assert_eq!((names.as_slice(), why), (want, clash), "refs {refs:?}");
        }
        let (got, why) = checkout_candidates("", &branches);
        assert!(got.is_empty() && why.is_none());
    }

    /// A Log panel whose selected commit carries `refs`, with the branch list above.
    fn checkout_panel(refs: &str) -> GitPanel {
        let mut p = panel();
        p.log[0].commit.refs = refs.into();
        p.branches = checkout_branches();
        p
    }

    fn checkout(p: &mut GitPanel) -> Vec<Effect> {
        with_cx(|cx| p.handle_action(&GitAction::LogCheckout, cx))
    }

    #[test]
    fn checkout_revision_switches_to_the_one_local_branch() {
        let mut p = checkout_panel("dev, origin/dev");
        let fx = checkout(&mut p);
        let [Effect::Git(GitRequest::BranchOp { branch, op })] = fx.as_slice() else { panic!("{fx:?}") };
        assert_eq!((branch.name.as_str(), *op), ("dev", BranchOp::Checkout));
        p.area = Area::Files;
        let fx = checkout(&mut p);
        assert!(matches!(fx.as_slice(), [Effect::Git(GitRequest::BranchOp { op: BranchOp::Checkout, .. })]), "{fx:?}");
    }

    #[test]
    fn checkout_revision_tracks_a_lone_remote() {
        let fx = checkout(&mut checkout_panel("origin/only"));
        let [Effect::Git(GitRequest::BranchOp { branch, op })] = fx.as_slice() else { panic!("{fx:?}") };
        assert_eq!((branch.name.as_str(), *op), ("origin/only", BranchOp::CheckoutTracking));
    }

    #[test]
    fn checkout_revision_offers_a_menu_for_several() {
        let fx = checkout(&mut checkout_panel("upstream/only, origin/only"));
        assert_eq!(fx.len(), 1, "{fx:?}");
        let Some(Effect::ContextMenu(m)) = fx.first() else { panic!("{fx:?}") };
        let rows: Vec<(&str, Option<&Action>)> = m.items.iter().map(|i| (i.label.as_str(), i.action.as_ref())).collect();
        let tracking = |b: &str| Action::Git(GitAction::BranchOp { branch: b.into(), op: BranchOp::CheckoutTracking });
        assert_eq!(
            rows,
            [
                ("Checkout 'origin/only' as a new local branch", Some(&tracking("origin/only"))),
                ("Checkout 'upstream/only' as a new local branch", Some(&tracking("upstream/only"))),
            ]
        );
        let fx = checkout(&mut checkout_panel("right, left"));
        let Some(Effect::ContextMenu(m)) = fx.first() else { panic!("{fx:?}") };
        let local = |b: &str| Action::Git(GitAction::BranchOp { branch: b.into(), op: BranchOp::Checkout });
        assert_eq!(m.items[0].label, "Checkout 'left'");
        assert_eq!(m.items[0].action, Some(local("left")));
        assert_eq!(m.items[1].label, "Checkout 'right'");
        assert_eq!(m.items[1].action, Some(local("right")));
        assert_eq!(fx.len(), 1);
    }

    #[test]
    fn checkout_revision_detaches_without_a_branch() {
        let fx = checkout(&mut checkout_panel("tag: v1"));
        assert!(matches!(fx.as_slice(), [Effect::Git(GitRequest::CheckoutCommit(sha))] if sha == &"a".repeat(40)), "{fx:?}");
    }

    #[test]
    fn checkout_revision_says_why_it_detaches() {
        let fx = checkout(&mut checkout_panel("origin/clash"));
        let [Effect::Notify(Level::Info, text), Effect::Git(GitRequest::CheckoutCommit(sha))] = fx.as_slice() else { panic!("{fx:?}") };
        assert_eq!(text, "Local branch 'clash' is at another commit, so HEAD detaches");
        assert_eq!(sha, &"a".repeat(40));
    }

    #[test]
    fn checkout_revision_is_refused_mid_merge() {
        let mut p = checkout_panel("dev");
        p.snapshot =
            Some(StatusSnapshot { in_progress: Some(InProgress { kind: InProgressKind::Merge, step: None }), ..Default::default() });
        let fx = checkout(&mut p);
        assert!(matches!(fx.as_slice(), [Effect::Notify(Level::Warn, _)]), "{fx:?}");
    }

    #[test]
    fn right_and_left_open_and_close_folders() {
        let mut p = panel();
        assert_eq!(p.log_tree.rows().len(), 9);
        p.area = Area::Files;
        p.files_list.select(1);
        with_cx(|cx| p.handle_action(&GitAction::Left, cx));
        assert_eq!(p.log_tree.rows().len(), 8);
        assert_eq!(p.files_list.len, 8);
        assert_eq!(p.area, Area::Files);
        with_cx(|cx| p.handle_action(&GitAction::Left, cx));
        assert_eq!(p.area, Area::Log);
        p.area = Area::Files;
        with_cx(|cx| p.handle_action(&GitAction::Right, cx));
        assert_eq!(p.log_tree.rows().len(), 9);
        with_cx(|cx| p.handle_action(&GitAction::Right, cx));
        assert_eq!(p.files_list.selected, 2);
    }

    #[test]
    fn enter_on_a_file_requests_its_commit_diff() {
        let mut p = panel();
        p.area = Area::Files;
        assert!(p.log_file_key().is_none());
        p.files_list.select(1);
        assert!(p.log_file_key().is_none());
        p.files_list.select(2);
        let fx = with_cx(|cx| p.handle_action(&GitAction::OpenFile, cx));
        assert!(
            fx.iter().any(|e| matches!(e, Effect::Git(GitRequest::LoadDiff(DiffKey::Commit { rel, .. })) if rel == "docs/a.md")),
            "{fx:?}"
        );
        assert_eq!(p.area, Area::LogDiff);
    }

    #[test]
    fn file_menu_has_commit_items_disabled_while_busy() {
        let mut p = panel();
        p.area = Area::Files;
        p.files_list.select(2);
        let fx = with_cx(|cx| p.log_file_menu(None, cx));
        let got = labels(&fx);
        let names: Vec<&str> = got.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            names,
            [
                "Show Diff",
                "Open Diff in Editor Tab",
                "Open in Editor",
                "Copy Path",
                "Show History for File",
                "",
                "Copy Revision Number",
                "Cherry-Pick",
                "Revert Commit…",
                "Checkout Revision"
            ]
        );
        assert_eq!(got[2], ("Open in Editor".to_string(), false));
        assert!(got[7].1);
        let Some(Effect::ContextMenu(m)) = fx.first() else { unreachable!() };
        assert!(m.items[5].action.is_none());
        assert_eq!(m.items[1].action, Some(Action::Git(GitAction::LogFileOp(LogFileOp::OpenDiffTab))));
        assert_eq!(m.items[0].action, Some(Action::Git(GitAction::LogFileOp(LogFileOp::ShowDiff))));

        p.snapshot =
            Some(StatusSnapshot { in_progress: Some(InProgress { kind: InProgressKind::Merge, step: None }), ..Default::default() });
        let got = labels(&with_cx(|cx| p.log_file_menu(None, cx)));
        assert_eq!(got[6], ("Copy Revision Number".to_string(), true));
        assert_eq!(got[7], ("Cherry-Pick".to_string(), false));
        assert!(!got[8].1 && !got[9].1);
    }

    fn sole_diff_tab(fx: &[Effect]) -> DiffKey {
        match fx {
            [Effect::OpenDiffTab { files, at: 0, .. }] if files.len() == 1 => files[0].clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn show_diff_in_log_files_opens_the_selected_file() {
        let mut p = panel();
        p.area = Area::Files;
        p.files_list.select(2);
        let want = p.log_file_key().unwrap();
        let fx = with_cx(|cx| p.handle_action(&GitAction::ShowDiff, cx));
        assert_eq!(sole_diff_tab(&fx), want);
        // The folder row has no file: a note, no tab.
        p.files_list.select(1);
        assert!(p.log_file_key().is_none());
        let fx = with_cx(|cx| p.handle_action(&GitAction::ShowDiff, cx));
        assert!(matches!(fx.as_slice(), [Effect::Notify(_, t)] if t == "Select a file to show its diff"), "{fx:?}");
    }

    #[test]
    fn show_diff_in_stash_files_opens_the_selected_file() {
        let mut p = GitPanel::new();
        p.tab = GitTab::Stash;
        p.area = Area::StashFiles;
        let fx = with_cx(|cx| p.handle_action(&GitAction::ShowDiff, cx));
        assert!(matches!(fx.as_slice(), [Effect::Notify(_, t)] if t == "Select a file to show its diff"), "{fx:?}");
        p.stash_details = Some(CommitDetails { sha: "s".into(), body: String::new(), files: vec![file('M', "x.rs")] });
        p.stash_files.set_len(1);
        let fx = with_cx(|cx| p.handle_action(&GitAction::ShowDiff, cx));
        assert_eq!(sole_diff_tab(&fx), DiffKey::Commit { sha: "s".into(), rel: "x.rs".into(), old: None });
    }

    #[test]
    fn open_diff_tab_file_op_pushes_the_selected_file() {
        let mut p = panel();
        p.area = Area::Files;
        p.files_list.select(2);
        let want = p.log_file_key().unwrap();
        let fx = with_cx(|cx| p.log_file_op(LogFileOp::OpenDiffTab, cx));
        assert_eq!(sole_diff_tab(&fx), want);
        p.files_list.select(1);
        assert!(with_cx(|cx| p.log_file_op(LogFileOp::OpenDiffTab, cx)).is_empty());
    }

    #[test]
    fn folder_and_root_menus() {
        let mut p = panel();
        p.area = Area::Files;
        p.files_list.select(1);
        let got: Vec<String> = labels(&with_cx(|cx| p.log_file_menu(None, cx))).into_iter().map(|(l, _)| l).collect();
        let mut want = vec!["Expand All", "Collapse All", "Copy Path", "Show History for Folder", ""];
        want.extend(COMMIT_ITEMS);
        assert_eq!(got, want);
        p.files_list.select(0);
        let got: Vec<String> = labels(&with_cx(|cx| p.log_file_menu(None, cx))).into_iter().map(|(l, _)| l).collect();
        let mut want = vec!["Expand All", "Collapse All", ""];
        want.extend(COMMIT_ITEMS);
        assert_eq!(got, want);
    }

    #[test]
    fn collapse_all_then_show_history_of_a_folder() {
        let mut p = panel();
        with_cx(|cx| p.log_file_op(LogFileOp::CollapseAll, cx));
        assert_eq!(p.log_tree.rows().len(), 4);
        p.files_list.select(2);
        let fx = with_cx(|cx| p.log_file_op(LogFileOp::ShowHistory, cx));
        assert_eq!(p.log_filter().path, "src");
        assert!(fx.iter().any(|e| matches!(e, Effect::Git(GitRequest::LoadLog { .. }))), "{fx:?}");
    }

    fn commit(sha: &str, parent: Option<&str>) -> LogCommit {
        LogCommit {
            sha: sha.into(),
            parents: parent.map(str::to_string).into_iter().collect(),
            short: sha.into(),
            author: "me".into(),
            date: "now".into(),
            refs: String::new(),
            subject: format!("subject {sha}"),
        }
    }

    #[test]
    fn several_selected_rows_show_the_union_of_their_files() {
        let mut p = GitPanel::new();
        p.tab = GitTab::Log;
        for c in [commit("a", Some("b")), commit("b", Some("c")), commit("c", None)] {
            p.log.push(LogRow { graph: "●".into(), commit: c });
        }
        p.log_list.set_len(3);
        p.log_sel = [0, 1].into_iter().collect();
        let fx = with_cx(|cx| p.sync_details(cx));
        let spans = match fx.as_slice() {
            [Effect::Git(GitRequest::LoadCommitsFiles(spans))] => spans.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].base.as_deref(), spans[0].top.as_str()), (Some("c"), "a"));

        // A result for other spans is stale.
        let other = vec![crate::git::model::DiffSpan { base: None, top: "a".into(), header: String::new() }];
        p.set_log_files(&other, Ok(Vec::new()), Path::new("/tmp/root"));
        assert!(p.details.is_none());

        let key = DiffKey::Range { base: Some("c".into()), top: "a".into(), rel: "x.rs".into(), old: None };
        p.set_log_files(&spans, Ok(vec![(file('M', "x.rs"), key.clone())]), Path::new("/tmp/root"));
        assert!(p.details.is_some());
        p.area = Area::Files;
        p.files_list.select(1);
        assert_eq!(p.log_file_key(), Some(key));

        // A single-commit answer for the cursor commit is ignored.
        p.set_log_details(
            "a",
            Ok(CommitDetails { sha: "a".into(), body: String::new(), files: vec![file('M', "y.rs")] }),
            Path::new("/tmp/root"),
        );
        assert_eq!(p.details.as_ref().map(|d| d.files.len()), Some(1));
        assert_eq!(p.details.as_ref().map(|d| d.files[0].path.as_str()), Some("x.rs"));

        let names: Vec<String> = labels(&with_cx(|cx| p.log_file_menu(None, cx))).into_iter().map(|(l, _)| l).collect();
        assert_eq!(names, ["Show Diff", "Open Diff in Editor Tab", "Open in Editor", "Copy Path", "Show History for File"]);

        p.area = Area::Log;
        let fx = with_cx(|cx| p.handle_action(&GitAction::Down, cx));
        assert!(p.log_sel.is_empty());
        assert!(fx.iter().any(|e| matches!(e, Effect::Git(GitRequest::LoadCommit(sha)) if sha == "b")), "{fx:?}");
    }

    #[test]
    fn ref_labels_shorten_and_merge() {
        let b = |name: &str, current, remote| BranchInfo { name: name.into(), upstream: None, ahead: 0, behind: 0, current, remote };
        let branches = vec![
            b("main", true, false),
            b("parser", false, false),
            b("x", false, false),
            b("feature/x", false, false),
            b("feature/y", false, false),
            b("origin/main", false, true),
            b("upstream/main", false, true),
            b("origin/parser", false, true),
            b("origin/only", false, true),
            b("origin/feature/y", false, true),
        ];
        let cases = [
            ("HEAD -> main, origin/main, upstream/main, origin/HEAD", "* origin & upstream & main"),
            ("HEAD -> main, upstream/main, origin/main", "* origin & upstream & main"),
            ("HEAD -> main", "* main"),
            ("parser, origin/parser", "origin & parser"),
            ("origin/parser, parser", "origin & parser"),
            ("origin/parser", "origin/parser"),
            ("origin/only", "origin/only"),
            ("feature/y, origin/feature/y", "origin & feature/y"),
            ("feature/x, x", "feature/x, x"),
            ("tag: v0.1.0, parser", "parser, tag: v0.1.0"),
            ("tag: v0.1.0, origin/only, x, HEAD, parser, HEAD -> main", "HEAD, * main, parser, x, origin/only, tag: v0.1.0"),
            ("HEAD", "HEAD"),
            ("HEAD, parser", "HEAD, parser"),
            ("origin/HEAD", ""),
            ("", ""),
        ];
        for (refs, want) in cases {
            assert_eq!(ref_labels(refs, &branches, 100), want, "refs {refs:?}");
        }
        assert_eq!(ref_labels("main, origin/main", &[], 100), "main, origin/main");
    }

    #[test]
    fn ref_labels_drop_the_remote_prefixes_under_the_cap() {
        let b = |name: &str, current, remote| BranchInfo { name: name.into(), upstream: None, ahead: 0, behind: 0, current, remote };
        let branches =
            vec![b("solo", true, false), b("origin/solo", false, true), b("upstream/solo", false, true), b("main", false, false)];
        let refs = "HEAD -> solo, upstream/solo, origin/solo, main, tag: v0.1.0";
        // (cap, label): the short form is 25 columns wide.
        let cases = [
            (60, "* origin & upstream & solo, main, tag: v0.1.0"),
            (25, "* solo, main, tag: v0.1.0"),
            (10, "* solo, m\u{2026}"),
            (6, "* sol\u{2026}"),
        ];
        for (cap, want) in cases {
            assert_eq!(ref_labels(refs, &branches, cap), want, "cap {cap}");
        }
        assert_eq!(ref_labels("HEAD -> solo, origin/solo", &branches, 8), "* solo");
        assert_eq!(ref_labels("HEAD -> solo, origin/solo", &branches, 25), "* origin & solo");
        assert_eq!(ref_labels("main, origin/main", &[b("main", false, false), b("origin/main", false, true)], 7), "main");
    }

    #[test]
    fn refs_cap_and_author_fits() {
        assert_eq!(refs_cap(60), 30);
        assert_eq!(refs_cap(39), 19);
        assert_eq!(refs_cap(32), 16);
        assert_eq!(refs_cap(15), 4);
        assert_eq!(refs_cap(14), 0);
        assert_eq!(refs_cap(0), 0);
        assert_eq!(refs_cap(26), 15);
        assert!(author_fits(43, 17));
        assert!(!author_fits(42, 17));
        assert!(!author_fits(33, 33));
    }
}
