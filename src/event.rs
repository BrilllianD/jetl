//! Messages flowing from background services into the app loop.

use std::path::PathBuf;

use crate::ui::focus::PanelId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// What a layout op does once its write has succeeded (`App::layouts_loaded`);
/// a failed op reports its error and does none of it.
#[derive(Debug)]
pub struct LayoutDone {
    /// Shown as an Info toast.
    pub toast: String,
    /// `App::current_layout` becomes this; `None` leaves it.
    pub current: Option<String>,
    /// The current layout was deleted: `App::current_layout` becomes `None`.
    pub clear_current: bool,
}

#[derive(Debug)]
pub enum AppMsg {
    /// A file read for a new tab, tagged with the workspace generation at
    /// the time of the request so loads from a previous root are dropped.
    FileLoaded {
        generation: u64,
        path: PathBuf,
        result: std::io::Result<Vec<u8>>,
        focus: bool,
        /// A read for the debugger: the ticket of `DebugState::view` when it began. `None` for every other open.
        view: Option<u64>,
    },
    Git(crate::git::GitMsg),
    /// yman results (`yman::YmanService`), tagged with the workspace generation.
    Yman(crate::yman::YmanMsg),
    /// `git worktree list`, tagged with the workspace generation like `GitMsg`.
    Worktrees {
        generation: u64,
        result: anyhow::Result<Vec<crate::worktree::Worktree>>,
    },
    /// A saved session and its files (`files[i]` belongs to
    /// `session.files[i]`), tagged with the workspace generation.
    SessionLoaded {
        generation: u64,
        session: Box<crate::workspace::Session>,
        files: Vec<std::io::Result<Vec<u8>>>,
    },
    /// The files of a branch's editor set (`files[i]` belongs to
    /// `set.files[i]`), read for branch swap `seq` (`App::sync_branch_set`),
    /// tagged with the workspace generation.
    BranchSetLoaded {
        generation: u64,
        seq: u64,
        set: crate::workspace::BranchSet,
        files: Vec<std::io::Result<Vec<u8>>>,
    },
    /// Config files changed (`config::watch::ConfigWatcher`): which kinds,
    /// sorted. No generation: not tied to the root, and the reload reads
    /// the files when it runs.
    ConfigChanged {
        files: Vec<crate::config::watch::ConfigFile>,
    },
    /// A debounced batch of filesystem changes under the root.
    FsChanged {
        generation: u64,
        changes: crate::files::watch::Changes,
    },
    /// The watcher started (it runs off the main thread) or could not.
    WatcherStarted {
        generation: u64,
        result: anyhow::Result<crate::files::watch::Watcher>,
    },
    /// A batch of events from the outside watcher (folders outside the root
    /// that language servers registered), tagged with the workspace generation.
    LspWatched {
        generation: u64,
        events: Vec<(PathBuf, crate::files::watch::FileChange)>,
    },
    /// The outside watcher for `dirs` (`(folder, recursive)`) started or could not.
    LspWatcherStarted {
        generation: u64,
        dirs: Vec<(PathBuf, bool)>,
        result: anyhow::Result<crate::files::watch::Watcher>,
    },
    /// An open file read again after it changed on disk.
    DiskText {
        generation: u64,
        path: PathBuf,
        result: std::io::Result<Vec<u8>>,
    },
    /// A rename preview built (`App::rename_preview_start`); dropped when the root or the Find seq moved on.
    RenamePreview {
        generation: u64,
        seq: u64,
        result: Box<Result<(crate::files::rename_preview::RenamePreview, Vec<crate::files::find::Hit>), String>>,
    },
    /// Find in files results (`FindPanel::search`).
    Find {
        generation: u64,
        batch: crate::files::find::Batch,
    },
    /// TODO window scan results (`TodoPanel::scan`).
    Todo {
        generation: u64,
        batch: crate::files::find::Batch,
    },
    /// Replace in files finished writing the files that were not open.
    ReplaceDone {
        generation: u64,
        files: usize,
        matches: usize,
        errors: Vec<String>,
    },
    /// A file operation from the tree finished.
    FsOpDone {
        generation: u64,
        op: crate::files::ops::FsOp,
        /// Whether the path (a rename's source) was a directory, read before the operation.
        is_dir: bool,
        result: Result<(), String>,
    },
    /// Re-read tree directories (`ProjectPanel::begin_refresh`), tagged
    /// with the workspace generation.
    TreeLoaded {
        generation: u64,
        seq: u64,
        dirs: crate::files::tree::DirListing,
    },
    /// The palette's file walk of the root, tagged with the workspace
    /// generation and its request order.
    FileIndex {
        generation: u64,
        seq: u64,
        files: Vec<String>,
    },
    /// Symbols parsed by the project index (`SymbolIndex::start`), tagged
    /// with the workspace generation and the run's seq.
    Symbols {
        generation: u64,
        seq: u64,
        batch: crate::symbols::index::SymbolBatch,
    },
    /// Import edges built in `spawn_blocking` from an index snapshot (`App::spawn_import_edges`), tagged with the
    /// workspace generation; `ImportEdges::version` is the index version they were built from
    /// (`SymbolIndex::on_edges` drops another).
    ImportEdges {
        generation: u64,
        edges: std::sync::Arc<crate::symbols::index::ImportEdges>,
    },
    /// Children of an index-fed Hierarchy node computed in `spawn_blocking` (`App::hierarchy_index_children`), tagged
    /// with the workspace generation, the tree id and the node (`App::hierarchy_index_answer` drops another).
    /// Each def comes with its line read from disk (`None`: unreadable).
    HierarchyIndexChildren {
        generation: u64,
        tree: u64,
        node: usize,
        children: Vec<(crate::symbols::graph::Def, Option<String>)>,
    },
    /// The SCIP index named by `[index] scip`, loaded in `spawn_blocking`;
    /// tagged with the workspace generation and the load's seq.
    Scip {
        generation: u64,
        seq: u64,
        /// `Err`: the message and the index file's mtime when the load began.
        result: Result<Box<crate::symbols::scip::ScipIndex>, (String, Option<std::time::SystemTime>)>,
    },
    /// An off-thread palette filter (`Palette::refilter` for large sources):
    /// candidate indices, best first, of filter `seq`; tagged with the
    /// workspace generation. Cancelled filters send nothing.
    PaletteFiltered {
        generation: u64,
        seq: u64,
        rows: Vec<usize>,
    },
    /// Recent project roots that still exist, newest first (not tied to the
    /// root). `version` is the file state's version taken under the file
    /// lock (`recent_projects::update`); a higher one saw a later state, and
    /// `seq` is the request's, which tells replies older than a forget.
    /// `error` is a failed write of a
    /// forgotten project (`App::forget_recent_project`).
    RecentProjects {
        seq: u64,
        version: u64,
        roots: Vec<PathBuf>,
        error: Option<String>,
    },
    /// Named layouts read (and changed, when an op was given) by
    /// `layouts::update`; `version` is taken under the file lock, a higher one
    /// saw a later state. `error` is shown as a warning. `done` is what the
    /// op's caller wants on success; it applies even to a stale reply.
    Layouts {
        version: u64,
        layouts: Vec<crate::layouts::NamedLayout>,
        error: Option<String>,
        done: Option<LayoutDone>,
    },
    /// The system clipboard read by `editor.paste` (`App::paste`), tagged
    /// with the workspace generation and the request's seq.
    Paste {
        generation: u64,
        seq: u64,
        result: Result<String, String>,
    },
    /// A compare (`App::compare_with_clipboard`, `App::compare_with_file`)
    /// computed off the main thread, tagged with the workspace generation;
    /// it opens a diff tab titled `title` over `key`.
    Compared {
        generation: u64,
        title: String,
        key: crate::git::model::DiffKey,
        result: Result<crate::git::diff::Compared, String>,
    },
    /// A formatter run for buffer `id` (`App::start_format`), tagged with
    /// the workspace generation and the request's seq.
    Formatted {
        generation: u64,
        id: crate::editor::buffer::BufferId,
        seq: u64,
        result: Result<String, crate::util::format::Failure>,
    },
    /// Output lines of run `run` (`App::run_task`), tagged with the
    /// workspace generation.
    RunOutput {
        generation: u64,
        run: u64,
        lines: Vec<crate::run::matcher::RunLine>,
    },
    /// Run `run` ended; sent after its last output.
    RunExited {
        generation: u64,
        run: u64,
        end: crate::run::process::RunEnd,
    },
    /// The task picker's tasks (`App::load_run_tasks`), tagged with the
    /// workspace generation and the request's seq; the error is an invalid
    /// `.jetl/run.toml`.
    RunTasks {
        generation: u64,
        seq: u64,
        tasks: Vec<crate::run::RunTask>,
        error: Option<String>,
    },
    /// Output (or the exit) of the Terminal window's shell `id` waits in its
    /// inbox (`terminal::pty::Session::drain`); tagged with the workspace
    /// generation. At most one is on the bus per shell.
    TerminalOutput {
        generation: u64,
        id: u64,
    },
    /// A language server event, tagged with the workspace generation and
    /// the server's id (`app/lsp.rs`).
    Lsp(LspMsg),
    /// A debug session's report (`app/debug.rs`); dropped unless its generation and session are the current ones.
    Dap(crate::dap::DapMsg),
    /// `debug.start`'s choice of configuration and adapter, made off the main thread; dropped on an older generation or seq.
    DebugReady {
        generation: u64,
        seq: u64,
        result: Box<Result<crate::app::Prepared, String>>,
    },
    /// Attach to Process's list, read off the main thread; dropped on an older generation or pick.
    DebugProcesses {
        generation: u64,
        seq: u64,
        items: Vec<crate::dap::procs::ProcInfo>,
        ptrace_scope: Option<u8>,
    },
    /// A Debug build step ended. Stale when `generation` or `seq` is not the current one, as `DebugReady`.
    DebugBuilt {
        generation: u64,
        seq: u64,
        end: crate::run::process::RunEnd,
        artifacts: Vec<crate::dap::build::Artifact>,
    },
    /// A batch of the program's output, split and matched off the main thread. Stale when `generation` is not the
    /// current one or `session` is not the Debug window's: lines can come after the session ended, so `Live` is
    /// not asked.
    DebugOutput {
        generation: u64,
        session: crate::dap::session::SessionId,
        lines: Vec<crate::run::matcher::RunLine>,
    },
    /// `VmRSS` of the debug adapter's process, in kB, read off the main thread (`App::debug_adapter_memory`); `None`
    /// when `/proc` could not be read. Stale when `generation` or `session` is not the live session's.
    DebugAdapterMemory {
        generation: u64,
        session: crate::dap::session::SessionId,
        kb: Option<u64>,
    },
    /// Definitions or usages with their line previews, the ones in files
    /// that are not open read from disk; shown in the Locations palette
    /// unless a newer definition or usages request (`seq`) was made.
    Locations {
        generation: u64,
        seq: u64,
        title: String,
        items: Vec<crate::lsp::locations::LocationItem>,
    },
    /// The Quick Definition popup's heading and code, the target file's text
    /// read off the main thread (an open buffer's, else the file's); shown
    /// unless the root, the newest definition request (`tag.seq`) or the
    /// caret, version or group changed.
    QuickDefinition {
        generation: u64,
        tag: crate::lsp::DocTag,
        heading: String,
        code: Result<String, String>,
        lang: Option<&'static str>,
    },
    /// The caret history's rows with their line text, the ones in files that
    /// are not open read from disk (`App::show_recent_locations`).
    RecentLocations {
        generation: u64,
        items: Vec<crate::lsp::locations::LocationItem>,
    },
    /// A workspace edit's disk steps (files that were not open edited,
    /// file operations) were made in order (`App::apply_workspace_edit`):
    /// how many changed, what failed, with what was done in the open
    /// buffers before; the file operations made, in order; and the
    /// `documentChanges` index of the step that failed and stopped the rest.
    EditsApplied {
        generation: u64,
        origin: crate::lsp::EditOrigin,
        done: crate::lsp::edits::EditCounts,
        errors: Vec<String>,
        ops: Vec<crate::files::ops::FsOp>,
        failed: Option<u32>,
    },
    /// A local history list, stored snapshot, diff or label change
    /// (`history::HistoryMsg`); no generation: the store is global.
    LocalHistory(crate::history::HistoryMsg),
    /// `doctor::probe`'s report for the Settings window's Dependencies page,
    /// tagged with the workspace generation (relative commands resolve against
    /// the root) and the request's seq.
    Dependencies {
        generation: u64,
        seq: u64,
        report: crate::doctor::Report,
    },
}

