# Architecture

jetl is a single-process terminal app: ratatui draws every frame from the
`App` struct, crossterm delivers input, and tokio runs background work. This
document maps the modules, follows an event through the system, and lists
the rules that keep async results and buffer edits consistent. Feature order
and backlog are in `docs/ROADMAP.md`; settings in `docs/CONFIG.md`; keys in
`docs/KEYMAP.md` (generated); the manual test harness in `docs/TESTING.md`.

## Module map

| Path | Role |
|---|---|
| `main.rs` | Argument parsing (`PATH[:LINE[:COL]]`: a file is rooted at its git work tree, else its directory, and handed to `App::new` as the file to open; with no argument outside a git repository the Recent Projects palette opens once the list loads), logging (`~/.local/state/jetl/jetl.log`, `RUST_LOG=jetl=debug`), `tui::init`, `App::run`, `tui::restore`. `--dump-keymap` prints `docs/KEYMAP.md`. |
| `tui.rs` | Owns terminal modes. Restore order is fixed (keyboard flags, mouse, paste, focus, alt screen, raw mode, cursor); the panic hook restores first. |
| `app/mod.rs` | `App` (all panels and services), `App::new`, the `run` loop, `handle_key`, `perform`, `Ctx` and the `ctx!` macro, tests (`test_app`). |
| `app/msg.rs` | `handle_msg` for every `AppMsg`, git results (`on_git_msg`, `git_op_done`: follow-ups such as the push remote choice, forcing an unmerged branch delete, conflict hints), `git_root_changed`, file open and decode. |
| `app/effects.rs` | `apply_effects`, `refresh_tree`, `git_request`. |
| `app/dialogs.rs` | `request_quit`, `finish_dialog` (one arm per `DialogId` answer), the branches popup's operations (`branch_op`). |
| `app/palette.rs` | Opening the palette, its keys, the file index (walked at startup, on root switches and when a file or symbol palette opens; the symbol index follows each load), the recent projects list, accepted rows. |
| `app/mouse.rs` | Mouse routing: toasts, overlays, drags, splitters, tabs, status bar, panels. |
| `app/watch.rs` | Starting the filesystem watcher and acting on its batches. |
| `app/find.rs` | Replace in files (open tabs through the editor, other files off the main thread). |
| `app/symbols.rs` | Starting symbol index runs (`request_symbols`, through `Coalesce`), applying `AppMsg::Symbols`, re-queuing watcher paths, and `sync_structure` (called by `ui::render` while the Structure window is shown). |
| `app/clipboard.rs` | Copy (OSC 52, the copy tool, jetl's register) and `editor.paste`: the focused text field (`PasteTarget`), the paste tool run off the main thread (`AppMsg::Paste`, dropped when focus moved), the register fallback with a one-time warning. Dialogs and the palette route the paste key here before their own key handling. |
| `app/format.rs` | External formatter runs (`Effect::Format`): one per buffer at a time (a newer request aborts the older one and keeps its save), tagged with the generation and a seq, aborted on root switch and quit; `AppMsg::Formatted` goes to `Editor::format_done`. |
| `app/run.rs` | Run tasks: `RunState` (the running job with its process group id, the last task, the picker's tasks), `run_task` (kills a running task first, spawns, starts `run::process::drive`), stop, rerun, `RunOutput`/`RunExited` checked against the generation and the run's seq, the task list loaded off the main thread for the picker. The job is killed on quit, on root switch and when `RunState` is dropped. |
| `app/terminal.rs` | The Terminal window's shell: started by the first frame that shows the window (`render_terminal`, at that frame's size, in the root; `[terminal] shell`, else `$SHELL`, else `/bin/sh`), `terminal_key` (the `[terminal]` keymap table, Esc Esc at the shell prompt, everything else to the shell or a restart once it ended), paste, `TerminalOutput` checked against the generation and the shell's id. Quitting, a root switch and dropping the app kill the shell's session (`TerminalPanel::reset`); the next one starts lazily in the new root. |
| `app/main_menu.rs` | The main menu bar (`app.main_menu`, F10): built from `ui::menu_bar::MAIN_MENU` with keymap shortcuts (`App::fill_shortcuts`, shared with context menus) and rows that need an active tab disabled when none is open; its keys and mouse; the chosen row runs through `App::perform`. |
| `app/lsp.rs` | Language servers in the app: `LspService` (the current root's `Client`s by server name, a failed one kept so it is not restarted; commands that could not start, reported once and not retried until `lsp.restart`; the documents open on servers; work done progress), `App::lsp_sync` (run by the loop after each batch: starts the server of a buffer's language, `didOpen` once it runs, `didChange` with the buffer's recorded edits, `didClose` for closed buffers, close and reopen after a path or language change), `lsp_saved` (`didSave` after a sync), `lsp_msg` (generation and `ServerId` checked, then state changes, `window/showMessage` to notifications, `$/progress` for the status bar, server requests answered: `workspace/configuration` from `settings`, null for capability registration and progress creation, MethodNotFound otherwise), `lsp_status` (the status bar segment), restart, root switch. Tests install in-process servers with `tests::with_fake_servers`. |
| `app/files.rs` | Tree file operations: `run_fs_op` off the main thread, `fs_op_done` (reload the tree, open, retarget or close tabs). |
| `workspace.rs` | `Workspace` (root and generation), root switching (`App::switch_root`), sessions (JSON per root: tabs in order with their pins, tool window layout and sizes, Git tab, tree hidden/ignored toggles, expanded tree folders; a missing one arrives empty; fields added later are `#[serde(default)]`). `session_loaded` skips tabs already open, applies the layout (sizes clamped, toggles winning over `[files]` config), expands the saved folders on the main thread, then opens the command line file. |
| `run/` | Run tasks. `mod.rs`: `RunTask` (`[[run.tasks]]`, `.jetl/run.toml`; a command is a `sh -c` string or a list of words) and the defaults detected from Cargo, npm, make and Go project files. `process.rs`: `spawn` (own session and process group, stdout and stderr on one pipe, stdin closed), `kill_group` (SIGKILL to the group), `drive` (a reader thread splits, cleans and matches lines; batches go out every 50 ms, then the exit). `matcher.rs`: problem matchers (rustc/cargo with the header's severity, tsc, pytest and tracebacks, generic `path:line:col`) turning lines into `RunLine`s with a severity and a link to an existing file. `panel.rs`: the Run tool window (status line, last 10 000 lines, follow unless scrolled up, links opened through `Effect::OpenFileAt`, next/previous problem). |
| `lsp/` | Language server protocol. `mod.rs`: `LspMsg`, `PendingTag` (what a request was for, carried to its `Response`), `LspRequest` (`Effect::Lsp`), `[lsp]` config with the built-in servers. `transport.rs`: JSON-RPC framing (`Content-Length`) over any `AsyncRead`/`AsyncWrite`, `Incoming`/`Outgoing`, reader and writer tasks. `client.rs`: `spawn_process` (own session, stderr to the log), `Client` (state, `notify`, `request` with a tag, `respond`, `shutdown`: `shutdown` and `exit`, two seconds, then the process group is killed; dropping it kills the group) and the driver task that owns the connection (sends `initialize` and holds everything else until it is answered, keeps the pending requests and fails those older than `request_timeout_ms`, reports a failed start, a startup timeout or an exit). `pos.rs`: char index to UTF-16 `Position` and back, clamped. `sync.rs`: `DocEdit` (one recorded edit in LSP terms), `SyncCaps`, language ids, file URIs. `fake.rs` (tests): an in-process server over `tokio::io::duplex`. |
| `terminal/` | The Terminal tool window (Alt+F12). `pty.rs`: `Session` (portable-pty; the shell leads its own session with the terminal as its controlling one; a reader thread fills an inbox and sends one `AppMsg::TerminalOutput` wake per batch the app has not drained yet, blocking at 1 MiB; a writer thread for input; a waiter thread reports the exit after the last output; `kill` SIGKILLs every process of the session, found through `/proc` on Linux, and runs on drop). `panel.rs`: `TerminalPanel` (the vt100 parser on the UI thread with 5000 lines of scrollback by default, answers to cursor position and device attribute queries, cell rendering with colors, attributes and wide characters, the real cursor while focused, resize to the text area every frame, wheel scrollback or arrow keys on the alternate screen, mouse reports when the program enabled them). `keys.rs`: xterm encoding of keys (application cursor mode), mouse (X10/normal/button/any motion, default, UTF-8 and SGR encodings) and paste (bracketed when enabled). |
| `recent_projects.rs` | `state/recent_projects.json`: roots with their last open time, newest first, deduplicated, at most 30. Recorded off the main thread at startup and on every root switch (`App::load_recent_projects`); a missing or corrupt file reads as empty; roots that are no longer directories are not listed. |
| `worktree/` | `git worktree` parsing and paths (`mod.rs`), the Worktrees tool window (`panel.rs`), the create/remove dialog flow (`flow.rs`, `impl App`). |
| `actions.rs` | `Action` enums and `REGISTRY` (id, description, action). Keymaps and the palette refer to actions by id. |
| `dispatch.rs` | Keys to actions: vim mode table (when modal), then the context stack innermost first, chords with a timeout, double Shift. |
| `config/` | `Config` and `Paths` (`mod.rs`), key parsing and tables (`keymap.rs`, defaults in `assets/keymap.default.toml`), `Theme` (`theme.rs`). |
| `event.rs` | `AppMsg` (background to app), `Effect` (panel to app), `Mouse`, `Level`. |
| `editor/mod.rs` | `Editor`: tabs with MRU, open/close, actions, mouse, rendering of tabs and text. Pinned tabs (`Buffer::pinned`) are always the first ones; moves (Alt+Shift+Left/Right, a drag in the tab bar: `Drag::Tab`, `Editor::drag_tab`) stay within the pinned or the unpinned group. Close others and close all but pinned skip pinned tabs; close all closes them too. `enforce_tab_limit` (`editor.max_tabs`, run by the app after a file opens and after a session restore) closes the least recently used clean, unpinned, inactive tabs. Right-click on a tab opens its context menu. |
| `editor/buffer.rs` | `Buffer`: rope, selection, undo, file metadata (path, line ending, mtime, dirty state). While open on a language server (`lsp_tracked`) every change records a `DocEdit` in `lsp_pending` before the rope changes (at most 512; past that, or for an edit inside a CRLF break, `lsp_full_resync` sends the whole text). |
| `editor/{find,io}.rs` | Find/replace bar (keys, stepping, rendering); saving and autosave. |
| `editor/format.rs` | `[format]` formatters: `FormatRequest` (the buffer snapshot: text, version, undo state), `format_done` (apply if the version is unchanged, warn on failure, then write for a save), and `Buffer::apply_formatted`: an imara-diff line diff turned into one transaction of the changed hunks, the selection mapped by the code before it, the view kept on its lines. |
| `editor/view.rs` | `layout_line`: the grapheme walk shared by rendering, `visual_col` and `col_to_char`. |
| `editor/gutter.rs` | Git marks against the HEAD text (`Buffer::diff_base`), and the change commands built on the same diff: revert the change at the caret (one `Buffer::apply`, so undoable), next/previous change. |
| `editor/blame.rs` | The blame column (`Buffer::blame`): author and date per line from `git::repo::blame` of HEAD, matched to buffer lines through the HEAD diff (recomputed with the gutter at tick rate, so edits shift it and edited lines show none). |
| `editor/…` | `selection`, `undo`, `syntax` (tree-sitter, language detection), `search`, `indent` (`.editorconfig`, detection), `brackets`, `expand`, `nav` (back/forward). |
| `files/tree.rs` | Project tool window: lazy `ignore` walk, `TreeRefresh` off the main thread, git status colors, speed search, context menu. |
| `files/ops.rs` | `FsOp` (create, rename, delete; never overwrites, never follows symlinks on delete) and name checks for the tree dialogs. |
| `files/walk.rs` | The `ignore` walk builder shared by the tree, Go to File and find in files. |
| `files/find.rs`, `files/find_panel.rs` | Find in files: the search (`Search::run` streams `Batch`es, cancelled by seq) and replacements; the Find tool window. |
| `files/watch.rs` | Debounced recursive watch of the root (notify); splits batches into working tree paths and a git flag. Opens and reads are not changes (jetl's own reads would loop). |
| `symbols/mod.rs` | `Symbol` and `extract`: runs a language's tags query (`LanguageDef::tags`, from the grammar crates' `TAGS_QUERY`: Rust, JavaScript, TypeScript/TSX, Python, Go, C, C++, Java) over a tree; definitions only, with nesting depth. |
| `symbols/index.rs` | `SymbolIndex`: the project symbols behind Go to Symbol. Follows the file index (files with tags, at most 5000, each at most 256 KiB), parses in `spawn_blocking` (`IndexJob`), skips files whose mtime is unchanged, drops and re-queues watcher paths. |
| `symbols/panel.rs` | Structure tool window: the active buffer's symbols from its live tree, re-extracted when the buffer version, language or active tab changes; the selection follows the caret while unfocused. |
| `git/mod.rs` | `GitService` (spawns work, tracks busy ops), `GitMsg`, `GitOp`, `GitRequest`. |
| `git/repo.rs` | Synchronous git2 calls; each opens the repository itself: status (with rename detection), blobs, stage/unstage, discard (`checkout_head` for staged paths, `checkout_index` for unstaged ones, new files deleted), commit and amend, branches (local, then remote), the push remote, stashes, blame. |
| `git/cli.rs` | The `git` command line: network, switch, log/show, worktrees, `git apply` of a patch on stdin (`git_with_input`), merge, rebase, cherry-pick, revert, branch and tag commands. No prompts, 120 s timeout. |
| `git/diff.rs` | `DiffDoc`: raw lines of both sides, side-by-side and unified rows, changed blocks, a unified patch of one block (`block_patch`, for hunk staging), and changed characters of modified line pairs (a second imara-diff pass). |
| `git/graph.rs` | `LogGraph`: lays out the Log tab's lanes from each commit's parents (`%P`), carried from page to page. |
| `git/messages.rs` | Commit message history per repository (keyed by its common git dir, so worktrees share it) in `state/commit_messages/`. |
| `git/model.rs` | Shared types: status entries (`old_rel` for renames), `DiffKey`, log rows and `LogFilter`, commit details, `BranchInfo` and `BranchOp`, `Blame`, `StashEntry`. |
| `git/panel/` | The Git tool window: shared state, input routing, header and console (`mod.rs`); the diff pane of every tab (`diff_view.rs`: folded unchanged runs, the current change, syntax colors, changed characters, side-by-side/unified); the Commit tab with amend and message history (`commit.rs`); the Log tab with its filter bar and commit actions (`log.rs`); the Stash tab (`stash.rs`). |
| `ui/` | `render` (frame order), `layout` (rects and hit testing; zen mode, where the editor text fills the screen and `LayoutState::zen` keeps the hidden tool windows and focus: showing any tool window leaves it, and sessions save the hidden windows, never zen itself), `focus` (panel and tool window ids), `dialog`, `palette`, `menu` (context menu, also the main menu's dropdowns; rows can be separators), `menu_bar` (the main menu: the static `MAIN_MENU` table of labels and registry ids, a bar over the top screen row with the open menu's dropdown under its title), `status_bar`, `tabs`, `toasts`, `tool_strip` (the left edge: a main menu button on the top row, then one two-row button per tool window, its Alt-number over `ToolWindowId::icon()`, emoji or one-cell symbols per `[ui] icons`; the hit rects go to `LayoutState::strip_hits` as `StripButton`s, and the hovered one, `App::strip_hover`, gets a one-line tooltip with its name and shortcut), `event_log` (the Event Log tool window over `App::notifications`: first line per entry, full text on Enter, follows new entries while its last row is selected and in view; the status bar notification opens it), `widgets/{list,text_input}`. |
| `util/` | `text` (width helpers), `coalesce` (one background request at a time), `fs` (`write_atomic`, shared by saves and replace in files), `clipboard` (clipboard tool selection from the environment and `[clipboard]`, running them with a 1 s timeout), `format` (running a formatter: stdin to stdout, its own session, the process group killed on timeout or when dropped). |

## Event flow

`App::run` is a `tokio::select!` over three sources, then drains the bus and
redraws when `dirty_frame` is set:

1. **Terminal events** (`EventStream`). Keys go to the open dialog, else the
   main menu bar, else the context menu, else the palette, else the focused Terminal window (which
   keeps only its `[terminal]` keymap table and Esc Esc for jetl and sends
   the rest to the shell), else the find bar, commit message box,
   Find window fields or the tree's speed search (which consume their keys
   and let the rest fall through), else `Dispatcher::dispatch` with the
   context stack from `App::contexts()` (`dialog` | `palette` |
   `editor.find`, focused panel, `global`). An `Action` reaches
   `App::perform`, which hands it to the owning panel. Mouse events go
   through `app/mouse.rs`: an active drag owns all events; otherwise
   `LayoutState::hit_test` picks splitter, strip, status bar, tabs or a
   panel, and the panel gets a `Mouse` with the click count resolved.
2. **The bus** (`mpsc::unbounded_channel` of `AppMsg`). Background work
   reports here: file loads, sessions, the file index, tree listings,
   worktree lists, find in files batches, tree file operations
   (`FsOpDone`), symbol index batches, the recent projects list, clipboard
   reads (`Paste`), formatter results (`Formatted`), Run task output and
   exits and the task picker's list, the Terminal shell's output wakes,
   language server events (`AppMsg::Lsp`), `AppMsg::Git(GitMsg)`, and the
   filesystem watcher
   (`FsChanged`: refresh the tree if a listed directory changed, refresh git
   status, reload clean tabs in place, ask once about tabs with unsaved
   edits). Without a watcher (off, or it failed to start), focus gain
   refreshes instead.
3. **A 100 ms tick**: toast and status bar notification expiry, gutter
   recomputation (debounces typing),
   the busy spinner, the Run window's spinner and elapsed time while a task
   runs, idle autosave, and the terminal's held-back Esc once no second one
   followed.

After the bus is drained (and unless quitting) the loop calls
`App::lsp_sync`, so the edits of one batch of input reach the language
server as one `didChange` per buffer.

Panels never reach into the app. They get a `Ctx` (built by `ctx!(self)`
from disjoint fields so the panel can be borrowed mutably at the same time)
and push `Effect`s: open a file, focus a panel, notify, show a dialog, run a
git request, switch root, refresh. `App::apply_effects` runs them in order
and collects refreshes (tree, git status, branch and worktree lists) to run
once after the batch, so saving ten files re-reads the tree once.

Dialogs are data: `Dialog { id: DialogId, kind }`. The answer comes back to
`App::finish_dialog` as `(DialogId, DialogResult)`. A dialog about a buffer
carries its `BufferId`, never a tab index: tabs can open while it is up.

Render order (`ui::render`): tool strip, left and bottom tool windows,
splitters, editor, status bar, strip tooltip (hidden while an overlay is
open), toasts, palette, context menu, main menu bar (over the top row, in zen
mode too), dialog. Mouse events go to the topmost open overlay (dialog, main
menu bar, context menu, palette) before anything below; the menus also get
motion, for hover. Other motion only updates the strip hover and redraws when
it changes. Rendering stores the rects that
mouse handling reads (layout rects, list rects, button rects).

## Background work and stale results

All blocking work runs in `tokio::task::spawn_blocking` (git2, file reads,
directory walks) or a tokio task (the `git` CLI). The pattern:

1. Clone what the work needs (root, generation, seq, `bus_tx`).
2. Run it off the main thread.
3. Send a tagged `AppMsg`.
4. In `handle_msg`, drop the result when its tag is stale.

Two tags:

- **Generation** (`Workspace::generation`) is bumped by every root switch
  (`Workspace::switch_root`). Every result that depends on the root carries
  it: `FileLoaded`, `SessionLoaded`, `FileIndex`, `TreeLoaded`,
  `Worktrees`, `Find`, `ReplaceDone`, `FsChanged`, `WatcherStarted`,
  `DiskText`, `FsOpDone`, `Symbols`, `Paste`, `Formatted`, `RunOutput`,
  `RunExited`, `RunTasks`, `TerminalOutput`, every `LspMsg`, every
  `GitMsg`. `GitService`
  keeps a copy, set by `set_root(root, generation)`, to tag its own
  results. A result whose generation differs from `workspace.generation` is
  dropped.
- **Seq** orders repeated requests of the same kind within a generation, so
  an older request that finishes late cannot overwrite a newer result:
  `GitMsg::Status` (`GitPanel::set_status`), `GitMsg::Blob` (per buffer,
  `Buffer::diff_base_seq`), `GitMsg::Blame` (per buffer, `BlameState::seq`;
  a buffer whose column was hidden meanwhile drops it), `FileIndex` (`FileIndex::seq`), `TreeLoaded`
  (`ProjectPanel::finish_refresh`), `Find` batches (`FindPanel::on_batch`;
  a newer search also stops the old walk through a shared cancel seq),
  `Symbols` batches (`SymbolIndex::on_batch`; only the newest run applies,
  and it stops older runs through a cancel seq), `RecentProjects`
  (`App::recent_projects_seq`; not tied to the root, so it carries no
  generation), `Paste` (only the newest request applies, and only while
  the text field it was asked for still has focus), `Formatted` (per
  buffer, only the newest request; the text is applied only while
  `Buffer::version` still matches the snapshot), `RunOutput` and
  `RunExited` (only the current run's `RunState::seq`; a new run, a root
  switch or quitting forgets the old one after killing its process group),
  `RunTasks` (the newest load), `TerminalOutput` (only the running shell's
  id, `TerminalPanel::seq`; the wake carries no bytes, it tells the app to
  drain the shell's inbox, so there is at most one per shell on the bus and
  a burst of output is parsed and drawn once), `LspMsg` (the `ServerId`
  of the current client for that server name: a restarted or stopped
  server's late messages are dropped; requests carry a `PendingTag` with
  the buffer, its version and a seq to check the answer against). A result is applied when its seq is not
  older than the last applied one. Results keyed by what they describe
  need no seq: diffs by `DiffKey` (each diff pane ignores keys it does not
  show), commit details by sha (the Log or Stash tab, whichever has it
  selected), log pages by `skip` and their `LogFilter` (a page of a
  replaced filter is dropped). Stash operations name the stash by its
  commit, not its index, so a list that changed meanwhile cannot hit the
  wrong entry.

New background work follows the same pattern: a generation always, a seq
when the request can repeat while one is in flight. Requests that bursts can
trigger (git status, tree refresh, symbol index runs) also go through
`util::Coalesce`: at most one runs, a request meanwhile runs once after it,
and a root switch resets it. A long run that reports in batches calls
`Coalesce::touch` so it is not taken for lost. `GitMsg::OpDone` clears
its busy id before the generation check, so the spinner never sticks.

## Conventions

- All buffer mutation goes through `Buffer::apply(changes, selection,
  kind)`, which records undo. `rope` is a public field for reading only.
  Reloading from disk is an `apply` too, so it can be undone.
- Rendering and mouse mapping in the editor share one `layout_line()` walk
  (`editor/view.rs`); a test checks they agree.
- A git2 `Repository` is opened inside the blocking call (`git/repo.rs`) and
  never stored in `App`.
- Network, worktree and history-rewriting operations (merge, rebase,
  cherry-pick, revert, branch and tag commands, `git apply` of hunk
  patches) go through `git/cli.rs`: stdin closed (or carrying the patch),
  no terminal prompt, `GIT_EDITOR=true`, `GIT_SSH_COMMAND` with
  `BatchMode=yes` for network subcommands, each child in its own session
  (`setsid`), 120 s timeout. Reads, staging, discard, commit and amend,
  stash and blame use git2.
- Destructive git operations (discard of files or a hunk, stash drop,
  branch delete, revert, rebase, tag delete) ask in a confirm dialog first;
  the request that runs them is sent from `App::finish_dialog`.
- No `dyn Panel`. Each panel has `handle_action(&XAction, &mut Ctx) -> bool`,
  `handle_mouse(&Mouse, Rect, &mut Ctx) -> bool` and `render(&mut Frame,
  Rect, focused, &Theme)`.
- `impl App` blocks may live outside `app/` when they belong to a feature
  (`workspace.rs`, `worktree/flow.rs`). Methods only the app uses are
  `pub(super)`.
- Saves are synchronous and atomic (`write_atomic`: temp file, fsync,
  rename, permissions kept). An explicit save of a file changed on disk asks
  first (`DialogId::OverwriteExternal`); autosave skips it. The exception is
  an explicit save (`editor.save`, `editor.save_all`) with `[format]
  on_save` and a formatter for the language: the cleanup runs, the
  formatter runs off the main thread, and `Editor::format_done` writes when
  it answers (formatted, unformatted after a failure, or the snapshot when
  the buffer changed meanwhile). Saves that must finish at once (closing a
  tab, quitting, switching roots, the overwrite answer, autosave) call
  `Editor::save`/`save_all`, which never format.
- Child processes that can outlive a request (formatters, Run tasks, the
  Terminal shell, language servers) start in their own session, so they never read from or
  draw over jetl's terminal, and are killed with what they started (the
  process group; for the shell, every process of its session): on quit,
  on a root switch and when their owner is dropped; a formatter also on
  its timeout or a newer request for the same buffer, a Run task when
  another one starts. Language servers are asked to shut down first (two
  seconds on quit, in the background on a root switch or restart). Clipboard tools are killed after 1 s. Their
  results follow the stale-result rules above.
- Lists keep their wheel scroll: `ListState::set_viewport` and `fit` bound
  the offset without pulling the view back to the selection.

## Extension points

**A new action.** Add the variant to its enum in `actions.rs`, a `REGISTRY`
entry with an id and description, a default binding in
`assets/keymap.default.toml` (plus a legacy fallback when the key needs the
kitty protocol), handle it in the panel's `handle_action`, then regenerate
the keymap doc: `cargo run -q -- --dump-keymap > docs/KEYMAP.md` (a test
fails until it matches).

**A new tool window.** In `ui/focus.rs` add a `ToolWindowId` at the end
(sessions store it; also to `ALL`, `panel`, `side`, `title`, `number`) and
a `PanelId` (`tool_window`, `context`). Add the panel to `App` and construct it in `App::new`; render it
in `ui::render` on its side; route its mouse in `App::forward_mouse`; send
its actions from `App::perform`; add an `[<context>]` table to the default
keymap and a `tool_window.<name>` registry entry
(`AppAction::ToggleToolWindow`) bound to its Alt+number key (or another
key; `number` is then `None`).

**A new dialog.** Add a `DialogId` variant (carry a `BufferId` or path,
never a tab index), open it with `Effect::Dialog(Dialog::confirm|input|choice
(...))`, and add an arm to `App::finish_dialog`.

**A new palette source.** Add a `Source` (label, rows, ranking in
`refilter`), map the `PaletteKind`s that show it in `Palette::new`, feed it
from `App::open_palette` (cached data first, then a refresh whose result
calls a `set_*` on the open palette), and add an `Accept` variant handled in
`App::finish_palette`.

**A language server feature.** Advertise it in
`lsp::client::client_capabilities`, add a `PendingTag` variant carrying what
the answer must still match (buffer id, `Buffer::version`, a seq), send with
`Client::request` once the client is `Running`, and handle the tag in
`App::lsp_msg`: deserialize the `Value` there, drop it when the buffer
version or seq moved on, convert positions with `lsp::pos`. Test against
`lsp::fake` through `app::lsp::tests::with_fake_servers`.

**New background work.** See the stale-results rules above: tag, send an
`AppMsg`, check the tag in `handle_msg`.

## Pitfalls

- Mouse capture blocks native selection; Shift+drag passes through in foot,
  alacritty and kitty. Copy uses OSC 52 (`set -s set-clipboard on` in tmux)
  plus `wl-copy`/`xclip`/`xsel` when found; Ctrl+V reads the clipboard
  through `wl-paste`/`xclip`/`xsel`, the terminal's paste shortcut through
  bracketed paste.
- Without the kitty keyboard protocol (tmux, most legacy terminals)
  Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter, Shift+Enter and double Shift do
  not arrive; keep legacy fallbacks in the default keymap.
- In raw mode Ctrl+C is copy and Ctrl+Z is undo.
- `Event::Resize` sets `needs_clear`; the next frame clears the terminal.
- The Terminal window takes Ctrl+Q, Ctrl+S, Ctrl+W and other global keys
  for the shell; only its `[terminal]` table (Alt+F12, Ctrl+Shift+Up/Down,
  Ctrl+V) and Esc Esc at the shell prompt stay with jetl. Shift keeps the
  mouse for jetl when the program asked for mouse reports; Shift+drag still
  selects natively in most terminals.
- ropey counts `\r\n` as one line break; the buffer keeps its line ending
  and writes it back. Stay on ropey 1.x.
- Grammar C code is slow to build cold; iterate with
  `--no-default-features --features lang-rust`.
- `git2` is built without default features, linking the system libgit2
  without OpenSSL or libssh2; anything that talks to a remote uses the CLI.
