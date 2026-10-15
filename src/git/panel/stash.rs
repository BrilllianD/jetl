//! Stash tab: the stash list, the files of the selected stash, and a diff
//! of a file in it (against the commit it was made on).

use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;

use super::{Area, Btn, GitPanel, put, render_diff, render_diff_header};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::GitRequest;
use crate::git::model::{CommitDetails, DiffKey, StashEntry};
use crate::git::repo::StashOp;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::widgets::list::ListState;

/// Which stash operation a key or button asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StashAction {
    Pop,
    Apply,
    Drop,
}

impl GitPanel {
    /// Apply a stash list, keeping the selected stash selected.
    pub fn set_stashes(&mut self, result: anyhow::Result<Vec<StashEntry>>, cx: &mut Ctx) {
        let prev = self.selected_stash().map(|s| s.oid.clone());
        match result {
            Ok(list) => {
                self.stashes = list;
                self.stash_error = None;
            }
            Err(e) => {
                self.stashes.clear();
                self.stash_error = Some(format!("{e:#}"));
            }
        }
        self.stash_list.set_len(self.stashes.len());
        let again = prev.and_then(|oid| self.stashes.iter().position(|s| s.oid == oid));
        self.stash_list.select(again.unwrap_or(self.stash_list.selected));
        if self.selected_stash().is_none() {
            self.stash_details = None;
            self.stash_diff.clear();
            if matches!(self.area, Area::StashFiles | Area::StashDiff) {
                self.area = Area::Stashes;
            }
        }
        self.sync_stash_details(cx);
    }

    pub fn stash_count(&self) -> usize {
        self.stashes.len()
    }

    pub(super) fn selected_stash(&self) -> Option<&StashEntry> {
        self.stashes.get(self.stash_list.selected)
    }

    /// Load the files of the selected stash when they are not shown yet.
    pub(super) fn sync_stash_details(&mut self, cx: &mut Ctx) {
        let Some(oid) = self.selected_stash().map(|s| s.oid.clone()) else { return };
        if self.stash_details.as_ref().is_some_and(|d| d.sha == oid) {
            return;
        }
        self.stash_details = None;
        self.stash_details_error = None;
        self.stash_files = ListState::default();
        cx.effects.push(Effect::Git(GitRequest::LoadCommit(oid)));
    }

    pub(super) fn set_stash_details(&mut self, result: anyhow::Result<CommitDetails>) {
        match result {
            Ok(d) => {
                self.stash_files = ListState::default();
                self.stash_files.set_len(d.files.len());
                self.stash_details = Some(d);
            }
            Err(e) => self.stash_details_error = Some(format!("{e:#}")),
        }
    }

    /// Ask for a message, then stash tracked changes.
    pub(super) fn stash_push(&mut self, cx: &mut Ctx) {
        let msg = if self.stash_untracked {
            "Stash local changes, untracked files included.\nMessage (optional):"
        } else {
            "Stash local changes to tracked files (untracked files stay).\nMessage (optional):"
        };
        let id = DialogId::StashMessage { untracked: self.stash_untracked };
        cx.effects.push(Effect::Dialog(Dialog::input(id, "Stash", msg, "", "Stash")));
    }

