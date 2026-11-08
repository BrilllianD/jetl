//! Find tool window (bottom, Alt+3): project-wide search with a query and
//! a replacement field, case/word/regex toggles, and results grouped by
//! file. Typing searches live; Up/Down move through the results, Enter
//! opens one. A structural toggle (Alt+T, feature `structural`) turns the
//! query into an ast-grep code pattern.
//!
//! The same window shows a rename preview (`open_preview`): the hits are a
//! language server's text edits (`files::rename_preview`), exclusion works
//! as for a search, and Replace All asks to apply the included edits and the
//! answer's file operations instead of replacing text. Typing in either
//! field or toggling an option drops the preview and searches as usual.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use regex::Regex;
use unicode_width::UnicodeWidthStr;

use crate::actions::FindAction;
use crate::app::Ctx;
use crate::config::FileIcons;
use crate::config::theme::Theme;
use crate::editor::search::{build_pattern, preserve_case, preserve_case_applies};
use crate::event::{AppMsg, Effect, Level, Mouse};
use crate::files::find::{Batch, Hit, MAX_HITS, Query, Search};
use crate::files::rename_preview::{RenamePreview, RenameSource};
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::PanelId;
use crate::ui::tool_window_block;
use crate::ui::widgets::file_rows;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Query,
    Replace,
}

/// A results row: a file header or one hit (index into `hits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    File { first: usize, count: usize },
    Hit(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Query,
    Replace,
    Case,
    Word,
    Regex,
    Structural,
    PreserveCase,
    ReplaceAll,
}

/// Columns the ` AST ` button takes in the header (gap included).
const STRUCTURAL_W: u16 = if cfg!(feature = "structural") { 6 } else { 0 };

const NOT_STRUCTURAL: &str = "Case, whole word and regex do not apply to a structural search";
const NO_STRUCTURAL_REPLACE: &str = "Structural search cannot replace; Alt+T turns it off";

pub struct FindPanel {
    /// `ui.file_icons`, set by `App::sync_file_icons`.
    pub file_icons: FileIcons,
    pub query: TextInput,
    pub replace: TextInput,
    pub show_replace: bool,
    field: Field,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Replacements take the case pattern of their match (Replace in Files only; never persisted).
    pub preserve_case: bool,
    /// The query is an ast-grep code pattern (feature `structural`).
    pub structural: bool,
    hits: Vec<Hit>,
    /// The running search's pattern, for the inline replacement preview.
    re: Option<Regex>,
    rows: Vec<Row>,
    /// Rename without a server (`open_rename`): the positions that start
    /// included, per absolute path; every other hit of this search starts
    /// excluded. Dropped by `reset`.
    keep: Option<HashMap<PathBuf, HashSet<(usize, usize)>>>,
    /// A language server's rename under review (`open_preview`). Dropped by `reset`.
    preview: Option<RenamePreview>,
    pub list: ListState,
    /// Seq of the newest search; a running search stops when it moves on.
    seq: u64,
    cancel: Arc<AtomicU64>,
    searching: bool,
    capped: bool,
    files_searched: usize,
    error: Option<String>,
    root: PathBuf,
    btns: Vec<(Btn, Rect)>,
    list_rect: Rect,
}

impl Default for FindPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl FindPanel {
    pub fn new() -> Self {
        Self {
            file_icons: FileIcons::None,
            query: TextInput::new(false),
            replace: TextInput::new(false),
            show_replace: false,
            field: Field::Query,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            preserve_case: false,
            structural: false,
            hits: Vec::new(),
            re: None,
            rows: Vec::new(),
            keep: None,
            preview: None,
            list: ListState::default(),
            seq: 0,
            cancel: Arc::new(AtomicU64::new(0)),
            searching: false,
            capped: false,
            files_searched: 0,
            error: None,
            root: PathBuf::new(),
            btns: Vec::new(),
            list_rect: Rect::default(),
        }
    }

    /// Open with `query` (the editor selection, if any) and the replace
    /// field shown or not; searches when the query changed.
    pub fn open(&mut self, query: Option<&str>, replace: bool, cx: &mut Ctx) {
        self.show_replace |= replace;
        self.field = Field::Query;
        let was_structural = replace && std::mem::take(&mut self.structural);
        if let Some(q) = query.filter(|q| !q.is_empty() && *q != self.query.text) {
            self.query.set_text(q);
            self.search(cx);
        } else if was_structural || (self.hits.is_empty() && !self.query.text.is_empty() && self.preview.is_none()) {
            self.search(cx);
        }
    }

    /// Drop results and stop the running search (root switch).
    pub fn reset(&mut self) {
        self.seq += 1;
        self.cancel.store(self.seq, Ordering::Relaxed);
        self.hits.clear();
        self.re = None;
        self.rows.clear();
        self.keep = None;
        self.preview = None;
        self.list = ListState::default();
        self.searching = false;
        self.error = None;
    }

    /// Textual rename without a server: query `old` (case and whole word on,
    /// regex off), the replace row with `new`, a fresh search. Hits outside
    /// `keep` (absolute path, then line and char column) start excluded.
    pub fn open_rename(&mut self, old: &str, new: &str, keep: HashMap<PathBuf, HashSet<(usize, usize)>>, cx: &mut Ctx) {
        self.query.set_text(old);
        self.replace.set_text(new);
        self.show_replace = true;
        self.field = Field::Query;
        self.case_sensitive = true;
        self.whole_word = true;
        self.regex = false;
        self.preserve_case = false;
        self.structural = false;
        self.search(cx);
        // After `search`: it calls `reset`, which drops the set. Batches arrive later.
        self.keep = Some(keep);
    }

    /// Show a language server's rename for review: query `old`, replacement `new` (case and whole word on),
    /// one result per edit in `hits`, nothing searched. Replace All applies the included edits.
    pub fn open_preview(&mut self, preview: RenamePreview, hits: Vec<Hit>, cx: &mut Ctx) {
        self.reset();
        self.root = cx.workspace.root.clone();
        self.query.set_text(&preview.old);
        self.replace.set_text(&preview.new);
        self.show_replace = true;
        self.field = Field::Query;
        self.case_sensitive = true;
        self.whole_word = true;
        self.regex = false;
        self.preserve_case = false;
        self.structural = false;
        self.capped = false;
        self.searching = false;
        let files = preview.stamps.len();
        self.preview = Some(preview);
        self.on_batch(Batch { seq: self.seq, hits, files_searched: files, done: true, capped: false });
    }

    /// The rename under review, if any.
    pub fn rename_preview(&self) -> Option<&RenamePreview> {
        self.preview.as_ref()
    }

