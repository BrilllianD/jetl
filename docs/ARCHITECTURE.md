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
| `main.rs` | Argument parsing, logging (`~/.local/state/jetl/jetl.log`, `RUST_LOG=jetl=debug`), `tui::init`, `App::run`, `tui::restore`. `--dump-keymap` prints `docs/KEYMAP.md`. |
| `tui.rs` | Owns terminal modes. Restore order is fixed (keyboard flags, mouse, paste, focus, alt screen, raw mode, cursor); the panic hook restores first. |
| `app/mod.rs` | `App` (all panels and services), `App::new`, the `run` loop, `handle_key`, `perform`, `Ctx` and the `ctx!` macro, tests (`test_app`). |
| `app/msg.rs` | `handle_msg` for every `AppMsg`, git results (`on_git_msg`, `git_op_done`), `git_root_changed`, file open and decode. |
| `app/effects.rs` | `apply_effects`, `refresh_tree`, `git_request`. |
| `app/dialogs.rs` | `request_quit`, `finish_dialog` (one arm per `DialogId` answer). |
| `app/palette.rs` | Opening the palette, its keys, the file index, accepted rows. |
| `app/mouse.rs` | Mouse routing: toasts, overlays, drags, splitters, tabs, status bar, panels. |
| `workspace.rs` | `Workspace` (root and generation), root switching (`App::switch_root`), sessions (JSON per root). |
| `worktree/` | `git worktree` parsing and paths (`mod.rs`), the Worktrees tool window (`panel.rs`), the create/remove dialog flow (`flow.rs`, `impl App`). |
| `actions.rs` | `Action` enums and `REGISTRY` (id, description, action). Keymaps and the palette refer to actions by id. |
| `dispatch.rs` | Keys to actions: vim mode table (when modal), then the context stack innermost first, chords with a timeout, double Shift. |
| `config/` | `Config` and `Paths` (`mod.rs`), key parsing and tables (`keymap.rs`, defaults in `assets/keymap.default.toml`), `Theme` (`theme.rs`). |
| `event.rs` | `AppMsg` (background to app), `Effect` (panel to app), `Mouse`, `Level`. |
| `editor/mod.rs` | `Editor`: tabs with MRU, open/close, actions, mouse, rendering of tabs and text. |
| `editor/buffer.rs` | `Buffer`: rope, selection, undo, file metadata (path, line ending, mtime, dirty state). |
| `editor/{find,io}.rs` | Find/replace bar (keys, stepping, rendering); saving, autosave, `write_atomic`. |
| `editor/view.rs` | `layout_line`: the grapheme walk shared by rendering, `visual_col` and `col_to_char`. |
| `editor/…` | `selection`, `undo`, `syntax` (tree-sitter, language detection), `search`, `gutter` (git marks), `indent` (`.editorconfig`, detection), `brackets`, `expand`, `nav` (back/forward). |
| `files/tree.rs` | Project tool window: lazy `ignore` walk, `TreeRefresh` off the main thread, git status colors. |
| `git/mod.rs` | `GitService` (spawns work, tracks busy ops), `GitMsg`, `GitOp`, `GitRequest`. |
| `git/repo.rs` | Synchronous git2 calls; each opens the repository itself. |
| `git/cli.rs` | The `git` command line: network, switch, log/show, worktrees. No prompts, 120 s timeout. |
| `git/{diff,model,panel}.rs` | Diff rows, shared types, the Git tool window (Commit, Log, Console). |
| `ui/` | `render` (frame order), `layout` (rects and hit testing), `focus` (panel and tool window ids), `dialog`, `palette`, `status_bar`, `tabs`, `toasts`, `tool_strip`, `widgets/{list,text_input}`. |
| `util/text.rs` | Width helpers. |

## Event flow

`App::run` is a `tokio::select!` over three sources, then drains the bus and
redraws when `dirty_frame` is set:

1. **Terminal events** (`EventStream`). Keys go to the open dialog, else the
   palette, else the find bar or commit message box (which consume editing
   keys and let the rest fall through), else `Dispatcher::dispatch` with the
   context stack from `App::contexts()` (`dialog` | `palette` |
   `editor.find`, focused panel, `global`). An `Action` reaches
   `App::perform`, which hands it to the owning panel. Mouse events go
   through `app/mouse.rs`: an active drag owns all events; otherwise
   `LayoutState::hit_test` picks splitter, strip, status bar, tabs or a
   panel, and the panel gets a `Mouse` with the click count resolved.
2. **The bus** (`mpsc::unbounded_channel` of `AppMsg`). Background work
   reports here: file loads, sessions, the file index, tree listings,
   worktree lists, and `AppMsg::Git(GitMsg)`.
