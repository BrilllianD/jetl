//! The application: owns all panels, routes input, applies effects, and runs
//! the event loop.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton};
use futures::StreamExt;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::actions::{Action, AppAction, Dir, EditorAction, EventLogAction, FindAction, ModeAction, RunAction};
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
use crate::ui::focus::{Focus, PanelId, Side, ToolWindowId};
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

pub use lsp::LspStatus;
pub use save_as::AfterSave;

// Declared after `ctx!` so the submodules can use it.
mod clipboard;
mod completion;
mod config_reload;
mod dialogs;
mod effects;
mod files;
mod find;
mod format;
mod history;
mod lightbulb;
mod lsp;
mod lsp_edit;
mod lsp_nav;
mod main_menu;
mod merge;
mod mouse;
mod msg;
mod palette;
mod recovery;
mod run;
mod save_as;
mod symbols;
mod terminal;
mod watch;

#[cfg(test)]
mod snapshot_tests;

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

/// When the log grows past this, its older half is dropped.
const NOTIFICATIONS_MAX: usize = 200;

#[derive(Debug, Clone)]
pub struct Notification {
    pub level: Level,
    pub text: String,
    pub at: Instant,
    /// Wall clock time, shown in the Event Log.
    pub wall: SystemTime,
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
    /// A tab being dragged to a new place in the tab bar.
    Tab(crate::editor::groups::GroupId, crate::editor::buffer::BufferId),
    /// The bar between two editor groups (the split's preorder index).
    GroupSplitter(usize),
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
    /// Session notifications, oldest first, at most `NOTIFICATIONS_MAX`;
    /// listed by the Event Log.
    pub notifications: Vec<Notification>,
    /// Whether the status bar showed a notification at the last tick.
    notification_shown: bool,
    pub toasts: crate::ui::toasts::Toasts,
    pub dialog: Option<Dialog>,
    /// Command palette overlay; captures input like a dialog.
    pub palette: Option<Palette>,
    /// App-wide palette filter seq, see `FilterBus`.
    palette_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Context menu overlay (right click, Shift+F10); takes input like a
    /// dialog, below dialogs.
    pub menu: Option<crate::ui::menu::ContextMenu>,
    /// Main menu bar overlay (F10); takes input like a dialog, below
    /// dialogs and above the context menu.
    pub menu_bar: Option<crate::ui::menu_bar::MenuBar>,
    /// Tool strip button under the pointer, with its rect; it shows a
    /// tooltip.
    pub strip_hover: Option<(crate::ui::layout::StripButton, Rect)>,
    pub file_index: FileIndex,
    /// Recent project roots for the palette, cached from the last load.
    recent_projects: Vec<PathBuf>,
    /// Last requested and last applied recent projects load.
    recent_projects_requested: u64,
    recent_projects_seq: u64,
    /// Record the root in the recent projects file (at startup and on root
    /// switches). Off until `main.rs` calls [`App::start_recent_projects`],
    /// so tests never write it: the write would land after the test removed
    /// its temp dir and make it again.
    record_recent: bool,
    /// Open the Recent Projects palette when the list first arrives (see
    /// [`App::pick_project_at_startup`]).
    pick_project: bool,
    /// A failed branch load was already reported to the open palette.
    branches_error_shown: bool,
    pub status_hits: Vec<(StatusHit, Rect)>,
    pub keyboard_enhanced: bool,
    /// Whether colors are drawn as true color (`ui.colors` and
    /// `$COLORTERM`); else the theme and the Terminal window's cells use
    /// the 256-color palette. Updated by config hot reload.
    pub theme_truecolor: bool,
    pub project: ProjectPanel,
    pub worktrees: WorktreePanel,
    pub git_panel: GitPanel,
    pub find: crate::files::find_panel::FindPanel,
    pub structure: crate::symbols::panel::StructurePanel,
    pub event_log: crate::ui::event_log::EventLogPanel,
    /// Project symbols for Go to Symbol, built after the file index loads.
    pub symbols: crate::symbols::index::SymbolIndex,
    /// When the open Go to Symbol palette last got the index (it is fed at
    /// most every `SYMBOLS_FEED` while batches arrive).
    symbols_fed: Option<Instant>,
    /// Caret target of a file being opened by `open_file_at` or
    /// `open_file_at_utf16`.
    pending_goto: Option<(PathBuf, msg::GotoTarget)>,
    /// An operation stopped on conflicts: the next status listing
    /// conflicted files opens Resolve Conflicts.
    pending_conflicts: bool,
    /// File from the command line (1-based line and column), opened once
    /// the session is restored so it ends up the active tab.
    pub(crate) pending_open: Option<(PathBuf, usize, usize)>,
    pub git: GitService,
    /// Local history's worker (`history/`); `GitService` holds a clone.
    pub history: crate::history::Handle,
    /// A snapshot, or changes of one, to put into a file once its tab opens
    /// (a revert of a file that was not open), as `pending_goto`.
    pub(crate) pending_revert: Option<(PathBuf, history::PendingRevert)>,
    pub editor: Editor,
    /// Watches the root for outside changes; `None` when off or failed.
    watcher: Option<crate::files::watch::Watcher>,
    /// Clipboard tools, jetl's copy register and the paste in flight.
    clipboard: clipboard::ClipboardState,
    /// Formatter runs in flight.
    pub(crate) format: format::FormatState,
    /// The running task, the last one and the picker's tasks.
    pub(crate) run: run::RunState,
    pub run_panel: crate::run::panel::RunPanel,
    /// The Problems window over `lsp.diagnostics`.
    pub problems: crate::lsp::panel::ProblemsPanel,
    /// The Local History window (`history/panel.rs`).
    pub history_panel: crate::history::panel::LocalHistoryPanel,
    /// The Terminal window and its shell.
    pub terminal: crate::terminal::panel::TerminalPanel,
    /// Language servers and the documents open on them.
    pub(crate) lsp: lsp::LspService,
    /// The Quick Documentation popup at the caret (`app/lsp_nav.rs`).
    pub hover: Option<crate::ui::hover::HoverPopup>,
    /// The code completion popup at the caret (`app/completion.rs`).
    pub completion: Option<crate::ui::completion::CompletionPopup>,
    /// The dirty buffers the panic hook writes (`recovery.rs`), refreshed
    /// by the run loop.
    pub(crate) recorder: crate::recovery::Recorder,
    /// When the next snapshot of unsaved edits is due (`[local_history]
    /// snapshot_unsaved_secs`); `None` when off.
    pub(crate) unsaved_due: Option<Instant>,
    /// Crash dumps of this root waiting for an answer (`app/recovery.rs`).
    recovery_offer: Option<recovery::RecoveryOffer>,
    /// The config watcher and what the config files held when last
    /// reloaded (`app/config_reload.rs`).
    pub(crate) config_reload: config_reload::ConfigReload,
    started: Instant,
}

impl App {
    pub fn new(root: PathBuf, config: Config, paths: Paths, open: Option<(PathBuf, usize, usize)>) -> Self {
        let (bus_tx, bus_rx) = mpsc::unbounded_channel();
        let keymap_text = crate::config::FileText::read(&paths.keymap_file());
        let (keymap, keymap_errors) = Keymap::load_text(&keymap_text, &paths.keymap_file());
        let colorterm = crate::config::color::colorterm_env();
        let (theme, theme_errors) = Theme::load(&config, &paths.config_dir, paths.omarchy_dir.as_deref(), colorterm.as_deref());
        let theme_truecolor = config.ui.colors.truecolor(colorterm.as_deref());
        let modal = config.keymap.mode == "vim";
        let mut dispatcher = Dispatcher::new(modal);
        dispatcher.double_tap = Duration::from_millis(config.ui.double_click_ms);
        let mode = if modal { Mode::Normal } else { Mode::Insert };
        let layout = LayoutState::new(config.ui.left_width, config.ui.bottom_height);
        let root_for_panels = root.clone();
        let workspace = Workspace::new(root.clone());
        let mut git = GitService::new(root.clone(), workspace.generation, bus_tx.clone());
        git.tab_width = config.editor.tab_width;
        git.messages_dir = paths.state_dir.join("commit_messages");
        let mut git_panel = GitPanel::new();
        git_panel.set_log_page(config.git.log_page);
        git_panel.set_log_all(config.git.log_all);
        let recorder = crate::recovery::Recorder::new(&paths.recovery_dir());
        let unsaved_due = config.local_history.unsaved_every().map(|d| Instant::now() + d);
        let history = crate::history::Handle::spawn(paths.history_dir(), config.local_history.clone(), bus_tx.clone());
        // The first job: retention applies before anything new is stored.
        history.prune();
        git.history = history.clone();
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
            palette_seq: Default::default(),
            menu: None,
            menu_bar: None,
            strip_hover: None,
            file_index: FileIndex::default(),
            recent_projects: Vec::new(),
            recent_projects_requested: 0,
            recent_projects_seq: 0,
            record_recent: false,
            pick_project: false,
            branches_error_shown: false,
            status_hits: Vec::new(),
            keyboard_enhanced: crate::tui::keyboard_enhanced(),
            theme_truecolor,
            project: ProjectPanel::new(root_for_panels.clone(), config.files.show_hidden, config.files.show_ignored),
            worktrees: WorktreePanel::new(),
            git_panel,
            find: Default::default(),
            structure: Default::default(),
            event_log: Default::default(),
            symbols: Default::default(),
            symbols_fed: None,
            pending_goto: None,
            pending_conflicts: false,
            pending_open: open,
            git,
            history,
            pending_revert: None,
            editor: Editor::new(&config),
            watcher: None,
            clipboard: Default::default(),
            format: Default::default(),
            run: Default::default(),
            run_panel: Default::default(),
            problems: crate::lsp::panel::ProblemsPanel::excluding(paths.scratches_dir()),
            history_panel: Default::default(),
            terminal: crate::terminal::panel::TerminalPanel::new(config.terminal.scrollback),
            lsp: Default::default(),
            hover: None,
            completion: None,
            recorder,
            unsaved_due,
            recovery_offer: None,
            config_reload: Default::default(),
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
        app.start_config_reload(keymap_text);
        app.git.refresh_status();
        app.git.load_branches();
        app.git.load_worktrees();
        app.git.load_messages();
        app.git.load_stashes();
        app.restore_session();
        app.start_watcher();
        // The symbol index follows the file index.
        app.refresh_file_index();
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
        self.notifications.push(Notification { level, text, at: now, wall: SystemTime::now() });
        let mut dropped = 0;
        if self.notifications.len() > NOTIFICATIONS_MAX {
            dropped = NOTIFICATIONS_MAX / 2;
            self.notifications.drain(..dropped);
        }
        self.event_log.pushed(self.notifications.len(), dropped);
        self.dirty_frame = true;
    }

    /// Fill each row's shortcut from the keymap: one per row (the
    /// shortest) keeps menus narrow.
    pub(crate) fn fill_shortcuts(&self, items: &mut [crate::ui::menu::MenuItem]) {
        for item in items.iter_mut() {
            if let Some(action) = &item.action {
                item.shortcut = self.shortcut(action).unwrap_or_default();
            }
        }
    }

