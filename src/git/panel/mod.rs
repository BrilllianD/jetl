//! Git tool window (bottom): tabs Commit | Log | Stash | Console | History.
//!
//! Commit: change list grouped Staged / Unstaged / Untracked with stage
//! checkboxes, the diff of the selected entry, and the commit message box.
//! Log: `git log --graph` rows, details of the selected commit, and a diff
//! of a file in it. Stash: the stash list, files of a stash and their diff.
//! Console: output of CLI operations. History: a local history snapshot
//! (`history/`) against the file's current text, with Revert.
//!
//! This module holds the shared state, input routing, the header, the
//! console and the diff renderer; `commit.rs`, `log.rs`, `stash.rs` and
//! `history.rs` hold the tabs.
//!
//! The panel only holds state; all git work is requested through
//! `Effect::Git` and the results are fed back by the app.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::GitRequest;
use super::diff::DiffDoc;
use super::model::{BranchInfo, CommitDetails, DiffKey, LogFilter, LogRow, Section, StashEntry, StatusSnapshot};
use crate::actions::GitAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::focus::PanelId;
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

mod commit;
mod diff_view;
mod history;
mod log;
mod stash;

use diff_view::{DiffOpts, DiffPane, render_diff, render_toggles};
use log::{CommitAction, LogFilterPart};
use stash::StashAction;

/// Stored in sessions (`SessionLayout::git_tab`); add variants, never
/// rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GitTab {
    Commit,
    Log,
    Console,
    Stash,
    History,
}

