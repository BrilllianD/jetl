//! The Tasks window in the app: the tracker is wanted the first time the
//! window renders, the panel's rows follow the service, and the selected
//! task's details are loaded when the selection changes
//! (`yman/panel.rs` holds the view, `yman/mod.rs` the data).

use super::{App, Ctx};
use crate::actions::TasksAction;
use crate::event::Level;
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::focus::{PanelId, ToolWindowId};
use crate::yman::cli::{self, StatusChange};
use crate::yman::panel::parse_tags;
use crate::yman::{Mutation, MutationDone, MutationKind, TrackerState};

/// The verb of a status change for dialog titles and labels.
fn verb(change: &StatusChange) -> String {
    match change {
        StatusChange::Start => "Start".into(),
        StatusChange::Done => "Done".into(),
        StatusChange::Cancel => "Cancel".into(),
        StatusChange::Reopen => "Reopen".into(),
        StatusChange::Move(s) => format!("Move to {s}"),
    }
}

impl App {
    /// Called from the render arm of the window: wants the tracker, syncs the
    /// rows and asks for the selected task's details.
    pub(crate) fn sync_tasks(&mut self) {
        if !self.yman.is_wanted() {
            self.yman.want();
        }
        self.tasks.sync(&self.yman);
        if let Some(id) = self.tasks.selected_id()
            && self.tasks.requested.as_deref() != Some(id)
        {
            let id = id.to_string();
            self.yman.load_task(&id);
            self.tasks.requested = Some(id);
        }
    }