    /// Pop, apply or drop (after a confirmation) the selected stash.
    pub(super) fn stash_action(&mut self, action: StashAction, cx: &mut Ctx) {
        let Some(s) = self.selected_stash() else {
            cx.notify(Level::Info, "No stash selected");
            return;
        };
        let oid = s.oid.clone();
        let req = match action {
            StashAction::Pop => StashOp::Pop { oid },
            StashAction::Apply => StashOp::Apply { oid },
            StashAction::Drop => {
                let msg = format!("Drop stash@{{{}}}: {}?\nThis cannot be undone.", s.index, s.message);
                cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::StashDrop { oid }, "Drop Stash", &msg, &["Drop", "Cancel"])));
                return;
            }
        };
        cx.effects.push(Effect::Git(GitRequest::Stash(req)));
    }

    pub(super) fn open_stash_file(&mut self, cx: &mut Ctx) {
        let Some(d) = &self.stash_details else { return };
        let Some(f) = d.files.get(self.stash_files.selected) else { return };
        let key = DiffKey::Commit { sha: d.sha.clone(), rel: f.path.clone(), old: f.old.clone() };
        self.stash_diff.request(key, cx);
        self.area = Area::StashDiff;
    }

    pub(super) fn close_stash_diff(&mut self) {
        self.stash_diff.clear();
        self.area = Area::StashFiles;
    }

    pub(super) fn click_stash_tab(&mut self, m: &Mouse, p: Position, cx: &mut Ctx) {
        if self.stash_diff.key.is_some() {
            if self.diff_rect.contains(p) {
                self.area = Area::StashDiff;
                self.stash_diff.click(m.y, false);
            }
            return;
        }
        if let Some(i) = self.stash_list.hit(self.stash_rect, m.x, m.y) {
            self.area = Area::Stashes;
            self.stash_list.select(i);
            self.sync_stash_details(cx);
        } else if let Some(i) = self.stash_files.hit(self.stash_files_rect, m.x, m.y) {
            self.area = Area::StashFiles;
            self.stash_files.select(i);
            self.open_stash_file(cx);
        } else if self.stash_rect.contains(p) {
            self.area = Area::Stashes;
        }
    }

    pub(super) fn render_stash_tab(&mut self, frame: &mut Frame, body: Rect, focused: bool, theme: &Theme) {
        let buf = frame.buffer_mut();
        if let Some(key) = self.stash_diff.key.clone() {
            let mut x = body.x;
            let r = put(buf, &mut x, body.y, body.right(), "[← Back]", theme.title.bg(theme.panel_bg));
            self.hits.push((Btn::Back, r));
            let index = self.selected_stash().map(|s| s.index).unwrap_or(0);
            let opts = self.diff_opts;
            let title = format!(" stash@{{{index}}}  {}", key.label());
            let rest = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
            self.diff_rect = rest;
            // First: it picks the layout the toggles show.
            render_diff(buf, rest, &mut self.stash_diff, theme, opts);
            let toggles = Some((self.stash_diff.unified, opts));
            let hits = render_diff_header(buf, (x, body.y, body.right()), &title, theme.title, toggles, theme);
            self.hits.extend(hits.into_iter().map(|(t, r)| (Btn::Diff(t), r)));
            return;
        }
        // Toolbar, then the list on the left and the files on the right.
        let mut x = body.x;
        let untracked = if self.stash_untracked { "[x] Untracked" } else { "[ ] Untracked" };
        let toolbar = [
            (Btn::StashPush, "[Stash…]"),
            (Btn::StashUntracked, untracked),
            (Btn::StashPop, "[Pop]"),
            (Btn::StashApply, "[Apply]"),
            (Btn::StashDrop, "[Drop]"),
        ];
        for (b, label) in toolbar {
            let r = put(buf, &mut x, body.y, body.right(), label, theme.title);
            self.hits.push((b, r));
            x += 1;
        }
        let rest = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
        let left_w = if rest.width >= 70 { rest.width * 3 / 5 } else { rest.width };
        self.stash_rect = Rect::new(rest.x, rest.y, left_w, rest.height);
        self.render_stash_list(buf, self.stash_rect, focused && self.area == Area::Stashes, theme);
        if rest.width > left_w + 1 {
            let sep_x = rest.x + left_w;
            for y in rest.y..rest.bottom() {
                buf[(sep_x, y)].set_symbol("│").set_style(theme.splitter);
            }
            let right = Rect::new(sep_x + 1, rest.y, rest.right() - sep_x - 1, rest.height);
            self.render_stash_files(buf, right, focused && self.area == Area::StashFiles, theme);
        }
    }

    fn render_stash_list(&mut self, buf: &mut Screen, r: Rect, active: bool, theme: &Theme) {
        self.stash_list.set_viewport(r.height as usize);
        if r.height == 0 {
            return;
        }
        if let Some(e) = &self.stash_error {
            buf.set_stringn(r.x, r.y, crate::util::text::fit(e, r.width as usize), r.width as usize, theme.error);
            return;
        }
        if self.stashes.is_empty() {
            buf.set_stringn(r.x, r.y, "No stashes (Ctrl+Alt+S stashes local changes)", r.width as usize, theme.dim);
            return;
        }
        for (vi, i) in self.stash_list.visible().enumerate() {
            let s = &self.stashes[i];
            let y = r.y + vi as u16;
            let selected = i == self.stash_list.selected;
            let base = if selected {
                if active { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let mut x = r.x;
            put(buf, &mut x, y, r.right(), &format!("stash@{{{}}}", s.index), if selected { base } else { theme.warn });
            x += 1;
            let msg = crate::util::text::fit(&s.message, r.right().saturating_sub(x) as usize);
            put(buf, &mut x, y, r.right(), &msg, base);
        }
    }

    fn render_stash_files(&mut self, buf: &mut Screen, r: Rect, active: bool, theme: &Theme) {
        if r.height == 0 || r.width < 4 {
            return;
        }
        let x0 = r.x + 1;
        let w = r.width.saturating_sub(1) as usize;
        if let Some(e) = &self.stash_details_error {
            buf.set_stringn(x0, r.y, crate::util::text::fit(e, w), w, theme.error);
            return;
        }
        let Some(d) = &self.stash_details else {
            if self.selected_stash().is_some() {
                buf.set_stringn(x0, r.y, "Loading…", w, theme.dim);
            }
            return;
        };
        buf.set_stringn(x0, r.y, format!("{} file{} changed", d.files.len(), if d.files.len() == 1 { "" } else { "s" }), w, theme.dim);
        self.stash_files_rect = Rect::new(r.x, r.y + 1, r.width, r.height.saturating_sub(1));
        self.stash_files.set_viewport(self.stash_files_rect.height as usize);
        for (vi, i) in self.stash_files.visible().enumerate() {
            let fy = self.stash_files_rect.y + vi as u16;
            let f = &d.files[i];
            let selected = i == self.stash_files.selected && active;
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
