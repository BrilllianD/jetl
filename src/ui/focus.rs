//! Panel identities and focus tracking.

use serde::{Deserialize, Serialize};

use crate::config::Icons;

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
    Terminal,
    Problems,
    LocalHistory,
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
    Terminal,
    Problems,
    LocalHistory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Bottom,
}

impl ToolWindowId {
    /// Strip order: left windows from the top, bottom ones from the bottom.
    pub const ALL: [ToolWindowId; 10] = [
        ToolWindowId::Project,
        ToolWindowId::Worktrees,
        ToolWindowId::Structure,
        ToolWindowId::Find,
        ToolWindowId::Run,
        ToolWindowId::Problems,
        ToolWindowId::Terminal,
        ToolWindowId::Git,
        ToolWindowId::EventLog,
        ToolWindowId::LocalHistory,
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
            ToolWindowId::Terminal => PanelId::Terminal,
            ToolWindowId::Problems => PanelId::Problems,
            ToolWindowId::LocalHistory => PanelId::LocalHistory,
        }
    }
    pub fn side(self) -> Side {
        match self {
            ToolWindowId::Project | ToolWindowId::Worktrees | ToolWindowId::Structure => Side::Left,
            ToolWindowId::Git
            | ToolWindowId::Find
            | ToolWindowId::EventLog
            | ToolWindowId::Run
            | ToolWindowId::Terminal
            | ToolWindowId::Problems
            | ToolWindowId::LocalHistory => Side::Bottom,
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
            ToolWindowId::Terminal => "Terminal",
            ToolWindowId::Problems => "Problems",
            ToolWindowId::LocalHistory => "Local History",
        }
    }
    /// The tool strip glyph: a two-cell emoji, or a symbol one cell wide
    /// everywhere, East Asian ambiguous width included.
    pub fn icon(self, icons: Icons) -> &'static str {
        if icons == Icons::Emoji {
            return match self {
                ToolWindowId::Project => "📁",
                ToolWindowId::Worktrees => "🌿",
                ToolWindowId::Git => "🔀",
                ToolWindowId::Find => "🔍",
                ToolWindowId::Structure => "🧩",
                ToolWindowId::EventLog => "🔔",
                ToolWindowId::Run => "🚀",
                ToolWindowId::Terminal => "💻",
                ToolWindowId::Problems => "⚠️",
                ToolWindowId::LocalHistory => "🕘",
            };
        }
        match self {
            ToolWindowId::Project => "⌂",
            ToolWindowId::Worktrees => "◫",
            ToolWindowId::Git => "⎇",
            ToolWindowId::Find => "⌕",
            ToolWindowId::Structure => "⊢",
            ToolWindowId::EventLog => "⚑",
            ToolWindowId::Run => "►",
            ToolWindowId::Terminal => "❯",
            ToolWindowId::Problems => "⚠",
            ToolWindowId::LocalHistory => "⟲",
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
            ToolWindowId::Terminal => None,
            ToolWindowId::Problems => Some(6),
            ToolWindowId::LocalHistory => None,
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
            PanelId::Terminal => Some(ToolWindowId::Terminal),
            PanelId::Problems => Some(ToolWindowId::Problems),
            PanelId::LocalHistory => Some(ToolWindowId::LocalHistory),
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
            PanelId::Terminal => "terminal",
            PanelId::Problems => "problems",
            PanelId::LocalHistory => "local_history",
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

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn icons_have_one_width_and_are_distinct() {
        for (set, w) in [(Icons::Unicode, 1), (Icons::Emoji, 2)] {
            for tw in ToolWindowId::ALL {
                let icon = tw.icon(set);
                assert_eq!((icon.width(), icon.width_cjk()), (w, w), "{tw:?} {set:?}");
            }
            let mut icons: Vec<&str> = ToolWindowId::ALL.iter().map(|tw| tw.icon(set)).collect();
            icons.sort();
            icons.dedup();
            assert_eq!(icons.len(), ToolWindowId::ALL.len(), "{set:?}");
        }
    }
}
