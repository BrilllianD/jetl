//! Command palette overlay: Go to File, Go to Symbol, Find Action, Search
//! Everywhere, Recent Files, Branches, Recent Projects, Run tasks, Scratch
//! Files, a file's Local History and a language server's locations
//! (definitions, usages). The app
//! owns it (`App.palette`) and routes all input to it while it is open,
//! like a dialog. Keys arrive as
//! `PaletteAction`s from the `palette` keymap context; unmapped keys edit the
//! query. The palette holds copies of its candidates (the file index, the
//! action list, branches, recent files and projects, symbols) that the app refreshes
//! as they load. Large file and symbol lists are ranked off the main thread
//! ([`FilterBus`], `AppMsg::PaletteFiltered`).

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, PaletteAction, PaletteKind};
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::git::BranchInfo;
use crate::lsp::locations::LocationItem;
use crate::scratch::ScratchFile;
use crate::symbols::index::SymbolEntry;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

/// Upper bound for the cached file walk, so a huge directory cannot stall
/// matching.
pub const MAX_FILES: usize = 200_000;

/// Files or symbols from which a non-empty query is ranked off the main
/// thread (when the palette has a [`FilterBus`]); smaller lists rank at
/// once. Below it a keystroke costs at most about 3 ms in release, 6 ms at
/// twice as many (`filter_200k_paths_timing`).
pub const BACKGROUND_MIN: usize = 10_000;

/// How many candidates a ranking scores between checks for cancellation.
const CANCEL_EVERY: usize = 1024;

/// Where filters of large sources run: the bus, the generation the palette
/// opened in, and the app-wide filter seq. Every refilter of any palette takes
/// the next seq, so a result from a closed palette never matches a new one, and
/// a running filter stops as soon as the seq moves past its own.
#[derive(Clone)]
pub struct FilterBus {
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
    pub generation: u64,
    pub seq: Arc<AtomicU64>,
}

/// The candidates `rows` index into, and the query they were ranked for.
struct Shown {
    files: Arc<Vec<String>>,
    symbols: Arc<Vec<SymbolEntry>>,
    query: String,
}

/// A filter running off the main thread.
struct Pending {
    seq: u64,
    /// The query changed since `rows` were ranked: select the top row on apply.
    reset: bool,
    /// The user moved the selection in the shown rows meanwhile.
    moved: bool,
    files: Arc<Vec<String>>,
    symbols: Arc<Vec<SymbolEntry>>,
    query: String,
}

/// The candidates of one off-thread filter.
enum FilterInput {
    Paths(Arc<Vec<String>>),
    Symbols(Arc<Vec<SymbolEntry>>),
}

/// What the selected row shows, to find it again among new rows.
enum RowKey {
    File(String),
    Symbol(Arc<str>, usize, usize, Arc<str>),
}

/// What a palette tab lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Files,
    Actions,
    Branches,
    Recent,
    Projects,
    Symbols,
    Tasks,
    /// A language server's locations (`PaletteKind::Locations`).
    Locations,
    Scratches,
    /// One file's local history snapshots (`PaletteKind::LocalHistory`).
    LocalHistory,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Files => "Files",
            Source::Actions => "Actions",
            Source::Branches => "Branches",
            Source::Recent => "Recent Files",
            Source::Projects => "Recent Projects",
            Source::Symbols => "Symbols",
            Source::Tasks => "Run",
            Source::Locations => "Locations",
            Source::Scratches => "Scratch Files",
            Source::LocalHistory => "Local History",
        }
    }
}

/// One Find Action row.
#[derive(Debug, Clone)]
pub struct ActionEntry {
    pub id: &'static str,
    pub desc: &'static str,
    pub action: Action,
    pub shortcut: Option<String>,
}

/// What the user picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accept {
    OpenFile(PathBuf),
    Action(Action),
    /// The branches popup's operations for a branch.
    Branch(crate::git::BranchInfo),
    NewBranch,
    SwitchRoot(PathBuf),
    /// Open a file at a 0-based line and char column.
    OpenFileAt {
        path: PathBuf,
        line: usize,
        col: usize,
    },
    RunTask(Box<crate::run::RunTask>),
    /// Shift+Delete on a scratch file: the app asks first.
    DeleteScratch(PathBuf),
    /// Shift+Delete on a recent project: the palette has dropped the row
    /// and stays open; the app drops it from the recent projects file.
    ForgetProject(PathBuf),
    /// A local history snapshot of `path`: its diff against the current
    /// text, in the Git window's History tab; `title` is the row's date,
    /// time and label.
    LocalHistoryDiff {
        path: PathBuf,
        id: String,
        at: u64,
        title: String,
    },
}

/// One Local History row, formatted once (`Palette::set_history`).
#[derive(Debug, Clone)]
struct HistoryRow {
    id: String,
    at: u64,
    /// `YYYY-MM-DD HH:MM:SS`, local time.
    when: String,
    label: String,
    /// "1.2 KB  +3 lines".
    info: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteResult {
    Accept(Accept),
    Cancel,
}

const NEW_BRANCH: &str = "+ New Branch…";

pub struct Palette {
    pub kind: PaletteKind,
    tabs: Vec<Source>,
    tab: usize,
    pub input: TextInput,
    list: ListState,
    /// Candidate indices of the current source in display order. For
    /// branches, 0 is "New Branch" and `i` is `branches[i - 1]`.
    rows: Vec<usize>,
    root: PathBuf,
    files: Arc<Vec<String>>,
    files_loading: bool,
    /// Recently used files, most recent first.
    recent: Vec<PathBuf>,
    actions: Vec<ActionEntry>,
    branches: Vec<BranchInfo>,
    /// Recent project roots other than the current one, newest first, and
    /// how they are shown (`~` for the home directory).
    projects: Vec<PathBuf>,
    projects_shown: Vec<String>,
    /// The project symbol index and whether it is still being built.
    symbols: Arc<Vec<SymbolEntry>>,
    symbols_loading: bool,
    /// Run tasks of the root and whether they are still loading.
    tasks: Vec<crate::run::RunTask>,
    tasks_loading: bool,
    /// Locations and the title they are shown under.
    locations: Vec<LocationItem>,
    locations_title: String,
    /// Scratch files, newest first, their names (what is matched) and
    /// local modification dates.
    scratches: Vec<ScratchFile>,
    scratch_names: Vec<String>,
    scratch_dates: Vec<String>,
    /// Local History: the file, the title, its snapshots newest first and
    /// whether they are still loading.
    history_path: Option<PathBuf>,
    history_title: String,
    history: Vec<HistoryRow>,
    history_loading: bool,
    matcher: Matcher,
    /// Where large sources are filtered (set by `App::open_palette`); none
    /// filters everything at once.
    bus: Option<FilterBus>,
    /// The filter seq: the bus's app-wide one once set, else the palette's own.
    filter_seq: Arc<AtomicU64>,
    background_min: usize,
    /// What `rows` index into (lags behind while a filter is pending).
    shown: Shown,
    pending: Option<Pending>,
    outer: Rect,
    list_rect: Rect,
    tab_rects: Vec<(usize, Rect)>,
}

impl Palette {
    pub fn new(kind: PaletteKind, root: PathBuf) -> Self {
        let tabs = match kind {
            PaletteKind::Files => vec![Source::Files],
            PaletteKind::Actions => vec![Source::Actions],
            PaletteKind::Everywhere => vec![Source::Files, Source::Symbols, Source::Actions, Source::Branches],
            PaletteKind::RecentFiles => vec![Source::Recent],
            PaletteKind::RecentProjects => vec![Source::Projects],
            PaletteKind::Symbols => vec![Source::Symbols],
            PaletteKind::RunTasks => vec![Source::Tasks],
            PaletteKind::Locations => vec![Source::Locations],
            PaletteKind::Scratches => vec![Source::Scratches],
            PaletteKind::LocalHistory => vec![Source::LocalHistory],
            // Go to Line stays a dialog and never opens a palette.
            PaletteKind::Branches | PaletteKind::GotoLine => vec![Source::Branches],
        };
        Self {
            kind,
            tabs,
            tab: 0,
            input: TextInput::new(false),
            list: ListState::default(),
            rows: Vec::new(),
            root,
            files: Arc::new(Vec::new()),
            files_loading: false,
            recent: Vec::new(),
            actions: Vec::new(),
            branches: Vec::new(),
            projects: Vec::new(),
            projects_shown: Vec::new(),
            symbols: Arc::new(Vec::new()),
            symbols_loading: false,
            tasks: Vec::new(),
            tasks_loading: false,
            locations: Vec::new(),
            locations_title: String::new(),
            scratches: Vec::new(),
            scratch_names: Vec::new(),
            scratch_dates: Vec::new(),
            history_path: None,
            history_title: "Local History".into(),
            history: Vec::new(),
            history_loading: false,
            matcher: Matcher::default(),
            bus: None,
            filter_seq: Arc::new(AtomicU64::new(0)),
            background_min: BACKGROUND_MIN,
            shown: Shown { files: Arc::new(Vec::new()), symbols: Arc::new(Vec::new()), query: String::new() },
            pending: None,
            outer: Rect::default(),
            list_rect: Rect::default(),
            tab_rects: Vec::new(),
        }
    }

