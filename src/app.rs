//! The application: owns all panels, routes input, applies effects, and runs
//! the event loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use futures::StreamExt;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::actions::{Action, AppAction, EditorAction, ModeAction, PaletteKind};
use crate::config::keymap::Keymap;
use crate::config::theme::Theme;
use crate::config::{Config, Paths};
use crate::dispatch::{Dispatch, Dispatcher, Mode, is_text_input};
use crate::editor::Editor;
use crate::event::{AppMsg, Effect, Level, Mouse};
use crate::files::ProjectPanel;
use crate::git::{BlobWhich, GitMsg, GitOp, GitPanel, GitRequest, GitService};
use crate::tui::Term;
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::focus::{Focus, PanelId, ToolWindowId};
use crate::ui::layout::{Hit, LayoutState, SplitterId};
use crate::ui::palette::{Accept, ActionEntry, Palette, PaletteResult, Source};
use crate::ui::status_bar::StatusHit;
use crate::ui::tabs::TabHit;
use crate::workspace::Workspace;
use crate::worktree::WorktreePanel;

/// Build a `Ctx` from disjoint fields so panels can be borrowed mutably at
/// the same time.
macro_rules! ctx {
    ($app:expr) => {
        Ctx { effects: Vec::new(), config: &$app.config, theme: &$app.theme, workspace: &$app.workspace, bus: $app.bus_tx.clone() }
    };
}

