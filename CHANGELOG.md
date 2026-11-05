# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Rename dialog (Shift+F6 with a language server): the check "Search in comments and strings" (Alt+C) adds whole-word matches of the old name in comments and strings to the preview; they are guesses, so they start excluded and are marked `text`, and Replace All writes only the ones you include.
- Rename (Shift+F6) with a language server shows its answer in the Find window before anything is written: one result per text edit, grouped by file; Alt+Delete leaves a result or a whole file out, Replace All (Alt+A) lists the file operations (create, rename, delete) and applies the included edits and all operations as one workspace edit (undo and Local History as before). A file changed since the preview refuses the whole rename. `[lsp] rename_preview = false` (Settings > Language Servers) applies the rename at once as before.
- SCIP indexes: set `[index] scip` to an `index.scip` (for example from `rust-analyzer scip .`) and Go to Definition and Find Usages use its exact results without a language server, in files not changed since the index was built.
- `[index] scip_command` and Run > Build SCIP Index run the indexer in the Run window and load the index again when it succeeds; without a language server the call hierarchy follows the SCIP index's exact references in files it covers.
- Structural search in Find in files: Alt+T (or the AST button) treats the query as an ast-grep code pattern, so `$A.unwrap()` finds every `.unwrap()` call and `foo($$$ARGS)` every call of `foo`, in each file whose language has a grammar in the build; match case, whole word, regex and replace do not apply while it is on. Behind the default cargo feature `structural`.
- Named tool window layouts: View | Save Current Layout as New… stores where the tool windows are (docks, sizes, strip order, Pinned, Unpinned and Undocked modes) under a name; Switch Layout… (also from Find Action) applies one; Save Changes in Current Layout, Rename Layout… and Delete Layout… manage them. Layouts are shared by every project, in `layouts.json` in the state directory.
- Rename (Shift+F6) works without a language server: it opens Find in files on the name (case sensitive, whole word) with the new name as replacement and a preview of every change; results the symbol index does not know as a use or definition of the name start excluded, and nothing changes until Replace All.
- Call Hierarchy (Ctrl+Alt+H) works without a language server: callers and callees come from the symbol index (calls matched by name, the caller's file and its imports first), and the window title says "(approximate)".
- Hierarchy: Navigate > File Dependencies shows the files the active file imports in the Hierarchy window (Tab: the files that import it), from the symbol index, expandable level by level and exportable as Mermaid or DOT.
- Undocked tool windows: View Mode > Undock in a strip button's right-click menu (or View | Undock or Dock Tool Window, `tool_window.toggle_undocked`) makes a window slide over the editor at its side instead of shrinking it. It hides on Esc or when another tool window takes focus, and only one is shown at a time. The strip menu's Pin/Unpin row is now the View Mode submenu (Dock Pinned, Dock Unpinned, Undock).
- Tool windows dock in six places: Left Top, Left Bottom, Bottom Left, Bottom Right, Right Top and Right Bottom. Two windows on one side share it, split by a draggable splitter. Use the strip button's right-click menu (Move to), View | Move Tool Window to …, the actions `tool_window.move_left_bottom`, `move_bottom_right` and `move_right_bottom`, or drag a button under its strip's top group. The strip shows each group, with a separator between a side's two groups, and Bottom Right windows at the foot of the right strip.
- Structure window, breadcrumbs, Go to Symbol and the symbol index (Find Usages without a language server) now cover C#, Kotlin, Ruby, PHP and Swift.
- Hierarchy: Export Hierarchy as Mermaid and as DOT (Navigate menu, Find Action) write the loaded call or type hierarchy to a new scratch file (`.md` with a mermaid block, or `.dot`); nodes not expanded yet end with `…`.
- Find Usages (Alt+F7) works without a language server: it lists the uses of the name at the caret found by the symbol index, the current file first, then files it imports or that import it, then its directory, marked "(unverified)" in the Locations title.
- Toolbar: a row at the top of the window (`[ui] toolbar`, on by default, also in Settings > Appearance) holds the main menu button, which leaves the tool strip; hidden in zen mode and in terminals under 12 rows.
- Toolbar: View | Toolbar, the action `app.toggle_toolbar` and the toolbar's right-click menu (Hide Toolbar) show or hide it; the choice is saved in `config.toml` as `[ui] toolbar`.
- Toolbar: the run widget on the right shows the selected run configuration (click to pick one) and ▶ to run it; while a task runs it shows a spinner with the task's name, ↻ Rerun and ■ Stop.
- Toolbar and Run window: a click on the run configuration selector opens a dropdown under it listing the configurations (picking one selects it without running), then Edit Configurations…, Rerun Failed Tests and Stop; Alt+Shift+F10 (`run.pick`) still opens the Run palette, which picks and runs.
- Toolbar: a branch widget (` ⎇ main ▾ `, with the operation in progress such as `Rebasing 2/3`) opens the branches popup; while it shows, the status bar leaves out the branch.
- Toolbar: Update Project (↓), Commit (✓) and Push (↑) buttons after the branch widget run `git.pull`, `git.commit_box` (opens the Commit tab and focuses the message; nothing is committed by the click) and `git.push`; they show in a git repository from 100 columns and drop after Settings and Search when the row gets narrow.
- Toolbar: a Project widget (the project's directory name, with the main project's name when it is a linked worktree) opens Recent Projects; Search Everywhere and Settings buttons sit at the right end. A narrow window drops Settings, then Search, then shortens the Project widget to ▾.
- Tool strip: drag a button to reorder it, or onto the other strip (or the other group) to move its window there; the drop target is highlighted (theme key `strip_drop`). The button's right-click menu has Move Button Up and Move Button Down. The order is saved in the session.
- Restore Default Layout (Shift+F12, `tool_window.restore_default_layout`; View menu and the tool strip button's right-click menu): every tool window goes back to its default side, the strip buttons to their default order, and a maximized window is restored; shown windows stay shown and panel sizes are kept.
- Tool windows can be unpinned (strip button right-click Unpin, View | Pin or Unpin Tool Window, `tool_window.toggle_pinned`): an unpinned window hides when focus moves to the editor or another window and on Esc; palettes, dialogs and menus over it keep it. The mode is saved in the session; Restore Default Layout pins every window.
- Replace in files previews each change in the Find window (the matched text and its replacement side by side on every result row) and Alt+Delete (`find.toggle_exclude`) excludes or includes the selected result, or all results of a file from its header; Replace All changes only the included results at their exact positions and leaves a file that changed since the search untouched, with an error.
- Input dialogs (Rename, Save As, New Worktree and the others with one text field): Ctrl+A selects the whole text; typing, pasting, Backspace or Delete then replaces it, and Left, Right, Home, End or a click in the field drops the selection.

### Changed

- `tool_window.move_left`, `move_bottom` and `move_right` move to Left Top, Bottom Left and Right Top. Dropping a strip button on the empty part of a strip now puts it at the end of the nearest group (the upper half of the gap splits the side) instead of in the place of the nearest button.
- Faster start on large projects: the symbol index (Go to Symbol, Find Usages without a server, import edges) is cached per project under the cache directory and only files whose size or modification time changed are parsed again.
- Tool strip buttons toggle their window on mouse release instead of press.
- Tool strip: when the two-row buttons do not fit (24-row terminals), every button is one row with only its icon, so all 13 tool windows have a button at 80x24; on a shorter strip its last row is a `⋯` More button whose menu lists the windows left without a button.

### Fixed

- Tool strip: dropping a button on the separator between a side's two groups no longer moves it out of its group; it stays in the dragged window's group, and the drop colour shows the target first.
- Tool strip: on a terminal only a few rows high a strip now shows the More button (⋯) instead of no way to reach its tool windows.
- Symbol index cache: a binary or oversized source file no longer rewrites the cache on every start, and updating a tree-sitter grammar now discards the old cache instead of keeping symbols extracted by the previous version.
- Concurrent saves of the same file could corrupt it: every atomic write now uses its own temp file.
- Git Log: a context menu opened from the keyboard (Shift+F10) on a commit, a file or a branch now opens directly under the selected row, or directly above it when it does not fit below, instead of one row lower or on top of the row.
- Status bar: a quiet log entry (such as a manual save) no longer blanks a still-current error or warning message; the slot keeps the newest non-quiet message until its timeout.
- Status bar: the current branch stays visible while the main menu bar covers the toolbar's branch widget.
- Scratch files: when writing a new scratch file's text fails (Export Hierarchy), the empty file no longer stays behind in Scratch Files holding its number.
- File Dependencies: a Java wildcard import lists the package's files from every source root, so a package split across `src/main/java` and `src/test/java` shows both sides, and a file alone in its package in one root still finds the other side.
- Import edges: the first File Dependencies or file ranking after indexing a large Java project no longer spends seconds on imports of external packages.
- Approximate call hierarchy: listing the callees of a long function in a large project no longer pauses the editor on repeat lookups; the first use after a change still rebuilds the import edges.
- The terminal cursor no longer shows on top of an Undocked tool window when the editor caret lies under it.
- Tool window layouts: the success toast and the current layout now wait for the write to succeed, so a failed save, rename or delete only shows its error and leaves the current layout as it was; rename and delete now confirm with a toast.
- SCIP indexes: an `index.scip` above 256 MiB is refused with one warning instead of loading it; it would take about 7 times its size in memory. After a load the freed parse memory is returned to the system: jetl's own 34 MiB index stays at 2.1 times its size instead of 3.6.

## [0.8.0] - 2026-11-03

### Added

- Git window: Ctrl+D (`git.show_diff`) on a file in the Commit, Log or Stash tab opens its diff side by side in a full-size editor tab (header toggles for layout and collapse, F7 / Shift+F7 between changes, Alt+N / Alt+P between files); the Log file menu has Open Diff in Editor Tab. In the Log list Ctrl+D shows the selected commits' files in one such tab, file by file.
- Git Log: the selected commit's files are a folder tree with one root (Left/Right close and open folders) and a context menu on every node: Show Diff, Open Diff in Editor Tab, Open in Editor, Copy Path, Show History, Expand/Collapse All, and the commit's Cherry-Pick, Revert, Checkout and Copy Revision Number.
- Git Log: with several commits selected the details list the union of their files as one tree, and a file's diff is its combined change across the selection (from the oldest selected commit's parent to the newest; a non-contiguous selection is marked in the header).

### Changed

- Git Log Show Diff (Ctrl+D) opens the selected commits' files side by side in one editor tab (Alt+N / Alt+P between files); the unified patch moved to Show Diff as Patch (`git.log_show_patch`, tab `Patch <sha8>`). Compare with Clipboard / File open a side-by-side diff tab instead of a patch (no longer saveable as a file). Diff tab titles name their source: `(staged)`, `(new)`, `(<sha8>)`, `(<base>..<top>)`.
- A conflicted file opened from the project tree or the Commit tab's Changes opens the three-pane conflict resolver; selecting it in Changes shows a hint instead of a diff of its conflict markers.
- Closing the last editor tab moves focus to the project tree and shows the Project tool window, so the arrow keys move the tree selection at once. Closing a tab while others stay open keeps focus in the editor, and closing the conflict resolver does not move focus.
- Saving a file shows no toast and no status bar message; the save is only listed in the Event Log (Alt+Shift+L).
- `editor.autosave` defaults to `"focus"`: modified files are saved when the terminal loses focus. Set `autosave = "off"` to save only with Ctrl+S.

## [0.7.0] - 2026-11-01

### Added

- Overwrite mode in the editor: Insert toggles it, and typed characters replace the character under the caret instead of being inserted (at a line end and over a selection they are inserted as usual). It is off on every start. While it is on, the status bar shows OVR and the caret is a block in terminals that support caret shapes; your own caret shape comes back when overwrite is off, when the editor loses focus and when jetl exits.
- Settings › Keymap's Add Key and Edit Key dialogs record the key as you press it (Ctrl+V is recorded, while Tab and Shift+Tab move between the dialog's parts and are typed in the text field like Esc); a Second stroke toggle records a two-key chord such as Ctrl+K Ctrl+C. Keys the terminal cannot deliver can still be typed in the text field below, as before.

