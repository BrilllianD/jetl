//! The Terminal window's shell: started by the first frame that shows the
//! window (at that frame's size, in the project root), fed keys, paste and
//! mouse while focused, its output pumped into the screen on each wake. One
//! shell per tab (`terminal::tabs`); quitting, switching roots and dropping
//! the app kill every tab's session, and a root switch leaves one tab whose
//! shell starts in the new root.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::App;
use crate::config::keymap::KeyCombo;
use crate::config::theme::Theme;
use crate::event::Level;
use crate::terminal::panel::Status;
use crate::terminal::pty::{Session, Spec};
use crate::ui::focus::{PanelId, ToolWindowId};

impl App {
    fn terminal_spec(&self, rows: u16, cols: u16) -> Spec {
        let mut env = vec![("TERM".to_string(), "xterm-256color".to_string())];
        // Promise 24-bit color only when jetl draws it; else programs fall
        // back to the palette instead of sending RGB the outer terminal may
        // not show (an inherited COLORTERM goes too).
        let mut env_remove = Vec::new();
        if self.theme_truecolor {
            env.push(("COLORTERM".to_string(), "truecolor".to_string()));
        } else {
            env_remove.push("COLORTERM".to_string());
        }
        let mut argv = self.config.terminal.argv();
        let mut clear_env = false;
        // Tests never run the user's shell or its startup files.
        if cfg!(test) {
            argv = vec!["/bin/sh".into()];
            clear_env = true;
            env.push(("PATH".into(), "/usr/bin:/bin".into()));
            env.push(("PS1".into(), "$ ".into()));
            env.push(("HOME".into(), self.workspace.root.to_string_lossy().into_owned()));
        }
        Spec { argv, cwd: self.workspace.root.clone(), env, env_remove, clear_env, rows, cols }
    }

    /// Start the active tab's shell on a `rows` x `cols` terminal.
    pub(crate) fn start_terminal(&mut self, rows: u16, cols: u16) {
        self.terminal.active_mut().reset();
        self.terminal.seq += 1;
        let spec = self.terminal_spec(rows, cols);
        tracing::debug!("terminal: starting {:?} in {}", spec.argv, spec.cwd.display());
        match Session::spawn(&spec, self.terminal.seq, self.workspace.generation, self.bus_tx.clone()) {
            Ok(s) => self.terminal.active_mut().start(s, rows, cols),
            Err(e) => {
                let e = format!("cannot start {}: {e:#}", spec.argv[0]);
                self.notify(Level::Error, format!("Terminal: {e}"));
                self.terminal.active_mut().failed(e);
            }
        }
    }

    /// Draw the Terminal window, starting the shell when there is none.
    pub(crate) fn render_terminal(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let inner = self.terminal.text_area(area);
        if self.terminal.active().status == Status::Idle && inner.width > 0 && inner.height > 0 {
            self.start_terminal(inner.height, inner.width);
        }
        self.terminal.render(frame, area, focused, theme, self.theme_truecolor);
    }