    pub fn source(&self) -> Source {
        self.tabs[self.tab]
    }

    /// Rows shown and the selected one (tests outside this module).
    #[cfg(test)]
    pub(crate) fn rows_selected(&self) -> (usize, usize) {
        (self.rows.len(), self.list.selected)
    }

    /// Filter large sources off the main thread from now on, under the
    /// app-wide seq of `bus`.
    pub fn set_background(&mut self, bus: FilterBus) {
        self.filter_seq = bus.seq.clone();
        self.bus = Some(bus);
    }

    #[cfg(test)]
    pub(crate) fn set_background_min(&mut self, n: usize) {
        self.background_min = n;
    }

    /// Whether a filter is running off the main thread.
    #[cfg(test)]
    pub(crate) fn filter_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether any tab lists `s` (the app only feeds sources that are shown).
    pub fn wants(&self, s: Source) -> bool {
        self.tabs.contains(&s)
    }

    pub fn set_query(&mut self, q: &str) {
        self.input.set_text(q);
        self.refilter(true);
    }

    pub fn set_files(&mut self, files: Arc<Vec<String>>, loading: bool) {
        self.files = files;
        self.files_loading = loading;
        if self.source() == Source::Files {
            self.refilter(false);
        }
    }

    pub fn set_recent(&mut self, recent: Vec<PathBuf>) {
        self.recent = recent;
        if matches!(self.source(), Source::Recent | Source::Files) {
            self.refilter(false);
        }
        // Recent Files opens on the previous file, like the tab switcher.
        if self.source() == Source::Recent && self.input.text.is_empty() && self.rows.len() > 1 {
            self.list.select(1);
        }
    }

    pub fn set_actions(&mut self, actions: Vec<ActionEntry>) {
        self.actions = actions;
        if self.source() == Source::Actions {
            self.refilter(false);
        }
    }

    pub fn set_branches(&mut self, branches: Vec<BranchInfo>) {
        self.branches = branches;
        if self.source() == Source::Branches {
            self.refilter(false);
        }
    }

    pub fn set_symbols(&mut self, symbols: Arc<Vec<SymbolEntry>>, loading: bool) {
        self.symbols = symbols;
        self.symbols_loading = loading;
        if self.source() == Source::Symbols {
            self.refilter(false);
        }
    }

    pub fn set_tasks(&mut self, tasks: Vec<crate::run::RunTask>, loading: bool) {
        self.tasks = tasks;
        self.tasks_loading = loading;
        if self.source() == Source::Tasks {
            self.refilter(false);
        }
    }

    /// The rows of a Locations palette, in the order given, under `title`.
    pub fn set_locations(&mut self, title: String, items: Vec<LocationItem>) {
        self.locations = items;
        self.locations_title = title;
        if self.source() == Source::Locations {
            self.refilter(true);
        }
    }

    /// The scratch files, newest first (`scratch::list`).
    pub fn set_scratches(&mut self, files: Vec<ScratchFile>) {
        self.scratch_names = files.iter().map(|f| f.name.clone()).collect();
        self.scratch_dates = files
            .iter()
            .map(|f| {
                let offset = crate::ui::event_log::local_offset(f.modified) / 60;
                crate::editor::blame::date(f.modified, i32::try_from(offset).unwrap_or(0))
            })
            .collect();
        self.scratches = files;
        if self.source() == Source::Scratches {
            self.refilter(false);
        }
    }

    /// Show the local history of `path`; the snapshots follow
    /// ([`Palette::set_history`]).
    pub fn set_history_target(&mut self, path: PathBuf) {
        let name = path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        self.history_title = format!("Local History: {name}");
        self.history_path = Some(path);
        self.history.clear();
        self.history_loading = true;
        if self.source() == Source::LocalHistory {
            self.refilter(true);
        }
    }

    /// The file whose local history is shown.
    pub fn history_path(&self) -> Option<&Path> {
        self.history_path.as_deref()
    }

