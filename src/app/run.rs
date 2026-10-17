//! Run tasks (`run.pick`, `run.rerun`, `run.stop`): one at a time, each in
//! its own process group, output streamed into the Run window. Starting a
//! task while one runs kills the old one first; quitting and switching
//! roots kill it too, and so does dropping the app.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::App;
use crate::actions::PaletteKind;
use crate::event::{AppMsg, Level};
use crate::run::matcher::{Matcher, RunLine};
use crate::run::panel::Status;
use crate::run::process::{self, RunEnd, Spec};
use crate::run::{MatcherKind, RunTask};
use crate::ui::focus::{PanelId, ToolWindowId};
use crate::ui::palette::Source;

#[derive(Debug, Default)]
pub struct RunState {
    /// Runs started so far; output of any but the current one is dropped.
    pub(crate) seq: u64,
    job: Option<Job>,
    /// What `run.rerun` starts.
    pub(crate) last: Option<RunTask>,
    /// The picker's tasks from the last load, and the loads' order.
    pub(crate) tasks: Vec<RunTask>,
    tasks_requested: u64,
    tasks_seq: u64,
    /// The Run window's selected configuration, by name: the last one
    /// picked, kept in the session.
    pub(crate) selected: Option<String>,
    /// A task list has arrived for this root.
    tasks_loaded: bool,
    /// `run.run_selected` waits for the list.
    run_selected_pending: bool,
}

#[derive(Debug)]
struct Job {
    run: u64,
    name: String,
    /// The process group; 0 once its leader was reaped.
    pid: Arc<AtomicU32>,
    task: tokio::task::AbortHandle,
    /// `run.stop` killed it.
    stopped: bool,
}

impl Job {
    fn kill(&self) {
        process::kill_group(self.pid.load(Ordering::SeqCst));
    }
}

impl RunState {
    pub fn running(&self) -> bool {
        self.job.is_some()
    }

    /// The running task's process group id, if any.
    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        self.job.as_ref().map(|j| j.pid.load(Ordering::SeqCst)).filter(|p| *p != 0)
    }

    /// Kill the running task's process group and forget it: its remaining
    /// output and exit are dropped (quit, root switch, a new run).
    pub(crate) fn kill(&mut self) {
        if let Some(job) = self.job.take() {
            job.kill();
            job.task.abort();
        }
    }
}

impl Drop for RunState {
    fn drop(&mut self) {
        self.kill();
    }
}

impl App {
    /// Start `task` in the Run window, killing a task that still runs.
    pub(crate) fn run_task(&mut self, task: RunTask) {
        self.run.kill();
        let root = self.workspace.root.clone();
        let cwd = match task.cwd.as_deref() {
            Some(d) if !d.is_empty() => root.join(d),
            _ => root.clone(),
        };
        self.run_panel.start(&task.name, &task.display());
        self.show_run_window();
        self.run.last = Some(task.clone());
        let argv = task.argv();
        let matcher_kind = task.matcher_kind();
        let spec = Spec { argv, cwd: cwd.clone(), env: task.env.into_iter().collect() };
        tracing::debug!("running {:?} in {}", spec.argv, cwd.display());
        let started = if cwd.is_dir() {
            process::spawn(&spec).map_err(|e| format!("cannot run {}: {e}", spec.argv.first().map_or("", |s| s.as_str())))
        } else {
            Err(format!("{} is not a directory", cwd.display()))
        };
        let spawned = match started {
            Ok(s) => s,
            Err(e) => {
                self.run_panel.finish(Status::Failed(e.clone()));
                self.notify(Level::Error, format!("Run {}: {e}", task.name));
                return;
            }
        };
        self.run.seq += 1;
        let run = self.run.seq;
        let pid = Arc::new(AtomicU32::new(spawned.pid));
        let matcher = match matcher_kind {
            MatcherKind::Plain => None,
            _ => Some(Matcher::new(&cwd, &root)),
        };
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        let handle = tokio::spawn(process::drive(spawned, matcher, generation, run, tx, pid.clone()));
        self.run.job = Some(Job { run, name: task.name, pid, task: handle.abort_handle(), stopped: false });
    }

    /// Show the Run window; it takes the focus only from the window it
    /// replaces.
    fn show_run_window(&mut self) {
        if self.layout.is_visible(ToolWindowId::Run) {
            return;
        }
        self.layout.show(ToolWindowId::Run);
        if self.focus.current.tool_window().is_some_and(|tw| !self.layout.is_visible(tw)) {
            self.focus.set(PanelId::Run);
        }
    }

