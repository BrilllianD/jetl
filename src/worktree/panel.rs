//! Worktrees tool window (placeholder until phase 4).

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::actions::WorktreeAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::tool_window_block;

#[derive(Debug, Default)]
pub struct WorktreePanel {}

impl WorktreePanel {
    pub fn new() -> Self {
        Self {}
    }
    pub fn handle_action(&mut self, _a: &WorktreeAction, _cx: &mut Ctx) -> bool {
        false
    }
    pub fn handle_mouse(&mut self, _m: &Mouse, _area: Rect, _cx: &mut Ctx) -> bool {
        false
    }
    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let block = tool_window_block(" Worktrees ", focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.buffer_mut().set_stringn(inner.x + 1, inner.y, "(worktrees in phase 4)", inner.width as usize, theme.dim);
    }
}