    /// The snapshots of the file, newest first (`history::store::list`);
    /// `offset` gives seconds east of UTC at a time (injected for tests).
    pub fn set_history(&mut self, entries: Vec<crate::history::Entry>, offset: impl Fn(i64) -> i64) {
        self.history = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                // Lines added or removed since the next older snapshot.
                let delta = match entries.get(i + 1) {
                    None => String::new(),
                    Some(older) if older.lines == e.lines => "  =".into(),
                    Some(older) if e.lines > older.lines => plural(e.lines - older.lines, "+"),
                    Some(older) => plural(older.lines - e.lines, "−"),
                };
                HistoryRow {
                    id: e.blob.clone(),
                    at: e.at,
                    when: crate::history::local_time(e.at, &offset),
                    label: e.label_in(&self.root),
                    info: format!("{}{delta}", human_size(e.len)),
                }
            })
            .collect();
        self.history_loading = false;
        if self.source() == Source::LocalHistory {
            self.refilter(false);
        }
    }

    /// Recent project roots, newest first; the current root is left out.
    pub fn set_projects(&mut self, roots: &[PathBuf]) {
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
        self.projects = roots.iter().filter(|r| **r != self.root).cloned().collect();
        self.projects_shown = self.projects.iter().map(|r| tilde_path(r, home.as_deref())).collect();
        if self.source() == Source::Projects {
            self.refilter(false);
        }
    }

    /// Rank the current source against the query. `reset` moves the
    /// selection to the top (the query changed); otherwise it is kept. Large
    /// files and symbols rank off the main thread when the palette has a bus
    /// ([`Palette::set_background`]); the shown rows stay until the result
    /// arrives ([`Palette::apply_filter`]).
    fn refilter(&mut self, reset: bool) {
        self.refilter_in(reset, true);
    }

    /// [`Palette::refilter`], at once unless `background` allows otherwise.
    fn refilter_in(&mut self, mut reset: bool, background: bool) {
        let q = self.input.text.trim().to_string();
        // Every refilter supersedes (and so cancels) the one in flight.
        let seq = self.filter_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut moved = false;
        if let Some(p) = self.pending.take() {
            reset |= p.reset;
            moved = !reset && p.moved;
        }
        let input = if background { self.background_input(&q) } else { None };
        if let Some(input) = input {
            let Some(bus) = &self.bus else { return };
            let (tx, generation, latest) = (bus.tx.clone(), bus.generation, self.filter_seq.clone());
            let query = q.clone();
            tokio::task::spawn_blocking(move || {
                let mut m = Matcher::default();
                let t = std::time::Instant::now();
                let cancelled = || latest.load(Ordering::Relaxed) != seq;
                let n = match &input {
                    FilterInput::Paths(p) => p.len(),
                    FilterInput::Symbols(s) => s.len(),
                };
                match rank_large(&query, &input, &mut m, &cancelled) {
                    None => tracing::debug!("palette filter {seq} cancelled"),
                    Some(rows) => {
                        tracing::debug!("palette filter {seq}: {} of {n} in {:?}", rows.len(), t.elapsed());
                        if !cancelled() {
                            let _ = tx.send(crate::event::AppMsg::PaletteFiltered { generation, seq, rows });
                        }
                    }
                }
            });
            self.pending = Some(Pending { seq, reset, moved, files: self.files.clone(), symbols: self.symbols.clone(), query: q });
            return;
        }
        self.rows = self.rank_now(&q);
        self.shown = Shown { files: self.files.clone(), symbols: self.symbols.clone(), query: q };
        self.list.set_len(self.rows.len());
        if reset {
            self.list.select(0);
            self.list.offset = 0;
        }
    }

    /// The candidates to rank off the main thread for `q`, if it should be.
    fn background_input(&self, q: &str) -> Option<FilterInput> {
        if q.is_empty() || self.bus.is_none() || tokio::runtime::Handle::try_current().is_err() {
            return None;
        }
        match self.source() {
            Source::Files if self.files.len() >= self.background_min => Some(FilterInput::Paths(self.files.clone())),
            Source::Symbols if self.symbols.len() >= self.background_min => Some(FilterInput::Symbols(self.symbols.clone())),
            _ => None,
        }
    }

    /// The current source ranked against `q` on this thread.
    fn rank_now(&mut self, q: &str) -> Vec<usize> {
        match self.source() {
            Source::Files => {
                if q.is_empty() {
                    // Open files first (most recent first), then the rest.
                    let mut rows: Vec<usize> = Vec::new();
                    for p in &self.recent {
                        if let Ok(rel) = p.strip_prefix(&self.root) {
                            let rel = rel.to_string_lossy();
                            if let Ok(i) = self.files.binary_search_by(|f| f.as_str().cmp(&rel)) {
                                rows.push(i);
                            }
                        }
                    }
                    let first: std::collections::HashSet<usize> = rows.iter().copied().collect();
                    rows.extend((0..self.files.len()).filter(|i| !first.contains(i)));
                    rows
                } else {
                    let input = FilterInput::Paths(self.files.clone());
                    rank_large(q, &input, &mut self.matcher, &|| false).unwrap_or_default()
                }
            }
            Source::Recent => {
                let shown: Vec<String> = self.recent.iter().map(|p| display_path(p, &self.root)).collect();
                if q.is_empty() { (0..shown.len()).collect() } else { rank_paths(q, &shown, &mut self.matcher) }
            }
            Source::Projects => {
                let shown = &self.projects_shown;
                if q.is_empty() { (0..shown.len()).collect() } else { rank_paths(q, shown, &mut self.matcher) }
            }
            Source::Scratches => {
                let names = &self.scratch_names;
                if q.is_empty() { (0..names.len()).collect() } else { rank_paths(q, names, &mut self.matcher) }
            }
            Source::LocalHistory => {
                if q.is_empty() {
                    (0..self.history.len()).collect()
                } else {
                    rank_texts(q, self.history.iter().map(|r| [r.label.as_str(), r.when.as_str()]), &mut self.matcher)
                }
            }
            Source::Actions => {
                if q.is_empty() {
                    (0..self.actions.len()).collect()
                } else {
                    let texts = self.actions.iter().map(|a| [a.desc, a.id]);
                    rank_texts(q, texts, &mut self.matcher)
                }
            }
            Source::Symbols => {
                if q.is_empty() {
                    (0..self.symbols.len()).collect()
                } else {
                    let input = FilterInput::Symbols(self.symbols.clone());
                    rank_large(q, &input, &mut self.matcher, &|| false).unwrap_or_default()
                }
            }
            Source::Tasks => {
                if q.is_empty() {
                    (0..self.tasks.len()).collect()
                } else {
                    let commands: Vec<String> = self.tasks.iter().map(|t| t.command.display()).collect();
                    let texts = self.tasks.iter().zip(&commands).map(|(t, c)| [t.name.as_str(), c.as_str()]);
                    rank_texts(q, texts, &mut self.matcher)
                }
            }
            Source::Locations => {
                if q.is_empty() {
                    (0..self.locations.len()).collect()
                } else {
                    let paths: Vec<String> = self.locations.iter().map(|l| display_path(&l.path, &self.root)).collect();
                    let texts = self.locations.iter().zip(&paths).map(|(l, p)| [l.text.as_str(), p.as_str()]);
                    rank_texts(q, texts, &mut self.matcher)
                }
            }
            Source::Branches => {
                let mut texts: Vec<[&str; 1]> = vec![["New Branch"]];
                texts.extend(self.branches.iter().map(|b| [b.name.as_str()]));
                if q.is_empty() { (0..texts.len()).collect() } else { rank_texts(q, texts.into_iter(), &mut self.matcher) }
            }
        }
    }

    /// Show the rows of off-thread filter `seq` if it is still the latest of
    /// this palette; false (dropped) otherwise.
    pub fn apply_filter(&mut self, seq: u64, rows: Vec<usize>) -> bool {
        if !self.pending.as_ref().is_some_and(|p| p.seq == seq) {
            return false;
        }
        if let Some(p) = self.pending.take() {
            self.show_rows(p, rows);
        }
        true
    }

    /// Rank a pending filter here and now (Enter right after typing): its
    /// job is cancelled and a late result dropped.
    fn finish_now(&mut self) {
        let Some(p) = self.pending.take() else { return };
        self.filter_seq.fetch_add(1, Ordering::Relaxed);
        let rows = self.rank_now(&p.query);
        self.show_rows(p, rows);
    }

    /// Show `rows`, ranked by pending filter `p`. The selected row stays
    /// selected (by what it shows) if the query is unchanged or the user
    /// picked it meanwhile; a new query selects the top row.
    fn show_rows(&mut self, p: Pending, rows: Vec<usize>) {
        let key = if p.moved || !p.reset { self.selected_key() } else { None };
        self.rows = rows;
        self.shown = Shown { files: p.files, symbols: p.symbols, query: p.query };
        self.list.set_len(self.rows.len());
        match key.and_then(|k| self.rows.iter().position(|&i| self.row_is(i, &k))) {
            Some(row) => self.list.select(row),
            None if p.reset => {
                self.list.select(0);
                self.list.offset = 0;
            }
            None => {}
        }
    }

    /// What the selected row shows (files and symbols only).
    fn selected_key(&self) -> Option<RowKey> {
        let i = *self.rows.get(self.list.selected)?;
        match self.source() {
            Source::Files => Some(RowKey::File(self.shown.files.get(i)?.clone())),
            Source::Symbols => {
                let e = self.shown.symbols.get(i)?;
                Some(RowKey::Symbol(e.path.clone(), e.symbol.line, e.symbol.col, e.symbol.name.clone()))
            }
            _ => None,
        }
    }

    /// Whether candidate `i` of the shown ones is what `key` names.
    fn row_is(&self, i: usize, key: &RowKey) -> bool {
        match key {
            RowKey::File(f) => self.shown.files.get(i).is_some_and(|x| x == f),
            RowKey::Symbol(path, line, col, name) => self
                .shown
                .symbols
                .get(i)
                .is_some_and(|e| e.path == *path && e.symbol.line == *line && e.symbol.col == *col && e.symbol.name == *name),
        }
    }

    /// Rows still index the previous tab's candidates, so the new tab ranks
    /// at once, never off the main thread.
    fn switch_tab(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.tab {
            self.tab = i;
            self.refilter_in(true, false);
        }
    }

    /// The user picked a row among the shown ones while a filter runs.
    fn moved(&mut self) {
        if let Some(p) = &mut self.pending {
            p.moved = true;
        }
    }

    fn accept(&self, row: usize) -> Option<PaletteResult> {
        let i = *self.rows.get(row)?;
        let a = match self.source() {
            Source::Files => Accept::OpenFile(self.root.join(self.shown.files.get(i)?)),
            Source::Recent => Accept::OpenFile(self.recent.get(i)?.clone()),
            Source::Projects => Accept::SwitchRoot(self.projects.get(i)?.clone()),
            Source::Symbols => {
                let e = self.shown.symbols.get(i)?;
                Accept::OpenFileAt { path: self.root.join(&*e.path), line: e.symbol.line, col: e.symbol.col }
            }
            Source::Actions => Accept::Action(self.actions.get(i)?.action.clone()),
            Source::Tasks => Accept::RunTask(Box::new(self.tasks.get(i)?.clone())),
            Source::Scratches => Accept::OpenFile(self.scratches.get(i)?.path.clone()),
            Source::LocalHistory => {
                let r = self.history.get(i)?;
                let title = format!("{}  {}", r.when, r.label);
                Accept::LocalHistoryDiff { path: self.history_path.clone()?, id: r.id.clone(), at: r.at, title }
            }
            Source::Locations => {
                let l = self.locations.get(i)?;
                Accept::OpenFileAt { path: l.path.clone(), line: l.line, col: l.col }
            }
            Source::Branches if i == 0 => Accept::NewBranch,
            Source::Branches => Accept::Branch(self.branches.get(i - 1)?.clone()),
        };
        Some(PaletteResult::Accept(a))
    }

    pub fn handle_action(&mut self, a: &PaletteAction) -> Option<PaletteResult> {
        let page = self.list.viewport.max(1);
        match a {
            PaletteAction::Up | PaletteAction::Down | PaletteAction::PageUp | PaletteAction::PageDown => {
                self.moved();
                match a {
                    PaletteAction::Up => self.list.up(1),
                    PaletteAction::Down => self.list.down(1),
                    PaletteAction::PageUp => self.list.up(page),
                    _ => self.list.down(page),
                }
            }
            // A row picked among the shown ones is taken as it is; otherwise
            // the best match of the query typed so far, ranked now.
            PaletteAction::Accept => {
                if self.pending.as_ref().is_some_and(|p| p.moved)
                    && let Some(r) = self.accept(self.list.selected)
                {
                    self.pending = None;
                    self.filter_seq.fetch_add(1, Ordering::Relaxed);
                    return Some(r);
                }
                self.finish_now();
                return self.accept(self.list.selected);
            }
            PaletteAction::Cancel => return Some(PaletteResult::Cancel),
            PaletteAction::Backspace => {
                self.input.backspace();
                self.refilter(true);
            }
            PaletteAction::DeleteWord => {
                self.input.delete_word_back();
                self.refilter(true);
            }
            PaletteAction::NextTab => self.switch_tab((self.tab + 1) % self.tabs.len()),
            PaletteAction::PrevTab => self.switch_tab((self.tab + self.tabs.len() - 1) % self.tabs.len()),
            PaletteAction::DeleteItem if self.source() == Source::Scratches => {
                let f = self.rows.get(self.list.selected).and_then(|&i| self.scratches.get(i))?;
                return Some(PaletteResult::Accept(Accept::DeleteScratch(f.path.clone())));
            }
            // Dropped at once; the selection stays on the row that moves up
            // into its place (the one above at the end).
            PaletteAction::DeleteItem if self.source() == Source::Projects => {
                let i = *self.rows.get(self.list.selected)?;
                let root = self.projects.remove(i);
                self.projects_shown.remove(i);
                self.refilter(false);
                return Some(PaletteResult::Accept(Accept::ForgetProject(root)));
            }
            // Nothing to delete: Shift+Delete deletes in the query, as before.
            PaletteAction::DeleteItem => {
                self.input.delete();
                self.refilter(true);
            }
        }
        None
    }

    /// Keys the keymap does not bind: typing and caret movement in the query.
    pub fn handle_text_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) => {
                self.input.insert(c);
                self.refilter(true);
            }
            KeyCode::Delete => {
                self.input.delete();
                self.refilter(true);
            }
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            _ => {}
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.input.insert_str(text.lines().next().unwrap_or(""));
        self.refilter(true);
    }

    /// Whether a screen cell is inside the palette as last drawn.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.outer.contains(ratatui::layout::Position::new(x, y))
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<PaletteResult> {
        let p = ratatui::layout::Position::new(m.x, m.y);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((i, _)) = self.tab_rects.iter().find(|(_, r)| r.contains(p)) {
                    self.switch_tab(*i);
                    return None;
                }
                if let Some(row) = self.list.hit(self.list_rect, m.x, m.y) {
                    self.moved();
                    self.list.select(row);
                    if m.clicks >= 2 {
                        return self.accept(row);
                    }
                    return None;
                }
                if !self.outer.contains(p) {
                    return Some(PaletteResult::Cancel);
                }
                None
            }
            MouseEventKind::ScrollUp => {
                self.list.scroll(-3);
                None
            }
            MouseEventKind::ScrollDown => {
                self.list.scroll(3);
                None
            }
            _ => None,
        }
    }

    fn title(&self) -> &str {
        match self.kind {
            PaletteKind::Locations => &self.locations_title,
            PaletteKind::LocalHistory => &self.history_title,
            PaletteKind::Files => "Go to File",
            PaletteKind::Actions => "Find Action",
            PaletteKind::Everywhere => "Search Everywhere",
            PaletteKind::RecentFiles => "Recent Files",
            PaletteKind::RecentProjects => "Recent Projects",
            PaletteKind::Symbols => "Go to Symbol",
            PaletteKind::RunTasks => "Run",
            PaletteKind::Scratches => "Scratch Files",
            PaletteKind::Branches | PaletteKind::GotoLine => "Git Branches",
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let w = (area.width.saturating_sub(4)).min(100).max(area.width.min(40));
        // Shrink to the results, but keep room for a few so the popup does
        // not jump on every keystroke.
        let header = if self.tabs.len() > 1 { 2 } else { 1 };
        let h = area.height.saturating_sub(4).min(22).min(2 + header + (self.rows.len() as u16).max(6));
        if w < 20 || h < 5 {
            return;
        }
        let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height / 8).min(4) + 1, w, h);
        self.outer = rect;
        frame.render_widget(Clear, rect);
        let bg = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.popup_border)
            .title(format!(" {} ", self.title()))
            .title_style(theme.title_focused)
            .style(bg);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let mut y = inner.y;

        // Tab row (Search Everywhere only).
        self.tab_rects.clear();
        if self.tabs.len() > 1 {
            let mut x = inner.x + 1;
            for (i, s) in self.tabs.iter().enumerate() {
                let label = format!(" {} ", s.label());
                let lw = label.width() as u16;
                if x + lw > inner.right() {
                    break;
                }
                let st = if i == self.tab { theme.list_selected.add_modifier(Modifier::BOLD) } else { bg };
                frame.buffer_mut().set_stringn(x, y, &label, lw as usize, st);
                self.tab_rects.push((i, Rect::new(x, y, lw, 1)));
                x += lw + 1;
            }
            let hint = "Tab: next";
            if x + hint.width() as u16 + 1 < inner.right() {
                frame.buffer_mut().set_stringn(inner.right() - hint.width() as u16 - 1, y, hint, hint.len(), theme.dim.bg(theme.popup_bg));
            }
            y += 1;
        }

        // Query row with the match count on the right.
        // A new index or batch still being ranked counts as loading.
        let pending = self.pending.as_ref();
        let loading = match self.source() {
            Source::Files => self.files_loading || pending.is_some_and(|p| !Arc::ptr_eq(&p.files, &self.shown.files)),
            Source::Symbols => self.symbols_loading || pending.is_some_and(|p| !Arc::ptr_eq(&p.symbols, &self.shown.symbols)),
            Source::Tasks => self.tasks_loading,
            Source::LocalHistory => self.history_loading,
            _ => false,
        };
        let count = match self.source() {
            Source::LocalHistory if loading => "loading…".to_string(),
            Source::Files if loading && self.files.is_empty() => "indexing…".to_string(),
            Source::Symbols if loading && self.symbols.is_empty() => "indexing…".to_string(),
            _ if loading => format!("{} (indexing…)", self.rows.len()),
            _ => format!("{}", self.rows.len()),
        };
        let cw = count.width() as u16;
        frame.buffer_mut().set_stringn(inner.x + 1, y, "›", 1, theme.status_accent.bg(theme.popup_bg));
        let qrect = Rect::new(inner.x + 3, y, inner.width.saturating_sub(cw + 6), 1);
        let placeholder = match self.source() {
            Source::Files | Source::Recent => "type a file name",
            Source::Actions => "type an action name",
            Source::Branches => "type a branch name",
            Source::Projects => "type a project name · Shift+Del forgets",
            Source::Symbols => "type a symbol name",
            Source::Tasks => "type a task name",
            Source::Locations => "type to filter",
            Source::Scratches => "type a scratch name · Shift+Del deletes",
            Source::LocalHistory => "type a date or label · Enter shows the diff",
        };
        self.input.render(frame, qrect, Style::default().bg(theme.bg).fg(theme.fg), placeholder, theme.dim.bg(theme.bg), true);
        frame.buffer_mut().set_stringn(inner.right().saturating_sub(cw + 1), y, &count, cw as usize, theme.dim.bg(theme.popup_bg));
        y += 1;

        // Results.
        let lrect = Rect::new(inner.x, y, inner.width, inner.bottom().saturating_sub(y));
        self.list_rect = lrect;
        self.list.fit(self.list.len, lrect.height as usize);
        if self.rows.is_empty() {
            let msg = match self.source() {
                Source::Files if loading => "Indexing files…",
                Source::Symbols if loading => "Indexing symbols…",
                Source::Tasks if loading => "Loading tasks…",
                Source::Tasks if self.input.text.trim().is_empty() => "No tasks: add [[run.tasks]] to config.toml or .jetl/run.toml",
                Source::Scratches if self.input.text.trim().is_empty() => "No scratch files: File > New Scratch File…",
                Source::LocalHistory if loading => "Loading…",
                Source::LocalHistory if self.input.text.trim().is_empty() => "No local history for {name} yet: saves record it",
                _ => "Nothing found",
            };
            let name = self.history_path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy()).unwrap_or_default();
            let msg = msg.replace("{name}", &name);
            frame.buffer_mut().set_stringn(lrect.x + 2, lrect.y, msg, lrect.width.saturating_sub(2) as usize, theme.dim.bg(theme.popup_bg));
            return;
        }
        // The query the rows on screen were ranked for.
        let pattern = Pattern::parse(&self.shown.query, CaseMatching::Smart, Normalization::Smart);
        for vi in self.list.visible() {
            let ry = lrect.y + (vi - self.list.offset) as u16;
            let selected = vi == self.list.selected;
            let base = if selected { theme.list_selected } else { bg };
            frame.buffer_mut().set_style(Rect::new(lrect.x, ry, lrect.width, 1), base);
            let dim = if selected { base } else { theme.dim.bg(theme.popup_bg) };
            let hl = base.fg(theme.status_accent.fg.unwrap_or(theme.fg)).add_modifier(Modifier::BOLD);
            let hl = if selected { base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { hl };
            let i = self.rows[vi];
            let width = lrect.width.saturating_sub(2) as usize;
            let spans: Vec<(String, Style)> = match self.source() {
                Source::Files | Source::Recent | Source::Projects => {
                    let path = match self.source() {
                        Source::Files => self.shown.files[i].clone(),
                        Source::Projects => self.projects_shown[i].clone(),
                        _ => display_path(&self.recent[i], &self.root),
                    };
                    let (dir, name) = match path.rfind('/') {
                        Some(k) => (path[..k + 1].to_string(), path[k + 1..].to_string()),
                        None => (String::new(), path.clone()),
                    };
                    let marks = match_indices(&pattern, &path, &mut self.matcher);
                    let name_off = dir.chars().count();
                    let mut v = highlight(&name, |ci| marks.contains(&((ci + name_off) as u32)), base, hl);
                    v.push(("  ".into(), base));
                    v.extend(highlight(&dir, |ci| marks.contains(&(ci as u32)), dim, hl));
                    v
                }
                Source::Actions => {
                    let a = &self.actions[i];
                    let mut v = vec![(a.desc.to_string(), base), ("  ".into(), base), (a.id.to_string(), dim)];
                    if let Some(s) = &a.shortcut {
                        // Right-align the shortcut when it fits.
                        let used: usize = v.iter().map(|(t, _)| t.width()).sum();
                        let sw = s.width();
                        if used + sw + 2 <= width {
                            v.push((" ".repeat(width - used - sw), base));
                            v.push((s.clone(), if selected { base } else { theme.status_accent.bg(theme.popup_bg) }));
                        }
                    }
                    v
                }
                Source::Symbols => {
                    let e = &self.shown.symbols[i];
                    let marks = match_indices(&pattern, &e.symbol.name, &mut self.matcher);
                    let mut v = highlight(&e.symbol.name, |ci| marks.contains(&(ci as u32)), base, hl);
                    v.push((format!("  {}", e.symbol.kind), dim));
                    v.push((format!("  {}:{}", e.path, e.symbol.line + 1), dim));
                    v
                }
                Source::Tasks => {
                    let t = &self.tasks[i];
                    let marks = match_indices(&pattern, &t.name, &mut self.matcher);
                    let mut v = highlight(&t.name, |ci| marks.contains(&(ci as u32)), base, hl);
                    let command = t.command.display();
                    if command != t.name {
                        v.push((format!("  {command}"), dim));
                    }
                    let origin = t.origin.label();
                    let used: usize = v.iter().map(|(t, _)| t.width()).sum();
                    if used + origin.width() + 2 <= width {
                        v.push((" ".repeat(width - used - origin.width()), base));
                        v.push((origin.to_string(), dim));
                    }
                    v
                }
                Source::Scratches => {
                    // The name, then the language and date on the right.
                    let name = &self.scratch_names[i];
                    let marks = match_indices(&pattern, name, &mut self.matcher);
                    let mut v = highlight(name, |ci| marks.contains(&(ci as u32)), base, hl);
                    let info = format!("{}  {}", self.scratches[i].language, self.scratch_dates[i]);
                    let used = name.width();
                    if used + info.width() + 2 <= width {
                        v.push((" ".repeat(width - used - info.width()), base));
                        v.push((info, dim));
                    }
                    v
                }
                Source::LocalHistory => {
                    // Date and label, then the size and line change on the right.
                    let r = &self.history[i];
                    let mut v = vec![(r.when.clone(), dim), ("  ".into(), base), (r.label.clone(), base)];
                    let used = r.when.width() + 2 + r.label.width();
                    if used + r.info.width() + 2 <= width {
                        v.push((" ".repeat(width - used - r.info.width()), base));
                        v.push((r.info.clone(), dim));
                    }
                    v
                }
                Source::Locations => {
                    // The text, cut to leave room for the place on the right.
                    let l = &self.locations[i];
                    let place = format!("{}:{}", display_path(&l.path, &self.root), l.line + 1);
                    let text = crate::util::text::fit(&l.text, width.saturating_sub(place.width() + 2));
                    let marks = match_indices(&pattern, &l.text, &mut self.matcher);
                    let mut v = highlight(&text, |ci| marks.contains(&(ci as u32)), base, hl);
                    let used = text.width();
                    v.push((" ".repeat(width.saturating_sub(used + place.width()).max(2)), base));
                    v.push((place, dim));
                    v
                }
                Source::Branches if i == 0 => vec![(NEW_BRANCH.to_string(), base)],
                Source::Branches => {
                    let b = &self.branches[i - 1];
                    let mut v = vec![
                        (if b.current { "* " } else { "  " }.to_string(), base),
                        (b.name.clone(), if b.current { base.add_modifier(Modifier::BOLD) } else { base }),
                    ];
                    if let Some(up) = &b.upstream {
                        let mut s = format!("  → {up}");
                        if b.ahead > 0 {
                            s.push_str(&format!(" ↑{}", b.ahead));
                        }
                        if b.behind > 0 {
                            s.push_str(&format!(" ↓{}", b.behind));
                        }
                        v.push((s, dim));
                    }
                    v
                }
            };
            let mut x = lrect.x + 1;
            let right = lrect.x + 1 + width as u16;
            for (text, st) in spans {
                if x >= right {
                    break;
                }
                let (nx, _) = frame.buffer_mut().set_stringn(x, ry, &text, (right - x) as usize, st);
                x = nx;
            }
        }
    }
}