    /// The preview and its included hits for Replace All; the rows stay.
    pub fn take_preview(&mut self) -> Option<(RenamePreview, Vec<Hit>)> {
        let preview = self.preview.take()?;
        let hits = self.hits.iter().filter(|h| !h.excluded).cloned().collect();
        Some((preview, hits))
    }

    /// Put a preview taken by `take_preview` back after a refused apply.
    pub fn restore_preview(&mut self, preview: RenamePreview) {
        self.preview = Some(preview);
    }

    /// A rename was applied: drop the results and clear both fields.
    pub fn finish_preview(&mut self) {
        self.reset();
        self.query.set_text("");
        self.replace.set_text("");
    }

    pub fn pattern(&self) -> Result<Regex, String> {
        build_pattern(&self.query.text, self.regex, self.case_sensitive, self.whole_word)
    }

    /// What to search for: the pattern source in structural mode, else the
    /// regex built from the query and options.
    pub fn search_query(&self) -> Result<Query, String> {
        if self.structural { Ok(Query::Structural(self.query.text.clone())) } else { self.pattern().map(Query::Regex) }
    }

    /// Whether the replace row is available.
    fn replace_shown(&self) -> bool {
        self.show_replace && !self.structural
    }

    /// Start a new search with the current query and options, cancelling
    /// the previous one.
    pub fn search(&mut self, cx: &mut Ctx) {
        self.reset();
        self.root = cx.workspace.root.clone();
        if self.query.text.is_empty() {
            return;
        }
        let query = match self.search_query() {
            Ok(q) => q,
            Err(e) => {
                self.error = Some(e.lines().last().unwrap_or("invalid pattern").trim().to_string());
                return;
            }
        };
        self.re = match &query {
            Query::Regex(re) => Some(re.clone()),
            Query::Structural(_) => None,
        };
        self.searching = true;
        self.capped = false;
        self.files_searched = 0;
        let search = Search {
            root: self.root.clone(),
            show_hidden: cx.config.files.show_hidden,
            show_ignored: cx.config.files.show_ignored,
            query,
            seq: self.seq,
            cancel: self.cancel.clone(),
        };
        let (generation, tx) = (cx.workspace.generation, cx.bus.clone());
        tokio::task::spawn_blocking(move || search.run(|batch| drop(tx.send(AppMsg::Find { generation, batch }))));
    }

