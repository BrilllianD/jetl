//! Git tool window (bottom): tabs Commit | Log | Stash | Console.
//!
//! Commit: change list grouped Staged / Unstaged / Untracked with stage
//! checkboxes, the diff of the selected entry, and the commit message box.
//! Log: `git log --graph` rows, details of the selected commit, and a diff
//! of a file in it. Stash: the stash list, files of a stash and their diff.
//! Console: output of CLI operations.
//!
//! This module holds the shared state, input routing, the header, the
//! console and the diff renderer; `commit.rs`, `log.rs` and `stash.rs`
//! hold the tabs.
//!
//! The panel only holds state; all git work is requested through
//! `Effect::Git` and the results are fed back by the app.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::GitRequest;
use super::diff::{DiffDoc, DiffRow, RowKind};
use super::model::{BranchInfo, CommitDetails, DiffKey, LogRow, Section, StashEntry, StatusSnapshot};
use crate::actions::GitAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Mouse};
use crate::ui::focus::PanelId;
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

mod commit;
mod log;
mod stash;

use stash::StashAction;

/// Stored in sessions (`SessionLayout::git_tab`); add variants, never
/// rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GitTab {
    Commit,
    Log,
    Console,
    Stash,
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

/// The diff view: what is shown, the computed rows, and the shared
/// vertical scroll of both sides.
#[derive(Debug, Default)]
struct DiffPane {
    key: Option<DiffKey>,
    doc: Option<DiffDoc>,
    error: Option<String>,
    loading: bool,
    scroll: usize,
    hscroll: usize,
    /// Rendered unified (narrow) last frame; scroll counts those rows.
    unified: bool,
    page: usize,
}

impl DiffPane {
    fn request(&mut self, key: DiffKey, cx: &mut Ctx) {
        if self.key.as_ref() != Some(&key) {
            self.doc = None;
            self.scroll = 0;
            self.hscroll = 0;
        }
        self.key = Some(key.clone());
        self.loading = true;
        self.error = None;
        cx.effects.push(Effect::Git(GitRequest::LoadDiff(key)));
    }

    fn clear(&mut self) {
        *self = DiffPane { unified: self.unified, page: self.page, ..Default::default() };
    }

