//! The `markdown.preview` action and the tick that keeps previews current.

use super::*;

/// The hint for a buffer that is not Markdown.
const NEEDS_MARKDOWN: &str = "Markdown preview needs a Markdown file";

impl App {
    /// `markdown.preview`: open the active Markdown file's preview tab, or
    /// focus the one already open; nothing on a preview itself.
    pub(super) fn markdown_preview(&mut self) {
        let Some(b) = self.editor.active_buffer() else { return };
        if b.preview.is_some() {
            return;
        }
        if self.editor.merge_active() {
            self.notify(Level::Info, crate::editor::merge::BUSY);
            return;
        }
        if !crate::editor::preview::is_markdown(b) {
            self.lsp_hint(NEEDS_MARKDOWN);
            return;
        }
        let id = b.id;
        if let Some(p) = self.editor.markdown_preview_of(id) {
            self.editor.focus_buffer_tab(p);
        } else {
            let rendered = crate::ui::markdown::render(&b.text(), &self.theme);
            self.editor.open_markdown_preview(id, rendered, &self.theme);
        }
        self.dirty_frame = true;
    }

    /// Render again the previews whose source changed (the app tick).
    pub(super) fn markdown_preview_tick(&mut self) {
        if self.editor.refresh_markdown_previews(&self.theme) {
            self.dirty_frame = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction};
    use crate::app::tests::{load, test_app};
    use crate::editor::selection::{Selection, Selections};

    fn preview(app: &mut App) {
        app.perform(Action::Editor(EditorAction::MarkdownPreview));
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    fn active_id(app: &App) -> crate::editor::buffer::BufferId {
        app.editor.active_buffer().unwrap().id
    }

    fn render_text(app: &App, src: &str) -> String {
        crate::ui::markdown::render(src, &app.theme).text
    }

    #[tokio::test]
    async fn opens_a_read_only_rendered_preview() {
        let (mut app, dir) = test_app("md-open");
        let src = "# Title\n\n- one\n";
        load(&mut app, "README.md", src);
        let (source, count) = (active_id(&app), app.editor.buffers.len());
        preview(&mut app);
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.name, "Preview README.md");
        assert!(b.path.is_none() && b.read_only && b.view.wrap.on);
        assert_eq!(b.preview.as_ref().unwrap().source, source);
        assert_eq!(b.text(), render_text(&app, src));
        assert_eq!(app.editor.buffers.len(), count + 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_second_call_focuses_the_same_preview() {
        let (mut app, dir) = test_app("md-focus");
        let path = load(&mut app, "README.md", "# Title\n");
        preview(&mut app);
        let (pid, count, toasted) = (active_id(&app), app.editor.buffers.len(), toasts(&app));
        app.open_file(path, true);
        assert_ne!(active_id(&app), pid);
        preview(&mut app);
        assert_eq!((active_id(&app), app.editor.buffers.len()), (pid, count));
        preview(&mut app);
        assert_eq!((active_id(&app), app.editor.buffers.len()), (pid, count));
        assert_eq!(toasts(&app), toasted);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_non_markdown_buffer_gives_the_hint() {
        let (mut app, dir) = test_app("md-hint");
        load(&mut app, "main.rs", "fn main() {}\n");
        let count = app.editor.buffers.len();
        preview(&mut app);
        assert_eq!(app.editor.buffers.len(), count);
        assert!(toasts(&app).iter().any(|t| t == NEEDS_MARKDOWN), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_source_edit_rerenders_on_the_tick_and_keeps_the_scroll() {
        let (mut app, dir) = test_app("md-follow");
        let src: String = (0..60).map(|i| format!("# h{i}\n")).collect();
        let path = load(&mut app, "README.md", &src);
        preview(&mut app);
        let pid = active_id(&app);
        {
            let b = app.editor.active_buffer_mut().unwrap();
            b.view.scroll_line = 20;
            let at = b.rope.line_to_char(25);
            b.view.sels = Selections::single(Selection::point(at));
        }
        app.open_file(path, true);
        let at = app.editor.active_buffer().unwrap().rope.line_to_char(40);
        app.editor.active_buffer_mut().unwrap().view.sels = Selections::single(Selection::point(at));
        app.perform(Action::Editor(EditorAction::InsertText("new ".into())));
        let source = app.editor.active_buffer().unwrap();
        let (new_src, version) = (source.text(), source.version);
        app.markdown_preview_tick();
        let i = app.editor.index_of_id(pid).unwrap();
        let b = &mut app.editor.buffers[i];
        assert_eq!(b.text(), crate::ui::markdown::render(&new_src, &crate::config::theme::Theme::default()).text);
        assert_eq!(b.preview.as_ref().unwrap().version, version);
        assert_eq!(b.view.scroll_line, 20);
        assert_eq!(b.cursor_line_col().0, 25);
        assert!(!b.dirty());
        assert!(!b.undo(), "undo must not reach the render");
        let v = b.version;
        assert!(!app.editor.refresh_markdown_previews(&app.theme), "nothing is due on the second tick");
        assert_eq!(app.editor.buffers[i].version, v);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn typing_in_the_preview_changes_nothing() {
        let (mut app, dir) = test_app("md-readonly");
        load(&mut app, "README.md", "# Title\n\ntext\n");
        preview(&mut app);
        let (text, version) = {
            let b = app.editor.active_buffer().unwrap();
            (b.text(), b.version)
        };
        for a in [EditorAction::InsertText("x".into()), EditorAction::Backspace, EditorAction::Undo, EditorAction::Redo] {
            app.perform(Action::Editor(a));
        }
        let b = app.editor.active_buffer().unwrap();
        assert_eq!((b.text(), b.version, b.dirty()), (text, version, false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_closed_source_leaves_the_preview() {
        let (mut app, dir) = test_app("md-closed");
        load(&mut app, "README.md", "# Title\n");
        let src = app.editor.active().unwrap();
        preview(&mut app);
        let pid = active_id(&app);
        let text = app.editor.active_buffer().unwrap().text();
        app.editor.force_close(src);
        app.markdown_preview_tick();
        let i = app.editor.index_of_id(pid).expect("the preview stays");
        assert_eq!(app.editor.buffers[i].text(), text);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_theme_change_rerenders() {
        let (mut app, dir) = test_app("md-theme");
        load(&mut app, "README.md", "# Title\n");
        let source = app.editor.active_buffer().unwrap().version;
        preview(&mut app);
        let mut theme = app.theme.clone();
        let marked = ratatui::style::Color::Rgb(1, 2, 3);
        theme.syntax.insert("text.title".into(), ratatui::style::Style::default().fg(marked));
        app.editor.restyle(&theme);
        assert_eq!(app.editor.active_buffer().unwrap().preview.as_ref().unwrap().version, u64::MAX);
        app.theme = theme;
        app.markdown_preview_tick();
        let p = app.editor.active_buffer().unwrap().preview.as_ref().unwrap();
        assert_eq!(p.version, source);
        assert!(!p.spans.is_empty());
        assert!(p.spans.iter().any(|(_, s)| s.fg == Some(marked)), "spans carry the new theme's heading style");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn save_as_turns_a_preview_into_a_plain_file() {
        let (mut app, dir) = test_app("md-save-as");
        let path = load(&mut app, "README.md", "# Title\n");
        let source = active_id(&app);
        preview(&mut app);
        let pid = active_id(&app);
        let to = dir.join("copy.txt");
        app.perform(Action::Editor(EditorAction::SaveAs));
        let d = app.dialog.take().expect("Save As prompt");
        app.finish_dialog(d.id, crate::ui::dialog::DialogResult::Text(to.to_string_lossy().into_owned()));
        let i = app.editor.index_of_id(pid).unwrap();
        assert!(app.editor.buffers[i].preview.is_none(), "the copy stops following its source");
        app.perform(Action::Editor(EditorAction::InsertText("mine ".into())));
        app.open_file(path, true);
        assert_eq!(active_id(&app), source);
        app.perform(Action::Editor(EditorAction::InsertText("new ".into())));
        app.markdown_preview_tick();
        let b = &app.editor.buffers[app.editor.index_of_id(pid).unwrap()];
        assert!(b.text().starts_with("mine "), "{:?}", b.text());
        assert!(b.dirty() && !b.read_only);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_preview_is_not_in_the_session() {
        let (mut app, dir) = test_app("md-session");
        let path = load(&mut app, "README.md", "# Title\n");
        preview(&mut app);
        let files = app.session().files;
        assert_eq!(files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), vec![path]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
