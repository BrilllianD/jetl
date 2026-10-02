//! Panel identities and focus tracking.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PanelId {
    Project,
    Worktrees,
    Editor,
    Git,
    Find,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolWindowId {
    Project,
    Worktrees,
    Git,
    Find,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Bottom,
}

impl ToolWindowId {
    pub const ALL: [ToolWindowId; 4] = [ToolWindowId::Project, ToolWindowId::Worktrees, ToolWindowId::Find, ToolWindowId::Git];

    pub fn panel(self) -> PanelId {
        match self {
            ToolWindowId::Project => PanelId::Project,
            ToolWindowId::Worktrees => PanelId::Worktrees,
            ToolWindowId::Git => PanelId::Git,
            ToolWindowId::Find => PanelId::Find,
        }
    }
    pub fn side(self) -> Side {
        match self {
            ToolWindowId::Project | ToolWindowId::Worktrees => Side::Left,
            ToolWindowId::Git | ToolWindowId::Find => Side::Bottom,
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            ToolWindowId::Project => "Project",
            ToolWindowId::Worktrees => "Worktrees",
            ToolWindowId::Git => "Git",
            ToolWindowId::Find => "Find",
        }
    }
    pub fn number(self) -> u8 {
        match self {
            ToolWindowId::Project => 1,
            ToolWindowId::Worktrees => 2,
            ToolWindowId::Git => 9,
            ToolWindowId::Find => 3,
        }
    }
}

impl PanelId {
    pub fn tool_window(self) -> Option<ToolWindowId> {
        match self {
            PanelId::Project => Some(ToolWindowId::Project),
            PanelId::Worktrees => Some(ToolWindowId::Worktrees),
            PanelId::Git => Some(ToolWindowId::Git),
            PanelId::Find => Some(ToolWindowId::Find),
            PanelId::Editor => None,
        }
    }
    /// Keymap context name for this panel.
    pub fn context(self) -> &'static str {
        match self {
            PanelId::Project => "tree",
            PanelId::Worktrees => "worktrees",
            PanelId::Editor => "editor",
            PanelId::Git => "git",
            PanelId::Find => "find",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Focus {
    pub current: PanelId,
    pub prev: PanelId,
}

impl Focus {
    pub fn new() -> Self {
        Self { current: PanelId::Editor, prev: PanelId::Editor }
    }
    pub fn set(&mut self, p: PanelId) {
        if p != self.current {
            self.prev = self.current;
            self.current = p;
        }
    }
    pub fn is(&self, p: PanelId) -> bool {
        self.current == p
    }
}

impl Default for Focus {
    fn default() -> Self {
        Self::new()
    }
}
