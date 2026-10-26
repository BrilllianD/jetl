//! TODO tool window (bottom): the lines of the project's files that hold the
//! word TODO or FIXME, grouped by file. A reduced `FindPanel`: it runs the
//! same `find::Search` with a fixed pattern, once when the window is first
//! shown (`App::sync_todo`) and again on F5.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use regex::Regex;
use unicode_width::UnicodeWidthStr;

use crate::actions::TodoAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{AppMsg, Effect, Mouse};
use crate::files::find::{Batch, Hit, Search};
use crate::ui::tool_window_block;
use crate::ui::widgets::file_rows;
use crate::ui::widgets::list::ListState;

/// What the window lists: the whole words, in upper case.
pub const PATTERN: &str = r"\b(TODO|FIXME)\b";

/// A list row: a file header or one hit (index into `hits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    File { first: usize, count: usize },
    Hit(usize),
}

#[derive(Default)]
pub struct TodoPanel {
    hits: Vec<Hit>,
    rows: Vec<Row>,
    pub list: ListState,
    /// Seq of the newest scan; a running scan stops when it moves on.
    seq: u64,
    cancel: Arc<AtomicU64>,
    started: bool,
    scanning: bool,
    capped: bool,
    root: PathBuf,
    list_rect: Rect,
}

impl TodoPanel {
    /// Whether a scan was started since the last reset (the first show of a root starts one).
    pub fn started(&self) -> bool {
        self.started
    }

    pub fn scanning(&self) -> bool {
        self.scanning
    }

    /// Stop the running scan and forget everything (root switch): the next show scans again.
    pub fn reset(&mut self) {
        self.seq += 1;
        self.cancel.store(self.seq, Ordering::Relaxed);
        self.hits.clear();
        self.rows.clear();
        self.list = ListState::default();
        self.started = false;
        self.scanning = false;
        self.capped = false;
    }

    /// Begin a new scan: cancel the previous one, clear the rows, and return the
    /// `Search` to run.
    pub fn search(&mut self, root: &Path, show_hidden: bool, show_ignored: bool) -> Search {
        self.seq += 1;
        self.cancel.store(self.seq, Ordering::Relaxed);
        self.hits.clear();
        self.rows.clear();
        self.list = ListState::default();
        self.capped = false;
        self.scanning = true;
        self.started = true;
        self.root = root.to_path_buf();
        Search {
            root: self.root.clone(),
            show_hidden,
            show_ignored,
            re: Regex::new(PATTERN).expect("valid pattern"),
            seq: self.seq,
            cancel: self.cancel.clone(),
        }
    }

    /// Scan the workspace root off the main thread; batches come back as `AppMsg::Todo`.
    pub fn scan(&mut self, cx: &mut Ctx) {
        let search = self.search(&cx.workspace.root, cx.config.files.show_hidden, cx.config.files.show_ignored);
        let (generation, tx) = (cx.workspace.generation, cx.bus.clone());
        tokio::task::spawn_blocking(move || search.run(|batch| drop(tx.send(AppMsg::Todo { generation, batch }))));
    }

    /// A batch of the current scan; an older scan's batch is ignored.
    pub fn on_batch(&mut self, batch: Batch) {
        if batch.seq != self.seq {
            return;
        }
        self.scanning = !batch.done;
        self.capped |= batch.capped;
        for hit in batch.hits {
            // One row per line: its first match only.
            if self.hits.last().is_some_and(|h| h.path == hit.path && h.line == hit.line) {
                continue;
            }
            let i = self.hits.len();
            let same_file = self.hits.last().is_some_and(|h| h.path == hit.path);
            self.hits.push(hit);
            if same_file {
                if let Some(Row::File { count, .. }) = self.rows.iter_mut().rev().find(|r| matches!(r, Row::File { .. })) {
                    *count += 1;
                }
            } else {
                self.rows.push(Row::File { first: i, count: 1 });
            }
            self.rows.push(Row::Hit(i));
        }
        let first = self.list.len == 0;
        self.list.set_len(self.rows.len());
        // Start on the first hit, not its file header.
        if first && self.rows.len() > 1 {
            self.list.select(1);
        }
    }

    /// Matching lines so far.
    pub fn count(&self) -> usize {
        self.hits.len()
    }

    /// Files with matches, in row order.
    pub fn files(&self) -> Vec<PathBuf> {
        self.rows.iter().filter_map(|r| if let Row::File { first, .. } = r { Some(self.hits[*first].path.clone()) } else { None }).collect()
    }

    fn open_selected(&self, cx: &mut Ctx) {
        let hit = match self.rows.get(self.list.selected) {
            Some(Row::Hit(i)) => self.hits.get(*i),
            Some(Row::File { first, .. }) => self.hits.get(*first),
            None => None,
        };
        if let Some(h) = hit {
            cx.effects.push(Effect::OpenFileAt { path: h.path.clone(), line: h.line, col: h.col });
        }
    }

