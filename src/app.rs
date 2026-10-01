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
use crate::config::keymap::{Keymap, default_keymap};
use crate::config::theme::Theme;
use crate::config::{Config, Paths};
use crate::dispatch::{Dispatch, Dispatcher, Mode, is_text_input};
use crate::editor::Editor;
use crate::event::{AppMsg, Effect, Level, Mouse};
use crate::files::ProjectPanel;
use crate::git::GitPanel;
use crate::tui::Term;
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::focus::{Focus, PanelId, ToolWindowId};
use crate::ui::layout::{Hit, LayoutState, SplitterId};
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
    pub dialog: Option<Dialog>,
    pub status_hits: Vec<(StatusHit, Rect)>,
    pub keyboard_enhanced: bool,
    pub project: ProjectPanel,
    pub worktrees: WorktreePanel,
    pub git_panel: GitPanel,
    pub editor: Editor,
}

impl App {
    pub fn new(root: PathBuf, config: Config, paths: Paths) -> Self {
        let (bus_tx, bus_rx) = mpsc::unbounded_channel();
        let mut keymap = default_keymap();
        if let Ok(text) = std::fs::read_to_string(paths.keymap_file()) {
            if let Err(e) = keymap.merge_toml(&text) {
                tracing::error!("user keymap invalid: {e}");
            }
        }
        let modal = config.keymap.mode == "vim";
        let mut dispatcher = Dispatcher::new(modal);
        dispatcher.double_tap = Duration::from_millis(config.ui.double_click_ms);
        let mode = if modal { Mode::Normal } else { Mode::Insert };
        let layout = LayoutState::new(config.ui.left_width, config.ui.bottom_height);
        let root_for_panels = root.clone();
        let mut app = Self {
            workspace: Workspace::new(root),
            theme: Theme::default(),
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
            dialog: None,
            status_hits: Vec::new(),
            keyboard_enhanced: crate::tui::keyboard_enhanced(),
            project: ProjectPanel::new(root_for_panels.clone(), config.files.show_hidden, config.files.show_ignored),
            worktrees: WorktreePanel::new(),
            git_panel: GitPanel::new(),
            editor: Editor::new(&config),
            config,
            paths,
        };
        if !app.keyboard_enhanced {
            app.notify(Level::Warn, "Terminal lacks the kitty keyboard protocol: Ctrl+Shift shortcuts, Ctrl+Tab and Shift Shift are unavailable");
        }
        app
    }


    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        match level {
            Level::Error => tracing::error!("{text}"),
            Level::Warn => tracing::warn!("{text}"),
            Level::Info => tracing::info!("{text}"),
        }
        self.notifications.push(Notification { level, text, at: Instant::now() });
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

    pub fn branch_label(&self) -> Option<String> {
        None
    }

    /// Keymap context stack, innermost first.
    fn contexts(&self) -> Vec<&'static str> {
        let mut v = Vec::with_capacity(3);
        if self.dialog.is_some() {
            v.push("dialog");
        } else {
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
    }

    fn handle_msg(&mut self, msg: AppMsg) {
        self.dirty_frame = true;
        match msg {
            AppMsg::Notify { level, text } => self.notify(level, text),
            AppMsg::SwitchRoot(p) => self.apply_effects(vec![Effect::SwitchRoot(p)]),
            AppMsg::FileLoaded { path, result, focus } => self.file_loaded(path, result, focus),
            AppMsg::FileSaved { .. } => {}
            AppMsg::Git(_) | AppMsg::Worktrees(_) => {}
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
            Event::FocusLost => {}
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
        let contexts = self.contexts();
        let r = self.dispatcher.dispatch(key, &contexts, self.mode, &self.keymap);
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
        // Git and worktree actions make their tool window visible when invoked
        // from elsewhere (e.g. Ctrl+K from the editor).
        match &action {
            Action::Git(_) if !self.layout.is_visible(ToolWindowId::Git) => {
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

    fn open_palette(&mut self, kind: PaletteKind) {
        self.notify(Level::Info, format!("{kind:?} palette arrives in phase 5"));
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
            (DialogId::Info, _) => {}
            (_, DialogResult::Cancel) => {}
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
                Effect::FileSaved(p) => {
                    self.notify(Level::Info, format!("Saved {}", p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
                    self.project.refresh();
                }
                Effect::Clipboard(text) => self.copy_to_clipboard(&text),
                Effect::SwitchRoot(p) => self.notify(Level::Info, format!("switch root to {} (phase 4)", p.display())),
                Effect::GitRefresh => {}
                Effect::Quit => self.request_quit(),
            }
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
        let tx = self.bus_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = std::fs::read(&path);
            let _ = tx.send(AppMsg::FileLoaded { path, result, focus });
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
        let head = &bytes[..bytes.len().min(8192)];
        if head.contains(&0) {
            self.notify(Level::Warn, format!("{} looks binary; not opening", path.display()));
            return;
        }
        let (text, lossy) = match String::from_utf8(bytes) {
            Ok(s) => (s, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
        if lossy {
            self.notify(Level::Warn, format!("{} is not valid UTF-8; shown with replacement characters", path.display()));
        }
        self.editor.open_text(path, &text, lossy);
        if focus {
            self.focus.set(PanelId::Editor);
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

        if let Some(d) = &mut self.dialog {
            if let Some(res) = d.handle_mouse(&mouse) {
                let d = self.dialog.take().unwrap();
                self.finish_dialog(d.id, res);
            }
            return;
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
