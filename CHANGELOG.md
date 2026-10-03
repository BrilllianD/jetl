# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Application main menu, JetBrains-style: F10 opens a menu bar over the top row (File, Edit,
  View, Navigate, Git, Run, Help) with shortcuts next to each item. Left/Right or the underlined
  letter switch menus, the mouse works too, Esc or F10 closes it. Menus taller than the
  terminal scroll. File > Settings… opens `config.toml` (created when missing), Help > Keymap
  Reference opens `docs/KEYMAP.md`, Help > About jetl shows the version.
- Compact tool strip: each tool window button is its Alt-number over an icon, so all of them fit
  on a 24-row terminal, with a menu button on top that opens the main menu. Hovering a button
  shows its name and shortcut. The icons are one-cell symbols by default; `[ui] icons = "emoji"`
  switches the strip and the menu bar to color emoji (needs a color emoji font).
- Language server client: servers start per project when the first file of their language opens
  (built in: rust-analyzer, typescript-language-server, pyright, gopls, clangd,
  lua-language-server, bash-language-server; `[lsp]` in `config.toml` changes or adds them).
  Open files are kept in sync incrementally as you type, saves and closed tabs are reported,
  server messages show as notifications, and the status bar shows the active file's server
  with its indexing progress, or that it is missing or stopped. Help > Restart Language Servers
  (`lsp.restart`) starts them again. Servers are shut down on quit and project switch, with
  everything they started. Diagnostics, navigation and completion build on this next.
- Diagnostics from language servers: a glyph in the gutter (`✖` error, `⚠` warning, `ℹ` info)
  and an underline in the severity's color over the text, the problem at the caret in the status
  bar, and the active file's error and warning counts there (`✖ 1 ⚠ 2`; a click opens the
  Problems window). F2 and Shift+F2 (Navigate > Next/Previous Problem) step through the file's
  problems. The Problems tool window (Alt+6, View > Problems) lists them grouped by file; Enter
  or a double click opens one at its position. `[lsp] max_diagnostics_per_file` caps a file's
  list (default 1000), and the theme styles `diag_error`, `diag_warn`, `diag_info` and
  `diag_hint` color them.
- Language server navigation. Ctrl+B or Ctrl+click goes to the declaration of the symbol at the
  caret (Navigate > Declaration or Usages); with several candidates a list opens, and on the
  declaration itself it lists the usages. Alt+F7 (Navigate > Find Usages) lists the usages with
  a preview of each line; typing filters the list and Enter jumps. Alt+Q (View > Quick
  Documentation) shows the hover documentation in a popup at the caret, scrolled with the arrow
  and page keys and closed by Esc or any other key. Back (Ctrl+Alt+Left) returns from every
  jump. Without a server for the file Ctrl+B opens Go to Symbol on the word at the caret.
- Code completion from language servers. A popup opens under the caret (above it near the
  bottom) as you type an identifier, after the server's trigger characters such as `.` or `::`,
  or on Ctrl+Space (Edit > Code Completion). Each row shows a kind glyph, the label and the
  detail; typing on filters the list fuzzily, Up/Down/PgUp/PgDn select, Enter or Tab (or a click)
  inserts, Esc closes, and leaving the word closes it too. Inserting applies the item's edit and
  its extra edits (such as an import) as one undo step, snippets flattened with the caret at
  their final position; items the server resolves lazily are resolved first, for at most 300 ms.
  `[lsp] completion_auto` (default on) and `completion_min_chars` (default 1) control the
  automatic popup. In the editor Ctrl+Space is now completion; legacy terminals send Ctrl+` as
  the same key, so there the branches popup is Ctrl+` only with the kitty protocol.
- Rename through the language server: Shift+F6 in the editor (Edit > Rename…) asks for the new
  name of the symbol at the caret (the server says what can be renamed, where it supports that)
  and renames it with its usages across the project. Open files change as one undo step each
  and stay unsaved; files that are not open are rewritten on disk. A toast says how many
  occurrences in how many files changed. An edit the server made for an older version of an
  open file, or one that needs creating, renaming or deleting files, changes nothing.
- Context actions: Alt+Enter in the editor (Edit > Show Context Actions) lists the server's quick
  fixes and refactorings for the caret or selection, given the problems there; choosing one makes
  its edit, resolving it first when the server computes edits lazily, and runs its command on the
  server, which may edit files in turn (`workspace/applyEdit`).
