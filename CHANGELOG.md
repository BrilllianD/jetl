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