/// Which part of the window has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Area {
    Changes,
    Diff,
    Message,
    Log,
    Files,
    LogDiff,
    Stashes,
    StashFiles,
    StashDiff,
    Console,
    HistoryDiff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Tab(GitTab),
    Amend,
    Refresh,
    Fetch,
    Pull,
    Push,
    StageAll,
    UnstageAll,
    Commit,
    CommitPush,
    Back,
    StashPush,
    StashPop,
    StashApply,
    StashDrop,
    DiffLayout,
    DiffCollapse,
    LogClearFilter,
    LogFilter(LogFilterPart),
    HistoryRevert,
    HistoryList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListRow {
    Header(Section, usize),
    Entry(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsoleKind {
    Command,
    Output,
    Error,
}

pub struct GitPanel {
    pub tab: GitTab,
    area: Area,
    snapshot: Option<StatusSnapshot>,
    status_error: Option<String>,
    status_seq: u64,
    rows: Vec<ListRow>,
    list: ListState,
    pub message: TextInput,
    /// The next commit replaces HEAD.
    amend: bool,
    /// The HEAD message put in the box when amend turned on; cleared again
    /// when amend turns off unedited.
    amend_prefill: Option<String>,
    /// Earlier commit messages of the repository, newest first.
    history: Vec<String>,
    /// Position in `history` while recalling, and the text typed before.
    history_pos: Option<usize>,
    history_draft: String,
    diff: DiffPane,
    log: Vec<LogRow>,
    /// Lanes after the last listed commit, for the next page.
    log_graph: crate::git::graph::LogGraph,
    log_list: ListState,
    log_loaded: bool,
    /// The filter of the log request in flight.
    log_loading: Option<LogFilter>,
    log_filter: LogFilter,
    /// `[git] log_all`, for the branch label.
    log_all: bool,
    log_end: bool,
    log_page: usize,
    details: Option<CommitDetails>,
    details_error: Option<String>,
    files_list: ListState,
    log_diff: DiffPane,
    stashes: Vec<StashEntry>,
    stash_error: Option<String>,
    stash_list: ListState,
    stash_details: Option<CommitDetails>,
    stash_details_error: Option<String>,
    stash_files: ListState,
    stash_diff: DiffPane,
    /// The History tab: a local history snapshot against the current text,
    /// and the snapshot's date and label.
    history_diff: DiffPane,
    history_title: String,
    /// Layout of every diff pane (Alt+U, Alt+F, or the header toggles).
    diff_opts: DiffOpts,
    console: Vec<(ConsoleKind, String)>,
    /// Lines scrolled up from the bottom of the console.
    console_scroll: usize,
    pub branches: Vec<BranchInfo>,
    hits: Vec<(Btn, Rect)>,
    changes_rect: Rect,
    message_rect: Rect,
    diff_rect: Rect,
    log_rect: Rect,
    files_rect: Rect,
    stash_rect: Rect,
    stash_files_rect: Rect,
    console_rect: Rect,
}

impl Default for GitPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl GitPanel {
    pub fn new() -> Self {
        Self {
            tab: GitTab::Commit,
            area: Area::Changes,
            snapshot: None,
            status_error: None,
            status_seq: 0,
            rows: Vec::new(),
            list: ListState::default(),
            message: TextInput::new(true),
            amend: false,
            amend_prefill: None,
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            diff: DiffPane::new(true),
            log: Vec::new(),
            log_graph: Default::default(),
            log_list: ListState::default(),
            log_loaded: false,
            log_loading: None,
            log_filter: LogFilter::default(),
            log_all: false,
            log_end: false,
            log_page: 500,
            details: None,
            details_error: None,
            files_list: ListState::default(),
            log_diff: DiffPane::new(true),
            stashes: Vec::new(),
            stash_error: None,
            stash_list: ListState::default(),
            stash_details: None,
            stash_details_error: None,
            stash_files: ListState::default(),
            stash_diff: DiffPane::new(true),
            history_diff: DiffPane::new(true),
            history_title: String::new(),
            diff_opts: DiffOpts { unified: None, collapse: true },
            console: Vec::new(),
            console_scroll: 0,
            branches: Vec::new(),
            hits: Vec::new(),
            changes_rect: Rect::default(),
            message_rect: Rect::default(),
            diff_rect: Rect::default(),
            log_rect: Rect::default(),
            files_rect: Rect::default(),
            stash_rect: Rect::default(),
            stash_files_rect: Rect::default(),
            console_rect: Rect::default(),
        }
    }

    /// Forget everything about the previous root (phase 4 root switch). The
    /// commit message and console survive.
    pub fn reset(&mut self) {
        let message = std::mem::take(&mut self.message);
        let console = std::mem::take(&mut self.console);
        let tab = self.tab;
        let (log_page, log_all) = (self.log_page, self.log_all);
        *self = Self::new();
        self.log_all = log_all;
        self.message = message;
        self.console = console;
        self.tab = tab;
        self.log_page = log_page;
        self.area = Self::default_area(tab);
    }

    pub fn snapshot(&self) -> Option<&StatusSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn branch_label(&self) -> Option<String> {
        self.snapshot.as_ref().and_then(|s| s.label())
    }

    pub fn log_loaded(&self) -> bool {
        self.log_loaded
    }

    pub fn set_log_page(&mut self, n: usize) {
        self.log_page = n.max(1);
    }

    pub fn set_log_all(&mut self, all: bool) {
        self.log_all = all;
    }

    /// The theme changed (hot reload): the diff panes compute their syntax
    /// colors again on the next render.
    pub fn theme_changed(&mut self) {
        for pane in [&mut self.diff, &mut self.log_diff, &mut self.stash_diff, &mut self.history_diff] {
            pane.restyle();
        }
    }

    fn default_area(tab: GitTab) -> Area {
        match tab {
            GitTab::Commit => Area::Changes,
            GitTab::Log => Area::Log,
            GitTab::Stash => Area::Stashes,
            GitTab::Console => Area::Console,
            GitTab::History => Area::HistoryDiff,
        }
    }

    pub fn set_diff(&mut self, key: &DiffKey, result: anyhow::Result<DiffDoc>) {
        match key {
            DiffKey::Change { .. } => self.diff.set(key, result),
            // A commit diff is shown by the Log tab or by the Stash tab
            // (stashes are commits); each pane ignores keys it does not show.
            DiffKey::Commit { .. } if self.stash_diff.key.as_ref() == Some(key) => self.stash_diff.set(key, result),
            DiffKey::Commit { .. } => self.log_diff.set(key, result),
            DiffKey::Local { .. } => self.history_diff.set(key, result),
        }
    }

    /// Details of a commit: the selected log commit or the selected stash.
    pub fn set_details(&mut self, sha: &str, result: anyhow::Result<CommitDetails>) {
        if self.selected_stash().is_some_and(|s| s.oid == sha) {
            self.set_stash_details(result);
        } else {
            self.set_log_details(sha, result);
        }
    }

    /// Record a finished operation in the console.
    pub fn log_op(&mut self, label: &str, ok: bool, output: &str) {
        self.console.push((ConsoleKind::Command, format!("$ {label}")));
        let kind = if ok { ConsoleKind::Output } else { ConsoleKind::Error };
        for line in output.lines() {
            self.console.push((kind, line.to_string()));
        }
        if !ok && output.trim().is_empty() {
            self.console.push((kind, "failed".into()));
        }
        if self.console.len() > 5000 {
            self.console.drain(..self.console.len() - 5000);
        }
        self.console_scroll = 0;
    }

    /// Switch tabs, loading the log the first time it shows.
    pub fn set_tab(&mut self, tab: GitTab, cx: &mut Ctx) {
        self.tab = tab;
        self.area = Self::default_area(tab);
        if tab == GitTab::Log && !self.log_loaded {
            self.request_log(cx);
        }
        if tab == GitTab::Console {
            self.console_scroll = 0;
        }
    }

    pub fn handle_action(&mut self, a: &GitAction, cx: &mut Ctx) -> bool {
        use GitAction as G;
        match a {
            // F5 on the History tab compares with the current text again.
            G::Refresh if self.tab == GitTab::History => self.reload_history_diff(cx),
            G::Refresh => cx.effects.push(Effect::Git(GitRequest::Refresh)),
            G::ToggleStage => match (self.tab, self.area) {
                (GitTab::Commit, Area::Diff) => self.hunk_op(false, cx),
                (GitTab::Commit, _) => self.toggle_selected(cx),
                _ => {}
            },
            G::LogFilter => {
                if self.tab != GitTab::Log {
                    self.set_tab(GitTab::Log, cx);
                }
                let items = ["Branch", "Text in message", "Author", "Path", "Clear all filters"].map(String::from).to_vec();
                cx.effects.push(Effect::Dialog(crate::ui::dialog::Dialog::choice(
                    crate::ui::dialog::DialogId::LogFilterKind,
                    "Filter Log",
                    "",
                    items,
                )));
            }
            G::FileHistory | G::ResolveConflicts => {}
            G::ContextMenu => self.log_context_menu(None, cx),
            G::CopySha => self.commit_action(CommitAction::CopySha, cx),
            G::LogCheckout => self.commit_action(CommitAction::Checkout, cx),
            G::LogNewBranch => self.commit_action(CommitAction::NewBranch, cx),
            G::LogCherryPick => self.commit_action(CommitAction::CherryPick, cx),
            G::LogRevert => self.commit_action(CommitAction::Revert, cx),
            G::LogNewTag => self.commit_action(CommitAction::NewTag, cx),
            G::LogDeleteTag => self.commit_action(CommitAction::DeleteTag, cx),
            G::ToggleDiffLayout => self.toggle_diff_layout(),
            G::ToggleDiffCollapse => self.toggle_diff_collapse(),
            G::NextHunk | G::PrevHunk => {
                let forward = *a == G::NextHunk;
                if !self.diff_pane().step_block(forward) {
                    cx.notify(Level::Info, if forward { "No more changes below" } else { "No more changes above" });
                }
            }
            G::StageAll => cx.effects.push(Effect::Git(GitRequest::StageAll)),
            G::Discard => match (self.tab, self.area) {
                (GitTab::Commit, Area::Diff) => self.hunk_op(true, cx),
                (GitTab::Commit, _) => self.discard_selected(cx),
                _ => {}
            },
            G::UnstageAll => cx.effects.push(Effect::Git(GitRequest::UnstageAll)),
            G::Commit => self.commit(false, cx),
            G::CommitAndPush => self.commit(true, cx),
            G::ToggleAmend => self.toggle_amend(cx),
            G::MessageOlder | G::MessageNewer => {
                self.tab = GitTab::Commit;
                self.area = Area::Message;
                self.recall(*a == G::MessageOlder);
            }
            G::CommitBox | G::FocusMessage => {
                self.tab = GitTab::Commit;
                self.area = Area::Message;
                cx.focus(PanelId::Git);
            }
            G::Push => cx.effects.push(Effect::Git(GitRequest::Push)),
            G::Pull => cx.effects.push(Effect::Git(GitRequest::Pull)),
            G::Fetch => cx.effects.push(Effect::Git(GitRequest::Fetch)),
            G::Branches => cx.effects.push(Effect::Git(GitRequest::BranchesPopup)),
            G::NewBranch => cx.effects.push(Effect::Git(GitRequest::NewBranch)),
            G::TabCommit => self.set_tab(GitTab::Commit, cx),
            G::TabLog => self.set_tab(GitTab::Log, cx),
            G::TabConsole => self.set_tab(GitTab::Console, cx),
            G::TabStash => self.set_tab(GitTab::Stash, cx),
            G::TabHistory => self.set_tab(GitTab::History, cx),
            G::HistoryRevert => self.history_revert(cx),
            G::StashPush => self.stash_push(cx),
            G::StashPop | G::StashApply | G::StashDrop => {
                if self.tab != GitTab::Stash {
                    // Pick a stash first; Delete elsewhere does nothing.
                    if *a != G::StashDrop {
                        self.set_tab(GitTab::Stash, cx);
                    }
                } else {
                    let action = match a {
                        G::StashPop => StashAction::Pop,
                        G::StashApply => StashAction::Apply,
                        _ => StashAction::Drop,
                    };
                    self.stash_action(action, cx);
                }
            }
            G::Up => self.move_by(-1, cx),
            G::Down => self.move_by(1, cx),
            G::PageUp => self.move_by(-(self.page() as isize), cx),
            G::PageDown => self.move_by(self.page() as isize, cx),
            G::Left => match self.area {
                Area::Diff => self.area = Area::Changes,
                Area::Files => self.area = Area::Log,
                Area::LogDiff => self.close_log_diff(),
                Area::StashFiles => self.area = Area::Stashes,
                Area::StashDiff => self.close_stash_diff(),
                Area::Changes | Area::Message | Area::Log | Area::Stashes | Area::Console | Area::HistoryDiff => {}
            },
            G::Right => match self.area {
                Area::Changes if self.diff.key.is_some() => self.area = Area::Diff,
                Area::Log if self.details.is_some() => self.area = Area::Files,
                Area::Files => self.open_commit_file(cx),
                Area::Stashes if self.stash_details.is_some() => self.area = Area::StashFiles,
                Area::StashFiles => self.open_stash_file(cx),
                _ => {}
            },
            G::OpenFile => match self.area {
                Area::Changes | Area::Diff => self.open_selected_file(cx),
                Area::Log => {
                    if self.details.as_ref().is_some_and(|d| !d.files.is_empty()) {
                        self.area = Area::Files;
                    }
                }
                Area::Files => self.open_commit_file(cx),
                Area::Stashes => {
                    if self.stash_details.as_ref().is_some_and(|d| !d.files.is_empty()) {
                        self.area = Area::StashFiles;
                    }
                }
                Area::StashFiles => self.open_stash_file(cx),
                Area::HistoryDiff => self.open_history_file(cx),
                Area::Message | Area::LogDiff | Area::StashDiff | Area::Console => {}
            },
            G::NextSection => {
                self.area = match self.area {
                    Area::Changes => Area::Diff,
                    Area::Diff => Area::Message,
                    Area::Message => Area::Changes,
                    Area::Log => Area::Files,
                    Area::Files => Area::Log,
                    Area::Stashes => Area::StashFiles,
                    Area::StashFiles => Area::Stashes,
                    a => a,
                }
            }
            G::Escape => match self.area {
                Area::LogDiff => self.close_log_diff(),
                Area::StashDiff => self.close_stash_diff(),
                Area::Message | Area::Diff => self.area = Area::Changes,
                Area::Files => self.area = Area::Log,
                Area::StashFiles => self.area = Area::Stashes,
                _ => cx.focus(PanelId::Editor),
            },
        }
        true
    }

    fn page(&self) -> usize {
        let rows = match self.area {
            Area::Changes => self.list.viewport,
            Area::Diff => self.diff.page,
            Area::LogDiff => self.log_diff.page,
            Area::Log => self.log_list.viewport,
            Area::Files => self.files_list.viewport,
            Area::Stashes => self.stash_list.viewport,
            Area::StashFiles => self.stash_files.viewport,
            Area::StashDiff => self.stash_diff.page,
            Area::HistoryDiff => self.history_diff.page,
            Area::Console | Area::Message => self.console_rect.height as usize,
        };
        rows.saturating_sub(1).max(1)
    }

    fn move_by(&mut self, delta: isize, cx: &mut Ctx) {
        let step = |l: &mut ListState| {
            if delta < 0 { l.up(delta.unsigned_abs()) } else { l.down(delta as usize) }
        };
        match self.area {
            Area::Changes => {
                step(&mut self.list);
                self.sync_diff(false, cx);
            }
            Area::Diff => self.diff.scroll_by(delta),
            Area::LogDiff => self.log_diff.scroll_by(delta),
            Area::Log => {
                step(&mut self.log_list);
                self.sync_details(cx);
                self.maybe_page_log(cx);
            }
            Area::Files => step(&mut self.files_list),
            Area::Stashes => {
                step(&mut self.stash_list);
                self.sync_stash_details(cx);
            }
            Area::StashFiles => step(&mut self.stash_files),
            Area::StashDiff => self.stash_diff.scroll_by(delta),
            Area::HistoryDiff => self.history_diff.scroll_by(delta),
            Area::Console => self.console_scroll = self.console_scroll.saturating_add_signed(-delta).min(self.console.len()),
            Area::Message => {}
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        let p = Position::new(m.x, m.y);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(btn) = self.hits.iter().find(|(_, r)| r.contains(p)).map(|(b, _)| *b) {
                    self.click_button(btn, cx);
                    return true;
                }
                self.click_tab(m, p, cx);
                true
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if self.tab == GitTab::Log {
                    self.click_log_tab(m, p, cx);
                }
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d: isize = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                if self.changes_rect.contains(p) {
                    self.list.scroll(d as i32);
                } else if self.diff_rect.contains(p) {
                    self.diff_pane().scroll_by(d);
                } else if self.stash_rect.contains(p) {
                    self.stash_list.scroll(d as i32);
                } else if self.stash_files_rect.contains(p) {
                    self.stash_files.scroll(d as i32);
                } else if self.log_rect.contains(p) {
                    self.log_list.scroll(d as i32);
                    self.maybe_page_log(cx);
                } else if self.files_rect.contains(p) {
                    self.files_list.scroll(d as i32);
                } else if self.console_rect.contains(p) {
                    self.console_scroll = self.console_scroll.saturating_add_signed(-d).min(self.console.len());
                } else if self.message_rect.contains(p) {
                    if d < 0 { self.message.up() } else { self.message.down() }
                }
                true
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                let inside = self.diff_rect.contains(p);
                let pane = self.diff_pane();
                if inside {
                    pane.hscroll = if m.kind == MouseEventKind::ScrollLeft { pane.hscroll.saturating_sub(4) } else { pane.hscroll + 4 };
                }
                true
            }
            _ => false,
        }
    }

    fn click_tab(&mut self, m: &Mouse, p: Position, cx: &mut Ctx) {
        match self.tab {
            GitTab::Commit => self.click_commit_tab(m, p, cx),
            GitTab::Log => self.click_log_tab(m, p, cx),
            GitTab::Stash => self.click_stash_tab(m, p, cx),
            GitTab::Console => self.area = Area::Console,
            GitTab::History => self.click_history_tab(m, p),
        }
    }

    /// Flip the diff panes between unified and side-by-side (from what the
    /// current tab shows); the choice then holds at any width.
    fn toggle_diff_layout(&mut self) {
        let unified = self.diff_pane().unified;
        self.diff_opts.unified = Some(!unified);
    }

    fn toggle_diff_collapse(&mut self) {
        self.diff_opts.collapse = !self.diff_opts.collapse;
    }

    /// The diff pane of the current tab.
    fn diff_pane(&mut self) -> &mut DiffPane {
        match self.tab {
            GitTab::Log => &mut self.log_diff,
            GitTab::Stash => &mut self.stash_diff,
            GitTab::History => &mut self.history_diff,
            GitTab::Commit | GitTab::Console => &mut self.diff,
        }
    }

    fn click_button(&mut self, btn: Btn, cx: &mut Ctx) {
        let req = match btn {
            Btn::Tab(t) => return self.set_tab(t, cx),
            Btn::Commit => return self.commit(false, cx),
            Btn::Amend => return self.toggle_amend(cx),
            Btn::CommitPush => return self.commit(true, cx),
            Btn::Back if self.tab == GitTab::Stash => return self.close_stash_diff(),
            Btn::Back => return self.close_log_diff(),
            Btn::StashPush => return self.stash_push(cx),
            Btn::StashPop => return self.stash_action(StashAction::Pop, cx),
            Btn::StashApply => return self.stash_action(StashAction::Apply, cx),
            Btn::StashDrop => return self.stash_action(StashAction::Drop, cx),
            Btn::DiffLayout => return self.toggle_diff_layout(),
            Btn::LogClearFilter => return self.set_log_filter(LogFilter::default(), cx),
            Btn::LogFilter(part) => return self.log_filter_dialog(part, cx),
            Btn::DiffCollapse => return self.toggle_diff_collapse(),
            Btn::HistoryRevert => return self.history_revert(cx),
            Btn::HistoryList => return self.history_list(cx),
            Btn::Refresh => GitRequest::Refresh,
            Btn::Fetch => GitRequest::Fetch,
            Btn::Pull => GitRequest::Pull,
            Btn::Push => GitRequest::Push,
            Btn::StageAll => GitRequest::StageAll,
            Btn::UnstageAll => GitRequest::UnstageAll,
        };
        cx.effects.push(Effect::Git(req));
    }

    // ---- rendering -------------------------------------------------------

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let title = match self.branch_label() {
            Some(b) => format!(" Git: {b} "),
            None => " Git ".to_string(),
        };
        let block = tool_window_block(&title, focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.hits.clear();
        for r in [
            &mut self.changes_rect,
            &mut self.message_rect,
            &mut self.diff_rect,
            &mut self.log_rect,
            &mut self.files_rect,
            &mut self.stash_rect,
            &mut self.stash_files_rect,
            &mut self.console_rect,
        ] {
            *r = Rect::default();
        }
        if inner.height < 2 || inner.width < 10 {
            return;
        }
        self.render_header(frame.buffer_mut(), Rect::new(inner.x, inner.y, inner.width, 1), theme);
        let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
        match self.tab {
            GitTab::Commit => self.render_commit_tab(frame, body, focused, theme),
            GitTab::Log => self.render_log_tab(frame, body, focused, theme),
            GitTab::Stash => self.render_stash_tab(frame, body, focused, theme),
            GitTab::Console => self.render_console(frame.buffer_mut(), body, theme),
            GitTab::History => self.render_history_tab(frame, body, focused, theme),
        }
    }

    /// Tabs on the left, toolbar buttons on the right.
    fn render_header(&mut self, buf: &mut Screen, r: Rect, theme: &Theme) {
        buf.set_style(r, Style::default().bg(theme.panel_bg));
        let mut x = r.x;
        let stash = match self.stashes.len() {
            0 => " Stash ".to_string(),
            n => format!(" Stash ({n}) "),
        };
        let tabs = [
            (GitTab::Commit, " Commit "),
            (GitTab::Log, " Log "),
            (GitTab::Stash, stash.as_str()),
            (GitTab::Console, " Console "),
            (GitTab::History, " History "),
        ];
        for (tab, label) in tabs {
            let st = if self.tab == tab { theme.tab_active } else { theme.tab };
            let rect = put(buf, &mut x, r.y, r.right(), label, st);
            self.hits.push((Btn::Tab(tab), rect));
            x += 1;
        }
        // As many tools as fit, Refresh (F5) dropped first.
        let mut tools: &[(Btn, &str)] =
            &[(Btn::Refresh, " Refresh "), (Btn::Fetch, " Fetch "), (Btn::Pull, " Pull "), (Btn::Push, " Push ")];
        let width = |tools: &[(Btn, &str)]| -> u16 { tools.iter().map(|(_, l)| l.width() as u16 + 1).sum() };
        while !tools.is_empty() && r.right() <= x + width(tools) {
            tools = &tools[1..];
        }
        let w = width(tools);
        if !tools.is_empty() {
            let mut tx = r.right() - w;
            for &(b, label) in tools {
                let rect = put(buf, &mut tx, r.y, r.right(), label, theme.title.bg(theme.panel_bg));
                self.hits.push((b, rect));
                tx += 1;
            }
        }
    }

    fn render_console(&mut self, buf: &mut Screen, r: Rect, theme: &Theme) {
        self.console_rect = r;
        if self.console.is_empty() {
            buf.set_stringn(r.x, r.y, "Output of git commands (fetch, pull, push, switch) appears here.", r.width as usize, theme.dim);
            return;
        }
        let h = r.height as usize;
        let max_scroll = self.console.len().saturating_sub(h);
        self.console_scroll = self.console_scroll.min(max_scroll);
        let end = self.console.len() - self.console_scroll;
        let start = end.saturating_sub(h);
        for (i, (kind, line)) in self.console[start..end].iter().enumerate() {
            let st = match kind {
                ConsoleKind::Command => theme.status_accent.add_modifier(Modifier::BOLD),
                ConsoleKind::Output => Style::default().fg(theme.fg),
                ConsoleKind::Error => theme.error,
            };
            buf.set_stringn(r.x, r.y + i as u16, line, r.width as usize, st);
        }
    }
}

/// Write `text` at `*x` clipped to `right`, advance `*x`, return its rect.
fn put(buf: &mut Screen, x: &mut u16, y: u16, right: u16, text: &str, style: Style) -> Rect {
    let w = (text.width() as u16).min(right.saturating_sub(*x));
    if w > 0 {
        buf.set_stringn(*x, y, text, w as usize, style);
    }
    let r = Rect::new(*x, y, w, 1);
    *x += w;
    r
}
