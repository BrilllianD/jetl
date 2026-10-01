//! Git tool window (bottom): tabs Commit | Log | Console.
//!
//! Commit: change list grouped Staged / Unstaged / Untracked with stage
//! checkboxes, the diff of the selected entry, and the commit message box.
//! Log: `git log --graph` rows, details of the selected commit, and a diff
//! of a file in it. Console: output of CLI operations.
//!
//! The panel only holds state; all git work is requested through
//! `Effect::Git` and the results are fed back by the app.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::diff::{DiffDoc, DiffRow, RowKind};
use super::model::{BranchInfo, ChangeEntry, CommitDetails, DiffKey, LogRow, Section, StatusSnapshot};
use super::GitRequest;
use crate::actions::GitAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::focus::PanelId;
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitTab {
    Commit,
    Log,
    Console,
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
    Console,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Tab(GitTab),
    Refresh,
    Fetch,
    Pull,
    Push,
    StageAll,
    UnstageAll,
    Commit,
    CommitPush,
    Back,
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
    console: Vec<(ConsoleKind, String)>,
    /// Lines scrolled up from the bottom of the console.
    console_scroll: usize,
    pub branches: Vec<BranchInfo>,
    /// The branch popup opens when the next branch list arrives.
    pub pending_branch_popup: bool,
    hits: Vec<(Btn, Rect)>,
    changes_rect: Rect,
    message_rect: Rect,
    diff_rect: Rect,
    log_rect: Rect,
    files_rect: Rect,
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
            console: Vec::new(),
            console_scroll: 0,
            branches: Vec::new(),
            pending_branch_popup: false,
            hits: Vec::new(),
            changes_rect: Rect::default(),
            message_rect: Rect::default(),
            diff_rect: Rect::default(),
            log_rect: Rect::default(),
            files_rect: Rect::default(),
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
            GitTab::Console => Area::Console,
        }
    }

    // ---- data from the service -------------------------------------------

    /// Apply a status result. Returns false when it is older than one
    /// already applied.
    pub fn set_status(&mut self, seq: u64, result: anyhow::Result<StatusSnapshot>, cx: &mut Ctx) -> bool {
        if seq < self.status_seq {
            return false;
        }
        self.status_seq = seq;
        let prev = self.selected_entry().map(|e| (e.section, e.rel.clone()));
        let prev_index = self.list.selected;
        match result {
            Ok(s) => {
                self.snapshot = Some(s);
                self.status_error = None;
            }
            Err(e) => {
                self.snapshot = None;
                self.status_error = Some(format!("{e:#}"));
            }
        }
        self.rebuild_rows();
        // Keep the same entry selected; when it moved to another section
        // (staged/unstaged) keep the row position instead.
        let again = prev.and_then(|(sec, rel)| self.rows.iter().position(|r| matches!(r, ListRow::Entry(i) if self.entry(*i).is_some_and(|e| e.section == sec && e.rel == rel))));
        match again {
            Some(i) => self.list.select(i),
            None => {
                // Land on an entry: a header is always followed by one.
                self.list.select(prev_index);
                if matches!(self.rows.get(self.list.selected), Some(ListRow::Header(..))) {
                    self.list.down(1);
                }
            }
        }
        self.sync_diff(true, cx);
        true
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        if let Some(s) = &self.snapshot {
            for sec in Section::ALL {
                let n = s.count(sec);
                if n == 0 {
                    continue;
                }
                self.rows.push(ListRow::Header(sec, n));
                for (i, e) in s.entries.iter().enumerate() {
                    if e.section == sec {
                        self.rows.push(ListRow::Entry(i));
                    }
                }
            }
        }
        self.list.set_len(self.rows.len());
    }

    fn entry(&self, i: usize) -> Option<&ChangeEntry> {
        self.snapshot.as_ref().and_then(|s| s.entries.get(i))
    }

    fn selected_entry(&self) -> Option<&ChangeEntry> {
        match self.rows.get(self.list.selected)? {
            ListRow::Entry(i) => self.entry(*i),
            ListRow::Header(..) => None,
        }
    }

    fn staged_count(&self) -> usize {
        self.snapshot.as_ref().map(|s| s.count(Section::Staged)).unwrap_or(0)
    }

    /// Show the diff of the selected entry. With `reload` the current one is
    /// fetched again (its content may have changed).
    fn sync_diff(&mut self, reload: bool, cx: &mut Ctx) {
        let key = self.selected_entry().map(|e| DiffKey::Change { section: e.section, rel: e.rel.clone() });
        match key {
            Some(k) if reload || self.diff.key.as_ref() != Some(&k) => self.diff.request(k, cx),
            Some(_) => {}
            None => self.diff.clear(),
        }
    }

    pub fn set_diff(&mut self, key: &DiffKey, result: anyhow::Result<DiffDoc>) {
        match key {
            DiffKey::Change { .. } => self.diff.set(key, result),
            DiffKey::Commit { .. } => self.log_diff.set(key, result),
        }
    }

    pub fn set_log(&mut self, skip: usize, result: anyhow::Result<Vec<LogRow>>, cx: &mut Ctx) {
        self.log_loading = false;
        let rows = match result {
            Ok(r) => r,
            Err(e) => {
                cx.notify(Level::Error, format!("git log: {e:#}"));
                return;
            }
        };
        let commits = rows.iter().filter(|r| r.commit.is_some()).count();
        if skip == 0 {
            let prev = self.selected_commit().map(|c| c.to_string());
            self.log = rows;
            self.log_list.set_len(self.log.len());
            let again = prev.and_then(|sha| self.log.iter().position(|r| r.commit.as_ref().is_some_and(|c| c.sha == sha)));
            self.log_list.select(again.unwrap_or(0));
        } else if skip == self.commit_count() {
            self.log.extend(rows);
            self.log_list.set_len(self.log.len());
        } else {
            return;
        }
        self.log_loaded = true;
        self.log_end = commits < self.log_page;
        self.sync_details(cx);
    }

    fn commit_count(&self) -> usize {
        self.log.iter().filter(|r| r.commit.is_some()).count()
    }

    fn selected_commit(&self) -> Option<&str> {
        self.log.get(self.log_list.selected).and_then(|r| r.commit.as_ref()).map(|c| c.sha.as_str())
    }

    /// Load details of the selected commit when they are not shown yet.
    fn sync_details(&mut self, cx: &mut Ctx) {
        let Some(sha) = self.selected_commit().map(str::to_string) else { return };
        if self.details.as_ref().is_some_and(|d| d.sha == sha) {
            return;
        }
        self.details = None;
        self.details_error = None;
        self.files_list = ListState::default();
        cx.effects.push(Effect::Git(GitRequest::LoadCommit(sha)));
    }

    pub fn set_details(&mut self, sha: &str, result: anyhow::Result<CommitDetails>) {
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
        if self.log_loading {
            return;
        }
        self.log_loading = true;
        cx.effects.push(Effect::Git(GitRequest::LoadLog { skip: 0 }));
    }

    /// Fetch the next page when the selection or view reached the end.
    fn maybe_page_log(&mut self, cx: &mut Ctx) {
        let at_end = self.log_list.len > 0 && (self.log_list.selected + 1 >= self.log_list.len || self.log_list.offset + self.log_list.viewport >= self.log_list.len);
        if at_end && self.log_loaded && !self.log_end && !self.log_loading {
            self.log_loading = true;
            cx.effects.push(Effect::Git(GitRequest::LoadLog { skip: self.commit_count() }));
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

    // ---- input -----------------------------------------------------------

    pub fn message_focused(&self) -> bool {
        self.tab == GitTab::Commit && self.area == Area::Message
    }

    /// Keys while the commit message has focus. Returns false for keys it
    /// does not use so global shortcuts still work.
    pub fn handle_message_key(&mut self, key: KeyEvent, cx: &mut Ctx) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let m = &mut self.message;
        match key.code {
            // Ctrl+Enter needs the kitty protocol; Alt+Enter works anywhere.
            KeyCode::Enter if ctrl || alt => self.commit(false, cx),
            KeyCode::Enter => m.insert('\n'),
            KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab => self.area = Area::Changes,
            KeyCode::Backspace if ctrl || alt => m.delete_word_back(),
            KeyCode::Char('h') if ctrl => m.backspace(),
            KeyCode::Backspace => m.backspace(),
            KeyCode::Delete => m.delete(),
            KeyCode::Left => m.left(),
            KeyCode::Right => m.right(),
            KeyCode::Up => m.up(),
            KeyCode::Down => m.down(),
            KeyCode::Home => m.home(),
            KeyCode::End => m.end(),
            KeyCode::Char(c) if !ctrl && !alt => m.insert(c),
            _ => return false,
        }
        true
    }

    pub fn paste_into_message(&mut self, text: &str) {
        self.message.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
    }

    fn commit(&mut self, push: bool, cx: &mut Ctx) {
        let message = self.message.text.trim_end().to_string();
        if message.trim().is_empty() {
            cx.notify(Level::Warn, "Commit message is empty");
            self.tab = GitTab::Commit;
            self.area = Area::Message;
            return;
        }
        if self.snapshot.is_some() && self.staged_count() == 0 {
            cx.notify(Level::Warn, "Nothing to commit: no staged changes (Space stages the selected file)");
            return;
        }
        cx.effects.push(Effect::Git(GitRequest::Commit { message, push }));
    }

    fn set_tab(&mut self, tab: GitTab, cx: &mut Ctx) {
        self.tab = tab;
        self.area = Self::default_area(tab);
        if tab == GitTab::Log && !self.log_loaded {
            self.request_log(cx);
        }
        if tab == GitTab::Console {
            self.console_scroll = 0;
        }
    }

    /// Stage or unstage the selected entry, or a whole section on its header.
    fn toggle_selected(&mut self, cx: &mut Ctx) {
        let Some(row) = self.rows.get(self.list.selected).copied() else { return };
        let Some(s) = &self.snapshot else { return };
        let (section, rels): (Section, Vec<String>) = match row {
            ListRow::Entry(i) => (s.entries[i].section, vec![s.entries[i].rel.clone()]),
            ListRow::Header(sec, _) => (sec, s.entries.iter().filter(|e| e.section == sec).map(|e| e.rel.clone()).collect()),
        };
        let req = if section == Section::Staged { GitRequest::Unstage(rels) } else { GitRequest::Stage(rels) };
        cx.effects.push(Effect::Git(req));
    }

    fn open_selected_file(&mut self, cx: &mut Ctx) {
        let Some(e) = self.selected_entry() else { return };
        if e.abs.is_file() {
            cx.open_file(e.abs.clone());
        } else {
            cx.notify(Level::Info, format!("{} is deleted", e.rel));
        }
    }

    fn open_commit_file(&mut self, cx: &mut Ctx) {
        let Some(d) = &self.details else { return };
        let Some((_, rel)) = d.files.get(self.files_list.selected) else { return };
        let key = DiffKey::Commit { sha: d.sha.clone(), rel: rel.clone() };
        self.log_diff.request(key, cx);
        self.area = Area::LogDiff;
    }

    fn close_log_diff(&mut self) {
        self.log_diff.clear();
        self.area = Area::Files;
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
            G::UnstageAll => cx.effects.push(Effect::Git(GitRequest::UnstageAll)),
            G::Commit => self.commit(false, cx),
            G::CommitAndPush => self.commit(true, cx),
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
            G::Up => self.move_by(-1, cx),
            G::Down => self.move_by(1, cx),
            G::PageUp => self.move_by(-(self.page() as isize), cx),
            G::PageDown => self.move_by(self.page() as isize, cx),
            G::Left => match self.area {
                Area::Diff => self.area = Area::Changes,
                Area::Files => self.area = Area::Log,
                Area::LogDiff => self.close_log_diff(),
                Area::Changes | Area::Message | Area::Log | Area::Console => {}
            },
            G::Right => match self.area {
                Area::Changes if self.diff.key.is_some() => self.area = Area::Diff,
                Area::Log if self.details.is_some() => self.area = Area::Files,
                Area::Files => self.open_commit_file(cx),
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
                Area::Message | Area::LogDiff | Area::Console => {}
            },
            G::NextSection => {
                self.area = match self.area {
                    Area::Changes => Area::Diff,
                    Area::Diff => Area::Message,
                    Area::Message => Area::Changes,
                    Area::Log => Area::Files,
                    Area::Files => Area::Log,
                    a => a,
                }
            }
            G::Escape => match self.area {
                Area::LogDiff => self.close_log_diff(),
                Area::Message | Area::Diff => self.area = Area::Changes,
                Area::Files => self.area = Area::Log,
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
                    if delta < 0 { self.log_list.up(1) } else { self.log_list.down(1) }
                    if self.log_list.selected == before {
                        break;
                    }
                }
                self.sync_details(cx);
                self.maybe_page_log(cx);
            }
            Area::Files => step(&mut self.files_list),
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
                    GitTab::Console => self.area = Area::Console,
                }
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d: isize = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                if self.changes_rect.contains(p) {
                    self.list.scroll(d as i32);
                } else if self.diff_rect.contains(p) {
                    if self.tab == GitTab::Log { self.log_diff.scroll_by(d) } else { self.diff.scroll_by(d) }
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
                let pane = if self.tab == GitTab::Log { &mut self.log_diff } else { &mut self.diff };
                if self.diff_rect.contains(p) {
                    pane.hscroll = if m.kind == MouseEventKind::ScrollLeft { pane.hscroll.saturating_sub(4) } else { pane.hscroll + 4 };
                }
                true
            }
            _ => false,
        }
    }

    fn click_button(&mut self, btn: Btn, cx: &mut Ctx) {
        let req = match btn {
            Btn::Tab(t) => return self.set_tab(t, cx),
            Btn::Commit => return self.commit(false, cx),
            Btn::CommitPush => return self.commit(true, cx),
            Btn::Back => return self.close_log_diff(),
            Btn::Refresh => GitRequest::Refresh,
            Btn::Fetch => GitRequest::Fetch,
            Btn::Pull => GitRequest::Pull,
            Btn::Push => GitRequest::Push,
            Btn::StageAll => GitRequest::StageAll,
            Btn::UnstageAll => GitRequest::UnstageAll,
        };
        cx.effects.push(Effect::Git(req));
    }

    fn click_commit_tab(&mut self, m: &Mouse, p: Position, cx: &mut Ctx) {
        if self.message_rect.contains(p) {
            self.area = Area::Message;
            self.message.click(self.message_rect, m.x, m.y);
        } else if self.diff_rect.contains(p) {
            self.area = Area::Diff;
        } else if let Some(i) = self.list.hit(self.changes_rect, m.x, m.y) {
            self.area = Area::Changes;
            self.list.select(i);
            // The checkbox sits at columns 2..5 of entry rows (headers: 0..).
            let cb = self.changes_rect.x + if matches!(self.rows[i], ListRow::Entry(_)) { 2 } else { 0 };
            if m.x >= cb && m.x < cb + 3 {
                self.toggle_selected(cx);
            } else if m.clicks >= 2 {
                self.open_selected_file(cx);
            }
            self.sync_diff(false, cx);
        } else if self.changes_rect.contains(p) {
            self.area = Area::Changes;
        }
    }

    fn click_log_tab(&mut self, m: &Mouse, p: Position, cx: &mut Ctx) {
        if self.log_diff.key.is_some() {
            if self.diff_rect.contains(p) {
                self.area = Area::LogDiff;
            }
            return;
        }
        if let Some(i) = self.log_list.hit(self.log_rect, m.x, m.y) {
            self.area = Area::Log;
            self.log_list.select(i);
            self.sync_details(cx);
            self.maybe_page_log(cx);
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
        for r in [&mut self.changes_rect, &mut self.message_rect, &mut self.diff_rect, &mut self.log_rect, &mut self.files_rect, &mut self.console_rect] {
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
            GitTab::Console => self.render_console(frame.buffer_mut(), body, theme),
        }
    }

    /// Tabs on the left, toolbar buttons on the right.
    fn render_header(&mut self, buf: &mut Screen, r: Rect, theme: &Theme) {
        buf.set_style(r, Style::default().bg(theme.panel_bg));
        let mut x = r.x;
        for (tab, label) in [(GitTab::Commit, " Commit "), (GitTab::Log, " Log "), (GitTab::Console, " Console ")] {
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

    fn render_commit_tab(&mut self, frame: &mut Frame, body: Rect, focused: bool, theme: &Theme) {
        let left_w = if body.width >= 70 { (body.width * 2 / 5).max(30) } else { body.width };
        let left = Rect::new(body.x, body.y, left_w, body.height);
        if body.width > left_w + 1 {
            let sep_x = body.x + left_w;
            for y in body.y..body.bottom() {
                frame.buffer_mut()[(sep_x, y)].set_symbol("│").set_style(theme.splitter);
            }
            let right = Rect::new(sep_x + 1, body.y, body.right() - sep_x - 1, body.height);
            self.diff_rect = right;
            let header = self.diff.key.as_ref().map(|k| match k {
                DiffKey::Change { section: Section::Staged, rel } => format!("{rel}  (HEAD ↔ staged)"),
                DiffKey::Change { section: Section::Unstaged, rel } => format!("{rel}  (staged ↔ working tree)"),
                DiffKey::Change { section: Section::Untracked, rel } => format!("{rel}  (new file)"),
                DiffKey::Commit { rel, .. } => rel.clone(),
            });
            let pane_focused = focused && self.area == Area::Diff;
            render_diff(frame.buffer_mut(), right, &mut self.diff, header.as_deref(), pane_focused, theme);
        }

        // Left column, bottom up: commit buttons, message, label, stage buttons, list.
        let h = left.height;
        let small = h < 8;
        let msg_h = if small { 1 } else { ((h - 4) / 3).clamp(1, 6) };
        let fixed = msg_h + if small { 1 } else { 3 };
        let list_h = h.saturating_sub(fixed);
        let buf = frame.buffer_mut();
        let mut y = left.y + list_h;
        self.changes_rect = Rect::new(left.x, left.y, left.width, list_h);
        self.render_changes(buf, self.changes_rect, focused, theme);
        if !small {
            let mut x = left.x;
            let r = put(buf, &mut x, y, left.right(), "[Stage all]", theme.title);
            self.hits.push((Btn::StageAll, r));
            x += 1;
            let r = put(buf, &mut x, y, left.right(), "[Unstage all]", theme.title);
            self.hits.push((Btn::UnstageAll, r));
            y += 1;
            let label = if self.area == Area::Message && focused { "Commit message (Ctrl/Alt+Enter commits, Esc leaves)" } else { "Commit message (Ctrl+K)" };
            buf.set_stringn(left.x, y, label, left.width as usize, theme.dim);
            y += 1;
        }
        self.message_rect = Rect::new(left.x, y, left.width, msg_h);
        let msg_focused = focused && self.area == Area::Message;
        self.message.render(frame, self.message_rect, Style::default().bg(theme.bg).fg(theme.fg), "Describe the change", theme.dim.bg(theme.bg), msg_focused);
        y += msg_h;
        if y < left.bottom() {
            let buf = frame.buffer_mut();
            let mut x = left.x;
            let style = if self.staged_count() > 0 { theme.tab_active } else { theme.tab };
            let r = put(buf, &mut x, y, left.right(), " Commit ", style);
            self.hits.push((Btn::Commit, r));
            x += 1;
            let r = put(buf, &mut x, y, left.right(), " Commit and Push ", theme.tab);
            self.hits.push((Btn::CommitPush, r));
        }
    }

    fn render_changes(&mut self, buf: &mut Screen, r: Rect, focused: bool, theme: &Theme) {
        self.list.set_viewport(r.height as usize);
        if r.height == 0 {
            return;
        }
        if let Some(e) = &self.status_error {
            buf.set_stringn(r.x, r.y, crate::util::text::fit(e, r.width as usize), r.width as usize, theme.dim);
            return;
        }
        let Some(snap) = &self.snapshot else {
            buf.set_stringn(r.x, r.y, "Loading…", r.width as usize, theme.dim);
            return;
        };
        if self.rows.is_empty() {
            buf.set_stringn(r.x, r.y, "No changes", r.width as usize, theme.dim);
            return;
        }
        let active = focused && self.area == Area::Changes;
        for (vi, i) in self.list.visible().enumerate() {
            let y = r.y + vi as u16;
            let selected = i == self.list.selected;
            let base = if selected {
                if active { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let mut x = r.x;
            match self.rows[i] {
                ListRow::Header(sec, n) => {
                    let label = format!("▾ {} ({n})", sec.title());
                    put(buf, &mut x, y, r.right(), &label, base.add_modifier(Modifier::BOLD));
                }
                ListRow::Entry(ei) => {
                    let e = &snap.entries[ei];
                    let check = if e.section == Section::Staged { "[x]" } else { "[ ]" };
                    x += 2;
                    put(buf, &mut x, y, r.right(), check, base);
                    x += 1;
                    let st_style = e.status.style(theme);
                    let letter_style = if selected { base } else { st_style };
                    put(buf, &mut x, y, r.right(), &e.status.letter().to_string(), letter_style);
                    x += 1;
                    let (dir, name) = match e.rel.rsplit_once('/') {
                        Some((d, n)) => (d, n),
                        None => ("", e.rel.as_str()),
                    };
                    let name_style = if selected { base } else { Style::default().fg(st_style.fg.unwrap_or(theme.fg)) };
                    put(buf, &mut x, y, r.right(), name, name_style);
                    if !dir.is_empty() {
                        x += 1;
                        put(buf, &mut x, y, r.right(), dir, if selected { base } else { theme.dim });
                    }
                }
            }
        }
    }

    fn render_log_tab(&mut self, frame: &mut Frame, body: Rect, focused: bool, theme: &Theme) {
        let buf = frame.buffer_mut();
        if let Some(key) = self.log_diff.key.clone() {
            let mut x = body.x;
            let r = put(buf, &mut x, body.y, body.right(), "[← Back]", theme.title.bg(theme.panel_bg));
            self.hits.push((Btn::Back, r));
            let header = match &key {
                DiffKey::Commit { sha, rel } => format!(" {}  {rel}", &sha[..sha.len().min(8)]),
                DiffKey::Change { rel, .. } => format!(" {rel}"),
            };
            put(buf, &mut x, body.y, body.right(), &header, theme.title);
            let rest = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
            self.diff_rect = rest;
            render_diff(buf, rest, &mut self.log_diff, None, focused && self.area == Area::LogDiff, theme);
            return;
        }
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
            let msg = if self.log_loading { "Loading…" } else { "No commits yet" };
            buf.set_stringn(r.x, r.y, msg, r.width as usize, theme.dim);
            return;
        }
        let palette = [Color::Rgb(104, 151, 187), Color::Rgb(98, 150, 85), Color::Rgb(204, 120, 50), Color::Rgb(152, 118, 170), Color::Rgb(187, 181, 41), Color::Rgb(32, 153, 157)];
        for (vi, i) in self.log_list.visible().enumerate() {
            let row = &self.log[i];
            let y = r.y + vi as u16;
            let selected = i == self.log_list.selected && row.commit.is_some();
            let base = if selected {
                if active { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let mut x = r.x;
            for (col, ch) in row.graph.chars().enumerate() {
                let glyph = match ch {
                    '*' => "●",
                    '|' => "│",
                    ' ' => " ",
                    _ => "",
                };
                let mut tmp = [0u8; 4];
                let s = if glyph.is_empty() { ch.encode_utf8(&mut tmp) } else { glyph };
                let color = palette[(col / 2) % palette.len()];
                let st = if selected { base } else { Style::default().fg(color) };
                put(buf, &mut x, y, r.right(), s, st);
            }
            let Some(c) = &row.commit else { continue };
            let mut right = String::new();
            if !c.author.is_empty() {
                right = format!(" {}, {}", c.author, c.date);
            }
            let right_w = right.width() as u16;
            put(buf, &mut x, y, r.right(), &c.short, if selected { base } else { theme.warn });
            x += 1;
            if !c.refs.is_empty() {
                put(buf, &mut x, y, r.right(), &format!("({})", c.refs), if selected { base.add_modifier(Modifier::BOLD) } else { theme.status_accent.add_modifier(Modifier::BOLD) });
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
        let commit = self.log.get(self.log_list.selected).and_then(|row| row.commit.as_ref());
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
            let (st, path) = &d.files[i];
            let selected = i == self.files_list.selected && active;
            let base = if selected { theme.list_selected } else { Style::default().fg(theme.fg) };
            if selected {
                buf.set_style(Rect::new(r.x, fy, r.width, 1), base);
            }
            let st_style = match st {
                'A' => theme.git_added,
                'D' => theme.git_deleted,
                _ => theme.git_modified,
            };
            let mut x = x0;
            put(buf, &mut x, fy, r.right(), &st.to_string(), if selected { base } else { st_style });
            x += 1;
            put(buf, &mut x, fy, r.right(), path, base);
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
