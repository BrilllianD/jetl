//! Log tab: `git log --graph` rows, details of the selected commit, and a
//! diff of a file in it.

use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{Area, Btn, GitPanel, GitTab, put, render_diff, render_diff_header};
use crate::actions::{Action, GitAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::GitRequest;
use crate::git::graph::LogGraph;
use crate::git::model::{CommitDetails, DiffKey, LogCommit, LogFilter, LogRev, LogRow, ResetMode};
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
        let Some(sha) = self.selected_commit().map(str::to_string) else { return };
        if self.details.as_ref().is_some_and(|d| d.sha == sha) {
            return;
        }
        self.details = None;
        self.details_error = None;
        self.files_list = ListState::default();
        cx.effects.push(Effect::Git(GitRequest::LoadCommit(sha)));
    }

    pub(super) fn set_log_details(&mut self, sha: &str, result: anyhow::Result<CommitDetails>) {
        if self.selected_commit() != Some(sha) {
            return;
        }
        match result {
            Ok(d) => {
                self.files_list = ListState::default();
                self.files_list.set_len(d.files.len());
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
        if matches!(action, CommitAction::Checkout | CommitAction::NewBranch | CommitAction::CherryPick | CommitAction::Revert)
            && let Some(ip) = self.in_progress().filter(|ip| ip.kind.blocks_switching())
        {
            return cx.notify(Level::Warn, ip.refusal());
        }
        match action {
            CommitAction::CopySha => {
                cx.notify(Level::Info, format!("Copied {sha}"));
                cx.effects.push(Effect::Clipboard(sha));
            }
            CommitAction::Checkout => cx.effects.push(Effect::Git(GitRequest::CheckoutCommit(sha))),
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

    /// Open the combined diff of the selected commits in an editor tab.
    pub(super) fn show_commits_diff(&mut self, cx: &mut Ctx) {
        let rows = self.log_selection();
        if self.tab != GitTab::Log || rows.is_empty() {
            return cx.notify(Level::Info, "Select a commit in the Log tab");
        }
        let commits: Vec<&LogCommit> = rows.iter().map(|&i| &self.log[i].commit).collect();
        let (title, spans) = crate::git::model::commits_diff(&commits);
        cx.effects.push(Effect::Git(GitRequest::CommitsDiff { title, spans }));
    }

    /// Commit actions for the selected commit, at the pointer or its row.
    pub(super) fn log_context_menu(&mut self, at: Option<(u16, u16)>, cx: &mut Ctx) {
        if self.tab != GitTab::Log || self.selected_commit().is_none() {
            return;
        }
        let (x, y) = at.unwrap_or_else(|| {
            let row = self.log_list.selected.saturating_sub(self.log_list.offset) as u16;
            (self.log_rect.x + 4, self.log_rect.y + row + 1)
        });
        let g = |a| Action::Git(a);
        let busy = self.in_progress().is_some_and(|ip| ip.kind.blocks_switching());
        let mut items = vec![
            MenuItem::new("Copy Revision Number", g(GitAction::CopySha)),
            MenuItem::new("Show Diff", g(GitAction::LogShowDiff)),
            MenuItem::new("Checkout Revision", g(GitAction::LogCheckout)).enabled(!busy),
            MenuItem::new("New Branch…", g(GitAction::LogNewBranch)).enabled(!busy),
            MenuItem::new("Cherry-Pick", g(GitAction::LogCherryPick)).enabled(!busy),
            MenuItem::new("Revert Commit…", g(GitAction::LogRevert)).enabled(!busy),
            MenuItem::new("Reset Current Branch to Here…", g(GitAction::LogReset)),
            MenuItem::new("New Tag…", g(GitAction::LogNewTag)),
            MenuItem::new("Delete Tag…", g(GitAction::LogDeleteTag)),
            MenuItem::new("Filter Log…", g(GitAction::LogFilter)),
        ];
        let at = self.log.get(self.log_list.selected).map(|r| branches_at(&r.commit.refs)).unwrap_or_default();
        let at: Vec<String> = at.into_iter().filter(|n| self.branches.iter().any(|b| b.name == *n)).collect();
        if !at.is_empty() {
            items.push(MenuItem::separator());
            items.extend(at.into_iter().map(|name| MenuItem::new(format!("Branch '{name}' ▸"), g(GitAction::LogBranch(name)))));
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

    pub(super) fn open_commit_file(&mut self, cx: &mut Ctx) {
        let Some(d) = &self.details else { return };
        let Some(f) = d.files.get(self.files_list.selected) else { return };
        let key = DiffKey::Commit { sha: d.sha.clone(), rel: f.path.clone(), old: f.old.clone() };
        self.log_diff.request(key, cx);
        self.area = Area::LogDiff;
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
        if let Some(i) = self.log_list.hit(self.log_rect, m.x, m.y) {
            use crossterm::event::{KeyModifiers as K, MouseButton, MouseEventKind};
            self.area = Area::Log;
            let right = m.kind == MouseEventKind::Down(MouseButton::Right);
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
            self.open_commit_file(cx);
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
                DiffKey::Change { .. } | DiffKey::Local { .. } => format!(" {}", key.label()),
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
            let mut right = String::new();
            if !c.author.is_empty() {
                right = format!(" {}, {}", c.author, c.date);
            }
            let right_w = right.width() as u16;
            put(buf, &mut x, y, r.right(), &c.short, if selected { base } else { theme.warn });
            x += 1;
            if !c.refs.is_empty() {
                put(
                    buf,
                    &mut x,
                    y,
                    r.right(),
                    &format!("({})", c.refs),
                    if selected { base.add_modifier(Modifier::BOLD) } else { theme.status_accent.add_modifier(Modifier::BOLD) },
                );
                x += 1;
            }
            let subject_end = if r.right() > x + right_w + 10 { r.right() - right_w } else { r.right() };
            let subject = crate::util::text::fit(&c.subject, subject_end.saturating_sub(x) as usize);
            put(buf, &mut x, y, subject_end, &subject, base);
            if subject_end < r.right() {
                let mut rx = r.right() - right_w;
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
        let n = self.log_selection().len();
        if n > 1 {
            buf.set_stringn(x0, y, format!("{n} commits selected"), w, theme.dim);
            y += 1;
        }
        let commit = self.log.get(self.log_list.selected).map(|row| &row.commit);
        if let Some(c) = commit {
            buf.set_stringn(x0, y, format!("{}  {}, {}", c.short, c.author, c.date), w, theme.title);
            y += 1;
        }
        // Message up to half the height, then the file list.
        let body_lines: Vec<&str> = d.body.lines().collect();
        let max_body = (r.height as usize / 2).saturating_sub(1).max(1);
        for line in body_lines.iter().take(max_body) {
            if y >= r.bottom() {
                return;
            }
            buf.set_stringn(x0, y, line, w, Style::default().fg(theme.fg));
            y += 1;
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
            let f = &d.files[i];
            let selected = i == self.files_list.selected && active;
            let base = if selected { theme.list_selected } else { Style::default().fg(theme.fg) };
            if selected {
                buf.set_style(Rect::new(r.x, fy, r.width, 1), base);
            }
            let st_style = match f.status {
                'A' => theme.git_added,
                'D' => theme.git_deleted,
                _ => theme.git_modified,
            };
            let mut x = x0;
            put(buf, &mut x, fy, r.right(), &f.status.to_string(), if selected { base } else { st_style });
            x += 1;
            let path = match &f.old {
                Some(old) => format!("{old} -> {}", f.path),
                None => f.path.clone(),
            };
            put(buf, &mut x, fy, r.right(), &path, base);
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

#[cfg(test)]
mod tests {
    use super::branches_at;

    #[test]
    fn branches_at_keeps_only_branches() {
        assert_eq!(branches_at("HEAD -> main, origin/main, origin/HEAD, tag: v1, dev"), ["main", "origin/main", "dev"]);
        assert!(branches_at("HEAD").is_empty());
        assert!(branches_at("").is_empty());
    }
}
