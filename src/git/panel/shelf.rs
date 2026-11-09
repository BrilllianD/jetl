//! Shelf tab: the shelved changes of the project (`git/shelf.rs`) and the
//! files of the selected entry. There is no diff pane here: Show Patch opens
//! the entry in a patch tab.

use ratatui::Frame;
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;

use super::{Area, Btn, GitPanel, put};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::GitRequest;
use crate::git::shelf::ShelfEntry;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::widgets::list::ListState;

impl GitPanel {
    /// Apply a shelf list unless it is older than the one held; the selected
    /// entry stays selected. An error keeps the old list.
    pub fn set_shelf(&mut self, version: u64, result: Result<Vec<ShelfEntry>, String>, _cx: &mut Ctx) {
        if version <= self.shelf_version {
            return;
        }
        self.shelf_version = version;
        let prev = self.selected_shelf().map(|e| e.id);
        match result {
            Ok(list) => {
                self.shelf_when = list.iter().map(|e| crate::history::local_time(e.time, self.shelf_offset)).collect();
                self.shelf = list;
                self.shelf_error = None;
            }
            Err(e) => self.shelf_error = Some(e),
        }
        self.shelf_list.set_len(self.shelf.len());
        let again = prev.and_then(|id| self.shelf.iter().position(|e| e.id == id));
        self.shelf_list.select(again.unwrap_or(self.shelf_list.selected));
        if again.is_none() {
            self.shelf_files = ListState::default();
        }
        self.shelf_files.set_len(self.selected_shelf().map_or(0, |e| e.files.len()));
        if self.selected_shelf().is_none() && self.area == Area::ShelfFiles {
            self.area = Area::Shelves;
        }
    }

    pub fn shelf_count(&self) -> usize {
        self.shelf.len()
    }

    #[cfg(test)]
    pub(crate) fn shelf_names(&self) -> Vec<String> {
        self.shelf.iter().map(|e| e.name.clone()).collect()
    }

    pub(super) fn selected_shelf(&self) -> Option<&ShelfEntry> {
        self.shelf.get(self.shelf_list.selected)
    }

    /// The files list follows the selected entry.
    pub(super) fn sync_shelf_files(&mut self) {
        self.shelf_files = ListState::default();
        self.shelf_files.set_len(self.selected_shelf().map_or(0, |e| e.files.len()));
    }

    /// Unshelve the selected entry, or its selected file when the files list has the focus.
    pub(super) fn unshelve_selected(&mut self, cx: &mut Ctx) {
        let Some(e) = self.selected_shelf() else {
            cx.notify(Level::Info, "No shelf entry selected");
            return;
        };
        let file = (self.area == Area::ShelfFiles).then(|| e.files.get(self.shelf_files.selected).cloned()).flatten();
        cx.effects.push(Effect::Git(GitRequest::Unshelve { id: e.id, name: e.name.clone(), file }));
    }

    /// Ask, then delete the selected entry.
    pub(super) fn delete_shelf_selected(&mut self, cx: &mut Ctx) {
        let Some(e) = self.selected_shelf() else {
            cx.notify(Level::Info, "No shelf entry selected");
            return;
        };
        let msg = format!("Delete shelf entry '{}'?\nIts patch is removed. This cannot be undone.", e.name);
        let id = DialogId::ShelfDelete { id: e.id, name: e.name.clone() };
        cx.effects.push(Effect::Dialog(Dialog::confirm(id, "Delete Shelf Entry", &msg, &["Delete", "Cancel"])));
    }

    /// Open the selected entry's patch in a patch tab.
    pub(super) fn show_shelf_patch(&mut self, cx: &mut Ctx) {
        let Some(e) = self.selected_shelf() else {
            cx.notify(Level::Info, "No shelf entry selected");
            return;
        };
        cx.effects.push(Effect::Git(GitRequest::ShelfPatch { id: e.id, title: format!("Shelf: {}", e.name) }));
    }

    pub(super) fn click_shelf_tab(&mut self, m: &Mouse, p: Position, cx: &mut Ctx) {
        if let Some(i) = self.shelf_list.hit(self.shelf_rect, m.x, m.y) {
            self.area = Area::Shelves;
            if i != self.shelf_list.selected {
                self.shelf_list.select(i);
                self.sync_shelf_files();
            }
        } else if let Some(i) = self.shelf_files.hit(self.shelf_files_rect, m.x, m.y) {
            self.area = Area::ShelfFiles;
            self.shelf_files.select(i);
            if m.clicks >= 2 {
                self.show_shelf_patch(cx);
            }
        } else if self.shelf_rect.contains(p) {
            self.area = Area::Shelves;
        }
    }

