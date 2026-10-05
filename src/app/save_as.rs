//! Save As: the path prompt (`Effect::SaveAs`, `DialogId::SaveAs`), the
//! checks on the typed path, the write, and what was waiting for it.
//!
//! What follows a save (`AfterSave`) travels inside the dialog's id, so Esc
//! at any prompt drops it and nothing is left behind in `App`. Untitled
//! buffers are asked about one at a time: quitting, closing tabs, switching
//! roots and Save All first save the buffers that have a file, then prompt
//! for the next untitled one after each successful Save As.

use std::path::PathBuf;

use super::{App, Ctx};
use crate::editor::buffer::BufferId;
use crate::event::Level;
use crate::git::BlobWhich;
use crate::ui::dialog::{Dialog, DialogId};

/// What to do once a Save As (or the save of every buffer) succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterSave {
    /// Nothing: an explicit Save As or Save.
    Nothing,
    /// Save All: the next untitled buffer.
    SaveAll,
    /// Quit once no untitled buffer is left unsaved.
    Quit,
    /// Close this buffer's tabs.
    CloseTab(BufferId),
    /// Close every tab.
    CloseAll,
    /// Close these buffers (close others, close all but pinned).
    CloseTabs(Vec<BufferId>),
    /// Switch the project root.
    SwitchRoot(PathBuf),
}

impl App {
    /// Ask where to save buffer `id`, prefilled with its path relative to
    /// the root (absolute outside it), or for an untitled buffer its name
    /// without " (recovered)".
    pub(super) fn ask_save_as(&mut self, id: BufferId, then: AfterSave) {
        let Some(b) = self.editor.index_of_id(id).map(|i| &self.editor.buffers[i]) else { return };
        let prefill = match &b.path {
            Some(p) => crate::ui::palette::display_path(p, &self.workspace.root),
            None => match b.name.strip_suffix(" (recovered)").unwrap_or(&b.name) {
                "" => "untitled.txt".to_string(),
                name => name.to_string(),
            },
        };
        let msg = format!("Where to save {}?\nPath relative to the project root:", b.name);
        self.dialog = Some(Dialog::input(DialogId::SaveAs { id, then }, "Save As", &msg, &prefill, "Save"));
    }