/// What panels get while handling input.
pub struct Ctx<'a> {
    pub effects: Vec<Effect>,
    pub config: &'a Config,
    pub theme: &'a Theme,
    pub workspace: &'a Workspace,
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
    /// Git generation (bumped on every root switch) the files belong to.
    pub generation: u64,
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
    pub toasts: crate::ui::toasts::Toasts,
    pub dialog: Option<Dialog>,
    /// Command palette overlay; captures input like a dialog.
    pub palette: Option<Palette>,
    pub file_index: FileIndex,
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
        let mut git = GitService::new(root.clone(), bus_tx.clone());
        git.tab_width = config.editor.tab_width;
        let mut git_panel = GitPanel::new();
        git_panel.set_log_page(config.git.log_page);
        let mut app = Self {
            workspace: Workspace::new(root),
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
            toasts: Default::default(),
            dialog: None,
            palette: None,
            file_index: FileIndex::default(),
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
            app.notify(Level::Warn, "Terminal lacks the kitty keyboard protocol: Ctrl+Shift shortcuts, Ctrl+Tab and Shift Shift are unavailable");
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
        // Expire the visible notification and pending chord display.
        if let Some(n) = self.notifications.last() {
            let ttl = Duration::from_secs(16);
            if n.at.elapsed() < ttl && n.at.elapsed().as_millis() % 1000 < 100 {
                self.dirty_frame = true;
            }
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

    fn handle_msg(&mut self, msg: AppMsg) {
        self.dirty_frame = true;
        match msg {
            AppMsg::Notify { level, text } => self.notify(level, text),
            AppMsg::SwitchRoot(p) => self.apply_effects(vec![Effect::SwitchRoot(p)]),
            AppMsg::FileLoaded { generation, path, result, focus } => {
                if generation == self.git.generation {
                    self.file_loaded(path, result, focus);
                } else {
                    tracing::debug!("dropping stale load of {}", path.display());
                }
            }
            AppMsg::FileSaved { .. } => {}
            AppMsg::Git(m) => self.on_git_msg(m),
            AppMsg::Worktrees { generation, result } => {
                if generation == self.git.generation {
                    self.worktrees.set_list(result, &self.workspace.root);
                } else {
                    tracing::debug!("dropping stale worktree list");
                }
            }
            AppMsg::SessionLoaded { generation, session, files } => self.session_loaded(generation, session, files),
            AppMsg::FileIndex { generation, files } => self.file_index_loaded(generation, files),
            AppMsg::Lsp(_) => {}
        }
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
                if self.focus.is(PanelId::Editor) && self.mode == Mode::Insert && is_text_input(&k) {
                    if let KeyCode::Char(c) = k.code {
                        self.perform(Action::Editor(EditorAction::InsertChar(c)));
                    }
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
            Action::Git(g) if !matches!(g, G::Branches | G::NewBranch | G::Push | G::Pull | G::Fetch | G::Refresh) && !self.layout.is_visible(ToolWindowId::Git) => {
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

    /// Open (or switch) the palette. Candidates come from the caches (file
    /// index, branch list); fresh ones are requested and fed in as they load.
    pub fn open_palette(&mut self, kind: PaletteKind) {
        if kind == PaletteKind::GotoLine {
            self.palette = None;
            self.perform(Action::Editor(EditorAction::GotoLine));
            return;
        }
        if self.palette.as_ref().is_some_and(|p| p.kind == kind) {
            return;
        }
        // Switching kinds keeps the typed query.
        let query = self.palette.take().map(|p| p.input.text).unwrap_or_default();
        let mut p = Palette::new(kind, self.workspace.root.clone());
        p.set_recent(self.editor.recent_paths());
        if p.wants(Source::Files) {
            p.set_files(self.file_index.files.clone(), true);
            self.refresh_file_index();
        }
        if p.wants(Source::Actions) {
            p.set_actions(self.palette_actions());
        }
        if p.wants(Source::Branches) {
            p.set_branches(self.git_panel.branches.clone());
            self.git.load_branches();
        }
        if !query.is_empty() {
            p.set_query(&query);
        }
        self.palette = Some(p);
        self.dispatcher.reset();
    }

    /// Find Action rows: every registry entry except the palette's own
    /// navigation (and vim modes unless modal), with its shortcut.
    fn palette_actions(&self) -> Vec<ActionEntry> {
        crate::actions::REGISTRY
            .iter()
            .filter(|a| !matches!(a.action, Action::Palette(_)))
            .filter(|a| self.dispatcher.modal || !matches!(a.action, Action::Mode(_)))
            .filter(|a| cfg!(debug_assertions) || a.action != Action::App(AppAction::DebugPanic))
            .map(|a| ActionEntry { id: a.id, desc: a.desc, action: a.action.clone(), shortcut: self.keymap.shortcut_for(&a.action, a.action.contexts()) })
            .collect()
    }

    /// Walk the root for the palette in `spawn_blocking`; the result is
    /// tagged with the git generation and dropped after a root switch.
    pub fn refresh_file_index(&mut self) {
        self.file_index.loading = true;
        let (root, generation, tx) = (self.workspace.root.clone(), self.git.generation, self.bus_tx.clone());
        let (hidden, ignored) = (self.config.files.show_hidden, self.config.files.show_ignored);
        tokio::task::spawn_blocking(move || {
            let files = crate::ui::palette::walk_files(&root, hidden, ignored);
            let _ = tx.send(AppMsg::FileIndex { generation, files });
        });
    }

    fn file_index_loaded(&mut self, generation: u64, files: Vec<String>) {
        if generation != self.git.generation {
            tracing::debug!("dropping stale file index");
            return;
        }
        self.file_index = FileIndex { generation, files: std::sync::Arc::new(files), loading: false };
        if let Some(p) = self.palette.as_mut().filter(|p| p.wants(Source::Files)) {
            p.set_files(self.file_index.files.clone(), false);
        }
    }

    /// Keys while the palette is open: its own keymap context first, then
    /// global bindings (which close it, except those opening a palette).
    fn palette_key(&mut self, key: KeyEvent) {
        let r = self.dispatcher.dispatch(key, &["palette", "global"], None, &self.keymap);
        let Some(p) = &mut self.palette else { return };
        let res = match r {
            Dispatch::Action(Action::Palette(a)) => p.handle_action(&a),
            Dispatch::Action(Action::App(AppAction::OpenPalette(kind))) => {
                self.open_palette(kind);
                None
            }
            Dispatch::Action(a) => {
                self.palette = None;
                self.perform(a);
                None
            }
            Dispatch::Unmapped(k) => {
                p.handle_text_key(k);
                None
            }
            Dispatch::Pending | Dispatch::Ignore => None,
        };
        if let Some(res) = res {
            self.finish_palette(res);
        }
    }

    fn finish_palette(&mut self, res: PaletteResult) {
        self.palette = None;
        self.dirty_frame = true;
        match res {
            PaletteResult::Cancel => {}
            PaletteResult::Accept(Accept::OpenFile(p)) => self.open_file(p, true),
            PaletteResult::Accept(Accept::Action(a)) => self.perform(a),
            PaletteResult::Accept(Accept::SwitchBranch(name)) => self.switch_branch(name),
            PaletteResult::Accept(Accept::NewBranch) => self.git_request(GitRequest::NewBranch),
        }
    }

    fn switch_branch(&mut self, name: String) {
        if self.branch_label().as_deref() == Some(name.as_str()) {
            self.notify(Level::Info, format!("Already on {name}"));
        } else {
            self.git.run_cli(GitOp::Switch(name.clone()), vec!["switch".into(), name]);
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

    fn request_quit(&mut self) {
        if self.editor.has_dirty() {
            self.dialog = Some(Dialog::confirm(DialogId::QuitDirty, "Unsaved changes", "Some files have unsaved changes.", &["Save all and exit", "Discard and exit", "Cancel"]));
        } else {
            self.should_quit = true;
        }
    }

    fn finish_dialog(&mut self, id: DialogId, res: DialogResult) {
        self.dirty_frame = true;
        match (id, res) {
            (DialogId::QuitDirty, DialogResult::Button(0)) => {
                self.perform(Action::Editor(EditorAction::SaveAll));
                self.should_quit = true;
            }
            (DialogId::QuitDirty, DialogResult::Button(1)) => self.should_quit = true,
            (DialogId::CloseTabDirty(i), DialogResult::Button(0)) => {
                let mut cx = ctx!(self);
                if self.editor.save(i, &mut cx) {
                    self.editor.force_close(i);
                }
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::CloseTabDirty(i), DialogResult::Button(1)) => self.editor.force_close(i),
            (DialogId::CloseAllDirty, DialogResult::Button(0)) => {
                let mut cx = ctx!(self);
                if self.editor.save_all(&mut cx) {
                    self.editor.force_close_all();
                }
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::CloseAllDirty, DialogResult::Button(1)) => self.editor.force_close_all(),
            (DialogId::GotoLine, DialogResult::Text(t)) => {
                let mut it = t.trim().split(':');
                let line = it.next().and_then(|s| s.trim().parse::<usize>().ok());
                let col = it.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                match line {
                    Some(l) => self.editor.goto_line(l, col),
                    None => self.notify(Level::Warn, "Go to line: expected [line][:column]"),
                }
            }
            (DialogId::NewBranchName, DialogResult::Text(t)) => {
                let name = t.trim().to_string();
                if name.is_empty() || name.starts_with('-') || name.contains(char::is_whitespace) {
                    self.notify(Level::Warn, format!("Invalid branch name {name:?}"));
                } else {
                    self.git.run_cli(GitOp::NewBranch(name.clone()), vec!["switch".into(), "-c".into(), name]);
                }
            }
            (DialogId::PushSetUpstream(branch), DialogResult::Button(0)) => {
                self.git.run_cli(GitOp::PushUpstream(branch.clone()), vec!["push".into(), "-u".into(), "origin".into(), branch]);
            }
            (DialogId::SwitchRootDirty(root), DialogResult::Button(0)) => {
                let mut cx = ctx!(self);
                let saved = self.editor.save_all(&mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
                if saved {
                    self.do_switch_root(root);
                }
            }
            (DialogId::SwitchRootDirty(root), DialogResult::Button(1)) => {
                if !self.config.worktree.switch_closes_tabs {
                    self.close_dirty_tabs();
                }
                self.do_switch_root(root);
            }
            (DialogId::Info, _) => {}
            (_, DialogResult::Cancel) => {}
            (
                id @ (DialogId::WorktreeCreateKind
                | DialogId::WorktreeNewBranchName
                | DialogId::WorktreeExistingBranch(_)
                | DialogId::WorktreePath { .. }
                | DialogId::WorktreeSwitchAfterCreate(_)
                | DialogId::WorktreeRemove { .. }),
                res,
            ) => self.finish_worktree_dialog(id, res),
            (id, res) => tracing::debug!("dialog {id:?} -> {res:?} (no handler yet)"),
        }
    }

    pub fn apply_effects(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Notify(level, text) => self.notify(level, text),
                Effect::Focus(p) => {
                    if let Some(tw) = p.tool_window() {
                        self.layout.show(tw);
                    }
                    self.focus.set(p);
                }
                Effect::OpenFile(p) => self.open_file(p, true),
                Effect::Dialog(d) => self.dialog = Some(d),
                Effect::FileSaved { path: p, quiet } => {
                    if !quiet {
                        self.notify(Level::Info, format!("Saved {}", p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
                    }
                    self.project.refresh();
                    self.git.refresh_status();
                }
                Effect::Clipboard(text) => self.copy_to_clipboard(&text),
                Effect::SwitchRoot(p) => self.switch_root(p),
                Effect::GitRefresh => {
                    self.project.refresh();
                    self.git.refresh_status();
                    self.git.load_branches();
                    self.git.load_worktrees();
                }
                Effect::Git(req) => self.git_request(req),
                Effect::Quit => self.request_quit(),
            }
        }
    }

    /// Run a request a panel made through `Effect::Git`.
    fn git_request(&mut self, req: GitRequest) {
        use crate::git::repo;
        match req {
            GitRequest::Refresh => self.apply_effects(vec![Effect::GitRefresh]),
            GitRequest::Stage(rels) => {
                let label = format!("stage {}", rels.join(" "));
                self.git.run_repo(GitOp::Stage, label, move |root| repo::stage(root, &rels).map(|_| String::new()));
            }
            GitRequest::Unstage(rels) => {
                let label = format!("unstage {}", rels.join(" "));
                self.git.run_repo(GitOp::Unstage, label, move |root| repo::unstage(root, &rels).map(|_| String::new()));
            }
            GitRequest::StageAll => {
                self.git.run_repo(GitOp::Stage, "stage all".into(), |root| repo::stage_all(root).map(|_| String::new()));
            }
            GitRequest::UnstageAll => {
                self.git.run_repo(GitOp::Unstage, "unstage all".into(), |root| repo::unstage_all(root).map(|_| String::new()));
            }
            GitRequest::Commit { message, push } => {
                let subject = message.lines().next().unwrap_or("").to_string();
                self.git.run_repo(GitOp::Commit { push }, format!("commit: {subject}"), move |root| repo::commit(root, &message));
            }
            GitRequest::Push => {
                self.git.run_cli(GitOp::Push, vec!["push".into()]);
            }
            GitRequest::Pull => {
                let mode = match self.config.git.pull_mode.as_str() {
                    "rebase" => "--rebase",
                    "merge" => "--no-rebase",
                    _ => "--ff-only",
                };
                self.git.run_cli(GitOp::Pull, vec!["pull".into(), mode.into()]);
            }
            GitRequest::Fetch => {
                self.git.fetch();
            }
            GitRequest::BranchesPopup => self.open_palette(PaletteKind::Branches),
            GitRequest::NewBranch => {
                self.dialog = Some(Dialog::input(DialogId::NewBranchName, "New Branch", "Create and switch to a new branch from HEAD:", "", "Create"));
            }
            GitRequest::LoadDiff(key) => self.git.load_diff(key),
            GitRequest::LoadLog { skip } => self.git.load_log(skip, self.config.git.log_page, self.config.git.log_all),
            GitRequest::LoadCommit(sha) => self.git.load_commit(sha),
            GitRequest::LoadWorktrees => self.git.load_worktrees(),
            GitRequest::WorktreePrune => self.prune_worktrees(),
        }
    }

    fn on_git_msg(&mut self, msg: GitMsg) {
        if let GitMsg::OpDone { id, .. } = &msg {
            self.git.finish(*id);
        }
        if msg.generation() != self.git.generation {
            tracing::debug!("dropping stale git result");
            return;
        }
        match msg {
            GitMsg::Status { seq, result, .. } => {
                let prev_head = self.git_panel.snapshot().map(|s| s.head.clone());
                let mut cx = ctx!(self);
                let applied = self.git_panel.set_status(seq, result, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
                if !applied {
                    return;
                }
                let snap = self.git_panel.snapshot();
                self.project.set_git_status(snap.map(|s| s.tree.clone()).unwrap_or_default());
                // A new HEAD changes every gutter base.
                if snap.map(|s| s.head.clone()) != prev_head {
                    for p in self.editor.open_paths() {
                        self.git.load_blob(p, BlobWhich::Head);
                    }
                }
            }
            GitMsg::Blob { path, which: BlobWhich::Head, text, .. } => {
                self.editor.set_diff_base(&path, text);
                self.editor.update_gutters();
            }
            GitMsg::Blob { .. } => {}
            GitMsg::Diff { key, result, .. } => self.git_panel.set_diff(&key, result),
            GitMsg::Branches { result, .. } => {
                let worktree = std::mem::take(&mut self.worktrees.pending_existing);
                let palette = self.palette.as_mut().filter(|p| p.wants(Source::Branches));
                match result {
                    Ok(list) => {
                        if let Some(p) = palette {
                            p.set_branches(list.clone());
                        }
                        self.git_panel.branches = list;
                        if worktree {
                            self.open_existing_branch_dialog();
                        }
                    }
                    Err(e) if palette.is_some() || worktree => self.notify(Level::Error, format!("Branches: {e:#}")),
                    Err(_) => {}
                }
            }
            GitMsg::Log { skip, result, .. } => {
                let mut cx = ctx!(self);
                self.git_panel.set_log(skip, result, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            GitMsg::CommitDetails { sha, result, .. } => self.git_panel.set_details(&sha, result),
            GitMsg::OpDone { op, label, ok, output, .. } => self.git_op_done(op, label, ok, output),
        }
    }

    fn git_op_done(&mut self, op: GitOp, label: String, ok: bool, output: String) {
        // Staging is too chatty for the console unless it failed.
        if !ok || !matches!(op, GitOp::Stage | GitOp::Unstage) {
            self.git_panel.log_op(&label, ok, &output);
        }
        if self.worktree_op_done(&op, ok, &output) {
            // Reported (or a follow-up dialog opened) by the worktree flow.
        } else if ok {
            match &op {
                GitOp::Commit { push } => {
                    self.git_panel.message.clear();
                    self.notify(Level::Info, format!("Committed {output}"));
                    if *push {
                        self.git_request(GitRequest::Push);
                    }
                }
                GitOp::Push | GitOp::PushUpstream(_) => self.notify(Level::Info, "Pushed"),
                GitOp::Pull => self.notify(Level::Info, "Pulled"),
                GitOp::Fetch => self.notify(Level::Info, "Fetched"),
                GitOp::Switch(b) => self.notify(Level::Info, format!("Switched to {b}")),
                GitOp::NewBranch(b) => self.notify(Level::Info, format!("Created and switched to {b}")),
                GitOp::Stage | GitOp::Unstage => {}
                GitOp::WorktreeAdd { .. } | GitOp::WorktreeRemove { .. } | GitOp::WorktreePrune => {}
            }
            if op.touches_worktree() {
                let n = self.editor.reload_clean_from_disk();
                if n > 0 {
                    tracing::info!("reloaded {n} buffer(s) after {label}");
                }
            }
        } else if op == GitOp::Push && output.contains("has no upstream branch") {
            let branch = self.branch_label().unwrap_or_default();
            let msg = format!("Branch '{branch}' has no upstream.\nPush it and set the upstream to origin/{branch}?");
            self.dialog = Some(Dialog::confirm(DialogId::PushSetUpstream(branch), "Push", &msg, &["Push", "Cancel"]));
        } else if matches!(op, GitOp::Commit { .. } | GitOp::Stage | GitOp::Unstage) {
            self.notify(Level::Error, error_line(&output));
        } else {
            self.notify(Level::Error, format!("{label}: {}", error_line(&output)));
        }
        self.project.refresh();
        self.git.refresh_status();
        self.git.load_branches();
        // Branch switches and worktree operations both change the list.
        self.git.load_worktrees();
        if self.git_panel.log_loaded() && !matches!(op, GitOp::Stage | GitOp::Unstage) {
            let mut cx = ctx!(self);
            self.git_panel.request_log(&mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
        }
    }

    /// Point git at a new project root (worktree switch): bump the
    /// generation so in-flight results are dropped, forget the old state,
    /// and load the new one.
    pub fn git_root_changed(&mut self, root: PathBuf) {
        self.git.set_root(root);
        self.git_panel.reset();
        self.project.set_git_status(Default::default());
        for b in &mut self.editor.buffers {
            b.diff_base = None;
            b.gutter.clear();
        }
        self.git.refresh_status();
        self.git.load_branches();
        for p in self.editor.open_paths() {
            self.git.load_blob(p, BlobWhich::Head);
        }
        if self.git_panel.tab == crate::git::panel::GitTab::Log {
            let mut cx = ctx!(self);
            self.git_panel.request_log(&mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
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

    /// Open a file: reuse the tab when already open, otherwise read it off
    /// the main loop and finish in `file_loaded`.
    pub fn open_file(&mut self, path: PathBuf, focus: bool) {
        if let Some(i) = self.editor.index_of(&path) {
            let mut cx = ctx!(self);
            self.editor.activate(i, &mut cx);
            if focus {
                self.focus.set(PanelId::Editor);
            }
            return;
        }
        let (tx, generation) = (self.bus_tx.clone(), self.git.generation);
        tokio::task::spawn_blocking(move || {
            let result = std::fs::read(&path);
            let _ = tx.send(AppMsg::FileLoaded { generation, path, result, focus });
        });
    }

    fn file_loaded(&mut self, path: PathBuf, result: std::io::Result<Vec<u8>>, focus: bool) {
        let bytes = match result {
            Ok(b) => b,
            Err(e) => {
                self.notify(Level::Error, format!("open {}: {e}", path.display()));
                return;
            }
        };
        let Some((text, lossy)) = self.decode_file(&path, bytes) else { return };
        self.editor.open_text(path.clone(), &text, lossy, &self.theme);
        self.git.load_blob(path, BlobWhich::Head);
        if focus {
            self.focus.set(PanelId::Editor);
        }
    }

    /// File bytes as text and whether decoding was lossy; `None` (with a
    /// notification) for binary files.
    pub fn decode_file(&mut self, path: &std::path::Path, bytes: Vec<u8>) -> Option<(String, bool)> {
        let head = &bytes[..bytes.len().min(8192)];
        if head.contains(&0) {
            self.notify(Level::Warn, format!("{} looks binary; not opening", path.display()));
            return None;
        }
        let (text, lossy) = match String::from_utf8(bytes) {
            Ok(s) => (s, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
        if lossy {
            self.notify(Level::Warn, format!("{} is not valid UTF-8; shown with replacement characters", path.display()));
        }
        Some((text, lossy))
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

    fn resolve_clicks(&mut self, m: &MouseEvent) -> u8 {
        let MouseEventKind::Down(button) = m.kind else { return 0 };
        let now = Instant::now();
        let limit = Duration::from_millis(self.config.ui.double_click_ms);
        let count = match self.last_click {
            Some(lc) if lc.button == button && lc.x == m.column && lc.y == m.row && now.duration_since(lc.at) <= limit => (lc.count % 3) + 1,
            _ => 1,
        };
        self.last_click = Some(LastClick { at: now, x: m.column, y: m.row, button, count });
        count
    }

    fn handle_mouse(&mut self, m: MouseEvent) {
        if m.kind == MouseEventKind::Moved && self.drag.is_none() {
            return;
        }
        self.dirty_frame = true;
        let clicks = self.resolve_clicks(&m);
        let mouse = Mouse { kind: m.kind, x: m.column, y: m.row, clicks, mods: m.modifiers };
        // A click on a visible part of a toast closes it (the palette and
        // dialogs are drawn over toasts).
        let over_overlay = self.palette.as_ref().is_some_and(|p| p.contains(m.column, m.row)) || self.dialog.as_ref().is_some_and(|d| d.contains(m.column, m.row));
        if matches!(m.kind, MouseEventKind::Down(_)) && !over_overlay && self.toasts.dismiss_at(m.column, m.row) {
            self.drag = None;
            return;
        }

        if self.palette.is_some() && self.dialog.is_none() {
            if matches!(m.kind, MouseEventKind::Up(_)) {
                self.drag = None;
            }
            if let Some(res) = self.palette.as_mut().and_then(|p| p.handle_mouse(&mouse)) {
                self.finish_palette(res);
            }
            return;
        }
        if let Some(d) = &mut self.dialog {
            // A dialog that opened mid-drag (e.g. from a background result)
            // receives the release; end the drag too.
            if matches!(m.kind, MouseEventKind::Up(_)) {
                self.drag = None;
            }
            if let Some(res) = d.handle_mouse(&mouse) {
                let d = self.dialog.take().unwrap();
                self.finish_dialog(d.id, res);
            }
            return;
        }

        // A press while a drag is active means its release was lost.
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.drag = None;
        }
        // An active drag owns every event until the button is released.
        if let Some(drag) = self.drag {
            match m.kind {
                MouseEventKind::Up(_) => {
                    self.drag = None;
                    if let Drag::Panel(p, area) = drag {
                        self.forward_mouse(p, area, &mouse);
                    }
                }
                MouseEventKind::Drag(_) | MouseEventKind::Moved => match drag {
                    Drag::Splitter(id) => self.layout.drag_splitter(id, m.column, m.row),
                    Drag::Panel(p, area) => self.forward_mouse(p, area, &mouse),
                },
                _ => {}
            }
            return;
        }

        let hit = self.layout.hit_test(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(button) => match hit {
                Hit::Splitter(id) if button == MouseButton::Left => self.drag = Some(Drag::Splitter(id)),
                Hit::Splitter(_) => {}
                Hit::Strip => {
                    if let Some(tw) = self.layout.strip_hit(m.column, m.row) {
                        self.toggle_tool_window(tw);
                    }
                }
                Hit::Status => self.click_status(&mouse),
                Hit::Tabs(area) => {
                    self.focus.set(PanelId::Editor);
                    match self.editor.tab_hits.hit(m.column, m.row) {
                        Some(TabHit::Close(i)) => self.close_tab_index(i),
                        Some(TabHit::Tab(i)) if button == MouseButton::Middle => self.close_tab_index(i),
                        Some(TabHit::Tab(i)) => self.activate_tab(i),
                        None => {
                            let mut cx = ctx!(self);
                            self.editor.handle_tab_mouse(&mouse, area, &mut cx);
                            let fx = cx.effects;
                            self.apply_effects(fx);
                        }
                    }
                }
                Hit::Panel(p, area) => {
                    self.focus.set(p);
                    if button == MouseButton::Left {
                        self.drag = Some(Drag::Panel(p, area));
                    }
                    self.forward_mouse(p, area, &mouse);
                }
                Hit::None => {}
            },
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                if let Hit::Panel(p, area) = hit {
                    self.forward_mouse(p, area, &mouse);
                } else if let Hit::Tabs(area) = hit {
                    let mut cx = ctx!(self);
                    self.editor.handle_tab_mouse(&mouse, area, &mut cx);
                    let fx = cx.effects;
                    self.apply_effects(fx);
                }
            }
            _ => {}
        }
    }

    fn forward_mouse(&mut self, p: PanelId, area: Rect, m: &Mouse) {
        let mut cx = ctx!(self);
        let _ = match p {
            PanelId::Editor => self.editor.handle_mouse(m, area, &mut cx),
            PanelId::Project => self.project.handle_mouse(m, area, &mut cx),
            PanelId::Worktrees => self.worktrees.handle_mouse(m, area, &mut cx),
            PanelId::Git => self.git_panel.handle_mouse(m, area, &mut cx),
        };
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    fn click_status(&mut self, m: &Mouse) {
        let p = ratatui::layout::Position::new(m.x, m.y);
        let hit = self.status_hits.iter().find(|(_, r)| r.contains(p)).map(|(h, _)| *h);
        match hit {
            Some(StatusHit::Branch) => self.perform(Action::Git(crate::actions::GitAction::Branches)),
            Some(StatusHit::Position) => self.perform(Action::Editor(EditorAction::GotoLine)),
            Some(StatusHit::Notification) => {
                if let Some(n) = self.notifications.last().cloned() {
                    self.dialog = Some(Dialog::info("Notification", &n.text));
                }
            }
            None => {}
        }
    }

    fn activate_tab(&mut self, i: usize) {
        let mut cx = ctx!(self);
        self.editor.activate(i, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    fn close_tab_index(&mut self, i: usize) {
        let mut cx = ctx!(self);
        self.editor.close(i, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }
}

/// The most telling line of a failed command's output: the first `fatal:`
/// or `error:` line, else the first non-empty one.
fn error_line(output: &str) -> String {
    let lines = || output.lines().map(str::trim).filter(|l| !l.is_empty());
    lines()
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .or_else(|| lines().next())
        .unwrap_or("failed")
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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
        let old = app.git.generation;
        // The root switches while the read is in flight.
        app.git.set_root(app.workspace.root.clone());
        app.handle_msg(AppMsg::FileLoaded { generation: old, path: path.clone(), result: Ok(b"old".to_vec()), focus: true });
        assert!(app.editor.buffers.is_empty());
        let current = app.git.generation;
        app.handle_msg(AppMsg::FileLoaded { generation: current, path: path.clone(), result: Ok(b"new".to_vec()), focus: true });
        assert_eq!(app.editor.buffers.len(), 1);
        assert_eq!(app.editor.buffers[0].text(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
