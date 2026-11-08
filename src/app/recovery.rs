//! Crash recovery in the app: keeping the panic hook's snapshot of dirty
//! buffers current, offering a root's crash dumps once its session is
//! loaded, and restoring them (`recovery.rs` has the dump format, the scan
//! and the hook side). Hot exit keeps the dirty buffers over a quit and
//! restores them silently at the next load of the root (`hot_exit_write`,
//! `hot_exit_on_exit`, `restore_hot_exit`).

use std::path::Path;

use super::App;
use crate::event::Level;
use crate::recovery::{Found, FoundEntry, RecoverySnapshot, SnapBuffer};
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::focus::PanelId;

/// Crash dumps of the current root offered in the open recovery dialogs.
pub(crate) struct RecoveryOffer {
    pub found: Found,
    /// Entries restored so far.
    pub restored: usize,
}

/// Entries the dialog lists before "… and N more".
const MAX_LISTED: usize = 8;

impl App {
    /// Let the panic hook write this app's dirty buffers (`main.rs`; tests
    /// never arm it).
    pub fn arm_recovery(&self) {
        self.recorder.arm();
    }

    /// Remove crash dumps older than 30 days in the background, except the
    /// current root's (`main.rs` at startup; tests never call it), so dumps
    /// of projects never opened again do not stay forever.
    pub fn prune_recovery_dumps(&self) {
        let dir = self.paths.recovery_dir();
        let root = self.workspace.root.clone();
        tokio::task::spawn_blocking(move || {
            let n = crate::recovery::prune(&dir, &root, crate::recent_projects::now(), crate::recovery::MAX_AGE_SECS);
            if n > 0 {
                tracing::info!("recovery: pruned {n} crash dump(s) older than 30 days from {}", dir.display());
            } else {
                tracing::debug!("recovery: no crash dumps to prune");
            }
        });
    }

    /// Refresh the panic hook's snapshot when a dirty buffer's version,
    /// saved state or path, or the root, changed since the last one (the
    /// run loop, once per turn). Rope clones are O(1).
    pub(super) fn recovery_snapshot(&mut self) {
        let generation = self.workspace.generation;
        let ed = &self.editor;
        let dirty = || ed.buffers.iter().filter(|b| b.dirty() && !ed.is_merge_buffer(b.id));
        if self.recorder.is_current(generation, dirty()) {
            return;
        }
        let snap = RecoverySnapshot { root: self.workspace.root.clone(), buffers: dirty().map(SnapBuffer::of).collect() };
        self.recorder.store(generation, snap);
    }

    /// `run` returned an error (a failed draw, not a panic): write the
    /// dirty buffers into this process's dump directory, as the panic hook
    /// would, so they are offered when the project opens again. Returns
    /// the directory and the number of buffers written; `None` when nothing
    /// was dirty or the write failed (logged).
    pub(crate) fn dump_after_error(&mut self) -> Option<(std::path::PathBuf, usize)> {
        self.recovery_snapshot();
        let dir = self.recorder.dir.clone();
        match crate::recovery::write_dump(&dir, self.recorder.last(), std::process::id(), crate::recent_projects::now()) {
            Ok(0) => None,
            Ok(n) => {
                tracing::error!("recovery: {n} unsaved buffers written to {}", dir.display());
                Some((dir, n))
            }
            Err(e) => {
                tracing::error!("recovery: writing {}: {e}", dir.display());
                None
            }
        }
    }

    /// `run` returned an error (`main.rs`): the crash dump first
    /// ([`App::dump_after_error`]), then the rest of a quit that does not
    /// draw ([`App::shutdown`]). The dump stays for the next start; the
    /// result is the dump's directory and buffer count, for the note
    /// printed once the terminal is restored.
    pub async fn finish_after_error(&mut self) -> Option<(std::path::PathBuf, usize)> {
        let saved = self.dump_after_error();
        self.shutdown().await;
        saved
    }

    /// Keep the dirty buffers for the next session of this root (hot exit;
    /// the same set as [`App::recovery_snapshot`]). Returns the number of
    /// buffers kept, or why the write failed.
    pub(super) fn hot_exit_write(&mut self) -> Result<usize, String> {
        let ed = &self.editor;
        let buffers = ed.buffers.iter().filter(|b| b.dirty() && !ed.is_merge_buffer(b.id)).map(SnapBuffer::of).collect();
        let snap = RecoverySnapshot { root: self.workspace.root.clone(), buffers };
        let (hot, rec) = (self.paths.hot_exit_dir(), self.paths.recovery_dir());
        let n = crate::recovery::write_hot_exit(&hot, &rec, &snap, std::process::id(), crate::recent_projects::now())
            .map_err(|e| e.to_string())?;
        if n > 0 {
            tracing::info!("hot exit: {n} unsaved buffer(s) kept in {}", hot.display());
        }
        Ok(n)
    }