### Fixed

- Files under `/etc/nginx/` whose extension belongs to another language, such as `.lua`, `.json` or `.sh`, are highlighted as that language instead of nginx; extensionless files, `.conf` files and files such as `mime.types` stay nginx.
- Changing a key on Settings › Keymap rewrites that entry of `keymap.toml` where it stands, keeping its comments, instead of removing it and appending the new key at the end of the table.

## [0.6.0] - 2026-10-29

### Added

- Settings › Keymap lists every key binding as a tree of contexts, actions and keys, with a search over action names, ids and keys; the Vim keymap mode setting moved to Settings › Keymap › Mode.
- Settings › Keymap edits key bindings: Enter or Insert on an action adds a key, Enter on a key changes it and Delete removes it; a key already bound in that context asks before it is replaced. Changes are marked and written to `keymap.toml` on OK or Apply, keeping your other entries and comments; Cancel discards them. `keymap.toml` also accepts keys spelled as shown (`Ctrl+Shift+F`).
- Settings › Keymap marks conflicting keys: `^` on a key that overrides another action bound to the same key in an enclosing context (such as Global), `!` on a key that starts a longer key of the same context and on that longer key, which can never fire. The selected key names the other binding, and staged edits show their conflicts before OK.
- The Git Log context menu's `Branch '<name>'` rows open a submenu with every operation for that branch (checkout, new branch, compare, merge, rebase, push, rename, delete); it opens on Right, Enter or mouse hover and closes on Left or Esc. Operations git refuses during a merge or rebase are shown disabled.
- File-type icons before file names in the Project tree, on editor tabs, in Go to File, Recent Files and Search Everywhere file rows, in the Find, Problems, TODO and Locations file headers and in the Git Commit and Log file lists, coloured per file kind from the theme's syntax colours. The new `ui.file_icons` setting (Settings > Appearance > File icons) picks Nerd Font glyphs ("nerd", the default, needs a Nerd Font), one ASCII character per file kind ("ascii") or none ("none"), and applies at once.
- The Terminal window always shows its tab row, also with a single tab, with New and Close buttons on the right; a middle click on a tab label closes that tab.
- Syntax highlighting for Makefiles (`Makefile`, `GNUmakefile`, `*.mk`) and INI-style config files (`.ini`, `.cfg`, `.conf`, `.gitconfig`, `.editorconfig`, systemd units and more); `.env` and `.env.*` files highlight as shell, and more file names use the JSON, TOML, YAML, XML and shell grammars. An exact file name now always wins over another language's extension.
- Syntax highlighting for CMake (`CMakeLists.txt`, `*.cmake`), diff and patch files, Java `.properties`, pip requirements files (`requirements*.txt`), nginx configs (`nginx.conf` and everything under `/etc/nginx/`) and Git commit messages (`COMMIT_EDITMSG`).
- Syntax highlighting for SQL (`*.sql`), Ruby (`*.rb`, `Gemfile`, `Rakefile`, `Vagrantfile` and `#!/usr/bin/env ruby` scripts), Kotlin (`*.kt`, `*.kts`), Swift (`*.swift`) and Zig (`*.zig`, `*.zon`).
- Syntax highlighting for HCL and Terraform (`*.tf`, `*.tfvars`, `*.hcl`), Nix, Protocol Buffers (`*.proto`) and GraphQL (`*.graphql`, `*.graphqls`, `*.gql`).
- The Git Log shows a branch tree on its left (HEAD, Local and Remote by remote, with collapsible groups). Clicking a branch, or Space on it, lists that branch's history; Enter, Shift+F10 or a right click opens its operations menu. Left and Right move between the tree and the commit list. The tree hides when the Git window is narrower than 100 columns.

