//! Panel identities and focus tracking.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PanelId {
    Project,
    Worktrees,
    Editor,
    Git,
    Find,
    Structure,
    EventLog,
    Run,
}

/// Saved in sessions: new variants go last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolWindowId {
    Project,
    Worktrees,
    Git,
    Find,
    Structure,
    EventLog,
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Bottom,
}

impl ToolWindowId {
    pub const ALL: [ToolWindowId; 7] = [
        ToolWindowId::Project,
        ToolWindowId::Worktrees,
        ToolWindowId::Structure,
        ToolWindowId::Find,
        ToolWindowId::Run,
        ToolWindowId::Git,
        ToolWindowId::EventLog,
    ];

    pub fn panel(self) -> PanelId {
        match self {
            ToolWindowId::Project => PanelId::Project,
            ToolWindowId::Worktrees => PanelId::Worktrees,
            ToolWindowId::Git => PanelId::Git,
            ToolWindowId::Find => PanelId::Find,
            ToolWindowId::Structure => PanelId::Structure,
            ToolWindowId::EventLog => PanelId::EventLog,
            ToolWindowId::Run => PanelId::Run,
        }
    }
    pub fn side(self) -> Side {
        match self {
            ToolWindowId::Project | ToolWindowId::Worktrees | ToolWindowId::Structure => Side::Left,
            ToolWindowId::Git | ToolWindowId::Find | ToolWindowId::EventLog | ToolWindowId::Run => Side::Bottom,
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            ToolWindowId::Project => "Project",
            ToolWindowId::Worktrees => "Worktrees",
            ToolWindowId::Git => "Git",
            ToolWindowId::Find => "Find",
            ToolWindowId::Structure => "Structure",
            ToolWindowId::EventLog => "Event Log",
            ToolWindowId::Run => "Run",
        }
    }
    /// The Alt+number key shown on the tool strip; windows opened by other
    /// keys have none.
    pub fn number(self) -> Option<u8> {
        match self {
            ToolWindowId::Project => Some(1),
            ToolWindowId::Worktrees => Some(2),
            ToolWindowId::Git => Some(9),
            ToolWindowId::Find => Some(3),
            ToolWindowId::Structure => Some(7),
            ToolWindowId::EventLog => None,
            ToolWindowId::Run => Some(4),
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
            PanelId::Structure => Some(ToolWindowId::Structure),
            PanelId::EventLog => Some(ToolWindowId::EventLog),
            PanelId::Run => Some(ToolWindowId::Run),
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
            PanelId::Structure => "structure",
            PanelId::EventLog => "event_log",
            PanelId::Run => "run",
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
