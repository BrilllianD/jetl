//! Commit tab: the change list (Staged / Unstaged / Untracked with stage
//! checkboxes), the diff of the selected entry, and the commit message box.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use super::{Area, Btn, GitPanel, GitTab, ListRow, put, render_diff, render_diff_header};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::model::{ChangeEntry, DiffKey, FileStatus, Section, StatusSnapshot};
use crate::git::{GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId};

/// Shown in the Commit tab's diff area for a conflicted entry.
pub(super) const CONFLICT_HINT: &str = "Conflicted: Enter opens the resolver";

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
        if self.selected_entry().is_some_and(|e| e.status == FileStatus::Conflicted) {
            return self.diff.clear();
        }
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

    #[cfg(test)]
    /// The key of the diff the Commit tab shows or has requested.
    pub fn diff_key(&self) -> Option<&DiffKey> {
        self.diff.key.as_ref()
    }

    #[cfg(test)]
    /// The Commit tab diff's line cursor: `(line, line_anchor)`.
    pub fn diff_line_cursor(&self) -> (Option<usize>, Option<usize>) {
        (self.diff.line, self.diff.line_anchor)
    }

    /// Keys while the commit message has focus. Returns false for keys it
    /// does not use so global shortcuts still work.
    pub fn handle_message_key(&mut self, key: KeyEvent, cx: &mut Ctx) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let m = &mut self.message;
        match key.code {
            // Ctrl+Shift+arrows resize the window through the keymap.
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if ctrl && shift => return false,
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
            // Alt+Up/Down recall history through the keymap.
            KeyCode::Up if !alt => m.up(),
            KeyCode::Down if !alt => m.down(),
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
        // Amending may only reword HEAD.
        if !self.amend && self.snapshot.is_some() && self.staged_count() == 0 {
            cx.notify(Level::Warn, "Nothing to commit: no staged changes (Space stages the selected file)");
            return;
        }
        cx.effects.push(Effect::Git(GitRequest::Commit { message, push, amend: self.amend }));
    }

    /// A commit or amend went through: start over with an empty box.
    pub fn committed(&mut self) {
        self.message.clear();
        self.amend = false;
        self.amend_prefill = None;
        self.history_pos = None;
    }

    /// Turn amend on (prefilling an empty box with the HEAD message, which
    /// arrives as `set_head_message`) or off (clearing that prefill when it
    /// was not edited).
    pub(super) fn toggle_amend(&mut self, cx: &mut Ctx) {
        self.amend = !self.amend;
        if self.amend {
            if self.message.text.trim().is_empty() {
                cx.effects.push(Effect::Git(GitRequest::LoadHeadMessage));
            }
        } else if let Some(prefill) = self.amend_prefill.take()
            && self.message.text == prefill
        {
            self.message.clear();
        }
    }

    /// The HEAD message for the amend prefill; an error turns amend off
    /// and comes back for a notification.
    pub fn set_head_message(&mut self, result: anyhow::Result<String>) -> Result<(), String> {
        match result {
            Ok(msg) if self.amend && self.message.text.trim().is_empty() => {
                self.message.set_text(&msg);
                self.amend_prefill = Some(msg);
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(e) => {
                self.amend = false;
                Err(format!("{e:#}"))
            }
        }
    }

    pub fn set_history(&mut self, messages: Vec<String>) {
        self.history = messages;
        self.history_pos = None;
    }

    /// Step through earlier messages (`older`) or back towards the text
    /// typed before recalling.
    pub(super) fn recall(&mut self, older: bool) {
        let next = match (self.history_pos, older) {
            (None, true) if !self.history.is_empty() => {
                self.history_draft = self.message.text.clone();
                Some(0)
            }
            (Some(i), true) => Some((i + 1).min(self.history.len().saturating_sub(1))),
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
            (None, _) => return,
        };
        self.history_pos = next;
        match next {
            Some(i) => self.message.set_text(&self.history[i]),
            None => {
                let draft = std::mem::take(&mut self.history_draft);
                self.message.set_text(&draft);
            }
        }
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

    /// `hunk_op` with a line cursor in the diff: stage, unstage or discard
    /// only the selected lines.
    pub(super) fn line_op(&mut self, discard: bool, cx: &mut Ctx) {
        const HINT: &str = "Only lines of a modified text file can be staged one by one";
        let Some(e) = self.selected_entry() else { return };
        if e.status != FileStatus::Modified || e.section == Section::Untracked || e.old_rel.is_some() {
            cx.notify(Level::Info, HINT);
            return;
        }
        let (section, rel) = (e.section, e.rel.clone());
        let Some(doc) = &self.diff.doc else { return };
        if doc.binary || doc.lossy {
            cx.notify(Level::Info, HINT);
            return;
        }
        let Some(rows) = self.diff.selected_lines() else { return };
        if rows.end > if self.diff.unified { doc.unified.len() } else { doc.rows.len() } {
            return;
        }
        let (o, n) = doc.row_lines(self.diff.unified, rows);
        let reverse = discard || section == Section::Staged;
        let Some(patch) = doc.lines_patch(o, n, &rel, &rel, 3, reverse) else {
            cx.notify(Level::Info, "No change selected in the diff");
            return;
        };
        if self.send_patch(section, discard, patch, &rel, cx) {
            self.diff.line = None;
            self.diff.line_anchor = None;
        }
    }

    /// Stage or unstage (`discard`: throw away, after a confirmation) the
    /// current block of the diff. Only plain modifications have hunks to
    /// pick; anything else falls back to the whole file.
    pub(super) fn hunk_op(&mut self, discard: bool, cx: &mut Ctx) {
        if self.diff.line.is_some() {
            return self.line_op(discard, cx);
        }
        let Some(e) = self.selected_entry() else { return };
        if e.status != FileStatus::Modified || e.section == Section::Untracked {
            return if discard { self.discard_selected(cx) } else { self.toggle_selected(cx) };
        }
        let (section, rel) = (e.section, e.rel.clone());
        let Some(doc) = &self.diff.doc else { return };
        let Some(patch) = doc.block_patch(self.diff.block, &rel, &rel, 3) else {
            cx.notify(Level::Info, "No change selected in the diff");
            return;
        };
        self.send_patch(section, discard, patch, &rel, cx);
    }

    /// Stage, unstage or (after a confirmation) discard `patch` of `rel`.
    /// False when only a warning was shown.
    fn send_patch(&mut self, section: Section, discard: bool, patch: String, rel: &str, cx: &mut Ctx) -> bool {
        let req = match (section, discard) {
            (Section::Staged, false) => GitRequest::ApplyPatch { patch, cached: true, reverse: true, op: GitOp::Unstage },
            (_, false) => GitRequest::ApplyPatch { patch, cached: true, reverse: false, op: GitOp::Stage },
            (Section::Staged, true) => {
                cx.notify(Level::Warn, "Unstage the change first (Space), then discard it");
                return false;
            }
            (_, true) => {
                let msg = format!("Discard this change in {rel}?\nThis cannot be undone.");
                cx.effects.push(Effect::Dialog(Dialog::confirm(
                    DialogId::DiscardHunk { patch },
                    "Discard Change",
                    &msg,
                    &["Discard", "Cancel"],
                )));
                return true;
            }
        };
        cx.effects.push(Effect::Git(req));
        true
    }

    /// Ask to discard the selected entry, or a whole section on its header.
    pub(super) fn discard_selected(&mut self, cx: &mut Ctx) {
        let Some(row) = self.rows.get(self.list.selected).copied() else { return };
        let Some(s) = &self.snapshot else { return };
        let entries: Vec<&ChangeEntry> = match row {
            ListRow::Entry(i) => vec![&s.entries[i]],
            ListRow::Header(sec, _) => s.entries.iter().filter(|e| e.section == sec).collect(),
        };
        if entries.iter().any(|e| e.status == FileStatus::Conflicted) {
            cx.notify(Level::Warn, "Cannot discard a conflicted file; resolve it first");
            return;
        }
        let what = match entries.as_slice() {
            [e] => e.label(),
            list => format!("{} files", list.len()),
        };
        let deletes = entries.iter().filter(|e| matches!(e.status, FileStatus::Untracked | FileStatus::Added)).count();
        let mut msg = format!("Discard all changes to {what}?");
        if deletes > 0 {
            msg.push_str(&format!("\n{deletes} new file{} will be deleted.", if deletes == 1 { "" } else { "s" }));
        }
        msg.push_str("\nThis cannot be undone.");
        let items = entries.iter().flat_map(|e| e.paths().into_iter().map(|p| (e.section, p))).collect();
        cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::GitDiscard(items), "Discard Changes", &msg, &["Discard", "Cancel"])));
    }

    /// Open the selected entry's file; a conflicted one opens the
    /// conflict resolver.
    pub(super) fn open_selected_file(&mut self, cx: &mut Ctx) {
        let Some(e) = self.selected_entry() else { return };
        if e.status == FileStatus::Conflicted {
            cx.effects.push(Effect::OpenOrResolve(e.abs.clone()));
        } else if e.abs.is_file() {
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
            if m.clicks >= 2 {
                self.diff.click_line(m.y);
            } else {
                self.diff.click(m.y, false);
            }
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
            let conflicted = self.selected_entry().is_some_and(|e| e.status == FileStatus::Conflicted);
            let header = self.diff.key.as_ref().map(|k| match k {
                DiffKey::Change { section: Section::Staged, .. } => format!("{}  (HEAD ↔ staged)", k.label()),
                DiffKey::Change { section: Section::Unstaged, .. } => format!("{}  (staged ↔ working tree)", k.label()),
                DiffKey::Change { section: Section::Untracked, .. } => format!("{}  (new file)", k.label()),
                DiffKey::Commit { .. } | DiffKey::Local { .. } | DiffKey::Range { .. } => k.label(),
            });
            let opts = self.diff_opts;
            let header = header.filter(|_| right.height > 0);
            let rest = match header {
                Some(_) => Rect::new(right.x, right.y + 1, right.width, right.height - 1),
                None => right,
            };
            if conflicted {
                if right.height > 0 {
                    frame.buffer_mut().set_stringn(right.x + 1, right.y, CONFLICT_HINT, right.width.saturating_sub(1) as usize, theme.dim);
                }
            } else {
                // First: it picks the layout the toggles show.
                render_diff(frame.buffer_mut(), rest, &mut self.diff, theme, opts);
            }
            if let Some(title) = header {
                let style = if focused && self.area == Area::Diff { theme.title_focused } else { theme.title };
                let toggles = self.diff.doc.is_some().then_some((self.diff.unified, opts));
                let row = (right.x + 1, right.y, right.right());
                let hits = render_diff_header(frame.buffer_mut(), row, &title, style, toggles, theme);
                self.hits.extend(hits.into_iter().map(|(t, r)| (Btn::Diff(t), r)));
            }
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
                "Commit message (Ctrl/Alt+Enter commits, Alt+Up/Down history, Esc leaves)"
            } else {
                "Commit message (Ctrl+K)"
            };
            // The amend checkbox sits at the right end of the label row.
            let amend = if self.amend { "[x] Amend" } else { "[ ] Amend" };
            let amend_w = amend.len() as u16;
            let label_w = left.width.saturating_sub(amend_w + 1);
            buf.set_stringn(left.x, y, crate::util::text::fit(label, label_w as usize), label_w as usize, theme.dim);
            let mut ax = left.right().saturating_sub(amend_w).max(left.x);
            let r = put(buf, &mut ax, y, left.right(), amend, if self.amend { theme.warn } else { theme.title });
            self.hits.push((Btn::Amend, r));
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
            let style = if self.staged_count() > 0 || self.amend { theme.tab_active } else { theme.tab };
            let (commit, commit_push) = if self.amend { (" Amend ", " Amend and Push ") } else { (" Commit ", " Commit and Push ") };
            let r = put(buf, &mut x, y, left.right(), commit, style);
            self.hits.push((Btn::Commit, r));
            x += 1;
            let r = put(buf, &mut x, y, left.right(), commit_push, theme.tab);
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
        let icons = self.file_icons;
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
                    if let Some((icon, st)) = crate::ui::file_icons::span(name, false, icons, base, theme) {
                        put(buf, &mut x, y, r.right(), &icon, st);
                    }
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
