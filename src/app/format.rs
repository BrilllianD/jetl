//! External formatter runs (`Effect::Format`): one per buffer at a time,
//! off the main thread, results checked against the generation and the
//! request's seq before the editor applies them (`Editor::format_done`).

use std::collections::HashMap;

use super::{App, Ctx};
use crate::editor::buffer::BufferId;
use crate::editor::format::{FormatRequest, Snapshot};
use crate::event::AppMsg;

#[derive(Debug, Default)]
pub struct FormatState {
    /// Requests so far; only a buffer's newest one applies.
    seq: u64,
    /// The run in flight per buffer.
    jobs: HashMap<BufferId, Job>,
}

#[derive(Debug)]
struct Job {
    seq: u64,
    save: bool,
    snapshot: Snapshot,
    /// Aborting drops the run, which kills the formatter.
    task: tokio::task::AbortHandle,
}

impl FormatState {
    /// Stop every run (root switch, quit).
    pub(crate) fn abort_all(&mut self) {
        for (_, job) in self.jobs.drain() {
            job.task.abort();
        }
    }
}

impl App {
    /// Run `req`'s formatter in the project root. A newer request for the
    /// same buffer replaces one in flight (killing it) and keeps its save.
    pub(super) fn start_format(&mut self, req: FormatRequest) {
        self.format.seq += 1;
        let seq = self.format.seq;
        let mut save = req.save;
        if let Some(old) = self.format.jobs.remove(&req.id) {
            old.task.abort();
            save |= old.save;
        }
        let (generation, tx, root) = (self.workspace.generation, self.bus_tx.clone(), self.workspace.root.clone());
        let (id, argv, timeout, input) = (req.id, req.argv, req.timeout, req.snapshot.text.clone());
        tracing::debug!("formatting buffer {id} with {argv:?}");
        let task = tokio::spawn(async move {
            let result = crate::util::format::run(&argv, &root, input, timeout).await;
            let _ = tx.send(AppMsg::Formatted { generation, id, seq, result });
        });
        self.format.jobs.insert(id, Job { seq, save, snapshot: req.snapshot, task: task.abort_handle() });
    }

    /// A formatter finished. Dropped when the root changed or a newer
    /// request for the buffer is in flight.
    pub(super) fn format_done(&mut self, generation: u64, id: BufferId, seq: u64, result: Result<String, crate::util::format::Failure>) {
        if generation != self.workspace.generation || self.format.jobs.get(&id).is_none_or(|j| j.seq != seq) {
            tracing::debug!("dropping a stale format result for buffer {id}");
            return;
        }
        let Some(job) = self.format.jobs.remove(&id) else { return };
        let mut cx = ctx!(self);
        self.editor.format_done(id, job.save, job.snapshot, result, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction};
    use crate::app::tests::{handle_next, load, test_app};
    use crate::config::Config;
    use crate::editor::Editor;
    use crate::event::Level;

    /// An app whose Rust files format with `script` (`sh -c`), saves
    /// formatting them, and a.rs open with `text`.
    fn app_with(name: &str, script: &str, text: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        let mut config = Config::default();
        config.format.on_save = true;
        config.format.commands.insert("rust".into(), vec!["sh".into(), "-c".into(), script.into()]);
        app.editor = Editor::new(&config);
        let path = load(&mut app, "a.rs", text);
        (app, dir, path)
    }

    fn text(app: &App) -> String {
        app.editor.active_buffer().unwrap().text()
    }

    fn formatted(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Formatted { .. })
    }

    fn act(app: &mut App, a: EditorAction) {
        app.perform(Action::Editor(a));
    }

    fn warnings(app: &App) -> Vec<String> {
        // Not the terminal's own startup warning.
        app.notifications.iter().filter(|n| n.level == Level::Warn && !n.text.starts_with("Terminal")).map(|n| n.text.clone()).collect()
    }

