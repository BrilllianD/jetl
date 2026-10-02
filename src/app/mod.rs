//! The application: owns all panels, routes input, applies effects, and runs
//! the event loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, MouseButton};
use futures::StreamExt;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::actions::{Action, AppAction, EditorAction, ModeAction};
use crate::config::keymap::Keymap;
use crate::config::theme::Theme;
use crate::config::{Config, Paths};
use crate::dispatch::{Dispatch, Dispatcher, Mode, is_text_input};
use crate::editor::Editor;
use crate::event::{AppMsg, Effect, Level};
use crate::files::ProjectPanel;
use crate::git::{GitPanel, GitService};
use crate::tui::Term;
use crate::ui::dialog::Dialog;
use crate::ui::focus::{Focus, PanelId, ToolWindowId};
use crate::ui::layout::{LayoutState, SplitterId};
use crate::ui::palette::Palette;
use crate::ui::status_bar::StatusHit;
use crate::workspace::Workspace;
use crate::worktree::WorktreePanel;

/// Build a `Ctx` from disjoint fields so panels can be borrowed mutably at
/// the same time.
macro_rules! ctx {
    ($app:expr) => {
        Ctx { effects: Vec::new(), config: &$app.config, theme: &$app.theme, workspace: &$app.workspace, bus: $app.bus_tx.clone() }
    };
}

// Declared after `ctx!` so the submodules can use it.
mod dialogs;
mod effects;
mod mouse;
mod msg;
mod palette;

/// What panels get while handling input.
pub struct Ctx<'a> {
    pub effects: Vec<Effect>,
    pub config: &'a Config,
    pub theme: &'a Theme,
    pub workspace: &'a Workspace,
    /// Lets a panel start background work that reports back as an
    /// `AppMsg`; nothing uses it yet.
    pub bus: UnboundedSender<AppMsg>,
}

