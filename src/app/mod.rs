//! The application: owns all panels, routes input, applies effects, and runs
//! the event loop.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton};
use futures::StreamExt;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::actions::{Action, AppAction, DebugAction, Dir, EditorAction, EventLogAction, FindAction, ModeAction, RunAction};
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
use crate::ui::focus::{Dock, Focus, PanelId, Side, ToolWindowId};
use crate::ui::layout::{LayoutState, SplitterId, StripDrop, ViewMode};
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

pub use debug::Prepared;
pub use lsp::LspStatus;
pub use save_as::AfterSave;

// Declared after `ctx!` so the submodules can use it.
mod clipboard;
mod compare;
mod completion;
mod config_reload;
mod debug;
mod dialogs;
mod effects;
mod files;
mod find;
mod format;
mod git_ops;
mod hierarchy;
mod history;
mod http;
mod inlay;
mod layouts;
mod lightbulb;
mod lsp;
mod lsp_edit;
mod lsp_nav;
mod lsp_watch;
mod main_menu;
mod markdown;
mod merge;
mod mouse;
mod msg;
mod palette;
mod recovery;
mod run;
mod run_configs;
mod save_as;
mod semantic;
mod settings;
mod signature;
mod snippets;
mod switcher;
mod symbols;
mod tasks;
mod terminal;
mod todo;
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
    /// Logged only: never offered to the status bar.
    pub quiet: bool,
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
    /// A tool strip button pressed with the left button: released on it
    /// without leaving it, it toggles its window; moved, it drops (`StripDrop`).
    /// `x`, `y` are the last pointer cell.
    StripButton {
        tw: ToolWindowId,
        x: u16,
        y: u16,
        moved: bool,
    },
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
    /// Switcher popup (Ctrl+Tab); takes input like the palette, below the
    /// dialogs and menus.
    pub switcher: Option<crate::ui::switcher::Switcher>,
    /// App-wide palette filter seq, see `FilterBus`.
    palette_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Context menu overlay (right click, Shift+F10); takes input like a
    /// dialog, below dialogs.
    pub menu: Option<crate::ui::menu::ContextMenu>,
    /// Main menu bar overlay (F10); takes input like a dialog, below
    /// dialogs and above the context menu.
    pub menu_bar: Option<crate::ui::menu_bar::MenuBar>,
    /// The Settings window, over the menus and under dialogs.
    pub settings: Option<crate::ui::settings::SettingsWindow>,
    pub run_configs: Option<crate::ui::run_configs::RunConfigsWindow>,
    /// Tool strip button under the pointer, with its rect; it shows a
    /// tooltip.
    pub strip_hover: Option<(crate::ui::layout::StripButton, Rect)>,
    /// Toolbar button under the pointer, with its rect; it shows a tooltip.
    pub toolbar_hover: Option<(crate::ui::layout::ToolbarButton, Rect)>,
    pub file_index: FileIndex,
    /// Recent project roots for the palette, cached from the last load.
    recent_projects: Vec<PathBuf>,
    /// Last requested recent projects load or forget, the seq of the latest
    /// forget (replies to older requests are dropped), and the file version
    /// of the list shown.
    recent_projects_requested: u64,
    recent_projects_forgot: u64,
    /// Forgets whose reply has not arrived, by seq; their roots are filtered
    /// out of every reply until then, so a load that read the file before the
    /// forget wrote it cannot show them.
    recent_projects_forgetting: Vec<(u64, PathBuf)>,
    recent_projects_version: u64,
    /// Saved layouts as of the newest reply, file order.
    pub(crate) named_layouts: Vec<crate::layouts::NamedLayout>,
    /// The file version `named_layouts` reflects; older replies are dropped.
    layouts_version: u64,
    /// The layout last applied or saved as new, for Save Changes; not persisted.
    current_layout: Option<String>,
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
    /// The SCIP index of `[index] scip` (`load_scip`).
    pub scip: crate::symbols::scip::ScipState,
    /// When the open Go to Symbol palette last got the index (it is fed at
    /// most every `SYMBOLS_FEED` while batches arrive).
    symbols_fed: Option<Instant>,
    /// Caret target of a file being opened by `open_file_at` or
    /// `open_file_at_utf16`.
    pending_goto: Option<(PathBuf, msg::GotoTarget)>,
    /// An operation stopped on conflicts: the next status listing
    /// conflicted files opens Resolve Conflicts.
    pending_conflicts: bool,
    /// A conflicted path just resolved (GitOp::Resolve): the first status that no longer lists it offers the conflicted files left.
    resolved_conflict: Option<String>,
    /// File from the command line (1-based line and column), opened once
    /// the session is restored so it ends up the active tab.
    pub(crate) pending_open: Option<(PathBuf, usize, usize)>,
    /// Editor sets of the root's other branches, from the session
    /// (`workspace::BranchSet`).
    pub(crate) branch_sets: std::collections::BTreeMap<String, crate::workspace::BranchSet>,
    /// The branch whose editor set is open, as the session recorded it.
    pub(crate) session_branch: Option<String>,
    /// The periodic session save.
    pub(crate) session_save: crate::workspace::SessionSave,
    /// The branch swap whose files are being read.
    pub(crate) branch_swap: crate::workspace::BranchSwap,
    pub git: GitService,
    /// The yman task tracker of the root; loads nothing until `want()`.
    pub yman: crate::yman::YmanService,
    /// Local history's worker (`history/`); `GitService` holds a clone.
    pub history: crate::history::Handle,
    /// A snapshot, or changes of one, to put into a file once its tab opens
    /// (a revert of a file that was not open), as `pending_goto`.
    pub(crate) pending_revert: std::collections::HashMap<PathBuf, history::PendingRevert>,
    pub editor: Editor,
    /// Watches the root for outside changes; `None` when off or failed.
    watcher: Option<crate::files::watch::Watcher>,
    /// Clipboard tools, jetl's copy register and the paste in flight.
    clipboard: clipboard::ClipboardState,
    /// Formatter runs in flight.
    pub(crate) format: format::FormatState,
    /// The running task, the last one and the picker's tasks.
    pub(crate) run: run::RunState,
    pub(crate) debug: debug::DebugState,
    pub run_panel: crate::run::panel::RunPanel,
    /// The Debug window's Console and toolbar.
    pub debug_panel: crate::dap::panel::DebugPanel,
    /// The temp file holding the body of the last HTTP request sent (Rerun reads it again).
    pub(crate) http_body: Option<std::path::PathBuf>,
    /// The Problems window over `lsp.diagnostics`.
    pub problems: crate::lsp::panel::ProblemsPanel,
    /// The Local History window (`history/panel.rs`).
    pub history_panel: crate::history::panel::LocalHistoryPanel,
    /// The Tasks window over `yman` (`yman/panel.rs`).
    pub tasks: crate::yman::panel::TasksPanel,
    /// The TODO window (`files/todo.rs`).
    pub todo: crate::files::todo::TodoPanel,
    /// The call or type hierarchy shown in the Hierarchy window (`app/hierarchy.rs`).
    pub hierarchy: Option<crate::lsp::hierarchy::HierarchyTree>,
    /// The Hierarchy window's view state (`ui/hierarchy.rs`).
    pub hierarchy_panel: crate::ui::hierarchy::HierarchyPanel,
    /// The Terminal window and its shell.
    pub terminal: crate::terminal::tabs::TerminalTabs,
    /// Language servers and the documents open on them.
    pub(crate) lsp: lsp::LspService,
    /// The Quick Documentation popup at the caret (`app/lsp_nav.rs`).
    pub hover: Option<crate::ui::hover::HoverPopup>,
    /// The code completion popup at the caret (`app/completion.rs`).
    pub completion: Option<crate::ui::completion::CompletionPopup>,
    /// Live templates: built-in and user snippets (`config::snippets`).
    pub snippets: crate::config::snippets::SnippetSet,
    /// The Parameter Info popup at the caret (`app/signature.rs`).
    pub signature: Option<crate::ui::signature::SignaturePopup>,
    /// The dirty buffers the panic hook writes (`recovery.rs`), refreshed
    /// by the run loop.
    pub(crate) recorder: crate::recovery::Recorder,
    /// When the next snapshot of unsaved edits is due (`[local_history]
    /// snapshot_unsaved_secs`); `None` when off.
    pub(crate) unsaved_due: Option<Instant>,
    /// When the next hourly prune of the Local History store is due.
    pub(crate) prune_due: Option<Instant>,
    /// Local History diff builds: one at a time (`app/history.rs`).
    pub(crate) local_diff: crate::util::coalesce::Coalesce,
    /// The diff request held back while a build runs; the newest wins.
    pub(crate) local_diff_next: Option<(crate::git::DiffKey, u64)>,
    /// The path, tab `version` and time of the open-tab diff last built:
    /// the tick asks again when the tab moved on.
    pub(crate) local_diff_follow: Option<(PathBuf, u64, Instant)>,
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
        let snippet_inputs = crate::config::snippets::SnippetSet::read(&paths.snippets_dir());
        let (snippets, snippet_errors) = crate::config::snippets::SnippetSet::build(&snippet_inputs);
        let theme_truecolor = config.ui.colors.truecolor(colorterm.as_deref());
        let modal = config.keymap.mode == "vim";
        let mut dispatcher = Dispatcher::new(modal);
        dispatcher.double_tap = Duration::from_millis(config.ui.double_click_ms);
        let mode = if modal { Mode::Normal } else { Mode::Insert };
        let layout = LayoutState::new(config.ui.left_width, config.ui.bottom_height);
        let root_for_panels = root.clone();
        let workspace = Workspace::new(root.clone());
        let mut git = GitService::new(root.clone(), workspace.generation, bus_tx.clone());
        let yman = crate::yman::YmanService::new(root.clone(), workspace.generation, bus_tx.clone());
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
            switcher: None,
            palette_seq: Default::default(),
            menu: None,
            menu_bar: None,
            settings: None,
            run_configs: None,
            strip_hover: None,
            toolbar_hover: None,
            file_index: FileIndex::default(),
            recent_projects: Vec::new(),
            recent_projects_requested: 0,
            recent_projects_forgot: 0,
            recent_projects_forgetting: Vec::new(),
            recent_projects_version: 0,
            named_layouts: Vec::new(),
            layouts_version: 0,
            current_layout: None,
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
            scip: Default::default(),
            symbols_fed: None,
            pending_goto: None,
            pending_conflicts: false,
            resolved_conflict: None,
            pending_open: open,
            branch_sets: Default::default(),
            session_branch: None,
            session_save: Default::default(),
            branch_swap: Default::default(),
            git,
            yman,
            history,
            pending_revert: std::collections::HashMap::new(),
            editor: Editor::new(&config),
            watcher: None,
            clipboard: Default::default(),
            format: Default::default(),
            run: Default::default(),
            debug: Default::default(),
            run_panel: Default::default(),
            debug_panel: Default::default(),
            http_body: None,
            problems: crate::lsp::panel::ProblemsPanel::excluding(paths.scratches_dir()),
            history_panel: Default::default(),
            tasks: Default::default(),
            todo: Default::default(),
            hierarchy: None,
            hierarchy_panel: Default::default(),
            terminal: crate::terminal::tabs::TerminalTabs::new(config.terminal.scrollback),
            lsp: lsp::LspService::with_inlays(config.lsp.inlay_hints),
            hover: None,
            completion: None,
            snippets,
            signature: None,
            recorder,
            unsaved_due,
            prune_due: Some(Instant::now() + crate::history::PRUNE_EVERY),
            local_diff: Default::default(),
            local_diff_next: None,
            local_diff_follow: None,
            recovery_offer: None,
            config_reload: config_reload::ConfigReload::new(snippet_inputs),
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
        for e in keymap_errors.into_iter().chain(theme_errors).chain(snippet_errors.into_iter().map(|(_, e)| e)) {
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
        app.load_scip();
        app.sync_file_icons();
        app
    }

    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        self.toasts.push(level, &text, Instant::now());
        self.record(level, text, false);
    }

    /// Record `text` in the Event Log (and the tracing log) without a toast
    /// or a status bar message.
    pub fn log_only(&mut self, level: Level, text: impl Into<String>) {
        self.record(level, text.into(), true);
    }

    fn record(&mut self, level: Level, text: String, quiet: bool) {
        match level {
            Level::Error => tracing::error!("{text}"),
            Level::Warn => tracing::warn!("{text}"),
            Level::Info => tracing::info!("{text}"),
        }
        let now = Instant::now();
        self.notifications.push(Notification { level, text, at: now, wall: SystemTime::now(), quiet });
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
            self.fill_shortcuts(&mut item.children);
        }
    }

    /// The shortest binding of an action in its usual contexts.
    pub(crate) fn shortcut(&self, action: &Action) -> Option<String> {
        self.keymap.bindings_for(action, action.contexts()).into_iter().next()
    }

    /// The newest non-quiet notification while within its level's TTL; quiet entries
    /// are skipped, not blanking the slot.
    pub fn current_notification(&self) -> Option<&Notification> {
        let n = self.notifications.iter().rev().find(|n| !n.quiet)?;
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

    /// The git operation in progress, from the last status.
    pub fn in_progress(&self) -> Option<crate::git::model::InProgress> {
        self.git_panel.in_progress()
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
            if self.snippet_prefix_here() {
                v.push("editor.snippet_prefix");
            }
            if self.focus.is(PanelId::Editor) && self.editor.merge_active() {
                v.push("editor.merge");
            }
            let suspended = self.debug.suspended();
            let above = self.focus.is(PanelId::Editor) || self.focus.is(PanelId::Debug);
            if suspended && above {
                v.push("debug.suspended");
            }
            v.push(self.focus.current.context());
            if suspended && !above {
                v.push("debug.suspended");
            }
            if self.debug.active() || self.debug.starting() {
                v.push("debug.active");
            }
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
                crate::tui::set_caret_style(self.caret_style());
                self.dirty_frame = false;
            }
        }
        Ok(())
    }

    /// Leaving jetl, after a quit or a run loop error; nothing here draws.
    /// Writes the local history snapshots still queued (the saves of the
    /// quit dialog) and the session, kills formatters, the Run task and the
    /// Terminal shell with what they started, and ends the debug session
    /// and shuts the language servers down, together (two seconds at most for the servers; a debug
    /// adapter that does not answer takes `dap::client::SHUTDOWN_LIMIT`).
    pub(crate) async fn shutdown(&mut self) {
        self.history.flush(Duration::from_secs(3));
        self.save_session();
        self.format.abort_all();
        self.run.kill();
        if let Some(body) = self.http_body.take() {
            let _ = std::fs::remove_file(body);
        }
        self.terminal.reset();
        let debug = self.debug_finish();
        tokio::join!(self.lsp.shutdown_all(), debug);
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
        if let Some((path, base)) = self.editor.want_inline_blame() {
            self.git.load_blame(path, base);
        }
        // The spinners (Run window, toolbar) and the elapsed time of a running task.
        if self.git.is_busy()
            || self.lsp.busy()
            || (self.run.running() && (self.layout.is_visible(ToolWindowId::Run) || !self.layout.rects.toolbar.is_empty()))
        {
            self.dirty_frame = true;
        }
        self.completion_timeout(Instant::now());
        self.completion_docs_tick(Instant::now());
        self.format_timeout(Instant::now());
        self.lightbulb_tick(Instant::now());
        self.inlay_tick(Instant::now());
        self.semantic_tick(Instant::now());
        self.markdown_preview_tick();
        if self.editor.autosave_due(Instant::now()) {
            self.autosave();
        }
        self.session_tick(Instant::now());
        self.unsaved_snapshots(Instant::now());
        self.history_prune_tick(Instant::now());
        self.history_diff_follow(Instant::now());
        self.terminal_tick();
        self.lsp_confirm_orphaned();
        self.debug_sync_breakpoints();
        self.debug_offer_artifacts();
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
                self.yman.refresh();
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
        self.lsp_confirm_orphaned();
    }

    fn handle_paste(&mut self, text: String) {
        self.completion_flush();
        if let Some(d) = &mut self.dialog {
            // Paste into text fields only.
            if let crate::ui::dialog::DialogKind::Input { input, .. } = &mut d.kind {
                input.insert_str(&text.replace(['\n', '\r'], " "));
                d.clear_path_hint();
            } else if let crate::ui::dialog::DialogKind::NewTask(f) = &mut d.kind {
                f.paste(&text);
            } else if let crate::ui::dialog::DialogKind::Rebase(f) = &mut d.kind {
                f.paste(&text);
            } else if let crate::ui::dialog::DialogKind::KeyCapture(f) = &mut d.kind {
                f.paste(&text);
            }
            return;
        }
        if self.settings.is_some() || self.run_configs.is_some() || self.switcher.is_some() {
            return;
        }
        if let Some(p) = &mut self.palette {
            p.paste(&text);
            self.workspace_symbols_sync();
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
        let had_tabs = self.has_user_tabs();
        let undocked =
            self.focus.current.tool_window().filter(|tw| self.layout.view_mode(*tw) == ViewMode::Undocked && self.layout.is_visible(*tw));
        let esc = key.code == KeyCode::Esc && key.kind != KeyEventKind::Release;
        self.handle_key_inner(key);
        // Esc that leaves an Undocked window slides it away; a panel's own Esc
        // (find, filter) that keeps focus does not.
        if esc
            && let Some(tw) = undocked
            && !self.focus.is(tw.panel())
        {
            self.layout.hide(tw);
        }
        self.focus_tree_after_last_close(had_tabs);
        self.lsp_confirm_orphaned();
    }

    /// An overlay is open over the editor: completion, hover, signature
    /// help and snippets stand down while it is. A new overlay over the
    /// editor adds itself here, or in `caret_overlay_open` when it draws
    /// its own caret or hides the editor's.
    pub(crate) fn editor_overlay_open(&self) -> bool {
        self.menu.is_some() || self.menu_bar.is_some() || self.caret_overlay_open()
    }

    /// The overlays that take the terminal caret from the editor. A menu
    /// or the menu bar does not: the editor still places its caret under it.
    fn caret_overlay_open(&self) -> bool {
        self.palette.is_some() || self.switcher.is_some() || self.dialog.is_some() || self.settings.is_some() || self.run_configs.is_some()
    }

    /// The caret shape the terminal should show after the last draw: a
    /// steady block in overwrite mode while the editor caret is the one
    /// drawn (editor focused, no overlay that takes the caret, find bar not
    /// focused, caret on screen), else the user's own shape. An open menu or
    /// menu bar keeps the block: the editor caret stays under it. Reads the
    /// caret cell of the last draw, so call it after `term.draw`.
    pub(crate) fn caret_style(&self) -> SetCursorStyle {
        let shown = self.focus.is(PanelId::Editor)
            && !self.caret_overlay_open()
            && !self.editor.find_focused()
            && self.editor.caret_cell().is_some();
        if self.editor.overwrite() && shown { SetCursorStyle::SteadyBlock } else { SetCursorStyle::DefaultUserShape }
    }

    /// An armed key-capture field has focus: the paste key is a key to record.
    fn dialog_capturing(&self) -> bool {
        matches!(&self.dialog, Some(d) if matches!(&d.kind, crate::ui::dialog::DialogKind::KeyCapture(f) if f.capturing()))
    }

    fn handle_key_inner(&mut self, key: KeyEvent) {
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
            // Dialogs take every key, except that the paste key pastes; an
            // armed key-capture field records it instead.
            if self.is_paste_key(key) && !self.dialog_capturing() {
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
        if self.settings.is_some() {
            if key.kind != crossterm::event::KeyEventKind::Release && !matches!(key.code, KeyCode::Modifier(_)) {
                self.settings_key(key);
            }
            return;
        }
        if self.run_configs.is_some() {
            if key.kind != crossterm::event::KeyEventKind::Release && !matches!(key.code, KeyCode::Modifier(_)) {
                self.run_configs_key(key);
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
        // Release events and bare modifiers reach it: Ctrl released opens.
        if self.switcher.is_some() {
            self.switcher_key(key);
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
        // Parameter Info takes Esc; other keys go on.
        if self.signature_key(key) {
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
        // And for the Tasks window's filter.
        if self.focus.is(PanelId::Tasks) && key.kind != crossterm::event::KeyEventKind::Release && self.tasks.handle_key(key, &self.yman) {
            self.dispatcher.reset();
            return;
        }
        self.snippet_check();
        self.snippet_parse();
        let contexts = self.contexts();
        let r = self.dispatcher.dispatch(key, &contexts, Some(self.mode), &self.keymap);
        match r {
            Dispatch::Action(Action::Editor(a @ (EditorAction::Switcher | EditorAction::SwitcherBack))) => {
                self.open_switcher(a == EditorAction::SwitcherBack, key.modifiers.contains(KeyModifiers::CONTROL))
            }
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
            Action::Editor(EditorAction::ExpandSnippet) => {
                drop(cx);
                self.expand_snippet();
                return;
            }
            Action::Editor(a @ (EditorAction::Switcher | EditorAction::SwitcherBack)) => {
                let back = *a == EditorAction::SwitcherBack;
                drop(cx);
                self.open_switcher(back, false);
                return;
            }
            Action::Editor(EditorAction::SurroundWith) => {
                drop(cx);
                self.surround_with();
                return;
            }
            Action::Editor(EditorAction::CompareWithClipboard) => {
                drop(cx);
                self.compare_with_clipboard();
                return;
            }
            Action::Editor(EditorAction::CompareWithFile) => {
                drop(cx);
                self.compare_with_file_pick();
                return;
            }
            Action::Editor(EditorAction::MarkdownPreview) => {
                drop(cx);
                self.markdown_preview();
                return;
            }
            // Group commands take the keyboard to the editor, as in JetBrains.
            Action::Editor(
                a @ (EditorAction::SplitRight
                | EditorAction::SplitDown
                | EditorAction::NextGroup
                | EditorAction::PrevGroup
                | EditorAction::FocusGroupLeft
                | EditorAction::FocusGroupRight
                | EditorAction::FocusGroupUp
                | EditorAction::FocusGroupDown
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
            Action::Run(RunAction::RunSelected) => {
                drop(cx);
                self.run_selected();
                return;
            }
            Action::Run(RunAction::ConfigMenu { x, y }) => {
                drop(cx);
                self.run_config_menu(*x, *y);
                return;
            }
            Action::Run(RunAction::Select(name)) => {
                drop(cx);
                self.select_run_config(name.clone());
                return;
            }
            Action::Run(RunAction::EditConfigs) => {
                drop(cx);
                self.open_run_configs();
                return;
            }
            Action::Run(RunAction::Rerun) => {
                drop(cx);
                self.rerun();
                return;
            }
            Action::Run(RunAction::RerunFailed) => {
                drop(cx);
                self.rerun_failed_run();
                return;
            }
            Action::Run(RunAction::TestAtCaret) => {
                drop(cx);
                self.run_test_at_caret();
                return;
            }
            Action::Run(RunAction::HttpRequestAtCaret) => {
                drop(cx);
                self.http_request_at_caret();
                return;
            }
            Action::Run(RunAction::BuildScipIndex) => {
                drop(cx);
                self.build_scip_index();
                return;
            }
            Action::Run(RunAction::Stop) => {
                drop(cx);
                self.stop_run();
                return;
            }
            Action::Run(a) => self.run_panel.handle_action(a, &mut cx),
            Action::Debug(
                a @ (DebugAction::Up
                | DebugAction::Down
                | DebugAction::PageUp
                | DebugAction::PageDown
                | DebugAction::Home
                | DebugAction::End
                | DebugAction::Open),
            ) => self.debug_panel.handle_action(a, &mut cx),
            Action::Debug(a) => {
                drop(cx);
                self.perform_debug(a.clone());
                return;
            }
            Action::Problems(a) => self.problems.handle_action(a, &self.lsp.diagnostics, self.editor.active_path(), &mut cx),
            Action::History(a) => self.history_panel.handle_action(a, &mut cx),
            Action::Tasks(a) => {
                drop(cx);
                self.perform_tasks(a.clone());
                return;
            }
            Action::Todo(a) => self.todo.handle_action(a, &mut cx),
            Action::Hierarchy(a) => {
                drop(cx);
                self.perform_hierarchy(a.clone());
                return;
            }
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
                    crate::actions::LspAction::GotoTypeDefinition => {
                        self.lsp_goto(crate::lsp::caps::TYPE_DEFINITION, crate::lsp::PendingTag::TypeDefinition)
                    }
                    crate::actions::LspAction::GotoImplementation => {
                        self.lsp_goto(crate::lsp::caps::IMPLEMENTATION, crate::lsp::PendingTag::Implementation)
                    }
                    crate::actions::LspAction::GotoDeclaration => {
                        self.lsp_goto(crate::lsp::caps::DECLARATION, crate::lsp::PendingTag::Declaration)
                    }
                    crate::actions::LspAction::FindUsages => self.lsp_find_usages(),
                    crate::actions::LspAction::QuickDoc => self.lsp_quick_doc(),
                    crate::actions::LspAction::QuickDefinition => self.lsp_quick_definition(),
                    crate::actions::LspAction::CallHierarchy => self.hierarchy_start(true),
                    crate::actions::LspAction::TypeHierarchy => self.hierarchy_start(false),
                    crate::actions::LspAction::ParameterInfo => self.lsp_parameter_info(),
                    crate::actions::LspAction::Complete => self.lsp_complete(),
                    crate::actions::LspAction::Rename => self.lsp_rename(),
                    crate::actions::LspAction::ContextActions => self.lsp_code_actions(lsp_edit::ActionsFor::Context),
                    crate::actions::LspAction::FileStructure => self.lsp_file_structure(),
                    crate::actions::LspAction::RefactorThis => self.lsp_code_actions(lsp_edit::ActionsFor::Refactor),
                    crate::actions::LspAction::OptimizeImports => self.lsp_code_actions(lsp_edit::ActionsFor::OrganizeImports),
                    crate::actions::LspAction::Refactoring(r) => self.lsp_code_actions(lsp_edit::ActionsFor::Kind(*r)),
                    crate::actions::LspAction::ToggleInlayHints => self.toggle_inlay_hints(),
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
            self.signature_after_edit(typed);
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
                if self.layout.shown().is_empty() {
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
            AppAction::NextToolWindowTab => self.cycle_tool_window_tab(true),
            AppAction::PrevToolWindowTab => self.cycle_tool_window_tab(false),
            AppAction::MaximizeToolWindow => {
                if let Some(tw) = self.focus.current.tool_window() {
                    self.layout.toggle_maximize(tw);
                }
            }
            AppAction::LastToolWindow => self.last_tool_window(),
            AppAction::MoveToolWindow { tw, dock } => {
                if let Some(tw) = tw.or(self.focus.current.tool_window()) {
                    self.move_tool_window(tw, dock);
                }
            }
            AppAction::MoveToolWindowButton { tw, up } => self.layout.move_button(tw, up),
            AppAction::RestoreDefaultLayout => self.restore_default_layout(),
            AppAction::SwitchLayout => self.switch_layout(),
            AppAction::SaveLayoutAsNew => self.save_layout_as_new(),
            AppAction::SaveLayoutChanges => self.save_layout_changes(),
            AppAction::RenameLayout => self.rename_layout(),
            AppAction::DeleteLayout => self.delete_layout(),
            AppAction::ToggleZen => self.toggle_zen(),
            AppAction::ToggleToolbar => self.toggle_toolbar(),
            AppAction::TogglePinned { tw } => {
                if let Some(tw) = tw.or(self.focus.current.tool_window()) {
                    let next = if self.layout.view_mode(tw) == ViewMode::Pinned { ViewMode::Unpinned } else { ViewMode::Pinned };
                    self.layout.set_view_mode(tw, next);
                }
            }
            AppAction::SetViewMode { tw, mode } => self.set_tool_window_mode(tw, mode),
            AppAction::ToggleUndocked { tw } => {
                if let Some(tw) = tw.or(self.focus.current.tool_window()) {
                    let next = if self.layout.view_mode(tw) == ViewMode::Undocked { ViewMode::Pinned } else { ViewMode::Undocked };
                    self.set_tool_window_mode(tw, next);
                }
            }
            AppAction::Escape => {
                if let Some(tw) = self.focus.current.tool_window()
                    && self.layout.view_mode(tw) != ViewMode::Pinned
                {
                    self.layout.hide(tw);
                }
                self.focus.set(PanelId::Editor);
            }
            AppAction::OpenPalette(kind) => self.open_palette(kind),
            AppAction::RecentLocations => self.show_recent_locations(),
            AppAction::DebugPanic => {
                if cfg!(debug_assertions) {
                    panic!("debug panic requested via keymap");
                }
            }
            AppAction::Refresh => self.apply_effects(vec![Effect::GitRefresh]),
            AppAction::MainMenu => self.toggle_main_menu(),
            AppAction::About => self.notify(Level::Info, format!("jetl {}", env!("CARGO_PKG_VERSION"))),
            AppAction::OpenSettings => self.open_settings(),
            AppAction::Settings => self.open_settings_window(),
            AppAction::KeymapReference => self.keymap_reference(),
            AppAction::NewScratch => self.new_scratch(),
            AppAction::LocalHistory => self.local_history(),
            AppAction::PutLabel => self.put_label(),
            AppAction::NewTerminalTab => self.new_terminal_tab(),
            AppAction::CloseTerminalTab => self.close_terminal_tab(),
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

    /// Move `tw` to `dock`; focus leaves a window the move hid.
    pub(crate) fn move_tool_window(&mut self, tw: ToolWindowId, dock: Dock) {
        self.layout.move_to_dock(tw, dock);
        if let Some(cur) = self.focus.current.tool_window()
            && !self.layout.is_visible(cur)
        {
            self.focus.set(PanelId::Editor);
        }
    }

    /// Set a tool window's view mode; focus the editor when the focused
    /// window was hidden by it (undocking another window hides the shown one).
    pub(crate) fn set_tool_window_mode(&mut self, tw: ToolWindowId, mode: ViewMode) {
        self.layout.set_view_mode(tw, mode);
        if let Some(cur) = self.focus.current.tool_window()
            && !self.layout.is_visible(cur)
        {
            self.focus.set(PanelId::Editor);
        }
    }

    /// Restore Default Layout; focus the editor when the focused window was
    /// hidden by it.
    pub(crate) fn restore_default_layout(&mut self) {
        self.layout.restore_default();
        if let Some(cur) = self.focus.current.tool_window()
            && !self.layout.is_visible(cur)
        {
            self.focus.set(PanelId::Editor);
        }
    }

    /// A strip button dropped on `drop`: reorder, and move the window to the
    /// drop's dock.
    pub(crate) fn drop_strip_button(&mut self, tw: ToolWindowId, drop: StripDrop) {
        if let Some(dock) = self.layout.drop_button(tw, drop) {
            self.move_tool_window(tw, dock);
        }
    }

    /// Right-click on a tool strip button: the docks `tw` can move to and its
    /// button and view mode rows.
    pub(crate) fn tool_window_side_menu(&mut self, tw: ToolWindowId, x: u16, y: u16) {
        use crate::ui::menu::{ContextMenu, MenuItem};
        let now = self.layout.dock_of(tw);
        let moves: Vec<MenuItem> = Dock::ALL
            .into_iter()
            .map(|dock| MenuItem::new(dock.title(), Action::App(AppAction::MoveToolWindow { tw: Some(tw), dock })).enabled(now != dock))
            .collect();
        let modes: Vec<MenuItem> =
            [("Dock Pinned", ViewMode::Pinned), ("Dock Unpinned", ViewMode::Unpinned), ("Undock", ViewMode::Undocked)]
                .into_iter()
                .map(|(label, mode)| {
                    MenuItem::new(label, Action::App(AppAction::SetViewMode { tw, mode })).enabled(self.layout.view_mode(tw) != mode)
                })
                .collect();
        let button = |label: &str, up: bool| {
            MenuItem::new(label, Action::App(AppAction::MoveToolWindowButton { tw, up })).enabled(self.layout.can_move_button(tw, up))
        };
        let items = vec![
            MenuItem::submenu("Move to", moves),
            MenuItem::separator(),
            button("Move Button Up", true),
            button("Move Button Down", false),
            MenuItem::separator(),
            MenuItem::new("Restore Default Layout", Action::App(AppAction::RestoreDefaultLayout)),
            MenuItem::separator(),
            MenuItem::submenu("View Mode", modes),
        ];
        self.apply_effects(vec![Effect::ContextMenu(ContextMenu::new(items, x, y))]);
    }

    /// A right press on the toolbar row: its context menu.
    pub(crate) fn toolbar_menu(&mut self, x: u16, y: u16) {
        use crate::ui::menu::{ContextMenu, MenuItem};
        let items = vec![MenuItem::new("Hide Toolbar", Action::App(AppAction::ToggleToolbar))];
        self.apply_effects(vec![Effect::ContextMenu(ContextMenu::new(items, x, y))]);
    }

    /// A press on a strip's More button: a menu of that strip's windows
    /// without a button, in strip order; a row toggles its window.
    pub(crate) fn strip_more_menu(&mut self, right: bool, x: u16, y: u16) {
        use crate::ui::menu::{ContextMenu, MenuItem};
        let items: Vec<MenuItem> = self
            .layout
            .strip_overflow(right)
            .into_iter()
            .map(|tw| MenuItem::new(tw.title(), Action::App(AppAction::ToggleToolWindow(tw))))
            .collect();
        if !items.is_empty() {
            self.apply_effects(vec![Effect::ContextMenu(ContextMenu::new(items, x, y))]);
        }
    }

    /// F12, JetBrains-style: focus the tool window that last had focus,
    /// showing it first; before any had focus, a visible one or Project.
    fn last_tool_window(&mut self) {
        let tw = self.focus.last_tool.or_else(|| self.layout.shown().first().copied()).unwrap_or(ToolWindowId::Project);
        if !self.layout.is_visible(tw) {
            self.toggle_tool_window(tw);
        } else {
            self.focus.set(tw.panel());
        }
    }

    /// Flip `[ui] toolbar` in `config.toml`; the reload applies it at once
    /// and an open Settings window follows (yman 354).
    fn toggle_toolbar(&mut self) {
        use crate::config::edit::{Edit, Value};
        let on = !self.config.ui.toolbar;
        if let Err(e) = self.write_settings(&[("ui.toolbar", Edit::Set(Value::Bool(on)))]) {
            self.notify(Level::Error, e);
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

    /// Alt+Right / Alt+Left in a tool window with inner tabs (Git, Run, Terminal): the next or
    /// previous tab, wrapping. The focused panel decides; one without tabs
    /// (and the editor) ignores it.
    fn cycle_tool_window_tab(&mut self, forward: bool) {
        match self.focus.current {
            PanelId::Git => {
                let tab = self.git_panel.tab.step(forward);
                self.set_git_tab(tab);
            }
            // Two views: either direction flips them; no-op without tests.
            PanelId::Run => self.perform(Action::Run(RunAction::ToggleView)),
            PanelId::Terminal => self.terminal.step(forward),
            _ => {}
        }
    }

    /// Ctrl+Shift+arrows, JetBrains-style: move the free edge of the focused
    /// tool window (width for a left or right window, height for a bottom one). Keys
    /// along the other axis do nothing. With the editor focused the arrow's
    /// axis picks the window, and the editor keeps Ctrl+Shift+Left/Right for
    /// word selection.
    fn resize_tool_window(&mut self, dir: Dir) {
        const STEP_W: i16 = 2;
        const STEP_H: i16 = 1;
        if self.layout.maximized_window().is_some() {
            return;
        }
        let tw = match self.focus.current.tool_window() {
            Some(tw) => Some(tw).filter(|tw| self.layout.is_visible(*tw)),
            None => match dir {
                Dir::Up | Dir::Down => self.layout.bottom.or(self.layout.bottom2),
                Dir::Left | Dir::Right => self.layout.left.or(self.layout.left2).or(self.layout.right).or(self.layout.right2),
            },
        };
        let Some(tw) = tw else { return };
        let (id, delta) = match (self.layout.side_of(tw), dir) {
            (Side::Left, Dir::Right) => (SplitterId::Left, STEP_W),
            (Side::Left, Dir::Left) => (SplitterId::Left, -STEP_W),
            (Side::Bottom, Dir::Up) => (SplitterId::Bottom, STEP_H),
            (Side::Bottom, Dir::Down) => (SplitterId::Bottom, -STEP_H),
            (Side::Right, Dir::Left) => (SplitterId::Right, STEP_W),
            (Side::Right, Dir::Right) => (SplitterId::Right, -STEP_W),
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

    /// Tabs the user can close, outside the conflict resolver's own panes.
    pub(super) fn has_user_tabs(&self) -> bool {
        !self.editor.merge_active() && !self.editor.buffers.is_empty()
    }

    /// An input that closed the last tab while the editor had focus hands
    /// focus to the project tree. `had_tabs` is `has_user_tabs()` before the
    /// input; the resolver's close is not a last tab (it was false then).
    pub(super) fn focus_tree_after_last_close(&mut self, had_tabs: bool) {
        if had_tabs && self.editor.buffers.is_empty() && !self.editor.merge_active() && self.focus.is(PanelId::Editor) {
            self.layout.show(ToolWindowId::Project);
            self.focus.set(PanelId::Project);
        }
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
        // No file icons unless a test sets them: tree and tab text stays as before.
        config.ui.file_icons = crate::config::FileIcons::None;
        // No toolbar unless a test sets it: rects and snapshots stay as before (yman 350).
        config.ui.toolbar = false;
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
    async fn closing_the_last_tab_focuses_the_project_tree() {
        let (mut app, dir) = test_app("last-tab-focus");
        for n in ["a.txt", "b.txt"] {
            let path = app.workspace.root.join(n);
            app.editor.open_text(path, "x\n", false, &app.theme);
        }
        app.focus.set(PanelId::Editor);
        app.layout.hide(ToolWindowId::Project);
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        app.handle_key(ctrl_w);
        assert_eq!(app.editor.buffers.len(), 1);
        assert!(app.focus.is(PanelId::Editor));
        assert!(!app.layout.is_visible(ToolWindowId::Project));
        app.handle_key(ctrl_w);
        assert!(app.editor.buffers.is_empty());
        assert!(app.focus.is(PanelId::Project));
        assert!(app.layout.is_visible(ToolWindowId::Project));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn clicking_the_close_buttons_of_the_last_tabs_focuses_the_project_tree() {
        let (mut app, dir) = test_app("last-tab-click");
        for n in ["a.txt", "b.txt"] {
            let path = app.workspace.root.join(n);
            app.editor.open_text(path, "x\n", false, &app.theme);
        }
        app.focus.set(PanelId::Editor);
        app.layout.hide(ToolWindowId::Project);
        for left in [1, 0] {
            assert_eq!(app.editor.buffers.len(), left + 1);
            draw_frame(&mut app);
            let r = app.editor.focused_group().tab_hits.close.iter().find(|(i, _)| *i == left).expect("close button drawn").1;
            app.handle_mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: r.x,
                row: r.y,
                modifiers: KeyModifiers::NONE,
            });
            assert_eq!(app.editor.buffers.len(), left);
        }
        assert!(app.focus.is(PanelId::Project));
        assert!(app.layout.is_visible(ToolWindowId::Project));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dont_save_on_the_last_dirty_tab_focuses_the_project_tree() {
        let (mut app, dir) = test_app("last-tab-dirty");
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path, "x\n", false, &app.theme);
        app.focus.set(PanelId::Editor);
        app.layout.hide(ToolWindowId::Project);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(matches!(app.dialog.as_ref().map(|d| &d.id), Some(DialogId::CloseTabDirty(_))));
        assert!(app.focus.is(PanelId::Editor));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.editor.buffers.is_empty());
        assert!(app.focus.is(PanelId::Project));
        assert!(app.layout.is_visible(ToolWindowId::Project));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn focusing_the_empty_editor_does_not_move_focus() {
        let (mut app, dir) = test_app("empty-editor-focus");
        app.focus.set(PanelId::Editor);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn caret_is_a_block_only_in_overwrite_at_the_editor_caret() {
        use crate::actions::PaletteKind;
        use crossterm::cursor::SetCursorStyle::{DefaultUserShape, SteadyBlock};
        let (mut app, dir) = test_app("caret-style");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let toggle = |app: &mut App| app.perform(Action::Editor(EditorAction::ToggleOverwrite));
        macro_rules! shape {
            () => {{
                term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
                app.caret_style()
            }};
        }
        toggle(&mut app);
        assert_eq!(shape!(), DefaultUserShape, "no caret cell");
        toggle(&mut app);
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path, "abc\n", false, &app.theme);
        assert_eq!(shape!(), DefaultUserShape, "overwrite off");
        toggle(&mut app);
        assert_eq!(shape!(), SteadyBlock);
        app.focus.set(PanelId::Project);
        assert_eq!(shape!(), DefaultUserShape, "editor not focused");
        app.focus.set(PanelId::Editor);
        assert_eq!(shape!(), SteadyBlock);
        app.perform(Action::Editor(EditorAction::Find));
        assert!(app.editor.find_focused());
        assert_eq!(shape!(), DefaultUserShape, "find bar focused");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.editor.find_focused());
        assert_eq!(shape!(), SteadyBlock);
        app.perform(Action::App(AppAction::OpenPalette(PaletteKind::Actions)));
        assert!(app.palette.is_some());
        assert_eq!(shape!(), DefaultUserShape, "palette open");
        app.palette = None;
        app.menu = Some(crate::ui::menu::ContextMenu::new(Vec::new(), 0, 0));
        assert_eq!(shape!(), SteadyBlock, "context menu open");
        app.perform(Action::App(AppAction::OpenPalette(PaletteKind::Actions)));
        app.menu = Some(crate::ui::menu::ContextMenu::new(Vec::new(), 0, 0));
        assert!(app.palette.is_some() && app.menu.is_some());
        assert_eq!(shape!(), DefaultUserShape, "palette under a menu");
        app.palette = None;
        app.menu = None;
        app.toggle_main_menu();
        assert!(app.menu_bar.is_some());
        assert_eq!(shape!(), SteadyBlock, "menu bar open");
        app.menu_bar = None;
        toggle(&mut app);
        assert_eq!(shape!(), DefaultUserShape, "overwrite off again");
        let _ = std::fs::remove_dir_all(&dir);
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
    async fn quiet_entry_keeps_earlier_status_message() {
        let (mut app, dir) = test_app("quiet-keeps-status");
        app.notify(Level::Error, "boom");
        app.log_only(Level::Info, "Saved a.txt");
        assert_eq!(app.current_notification().map(|n| n.text.as_str()), Some("boom"));
        // Expire every non-quiet entry (the test app also holds a startup warning).
        let old = Instant::now().checked_sub(Duration::from_secs(16)).expect("monotonic clock past 16 s");
        for n in app.notifications.iter_mut().filter(|n| !n.quiet) {
            n.at = old;
        }
        assert!(app.current_notification().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn save_logs_without_a_toast() {
        let (mut app, dir) = test_app("save-log");
        let _path = load(&mut app, "a.txt", "x");
        app.perform(Action::Editor(EditorAction::InsertText("y".into())));
        app.perform(Action::Editor(EditorAction::Save));
        assert!(!app.editor.has_dirty());
        assert!(app.notifications.last().is_some_and(|n| n.text == "Saved a.txt"));
        // The quiet save is never offered: the status slot still holds the test app's startup
        // warning (`keyboard_enhanced` is false without `tui::init`).
        let current = app.current_notification().expect("the startup warning is current");
        assert!(current.level == Level::Warn && current.text.starts_with("Terminal lacks the kitty keyboard protocol"), "{}", current.text);
        assert!(!app.toasts.items().iter().any(|t| t.text.starts_with("Saved")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn focus_lost_autosaves_by_default() {
        let (mut app, dir) = test_app("focus-autosave");
        assert_eq!(app.config.editor.autosave, crate::config::Autosave::Focus);
        let path = load(&mut app, "a.txt", "x");
        app.perform(Action::Editor(EditorAction::InsertText("y".into())));
        let before = app.notifications.len();
        app.handle_term_event(crossterm::event::Event::FocusLost);
        assert!(!app.editor.has_dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "yx\n");
        assert_eq!(app.notifications.len(), before);
        let _ = std::fs::remove_dir_all(dir);
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
        app.handle_msg(AppMsg::RecentProjects { seq: 1, version: 1, roots: vec![root.clone()], error: None });
        assert!(app.palette.is_none());

        app.pick_project_at_startup();
        app.handle_msg(AppMsg::RecentProjects { seq: 2, version: 2, roots: vec![root.clone(), other.clone()], error: None });
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::RecentProjects));
        // A late reply to an older request does not replace the list.
        app.handle_msg(AppMsg::RecentProjects { seq: 1, version: 1, roots: vec![root.clone()], error: None });
        let res = app.palette.as_mut().unwrap().handle_action(&PaletteAction::Accept).unwrap();
        assert_eq!(res, PaletteResult::Accept(Accept::SwitchRoot(other.clone())));
        app.finish_palette(res);
        assert_eq!(app.workspace.root, other);
        // Offered once only.
        app.handle_msg(AppMsg::RecentProjects { seq: 3, version: 3, roots: vec![other.clone(), root.clone()], error: None });
        assert!(app.palette.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_load_that_read_before_a_forget_does_not_bring_the_root_back() {
        let (mut app, dir) = test_app("recent-forget-race");
        let root = app.workspace.root.clone();
        let (a, b) = (dir.join("a"), dir.join("b"));
        app.recent_projects = vec![root.clone(), a.clone(), b.clone()];
        // As if forget seq 1, then load seq 2 were asked.
        app.recent_projects_requested = 2;
        app.recent_projects_forgot = 1;
        app.recent_projects_forgetting = vec![(1, b.clone())];
        let reply = |seq, version, roots: &[&PathBuf]| AppMsg::RecentProjects {
            seq,
            version,
            roots: roots.iter().map(|r| (*r).clone()).collect(),
            error: None,
        };
        // The load read the file first, the forget wrote after it.
        app.handle_msg(reply(2, 10, &[&root, &a, &b]));
        assert_eq!(app.recent_projects, [root.clone(), a.clone()]);
        app.handle_msg(reply(1, 11, &[&root, &a]));
        assert!(app.recent_projects_forgetting.is_empty());
        assert_eq!(app.recent_projects, [root.clone(), a.clone()]);
        // Older than the forget: the barrier. Older file state: the version.
        app.handle_msg(reply(0, 12, &[&root]));
        app.handle_msg(reply(3, 9, &[&root, &a, &b]));
        assert_eq!(app.recent_projects, [root, a]);
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
        let changed = || Changes { paths: vec![a.clone(), b.clone()], ..Default::default() };
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
        app.handle_msg(AppMsg::FsChanged { generation: g, changes: Changes { paths: vec![a], ..Default::default() } });
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
        let search = crate::files::find::Search {
            root: app.workspace.root.clone(),
            show_hidden: false,
            show_ignored: false,
            query: app.find.search_query().unwrap(),
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
        // Alt+Delete excludes the selected result (`let foo`) from Replace All.
        app.focus.set(PanelId::Find);
        assert_eq!(app.find.list.selected, 3);
        app.handle_key(KeyEvent::new(KeyCode::Delete, crossterm::event::KeyModifiers::ALT));
        assert_eq!((app.find.included_count(), app.find.excluded_count()), (3, 1));
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
        assert_eq!(std::fs::read_to_string(&b_path).unwrap(), "let foo = 1;\nbar(bar);\n");
        assert!(app.notifications.iter().any(|n| n.text == "Replaced 3 matches in 2 files"), "{:?}", app.notifications);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(all(feature = "structural", feature = "lang-rust"))]
    #[tokio::test]
    async fn structural_find_matches_code_and_refuses_text_options() {
        let (mut app, dir) = test_app("find-structural");
        let root = app.workspace.root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn f() {\n    let a = p(i).unwrap();\n    a.unwrap_or(0);\n}\n").unwrap();
        std::fs::write(root.join("notes.txt"), "x.unwrap()\n").unwrap();
        let none = crossterm::event::KeyModifiers::NONE;
        app.perform(Action::Find(FindAction::InFiles));
        app.perform(Action::Find(FindAction::ToggleStructural));
        assert!(app.find.structural);
        for c in "$A.unwrap()".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), none));
        }
        run_find(&mut app);
        assert_eq!(app.find.hit_count(), 1);
        assert_eq!(app.find.hit_files(), [root.join("src/lib.rs")]);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), crossterm::event::KeyModifiers::ALT));
        assert!(!app.find.regex);
        let toast = |app: &App, text: &str| app.notifications.iter().any(|n| n.level == Level::Info && n.text == text);
        assert!(toast(&app, "Case, whole word and regex do not apply to a structural search"), "{:?}", app.notifications);
        app.perform(Action::Find(FindAction::ReplaceAll));
        assert!(toast(&app, "Structural search cannot replace; Alt+T turns it off"));
        assert!(app.dialog.is_none());
        let refusals =
            |app: &App| app.notifications.iter().filter(|n| n.text == "Structural search cannot replace; Alt+T turns it off").count();
        let before = refusals(&app);
        app.perform(Action::Find(FindAction::ToggleReplace));
        assert!(!app.find.show_replace && app.find.structural);
        assert_eq!(refusals(&app), before + 1, "Alt+R refuses with a toast: {:?}", app.notifications);
        app.perform(Action::Find(FindAction::ReplaceInFiles));
        assert!(!app.find.structural && app.find.show_replace);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), crossterm::event::KeyModifiers::ALT));
        assert!(app.find.structural, "Alt+T is bound in the Find context");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn replace_in_files_skips_a_file_changed_since_the_search() {
        let (mut app, dir) = test_app("replace-stale");
        let root = app.workspace.root.clone();
        let (p1, p2, p3) = (root.join("one.txt"), root.join("two.txt"), root.join("three.txt"));
        std::fs::write(&p1, "foo one\n").unwrap();
        std::fs::write(&p2, "foo two\n").unwrap();
        let open = load(&mut app, "three.txt", "foo three\n");
        assert_eq!(open, p3);
        std::fs::write(&p3, "foo three\n").unwrap();
        app.perform(Action::Find(FindAction::ReplaceInFiles));
        for c in "foo".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), crossterm::event::KeyModifiers::NONE));
        }
        run_find(&mut app);
        assert_eq!(app.find.hit_count(), 3);
        // Two files change after the search: one on disk, one open buffer.
        std::fs::write(&p2, "new line\nfoo two\n").unwrap();
        let i = app.editor.index_of(&p3).unwrap();
        app.editor.buffers[i].apply(
            vec![crate::editor::undo::Change::insert(0, "xx\n")],
            crate::editor::selection::Selection::point(0),
            crate::editor::undo::TxKind::Other,
        );
        app.find.replace.set_text("bar");
        app.find.show_replace = true;
        app.finish_dialog(DialogId::ReplaceInFiles, DialogResult::Button(0));
        handle_next(&mut app, |m| matches!(m, AppMsg::ReplaceDone { .. })).await;
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "bar one\n");
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "new line\nfoo two\n", "untouched");
        assert_eq!(app.editor.buffers[i].text(), "xx\nfoo three\n", "untouched");
        for p in [&p2, &p3] {
            let want = format!("Replace in files: {}: changed since the search; search again", p.display());
            assert!(app.notifications.iter().any(|n| n.text == want), "{want}: {:?}", app.notifications);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wait for the first bus message `want` accepts and handle it; other
    /// messages are dropped.
    pub(crate) async fn handle_next(app: &mut App, want: impl Fn(&AppMsg) -> bool) {
        let msg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match app.bus_rx.recv().await {
                    Some(m) if want(&m) => break m,
                    // Local History diff builds run one at a time: an answer
                    // skipped here would hold the next one back for good.
                    Some(m @ AppMsg::LocalHistory(crate::history::HistoryMsg::Diff { .. })) => app.handle_msg(m),
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
            files: vec![(
                "a.rs".into(),
                Some(FileSymbols { mtime: None, size: 0, symbols: vec![symbol(name, 0)], refs: Vec::new(), imports: Vec::new() }),
            )],
            done: true,
            changed: true,
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
        app.handle_msg(AppMsg::FsChanged { generation: g, changes: Changes { paths: vec![path.clone()], ..Default::default() } });
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
        crate::git::repo::tests::isolate(root);
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

    fn open_names(app: &App) -> Vec<String> {
        let mut names = app.editor.tab_names();
        names.sort();
        names
    }

    fn set_names(app: &App, branch: &str) -> Vec<String> {
        app.branch_sets[branch].files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    fn branch_set_loaded(m: &AppMsg) -> bool {
        matches!(m, AppMsg::BranchSetLoaded { .. })
    }

    #[tokio::test]
    async fn branch_switch_swaps_the_editor_set() {
        use crate::git::repo::tests::git_cli;
        let (mut app, dir) = test_app("branch-swap");
        init_repo(&app, &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        let root = app.workspace.root.clone();
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        let generation = app.workspace.generation;
        app.session_loaded(generation, crate::workspace::Session { root: root.clone(), ..Default::default() }, vec![]);
        load(&mut app, "a.txt", "a\n");
        load(&mut app, "b.txt", "b\n");
        feed_status(&mut app, 1);
        assert_eq!(app.session_branch.as_deref(), Some(main.as_str()));
        assert_eq!(open_names(&app), ["a.txt", "b.txt"]);

        // A branch never used keeps the set, which becomes main's.
        git_cli(&root, &["switch", "-qc", "dev"], true);
        feed_status(&mut app, 2);
        assert_eq!(open_names(&app), ["a.txt", "b.txt"]);
        assert_eq!(app.session_branch.as_deref(), Some("dev"));
        assert_eq!(set_names(&app, &main), ["a.txt", "b.txt"]);

        // On dev: close b, open c and edit it.
        let b = app.editor.index_of(&root.join("b.txt")).unwrap();
        app.editor.force_close(b);
        load(&mut app, "c.txt", "c\n");
        type_text(&mut app, "edit");

        git_cli(&root, &["switch", "-q", &main], true);
        feed_status(&mut app, 3);
        assert_eq!(open_names(&app), ["a.txt", "c.txt"], "the dirty tab stays, b waits for its read");
        handle_next(&mut app, branch_set_loaded).await;
        assert_eq!(open_names(&app), ["a.txt", "b.txt", "c.txt"]);
        let c = app.editor.index_of(&root.join("c.txt")).unwrap();
        assert!(app.editor.buffers[c].dirty());
        assert_eq!(set_names(&app, "dev"), ["a.txt", "c.txt"]);
        assert!(!app.branch_sets.contains_key(&main));
        let session = app.session();
        assert_eq!(session.branch.as_deref(), Some(main.as_str()));
        assert_eq!(session.branches.keys().collect::<Vec<_>>(), ["dev"]);

        // Rapid switches: only the last swap applies, dev's set stays whole.
        git_cli(&root, &["switch", "-q", "dev"], true);
        feed_status(&mut app, 4);
        git_cli(&root, &["switch", "-q", &main], true);
        feed_status(&mut app, 5);
        handle_next(&mut app, branch_set_loaded).await;
        handle_next(&mut app, branch_set_loaded).await;
        assert_eq!(open_names(&app), ["a.txt", "b.txt", "c.txt"]);
        assert_eq!(set_names(&app, "dev"), ["a.txt", "c.txt"]);
        assert_eq!(app.session_branch.as_deref(), Some(main.as_str()));
        assert!(app.branch_swap.pending.is_none());

        // Detached HEAD changes nothing.
        git_cli(&root, &["switch", "-q", "--detach"], true);
        feed_status(&mut app, 6);
        assert_eq!(open_names(&app), ["a.txt", "b.txt", "c.txt"]);
        assert_eq!(app.session_branch.as_deref(), Some(main.as_str()));
        assert!(app.branch_swap.pending.is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
        // In the editor Left/Right keep selecting by word, Up/Down resize the
        // bottom window, and the action resizes the left one.
        app.focus.set(PanelId::Editor);
        assert!(app.layout.is_visible(ToolWindowId::Git));
        let (w, h) = (app.layout.left_w, app.layout.bottom_h);
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Right));
        assert_eq!((app.layout.left_w, app.layout.bottom_h), (w, h));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.layout.bottom_h, h + 1);
        app.perform(Action::App(AppAction::ResizeToolWindow(Dir::Right)));
        assert_eq!(app.layout.left_w, w + 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[tokio::test]
    async fn alt_arrows_cycle_the_git_window_tabs() {
        use crate::actions::GitAction;
        use crate::git::panel::GitTab;
        let (mut app, dir) = test_app("alt-arrows-git");
        app.layout.compute(Rect::new(0, 0, 120, 30));
        app.toggle_tool_window(ToolWindowId::Git);
        assert!(app.focus.is(PanelId::Git));
        assert_eq!(app.git_panel.tab, GitTab::Commit);
        for want in [GitTab::Log, GitTab::Stash, GitTab::Console, GitTab::Commit] {
            app.handle_key(alt(KeyCode::Right));
            assert_eq!(app.git_panel.tab, want);
        }
        app.handle_key(alt(KeyCode::Left));
        assert_eq!(app.git_panel.tab, GitTab::Console);
        app.perform(Action::Git(GitAction::TabCommit));
        app.perform(Action::Git(GitAction::FocusMessage));
        assert!(app.git_panel.message_focused());
        app.handle_key(alt(KeyCode::Right));
        assert_eq!(app.git_panel.tab, GitTab::Commit, "the message box keeps the key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_arrows_flip_the_run_views() {
        use crate::run::panel::View;
        let (mut app, dir) = test_app("alt-arrows-run");
        app.layout.compute(Rect::new(0, 0, 120, 30));
        app.toggle_tool_window(ToolWindowId::Run);
        assert!(app.focus.is(PanelId::Run));
        app.run_panel.start("t", "cargo test", true);
        assert_eq!(app.run_panel.view(), View::Tests);
        app.handle_key(alt(KeyCode::Right));
        assert_eq!(app.run_panel.view(), View::Output);
        app.handle_key(alt(KeyCode::Left));
        assert_eq!(app.run_panel.view(), View::Tests);
        app.run_panel.start("o", "echo", false);
        app.handle_key(alt(KeyCode::Right));
        assert_eq!(app.run_panel.view(), View::Output);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_arrows_elsewhere_still_switch_editor_tabs() {
        let (mut app, dir) = test_app("alt-arrows-editor");
        load(&mut app, "a.txt", "a\n");
        let b = load(&mut app, "b.txt", "b\n");
        app.focus.set(PanelId::Editor);
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        app.handle_key(alt(KeyCode::Right));
        let after_key = app.editor.active_path().map(|p| p.to_path_buf());
        app.handle_key(alt(KeyCode::Left));
        assert_eq!(app.editor.active_path(), Some(b.as_path()));
        app.perform(Action::Editor(EditorAction::NextTab));
        assert_eq!(app.editor.active_path().map(|p| p.to_path_buf()), after_key);
        assert_ne!(after_key.as_deref(), Some(b.as_path()));
        app.focus.set(PanelId::Project);
        let before = app.editor.active_path().map(|p| p.to_path_buf());
        app.perform(Action::App(AppAction::NextToolWindowTab));
        assert_eq!(app.editor.active_path().map(|p| p.to_path_buf()), before);
        // A window without inner tabs leaves Alt+Right to the editor.
        app.handle_key(alt(KeyCode::Right));
        assert_ne!(app.editor.active_path().map(|p| p.to_path_buf()), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tool_window_tab_shortcut_is_shown() {
        let (app, dir) = test_app("tool-window-tab-shortcut");
        assert_eq!(app.shortcut(&Action::App(AppAction::NextToolWindowTab)).as_deref(), Some("Alt+Right"));
        assert_eq!(app.shortcut(&Action::App(AppAction::PrevToolWindowTab)).as_deref(), Some("Alt+Left"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn draw_frame(app: &mut App) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| crate::ui::render(app, f)).unwrap();
    }

    #[tokio::test]
    async fn maximize_fills_the_screen_and_focus_change_restores() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("maximize");
        let max = || KeyEvent::new(KeyCode::Char('M'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        // With the editor focused the key does nothing.
        app.handle_key(max());
        assert_eq!(app.layout.maximized, None);
        app.toggle_tool_window(ToolWindowId::Git);
        draw_frame(&mut app);
        let old_h = app.layout.rects.bottom.height;
        app.handle_key(max());
        draw_frame(&mut app);
        assert_eq!(app.layout.maximized_window(), Some(ToolWindowId::Git));
        assert_eq!(app.layout.rects.bottom.height, 29);
        assert!(app.layout.rects.editor.is_empty());
        app.handle_key(max());
        draw_frame(&mut app);
        assert_eq!(app.layout.rects.bottom.height, old_h);
        app.handle_key(max());
        app.perform(Action::App(AppAction::FocusEditor));
        draw_frame(&mut app);
        assert_eq!(app.layout.maximized, None);
        assert!(!app.layout.rects.editor.is_empty());
        // The kitty spelling of Ctrl+Shift+'.
        app.focus.set(PanelId::Git);
        app.handle_key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::CONTROL));
        assert_eq!(app.layout.maximized, Some(ToolWindowId::Git));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unpinned_window_hides_when_focus_leaves() {
        use crate::actions::{FindAction, PaletteKind};
        let (mut app, dir) = test_app("unpinned");
        app.toggle_tool_window(ToolWindowId::Git);
        app.perform(Action::App(AppAction::TogglePinned { tw: None }));
        draw_frame(&mut app);
        assert!(app.layout.is_visible(ToolWindowId::Git));
        app.perform(Action::App(AppAction::OpenPalette(PaletteKind::Files)));
        draw_frame(&mut app);
        assert!(app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Git));
        app.palette = None;
        app.tool_window_side_menu(ToolWindowId::Git, 1, 1);
        draw_frame(&mut app);
        assert!(app.layout.is_visible(ToolWindowId::Git));
        app.menu = None;
        app.toggle_main_menu();
        draw_frame(&mut app);
        assert!(app.layout.is_visible(ToolWindowId::Git));
        app.toggle_main_menu();
        app.perform(Action::App(AppAction::FocusEditor));
        draw_frame(&mut app);
        assert!(!app.layout.is_visible(ToolWindowId::Git));

        app.toggle_tool_window(ToolWindowId::Find);
        app.perform(Action::App(AppAction::TogglePinned { tw: None }));
        app.perform(Action::Find(FindAction::Escape));
        draw_frame(&mut app);
        assert!(!app.layout.is_visible(ToolWindowId::Find));

        app.toggle_tool_window(ToolWindowId::Problems);
        app.perform(Action::App(AppAction::FocusEditor));
        draw_frame(&mut app);
        assert!(app.layout.is_visible(ToolWindowId::Problems), "pinned stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn escape_hides_only_an_unpinned_window() {
        let (mut app, dir) = test_app("escape-unpinned");
        app.toggle_tool_window(ToolWindowId::Git);
        app.perform(Action::App(AppAction::Escape));
        assert!(app.focus.is(PanelId::Editor));
        assert!(app.layout.is_visible(ToolWindowId::Git));
        app.focus.set(PanelId::Git);
        app.perform(Action::App(AppAction::TogglePinned { tw: None }));
        app.perform(Action::App(AppAction::Escape));
        assert!(!app.layout.is_visible(ToolWindowId::Git));
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn view_menu_pin_row_needs_a_focused_tool_window() {
        let (mut app, dir) = test_app("pin-row");
        let enabled = |app: &super::App| {
            let bar = app.build_main_menu();
            let view = bar.menus().iter().find(|m| m.title == "View").unwrap();
            view.items.iter().find(|i| i.label == "Pin or Unpin Tool Window").unwrap().enabled
        };
        assert!(!enabled(&app));
        app.focus.set(PanelId::Git);
        assert!(enabled(&app));
        app.focus.set(PanelId::Editor);
        let undock = |app: &super::App| {
            let bar = app.build_main_menu();
            let view = bar.menus().iter().find(|m| m.title == "View").unwrap();
            view.items.iter().find(|i| i.label == "Undock or Dock Tool Window").unwrap().enabled
        };
        assert!(!undock(&app));
        app.focus.set(PanelId::Git);
        assert!(undock(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_undocked_is_a_registry_action() {
        assert_eq!(Action::from_id("tool_window.toggle_undocked"), Some(Action::App(AppAction::ToggleUndocked { tw: None })));
    }

    #[tokio::test]
    async fn undocked_window_slides_over_and_hides() {
        use crate::ui::layout::ViewMode;
        use crossterm::event::{MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("undocked");
        app.toggle_tool_window(ToolWindowId::Find);
        app.perform(Action::from_id("tool_window.toggle_undocked").unwrap());
        assert_eq!(app.layout.view_mode(ToolWindowId::Find), ViewMode::Undocked);
        draw_frame(&mut app);
        assert!(app.layout.is_visible(ToolWindowId::Find));
        assert!(app.layout.rects.bottom.is_empty());
        let ov = app.layout.rects.overlay;
        assert!(!ov.is_empty());
        app.perform(Action::App(AppAction::FocusEditor));
        draw_frame(&mut app);
        assert!(app.layout.is_visible(ToolWindowId::Find));
        let click =
            MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: ov.x + 2, row: ov.y + 1, modifiers: KeyModifiers::NONE };
        app.handle_mouse(click);
        assert!(app.focus.is(PanelId::Find));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        draw_frame(&mut app);
        assert!(!app.layout.is_visible(ToolWindowId::Find));
        assert!(app.focus.is(PanelId::Editor));

        app.toggle_tool_window(ToolWindowId::Find);
        app.toggle_tool_window(ToolWindowId::Project);
        draw_frame(&mut app);
        assert!(!app.layout.is_visible(ToolWindowId::Find));

        app.layout.set_view_mode(ToolWindowId::Git, ViewMode::Undocked);
        app.layout.show(ToolWindowId::Find);
        app.toggle_tool_window(ToolWindowId::Git);
        assert!(app.layout.is_visible(ToolWindowId::Git) && !app.layout.is_visible(ToolWindowId::Find));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn undocked_window_stays_on_an_esc_that_keeps_focus() {
        use crate::ui::layout::ViewMode;
        let (mut app, dir) = test_app("undocked-esc");
        app.layout.set_view_mode(ToolWindowId::Project, ViewMode::Undocked);
        app.layout.show(ToolWindowId::Project);
        app.focus.set(PanelId::Project);
        draw_frame(&mut app);
        let key = |c| KeyEvent::new(c, crossterm::event::KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.project.filter(), "l");
        app.handle_key(key(KeyCode::Esc));
        draw_frame(&mut app);
        assert_eq!(app.project.filter(), "");
        assert!(app.focus.is(PanelId::Project), "the first Esc only clears the filter");
        assert!(app.layout.is_visible(ToolWindowId::Project), "focus stayed, so the window stays");
        assert!(!app.layout.rects.overlay.is_empty());
        app.handle_key(key(KeyCode::Esc));
        draw_frame(&mut app);
        assert!(app.focus.is(PanelId::Editor));
        assert!(!app.layout.is_visible(ToolWindowId::Project));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn f12_jumps_to_the_last_tool_window() {
        let (mut app, dir) = test_app("f12-last");
        let f12 = || KeyEvent::new(KeyCode::F(12), crossterm::event::KeyModifiers::NONE);
        app.handle_key(f12());
        assert!(app.focus.is(PanelId::Project));
        app.toggle_tool_window(ToolWindowId::Git);
        app.perform(Action::App(AppAction::FocusEditor));
        app.handle_key(f12());
        assert!(app.focus.is(PanelId::Git));
        app.toggle_tool_window(ToolWindowId::Git);
        assert!(!app.layout.is_visible(ToolWindowId::Git));
        app.handle_key(f12());
        assert!(app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Git));
        app.handle_key(f12());
        assert!(app.layout.is_visible(ToolWindowId::Git) && app.focus.is(PanelId::Git));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_5_and_alt_8_open_event_log_and_local_history() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("alt-5-8");
        app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::ALT));
        assert!(app.layout.is_visible(ToolWindowId::EventLog) && app.focus.is(PanelId::EventLog));
        app.handle_key(KeyEvent::new(KeyCode::Char('8'), KeyModifiers::ALT));
        assert!(app.layout.is_visible(ToolWindowId::LocalHistory) && app.focus.is(PanelId::LocalHistory));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn double_click_on_a_tool_window_border_maximizes() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("dbl-border");
        app.toggle_tool_window(ToolWindowId::Git);
        draw_frame(&mut app);
        let double = |app: &mut App| {
            let r = app.layout.rects.bottom;
            for _ in 0..2 {
                for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
                    app.handle_mouse(MouseEvent { kind, column: r.x + 5, row: r.y, modifiers: KeyModifiers::NONE });
                }
            }
        };
        double(&mut app);
        draw_frame(&mut app);
        assert_eq!(app.layout.maximized_window(), Some(ToolWindowId::Git));
        double(&mut app);
        draw_frame(&mut app);
        assert_eq!(app.layout.maximized, None);
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
    async fn branch_sits_at_the_right_end_and_opens_branches() {
        use crate::ui::status_bar::StatusHit;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use unicode_width::UnicodeWidthStr;
        let (mut app, dir) = test_app("status-branch-right");
        app.handle_msg(AppMsg::Git(GitMsg::Status {
            generation: app.git.generation,
            seq: 1,
            result: Ok(crate::git::model::StatusSnapshot {
                workdir: app.workspace.root.clone(),
                branch: Some("feature/x".into()),
                ..Default::default()
            }),
        }));
        assert_eq!(app.branch_label().as_deref(), Some("feature/x"));
        load(&mut app, "a.txt", "x\n");

        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let frame = term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let row: String = (0..100u16).map(|x| frame.buffer[(x, 29)].symbol()).collect();
        assert!(row.trim_end().ends_with("feature/x"), "{row:?}");
        assert!(row.find("Ln 1, Col 1") < row.find("feature/x") && row.contains("Ln 1, Col 1"), "{row:?}");
        let (_, r) = *app.status_hits.iter().find(|(h, _)| *h == StatusHit::Branch).unwrap();
        assert_eq!(r.right(), 100);
        assert_eq!(r.width, "feature/x".width() as u16 + 2);

        let click = |kind| MouseEvent { kind, column: r.x + 1, row: r.y, modifiers: KeyModifiers::NONE };
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)));
        app.handle_mouse(click(MouseEventKind::Up(MouseButton::Left)));
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(crate::actions::PaletteKind::Branches));

        // Narrow line: position fits, position plus branch does not.
        let frame = term.draw(|f| crate::ui::status_bar::render(&mut app, f, Rect::new(0, 0, 24, 1))).unwrap();
        let row: String = (0..24u16).map(|x| frame.buffer[(x, 0)].symbol()).collect();
        assert!(row.contains("Ln 1, Col 1") && !row.contains("feature/x"), "{row:?}");
        assert!(!app.status_hits.iter().any(|(h, _)| *h == StatusHit::Branch));

        // Rank-1 tie: the branch drops before the modal mode, which drops before the problems.
        let path = app.workspace.root.join("a.txt");
        let lsp_diag = lsp_types::Diagnostic {
            range: lsp_types::Range::default(),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "boom".into(),
            ..Default::default()
        };
        let per_file = app.config.lsp.max_diagnostics_per_file;
        app.lsp.diagnostics.publish("fake", path, vec![crate::lsp::diagnostics::Diagnostic::from_lsp(lsp_diag)], per_file);
        app.dispatcher.modal = true;
        let mode = app.mode.label().to_string();
        let mut seen = [false; 3];
        for w in 10..100u16 {
            let frame = term.draw(|f| crate::ui::status_bar::render(&mut app, f, Rect::new(0, 0, w, 1))).unwrap();
            let row: String = (0..w).map(|x| frame.buffer[(x, 0)].symbol()).collect();
            let (problems, modal, branch) = (row.contains("✖ 1"), row.contains(&mode), row.contains("feature/x"));
            assert!(!branch || modal, "branch kept over modal at {w}: {row:?}");
            assert!(!modal || problems, "modal kept over problems at {w}: {row:?}");
            seen[0] |= problems && !modal;
            seen[1] |= modal && !branch;
            seen[2] |= branch;
        }
        assert_eq!(seen, [true; 3], "every tie step is reached");
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
        assert_eq!(d.id, DialogId::StashMessage { untracked: false });
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
        repo::stash(&root, &repo::StashOp::Push { message: "wip".into(), untracked: false }).unwrap();
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
        let index = crate::git::repo::blob_text(&app.workspace.root, &f, BlobWhich::Index, None).unwrap();
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
        let index = crate::git::repo::blob_text(&app.workspace.root, &f, BlobWhich::Index, None).unwrap();
        assert_eq!(index, base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_arrows_stage_unstage_and_discard_single_lines() {
        use crate::actions::GitAction;
        use crate::git::{BlobWhich, Section};
        let (mut app, dir) = test_app("line-stage");
        let base: String = (1..=12).map(|i| format!("{i}\n")).collect();
        init_repo(&app, &[("f.txt", &base)]);
        let f = app.workspace.root.join("f.txt");
        let with = |extra: &str| base.replacen("5\n", &format!("5\n{extra}"), 1);
        std::fs::write(&f, with("a\nb\nc\n")).unwrap();
        feed_status(&mut app, 1);
        feed_change_diff(&mut app, Section::Unstaged, "f.txt");
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::NextSection));
        app.perform(Action::Git(GitAction::LineDown));
        app.perform(Action::Git(GitAction::LineExtendDown));
        assert!(app.git_panel.diff_line_cursor().1.is_some());
        app.perform(Action::Git(GitAction::ToggleStage));
        assert_eq!(app.git_panel.diff_line_cursor(), (None, None));
        handle_next(&mut app, git_op_done).await;
        let index = crate::git::repo::blob_text(&app.workspace.root, &f, BlobWhich::Index, None).unwrap();
        assert_eq!(index, with("a\nb\n"));
        let cached = std::process::Command::new("git").args(["diff", "--cached"]).current_dir(&app.workspace.root).output().unwrap();
        let cached = String::from_utf8_lossy(&cached.stdout).into_owned();
        assert!(cached.contains("+a") && cached.contains("+b") && !cached.contains("+c"), "{cached}");
        // Unstage `b` from the Staged diff.
        feed_status(&mut app, 2);
        app.perform(Action::Git(GitAction::Escape));
        app.perform(Action::Git(GitAction::Up));
        app.perform(Action::Git(GitAction::Up));
        feed_change_diff(&mut app, Section::Staged, "f.txt");
        app.perform(Action::Git(GitAction::NextSection));
        app.perform(Action::Git(GitAction::LineDown));
        app.perform(Action::Git(GitAction::LineDown));
        assert!(app.git_panel.diff_line_cursor().0.is_some());
        app.perform(Action::Git(GitAction::ToggleStage));
        assert_eq!(app.git_panel.diff_line_cursor(), (None, None));
        handle_next(&mut app, git_op_done).await;
        let index = crate::git::repo::blob_text(&app.workspace.root, &f, BlobWhich::Index, None).unwrap();
        assert_eq!(index, with("a\n"));
        // Discard `c` from the work tree.
        feed_status(&mut app, 3);
        app.perform(Action::Git(GitAction::Escape));
        app.perform(Action::Git(GitAction::Down));
        app.perform(Action::Git(GitAction::Down));
        feed_change_diff(&mut app, Section::Unstaged, "f.txt");
        app.perform(Action::Git(GitAction::NextSection));
        app.perform(Action::Git(GitAction::LineDown));
        app.perform(Action::Git(GitAction::LineDown));
        app.perform(Action::Git(GitAction::Discard));
        let d = app.dialog.take().expect("asks first");
        assert!(matches!(d.id, DialogId::DiscardHunk { .. }));
        app.finish_dialog(d.id, DialogResult::Button(0));
        handle_next(&mut app, git_op_done).await;
        assert_eq!(std::fs::read_to_string(&f).unwrap(), with("a\nb\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn line_staging_hint_on_an_added_file() {
        use crate::actions::GitAction;
        use crate::git::Section;
        let (mut app, dir) = test_app("line-stage-added");
        init_repo(&app, &[("f.txt", "1\n")]);
        std::fs::write(app.workspace.root.join("n.txt"), "x\ny\n").unwrap();
        let root = app.workspace.root.clone();
        let git = |args: &[&str]| {
            let o = std::process::Command::new("git").args(args).current_dir(&root).output().unwrap();
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        git(&["add", "n.txt"]);
        feed_status(&mut app, 1);
        feed_change_diff(&mut app, Section::Staged, "n.txt");
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::NextSection));
        app.perform(Action::Git(GitAction::LineDown));
        app.perform(Action::Git(GitAction::ToggleStage));
        let last = app.notifications.last().map(|n| n.text.clone());
        assert_eq!(last.as_deref(), Some("Only lines of a modified text file can be staged one by one"));
        assert!(git(&["diff", "--cached", "--name-only"]).contains("n.txt"));
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
        let blame = crate::git::repo::blame(&app.workspace.root, &a, None);
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
        let blame = crate::git::repo::blame(&app.workspace.root, &a, None);
        app.handle_msg(AppMsg::Git(GitMsg::Blame { generation: g, seq: 3, path: a, result: blame }));
        assert!(app.editor.buffers[0].blame.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn inline_blame_loads_quietly_without_the_column() {
        use crate::editor::view::gutter_width;
        let (mut app, dir) = test_app("inline-blame");
        init_repo(&app, &[("a.txt", "one\ntwo\n")]);
        let a = load(&mut app, "a.txt", "one\ntwo\n");
        assert!(app.editor.want_inline_blame().is_none(), "off by default");
        let plain = gutter_width(&app.editor.buffers[0], 0);
        assert_eq!(plain, 7);
        let mut c = app.config.clone();
        c.editor.inline_blame = true;
        app.editor.apply_config(&c);
        app.on_tick();
        let state = app.editor.buffers[0].blame.as_ref().expect("asked");
        assert!(!state.column);
        assert_eq!(gutter_width(&app.editor.buffers[0], 0), plain);
        assert!(app.editor.want_inline_blame().is_none(), "asks once");
        let g = app.git.generation;
        let blame = crate::git::repo::blame(&app.workspace.root, &a, None);
        app.handle_msg(AppMsg::Git(GitMsg::Blame { generation: g, seq: u64::MAX / 2, path: a.clone(), result: blame }));
        let state = app.editor.buffers[0].blame.as_ref().unwrap();
        assert_eq!(state.commit_at(0).map(|c| c.summary.as_str()), Some("init"));
        // The column shows and hides without dropping the data.
        app.perform(Action::Editor(EditorAction::ToggleBlame));
        let state = app.editor.buffers[0].blame.as_ref().unwrap();
        assert!(state.column && state.data.is_some());
        assert_eq!(gutter_width(&app.editor.buffers[0], 0), plain + crate::editor::blame::WIDTH);
        app.perform(Action::Editor(EditorAction::ToggleBlame));
        let state = app.editor.buffers[0].blame.as_ref().unwrap();
        assert!(!state.column && state.data.is_some());
        // Turning the key off drops the inline-only state.
        c.editor.inline_blame = false;
        app.editor.apply_config(&c);
        assert!(app.editor.buffers[0].blame.is_none());
        // An untracked file fails quietly and is not asked again.
        c.editor.inline_blame = true;
        app.editor.apply_config(&c);
        let b = load(&mut app, "b.txt", "x\n");
        let (path, _) = app.editor.want_inline_blame().expect("asks for b.txt");
        assert_eq!(path, b);
        app.notifications.clear();
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Blame {
            generation: g,
            seq: u64::MAX / 2,
            path: b.clone(),
            result: Err(anyhow::anyhow!("untracked")),
        }));
        let state = app.editor.buffers.iter().find(|x| x.path.as_deref() == Some(b.as_path())).unwrap().blame.as_ref().unwrap();
        assert!(state.data.is_none());
        assert!(app.notifications.is_empty(), "{:?}", app.notifications);
        assert!(app.editor.want_inline_blame().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_renamed_buffer_keeps_its_git_base_until_the_rename_is_committed() {
        use crate::editor::gutter::MarkKind;
        use crate::files::ops::FsOp;
        let (mut app, dir) = test_app("rename-base");
        let text = "one\ntwo\nthree\n";
        init_repo(&app, &[("a.rs", text), ("b.rs", "b\n")]);
        feed_status(&mut app, 1);
        let old = load(&mut app, "a.rs", text);
        type_text(&mut app, "x ");
        let new = app.workspace.root.join("c.rs");
        app.run_fs_op(FsOp::Rename { from: old.clone(), to: new.clone(), overwrite: false });
        handle_next(&mut app, |m| matches!(m, AppMsg::FsOpDone { .. })).await;
        assert_eq!(app.editor.buffers[0].path.as_deref(), Some(new.as_path()));
        assert_eq!(app.editor.buffers[0].base_path.as_deref(), Some(old.as_path()));
        // Another file is committed: HEAD moves and every base reloads.
        let root = app.workspace.root.clone();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&root).status().unwrap().success());
        std::fs::write(root.join("b.rs"), "b2\n").unwrap();
        git(&["add", "b.rs"]);
        git(&["-c", "user.name=Test", "-c", "user.email=t@e.x", "commit", "-qm", "other"]);
        feed_status(&mut app, 2);
        let want = new.clone();
        handle_next(&mut app, |m| matches!(m, AppMsg::Git(GitMsg::Blob { path, .. }) if *path == want)).await;
        app.editor.update_gutters();
        let kinds = |app: &App| app.editor.buffers[0].gutter.iter().map(|m| (m.line, m.kind)).collect::<Vec<_>>();
        assert_eq!(app.editor.buffers[0].diff_base.as_deref(), Some(text));
        assert_eq!(kinds(&app), vec![(0, MarkKind::Modified)], "one edited line, not every line added");
        // Renaming back drops the old path.
        app.run_fs_op(FsOp::Rename { from: new, to: old, overwrite: false });
        handle_next(&mut app, |m| matches!(m, AppMsg::FsOpDone { .. })).await;
        assert_eq!(app.editor.buffers[0].base_path, None);
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
    async fn in_progress_merge_greys_and_refuses_switching() {
        use crate::actions::GitAction;
        use crate::git::model::{BranchOp, InProgress, InProgressKind};
        let (mut app, dir) = test_app("in-progress");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&app, &["switch", "-qc", "dev"]);
        std::fs::write(root.join("a.txt"), "dev\n").unwrap();
        git(&app, &["commit", "-qam", "dev change"]);
        git(&app, &["switch", "-q", &main]);
        std::fs::write(root.join("a.txt"), "main\n").unwrap();
        git(&app, &["commit", "-qam", "main change"]);
        app.git_request(crate::git::GitRequest::Merge("dev".into()));
        handle_next(&mut app, git_op_done).await;
        feed_status(&mut app, 2);
        app.dialog = None;
        assert_eq!(app.in_progress(), Some(InProgress { kind: InProgressKind::Merge, step: None }));
        // The Log menu greys out the switching rows.
        app.git_panel.branches = crate::git::repo::branches(&root).unwrap();
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        app.perform(Action::Git(GitAction::ContextMenu));
        let menu = app.menu.as_ref().expect("context menu");
        let enabled = |label: &str| menu.items.iter().find(|i| i.label == label).unwrap_or_else(|| panic!("{label}")).enabled;
        for label in ["Checkout Revision", "New Branch…", "Cherry-Pick", "Revert Commit…"] {
            assert!(!enabled(label), "{label}");
        }
        assert!(enabled("Copy Revision Number") && enabled("Show Diff"));
        let branch_row = menu.items.iter().find(|i| i.label.starts_with("Branch '")).expect("branch row");
        let shown = branch_row.label.trim_start_matches("Branch '").trim_end_matches('\'').to_string();
        let child_enabled =
            |label: &str| branch_row.children.iter().find(|c| c.label == label).unwrap_or_else(|| panic!("{label}")).enabled;
        assert!(!child_enabled(&format!("New Branch from '{shown}'…")));
        assert!(child_enabled("Rename…"), "ops git allows mid-merge stay enabled");
        app.menu = None;
        // The branches pane's menu greys out the same rows.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        app.perform(Action::Git(GitAction::Left));
        for _ in 0..3 {
            app.perform(Action::Git(GitAction::Down));
        }
        app.perform(Action::Git(GitAction::OpenFile));
        let menu = app.menu.as_ref().expect("branch menu");
        let enabled = |label: &str| menu.items.iter().find(|i| i.label == label).unwrap_or_else(|| panic!("{label}")).enabled;
        for label in [
            "Checkout".to_string(),
            "New Branch from 'dev'…".to_string(),
            format!("Merge 'dev' into '{main}'…"),
            format!("Rebase '{main}' onto 'dev'…"),
        ] {
            assert!(!enabled(&label), "{label}");
        }
        for label in [format!("Compare with '{main}'"), "Rename…".to_string(), "Delete…".to_string()] {
            assert!(enabled(&label), "{label}");
        }
        app.menu = None;
        app.perform(Action::Git(GitAction::BranchOp { branch: "dev".into(), op: BranchOp::Merge }));
        assert_eq!(app.notifications.last().unwrap().text, "A merge is in progress: finish or abort it first");
        assert!(app.dialog.is_none());
        // The action itself refuses.
        let refusal = "A merge is in progress: finish or abort it first";
        app.perform(Action::Git(GitAction::LogCherryPick));
        assert_eq!(app.notifications.last().unwrap().text, refusal);
        assert!(app.git.busy.is_empty());
        // The branches popup leaves out the switching operations.
        let dev = crate::git::repo::branches(&root).unwrap().into_iter().find(|b| b.name == "dev").unwrap();
        app.git_request(crate::git::GitRequest::BranchOps(dev));
        let d = app.dialog.take().expect("branch operations");
        let DialogId::BranchOps { ops, .. } = &d.id else { panic!("{:?}", d.id) };
        for op in [BranchOp::Merge, BranchOp::Rebase, BranchOp::Checkout, BranchOp::NewFrom] {
            assert!(!ops.contains(&op), "{op:?}");
        }
        assert!(ops.contains(&BranchOp::Delete));
        assert!(d.message.contains("in progress"), "{}", d.message);
        // Switching refuses.
        app.switch_branch("dev".into());
        assert_eq!(app.notifications.last().unwrap().text, refusal);
        assert!(app.git.busy.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn log_menu_branch_submenu_runs_branch_operations() {
        use crate::actions::GitAction;
        use crate::git::model::BranchOp;
        use crate::git::{LogFilter, LogRev};
        let (mut app, dir) = test_app("log-menu-branches");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&app, &["branch", "dev"]);
        feed_status(&mut app, 1);
        app.git_panel.branches = crate::git::repo::branches(&root).unwrap();
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        app.perform(Action::Git(GitAction::ContextMenu));
        let menu = app.menu.as_ref().expect("context menu");
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert!(menu.items.iter().any(|i| i.is_separator()), "separator: {labels:?}");
        assert!(labels.contains(&format!("Branch '{main}'").as_str()) && labels.contains(&"Branch 'dev'"), "{labels:?}");
        let row = menu.items.iter().find(|i| i.label == "Branch 'dev'").unwrap();
        assert!(row.action.is_none() && row.enabled);
        let dev_info = app.git_panel.branches.iter().find(|b| b.name == "dev").unwrap();
        let want: Vec<String> = BranchOp::for_branch(dev_info, Some(&main)).into_iter().map(|(_, l)| l).collect();
        let got: Vec<String> = row.children.iter().map(|c| c.label.clone()).collect();
        assert_eq!(got, want);
        let child = |op| {
            let a = Action::Git(GitAction::BranchOp { branch: "dev".into(), op });
            row.children.iter().find(|c| c.action.as_ref() == Some(&a)).unwrap_or_else(|| panic!("{op:?}")).action.clone().unwrap()
        };
        let (merge, compare) = (child(BranchOp::Merge), child(BranchOp::Compare));
        app.menu = None;
        app.perform(merge);
        let d = app.dialog.take().expect("merge confirm");
        assert_eq!(d.id, DialogId::MergeBranch { name: "dev".into() });
        app.perform(compare);
        let want = LogFilter { rev: LogRev::Rev("HEAD..dev".into()), ..Default::default() };
        assert_eq!(app.git_panel.log_filter(), &want);
        app.perform(Action::Git(GitAction::BranchOp { branch: "gone".into(), op: BranchOp::Merge }));
        let n = app.notifications.last().unwrap();
        assert_eq!((n.level, n.text.as_str()), (crate::event::Level::Info, "Branch 'gone' no longer exists"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn branches_pane_filters_log_and_opens_branch_menu() {
        use crate::actions::GitAction;
        use crate::git::LogRev;
        use crate::git::model::BranchOp;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("branches-pane");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        let main = crate::git::repo::status(&root).unwrap().branch.unwrap();
        git(&app, &["branch", "dev"]);
        feed_status(&mut app, 1);
        app.git_panel.branches = crate::git::repo::branches(&root).unwrap();
        app.toggle_tool_window(ToolWindowId::Git);
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        assert!(app.git_panel.branches_rect().width > 0);
        app.perform(Action::Git(GitAction::Left));
        for _ in 0..3 {
            app.perform(Action::Git(GitAction::Down));
        }
        app.perform(Action::Git(GitAction::ToggleStage));
        assert_eq!(app.git_panel.log_filter().rev, LogRev::Rev("dev".into()));
        app.perform(Action::Git(GitAction::OpenFile));
        let menu = app.menu.as_ref().expect("branch menu");
        let dev_info = app.git_panel.branches.iter().find(|b| b.name == "dev").unwrap();
        let want: Vec<String> = BranchOp::for_branch(dev_info, Some(&main)).into_iter().map(|(_, l)| l).collect();
        let got: Vec<String> = menu.items.iter().map(|i| i.label.clone()).collect();
        assert_eq!(got, want);
        assert!(menu.items.iter().all(|i| !i.is_separator()));
        app.menu = None;
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let r = app.git_panel.branches_rect();
        let modifiers = KeyModifiers::NONE;
        let click = |app: &mut App, button, row| {
            let column = r.x + 3;
            app.handle_mouse(MouseEvent { kind: MouseEventKind::Down(button), column, row, modifiers });
            app.handle_mouse(MouseEvent { kind: MouseEventKind::Up(button), column, row, modifiers });
        };
        click(&mut app, MouseButton::Left, r.y);
        assert_eq!(app.git_panel.log_filter().rev, LogRev::Head);
        assert!(app.menu.is_none());
        click(&mut app, MouseButton::Right, r.y + 3);
        assert!(app.menu.is_some());
        app.menu = None;
        // Esc returns focus to the list, so the context menu is the commit menu.
        app.perform(Action::Git(GitAction::Escape));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        app.perform(Action::Git(GitAction::ContextMenu));
        let menu = app.menu.as_ref().expect("commit menu");
        assert!(menu.items.iter().any(|i| i.label == "Copy Revision Number"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn show_diff_as_patch_opens_a_patch_tab() {
        use crate::actions::GitAction;
        let (mut app, dir) = test_app("show-diff");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        git(&app, &["commit", "-qam", "change a"]);
        feed_status(&mut app, 1);
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        let sha8 = rows[0].sha[..8].to_string();
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        let is_diff = |m: &AppMsg| matches!(m, AppMsg::Git(GitMsg::CommitsDiff { .. }));
        // A stale answer opens nothing.
        let before = app.editor.buffers.len();
        app.handle_msg(AppMsg::Git(GitMsg::CommitsDiff {
            generation: g + 1,
            title: "Diff x".into(),
            spans: Vec::new(),
            result: Ok("x\n".into()),
        }));
        assert_eq!(app.editor.buffers.len(), before);
        app.perform(Action::Git(GitAction::LogShowPatch));
        handle_next(&mut app, is_diff).await;
        let b = app.editor.active_buffer().expect("patch tab");
        assert!(b.read_only && b.path.is_none());
        assert_eq!(b.name, format!("Patch {sha8}"));
        let text = b.text();
        assert!(text.contains("-a\n") && text.contains("+b\n"), "{text}");
        assert!(app.focus.is(PanelId::Editor));
        app.perform(Action::Editor(EditorAction::NextChange));
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.text().lines().nth(b.cursor_line_col().0), Some("-a"));
        let count = app.editor.buffers.len();
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::LogShowPatch));
        handle_next(&mut app, is_diff).await;
        assert_eq!(app.editor.buffers.len(), count);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn show_diff_opens_a_diff_tab_over_the_commit_files() {
        use crate::actions::GitAction;
        use crate::git::model::DiffKey;
        let (mut app, dir) = test_app("show-diff-tab");
        init_repo(&app, &[("a.txt", "a\n")]);
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/b.txt"), "n\n").unwrap();
        git(&app, &["add", "."]);
        git(&app, &["commit", "-qm", "change a, add d/b"]);
        feed_status(&mut app, 1);
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        let sha8 = rows[0].sha[..8].to_string();
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        let is_files = |m: &AppMsg| matches!(m, AppMsg::Git(GitMsg::CommitsFiles { tab: Some(_), .. }));
        // A stale answer opens nothing.
        let before = app.editor.buffers.len();
        app.handle_msg(AppMsg::Git(GitMsg::CommitsFiles {
            generation: g + 1,
            spans: Vec::new(),
            tab: Some("Diff x".into()),
            result: Ok(Vec::new()),
        }));
        assert_eq!(app.editor.buffers.len(), before);
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::ShowDiff));
        handle_next(&mut app, is_files).await;
        let b = app.editor.active_buffer().expect("diff tab");
        assert_eq!(b.name, format!("Diff {sha8}"));
        let t = b.diff.as_ref().expect("diff tab");
        let rels: Vec<&str> = t.files.iter().map(DiffKey::rel).collect();
        assert_eq!(rels, ["a.txt", "d/b.txt"]);
        assert!(t.files.iter().all(|k| matches!(k, DiffKey::Range { .. })));
        assert!(app.focus.is(PanelId::Editor));
        handle_next(&mut app, |m| matches!(m, AppMsg::Git(GitMsg::Diff { .. }))).await;
        assert!(app.editor.active_buffer().unwrap().diff.as_ref().unwrap().pane.doc.is_some());
        app.perform(Action::Editor(EditorAction::DiffNextFile));
        assert_eq!(app.editor.active_buffer().unwrap().diff.as_ref().unwrap().at, 1);
        let count = app.editor.buffers.len();
        app.focus.set(PanelId::Git);
        app.perform(Action::Git(GitAction::LogShowDiff));
        handle_next(&mut app, is_files).await;
        assert_eq!(app.editor.buffers.len(), count);
        assert_eq!(app.editor.active_buffer().unwrap().diff.as_ref().unwrap().at, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn log_ctrl_click_on_the_last_row_fetches_the_next_page() {
        use crate::actions::GitAction;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("log-ctrl-page");
        init_repo(&app, &[("x.txt", "x=1\n")]);
        let root = app.workspace.root.clone();
        for v in ["x=2", "x=3"] {
            std::fs::write(root.join("x.txt"), format!("{v}\n")).unwrap();
            git(&app, &["commit", "-qam", v]);
        }
        feed_status(&mut app, 1);
        app.toggle_tool_window(ToolWindowId::Git);
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(rows.len(), 3);
        let g = app.git.generation;
        // A full first page does not mark the log as ended.
        app.git_panel.set_log_page(3);
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        assert!(!app.git_panel.log_loading());
        let r = app.git_panel.log_rect();
        let (column, row) = (r.x + 3, r.y + 2);
        let modifiers = KeyModifiers::CONTROL;
        app.handle_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers });
        app.handle_mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column, row, modifiers });
        assert_eq!(app.git_panel.log_selection(), [0, 2]);
        assert!(app.git_panel.log_loading());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn log_multi_select_combines_a_first_parent_range() {
        use crate::actions::GitAction;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (mut app, dir) = test_app("log-multi-select");
        init_repo(&app, &[("x.txt", "x=1\n")]);
        let root = app.workspace.root.clone();
        for v in ["x=2", "x=3"] {
            std::fs::write(root.join("x.txt"), format!("{v}\n")).unwrap();
            git(&app, &["commit", "-qam", v]);
        }
        feed_status(&mut app, 1);
        app.toggle_tool_window(ToolWindowId::Git);
        app.perform(Action::Git(GitAction::TabLog));
        let filter = app.git_panel.log_filter().clone();
        let out =
            std::process::Command::new("git").args(crate::git::cli::log_args(0, 100, true, &filter)).current_dir(&root).output().unwrap();
        let rows = crate::git::cli::parse_log(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(rows.len(), 3);
        let (c8, a8) = (rows[0].sha[..8].to_string(), rows[2].sha[..8].to_string());
        let oldest_parent = rows[2].parents.first().cloned();
        assert_eq!(oldest_parent, None);
        let g = app.git.generation;
        app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(rows) }));
        app.perform(Action::Git(GitAction::ExtendDown));
        app.perform(Action::Git(GitAction::ExtendDown));
        assert_eq!(app.git_panel.log_selection(), [0, 1, 2]);
        // At the last row the range stays.
        app.perform(Action::Git(GitAction::ExtendDown));
        assert_eq!(app.git_panel.log_selection(), [0, 1, 2]);
        app.perform(Action::Git(GitAction::LogShowDiff));
        let is_diff = |m: &AppMsg| matches!(m, AppMsg::Git(GitMsg::CommitsFiles { tab: Some(_), .. }));
        handle_next(&mut app, is_diff).await;
        let b = app.editor.active_buffer().expect("diff tab");
        assert_eq!(b.name, format!("Diff {a8}..{c8}"));
        let files = &b.diff.as_ref().expect("diff tab").files;
        assert_eq!(files.len(), 1);
        let crate::git::model::DiffKey::Range { base, top, rel, .. } = &files[0] else { panic!("range key: {files:?}") };
        assert_eq!(rel, "x.txt");
        assert_eq!(base, &oldest_parent);
        assert!(top.starts_with(&c8), "{top}");
        app.perform(Action::Git(GitAction::Down));
        assert_eq!(app.git_panel.log_selection(), [2]);
        app.perform(Action::Git(GitAction::Up));
        app.perform(Action::Git(GitAction::Up));
        app.perform(Action::Git(GitAction::Up));
        assert_eq!(app.git_panel.log_selection(), [0]);
        // Mouse: one draw lays the rows out.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        term.draw(|f| crate::ui::render(&mut app, f)).unwrap();
        let r = app.git_panel.log_rect();
        let press = |app: &mut App, row: u16, button, modifiers| {
            let (column, row) = (r.x + 3, r.y + row);
            app.handle_mouse(MouseEvent { kind: MouseEventKind::Down(button), column, row, modifiers });
            app.handle_mouse(MouseEvent { kind: MouseEventKind::Up(button), column, row, modifiers });
        };
        press(&mut app, 0, MouseButton::Left, KeyModifiers::NONE);
        press(&mut app, 2, MouseButton::Left, KeyModifiers::CONTROL);
        assert_eq!(app.git_panel.log_selection(), [0, 2]);
        assert_eq!(app.git_panel.log_cursor(), 2);
        press(&mut app, 2, MouseButton::Left, KeyModifiers::CONTROL);
        // The deselected row gives up the cursor; Show Diff covers row 0 only.
        assert_eq!(app.git_panel.log_selection(), [0]);
        assert_eq!(app.git_panel.log_cursor(), 0);
        // The last selected row stays.
        press(&mut app, 0, MouseButton::Left, KeyModifiers::CONTROL);
        assert_eq!(app.git_panel.log_selection(), [0]);
        assert_eq!(app.git_panel.log_cursor(), 0);
        app.perform(Action::Git(GitAction::LogShowDiff));
        handle_next(&mut app, is_diff).await;
        let one = app.editor.active_buffer().unwrap();
        assert!(one.name.starts_with("Diff ") && !one.name.contains(" of "), "{}", one.name);
        press(&mut app, 0, MouseButton::Left, KeyModifiers::NONE);
        press(&mut app, 2, MouseButton::Left, KeyModifiers::SHIFT);
        assert_eq!(app.git_panel.log_selection(), [0, 1, 2]);
        press(&mut app, 1, MouseButton::Right, KeyModifiers::NONE);
        assert_eq!(app.git_panel.log_selection(), [0, 1, 2]);
        assert!(app.menu.is_some());
        app.menu = None;
        // Outside the selection a right press selects that row alone.
        press(&mut app, 0, MouseButton::Left, KeyModifiers::NONE);
        press(&mut app, 2, MouseButton::Right, KeyModifiers::NONE);
        assert_eq!(app.git_panel.log_selection(), [2]);
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
    async fn alt_w_chords_drive_groups() {
        use crossterm::event::KeyModifiers;
        let (mut app, dir) = test_app("alt-w-groups");
        load(&mut app, "a.txt", "a\n");
        load(&mut app, "b.txt", "b\n");
        app.perform(Action::Editor(EditorAction::SplitRight));
        let ids = app.editor.group_ids();
        app.focus.set(PanelId::Project);
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT));
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focus.current, PanelId::Editor);
        assert_eq!(app.editor.focused_group().id, ids[0]);
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT));
        app.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT));
        assert_eq!(app.editor.group_ids().len(), 1);
        let find = app.keymap.table("find").unwrap();
        let w = crate::config::keymap::parse_chord("alt-w").unwrap();
        assert_eq!(find.bindings.get(&w), Some(&Action::Find(crate::actions::FindAction::ToggleWord)));
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
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
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
    use crate::actions::{Action, AppAction, Dir};
    use crate::ui::focus::{Dock, PanelId, Side, ToolWindowId};
    use crate::ui::layout::{StripButton, StripDrop};
    use crate::ui::snapshot::row_text;
    use ratatui::layout::Rect;

    fn ev(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    /// A left click: press and release on one cell.
    fn click(app: &mut super::App, x: u16, y: u16) {
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), x, y));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), x, y));
    }

    /// The rect of a window's strip button in the last frame.
    fn button(app: &super::App, tw: ToolWindowId) -> Rect {
        app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(tw)).unwrap_or_else(|| panic!("{tw:?} has no button")).1
    }

    fn draw_at(app: &mut super::App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone()
    }

    fn draw(app: &mut super::App) -> ratatui::buffer::Buffer {
        draw_at(app, 100, 24)
    }

    #[tokio::test]
    async fn icons_follow_the_config() {
        use crate::config::Icons;
        let (mut app, dir) = test_app("strip-icons");
        for (icons, project, bar) in [(Icons::Emoji, "📁", " ☰  File"), (Icons::Unicode, "⌂", " ≣  File")] {
            app.config.ui.icons = icons;
            let buf = draw(&mut app);
            let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Project)).unwrap();
            assert_eq!(r.height, 1);
            assert_eq!(row_text(&buf, r.y, 3).trim(), project, "{icons:?}");
            // The button colours span the whole strip, behind a wide glyph too.
            assert!((0..3).all(|x| buf[(x, r.y)].bg == buf[(0, r.y)].bg), "{icons:?}");
            assert_eq!(row_text(&buf, 0, 3).trim(), crate::ui::menu_bar::menu_glyph(icons));
            app.toggle_main_menu();
            let buf = draw(&mut app);
            assert!(row_text(&buf, 0, 20).starts_with(bar), "{icons:?}");
            app.toggle_main_menu();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn every_tool_window_is_reachable_at_24_rows() {
        let (mut app, dir) = test_app("strip-fit");
        draw_at(&mut app, 80, 24);
        assert_eq!(app.layout.rects.strip.height, 23);
        let hits = app.layout.strip_hits.clone();
        assert_eq!(hits[0], (StripButton::Menu, Rect::new(0, 0, 3, 1)));
        for tw in ToolWindowId::ALL {
            let (_, r) =
                *hits.iter().find(|(b, _)| *b == StripButton::Tool(tw)).unwrap_or_else(|| panic!("{tw:?} has no button: {hits:?}"));
            assert_eq!(r.height, 1, "{tw:?}");
            assert!(app.layout.rects.strip.contains(r.as_position()) && r.bottom() <= app.layout.rects.strip.bottom());
        }
        assert!(!hits.iter().any(|(b, _)| *b == StripButton::More));
        assert!(app.layout.strip_overflow(false).is_empty());
        for (i, (_, a)) in hits.iter().enumerate() {
            assert!(hits[i + 1..].iter().all(|(_, b)| a.intersection(*b).is_empty()), "{hits:?}");
        }

        // The menu button opens the main menu; the bar then covers it.
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 1, 0));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 1, 0));
        assert!(app.menu_bar.is_some());
        app.toggle_main_menu();
        // A tool button toggles its window.
        let (_, r) = *hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Git)).unwrap();
        click(&mut app, r.x + 1, r.y);
        assert!(app.layout.is_visible(ToolWindowId::Git));
        click(&mut app, r.x + 1, r.y);
        assert!(!app.layout.is_visible(ToolWindowId::Git));
        let (_, r) = *hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Hierarchy)).unwrap();
        click(&mut app, r.x + 1, r.y);
        assert!(app.layout.is_visible(ToolWindowId::Hierarchy));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn more_button_lists_the_windows_without_a_button() {
        use ToolWindowId::*;
        let (mut app, dir) = test_app("strip-more");
        let buf = draw_at(&mut app, 80, 12);
        assert_eq!(app.layout.rects.strip.height, 11);
        let tools: Vec<_> = app.layout.strip_hits.iter().filter(|(b, _)| matches!(b, StripButton::Tool(_))).cloned().collect();
        assert_eq!(tools.len(), 9, "{tools:?}");
        let row = |tw| tools.iter().find(|(b, _)| *b == StripButton::Tool(tw)).unwrap().1.y;
        assert_eq!([row(Project), row(Worktrees), row(Structure)], [1, 2, 3]);
        assert_eq!([row(Find), row(EventLog)], [9, 4]);
        assert!(app.layout.strip_hits.contains(&(StripButton::More, Rect::new(0, 10, 3, 1))));
        assert_eq!(app.layout.strip_overflow(false), [LocalHistory, Tasks, Todo, Hierarchy, Debug]);
        assert_eq!(row_text(&buf, 10, 3).trim(), "\u{22ef}");

        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 1, 10));
        let menu = app.menu.as_ref().expect("more menu");
        let labels: Vec<_> = menu.items.iter().map(|i| i.label.clone()).collect();
        assert_eq!(labels, ["Local History", "Tasks", "TODO", "Hierarchy", "Debug"]);
        let todo = Action::App(AppAction::ToggleToolWindow(Todo));
        assert_eq!(menu.items[2].shortcut, app.shortcut(&todo).unwrap_or_default());
        let a = app.menu.take().unwrap().items[2].action.clone().unwrap();
        app.perform(a);
        assert!(app.layout.is_visible(Todo));
        let buf = draw_at(&mut app, 80, 12);
        if let Some(bg) = app.theme.strip_item_active.bg {
            assert_eq!(buf[(1, 10)].bg, bg);
        }
        app.handle_mouse(ev(MouseEventKind::Moved, 1, 10));
        let buf = draw_at(&mut app, 80, 12);
        let tip: String = (3..30).map(|x| buf[(x, 10)].symbol().to_string()).collect();
        assert!(tip.starts_with(" More Tool Windows "), "{tip}");

        app.move_tool_window(Hierarchy, Dock { side: Side::Right, split: false });
        draw_at(&mut app, 80, 12);
        let hits = &app.layout.strip_hits;
        assert!(hits.contains(&(StripButton::Tool(Hierarchy), Rect::new(77, 2, 3, 2))), "{hits:?}");
        assert_eq!(app.layout.strip_overflow(false), [LocalHistory, Tasks, Todo, Debug]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn strip_button_toggles_on_release() {
        use ToolWindowId::*;
        let (mut app, dir) = test_app("strip-release");
        draw_at(&mut app, 120, 40);
        let (g, run) = (button(&app, Git), button(&app, Run));
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), g.x + 1, g.y));
        assert!(!app.layout.is_visible(Git));
        assert!(matches!(app.drag, Some(super::Drag::StripButton { tw: Git, moved: false, .. })));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), g.x + 1, g.y));
        assert!(app.layout.is_visible(Git) && app.drag.is_none());
        app.toggle_tool_window(Git);

        // Left to another button and back: a drag, so no toggle and no reorder.
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), g.x + 1, g.y));
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), run.x + 1, run.y));
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), g.x + 1, g.y));
        assert_eq!(app.strip_drop(), None);
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), g.x + 1, g.y));
        assert!(!app.layout.is_visible(Git));
        assert_eq!(app.layout.order(), ToolWindowId::ALL);

        // Released on another button without motion events: nothing.
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), g.x + 1, g.y));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), run.x + 1, run.y));
        assert!(!app.layout.is_visible(Git) && !app.layout.is_visible(Run));
        assert_eq!(app.layout.order(), ToolWindowId::ALL);

        // The middle button does nothing.
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Middle), g.x + 1, g.y));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Middle), g.x + 1, g.y));
        assert!(!app.layout.is_visible(Git) && app.drag.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drag `tw` from its button to `(x, y)`, release there.
    fn drag_button(app: &mut super::App, tw: ToolWindowId, x: u16, y: u16) {
        let r = button(app, tw);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), r.x + 1, r.y));
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), x, y));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), x, y));
    }

    fn group(app: &super::App, side: Side) -> Vec<ToolWindowId> {
        app.layout.order().iter().copied().filter(|t| app.layout.side_of(*t) == side).collect()
    }

    #[tokio::test]
    async fn dragging_a_strip_button_reorders_and_moves() {
        use ToolWindowId::*;
        // (a) Within a group, with the marker drawn on the target.
        let (mut app, dir) = test_app("strip-drag-a");
        draw_at(&mut app, 120, 40);
        let (find, problems) = (button(&app, Find), button(&app, Problems));
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), find.x + 1, find.y));
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), problems.x + 1, problems.y));
        assert_eq!(app.strip_drop(), Some(StripDrop::Onto(Problems)));
        let buf = draw_at(&mut app, 120, 40);
        if let Some(bg) = app.theme.strip_drop.bg {
            assert!((0..problems.width).all(|d| buf[(problems.x + d, problems.y)].bg == bg));
            assert!((0..find.width).all(|d| buf[(find.x + d, find.y)].bg != bg));
        }
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), problems.x + 1, problems.y));
        assert_eq!(group(&app, Side::Bottom)[..3], [Run, Problems, Find]);
        draw_at(&mut app, 120, 40);
        assert_eq!(button(&app, Find), problems);
        assert_eq!(app.strip_drop(), None);
        let _ = std::fs::remove_dir_all(&dir);

        // (b) Onto a button of another group: the window changes side.
        let (mut app, dir) = test_app("strip-drag-b");
        draw_at(&mut app, 120, 40);
        let wt = button(&app, Worktrees);
        drag_button(&mut app, Git, wt.x + 1, wt.y);
        assert_eq!(app.layout.side_of(Git), Side::Left);
        draw_at(&mut app, 120, 40);
        let (g, wt) = (button(&app, Git), button(&app, Worktrees));
        assert!(g.y < wt.y && g.y > button(&app, Project).y, "{g:?} {wt:?}");
        assert_eq!(group(&app, Side::Left)[..3], [Project, Git, Worktrees]);
        let _ = std::fs::remove_dir_all(&dir);

        // (c) Onto the right strip: a shown window stays shown.
        let (mut app, dir) = test_app("strip-drag-c");
        app.move_tool_window(Hierarchy, Dock { side: Side::Right, split: false });
        app.toggle_tool_window(Git);
        draw_at(&mut app, 120, 40);
        let h = button(&app, Hierarchy);
        drag_button(&mut app, Git, h.x + 1, h.y);
        assert_eq!(app.layout.side_of(Git), Side::Right);
        assert_eq!(app.layout.right, Some(Git));
        let _ = std::fs::remove_dir_all(&dir);

        // (d) A side without a button: the bar, then the drop.
        let (mut app, dir) = test_app("strip-drag-d");
        for tw in [Project, Worktrees, Structure] {
            app.move_tool_window(tw, Dock { side: Side::Bottom, split: false });
        }
        draw_at(&mut app, 120, 40);
        let f = button(&app, Find);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), f.x + 1, f.y));
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 1, 3));
        assert_eq!(app.strip_drop(), Some(StripDrop::End(Dock { side: Side::Left, split: false })));
        let buf = draw_at(&mut app, 120, 40);
        assert_eq!(row_text(&buf, 1, 3), "━━━");
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 1, 3));
        assert_eq!(app.layout.side_of(Find), Side::Left);
        let _ = std::fs::remove_dir_all(&dir);

        // (d2) The Bottom and Right bars use the drop colour too.
        let (mut app, dir) = test_app("strip-drag-d2");
        app.move_tool_window(Hierarchy, Dock { side: Side::Right, split: false });
        for tw in [Find, Run, Problems, Terminal, Git, EventLog, LocalHistory, Tasks, Todo, Debug] {
            app.move_tool_window(tw, Dock { side: Side::Left, split: false });
        }
        draw_at(&mut app, 120, 40);
        let p = button(&app, Project);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), p.x + 1, p.y));
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 1, 38));
        assert_eq!(app.strip_drop(), Some(StripDrop::End(Dock { side: Side::Bottom, split: false })));
        let buf = draw_at(&mut app, 120, 40);
        if let Some(bg) = app.theme.strip_drop.bg {
            assert!(
                (0..app.layout.rects.strip.height)
                    .any(|d| buf[(1, app.layout.rects.strip.y + d)].symbol() == "━" && buf[(1, app.layout.rects.strip.y + d)].bg == bg)
            );
        }
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 1, 38));
        let _ = std::fs::remove_dir_all(&dir);

        // (e) Dropped on the menu cell or outside the strip: nothing.
        let (mut app, dir) = test_app("strip-drag-e");
        draw_at(&mut app, 120, 40);
        drag_button(&mut app, Find, 1, 0);
        drag_button(&mut app, Find, 60, 10);
        assert_eq!(app.layout.order(), ToolWindowId::ALL);
        assert_eq!(app.layout.side_of(Find), Side::Bottom);
        assert!(app.menu_bar.is_none() && app.drag.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dragging_into_the_gap_splits_and_reorders_inside_a_group() {
        use ToolWindowId::*;
        let (mut app, dir) = test_app("strip-drag-split");
        draw_at(&mut app, 120, 40);
        let s = button(&app, Structure);
        drag_button(&mut app, Structure, 1, s.bottom() + 2);
        assert_eq!(app.layout.dock_of(Structure), Dock { side: Side::Left, split: true });
        let buf = draw_at(&mut app, 120, 40);
        let (w, s) = (button(&app, Worktrees), button(&app, Structure));
        assert!(s.y > w.bottom(), "{w:?} {s:?}");
        assert!((w.bottom()..s.y).any(|y| row_text(&buf, y, 3) == "───"));
        // Dropping onto a button of a split group keeps the split.
        app.move_tool_window(Project, Dock { side: Side::Left, split: true });
        app.toggle_tool_window(Structure);
        draw_at(&mut app, 120, 40);
        let before = app.layout.order().iter().position(|t| *t == Structure) < app.layout.order().iter().position(|t| *t == Project);
        let p = button(&app, Project);
        drag_button(&mut app, Structure, p.x + 1, p.y);
        assert_eq!(app.layout.dock_of(Structure), Dock { side: Side::Left, split: true });
        assert_eq!(app.layout.left2, Some(Structure));
        let after = app.layout.order().iter().position(|t| *t == Structure) < app.layout.order().iter().position(|t| *t == Project);
        assert_ne!(before, after, "buttons swapped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn side_menu_move_to_submenu() {
        let (mut app, dir) = test_app("side-menu-move-to");
        app.toggle_tool_window(ToolWindowId::Git);
        app.tool_window_side_menu(ToolWindowId::Git, 1, 1);
        let menu = app.menu.take().expect("context menu");
        assert_eq!(menu.items[0].label, "Move to");
        let kids = &menu.items[0].children;
        assert_eq!(
            kids.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["Left Top", "Left Bottom", "Bottom Left", "Bottom Right", "Right Top", "Right Bottom"]
        );
        assert_eq!(kids.iter().map(|i| i.enabled).collect::<Vec<_>>(), [true, true, false, true, true, true]);
        assert_eq!(menu.items[5].label, "Restore Default Layout");
        app.perform(kids[1].action.clone().unwrap());
        assert_eq!(app.layout.left2, Some(ToolWindowId::Git));
        assert_eq!(app.layout.dock_of(ToolWindowId::Git), Dock { side: Side::Left, split: true });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dropping_on_the_strip_separator_keeps_the_group() {
        use ToolWindowId::*;
        let left_bottom = Dock { side: Side::Left, split: true };
        let left_top = Dock { side: Side::Left, split: false };
        for (w, h) in [(120, 40), (80, 24)] {
            let (mut app, dir) = test_app("strip-sep-drop");
            let drag_to = |app: &mut super::App, tw: ToolWindowId, y: u16| {
                let r = button(app, tw);
                app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), r.x + 1, r.y));
                app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 1, y));
            };
            app.move_tool_window(Structure, left_bottom);
            let buf = draw_at(&mut app, w, h);
            let sep = button(&app, Worktrees).bottom();
            assert!(row_text(&buf, sep, 3).contains('─'), "{w}x{h}: row {sep} is not the separator");
            drag_to(&mut app, Structure, sep);
            assert_eq!(app.strip_drop(), None, "{w}x{h}");
            app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 1, sep));
            assert_eq!(app.layout.dock_of(Structure), left_bottom, "{w}x{h}");

            app.move_tool_window(Git, left_bottom);
            draw_at(&mut app, w, h);
            let sep = button(&app, Worktrees).bottom();
            drag_to(&mut app, Git, sep);
            assert_eq!(app.strip_drop(), Some(StripDrop::Onto(Structure)), "{w}x{h}");
            app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 1, sep));
            assert_eq!(app.layout.dock_of(Git), left_bottom, "{w}x{h}");
            draw_at(&mut app, w, h);

            let sep = button(&app, Worktrees).bottom();
            drag_to(&mut app, Project, sep);
            assert_eq!(app.strip_drop(), Some(StripDrop::Onto(Worktrees)), "{w}x{h}");
            app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 1, sep));
            assert_eq!(app.layout.dock_of(Project), left_top, "{w}x{h}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn a_window_without_a_button_has_a_more_cell() {
        use ToolWindowId::*;
        let rt = Dock { side: Side::Right, split: false };
        let rb = Dock { side: Side::Right, split: true };
        let shapes: [(&str, &[(ToolWindowId, Dock)]); 4] = [
            ("default", &[]),
            ("structure left bottom", &[(Structure, Dock { side: Side::Left, split: true })]),
            ("git right top, problems right bottom", &[(Git, rt), (Problems, rb)]),
            ("problems bottom right", &[(Problems, Dock { side: Side::Bottom, split: true })]),
        ];
        for (name, moves) in shapes {
            let (mut app, dir) = test_app("strip-more-cell");
            for (tw, d) in moves {
                app.move_tool_window(*tw, *d);
            }
            for h in 2..=45u16 {
                draw_at(&mut app, 120, h);
                for (right, strip) in [(false, app.layout.rects.strip), (true, app.layout.rects.strip_right)] {
                    if strip.is_empty() || (!right && strip.height == 1) {
                        continue;
                    }
                    if app.layout.strip_overflow(right).is_empty() {
                        continue;
                    }
                    let more = Rect::new(strip.x, strip.bottom() - 1, strip.width, 1);
                    assert!(app.layout.strip_hits.contains(&(StripButton::More, more)), "{name} h={h} right={right}: no More cell");
                    let docks = crate::ui::layout::LayoutState::strip_docks(right);
                    let has = |d: Dock| ToolWindowId::ALL.iter().any(|t| app.layout.dock_of(*t) == d);
                    let sep = u16::from(has(docks[0]) && has(docks[1]));
                    let off = if right { 2 } else { 1 };
                    let tools = app
                        .layout
                        .strip_hits
                        .iter()
                        .filter(|(b, r)| matches!(b, StripButton::Tool(_)) && strip.contains(r.as_position()))
                        .count() as u16;
                    assert_eq!(tools, strip.height.saturating_sub(off + sep + 1), "{name} h={h} right={right}: wasted slot");
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        // The two named cases.
        let (mut app, dir) = test_app("strip-more-left");
        app.move_tool_window(Structure, Dock { side: Side::Left, split: true });
        draw_at(&mut app, 80, 3);
        let strip = app.layout.rects.strip;
        let more = Rect::new(strip.x, 1, strip.width, 1);
        assert!(app.layout.strip_hits.contains(&(StripButton::More, more)), "{:?}", app.layout.strip_hits);
        assert_eq!(app.layout.strip_overflow(false).len(), ToolWindowId::ALL.iter().filter(|t| !app.layout.on_right_strip(**t)).count());
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), more.x + 1, more.y));
        assert!(app.menu.is_some());
        let _ = std::fs::remove_dir_all(&dir);

        let (mut app, dir) = test_app("strip-more-right");
        app.move_tool_window(Git, rt);
        draw_at(&mut app, 80, 3);
        let strip = app.layout.rects.strip_right;
        assert_eq!(strip.height, 2);
        let more = Rect::new(strip.x, 1, strip.width, 1);
        assert!(app.layout.strip_hits.contains(&(StripButton::More, more)), "{:?}", app.layout.strip_hits);
        assert_eq!(app.layout.strip_overflow(true), [Git]);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), more.x + 1, more.y));
        assert!(app.menu.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn right_strip_has_one_row_buttons_and_a_more_button() {
        use ToolWindowId::*;
        let (mut app, dir) = test_app("strip-right-more");
        for tw in [Find, Run, Problems, Terminal, Git, EventLog, LocalHistory, Tasks, Todo, Hierarchy] {
            app.move_tool_window(tw, Dock { side: Side::Right, split: false });
        }
        draw_at(&mut app, 80, 12);
        let strip = app.layout.rects.strip_right;
        assert_eq!(strip.height, 11);
        let more = Rect::new(strip.x, strip.bottom() - 1, strip.width, 1);
        assert!(app.layout.strip_hits.contains(&(StripButton::More, more)), "{:?}", app.layout.strip_hits);
        assert_eq!(button(&app, Find), Rect::new(strip.x, 2, strip.width, 1));
        assert_eq!(app.layout.strip_overflow(true), [Todo, Hierarchy]);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), more.x + 1, more.y));
        let menu = app.menu.as_ref().expect("more menu");
        assert_eq!(menu.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["TODO", "Hierarchy"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hovering_a_strip_button_shows_its_name_and_shortcut() {
        let (mut app, dir) = test_app("strip-hover");
        draw(&mut app);
        let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Project)).unwrap();
        app.dirty_frame = false;
        app.handle_mouse(ev(MouseEventKind::Moved, r.x, r.y));
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

    #[tokio::test]
    async fn toolbar_hosts_the_menu_button() {
        use crate::ui::layout::ToolbarButton;
        let (mut app, dir) = test_app("toolbar-menu");
        app.config.ui.toolbar = true;
        let buf = draw(&mut app);
        assert_eq!(app.layout.toolbar_hits[0], (ToolbarButton::Menu, Rect::new(0, 0, 3, 1)));
        assert!(!app.layout.strip_hits.iter().any(|(b, _)| *b == StripButton::Menu));
        assert!(app.layout.strip_hits.iter().all(|(_, r)| r.y >= 1));
        for tw in ToolWindowId::ALL {
            button(&app, tw);
        }
        assert!(row_text(&buf, 0, 100).starts_with(" ≣ "));
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 1, 0));
        assert!(app.menu_bar.is_some());
        draw(&mut app);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 1, 0));
        assert!(app.menu_bar.is_none());

        app.handle_mouse(ev(MouseEventKind::Moved, 1, 0));
        assert_eq!(app.toolbar_hover, Some((ToolbarButton::Menu, Rect::new(0, 0, 3, 1))));
        let buf = draw(&mut app);
        let key = app.shortcut(&Action::App(AppAction::MainMenu)).unwrap();
        let row = row_text(&buf, 1, 40);
        assert!(row.contains("Main Menu") && row.contains(&key), "{row:?}");
        app.handle_mouse(ev(MouseEventKind::Moved, 50, 10));
        assert!(app.toolbar_hover.is_none());

        // The strip toggles still work one row lower.
        let r = button(&app, ToolWindowId::Git);
        click(&mut app, r.x + 1, r.y);
        assert!(app.layout.is_visible(ToolWindowId::Git));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn toolbar_branch_widget_replaces_the_status_branch() {
        use crate::actions::PaletteKind;
        use crate::event::AppMsg;
        use crate::git::GitMsg;
        use crate::git::model::{InProgress, InProgressKind, StatusSnapshot};
        use crate::ui::layout::ToolbarButton;
        use crate::ui::status_bar::StatusHit;
        let (mut app, dir) = test_app("toolbar-branch");
        app.config.ui.toolbar = true;
        let status = |app: &mut super::App, seq, in_progress| {
            app.handle_msg(AppMsg::Git(GitMsg::Status {
                generation: app.git.generation,
                seq,
                result: Ok(StatusSnapshot {
                    workdir: app.workspace.root.clone(),
                    branch: Some("feature/x".into()),
                    in_progress,
                    ..Default::default()
                }),
            }));
        };
        status(&mut app, 1, None);
        let buf = draw(&mut app);
        let (_, r) = *app.layout.toolbar_hits.iter().find(|(b, _)| *b == ToolbarButton::Branch).unwrap();
        assert_eq!((r.x, r.y), (13, 0));
        assert!(row_text(&buf, 0, 100).contains(" ⎇ feature/x ▾ "));
        assert!(!row_text(&buf, 23, 100).contains("feature/x"));
        assert!(!app.status_hits.iter().any(|(h, _)| *h == crate::ui::status_bar::StatusHit::Branch));

        app.handle_mouse(ev(MouseEventKind::Moved, r.x + 1, 0));
        let buf = draw(&mut app);
        assert!(row_text(&buf, 1, 100).contains("Branches"));
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), r.x + 1, 0));
        assert_eq!(app.palette.as_ref().map(|p| p.kind), Some(PaletteKind::Branches));
        app.palette = None;

        status(&mut app, 2, Some(InProgress { kind: InProgressKind::Rebase, step: Some((2, 3)) }));
        let buf = draw(&mut app);
        assert!(row_text(&buf, 0, 100).contains(" ⎇ feature/x (Rebasing 2/3) ▾ "));

        app.config.ui.toolbar = false;
        let buf = draw(&mut app);
        assert!(row_text(&buf, 23, 100).trim_end().ends_with("feature/x"));
        assert!(app.status_hits.iter().any(|(h, _)| *h == StatusHit::Branch));

        // Widget dropped from the row (narrow step 5): the status bar shows it again.
        app.config.ui.toolbar = true;
        draw(&mut app);
        app.layout.toolbar_hits.retain(|(b, _)| *b != ToolbarButton::Branch);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 1)).unwrap();
        let frame = term.draw(|f| crate::ui::status_bar::render(&mut app, f, Rect::new(0, 0, 100, 1))).unwrap();
        assert!(row_text(frame.buffer, 0, 100).contains("feature/x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_toolbar_config(app: &super::App, on: bool) {
        let file = app.paths.config_file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, format!("# mine\n[lsp]\nenabled = false\n[ui]\ntoolbar = {on}\n")).unwrap();
    }

    #[tokio::test]
    async fn toggle_toolbar_writes_the_config() {
        let (mut app, dir) = test_app("toggle-toolbar");
        write_toolbar_config(&app, false);
        app.config.ui.toolbar = false;
        app.perform(Action::App(AppAction::ToggleToolbar));
        assert!(app.config.ui.toolbar);
        let text = std::fs::read_to_string(app.paths.config_file()).unwrap();
        assert!(text.contains("toolbar = true") && text.contains("# mine") && text.contains("enabled = false"), "{text}");
        assert!(!app.config.lsp.enabled);
        draw(&mut app);
        assert!(!app.layout.toolbar_hits.is_empty());
        app.perform(Action::App(AppAction::ToggleToolbar));
        assert!(!app.config.ui.toolbar);
        let text = std::fs::read_to_string(app.paths.config_file()).unwrap();
        assert!(text.contains("toolbar = false"), "{text}");
        draw(&mut app);
        assert!(app.layout.toolbar_hits.is_empty());
        assert_eq!(app.layout.strip_hits[0].0, StripButton::Menu);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn toolbar_right_click_hides_it() {
        use crossterm::event::{KeyCode, KeyEvent};
        let (mut app, dir) = test_app("toolbar-right");
        write_toolbar_config(&app, true);
        app.config.ui.toolbar = true;
        draw(&mut app);
        let right = |app: &mut super::App, x| {
            app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Right), x, 0));
            app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Right), x, 0));
        };
        right(&mut app, 50);
        let menu = app.menu.as_ref().expect("context menu");
        assert_eq!(menu.items.len(), 1);
        assert_eq!(menu.items[0].label, "Hide Toolbar");
        assert_eq!(menu.items[0].action, Some(Action::App(AppAction::ToggleToolbar)));
        assert!(app.menu_bar.is_none());
        app.menu = None;
        right(&mut app, 1);
        assert!(app.menu.is_some());
        assert!(app.menu_bar.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.menu.is_none());
        assert!(!app.config.ui.toolbar);
        draw(&mut app);
        assert!(app.layout.toolbar_hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn toolbar_hit(app: &super::App, b: crate::ui::layout::ToolbarButton) -> Rect {
        toolbar_hit_in(&app.layout.toolbar_hits, b)
    }

    #[tokio::test]
    async fn status_bar_shows_the_branch_while_the_menu_bar_covers_the_toolbar() {
        use crate::event::AppMsg;
        use crate::git::GitMsg;
        use crate::git::model::StatusSnapshot;
        use crate::ui::status_bar::StatusHit;
        let (mut app, dir) = test_app("toolbar-branch-menu");
        app.config.ui.toolbar = true;
        app.handle_msg(AppMsg::Git(GitMsg::Status {
            generation: app.git.generation,
            seq: 1,
            result: Ok(StatusSnapshot { workdir: app.workspace.root.clone(), branch: Some("feature/x".into()), ..Default::default() }),
        }));
        let on_status = |app: &super::App| app.status_hits.iter().any(|(h, _)| *h == StatusHit::Branch);

        let buf = draw(&mut app);
        assert!(row_text(&buf, 0, 100).contains("feature/x"));
        assert!(!row_text(&buf, 23, 100).contains("feature/x"));
        assert!(!on_status(&app));

        app.toggle_main_menu();
        let buf = draw(&mut app);
        assert!(!row_text(&buf, 0, 100).contains("feature/x"), "the menu bar covers the widget");
        assert!(row_text(&buf, 23, 100).trim_end().ends_with("feature/x"));
        assert!(on_status(&app));

        app.toggle_main_menu();
        let buf = draw(&mut app);
        assert!(row_text(&buf, 0, 100).contains("feature/x"));
        assert!(!row_text(&buf, 23, 100).contains("feature/x"));
        assert!(!on_status(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn toolbar_run_widget_runs_the_run_actions() {
        use crate::ui::layout::ToolbarButton as B;
        use crate::ui::palette::Source;
        let (mut app, dir) = test_app("toolbar-run");
        app.config.ui.toolbar = true;
        draw(&mut app);
        let run = toolbar_hit(&app, B::Run);
        assert_eq!(run.right(), 100 - 8);
        let cfg = toolbar_hit(&app, B::RunConfig);
        app.run.set_loaded_tasks(vec![]);
        click(&mut app, cfg.x + 1, 0);
        assert!(app.palette.is_none());
        draw(&mut app);
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.rect().y, 1);
        assert!(menu.rect().x <= cfg.x);
        assert_eq!(menu.items[0].label, "No Configurations");
        app.menu = None;
        assert!(app.run.selected.is_none());
        click(&mut app, run.x + 1, 0);
        assert!(app.palette.as_ref().unwrap().wants(Source::Tasks));
        app.palette = None;

        app.run.fake_running("nap");
        let buf = draw(&mut app);
        let buttons: Vec<_> = app.layout.toolbar_hits.iter().map(|(b, _)| *b).collect();
        assert_eq!(buttons, [B::Menu, B::Project, B::RunConfig, B::Rerun, B::Stop, B::Search, B::Settings]);
        assert!(row_text(&buf, 0, 100).contains("nap ▾"));
        let rerun = toolbar_hit(&app, B::Rerun);
        click(&mut app, rerun.x + 1, 0);
        assert!(app.palette.as_ref().unwrap().wants(Source::Tasks));
        app.palette = None;
        let stop = toolbar_hit(&app, B::Stop);
        assert!(!app.run.stop_requested());
        click(&mut app, stop.x + 1, 0);
        assert!(app.run.stop_requested(), "the Stop press reached stop_run");
        assert!(!app.notifications.iter().any(|n| n.text.contains("No task is running")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn toolbar_project_search_settings() {
        use crate::actions::PaletteKind;
        use crate::ui::layout::ToolbarButton as B;
        let (mut app, dir) = test_app("toolbar-pss");
        app.config.ui.toolbar = true;
        // A startup toast would cover the tooltip at the right edge.
        app.toasts.expire(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        let buf = draw(&mut app);
        let project = toolbar_hit(&app, B::Project);
        assert_eq!(project, Rect::new(4, 0, 8, 1));
        assert!(row_text(&buf, 0, 100).starts_with(" ≣   root ▾ "), "{:?}", row_text(&buf, 0, 100));
        let hits = app.layout.toolbar_hits.clone();
        let (search, settings) = (hits[hits.len() - 2], hits[hits.len() - 1]);
        assert_eq!((search.0, settings.0), (B::Search, B::Settings));
        assert_eq!(settings.1.right(), 100);
        assert_eq!(search.1.right(), settings.1.x - 1);

        click(&mut app, project.x + 1, 0);
        assert_eq!(app.palette.as_ref().unwrap().kind, PaletteKind::RecentProjects);
        app.palette = None;
        draw(&mut app);
        click(&mut app, search.1.x + 1, 0);
        assert_eq!(app.palette.as_ref().unwrap().kind, PaletteKind::Everywhere);
        app.palette = None;
        draw(&mut app);
        click(&mut app, settings.1.x + 1, 0);
        assert!(app.settings.is_some());
        app.settings = None;
        draw(&mut app);

        app.handle_mouse(ev(MouseEventKind::Moved, search.1.x + 1, 0));
        let buf = draw(&mut app);
        let row = row_text(&buf, 1, 100);
        assert!(row.contains(" Search Everywhere "), "{row:?}");
        app.handle_mouse(ev(MouseEventKind::Moved, 50, 10));

        let wide = {
            draw_at(&mut app, 200, 24);
            app.layout.toolbar_hits.clone()
        };
        let left = |b: B| matches!(b, B::Menu | B::Project | B::Branch);
        let left_end = wide.iter().filter(|(b, _)| left(*b)).map(|(_, r)| r.right()).max().unwrap();
        let right_start = wide.iter().filter(|(b, _)| !left(*b)).map(|(_, r)| r.x).min().unwrap();
        let needed = left_end + 1 + (200 - right_start);
        assert!(needed <= 120, "{needed}");
        let has = |app: &super::App, b: B| app.layout.toolbar_hits.iter().any(|(x, _)| *x == b);
        draw_at(&mut app, needed, 24);
        assert!(has(&app, B::Settings) && has(&app, B::Search));
        draw_at(&mut app, needed - 1, 24);
        assert!(!has(&app, B::Settings) && has(&app, B::Search));
        draw_at(&mut app, needed - 5, 24);
        assert!(!has(&app, B::Settings) && !has(&app, B::Search));
        assert_eq!(toolbar_hit(&app, B::Project).width, toolbar_hit_in(&wide, B::Project).width);
        draw_at(&mut app, 30, 24);
        assert_eq!(toolbar_hit(&app, B::Menu), Rect::new(0, 0, 3, 1));
        for (b, _) in wide.iter().filter(|(b, _)| matches!(b, B::Run | B::Rerun | B::Stop)) {
            assert!(has(&app, *b), "{b:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn toolbar_hit_in(hits: &[(crate::ui::layout::ToolbarButton, Rect)], b: crate::ui::layout::ToolbarButton) -> Rect {
        hits.iter().find(|(x, _)| *x == b).map(|(_, r)| *r).expect("toolbar button")
    }

    #[tokio::test]
    async fn toolbar_run_tooltip() {
        use crate::ui::layout::ToolbarButton as B;
        let (mut app, dir) = test_app("toolbar-run-tip");
        app.config.ui.toolbar = true;
        app.run.fake_running("nap");
        // A startup toast would cover the tooltip at the right edge.
        app.toasts.expire(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        draw(&mut app);
        let stop = toolbar_hit(&app, B::Stop);
        app.handle_mouse(ev(MouseEventKind::Moved, stop.x + 1, 0));
        let buf = draw(&mut app);
        let key = app.shortcut(&Action::Run(crate::actions::RunAction::Stop)).unwrap();
        let row = row_text(&buf, 1, 100);
        assert!(row.contains("Stop") && row.contains(&key), "{row:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn toolbar_hides_below_12_rows_and_in_zen() {
        use crate::ui::layout::Hit;
        let (mut app, dir) = test_app("toolbar-hide");
        app.config.ui.toolbar = true;
        draw_at(&mut app, 80, 11);
        assert!(app.layout.toolbar_hits.is_empty());
        assert_eq!(app.layout.strip_hits[0], (StripButton::Menu, Rect::new(0, 0, 3, 1)));
        draw_at(&mut app, 80, 12);
        assert!(app.layout.toolbar_hits.iter().any(|(b, _)| *b == crate::ui::layout::ToolbarButton::Menu));
        app.perform(Action::App(AppAction::ToggleZen));
        assert!(app.layout.zen.is_some());
        draw_at(&mut app, 80, 24);
        assert!(app.layout.toolbar_hits.is_empty());
        assert_ne!(app.layout.hit_test(1, 0), Hit::Toolbar);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn strip_menu_sets_the_view_mode() {
        use crate::ui::layout::ViewMode;
        let (mut app, dir) = test_app("strip-pin");
        draw(&mut app);
        let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Git)).unwrap();
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Right), r.x + 1, r.y));
        let menu = app.menu.take().expect("context menu");
        assert_eq!(menu.items[7].label, "View Mode");
        let kids = &menu.items[7].children;
        assert_eq!(kids.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Dock Pinned", "Dock Unpinned", "Undock"]);
        assert_eq!(kids.iter().map(|i| i.enabled).collect::<Vec<_>>(), [false, true, true]);
        app.perform(kids[1].action.clone().unwrap());
        assert_eq!(app.layout.view_mode(ToolWindowId::Git), ViewMode::Unpinned);
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Right), r.x + 1, r.y));
        let menu = app.menu.take().unwrap();
        assert_eq!(menu.items[7].children.iter().map(|i| i.enabled).collect::<Vec<_>>(), [true, false, true]);
        app.perform(menu.items[7].children[2].action.clone().unwrap());
        assert_eq!(app.layout.view_mode(ToolWindowId::Git), ViewMode::Undocked);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn strip_right_click_moves_a_window() {
        let (mut app, dir) = test_app("strip-move");
        draw(&mut app);
        let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Git)).unwrap();
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Right), r.x + 1, r.y));
        let menu = app.menu.as_ref().expect("context menu");
        let labels: Vec<_> = menu.items.iter().map(|i| i.label.clone()).collect();
        assert_eq!(labels.len(), 8);
        assert_eq!(labels[6..], ["", "View Mode"]);
        assert_eq!(labels[2..4], ["Move Button Up", "Move Button Down"]);
        assert_eq!(labels[5], "Restore Default Layout");
        assert_eq!(menu.items.iter().map(|i| i.enabled).collect::<Vec<_>>(), [true, true, true, true, true, true, true, true]);
        assert!(!app.layout.is_visible(ToolWindowId::Git), "right-click does not toggle");
        let pos = |app: &super::App, tw| app.layout.order().iter().position(|t| *t == tw).unwrap();
        assert!(pos(&app, ToolWindowId::Terminal) < pos(&app, ToolWindowId::Git));
        let a = app.menu.take().unwrap().items[3].action.clone().unwrap();
        app.perform(a);
        assert!(pos(&app, ToolWindowId::Git) < pos(&app, ToolWindowId::Terminal), "Move Button Down swapped it with the button below");
        draw(&mut app);
        let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Git)).unwrap();
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Right), r.x + 1, r.y));
        let a = app.menu.take().expect("context menu").items[0].children[4].action.clone().unwrap();
        app.perform(a);
        draw(&mut app);
        assert_eq!(app.layout.side_of(ToolWindowId::Git), Side::Right);
        let strip = app.layout.rects.strip_right;
        assert_eq!(strip, Rect::new(97, 0, 3, 23));
        let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Git)).unwrap();
        assert!(strip.contains(r.as_position()));
        // A toast in the top-right corner stays left of the strip and does not take the click.
        app.notify(crate::event::Level::Info, "toast");
        draw(&mut app);
        assert!(!app.toasts.is_empty());
        click(&mut app, r.x + 1, r.y);
        assert_eq!(app.layout.right, Some(ToolWindowId::Git));
        assert!(app.focus.is(PanelId::Git));
        let (_, p) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Project)).unwrap();
        click(&mut app, p.x + 1, p.y);
        assert!(app.layout.is_visible(ToolWindowId::Project));
        assert!(app.focus.is(PanelId::Project));
        // Toasts are drawn over the tooltip row; clear them to read it.
        app.toasts = Default::default();
        app.handle_mouse(ev(MouseEventKind::Moved, r.x, r.y));
        let buf = draw(&mut app);
        let row: String = (0..97).map(|x| buf[(x, r.y)].symbol().to_string()).collect();
        assert!(row.trim_end().ends_with(" Git  Alt+9"), "{row}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_default_layout_from_key_menu_and_side_menu() {
        let (mut app, dir) = test_app("restore-default");
        app.toggle_tool_window(ToolWindowId::Git);
        app.layout.move_button(ToolWindowId::Git, true);
        app.move_tool_window(ToolWindowId::Git, Dock { side: Side::Right, split: false });
        app.focus.set(PanelId::Git);
        app.perform(Action::App(AppAction::MaximizeToolWindow));
        assert!(!app.layout.order_overrides().is_empty());
        assert!(app.layout.maximized.is_some());
        app.handle_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::F(12), KeyModifiers::SHIFT));
        assert_eq!(app.layout.side_of(ToolWindowId::Git), Side::Bottom);
        assert!(app.layout.order_overrides().is_empty());
        assert_eq!(app.layout.maximized, None);
        assert!(app.layout.is_visible(ToolWindowId::Git));
        assert!(app.focus.is(PanelId::Git));

        app.move_tool_window(ToolWindowId::Git, Dock { side: Side::Right, split: false });
        app.perform(Action::App(AppAction::FocusPanel(PanelId::Run)));
        draw(&mut app);
        let (_, r) = *app.layout.strip_hits.iter().find(|(b, _)| *b == StripButton::Tool(ToolWindowId::Git)).unwrap();
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Right), r.x + 1, r.y));
        let a = app.menu.take().expect("context menu").items[5].action.clone().unwrap();
        app.perform(a);
        assert_eq!(app.layout.bottom, Some(ToolWindowId::Git));
        assert!(!app.layout.is_visible(ToolWindowId::Run));
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn move_focused_window_by_action() {
        let (mut app, dir) = test_app("move-window");
        app.toggle_tool_window(ToolWindowId::Git);
        app.perform(Action::App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Right, split: false } }));
        assert_eq!((app.layout.right, app.layout.bottom), (Some(ToolWindowId::Git), None));
        assert!(app.focus.is(PanelId::Git));
        draw(&mut app);
        let w = app.layout.right_w;
        // Ctrl+Shift+Right pulls the right window's edge in; the editor has no room to give.
        app.perform(Action::App(AppAction::ResizeToolWindow(Dir::Right)));
        assert_eq!(app.layout.right_w, w - 2);
        app.perform(Action::App(AppAction::ResizeToolWindow(Dir::Left)));
        assert_eq!(app.layout.right_w, w);
        app.focus.set(PanelId::Project);
        app.perform(Action::App(AppAction::MoveToolWindow { tw: Some(ToolWindowId::Git), dock: Dock { side: Side::Left, split: false } }));
        assert_eq!(app.layout.left, Some(ToolWindowId::Git));
        assert!(!app.layout.is_visible(ToolWindowId::Project));
        assert!(app.focus.is(PanelId::Editor), "the focused window was displaced");

        // View menu rows.
        let enabled = |app: &super::App| {
            let bar = app.build_main_menu();
            let view = bar.menus().iter().find(|m| m.title == "View").unwrap();
            [
                "Move Tool Window to Left Top",
                "Move Tool Window to Left Bottom",
                "Move Tool Window to Bottom Left",
                "Move Tool Window to Bottom Right",
                "Move Tool Window to Right Top",
                "Move Tool Window to Right Bottom",
            ]
            .map(|l| view.items.iter().find(|i| i.label == l).unwrap().enabled)
        };
        assert_eq!(enabled(&app), [false; 6], "editor focused");
        app.focus.set(PanelId::Git);
        assert_eq!(enabled(&app), [false, true, true, true, true, true]);
        assert_eq!(
            Action::from_id("tool_window.move_bottom_right"),
            Some(Action::App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Bottom, split: true } }))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