    /// Result for `key`; ignored when the pane shows something else now.
    fn set(&mut self, key: &DiffKey, result: anyhow::Result<DiffDoc>) {
        if self.key.as_ref() != Some(key) {
            return;
        }
        self.loading = false;
        match result {
            Ok(doc) => {
                // Jump to the first change of a newly opened diff only.
                if self.doc.is_none() {
                    self.scroll = doc.first_change(self.unified).unwrap_or(0).saturating_sub(2);
                }
                self.doc = Some(doc);
                self.error = None;
            }
            Err(e) => {
                self.doc = None;
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    fn rows_len(&self) -> usize {
        self.doc.as_ref().map(|d| if self.unified { d.unified.len() } else { d.rows.len() }).unwrap_or(0)
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.rows_len().saturating_sub(self.page.max(1));
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }
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
    log_list: ListState,
    log_loaded: bool,
    log_loading: bool,
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
            diff: DiffPane::default(),
            log: Vec::new(),
            log_list: ListState::default(),
            log_loaded: false,
            log_loading: false,
            log_end: false,
            log_page: 500,
            details: None,
            details_error: None,
            files_list: ListState::default(),
            log_diff: DiffPane::default(),
            stashes: Vec::new(),
            stash_error: None,
            stash_list: ListState::default(),
            stash_details: None,
            stash_details_error: None,
            stash_files: ListState::default(),
            stash_diff: DiffPane::default(),
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
        let log_page = self.log_page;
        *self = Self::new();
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

    fn default_area(tab: GitTab) -> Area {
        match tab {
            GitTab::Commit => Area::Changes,
            GitTab::Log => Area::Log,
            GitTab::Stash => Area::Stashes,
            GitTab::Console => Area::Console,
        }
    }

    pub fn set_diff(&mut self, key: &DiffKey, result: anyhow::Result<DiffDoc>) {
        match key {
            DiffKey::Change { .. } => self.diff.set(key, result),
            // A commit diff is shown by the Log tab or by the Stash tab
            // (stashes are commits); each pane ignores keys it does not show.
            DiffKey::Commit { .. } if self.stash_diff.key.as_ref() == Some(key) => self.stash_diff.set(key, result),
            DiffKey::Commit { .. } => self.log_diff.set(key, result),
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
            G::Refresh => cx.effects.push(Effect::Git(GitRequest::Refresh)),
            G::ToggleStage => {
                if self.tab == GitTab::Commit {
                    self.toggle_selected(cx);
                }
            }
            G::StageAll => cx.effects.push(Effect::Git(GitRequest::StageAll)),
            G::Discard => {
                if self.tab == GitTab::Commit {
                    self.discard_selected(cx);
                }
            }
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
                Area::Changes | Area::Message | Area::Log | Area::Stashes | Area::Console => {}
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
                // Skip graph connector rows in the direction of travel.
                while self.log.get(self.log_list.selected).is_some_and(|r| r.commit.is_none()) {
                    let before = self.log_list.selected;
                    if delta < 0 {
                        self.log_list.up(1)
                    } else {
                        self.log_list.down(1)
                    }
                    if self.log_list.selected == before {
                        break;
                    }
                }
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
                match self.tab {
                    GitTab::Commit => self.click_commit_tab(m, p, cx),
                    GitTab::Log => self.click_log_tab(m, p, cx),
                    GitTab::Stash => self.click_stash_tab(m, p, cx),
                    GitTab::Console => self.area = Area::Console,
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

    /// The diff pane of the current tab.
    fn diff_pane(&mut self) -> &mut DiffPane {
        match self.tab {
            GitTab::Log => &mut self.log_diff,
            GitTab::Stash => &mut self.stash_diff,
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
        for (tab, label) in
            [(GitTab::Commit, " Commit "), (GitTab::Log, " Log "), (GitTab::Stash, stash.as_str()), (GitTab::Console, " Console ")]
        {
            let st = if self.tab == tab { theme.tab_active } else { theme.tab };
            let rect = put(buf, &mut x, r.y, r.right(), label, st);
            self.hits.push((Btn::Tab(tab), rect));
            x += 1;
        }
        let tools = [(Btn::Refresh, " Refresh "), (Btn::Fetch, " Fetch "), (Btn::Pull, " Pull "), (Btn::Push, " Push ")];
        let w: u16 = tools.iter().map(|(_, l)| l.width() as u16 + 1).sum();
        if r.right() > x + w {
            let mut tx = r.right() - w;
            for (b, label) in tools {
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

/// Draw `text` from display column `skip` into `width` cells at (x, y).
fn put_cols(buf: &mut Screen, x: u16, y: u16, width: u16, text: &str, skip: usize, style: Style) {
    let mut col = 0usize;
    let mut cx = x;
    let right = x + width;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if w == 0 {
            continue;
        }
        if col < skip {
            // A wide char cut by the scroll edge leaves a blank.
            if col + w > skip {
                cx += (col + w - skip) as u16;
            }
            col += w;
            continue;
        }
        if cx + w as u16 > right {
            break;
        }
        let mut tmp = [0u8; 4];
        buf[(cx, y)].set_symbol(ch.encode_utf8(&mut tmp)).set_style(style);
        for i in 1..w as u16 {
            buf[(cx + i, y)].reset();
            buf[(cx + i, y)].set_style(style);
        }
        cx += w as u16;
        col += w;
    }
}

/// Render a diff pane: optional header row, then side-by-side rows, or
/// unified rows when narrower than 100 columns. Both sides share one
/// vertical scroll.
fn render_diff(buf: &mut Screen, area: Rect, pane: &mut DiffPane, header: Option<&str>, focused: bool, theme: &Theme) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let mut r = area;
    if let Some(h) = header {
        let style = if focused { theme.title_focused } else { theme.title };
        buf.set_stringn(r.x + 1, r.y, crate::util::text::fit(h, r.width.saturating_sub(1) as usize), r.width as usize, style);
        r = Rect::new(r.x, r.y + 1, r.width, r.height - 1);
    }
    pane.page = r.height as usize;
    let msg = |buf: &mut Screen, text: &str, style: Style| buf.set_stringn(r.x + 1, r.y, text, r.width.saturating_sub(1) as usize, style);
    if r.height == 0 {
        return;
    }
    if pane.key.is_none() {
        msg(buf, "Select a file to see its changes", theme.dim);
        return;
    }
    if let Some(e) = &pane.error {
        msg(buf, e, theme.error);
        return;
    }
    let Some(doc) = &pane.doc else {
        if pane.loading {
            msg(buf, "Loading…", theme.dim);
        }
        return;
    };
    if doc.binary {
        msg(buf, "Binary file; no text diff", theme.dim);
        return;
    }
    if doc.rows.is_empty() {
        msg(buf, "Empty file", theme.dim);
        return;
    }
    let unified = r.width < 100;
    if unified != pane.unified {
        // Keep roughly the same place when the layout switches modes.
        pane.scroll = map_scroll(doc, pane.scroll, pane.unified);
        pane.unified = unified;
    }
    let rows: &[DiffRow] = if unified { &doc.unified } else { &doc.rows };
    pane.scroll = pane.scroll.min(rows.len().saturating_sub(r.height as usize));
    let digits = doc.old.len().max(doc.new.len()).max(1).to_string().len().max(3) as u16;
    let num_style = theme.line_number;
    let add = Style::default().fg(theme.fg).bg(theme.diff_add_bg);
    let del = Style::default().fg(theme.fg).bg(theme.diff_del_bg);
    let modi = Style::default().fg(theme.fg).bg(theme.diff_mod_bg);
    let plain = Style::default().fg(theme.fg).bg(theme.bg);
    let filler = Style::default().bg(theme.panel_bg);
    for (vi, row) in rows.iter().skip(pane.scroll).take(r.height as usize).enumerate() {
        let y = r.y + vi as u16;
        if unified {
            // old# new# sign text
            let (style, sign) = match row.kind {
                RowKind::Removed => (del, "-"),
                RowKind::Added => (add, "+"),
                _ => (plain, " "),
            };
            let nums_w = digits * 2 + 2;
            buf.set_style(Rect::new(r.x + nums_w, y, r.width.saturating_sub(nums_w), 1), style);
            if let Some(l) = row.left {
                buf.set_stringn(r.x, y, format!("{:>w$}", l + 1, w = digits as usize), digits as usize, num_style);
            }
            if let Some(l) = row.right {
                buf.set_stringn(r.x + digits + 1, y, format!("{:>w$}", l + 1, w = digits as usize), digits as usize, num_style);
            }
            buf.set_stringn(r.x + nums_w, y, sign, 1, style);
            let text = row.right.map(|l| &doc.new[l]).or(row.left.map(|l| &doc.old[l])).map(String::as_str).unwrap_or("");
            let tx = r.x + nums_w + 1;
            put_cols(buf, tx, y, r.right().saturating_sub(tx), text, pane.hscroll, style);
        } else {
            let half = (r.width - 1) / 2;
            let sides = [(r.x, row.left, &doc.old), (r.x + half + 1, row.right, &doc.new)];
            for (side, (sx, line, lines)) in sides.into_iter().enumerate() {
                let w = if side == 0 { half } else { r.right() - sx };
                let style = match (row.kind, line) {
                    (_, None) => filler,
                    (RowKind::Equal, _) => plain,
                    (RowKind::Modified, _) => modi,
                    (RowKind::Removed, _) => del,
                    (RowKind::Added, _) => add,
                };
                let text_x = sx + digits + 1;
                buf.set_style(Rect::new(text_x, y, w.saturating_sub(digits + 1), 1), style);
                if let Some(l) = line {
                    buf.set_stringn(sx, y, format!("{:>w$}", l + 1, w = digits as usize), digits as usize, num_style);
                    put_cols(buf, text_x, y, (sx + w).saturating_sub(text_x), &lines[l], pane.hscroll, style);
                }
            }
            buf[(r.x + half, y)].set_symbol("│").set_style(theme.splitter);
        }
    }
}

/// Translate a scroll position between side-by-side and unified rows by
/// the old/new line it shows.
fn map_scroll(doc: &DiffDoc, scroll: usize, from_unified: bool) -> usize {
    let (from, to) = if from_unified { (&doc.unified, &doc.rows) } else { (&doc.rows, &doc.unified) };
    let Some(row) = from.get(scroll) else { return 0 };
    to.iter().position(|r| (row.right.is_some() && r.right == row.right) || (row.left.is_some() && r.left == row.left)).unwrap_or(0)
}
