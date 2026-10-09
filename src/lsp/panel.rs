//! Problems tool window (bottom, Alt+6): the language servers' diagnostics
//! grouped by file, each file's most severe first in the counts and its
//! problems in position order, each followed by its related locations
//! (`relatedInformation`, always shown). Enter, a double click or the
//! context menu opens a problem or related location at its position; the
//! rows follow the store, keeping the selected problem when it is still
//! there (found by identity when its range moved with an edit). Alt+C
//! (`problems.toggle_current_file`) lists only the editor's active file.

use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::diagnostics::{Counts, Diagnostic, DiagnosticStore, Related, display_pos};
use crate::actions::{Action, ProblemsAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::ui::menu::{ContextMenu, MenuItem};
use crate::ui::tool_window_block;
use crate::ui::widgets::file_rows;
use crate::ui::widgets::list::ListState;

/// A row: a file header, one of its problems (index into its list) or one
/// of a problem's related locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    File(usize),
    Item(usize, usize),
    Related(usize, usize, usize),
}

/// The selection to find again after the rows are rebuilt.
#[derive(Debug, Clone)]
struct Keep {
    path: PathBuf,
    diag: Option<Diagnostic>,
    /// Its place among the problems equal to it by `same_problem` and
    /// their number: while that number holds, the same place is the same
    /// problem, even after an edit moved them all (an exact match could be
    /// another one moved onto its old range).
    nth: usize,
    of: usize,
    related: Option<usize>,
}

/// The selected row in store terms.
struct Sel<'s> {
    path: &'s Path,
    diag: Option<&'s Diagnostic>,
    related: Option<&'s Related>,
}

#[derive(Debug, Default)]
pub struct ProblemsPanel {
    files: Vec<PathBuf>,
    rows: Vec<Row>,
    pub list: ListState,
    /// Store version the rows were built from, and the active file while
    /// `current_only`.
    built: Option<(u64, Option<PathBuf>)>,
    /// The selected file and problem, as of the last sync or selection
    /// change: found again after the store changes.
    keep: Option<Keep>,
    /// Only the editor's active file is listed (in memory only).
    current_only: bool,
    /// Files under it are left out of the project scope: scratch files.
    pub exclude: Option<PathBuf>,
    list_rect: Rect,
}

impl ProblemsPanel {
    /// A panel whose project scope leaves out the files under `dir`.
    pub fn excluding(dir: PathBuf) -> Self {
        Self { exclude: Some(dir), ..Default::default() }
    }

    /// Rebuild the rows when the store (or, filtered, the active file)
    /// changed, keeping the selection on the same problem (or file) when it
    /// is still listed.
    pub fn sync(&mut self, store: &DiagnosticStore, active: Option<&Path>) {
        let key = (store.version(), active.filter(|_| self.current_only).map(Path::to_path_buf));
        if self.built.as_ref() == Some(&key) {
            return;
        }
        let keep = self.keep.take();
        self.built = Some(key);
        self.files.clear();
        self.rows.clear();
        let exclude = self.exclude.as_deref();
        let listed =
            store.files().filter(|(p, _)| if self.current_only { active == Some(*p) } else { !exclude.is_some_and(|d| p.starts_with(d)) });
        for (fi, (path, items)) in listed.enumerate() {
            self.files.push(path.to_path_buf());
            self.rows.push(Row::File(fi));
            for (i, d) in items.iter().enumerate() {
                self.rows.push(Row::Item(fi, i));
                self.rows.extend((0..d.related.len()).map(|k| Row::Related(fi, i, k)));
            }
        }
        let first = self.list.len == 0;
        self.list.set_len(self.rows.len());
        let at = keep.and_then(|k| {
            let fi = self.files.iter().position(|p| *p == k.path)?;
            let items = store.get(&k.path);
            let i = k.diag.and_then(|d| {
                let same: Vec<usize> = (0..items.len()).filter(|&i| items[i].same_problem(&d)).collect();
                let exact = || items.iter().position(|x| *x == d);
                if same.len() == k.of { same.get(k.nth).copied() } else { exact().or(same.get(k.nth).or(same.last()).copied()) }
            });
            let related = i.zip(k.related).filter(|&(i, r)| r < items[i].related.len());
            let row = match (i, related) {
                (_, Some((i, r))) => Row::Related(fi, i, r),
                (Some(i), None) => Row::Item(fi, i),
                (None, None) => Row::File(fi),
            };
            self.rows.iter().position(|r| *r == row)
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
        let k = self.rows.get(self.list.selected).and_then(|r| match *r {
            Row::Related(_, _, k) => Some(k),
            _ => None,
        });
        let place = |s: &Sel| {
            let (items, d) = (store.get(s.path), s.diag?);
            let i = items.iter().position(|x| std::ptr::eq(x, d))?;
            let nth = items[..i].iter().filter(|x| x.same_problem(d)).count();
            Some((nth, items.iter().filter(|x| x.same_problem(d)).count()))
        };
        self.keep = self.selected(store).map(|s| {
            let (nth, of) = place(&s).unwrap_or_default();
            Keep { path: s.path.to_path_buf(), diag: s.diag.cloned(), nth, of, related: k }
        });
    }

    /// The selected row's file, problem (none on a file header) and
    /// related location (on a related row).
    fn selected<'s>(&self, store: &'s DiagnosticStore) -> Option<Sel<'s>> {
        let (fi, item, rel) = match *self.rows.get(self.list.selected)? {
            Row::File(fi) => (fi, None, None),
            Row::Item(fi, i) => (fi, Some(i), None),
            Row::Related(fi, i, k) => (fi, Some(i), Some(k)),
        };
        let (path, items) = store.files().find(|(p, _)| *p == self.files[fi])?;
        let diag = item.and_then(|i| items.get(i));
        let related = rel.and_then(|k| diag?.related.get(k));
        Some(Sel { path, diag, related })
    }

