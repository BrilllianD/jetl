//! Command palette overlay: Go to File, Go to Symbol, Find Action, Search
//! Everywhere, Recent Files, Branches, Recent Projects, Run tasks, Scratch
//! Files and a language server's locations
//! (definitions, usages; grouped by file under headers when they span
//! several, each usage's range marked). The app
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
use crate::config::FileIcons;
use crate::config::theme::Theme;
use crate::editor::breakpoint_glyph;
use crate::editor::buffer::{BpState, BreakpointSpec};
use crate::event::Mouse;
use crate::git::BranchInfo;
use crate::lsp::locations::LocationItem;
use crate::scratch::ScratchFile;
use crate::symbols::index::SymbolEntry;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;
use crate::ui::widgets::{file_rows, highlight, runs};

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
    /// The source `rows` were ranked for.
    source: Source,
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
    source: Source,
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
    /// View Breakpoints (`PaletteKind::Breakpoints`).
    Breakpoints,
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
            Source::Breakpoints => "Breakpoints",
        }
    }
}

/// A View Breakpoints row. The texts are made by the app; adapter text has been through `one_line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointItem {
    pub path: PathBuf,
    /// 0-based, the mark's key.
    pub line: usize,
    pub state: BpState,
    pub spec: BreakpointSpec,
    pub place: String,
    pub detail: String,
    pub verdict: String,
}

