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
    Tasks,
    Todo,
    Hierarchy,
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
    Tasks,
    Todo,
    Hierarchy,
}

/// Saved in sessions: new variants go last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Left,
    Bottom,
    Right,
}

impl ToolWindowId {
    /// Strip order: left windows from the top, bottom ones from the bottom.
    pub const ALL: [ToolWindowId; 13] = [
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
        ToolWindowId::Tasks,
        ToolWindowId::Todo,
        ToolWindowId::Hierarchy,
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
            ToolWindowId::Tasks => PanelId::Tasks,
            ToolWindowId::Todo => PanelId::Todo,
            ToolWindowId::Hierarchy => PanelId::Hierarchy,
        }
    }
    /// The side before the user moves it; `LayoutState::side_of` is the
    /// effective one.
    pub fn default_side(self) -> Side {
        match self {
            ToolWindowId::Project | ToolWindowId::Worktrees | ToolWindowId::Structure => Side::Left,
            ToolWindowId::Git
            | ToolWindowId::Find
            | ToolWindowId::EventLog
            | ToolWindowId::Run
            | ToolWindowId::Terminal
            | ToolWindowId::Problems
            | ToolWindowId::LocalHistory
            | ToolWindowId::Tasks
            | ToolWindowId::Todo
            | ToolWindowId::Hierarchy => Side::Bottom,
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
            ToolWindowId::Tasks => "Tasks",
            ToolWindowId::Todo => "TODO",
            ToolWindowId::Hierarchy => "Hierarchy",
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
                ToolWindowId::Tasks => "📋",
                ToolWindowId::Todo => "📝",
                ToolWindowId::Hierarchy => "🌳",
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
            ToolWindowId::Tasks => "✓",
            ToolWindowId::Todo => "✎",
            ToolWindowId::Hierarchy => "⋔",
        }
    }
    /// The Alt+number key shown on the tool strip; Terminal (Alt+F12), Tasks, TODO and Hierarchy have none.
    pub fn number(self) -> Option<u8> {
        match self {
            ToolWindowId::Project => Some(1),
            ToolWindowId::Worktrees => Some(2),
            ToolWindowId::Git => Some(9),
            ToolWindowId::Find => Some(3),
            ToolWindowId::Structure => Some(7),
            ToolWindowId::EventLog => Some(5),
            ToolWindowId::Run => Some(4),
            ToolWindowId::Terminal => None,
            ToolWindowId::Problems => Some(6),
            ToolWindowId::LocalHistory => Some(8),
            ToolWindowId::Tasks | ToolWindowId::Todo | ToolWindowId::Hierarchy => None,
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
            PanelId::Tasks => Some(ToolWindowId::Tasks),
            PanelId::Todo => Some(ToolWindowId::Todo),
            PanelId::Hierarchy => Some(ToolWindowId::Hierarchy),
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
            PanelId::Tasks => "tasks",
            PanelId::Todo => "todo",
            PanelId::Hierarchy => "hierarchy",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Focus {
    pub current: PanelId,
    pub prev: PanelId,
    /// The tool window that last had focus (F12 goes back to it).
    pub last_tool: Option<ToolWindowId>,
    /// Counts `set` calls, a call that keeps the panel included;
    /// `LayoutState::keep_unpinned` compares it between frames.
    pub moves: u64,
}

impl Focus {
    pub fn new() -> Self {
        Self { current: PanelId::Editor, prev: PanelId::Editor, last_tool: None, moves: 0 }
    }
    pub fn set(&mut self, p: PanelId) {
        self.moves = self.moves.wrapping_add(1);
        if let Some(tw) = p.tool_window() {
            self.last_tool = Some(tw);
        }
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
    fn set_counts_every_call() {
        let mut f = Focus::new();
        f.set(PanelId::Editor);
        f.set(PanelId::Editor);
        assert_eq!((f.moves, f.current), (2, PanelId::Editor));
    }

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

    #[test]
    fn numbers_are_distinct() {
        let mut nums: Vec<u8> = ToolWindowId::ALL.iter().filter_map(|tw| tw.number()).collect();
        let n = nums.len();
        nums.sort();
        nums.dedup();
        assert_eq!(nums.len(), n);
        assert_eq!(ToolWindowId::EventLog.number(), Some(5));
        assert_eq!(ToolWindowId::LocalHistory.number(), Some(8));
        assert_eq!(ToolWindowId::Terminal.number(), None);
    }

    #[test]
    fn set_records_the_last_tool_window() {
        let mut f = Focus::new();
        assert_eq!(f.last_tool, None);
        f.set(PanelId::Git);
        f.set(PanelId::Editor);
        assert_eq!(f.last_tool, Some(ToolWindowId::Git));
        f.set(PanelId::Project);
        assert_eq!(f.last_tool, Some(ToolWindowId::Project));
    }
}