    /// `run.rerun`: the last task, or the picker when nothing ran yet.
    pub(super) fn rerun(&mut self) {
        match self.run.last.clone() {
            Some(task) => self.run_task(task),
            None => self.open_palette(PaletteKind::RunTasks),
        }
    }

    /// `run.stop`: kill the task's process group; the exit still arrives.
    pub(super) fn stop_run(&mut self) {
        match &mut self.run.job {
            Some(job) => {
                job.stopped = true;
                job.kill();
            }
            None => self.notify(Level::Info, "No task is running"),
        }
    }

    fn current_run(&self, generation: u64, run: u64) -> bool {
        generation == self.workspace.generation && self.run.job.as_ref().is_some_and(|j| j.run == run)
    }

    pub(super) fn run_output(&mut self, generation: u64, run: u64, lines: Vec<RunLine>) {
        if !self.current_run(generation, run) {
            tracing::debug!("dropping output of an old run");
            return;
        }
        self.run_panel.push(lines);
    }

    /// A task ended: show its status, and notify once when it failed.
    pub(super) fn run_exited(&mut self, generation: u64, run: u64, end: RunEnd) {
        if !self.current_run(generation, run) {
            tracing::debug!("dropping the exit of an old run");
            return;
        }
        let Some(job) = self.run.job.take() else { return };
        let status = match end {
            RunEnd::Code(c) => Status::Exited(c),
            RunEnd::Signal(_) if job.stopped => Status::Stopped,
            RunEnd::Signal(s) => Status::Signal(s),
            RunEnd::Failed(e) => Status::Failed(e),
        };
        let name = &job.name;
        match &status {
            Status::Exited(0) | Status::Stopped | Status::Running => {}
            Status::Exited(c) => self.notify(Level::Error, format!("Run {name} failed with exit code {c}")),
            Status::Signal(s) => self.notify(Level::Error, format!("Run {name} was killed by signal {s}")),
            Status::Failed(e) => self.notify(Level::Error, format!("Run {name}: {e}")),
        }
        self.run_panel.finish(status);
    }

    /// Load the picker's tasks off the main thread (they read project files).
    pub(crate) fn load_run_tasks(&mut self) {
        self.run.tasks_requested += 1;
        let seq = self.run.tasks_requested;
        let (root, generation, tx) = (self.workspace.root.clone(), self.workspace.generation, self.bus_tx.clone());
        let configured = self.config.run.all();
        tokio::task::spawn_blocking(move || {
            let (tasks, error) = crate::run::load_tasks(&root, &configured);
            let _ = tx.send(AppMsg::RunTasks { generation, seq, tasks, error });
        });
    }

    pub(super) fn run_tasks_loaded(&mut self, generation: u64, seq: u64, tasks: Vec<RunTask>, error: Option<String>) {
        if generation != self.workspace.generation || seq <= self.run.tasks_seq {
            tracing::debug!("dropping a stale task list");
            return;
        }
        self.run.tasks_seq = seq;
        self.run.tasks_loaded = true;
        self.run.tasks = tasks;
        if let Some(e) = error {
            self.notify(Level::Warn, e);
        }
        let loading = seq < self.run.tasks_requested;
        if let Some(p) = self.palette.as_mut().filter(|p| p.wants(Source::Tasks)) {
            p.set_tasks(self.run.tasks.clone(), loading);
        }
        if seq == self.run.tasks_requested && std::mem::take(&mut self.run.run_selected_pending) {
            self.run_selected();
        }
    }

    /// `run.run_selected`: the selected configuration; the picker when none
    /// is selected or it no longer exists. Waits for the list when it has
    /// not loaded yet.
    pub(super) fn run_selected(&mut self) {
        let Some(name) = self.run.selected.clone() else { return self.open_palette(PaletteKind::RunTasks) };
        if !self.run.tasks_loaded {
            self.run.run_selected_pending = true;
            self.load_run_tasks();
            return;
        }
        match self.run.tasks.iter().find(|t| t.name == name).cloned() {
            Some(t) => self.run_task(t),
            None => {
                self.notify(Level::Warn, format!("Run configuration '{name}' no longer exists"));
                self.open_palette(PaletteKind::RunTasks);
            }
        }
    }

