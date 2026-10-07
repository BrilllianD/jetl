//! Formatter runs (`Effect::Format`), one per buffer at a time: the
//! `[format]` command off the main thread, or the buffer's language server
//! (`textDocument/formatting`, given up after `[lsp] request_timeout_ms`,
//! or when the server goes away). Results are checked against the
//! generation and the request's seq before the editor applies them
//! (`Editor::format_done`); a save waiting for one writes either way.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use lsp_types::{Position, TextEdit};
use serde_json::{Value, json};

use super::lsp::Inflight;
use super::lsp_nav::FORMATTING;
use super::{App, Ctx};
use crate::editor::buffer::BufferId;
use crate::editor::format::{FormatRequest, Formatter, Snapshot};
use crate::event::{AppMsg, Level};
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag};
use crate::util::format::Failure;

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
    run: Run,
}

#[derive(Debug)]
enum Run {
    /// Aborting drops the run, which kills the formatter.
    Command(tokio::task::AbortHandle),
    /// A language server request, and when to stop waiting for it.
    Lsp { req: Inflight, deadline: Instant },
}

impl FormatState {
    /// Whether buffer `id` (any buffer: `None`) has a run in flight.
    #[cfg(test)]
    pub(super) fn running(&self, id: Option<BufferId>) -> bool {
        id.map_or(!self.jobs.is_empty(), |id| self.jobs.contains_key(&id))
    }

    /// Stop every run (root switch, quit).
    pub(crate) fn abort_all(&mut self) {
        for (_, job) in self.jobs.drain() {
            if let Run::Command(task) = job.run {
                task.abort();
            }
        }
    }
}

fn failure(summary: String) -> Failure {
    Failure { summary, stderr: String::new() }
}

impl App {
    /// Run `req`'s formatter. A newer request for the same buffer replaces
    /// one in flight (killing or cancelling it) and keeps its save.
    pub(super) fn start_format(&mut self, req: FormatRequest) {
        self.format.seq += 1;
        let seq = self.format.seq;
        let mut save = req.save;
        if let Some(old) = self.format.jobs.remove(&req.id) {
            self.stop_format_run(old.run);
            save |= old.save;
        }
        let argv = match req.via {
            Formatter::Command(argv) => argv,
            Formatter::Lsp => return self.start_lsp_format(req.id, seq, save, req.snapshot),
        };
        let (generation, tx, root) = (self.workspace.generation, self.bus_tx.clone(), self.workspace.root.clone());
        let (id, timeout, input) = (req.id, req.timeout, req.snapshot.text.clone());
        tracing::debug!("formatting buffer {id} with {argv:?}");
        let task = tokio::spawn(async move {
            let result = crate::util::format::run(&argv, &root, input, timeout).await;
            let _ = tx.send(AppMsg::Formatted { generation, id, seq, result });
        });
        self.format.jobs.insert(id, Job { seq, save, snapshot: req.snapshot, run: Run::Command(task.abort_handle()) });
    }

    /// Stop buffer `id`'s run, if any (Save As: its result is for the old
    /// path). The command is killed, a language server request cancelled;
    /// a late answer finds no job and is dropped.
    pub(super) fn abort_format(&mut self, id: BufferId) {
        if let Some(job) = self.format.jobs.remove(&id) {
            self.stop_format_run(job.run);
        }
    }

    fn stop_format_run(&self, run: Run) {
        match run {
            Run::Command(task) => task.abort(),
            Run::Lsp { req, .. } => {
                if let Some(c) = self.lsp.clients.get(&req.server) {
                    c.cancel(req.id);
                }
            }
        }
    }