/// An exception filter row of View Breakpoints. `label` has been through `one_line` (the id when the adapter gave none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionFilterItem {
    pub adapter: String,
    pub filter: String,
    pub label: String,
    pub on: bool,
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
    /// Space or Enter on an exception filter row: the palette stays open; the app flips it.
    ToggleExceptionFilter {
        adapter: String,
        filter: String,
    },
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
    /// Open a file at a language server position (UTF-16 column).
    OpenFileAtUtf16 {
        path: PathBuf,
        pos: lsp_types::Position,
    },
    RunTask(Box<crate::run::RunTask>),
    /// Shift+Delete on a scratch file: the app asks first.
    DeleteScratch(PathBuf),
    /// Shift+Delete on a recent project: the palette has dropped the row
    /// and stays open; the app drops it from the recent projects file.
    ForgetProject(PathBuf),
    /// Shift+Delete on a breakpoint: the palette has dropped the row and stays open; the app removes the breakpoint.
    RemoveBreakpoint {
        path: PathBuf,
        line: usize,
    },
    /// Alt+Shift+F8 on a breakpoint: the palette closes and the app asks what to edit.
    EditBreakpoint {
        path: PathBuf,
        line: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteResult {
    Accept(Accept),
    Cancel,
}

const NEW_BRANCH: &str = "+ New Branch…";

pub struct Palette {
    pub kind: PaletteKind,
    /// `ui.file_icons`, copied by the app when the palette opens.
    pub file_icons: FileIcons,
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
    /// The locations are shown in the order given, without file headers.
    locations_flat: bool,
    /// The Locations rows shown: file headers (when the items span several
    /// files) and items, both indexing `locations`. `rows` index into this.
    loc_rows: Vec<LocRow>,
    /// Scratch files, newest first, their names (what is matched) and
    /// local modification dates.
    scratches: Vec<ScratchFile>,
    scratch_names: Vec<String>,
    scratch_dates: Vec<String>,
    /// View Breakpoints rows, paths then lines ascending.
    breakpoints: Vec<BreakpointItem>,
    /// View Breakpoints rows above the breakpoints: adapter name, then declared order.
    exception_filters: Vec<ExceptionFilterItem>,
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
            PaletteKind::RunTasks | PaletteKind::SelectRunConfig => vec![Source::Tasks],
            PaletteKind::Locations => vec![Source::Locations],
            PaletteKind::Scratches => vec![Source::Scratches],
            PaletteKind::Breakpoints => vec![Source::Breakpoints],
            PaletteKind::CompareWith => vec![Source::Files],
            // Go to Line stays a dialog and never opens a palette.
            PaletteKind::Branches | PaletteKind::GotoLine => vec![Source::Branches],
        };
        let first = tabs[0];
        Self {
            kind,
            file_icons: FileIcons::None,
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
            locations_flat: false,
            loc_rows: Vec::new(),
            scratches: Vec::new(),
            scratch_names: Vec::new(),
            scratch_dates: Vec::new(),
            breakpoints: Vec::new(),
            exception_filters: Vec::new(),
            matcher: Matcher::default(),
            bus: None,
            filter_seq: Arc::new(AtomicU64::new(0)),
            background_min: BACKGROUND_MIN,
            shown: Shown { files: Arc::new(Vec::new()), symbols: Arc::new(Vec::new()), query: String::new(), source: first },
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

    /// Like [`Palette::set_locations`], but the rows keep the order given and
    /// get no file headers (a history, not usages).
    pub fn set_locations_in_order(&mut self, title: String, items: Vec<LocationItem>) {
        self.locations_flat = true;
        self.set_locations(title, items);
    }

    /// The Locations item rows in display order, as `(path, line)`.
    #[cfg(test)]
    pub(crate) fn shown_locations(&self) -> Vec<(std::path::PathBuf, usize)> {
        self.loc_rows
            .iter()
            .filter_map(|r| match r {
                LocRow::Item(i) => Some((self.locations[*i].path.clone(), self.locations[*i].line)),
                LocRow::File { .. } => None,
            })
            .collect()
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

    /// The View Breakpoints rows; the selected index is kept.
    pub fn set_breakpoints(&mut self, items: Vec<BreakpointItem>) {
        self.breakpoints = items;
        if self.source() == Source::Breakpoints {
            self.refilter(false);
        }
    }

    /// The exception filter rows of View Breakpoints; the selected index is kept.
    pub fn set_exception_filters(&mut self, items: Vec<ExceptionFilterItem>) {
        self.exception_filters = items;
        if self.source() == Source::Breakpoints {
            self.refilter(false);
        }
    }

    /// The filter rows as set (tests outside this module).
    #[cfg(test)]
    pub(crate) fn exception_filter_items(&self) -> &[ExceptionFilterItem] {
        &self.exception_filters
    }

    /// The rows as set (tests outside this module).
    #[cfg(test)]
    pub(crate) fn breakpoint_items(&self) -> &[BreakpointItem] {
        &self.breakpoints
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
        let subset = if q.is_empty() { None } else { self.narrow_rows(&q) };
        let input = if background { self.background_input(&q, subset.as_ref().map(Vec::len)) } else { None };
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
                match rank_large(&query, &input, subset.as_deref(), &mut m, &cancelled) {
                    None => tracing::debug!("palette filter {seq} cancelled"),
                    Some(rows) => {
                        let from = subset.as_ref().map_or(n, Vec::len);
                        tracing::debug!("palette filter {seq}: {} of {n} (ranked {from}) in {:?}", rows.len(), t.elapsed());
                        if !cancelled() {
                            let _ = tx.send(crate::event::AppMsg::PaletteFiltered { generation, seq, rows });
                        }
                    }
                }
            });
            self.pending = Some(Pending {
                seq,
                reset,
                moved,
                files: self.files.clone(),
                symbols: self.symbols.clone(),
                query: q,
                source: self.source(),
            });
            return;
        }
        self.rows = self.rank_now(&q, subset);
        self.shown = Shown { files: self.files.clone(), symbols: self.symbols.clone(), query: q, source: self.source() };
        self.list.set_len(self.rows.len());
        if reset {
            // Under a file header, start on its first item.
            let skip_header =
                self.source() == Source::Locations && matches!(self.loc_rows.first(), Some(LocRow::File { .. })) && self.rows.len() > 1;
            self.list.select(usize::from(skip_header));
            self.list.offset = 0;
        }
    }

    /// Build `loc_rows` from the matched item indices: flat when the items
    /// span fewer than two files or were set in order
    /// ([`Palette::set_locations_in_order`]), else in file order under a
    /// header per file.
    fn group_locations(&mut self, mut matched: Vec<usize>) {
        let first = self.locations.first().map(|l| &l.path);
        let grouped = !self.locations_flat && first.is_some_and(|f| self.locations.iter().any(|l| &l.path != f));
        if !grouped {
            self.loc_rows = matched.into_iter().map(LocRow::Item).collect();
            return;
        }
        matched.sort_unstable();
        let mut out = Vec::with_capacity(matched.len() * 2);
        let mut k = 0;
        while k < matched.len() {
            let path = &self.locations[matched[k]].path;
            let count = matched[k..].iter().take_while(|&&i| &self.locations[i].path == path).count();
            out.push(LocRow::File { first: matched[k], count });
            out.extend(matched[k..k + count].iter().map(|&i| LocRow::Item(i)));
            k += count;
        }
        self.loc_rows = out;
    }

    /// The candidates to rank off the main thread for `q`, if it should be.
    /// `narrowed` is the number of rows to rank when narrowing a shown filter.
    fn background_input(&self, q: &str, narrowed: Option<usize>) -> Option<FilterInput> {
        if q.is_empty() || self.bus.is_none() || tokio::runtime::Handle::try_current().is_err() {
            return None;
        }
        match self.source() {
            Source::Files if narrowed.unwrap_or(self.files.len()) >= self.background_min => Some(FilterInput::Paths(self.files.clone())),
            Source::Symbols if narrowed.unwrap_or(self.symbols.len()) >= self.background_min => {
                Some(FilterInput::Symbols(self.symbols.clone()))
            }
            _ => None,
        }
    }

    /// The shown rows to rank for `q` instead of every candidate: the shown
    /// rows are the complete match set of the shown query, so when `q`
    /// extends it (see [`narrowable`]) over the same candidates and source,
    /// nothing outside them can match. `None` means rank everything.
    fn narrow_rows(&self, q: &str) -> Option<Vec<usize>> {
        let same = match self.source() {
            Source::Files => Arc::ptr_eq(&self.shown.files, &self.files),
            Source::Symbols => Arc::ptr_eq(&self.shown.symbols, &self.symbols),
            _ => return None,
        };
        (same && self.shown.source == self.source() && narrowable(&self.shown.query, q)).then(|| self.rows.clone())
    }

    /// The current source ranked against `q` on this thread.
    fn rank_now(&mut self, q: &str, subset: Option<Vec<usize>>) -> Vec<usize> {
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
                    rank_large(q, &input, subset.as_deref(), &mut self.matcher, &|| false).unwrap_or_default()
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
            Source::Breakpoints => {
                let nf = self.exception_filters.len();
                if q.is_empty() {
                    (0..nf + self.breakpoints.len()).collect()
                } else {
                    // Filter rows stay above the breakpoint rows.
                    let filters = self.exception_filters.iter().map(|f| [f.label.as_str(), f.adapter.as_str()]);
                    let mut rows = rank_texts(q, filters, &mut self.matcher);
                    let texts = self.breakpoints.iter().map(|b| [b.place.as_str(), b.detail.as_str()]);
                    rows.extend(rank_texts(q, texts, &mut self.matcher).into_iter().map(|i| i + nf));
                    rows
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
                    rank_large(q, &input, subset.as_deref(), &mut self.matcher, &|| false).unwrap_or_default()
                }
            }
            Source::Tasks => {
                if q.is_empty() {
                    (0..self.tasks.len()).collect()
                } else {
                    let commands: Vec<String> = self.tasks.iter().map(|t| t.display()).collect();
                    let texts = self.tasks.iter().zip(&commands).map(|(t, c)| [t.name.as_str(), c.as_str()]);
                    rank_texts(q, texts, &mut self.matcher)
                }
            }
            Source::Locations => {
                let matched = if q.is_empty() {
                    (0..self.locations.len()).collect()
                } else {
                    let paths: Vec<String> = self.locations.iter().map(|l| display_path(&l.path, &self.root)).collect();
                    let texts = self.locations.iter().zip(&paths).map(|(l, p)| [l.text.as_str(), p.as_str()]);
                    rank_texts(q, texts, &mut self.matcher)
                };
                self.group_locations(matched);
                (0..self.loc_rows.len()).collect()
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
        let subset = if p.query.is_empty() { None } else { self.narrow_rows(&p.query) };
        let rows = self.rank_now(&p.query, subset);
        self.show_rows(p, rows);
    }

    /// Show `rows`, ranked by pending filter `p`. The selected row stays
    /// selected (by what it shows) if the query is unchanged or the user
    /// picked it meanwhile; a new query selects the top row.
    fn show_rows(&mut self, p: Pending, rows: Vec<usize>) {
        let key = if p.moved || !p.reset { self.selected_key() } else { None };
        self.rows = rows;
        self.shown = Shown { files: p.files, symbols: p.symbols, query: p.query, source: p.source };
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
                let path = self.root.join(&*e.path);
                if e.utf16 {
                    Accept::OpenFileAtUtf16 { path, pos: lsp_types::Position::new(e.symbol.line as u32, e.symbol.col as u32) }
                } else {
                    Accept::OpenFileAt { path, line: e.symbol.line, col: e.symbol.col }
                }
            }
            Source::Actions => Accept::Action(self.actions.get(i)?.action.clone()),
            Source::Tasks => Accept::RunTask(Box::new(self.tasks.get(i)?.clone())),
            Source::Scratches => Accept::OpenFile(self.scratches.get(i)?.path.clone()),
            Source::Breakpoints if i < self.exception_filters.len() => {
                let f = &self.exception_filters[i];
                Accept::ToggleExceptionFilter { adapter: f.adapter.clone(), filter: f.filter.clone() }
            }
            Source::Breakpoints => {
                let b = self.breakpoints.get(i - self.exception_filters.len())?;
                Accept::OpenFileAt { path: b.path.clone(), line: b.line, col: 0 }
            }
            Source::Locations => {
                let at = match self.loc_rows.get(i)? {
                    LocRow::File { first, .. } => *first,
                    LocRow::Item(k) => *k,
                };
                let l = self.locations.get(at)?;
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
            // Dropped at once, like a project; the app removes the breakpoint.
            PaletteAction::DeleteItem if self.source() == Source::Breakpoints => {
                let i = self.rows.get(self.list.selected)?.checked_sub(self.exception_filters.len())?;
                let b = self.breakpoints.remove(i);
                self.refilter(false);
                return Some(PaletteResult::Accept(Accept::RemoveBreakpoint { path: b.path, line: b.line }));
            }
            PaletteAction::EditItem if self.source() == Source::Breakpoints => {
                let nf = self.exception_filters.len();
                let b = self.rows.get(self.list.selected).and_then(|&i| i.checked_sub(nf)).and_then(|i| self.breakpoints.get(i))?;
                return Some(PaletteResult::Accept(Accept::EditBreakpoint { path: b.path.clone(), line: b.line }));
            }
            PaletteAction::EditItem => {}
            // A filter row toggles with an empty or blank query; every other case types the space the key stands for.
            PaletteAction::ToggleItem => {
                let on_filter = self.source() == Source::Breakpoints
                    && self.input.text.trim().is_empty()
                    && self.rows.get(self.list.selected).is_some_and(|&i| i < self.exception_filters.len());
                if on_filter {
                    return self.accept(self.list.selected);
                }
                self.input.insert(' ');
                self.refilter(true);
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

    /// The palette's area as last drawn.
    pub fn rect(&self) -> Rect {
        self.outer
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
            PaletteKind::Files => "Go to File",
            PaletteKind::Actions => "Find Action",
            PaletteKind::Everywhere => "Search Everywhere",
            PaletteKind::RecentFiles => "Recent Files",
            PaletteKind::RecentProjects => "Recent Projects",
            PaletteKind::Symbols => "Go to Symbol",
            PaletteKind::RunTasks => "Run",
            PaletteKind::SelectRunConfig => "Select Run Configuration",
            PaletteKind::Scratches => "Scratch Files",
            PaletteKind::Breakpoints => "Breakpoints",
            PaletteKind::CompareWith => "Compare With",
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
            _ => false,
        };
        let count = match self.source() {
            Source::Files if loading && self.files.is_empty() => "indexing…".to_string(),
            Source::Symbols if loading && self.symbols.is_empty() => "indexing…".to_string(),
            _ if loading => format!("{} (indexing…)", self.rows.len()),
            Source::Locations => format!("{}", self.loc_rows.iter().filter(|r| matches!(r, LocRow::Item(_))).count()),
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
            Source::Breakpoints => "filter · Space toggles · Shift+Del removes · Alt+Shift+F8 edits",
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
                Source::Tasks if self.input.text.trim().is_empty() => {
                    "No run configurations: Run > Edit Configurations… or [[run.configs]] in config.toml"
                }
                Source::Scratches if self.input.text.trim().is_empty() => "No scratch files: File > New Scratch File…",
                Source::Breakpoints if self.input.text.trim().is_empty() => "No breakpoints: Ctrl+F8 sets one on the caret's line",
                _ => "Nothing found",
            };
            frame.buffer_mut().set_stringn(lrect.x + 2, lrect.y, msg, lrect.width.saturating_sub(2) as usize, theme.dim.bg(theme.popup_bg));
            return;
        }
        // The query the rows on screen were ranked for.
        let pattern = Pattern::parse(&self.shown.query, CaseMatching::Smart, Normalization::Smart);
        let icons = self.file_icons;
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
                    if self.source() != Source::Projects
                        && let Some(icon) = crate::ui::file_icons::span(&name, false, icons, base, theme)
                    {
                        v.insert(0, icon);
                    }
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
                    let command = t.display();
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
                Source::Breakpoints if i < self.exception_filters.len() => {
                    let f = &self.exception_filters[i];
                    let marks = match_indices(&pattern, &f.label, &mut self.matcher);
                    let box_ = if f.on { "[x] " } else { "[ ] " };
                    // The adapter on the right when it fits with two spaces before it; else the label gets the whole row.
                    let tag = f.adapter.width() + 2;
                    let (label, tag) = if f.label.width() + box_.width() + tag <= width {
                        (f.label.clone(), true)
                    } else if width > box_.width() + tag + 4 {
                        (crate::util::text::fit(&f.label, width - box_.width() - tag), true)
                    } else {
                        (crate::util::text::fit(&f.label, width.saturating_sub(box_.width())), false)
                    };
                    let mut v = vec![(box_.to_string(), base)];
                    v.extend(highlight(&label, |ci| marks.contains(&(ci as u32)), base, hl));
                    let used: usize = v.iter().map(|(t, _)| t.width()).sum();
                    if tag {
                        v.push((" ".repeat(width - used - f.adapter.width()), base));
                        v.push((f.adapter.clone(), dim));
                    }
                    v
                }
                Source::Breakpoints => {
                    let b = &self.breakpoints[i - self.exception_filters.len()];
                    // Glyph, place, detail; the verdict right-aligned, at most half the row.
                    let (glyph, gstyle) = breakpoint_glyph(b.state, &b.spec, theme);
                    let gstyle = if selected { base } else { bg.patch(gstyle) };
                    let marks = match_indices(&pattern, &b.place, &mut self.matcher);
                    let verdict = crate::util::text::fit(&b.verdict, width / 2);
                    let mut v = vec![(glyph.to_string(), gstyle), (" ".to_string(), base)];
                    v.extend(highlight(&b.place, |ci| marks.contains(&(ci as u32)), base, hl));
                    let used = 2 + b.place.width();
                    if !b.detail.is_empty() {
                        let room = width.saturating_sub(used + 2 + verdict.width() + 2);
                        if room > 0 {
                            v.push(("  ".into(), base));
                            v.push((crate::util::text::fit(&b.detail, room), dim));
                        }
                    }
                    let used: usize = v.iter().map(|(t, _)| t.width()).sum();
                    if !verdict.is_empty() && used + verdict.width() + 2 <= width {
                        v.push((" ".repeat(width - used - verdict.width()), base));
                        v.push((verdict, dim));
                    }
                    v
                }
                Source::Locations => {
                    let k = match self.loc_rows[i] {
                        LocRow::File { first, count } => {
                            let r = Rect::new(lrect.x, ry, lrect.width, 1);
                            file_rows::file_header(
                                frame.buffer_mut(),
                                r,
                                &self.root,
                                &self.locations[first].path,
                                &count.to_string(),
                                base,
                                theme,
                                icons,
                            );
                            continue;
                        }
                        LocRow::Item(k) => k,
                    };
                    let l = &self.locations[k];
                    let grouped = self.loc_rows.iter().any(|r| matches!(r, LocRow::File { .. }));
                    let marks = match_indices(&pattern, &l.text, &mut self.matcher);
                    let usage = l.mark.clone().unwrap_or(0..0);
                    let style_of = |ci: usize| {
                        let st = if marks.contains(&(ci as u32)) { hl } else { base };
                        if usage.contains(&ci) { st.bg(theme.search_match_bg) } else { st }
                    };
                    if grouped {
                        // The line number, then the text.
                        let num = format!("{:>4}  ", l.line + 1);
                        let text = crate::util::text::fit(&l.text, width.saturating_sub(num.width()));
                        let mut v = vec![(num, base.patch(theme.line_number))];
                        v.extend(runs(&text, style_of));
                        v
                    } else {
                        // The text, cut to leave room for the place on the right.
                        let place = format!("{}:{}", display_path(&l.path, &self.root), l.line + 1);
                        let text = crate::util::text::fit(&l.text, width.saturating_sub(place.width() + 2));
                        let mut v = runs(&text, style_of);
                        let used = text.width();
                        v.push((" ".repeat(width.saturating_sub(used + place.width()).max(2)), base));
                        v.push((place, dim));
                        v
                    }
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

/// A Locations row: a file header (its first item and item count) or an
/// item; indices into `Palette::locations`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocRow {
    File { first: usize, count: usize },
    Item(usize),
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
    rank_paths_until(query, paths, None, matcher, &|| false).unwrap_or_default()
}

/// [`rank_paths`] that checks `cancelled` every [`CANCEL_EVERY`] scored paths
/// and before the sort, and gives up with `None` once it holds. `subset`
/// limits the ranking to those indices of `paths`.
fn rank_paths_until(
    query: &str,
    paths: &[String],
    subset: Option<&[usize]>,
    matcher: &mut Matcher,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<usize>> {
    matcher.config = nucleo_matcher::Config::DEFAULT.match_paths();
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = Vec::new();
    let indices: Box<dyn Iterator<Item = usize> + '_> = match subset {
        Some(s) => Box::new(s.iter().copied()),
        None => Box::new(0..paths.len()),
    };
    for (k, i) in indices.enumerate() {
        let p = &paths[i];
        if k % CANCEL_EVERY == 0 && k > 0 && cancelled() {
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
    rank_texts_until(query, items.enumerate(), matcher, &|| false).unwrap_or_default()
}

/// [`rank_texts`] that checks `cancelled` every [`CANCEL_EVERY`] candidates
/// and before the sort, and gives up with `None` once it holds. Each item
/// carries its original index, which is returned and breaks ties.
fn rank_texts_until<'a, const N: usize>(
    query: &str,
    items: impl Iterator<Item = (usize, [&'a str; N])>,
    matcher: &mut Matcher,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<usize>> {
    matcher.config = nucleo_matcher::Config::DEFAULT;
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = Vec::new();
    for (k, (i, texts)) in items.enumerate() {
        if k % CANCEL_EVERY == 0 && k > 0 && cancelled() {
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
/// same rows. `subset` limits it to those candidate indices. `None` when
/// `cancelled` turned true.
fn rank_large(
    query: &str,
    input: &FilterInput,
    subset: Option<&[usize]>,
    matcher: &mut Matcher,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<usize>> {
    match input {
        FilterInput::Paths(paths) => rank_paths_until(query, paths, subset, matcher, cancelled),
        FilterInput::Symbols(symbols) => match subset {
            Some(s) => rank_texts_until(query, s.iter().map(|&i| (i, [&*symbols[i].symbol.name])), matcher, cancelled),
            None => rank_texts_until(query, symbols.iter().enumerate().map(|(i, e)| (i, [&*e.symbol.name])), matcher, cancelled),
        },
    }
}

/// Whether the rows matching `prev` hold every row matching `q`, so `q` can
/// be ranked over them alone. Plain fuzzy atoms, added atoms, smart case and
/// smart normalization keep the subset property; nucleo pattern syntax does
/// not (`!a` to `!ab` matches more, and `foo$` to `foo$x`, `'` and `^` atoms
/// change meaning), so any such character in `q` rules narrowing out.
fn narrowable(prev: &str, q: &str) -> bool {
    !prev.is_empty() && q.len() > prev.len() && q.starts_with(prev) && !q.contains(['!', '\\', '^', '$', '\''])
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

    #[test]
    fn locations_in_order_keep_the_order_without_headers() {
        let item = |f: &str, line| LocationItem::at_line(PathBuf::from(format!("/w/{f}")), line, 0, "x");
        let items = || vec![item("a.rs", 1), item("b.rs", 2), item("a.rs", 3)];
        let mut p = Palette::new(PaletteKind::Locations, PathBuf::from("/w"));
        p.set_locations_in_order("Recent".into(), items());
        let want = vec![(PathBuf::from("/w/a.rs"), 1), (PathBuf::from("/w/b.rs"), 2), (PathBuf::from("/w/a.rs"), 3)];
        assert_eq!(p.shown_locations(), want);
        assert_eq!(p.loc_rows.len(), 3, "no file headers");
        let mut g = Palette::new(PaletteKind::Locations, PathBuf::from("/w"));
        // Grouping assumes items sorted by path (`App::show_locations`).
        g.set_locations("Usages".into(), vec![item("a.rs", 1), item("a.rs", 3), item("b.rs", 2)]);
        assert_eq!(g.loc_rows.iter().filter(|r| matches!(r, LocRow::File { .. })).count(), 2);
    }

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

    fn item(path: &str, line: usize, detail: &str) -> BreakpointItem {
        let spec = BreakpointSpec { condition: (!detail.is_empty()).then(|| detail.to_string()), log_message: None };
        BreakpointItem {
            path: PathBuf::from("/w").join(path),
            line,
            state: BpState::Idle,
            spec,
            place: format!("{path}:{}", line + 1),
            detail: if detail.is_empty() { String::new() } else { format!("if {detail}") },
            verdict: String::new(),
        }
    }

    #[test]
    fn breakpoints_filter_accept_remove_and_edit() {
        let mut p = Palette::new(PaletteKind::Breakpoints, PathBuf::from("/w"));
        p.set_breakpoints(vec![item("loop.c", 4, "i == 3"), item("loop.c", 6, ""), item("demo.c", 2, "count > 100")]);
        assert_eq!(p.rows_selected(), (3, 0));
        p.set_query("loop");
        assert_eq!(p.rows_selected().0, 2);
        p.set_query("count");
        assert_eq!(p.rows_selected().0, 1, "a condition's text finds its row");
        assert_eq!(
            p.handle_action(&PaletteAction::Accept),
            Some(PaletteResult::Accept(Accept::OpenFileAt { path: PathBuf::from("/w/demo.c"), line: 2, col: 0 }))
        );
        // The selected row, not the first.
        p.set_query("");
        p.handle_action(&PaletteAction::Down);
        let at = |path: &str, line| Some(PaletteResult::Accept(Accept::EditBreakpoint { path: PathBuf::from(path), line }));
        assert_eq!(p.handle_action(&PaletteAction::EditItem), at("/w/loop.c", 6));
        assert_eq!(p.rows_selected(), (3, 1), "editing drops nothing");
        let removed = Some(PaletteResult::Accept(Accept::RemoveBreakpoint { path: PathBuf::from("/w/loop.c"), line: 6 }));
        assert_eq!(p.handle_action(&PaletteAction::DeleteItem), removed);
        assert_eq!(p.rows_selected(), (2, 1));
        assert_eq!(p.breakpoint_items().len(), 2);
        // Nothing shown: Shift+Delete and Alt+Shift+F8 do nothing, as Shift+Delete on Recent Projects.
        p.set_query("zzz");
        assert_eq!(p.rows_selected().0, 0);
        assert_eq!(p.handle_action(&PaletteAction::DeleteItem), None);
        assert_eq!(p.handle_action(&PaletteAction::EditItem), None);
        assert_eq!(p.breakpoint_items().len(), 2);
    }

    fn filter_item(filter: &str, label: &str, on: bool) -> ExceptionFilterItem {
        ExceptionFilterItem { adapter: "gdb".into(), filter: filter.into(), label: label.into(), on }
    }

    fn toggle_of(filter: &str) -> Option<PaletteResult> {
        Some(PaletteResult::Accept(Accept::ToggleExceptionFilter { adapter: "gdb".into(), filter: filter.into() }))
    }

    #[test]
    fn filter_rows_sit_on_top_and_toggle() {
        let mut p = Palette::new(PaletteKind::Breakpoints, PathBuf::from("/w"));
        p.set_exception_filters(vec![
            filter_item("throw", "C++ exceptions, when thrown", true),
            filter_item("catch", "C++ exceptions, when caught", false),
        ]);
        p.set_breakpoints(vec![item("throw.c", 4, ""), item("loop.c", 6, "")]);
        assert_eq!(p.rows, vec![0, 1, 2, 3], "filter, filter, breakpoint, breakpoint");
        assert_eq!(p.handle_action(&PaletteAction::ToggleItem), toggle_of("throw"));
        assert_eq!(p.input.text, "");
        p.handle_action(&PaletteAction::Down);
        assert_eq!(p.handle_action(&PaletteAction::ToggleItem), toggle_of("catch"));
        // On a breakpoint row the key types a space.
        p.handle_action(&PaletteAction::Down);
        assert_eq!(p.handle_action(&PaletteAction::ToggleItem), None);
        assert_eq!(p.input.text, " ");
        // A blank query ranks every row, so a filter row still toggles under it.
        assert_eq!(p.rows, vec![0, 1, 2, 3], "the space leaves every row");
        assert_eq!(p.rows_selected().1, 0, "the list is back on its first row");
        assert_eq!(p.handle_action(&PaletteAction::ToggleItem), toggle_of("throw"));
        assert_eq!(p.input.text, " ");
        // With a query it types too, and Enter toggles.
        p.set_query("caught");
        assert_eq!(p.rows.first(), Some(&1), "the catch row first");
        assert_eq!(p.handle_action(&PaletteAction::ToggleItem), None);
        assert_eq!(p.input.text, "caught ");
        p.set_query("caught");
        assert_eq!(p.handle_action(&PaletteAction::Accept), toggle_of("catch"));
        // A label and a place both match: the filter row stays first.
        p.set_query("throw");
        assert_eq!(p.rows, vec![0, 2], "the throw filter, then throw.c");
        // Shift+Delete and Alt+Shift+F8 leave a filter row alone.
        p.set_query("");
        let before = p.rows.clone();
        assert_eq!(p.handle_action(&PaletteAction::DeleteItem), None);
        assert_eq!(p.handle_action(&PaletteAction::EditItem), None);
        assert_eq!((p.rows.clone(), p.input.text.as_str(), p.breakpoint_items().len()), (before, "", 2));
        // A breakpoint row below the filters is still the right breakpoint.
        p.handle_action(&PaletteAction::Down);
        p.handle_action(&PaletteAction::Down);
        p.handle_action(&PaletteAction::Down);
        let removed = Some(PaletteResult::Accept(Accept::RemoveBreakpoint { path: PathBuf::from("/w/loop.c"), line: 6 }));
        assert_eq!(p.handle_action(&PaletteAction::DeleteItem), removed);
    }

    #[test]
    fn toggle_item_types_a_space_elsewhere() {
        let mut p = Palette::new(PaletteKind::Actions, PathBuf::from("/w"));
        p.set_query("ab");
        assert_eq!(p.handle_action(&PaletteAction::ToggleItem), None);
        assert_eq!(p.input.text, "ab ");
    }

    #[test]
    fn edit_item_does_nothing_elsewhere() {
        let mut p = Palette::new(PaletteKind::Actions, PathBuf::from("/w"));
        p.set_query("ab");
        assert_eq!(p.handle_action(&PaletteAction::EditItem), None);
        assert_eq!(p.input.text, "ab");
        let mut p = Palette::new(PaletteKind::Scratches, PathBuf::from("/w"));
        p.set_scratches(vec![ScratchFile { path: PathBuf::from("/s/a.txt"), name: "a.txt".into(), language: "Plain text", modified: 0 }]);
        assert_eq!(p.handle_action(&PaletteAction::EditItem), None);
        assert_eq!(p.rows_selected().0, 1);
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
        assert_eq!(rank_paths_until("src", &files, None, &mut m, &|| true), None);
        assert_eq!(rank_paths_until("src", &files, None, &mut m, &|| false), Some(rank_paths("src", &files, &mut m)));
        let texts = || files.iter().enumerate().map(|(i, f)| (i, [f.as_str()]));
        assert_eq!(rank_texts_until("src", texts(), &mut m, &|| true), None);
        assert_eq!(
            rank_texts_until("src", texts(), &mut m, &|| false),
            Some(rank_texts("src", files.iter().map(|f| [f.as_str()]), &mut m))
        );
        // A long list stops part way, at the first check.
        let many: Vec<String> = (0..10 * CANCEL_EVERY).map(|i| format!("f{i}")).collect();
        let checks = std::cell::Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            true
        };
        assert_eq!(rank_paths_until("f", &many, None, &mut m, &cancelled), None);
        assert_eq!(checks.get(), 1);
    }

    #[test]
    fn narrowable_rejects_pattern_syntax() {
        assert!(narrowable("ab", "abc"));
        assert!(narrowable("a", "a b"));
        assert!(narrowable("ab", "abC"));
        assert!(!narrowable("ab", "ab"));
        assert!(!narrowable("abc", "ab"));
        assert!(!narrowable("ab", "xabc"));
        assert!(!narrowable("", "a"));
        for c in ['!', '\\', '^', '$', '\''] {
            assert!(!narrowable("ab", &format!("ab{c}")), "{c}");
            assert!(!narrowable("ab", &format!("ab{c}c")), "{c}");
        }
    }

    fn generated_paths() -> Vec<String> {
        let mut v: Vec<String> = (0..3000)
            .map(|i| {
                let ext = if i % 3 == 0 { "rs" } else { "toml" };
                let case = if i % 5 == 0 { "Mod" } else { "mod" };
                format!("CRATE{}/src/{case}{}/sub-{}/file_{i}.{ext}", i % 4, i % 11, i % 7)
            })
            .collect();
        v.extend(["src/café.rs", "src/cafe.rs", "src/app/mod.rs", "src/app.rs"].map(String::from));
        v.sort();
        v
    }

    #[test]
    fn narrowed_rows_equal_a_full_rank() {
        let paths = generated_paths();
        let files = Arc::new(paths.clone());
        for typed in ["src/app", "Mod1 fi", "file_12 rs", "cafe", "CRATE3/sub"] {
            let mut p = Palette::new(PaletteKind::Files, PathBuf::from("/w"));
            p.set_files(files.clone(), false);
            let mut prefix = String::new();
            for c in typed.chars() {
                prefix.push(c);
                p.set_query(&prefix);
                let full = rank_paths(prefix.trim(), &paths, &mut Matcher::default());
                assert_eq!(p.rows, full, "query {prefix:?}");
            }
        }
    }

    #[test]
    fn narrowing_skips_negation() {
        let paths: Vec<String> = ["xa", "ab", "b"].map(String::from).to_vec();
        let mut p = Palette::new(PaletteKind::Files, PathBuf::from("/w"));
        p.set_files(Arc::new(paths.clone()), false);
        p.set_query("!a");
        p.set_query("!ab");
        let full = rank_paths("!ab", &paths, &mut Matcher::default());
        assert!(full.contains(&0));
        assert_eq!(p.rows, full);
    }

    fn symbol_entry(name: &str, line: usize) -> SymbolEntry {
        SymbolEntry {
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
            utf16: false,
        }
    }

    #[test]
    fn narrowed_symbols_equal_a_full_rank() {
        let symbols: Vec<SymbolEntry> =
            (0..500).map(|i| symbol_entry(&format!("{}handle_{}_{i}", ["Do", "do", "run_"][i % 3], i % 13), i)).collect();
        let symbols = Arc::new(symbols);
        let mut p = Palette::new(PaletteKind::Symbols, PathBuf::from("/w"));
        p.set_symbols(symbols.clone(), false);
        let mut prefix = String::new();
        for c in "handle_1 4".chars() {
            prefix.push(c);
            p.set_query(&prefix);
            let full = rank_texts(prefix.trim(), symbols.iter().map(|e| [&*e.symbol.name]), &mut Matcher::default());
            assert_eq!(p.rows, full, "query {prefix:?}");
        }
    }

    #[tokio::test]
    async fn background_narrowing_equals_a_full_rank() {
        let (mut p, mut rx) = bus_palette(PaletteKind::Files);
        p.set_files(Arc::new(fixture()), false);
        p.set_query("s");
        let (seq, rows) = next_filter(&mut rx).await;
        assert!(p.apply_filter(seq, rows));
        p.set_query("sr");
        assert!(p.filter_pending());
        let (seq, rows) = next_filter(&mut rx).await;
        assert!(p.apply_filter(seq, rows));
        assert_eq!(p.rows, rank_paths("sr", &fixture(), &mut Matcher::default()));
    }

    #[tokio::test]
    async fn narrowed_subset_below_threshold_ranks_at_once() {
        let paths: Vec<String> = (0..100)
            .map(|i| match i {
                0..=9 => format!("xq{i:03}"),
                10..=59 => format!("x{i:03}"),
                _ => format!("y{i:03}"),
            })
            .collect();
        let (mut p, mut rx) = bus_palette(PaletteKind::Files);
        p.set_background_min(70);
        p.set_files(Arc::new(paths.clone()), false);
        p.set_query("x");
        assert!(p.filter_pending());
        let (seq, rows) = next_filter(&mut rx).await;
        assert!(p.apply_filter(seq, rows));
        assert_eq!(p.rows.len(), 60);
        p.set_query("xq");
        assert!(!p.filter_pending());
        assert_eq!(p.rows, rank_paths("xq", &paths, &mut Matcher::default()));
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
            utf16: false,
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
        // Typing a query char by char: each keystroke narrows the last.
        let mut p = Palette::new(PaletteKind::Files, PathBuf::from("/w"));
        p.set_files(paths.clone(), false);
        let (mut prefix, mut total, mut worst) = (String::new(), Duration::ZERO, Duration::ZERO);
        for c in "file_1234".chars() {
            prefix.push(c);
            let t = Instant::now();
            p.set_query(&prefix);
            let d = t.elapsed();
            total += d;
            worst = worst.max(d);
        }
        eprintln!("file_1234 typed char by char: total {:.2} ms, worst keystroke {:.2} ms", ms(total), ms(worst));
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

    #[test]
    fn compare_with_lists_files_under_its_own_title() {
        let mut p = Palette::new(PaletteKind::CompareWith, PathBuf::from("/w"));
        assert_eq!(p.title(), "Compare With");
        assert!(p.wants(Source::Files));
        p.set_files(Arc::new(vec!["a.txt".to_string()]), false);
        let res = p.handle_action(&PaletteAction::Accept);
        assert_eq!(res, Some(PaletteResult::Accept(Accept::OpenFile(PathBuf::from("/w/a.txt")))));
    }
}
