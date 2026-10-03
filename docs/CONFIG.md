# Configuration

jetl reads three optional files from its config directory,
`$XDG_CONFIG_HOME/jetl/` (usually `~/.config/jetl/`):

| File | Purpose |
|---|---|
| `config.toml` | Editor, UI, git, worktree, keymap mode and file tree settings |
| `keymap.toml` | Key bindings merged on top of the built-in keymap |
| `theme.toml` | Color overrides on top of the built-in Darcula theme |

All three are read once at startup. A missing file is fine. If a file cannot
be read or is not valid TOML, jetl logs the error, shows a notification, and
uses the built-in defaults for that whole file. In `keymap.toml` and
`theme.toml`, a single bad entry (unknown action, bad key, bad color) is
skipped and reported; the other entries still apply. In `config.toml`, a
value of the wrong type rejects the whole file, because settings depend on
each other.

The log is at `$XDG_STATE_HOME/jetl/jetl.log` (usually
`~/.local/state/jetl/jetl.log`). Sessions (per project root: open tabs, tool
window layout, expanded Project tree folders and its hidden/ignored toggles)
live in `$XDG_STATE_HOME/jetl/sessions/`; recently opened project roots (the
Recent Projects palette, Alt+Shift+E) in `$XDG_STATE_HOME/jetl/recent_projects.json`.

## config.toml

Every key is optional; omitted keys keep their defaults. Unknown keys are
ignored. The full default file:

```toml
[editor]
tab_width = 4                  # columns per tab stop and indent level when neither .editorconfig nor the file decides (see below)
max_highlight_bytes = 2097152  # files above this size (2 MiB) open without syntax highlighting or gutter marks
scrolloff = 3                  # lines kept visible above and below the caret
hscrolloff = 5                 # columns kept visible left and right of the caret
cjk_ambiguous_wide = false     # reserved; has no effect yet
auto_close = true              # type brackets and quotes in pairs, step over closers, wrap selections
highlight_occurrences = true   # highlight the word at the caret (or the selected text) elsewhere on screen
trim_trailing_whitespace = true # on save, strip trailing spaces and tabs (not on the caret line, never in Markdown)
final_newline = true           # on save, end a non-empty file with a line break
autosave = "off"               # "off", "focus" (save when the terminal loses focus) or "idle" (save after a pause in editing)
autosave_idle_ms = 1500        # pause before an "idle" autosave
max_tabs = 0                   # open tabs beyond this close the least recently used one that is clean
                               # and unpinned (none such: the limit is exceeded); 0 = no limit

[ui]
theme = "darcula"              # reserved; Darcula is the only built-in theme (use theme.toml to change colors)
left_width = 32                # initial width of the left tool window (Project, Worktrees)
bottom_height = 14             # initial height of the bottom tool window (Git)
double_click_ms = 400          # double click and Shift Shift interval
icons = "unicode"              # tool strip and menu icons: "unicode" (one-cell symbols) or "emoji" (needs a color emoji font)

[git]
pull_mode = "ff-only"          # "ff-only" (git pull --ff-only), "rebase" (--rebase) or "merge" (--no-rebase)
log_all = false                # Log tab shows all branches (git log --all) instead of HEAD only
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
watch = true                   # follow changes made outside jetl (tree, git state, open files)

[clipboard]
paste_command = ""             # prints the system clipboard; "" = detect, "none" = jetl's own last copy only
copy_command = ""              # takes copied text on stdin, besides OSC 52; "" = the detected tool's
                               # partner, "none" = OSC 52 only

[format]
on_save = false                # Ctrl+S and save all run the file's formatter before writing
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

# Run tasks (Alt+Shift+F10 picks one). None by default; examples:
# [[run.tasks]]
# name = "build"                 # shown in the picker and the Run window
# command = "cargo build 2>&1"   # a string runs through `sh -c`
# [[run.tasks]]
# name = "web tests"
# command = ["npm", "test", "--", "--watch=false"]  # a list of words runs directly, no shell
# cwd = "web"                    # relative to the project root; default the root
# env = { CI = "1" }             # added to jetl's environment

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
# Built-in servers (table below) take changes here; a table with a new
# name adds a server. Omitted keys keep the built-in values.
# [lsp.servers.rust-analyzer]
# command = ["rust-analyzer"]    # the command and its arguments; [] turns the server off
# languages = ["rust"]           # language ids, as the status bar shows them
# init_options = {}              # sent as initializationOptions
# settings = { rust-analyzer = { check = { command = "clippy" } } }  # answers workspace/configuration
```

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
tmux that means `set -g focus-events on`.

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

The result is applied as one undoable edit of the changed lines only: Ctrl+Z
takes the whole reformat back, the caret stays on the same code and the view
on the same lines. Output identical to the buffer changes nothing. With
`on_save`, Ctrl+S first applies the save cleanup (`trim_trailing_whitespace`,
`final_newline`), then waits for the formatter and writes its text. If the
formatter fails or times out, the file is saved unformatted and a warning
shows its first error line (the full output is in the Event Log, Alt+Shift+L).
If you type while it runs, the reformat is dropped and the file gets the
text as it was when you pressed Ctrl+S; your later edits stay unsaved.

