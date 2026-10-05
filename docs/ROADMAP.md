# jetl roadmap

This is the feature backlog for jetl after phases 0-5. Each item below is also tracked as a yman
task (`yman ls`); phases are umbrella tasks and the rows are their subtasks. Phases 6 to 11 are
done (see Order).

## What exists today

- **Editor:** multiple carets (`Selections`, edits at every caret through `Buffer::edit_carets`) and
  column selection (`view::block_selections`). Also there:
  undo grouping, incremental tree-sitter reparse (`Syntax::tree()` feeds brackets, expand selection and the
  Structure window), smart
  Home, word motions, duplicate/delete line, line comment toggle, CRLF detection, atomic save, and
  a 2 MiB highlight threshold. Phase 6 added bracket logic, indent/dedent of a selection,
  move-line and autosave; open files reload or ask when they change on disk. Phase 9 added
  format on save through an external formatter (`[format]`), pinned tabs, tab reorder and a tab
  limit that closes the least recently used clean, unpinned tab. Phase 11 added split panes:
  nested tab groups whose tabs are views over shared buffers, then multiple carets, column
  selection, code folding (per view, through `editor/rows.rs`) and soft wrap (`editor/wrap.rs`,
  segments in `Rows`). The three-pane conflict resolver (`editor/merge.rs`) lays its panes out
  in the same groups.
- **Shell:** fixed layout with one left slot and one bottom slot. The side each tool window lives
  on is hardcoded in `ToolWindowId::side()`. A new bottom tool window is just a new enum variant
  and needs no layout rewrite. The palette has Files/Symbols/Actions/Everywhere/Recent/Branches/Recent Projects modes.
  Dialogs come in confirm, input and choice kinds; the Project tree has a right-click context
  menu. The session stores tabs (with their pins), carets, bookmarks, folds, the tool window layout and
  expanded tree folders. Phase 9 added the bottom tool windows Terminal (a PTY shell with vt100
  emulation), Run (tasks from config or detected project files, problem matchers with
  click-to-jump) and Event Log (the 200 entries of `App.notifications`), zen mode, keyboard
  resize of tool windows, and paste from the system clipboard (`wl-paste`/`xclip`/`xsel`).
  After it came an F10 application menu bar, and a compact tool strip: a menu button, then
  number and icon buttons with the name and shortcut in a hover tooltip.
- **Git:** files or single changes (hunks) are staged, unstaged and discarded; renames show as
  `R old -> new`. Commits can amend HEAD, and earlier messages are recalled per repository.
  Gutter marks are based on HEAD; the change at the caret can be reverted, changes are stepped
  through, and a blame column shows author and date. The diff marks changed characters, is
  syntax highlighted, folds unchanged lines and switches between side-by-side and unified. The
  log draws its own graph from parents, filters by branch, text, author and path, shows file
  history, and runs commit actions (checkout, branch, cherry-pick, revert, tags). There is a
  Stash tab, and the branches popup checks out, merges, rebases, renames and deletes branches.
  Conflicted files (`U`) open in a three-pane resolver (yours, result, theirs). A filesystem
  watcher refreshes the tree and git status; focus gain refreshes only when the watcher is off
  (`[files] watch`) or failed to start.
- **Infra:** an LSP client (Phase 10): servers per language and root, JSON-RPC on
  tokio, incremental document sync in UTF-16, `[lsp]` config, a status bar segment;
  diagnostics in the gutter, the text and a Problems window; go to definition, usages and
  hover documentation; completion with a popup; rename, context actions and workspace edits
  (open files one undo step each, the rest edited on disk), file structure, and formatting
  through the server when `[format]` has no command.
  Background services follow one pattern: clone (root, generation, tx), spawn, send a tagged
  message, and the app drops stale generations.
  `docs/ARCHITECTURE.md`, a README and CI (fmt, clippy, test) exist, with render snapshot
  tests. A panic writes the unsaved buffers to a recovery directory, and the next session load
  of that project offers to restore them (`recovery.rs`, `app/recovery.rs`).

## Feature catalog

Effort: S = hours to a day, M = 1-3 days, L = about a week, XL = multiple weeks.

### A. Editor ergonomics

