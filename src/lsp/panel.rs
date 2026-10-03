//! Problems tool window (bottom, Alt+6): the language servers' diagnostics
//! grouped by file, each file's most severe first in the counts and its
//! problems in position order. Enter, a double click or the context menu
//! opens one at its position; the rows follow the store, keeping the
//! selected problem when it is still there.

use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::diagnostics::{Counts, Diagnostic, DiagnosticStore, display_pos};
use crate::actions::{Action, ProblemsAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::menu::{ContextMenu, MenuItem};
use crate::ui::tool_window_block;
use crate::ui::widgets::file_rows;
use crate::ui::widgets::list::ListState;

/// A row: a file header or one of its problems (index into its list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    File(usize),
    Item(usize, usize),
}

#[derive(Debug, Default)]
pub struct ProblemsPanel {
    files: Vec<PathBuf>,
    rows: Vec<Row>,
    pub list: ListState,
    /// Store version the rows were built from.
    built: Option<u64>,
    /// The selected file and problem, as of the last sync or selection
    /// change: found again after the store changes.
    keep: Option<(PathBuf, Option<Diagnostic>)>,
    list_rect: Rect,
}

impl ProblemsPanel {
    /// Rebuild the rows when the store changed, keeping the selection on
    /// the same problem (or file) when it is still listed.
    pub fn sync(&mut self, store: &DiagnosticStore) {
        if self.built == Some(store.version()) {
            return;
        }
        let keep = self.keep.take();
        self.built = Some(store.version());
        self.files.clear();
        self.rows.clear();
        for (fi, (path, items)) in store.files().enumerate() {
            self.files.push(path.to_path_buf());
            self.rows.push(Row::File(fi));
            self.rows.extend((0..items.len()).map(|i| Row::Item(fi, i)));
        }
        let first = self.list.len == 0;
        self.list.set_len(self.rows.len());
        let at = keep.and_then(|(path, d)| {
            let fi = self.files.iter().position(|p| *p == path)?;
            let i = d.and_then(|d| store.get(&path).iter().position(|x| *x == d));
            self.rows.iter().position(|r| *r == i.map_or(Row::File(fi), |i| Row::Item(fi, i)))
        });
        match at {
            Some(i) => self.list.select(i),
            // Start on the first problem, not its file header.
            None if first && self.rows.len() > 1 => self.list.select(1),
            None => {}
        }
        self.remember(store);
    }

    fn remember(&mut self, store: &DiagnosticStore) {
        self.keep = self.selected(store).map(|(p, d)| (p.to_path_buf(), d.cloned()));
    }

