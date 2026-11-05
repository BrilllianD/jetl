//! Every user-triggerable operation is an `Action`. Keymaps and the command
//! palette refer to actions by their string id through the registry.

use std::sync::LazyLock;

use crate::lsp::Refactoring;
use crate::ui::focus::{Dock, PanelId, Side, ToolWindowId};
use crate::ui::layout::ViewMode;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    App(AppAction),
    Editor(EditorAction),
    Tree(TreeAction),
    Git(GitAction),
    Worktree(WorktreeAction),
    Find(FindAction),
    Structure(StructureAction),
    EventLog(EventLogAction),
    Run(RunAction),
    Debug(DebugAction),
    Problems(ProblemsAction),
    History(HistoryAction),
    Tasks(TasksAction),
    Todo(TodoAction),
    Hierarchy(HierarchyAction),
    Lsp(LspAction),
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
    /// Every tool window back on its default side, default strip order, nothing maximized (Shift+F12).
    RestoreDefaultLayout,
    /// Pick a saved tool window layout and apply it.
    SwitchLayout,
    /// Save the tool window placement under a new name.
    SaveLayoutAsNew,
    /// Overwrite the current named layout with the tool window placement.
    SaveLayoutChanges,
    /// Rename a saved tool window layout.
    RenameLayout,
    /// Delete a saved tool window layout.
    DeleteLayout,
    /// Move the focused tool window's free edge (Ctrl+Shift+arrows).
    ResizeToolWindow(Dir),
    /// The focused tool window's next inner tab (Git, Run, Terminal), wrapping.
    NextToolWindowTab,
    /// The focused tool window's previous inner tab, wrapping.
    PrevToolWindowTab,
    /// Make the focused tool window fill the content area, or restore it.
    MaximizeToolWindow,
    /// Focus the tool window that last had focus, showing it first (F12).
    LastToolWindow,
    /// Toggle a tool window between Pinned and Unpinned: the given one, or
    /// with `None` the focused one (View menu, palette).
    TogglePinned {
        tw: Option<ToolWindowId>,
    },
    /// Set a tool window's view mode (the side menu's View Mode rows; no key).
    SetViewMode {
        tw: ToolWindowId,
        mode: ViewMode,
    },
    /// Toggle a tool window between Undocked and Pinned: the given one, or
    /// with `None` the focused one (View menu, palette).
    ToggleUndocked {
        tw: Option<ToolWindowId>,
    },
    /// Put a tool window in a dock: the given one, or with `None` the
    /// focused one (View menu, palette).
    MoveToolWindow {
        tw: Option<ToolWindowId>,
        dock: Dock,
    },
    /// Swap a tool window's strip button with its neighbour above or below
    /// (the side menu; no key).
    MoveToolWindowButton {
        tw: ToolWindowId,
        up: bool,
    },
    /// Show only the editor text; again to put the layout back.
    ToggleZen,
    /// Show or hide the toolbar row: flips `[ui] toolbar` in `config.toml`.
    ToggleToolbar,
    Escape,
    OpenPalette(PaletteKind),
    /// Recent Locations: the caret history in the Locations palette, newest first.
    RecentLocations,
    DebugPanic,
    Refresh,
    /// Open the main menu bar, or close it when open.
    MainMenu,
    /// Show the version.
    About,
    /// Open `config.toml`, creating it when missing.
    OpenSettings,
    /// The Settings window.
    Settings,
    /// Open the project's `docs/KEYMAP.md`, else point at the online copy.
    KeymapReference,
    /// Ask for a language, then create and open a scratch file.
    NewScratch,
    /// The Local History window on the active file or the selected tree
    /// entry (a folder: its files with history; no file: the project's).
    LocalHistory,
    /// Put a local history label on the active file or the selected tree entry.
    PutLabel,
    /// Terminal window: a new tab with another shell (`terminal.new_tab`).
    NewTerminalTab,
    /// Terminal window: end the active tab's shell and close the tab (`terminal.close_tab`).
    CloseTerminalTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaletteKind {
    Files,
    Actions,
    Everywhere,
    RecentFiles,
    GotoLine,
    Branches,
    RecentProjects,
    Symbols,
    /// The Run task picker.
    RunTasks,
    /// Places a language server answered with (definitions, usages); opened
    /// by the app with its rows (`App::show_locations`), not by an action.
    Locations,
    /// The scratch files (`scratch.rs`), newest first.
    Scratches,
    /// Go to File's list; the accepted file is compared with the active tab (`editor.compare_with_file`).
    CompareWith,
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
    Unindent,
    Backspace,
    Delete,
    DeleteWordBack,
    DeleteWordForward,
    Move(Motion, bool),
    Undo,
    Redo,
    Save,
    SaveAs,
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
    /// Close every unpinned tab but the active one.
    CloseOtherTabs,
    /// Close every unpinned tab.
    CloseUnpinnedTabs,
    TogglePinTab,
    MoveTabLeft,
    MoveTabRight,
    Switcher,
    /// The Switcher with the selection starting on the least recent file.
    SwitcherBack,
    /// Split the focused tab group: a new group to the right or below
    /// shows the same tab.
    SplitRight,
    SplitDown,
    /// Focus the next (previous) tab group.
    NextGroup,
    PrevGroup,
    /// Focus the tab group left of (right of, above, below) the focused
    /// one; nothing when there is none.
    FocusGroupLeft,
    FocusGroupRight,
    FocusGroupUp,
    FocusGroupDown,
    /// Merge the focused group's tabs into the group beside it.
    Unsplit,
    UnsplitAll,
    /// Move the active tab to the group beside (splitting right first when
    /// there is one group).
    MoveTabToOppositeGroup,
    Escape,
    /// Select a completion snippet's next tabstop (`editor.snippet`); past
    /// the last one, the snippet's end, which finishes it.
    NextTabstop,
    /// Select a completion snippet's previous tabstop.
    PrevTabstop,
    /// Expand the live template whose prefix is the word before the caret (`editor.snippet_prefix`).
    ExpandSnippet,
    /// Wrap the selection (else the caret line) with a live template that uses `$TM_SELECTED_TEXT`, picked from a dialog.
    SurroundWith,
    /// Diff the clipboard against the active tab (or its selection) in a patch tab.
    CompareWithClipboard,
    /// Pick a file (Compare With palette) and diff it against the active tab.
    CompareWithFile,
    /// Open or focus a read-only, rendered preview of the active Markdown file.
    MarkdownPreview,
    ToggleComment,
    ToggleBlockComment,
    MoveLineUp,
    MoveLineDown,
    JoinLines,
    ToggleCase,
    /// Uppercase the selections, or the word at each caret.
    ToUpperCase,
    /// Lowercase the selections, or the word at each caret.
    ToLowerCase,
    /// Sort the selected lines, else the whole file.
    SortLines,
    /// Reverse the selected lines, else the whole file.
    ReverseLines,
    GotoBracket,
    ExpandSelection,
    ShrinkSelection,
    AddNextOccurrence,
    UnselectOccurrence,
    SelectAllOccurrences,
    CloneCaretAbove,
    CloneCaretBelow,
    ToggleColumnMode,
    ToggleOverwrite,
    ToggleSoftWrap,
    /// Fold the innermost region at each caret not folded yet.
    FoldCollapse,
    /// Unfold the region folded on each caret's line.
    FoldExpand,
    FoldCollapseRecursive,
    FoldExpandRecursive,
    FoldCollapseAll,
    FoldExpandAll,
    /// Fold the selected lines; without a selection, unfold.
    FoldSelection,
    ScrollUp(usize),
    ScrollDown(usize),
    NavBack,
    NavForward,
    ToggleBookmark,
    ShowBookmarks,
    /// Ctrl+F11: choose a digit for the caret line's bookmark.
    ToggleNumberedBookmark,
    /// Ctrl+0..9: jump to the bookmark with this digit.
    GotoNumberedBookmark(u8),
    /// Ctrl+F8: toggle a breakpoint on every caret's line.
    ToggleBreakpoint,
    LineEndingLf,
    LineEndingCrlf,
    SetLanguage,
    /// Put the HEAD text of the change at the caret back.
    RevertChange,
    NextChange,
    PrevChange,
    /// Show the next file of a diff tab.
    DiffNextFile,
    /// Show the previous file of a diff tab.
    DiffPrevFile,
    /// Show or hide the git blame column.
    ToggleBlame,
    /// Reformat the buffer with its language's external formatter.
    Format,
    /// The conflict resolver's commands (`editor.merge` context).
    Merge(MergeAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeAction {
    /// Caret to the next (previous) unresolved change in the focused pane.
    NextConflict,
    PrevConflict,
    /// Yours (theirs) for the change at the caret; after the other side,
    /// both.
    AcceptLeft,
    AcceptRight,
    /// Leave the change at the caret out.
    Ignore,
    ApplyNonConflicting,
    /// Yours (theirs) for the whole file.
    AcceptAllLeft,
    AcceptAllRight,
    /// Write the result, stage it and close the resolver.
    Apply,
    /// Close the resolver without writing (Esc does the editor's Esc first
    /// while that has something to do).
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TreeAction {
    /// New file or directory (asks which).
    New,
    NewFile,
    NewDir,
    Rename,
    Delete,
    CopyPath,
    CopyRelPath,
    ToggleHidden,
    ToggleIgnored,
    ContextMenu,
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
    Discard,
    NextHunk,
    PrevHunk,
    ToggleDiffLayout,
    ToggleDiffCollapse,
    /// History of the active file or the selected tree entry.
    FileHistory,
    /// Pick a conflicted file, then merge it or take one side.
    ResolveConflicts,
    /// Continue the rebase, merge, cherry-pick or revert in progress.
    ContinueOperation,
    /// Skip the commit the rebase, cherry-pick or revert stopped on.
    SkipOperation,
    /// Abort the operation in progress.
    AbortOperation,
    LogFilter,
    ContextMenu,
    CopySha,
    LogShowDiff,
    LogShowPatch,
    /// The selected file's diff in an editor tab; in the Log list the selected commits.
    ShowDiff,
    LogCheckout,
    LogNewBranch,
    LogCherryPick,
    LogRevert,
    LogReset,
    /// Load the todo of the commits from the selected one to HEAD into the Interactive Rebase dialog.
    LogInteractiveRebase,
    LogNewTag,
    LogDeleteTag,
    /// One operation of a branch (a context menu row; not in the keymap).
    BranchOp {
        branch: String,
        op: crate::git::model::BranchOp,
    },
    /// One action on the selected node of the Log details' file tree (a context menu row; not in the keymap).
    LogFileOp(crate::git::model::LogFileOp),
    Commit,
    CommitAndPush,
    CommitBox,
    ToggleAmend,
    MessageOlder,
    MessageNewer,
    LineUp,
    LineDown,
    LineExtendUp,
    LineExtendDown,
    Push,
    PushOptions,
    Pull,
    Fetch,
    Branches,
    NewBranch,
    TabCommit,
    TabLog,
    TabConsole,
    TabStash,
    StashPush,
    StashIncludeUntracked,
    StashPop,
    StashApply,
    StashDrop,
    Up,
    Down,
    ExtendUp,
    ExtendDown,
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
pub enum FindAction {
    /// Open the Find window with the editor selection as the query.
    InFiles,
    ReplaceInFiles,
    NextResult,
    PrevResult,
    Up,
    Down,
    PageUp,
    PageDown,
    Open,
    NextField,
    ToggleCase,
    ToggleWord,
    ToggleRegex,
    ToggleStructural,
    ToggleReplace,
    ReplaceAll,
    ToggleExclude,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StructureAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Jump to the selected symbol.
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventLogAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Show the selected entry's full text.
    Details,
    Copy,
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RunAction {
    /// Run the selected configuration; the picker when none.
    RunSelected,
    /// The dropdown of run configurations, anchored under the cell `x`, `y`
    /// (the toolbar widget, the Run window's picker).
    ConfigMenu {
        x: u16,
        y: u16,
    },
    /// Make the named configuration the selected one without running it
    /// (the dropdown's rows).
    Select(String),
    /// Open the Run Configurations window.
    EditConfigs,
    Rerun,
    /// Rerun only the failed tests of the last run.
    RerunFailed,
    /// Run the test around the caret.
    TestAtCaret,
    /// Send the HTTP request around the caret with curl.
    HttpRequestAtCaret,
    /// Run [index] scip_command, then load the SCIP index again.
    BuildScipIndex,
    Stop,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Switch between the output and the test tree.
    ToggleView,
    /// Expand / collapse the selected tree row.
    Expand,
    Collapse,
    /// Open the selected line's location.
    Open,
    NextProblem,
    PrevProblem,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DebugAction {
    /// Start the selected configuration under its debug adapter, replacing a live session.
    Start,
    /// Stop the session; pressed again, skip ahead a stage.
    Stop,
    /// Let the paused program run on.
    Resume,
    /// Run the paused program to the next line of the same frame.
    StepOver,
    /// Run the paused program to the next line, entering a call.
    StepInto,
    /// Run the paused program until its frame returns.
    StepOut,
    /// Pause the running program.
    Pause,
    /// Open the file at the line the program is paused on.
    ShowExecutionPoint,
    // The Debug tool window's Console list.
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Open the selected Console line's location.
    Open,
}

/// The Problems tool window (language server diagnostics).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProblemsAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Open the selected problem's location.
    Open,
    /// Copy the selected problem's message.
    Copy,
    /// Show only the active file's problems, or all again.
    ToggleCurrentFile,
}

/// The TODO tool window (`files/todo.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TodoAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Open,
    Refresh,
}

/// The Hierarchy tool window (`ui/hierarchy.rs`, `app/hierarchy.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HierarchyAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Expand,
    Collapse,
    Open,
    ToggleDirection,
    Refresh,
    /// Write the loaded tree to a new scratch file as a Mermaid diagram.
    ExportMermaid,
    /// Write the loaded tree to a new scratch file as a DOT graph.
    ExportDot,
    /// Open the Hierarchy window on the active file's imports (symbol index).
    FileDependencies,
}

/// The Tasks tool window (`yman/panel.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TasksAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Switch between the list and the details.
    NextSection,
    /// Focus the details.
    Details,
    ToggleClosed,
    FilterTag,
    FilterAssignee,
    /// Choose a related task of the selected one.
    JumpRelated,
    /// Change the selected task's status (start, done, cancel, reopen, move), with an optional note.
    ChangeStatus,
    /// Set the selected task's priority (0-9).
    ChangePriority,
    /// Set or clear the selected task's assignee.
    Assign,
    /// Edit the selected task's tags.
    EditTags,
    /// Add a comment to the selected task.
    Comment,
    /// Create a task with the New Task dialog.
    NewTask,
    Refresh,
    /// Run `yman sync`: fetch, merge and push the task history.
    SyncTracker,
    ContextMenu,
    /// Clear the typed filter, leave the details, or focus the editor.
    Escape,
}

/// The Local History tool window (`history/panel.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HistoryAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Back to the snapshot list.
    Left,
    /// To the diff.
    Right,
    /// Between the list and the diff.
    NextSection,
    /// Open the file in the editor (in the project view: show the selected
    /// file's history, or open or close a label).
    OpenFile,
    /// The project view: the files with history (from a file's: back).
    Files,
    NextChange,
    PrevChange,
    ToggleDiffLayout,
    ToggleDiffCollapse,
    /// List the snapshots again.
    Refresh,
    /// Put the selected snapshot back into the file (undoable).
    Revert,
    /// Put the selected changes of the diff back (undoable).
    RevertChanges,
    /// In the diff: the change above or below joins the selection (in the
    /// list: up or down).
    ExtendUp,
    ExtendDown,
    /// Select every change of the diff.
    SelectAllChanges,
    /// In the diff: the line cursor moves to the changed row below or above
    /// (in the list: down or up); the extend ones grow the selected lines.
    LineUp,
    LineDown,
    LineExtendUp,
    LineExtendDown,
    /// Put a label on the file shown.
    PutLabel,
    /// Remove the selected label (asks first).
    RemoveLabel,
    ContextMenu,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LspAction {
    /// Stop every language server; they start again for the open files.
    Restart,
    /// Move the caret to the next diagnostic in the file (wrapping).
    NextProblem,
    PrevProblem,
    /// Ctrl+B: go to the definition at the caret; on the definition
    /// itself, its usages.
    GotoDefinition,
    /// Ctrl+Shift+B: go to the type declaration of the symbol at the caret.
    GotoTypeDefinition,
    /// Ctrl+Alt+B: go to the implementations of the symbol at the caret.
    GotoImplementation,
    /// Go to the declaration (not the definition) of the symbol at the caret.
    GotoDeclaration,
    /// Alt+F7: the usages of what is at the caret.
    FindUsages,
    /// Alt+Q: the hover documentation at the caret in a popup.
    QuickDoc,
    /// Ctrl+Shift+I / Alt+Shift+I: the definition at the caret in a popup.
    QuickDefinition,
    /// Ctrl+Alt+H: the callers and callees of the function at the caret in the Hierarchy window.
    CallHierarchy,
    /// Ctrl+Alt+Shift+H: the supertypes and subtypes of the type at the caret in the Hierarchy window.
    TypeHierarchy,
    /// Ctrl+P: the signatures of the call at the caret, its active parameter bold.
    ParameterInfo,
    /// Ctrl+Space: code completion at the caret.
    Complete,
    /// Shift+F6: rename the symbol at the caret and its usages.
    Rename,
    /// Alt+Enter: quick fixes and refactorings at the caret.
    ContextActions,
    /// Ctrl+F12: the file's symbols in a palette.
    FileStructure,
    /// Alt+Shift+T: the server's refactorings at the caret or selection.
    RefactorThis,
    /// Ctrl+Alt+O: the server's organize imports action.
    OptimizeImports,
    /// Ctrl+Alt+M, Ctrl+Alt+V, Ctrl+Alt+C, Ctrl+Alt+N, Ctrl+F6, F6, Alt+Delete: the server's
    /// code action of one refactoring kind at the caret or selection.
    Refactoring(Refactoring),
    /// View > Inlay Hints: show or hide the server's inlay hints for this session.
    ToggleInlayHints,
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
    /// Delete the selected item where a list allows it (Scratch Files).
    DeleteItem,
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
        "app.main_menu" => "Main menu", App(AppAction::MainMenu);
        "app.about" => "About jetl", App(AppAction::About);
        "app.settings" => "Settings…", App(AppAction::Settings);
        "app.open_settings" => "Open settings file (config.toml)", App(AppAction::OpenSettings);
        "app.keymap_reference" => "Keymap reference", App(AppAction::KeymapReference);
        "tool_window.project" => "Project tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Project));
        "tool_window.worktrees" => "Worktrees tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Worktrees));
        "tool_window.git" => "Git tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Git));
        "tool_window.find" => "Find tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Find));
        "tool_window.structure" => "Structure tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Structure));
        "tool_window.event_log" => "Event Log tool window", App(AppAction::ToggleToolWindow(ToolWindowId::EventLog));
        "tool_window.run" => "Run tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Run));
        "tool_window.terminal" => "Terminal tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Terminal));
        "terminal.new_tab" => "Terminal: new tab (another shell)", App(AppAction::NewTerminalTab);
        "terminal.close_tab" => "Terminal: close the active tab (ends its shell)", App(AppAction::CloseTerminalTab);
        "tool_window.problems" => "Problems tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Problems));
        "tool_window.hide_all" => "Hide all tool windows", App(AppAction::HideAllToolWindows);
        "tool_window.hide_focused" => "Hide active tool window", App(AppAction::HideFocusedToolWindow);
        "tool_window.restore_default_layout" => "Restore the default tool window layout", App(AppAction::RestoreDefaultLayout);
        "tool_window.switch_layout" => "Switch to a saved tool window layout…", App(AppAction::SwitchLayout);
        "tool_window.save_layout_as_new" => "Save the tool window layout as a new named layout…", App(AppAction::SaveLayoutAsNew);
        "tool_window.save_layout_changes" => "Save changes in the current tool window layout", App(AppAction::SaveLayoutChanges);
        "tool_window.rename_layout" => "Rename a saved tool window layout…", App(AppAction::RenameLayout);
        "tool_window.delete_layout" => "Delete a saved tool window layout…", App(AppAction::DeleteLayout);
        "tool_window.stretch_left" => "Stretch tool window to the left", App(AppAction::ResizeToolWindow(Dir::Left));
        "tool_window.stretch_right" => "Stretch tool window to the right", App(AppAction::ResizeToolWindow(Dir::Right));
        "tool_window.stretch_up" => "Stretch tool window up", App(AppAction::ResizeToolWindow(Dir::Up));
        "tool_window.stretch_down" => "Stretch tool window down", App(AppAction::ResizeToolWindow(Dir::Down));
        "tool_window.next_tab" => "Next tab of the tool window", App(AppAction::NextToolWindowTab);
        "tool_window.prev_tab" => "Previous tab of the tool window", App(AppAction::PrevToolWindowTab);
        "tool_window.maximize" => "Maximize or restore the active tool window", App(AppAction::MaximizeToolWindow);
        "tool_window.toggle_pinned" => "Pin or unpin the active tool window (an unpinned one hides when focus leaves it)", App(AppAction::TogglePinned { tw: None });
        "tool_window.toggle_undocked" => "Undock or dock the active tool window (an undocked one slides over the editor)", App(AppAction::ToggleUndocked { tw: None });
        "tool_window.last" => "Jump to the last active tool window", App(AppAction::LastToolWindow);
        "tool_window.move_left" => "Move the active tool window to Left Top", App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Left, split: false } });
        "tool_window.move_left_bottom" => "Move the active tool window to Left Bottom", App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Left, split: true } });
        "tool_window.move_bottom" => "Move the active tool window to Bottom Left", App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Bottom, split: false } });
        "tool_window.move_bottom_right" => "Move the active tool window to Bottom Right", App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Bottom, split: true } });
        "tool_window.move_right" => "Move the active tool window to Right Top", App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Right, split: false } });
        "tool_window.move_right_bottom" => "Move the active tool window to Right Bottom", App(AppAction::MoveToolWindow { tw: None, dock: Dock { side: Side::Right, split: true } });
        "app.zen" => "Toggle Zen mode (editor text only)", App(AppAction::ToggleZen);
        "app.toggle_toolbar" => "Show or hide the toolbar", App(AppAction::ToggleToolbar);
        "palette.files" => "Go to file", App(AppAction::OpenPalette(PaletteKind::Files));
        "palette.actions" => "Find action", App(AppAction::OpenPalette(PaletteKind::Actions));
        "palette.search_everywhere" => "Search everywhere", App(AppAction::OpenPalette(PaletteKind::Everywhere));
        "palette.recent_files" => "Recent files", App(AppAction::OpenPalette(PaletteKind::RecentFiles));
        "palette.recent_locations" => "Recent locations", App(AppAction::RecentLocations);
        "palette.goto_line" => "Go to line", App(AppAction::OpenPalette(PaletteKind::GotoLine));
        "palette.recent_projects" => "Recent projects", App(AppAction::OpenPalette(PaletteKind::RecentProjects));
        "palette.symbols" => "Go to symbol", App(AppAction::OpenPalette(PaletteKind::Symbols));

        "editor.newline" => "Insert newline", Editor(E::InsertNewline);
        "editor.tab" => "Insert tab or indent selected lines", Editor(E::InsertTab);
        "editor.unindent" => "Unindent line or selection", Editor(E::Unindent);
        "editor.backspace" => "Delete left", Editor(E::Backspace);
        "editor.delete" => "Delete right", Editor(E::Delete);
        "editor.delete_word_back" => "Delete word left", Editor(E::DeleteWordBack);
        "editor.delete_word_forward" => "Delete word right", Editor(E::DeleteWordForward);
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
        "editor.save_as" => "Save as…", Editor(E::SaveAs);
        "editor.save_all" => "Save all", Editor(E::SaveAll);
        "editor.new_scratch" => "New scratch file…", App(AppAction::NewScratch);
        "editor.scratch_files" => "Scratch files", App(AppAction::OpenPalette(PaletteKind::Scratches));
        "editor.local_history" => "Local history of the file (editor) or the selected entry (Project tree; a folder lists its files), in its tool window", App(AppAction::LocalHistory);
        "editor.put_label" => "Local history: put label…", App(AppAction::PutLabel);
        // After the two above, so "local history" in Find Action picks them first.
        "tool_window.local_history" => "Local History tool window", App(AppAction::ToggleToolWindow(ToolWindowId::LocalHistory));
        "tool_window.tasks" => "Tasks tool window (yman)", App(AppAction::ToggleToolWindow(ToolWindowId::Tasks));
        "tool_window.todo" => "TODO tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Todo));
        "tool_window.hierarchy" => "Hierarchy tool window (call and type hierarchy)", App(AppAction::ToggleToolWindow(ToolWindowId::Hierarchy));
        "tool_window.debug" => "Debug tool window", App(AppAction::ToggleToolWindow(ToolWindowId::Debug));
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
        "editor.close_all_tabs" => "Close all tabs, pinned ones too", Editor(E::CloseAllTabs);
        "editor.close_other_tabs" => "Close other tabs (keeps pinned ones)", Editor(E::CloseOtherTabs);
        "editor.close_unpinned_tabs" => "Close all tabs but pinned", Editor(E::CloseUnpinnedTabs);
        "editor.toggle_pin_tab" => "Pin or unpin the active tab", Editor(E::TogglePinTab);
        "editor.move_tab_left" => "Move the active tab left", Editor(E::MoveTabLeft);
        "editor.move_tab_right" => "Move the active tab right", Editor(E::MoveTabRight);
        "editor.switcher" => "Switcher (recent tabs and tool windows)", Editor(E::Switcher);
        "editor.switcher_back" => "Switcher, selecting backwards (recent tabs and tool windows)", Editor(E::SwitcherBack);
        "editor.split_right" => "Split right (the tab in a new group beside)", Editor(E::SplitRight);
        "editor.split_down" => "Split down (the tab in a new group below)", Editor(E::SplitDown);
        "editor.next_group" => "Next tab group", Editor(E::NextGroup);
        "editor.prev_group" => "Previous tab group", Editor(E::PrevGroup);
        "editor.focus_group_left" => "Focus the tab group to the left", Editor(E::FocusGroupLeft);
        "editor.focus_group_right" => "Focus the tab group to the right", Editor(E::FocusGroupRight);
        "editor.focus_group_up" => "Focus the tab group above", Editor(E::FocusGroupUp);
        "editor.focus_group_down" => "Focus the tab group below", Editor(E::FocusGroupDown);
        "editor.unsplit" => "Unsplit (merge the group into the one beside it)", Editor(E::Unsplit);
        "editor.unsplit_all" => "Unsplit all (one group)", Editor(E::UnsplitAll);
        "editor.move_tab_to_opposite_group" => "Move the active tab to the opposite group", Editor(E::MoveTabToOppositeGroup);
        "editor.escape" => "Escape (close find, clear selection)", Editor(E::Escape);
        "editor.next_tabstop" => "Snippet: select the next tabstop (the last one ends the snippet)", Editor(E::NextTabstop);
        "editor.prev_tabstop" => "Snippet: select the previous tabstop", Editor(E::PrevTabstop);
        "editor.expand_snippet" => "Expand the live template (snippet) whose prefix is the word before the caret", Editor(E::ExpandSnippet);
        "editor.surround_with" => "Surround the selection with a live template that uses $TM_SELECTED_TEXT", Editor(E::SurroundWith);
        "editor.compare_with_clipboard" => "Compare the file (or the selection) with the clipboard", Editor(E::CompareWithClipboard);
        "editor.compare_with_file" => "Compare the file with another file…", Editor(E::CompareWithFile);
        "markdown.preview" => "Markdown: open or focus a read-only preview of the active Markdown file", Editor(E::MarkdownPreview);
        "editor.toggle_comment" => "Comment with line comment", Editor(E::ToggleComment);
        "editor.toggle_block_comment" => "Comment with block comment", Editor(E::ToggleBlockComment);
        "editor.move_line_up" => "Move line up", Editor(E::MoveLineUp);
        "editor.move_line_down" => "Move line down", Editor(E::MoveLineDown);
        "editor.join_lines" => "Join lines", Editor(E::JoinLines);
        "editor.toggle_case" => "Toggle case", Editor(E::ToggleCase);
        "editor.to_upper_case" => "To upper case (selection or word at the caret)", Editor(E::ToUpperCase);
        "editor.to_lower_case" => "To lower case (selection or word at the caret)", Editor(E::ToLowerCase);
        "editor.sort_lines" => "Sort lines (selected lines, else the whole file)", Editor(E::SortLines);
        "editor.reverse_lines" => "Reverse lines (selected lines, else the whole file)", Editor(E::ReverseLines);
        "editor.goto_bracket" => "Move caret to matching bracket", Editor(E::GotoBracket);
        "editor.expand_selection" => "Extend selection to the enclosing syntax node", Editor(E::ExpandSelection);
        "editor.shrink_selection" => "Shrink selection back one step", Editor(E::ShrinkSelection);
        "editor.add_next_occurrence" => "Add selection for next occurrence (a caret there)", Editor(E::AddNextOccurrence);
        "editor.unselect_occurrence" => "Unselect the last added occurrence", Editor(E::UnselectOccurrence);
        "editor.select_all_occurrences" => "Select all occurrences (a caret at each)", Editor(E::SelectAllOccurrences);
        "editor.clone_caret_above" => "Clone caret above", Editor(E::CloneCaretAbove);
        "editor.clone_caret_below" => "Clone caret below", Editor(E::CloneCaretBelow);
        "editor.toggle_column_mode" => "Toggle column selection mode", Editor(E::ToggleColumnMode);
        "editor.toggle_overwrite" => "Toggle insert/overwrite mode", Editor(E::ToggleOverwrite);
        "editor.toggle_soft_wrap" => "Soft-wrap the active editor (on or off)", Editor(E::ToggleSoftWrap);
        "editor.fold_collapse" => "Collapse the code region at the caret", Editor(E::FoldCollapse);
        "editor.fold_expand" => "Expand the folded region at the caret", Editor(E::FoldExpand);
        "editor.fold_collapse_recursive" => "Collapse the region at the caret and every region in it", Editor(E::FoldCollapseRecursive);
        "editor.fold_expand_recursive" => "Expand the region at the caret and every region in it", Editor(E::FoldExpandRecursive);
        "editor.fold_collapse_all" => "Collapse all code regions", Editor(E::FoldCollapseAll);
        "editor.fold_expand_all" => "Expand all folded regions", Editor(E::FoldExpandAll);
        "editor.fold_selection" => "Fold the selection (or expand the fold at the caret)", Editor(E::FoldSelection);
        "editor.scroll_up" => "Scroll up", Editor(E::ScrollUp(1));
        "editor.scroll_down" => "Scroll down", Editor(E::ScrollDown(1));
        "editor.nav_back" => "Navigate back to the previous caret position", Editor(E::NavBack);
        "editor.nav_forward" => "Navigate forward", Editor(E::NavForward);
        "editor.toggle_bookmark" => "Toggle bookmark on the caret line", Editor(E::ToggleBookmark);
        "editor.show_bookmarks" => "Show bookmarks", Editor(E::ShowBookmarks);
        "editor.toggle_numbered_bookmark" => "Toggle numbered bookmark (a digit) on the caret line", Editor(E::ToggleNumberedBookmark);
        "debug.toggle_breakpoint" => "Toggle breakpoint on each caret's line; a live debug session gets the change", Editor(E::ToggleBreakpoint);
        "editor.goto_bookmark_0" => "Go to bookmark 0", Editor(E::GotoNumberedBookmark(0));
        "editor.goto_bookmark_1" => "Go to bookmark 1", Editor(E::GotoNumberedBookmark(1));
        "editor.goto_bookmark_2" => "Go to bookmark 2", Editor(E::GotoNumberedBookmark(2));
        "editor.goto_bookmark_3" => "Go to bookmark 3", Editor(E::GotoNumberedBookmark(3));
        "editor.goto_bookmark_4" => "Go to bookmark 4", Editor(E::GotoNumberedBookmark(4));
        "editor.goto_bookmark_5" => "Go to bookmark 5", Editor(E::GotoNumberedBookmark(5));
        "editor.goto_bookmark_6" => "Go to bookmark 6", Editor(E::GotoNumberedBookmark(6));
        "editor.goto_bookmark_7" => "Go to bookmark 7", Editor(E::GotoNumberedBookmark(7));
        "editor.goto_bookmark_8" => "Go to bookmark 8", Editor(E::GotoNumberedBookmark(8));
        "editor.goto_bookmark_9" => "Go to bookmark 9", Editor(E::GotoNumberedBookmark(9));
        "editor.line_ending_lf" => "Convert line endings to LF", Editor(E::LineEndingLf);
        "editor.line_ending_crlf" => "Convert line endings to CRLF", Editor(E::LineEndingCrlf);
        "editor.set_language" => "Set the file's language (highlighting)", Editor(E::SetLanguage);
        "editor.revert_change" => "Revert the change at the caret to HEAD", Editor(E::RevertChange);
        "editor.diff_next_file" => "Diff tab: show the next file", Editor(E::DiffNextFile);
        "editor.diff_prev_file" => "Diff tab: show the previous file", Editor(E::DiffPrevFile);
        "editor.next_change" => "Go to the next change", Editor(E::NextChange);
        "editor.prev_change" => "Go to the previous change", Editor(E::PrevChange);
        "editor.toggle_blame" => "Annotate with git blame (show or hide)", Editor(E::ToggleBlame);
        "editor.format" => "Reformat code or the selection ([format] command, else the language server)", Editor(E::Format);

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
        "tree.new" => "Tree: new file or directory", Tree(TreeAction::New);
        "tree.new_file" => "Tree: new file", Tree(TreeAction::NewFile);
        "tree.new_dir" => "Tree: new directory", Tree(TreeAction::NewDir);
        "tree.rename" => "Tree: rename", Tree(TreeAction::Rename);
        "tree.delete" => "Tree: delete", Tree(TreeAction::Delete);
        "tree.copy_path" => "Tree: copy absolute path", Tree(TreeAction::CopyPath);
        "tree.copy_rel_path" => "Tree: copy path relative to the project", Tree(TreeAction::CopyRelPath);
        "tree.toggle_hidden" => "Tree: show or hide hidden files", Tree(TreeAction::ToggleHidden);
        "tree.toggle_ignored" => "Tree: show or hide ignored files", Tree(TreeAction::ToggleIgnored);
        "tree.context_menu" => "Tree: context menu", Tree(TreeAction::ContextMenu);

        "git.refresh" => "Git: refresh", Git(GitAction::Refresh);
        "git.toggle_stage" => "Git: stage/unstage selected (in the diff: the current change)", Git(GitAction::ToggleStage);
        "git.stage_all" => "Git: stage all", Git(GitAction::StageAll);
        "git.unstage_all" => "Git: unstage all", Git(GitAction::UnstageAll);
        "git.discard" => "Git: discard changes of the selected file or section (in the diff: the current change)", Git(GitAction::Discard);
        "git.next_hunk" => "Git: next change in the diff", Git(GitAction::NextHunk);
        "git.prev_hunk" => "Git: previous change in the diff", Git(GitAction::PrevHunk);
        "git.toggle_diff_layout" => "Git: diff side-by-side or unified", Git(GitAction::ToggleDiffLayout);
        "git.toggle_diff_collapse" => "Git: fold or show unchanged lines in the diff", Git(GitAction::ToggleDiffCollapse);
        "git.file_history" => "Git: show history for the file (editor) or the selected entry (Project tree)", Git(GitAction::FileHistory);
        "git.resolve_conflicts" => "Git: resolve conflicts (three-pane merge or take one side)", Git(GitAction::ResolveConflicts);
        "git.continue_operation" => "Git: continue the rebase, merge, cherry-pick or revert in progress", Git(GitAction::ContinueOperation);
        "git.skip_operation" => "Git: skip the commit the rebase, cherry-pick or revert stopped on", Git(GitAction::SkipOperation);
        "git.abort_operation" => "Git: abort the rebase, merge, cherry-pick or revert in progress", Git(GitAction::AbortOperation);
        "git.log_filter" => "Git: filter the log (branch, text, author, path)", Git(GitAction::LogFilter);
        "git.context_menu" => "Git: context menu of the selected commit, branch or commit file", Git(GitAction::ContextMenu);
        "git.copy_sha" => "Git: copy the selected commit's revision number", Git(GitAction::CopySha);
        "git.show_diff" => "Git: show the diff in an editor tab: the selected file, or in the Log list the selected commits", Git(GitAction::ShowDiff);
        "git.log_show_diff" => "Git: show the selected commits' files side by side in an editor tab", Git(GitAction::LogShowDiff);
        "git.log_show_patch" => "Git: show the combined diff of the selected commits as a unified patch in an editor tab", Git(GitAction::LogShowPatch);
        "git.log_checkout" => "Git: check out the selected commit (detached)", Git(GitAction::LogCheckout);
        "git.log_new_branch" => "Git: new branch at the selected commit", Git(GitAction::LogNewBranch);
        "git.log_cherry_pick" => "Git: cherry-pick the selected commit", Git(GitAction::LogCherryPick);
        "git.log_revert" => "Git: revert the selected commit", Git(GitAction::LogRevert);
        "git.log_reset" => "Git: reset the current branch to the selected commit (soft, mixed or hard)", Git(GitAction::LogReset);
        "git.log_interactive_rebase" => "Git: interactively rebase from the selected commit (pick, reword, squash, fixup, drop, reorder)", Git(GitAction::LogInteractiveRebase);
        "git.log_new_tag" => "Git: tag the selected commit", Git(GitAction::LogNewTag);
        "git.log_delete_tag" => "Git: delete a tag of the selected commit", Git(GitAction::LogDeleteTag);
        "git.commit" => "Git: commit", Git(GitAction::Commit);
        "git.commit_and_push" => "Git: commit and push", Git(GitAction::CommitAndPush);
        "git.commit_box" => "Git: open commit window", Git(GitAction::CommitBox);
        "git.toggle_amend" => "Git: amend the last commit (toggle)", Git(GitAction::ToggleAmend);
        "git.message_older" => "Git: previous commit message from history", Git(GitAction::MessageOlder);
        "git.message_newer" => "Git: next commit message from history", Git(GitAction::MessageNewer);
        "git.line_up" => "Git: in the Commit tab diff, the line cursor to the changed line above (elsewhere: the previous commit message)", Git(GitAction::LineUp);
        "git.line_down" => "Git: in the Commit tab diff, the line cursor to the changed line below (elsewhere: the next commit message)", Git(GitAction::LineDown);
        "git.line_extend_up" => "Git: in the Commit tab diff, select the changed line above too", Git(GitAction::LineExtendUp);
        "git.line_extend_down" => "Git: in the Commit tab diff, select the changed line below too", Git(GitAction::LineExtendDown);
        "git.push" => "Git: push", Git(GitAction::Push);
        "git.push_options" => "Git: push with options (tags, force with lease)", Git(GitAction::PushOptions);
        "git.pull" => "Git: pull (update project)", Git(GitAction::Pull);
        "git.fetch" => "Git: fetch", Git(GitAction::Fetch);
        "git.branches_popup" => "Git: branches", Git(GitAction::Branches);
        "git.new_branch" => "Git: new branch", Git(GitAction::NewBranch);
        "git.tab_commit" => "Git: Commit tab", Git(GitAction::TabCommit);
        "git.tab_log" => "Git: Log tab", Git(GitAction::TabLog);
        "git.tab_console" => "Git: Console tab", Git(GitAction::TabConsole);
        "git.tab_stash" => "Git: Stash tab", Git(GitAction::TabStash);
        "git.stash_push" => "Git: stash local changes", Git(GitAction::StashPush);
        "git.stash_include_untracked" => "Git: include untracked files in the next stash (toggle)", Git(GitAction::StashIncludeUntracked);
        "git.stash_pop" => "Git: pop the selected stash", Git(GitAction::StashPop);
        "git.stash_apply" => "Git: apply the selected stash", Git(GitAction::StashApply);
        "git.stash_drop" => "Git: drop the selected stash", Git(GitAction::StashDrop);
        "git.up" => "Git: up", Git(GitAction::Up);
        "git.down" => "Git: down", Git(GitAction::Down);
        "git.extend_selection_up" => "Git: in the Log, select the commit above too (elsewhere: up)", Git(GitAction::ExtendUp);
        "git.extend_selection_down" => "Git: in the Log, select the commit below too (elsewhere: down)", Git(GitAction::ExtendDown);
        "git.page_up" => "Git: page up", Git(GitAction::PageUp);
        "git.page_down" => "Git: page down", Git(GitAction::PageDown);
        "git.left" => "Git: left (in a commit's files: close the folder, else back to the list)", Git(GitAction::Left);
        "git.right" => "Git: right (in a commit's files: open the folder or the file's diff)", Git(GitAction::Right);
        "git.open_file" => "Git: open selected file", Git(GitAction::OpenFile);
        "git.focus_message" => "Git: focus commit message", Git(GitAction::FocusMessage);
        "git.next_section" => "Git: next pane", Git(GitAction::NextSection);
        "git.escape" => "Git: back (close diff, leave message, return to editor)", Git(GitAction::Escape);

        "merge.next_conflict" => "Merge: next unresolved change", Editor(E::Merge(MergeAction::NextConflict));
        "merge.prev_conflict" => "Merge: previous unresolved change", Editor(E::Merge(MergeAction::PrevConflict));
        "merge.accept_left" => "Merge: accept yours for the change at the caret", Editor(E::Merge(MergeAction::AcceptLeft));
        "merge.accept_right" => "Merge: accept theirs for the change at the caret", Editor(E::Merge(MergeAction::AcceptRight));
        "merge.ignore" => "Merge: ignore the change at the caret", Editor(E::Merge(MergeAction::Ignore));
        "merge.apply_non_conflicting" => "Merge: apply all non-conflicting changes", Editor(E::Merge(MergeAction::ApplyNonConflicting));
        "merge.accept_all_left" => "Merge: take yours for the whole file", Editor(E::Merge(MergeAction::AcceptAllLeft));
        "merge.accept_all_right" => "Merge: take theirs for the whole file", Editor(E::Merge(MergeAction::AcceptAllRight));
        "merge.apply" => "Merge: save the result, stage it and close the resolver", Editor(E::Merge(MergeAction::Apply));
        "merge.cancel" => "Merge: close the resolver without saving", Editor(E::Merge(MergeAction::Cancel));

        "worktree.refresh" => "Worktrees: refresh", Worktree(WorktreeAction::Refresh);
        "worktree.new" => "Worktrees: new", Worktree(WorktreeAction::New);
        "worktree.remove" => "Worktrees: remove", Worktree(WorktreeAction::Remove);
        "worktree.switch" => "Worktrees: switch", Worktree(WorktreeAction::Switch);
        "worktree.prune" => "Worktrees: prune", Worktree(WorktreeAction::Prune);
        "worktree.up" => "Worktrees: up", Worktree(WorktreeAction::Up);
        "worktree.down" => "Worktrees: down", Worktree(WorktreeAction::Down);

        "find.in_files" => "Find in files", Find(FindAction::InFiles);
        "find.replace_in_files" => "Replace in files", Find(FindAction::ReplaceInFiles);
        "find.next_result" => "Find: next result (opens it)", Find(FindAction::NextResult);
        "find.prev_result" => "Find: previous result (opens it)", Find(FindAction::PrevResult);
        "find.up" => "Find: up", Find(FindAction::Up);
        "find.down" => "Find: down", Find(FindAction::Down);
        "find.page_up" => "Find: page up", Find(FindAction::PageUp);
        "find.page_down" => "Find: page down", Find(FindAction::PageDown);
        "find.open" => "Find: open selected result", Find(FindAction::Open);
        "find.next_field" => "Find: switch between query and replacement", Find(FindAction::NextField);
        "find.toggle_case" => "Find: match case", Find(FindAction::ToggleCase);
        "find.toggle_word" => "Find: whole words", Find(FindAction::ToggleWord);
        "find.toggle_regex" => "Find: regex", Find(FindAction::ToggleRegex);
        "find.toggle_structural" => "Find: structural search (code pattern with $X)", Find(FindAction::ToggleStructural);
        "find.toggle_replace" => "Find: show or hide the replacement", Find(FindAction::ToggleReplace);
        "find.replace_all" => "Find: replace all results", Find(FindAction::ReplaceAll);
        "find.toggle_exclude" => "Find: exclude or include the selected result", Find(FindAction::ToggleExclude);
        "find.escape" => "Find: back to the editor", Find(FindAction::Escape);

        "structure.up" => "Structure: up", Structure(StructureAction::Up);
        "structure.down" => "Structure: down", Structure(StructureAction::Down);
        "structure.page_up" => "Structure: page up", Structure(StructureAction::PageUp);
        "structure.page_down" => "Structure: page down", Structure(StructureAction::PageDown);
        "structure.home" => "Structure: first symbol", Structure(StructureAction::Home);
        "structure.end" => "Structure: last symbol", Structure(StructureAction::End);
        "structure.open" => "Structure: jump to the selected symbol", Structure(StructureAction::Open);

        "event_log.up" => "Event Log: up", EventLog(EventLogAction::Up);
        "event_log.down" => "Event Log: down", EventLog(EventLogAction::Down);
        "event_log.page_up" => "Event Log: page up", EventLog(EventLogAction::PageUp);
        "event_log.page_down" => "Event Log: page down", EventLog(EventLogAction::PageDown);
        "event_log.home" => "Event Log: first entry", EventLog(EventLogAction::Home);
        "event_log.end" => "Event Log: last entry", EventLog(EventLogAction::End);
        "event_log.details" => "Event Log: show the full text of the selected entry", EventLog(EventLogAction::Details);
        "event_log.copy" => "Event Log: copy the selected entry", EventLog(EventLogAction::Copy);
        "event_log.clear" => "Event Log: clear", EventLog(EventLogAction::Clear);

        "run.pick" => "Run: choose a configuration and run it", App(AppAction::OpenPalette(PaletteKind::RunTasks));
        "run.run_selected" => "Run: run the selected configuration", Run(RunAction::RunSelected);
        "run.edit_configs" => "Run: edit run configurations", Run(RunAction::EditConfigs);
        "run.rerun" => "Run: run the last run again", Run(RunAction::Rerun);
        "run.rerun_failed" => "Run: rerun the failed tests", Run(RunAction::RerunFailed);
        "run.test_at_caret" => "Run: run the test at the caret", Run(RunAction::TestAtCaret);
        "run.http_request_at_caret" => "Run: send the HTTP request at the caret of a .http file with curl", Run(RunAction::HttpRequestAtCaret);
        "run.build_scip_index" => "Run: build the SCIP index with [index] scip_command, then load it", Run(RunAction::BuildScipIndex);
        "run.stop" => "Run: stop the running task", Run(RunAction::Stop);
        "run.toggle_view" => "Run: switch between output and the test tree", Run(RunAction::ToggleView);
        "run.expand" => "Run: expand the selected tree row", Run(RunAction::Expand);
        "run.collapse" => "Run: collapse the selected tree row", Run(RunAction::Collapse);
        "run.up" => "Run: up", Run(RunAction::Up);
        "run.down" => "Run: down", Run(RunAction::Down);
        "run.page_up" => "Run: page up", Run(RunAction::PageUp);
        "run.page_down" => "Run: page down", Run(RunAction::PageDown);
        "run.home" => "Run: first line", Run(RunAction::Home);
        "run.end" => "Run: last line", Run(RunAction::End);
        "run.open" => "Run: jump to the selected line's location", Run(RunAction::Open);
        "run.next_problem" => "Run: next problem (opens it)", Run(RunAction::NextProblem);
        "run.prev_problem" => "Run: previous problem (opens it)", Run(RunAction::PrevProblem);
        "debug.start" => "Debug: start the selected run configuration under its debug adapter", Debug(DebugAction::Start);
        "debug.stop" => "Debug: stop the debug session", Debug(DebugAction::Stop);
        "debug.resume" => "Debug: Resume", Debug(DebugAction::Resume);
        "debug.step_over" => "Debug: Step Over", Debug(DebugAction::StepOver);
        "debug.step_into" => "Debug: Step Into", Debug(DebugAction::StepInto);
        "debug.step_out" => "Debug: Step Out", Debug(DebugAction::StepOut);
        "debug.pause" => "Debug: Pause", Debug(DebugAction::Pause);
        "debug.show_execution_point" => "Debug: Show Execution Point", Debug(DebugAction::ShowExecutionPoint);
        "debug.up" => "Debug: up", Debug(DebugAction::Up);
        "debug.down" => "Debug: down", Debug(DebugAction::Down);
        "debug.page_up" => "Debug: page up", Debug(DebugAction::PageUp);
        "debug.page_down" => "Debug: page down", Debug(DebugAction::PageDown);
        "debug.home" => "Debug: first line", Debug(DebugAction::Home);
        "debug.end" => "Debug: last line", Debug(DebugAction::End);
        "debug.open" => "Debug: jump to the selected line's location", Debug(DebugAction::Open);

        "lsp.restart" => "Restart language servers", Lsp(LspAction::Restart);
        "lsp.next_problem" => "Next problem (diagnostic) in the file", Lsp(LspAction::NextProblem);
        "lsp.prev_problem" => "Previous problem (diagnostic) in the file", Lsp(LspAction::PrevProblem);
        "lsp.goto_definition" => "Go to declaration (on a declaration: its usages)", Lsp(LspAction::GotoDefinition);
        "lsp.goto_type_definition" => "Go to the type declaration of the symbol at the caret", Lsp(LspAction::GotoTypeDefinition);
        "lsp.goto_implementation" => "Go to the implementations of the symbol at the caret", Lsp(LspAction::GotoImplementation);
        "lsp.goto_declaration" => "Go to the declaration (not the definition) of the symbol at the caret", Lsp(LspAction::GotoDeclaration);
        "lsp.find_usages" => "Find usages of the symbol at the caret", Lsp(LspAction::FindUsages);
        "lsp.parameter_info" => "Parameter info: the signature of the call at the caret", Lsp(LspAction::ParameterInfo);
        "lsp.quick_doc" => "Quick documentation for the symbol at the caret", Lsp(LspAction::QuickDoc);
        "lsp.complete" => "Code completion at the caret", Lsp(LspAction::Complete);
        "lsp.rename" => "Rename the symbol at the caret and its usages", Lsp(LspAction::Rename);
        "lsp.context_actions" => "Show context actions (quick fixes, refactorings) at the caret", Lsp(LspAction::ContextActions);
        "lsp.file_structure" => "File structure: the file's symbols from its language server", Lsp(LspAction::FileStructure);
        "lsp.refactor_this" => "Refactor This: the server's refactorings at the caret or selection", Lsp(LspAction::RefactorThis);
        "lsp.optimize_imports" => "Optimize imports (the server's organize imports action)", Lsp(LspAction::OptimizeImports);
        "lsp.extract_method" => "Extract Method: the server's extract function refactoring at the caret or selection", Lsp(LspAction::Refactoring(Refactoring::ExtractMethod));
        "lsp.extract_variable" => "Extract Variable: the server's extract variable refactoring at the caret or selection", Lsp(LspAction::Refactoring(Refactoring::ExtractVariable));
        "lsp.extract_constant" => "Extract Constant: the server's extract constant refactoring at the caret or selection", Lsp(LspAction::Refactoring(Refactoring::ExtractConstant));
        "lsp.inline" => "Inline: the server's inline refactoring at the caret", Lsp(LspAction::Refactoring(Refactoring::Inline));
        "lsp.change_signature" => "Change Signature: the server's change signature refactoring at the caret", Lsp(LspAction::Refactoring(Refactoring::ChangeSignature));
        "lsp.move" => "Move: the server's move refactoring at the caret or selection", Lsp(LspAction::Refactoring(Refactoring::Move));
        "lsp.safe_delete" => "Safe Delete: the server's delete or remove refactoring at the caret", Lsp(LspAction::Refactoring(Refactoring::SafeDelete));
        "lsp.quick_definition" => "Quick definition: the definition of the symbol at the caret in a popup", Lsp(LspAction::QuickDefinition);
        "lsp.call_hierarchy" => "Call hierarchy: the callers and callees of the function at the caret", Lsp(LspAction::CallHierarchy);
        "lsp.type_hierarchy" => "Type hierarchy: the supertypes and subtypes of the type at the caret", Lsp(LspAction::TypeHierarchy);
        "lsp.toggle_inlay_hints" => "Show or hide inlay hints", Lsp(LspAction::ToggleInlayHints);

        "problems.up" => "Problems: up", Problems(ProblemsAction::Up);
        "problems.down" => "Problems: down", Problems(ProblemsAction::Down);
        "problems.page_up" => "Problems: page up", Problems(ProblemsAction::PageUp);
        "problems.page_down" => "Problems: page down", Problems(ProblemsAction::PageDown);
        "problems.home" => "Problems: first row", Problems(ProblemsAction::Home);
        "problems.end" => "Problems: last row", Problems(ProblemsAction::End);
        "problems.open" => "Problems: jump to the selected problem", Problems(ProblemsAction::Open);
        "problems.copy" => "Problems: copy the selected message", Problems(ProblemsAction::Copy);
        "problems.toggle_current_file" => "Problems: show only the current file", Problems(ProblemsAction::ToggleCurrentFile);
        "todo.up" => "TODO: up", Todo(TodoAction::Up);
        "todo.down" => "TODO: down", Todo(TodoAction::Down);
        "todo.page_up" => "TODO: page up", Todo(TodoAction::PageUp);
        "todo.page_down" => "TODO: page down", Todo(TodoAction::PageDown);
        "todo.home" => "TODO: first row", Todo(TodoAction::Home);
        "todo.end" => "TODO: last row", Todo(TodoAction::End);
        "todo.open" => "TODO: open the selected match", Todo(TodoAction::Open);
        "todo.refresh" => "TODO: scan the project again", Todo(TodoAction::Refresh);
        "hierarchy.up" => "Hierarchy: up", Hierarchy(HierarchyAction::Up);
        "hierarchy.down" => "Hierarchy: down", Hierarchy(HierarchyAction::Down);
        "hierarchy.page_up" => "Hierarchy: page up", Hierarchy(HierarchyAction::PageUp);
        "hierarchy.page_down" => "Hierarchy: page down", Hierarchy(HierarchyAction::PageDown);
        "hierarchy.home" => "Hierarchy: first row", Hierarchy(HierarchyAction::Home);
        "hierarchy.end" => "Hierarchy: last row", Hierarchy(HierarchyAction::End);
        "hierarchy.expand" => "Hierarchy: expand the selected row (loads its children)", Hierarchy(HierarchyAction::Expand);
        "hierarchy.collapse" => "Hierarchy: collapse the selected row, or go to its parent", Hierarchy(HierarchyAction::Collapse);
        "hierarchy.open" => "Hierarchy: open the selected function or type", Hierarchy(HierarchyAction::Open);
        "hierarchy.toggle_direction" => "Hierarchy: switch callers and callees, supertypes and subtypes, or imports and imported by", Hierarchy(HierarchyAction::ToggleDirection);
        "hierarchy.refresh" => "Hierarchy: refresh the tree (ask the server again, or recompute from the symbol index)", Hierarchy(HierarchyAction::Refresh);
        "hierarchy.export_mermaid" => "Hierarchy: export the loaded tree as a Mermaid diagram (scratch file)", Hierarchy(HierarchyAction::ExportMermaid);
        "hierarchy.export_dot" => "Hierarchy: export the loaded tree as a Graphviz DOT graph (scratch file)", Hierarchy(HierarchyAction::ExportDot);
        "hierarchy.file_dependencies" => "File dependencies: the files the active file imports, and those that import it, in the Hierarchy window", Hierarchy(HierarchyAction::FileDependencies);

        "local_history.up" => "Local History: up", History(HistoryAction::Up);
        "local_history.down" => "Local History: down", History(HistoryAction::Down);
        "local_history.page_up" => "Local History: page up", History(HistoryAction::PageUp);
        "local_history.page_down" => "Local History: page down", History(HistoryAction::PageDown);
        "local_history.home" => "Local History: first row (in the diff: top)", History(HistoryAction::Home);
        "local_history.end" => "Local History: last row (in the diff: bottom)", History(HistoryAction::End);
        "local_history.left" => "Local History: back to the snapshot list", History(HistoryAction::Left);
        "local_history.right" => "Local History: to the diff", History(HistoryAction::Right);
        "local_history.next_section" => "Local History: switch between the list and the diff", History(HistoryAction::NextSection);
        "local_history.open_file" => "Local History: open the file in the editor (in the project view: the selected file's history, or a label's files)", History(HistoryAction::OpenFile);
        "local_history.files" => "Local History: the project's files with history, deleted ones included (from a file's history: back)", History(HistoryAction::Files);
        "local_history.next_change" => "Local History: next change in the diff", History(HistoryAction::NextChange);
        "local_history.prev_change" => "Local History: previous change in the diff", History(HistoryAction::PrevChange);
        "local_history.toggle_diff_layout" => "Local History: diff side-by-side or unified", History(HistoryAction::ToggleDiffLayout);
        "local_history.toggle_diff_collapse" => "Local History: fold or show unchanged lines in the diff", History(HistoryAction::ToggleDiffCollapse);
        "local_history.refresh" => "Local History: list the snapshots (or the files) again", History(HistoryAction::Refresh);
        "local_history.revert" => "Local History: revert the file to the selected snapshot (undoable)", History(HistoryAction::Revert);
        "local_history.revert_changes" => "Local History: revert the selected changes of the diff (undoable)", History(HistoryAction::RevertChanges);
        "local_history.extend_selection_up" => "Local History: in the diff, select the change above too (in the list: up)", History(HistoryAction::ExtendUp);
        "local_history.extend_selection_down" => "Local History: in the diff, select the change below too (in the list: down)", History(HistoryAction::ExtendDown);
        "local_history.select_all_changes" => "Local History: select every change of the diff", History(HistoryAction::SelectAllChanges);
        "local_history.line_up" => "Local History: in the diff, the line cursor to the changed line above (in the list: up)", History(HistoryAction::LineUp);
        "local_history.line_down" => "Local History: in the diff, the line cursor to the changed line below (in the list: down)", History(HistoryAction::LineDown);
        "local_history.line_extend_up" => "Local History: in the diff, select the changed line above too", History(HistoryAction::LineExtendUp);
        "local_history.line_extend_down" => "Local History: in the diff, select the changed line below too", History(HistoryAction::LineExtendDown);
        "local_history.put_label" => "Local History: put a label on the file…", History(HistoryAction::PutLabel);
        "local_history.remove_label" => "Local History: remove the selected label (of every file it is on)…", History(HistoryAction::RemoveLabel);
        "local_history.context_menu" => "Local History: actions for the selected snapshot", History(HistoryAction::ContextMenu);
        "local_history.escape" => "Local History: clear the filter, back to the list, or focus the editor", History(HistoryAction::Escape);

        "tasks.up" => "Tasks: up (in the details: scroll up)", Tasks(TasksAction::Up);
        "tasks.down" => "Tasks: down (in the details: scroll down)", Tasks(TasksAction::Down);
        "tasks.page_up" => "Tasks: page up", Tasks(TasksAction::PageUp);
        "tasks.page_down" => "Tasks: page down", Tasks(TasksAction::PageDown);
        "tasks.home" => "Tasks: first row (in the details: top)", Tasks(TasksAction::Home);
        "tasks.end" => "Tasks: last row (in the details: bottom)", Tasks(TasksAction::End);
        "tasks.next_section" => "Tasks: switch between the list and the details", Tasks(TasksAction::NextSection);
        "tasks.details" => "Tasks: focus the details", Tasks(TasksAction::Details);
        "tasks.toggle_closed" => "Tasks: show or hide closed tasks", Tasks(TasksAction::ToggleClosed);
        "tasks.filter_tag" => "Tasks: filter by tag…", Tasks(TasksAction::FilterTag);
        "tasks.filter_assignee" => "Tasks: filter by assignee…", Tasks(TasksAction::FilterAssignee);
        "tasks.jump_related" => "Tasks: jump to a related task…", Tasks(TasksAction::JumpRelated);
        "tasks.change_status" => "Tasks: change the selected task's status…", Tasks(TasksAction::ChangeStatus);
        "tasks.change_priority" => "Tasks: change the selected task's priority…", Tasks(TasksAction::ChangePriority);
        "tasks.assign" => "Tasks: assign the selected task…", Tasks(TasksAction::Assign);
        "tasks.edit_tags" => "Tasks: edit the selected task's tags…", Tasks(TasksAction::EditTags);
        "tasks.comment" => "Tasks: comment on the selected task…", Tasks(TasksAction::Comment);
        "tasks.new_task" => "Tasks: new task…", Tasks(TasksAction::NewTask);
        "tasks.refresh" => "Tasks: reload the tracker", Tasks(TasksAction::Refresh);
        "tasks.sync" => "Tasks: sync the tracker with its remote (yman sync)", Tasks(TasksAction::SyncTracker);
        "tasks.context_menu" => "Tasks: actions for the selected task", Tasks(TasksAction::ContextMenu);
        "tasks.escape" => "Tasks: clear the filter, back to the list, or focus the editor", Tasks(TasksAction::Escape);

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
        "palette.delete_item" => "Palette: delete the selected item (Scratch Files, Recent Projects)", Palette(PaletteAction::DeleteItem);

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
            // Bound only in the windows that have inner tabs.
            Action::App(AppAction::NextToolWindowTab | AppAction::PrevToolWindowTab) => &["git", "run", "terminal"],
            Action::App(AppAction::NewTerminalTab | AppAction::CloseTerminalTab) => &["terminal"],
            Action::App(_) => &["global"],
            Action::Editor(EditorAction::Merge(_)) => &["editor.merge"],
            Action::Editor(EditorAction::NextTabstop | EditorAction::PrevTabstop) => &["editor.snippet"],
            Action::Editor(EditorAction::ExpandSnippet) => &["editor.snippet_prefix"],
            Action::Editor(_) => &["editor", "global"],
            Action::Tree(_) => &["tree", "global"],
            Action::Git(_) => &["git", "global"],
            Action::Worktree(_) => &["worktrees", "global"],
            Action::Find(_) => &["find", "global"],
            Action::Structure(_) => &["structure", "global"],
            Action::EventLog(_) => &["event_log", "global"],
            Action::Run(_) => &["run", "global"],
            Action::Debug(DebugAction::Stop) => &["debug.active"],
            Action::Debug(
                DebugAction::Resume
                | DebugAction::StepOver
                | DebugAction::StepInto
                | DebugAction::StepOut
                | DebugAction::ShowExecutionPoint,
            ) => &["debug.suspended"],
            Action::Debug(
                DebugAction::Up
                | DebugAction::Down
                | DebugAction::PageUp
                | DebugAction::PageDown
                | DebugAction::Home
                | DebugAction::End
                | DebugAction::Open,
            ) => &["debug", "global"],
            Action::Debug(_) => &["global"],
            Action::Problems(_) => &["problems", "global"],
            Action::History(_) => &["local_history", "global"],
            Action::Tasks(_) => &["tasks", "global"],
            Action::Todo(_) => &["todo", "global"],
            Action::Hierarchy(_) => &["hierarchy", "global"],
            Action::Lsp(_) => &["editor", "global"],
            Action::Palette(_) => &["palette"],
            Action::Mode(_) => &["mode.normal", "mode.insert", "mode.visual"],
        }
    }
}