| Feature | Effort | Notes |
|---|---|---|
| Bracket matching highlight + jump (Ctrl+Shift+M) | S | Walk `Syntax::tree()` node at caret; char scan fallback without a grammar |
| Auto-close brackets/quotes, skip-over on closing char, delete pair on Backspace | S | Pure `Buffer` transaction, config `editor.auto_close` |
| Language-aware indent on Enter (after `{`, `:`, `(`; dedent on `}`) | M | tree-sitter node kinds per `LanguageDef`; copy-indent stays as fallback |
| Tab / Shift+Tab indent and dedent a multi-line selection | S | Today Tab replaces the selection |
| Move line/selection up/down, join lines, toggle case, delete word forward | S | One transaction each |
| Expand/shrink selection by syntax node (Ctrl+Alt+W / Ctrl+Alt+Shift+W) | S | tree-sitter node ancestors; Ctrl+W is close tab here |
| Highlight other occurrences of the word under the caret | S | Reuse `search.rs` matcher on the visible range only |
| Block comment toggle (Ctrl+Shift+/) | S | `block_comment` on `LanguageDef` |
| Strip trailing whitespace and ensure final newline on save | S | In `save()`, as an undoable transaction |
| Hard tabs, indent auto-detection, `.editorconfig` | M | Per-buffer indent settings |
| Word wrap | M | Done: `editor/wrap.rs` on the shared `layout_line()` walk, segments in `Rows`; follow-ups: extra continuation indent setting, per-file persistence |
| Column (block) selection | M | Done: `view::block_selections`, `DragMode::Column`; wrap must go through `Buffer::blocks_supported` |
| Multiple carets | L | Done: `Selections`, `Buffer::edit_carets` |
| Code folding by tree-sitter node | M | Done: `editor/fold.rs` (`Folds` per view), visible rows through `editor/rows.rs`; wrap extends `Rows` with segments |
| Back/forward caret navigation, bookmarks | S-M | Position ring in `Editor`; bookmarks persisted in session |
| Numbered bookmarks (Ctrl+F11, Ctrl+0..9) | S | Done: `editor/bookmarks.rs`, a digit per `Buffer::bookmarks` entry, unique per session; closed files keep theirs; legacy fallback Alt+B then the digit |
| Line-ending conversion, BOM preservation, language override from the status bar | S | Status bar gains language/indent/encoding segments |
| Redo caret mismatch after delete_line/duplicate_line/toggle_comment | S | Bug: `sel` is set after `apply` |
| Autosave on focus loss / idle | S | `editor.autosave = "off" \| "focus" \| "idle"` |

### B. Languages

| Feature | Effort | Notes |
|---|---|---|
| Grammars: C#, YAML, HTML, CSS, SCSS, XML, then Go, C, C++, Java, PHP, Lua, Dockerfile, Vue | S each | One `LanguageDef` + `lang-*` feature each |
| Injections (Markdown code blocks, `<script>`/`<style>` in Vue/HTML) | M | Second parser per injection range |
| Shebang / modeline detection | S | `detect()` is extension-only today |

### C. Project-wide navigation and files

All of section C shipped in Phase 7.

| Feature | Effort | Notes |
|---|---|---|
| Find in files (Ctrl+Shift+F), replace in files | M | `grep-searcher` + `grep-regex` over the `ignore` walk; new bottom tool window |
| Symbol search and Structure tool window | M | Grammar crates ship `TAGS_QUERY` |
| Filesystem watcher: auto-refresh tree and git, external edit reload, save conflict check | M | `notify` 9 + `notify-debouncer-full`; replaces focus-gain polling |
| File tree operations: new/rename/delete/copy path/filter/hidden toggle | M | Dialogs exist |
| Right-click context menus | M | New overlay widget reusing `ListState` |
| `jetl file.rs:12`, open a file and root at its repository | S | `main.rs` argument parsing |
| Recent projects palette | S | Reuse `switch_root` |
| Persist layout in session | S | `Session` fields with `#[serde(default)]` |

### D. Git depth

All of section D shipped in Phase 8; the conflict resolver came after split panes (Phase 11).

| Feature | Effort | Notes |
|---|---|---|
| Discard changes for a file / selection | S | git2 `checkout_head` with path |
| Amend and commit message history | S | git2 amend |
| Hunk stage/unstage, gutter revert, next/prev change | M | Patch from `DiffDoc` rows through `git apply --cached` |
| Intra-line diff, syntax in diff, layout toggle, collapse unchanged | M | Second imara-diff pass on chars |
| Stash list/push/pop/apply/drop | M | git2 stash API |
| Blame gutter | M | git2 blame in `spawn_blocking` |
| Log filters, file history, commit actions | M | Extend `cli::log`; actions through `GitOp` |
| Branch delete/rename/remote/merge/rebase, tags | M | CLI for merge/rebase |
| Conflict resolver (3-pane merge) | L | Done: a temporary layout of split panes over scratch buffers (`editor/merge.rs`) |

### E. Shell, terminal, run

All of section E shipped in Phase 9 except split editor panes, which shipped in Phase 11.

