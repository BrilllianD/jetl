//! Worktrees tool window: the worktrees of the repository with a toolbar
//! (`[+] [Remove] [Prune]`). Enter or a double click switches the project
//! root; creation and removal go through dialogs finished by the app.

use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::actions::WorktreeAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::GitRequest;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;
use crate::util::text::fit;

use super::{Worktree, current_index};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    New,
    Remove,
    Prune,
}

/// Items of the first create-flow dialog, in order.
pub const CREATE_KINDS: [&str; 2] = ["Existing branch", "New branch"];

#[derive(Debug, Default)]
pub struct WorktreePanel {
    pub list: Vec<Worktree>,
    /// Index of the worktree holding the project root.
    pub current: Option<usize>,
    pub error: Option<String>,
    loaded: bool,
    pub state: ListState,
    /// The existing-branch dialog opens when the next branch list arrives.
    pub pending_existing: bool,
    hits: Vec<(Btn, Rect)>,
    list_rect: Rect,
}

impl WorktreePanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a fresh list, keeping the selection on the same path.
    pub fn set_list(&mut self, result: anyhow::Result<Vec<Worktree>>, root: &Path) {
        self.loaded = true;
        let keep = self.selected().map(|w| w.path.clone());
        match result {
            Ok(list) => {
                self.current = current_index(&list, root);
                self.list = list;
                self.error = None;
            }
            Err(e) => {
                self.list.clear();
                self.current = None;
                // git's first line ("fatal: not a git repository ...") says it.
                self.error = Some(format!("{e:#}").lines().next().unwrap_or("failed").to_string());
            }
        }
        self.state.set_len(self.list.len());
        let sel = keep.and_then(|p| self.list.iter().position(|w| w.path == p)).or(self.current).unwrap_or(0);
        self.state.select(sel);
    }

    /// Forget the list (root switch); a new one is requested right after.
    pub fn reset(&mut self) {
        let state = std::mem::take(&mut self.state);
        *self = Self::new();
        self.state.viewport = state.viewport;
    }

    pub fn selected(&self) -> Option<&Worktree> {
        self.list.get(self.state.selected)
    }

    /// The main worktree's path, once listed.
    pub fn main_root(&self) -> Option<&Path> {
        self.list.first().filter(|w| w.is_main).map(|w| w.path.as_path())
    }

    pub fn handle_action(&mut self, a: &WorktreeAction, cx: &mut Ctx) -> bool {
        match a {
            WorktreeAction::Up => self.state.up(1),
            WorktreeAction::Down => self.state.down(1),
            WorktreeAction::Refresh => cx.effects.push(Effect::Git(GitRequest::LoadWorktrees)),
            WorktreeAction::Switch => self.switch_selected(cx),
            WorktreeAction::New => self.start_create(cx),
            WorktreeAction::Remove => self.remove_selected(cx),
            WorktreeAction::Prune => cx.effects.push(Effect::Git(GitRequest::WorktreePrune)),
        }
        true
    }

    fn switch_selected(&mut self, cx: &mut Ctx) {
        let Some(w) = self.selected() else { return };
        if Some(self.state.selected) == self.current {
            cx.notify(Level::Info, format!("Already in {}", w.path.display()));
        } else if w.bare {
            cx.notify(Level::Warn, "A bare repository has no work tree to open");
        } else if w.prunable.is_some() || !w.path.is_dir() {
            cx.notify(Level::Warn, format!("{} no longer exists; prune it", w.path.display()));
        } else {
            cx.effects.push(Effect::SwitchRoot(w.path.clone()));
        }
    }

    fn start_create(&mut self, cx: &mut Ctx) {
        if self.main_root().is_none() {
            let why = self.error.clone().unwrap_or_else(|| "worktrees are not loaded yet".into());
            cx.notify(Level::Warn, format!("New worktree: {why}"));
            return;
        }
        let items = CREATE_KINDS.iter().map(|s| s.to_string()).collect();
        cx.effects.push(Effect::Dialog(Dialog::choice(DialogId::WorktreeCreateKind, "New Worktree", "Check out in the new worktree:", items)));
    }

    fn remove_selected(&mut self, cx: &mut Ctx) {
        let Some(w) = self.selected() else { return };
        if w.is_main {
            cx.notify(Level::Warn, "The main worktree cannot be removed");
            return;
        }
        if Some(self.state.selected) == self.current {
            cx.notify(Level::Warn, "Cannot remove the current worktree; switch to another one first");
            return;
        }
        let kept = match &w.branch {
            Some(b) => format!("\nBranch {b} is kept."),
            None => String::new(),
        };
        let msg = format!("Remove the worktree at\n{}?{kept}", w.path.display());
        let id = DialogId::WorktreeRemove { path: w.path.clone(), force: false };
        cx.effects.push(Effect::Dialog(Dialog::confirm(id, "Remove Worktree", &msg, &["Remove", "Cancel"])));
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        let p = Position::new(m.x, m.y);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(btn) = self.hits.iter().find(|(_, r)| r.contains(p)).map(|(b, _)| *b) {
                    let a = match btn {
                        Btn::New => WorktreeAction::New,
                        Btn::Remove => WorktreeAction::Remove,
                        Btn::Prune => WorktreeAction::Prune,
                    };
                    return self.handle_action(&a, cx);
                }
                let Some(i) = self.state.hit(self.list_rect, m.x, m.y) else { return false };
                self.state.select(i);
                if m.clicks >= 2 {
                    self.switch_selected(cx);
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.state.scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.state.scroll(3);
                true
            }
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let block = tool_window_block(" Worktrees ", focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.hits.clear();
        self.list_rect = Rect::default();
        if inner.height == 0 || inner.width < 4 {
            return;
        }
        let buf = frame.buffer_mut();
        // Toolbar.
        buf.set_style(Rect::new(inner.x, inner.y, inner.width, 1), Style::default().bg(theme.panel_bg));
        let mut x = inner.x;
        for (b, label) in [(Btn::New, "[+]"), (Btn::Remove, "[Remove]"), (Btn::Prune, "[Prune]")] {
            let w = (label.width() as u16).min(inner.right().saturating_sub(x));
            if w == 0 {
                break;
            }
            buf.set_stringn(x, inner.y, label, w as usize, theme.title.bg(theme.panel_bg));
            self.hits.push((b, Rect::new(x, inner.y, w, 1)));
            x += w + 1;
        }
        let r = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
        self.list_rect = r;
        self.state.set_viewport(r.height as usize);
        if r.height == 0 {
            return;
        }
        if let Some(e) = &self.error {
            buf.set_stringn(r.x + 1, r.y, fit(e, r.width.saturating_sub(1) as usize), r.width as usize, theme.dim);
            return;
        }
        if !self.loaded {
            buf.set_stringn(r.x + 1, r.y, "Loading…", r.width as usize, theme.dim);
            return;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for (vi, i) in self.state.visible().enumerate() {
            let w = &self.list[i];
            let y = r.y + vi as u16;
            let selected = i == self.state.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let mut tags = Vec::new();
            if w.is_main {
                tags.push("main");
            }
            if Some(i) == self.current {
                tags.push("current");
            }
            if w.locked.is_some() {
                tags.push("locked");
            }
            if w.prunable.is_some() {
                tags.push("prunable");
            }
            let tags = if tags.is_empty() { String::new() } else { format!("({})", tags.join(", ")) };
            let label = w.label();
            let path = display_path(&w.path, home.as_deref());
            // branch  path  (tags); the path gives way first so the tags stay
            // visible in a narrow window.
            let width = r.width.saturating_sub(1) as usize;
            let label_w = label.width().min(width);
            let tags_w = tags.width();
            let room = width.saturating_sub(label_w + 2 + if tags_w > 0 { tags_w + 2 } else { 0 });
            let mut x = r.x + 1;
            let right = r.right();
            let label_style = if Some(i) == self.current && !selected { base.patch(theme.status_accent) } else { base }.add_modifier(Modifier::BOLD);
            buf.set_stringn(x, y, &label, label_w, label_style);
            x += label_w as u16 + 2;
            if room > 0 && x < right {
                let p = fit_left(&path, room);
                buf.set_stringn(x, y, &p, (right - x) as usize, if selected { base } else { theme.dim });
                x += p.width() as u16 + 2;
            }
            if tags_w > 0 && x < right {
                buf.set_stringn(x, y, &tags, (right - x) as usize, base);
            }
        }
    }
}

/// `path` with the home directory shown as `~`.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Keep the end of `s` within `max` cells, with a leading `…` when cut.
fn fit_left(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for ch in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        tail.push(ch);
        w += cw;
    }
    let tail: String = tail.into_iter().rev().collect();
    // Prefer cutting at a path separator: `…/jt` over `…mp/jt`.
    let tail = match tail.find('/') {
        Some(i) if i > 0 => &tail[i..],
        _ => &tail[..],
    };
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_left_keeps_the_tail() {
        assert_eq!(fit_left("/tmp/jt.feature", 20), "/tmp/jt.feature");
        assert_eq!(fit_left("/tmp/jt.feature", 8), "…feature");
        assert_eq!(fit_left("/tmp/jt", 6), "…/jt");
        assert_eq!(fit_left("/tmp/x", 0), "");
        assert_eq!(display_path(Path::new("/home/u/src/a"), Some(Path::new("/home/u"))), "~/src/a");
        assert_eq!(display_path(Path::new("/tmp/a"), Some(Path::new("/home/u"))), "/tmp/a");
    }
}
