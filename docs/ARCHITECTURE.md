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
| `main.rs` | Argument parsing (`PATH[:LINE[:COL]]`: a file is rooted at its git work tree, else its directory, and handed to `App::new` as the file to open; with no argument outside a git repository the Recent Projects palette opens once the list loads), logging (`~/.local/state/jetl/jetl.log`, `RUST_LOG=jetl=debug`), `tui::init`, `App::new`, `app.arm_recovery()` (hands the crash recovery snapshot to the panic hook), `app.prune_recovery_dumps()` (removes dumps older than 30 days of other roots, in `spawn_blocking`), `app.start_recent_projects()` (records the root in the recent projects file, and every root switch's from then on), `App::run`, then on an error (a failed draw, which skips the quit prompt) `App::finish_after_error` (`App::dump_after_error`, the same dump the panic hook writes, into this process's recovery directory, then `App::shutdown`), `tui::restore`, and the hook's stderr note (`recovery::print_dump_note`). `--dump-keymap` prints `docs/KEYMAP.md`. |
| `tui.rs` | Owns terminal modes. Restore order is fixed (keyboard flags, mouse, paste, focus, alt screen, raw mode, cursor); the panic hook restores first, then writes the unsaved buffers (`recovery::dump_on_panic`), then logs and runs the default hook, and last prints where the buffers went on stderr. |
| `app/mod.rs` | `App` (all panels and services), `App::new`, the `run` loop, `shutdown` (what leaving jetl does without drawing, after a quit or a run loop error), `handle_key`, `perform`, `Ctx` and the `ctx!` macro, tests (`test_app`). |
| `app/msg.rs` | `handle_msg` for every `AppMsg`, git results (`on_git_msg`, `git_op_done`: follow-ups such as the push remote choice, forcing an unmerged branch delete, conflict hints for merge, rebase, cherry-pick, revert and stash pop/apply, naming Git > Continue Operation / Abort Operation), `git_root_changed`, file open and decode (`open_file_at` with a char column, `open_file_at_utf16` with an LSP position converted against the text once it is loaded; `Effect::OpenFileAt`/`OpenFileAtUtf16`). |
| `app/effects.rs` | `apply_effects`, `refresh_tree`, `git_request` (refuses `GitRequest::refused_mid_operation` requests while an operation is in progress). |
| `app/dialogs.rs` | `request_quit`, `complete_dialog_path` (Tab in an input dialog with `paths` (Save As, the worktree path prompt): `files::complete` on the text before the cursor, `folders_only` from the dialog, the hint set from the result), `finish_dialog` (one arm per `DialogId` answer; the conflict resolver's and crash recovery's dialogs go to `finish_merge_dialog` and `finish_recovery_dialog` first; the Save buttons of the unsaved-changes dialogs go to `save_then`), the branches popup's operations (`branch_op`). |
| `app/save_as.rs` | Save As. `Effect::SaveAs { id, then }` (`editor.save_as`, Ctrl+S or Save All on an untitled buffer) opens `ask_save_as` (`DialogId::SaveAs`, prefilled with the path relative to the root, absolute outside it, or the untitled name without " (recovered)"; the prompt completes paths on Tab, `Dialog::with_path_completion`). `resolve_save_as`: `~` expanded, joined to the root, `util::fs::normalize`d, the parent canonicalized when it exists. `finish_save_as`: its own path is a plain save; a path open in another buffer is refused (one buffer per path); an existing file asks first (`DialogId::SaveAsOverwrite`); else `save_as_write` (missing folders created, the buffer's format job aborted (`App::abort_format`), `Editor::save_as` (formatting after the write when `AfterSave::formats`: only Save As itself and Save All wait for a formatter), the tree folders reloaded, the HEAD blob requested; the language server document moves through `lsp_sync`'s path check). `AfterSave` is what waits for the save: `save_then` (a dirty dialog's Save: the buffers with a file first, `save_all`/`save_and_close_ids`) and `resume` (the next untitled buffer of `Editor::untitled_dirty` gets the next prompt; none left: quit, close, switch root). |
| `app/palette.rs` | Opening the palette, its keys, the file index (walked at startup, on root switches and when a file or symbol palette opens; the symbol index follows each load), the recent projects list, the scratch files (`scratch::list`, read on the main thread whenever Scratch Files opens: one small directory, current after a delete), accepted rows. |
| `app/mouse.rs` | Mouse routing: toasts, overlays, drags, splitters, tabs, status bar, panels. Over the editor area `Editor::hit_test` comes first (`editor_area_mouse`): a group splitter starts `Drag::GroupSplitter`, a press on a tab or body focuses its group, the wheel scrolls the group under the pointer without moving focus. |
| `app/watch.rs` | Starting the filesystem watcher (also tells `lsp_set_watch_files` whether it runs, `Err` included) and acting on its batches (`Changes::events` go to `lsp_watched` first). |
| `app/lsp_watch.rs` | File change events for language servers: `lsp_watched` sends the root watcher's `Changes::events` as `workspace/didChangeWatchedFiles` to each server whose `Registry::watches` wants them; `lsp_retarget_watch` keeps one extra watcher (`files::watch::start_outside`, `LspService::outside`, at most 64 folders) for registered folders outside the root (recursive; an ancestor of the root non-recursive); `lsp_set_watch_files` restarts the servers when the root watcher goes on or off, so they are (not) offered the capability; a failed outside watcher restarts the servers once without the capability (`LspService::outside_failed`, until the root changes) and warns once; registered folders that do not exist are skipped. |
| `app/config_reload.rs` | Hot reload: `ConfigChanged` from the config watcher; each file reloads on its own (text compared with what was last processed, so repeats are silent); `apply_config` swaps `App::config` and applies the live keys (`config::reload` classes); the theme reload restyles buffers and diff panes; restart-needed keys are named in the notification; errors keep the old value; `write_settings` writes the Settings window's edits into `config.toml` and reloads once. A watcher that cannot start (`config_watcher_started`, once at startup) is a Warn notification; reloads then happen only at the next start. |
| `app/find.rs` | Replace in files (open tabs through the editor, other files off the main thread). |
| `app/symbols.rs` | Starting symbol index runs (`request_symbols`, through `Coalesce`), applying `AppMsg::Symbols`, re-queuing watcher paths, and `sync_structure` (called by `ui::render` while the Structure window is shown). Also the Symbols palette's `workspace/symbol` requests: `workspace_symbols_sync` (after each palette key, paste and click) sends the trimmed query to every running non-scratch server that has it and cancels the previous query's requests; answers count only for the newest seq (`LspService::ws_symbols`); `palette_symbols` appends the servers' rows whose file and line the index lacks (`SymbolEntry::utf16` marks the server's UTF-16 columns). |
| `app/clipboard.rs` | Copy (OSC 52, the copy tool, jetl's register) and `editor.paste`: the focused text field (`PasteTarget`), the paste tool run off the main thread (`AppMsg::Paste`, dropped when focus moved), the register fallback with a one-time warning. Dialogs and the palette route the paste key here before their own key handling. |
| `app/format.rs` | Formatter runs (`Effect::Format`): one per buffer at a time (a newer request aborts or cancels the older one and keeps its save). A `[format]` command runs off the main thread, tagged with the generation and a seq, aborted on root switch and quit (`AppMsg::Formatted`); `Formatter::Lsp` sends `textDocument/formatting` through `lsp_send` (not the per-method `inflight`: one per buffer), or `textDocument/rangeFormatting` for `FormatRequest::range` (the span of the selections, set by `editor.format` only) when the server has it (else the whole file; the range is dropped when the request inherits a pending save), its answer (`PendingTag::Formatting`, matched by the job's request seq) made into the formatted text by `lsp::edits::edit_text`. A server that does not format means a save writes at once; one that is still starting (`Run::Starting`) holds the save until `format_server_started` (from `LspMsg::Started`), within `request_timeout_ms` counted from the save (`not started in N s`); a Save As while a save format is pending formats even a clean buffer (`save_format_pending`); one that does not answer within `request_timeout_ms` (`format_timeout`, on the tick) or goes away (`format_server_gone`) fails the job, so a save writes unformatted. Every result goes to `Editor::format_done`. `abort_format` (Save As) kills the buffer's command or cancels its server request. |
| `app/run.rs` | Run tasks: `RunState` (the running job with its process group id, the last task, the picker's tasks), `run_task` (kills a running task first, spawns, starts `run::process::drive`), stop, rerun, `RunOutput`/`RunExited` checked against the generation and the run's seq, the task list loaded off the main thread for the picker. The job is killed on quit, on root switch and when `RunState` is dropped. |
| `app/terminal.rs` | The Terminal window's shell: started by the first frame that shows the window (`render_terminal`, at that frame's size, in the root; `[terminal] shell`, else `$SHELL`, else `/bin/sh`; `COLORTERM=truecolor` only while `App::theme_truecolor`, else removed from the inherited environment; hot reload updates the flag for the next shell), `terminal_key` (the `[terminal]` keymap table, Esc Esc at the shell prompt, everything else to the shell or a restart once it ended), paste, `TerminalOutput` checked against the generation and the shell's id. Quitting, a root switch and dropping the app kill the shell's session (`TerminalPanel::reset`); the next one starts lazily in the new root. |
| `app/main_menu.rs` | The main menu bar (`app.main_menu`, F10): built from `ui::menu_bar::MAIN_MENU` with keymap shortcuts (`App::fill_shortcuts`, shared with context menus) and rows that need an active tab disabled when none is open; its keys and mouse; the chosen row runs through `App::perform`. |
| `app/lsp.rs` | Language servers in the app: `LspService` (the current root's `Client`s by server name, plus `"<name> (scratches)"` clients for scratch files when `[lsp] scratches` is on: rooted at the scratches directory, rust-analyzer started with `detachedFiles` (`scratch_files`, `scratch_init_options`; `LspService::detached` remembers them and a Rust scratch not in the list restarts that client), a failed one kept so it is not restarted; commands that could not start, reported once and not retried until `lsp.restart`; the documents open on servers; work done progress), `App::lsp_sync` (run by the loop after each batch: starts the server of a buffer's language, `didOpen` once it runs, `didChange` with the buffer's recorded edits (first shifting that file's stored diagnostics through them, `DiagnosticStore::shift`, and recording them per version in `Doc::sent`, at most `SENT_KEEP`; a whole-text resync records a gap and shifts nothing), `didClose` for closed buffers, close and reopen after a path or language change; `Doc::opened` and `Doc::kind` track what the server was told, so a `didOpen`/`didClose`/`didChange`/`didSave` registration or unregistration (`Caps::sync`) re-syncs the document at the next call (a document closed by a `didOpen` unregistration gets no `didChange` or `didSave` until it is opened again): a late `didOpen` sends the whole text, a switch from `NONE` to a change kind sends the whole text; files under the scratches directory open on a server only with `[lsp] scratches`, on their own client), `lsp_saved` (`didSave` after a sync), `lsp_msg` (generation and `ServerId` checked, then state changes, `window/showMessage` to notifications, `$/progress` for the status bar, `textDocument/publishDiagnostics` into `LspService::diagnostics` (a publish made for an older version of an open document is mapped forward through the edits sent since, `Doc::edits_since`; one that cannot be mapped is stored as sent), server requests answered: `workspace/configuration` from `settings`, capability registrations kept in the server's `Client::registry` (`client/registerCapability`; `client/unregisterCapability` with `unregisterations` or `unregistrations`; malformed params refused with InvalidParams, answered null otherwise), null for progress creation, `workspace/inlayHint/refresh` (answered null, hints refetched), `workspace/applyEdit` through `apply_workspace_edit` once the edit is made, `window/showMessageRequest` with actions as a choice dialog (`DialogId::LspMessageRequest`) answered with the pick or null to the same `ServerId` (`finish_lsp_message`; a restarted server gets nothing), MethodNotFound otherwise), `lsp_status` (the status bar segment), `sync_diag_marks` (each buffer's `diag_marks` against the store, called by `ui::render`), F2 stepping (`lsp_step_problem`), the status bar's problem counts and caret problem, `show_problems`, restart, root switch (`LspService::watch_files`, `outside`, `outside_watcher`: see `app/lsp_watch.rs`; both clear the diagnostics, as does a server going away, and its registrations). Tests install in-process servers with `tests::with_fake_servers`. |
| `app/completion.rs` | Code completion: `lsp.complete` (Ctrl+Space) and `completion_after_edit` (called by `perform` after every editor action: refilters the open popup or closes it when the caret left its word, asks again for an incomplete list, asks by itself after an `InsertChar` of a trigger character or once the word has `completion_min_chars`). Requests go through `lsp_doc_request` (`PendingTag::Completion`; `LspService::completion` keeps the newest with its `Origin`); an answer is used while `Origin::prefix` holds, not only at the exact version and caret, and filtered by the word typed since. `completion_key` routes keys while the popup is up; accepting sends `completionItem/resolve` first when the server supports it and waits at most `RESOLVE_WAIT` (the tick, or any key, click or paste, accepts without it: `completion_flush`); `completion_docs_tick` (on the tick) fetches the documentation of the selected item 150 ms after the selection settles (`completionItem/resolve` as `PendingTag::CompletionDocs`, tracked in `CompletionReqs::docs`; the answer counts only while the popup, its origin and the selected item are unchanged, and is merged into `state.items` and `CompletionPopup::resolved` so accepting skips its own resolve; without resolve support the item's own `detail`/`documentation` are shown); a commit character (the item's `commitCharacters`, else `Caps::completion_commit_chars`) typed while the popup is `armed` (Ctrl+Space, or the selection moved) accepts and flushes, then goes on to the editor; `completion_apply` makes `lsp::completion::edit_for`'s changes one transaction; a snippet accepted at one caret selects its first tabstop and starts an `editor::snippet::SnippetSession` (`Editor::set_snippet`), and `App::snippet_check` (before every key's dispatch) ends it once the editor lost focus, an overlay or the find bar is up, or the mode is not Insert. |
| `app/lsp_nav.rs` | Language server navigation about the active buffer's caret: type definition, implementation and declaration (`lsp_goto`, answered by `lsp_goto_answer`: one target jumps, several open the Locations palette); `lsp_doc_request` (sync first, check the server runs and has the capability (`App::lsp_caps`: the `initialize` capabilities plus the registrations whose document selector matches the buffer, `lsp::caps::Caps::has`), cancel the request of the same method in flight with `$/cancelRequest`, send with a `DocTag`; `LspService::inflight` keeps the newest per method), `lsp_response` (only the newest of its method, only while the buffer, its version and the caret are unchanged; cancelled and content-modified errors are silent, others a toast). Go to definition (one target: `Editor::push_nav_here`, then `open_file_at_utf16`; several: the Locations palette; one containing the caret: usages instead; no server: Go to Symbol on the word), usages (`includeDeclaration: false`; exactly one usage jumps straight to it with the toast `Only one usage`), hover (`HoverPopup`). `show_location_targets` takes line previews from open buffers and reads the other files in `spawn_blocking` (`AppMsg::Locations`, dropped unless its seq is `LspService::nav_seq`); `show_locations` opens the Locations palette. `hover_check` drops the popup when it no longer applies; `hover_key` routes keys while it is up. `lsp_check` and `lsp_send` are the steps under `lsp_tagged_request` (`lsp_send` with `newest: false` leaves tracking to the caller). |
| `app/signature.rs` | Parameter Info: `lsp.parameter_info` (Ctrl+P) asks `textDocument/signatureHelp` (triggerKind 1, `signature_explicit` true so an empty answer toasts `No parameter info` and a refusal toasts why); `signature_after_edit` (called from `perform` after every editor action, in Insert mode only) asks with triggerKind 2 and `triggerCharacter` when a typed character ends one of `Caps::signature_triggers`, and while the popup is open asks again with `isRetrigger` and `activeSignatureHelp` (triggerKind 2 for a retrigger character, else 3); several carets ask nothing and close the popup. `signature_answer` replaces the popup or, for a null or empty answer, clears it. `signature_check` closes it when an overlay is up, the editor is unfocused, or the group or buffer changed; `signature_close` also cancels the request in flight, so a late answer cannot reopen it; `signature_key` takes Esc. |
| `app/lightbulb.rs` | The lightbulb: on the tick (`lightbulb_tick`), once the key (active buffer, its version, caret line, `DiagnosticStore::version`) has held for `LIGHTBULB_DELAY`, one quickfix `textDocument/codeAction` (`only: ["quickfix"]`, `triggerKind` 2) for the caret line, only while the editor has focus, when a diagnostic overlaps it and the server's `Caps::code_action_kinds` allow quick fixes. Its own `Inflight` via `lsp_send(newest = false)`, so Alt+Enter and it never cancel each other; a key change cancels it. The answer (`PendingTag::Lightbulb`, routed in `lsp_response` before the `inflight` check) counts only for that seq and buffer version; an enabled `quickfix`/`quickfix.*` action sets `Buffer::lightbulb = (version, line)`, drawn by `editor/view.rs` in the diagnostic column of the focused view's caret line while both still hold. |
| `app/inlay.rs` | Inlay hints: on the tick (`inlay_tick`), once the key (active buffer of the focused group, its version, `Editor::visible_lines`) has held for `INLAY_DELAY` (300 ms), one `textDocument/inlayHint` for the visible lines plus one screen above and below, when the server has `Caps::has(INLAY_HINT)` and `LspService::inlays_shown` is on (not focus-gated). Nothing is asked when `InlayHints::fetched` is for this version and covers the visible lines, so scrolling inside the margin sends nothing. Its own `Inflight` via `lsp_send(newest = false)`; a key change cancels it (`$/cancelRequest`). The answer (`PendingTag::InlayHints`, routed in `lsp_response` before the `inflight` check) counts only for that seq and buffer version, else it is dropped; the old hints stay, shifted by the edits (`Buffer::apply_change`), until a newer answer replaces them; an error answer is only logged. `workspace/inlayHint/refresh` forgets `fetched` on every buffer and restarts the debounce. `set_inlays_shown` (`[lsp] inlay_hints`, Live) clears all hints and cancels the request when off; `toggle_inlay_hints` (action `lsp.toggle_inlay_hints`, View > Inlay Hints, no key) flips it for the session and toasts `Inlay hints: on` or `off`; a config reload calls `set_inlays_shown` only when `lsp.inlay_hints` itself changed (`ConfigDiff::has`), so an unrelated reload keeps a toggled-off state. `untrack` clears a buffer's hints when its server goes. |
| `app/lsp_edit.rs` | Rename (`lsp.rename`, Shift+F6: `prepareRename` when the server has it, its range or placeholder, else the word at the caret, into `DialogId::LspRename`, then `textDocument/rename`), context actions (`lsp.context_actions`, Alt+Enter: `textDocument/codeAction` for the selection with the diagnostics overlapping it, `Diagnostic::to_lsp`; the items wait in `LspService::edit` while `DialogId::LspCodeAction` is up; a chosen action's edit is made, then its command runs through `workspace/executeCommand` if the server lists it; an action with neither is resolved first, `codeAction/resolve`; preferred actions listed first, `preferred_first`), File Structure (`lsp.file_structure`, Ctrl+F12: `textDocument/documentSymbol` into the Locations palette, nested by indentation, `lsp::symbols`). `apply_workspace_edit` makes every workspace edit; it asks `edits::confirmation` first (every server edit is read with `edits::read_workspace_edit` / `prune_annotations`, which drop the `changeAnnotations` that no change refers to, as rust-analyzer lists an unused one in each edit) and parks an edit with a referenced `needsConfirmation` annotation (`EditReqs::confirm`) behind `DialogId::LspConfirmEdit`: Apply makes it as below (then a code action's command), Cancel refuses it as "declined" (an `EditOrigin::FileOp` one, from `will*Files`, continues the pending tree operation when applied or refused), answers and `workspace/applyEdit` alike: planned first (`lsp::edits::plan_workspace`, the steps simulated in order against every open buffer and `RealDisk`, a few `lstat`s on the main thread: version mismatch, overlapping edits, a file operation whose options, path (outside the root, through a symlink, `.git`) or open tab (dirty, or edited and then deleted/replaced) do not allow it refuse it whole); clean tabs of deleted or replaced files closed; one transaction per open buffer (left unsaved); the disk steps (closed-file edits, `FsOp`s, a delete or overwrite first waiting for its local history `Ack`) in order in one `spawn_blocking`, stopping at the first failure (`abort`) (`AppMsg::EditsApplied` with the ops made and the failed index; a previous root's only carries renames into local history); `edits_applied` follows the ops (rename: `history_renamed` then `Editor::retarget`, the document reopened by `lsp_sync`; delete: `history_files_changed`, `close_under`), reports (rename: occurrences and files; the file operations), answers `workspace/applyEdit` (`applied`, `failureReason`, `failedChange`) and refreshes tree and git. |
| `app/merge.rs` | Conflict resolution: Git > Resolve Conflicts (`resolve_conflicts`: a choice of the conflicted paths of the last status, then Merge…, Accept Yours, Accept Theirs; `DialogId::ResolveConflict`), `conflict_loaded` (`GitMsg::Conflict`: `MergeInput::classify`, then `Editor::open_merge`, or the dialog without Merge… when no line merge is possible), `offer_conflicts` (after an operation stopped on conflicts, `pending_conflicts` makes the next status with conflicted files open the choice), `offer_next_conflict` (`resolved_conflict`, set by a finished `GitOp::Resolve`, which the resolver's Apply now runs too through `GitRequest::StageResolved`: the first status no longer listing that file reopens the choice with the files left, or notifies "All conflicts resolved"), the resolver's dialogs (`MergeApply`, `MergeCancel`, `MergeQuit`) and `quit_merge` (quitting closes the resolver, asking when the result has the user's work). |
| `app/git_ops.rs` | Git operations in progress: `progress_request` (Continue / Skip / Abort through `InProgressKind::args`; Continue with `U` files asks Resolve… / Stage and Continue; Abort confirms), `git_action_available` (main menu and palette), `finish_git_ops_dialog`. Reset (`DialogId::ResetMode`, then a `ResetHard` confirm; only `GitOp::Reset` hard touches the work tree, so only it is snapshotted). `push_dialog` (`DialogId::PushOptions`; the toggle buttons reopen the dialog with the flag flipped and send `GitRequest::PushWith` to `run_cli_steps`). |
| `app/recovery.rs` | Crash recovery in the app. `recovery_snapshot` (the run loop, after each batch): stores a new `RecoverySnapshot` of the dirty buffers that are not the resolver's scratch buffers only when `Recorder::is_current` says a buffer's id, version, saved state or path, or the generation, changed. `offer_recovery` (the end of `session_loaded`; skipped while a dialog or an offer is up or the resolver is open): `recovery::scan` of the root, then `DialogId::Recover` (Restore all / Discard / Review…, at most 8 entries listed); `finish_recovery_dialog`: Restore all restores every entry, Discard removes the dumps, Review… asks per entry (`DialogId::RecoverEntry`, Restore / Skip), Esc keeps the dumps for the next session load. `restore_recovered`: over the file's buffer when it is open and clean, else over the file read from disk (`decode_file`; a missing file keeps its path), else (open with its own unsaved edits, binary, unreadable) an untitled `<name> (recovered)` copy (`Editor::open_untitled`); always `Editor::recover_text`, one `Buffer::apply` with no `mark_saved`. The dumps go once answered. |
| `app/files.rs` | Tree file operations: `run_fs_op` first sends `workspace/willCreateFiles|willRenameFiles|willDeleteFiles` to the running, non-scratch servers whose `Caps::file_operation` filters match (sorted by name; a `PendingFileOp` per operation in `LspService::file_ops`, keyed by seq, filled by `will_file_op_answer`; an error, a timeout or a server that went away just stops being waited for), applies their edits one after another through `apply_workspace_edit` (`EditOrigin::FileOp` continues the chain via `file_op_continue`, also when an edit is refused), then `run_fs_op_now` runs the `FsOp` off the main thread with no interested server at once (a delete first waits for its local history `Ack`); a restart runs waiting operations, a root change drops them; `fs_op_done` (`FsOpDone::is_dir`, read before the operation, sends `did*Files` to the servers whose filters match; reload the tree, open, retarget or close tabs) (reload the tree, open, retarget or close tabs). A successful rename first queues `history::Handle::rename` (before `Editor::retarget`, so snapshots of the old path queued until then are carried; also for a previous root's `FsOpDone`, since the store is global), then retargets the tabs of the file or under the directory (`Editor::retarget`: each buffer gets `Editor::path_setup` for its new path through `adopt_moved_path`, which takes the language, `.editorconfig` settings and soft wrap only where they differ from what the old path gave, so choices made by hand survive a rename within a language, and keeps `disk_mtime`; `lsp_sync` then closes and reopens the document as after Save As), and the back/forward history, `pending_nav` and closed-file bookmarks follow. |
| `app/history.rs` | Local history in the app: `editor.local_history` (Alt+Shift+H; the tree selection when the Project window has focus, else the active file) shows and focuses the Local History window on that file (`show_local_history`: `LocalHistoryPanel::set_target`, then `Handle::list`), or its project view on a folder, or on the root with no file (`show_local_history_dir`: `LocalHistoryPanel::show_project`, a scan); opening the window (`toggle_tool_window` -> `history_window_opened`) scans the project view again when it shows it, else with nothing shown takes the active file, else the root's files. `history_request` maps the window's `Effect::History` requests (`List`, `Recent`, `Diff`, `Revert`, `RevertChanges`, `PutLabel`, `RemoveLabel`; `Capture` from saves). `history_msg`: `List` goes to the window while it shows that path (`set_entries`); `Recent` while its project view shows that folder (`set_files`); `Stored` (a snapshot of a path landed: a save, the unsaved timer, a git operation) and `Labelled` / `LabelRemoved` (after their toast) list again while the window shows that path, and scan again while the window is visible on a project view holding it (`history_files_changed`, also called by a tree delete; a tree rename and a root switch move or reset the project view, `history_renamed`, `history_root_changed`); `Diff` goes to the window, which keeps only its newest request's. The diff (`load_local_diff`: the snapshot against the open buffer's rope or the disk, `DiffDoc::build` in `spawn_blocking`, answered as `HistoryMsg::Diff` with the window's seq; builds are coalesced by `local_diff` (`Coalesce`): one runs, the newest request waits in `local_diff_next` and starts when the running one's `Diff` lands; `local_diff_follow` records the tab `version` a diff was built from, and the tick's `history_diff_follow` asks again, at most every 0.5 s, while the window is visible, shows that path and the tab has changed, so the right side follows typing; same key, so scroll and block stay). Revert (`HistoryMsg::Blob`, applied only while the window shows that path with that snapshot selected, in either view, `LocalHistoryPanel::shows`: `Editor::restore_text` into the open tab, else `pending_revert` until `file_loaded`, as `pending_goto`; a missing file gets an empty tab with its path, its missing folders made again (`recreate_parent`) so the save can write it; the window then compares again). Revert of a whole label (the project view's label row: `Request::RevertLabel` -> a confirm, `DialogId::RevertLabel`; Revert queues `Handle::blobs` -> `Job::Blobs` -> `HistoryMsg::Blobs`, applied only while the view still lists that `(at, name)` label (`shows_label`): `revert_label` runs `revert_to_with(.., focus: false)` per member (open tab: `apply_revert_with` quiet; closed file: `PendingRevert::Member` and an open without focus; missing file: parent made again and an empty tab), one `enforce_tab_limit` at the end (it skips dirty tabs), one toast, the focus stays in the window; `pending_revert` is a map by path so several can wait; a root switch drops the queued ones and names each label member in a warning, `pending_reverts_dropped`). Revert of changes (`revert_changes`: a `ChangesRevert`, the diff's selected changes as `gutter::revert_spans` edits plus `base`, the text the diff compared; no worker trip, the doc holds the snapshot's lines; `apply_changes_revert` -> `Editor::restore_changes`, one `Buffer::apply`, only while the tab's text is still `base`, else a toast and a new comparison; the tab is shown, the focus stays in the window, the diff is compared again; a file not open waits in `pending_revert` (`PendingRevert::Changes`) and is opened without focus, its loaded text checked the same way). `history_renamed` (a Project tree rename, also a previous root's `FsOpDone`): `Handle::rename` first, then `LocalHistoryPanel::retarget` (the file or a folder above it) and a new list, queued after the carry. `capture_disk` (a file on disk before Overwrite or Save As replaces it, read on the main thread within `max_file_kb`), `unsaved_snapshots` (from the tick: when `unsaved_due` has passed, re-arms it by `snapshot_unsaved_secs` and, unless local history is off, queues `Editor::unsaved_captures` as `Kind::Unsaved`; while off nothing is marked seen, so the edits are taken once it is back on; `apply_config` re-arms the timer when the key changes), `snapshot_discarded` (every "Don't Save" answer: quit, close tab, close all, close tabs, switch root; before anything closes it queues `Editor::discard_captures` as `Kind::BeforeGit` with `DISCARD_DETAIL`, labelled "Before discarding unsaved edits"; independent of the unsaved timer, needs local history on; an `Unsaved` last entry of the same text is relabelled by the usual promote rule; quit relies on the exit flush), Put Label (`editor.put_label` with the same target, `history_target`, or the window's Put Label (Alt+L) on its file, `ask_put_label`; refused while local history is off; an input dialog, `DialogId::PutLabel`; the name trimmed, cut to `MAX_LABEL_CHARS` (100), an empty one refused; the open tab's rope (unsaved edits included), else the file read on the worker; on a folder or the root, `Handle::put_label_dir` -> `Job::LabelDir`: the open tabs under it as rope clones, plus the git-changed files not deleted and the present files with history under it, from disk, capped at `MAX_FILES`, one `at` and name; `Labelled.result` is the count) and label removal (the window's Remove Label (Delete) on a label row, `ask_remove_label`: a confirm, `DialogId::RemoveLabel` with every entry of the label (one file's, or a project view label row's on several files); Remove queues `Handle::remove_label`). |
| `app/snapshot_tests.rs` | Render snapshot tests (`cfg(test)`): `stable_app` (a `test_app` with nothing that depends on the terminal or the clock), `draw_app` (the whole frame) and `draw_editor` (the editor area only), which first drop the startup requests' results unhandled; the frames are compared with `insta` against `app/snapshots/*.snap` (see `docs/TESTING.md`). |
| `workspace.rs` | `Workspace` (root and generation), root switching (`App::switch_root`), sessions (JSON per root: tabs in order with their pins, bookmarks (`SessionFile::bookmarks`, the numbered ones in `bookmark_digits`; numbered bookmarks of closed files in `closed_bookmarks`, their plain ones per file in `closed_plain_bookmarks`, a separate field so older builds still read the session), the split editor's groups (`splits`, only with more than one group: the tree with each group's views as file indices with their own caret, scroll and pin), tool window layout and sizes, Git tab, tree hidden/ignored toggles, expanded tree folders; a missing one arrives empty; fields added later are `#[serde(default)]`). `session_loaded` skips tabs already open, lays them out in the saved groups (`Editor::restore_splits`: files that did not open drop out, empty groups collapse), applies the layout (sizes clamped, toggles winning over `[files]` config), expands the saved folders on the main thread, then opens the command line file and ends with `offer_recovery` (so a root switch offers that root's crash dumps too). |
| `run/` | Run tasks. `mod.rs`: `RunTask` (`[[run.tasks]]`, `.jetl/run.toml`; a command is a `sh -c` string or a list of words) and the defaults detected from Cargo, npm, make and Go project files. `process.rs`: `spawn` (own session and process group, stdout and stderr on one pipe, stdin closed), `kill_group` (SIGKILL to the group), `drive` (a reader thread splits, cleans and matches lines; batches go out every 50 ms, then the exit). `matcher.rs`: problem matchers (rustc/cargo with the header's severity, tsc, pytest and tracebacks, generic `path:line:col`) turning lines into `RunLine`s with a severity and a link to an existing file. `panel.rs`: the Run tool window (status line, last 10 000 lines, follow unless scrolled up, links opened through `Effect::OpenFileAt`, next/previous problem). |
| `lsp/` | Language server protocol. `mod.rs`: `LspMsg`, `PendingTag` (what a request was for, carried to its `Response`: `Definition`, `References`, `Hover`, `SignatureHelp`, `Completion`, `CompletionResolve`, `CompletionDocs`, `PrepareRename`, `Rename`, `CodeAction`, `CodeActionResolve`, `ExecuteCommand`, `DocumentSymbols`, `TypeDefinition`, `Implementation`, `Declaration`, `Formatting` carry a `DocTag` with the buffer, its version, the UTF-16 position and the seq; `WillFileOp(seq)` is `workspace/will*Files` of a tree file operation; `WorkspaceSymbols(seq)` is a `workspace/symbol` for the Symbols palette, with no document), `EditOrigin` (what a workspace edit is for, carried to `AppMsg::EditsApplied`; `FileOp` is a server's edit before a tree file operation), `LspRequest` (`Effect::Lsp`), `[lsp]` config with the built-in servers. `transport.rs`: JSON-RPC framing (`Content-Length`) over any `AsyncRead`/`AsyncWrite`, `Incoming`/`Outgoing`, reader and writer tasks. `client.rs`: `spawn_process` (own session, stderr to the log), `Client` (state, `notify`, `request` with a tag, `respond`, `shutdown`: `shutdown` and `exit`, two seconds, then the process group is killed; dropping it kills the group; `registry`: the server's dynamic registrations, gone with the `Client`; `watch_files`: whether `client_capabilities(watch_files)` offered `didChangeWatchedFiles`) and the driver task that owns the connection (sends `initialize` and holds everything else until it is answered, keeps the pending requests and fails those older than `request_timeout_ms`, reports a failed start, a startup timeout or an exit). `registry.rs`: `Registry` (`client/registerCapability` and `unregisterCapability` params parsed from JSON, all or nothing; replaced by id), document selectors (language id, `file` scheme, LSP globs through `globset`, relative patterns), `workspace/didChangeWatchedFiles` watchers on `Registration` (glob and kind bits; `watches(path, FileChange)`, `watch_bases`) and `Glob`. `caps.rs`: the method names and `Caps` (static capabilities and applicable registrations combined: `has`, `dynamic` options, union of trigger characters and commands; `file_operation`: `workspace.fileOperations` filters and registrations for a path). `pos.rs`: char index to UTF-16 `Position` and back, clamped; the rope's lines are LSP's (see Pitfalls). `diagnostics.rs`: `Diagnostic` (with `Tags`, unnecessary and deprecated, and `Related` locations from `relatedInformation`; `same_problem` ignores ranges), `shift_pos`/`shift_all` (positions and diagnostics through `DocEdit`s in LSP coordinates: text typed at a range's boundary is not absorbed, a non-empty range emptied by an edit is dropped, point diagnostics move), `DiagnosticStore` (by path; an open document's ranges are in terms of the text last sent to its server, `shift` moves them; a publish replaces the file's list, capped per file by `max_diagnostics_per_file` and in total by `MAX_TOTAL`, the most severe kept, sorted by position; `version` bumped by every change; `get`, `files`, `counts`, `clear_server`, `clear`) and `DiagMarks` (a buffer's diagnostics in chars: marks, the worst severity per line for the gutter, non-overlapping underline spans, merged unnecessary and deprecated char ranges for `tags_at`; rebuilt when the store version, buffer version or path changed). `panel.rs`: the Problems tool window (rows grouped by file over the store, each problem followed by its related locations, rebuilt per store version keeping the selected problem, by equality or else `same_problem`, since ranges move with edits; Enter opens a problem or related location through `Effect::OpenFileAtUtf16`; `problems.toggle_current_file`, Alt+C, lists only the editor's active file, in memory only; the project scope leaves out files under `ProblemsPanel::exclude`, the scratches directory, and its title counts only the files it lists). `sync.rs`: `DocEdit` (one recorded edit in LSP terms), `SyncCaps` (`open`, `close`, `change`, `save`; `Caps::sync` adds the applicable registrations to the static ones), language ids, file URIs. `completion.rs`: `Origin` (where a request was made: the word start, the caret as char index and position, the length of the text after the caret; `prefix` is the word typed since, `None` once the caret left it), `CompletionState` (items, `isIncomplete`, the nucleo-filtered order: items starting with the prefix first, then score, `sortText`), `parse_snippet` (the flat text and its `Tabstop`s: placeholders, choices as their first option, nested ones with their parent), `edit_for` (the item's `textEdit`, else `insertText` or label over the typed word, its positions mapped across the typing since the request, plus `additionalTextEdits`, planned by `edits::plan`; the caret at `$0` or after the text; `Accept::stops` the snippet's tabstops in the text after it, with an implicit `$0` at the end). `edits.rs`: `CharEdit` and `plan`, several edits of one text as the changes of one `Buffer::apply` transaction (sorted by position descending so each stays valid, overlaps refused, `Plan::starts` for where each edit's text lands); `map_pos` (a caret through a plan); workspace edits: `workspace_steps` (ordered `Step`s: text edits and `FileOp`s with their `documentChanges` index; `documentChanges` over `changes`), `plan_workspace` (simulated over `Disk`: `Entry` per path, overlays for created, moved and deleted paths; version check against `OpenDoc`s; returns open-buffer plans, tabs to close and ordered `DiskStep`s), `confirmation` (the annotations of an edit when one needs confirmation, for the dialog), `edit_text`/`edit_file` (a file that is not open, its line breaks kept, written atomically). `symbols.rs`: `documentSymbol` answers (the `DocumentSymbol` tree or flat `SymbolInformation`s nested by range) as rows with a depth, and a glyph per `SymbolKind` (`kind_name`: its name); `workspace_entries`: a `workspace/symbol` answer as symbol index rows. `locations.rs`: definition and references answers (`Location`, arrays, `LocationLink`s) as `Target`s, and `LocationItem`, a Locations palette row (path, line, char column, text, and `mark`, the chars of the text the usage's range covers), with `read_items` for files that are not open. `fake.rs` (tests): an in-process server over `tokio::io::duplex`. |
| `terminal/` | The Terminal tool window (Alt+F12). `pty.rs`: `Session` (portable-pty; the shell leads its own session with the terminal as its controlling one; a reader thread fills an inbox and sends one `AppMsg::TerminalOutput` wake per batch the app has not drained yet, blocking at 1 MiB; a writer thread for input; a waiter thread reports the exit after the last output; `kill` SIGKILLs every process of the session, found through `/proc` on Linux, and runs on drop). `panel.rs`: `TerminalPanel` (the vt100 parser on the UI thread with 5000 lines of scrollback by default, answers to cursor position and device attribute queries, cell rendering with colors (RGB cells mapped by `color::to_ansi256` unless `App::theme_truecolor`), attributes and wide characters, the real cursor while focused, resize to the text area every frame, wheel scrollback or arrow keys on the alternate screen, mouse reports when the program enabled them). `keys.rs`: xterm encoding of keys (application cursor mode), mouse (X10/normal/button/any motion, default, UTF-8 and SGR encodings) and paste (bracketed when enabled). |
| `recovery.rs` | Crash recovery of unsaved buffers. `RecoverySnapshot` (the root and a `SnapBuffer` per dirty buffer: id, path, name, version, saved state and a `Rope` clone, O(1)) shared with the panic hook through `Arc<Mutex<_>>`; `Recorder` (the app's side: `is_current` compares without allocating, `store` replaces the value whole, `clear` on a clean exit empties it and removes this process's directory). `arm` puts the directory and the mutex in a `OnceLock` for the hook; `dump_on_panic` copies the snapshot with a bounded `try_lock` (50 tries 2 ms apart, a poisoned lock still read, never a blocking `lock`) and `write_dump`s it: `state/recovery/<pid>-<start secs>/<id>.txt` (each through a `.tmp` and a rename, no fsync), then `manifest.json` (`Manifest` version 1: pid, root, time, `Entry` per buffer with its file, path or `untitled`, name; a path that is not UTF-8 becomes an untitled entry). `scan(recovery_dir, root)`: directories not named `<u32>-<u64>` and those of a live pid (`kill(pid, 0)`, `EPERM` counts as alive) are left alone; a dead owner's directory without a manifest or with no buffers is removed; an invalid manifest is logged and left; other roots are left for when they open; the rest is a `Found`, newest dump first, the newest entry per path only. `prune(recovery_dir, keep_root, now, MAX_AGE_SECS)` (startup, off the main thread): removes dead owners' dumps older than 30 days by the manifest's time (else the start time in the name), never `keep_root`'s, a live owner's or a directory not named `<u32>-<u64>`. |
| `scratch.rs` | Scratch files: one global directory, `Paths::scratches_dir` (`state/scratches`), made by the first scratch. `create` (lowest free `scratch_<n>.<ext>` per extension through `create_new`, safe against another jetl), `list` (newest first, dotfiles and directories left out, a missing directory is empty and not created), the language choice (`DialogId::NewScratch`: plain text, then `LANGUAGES`, each language's first extension). `impl App`: `editor.new_scratch` (Ctrl+Alt+Shift+Insert) asks, then creates the file and opens it at once (`open_text` of empty text); the Scratch Files palette's Shift+Delete (`palette.delete_item`, `Accept::DeleteScratch`) asks (`DialogId::DeleteScratch`), removes the file, closes its tab (`close_under`) and reopens the palette, which Cancel does too. Scratches are ordinary files with a path: save, autosave, crash recovery and sessions need nothing more. |
| `history/` | Local history. `mod.rs`: `[local_history]` config, `Handle` (cloned into `GitService`; a `std::sync::mpsc` queue to one named thread, `jetl-history`, which owns every store access: captures, `rename` (no `Ack`: nothing is destroyed), `before_git` and `before_delete` (each returns an `Ack` the destructive operation waits on, at most 10 s), labels (`Job::Label`, `Job::RemoveLabel`; removal runs while local history is off, as prune does), lists, blobs, prune, `flush`), `Capture` (a `String`, a rope clone or bytes; stamped when queued), `Request` (`Effect::History`), `HistoryMsg` (`AppMsg::LocalHistory`; `Stored` is sent for every snapshot `record` adds or replaces), `local_time`; `Job::Recent` scans for the project view through an `IndexCache` the worker keeps (made by the first scan). `project.rs`: the window's project view (`ProjectView`): the files under a folder with history from `store::recent`, newest change first (deleted ones crossed out, renamed-away ones saying where), every file's label of one `(at, name)` grouped into one row that Enter expands into its files, a filter, the selection kept by `RowId` across scans, `scanning` / `again` (one scan in flight, one more asked when it lands), `want` (the row to select when back from a file). `panel.rs`: the Local History tool window (`LocalHistoryPanel`), two views (`View`): the project view, kept while a file's is shown (Enter drills in, Backspace or `[All Files]` back), and one file (`Target`, kept until `editor.local_history` picks another; a tree rename moves it), its snapshots newest first (`set_entries`: the filter and the selected snapshot, by `(at, blob, kind, detail)`, survive a new list, else the same row; the selection's diff is asked again), a speed-search filter (every term a case-insensitive substring of the full date and label), the toolbar (Revert, Revert Change (the diff's selected changes: `DiffPane::selected_blocks`, or with the line cursor (`DiffPane::line` / `line_anchor`, row indices of the current layout, Local History only) the selected rows through `DiffDoc::row_lines`; refused while the diff reloads or for non-UTF-8 text), Put Label…, Remove Label, the filter), the list beside the diff (one at a time below 60 columns), and `HistoryAction`s that push `Effect::History` requests. `store.rs`: the global content-addressed store (`Paths::history_dir`): blobs by the git blob sha1 of the raw bytes, compressed (zlib, `miniz_oxide` level 6, on the worker) under `blobs-zlib/<2>/` when 64 bytes and up and smaller that way, else raw under `blobs/<2>/` (also every blob an older jetl wrote), written through a pid-named temp file and a rename; one JSONL index per key hash (`index/<fnv1a64>.jsonl`, every line names its key); the key is the canonical path (`store::key`: every symlink resolved; a missing file follows a dangling symlink or canonicalizes its nearest existing ancestor, so a deleted file keeps the key it was stored under), so a file, a symlink to it and a path through a symlinked folder share one history, while the caller keeps showing the path it asked with (`HistoryMsg::List` echoes it); `record` (skips files over `max_file_kb`, binary ones, the store's own files, and bytes equal to the path's last entry, except that a snapshot of another kind of the bytes an `Unsaved` last entry holds replaces that entry instead of being `Same`, so a save relabels it "Saved"; automatic snapshots (`Kind::automatic`: autosave and `Kind::Unsaved`, the unsaved edits of a tab) form one family: one whose path's last entry is automatic and of the same run, the run's first taken under `AUTOSAVE_RUN` (5 min) ago and kept in `Entry::since`, replaces that line in place, taking the new kind (`replace_last`: under `prune.lock`, temp file and rename, other lines kept; when another instance holds the lock it appends instead), and its old blob is left to prune's gc; any other kind ends a run; a label, `Kind::pinned`, is never `Same` and never replaces: it is always appended and ends a run), `remove_entry` (a label's line removed under `prune.lock` through `drop_lines`, every other line kept verbatim; `Removal::Locked` when another instance holds the lock), `list` (newest first, corrupt lines skipped; also reads the index of the path as given, for entries an older jetl stored under it), `recent` (the keys under a folder, component-wise, with their newest entry, count and labels; `FileState` from a stat of each key: present, deleted, or renamed away when a `Renamed` marker names it as its old path no older than its newest entry; through `IndexCache`, which re-reads an index file only when its mtime, size or inode changed and keeps one `KeyHistory` per key, not the entries), `read_blob` (tries the compressed file first; a damaged one is an `InvalidData` error, or the raw copy when there is one), `rename` (a Project tree rename: the old key's entries are appended to the new key's index with `path` rewritten and blobs shared, those the new key already has left out (and a "Renamed from" its own path, so renaming back adds no duplicates), then a `Kind::Renamed` marker whose detail is the old path and whose text is the file now (else the newest carried snapshot's); a folder rename scans every index file once for keys under the old key; appended under `prune.lock` when it is free, else without it; the old key keeps its entries, so a new file at the old path never mixes with the renamed one; index files may then be out of time order, so `entries_of` sorts by time and prune rewrites sorted; labels move instead: the old key's label lines are dropped under the lock, and stay copied when it is held), `prune` (under `prune.lock`: first `migrate_keys` moves entries stored under a non-canonical path (written by an older jetl) into their key's index, merged by time, so they are counted with the rest; then by age (labels exempt), then the oldest entries of any file while the referenced blobs' bytes on disk exceed the cap, labels last; changed indexes rewritten through a rename; unreferenced blobs deleted once 10 minutes old). |
| `recent_projects.rs` | `state/recent_projects.json`: roots with their last open time, newest first, deduplicated, at most 30. Recorded off the main thread at startup and on every root switch (`App::load_recent_projects`) once `main.rs` called `App::start_recent_projects` (tests never do, so no write lands in a removed temp dir); Shift+Delete in the palette forgets a root (`recent_projects::forget`, `App::forget_recent_project`: the row goes at once, the write runs off the main thread; replies carry the version taken under the file lock, the app shows only a newer version and drops replies to requests asked before the latest forget, so a load that read the file before the write cannot bring the row back; a failed write warns and lists the file as it is); read-modify-writes share one lock, so a forget and a recording never lose each other's change; a missing or corrupt file reads as empty; roots that are no longer directories are not listed. |
| `worktree/` | `git worktree` parsing and paths (`mod.rs`), the Worktrees tool window (`panel.rs`), the create/remove dialog flow (`flow.rs`, `impl App`; a forced remove first snapshots the worktree's changed files to local history, see `GitService::before`). |
| `actions.rs` | `Action` enums and `REGISTRY` (id, description, action). Keymaps and the palette refer to actions by id. |
| `dispatch.rs` | Keys to actions: vim mode table (when modal), then the context stack innermost first, chords with a timeout, double Shift. |
| `config/` | `Config`, `Paths` and `FileText` (`mod.rs`), key parsing and tables (`keymap.rs`, defaults in `assets/keymap.default.toml`), `Theme` (`theme.rs`: built-in themes in `assets/themes/*.toml`, `Theme::load`, `Theme::load_report` for reload, `Theme::sources`), color depth and the 256-color mapping (`color.rs`), every key's reload class and `diff` (`reload.rs`; the macro destructures each section exhaustively, so a new key must be classified), format-preserving `config.toml` edits with `toml_edit` (`edit.rs`: `Value`, `Edit`, `apply_edits`; comments and order kept, a new table goes after the file's trailing comments, an invalid result refused), the Settings window's catalog (`catalog.rs`: `PAGES`, the page tree with each page's keys, and one `Setting` per key with label, kind and help; the reload class comes from `reload::class_of`), `ConfigWatcher` (`watch.rs`: a non-recursive debounced watch of the config dir, the theme file's dir and symlink targets; re-targeted after every config reload). |
| `ui/settings.rs` | `SettingsWindow` state (pending edits per key, key/mouse handling) and rendering: page tree, rows with the `*` pending marker and class tags, value cells, Apply / OK / Cancel buttons and their hit rects; below 30 inner columns (a terminal under 36 columns) only the frame is drawn. |
| `app/settings.rs` | Opens the window, routes its input, runs the value dialogs and the discard confirmation; Apply sends the edits to `App::write_settings`. |
| `event.rs` | `AppMsg` (background to app), `Effect` (panel to app), `Mouse`, `Level`. |
| `editor/mod.rs` | `Editor`: the buffers (`buffers`, the document store; its order is not tab order), the tab groups and their split tree, the global MRU, back/forward, actions, mouse, rendering. `active()` is the buffer of the focused group's shown tab, so `active_buffer()` callers need not know about groups. `render` lays the tree out over the centre, draws the splitter bars, then each group (tab bar of the `tabs` height, none in zen mode, over its find bar and text), the focused group last. |
| `editor/groups.rs` | `TabGroup` (its `View`s in bar order, the shown slot, its own MRU, tab hit rects, text area, find bar hits, caret cell) and the tab operations on `Editor`: `set_active`/`show` (a buffer opened in the focused group gets a tab there, right after the shown one and never among the pinned, with a `fork` of its live view state when it is shown elsewhere), `close_slot` (a buffer with tabs in other groups only loses this one, without asking; its last tab asks when dirty), `force_close` (the buffer and every tab of it), `close_tabs` (close others, close all but pinned and close all act on the focused group), pins, moves (Alt+Shift+Left/Right, a tab drag: `Drag::Tab`, `Editor::drag_tab`, within the pinned or unpinned tabs; over another group's bar it moves the tab there at the slot under the pointer, over its body it sets `tab_drop` (a ghost tab in that bar) and `drop_tab` appends on release; the app keeps `Drag::Tab` on the group `drag_tab` returns; refused while the merge view is open), `enforce_tab_limit` (`editor.max_tabs` counts buffers; the least recently used clean one without a pinned tab that no group shows goes), the tab context menu, `split`, `unsplit`, `unsplit_all`, `move_to_opposite`, `cycle_group`, `collapse_empty` (a group left without tabs goes, never the last one), `make_live` and `sync_parked` (see Conventions), `hit_test` (`EditorHit`) and `restore_splits`. `check_invariants` (tests) asserts the rules below. |
| `editor/split.rs` | `SplitNode` (a leaf is a group; a split has an `Axis`, a ratio in permille and two children), `layout` (the leaves' rects and the one-cell bars, `MIN_GROUP_W`/`MIN_GROUP_H` kept while there is room for both sides), `split`, `remove` (the sibling takes the split's place), `sibling`, `set_ratio` by preorder index, `SplitterHit::ratio_at` for a bar drag. |
| `editor/view_state.rs` | `ViewState`: what one view of a buffer owns (its carets `sels` with their desired and virtual columns, scroll, follow key, expand and occurrence history, the column `block` it last made, find bar, collapsed `folds`, soft `wrap`); the top row is `scroll_line` plus `scroll_seg`, the wrap segment shown first (`top`/`set_top`); `sel()` is the primary selection, `set_sel` makes one caret; `fork` for a new view (the primary caret only, a copy of the folds, the wrap toggle with an empty cache). `EditLog`: the buffer's text changes since the last frame (`LogEdit::Edit` in chars, `Reset` for a whole-text replacement), capped at 4096. `Parked`: a view's state while another view of the buffer is live; `catch_up` maps every caret and selection, the folds, the top line and the find bar origin through the log (a reload or dropped edits clamp instead; carets that meet merge), drops the expand and occurrence history and the carets' virtual columns (they sit at the line ends) and keeps a view that followed its primary caret following it without a scroll. |
| `editor/buffer.rs` | `Buffer`: rope, the live view's state (`view: ViewState`), undo, file metadata (path, line ending, mtime, dirty state), the edit commands (see Carets below: `edit_carets` with a `CaretEdit` per caret, line commands over the union of caret lines; `reorder_lines` sorts or reverses the selected line blocks, else the whole text, in one `apply`; `change_case` maps selections or words to upper, lower or toggled case). `recovered` marks text restored after a crash: autosave skips it until a write clears it. `autosave_seen` and `history_seen` are the versions the idle autosave timer and the last snapshot of unsaved edits saw. Every change goes through `apply_change`, which also feeds `edit_log` for the parked views. While open on a language server (`lsp_tracked`) every change records a `DocEdit` in `lsp_pending` before the rope changes (at most 512; past that, or for an edit inside a CRLF break, `lsp_full_resync` sends the whole text). |
| `editor/carets.rs` | Commands that add and remove carets: Add Next Occurrence (Alt+J), Unselect Occurrence, Select All Occurrences (from `search::occurrences`, or the find bar's matches), Clone Caret Above/Below. |
| `editor/fold.rs` | Code folding. `Fold`: two char anchors; `lines` derives its header line and the last line it hides (the closing line hides only when nothing but closing punctuation follows, so `} else {` stays). `Folds`: a view's collapsed folds (`ViewState::folds`), mapped through every change (`map_edit`: an anchor inside replaced text drops the fold, an insert at the end stays outside), `rebuild` of the merged hidden line ranges, `reveal` (unfold what hides a caret), `header_at`, `collapse`, `expand_at`, session conversion. Candidates: `Buffer::fold_candidates(lines)` (one per header line, the largest; cached per version and lines in `Buffer::fold_cache`) and `fold_chain(line)` (innermost first), from the syntax tree (a named node hiding a line that ends in a closing token, is a body kind or a comment, never a string; runs of line comments and of imports; a body that starts its own line, as in Python or Allman braces, takes the line before as its header), in a patch tab from its sections (`Patch::folds`: `== ` commit > `diff ` file > `@@` hunk), or else from the indentation. The commands on `Buffer`: `fold_collapse`, `fold_expand`, `fold_collapse_all`, `fold_selection`, `toggle_fold` (the gutter marker), and `carets_out_of_folds`. Line commands see a collapsed fold as one line: `Buffer::fold_block` widens a line run over the hidden lines (used by `line_blocks`, `delete_line`, `duplicate_line`, `select_line`, `move_lines` and its neighbour), and `duplicate_line` / `move_lines` re-collapse the folds of the copied or moved lines through `Folds::collapse_saved`; `reveal` leaves a fold alone when a selection covers all its hidden text. A transaction remembers the folds its edits dropped (`Transaction::folds`, from `apply_change`'s return), and undo restores them in the undoing view only. |
| `editor/inlay.rs` | LSP inlay hints as virtual text. `Inlay { at, label, width, pad_left, pad_right, param }` (`Inlay::new` sanitises the label: controls become spaces, at most `MAX_LABEL_WIDTH` cells with `…`; `None` when nothing visible is left). `InlayHints` (`Buffer::inlays`, sorted by `at`, at most `MAX_INLAYS`): `set` (an answer, with the buffer version and lines it covers), `clear`, `forget_fetch`, `on_line` (the hints of a line, its end included), `width_on_line`, `generation` (bumped when hints are set, dropped, moved; part of the wrap cache key), and `map_edit`, which `Buffer::apply_change` calls for every change (undo and redo too; a whole-text replacement clears): a hint before the edit stays, inside replaced text it drops, after it shifts; at an insertion point a parameter hint stays and any other hint moves after the inserted text; at the start of replaced text it stays, at its end it moves to the end of the new text. `NO_INLAYS` is what `Rows::new` points at. |
| `editor/rows.rs` | `Rows`: the screen rows of a view, `Row { line, seg }` (ordered as on screen; `seg` is signed: negative for the top fillers before line 0). Line-level walks under the folds (`is_hidden`, `visible`, `next`/`prev`, `step`, `distance`) and row-level ones that also split a wrapped line into its segments (`segs`, `seg_range`, `row_of`, `clamp`, `next_row`/`prev_row`, `step_row`, `row_distance` with a cap, `at_offset`, `iter_from`, `last_row`). `Filler { line, rows, block, bg, top }`: blank rows after a line (or, with `top`, before line 0) that no text backs (`ViewState::fillers`, set by the resolver every frame); they are extra segments after the line's text ones (`text_segs`, `segs` = text plus fillers), so every walk counts them (top ones sit at segs `-top_rows()..0` of line 0: `first_seg`, `last_seg`, `top_rows`, `filler_row`); `is_filler`, `filler` (entry and index), `text_row` (moves a caret off a filler row). `Rows::new` never wraps and has no fillers or hints; `Rows::view` (or `Buffer::rows`) takes a view's folds, `Wrap` and fillers, and `with_inlays` the buffer's hints (`inlays_on`, `inlay_width`; `line_wrap` passes them to the wrap). |
| `editor/wrap.rs` | Soft wrap. `wrap_line` breaks a line for a text width on the `layout_line` walk: before the first grapheme after whitespace (which hangs), else before the grapheme that does not fit; the last column stays free for an end-of-row caret; continuation rows are indented by the leading indent, at most half the width; lines over 100k chars do not wrap. `LineWrap`: the segment starts (char offset and unwrapped column) and the indent. `Wrap` (one per view): the toggle, the width and tab width set by each render, and a per-line cache of breaks keyed by buffer version, geometry and `InlayHints::generation`. Hints count as units with the text cell after them: the break goes before the hint (the unit's column), so a hint and its char share a row; a hint at the line end hangs like trailing whitespace and never breaks. `LineWrap::starts` columns include hints. |
| `editor/bookmarks.rs` | Bookmarks across buffers: the Shift+F11 chooser (`BookmarkTarget`: a line of an open buffer, or a closed file's path and bookmarked line), numbered bookmarks (Ctrl+F11 opens a confirm dialog with a button per digit, `DialogId::BookmarkDigit`; `set_bookmark_digit` gives the digit to the line and removes the bookmark that had it, so a digit is unique in the session; the line's own digit removes its bookmark), jumps (`goto_digit`: Ctrl+0..9 or Alt+B then a digit, pushing back/forward history; a closed file reopens through `pending_nav` with `PendingAt::Line`). Line bookmarks and their digits live in `Buffer::bookmarks` and follow edits in `apply_change`; when a file's last tab closes (`force_close`, `force_close_all`) all its bookmarks, plain and numbered, wait in `Editor::closed_bookmarks` (path to line to digit, `park_bookmarks`), follow renames and deletes, and go back into the buffer when it opens (`open_text`; lines past the end land on the last one). |
| `editor/snippet.rs` | Completion snippet tabstops. `Tabstop` (number, char range, enclosing placeholder). `SnippetSession` (`Editor::snippet`, one at a time): the stops of the accepted snippet, the groups in Tab order (numbers ascending, `$0` last) and the current one; `catch_up` maps the stops through the buffer's `EditLog` like the resolver's anchors (the current group's stops grow with text typed at their ends, the others keep out of it; an edit in a current stop marks it edited, which kills the stops nested in it) and gives up on an undo below the insertion, a reload or lost edits; `follow` keeps the session while the primary caret is in a live stop (switching to its group); `step` is Tab and Shift+Tab (`editor.next_tabstop`/`prev_tabstop` in the `editor.snippet` context; past the last group it selects `$0` and ends). `Editor::snippet_check` drops it when its group or buffer is no longer the shown one; `sync_parked` runs `snippet_catch_up` before the logs are cleared; Esc ends it. |
| `editor/merge.rs` | The conflict resolver. `open_merge` puts the groups, tree and focus aside (`Saved`, which `session_layout` returns for sessions) and lays out three scratch buffers without a path (yours and theirs `Buffer::read_only`, the result from the base with the non-conflicting chunks applied as one undo step) as three groups, or two below `TWO_PANE_BELOW` columns (yours and theirs tabs of the left one; `merge_fit` switches on resize). `MergeSession`: the chunks (`git::merge::chunks`), each chunk's anchor in the result (chars) and `ChunkState` (each side `Pending`, `Applied`, `Ignored`; `edited` by hand), and a history of both by the result's undo state. `catch_up` maps the anchors through the result's `EditLog` (from `sync_parked` and before every command; an undo state seen before restores exactly). Commands (`MergeAction`, the `editor.merge` keymap context): accept yours/theirs at the caret (after the other side: both), ignore, apply non-conflicting, whole file, next/previous unresolved, Apply (the file's buffer gets the result as one change, is written and staged), Cancel. Rendering: `merge_decor` (`view::LineDecor`: chunk backgrounds and the `»` `«` `×` glyph column left of the gutter, clicked through `merge_mouse`), the header row with the counter and buttons (`EditorHit::MergeHeader`), linked scroll (`merge_master` leads, drawn first; `merge_fill` pads the panes with `Filler`s so each block - the run before the first chunk, each chunk, each run between - takes the same rows in all three; an empty block at the top of a pane gets top fillers before line 0; `merge_follow` maps its top by block and offset, `block_offset`/`block_row`), splitter cells in the chunk colors. `merge_guard` refuses layout commands and closes on CloseTab. |
| `editor/patch.rs` | Patch tabs (Git Log Show Diff): `Patch::parse` finds the added and removed lines of a unified diff text (inside `@@` hunks only, so `--- a/x` headers do not count) as `PatchLine` runs and the first line of every block of changes. `Patch::decor` gives them `diff_add_bg` / `diff_del_bg` backgrounds through `view::LineDecor` (fed by `Editor::render` for the shown patch tabs only); `Buffer::goto_change` steps through `change_starts` when `Buffer::patch` is set, so F7 / Shift+F7 work. `Editor::open_patch` opens a read-only pathless buffer (activating the open one of the same spans); Save As turns it into an ordinary file. `Patch::parse` also records the section header lines (`Section`: `== ` commit, `diff ` file, `@@` hunk); `Patch::folds` nests them commit > file > hunk, each running to the line before the next header of its level or an outer one, trailing empty lines left out, ending at the next line's start (so the placeholder is a plain `…`). `find_candidates` and `fold_chain` use them instead of indent folds; the header lines stay valid because a patch tab is read-only and Save As clears `patch`. |
| `editor/column.rs` | Column selection and virtual space on a `Buffer`: `set_block`, `current_block`, `extend_block` (Shift+arrows, Home, End in column selection mode), `blocks_supported` (false while the view soft-wraps), `virtual_pads`, `virtual_delete`, `virtual_caret` (Alt+click and column-mode click past a line end) and `move_virtual` (arrows through virtual space). |
| `editor/{find,io}.rs` | Find/replace bar (keys, stepping, rendering); saving and autosave. `io.rs`: `prepare_write` (path and dirty checks, then `cleanup_for_save`), `write_text` and `save_as` share `mark_written` (clean, mtime, `recovered` cleared, the written text as a local history `Capture` through `Effect::History`, then `Effect::FileSaved`); `save_as` writes first and only on success takes the new path (`adopt_path` with `Editor::path_setup`, also used by `open_text`: name, language detected again, highlighting, mtime, `.editorconfig`, indent, soft wrap). `save_all` and `save_and_close_ids` skip untitled buffers; Save and Save All leave them to `Effect::SaveAs`. `unsaved_captures` returns local history captures (rope clones, `Kind::Unsaved`) of the titled, dirty, writable, non-merge buffers whose `version` differs from `history_seen`, marking each seen (one over `max_file_kb` too, without a capture); the App queues them. `discard_captures` is the same filter for "Don't Save" (all buffers or a list of ids, `Kind::BeforeGit`, ignores `history_seen`, `&self`). |
| `editor/format.rs` | Formatters: `Formatter` (the `[format]` command of the language, which wins, else `Lsp`: the app decides whether the server formats), `FormatRequest` (the buffer snapshot: text, version, undo state, path), `format_done` (apply only while the version and the path are unchanged, warn on failure, then write for a save, nothing when that text is already saved), and `Buffer::apply_formatted`: an imara-diff line diff turned into one transaction of the changed hunks, the selection mapped by the code before it, the view kept on its lines. |
| `editor/view.rs` | `layout_line_inlays`: the grapheme walk, with the line's inlay hints emitted as cells before the grapheme at their anchor (`CellKind::InlayPad` / `InlayLabel`, `Cell::is_inlay`; `char_idx` is the anchor; line-end hints after the last grapheme); `layout_line` is the same walk without hints, in text columns (`visual_col`, `col_to_char`, `col_to_char_ceil`, `block_selections`, column mode). `layout_row` walks with the hints, and so do `char_to_row_x` and `row_x_to_char`: a caret at an anchor stands after the hints (`char_to_row_x` skips hint cells), a click on a hint gives its anchor, `caret_col` is the caret's row x (virtual space plus the line's hints), `text_col_at` takes the hints out of a screen column for column blocks and virtual carets. `render` draws hint cells first in the callback, in `theme.inlay_hint`, with no highlight, selection, search or caret styling. Controls and the U+2028/U+2029 separators (`util::text::shown_escaped`) are one grapheme drawn as a dim escape, `^X` or `\u{…}`. On the walk, per screen row, `layout_row` (each cell with its row x: `RowGeom` indent plus column minus the segment's first column), `char_to_row_x` (caret to row and x) and `row_x_to_char` (click to char); a filler row has no cells (`row_geom` empty at the line end, `layout_row` calls nothing, a click gives the line end (the line start on a top filler), `render` paints only its background); `block_selections` turns a column block into carets (virtual ones at the short lines' ends); the caret, the terminal cursor and `ensure_visible` follow `caret_col`. `LineDecor` (the conflict resolver's line backgrounds and glyph column, `RenderOpts::decor`; `RenderOpts::follow` false keeps the scroll a linked pane was given). `render` sets the view's wrap geometry, walks the `Rows` from the top row, draws the gutter on a line's first row (line number, bookmark (`◆`, or a numbered bookmark's digit in reverse video), diff or diagnostic mark, fold marker: `▾` where a region can fold, `▸` on a folded line, which also shows the marks of the lines it hides; `↪` on wrapped continuation rows), and on a line's last row the line-break selection cell, a line-end caret and a collapsed fold's `placeholder`. A wrapped view never scrolls sideways. `render` draws indent guides in the per-cell callback (leading whitespace, line column a multiple of the indent step, first row of a line only) and, on blank lines, after the row walk in virtual cells at the stops below the smaller neighbour indent (`blank_guide_levels`, one pass per frame, `GUIDE_SCAN` lines beyond the window); pure paint, mouse mapping unaffected. Rendering draws `Buffer::diag_marks`: a glyph in the diff mark column (it wins over the diff mark) and the `diag_*` theme style's modifiers over the text of a diagnostic; text tagged unnecessary takes `dim`'s foreground, deprecated text is struck through. |
| `editor/gutter.rs` | Git marks against the HEAD text (`Buffer::diff_base`), and the change commands built on the same diff: revert the change at the caret (one `Buffer::apply`, so undoable), next/previous change. `revert_spans` (old lines back over new ones for line-range pairs, last first) is the one hunk revert, also used by the Local History window's Revert Change. |
| `editor/blame.rs` | The blame column (`Buffer::blame`): author and date per line from `git::repo::blame` of HEAD, matched to buffer lines through the HEAD diff (recomputed with the gutter at tick rate, so edits shift it and edited lines show none). |
| `editor/syntax.rs` | Tree-sitter highlighting and language detection. `LanguageDef`: the highlight query, the tags query (symbols) and the injections query (`injections`, compiled on first use; Markdown fences, front matter and HTML blocks, HTML and Vue `<script>` / `<style>` with the `lang` attribute). `Syntax`: the host parser and tree, and `Injection`s, each an inner tree parsed by a second parser over the content node's included ranges (named children cut out), so its nodes carry document offsets. After each host reparse `update_injections` queries only the edited and changed region; an inner tree is parsed again, from its old tree, only when an edit touched it or its ranges changed, the others are shifted by `note_change`. At most `MAX_INJECTIONS` per buffer; no injections inside injections. `tree_at` / `tree_at_char` give the tree that owns a position (inner or host) for bracket matching, string checks of auto-close, closer dedent and Extend Selection; `trees` (host first) feeds the fold candidates; `highlights` queries the host, then the injections that meet the window, and flattens (the inner style wins). The Structure window and the symbol index use the host tree only. |
| `editor/…` | `selection` (`Selection`, `Selections`, motions), `undo`, `search`, `indent` (`.editorconfig`, detection), `brackets`, `expand`, `nav` (back/forward). |
| `files/tree.rs` | Project tool window: lazy `ignore` walk, `TreeRefresh` off the main thread, git status colors, speed search, context menu. |
| `files/complete.rs` | Shell-like completion of a typed path (`complete`: one `read_dir` of the folder before the last `/`, resolved like `App::resolve_save_as` with `worktree::expand_tilde_in`; matches sorted, folders with `/`, dotfiles only for a `.` prefix, case-sensitive, names that are not UTF-8 skipped; inserts the only match's rest or the common prefix; `folders_only`, the worktree path prompt, drops files from insert, matches and count). |
| `files/ops.rs` | `FsOp` (create, rename, delete; the tree never overwrites and deletes recursively, workspace edits pass `overwrite` and `recursive` from the server; never follows symlinks on delete) and name checks for the tree dialogs. |
| `files/walk.rs` | The `ignore` walk builder shared by the tree, Go to File and find in files. |
| `files/find.rs`, `files/find_panel.rs` | Find in files: the search (`Search::run` streams `Batch`es, cancelled by seq) and replacements; the Find tool window. |
| `files/watch.rs` | Debounced recursive watch of the root (notify); splits batches into working tree paths and a git flag; `Changes::events` keeps per-path `FileChange`s (`change_of`, merged by `events_of`) for language servers, and `start_outside` watches the folders servers registered outside the root. Opens and reads are not changes (jetl's own reads would loop). `Changes::renames` holds the `RenameMode::Both` events with both paths inside the root and outside git dirs (`renames_of`); `App::fs_changed` hands them to `history_moved` before anything else, so Local History follows `mv` and file managers through the Task 4 move path (the worker re-checks old gone / new present; `store::RENAME_ECHO` stops a second marker for renames jetl made itself). Separate delete and create events are not paired. `changes_content` and `DEBOUNCE` are shared with the config watcher. |
| `symbols/mod.rs` | `Symbol` and `extract`: runs a language's tags query (`LanguageDef::tags`, from the grammar crates' `TAGS_QUERY`: Rust, JavaScript, TypeScript/TSX, Python, Go, C, C++, Java) over a tree; definitions only, with nesting depth. |
| `symbols/index.rs` | `SymbolIndex`: the project symbols behind Go to Symbol. Follows the file index (files with tags, at most 5000, each at most 256 KiB), parses in `spawn_blocking` (`IndexJob`), skips files whose mtime is unchanged, drops and re-queues watcher paths. |
| `symbols/panel.rs` | Structure tool window: the active buffer's symbols from its live tree, re-extracted when the buffer version, language or active tab changes; the selection follows the caret while unfocused. |
| `git/mod.rs` | `GitService` (spawns work, tracks busy ops; `run_cli_steps` runs several commands as one operation and stops at the first failure), `GitMsg`, `GitOp`, `GitRequest`. |
| `git/repo.rs` | Synchronous git2 calls; each opens the repository itself: status (with rename detection; it also reads `repo.state()` into `in_progress`, the rebase step from `rebase-merge/msgnum`/`end` or `rebase-apply/next`/`last`), blobs, conflict stages with labels for yours and theirs from the operation in progress, or the stash after a stash pop/apply (`conflict_sides`) and taking one side whole (`resolve_side`; a symbolic link stage is recreated as a link, a gitlink stage is staged without touching the work tree, and `conflict_sides` reports them as `special`), stage/unstage, discard (`checkout_head` for staged paths, `checkout_index` for unstaged ones, new files deleted), commit and amend, branches (local, then remote), the push remote, stashes (push of tracked files, list, drop; pop and apply, and a push with untracked files, are in `cli.rs`), blame. A renamed buffer keeps its old path as `Buffer::base_path` (set by `Editor::retarget`, cleared by Save As, not saved in the session), and the HEAD blob and blame fall back to it (`blob_text`, `blame` take it) until the rename is committed. |
| `git/cli.rs` | The `git` command line: network, switch, log/show, worktrees, `git apply` of a patch on stdin (`git_with_input`), merge, rebase, cherry-pick, revert, stash pop/apply (`stash_apply`: the stash found by commit oid in `git stash list`), branch and tag commands, `push_steps` (`--force-with-lease` only, never a plain force; tags as a second step) and `stash_push_args` (`--include-untracked`). No prompts, git's messages in the C locale (`LC_MESSAGES=C`, they are parsed), 120 s timeout. |
| `git/merge.rs` | Pure three-way merge input: `ConflictSides` (stages 1-3, the work file, labels, `special: Option<Special>` for a symbolic link or submodule), `MergeInput::classify` (a `Special` side is checked first and allows only taking a side whole; a missing base is empty; a binary side, a deleted side or text that is not UTF-8 allow only taking a side whole; no stages parses the work file's markers, `parse_markers`, diff3 included; CRLF and LF made alike to the work file's ending when the sides differ), `chunks` (the base-to-yours and base-to-theirs hunks grouped where they overlap or touch: `OursOnly`, `TheirsOnly`, `BothSame`, `Conflict`, each with its line ranges in the three texts). |
| `git/diff.rs` | `DiffDoc`: raw lines of both sides, side-by-side and unified rows, changed blocks, a unified patch of one block (`block_patch`, for hunk staging), and changed characters of modified line pairs (a second imara-diff pass). |
| `git/graph.rs` | `LogGraph`: lays out the Log tab's lanes from each commit's parents (`%P`), carried from page to page. |
| `git/messages.rs` | Commit message history per repository (keyed by its common git dir, so worktrees share it) in `state/commit_messages/`. |
| `git/model.rs` | Shared types: status entries (`old_rel` for renames), `DiffKey`, log rows and `LogFilter`, commit details, `BranchInfo` and `BranchOp`, `Blame`, `StashEntry`, `InProgress` / `InProgressKind` (operation stopped halfway, with the rebase step). |
| `git/panel/` | The Git tool window: shared state, input routing, header (it shows the operation in progress after the tabs; its tools become Continue / Skip / Abort while one is in progress) and console (`mod.rs`); the diff pane of every tab (`diff_view.rs`: the header row, `render_diff_header`, with the layout toggles at its right end and the title cut before them; folded unchanged runs, the current change, syntax colors, changed characters, side-by-side/unified); the Commit tab with amend and message history (`commit.rs`); the Log tab with its filter bar, commit actions (including Reset Current Branch to Here…, `GitAction::LogReset`) and, for the branches at the selected commit, one `Branch '<name>' ▸` context menu row each that opens the Branches popup's operations (`GitAction::LogBranch`; `log.rs`); Show Diff (Ctrl+D, `GitAction::LogShowDiff`; `log_sel` and `log_anchor` hold the Log's multi-selection beside the cursor row (Shift+Up/Down, Shift/Ctrl+click; any move, plain click, reload or filter change collapses it; commit actions use the cursor row only) and `log_selection()` lists the rows: `commits_diff` cuts the selected rows into first-parent chains, `GitRequest::CommitsDiff` builds the text, `App::open_patch` shows it); the Stash tab (`stash.rs`; its `[ ] Untracked` toggle, `stash_untracked`, makes the stash include untracked files). `diff_view.rs`'s `DiffPane`, `render_diff` and `render_diff_header` (whose toggles come back as `DiffToggle`s) are also the Local History window's; only that window sets `DiffPane::anchor`, the other end of a range of selected changes (Shift+arrows, a Shift or Ctrl click, select all), whose line numbers render like the current change's. |
| `ui/` | `render` (frame order), `hover` (the Quick Documentation popup: markdown shown plainly, fenced code highlighted by its fence's language (else the buffer's) through `syntax::highlight_text`; `DocLine`, `wrap_lines`, `draw_lines` shared with the completion docs panel; placed under or above the caret cell `Editor::caret_cell` of the focused group, scrolled by its keys), `signature` (the Parameter Info popup: one row per signature, at most eight, the active parameter bold through `param_range` (a `Simple` label is found in the signature, `LabelOffsets` are UTF-16); above the caret line, below it when there is no room above, none when neither fits; `1/N` on the bottom border), `completion` (the completion popup: at most ten rows of kind glyph, label with the chars the typed word matched highlighted (`CompletionState::match_indices`, `widgets::highlight`), `labelDetails.detail` dimmed after it and the dimmed detail, under or above the caret line with the labels under the typed word; the documentation panel of the selected item beside the list, else stacked on the far side from the caret line, built by `docs_lines` and drawn with `hover::draw_lines`, scrolled by Alt+Up/Alt+Down and the wheel), `layout` (rects and hit testing; zen mode, where the editor text fills the screen and `LayoutState::zen` keeps the hidden tool windows and focus: showing any tool window leaves it, and sessions save the hidden windows, never zen itself), `focus` (panel and tool window ids), `dialog` (input dialogs may carry a `PathCompletion` (`with_path_completion`, or `with_folder_completion` for folders only): the Tab state and the hint, a note or a grid of matches under the input, at most 8 rows), `palette` (Locations rows are grouped by file under `file_rows::file_header` headers when the items span several files, a header being selectable and opening the file's first location; each row marks its usage's range with `search_match_bg`), `menu` (context menu, also the main menu's dropdowns; rows can be separators), `menu_bar` (the main menu: the static `MAIN_MENU` table of labels and registry ids, a bar over the top screen row with the open menu's dropdown under its title), `status_bar`, `tabs`, `toasts`, `tool_strip` (the left edge: a main menu button on the top row, then one two-row button per tool window, its Alt-number over `ToolWindowId::icon()`, emoji or one-cell symbols per `[ui] icons`; the hit rects go to `LayoutState::strip_hits` as `StripButton`s, and the hovered one, `App::strip_hover`, gets a one-line tooltip with its name and shortcut), `event_log` (the Event Log tool window over `App::notifications`: first line per entry, full text on Enter, follows new entries while its last row is selected and in view; the status bar notification opens it), `widgets/{list,text_input,file_rows}` (single-line `text_input`s scroll sideways to keep the cursor visible; `file_rows`: the file header and item spans of file-grouped lists, shared by Find and Problems), `snapshot` (`cfg(test)`: a drawn `TestBackend` frame as text, with `Mark`s that name theme colors in a grid under it, and `row_text`). |
| `util/` | `text` (width helpers), `coalesce` (one background request at a time), `fs` (`write_atomic`, shared by saves and replace in files; `normalize`, a lexical `.` and `..` pass), `clipboard` (clipboard tool selection from the environment and `[clipboard]`, running them with a 1 s timeout), `format` (running a formatter: stdin to stdout, its own session, the process group killed on timeout or when dropped), `process_gone` (tests, Linux: polls `/proc` until a killed process is gone, a zombie or being reaped). |

## Event flow

`App::run` is a `tokio::select!` over three sources, then drains the bus and
redraws when `dirty_frame` is set:

1. **Terminal events** (`EventStream`). Keys go to the open dialog (Tab in a
   path-completing input goes to `App::complete_dialog_path`), else the
   main menu bar, else the context menu, else the palette, else the hover
   popup (Esc and its scroll keys; any other key closes it and goes on),
   else the focused Terminal window (which
   keeps only its `[terminal]` keymap table and Esc Esc for jetl and sends
   the rest to the shell), else the find bar, commit message box,
   Find window fields, the tree's speed search or the Local History
   window's filter (which consume their keys
   and let the rest fall through), else `Dispatcher::dispatch` with the
   context stack from `App::contexts()` (`dialog` | `palette` |
   `editor.find`, `editor.snippet` while a completion snippet's tabstops
   are walked, `editor.merge` while the conflict resolver is open,
   focused panel, `global`). An `Action` reaches
   `App::perform`, which hands it to the owning panel. Mouse events go
   through `app/mouse.rs`: an active drag owns all events; otherwise
   `LayoutState::hit_test` picks splitter, strip, status bar, tabs or a
   panel (over the editor area `Editor::hit_test` then picks a group
   splitter, tab, tab bar or group body), and the panel gets a `Mouse`
   with the click count resolved.
2. **The bus** (`mpsc::unbounded_channel` of `AppMsg`). Background work
   reports here: file loads, sessions, the file index, tree listings,
   worktree lists, find in files batches, tree file operations
   (`FsOpDone`), symbol index batches, palette filter results
   (`PaletteFiltered`), the recent projects list, clipboard
   reads (`Paste`), formatter results (`Formatted`), Run task output and
   exits and the task picker's list, the Terminal shell's output wakes,
   language server events (`AppMsg::Lsp`), local history lists and
   snapshots (`AppMsg::LocalHistory`), `AppMsg::Git(GitMsg)`, config
   file changes (`ConfigChanged`), and the filesystem watcher
   (`FsChanged`: refresh the tree if a listed directory changed, refresh git
   status, reload clean tabs in place, ask once about tabs with unsaved
   edits). Without a watcher (off, or it failed to start), focus gain
   refreshes instead.
3. **A 100 ms tick**: toast and status bar notification expiry, gutter
   recomputation (debounces typing),
   the busy spinner, the Run window's spinner and elapsed time while a task
   runs, idle autosave, snapshots of unsaved edits when due
   (`App::unsaved_snapshots`), and the terminal's held-back Esc once no
   second one followed.

After the bus is drained (and unless quitting) the loop refreshes the
crash recovery snapshot (`App::recovery_snapshot`, only when a dirty
buffer's version, saved state or path, or the generation, changed), then
calls `App::lsp_sync`, so the edits of one batch of input reach the language
server as one `didChange` per buffer. Leaving the loop on quit clears the
snapshot and removes this process's recovery directory, then runs
`App::shutdown`: the queued local history snapshots are flushed, the
session saved, formatters, the Run task and the Terminal shell killed,
and the language servers shut down; nothing there draws. Leaving it with
an error (`term.draw` or `term.clear` failed) returns at once; `main.rs`
then calls `App::finish_after_error`, which writes the dirty buffers as a
crash dump (`App::dump_after_error`) and then runs the same
`App::shutdown`, before restoring the terminal. The dump stays.

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
splitters, editor, status bar, hover popup, completion popup, strip tooltip (hidden while an overlay is
open), palette, context menu, main menu bar (over the top row, in zen
mode too), Settings window, dialog, toasts (laid out below any open overlay they would cover, never over one). Mouse events go to the topmost open overlay (dialog, Settings window, main
menu bar, context menu, palette) before anything below (the completion
popup takes the wheel over it and accepts a clicked row, other clicks close
it; the hover popup takes the wheel over it, and any click closes it); the menus also get
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

Three tags, and one rule for popups:

- **Generation** (`Workspace::generation`) is bumped by every root switch
  (`Workspace::switch_root`). Every result that depends on the root carries
  it: `FileLoaded`, `SessionLoaded`, `FileIndex`, `TreeLoaded`,
  `Worktrees`, `Find`, `ReplaceDone`, `FsChanged`, `WatcherStarted`,
  `DiskText`, `FsOpDone`, `Symbols`, `PaletteFiltered`, `Paste`, `Formatted`, `RunOutput`,
  `RunExited`, `RunTasks`, `TerminalOutput`, every `LspMsg`, `Locations`, every
  `GitMsg`. `GitService`
  keeps a copy, set by `set_root(root, generation)`, to tag its own
  results. A result whose generation differs from `workspace.generation` is
  dropped. `ConfigChanged` carries no tag: it does not depend on the root,
  and the reload reads the files when it runs.
- `GitMsg::CommitsDiff` carries the generation only: its spans are immutable commits, and a repeated request for the same spans (kept on `Patch::spans`) activates the open tab.
- **Group id**: LSP doc answers (`DocTag::group`) carry the focused editor
  group of their request and are dropped when another group is focused
  when they arrive (the same buffer in two groups shows no popup in the
  other one); completion and its docs answers check it too.
- **Seq** orders repeated requests of the same kind within a generation, so
  an older request that finishes late cannot overwrite a newer result:
  `GitMsg::Status` (`GitPanel::set_status`), `GitMsg::Blob` (per buffer,
  `Buffer::diff_base_seq`), `GitMsg::Blame` (per buffer, `BlameState::seq`;
  a buffer whose column was hidden meanwhile drops it), `FileIndex` (`FileIndex::seq`), `TreeLoaded`
  (`ProjectPanel::finish_refresh`), `Find` batches (`FindPanel::on_batch`;
  a newer search also stops the old walk through a shared cancel seq),
  `Symbols` batches (`SymbolIndex::on_batch`; only the newest run applies,
  and it stops older runs through a cancel seq), `PaletteFiltered` (one
  app-wide `App::palette_seq` shared by every palette through `FilterBus`:
  every refilter takes the next seq, so only the newest filter of the open
  palette applies; a running filter checks it every 1024 candidates and
  stops without sending; closing the palette cancels its filter),
  `RecentProjects`
  (`App::recent_projects_version`: only a newer file version applies;
  `App::recent_projects_forgot`: the seq of the latest forget, replies to older requests are dropped; not tied to the
  root, so it carries no generation), `Paste` (only the newest request applies, and only while
  the text field it was asked for still has focus), `Formatted` (per
  buffer, only the newest request; the text is applied only while
  `Buffer::version` and the buffer's path still match the snapshot), `RunOutput` and
  `RunExited` (only the current run's `RunState::seq`; a new run, a root
  switch or quitting forgets the old one after killing its process group),
  `RunTasks` (the newest load), `TerminalOutput` (only the running shell's
  id, `TerminalPanel::seq`; the wake carries no bytes, it tells the app to
  drain the shell's inbox, so there is at most one per shell on the bus and
  a burst of output is parsed and drawn once), `LspMsg` (the `ServerId`
  of the current client for that server name: a restarted or stopped
  server's late messages are dropped; requests carry a `PendingTag` with
  the buffer, its version, the position and a seq: only the newest request
  of a method is answered (`LspService::inflight`; a newer one cancels it
  with `$/cancelRequest`), and only while the buffer, its version and the
  caret are unchanged; completion answers instead while the caret is in
  the word they were asked for, `lsp::completion::Origin`; formatting
  answers match their buffer's format job; inlay hint answers (`PendingTag::InlayHints`)
  match `LspService::inlay`'s seq and the buffer version; a command's answer only reports
  errors), `workspace/symbol` answers (only the newest palette query's seq, `LspService::ws_symbols`), `Locations` (only the newest definition or usages
  request, `LspService::nav_seq`), `EditsApplied` (only for the current root; a previous root's renames still reach local history, as FsOpDone). A result is applied when its seq is not
  older than the last applied one. Results keyed by what they describe
  need no seq: diffs by `DiffKey` (each diff pane ignores keys it does not
  show), commit details by sha (the Log or Stash tab, whichever has it
  selected), log pages by `skip` and their `LogFilter` (a page of a
  replaced filter is dropped). Stash operations name the stash by its
  commit, not its index, so a list that changed meanwhile cannot hit the
  wrong entry.

The hover and completion popups belong to the editor group they opened in
(`HoverPopup::group`, `CompletionPopup::group`): `hover_check` and the
completion refilter close them as soon as another group has focus, and
they anchor at the focused group's caret cell. A snippet session belongs
to its buffer and group the same way (`Editor::snippet_check`), and its
tabstops are positions in the text: they follow every edit through the
`EditLog`, and an undo of the insertion or a reload ends the session.

Local history (`history/`) is the exception to `spawn_blocking`: every
store access runs on its own thread, `jetl-history`, in the order queued,
so the dedupe check, index appends and prune never race in one process and
a list queued after a capture sees it (the startup prune is the first job).
A destructive operation that rewrites files (a git operation for which
`GitOp::touches_worktree`, `worktree remove --force` (whose `before_git`
runs on the removed worktree, not the root; `GitService::before`), a tree
delete) takes an `Ack` from the handle
before it starts and waits on it inside its blocking task (a CLI op in a
`spawn_blocking` of its own), at most 10 s; Overwrite and Save As read the
file on the main thread and queue the bytes, which is enough since the
write that follows is synchronous. `Job::Rename` has no `Ack` (nothing is
destroyed); its ordering relies on `fs_op_done` queueing it on the main
thread after every capture of the old path and before `Editor::retarget`,
so later captures land under the new key after the marker. Its results carry no generation (the
store is global, and a snapshot is still worth showing after a root
switch): a list is applied while the Local History window shows its path
(the worker answers in queue order, so the last list of a path is the
newest; no seq), a scan (`Recent`) while its project view shows that
folder (the same rule; the panel keeps one scan in flight and asks once
more when it lands if anything asked meanwhile, so a git operation's burst
of `Stored` reports costs two scans; F5 always asks), a `Stored` report and `Labelled` / `LabelRemoved` (which
always give their toast) list again while the window shows that path, a
blob while the window shows its path with that snapshot selected. A
revert of the diff's selected changes needs no worker answer; its edits
apply only while the buffer's text equals the diff's current side (a full
compare at apply time, also on `FileLoaded` for a file it opened), since
the diff may have been made before an edit, an undo or a reload. Its diff
is `HistoryMsg::Diff` keyed by `DiffKey::Local` and the window's diff seq:
only the newest request applies, so of two loads of the same snapshot (a
list refresh while one runs) the older cannot land last. `load_local_diff` reads (and
inflates) its blob in `spawn_blocking` rather than on `jetl-history`, which
is safe because a blob file only appears by a rename; compression runs only
on `jetl-history`.

Palette filtering of Files and Symbols at or above `BACKGROUND_MIN`
(10000) candidates with a non-empty query runs in `spawn_blocking`;
smaller lists, the empty query and a tab switch rank synchronously. While a
filter runs, the palette keeps showing the previous rows, ranked against
the candidate `Arc`s and query in `Shown` (render and accept read only
those, and the match highlight uses that query); results keep the
selection by item identity unless the query changed. Enter with a filter
pending ranks the current query synchronously once, unless the user moved
the selection, which accepts the shown row. A query that extends the shown one (same candidate `Arc`s and source in
`Shown`, no `! \ ^ $ '`, see `narrowable`) ranks only the shown rows, which
are the complete match set of the shown query, and gives the same rows as
a full rank; a narrowed subset under `BACKGROUND_MIN` ranks at once, and
Enter's synchronous rank narrows too. Palettes built without
`App::open_palette` (Locations, tests) have no `FilterBus`
and always rank synchronously.

Save As's Tab completion (and the worktree path prompt's, folders only,
relative to the project root) reads one directory on the main thread: an
explicit keypress, and Enter touches the same path synchronously anyway;
there is no result to go stale.

New background work follows the same pattern: a generation always, a seq
when the request can repeat while one is in flight. Requests that bursts can
trigger (git status, tree refresh, symbol index runs) also go through
`util::Coalesce`: at most one runs, a request meanwhile runs once after it,
and a root switch resets it. A long run that reports in batches calls
`Coalesce::touch` so it is not taken for lost. `GitMsg::OpDone` clears
its busy id before the generation check, so the spinner never sticks.

## Conventions

- All buffer mutation goes through `Buffer::apply(changes, selections,
  kind)` (or `apply_then`), which records undo with the carets before and
  after. `rope` is a public field for reading only. Reloading from disk is
  an `apply` too, so it can be undone. An edit made at every caret goes
  through `Buffer::edit_carets` (see Carets).
- Rendering and mouse mapping in the editor share one `layout_line()` walk
  (`editor/view.rs`), soft wrap included (`layout_row`, `char_to_row_x`,
  `row_x_to_char`); tests click every cell of wrapped text and check the
  caret lands on that row and stays put when its cell is clicked.
  Text-column helpers walk without hints; geometry (`layout_row` and
  everything on it) walks with them.
- Visible rows only through `Rows` (`editor/rows.rs`): rendering, scrolling
  (the top row `scroll_line`/`scroll_seg`: a buffer line a fold never
  hides, and a segment render clamps), Up/Down and PageUp/PageDown
  (`selection::vertical_target`), mouse rows, Clone Caret and column
  blocks. Anything that moves by screen rows uses the row-level methods on
  `Buffer::rows()` (or `Rows::view` where the carets are borrowed mutably),
  so it is right with wrap on; `Rows::new` is for line-level questions
  (hidden, visible). With wrap on, desired columns are row x (the caret
  keeps its place on screen), Home and End stop at the row's ends first,
  and the breaks are cached per view for the width the view was last drawn
  at (each split pane breaks at its own). Folds belong to a view (`ViewState::folds`) and map
  through edits like carets (`Buffer::apply_change` for the live view,
  `Parked::catch_up` for the others; a reload drops them). A caret that
  ends up inside a fold unfolds it: `Editor::handle_action` and
  `handle_mouse` end with `Buffer::reveal_carets`, and `view::render`
  reveals first, for jumps made elsewhere (go to definition, F2, find in
  files, bookmarks, back/forward, session restore).
- Split panes use a live slot. `Buffer::view` holds the state of exactly
  one view of the buffer, its live view (`View::parked == None`); every
  other view keeps its state in `View::parked`. `Editor::make_live(group,
  slot)` swaps a parked state in and parks the old one in the tab it came
  from; it runs only from `show` (and so `focus_group`), render (each
  group's shown tab, the focused group last), the wheel over a group
  that is not focused (swap, scroll, swap back) and closing a live tab.
  Between events the focused group's shown tab is therefore live, and
  `active_buffer_mut().view` is the state of what the user sees. Edits
  reach parked views through `Buffer::edit_log` (fed by `apply_change`,
  so undo, redo, reloads, formatting and language server edits are all
  logged): `sync_parked` catches them up at the start of every frame and
  clears the logs, and `unpark` catches up first. Undo and redo put the
  caret back into the live view, the one that undid.
- Code outside the editor touches `b.view` only of `active_buffer_mut()`
  or of a buffer it has just edited (to place the caret after its own
  edit), and never keeps view state across events: for any other buffer
  `b.view` may belong to a view in some other group.
- Each buffer has at least one tab, at most one per group, and exactly
  one live view; the split tree's leaves are the groups; a dirty buffer
  asks only when its last tab closes; the last group is never removed,
  and any other group left without tabs collapses.
- A git2 `Repository` is opened inside the blocking call (`git/repo.rs`) and
  never stored in `App`.
- Network, worktree and history-rewriting operations (merge, rebase,
  cherry-pick, revert, branch and tag commands, `git apply` of hunk
  patches) go through `git/cli.rs`: stdin closed (or carrying the patch),
  no terminal prompt, `GIT_EDITOR=true`, `LC_MESSAGES=C` (git's messages are
  parsed; hooks keep the user's charset), `GIT_SSH_COMMAND` with
  `BatchMode=yes` for network subcommands, each child in its own session
  (`setsid`), 120 s timeout. Reads, staging, discard, commit and amend,
  stash push/list/drop, blame, conflict stages and taking one side of a conflict use git2; stash pop/apply use the
  command line, so a conflict leaves stages for the resolver.
- The conflict resolver replaces the layout while it is open: the groups
  hold only its three scratch buffers, the real groups wait in
  `MergeSession::saved` (caught up by `sync_parked`, cleaned of buffers
  closed meanwhile when put back, and what sessions save). Nothing may add
  a tab meanwhile: layout commands, back/forward and bookmarks are refused
  (`merge_guard`), `App::open_file`, `open_file_at*` and file loads refuse
  with a toast, so go to definition, find in files and the palettes do too.
  Scratch buffers never count as unsaved (`has_dirty`, `save_all`), are
  never reloaded (no path) and leave with the resolver.
- Destructive git operations (discard of files or a hunk, stash drop,
  branch delete, merge, revert, rebase, tag delete) ask in a confirm dialog first;
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
  `Editor::save`/`save_all`, which never format. Save As writes first,
  then (with `on_save`, unsaved edits, and nothing waiting but Save All)
  starts the format job for the new path behind `FileSaved`, so the
  language server already has the new document; its answer writes again
  only if it changed the text.
- Child processes that can outlive a request (formatters, Run tasks, the
  Terminal shell, language servers) start in their own session, so they never read from or
  draw over jetl's terminal, and are killed with what they started (the
  process group; for the shell, every process of its session): on quit
  and after a run loop error (`App::shutdown`),
  on a root switch and when their owner is dropped; a formatter also on
  its timeout or a newer request for the same buffer, a Run task when
  another one starts. Language servers are asked to shut down first (two
  seconds on quit, in the background on a root switch or restart). Clipboard tools are killed after 1 s. Their
  results follow the stale-result rules above.
- Lists keep their wheel scroll: `ListState::set_viewport` and `fit` bound
  the offset without pulling the view back to the selection.
- Panic path (`tui::install_panic_hook`, `recovery.rs`): the terminal is
  restored first, then the dump is written; the hook never blocks (a
  bounded `try_lock` on the snapshot, files written after the lock is
  released, no `unwrap`, no fsync). Each process writes only into its own
  `recovery/<pid>-<secs>/`, a clean exit removes it, and a directory whose
  owner is alive is never offered or removed. Restored text goes through
  `Buffer::apply` (dirty against the file, undoable, never written by
  autosave before an explicit save). Tests never arm the hook: each `App`
  has its own snapshot.

## Carets

A view has one or more carets: `ViewState::sels`, a `Selections`
(`editor/selection.rs`). It is never empty, sorted by start, and never
overlapping (`normalize` merges ranges that overlap, and a bare caret that
touches another caret or range; the merged one keeps the direction of the
member holding the primary). Each caret has its own desired column for
Up/Down. The primary caret is the last one added, as in JetBrains: the view
scrolls to it, the terminal cursor is on it (the others are cells in the
`secondary_caret` theme style), the status bar reports it, and everything
that asks about "the caret" reads it through `ViewState::sel()`: hover, go
to definition, usages, rename, code actions, file structure, bookmarks,
back/forward positions, the find bar's origin and initial query, the
occurrence and bracket highlights, and the session (only the primary caret
is saved). Completion is asked about the primary caret too; accepting makes the
item's edit at every caret whose typed prefix equals the primary's (the
other carets are mapped through the edits) and the additional edits go in
once; it does not open by itself while there are several carets, only on
Ctrl+Space. Accepting a
snippet at a single caret selects its first tabstop instead, its mirrors
(same-number tabstops) as extra carets, so typing replaces them all; an
accept made with several carets leaves the caret at `$0` and walks no
tabstops.

Edit commands come in three shapes:

- **At every caret** (typing with auto-close and closer dedent, Backspace,
  Delete, word deletion, Enter, Tab, toggle case, block comments, duplicate
  line, paste, cut): `Buffer::edit_carets(f)` asks `f` for each caret's
  `CaretEdit` (a range of the current text, its new text, where the caret
  lands: `After::Keep` or offsets in the new text, and the `TxKind`),
  planned against the unchanged rope. Parts that overlap, or insert at the
  same place, collapse into the first one; the rest are applied back to
  front so each `Change.at` stays a position of the text before the edit,
  each part reduced to its smallest change (`minimal_changes`: a wrapped
  selection becomes two inserts). The carets after it are computed walking
  forward with the length change so far. The transaction is `Typing` or
  `Deleting` only when every part is. Commands that are a single change
  per caret write a pure `*_edit(&self, sel, ..) -> CaretEdit`.
- **Over the union of caret lines** (indent, unindent, line comments,
  delete line, join lines, move lines): the selected lines of every caret,
  merged where they touch (`line_blocks`), become one change list, and every
  selection is mapped through it. Line comments make one comment/uncomment
  decision for all lines; moving is refused when any block is at the edge.
- **Whole text** (formatting, workspace edits, line ending conversion, the
  save cleanup, parked views catching up): every caret is mapped through the
  changes; the save cleanup keeps the trailing whitespace of every caret
  line.

Commands that place the caret somewhere (a click, go to line, find
next/previous, replace all, revert or next change, select all, select line,
back/forward, bookmark jumps, a reload) call `ViewState::set_sel` and leave
one caret. A path that hands `apply` a single `Selection` also leaves one.

Undo merges consecutive typing by position only for one caret; with
several carets (or several changes) an edit continues the last transaction
when it starts from the carets that one left (`sel_before == sel_after`).
Undo and redo put every caret back.

Esc closes the find bar first, then drops the other carets (the primary
keeps its selection), then collapses the selection. Copy at several carets
remembers each caret's piece (`Editor::multi_clip`); pasting the same text
at as many carets gives each its piece, text with exactly one line per
caret is split by line, anything else goes to every caret whole. Alt+click
adds a caret (and makes it the primary) or removes the one clicked; the
press starts `DragMode::AltPending`, which becomes `DragMode::Column` once
the pointer reaches another cell (Alt+Shift+drag too).

A column block (`view_state::Block`) is two corners, each a (line, visual
column); the column under the pointer is not clamped to the line, so a
block can reach past short lines. `view::block_selections` makes it a caret
per line on the same `layout_line` walk as rendering: the graphemes between
the two columns (one cut by either edge is inside, so a tab or a wide char
is taken whole), a caret at the end of a line too short to reach the block,
a caret per line for a zero-width block; the
head's line holds the primary. `ViewState::block` remembers the corners so
that extending keeps a column a short line lost; it only counts while the
carets are still the block's selections (`Buffer::current_block` checks by
making them again), else the block is taken from the outer carets. Column
selection mode (`Editor::column_mode`, Alt+Shift+Insert, ` Column` in the
status bar, not saved) turns a plain drag and Shift+arrows, Home and End
into block changes (Shift+click moves the head corner).

Virtual space is the empty cells past a line end where a caret can stand.
`Selection::virt` (`Virt { anchor, head }`, two optional columns) holds it per
caret: `Some(c)` is set only on an end at its line's end, with `c` greater than
the line's width; every reader (`head_col` / `anchor_col`, and the caret and
selection drawing in `render`) takes the larger of `c` and that end's own visual
column, so a stale value never moves anything left of the line end. `head_col` /
`anchor_col` give the visual column of an end (`virt` or the real column,
whichever is larger), and `render`, `ensure_visible`, the terminal cursor and
the status bar's `Col` all read `head_col`. `Selection::new`, `point`,
`set_head`, `with_range`, `Selections::map` and so every motion, edit, click,
drag, find and goto clear it; `clamp`, `collapse_to_primary`, `fork`, `push` and
`edit_carets` carets that land with `After::Keep` keep it; `After::Virtual`,
`block_selections`, `Buffer::virtual_caret` and `Buffer::move_virtual` set it.
When `normalize` merges two empty carets, the primary member's `virt` wins, else
the first one's; a merged range keeps its lead member's only when that range is
the whole merged range. `drop_stale_virt` (run by `edit_carets` on the carets it
keeps) removes the column of an end that is no longer at its line's end; it does
not check that the column is still past the line's width (`Buffer` has no tab
width there), which the readers' clamp makes harmless. Undo and redo restore the
columns recorded in `sel_before` / `sel_after` (the text is identical) but drop
`view.block`; a parked view that catches up on edits loses its columns, like its
block.

`view::block_selections` sets `virt` on the carets it makes at the end of lines
too short for the block, so a block is a row of virtual carets.
`Buffer::virtual_pads` gives each empty caret in virtual space the spaces it
lacks; `handle_action` leaves them in `Buffer::virtual_pad` before `InsertChar`,
`InsertText` and `InsertTab`, and `edit_carets` takes them and prepends them to
that caret's edit, so pad and text are one undo step (`insert_tab` counts the
pad when it picks the tab stop). Backspace, Delete and the word deletions go
through `Buffer::virtual_delete`: a caret that is empty in chars but has virtual
cells changes no text. If its cells have width it goes to their left column;
otherwise Delete and Ctrl+Delete leave it, Backspace moves it one column left and
Ctrl+Backspace moves it to the line end. Every other caret edits as usual, all in
one transaction, and `After::Virtual(col)` keeps an empty caret where it is with
that column. Enter does not pad: the line breaks at its end.

`Buffer::virtual_caret(cell)` makes a caret at a clicked cell past a line end:
Alt+click in any mode (a second Alt+click on it removes it), a plain click in
column selection mode. `Buffer::move_virtual` steps Left, Right, Up and Down
through virtual space in column selection mode; outside it Up/Down from a
virtual caret aim at its column, which seeds the desired column. Soft wrap has
no virtual space: `Buffer::blocks_supported` gates every producer and consumer,
`E::ToggleSoftWrap` clears every caret's `virt` and `view.block`, and `render`
ignores the columns while the rows wrap. Blocks are buffer lines; soft wrap
refuses them in `Buffer::blocks_supported` (toggling column mode shows a notice;
wrapping turns it off) until blocks are made of visual rows.

## Theme loading

`Theme::load(config, config_dir, omarchy, colorterm)` builds the theme in
layers: the built-in named by `ui.theme`, or a theme file layered on
Darcula, or (`auto` on Omarchy) the Omarchy palette rendered into
`assets/themes/omarchy.toml` (`config/omarchy.rs`, then
`Theme::separate_highlights` nudges highlights that came out equal) and the
theme's `jetl.toml`, then `config_dir/theme.toml`, then, without true color, the mapping of every RGB
color to the nearest 256-color index (`color::to_ansi256`; a document's
`[colors256]` table applies right after its own top level in that case). It
returns the theme and the messages for notifications. `App::new` calls it
with `color::colorterm_env()`, which is always true color in tests, and
`Paths::omarchy_dir` (`omarchy::current_dir()`, always `None` in tests), so
snapshots and color assertions do not depend on the environment. The
`Theme` struct is generated by `theme_fields!`, so every field is a theme
key and reached by `Theme::map_colors`. Hot reload
(`app/config_reload.rs`) calls `Theme::load_report` with
`OnBadFile::Reject`, so a document that fails as a whole keeps the current
theme instead of falling back to GitHub Dark; when the theme changed,
`Editor::restyle` gives every buffer's `Syntax` (the conflict resolver's
panes included) its new capture styles, keeping the parse tree, and
`GitPanel::theme_changed` drops the diff panes' syntax spans, which the next
render computes again. For the Omarchy theme, `Theme::sources` lists
Omarchy's `current/theme.name`: `omarchy-theme-set` replaces the
`current/theme/` directory and then rewrites that file, so the watch on the
stable `current/` directory sees every switch.

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
(...))`, and add an arm to `App::finish_dialog` (before the catch-all
`(_, Cancel)`). When something must happen after the answer (and maybe after
further dialogs), carry it in the `DialogId` rather than in `App`, as Save
As does with `AfterSave` (`DialogId::SaveAs { id, then }`): Esc drops it with
the dialog, so no state goes stale, and each step hands it to the next
dialog it opens. An arm that forgets to pass it on ends the chain silently.
A path input opts into Tab completion with `.with_path_completion(base)`.

**A new palette source.** Add a `Source` (label, rows, ranking in
`refilter`), map the `PaletteKind`s that show it in `Palette::new`, feed it
from `App::open_palette` (cached data first, then a refresh whose result
calls a `set_*` on the open palette), and add an `Accept` variant handled in
`App::finish_palette`.

**A language server feature.** Advertise it in
`lsp::client::client_capabilities` (with `dynamicRegistration: true`), add
its method to `lsp::caps::Caps::has` (static capability or an applicable
registration; options it needs from `Caps::dynamic`) and a name in
`lsp_nav::what`, add a `PendingTag` variant carrying what
the answer must still match (buffer id, `Buffer::version`, a seq; a request
about the caret uses `DocTag` and `App::lsp_doc_request`, which also cancels
the method's request in flight; `App::lsp_tagged_request` takes other
params, as `completionItem/resolve` does), send with
`Client::request` once the client is `Running`, and handle the tag in
`App::lsp_msg` (`App::lsp_response` for `DocTag` requests): deserialize the `Value` there, drop it when the buffer
version or seq moved on, convert positions with `lsp::pos`. Edits made at
once (several `TextEdit`s) go through `lsp::edits::plan` into one
transaction; a `WorkspaceEdit` goes through `App::apply_workspace_edit`. Test against
`lsp::fake` through `app::lsp::tests::with_fake_servers`.

**A new config key.** Add the field and its `Default`, document it in
CONFIG.md, and classify it in `config/reload.rs` (the build fails until it
is). A `Live` key is applied in `App::apply_config`
(`app/config_reload.rs`), or in `Editor::apply_config` for `[editor]` keys
(which destructures `EditorConfig`, so it fails to build too); a key read
only at startup is `Restart`. Add a row for the key in `config/catalog.rs`
(the catalog test fails until every key has one).

**A new theme key.** Add it to `theme_fields!` in `config/theme.rs` with a
doc comment, set it in every `assets/themes/*.toml`, the Omarchy template included
(`builtin_themes_set_every_key` and `omarchy_template_sets_every_key` fail
otherwise), and list it in CONFIG.md.

**A built-in theme.** Add `assets/themes/<name>.toml` setting every key,
a `BuiltinTheme` constant in `BUILTIN_THEMES`, and a line in CONFIG.md;
`builtin_themes_keep_highlights_apart` checks that its highlights stay
distinct in both true color and 256 colors (add a `[colors256]` table if
not).

**New background work.** See the stale-results rules above: tag, send an
`AppMsg`, check the tag in `handle_msg`.

**A new destructive operation.** If it overwrites or deletes files the user
may want back, take a local history `Ack` before it starts
(`history::Handle::before_git`, `before_delete`, or a `Capture` queued
first when the old text is at hand, as `capture_disk` and the reloads do)
and wait on it off the main thread before touching the files.

**A new operation that moves files** (a tree move or paste, `git mv`)
calls `App::history_renamed(from, to)` (`history.rename`, then the Local
History window follows) on the main thread once the move succeeded and
before the tabs retarget, so the file's local history follows it.
A move you only learn of afterwards (git renames) goes through
`App::history_moved(pairs)` instead: `Handle::moved` -> `Job::Moved`, where the
worker carries only pairs whose old path is gone and new path exists, and
answers `HistoryMsg::Moved` (`history_retarget` moves the window). `GitService`
records `repo::rename_base` before an operation that touches the work tree
(only with local history on), `repo::renames_since` after it (HEAD tree
renames found by `find_similar`, plus newly staged ones) and sends them in
`GitMsg::OpDone.renames`; `on_git_msg` hands them on before the stale-root
check, since the paths are absolute. `store::carry` skips a second
"Renamed from" marker for the same old path within `RENAME_ECHO` (60 s) when
it copied nothing, so one move seen twice marks once.

**A new local history entry the user owns** (one kept until removed, as a
label) sets `Kind::pinned`; anything that rewrites an index file does it
under `prune.lock` (`drop_lines` / `write_index`).

## Pitfalls

- Mouse capture blocks native selection; Shift+drag passes through in foot,
  alacritty and kitty, so Alt+Shift+drag rarely reaches jetl (Alt+drag
  makes the same block). Some window managers take Alt+click and Alt+drag
  (moving windows); multiple carets and blocks then need the keyboard
  (Alt+J, Clone Caret, column selection mode with Shift+arrows). Copy uses OSC 52 (`set -s set-clipboard on` in tmux)
  plus `wl-copy`/`xclip`/`xsel` when found; Ctrl+V reads the clipboard
  through `wl-paste`/`xclip`/`xsel`, the terminal's paste shortcut through
  bracketed paste.
- Without the kitty keyboard protocol (tmux, most legacy terminals)
  Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter, Shift+Enter and double Shift do
  not arrive; keep legacy fallbacks in the default keymap. There Ctrl+/ and
  Ctrl+7 are the same byte: `App::handle_key` reads Ctrl+7 as Ctrl+/
  (`KeyCombo::legacy_input`) unless `App::keyboard_enhanced`, so a
  `ctrl-7` binding needs the kitty protocol.
- In raw mode Ctrl+C is copy and Ctrl+Z is undo.
- A panic in a background thread (a `spawn_blocking` task) runs the panic
  hook too: the terminal is restored and a recovery dump written while the
  app keeps running. A clean exit removes that dump.
- Take every color from `Theme`; a hard-coded `Color::Rgb` skips themes and
  the 256-color fallback.
- `Event::Resize` sets `needs_clear`; the next frame clears the terminal.
- The Terminal window takes Ctrl+Q, Ctrl+S, Ctrl+W and other global keys
  for the shell; only its `[terminal]` table (Alt+F12, Ctrl+Shift+Up/Down,
  Ctrl+V) and Esc Esc at the shell prompt stay with jetl. Shift keeps the
  mouse for jetl when the program asked for mouse reports; Shift+drag still
  selects natively in most terminals.
- ropey is built with `default-features = false, features = ["simd",
  "cr_lines"]`: lines break at `\n`, `\r\n` (one break) and a lone `\r` only,
  the LSP set, so `lsp::pos` needs no line index of its own. The default
  `unicode_lines` would also break at VT, FF, NEL, U+2028 and U+2029, and
  every line number would then disagree with servers, git and compilers
  (`lsp::pos::tests::ropey_breaks_lines_like_lsp` fails if another crate turns
  it back on through feature unification). The change gutter, blame, the merge
  editor's chunks, find in files, location previews, folds and outline lines
  follow the rope's lines (`util::text::lines_inclusive`,
  `git::diff::rope_hunks`, tree-sitter bytes mapped through the rope). Kept on
  `\n` on purpose: `DiffDoc`, `side_by_side` and the diff views (git's view of
  a file, and their line indexes feed `git apply` patches, which count `\n`),
  Local History revert, `replace_by_lines`, Run problem matchers, highlighting
  (byte ranges) and conflict marker parsing; so for a lone-`\r` file the diff
  view and the gutter can disagree on line numbers. The buffer keeps its line
  ending and writes it back. Stay on ropey 1.x.
- Grammar C code is slow to build cold; iterate with
  `--no-default-features --features lang-rust`.
- Scratch files live outside the root: the file watcher does not see them
  (an outside edit is caught by the mtime check when saving), and
  `lsp_sync` skips them (servers are rooted at the project; a buffer saved
  into the scratches directory leaves its server). Select in Project says
  so instead of "outside the project".
- Local history: the run loop's exit flushes the history queue (3 s at
  most) so the saves of the quit dialog are stored; a snapshot queued after
  that is lost. Several instances share the store: blob writes and index
  appends are safe, but an append from another instance during a prune's
  index rewrite, or during the rewrite of an autosave merge of the same
  file (a few ms), is lost; prune and a merge exclude each other through
  `prune.lock` (a prune finding it held retries once after 50 ms).
  `Entry::since` is optional and left out of the line when unset; an
  older jetl ignores it and drops it when its prune rewrites an index (the
  next autosave then starts a new run). An older dev build cannot parse
  `"kind":"unsaved"` lines: it skips them in lists and drops them when its
  prune rewrites that index (corrupt lines are dropped on rewrite). From this
  build on, prune keeps lines of unknown kinds (forward compatibility): a line
  that is JSON with `v`, `at` and `kind` but no known `Entry` is rewritten
  verbatim in time order and its `blob` is not collected; lists still skip it. Blobs
  replaced by a merge of automatic snapshots are orphans until the next
  prune (at startup, every hour (`PRUNE_EVERY`) or on a retention change), so a long session with a
  short `snapshot_unsaved_secs` can leave the store over `max_total_mb`
  for a while. Keys are canonical paths, resolved on
  the worker (never canonicalize on the main thread); an `Entry`'s `path`
  is that key, so the UI shows the path it asked with, not `Entry::path`.
  The index line format is unchanged (`v` 1): an older jetl reads it and
  finds a file's history when it opens the canonical path; what it then
  appends under a symlinked path is merged by the next prune. An older
  jetl never looks in `blobs-zlib/`: it shows a "No such file" error for
  compressed snapshots, never deletes them, and over-counts the store (raw
  lengths), so it prunes early; raw blobs are never migrated, they age out
  by `keep_days`. Renames: an older dev build cannot parse
  `"kind":"renamed"` lines (skipped, and dropped when its prune rewrites
  that index), and its `Kind::label` ignores a Save As detail (plain
  "Saved As"). Only Project tree renames and those git makes through jetl's
  git operations (checkout, switch, pull, merge, rebase, cherry-pick, revert,
  stash apply/pop; not copies, not `fetch`) are followed: `mv` or `git mv` in
  a shell, and other tools leave the history under the old path; another instance with a tab on the old path
  keeps saving there. A folder rename costs one scan of `index/` on the
  worker; carried copies double index lines, not blob bytes (prune counts
  each blob once, and a copy has its original's time, so both age out
  together). The project view's first scan reads every index file (as the
startup prune does); later ones stat them and re-read only the changed
ones, plus a stat per file under the folder; outside deletions show on the
next scan (F5), the file watcher does not trigger one. Labels: an older dev build cannot parse `"kind":"label"`
  lines (skipped), and its startup prune rewrites any index holding one,
  deleting the labels. Labels are per file, unlike JetBrains' project-wide
  labels. A save of the labelled text right after adds no row (it is the
  last entry's bytes, as after a Renamed marker). Labels can keep the store
  at its cap: they are dropped only when they alone exceed it.
  `test_app` has `Handle::disabled()` (a worker could write into the temp
  dir after the test removed it), after flushing `App::new`'s worker so its
  startup prune cannot hold `prune.lock` while a later worker replaces an
  entry; `with_history` turns it on in the
  test's temp state dir, and `app::history::tests::snapshots` flushes first
  (and fails when the queue did not drain). Tests wait for the window's
  newest diff with `diff_landed`, never for any `HistoryMsg::Diff`: an older
  request's answer may land first and is dropped by its seq.
  For the same reason `App::new` starts no background work that writes:
  the recent projects file is recorded only after `main.rs` calls
  `App::start_recent_projects`, as crash dumps are pruned only by
  `App::prune_recovery_dumps`.
- `git2` is built without default features, linking the system libgit2
  without OpenSSL or libssh2; anything that talks to a remote uses the CLI.
