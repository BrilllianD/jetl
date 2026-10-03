//! Commit tab: the change list (Staged / Unstaged / Untracked with stage
//! checkboxes), the diff of the selected entry, and the commit message box.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use super::{Area, Btn, GitPanel, GitTab, ListRow, put, render_diff};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::GitRequest;
use crate::git::model::{ChangeEntry, DiffKey, Section, StatusSnapshot};

impl GitPanel {
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
        let again = prev.and_then(|(sec, rel)| {
            self.rows
                .iter()
                .position(|r| matches!(r, ListRow::Entry(i) if self.entry(*i).is_some_and(|e| e.section == sec && e.rel == rel)))
        });
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
    pub(super) fn sync_diff(&mut self, reload: bool, cx: &mut Ctx) {
        let key = self.selected_entry().map(|e| DiffKey::Change { section: e.section, rel: e.rel.clone(), old: e.old_rel.clone() });
        match key {
            Some(k) if reload || self.diff.key.as_ref() != Some(&k) => self.diff.request(k, cx),
            Some(_) => {}
            None => self.diff.clear(),
        }
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

    pub(super) fn commit(&mut self, push: bool, cx: &mut Ctx) {
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

    /// Stage or unstage the selected entry, or a whole section on its header.
    pub(super) fn toggle_selected(&mut self, cx: &mut Ctx) {
        let Some(row) = self.rows.get(self.list.selected).copied() else { return };
        let Some(s) = &self.snapshot else { return };
        let (section, rels): (Section, Vec<String>) = match row {
            ListRow::Entry(i) => (s.entries[i].section, s.entries[i].paths()),
            ListRow::Header(sec, _) => (sec, s.entries.iter().filter(|e| e.section == sec).flat_map(|e| e.paths()).collect()),
        };
        let req = if section == Section::Staged { GitRequest::Unstage(rels) } else { GitRequest::Stage(rels) };
        cx.effects.push(Effect::Git(req));
    }

    pub(super) fn open_selected_file(&mut self, cx: &mut Ctx) {
        let Some(e) = self.selected_entry() else { return };
        if e.abs.is_file() {
            cx.open_file(e.abs.clone());
        } else {
            cx.notify(Level::Info, format!("{} is deleted", e.rel));
        }
    }

    pub(super) fn click_commit_tab(&mut self, m: &Mouse, p: Position, cx: &mut Ctx) {
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

    pub(super) fn render_commit_tab(&mut self, frame: &mut Frame, body: Rect, focused: bool, theme: &Theme) {
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
                DiffKey::Change { section: Section::Staged, .. } => format!("{}  (HEAD ↔ staged)", k.label()),
                DiffKey::Change { section: Section::Unstaged, .. } => format!("{}  (staged ↔ working tree)", k.label()),
                DiffKey::Change { section: Section::Untracked, .. } => format!("{}  (new file)", k.label()),
                DiffKey::Commit { .. } => k.label(),
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
            let label = if self.area == Area::Message && focused {
                "Commit message (Ctrl/Alt+Enter commits, Esc leaves)"
            } else {
                "Commit message (Ctrl+K)"
            };
            buf.set_stringn(left.x, y, label, left.width as usize, theme.dim);
            y += 1;
        }
        self.message_rect = Rect::new(left.x, y, left.width, msg_h);
        let msg_focused = focused && self.area == Area::Message;
        self.message.render(
            frame,
            self.message_rect,
            Style::default().bg(theme.bg).fg(theme.fg),
            "Describe the change",
            theme.dim.bg(theme.bg),
            msg_focused,
        );
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
                    if let Some(old) = &e.old_rel {
                        // A rename shows both paths in full.
                        put(buf, &mut x, y, r.right(), &format!("{old} -> {}", e.rel), name_style);
                        continue;
                    }
                    put(buf, &mut x, y, r.right(), name, name_style);
                    if !dir.is_empty() {
                        x += 1;
                        put(buf, &mut x, y, r.right(), dir, if selected { base } else { theme.dim });
                    }
                }
            }
        }
    }
}
