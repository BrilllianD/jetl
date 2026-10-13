# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Language servers that ask for file change events get them from jetl's file watcher, including folders outside the project
  they register (rust-analyzer then stops running its own watcher). With `[files] watch = false` they keep watching by
  themselves; changing that setting restarts the language servers.
- Language servers that register document syncing after they start (open, change, save, close) get it honoured
  mid-session.
- Language server edits marked as needing confirmation (for example rust-analyzer's edits outside the workspace) ask first,
  listing the marked changes; Cancel changes nothing.
- Creating, renaming and deleting files in the Project tree tells language servers (`workspace/willRenameFiles` and the
  like): rust-analyzer updates `mod` declarations when a module file or folder is renamed.
- Legacy terminal and tmux keys for folding: Alt+Shift+F folds the selection, Alt+{ and Alt+} collapse and expand recursively.
- Legacy terminal and tmux keys: Alt+Y redoes, Alt+Shift+W saves all, Alt+Shift+K pushes, Alt+Shift+Q hides the active
  tool window.
- Tab completes the folder in New Worktree's directory prompt (folders only), like Save As.
- Dragging a selection past the left or right edge scrolls sideways (soft wrap off).
- Column selection: typing and pasting pad lines shorter than the block with spaces up to its left column, and
  their carets are drawn there. Backspace there moves the block's column left without touching the short lines, and
  Delete joins nothing there. Enter and Tab in that virtual space, Alt+click into it and a selection highlight past
  a line end are not supported yet.
- The Local History window's diff has a line cursor: Alt+Up/Down moves it over the changed rows, Alt+Shift+Up/Down
  grows the selected lines, a double click on a changed row sets it, and Esc leaves it. Revert Changes (toolbar
  `[Revert N Lines]`, menu "Revert Selected Lines") then puts back only those lines; the selected rows' line
  numbers are reversed. F7, Shift+arrows, Ctrl+A and a single click go back to whole changes.
- The Local History window's file view list shows the seconds (`MM-DD HH:MM:SS`) on rows whose minute equals that
  of the row above or below, so saves within one minute can be told apart.
- The Local History window's diff follows edits of the shown file: while the window is visible and the diff
  compares a snapshot with an open tab, typing in that tab updates the diff within half a second, keeping the
  scroll and the selected change. Quick selection moves no longer pile up diff builds: one runs, the newest waits.
- Revert on a label row of the Local History window's project view asks `Revert N files to label "x"?` and,
  on Revert, puts every file of the label back to its labelled text: one undoable edit per open tab, files
  without a tab opened in the background, a missing file reopened as an empty tab with the text. The edits stay
  unsaved; files that are not UTF-8 or whose text is gone are skipped and named in the toast. Refused while
  the conflict resolver is active.
- Put Label works on a folder or the whole project: select a folder in the Project tree (or use Put Label in
  the Local History window's project view) and every open file under it (unsaved edits included), every
  changed file git lists and every file with history gets the same label, which the project view shows as
  one label row; removing it removes it from every file.
- Local History follows files and folders renamed or moved inside the project by another program (`mv`, a
  file manager, another editor): the history moves to the new path with a "Renamed from" row, and the
  Local History window follows. Moves from or to outside the project are not followed.
- Local History follows files that git renames: after checkout, switch, pull, merge, rebase, cherry-pick,
  revert or stash apply, the history of each renamed file moves to its new path with a "Renamed from" row,
  as for a rename in the Project tree.
- Local History keeps the unsaved edits you discard: answering Don't Save when quitting, closing tabs or
  switching the project root first stores each dirty file's text as "Before discarding unsaved edits",
  whether or not the periodic unsaved snapshots are on.
- The conflict resolver pads the shorter side of a change with blank rows, so the three panes stay aligned
  while scrolling, inside a change of unequal sizes and after accepting both sides.
- After a conflicted file is resolved (Accept Yours/Theirs or Apply in the resolver), Resolve Conflicts
  reopens with the files left, or says that all conflicts are resolved.
- Conflicted symbolic links and submodules can be resolved by taking yours or theirs (a link is
  recreated, a submodule's commit is staged without touching its checkout).
- Tabs drag between split groups: onto another group's tab bar (left half of a tab inserts before it, right
  half after, pinned tabs stay among the pinned ones), or onto its text, where a ghost tab shows and the
  release appends it. The target group takes focus; an emptied group collapses. Not while the merge
  view is open.
- A file's View | Soft Wrap toggle is saved in the session (per view in split groups) and restored on
  restart; files left at their configured default store nothing and keep following the config.
- `editor.soft_wrap_extra_indent` (cells, default 0, applies at once) shifts the continuation rows of a
  soft-wrapped line further right, after the line's own indent; together at most half the width.
- `[lsp] scratches = true` (default off, applies at once) gives scratch files language servers: a
  separate client per server, named `<server> (scratches)` and rooted at the scratches directory
  (rust-analyzer gets the scratch files as `detachedFiles`; a new Rust scratch restarts that
  client). Their problems show in the editor and status bar; the Problems window lists them only
  in its current-file scope, and its project scope counts only the files it lists.
- The Locations list (Find Usages, several declarations) groups its rows by file under file headers
  when the results span several files, shows each line number, and highlights the usage itself in
  the preview. Enter on a header opens that file's first location. Find Usages with a single
  result jumps straight to it.
- Completion shows a documentation panel beside the popup (stacked above or below it when
  there is no room) with the selected item's signature and documentation, fetched from the
  server a moment after the selection settles; Alt+Up/Alt+Down and the mouse wheel scroll it.
  The characters you typed are highlighted in each label, the server's label details are
  shown dimmed after it, and commit characters (such as `(`) accept the selected item and are
  then typed, once the list was asked for with Ctrl+Space or the selection was moved.
- Quick Documentation (Alt+Q) highlights fenced code: a fence's language (```` ```rust ````), or the
  file's language for a bare fence; unknown languages stay plain.
- Completion snippets have tabstops: accepting a function selects its first argument
  placeholder, Tab and Shift+Tab move between the placeholders (several copies of one are
  edited together), and the last Tab goes to the end of the snippet. Esc, or moving the caret
  out of the placeholders, ends it; Tab then indents again.
- Language server edits can now create, rename and delete files (rust-analyzer's module
  rename, "move module to file", code actions that make new files). jetl tells servers it
  supports this. Everything is checked before anything changes: an edit that would overwrite
  or delete a file with unsaved changes, touch a path outside the project or inside `.git`,
  create a file that exists or rename/delete one that does not (unless the server allows it)
  is refused whole. A renamed file's tab follows it, with its unsaved edits and local
  history; tabs of deleted files close; created files are not opened. File operations
  cannot be undone with Undo; Local History keeps a snapshot of deleted and replaced files.
  If a step fails on disk, the steps after it are not made.
- Reformat Code (Ctrl+Alt+L) with a selection formats only the selection when the language
  server supports range formatting (several selections: the span from the first to the
  last); otherwise, and for `[format]` commands and saves, the whole file is formatted.
- Lightbulb: when the caret rests on a line with a problem and the language server has quick
  fixes for it, a `✱` replaces the line's gutter glyph (new `lightbulb` theme style).
  Alt+Enter lists the server's preferred fixes first, so Enter runs the preferred one.
- Diagnostics follow your edits: underlines, gutter marks, F2 and the Problems window move
  with the text you type until the language server reports again, and a report made for an
  older version of the file is moved to where the text is now. A problem whose text you
  delete disappears at once.
- Code the language server marks as unused is drawn faded, deprecated code struck through.
- The Problems window lists each problem's related locations under it (Enter jumps there),
  and Alt+C (or the context menu) shows only the file open in the editor.

### Changed

- Go to File and Go to Symbol narrow the previous matches while a query is being typed, so keystrokes in large
  projects are much cheaper.
- A stash pop or apply that conflicts with the current commit now leaves the conflicts for Resolve Conflicts
  (the entry is kept) instead of refusing; local changes in the way still make it refuse and change nothing.
- Soft wrap: End and vertical moves stop at the end of a wrapped row, with the caret drawn after
  its last character (not on the next row); a click past a row's text does the same. Typing there
  inserts at that offset, and Right moves the caret to the next row's start.
- A collapsed fold counts as one line for Delete Line, Duplicate Line, Select Line, Move Line
  Up/Down, Comment Line and Indent/Unindent: the header and its hidden lines are handled together,
  moving a line past a folded one jumps over the whole fold, and the fold stays collapsed after a
  move or duplicate (the copy too). Select All and Select Line no longer expand the folds they
  cover. The gutter mark for deleted lines is `▔` instead of `▸`, which looked like a fold arrow.

### Fixed

- A language server that stops wanting a document open no longer gets changes for it; Restart Language Servers no longer
  races a server edit of a tree rename; when the watcher for folders outside the project cannot start, the servers restart
  once and watch by themselves; a confirmation replaced by another dialog is declined at once.
- Dialogs on short terminals keep their input and buttons visible (the message is cut with `…`); indented paths in dialog
  messages keep their indent when wrapped; a forgotten recent project no longer reappears briefly.
- The Local History diff keeps following edits while builds are slow; a label revert names the files that could not be opened.
- Completion keeps its auto-imports when several carets are active and a caret's copy of the edit overlaps another.
- Alt+click past a wrapped row end, where a caret sits at the next row's start, shows the caret on the clicked row.
- Formatting a file with lone CR line breaks replaces only the changed lines.
- Language server edits ask for confirmation only when a change refers to an annotation that needs it: rust-analyzer's
  renames, quick fixes and file renames no longer always open "Confirm Changes" for an annotation nothing uses.
- Recent Projects: reopening the palette right after forgetting a project can no longer list it again.
- Single-line text fields (dialogs, palette, find bars) scroll sideways to keep the cursor visible instead of
  cutting long text; a click in an input dialog's field lands on the char under it.
- Toasts no longer hide under the palette, menus or dialogs: they move below them and can be clicked away while the overlay stays open.
- Dialog messages wider than the screen wrap onto more lines instead of being cut.
- Files with old Mac (lone CR) line breaks: change gutter, blame, merge, find in files, location previews and folds use the same lines as the editor.
- Format on save waits for a language server that is still starting (Save As to a new language, or Ctrl+S right after
  opening).
- After a rename in the Project tree, the change gutter and blame keep comparing with the file's committed version until the rename is committed.
- Undo brings back a collapsed fold that the undone edit removed.
- Accepting a completion with several carets inserts it at every caret that typed the same prefix.
- Double-click, Ctrl+click and Alt+click past the end of a wrapped row act on that row's last word or end.
- Local history: a running jetl prunes the store every hour, not only at startup, so orphan blobs of merged
  snapshots do not pile up past `max_total_mb` in a long session.
- Local history: pruning keeps index lines written by a newer jetl (an unknown kind) and their blobs, instead
  of dropping them when it rewrites an index.
- Conflicts after a merge, rebase, cherry-pick, revert or stash pop open Resolve Conflicts with a localized git too (git's messages are in the C locale), and a failed status refresh after a resolve no longer reports that all conflicts are resolved.
- The conflict resolver keeps its panes aligned when a change adds lines at the very top of the file (the other panes get blank rows before their first line).
- After a conflicting stash pop or apply the resolver names theirs after the stash (`stash On main: wip`) instead of `theirs`.
- Multiple carets: Duplicate Line with two carets on one line keeps both; Clone Caret Above/Below
  and Up/Down keep the visual column across tabs (soft wrap off); any caret motion resets Add Next
  Occurrence (and its whole-word mode); Alt+Shift+drag adds the column block to the existing
  carets; Ctrl+Shift+Left/Right in column mode extend the block by word.
- Split groups: a hover, signature or completion answer requested in one group no longer shows in
  another group showing the same buffer; resizing a view no longer re-scrolls a caret that was
  already off screen; reloading a file changed on disk keeps every caret (parked views too), fold
  and bookmark on its line through a line diff. Alt+Shift+Tab (previous group) is marked as needing
  the kitty keyboard protocol; Alt+Tab cycles groups elsewhere.

## [0.2.0] - 2026-10-09

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
- Themes: `[ui] theme` picks `auto` (default, see below), `omarchy`, the built-in
  `github-dark` (GitHub Dark Default), `darcula` or `light` (IntelliJ Light colors), or a theme
  file (a path ending in `.toml`, relative to the config directory) layered on Darcula.
  `theme.toml` still overrides single colors on top. An unknown name or a broken theme file
  falls back to GitHub Dark with a notification. Style tables accept `reverse`.
- Omarchy theme: on Omarchy, the default `auto` theme takes its colors from the current
  desktop theme's palette (`~/.local/state/omarchy/current/theme/colors.toml`) and follows
  every theme switch live. A `jetl.toml` in the Omarchy theme directory overrides colors per
  theme. Without Omarchy, `auto` is GitHub Dark.
- 256-color fallback: unless `$COLORTERM` is `truecolor` or `24bit`, theme colors are drawn as
  the nearest colors of the 256-color palette. `[ui] colors = "truecolor"` or `"256"` overrides
  the detection, and themes can give exact colors for this case in a `[colors256]` table.
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
- Soft wrap. Lines longer than the editor is wide continue on the next screen rows, broken after
  whitespace (a word longer than a row is cut), continuation rows indented like the line and
  marked `↪` in the gutter. Markdown, plain text, reStructuredText and AsciiDoc files open
  wrapped (`[editor] soft_wrap_files`), everything with `soft_wrap = true`; View > Soft-Wrap
  (`editor.toggle_soft_wrap`, no default key) turns it on or off for the active editor. Up/Down,
  PageUp/PageDown, the wheel, Clone Caret and the mouse go by screen rows and keep the caret's
  place on screen; Home and End stop at the row's ends before the line's. Each split pane wraps
  at its own width; folding works alongside. Column selection is not available while wrapped.
  The toggle is not saved with the session.
- Three-pane conflict resolver. Git > Resolve Conflicts (`git.resolve_conflicts`, also offered
  by itself after a merge, rebase, cherry-pick or revert stops on conflicts) lists the conflicted
  files; pick one, then Merge… or take yours or theirs whole (the only choice for a binary file or
  one a side deleted). Enter or a double click on a `U` file in the Commit tab opens the merge
  too. The editor area becomes yours, the result and theirs side by side (below 90 columns yours
  and theirs share the left pane as two tabs, Alt+Left/Right), scrolling together; the layout
  comes back when it closes. The result starts from the common base with every non-conflicting
  change applied (undo shows the base). Changes are colored, conflicts in `merge_conflict_bg`;
  `»` takes yours, `«` theirs (after the other side: both) and `×` ignores, by click or
  Alt+Shift+Left/Right and Alt+Shift+X at the caret; F7 and Shift+F7 step through what is left,
  Alt+Shift+A applies the remaining non-conflicting changes, and the header has the counter and
  Accept Yours, Accept Theirs (whole file), Apply Non-Conflicting, Apply and Cancel. The result
  is editable; an edit inside a change resolves it. Apply (Ctrl+Enter, Alt+Enter) writes the file,
  stages it and closes; Cancel (Esc) closes without writing, asking first when you changed
  something. Layout commands and opening other files wait until it closes.
- Render snapshot tests: split panes, multiple carets, occurrences, folding, soft wrap and the
  find/replace bar in the editor, the project tree and its context menu, the Git window's Commit,
  Log, Stash and Console tabs, the Find and Problems windows, the main menu bar, confirm, input
  and choice dialogs, the hover and completion popups, toasts, the Find Action and Go to File
  palettes and the conflict resolver are drawn on a test backend and compared as text with `insta`
  (`src/app/snapshots/`). Carets, selections, fold placeholders and chunk colors show as letters
  in a grid under each frame, so a highlight in the wrong cell fails a test.
- Crash recovery: when jetl crashes, the unsaved changes of every modified file are written to
  `$XDG_STATE_HOME/jetl/recovery/` (by default `~/.local/state/jetl/recovery/`), and the next
  time that project opens a dialog offers them: Restore all, Discard, or Review… to restore or
  skip each file. Restored files open unsaved over the text on disk (undo goes back to it); the
  files on disk do not change and autosave leaves them alone until you save. Esc keeps the
  changes for the next time. Dumps of other projects, and of a jetl that is still running, are
  left alone.
- Config hot reload: saving `config.toml`, `keymap.toml`, `theme.toml` or the `ui.theme` file
  applies it while jetl runs. A file that cannot be read or is not valid TOML keeps the current
  settings and shows the error; settings that need a restart of jetl or of the language servers
  are named in the notification. Open tabs keep their soft wrap; the new default applies to files
  opened afterwards.

- Numbered bookmarks, JetBrains-style: Ctrl+F11 asks for a digit for the caret line's bookmark
  (pressing the line's own digit removes it; a digit used elsewhere moves here), Ctrl+0..9 jump
  to the bookmark with that digit, opening its file when it was closed. Without the kitty
  keyboard protocol Alt+B then the digit jumps. The gutter shows the digit instead of ◆, the
  Shift+F11 list shows it too, and sessions keep the digits (older session files load as before).
- Save As (File > Save As…, Alt+Shift+S): asks for a path relative to the project root (`~` and
  absolute paths work too), creates missing folders, asks before overwriting an existing file
  and refuses a file open in another tab. The tab then follows the new file, with its language
  and `.editorconfig` settings. Untitled buffers (crash recovery's `(recovered)` copies) can now
  be saved: Ctrl+S and Save All ask for their path, and quitting, closing tabs or switching
  projects with "Save" asks for each one in turn; Esc at any prompt cancels the quit or close.
  With `[format] on_save`, Save As formats the file for its new path, as Ctrl+S does (the
  language may change: `notes.txt` saved as `notes.rs` gets the Rust formatter). Tab completes
  the typed path like a shell: the common prefix first, the matching files and folders listed
  under the input on a second Tab (folders end in `/`, hidden ones only after a typed `.`).
- Scratch files: File > New Scratch File… (Ctrl+Alt+Shift+Insert, as in JetBrains) asks for a
  language and opens a new `scratch_<n>.<ext>` kept outside every project, in
  `$XDG_STATE_HOME/jetl/scratches/`. File > Scratch Files… lists them, newest first, with their
  language and date; Enter opens one, Shift+Delete deletes it after asking. They save, autosave
  and come back with the session like any file; language servers leave them alone.
- Local history: every save stores a snapshot of the file, and so does everything that is about
  to overwrite or delete one (git operations that rewrite the work tree, reloads from disk,
  Overwrite of an outside change, Save As over a file, Delete in the Project tree). File > Local
  History… (Alt+Shift+H; also in the Project tree's context menu) opens the Local History tool
  window on the file: its snapshots with their date, reason, size and line change on the left,
  the selected one against the current text on the right. The list follows new snapshots as
  they are stored, typing filters it, Revert (Alt+R) puts the snapshot back as one undoable
  edit, and the window follows the file when it is renamed in the Project tree. Snapshots live in
  `$XDG_STATE_HOME/jetl/history/`, shared by every project, and are pruned after 5 days or
  beyond 100 MB (`[local_history]`).
- Local history can snapshot unsaved edits on a timer: set `[local_history]
  snapshot_unsaved_secs` (off by default, at least 10) and every open file with unsaved changes
  is stored as "Unsaved changes" each period, so work between saves survives a killed jetl or a
  closed terminal. They merge with autosaves into one snapshot per 5 minutes, and saving the
  same text turns the latest into "Saved".
- Local history: Revert Change (Ctrl+Alt+Z in the Local History window, its toolbar and menu)
  puts back only the selected changes of the diff, as one undoable edit, and keeps you in the
  window to go on. F7 or a click picks a change; Shift+Down / Shift+Up (or a Shift or Ctrl click)
  grow the selection, Ctrl+A takes every change. Nothing is reverted when the file changed since
  the diff was made: it is compared again instead.
- Local history labels: File > Local History: Put Label… (also in the Project tree's context
  menu) stores the file's current text under a name you choose, shown as "Label: <name>" in its
  Local History list; Put Label… (Alt+L) in the Local History window does the same for the file
  it shows. Labels are kept past `keep_days` and pruned for size only after every other
  snapshot; Delete on a label's row in the window removes it. A label applies to one file, not
  the whole project.
- Local history lists every file of the project (or of a folder) that has snapshots, deleted
  files included: Backspace or `[All Files]` in the Local History window, File > Local History…
  on a folder in the Project tree, or with no file open. Each row shows the file, what its
  newest snapshot was taken for and when; deleted files are crossed out, files renamed in the
  Project tree say where they went, and a label is one row that lists its files. The diff shows
  the selected file's newest snapshot, Revert brings a deleted file back in a tab to save, Enter
  shows the file's own snapshots and Backspace comes back.
- Recent Projects: Shift+Delete forgets the selected project. Its row goes at once and the
  palette stays open; the project itself is not touched, and opening it again lists it again.
  The current project is never listed, so it cannot be forgotten.

### Changed

- The default theme is `auto` (the Omarchy palette, else GitHub Dark) instead of Darcula; set
  `[ui] theme = "darcula"` to keep Darcula.
- Dialogs are wide enough to show their whole title.
- Minimum supported Rust version is now 1.90 (was 1.88). tree-sitter 0.27 already required
  1.90, so 1.88 never built; hover Markdown parsing no longer uses `if let` match guards.
- An invalid `theme.toml` now leaves the chosen theme in place instead of falling back to
  Darcula. Git log graph lanes take their colors from the theme.
- Go to File, Go to Symbol and Search Everywhere filter very large lists (10000 files or
  symbols and up) off the main thread, so typing stays responsive near the 200000-file cap.
  The previous results stay on screen until the new ones arrive, and Enter right after typing
  still opens the best match for the full query. The log warns about the file index cap only
  when a tree has more than 200000 files, not exactly that many.
- Local history keys a file by its real path, so a file and a symlink to it (or a path through
  a symlinked folder, such as a project opened through a symlink) share one history instead of
  two. The Local History window still shows the path you opened. A deleted file stays listed
  under the path it was opened with. Snapshots stored by an earlier build under a symlinked path
  are merged into the file's history when jetl starts.
- Local history stores snapshots compressed (zlib), so about three to four times as much
  source fits in the store; `max_total_mb` counts the compressed size on disk. Snapshots stored
  by an earlier build stay readable and are pruned as before.
- Local history merges consecutive autosaves of a file: an autosave replaces the file's last
  snapshot when that is an autosave whose run began less than 5 minutes ago, so idle autosave
  no longer floods the list. Saves and snapshots taken before git operations, reloads and
  deletes are never merged and end a run.
- Local history snapshots the modified and untracked files of a worktree before Force remove
  in the Worktrees window deletes it ("Before git worktree remove --force"), as it already did
  before git operations that rewrite the open project.
- Local history follows renames made in the Project tree: a renamed file, or every file under a
  renamed folder, keeps its snapshots under the new name, below a "Renamed from …" row. Save As
  starts a new history whose first snapshot names the file it came from. Renames made outside
  jetl, by git or a shell, are not followed yet.

### Removed

- `editor.cjk_ambiguous_wide`, which was reserved and had no effect. Configs that still set it
  load as before (unknown keys are ignored).

### Fixed

- A reformat still running when its file is renamed or moved in the Project tree no longer lands
  on the moved tab; a save waiting for it writes the text as it was.
- The hint shown in an editor without tabs wraps onto centred lines in a narrow pane instead of
  being cut at the right edge.
- A dialog with many buttons is wide enough for all of them.
- With 256 colors (`ui.colors = "256"`, or no true color detected) the Terminal window no longer
  passes RGB colors through: they are drawn as the nearest palette colors, and the shell no
  longer gets `COLORTERM=truecolor` (a change of `ui.colors` applies to the next shell).
- Plain (F11) bookmarks of a file are kept when its tab closes, like numbered ones: they come
  back when the file opens again, are listed in Shift+F11 as "(closed)", and are saved with the
  session.
- With the kitty keyboard protocol, Ctrl+7 in the editor jumps to numbered bookmark 7 instead of
  toggling a line comment. In legacy terminals and tmux, where Ctrl+/ and Ctrl+7 send the same
  key, it is read as Ctrl+/ everywhere, so a `ctrl-7` binding in `keymap.toml` needs the kitty
  protocol.
- When the config files cannot be watched (for example the inotify limit is reached), a warning
  now says that config changes apply at the next start, instead of hot reload silently not
  happening.
- Crash recovery dumps of projects that are never opened again no longer stay forever: dumps
  older than 30 days are removed when jetl starts (the opened project's own dumps are still
  offered).
- When jetl stops on a terminal error (a failed redraw) instead of a panic, unsaved buffers are
  still written as a crash recovery dump and offered when the project opens again.
- When jetl stops on a terminal error, it now also cleans up as a normal quit does: the session
  is saved, a running task and the Terminal shell are stopped with everything they started, and
  language servers are shut down.
- In a narrow Git window the diff header (the file name and what it compares) is cut with `…`
  before the `[Unified]` and `Collapse` toggles instead of running under them. In the Log and
  Stash tabs a long header no longer hides the toggles.
- Renaming or moving an open file in the Project tree (or a directory with open files in it)
  now sets each tab up for its new path, as Save As does: renaming `a.txt` to `a.rs` gives Rust
  highlighting and the Rust language server, and the `.editorconfig` settings and soft wrap of
  the new path apply. What the new path does not change stays as it was: a language, indent or
  soft wrap picked by hand, folds, carets, unsaved edits and undo history.
- Vertical tab, form feed, NEL (U+0085) and the Unicode line and paragraph separators (U+2028,
  U+2029) no longer start a new line in the editor: lines are broken only at `\n`, `\r\n` and
  `\r`, as language servers, git, compilers and find in files count them. Line numbers, git
  gutter and blame marks, diagnostics, go to definition and LSP edits in such files now land on
  the right line, and `didChange` no longer desyncs the server. The characters are shown inline
  as dim escapes (`^K`, `^L`, `\u{85}`, `\u{2028}`, `\u{2029}`), and the file is saved byte for
  byte as before. Positions remembered in a session for such a file may be a few lines off once.
- Language servers that turn on features after they start (dynamic registration) now get
  them used: formatting, rename, context actions, completion (with its trigger characters),
  go to definition, find usages, documentation, file structure and commands registered for
  the file's language or path work, and stop when the server unregisters them. jetl now tells
  servers it accepts such registrations for these features.

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

[Unreleased]: https://bitbucket.org/BrilllianD/jetl-ide/branches/compare/HEAD..v0.2.0
[0.2.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.2.0/
[0.1.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.1.0/
