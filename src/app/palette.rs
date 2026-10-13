//! The command palette: opening, keys, file index, accepted rows.

use crossterm::event::KeyEvent;

use super::{App, FileIndex};
use crate::actions::{Action, AppAction, EditorAction, PaletteKind};
use crate::dispatch::Dispatch;
use crate::event::{AppMsg, Effect};
use crate::git::GitRequest;
use crate::ui::palette::{Accept, ActionEntry, FilterBus, Palette, PaletteResult, Source};

impl App {
    /// Open (or switch) the palette. Candidates come from the caches (file
    /// index, branch list); fresh ones are requested and fed in as they load.
    pub fn open_palette(&mut self, kind: PaletteKind) {
        if kind == PaletteKind::GotoLine {
            self.palette = None;
            self.perform(Action::Editor(EditorAction::GotoLine));
            return;
        }
        if self.palette.as_ref().is_some_and(|p| p.kind == kind) {
            return;
        }
        // Switching kinds keeps the typed query.
        let query = self.palette.take().map(|mut p| std::mem::take(&mut p.input.text)).unwrap_or_default();
        let mut p = Palette::new(kind, self.workspace.root.clone());
        p.set_background(FilterBus { tx: self.bus_tx.clone(), generation: self.workspace.generation, seq: self.palette_seq.clone() });
        p.set_recent(self.editor.recent_paths());
        if p.wants(Source::Files) {
            p.set_files(self.file_index.files.clone(), true);
            self.refresh_file_index();
        }
        if p.wants(Source::Actions) {
            p.set_actions(self.palette_actions());
        }
        if p.wants(Source::Symbols) {
            p.set_symbols(self.symbols.entries(), self.symbols.indexing());
            self.symbols_fed = Some(std::time::Instant::now());
            // Catches files added or changed while nothing was watching.
            if !p.wants(Source::Files) {
                self.refresh_file_index();
            }
        }
        if p.wants(Source::Projects) {
            p.set_projects(&self.recent_projects);
            self.load_recent_projects(false);
        }
        if p.wants(Source::Tasks) {
            p.set_tasks(self.run.tasks.clone(), true);
            self.load_run_tasks();
        }
        if p.wants(Source::Scratches) {
            // One read of a small directory jetl owns: current at once, and
            // right after a delete.
            p.set_scratches(crate::scratch::list(&self.paths.scratches_dir()));
        }
        if p.wants(Source::Branches) {
            p.set_branches(self.git_panel.branches.clone());
            self.branches_error_shown = false;
            self.git.load_branches();
        }
        if !query.is_empty() {
            p.set_query(&query);
        }
        self.palette = Some(p);
        self.dispatcher.reset();
    }

    /// Find Action rows: every registry entry except the palette's own
    /// navigation (and vim modes unless modal), with its shortcut.
    pub(super) fn palette_actions(&self) -> Vec<ActionEntry> {
        crate::actions::REGISTRY
            .iter()
            .filter(|a| !matches!(a.action, Action::Palette(_)))
            .filter(|a| self.dispatcher.modal || !matches!(a.action, Action::Mode(_)))
            .filter(|a| cfg!(debug_assertions) || a.action != Action::App(AppAction::DebugPanic))
            .map(|a| ActionEntry {
                id: a.id,
                desc: a.desc,
                action: a.action.clone(),
                shortcut: self.keymap.shortcut_for(&a.action, a.action.contexts()),
            })
            .collect()
    }

    /// Walk the root for the palette in `spawn_blocking`; the result is
    /// tagged with the workspace generation and dropped after a root switch.
    pub fn refresh_file_index(&mut self) {
        self.file_index.loading = true;
        self.file_index.requested += 1;
        let seq = self.file_index.requested;
        let (root, generation, tx) = (self.workspace.root.clone(), self.workspace.generation, self.bus_tx.clone());
        let (hidden, ignored) = (self.config.files.show_hidden, self.config.files.show_ignored);
        tokio::task::spawn_blocking(move || {
            let files = crate::ui::palette::walk_files(&root, hidden, ignored);
            let _ = tx.send(AppMsg::FileIndex { generation, seq, files });
        });
    }