/// Closing the palette stops its filter; a newer palette's filter (already
/// past this seq) is left alone.
impl Drop for Palette {
    fn drop(&mut self) {
        if let Some(p) = &self.pending {
            let _ = self.filter_seq.compare_exchange(p.seq, p.seq + 1, Ordering::Relaxed, Ordering::Relaxed);
        }
    }
}

/// "1 line" / "+3 lines" with `sign` in front.
fn plural(n: u64, sign: &str) -> String {
    format!("  {sign}{n} line{}", if n == 1 { "" } else { "s" })
}

/// "512 B", "1.2 KB", "3.4 MB".
fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

/// Split `text` into runs, styling the chars for which `marked(char index)`
/// holds with `hl`.
fn highlight(text: &str, marked: impl Fn(usize) -> bool, base: Style, hl: Style) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    for (ci, c) in text.chars().enumerate() {
        let st = if marked(ci) { hl } else { base };
        match out.last_mut() {
            Some((s, last)) if *last == st => s.push(c),
            _ => out.push((c.to_string(), st)),
        }
    }
    out
}

/// Char indices of `path` that the query matched (for highlighting).
fn match_indices(pattern: &Pattern, path: &str, matcher: &mut Matcher) -> Vec<u32> {
    matcher.config = nucleo_matcher::Config::DEFAULT.match_paths();
    let mut buf = Vec::new();
    let mut idx = Vec::new();
    if pattern.indices(Utf32Str::new(path, &mut buf), matcher, &mut idx).is_none() {
        idx.clear();
    }
    idx.sort_unstable();
    idx.dedup();
    idx
}

