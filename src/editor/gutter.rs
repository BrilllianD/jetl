//! Git diff marks shown in the editor gutter (computed in phase 3).

use ratatui::style::Color;

use crate::config::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GutterMark {
    pub line: usize,
    pub kind: MarkKind,
}

impl GutterMark {
    pub fn glyph(&self, theme: &Theme) -> (&'static str, Color) {
        match self.kind {
            MarkKind::Added => ("▎", theme.gutter_added),
            MarkKind::Modified => ("▎", theme.gutter_modified),
            MarkKind::Deleted => ("▸", theme.gutter_deleted),
        }
    }
}
