//! The application: owns all panels, routes input, applies effects, and runs
//! the event loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, MouseButton};
use futures::StreamExt;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::actions::{Action, AppAction, EditorAction, FindAction, ModeAction};
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
mod files;
mod find;
mod mouse;
mod msg;
mod palette;
mod watch;

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
    /// Context menu overlay (right click, Shift+F10); takes input like a
    /// dialog, below dialogs.
    pub menu: Option<crate::ui::menu::ContextMenu>,
    pub file_index: FileIndex,
    /// Recent project roots for the palette, cached from the last load.
    recent_projects: Vec<PathBuf>,
    /// Last requested and last applied recent projects load.
    recent_projects_requested: u64,
    recent_projects_seq: u64,
    /// Open the Recent Projects palette when the list first arrives (see
    /// [`App::pick_project_at_startup`]).
    pick_project: bool,
    /// A failed branch load was already reported to the open palette.
    branches_error_shown: bool,
    pub status_hits: Vec<(StatusHit, Rect)>,
    pub keyboard_enhanced: bool,
    pub project: ProjectPanel,
    pub worktrees: WorktreePanel,
    pub git_panel: GitPanel,
    pub find: crate::files::find_panel::FindPanel,
    /// Caret target of a file being opened by `open_file_at`.
    pending_goto: Option<(PathBuf, usize, usize)>,
    /// File from the command line (1-based line and column), opened once
    /// the session is restored so it ends up the active tab.
    pub(crate) pending_open: Option<(PathBuf, usize, usize)>,
    pub git: GitService,
    pub editor: Editor,
    /// Watches the root for outside changes; `None` when off or failed.
    watcher: Option<crate::files::watch::Watcher>,
    started: Instant,
}

