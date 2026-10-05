//! History tab: a local history snapshot (`history/`) against the file's
//! current text (its open buffer, else the disk), with Revert and a way
//! back to the file's snapshot list. The diff is loaded by the app
//! (`App::load_local_diff`), keyed by `DiffKey::Local`.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use super::{Area, Btn, GitPanel, GitTab, put, render_diff, render_diff_header};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::model::DiffKey;
use crate::history::Request;

impl GitPanel {
    /// Show snapshot `key` against the current text; `title` is its date,
    /// time and label.
    pub fn show_local_diff(&mut self, key: DiffKey, title: String, cx: &mut Ctx) {
        self.tab = GitTab::History;
        self.area = Area::HistoryDiff;
        self.history_title = title;
        self.history_diff.request(key, cx);
    }

    /// The snapshot the History tab shows.
    pub fn history_key(&self) -> Option<&DiffKey> {
        self.history_diff.key.as_ref()
    }

    /// The snapshot's date, time and label.
    pub fn history_title(&self) -> &str {
        &self.history_title
    }

    /// Compare with the current text again (F5, after a revert).
    pub fn reload_history_diff(&mut self, cx: &mut Ctx) {
        if let Some(key) = self.history_diff.key.clone() {
            self.history_diff.request(key, cx);
        }
    }

    pub(super) fn history_revert(&mut self, cx: &mut Ctx) {
        match self.history_diff.key.clone() {
            Some(DiffKey::Local { path, id, .. }) => cx.effects.push(Effect::History(Request::Revert { path, id })),
            _ => cx.notify(Level::Info, "No snapshot shown: File > Local History… picks one"),
        }
    }

    /// The file's snapshot list again.
    pub(super) fn history_list(&mut self, cx: &mut Ctx) {
        if let Some(DiffKey::Local { path, .. }) = &self.history_diff.key {
            cx.effects.push(Effect::History(Request::Show(path.clone())));
        }
    }

    pub(super) fn open_history_file(&mut self, cx: &mut Ctx) {
        if let Some(DiffKey::Local { path, .. }) = &self.history_diff.key {
            cx.open_file(path.clone());
        }
    }

    pub(super) fn click_history_tab(&mut self, m: &Mouse, p: Position) {
        if self.history_diff.key.is_some() && self.diff_rect.contains(p) {
            self.area = Area::HistoryDiff;
            self.history_diff.click(m.y);
        }
    }

    pub(super) fn render_history_tab(&mut self, frame: &mut Frame, body: Rect, theme: &Theme) {
        let buf = frame.buffer_mut();
        let Some(DiffKey::Local { rel, .. }) = self.history_diff.key.clone() else {
            let msg = "File > Local History… picks a snapshot to compare with the current text";
            buf.set_stringn(body.x, body.y, msg, body.width as usize, theme.dim);
            return;
        };
        let mut x = body.x;
        for (b, label) in [(Btn::HistoryRevert, "[Revert]"), (Btn::HistoryList, "[Snapshots…]")] {
            let r = put(buf, &mut x, body.y, body.right(), label, theme.title.bg(theme.panel_bg));
            self.hits.push((b, r));
            x += 1;
        }
        let opts = self.diff_opts;
        let title = format!("{rel}  {}  ↔  current", self.history_title);
        let rest = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
        self.diff_rect = rest;
        // First: it picks the layout the toggles show.
        render_diff(buf, rest, &mut self.history_diff, theme, opts);
        let toggles = Some((self.history_diff.unified, opts));
        let hits = render_diff_header(buf, (x, body.y, body.right()), &title, theme.title, toggles, theme);
        self.hits.extend(hits);
    }
}
