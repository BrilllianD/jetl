//! Mouse routing: overlays, drags, splitters, tabs, status bar, panels.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::{App, Ctx, Drag, LastClick};
use crate::actions::{Action, EditorAction};
use crate::event::Mouse;
use crate::ui::focus::PanelId;
use crate::ui::layout::{Hit, StripButton};
use crate::ui::status_bar::StatusHit;
use crate::ui::tabs::TabHit;

impl App {
    pub(super) fn resolve_clicks(&mut self, m: &MouseEvent) -> u8 {
        let MouseEventKind::Down(button) = m.kind else { return 0 };
        let now = Instant::now();
        let limit = Duration::from_millis(self.config.ui.double_click_ms);
        let count = match self.last_click {
            Some(lc) if lc.button == button && lc.x == m.column && lc.y == m.row && now.duration_since(lc.at) <= limit => {
                (lc.count % 3) + 1
            }
            _ => 1,
        };
        self.last_click = Some(LastClick { at: now, x: m.column, y: m.row, button, count });
        count
    }

    pub(super) fn handle_mouse(&mut self, m: MouseEvent) {
        // Motion matters to drags and to the menus' hover highlight; other
        // motion only moves the strip tooltip, redrawn when it changes.
        if m.kind == MouseEventKind::Moved && self.drag.is_none() && self.menu.is_none() && self.menu_bar.is_none() {
            let hover = if self.palette.is_none() && self.dialog.is_none() { self.layout.strip_hit(m.column, m.row) } else { None };
            if hover != self.strip_hover {
                self.strip_hover = hover;
                self.dirty_frame = true;
            }
            return;
        }
        self.dirty_frame = true;
        self.strip_hover = None;
        let clicks = self.resolve_clicks(&m);
        let mouse = Mouse { kind: m.kind, x: m.column, y: m.row, clicks, mods: m.modifiers };
        // A click on a visible part of a toast closes it (the palette and
        // dialogs are drawn over toasts).
        let over_overlay = self.palette.as_ref().is_some_and(|p| p.contains(m.column, m.row))
            || self.dialog.as_ref().is_some_and(|d| d.contains(m.column, m.row))
            || self.menu.as_ref().is_some_and(|c| c.contains(m.column, m.row))
            || self.menu_bar.as_ref().is_some_and(|b| b.contains(m.column, m.row));
        if matches!(m.kind, MouseEventKind::Down(_)) && !over_overlay && self.toasts.dismiss_at(m.column, m.row) {
            self.drag = None;
            return;
        }

        // An accepted completion waiting for its resolve goes in first.
        self.completion_flush();
        // The completion popup: the wheel moves its selection, a click on a
        // row accepts it, a click anywhere else closes it (and does what it
        // does there).
        if let Some(c) = &mut self.completion {
            let on = c.contains(m.column, m.row);
            match m.kind {
                MouseEventKind::ScrollUp if on => return c.scroll_by(-1),
                MouseEventKind::ScrollDown if on => return c.scroll_by(1),
                MouseEventKind::Down(MouseButton::Left) if on => {
                    if let Some(row) = c.row_at(m.column, m.row) {
                        c.state.selected = row;
                        self.completion_accept();
                    }
                    return;
                }
                MouseEventKind::Down(_) if on => return,
                MouseEventKind::Down(_) => self.completion_close(),
                _ => {}
            }
        }
        // The hover popup scrolls under the wheel; a click anywhere closes
        // it (and does what it does there, unless it was on the popup).
        if let Some(h) = &mut self.hover {
            let on = h.contains(m.column, m.row);
            match m.kind {
                MouseEventKind::ScrollUp if on => return h.scroll_by(-3),
                MouseEventKind::ScrollDown if on => return h.scroll_by(3),
                MouseEventKind::Down(_) => {
                    self.hover = None;
                    if on {
                        return;
                    }
                }
                _ => {}
            }
        }
        if self.palette.is_some() && self.dialog.is_none() && self.menu.is_none() && self.menu_bar.is_none() {
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
        if self.menu_bar.is_some() {
            self.main_menu_mouse(&m, &mouse);
            return;
        }
        if let Some(menu) = &mut self.menu {
            if matches!(m.kind, MouseEventKind::Up(_)) {
                self.drag = None;
            }
            match menu.handle_mouse(&mouse) {
                Some(crate::ui::menu::MenuResult::Run(a)) => {
                    self.menu = None;
                    self.perform(a);
                }
                Some(crate::ui::menu::MenuResult::Cancel) => self.menu = None,
                None => {}
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
                    Drag::Tab(id) => self.editor.drag_tab(id, m.column),
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
                Hit::Strip => match self.layout.strip_hit(m.column, m.row) {
                    Some((StripButton::Menu, _)) => self.toggle_main_menu(),
                    Some((StripButton::Tool(tw), _)) => self.toggle_tool_window(tw),
                    None => {}
                },
                Hit::Status => self.click_status(&mouse),
                Hit::Tabs(area) => {
                    self.focus.set(PanelId::Editor);
                    match self.editor.tab_hits.hit(m.column, m.row) {
                        // A pinned tab's close button is its pin.
                        Some(TabHit::Close(i)) if self.editor.is_pinned(i) => {
                            self.editor.set_pinned(i, false);
                        }
                        Some(TabHit::Close(i)) => self.close_tab_index(i),
                        Some(TabHit::Tab(i)) if button == MouseButton::Middle => self.close_tab_index(i),
                        Some(TabHit::Tab(i)) if button == MouseButton::Right => {
                            self.activate_tab(i);
                            let mut cx = ctx!(self);
                            self.editor.tab_menu(m.column, m.row, &mut cx);
                            let fx = cx.effects;
                            self.apply_effects(fx);
                        }
                        // The press activates; dragging before the release
                        // moves the tab.
                        Some(TabHit::Tab(i)) => {
                            self.activate_tab(i);
                            self.drag = self.editor.buffers.get(i).map(|b| Drag::Tab(b.id));
                        }
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

    pub(super) fn forward_mouse(&mut self, p: PanelId, area: Rect, m: &Mouse) {
        let mut cx = ctx!(self);
        let _ = match p {
            PanelId::Editor => self.editor.handle_mouse(m, area, &mut cx),
            PanelId::Project => self.project.handle_mouse(m, area, &mut cx),
            PanelId::Worktrees => self.worktrees.handle_mouse(m, area, &mut cx),
            PanelId::Git => self.git_panel.handle_mouse(m, area, &mut cx),
            PanelId::Find => self.find.handle_mouse(m, area, &mut cx),
            PanelId::Structure => self.structure.handle_mouse(m, area, &mut cx),
            PanelId::EventLog => self.event_log.handle_mouse(m, &self.notifications, &mut cx),
            PanelId::Run => self.run_panel.handle_mouse(m, &mut cx),
            PanelId::Terminal => self.terminal.handle_mouse(m),
            PanelId::Problems => self.problems.handle_mouse(m, &self.lsp.diagnostics, &mut cx),
        };
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    pub(super) fn click_status(&mut self, m: &Mouse) {
        let p = ratatui::layout::Position::new(m.x, m.y);
        let hit = self.status_hits.iter().find(|(_, r)| r.contains(p)).map(|(h, _)| *h);
        match hit {
            Some(StatusHit::Branch) => self.perform(Action::Git(crate::actions::GitAction::Branches)),
            Some(StatusHit::Position) => self.perform(Action::Editor(EditorAction::GotoLine)),
            Some(StatusHit::LineEnding) => self.dialog = self.editor.line_ending_dialog(),
            Some(StatusHit::Language) => self.perform(Action::Editor(EditorAction::SetLanguage)),
            Some(StatusHit::Indent) => self.dialog = self.editor.indent_dialog(),
            // Shown for information; other encodings come later.
            Some(StatusHit::Encoding) => {}
            // The status bar shows the newest entry.
            Some(StatusHit::Notification) if !self.notifications.is_empty() => self.open_event_log(self.notifications.len() - 1),
            Some(StatusHit::Notification) => {}
            Some(StatusHit::Problems) => self.show_problems(),
            None => {}
        }
    }

    pub(super) fn activate_tab(&mut self, i: usize) {
        let mut cx = ctx!(self);
        self.editor.activate(i, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }

    pub(super) fn close_tab_index(&mut self, i: usize) {
        let mut cx = ctx!(self);
        self.editor.close(i, &mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }
}
