//! Editor: tabs + buffers (placeholder until phase 1).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::actions::EditorAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::tabs::TabHits;

#[derive(Debug, Default)]
pub struct Editor {
    pub tab_hits: TabHits,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn handle_action(&mut self, _a: &EditorAction, _cx: &mut Ctx) -> bool {
        false
    }
    pub fn handle_text(&mut self, _s: &str, _cx: &mut Ctx) {}
    pub fn handle_mouse(&mut self, _m: &Mouse, _area: Rect, _cx: &mut Ctx) -> bool {
        false
    }
    pub fn handle_tab_mouse(&mut self, _m: &Mouse, _area: Rect, _cx: &mut Ctx) -> bool {
        false
    }
    pub fn activate(&mut self, _i: usize, _cx: &mut Ctx) {}
    pub fn close(&mut self, _i: usize, _cx: &mut Ctx) {}
    pub fn status_position(&self) -> Option<String> {
        None
    }
    pub fn status_line_ending(&self) -> Option<String> {
        None
    }
    pub fn has_dirty(&self) -> bool {
        false
    }
    pub fn render(&mut self, frame: &mut Frame, tabs: Rect, area: Rect, focused: bool, theme: &Theme) {
        let buf = frame.buffer_mut();
        buf.set_style(tabs, theme.tab);
        buf.set_style(area, Style::default().bg(theme.bg));
        let hint = "No files open. Alt+1 opens the Project view; Ctrl+Shift+N finds a file.";
        if area.height > 2 {
            let x = area.x + area.width.saturating_sub(hint.len() as u16) / 2;
            buf.set_stringn(x, area.y + area.height / 2, hint, area.width as usize, if focused { theme.dim } else { theme.dim });
        }
    }
}
