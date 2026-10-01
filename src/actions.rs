//! Every user-triggerable operation is an `Action`. Keymaps and the command
//! palette refer to actions by their string id through the registry.

use std::sync::LazyLock;

use crate::ui::focus::{PanelId, ToolWindowId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    App(AppAction),
    Editor(EditorAction),
    Tree(TreeAction),
    Git(GitAction),
    Worktree(WorktreeAction),
    Palette(PaletteAction),
    Mode(ModeAction),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppAction {
    Quit,
    FocusEditor,
    FocusPanel(PanelId),
    ToggleToolWindow(ToolWindowId),
    HideAllToolWindows,
    HideFocusedToolWindow,
    Escape,
    OpenPalette(PaletteKind),
    DebugPanic,
    Refresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaletteKind {
    Files,
    Actions,
    Everywhere,
    RecentFiles,
    GotoLine,
    Branches,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
    DocStart,
    DocEnd,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EditorAction {
    InsertChar(char),
    InsertText(String),
    InsertNewline,
    InsertTab,
    Backspace,
    Delete,
    DeleteWordBack,
    Move(Motion, bool),
    Undo,
    Redo,
    Save,
    SaveAll,
    Find,
    Replace,
    FindNext,
    FindPrev,
    ReplaceOne,
    ReplaceAll,
    FindToggleCase,
    FindToggleWord,
    FindToggleRegex,
    FindToggleField,
    GotoLine,
    DuplicateLine,
    DeleteLine,
    SelectAll,
    SelectLine,
    Copy,
    Cut,
    Paste,
    NextTab,
    PrevTab,
    CloseTab,
    CloseAllTabs,
    Switcher,
    Escape,
    ToggleComment,
    ScrollUp(usize),
    ScrollDown(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TreeAction {
    Up,
    Down,
    Expand,
    Collapse,
    Toggle,
    Open,
    Refresh,
    SelectInProject,
    Parent,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitAction {
    Refresh,
    ToggleStage,
    StageAll,
    UnstageAll,
    Commit,
    CommitAndPush,
    CommitBox,
    Push,
    Pull,
    Fetch,
    Branches,
    NewBranch,
    TabCommit,
    TabLog,
    TabConsole,
    Up,
    Down,
    PageUp,
    PageDown,
    Left,
    Right,
    OpenFile,
    FocusMessage,
    NextSection,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorktreeAction {
    Refresh,
    New,
    Remove,
    Switch,
    Prune,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PaletteAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Accept,
    Cancel,
    Backspace,
    DeleteWord,
    NextTab,
    PrevTab,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModeAction {
    Normal,
    Insert,
    Visual,
}

pub struct ActionInfo {
    pub id: &'static str,
    pub desc: &'static str,
    pub action: Action,
}

macro_rules! reg {
    ($($id:literal => $desc:literal, $a:expr;)*) => {
        vec![$(ActionInfo { id: $id, desc: $desc, action: $a }),*]
    };
}

pub static REGISTRY: LazyLock<Vec<ActionInfo>> = LazyLock::new(|| {
    use Action::*;
    use EditorAction as E;
    use Motion as M;
    reg! {
        "app.quit" => "Exit jetl", App(AppAction::Quit);
        "app.focus_editor" => "Focus editor", App(AppAction::FocusEditor);
        "app.escape" => "Escape", App(AppAction::Escape);
        "app.refresh" => "Refresh project and git state", App(AppAction::Refresh);
        "app.debug_panic" => "Debug: trigger a panic", App(AppAction::DebugPanic);
        "tool_window.project" => "Project tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Project));
        "tool_window.worktrees" => "Worktrees tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Worktrees));
        "tool_window.git" => "Git tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Git));
        "tool_window.hide_all" => "Hide all tool windows", App(AppAction::HideAllToolWindows);
        "tool_window.hide_focused" => "Hide active tool window", App(AppAction::HideFocusedToolWindow);
        "palette.files" => "Go to file", App(AppAction::OpenPalette(PaletteKind::Files));
        "palette.actions" => "Find action", App(AppAction::OpenPalette(PaletteKind::Actions));
        "palette.search_everywhere" => "Search everywhere", App(AppAction::OpenPalette(PaletteKind::Everywhere));
        "palette.recent_files" => "Recent files", App(AppAction::OpenPalette(PaletteKind::RecentFiles));
        "palette.goto_line" => "Go to line", App(AppAction::OpenPalette(PaletteKind::GotoLine));

        "editor.newline" => "Insert newline", Editor(E::InsertNewline);
        "editor.tab" => "Insert tab", Editor(E::InsertTab);
        "editor.backspace" => "Delete left", Editor(E::Backspace);
        "editor.delete" => "Delete right", Editor(E::Delete);
        "editor.delete_word_back" => "Delete word left", Editor(E::DeleteWordBack);
        "editor.left" => "Move left", Editor(E::Move(M::Left, false));
        "editor.right" => "Move right", Editor(E::Move(M::Right, false));
        "editor.up" => "Move up", Editor(E::Move(M::Up, false));
        "editor.down" => "Move down", Editor(E::Move(M::Down, false));
        "editor.word_left" => "Move word left", Editor(E::Move(M::WordLeft, false));
        "editor.word_right" => "Move word right", Editor(E::Move(M::WordRight, false));
        "editor.line_start" => "Line start", Editor(E::Move(M::LineStart, false));
        "editor.line_end" => "Line end", Editor(E::Move(M::LineEnd, false));
        "editor.page_up" => "Page up", Editor(E::Move(M::PageUp, false));
        "editor.page_down" => "Page down", Editor(E::Move(M::PageDown, false));
        "editor.doc_start" => "Document start", Editor(E::Move(M::DocStart, false));
        "editor.doc_end" => "Document end", Editor(E::Move(M::DocEnd, false));
        "editor.left_extend" => "Select left", Editor(E::Move(M::Left, true));
        "editor.right_extend" => "Select right", Editor(E::Move(M::Right, true));
        "editor.up_extend" => "Select up", Editor(E::Move(M::Up, true));
        "editor.down_extend" => "Select down", Editor(E::Move(M::Down, true));
        "editor.word_left_extend" => "Select word left", Editor(E::Move(M::WordLeft, true));
        "editor.word_right_extend" => "Select word right", Editor(E::Move(M::WordRight, true));
        "editor.line_start_extend" => "Select to line start", Editor(E::Move(M::LineStart, true));
        "editor.line_end_extend" => "Select to line end", Editor(E::Move(M::LineEnd, true));
        "editor.page_up_extend" => "Select page up", Editor(E::Move(M::PageUp, true));
        "editor.page_down_extend" => "Select page down", Editor(E::Move(M::PageDown, true));
        "editor.doc_start_extend" => "Select to document start", Editor(E::Move(M::DocStart, true));
        "editor.doc_end_extend" => "Select to document end", Editor(E::Move(M::DocEnd, true));
        "editor.undo" => "Undo", Editor(E::Undo);
        "editor.redo" => "Redo", Editor(E::Redo);
        "editor.save" => "Save", Editor(E::Save);
        "editor.save_all" => "Save all", Editor(E::SaveAll);
        "editor.find" => "Find", Editor(E::Find);
        "editor.replace" => "Replace", Editor(E::Replace);
        "editor.find_next" => "Find next", Editor(E::FindNext);
        "editor.find_prev" => "Find previous", Editor(E::FindPrev);
        "editor.replace_one" => "Replace current match", Editor(E::ReplaceOne);
        "editor.replace_all" => "Replace all matches", Editor(E::ReplaceAll);
        "editor.find_case" => "Find: toggle match case", Editor(E::FindToggleCase);
        "editor.find_word" => "Find: toggle whole words", Editor(E::FindToggleWord);
        "editor.find_regex" => "Find: toggle regex", Editor(E::FindToggleRegex);
        "editor.find_field" => "Find: switch between find and replace fields", Editor(E::FindToggleField);
        "editor.goto_line" => "Go to line", Editor(E::GotoLine);
        "editor.duplicate_line" => "Duplicate line", Editor(E::DuplicateLine);
        "editor.delete_line" => "Delete line", Editor(E::DeleteLine);
        "editor.select_all" => "Select all", Editor(E::SelectAll);
        "editor.select_line" => "Select line", Editor(E::SelectLine);
        "editor.copy" => "Copy", Editor(E::Copy);
        "editor.cut" => "Cut", Editor(E::Cut);
        "editor.paste" => "Paste", Editor(E::Paste);
        "editor.next_tab" => "Next tab", Editor(E::NextTab);
        "editor.prev_tab" => "Previous tab", Editor(E::PrevTab);
        "editor.close_tab" => "Close tab", Editor(E::CloseTab);
        "editor.close_all_tabs" => "Close all tabs", Editor(E::CloseAllTabs);
        "editor.switcher" => "Switcher (recent tabs)", Editor(E::Switcher);
        "editor.escape" => "Escape (close find, clear selection)", Editor(E::Escape);
        "editor.toggle_comment" => "Comment with line comment", Editor(E::ToggleComment);
        "editor.scroll_up" => "Scroll up", Editor(E::ScrollUp(1));
        "editor.scroll_down" => "Scroll down", Editor(E::ScrollDown(1));

        "tree.up" => "Tree: up", Tree(TreeAction::Up);
        "tree.down" => "Tree: down", Tree(TreeAction::Down);
        "tree.expand" => "Tree: expand", Tree(TreeAction::Expand);
        "tree.collapse" => "Tree: collapse", Tree(TreeAction::Collapse);
        "tree.toggle" => "Tree: toggle", Tree(TreeAction::Toggle);
        "tree.open" => "Tree: open", Tree(TreeAction::Open);
        "tree.parent" => "Tree: go to parent", Tree(TreeAction::Parent);
        "tree.refresh" => "Tree: refresh", Tree(TreeAction::Refresh);
        "tree.select_in_project" => "Select in Project", Tree(TreeAction::SelectInProject);
        "tree.page_up" => "Tree: page up", Tree(TreeAction::PageUp);
        "tree.page_down" => "Tree: page down", Tree(TreeAction::PageDown);
        "tree.home" => "Tree: first", Tree(TreeAction::Home);
        "tree.end" => "Tree: last", Tree(TreeAction::End);

        "git.refresh" => "Git: refresh", Git(GitAction::Refresh);
        "git.toggle_stage" => "Git: stage/unstage selected", Git(GitAction::ToggleStage);
        "git.stage_all" => "Git: stage all", Git(GitAction::StageAll);
        "git.unstage_all" => "Git: unstage all", Git(GitAction::UnstageAll);
        "git.commit" => "Git: commit", Git(GitAction::Commit);
        "git.commit_and_push" => "Git: commit and push", Git(GitAction::CommitAndPush);
        "git.commit_box" => "Git: open commit window", Git(GitAction::CommitBox);
        "git.push" => "Git: push", Git(GitAction::Push);
        "git.pull" => "Git: pull (update project)", Git(GitAction::Pull);
        "git.fetch" => "Git: fetch", Git(GitAction::Fetch);
        "git.branches_popup" => "Git: branches", Git(GitAction::Branches);
        "git.new_branch" => "Git: new branch", Git(GitAction::NewBranch);
        "git.tab_commit" => "Git: Commit tab", Git(GitAction::TabCommit);
        "git.tab_log" => "Git: Log tab", Git(GitAction::TabLog);
        "git.tab_console" => "Git: Console tab", Git(GitAction::TabConsole);
        "git.up" => "Git: up", Git(GitAction::Up);
        "git.down" => "Git: down", Git(GitAction::Down);
        "git.page_up" => "Git: page up", Git(GitAction::PageUp);
        "git.page_down" => "Git: page down", Git(GitAction::PageDown);
        "git.left" => "Git: left", Git(GitAction::Left);
        "git.right" => "Git: right", Git(GitAction::Right);
        "git.open_file" => "Git: open selected file", Git(GitAction::OpenFile);
        "git.focus_message" => "Git: focus commit message", Git(GitAction::FocusMessage);
        "git.next_section" => "Git: next pane", Git(GitAction::NextSection);
        "git.escape" => "Git: back (close diff, leave message, return to editor)", Git(GitAction::Escape);

        "worktree.refresh" => "Worktrees: refresh", Worktree(WorktreeAction::Refresh);
        "worktree.new" => "Worktrees: new", Worktree(WorktreeAction::New);
        "worktree.remove" => "Worktrees: remove", Worktree(WorktreeAction::Remove);
        "worktree.switch" => "Worktrees: switch", Worktree(WorktreeAction::Switch);
        "worktree.prune" => "Worktrees: prune", Worktree(WorktreeAction::Prune);
        "worktree.up" => "Worktrees: up", Worktree(WorktreeAction::Up);
        "worktree.down" => "Worktrees: down", Worktree(WorktreeAction::Down);

        "palette.up" => "Palette: up", Palette(PaletteAction::Up);
        "palette.down" => "Palette: down", Palette(PaletteAction::Down);
        "palette.page_up" => "Palette: page up", Palette(PaletteAction::PageUp);
        "palette.page_down" => "Palette: page down", Palette(PaletteAction::PageDown);
        "palette.accept" => "Palette: accept", Palette(PaletteAction::Accept);
        "palette.cancel" => "Palette: cancel", Palette(PaletteAction::Cancel);
        "palette.backspace" => "Palette: backspace", Palette(PaletteAction::Backspace);
        "palette.delete_word" => "Palette: delete word", Palette(PaletteAction::DeleteWord);
        "palette.next_tab" => "Palette: next tab", Palette(PaletteAction::NextTab);
        "palette.prev_tab" => "Palette: previous tab", Palette(PaletteAction::PrevTab);

        "mode.normal" => "Vim: normal mode", Mode(ModeAction::Normal);
        "mode.insert" => "Vim: insert mode", Mode(ModeAction::Insert);
        "mode.visual" => "Vim: visual mode", Mode(ModeAction::Visual);
    }
});

impl Action {
    pub fn from_id(id: &str) -> Option<Action> {
        REGISTRY.iter().find(|a| a.id == id).map(|a| a.action.clone())
    }

    pub fn id(&self) -> Option<&'static str> {
        REGISTRY.iter().find(|a| &a.action == self).map(|a| a.id)
    }

    /// Keymap contexts where this action is normally bound, most specific
    /// first; used to look up the shortcut shown next to it.
    pub fn contexts(&self) -> &'static [&'static str] {
        match self {
            Action::App(_) => &["global"],
            Action::Editor(_) => &["editor", "global"],
            Action::Tree(_) => &["tree", "global"],
            Action::Git(_) => &["git", "global"],
            Action::Worktree(_) => &["worktrees", "global"],
            Action::Palette(_) => &["palette"],
            Action::Mode(_) => &["mode.normal", "mode.insert", "mode.visual"],
        }
    }
}