    pub(super) fn file_index_loaded(&mut self, generation: u64, seq: u64, files: Vec<String>) {
        if generation != self.workspace.generation || seq <= self.file_index.seq {
            tracing::debug!("dropping stale file index");
            return;
        }
        let requested = self.file_index.requested;
        self.file_index = FileIndex { generation, seq, requested, files: std::sync::Arc::new(files), loading: seq < requested };
        if let Some(p) = self.palette.as_mut().filter(|p| p.wants(Source::Files)) {
            p.set_files(self.file_index.files.clone(), false);
        }
        self.symbols.set_files(&self.file_index.files);
        self.request_symbols();
    }

    /// Rows of an off-thread palette filter; dropped unless they belong to
    /// the open palette's latest filter.
    pub(super) fn palette_filtered(&mut self, generation: u64, seq: u64, rows: Vec<usize>) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping palette filter of a previous root");
        } else if !self.palette.as_mut().is_some_and(|p| p.apply_filter(seq, rows)) {
            tracing::debug!("dropping stale palette filter");
        }
    }

    /// Record the current root in the recent projects file and load the
    /// list (`main.rs` at startup; tests never call it); root switches
    /// record their root from then on.
    pub fn start_recent_projects(&mut self) {
        self.record_recent = true;
        self.load_recent_projects(true);
    }

    /// Read the recent projects file in `spawn_blocking`, first recording the
    /// current root when `touch` is set and recording is on (startup and
    /// root switches). The reply carries the file's version and does not
    /// depend on the root.
    pub(crate) fn load_recent_projects(&mut self, touch: bool) {
        self.recent_projects_requested += 1;
        let seq = self.recent_projects_requested;
        let (file, tx) = (self.paths.recent_projects_file(), self.bus_tx.clone());
        let root = (touch && self.record_recent).then(|| self.workspace.root.clone());
        let at = crate::recent_projects::now();
        tokio::task::spawn_blocking(move || {
            let (version, roots) = crate::recent_projects::update(&file, root.as_deref(), at);
            let _ = tx.send(AppMsg::RecentProjects { seq, version, roots, error: None });
        });
    }

    /// Shift+Delete in Recent Projects: the palette already dropped the row
    /// (and never lists the current root, so it cannot be forgotten here).
    /// Drop it from the list at once and from the file in `spawn_blocking`.
    /// A load asked earlier would bring the row back, so `recent_projects_forgot`
    /// drops its reply; a load asked later that read the file before the
    /// write arrives with the row, so `recent_projects_forgetting` filters the
    /// root out of every reply until the forget's own reply lands. A failed
    /// write lists the file as it is and warns.
    fn forget_recent_project(&mut self, root: std::path::PathBuf) {
        self.recent_projects.retain(|r| *r != root);
        self.recent_projects_requested += 1;
        let seq = self.recent_projects_requested;
        self.recent_projects_forgot = seq;
        self.recent_projects_forgetting.push((seq, root.clone()));
        let (file, tx) = (self.paths.recent_projects_file(), self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let (version, roots, saved) = crate::recent_projects::forget(&file, &root);
            let error = saved.err().map(|e| format!("Forget {} in recent projects: {e:#}", root.display()));
            let _ = tx.send(AppMsg::RecentProjects { seq, version, roots, error });
        });
    }

    pub(super) fn recent_projects_loaded(&mut self, seq: u64, version: u64, roots: Vec<std::path::PathBuf>, error: Option<String>) {
        if let Some(e) = error {
            self.notify(crate::event::Level::Warn, e);
        }
        self.recent_projects_forgetting.retain(|(s, _)| *s != seq);
        if seq < self.recent_projects_forgot || version <= self.recent_projects_version {
            tracing::debug!("dropping stale recent projects");
            return;
        }
        let mut roots = roots;
        roots.retain(|r| !self.recent_projects_forgetting.iter().any(|(_, f)| f == r));
        self.recent_projects_version = version;
        self.recent_projects = roots;
        if let Some(p) = self.palette.as_mut().filter(|p| p.wants(Source::Projects)) {
            p.set_projects(&self.recent_projects);
        }
        if std::mem::take(&mut self.pick_project)
            && self.palette.is_none()
            && self.dialog.is_none()
            && self.recent_projects.iter().any(|r| *r != self.workspace.root)
        {
            self.open_palette(PaletteKind::RecentProjects);
        }
    }

    /// Started without an argument outside a git repository: offer the
    /// recent projects once their list loads (if there are any).
    pub fn pick_project_at_startup(&mut self) {
        self.pick_project = true;
    }

    /// Keys while the palette is open: its own keymap context first, then
    /// global bindings (which close it, except those opening a palette).
    pub(super) fn palette_key(&mut self, key: KeyEvent) {
        let r = self.dispatcher.dispatch(key, &["palette", "global"], None, &self.keymap);
        let Some(p) = &mut self.palette else { return };
        let res = match r {
            Dispatch::Action(Action::Palette(a)) => p.handle_action(&a),
            Dispatch::Action(Action::Editor(crate::actions::EditorAction::Paste)) => {
                self.paste();
                None
            }
            Dispatch::Action(Action::App(AppAction::OpenPalette(kind))) => {
                self.open_palette(kind);
                None
            }
            Dispatch::Action(a) => {
                self.palette = None;
                self.perform(a);
                None
            }
            Dispatch::Unmapped(k) => {
                p.handle_text_key(k);
                None
            }
            Dispatch::Pending | Dispatch::Ignore => None,
        };
        if let Some(res) = res {
            self.finish_palette(res);
        }
    }

    pub(super) fn finish_palette(&mut self, res: PaletteResult) {
        // Forgetting a recent project keeps the palette open, without the row.
        if !matches!(res, PaletteResult::Accept(Accept::ForgetProject(_))) {
            self.palette = None;
        }
        self.dirty_frame = true;
        match res {
            PaletteResult::Cancel => {}
            PaletteResult::Accept(Accept::OpenFile(p)) => self.open_file(p, true),
            PaletteResult::Accept(Accept::Action(a)) => self.perform(a),
            PaletteResult::Accept(Accept::Branch(b)) => self.git_request(GitRequest::BranchOps(b)),
            PaletteResult::Accept(Accept::NewBranch) => self.git_request(GitRequest::NewBranch),
            PaletteResult::Accept(Accept::SwitchRoot(root)) => self.apply_effects(vec![Effect::SwitchRoot(root)]),
            PaletteResult::Accept(Accept::RunTask(task)) => self.run_task(*task),
            PaletteResult::Accept(Accept::DeleteScratch(path)) => self.ask_delete_scratch(path),
            PaletteResult::Accept(Accept::ForgetProject(root)) => self.forget_recent_project(root),
            // A symbol or location: back returns here even after a short jump.
            PaletteResult::Accept(Accept::OpenFileAt { path, line, col }) => {
                self.editor.push_nav_here();
                self.apply_effects(vec![Effect::OpenFileAt { path, line, col }])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::tests::{handle_next, test_app};

    /// Handle filter results until the open palette has none pending; an
    /// older filter may finish before it sees the cancel, so its result is
    /// handled (and dropped) too. Other messages are dropped.
    async fn settle_palette(app: &mut App) {
        while app.palette.as_ref().is_some_and(|p| p.filter_pending()) {
            handle_next(app, |m| matches!(m, AppMsg::PaletteFiltered { .. })).await;
        }
    }

    /// Go to File with a synthetic index of 5000 paths and
    /// `src/target_file.rs`, filtering off the main thread from one file up.
    fn large_go_to_file(app: &mut App) {
        app.open_palette(PaletteKind::Files);
        app.palette.as_mut().unwrap().set_background_min(1);
        let mut files: Vec<String> = (0..5000).map(|i| format!("pkg{}/mod_{i}.rs", i % 50)).collect();
        files.push("src/target_file.rs".into());
        files.sort();
        let (generation, seq) = (app.workspace.generation, app.file_index.requested);
        app.handle_msg(AppMsg::FileIndex { generation, seq, files });
    }

    fn type_keys(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn accept(app: &mut App) -> Option<PaletteResult> {
        app.palette.as_mut().unwrap().handle_action(&crate::actions::PaletteAction::Accept)
    }

    #[tokio::test]
    async fn go_to_file_filters_large_indexes_off_the_main_thread() {
        let (mut app, dir) = test_app("palette-large");
        large_go_to_file(&mut app);
        assert_eq!(app.palette.as_ref().unwrap().rows_selected(), (5001, 0));
        type_keys(&mut app, "targ");
        let p = app.palette.as_ref().unwrap();
        assert!(p.filter_pending());
        assert_eq!(p.rows_selected(), (5001, 0), "the previous rows stay until the result");
        settle_palette(&mut app).await;
        let target = app.workspace.root.join("src/target_file.rs");
        assert_eq!(accept(&mut app), Some(PaletteResult::Accept(Accept::OpenFile(target))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_palette_filter_results_are_dropped() {
        let (mut app, dir) = test_app("palette-stale");
        large_go_to_file(&mut app);
        type_keys(&mut app, "t");
        let seq = app.palette_seq.load(Ordering::Relaxed);
        let generation = app.workspace.generation;
        // A previous root's result, and an older filter's.
        app.handle_msg(AppMsg::PaletteFiltered { generation: generation + 1, seq, rows: vec![] });
        app.handle_msg(AppMsg::PaletteFiltered { generation, seq: seq - 1, rows: vec![] });
        let p = app.palette.as_ref().unwrap();
        assert!(p.filter_pending());
        assert_eq!(p.rows_selected(), (5001, 0));
        settle_palette(&mut app).await;
        let (rows, _) = app.palette.as_ref().unwrap().rows_selected();
        assert!(rows > 0 && rows < 5001, "{rows}");
        // A late result once the palette is closed.
        app.palette = None;
        app.handle_msg(AppMsg::PaletteFiltered { generation, seq, rows: vec![0] });
        assert!(app.palette.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn enter_right_after_typing_opens_the_best_match() {
        let (mut app, dir) = test_app("palette-enter");
        let target = app.workspace.root.join("src/target_file.rs");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "fn main() {}\n").unwrap();
        large_go_to_file(&mut app);
        type_keys(&mut app, "targetfile");
        assert!(app.palette.as_ref().unwrap().filter_pending());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.palette.is_none());
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_path(), Some(target.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// docs/TESTING.md types "local history" in Find Action and expects the
    /// window on the file; Put Label, registered after it, comes second.
    #[tokio::test]
    async fn find_action_local_history_ranks_the_window_first() {
        use crate::actions::{Action, AppAction, PaletteAction};
        let (mut app, dir) = test_app("palette-local-history-rank");
        app.open_palette(PaletteKind::Actions);
        let p = app.palette.as_mut().unwrap();
        p.set_query("local history");
        let first = p.handle_action(&PaletteAction::Accept);
        assert_eq!(first, Some(PaletteResult::Accept(Accept::Action(Action::App(AppAction::LocalHistory)))));
        p.handle_action(&PaletteAction::Down);
        let second = p.handle_action(&PaletteAction::Accept);
        assert_eq!(second, Some(PaletteResult::Accept(Accept::Action(Action::App(AppAction::PutLabel)))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
