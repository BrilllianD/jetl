//! Scratch files: throwaway files of any language kept outside every
//! project, in one global directory (`Paths::scratches_dir`, made by the
//! first scratch). File > New Scratch File… picks a language and creates
//! `scratch_<n>.<ext>` there; File > Scratch Files… lists them in a palette.
//! They are plain files, so saving, autosave, crash recovery and sessions
//! need nothing special; language servers skip them (`App::lsp_sync`) and
//! the file watcher does not see them.

use std::io;
use std::path::{Path, PathBuf};

use crate::actions::PaletteKind;
use crate::app::App;
use crate::editor::syntax::{self, LANGUAGES};
use crate::event::Level;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::PanelId;

/// The highest number tried for one extension.
const MAX_N: u32 = 9999;

/// The language choice's first item.
pub const PLAIN_TEXT: &str = "Plain text";

/// One file in the scratches directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchFile {
    pub path: PathBuf,
    pub name: String,
    /// The language its name gives, or "Plain text".
    pub language: &'static str,
    /// Last modification, Unix seconds.
    pub modified: i64,
}

/// Create `scratch_<n>.<ext>` in `dir` with the lowest free `n` (counted per
/// extension) and return its path. The directory is created here, so it
/// only appears with the first scratch. `create_new` makes two jetl
/// instances racing for a name each get their own file.
pub fn create(dir: &Path, ext: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for n in 1..=MAX_N {
        let path = dir.join(format!("scratch_{n}.{ext}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!("{MAX_N} scratch files of .{ext} exist already")))
}

/// The files in `dir`, newest first, then by name. A missing directory is
/// an empty list (and is not created); dotfiles (`write_atomic`'s temp
/// files) and directories are left out.
pub fn list(dir: &Path) -> Vec<ScratchFile> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<ScratchFile> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
            let path = e.path();
            let language = syntax::detect(&path).map_or(PLAIN_TEXT, |l| l.name);
            Some(ScratchFile { path, name, language, modified })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The languages a scratch file can be: those with an extension to name it by.
fn scratch_languages() -> impl Iterator<Item = &'static syntax::LanguageDef> {
    LANGUAGES.iter().filter(|l| !l.extensions.is_empty())
}

/// New Scratch File: plain text, then every built-in language with an extension.
pub fn language_dialog() -> Dialog {
    let items = std::iter::once(PLAIN_TEXT.to_string()).chain(scratch_languages().map(|l| l.name.to_string())).collect();
    Dialog::choice(DialogId::NewScratch, "New Scratch File", "Language:", items)
}

/// The extension for item `i` of [`language_dialog`]: `txt` for plain
/// text, else the language's first extension.
pub fn extension(i: usize) -> Option<&'static str> {
    match i.checked_sub(1) {
        None => Some("txt"),
        Some(i) => scratch_languages().nth(i)?.extensions.first().copied(),
    }
}

impl App {
    /// `editor.new_scratch`: ask for the language first.
    pub(crate) fn new_scratch(&mut self) {
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        self.dialog = Some(language_dialog());
    }

    /// The language chosen: create the file and open it (empty, so at
    /// once, without a read).
    pub(crate) fn create_scratch(&mut self, choice: usize) {
        let Some(ext) = extension(choice) else { return };
        if self.editor.merge_active() {
            return self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        let dir = self.paths.scratches_dir();
        match create(&dir, ext) {
            Ok(path) => {
                self.editor.open_text(path, "", false, &self.theme);
                self.editor.enforce_tab_limit();
                self.focus.set(PanelId::Editor);
            }
            Err(e) => self.notify(Level::Error, format!("New scratch file in {}: {e}", dir.display())),
        }
    }

    /// Shift+Delete on a row of the Scratch Files palette.
    pub(crate) fn ask_delete_scratch(&mut self, path: PathBuf) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut msg = format!("Delete scratch file {name}?\nThis cannot be undone.");
        if self.editor.index_of(&path).is_some() {
            msg.push_str("\nIts tab closes; unsaved edits are lost.");
        }
        self.dialog = Some(Dialog::confirm(DialogId::DeleteScratch(path), "Delete Scratch File", &msg, &["Delete", "Cancel"]));
    }

    /// The delete dialog's answer; either way the palette comes back.
    pub(crate) fn finish_delete_scratch(&mut self, path: PathBuf, delete: bool) {
        if delete {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    self.editor.close_under(&path);
                    self.notify(Level::Info, format!("Deleted scratch file {name}"));
                }
                Err(e) => self.notify(Level::Error, format!("Delete {name}: {e}")),
            }
        }
        self.open_palette(PaletteKind::Scratches);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, AppAction, EditorAction, PaletteAction};
    use crate::app::tests::{handle_next, test_app};
    use crate::event::AppMsg;
    use crate::ui::dialog::{DialogKind, DialogResult};
    use crate::ui::palette::{Accept, PaletteResult, Source};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-scratch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        list(dir).into_iter().map(|f| f.name).collect()
    }

    #[test]
    fn create_takes_the_lowest_free_number_per_extension() {
        let base = temp("create");
        let dir = base.join("scratches");
        assert!(!dir.exists());
        assert_eq!(list(&dir), Vec::new());
        assert!(!dir.exists(), "listing does not create the directory");
        assert_eq!(create(&dir, "txt").unwrap(), dir.join("scratch_1.txt"));
        assert!(dir.is_dir());
        assert_eq!(create(&dir, "txt").unwrap(), dir.join("scratch_2.txt"));
        assert_eq!(create(&dir, "log").unwrap(), dir.join("scratch_1.log"));
        // A gap is filled; existing files are never touched.
        std::fs::remove_file(dir.join("scratch_1.txt")).unwrap();
        std::fs::write(dir.join("scratch_3.txt"), "keep").unwrap();
        assert_eq!(create(&dir, "txt").unwrap(), dir.join("scratch_1.txt"));
        assert_eq!(create(&dir, "txt").unwrap(), dir.join("scratch_4.txt"));
        assert_eq!(std::fs::read_to_string(dir.join("scratch_3.txt")).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn list_is_newest_first_without_dotfiles_or_directories() {
        let dir = temp("list");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
        for (name, secs) in [("b.txt", 100), ("a.txt", 100), ("new.log", 300), ("old.txt", 50), (".tmp-x", 400)] {
            let f = std::fs::File::create(dir.join(name)).unwrap();
            f.set_modified(at(secs)).unwrap();
        }
        assert_eq!(names(&dir), ["new.log", "a.txt", "b.txt", "old.txt"]);
        let first = &list(&dir)[0];
        assert_eq!((first.language, first.modified, first.path.clone()), (PLAIN_TEXT, 300, dir.join("new.log")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_language_choice_names_a_file_of_that_language() {
        assert_eq!(extension(0), Some("txt"));
        assert_eq!(syntax::detect(Path::new("scratch_1.txt")).map(|l| l.name), None);
        for (i, l) in scratch_languages().enumerate() {
            let ext = extension(i + 1).unwrap_or_else(|| panic!("{} has no extension", l.name));
            let path = PathBuf::from(format!("scratch_1.{ext}"));
            assert_eq!(syntax::detect(&path).map(|d| d.name), Some(l.name), "{}", path.display());
        }
        let n = scratch_languages().count();
        assert_eq!(extension(n + 1), None);
        let DialogKind::Choice { items, .. } = language_dialog().kind else { panic!("a choice") };
        assert_eq!(items.len(), n + 1);
        assert_eq!(items[0], PLAIN_TEXT);
    }

    fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.handle_key(KeyEvent::new(code, mods));
    }

    #[tokio::test]
    async fn new_scratch_opens_it_and_the_palette_lists_and_deletes_it() {
        let (mut app, dir) = test_app("scratch-flow");
        let scratches = app.paths.scratches_dir();
        // Ctrl+Alt+Shift+Insert asks for the language; the filter finds plain
        // text.
        key(&mut app, KeyCode::Insert, KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(app.dialog.as_ref().map(|d| &d.id), Some(&DialogId::NewScratch));
        assert!(!scratches.exists(), "made by the first scratch, not by asking");
        for c in "plain".chars() {
            key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.dialog.is_none());
        let path = scratches.join("scratch_1.txt");
        assert!(path.is_file());
        assert_eq!(app.editor.active_path(), Some(path.as_path()));
        assert!(app.focus.is(PanelId::Editor));
        app.perform(Action::Editor(EditorAction::InsertText("notes".into())));
        // A second one takes the next number.
        app.finish_dialog(DialogId::NewScratch, DialogResult::Choice(0));
        let second = scratches.join("scratch_2.txt");
        assert_eq!(app.editor.active_path(), Some(second.as_path()));
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(SystemTime::now() + Duration::from_secs(60)).unwrap();

        app.perform(Action::App(AppAction::OpenPalette(PaletteKind::Scratches)));
        let p = app.palette.as_mut().expect("the palette");
        assert_eq!(p.source(), Source::Scratches);
        // Newest first: Shift+Delete asks about scratch_1.txt, whose tab has
        // unsaved edits.
        key(&mut app, KeyCode::Delete, KeyModifiers::SHIFT);
        assert!(app.palette.is_none());
        let d = app.dialog.take().expect("asks first");
        assert_eq!(d.id, DialogId::DeleteScratch(path.clone()));
        assert!(d.message.contains("unsaved edits are lost"), "{}", d.message);
        // Cancel keeps it and brings the palette back.
        app.finish_dialog(d.id, DialogResult::Cancel);
        assert!(path.is_file());
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::Scratches));
        app.palette.as_mut().unwrap().handle_action(&PaletteAction::Down);
        let res = app.palette.as_mut().unwrap().handle_action(&PaletteAction::DeleteItem);
        assert_eq!(res, Some(PaletteResult::Accept(Accept::DeleteScratch(second.clone()))));
        app.palette.as_mut().unwrap().handle_action(&PaletteAction::Up);
        key(&mut app, KeyCode::Delete, KeyModifiers::SHIFT);
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert!(!path.exists());
        assert!(app.editor.index_of(&path).is_none(), "its tab closed");
        assert!(app.editor.index_of(&second).is_some());
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::Scratches));
        // Enter opens the one left.
        app.editor.force_close(app.editor.index_of(&second).unwrap());
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_path(), Some(second.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
