# Configuration

jetl reads three optional files from its config directory,
`$XDG_CONFIG_HOME/jetl/` (usually `~/.config/jetl/`):

| File | Purpose |
|---|---|
| `config.toml` | Editor, UI, git, worktree, keymap mode and file tree settings |
| `keymap.toml` | Key bindings merged on top of the built-in keymap |
| `theme.toml` | Color overrides on top of the theme chosen by `ui.theme` |

jetl watches these files while it runs and applies a change when a file is
saved (after a 150 ms pause, like the project watcher; see
[Reloading](#reloading)). If the watch cannot start (for example the
system's inotify limit is reached), a warning says so once and changes apply
at the next start. A missing file means the built-in defaults. If a
file cannot be read or is not valid TOML, jetl logs the error and shows a
notification. At startup a file that cannot be read or is not valid TOML is
replaced by the defaults; when a running jetl reloads it, the current
settings stay instead and a notification names the error. In `keymap.toml`
and `theme.toml`, a single bad entry (unknown action, bad key, bad color) is
skipped and reported; the other entries still apply. In `config.toml`, a
value of the wrong type rejects the whole file, because settings depend on
each other.

The log is at `$XDG_STATE_HOME/jetl/jetl.log` (usually
`~/.local/state/jetl/jetl.log`). Sessions (per project root: open tabs, tool
window layout, expanded Project tree folders and its hidden/ignored toggles; the open tabs, splits and
active tab are kept per git branch: switching branches closes the clean tabs and opens the other
branch's saved ones, dirty tabs stay, a branch not used before keeps the current tabs)
live in `$XDG_STATE_HOME/jetl/sessions/`; recently opened project roots (the
Recent Projects palette, Alt+Shift+E; Shift+Delete forgets one) in `$XDG_STATE_HOME/jetl/recent_projects.json`;
scratch files (File > New Scratch File…, shared by every project) in
`$XDG_STATE_HOME/jetl/scratches/`, created with the first one; local history
snapshots (File > Local History…, Alt+Shift+H; shared by every project and jetl
instance) in `$XDG_STATE_HOME/jetl/history/`, created with the first one.

## Reloading

Saving `config.toml`, `keymap.toml`, `theme.toml` or the theme file named by
`ui.theme` applies it at once, and the status bar says "Reloaded …". With
the Omarchy theme (`ui.theme = "auto"` on Omarchy, see Themes) a switch of
the desktop theme restyles jetl too. Each
file is reloaded on its own: a broken `keymap.toml` keeps the current key
bindings while a valid `config.toml` saved with it still applies. A file
whose text did not change is ignored.

Most settings take effect immediately. These apply to files opened
afterwards, while open tabs keep their own state: `editor.soft_wrap`,
`editor.soft_wrap_files`, `editor.tab_width`, `editor.max_highlight_bytes`.
`terminal.shell`, `terminal.args` and `terminal.scrollback` apply to the
next shell. `debug.adapters` applies when the next debug session starts. `lsp.servers`, `lsp.startup_timeout_ms` and
`lsp.request_timeout_ms` apply after Help > Restart Language Servers;
changing `lsp.enabled` stops or starts the servers. `ui.left_width` and
`ui.bottom_height` are initial sizes and need a restart of jetl. The reload
notification lists any changed setting that needs a restart.
`files.show_hidden` and `files.show_ignored` apply to Go to File and Find in
Files; the Project tree keeps its own toggles. Lowering `editor.max_tabs`
closes nothing until the next tab opens. An open palette or menu shows the
new shortcuts the next time it opens.

jetl creates the config directory at startup if it is missing, so that it
can be watched. If the configuration files are symlinks, their targets are
watched too. A theme file outside the config directory is watched in its
own directory, which must exist.

### Settings window

Ctrl+Alt+S or File > Settings… opens the Settings window (in the Git window
Ctrl+Alt+S stashes, so use the menu there). A page tree on the left
(Appearance, Editor with General, Soft Wraps, Saving and Formatting,
Keymap, Version Control, Files, Tools, Language Servers) and the page's rows on the right, each with its value and a tag for when a
change applies; the selected row's help line ends with its dotted key (`Key: ui.left_width`). A row with no tag applies at once. The tags are:

- `new tabs`: applies to files opened afterwards;
- `next shell`: applies when the Terminal window starts its next shell;
- `restart servers`: applies after Help > Restart Language Servers;
- `restart jetl`: needs a restart of jetl; Apply names such keys in a warning.

Keys: Up/Down move in the tree or the rows, Tab and Enter/Right on a page
switch between them, Left goes back to the tree; Enter or Space
changes the row (flips a Bool, opens a value dialog for a number or text,
cycles or picks a choice); Left/Right step a choice and turn a checkbox off/on; Delete resets
the row to its default (removes the key from the file). A changed row shows
`*` and its new value in the modified colour until it is applied. Ctrl+S or
the Apply button writes the pending changes to `config.toml` and applies
them through the same reload as a saved file; OK applies and closes; Cancel
and Esc close, and with pending changes ask Discard / Keep Editing before
dropping them. The Apply button is dim with nothing pending. A mouse click
selects a tree row or a row, a click on a Bool value flips it, and a double
click acts as Enter.

Edits are made in the text of the file: comments, key order and blank lines
stay, and a missing section is appended. If the result would not be a valid
configuration, the change is refused with a message and nothing is written.
A symlinked `config.toml` stays a symlink; the target is written. While
changes are pending, rows that open another file are refused.

Lists are edited as comma-separated text, so a value that contains a comma
must be edited in the file. Tables (`[format]` commands, `[[run.tasks]]`,
`[lsp.servers.*]`, `[debug.adapters.*]`, `[run.debug.*]`) and `keymap.toml` and `theme.toml` are edited in the
files; their rows only say so, and Enter on one opens `config.toml`. "Open settings file
(config.toml)" in the action palette still opens the file itself.

## config.toml

Every key is optional; omitted keys keep their defaults. Unknown keys are
ignored. The full default file:

```toml
[editor]
tab_width = 4                  # columns per tab stop and indent level when neither .editorconfig nor the file decides (see below)
max_highlight_bytes = 2097152  # files above this size (2 MiB) open without syntax highlighting or gutter marks
scrolloff = 3                  # lines kept visible above and below the caret
hscrolloff = 5                 # columns kept visible left and right of the caret
auto_close = true              # type brackets and quotes in pairs, step over closers, wrap selections
highlight_occurrences = true   # highlight the word at the caret (or the selected text) elsewhere on screen
indent_guides = true           # a │ at each indent stop in a line's leading whitespace (theme style indent_guide)
inline_blame = false           # author, date and summary of the caret line's commit after its text (theme style inline_blame)
breadcrumbs = false            # a row at the bottom of each editor pane naming the definitions around the caret, e.g. "impl Point › fn new"; click one to go there (theme style breadcrumbs)
sticky_scroll = false          # pin the header lines of the definitions scrolled past at the top of each pane; click one to go there (theme style sticky_scroll)
snippets = true                # live templates: Tab after a snippet's prefix expands it, snippets listed in completion (see "Live templates")
trim_trailing_whitespace = true # on save, strip trailing spaces and tabs (not on the caret line, never in Markdown)
final_newline = true           # on save, end a non-empty file with a line break
autosave = "focus"             # "off", "focus" (save when the terminal loses focus) or "idle" (save after a pause in editing)
autosave_idle_ms = 1500        # pause before an "idle" autosave
max_tabs = 0                   # open tabs beyond this close the least recently used one that is clean
                               # and unpinned (none such: the limit is exceeded); 0 = no limit
soft_wrap = false              # soft-wrap every file when opened
soft_wrap_files = ["*.md", "*.txt", "*.rst", "*.adoc"]  # files soft-wrapped when opened anyway: "*.ext" matches
                               # names ending in ".ext", anything else a whole file name
soft_wrap_extra_indent = 0     # extra cells continuation rows of a wrapped line are shifted by, after the line's own indent (at most half the width together)

[ui]
theme = "auto"                 # "auto" (the Omarchy theme's palette when there is one, else
                               # "github-dark"), "omarchy", "github-dark", "darcula", "light",
                               # or a theme file: a path ending in .toml (relative to this
                               # directory; "~/" expands), layered on Darcula
left_width = 32                # initial width of the left tool window (Project, Worktrees)
bottom_height = 14             # initial height of the bottom tool window (Git)
double_click_ms = 400          # double click and Shift Shift interval
icons = "unicode"              # tool strip and menu icons: "unicode" (one-cell symbols) or "emoji" (needs a color emoji font)
file_icons = "nerd"            # file-type icons before file names (project tree, tabs, file palettes, Find, Problems, TODO, Git lists): "nerd" (Nerd Font glyphs, needs a Nerd Font), "ascii" (one character per file kind) or "none"
colors = "auto"                # "auto" (true color when $COLORTERM is truecolor or 24bit, else
                               # the 256-color palette), "truecolor" or "256"
toolbar = true                 # the top row with the main menu button; hidden in zen mode and in terminals under 12 rows

[git]
pull_mode = "ff-only"          # "ff-only" (git pull --ff-only), "rebase" (--rebase) or "merge" (--no-rebase)
log_all = true                 # Log tab shows all branches (branches, remote branches, tags and HEAD); false: HEAD only
log_page = 500                 # commits loaded per Log page

[worktree]
default_dir = "{parent}/{repo}.{branch}"  # suggested path for new worktrees; {parent} is the main
                                          # worktree's parent dir, {repo} its name, {branch} the
                                          # branch with "/" replaced by "-"; "~/" expands
switch_closes_tabs = true      # switching worktrees closes the tabs of the old root

[keymap]
mode = "default"               # "vim" turns on the [mode.*] keymap tables (reserved, minimal)

[files]
show_hidden = true             # show dot files in the Project tree and Go to File; the tree's
                               # toggle, saved in the session, wins once a root has one
show_ignored = false           # show files ignored by .gitignore in the Project tree and Go to File
                               # (same session rule for the tree)
watch = true                   # follow changes made outside jetl (tree, git state, open files); also feeds
                               # language servers' file change events

[clipboard]
paste_command = ""             # prints the system clipboard; "" = detect, "none" = jetl's own last copy only
copy_command = ""              # takes copied text on stdin, besides OSC 52; "" = the detected tool's
                               # partner, "none" = OSC 52 only

[format]
on_save = false                # Ctrl+S, Save All and Save As run the file's formatter before writing
timeout_ms = 5000              # a formatter still running after this is killed
# One entry per language: the command and its arguments, as a list. Keys are
# language ids (the status bar shows them): rust, python, javascript, typescript,
# tsx, csharp, go, json, ... None are set by default. Examples:
# rust = ["rustfmt", "--edition", "2021", "--emit", "stdout"]
# python = ["black", "-q", "-"]
# javascript = ["prettier", "--stdin-filepath", "{path}"]
# typescript = ["prettier", "--stdin-filepath", "{path}"]
# csharp = ["csharpier", "format", "--write-stdout"]
# go = ["gofmt"]

# Run configurations (Alt+Shift+F10 picks one). None by default; examples:
# [[run.configs]]
# name = "build"                 # shown in the picker and the Run window
# command = "cargo build 2>&1"   # a string runs through `sh -c`
# [[run.configs]]
# name = "web tests"
# command = ["npm", "test", "--"]  # a list of words runs directly, no shell
# args = ["--watch=false"]       # appended to the command
# cwd = "web"                    # relative to the project root; default the root
# env = { CI = "1" }             # added to jetl's environment
# matcher = "problems"           # "problems" (links), "tests" (test results), "none"; unset: tests for cargo test / pytest / vitest / jest

# How a run configuration is debugged, by its name. .jetl/run.toml holds the
# same tables as [debug.<name>], which win. None by default; example:
# [run.debug.demo]
# adapter = "gdb"                # unset: the first installed for the language
# language = "c"                 # unset: the active buffer's language
# launch = { program = "${workspaceFolder}/demo", args = ["x"] }  # passed to the adapter's launch request

# Built-in debug adapters take changes here; a table with a new name adds
# one. Omitted keys keep the built-in values.
# [debug.adapters.lldb-dap]
# command = ["lldb-dap"]         # the command and its arguments; [] turns the adapter off
# languages = ["rust", "c", "cpp"]  # language ids, as the status bar shows them
# launch = { console = "internalConsole" }  # replaces the built-in launch arguments whole

[terminal]
shell = ""                     # the Terminal window's shell; "" = $SHELL, else /bin/sh
args = []                      # its arguments, e.g. ["-l"] for a login shell
scrollback = 5000              # lines kept above the screen for the mouse wheel

[lsp]
enabled = true                 # start language servers for open files
startup_timeout_ms = 15000     # a server that has not initialized by then is killed
request_timeout_ms = 5000      # a request to a server unanswered by then fails
max_diagnostics_per_file = 1000  # problems kept per file, the most severe first
completion_auto = true         # open the completion popup while typing (else Ctrl+Space only)
completion_min_chars = 1       # identifier chars typed before it opens by itself
inlay_hints = true             # show the server's inlay hints (types, parameter names)
semantic_tokens = true         # colour code by the server's semantic tokens on top of tree-sitter
rename_preview = true          # review a server's rename in the Find window first (else it applies at once)
scratches = false              # language servers for scratch files too (own clients)
# Built-in servers (table below) take changes here; a table with a new
# name adds a server. Omitted keys keep the built-in values.
# [lsp.servers.rust-analyzer]
# command = ["rust-analyzer"]    # the command and its arguments; [] turns the server off
# languages = ["rust"]           # language ids, as the status bar shows them
# init_options = {}              # sent as initializationOptions
# settings = { rust-analyzer = { check = { command = "clippy" } } }  # answers workspace/configuration
# init_options = { workspace = { symbol = { search = { kind = "all_symbols" } } } }  # rust-analyzer lists only types in Go to Symbol by default

[local_history]
enabled = true                 # snapshot files on save and before they are overwritten or deleted
keep_days = 5                  # snapshots older than this are pruned (0 drops every one at the next prune; labels are kept)
max_total_mb = 100             # the oldest snapshots are pruned while the store holds more (compressed size on disk; labels go last)
max_file_kb = 2048             # larger files are not snapshotted
snapshot_unsaved_secs = 0      # every this many seconds, snapshot unsaved edits of open files (0: off; at least 10; 60 is a good value)

[index]
scip = ""                      # SCIP index file relative to the root (e.g. "index.scip"); "" = none
scip_command = ""              # shell command that writes the index (run.build_scip_index); "" = none
```

SCIP index (`[index] scip`): build one with `rust-analyzer scip . --output
index.scip` (other indexers: scip-typescript, scip-python, scip-java,
scip-clang, scip-go) and set the key to its path under the project root.
Without a language server, Ctrl+B and Alt+F7 then use its exact definitions
and usages in files not changed since it was written (saved, and not newer
than the index file); other files fall back to the unverified symbol-index
candidates. The index is read again when the key changes, at start, after a
project switch, and on the next lookup after the file changes on disk. A file
that cannot be read or is not a SCIP index gives one warning and no index.
So does a file above 256 MiB, which is refused unread: loading builds the whole
index in memory first, about 7 times the file size at its peak.
`[index] scip_command` is a shell command (for example `rust-analyzer scip .
--output index.scip`); Run > Build SCIP Index (`run.build_scip_index`) runs
it in the project root as a Run task named `Build SCIP index`, with its output
in the Run window, and loads the index again when it exits 0.

With `watch` on, files changed by other programs refresh the Project tree
and the Git window, and an open tab reloads in place when it has no unsaved
edits. A tab with unsaved edits asks: Overwrite (write yours now), Reload
(take the disk version; undoable) or Cancel (decide when you save). Changes
under `.git` (commits, checkouts, staging from a shell) refresh the Git
window. If the watch cannot start (for example the inotify watch limit,
`fs.inotify.max_user_watches`), jetl warns and falls back to refreshing when
the terminal regains focus.

Autosave writes every modified tab that has a file, silently (errors are
still shown), and applies `trim_trailing_whitespace` and `final_newline` like
Ctrl+S. It skips untitled tabs, files that were not valid UTF-8 (saving would
re-encode them), and files changed on disk since they were opened or last
saved; Ctrl+S still saves those. After a failed write it waits for the next
edit before retrying. `"focus"` needs focus reporting from the terminal; under
tmux that means `set -g focus-events on`. The default is "focus"; set "off" to
save only with Ctrl+S.

Paste (Ctrl+V, `editor.paste`) reads the system clipboard with a command,
because terminals hand the clipboard over only on their own paste shortcut
(Ctrl+Shift+V or Shift+Insert, which also works). With `paste_command` empty
jetl looks on `PATH` once: `wl-paste --no-newline` when `WAYLAND_DISPLAY` is
set, else `xclip -selection clipboard -o` or `xsel --clipboard --output`
when `DISPLAY` is set, `pbpaste` on macOS. Commands are split on whitespace
(no quoting) and killed after 1 s. If none is found or it fails, jetl warns
once and pastes its own last copy. The text goes to the field that had focus
when Ctrl+V was pressed; if focus moves before the command answers, it is
dropped. Copies always go out through OSC 52, and also to the detected tool's
partner (`wl-copy`, `xclip -i`, `xsel --input`, `pbcopy`), so a terminal
without OSC 52 still leaves the clipboard and paste in step. That tool keeps
a process running to serve the clipboard until something else is copied, as
it does from a shell. A configured `paste_command` gets no copy tool unless
`copy_command` names one.

Formatting (`[format]`) pipes the whole file through an external command:
the text goes in on stdin and the formatted text must come out on stdout,
so a formatter that only rewrites files in place (`dotnet format`) does not
fit; use one with a stdin mode (CSharpier for C#). `{path}` in an argument
becomes the file's absolute path, for tools that pick their settings or
parser from it (prettier's `--stdin-filepath`). The command runs in the
project root, without a terminal, and is killed with everything it started
after `timeout_ms`. Ctrl+Alt+L (`editor.format`, Reformat Code) formats the
current file without saving, whether or not `on_save` is set.

A `[format]` command for the file's language always wins. Without one, the
file's language server formats it when it can (`textDocument/formatting`,
with the file's indentation: its width as the tab size, spaces or tabs), for
Ctrl+Alt+L and `on_save` alike; `timeout_ms` does not apply, the server gets
`[lsp] request_timeout_ms` and a save writes the file unformatted after that
(or when the server stops). A file with neither is saved as it is, and
Ctrl+Alt+L says why there is no formatter.

With a selection, Ctrl+Alt+L asks the language server to format only the
selected text (`textDocument/rangeFormatting`) when the server can; with
several selections (a block selection too) it asks for the span from the start
of the first to the end of the last, which may start and end mid-line; many
servers widen a range to whole lines or statements. A server without range formatting formats the
whole file. `[format]` commands and saves always format the whole file.

The result is applied as one undoable edit of the changed lines only: Ctrl+Z
takes the whole reformat back, the caret stays on the same code and the view
on the same lines. Output identical to the buffer changes nothing. With
`on_save`, Ctrl+S first applies the save cleanup (`trim_trailing_whitespace`,
`final_newline`), then waits for the formatter and writes its text. If the
formatter fails or times out, the file is saved unformatted and a warning
shows its first error line (the full output is in the Event Log, Alt+Shift+L).
If you type while it runs, the reformat is dropped and the file gets the
text as it was when you pressed Ctrl+S; your later edits stay unsaved.

Only explicit saves format: Ctrl+S, Save All and Save As (Alt+Shift+S) of a
file with unsaved changes. Save As writes the file first, then formats it
with the formatter of the new path's language (`{path}` is the new path;
`notes.txt` saved as `notes.rs` gets the Rust formatter, or rust-analyzer
once it runs) and saves the result again; a failure or timeout leaves the
file as Save As wrote it, with the warning. Autosave, a Save As of a file
without unsaved changes, and the saves done by closing a tab, quitting or
switching projects with unsaved changes ("Save all and exit", including the
Save As prompts they bring up) write at once without the formatter, so they
never wait on it.

Run configurations (`[[run.configs]]`) are listed by `run.pick`
(Alt+Shift+F10), then those of the project's own `.jetl/run.toml`, which
holds the same entries as `[[configs]]` tables. The older `[[run.tasks]]` and
`[[tasks]]` are still read and listed before the configurations of the same
file. A change to `.jetl/run.toml` or to the `[run]` section of config.toml
is picked up at once (Live), and nothing runs by itself. When there are none,
the picker offers defaults found in the project root: `cargo build`,
`cargo test` and `cargo run` for a `Cargo.toml`, `npm run <script>` for each
of the first 20 `package.json` scripts, `make` for a Makefile, and
`go build ./...` and `go test ./...` for a `go.mod`. A configuration runs only
when you pick it (or rerun it), so a project file cannot start anything by
itself.

Each entry has `name`, `command` (a string for `sh -c`, or a list of words),
and optionally `args`, `cwd`, `env` and `matcher`. `args` are appended to the
command's words; to a string command they are passed as `"$@"`, so they
reach the shell command unsplit. `matcher` is `"problems"` (file locations
in the output become links), `"tests"` (test results are parsed from
`cargo test`, `pytest -v`, `vitest --reporter=tap-flat` and `jest --verbose` output, with the
failures' file locations linked too) or `"none"` (plain output). Unset, it is
`"tests"` for a command that runs `cargo test`, `pytest` (or `python -m
pytest`), `vitest` or `jest`, else `"problems"`. For a command given as a list of
words and read as tests, jetl adds `-v` to a pytest command,
`--reporter=tap-flat` to a vitest command and `--verbose` to a jest command
when they are missing (the Run
window header shows the command as started); a string command is never
rewritten, so give it those flags yourself. (For a list command jetl also
drops pytest's `-q`, which would cancel the `-v`.) Jest without `--verbose` (a
string command) still lists its failed tests, but not the passed ones.

Debug adapters (`[debug.adapters.<name>]`) are the programs jetl talks the
Debug Adapter Protocol to, over stdin and stdout. These are built in, and the
first one found on `PATH` is the default for a language:

| Adapter | Command | Languages | Launch arguments |
|---|---|---|---|
| `codelldb` | `codelldb` | rust, c, cpp | `terminal = "console"`, `sourceLanguages = ["${language}"]` |
| `lldb-dap` | `lldb-dap` | rust, c, cpp | `console = "internalConsole"` |
| `gdb` | `gdb -q -i=dap` | c, cpp, rust | none |
| `debugpy` | `python3 -P -m debugpy.adapter` | python | `console = "internalConsole"`, `subProcess = false` |

Only `gdb` has been tried here; the launch arguments of the other three come
from reading their sources. `jetl --doctor` and Settings > Dependencies show
which are found; debugpy's row checks `python3` only, since checking the
module would run a process.

A table with a new name comes before the built-in ones. `command = []` turns
an adapter off. `launch` replaces the built-in launch arguments whole, and
`${language}` in it becomes the language in use. Delve and js-debug need a
socket and are not supported.

A debug block says how a run configuration is debugged. It is
`[debug.<name>]` in `.jetl/run.toml` or `[run.debug.<name>]` in config.toml,
named after the configuration (`[debug."web tests"]` for a name with a
space); a block of the same name in the project file replaces the other
whole. `[debug.<name>]` is the form for `.jetl/run.toml`, `[run.debug.<name>]`
for `config.toml`; the other form in either file is ignored with no warning. Renaming a configuration in Edit Configurations does not rename its
block. `adapter` names an adapter; unset, it is the first found for
`language`, which defaults to the active buffer's language. `launch` is a
table passed to the adapter's `launch` request as it stands, so what it can
hold depends on the adapter (`program` and `args` for all four built-in
ones).

The launch arguments are built in this order, each replacing the same
top-level key of the one before: the adapter's `launch`; `cwd` (the
configuration's `cwd`, with the variables below replaced, under the project root
unless that makes it absolute, else the root); `env` (the
configuration's `env`; for gdb also jetl's environment beneath it, since gdb
takes `env` as the exact environment and a user-defined adapter does not get
this); the block's `launch`. So a block's `env` replaces the whole of it, and
with gdb it is the debuggee's whole environment: `env = { A = "1" }` drops
`PATH` and `HOME`. Then `${workspaceFolder}`, `${file}` and `${artifact}` are replaced in every
string value, not in keys; one with no value stays as written, and the adapter
reports it. Values go in as they are, unquoted, so a file name with odd
characters reaches a debugger command as written when a block puts `${file}`
in one. The configuration's `command` and `args` are not used under Debug: the adapter
starts the program named in `launch`.

**A project's `.jetl/run.toml` can run commands through Debug.** `launch`
can carry debugger commands (`initCommands` in lldb-dap and codelldb, for
example), and the file chooses the program and the adapter by name, so
pressing Debug in a project can run anything that file says, exactly as
pressing Run can today. There is no separate confirmation. Nothing starts
until you press it, so read the `.jetl/run.toml` of a project you do not
trust first. A project file cannot define or change an adapter's command;
`[debug.adapters.*]` is read from your config.toml only. A relative `command`
in your own `config.toml` resolves under the project root, so a project can
supply that program. The built-in debugpy adapter needs Python 3.11: it runs
with `-P` to keep the project's files (a `debugpy/` directory in it, say) off
its import path, and an older Python fails with an error the session reports.

A debug table of the wrong shape does not affect Run or Edit Configurations,
which do not read the debug tables (a TOML syntax error anywhere in the file
stops them all); a bad `[run.debug.<name>]` in `config.toml` invalidates
that file like any other bad key.

Run > Debug (Shift+F9) starts the configuration selected in the Run window, else
the first one with a debug table, under its adapter, in the project root; the
program is the one named in `launch`. Only one session runs at a time: starting
another stops the first, with no prompt. Ctrl+F2 or Run > Stop Debugging stops it,
and Ctrl+F2 reaches a Run task only while no session is live (under a dialog or the palette it still stops the Run task, not the session). Debug refuses a
working directory whose name holds a line break, because gdb runs `cd` with it
unquoted and would run the following lines as commands. Renaming a configuration
leaves its table behind; the toast names the table to write.

With gdb, a `program` or `cwd` that does not exist gives no error: gdb answers `launch`
with success and logs the failure only, so the session stays idle with `Debugging <name> with gdb`
as its only sign, and Ctrl+F2 ends it.

Things to know about arguments and test runs: with `command = ["sh", "-c",
script]` the first of `args` becomes `$0` of the script, not `$1`. A compound
string such as `cd web && cargo test` passes `args` to its last command only.
Rerun Failed adds its words to `args`, after yours: cargo flags such as
`--release` go before a `--`, libtest flags after it. And `cargo test` stops
at the first failing test binary, so the tree lacks the later ones; add
`--no-fail-fast` to see them all.

Run > Edit Configurations… (also the Run window's toolbar button and the
action `run.edit_configs`) opens the Run Configurations window: the list of
what the picker offers (config.toml, project and detected ones, each with its
origin) on the left, the selected one's fields (Name, Command, Arguments,
Working directory, Environment, Output) on the right. Enter or a double click
on a field asks for its value (Arguments and Environment as shell words,
`KEY=value` for Environment; Output is a choice). Insert or `[+]` adds a
configuration, Delete or `[−]` removes a project one, Ctrl+D or `[Copy]`
copies any configuration into a new project one, Ctrl+S or Apply saves, OK
saves and closes, Esc asks before dropping changes. Saving writes only
`.jetl/run.toml` (created with its folder when missing): comments and the
entries you did not change are kept (a comment directly above a configuration
goes with it when that configuration is removed, except above the file's first
table, where it stays as the file's header), and nothing runs, neither
on opening nor on saving. Entries from config.toml and the detected defaults are read-only:
copy one to change it in the project. A file that changed on disk since the
window loaded it is not overwritten; reopen the window. An invalid
`.jetl/run.toml` is shown as an error and makes the window read-only.

A task runs without a terminal: stdin is closed and stdout and stderr share
one pipe, so their lines interleave as written, but programs that color or
animate only on a terminal print plain output (escape codes that still come
through are stripped). The Run window (Alt+4) has a toolbar on its first row:
the configuration picker (click it for the Alt+Shift+F10 picker), ▶ Run,
↻ Rerun, ■ Stop (dim and inert when nothing ran or runs), Rerun Failed (only
for runs read as tests) and Edit Configurations… (a narrow window drops the
buttons that do not fit, never the picker). Below it come the
status, elapsed time and the last 10 000 lines, following new output unless
you scroll up.
File locations in the output become links (rustc and cargo, tsc, pytest and
Python tracebacks, and any `path:line:col`), resolved against the task's
directory and its parents up to the project root; only files that exist are
linked. Errors are red, warnings yellow. A click on a link, Enter or a double
click opens it; F4 and Shift+F4 (Ctrl+Alt+Down/Up in the Run window) step
through them from anywhere. Every pick selects its configuration (the toolbar
shows it, and the session keeps it per project). Shift+F10 runs the selected
configuration (the picker when none is selected or it no longer exists; in
the Project, Git and Local History windows Shift+F10 opens the context menu
instead), Ctrl+F5 runs the last run again, and Ctrl+F2 stops it. Restoring
the session only selects; nothing runs until you do. GNOME Terminal keeps
Shift+F10 for itself: use the Run menu there.

A run read as tests (matcher `tests`) opens the Run window in the Tests view:
the status line adds `Tests: N passed, N failed, N ignored`, and the list is a
tree of suites (the libtest binary, the pytest, vitest or jest file), groups (the
module path, classes or describe blocks) and tests, sorted by group and name,
each with `✓` (passed), `✗` (failed) or `○` (ignored). A failed test is
expanded with its output below it, the location of the assert underlined as a
link. Enter or a double click on a test opens its failure location, on an
output line that line's link; F4 and Shift+F4 step through the failed tests.
Right and Left expand and collapse the selected suite, group or failed test
(a collapse stays while the run goes on). The ` Output ` and ` Tests ` tabs on
the right of the toolbar, or `t`, switch to the raw output and back; the tabs
are the first thing a narrow window drops. A Tests run that ends with no
parsed test (a compile error) switches to the Output view by itself. Rerun
Failed (the toolbar button, Run > Rerun Failed Tests, the action
`run.rerun_failed`) runs the same command for the failed tests only: `cargo
test -- --exact <test names>`, pytest with the failed node ids, vitest with
the failed files and `-t` over their names, jest with the failed files and `-t`
over their names (a file that failed to load reruns as a whole; the other tests of those files
show as ignored, which is jest's own reporting). A `-t` already in the command or in `args` is replaced; a string command is split on whitespace for this, so one holding quotes, a backslash, a line break or a shell operator (`;`, `&`, `|`, `<`, `>`, a backtick, `$(`) keeps its `-t`. Not shown: per-test timing, a
running state per test, filters; results are not saved.

Run Test at Caret (Ctrl+Shift+F10, Run > Run Test at Caret, the action
`run.test_at_caret`) runs the test around the caret of the active editor
through the Run window, in a task named `Test <path>` that does not change
the selected configuration. Rust: the innermost `#[test]` function (also
`#[tokio::test]`, and with the caret on its attribute line), else the
innermost inline `mod`; `cargo test <module path>::<fn> -- --exact` (a module:
`cargo test <module path>::`) in the directory of the nearest `Cargo.toml`;
`tests/x.rs` adds `--test x`, `src/bin/x.rs` adds `--bin x`. Python: the
innermost `test*` function with its classes, else the innermost `Test*`
class; `pytest -v <file>::<Class>::<test>` in the project root. JavaScript,
TypeScript and TSX: the enclosing `it(...)` / `test(...)` (also `.only`,
`.skip`, `.concurrent`) with a plain string title and the titles of its
`describe(...)` calls; `npx vitest run <file> -t <pattern>` when the nearest
`package.json` lists vitest (the tests view), else `npx jest <file> -t
<pattern>` for jest (the tests view too), else a note. Anything else, a
file without a syntax tree, an unsaved new file or a caret outside any test
gives an info note; a file with unsaved changes runs from disk and says so.
Ctrl+Shift+F10 reaches jetl in xterm-compatible terminals and tmux without the
kitty protocol; the Run menu and F1 work everywhere.

Send HTTP Request at Caret (Ctrl+Alt+R, Run > Send HTTP Request at Caret, the action `run.http_request_at_caret`) sends the request around the caret of a `.http` or `.rest` file with `curl -q -sS -i --max-time 30` and shows the response in the Run window, in a task named after the request (`# @name`, else the text after `###`, else the method and URL). Requests are separated by lines starting `###`; a request is a method and URL line (a bare URL means GET), header lines, a blank line and the body; `#` and `//` lines are comments and response-handler lines (`> ...`) are ignored. `{{name}}` is replaced from the `dev` object of `http-client.env.json` in the file's directory, else in the project root. The text is sent as typed, saved or not. curl must be in `PATH`; curl's own config file (`~/.curlrc`) is not read; there is no timeout setting.

One task runs at a time: starting another kills the running one first, as
do quitting jetl and switching projects. A stop sends SIGKILL to the task's
whole process group, so the processes it started (cargo's rustc, npm's node)
go with it; programs that put themselves in a new session (daemons) are not
tracked. A task that fails (a non-zero exit code, killed by a signal jetl
did not send, or a command that cannot start) gives one error notification.

The Terminal window (Alt+F12) runs the shell on a pseudo terminal in the
project root with `TERM=xterm-256color`, started the first time the window
is shown. With true color (see `ui.colors`) the shell also gets
`COLORTERM=truecolor`; without it `COLORTERM` is removed and RGB colors that
programs still send are drawn as their nearest 256-color palette entries. A
change of `ui.colors` applies to the window's colors at once and to the
environment of the next shell; a running shell keeps its `COLORTERM`. Full-screen programs (vim, less, htop)
work, at the window's size, which follows Ctrl+Shift+Up/Down and the
terminal's own resizes. While it has focus every key goes to the shell,
global shortcuts such as Ctrl+Q and Ctrl+S included, except the keys in the
`[terminal]` table of `keymap.toml` (by default Alt+F12 to hide it,
Alt+Shift+M to maximize it, Ctrl+Shift+Up/Down to resize it and Ctrl+V to paste the system clipboard)
and Esc Esc at the shell prompt, which focuses the editor. There a lone Esc
reaches the shell after the double click interval or with the next key;
full-screen programs get every Esc at once and never lose focus to it. The
terminal's own paste shortcut pastes too, bracketed when the program asked
for it. The mouse wheel scrolls back through `scrollback` lines (any key
returns to the bottom); in less or man it scrolls the page, and programs
that ask for the mouse (vim with `mouse=a`, htop) get clicks, drags and the
wheel, unless Shift is held.

One shell runs at a time. When it exits the window shows
`[process exited N]`, and a key or opening the window again starts a new
one. Quitting jetl and switching projects kill the shell together with
everything running in its session (jobs, background processes, `nohup`
ones); programs that start a session of their own (`setsid`, daemons)
survive, as they would a closed terminal. After a project switch the next
shell starts in the new root.

Language servers (`[lsp]`) start when the first file of one of their
languages opens, one per server and project, in the project root, without a
terminal (their own session; stderr goes to the log at debug level). These
are built in; each one is used only when its command is on `PATH`:

| Server | Command | Languages |
|---|---|---|
| `rust-analyzer` | `rust-analyzer` | rust |
| `typescript-language-server` | `typescript-language-server --stdio` | javascript, typescript, tsx |
| `pyright` | `pyright-langserver --stdio` | python |
| `gopls` | `gopls` | go |
| `clangd` | `clangd` | c, cpp |
| `lua-language-server` | `lua-language-server` | lua |
| `bash-language-server` | `bash-language-server start` | bash |

A server you add (a `[lsp.servers.<name>]` table with a new name, a
`command` and `languages`) comes before the built-in ones, so
`[lsp.servers.pylsp]` with `languages = ["python"]` replaces pyright. A
server takes the files it was started for: open buffers are synced as you
type (only the edited ranges, in UTF-16 positions), saves are reported, and
closed tabs are closed on the server. `settings` answers the server's
`workspace/configuration` requests by section (`rust-analyzer` above);
`init_options` goes with `initialize`.

Scratch files (File > New Scratch File…) get no language server by default.
With `scratches = true` every configured server also gets a separate client
for them, named `<server> (scratches)`, rooted at the scratches directory and
started with the server's own `command`, `settings` and `init_options`.
rust-analyzer is told about the scratch files as `detachedFiles` (read once
at its start, so a new Rust scratch restarts that client). The scratches'
problems show in the editor and the status bar, in the Problems window only
in its current-file scope (Alt+C). The key applies at once and restarts the
servers.

Rename (Shift+F6) with a server does not write at once: with `[lsp]
rename_preview = true` (the default) the Find window opens with one result per
text edit of the server's answer, grouped by file. Alt+Delete leaves a result or
a file out, Replace All (Alt+A) asks and applies the included edits and every
file operation of the answer through the usual workspace-edit path, so undo and
Local History are those of a normal apply (one undo step per open buffer).
Typing in either field or toggling an option drops the preview and runs an
ordinary search; nothing is applied. A file that changed since the preview
(edited in a tab, or on disk) refuses the whole rename. With `false` the
server's rename applies at once. The key is read when the answer arrives.
With the key on, the rename dialog has a check row, "Search in comments and
strings" (Alt+C, or a click; kept for the session): the preview then also lists
whole-word matches of the old name in comments and strings, and in whole files
with no grammar and Markdown. Those rows are guesses, so they start excluded and
carry a dim `text` marker; include one with Alt+Delete and Replace All writes it
with the rest.

Without a server for the buffer, and with a fresh `[index] scip` that has an
occurrence at the caret, Rename lists the index's occurrences of the symbol in the
same preview (always reviewed, whatever `rename_preview` says). A file that the
index does not cover, or that is edited or newer than the index, shows text
matches of the name instead, excluded and marked `text`; a local variable is
listed only in its document.
A symbol with an occurrence whose exact span another symbol also has (a shorthand
`S { x }` is both the local and the field) is not renamed from the index: the
dialog says `as text` and the rename lists text matches.

The status bar shows the server of the active file: its name, a spinner
with what it reports while it starts or indexes, `LSP: <name> missing` when
its command was not found (reported once in a notification), `LSP: <name>
stopped` when it failed or exited, and `LSP: off` with `enabled = false`.
Nothing is retried on its own: Help > Restart Language Servers
(`lsp.restart`) stops every server and starts them again for the open
files, and tries missing commands again. Servers stop when you quit or
switch projects: jetl asks them to shut down, waits up to two seconds, then
kills each server's process group (anything it started with it).

Diagnostics the servers publish show in the editor and the Problems window
(Alt+6, View > Problems). In the editor the line's worst problem puts a
glyph in the gutter's change column (`✖` error, `⚠` warning, `ℹ` info; it
covers the diff mark there) and the text in its range is underlined in the
severity's color (the `diag_error`, `diag_warn`, `diag_info` and `diag_hint`
theme styles; hints are not underlined by default). F2 and Shift+F2 (Navigate
> Next/Previous Problem) move the caret through the file's problems, hints
skipped, wrapping around; the problem at the caret shows in the status bar
when no notification is up. The status bar also counts the active file's
errors and warnings (`✖ 1 ⚠ 2`) while it is open on a server; a click opens
the Problems window on that file. The window groups the problems by file
(counts on each file row), Enter or a double click opens one at its
position, Ctrl+C copies its message. A file keeps at most
`max_diagnostics_per_file` problems and all files together at most 20000
(a note in the Event Log the first time that cuts a list); the most severe
are kept. A server's problems go when it stops, and all of them on a
project switch or restart. Problems stay where the server put them while
you type, until it publishes again.

When the caret rests on a line with a problem, jetl asks the server (after
about a quarter of a second) whether it has quick fixes there; if it does, a
`✱` lightbulb (the `lightbulb` theme style) replaces the line's glyph in the
gutter until you type or move to another line. The underline and the status
bar still show the problem. Alt+Enter lists the fixes, the server's preferred
ones first, so Enter runs the preferred fix.

Code completion comes from the file's server. With `completion_auto` the
popup opens by itself once the identifier at the caret has
`completion_min_chars` characters, and at once after one of the server's
trigger characters (`.` and `::` for rust-analyzer); Ctrl+Space (Edit >
Code Completion) asks at any time and says so when there is nothing. Typing
on filters the list (fuzzy, items starting with what you typed first); a
server that marks its list incomplete is asked again as the word changes.
Up/Down (wrapping), PgUp/PgDn and the mouse wheel select, Enter, Tab or a
click insert, Esc closes, and moving out of the word closes the popup.
Inserting makes the item's edit and its additional edits (an import, say)
one undo step; snippet placeholders are inserted as plain text with the
caret at the snippet's end mark (`$0`), else after it. Items a server fills
in lazily are resolved first; when that takes over 300 ms, or you keep
typing, the item goes in as it is.

Local history (`[local_history]`) keeps snapshots of your files in
`$XDG_STATE_HOME/jetl/history/`, outside every project, so it works in any
folder, git or not, scratch files included. A snapshot is taken on every
save (Ctrl+S, Save All, autosave, Save As, the saves of closing and
quitting) of the text written, and before jetl overwrites or deletes a
file: a git operation that rewrites the work tree (switch, checkout, pull,
merge, rebase, cherry-pick, revert, stash, discard of files or changes,
taking one side of a conflict) first stores every changed or untracked file
of the repository (at most 500), Force remove of a worktree in the
Worktrees window stores the changed and untracked files of that worktree
the same way (a plain remove needs no snapshot: git refuses it while any
file is changed or untracked), a reload from disk stores the tab's text,
Overwrite in the changed-on-disk dialog and Save As over a file store the
file on disk, and Delete in the Project tree stores the file, or the files
under a folder that the tree would show (at most 500). A snapshot equal to
the file's last one is not stored again, and identical contents are stored
once for all files. Autosaves are merged so the list stays readable: an
autosave replaces the file's last snapshot when that is an autosave too and
the first autosave of that run is less than 5 minutes old, so a long
editing session leaves one autosave snapshot per 5 minutes (the newest text
of each). Any other snapshot (a save, Save As, before a git operation, a
reload or a delete) is never merged and ends the run.

Renaming a file or folder in the Project tree, or a rename a language server
makes (a module rename, a context action), carries its snapshots to the
new path: the list shows a "Renamed from notes.txt" row (the text at the
rename) above the old path's snapshots, which stay listed under the old
path too, so a new file created there later starts from that path's
history. Save As is a copy, not a move: the new file's history starts with
its "Saved As from notes.txt" snapshot, and the old file keeps its own.
Renames made outside jetl (`mv` or `git mv` in a terminal, or files renamed
by a checkout, merge or pull) are not followed: the old path keeps its
snapshots.

Put a label on a file to keep a named snapshot: File > Local History: Put
Label… (`editor.put_label`, no key by default; also the Project tree's
context menu, and Put Label… or Alt+L in the Local History window, for the
file it shows) asks a name and stores the active file's text, unsaved edits
included (or the selected tree file as on disk), as a "Label: <name>" row,
in bold. A label applies to one file, not the whole project as in
JetBrains (the window's list of files shows it as one row). It is kept past `keep_days`, and dropped by the size limit only
after every other snapshot; Delete on its row in the Local History
window removes it (after asking). A label moves with its file when renamed in
jetl. Saving the labelled text right after adds no row.

With `snapshot_unsaved_secs` set, the text of every tab with unsaved edits
that has a file is also snapshotted on that timer ("Unsaved changes"), so
work between saves survives a killed jetl, a closed terminal, a power loss
or a "Discard and exit". Untitled tabs are not (crash recovery covers them
on a crash). Snapshots of unsaved edits merge with autosaves like autosaves
do (one per file per 5 minutes); a save of the same text turns the latest
one into "Saved", and any other snapshot ends the run.

Snapshots are stored compressed (zlib), so about three
to four times as much source text fits under `max_total_mb`, which counts
the size on disk. Binary files and files over `max_file_kb` are skipped.

File > Local History… (`editor.local_history`, Alt+Shift+H; also the
Project tree's context menu) opens the Local History tool window (bottom;
View > Local History or its strip button toggle it) on the active file, or
on the selected tree entry when the tree has focus. It keeps showing that
file when you switch tabs, until Local History is asked for another file;
renaming the file or a folder above it in the Project tree takes the window
along. On the left are the file's snapshots, newest first: date and time,
what it was taken for ("Saved", "Before git switch main", labels in bold),
and on wide windows the size and the line count change from the one
before. The list follows new snapshots as they are stored (saves,
autosaves, the unsaved timer, git operations, labels), keeping the selected
snapshot and the filter. Typing filters it (every word must appear in the
date or the label; Backspace deletes, Esc clears). On the right is the
selected snapshot against the file's current text (its unsaved edits
included), with the layout toggles of every diff (Alt+U, Alt+F), F7 and
Shift+F7 to step through the changes; Tab, Right and Left move between the
list and the diff (below 60 columns they take turns), F5 lists again, Enter
opens the file. Revert (the button, or Alt+R) puts the snapshot's text into
the file's tab as one undoable edit (Ctrl+Z takes it back), opening the
file first if needed; nothing is written until you save. Revert Change (the
button, Ctrl+Alt+Z, or "Revert Selected Changes" in the menu) puts back only
the diff's selected changes, also as one undoable edit, and leaves you in
the window with the diff compared again: F7 or a click picks one change,
Shift+Down / Shift+Up in the diff (or a Shift or Ctrl click) grow the
selection to the changes below or above, Ctrl+A takes them all; their line
numbers stand out and the button counts them. When the file's text changed
since the diff was made, nothing is reverted and the diff is compared
again. Put Label… (Alt+L)
and Remove Label (Delete, on a label row) work in place; Shift+F10 or a
right click opens the same actions as a menu. Esc clears the filter, then
goes back to the list, then to the editor.

The window also lists the files that have local history, deleted ones
included: Backspace or `[All Files]` in a file's view, File > Local
History… on a folder selected in the Project tree (that folder's files), or
File > Local History… with no file open (the project's). The title names
the folder (`Local History: src/`). Each row is a file under that folder
(files of other projects and worktrees are left out), newest change first:
its name and folder, what its newest snapshot was taken for and when. A
file deleted since is crossed out and says "deleted"; one renamed in the
Project tree says where it went. A label shows as one bold row (with how
many files carry it: a label of the same name and time on several files is
one row), and Enter lists its files below it. On the right is the
selected file's newest snapshot (or a label's, on one of its files)
against its current text, or against nothing for a deleted file. Revert
(Alt+R) and Revert Change work there too: on a deleted file they open a
tab with the snapshot's text (its folder is made again if it went too), so
saving brings the file back. Delete on a label row removes the label from
every file it is on (after asking). Enter (or a double click) on a file
shows its own snapshots, and Backspace comes back to the same row. Typing
filters the rows (every word must appear in the date, the path or what it
was taken for; `deleted` lists the deleted files). The list follows new
snapshots and Project tree deletes and renames under the folder while it
is shown; files deleted outside jetl show as deleted after F5.

Snapshots older than `keep_days` go (labels excepted), then the oldest
while the store holds more than `max_total_mb`, labels last, when jetl starts and when either setting
changes; all five settings apply at once (changing
`snapshot_unsaved_secs` restarts its timer). Several jetl instances share the
store: one prunes at a time, and a snapshot stored by another instance
during a prune may be lost. Snapshots are plain files, like crash recovery
dumps, protected only by the state directory's permissions; with
`snapshot_unsaved_secs` set, or a label put on a tab with unsaved edits,
that includes text you never saved (a pasted secret, say). A file has one
history however it is reached: through a symlink to it, or through a
symlinked folder (a project opened through a symlink included); the list
shows the path you opened.

## Indentation and .editorconfig

Each tab has its own indentation, shown in the status bar ("Tab" or
"N spaces"; click it to switch between a tab and 2, 4 or 8 spaces for that
tab). Tab, Enter after an opening bracket, indent and unindent use it: with
tabs, Tab inserts a tab character and unindent removes one; with spaces, Tab
inserts spaces up to the next stop and unindent removes up to one level of
spaces (or a tab). When a file opens, its indentation comes from, in order:

1. [EditorConfig](https://editorconfig.org): the `.editorconfig` files in
   the file's directory and its ancestors, up to the filesystem root or the
   first file with `root = true`. Closer files win over farther ones, later
   sections over earlier ones; sections use EditorConfig globs (`[*]`,
   `[*.rs]`, `[*.{yml,yaml}]`, `[lib/**.rs]`, ...).
2. The file itself: tabs when more of its first 1000 lines start with a tab
   than with spaces, otherwise the most common indent step of 2, 4 or 8
   spaces when there is enough evidence.
3. `editor.tab_width` spaces.

Keys `.editorconfig` leaves unset are filled from the next source, so
`indent_style = tab` alone keeps the tab width from `editor.tab_width`, and
`indent_style = space` alone takes the size the file uses. The keys read:

| Key | Effect |
|---|---|
| `indent_style` | `tab` or `space` |
| `indent_size` | columns per level (`tab` means use `tab_width`) |
| `tab_width` | display width of a tab in that file (also the level width with tabs) |
| `end_of_line` | `lf` or `crlf`, for files that do not exist yet; existing files keep the line ending they have |
| `trim_trailing_whitespace` | overrides `editor.trim_trailing_whitespace` for the file (Markdown is never trimmed) |
| `insert_final_newline` | overrides `editor.final_newline` for the file |

Without `tab_width` (or `indent_size` with tabs) in `.editorconfig`, tabs are
drawn `editor.tab_width` columns wide. Changing `.editorconfig` applies to
files opened after the change.

## Live templates (snippets)

A snippet is a short prefix that Tab expands into a larger piece of text with
places to fill in. jetl ships a few (Rust `fn`, `test`, `impl`, `match`, `if`, `ifelse`,
`while`, `for`, `loop`, `unsafe`; Python `def`, `class`, `ifmain`, `if`, `ifelse`,
`try`, `while`, `for`, `with`; TypeScript and JavaScript `fn`, `log`, `if`,
`ifelse`, `try`, `while`, `for`; Go `func`, `iferr`, `if`, `ifelse`, `for`,
`forr`); you add your own as
TOML files in `snippets/` next to `config.toml` (`~/.config/jetl/snippets/`).
`editor.snippets = false` turns the feature off: no snippet rows in
completion, Tab never expands, and Edit > Expand Live Template says so.

Files are named after the language as the status bar shows it
(`LanguageDef::name`: `rust.toml`, `python.toml`, `typescript.toml`,
`tsx.toml`, `javascript.toml`, `go.toml`, ...). `all.toml` applies to every
buffer, plain text included. A `tsx` buffer also reads `typescript.toml`.
Only the folder itself is read, not subfolders, and only `*.toml` files.

    # ~/.config/jetl/snippets/rust.toml
    [[snippet]]
    prefix = "pp"                      # required: letters, digits and _
    description = "debug print"        # optional, shown in the popup
    body = 'println!("{:?}", ${1:x});$0'   # required

Each `[[snippet]]` has `prefix`, `body` and an optional `description`;
other keys are ignored.

The body uses the language server snippet syntax: `$1`, `$2` are tabstops
walked with Tab (Shift+Tab back), `${1:placeholder}` has a default text,
`${1|a,b|}` offers a choice (the first is inserted), a repeated number is a
mirror edited together with the first, and `$0` is where the caret ends. A tab
character (`\t` in a basic string; a literal string needs a real tab) is one
indent level of the buffer (tab or spaces as the file uses); the following
lines get the indent of the line the caret is on. In a TOML basic string
(`"..."`) write `\\$` for a literal `$`; a literal string (`'...'` or
`'''...'''`) needs no escapes and is the easy way to write bodies with `$`
and `\`.

Variables are filled in when the snippet is accepted, as in VS Code. A value
goes in as plain text (never read as snippet syntax); a multi-line value takes
the indentation of its line. An unknown name, or one with an empty value,
inserts its default (`${NAME:default}`), else nothing. The same variables work
in language server snippets. A transform `${NAME/regex/format/options}` works
as in VS Code: the format has `$1`, `${1:/upcase}`, `${1:/downcase}`,
`${1:/capitalize}`, `${1:/camelcase}`, `${1:/pascalcase}`, `${1:+if}`,
`${1:-else}` and `${1:?if:else}`; options are `g` (all matches), `i`, `m` and
`s`. For example `${TM_FILENAME/(.*)\..+$/$1/}` is the file name without its
last extension. The regex is Rust `regex` syntax (no lookaround); an invalid
one inserts nothing. In a TOML basic string every `\` doubles, so use a
literal string. Not supported: `$CLIPBOARD` and tabstop transforms (`${1/re/fmt/}`).

- File: `TM_FILENAME`, `TM_FILENAME_BASE`, `TM_DIRECTORY`, `TM_FILEPATH`,
  `RELATIVE_FILEPATH` (to the project root); an untitled file has none of them.
- Project: `WORKSPACE_NAME`, `WORKSPACE_FOLDER`.
- Text: `TM_SELECTED_TEXT` and `SELECTION` (empty for a plain expansion),
  `TM_CURRENT_LINE`, `TM_CURRENT_WORD` (the word the prefix is in),
  `TM_LINE_INDEX`, `TM_LINE_NUMBER`, `CURSOR_INDEX`, `CURSOR_NUMBER`.
- Local date and time: `CURRENT_YEAR`, `CURRENT_YEAR_SHORT`, `CURRENT_MONTH`,
  `CURRENT_MONTH_NAME`, `CURRENT_MONTH_NAME_SHORT`, `CURRENT_DATE`,
  `CURRENT_DAY_NAME`, `CURRENT_DAY_NAME_SHORT`, `CURRENT_HOUR`,
  `CURRENT_MINUTE`, `CURRENT_SECOND`, `CURRENT_SECONDS_UNIX`,
  `CURRENT_TIMEZONE_OFFSET`.
- Random: `RANDOM`, `RANDOM_HEX`, `UUID`.
- Language comments: `LINE_COMMENT`, `BLOCK_COMMENT_START`, `BLOCK_COMMENT_END`.

Surround With: Ctrl+Alt+T (Edit > Surround With…) lists the snippets whose
body uses `$TM_SELECTED_TEXT` (or `$SELECTION`) and wraps the selection with
the chosen one. With no selection it wraps the caret line (from its first
non-blank character); a selection that covers whole lines is taken line-wise;
any other selection is wrapped exactly. The selected text keeps its own
indentation relative to the template's line. It is one undo step. Each caret
or selection is wrapped; the tabstops are walked only for a single selection
(with several, each caret lands at its copy's `$0`). The built-in surround
templates are Rust `if`, `ifelse`, `while`, `for`, `loop`, `unsafe`; Python `if`,
`ifelse`, `while`, `for`, `with`, `try`; TypeScript and JavaScript `if`, `ifelse`,
`while`, `for`, `try`; Go `if`, `ifelse`, `for`, `forr`. A user one:

    [[snippet]]
    prefix = "iflet"
    description = "if let Some"
    body = 'if let Some(${1:x}) = ${2:opt} {\n\t${TM_SELECTED_TEXT}$0\n}'

(`\n` and `\t` in a basic string `"..."` mean the same.) Used with Tab and no
selection, `$TM_SELECTED_TEXT` is its default, else nothing. Not available in
Vim's Visual mode yet.

For a buffer of language `L` the lookup order is `L.toml` (your file merged
over the built-ins: a snippet with the prefix of a built-in replaces it, and
within one file a later duplicate prefix replaces an earlier one), then
`typescript.toml` for `tsx`, then `all.toml`; the first snippet with a given
prefix wins. So a prefix in `all.toml` does not override a built-in of the
language: put the override in `<language>.toml`.

Tab expands when the editor has the focus, the find bar and the merge view
are not in use, the mode is Insert, there is one caret with no selection, the
buffer is writable, the word before the caret equals a prefix (case
sensitive) and no word character follows the caret, and the prefix is not inside a string
or comment (by the file's syntax tree; plain text files have none, so they
always qualify). Ctrl+Space and Edit > Expand Live Template still work there.
The expansion is one undo step. Tab order: an open completion
popup accepts first, then an active snippet walks to its next tabstop, then a
prefix expands, then `editor.tab` indents. The binding is the `tab` entry of
the `editor.snippet_prefix` context (action `editor.expand_snippet`); unbind
it there to keep Tab for indentation.

Snippets also appear as rows in the completion popup, after the language
server's rows, for the words whose start matches the prefix (not fuzzy).
Ctrl+Space lists them when no server runs.

The folder is watched: creating, editing or deleting a file in it, or
creating the folder, applies at once and shows `Reloaded snippets`. A file
that cannot be parsed or read, or a snippet with a bad prefix, is reported
once per change of its text (`snippets/rust.toml is invalid: ...; its
snippets are ignored`) and contributes nothing; the built-ins and the other
files stay. A reload where no text changed says nothing.

## keymap.toml

Same format as the built-in keymap, `assets/keymap.default.toml`; every
default binding is listed in [KEYMAP.md](KEYMAP.md). Tables are keymap
contexts, keys are key strings, values are action ids:

```toml
[global]
"ctrl-q" = "none"              # remove a default binding
"f2" = "app.quit"              # add one

[editor]
"ctrl-k ctrl-c" = "editor.toggle_comment"   # chords: space-separated combos (1 s timeout)
```

- Key grammar: `[ctrl-][alt-][shift-][super-]<key>`, where `<key>` is a
  single character, `f1`..`f24`, or one of `enter tab esc space backspace
  delete insert up down left right home end pageup pagedown`, plus the names
  `backtick minus plus slash backslash`. `"shift shift"` is a double tap of
  Shift.
- Keys may also be written as KEYMAP.md and Settings show them
  (`Ctrl+Shift+F`, `Ctrl+K Ctrl+C`); in that spelling the letter's case does
  not mean Shift. A bare capital letter (`"G"`) means Shift+G.
- `"none"` (or an empty string) unbinds the key in that context.
- Contexts, innermost first: `editor.find` (find bar focused), `editor`,
  `tree`, `git`, `worktrees`, `palette` (command palette open), `dialog`,
  then `global`. The first context with a binding wins. `[mode.normal]`,
  `[mode.insert]` and `[mode.visual]` are only consulted when
  `[keymap] mode = "vim"`.
- Without the kitty keyboard protocol (tmux, many legacy terminals),
  Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter, Shift Shift and most Ctrl+digit
  keys never arrive; the default keymap carries fallbacks for them (Ctrl+N,
  Alt+S, F1, Alt+Enter, Alt+Left/Right, Alt+B then a digit for Ctrl+0..9). The status bar shows `legacy keys` in that case.

Settings › Keymap edits this file: Enter or Insert on an action adds a key,
Enter on a key changes it, Delete removes it. Changes are marked `*` and
written on OK or Apply; other entries and comments are kept, a removed
default binding is written as `"none"`, and Cancel discards them.

To regenerate KEYMAP.md after changing the defaults or the action registry:

    cargo run -q -- --dump-keymap > docs/KEYMAP.md

## Themes and theme.toml

`ui.theme` picks the colors: `auto` (default), `omarchy`, the built-in
`github-dark` (GitHub Dark Default), `darcula` (IntelliJ Darcula) or `light`
(IntelliJ Light), or a theme file. A value with a `/`, a leading `~` or a
`.toml` ending is a file path, relative to the config directory. A theme
file has the same format as `theme.toml` and is applied on top of Darcula,
so it may set only some keys; the complete built-in themes are in
`assets/themes/` in the repository. An unknown name, or a theme file that
cannot be read or is not valid TOML, gives a notification and GitHub Dark.
`theme.toml` is then applied on top of the chosen theme. If it is not valid
TOML it is ignored and the chosen theme stays. On reload, an unknown theme
name, or a theme file or `theme.toml` that cannot be read or is not valid
TOML, keeps the current theme (at startup GitHub Dark is used instead, or
the `theme.toml` overrides are skipped).

### Omarchy

On [Omarchy](https://omarchy.org), `auto` (and `omarchy`) takes its colors
from the current desktop theme: the palette in
`$XDG_STATE_HOME/omarchy/current/theme/colors.toml` (or
`~/.config/omarchy/current/theme/colors.toml` on older releases) is mapped
onto every theme key by `assets/themes/omarchy.toml`, with syntax colors
like Omarchy's Helix theme. Palettes with only `color0`..`color15` work too,
and missing shades are derived as Omarchy derives them. Switching the
Omarchy theme (`omarchy-theme-set`, the theme menu) restyles jetl at once.
Without Omarchy `auto` is GitHub Dark; `omarchy` gives a notification and
GitHub Dark.

A `jetl.toml` in the Omarchy theme directory (shipped by a theme, or made
from an Omarchy template `~/.config/omarchy/themed/jetl.toml.tpl`) is
applied on top of the palette, before `theme.toml`, in the same format. A
palette that is not valid TOML or lacks one of `background`, `foreground`,
`red`, `green`, `yellow`, `blue`, `magenta` and `cyan` gives a notification
and GitHub Dark at startup, and keeps the current theme on reload.

Without true color (`ui.colors = "auto"` and `$COLORTERM` not `truecolor` or
`24bit`; ssh, sudo and tmux often drop it), every `"#rrggbb"` color is drawn
as the nearest color of the 256-color palette, indices 16 to 255. Color names
and indices like `"208"` are used as they are. Dark tints tend to end up
gray, so a theme can give exact colors for that case in a `[colors256]`
table, which takes the same entries as the top level and applies only with
256 colors (the dark built-in themes use it for diff and merge backgrounds). Set
`ui.colors = "truecolor"` if your terminal supports true color but does not
say so. The Terminal window follows the same choice (see above).

Each entry of `theme.toml` or a theme file is either a color string or a
style table:

```toml
bg = "#1e1f22"                                   # color
selection_bg = { bg = "#214283" }                # same, table form
title_focused = { fg = "#ffffff", bold = true }  # style: only listed attributes change
line_number = "#606366"                          # on a style, a string sets the foreground

[syntax]
keyword = { fg = "#cf8e6d", bold = false }
"string.escape" = "#cf8e6d"                      # dotted capture names need quotes...

[syntax.function]                                # ...or a nested table
macro = "#b3ae60"                                # = "function.macro"

[colors256]                                      # only with 256 colors
selection_bg = "24"
```

- Colors are `"#rrggbb"`, a color name (`black red green yellow blue magenta
  cyan gray darkgray lightred lightgreen lightyellow lightblue lightmagenta
  lightcyan white`), `"reset"` (the terminal default), or a 256-color index
  like `"208"`.
- A table may contain `fg`, `bg` (colors) and `bold`, `italic`, `underline`,
  `reverse` (`true` adds, `false` removes). Anything else is an error.
- Color fields take a string or a table with just `fg` or `bg`.

Color fields: `bg fg panel_bg strip_bg status_bg status_fg current_line_bg
selection_bg search_match_bg search_current_bg occurrence_bg gutter_added
gutter_modified gutter_deleted diff_add_bg diff_del_bg diff_mod_bg
diff_add_word_bg diff_del_word_bg merge_conflict_bg merge_resolved_bg popup_bg`.
The conflict resolver colors an unresolved conflict with `merge_conflict_bg`, a
change only one side made with `diff_add_bg`, `diff_del_bg` or `diff_mod_bg`,
and a resolved change with `merge_resolved_bg`.

Style fields: `border border_focused title title_focused strip_item
strip_item_active strip_drop status_accent tab tab_active tab_dirty line_number
line_number_current bracket_match secondary_caret bookmark breakpoint breakpoint_unverified fold_placeholder git_added git_modified git_deleted git_untracked
git_conflicted git_ignored popup_border list_selected list_selected_unfocused
dim error warn info diag_error diag_warn diag_info diag_hint lightbulb indent_guide inlay_hint inline_blame breadcrumbs sticky_scroll splitter`.
`strip_drop` marks the drop target while a tool strip button is dragged; fg and bg are used.
`breakpoint` is the gutter's `●`, `breakpoint_unverified` its `○`, for a breakpoint the adapter has not verified or has refused.
`indent_guide` colours the indent guides (`editor.indent_guides`); only its
foreground is used.
`inlay_hint` draws the language server's inlay hints (`[lsp] inlay_hints`); fg and
bg are used.
`inline_blame` colours the inline blame after the caret line (`editor.inline_blame`); only its foreground is used.
`breadcrumbs` is the breadcrumb row under each editor pane (`editor.breadcrumbs`); fg and bg are used,
and the ` › ` separators take the foreground of `dim`.
`sticky_scroll` is the background of sticky scroll's pinned header rows (`editor.sticky_scroll`); only bg is used, the header text keeps its syntax colours.
`secondary_caret` is the cell of every caret but the primary one (which is
the terminal cursor; in a split pane without focus every caret of a
multi-caret view gets it): reverse video by default, which an override keeps
unless it sets `reverse = false`, so its `fg` and `bg` swap on screen. `fold_placeholder` is what a collapsed
fold shows after its header line (`…`, plus the closing line it hides, as in
`fn main() {…}`): it takes the selection or find match background when one
reaches into the fold, and the mark of the worst diagnostic it hides. The
`diag_*` styles color a diagnostic's gutter glyph and Problems icon (`fg`)
and mark the text in its range: their modifiers apply to it, an underline
takes the `fg` color, and a `bg` replaces the text background.

`[syntax]` keys are tree-sitter capture names. A capture is styled by the
longest matching dotted prefix, so `keyword` also covers `keyword.function`
unless that has its own entry. Built-in captures: `keyword
keyword.directive string string.special string.special.key string.escape
escape comment comment.documentation number boolean constant
constant.builtin function function.builtin function.macro type.builtin
property attribute label operator punctuation punctuation.special
variable.builtin variable.parameter variable.mutable type.parameter
module tag embedded text.title text.literal text.reference text.uri
none`, plus the CSS/SCSS at-rule captures `media import keyframes
charset namespace supports at use forward include mixin extend each for
while return debug warn` and the Lua captures `conditional repeat method
preproc`. New names (for example `type` or `variable`) can be added;
note that the default theme leaves `type` and `variable` unstyled on
purpose, because highlight queries list them before more specific captures
of the same node. Semantic tokens from a language server (`[lsp]
semantic_tokens`) are drawn over tree-sitter's colours under the capture
names `function`, `function.method`, `function.macro`,
`variable.parameter`, `variable`, `property`, `constant`, `module`,
`type`, `type.interface`, `type.parameter`, `keyword`, `comment`,
`string`, `number`, `string.regexp`, `operator`, `attribute`, `label`,
`variable.builtin`, `type.builtin`, `boolean` and `string.escape`, with at
most one suffix `.mutable`, `.builtin` or `.readonly` (so a mutable
variable is `variable.mutable`); a token whose name the theme does not
style keeps tree-sitter's colour. A doc comment is `comment.documentation`.