    /// Ask buffer `id`'s language server to format it. Without one that
    /// formats, a save writes the text as it is, and a manual format says
    /// how to get a formatter.
    fn start_lsp_format(&mut self, id: BufferId, seq: u64, save: bool, snapshot: Snapshot) {
        let Some(b) = self.editor.index_of_id(id).map(|i| &self.editor.buffers[i]) else { return };
        let (language, indent) = (b.language().map_or("", |l| l.name), b.indent);
        let params = json!({"options": {"tabSize": indent.width, "insertSpaces": !indent.hard_tabs}});
        match self.lsp_send(id, Position::default(), FORMATTING, params, PendingTag::Formatting, false) {
            Ok(req) => {
                let deadline = Instant::now() + Duration::from_millis(self.config.lsp.request_timeout_ms);
                self.format.jobs.insert(id, Job { seq, save, snapshot, run: Run::Lsp { req, deadline } });
            }
            Err(_) if save => {
                let text = snapshot.text.clone();
                self.finish_format(id, save, snapshot, Ok(text));
            }
            Err(why) => {
                let mut msg = format!("No formatter for {language} files: add `{language} = [...]` under [format] in config.toml");
                if !why.is_empty() {
                    msg = format!("{msg} ({why})");
                }
                self.notify(Level::Warn, msg);
            }
        }
    }

    /// A formatter finished. Dropped when the root changed or a newer
    /// request for the buffer is in flight.
    pub(super) fn format_done(&mut self, generation: u64, id: BufferId, seq: u64, result: Result<String, Failure>) {
        if generation != self.workspace.generation || self.format.jobs.get(&id).is_none_or(|j| j.seq != seq) {
            tracing::debug!("dropping a stale format result for buffer {id}");
            return;
        }
        let Some(job) = self.format.jobs.remove(&id) else { return };
        self.finish_format(id, job.save, job.snapshot, result);
    }