/// A path relative to `root` when inside it, else absolute; `/`-separated.
pub fn display_path(p: &Path, root: &Path) -> String {
    match p.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

/// `p` with the home directory written as `~`.
fn tilde_path(p: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| p.strip_prefix(h).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Some(rel) => format!("~/{}", rel.to_string_lossy()),
        None => p.to_string_lossy().into_owned(),
    }
}

/// Indices of `paths` matching `query`, best first. A path scores the fuzzy
/// match over the whole path plus a bonus when the file name alone matches
/// too, so `apprs` prefers `src/app.rs` over `src/apply/parse.rs`; ties go to
/// the shorter, then alphabetically first path.
pub fn rank_paths(query: &str, paths: &[String], matcher: &mut Matcher) -> Vec<usize> {
    rank_paths_until(query, paths, matcher, &|| false).unwrap_or_default()
}

/// [`rank_paths`] that checks `cancelled` every [`CANCEL_EVERY`] paths and
/// before the sort, and gives up with `None` once it holds.
fn rank_paths_until(query: &str, paths: &[String], matcher: &mut Matcher, cancelled: &dyn Fn() -> bool) -> Option<Vec<usize>> {
    matcher.config = nucleo_matcher::Config::DEFAULT.match_paths();
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if i % CANCEL_EVERY == 0 && i > 0 && cancelled() {
            return None;
        }
        let Some(full) = pattern.score(Utf32Str::new(p, &mut buf), matcher) else { continue };
        let name = p.rsplit('/').next().unwrap_or(p);
        let name_score = pattern.score(Utf32Str::new(name, &mut buf), matcher).unwrap_or(0);
        scored.push((full + name_score, i));
    }
    if cancelled() {
        return None;
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| paths[a.1].len().cmp(&paths[b.1].len())).then_with(|| paths[a.1].cmp(&paths[b.1])));
    Some(scored.into_iter().map(|(_, i)| i).collect())
}

