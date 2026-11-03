# Key bindings

Generated from the action registry (`src/actions.rs`) and the default keymap
(`assets/keymap.default.toml`) by `cargo run -q -- --dump-keymap > docs/KEYMAP.md`.
Do not edit by hand; `cargo test` fails when it is out of date.

Contexts are searched innermost first (for example `editor.find`, then `editor`,
then `global`). Bindings marked * need the kitty keyboard protocol (foot, kitty,
WezTerm, Ghostty); legacy terminals and tmux have the listed fallbacks instead
(where Alt+Shift+Tab does not arrive, Alt+Tab cycles the tab groups).
Override any of them in `~/.config/jetl/keymap.toml` (see [CONFIG.md](CONFIG.md)).
While the Terminal window has focus only its `terminal` table is searched: every other
key goes to the shell, and Esc Esc at the shell prompt focuses the editor.

## global

| Keys | Action | Description |
|---|---|---|
| `Ctrl+Q` | `app.quit` | Exit jetl |
| `F5` | `app.refresh` | Refresh project and git state |
| `Ctrl+Alt+Shift+F12` | `app.debug_panic` | Debug: trigger a panic |
| `F10` | `app.main_menu` | Main menu |
| `Ctrl+Alt+S` | `app.settings` | Settings… |
| `Alt+1` | `tool_window.project` | Project tool window |
| `Alt+2` | `tool_window.worktrees` | Worktrees tool window |
| `Alt+9` | `tool_window.git` | Git tool window |
| `Alt+3` | `tool_window.find` | Find tool window |
| `Alt+7` | `tool_window.structure` | Structure tool window |
| `Alt+5` | `tool_window.event_log` | Event Log tool window |
| `Alt+Shift+L` | `tool_window.event_log` | Event Log tool window |
| `Alt+4` | `tool_window.run` | Run tool window |
| `Alt+F12` | `tool_window.terminal` | Terminal tool window |
| `Alt+6` | `tool_window.problems` | Problems tool window |
| `Ctrl+Shift+F12` | `tool_window.hide_all` | Hide all tool windows |
| `Alt+Shift+Q` | `tool_window.hide_focused` | Hide active tool window |
| `Shift+Esc` * | `tool_window.hide_focused` | Hide active tool window |
| `Shift+F12` | `tool_window.restore_default_layout` | Restore the default tool window layout |
| `Ctrl+Shift+Left` | `tool_window.stretch_left` | Stretch tool window to the left |
| `Ctrl+Shift+Right` | `tool_window.stretch_right` | Stretch tool window to the right |
| `Ctrl+Shift+Up` | `tool_window.stretch_up` | Stretch tool window up |
| `Ctrl+Shift+Down` | `tool_window.stretch_down` | Stretch tool window down |
| `Alt+Shift+M` | `tool_window.maximize` | Maximize or restore the active tool window |
| `Ctrl+"` * | `tool_window.maximize` | Maximize or restore the active tool window |
| `F12` | `tool_window.last` | Jump to the last active tool window |
| `Alt+Shift+Z` | `app.zen` | Toggle Zen mode (editor text only) |
| `Ctrl+N` | `palette.files` | Go to file |
| `Ctrl+Shift+N` * | `palette.files` | Go to file |
| `Ctrl+Shift+A` * | `palette.actions` | Find action |
| `Ctrl+Shift+P` * | `palette.actions` | Find action |
| `F1` | `palette.actions` | Find action |
| `Alt+S` | `palette.search_everywhere` | Search everywhere |
| `Shift Shift` * | `palette.search_everywhere` | Search everywhere |
| `Ctrl+E` | `palette.recent_files` | Recent files |
| `Ctrl+Shift+E` * | `palette.recent_locations` | Recent locations |
| `Alt+Shift+E` | `palette.recent_projects` | Recent projects |
| `Alt+Shift+N` | `palette.symbols` | Go to symbol |
| `Ctrl+Alt+Shift+N` * | `palette.symbols` | Go to symbol |
| `Ctrl+S` | `editor.save` | Save |
| `Alt+Shift+S` | `editor.save_as` | Save as… |
| `Alt+Shift+W` | `editor.save_all` | Save all |
| `Ctrl+Shift+S` * | `editor.save_all` | Save all |
| `Ctrl+Alt+Shift+Insert` | `editor.new_scratch` | New scratch file… |
| `Alt+Shift+H` | `editor.local_history` | Local history of the file (editor) or the selected entry (Project tree; a folder lists its files), in its tool window |
| `Alt+8` | `tool_window.local_history` | Local History tool window |
| `Ctrl+V` | `editor.paste` | Paste |
| `Alt+Right` | `editor.next_tab` | Next tab |
| `Alt+Left` | `editor.prev_tab` | Previous tab |
| `Ctrl+F4` | `editor.close_tab` | Close tab |
| `Ctrl+W` | `editor.close_tab` | Close tab |
| `Alt+W A` | `editor.close_all_tabs` | Close all tabs, pinned ones too |
| `Alt+W O` | `editor.close_other_tabs` | Close other tabs (keeps pinned ones) |
| `Alt+W P` | `editor.close_unpinned_tabs` | Close all tabs but pinned |
| `Alt+Shift+P` | `editor.toggle_pin_tab` | Pin or unpin the active tab |
| `Alt+Shift+Left` | `editor.move_tab_left` | Move the active tab left |
| `Alt+Shift+Right` | `editor.move_tab_right` | Move the active tab right |
| `Ctrl+Tab` * | `editor.switcher` | Switcher (recent tabs and tool windows) |
| `Ctrl+Shift+Tab` * | `editor.switcher_back` | Switcher, selecting backwards (recent tabs and tool windows) |
| `Alt+\` | `editor.split_right` | Split right (the tab in a new group beside) |
| `Alt+-` | `editor.split_down` | Split down (the tab in a new group below) |
| `Alt+Tab` | `editor.next_group` | Next tab group |
| `Alt+Shift+Tab` * | `editor.prev_group` | Previous tab group |
| `Alt+W Left` | `editor.focus_group_left` | Focus the tab group to the left |
| `Alt+W Right` | `editor.focus_group_right` | Focus the tab group to the right |
| `Alt+W Up` | `editor.focus_group_up` | Focus the tab group above |
| `Alt+W Down` | `editor.focus_group_down` | Focus the tab group below |
| `Alt+W U` | `editor.unsplit` | Unsplit (merge the group into the one beside it) |
| `Alt+W Shift+U` | `editor.unsplit_all` | Unsplit all (one group) |
| `Alt+W M` | `editor.move_tab_to_opposite_group` | Move the active tab to the opposite group |
| `Ctrl+0` * | `editor.goto_bookmark_0` | Go to bookmark 0 |
| `Alt+B 0` | `editor.goto_bookmark_0` | Go to bookmark 0 |
| `Ctrl+1` * | `editor.goto_bookmark_1` | Go to bookmark 1 |
| `Alt+B 1` | `editor.goto_bookmark_1` | Go to bookmark 1 |
| `Ctrl+2` * | `editor.goto_bookmark_2` | Go to bookmark 2 |
| `Alt+B 2` | `editor.goto_bookmark_2` | Go to bookmark 2 |
| `Ctrl+3` * | `editor.goto_bookmark_3` | Go to bookmark 3 |
| `Alt+B 3` | `editor.goto_bookmark_3` | Go to bookmark 3 |
| `Ctrl+4` | `editor.goto_bookmark_4` | Go to bookmark 4 |
| `Alt+B 4` | `editor.goto_bookmark_4` | Go to bookmark 4 |
| `Ctrl+5` | `editor.goto_bookmark_5` | Go to bookmark 5 |
| `Alt+B 5` | `editor.goto_bookmark_5` | Go to bookmark 5 |
| `Ctrl+6` | `editor.goto_bookmark_6` | Go to bookmark 6 |
| `Alt+B 6` | `editor.goto_bookmark_6` | Go to bookmark 6 |
| `Ctrl+7` * | `editor.goto_bookmark_7` | Go to bookmark 7 |
| `Alt+B 7` | `editor.goto_bookmark_7` | Go to bookmark 7 |
| `Ctrl+8` * | `editor.goto_bookmark_8` | Go to bookmark 8 |
| `Alt+B 8` | `editor.goto_bookmark_8` | Go to bookmark 8 |
| `Ctrl+9` * | `editor.goto_bookmark_9` | Go to bookmark 9 |
| `Alt+B 9` | `editor.goto_bookmark_9` | Go to bookmark 9 |
| `Alt+F1` | `tree.select_in_project` | Select in Project |
| `Alt+H` | `git.file_history` | Git: show history for the file (editor) or the selected entry (Project tree) |
| `Alt+0` | `git.commit_box` | Git: open commit window |
| `Ctrl+K` | `git.commit_box` | Git: open commit window |
| `Alt+Shift+K` | `git.push` | Git: push |
| `Ctrl+Shift+K` * | `git.push` | Git: push |
| `Ctrl+T` | `git.pull` | Git: pull (update project) |
| `Ctrl+Space` | `git.branches_popup` | Git: branches |
| `` Ctrl+` `` | `git.branches_popup` | Git: branches |
| `Ctrl+Shift+F` * | `find.in_files` | Find in files |
| `Ctrl+Shift+R` * | `find.replace_in_files` | Replace in files |
| `Ctrl+Alt+Down` | `find.next_result` | Find: next result (opens it) |
| `Ctrl+Alt+Up` | `find.prev_result` | Find: previous result (opens it) |
| `Alt+Shift+F10` | `run.pick` | Run: choose a configuration and run it |
| `Shift+F10` | `run.run_selected` | Run: run the selected configuration |
| `Ctrl+F5` | `run.rerun` | Run: run the last run again |
| `Ctrl+Shift+F10` | `run.test_at_caret` | Run: run the test at the caret |
| `Ctrl+Alt+R` | `run.http_request_at_caret` | Run: send the HTTP request at the caret of a .http file with curl |
| `Ctrl+F2` | `run.stop` | Run: stop the running task |
| `F4` | `run.next_problem` | Run: next problem (opens it) |
| `Shift+F4` | `run.prev_problem` | Run: previous problem (opens it) |