impl App {
    pub fn new(root: PathBuf, config: Config, paths: Paths, open: Option<(PathBuf, usize, usize)>) -> Self {
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
            menu: None,
            file_index: FileIndex::default(),
            recent_projects: Vec::new(),
            recent_projects_requested: 0,
            recent_projects_seq: 0,
            pick_project: false,
            branches_error_shown: false,
            status_hits: Vec::new(),
            keyboard_enhanced: crate::tui::keyboard_enhanced(),
            project: ProjectPanel::new(root_for_panels.clone(), config.files.show_hidden, config.files.show_ignored),
            worktrees: WorktreePanel::new(),
            git_panel,
            find: Default::default(),
            pending_goto: None,
            pending_open: open,
            git,
            editor: Editor::new(&config),
            watcher: None,
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
        app.start_watcher();
        app.load_recent_projects(true);
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
                // The watcher reports outside changes as they happen.
                if self.watcher.is_none() {
                    self.dirty_frame = true;
                    self.apply_effects(vec![Effect::GitRefresh]);
                }
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
        if self.focus.is(PanelId::Find) {
            let mut cx = ctx!(self);
            self.find.paste(&text, &mut cx);
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
        if let Some(m) = &mut self.menu {
            if key.kind == crossterm::event::KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
                return;
            }
            match m.handle_key(key) {
                Some(crate::ui::menu::MenuResult::Run(a)) => {
                    self.menu = None;
                    self.perform(a);
                }
                Some(crate::ui::menu::MenuResult::Cancel) => self.menu = None,
                None => {}
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
        // And for the Find window's query and replacement fields.
        if self.focus.is(PanelId::Find) && key.kind != crossterm::event::KeyEventKind::Release {
            let mut cx = ctx!(self);
            let used = self.find.handle_key(key, &mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
            if used {
                self.dispatcher.reset();
                return;
            }
        }
        // And for the tree's speed search.
        if self.focus.is(PanelId::Project) && key.kind != crossterm::event::KeyEventKind::Release && self.project.handle_key(key) {
            self.dispatcher.reset();
            return;
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
            Action::Find(a @ (FindAction::InFiles | FindAction::ReplaceInFiles)) => {
                let query = self.editor.active_buffer().map(|b| b.selected_text()).filter(|t| !t.contains('\n'));
                self.layout.show(ToolWindowId::Find);
                self.focus.set(PanelId::Find);
                self.find.open(query.as_deref(), *a == FindAction::ReplaceInFiles, &mut cx);
                true
            }
            Action::Find(FindAction::NextResult) => {
                self.find.step(true, &mut cx);
                true
            }
            Action::Find(FindAction::PrevResult) => {
                self.find.step(false, &mut cx);
                true
            }
            Action::Find(a) => self.find.handle_action(a, &mut cx),
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
    use crate::ui::dialog::{DialogId, DialogResult};

    /// An app rooted at a fresh temp directory with config and state under
    /// it, so tests never touch the user's files.
    pub(crate) fn test_app(name: &str) -> (App, PathBuf) {
        test_app_open(name, |_| None)
    }

    /// [`test_app`] with a command line file, given the root.
    fn test_app_open(name: &str, open: impl FnOnce(&std::path::Path) -> Option<(PathBuf, usize, usize)>) -> (App, PathBuf) {
        let dir = std::env::temp_dir().join(format!("jetl-test-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let paths = Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), cache_dir: dir.join("cache") };
        let open = open(&root);
        (App::new(root.clone(), Config::default(), paths, open), dir)
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
    async fn save_all_refreshes_tree_and_status_once() {
        let (mut app, dir) = test_app("save-all");
        let root = app.workspace.root.clone();
        let g = app.workspace.generation;
        for name in ["a.txt", "b.txt", "c.txt"] {
            app.handle_msg(AppMsg::FileLoaded { generation: g, path: root.join(name), result: Ok(b"x".to_vec()), focus: true });
            app.perform(Action::Editor(EditorAction::InsertText("y".into())));
        }
        let (tree, status) = (app.project.refresh_requests(), app.git.status_requests());
        app.perform(Action::Editor(EditorAction::SaveAll));
        assert!(!app.editor.has_dirty());
        assert_eq!(app.project.refresh_requests(), tree + 1);
        assert_eq!(app.git.status_requests(), status + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_tree_listing_is_dropped() {
        let (mut app, dir) = test_app("stale-tree");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("new.txt"), "").unwrap();
        let req = app.project.begin_refresh().unwrap();
        let msg = |generation| AppMsg::TreeLoaded { generation, seq: req.seq, dirs: req.run() };
        app.handle_msg(msg(app.workspace.generation - 1));
        assert_eq!(app.project.selected_row().map(|r| r.name.clone()), None, "listing of the previous root");
        app.handle_msg(msg(app.workspace.generation));
        assert_eq!(app.project.selected_row().map(|r| r.name.clone()).as_deref(), Some("new.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Open `name` (with `text`) in a tab of `app`, as a finished load.
    fn load(app: &mut App, name: &str, text: &str) -> PathBuf {
        let path = app.workspace.root.join(name);
        std::fs::write(&path, text).unwrap();
        let generation = app.workspace.generation;
        app.handle_msg(AppMsg::FileLoaded { generation, path: path.clone(), result: Ok(text.as_bytes().to_vec()), focus: true });
        path
    }

    fn type_text(app: &mut App, text: &str) {
        app.perform(Action::Editor(EditorAction::InsertText(text.into())));
    }

    #[tokio::test]
    async fn stale_worktree_list_and_session_are_dropped() {
        let (mut app, dir) = test_app("stale-wt-session");
        let g = app.workspace.generation;
        app.handle_msg(AppMsg::Worktrees { generation: g - 1, result: Err(anyhow::anyhow!("old root")) });
        assert_eq!(app.worktrees.error, None);
        app.handle_msg(AppMsg::Worktrees { generation: g, result: Err(anyhow::anyhow!("fatal: not a git repository")) });
        assert_eq!(app.worktrees.error.as_deref(), Some("fatal: not a git repository"));

        let path = app.workspace.root.join("a.txt");
        let file = crate::workspace::SessionFile { path, ..Default::default() };
        let session = crate::workspace::Session { root: app.workspace.root.clone(), files: vec![file], ..Default::default() };
        app.handle_msg(AppMsg::SessionLoaded { generation: g - 1, session: session.clone(), files: vec![Ok(b"x".to_vec())] });
        assert!(app.editor.buffers.is_empty());
        app.handle_msg(AppMsg::SessionLoaded { generation: g, session, files: vec![Ok(b"x".to_vec())] });
        assert_eq!(app.editor.buffers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn command_line_file_opens_after_the_session_without_a_duplicate() {
        let (mut app, dir) = test_app_open("cli-open", |root| Some((root.join("b.txt"), 3, 2)));
        let root = app.workspace.root.clone();
        let (a, b) = (root.join("a.txt"), root.join("b.txt"));
        let file = |path: &PathBuf| crate::workspace::SessionFile { path: path.clone(), ..Default::default() };
        let session = crate::workspace::Session { root, files: vec![file(&a), file(&b)], active: Some(0), ..Default::default() };
        let g = app.workspace.generation;
        let text = || Ok(b"zero\none\ntwo\nthree\n".to_vec());
        app.handle_msg(AppMsg::SessionLoaded { generation: g, session, files: vec![text(), text()] });
        assert_eq!(app.editor.buffers.len(), 2);
        let active = app.editor.active_buffer().unwrap();
        assert_eq!(active.path.as_ref(), Some(&b));
        assert_eq!(active.cursor_line_col(), (2, 1));
        assert!(app.pending_open.is_none());
        // A later session for the same root (a switch back) skips open tabs.
        let again = crate::workspace::Session { root: app.workspace.root.clone(), files: vec![file(&b)], ..Default::default() };
        app.handle_msg(AppMsg::SessionLoaded { generation: g, session: again, files: vec![text()] });
        assert_eq!(app.editor.buffers.len(), 2);
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col(), (2, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn command_line_file_opens_without_a_session() {
        let (mut app, dir) = test_app_open("cli-open-fresh", |root| {
            std::fs::write(root.join("a.txt"), "zero\none\ntwo\n").unwrap();
            Some((root.join("a.txt"), 2, 3))
        });
        // The empty session arrives first, then the file read it starts.
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.editor.buffers.is_empty() && Instant::now() < deadline {
            if let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(100), app.bus_rx.recv()).await {
                app.handle_msg(msg);
            }
        }
        assert_eq!(app.editor.buffers.len(), 1);
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col(), (1, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quit_dirty_save_all_quits_only_when_everything_saved() {
        let (mut app, dir) = test_app("quit-dirty");
        let a = load(&mut app, "a.txt", "a\n");
        type_text(&mut app, "1");
        app.perform(Action::App(AppAction::Quit));
        let d = app.dialog.take().expect("asks about unsaved changes");
        assert_eq!(d.id, DialogId::QuitDirty);
        // The file changed on disk meanwhile: the save waits for an answer.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&a, "theirs\n").unwrap();
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert!(!app.should_quit);
        let d = app.dialog.take().expect("overwrite or reload");
        assert!(matches!(d.id, DialogId::OverwriteExternal(_)));
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1a\n");
        app.perform(Action::App(AppAction::Quit));
        assert!(app.dialog.is_none() && app.should_quit, "nothing dirty: quits at once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quit_dirty_discard_quits_without_writing() {
        let (mut app, dir) = test_app("quit-discard");
        let a = load(&mut app, "a.txt", "a\n");
        type_text(&mut app, "1");
        app.perform(Action::App(AppAction::Quit));
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Button(1));
        assert!(app.should_quit);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn switch_root_dirty_save_then_switch() {
        let (mut app, dir) = test_app("switch-dirty");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other = std::fs::canonicalize(&other).unwrap();
        let a = load(&mut app, "a.txt", "a\n");
        type_text(&mut app, "1");
        let g = app.workspace.generation;
        app.switch_root(other.clone());
        let d = app.dialog.take().expect("asks about unsaved changes");
        assert!(matches!(&d.id, DialogId::SwitchRootDirty(p) if *p == other));
        assert_eq!(app.workspace.root, a.parent().unwrap(), "no switch before the answer");
        app.finish_dialog(d.id, DialogResult::Button(0));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1a\n");
        assert_eq!(app.workspace.root, other);
        assert_eq!((app.workspace.generation, app.git.generation), (g + 1, g + 1));
        assert!(app.editor.buffers.is_empty(), "worktree.switch_closes_tabs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn recent_projects_palette_at_startup_switches_root() {
        use crate::actions::{PaletteAction, PaletteKind};
        use crate::ui::palette::{Accept, PaletteResult};
        let (mut app, dir) = test_app("recent-projects");
        let root = app.workspace.root.clone();
        let other = std::fs::canonicalize(root.parent().unwrap()).unwrap().join("other");
        std::fs::create_dir_all(&other).unwrap();
        // Only the current root is known: nothing to offer.
        app.pick_project_at_startup();
        app.handle_msg(AppMsg::RecentProjects { seq: 1, roots: vec![root.clone()] });
        assert!(app.palette.is_none());

        app.pick_project_at_startup();
        app.handle_msg(AppMsg::RecentProjects { seq: 2, roots: vec![root.clone(), other.clone()] });
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::RecentProjects));
        // A late reply to an older request does not replace the list.
        app.handle_msg(AppMsg::RecentProjects { seq: 1, roots: vec![root.clone()] });
        let res = app.palette.as_mut().unwrap().handle_action(&PaletteAction::Accept).unwrap();
        assert_eq!(res, PaletteResult::Accept(Accept::SwitchRoot(other.clone())));
        app.finish_palette(res);
        assert_eq!(app.workspace.root, other);
        // Offered once only.
        app.handle_msg(AppMsg::RecentProjects { seq: 3, roots: vec![other.clone(), root.clone()] });
        assert!(app.palette.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dialog_takes_keys_before_the_keymap() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("dialog-keys");
        load(&mut app, "a.txt", "a\n");
        type_text(&mut app, "1");
        app.perform(Action::App(AppAction::Quit));
        // Ctrl+Q again would request a quit; the dialog swallows it.
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.dialog.is_some() && !app.should_quit);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.dialog.is_none() && !app.should_quit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tool_window_toggle_shows_focuses_then_hides() {
        let (mut app, dir) = test_app("toggle-tw");
        app.layout.hide_all();
        app.toggle_tool_window(ToolWindowId::Git);
        assert!(app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Git));
        app.focus.set(PanelId::Editor);
        app.toggle_tool_window(ToolWindowId::Git);
        assert!(app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Git), "visible but unfocused: focus");
        app.toggle_tool_window(ToolWindowId::Git);
        assert!(!app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outside_change_reloads_clean_tabs_and_asks_once_for_dirty_ones() {
        use crate::files::watch::Changes;
        let (mut app, dir) = test_app("outside-change");
        let g = app.workspace.generation;
        let a = load(&mut app, "a.txt", "a\n");
        let b = load(&mut app, "b.txt", "b\n");
        type_text(&mut app, "mine ");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&a, "theirs a\n").unwrap();
        std::fs::write(&b, "theirs b\n").unwrap();
        let changed = || Changes { paths: vec![a.clone(), b.clone()], git: false };
        app.handle_msg(AppMsg::FsChanged { generation: g, changes: changed() });
        // b.txt has unsaved edits: one question.
        let d = app.dialog.take().expect("asks about b.txt");
        assert_eq!(d.id, DialogId::OverwriteExternal(app.editor.buffers[1].id));
        app.handle_msg(AppMsg::FsChanged { generation: g, changes: changed() });
        assert!(app.dialog.is_none(), "asked once");
        // a.txt is clean: read off the main thread, then swapped in.
        app.handle_msg(AppMsg::DiskText { generation: g, path: a.clone(), result: Ok(b"theirs a\n".to_vec()) });
        assert_eq!(app.editor.buffers[0].text(), "theirs a\n");
        assert!(!app.editor.buffers[0].dirty());
        assert_eq!(app.editor.buffers[1].text(), "mine b\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outside_change_never_replaces_an_open_dialog() {
        use crate::files::watch::Changes;
        let (mut app, dir) = test_app("outside-change-dialog");
        let g = app.workspace.generation;
        let a = load(&mut app, "a.txt", "a\n");
        type_text(&mut app, "mine ");
        app.perform(Action::App(AppAction::Quit));
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&a, "theirs\n").unwrap();
        app.handle_msg(AppMsg::FsChanged { generation: g, changes: Changes { paths: vec![a], git: false } });
        assert_eq!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::QuitDirty));
        assert!(app.notifications.iter().any(|n| n.text.contains("changed on disk")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watcher_failure_warns_and_focus_gain_refreshes_instead() {
        let (mut app, dir) = test_app("watch-fail");
        let g = app.workspace.generation;
        let err = anyhow::anyhow!("inotify watch limit reached");
        app.handle_msg(AppMsg::WatcherStarted { generation: g, result: Err(err) });
        assert!(app.notifications.iter().any(|n| n.level == Level::Warn && n.text.contains("watch limit")));
        let before = app.git.status_requests();
        app.handle_term_event(Event::FocusGained);
        assert_eq!(app.git.status_requests(), before + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run the Find window's current search on this thread and deliver it.
    fn run_find(app: &mut App) {
        let re = app.find.pattern().unwrap();
        let search = crate::files::find::Search {
            root: app.workspace.root.clone(),
            show_hidden: false,
            show_ignored: false,
            re,
            seq: app.find.current_seq(),
            cancel: app.find.cancel_handle(),
        };
        let g = app.workspace.generation;
        let mut batches = Vec::new();
        search.run(|b| batches.push(b));
        for batch in batches {
            app.handle_msg(AppMsg::Find { generation: g, batch });
        }
    }

    #[tokio::test]
    async fn find_in_files_opens_results_and_replaces_in_open_and_closed_files() {
        let (mut app, dir) = test_app("find-in-files");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/b.rs"), "let foo = 1;\nfoo(foo);\n").unwrap();
        let a = load(&mut app, "a.txt", "no\nfoo here\n");
        app.perform(Action::Find(FindAction::InFiles));
        assert!(app.layout.is_visible(ToolWindowId::Find) && app.focus.is(PanelId::Find));
        for c in "foo".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), crossterm::event::KeyModifiers::NONE));
        }
        run_find(&mut app);
        assert_eq!(app.find.hit_count(), 4);
        assert_eq!(app.find.hit_files(), [a.clone(), root.join("src/b.rs")]);
        // Down from the first hit (a.txt) passes the src/b.rs header to its
        // first hit; Enter opens it at the match.
        app.handle_key(KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE));
        let g = app.workspace.generation;
        let b_path = root.join("src/b.rs");
        let bytes = std::fs::read(&b_path).unwrap();
        app.handle_msg(AppMsg::FileLoaded { generation: g, path: b_path.clone(), result: Ok(bytes), focus: true });
        assert_eq!(app.editor.active_path(), Some(b_path.as_path()));
        assert_eq!(app.editor.status_position().as_deref(), Some("Ln 1, Col 5"));
        app.editor.force_close(app.editor.index_of(&b_path).unwrap());
        // Replace: the open a.txt changes in the editor, src/b.rs on disk.
        app.find.replace.set_text("bar");
        app.find.show_replace = true;
        app.finish_dialog(DialogId::ReplaceInFiles, DialogResult::Button(0));
        let a_buf = &app.editor.buffers[app.editor.index_of(&a).unwrap()];
        assert_eq!(a_buf.text(), "no\nbar here\n");
        assert!(a_buf.dirty(), "open files are changed, not saved");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "no\nfoo here\n");
        // The disk part runs off the main thread; wait for its message.
        let msg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match app.bus_rx.recv().await {
                    Some(m @ AppMsg::ReplaceDone { .. }) => break m,
                    Some(_) => {}
                    None => panic!("bus closed"),
                }
            }
        })
        .await
        .unwrap();
        app.handle_msg(msg);
        assert_eq!(std::fs::read_to_string(&b_path).unwrap(), "let bar = 1;\nbar(bar);\n");
        assert!(app.notifications.iter().any(|n| n.text == "Replaced 4 matches in 2 files"), "{:?}", app.notifications);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wait for the first bus message `want` accepts and handle it; other
    /// messages are dropped.
    async fn handle_next(app: &mut App, want: impl Fn(&AppMsg) -> bool) {
        let msg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match app.bus_rx.recv().await {
                    Some(m) if want(&m) => break m,
                    Some(_) => {}
                    None => panic!("bus closed"),
                }
            }
        })
        .await
        .expect("message in time");
        app.handle_msg(msg);
    }

    fn fs_op_done(m: &AppMsg) -> bool {
        matches!(m, AppMsg::FsOpDone { .. })
    }

    #[tokio::test]
    async fn tree_new_file_creates_opens_and_selects_it() {
        let (mut app, dir) = test_app("tree-new");
        let root = app.workspace.root.clone();
        let path = root.join("src/new.rs");
        app.finish_dialog(DialogId::TreeNew { dir: root.clone(), is_dir: false }, DialogResult::Text(" src/new.rs ".into()));
        handle_next(&mut app, fs_op_done).await;
        assert!(path.is_file());
        assert_eq!(app.project.selected_row().map(|r| r.path.clone()), Some(path.clone()), "new folders expand to it");
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_path(), Some(path.as_path()));
        // A taken name fails without touching the file.
        std::fs::write(&path, "keep").unwrap();
        app.finish_dialog(DialogId::TreeNew { dir: root.join("src"), is_dir: false }, DialogResult::Text("new.rs".into()));
        handle_next(&mut app, fs_op_done).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
        assert!(app.notifications.last().is_some_and(|n| n.level == Level::Error && n.text.starts_with("Cannot create src/new.rs")));
        app.finish_dialog(DialogId::TreeNew { dir: root, is_dir: true }, DialogResult::Text("../out".into()));
        assert!(app.notifications.last().is_some_and(|n| n.level == Level::Warn), "checked before running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tree_rename_retargets_open_tabs() {
        let (mut app, dir) = test_app("tree-rename");
        let root = app.workspace.root.clone();
        let a = load(&mut app, "a.txt", "a\n");
        type_text(&mut app, "mine ");
        app.finish_dialog(DialogId::TreeRename(a.clone()), DialogResult::Text("b.txt".into()));
        handle_next(&mut app, fs_op_done).await;
        let b = root.join("b.txt");
        assert!(b.is_file() && !a.exists());
        assert_eq!(app.editor.buffers[0].path.as_deref(), Some(b.as_path()));
        assert_eq!(app.editor.buffers[0].name, "b.txt");
        assert_eq!(app.editor.buffers[0].text(), "mine a\n", "unsaved edits stay");
        assert_eq!(app.project.selected_row().map(|r| r.path.clone()), Some(b));
        // Tabs under a renamed directory follow it.
        std::fs::create_dir_all(root.join("d")).unwrap();
        let x = load(&mut app, "d/x.txt", "x\n");
        app.finish_dialog(DialogId::TreeRename(root.join("d")), DialogResult::Text("e".into()));
        handle_next(&mut app, fs_op_done).await;
        assert!(!x.exists());
        assert_eq!(app.editor.active_path(), Some(root.join("e/x.txt").as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tree_delete_directory_closes_its_tabs() {
        let (mut app, dir) = test_app("tree-delete");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("d/sub")).unwrap();
        load(&mut app, "a.txt", "a\n");
        load(&mut app, "d/x.txt", "x\n");
        load(&mut app, "d/sub/y.txt", "y\n");
        type_text(&mut app, "unsaved ");
        app.finish_dialog(DialogId::TreeDelete(root.join("d")), DialogResult::Button(1));
        assert!(root.join("d").exists(), "Cancel keeps it");
        app.finish_dialog(DialogId::TreeDelete(root.join("d")), DialogResult::Button(0));
        handle_next(&mut app, fs_op_done).await;
        assert!(!root.join("d").exists());
        let names = app.editor.buffers.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
        assert_eq!(names, ["a.txt"]);
        assert!(app.notifications.iter().any(|n| n.text == "Deleted d (closed 2 tabs)"), "{:?}", app.notifications);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tree_typing_filters_rows_and_esc_clears() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("tree-filter");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        for f in ["alpha.txt", "beta.txt", "src/lib.rs"] {
            std::fs::write(root.join(f), "").unwrap();
        }
        app.project.refresh();
        app.project.select_path(&root.join("src/lib.rs"));
        app.focus.set(PanelId::Project);
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let rows = |app: &App| app.project.row_names();
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(rows(&app), ["src", "lib.rs"], "matches with their folders");
        assert_eq!(app.project.selected_row().map(|r| r.name.as_str()), Some("lib.rs"));
        app.handle_key(key(KeyCode::Char('x')));
        assert!(rows(&app).is_empty());
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.project.filter(), "li");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.project.filter(), "");
        assert_eq!(rows(&app), ["src", "lib.rs", "alpha.txt", "beta.txt"]);
        assert!(app.focus.is(PanelId::Project), "the first Esc only clears");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tree_right_click_opens_menu_and_enter_runs_an_item() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("tree-menu");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "").unwrap();
        app.project.refresh();
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let frame = term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let (x, y) = (0..30u16)
            .flat_map(|y| (0..90u16).map(move |x| (x, y)))
            .find(|&(x, y)| (0..5).all(|i| frame.buffer[(x + i, y)].symbol() == &"a.txt"[i as usize..=i as usize]))
            .expect("a.txt row drawn");
        let mouse = |kind| MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right)));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Right)));
        let menu = app.menu.as_ref().expect("menu open");
        assert_eq!(app.project.selected_row().map(|r| r.name.as_str()), Some("a.txt"));
        let rename = menu.items.iter().find(|i| i.label.starts_with("Rename")).unwrap();
        assert!(rename.enabled);
        assert_eq!(rename.shortcut, "Shift+F6");
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        // The first item, New File, asks for a name in the row's folder.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.menu.is_none());
        assert_eq!(app.dialog.as_ref().map(|d| d.id.clone()), Some(DialogId::TreeNew { dir: root, is_dir: false }));
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