| Feature | Effort | Notes |
|---|---|---|
| Built-in terminal tool window | L | `portable-pty` + `vt100` (or `tui-term`) |
| Run/task window with problem matchers | M | tokio process + line parser |
| Format on save via external formatter | S-M | `[format]` config per language |
| Split editor panes | L | Done (Phase 11): tab groups with independent views over shared buffers |
| Zen / maximize editor | S | |
| Event Log tool window | S | `App.notifications` already holds 200 entries |
| Tab reorder, pin, limit | S | |
| Keyboard resize of tool windows | S | `LayoutState` setters exist |
| Wayland clipboard paste fallback | S | `wl-paste` / `xclip` |

### F. Language intelligence (LSP)

| Step | Effort | Notes |
|---|---|---|
| LSP client core | L | Done: `lsp-types`, own JSON-RPC layer on tokio, UTF-16 positions, `[lsp]` config |
| Diagnostics + Problems window | M | Done: gutter glyphs, underlines, status bar counts, F2, Problems window (Alt+6) |
| Go to definition, references, hover | M | Done: Ctrl+B and Ctrl+click, Alt+F7 usages with previews, Alt+Q hover popup |
| Completion popup | L | Done: auto-trigger and Ctrl+Space, fuzzy filter, snippets flattened, additional edits, resolve |
| Rename, code actions, formatting, document symbols | M | Done: Shift+F6 rename across files, Alt+Enter context actions with resolve and `workspace/applyEdit`, Ctrl+F12 file structure, formatting through the server when `[format]` has no command |

### G. Themes, vim, config

| Feature | Effort | Notes |
|---|---|---|
| Named themes, built-in light theme, 256-color fallback | S-M | Done: `ui.theme` darcula/light/theme file, `theme.toml` on top, 256-color fallback (`$COLORTERM`, `ui.colors`) |
| Config hot reload | S | Done: config, keymap and theme files reload through a config-dir watch; broken files keep the current value; restart-needed keys are named |
| Vim mode | L | Dispatcher mode tables exist |
| Local History | M | Reuse `DiffDoc` view |
| Scratch files | S | Done: File > New Scratch File… (Ctrl+Alt+Shift+Insert) picks a language and opens `scratch_<n>.<ext>` in `state/scratches/`; File > Scratch Files… lists them (Shift+Delete deletes); no language server for them |

### H. Quality and distribution

| Feature | Effort | Notes |
|---|---|---|
| `docs/ARCHITECTURE.md`, README | S | Both done |
| Render snapshot tests (`TestBackend` + `insta`) | M | `test_app()` harness exists |
| CI, release binaries, AUR | S | CI done (GitHub Actions, Bitbucket Pipelines); local release via `scripts/package.sh` (tarball + Arch package); remote releases and AUR open |
| Crash recovery of dirty buffers | S | Done: the panic hook writes the dirty buffers to `state/recovery/<pid>-<secs>/` (bounded `try_lock`, never blocks); the next session load of that root offers Restore all / Discard / Review…; restored text opens unsaved and undoable, no autosave until an explicit save; Save As (Alt+Shift+S) saves the untitled ones |
| Palette filter off the main thread | S | nucleo supports it |

## Order

1. **Phase 6: editor ergonomics + grammars.** Done. This phase covers the small items in A and the
   grammars in B. It carries no architectural risk and is felt in daily use right away.
2. **Phase 7: project-wide.** Done. The watcher came first, then find and replace in files, tree
   operations with context menu and speed search, `file:line` CLI, layout persistence, recent
   projects, and symbol search with the Structure window (pulled forward from Phase 10).
3. **Phase 8: git depth.** Done. The 3-pane conflict resolver came last, on split panes
   (Phase 11). Discard, amend and message history, hunk staging and gutter revert, intra-line and
   syntax-highlighted diff, stash, blame, log filters and file history with commit actions, branch
   management with merge, rebase and tags; also rename detection, the push remote choice and a
   log graph that keeps its lanes across pages.
4. **Phase 9: terminal and run.** Done. The built-in Terminal window, the Run window with tasks
   and problem matchers, format on save through an external formatter, the Event Log window, zen
   mode, tab reorder, pins and a tab limit, keyboard resize of tool windows, and paste from the
   system clipboard.
5. **Phase 10: language intelligence.** Done. tree-sitter symbols shipped in Phase 7. The LSP
   core came first, then diagnostics and navigation, completion, and last rename, code actions,
   file structure and formatting through the server.
6. **Phase 11: editor layout.** Done. Split panes, multi-caret, column selection, folding and soft
   wrap; it had the highest implementation risk, so it went last. The conflict resolver from
   Phase 8 followed split panes.
7. **Ongoing:** themes, vim mode, local history, snapshot tests, CI, README.