    /// The run loop ended, hot exit on and nothing discarded: write the dirty
    /// buffers again, so text that changed after [`App::request_quit`] wrote
    /// (late messages) and quits that never asked (the terminal closed its
    /// event stream) are kept too. With nothing dirty, a dump this process
    /// wrote earlier is stale (late messages saved everything) and goes.
    /// A failed write falls back to a crash dump ([`App::dump_after_error`])
    /// and returns false: the caller must then keep the recorder armed.
    pub(crate) fn hot_exit_on_exit(&mut self) -> bool {
        if !self.config.files.hot_exit || self.quit_discards {
            return true;
        }
        if !self.editor.has_dirty() {
            crate::recovery::remove_own_hot_exit(&self.paths.hot_exit_dir(), &self.workspace.root, std::process::id());
            return true;
        }
        match self.hot_exit_write() {
            Ok(_) => true,
            Err(e) => {
                tracing::error!("hot exit: {e}");
                self.dump_after_error();
                false
            }
        }
    }

    /// Restore the buffers kept by a hot exit of this root, silently, then
    /// remove the dump (also when nothing could be restored; the setting
    /// does not matter, the text is the user's). The active tab stays the
    /// one the session chose.
    pub(crate) fn restore_hot_exit(&mut self) {
        let Some(found) = crate::recovery::take_hot_exit(&self.paths.hot_exit_dir(), &self.workspace.root) else { return };
        let active = self.editor.active_buffer().map(|b| b.id);
        let mut moved = Vec::new();
        let mut n = 0;
        for e in &found.entries {
            if !self.restore_recovered(e) {
                continue;
            }
            n += 1;
            if let Some(path) = &e.path
                && let Some(i) = self.editor.index_of(path)
                && let Some(m) = crate::editor::buffer::file_mtime(path)
                && m.duration_since(std::time::UNIX_EPOCH).is_ok_and(|d| d.as_secs() >= found.time)
            {
                // Changed since the dump (or in its second: mtimes compare in whole seconds): an explicit save asks first.
                self.editor.buffers[i].disk_mtime = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(found.time));
                moved.push(e.label(&self.workspace.root));
            }
        }
        crate::recovery::remove_dumps(&found.dirs);
        // Restoring is not navigation.
        self.editor.clear_nav();
        match active.filter(|&id| self.editor.index_of_id(id).is_some()) {
            Some(id) => {
                self.editor.focus_buffer_tab(id);
            }
            None if n > 0 => self.focus.set(PanelId::Editor),
            None => {}
        }
        if n > 0 {
            self.notify(Level::Info, format!("Restored unsaved changes of {n} {} from the last session", files(n)));
        }
        if !moved.is_empty() {
            let what = if moved.len() <= 3 { moved.join(", ") } else { format!("{} files", moved.len()) };
            self.notify(Level::Warn, format!("Changed on disk since the last session: {what}; saving asks before overwriting"));
        }
        self.dirty_frame = true;
    }

    /// Offer the crash dumps of the current root (the end of every session
    /// load). Skipped while an offer or another dialog is up, or the
    /// conflict resolver is open: the dumps stay for the next time.
    pub(crate) fn offer_recovery(&mut self) {
        if self.recovery_offer.is_some() || self.dialog.is_some() || self.editor.merge_active() {
            return;
        }
        let root = self.workspace.root.clone();
        let Some(found) = crate::recovery::scan(&self.paths.recovery_dir(), &root) else { return };
        if found.entries.is_empty() {
            crate::recovery::remove_dumps(&found.dirs);
            return;
        }
        self.dialog = Some(recovery_dialog(&found, &root));
        self.recovery_offer = Some(RecoveryOffer { found, restored: 0 });
        self.dirty_frame = true;
    }

    /// The answers of the recovery dialogs; false for other dialogs.
    pub(super) fn finish_recovery_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        if !matches!(id, DialogId::Recover | DialogId::RecoverEntry(_)) {
            return false;
        }
        let Some(offer) = &self.recovery_offer else { return true };
        if offer.found.root != self.workspace.root {
            self.recovery_offer = None;
            return true;
        }
        let entries = offer.found.entries.clone();
        match (id, res) {
            (DialogId::Recover, DialogResult::Button(0)) => {
                let n = entries.iter().filter(|e| self.restore_recovered(e)).count();
                if let Some(o) = &mut self.recovery_offer {
                    o.restored = n;
                }
                self.end_recovery(false);
            }
            (DialogId::Recover, DialogResult::Button(1)) => self.end_recovery(true),
            (DialogId::Recover, DialogResult::Button(2)) => self.review_recovery(0),
            (DialogId::Recover, _) => {
                // A stray Esc must not lose the user's work.
                self.recovery_offer = None;
                self.notify(Level::Info, "Unsaved changes kept; they are offered again when this project opens");
            }
            (DialogId::RecoverEntry(i), DialogResult::Button(0)) => {
                let restored = entries.get(*i).is_some_and(|e| self.restore_recovered(e));
                if let Some(o) = self.recovery_offer.as_mut().filter(|_| restored) {
                    o.restored += 1;
                }
                self.review_recovery(i + 1);
            }
            (DialogId::RecoverEntry(i), _) => self.review_recovery(i + 1),
            _ => {}
        }
        true
    }

    /// Ask about entry `i`, or finish after the last one.
    fn review_recovery(&mut self, i: usize) {
        match &self.recovery_offer {
            Some(o) if i < o.found.entries.len() => self.dialog = Some(review_dialog(&o.found, &self.workspace.root, i)),
            _ => self.end_recovery(false),
        }
    }

    /// Open a recovered entry as an unsaved buffer (nothing is written):
    /// over the file's buffer when it is open without unsaved edits, else
    /// over the file as read from disk (a missing file keeps its path, so a
    /// save recreates it); a file that is open with its own unsaved edits,
    /// binary or unreadable gets an untitled `<name> (recovered)` copy.
    /// False when nothing was restored.
    fn restore_recovered(&mut self, e: &FoundEntry) -> bool {
        let text = match std::fs::read(&e.text_file) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) => {
                self.notify(Level::Warn, format!("recovery: cannot read the saved text of {}: {err}", e.label(&self.workspace.root)));
                return false;
            }
        };
        let Some(path) = &e.path else { return self.restore_untitled(&e.name, &text) };
        let copy = format!("{} (recovered)", e.name);
        if let Some(i) = self.editor.index_of(path) {
            if self.editor.buffers[i].dirty() {
                // Neither version is lost.
                return self.restore_untitled(&copy, &text);
            }
            return self.editor.recover_text(i, &text);
        }
        let disk = match std::fs::read(path) {
            Ok(bytes) => self.decode_file(path, bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some((String::new(), false)),
            Err(err) => {
                self.notify(Level::Warn, format!("recovery: reading {}: {err}", path.display()));
                None
            }
        };
        let Some((disk, lossy)) = disk else {
            self.notify(Level::Warn, format!("recovery: {} restored as \"{copy}\"", path.display()));
            return self.restore_untitled(&copy, &text);
        };
        let i = self.editor.open_text(path.clone(), &disk, lossy, &self.theme);
        if self.editor.buffers[i].disk_mtime.is_some() {
            self.git.load_blob(path.clone(), crate::git::BlobWhich::Head, None);
        }
        self.editor.recover_text(i, &text)
    }

    /// `text` in a new untitled buffer named `name`; nothing for empty text.
    fn restore_untitled(&mut self, name: &str, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let i = self.editor.open_untitled(name);
        self.editor.recover_text(i, text)
    }

    /// Remove the offered dumps and report what was restored.
    fn end_recovery(&mut self, discarded: bool) {
        let Some(offer) = self.recovery_offer.take() else { return };
        crate::recovery::remove_dumps(&offer.found.dirs);
        // Restoring is not navigation.
        self.editor.clear_nav();
        let n = offer.restored;
        if !discarded && n > 0 {
            // An empty session leaves focus on the tree; the recovered tabs take it.
            self.focus.set(PanelId::Editor);
            self.notify(Level::Info, format!("Restored unsaved changes of {n} {}; save to keep them", files(n)));
        }
        self.dirty_frame = true;
    }
}