- File structure: Ctrl+F12 (Navigate > File Structure…) lists the file's symbols from its
  language server, nested by indentation with a kind glyph; typing filters, Enter jumps. The
  Structure window (Alt+7) stays tree-sitter based.
- Formatting through the language server: without a `[format]` command for the file's language,
  Reformat Code (Ctrl+Alt+L) and format on save use the server when it formats (with the file's
  indentation). A `[format]` command still wins. A save waits for the server at most
  `[lsp] request_timeout_ms`, then writes the file unformatted.
- Split editor panes: Alt+\\ splits the editor right and Alt+- splits it down (View > Split
  Right/Down, or the tab's right-click menu); splits nest. Each pane has its own tabs, and the
  same file can be open in several panes, each with its own caret, selection, scroll and find
  bar; an edit in one pane moves the carets in the others with the text. Undo restores the caret
  of the pane that undoes. Alt+Tab and Alt+Shift+Tab move between panes (View > Next Tab Group;
  the window manager may take Alt+Tab), a click focuses a pane, the wheel scrolls the pane under
  the pointer, and the bar between two panes drags. Opening a file puts it in the focused pane.
  Closing a file's tab in one pane keeps the file open in the others without asking; a pane
  whose last tab closes goes away. Move Tab to Opposite Group moves the tab to the pane beside
  it (splitting first when there is one), Unsplit merges the focused pane into its neighbour and
  Unsplit All merges everything into one. Close Others, Close All but Pinned and Close All act on
  the focused pane. Sessions restore the panes with their tabs, carets and scroll.
- Multiple carets, as in JetBrains. Alt+J selects the word at the caret, then adds the next
  occurrence of the selected text as another caret (whole words when it started from a word,
  wrapping at the end); Alt+Shift+J takes the last one back; Ctrl+Alt+Shift+J (Ctrl+Alt+J in
  terminals without the kitty protocol) selects every occurrence, or every match of the open find
  bar. Alt+click adds a caret, or removes the one clicked. Clone Caret Above/Below are in the
  Edit menu and F1. Typing, Backspace, Delete, word deletion, Enter with smart indent, Tab,
  auto-close pairs, toggle case, block comments, duplicate line and the motions act at every
  caret; indent, unindent, line comments, delete, join and move line act on all caret lines; one
  undo step undoes the edit at every caret and puts every caret back. Copy at several carets
  copies each selection (or caret line); pasting it at as many carets gives each caret its piece,
  and text with one line per caret is split the same way. Esc first drops the other carets, then
  the selection. Extra carets are drawn in reverse video (theme key `secondary_caret`), and the
  status bar shows their count. Language server features and completion work at the primary
  caret (the last one added); completion does not pop up by itself while there are several.
  At most 1000 carets.
- Column (block) selection. Alt+drag (Alt+Shift+drag too) selects a rectangle by screen columns,
  a caret on every line: tabs and wide characters cut by an edge are taken whole, lines too short
  to reach it get a caret at their end, and a block of zero width is a caret per line. Typing,
  paste and deletion then act on every line as with multiple carets, undone in one step.
  Column Selection Mode (Alt+Shift+Insert, Edit menu) makes a plain drag and Shift+arrows,
  Shift+Home and Shift+End select blocks too; the status bar shows `Column` while it is on.
- Code folding. A marker column in the gutter shows `▾` where a region can fold (bodies, blocks,
  classes, bracketed lists, JSON objects and arrays, multi-line comments, runs of line comments
  and of imports, found in the syntax tree; by indentation in files without a grammar) and `▸`
  on a folded line; a click on it folds or unfolds. A folded region shows as its first line with
  a placeholder (`fn main() {…}`, theme key `fold_placeholder`), and the bookmarks, git marks
  and worst diagnostic of the lines it hides; a click on the placeholder unfolds it. Ctrl+-
  folds the region at the caret (again: the one around it), Ctrl+= unfolds, Ctrl+Alt+- and
  Ctrl+Alt+= do it recursively, Ctrl+Shift+- folds everything, Ctrl+. folds the selection; the
  View menu has Expand, Collapse, Expand All, Collapse All and Fold Selection, and Alt+, Alt+.
  Alt+< Alt+> collapse, expand, collapse all and expand all in terminals without the kitty
  protocol. Up/Down, PageUp/PageDown, the wheel and the mouse step over folded lines; a caret
  that lands inside a fold (Right at the header's end, Enter there, undo, find, go to line, F2,
  go to definition) unfolds it. Each split pane folds on its own. Folds follow edits and are
  saved with the session; reloading the file from disk drops them.

## [0.1.0] - 2026-10-03

First release.

### Editor

- Tabbed editor over a rope buffer with undo grouping, smart Home, word motions, duplicate,
  delete, move and join lines, toggle case and delete word forward.
- Tree-sitter syntax highlighting with incremental reparsing for Rust, JavaScript, TypeScript,
  TSX, Python, JSON, TOML, Bash, Markdown, C#, YAML, HTML, CSS, SCSS, XML, Go, C, C++, Java, PHP,
  Lua and Dockerfile, each behind a `lang-*` cargo feature. Language detection by extension,
  file name, shebang and modeline. Files above 2 MiB open without highlighting.
- Bracket matching and jump, auto-closed brackets and quotes, language-aware indent on Enter and
  dedent on closers, indent and unindent of a selection, line and block comment toggle, expand
  and shrink selection by syntax node, and highlighting of the word under the caret.
- Per-buffer indentation from `.editorconfig` or detection; line ending, encoding, indent and
  language segments in the status bar.
- Atomic save with optional trailing whitespace trimming and final newline; autosave on focus
  loss or after an idle pause; format on save through an external formatter per language.
- Back/forward caret navigation and bookmarks.
- Open files reload when changed on disk, and saving over an outside change asks first.
- Find and replace in the file, go to line.
- Pinned tabs, tab reorder and a tab limit that closes the least recently used clean, unpinned
  tab.

### Project tree and navigation

- Project tree with new, rename, delete and copy path, a right-click context menu, speed search,
  and hidden and ignored file toggles.
- Filesystem watcher that refreshes the tree, git state and open files; focus-gain refresh as a
  fallback.
- Palette with Files, Symbols, Actions, Search Everywhere, Recent Files, Branches and Recent
  Projects modes.
- Symbol search and a Structure tool window from tree-sitter tags.
- `jetl PATH:LINE:COL` opens a file at a position, rooted at its git repository.
- Sessions per project root restore tabs, pins, carets, bookmarks, tool window layout and
  expanded tree folders.

### Search

- Find in files and replace in files in the Find tool window, with next and previous result
  navigation.

### Git

- Git tool window with Commit, Log, Stash and Console tabs.
- Stage, unstage and discard files or single changes; rename detection.
- Commit, amend, per-repository commit message history, push (with remote choice for `-u`) and
  pull (fast-forward only, rebase or merge).
- Diff view with intra-line change marks, syntax highlighting, folding of unchanged lines and
  side-by-side or unified layout.
- Gutter marks against HEAD, revert of the change at the caret, next and previous change, and a
  blame column.
- Log with its own graph, filters by branch, text, author and path, file history, and commit
  actions: checkout, new branch, cherry-pick, revert and tags.
- Branches popup to check out, merge, rebase, rename and delete branches.
- Worktrees tool window to create, switch, remove and prune worktrees.
- Network and worktree operations run through the `git` CLI; ssh never prompts over the TUI.

### Terminal and Run

- Terminal tool window: a PTY shell with vt100 emulation and scrollback.
- Run tool window: tasks from `config.toml` or detected from project files, problem matchers with
  click-to-jump, rerun and stop.

### UI and configuration

- JetBrains-style default keymap with fallbacks for terminals without the kitty keyboard
  protocol; `jetl --dump-keymap` prints it.
- `config.toml`, `keymap.toml` and `theme.toml` under `$XDG_CONFIG_HOME/jetl/`; built-in Darcula
  theme with color overrides.
- Mouse support, toasts, confirm, input and choice dialogs, and an Event Log tool window.
- Zen mode, keyboard resize of tool windows, paste from the system clipboard (`wl-paste`,
  `xclip`, `xsel`) and copy over OSC 52.
- A panic hook restores the terminal; logs go to `$XDG_STATE_HOME/jetl/jetl.log`.

### Known limitations

- No LSP support yet (planned for Phase 10).
- No split editor panes, multiple carets, column selection, code folding or word wrap (planned
  for Phase 11).
- No 3-pane conflict resolver; conflicted files show as `U` and are resolved in the editor.

[0.1.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.1.0/