### Changed

- A new Terminal tab takes the lowest number no open tab uses, so after closing Local (2) the next new tab is Local (2) again; it is still added at the end of the tab row.
- The current branch moved to the right end of the status bar; a click on it still opens the branches popup.
- The status bar shows the active file's path relative to the project, such as src › app › mod.rs, in place of the project name; a long path is cut from the start.

### Fixed

- A click in the fold column of a Markdown preview tab no longer folds the rendered text.
- Semantic highlighting and inlay hints now appear as soon as a language server turns them on after startup, without an edit or a tab switch first.
- Send HTTP Request no longer reads ~/.curlrc, so a proxy or other options set there do not change the request; a stray }} before a {{ on the request line no longer joins the rest of the line into one word; and when the request body file cannot be created, jetl no longer deletes a file of the same name it did not create.
- Refresh in the Git window (F5 or the Refresh button) and the app-wide Refresh now reload a shown Log, so commits made outside jetl, for example from a shell, appear without switching tabs.

## [0.5.0] - 2026-10-27

### Added

- HTTP requests from .http files: Ctrl+Alt+R (Run > Send HTTP Request at Caret) sends the request at the caret with curl and shows the status line, headers and body in the Run window under the request's name; requests are separated by ###, named by # @name, and {{variables}} come from the dev environment of http-client.env.json. Rerun sends it again; without curl in PATH you get a note.
- Interactive rebase from the Git window's Log tab: select a commit and press Alt+Shift+I (or right-click, Interactively Rebase from Here…) to edit the todo of that commit and the later ones in a dialog: reorder with Alt+Up/Down, pick, reword, squash, fixup or drop with p, r, s, f, d, and edit a reworded message in place; Enter runs `git rebase -i` with that todo. Squash keeps git's combined message. A dirty tree, an operation in progress, a commit off the current branch or merges in the range are refused with a warning; conflicts stop the rebase as usual (Continue, Skip, Abort).
- Semantic highlighting: with a language server that sends semantic tokens (rust-analyzer), parameters, mutable variables (underlined), type parameters, modules, macros and other kinds are coloured by the server on top of tree-sitter, and the colours stay in place while typing until the next answer. The built-in themes gain `variable.parameter`, `variable.mutable`, `type.parameter` and `module` `[syntax]` entries; `[lsp] semantic_tokens = false` (Settings > Language Servers) turns it off.
- Markdown preview (Alt+Shift+V in a Markdown file, View > Markdown Preview): a read-only tab renders the file's headings, lists and task boxes, quotes, emphasis, inline code, links, highlighted code blocks, tables and rules, and follows your edits as you type while keeping its scroll; images show as [image: alt]. Previews are not saved in sessions.
- Line staging in the Git window's Commit tab: in the diff, Alt+Up/Down puts a line cursor on the changed rows and Alt+Shift+Up/Down grows the selection (a double click on a changed row sets it); Space then stages only the selected lines, or unstages them in a staged file's diff, and Ctrl+Alt+Z discards them from the working tree after a confirmation. Esc leaves line mode; in the commit message box Alt+Up/Down still recall earlier messages.
- Call Hierarchy (Ctrl+Alt+H, Navigate > Call Hierarchy) and Type Hierarchy (Ctrl+Alt+Shift+H with the kitty protocol, Navigate > Type Hierarchy) show the callers or subtypes of the symbol at the caret in a Hierarchy tool window, as a tree whose rows load their children when expanded (Right); Tab switches to callees or supertypes, Enter opens the selected function or type, F5 asks the language server again. The window is not saved in sessions.
- Terminal tabs: Ctrl+Shift+T (Alt+Shift+T without the kitty keyboard protocol) in the Terminal window, or View > New Terminal Tab, starts another shell in a new tab; Ctrl+F4 ends the active tab's shell and closes it (closing the last one hides the window). Alt+Right / Alt+Left or a click on the tab row switch tabs, so the shell no longer receives Alt+Left/Right; map them to "none" in the `[terminal]` table of keymap.toml to give them back.
- A TODO tool window (View > TODO) lists the lines of the project's files that hold the word TODO or FIXME (case-sensitive), grouped by file, with the count in its title; Enter or a click opens the line, F5 scans again. The scan skips hidden and ignored files as Find in Files does and runs when the window is first shown.
- Compare with Clipboard and Compare with File… (View menu, also in Find Action): the unified diff of the active file, or of its selection, against the clipboard or another project file opens in a read-only diff tab (F7 steps through the changes); identical contents give a hint instead.
- Quick Definition (Ctrl+Shift+I, or Alt+Shift+I in terminals without the kitty protocol; View > Quick Definition): the definition of the symbol at the caret, its file and line and its code, in a popup without leaving the file; Esc closes it.
- Refactor This (Alt+Shift+T, Ctrl+Alt+Shift+T with the kitty protocol, Edit > Refactor This…) lists the language server's refactorings at the caret or selection; Optimize Imports (Ctrl+Alt+O, Edit > Optimize Imports) runs the server's organize imports action for the file, or lets you choose when it offers several.
- Sticky scroll (`[editor] sticky_scroll = true`, or Settings > Editor > General): the header lines of the definitions you scrolled into (impl, function, class, heading) stay pinned at the top of each editor pane; click one to go to it; theme style `sticky_scroll`.
- Recent Locations (Ctrl+Shift+E, Navigate > Recent Locations…): the places the caret recently was, from the back/forward history, newest first, with each line's code; type to filter, Enter jumps (a closed file reopens there).
- Inline blame (`[editor] inline_blame = true`, also in Settings > Editor > General): the caret line shows its author, relative date and commit summary after the text, in the focused pane, for lines unchanged since HEAD.
- Breadcrumbs (`[editor] breadcrumbs = true`, or Settings > Editor > General): a row at the bottom of each editor pane shows the definitions around the caret, such as `impl Point › fn new`, for languages with a structure (tags) query, with short kind labels (`fn`, `impl`, `class`, ...) and bare Markdown headings; a click on a crumb jumps to that definition; theme style `breadcrumbs`.
- New Task dialog (Alt+Insert in the Tasks window, File > New Task…, or the Tasks context menu): title, tags with completion from `yman tags`, priority, assignee, related ids and a multi-line body; runs `yman add` and selects the new task.
- The Tasks window title shows the tracker against its remote (↑ to push, ↓ to pull, in sync, local only, or an unresolved sync merge), and Ctrl+Alt+Y (or Sync Tasks in its context menu) runs yman sync; a local-only tracker is not synced, and a conflicted merge says how to finish it.
- The Tasks window changes the selected task: status with an optional note (Alt+M), priority (Alt+P), assignee (Alt+Shift+A), tags (Alt+Shift+T) and comments (Alt+N), also from its context menu; yman's errors and notes show as notifications.
- A Tasks tool window (View > Tasks) lists the project's yman tasks: open ones by default (Alt+C shows closed), filter by typing, by tag (Alt+T) or assignee (Alt+A); the selected task's body, related tasks (Alt+J or a click jumps), attachments and discussion show beside the list. Filters and the selection are saved per project.
- Tool windows can sit on the left, the bottom or a new right side: right-click a tool strip button, or View > Move Tool Window to Left / Bottom / Right (also in the action palette). A second strip on the right edge appears while a window is on the right; the choice is saved per project.
- Alt+Right / Alt+Left switch the tabs inside the focused Git window (Commit, Log, Stash, Console, wrapping) and flip the Run window's Output and Tests views (View > Next / Previous Tool Window Tab). In these windows they no longer switch editor tabs; the commit message box keeps them for the caret.
- Tool windows: Alt+Shift+M (Ctrl+Shift+' with the kitty protocol, View > Maximize Tool Window, or a double-click on the window's top border) makes the focused tool window fill the area right of the strip; focusing anything else restores the layout. F12 jumps to the last active tool window (View > Jump to Last Tool Window), showing it if hidden. Alt+5 toggles the Event Log and Alt+8 Local History, and the tool strip shows their numbers. With the editor focused, Ctrl+Shift+Up/Down resize the bottom tool window.
- Alt+W then an arrow focuses the tab group on that side of the focused one (View > Focus Left Group and the others); Alt+W M moves the tab to the opposite group, Alt+W U / Shift+U unsplit and unsplit all, Alt+W O / A / P close other, all and unpinned tabs. The find bar and the Find window keep Alt+W for Whole Words.
- Switcher popup (Ctrl+Tab, Ctrl+Shift+Tab, Navigate > Switcher…): recent tabs and the tool windows in two columns; Tab or the arrows move the selection, releasing Ctrl, Enter or a click opens it, Esc cancels. Releasing Ctrl needs a terminal with the kitty keyboard protocol, as Ctrl+Tab itself does.
- The open tabs, splits and active tab follow the git branch: switching branches (in jetl or with `git switch`, also while jetl is closed) saves the branch's set, closes the clean tabs and opens the other branch's saved ones; dirty tabs stay open, a branch not used before keeps the current tabs, a detached HEAD changes nothing.
- The session (open tabs, carets, layout) is saved every few seconds while jetl runs, so a crash no longer loses the tabs opened since the start.
- Test runner tree: jest runs (npx jest, yarn jest) show the Tests view too, with Rerun Failed and Run Test at Caret; jetl adds --verbose to a jest command given as a list of words.
- Live templates and language server snippets fill in variables: `$TM_FILENAME`, `$TM_LINE_NUMBER`, `$CURRENT_YEAR`, `$CURRENT_DATE`, `$UUID` and the rest of the VS Code set except `$CLIPBOARD`; unknown ones still insert their default.
- Surround With (Ctrl+Alt+T, Edit > Surround With…): wrap the selection, or the caret line, with a live template that uses `$TM_SELECTED_TEXT`; a selection that covers whole lines (it may end at or inside the leading indent of the next line) wraps those lines.
- More built-in live templates for Surround With: `for`, `ifelse` and the other block forms for Rust (`loop`, `unsafe`), Python (`while`, `with`) and TypeScript (`while`), and new built-in JavaScript and Go snippets (Go: `func`, `iferr`, `if`, `ifelse`, `for`, `forr`).
- Snippet variable transforms: `${TM_FILENAME/(.*)\..+$/$1/}` rewrites a variable with a regex and a format (`$1`, `${1:/upcase}`, `${1:+if}`, `${1:?if:else}`; options `g`, `i`, `m`, `s`) as in VS Code.
- Structure window (Alt+7) lists Markdown headings: ATX headings nest by level, setext headings sit at the depth of their enclosing section.
- `jetl --doctor` lists the optional tools jetl uses and whether they were found: language servers, formatters, clipboard tools, compiled grammars, kitty keyboard protocol and true color support, git and yman.
- Settings > Dependencies shows the same check as `jetl --doctor` inside jetl: which language servers, formatters, clipboard tools, grammars and terminal features are available. Enter checks again.

### Changed

- Debug: Trigger a Panic moved from F12 to Ctrl+Alt+Shift+F12; F12 now jumps to the last active tool window.

### Fixed

- The Tasks window opened on a linked git worktree lists the repository's yman tasks: yman runs in the main checkout, where the tracker lives, instead of reporting that the project has no tracker.
- Starting jetl, or switching root, with no tab open and no file on the command line shows the Project window and focuses it instead of leaving focus on an empty editor.
- Rerun Failed on a jest run configuration whose command is one string drops the string's own `-t`/`--testNamePattern` (after the `jest` word), so the rerun has one `-t`; a string with quotes, a backslash, a line break or a shell operator keeps it.
- Markdown: a document that starts with `---` (or `+++`) and has a code fence before the next `---` line is no longer taken as YAML (or TOML) front matter; the fence and the rest are highlighted as Markdown.
- Stage and Continue shows the plain command (`git add a.txt && git rebase --continue`) in the status bar, the Git console and Local History instead of git's pathspec syntax.
- Inlay hints: the caret at a type hint's place is drawn before the hint, so typing there no longer shows the caret on the far side of the hint (parameter name hints keep the caret after them).
- Live templates: Tab no longer expands a snippet prefix inside a string or a comment (Ctrl+Space and Edit > Expand Live Template still do).
- Structure window and symbol search: Markdown headings inside a block quote or a list item (`> # Q`, `- # Item`) are no longer listed as symbols.
- In a linked git worktree, jetl now sees changes to the main repository's refs, `packed-refs` and other worktrees (yman's tracker writes included), so git status and the Tasks window refresh after them.
- Settings > Dependencies no longer says Checking… when a check started before a root switch is dropped; the page says "Not checked. Enter checks." until Enter runs the check again.

## [0.4.0] - 2026-10-17

### Added

- Run > Run Test at Caret (Ctrl+Shift+F10) runs the Rust, Python or JavaScript/TypeScript test around the caret (cargo test, pytest, vitest or jest). Shift+F10 now runs the selected configuration; Ctrl+F5 still reruns the last run.
- Test runner tree: `cargo test`, `pytest` and `vitest` runs show a Tests view in the Run window (suites, modules or classes, tests; passed, failed and ignored counts), failed tests expanded with their output and a link to the assert; Rerun Failed reruns only those.
- Run > Edit Configurations… (also in the Run window's toolbar): a form for the project's run configurations, saved to `.jetl/run.toml` with its comments kept; config.toml and detected ones can be copied into the project.
- The Run window has a toolbar with a configuration picker, Run, Rerun and Stop; Shift+F10 runs the selected configuration (the last one picked, remembered per project), Ctrl+F5 reruns the last run.
- Run configurations: `[[run.configs]]` in config.toml and `[[configs]]` in `.jetl/run.toml` take `args` and `matcher` (`problems`, `none`) besides name, command, cwd and env; edits to either file apply at once.
- Live templates: Tab right after a snippet's prefix expands it (tabstops walked with Tab, as for completion snippets), and matching snippets are listed in the completion popup after the language server's items (Ctrl+Space lists them even without a server). Built-ins for Rust (`fn`, `test`, `impl`, `match`), Python (`def`, `class`, `ifmain`) and TypeScript (`fn`, `log`); your own in `~/.config/jetl/snippets/<language>.toml` and `all.toml`; edits to the snippet files apply at once; `[editor] snippets = false` turns live templates off.
- Inlay hints from the language server (types, parameter names) drawn in the text for the visible lines; soft wrap counts them and a click on one puts the caret at its place. They are asked for again after a request timed out and when the server finishes loading. `[lsp] inlay_hints` turns them off. View > Inlay Hints shows or hides them for the session.
- Indent guides: a thin line at each indent level in the leading whitespace, continued through blank lines inside a block (`editor.indent_guides`, on by default; theme style `indent_guide`).
- Edit > Sort Lines, Reverse Lines, To Upper Case and To Lower Case (also in the actions palette): on the selected lines, or the whole file when nothing is selected; one undo step.
- Markdown code fences are highlighted as their language, and HTML and Vue `<script>` / `<style>` blocks as JavaScript (or TypeScript with `lang="ts"`) and CSS (or SCSS); folding, bracket matching and Extend Selection follow the embedded language.
- Go to Symbol also lists the language servers' workspace symbols (merged with the built-in index) and opens them at the right column.
- A language server's question with buttons (`window/showMessageRequest`) opens a choice dialog; the pick goes back to the server.
- View > Parameter Info (Ctrl+P): the signatures of the call at the caret with the current parameter bold; it opens by itself after `(` or `,` when the language server supports it and follows the caret while open.
- Navigate > Type Declaration (Ctrl+Shift+B, Alt+Shift+B), Implementation(s) (Ctrl+Alt+B) and Declaration through the language server; several results open the Locations palette.
- Git > Push… opens a Push dialog with Tags and Force with lease toggles (a force push is always `--force-with-lease`, never a plain force). The Stash tab has an Untracked toggle that stashes untracked files too.
- Git Log: Reset Current Branch to Here… in the commit menu (Soft, Mixed or Hard; Hard asks first and Local History keeps a copy of the changed files).
- Column selection: Alt+click past a line end puts a caret on that cell (virtual space), and typing there pads the line with spaces. In column selection mode a click past a line end does too. Right and Left step through the space past a line end, and Up/Down keep each caret's column over short lines. Backspace, Ctrl+Backspace, Delete and Ctrl+Delete there move the caret without changing the text.
- Settings window (Ctrl+Alt+S, File > Settings…): a page tree of the `config.toml` keys, each row tagged with when a change applies (new tabs, next shell, restart servers, restart jetl). Edits keep the file's comments and order; Live keys apply on Apply, Ctrl+S or OK.
- Git Log: Ctrl+D (or Show Diff in the commit menu) opens the diff of the selected commit in a read-only editor tab with green and red lines; F7 / Shift+F7 step through the changes. The tab can be saved as a file.
- Patch tabs (Git Log Show Diff) fold by commit, file and hunk: each `==`, `diff --git` and `@@` line has a fold marker, and Collapse All leaves one row per commit.
- Git Log: Shift+Up / Shift+Down select a range of commits, Shift+click extends it and Ctrl+click toggles a row. Show Diff on several commits opens one tab: each run of commits that follow one another by first parent is one diff from its oldest commit's parent to its newest commit, the runs one after another under `==` headers.
- The Git Log commit menu has a `Branch '<name>' ▸` row for each branch at the commit; it opens that branch's operations. The Branches popup (Alt+`) gained Compare with the current branch (the Log of `HEAD..<branch>`) and Push, and Merge asks first.
- The Git window header and the status bar show a merge, rebase (with its step, `Rebasing 2/5`), cherry-pick, revert or bisect in progress; while one runs, the Log commit menu greys out Checkout, New Branch, Cherry-Pick and Revert, and the branches popup hides checkout, merge and rebase.
- Git > Continue Operation, Skip Commit and Abort Operation (also in the Git window header and the Actions palette while a rebase, merge, cherry-pick or revert is in progress). Continue with unresolved files offers Resolve… or Stage and Continue; Abort asks first and keeps a Local History copy.

### Changed

- Ctrl+Alt+B is now Implementation(s), as in JetBrains; Git blame stays in View > Git Blame and the actions palette.
- Committing from the Commit tab is refused during a merge, rebase, cherry-pick or revert; use Continue Operation, which makes the right commit.
- The Git Log lists all branches by default (`[git] log_all = true`); stash commits are left out of it. Set `log_all = false` for HEAD only.

### Fixed

- Git Log no longer lists commits from refs outside branches, remote branches and tags (such as yman's `refs/yman/*`).
- Typing right after turning soft wrap on with a column block no longer pads short lines with spaces.
- Git Log: a Ctrl+click on the last loaded row fetches the next page, like the other ways to move there. Patch tabs no longer show indentation fold markers.
- Dialogs no longer panic on a terminal smaller than the dialog's minimum size.
- A document that a language server closed through a `didOpen` unregistration no longer gets `didSave` until it is opened again.
- Switching the project root while a Revert to Label still waits for closed files to open now names each file that was not reverted instead of dropping it silently.

## [0.3.0] - 2026-10-13

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
- Column selection has virtual space past line ends: typing, pasting and Tab pad lines shorter than the block with
  spaces up to its column, Backspace there moves the block's column left without touching the short lines, Delete
  joins nothing there, and Enter breaks a short line at its end without padding. The block is highlighted past line
  ends, its carets are drawn on its column, and Shift+Right in column mode goes on past the widest line. Alt+click
  into virtual space and virtual space without a column block are not supported yet.
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

- Tab with spaces goes to the next indent stop by screen column, so a tab before the caret counts as its width.
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

[Unreleased]: https://bitbucket.org/BrilllianD/jetl-ide/branches/compare/HEAD..v0.8.0
[0.8.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.8.0/
[0.7.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.7.0/
[0.6.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.6.0/
[0.5.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.5.0/
[0.4.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.4.0/
[0.3.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.3.0/
[0.2.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.2.0/
[0.1.0]: https://bitbucket.org/BrilllianD/jetl-ide/src/v0.1.0/
