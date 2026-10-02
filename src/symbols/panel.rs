//! Structure tool window: the active buffer's symbols from its live syntax
//! tree, indented by nesting. Re-extracted when the buffer's version, its
//! language or the active tab changes (checked every frame, cheaply); the
//! selection follows the caret while the window is not focused. Enter or a
//! click jumps to the symbol.

use std::path::PathBuf;

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{Symbol, extract};
use crate::actions::StructureAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::editor::buffer::{Buffer, BufferId};
use crate::event::{Effect, Mouse};
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;

#[derive(Debug, Default)]
pub struct StructurePanel {
    /// Buffer, version and language the symbols were extracted for.
    key: Option<(BufferId, u64, &'static str)>,
    pub symbols: Vec<Symbol>,
    path: Option<PathBuf>,
    /// Shown instead of an empty list.
    empty: String,
    pub state: ListState,
    list_rect: Rect,
    /// Buffer and caret line the selection last followed.
    followed: Option<(BufferId, usize)>,
}

impl StructurePanel {
    /// Bring the list up to date with the active buffer (`None`: no tab).
    /// Cheap unless the buffer changed; `follow` moves the selection to the
    /// symbol around the caret when the caret moved.
    pub fn sync(&mut self, buffer: Option<&mut Buffer>, follow: bool) {
        let Some(b) = buffer else {
            if self.key.is_some() || self.empty.is_empty() {
                *self = Self { state: std::mem::take(&mut self.state), ..Self::default() };
                self.empty = "No file open".into();
                self.state.set_len(0);
            }
            return;
        };
        let lang = b.syntax.as_ref().map(|s| s.def.name).unwrap_or("");
        let key = (b.id, b.version, lang);
        if self.key != Some(key) {
            let same_buffer = self.key.is_some_and(|k| k.0 == b.id);
            self.symbols = match &mut b.syntax {
                Some(syn) if syn.def.tags_query().is_some() => {
                    syn.reparse(&b.rope);
                    syn.tree().map(|t| extract(syn.def, t, &b.rope)).unwrap_or_default()
                }
                _ => Vec::new(),
            };
            self.empty = match &b.syntax {
                None => "No structure for plain text".into(),
                Some(s) if s.def.tags.is_none() => format!("No structure for {}", s.def.name),
                Some(_) => "No symbols".into(),
            };
            self.path = b.path.clone();
            self.key = Some(key);
            self.state.set_len(self.symbols.len());
            if !same_buffer {
                self.state.select(0);
                self.state.offset = 0;
                self.followed = None;
            }
        }
        if follow {
            let line = b.cursor_line_col().0;
            if self.followed != Some((b.id, line)) {
                self.followed = Some((b.id, line));
                if let Some(i) = self.symbols.iter().rposition(|s| s.start_line <= line && line <= s.end_line) {
                    self.state.select(i);
                }
            }
        }
    }

    pub fn selected(&self) -> Option<&Symbol> {
        self.symbols.get(self.state.selected)
    }

    pub fn handle_action(&mut self, a: &StructureAction, cx: &mut Ctx) -> bool {
        let page = self.state.viewport.max(1);
        match a {
            StructureAction::Up => self.state.up(1),
            StructureAction::Down => self.state.down(1),
            StructureAction::PageUp => self.state.up(page),
            StructureAction::PageDown => self.state.down(page),
            StructureAction::Home => self.state.home(),
            StructureAction::End => self.state.end(),
            StructureAction::Open => self.jump(cx),
        }
        true
    }

    fn jump(&self, cx: &mut Ctx) {
        if let (Some(s), Some(path)) = (self.selected(), &self.path) {
            cx.effects.push(Effect::OpenFileAt { path: path.clone(), line: s.line, col: s.col });
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(i) = self.state.hit(self.list_rect, m.x, m.y) else { return false };
                self.state.select(i);
                self.jump(cx);
                true
            }
            MouseEventKind::ScrollUp => {
                self.state.scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.state.scroll(3);
                true
            }
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let block = tool_window_block(" Structure ", focused, theme);
        let r = block.inner(area);
        frame.render_widget(block, area);
        self.list_rect = r;
        self.state.set_viewport(r.height as usize);
        if r.height == 0 || r.width < 4 {
            return;
        }
        let buf = frame.buffer_mut();
        if self.symbols.is_empty() {
            buf.set_stringn(r.x + 1, r.y, &self.empty, r.width.saturating_sub(1) as usize, theme.dim);
            return;
        }
        for (vi, i) in self.state.visible().enumerate() {
            let s = &self.symbols[i];
            let y = r.y + vi as u16;
            let selected = i == self.state.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let right = r.right();
            let x = r.x + 1 + (2 * s.depth as u16).min(r.width / 2);
            if x >= right {
                continue;
            }
            let (nx, _) = buf.set_stringn(x, y, &*s.name, (right - x) as usize, base.add_modifier(Modifier::BOLD));
            let kind = format!(" {}", s.kind);
            if nx + (kind.width() as u16) <= right {
                buf.set_stringn(nx, y, &kind, (right - nx) as usize, if selected { base } else { theme.dim });
            }
        }
    }
}
