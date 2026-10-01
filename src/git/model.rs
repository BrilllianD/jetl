//! Git data types shared between the service, panels and the tree.

use ratatui::style::Style;

use crate::config::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileStatus {
    Ignored,
    Untracked,
    Added,
    Modified,
    Deleted,
    Renamed,
    Conflicted,
}

impl FileStatus {
    pub fn style(self, theme: &Theme) -> Style {
        match self {
            FileStatus::Ignored => theme.git_ignored,
            FileStatus::Untracked => theme.git_untracked,
            FileStatus::Added => theme.git_added,
            FileStatus::Modified | FileStatus::Renamed => theme.git_modified,
            FileStatus::Deleted => theme.git_deleted,
            FileStatus::Conflicted => theme.git_conflicted,
        }
    }
}