    /// A root switch: the task belongs to the old root.
    pub(crate) fn run_root_changed(&mut self) {
        self.run.kill();
        self.run.last = None;
        self.run.tasks.clear();
        self.run.selected = None;
        self.run.tasks_loaded = false;
        self.run.run_selected_pending = false;
        self.run_configs = None;
        self.run_panel.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::actions::{Action, PaletteAction, RunAction};
    use crate::app::tests::{handle_next, test_app};
    use crate::files::watch::Changes;
    use crate::run::matcher::Severity;
    #[cfg(target_os = "linux")]
    use crate::util::process_gone;

    fn shell(name: &str, script: &str) -> RunTask {
        let text = format!("[[run.tasks]]\nname = {name:?}\ncommand = {script:?}\n");
        crate::config::Config::parse(&text).unwrap().run.tasks.remove(0)
    }

    /// Handle run messages until the current run's exit.
    async fn until_exit(app: &mut App) {
        while app.run.running() {
            handle_next(app, |m| matches!(m, AppMsg::RunOutput { .. } | AppMsg::RunExited { .. })).await;
        }
    }

    fn errors(app: &App) -> Vec<String> {
        app.notifications.iter().filter(|n| n.level == Level::Error).map(|n| n.text.clone()).collect()
    }

    /// Wait until `file` has a line and return it.
    async fn read_pid(file: &std::path::Path) -> u32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(s) = std::fs::read_to_string(file)
                && s.ends_with('\n')
            {
                return s.trim().parse().unwrap();
            }
            assert!(std::time::Instant::now() < deadline, "no pid in {}", file.display());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn picked_task_streams_output_fails_once_and_jumps_to_the_problem() {
        let (mut app, dir) = test_app("run-task");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        app.config.run.tasks = vec![shell("check", r#"printf 'Compiling\nerror: x\n --> src/a.rs:2:3\n' >&2; echo done; exit 1"#)];
        app.perform(Action::App(crate::actions::AppAction::OpenPalette(PaletteKind::RunTasks)));
        handle_next(&mut app, |m| matches!(m, AppMsg::RunTasks { .. })).await;
        let res = app.palette.as_mut().unwrap().handle_action(&PaletteAction::Accept).unwrap();
        app.finish_palette(res);
        assert!(app.layout.is_visible(ToolWindowId::Run));
        until_exit(&mut app).await;
        let texts: Vec<&str> = app.run_panel.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Compiling", "error: x", " --> src/a.rs:2:3", "done"], "stderr and stdout in order");
        let problem = &app.run_panel.lines[2];
        assert_eq!(problem.severity, Some(Severity::Error));
        let link = problem.link.as_ref().unwrap();
        assert_eq!((link.path.clone(), link.line, link.col), (root.join("src/a.rs"), 1, 2));
        assert_eq!(app.run_panel.status(), Some(&Status::Exited(1)));
        assert_eq!(errors(&app), ["Run check failed with exit code 1"]);
        // The next problem opens the file at its line and column.
        app.perform(Action::Run(RunAction::NextProblem));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.path.as_deref(), Some(root.join("src/a.rs").as_path()));
        assert_eq!(b.cursor_line_col(), (1, 2));
        assert!(app.focus.is(PanelId::Editor));
        // Rerun runs it again; a passing run does not notify.
        app.run.last = Some(shell("check", "echo ok"));
        app.perform(Action::Run(RunAction::Rerun));
        until_exit(&mut app).await;
        assert_eq!(app.run_panel.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["ok"]);
        assert_eq!(app.run_panel.status(), Some(&Status::Exited(0)));
        assert_eq!(errors(&app).len(), 1);
        // A command that cannot start fails at once.
        app.run_task(RunTask { command: crate::run::TaskCommand::Argv(vec!["jetl-no-such-program".into()]), ..shell("bad", "") });
        assert!(!app.run.running());
        assert!(matches!(app.run_panel.status(), Some(Status::Failed(e)) if e.starts_with("cannot run jetl-no-such-program")));
        assert_eq!(errors(&app).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stop_kills_the_process_group() {
        let (mut app, dir) = test_app("run-stop");
        let pid_file = dir.join("pid");
        app.run_task(shell("wait", &format!("echo started; sleep 30 & echo $! > '{}'; wait", pid_file.display())));
        let leader = app.run.pid().unwrap();
        let child = read_pid(&pid_file).await;
        // The first line arrives before the stop, so it cannot be cut off by
        // the exit report's drain timeout on a busy machine.
        while app.run_panel.lines.is_empty() {
            handle_next(&mut app, |m| matches!(m, AppMsg::RunOutput { .. })).await;
        }
        app.perform(Action::Run(RunAction::Stop));
        until_exit(&mut app).await;
        assert_eq!(app.run_panel.status(), Some(&Status::Stopped));
        assert_eq!(app.run_panel.lines.front().map(|l| l.text.as_str()), Some("started"));
        assert!(errors(&app).is_empty(), "a stop is no failure");
        #[cfg(target_os = "linux")]
        {
            assert!(process_gone(child).await, "sleep {child} still runs");
            assert!(process_gone(leader).await, "the shell {leader} still runs");
        }
        let _ = (leader, child);
        app.perform(Action::Run(RunAction::Stop));
        assert!(app.notifications.last().unwrap().text == "No task is running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_new_run_or_root_kills_the_old_one_and_drops_its_output() {
        let (mut app, dir) = test_app("run-stale");
        let (first, second) = (dir.join("pid1"), dir.join("pid2"));
        app.run_task(shell("one", &format!("sleep 30 & echo $! > '{}'; wait", first.display())));
        let old = app.run.seq;
        let child = read_pid(&first).await;
        app.run_task(shell("two", &format!("echo two; sleep 30 & echo $! > '{}'; wait", second.display())));
        #[cfg(target_os = "linux")]
        assert!(process_gone(child).await, "the old run's sleep {child} still runs");
        // Late output and exit of the old run, or of another root, are dropped.
        let g = app.workspace.generation;
        app.handle_msg(AppMsg::RunOutput { generation: g, run: old, lines: vec![RunLine::plain("old")] });
        app.handle_msg(AppMsg::RunOutput { generation: g - 1, run: app.run.seq, lines: vec![RunLine::plain("other root")] });
        app.handle_msg(AppMsg::RunExited { generation: g, run: old, end: RunEnd::Code(3) });
        assert!(app.run.running() && app.run_panel.running());
        assert!(app.run_panel.lines.iter().all(|l| l.text != "old" && l.text != "other root"));
        handle_next(&mut app, |m| matches!(m, AppMsg::RunOutput { .. })).await;
        assert_eq!(app.run_panel.lines.back().map(|l| l.text.as_str()), Some("two"));
        // A root switch kills the task and clears the window.
        let child = read_pid(&second).await;
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(!app.run.running() && app.run_panel.status().is_none() && app.run.last.is_none());
        #[cfg(target_os = "linux")]
        assert!(process_gone(child).await, "sleep {child} survived the root switch");
        let _ = child;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn config_with_args_env_and_cwd_runs_there() {
        let (mut app, dir) = test_app("run-config");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let command = vec!["sh", "-c", "pwd -P; printf '%s\\n' \"$FOO\" \"$@\"", "sh"];
        let text = format!(
            "[[run.configs]]\nname = \"where\"\ncommand = {command:?}\nargs = [\"a b\", \"c\"]\ncwd = \"sub\"\nenv = {{ FOO = \"bar\" }}\n"
        );
        let task = crate::config::Config::parse(&text).unwrap().run.configs.remove(0);
        let shown = task.display();
        app.run_task(task);
        until_exit(&mut app).await;
        let texts: Vec<String> = app.run_panel.lines.iter().map(|l| l.text.clone()).collect();
        let sub = std::fs::canonicalize(root.join("sub")).unwrap();
        assert_eq!(texts, [sub.display().to_string(), "bar".into(), "a b".into(), "c".into()]);
        assert_eq!(app.run_panel.command(), Some(shown.as_str()));
        assert!(shown.ends_with("sh 'a b' c"), "{shown}");
        assert_eq!(app.run_panel.status(), Some(&Status::Exited(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn matcher_none_shows_plain_lines() {
        let (mut app, dir) = test_app("run-plain");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn main() {}\n").unwrap();
        let script = "printf 'error: x\\n --> src/a.rs:1:1\\n'";
        let plain = RunTask { matcher: Some(crate::run::MatcherKind::Plain), ..shell("plain", script) };
        app.run_task(plain);
        until_exit(&mut app).await;
        assert_eq!(app.run_panel.lines.len(), 2);
        assert!(app.run_panel.lines.iter().all(|l| l.link.is_none() && l.severity.is_none()));
        app.run_task(shell("linked", script));
        until_exit(&mut app).await;
        assert!(app.run_panel.lines.iter().any(|l| l.link.is_some()), "the default matcher links the location");
        assert_eq!(app.run_panel.lines[0].severity, Some(Severity::Error));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn project_file_and_config_changes_reload_the_list() {
        let (mut app, dir) = test_app("run-reload");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join(".jetl")).unwrap();
        let file = crate::run::edit::project_file(&root);
        std::fs::write(&file, "[[configs]]\nname = \"x\"\ncommand = \"true\"\n").unwrap();
        let generation = app.workspace.generation;
        app.handle_msg(AppMsg::FsChanged { generation, changes: Changes { paths: vec![file], ..Default::default() } });
        handle_next(&mut app, |m| matches!(m, AppMsg::RunTasks { .. })).await;
        let names = |app: &App| app.run.tasks.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&app), ["x"]);
        std::fs::create_dir_all(&app.paths.config_dir).unwrap();
        std::fs::write(app.paths.config_file(), "[lsp]\nenabled = false\n[[run.configs]]\nname = \"c\"\ncommand = \"true\"\n").unwrap();
        app.config_changed(vec![crate::config::watch::ConfigFile::Config]);
        handle_next(&mut app, |m| matches!(m, AppMsg::RunTasks { .. })).await;
        assert_eq!(names(&app), ["c", "x"]);
        let requested = app.run.tasks_requested;
        app.handle_msg(AppMsg::FsChanged { generation, changes: Changes { paths: vec![root.join("src/other.rs")], ..Default::default() } });
        assert_eq!(app.run.tasks_requested, requested, "an unrelated path reloads nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn two_configs(app: &mut App) {
        app.config.run.tasks = vec![shell("a", "echo a"), shell("b", "echo b")];
    }

    fn texts(app: &App) -> Vec<String> {
        app.run_panel.lines.iter().map(|l| l.text.clone()).collect()
    }

    #[tokio::test]
    async fn shift_f10_runs_the_selected_configuration() {
        let (mut app, dir) = test_app("run-shift-f10");
        two_configs(&mut app);
        app.perform(Action::App(crate::actions::AppAction::OpenPalette(PaletteKind::RunTasks)));
        handle_next(&mut app, |m| matches!(m, AppMsg::RunTasks { .. })).await;
        app.palette.as_mut().unwrap().set_query("b");
        let res = app.palette.as_mut().unwrap().handle_action(&PaletteAction::Accept).unwrap();
        app.finish_palette(res);
        assert_eq!(app.run.selected.as_deref(), Some("b"));
        until_exit(&mut app).await;
        app.run.last = Some(shell("other", "echo other"));
        app.handle_key(KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT));
        until_exit(&mut app).await;
        assert_eq!(texts(&app), ["b"], "Shift+F10 runs the selected configuration, not the last run");
        app.run.last = Some(shell("other", "echo other"));
        app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::CONTROL));
        until_exit(&mut app).await;
        assert_eq!(texts(&app), ["other"], "Ctrl+F5 reruns the last run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_selected_waits_for_the_list_and_handles_a_missing_name() {
        let (mut app, dir) = test_app("run-selected-wait");
        two_configs(&mut app);
        app.run.selected = Some("b".into());
        assert!(!app.run.tasks_loaded);
        app.perform(Action::Run(RunAction::RunSelected));
        assert!(!app.run.running(), "nothing runs before the list is there");
        handle_next(&mut app, |m| matches!(m, AppMsg::RunTasks { .. })).await;
        assert!(app.run.running());
        until_exit(&mut app).await;
        assert_eq!(texts(&app), ["b"]);
        // The name is gone: a warning and the picker.
        app.run.selected = Some("gone".into());
        app.perform(Action::Run(RunAction::RunSelected));
        assert!(app.notifications.iter().any(|n| n.level == Level::Warn && n.text == "Run configuration 'gone' no longer exists"));
        assert!(app.palette.is_some());
        assert!(!app.run.running());
        // Nothing selected: the picker, nothing runs.
        app.palette = None;
        app.run.selected = None;
        app.perform(Action::Run(RunAction::RunSelected));
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::RunTasks));
        assert!(!app.run.running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn selection_survives_through_the_session() {
        let (mut app, dir) = test_app("run-selected-session");
        two_configs(&mut app);
        app.run.selected = Some("b".into());
        let session = app.session();
        assert_eq!(session.run_config.as_deref(), Some("b"));
        app.run_root_changed();
        assert_eq!(app.run.selected, None, "a root switch clears it");
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![]);
        assert_eq!(app.run.selected.as_deref(), Some("b"));
        handle_next(&mut app, |m| matches!(m, AppMsg::RunTasks { .. })).await;
        assert!(!app.run.running(), "restoring runs nothing");
        assert!(app.run_panel.status().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