pub use crate::lsp::LspMsg;

/// Side effects a panel asks the app to perform after handling input.
#[derive(Debug)]
pub enum Effect {
    OpenFile(PathBuf),
    /// Open a file, or the conflict resolver when the last status lists it as conflicted (App::open_or_resolve).
    OpenOrResolve(PathBuf),
    /// Open (or activate) a diff tab over files, showing files[at] (App::open_diff_tab).
    OpenDiffTab {
        title: String,
        files: Vec<crate::git::model::DiffKey>,
        at: usize,
    },
    /// Open a file with the caret at a 0-based line and char column.
    OpenFileAt {
        path: PathBuf,
        line: usize,
        col: usize,
    },
    /// Open a file with the caret at an LSP position (0-based line, UTF-16
    /// column), converted once the text is loaded
    /// (`App::open_file_at_utf16`).
    OpenFileAtUtf16 {
        path: PathBuf,
        pos: lsp_types::Position,
    },
    /// A stack frame was chosen in the Debug window's list, by key or mouse; `focus` also moves focus to the editor.
    DebugFrame {
        index: usize,
        focus: bool,
    },
    Focus(PanelId),
    Notify(Level, String),
    SwitchRoot(PathBuf),
    /// Re-read the project tree off the main thread.
    RefreshTree,
    /// Tree, git status, branches and worktrees.
    GitRefresh,
    /// A git operation or query for the app to run through `GitService`.
    Git(crate::git::GitRequest),
    Dialog(crate::ui::dialog::Dialog),
    /// Ask where to save buffer `id` (`App::ask_save_as`), then go on with
    /// `then`.
    SaveAs {
        id: crate::editor::buffer::BufferId,
        then: crate::app::AfterSave,
    },
    /// `quiet` (autosave) skips the "Saved" log entry.
    FileSaved {
        path: PathBuf,
        quiet: bool,
    },
    Clipboard(String),
    /// Run a formatter off the main thread (`App::start_format`).
    Format(crate::editor::format::FormatRequest),
    /// Open a context menu (`App.menu`).
    ContextMenu(crate::ui::menu::ContextMenu),
    /// Run an action after the panel's input was handled (toolbar buttons).
    Perform(crate::actions::Action),
    /// Ask the language servers (`app/lsp.rs`).
    Lsp(crate::lsp::LspRequest),
    /// Take a snapshot, list a file's snapshots, diff or revert to one,
    /// ask for a label or its removal (`app/history.rs`).
    History(crate::history::Request),
    Quit,
}

/// A mouse event with click count already resolved by the app.
#[derive(Debug, Clone, Copy)]
pub struct Mouse {
    pub kind: crossterm::event::MouseEventKind,
    pub x: u16,
    pub y: u16,
    /// 1 = single, 2 = double, 3 = triple (only for button presses).
    pub clicks: u8,
    pub mods: crossterm::event::KeyModifiers,
}
