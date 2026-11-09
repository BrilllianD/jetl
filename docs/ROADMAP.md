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
- **Shell:** tool windows dock in six places (Left Top, Left Bottom, Bottom Left, Bottom Right,
  Right Top, Right Bottom), two to a side with a splitter, and move through the strip button's
  Move to menu, View | Move Tool Window to … or a drag. A new tool window is a new enum variant
  and needs no layout rewrite. The palette has Files/Symbols/Actions/Everywhere/Recent/Branches/Recent Projects modes.
  Dialogs come in confirm, input and choice kinds; the Project tree has a right-click context
  menu. The session stores tabs (with their pins), carets, bookmarks, folds, the tool window layout and
  expanded tree folders. Phase 9 added the bottom tool windows Terminal (a PTY shell with vt100
  emulation), Run (tasks from config or detected project files, run configurations, problem matchers with
  click-to-jump, a test tree) and Event Log (the 200 entries of `App.notifications`), zen mode, keyboard
  resize of tool windows, and paste from the system clipboard (`wl-paste`/`xclip`/`xsel`).
  After it came an F10 application menu bar, and a compact tool strip: a menu button, then
  number and icon buttons with the name and shortcut in a hover tooltip.
- **Git:** files or single changes (hunks) are staged, unstaged and discarded; renames show as
  `R old -> new`. Commits can amend HEAD, and earlier messages are recalled per repository.
  Gutter marks are based on HEAD; the change at the caret can be reverted, changes are stepped
  through, and a blame column shows author and date. The diff marks changed characters, is
  syntax highlighted, folds unchanged lines and switches between side-by-side and unified. The
  log (branches, remote branches, tags and HEAD by default) draws its own graph from parents, filters by branch, text, author and path, shows file
  history, and runs commit actions (checkout, branch, cherry-pick, revert, tags); the commit menu has
  per-branch operations and Show Diff (the diff of one commit or of several selected ones with Shift+Up/Down or Shift/Ctrl+click, in an editor tab, F7 steps through changes). There is a Stash tab, and the branches popup checks out, compares, merges, rebases,
  pushes, renames and deletes branches.
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
| Move line/selection up/down, join lines, toggle case, delete word forward | S | One transaction each; Sort Lines, Reverse Lines, To Upper/Lower Case (Edit menu, unbound) done |
| Expand/shrink selection by syntax node (Ctrl+Alt+W / Ctrl+Alt+Shift+W) | S | tree-sitter node ancestors; Ctrl+W is close tab here |
| Highlight other occurrences of the word under the caret | S | Reuse `search.rs` matcher on the visible range only |
| Indent guides | S-M | Done: │ at indent stops in leading whitespace (editor/view.rs render callback), continued through blank lines (min of the neighbours' indent, 100-line scan), theme style indent_guide, editor.indent_guides (Live) |
| Block comment toggle (Ctrl+Shift+/) | S | `block_comment` on `LanguageDef` |
| Strip trailing whitespace and ensure final newline on save | S | In `save()`, as an undoable transaction |
| Hard tabs, indent auto-detection, `.editorconfig` | M | Per-buffer indent settings |
| Word wrap | M | Done: `editor/wrap.rs` on the shared `layout_line()` walk, segments in `Rows`; extra continuation indent done (`editor.soft_wrap_extra_indent`); per-file toggle persists in the session (`SessionView::wrap`) |
| Column (block) selection | M | Done: `view::block_selections`, `DragMode::Column`; wrap must go through `Buffer::blocks_supported` |
| Multiple carets | L | Done: `Selections`, `Buffer::edit_carets` |
| Code folding by tree-sitter node | M | Done: `editor/fold.rs` (`Folds` per view), visible rows through `editor/rows.rs`; wrap extends `Rows` with segments |
| Back/forward caret navigation, bookmarks | S-M | Position ring in `Editor`; bookmarks persisted in session |
| Numbered bookmarks (Ctrl+F11, Ctrl+0..9) | S | Done: `editor/bookmarks.rs`, a digit per `Buffer::bookmarks` entry, unique per session; closed files keep theirs; legacy fallback Alt+B then the digit |
| Line-ending conversion, BOM preservation, language override from the status bar | S | Status bar gains language/indent/encoding segments |
| Redo caret mismatch after delete_line/duplicate_line/toggle_comment | S | Done: fixed in yman 54 |
| Autosave on focus loss / idle | S | `editor.autosave = "off" \| "focus" \| "idle"` |
| Live templates (snippets) | M | Done: `config/snippets.rs` (built-ins in `assets/snippets/`, user `snippets/<language>.toml` and `all.toml`), completion rows after the server's, Tab after an exact prefix (`editor.snippet_prefix`), not in strings or comments, through the LSP snippet path; hot reload of the folder, `editor.snippets` (Live), Settings row with the folder, snippet variables (`TM_*`, `CURRENT_*`, `UUID`, ...); Surround With (Ctrl+Alt+T) |

### B. Languages

| Feature | Effort | Notes |
|---|---|---|
| Grammars: C#, YAML, HTML, CSS, SCSS, XML, then Go, C, C++, Java, PHP, Lua, Dockerfile, Vue | S each | One `LanguageDef` + `lang-*` feature each; Vue uses the HTML grammar with injections (`<script>`, `<style>`, `lang` attribute) |
| Injections (Markdown code blocks, `<script>`/`<style>` in Vue/HTML) | M | Done: `editor/syntax.rs` injections, a second parser over included ranges; folds and brackets from the inner tree; only edited blocks re-parse |
| Shebang / modeline detection | S | Done: `detect_text()` falls back to the shebang, then a vim or emacs modeline |

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
| Hunk stage/unstage, gutter revert, next/prev change | M | Patch from `DiffDoc` rows through `git apply --cached`; single lines too (`lines_patch`, run 13) |
| Intra-line diff, syntax in diff, layout toggle, collapse unchanged | M | Second imara-diff pass on chars |
| Stash list/push/pop/apply/drop | M | git2 stash API |
| Blame gutter | M | git2 blame in `spawn_blocking` |
| Log filters, file history, commit actions | M | Extend `cli::log`; actions through `GitOp` |
| Branch delete/rename/remote/merge/rebase, tags | M | CLI for merge/rebase |
| Conflict resolver (3-pane merge) | L | Done: a temporary layout of split panes over scratch buffers (`editor/merge.rs`) |
| Operation in progress (header, status bar, refused actions) | S-M | `repo.state()` in `StatusSnapshot`; done (yman 247) |
| Continue / Skip / Abort a rebase, merge, cherry-pick or revert | M | git CLI with GIT_EDITOR=true; done (yman 248) |
| Interactive rebase (Log: Interactively Rebase from Here) | M | git rebase -i with jetl's todo through `-c sequence.editor`; done (yman 266) |
| Reset current branch to a commit (soft, mixed, hard) | S-M | git CLI; hard confirms and snapshots; done (yman 249) |
| Push tags, force with lease, stash untracked | S | git CLI; done (yman 250) |

### E. Shell, terminal, run

All of section E shipped in Phase 9 except split editor panes, which shipped in Phase 11.

| Feature | Effort | Notes |
|---|---|---|
| Built-in terminal tool window | L | `portable-pty` + `vt100` (or `tui-term`) |
| Run/task window with problem matchers | M | tokio process + line parser |
| Run configurations | M | Done: [[run.configs]] / .jetl/run.toml with args, cwd, env, matcher; toolbar picker, Shift+F10 runs the selected one (kept in the session), Edit Configurations window writing .jetl/run.toml through toml_edit |
| Test runner tree | M | Done: tests matcher for libtest, pytest -v, flat TAP (vitest) and jest --verbose, Tests view with counts, failed tests expanded and linked, Rerun Failed per runner, Ctrl+Shift+F10 runs the test at the caret (Rust, Python, JS/TS) |
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
| Signature help | S | Done: Ctrl+P (View > Parameter Info) and the server's trigger characters open a popup above the caret with the active parameter bold; it follows the caret while open |
| Go to definition, references, hover | M | Done: Ctrl+B and Ctrl+click, Alt+F7 usages with previews, Alt+Q hover popup, Ctrl+Shift+B type declaration, Ctrl+Alt+B implementations, Navigate > Declaration, workspace symbols in Go to Symbol, showMessageRequest buttons |
| Completion popup | L | Done: auto-trigger and Ctrl+Space, fuzzy filter, snippet tabstops (Tab/Shift+Tab), additional edits, resolve, documentation panel, matched chars, labelDetails, commit characters |
| Rename, code actions, formatting, document symbols | M | Done: Shift+F6 rename across files, Alt+Enter context actions with resolve and `workspace/applyEdit`, Ctrl+F12 file structure, formatting through the server when `[format]` has no command (the selection only when the server formats ranges); a lightbulb for quick fixes, preferred actions first; workspace edits create, rename and delete files |
| Inlay hints | M | Done: `textDocument/inlayHint` for the visible lines plus a screen each way, refetched after edits and scrolling settle, kept and shifted until the new answer; drawn as virtual text in the layout walk (soft wrap counts them, a click lands on the anchor, the caret stands before a type hint and after a parameter hint, column mode ignores them); `[lsp] inlay_hints`, View > Inlay Hints |
| Semantic highlighting | M | Done: `textDocument/semanticTokens/full` (`/range` for servers without it), asked again after edits settle and on refresh, kept and shifted until the next answer; drawn over tree-sitter through `[syntax]` capture names; `[lsp] semantic_tokens` |

### G. Themes, vim, config

| Feature | Effort | Notes |
|---|---|---|
| Named themes, built-in light theme, 256-color fallback | S-M | Done: `ui.theme` darcula/light/theme file, `theme.toml` on top, 256-color fallback (`$COLORTERM`, `ui.colors`) |
| Config hot reload | S | Done: config, keymap and theme files reload through a config-dir watch; broken files keep the current value; restart-needed keys are named |
| Settings window | M | Done: Ctrl+Alt+S / File > Settings… edits `config.toml` in place (comments and order kept) with the reload class of every key shown; Apply, OK, Cancel; lists are edited as comma-separated text; tables, keymap and theme stay in their files |
| Vim mode | L | Dispatcher mode tables exist |
| Local History | M | Done: snapshots on every save and before git operations that rewrite the work tree, reloads, Overwrite, Save As over a file and tree deletes, and optionally of unsaved edits on a timer (`snapshot_unsaved_secs`), in one global content-addressed, compressed store (`state/history/`, its own writer thread, pruned by `[local_history]` age and size); File > Local History… (Alt+Shift+H) opens the Local History tool window: the file's live snapshot list (filter, follows renames) next to the diff of the selected one, Revert (Alt+R) is one undoable edit, Revert Change (Ctrl+Alt+Z) puts back only the diff's selected changes; Put Label keeps named snapshots, put and removed in the window; its project view lists the files of the project or a folder with history, deleted and renamed ones included, labels grouped |
| Scratch files | S | Done: File > New Scratch File… (Ctrl+Alt+Shift+Insert) picks a language and opens `scratch_<n>.<ext>` in `state/scratches/`; File > Scratch Files… lists them (Shift+Delete deletes); no language server for them unless `[lsp] scratches = true` (own clients rooted at the scratches directory, rust-analyzer via `detachedFiles`) |

### H. Quality and distribution

| Feature | Effort | Notes |
|---|---|---|
| `docs/ARCHITECTURE.md`, README | S | Both done |
| Render snapshot tests (`TestBackend` + `insta`) | M | `test_app()` harness exists |
| CI, release binaries, AUR | S | CI done (GitHub Actions, Bitbucket Pipelines); local release via `scripts/package.sh` (tarball + Arch package); remote releases and AUR open |
| Crash recovery of dirty buffers | S | Done: the panic hook writes the dirty buffers to `state/recovery/<pid>-<secs>/` (bounded `try_lock`, never blocks); the next session load of that root offers Restore all / Discard / Review…; restored text opens unsaved and undoable, no autosave until an explicit save; Save As (Alt+Shift+S) saves the untitled ones |
| Palette filter off the main thread | S | Done: Files and Symbols of 10000 candidates and up rank in `spawn_blocking` under an app-wide cancel seq; the shown rows stay until the result, Enter ranks the typed query at once; a longer query narrows the previous result (`narrow_rows`) |

### I. Stage E

Ranked after 0.9.0 against JetBrains, Zed, Helix and VS Code; tracked as yman epic 452, one task per row in this order.

| Rank | Feature | Effort | Builds on |
|---|---|---|---|
| 1 | Next/previous method (Alt+Up/Down) | S | Done: tags query symbols (function, method, constructor), Alt+Up/Down in the editor, Navigate menu |
| 2 | Disable a breakpoint without removing it, hit counts | S | Done: disable (dim, not sent; Space in View Breakpoints), hit count in Edit Breakpoint (gdb: N-1 hits skipped), both in the session |
| 3 | Right margin and column rulers (`editor.rulers`) | S | indent guide paint. Done: │ in theme style ruler after each column of editor.rulers (Live) on the cells a row's text leaves empty, after horizontal scroll |
| 4 | LSP document highlight (read/write kinds) | S | `highlight_occurrences`. Done: `textDocument/documentHighlight` after 200 ms on the caret word, kinds on `occurrence_read` / `occurrence_write` / `occurrence_bg`, textual fallback |
| 5 | Inline values while debugging | M | inlay hint virtual text, Variables. Done: `[debug] inline_values` (Live), a second virtual-text layer `InlineValues` in `editor/inlay.rs` drawn in the `inlay_hint` style after the line-end hints, rebuilt from the Variables tree at each stop (the window may be hidden), cleared on resume, stop and edit |
| 6 | Hot exit: unsaved buffers kept across quit | M | Done: `[files] hot_exit` (on): quit asks nothing, dirty buffers kept in `state/hot_exit/<key>/` (crash-dump format), restored silently and unsaved at the next session load; last process to quit owns the dump, an earlier one goes to crash recovery |
| 7 | Custom folding regions, fold selection | S-M | Done: custom regions from comment markers (`region`/`endregion`, `#region`/`#endregion`, `<editor-fold>`); fold selection was already Ctrl+. / Alt+Shift+F |
| 8 | Preserve case in replace | S | Done: Preserve case toggle (AB, Alt+P) in the editor find bar and Replace in Files; off with regex group references |
| 9 | Bracket pair colours | S | Done: `()[]{}` tokens from the tree (strings and comments skipped, injections included) coloured by nesting depth in theme styles bracket_1..3, editor.bracket_pair_colours (Live, off by default) |
| 10 | New file from a template | S | Done: New > From Template… in the Project tree, templates in ~/.config/jetl/templates/ and .jetl/templates/, snippet syntax and variables, tabstops |
| 11 | Spell checking through a language server | S-M | Done: `extra = true` servers next to the main one (sync, diagnostics, file notifications only); CONFIG recipes for typos-lsp and harper-ls |
| 12 | Instruction stepping and disassembly | M | `ResumeKind`, DAP `disassemble`. Done: next/stepIn with granularity instruction (Ctrl+Shift+F7, Alt+Shift+F7), Disassembly tab in the Debug window (32 instructions forward from an anchor kept while the pc stays in view; gdb cannot disassemble backwards) |
| 13 | Keyboard macros | M | Done: record (Ctrl+Shift+F4), play (Ctrl+Shift+F5, one undo step per file), Save Macro As into state/macros.json, Find Action entries |
| 14 | Shelve and unshelve changes | M | Done: Alt+Shift+V in the Commit tab (files, a header's section, or the diff pane's block or lines) stores a patch in state/shelf/<hash of the root>/ and reverts the work tree; the Shelf tab lists entries with Unshelve (Alt+Shift+U: plain `git apply`, then `--3way`), Show Patch and Delete (confirmed); staged changes come back unstaged |
| 15 | Find in Files results in an editor tab | M | Done: Alt+Enter in the Find window (`find.open_in_tab`) opens a pathless read-only tab (`editor/search_tab.rs`) with its own `Search`; Enter or a double click opens a hit, F5 searches again, Ctrl+F works; never in the session |
| 16 | Coverage gutter from lcov | M | gutter. Done: line numbers coloured from [coverage] lcov (coverage_covered, coverage_uncovered; Live), View > Coverage, reloaded on change, stale per file |
| 17 | Increment / decrement the number at the caret | S | caret text transforms |
| 18 | Pipe the selection through a shell command | S | `[format]` runner |
| 19 | Search Everywhere tabs | S | `palette.search_everywhere` |
| 20 | Mnemonic bookmarks and groups | S | `editor/bookmarks.rs` |

Carried over from Stage D: other debug adapters (research first), vim mode, publishing release binaries.

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
8. **Stage E:** section I, in rank order.