    pub(super) fn render_shelf_tab(&mut self, frame: &mut Frame, body: Rect, focused: bool, theme: &Theme) {
        let buf = frame.buffer_mut();
        let mut x = body.x;
        let toolbar = [(Btn::ShelfUnshelve, "[Unshelve]"), (Btn::ShelfShow, "[Show Patch]"), (Btn::ShelfDelete, "[Delete]")];
        for (b, label) in toolbar {
            let r = put(buf, &mut x, body.y, body.right(), label, theme.title);
            self.hits.push((b, r));
            x += 1;
        }
        let rest = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
        let left_w = if rest.width >= 70 { rest.width * 3 / 5 } else { rest.width };
        self.shelf_rect = Rect::new(rest.x, rest.y, left_w, rest.height);
        self.render_shelf_list(buf, self.shelf_rect, focused && self.area == Area::Shelves, theme);
        if rest.width > left_w + 1 {
            let sep_x = rest.x + left_w;
            for y in rest.y..rest.bottom() {
                buf[(sep_x, y)].set_symbol("│").set_style(theme.splitter);
            }
            let right = Rect::new(sep_x + 1, rest.y, rest.right() - sep_x - 1, rest.height);
            self.render_shelf_files(buf, right, focused && self.area == Area::ShelfFiles, theme);
        }
    }

    fn render_shelf_list(&mut self, buf: &mut Screen, r: Rect, active: bool, theme: &Theme) {
        self.shelf_list.set_viewport(r.height.saturating_sub(u16::from(self.shelf_error.is_some())) as usize);
        if r.height == 0 {
            return;
        }
        let mut top = r.y;
        if let Some(e) = &self.shelf_error {
            buf.set_stringn(r.x, r.y, crate::util::text::fit(e, r.width as usize), r.width as usize, theme.error);
            top += 1;
        }
        if self.shelf.is_empty() {
            let text = "No shelved changes (Alt+Shift+V in the Commit tab shelves the selected files)";
            buf.set_stringn(r.x, top, crate::util::text::fit(text, r.width as usize), r.width as usize, theme.dim);
            return;
        }
        for (vi, i) in self.shelf_list.visible().enumerate() {
            let y = top + vi as u16;
            if y >= r.bottom() {
                break;
            }
            let e = &self.shelf[i];
            let selected = i == self.shelf_list.selected;
            let base = if selected {
                if active { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let n = e.files.len();
            let meta = format!("{}  {n} file{}", self.shelf_when.get(i).map_or("", String::as_str), if n == 1 { "" } else { "s" });
            let mut x = r.x;
            let name = crate::util::text::fit(&e.name, (r.width as usize).saturating_sub(meta.chars().count() + 2).max(1));
            put(buf, &mut x, y, r.right(), &name, base);
            x += 2;
            let dim = if selected { base } else { theme.dim };
            let room = r.right().saturating_sub(x) as usize;
            put(buf, &mut x, y, r.right(), &crate::util::text::fit(&meta, room), dim);
        }
    }

    fn render_shelf_files(&mut self, buf: &mut Screen, r: Rect, active: bool, theme: &Theme) {
        if r.height < 2 || r.width < 4 {
            return;
        }
        let x0 = r.x + 1;
        let w = r.width.saturating_sub(1) as usize;
        let Some(e) = self.selected_shelf() else { return };
        let files = e.files.clone();
        let n = files.len();
        buf.set_stringn(x0, r.y, format!("{n} file{} shelved", if n == 1 { "" } else { "s" }), w, theme.dim);
        self.shelf_files_rect = Rect::new(r.x, r.y + 1, r.width, r.height.saturating_sub(1));
        self.shelf_files.set_viewport(self.shelf_files_rect.height as usize);
        for (vi, i) in self.shelf_files.visible().enumerate() {
            let Some(f) = files.get(i) else { break };
            let fy = self.shelf_files_rect.y + vi as u16;
            let selected = i == self.shelf_files.selected && active;
            let base = if selected { theme.list_selected } else { Style::default().fg(theme.fg) };
            if selected {
                buf.set_style(Rect::new(r.x, fy, r.width, 1), base);
            }
            let mut x = x0;
            put(buf, &mut x, fy, r.right(), &crate::util::text::fit(f, w), base);
        }
    }
}
