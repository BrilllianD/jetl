# jetl roadmap

This is the feature backlog for jetl after phases 0-5. Each item below is also tracked as a yman
task (`yman ls`); phases are umbrella tasks and the rows are their subtasks. Phases 6 and 7 are
done (see Order).

## What exists today

- **Editor:** single caret. `Buffer::apply` already accepts multi-range change lists. Also there:
  undo grouping, incremental tree-sitter reparse (`Syntax::tree()` feeds brackets, expand selection and the
  Structure window), smart
  Home, word motions, duplicate/delete line, line comment toggle, CRLF detection, atomic save, and
  a 2 MiB highlight threshold. Phase 6 added bracket logic, indent/dedent of a selection,
  move-line and autosave; open files reload or ask when they change on disk. Missing: wrap,
  folding, multiple carets, column selection.
- **Shell:** fixed layout with one left slot and one bottom slot. The side each tool window lives
  on is hardcoded in `ToolWindowId::side()`. A new bottom tool window is just a new enum variant
  and needs no layout rewrite. The palette has Files/Symbols/Actions/Everywhere/Recent/Branches/Recent Projects modes.
  Dialogs come in confirm, input and choice kinds; the Project tree has a right-click context
  menu. The session stores tabs, carets, bookmarks, the tool window layout and expanded tree folders.
  `App.notifications` keeps 200 entries, but nothing in the UI shows them.
- **Git:** staging is whole-file only. Gutter marks are based on HEAD and are render-only. The
  diff is side-by-side or unified depending on width, with line-level coloring. There is a log
  with graph, details and file diff, and branches are reachable from the palette. Missing: amend,
  stash, blame, discard, hunk operations, merge/rebase, tags, branch delete and a conflict UI. A
  filesystem watcher refreshes the tree and git status; focus gain refreshes only when the
  watcher is off (`[files] watch`) or failed to start.
- **Infra:** `AppMsg::Lsp(LspMsg)` is an empty stub. Background services follow one pattern:
  clone (root, generation, tx), spawn, send a tagged message, and the app drops stale generations.
  `docs/ARCHITECTURE.md` and CI (fmt, clippy, test) exist; there is no README or snapshot tests yet.

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
| Word wrap | M | Must stay inside the shared `layout_line()` walk |
| Column (block) selection | M | Needs a selection list; pairs with multi-caret |
| Multiple carets | L | `apply()` already accepts multi-range change lists |
| Code folding by tree-sitter node | M | Fold state per view; `layout_line` skips folded ranges |
| Back/forward caret navigation, bookmarks | S-M | Position ring in `Editor`; bookmarks persisted in session |
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
| Conflict resolver (3-pane merge) | L | Waits for split panes |

### E. Shell, terminal, run

| Feature | Effort | Notes |
|---|---|---|
| Built-in terminal tool window | L | `portable-pty` + `vt100` (or `tui-term`) |
| Run/task window with problem matchers | M | tokio process + line parser |
| Format on save via external formatter | S-M | `[format]` config per language |
| Split editor panes | L | Tab groups with independent views over shared buffers |
| Zen / maximize editor | S | |
| Event Log tool window | S | `App.notifications` already holds 200 entries |
| Tab reorder, pin, limit | S | |
| Keyboard resize of tool windows | S | `LayoutState` setters exist |
| Wayland clipboard paste fallback | S | `wl-paste` / `xclip` |

### F. Language intelligence (LSP)

| Step | Effort | Notes |
|---|---|---|
| LSP client core | L | `lsp-types`, own JSON-RPC layer on tokio, UTF-16 positions, `[lsp]` config |
| Diagnostics + Problems window | M | |
| Go to definition, references, hover | M | Needs back/forward navigation |
| Completion popup | L | |
| Rename, code actions, formatting, document symbols | M | WorkspaceEdit across files |

### G. Themes, vim, config

| Feature | Effort | Notes |
|---|---|---|
| Named themes, built-in light theme, 256-color fallback | S-M | `theme_fields!` makes a second table cheap |
| Config hot reload | S | After the watcher |
| Vim mode | L | Dispatcher mode tables exist |
| Local History | M | Reuse `DiffDoc` view |
| Scratch files | S | Buffers rooted in the state dir |

### H. Quality and distribution

| Feature | Effort | Notes |
|---|---|---|
| `docs/ARCHITECTURE.md`, README | S | ARCHITECTURE.md done; README open |
| Render snapshot tests (`TestBackend` + `insta`) | M | `test_app()` harness exists |
| CI, release binaries, AUR | S | CI done (GitHub Actions, Bitbucket Pipelines); releases open |
| Crash recovery of dirty buffers | S | Panic hook writes to a recovery dir |
| Palette filter off the main thread | S | nucleo supports it |

## Order

1. **Phase 6: editor ergonomics + grammars.** Done. This phase covers the small items in A and the
   grammars in B. It carries no architectural risk and is felt in daily use right away.
2. **Phase 7: project-wide.** Done. The watcher came first, then find and replace in files, tree
   operations with context menu and speed search, `file:line` CLI, layout persistence, recent
   projects, and symbol search with the Structure window (pulled forward from Phase 10).
3. **Phase 8: git depth.** Discard, amend, hunks, intra-line diff, stash, blame, log filters and
   branch management.
4. **Phase 9: terminal and run.** Terminal, run window, format on save, event log and tab polish.
5. **Phase 10: language intelligence.** tree-sitter symbols shipped in Phase 7. LSP core comes
   first, then diagnostics and navigation, and last completion, rename and code actions.
6. **Phase 11: editor layout.** Split panes, multi-caret, column selection, folding and wrap. This
   has the highest implementation risk, so it goes last.
7. **Ongoing:** themes, vim mode, local history, snapshot tests, CI, README, crash recovery.
