//! The command palette: opening, keys, file index, accepted rows.

use crossterm::event::KeyEvent;

use super::{App, FileIndex};
use crate::actions::{Action, AppAction, EditorAction, PaletteKind};
use crate::dispatch::Dispatch;
use crate::event::AppMsg;
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
    /// tagged with the git generation and dropped after a root switch.
    pub fn refresh_file_index(&mut self) {
        self.file_index.loading = true;
        let (root, generation, tx) = (self.workspace.root.clone(), self.git.generation, self.bus_tx.clone());
        let (hidden, ignored) = (self.config.files.show_hidden, self.config.files.show_ignored);
        tokio::task::spawn_blocking(move || {
            let files = crate::ui::palette::walk_files(&root, hidden, ignored);
            let _ = tx.send(AppMsg::FileIndex { generation, files });
        });
    }

    pub(super) fn file_index_loaded(&mut self, generation: u64, files: Vec<String>) {
        if generation != self.git.generation {
            tracing::debug!("dropping stale file index");
            return;
        }
        self.file_index = FileIndex { generation, files: std::sync::Arc::new(files), loading: false };
        if let Some(p) = self.palette.as_mut().filter(|p| p.wants(Source::Files)) {
            p.set_files(self.file_index.files.clone(), false);
        }
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
        }
    }
}