3. **A 100 ms tick**: toast expiry, gutter recomputation (debounces typing),
   the busy spinner, idle autosave.

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
splitters, editor, status bar, toasts, palette, dialog. Rendering stores the
rects that mouse handling reads (layout rects, list rects, button rects).

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
  `Worktrees`, every `GitMsg`. `GitService` keeps a copy, set by
  `set_root(root, generation)`, to tag its own results. A result whose
  generation differs from `workspace.generation` is dropped.
- **Seq** orders repeated requests of the same kind within a generation, so
  an older request that finishes late cannot overwrite a newer result:
  `GitMsg::Status` (`GitPanel::set_status`), `GitMsg::Blob` (per buffer,
  `Buffer::diff_base_seq`), `FileIndex` (`FileIndex::seq`), `TreeLoaded`
  (`ProjectPanel::finish_refresh`). A result is applied when its seq is not
  older than the last applied one. Results keyed by what they describe
  (diffs by `DiffKey`, commit details by sha, log pages by `skip`) need no
  seq.

New background work follows the same pattern: a generation always, a seq
when the request can repeat while one is in flight. `GitMsg::OpDone` clears
its busy id before the generation check, so the spinner never sticks.

## Conventions

- All buffer mutation goes through `Buffer::apply(changes, selection,
  kind)`, which records undo. `rope` is a public field for reading only.
  Reloading from disk is an `apply` too, so it can be undone.
- Rendering and mouse mapping in the editor share one `layout_line()` walk
  (`editor/view.rs`); a test checks they agree.
- A git2 `Repository` is opened inside the blocking call (`git/repo.rs`) and
  never stored in `App`.
- Network and worktree operations go through `git/cli.rs`: stdin closed, no
  terminal prompt, `GIT_SSH_COMMAND` with `BatchMode=yes` for network
  subcommands, each child in its own session (`setsid`), 120 s timeout.
- No `dyn Panel`. Each panel has `handle_action(&XAction, &mut Ctx) -> bool`,
  `handle_mouse(&Mouse, Rect, &mut Ctx) -> bool` and `render(&mut Frame,
  Rect, focused, &Theme)`.
- `impl App` blocks may live outside `app/` when they belong to a feature
  (`workspace.rs`, `worktree/flow.rs`). Methods only the app uses are
  `pub(super)`.
- Saves are synchronous and atomic (`write_atomic`: temp file, fsync,
  rename, permissions kept). An explicit save of a file changed on disk asks
  first (`DialogId::OverwriteExternal`); autosave skips it.
- Lists keep their wheel scroll: `ListState::set_viewport` and `fit` bound
  the offset without pulling the view back to the selection.

## Extension points

**A new action.** Add the variant to its enum in `actions.rs`, a `REGISTRY`
entry with an id and description, a default binding in
`assets/keymap.default.toml` (plus a legacy fallback when the key needs the
kitty protocol), handle it in the panel's `handle_action`, then regenerate
the keymap doc: `cargo run -q -- --dump-keymap > docs/KEYMAP.md` (a test
fails until it matches).

**A new tool window.** In `ui/focus.rs` add a `ToolWindowId` (to `ALL`,
`panel`, `side`, `title`, `number`) and a `PanelId` (`tool_window`,
`context`). Add the panel to `App` and construct it in `App::new`; render it
in `ui::render` on its side; route its mouse in `App::forward_mouse`; send
its actions from `App::perform`; add an `[<context>]` table to the default
keymap and a `tool_window.<name>` registry entry
(`AppAction::ToggleToolWindow`) bound to its Alt+number key.

**A new dialog.** Add a `DialogId` variant (carry a `BufferId` or path,
never a tab index), open it with `Effect::Dialog(Dialog::confirm|input|choice
(...))`, and add an arm to `App::finish_dialog`.

**A new palette source.** Add a `Source` (label, rows, ranking in
`refilter`), map the `PaletteKind`s that show it in `Palette::new`, feed it
from `App::open_palette` (cached data first, then a refresh whose result
calls a `set_*` on the open palette), and add an `Accept` variant handled in
`App::finish_palette`.

**New background work.** See the stale-results rules above: tag, send an
`AppMsg`, check the tag in `handle_msg`.

## Pitfalls

- Mouse capture blocks native selection; Shift+drag passes through in foot,
  alacritty and kitty. Copy uses OSC 52 (`set -s set-clipboard on` in tmux).
- Without the kitty keyboard protocol (tmux, most legacy terminals)
  Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter, Shift+Enter and double Shift do
  not arrive; keep legacy fallbacks in the default keymap.
- In raw mode Ctrl+C is copy and Ctrl+Z is undo.
- `Event::Resize` sets `needs_clear`; the next frame clears the terminal.
- ropey counts `\r\n` as one line break; the buffer keeps its line ending
  and writes it back. Stay on ropey 1.x.
- Grammar C code is slow to build cold; iterate with
  `--no-default-features --features lang-rust`.
- `git2` is built without default features, linking the system libgit2
  without OpenSSL or libssh2; anything that talks to a remote uses the CLI.