Only explicit saves format. Autosave, and the saves done by closing a tab,
quitting or switching projects with unsaved changes ("Save all and exit"),
write at once without the formatter, so they never wait on it.

Run tasks (`[[run.tasks]]`) are listed by `run.pick` (Alt+Shift+F10), then
the project's own `.jetl/run.toml`, which holds the same entries as
`[[tasks]]` tables and is read each time the picker opens. When neither has
any, the picker offers defaults found in the project root: `cargo build`,
`cargo test` and `cargo run` for a `Cargo.toml`, `npm run <script>` for each
of the first 20 `package.json` scripts, `make` for a Makefile, and
`go build ./...` and `go test ./...` for a `go.mod`. A task runs only when
you pick it (or rerun it), so a project file cannot start anything by
itself.

A task runs without a terminal: stdin is closed and stdout and stderr share
one pipe, so their lines interleave as written, but programs that color or
animate only on a terminal print plain output (escape codes that still come
through are stripped). The Run window (Alt+4) shows its status, elapsed time
and the last 10 000 lines, following new output unless you scroll up.
File locations in the output become links (rustc and cargo, tsc, pytest and
Python tracebacks, and any `path:line:col`), resolved against the task's
directory and its parents up to the project root; only files that exist are
linked. Errors are red, warnings yellow. A click on a link, Enter or a double
click opens it; F4 and Shift+F4 (Ctrl+Alt+Down/Up in the Run window) step
through them from anywhere. Shift+F10 or Ctrl+F5 runs the last task again
(in the Project and Git windows Shift+F10 opens the context menu instead),
and Ctrl+F2 stops it.

One task runs at a time: starting another kills the running one first, as
do quitting jetl and switching projects. A stop sends SIGKILL to the task's
whole process group, so the processes it started (cargo's rustc, npm's node)
go with it; programs that put themselves in a new session (daemons) are not
tracked. A task that fails (a non-zero exit code, killed by a signal jetl
did not send, or a command that cannot start) gives one error notification.

The Terminal window (Alt+F12) runs the shell on a pseudo terminal in the
project root with `TERM=xterm-256color` and `COLORTERM=truecolor`, started
the first time the window is shown. Full-screen programs (vim, less, htop)
work, at the window's size, which follows Ctrl+Shift+Up/Down and the
terminal's own resizes. While it has focus every key goes to the shell,
global shortcuts such as Ctrl+Q and Ctrl+S included, except the keys in the
`[terminal]` table of `keymap.toml` (by default Alt+F12 to hide it,
Ctrl+Shift+Up/Down to resize it and Ctrl+V to paste the system clipboard)
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
- `"none"` (or an empty string) unbinds the key in that context.
- Contexts, innermost first: `editor.find` (find bar focused), `editor`,
  `tree`, `git`, `worktrees`, `palette` (command palette open), `dialog`,
  then `global`. The first context with a binding wins. `[mode.normal]`,
  `[mode.insert]` and `[mode.visual]` are only consulted when
  `[keymap] mode = "vim"`.
- Without the kitty keyboard protocol (tmux, many legacy terminals),
  Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter and Shift Shift never arrive; the
  default keymap carries fallbacks for them (Ctrl+N, Alt+S, F1, Alt+Enter,
  Alt+Left/Right). The status bar shows `legacy keys` in that case.

To regenerate KEYMAP.md after changing the defaults or the action registry:

    cargo run -q -- --dump-keymap > docs/KEYMAP.md

## theme.toml

Overrides the Darcula theme. Each entry is either a color string or a style
table:

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
```

- Colors are `"#rrggbb"`, a color name (`black red green yellow blue magenta
  cyan gray darkgray lightred lightgreen lightyellow lightblue lightmagenta
  lightcyan white`), `"reset"` (the terminal default), or a 256-color index
  like `"208"`.
- A table may contain `fg`, `bg` (colors) and `bold`, `italic`, `underline`
  (`true` adds, `false` removes). Anything else is an error.
- Color fields take a string or a table with just `fg` or `bg`.

Color fields: `bg fg panel_bg strip_bg status_bg status_fg current_line_bg
selection_bg search_match_bg search_current_bg occurrence_bg gutter_added
gutter_modified gutter_deleted diff_add_bg diff_del_bg diff_mod_bg
diff_add_word_bg diff_del_word_bg popup_bg`.

Style fields: `border border_focused title title_focused strip_item
strip_item_active status_accent tab tab_active tab_dirty line_number
line_number_current bracket_match bookmark git_added git_modified git_deleted git_untracked
git_conflicted git_ignored popup_border list_selected list_selected_unfocused
dim error warn info diag_error diag_warn diag_info diag_hint splitter`. The
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
variable.builtin tag embedded text.title text.literal text.reference
text.uri none`, plus the CSS/SCSS at-rule captures `media import keyframes
charset namespace supports at use forward include mixin extend each for
while return debug warn` and the Lua captures `conditional repeat method
preproc`. New names (for example `type` or `variable`) can be added;
note that the default theme leaves `type` and `variable` unstyled on
purpose, because highlight queries list them before more specific captures
of the same node.
