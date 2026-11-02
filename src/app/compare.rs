//! Compare with Clipboard and Compare with File: the active tab's text
//! against another text, as a side-by-side diff tab
//! (`git::diff::compare`, `Editor::open_doc_tab`).

use std::path::{Path, PathBuf};

use super::App;
use crate::actions::PaletteKind;
use crate::event::{AppMsg, Level};
use crate::git::diff::{Compared, compare};
use crate::git::model::DiffKey;
use crate::ui::focus::PanelId;
use crate::util::clipboard::{self, Paste};

const IDENTICAL: &str = "Contents are identical";
const BINARY: &str = "Binary files differ";
const EMPTY: &str = "The clipboard is empty";
const NEEDS_TAB: &str = "Compare needs an open tab";

/// The old side of a compare computed by `spawn_compare`.
enum Other {
    Text(String),
    File(PathBuf),
}

/// `path` relative to `root`, else as it is.
fn rel_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

impl App {
    /// `(title name, rel, new side header name, new side text)` of the active
    /// tab; `rel` is its path relative to the root, else its name.
    fn compare_new_side(&self, selection: bool) -> Option<(String, String, String, String)> {
        let b = self.editor.active_buffer()?;
        let rel = b.path.as_deref().map(|p| rel_name(&self.workspace.root, p)).unwrap_or_else(|| b.name.clone());
        let mut header = rel.clone();
        let selected = selection && !b.view.sel().range().is_empty();
        if selected {
            header.push_str(" (selection)");
        }
        let text = if selected { b.selected_text() } else { b.text() };
        Some((b.name.clone(), rel, header, text))
    }

    fn compare_busy(&mut self) -> bool {
        let busy = self.editor.merge_active();
        if busy {
            self.notify(Level::Info, crate::editor::merge::BUSY);
        }
        busy
    }

