//! Modal dialogs: confirmation with buttons, text input, and a choice list.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

/// Identifies what the app should do with the dialog's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogId {
    QuitDirty,
    /// Keyed by buffer, not tab index: tabs can open while the dialog is up.
    CloseTabDirty(crate::editor::buffer::BufferId),
    CloseAllDirty,
    /// Close others or close all but pinned with unsaved tabs among them.
    CloseTabsDirty(Vec<crate::editor::buffer::BufferId>),
    /// Tree: File or Directory, then a name, created in the directory.
    TreeNewKind(std::path::PathBuf),
    TreeNew {
        dir: std::path::PathBuf,
        is_dir: bool,
    },
    TreeRename(std::path::PathBuf),
    TreeDelete(std::path::PathBuf),
    /// Replace every Find window result with its replacement text.
    ReplaceInFiles,
    /// Explicit save of a file changed on disk: Overwrite / Reload / Cancel.
    OverwriteExternal(crate::editor::buffer::BufferId),
    /// Unsaved buffers before switching the project root to the path.
    SwitchRootDirty(std::path::PathBuf),
    WorktreeRemove {
        path: std::path::PathBuf,
        force: bool,
    },
    /// Create flow, step 1: item `i` is `worktree::panel::CREATE_KINDS[i]`.
    WorktreeCreateKind,
    /// Create flow, new branch: its name (starts at HEAD).
    WorktreeNewBranchName,
    /// Create flow, existing branch: item `i` is `names[i]`.
    WorktreeExistingBranch(Vec<String>),
    /// Create flow, last step: the directory.
    WorktreePath {
        branch: String,
        new_branch: bool,
    },
    WorktreeSwitchAfterCreate(std::path::PathBuf),
    NewBranchName,
    /// Log filter: which part to edit (Branch, Text, Author, Path, Clear).
    LogFilterKind,
    LogFilterText,
    LogFilterAuthor,
    LogFilterPath,
    /// Log of a branch: item `i` is `revs[i]`.
    LogBranch(Vec<crate::git::LogRev>),
    /// Name of a new branch starting at a commit or branch.
    NewBranchAt {
        rev: String,
    },
    /// The branches popup's operations for a branch: item `i` is `ops[i]`.
    BranchOps {
        branch: crate::git::BranchInfo,
        ops: Vec<crate::git::model::BranchOp>,
    },
    RenameBranch {
        from: String,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    RebaseOnto {
        onto: String,
    },
    MergeBranch {
        name: String,
    },
    NewTag {
        sha: String,
    },
    /// Delete one of these tags (several on the commit: a choice first).
    DeleteTag {
        tags: Vec<String>,
    },
    RevertCommit {
        sha: String,
    },
    /// Which `git reset` mode to use on the current branch for this commit.
    ResetMode {
        sha: String,
    },
    /// Confirm `git reset --hard` to this commit: Reset / Cancel.
    ResetHard {
        sha: String,
    },
    /// Message for a new stash; `untracked` stashes untracked files too.
    StashMessage {
        untracked: bool,
    },
    /// Push options. Button 0 pushes, 1 and 2 toggle.
    PushOptions {
        tags: bool,
        force_lease: bool,
    },
    /// Drop the stash with this commit id.
    StashDrop {
        oid: String,
    },
    /// Reverse-apply this patch (one diff block) to the work tree.
    DiscardHunk {
        patch: String,
    },
    /// Discard the changes of these paths (Commit tab).
    GitDiscard(Vec<(crate::git::Section, String)>),
    /// Push the branch to the remote and set it as upstream.
    PushSetUpstream {
        branch: String,
        remote: String,
    },
    /// Several remotes: item `i` is `remotes[i]`; picking one pushes.
    PushUpstreamRemote {
        branch: String,
        remotes: Vec<String>,
    },
    Info,
    GotoLine,
    /// Item `i` goes to bookmark `.0[i]`.
    Bookmarks(Vec<crate::editor::BookmarkTarget>),
    /// Ctrl+F11: button `d` gives digit `d` to the bookmark on line `.1`
    /// of buffer `.0`.
    BookmarkDigit(crate::editor::buffer::BufferId, usize),
    /// Line ending of the buffer: item 0 is LF, 1 is CRLF.
    LineEnding(crate::editor::buffer::BufferId),
    /// Language of the buffer: item 0 is plain text, `i` is `LANGUAGES[i - 1]`.
    Language(crate::editor::buffer::BufferId),
    /// Indent of the buffer: item 0 is a tab, then 2, 4 and 8 spaces.
    Indent(crate::editor::buffer::BufferId),
    /// The new name for a language server rename at this place.
    LspRename(crate::lsp::DocTag),
    /// The context actions answered to this request (kept in the app).
    LspCodeAction(crate::lsp::DocTag),
    /// A workspace edit with changes that need confirmation (`EditReqs::confirm`, by seq):
    /// Apply / Cancel.
    LspConfirmEdit(u64),
    /// Git > Resolve Conflicts: item `i` is conflicted path `rels[i]`
    /// (relative to the work tree).
    ResolveConflictPick(Vec<String>),
    /// Continue with these conflicted paths: Resolve… / Stage and Continue / Cancel.
    ContinueWithConflicts {
        kind: crate::git::model::InProgressKind,
        rels: Vec<String>,
    },
    /// Abort the operation in progress: Abort / Cancel.
    AbortOperation(crate::git::model::InProgressKind),
    /// What to do with conflicted `abs` (`rel` in the work tree): Merge…,
    /// Accept Yours, Accept Theirs, Cancel; without `merge` (no line merge
    /// possible) the first button is left out.
    ResolveConflict {
        abs: std::path::PathBuf,
        rel: String,
        merge: bool,
    },
    /// Apply the merge result with changes still unresolved.
    MergeApply,
    /// Close the resolver, dropping the user's work in the result.
    MergeCancel,
    /// Quit while the resolver holds the user's work.
    MergeQuit,
    /// Save As of buffer `id`: the path typed, then `then`
    /// (`App::finish_save_as`). Esc drops the continuation.
    SaveAs {
        id: crate::editor::buffer::BufferId,
        then: crate::app::AfterSave,
    },
    /// Save As onto an existing file: Overwrite / Cancel.
    SaveAsOverwrite {
        id: crate::editor::buffer::BufferId,
        path: std::path::PathBuf,
        then: crate::app::AfterSave,
    },
    /// New Scratch File: item 0 is plain text, `i` is `LANGUAGES[i - 1]`.
    NewScratch,
    /// Delete this scratch file (from the Scratch Files palette): Delete /
    /// Cancel; both reopen the palette.
    DeleteScratch(std::path::PathBuf),
    /// Put Label: the name of a local history label on this file.
    PutLabel(std::path::PathBuf),
    /// Remove local history label `name`: its entries, each with the file
    /// it is shown as (one, or every file of a label put on several; the
    /// Local History window's Remove Label): Remove / Cancel.
    RemoveLabel {
        name: String,
        entries: Vec<(std::path::PathBuf, crate::history::Entry)>,
    },
    /// Revert every file of local history label `name` (put at `at`) to its
    /// labelled text (the Local History window's Revert on a label row):
    /// Revert / Cancel.
    RevertLabel {
        at: u64,
        name: String,
        members: Vec<(std::path::PathBuf, crate::history::Entry)>,
    },
    /// Unsaved changes recovered after a crash (`App::offer_recovery`):
    /// Restore all / Discard / Review…
    Recover,
    /// Review: restore or skip recovered entry `.0`.
    RecoverEntry(usize),
    /// A new value for this Settings window key.
    SettingValue(&'static str),
    /// Close the Settings window, dropping its changes: Discard / Keep Editing.
    SettingsDiscard,
}

#[derive(Debug, Clone)]
pub enum DialogKind {
    Confirm { buttons: Vec<String>, selected: usize },
    Input { input: TextInput, ok: String },
    Choice { items: Vec<String>, list: ListState, filter: TextInput },
}

/// Tab completion of a typed path in an input dialog
/// (`Dialog::with_path_completion`); `App::complete_dialog_path` reads the folder.
#[derive(Debug, Clone)]
pub struct PathCompletion {
    /// Relative paths start here (the project root).
    pub base: PathBuf,
    /// The last key was Tab: a Tab that adds nothing then lists the matches.
    pub tabbed: bool,
    /// Offer folders only (the worktree path prompt), not files.
    pub folders_only: bool,
    /// Shown under the input until the next other key.
    pub hint: Option<PathHint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathHint {
    /// One dim line: "No match", "N matches; Tab again lists them", a read error.
    Note(String),
    /// The matches, directories ending in `/`.
    List(Vec<String>),
}

/// Rows of a path hint, at most this many (the last one may say how many more).
const MAX_HINT_ROWS: usize = 8;

#[derive(Debug, Clone)]
pub enum DialogResult {
    Button(usize),
    Text(String),
    Choice(usize),
    Cancel,
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub id: DialogId,
    pub title: String,
    pub message: String,
    pub kind: DialogKind,
    /// Tab completes a typed path (input dialogs that opt in). Boxed: most
    /// dialogs have none, and `Effect::Dialog` carries a whole `Dialog`.
    pub paths: Option<Box<PathCompletion>>,
    button_rects: Vec<Rect>,
    body_rect: Rect,
    input_rect: Rect,
    list_rect: Rect,
}

impl Dialog {
    pub fn confirm(id: DialogId, title: &str, message: &str, buttons: &[&str]) -> Self {
        Self {
            id,
            title: title.into(),
            message: message.into(),
            kind: DialogKind::Confirm { buttons: buttons.iter().map(|s| s.to_string()).collect(), selected: 0 },
            paths: None,
            button_rects: vec![],
            body_rect: Rect::default(),
            input_rect: Rect::default(),
            list_rect: Rect::default(),
        }
    }
    pub fn info(title: &str, message: &str) -> Self {
        Self::confirm(DialogId::Info, title, message, &["OK"])
    }
    pub fn input(id: DialogId, title: &str, message: &str, initial: &str, ok: &str) -> Self {
        Self {
            id,
            title: title.into(),
            message: message.into(),
            kind: DialogKind::Input { input: TextInput::new(false).with_text(initial), ok: ok.into() },
            paths: None,
            button_rects: vec![],
            body_rect: Rect::default(),
            input_rect: Rect::default(),
            list_rect: Rect::default(),
        }
    }
    pub fn choice(id: DialogId, title: &str, message: &str, items: Vec<String>) -> Self {
        let mut list = ListState::default();
        list.set_len(items.len());
        Self {
            id,
            title: title.into(),
            message: message.into(),
            kind: DialogKind::Choice { items, list, filter: TextInput::new(false) },
            paths: None,
            button_rects: vec![],
            body_rect: Rect::default(),
            input_rect: Rect::default(),
            list_rect: Rect::default(),
        }
    }

    /// Tab completes the typed path, relative paths starting at `base`
    /// (an input dialog; `App::complete_dialog_path`).
    pub fn with_path_completion(self, base: PathBuf) -> Self {
        self.with_completion(base, false)
    }

    /// [`Self::with_path_completion`] offering folders only.
    pub fn with_folder_completion(self, base: PathBuf) -> Self {
        self.with_completion(base, true)
    }

    fn with_completion(mut self, base: PathBuf, folders_only: bool) -> Self {
        self.paths = Some(Box::new(PathCompletion { base, tabbed: false, folders_only, hint: None }));
        self
    }

    /// Drop the path hint and the Tab streak: the text changed otherwise.
    pub fn clear_path_hint(&mut self) {
        if let Some(p) = &mut self.paths {
            p.tabbed = false;
            p.hint = None;
        }
    }

    fn filtered(&self) -> Vec<usize> {
        match &self.kind {
            DialogKind::Choice { items, filter, .. } => {
                let f = filter.text.to_lowercase();
                items.iter().enumerate().filter(|(_, s)| f.is_empty() || s.to_lowercase().contains(&f)).map(|(i, _)| i).collect()
            }
            _ => vec![],
        }
    }

    /// Returns Some when the dialog is finished.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<DialogResult> {
        // Any key here is "another key" for path completion: App takes Tab
        // first in dialogs with `paths` (`App::complete_dialog_path`), so
        // routing Tab through here would clear the hint it just set.
        self.clear_path_hint();
        match &mut self.kind {
            DialogKind::Confirm { buttons, selected } => match key.code {
                KeyCode::Esc => Some(DialogResult::Cancel),
                KeyCode::Enter => Some(DialogResult::Button(*selected)),
                KeyCode::Left | KeyCode::BackTab => {
                    *selected = (*selected + buttons.len() - 1) % buttons.len();
                    None
                }
                KeyCode::Right | KeyCode::Tab => {
                    *selected = (*selected + 1) % buttons.len();
                    None
                }
                KeyCode::Char(c) => {
                    // First-letter accelerators, e.g. Y/N/C.
                    let lc = c.to_ascii_lowercase();
                    buttons.iter().position(|b| b.chars().next().map(|f| f.to_ascii_lowercase()) == Some(lc)).map(DialogResult::Button)
                }
                _ => None,
            },
            DialogKind::Input { input, .. } => match key.code {
                KeyCode::Esc => Some(DialogResult::Cancel),
                KeyCode::Enter => Some(DialogResult::Text(input.text.clone())),
                KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.delete_word_back();
                    None
                }
                KeyCode::Backspace => {
                    input.backspace();
                    None
                }
                KeyCode::Delete => {
                    input.delete();
                    None
                }
                KeyCode::Left => {
                    input.left();
                    None
                }
                KeyCode::Right => {
                    input.right();
                    None
                }
                KeyCode::Home => {
                    input.home();
                    None
                }
                KeyCode::End => {
                    input.end();
                    None
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    input.insert(c);
                    None
                }
                _ => None,
            },
            DialogKind::Choice { .. } => {
                if key.code == KeyCode::Esc {
                    return Some(DialogResult::Cancel);
                }
                if key.code == KeyCode::Enter {
                    let f = self.filtered();
                    let DialogKind::Choice { list, .. } = &self.kind else { unreachable!() };
                    return f.get(list.selected).map(|i| DialogResult::Choice(*i));
                }
                let DialogKind::Choice { list, filter, .. } = &mut self.kind else { unreachable!() };
                match key.code {
                    KeyCode::Up => list.up(1),
                    KeyCode::Down => list.down(1),
                    KeyCode::PageUp => list.up(list.viewport.max(1)),
                    KeyCode::PageDown => list.down(list.viewport.max(1)),
                    KeyCode::Home => list.home(),
                    KeyCode::End => list.end(),
                    KeyCode::Backspace => {
                        filter.backspace();
                        list.select(0);
                    }
                    KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                        filter.insert(c);
                        list.select(0);
                    }
                    _ => {}
                }
                None
            }
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<DialogResult> {
        use crossterm::event::{MouseButton, MouseEventKind};
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let p = ratatui::layout::Position::new(m.x, m.y);
                if let Some(i) = self.button_rects.iter().position(|r| r.contains(p)) {
                    return match &self.kind {
                        DialogKind::Confirm { .. } => Some(DialogResult::Button(i)),
                        DialogKind::Input { input, .. } => {
                            if i == 0 {
                                Some(DialogResult::Text(input.text.clone()))
                            } else {
                                Some(DialogResult::Cancel)
                            }
                        }
                        DialogKind::Choice { .. } => Some(DialogResult::Cancel),
                    };
                }
                if let DialogKind::Choice { list, .. } = &mut self.kind
                    && let Some(row) = list.hit(self.list_rect, m.x, m.y)
                {
                    let f = self.filtered();
                    if m.clicks >= 2 {
                        return f.get(row).map(|i| DialogResult::Choice(*i));
                    }
                    let DialogKind::Choice { list, .. } = &mut self.kind else { unreachable!() };
                    list.select(row);
                    return None;
                }
                if let DialogKind::Input { input, .. } = &mut self.kind {
                    input.click(self.input_rect, m.x, m.y);
                    // The cursor may have moved: the hint is about the old place.
                    self.clear_path_hint();
                }
                if !self.body_rect.contains(p) && !self.outer_rect().contains(p) {
                    return Some(DialogResult::Cancel);
                }
                None
            }
            MouseEventKind::ScrollUp => {
                if let DialogKind::Choice { list, .. } = &mut self.kind {
                    list.scroll(-3);
                }
                None
            }
            MouseEventKind::ScrollDown => {
                if let DialogKind::Choice { list, .. } = &mut self.kind {
                    list.scroll(3);
                }
                None
            }
            _ => None,
        }
    }

    fn outer_rect(&self) -> Rect {
        self.body_rect
    }

    /// The dialog's area as last drawn.
    pub fn rect(&self) -> Rect {
        self.outer_rect()
    }

    /// Whether a screen cell is inside the dialog as last drawn.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.outer_rect().contains(ratatui::layout::Position::new(x, y))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let msg_lines: Vec<&str> = self.message.lines().collect();
        let msg_w = msg_lines.iter().map(|l| l.width()).max().unwrap_or(0);
        let w = match &self.kind {
            DialogKind::Confirm { buttons, .. } => {
                // "[ label ]" per button, one column between them.
                let bw: usize = buttons.iter().map(|b| b.width() + 4).sum::<usize>() + buttons.len().saturating_sub(1) + 2;
                (msg_w.max(bw) + 4).max(30)
            }
            DialogKind::Input { .. } | DialogKind::Choice { .. } => (msg_w + 4).max(50),
        };
        // Wide enough for the title (" title " plus the corners).
        let w = w.max(self.title.width() + 4);
        let w = (w as u16).min(area.width.saturating_sub(2)).max(10).min(area.width);
        // Message lines wider than the text area wrap onto more rows.
        let msg_rows = message_rows(&self.message, (w as usize).saturating_sub(4));
        let mut h = match &self.kind {
            DialogKind::Confirm { .. } => msg_rows.len() + 4,
            DialogKind::Input { .. } => msg_rows.len() + 5,
            DialogKind::Choice { .. } => (msg_rows.len() + 3 + 12).min(area.height as usize),
        };
        // A path hint goes between the input and the buttons, as many rows
        // as the screen has room for.
        let hint = self.paths.as_ref().and_then(|p| p.hint.as_ref()).filter(|_| matches!(self.kind, DialogKind::Input { .. }));
        let hints = match hint {
            Some(hint) => {
                let base_h = msg_rows.len() + 5;
                let rows = MAX_HINT_ROWS.min((area.height as usize).saturating_sub(1).saturating_sub(base_h));
                hint_lines(hint, (w as usize).saturating_sub(4), rows)
            }
            None => vec![],
        };
        h += hints.len();
        let h = (h as u16).min(area.height.saturating_sub(1)).max(5).min(area.height);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 3;
        let rect = Rect::new(x, y, w, h);
        self.body_rect = rect;
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.popup_border)
            .title(format!(" {} ", self.title))
            .title_style(theme.title_focused)
            .style(Style::default().bg(theme.popup_bg).fg(theme.fg));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        // A frame too small for the controls stays empty.
        let min_inner_h = match &self.kind {
            DialogKind::Confirm { .. } => 1,
            DialogKind::Input { .. } => 3,
            DialogKind::Choice { .. } => 2,
        };
        if inner.height < min_inner_h || inner.width < 2 {
            self.button_rects.clear();
            self.input_rect = Rect::default();
            self.list_rect = Rect::default();
            return;
        }
        // On a short screen the message gives way to the controls below it.
        let reserve = match &self.kind {
            DialogKind::Confirm { .. } => 2,
            DialogKind::Input { .. } => 3 + hints.len(),
            DialogKind::Choice { .. } => 2,
        };
        let mut msg_rows = msg_rows;
        let room = (inner.height as usize).saturating_sub(reserve);
        if msg_rows.len() > room {
            cut_rows(&mut msg_rows, room, inner.width.saturating_sub(2) as usize);
        }
        let buf = frame.buffer_mut();
        let mut row = inner.y;
        for line in msg_rows.iter() {
            buf.set_stringn(inner.x + 1, row, line, inner.width.saturating_sub(2) as usize, Style::default().fg(theme.fg));
            row += 1;
        }
        self.button_rects.clear();
        match &mut self.kind {
            DialogKind::Confirm { buttons, selected } => {
                let by = inner.bottom().saturating_sub(1);
                let total: u16 = buttons.iter().map(|b| b.width() as u16 + 4).sum::<u16>() + (buttons.len() as u16).saturating_sub(1);
                let mut bx = inner.x + inner.width.saturating_sub(total) / 2;
                for (i, b) in buttons.iter().enumerate() {
                    let label = format!("[ {b} ]");
                    let bw = label.width() as u16;
                    let st = if i == *selected {
                        theme.list_selected.add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg).bg(theme.popup_bg)
                    };
                    let r = Rect::new(bx, by, bw, 1).intersection(inner);
                    buf.set_stringn(r.x, by, &label, r.width as usize, st);
                    self.button_rects.push(r);
                    bx += bw + 1;
                }
            }
            DialogKind::Input { input, ok } => {
                let iy = row + 1;
                let irect = Rect::new(inner.x + 1, iy, inner.width.saturating_sub(2), 1);
                self.input_rect = irect;
                let by = inner.bottom().saturating_sub(1);
                let labels = [format!("[ {ok} ]"), "[ Cancel ]".to_string()];
                let total: u16 = labels.iter().map(|l| l.width() as u16).sum::<u16>() + 1;
                let mut bx = inner.x + inner.width.saturating_sub(total) / 2;
                for l in &labels {
                    let bw = l.width() as u16;
                    let r = Rect::new(bx, by, bw, 1).intersection(inner);
                    buf.set_stringn(r.x, by, l, r.width as usize, Style::default().fg(theme.fg).bg(theme.popup_bg));
                    self.button_rects.push(r);
                    bx += bw + 1;
                }
                let hint_w = inner.width.saturating_sub(2);
                for (k, (line, dim)) in hints.iter().enumerate() {
                    let st = Style::default().fg(theme.fg).bg(theme.popup_bg);
                    let st = if *dim { st.patch(theme.dim) } else { st };
                    buf.set_stringn(inner.x + 1, iy + 1 + k as u16, line, hint_w as usize, st);
                }
                input.render(frame, irect, Style::default().bg(theme.bg).fg(theme.fg), "", theme.dim, true);
                self.body_rect = rect;
            }
            DialogKind::Choice { items, list, filter } => {
                let fy = row;
                let frect = Rect::new(inner.x + 1, fy, inner.width.saturating_sub(2), 1);
                let lrect = Rect::new(inner.x + 1, fy + 1, inner.width.saturating_sub(2), inner.bottom().saturating_sub(fy + 1));
                self.list_rect = lrect;
                let f = filter.text.to_lowercase();
                let filtered: Vec<usize> =
                    items.iter().enumerate().filter(|(_, s)| f.is_empty() || s.to_lowercase().contains(&f)).map(|(i, _)| i).collect();
                list.fit(filtered.len(), lrect.height as usize);
                for (vi, idx) in filtered.iter().enumerate().skip(list.offset).take(lrect.height as usize) {
                    let y = lrect.y + (vi - list.offset) as u16;
                    let st = if vi == list.selected { theme.list_selected } else { Style::default().fg(theme.fg).bg(theme.popup_bg) };
                    let buf = frame.buffer_mut();
                    buf.set_style(Rect::new(lrect.x, y, lrect.width, 1), st);
                    buf.set_stringn(lrect.x + 1, y, &items[*idx], lrect.width.saturating_sub(1) as usize, st);
                }
                filter.render(frame, frect, Style::default().bg(theme.bg).fg(theme.fg), "type to filter", theme.dim, true);
            }
        }
    }
}