    pub fn handle_action(&mut self, a: &TodoAction, cx: &mut Ctx) -> bool {
        let page = self.list.viewport.max(1);
        match a {
            TodoAction::Up => self.list.up(1),
            TodoAction::Down => self.list.down(1),
            TodoAction::PageUp => self.list.up(page),
            TodoAction::PageDown => self.list.down(page),
            TodoAction::Home => self.list.home(),
            TodoAction::End => self.list.end(),
            TodoAction::Open => self.open_selected(cx),
            TodoAction::Refresh => self.scan(cx),
        }
        true
    }

    pub fn handle_mouse(&mut self, m: &Mouse, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.list.hit(self.list_rect, m.x, m.y) {
                    self.list.select(i);
                    self.open_selected(cx);
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

    fn title(&self) -> String {
        if !self.started || (self.scanning && self.hits.is_empty()) {
            " TODO ".into()
        } else {
            format!(" TODO ({}{}) ", self.hits.len(), if self.capped { "+" } else { "" })
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let title = self.title();
        let block = tool_window_block(&title, focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.list_rect = Rect::default();
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let buf = frame.buffer_mut();
        if self.rows.is_empty() {
            let hint = if self.scanning || !self.started { "Scanning…" } else { "No TODO or FIXME found" };
            buf.set_stringn(inner.x + 1, inner.y, hint, inner.width.saturating_sub(1) as usize, theme.dim);
            return;
        }
        let lr = inner;
        self.list_rect = lr;
        self.list.set_viewport(lr.height as usize);
        for (vi, i) in self.list.visible().enumerate() {
            let y = lr.y + vi as u16;
            let selected = i == self.list.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(lr.x, y, lr.width, 1), base);
            }
            match self.rows[i] {
                Row::File { first, count } => {
                    let row = Rect::new(lr.x, y, lr.width, 1);
                    file_rows::file_header(buf, row, &self.root, &self.hits[first].path, &count.to_string(), base, theme);
                }
                Row::Hit(h) => {
                    let hit = &self.hits[h];
                    let num = format!("{:>5}: ", hit.line + 1);
                    let mut x = lr.x + 1;
                    buf.set_stringn(x, y, &num, lr.right().saturating_sub(x) as usize, base.patch(theme.line_number));
                    x += num.width() as u16;
                    // The line without its indentation and trailing blanks.
                    let lead = hit.preview.chars().take_while(|c| c.is_whitespace()).count();
                    let col = hit.preview_col.saturating_sub(lead);
                    let text = hit.preview.trim();
                    let pre: String = text.chars().take(col).collect();
                    let mid: String = text.chars().skip(col).take(hit.len).collect();
                    let post: String = text.chars().skip(col + hit.len).collect();
                    let hl = base.bg(theme.search_match_bg);
                    for (text, style) in [(pre.as_str(), base), (mid.as_str(), hl), (post.as_str(), base)] {
                        if x >= lr.right() {
                            break;
                        }
                        (x, _) = buf.set_stringn(x, y, text, lr.right().saturating_sub(x) as usize, style);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::find::search_text;

    fn batch(seq: u64, hits: Vec<Hit>, done: bool) -> Batch {
        Batch { seq, hits, files_searched: 1, done, capped: false }
    }

    #[test]
    fn pattern_matches_whole_upper_case_words() {
        let re = Regex::new(PATTERN).unwrap();
        for yes in ["// TODO: x", "# FIXME", "TODO"] {
            assert!(re.is_match(yes), "{yes}");
        }
        for no in ["todo!()", "TODOS", "XTODO", "Fixme"] {
            assert!(!re.is_match(no), "{no}");
        }
    }

    #[test]
    fn batches_group_by_file_one_row_per_line() {
        let re = Regex::new(PATTERN).unwrap();
        let mut p = TodoPanel::default();
        p.search(Path::new("/r"), false, false);
        let mut hits = search_text(Path::new("/r/a.txt"), b"x\nTODO and FIXME\n", &re, 100);
        hits.extend(search_text(Path::new("/r/b.txt"), b"FIXME\n// TODO\n", &re, 100));
        assert_eq!(hits.len(), 4);
        p.on_batch(batch(p.seq, hits, false));
        assert_eq!(p.count(), 3);
        assert_eq!(p.files(), [PathBuf::from("/r/a.txt"), PathBuf::from("/r/b.txt")]);
        assert_eq!(p.list.selected, 1);
        assert!(p.scanning());
        p.on_batch(batch(p.seq, Vec::new(), true));
        assert!(!p.scanning());
    }

    #[test]
    fn an_older_scan_is_ignored() {
        let re = Regex::new(PATTERN).unwrap();
        let mut p = TodoPanel::default();
        p.search(Path::new("/r"), false, false);
        let old = p.seq;
        p.search(Path::new("/r"), false, false);
        p.on_batch(batch(old, search_text(Path::new("/r/a.txt"), b"TODO\n", &re, 10), true));
        assert_eq!(p.count(), 0);
        assert!(p.scanning());
    }

    #[test]
    fn reset_forgets_the_scan() {
        let mut p = TodoPanel::default();
        p.search(Path::new("/r"), false, false);
        assert!(p.started());
        p.reset();
        assert!(!p.started() && !p.scanning());
    }
}