    /// The selected row's file and problem (none on a file header).
    fn selected<'s>(&self, store: &'s DiagnosticStore) -> Option<(&'s Path, Option<&'s Diagnostic>)> {
        let (fi, item) = match *self.rows.get(self.list.selected)? {
            Row::File(fi) => (fi, None),
            Row::Item(fi, i) => (fi, Some(i)),
        };
        let (path, items) = store.files().find(|(p, _)| *p == self.files[fi])?;
        Some((path, item.and_then(|i| items.get(i))))
    }

    /// Select the first problem of `path` (the status bar counts).
    pub fn select_file(&mut self, path: &Path, store: &DiagnosticStore) {
        self.sync(store);
        if let Some(fi) = self.files.iter().position(|p| p == path) {
            let i = self.rows.iter().position(|r| *r == Row::File(fi)).unwrap_or(0);
            self.list.select(i + 1);
        }
        self.remember(store);
    }

    /// Open the selected problem; a file header opens its first one.
    fn open(&self, store: &DiagnosticStore, cx: &mut Ctx) {
        let Some((path, d)) = self.selected(store) else { return };
        let Some(d) = d.or_else(|| store.get(path).first()) else { return };
        cx.effects.push(Effect::OpenFileAtUtf16 { path: path.to_path_buf(), pos: d.range.start });
    }

    fn copy(&self, store: &DiagnosticStore, cx: &mut Ctx) {
        match self.selected(store) {
            Some((_, Some(d))) => cx.effects.push(Effect::Clipboard(d.message.clone())),
            _ => cx.notify(Level::Info, "Select a problem to copy its message"),
        }
    }

    pub fn handle_action(&mut self, a: &ProblemsAction, store: &DiagnosticStore, cx: &mut Ctx) -> bool {
        self.sync(store);
        let page = self.list.viewport.max(1);
        match a {
            ProblemsAction::Up => self.list.up(1),
            ProblemsAction::Down => self.list.down(1),
            ProblemsAction::PageUp => self.list.up(page),
            ProblemsAction::PageDown => self.list.down(page),
            ProblemsAction::Home => self.list.home(),
            ProblemsAction::End => self.list.end(),
            ProblemsAction::Open => self.open(store, cx),
            ProblemsAction::Copy => self.copy(store, cx),
        }
        self.remember(store);
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, store: &DiagnosticStore, cx: &mut Ctx) -> bool {
        self.sync(store);
        match m.kind {
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let Some(i) = self.list.hit(self.list_rect, m.x, m.y) else { return false };
                self.list.select(i);
                self.remember(store);
                if button == MouseButton::Right {
                    let problem = matches!(self.rows.get(i), Some(Row::Item(..)));
                    let items = vec![
                        MenuItem::new("Jump to Source", Action::Problems(ProblemsAction::Open)),
                        MenuItem::new("Copy Message", Action::Problems(ProblemsAction::Copy)).enabled(problem),
                    ];
                    cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, m.x, m.y)));
                } else if m.clicks >= 2 {
                    self.open(store, cx);
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.list.scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.list.scroll(3);
                true
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: &Theme,
        store: &DiagnosticStore,
        root: &Path,
        hint: &str,
    ) {
        self.sync(store);
        let c = store.total_counts();
        let title = format!(" Problems: {} ", summary(c));
        let block = tool_window_block(&title, focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        self.list_rect = Rect::default();
        if r.height == 0 || r.width < 4 {
            return;
        }
        let buf = frame.buffer_mut();
        if self.rows.is_empty() {
            buf.set_stringn(r.x + 1, r.y, hint, r.width.saturating_sub(1) as usize, theme.dim);
            return;
        }
        self.list_rect = r;
        self.list.fit(self.rows.len(), r.height as usize);
        for (vi, i) in self.list.visible().enumerate() {
            let y = r.y + vi as u16;
            let row = Rect::new(r.x, y, r.width, 1);
            let selected = i == self.list.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(row, base);
            }
            match self.rows[i] {
                Row::File(fi) => {
                    let path = &self.files[fi];
                    let count = summary(store.counts(path));
                    file_rows::file_header(buf, row, root, path, &count, base, theme);
                }
                Row::Item(fi, k) => {
                    let Some(d) = store.get(&self.files[fi]).get(k) else { continue };
                    let glyph_style = base.fg(d.severity.style(theme).fg.unwrap_or(theme.fg));
                    let (origin, pos) = (d.origin(), display_pos(d.range.start));
                    let origin = if origin.is_empty() { String::new() } else { format!("  {origin}") };
                    file_rows::spans(
                        buf,
                        row,
                        &[
                            ("  ", base),
                            (d.severity.glyph(), glyph_style),
                            (" ", base),
                            (d.summary(), base),
                            (&origin, base.patch(theme.dim)),
                            ("  ", base),
                            (&pos, base.patch(theme.dim)),
                        ],
                    );
                }
            }
        }
    }
}

/// `2 errors, 1 warning`; infos and hints only when there is nothing
/// worse; `no problems` when empty.
pub fn summary(c: Counts) -> String {
    let n = |k: usize, one: &str| format!("{k} {one}{}", if k == 1 { "" } else { "s" });
    let mut parts = Vec::new();
    if c.errors > 0 {
        parts.push(n(c.errors, "error"));
    }
    if c.warnings > 0 {
        parts.push(n(c.warnings, "warning"));
    }
    if parts.is_empty() {
        if c.infos > 0 {
            parts.push(n(c.infos, "info"));
        }
        if c.hints > 0 {
            parts.push(n(c.hints, "hint"));
        }
    }
    if parts.is_empty() { "no problems".into() } else { parts.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::diagnostics::Severity;
    use crate::lsp::diagnostics::tests::diag;

    #[test]
    fn rows_follow_the_store_and_keep_the_selection() {
        let mut store = DiagnosticStore::default();
        let (a, b) = (PathBuf::from("/r/a.rs"), PathBuf::from("/r/b.rs"));
        store.publish("ra", b.clone(), vec![diag(4, 0, 4, 1, Severity::Warning, "bw")], 100);
        let mut p = ProblemsPanel::default();
        p.sync(&store);
        assert_eq!(p.rows, [Row::File(0), Row::Item(0, 0)]);
        assert_eq!(p.list.selected, 1, "starts on the first problem");
        // A file sorting before it arrives: the selection stays on "bw".
        store.publish("ra", a.clone(), vec![diag(0, 0, 0, 1, Severity::Error, "ae"), diag(2, 0, 2, 1, Severity::Error, "ae2")], 100);
        p.sync(&store);
        assert_eq!(p.rows.len(), 5);
        assert_eq!(p.selected(&store).and_then(|(_, d)| d).map(|d| d.message.as_str()), Some("bw"));
        // Gone: the selection stays where it was, clamped.
        store.publish("ra", b.clone(), vec![], 100);
        p.sync(&store);
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.list.selected, 2);
        p.select_file(&a, &store);
        assert_eq!(p.list.selected, 1);
        assert_eq!(summary(store.total_counts()), "2 errors");
        assert_eq!(summary(Counts { warnings: 1, hints: 3, ..Default::default() }), "1 warning");
        assert_eq!(summary(Counts { hints: 1, ..Default::default() }), "1 hint");
        assert_eq!(summary(Counts::default()), "no problems");
    }
}