/// Indices of candidates (each with one or more texts) matching `query`,
/// best text score first; ties keep the input order.
fn rank_texts<'a, const N: usize>(query: &str, items: impl Iterator<Item = [&'a str; N]>, matcher: &mut Matcher) -> Vec<usize> {
    rank_texts_until(query, items, matcher, &|| false).unwrap_or_default()
}

/// [`rank_texts`] that checks `cancelled` every [`CANCEL_EVERY`] candidates
/// and before the sort, and gives up with `None` once it holds.
fn rank_texts_until<'a, const N: usize>(
    query: &str,
    items: impl Iterator<Item = [&'a str; N]>,
    matcher: &mut Matcher,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<usize>> {
    matcher.config = nucleo_matcher::Config::DEFAULT;
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = Vec::new();
    for (i, texts) in items.enumerate() {
        if i % CANCEL_EVERY == 0 && i > 0 && cancelled() {
            return None;
        }
        let best = texts.iter().filter_map(|t| pattern.score(Utf32Str::new(t, &mut buf), matcher)).max();
        if let Some(s) = best {
            scored.push((s, i));
        }
    }
    if cancelled() {
        return None;
    }
    scored.sort_by_key(|&(s, i)| (Reverse(s), i));
    Some(scored.into_iter().map(|(_, i)| i).collect())
}

/// Rank a large source (files or symbols) against a non-empty query; the
/// one ranking used both off the main thread and on it, so both give the
/// same rows. `None` when `cancelled` turned true.
fn rank_large(query: &str, input: &FilterInput, matcher: &mut Matcher, cancelled: &dyn Fn() -> bool) -> Option<Vec<usize>> {
    match input {
        FilterInput::Paths(paths) => rank_paths_until(query, paths, matcher, cancelled),
        FilterInput::Symbols(symbols) => rank_texts_until(query, symbols.iter().map(|e| [&*e.symbol.name]), matcher, cancelled),
    }
}

/// Every file under `root` as a sorted, `/`-separated relative path,
/// honouring the same hidden/ignore settings as the Project tree. Runs in
/// `spawn_blocking`.
pub fn walk_files(root: &Path, show_hidden: bool, show_ignored: bool) -> Vec<String> {
    let mut out = Vec::new();
    for entry in crate::files::walk::builder(root, show_hidden, show_ignored).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file() || t.is_symlink()) {
            continue;
        }
        // Only a file beyond the cap is worth the warning.
        if out.len() >= MAX_FILES {
            tracing::warn!("file index of {} capped at {MAX_FILES} files", root.display());
            break;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<String> {
        [
            "Cargo.toml",
            "PLAN.md",
            "assets/keymap.default.toml",
            "docs/ARCHITECTURE.md",
            "src/actions.rs",
            "src/app.rs",
            "src/apply/parse.rs",
            "src/editor/mod.rs",
            "src/main.rs",
            "src/ui/palette.rs",
            "src/worktree/panel.rs",
            "tests/app_test.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn top(query: &str) -> Vec<String> {
        let files = fixture();
        let mut m = Matcher::default();
        rank_paths(query, &files, &mut m).into_iter().map(|i| files[i].clone()).collect()
    }

    #[test]
    fn app_queries_rank_src_app_rs_first() {
        assert_eq!(top("apprs")[0], "src/app.rs");
        assert_eq!(top("app")[0], "src/app.rs");
        assert_eq!(top("srcapp")[0], "src/app.rs");
        assert_eq!(top("app.rs")[0], "src/app.rs");
    }

    #[test]
    fn ranking_prefers_file_names_and_drops_non_matches() {
        assert_eq!(top("palette")[0], "src/ui/palette.rs");
        assert_eq!(top("mainrs"), vec!["src/main.rs".to_string()]);
        assert!(top("zzz").is_empty());
        // Smart case: an uppercase query only matches uppercase.
        assert_eq!(top("PLAN"), vec!["PLAN.md".to_string()]);
    }

    #[test]
    fn actions_rank_by_description_or_id() {
        let mut m = Matcher::default();
        let items = [["Go to file", "palette.files"], ["Save all", "editor.save_all"], ["Save", "editor.save"]];
        let r = rank_texts("save", items.iter().copied(), &mut m);
        assert_eq!(r.len(), 2);
        let r = rank_texts("palfil", items.iter().copied(), &mut m);
        assert_eq!(r, vec![0]);
    }

    #[test]
    fn projects_leave_out_the_root_and_accept_switches_root() {
        let mut p = Palette::new(PaletteKind::RecentProjects, PathBuf::from("/w/here"));
        p.set_projects(&[PathBuf::from("/w/here"), PathBuf::from("/w/alpha"), PathBuf::from("/w/beta")]);
        assert_eq!(p.rows, [0, 1]);
        assert_eq!(p.handle_action(&PaletteAction::Accept), Some(PaletteResult::Accept(Accept::SwitchRoot(PathBuf::from("/w/alpha")))));
        p.set_query("beta");
        assert_eq!(p.handle_action(&PaletteAction::Accept), Some(PaletteResult::Accept(Accept::SwitchRoot(PathBuf::from("/w/beta")))));
    }

    #[test]
    fn delete_item_forgets_a_project_and_keeps_a_neighbour_selected() {
        let w = |n: &str| PathBuf::from("/w").join(n);
        let mut p = Palette::new(PaletteKind::RecentProjects, w("here"));
        p.set_projects(&[w("here"), w("alpha"), w("beta"), w("gamma")]);
        p.handle_action(&PaletteAction::Down);
        let forget = |p: &mut Palette| p.handle_action(&PaletteAction::DeleteItem);
        assert_eq!(forget(&mut p), Some(PaletteResult::Accept(Accept::ForgetProject(w("beta")))));
        // The row below takes its place; at the end, the one above.
        assert_eq!((p.rows.len(), p.list.selected), (2, 1));
        assert_eq!(forget(&mut p), Some(PaletteResult::Accept(Accept::ForgetProject(w("gamma")))));
        assert_eq!((p.rows.len(), p.list.selected), (1, 0));
        assert_eq!(p.handle_action(&PaletteAction::Accept), Some(PaletteResult::Accept(Accept::SwitchRoot(w("alpha")))));
        // A filtered row; nothing left to forget leaves the query alone.
        p.set_query("alp");
        assert_eq!(forget(&mut p), Some(PaletteResult::Accept(Accept::ForgetProject(w("alpha")))));
        assert_eq!(forget(&mut p), None);
        assert_eq!(p.input.text, "alp");
        assert!(p.projects.is_empty() && p.projects_shown.is_empty());
    }

    #[test]
    fn delete_item_deletes_a_scratch_only_on_scratch_files() {
        let f = |name: &str| ScratchFile { path: PathBuf::from("/s").join(name), name: name.into(), language: "Plain text", modified: 0 };
        let mut p = Palette::new(PaletteKind::Scratches, PathBuf::from("/w"));
        p.set_scratches(vec![f("scratch_2.txt"), f("scratch_1.txt")]);
        p.set_query("1");
        let del = Some(PaletteResult::Accept(Accept::DeleteScratch(PathBuf::from("/s/scratch_1.txt"))));
        assert_eq!(p.handle_action(&PaletteAction::DeleteItem), del);
        assert_eq!(
            p.handle_action(&PaletteAction::Accept),
            Some(PaletteResult::Accept(Accept::OpenFile(PathBuf::from("/s/scratch_1.txt"))))
        );
        p.set_query("zzz");
        assert_eq!(p.handle_action(&PaletteAction::DeleteItem), None);

        // Elsewhere it deletes forward in the query.
        let mut p = Palette::new(PaletteKind::Actions, PathBuf::from("/w"));
        p.set_query("ab");
        p.input.home();
        assert_eq!(p.handle_action(&PaletteAction::DeleteItem), None);
        assert_eq!(p.input.text, "b");
    }

    #[test]
    fn home_is_shown_as_tilde() {
        let home = Some(Path::new("/home/u"));
        assert_eq!(tilde_path(Path::new("/home/u/src/jetl"), home), "~/src/jetl");
        assert_eq!(tilde_path(Path::new("/home/u"), home), "~");
        assert_eq!(tilde_path(Path::new("/home/user2/x"), home), "/home/user2/x");
        assert_eq!(tilde_path(Path::new("/opt/x"), None), "/opt/x");
    }

    #[test]
    fn rank_until_stops_when_cancelled_and_matches_otherwise() {
        let files = fixture();
        let mut m = Matcher::default();
        assert_eq!(rank_paths_until("src", &files, &mut m, &|| true), None);
        assert_eq!(rank_paths_until("src", &files, &mut m, &|| false), Some(rank_paths("src", &files, &mut m)));
        let texts = || files.iter().map(|f| [f.as_str()]);
        assert_eq!(rank_texts_until("src", texts(), &mut m, &|| true), None);
        assert_eq!(rank_texts_until("src", texts(), &mut m, &|| false), Some(rank_texts("src", texts(), &mut m)));
        // A long list stops part way, at the first check.
        let many: Vec<String> = (0..10 * CANCEL_EVERY).map(|i| format!("f{i}")).collect();
        let checks = std::cell::Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            true
        };
        assert_eq!(rank_paths_until("f", &many, &mut m, &cancelled), None);
        assert_eq!(checks.get(), 1);
    }

    /// A Files palette filtering off the main thread from one candidate up,
    /// and its bus.
    fn bus_palette(kind: PaletteKind) -> (Palette, tokio::sync::mpsc::UnboundedReceiver<crate::event::AppMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut p = Palette::new(kind, PathBuf::from("/w"));
        p.set_background(FilterBus { tx, generation: 1, seq: Arc::new(AtomicU64::new(0)) });
        p.set_background_min(1);
        (p, rx)
    }

    /// The next filter result on the bus: its seq and rows.
    async fn next_filter(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::event::AppMsg>) -> (u64, Vec<usize>) {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("a filter result in time");
        match msg {
            Some(crate::event::AppMsg::PaletteFiltered { seq, rows, .. }) => (seq, rows),
            _ => panic!("expected a palette filter result"),
        }
    }

    fn pending_seq(p: &Palette) -> u64 {
        p.pending.as_ref().expect("a filter is pending").seq
    }

    fn type_text(p: &mut Palette, text: &str) {
        for c in text.chars() {
            p.handle_text_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    #[tokio::test]
    async fn small_and_busless_palettes_filter_at_once() {
        let files: Vec<String> = (0..30_000).map(|i| format!("dir{}/file_{i}.rs", i % 7)).collect();
        let mut p = Palette::new(PaletteKind::Files, PathBuf::from("/w"));
        p.set_files(Arc::new(files.clone()), false);
        p.set_query("file_123");
        assert!(!p.filter_pending());
        assert_eq!(p.rows, rank_paths("file_123", &files, &mut Matcher::default()));
        // With a bus, below the threshold: at once too.
        let (mut p, _rx) = bus_palette(PaletteKind::Files);
        p.set_background_min(BACKGROUND_MIN);
        p.set_files(Arc::new(fixture()), false);
        p.set_query("src");
        assert!(!p.filter_pending());
        assert_eq!(p.rows, rank_paths("src", &fixture(), &mut Matcher::default()));
    }

    #[tokio::test]
    async fn background_filter_keeps_rows_until_its_result() {
        let (mut p, mut rx) = bus_palette(PaletteKind::Files);
        p.set_files(Arc::new(fixture()), false);
        // The empty query lists everything at once.
        assert!(!p.filter_pending());
        let all = p.rows.clone();
        assert_eq!(all.len(), fixture().len());
        p.handle_action(&PaletteAction::Down);
        p.set_query("src");
        assert!(p.filter_pending());
        assert_eq!((&p.rows, p.shown.query.as_str(), p.list.selected), (&all, "", 1));
        let (seq, rows) = next_filter(&mut rx).await;
        assert!(p.apply_filter(seq, rows));
        assert!(!p.filter_pending());
        assert_eq!(p.rows, rank_paths("src", &fixture(), &mut Matcher::default()));
        assert_eq!((p.shown.query.as_str(), p.list.selected), ("src", 0));
        // Applied once only.
        assert!(!p.apply_filter(seq, vec![0]));
    }

    #[tokio::test]
    async fn stale_seq_is_rejected() {
        let (mut p, mut rx) = bus_palette(PaletteKind::Files);
        p.set_files(Arc::new(fixture()), false);
        p.set_query("a");
        let first = pending_seq(&p);
        p.set_query("ab");
        let latest = pending_seq(&p);
        assert!(latest > first);
        let before = p.rows.clone();
        assert!(!p.apply_filter(first, vec![0]));
        assert_eq!(p.rows, before);
        // The first job may finish before it sees the cancel: dropped too.
        loop {
            let (seq, rows) = next_filter(&mut rx).await;
            if p.apply_filter(seq, rows) {
                assert_eq!(seq, latest);
                break;
            }
            assert_eq!(seq, first);
        }
        assert_eq!(p.rows, rank_paths("ab", &fixture(), &mut Matcher::default()));
    }

    #[tokio::test]
    async fn enter_while_pending_accepts_top_match_of_the_typed_query() {
        let (mut p, _rx) = bus_palette(PaletteKind::Files);
        p.set_files(Arc::new(fixture()), false);
        type_text(&mut p, "mainrs");
        let seq = pending_seq(&p);
        let res = p.handle_action(&PaletteAction::Accept);
        assert_eq!(res, Some(PaletteResult::Accept(Accept::OpenFile(PathBuf::from("/w/src/main.rs")))));
        // The job is cancelled; its result, if any, would be dropped.
        assert!(p.filter_seq.load(Ordering::Relaxed) > seq);
        assert!(!p.apply_filter(seq, vec![0]));
    }

    #[tokio::test]
    async fn enter_after_moving_accepts_the_shown_row() {
        let (mut p, mut rx) = bus_palette(PaletteKind::Files);
        p.set_files(Arc::new(fixture()), false);
        p.set_query("s");
        let (seq, rows) = next_filter(&mut rx).await;
        assert!(p.apply_filter(seq, rows));
        let ranked = rank_paths("s", &fixture(), &mut Matcher::default());
        type_text(&mut p, "r");
        let seq = pending_seq(&p);
        p.handle_action(&PaletteAction::Down);
        let res = p.handle_action(&PaletteAction::Accept);
        let second = PathBuf::from("/w").join(&fixture()[ranked[1]]);
        assert_eq!(res, Some(PaletteResult::Accept(Accept::OpenFile(second))));
        assert!(!p.filter_pending());
        assert!(p.filter_seq.load(Ordering::Relaxed) > seq);
    }

    #[tokio::test]
    async fn non_reset_apply_keeps_the_selected_symbol() {
        let entry = |name: &str, line: usize| SymbolEntry {
            path: Arc::from("src/lib.rs"),
            symbol: crate::symbols::Symbol {
                name: Arc::from(name),
                kind: "function",
                line,
                col: 3,
                start_line: line,
                end_line: line,
                depth: 0,
            },
        };
        let names = |p: &Palette| -> Vec<String> { p.rows.iter().map(|&i| p.shown.symbols[i].symbol.name.to_string()).collect() };
        let selected = |p: &Palette| names(p)[p.list.selected].clone();
        let (mut p, mut rx) = bus_palette(PaletteKind::Symbols);
        p.set_symbols(Arc::new(vec![entry("foo_a", 10), entry("foo_b", 20), entry("foo_c", 30), entry("foo_d", 40)]), true);
        p.set_query("foo");
        let (seq, rows) = next_filter(&mut rx).await;
        assert!(p.apply_filter(seq, rows));
        p.handle_action(&PaletteAction::Down);
        p.handle_action(&PaletteAction::Down);
        assert_eq!((p.list.selected, selected(&p)), (2, "foo_c".to_string()));
        // A batch adds two matches in front of it.
        let mut more = vec![entry("foo_x", 1), entry("foo_y", 2)];
        more.extend(p.symbols.iter().cloned());
        p.set_symbols(Arc::new(more), false);
        assert!(p.filter_pending());
        let (seq, rows) = next_filter(&mut rx).await;
        assert!(p.apply_filter(seq, rows));
        assert_eq!(names(&p).len(), 6);
        assert_eq!((p.list.selected, selected(&p)), (4, "foo_c".to_string()));
        // A batch over a typed query: the top row once both land.
        type_text(&mut p, "_");
        p.set_symbols(Arc::new(p.symbols.iter().cloned().collect()), false);
        let latest = pending_seq(&p);
        loop {
            let (seq, rows) = next_filter(&mut rx).await;
            if p.apply_filter(seq, rows) {
                assert_eq!(seq, latest);
                break;
            }
        }
        assert_eq!(p.list.selected, 0);
    }

    #[tokio::test]
    async fn dropping_a_palette_cancels_its_filter() {
        let (mut p, _rx) = bus_palette(PaletteKind::Files);
        let shared = p.filter_seq.clone();
        p.set_files(Arc::new(fixture()), false);
        p.set_query("src");
        let seq = pending_seq(&p);
        drop(p);
        assert_ne!(shared.load(Ordering::Relaxed), seq);
        // A newer palette's filter is never cancelled by a late drop.
        let (mut old, _rx) = bus_palette(PaletteKind::Files);
        old.set_background(FilterBus { tx: old.bus.clone().unwrap().tx, generation: 1, seq: shared.clone() });
        old.set_files(Arc::new(fixture()), false);
        old.set_query("a");
        let (mut new, _rx2) = bus_palette(PaletteKind::Files);
        new.set_background(FilterBus { tx: new.bus.clone().unwrap().tx, generation: 1, seq: shared.clone() });
        new.set_files(Arc::new(fixture()), false);
        new.set_query("b");
        let seq = pending_seq(&new);
        drop(old);
        assert_eq!(shared.load(Ordering::Relaxed), seq);
    }

    /// Prints the main-thread cost of filtering 200000 paths before (at
    /// once) and after (off the main thread), and of Enter right after
    /// typing. Run in release:
    /// `cargo test --release --lib filter_200k_paths_timing -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn filter_200k_paths_timing() {
        use std::time::{Duration, Instant};
        let gen_paths = |n: usize| {
            let mut v: Vec<String> =
                (0..n).map(|i| format!("crate{}/src/mod{}/sub{}/file_{}.rs", i % 40, i / 40 % 50, i / 2000 % 10, i)).collect();
            v.sort();
            Arc::new(v)
        };
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let paths = gen_paths(MAX_FILES);
        let queries = ["f", "mod", "file_1234", "crate3/sub/fil", "zzzz"];
        eprintln!("{} paths, BACKGROUND_MIN {BACKGROUND_MIN}", paths.len());
        for q in queries {
            let mut sync = Palette::new(PaletteKind::Files, PathBuf::from("/w"));
            sync.set_files(paths.clone(), false);
            let t = Instant::now();
            sync.set_query(q);
            let before = t.elapsed();

            let (mut p, mut rx) = bus_palette(PaletteKind::Files);
            p.set_background_min(BACKGROUND_MIN);
            p.set_files(paths.clone(), false);
            let t = Instant::now();
            p.set_query(q);
            let main = t.elapsed();
            assert!(p.filter_pending());
            let (seq, rows) = next_filter(&mut rx).await;
            let arrived = t.elapsed();
            let t = Instant::now();
            assert!(p.apply_filter(seq, rows));
            let apply = t.elapsed();
            assert_eq!(p.rows, sync.rows, "query {q:?}");

            // Enter right after typing the query.
            p.set_query("");
            p.set_query(q);
            let t = Instant::now();
            let res = p.handle_action(&PaletteAction::Accept);
            let enter = t.elapsed();
            assert_eq!(res, sync.handle_action(&PaletteAction::Accept));
            eprintln!(
                "{q:>16}: {:>6} rows | before {:>7.2} ms | after: main {:>6.3} ms, result in {:>7.2} ms, apply {:>6.3} ms | Enter {:>7.2} ms",
                sync.rows.len(),
                ms(before),
                ms(main),
                ms(arrived),
                ms(apply),
                ms(enter)
            );
        }
        let mut p = Palette::new(PaletteKind::Files, PathBuf::from("/w"));
        p.set_files(paths.clone(), false);
        p.set_query("f");
        let t = Instant::now();
        p.set_query("");
        eprintln!("empty query (at once): {:.2} ms", ms(t.elapsed()));
        // The cost of one keystroke at once, by list size: where to start
        // filtering off the main thread.
        for n in [5_000, 10_000, 15_000, 20_000, 50_000, 100_000] {
            let paths = gen_paths(n);
            let mut p = Palette::new(PaletteKind::Files, PathBuf::from("/w"));
            p.set_files(paths, false);
            let worst = queries
                .iter()
                .map(|q| {
                    let t = Instant::now();
                    p.set_query(q);
                    t.elapsed()
                })
                .max()
                .unwrap_or_default();
            eprintln!("{n:>7} paths at once: worst keystroke {:.2} ms", ms(worst));
        }
    }

    #[test]
    fn walk_skips_git_and_ignored() {
        let dir = std::env::temp_dir().join(format!("jetl-test-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in ["src/a.rs", "target/x.o", ".git/HEAD", ".gitignore", "b.md"] {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, if f == ".gitignore" { "target\n" } else { "" }).unwrap();
        }
        let files = walk_files(&dir, true, false);
        assert_eq!(files, vec![".gitignore", "b.md", "src/a.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