    fn finish_format(&mut self, id: BufferId, save: bool, snapshot: Snapshot, result: Result<String, Failure>) {
        let mut cx = ctx!(self);
        self.editor.format_done(id, save, snapshot, result, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// A language server's formatting answer: the text with its edits made
    /// (applied by the editor while the buffer is as it was sent).
    pub(super) fn lsp_format_answer(&mut self, name: &str, t: DocTag, result: Result<Value, ResponseError>) {
        let current = self.format.jobs.get(&t.buffer).is_some_and(|j| matches!(&j.run, Run::Lsp { req, .. } if req.seq == t.seq));
        if !current {
            return tracing::debug!("dropping a stale formatting answer for buffer {}", t.buffer);
        }
        let Some(job) = self.format.jobs.remove(&t.buffer) else { return };
        let result = match result.map(serde_json::from_value::<Option<Vec<TextEdit>>>) {
            Ok(Ok(edits)) => crate::lsp::edits::edit_text(&job.snapshot.text, &edits.unwrap_or_default())
                .map(|(text, _)| text)
                .map_err(|_| failure(format!("{name}: overlapping edits"))),
            Ok(Err(e)) => Err(failure(format!("{name}: unexpected answer ({e})"))),
            Err(e) => Err(failure(format!("{name}: {}", e.message))),
        };
        self.finish_format(t.buffer, job.save, job.snapshot, result);
    }

    /// Give up on language server formats unanswered by `now` (the tick):
    /// a save waiting for one writes the text unformatted.
    pub(super) fn format_timeout(&mut self, now: Instant) {
        let secs = Duration::from_millis(self.config.lsp.request_timeout_ms).as_secs_f32();
        self.fail_lsp_formats(|req, deadline| (now >= deadline).then(|| format!("{}: no answer in {secs} s", req.server)));
    }

    /// Server `name` went away: its formats fail.
    pub(super) fn format_server_gone(&mut self, name: &str) {
        self.fail_lsp_formats(|req, _| (req.server == name).then(|| format!("{name} stopped")));
    }

    fn fail_lsp_formats(&mut self, why: impl Fn(&Inflight, Instant) -> Option<String>) {
        let failed: Vec<(BufferId, String)> = self
            .format
            .jobs
            .iter()
            .filter_map(|(id, j)| match &j.run {
                Run::Lsp { req, deadline } => why(req, *deadline).map(|w| (*id, w)),
                Run::Command(_) => None,
            })
            .collect();
        for (id, why) in failed {
            let Some(job) = self.format.jobs.remove(&id) else { continue };
            self.stop_format_run(job.run);
            self.finish_format(id, job.save, job.snapshot, Err(failure(why)));
        }
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
        crate::app::tests::with_history(&mut app);
        act(&mut app, EditorAction::Format);
        act(&mut app, EditorAction::InsertText("x".into()));
        handle_next(&mut app, formatted).await;
        assert_eq!(text(&app), "xfn a() {}\n", "not formatted over the edit");
        // A save edited meanwhile writes what was saved, unformatted.
        act(&mut app, EditorAction::Save);
        act(&mut app, EditorAction::InsertText("y".into()));
        handle_next(&mut app, formatted).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xfn a() {}\n");
        // Local history has what was written, not the buffer.
        let snaps = crate::app::history::tests::snapshots(&app, &path);
        assert_eq!(snaps.first().map(|(_, t)| t.as_str()), Some("xfn a() {}\n"));
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

    #[tokio::test]
    async fn a_rename_while_formatting_drops_the_format() {
        let (mut app, dir, path) = app_with("format-rename", "tr a-z A-Z", "fn a() {}\n");
        act(&mut app, EditorAction::InsertText("x".into()));
        act(&mut app, EditorAction::Save);
        let moved = app.workspace.root.join("b.rs");
        std::fs::rename(&path, &moved).unwrap();
        app.editor.retarget(&path, &moved, &app.theme);
        handle_next(&mut app, formatted).await;
        assert_eq!(text(&app), "xfn a() {}\n", "not formatted after the move");
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "xfn a() {}\n", "the save writes the text as it was");
        assert!(!app.editor.active_buffer().unwrap().dirty());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    mod lsp {
        use super::*;
        use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
        use crate::lsp::fake::FakeServer;
        use serde_json::json;

        const UGLY: &str = "fn a(){\nx( 1,2 );\n}\n";
        const PRETTY: &str = "fn a() {\n    x(1, 2);\n}\n";

        /// The edits a server makes of `UGLY` into `PRETTY`.
        fn edits() -> serde_json::Value {
            let r =
                |l0: u32, c0: u32, l1: u32, c1: u32| json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}});
            json!([
                {"range": r(0, 6, 0, 6), "newText": " "},
                {"range": r(1, 0, 1, 0), "newText": "    "},
                {"range": r(1, 2, 1, 3), "newText": ""},
                {"range": r(1, 5, 1, 5), "newText": " "},
                {"range": r(1, 6, 1, 7), "newText": ""},
            ])
        }

        /// a.rs open on a server that formats (or not), saves formatting.
        async fn setup(name: &str, formats: bool) -> (App, FakeServer, std::path::PathBuf, std::path::PathBuf) {
            let (mut app, dir) = test_app(name);
            let mut caps = incremental();
            caps["documentFormattingProvider"] = json!(formats);
            let mut servers = with_fake_servers(&mut app, caps);
            app.editor.format.on_save = true;
            let server = open_on_server(&mut app, &mut servers, "a.rs", UGLY).await;
            let path = app.workspace.root.join("a.rs");
            (app, server, dir, path)
        }

        #[tokio::test]
        async fn without_a_command_the_language_server_formats() {
            let (mut app, mut server, dir, path) = setup("format-lsp", true).await;
            act(&mut app, EditorAction::Format);
            let req = server.recv("textDocument/formatting").await;
            assert_eq!(req["params"]["options"], json!({"tabSize": 4, "insertSpaces": true}));
            assert!(req["params"]["textDocument"]["uri"].as_str().unwrap().ends_with("/a.rs"));
            server.respond(req["id"].as_i64().unwrap(), edits());
            handle_next(&mut app, is_lsp).await;
            assert_eq!(text(&app), PRETTY);
            assert!(app.editor.active_buffer().unwrap().dirty());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), UGLY, "a manual format does not save");
            act(&mut app, EditorAction::Undo);
            assert_eq!(text(&app), UGLY, "one undo step");
            // Nothing to change: said so.
            act(&mut app, EditorAction::Redo);
            act(&mut app, EditorAction::Format);
            let req = server.recv("textDocument/formatting").await;
            server.respond(req["id"].as_i64().unwrap(), json!(null));
            handle_next(&mut app, is_lsp).await;
            assert!(app.notifications.iter().any(|n| n.text == "a.rs is already formatted"));

