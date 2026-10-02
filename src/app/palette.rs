//! The command palette: opening, keys, file index, accepted rows.

use crossterm::event::KeyEvent;

use super::{App, FileIndex};
use crate::actions::{Action, AppAction, EditorAction, PaletteKind};
use crate::dispatch::Dispatch;
use crate::event::{AppMsg, Effect};
use crate::git::GitRequest;
use crate::ui::palette::{Accept, ActionEntry, Palette, PaletteResult, Source};

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
        let query = self.palette.take().map(|p| p.input.text).unwrap_or_default();
        let mut p = Palette::new(kind, self.workspace.root.clone());
        p.set_recent(self.editor.recent_paths());
        if p.wants(Source::Files) {
            p.set_files(self.file_index.files.clone(), true);
            self.refresh_file_index();
        }
        if p.wants(Source::Actions) {
            p.set_actions(self.palette_actions());
        }
        if p.wants(Source::Projects) {
            p.set_projects(&self.recent_projects);
            self.load_recent_projects(false);
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
    }

    /// Read the recent projects file in `spawn_blocking`, first recording the
    /// current root when `touch` is set (startup and root switches). The
    /// result is ordered by a seq; it does not depend on the root.
    pub(crate) fn load_recent_projects(&mut self, touch: bool) {
        self.recent_projects_requested += 1;
        let seq = self.recent_projects_requested;
        let (file, tx) = (self.paths.recent_projects_file(), self.bus_tx.clone());
        let root = touch.then(|| self.workspace.root.clone());
        let at = crate::recent_projects::now();
        tokio::task::spawn_blocking(move || {
            let roots = crate::recent_projects::update(&file, root.as_deref(), at);
            let _ = tx.send(AppMsg::RecentProjects { seq, roots });
        });
    }

    pub(super) fn recent_projects_loaded(&mut self, seq: u64, roots: Vec<std::path::PathBuf>) {
        if seq < self.recent_projects_seq {
            tracing::debug!("dropping stale recent projects");
            return;
        }
        self.recent_projects_seq = seq;
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
        self.palette = None;
        self.dirty_frame = true;
        match res {
            PaletteResult::Cancel => {}
            PaletteResult::Accept(Accept::OpenFile(p)) => self.open_file(p, true),
            PaletteResult::Accept(Accept::Action(a)) => self.perform(a),
            PaletteResult::Accept(Accept::SwitchBranch(name)) => self.switch_branch(name),
            PaletteResult::Accept(Accept::NewBranch) => self.git_request(GitRequest::NewBranch),
            PaletteResult::Accept(Accept::SwitchRoot(root)) => self.apply_effects(vec![Effect::SwitchRoot(root)]),
        }
    }
}