impl Ctx<'_> {
    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        self.effects.push(Effect::Notify(level, text.into()));
    }
    pub fn open_file(&mut self, p: PathBuf) {
        self.effects.push(Effect::OpenFile(p));
    }
    pub fn focus(&mut self, p: PanelId) {
        self.effects.push(Effect::Focus(p));
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

/// The palette's cached file list of the current root.
#[derive(Debug, Default)]
pub struct FileIndex {
    /// Workspace generation the files belong to.
    pub generation: u64,
    /// Request order of the walk that produced `files`; older walks that
    /// finish later are dropped.
    pub seq: u64,
    /// Walks requested so far.
    pub requested: u64,
    pub files: std::sync::Arc<Vec<String>>,
    pub loading: bool,
}

#[derive(Debug, Clone, Copy)]
enum Drag {
    Splitter(SplitterId),
    Panel(PanelId, Rect),
}

#[derive(Debug, Clone, Copy)]
struct LastClick {
    at: Instant,
    x: u16,
    y: u16,
    button: MouseButton,
    count: u8,
}

pub struct App {
    pub workspace: Workspace,
    pub config: Config,
    pub paths: Paths,
    pub theme: Theme,
    pub layout: LayoutState,
    pub focus: Focus,
    pub keymap: Keymap,
    pub dispatcher: Dispatcher,
    pub mode: Mode,
    pub should_quit: bool,
    pub dirty_frame: bool,
    needs_clear: bool,
    pub bus_tx: UnboundedSender<AppMsg>,
    bus_rx: UnboundedReceiver<AppMsg>,
    drag: Option<Drag>,
    last_click: Option<LastClick>,
    pub notifications: Vec<Notification>,
    /// Whether the status bar showed a notification at the last tick.
    notification_shown: bool,
    pub toasts: crate::ui::toasts::Toasts,
    pub dialog: Option<Dialog>,
    /// Command palette overlay; captures input like a dialog.
    pub palette: Option<Palette>,
    pub file_index: FileIndex,
    /// A failed branch load was already reported to the open palette.
    branches_error_shown: bool,
    pub status_hits: Vec<(StatusHit, Rect)>,
    pub keyboard_enhanced: bool,
    pub project: ProjectPanel,
    pub worktrees: WorktreePanel,
    pub git_panel: GitPanel,
    pub git: GitService,
    pub editor: Editor,
    started: Instant,
}

impl App {
    pub fn new(root: PathBuf, config: Config, paths: Paths) -> Self {
        let (bus_tx, bus_rx) = mpsc::unbounded_channel();
        let (keymap, keymap_errors) = Keymap::load(&paths.keymap_file());
        let (theme, theme_errors) = Theme::load(&paths.theme_file());
        let modal = config.keymap.mode == "vim";
        let mut dispatcher = Dispatcher::new(modal);
        dispatcher.double_tap = Duration::from_millis(config.ui.double_click_ms);
        let mode = if modal { Mode::Normal } else { Mode::Insert };
        let layout = LayoutState::new(config.ui.left_width, config.ui.bottom_height);
        let root_for_panels = root.clone();
        let workspace = Workspace::new(root.clone());
        let mut git = GitService::new(root.clone(), workspace.generation, bus_tx.clone());
        git.tab_width = config.editor.tab_width;
        let mut git_panel = GitPanel::new();
        git_panel.set_log_page(config.git.log_page);
        let mut app = Self {
            workspace,
            theme,
            layout,
            focus: Focus::new(),
            keymap,
            dispatcher,
            mode,
            should_quit: false,
            dirty_frame: true,
            needs_clear: false,
            bus_tx,
            bus_rx,
            drag: None,
            last_click: None,
            notifications: Vec::new(),
            notification_shown: false,
            toasts: Default::default(),
            dialog: None,
            palette: None,
            file_index: FileIndex::default(),
            branches_error_shown: false,
            status_hits: Vec::new(),
            keyboard_enhanced: crate::tui::keyboard_enhanced(),
            project: ProjectPanel::new(root_for_panels.clone(), config.files.show_hidden, config.files.show_ignored),
            worktrees: WorktreePanel::new(),
            git_panel,
            git,
            editor: Editor::new(&config),
            started: Instant::now(),
            config,
            paths,
        };
        if !app.keyboard_enhanced {
            app.notify(
                Level::Warn,
                "Terminal lacks the kitty keyboard protocol: Ctrl+Shift shortcuts, Ctrl+Tab and Shift Shift are unavailable",
            );
        }
        for e in keymap_errors.into_iter().chain(theme_errors) {
            app.notify(Level::Error, e);
        }
        app.git.refresh_status();
        app.git.load_branches();
        app.git.load_worktrees();
        app.restore_session();
        app
    }

    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        match level {
            Level::Error => tracing::error!("{text}"),
            Level::Warn => tracing::warn!("{text}"),
            Level::Info => tracing::info!("{text}"),
        }
        let now = Instant::now();
        self.toasts.push(level, &text, now);
        self.notifications.push(Notification { level, text, at: now });
        if self.notifications.len() > 200 {
            self.notifications.drain(..100);
        }
        self.dirty_frame = true;
    }

    pub fn current_notification(&self) -> Option<&Notification> {
        let n = self.notifications.last()?;
        let ttl = match n.level {
            Level::Error => 15,
            Level::Warn => 10,
            Level::Info => 5,
        };
        (n.at.elapsed() < Duration::from_secs(ttl)).then_some(n)
    }

    /// Current branch, or the short sha when HEAD is detached.
    pub fn branch_label(&self) -> Option<String> {
        self.git_panel.branch_label()
    }

    /// Spinner frame and label of the oldest running git operation.
    pub fn busy_label(&self) -> Option<String> {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let (_, label) = self.git.busy.first()?;
        let frame = FRAMES[(self.started.elapsed().as_millis() / 100) as usize % FRAMES.len()];
        let more = if self.git.busy.len() > 1 { format!(" (+{})", self.git.busy.len() - 1) } else { String::new() };
        Some(format!("{frame} {label}{more}"))
    }

    /// Keymap context stack, innermost first.
    fn contexts(&self) -> Vec<&'static str> {
        let mut v = Vec::with_capacity(3);
        if self.dialog.is_some() {
            v.push("dialog");
        } else if self.palette.is_some() {
            v.push("palette");
        } else {
            if self.focus.is(PanelId::Editor) && self.editor.find_focused() {
                v.push("editor.find");
            }
            v.push(self.focus.current.context());
        }
        v.push("global");
        v
    }

    pub async fn run(&mut self, term: &mut Term) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(ev)) => self.handle_term_event(ev),
                    Some(Err(e)) => { tracing::error!("terminal event error: {e}"); }
                    None => { self.should_quit = true; }
                },
                Some(msg) = self.bus_rx.recv() => self.handle_msg(msg),
                _ = tick.tick() => self.on_tick(),
            }
            while let Ok(msg) = self.bus_rx.try_recv() {
                self.handle_msg(msg);
            }
            if self.should_quit {
                break;
            }
            if self.needs_clear {
                term.clear()?;
                self.needs_clear = false;
                self.dirty_frame = true;
            }
            if self.dirty_frame {
                term.draw(|f| crate::ui::render(self, f))?;
                self.dirty_frame = false;
            }
        }
        self.save_session();
        Ok(())
    }

    fn on_tick(&mut self) {
        // Redraw once when the status bar notification expires.
        let shown = self.current_notification().is_some();
        if shown != self.notification_shown {
            self.notification_shown = shown;
            self.dirty_frame = true;
        }
        if self.toasts.expire(Instant::now()) {
            self.dirty_frame = true;
        }
        // Gutter marks follow edits at tick rate, which debounces typing.
        if self.editor.update_gutters() {
            self.dirty_frame = true;
        }
        if self.git.is_busy() {
            self.dirty_frame = true;
        }
        if self.editor.autosave_due(Instant::now()) {
            self.autosave();
        }
    }

    fn autosave(&mut self) {
        let mut cx = ctx!(self);
        self.editor.autosave(&mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
        // The tab's dirty mark changes.
        self.dirty_frame = true;
    }

    fn handle_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => {
                self.dirty_frame = true;
                self.handle_key(key);
            }
            Event::Mouse(m) => self.handle_mouse(m),
            Event::Paste(text) => {
                self.dirty_frame = true;
                self.handle_paste(text);
            }
            Event::Resize(_, _) => {
                self.needs_clear = true;
            }
            Event::FocusGained => {
                self.dirty_frame = true;
                self.apply_effects(vec![Effect::GitRefresh]);
            }
            Event::FocusLost => {
                if self.editor.autosave_on_focus_lost() {
                    self.autosave();
                }
            }
        }
    }

    fn handle_paste(&mut self, text: String) {
        if let Some(d) = &mut self.dialog {
            // Paste into text fields only.
            if let crate::ui::dialog::DialogKind::Input { input, .. } = &mut d.kind {
                input.insert_str(&text.replace(['\n', '\r'], " "));
            }
            return;
        }
        if let Some(p) = &mut self.palette {
            p.paste(&text);
            return;
        }
        if self.focus.is(PanelId::Editor) && self.editor.find_focused() {
            self.editor.paste_into_find(&text);
            return;
        }
        if self.focus.is(PanelId::Git) && self.git_panel.message_focused() {
            self.git_panel.paste_into_message(&text);
            return;
        }
        if self.focus.is(PanelId::Editor) {
            let mut cx = ctx!(self);
            self.editor.handle_action(&EditorAction::InsertText(text), &mut cx);
            let effects = cx.effects;
            self.apply_effects(effects);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if let Some(d) = &mut self.dialog {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return;
            }
            if let KeyCode::Modifier(_) = key.code {
                return;
            }
            if let Some(res) = d.handle_key(key) {
                let d = self.dialog.take().unwrap();
                self.finish_dialog(d.id, res);
            }
            return;
        }
        if self.palette.is_some() {
            self.palette_key(key);
            return;
        }
        // The find bar consumes its editing keys before the keymap; anything
        // it does not use (Ctrl+S, F3, Alt+1) falls through.
        if self.focus.is(PanelId::Editor) && self.editor.find_focused() && key.kind != crossterm::event::KeyEventKind::Release {
            let mut cx = ctx!(self);
            let used = self.editor.handle_find_key(key, &mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
            if used {
                self.dispatcher.reset();
                return;
            }
        }
        // Same for the commit message box.
        if self.focus.is(PanelId::Git) && self.git_panel.message_focused() && key.kind != crossterm::event::KeyEventKind::Release {
            let mut cx = ctx!(self);
            let used = self.git_panel.handle_message_key(key, &mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
            if used {
                self.dispatcher.reset();
                return;
            }
        }
        let contexts = self.contexts();
        let r = self.dispatcher.dispatch(key, &contexts, Some(self.mode), &self.keymap);
        match r {
            Dispatch::Action(a) => self.perform(a),
            Dispatch::Pending | Dispatch::Ignore => {}
            Dispatch::Unmapped(k) => {
                if self.focus.is(PanelId::Editor)
                    && self.mode == Mode::Insert
                    && is_text_input(&k)
                    && let KeyCode::Char(c) = k.code
                {
                    self.perform(Action::Editor(EditorAction::InsertChar(c)));
                }
            }
        }
    }

    pub fn perform(&mut self, action: Action) {
        self.dirty_frame = true;
        let mut cx = ctx!(self);
        let handled = match &action {
            Action::App(a) => {
                drop(cx);
                self.perform_app(a.clone());
                return;
            }
            Action::Mode(m) => {
                drop(cx);
                self.mode = match m {
                    ModeAction::Normal => Mode::Normal,
                    ModeAction::Insert => Mode::Insert,
                    ModeAction::Visual => Mode::Visual,
                };
                return;
            }
            Action::Editor(a) => self.editor.handle_action(a, &mut cx),
            Action::Tree(crate::actions::TreeAction::SelectInProject) => {
                drop(cx);
                self.select_in_project();
                return;
            }
            Action::Tree(a) => self.project.handle_action(a, &mut cx),
            Action::Git(a) => self.git_panel.handle_action(a, &mut cx),
            Action::Worktree(a) => self.worktrees.handle_action(a, &mut cx),
            Action::Palette(_) => false,
        };
        let effects = cx.effects;
        if !handled {
            tracing::debug!("unhandled action {action:?}");
        }
        // Git actions that work inside the tool window make it visible when
        // invoked from elsewhere (e.g. Ctrl+K from the editor); popups and
        // network operations do not.
        use crate::actions::GitAction as G;
        match &action {
            Action::Git(g)
                if !matches!(g, G::Branches | G::NewBranch | G::Push | G::Pull | G::Fetch | G::Refresh)
                    && !self.layout.is_visible(ToolWindowId::Git) =>
            {
                self.layout.show(ToolWindowId::Git);
                self.focus.set(PanelId::Git);
            }
            _ => {}
        }
        self.apply_effects(effects);
    }

    fn perform_app(&mut self, a: AppAction) {
        match a {
            AppAction::Quit => self.request_quit(),
            AppAction::FocusEditor => self.focus.set(PanelId::Editor),
            AppAction::FocusPanel(p) => {
                if let Some(tw) = p.tool_window() {
                    self.layout.show(tw);
                }
                self.focus.set(p);
            }
            AppAction::ToggleToolWindow(tw) => self.toggle_tool_window(tw),
            AppAction::HideAllToolWindows => {
                if self.layout.left.is_none() && self.layout.bottom.is_none() {
                    self.layout.show(ToolWindowId::Project);
                } else {
                    self.layout.hide_all();
                    self.focus.set(PanelId::Editor);
                }
            }
            AppAction::HideFocusedToolWindow => {
                if let Some(tw) = self.focus.current.tool_window() {
                    self.layout.hide(tw);
                    self.focus.set(PanelId::Editor);
                }
            }
            AppAction::Escape => self.focus.set(PanelId::Editor),
            AppAction::OpenPalette(kind) => self.open_palette(kind),
            AppAction::DebugPanic => {
                if cfg!(debug_assertions) {
                    panic!("debug panic requested via keymap");
                }
            }
            AppAction::Refresh => self.apply_effects(vec![Effect::GitRefresh]),
        }
    }

    /// JetBrains semantics: hidden -> show and focus; visible but unfocused ->
    /// focus; focused -> hide and return to the editor.
    pub fn toggle_tool_window(&mut self, tw: ToolWindowId) {
        let panel = tw.panel();
        if !self.layout.is_visible(tw) {
            self.layout.show(tw);
            self.focus.set(panel);
        } else if !self.focus.is(panel) {
            self.focus.set(panel);
        } else {
            self.layout.hide(tw);
            self.focus.set(PanelId::Editor);
        }
    }

    fn select_in_project(&mut self) {
        let Some(p) = self.editor.active_path().map(|p| p.to_path_buf()) else {
            self.notify(Level::Info, "No file is open");
            return;
        };
        self.layout.show(ToolWindowId::Project);
        self.focus.set(PanelId::Project);
        if !self.project.select_path(&p) {
            self.notify(Level::Warn, format!("{} is outside the project", p.display()));
        }
    }

    /// OSC 52 copy so the selection reaches the system clipboard through the
    /// terminal; the editor keeps its own register for paste.
    fn copy_to_clipboard(&mut self, text: &str) {
        use crossterm::clipboard::CopyToClipboard;
        use std::io::Write;
        let mut out = std::io::stdout();
        if crossterm::execute!(out, CopyToClipboard::to_clipboard_from(text)).is_err() {
            tracing::debug!("OSC 52 copy failed");
        }
        let _ = out.flush();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::git::GitMsg;

    /// An app rooted at a fresh temp directory with config and state under
    /// it, so tests never touch the user's files.
    pub(crate) fn test_app(name: &str) -> (App, PathBuf) {
        let dir = std::env::temp_dir().join(format!("jetl-test-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let paths = Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), cache_dir: dir.join("cache") };
        (App::new(root.clone(), Config::default(), paths), dir)
    }

    #[tokio::test]
    async fn stale_file_load_is_dropped() {
        let (mut app, dir) = test_app("stale-load");
        let path = app.workspace.root.join("a.txt");
        let old = app.workspace.generation;
        // The root switches while the read is in flight.
        app.workspace.generation += 1;
        app.handle_msg(AppMsg::FileLoaded { generation: old, path: path.clone(), result: Ok(b"old".to_vec()), focus: true });
        assert!(app.editor.buffers.is_empty());
        let current = app.workspace.generation;
        app.handle_msg(AppMsg::FileLoaded { generation: current, path: path.clone(), result: Ok(b"new".to_vec()), focus: true });
        assert_eq!(app.editor.buffers.len(), 1);
        assert_eq!(app.editor.buffers[0].text(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_index_keeps_the_newest_walk() {
        let (mut app, dir) = test_app("file-index-seq");
        let g = app.workspace.generation;
        app.file_index.requested = 2;
        let files = |n: &str| vec![n.to_string()];
        app.handle_msg(AppMsg::FileIndex { generation: g, seq: 2, files: files("new") });
        assert!(!app.file_index.loading);
        app.handle_msg(AppMsg::FileIndex { generation: g, seq: 1, files: files("old") });
        assert_eq!(*app.file_index.files, files("new"), "an older walk finishing late is dropped");
        app.file_index.requested = 3;
        app.handle_msg(AppMsg::FileIndex { generation: g - 1, seq: 3, files: files("other root") });
        assert_eq!(*app.file_index.files, files("new"), "a walk of the previous root is dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn older_head_blob_does_not_replace_newer() {
        let (mut app, dir) = test_app("blob-seq");
        let path = app.workspace.root.join("a.txt");
        let g = app.workspace.generation;
        app.handle_msg(AppMsg::FileLoaded { generation: g, path: path.clone(), result: Ok(b"x".to_vec()), focus: true });
        let blob = |seq: u64, text: &str| {
            AppMsg::Git(GitMsg::Blob {
                generation: g,
                seq,
                path: path.clone(),
                which: crate::git::BlobWhich::Head,
                text: Some(text.into()),
            })
        };
        app.handle_msg(blob(2, "new head"));
        app.handle_msg(blob(1, "old head"));
        assert_eq!(app.editor.buffers[0].diff_base.as_deref(), Some("new head"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn close_dirty_answer_hits_the_right_tab_after_a_load() {
        let (mut app, dir) = test_app("close-dirty");
        let root = app.workspace.root.clone();
        let g = app.workspace.generation;
        let load = |name: &str| AppMsg::FileLoaded { generation: g, path: root.join(name), result: Ok(b"text".to_vec()), focus: true };
        app.handle_msg(load("a.txt"));
        app.handle_msg(load("b.txt"));
        app.perform(Action::Editor(EditorAction::InsertText("dirty ".into())));
        app.activate_tab(0);
        app.close_tab_index(1);
        let d = app.dialog.take().expect("dirty close asks");
        // A tab opens next to the active one while the dialog is up.
        app.handle_msg(load("c.txt"));
        let names = |app: &App| app.editor.buffers.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&app), ["a.txt", "c.txt", "b.txt"]);
        app.finish_dialog(d.id, crate::ui::dialog::DialogResult::Button(0));
        assert_eq!(names(&app), ["a.txt", "c.txt"]);
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "dirty text\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn branch_load_error_reports_once_per_palette() {
        let (mut app, dir) = test_app("branch-error");
        app.open_palette(crate::actions::PaletteKind::Branches);
        let errors = |app: &App| app.notifications.iter().filter(|n| n.text.starts_with("Branches:")).count();
        let fail = |app: &App| AppMsg::Git(GitMsg::Branches { generation: app.git.generation, result: Err(anyhow::anyhow!("not a repo")) });
        app.handle_msg(fail(&app));
        app.handle_msg(fail(&app));
        assert_eq!(errors(&app), 1);
        // Reopening the palette reports a persisting error again.
        app.palette = None;
        app.open_palette(crate::actions::PaletteKind::Branches);
        app.handle_msg(fail(&app));
        assert_eq!(errors(&app), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