    fn spawn_compare(&self, title: String, key: DiffKey, other: Other, old_name: String, new: String) {
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let old = match other {
                Other::Text(t) => t.into_bytes(),
                Other::File(p) => match std::fs::read(&p) {
                    Ok(b) => b,
                    Err(e) => {
                        let result = Err(format!("Cannot read {old_name}: {e}"));
                        let _ = tx.send(AppMsg::Compared { generation, title, key, result });
                        return;
                    }
                },
            };
            let result = Ok(compare(&old, new.as_bytes()));
            let _ = tx.send(AppMsg::Compared { generation, title, key, result });
        });
    }

    /// `editor.compare_with_clipboard`: the clipboard (read like Paste: the
    /// tool off the main loop, else jetl's register) against the active tab
    /// or its selection.
    pub(super) fn compare_with_clipboard(&mut self) {
        if self.compare_busy() {
            return;
        }
        let Some((name, rel, new_name, new)) = self.compare_new_side(true) else { return self.lsp_hint(NEEDS_TAB) };
        let title = format!("{name} vs Clipboard");
        let key = DiffKey::Text { rel, old_name: "Clipboard".into(), new_name };
        let register = self.clipboard.register.clone();
        match self.clipboard_tools().paste.clone() {
            Paste::Command(argv) => {
                let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
                tokio::spawn(async move {
                    let send = |result| {
                        let _ = tx.send(AppMsg::Compared { generation, title: title.clone(), key: key.clone(), result });
                    };
                    let text = match clipboard::read(&argv).await {
                        Ok(t) => t,
                        Err(_) if !register.is_empty() => register,
                        Err(e) => return send(Err(format!("System clipboard unavailable ({e})"))),
                    };
                    if text.is_empty() {
                        return send(Err(EMPTY.into()));
                    }
                    match tokio::task::spawn_blocking(move || compare(text.as_bytes(), new.as_bytes())).await {
                        Ok(c) => send(Ok(c)),
                        Err(e) => tracing::debug!("compare task failed: {e}"),
                    }
                });
            }
            Paste::Disabled | Paste::Missing => {
                if register.is_empty() {
                    return self.lsp_hint(EMPTY);
                }
                self.spawn_compare(title, key, Other::Text(register), "Clipboard".into(), new);
            }
        }
    }

    /// `editor.compare_with_file`: pick the other file in the Compare With
    /// palette (`finish_palette` calls `compare_with_file`).
    pub(super) fn compare_with_file_pick(&mut self) {
        if self.compare_busy() {
            return;
        }
        if self.editor.active_buffer().is_none() {
            return self.lsp_hint(NEEDS_TAB);
        }
        self.open_palette(PaletteKind::CompareWith);
    }

    /// Compare `path` (its open buffer's text, else the disk's) with the
    /// active tab.
    pub(super) fn compare_with_file(&mut self, path: PathBuf) {
        if self.compare_busy() {
            return;
        }
        let Some((name, rel, new_name, new)) = self.compare_new_side(false) else { return self.lsp_hint(NEEDS_TAB) };
        let other_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        let title = format!("{name} vs {other_name}");
        let old_name = rel_name(&self.workspace.root, &path);
        let key = DiffKey::Text { rel, old_name: old_name.clone(), new_name };
        let other = match self.editor.index_of(&path) {
            Some(i) => Other::Text(self.editor.buffers[i].text()),
            None => Other::File(path),
        };
        self.spawn_compare(title, key, other, old_name, new);
    }

    /// A compare finished: open its diff tab, or give the hint.
    pub(super) fn compared(&mut self, generation: u64, title: String, key: DiffKey, result: Result<Compared, String>) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping a compare of a previous root");
            return;
        }
        match result {
            Err(e) => self.lsp_hint(e),
            Ok(Compared::Identical) => self.lsp_hint(IDENTICAL),
            Ok(Compared::Binary) => self.lsp_hint(BINARY),
            Ok(Compared::Doc(doc)) => {
                if self.compare_busy() {
                    return;
                }
                self.editor.diff_opts = self.git_panel.diff_opts();
                self.editor.open_doc_tab(&title, key, doc);
                self.focus.set(PanelId::Editor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction};
    use crate::app::tests::{handle_next, load, test_app};
    use crate::editor::selection::{Selection, Selections};
    use crate::git::diff::{DiffDoc, RowKind};
    use crate::ui::palette::{Accept, PaletteResult};
    use crate::util::clipboard::Tools;

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    fn is_compared(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Compared { .. })
    }

    async fn compare_clip(app: &mut App) {
        app.perform(Action::Editor(EditorAction::CompareWithClipboard));
        handle_next(app, is_compared).await;
    }

    /// The diff the active diff tab shows.
    fn tab_doc(app: &App) -> &DiffDoc {
        app.editor.active_buffer().unwrap().diff.as_ref().unwrap().pane.doc.as_ref().expect("doc")
    }

    fn tab_key(app: &App) -> &DiffKey {
        app.editor.active_buffer().unwrap().diff.as_ref().unwrap().pane.key.as_ref().expect("key")
    }

    #[tokio::test]
    async fn compare_with_clipboard_opens_a_diff_tab() {
        let (mut app, dir) = test_app("cmp-clip");
        let path = load(&mut app, "a.txt", "one\ntwo\nthree\n");
        app.clipboard.register = "one\n2\nthree\n".into();
        compare_clip(&mut app).await;
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.name, "a.txt vs Clipboard");
        assert!(b.diff.is_some() && b.path.is_none() && b.read_only);
        let want = DiffKey::Text { rel: "a.txt".into(), old_name: "Clipboard".into(), new_name: "a.txt".into() };
        assert_eq!(tab_key(&app), &want);
        assert!(tab_doc(&app).rows.iter().any(|r| r.kind != RowKind::Equal));
        assert!(app.focus.is(PanelId::Editor));
        let count = app.editor.buffers.len();
        app.open_file(path, true);
        assert_eq!(app.editor.active_buffer().unwrap().name, "a.txt");
        compare_clip(&mut app).await;
        assert_eq!(app.editor.buffers.len(), count);
        assert_eq!(app.editor.active_buffer().unwrap().name, "a.txt vs Clipboard");
        app.clipboard.register = "one\nTWO\nthree\n".into();
        app.open_file(app.workspace.root.join("a.txt"), true);
        compare_clip(&mut app).await;
        assert_eq!(app.editor.buffers.len(), count);
        assert!(tab_doc(&app).old.concat().contains("TWO"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compare_with_clipboard_uses_the_selection() {
        let (mut app, dir) = test_app("cmp-sel");
        load(&mut app, "a.txt", "one\ntwo\n");
        app.editor.active_buffer_mut().unwrap().view.sels = Selections::new(vec![Selection::new(4, 7)], 0);
        app.clipboard.register = "TWO".into();
        compare_clip(&mut app).await;
        let DiffKey::Text { new_name, .. } = tab_key(&app) else { panic!("text key") };
        assert_eq!(new_name, "a.txt (selection)");
        assert_eq!(tab_doc(&app).new.concat(), "two");
        assert_eq!(tab_doc(&app).old.concat(), "TWO");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compare_hints() {
        let (mut app, dir) = test_app("cmp-hints");
        load(&mut app, "a.txt", "one\n");
        app.perform(Action::Editor(EditorAction::CompareWithClipboard));
        assert!(toasts(&app).contains(&"The clipboard is empty".to_string()));
        assert_eq!(app.editor.buffers.len(), 1);
        app.clipboard.register = "one\n".into();
        compare_clip(&mut app).await;
        assert!(toasts(&app).contains(&"Contents are identical".to_string()));
        assert_eq!(app.editor.buffers.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compare_without_a_tab_says_so() {
        let (mut app, dir) = test_app("cmp-no-tab");
        let count = app.editor.buffers.len();
        let b = app.workspace.root.join("b.txt");
        std::fs::write(&b, "x\n").unwrap();
        app.clipboard.register = "x\n".into();
        let tab = "Compare needs an open tab".to_string();
        app.perform(Action::Editor(EditorAction::CompareWithClipboard));
        assert!(toasts(&app).contains(&tab), "{:?}", toasts(&app));
        app.toasts = Default::default();
        app.perform(Action::Editor(EditorAction::CompareWithFile));
        assert!(toasts(&app).contains(&tab), "{:?}", toasts(&app));
        assert!(app.palette.is_none());
        app.toasts = Default::default();
        app.compare_with_file(b);
        assert!(toasts(&app).contains(&tab), "{:?}", toasts(&app));
        assert_eq!(app.editor.buffers.len(), count);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compare_with_clipboard_reports_a_failing_tool() {
        let (mut app, dir) = test_app("cmp-tool-fails");
        load(&mut app, "a.txt", "one\n");
        app.clipboard.tools =
            Some(Tools { paste: Paste::Command(vec!["sh".into(), "-c".into(), "echo nope >&2; exit 1".into()]), copy: None });
        compare_clip(&mut app).await;
        assert!(toasts(&app).contains(&"System clipboard unavailable (sh: nope)".to_string()), "{:?}", toasts(&app));
        assert_eq!(app.editor.buffers.len(), 1);
        app.clipboard.register = "one\n".into();
        app.toasts = Default::default();
        compare_clip(&mut app).await;
        assert!(toasts(&app).contains(&"Contents are identical".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compare_while_merging_is_refused() {
        let (mut app, dir) = test_app("cmp-merge");
        let a = load(&mut app, "a.txt", "one\n");
        app.clipboard.register = "two\n".into();
        let texts = crate::git::merge::MergeTexts { base: "x\n".into(), ours: "y\n".into(), theirs: "z\n".into() };
        app.editor.open_merge(a.clone(), "a.txt".into(), texts, ["main".into(), "dev".into()], "x\n".into(), &app.theme);
        assert!(app.editor.merge_active());
        let count = app.editor.buffers.len();
        let busy = |app: &App| app.notifications.last().map(|n| n.text.clone());
        for action in [EditorAction::CompareWithClipboard, EditorAction::CompareWithFile] {
            app.notifications.clear();
            app.perform(Action::Editor(action));
            assert_eq!(busy(&app).as_deref(), Some(crate::editor::merge::BUSY));
            assert!(app.palette.is_none());
            assert_eq!(app.editor.buffers.len(), count);
        }
        app.notifications.clear();
        app.compare_with_file(a);
        assert_eq!(busy(&app).as_deref(), Some(crate::editor::merge::BUSY));
        assert_eq!(app.editor.buffers.len(), count);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compare_with_clipboard_reads_the_tool() {
        let (mut app, dir) = test_app("cmp-tool");
        app.clipboard.tools = Some(Tools { paste: Paste::Command(vec!["sh".into(), "-c".into(), "printf 'one\\n'".into()]), copy: None });
        load(&mut app, "a.txt", "one\ntwo\n");
        compare_clip(&mut app).await;
        assert_eq!(tab_doc(&app).old.concat(), "one\n");
        assert_eq!(tab_doc(&app).new.concat(), "one\ntwo\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compare_with_file_picks_from_the_palette() {
        let (mut app, dir) = test_app("cmp-file");
        let root = app.workspace.root.clone();
        let a = load(&mut app, "a.txt", "x\ny\n");
        std::fs::write(root.join("b.txt"), "x\nz\n").unwrap();
        app.perform(Action::Editor(EditorAction::CompareWithFile));
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::CompareWith));
        app.finish_palette(PaletteResult::Accept(Accept::OpenFile(root.join("b.txt"))));
        handle_next(&mut app, is_compared).await;
        assert!(app.palette.is_none());
        assert_eq!(app.editor.active_buffer().unwrap().name, "a.txt vs b.txt");
        let DiffKey::Text { old_name, .. } = tab_key(&app) else { panic!("text key") };
        assert_eq!(old_name, "b.txt");
        assert_eq!(tab_doc(&app).old.concat(), "x\nz\n");
        assert_eq!(tab_doc(&app).new.concat(), "x\ny\n");

        let count = app.editor.buffers.len();
        std::fs::write(root.join("c.bin"), b"a\0b").unwrap();
        app.open_file(a.clone(), true);
        app.perform(Action::Editor(EditorAction::CompareWithFile));
        app.finish_palette(PaletteResult::Accept(Accept::OpenFile(root.join("c.bin"))));
        handle_next(&mut app, is_compared).await;
        assert!(toasts(&app).contains(&"Binary files differ".to_string()));
        assert_eq!(app.editor.buffers.len(), count);

        app.perform(Action::Editor(EditorAction::CompareWithFile));
        app.finish_palette(PaletteResult::Accept(Accept::OpenFile(root.join("none.txt"))));
        handle_next(&mut app, is_compared).await;
        assert!(toasts(&app).iter().any(|t| t.starts_with("Cannot read none.txt:")), "{:?}", toasts(&app));

        load(&mut app, "d.txt", "q\n");
        app.perform(Action::Editor(EditorAction::InsertText("w".into())));
        app.open_file(a, true);
        app.perform(Action::Editor(EditorAction::CompareWithFile));
        app.finish_palette(PaletteResult::Accept(Accept::OpenFile(root.join("d.txt"))));
        handle_next(&mut app, is_compared).await;
        assert_eq!(tab_doc(&app).old.concat(), "wq\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stale_compare_is_dropped() {
        let (mut app, dir) = test_app("cmp-stale");
        load(&mut app, "a.txt", "a\n");
        let g = app.workspace.generation;
        app.handle_msg(AppMsg::Compared {
            generation: g + 1,
            title: "x vs Clipboard".into(),
            key: DiffKey::Text { rel: "a.txt".into(), old_name: "Clipboard".into(), new_name: "a.txt".into() },
            result: Ok(Compared::Doc(DiffDoc::build(b"a\n", b"b\n", 4))),
        });
        assert_eq!(app.editor.buffers.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
