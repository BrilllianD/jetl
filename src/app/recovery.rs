//! Crash recovery in the app: keeping the panic hook's snapshot of dirty
//! buffers current, offering a root's crash dumps once its session is
//! loaded, and restoring them (`recovery.rs` has the dump format, the scan
//! and the hook side).

use std::path::Path;

use super::App;
use crate::event::Level;
use crate::recovery::{Found, FoundEntry, RecoverySnapshot, SnapBuffer};
use crate::ui::dialog::{Dialog, DialogId, DialogResult};

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
    pub fn dump_after_error(&mut self) -> Option<(std::path::PathBuf, usize)> {
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
            self.git.load_blob(path.clone(), crate::git::BlobWhich::Head);
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
        AppMsg::SessionLoaded { generation: app.workspace.generation, session: Session { root, files, ..Default::default() }, files: bytes }
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
}
