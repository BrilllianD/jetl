//! Replace in files: open tabs change through the editor (undoable), the
//! other files are rewritten off the main thread.

use super::{App, Ctx};
use crate::event::{AppMsg, Level};
use crate::files::find::{replace_text, replacements};

impl App {
    pub(super) fn replace_in_files(&mut self) {
        let re = match self.find.pattern() {
            Ok(re) => re,
            Err(e) => return self.notify(Level::Error, format!("Replace in files: {e}")),
        };
        let (with, expand) = (self.find.replace.text.clone(), self.find.regex);
        let (mut files, mut matches, mut disk) = (0usize, 0usize, Vec::new());
        for path in self.find.hit_files() {
            match self.editor.index_of(&path) {
                Some(i) => {
                    let n = self.editor.replace_in_buffer(i, &re, &with, expand);
                    files += usize::from(n > 0);
                    matches += n;
                }
                None => disk.push(path),
            }
        }
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let mut errors = Vec::new();
            for path in disk {
                // Only text the search would have matched: UTF-8, no NUL.
                let text = match std::fs::read(&path).map(String::from_utf8) {
                    Ok(Ok(t)) if !t.as_bytes()[..t.len().min(8192)].contains(&0) => t,
                    Ok(_) => continue,
                    Err(e) => {
                        errors.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                };
                let reps = replacements(&text, &re, &with, expand);
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
