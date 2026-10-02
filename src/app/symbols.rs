//! The project symbol index (runs off the main thread, fed to the Go to
//! Symbol palette) and the Structure window's sync with the active buffer.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::App;
use crate::event::AppMsg;
use crate::symbols::index::SymbolBatch;
use crate::ui::focus::{PanelId, ToolWindowId};
use crate::ui::palette::Source;

/// An open palette gets the growing index at most this often.
const SYMBOLS_FEED: Duration = Duration::from_millis(300);

impl App {
    /// Run the index, or once more after the run in flight.
    pub(super) fn request_symbols(&mut self) {
        if self.symbols.request() {
            self.spawn_symbols();
        }
    }

    fn spawn_symbols(&mut self) {
        let job = self.symbols.start(self.workspace.root.clone());
        let (generation, seq, tx) = (self.workspace.generation, job.seq, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            job.run(|batch| {
                let _ = tx.send(AppMsg::Symbols { generation, seq, batch });
            })
        });
    }

    pub(super) fn symbols_loaded(&mut self, generation: u64, seq: u64, batch: SymbolBatch) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping symbols of a previous root");
            return;
        }
        let done = batch.done;
        match self.symbols.on_batch(seq, batch) {
            None => return tracing::debug!("dropping symbols of an older index run"),
            Some(true) => self.spawn_symbols(),
            Some(false) => {}
        }
        if done {
            tracing::debug!("symbol index: {} files", self.symbols.file_count());
        }
        let due = done || self.symbols_fed.is_none_or(|t| t.elapsed() >= SYMBOLS_FEED);
        if due && let Some(p) = self.palette.as_mut().filter(|p| p.wants(Source::Symbols)) {
            p.set_symbols(self.symbols.entries(), self.symbols.indexing());
            self.symbols_fed = Some(Instant::now());
        }
    }

    /// Watcher paths: parse changed files again; an unknown source file
    /// means the file index (which the symbol index follows) is out of date.
    pub(super) fn symbols_changed(&mut self, paths: &[PathBuf]) {
        let root = &self.workspace.root;
        let rels: Vec<String> =
            paths.iter().filter_map(|p| p.strip_prefix(root).ok()).map(|r| r.to_string_lossy().replace('\\', "/")).collect();
        let (queued, unknown) = self.symbols.invalidate(&rels);
        if queued {
            self.request_symbols();
        }
        if unknown {
            self.refresh_file_index();
        }
    }

    /// Bring the Structure window up to date with the active buffer (every
    /// frame while it is shown; re-extracts only after a change).
    pub(crate) fn sync_structure(&mut self) {
        if !self.layout.is_visible(ToolWindowId::Structure) {
            return;
        }
        let follow = !self.focus.is(PanelId::Structure);
        self.structure.sync(self.editor.active_buffer_mut(), follow);
    }
}