## editor

| Keys | Action | Description |
|---|---|---|
| `Enter` | `editor.newline` | Insert newline |
| `Tab` | `editor.tab` | Insert tab or indent selected lines |
| `Shift+Tab` | `editor.unindent` | Unindent line or selection |
| `Backspace` | `editor.backspace` | Delete left |
| `Delete` | `editor.delete` | Delete right |
| `Ctrl+Backspace` | `editor.delete_word_back` | Delete word left |
| `Ctrl+Delete` | `editor.delete_word_forward` | Delete word right |
| `Left` | `editor.left` | Move left |
| `Right` | `editor.right` | Move right |
| `Up` | `editor.up` | Move up |
| `Down` | `editor.down` | Move down |
| `Ctrl+Left` | `editor.word_left` | Move word left |
| `Ctrl+Right` | `editor.word_right` | Move word right |
| `Home` | `editor.line_start` | Line start |
| `End` | `editor.line_end` | Line end |
| `PageUp` | `editor.page_up` | Page up |
| `PageDown` | `editor.page_down` | Page down |
| `Ctrl+Home` | `editor.doc_start` | Document start |
| `Ctrl+End` | `editor.doc_end` | Document end |
| `Shift+Left` | `editor.left_extend` | Select left |
| `Shift+Right` | `editor.right_extend` | Select right |
| `Shift+Up` | `editor.up_extend` | Select up |
| `Shift+Down` | `editor.down_extend` | Select down |
| `Ctrl+Shift+Left` | `editor.word_left_extend` | Select word left |
| `Ctrl+Shift+Right` | `editor.word_right_extend` | Select word right |
| `Shift+Home` | `editor.line_start_extend` | Select to line start |
| `Shift+End` | `editor.line_end_extend` | Select to line end |
| `Shift+PageUp` | `editor.page_up_extend` | Select page up |
| `Shift+PageDown` | `editor.page_down_extend` | Select page down |
| `Ctrl+Shift+Home` | `editor.doc_start_extend` | Select to document start |
| `Ctrl+Shift+End` | `editor.doc_end_extend` | Select to document end |
| `Ctrl+Z` | `editor.undo` | Undo |
| `Alt+Y` | `editor.redo` | Redo |
| `Ctrl+Shift+Z` * | `editor.redo` | Redo |
| `Ctrl+F` | `editor.find` | Find |
| `Ctrl+R` | `editor.replace` | Replace |
| `F3` | `editor.find_next` | Find next |
| `Shift+F3` * | `editor.find_prev` | Find previous |
| `Ctrl+G` | `editor.goto_line` | Go to line |
| `Ctrl+D` | `editor.duplicate_line` | Duplicate line |
| `Ctrl+Y` | `editor.delete_line` | Delete line |
| `Ctrl+A` | `editor.select_all` | Select all |
| `Ctrl+C` | `editor.copy` | Copy |
| `Ctrl+X` | `editor.cut` | Cut |
| `Esc` | `editor.escape` | Escape (close find, clear selection) |
| `Ctrl+Alt+T` | `editor.surround_with` | Surround the selection with a live template that uses $TM_SELECTED_TEXT |
| `Alt+Shift+V` | `markdown.preview` | Markdown: open or focus a read-only preview of the active Markdown file |
| `Ctrl+/` | `editor.toggle_comment` | Comment with line comment |
| `Ctrl+?` * | `editor.toggle_block_comment` | Comment with block comment |
| `Alt+Shift+Up` | `editor.move_line_up` | Move line up |
| `Alt+Shift+Down` | `editor.move_line_down` | Move line down |
| `Ctrl+Shift+J` * | `editor.join_lines` | Join lines |
| `Ctrl+Shift+U` * | `editor.toggle_case` | Toggle case |
| `Ctrl+Shift+M` * | `editor.goto_bracket` | Move caret to matching bracket |
| `Ctrl+Alt+W` | `editor.expand_selection` | Extend selection to the enclosing syntax node |
| `Ctrl+Alt+Shift+W` * | `editor.shrink_selection` | Shrink selection back one step |
| `Alt+J` | `editor.add_next_occurrence` | Add selection for next occurrence (a caret there) |
| `Alt+Shift+J` | `editor.unselect_occurrence` | Unselect the last added occurrence |
| `Ctrl+Alt+J` | `editor.select_all_occurrences` | Select all occurrences (a caret at each) |
| `Ctrl+Alt+Shift+J` * | `editor.select_all_occurrences` | Select all occurrences (a caret at each) |
| `Alt+Shift+Insert` | `editor.toggle_column_mode` | Toggle column selection mode |
| `Insert` | `editor.toggle_overwrite` | Toggle insert/overwrite mode |
| `Alt+,` | `editor.fold_collapse` | Collapse the code region at the caret |
| `Ctrl+-` * | `editor.fold_collapse` | Collapse the code region at the caret |
| `Alt+.` | `editor.fold_expand` | Expand the folded region at the caret |
| `Ctrl++` * | `editor.fold_expand` | Expand the folded region at the caret |
| `Ctrl+=` * | `editor.fold_expand` | Expand the folded region at the caret |
| `Alt+{` | `editor.fold_collapse_recursive` | Collapse the region at the caret and every region in it |
| `Ctrl+Alt+-` * | `editor.fold_collapse_recursive` | Collapse the region at the caret and every region in it |
| `Alt+}` | `editor.fold_expand_recursive` | Expand the region at the caret and every region in it |
| `Ctrl+Alt+=` * | `editor.fold_expand_recursive` | Expand the region at the caret and every region in it |
| `Alt+<` | `editor.fold_collapse_all` | Collapse all code regions |
| `Ctrl+_` * | `editor.fold_collapse_all` | Collapse all code regions |
| `Alt+>` | `editor.fold_expand_all` | Expand all folded regions |
| `Alt+Shift+F` | `editor.fold_selection` | Fold the selection (or expand the fold at the caret) |
| `Ctrl+.` * | `editor.fold_selection` | Fold the selection (or expand the fold at the caret) |
| `Ctrl+Up` | `editor.scroll_up` | Scroll up |
| `Ctrl+Down` | `editor.scroll_down` | Scroll down |
| `Ctrl+Alt+Left` | `editor.nav_back` | Navigate back to the previous caret position |
| `Ctrl+Alt+Right` | `editor.nav_forward` | Navigate forward |
| `F11` | `editor.toggle_bookmark` | Toggle bookmark on the caret line |
| `Shift+F11` | `editor.show_bookmarks` | Show bookmarks |
| `Ctrl+F11` | `editor.toggle_numbered_bookmark` | Toggle numbered bookmark (a digit) on the caret line |
| `Ctrl+Alt+Z` | `editor.revert_change` | Revert the change at the caret to HEAD |
| `Alt+N` | `editor.diff_next_file` | Diff tab: show the next file |
| `Alt+P` | `editor.diff_prev_file` | Diff tab: show the previous file |
| `Ctrl+Alt+Shift+Down` | `editor.next_change` | Go to the next change |
| `F7` | `editor.next_change` | Go to the next change |
| `Ctrl+Alt+Shift+Up` | `editor.prev_change` | Go to the previous change |
| `Shift+F7` | `editor.prev_change` | Go to the previous change |
| `Ctrl+Alt+L` | `editor.format` | Reformat code or the selection ([format] command, else the language server) |
| `F2` | `lsp.next_problem` | Next problem (diagnostic) in the file |
| `Shift+F2` | `lsp.prev_problem` | Previous problem (diagnostic) in the file |
| `Ctrl+B` | `lsp.goto_definition` | Go to declaration (on a declaration: its usages) |
| `Alt+Shift+B` | `lsp.goto_type_definition` | Go to the type declaration of the symbol at the caret |
| `Ctrl+Shift+B` * | `lsp.goto_type_definition` | Go to the type declaration of the symbol at the caret |
| `Ctrl+Alt+B` | `lsp.goto_implementation` | Go to the implementations of the symbol at the caret |
| `Alt+F7` | `lsp.find_usages` | Find usages of the symbol at the caret |
| `Ctrl+P` | `lsp.parameter_info` | Parameter info: the signature of the call at the caret |
| `Alt+Q` | `lsp.quick_doc` | Quick documentation for the symbol at the caret |
| `Ctrl+Space` | `lsp.complete` | Code completion at the caret |
| `Shift+F6` | `lsp.rename` | Rename the symbol at the caret and its usages |
| `Alt+Enter` | `lsp.context_actions` | Show context actions (quick fixes, refactorings) at the caret |
| `Ctrl+F12` | `lsp.file_structure` | File structure: the file's symbols from its language server |
| `Alt+Shift+T` | `lsp.refactor_this` | Refactor This: the server's refactorings at the caret or selection |
| `Ctrl+Alt+Shift+T` * | `lsp.refactor_this` | Refactor This: the server's refactorings at the caret or selection |
| `Ctrl+Alt+O` | `lsp.optimize_imports` | Optimize imports (the server's organize imports action) |
| `Alt+Shift+I` | `lsp.quick_definition` | Quick definition: the definition of the symbol at the caret in a popup |
| `Ctrl+Shift+I` * | `lsp.quick_definition` | Quick definition: the definition of the symbol at the caret in a popup |
| `Ctrl+Alt+H` | `lsp.call_hierarchy` | Call hierarchy: the callers and callees of the function at the caret |
| `Ctrl+Alt+Shift+H` * | `lsp.type_hierarchy` | Type hierarchy: the supertypes and subtypes of the type at the caret |

## editor.merge

| Keys | Action | Description |
|---|---|---|
| `F7` | `merge.next_conflict` | Merge: next unresolved change |
| `Shift+F7` | `merge.prev_conflict` | Merge: previous unresolved change |
| `Alt+Shift+Left` | `merge.accept_left` | Merge: accept yours for the change at the caret |
| `Alt+Shift+Right` | `merge.accept_right` | Merge: accept theirs for the change at the caret |
| `Alt+Shift+X` | `merge.ignore` | Merge: ignore the change at the caret |
| `Alt+Shift+A` | `merge.apply_non_conflicting` | Merge: apply all non-conflicting changes |
| `Alt+Enter` | `merge.apply` | Merge: save the result, stage it and close the resolver |
| `Ctrl+Enter` * | `merge.apply` | Merge: save the result, stage it and close the resolver |
| `Esc` | `merge.cancel` | Merge: close the resolver without saving |

## editor.snippet

| Keys | Action | Description |
|---|---|---|
| `Tab` | `editor.next_tabstop` | Snippet: select the next tabstop (the last one ends the snippet) |
| `Shift+Tab` | `editor.prev_tabstop` | Snippet: select the previous tabstop |

## editor.snippet_prefix

| Keys | Action | Description |
|---|---|---|
| `Tab` | `editor.expand_snippet` | Expand the live template (snippet) whose prefix is the word before the caret |

## tree

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `Up` | `tree.up` | Tree: up |
| `Down` | `tree.down` | Tree: down |
| `Right` | `tree.expand` | Tree: expand |
| `Left` | `tree.collapse` | Tree: collapse |
| `Space` | `tree.toggle` | Tree: toggle |
| `Enter` | `tree.open` | Tree: open |
| `Backspace` | `tree.parent` | Tree: go to parent |
| `F5` | `tree.refresh` | Tree: refresh |
| `PageUp` | `tree.page_up` | Tree: page up |
| `PageDown` | `tree.page_down` | Tree: page down |
| `Home` | `tree.home` | Tree: first |
| `End` | `tree.end` | Tree: last |
| `Alt+Insert` | `tree.new` | Tree: new file or directory |
| `Shift+F6` | `tree.rename` | Tree: rename |
| `Delete` | `tree.delete` | Tree: delete |
| `Ctrl+C` | `tree.copy_path` | Tree: copy absolute path |
| `Ctrl+Shift+C` * | `tree.copy_path` | Tree: copy absolute path |
| `Ctrl+Alt+Shift+C` * | `tree.copy_rel_path` | Tree: copy path relative to the project |
| `Shift+F10` | `tree.context_menu` | Tree: context menu |

## git

| Keys | Action | Description |
|---|---|---|
| `Alt+Right` | `tool_window.next_tab` | Next tab of the tool window |
| `Alt+Left` | `tool_window.prev_tab` | Previous tab of the tool window |
| `F5` | `git.refresh` | Git: refresh |
| `Space` | `git.toggle_stage` | Git: stage/unstage selected (in the diff: the current change) |
| `Ctrl+Alt+A` | `git.stage_all` | Git: stage all |
| `Ctrl+Alt+Z` | `git.discard` | Git: discard changes of the selected file or section (in the diff: the current change) |
| `F7` | `git.next_hunk` | Git: next change in the diff |
| `Shift+F7` | `git.prev_hunk` | Git: previous change in the diff |
| `Alt+U` | `git.toggle_diff_layout` | Git: diff side-by-side or unified |
| `Alt+F` | `git.toggle_diff_collapse` | Git: fold or show unchanged lines in the diff |
| `Ctrl+F` | `git.log_filter` | Git: filter the log (branch, text, author, path) |
| `Shift+F10` | `git.context_menu` | Git: context menu of the selected commit, branch or commit file |
| `Ctrl+C` | `git.copy_sha` | Git: copy the selected commit's revision number |
| `Ctrl+D` | `git.show_diff` | Git: show the diff in an editor tab: the selected file, or in the Log list the selected commits |
| `Alt+Shift+C` | `git.log_checkout` | Git: check out the selected commit (detached) |
| `Alt+Shift+B` | `git.log_new_branch` | Git: new branch at the selected commit |
| `Alt+Shift+P` | `git.log_cherry_pick` | Git: cherry-pick the selected commit |
| `Alt+Shift+R` | `git.log_revert` | Git: revert the selected commit |
| `Alt+Shift+I` | `git.log_interactive_rebase` | Git: interactively rebase from the selected commit (pick, reword, squash, fixup, drop, reorder) |
| `Alt+Shift+T` | `git.log_new_tag` | Git: tag the selected commit |
| `Alt+Shift+D` | `git.log_delete_tag` | Git: delete a tag of the selected commit |
| `Alt+Enter` | `git.commit` | Git: commit |
| `Ctrl+Enter` * | `git.commit` | Git: commit |
| `Alt+M` | `git.toggle_amend` | Git: amend the last commit (toggle) |
| `Alt+Up` | `git.line_up` | Git: in the Commit tab diff, the line cursor to the changed line above (elsewhere: the previous commit message) |
| `Alt+Down` | `git.line_down` | Git: in the Commit tab diff, the line cursor to the changed line below (elsewhere: the next commit message) |
| `Alt+Shift+Up` | `git.line_extend_up` | Git: in the Commit tab diff, select the changed line above too |
| `Alt+Shift+Down` | `git.line_extend_down` | Git: in the Commit tab diff, select the changed line below too |
| `Ctrl+Alt+N` | `git.new_branch` | Git: new branch |
| `Alt+C` | `git.tab_commit` | Git: Commit tab |
| `Alt+L` | `git.tab_log` | Git: Log tab |
| `Alt+O` | `git.tab_console` | Git: Console tab |
| `Alt+T` | `git.tab_stash` | Git: Stash tab |
| `Ctrl+Alt+S` | `git.stash_push` | Git: stash local changes |
| `Alt+P` | `git.stash_pop` | Git: pop the selected stash |
| `Alt+A` | `git.stash_apply` | Git: apply the selected stash |
| `Delete` | `git.stash_drop` | Git: drop the selected stash |
| `Up` | `git.up` | Git: up |
| `Down` | `git.down` | Git: down |
| `Shift+Up` | `git.extend_selection_up` | Git: in the Log, select the commit above too (elsewhere: up) |
| `Shift+Down` | `git.extend_selection_down` | Git: in the Log, select the commit below too (elsewhere: down) |
| `PageUp` | `git.page_up` | Git: page up |
| `PageDown` | `git.page_down` | Git: page down |
| `Left` | `git.left` | Git: left (in a commit's files: close the folder, else back to the list) |
| `Right` | `git.right` | Git: right (in a commit's files: open the folder or the file's diff) |
| `Enter` | `git.open_file` | Git: open selected file |
| `Tab` | `git.next_section` | Git: next pane |
| `Esc` | `git.escape` | Git: back (close diff, leave message, return to editor) |

## worktrees

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `F5` | `worktree.refresh` | Worktrees: refresh |
| `N` | `worktree.new` | Worktrees: new |
| `Delete` | `worktree.remove` | Worktrees: remove |
| `Enter` | `worktree.switch` | Worktrees: switch |
| `P` | `worktree.prune` | Worktrees: prune |
| `Up` | `worktree.up` | Worktrees: up |
| `Down` | `worktree.down` | Worktrees: down |

## palette

| Keys | Action | Description |
|---|---|---|
| `Ctrl+P` | `palette.up` | Palette: up |
| `Up` | `palette.up` | Palette: up |
| `Ctrl+N` | `palette.down` | Palette: down |
| `Down` | `palette.down` | Palette: down |
| `PageUp` | `palette.page_up` | Palette: page up |
| `PageDown` | `palette.page_down` | Palette: page down |
| `Enter` | `palette.accept` | Palette: accept |
| `Esc` | `palette.cancel` | Palette: cancel |
| `Backspace` | `palette.backspace` | Palette: backspace |
| `Ctrl+Backspace` | `palette.delete_word` | Palette: delete word |
| `Ctrl+W` | `palette.delete_word` | Palette: delete word |
| `Tab` | `palette.next_tab` | Palette: next tab |
| `Shift+Tab` | `palette.prev_tab` | Palette: previous tab |
| `Shift+Delete` | `palette.delete_item` | Palette: delete the selected item (Scratch Files, Recent Projects) |

## mode.normal

| Keys | Action | Description |
|---|---|---|
| `H` | `editor.left` | Move left |
| `L` | `editor.right` | Move right |
| `K` | `editor.up` | Move up |
| `J` | `editor.down` | Move down |
| `G G` | `editor.doc_start` | Document start |
| `U` | `editor.undo` | Undo |
| `Ctrl+R` | `editor.redo` | Redo |
| `D D` | `editor.delete_line` | Delete line |
| `I` | `mode.insert` | Vim: insert mode |
| `V` | `mode.visual` | Vim: visual mode |

## mode.insert

| Keys | Action | Description |
|---|---|---|
| `Esc` | `mode.normal` | Vim: normal mode |

## event_log

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `Up` | `event_log.up` | Event Log: up |
| `Down` | `event_log.down` | Event Log: down |
| `PageUp` | `event_log.page_up` | Event Log: page up |
| `PageDown` | `event_log.page_down` | Event Log: page down |
| `Home` | `event_log.home` | Event Log: first entry |
| `End` | `event_log.end` | Event Log: last entry |
| `Enter` | `event_log.details` | Event Log: show the full text of the selected entry |
| `Ctrl+C` | `event_log.copy` | Event Log: copy the selected entry |
| `Delete` | `event_log.clear` | Event Log: clear |

## find

| Keys | Action | Description |
|---|---|---|
| `Up` | `find.up` | Find: up |
| `Down` | `find.down` | Find: down |
| `PageUp` | `find.page_up` | Find: page up |
| `PageDown` | `find.page_down` | Find: page down |
| `Enter` | `find.open` | Find: open selected result |
| `Tab` | `find.next_field` | Find: switch between query and replacement |
| `Alt+C` | `find.toggle_case` | Find: match case |
| `Alt+W` | `find.toggle_word` | Find: whole words |
| `Alt+X` | `find.toggle_regex` | Find: regex |
| `Alt+R` | `find.toggle_replace` | Find: show or hide the replacement |
| `Alt+A` | `find.replace_all` | Find: replace all results |
| `Esc` | `find.escape` | Find: back to the editor |

## hierarchy

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `Up` | `hierarchy.up` | Hierarchy: up |
| `Down` | `hierarchy.down` | Hierarchy: down |
| `PageUp` | `hierarchy.page_up` | Hierarchy: page up |
| `PageDown` | `hierarchy.page_down` | Hierarchy: page down |
| `Home` | `hierarchy.home` | Hierarchy: first row |
| `End` | `hierarchy.end` | Hierarchy: last row |
| `Right` | `hierarchy.expand` | Hierarchy: expand the selected row (loads its children) |
| `Left` | `hierarchy.collapse` | Hierarchy: collapse the selected row, or go to its parent |
| `Enter` | `hierarchy.open` | Hierarchy: open the selected function or type |
| `Tab` | `hierarchy.toggle_direction` | Hierarchy: switch callers and callees, or supertypes and subtypes |
| `F5` | `hierarchy.refresh` | Hierarchy: ask the server again |

## local_history

| Keys | Action | Description |
|---|---|---|
| `Up` | `local_history.up` | Local History: up |
| `Down` | `local_history.down` | Local History: down |
| `PageUp` | `local_history.page_up` | Local History: page up |
| `PageDown` | `local_history.page_down` | Local History: page down |
| `Home` | `local_history.home` | Local History: first row (in the diff: top) |
| `End` | `local_history.end` | Local History: last row (in the diff: bottom) |
| `Left` | `local_history.left` | Local History: back to the snapshot list |
| `Right` | `local_history.right` | Local History: to the diff |
| `Tab` | `local_history.next_section` | Local History: switch between the list and the diff |
| `Enter` | `local_history.open_file` | Local History: open the file in the editor (in the project view: the selected file's history, or a label's files) |
| `Backspace` | `local_history.files` | Local History: the project's files with history, deleted ones included (from a file's history: back) |
| `F7` | `local_history.next_change` | Local History: next change in the diff |
| `Shift+F7` | `local_history.prev_change` | Local History: previous change in the diff |
| `Alt+U` | `local_history.toggle_diff_layout` | Local History: diff side-by-side or unified |
| `Alt+F` | `local_history.toggle_diff_collapse` | Local History: fold or show unchanged lines in the diff |
| `F5` | `local_history.refresh` | Local History: list the snapshots (or the files) again |
| `Alt+R` | `local_history.revert` | Local History: revert the file to the selected snapshot (undoable) |
| `Ctrl+Alt+Z` | `local_history.revert_changes` | Local History: revert the selected changes of the diff (undoable) |
| `Shift+Up` | `local_history.extend_selection_up` | Local History: in the diff, select the change above too (in the list: up) |
| `Shift+Down` | `local_history.extend_selection_down` | Local History: in the diff, select the change below too (in the list: down) |
| `Ctrl+A` | `local_history.select_all_changes` | Local History: select every change of the diff |
| `Alt+Up` | `local_history.line_up` | Local History: in the diff, the line cursor to the changed line above (in the list: up) |
| `Alt+Down` | `local_history.line_down` | Local History: in the diff, the line cursor to the changed line below (in the list: down) |
| `Alt+Shift+Up` | `local_history.line_extend_up` | Local History: in the diff, select the changed line above too |
| `Alt+Shift+Down` | `local_history.line_extend_down` | Local History: in the diff, select the changed line below too |
| `Alt+L` | `local_history.put_label` | Local History: put a label on the file… |
| `Delete` | `local_history.remove_label` | Local History: remove the selected label (of every file it is on)… |
| `Shift+F10` | `local_history.context_menu` | Local History: actions for the selected snapshot |
| `Esc` | `local_history.escape` | Local History: clear the filter, back to the list, or focus the editor |

## problems

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `Up` | `problems.up` | Problems: up |
| `Down` | `problems.down` | Problems: down |
| `PageUp` | `problems.page_up` | Problems: page up |
| `PageDown` | `problems.page_down` | Problems: page down |
| `Home` | `problems.home` | Problems: first row |
| `End` | `problems.end` | Problems: last row |
| `Enter` | `problems.open` | Problems: jump to the selected problem |
| `Ctrl+C` | `problems.copy` | Problems: copy the selected message |
| `Alt+C` | `problems.toggle_current_file` | Problems: show only the current file |

## run

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `Alt+Right` | `tool_window.next_tab` | Next tab of the tool window |
| `Alt+Left` | `tool_window.prev_tab` | Previous tab of the tool window |
| `T` | `run.toggle_view` | Run: switch between output and the test tree |
| `Right` | `run.expand` | Run: expand the selected tree row |
| `Left` | `run.collapse` | Run: collapse the selected tree row |
| `Up` | `run.up` | Run: up |
| `Down` | `run.down` | Run: down |
| `PageUp` | `run.page_up` | Run: page up |
| `PageDown` | `run.page_down` | Run: page down |
| `Home` | `run.home` | Run: first line |
| `End` | `run.end` | Run: last line |
| `Enter` | `run.open` | Run: jump to the selected line's location |
| `Ctrl+Alt+Down` | `run.next_problem` | Run: next problem (opens it) |
| `Ctrl+Alt+Up` | `run.prev_problem` | Run: previous problem (opens it) |

## structure

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `Up` | `structure.up` | Structure: up |
| `Down` | `structure.down` | Structure: down |
| `PageUp` | `structure.page_up` | Structure: page up |
| `PageDown` | `structure.page_down` | Structure: page down |
| `Home` | `structure.home` | Structure: first symbol |
| `End` | `structure.end` | Structure: last symbol |
| `Enter` | `structure.open` | Structure: jump to the selected symbol |

## tasks

| Keys | Action | Description |
|---|---|---|
| `Up` | `tasks.up` | Tasks: up (in the details: scroll up) |
| `Down` | `tasks.down` | Tasks: down (in the details: scroll down) |
| `PageUp` | `tasks.page_up` | Tasks: page up |
| `PageDown` | `tasks.page_down` | Tasks: page down |
| `Home` | `tasks.home` | Tasks: first row (in the details: top) |
| `End` | `tasks.end` | Tasks: last row (in the details: bottom) |
| `Tab` | `tasks.next_section` | Tasks: switch between the list and the details |
| `Enter` | `tasks.details` | Tasks: focus the details |
| `Alt+C` | `tasks.toggle_closed` | Tasks: show or hide closed tasks |
| `Alt+T` | `tasks.filter_tag` | Tasks: filter by tag… |
| `Alt+A` | `tasks.filter_assignee` | Tasks: filter by assignee… |
| `Alt+J` | `tasks.jump_related` | Tasks: jump to a related task… |
| `Alt+M` | `tasks.change_status` | Tasks: change the selected task's status… |
| `Alt+P` | `tasks.change_priority` | Tasks: change the selected task's priority… |
| `Alt+Shift+A` | `tasks.assign` | Tasks: assign the selected task… |
| `Alt+Shift+T` | `tasks.edit_tags` | Tasks: edit the selected task's tags… |
| `Alt+N` | `tasks.comment` | Tasks: comment on the selected task… |
| `Alt+Insert` | `tasks.new_task` | Tasks: new task… |
| `F5` | `tasks.refresh` | Tasks: reload the tracker |
| `Ctrl+Alt+Y` | `tasks.sync` | Tasks: sync the tracker with its remote (yman sync) |
| `Shift+F10` | `tasks.context_menu` | Tasks: actions for the selected task |
| `Esc` | `tasks.escape` | Tasks: clear the filter, back to the list, or focus the editor |

## terminal

| Keys | Action | Description |
|---|---|---|
| `Alt+F12` | `tool_window.terminal` | Terminal tool window |
| `Alt+Shift+T` | `terminal.new_tab` | Terminal: new tab (another shell) |
| `Ctrl+Shift+T` * | `terminal.new_tab` | Terminal: new tab (another shell) |
| `Ctrl+F4` | `terminal.close_tab` | Terminal: close the active tab (ends its shell) |
| `Ctrl+Shift+Up` | `tool_window.stretch_up` | Stretch tool window up |
| `Ctrl+Shift+Down` | `tool_window.stretch_down` | Stretch tool window down |
| `Alt+Right` | `tool_window.next_tab` | Next tab of the tool window |
| `Alt+Left` | `tool_window.prev_tab` | Previous tab of the tool window |
| `Alt+Shift+M` | `tool_window.maximize` | Maximize or restore the active tool window |
| `Ctrl+"` * | `tool_window.maximize` | Maximize or restore the active tool window |
| `Ctrl+V` | `editor.paste` | Paste |

## todo

| Keys | Action | Description |
|---|---|---|
| `Esc` | `app.focus_editor` | Focus editor |
| `Up` | `todo.up` | TODO: up |
| `Down` | `todo.down` | TODO: down |
| `PageUp` | `todo.page_up` | TODO: page up |
| `PageDown` | `todo.page_down` | TODO: page down |
| `Home` | `todo.home` | TODO: first row |
| `End` | `todo.end` | TODO: last row |
| `Enter` | `todo.open` | TODO: open the selected match |
| `F5` | `todo.refresh` | TODO: scan the project again |

## Without a default binding

Reachable from Find Action (F1) or by binding them in `keymap.toml`.

| Action | Description |
|---|---|
| `app.escape` | Escape |
| `app.about` | About jetl |
| `app.open_settings` | Open settings file (config.toml) |
| `app.keymap_reference` | Keymap reference |
| `tool_window.toggle_pinned` | Pin or unpin the active tool window (an unpinned one hides when focus leaves it) |
| `tool_window.move_left` | Move the active tool window to Left Top |
| `tool_window.move_left_bottom` | Move the active tool window to Left Bottom |
| `tool_window.move_bottom` | Move the active tool window to Bottom Left |
| `tool_window.move_bottom_right` | Move the active tool window to Bottom Right |
| `tool_window.move_right` | Move the active tool window to Right Top |
| `tool_window.move_right_bottom` | Move the active tool window to Right Bottom |
| `app.toggle_toolbar` | Show or hide the toolbar |
| `palette.goto_line` | Go to line |
| `editor.scratch_files` | Scratch files |
| `editor.put_label` | Local history: put label… |
| `tool_window.tasks` | Tasks tool window (yman) |
| `tool_window.todo` | TODO tool window |
| `tool_window.hierarchy` | Hierarchy tool window (call and type hierarchy) |
| `editor.replace_one` | Replace current match |
| `editor.replace_all` | Replace all matches |
| `editor.find_case` | Find: toggle match case |
| `editor.find_word` | Find: toggle whole words |
| `editor.find_regex` | Find: toggle regex |
| `editor.find_field` | Find: switch between find and replace fields |
| `editor.select_line` | Select line |
| `editor.compare_with_clipboard` | Compare the file (or the selection) with the clipboard |
| `editor.compare_with_file` | Compare the file with another file… |
| `editor.to_upper_case` | To upper case (selection or word at the caret) |
| `editor.to_lower_case` | To lower case (selection or word at the caret) |
| `editor.sort_lines` | Sort lines (selected lines, else the whole file) |
| `editor.reverse_lines` | Reverse lines (selected lines, else the whole file) |
| `editor.clone_caret_above` | Clone caret above |
| `editor.clone_caret_below` | Clone caret below |
| `editor.toggle_soft_wrap` | Soft-wrap the active editor (on or off) |
| `editor.line_ending_lf` | Convert line endings to LF |
| `editor.line_ending_crlf` | Convert line endings to CRLF |
| `editor.set_language` | Set the file's language (highlighting) |
| `editor.toggle_blame` | Annotate with git blame (show or hide) |
| `tree.new_file` | Tree: new file |
| `tree.new_dir` | Tree: new directory |
| `tree.toggle_hidden` | Tree: show or hide hidden files |
| `tree.toggle_ignored` | Tree: show or hide ignored files |
| `git.unstage_all` | Git: unstage all |
| `git.resolve_conflicts` | Git: resolve conflicts (three-pane merge or take one side) |
| `git.continue_operation` | Git: continue the rebase, merge, cherry-pick or revert in progress |
| `git.skip_operation` | Git: skip the commit the rebase, cherry-pick or revert stopped on |
| `git.abort_operation` | Git: abort the rebase, merge, cherry-pick or revert in progress |
| `git.log_show_diff` | Git: show the selected commits' files side by side in an editor tab |
| `git.log_show_patch` | Git: show the combined diff of the selected commits as a unified patch in an editor tab |
| `git.log_reset` | Git: reset the current branch to the selected commit (soft, mixed or hard) |
| `git.commit_and_push` | Git: commit and push |
| `git.message_older` | Git: previous commit message from history |
| `git.message_newer` | Git: next commit message from history |
| `git.push_options` | Git: push with options (tags, force with lease) |
| `git.fetch` | Git: fetch |
| `git.stash_include_untracked` | Git: include untracked files in the next stash (toggle) |
| `git.focus_message` | Git: focus commit message |
| `merge.accept_all_left` | Merge: take yours for the whole file |
| `merge.accept_all_right` | Merge: take theirs for the whole file |
| `run.edit_configs` | Run: edit run configurations |
| `run.rerun_failed` | Run: rerun the failed tests |
| `lsp.restart` | Restart language servers |
| `lsp.goto_declaration` | Go to the declaration (not the definition) of the symbol at the caret |
| `lsp.toggle_inlay_hints` | Show or hide inlay hints |
| `hierarchy.export_mermaid` | Hierarchy: export the loaded tree as a Mermaid diagram (scratch file) |
| `hierarchy.export_dot` | Hierarchy: export the loaded tree as a Graphviz DOT graph (scratch file) |
