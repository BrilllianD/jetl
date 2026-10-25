//! The Tasks window in the app: the tracker is wanted the first time the
//! window renders, the panel's rows follow the service, and the selected
//! task's details are loaded when the selection changes
//! (`yman/panel.rs` holds the view, `yman/mod.rs` the data).

use super::{App, Ctx};
use crate::actions::TasksAction;
use crate::ui::dialog::{DialogId, DialogResult};
use crate::ui::focus::{PanelId, ToolWindowId};

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

    /// The answers of the Tasks window's choice dialogs.
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
}
