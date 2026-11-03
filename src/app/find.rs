//! Replace in files: only the included hits, located by position. Open tabs
//! change through the editor (undoable), the other files are rewritten off
//! the main thread; a file changed since the search is left alone.

use super::{App, Ctx};
use crate::event::{AppMsg, Level};
use crate::files::find::{hit_replacements, replace_text};

fn stale(path: &std::path::Path) -> String {
    format!("{}: changed since the search; search again", path.display())
}

impl App {
    pub(super) fn replace_in_files(&mut self) {
        let re = match self.find.pattern() {
            Ok(re) => re,
            Err(e) => return self.notify(Level::Error, format!("Replace in files: {e}")),
        };
        let (with, expand) = (self.find.replace.text.clone(), self.find.regex);
        let (mut files, mut matches, mut disk, mut errors) = (0usize, 0usize, Vec::new(), Vec::new());
        for (path, hits) in self.find.included_by_file() {
            match self.editor.index_of(&path) {
                Some(i) => match self.editor.replace_hits(i, &hits, &re, &with, expand) {
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
                let Some(reps) = hit_replacements(&text, &hits, &re, &with, expand) else {
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