fn files(n: usize) -> &'static str {
    if n == 1 { "file" } else { "files" }
}

/// Restore all / Discard / Review… for every offered entry.
fn recovery_dialog(found: &Found, root: &Path) -> Dialog {
    let n = found.entries.len();
    let mut msg = format!("jetl did not exit cleanly. Unsaved changes of {n} {} were kept:\n", files(n));
    for e in found.entries.iter().take(MAX_LISTED) {
        msg.push_str(&format!("\n  {}", e.label(root)));
    }
    if n > MAX_LISTED {
        msg.push_str(&format!("\n  … and {} more", n - MAX_LISTED));
    }
    msg.push_str("\n\nRestored files open unsaved; the files on disk do not change.");
    Dialog::confirm(DialogId::Recover, "Recover Unsaved Changes", &msg, &["Restore all", "Discard", "Review…"])
}

/// Restore / Skip for entry `i`.
fn review_dialog(found: &Found, root: &Path, i: usize) -> Dialog {
    let title = format!("Recover Unsaved Changes ({} of {})", i + 1, found.entries.len());
    let msg = format!("Restore the unsaved changes of\n\n  {}", found.entries[i].label(root));
    Dialog::confirm(DialogId::RecoverEntry(i), &title, &msg, &["Restore", "Skip"])
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use super::super::tests::{load, test_app};
    use super::App;
    use crate::actions::{Action, EditorAction};
    use crate::event::AppMsg;
    use crate::recovery::{RecoverySnapshot, SnapBuffer};
    use crate::ui::dialog::{DialogId, DialogResult};
    use crate::ui::focus::PanelId;
    use crate::workspace::{Session, SessionFile};

    /// Above any Linux `pid_max`, so never a live process.
    pub(crate) const DEAD_PID: u32 = 999_999_999;

    /// A crash dump of the app's root written at `time` by a dead process,
    /// one buffer per `(path relative to the root, name, text)`.
    pub(crate) fn seed_dump(app: &App, time: u64, entries: &[(Option<&str>, &str, &str)]) -> PathBuf {
        seed_dump_of(app, &app.workspace.root, time, entries)
    }

    fn seed_dump_of(app: &App, root: &Path, time: u64, entries: &[(Option<&str>, &str, &str)]) -> PathBuf {
        let dir = app.paths.recovery_dir().join(format!("{DEAD_PID}-{time}"));
        let buffers = entries
            .iter()
            .enumerate()
            .map(|(i, (rel, name, text))| SnapBuffer {
                id: i as u64 + 1,
                path: rel.map(|r| root.join(r)),
                name: name.to_string(),
                version: 1,
                saved_state: 0,
                text: ropey::Rope::from_str(text),
            })
            .collect();
        let snap = RecoverySnapshot { root: root.to_path_buf(), buffers };
        assert_eq!(crate::recovery::write_dump(&dir, &snap, DEAD_PID, time).unwrap(), entries.len());
        dir
    }

    /// The root's session with `rels` as its tabs, as loaded from disk.
    fn session_with(app: &App, rels: &[&str]) -> AppMsg {
        let root = app.workspace.root.clone();
        let files = rels.iter().map(|r| SessionFile { path: root.join(r), ..Default::default() }).collect();
        let bytes = rels.iter().map(|r| std::fs::read(root.join(r))).collect();
        AppMsg::SessionLoaded {
            generation: app.workspace.generation,
            session: Box::new(Session { root, files, ..Default::default() }),
            files: bytes,
        }
    }

    fn answer(app: &mut App, want: DialogId, res: DialogResult) {
        let d = app.dialog.take().expect("a recovery dialog");
        assert_eq!(d.id, want);
        app.finish_dialog(d.id, res);
    }

    fn buffer<'a>(app: &'a App, name: &str) -> &'a crate::editor::buffer::Buffer {
        app.editor.buffers.iter().find(|b| b.name == name).unwrap_or_else(|| panic!("no buffer {name}"))
    }

    #[tokio::test]
    async fn restore_all_after_session() {
        let (mut app, dir) = test_app("recovery-restore-all");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "disk\n").unwrap();
        let dump = seed_dump(
            &app,
            100,
            &[(Some("a.txt"), "a.txt", "recovered\n"), (None, "scratch", "note\n"), (Some("gone.txt"), "gone.txt", "lost\n")],
        );
        app.handle_msg(session_with(&app, &["a.txt"]));
        let msg = app.dialog.as_ref().map(|d| d.message.clone()).unwrap_or_default();
        assert!(msg.contains("  a.txt\n  scratch (untitled)\n  gone.txt"), "{msg}");
        answer(&mut app, DialogId::Recover, DialogResult::Button(0));
        assert_eq!(app.editor.buffers.iter().filter(|b| b.path.as_deref() == Some(&*root.join("a.txt"))).count(), 1, "no duplicate tab");
        let a = buffer(&app, "a.txt");
        assert_eq!(a.text(), "recovered\n");
        assert!(a.dirty() && a.recovered);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "disk\n");
        let s = buffer(&app, "scratch");
        assert_eq!((s.path.as_deref(), s.text().as_str(), s.dirty()), (None, "note\n", true));
        let g = buffer(&app, "gone.txt");
        assert_eq!(g.path.as_deref(), Some(&*root.join("gone.txt")));
        assert!(g.dirty() && g.text() == "lost\n");
        assert!(!root.join("gone.txt").exists());
        assert!(!dump.exists());
        assert!(app.dialog.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_into_empty_session_focuses_editor() {
        let (mut app, dir) = test_app("recovery-focus");
        seed_dump(&app, 100, &[(None, "scratch", "note\n")]);
        app.handle_msg(session_with(&app, &[]));
        assert!(app.focus.is(PanelId::Project), "empty session focuses the tree");
        answer(&mut app, DialogId::Recover, DialogResult::Button(0));
        assert_eq!(app.editor.buffers.len(), 1);
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn discard_removes_dumps() {
        let (mut app, dir) = test_app("recovery-discard");
        let dump = seed_dump(&app, 100, &[(Some("a.txt"), "a.txt", "recovered\n")]);
        app.handle_msg(session_with(&app, &[]));
        answer(&mut app, DialogId::Recover, DialogResult::Button(1));
        assert!(app.editor.buffers.is_empty());
        assert!(!dump.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn review_restores_only_chosen() {
        let (mut app, dir) = test_app("recovery-review");
        let dump = seed_dump(&app, 100, &[(Some("a.txt"), "a.txt", "one\n"), (Some("b.txt"), "b.txt", "two\n")]);
        app.handle_msg(session_with(&app, &[]));
        answer(&mut app, DialogId::Recover, DialogResult::Button(2));
        assert_eq!(app.dialog.as_ref().map(|d| d.title.as_str()), Some("Recover Unsaved Changes (1 of 2)"));
        answer(&mut app, DialogId::RecoverEntry(0), DialogResult::Button(0));
        answer(&mut app, DialogId::RecoverEntry(1), DialogResult::Cancel);
        assert!(app.dialog.is_none());
        let names: Vec<&str> = app.editor.buffers.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["a.txt"]);
        assert_eq!(app.editor.buffers[0].text(), "one\n");
        assert!(!dump.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn escape_keeps_dumps_for_next_time() {
        let (mut app, dir) = test_app("recovery-escape");
        let dump = seed_dump(&app, 100, &[(Some("a.txt"), "a.txt", "recovered\n")]);
        app.handle_msg(session_with(&app, &[]));
        answer(&mut app, DialogId::Recover, DialogResult::Cancel);
        assert!(dump.exists());
        assert!(app.editor.buffers.is_empty());
        app.handle_msg(session_with(&app, &[]));
        assert_eq!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::Recover), "offered again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dump_of_another_root_is_not_offered() {
        let (mut app, dir) = test_app("recovery-other-root");
        let dump = seed_dump_of(&app, Path::new("/elsewhere"), 100, &[(Some("a.txt"), "a.txt", "recovered\n")]);
        app.handle_msg(session_with(&app, &[]));
        assert!(app.dialog.is_none());
        assert!(dump.join(crate::recovery::MANIFEST).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dirty_open_buffer_gets_an_untitled_copy() {
        let (mut app, dir) = test_app("recovery-dirty-open");
        load(&mut app, "a.txt", "disk\n");
        app.perform(Action::Editor(EditorAction::InsertText("mine ".into())));
        seed_dump(&app, 100, &[(Some("a.txt"), "a.txt", "recovered\n")]);
        app.offer_recovery();
        answer(&mut app, DialogId::Recover, DialogResult::Button(0));
        assert_eq!(buffer(&app, "a.txt").text(), "mine disk\n");
        let copy = buffer(&app, "a.txt (recovered)");
        assert_eq!((copy.path.as_deref(), copy.text().as_str(), copy.dirty()), (None, "recovered\n", true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn startup_prunes_old_dumps_of_other_roots() {
        let (app, dir) = test_app("recovery-prune");
        let other = seed_dump_of(&app, Path::new("/elsewhere"), 100, &[(Some("a.txt"), "a.txt", "old\n")]);
        let own = seed_dump(&app, 101, &[(Some("a.txt"), "a.txt", "old\n")]);
        app.prune_recovery_dumps();
        for _ in 0..200 {
            if !other.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!other.exists(), "a month-old dump of another root is pruned");
        assert!(own.exists(), "the current root's dump is left for the offer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_error_dumps_dirty_buffers() {
        let (mut app, dir) = test_app("recovery-run-error");
        load(&mut app, "a.txt", "disk\n");
        assert!(app.dump_after_error().is_none(), "nothing dirty: nothing written");
        assert!(!app.recorder.dir.exists());
        // Typed after the run loop's last snapshot: still written.
        app.perform(Action::Editor(EditorAction::InsertText("typed ".into())));
        let (dump, n) = app.dump_after_error().expect("a dump");
        assert_eq!((dump.as_path(), n), (app.recorder.dir.as_path(), 1));
        let m = crate::recovery::read_manifest(&dump).expect("manifest");
        assert_eq!(m.root, app.workspace.root);
        assert_eq!(m.buffers[0].path.as_deref(), Some(&*app.workspace.root.join("a.txt")));
        assert_eq!(std::fs::read_to_string(dump.join(&m.buffers[0].file)).unwrap(), "typed disk\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run loop error writes the dump, then cleans up as a quit does:
    /// session saved, the Run task and the Terminal shell killed, language
    /// servers shut down. The dump stays.
    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn run_error_cleans_up_like_a_quit() {
        use crate::app::lsp::tests::{incremental, open_on_server, with_fake_servers};
        let (mut app, dir) = test_app("recovery-error-cleanup");
        // Quitting saves the session only once it is loaded.
        let (root, generation) = (app.workspace.root.clone(), app.workspace.generation);
        app.session_loaded(generation, Session { root, ..Default::default() }, vec![]);
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        app.perform(Action::Editor(EditorAction::InsertText("// x\n".into())));
        let pid_file = dir.join("pid");
        let text = format!("[[run.tasks]]\nname = \"wait\"\ncommand = \"sleep 30 & echo $! > '{}'; wait\"\n", pid_file.display());
        let task = crate::config::Config::parse(&text).unwrap().run.tasks.remove(0);
        app.run_task(task);
        let task_pid = app.run.pid().unwrap();
        app.toggle_tool_window(crate::ui::focus::ToolWindowId::Terminal);
        app.start_terminal(10, 60);
        let shell = app.terminal.active().pid().unwrap();

        let (dump, n) = app.finish_after_error().await.expect("a dump");
        assert_eq!(n, 1);
        let m = crate::recovery::read_manifest(&dump).expect("the dump stays");
        assert_eq!(std::fs::read_to_string(dump.join(&m.buffers[0].file)).unwrap(), "// x\nfn a() {}\n");
        let session = Session::load(&app.paths.sessions_dir(), &app.workspace.root).expect("session saved");
        assert_eq!(session.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), [app.workspace.root.join("a.rs")]);
        assert!(!app.run.running() && !app.terminal.active().running());
        server.recv("shutdown").await;
        server.recv("exit").await;
        #[cfg(target_os = "linux")]
        {
            assert!(crate::util::process_gone(task_pid).await, "the Run task {task_pid} still runs");
            assert!(crate::util::process_gone(shell).await, "the shell {shell} still runs");
        }
        let _ = (task_pid, shell);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_follows_dirty_buffers() {
        let (mut app, dir) = test_app("recovery-snapshot");
        load(&mut app, "a.txt", "disk\n");
        app.recovery_snapshot();
        assert!(app.recorder.last().buffers.is_empty());
        let stored = app.recorder.stored();
        assert!(stored <= 1);
        app.perform(Action::Editor(EditorAction::InsertText("typed ".into())));
        app.recovery_snapshot();
        let last = app.recorder.last();
        assert_eq!(last.root, app.workspace.root);
        assert_eq!(last.buffers.len(), 1);
        assert_eq!(last.buffers[0].text.to_string(), "typed disk\n");
        let stored = app.recorder.stored();
        app.recovery_snapshot();
        assert_eq!(app.recorder.stored(), stored, "nothing changed: not stored again");
        app.perform(Action::Editor(EditorAction::Save));
        app.recovery_snapshot();
        assert!(app.recorder.last().buffers.is_empty(), "saved: nothing to recover");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Hot exit (yman 458).

    fn hot_dir(app: &App) -> PathBuf {
        app.paths.hot_exit_dir().join(crate::recovery::root_key(&app.workspace.root))
    }

    fn manifest(app: &App) -> crate::recovery::Manifest {
        crate::recovery::read_manifest(&hot_dir(app)).expect("a hot-exit manifest")
    }

    fn insert(app: &mut App, text: &str) {
        app.perform(Action::Editor(EditorAction::InsertText(text.into())));
    }

    /// `a.txt` (`"disk\n"`) with "mine " typed and an untitled `notes`.
    fn dirty_a_and_notes(app: &mut App) {
        std::fs::write(app.workspace.root.join("a.txt"), "disk\n").unwrap();
        load(app, "a.txt", "disk\n");
        insert(app, "mine ");
        app.editor.open_untitled("notes");
        insert(app, "note\n");
    }

    fn infos(app: &App) -> Vec<String> {
        app.notifications.iter().filter(|n| n.level == crate::event::Level::Info).map(|n| n.text.clone()).collect()
    }

    #[tokio::test]
    async fn quit_keeps_unsaved_buffers_without_asking() {
        let (mut app, dir) = test_app("hot-exit-quit");
        dirty_a_and_notes(&mut app);
        app.perform(Action::App(crate::actions::AppAction::Quit));
        assert!(app.dialog.is_none() && app.should_quit);
        let m = manifest(&app);
        assert_eq!(m.buffers.len(), 2);
        let root = app.workspace.root.clone();
        assert_eq!((m.buffers[0].path.as_deref(), m.buffers[0].name.as_str()), (Some(root.join("a.txt").as_path()), "a.txt"));
        assert_eq!((m.buffers[1].path.as_deref(), m.buffers[1].name.as_str()), (None, "notes"));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "disk\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn next_session_restores_silently() {
        let (mut app, dir) = test_app("hot-exit-restore");
        dirty_a_and_notes(&mut app);
        app.perform(Action::App(crate::actions::AppAction::Quit));
        assert!(app.should_quit);
        app.should_quit = false;
        app.editor.force_close_all();
        app.notifications.clear();
        app.handle_msg(session_with(&app, &["a.txt"]));
        assert!(app.dialog.is_none());
        let a = buffer(&app, "a.txt");
        assert_eq!(a.text(), "mine disk\n");
        assert!(a.dirty() && a.recovered);
        let n = buffer(&app, "notes");
        assert_eq!((n.path.as_deref(), n.text().as_str(), n.dirty()), (None, "note\n", true));
        assert!(!hot_dir(&app).exists());
        assert_eq!(app.editor.active_buffer().map(|b| b.name.as_str()), Some("a.txt"));
        assert_eq!(infos(&app), ["Restored unsaved changes of 2 files from the last session"]);
        // Undo returns the file to its disk text.
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(buffer(&app, "a.txt").text(), "disk\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hot_exit_off_keeps_the_dialog() {
        let (mut app, dir) = test_app("hot-exit-off");
        app.config.files.hot_exit = false;
        dirty_a_and_notes(&mut app);
        app.request_quit();
        assert_eq!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::QuitDirty));
        answer(&mut app, DialogId::QuitDirty, DialogResult::Button(1));
        assert!(app.should_quit);
        app.hot_exit_on_exit();
        assert!(!app.paths.hot_exit_dir().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_hot_exit_falls_back_to_the_dialog() {
        let (mut app, dir) = test_app("hot-exit-fail");
        dirty_a_and_notes(&mut app);
        std::fs::create_dir_all(&app.paths.state_dir).unwrap();
        let blocker = app.paths.hot_exit_dir();
        std::fs::write(&blocker, "not a directory").unwrap();
        app.request_quit();
        assert_eq!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::QuitDirty));
        assert!(!app.should_quit);
        assert!(app.notifications.iter().any(|n| n.level == crate::event::Level::Error && n.text.starts_with("Hot exit failed")));
        answer(&mut app, DialogId::QuitDirty, DialogResult::Button(1));
        std::fs::remove_file(&blocker).unwrap();
        app.hot_exit_on_exit();
        assert!(!blocker.exists(), "Discard writes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn loop_end_writes_when_the_terminal_closes() {
        let (mut app, dir) = test_app("hot-exit-loop-end");
        dirty_a_and_notes(&mut app);
        app.should_quit = true;
        app.hot_exit_on_exit();
        assert_eq!(manifest(&app).buffers.len(), 2);
        assert_eq!(std::fs::read_to_string(hot_dir(&app).join(&manifest(&app).buffers[1].file)).unwrap(), "note\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn merge_work_still_asks_and_stays_out() {
        let (mut app, dir) = test_app("hot-exit-merge");
        let a = load(&mut app, "a.txt", "a\n");
        let texts = crate::git::merge::MergeTexts { base: "x\n".into(), ours: "y\n".into(), theirs: "z\n".into() };
        app.editor.open_merge(a, "a.txt".into(), texts, ["main".into(), "dev".into()], "x\n".into(), &app.theme);
        app.perform(Action::Editor(EditorAction::Merge(crate::actions::MergeAction::AcceptLeft)));
        app.editor.open_untitled("notes");
        insert(&mut app, "note\n");
        assert!(
            app.editor.buffers.iter().any(|b| b.dirty() && app.editor.is_merge_buffer(b.id)),
            "the resolver has a dirty scratch buffer"
        );
        assert_eq!(app.hot_exit_write(), Ok(1), "only the untitled buffer is kept while the resolver is open");
        app.request_quit();
        answer(&mut app, DialogId::MergeQuit, DialogResult::Button(0));
        assert!(app.should_quit && app.dialog.is_none());
        let m = manifest(&app);
        assert_eq!(m.buffers.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["notes"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_keeps_a_dirty_open_buffer() {
        let (mut app, dir) = test_app("hot-exit-dirty-open");
        let root = app.workspace.root.clone();
        let a = root.join("a.txt");
        let snap = RecoverySnapshot {
            root: root.clone(),
            buffers: vec![SnapBuffer {
                id: 1,
                path: Some(a),
                name: "a.txt".into(),
                version: 1,
                saved_state: 0,
                text: ropey::Rope::from_str("kept\n"),
            }],
        };
        crate::recovery::write_hot_exit(&app.paths.hot_exit_dir(), &app.paths.recovery_dir(), &snap, DEAD_PID, 100).unwrap();
        load(&mut app, "a.txt", "disk\n");
        insert(&mut app, "mine ");
        app.restore_hot_exit();
        assert_eq!(buffer(&app, "a.txt").text(), "mine disk\n");
        let copy = buffer(&app, "a.txt (recovered)");
        assert_eq!((copy.path.as_deref(), copy.text().as_str(), copy.dirty()), (None, "kept\n", true));
        assert!(!hot_dir(&app).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hot_exit_text_survives_a_crash_dump_of_the_same_file() {
        let (mut app, dir) = test_app("hot-exit-and-crash");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "disk\n").unwrap();
        let snap = RecoverySnapshot {
            root: root.clone(),
            buffers: vec![SnapBuffer {
                id: 1,
                path: Some(root.join("a.txt")),
                name: "a.txt".into(),
                version: 1,
                saved_state: 0,
                text: ropey::Rope::from_str("hot\n"),
            }],
        };
        crate::recovery::write_hot_exit(&app.paths.hot_exit_dir(), &app.paths.recovery_dir(), &snap, DEAD_PID, 100).unwrap();
        seed_dump(&app, 200, &[(Some("a.txt"), "a.txt", "crash\n")]);
        app.handle_msg(session_with(&app, &["a.txt"]));
        answer(&mut app, DialogId::Recover, DialogResult::Button(0));
        assert_eq!(buffer(&app, "a.txt").text(), "hot\n", "the hot-exit text is not overwritten by the crash dump");
        assert_eq!(buffer(&app, "a.txt (recovered)").text(), "crash\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_write_at_loop_end_keeps_a_crash_dump_and_the_recorder() {
        let (mut app, dir) = test_app("hot-exit-loop-fail");
        dirty_a_and_notes(&mut app);
        std::fs::create_dir_all(&app.paths.state_dir).unwrap();
        std::fs::write(app.paths.hot_exit_dir(), "not a directory").unwrap();
        // The terminal closed its event stream: no request_quit ran.
        app.should_quit = true;
        assert!(!app.hot_exit_on_exit(), "the recorder must stay armed");
        let m = crate::recovery::read_manifest(&app.recorder.dir).expect("a crash dump");
        assert_eq!(m.buffers.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn text_saved_after_the_quit_wrote_drops_the_stale_dump() {
        let (mut app, dir) = test_app("hot-exit-late-save");
        dirty_a_and_notes(&mut app);
        app.perform(Action::App(crate::actions::AppAction::Quit));
        assert!(app.should_quit && hot_dir(&app).exists());
        // A late message (format result, LSP edit) saved everything.
        app.perform(Action::Editor(EditorAction::Save));
        app.editor.force_close_all();
        assert!(!app.editor.has_dirty());
        assert!(app.hot_exit_on_exit());
        assert!(!hot_dir(&app).exists(), "the dump would restore older text over the saved file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_dump_of_another_process_stays_when_nothing_is_dirty() {
        let (mut app, dir) = test_app("hot-exit-other-pid");
        let root = app.workspace.root.clone();
        let snap = RecoverySnapshot {
            root: root.clone(),
            buffers: vec![SnapBuffer {
                id: 1,
                path: None,
                name: "n".into(),
                version: 1,
                saved_state: 0,
                text: ropey::Rope::from_str("x\n"),
            }],
        };
        crate::recovery::write_hot_exit(&app.paths.hot_exit_dir(), &app.paths.recovery_dir(), &snap, DEAD_PID, 100).unwrap();
        assert!(app.hot_exit_on_exit());
        assert!(hot_dir(&app).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hot-exit dump of `a.txt` with `text`, written at `time`.
    fn seed_hot(app: &App, time: u64, text: &str) {
        let root = app.workspace.root.clone();
        let snap = RecoverySnapshot {
            root: root.clone(),
            buffers: vec![SnapBuffer {
                id: 1,
                path: Some(root.join("a.txt")),
                name: "a.txt".into(),
                version: 1,
                saved_state: 0,
                text: ropey::Rope::from_str(text),
            }],
        };
        crate::recovery::write_hot_exit(&app.paths.hot_exit_dir(), &app.paths.recovery_dir(), &snap, DEAD_PID, time).unwrap();
    }

    fn warns(app: &App) -> Vec<String> {
        app.notifications
            .iter()
            .filter(|n| n.level == crate::event::Level::Warn && n.text.starts_with("Changed on disk"))
            .map(|n| n.text.clone())
            .collect()
    }

    #[tokio::test]
    async fn restore_of_a_file_changed_since_the_dump_asks_before_saving() {
        let (mut app, dir) = test_app("hot-exit-changed");
        std::fs::write(app.workspace.root.join("a.txt"), "pulled\n").unwrap();
        // The dump is older than the file on disk.
        seed_hot(&app, 100, "mine\n");
        app.handle_msg(session_with(&app, &["a.txt"]));
        assert_eq!(buffer(&app, "a.txt").text(), "mine\n");
        assert!(buffer(&app, "a.txt").recovered);
        assert_eq!(warns(&app).len(), 1);
        assert!(warns(&app)[0].contains("a.txt"), "{:?}", warns(&app));
        app.perform(Action::Editor(EditorAction::Save));
        assert_eq!(app.dialog.as_ref().map(|d| matches!(d.id, DialogId::OverwriteExternal(_))), Some(true));
        assert_eq!(std::fs::read_to_string(app.workspace.root.join("a.txt")).unwrap(), "pulled\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_of_an_unchanged_file_saves_without_asking() {
        let (mut app, dir) = test_app("hot-exit-unchanged");
        std::fs::write(app.workspace.root.join("a.txt"), "disk\n").unwrap();
        // The dump is newer than the file.
        seed_hot(&app, crate::recent_projects::now() + 1000, "mine\n");
        app.handle_msg(session_with(&app, &["a.txt"]));
        assert!(warns(&app).is_empty(), "{:?}", warns(&app));
        app.perform(Action::Editor(EditorAction::Save));
        assert!(app.dialog.is_none());
        assert_eq!(std::fs::read_to_string(app.workspace.root.join("a.txt")).unwrap(), "mine\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