    pub(super) fn perform_tasks(&mut self, a: TasksAction) {
        if a == TasksAction::SyncTracker {
            self.run_tracker_sync();
            return;
        }
        if a == TasksAction::Refresh {
            if !self.yman.is_wanted() {
                self.yman.want();
            }
            self.yman.refresh();
            return;
        }
        let mut cx = ctx!(self);
        self.tasks.handle_action(&a, &self.yman, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
        if self.layout.is_visible(ToolWindowId::Tasks) {
            self.sync_tasks();
        }
    }

    /// Start a yman write; tells the user when another one still runs.
    pub(super) fn run_yman_mutation(&mut self, m: Mutation) {
        if !self.yman.mutate(m) {
            let busy = self.yman.busy().unwrap_or_default().to_string();
            self.notify(Level::Info, format!("Wait for {busy} to finish"));
        }
    }

    /// `yman sync` through the mutation runner; refused when the loaded
    /// status says the tracker has no remote.
    pub(super) fn run_tracker_sync(&mut self) {
        if self.yman.local_only() {
            self.notify(
                Level::Info,
                "Tasks are local only (no \"origin\" remote); connect one with yman init --remote <url> in a terminal",
            );
            return;
        }
        self.run_yman_mutation(Mutation { kind: MutationKind::Sync, label: "Sync tasks".into(), args: vec!["sync".into()], input: None });
    }

    /// Report a finished yman write: an error, or yman's notes and warnings.
    pub(super) fn finish_yman_mutation(&mut self, done: MutationDone) {
        match &done.result {
            Err(e) => self.notify(Level::Error, format!("{}: {}", done.label, e.message())),
            Ok(out) => {
                let err = out.stderr.trim();
                let warn = err.lines().any(|l| {
                    let l = l.trim_start();
                    l.starts_with("warning:") || l.starts_with("error:")
                });
                match done.kind {
                    MutationKind::Task(_) => {
                        if !err.is_empty() {
                            self.notify(if warn { Level::Warn } else { Level::Info }, format!("{}: {err}", done.label));
                        }
                    }
                    // One toast: the summary, raised to a warning when yman
                    // warned; its plain notes are dropped.
                    MutationKind::Sync => {
                        let text = String::from_utf8_lossy(&out.stdout);
                        let line = text.lines().rev().map(str::trim).find(|l| !l.is_empty());
                        let summary = line.map_or_else(|| "done".to_string(), |l| l.split_whitespace().collect::<Vec<_>>().join(" "));
                        if warn {
                            self.notify(
                                Level::Warn,
                                format!("{}: {summary} ({})", done.label, err.split_whitespace().collect::<Vec<_>>().join(" ")),
                            );
                        } else {
                            self.notify(Level::Info, format!("{}: {summary}", done.label));
                        }
                    }
                }
            }
        }
    }

    /// Ask for the optional note of a status change.
    fn open_note(&mut self, task: &str, change: StatusChange) {
        let v = verb(&change);
        let ok = if matches!(change, StatusChange::Move(_)) { "Move".to_string() } else { v.clone() };
        self.dialog = Some(Dialog::input(
            DialogId::TasksNote(task.to_string(), change),
            &format!("{v} #{task}"),
            "Optional note for the discussion (Enter skips)",
            "",
            &ok,
        ));
    }

    fn task_assignee(&self, id: &str) -> String {
        match &self.yman.state {
            TrackerState::Ready(s) => {
                s.tasks.iter().find(|t| t.id == id).and_then(|t| t.assignee.as_deref()).unwrap_or("").trim().to_string()
            }
            _ => String::new(),
        }
    }

    fn task_mutation(&mut self, id: &str, label: String, args: Vec<String>, input: Option<Vec<u8>>) {
        self.run_yman_mutation(Mutation { kind: MutationKind::Task(id.to_string()), label, args, input });
    }

    /// The answers of the Tasks window's dialogs.
    pub(super) fn finish_tasks_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        let pick = |values: &[String]| match res {
            DialogResult::Choice(i) => values.get(*i).cloned(),
            _ => None,
        };
        match id {
            DialogId::TasksTag(values) => {
                if let Some(v) = pick(values) {
                    self.tasks.set_tag(Some(v).filter(|s| !s.is_empty()), &self.yman);
                }
            }
            DialogId::TasksAssignee(values) => {
                if let Some(v) = pick(values) {
                    self.tasks.set_assignee(Some(v).filter(|s| !s.is_empty()), &self.yman);
                }
            }
            DialogId::TasksJump(values) => {
                if let Some(v) = pick(values) {
                    let mut cx = ctx!(self);
                    self.tasks.jump(&v, &self.yman, &mut cx);
                    let fx = cx.effects;
                    self.apply_effects(fx);
                    self.focus.set(PanelId::Tasks);
                }
            }
            DialogId::TasksStatus(task, values) => {
                if let DialogResult::Choice(i) = res {
                    match values.get(*i) {
                        Some(Some(change)) => self.open_note(task, change.clone()),
                        Some(None) => {
                            self.dialog = Some(Dialog::input(
                                DialogId::TasksMoveTo(task.clone()),
                                &format!("Move #{task}"),
                                "Status to move the task to",
                                "",
                                "Next",
                            ));
                        }
                        None => {}
                    }
                }
            }
            DialogId::TasksMoveTo(task) => {
                if let DialogResult::Text(s) = res
                    && !s.trim().is_empty()
                {
                    self.open_note(task, StatusChange::Move(s.trim().into()));
                }
            }
            DialogId::TasksNote(task, change) => {
                if let DialogResult::Text(note) = res {
                    let label = match change {
                        StatusChange::Move(s) => format!("Move #{task} to {s}"),
                        c => format!("{} #{task}", verb(c)),
                    };
                    self.task_mutation(task, label, cli::status_change_args(task, change, Some(note)), None);
                }
            }
            DialogId::TasksPriority(task) => {
                if let DialogResult::Choice(i) = res
                    && *i <= 9
                {
                    self.task_mutation(task, format!("Priority #{task}"), cli::priority_args(task, *i as u8), None);
                }
            }
            DialogId::TasksAssign(task) => {
                if let DialogResult::Text(s) = res {
                    let who = s.trim();
                    if who != self.task_assignee(task) {
                        let args = cli::assign_args(task, Some(who).filter(|w| !w.is_empty()));
                        self.task_mutation(task, format!("Assign #{task}"), args, None);
                    }
                }
            }
            DialogId::TasksTags(task, old) => {
                if let DialogResult::Text(s) = res {
                    let new = parse_tags(s);
                    let add: Vec<String> = new.iter().filter(|t| !old.contains(t)).cloned().collect();
                    let remove: Vec<String> = old.iter().filter(|t| !new.contains(t)).cloned().collect();
                    if !add.is_empty() || !remove.is_empty() {
                        self.task_mutation(task, format!("Tags #{task}"), cli::tag_change_args(task, &add, &remove), None);
                    }
                }
            }
            DialogId::TasksComment(task) => {
                if let DialogResult::Text(s) = res {
                    let text = s.trim();
                    if text.is_empty() {
                        self.notify(Level::Info, "The comment is empty");
                    } else {
                        self.task_mutation(task, format!("Comment #{task}"), cli::comment_args(task), Some(text.as_bytes().to_vec()));
                    }
                }
            }
            _ => return false,
        }
        if self.layout.is_visible(ToolWindowId::Tasks) {
            self.sync_tasks();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Action;
    use crate::app::tests::{handle_next, test_app};
    use crate::yman::panel::tests::{sample, set_snapshot};
    use crate::yman::{LoadedTask, TaskDetails, TrackerState, YmanError, YmanMsg};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn draw(app: &mut App, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let buf = term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn details(id: &str, body: &str) -> TaskDetails {
        TaskDetails {
            id: id.into(),
            priority: 2,
            status: "doing".into(),
            title: "Crash on save".into(),
            tags: vec!["bug".into()],
            assignee: None,
            links: vec![],
            related: vec!["1".into()],
            related_by: vec![],
            created: "2026-10-25T16:06:18Z".into(),
            updated: "2026-10-25T16:06:18Z".into(),
            dir: String::new(),
            body: body.into(),
            attachments: vec![],
            discussion: vec![],
            discussion_total: 0,
        }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn opening_the_window_wants_the_tracker() {
        let (mut app, dir) = test_app("tasks-open");
        app.yman.program = "jetl-no-such-yman-xyz".into();
        app.toggle_tool_window(ToolWindowId::Tasks);
        assert!(app.layout.is_visible(ToolWindowId::Tasks) && app.focus.is(PanelId::Tasks));
        assert!(!app.yman.is_wanted());
        draw(&mut app, 100, 30);
        assert!(app.yman.is_wanted());
        handle_next(&mut app, |m| matches!(m, crate::event::AppMsg::Yman(YmanMsg::Snapshot { .. }))).await;
        assert_eq!(app.yman.state, TrackerState::Unavailable(YmanError::NoBinary));
        let text = draw(&mut app, 100, 30);
        assert!(text.contains("yman is not installed"), "{text}");
        assert!(text.contains("F5 retries"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn selection_loads_details_and_keys_move_it() {
        let (mut app, dir) = test_app("tasks-details");
        app.yman.program = "jetl-no-such-yman-xyz".into();
        app.toggle_tool_window(ToolWindowId::Tasks);
        set_snapshot(&mut app.yman, sample());
        draw(&mut app, 120, 30);
        assert_eq!(app.tasks.requested.as_deref(), Some("1"));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.tasks.requested.as_deref(), Some("2"));
        app.yman.task = Some(LoadedTask { id: "2".into(), result: Ok(details("2", "hello body")) });
        app.yman.revision += 1;
        let text = draw(&mut app, 120, 30);
        assert!(text.contains("hello body"), "{text}");
        assert!(text.contains("#2 Crash on save"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tag_dialog_sets_the_filter() {
        let (mut app, dir) = test_app("tasks-tag");
        app.yman.program = "jetl-no-such-yman-xyz".into();
        app.toggle_tool_window(ToolWindowId::Tasks);
        set_snapshot(&mut app.yman, sample());
        draw(&mut app, 120, 30);
        app.perform(Action::Tasks(TasksAction::FilterTag));
        let d = app.dialog.take().expect("tag dialog");
        let DialogId::TasksTag(values) = d.id.clone() else { panic!("{:?}", d.id) };
        assert_eq!(values[0], "");
        let ui = values.iter().position(|v| v == "ui").unwrap();
        app.finish_dialog(d.id, DialogResult::Choice(ui));
        let text = draw(&mut app, 120, 30);
        assert!(text.contains("[Tag: ui]"), "{text}");
        assert!(text.contains("Tasks: 2 of 3"), "{text}");
        assert!(text.contains("Fix the parser") && text.contains("Polish the toolbar"), "{text}");
        assert!(!text.contains("Crash on save") && !text.contains("Old chore"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn jump_reveals_a_closed_task() {
        let (mut app, dir) = test_app("tasks-jump");
        app.yman.program = "jetl-no-such-yman-xyz".into();
        app.toggle_tool_window(ToolWindowId::Tasks);
        set_snapshot(&mut app.yman, sample());
        draw(&mut app, 120, 30);
        for c in "bug".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let mut cx = ctx!(app);
        assert!(app.tasks.jump("3", &app.yman, &mut cx));
        assert!(!app.tasks.jump("99", &app.yman, &mut cx));
        let fx = cx.effects;
        app.apply_effects(fx);
        assert_eq!(app.tasks.selected_id(), Some("3"));
        let state = app.tasks.session_state();
        assert!(state.filter.is_empty() && state.show_closed);
        assert!(app.notifications.last().is_some_and(|n| n.text == "Task 99 is not in the list"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn real_tracker_lists_open_tasks() {
        use crate::yman::cli::{PROGRAM, yman_available};
        if !yman_available() {
            eprintln!("skipping: yman not on PATH");
            return;
        }
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let (mut app, dir) = test_app("tasks-real");
        let _cleanup = Cleanup(dir);
        let root = app.workspace.root.clone();
        let run = |cmd: &str, args: &[&str]| {
            let out = std::process::Command::new(cmd)
                .args(args)
                .current_dir(&root)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run("git", &["init", "-q"]);
        crate::git::repo::tests::isolate(&root);
        run("git", &["config", "user.name", "t"]);
        run("git", &["config", "user.email", "t@t"]);
        run(PROGRAM, &["init", "--offline"]);
        run(PROGRAM, &["add", "A"]);
        run(PROGRAM, &["add", "B"]);
        run(PROGRAM, &["done", "2"]);
        app.toggle_tool_window(ToolWindowId::Tasks);
        draw(&mut app, 100, 30);
        handle_next(&mut app, |m| matches!(m, crate::event::AppMsg::Yman(YmanMsg::Snapshot { .. }))).await;
        app.tasks.sync(&app.yman);
        assert_eq!(app.tasks.session_state().selected.as_deref(), Some("1"));
        let text = draw(&mut app, 100, 30);
        assert!(text.contains("Tasks: 1 open"), "{text}");
        app.perform(Action::Tasks(TasksAction::ToggleClosed));
        let text = draw(&mut app, 100, 30);
        assert!(text.contains("Tasks: 2 "), "{text}");
    }

    use crate::event::Level;
    use crate::ui::dialog::DialogKind;
    use crate::yman::cli::StatusChange;
    use crate::yman::panel::tests::snap;

    fn tasks_app(name: &str) -> (App, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        app.yman.program = "jetl-no-such-yman-xyz".into();
        app.toggle_tool_window(ToolWindowId::Tasks);
        set_snapshot(&mut app.yman, sample());
        draw(&mut app, 120, 30);
        (app, dir)
    }

    fn take_dialog(app: &mut App) -> crate::ui::dialog::Dialog {
        app.dialog.take().expect("a dialog")
    }

    fn running_args(app: &App) -> Vec<String> {
        app.yman.running.as_ref().expect("a running mutation").args.clone()
    }

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn status_change_asks_for_a_note_then_runs() {
        let (mut app, dir) = tasks_app("tasks-status");
        app.perform(Action::Tasks(TasksAction::ChangeStatus));
        let d = take_dialog(&mut app);
        let DialogId::TasksStatus(task, v) = d.id.clone() else { panic!("{:?}", d.id) };
        assert_eq!(task, "1");
        assert_eq!(v[..3], [Some(StatusChange::Start), Some(StatusChange::Done), Some(StatusChange::Cancel)]);
        assert_eq!(v.last(), Some(&None));
        assert!(v.contains(&Some(StatusChange::Move("doing".into()))));
        assert!(!v.contains(&Some(StatusChange::Move("todo".into()))));
        app.finish_dialog(d.id, DialogResult::Choice(1));
        let d = take_dialog(&mut app);
        assert_eq!(d.id, DialogId::TasksNote("1".into(), StatusChange::Done));
        app.finish_dialog(d.id, DialogResult::Text("shipped".into()));
        assert_eq!(running_args(&app), strs(&["done", "--message=shipped", "1"]));
        assert_eq!(app.yman.busy(), Some("Done #1"));
        assert!(draw(&mut app, 120, 30).contains("· Done #1…"));
        app.perform(Action::Tasks(TasksAction::ChangePriority));
        assert!(app.dialog.is_none());
        assert_eq!(app.notifications.last().map(|n| n.text.as_str()), Some("Wait for Done #1 to finish"));
        handle_next(&mut app, |m| matches!(m, crate::event::AppMsg::Yman(YmanMsg::Mutation { .. }))).await;
        assert_eq!(app.yman.busy(), None);
        let n = app.notifications.last().unwrap();
        assert_eq!(n.level, Level::Error);
        assert!(n.text.starts_with("Done #1: yman is not installed"), "{}", n.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn move_to_typed_status() {
        let (mut app, dir) = tasks_app("tasks-move");
        app.perform(Action::Tasks(TasksAction::ChangeStatus));
        let d = take_dialog(&mut app);
        let DialogId::TasksStatus(_, v) = d.id.clone() else { panic!("{:?}", d.id) };
        app.finish_dialog(d.id, DialogResult::Choice(v.len() - 1));
        let d = take_dialog(&mut app);
        assert_eq!(d.id, DialogId::TasksMoveTo("1".into()));
        app.finish_dialog(d.id, DialogResult::Text("blocked".into()));
        let d = take_dialog(&mut app);
        assert_eq!(d.id, DialogId::TasksNote("1".into(), StatusChange::Move("blocked".into())));
        app.finish_dialog(d.id, DialogResult::Text(String::new()));
        assert_eq!(running_args(&app), strs(&["move", "1", "blocked"]));
        assert_eq!(app.yman.busy(), Some("Move #1 to blocked"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn priority_dialog_preselects_the_current_one() {
        let (mut app, dir) = tasks_app("tasks-prio");
        app.perform(Action::Tasks(TasksAction::ChangePriority));
        let d = take_dialog(&mut app);
        assert_eq!(d.id, DialogId::TasksPriority("1".into()));
        let DialogKind::Choice { list, .. } = &d.kind else { panic!("not a choice") };
        assert_eq!(list.selected, 2);
        app.finish_dialog(d.id, DialogResult::Choice(0));
        assert_eq!(running_args(&app), strs(&["prio", "1", "0"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn assign_tags_and_comment_build_their_commands() {
        let (mut app, dir) = tasks_app("tasks-fields");
        app.finish_dialog(DialogId::TasksAssign("1".into()), DialogResult::Text("claude".into()));
        assert!(app.yman.running.is_none(), "1 is already claude's");
        app.finish_dialog(DialogId::TasksAssign("1".into()), DialogResult::Text(String::new()));
        assert_eq!(running_args(&app), strs(&["set", "--no-assignee", "1"]));
        app.yman.running = None;
        app.finish_dialog(DialogId::TasksAssign("1".into()), DialogResult::Text(" bob ".into()));
        assert_eq!(running_args(&app), strs(&["set", "--assignee=bob", "1"]));
        app.yman.running = None;
        app.perform(Action::Tasks(TasksAction::EditTags));
        let d = take_dialog(&mut app);
        assert_eq!(d.id, DialogId::TasksTags("1".into(), vec!["ui".into()]));
        app.finish_dialog(d.id.clone(), DialogResult::Text("ui".into()));
        assert!(app.yman.running.is_none());
        app.finish_dialog(d.id, DialogResult::Text("ui, bug".into()));
        assert_eq!(running_args(&app), strs(&["set", "--tag=bug", "1"]));
        app.yman.running = None;
        app.finish_dialog(DialogId::TasksComment("1".into()), DialogResult::Text("  hi  ".into()));
        assert_eq!(running_args(&app), strs(&["comment", "1"]));
        assert_eq!(app.yman.running.as_ref().unwrap().input, Some(b"hi".to_vec()));
        app.yman.running = None;
        app.finish_dialog(DialogId::TasksComment("1".into()), DialogResult::Text("  ".into()));
        assert!(app.yman.running.is_none());
        assert_eq!(app.notifications.last().map(|n| n.text.as_str()), Some("The comment is empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_selection_refuses() {
        let (mut app, dir) = tasks_app("tasks-none");
        set_snapshot(&mut app.yman, snap(vec![]));
        draw(&mut app, 120, 30);
        app.perform(Action::Tasks(TasksAction::ChangeStatus));
        assert!(app.dialog.is_none());
        assert_eq!(app.notifications.last().map(|n| n.text.as_str()), Some("No task selected"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sync_runs_through_the_runner() {
        let (mut app, dir) = tasks_app("tasks-sync");
        app.perform(Action::Tasks(TasksAction::SyncTracker));
        assert_eq!(running_args(&app), strs(&["sync"]));
        assert_eq!(app.yman.busy(), Some("Sync tasks"));
        assert!(draw(&mut app, 120, 30).contains("· Sync tasks…"));
        handle_next(&mut app, |m| matches!(m, crate::event::AppMsg::Yman(YmanMsg::Mutation { .. }))).await;
        assert_eq!(app.yman.busy(), None);
        let n = app.notifications.last().unwrap();
        assert_eq!(n.level, Level::Error);
        assert!(n.text.starts_with("Sync tasks: yman is not installed"), "{}", n.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sync_refuses_without_a_remote() {
        let (mut app, dir) = tasks_app("tasks-sync-local");
        let mut s = sample();
        s.status = Some(serde_json::from_str(r#"{"remote":{"origin":false}}"#).unwrap());
        set_snapshot(&mut app.yman, s);
        app.perform(Action::Tasks(TasksAction::SyncTracker));
        assert!(app.yman.running.is_none());
        let n = app.notifications.last().unwrap();
        assert!(n.text.contains("local only") && n.text.contains("yman init --remote"), "{}", n.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sync_reports_the_summary_and_merge_conflicts() {
        use crate::yman::MutationDone;
        use crate::yman::cli::YmanOutput;
        let (mut app, dir) = tasks_app("tasks-sync-done");
        let stdout = b"synced  pulled 1, pushed 0, renumbered 0  refs/yman/local @ abc1234\n".to_vec();
        let result = Ok(YmanOutput { code: Some(0), stdout, stderr: String::new() });
        app.finish_yman_mutation(MutationDone { kind: MutationKind::Sync, label: "Sync tasks".into(), result });
        let n = app.notifications.last().unwrap();
        assert_eq!(n.level, Level::Info);
        assert_eq!(n.text, "Sync tasks: synced pulled 1, pushed 0, renumbered 0 refs/yman/local @ abc1234");
        let line = "conflicts in 1 file(s); edit them, remove markers, then: yman sync --continue  (or: yman sync --abort)";
        let result = Err(YmanError::MergeUnresolved(line.into()));
        app.finish_yman_mutation(MutationDone { kind: MutationKind::Sync, label: "Sync tasks".into(), result });
        let n = app.notifications.last().unwrap();
        assert_eq!(n.level, Level::Error);
        assert_eq!(n.text, "Sync tasks: Sync merge unresolved: run yman sync --continue or --abort");
        assert!(n.text.len() <= 80, "{}", n.text);
        let base = app.notifications.len();
        let stdout = b"synced  pulled 0, pushed 1\n".to_vec();
        let result = Ok(YmanOutput { code: Some(0), stdout: stdout.clone(), stderr: "note: fetched\n".into() });
        app.finish_yman_mutation(MutationDone { kind: MutationKind::Sync, label: "Sync tasks".into(), result });
        assert_eq!(app.notifications.len(), base + 1);
        assert_eq!(app.notifications.last().unwrap().text, "Sync tasks: synced pulled 0, pushed 1");
        let result = Ok(YmanOutput { code: Some(0), stdout, stderr: "warning: slow remote\n".into() });
        app.finish_yman_mutation(MutationDone { kind: MutationKind::Sync, label: "Sync tasks".into(), result });
        assert_eq!(app.notifications.len(), base + 2);
        let n = app.notifications.last().unwrap();
        assert_eq!(n.level, Level::Warn);
        assert_eq!(n.text, "Sync tasks: synced pulled 0, pushed 1 (warning: slow remote)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