    /// A key while the terminal has focus: the `[terminal]` keymap table's
    /// keys and Esc Esc stay with jetl, every other one goes to the shell
    /// (or restarts it once it ended). Always consumed.
    ///
    /// Esc Esc leaves only at the shell prompt: there a lone Esc is held
    /// back until the next key or the double click interval, so a leaving
    /// pair sends nothing (a sent Esc would turn the shell's next key into
    /// Alt+key). Full-screen programs (vim, less) get every Esc at once and
    /// never lose the focus to it.
    pub(super) fn terminal_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return;
        }
        let chord = [KeyCombo::normalize(key.code, key.modifiers)];
        if let Some(a) = self.keymap.table("terminal").and_then(|t| t.bindings.get(chord.as_slice())).cloned() {
            self.terminal.active_mut().flush_esc();
            self.perform(a);
            return;
        }
        let double_tap = self.dispatcher.double_tap;
        match self.terminal.active().status {
            Status::Running
                if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE && !self.terminal.active().alternate_screen() =>
            {
                let now = Instant::now();
                if self.terminal.active_mut().pending_esc.take().is_some_and(|t| now.duration_since(t) <= double_tap) {
                    self.focus.set(PanelId::Editor);
                } else {
                    self.terminal.active_mut().pending_esc = Some(now);
                }
            }
            Status::Running => {
                self.terminal.active_mut().flush_esc();
                self.terminal.active_mut().send_key(&key);
            }
            Status::Ended(_) => self.terminal.restart(),
            Status::Idle => {}
        }
    }

    /// Send a held-back Esc once no second one followed in time (the tick).
    pub(super) fn terminal_tick(&mut self) {
        if self.terminal.active().pending_esc.is_some_and(|t| t.elapsed() > self.dispatcher.double_tap) {
            self.terminal.active_mut().flush_esc();
        }
    }

    pub(super) fn terminal_paste(&mut self, text: &str) {
        self.terminal.active_mut().flush_esc();
        self.terminal.active_mut().paste(text);
    }

    /// The shell wrote or exited; drop wakes of an earlier shell or root.
    pub(super) fn terminal_output(&mut self, generation: u64, id: u64) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping output of an old terminal");
            return;
        }
        match self.terminal.find_mut(id) {
            Some(p) => p.pump(),
            None => tracing::debug!("dropping output of an old terminal"),
        }
    }

    /// Add a tab, make it active, show and focus the window. Its shell starts
    /// on the next frame, like the first one.
    pub(super) fn new_terminal_tab(&mut self) {
        self.terminal.push();
        if !self.layout.is_visible(ToolWindowId::Terminal) {
            self.layout.show(ToolWindowId::Terminal);
        }
        self.focus.set(PanelId::Terminal);
    }

    /// End the active tab's shell and close the tab; the last one also hides
    /// the window (the next open starts a fresh shell).
    pub(super) fn close_terminal_tab(&mut self) {
        if self.terminal.len() > 1 {
            self.terminal.close_active();
            return;
        }
        self.terminal.reset();
        if self.layout.is_visible(ToolWindowId::Terminal) {
            self.layout.hide(ToolWindowId::Terminal);
        }
        if self.focus.is(PanelId::Terminal) {
            self.focus.set(PanelId::Editor);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    use super::*;
    use crate::actions::{Action, AppAction};
    use crate::app::tests::{handle_next, test_app};
    use crate::event::AppMsg;
    use crate::ui::focus::ToolWindowId;

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) });
        }
    }

    /// Pump terminal output until `done` holds, for up to 5 s.
    async fn until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(app) {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for {what}:\n{}", app.terminal.active().contents()));
            let _ = tokio::time::timeout(left, handle_next(app, |m| matches!(m, AppMsg::TerminalOutput { .. }))).await;
        }
    }

    fn open(app: &mut App) {
        app.perform(Action::App(AppAction::ToggleToolWindow(ToolWindowId::Terminal)));
        assert!(app.focus.is(PanelId::Terminal));
        // What the first frame showing the window does.
        app.start_terminal(10, 60);
        assert!(app.terminal.active().running());
    }

    #[tokio::test]
    async fn shell_runs_in_the_root_shows_its_exit_and_restarts() {
        let (mut app, dir) = test_app("term-shell");
        open(&mut app);
        type_text(&mut app, "printf 'hel''lo\\n'; pwd\n");
        let root = app.workspace.root.display().to_string();
        until(&mut app, "hello", |a| a.terminal.active().contents().lines().any(|l| l == "hello")).await;
        until(&mut app, "the root", |a| a.terminal.active().contents().lines().any(|l| l == root)).await;
        type_text(&mut app, "exit 3\n");
        until(&mut app, "the exit", |a| !a.terminal.active().running()).await;
        assert_eq!(app.terminal.active().status, Status::Ended("exited 3".into()));
        assert!(app.terminal.active().contents().contains("[process exited 3]"));
        assert!(app.terminal.active().session.is_none());
        // The next key restarts it at the next frame.
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.terminal.active().status, Status::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn draw_ui(app: &mut App) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| crate::ui::render(app, f)).unwrap();
    }

    fn click(app: &mut App, button: MouseButton, r: ratatui::layout::Rect) {
        app.handle_mouse(MouseEvent { kind: MouseEventKind::Down(button), column: r.x + 1, row: r.y, modifiers: KeyModifiers::NONE });
    }

    #[tokio::test]
    async fn tab_row_buttons_and_middle_press_manage_tabs() {
        use crate::terminal::tabs::RowHit;
        let (mut app, dir) = test_app("term-row-buttons");
        open(&mut app);

        draw_ui(&mut app);
        let new = app.terminal.hit_rect(RowHit::New).expect("New button drawn with one tab");
        click(&mut app, MouseButton::Left, new);
        assert_eq!((app.terminal.len(), app.terminal.active), (2, 1));
        assert!(app.focus.is(PanelId::Terminal));

        draw_ui(&mut app);
        let first = app.terminal.hit_rect(RowHit::Tab(0)).unwrap();
        click(&mut app, MouseButton::Middle, first);
        assert_eq!(app.terminal.len(), 1);
        assert_eq!(app.terminal.label(0), "Local (2)");
        assert!(app.layout.is_visible(ToolWindowId::Terminal));

        draw_ui(&mut app);
        let close = app.terminal.hit_rect(RowHit::Close).unwrap();
        click(&mut app, MouseButton::Left, close);
        assert!(!app.layout.is_visible(ToolWindowId::Terminal));
        assert!(app.focus.is(PanelId::Editor));
        assert_eq!(app.terminal.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn colorterm_follows_the_color_depth() {
        let (mut app, dir) = test_app("term-colorterm");
        assert!(app.theme_truecolor, "tests see COLORTERM=truecolor");
        let spec = app.terminal_spec(10, 60);
        assert!(spec.env.contains(&("COLORTERM".into(), "truecolor".into())));
        assert!(spec.env_remove.is_empty());
        // `ui.colors = "256"` by hot reload: the next shell gets no COLORTERM.
        let mut config = app.config.clone();
        config.ui.colors = crate::config::ColorMode::Ansi256;
        app.apply_config(config);
        assert!(!app.theme_truecolor);
        let spec = app.terminal_spec(10, 60);
        assert!(!spec.env.iter().any(|(k, _)| k == "COLORTERM"));
        assert_eq!(spec.env_remove, ["COLORTERM"]);
        open(&mut app);
        type_text(&mut app, "echo \"[${COLORTERM-unset}]\"\n");
        until(&mut app, "COLORTERM unset", |a| a.terminal.active().contents().lines().any(|l| l == "[unset]")).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reserved_keys_stay_with_jetl_and_esc_esc_leaves() {
        let (mut app, dir) = test_app("term-keys");
        open(&mut app);
        // At the prompt a lone Esc waits for the next key; a second one
        // soon after leaves without sending anything.
        press(&mut app, KeyCode::Esc);
        assert!(app.focus.is(PanelId::Terminal) && app.terminal.active().pending_esc.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.focus.is(PanelId::Editor) && app.terminal.active().pending_esc.is_none());
        // Any other key sends the held Esc first; so does the tick, later.
        app.focus.set(PanelId::Terminal);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('x'));
        assert!(app.terminal.active().pending_esc.is_none());
        press(&mut app, KeyCode::Esc);
        app.terminal.active_mut().pending_esc = Some(Instant::now() - Duration::from_secs(1));
        app.terminal_tick();
        assert!(app.terminal.active().pending_esc.is_none() && app.focus.is(PanelId::Terminal));
        // A full-screen program gets every Esc at once and keeps the focus.
        type_text(&mut app, "\nprintf '\\033[?1049h'\n");
        until(&mut app, "the alternate screen", |a| a.terminal.active().alternate_screen()).await;
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert!(app.focus.is(PanelId::Terminal) && app.terminal.active().pending_esc.is_none());
        // Global keys such as Ctrl+Q go to the shell; Alt+F12 hides.
        app.focus.set(PanelId::Terminal);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.dialog.is_none() && !app.should_quit);
        app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::ALT));
        assert!(!app.layout.is_visible(ToolWindowId::Terminal));
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resize_reaches_the_shell() {
        let (mut app, dir) = test_app("term-resize");
        open(&mut app);
        type_text(&mut app, "stty size\n");
        until(&mut app, "10 60", |a| a.terminal.active().contents().lines().any(|l| l == "10 60")).await;
        app.terminal.active_mut().resize(7, 42);
        assert_eq!(app.terminal.active().size(), (7, 42));
        type_text(&mut app, "clear; stty size\n");
        until(&mut app, "7 42", |a| a.terminal.active().contents().lines().any(|l| l == "7 42")).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn paste_goes_to_the_shell() {
        let (mut app, dir) = test_app("term-paste");
        open(&mut app);
        app.handle_paste("echo pas''ted\n".into());
        until(&mut app, "pasted", |a| a.terminal.active().contents().lines().any(|l| l == "pasted")).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quit_root_switch_and_drop_kill_the_session() {
        let (mut app, dir) = test_app("term-kill");
        open(&mut app);
        let pid_file = dir.join("job");
        // A background job, in its own process group under job control.
        type_text(&mut app, &format!("sleep 30 & echo $! > '{}'; echo started\n", pid_file.display()));
        until(&mut app, "the job", |a| a.terminal.active().contents().lines().any(|l| l == "started")).await;
        let job: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let shell = app.terminal.active().pid().unwrap();
        // A late wake of this shell after a root switch is dropped.
        let (g, id) = (app.workspace.generation, app.terminal.seq);
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert_eq!(app.terminal.active().status, Status::Idle);
        app.handle_msg(AppMsg::TerminalOutput { generation: g, id });
        #[cfg(target_os = "linux")]
        {
            assert!(crate::util::process_gone(shell).await, "the shell {shell} survived the root switch");
            assert!(crate::util::process_gone(job).await, "its job {job} survived the root switch");
        }
        // A new shell starts in the new root; dropping the app kills it.
        app.start_terminal(5, 40);
        let shell = app.terminal.active().pid().unwrap();
        type_text(&mut app, "pwd\n");
        let root = app.workspace.root.display().to_string();
        until(&mut app, "the new root", |a| a.terminal.active().contents().lines().any(|l| l == root)).await;
        drop(app);
        #[cfg(target_os = "linux")]
        assert!(crate::util::process_gone(shell).await, "the shell {shell} survived the app");
        let _ = (shell, job);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn pid_of(app: &App, tab: usize) -> u32 {
        app.terminal.tabs[tab].panel.pid().unwrap()
    }

    #[tokio::test]
    async fn tabs_keep_separate_shells_and_output() {
        let (mut app, dir) = test_app("term-tabs");
        open(&mut app);
        type_text(&mut app, "X=one; echo set\n");
        until(&mut app, "set", |a| a.terminal.active().contents().lines().any(|l| l == "set")).await;
        app.perform(Action::App(AppAction::NewTerminalTab));
        assert_eq!((app.terminal.len(), app.terminal.active), (2, 1));
        assert!(app.focus.is(PanelId::Terminal));
        assert_eq!(app.terminal.active().status, Status::Idle);
        app.start_terminal(10, 60);
        assert_ne!(pid_of(&app, 0), pid_of(&app, 1));
        type_text(&mut app, "echo \"[${X-unset}]\"\n");
        until(&mut app, "[unset]", |a| a.terminal.active().contents().lines().any(|l| l == "[unset]")).await;
        app.handle_key(alt(KeyCode::Left));
        assert_eq!(app.terminal.active, 0);
        type_text(&mut app, "echo \"[$X]\"\n");
        until(&mut app, "[one]", |a| a.terminal.active().contents().lines().any(|l| l == "[one]")).await;
        // Output of an inactive tab is parsed and kept.
        type_text(&mut app, "sleep 0.3; echo late\n");
        app.handle_key(alt(KeyCode::Right));
        assert_eq!(app.terminal.active, 1);
        until(&mut app, "late in tab 1", |a| a.terminal.tabs[0].panel.contents().lines().any(|l| l == "late")).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn close_tab_activates_the_left_neighbour_and_the_last_hides() {
        let (mut app, dir) = test_app("term-close-tab");
        open(&mut app);
        for _ in 0..2 {
            app.perform(Action::App(AppAction::NewTerminalTab));
            app.start_terminal(10, 60);
        }
        let pids: Vec<u32> = (0..3).map(|i| pid_of(&app, i)).collect();
        app.handle_key(alt(KeyCode::Left));
        assert_eq!(app.terminal.active, 1);
        app.handle_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::CONTROL));
        assert_eq!((app.terminal.len(), app.terminal.active), (2, 0));
        assert_eq!(app.terminal.label(1), "Local (3)");
        #[cfg(target_os = "linux")]
        assert!(crate::util::process_gone(pids[1]).await, "the closed tab's shell survived");
        app.perform(Action::App(AppAction::CloseTerminalTab));
        assert_eq!((app.terminal.len(), app.terminal.active), (1, 0));
        app.perform(Action::App(AppAction::CloseTerminalTab));
        assert!(!app.layout.is_visible(ToolWindowId::Terminal));
        assert!(app.focus.is(PanelId::Editor));
        assert_eq!((app.terminal.len(), app.terminal.label(0)), (1, "Local".to_string()));
        assert_eq!(app.terminal.active().status, Status::Idle);
        #[cfg(target_os = "linux")]
        for pid in pids {
            assert!(crate::util::process_gone(pid).await, "shell {pid} survived");
        }
        app.perform(Action::App(AppAction::ToggleToolWindow(ToolWindowId::Terminal)));
        assert!(app.layout.is_visible(ToolWindowId::Terminal));
        assert_eq!(app.terminal.active().status, Status::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn new_tab_opens_the_hidden_window() {
        let (mut app, dir) = test_app("term-new-tab");
        assert!(!app.layout.is_visible(ToolWindowId::Terminal));
        app.perform(Action::App(AppAction::NewTerminalTab));
        assert!(app.layout.is_visible(ToolWindowId::Terminal) && app.focus.is(PanelId::Terminal));
        assert_eq!((app.terminal.len(), app.terminal.active), (2, 1));
        assert_eq!(app.terminal.label(0), "Local");
        assert_eq!(app.terminal.label(1), "Local (2)");
        app.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::ALT | KeyModifiers::SHIFT));
        assert_eq!((app.terminal.len(), app.terminal.active), (3, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn root_switch_ends_every_tab() {
        let (mut app, dir) = test_app("term-tabs-root");
        open(&mut app);
        app.perform(Action::App(AppAction::NewTerminalTab));
        app.start_terminal(10, 60);
        let pids = [pid_of(&app, 0), pid_of(&app, 1)];
        let (g, id) = (app.workspace.generation, app.terminal.seq);
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert_eq!(app.terminal.len(), 1);
        assert_eq!(app.terminal.active().status, Status::Idle);
        #[cfg(target_os = "linux")]
        for pid in pids {
            assert!(crate::util::process_gone(pid).await, "shell {pid} survived the root switch");
        }
        let _ = pids;
        app.handle_msg(AppMsg::TerminalOutput { generation: g, id });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tab_shortcuts_are_shown() {
        let (app, dir) = test_app("term-tab-keys");
        let new = app.shortcut(&Action::App(AppAction::NewTerminalTab));
        assert!(matches!(new.as_deref(), Some("Ctrl+Shift+T" | "Alt+Shift+T")), "{new:?}");
        assert_eq!(app.shortcut(&Action::App(AppAction::CloseTerminalTab)).as_deref(), Some("Ctrl+F4"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