    /// The shortest binding of an action in its usual contexts.
    pub(crate) fn shortcut(&self, action: &Action) -> Option<String> {
        self.keymap.bindings_for(action, action.contexts()).into_iter().next()
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

    /// The busy spinner's current frame.
    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[(self.started.elapsed().as_millis() / 100) as usize % FRAMES.len()]
    }

    /// Spinner frame and label of the oldest running git operation.
    pub fn busy_label(&self) -> Option<String> {
        let (_, label) = self.git.busy.first()?;
        let frame = self.spinner();
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
            if self.focus.is(PanelId::Editor) && self.editor.snippet_active() {
                v.push("editor.snippet");
            }
            if self.focus.is(PanelId::Editor) && self.editor.merge_active() {
                v.push("editor.merge");
            }
            v.push(self.focus.current.context());
        }
        v.push("global");
        v
    }

    /// The event loop until a quit, then [`App::shutdown`]. An error (a
    /// failed draw or clear) returns at once, skipping the quit prompt and
    /// the shutdown: `main.rs` then calls [`App::finish_after_error`].
    pub async fn run(&mut self, term: &mut Term) -> Result<()> {
        self.event_loop(term).await?;
        // Quitting on purpose: a panic from here on writes nothing.
        self.recorder.clear();
        self.shutdown().await;
        Ok(())
    }

    async fn event_loop(&mut self, term: &mut Term) -> Result<()> {
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
            self.recovery_snapshot();
            self.lsp_sync();
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
        Ok(())
    }

    /// Leaving jetl, after a quit or a run loop error; nothing here draws.
    /// Writes the local history snapshots still queued (the saves of the
    /// quit dialog) and the session, kills formatters, the Run task and the
    /// Terminal shell with what they started, and shuts the language
    /// servers down (two seconds at most).
    pub(crate) async fn shutdown(&mut self) {
        self.history.flush(Duration::from_secs(3));
        self.save_session();
        self.format.abort_all();
        self.run.kill();
        self.terminal.reset();
        self.lsp.shutdown_all().await;
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
        // The spinners and the elapsed time of a running task.
        if self.git.is_busy() || self.lsp.busy() || (self.run.running() && self.layout.is_visible(ToolWindowId::Run)) {
            self.dirty_frame = true;
        }
        self.completion_timeout(Instant::now());
        self.completion_docs_tick(Instant::now());
        self.format_timeout(Instant::now());
        self.lightbulb_tick(Instant::now());
        if self.editor.autosave_due(Instant::now()) {
            self.autosave();
        }
        self.unsaved_snapshots(Instant::now());
        self.terminal_tick();
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
        self.completion_flush();
        if let Some(d) = &mut self.dialog {
            // Paste into text fields only.
            if let crate::ui::dialog::DialogKind::Input { input, .. } = &mut d.kind {
                input.insert_str(&text.replace(['\n', '\r'], " "));
                d.clear_path_hint();
            }
            return;
        }
        if let Some(p) = &mut self.palette {
            p.paste(&text);
            return;
        }
        if self.focus.is(PanelId::Terminal) {
            self.terminal_paste(&text);
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

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        let key = crate::config::keymap::KeyCombo::legacy_input(key, self.keyboard_enhanced);
        // An accepted completion waiting for its resolve goes in first.
        if key.kind != crossterm::event::KeyEventKind::Release && !matches!(key.code, KeyCode::Modifier(_)) {
            self.completion_flush();
        }
        if self.dialog.is_some() {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return;
            }
            if let KeyCode::Modifier(_) = key.code {
                return;
            }
            // Dialogs take every key, except that the paste key pastes.
            if self.is_paste_key(key) {
                self.paste();
                return;
            }
            // Tab completes a typed path (Save As); other dialogs keep their Tab.
            if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE && self.dialog.as_ref().is_some_and(|d| d.paths.is_some()) {
                self.complete_dialog_path();
                return;
            }
            let Some(d) = &mut self.dialog else { return };
            if let Some(res) = d.handle_key(key) {
                let d = self.dialog.take().unwrap();
                self.finish_dialog(d.id, res);
            }
            return;
        }
        if self.menu_bar.is_some() {
            if key.kind != crossterm::event::KeyEventKind::Release && !matches!(key.code, KeyCode::Modifier(_)) {
                self.main_menu_key(key);
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
        // The hover popup takes Esc and its scroll keys; other keys close
        // it and go on.
        if self.hover.is_some() && self.hover_key(key) {
            return;
        }
        // So does the completion popup with its selection keys, Enter, Tab.
        if self.completion.is_some() && self.completion_key(key) {
            return;
        }
        // The terminal takes every key but a few (`[terminal]`, Esc Esc).
        if self.focus.is(PanelId::Terminal) && self.layout.is_visible(ToolWindowId::Terminal) {
            self.dispatcher.reset();
            self.terminal_key(key);
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
        // And for the Local History window's filter.
        if self.focus.is(PanelId::LocalHistory) && key.kind != crossterm::event::KeyEventKind::Release {
            let mut cx = ctx!(self);
            let used = self.history_panel.handle_key(key, &mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
            if used {
                self.dispatcher.reset();
                return;
            }
        }
        self.snippet_check();
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
            Action::Editor(EditorAction::Paste) => {
                drop(cx);
                self.paste();
                return;
            }
            // Group commands take the keyboard to the editor, as in JetBrains.
            Action::Editor(
                a @ (EditorAction::SplitRight
                | EditorAction::SplitDown
                | EditorAction::NextGroup
                | EditorAction::PrevGroup
                | EditorAction::Unsplit
                | EditorAction::UnsplitAll
                | EditorAction::MoveTabToOppositeGroup),
            ) => {
                self.focus.set(PanelId::Editor);
                self.editor.handle_action(a, &mut cx)
            }
            // So does a jump to a numbered bookmark that exists.
            Action::Editor(a @ EditorAction::GotoNumberedBookmark(d)) => {
                if self.editor.has_digit(*d) && !self.editor.merge_active() {
                    self.focus.set(PanelId::Editor);
                }
                self.editor.handle_action(a, &mut cx)
            }
            Action::Editor(a) => self.editor.handle_action(a, &mut cx),
            Action::Tree(crate::actions::TreeAction::SelectInProject) => {
                drop(cx);
                self.select_in_project();
                return;
            }
            Action::Tree(a) => self.project.handle_action(a, &mut cx),
            Action::Git(crate::actions::GitAction::FileHistory) => {
                drop(cx);
                self.file_history();
                return;
            }
            Action::Git(crate::actions::GitAction::ResolveConflicts) => {
                drop(cx);
                self.resolve_conflicts();
                return;
            }
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
            Action::Structure(a) => self.structure.handle_action(a, &mut cx),
            Action::EventLog(EventLogAction::Copy) => {
                drop(cx);
                self.copy_event();
                return;
            }
            Action::EventLog(EventLogAction::Clear) => {
                drop(cx);
                self.notifications.clear();
                self.event_log.clear();
                return;
            }
            Action::EventLog(a) => self.event_log.handle_action(a, &self.notifications, &mut cx),
            Action::Run(RunAction::Rerun) => {
                drop(cx);
                self.rerun();
                return;
            }
            Action::Run(RunAction::Stop) => {
                drop(cx);
                self.stop_run();
                return;
            }
            Action::Run(a) => self.run_panel.handle_action(a, &mut cx),
            Action::Problems(a) => self.problems.handle_action(a, &self.lsp.diagnostics, self.editor.active_path(), &mut cx),
            Action::History(a) => self.history_panel.handle_action(a, &mut cx),
            Action::Lsp(crate::actions::LspAction::Restart) => {
                drop(cx);
                self.lsp_restart();
                return;
            }
            Action::Lsp(a @ (crate::actions::LspAction::NextProblem | crate::actions::LspAction::PrevProblem)) => {
                drop(cx);
                self.lsp_step_problem(*a == crate::actions::LspAction::NextProblem);
                return;
            }
            Action::Lsp(a) => {
                drop(cx);
                match a {
                    crate::actions::LspAction::GotoDefinition => self.lsp_goto_definition(),
                    crate::actions::LspAction::FindUsages => self.lsp_find_usages(),
                    crate::actions::LspAction::QuickDoc => self.lsp_quick_doc(),
                    crate::actions::LspAction::Complete => self.lsp_complete(),
                    crate::actions::LspAction::Rename => self.lsp_rename(),
                    crate::actions::LspAction::ContextActions => self.lsp_context_actions(),
                    crate::actions::LspAction::FileStructure => self.lsp_file_structure(),
                    _ => {}
                }
                return;
            }
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
        // The completion list follows the word at the caret, and typing
        // may ask for one.
        if let Action::Editor(a) = &action {
            let typed = if let EditorAction::InsertChar(c) = a { Some(*c) } else { None };
            self.completion_after_edit(typed);
        }
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
            AppAction::HideAllToolWindows if self.layout.zen.is_some() => self.toggle_zen(),
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
            AppAction::ResizeToolWindow(dir) => self.resize_tool_window(dir),
            AppAction::ToggleZen => self.toggle_zen(),
            AppAction::Escape => self.focus.set(PanelId::Editor),
            AppAction::OpenPalette(kind) => self.open_palette(kind),
            AppAction::DebugPanic => {
                if cfg!(debug_assertions) {
                    panic!("debug panic requested via keymap");
                }
            }
            AppAction::Refresh => self.apply_effects(vec![Effect::GitRefresh]),
            AppAction::MainMenu => self.toggle_main_menu(),
            AppAction::About => self.notify(Level::Info, format!("jetl {}", env!("CARGO_PKG_VERSION"))),
            AppAction::OpenSettings => self.open_settings(),
            AppAction::KeymapReference => self.keymap_reference(),
            AppAction::NewScratch => self.new_scratch(),
            AppAction::LocalHistory => self.local_history(),
            AppAction::PutLabel => self.put_label(),
        }
    }

    fn open_settings(&mut self) {
        let path = self.paths.config_file();
        if !path.exists() {
            let created =
                path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, config_reload::SETTINGS_STUB));
            if let Err(e) = created {
                return self.notify(Level::Error, format!("create {}: {e}", path.display()));
            }
            // Its own write is not a change for hot reload.
            self.settings_created(config_reload::SETTINGS_STUB);
        }
        self.open_file(path, true);
    }

    fn keymap_reference(&mut self) {
        let local = self.workspace.root.join("docs/KEYMAP.md");
        if local.is_file() {
            self.open_file(local, true);
        } else {
            let url = concat!(env!("CARGO_PKG_REPOSITORY"), "/src/main/docs/KEYMAP.md");
            self.notify(Level::Info, format!("Keymap reference: {url}"));
        }
    }

    /// JetBrains semantics: hidden -> show and focus; visible but unfocused ->
    /// focus; focused -> hide and return to the editor.
    pub fn toggle_tool_window(&mut self, tw: ToolWindowId) {
        let panel = tw.panel();
        if tw == ToolWindowId::Terminal {
            // Opening the window after the shell ended starts a new one.
            self.terminal.restart();
        }
        if !self.layout.is_visible(tw) {
            self.layout.show(tw);
            self.focus.set(panel);
            if tw == ToolWindowId::LocalHistory {
                self.history_window_opened();
            }
        } else if !self.focus.is(panel) {
            self.focus.set(panel);
        } else {
            self.layout.hide(tw);
            self.focus.set(PanelId::Editor);
        }
    }

    /// Zen mode: hide the tool windows, tabs, strip and status bar, and focus
    /// the editor; leaving puts back the windows and the focus. Opening a
    /// tool window meanwhile also leaves (`LayoutState::show`), and the
    /// caller then focuses that window.
    fn toggle_zen(&mut self) {
        if let Some(focus) = self.layout.leave_zen() {
            // A session restored meanwhile may have dropped the window.
            let shown = focus.tool_window().is_none_or(|tw| self.layout.is_visible(tw));
            self.focus.set(if shown { focus } else { PanelId::Editor });
        } else {
            self.layout.enter_zen(self.focus.current);
            self.focus.set(PanelId::Editor);
        }
    }

    /// Ctrl+Shift+arrows, JetBrains-style: move the free edge of the focused
    /// tool window (width for a left window, height for a bottom one). Keys
    /// along the other axis, and any with the editor focused, do nothing.
    fn resize_tool_window(&mut self, dir: Dir) {
        const STEP_W: i16 = 2;
        const STEP_H: i16 = 1;
        let Some(tw) = self.focus.current.tool_window().filter(|tw| self.layout.is_visible(*tw)) else { return };
        let (id, delta) = match (tw.side(), dir) {
            (Side::Left, Dir::Right) => (SplitterId::Left, STEP_W),
            (Side::Left, Dir::Left) => (SplitterId::Left, -STEP_W),
            (Side::Bottom, Dir::Up) => (SplitterId::Bottom, STEP_H),
            (Side::Bottom, Dir::Down) => (SplitterId::Bottom, -STEP_H),
            _ => return,
        };
        self.layout.resize(id, delta);
    }

    /// `git.file_history`: the selected tree entry when the tree has focus,
    /// else the active file.
    fn file_history(&mut self) {
        let path = if self.focus.is(PanelId::Project) {
            self.project.selected_row().map(|r| r.path.clone())
        } else {
            self.editor.active_path().map(|p| p.to_path_buf())
        };
        let Some(path) = path else {
            self.notify(Level::Info, "No file selected");
            return;
        };
        let Ok(rel) = path.strip_prefix(&self.workspace.root) else {
            self.notify(Level::Warn, format!("{} is outside the project", path.display()));
            return;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        self.layout.show(ToolWindowId::Git);
        self.focus.set(PanelId::Git);
        let mut cx = ctx!(self);
        self.git_panel.show_history(&rel, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    /// Show and focus the Event Log with entry `i` selected.
    pub(crate) fn open_event_log(&mut self, i: usize) {
        self.layout.show(ToolWindowId::EventLog);
        self.focus.set(PanelId::EventLog);
        self.event_log.state.set_len(self.notifications.len());
        self.event_log.select(i);
    }

    /// `event_log.copy`: the selected entry's full text. The confirmation is
    /// a toast only, so copying does not add to the log it copies from.
    fn copy_event(&mut self) {
        let Some(n) = self.notifications.get(self.event_log.state.selected) else { return };
        let text = n.text.clone();
        self.copy_to_clipboard(&text);
        self.toasts.push(Level::Info, "Copied the entry to the clipboard", Instant::now());
    }

    fn select_in_project(&mut self) {
        let Some(p) = self.editor.active_path().map(|p| p.to_path_buf()) else {
            self.notify(Level::Info, "No file is open");
            return;
        };
        self.layout.show(ToolWindowId::Project);
        self.focus.set(PanelId::Project);
        if p.starts_with(self.paths.scratches_dir()) {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.notify(Level::Info, format!("{name} is a scratch file, outside every project (File > Scratch Files…)"));
        } else if !self.project.select_path(&p) {
            self.notify(Level::Warn, format!("{} is outside the project", p.display()));
        }
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
        let paths = Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), cache_dir: dir.join("cache"), omarchy_dir: None };
        let open = open(&root);
        // No language servers unless a test installs fakes
        // (`lsp::tests::with_fake_servers`).
        let mut config = Config::default();
        config.lsp.enabled = false;
        let mut app = App::new(root.clone(), config, paths, open);
        // `App::new`'s worker queued a prune of this test's store: let it run
        // (on no store yet) before that worker is dropped, or it may run late
        // and hold `prune.lock` while `with_history`'s worker replaces an
        // entry, which then appends one instead.
        assert!(app.history.flush(Duration::from_secs(5)), "the startup prune did not finish");
        // Never the user's clipboard tools.
        app.clipboard.tools = Some(crate::util::clipboard::Tools { paste: crate::util::clipboard::Paste::Disabled, copy: None });
        // No local history unless a test turns it on (`with_history`): the
        // worker would write into the temp dir after the test removed it.
        app.history = crate::history::Handle::disabled();
        app.git.history = app.history.clone();
        (app, dir)
    }

    /// Local history on, in the test's temp state dir (`paths.history_dir`).
    pub(crate) fn with_history(app: &mut App) {
        app.history = crate::history::Handle::spawn(app.paths.history_dir(), app.config.local_history.clone(), app.bus_tx.clone());
        app.git.history = app.history.clone();
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
        app.editor.show_slot(0);
        let g = app.editor.focused_group().id;
        app.close_tab_slot(g, 1);
        let d = app.dialog.take().expect("dirty close asks");
        // A tab opens next to the active one while the dialog is up.
        app.handle_msg(load("c.txt"));
        let names = |app: &App| app.editor.tab_names();
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
    pub(crate) fn load(app: &mut App, name: &str, text: &str) -> PathBuf {
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
        app.handle_msg(AppMsg::RecentProjects { seq: 1, roots: vec![root.clone()], error: None });
        assert!(app.palette.is_none());

        app.pick_project_at_startup();
        app.handle_msg(AppMsg::RecentProjects { seq: 2, roots: vec![root.clone(), other.clone()], error: None });
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::RecentProjects));
        // A late reply to an older request does not replace the list.
        app.handle_msg(AppMsg::RecentProjects { seq: 1, roots: vec![root.clone()], error: None });
        let res = app.palette.as_mut().unwrap().handle_action(&PaletteAction::Accept).unwrap();
        assert_eq!(res, PaletteResult::Accept(Accept::SwitchRoot(other.clone())));
        app.finish_palette(res);
        assert_eq!(app.workspace.root, other);
        // Offered once only.
        app.handle_msg(AppMsg::RecentProjects { seq: 3, roots: vec![other.clone(), root.clone()], error: None });
        assert!(app.palette.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn recent_projects_are_recorded_only_once_started() {
        let recent = |m: &AppMsg| matches!(m, AppMsg::RecentProjects { .. });
        let (mut app, dir) = test_app("recent-record");
        let (root, file) = (app.workspace.root.clone(), app.paths.recent_projects_file());
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other = std::fs::canonicalize(&other).unwrap();
        // Tests never start it: a root switch loads the list but writes nothing.
        app.do_switch_root(other.clone());
        handle_next(&mut app, recent).await;
        assert!(!file.exists(), "nothing recorded before start_recent_projects");
        // What main.rs does at startup; root switches record from then on.
        app.start_recent_projects();
        handle_next(&mut app, recent).await;
        assert_eq!(app.recent_projects, vec![other.clone()]);
        app.do_switch_root(root.clone());
        handle_next(&mut app, recent).await;
        let roots: Vec<PathBuf> = crate::recent_projects::load(&file).into_iter().map(|p| p.root).collect();
        assert_eq!(roots, [root, other]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn shift_delete_forgets_a_recent_project_and_keeps_the_palette_open() {
        use crate::actions::PaletteKind;
        use crate::ui::palette::Source;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let recent = |m: &AppMsg| matches!(m, AppMsg::RecentProjects { .. });
        let shift_delete = |app: &mut App| app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::SHIFT));
        let (mut app, dir) = test_app("recent-forget");
        let (root, file) = (app.workspace.root.clone(), app.paths.recent_projects_file());
        let [a, b, c] = ["a", "b", "c"].map(|n| {
            std::fs::create_dir_all(dir.join(n)).unwrap();
            std::fs::canonicalize(dir.join(n)).unwrap()
        });
        for (i, r) in [&a, &b, &c, &root].into_iter().enumerate() {
            crate::recent_projects::update(&file, Some(r), i as u64 + 1);
        }
        let on_disk = || crate::recent_projects::load(&file).into_iter().map(|p| p.root).collect::<Vec<_>>();
        app.open_palette(PaletteKind::RecentProjects);
        handle_next(&mut app, recent).await;
        // Rows c, b, a (the current root is not listed): forget b.
        app.palette.as_mut().unwrap().handle_action(&crate::actions::PaletteAction::Down);
        shift_delete(&mut app);
        let p = app.palette.as_ref().expect("stays open");
        assert_eq!((p.kind, p.rows_selected()), (PaletteKind::RecentProjects, (2, 1)));
        assert_eq!(app.recent_projects, [root.clone(), c.clone(), a.clone()], "dropped at once");
        handle_next(&mut app, recent).await;
        assert_eq!(on_disk(), [root.clone(), c.clone(), a.clone()]);
        assert_eq!(app.recent_projects, [root.clone(), c.clone(), a.clone()]);
        assert_eq!(app.palette.as_ref().map(|p| p.rows_selected().0), Some(2));

        // A late reply to a load asked before the forget does not bring it back.
        app.load_recent_projects(false);
        shift_delete(&mut app);
        assert_eq!(app.palette.as_ref().map(|p| p.rows_selected().0), Some(1));
        handle_next(&mut app, recent).await;
        handle_next(&mut app, recent).await;
        assert_eq!(on_disk(), [root.clone(), c.clone()]);
        assert_eq!(app.recent_projects, [root.clone(), c.clone()]);
        assert_eq!(app.palette.as_ref().map(|p| p.rows_selected().0), Some(1));

        // A failed write warns and lists the file as it is.
        std::fs::create_dir_all(file.with_extension("json.tmp")).unwrap();
        shift_delete(&mut app);
        assert_eq!(app.palette.as_ref().map(|p| p.rows_selected().0), Some(0));
        handle_next(&mut app, recent).await;
        let n = app.notifications.last().expect("a warning");
        assert!(n.level == Level::Warn && n.text.contains("recent projects"), "{}", n.text);
        assert_eq!(on_disk(), [root.clone(), c.clone()]);
        assert_eq!(app.palette.as_ref().map(|p| p.rows_selected().0), Some(1));

        // Other sources still delete forward in the query.
        app.open_palette(PaletteKind::Actions);
        for code in [KeyCode::Char('a'), KeyCode::Char('b'), KeyCode::Home] {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        shift_delete(&mut app);
        let p = app.palette.as_ref().expect("stays open");
        assert_eq!((p.source(), p.input.text.as_str()), (Source::Actions, "b"));
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
    pub(crate) fn run_find(app: &mut App) {
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
    pub(crate) async fn handle_next(app: &mut App, want: impl Fn(&AppMsg) -> bool) {
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
        assert_eq!(app.project.selected_row().map(|r| r.path.clone()), Some(b.clone()));
        // A new extension brings its language.
        #[cfg(feature = "lang-rust")]
        {
            app.finish_dialog(DialogId::TreeRename(b), DialogResult::Text("b.rs".into()));
            handle_next(&mut app, fs_op_done).await;
            assert_eq!(app.editor.status_language().as_deref(), Some("rust"));
            assert_eq!(app.editor.buffers[0].text(), "mine a\n");
        }
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
    async fn f10_opens_the_main_menu_and_about_shows_the_version() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("main-menu");
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        // The action closes a context menu and toggles.
        app.menu = Some(crate::ui::menu::ContextMenu::new(Vec::new(), 0, 0));
        app.perform(Action::App(AppAction::MainMenu));
        assert!(app.menu.is_none() && app.menu_bar.is_some());
        app.perform(Action::App(AppAction::MainMenu));
        assert!(app.menu_bar.is_none());
        app.handle_key(key(KeyCode::F(10)));
        let bar = app.menu_bar.as_ref().expect("menu bar open");
        assert_eq!(bar.open_title(), "File");
        let save = bar.dropdown().items.iter().find(|i| i.label == "Save").unwrap();
        assert!(!save.enabled, "no tab is open");
        assert_eq!(save.shortcut, "Ctrl+S");
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.menu_bar.as_ref().map(|b| b.open_title()), Some("Edit"));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.menu_bar.is_none());
        app.handle_key(key(KeyCode::F(10)));
        app.handle_key(key(KeyCode::F(10)));
        assert!(app.menu_bar.is_none(), "F10 toggles");
        // Drawn in zen mode too, over the editor's top row.
        app.perform(Action::App(AppAction::ToggleZen));
        app.handle_key(key(KeyCode::F(10)));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let frame = term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let top = crate::ui::snapshot::row_text(frame.buffer, 0, 31);
        assert!(top.starts_with(" ≣  File  Edit  View  Navigate"), "{top:?}");
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.menu_bar.is_none());
        // No docs/KEYMAP.md in this project: the online copy.
        assert!(app.notifications.last().is_some_and(|n| n.text.ends_with("/docs/KEYMAP.md")), "{:?}", app.notifications);
        app.handle_key(key(KeyCode::F(10)));
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        let version = format!("jetl {}", env!("CARGO_PKG_VERSION"));
        assert!(app.notifications.last().is_some_and(|n| n.level == Level::Info && n.text == version), "{:?}", app.notifications);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn open_settings_creates_and_opens_config_toml() {
        let (mut app, dir) = test_app("open-settings");
        let path = app.paths.config_file();
        assert!(!path.exists());
        app.perform(Action::App(AppAction::OpenSettings));
        assert!(std::fs::read_to_string(&path).unwrap().contains("docs/CONFIG.md"));
        let msg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match app.bus_rx.recv().await {
                    Some(m @ AppMsg::FileLoaded { .. }) => break m,
                    Some(_) => {}
                    None => panic!("bus closed"),
                }
            }
        })
        .await
        .unwrap();
        app.handle_msg(msg);
        assert_eq!(app.editor.active_buffer().and_then(|b| b.path.clone()), Some(path));
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

    fn symbol(name: &str, line: usize) -> crate::symbols::Symbol {
        crate::symbols::Symbol { name: name.into(), kind: "function", line, col: 3, start_line: line, end_line: line, depth: 0 }
    }

    fn symbol_names(app: &mut App) -> Vec<String> {
        app.symbols.entries().iter().map(|e| e.symbol.name.to_string()).collect()
    }

    #[tokio::test]
    async fn stale_symbol_batches_are_dropped() {
        use crate::symbols::index::{FileSymbols, SymbolBatch};
        let (mut app, dir) = test_app("symbols-stale");
        let (root, g) = (app.workspace.root.clone(), app.workspace.generation);
        app.symbols.set_files(&["a.rs".to_string()]);
        let first = app.symbols.start(root.clone()).seq;
        let second = app.symbols.start(root).seq;
        let batch = |name: &str| SymbolBatch {
            files: vec![("a.rs".into(), Some(FileSymbols { mtime: None, symbols: vec![symbol(name, 0)] }))],
            done: true,
        };
        app.handle_msg(AppMsg::Symbols { generation: g, seq: first, batch: batch("older run") });
        assert!(symbol_names(&mut app).is_empty(), "a batch of an older run is dropped");
        app.handle_msg(AppMsg::Symbols { generation: g - 1, seq: second, batch: batch("other root") });
        assert!(symbol_names(&mut app).is_empty(), "a batch of the previous root is dropped");
        app.handle_msg(AppMsg::Symbols { generation: g, seq: second, batch: batch("current") });
        assert_eq!(symbol_names(&mut app), ["current"]);
        assert!(!app.symbols.indexing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn symbol_palette_jumps_and_changed_files_are_parsed_again() {
        use crate::actions::{PaletteAction, PaletteKind};
        use crate::files::watch::Changes;
        use crate::ui::palette::{Accept, PaletteResult};
        let (mut app, dir) = test_app("symbols-palette");
        let (root, g) = (app.workspace.root.clone(), app.workspace.generation);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let path = root.join("src/effects.rs");
        std::fs::write(&path, "fn other() {}\n\nimpl App {\n    fn apply_effects(&mut self) {}\n}\n").unwrap();
        std::fs::write(root.join("big.rs"), format!("fn big() {{}}\n{}", "//\n".repeat(200_000))).unwrap();
        app.handle_msg(AppMsg::FileIndex {
            generation: g,
            seq: 1,
            files: vec!["README.md".into(), "big.rs".into(), "src/effects.rs".into()],
        });
        // Run the index on this thread (the app started it in the background).
        let run = |app: &mut App| {
            let job = app.symbols.start(app.workspace.root.clone());
            let (seq, mut batches) = (job.seq, Vec::new());
            job.run(|b| batches.push(b));
            for batch in batches {
                app.handle_msg(AppMsg::Symbols { generation: g, seq, batch });
            }
        };
        run(&mut app);
        assert_eq!(symbol_names(&mut app), ["other", "apply_effects"], "big.rs is over the size limit");
        app.perform(Action::App(AppAction::OpenPalette(PaletteKind::Symbols)));
        for c in "apply".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), crossterm::event::KeyModifiers::NONE));
        }
        let res = app.palette.as_mut().unwrap().handle_action(&PaletteAction::Accept).unwrap();
        assert_eq!(res, PaletteResult::Accept(Accept::OpenFileAt { path: path.clone(), line: 3, col: 7 }));
        app.finish_palette(res);
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(app.editor.active_path(), Some(path.as_path()));
        assert_eq!(app.editor.status_position().as_deref(), Some("Ln 4, Col 8"));
        // An outside change drops the file's symbols until it is parsed again.
        std::fs::write(&path, "fn renamed() {}\n").unwrap();
        app.handle_msg(AppMsg::FsChanged { generation: g, changes: Changes { paths: vec![path.clone()], git: false } });
        assert_eq!(symbol_names(&mut app), [] as [&str; 0]);
        run(&mut app);
        assert_eq!(symbol_names(&mut app), ["renamed"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn structure_lists_the_active_buffer_and_follows_edits_and_tabs() {
        let (mut app, dir) = test_app("structure");
        let names = |app: &App| app.structure.symbols.iter().map(|s| (s.name.to_string(), s.depth)).collect::<Vec<_>>();
        load(&mut app, "a.rs", "fn one() {}\nmod m {\n    fn two() {}\n}\n");
        app.perform(Action::App(AppAction::ToggleToolWindow(ToolWindowId::Structure)));
        assert!(app.layout.is_visible(ToolWindowId::Structure) && app.focus.is(PanelId::Structure));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        assert_eq!(names(&app), [("one".into(), 0), ("m".into(), 0), ("two".into(), 1)]);
        // Enter jumps to the selected symbol.
        app.handle_key(KeyEvent::new(KeyCode::End, crossterm::event::KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE));
        assert!(app.focus.is(PanelId::Editor));
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col(), (2, 7));
        // An edit shows on the next frame.
        app.perform(Action::Editor(EditorAction::Move(crate::actions::Motion::DocStart, false)));
        type_text(&mut app, "fn zero() {}\n");
        app.sync_structure();
        assert_eq!(names(&app)[0], ("zero".into(), 0));
        // The caret is now on `fn one`, below the inserted line.
        assert_eq!(app.structure.selected().map(|s| s.name.to_string()).as_deref(), Some("one"), "follows the caret");
        // Another tab, then plain text.
        load(&mut app, "b.rs", "struct B;\n");
        app.sync_structure();
        assert_eq!(names(&app), [("B".into(), 0)]);
        load(&mut app, "c.txt", "text\n");
        app.sync_structure();
        assert!(names(&app).is_empty());
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

    #[tokio::test]
    async fn push_without_upstream_asks_for_the_remote() {
        use crate::git::repo::PushRemote;
        let (mut app, dir) = test_app("push-remote");
        let g = app.git.generation;
        let msg = |result| AppMsg::Git(GitMsg::PushRemote { generation: g, branch: "feat".into(), result: Ok(result) });
        app.handle_msg(msg(PushRemote::One("fork".into())));
        let d = app.dialog.take().expect("confirm");
        assert_eq!(d.id, DialogId::PushSetUpstream { branch: "feat".into(), remote: "fork".into() });
        assert!(d.message.contains("fork/feat"));
        app.handle_msg(msg(PushRemote::Choose(vec!["a".into(), "b".into()])));
        let d = app.dialog.take().expect("choice");
        assert_eq!(d.id, DialogId::PushUpstreamRemote { branch: "feat".into(), remotes: vec!["a".into(), "b".into()] });
        app.handle_msg(msg(PushRemote::NoRemote));
        assert!(app.dialog.is_none());
        assert!(app.notifications.last().is_some_and(|n| n.text.starts_with("No remote configured")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Make the test app's root a repository with `files` committed.
    pub(crate) fn init_repo(app: &App, files: &[(&str, &str)]) {
        let root = &app.workspace.root;
        let repo = git2::Repository::init(root).unwrap();
        let mut cfg = git2::Config::open(&root.join(".git/config")).unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        for (name, text) in files {
            std::fs::write(root.join(name), text).unwrap();
        }
        crate::git::repo::stage_all(root).unwrap();
        crate::git::repo::commit(root, "init").unwrap();
        drop(repo);
    }

    /// Feed the app a fresh status of its root, as the service would.
    pub(crate) fn feed_status(app: &mut App, seq: u64) {
        let result = crate::git::repo::status(&app.workspace.root);
        app.handle_msg(AppMsg::Git(GitMsg::Status { generation: app.git.generation, seq, result }));
    }

    fn git_op_done(m: &AppMsg) -> bool {
        matches!(m, AppMsg::Git(GitMsg::OpDone { .. }))
    }

    #[tokio::test]
    async fn discard_asks_then_restores_the_file_and_reloads_its_tab() {
        use crate::actions::GitAction;
        let (mut app, dir) = test_app("discard");
        init_repo(&app, &[("a.txt", "a\n")]);
        let a = load(&mut app, "a.txt", "changed\n");
        feed_status(&mut app, 1);
        app.focus.set(PanelId::Git);
        // The selection lands on the first entry.
        app.perform(Action::Git(GitAction::Discard));
        let d = app.dialog.take().expect("asks first");
        assert_eq!(d.id, DialogId::GitDiscard(vec![(crate::git::Section::Unstaged, "a.txt".into())]));
        assert!(d.message.contains("a.txt"));
        app.finish_dialog(d.id.clone(), DialogResult::Button(1));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "changed\n", "Cancel keeps it");
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, git_op_done).await;
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n");
        assert_eq!(app.editor.buffers[0].text(), "a\n", "the clean tab reloads");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ctrl_shift_arrows_resize_the_focused_tool_window() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("resize-tool-window");
        let key = |code| KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        app.layout.compute(Rect::new(0, 0, 120, 30));
        app.toggle_tool_window(ToolWindowId::Git);
        let (w, h) = (app.layout.left_w, app.layout.bottom_h);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.layout.bottom_h, h + 1);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.layout.bottom_h, h - 1);
        app.handle_key(key(KeyCode::Left));
        assert_eq!((app.layout.left_w, app.layout.bottom_h), (w, h - 1), "a bottom window has no width to change");
        for _ in 0..40 {
            app.handle_key(key(KeyCode::Up));
        }
        assert_eq!(app.layout.bottom_h, 30 - 1 - crate::ui::layout::MIN_CENTER_H - 1, "clamped like a drag");
        // The commit message box leaves the chord to the keymap.
        app.perform(Action::Git(crate::actions::GitAction::FocusMessage));
        assert!(app.git_panel.message_focused());
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.layout.bottom_h, 30 - 1 - crate::ui::layout::MIN_CENTER_H - 2);
        // A left window changes its width.
        app.toggle_tool_window(ToolWindowId::Project);
        assert!(app.focus.is(PanelId::Project));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.layout.left_w, w + 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.layout.left_w, w + 2);
        for _ in 0..40 {
            app.handle_key(key(KeyCode::Left));
        }
        assert_eq!(app.layout.left_w, crate::ui::layout::MIN_LEFT_W);
        // In the editor the chord keeps its meaning: Left/Right select by
        // word, Up/Down do nothing.
        app.focus.set(PanelId::Editor);
        let (w, h) = (app.layout.left_w, app.layout.bottom_h);
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Up, KeyCode::Down] {
            app.handle_key(key(code));
        }
        assert_eq!((app.layout.left_w, app.layout.bottom_h), (w, h));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_j_adds_carets_that_type_together_and_escape_drops_them() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("carets");
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path, "let a = 1;\na + a;\n", false, &app.theme);
        app.focus.set(PanelId::Editor);
        app.editor.active_buffer_mut().unwrap().goto_line(0, 4);
        let alt = |c: char, m: KeyModifiers| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT | m);
        for _ in 0..3 {
            app.handle_key(alt('j', KeyModifiers::NONE));
        }
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.view.sels.len(), 3, "the word, then two more");
        assert_eq!(app.editor.status_position().as_deref(), Some("Ln 2, Col 6 (3 carets, 3 chars)"));
        app.handle_key(alt('J', KeyModifiers::SHIFT));
        assert_eq!(app.editor.active_buffer().unwrap().view.sels.len(), 2, "Alt+Shift+J takes the last one back");
        // Ctrl+Alt+J (the legacy key) takes them all.
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT | KeyModifiers::CONTROL));
        assert_eq!(app.editor.active_buffer().unwrap().view.sels.len(), 3);
        for c in "bc".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.editor.active_buffer().unwrap().text(), "let bc = 1;\nbc + bc;\n");
        app.perform(Action::Editor(EditorAction::Undo));
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.text(), "let a = 1;\na + a;\n", "one undo step");
        assert_eq!(b.view.sels.len(), 3, "with every caret back");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.editor.active_buffer().unwrap().view.sels.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn zen_mode_shows_only_text_and_puts_the_layout_back() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("zen");
        let zen = || KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path, "alpha\nbeta gamma\n", false, &app.theme);
        app.toggle_tool_window(ToolWindowId::Git);
        app.layout.left_w = 33;
        let before = (app.layout.left, app.layout.left_w, app.layout.bottom, app.layout.bottom_h);
        assert_eq!((before.0, before.2), (Some(ToolWindowId::Project), Some(ToolWindowId::Git)));
        assert!(app.focus.is(PanelId::Git));

        app.handle_key(zen());
        assert!(app.layout.zen.is_some() && app.focus.is(PanelId::Editor));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let frame = term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let rows: Vec<String> = (0..30u16).map(|y| (0..100u16).map(|x| frame.buffer[(x, y)].symbol()).collect::<String>()).collect();
        assert!(rows[0].contains("alpha") && rows[1].contains("beta gamma"), "text starts on the top row: {rows:#?}");
        let name = app.workspace.name();
        assert!(!rows.iter().any(|r| r.contains("a.txt") || r.contains(&name)), "no tabs or status bar: {rows:#?}");
        assert!(app.layout.strip_hits.is_empty() && app.status_hits.is_empty());
        assert_eq!(app.layout.rects.editor, Rect::new(0, 0, 100, 30));
        // A click lands on the character drawn under it.
        let x = rows[1].find("gamma").unwrap() as u16;
        let click = |kind| MouseEvent { kind, column: x, row: 1, modifiers: KeyModifiers::NONE };
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)));
        app.handle_mouse(click(MouseEventKind::Up(MouseButton::Left)));
        assert_eq!(app.editor.buffers[0].cursor_line_col(), (1, 5));

        // The session keeps the pre-zen layout.
        let saved = app.session().layout.unwrap();
        assert_eq!((saved.left, saved.left_w, saved.bottom, saved.bottom_h), before);

        // Toggling back restores the windows, sizes and focus.
        app.handle_key(zen());
        assert!(app.layout.zen.is_none());
        assert_eq!((app.layout.left, app.layout.left_w, app.layout.bottom, app.layout.bottom_h), before);
        assert!(app.focus.is(PanelId::Git));

        // A tool window key leaves zen with the rest of the layout and
        // focuses that window.
        app.handle_key(zen());
        app.handle_key(KeyEvent::new(KeyCode::Char('7'), KeyModifiers::ALT));
        assert!(app.layout.zen.is_none());
        assert_eq!((app.layout.left, app.layout.bottom), (Some(ToolWindowId::Structure), Some(ToolWindowId::Git)));
        assert!(app.focus.is(PanelId::Structure));
        // So does hide all, which puts everything back.
        app.handle_key(zen());
        app.perform(Action::App(AppAction::HideAllToolWindows));
        assert_eq!((app.layout.left, app.layout.bottom), (Some(ToolWindowId::Structure), Some(ToolWindowId::Git)));
        assert!(app.focus.is(PanelId::Structure));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn amend_prefills_head_message_and_history_recalls_messages() {
        use crate::actions::GitAction;
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("amend");
        init_repo(&app, &[("a.txt", "a\n")]);
        feed_status(&mut app, 1);
        let g = app.git.generation;
        app.perform(Action::Git(GitAction::FocusMessage));
        app.perform(Action::Git(GitAction::ToggleAmend));
        let head = crate::git::repo::head_message(&app.workspace.root);
        app.handle_msg(AppMsg::Git(GitMsg::HeadMessage { generation: g, result: head }));
        assert_eq!(app.git_panel.message.text, "init");
        // Amending needs no staged changes.
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        handle_next(&mut app, git_op_done).await;
        assert_eq!(crate::git::repo::head_message(&app.workspace.root).unwrap(), "init 2");
        assert!(app.git_panel.message.text.is_empty(), "the box empties after the amend");
        // The history was recorded (the app reloads it in the background).
        app.git_panel.set_history(crate::git::messages::load(&app.git.messages_dir, &app.workspace.root));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.git_panel.message.text, "init 2");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.git_panel.message.text, "init 2", "stops at the oldest");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
        assert_eq!(app.git_panel.message.text, "x", "back to the draft");
        // Turning amend off again drops an unedited prefill.
        app.git_panel.message.clear();
        app.perform(Action::Git(GitAction::ToggleAmend));
        let head = crate::git::repo::head_message(&app.workspace.root);
        app.handle_msg(AppMsg::Git(GitMsg::HeadMessage { generation: g, result: head }));
        assert_eq!(app.git_panel.message.text, "init 2");
        app.perform(Action::Git(GitAction::ToggleAmend));
        assert!(app.git_panel.message.text.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stash_push_pop_and_drop_round_trip() {
        use crate::actions::GitAction;
        let (mut app, dir) = test_app("stash");
        init_repo(&app, &[("a.txt", "a\n")]);
        let a = app.workspace.root.join("a.txt");
        std::fs::write(&a, "mine\n").unwrap();
        let g = app.git.generation;
        let feed_stashes = |app: &mut App| {
            let result = crate::git::repo::stashes(&app.workspace.root);
            app.handle_msg(AppMsg::Git(GitMsg::Stashes { generation: g, result }));
        };
        app.perform(Action::Git(GitAction::TabStash));
        app.perform(Action::Git(GitAction::StashPush));
        let d = app.dialog.take().expect("asks for a message");
        assert_eq!(d.id, DialogId::StashMessage);
        app.finish_dialog(d.id, DialogResult::Text("wip".into()));
        handle_next(&mut app, git_op_done).await;
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n");
        feed_stashes(&mut app);
        assert_eq!(app.git_panel.stash_count(), 1);
        app.perform(Action::Git(GitAction::StashApply));
        handle_next(&mut app, git_op_done).await;
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "mine\n");
        // Drop asks first.
        app.perform(Action::Git(GitAction::StashDrop));
        let d = app.dialog.take().expect("confirm");
        assert!(matches!(d.id, DialogId::StashDrop { .. }) && d.message.contains("wip"));
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, git_op_done).await;
        feed_stashes(&mut app);
        assert_eq!(app.git_panel.stash_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_conflicting_stash_pop_offers_the_resolver() {
        use crate::git::{GitRequest, repo};
        let (mut app, dir) = test_app("stash-conflict");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "mine\n").unwrap();
        repo::stash(&root, &repo::StashOp::Push { message: "wip".into() }).unwrap();
        std::fs::write(root.join("a.txt"), "head\n").unwrap();
        repo::stage_all(&root).unwrap();
        repo::commit(&root, "second").unwrap();
        let oid = repo::stashes(&root).unwrap()[0].oid.clone();
        app.git_request(GitRequest::Stash(repo::StashOp::Pop { oid }));
        handle_next(&mut app, git_op_done).await;
        let n = app.notifications.last().expect("a warning");
        assert_eq!(n.level, Level::Warn);
        assert_eq!(
            n.text,
            "The stash conflicts with your files: resolve the U files (Git > Resolve Conflicts); the stash entry is kept, drop it when done"
        );
        assert_eq!(repo::stashes(&root).unwrap().len(), 1, "the entry is kept");
        feed_status(&mut app, 1);
        let d = app.dialog.take().expect("Resolve Conflicts");
        assert_eq!(d.id, DialogId::ResolveConflictPick(vec!["a.txt".into()]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Feed the Commit tab the diff it asked for, computed on this thread.
    fn feed_change_diff(app: &mut App, section: crate::git::Section, rel: &str) {
        let (old, new) = crate::git::repo::change_texts(&app.workspace.root, section, rel, None).unwrap();
        let key = crate::git::DiffKey::Change { section, rel: rel.into(), old: None };
        let result = Ok(crate::git::diff::DiffDoc::build(&old, &new, 4));
        app.handle_msg(AppMsg::Git(GitMsg::Diff { generation: app.git.generation, key, result }));
    }

    #[tokio::test]
    async fn space_in_the_diff_stages_and_unstages_one_change() {
        use crate::actions::GitAction;
        use crate::git::{BlobWhich, Section};
        let (mut app, dir) = test_app("hunk-stage");
        let base: String = (1..=12).map(|i| format!("{i}\n")).collect();
        init_repo(&app, &[("f.txt", &base)]);
        let f = app.workspace.root.join("f.txt");
        std::fs::write(&f, base.replacen("2\n", "two\n", 1).replace("11\n", "eleven\n")).unwrap();
        feed_status(&mut app, 1);
        feed_change_diff(&mut app, Section::Unstaged, "f.txt");
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::NextSection));
        // The second change, then Space.
        app.perform(Action::Git(GitAction::NextHunk));
        app.perform(Action::Git(GitAction::ToggleStage));
        handle_next(&mut app, git_op_done).await;
        let index = crate::git::repo::blob_text(&app.workspace.root, &f, BlobWhich::Index).unwrap();
        assert_eq!(index, base.replace("11\n", "eleven\n"), "only the second change is staged");
        // Unstage it again from the Staged diff.
        feed_status(&mut app, 2);
        app.perform(Action::Git(GitAction::Escape));
        app.perform(Action::Git(GitAction::Up));
        app.perform(Action::Git(GitAction::Up));
        feed_change_diff(&mut app, Section::Staged, "f.txt");
        app.perform(Action::Git(GitAction::NextSection));
        app.perform(Action::Git(GitAction::ToggleStage));
        handle_next(&mut app, git_op_done).await;
        let index = crate::git::repo::blob_text(&app.workspace.root, &f, BlobWhich::Index).unwrap();
        assert_eq!(index, base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn blame_column_keeps_the_newest_result_and_opens_the_commit() {
        let (mut app, dir) = test_app("blame");
        init_repo(&app, &[("a.txt", "one\ntwo\n")]);
        let a = load(&mut app, "a.txt", "one\ntwo\n");
        app.perform(Action::Editor(EditorAction::ToggleBlame));
        assert!(app.editor.buffers[0].blame.is_some());
        let g = app.git.generation;
        let blame = crate::git::repo::blame(&app.workspace.root, &a);
        app.handle_msg(AppMsg::Git(GitMsg::Blame { generation: g, seq: 2, path: a.clone(), result: blame }));
        let stale = Err(anyhow::anyhow!("older request"));
        app.handle_msg(AppMsg::Git(GitMsg::Blame { generation: g, seq: 1, path: a.clone(), result: stale }));
        let state = app.editor.buffers[0].blame.as_ref().expect("still shown");
        let sha = state.commit_at(1).map(|c| c.sha.clone()).expect("line 2 has a commit");
        assert_eq!(state.commit_at(0).map(|c| c.author.as_str()), Some("Test"));
        // The column's commit opens in the Log tab.
        app.git_request(crate::git::GitRequest::ShowCommit(sha));
        assert_eq!(app.git_panel.tab, crate::git::panel::GitTab::Log);
        assert!(app.focus.is(PanelId::Git));
        // Toggling again hides it; a late result is dropped.
        app.perform(Action::Editor(EditorAction::ToggleBlame));
        let blame = crate::git::repo::blame(&app.workspace.root, &a);
        app.handle_msg(AppMsg::Git(GitMsg::Blame { generation: g, seq: 3, path: a, result: blame }));
        assert!(app.editor.buffers[0].blame.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn log_filters_file_history_and_commit_actions() {
        use crate::actions::GitAction;
        use crate::git::{LogFilter, LogRev};
        let (mut app, dir) = test_app("log-filter");
        init_repo(&app, &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        load(&mut app, "a.txt", "a\n");
        let g = app.git.generation;
        // History of the active file: the Log tab, filtered by its path.
        app.perform(Action::Git(GitAction::FileHistory));
        assert_eq!(app.git_panel.tab, crate::git::panel::GitTab::Log);
        let by_path = LogFilter { path: "a.txt".into(), ..Default::default() };
        // A page of another filter is dropped; the right one lands.
        let root = app.workspace.root.clone();
        let page = |filter: &LogFilter| {
            let out = std::process::Command::new("git")
                .args(crate::git::cli::log_args(0, 100, false, filter))
                .current_dir(&root)
                .output()
                .unwrap();
            crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout))
        };
        let other = page(&LogFilter::default());
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter: LogFilter::default(), result: Ok(other) }));
        assert!(!app.git_panel.log_loaded());
        let rows = page(&by_path);
        assert_eq!(rows.len(), 1);
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter: by_path.clone(), result: Ok(rows) }));
        assert!(app.git_panel.log_loaded());
        // Ctrl+F, Text: the filter keeps the path and adds the text.
        app.perform(Action::Git(GitAction::LogFilter));
        let d = app.dialog.take().unwrap();
        app.finish_dialog(d.id, DialogResult::Choice(1));
        let d = app.dialog.take().unwrap();
        assert_eq!(d.id, DialogId::LogFilterText);
        app.finish_dialog(d.id, DialogResult::Text(" init ".into()));
        let f = app.git_panel.log_filter();
        assert_eq!((f.path.as_str(), f.text.as_str(), &f.rev), ("a.txt", "init", &LogRev::Default));
        // Commit actions work on the selected commit (the page is reloaded
        // for the new filter first).
        let f = f.clone();
        let rows = page(&f);
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter: f, result: Ok(rows) }));
        app.perform(Action::Git(GitAction::LogRevert));
        let d = app.dialog.take().expect("revert asks");
        assert!(matches!(d.id, DialogId::RevertCommit { .. }) && d.message.contains("init"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn git(app: &App, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(&app.workspace.root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[tokio::test]
    async fn branch_delete_asks_to_force_and_merge_conflicts_show_as_u() {
        use crate::git::model::BranchOp;
        let (mut app, dir) = test_app("branches");
        init_repo(&app, &[("a.txt", "a\n")]);
        let main = crate::git::repo::status(&app.workspace.root).unwrap().branch.unwrap();
        git(&app, &["switch", "-qc", "dev"]);
        std::fs::write(app.workspace.root.join("a.txt"), "dev\n").unwrap();
        git(&app, &["commit", "-qam", "dev change"]);
        git(&app, &["switch", "-q", &main]);
        std::fs::write(app.workspace.root.join("a.txt"), "main\n").unwrap();
        git(&app, &["commit", "-qam", "main change"]);
        feed_status(&mut app, 1);
        let dev = crate::git::repo::branches(&app.workspace.root).unwrap().into_iter().find(|b| b.name == "dev").unwrap();
        // The popup lists the operations; Delete asks first.
        app.git_request(crate::git::GitRequest::BranchOps(dev.clone()));
        let d = app.dialog.take().expect("branch operations");
        let DialogId::BranchOps { ops, .. } = &d.id else { panic!("{:?}", d.id) };
        let delete = ops.iter().position(|o| *o == BranchOp::Delete).unwrap();
        app.finish_dialog(d.id.clone(), DialogResult::Choice(delete));
        let d = app.dialog.take().expect("confirm");
        assert_eq!(d.id, DialogId::DeleteBranch { name: "dev".into(), force: false });
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, git_op_done).await;
        // Not merged: git refuses, and the app offers to force it.
        let d = app.dialog.take().expect("force?");
        assert_eq!(d.id, DialogId::DeleteBranch { name: "dev".into(), force: true });
        app.finish_dialog(d.id, DialogResult::Cancel);
        // Merging it conflicts; the file shows as conflicted.
        app.git_request(crate::git::GitRequest::Merge("dev".into()));
        handle_next(&mut app, git_op_done).await;
        assert!(app.notifications.iter().any(|n| n.text.starts_with("Merge stopped on conflicts")), "{:?}", app.notifications);
        let snap = crate::git::repo::status(&app.workspace.root).unwrap();
        assert_eq!(snap.entries[0].status, crate::git::model::FileStatus::Conflicted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn event_log_lists_follows_clears_and_opens_from_the_status_bar() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("event-log");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.perform(Action::EventLog(EventLogAction::Clear));
        assert!(app.notifications.is_empty());
        for i in 0..30 {
            app.notify(Level::Info, format!("event {i}"));
        }
        app.notify(Level::Error, "push failed\nremote said no");
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::ALT | KeyModifiers::SHIFT));
        assert!(app.layout.is_visible(ToolWindowId::EventLog) && app.focus.is(PanelId::EventLog));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let screen = |term: &mut ratatui::Terminal<ratatui::backend::TestBackend>, app: &mut App| {
            let frame = term.draw(|f| crate::ui::render(app, f)).unwrap();
            let b = &frame.buffer;
            (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>()
        };
        let rows = screen(&mut term, &mut app);
        // From the bottom: toasts may show the same text above.
        let row_of = |rows: &[String], text: &str| rows.iter().rposition(|r| r.contains(text));
        // Newest last, in view, with the rest of a multi-line text marked.
        let (prev, last) = (row_of(&rows, "event 29").unwrap(), row_of(&rows, "push failed +1 line").unwrap());
        assert_eq!(last, prev + 1, "{rows:#?}");
        assert!(rows[last].contains('×'));
        assert_eq!(app.event_log.state.selected, 30);
        // Enter shows the full text.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.dialog.take().map(|d| d.message).as_deref(), Some("push failed\nremote said no"));
        // Moving up stops following; End resumes.
        app.handle_key(key(KeyCode::Up));
        app.notify(Level::Info, "later");
        assert_eq!(app.event_log.state.selected, 29);
        app.handle_key(key(KeyCode::End));
        app.notify(Level::Warn, "latest");
        assert_eq!(app.event_log.state.selected, 32);
        // So does wheeling away from the end.
        screen(&mut term, &mut app);
        let r = app.layout.rects.bottom;
        let wheel = MouseEvent { kind: MouseEventKind::ScrollUp, column: r.x + 5, row: r.y + 2, modifiers: KeyModifiers::NONE };
        app.handle_mouse(wheel);
        app.notify(Level::Info, "unseen");
        assert_eq!(app.event_log.state.selected, 32);
        let rows = screen(&mut term, &mut app);
        assert!(!rows[r.y as usize..r.bottom() as usize].iter().any(|row| row.contains("unseen")), "{rows:#?}");
        // Ctrl+Shift+Up grows it.
        let h = app.layout.bottom_h;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert_eq!(app.layout.bottom_h, h + 1);
        // Delete clears it.
        app.handle_key(key(KeyCode::Delete));
        assert!(app.notifications.is_empty() && app.event_log.state.len == 0);
        assert!(row_of(&screen(&mut term, &mut app), "No events").is_some());
        // A click on the status bar notification opens the log on it.
        app.notify(Level::Info, "one");
        app.notify(Level::Warn, "disk full");
        app.perform(Action::App(AppAction::HideFocusedToolWindow));
        app.event_log.select(0);
        screen(&mut term, &mut app);
        let (_, r) = *app.status_hits.iter().find(|(h, _)| *h == StatusHit::Notification).expect("notification shown");
        let click = |kind| MouseEvent { kind, column: r.x + 1, row: r.y, modifiers: KeyModifiers::NONE };
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)));
        app.handle_mouse(click(MouseEventKind::Up(MouseButton::Left)));
        assert!(app.layout.is_visible(ToolWindowId::EventLog) && app.focus.is(PanelId::EventLog));
        assert_eq!(app.notifications[app.event_log.state.selected].text, "disk full");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tab_bar_drag_pin_menu_and_session() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("tab-polish");
        for n in ["a.txt", "b.txt", "c.txt"] {
            load(&mut app, n, "x\n");
        }
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let mut draw = |app: &mut App| {
            let frame = term.draw(|f| crate::ui::render(app, f)).unwrap();
            let r = app.layout.rects.tabs;
            (r.x..r.right()).map(|x| frame.buffer[(x, r.y)].symbol().to_string()).collect::<String>()
        };
        let names = |app: &App| app.editor.tab_names();
        let active = |app: &App| app.editor.active_buffer().unwrap().name.clone();
        let tab = |app: &App, i| app.editor.focused_group().tab_hits.rect_of(i).unwrap();
        let ev = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let left = MouseButton::Left;
        draw(&mut app);
        // Drag c onto a: it moves to the front and is active.
        let (c, a) = (tab(&app, 2), tab(&app, 0));
        app.handle_mouse(ev(MouseEventKind::Down(left), c.x + 2, c.y));
        app.handle_mouse(ev(MouseEventKind::Drag(left), a.x + 1, a.y + 1));
        app.handle_mouse(ev(MouseEventKind::Up(left), a.x + 1, a.y + 1));
        assert_eq!(names(&app), ["c.txt", "a.txt", "b.txt"]);
        assert_eq!(active(&app), "c.txt");
        // A click without moving only activates.
        draw(&mut app);
        let b = tab(&app, 2);
        app.handle_mouse(ev(MouseEventKind::Down(left), b.x + 2, b.y));
        app.handle_mouse(ev(MouseEventKind::Up(left), b.x + 2, b.y));
        assert_eq!((names(&app), active(&app)), (vec!["c.txt".into(), "a.txt".into(), "b.txt".into()], "b.txt".into()));
        // Right-click a: its menu pins it, which moves it first and shows
        // the pin in place of the close button.
        let a = tab(&app, 1);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Right), a.x + 2, a.y));
        assert_eq!(app.menu.as_ref().map(|m| m.items[0].label.as_str()), Some("Pin Tab"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(names(&app), ["a.txt", "c.txt", "b.txt"]);
        assert!(app.editor.focused_group().views[0].pinned);
        assert!(draw(&mut app).starts_with("  a.txt ⚲ "));
        // The only pinned tab cannot move right; Alt+Shift+Left moves b
        // before c.
        app.perform(Action::Editor(EditorAction::MoveTabRight));
        assert_eq!(names(&app), ["a.txt", "c.txt", "b.txt"]);
        app.editor.show_slot(2);
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::SHIFT));
        assert_eq!(names(&app), ["a.txt", "b.txt", "c.txt"]);

        // The order and the pin survive a session round trip.
        let session = app.session();
        assert_eq!(session.files.iter().map(|f| f.pinned).collect::<Vec<_>>(), [true, false, false]);
        session.save(&app.paths.sessions_dir()).unwrap();
        let (mut fresh, fresh_dir) = test_app("tab-polish-2");
        fresh.workspace.root = app.workspace.root.clone();
        let loaded = crate::workspace::Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);
        let files = loaded.files.iter().map(|_| Ok(b"x\n".to_vec())).collect();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, files);
        assert_eq!(names(&fresh), ["a.txt", "b.txt", "c.txt"]);
        assert!(fresh.editor.focused_group().views[0].pinned && !fresh.editor.focused_group().views[1].pinned);
        assert_eq!(active(&fresh), "b.txt");
        // Close others keeps the pinned tab.
        fresh.perform(Action::Editor(EditorAction::CloseOtherTabs));
        assert_eq!(names(&fresh), ["a.txt", "b.txt"]);
        // A click on the pin unpins.
        draw(&mut fresh);
        let (_, pin) = *fresh.editor.focused_group().tab_hits.close.iter().find(|(i, _)| *i == 0).unwrap();
        fresh.handle_mouse(ev(MouseEventKind::Down(left), pin.x, pin.y));
        fresh.handle_mouse(ev(MouseEventKind::Up(left), pin.x, pin.y));
        assert_eq!(names(&fresh), ["a.txt", "b.txt"]);
        assert!(!fresh.editor.focused_group().views[0].pinned);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[tokio::test]
    async fn tab_limit_applies_to_opened_files_and_sessions() {
        let (mut app, dir) = test_app("tab-limit");
        let mut config = Config::default();
        config.editor.max_tabs = 2;
        app.editor = Editor::new(&config);
        for n in ["a.txt", "b.txt", "c.txt"] {
            load(&mut app, n, "x\n");
        }
        let names = |app: &App| app.editor.tab_names();
        assert_eq!(names(&app), ["b.txt", "c.txt"]);
        // A session keeps its most recent tabs, not its first ones.
        let root = app.workspace.root.clone();
        let file = |n: &str| crate::workspace::SessionFile { path: root.join(n), ..Default::default() };
        let session = crate::workspace::Session {
            root: root.clone(),
            files: vec![file("x.txt"), file("y.txt"), file("z.txt")],
            active: Some(0),
            recent: vec![0, 1, 2],
            ..Default::default()
        };
        app.editor.force_close_all();
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, (0..3).map(|_| Ok(b"x\n".to_vec())).collect());
        assert_eq!(names(&app), ["x.txt", "y.txt"]);
        assert_eq!(app.editor.active_buffer().unwrap().name, "x.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn split_groups_take_clicks_drags_and_the_wheel() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("split-mouse");
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        load(&mut app, "a.txt", &text);
        load(&mut app, "b.txt", "b\n");
        app.perform(Action::Editor(EditorAction::SplitRight));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let mut draw = |app: &mut App| {
            term.draw(|f| crate::ui::render(app, f)).unwrap();
            app.editor.check_invariants();
        };
        draw(&mut app);
        let ids = app.editor.group_ids();
        let (left, right) = (ids[0], ids[1]);
        assert_eq!(app.editor.focused_group().id, right);
        let ev = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let left_btn = MouseButton::Left;
        let click = |app: &mut App, x, y| {
            app.handle_mouse(ev(MouseEventKind::Down(left_btn), x, y));
            app.handle_mouse(ev(MouseEventKind::Up(left_btn), x, y));
        };
        // A tab click in the left group shows a.txt there and focuses it.
        let tab = app.editor.group(left).unwrap().tab_hits.rect_of(0).unwrap();
        click(&mut app, tab.x + 2, tab.y);
        assert_eq!((app.editor.focused_group().id, app.editor.active_buffer().unwrap().name.as_str()), (left, "a.txt"));
        // The right group gets a.txt too; a click in the left text focuses
        // the left group and puts its caret on the clicked cell.
        app.perform(Action::Editor(EditorAction::NextGroup));
        app.perform(Action::Editor(EditorAction::Switcher));
        assert_eq!(app.editor.active_buffer().unwrap().name, "a.txt");
        draw(&mut app);
        let ta = app.editor.group(left).unwrap().text_area;
        click(&mut app, ta.x + 3, ta.y + 2);
        assert_eq!(app.editor.focused_group().id, left);
        assert_eq!(app.editor.active_buffer().unwrap().cursor_line_col(), (2, 3));
        draw(&mut app);
        // The wheel over the right group scrolls it; focus stays left.
        let rt = app.editor.group(right).unwrap().text_area;
        app.handle_mouse(ev(MouseEventKind::ScrollDown, rt.x + 1, rt.y + 1));
        draw(&mut app);
        assert_eq!(app.editor.focused_group().id, left);
        assert_eq!(app.editor.active_buffer().unwrap().view.scroll_line, 0);
        // A drag that starts in the left text stays there: past the bar it
        // selects to the end of the left group's line.
        app.handle_mouse(ev(MouseEventKind::Down(left_btn), ta.x, ta.y + 4));
        app.handle_mouse(ev(MouseEventKind::Drag(left_btn), rt.x + 5, ta.y + 4));
        app.handle_mouse(ev(MouseEventKind::Up(left_btn), rt.x + 5, ta.y + 4));
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.selected_text(), "line 4");
        assert_eq!(app.editor.focused_group().id, left);
        app.perform(Action::Editor(EditorAction::NextGroup));
        let b = app.editor.active_buffer().unwrap();
        assert_eq!((b.view.scroll_line, b.view.sel().is_empty()), (3, true), "the right view kept its own scroll and caret");
        draw(&mut app);
        // Dragging the bar moves it.
        let bar = app.editor.group(left).unwrap().last_area.right();
        let y = ta.y + 5;
        app.handle_mouse(ev(MouseEventKind::Down(left_btn), bar, y));
        app.handle_mouse(ev(MouseEventKind::Drag(left_btn), bar + 10, y));
        app.handle_mouse(ev(MouseEventKind::Up(left_btn), bar + 10, y));
        draw(&mut app);
        assert_eq!(app.editor.group(left).unwrap().last_area.right(), bar + 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two split groups at 100x30: the left shows `files`, the right the
    /// last of them (one buffer in two groups) and then `right_only`.
    struct TabDrag {
        app: App,
        dir: PathBuf,
        term: ratatui::Terminal<ratatui::backend::TestBackend>,
        left: crate::editor::groups::GroupId,
        right: crate::editor::groups::GroupId,
    }

    impl TabDrag {
        fn new(name: &str, files: &[&str], right_only: &[&str]) -> Self {
            let (mut app, dir) = test_app(name);
            for f in files {
                load(&mut app, f, "x\n");
            }
            app.perform(Action::Editor(EditorAction::SplitRight));
            let term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            let ids = app.editor.group_ids();
            let mut t = Self { app, dir, term, left: ids[0], right: ids[1] };
            for f in right_only {
                load(&mut t.app, f, "x\n");
            }
            t.draw();
            t
        }

        fn draw(&mut self) {
            let app = &mut self.app;
            self.term.draw(|f| crate::ui::render(app, f)).unwrap();
            app.editor.check_invariants();
        }

        fn names(&self, g: crate::editor::groups::GroupId) -> Vec<String> {
            let ed = &self.app.editor;
            let group = ed.group(g).unwrap();
            group.views.iter().map(|v| ed.buffers.iter().find(|b| b.id == v.id).unwrap().name.clone()).collect()
        }

        /// The cell at `pct` percent of the width of group `g`'s tab `slot`.
        fn tab_cell(&self, g: crate::editor::groups::GroupId, slot: usize, pct: u16) -> (u16, u16) {
            let r = self.app.editor.group(g).unwrap().tab_hits.rect_of(slot).unwrap();
            (r.x + (r.width - 1) * pct / 100, r.y)
        }

        /// The last cell of group `g`'s tab bar (past its last tab).
        fn bar_end(&self, g: crate::editor::groups::GroupId) -> (u16, u16) {
            let bar = self.app.editor.group(g).unwrap().tab_area;
            (bar.right() - 1, bar.y)
        }

        fn ev(&mut self, kind: crossterm::event::MouseEventKind, (x, y): (u16, u16)) {
            let m = crossterm::event::MouseEvent { kind, column: x, row: y, modifiers: crossterm::event::KeyModifiers::NONE };
            self.app.handle_mouse(m);
        }

        /// Press on `from`, move through `via`, release on the last cell.
        fn drag(&mut self, from: (u16, u16), via: &[(u16, u16)]) {
            use crossterm::event::{MouseButton::Left, MouseEventKind::*};
            self.ev(Down(Left), from);
            for p in via {
                self.ev(Drag(Left), *p);
            }
            self.ev(Up(Left), *via.last().unwrap());
            self.draw();
        }
    }

    #[tokio::test]
    async fn tab_drag_reorders_within_a_group() {
        let mut t = TabDrag::new("tab-drag-reorder", &["a.txt", "b.txt", "c.txt"], &[]);
        let left = t.left;
        assert_eq!(t.names(left), ["a.txt", "b.txt", "c.txt"]);
        let from = t.tab_cell(left, 0, 50);
        let to = t.tab_cell(left, 2, 90);
        t.drag(from, &[to]);
        assert_eq!(t.names(left), ["b.txt", "c.txt", "a.txt"]);
        assert_eq!(t.names(t.right), ["c.txt"]);
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[tokio::test]
    async fn tab_drag_moves_to_another_bar_at_the_slot() {
        let mut t = TabDrag::new("tab-drag-move", &["a.txt", "b.txt", "c.txt"], &["d.txt"]);
        let (left, right) = (t.left, t.right);
        assert_eq!(t.names(right), ["c.txt", "d.txt"]);
        // Left half of d.txt: before it.
        let (from, to) = (t.tab_cell(left, 0, 50), t.tab_cell(right, 1, 10));
        t.drag(from, &[to]);
        assert_eq!(t.names(left), ["b.txt", "c.txt"]);
        assert_eq!(t.names(right), ["c.txt", "a.txt", "d.txt"]);
        assert_eq!(t.app.editor.focused_group().id, right);
        assert_eq!(t.app.editor.active_buffer().unwrap().name, "a.txt");
        // Right half of c.txt puts b.txt after it.
        let (from, to) = (t.tab_cell(left, 0, 50), t.tab_cell(right, 0, 90));
        t.drag(from, &[to]);
        assert_eq!(t.names(left), ["c.txt"]);
        assert_eq!(t.names(right), ["c.txt", "b.txt", "a.txt", "d.txt"]);
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[tokio::test]
    async fn tab_drag_keeps_pinned_tabs_in_the_pinned_range() {
        let mut t = TabDrag::new("tab-drag-pinned", &["a.txt", "b.txt", "c.txt"], &["d.txt"]);
        let (left, right) = (t.left, t.right);
        // c.txt (slot 0) is pinned in the right group: a.txt lands after it.
        t.app.editor.set_pinned_slot(right, 0, true);
        t.draw();
        let (from, to) = (t.tab_cell(left, 0, 50), t.tab_cell(right, 0, 10));
        t.drag(from, &[to]);
        assert_eq!(t.names(right), ["c.txt", "a.txt", "d.txt"]);
        // A pinned tab dropped past the last tab stays among the pinned.
        t.app.editor.set_pinned_slot(left, 0, true);
        t.draw();
        let (from, to) = (t.tab_cell(left, 0, 50), t.bar_end(right));
        t.drag(from, &[to]);
        assert_eq!(t.names(right), ["c.txt", "b.txt", "a.txt", "d.txt"]);
        let views = &t.app.editor.group(right).unwrap().views;
        assert_eq!(views.iter().map(|v| v.pinned).collect::<Vec<_>>(), [true, true, false, false]);
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[tokio::test]
    async fn tab_drag_onto_a_group_that_shows_the_buffer_drops_the_source_view() {
        let mut t = TabDrag::new("tab-drag-shared", &["a.txt", "b.txt", "c.txt"], &["d.txt"]);
        let (left, right) = (t.left, t.right);
        let (from, to) = (t.tab_cell(left, 2, 50), t.tab_cell(right, 1, 10));
        t.drag(from, &[to]);
        assert_eq!(t.names(left), ["a.txt", "b.txt"]);
        assert_eq!(t.names(right), ["c.txt", "d.txt"]);
        assert_eq!(t.app.editor.focused_group().id, right);
        assert_eq!(t.app.editor.active_buffer().unwrap().name, "c.txt");
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[tokio::test]
    async fn tab_drag_of_the_last_tab_collapses_the_source_group() {
        let mut t = TabDrag::new("tab-drag-collapse", &["a.txt", "b.txt"], &["d.txt"]);
        let (left, right) = (t.left, t.right);
        // Leave d.txt alone in the right group.
        t.app.editor.activate_slot(right, 0);
        t.app.perform(Action::Editor(EditorAction::CloseTab));
        t.draw();
        assert_eq!(t.names(right), ["d.txt"]);
        let (from, to) = (t.tab_cell(right, 0, 50), t.tab_cell(left, 1, 90));
        t.drag(from, &[to]);
        assert_eq!(t.app.editor.group_ids(), [left]);
        assert_eq!(t.names(left), ["a.txt", "b.txt", "d.txt"]);
        assert_eq!(t.app.editor.active_buffer().unwrap().name, "d.txt");
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[tokio::test]
    async fn tab_drop_on_another_body_appends_and_focuses() {
        use crossterm::event::{MouseButton::Left, MouseEventKind::*};
        let mut t = TabDrag::new("tab-drag-body", &["a.txt", "b.txt", "c.txt"], &["d.txt"]);
        let (left, right) = (t.left, t.right);
        let from = t.tab_cell(left, 1, 50);
        let body = t.app.editor.group(right).unwrap().text_area;
        let over = (body.x + 3, body.y + 2);
        t.ev(Down(Left), from);
        t.ev(Drag(Left), over);
        t.draw();
        assert_eq!(t.names(right), ["c.txt", "d.txt"], "hovering moves nothing");
        let bar = t.app.editor.group(right).unwrap().tab_area;
        let row: String = (bar.x..bar.right()).map(|x| t.term.backend().buffer()[(x, bar.y)].symbol().to_string()).collect();
        assert!(row.contains(" + b.txt "), "{row:?}");
        t.ev(Up(Left), over);
        t.draw();
        assert_eq!(t.names(left), ["a.txt", "c.txt"]);
        assert_eq!(t.names(right), ["c.txt", "d.txt", "b.txt"]);
        assert_eq!(t.app.editor.focused_group().id, right);
        assert_eq!(t.app.editor.active_buffer().unwrap().name, "b.txt");
        let _ = std::fs::remove_dir_all(&t.dir);
    }
}

#[cfg(test)]
mod strip_tests {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use super::tests::test_app;
    use crate::ui::focus::ToolWindowId;
    use crate::ui::layout::StripButton;
    use crate::ui::snapshot::row_text;

    fn ev(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    fn draw(app: &mut super::App) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone()
    }

    #[tokio::test]
    async fn icons_follow_the_config() {
        use crate::config::Icons;
        let (mut app, dir) = test_app("strip-icons");
        for (icons, project, bar) in [(Icons::Emoji, "📁", " ☰  File"), (Icons::Unicode, "⌂", " ≣  File")] {
            app.config.ui.icons = icons;
            let buf = draw(&mut app);
            let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Project)).unwrap();
            assert_eq!(row_text(&buf, r.y + 1, 3).trim(), project, "{icons:?}");
            // The button colours span the whole strip, behind a wide glyph too.
            assert!((0..3).all(|x| buf[(x, r.y + 1)].bg == buf[(0, r.y)].bg), "{icons:?}");
            assert_eq!(row_text(&buf, 0, 3).trim(), crate::ui::menu_bar::menu_glyph(icons));
            app.toggle_main_menu();
            let buf = draw(&mut app);
            assert!(row_text(&buf, 0, 20).starts_with(bar), "{icons:?}");
            app.toggle_main_menu();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn every_strip_button_fits_24_rows_and_clicks() {
        let (mut app, dir) = test_app("strip-fit");
        draw(&mut app);
        assert_eq!(app.layout.rects.strip.height, 23);
        let hits = app.layout.strip_hits.clone();
        assert_eq!(hits.len(), 1 + ToolWindowId::ALL.len(), "{hits:?}");
        assert_eq!(hits[0], (StripButton::Menu, ratatui::layout::Rect::new(0, 0, 3, 1)));
        for (i, (_, a)) in hits.iter().enumerate() {
            assert!(app.layout.rects.strip.contains(a.as_position()) && a.bottom() <= app.layout.rects.strip.bottom());
            assert!(hits[i + 1..].iter().all(|(_, b)| a.intersection(*b).is_empty()), "{hits:?}");
        }

        // The menu button opens the main menu; the bar then covers it.
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 1, 0));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 1, 0));
        assert!(app.menu_bar.is_some());
        app.toggle_main_menu();
        // A tool button toggles its window, on either row.
        let (_, r) = *hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Git)).unwrap();
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), r.x + 1, r.y + 1));
        assert!(app.layout.is_visible(ToolWindowId::Git));
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), r.x + 1, r.y));
        assert!(!app.layout.is_visible(ToolWindowId::Git));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hovering_a_strip_button_shows_its_name_and_shortcut() {
        let (mut app, dir) = test_app("strip-hover");
        draw(&mut app);
        let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Project)).unwrap();
        app.dirty_frame = false;
        app.handle_mouse(ev(MouseEventKind::Moved, r.x, r.y + 1));
        assert_eq!(app.strip_hover, Some((StripButton::Tool(ToolWindowId::Project), r)));
        assert!(app.dirty_frame);
        // Motion within the same button does not redraw.
        app.dirty_frame = false;
        app.handle_mouse(ev(MouseEventKind::Moved, r.x + 2, r.y));
        assert!(!app.dirty_frame);
        let buf = draw(&mut app);
        let row: String = (r.right()..r.right() + 16).map(|x| buf[(x, r.y)].symbol().to_string()).collect();
        assert_eq!(row, " Project  Alt+1 ");

        app.handle_mouse(ev(MouseEventKind::Moved, 1, 0));
        assert_eq!(app.strip_hover.map(|(b, _)| b), Some(StripButton::Menu));
        let buf = draw(&mut app);
        let row: String = (3..19).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, " Main Menu  F10 ");

        // Leaving the strip, or a press, clears it.
        app.handle_mouse(ev(MouseEventKind::Moved, 50, 10));
        assert!(app.strip_hover.is_none() && app.dirty_frame);
        app.handle_mouse(ev(MouseEventKind::Moved, 1, 0));
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 50, 10));
        assert!(app.strip_hover.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
