//! Replace in files: only the included hits, located by position. Open tabs
//! change through the editor (undoable), the other files are rewritten off
//! the main thread; a file changed since the search is left alone. A Find
//! results tab runs its own search (`find_open_in_tab`, `search_tab_rerun`).

use super::{App, Ctx};
use crate::editor::buffer::BufferId;
use crate::editor::search::preserve_case_applies;
use crate::event::{AppMsg, Level};
use crate::files::find::{Search, hit_replacements, replace_text};
use crate::ui::focus::PanelId;

fn stale(path: &std::path::Path) -> String {
    format!("{}: changed since the search; search again", path.display())
}

impl App {
    /// Alt+Enter in the Find window: show its search as a results tab in the editor.
    pub(super) fn find_open_in_tab(&mut self) {
        if self.find.rename_preview().is_some() {
            return self.notify(Level::Info, "A rename preview cannot open in a tab");
        }
        let spec = self.find.spec();
        if spec.text.is_empty() {
            return self.notify(Level::Info, "Type a query in the Find window first");
        }
        if let Err(e) = spec.query() {
            return self.notify(Level::Error, format!("Find in Files: {}", crate::files::find::pattern_error(&e)));
        }
        let cx = ctx!(self);
        let (_, run) = self.editor.open_search_tab(spec, &cx);
        drop(cx);
        if let Some((id, search)) = run {
            self.spawn_search_tab(id, search);
        }
        self.focus.set(PanelId::Editor);
    }

    /// F5 in a results tab: search again with the same query and options.
    pub(super) fn search_tab_rerun(&mut self) {
        let cx = ctx!(self);
        let run = self.editor.rerun_search_tab(&cx);
        drop(cx);
        if let Some((id, search)) = run {
            self.spawn_search_tab(id, search);
        }
    }

    fn spawn_search_tab(&self, id: BufferId, search: Search) {
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || search.run(|batch| drop(tx.send(AppMsg::SearchTab { generation, tab: id, batch }))));
    }

    pub(super) fn replace_in_files(&mut self) {
        if self.find.rename_preview().is_some() {
            return self.rename_preview_apply();
        }
        let re = match self.find.pattern() {
            Ok(re) => re,
            Err(e) => return self.notify(Level::Error, format!("Replace in files: {e}")),
        };
        let (with, expand) = (self.find.replace.text.clone(), self.find.regex);
        let preserve = preserve_case_applies(self.find.preserve_case, self.find.regex, &with);
        let (mut files, mut matches, mut disk, mut errors) = (0usize, 0usize, Vec::new(), Vec::new());
        for (path, hits) in self.find.included_by_file() {
            match self.editor.index_of(&path) {
                Some(i) => match self.editor.replace_hits(i, &hits, &re, &with, expand, preserve) {
                    Some(n) => {
                        files += usize::from(n > 0);
                        matches += n;
                    }
                    None => errors.push(stale(&path)),
                },
                None => disk.push((path, hits)),
            }
        }
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            for (path, hits) in disk {
                // Only text the search would have matched: UTF-8, no NUL.
                let text = match std::fs::read(&path).map(String::from_utf8) {
                    Ok(Ok(t)) if !t.as_bytes()[..t.len().min(8192)].contains(&0) => t,
                    Ok(_) => continue,
                    Err(e) => {
                        errors.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                };
                let Some(reps) = hit_replacements(&text, &hits, &re, &with, expand, preserve) else {
                    errors.push(stale(&path));
                    continue;
                };
                if reps.is_empty() {
                    continue;
                }
                match crate::util::fs::write_atomic(&path, &replace_text(&text, &reps)) {
                    Ok(_) => {
                        files += 1;
                        matches += reps.len();
                    }
                    Err(e) => errors.push(format!("{}: {e}", path.display())),
                }
            }
            let _ = tx.send(AppMsg::ReplaceDone { generation, files, matches, errors });
        });
    }

    pub(super) fn replace_done(&mut self, files: usize, matches: usize, errors: Vec<String>) {
        let s = |n: usize| if n == 1 { "" } else { "s" };
        self.notify(Level::Info, format!("Replaced {matches} match{} in {files} file{}", if matches == 1 { "" } else { "es" }, s(files)));
        for e in errors.iter().take(3) {
            self.notify(Level::Error, format!("Replace in files: {e}"));
        }
        if errors.len() > 3 {
            self.notify(Level::Error, format!("Replace in files: {} more errors (see the log)", errors.len() - 3));
            for e in &errors[3..] {
                tracing::error!("replace in files: {e}");
            }
        }
        // The results no longer match the files.
        let mut cx = ctx!(self);
        self.find.search(&mut cx);
    }
}