    /// The typed path made absolute: `~` expanded, joined to the root,
    /// `.` and `..` applied, the parent directory canonicalized when it
    /// exists (so an open buffer of the same file is found).
    pub(super) fn resolve_save_as(&self, text: &str) -> Result<PathBuf, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err("no path given".into());
        }
        let path = crate::util::fs::normalize(&self.workspace.root.join(crate::worktree::expand_tilde(t)));
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(format!("{} is not a file path", path.display()));
        };
        let path = std::fs::canonicalize(dir).map_or_else(|_| path.clone(), |d| d.join(name));
        if t.ends_with('/') || path.is_dir() {
            return Err(format!("{} is a directory", path.display()));
        }
        Ok(path)
    }

    /// The Save As prompt's answer for buffer `id`.
    pub(super) fn finish_save_as(&mut self, id: BufferId, text: String, then: AfterSave) {
        let Some(i) = self.editor.index_of_id(id) else { return };
        let path = match self.resolve_save_as(&text) {
            Ok(p) => p,
            Err(e) => return self.notify(Level::Warn, format!("Save As: {e}")),
        };
        let shown = crate::ui::palette::display_path(&path, &self.workspace.root);
        if self.editor.buffers[i].path.as_deref() == Some(path.as_path()) {
            // Its own file: a plain save, formatted only when nothing waits
            // for it.
            let mut cx = ctx!(self);
            let saved = if then == AfterSave::Nothing {
                let format = self.editor.format.on_save;
                self.editor.save_with(i, &mut cx, format)
            } else {
                self.editor.save(i, &mut cx)
            };
            let fx = cx.effects;
            self.apply_effects(fx);
            if saved {
                self.resume(then);
            }
            return;
        }
        // One buffer per path.
        if self.editor.index_of(&path).is_some() {
            return self.notify(Level::Warn, format!("{shown} is open in another tab; close it first"));
        }
        if path.exists() {
            let msg = format!("{shown} already exists.\nOverwrite it?");
            self.dialog = Some(Dialog::confirm(DialogId::SaveAsOverwrite { id, path, then }, "Save As", &msg, &["Overwrite", "Cancel"]));
            return;
        }
        self.save_as_write(id, path, then);
    }

    /// Write buffer `id` to `path` (missing directories are created), make
    /// it that file's buffer, then go on with `then`.
    pub(super) fn save_as_write(&mut self, id: BufferId, path: PathBuf, then: AfterSave) {
        let Some(i) = self.editor.index_of_id(id) else { return };
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return self.notify(Level::Error, format!("Save As: cannot create {}: {e}", dir.display()));
        }
        // A formatter's late answer must not be written to the new path.
        self.format.abort(id);
        // The file about to be overwritten (after `SaveAsOverwrite`).
        if path.is_file() {
            self.capture_disk(&path, crate::history::Kind::BeforeSaveAs);
        }
        let mut cx = ctx!(self);
        // `FileSaved` syncs the language server: the old document closes
        // and the new one opens.
        let saved = self.editor.save_as(i, path.clone(), &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
        if !saved {
            return;
        }
        self.reload_parents(&path);
        self.git.load_blob(path, BlobWhich::Head);
        self.resume(then);
    }

    /// "Save" in a dialog about unsaved changes: save the buffers that have
    /// a file, then ask about the untitled ones. A failed save or a file
    /// changed on disk stops here, as it did before Save As.
    pub(super) fn save_then(&mut self, then: AfterSave) {
        match then {
            AfterSave::CloseTab(id) => {
                let Some(i) = self.editor.index_of_id(id) else { return };
                if self.editor.buffers[i].path.is_none() {
                    return self.ask_save_as(id, AfterSave::CloseTab(id));
                }
                let mut cx = ctx!(self);
                if self.editor.save(i, &mut cx) {
                    self.editor.force_close(i);
                }
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            AfterSave::CloseTabs(ids) => {
                let mut cx = ctx!(self);
                self.editor.save_and_close_ids(&ids, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
                self.resume(AfterSave::CloseTabs(ids));
            }
            then => {
                let mut cx = ctx!(self);
                let saved = self.editor.save_all(&mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
                if saved {
                    self.resume(then);
                }
            }
        }
    }

    /// Go on after a successful save: the next untitled buffer's prompt,
    /// else what was asked for. A dialog that came up meanwhile (a file
    /// changed on disk) ends the chain.
    pub(super) fn resume(&mut self, then: AfterSave) {
        if self.dialog.is_some() {
            return;
        }
        match then {
            AfterSave::Nothing => {}
            AfterSave::CloseTab(id) => {
                if let Some(i) = self.editor.index_of_id(id) {
                    self.editor.force_close(i);
                }
            }
            AfterSave::CloseTabs(ids) => match self.editor.untitled_dirty(Some(&ids)) {
                Some(id) => self.ask_save_as(id, AfterSave::CloseTabs(ids)),
                None => {
                    // Clean ones only: saving again would ask again about
                    // a file changed on disk.
                    let ed = &self.editor;
                    let clean: Vec<BufferId> =
                        ids.iter().copied().filter(|id| ed.index_of_id(*id).is_some_and(|i| !ed.buffers[i].dirty())).collect();
                    self.editor.force_close_ids(&clean);
                }
            },
            then => match self.editor.untitled_dirty(None) {
                Some(id) => self.ask_save_as(id, then),
                None => match then {
                    AfterSave::Quit => self.should_quit = true,
                    AfterSave::CloseAll => self.editor.force_close_all(),
                    AfterSave::SwitchRoot(root) => self.do_switch_root(root),
                    _ => {}
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::actions::{Action, AppAction, EditorAction};
    use crate::app::tests::{load, test_app};
    use crate::editor::undo::TxKind;
    use crate::ui::dialog::{DialogKind, DialogResult};

    /// [`test_app`] without the startup notifications (the terminal's
    /// keyboard protocol).
    fn fresh_app(name: &str) -> (App, PathBuf) {
        let (mut app, dir) = test_app(name);
        app.notifications.clear();
        (app, dir)
    }

    /// An untitled buffer named `name` with unsaved `text`, made active.
    fn untitled(app: &mut App, name: &str, text: &str) -> BufferId {
        let i = app.editor.open_untitled(name);
        app.editor.buffers[i].insert(text, TxKind::Typing);
        app.editor.buffers[i].id
    }

    fn edit(app: &mut App, text: &str) {
        app.perform(Action::Editor(EditorAction::InsertText(text.into())));
    }

    /// The Save As prompt that is up: its buffer, continuation and prefill.
    fn prompt(app: &mut App) -> (BufferId, AfterSave, String) {
        let d = app.dialog.take().expect("a dialog");
        let DialogId::SaveAs { id, then } = d.id else { panic!("not Save As: {:?}", d.id) };
        let DialogKind::Input { input, .. } = d.kind else { panic!("not an input") };
        (id, then, input.text)
    }

    /// Answer the Save As prompt that is up with `text`.
    fn answer(app: &mut App, text: &str) -> BufferId {
        let (id, then, _) = prompt(app);
        app.finish_dialog(DialogId::SaveAs { id, then }, DialogResult::Text(text.into()));
        id
    }

    fn path_of(app: &App, id: BufferId) -> Option<PathBuf> {
        app.editor.index_of_id(id).and_then(|i| app.editor.buffers[i].path.clone())
    }

    fn errors(app: &App) -> Vec<String> {
        app.notifications.iter().filter(|n| n.level != Level::Info).map(|n| n.text.clone()).collect()
    }

    #[tokio::test]
    async fn save_of_an_untitled_buffer_prompts_and_creates_folders() {
        let (mut app, dir) = fresh_app("save-as-untitled");
        let id = untitled(&mut app, "notes (recovered)", "hello\n");
        app.perform(Action::Editor(EditorAction::Save));
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let (_, then, prefill) = prompt(&mut app);
        assert_eq!((then, prefill.as_str()), (AfterSave::Nothing, "notes"));
        app.finish_dialog(DialogId::SaveAs { id, then: AfterSave::Nothing }, DialogResult::Text("sub/deeper/notes.txt".into()));
        let path = app.workspace.root.join("sub/deeper/notes.txt");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        assert_eq!(path_of(&app, id), Some(path));
        let b = app.editor.active_buffer().unwrap();
        assert!(!b.dirty() && b.name == "notes.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quit_asks_for_each_untitled_buffer_in_turn() {
        let (mut app, dir) = fresh_app("save-as-quit");
        let a = load(&mut app, "a.txt", "a\n");
        edit(&mut app, "1");
        let u1 = untitled(&mut app, "one", "1\n");
        let u2 = untitled(&mut app, "two", "2\n");
        app.perform(Action::App(AppAction::Quit));
        assert_eq!(app.dialog.take().map(|d| d.id), Some(DialogId::QuitDirty));
        app.finish_dialog(DialogId::QuitDirty, DialogResult::Button(0));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1a\n", "files are saved first");
        assert_eq!(answer(&mut app, "one.txt"), u1);
        // Esc at the second prompt: still open, the first one stays saved.
        let (id, then, _) = prompt(&mut app);
        assert_eq!((id, &then), (u2, &AfterSave::Quit));
        app.finish_dialog(DialogId::SaveAs { id, then }, DialogResult::Cancel);
        assert!(!app.should_quit && app.dialog.is_none());
        assert!(app.workspace.root.join("one.txt").is_file());
        // Quit again: only the second one is left.
        app.perform(Action::App(AppAction::Quit));
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert!(!app.should_quit);
        assert_eq!(answer(&mut app, "two.txt"), u2);
        assert!(app.should_quit);
        assert_eq!(std::fs::read_to_string(app.workspace.root.join("two.txt")).unwrap(), "2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn closing_an_untitled_tab_saves_it_then_closes() {
        let (mut app, dir) = fresh_app("save-as-close-tab");
        let id = untitled(&mut app, "untitled", "x\n");
        app.finish_dialog(DialogId::CloseTabDirty(id), DialogResult::Button(0));
        let (_, then, prefill) = prompt(&mut app);
        assert_eq!((then, prefill.as_str()), (AfterSave::CloseTab(id), "untitled"));
        app.finish_dialog(DialogId::SaveAs { id, then: AfterSave::CloseTab(id) }, DialogResult::Text("x.txt".into()));
        assert!(app.editor.index_of_id(id).is_none(), "closed");
        assert_eq!(std::fs::read_to_string(app.workspace.root.join("x.txt")).unwrap(), "x\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn closing_several_tabs_saves_files_and_asks_for_untitled_ones() {
        let (mut app, dir) = fresh_app("save-as-close-tabs");
        let keep = load(&mut app, "keep.txt", "k\n");
        let a = load(&mut app, "a.txt", "a\n");
        edit(&mut app, "1");
        let c = load(&mut app, "c.txt", "c\n");
        let u = untitled(&mut app, "u", "u\n");
        let id_of = |app: &App, p: &Path| app.editor.buffers[app.editor.index_of(p).unwrap()].id;
        let ids = vec![id_of(&app, &a), u, id_of(&app, &c)];
        app.finish_dialog(DialogId::CloseTabsDirty(ids.clone()), DialogResult::Button(0));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1a\n");
        assert!(app.editor.index_of(&a).is_none() && app.editor.index_of(&c).is_none(), "saved and clean ones close at once");
        let (id, then, _) = prompt(&mut app);
        assert_eq!((id, &then), (u, &AfterSave::CloseTabs(ids)));
        app.finish_dialog(DialogId::SaveAs { id, then }, DialogResult::Text("u.txt".into()));
        assert!(app.editor.index_of_id(u).is_none());
        assert_eq!(app.editor.buffers.len(), 1);
        assert!(app.editor.index_of(&keep).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn switching_roots_asks_for_untitled_buffers_first() {
        let (mut app, dir) = fresh_app("save-as-switch");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other = std::fs::canonicalize(&other).unwrap();
        let root = app.workspace.root.clone();
        untitled(&mut app, "u", "u\n");
        app.switch_root(other.clone());
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert_eq!(app.workspace.root, root, "not before the prompt is answered");
        answer(&mut app, "u.txt");
        assert_eq!(app.workspace.root, other);
        assert!(root.join("u.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_all_saves_files_then_asks_for_untitled_buffers() {
        let (mut app, dir) = fresh_app("save-as-save-all");
        let a = load(&mut app, "a.txt", "a\n");
        edit(&mut app, "1");
        let u = untitled(&mut app, "u", "u\n");
        app.perform(Action::Editor(EditorAction::SaveAll));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1a\n");
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let (id, then, _) = prompt(&mut app);
        assert_eq!((id, then), (u, AfterSave::SaveAll));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_existing_file_asks_before_it_is_overwritten() {
        let (mut app, dir) = fresh_app("save-as-overwrite");
        let b = app.workspace.root.join("b.txt");
        std::fs::write(&b, "old\n").unwrap();
        let id = untitled(&mut app, "u", "new\n");
        app.perform(Action::Editor(EditorAction::SaveAs));
        answer(&mut app, "b.txt");
        let d = app.dialog.take().expect("overwrite?");
        assert_eq!(d.id, DialogId::SaveAsOverwrite { id, path: b.clone(), then: AfterSave::Nothing });
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "old\n", "not before the answer");
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "new\n");
        assert_eq!(path_of(&app, id), Some(b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_path_open_in_another_tab_is_refused() {
        let (mut app, dir) = fresh_app("save-as-open-elsewhere");
        let a = load(&mut app, "a.txt", "a\n");
        let id = untitled(&mut app, "u", "u\n");
        app.perform(Action::Editor(EditorAction::SaveAs));
        answer(&mut app, "./a.txt");
        assert!(app.dialog.is_none());
        assert_eq!(path_of(&app, id), None);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n");
        assert!(errors(&app).iter().any(|e| e.contains("open in another tab")), "{:?}", errors(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_as_onto_its_own_path_is_a_plain_save() {
        let (mut app, dir) = fresh_app("save-as-same");
        let a = load(&mut app, "a.txt", "a\n");
        edit(&mut app, "1");
        app.perform(Action::Editor(EditorAction::SaveAs));
        let (id, then, prefill) = prompt(&mut app);
        assert_eq!(prefill, "a.txt", "relative to the root");
        app.finish_dialog(DialogId::SaveAs { id, then }, DialogResult::Text("a.txt".into()));
        assert!(app.dialog.is_none(), "no overwrite question");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1a\n");
        assert!(!app.editor.active_buffer().unwrap().dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn typed_paths_resolve_against_the_root() {
        let (app, dir) = fresh_app("save-as-resolve");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        assert_eq!(app.resolve_save_as(" ./x/../y.txt "), Ok(root.join("y.txt")));
        assert_eq!(app.resolve_save_as("new/z.txt"), Ok(root.join("new/z.txt")), "a missing folder is fine");
        let outside = dir.join("out.txt");
        assert_eq!(app.resolve_save_as(outside.to_str().unwrap()), Ok(std::fs::canonicalize(&dir).unwrap().join("out.txt")));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| std::fs::canonicalize(h).ok()) {
            assert_eq!(app.resolve_save_as("~/jetl-save-as.txt"), Ok(home.join("jetl-save-as.txt")));
        }
        assert!(app.resolve_save_as("  ").is_err());
        assert!(app.resolve_save_as("sub").is_err(), "a directory");
        assert!(app.resolve_save_as("newdir/").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_as_moves_the_document_on_the_language_server() {
        use crate::app::lsp::tests::{incremental, open_on_server, with_fake_servers};
        let (mut app, dir) = fresh_app("save-as-lsp");
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "fn a() {}\n").await;
        let old = crate::lsp::sync::uri(&app.workspace.root.join("a.rs")).unwrap();
        app.perform(Action::Editor(EditorAction::SaveAs));
        answer(&mut app, "b.rs");
        let new = crate::lsp::sync::uri(&app.workspace.root.join("b.rs")).unwrap();
        assert_eq!(server.recv("textDocument/didClose").await["params"]["textDocument"]["uri"], old.as_str());
        let open = server.recv("textDocument/didOpen").await;
        assert_eq!(open["params"]["textDocument"]["uri"], new.as_str());
        assert_eq!(open["params"]["textDocument"]["text"], "fn a() {}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