    /// Select the first problem of `path` (the status bar counts).
    pub fn select_file(&mut self, path: &Path, store: &DiagnosticStore, active: Option<&Path>) {
        self.sync(store, active);
        if let Some(fi) = self.files.iter().position(|p| p == path) {
            let i = self.rows.iter().position(|r| *r == Row::File(fi)).unwrap_or(0);
            self.list.select(i + 1);
        }
        self.remember(store);
    }

    /// Open the selected problem or related location; a file header opens
    /// its first problem.
    fn open(&self, store: &DiagnosticStore, cx: &mut Ctx) {
        let Some(s) = self.selected(store) else { return };
        if let Some(r) = s.related {
            cx.effects.push(Effect::OpenFileAtUtf16 { path: r.path.clone(), pos: r.range.start });
            return;
        }
        let Some(d) = s.diag.or_else(|| store.get(s.path).first()) else { return };
        cx.effects.push(Effect::OpenFileAtUtf16 { path: s.path.to_path_buf(), pos: d.range.start });
    }

    fn copy(&self, store: &DiagnosticStore, cx: &mut Ctx) {
        match self.selected(store) {
            Some(Sel { related: Some(r), .. }) => cx.effects.push(Effect::Clipboard(r.message.clone())),
            Some(Sel { diag: Some(d), .. }) => cx.effects.push(Effect::Clipboard(d.message.clone())),
            _ => cx.notify(Level::Info, "Select a problem to copy its message"),
        }
    }

    /// Show only the active file's problems, or all again.
    fn toggle_current_file(&mut self, store: &DiagnosticStore, active: Option<&Path>) {
        self.current_only = !self.current_only;
        self.built = None;
        self.sync(store, active);
        if !self.rows.is_empty() && matches!(self.rows.get(self.list.selected), Some(Row::File(_))) {
            self.list.down(1);
        }
    }