    /// Hits of the running search; older searches' batches are ignored.
    pub fn on_batch(&mut self, batch: Batch) {
        if batch.seq != self.seq {
            return;
        }
        self.files_searched = batch.files_searched;
        self.searching = !batch.done;
        self.capped |= batch.capped;
        for mut hit in batch.hits {
            if let Some(keep) = &self.keep {
                hit.excluded = !keep.get(&hit.path).is_some_and(|s| s.contains(&(hit.line, hit.col)));
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

    pub fn current_seq(&self) -> u64 {
        self.seq
    }

    #[cfg(test)]
    pub fn cancel_handle(&self) -> Arc<AtomicU64> {
        self.cancel.clone()
    }

    /// Every result, included or not.
    #[cfg(test)]
    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    pub fn hit_count(&self) -> usize {
        self.hits.len()
    }

    /// Distinct files with hits, in result order.
    pub fn hit_files(&self) -> Vec<PathBuf> {
        self.rows.iter().filter_map(|r| if let Row::File { first, .. } = r { Some(self.hits[*first].path.clone()) } else { None }).collect()
    }

    /// Flip the selected result between included and excluded; on a file
    /// header, all of that file's results: when any is included, exclude them
    /// all; when all are excluded, include them, but in a rename preview the
    /// textual guesses stay excluded unless the file has no other row.
    pub fn toggle_exclude(&mut self) {
        match self.rows.get(self.list.selected).copied() {
            Some(Row::Hit(i)) => self.hits[i].excluded = !self.hits[i].excluded,
            Some(Row::File { first, count }) => {
                let rows = &mut self.hits[first..first + count];
                if !rows.iter().all(|h| h.excluded) {
                    rows.iter_mut().for_each(|h| h.excluded = true);
                } else {
                    let guess = |h: &Hit| h.rename.as_ref().is_some_and(|r| r.source == RenameSource::Textual);
                    let only_guesses = rows.iter().all(guess);
                    rows.iter_mut().filter(|h| only_guesses || !guess(h)).for_each(|h| h.excluded = false);
                }
            }
            None => {}
        }
    }

    pub fn included_count(&self) -> usize {
        self.hits.iter().filter(|h| !h.excluded).count()
    }

    pub fn excluded_count(&self) -> usize {
        self.hits.iter().filter(|h| h.excluded).count()
    }

    /// The hits Replace All changes, grouped by file in result order; files
    /// with none left out.
    pub fn included_by_file(&self) -> Vec<(PathBuf, Vec<Hit>)> {
        self.rows
            .iter()
            .filter_map(|r| {
                let Row::File { first, count } = r else { return None };
                let hits: Vec<Hit> = self.hits[*first..*first + *count].iter().filter(|h| !h.excluded).cloned().collect();
                (!hits.is_empty()).then(|| (self.hits[*first].path.clone(), hits))
            })
            .collect()
    }

    /// What Replace All would put in place of `hit`, for display.
    fn replacement_of(&self, hit: &Hit) -> String {
        let with = &self.replace.text;
        let mut s = String::new();
        if let Some(r) = &hit.rename {
            s.push_str(&r.new_text);
        } else {
            match self.re.as_ref().filter(|_| self.regex).and_then(|re| re.captures(&hit.matched)) {
                Some(caps) => caps.expand(with, &mut s),
                None => s.push_str(with),
            }
            if preserve_case_applies(self.preserve_case, self.regex, with) {
                s = preserve_case(&hit.matched, &s);
            }
        }
        s.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect()
    }

    fn selected_hit(&self) -> Option<&Hit> {
        match self.rows.get(self.list.selected)? {
            Row::Hit(i) => self.hits.get(*i),
            Row::File { first, .. } => self.hits.get(*first),
        }
    }

    fn open_selected(&self, cx: &mut Ctx) {
        if let Some(h) = self.selected_hit() {
            cx.effects.push(Effect::OpenFileAt { path: h.path.clone(), line: h.line, col: h.col });
        }
    }

    /// Select the next or previous hit (wrapping) and open it; used from
    /// anywhere through `find.next_result` / `find.prev_result`.
    pub fn step(&mut self, forward: bool, cx: &mut Ctx) {
        let n = self.rows.len();
        if self.hits.is_empty() {
            cx.notify(Level::Info, "No find results; Ctrl+Shift+F or Alt+3 searches the project");
            return;
        }
        let mut i = self.list.selected;
        for _ in 0..n {
            i = if forward { (i + 1) % n } else { (i + n - 1) % n };
            if matches!(self.rows[i], Row::Hit(_)) {
                break;
            }
        }
        self.list.select(i);
        self.open_selected(cx);
    }

    fn input(&mut self) -> &mut TextInput {
        match self.field {
            Field::Query => &mut self.query,
            Field::Replace => &mut self.replace,
        }
    }

    /// Editing keys for the focused field; false for keys the keymap
    /// handles (Up/Down, Enter, toggles, Escape, global shortcuts).
    pub fn handle_key(&mut self, key: KeyEvent, cx: &mut Ctx) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let (before, before_replace) = (self.query.text.clone(), self.replace.text.clone());
        let input = self.input();
        match key.code {
            // Ctrl+Shift+Left/Right resize the window through the keymap.
            KeyCode::Left | KeyCode::Right if ctrl && shift => return false,
            KeyCode::Backspace if ctrl || alt => input.delete_word_back(),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete if !alt => input.delete(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            KeyCode::Char(c) if !ctrl && !alt => input.insert(c),
            _ => return false,
        }
        if self.query.text != before || (self.preview.is_some() && self.replace.text != before_replace) {
            self.search(cx);
        }
        true
    }

    pub fn paste(&mut self, text: &str, cx: &mut Ctx) {
        let (before, before_replace) = (self.query.text.clone(), self.replace.text.clone());
        self.input().insert_str(&text.replace(['\r', '\n'], " "));
        if self.query.text != before || (self.preview.is_some() && self.replace.text != before_replace) {
            self.search(cx);
        }
    }

    pub fn handle_action(&mut self, a: &FindAction, cx: &mut Ctx) -> bool {
        let page = self.list.viewport.max(1);
        match a {
            FindAction::Up => self.list.up(1),
            FindAction::Down => self.list.down(1),
            FindAction::PageUp => self.list.up(page),
            FindAction::PageDown => self.list.down(page),
            FindAction::Open => self.open_selected(cx),
            FindAction::NextField => {
                self.field = if self.field == Field::Query && self.replace_shown() { Field::Replace } else { Field::Query };
            }
            FindAction::ToggleCase => self.toggle(|p| &mut p.case_sensitive, cx),
            FindAction::ToggleWord => self.toggle(|p| &mut p.whole_word, cx),
            FindAction::ToggleRegex => self.toggle(|p| &mut p.regex, cx),
            FindAction::TogglePreserveCase => self.toggle_preserve_case(cx),
            FindAction::ToggleStructural => self.toggle_structural(cx),
            FindAction::ToggleReplace if self.structural => cx.notify(Level::Info, NO_STRUCTURAL_REPLACE),
            FindAction::ToggleReplace => {
                self.show_replace = !self.show_replace;
                self.field = if self.show_replace { Field::Replace } else { Field::Query };
            }
            FindAction::ReplaceAll => self.ask_replace_all(cx),
            FindAction::ToggleExclude => self.toggle_exclude(),
            FindAction::Escape => cx.focus(PanelId::Editor),
            // Handled by the app (they need the editor and the layout).
            FindAction::InFiles | FindAction::ReplaceInFiles | FindAction::NextResult | FindAction::PrevResult => return false,
        }
        true
    }

    fn toggle_structural(&mut self, cx: &mut Ctx) {
        if !cfg!(feature = "structural") {
            cx.notify(Level::Info, "Structural search is not in this build (cargo feature \"structural\")");
            return;
        }
        self.structural = !self.structural;
        self.field = Field::Query;
        self.search(cx);
    }

    fn toggle(&mut self, flag: impl FnOnce(&mut Self) -> &mut bool, cx: &mut Ctx) {
        if self.structural {
            cx.notify(Level::Info, NOT_STRUCTURAL);
            return;
        }
        let f = flag(self);
        *f = !*f;
        self.search(cx);
    }

    /// Flip preserve case. The matches do not depend on it, so no new search.
    fn toggle_preserve_case(&mut self, cx: &mut Ctx) {
        if self.structural {
            cx.notify(Level::Info, NO_STRUCTURAL_REPLACE);
            return;
        }
        self.preserve_case ^= true;
        if self.preserve_case && !self.preserve_applies() {
            cx.notify(Level::Info, "Preserve case is off while a regex replacement uses groups ($1, ${name})");
        }
    }

    fn preserve_applies(&self) -> bool {
        preserve_case_applies(self.preserve_case, self.regex, &self.replace.text)
    }

    fn ask_replace_all(&mut self, cx: &mut Ctx) {
        if let Some(preview) = &self.preview {
            let included: Vec<Hit> = self.hits.iter().filter(|h| !h.excluded).cloned().collect();
            if included.is_empty() && preview.op_count() == 0 && preview.unlisted.is_empty() {
                cx.notify(Level::Info, "Every result is excluded; nothing to replace");
                return;
            }
            let msg = preview.confirm_message(&included, self.excluded_count(), &self.root);
            cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::ReplaceInFiles, "Rename", &msg, &["Rename", "Cancel"])));
            return;
        }
        if self.structural {
            cx.notify(Level::Info, NO_STRUCTURAL_REPLACE);
            return;
        }
        if self.searching {
            cx.notify(Level::Info, "Still searching; replace when the search finishes");
            return;
        }
        if self.hits.is_empty() {
            cx.notify(Level::Info, "Nothing to replace");
            return;
        }
        if !self.show_replace {
            self.show_replace = true;
            self.field = Field::Replace;
            return;
        }
        let n = self.included_count();
        if n == 0 {
            cx.notify(Level::Info, "Every result is excluded; nothing to replace");
            return;
        }
        let files = self.included_by_file().len();
        let msg = format!(
            "Replace {n} match{} in {files} file{} with \"{}\"{}?\nOpen files change in the editor (undoable); others are written to disk.",
            if n == 1 { "" } else { "es" },
            if files == 1 { "" } else { "s" },
            self.replace.text,
            if self.preserve_applies() { " (preserving case)" } else { "" }
        );
        cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::ReplaceInFiles, "Replace in Files", &msg, &["Replace", "Cancel"])));
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let p = ratatui::layout::Position::new(m.x, m.y);
                if let Some((b, r)) = self.btns.iter().find(|(_, r)| r.contains(p)).copied() {
                    match b {
                        Btn::Query => {
                            self.field = Field::Query;
                            self.query.click(r, m.x, m.y);
                        }
                        Btn::Replace => {
                            self.field = Field::Replace;
                            self.replace.click(r, m.x, m.y);
                        }
                        Btn::Case => self.toggle(|p| &mut p.case_sensitive, cx),
                        Btn::Word => self.toggle(|p| &mut p.whole_word, cx),
                        Btn::Regex => self.toggle(|p| &mut p.regex, cx),
                        Btn::Structural => self.toggle_structural(cx),
                        Btn::PreserveCase => self.toggle_preserve_case(cx),
                        Btn::ReplaceAll => self.ask_replace_all(cx),
                    }
                    return true;
                }
                if let Some(i) = self.list.hit(self.list_rect, m.x, m.y) {
                    self.list.select(i);
                    if m.clicks >= 2 {
                        self.open_selected(cx);
                    }
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

    fn status(&self) -> (String, Option<Level>) {
        if let Some(e) = &self.error {
            return (e.clone(), Some(Level::Error));
        }
        if self.query.text.is_empty() {
            return ("type to search the project".into(), None);
        }
        let (n, files) = (self.hits.len(), self.hit_files().len());
        let found = format!(
            "{n}{} match{} in {files} file{}",
            if self.capped { "+" } else { "" },
            if n == 1 { "" } else { "es" },
            if files == 1 { "" } else { "s" }
        );
        let found = match self.excluded_count() {
            0 => found,
            k => format!("{found}; {k} excluded"),
        };
        if let Some(p) = &self.preview {
            let ops = match p.op_count() {
                0 => String::new(),
                1 => " + 1 file operation".into(),
                n => format!(" + {n} file operations"),
            };
            return (format!("{found}{ops}"), None);
        }
        if self.searching {
            (format!("Searching… {found} ({} files)", self.files_searched), None)
        } else if n == 0 {
            let msg = if self.structural {
                format!("No matches ({} files searched; the pattern must parse as code in each file's language)", self.files_searched)
            } else {
                format!("No matches ({} files searched)", self.files_searched)
            };
            (msg, Some(Level::Warn))
        } else if self.capped {
            (format!("{found}; stopped at {MAX_HITS}"), Some(Level::Warn))
        } else {
            (found, None)
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let block = tool_window_block(" Find ", focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.btns.clear();
        self.list_rect = Rect::default();
        if inner.height == 0 || inner.width < 20 {
            return;
        }
        let bar = Style::default().bg(theme.panel_bg).fg(theme.fg);
        let input_style = Style::default().bg(theme.bg).fg(theme.fg);
        let right = inner.right();
        let put = |frame: &mut Frame, x: &mut u16, y: u16, text: &str, style: Style| -> Rect {
            let w = (text.width() as u16).min(right.saturating_sub(*x));
            if w > 0 {
                frame.buffer_mut().set_stringn(*x, y, text, w as usize, style);
            }
            let r = Rect::new(*x, y, w, 1);
            *x += w;
            r
        };
        let inputs: u16 = if self.replace_shown() { 2 } else { 1 };
        // The status has a row of its own, given up before the last results row.
        let status_row = inner.height >= inputs + 2;
        let header_h = (inputs + u16::from(status_row)).min(inner.height);
        frame.buffer_mut().set_style(Rect::new(inner.x, inner.y, inner.width, header_h), bar);
        let input_w = inner.width.saturating_sub(10 + 16 + STRUCTURAL_W).clamp(10, 60);

        let (y, mut x) = (inner.y, inner.x + 1);
        put(frame, &mut x, y, "Find:    ", bar);
        let field = Rect::new(x, y, input_w.min(right.saturating_sub(x)), 1);
        self.query.render(
            frame,
            field,
            input_style,
            if self.structural { "code pattern, $X matches a node" } else { "text or regex" },
            theme.dim.bg(theme.bg),
            focused && self.field == Field::Query,
        );
        self.btns.push((Btn::Query, field));
        x += field.width;
        let (on, off) = (theme.strip_item_active, theme.dim.bg(theme.panel_bg));
        for (b, label, enabled) in
            [(Btn::Case, " Cc ", self.case_sensitive), (Btn::Word, " W ", self.whole_word), (Btn::Regex, " .* ", self.regex)]
        {
            put(frame, &mut x, y, " ", bar);
            let r = put(frame, &mut x, y, label, if enabled && !self.structural { on } else { off });
            self.btns.push((b, r));
        }
        if cfg!(feature = "structural") {
            put(frame, &mut x, y, " ", bar);
            let r = put(frame, &mut x, y, " AST ", if self.structural { on } else { off });
            self.btns.push((Btn::Structural, r));
        }

        if self.replace_shown() && header_h > 1 {
            let (y, mut x) = (inner.y + 1, inner.x + 1);
            put(frame, &mut x, y, "Replace: ", bar);
            // Leave room for ` AB ` and ` Replace all ` with their gaps.
            let replace_w = input_w.min(right.saturating_sub(x + 1 + 4 + 1 + 13).max(10));
            let field = Rect::new(x, y, replace_w.min(right.saturating_sub(x)), 1);
            self.replace.render(frame, field, input_style, "replace with", theme.dim.bg(theme.bg), focused && self.field == Field::Replace);
            self.btns.push((Btn::Replace, field));
            x += field.width;
            put(frame, &mut x, y, " ", bar);
            let shaped = self.preserve_applies() && self.preview.is_none();
            let r = put(frame, &mut x, y, " AB ", if shaped { on } else { off });
            self.btns.push((Btn::PreserveCase, r));
            put(frame, &mut x, y, " ", bar);
            let r = put(frame, &mut x, y, " Replace all ", theme.tab_active);
            self.btns.push((Btn::ReplaceAll, r));
        }
        if status_row {
            let (y, mut x) = (inner.y + inputs, inner.x + 1);
            let (status, level) = self.status();
            let st = match level {
                Some(Level::Error) => theme.error.bg(theme.panel_bg),
                Some(Level::Warn) => theme.warn.bg(theme.panel_bg),
                _ => theme.dim.bg(theme.panel_bg),
            };
            put(frame, &mut x, y, &status, st);
        }
        self.btns.retain(|(_, r)| r.width > 0);

        let lr = Rect::new(inner.x, inner.y + header_h, inner.width, inner.height - header_h);
        self.list_rect = lr;
        self.list.set_viewport(lr.height as usize);
        let dimmed = |s: Style| s.fg(theme.dim.fg.unwrap_or(theme.fg));
        let buf = frame.buffer_mut();
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
                    let all_off = self.hits[first..first + count].iter().all(|h| h.excluded);
                    let head = if all_off { dimmed(base) } else { base };
                    file_rows::file_header(buf, row, &self.root, &self.hits[first].path, &count.to_string(), head, theme, self.file_icons);
                }
                Row::Hit(h) => {
                    let hit = &self.hits[h];
                    let base = if hit.excluded { dimmed(base) } else { base };
                    let num = format!("{:>6}  ", hit.line + 1);
                    let num_style = if hit.excluded { dimmed(base.patch(theme.line_number)) } else { base.patch(theme.line_number) };
                    let mut x = lr.x + 1;
                    buf.set_stringn(x, y, &num, lr.right().saturating_sub(x) as usize, num_style);
                    x += num.width() as u16;
                    // Text before, in and after the match.
                    let pre: String = hit.preview.chars().take(hit.preview_col).collect();
                    let mid: String = hit.preview.chars().skip(hit.preview_col).take(hit.len).collect();
                    let post: String = hit.preview.chars().skip(hit.preview_col + hit.len).collect();
                    let preview = self.replace_shown() && !self.replace.text.is_empty() && !hit.excluded;
                    let new = if preview { self.replacement_of(hit) } else { String::new() };
                    let (mid_style, new_style) = if preview {
                        (base.bg(theme.diff_del_word_bg), base.bg(theme.diff_add_word_bg))
                    } else if hit.excluded {
                        (base, base)
                    } else {
                        (base.bg(theme.search_match_bg), base)
                    };
                    for (text, style) in
                        [(pre.trim_start(), base), (mid.as_str(), mid_style), (new.as_str(), new_style), (post.as_str(), base)]
                    {
                        if x >= lr.right() {
                            break;
                        }
                        (x, _) = buf.set_stringn(x, y, text, lr.right().saturating_sub(x) as usize, style);
                    }
                    if hit.rename.as_ref().is_some_and(|r| r.source == RenameSource::Textual) && lr.width > 5 {
                        buf.set_stringn(lr.right() - 5, y, " text", 5, dimmed(base));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::workspace::Workspace;

    fn hit(path: &str, line: usize, matched: &str) -> Hit {
        Hit {
            path: path.into(),
            line,
            col: 0,
            len: matched.chars().count(),
            preview: matched.into(),
            preview_col: 0,
            matched: matched.into(),
            excluded: false,
            rename: None,
        }
    }

    fn panel() -> FindPanel {
        let mut p = FindPanel::new();
        p.on_batch(Batch {
            seq: 0,
            hits: vec![hit("a", 0, "x1"), hit("a", 1, "x2"), hit("b", 0, "x3")],
            files_searched: 2,
            done: true,
            capped: false,
        });
        p
    }

    #[test]
    fn toggling_a_hit_and_a_file_row() {
        let mut p = panel();
        // Rows: 0 a, 1 a:0, 2 a:1, 3 b, 4 b:0.
        p.list.select(2);
        p.toggle_exclude();
        assert_eq!((p.included_count(), p.excluded_count()), (2, 1));
        p.query.set_text("x");
        assert_eq!(p.status().0, "3 matches in 2 files; 1 excluded");
        let by_file = p.included_by_file();
        assert_eq!(by_file.iter().map(|(f, h)| (f.to_str().unwrap(), h.len())).collect::<Vec<_>>(), [("a", 1), ("b", 1)]);
        p.list.select(0);
        p.toggle_exclude();
        assert_eq!(
            p.included_by_file().iter().map(|(f, _)| f.to_str().unwrap()).collect::<Vec<_>>(),
            ["b"],
            "file row excludes the rest of a"
        );
        p.toggle_exclude();
        assert_eq!(p.included_count(), 3, "all excluded in the file: include them all");
        p.list.select(3);
        p.toggle_exclude();
        p.list.select(0);
        p.toggle_exclude();
        assert_eq!(p.included_count(), 0, "a file row excludes");
        p.list.select(0);
        p.toggle_exclude();
        assert_eq!(p.included_count(), 2);
    }

    #[tokio::test]
    async fn bare_delete_edits_the_query_and_excludes_nothing() {
        let mut p = panel();
        p.query.set_text("xy");
        p.list.select(1);
        let dir = std::env::temp_dir().join(format!("jetl-test-find-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(dir.clone()));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // At the end of the text Delete has nothing to remove and keeps the results.
        assert!(p.handle_key(key(KeyCode::Delete), &mut cx));
        assert_eq!(p.query.text, "xy");
        assert_eq!((p.included_count(), p.excluded_count()), (3, 0));
        // Before "y" it deletes that character.
        assert!(p.handle_key(key(KeyCode::Left), &mut cx));
        assert!(p.handle_key(key(KeyCode::Delete), &mut cx));
        assert_eq!(p.query.text, "x");
        assert_eq!(p.excluded_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_excludes_hits_outside_the_set_until_reset() {
        let mut p = FindPanel::new();
        p.keep = Some(HashMap::from([(PathBuf::from("a"), HashSet::from([(0, 0)]))]));
        let at = |path, line| Hit { line, ..hit(path, 0, "x") };
        let batch = |p: &FindPanel| Batch {
            seq: p.current_seq(),
            hits: vec![at("a", 0), at("a", 1), at("b", 0)],
            files_searched: 2,
            done: true,
            capped: false,
        };
        let b = batch(&p);
        p.on_batch(b);
        assert_eq!((p.included_count(), p.excluded_count()), (1, 2));
        p.reset();
        assert!(p.keep.is_none());
        let b = batch(&p);
        p.on_batch(b);
        assert_eq!(p.excluded_count(), 0);
    }

    #[test]
    fn replacement_preview_literal_and_regex() {
        let mut p = panel();
        p.replace.set_text("$1!");
        let h = hit("a", 0, "ab");
        assert_eq!(p.replacement_of(&h), "$1!", "literal");
        p.regex = true;
        p.re = Some(Regex::new("(a)b").unwrap());
        assert_eq!(p.replacement_of(&h), "a!");
    }

    #[test]
    fn replacement_preview_preserves_case() {
        let mut p = panel();
        p.replace.set_text("bar");
        let h = hit("a", 0, "Foo");
        assert_eq!(p.replacement_of(&h), "bar");
        p.preserve_case = true;
        assert_eq!(p.replacement_of(&h), "Bar");
        p.replace.set_text("$1");
        p.regex = true;
        p.re = Some(Regex::new("(?i)(foo)").unwrap());
        assert_eq!(p.replacement_of(&h), "Foo", "groups: the expansion is not shaped");
        p.re = Some(Regex::new("(?i)(foo)").unwrap());
        p.replace.set_text("bar$$");
        assert_eq!(p.replacement_of(&h), "Bar$", "no reference: shaped");
    }

    #[tokio::test]
    async fn preserve_case_toggle_does_not_search() {
        let (dir, config, theme, workspace) = temp_root("preserve-toggle");
        let mut cx = test_ctx(&config, &theme, &workspace);
        let mut p = FindPanel::new();
        p.query.set_text("foo");
        p.search(&mut cx);
        let seq = p.current_seq();
        cx.effects.clear();
        assert!(p.handle_action(&FindAction::TogglePreserveCase, &mut cx));
        assert!(p.preserve_case);
        assert_eq!(p.current_seq(), seq, "no new search");
        assert!(cx.effects.is_empty(), "no toast without groups");
        p.regex = true;
        p.replace.set_text("$1");
        p.handle_action(&FindAction::TogglePreserveCase, &mut cx);
        p.handle_action(&FindAction::TogglePreserveCase, &mut cx);
        let warn = "Preserve case is off while a regex replacement uses groups ($1, ${name})";
        assert!(matches!(cx.effects.as_slice(), [Effect::Notify(Level::Info, t)] if t == warn), "toast only when switching on");
        p.preserve_case = false;
        p.structural = true;
        cx.effects.clear();
        p.handle_action(&FindAction::TogglePreserveCase, &mut cx);
        assert!(!p.preserve_case);
        assert!(matches!(cx.effects.as_slice(), [Effect::Notify(Level::Info, t)] if t == NO_STRUCTURAL_REPLACE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ask_replace_all_names_preserved_case() {
        let (dir, config, theme, workspace) = temp_root("ask-pc");
        let mut cx = test_ctx(&config, &theme, &workspace);
        let mut p = panel();
        p.show_replace = true;
        p.replace.set_text("bar");
        let msg = |cx: &mut Ctx| match cx.effects.pop() {
            Some(Effect::Dialog(d)) => d.message.clone(),
            _ => panic!("no dialog"),
        };
        p.ask_replace_all(&mut cx);
        assert!(msg(&mut cx).starts_with("Replace 3 matches in 2 files with \"bar\"?"));
        p.preserve_case = true;
        p.ask_replace_all(&mut cx);
        assert!(msg(&mut cx).starts_with("Replace 3 matches in 2 files with \"bar\" (preserving case)?"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preserve_case_button_is_drawn_on_the_replace_row() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = Theme::default();
        let (on, off) = (theme.strip_item_active, theme.dim.bg(theme.panel_bg));
        let mut p = FindPanel::new();
        p.show_replace = true;
        let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
        let mut check = |p: &mut FindPanel, want: Style, why: &str| {
            term.draw(|f| p.render(f, f.area(), true, &theme)).unwrap();
            let buf = term.backend().buffer();
            let r = p.btns.iter().find(|(b, _)| *b == Btn::PreserveCase).map(|(_, r)| *r).expect("button");
            let label: String = (0..r.width).map(|dx| buf[(r.x + dx, r.y)].symbol()).collect();
            assert_eq!(label, " AB ", "{why}");
            let cell = &buf[(r.x + 1, r.y)];
            assert_eq!((cell.fg, cell.bg), (want.fg.unwrap_or_default(), want.bg.unwrap_or_default()), "{why}");
        };
        check(&mut p, off, "flag off");
        p.preserve_case = true;
        check(&mut p, on, "flag on");
        p.regex = true;
        p.replace.set_text("$1");
        check(&mut p, off, "regex with a group reference");
    }

    fn test_ctx<'a>(config: &'a Config, theme: &'a Theme, workspace: &'a Workspace) -> Ctx<'a> {
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        Ctx { effects: Vec::new(), config, theme, workspace, bus }
    }

    #[tokio::test]
    async fn open_rename_turns_structural_off() {
        let mut p = FindPanel::new();
        p.structural = true;
        p.preserve_case = true;
        let dir = std::env::temp_dir().join(format!("jetl-test-find-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(dir.clone()));
        let mut cx = test_ctx(&config, &theme, &workspace);
        p.open_rename("old", "new", HashMap::new(), &mut cx);
        assert!(!p.structural && !p.preserve_case && p.show_replace && p.replace_shown());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_option_buttons_are_drawn_off_while_structural() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = Theme::default();
        let (on, off) = (theme.strip_item_active, theme.dim.bg(theme.panel_bg));
        assert_ne!(on, off);
        let mut p = FindPanel::new();
        (p.case_sensitive, p.whole_word, p.regex) = (true, true, true);
        let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
        for (structural, want) in [(true, off), (false, on)] {
            p.structural = structural;
            term.draw(|f| p.render(f, f.area(), true, &theme)).unwrap();
            let buf = term.backend().buffer();
            for label in [" Cc ", " W ", " .* "] {
                let r = p.btns.iter().map(|(_, r)| *r).find(|r| {
                    r.width as usize == label.len() && { (0..r.width).map(|dx| buf[(r.x + dx, r.y)].symbol()).collect::<String>() == label }
                });
                let r = r.unwrap_or_else(|| panic!("{label:?} not drawn"));
                let cell = &buf[(r.x + 1, r.y)];
                assert_eq!(
                    (cell.fg, cell.bg),
                    (want.fg.unwrap_or_default(), want.bg.unwrap_or_default()),
                    "{label:?} structural={structural}"
                );
            }
        }
    }

    fn rename_hit(path: &str, line: usize, col: usize, new_text: &str, at: Option<(u32, u32)>, source: RenameSource) -> Hit {
        use crate::files::rename_preview::RenameEdit;
        use lsp_types::{Position, Range, TextEdit};
        let edit = TextEdit {
            range: Range::new(Position::new(line as u32, col as u32), Position::new(line as u32, col as u32 + 3)),
            new_text: new_text.into(),
        };
        Hit {
            col,
            len: 3,
            preview: "xx old yy".into(),
            preview_col: 3,
            matched: "old".into(),
            excluded: source == RenameSource::Textual,
            rename: Some(Box::new(RenameEdit { new_text: new_text.into(), edit, at, source })),
            ..hit(path, line, "old")
        }
    }

    fn preview_of(ops: usize) -> RenamePreview {
        use crate::lsp::edits::{FileOp, Step};
        let steps = (0..ops)
            .map(|i| Step::Op {
                index: i as u32,
                op: FileOp::Delete { path: format!("/p/f{i}").into(), recursive: false, ignore_if_not_exists: true },
            })
            .collect();
        RenamePreview {
            old: "old".into(),
            new: "new".into(),
            steps,
            stamps: HashMap::new(),
            unlisted: HashSet::new(),
            textual: Default::default(),
            scip: Default::default(),
        }
    }

    fn rows(ops: usize) -> (RenamePreview, Vec<Hit>) {
        let hits = vec![
            rename_hit("a", 0, 3, "new", Some((0, 0)), RenameSource::Exact),
            rename_hit("a", 2, 3, "new: ", Some((0, 1)), RenameSource::Exact),
            rename_hit("b", 1, 3, "new", Some((1, 0)), RenameSource::Exact),
        ];
        (preview_of(ops), hits)
    }

    fn temp_root(name: &str) -> (std::path::PathBuf, Config, Theme, Workspace) {
        let dir = std::env::temp_dir().join(format!("jetl-test-find-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), Config::default(), Theme::default(), Workspace::new(dir))
    }

    #[tokio::test]
    async fn open_preview_sets_fields_and_groups_rows() {
        let (dir, config, theme, workspace) = temp_root("preview-open");
        let mut cx = test_ctx(&config, &theme, &workspace);
        let mut p = FindPanel::new();
        p.structural = true;
        p.regex = true;
        p.preserve_case = true;
        let (preview, hits) = rows(0);
        p.open_preview(preview, hits, &mut cx);
        assert_eq!((p.query.text.as_str(), p.replace.text.as_str()), ("old", "new"));
        assert!(p.show_replace && p.case_sensitive && p.whole_word && !p.regex && !p.preserve_case && !p.structural && !p.searching);
        assert_eq!(p.rows, [Row::File { first: 0, count: 2 }, Row::Hit(0), Row::Hit(1), Row::File { first: 2, count: 1 }, Row::Hit(2)]);
        assert_eq!(p.list.selected, 1, "starts on the first hit");
        assert!(p.rename_preview().is_some());
        assert_eq!(p.status().0, "3 matches in 2 files");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn typing_in_either_field_or_toggling_drops_the_preview() {
        let (dir, config, theme, workspace) = temp_root("preview-type");
        let mut cx = test_ctx(&config, &theme, &workspace);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let open = |p: &mut FindPanel, cx: &mut Ctx| {
            let (preview, hits) = rows(0);
            p.open_preview(preview, hits, cx);
        };
        let mut p = FindPanel::new();
        open(&mut p, &mut cx);
        assert!(p.handle_key(key('x'), &mut cx));
        assert!(p.rename_preview().is_none() && p.query.text == "oldx", "query key");
        open(&mut p, &mut cx);
        p.handle_action(&FindAction::NextField, &mut cx);
        assert!(p.handle_key(key('x'), &mut cx));
        assert!(p.rename_preview().is_none() && p.replace.text == "newx", "replace key");
        open(&mut p, &mut cx);
        p.handle_action(&FindAction::NextField, &mut cx);
        p.paste("zz", &mut cx);
        assert!(p.rename_preview().is_none(), "paste in replace");
        open(&mut p, &mut cx);
        p.handle_action(&FindAction::ToggleCase, &mut cx);
        assert!(p.rename_preview().is_none() && !p.case_sensitive, "toggle");
        // An ordinary search does not search again when only the replacement changes.
        let seq = p.current_seq();
        p.handle_action(&FindAction::NextField, &mut cx);
        assert!(p.handle_key(key('y'), &mut cx));
        assert_eq!(p.current_seq(), seq);
        assert_eq!(p.replace.text, "newy", "typed into the replace field");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacement_of_uses_the_rows_own_text() {
        let mut p = FindPanel::new();
        p.replace.set_text("new");
        let h = rename_hit("a", 0, 3, "new:\t", Some((0, 0)), RenameSource::Exact);
        assert_eq!(p.replacement_of(&h), "new: ");
        assert_eq!(p.replacement_of(&hit("a", 0, "old")), "new");
    }

    #[tokio::test]
    async fn take_and_restore_preview() {
        let (dir, config, theme, workspace) = temp_root("preview-take");
        let mut cx = test_ctx(&config, &theme, &workspace);
        let mut p = FindPanel::new();
        let (preview, hits) = rows(0);
        p.open_preview(preview, hits, &mut cx);
        p.list.select(2);
        p.toggle_exclude();
        let (preview, included) = p.take_preview().unwrap();
        assert_eq!(included.iter().map(|h| (h.path.to_str().unwrap(), h.line)).collect::<Vec<_>>(), [("a", 0), ("b", 1)]);
        assert!(p.rename_preview().is_none());
        assert_eq!((p.hit_count(), p.rows.len()), (3, 5), "rows stay");
        p.restore_preview(preview);
        assert!(p.rename_preview().is_some());
        p.finish_preview();
        assert!(p.rename_preview().is_none());
        assert_eq!((p.hit_count(), p.query.text.as_str(), p.replace.text.as_str()), (0, "", ""));
        assert_eq!(p.status().0, "type to search the project");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_counts_excluded_and_file_operations() {
        let (dir, config, theme, workspace) = temp_root("preview-status");
        let mut cx = test_ctx(&config, &theme, &workspace);
        let mut p = FindPanel::new();
        let (preview, hits) = rows(2);
        p.open_preview(preview, hits, &mut cx);
        assert_eq!(p.status(), ("3 matches in 2 files + 2 file operations".into(), None));
        p.list.select(2);
        p.toggle_exclude();
        assert_eq!(p.status().0, "3 matches in 2 files; 1 excluded + 2 file operations");
        p.open_preview(preview_of(1), Vec::new(), &mut cx);
        assert_eq!(p.status().0, "0 matches in 0 files + 1 file operation");
        assert!(p.rename_preview().is_some());
        // Showing the window with only operations keeps the preview.
        p.open(None, false, &mut cx);
        assert!(p.rename_preview().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn textual_row_draws_a_dim_marker() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = Theme::default();
        let mut p = FindPanel::new();
        p.query.set_text("old");
        p.preview = Some(preview_of(0));
        p.on_batch(Batch {
            seq: p.current_seq(),
            hits: vec![
                rename_hit("a", 0, 3, "new", None, RenameSource::Textual),
                rename_hit("a", 1, 3, "new", Some((0, 0)), RenameSource::Exact),
            ],
            files_searched: 1,
            done: true,
            capped: false,
        });
        let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
        term.draw(|f| p.render(f, f.area(), true, &theme)).unwrap();
        let buf = term.backend().buffer();
        let line = |y: u16| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>();
        let r = p.list_rect;
        let (textual, exact) = (line(r.y + 1), line(r.y + 2));
        assert!(textual.trim_end().trim_end_matches('\u{2502}').trim_end().ends_with(" text"), "{textual:?}");
        assert!(!exact.contains(" text"), "{exact:?}");
        let x = r.right() - 5;
        assert_eq!(buf[(x + 1, r.y + 1)].fg, theme.dim.fg.unwrap_or(theme.fg));
    }

    /// The panel drawn in a `w` x `h` terminal; every row as text.
    fn drawn(p: &mut FindPanel, w: u16, h: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| p.render(f, f.area(), true, &theme)).unwrap();
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect()
    }

    #[test]
    fn status_row_reads_whole_at_both_window_widths() {
        let mut wrong = Vec::new();
        for w in [77u16, 117] {
            let mut p = FindPanel::new();
            p.query.set_text("old");
            p.preview = Some(preview_of(1));
            p.show_replace = true;
            p.on_batch(Batch { seq: p.current_seq(), hits: rows(1).1, files_searched: 2, done: true, capped: false });
            p.list.select(1);
            p.toggle_exclude();
            let lines = drawn(&mut p, w, 10);
            let want = "3 matches in 2 files; 1 excluded + 1 file operation";
            if !lines[3].contains(want) {
                wrong.push(format!("width {w}: row 3 is {:?}", lines[3]));
            }
            if p.list_rect.y != 4 {
                wrong.push(format!("width {w}: list starts at {}", p.list_rect.y));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn an_ordinary_search_has_the_status_under_find() {
        let mut p = FindPanel::new();
        p.query.set_text("old");
        p.on_batch(Batch {
            seq: p.current_seq(),
            hits: vec![hit("a", 0, "old"), hit("a", 1, "old")],
            files_searched: 1,
            done: true,
            capped: false,
        });
        let lines = drawn(&mut p, 60, 10);
        assert!(lines[2].contains("2 matches in 1 file"), "{:?}", lines[2]);
        assert!(!lines[1].contains("matches"), "{:?}", lines[1]);
        assert_eq!(p.list_rect.y, 3);
        assert_eq!(p.list.hit(p.list_rect, 5, 2), None);
        assert_eq!(p.list.hit(p.list_rect, 5, 3), Some(0));
    }

    #[test]
    fn a_short_window_gives_up_the_status_row_first() {
        let mut wrong = Vec::new();
        // (replace row, terminal height, status row, list rows)
        let cases =
            [(false, 4u16, None, 1u16), (false, 5, Some(2usize), 1), (false, 3, None, 0), (true, 5, None, 1), (true, 6, Some(3), 1)];
        for (replace, h, status_at, list_h) in cases {
            let mut p = FindPanel::new();
            p.query.set_text("old");
            p.show_replace = replace;
            p.on_batch(Batch {
                seq: p.current_seq(),
                hits: vec![hit("a", 0, "old"), hit("a", 1, "old")],
                files_searched: 1,
                done: true,
                capped: false,
            });
            let lines = drawn(&mut p, 60, h);
            let at: Vec<usize> = (0..lines.len()).filter(|&y| lines[y].contains("matches in")).collect();
            if at != status_at.into_iter().collect::<Vec<_>>() {
                wrong.push(format!("replace {replace}, height {h}: status on rows {at:?}, want {status_at:?}"));
            }
            if p.list_rect.height != list_h {
                wrong.push(format!("replace {replace}, height {h}: {} list rows, want {list_h}", p.list_rect.height));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn ask_replace_all_in_preview() {
        let (dir, config, theme, workspace) = temp_root("preview-ask");
        let mut cx = test_ctx(&config, &theme, &workspace);
        let mut p = FindPanel::new();
        p.open_preview(preview_of(1), Vec::new(), &mut cx);
        p.ask_replace_all(&mut cx);
        let Some(Effect::Dialog(d)) = cx.effects.pop() else { panic!("a dialog") };
        assert_eq!((d.title.as_str(), d.id), ("Rename", DialogId::ReplaceInFiles));
        assert!(d.message.contains("File operations:\n  delete /p/f0"), "{}", d.message);
        assert!(cx.effects.is_empty());
        // Everything excluded and no operation: the toast.
        let (preview, hits) = rows(0);
        p.open_preview(preview, hits, &mut cx);
        p.list.select(0);
        p.toggle_exclude();
        p.list.select(3);
        p.toggle_exclude();
        p.ask_replace_all(&mut cx);
        assert!(matches!(cx.effects.as_slice(), [Effect::Notify(Level::Info, t)] if t == "Every result is excluded; nothing to replace"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn preview_panel(hits: Vec<Hit>) -> FindPanel {
        let mut p = FindPanel::new();
        p.query.set_text("old");
        p.preview = Some(preview_of(0));
        p.on_batch(Batch { seq: p.current_seq(), hits, files_searched: 1, done: true, capped: false });
        p
    }

    fn included(p: &FindPanel) -> Vec<bool> {
        p.hits.iter().map(|h| !h.excluded).collect()
    }

    #[test]
    fn header_toggle_in_a_preview_keeps_guesses_out_unless_alone() {
        // Mixed file: a (exact, guess, exact), b (guess only).
        let mut p = preview_panel(vec![
            rename_hit("a", 0, 3, "new", Some((0, 0)), RenameSource::Exact),
            rename_hit("a", 1, 3, "new", None, RenameSource::Textual),
            rename_hit("a", 2, 3, "new", Some((0, 1)), RenameSource::Exact),
            rename_hit("b", 0, 3, "new", None, RenameSource::Textual),
        ]);
        // Rows: 0 a, 1-3 its hits, 4 b, 5 b:0.
        p.list.select(0);
        p.toggle_exclude();
        assert_eq!(included(&p), [false, false, false, false], "any included: exclude all");
        p.toggle_exclude();
        assert_eq!(included(&p), [true, false, true, false], "all excluded: exact rows return, guesses stay out");
        p.toggle_exclude();
        assert_eq!(included(&p), [false, false, false, false]);
        // A file of guesses only: one press includes them all.
        p.list.select(4);
        p.toggle_exclude();
        assert_eq!(included(&p), [false, false, false, true]);
    }

    #[test]
    fn header_toggle_in_an_ordinary_search_flips_every_row() {
        let mut p = panel();
        p.list.select(0);
        p.toggle_exclude();
        assert_eq!(included(&p), [false, false, true]);
        p.toggle_exclude();
        assert_eq!(included(&p), [true, true, true]);
    }
}
