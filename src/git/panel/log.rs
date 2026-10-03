//! Log tab: `git log --graph` rows, details of the selected commit, and a
//! diff of a file in it.

use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{Area, Btn, GitPanel, put, render_diff, render_toggles};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::GitRequest;
use crate::git::model::{CommitDetails, DiffKey, LogRow};
use crate::ui::widgets::list::ListState;

impl GitPanel {
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
        if self.log_loading {
            return;
        }
        self.log_loading = true;
        cx.effects.push(Effect::Git(GitRequest::LoadLog { skip: 0 }));
    }

    /// Fetch the next page when the selection or view reached the end.
    pub(super) fn maybe_page_log(&mut self, cx: &mut Ctx) {
        let at_end = self.log_list.len > 0
            && (self.log_list.selected + 1 >= self.log_list.len || self.log_list.offset + self.log_list.viewport >= self.log_list.len);
        if at_end && self.log_loaded && !self.log_end && !self.log_loading {
            self.log_loading = true;
            cx.effects.push(Effect::Git(GitRequest::LoadLog { skip: self.commit_count() }));
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
                self.log_diff.click(m.y);
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

    pub(super) fn render_log_tab(&mut self, frame: &mut Frame, body: Rect, focused: bool, theme: &Theme) {
        let buf = frame.buffer_mut();
        if let Some(key) = self.log_diff.key.clone() {
            let mut x = body.x;
            let r = put(buf, &mut x, body.y, body.right(), "[← Back]", theme.title.bg(theme.panel_bg));
            self.hits.push((Btn::Back, r));
            let header = match &key {
                DiffKey::Commit { sha, .. } => format!(" {}  {}", &sha[..sha.len().min(8)], key.label()),
                DiffKey::Change { .. } => format!(" {}", key.label()),
            };
            put(buf, &mut x, body.y, body.right(), &header, theme.title);
            let rest = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
            self.diff_rect = rest;
            let opts = self.diff_opts;
            render_diff(buf, rest, &mut self.log_diff, None, focused && self.area == Area::LogDiff, theme, opts);
            let hits = render_toggles(buf, body.y, x, body.right(), self.log_diff.unified, opts, theme);
            self.hits.extend(hits);
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
        let palette = [
            Color::Rgb(104, 151, 187),
            Color::Rgb(98, 150, 85),
            Color::Rgb(204, 120, 50),
            Color::Rgb(152, 118, 170),
            Color::Rgb(187, 181, 41),
            Color::Rgb(32, 153, 157),
        ];
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