    #[tokio::test]
    async fn save_writes_the_formatted_text_as_one_undo_step() {
        // Strips indentation: the caret stays on its code.
        let (mut app, dir, path) = app_with("format-save", "sed 's/^ *//'", "fn a() {\n}\n");
        act(&mut app, EditorAction::Move(crate::actions::Motion::Down, false));
        act(&mut app, EditorAction::InsertText("        x();\n".into()));
        act(&mut app, EditorAction::Move(crate::actions::Motion::Up, false));
        for _ in 0..8 {
            act(&mut app, EditorAction::Move(crate::actions::Motion::Right, false));
        }
        let caret = |app: &App| app.editor.active_buffer().unwrap().cursor_line_col();
        assert_eq!(caret(&app), (1, 8), "before x");
        let typed = text(&app);
        act(&mut app, EditorAction::Save);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n}\n", "written once formatted");
        handle_next(&mut app, formatted).await;
        assert_eq!(text(&app), "fn a() {\nx();\n}\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\nx();\n}\n");
        assert!(!app.editor.active_buffer().unwrap().dirty());
        assert_eq!(caret(&app), (1, 0), "still before x");
        act(&mut app, EditorAction::Undo);
        assert_eq!(text(&app), typed, "the format is one undo step");
        assert!(app.editor.active_buffer().unwrap().dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unchanged_output_and_manual_format() {
        let (mut app, dir, path) = app_with("format-same", "cat", "fn a() {}\n");
        act(&mut app, EditorAction::Format);
        handle_next(&mut app, formatted).await;
        let b = app.editor.active_buffer().unwrap();
        assert!(b.undo.is_empty() && !b.dirty(), "no transaction");
        assert!(app.notifications.iter().any(|n| n.text == "a.rs is already formatted"));
        // A manual format changes the buffer, not the file.
        app.editor.format.commands.insert("rust".into(), vec!["tr".into(), "a-z".into(), "A-Z".into()]);
        act(&mut app, EditorAction::Format);
        handle_next(&mut app, formatted).await;
        assert_eq!(text(&app), "FN A() {}\n");
        assert!(app.editor.active_buffer().unwrap().dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {}\n");
        // No formatter for the language: a warning, no request.
        app.editor.format.commands.clear();
        act(&mut app, EditorAction::Format);
        assert!(warnings(&app).iter().any(|w| w.starts_with("No formatter for rust files")));
        assert!(app.format.jobs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edits_while_formatting_drop_the_format() {
        let (mut app, dir, path) = app_with("format-stale", "tr a-z A-Z", "fn a() {}\n");
        act(&mut app, EditorAction::Format);
        act(&mut app, EditorAction::InsertText("x".into()));
        handle_next(&mut app, formatted).await;
        assert_eq!(text(&app), "xfn a() {}\n", "not formatted over the edit");
        // A save edited meanwhile writes what was saved, unformatted.
        act(&mut app, EditorAction::Save);
        act(&mut app, EditorAction::InsertText("y".into()));
        handle_next(&mut app, formatted).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xfn a() {}\n");
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.text(), "xyfn a() {}\n");
        assert!(b.dirty(), "the later edit is not saved");
        // A superseded run or one from another root is dropped.
        let (id, g) = (b.id, app.workspace.generation);
        act(&mut app, EditorAction::Format);
        let seq = app.format.seq;
        app.handle_msg(AppMsg::Formatted { generation: g, id, seq: seq - 1, result: Ok("old".into()) });
        app.handle_msg(AppMsg::Formatted { generation: g + 1, id, seq, result: Ok("other root".into()) });
        assert_eq!(text(&app), "xyfn a() {}\n");
        app.handle_msg(AppMsg::Formatted { generation: g, id, seq, result: Ok("new\n".into()) });
        assert_eq!(text(&app), "new\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failing_formatter_saves_unformatted_and_warns() {
        let script = "cat >/dev/null; echo 'error: expected item' >&2; echo ' --> <stdin>:2:1' >&2; exit 1";
        let (mut app, dir, path) = app_with("format-fail", script, "fn a() {}\n");
        act(&mut app, EditorAction::InsertText("x".into()));
        act(&mut app, EditorAction::Save);
        handle_next(&mut app, formatted).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xfn a() {}\n");
        assert!(!app.editor.active_buffer().unwrap().dirty());
        let w = warnings(&app);
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0], "Format a.rs: sh: error: expected item; saved unformatted\nerror: expected item\n --> <stdin>:2:1");
        // Closing and quitting save at once, without the formatter.
        act(&mut app, EditorAction::InsertText("y".into()));
        app.perform(Action::App(crate::actions::AppAction::Quit));
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, crate::ui::dialog::DialogResult::Button(0));
        assert!(app.should_quit && app.format.jobs.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xyfn a() {}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