            // A [format] command wins over the server.
            app.editor.format.commands.insert("rust".into(), vec!["tr".into(), "a-z".into(), "A-Z".into()]);
            act(&mut app, EditorAction::Format);
            handle_next(&mut app, formatted).await;
            assert_eq!(text(&app), PRETTY.to_uppercase());
            app.lsp_sync();
            assert!(server.drain().iter().all(|m| m["method"] != "textDocument/formatting"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn save_waits_for_the_server_and_writes_the_formatted_text() {
            let (mut app, mut server, dir, path) = setup("format-lsp-save", true).await;
            // Edited (dirty) back to the text the edits are for.
            act(&mut app, EditorAction::InsertText(" ".into()));
            act(&mut app, EditorAction::Backspace);
            act(&mut app, EditorAction::Save);
            let req = server.recv("textDocument/formatting").await;
            assert_eq!(std::fs::read_to_string(&path).unwrap(), UGLY, "not written before the answer");
            server.respond(req["id"].as_i64().unwrap(), edits());
            handle_next(&mut app, is_lsp).await;
            assert_eq!(std::fs::read_to_string(&path).unwrap(), PRETTY);
            assert!(!app.editor.active_buffer().unwrap().dirty());
            assert!(warnings(&app).is_empty(), "{:?}", warnings(&app));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn a_server_that_does_not_answer_in_time_saves_unformatted() {
            let (mut app, mut server, dir, path) = setup("format-lsp-timeout", true).await;
            act(&mut app, EditorAction::InsertText("// x\n".into()));
            act(&mut app, EditorAction::Save);
            let req = server.recv("textDocument/formatting").await;
            let late = Duration::from_millis(app.config.lsp.request_timeout_ms);
            app.format_timeout(Instant::now() + late);
            let saved = format!("// x\n{UGLY}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
            assert!(!app.editor.active_buffer().unwrap().dirty());
            assert_eq!(warnings(&app), ["Format a.rs: rust-analyzer: no answer in 5 s; saved unformatted"]);
            assert_eq!(server.recv("$/cancelRequest").await["params"]["id"], req["id"]);
            // The late answer changes nothing.
            server.respond(req["id"].as_i64().unwrap(), edits());
            handle_next(&mut app, is_lsp).await;
            assert_eq!(text(&app), saved);
            // Quitting with unsaved edits saves at once, without the server.
            act(&mut app, EditorAction::InsertText("y".into()));
            app.perform(Action::App(crate::actions::AppAction::Quit));
            let d = app.dialog.take().unwrap();
            app.finish_dialog(d.id, crate::ui::dialog::DialogResult::Button(0));
            assert!(app.should_quit && app.format.jobs.is_empty());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("// x\ny{UGLY}"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn without_a_formatter_save_writes_at_once_and_format_says_so() {
            let (mut app, mut server, dir, path) = setup("format-lsp-none", false).await;
            act(&mut app, EditorAction::InsertText("x".into()));
            act(&mut app, EditorAction::Save);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("x{UGLY}"));
            assert!(warnings(&app).is_empty(), "{:?}", warnings(&app));
            act(&mut app, EditorAction::Format);
            assert_eq!(
                warnings(&app),
                [
                    "No formatter for rust files: add `rust = [...]` under [format] in config.toml (rust-analyzer does not support formatting)"
                ]
            );
            assert!(app.format.jobs.is_empty());
            app.lsp_sync();
            assert!(server.drain().iter().all(|m| m["method"] != "textDocument/formatting"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn a_formatter_registered_later_formats_until_unregistered() {
            use crate::app::lsp_nav::{FORMATTING, RENAME};
            let (mut app, mut server, dir, _) = setup("format-lsp-dynamic", false).await;
            let id = app.editor.active_buffer().unwrap().id;
            assert_eq!(app.lsp_check(id, FORMATTING), Err("rust-analyzer does not support formatting".into()));
            let selector = json!([{"language": "rust", "scheme": "file", "pattern": "**/*.rs"}]);
            let fmt = json!({"id": "fmt", "method": "textDocument/formatting", "registerOptions": {"documentSelector": selector}});
            server.request(1, "client/registerCapability", json!({"registrations": [fmt]}));
            handle_next(&mut app, is_lsp).await;
            assert_eq!(server.response(1).await["result"], serde_json::Value::Null);
            act(&mut app, EditorAction::Format);
            let req = server.recv("textDocument/formatting").await;
            server.respond(req["id"].as_i64().unwrap(), edits());
            handle_next(&mut app, is_lsp).await;
            assert_eq!(text(&app), PRETTY);

            // A registration for another language does not apply.
            let py =
                json!({"id": "py", "method": "textDocument/rename", "registerOptions": {"documentSelector": [{"language": "python"}]}});
            server.request(2, "client/registerCapability", json!({"registrations": [py]}));
            handle_next(&mut app, is_lsp).await;
            assert_eq!(server.response(2).await["result"], serde_json::Value::Null);
            assert!(app.lsp_check(id, RENAME).is_err());

            server.request(
                3,
                "client/unregisterCapability",
                json!({"unregisterations": [{"id": "fmt", "method": "textDocument/formatting"}]}),
            );
            handle_next(&mut app, is_lsp).await;
            assert_eq!(server.response(3).await["result"], serde_json::Value::Null);
            assert_eq!(app.lsp_check(id, FORMATTING), Err("rust-analyzer does not support formatting".into()));
            act(&mut app, EditorAction::Format);
            assert!(warnings(&app).iter().any(|w| w.starts_with("No formatter for rust files")), "{:?}", warnings(&app));
            app.lsp_sync();
            assert!(server.drain().iter().all(|m| m["method"] != "textDocument/formatting"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Answer the Save As prompt for the active buffer with `to`.
        fn save_as(app: &mut App, to: &str) {
            act(app, EditorAction::SaveAs);
            let d = app.dialog.take().expect("Save As prompt");
            app.finish_dialog(d.id, crate::ui::dialog::DialogResult::Text(to.into()));
        }

        #[tokio::test]
        async fn save_as_formats_through_the_new_documents_server() {
            let (mut app, mut server, dir, _) = setup("format-lsp-save-as", true).await;
            act(&mut app, EditorAction::InsertText(" ".into()));
            act(&mut app, EditorAction::Backspace);
            save_as(&mut app, "b.rs");
            let b = app.workspace.root.join("b.rs");
            assert_eq!(std::fs::read_to_string(&b).unwrap(), UGLY, "written first");
            let close = server.recv("textDocument/didClose").await;
            assert!(close["params"]["textDocument"]["uri"].as_str().unwrap().ends_with("/a.rs"));
            let open = server.recv("textDocument/didOpen").await;
            assert!(open["params"]["textDocument"]["uri"].as_str().unwrap().ends_with("/b.rs"));
            let req = server.recv("textDocument/formatting").await;
            assert!(req["params"]["textDocument"]["uri"].as_str().unwrap().ends_with("/b.rs"));
            server.respond(req["id"].as_i64().unwrap(), edits());
            handle_next(&mut app, is_lsp).await;
            assert_eq!(std::fs::read_to_string(&b).unwrap(), PRETTY);
            assert_eq!(text(&app), PRETTY);
            assert!(!app.editor.active_buffer().unwrap().dirty());
            assert!(warnings(&app).is_empty(), "{:?}", warnings(&app));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn save_as_cancels_the_servers_format_in_flight() {
            let (mut app, mut server, dir, path) = setup("format-lsp-save-as-cancel", true).await;
            act(&mut app, EditorAction::InsertText(" ".into()));
            act(&mut app, EditorAction::Backspace);
            act(&mut app, EditorAction::Save);
            let first = server.recv("textDocument/formatting").await;
            save_as(&mut app, "b.rs");
            assert_eq!(server.recv("$/cancelRequest").await["params"]["id"], first["id"]);
            let second = server.recv("textDocument/formatting").await;
            assert!(second["params"]["textDocument"]["uri"].as_str().unwrap().ends_with("/b.rs"));
            // The late answer for a.rs changes nothing.
            server.respond(first["id"].as_i64().unwrap(), edits());
            handle_next(&mut app, is_lsp).await;
            let b = app.workspace.root.join("b.rs");
            assert_eq!(text(&app), UGLY);
            assert_eq!(std::fs::read_to_string(&b).unwrap(), UGLY);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), UGLY);
            // No answer for b.rs: the file stays as Save As wrote it.
            std::fs::remove_file(&b).unwrap();
            let late = Duration::from_millis(app.config.lsp.request_timeout_ms);
            app.format_timeout(Instant::now() + late);
            assert_eq!(warnings(&app), ["Format b.rs: rust-analyzer: no answer in 5 s; saved unformatted"]);
            assert!(!b.exists(), "not written again");
            assert!(!app.editor.active_buffer().unwrap().dirty());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
