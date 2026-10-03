//! Command palette overlay: Go to File, Go to Symbol, Find Action, Search
//! Everywhere, Recent Files, Branches, Recent Projects, Run tasks and a
//! language server's locations (definitions, usages). The app owns it (`App.palette`) and routes all
//! input to it while it is open, like a dialog. Keys arrive as
//! `PaletteAction`s from the `palette` keymap context; unmapped keys edit the
//! query. The palette holds copies of its candidates (the file index, the
//! action list, branches, recent files and projects, symbols) that the app refreshes
//! as they load.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
use crate::symbols::index::SymbolEntry;
use crate::ui::widgets::list::ListState;
use crate::ui::widgets::text_input::TextInput;

/// Upper bound for the cached file walk, so a huge directory cannot stall
/// matching.
pub const MAX_FILES: usize = 200_000;

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
    matcher: Matcher,
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
            matcher: Matcher::default(),
            outer: Rect::default(),
            list_rect: Rect::default(),
            tab_rects: Vec::new(),
        }
    }

    pub fn source(&self) -> Source {
        self.tabs[self.tab]
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
    /// selection to the top (the query changed); otherwise it is kept.
    fn refilter(&mut self, reset: bool) {
        let q = self.input.text.trim().to_string();
        self.rows = match self.source() {
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
                    rank_paths(&q, &self.files, &mut self.matcher)
                }
            }
            Source::Recent => {
                let shown: Vec<String> = self.recent.iter().map(|p| display_path(p, &self.root)).collect();
                if q.is_empty() { (0..shown.len()).collect() } else { rank_paths(&q, &shown, &mut self.matcher) }
            }
            Source::Projects => {
                let shown = &self.projects_shown;
                if q.is_empty() { (0..shown.len()).collect() } else { rank_paths(&q, shown, &mut self.matcher) }
            }
            Source::Actions => {
                if q.is_empty() {
                    (0..self.actions.len()).collect()
                } else {
                    let texts = self.actions.iter().map(|a| [a.desc, a.id]);
                    rank_texts(&q, texts, &mut self.matcher)
                }
            }
            Source::Symbols => {
                if q.is_empty() {
                    (0..self.symbols.len()).collect()
                } else {
                    rank_texts(&q, self.symbols.iter().map(|e| [&*e.symbol.name]), &mut self.matcher)
                }
            }
            Source::Tasks => {
                if q.is_empty() {
                    (0..self.tasks.len()).collect()
                } else {
                    let commands: Vec<String> = self.tasks.iter().map(|t| t.command.display()).collect();
                    let texts = self.tasks.iter().zip(&commands).map(|(t, c)| [t.name.as_str(), c.as_str()]);
                    rank_texts(&q, texts, &mut self.matcher)
                }
            }
            Source::Locations => {
                if q.is_empty() {
                    (0..self.locations.len()).collect()
                } else {
                    let paths: Vec<String> = self.locations.iter().map(|l| display_path(&l.path, &self.root)).collect();
                    let texts = self.locations.iter().zip(&paths).map(|(l, p)| [l.text.as_str(), p.as_str()]);
                    rank_texts(&q, texts, &mut self.matcher)
                }
            }
            Source::Branches => {
                let mut texts: Vec<[&str; 1]> = vec![["New Branch"]];
                texts.extend(self.branches.iter().map(|b| [b.name.as_str()]));
                if q.is_empty() { (0..texts.len()).collect() } else { rank_texts(&q, texts.into_iter(), &mut self.matcher) }
            }
        };
        self.list.set_len(self.rows.len());
        if reset {
            self.list.select(0);
            self.list.offset = 0;
        }
    }

    fn switch_tab(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.tab {
            self.tab = i;
            self.refilter(true);
        }
    }

    fn accept(&self, row: usize) -> Option<PaletteResult> {
        let i = *self.rows.get(row)?;
        let a = match self.source() {
            Source::Files => Accept::OpenFile(self.root.join(self.files.get(i)?)),
            Source::Recent => Accept::OpenFile(self.recent.get(i)?.clone()),
            Source::Projects => Accept::SwitchRoot(self.projects.get(i)?.clone()),
            Source::Symbols => {
                let e = self.symbols.get(i)?;
                Accept::OpenFileAt { path: self.root.join(&*e.path), line: e.symbol.line, col: e.symbol.col }
            }
            Source::Actions => Accept::Action(self.actions.get(i)?.action.clone()),
            Source::Tasks => Accept::RunTask(Box::new(self.tasks.get(i)?.clone())),
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
            PaletteAction::Up => self.list.up(1),
            PaletteAction::Down => self.list.down(1),
            PaletteAction::PageUp => self.list.up(page),
            PaletteAction::PageDown => self.list.down(page),
            PaletteAction::Accept => return self.accept(self.list.selected),
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
        let loading = match self.source() {
            Source::Files => self.files_loading,
            Source::Symbols => self.symbols_loading,
            Source::Tasks => self.tasks_loading,
            _ => false,
        };
        let count = match self.source() {
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
            Source::Projects => "type a project name",
            Source::Symbols => "type a symbol name",
            Source::Tasks => "type a task name",
            Source::Locations => "type to filter",
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
                _ => "Nothing found",
            };
            frame.buffer_mut().set_stringn(lrect.x + 2, lrect.y, msg, lrect.width.saturating_sub(2) as usize, theme.dim.bg(theme.popup_bg));
            return;
        }
        let pattern = Pattern::parse(self.input.text.trim(), CaseMatching::Smart, Normalization::Smart);
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
                        Source::Files => self.files[i].clone(),
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
                    let e = &self.symbols[i];
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
    matcher.config = nucleo_matcher::Config::DEFAULT.match_paths();
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let Some(full) = pattern.score(Utf32Str::new(p, &mut buf), matcher) else { continue };
        let name = p.rsplit('/').next().unwrap_or(p);
        let name_score = pattern.score(Utf32Str::new(name, &mut buf), matcher).unwrap_or(0);
        scored.push((full + name_score, i));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| paths[a.1].len().cmp(&paths[b.1].len())).then_with(|| paths[a.1].cmp(&paths[b.1])));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Indices of candidates (each with one or more texts) matching `query`,
/// best text score first; ties keep the input order.
fn rank_texts<'a, const N: usize>(query: &str, items: impl Iterator<Item = [&'a str; N]>, matcher: &mut Matcher) -> Vec<usize> {
    matcher.config = nucleo_matcher::Config::DEFAULT;
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = Vec::new();
    for (i, texts) in items.enumerate() {
        let best = texts.iter().filter_map(|t| pattern.score(Utf32Str::new(t, &mut buf), matcher)).max();
        if let Some(s) = best {
            scored.push((s, i));
        }
    }
    scored.sort_by_key(|&(s, i)| (Reverse(s), i));
    scored.into_iter().map(|(_, i)| i).collect()
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
        if let Ok(rel) = entry.path().strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
        if out.len() >= MAX_FILES {
            tracing::warn!("file index of {} capped at {MAX_FILES} files", root.display());
            break;
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
    fn home_is_shown_as_tilde() {
        let home = Some(Path::new("/home/u"));
        assert_eq!(tilde_path(Path::new("/home/u/src/jetl"), home), "~/src/jetl");
        assert_eq!(tilde_path(Path::new("/home/u"), home), "~");
        assert_eq!(tilde_path(Path::new("/home/user2/x"), home), "/home/user2/x");
        assert_eq!(tilde_path(Path::new("/opt/x"), None), "/opt/x");
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
