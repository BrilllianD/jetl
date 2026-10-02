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
live in `$XDG_STATE_HOME/jetl/sessions/`.

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

[ui]
theme = "darcula"              # reserved; Darcula is the only built-in theme (use theme.toml to change colors)
left_width = 32                # initial width of the left tool window (Project, Worktrees)
bottom_height = 14             # initial height of the bottom tool window (Git)
double_click_ms = 400          # double click and Shift Shift interval

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
gutter_modified gutter_deleted diff_add_bg diff_del_bg diff_mod_bg popup_bg`.

Style fields: `border border_focused title title_focused strip_item
strip_item_active status_accent tab tab_active tab_dirty line_number
line_number_current bracket_match bookmark git_added git_modified git_deleted git_untracked
git_conflicted git_ignored popup_border list_selected list_selected_unfocused
dim error warn info splitter`.

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