    pub fn handle_action(&mut self, a: &ProblemsAction, store: &DiagnosticStore, active: Option<&Path>, cx: &mut Ctx) -> bool {
        self.sync(store, active);
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
            ProblemsAction::ToggleCurrentFile => self.toggle_current_file(store, active),
        }
        self.remember(store);
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, store: &DiagnosticStore, active: Option<&Path>, cx: &mut Ctx) -> bool {
        self.sync(store, active);
        match m.kind {
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let Some(i) = self.list.hit(self.list_rect, m.x, m.y) else { return false };
                self.list.select(i);
                self.remember(store);
                if button == MouseButton::Right {
                    let message = matches!(self.rows.get(i), Some(Row::Item(..) | Row::Related(..)));
                    let filter = if self.current_only { "Show All Files" } else { "Show Current File Only" };
                    let items = vec![
                        MenuItem::new("Jump to Source", Action::Problems(ProblemsAction::Open)),
                        MenuItem::new("Copy Message", Action::Problems(ProblemsAction::Copy)).enabled(message),
                        MenuItem::new(filter, Action::Problems(ProblemsAction::ToggleCurrentFile)),
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
        active: Option<&Path>,
        hint: &str,
    ) {
        self.sync(store, active);
        let title = if self.current_only {
            format!(" Problems (current file): {} ", summary(active.map(|p| store.counts(p)).unwrap_or_default()))
        } else {
            let mut total = Counts::default();
            for n in self.files.iter().map(|p| store.counts(p)) {
                total.errors += n.errors;
                total.warnings += n.warnings;
                total.infos += n.infos;
                total.hints += n.hints;
            }
            format!(" Problems: {} ", summary(total))
        };
        let filtered_hint;
        let hint = match (self.current_only, active) {
            (false, _) => hint,
            (true, None) => "No file open in the editor",
            (true, Some(p)) => {
                filtered_hint = format!(
                    "No problems in {}",
                    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
                );
                &filtered_hint
            }
        };
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
                Row::Related(fi, i, k) => {
                    let path = &self.files[fi];
                    let Some(r) = store.get(path).get(i).and_then(|d| d.related.get(k)) else { continue };
                    let pos = display_pos(r.range.start);
                    let at = if r.path == *path {
                        pos
                    } else {
                        let shown = r.path.strip_prefix(root).unwrap_or(&r.path);
                        format!("{}:{pos}", shown.display())
                    };
                    file_rows::spans(
                        buf,
                        row,
                        &[
                            ("      ", base),
                            ("↳ ", base.patch(theme.dim)),
                            (r.message.lines().next().unwrap_or(""), base),
                            ("  ", base),
                            (&at, base.patch(theme.dim)),
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
        p.sync(&store, None);
        assert_eq!(p.rows, [Row::File(0), Row::Item(0, 0)]);
        assert_eq!(p.list.selected, 1, "starts on the first problem");
        // A file sorting before it arrives: the selection stays on "bw".
        store.publish("ra", a.clone(), vec![diag(0, 0, 0, 1, Severity::Error, "ae"), diag(2, 0, 2, 1, Severity::Error, "ae2")], 100);
        p.sync(&store, None);
        assert_eq!(p.rows.len(), 5);
        assert_eq!(p.selected(&store).and_then(|s| s.diag).map(|d| d.message.as_str()), Some("bw"));
        // Gone: the selection stays where it was, clamped.
        store.publish("ra", b.clone(), vec![], 100);
        p.sync(&store, None);
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.list.selected, 2);
        p.select_file(&a, &store, None);
        assert_eq!(p.list.selected, 1);
        assert_eq!(summary(store.total_counts()), "2 errors");
        assert_eq!(summary(Counts { warnings: 1, hints: 3, ..Default::default() }), "1 warning");
        assert_eq!(summary(Counts { hints: 1, ..Default::default() }), "1 hint");
        assert_eq!(summary(Counts::default()), "no problems");
    }

    #[test]
    fn scratches_are_left_out_of_the_project_scope() {
        let mut store = DiagnosticStore::default();
        let (a, x) = (PathBuf::from("/w/a.rs"), PathBuf::from("/s/x.rs"));
        store.publish("ra", a.clone(), vec![diag(0, 0, 0, 1, Severity::Error, "e")], 100);
        store.publish("ra (scratches)", x.clone(), vec![diag(0, 0, 0, 1, Severity::Error, "x")], 100);
        let mut p = ProblemsPanel { exclude: Some("/s".into()), ..Default::default() };
        p.sync(&store, Some(&x));
        assert_eq!(p.files, [a]);
        p.current_only = true;
        p.sync(&store, Some(&x));
        assert_eq!(p.files, [x]);
    }

    fn related(path: &str, line: u32, message: &str) -> Related {
        let at = lsp_types::Position::new(line, 2);
        Related { path: PathBuf::from(path), range: lsp_types::Range::new(at, at), message: message.into() }
    }

    #[tokio::test]
    async fn related_rows_open_their_location() {
        use crate::lsp::sync::DocEdit;
        let (app, dir) = crate::app::tests::test_app("problems-related");
        let mut cx =
            Ctx { effects: Vec::new(), config: &app.config, theme: &app.theme, workspace: &app.workspace, bus: app.bus_tx.clone() };
        let a = PathBuf::from("/r/a.rs");
        let mut d = diag(3, 0, 3, 1, Severity::Error, "e");
        d.related = vec![related("/r/a.rs", 1, "here"), related("/r/b.rs", 5, "there")];
        let mut store = DiagnosticStore::default();
        store.publish("ra", a.clone(), vec![d], 100);
        let mut p = ProblemsPanel::default();
        p.sync(&store, None);
        assert_eq!(p.rows, [Row::File(0), Row::Item(0, 0), Row::Related(0, 0, 0), Row::Related(0, 0, 1)]);
        p.handle_action(&ProblemsAction::End, &store, None, &mut cx);
        p.handle_action(&ProblemsAction::Open, &store, None, &mut cx);
        p.handle_action(&ProblemsAction::Copy, &store, None, &mut cx);
        match &cx.effects[..] {
            [Effect::OpenFileAtUtf16 { path, pos }, Effect::Clipboard(text)] => {
                assert_eq!((path.as_path(), *pos, text.as_str()), (Path::new("/r/b.rs"), lsp_types::Position::new(5, 2), "there"));
            }
            other => panic!("{other:?}"),
        }
        // An edit above moves every range; the selection stays on the row.
        let above = DocEdit { start: lsp_types::Position::new(0, 0), end: lsp_types::Position::new(0, 0), text: "\n".into() };
        store.shift(&a, &[above]);
        p.sync(&store, None);
        assert_eq!(store.get(&a)[0].range.start.line, 4);
        assert_eq!(p.list.selected, 3);
        assert_eq!(p.selected(&store).and_then(|s| s.related).map(|r| r.message.as_str()), Some("there"));
        drop(cx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_file_filter() {
        let mut store = DiagnosticStore::default();
        let (a, b) = (PathBuf::from("/r/a.rs"), PathBuf::from("/r/b.rs"));
        store.publish("ra", a.clone(), vec![diag(0, 0, 0, 1, Severity::Error, "ae")], 100);
        store.publish("ra", b.clone(), vec![diag(1, 0, 1, 1, Severity::Warning, "bw"), diag(2, 0, 2, 1, Severity::Warning, "bw2")], 100);
        let mut p = ProblemsPanel::default();
        p.sync(&store, Some(&b));
        assert_eq!(p.rows.len(), 5);
        p.list.select(3);
        p.remember(&store);
        p.toggle_current_file(&store, Some(&b));
        assert_eq!(p.files, std::slice::from_ref(&b));
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.selected(&store).and_then(|s| s.diag).map(|d| d.message.as_str()), Some("bw"), "kept");
        p.sync(&store, Some(&a));
        assert_eq!(p.files, std::slice::from_ref(&a), "another active file: its rows");
        p.sync(&store, None);
        assert!(p.rows.is_empty(), "no active file: nothing");
        p.sync(&store, Some(&b));
        p.list.select(2);
        p.remember(&store);
        p.toggle_current_file(&store, Some(&b));
        assert_eq!(p.files, [a, b]);
        assert_eq!(p.selected(&store).and_then(|s| s.diag).map(|d| d.message.as_str()), Some("bw2"), "kept when shown all again");
    }

    #[test]
    fn identical_problems_keep_the_selected_one_when_ranges_move() {
        use crate::lsp::sync::DocEdit;
        let mut store = DiagnosticStore::default();
        let a = PathBuf::from("/r/a.rs");
        let same = |l| diag(l, 0, l, 1, Severity::Error, "mismatched types");
        store.publish("ra", a.clone(), vec![same(1), same(2), same(3)], 100);
        let mut p = ProblemsPanel::default();
        p.sync(&store, None);
        p.list.select(2);
        p.remember(&store);
        // Two edits above all three, the panel synced after the first only
        // (as when it is hidden): still the second.
        let above = DocEdit { start: lsp_types::Position::new(0, 0), end: lsp_types::Position::new(0, 0), text: "\n".into() };
        store.shift(&a, std::slice::from_ref(&above));
        p.sync(&store, None);
        store.shift(&a, &[above]);
        p.sync(&store, None);
        assert_eq!(p.list.selected, 2);
        assert_eq!(p.selected(&store).and_then(|s| s.diag).map(|d| d.range.start.line), Some(4));
    }
}