/// Keep the first `max` rows, the last of them cut with `…`.
fn cut_rows(rows: &mut Vec<String>, max: usize, width: usize) {
    rows.truncate(max);
    if let Some(last) = rows.last_mut() {
        *last = crate::util::text::fit(&format!("{last} …"), width);
        if !last.ends_with('…') {
            last.push('…');
        }
    }
}

/// The message as rows at most `width` cells wide: a line that fits stays as
/// it is (an empty line stays an empty row), a wider one wraps at spaces; a
/// wrapped line keeps its leading indent on every row.
fn message_rows(message: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in message.lines() {
        let rest = line.trim_start_matches(' ');
        let indent = line.len() - rest.len();
        if line.width() <= width {
            rows.push(line.to_string());
        } else if indent > 0 && indent < width / 2 {
            let pad = " ".repeat(indent);
            rows.extend(crate::ui::toasts::wrap(rest, width - indent, usize::MAX).into_iter().map(|r| format!("{pad}{r}")));
        } else {
            rows.extend(crate::ui::toasts::wrap(line, width, usize::MAX));
        }
    }
    rows
}

/// `hint` as lines `width` cells wide, at most `rows`, each with whether it
/// is dim: a note, or the names in columns as wide as the longest plus two,
/// filled row by row; when they do not fit, the last line is "… and N more".
fn hint_lines(hint: &PathHint, width: usize, rows: usize) -> Vec<(String, bool)> {
    if rows == 0 || width == 0 {
        return vec![];
    }
    let names = match hint {
        PathHint::Note(s) => return vec![(s.clone(), true)],
        PathHint::List(names) => names,
    };
    let col = names.iter().map(|n| n.width()).max().unwrap_or(0) + 2;
    let cols = ((width + 2) / col).max(1);
    let need = names.len().div_ceil(cols);
    let shown = if need <= rows { need } else { rows - 1 };
    let mut out: Vec<(String, bool)> = names
        .chunks(cols)
        .take(shown)
        .map(|row| {
            let mut line = String::new();
            for n in row {
                line.push_str(n);
                line.extend(std::iter::repeat_n(' ', col - n.width()));
            }
            (line.trim_end().to_string(), false)
        })
        .collect();
    if shown < need {
        out.push((format!("… and {} more", names.len() - shown * cols), true));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(names: &[&str]) -> PathHint {
        PathHint::List(names.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn long_message_lines_wrap_and_short_ones_stay() {
        let sentence = "alpha beta gamma delta eps zeta";
        let msg = format!("  a.rs\n\n{sentence}");
        let rows = message_rows(&msg, 12);
        assert_eq!(rows[0], "  a.rs");
        assert_eq!(rows[1], "");
        assert!(rows.len() > 3);
        assert!(rows.iter().all(|r| r.width() <= 12));
        assert_eq!(rows[2..].join(" "), sentence);
    }

    #[test]
    fn an_indented_wide_line_keeps_its_indent() {
        let rows = message_rows("  /aaaa/bbbb/cccc/dddd/eeee", 12);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|r| r.starts_with("  ") && r.width() <= 12), "{rows:?}");
        let joined: String = rows.iter().map(|r| r.trim_start()).collect();
        assert_eq!(joined, "/aaaa/bbbb/cccc/dddd/eeee");
    }

    fn row_text(term: &ratatui::Terminal<ratatui::backend::TestBackend>, y: u16) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()
    }

    #[test]
    fn a_short_screen_cuts_the_message_not_the_controls() {
        let theme = Theme::default();
        let msg: Vec<String> = (1..=12).map(|n| format!("line {n}")).collect();
        let msg = msg.join("\n");
        let area = Rect::new(0, 0, 60, 10);

        let mut d = Dialog::input(DialogId::PutLabel(std::path::PathBuf::from("/x")), "Put Label", &msg, "v1", "OK");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
        term.draw(|f| d.render(f, area, &theme)).unwrap();
        assert!(d.input_rect.y < d.button_rects[0].y);
        assert!(d.button_rects[0].y < d.rect().bottom() - 1);
        let above = row_text(&term, d.input_rect.y - 2);
        assert!(above.contains('…'), "{above:?}");
        assert!(above.trim_end().trim_end_matches('│').trim_end().ends_with('…'), "{above:?}");

        let mut d = Dialog::confirm(DialogId::QuitDirty, "Quit", &msg, &["Yes", "No"]);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
        term.draw(|f| d.render(f, area, &theme)).unwrap();
        let by = d.button_rects[0].y;
        assert!(by < d.rect().bottom() - 1);
        let above = row_text(&term, by - 2);
        assert!(above.trim_end().trim_end_matches('│').trim_end().ends_with('…'), "{above:?}");
    }

    #[test]
    fn settings_dialogs_fit_tiny_screens() {
        let theme = Theme::default();
        let inside = |r: Rect, outer: Rect| {
            r.width == 0
                || r.height == 0
                || (r.x >= outer.x && r.y >= outer.y && r.right() <= outer.right() && r.bottom() <= outer.bottom())
        };
        let mut bad = Vec::new();
        for w in 1..=60u16 {
            for h in 1..=14u16 {
                let area = Rect::new(0, 0, w, h);
                let dialogs = [
                    Dialog::confirm(DialogId::SettingsDiscard, "Settings", "Discard 2 changed settings?", &["Discard", "Keep Editing"]),
                    Dialog::input(
                        DialogId::SettingValue("editor.scrolloff"),
                        "Lines kept above and below the caret",
                        "A whole number, 0 or more:",
                        "5",
                        "OK",
                    ),
                    Dialog::choice(DialogId::SettingValue("ui.theme"), "Theme", "", vec!["auto".into(), "darcula".into(), "Other…".into()]),
                ];
                for (n, mut d) in dialogs.into_iter().enumerate() {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
                        term.draw(|f| d.render(f, area, &theme)).unwrap();
                    }));
                    if r.is_err() {
                        bad.push(format!("{w}x{h} dialog {n}: panic"));
                        continue;
                    }
                    let rect = d.rect();
                    let rects = d.button_rects.iter().copied().chain([d.input_rect, d.list_rect]);
                    if !inside(rect, area) || !rects.into_iter().all(|r| inside(r, rect)) {
                        bad.push(format!("{w}x{h} dialog {n}: outside {rect:?}"));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "{} bad sizes, first: {:?}", bad.len(), &bad[..bad.len().min(12)]);
    }

    fn texts(lines: &[(String, bool)]) -> Vec<&str> {
        lines.iter().map(|(s, _)| s.as_str()).collect()
    }

    #[test]
    fn a_note_is_one_dim_line() {
        assert_eq!(hint_lines(&PathHint::Note("No match".into()), 40, 8), vec![("No match".to_string(), true)]);
    }

    #[test]
    fn names_fill_columns_row_by_row() {
        assert_eq!(hint_lines(&list(&["a.rs", "b.rs", "c/"]), 40, 8), vec![("a.rs  b.rs  c/".to_string(), false)]);
        let names: Vec<String> = (0..12).map(|i| format!("n{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        // Columns 5 wide: (20 + 2) / 5 = 4 a row.
        let lines = hint_lines(&list(&names), 20, 8);
        assert_eq!(texts(&lines), ["n00  n01  n02  n03", "n04  n05  n06  n07", "n08  n09  n10  n11"]);
        assert!(lines.iter().all(|(_, dim)| !dim));
    }

    #[test]
    fn overflow_ends_with_how_many_more() {
        let names: Vec<String> = (0..41).map(|i| format!("n{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let lines = hint_lines(&list(&names), 20, 8);
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[6].0, "n24  n25  n26  n27");
        assert_eq!(lines[7], ("… and 13 more".to_string(), true));
        assert_eq!(hint_lines(&list(&names), 20, 1), vec![("… and 41 more".to_string(), true)]);
        assert!(hint_lines(&list(&names), 20, 0).is_empty());
    }

    #[test]
    fn a_wide_name_gets_one_column() {
        let lines = hint_lines(&list(&["a-very-long-name.txt", "b"]), 10, 8);
        assert_eq!(texts(&lines), ["a-very-long-name.txt", "b"]);
    }
}
