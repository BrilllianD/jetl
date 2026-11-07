# Testing

## Automated

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

CI runs the same three commands (`bitbucket-pipelines.yml`,
`.github/workflows/ci.yml`): fmt and clippy on stable, tests on stable and
on the MSRV (`rust-version` in `Cargo.toml`).

Tests live next to the code (`#[cfg(test)] mod tests`). For anything that
goes through the app (messages, effects, dialogs, key routing) use
`crate::app::tests::test_app(name)`: it builds an `App` rooted at a fresh
temp directory with its own config and state, so tests never touch the
user's files. Feed background results with `app.handle_msg(AppMsg::…)`;
nothing reads the bus in tests, so work the app spawns only matters when you
send its result yourself. Git tests make the root a repository with
`init_repo(&app, files)` and feed its status with `feed_status`; operations
the app runs (git2 or the `git` CLI) report back on the bus, which
`handle_next(&mut app, want)` waits for. Terminal window tests run
`/bin/sh` with a cleared environment (`App::terminal_spec` under
`cfg(test)`), never your shell or its startup files. Tests that check a
killed child process (Run tasks, the shell, formatters) wait with
`crate::util::process_gone(pid)` (Linux): it polls `/proc` for up to 10 s
and counts a zombie or a process being reaped (`X`) as gone; never sleep a
fixed time instead. Nothing `App::new` starts writes in
the background (`main.rs` starts the recent projects file and the crash
dump prune; local history is `Handle::disabled()` in `test_app`), so a
removed temp directory stays removed: keep it that way for new startup
work. Language servers are
off in `test_app`; `app::lsp::tests::with_fake_servers` turns them on with
in-process servers (`lsp::fake::FakeServer`: answers `initialize` and
`shutdown` itself, hands the test every message it gets, and sends
notifications, requests and responses), so tests never start a real
server. Config reload tests write files into `app.paths.config_dir` and
send `AppMsg::ConfigChanged` themselves; every `config.toml` they write sets
`[lsp] enabled = false`. `docs/KEYMAP.md` is checked by a test; regenerate
it with `cargo run -q -- --dump-keymap > docs/KEYMAP.md`.

Palette filtering has an `#[ignore]` benchmark that prints, for 200000
paths, the per-keystroke main-thread cost of filtering at once (before) and
off the main thread (after), when the result arrives, the cost of Enter
right after typing, and the at-once cost by list size (what
`BACKGROUND_MIN` is tuned on). It asserts only that both ways give the same
rows; run it in release:

    cargo test --release --lib filter_200k_paths_timing -- --ignored --nocapture

## Snapshot tests

`src/app/snapshot_tests.rs` draws whole frames, or the editor area alone,
on ratatui's `TestBackend` and compares them as text with
[`insta`](https://insta.rs) against `src/app/snapshots/*.snap`. They cover
split panes, carets, occurrences, folding, soft wrap, bookmarks, breakpoints in the gutter, the
numbered bookmark chooser and the bookmarks list with closed files, the
editor's find/replace bar, the project tree and its context menu, the Settings window (page tree, Bool, number and
choice rows, pending markers), the Git
window's Commit tab (also at 80 columns, its diff header cut before the
toggles), its Log (also with two commits selected, and the Log's commit context menu), Stash and Console tabs, the Find and Problems windows
(diagnostic underlines in the editor too), the lightbulb in the gutter, the main menu bar, confirm,
input and choice dialogs, the hover and completion popups, toasts, the
palettes (Scratch Files and Recent Projects included), the conflict resolver (filler rows included), the patch tab, the crash
recovery dialogs and the Save As prompt and its path completion list.

Things that depend on the clock are given as data: the Log tab's commits
carry their relative dates as `git log` formatted them, toasts expire on
the tick and never while drawing, and LSP popups are built from a hover or
completion answer without a server.

Helpers (in `snapshot_tests.rs` unless noted):

- `stable_app(name)`: a `test_app` with nothing that depends on the
  terminal or the clock. It turns the kitty keyboard flag on, clears
  notifications and toasts, and drops the startup requests' results. It
  returns a `Fixture` that removes the temp directory on drop.
- `draw_app(app, w, h)`: the whole frame (`ui::render`). `draw_editor(app,
  w, h)`: only `Editor::render`, its tab bars on the top row, so editor
  tests do not change when the chrome does. Both settle first: they drop
  whatever background work sent meanwhile and update the gutters. They
  also fail if a notification is showing, because how long it shows
  depends on the test's speed.
- Soft wrap needs the view's width, so draw once before moving by screen
  rows, as the run loop always has.
- `ui::snapshot::snapshot(term, marks, mark_area)` makes the text;
  `Mark::bg`, `Mark::style`, `Mark::modifier` and `Mark::underline` (an
  underline's color: diagnostics) name the cells to mark.

Format:

```
80x14 cursor=(51,6)          width x height, the terminal cursor or "hidden"
00   notes.log ×             one line per screen row: its number, then the
01   1  ▾ alpha beta gamma   text as it shows (right-trimmed)
-- marks: C=caret S=selection F=fold O=occurrence
03          OOOOO            the same columns, a letter per marked cell
```

The marks grid lists only rows with a marked cell, and only inside
`mark_area` when one is given. The first mark that matches a cell wins. A
wide glyph's letter repeats over both of its cells, so it lines up under
the text in an editor. Colors are never printed: each mark names a theme
field (`selection_bg`, `fold_placeholder`, `merge_conflict_bg`, …), so a
theme change does not touch the snapshots, but a highlight in the wrong
cell does. The terminal cursor is the primary caret; the other carets are
`C` cells. A snapshot must not contain the temp directory path.

Updating (cargo-insta is not needed):

```sh
INSTA_UPDATE=always cargo test snapshot_tests   # rewrite the .snap files
git diff src/app/snapshots/                      # review every change
```

Read each changed snapshot and check that the cursor, the carets and the
marks are in the right cells before committing it. A snapshot that records
a bug is worse than none. If a frame shows a bug you are not fixing, keep
the snapshot as it is, say so in a comment at the test, and file the bug.
Without `INSTA_UPDATE`, a local run writes `*.snap.new` next to a
mismatching snapshot; they are git-ignored. On CI (`CI=true`, set by
GitHub Actions and Bitbucket Pipelines) nothing is written, and a missing
or different snapshot fails the test. Tests that need a grammar are
behind its feature (`#[cfg(feature = "lang-rust")]` for `.rs` folding,
`lang-toml` where a `.toml` tab's language shows in the status bar, and
`structural` for frames that show the Find window's header); the
rest use plain file types, so every feature set gives the same frames.

## Manual: the tmux harness

Behavior that needs a real terminal (rendering, mouse, focus, timing) is
checked by running jetl inside a detached tmux session and reading the
screen back. Do not close a task on "it compiles".

If `tmux` is shadowed by a shell alias or plugin, call the binary through a
wrapper with its own server socket:

```sh
cat > /tmp/tm.sh <<'EOF'
#!/bin/sh
exec /usr/bin/tmux -L jetl "$@"
EOF
chmod +x /tmp/tm.sh
```

A scratch project (config and state go to `/tmp/jt-home` below, away from
your home directory):

```sh
T=/tmp/jt; rm -rf $T; mkdir -p $T/src $T/docs && git -C $T init -q
printf 'fn main() {\n\tprintln!("héllo 世界");\n}\n' > $T/src/main.rs
echo notes > $T/docs/notes.md; echo readme > $T/README.md; echo target > $T/.gitignore
git -C $T add . && git -C $T -c user.name=t -c user.email=t@t commit -qm init
```

Run from the repository root and inspect:

```sh
cargo build --no-default-features --features lang-rust
/tmp/tm.sh -f /dev/null new-session -d -s jetl -x 120 -y 30 \
  "XDG_CONFIG_HOME=/tmp/jt-home/config XDG_STATE_HOME=/tmp/jt-home/state $PWD/target/debug/jetl $T 2>/tmp/jetl-stderr.txt"
sleep 2; /tmp/tm.sh capture-pane -t jetl -p          # dump the screen
/tmp/tm.sh send-keys -t jetl M-1                     # Alt+1
/tmp/tm.sh send-keys -t jetl -l 'text'               # literal typing ('\;' for a lone ;)
/tmp/tm.sh send-keys -t jetl C-s Enter Escape F5     # named keys
/tmp/tm.sh kill-server
```

Mouse input is raw SGR escape bytes (1-based column;row):

```sh
hex() { printf '%s' "$1" | od -An -tx1 | tr -d '\n'; }
press()   { /tmp/tm.sh send-keys -t jetl -H $(hex "$(printf '\033[<%d;%d;%dM' $1 $2 $3)"); }  # button col row
release() { /tmp/tm.sh send-keys -t jetl -H $(hex "$(printf '\033[<%d;%d;%dm' $1 $2 $3)"); }
press 0 10 4; release 0 10 4                  # left click at col 10, row 4
press 0 47 2; press 32 50 4; release 0 50 4   # drag (32 = left button motion)
press 64 10 4                                 # wheel up; 65 = wheel down
```

Leave more than 400 ms between clicks on the same cell unless you want a
double click.

The toolbar (`[ui] toolbar`, on by default) takes screen row 1 and moves
everything else one row down. The row numbers in the scenarios below were taken
without it, so their `config.toml` gets `[ui]` / `toolbar = false` (next to
`[lsp] enabled = false`), or add one to every row.

### Scenario: split panes

At 80x24 and at 120x40, with a long file open (`docs/notes.md` with 80
lines works):

```sh
/tmp/tm.sh send-keys -t jetl 'M-\'        # split right: both panes show the file
/tmp/tm.sh send-keys -t jetl -l 'NEW'; /tmp/tm.sh send-keys -t jetl Enter
/tmp/tm.sh send-keys -t jetl M-Tab        # the other pane: its caret moved down a line
/tmp/tm.sh send-keys -t jetl M--          # split down
/tmp/tm.sh display -p -t jetl '#{cursor_x},#{cursor_y}'   # the cursor is in the focused pane
```

Then click into another pane's text (it takes focus and the caret), wheel
over a pane that is not focused (it scrolls, the cursor stays), drag a bar
between panes (find its column in the captured screen; a pane keeps 20
columns and 4 rows while there is room), right-click a tab (the menu has
Split Right/Down, Move to Opposite Group, Unsplit), and run Unsplit and
Unsplit All from F1. At 80x24 with the Project window open a second split
right is refused with a toast.

Tab drags between panes (needs mouse drags, e.g. an SGR mouse script, which
tmux `send-keys` cannot send): with two panes holding several tabs, drag a
tab onto the other pane's bar (left half of a tab drops before it, right
half after, past the last tab appends; the other pane takes focus), onto the
other pane's text (` + name ` ghost tab at the end of its bar, the release
appends), and a pinned tab onto the other bar (it stays among the pinned
ones). Dragging the only tab of a pane out collapses that pane.

### Scenario: multiple carets

At 80x24 and at 120x40, with a small file whose word repeats
(`printf 'let a = 1;\nlet b = a + a;\nprint(a);\n' > $T/docs/notes.md`,
opened as `jetl $T/docs/notes.md`, the Project window hidden with Alt+1):

```sh
for i in 1 2 3 4; do /tmp/tm.sh send-keys -t jetl Right; done
for i in 1 2 3; do /tmp/tm.sh send-keys -t jetl M-j; sleep 0.3; done
/tmp/tm.sh capture-pane -t jetl -p | tail -1     # Ln 2, Col 14 (3 carets, 3 chars)
/tmp/tm.sh send-keys -t jetl -l 'XY'             # every `a` but the one in print(a)
/tmp/tm.sh send-keys -t jetl C-z                 # one step back, three carets again
/tmp/tm.sh send-keys -t jetl Escape; sleep 0.6   # one caret, still selecting
/tmp/tm.sh send-keys -t jetl Escape              # the selection goes
press 8 10 2; release 8 10 2                     # Alt+click (8 = Alt): a caret on line 1
press 8 12 4; release 8 12 4                     # another on line 3, now the primary
press 8 12 4; release 8 12 4                     # Alt+click on it again removes it
```

`capture-pane -e` shows the other carets as reverse video (`ESC[7m`), a
caret at a line end on the cell after the text. Then split right (`M-\`),
type in the new pane, and check that the other pane still draws all its
carets, moved with the text; `M-Tab` back to it and type at all of them.

### Scenario: toasts

Fixture: `T=$(mktemp -d)`, `seq -f 'line %g of the toast check' 1 40 > $T/notes.txt`,
`[lsp] enabled = false` in a private config. `jetl $T/notes.txt`, Esc (focuses
the editor), Ctrl+Z: the Info toast `Nothing to undo`. Under tmux the Warning
`Terminal lacks the kitty keyboard protocol: ...` may stack below it: its 8 s run
from startup, so once the Info is gone it takes the top slot only if still up. In `capture-pane`, the `╭` of the first
toast is in cell 43 and its `╮` in cell 78 at 80x24 (0-based), and in cells 67 and
118 at 120x40. At 80x24 every line number beside it is whole.

Then Settings (Ctrl+Alt+S), Language Servers page, set `SCIP index file` to
`missing.scip`, Ctrl+S. At 120x40 the Warn toast `SCIP index missing.scip: ...`
shows at once below the window, the Info `Reloaded config.toml` waits below it
and takes its slot when the Warn is gone about 8 s later, with Settings still
open; the Info is gone 4 s after that. At 80x24 no toast shows. Wait 10 s and
close Settings: the Warn toast is there with the Info `Reloaded config.toml`
below it; 6 s later the Warn still is (the Info is gone); 3 s after that it is
gone. The Event Log (Alt+5) has one row for it.

### Scenario: short screen dialog

A Confirm whose message is longer than the screen (yman 222), at 60x12:

```sh
# fresh T and session as above (120x30), then:
/tmp/tm.sh send-keys -t jetl F1; /tmp/tm.sh send-keys -t jetl -l 'replace in files'
/tmp/tm.sh send-keys -t jetl Enter; /tmp/tm.sh send-keys -t jetl -l 'readme'; sleep 1
/tmp/tm.sh send-keys -t jetl Tab; /tmp/tm.sh send-keys -t jetl -l "$(printf '%0300d' 0)"; sleep 1
/tmp/tm.sh resize-window -t jetl -x 60 -y 12; sleep 0.5
/tmp/tm.sh send-keys -t jetl M-a                  # find.replace_all
/tmp/tm.sh capture-pane -p -t jetl
/tmp/tm.sh send-keys -t jetl Escape
git -C $T status --short                          # prints nothing
/tmp/tm.sh kill-server
```

The "Replace in Files" box is at most 11 rows, its message ends with a row
cut with `…`, `[ Replace ]` and `[ Cancel ]` are whole directly above the
bottom border, and nothing is drawn past the box's borders. Escape closes
it and nothing is replaced.

### Scenario: replace in files preview

At 120x40 and 80x24 (yman 373). Ctrl+Shift+R is a legacy key under tmux, so
open the Find window with Alt+3:

```sh
printf 'foo one\nfoo two\nfoo three\n' > $T/n.txt
# fresh session as above, then:
/tmp/tm.sh send-keys -t jetl M-3; /tmp/tm.sh send-keys -t jetl -l 'foo'; sleep 1
/tmp/tm.sh send-keys -t jetl M-r; /tmp/tm.sh send-keys -t jetl -l 'bar'; sleep 0.5
/tmp/tm.sh capture-pane -p -t jetl       # every result reads "foobar one" style
/tmp/tm.sh send-keys -t jetl Tab Down M-DC Down M-DC   # Alt+Delete on results 2 and 3
/tmp/tm.sh capture-pane -p -t jetl       # status ends "; 2 excluded", those rows show no "bar"
/tmp/tm.sh send-keys -t jetl M-a; sleep 0.5; /tmp/tm.sh send-keys -t jetl Enter; sleep 1.5
cat $T/n.txt                             # "bar one", then "foo two" and "foo three"
# Search again, then change the file behind the app's back:
printf 'x\nfoo one\n' > $T/n.txt
/tmp/tm.sh send-keys -t jetl M-a; sleep 0.5; /tmp/tm.sh send-keys -t jetl Enter; sleep 1.5
/tmp/tm.sh capture-pane -p -t jetl | tail -2   # "Replace in files: <path>: changed since the search; search again"
cat $T/n.txt                             # unchanged
/tmp/tm.sh kill-server
```

Delete without Alt still deletes a character in the Find field. A file row
excluded with Alt+Delete dims all its results.

### Scenario: rename without a language server

At 120x40 and 80x24 (yman 373), `[lsp] enabled = false`:

```sh
mkdir -p $T/src
printf 'fn helper() {}\nfn main() {\n    helper(); // helper\n}\n' > $T/src/main.rs
printf 'fn f() {\n    helper();\n}\n' > $T/src/util.rs
printf 'helper here\n' > $T/notes.txt
# start jetl on $T itself as above (fresh state dir), wait for indexing (~1 s), then open src/main.rs:
/tmp/tm.sh send-keys -t jetl C-n; sleep 0.5; /tmp/tm.sh send-keys -t jetl -l 'main.rs'; sleep 0.5; /tmp/tm.sh send-keys -t jetl Enter; sleep 2
/tmp/tm.sh send-keys -t jetl Down Down Right Right Right Right Right   # caret in "helper" on line 3
/tmp/tm.sh send-keys -t jetl S-F6; sleep 0.5
/tmp/tm.sh capture-pane -p -t jetl       # dialog "Rename 'helper' as text to:", button Preview
/tmp/tm.sh send-keys -t jetl C-a   # selects "helper"; typing replaces it
/tmp/tm.sh send-keys -t jetl -l 'assist'; sleep 0.3; /tmp/tm.sh send-keys -t jetl Enter; sleep 2
/tmp/tm.sh capture-pane -p -t jetl       # Find: 5 results (notes.txt 1, main.rs 3, util.rs 1); notes.txt and the comment dimmed
/tmp/tm.sh send-keys -t jetl M-a; sleep 1  # dialog "Replace 3 matches in 2 files with "assist"?"
/tmp/tm.sh send-keys -t jetl Enter; sleep 2
cat $T/src/util.rs; cat $T/notes.txt     # util has assist(); notes unchanged
/tmp/tm.sh kill-server
```

The toast says "Replaced 3 matches in 2 files". The main.rs buffer holds the
change unsaved (assist twice, the comment still says helper). Bare Delete in
the Find field (Home, Delete) removes the first letter of the query and
excludes nothing. Any later search in Find shows every result included again.

### Scenario: rename preview with a language server

At 120x40 and 80x24 (yman 397), with rust-analyzer, `[lsp]` enabled, a
project, config, state and cache under `/tmp/jetl-397a` and the harness
script `/tmp/tm24.sh` (as `/tmp/tm.sh` above, socket `jetl24`). Shift+F6
reaches jetl as `S-F6`:

```sh
T=/tmp/jetl-397a/proj; mkdir -p $T/src
printf '[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2021"\n' > $T/Cargo.toml
printf 'mod util;\nfn main() {\n    util::helper();\n    util::helper();\n}\n' > $T/src/main.rs
printf 'pub fn helper() {}\n' > $T/src/util.rs
# start jetl on $T with XDG_CONFIG_HOME, XDG_STATE_HOME and XDG_CACHE_HOME under /tmp/jetl-397a,
# open src/main.rs, wait until rust-analyzer has finished indexing, caret in `helper` on line 3
/tmp/tm24.sh send-keys -t jetl S-F6; sleep 1
/tmp/tm24.sh capture-pane -p -t jetl      # dialog "Rename 'helper' and its usages to:", button Preview
/tmp/tm24.sh send-keys -t jetl C-a; /tmp/tm24.sh send-keys -t jetl -l 'assist'; /tmp/tm24.sh send-keys -t jetl Enter; sleep 2
/tmp/tm24.sh capture-pane -p -t jetl      # Find focused: main.rs 2 rows, util.rs 1, the toast; nothing written
cat $T/src/util.rs                         # still helper
/tmp/tm24.sh send-keys -t jetl Down M-DC   # Alt+Delete on the second row: the row is dimmed
/tmp/tm24.sh send-keys -t jetl M-a; sleep 0.5
/tmp/tm24.sh capture-pane -p -t jetl      # dialog "Rename": "2 occurrences in 2 files; 1 excluded."
/tmp/tm24.sh send-keys -t jetl Enter; sleep 2
/tmp/tm24.sh capture-pane -p -t jetl      # main.rs: one assist, one helper; Find empty, "type to search the project"
cat $T/src/util.rs                         # assist
/tmp/tm24.sh send-keys -t jetl C-z         # main.rs restored in one step (focus the editor first)
```

Repeat the rename, then type a letter in the Find field: an ordinary
search, nothing applied. Repeat it again and change `util.rs` from another
shell before Alt+A, Enter: the error toast `src/util.rs changed since the
preview; rename again`, nothing changes and the results stay. Last, write
`[lsp]\nrename_preview = false` to `config.toml` (it applies live):
Shift+F6 shows the button `Refactor` and Enter applies at once. The Find
status line is clipped on a narrow window (the file operation count may not
show) and may not show `1 excluded`; the confirm dialog always says it
(`; 1 excluded`) and lists the operations. Stderr is empty and
Ctrl+Q exits 0.

### Scenario: rename with comments and strings

At 120x40 and 80x24 (yman 397), with rust-analyzer, `[lsp]` enabled, a
project, config, state and cache under `/tmp/jetl-397b` and the harness
script `/tmp/tm24.sh` as above. Ctrl+Shift+A does not arrive under tmux (no
kitty protocol); nothing here needs it. Shift+F6 is `S-F6`, Alt+C `M-c`:

```sh
T=/tmp/jetl-397b/proj; mkdir -p $T/src
printf '[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2021"\n' > $T/Cargo.toml
printf 'mod util;\n// helper is called twice\nfn main() {\n    util::helper();\n    util::helper();\n    println!("helper");\n}\n' > $T/src/main.rs
printf 'pub fn helper() {}\n' > $T/src/util.rs
printf 'helper notes\n' > $T/notes.txt
# start jetl on $T, open src/main.rs, wait until rust-analyzer has finished indexing, caret in `helper` on line 4
/tmp/tm24.sh send-keys -t jetl S-F6; sleep 1
/tmp/tm24.sh capture-pane -p -t jetl      # "[ ] Search in comments and strings   Alt+C", button Preview
/tmp/tm24.sh send-keys -t jetl M-c         # "[x]"
/tmp/tm24.sh send-keys -t jetl C-a; /tmp/tm24.sh send-keys -t jetl -l 'assist'; /tmp/tm24.sh send-keys -t jetl Enter; sleep 3
/tmp/tm24.sh capture-pane -p -t jetl      # Find: notes.txt 1 (text), main.rs comment (text), 2 exact, string (text); util.rs 1; toast "3 text matches start excluded"
cat $T/notes.txt $T/src/util.rs            # still helper
/tmp/tm24.sh send-keys -t jetl M-a; sleep 0.5
/tmp/tm24.sh capture-pane -p -t jetl      # "3 occurrences in 2 files; 3 excluded." and "Text matches (guesses): 0 included, 3 left out."
/tmp/tm24.sh send-keys -t jetl Escape      # the dialog goes, the preview stays
/tmp/tm24.sh send-keys -t jetl Down Down M-DC   # the selection starts on the first result; the comment row is two Downs on, no longer dimmed
/tmp/tm24.sh send-keys -t jetl M-a; sleep 0.5
/tmp/tm24.sh capture-pane -p -t jetl      # "4 occurrences in 2 files; 2 excluded." and "Text matches (guesses): 1 included, 2 left out."
/tmp/tm24.sh send-keys -t jetl Enter; sleep 2
/tmp/tm24.sh capture-pane -p -t jetl      # main.rs: the comment says assist, the string still helper, both calls assist
cat $T/notes.txt $T/src/util.rs            # notes still helper, util assist
/tmp/tm24.sh send-keys -t jetl C-z         # focus the editor first: main.rs restored in one step
printf 'pub fn helper() {}\n' > $T/src/util.rs; sleep 5   # Ctrl+Z does not restore util.rs on disk: restore it from the shell and wait
/tmp/tm24.sh send-keys -t jetl S-F6        # the check still shows "[x]" this session
```

On a file header whose rows are all excluded, Alt+Delete includes the exact
rows and leaves the text matches excluded (a file of text matches only: all
included); with any row included it excludes them all.

Esc closes that dialog. Before every later Shift+F6, press Ctrl+Z in the editor
if `main.rs` changed, restore `src/util.rs` and `notes.txt` from the shell, and
wait about five seconds, as above. In each dialog Ctrl+A, type `assist`, Enter.
Stale, excluded: Shift+F6, change `notes.txt` from another shell, Alt+A, Enter:
it applies and `notes.txt` is untouched by jetl. Stale, included: restore as
above, Shift+F6, include the `notes.txt` row, change `notes.txt`, Alt+A, Enter:
the error toast `notes.txt changed since the preview; rename
again`, nothing changes and the rows stay. With `[lsp]\nrename_preview =
false` in `config.toml` Shift+F6 shows no check row and the button `Refactor`.
The Find status line is clipped on a narrow window; the confirm dialog says
the counts. Stderr is empty and Ctrl+Q exits 0.

### Scenario: column selection

At 80x24 and at 120x40, with a file of tabs, wide characters and a short
line that does not soft-wrap (`.log`: column selection refuses soft wrap,
which `.md` files get; `printf '\tab日本語x\n\tab日本語x\nab\n\tab日本語x\n' > $T/docs/notes.log`,
opened as `jetl $T/docs/notes.log`; the Project window starts focused, so
hide it with Alt+1 twice).
`X` is the 1-based screen column of the text's first cell (`X=11` past the
3-column tool strip and the 7-column gutter; check with
`display -p '#{cursor_x}'` plus one while the caret is at a line start), so
`$((X+k))` is the cell at visual column k (0-based; the status bar's `Col`
is 1-based). The first line is on row 2. Toasts take the right half of the
top rows (from screen column 40 at 80x24 with the Project window hidden; a
gutter under that column moves the edge right of it) and a press on one only closes
it, so keep presses left of that or wait until `capture-pane` shows no toast:

```sh
press 8 $((X+5)) 2; press 40 $((X+6)) 4; press 40 $((X+7)) 5; release 8 $((X+7)) 5
/tmp/tm.sh capture-pane -t jetl -p | tail -1     # Ln 4, Col 9 (4 carets, 6 chars)
/tmp/tm.sh send-keys -t jetl -l 'Q'              # b日 becomes Q on the long lines, Q after "ab"
/tmp/tm.sh send-keys -t jetl C-z                 # the block again
press 12 $((X+20)) 3; press 44 $((X+12)) 2; release 12 $((X+12)) 2   # Alt+Shift, right to left from past the line ends: x, x added (6 carets)
/tmp/tm.sh send-keys -t jetl Escape; sleep 0.6; /tmp/tm.sh send-keys -t jetl Escape
/tmp/tm.sh send-keys -t jetl C-Home Right        # after the tab of line 1
/tmp/tm.sh send-keys -t jetl M-S-IC              # Alt+Shift+Insert: status ends in " Column"
for i in 1 2 3; do /tmp/tm.sh send-keys -t jetl S-Down; done   # status "Ln 4, Col 5 (4 carets) Column": column 4, after the tab
/tmp/tm.sh send-keys -t jetl -l '#'              # a # on all four lines, at the end of "ab"
/tmp/tm.sh send-keys -t jetl C-z
# Virtual space: a zero-width block passing the short line, then typing
press 8 $((X+4)) 2; press 40 $((X+4)) 4; release 8 $((X+4)) 4   # a zero-width block at column 4, after the tab
/tmp/tm.sh send-keys -t jetl -l 'V'              # "ab" is padded with spaces to the column, V in line
/tmp/tm.sh send-keys -t jetl C-z                 # pad and V gone in one step
press 8 $((X+5)) 2; press 40 $((X+5)) 4; release 8 $((X+5)) 4   # a zero-width block at column 5, after `a`
/tmp/tm.sh send-keys -t jetl BSpace              # `a` gone on lines 1-2; "ab" kept, its caret one cell left
/tmp/tm.sh send-keys -t jetl DC                  # `b` gone on lines 1-2; "ab" not joined with line 4
/tmp/tm.sh send-keys -t jetl C-z                 # both lines back
press 8 $((X+4)) 2; press 40 $((X+4)) 4; release 8 $((X+4)) 4   # a zero-width block at column 4 again
/tmp/tm.sh send-keys -t jetl Tab                 # every caret at the next stop (column 8), "ab" padded
/tmp/tm.sh send-keys -t jetl C-z
press 8 $((X+5)) 2; press 40 $((X+5)) 4; release 8 $((X+5)) 4   # column 5 again
/tmp/tm.sh send-keys -t jetl Enter               # lines 1-2 split before `b`; "ab" breaks at its end, no spaces
/tmp/tm.sh send-keys -t jetl C-z
press 8 $((X+5)) 2; press 40 $((X+14)) 4; release 8 $((X+14)) 4   # a block from column 5 to past "x": cells past the ends selected
/tmp/tm.sh send-keys -t jetl Escape
/tmp/tm.sh send-keys -t jetl C-Home Right Right  # before `b` of line 1; column mode still on from above
for i in 1 2; do /tmp/tm.sh send-keys -t jetl S-Down; done
for i in $(seq 70); do /tmp/tm.sh send-keys -t jetl S-Right; done   # past the widest line; carets on one column
/tmp/tm.sh send-keys -t jetl M-S-IC              # column mode off again (status without " Column")
press 0 $((X+1)) 5; release 0 $((X+1)) 5         # a plain click on line 4
press 8 $((X+8)) 4; release 8 $((X+8)) 4         # Alt+click past the end of "ab": a caret drawn on column 8
press 8 $((X+16)) 2; release 8 $((X+16)) 2       # Alt+click past "x" on line 1: "Ln 1, Col 17 (3 carets)"
/tmp/tm.sh send-keys -t jetl -l 'Z'              # "ab" padded to column 8, line 1 to 16, then Z; Z on line 4
/tmp/tm.sh send-keys -t jetl C-z                 # pads and Z gone in one step, the carets back past the ends
press 8 $((X+8)) 4; release 8 $((X+8)) 4         # the same cell again removes that caret (2 carets)
/tmp/tm.sh send-keys -t jetl Escape
/tmp/tm.sh send-keys -t jetl M-S-IC              # column mode on
press 0 $((X+8)) 4; release 0 $((X+8)) 4         # a click past "ab": "Ln 3, Col 9 Column"
/tmp/tm.sh send-keys -t jetl -H 1b 5b 31 32 37 3b 35 75   # Ctrl+Backspace: the caret to the end of "ab" (Col 3), nothing deleted
/tmp/tm.sh send-keys -t jetl Right Right Right   # past the end again: Col 6
/tmp/tm.sh send-keys -t jetl Up Down             # line 2 at Col 6, then back on "ab" at Col 6
/tmp/tm.sh send-keys -t jetl -H 1b 5b 33 3b 35 7e         # Ctrl+Delete: nothing changes, "ab" not joined
/tmp/tm.sh send-keys -t jetl BSpace -l 'V'       # Col 5, then "ab  V"
/tmp/tm.sh send-keys -t jetl C-z
/tmp/tm.sh send-keys -t jetl M-S-IC              # column mode off (status without " Column")
/tmp/tm.sh send-keys -t jetl Home                # line 3, before "ab": "Ln 3, Col 1"
/tmp/tm.sh send-keys -t jetl -H 1b 5b 33 3b 35 7e         # Ctrl+Delete deletes the word: line 3 is empty
/tmp/tm.sh send-keys -t jetl C-z                 # "ab" back
```

`capture-pane -e` shows the selected cells in the selection background
(the whole 日 when the edge cuts it), the block's cells past each line end
in the same background, every caret on the block's head column (reverse
video, also after "ab") and, at 80x24, the view scrolled sideways to keep
the last Shift+Right in sight (at 120x40 the 70 steps fit without scrolling). Alt+click without moving (`press 8`, `release
8` on one cell) still adds a caret. `capture-pane -e` shows the Alt+clicked carets in
reverse video on their cells past the line ends. tmux does not send Ctrl+Backspace
for `C-BSpace` (it types the name as text): the scenario sends the escapes `ESC [ 127;5 u` and
`ESC [ 3;5 ~` with `-H`. The Ctrl+Delete in virtual space changes nothing, so the probe on line 3 (an empty line after it, "ab" back after Ctrl+Z) is what shows the escape reaches jetl. `-H 08` is not Ctrl+Backspace for jetl (the status column stays).

### Scenario: code folding

At 80x24 and at 120x40, on a Rust file (`jetl src/editor/fold.rs` from
the repository root works, the Project window hidden with Alt+1). `G` is the
1-based screen column of the fold marker (`G=10` past the tool strip and
a three-digit line number; the text starts at `G+2`); the first text line
is on row 2:

```sh
/tmp/tm.sh send-keys -t jetl C-g; /tmp/tm.sh send-keys -t jetl -l '38'; /tmp/tm.sh send-keys -t jetl Enter
/tmp/tm.sh send-keys -t jetl M-,                 # fold the body around the caret: `pub fn lines(…) -> … {…}`
/tmp/tm.sh capture-pane -t jetl -p               # ▸ in the marker column, the line numbers jump past it
/tmp/tm.sh send-keys -t jetl Down Up             # steps over the fold, back to the header's end
/tmp/tm.sh send-keys -t jetl Right               # into the fold: it opens
/tmp/tm.sh send-keys -t jetl 'M-<'               # collapse all: every body and comment run is one row
press 65 10 10; press 65 10 10                   # the wheel steps over folds
press 0 $((G+4)) 5; release 0 $((G+4)) 5         # a click below folds lands on the line shown there
press 0 $G 2; release 0 $G 2                     # the ▸ marker unfolds that line; again folds it
/tmp/tm.sh send-keys -t jetl 'M->'               # expand all
```

Line commands treat a fold as one line: with a function folded, Alt+Shift+Down
on the line above it jumps over the whole fold; Alt+Shift+Up on the folded
header moves the block up, still collapsed; Ctrl+D on the header copies the
block and the copy is collapsed too; Ctrl+Y deletes it whole; Ctrl+A leaves
the folds collapsed.

Then fold a function, click its placeholder (`…` after the header: it
unfolds with the caret at the header's end), fold it again and jump into
it with Ctrl+G (or F2 on a diagnostic inside): the fold opens. Split right
(`M-\`) and expand in one pane: the other keeps its folds. Ctrl+- and
Ctrl+= need the kitty protocol (tmux sends Ctrl+- as Ctrl+7, which toggles
a line comment); in foot or kitty check them and Ctrl+Shift+- (collapse
all) too.

### Scenario: soft wrap

At 80x24 and at 120x40, on a Markdown file with paragraphs longer than the
editor (`.md` opens wrapped), an indented one and a word longer than a row,
opened as `jetl $T/docs/notes.md` with the Project window hidden (Alt+1).
`X` is the 1-based screen column of the text's first cell (`X=11` past the
tool strip and the gutter; `↪` sits at `X-2` on continuation rows):

```sh
for i in 1 2 3 4 5; do /tmp/tm.sh send-keys -t jetl Right; done
/tmp/tm.sh send-keys -t jetl Down Down Down       # onto a paragraph's second row, same screen column
/tmp/tm.sh display -p -t jetl '#{cursor_x},#{cursor_y}'
/tmp/tm.sh send-keys -t jetl End                  # the row's last grapheme (its hanging space) ...
/tmp/tm.sh send-keys -t jetl End                  # ... then the line end, rows below
/tmp/tm.sh send-keys -t jetl Home Home Home       # the row's start, then smart Home: indent, column 1
press 0 $((X+9)) 6; release 0 $((X+9)) 6          # a continuation row: the caret lands on that row
press 0 79 6; release 0 79 6                      # past its text: on its last grapheme, same row
press 0 $((X-1)) 7; release 0 $((X-1)) 7          # the gutter: the row's first char
press 65 30 10; press 64 30 10                    # the wheel scrolls by rows
```

On the indented paragraph, continuation rows start under its text; a click
left of it goes to the row's start. Then open `src/main.rs` (code does not
wrap), F1 `soft-wrap` Enter: long lines wrap and the view never scrolls
sideways. Fold a body holding a wrapped line (`M-,`) and unfold it (`M->`):
the marker and placeholder stay on the header's first and last rows.
Alt+Shift+Insert shows "Column selection does not work with soft wrap".
Split right (`M-\`) and drag the bar: each pane breaks at its own width
and keeps its caret in view. Open the Project window: the editor re-wraps
to the narrower width.

### Scenario: conflict resolver

At 80x24 (two panes: yours and theirs share the left one) and at 120x40
(three panes), in a repository stopped on a merge conflict: two branches
change the same lines twice, and one of them also changes a line of its
own:

```sh
T=/tmp/jt-merge; rm -rf $T; mkdir -p $T && cd $T && git init -q
g() { git -c user.name=t -c user.email=t@t "$@"; }
printf 'fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n}\n' > a.rs
echo readme > README.md; git add . && g commit -qm init; main=$(git branch --show-current)
git switch -qc dev; sed -i 's/a = 1/a = 100/; s/c = 3/c = 30/; s/e = 5/e = 500/' a.rs; g commit -qam dev
git switch -q $main; sed -i 's/a = 1/a = 10/; s/e = 5/e = 50/' a.rs; g commit -qam main
g merge -q dev; cd -                              # CONFLICT (content): a.rs
```

Start jetl on `$T/README.md`, hide the Project window (Alt+1 twice: at
120 columns with it open the editor is under 90 columns and gets two
panes) and split right (`M-\`) so the layout to come back to has two
groups:

```sh
/tmp/tm.sh send-keys -t jetl F1; /tmp/tm.sh send-keys -t jetl -l 'resolve conflicts'
/tmp/tm.sh send-keys -t jetl Enter Enter Enter   # the action, a.rs, Merge…
/tmp/tm.sh capture-pane -t jetl -p               # header "a.rs  2 of 2 conflicts left [Accept Yours] …"; c = 30 already in
/tmp/tm.sh send-keys -t jetl M-S-Left            # yours on the first conflict: 1 of 2 left, « × stay to add theirs
/tmp/tm.sh send-keys -t jetl F7                  # the caret to the second conflict
press 0 $C 8; release 0 $C 8                     # the » on that row (C: its 1-based column, at the result pane's left edge)
/tmp/tm.sh send-keys -t jetl M-S-Right           # theirs after yours: both lines, "All changes resolved"
/tmp/tm.sh send-keys -t jetl M-Enter             # Apply (or click [Apply]): the split comes back, a.rs open
git -C $T status --short; git -C $T diff --cached  # M  a.rs, no markers
```

`capture-pane -e` shows conflicts in `merge_conflict_bg`, resolved changes in
`merge_resolved_bg`, and the splitter cells beside them in the same color.
At 80x24 `M-Tab` to the left pane and Alt+Right shows theirs. Then `git -C $T
merge --abort; git -C $T merge dev` (the watcher reloads), open the resolver
again, type into the result and press Esc: Cancel asks; Discard puts the split
back with nothing written (`UU a.rs`). With nothing typed, the header's
[Cancel] closes at once; Alt+Enter with conflicts left asks first. Typing into
yours or theirs changes nothing; Alt+\ and a file picked in Ctrl+N answer "Finish or cancel
the merge first".

Then `git -C $T merge --abort; git -C $T merge dev`, Alt+1 to the Project
window, select `a.rs`, Enter: the three panes open (Esc closes). Alt+9 to the
Git window, Commit tab: selecting `a.rs` (U) shows `Conflicted: Enter opens
the resolver` in the diff area, Enter opens the resolver. Ctrl+N `a.rs` still
opens the plain tab with the "has conflicts" notification.

### Scenario: rebase in progress

At 80x24 and at 120x40, in a repository stopped on a rebase conflict:

```sh
T=/tmp/jt-rebase; rm -rf $T; mkdir -p $T && cd $T && git init -q
git config user.name t; git config user.email t@t
echo base > a.txt; echo readme > README.md; git add . && git commit -qm init; main=$(git branch --show-current)
git switch -qc dev; echo dev > a.txt; git commit -qam 'dev 1'; echo two > b.txt; git add b.txt; git commit -qm 'dev 2'
git switch -q $main; echo main > a.txt; git commit -qam main; git switch -q dev
git rebase $main; cd -                          # CONFLICT (content): a.txt
```

Start jetl on `$T`. Alt+9 opens the Git window. Check:

- The header shows `Rebasing 1/2` after the tabs.
- The status bar shows it after the short sha.
- In the Log (Alt+L), Shift+F10 shows Checkout Revision, New Branch…, Cherry-Pick, Revert Commit… and Interactively Rebase from Here… greyed.
- Alt+` then Enter on the main branch lists no Checkout, Merge or Rebase.
- F10, G: Continue Operation, Skip Commit and Abort Operation are enabled. The header shows ` Continue  Skip  Abort `.
- Click Continue with `a.txt` still `U`: the dialog asks. Resolve… opens Resolve Conflicts; accept theirs.
- Click Continue, choose Stage and Continue: the rebase applies `dev 2` and ends with `Rebase finished`.
  `git -C $T log --oneline` shows `dev 2`, `dev 1`, `main`. The Git console shows
  `git add a.txt && git rebase --continue`. Local History on `a.txt` has a
  `Before git add a.txt && git rebase --continue` row.
- Then `git -C $T reset -q --hard ORIG_HEAD && git -C $T rebase $main` stops on the conflict again.
- Abort from the palette (F1, `abort operation`), then confirm: the branch is back
  (`git -C $T log --oneline -1` is `dev 2`).
- Local History (Alt+Shift+H on `a.txt`) has a `Before git rebase --abort` row.
- With a clean repository the palette does not list `continue operation` and the menu rows are greyed.

### Scenario: interactive rebase

At 80x24:

```sh
T=/tmp/jt-irb; rm -rf $T; mkdir -p $T && cd $T && git init -q
git config user.name t; git config user.email t@t
echo base > base.txt; git add . && git commit -qm init
for n in one two three four; do echo "body $n" > $n.txt; git add . && git commit -q -m $n -m "body $n"; done; cd -
```

Start jetl on `$T`. Alt+9 opens the Git window, Alt+L the Log. Check:

- Select `one`, press Alt+Shift+I: the dialog `Interactive Rebase from <sha>` lists `one`, `two`, `three`, `four`
  as `pick` rows, oldest first.
- `r` on `two` (the message box takes the focus and holds `two` and `body two`): replace it with a new subject,
  Tab back to the list, `s` on `three`, `d` on `four`, Enter. The status bar shows `Interactive rebase done`;
  `git -C $T log --oneline` shows the new subject over `one` and `init`, and `four.txt` is gone.
- Edit a tracked file and press Alt+Shift+I again: the warning `Commit or stash your changes before an
  interactive rebase`, no dialog.
- Restore the file, open the dialog again, `s` on the first row, Enter: `The first commit cannot be squashed or
  fixed up` shows in the dialog, which stays open; any key clears it; Esc cancels.

### Scenario: crash recovery

At 80x24 and at 120x40. The debug build is needed: `app.debug_panic`
("Debug: trigger a panic") panics only with debug assertions. jetl runs
in a shell that stays open, so the screen after the crash can be read:

```sh
T=/tmp/jt-rec; rm -rf $T /tmp/jt-home; mkdir -p $T && git -C $T init -q
printf 'one\ntwo\n' > $T/notes.txt; printf 'alpha\n' > $T/b.txt
cargo build --no-default-features --features lang-rust
run() { /tmp/tm.sh -f /dev/null new-session -d -s jetl -x 80 -y 24 \
  "XDG_CONFIG_HOME=/tmp/jt-home/config XDG_STATE_HOME=/tmp/jt-home/state $PWD/target/debug/jetl $T/notes.txt 2>/tmp/jetl-stderr.txt; echo EXIT=\$?; sleep 600"; }
run; sleep 2
/tmp/tm.sh send-keys -t jetl -l 'RECOVERED '
/tmp/tm.sh send-keys -t jetl F1; /tmp/tm.sh send-keys -t jetl -l 'trigger a panic'; /tmp/tm.sh send-keys -t jetl Enter
sleep 1; /tmp/tm.sh capture-pane -t jetl -p        # a plain shell screen (terminal restored), EXIT=101
cat /tmp/jetl-stderr.txt                           # the panic, then "jetl: unsaved changes of 1 buffer(s) saved to /tmp/jt-home/state/jetl/recovery/<pid>-<secs>; …"
cat /tmp/jt-home/state/jetl/recovery/*/manifest.json   # root, time, the notes.txt entry with "untitled": false
cat $T/notes.txt                                   # still one, two
/tmp/tm.sh kill-server; run; sleep 2
/tmp/tm.sh capture-pane -t jetl -p                 # "Recover Unsaved Changes" listing notes.txt, [ Restore all ] [ Discard ] [ Review… ]
/tmp/tm.sh send-keys -t jetl Enter                 # Restore all: *notes.txt, "RECOVERED one", an Info toast
cat $T/notes.txt; ls /tmp/jt-home/state/jetl/recovery/   # the file unchanged, no dump left
/tmp/tm.sh send-keys -t jetl C-z                   # back to the file's text, the tab clean
/tmp/tm.sh kill-server
```

Then repeat the crash with two modified files (type in notes.txt, Ctrl+N
`b.txt`, type there) and check, one start each:

- Review… (Tab Tab Enter): "Recover Unsaved Changes (1 of 2)" with the
  whole title, Restore / Skip per file (Esc skips); only the restored file
  opens modified; the dump is gone after the last one.
- Esc on the first dialog: nothing opens, a toast says the changes are kept,
  the dump stays and the dialog comes back on the next start. Ctrl+Q quits
  with EXIT=0 and leaves that dump alone (only its own directory goes).
- Discard (Tab Enter): nothing opens, the dump is gone.
- Pruning: a dump of another root older than 30 days goes at startup.
  Make one with `mkdir -p /tmp/jt-home/state/jetl/recovery/999999999-1`
  (no manifest, so the start time 1 in the name counts) and start once:
  the directory is gone, `jetl.log` says `pruned 1 crash dump(s)`.
- With a dump for `$T` present, start jetl on another project
  (`mkdir -p /tmp/jt-other; echo x > /tmp/jt-other/x.txt`, run it on
  `/tmp/jt-other/x.txt`): no dialog, and the `$T` dump is still there.

### Scenario: themes and 256 colors

At 80x24 and at 120x40, on the scratch project `$T` above. jetl reads
`$COLORTERM` itself, so set it (or unset it) on its command line:

```sh
mkdir -p /tmp/jt-home/config/jetl
printf '[ui]\ntheme = "light"\n' > /tmp/jt-home/config/jetl/config.toml
run() { /tmp/tm.sh -f /dev/null new-session -d -s jetl -x 80 -y 24 \
  "env $1 XDG_CONFIG_HOME=/tmp/jt-home/config XDG_STATE_HOME=/tmp/jt-home/state $PWD/target/debug/jetl $T"; }
run COLORTERM=truecolor; sleep 2
/tmp/tm.sh capture-pane -t jetl -p -e | grep -o '[34]8;2;[0-9;]*' | sort | uniq -c | sort -rn | head
                                                   # RGB codes; the editor background is 48;2;255;255;255
/tmp/tm.sh kill-server; run '-u COLORTERM'; sleep 2
/tmp/tm.sh capture-pane -t jetl -p -e | grep -c '[34]8;2;'   # 0: no RGB codes left
/tmp/tm.sh capture-pane -t jetl -p -e | grep -o '[34]8;5;[0-9]*' | sort | uniq -c | sort -rn | head
                                                   # 256-color codes; the editor background is 48;5;231
/tmp/tm.sh kill-server
```

In both modes, and with Darcula too (`theme = "darcula"`), look at the
Project tree, the Git window (Commit tab, the Log graph, a diff), the file
palette (Ctrl+N), Problems, a folded function and the conflict resolver
(the `/tmp/jt-merge` fixture above): every highlight readable, and diff and
merge chunks told apart. Then, one start each:

- `theme = "nope"`: a notification about the unknown theme, then GitHub
  Dark.
- No `theme` (or `theme = "auto"`) and no Omarchy palette under
  `/tmp/jt-home/state`: GitHub Dark (`48;2;13;17;23`).
- `theme = "mine.toml"` with `bg = "#202040"` in
  `/tmp/jt-home/config/jetl/mine.toml`: Darcula with that background
  (`48;2;32;32;64`).
- `theme.toml` with `selection_bg = "#ffcc00"` on `theme = "light"`: a
  selection (Shift+Right) shows `48;2;255;204;0`.
- `[ui] colors = "truecolor"` with COLORTERM unset: RGB codes again.

### Scenario: Omarchy theme

At 80x24 and at 120x40, on the scratch project `$T` above, with a fake
Omarchy state directory (the real desktop theme is left alone; Omarchy's
themes are in `~/.local/share/omarchy/themes/`):

```sh
O=/tmp/jt-home/state/omarchy/current; rm -rf /tmp/jt-home; mkdir -p $O/theme /tmp/jt-home/config/jetl
cp ~/.local/share/omarchy/themes/tokyo-night/colors.toml $O/theme/; echo tokyo-night > $O/theme.name
/tmp/tm.sh -f /dev/null new-session -d -s jetl -x 80 -y 24 \
  "env COLORTERM=truecolor XDG_CONFIG_HOME=/tmp/jt-home/config XDG_STATE_HOME=/tmp/jt-home/state $PWD/target/debug/jetl $T/src/main.rs"
sleep 2; /tmp/tm.sh capture-pane -t jetl -p -e | grep -c '48;2;26;27;38'   # Tokyo Night background #1a1b26
# Switch as omarchy-theme-set does: a new directory renamed over the old one, then theme.name.
mkdir $O/next-theme; cp ~/.local/share/omarchy/themes/gruvbox/colors.toml $O/next-theme/
rm -rf $O/theme; mv $O/next-theme $O/theme; echo gruvbox > $O/theme.name; sleep 1
/tmp/tm.sh capture-pane -t jetl -p | tail -1                 # Reloaded the Omarchy theme
/tmp/tm.sh capture-pane -t jetl -p -e | grep -c '48;2;40;40;40'     # Gruvbox background #282828
printf 'bg = "#202040"\n' > $O/theme/jetl.toml; echo gruvbox2 > $O/theme.name; sleep 1   # dark blue background
printf '[ui]\ntheme = "darcula"\n' > /tmp/jt-home/config/jetl/config.toml; sleep 1   # Darcula
/tmp/tm.sh kill-server
```

Check the same windows as in the themes scenario, also with COLORTERM
unset: every highlight readable.

### Scenario: config hot reload

At 80x24 and at 120x40, on the scratch project `$T` above, with an empty
config directory and a theme file directory outside it:

```sh
C=/tmp/jt-home/config/jetl; rm -rf /tmp/jt-home /tmp/jt-theme; mkdir -p $C /tmp/jt-theme
/tmp/tm.sh -f /dev/null new-session -d -s jetl -x 80 -y 24 \
  "env COLORTERM=truecolor XDG_CONFIG_HOME=/tmp/jt-home/config XDG_STATE_HOME=/tmp/jt-home/state $PWD/target/debug/jetl $T/src/main.rs"
printf '[ui]\ntheme = "light"\n' > $C/config.toml; sleep 1
/tmp/tm.sh capture-pane -t jetl -p | tail -1                 # Reloaded config.toml
/tmp/tm.sh capture-pane -t jetl -p -e | grep -c '48;2;255;255;255'   # white background
printf '[syntax]\nkeyword = "#ff0000"\n' > $C/theme.toml; sleep 1   # `fn` turns red: 38;2;255;0;0
printf '[editor]\n"f2" = "app.about"\n' > $C/keymap.toml; sleep 1   # [editor]: it binds F2 itself
/tmp/tm.sh send-keys -t jetl F2                               # "jetl 0.8.0"
printf '[editor\n' > $C/keymap.toml; sleep 1                  # error: keeping the current keymap; F2 still About
printf '[ui]\ntheme = "light"\nleft_width = 50\n[editor]\nsoft_wrap = true\n' > $C/config.toml; sleep 1
                                                              # warning names ui.left_width; main.rs stays unwrapped,
                                                              # a file opened now (Ctrl+N) wraps
printf 'bg = "#202040"\n' > /tmp/jt-theme/mine.toml
printf '[ui]\ntheme = "/tmp/jt-theme/mine.toml"\n' > $C/config.toml; sleep 1   # dark blue background
printf 'bg = "#402020"\n' > /tmp/jt-theme/mine.toml; sleep 1   # follows the new file: dark red
printf '[ui]\ntheme = "nope"\n' > $C/config.toml; sleep 1     # error, the theme stays
printf '[ui]\ntheme = "light"\n' > $C/.t && mv $C/.t $C/config.toml; sleep 1   # an atomic rename reloads too
/tmp/tm.sh kill-server
```

Then open File > Settings… and change `scrolloff` (see the Settings window
scenario below): "Reloaded config.toml". Run `touch $C/config.toml`: nothing.

### Scenario: Settings window

At 80x24 and at 120x40 (fresh `/tmp/jt-home` each time), on the scratch
project `$T`:

```sh
C=/tmp/jt-home/config/jetl; rm -rf /tmp/jt-home; mkdir -p $C
printf '# my settings\n[lsp]\nenabled = false\n[editor]\nscrolloff = 3   # caret margin\n' > $C/config.toml
/tmp/tm.sh -f /dev/null new-session -d -s jetl -x 80 -y 24 \
  "XDG_CONFIG_HOME=/tmp/jt-home/config XDG_STATE_HOME=/tmp/jt-home/state $PWD/target/debug/jetl $T/src/main.rs"
sleep 2; /tmp/tm.sh send-keys -t jetl C-M-s; sleep 0.3; /tmp/tm.sh capture-pane -t jetl -p   # the window, Appearance
/tmp/tm.sh capture-pane -t jetl -p -e | grep Apply           # [ Apply ] has a dim colour before it: nothing pending
```

- Down, Down, Enter (Editor > General), Down, Enter (scrolloff), BSpace, `8`,
  Enter: the row shows `*` and 8; `[ Apply ]` now has no colour code of its
  own (lit like `[ Cancel ]`).
- Ctrl+S: status "Reloaded config.toml" (cut to "Reloade…" at 80 columns);
  `cat $C/config.toml` keeps `# my settings` and `# caret margin` with
  `scrolloff = 8`.
- Mouse: a click on a tree row selects the page; a click on a Bool value
  (`[x]` of "Close brackets…") flips it and marks the row; two clicks on a
  number row open its value dialog. Esc closes the dialog, a second Esc with
  a pending change asks "Discard N changed settings?" ([ Discard ] / [ Keep
  Editing ]).
- Appearance > Left tool window width: Enter, BSpace x3, `40`, Enter, then a
  click on `[ Apply ]`: a Warning "Reloaded config.toml; restart jetl to
  apply ui.left_width"; the file gains `[ui]` `left_width = 40` after a blank
  line, the other lines unchanged.
- Narrow: `/tmp/tm.sh resize-window -t jetl -x 34 -y 20` with the window open shows only the frame with ` Settings `; `-x 40` keeps the buttons and the help lines inside the right border; Esc with a pending change at 40x12 shows the discard confirm inside the screen; `-x 8 -y 4` with the confirm open does not crash (`/tmp/jetl-stderr.txt` has no panic). Opened at 80x24, note the row and column of `[ OK ]`, then `resize-window -x 30 -y 24` (frame only) and click that cell (`press 0 <col> <row>; release 0 <col> <row>`): the window stays open with only its frame.
- Esc closes the window; F10 > File > Settings… (click the row) opens it.
  `touch $C/config.toml`: no new notification.
- `/tmp/tm.sh kill-server`.

### Scenario: numbered bookmarks

At 80x24 and at 120x40, on the scratch project `$T` above with an 80-line
`docs/notes.md` (`seq 1 80 | sed 's/^/line /' > $T/docs/notes.md`), opened as
`jetl $T/docs/notes.md`. tmux does not pass Ctrl+digit, so the jumps use the
legacy fallback Alt+B then the digit:

```sh
/tmp/tm.sh send-keys -t jetl C-g; /tmp/tm.sh send-keys -t jetl -l 20; /tmp/tm.sh send-keys -t jetl Enter
/tmp/tm.sh send-keys -t jetl C-F11      # Numbered Bookmark: ten digit buttons inside the box, [ 1 ] selected
/tmp/tm.sh send-keys -t jetl 3          # the gutter of line 20 shows 3 (reversed: capture -e has ESC[7m)
/tmp/tm.sh send-keys -t jetl C-n; /tmp/tm.sh send-keys -t jetl -l main.rs; /tmp/tm.sh send-keys -t jetl Enter
/tmp/tm.sh send-keys -t jetl M-b 3      # back in notes.md, Ln 20
/tmp/tm.sh send-keys -t jetl C-M-Left   # main.rs again
/tmp/tm.sh send-keys -t jetl M-b 3 C-w  # close notes.md
/tmp/tm.sh send-keys -t jetl M-b 3      # notes.md reopens at Ln 20 with its 3
/tmp/tm.sh send-keys -t jetl S-F11      # the list shows "3 docs/notes.md:20 line 20"
/tmp/tm.sh send-keys -t jetl Escape C-w C-q
```

Start jetl again on `$T` (notes.md closed): Alt+B 3 opens it at line 20
with the 3 in the gutter. Ctrl+F11 then 3 on that line removes the
bookmark; 3 on another line moves it there.

### Scenario: breakpoints in the gutter

No debugger is needed. At 80x24 with `MODE=plain` and at 120x40 with `MODE=ext`
(the `extended-keys on`, `extended-keys-format csi-u` settings of a user's tmux
config, set directly), each run from a fresh scratch project: `notes.log` (30
lines `line N`), `fold.log` (`head`, two indented lines, `last`) and `doc.md`,
with `[lsp] enabled = false` and `[ui] toolbar = false` in `config.toml`.
`/tmp/tm24.sh` is `exec /usr/bin/tmux -L jetl24 "$@"`; `click` sends an SGR
mouse press and release at a 1-based column and row.

1. In a pane running `cat -v`, `send-keys C-F8` then Enter shows `^[[19;5~`.
2. `jetl notes.log`, Ctrl+F8: `●` right after the number of `line 1`; Ctrl+F8
   again: gone.
3. Go to line 5 (Ctrl+G), Ctrl+F8: `●` on line 5, status bar `Ln 5,`.
4. Click the mark cell of `line 8` (the cell right after the number): `●` on
   line 8, the status bar still `Ln 5,`, nothing selected; the same click
   removes it, once more sets it.
5. Click the number's last digit on `line 12`: the caret goes to line 12, no
   glyph.
6. Go to line 1, Enter: the marks are on lines 6 and 9; Ctrl+Z: back on 5 and 8.
7. On line 5, F11 adds a bookmark: line 5 still shows `●`; Ctrl+F8 removes the
   breakpoint and `◆` shows; Ctrl+F8 brings `●` back. `capture-pane -e` shows
   the `●` with a foreground other than the number's.
8. Open `fold.log`, Ctrl+F8 on line 2, go to line 1, Alt+, (collapse): row 1
   reads `1● ▸ head…`; Alt+. expands it and `●` is on line 2.
9. Close `notes.log` (Ctrl+W) and open it again: `●` on lines 5 and 8. Close it,
   Ctrl+Q: exit status 0, empty stderr, and the session file under
   `$XDG_STATE_HOME/jetl/sessions` lists `notes.log` in `closed_breakpoints`
   with lines 4 and 7 and `fold.log` with `breakpoints` `[1]`; the word
   `Verified` is nowhere in it.
10. Start `jetl $T` and open `notes.log`: `●` on 5 and 8 (over the bookmark
    on 5); quit; the file is now under `files` with `breakpoints` 4 and 7.
    Start again: the tabs of the last session come back with their marks.
11. Open `doc.md`, Alt+Shift+V (preview), Ctrl+F8: a toast mentioning
    `Markdown preview`, no glyph.

`tmux send-keys` enters at tmux, so this does not prove the hop from a physical
keyboard through a terminal emulator: press Ctrl+F8 once in foot or kitty as
well, inside and outside tmux.

### Scenario: debug adapters in the doctor

No debugger runs: `--doctor` only looks the commands up (`pgrep -x gdb` gives
the same before and after). At 80x24 and at 120x40, from a fresh `$R`: `proj/a.c`,
`xdg/{config/jetl,state,cache}` with `[lsp] enabled = false` and `[ui] toolbar = false`
in `config.toml`, and `proj/.jetl/run.toml` holding a `# top` comment, `[[configs]]`
`demo` (`command = "echo hello"`), a `# how to debug it` comment and `[debug.demo]`
with `adapter = "gdb"` and `launch = { program = "${workspaceFolder}/demo" }`. Every
run sets `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` under `$R/xdg`.

1. `(cd $R/proj; env $ENVV jetl --doctor | sed -n '/^Debug adapters/,/^Formatters/p')`:
   the group sits between `Language servers` and `Formatters`, with `missing codelldb`
   (VSIX hint), `missing lldb-dap` (LLVM hint), `ok gdb /usr/bin/gdb (c, cpp, rust)`
   and `unknown debugpy python3 found, debugpy not checked (python)` when `gdb` and
   `python3` are installed and the others are not. Stderr is empty. The same inside a
   tmux pane of the size under test.
2. Append `[debug.adapters.codelldb]` with `command = []` and `[debug.adapters.mine]`
   with `command = ["nonexistent-dap"]`, `languages = ["go"]` to `config.toml`: `mine`
   comes first as `missing`, `nonexistent-dap not found (go)`; no `codelldb` row.
   Restore the file.
3. `jetl a.c`, Ctrl+Alt+S, Down to `Run` (under Tools): rows `Run tasks`, `Run configurations`,
   `Debug blocks`, `Debug adapters`, the last one ending in `next debug session`. Tab, three
   Downs: the help reads `[debug.adapters.<name>]`. Shift+Tab, End: the Dependencies page has the
   `Debug adapters` heading with the rows of step 1. Escape.
4. With jetl running, append `[debug.adapters.gdb]` and `languages = ["c"]` to `config.toml`
   from outside: the status says `Reloaded config.toml`, with no restart warning.
5. Alt+Shift+F10 lists `demo`; Enter runs it and the Run window shows `hello`. `.jetl/run.toml`
   is unchanged.
6. F1, `edit configurations`, Enter, Tab, Down twice, Enter, type `foo bar`, Enter, Ctrl+S:
   `.jetl/run.toml` has `args` for `demo` and everything from `# how to debug it` on is the
   block as written, comment included.
7. Change the `launch` line to `launch = "x"`, Alt+Shift+F10: the picker lists `demo` with no
   warning, and Enter runs it. A debug block of the wrong shape does not affect Run.

Each run exits 0 with an empty stderr on Ctrl+Q.

### Scenario: Debug and Stop

Needs `gdb` 14 or newer (`gdb -i=dap`) and `gcc`; nothing else is installed. At 80x24 and
at 120x40, with `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` under `/tmp` and
`[lsp] enabled = false`, `[ui] toolbar = false` in `config.toml`. Fixture `/tmp/jetl-dap-demo`:
`demo.c` (an `add(int, int)` called from `main`, which prints `x=3`), built with
`gcc -g -O0 -o demo demo.c`, and `.jetl/run.toml` with `[[configs]] name = "demo"`,
`command = ["./demo"]` and `[debug.demo]` with `adapter = "gdb"` and
`launch = { program = "${workspaceFolder}/demo" }`. A second fixture, `/tmp/jetl-dap-sleepy`, is
a program named `jetl-dbg-sleepy` that calls `sleep(300)`, with the same kind of `run.toml`. Count
processes with `pgrep -x jetl-dbg-sleepy` and `pgrep -f '[g]db -q -i=dap'` (the brackets keep the
shell's own command line out of the match), and start only when both print nothing.

1. F10, `r`: the Run menu has `Debug  Shift+F9` and `Stop Debugging  Ctrl+F2` after `Stop`, inside
   the screen. Escape twice.
2. In `demo`: Shift+F9. Toasts `Debugging demo with gdb`, then `Debug session ended, exit code 0`
   (no breakpoint is marked, so the program runs to its end). No `gdb` is left. Ctrl+F2 with
   nothing live reaches Run: `No task is running`.
3. Rewrite `.jetl/run.toml` and press Shift+F9 each time: without a `[debug.demo]` table,
   `No debug configuration: add [debug.<name>] to .jetl/run.toml`; with `adapter = "codelldb"`, the
   toast says `codelldb not found` with its install hint; with `launch = "x"`, a toast with
   `launch must be a table` (Run does not show it); with `launch = { program = "...", env = "x" }`, an
   error row in the Event Log (Alt+5) with gdb's text and then `Debug session ended`, and no `gdb` left.
4. A `cwd` with a line break: `launch = { program = "...", cwd = "/tmp\nshell touch /tmp/pwned" }`
   gives `Cannot debug 'demo': line break in its working directory` (all of it in the toast at 80x24, on
   two lines), starts no `gdb`, and `/tmp/pwned` does not exist.
5. A program that does not exist: `launch = { program = "${workspaceFolder}/nope" }`. gdb answers the
   launch with success and idles. The toast `The program did not start: check launch.program and
   launch.cwd` comes once, and the session stays live (one `gdb`) until Ctrl+F2:
   `Debug session stopped`, no `gdb` left.
6. In `/tmp/jetl-dap-sleepy`: Shift+F9 shows one `jetl-dbg-sleepy` and one `gdb` (the positive
   control: the counts above are read here, while it runs, before any stop). Ctrl+F2: `Debug session
   stopped` and neither is left. Shift+F9 twice in a row: the second replaces the first (another pid,
   one of each). Ctrl+F2.
7. Run the sleepy configuration with Shift+F10 as well, then Shift+F9: two `jetl-dbg-sleepy` and one
   `gdb`. The first Ctrl+F2 ends the session only (one `jetl-dbg-sleepy` is left, the Run task); the
   second ends the task.
8. Shift+F9, then `kill -9` the `gdb`: the toast is `Debug session ended: adapter exited`, and after two
   seconds no `jetl-dbg-sleepy` is left.
9. Shift+F9, then Ctrl+Q: jetl exits 0 with an empty stderr, and neither process is left.
10. An adapter that never answers: Shift+F9 on the sleeping program, read `State` and `TracerPid` in
    `/proc/<debuggee>/status` (`S (sleeping)`, the gdb's pid), `kill -STOP` the `gdb`, then Ctrl+F2.
    Observed with gdb 17.2: `Debug session stopped` after 4.4 s (the two stop stages of 2 s, then the
    kill), and neither process is left a moment later: killing gdb takes its debuggee, which gdb traces.
11. The same with Ctrl+Q instead of Ctrl+F2: jetl exits 0 with an empty stderr after 4.1 to 4.4 s,
    and neither process is left.

Each run exits 0 with an empty stderr on Ctrl+Q. What gdb cannot show, and the unit tests prove
instead: the other two start orders (js-debug's and delve's), a root switch and a replaced session's silence,
and the debugpy command (debugpy is not installed).

### Scenario: A program that did not start

Needs the fixture of `### Scenario: Debug and Stop` (`/tmp/jetl-dap-demo`, gdb 17.2), at 80x24 and at
120x40, with `RUST_LOG=jetl=debug` (the log is `$XDG_STATE_HOME/jetl/jetl.log`; warnings are logged there
as well as shown). Add to the fixture: `where.c`, a `main` that prints `cwd=` and `getcwd`'s result,
built `gcc -g -O0 -o where where.c`; a copy of `demo` with the execute bit removed (`noperm`); an empty
directory `sub`; `/tmp/jetl-dap-sleepy/jetl-dbg-sleepy` as in the scenario above; and `slowsh` outside the
repo, mode 0755: `#!/bin/sh`, `sleep 3`, `exec /bin/sh "$@"`. Start only when `pgrep -f '[g]db -q -i=dap'`
prints nothing. Rewrite `.jetl/run.toml` (one `demo` configuration and one `[debug.demo]` table) before each
press of Shift+F9, and read the toasts and the Event Log (Alt+5). Only gdb is checked: other adapters get
neither toast and were not tried.

1. `launch = { program = "${workspaceFolder}/nope" }`: the Warning `The program did not start: check
   launch.program and launch.cwd` comes once, all of it on screen at 80x24 (two lines). The log holds one
   `asking once more` and one `dap: the program did not start`, and one `gdb` is alive. Ctrl+F2: `Debug
   session stopped`, no `gdb` left.
2. `program = "${workspaceFolder}"` (a directory): the same toast and counts. Ctrl+F2.
3. `program = "missing-here", cwd = "nodir"`: both toasts (`Working directory not found, so gdb stays in
   the project root: nodir` first, the cwd as written). Ctrl+F2.
4. `program = "${workspaceFolder}/where", cwd = "${workspaceFolder}/nodir"`: the cwd toast with the
   absolute path, no `did not start` (the log has no `dap: the program did not start`), and the Debug
   window's Console says `cwd=/tmp/jetl-dap-demo`: gdb stayed in the project root and ran the program there.
5. The same with `cwd = "~"`: neither toast, the Console says your home directory. With `cwd = "sub"`:
   neither toast, `cwd=/tmp/jetl-dap-demo/sub`.
6. The false-positive guard: `program = "${workspaceFolder}/demo"`, no breakpoint, 10 starts in a row, each to
   `Debug session ended, exit code 0`: no `did not start` in the log. Then 20 more under CPU load (one busy
   loop per core in its own process group, killed by group id afterwards). `asking once more` counts the
   race (an empty first answer, the `process` event landing just after it); the unit tests cover that path.
   Observed here: 0 in 30 starts, so the race was not hit under real gdb.
7. Slow start: start jetl with `SHELL=<path to slowsh>`. `demo`, no breakpoint: no toast during the 3 s wait
   or after (the log shows `threads` answered 3.01 s after the `launch` answer, with the `process` event
   behind it), and `exit code 0`. Then `jetl-dbg-sleepy`: no toast; Ctrl+F2 ends it.
8. With the normal `SHELL`: `jetl-dbg-sleepy` for 5 s: no toast. `demo` with a mark on line 9: `paused
   (breakpoint)` in the Debug window's title, no toast; F9 to `exit code 0`.
9. A copy of `demo` without the execute bit as `program`: the shell's `permission denied` (`zsh:1:
   permission denied: .../noperm`) in the Console and the session ends with `exit code 0`, with no `did not
   start`. This case is not silent already, and gdb reports exit code 0 for it.
10. Ctrl+Q: exit 0, empty stderr, no `gdb` or debuggee left.

What gdb cannot show, and the unit tests prove instead: a failed `threads`, a user's Stop between the two
rounds, an `exited` with no `process`, a `process` with no `name`.

### Scenario: jetl killed with gdb stopped

Needs the setup of `### Scenario: Debug and Stop` (gdb 17.2, `gcc`, `XDG_*` under `/tmp`, `[lsp] enabled = false`),
and a copy of the sleepy fixture built with `-g` (`gcc -g -O0 -o jetl-dbg-sleepy sleepy.c`, `sleep(300)` on line 3),
with a `.jetl/run.toml` for it. At 80x24, with jetl started by `exec env ... jetl sleepy.c` in a tmux session on a private
socket, so the pane's pid is jetl's (check `/proc/<pid>/comm`). Start only when `pgrep -x jetl-dbg-sleepy` and
`pgrep -f '[g]db -q -i=dap'` print nothing. Use pids in every `kill`, never a pattern.

1. The cell: Shift+F9, wait for `pgrep -x jetl-dbg-sleepy`, take the gdb pid from `pgrep -f '[g]db -q -i=dap'`, and
   record `State` and `TracerPid` in `/proc/<pid>/status` of both (the debuggee reads `TracerPid` = the gdb's pid).
   For a stopped gdb: `kill -STOP <gdb>`, wait 0.2 s. Then `kill -<signal> <jetl>`, wait 2 s and read both again. For
   a stopped gdb, `kill -CONT <gdb>`, wait 2 s and read again.
2. The matrix, 12 cells: the debuggee running, and paused at a breakpoint (`Ctrl+G`, `3`, Enter, Ctrl+F8 on the `sleep`
   line before Shift+F9; wait for `paused (breakpoint)`), crossed with gdb normal and gdb stopped, crossed with `KILL`,
   `TERM` and `HUP`. Every cell reads both pids gone at +2 s, with the pids recorded before the kill.
   Before this change a stopped gdb stayed `T` and its debuggee `S`, traced by it, until the `kill -CONT`, for all
   three signals (measured under jetl with the debuggee running; `t` when paused comes from a probe with another parent). One more cell at 120x40: gdb stopped, running, `KILL`, the same reading.
3. An idle session stays: Shift+F9 and nothing else for 20 s, past the blocking pool's 10 s idle keep-alive. gdb and the
   debuggee still run, and `TracerPid` is unchanged. Ctrl+F2: `Debug session stopped`, neither left. If gdb dies
   here, `debug_launch` is no longer on the main loop's thread.
4. A normal quit: Shift+F9, Ctrl+Q: exit 0, empty stderr, neither left. Then steps 8 to 11 of `### Scenario: Debug and
   Stop` as before (step 8's toast wraps at 80 columns: `Debug session ended: adapter` and `exited` on the next
   line).
5. Another adapter is not tied: in `config.toml`, `[debug.adapters.notgdb]` with `command = ["sleep", "317"]` and
   `languages = ["c"]`, and `adapter = "notgdb"` in `.jetl/run.toml`. Shift+F9, then `kill -9 <jetl>`, wait 2 s:
   `pgrep -f '^sleep 317$'` still prints the pid. Kill it by that pid.
6. Nothing is left: `pgrep -x jetl-dbg-sleepy`, `pgrep -f '[g]db -q -i=dap'` and `pgrep -f '^sleep 317$'` print
   nothing, and the tmux socket has no server.

What gdb cannot show, and the unit tests prove instead: the tie itself is a kernel rule about the forking thread.
lldb-dap, codelldb and debugpy were not tried.

### Scenario: breakpoints sent to a debug session

Needs `gdb` 14 or newer and `gcc`. At 80x24 and at 120x40, with `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and
`XDG_CACHE_HOME` under `/tmp`, `[lsp] enabled = false` and `[ui] toolbar = false` in `config.toml`, and
`RUST_LOG=jetl=debug`: the steps read `$XDG_STATE_HOME/jetl/jetl.log`, only what a step added, and wait for it,
since the log is written off the main thread. Fixture `/tmp/jetl-dap-demo`, rebuilt each run: `demo.c` as in the
Debug and Stop scenario with one more line, a comment with no code on line 9 (between `int x = add(1, 2);` and
the `printf`); `other.c`, a file that is not part of the program (`int unused(void) { return 1; }`, three lines);
`demo` built with `gcc -g -O0 -o demo demo.c`; the same `.jetl/run.toml`. Start only when `pgrep -x demo` and
`pgrep -f '[g]db -q -i=dap'` print nothing. Line numbers below are as the gutter shows them (1-based); the log
shows 0-based lines. A toast (4 s Info, 8 s Warning, 12 s Error) keeps right of the gutter in the default layout, so a glyph can be read
under one; code to its right can be covered.

1. Negative control: `jetl demo.c`, Shift+F9 with no mark. The program runs to its end (`Debug session ended,
   exit code 0`), the log has `dap: breakpoints at start: {}` and no `-> setBreakpoints`.
2. Ctrl+G, `4`, Ctrl+F8: `4` shows `●`. Shift+F9: the Debug window's title reads `paused (breakpoint)` (no toast). In the log, `-> setBreakpoints` comes
   before `-> configurationDone` and appears once, a mark is `Pending` (gdb: `reason: Some("pending")`) before it is
   `Verified, adapter line None` after gdb's `breakpoint` events (three came in the capture; the count is not
   asserted). The pane shows `4●` and no `4○`. One `demo` is running, stopped.
3. Ctrl+G, `10`, Ctrl+F8 while paused: the log has `demo.c: [3, 9], modified false` once.
4. A line with no code: Ctrl+F8 on `10` again (back to `[3]`), then Ctrl+G, `9`, Ctrl+F8: `demo.c: [3, 8], modified
   false`, and the mark is `Verified, adapter line Some(9)`: gdb placed it on line 10, and the glyph stays on `9●`,
   not `10●`.
5. Ctrl+G, `1`, Enter: the marks move and the new lines go out, `demo.c: [4, 9], modified true`; the pane shows `5●` and
   `10●`. Ctrl+Z: `demo.c: [3, 8], modified false`, and `4●`, `9●`.
6. Positive control for `○`: Ctrl+N, `other.c`, Enter; Ctrl+G, `2`, Ctrl+F8. `other.c: [1], modified false` and `other.c:1
   is Pending` (gdb never finds the source, `reason: Some("pending")`, `line: None`). The pane shows `2○`, and still
   does a second later.
7. Ctrl+F8 again on `other.c`: `other.c: [], modified false` once, and no `2○` or `2●`.
8. Ctrl+F8 (`2○` again), then Ctrl+W: no `dap: sending breakpoints` line, since the file's marks stay in the session.
   Ctrl+N, `other.c`, Enter: `other.c: [1], modified false` again and `2○` back, not a bare `●`.
9. Ctrl+F2: `Debug session stopped`; `2○` is gone (`2●`), every state back to the default; no `demo` or `gdb` is left.
10. Open `demo.c`, close it with Ctrl+W (its marks stay), Shift+F9: the log's `breakpoints at start` names `demo.c`,
    the program stops there (`<- event stopped`) and the closed file's marks were sent. Ctrl+N, `demo.c`, Enter while
    paused: `demo.c: [3, 8], modified false` and `4●` with the state `Verified`.
11. Ctrl+Q with the session live: exit 0, empty stderr, no `demo` or `gdb`, and no `Verified` or `Pending` in the
    session files under `$XDG_STATE_HOME/jetl/sessions`: state is never saved. Start again on `demo.c`, Shift+F9: the
    saved marks are sent and the program stops (`4●`). Ctrl+F2.

That a cleared breakpoint no longer stops the program is shown under gdb in the next scenario; the unit tests prove
only that the empty list goes out, before a resume. What gdb cannot show, and the unit tests prove: `reason:
"failed"` (needs an internal gdb error); a `removed` event (gdb suppresses it for its own deletes); the other two
start orders; a closed file's lines dropped, and a rename; `sourceModified` having an effect (gdb ignores it); a
root switch.

### Scenario: stepping and the execution point

Needs `gdb` 14 or newer and `gcc`. At 80x24 and at 120x40, with the directories, config and log of the previous
scenario (`RUST_LOG=jetl=debug`, `[lsp] enabled = false`, `[ui] toolbar = false`), a private tmux socket, and the same
precondition on `pgrep`. Fixture `/tmp/jetl-dap-demo`, rebuilt each run: `demo.c` and `demo` as in the first
scenario of this section (line 9 is `int x = add(1, 2);`, line 10 is `int y = add(x, 4);`, line 11 is the `printf`);
`spin.c` (`volatile unsigned long n = 0; for (;;) { n++; }`, built as `spin`); `.jetl/run.toml` with the configs
`demo` and `spin` and a `[debug.<name>]` table for each; `git init` with `demo.c` and `spin.c` committed. A step
reads the pane only with no toast on it (a toast can cover code, not the gutter in the default layout), and counts the `▶` on the screen:
exactly one is the pass.

1. Negative control: `jetl demo.c`, Shift+F9 with no mark: the program runs to its end and the pane never shows a
   `▶`.
2. Ctrl+G, `9`, Ctrl+F8, Shift+F9: the title `paused (breakpoint)`, `9▶` (the line's background is not in a text capture; the
   unit snapshots show it), one `demo` running. The log has `dap: execution point .../demo.c:8` (0-based). The mark does not show under the arrow.
3. F8: `-> next` in the log, `10▶`, and `9●` is back. F7 on line 10 (`add(x, 4)`): `-> stepIn`, `4▶`, gdb stops on
   `demo.c:3` (0-based), the line of `int sum`. Shift+F8: `-> stepOut`, `10▶` again.
4. Ctrl+G, `1`, Enter, then Alt+F10: the caret is on line 10 again and the arrow is still there. F9: `-> continue`,
   `exit code 0`, no `▶`, no `demo` or `gdb` left.
5. A cleared breakpoint is not hit: marks on 9 and 11, Shift+F9, F9 (stops at `11▶`), Ctrl+F2. Shift+F9 again, stop on
   9, Ctrl+G, `11`, Ctrl+F8 to clear it, wait for `demo.c: [8], modified false` in the log, F9: the program ends, no
   `<- event stopped` after the F9. The same with no wait: Ctrl+G, `11`, Ctrl+F8, F9 sent together by one `send-keys`
   call: it ends too, since F9 flushes the list before `continue`.
6. Terminal: stopped on 9, Alt+F12, type `cat -v`, Enter, F8: the terminal shows `^[[19~` and the log has no
   `-> next`. Ctrl+C, Alt+F12, Ctrl+F2.
7. Git window: stopped on 9, Alt+9, F7: no `-> stepIn` (F7 is the Git next change there), F8: `-> next` and the arrow
   moves to 10. F7 again: still no `-> stepIn`. Esc, Ctrl+F2.
8. Pause: open `demo.c`, Ctrl+G, `9`, Ctrl+F8 (the mark on 9 is there from the earlier steps; this clears it), open `spin.c`, Alt+Shift+F10, pick `spin` (this starts a Run task: Ctrl+F2
   stops it, Alt+4 hides the Run window, whose toolbar draws a `▶` of its own; press Alt+4 twice if the window is
   still up), Shift+F9. F1, `Debug: Pause`, Enter: `-> threads` then `-> pause` in the log, the title `paused (pause)`
   (gdb's reason is `pause`), and the `▶` on the `spin.c` line `n++;` or the `for`. F9 and the arrow goes; F1,
   `Debug: Pause` again: `-> pause` with no new `-> threads`. Ctrl+F2.
9. F7 outside a pause: in the editor with no session, F7 moves to the next change (a line was edited: type a space
   at the end of line 12, Ctrl+G, `1`, Enter, F7 puts the caret on line 12) and sends nothing. Ctrl+Z.
10. Ctrl+Q while paused: exit 0, empty stderr, no `demo` or `gdb` left.
11. A refused step, program still stopped. Fixture `/tmp/jetl-dap-outer`: `outer.c` is
    `void _start(void) {` / `    volatile int n = 1;` / `    n = n + 1;` /
    `    __asm__("mov $60, %eax; xor %edi, %edi; syscall");` / `}`, built with
    `gcc -g -O0 -static -nostdlib -o outer outer.c`, and a `.jetl/run.toml` like the demo's. `jetl outer.c`,
    Ctrl+G, `3`, Enter, Ctrl+F8, Shift+F9: the title `paused (breakpoint)`, `3▶`. Shift+F8: `-> stepOut`,
    `<- response stepOut success false`, `-> stackTrace`, `<- response stackTrace success true` (the check; its `levels` 1
    is shown by the unit tests only, the log does not print arguments), a toast `stepOut: "finish" not meaningful in the
    outermost frame.` (it wraps at 80 columns), then the stop's own `-> stackTrace` and `-> threads` and `dap: execution point`.
    The title is `paused (breakpoint)` again, one `▶` on line 3 once the toast is gone. F8: `-> next`, `4▶`.
    F9: `exit code 0`, no `gdb` left.
12. A refused step, program not stopped. In `/tmp/jetl-dap-demo`, `launch = { program =
    "${workspaceFolder}/demo", stopOnEntry = true }`: Shift+F9, the toast `Paused in`, no source. Shift+F8: a
    toast `stepOut: Command aborted.`, `-> stackTrace` once and `<- response stackTrace success false` in the log (the
    log does not print the text, `notStopped`; no `dap: execution point` follows), the title says `running`, not `paused`,
    and no toast starts with `Could not read`. Ctrl+F2: `Debug session stopped`, no `gdb` or `demo`
    left. Restore `.jetl/run.toml`.

gdb can show: the arrow and the line on every stop, the three steps, the return with Alt+F10, the pause, a
cleared breakpoint that does not stop the program, and a refused step-out in both outcomes. gdb cannot show, and the unit tests prove: a refused `next`, `stepIn` or `continue` (gdb answers the first two before trying them), a stop with no
`threadId` (gdb always sends one), a frame with no source or with a `sourceReference` (gdb has the source here), a
stale answer to `stackTrace` after a step, the other two start orders, a `continued` event, a failed `stackTrace`, an
empty stack or thread list, the point clearing on a new start or a root switch, and the other keys of the Git, Local
History and merge windows. The hop from a physical keyboard through the terminal emulator into tmux stays the
user's to confirm (`send-keys` enters at tmux). Bare `F9` is taken by Omarchy's voxtype binding when voxtype is
installed (it is not on the test machine); `Run > Resume` stays.

### Scenario: a late file read for a stop

Needs `gdb` 14 or newer and `gcc`. Directories, config, log and `pgrep` precondition as in `### Scenario: stepping and
the execution point`, at 80x24 and at 120x40, with fresh `XDG_STATE_HOME` and `XDG_CACHE_HOME` before each step (the
marks are saved on quit, and a second Ctrl+F8 on a restored mark would clear it). A FIFO source holds the read open for
as long as the test wants, so the race is not left to chance. Fixture, rebuilt each run: `/tmp/jetl-dap-late/lib/helper.c`
is `int helper(int a) {` / `    return a + 1;` / `}`; `/tmp/jetl-dap-late/app/main.c` is `int helper(int);` /
`int main(void) {` / `    int x = helper(1);` / `    int y = x + 1;` / `    return y - 3;` / `}`; in `app`,
`gcc -g -O0 -o main main.c /tmp/jetl-dap-late/lib/helper.c` (so that the debug info names `helper.c`) and a
`.jetl/run.toml` with `[debug.main]` and `launch = { program = "${workspaceFolder}/main" }`. Then keep the text in
`lib/helper.real`, `rm lib/helper.c` and `mkfifo lib/helper.c`. The source lies outside the root so that nothing indexes
the FIFO. A blocked read holds a blocking thread: always feed the FIFO before quitting.

1. A late read after a later stop. From `app`, `jetl main.c`, Ctrl+G, `3`, Enter, Ctrl+F8, Ctrl+G, `4`, Enter, Ctrl+F8,
   Shift+F9: `3▶`. F7: the log has `dap: execution point .../helper.c:1`, no `helper.c` tab appears, `main.c` stays up.
   F9: `4▶`. `cat lib/helper.real > lib/helper.c` from another shell, wait one second: no `helper.c` tab, `4▶`, the log
   has `dropping the late read of`. F9: `exit code 0`, no `gdb` or `main` left. Recreate the FIFO.
2. Positive control, a read that is still current. As step 1 up to F7, then feed the FIFO: a `helper.c` tab opens and
   is active with its `▶` on line 2. F9 (`4▶`), F9 again: `exit code 0`. Recreate the FIFO.
3. A late read after Stop. As step 1 up to F7, Ctrl+F2 (`Debug session stopped`, no `gdb` or `main` left), then feed the
   FIFO: no `helper.c` tab, the log has `dropping the late read of`. Ctrl+Q: exit 0, empty stderr.

gdb can show: a read overtaken by a later stop and by Stop, and a current read. Only the unit tests show: the frame and
thread choices, the out-of-order landing, the user's pending Go To and where the focus goes.

### Scenario: Debug window and Console

Needs `gdb` 14 or newer and `gcc`. At 80x24 and at 120x40, a private tmux socket, `XDG_*` directories under `/tmp`,
`[lsp] enabled = false` and `[ui] toolbar = false`, `RUST_LOG=jetl=debug` (step 11 uses `jetl=info`, since debug
logging writes a line per event and would measure the log). Fixture `/tmp/jetl-dap-demo`: `demo.c` as in the first
scenario of this section plus a last `printf("café 😀 see demo.c:9 here\n");` (so lines 9 and 10 stay the calls);
`spin.c`; `flood.c` (`for (int i = 0; i < 100000; i++) printf("flood %d\n", i);`); `spew.c` (`for (;;) puts("spew");`);
`moved.c` (`int main(void) {`, `int a = 1;`, a comment-only line 3, `return a;`, a comment-only line 5, `}`); all built with `gcc -g -O0`;
`.jetl/run.toml` with a configuration and a `[debug.<name>]` table (adapter `gdb`) for each, and `typo`, whose
`program` is `${workspaceFolder}/no-such-program`. Start only when `pgrep -x 'demo|spin|flood|spew|moved'` and
`pgrep -f '[g]db -q -i=dap'` print nothing. A step reads the pane only with no toast on it. Alt+Shift+F10 (the
configuration picker) also starts a Run task, so after choosing press Ctrl+F2, wait until its process is gone, and hide
the Run window with Alt+4 (it replaced the Debug window in the dock; Alt+F5 brings the Debug window back). A mouse
press is `ESC [ < 0 ; col ; row M` then `m`, sent with `tmux send-keys -H`; the column counts a wide character as two
cells. The script of the last run is `~/.claude/plans/backlog-run-24-partC-S7-fix-evidence/scripts/d4a2-scenario.sh`.

1. Before any start the Debug window is not shown.
2. Ctrl+G, `9`, Ctrl+F8, Shift+F9: the window appears with the title `Debug: demo · paused (breakpoint)`, `9▶`, no
   `Paused (` toast anywhere, and the caret still in the editor (`Ln 9,`).
3. F8, F7, Shift+F8 step as in the stepping scenario; F9 ends the program. The Console, on the same screen, holds
   `x=3 y=7` and the `café 😀` line, and the title ends `ended`. gdb's `No source file named ...` and
   `Breakpoint 1 (...) pending.` are not there; the log holds them after `dap: not shown` (yman 427).
4. A click on `demo.c:9` in the Console puts the caret on `Ln 9,` of `demo.c` (Ctrl+G, `1` first). By key: Alt+F5
   focuses the window, End selects the last line, Up as often as the capture shows lines below the link, Enter.
5. Alt+F5 with the window shown and the editor focused focuses it; again hides it; again shows it. With the window hidden,
   Shift+F9 stops at the breakpoint and the window stays hidden; a `Paused (breakpoint)` toast says so (the Event Log
   keeps it) and `9▶` shows once it is gone; Ctrl+F2. With the window shown there is no such toast, the title has the
   reason.
6. Alt+F5 (shown and focused), Shift+F9, then F8: `-> next` in the log and `10▶`: the stepping keys work with the
   window focused. Ctrl+F2, Esc.
7. Buttons by mouse. At a stop, a click on ` Pause ` sends nothing (greyed); Step Over, Step Into, Step Out send
   `next`, `stepIn`, `stepOut`; Resume ends the program; at another stop Stop gives `Debug session stopped`. On `spin`:
   `running`, a click on Resume sends nothing, Pause sends `pause`, the title reads `paused`, Stop stops it.
8. A breakpoint on a comment-only line (`moved.c`, line 3) stops at line 4 (`4▶`) and the log says `Verified, adapter
   line Some(3)`. The Console writes `Breakpoint moved.c:3 moved to line 4` once, although gdb sends the `breakpoint`
   event three times (the log shows three `dap: breakpoint changed`): gdb answers `setBreakpoints` with `pending` and
   places the line in the event. A breakpoint on the blank line 7 of `demo.c` stays pending and the program runs to its
   end; the Console then has `Breakpoint demo.c:7 not placed: no code loaded for this line`, once, written at the
   program's end (or at the first stop, with a second mark on 9: before F9). A click on `demo.c:7` in that line puts the
   caret on `Ln 7,`. A mark that stays pending when Ctrl+F2 ends the session before any stop (`spin.c`, blank line 8)
   writes no such line. A plain start (one mark on `demo.c` line 9, Shift+F9, F9) writes no `moved`, `not placed` or
   `No source file named` line at all.
   While `moved.c` is paused at line 4, Ctrl+F8 on its comment-only line 5 sends `setBreakpoints` (`[2, 4]` in the log)
   and gdb answers it `verified` on line 6 (the log: `mark ...moved.c:4 is Verified, adapter line Some(5)`); the Console
   gets `Breakpoint moved.c:5 moved to line 6`, once. So gdb does answer a `setBreakpoints` sent while the program is
   paused, and the verdict line from a `setBreakpoints` answer runs against a real adapter. The program stops at 6 on
   the first F9 and ends on the second (`moved` returns 1: `exit code 1`).
9. A mistyped `program` (`typo`): the window stays `running` with `No output yet` until Ctrl+F2 (yman 416).
10. Marks back on `demo.c`, Shift+F9, Ctrl+Q while paused: exit 0, empty stderr, no `demo` or `gdb` left.
11. Restart with `RUST_LOG=jetl=info`. `flood`: from Shift+F9 to the toast `exit code 0`, then until `flood 99999` is the
    Console's last line: that last interval must be at most 5 s (observed 0.01 s to 0.12 s; the first is 1.3 s to 6.6 s). `spew`: after 3 s of output Alt+F5 and Alt+F5 hide the window within 1 s (observed at once), Alt+F5 shows
    it, Ctrl+F2 gives `Debug session stopped` within 10 s (observed 4.0 s to 4.2 s) and no `spew` or `gdb` is left.
    `ps -o rss=` of jetl: about 35 to 40 MB while spewing and after (37 MB and 38 MB in yman 425's run; the figure written here before was 100 to 125 MB).

gdb cannot show, and the unit tests prove: `stderr` and `important` styling (gdb's stderr arrives as `stdout`), a partial
last line (gdb is line-based), `telemetry`, a `Failed` verdict's message, a `removed` event's line, a `breakpoint` event for a mark in a
second file, a pending breakpoint placed later by a library load, stale `DebugOutput`, a root switch, the other start orders. Alt+F5 from a physical
keyboard through the terminal emulator into tmux stays the user's to confirm.

### Scenario: Console pressure

Under real `gdb` 17.2 (yman 425), release build (`CARGO_PROFILE_RELEASE_LTO=off` is faster to build; the figures hold either way). Needs at least 7000 MB available (`free -m`): a run can take 3 GB in jetl and 3 GB in gdb, and `/tmp` is tmpfs. The figures move by a factor of two with the machine's load. A run is one session of jetl on a pty whose master a script reads, once per second noting `VmRSS` and `VmHWM` of jetl and the state of its main thread; `stall` stops reading the master, which blocks jetl's terminal write exactly as a frozen ssh session does. `probe.py` and `sum.py` of that run are in `~/.claude/plans/backlog-run-25-S5b-evidence/`. Fixtures, each `/tmp/<dir>/p.c` built with `gcc -g -O0 -o p p.c`, with `.jetl/run.toml` holding `[[configs]] name = "p"`, `command = ["./p"]` and `[debug.p]` with `adapter = "gdb"`, `launch = { program = "${workspaceFolder}/p" }`:

- `spew5`: `int main(void){for(;;)puts("spew");}`
- `long16k`: `static char b[16384];`, `int main(void){memset(b,'x',sizeof b);b[sizeof b-1]='\n';for(;;)fwrite(b,1,sizeof b,stdout);}`
- `long1mnl`: `static char b[1<<20];`, `int main(void){memset(b,'y',sizeof b);b[sizeof b-1]='\n';for(;;){fwrite(b,1,sizeof b,stdout);fflush(stdout);}}`
- `uniq`: `int main(void){for(long i=0;;i++)printf("x%ld.c:1\n",i);}`
- `uniqlong`: `int main(void){long i=0;for(;;){int n=0;while(n<16000)n+=printf("x%ld.c:1 ",i++);putchar('\n');}}`
- `flood` as in the scenario above.

Figures, jetl's `VmHWM` unless noted, 120x40 (the `long16k` stall also at 80x24), after the gate and before it:

| Fixture, mode | After | Before |
|---|---|---|
| `long16k` stall 30 s | 307 MB, flat (80x24: 282 MB, in 23 s of the stall, when gdb reached the guard) | 3857 MB, over the guard after 20 s of the stall |
| `long16k` free 30 s | 317 MB, flat in the last 10 s | 2654 MB |
| `long1mnl` stall 20 s | 293 MB | about 3 GB (designer's run) |
| `spew5` stall 30 s | 82 MB, `resume_now` key latency 23 ms | 678 MB, 159 ms |
| `uniqlong` free 20 s | 200 MB | about 1.4 GB (designer's run) |
| `uniq` free 30 s | 38 MB | 123 MB, both from about 4 s after Ctrl+F2: gdb holds this output until then, so jetl sees none of it in the run |
| `spew5` free 30 s | 34 MB | 37 MB |
| `flood` | 35 MB | not run |

Each run: Ctrl+F2 until gdb is gone at most 5 s, Ctrl+Q exits 0. `of output dropped` appears in the Console in the `long16k` and the `spew5` stall runs and in neither `flood` nor `spew5` free.

Steps, at 80x24 and 120x40, a private tmux socket and `XDG_*` under `/tmp`, `RUST_LOG=jetl=info`:

1. Alt+Shift+F9 chooses `flood`, Shift+F9: `flood 99999` is the Console's last line within 5 s of the exit toast, and no `of output dropped` is on screen.
2. `spew`: after 3 s of output Alt+F5 hides the window at once; Alt+F5 shows it; Ctrl+F2 gives `Debug session stopped` within 10 s and no `spew` or `gdb` is left. jetl stays near 40 MB.
3. `long16k`: Shift+F9, then poll `capture-pane` every 100 ms for up to 60 s until `of output dropped` is there (it can be on screen only for a moment: the Console shows the end of each 50 ms batch). The row starts with a number of bytes, is in the warning colour (`capture-pane -e`: the colour of `important` text) and at 80x24 the words `of output dropped` fit. Ctrl+F2 gives `Debug session stopped`, Ctrl+Q exits 0 with empty stderr, and no `gdb` or `long16k` is left. jetl stays near 250 to 320 MB.

What the gate cannot do, by design: gdb's own memory grows 50 to 67 MB/s under a printing program (2 GB in 30 s for `spew5`) whatever jetl does, so a probe that runs `spew5` or `long16k` for half a minute can pass the 3 GB guard in gdb (27.5 s from the session's start, once); read `VmHWM` of jetl, not the guard. Unit tests prove the rest: the gate's limit and hysteresis, the notice's order and text, the give-back of a hidden line, the matcher's budget. The fake adapter stands in for none of the figures above.

### Scenario: Debug window, threads and frames

Needs `gdb` 14 or newer (the run used 17.2) and `gcc`. At 80x24 and at 120x40, a private tmux socket, `XDG_*`
directories under `/tmp`, `[lsp] enabled = false` and `[ui] toolbar = false`, `RUST_LOG=jetl=debug`. The fixture is the
one of the previous scenario plus `split.c` and `helper.c` (`main` calls `helper`, which is in the other file; `gcc -g -O0
-o split split.c helper.c`) and `threads.c` (a `pthread_create` of a `worker` that prints on line 5 and a `pthread_join`;
`gcc -g -O0 -pthread`), each with a configuration and a `[debug.<name>]` table. The steps run in a session that starts
with an empty `XDG_STATE_HOME`: the window layout, the open tabs and the breakpoints are kept there, so a session that
inherits them starts with the Debug window up and the breakpoint on line 9 toggled off by the step that sets it. A
background is read from `tmux capture-pane -e` (`bgof`): the SGR background in force where a text starts. The script of
the last run is `~/.claude/plans/backlog-run-24-D4b1-evidence/scripts/frames-scenario.sh`; it runs the previous
scenario's steps first.

1. Fresh state, `demo.c`, window hidden. Alt+F5: ` Frames ` and ` Console ` on a row of their own and the hint
   `Shift+F9 starts the selected configuration`. A click on ` Frames ` keeps the hint; a click on ` Console ` shows the Console.
2. Ctrl+G 9, Ctrl+F8, Shift+F9: `paused (breakpoint)`, `9▶`. The Console is still the tab. It shows `No output yet`: the two
   lines gdb gives for a breakpoint before the program loads are not shown (yman 427). The log holds
   one `-> stackTrace` and one `-> threads`, and `dap: threads [Thread { id: 1, name: "demo" }]`: gdb names the thread
   after the program.
3. Alt+F5 (focus), Alt+Right: `Thread 1: demo`, `main`, `demo.c:9`. F7: title `paused (step)`, `4▶`, rows `add demo.c:4`
   and `main demo.c:9`. gdb stops the list at `main`.
4. Down: `Ln 9,`, `Step Over` still up, and the background of `int x = add(1, 2);` differs from that of `int y =
   add(x, 4);` (observed `48;2;15;58;90` against `48;2;13;17;23` in the default theme).
5. Up: `Ln 4,`, and the two backgrounds are equal again. Down, Enter, then Up: `Ln 8,`, so the editor had the focus.
6. Alt+F5, Down, Alt+F10: `Ln 4,` and the mark gone. Alt+F5, Down, F8: `-> next`, `5▶`, the rows show `demo.c:5` and the
   mark is gone. F9: `exit code 0`, Frames shows `Frames show while the program is paused`, Alt+Left shows `x=3 y=7`.
7. `split` with `helper.c` open and a breakpoint on its line 2, `split.c` not open (checked on the tab row; the Project
   tree lists it): `2▶`; Frames: `helper helper.c:2`, `main split.c:3`. Down opens the tab `split.c` at `Ln 3,` and the
   focus stays in the window: Up gives `Ln 2,`.
8. Mouse: a click on the `main` row gives `Ln 3,`; a click on the thread row opens a menu with one row, Esc closes it.
9. `threads`, breakpoint on line 5, Shift+F9: `paused (breakpoint)`, `5▶`, and the log lists `Thread { id: 1 }` and
   `Thread { id: 2 }`, both named `threads`. The worker, thread 2, is the one stopped. `t` opens a menu of two rows. Down,
   Enter chooses thread 1: the log has a second `-> stackTrace`, and the rows are three `???  no source` frames and `main
   threads.c:12`. Three Downs reach `main`: `Ln 12,`, and `5▶` is still there. F9 with thread 1 chosen: `exit code 0`.
10. Frames selected, window hidden, `demo` breakpoint on 9: `Paused (breakpoint)` toast, then `9▶`, window still hidden.
11. Paused on Frames, Ctrl+F2: `Debug session stopped`, and Frames shows its paused hint. Ctrl+Q: exit 0, empty stderr, no
    `demo`, `split`, `threads` or gdb left.

At 80x24 the editor shows only a few lines, so the rows of steps 4 to 6 may be scrolled out; the script scrolls the editor
with the mouse wheel until both show.

gdb cannot show, and the unit tests prove: a frame whose source has a `sourceReference` (`a_frame_with_no_file_moves_nothing`),
a stop with no `threadId`, the stale choice (no way to hold an answer back), a failed stack, frame ids other than gdb's.

### Scenario: Text after a VS16 emoji

Needs `gdb` 14 or newer (the run used 17.2) and `gcc`. At 80x24 and at 120x40, a private tmux socket, `XDG_*`
directories under `/tmp` (an empty `XDG_STATE_HOME`), `[lsp] enabled = false` and `[ui] toolbar = false`,
`RUST_LOG=jetl=debug`. The fixture is a directory with `.jetl/run.toml` holding `[[configs]] name = "demo"`,
`command = ["./demo"]` and `[debug.demo]` with `adapter = "gdb"` and `launch = { program = "${workspaceFolder}/demo" }`,
and `demo.c`, built with `gcc -g -O0 -o demo demo.c`:

```c
#include <stdio.h>
int main(void) {
    puts("\xe2\x9d\x8c\xef\xb8\x8f boom abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz");
    fflush(stdout);
    int x = 3;
    printf("x=%d\n", x);
    return 0;
}
```

The long line runs past the right edge at both sizes, so a run of cells that lands too far right reaches the border. Tabs
are clicked as in the previous scenario (`ESC [ < 0 ; col ; row M` then `m`, sent with `tmux send-keys -H`). Nothing may
run from `pgrep -f '[g]db -q -i=dap'` or `pgrep -x demo` first.

1. Start jetl with `demo.c` as its path argument. Ctrl+G `5`, Ctrl+F8, Shift+F9: `paused (breakpoint)`, and `5▶` (at 80x24 only once the Warning
   toast over line 5 has gone).
2. Click ` Console `. It holds the fixture's `❌️ boom ...` line. That
   row has its closing border in the same column as the row above, and the emoji is visible.
3. Click ` Frames `. The thread row reads `Thread 1: demo ▾` with nothing to its right up to the border. No stray letter
   anywhere in the window.
4. Click ` Console `, then ` Frames `, then ` Console `, capturing each: as in 2 and 3.
5. F9: the program ends, `x=3` in the Console, borders intact. Ctrl+Q: exit 0, empty stderr, no `gdb` and no `demo` left.

Before the fix (`JetlBackend`, `src/tui.rs`) a Console row with the emoji could lose its closing border at both sizes and
show its text shifted by a column, and Frames kept a stray letter right of the thread row. Only tmux 3.7c was run: other terminals are
unverified. A Run window line with the same emoji is not scripted; the fix is below the panels. The script of the last
run is `~/.claude/plans/backlog-run-25-S2a-evidence/scripts/vs16-scenario.sh`.

### Scenario: Debug window, variables

Needs `gdb` 14 or newer (the run used 17.2) and `gcc`. At 80x24 and at 120x40, a private tmux socket, `XDG_*`
directories under `/tmp`, `[lsp] enabled = false` and `[ui] toolbar = false`, `RUST_LOG=jetl=debug`. The fixture is the
one of the previous scenario plus `vars.c` (a `struct point` holding a `struct inner` and a `const char *`, an `int[3]`,
an `int[300]`, a `char[]` with `é` and an emoji, a `char[400]` of `x`, a `union`, a null `void *`, and a call
`add(1, 2, &p, p, arr)` on line 19; `gcc -g -O0`) with its configuration and `[debug.vars]` table. The Variables
section runs in a session that starts with an empty `XDG_STATE_HOME`, after the previous scenario's steps. The script of
the last run is `~/.claude/plans/backlog-run-24-D4b2-evidence/scripts/vars-scenario.sh`. A row is matched by its shape
(`│ +x = -?[0-9]+  int`), since the source pane holds text such as `int x = add(1, 2);`.

1. `demo`, breakpoint on line 9, Shift+F9: `paused (breakpoint)`, `9▶`; the Console is still the tab. The log holds one
   `-> stackTrace` and no `-> scopes`.
2. Alt+F5 (focus), Alt+Right twice (Console, Frames, Variables): `Locals` open with `x = <number>  int` (uninitialised,
   read), `Registers` closed (no `rip` row). The log holds exactly one `-> scopes`.
3. F7 into `add`: `Arguments` open with `a = 1  int` and `b = 2  int`, `Locals` with `sum`, and no `x = ` row.
4. Enter on the first row closes `Arguments` (`a = 1` gone, `Arguments` stays), Enter again opens it from the kept
   children. The editor stays at `Ln 4,`.
5. Alt+Left to Frames, Down (`main`), Alt+Right: `Locals` with `x = `, no `a = 1`, the editor at `Ln 9,`. Three `-> scopes`
   in all since the stop. The log does not show the `frameId`; the unit test `the_tab_asks_scopes_of_the_selected_frame`
   covers it.
6. F8: `-> next`, `paused (step)`, `5▶`, `sum = 3  int`.
7. Ctrl+F2: `Debug session stopped`, and the tab says `Variables show while the program is paused`.
8. `vars`, breakpoint on line 19, Shift+F9, still on Variables: `Locals` with `p  struct point`, `arr  int [3]`,
   `big  int [300]` (these three with a `▸`), `wide = "café 😀"  char [11]`, `longs = 'x' <repeats 200 times>...  char [400]`, `uu`,
   `nul = 0x0  void *`, `r = <number>  int`, and `Registers`. gdb gives `main` no `Arguments` scope. The row of `p` has
   a name and a type and no ` = `.
9. Down, Right on `p`: `x = 1`, `y = 2`, `in  struct inner` and `name = "zed\033[31m\nline2"` (gdb writes the escapes as
   text, so the value holds no control character), `▾` beside `p`.
10. Three Downs to `in`, Right: `deep = 3  int`; Left closes it. Up three times to `p`, Left: `x = 1` gone.
11. Down to `arr`, Right: `0 = 7`, `1 = 8`, `2 = 9`. Left, then three Downs to `longs`: the row ends with the border.
12. Four Downs to `Registers`, Right: `▾ Registers`. The mouse wheel over the list scrolls it (six turns at both sizes to
    `rip = 0x... <main+239>  void (*)()`, which has a `▸`; three back to `rax = ...  int64_t`).
13. A click on the marker of `Registers` closes it: no `rax` row.
14. Ctrl+F2, then Alt+F5 to hide the window and Shift+F9 again: the `Paused (breakpoint)` toast, `19▶`, one `-> stackTrace`
    and no `-> scopes` while the window is hidden. Alt+F5 shows it and `-> scopes` follows within a second, with `Locals` drawn.
15. Ctrl+F2, Ctrl+Q: exit 0, empty stderr, no `demo`, `vars` or gdb left.

At 80x24 the list holds about ten rows, so the lower rows of steps 8 to 13 are scrolled; the script goes by key and by
the mouse wheel and says so in `v-m-wheel.txt`.

gdb cannot show, and the unit tests prove: a scope with `expensive: true` (gdb marks none), a refusal above `UNPAGED_LIMIT` and an adapter that ignores `start` and `count` (fake adapter only; gdb honours them), a raw control character in a value, a stale answer's timing (the
race is too short to place by hand), an answer over `MAX_BODY`, and a scope with no hint. Not run: scopes of a `???` frame.

### Scenario: Debug window, paged variables

Needs `gdb` 14 or newer (the run used 17.2), `gcc`, `g++` with libstdc++'s printers
(`/usr/share/gdb/auto-load/usr/lib/libstdc++.so.*-gdb.py`). At 80x24 and at 120x40, a private tmux socket, `XDG_*`
directories under `/tmp`, and a `config.toml` of:

    [lsp]
    enabled = false
    [ui]
    toolbar = false
    [debug.adapters.gdb]
    command = ["gdb", "-q", "-iex", "set debug dap-log-file <capture dir>/dap.log", "-i=dap"]

Wire proof: jetl's own log does not show a request's arguments, and gdb's `dap-log-file` does, with `READ: <<<...>>>`
for what jetl sent and `WROTE: <<<...>>>` for what gdb answered. Read the sent ones with
`grep -E '^READ.*"command": "variables"' dap.log | grep -o '"arguments": {[^}]*}'`, and the sizes with
`awk '/^WROTE/ && /"variables"/ {print length($0)}' dap.log`. With this `command` the pattern
`pgrep -f '[g]db -q -i=dap'` matches nothing, so count with `pgrep -f '[d]ap-log-file <capture dir>'`. Check it prints 1
during a session before trusting a 0 after.

Fixture in `/tmp/jetl-dap-demo`, built each run. `paged.c`, built `gcc -g -O0 -o paged paged.c`, line 15 being the
`printf`:

    #include <stdio.h>

    int main(void) {
        static int giant[2000000];
        static int huge[200000];
        int big[5000];
        int over[1001];
        int edge[1000];
        int i;
        for (i = 0; i < 5000; i++) big[i] = i;
        for (i = 0; i < 1001; i++) over[i] = i;
        for (i = 0; i < 1000; i++) edge[i] = i;
        huge[199999] = 7;
        giant[1999999] = 7;
        printf("%d\n", big[1] + over[1] + edge[1] + huge[199999] + giant[1999999]);
        return 0;
    }

`mapped.cc`, built `g++ -g -O0 -o mapped mapped.cc`, line 7 being the `printf`:

    #include <cstdio>
    #include <map>

    int main() {
        std::map<int, int> m;
        for (int i = 0; i < 100000; i++) m[i] = i;
        printf("%zu\n", m.size());
        return 0;
    }

`.jetl/run.toml` is rewritten to one configuration per session: `[[configs]] name = "paged"`,
`command = ["./paged"]`, `[debug.paged]`, `adapter = "gdb"`, `launch = { program = "${workspaceFolder}/paged" }`, and the
same for `mapped`.

Keys: open the file as jetl's argument, Ctrl+G with the line and Enter, Ctrl+F8, Shift+F9, then Alt+F5 and Alt+Right
twice for Variables. Reach a row by Home and then Down, one per row. Left collapses an open row, but on a closed row it
moves to the parent, so a loop that goes on with Down from there returns to the same row: always go from Home. A row is
matched by its shape (`[▸▾] big`), since the source pane holds the same names. A collapsed row keeps its
`first N of M shown` note.

1. `paged`, breakpoint on line 15, Shift+F9: `paused (breakpoint)`, `15▶`. The `initialize` line in `dap.log` holds
   `"supportsVariablePaging": true` and `"supportsVariableType": true`. gdb's answer to it holds no
   `supportsVariablePaging`.
2. Variables: `Locals` open with `giant  int [2000000]`, `huge  int [200000]`, `big  int [5000]`, `over  int [1001]`,
   `edge  int [1000]` (each with a `▸`), `i = 1000  int`, and `Registers` closed (the last two are below the fold at
   both sizes until End).
3. `giant`, Right: `▾ giant  first 1000 of 2000000 shown  int [2000000]` and rows `0 = 0  int`, `1 = 0  int`. Sent:
   `{"count": 1000, "start": 0, "variablesReference": N}`. Answer 71 008 characters. Without the capability this answer
   would be about 156 MB (ten times the 15 577 896 of the 200 000 element frame; not run), over `MAX_BODY`, and it would end the session. Left.
4. `huge`, Right: `first 1000 of 200000 shown  int [200000]`, the same request shape, answer 71 008. Left.
5. `big`, Right: `first 1000 of 5000 shown  int [5000]`, rows `0 = 0  int`, `1 = 1  int`. Sent paged, answer 72 898
   characters (372 898 before this change). Left.
6. `over`, Right: `first 1000 of 1001 shown  int [1001]`. Sent paged, answer 72 898 (72 973 before). Left.
7. `edge`, Right: `edge  int [1000]` with no note. Sent `{"variablesReference": N}`, with no `start` or `count`, answer
   72 898. Left.
8. F8, then `big` again, Right: `paused (step)`, a fresh tree (every container closed by the `Left`s above, so not
   reopened, and without a note), then `first 1000 of 5000 shown`, and no `could not read` or `list index out of range`
   anywhere. Sent paged, answer 72 898. `dap.log` holds `variables` for Locals only until the Right, then `big` with
   `"start": 0`.
9. Ctrl+F2, Ctrl+Q, rewrite `run.toml` for `mapped`, and start jetl anew on `mapped.cc`. Breakpoint on line 7, Shift+F9,
   Variables, `m`, Right: `▾ m … = std::map with 100000 elements` during the wait, then
   `first 1000 of 200000 shown = std::map with 100000 elements`, and rows `[0] = 0  const int`, `[1] = 0  int`. Sent
   paged, answer 77 788. Observed: 2.4 s under load at both sizes. A figure above the 10 s of `REQUEST_TIMEOUT` would
   show `could not read: timed out`.
10. Ctrl+F2, Shift+F9 again, Variables (the tab stays), `m`, Right, and Ctrl+F2 at once while `…` shows:
    `Debug session stopped` within about 2 s, and no gdb left.
11. Ctrl+Q: exit 0, empty stderr, no gdb or debuggee left.

What gdb cannot show, and the unit tests prove: a refusal above `UNPAGED_LIMIT`, and an adapter that ignores `start` and
`count`. Other adapters are unverified.

### Scenario: Debug window, load more

Needs what `### Scenario: Debug window, paged variables` needs: `gdb` 14 or newer (the run used 17.2), the same
`config.toml` with `dap-log-file`, the wire proof, the keys, `paged.c` and `mapped.cc`. One more fixture, `pages.c`,
built `gcc -g -O0 -o pages pages.c`, line 6 being the `printf`:

    #include <stdio.h>
    struct pt { int x; int y; };
    int main(void) {
        static struct pt pts[3000];
        pts[1500].x = 42;
        printf("%d\n", pts[1500].x);
        return 0;
    }

The list holds about ten rows at 80x24, so go to the load-more row from the end: End, then Up once per row below the
container (the roots and locals after it, and `Registers`, which is last). In `paged.c` with only `big` open that is End
and Up four times (`over`, `edge`, `i`, `Registers`); with only `over` open, three; with only `giant` open, six. Sent
requests: `grep -E '^READ.*"command": "variables"' dap.log | grep -o '"arguments": {[^}]*}'`.

`paged`, breakpoint on line 15, Shift+F9, Variables, at each size:

1. `big`, Right: `▾ big  first 1000 of 5000 shown  int [5000]`. End, Up four times: the selected row reads
   `load 1000 more  4000 left`, on one line at 80x24.
2. Enter: the selected row reads `1000 = 1000  int`, on the screen row the load-more row held, and the row above it is
   `999 = 999  int`. Sent: `{"count": 1000, "start": 1000, "variablesReference": N}`.
3. End, Up four times, Right: `2000 = 2000  int`. Sent `start` 2000. Left: the selection is on
   `▾ big  first 3000 of 5000 shown  int [5000]`.
4. End, Up four times, then a click on the row's text, not its marker: `3000 = 3000  int`. Sent `start` 3000. The click
   was column 15, row 18 at 80x24 and column 15, row 34 at 120x40 (1-based; the text starts at column 11, 0-based).
5. End, Up four times: the row reads `load 1000 more` with no `left`. Enter: `4000 = 4000  int`. Sent `start` 4000.
   End, Up four times: `4999 = 4999  int` with no load-more row after it. Home, Down to `big`: `▾ big  int [5000]` with no note.
6. Left on `big`, Right: the rows return and `dap.log` holds no new `variables` line. Left.
7. `over`, Right: `first 1000 of 1001 shown`. End, Up three times: `load 1 more`. Enter: `1000 = 1000  int`, no row after
   it, and `over` with no note. Sent `start` 1000. Left twice from there closes `over`.
8. `giant`, Right. End, Up six times: `load 1000 more  1999000 left`. Two Enters in one `send-keys`: `dap.log` holds
   exactly one new `"start": 1000` (2 lines before, 3 after), the row `1000 = 0  int` is selected, and `giant`'s row
   reads `first 2000 of 2000000 shown`.
9. A failed page. End, Up six times (`load 1000 more  1998000 left`). Find gdb with
   `pgrep -f '[d]ap-log-file <capture dir>'` and `kill -STOP` it. Enter: `reading 1000 more …`. After 9.72 s at 80x24
   and 9.67 s at 120x40 (the limit is `REQUEST_TIMEOUT`, 10 s): `load 1000 more  could not read: timed out`, the selection on
   the row, no toast. `kill -CONT` it and wait 2 s: the screen is identical, since the late answer is dropped. Enter:
   `2000 = 0  int`. `dap.log` holds `"start": 2000` three times in all, one of them for `big`; `giant`'s note reads
   `first 3000 of 2000000 shown`, not 4000.
10. A step with a page out. End, Up six times, then Enter and F8 in one `send-keys`: `paused (step)`, a fresh tree with
    `giant` open again on its first page, `first 1000 of 2000000 shown`, sent `"start": 0` and no `"start": 1000`, and
    nowhere `could not read`, `invalid variablesReference` or `list index out of range`. Home, Down, Left closes it
    again, so that `big` is the third row. `big`, Right: `first 1000 of 5000 shown`, rows `0 = 0  int`, `1 = 1  int`.
11. A stop with a page out. End, Up four times, then Enter and Ctrl+F2 in one `send-keys`: the title reads
    `Debug: paged · ended` and the pane `Variables show while the program is paused`; no gdb is left (0 after the stop).

`mapped`, breakpoint on line 7, at each size:

12. `m`, Right (about 2 s at `…`: 2.0 s at 80x24, 1.99 s at 120x40): `first 1000 of 200000 shown = std::map with 100000
    elements`. End, Up once: `load 1000 more  199000 left`. Enter: `[1000] = 500  const int`, then `[1001] = 500  int`,
    after 0.12 s at both sizes (a later page has no printer cost). Sent `start` 1000. The count is rows, twice the
    elements, as the note says. Left: `first 2000 of 200000 shown`. Ctrl+F2.

`pages`, breakpoint on line 6, at each size:

13. `pts`, Right: `first 1000 of 3000 shown`. End, Up once: `load 1000 more  2000 left`. Enter, then
    `tmux send-keys -N 500 Down`: `1500  struct pt`. Right, then Down twice: `x = 42  int`, `y = 0  int`. A child on a
    later page opens (the request for its reference followed the two `start` requests).
14. Ctrl+F2, Ctrl+Q: exit 0, empty stderr, no gdb or debuggee left.

Last run: `~/.claude/plans/backlog-run-25-S6b-evidence/live/<size>/`. Step 13's children are shown by the second run
(`<size>-b/pages`), which adds the two Downs.

What gdb cannot show, and the unit tests prove: a stale answer's timing, an adapter that ignores `start` or `count`, a
short page before the end (gdb's hint is exact), and the empty page past the end through jetl (gdb's hint means jetl never
asks there). Other adapters are unverified.

### Scenario: Debug window, variables across stops

Needs what `### Scenario: Debug window, paged variables` needs: `gdb` 14 or newer (the run used 17.2), `g++` with
libstdc++'s printers, the same `config.toml` with `dap-log-file`, the wire proof, the keys and `mapped.cc`. Start jetl
with `env -u CLAUDE_CODE_MESSAGING_TOKEN`: `dap.log` holds the launch environment. One more fixture, `kept.cc`, built
`g++ -g -O0 -o kept kept.cc`, line 23 being `p.x.a = 10;`, line 32 `p.x.b = i;` and line 35 `sum += callee(p);`:

    #include <cstdio>
    #include <vector>
    struct Inner { int a; int b; };
    struct Pair { Inner x; Inner y; };
    struct Base { int b0; };
    struct Der : Base { int d; int Base; };
    static int callee(Pair q) {
        Pair r = q;
        r.x.a += 1;
        return r.x.a;
    }
    int main() {
        static Pair pts[1500];
        pts[1200].x.a = 42;
        Pair p = {{1, 2}, {3, 4}};
        Der der;
        der.b0 = 1;
        der.d = 2;
        der.Base = 3;
        Inner *ptr = &p.y;
        std::vector<Inner> vec = {{5, 6}, {7, 8}};
        int sum = 0;
        p.x.a = 10;
        p.x.a = 11;
        {
            Inner p = {20, 21};
            sum += p.a;
            sum += p.b;
        }
        sum += 1;
        for (int i = 0; i < 200; i++) {
            p.x.b = i;
            sum += i;
        }
        sum += callee(p);
        sum += ptr->a + (int)vec.size() + der.d;
        printf("%d\n", sum);
        return 0;
    }

The Debug window shows about nine rows at both sizes, so a row is read by moving to it: the selected row
is the one with the highlight, which `capture-pane -e` shows as a background colour. Locals at line 23 read, in this
order: `pts`, `p`, `der`, `ptr`, `vec`, `sum`, then `Registers`. F8 stops at lines 24, 26, 27, 28, 30, 31, 32, 33, 31.
Wait about 3 s after each F8 and 4 s after 200 Downs (`send-keys -N 200 Down` is applied one key at a time, and a
capture too soon shows a row on the way). Alt+F5 focuses the Debug window, and again hides it; Escape returns to the
editor, where Ctrl+G and Ctrl+F8 set a breakpoint. Each step's error check: nowhere `could not read`,
`invalid variablesReference` or `list index out of range`, and no toast.

`kept`, breakpoint on line 23, Shift+F9, Variables, at each size:

1. `p`, Right: `▾ p  Pair`, `▸ x  Inner`, `▸ y  Inner`. Down, Right: `a = 1  int`, `b = 2  int`. The selection is on `x`.
2. F8: line 24, `paused (step)`. `▾ p`, `▾ x`, `a = 10  int`, the selection on `x` again. `dap.log`, this stop: `variables`
   for Locals, `p` and `x`, in that order, one each. The numbers: Locals 1, `pts` 3, `p` 4 and `der` 5 at both
   stops, so here the old number would also have done; step 4 shows that it does not always.
3. Without a step, still at line 24: `der`, Right, `Base`, Right: `b0 = 1  int`, and `Base #2 = 3  int` on `Base`'s level,
   with no marker. `ptr`, Right: `▸ value  Inner`. Right on it: `a = 3`, `b = 4`. `vec`, Right, `[1]`, Right: `a = 7`,
   `b = 8`.
4. F8: line 26, already inside the block. `▸ p  Inner`, `▸ pts` and `▸ p #2  Pair`, `p` and `p #2` closed though `p` was
   saved. `dap.log`, this stop: Locals, then `der`, `Base`, `ptr`, `value`, `vec` and `[1]` (references 1, 8, 7, 6, 11,
   12, 13), none for either `p`. This stop's Locals answer gives `pts` 4, `p` 5 and `der` 6, unlike the stops before
   it. A name that repeats under a variable (`Base #2`) is not ambiguous; one under the scope is.
5. Right on `p` (the `Inner`): `a = 0  int` and a `b` that is not. F8: line 27, `▸ p  Inner` closed again, since
   nothing is recorded at this level.
6. F8: line 28, the same.
7. F8: line 30. `▾ p  Pair`, `▾ x`, `a = 11  int`. The outer `p` came back: a path saved before the block survives it.
8. `x`, Left. F8 (line 31, `i` is the first row now): `▾ p`, `▸ x`, and `dap.log` shows no `variables` for `x`. Up to
   `p`, Left, F8: `▸ p`, and `p` is not read: no `variables` for it. Right: `▸ x` closed.
9. Home to `Locals`, Left: nothing under it. F8: `▸ Locals` and `▸ Registers`, and `dap.log` has `scopes` and no
   `variables` at this stop. Right: all of it back, `p`, `der`, `ptr` and `vec` open (`variables` for Locals, `p`,
   `der`, `Base`, `ptr`, `value`, `vec` and `[1]`, and none for `x`).
10. `pts`, Right: `first 1000 of 1500 shown`. End and Up 19 times to the load-more row (`load 500 more`), Enter:
    `1000  Pair`. `send-keys -N 200 Down` to `1200`, Right, Right, Right: `1200` open with `▾ x`. F8: `pts` open with
    `first 1000 of 1500 shown` and its first rows, no `1200`, the selection on `pts`. `dap.log`, this stop: `pts`
    with `"start": 0, "count": 1000`, and no `"start": 1000`.
11. End and Up 19 times to the load-more row, Enter: `1200` comes back open with `▾ x` under it and `a = 42  int`, with
    no key pressed on it. `dap.log`: `"start": 1000` for `pts`, then `1200`, then `x` (references 1223 and 1523).
12. End (`Registers`), Right: rows `ymm15 …`. F8: open again, with one `variables` for it (reference 2) at this stop. Left
    on a row, Left on `Registers` closes it. Home, Down twice, Left closes `pts`, so that `p` is on the screen for what
    follows.

`kept`, a breakpoint added on line 32 (Escape, Ctrl+G 32, Ctrl+F8), F9 to reach it, Alt+F5, and `x` open under `p` again
(Home, Down four times, Right), with the rows of steps 3 to 12 left as they are:

13. **A held step.** `send-keys -N 30 F8` in one command. When the screen settles, one line of the loop is marked, `i`
    matches the count, `▾ p`, `▾ x`, and the error check holds. Observed, from `dap.log`: 28 `next` of the 30 keys at
    120x40 (24 at 80x24; a key that comes while the program runs is ignored), 11 `variables` (9). The screen last
    changed 0.47 s after the keys at 120x40 (0.24 s at 80x24). That is no more than three `variables` per `next`,
    and far fewer: the chain is cut by each step.
14. **A stop mid-reopen.** `send-keys F8 F8 F8` in one command. Then the same check. Observed: 3 `next` and 9
    `variables` at both sizes; the screen last changed 0.24 s (0.13 s) after the keys.
15. **Into a function and back.** Close `pts`'s row if it is open, clear the breakpoint on line 32 (Escape, Ctrl+G 32,
    Ctrl+F8), set one on line 35, F9. F7: in `callee`, line 8, `paused (step)`. `▾ Arguments` and `▾ Locals` open, `▸ q`
    and `▸ r` closed, though `main` has `p` open. Alt+F5, Home, Down three times to `r`, Right, Down, Right: `▾ r`,
    `▾ x`, `a = 0  int`. F8 twice: line 9 with `a = 11`, line 10 with `a = 12`, and `r` stays open.
16. **A frame choice.** Still in `callee`: Alt+Left (the Frames tab), Down to `main`, Alt+Right (back to Variables):
    line 35, `▾ p`, `▾ x`. Alt+Left, Up to `callee`, Alt+Right: `▾ r`, `▾ x`, `a = 12`.
17. **Back, and hidden.** Shift+F8: in `main`, line 35, with `▾ p`, `▾ x` as they were, and `r` nowhere; `dap.log` has
    `scopes`, then nine `variables`: Locals and the eight open paths. Alt+F5 to hide (the Debug title is gone), F8 twice: `next` and `threads`, no `scopes` and no `variables`
    in `dap.log`. Alt+F5 to show: `scopes`, Locals and the open paths follow, and the screen settled 0.24 s (0.13 s
    at 80x24) after the key.
18. Ctrl+F2, then Shift+F9 again: a new session, at line 23, `Debugging kept with gdb`. `p` is closed. A held step with
    Locals only (`send-keys -N 30 F8`): the screen last changed after 0.69 s (0.58 s), 28 `next` of 30 (24), and 2
    `variables` (1). Ctrl+F2.

`mapped`, breakpoint on line 7, at each size:

19. `m`, Right (2 s at `…`): `first 1000 of 200000 shown`. F8: within a second, `▸ m` closed, and `dap.log` has no
    `variables` for `m` at this stop. Observed: the stop shows at line 8 0.12 s after the key at both sizes.
20. Right: the 2 s again, `start` 0 (2.0 s at 120x40, 2.03 s at 80x24, against 1.98 s and 2.02 s the first time). The
    user asked for it, and it works.
21. Ctrl+F2, Ctrl+Q: exit 0, empty stderr, no gdb or debuggee left.

`shadow`, a second fixture built `g++ -g -O0 -o shadow shadow.cc`, with `Other` having a member `x` like `Pair`'s, and a
breakpoint on line 8 (`p.x.a = 10;`; line 10 is `Other p = ...` and line 11 `sum += p.x.a;`):

    #include <cstdio>
    struct Inner { int a; int b; };
    struct Pair { Inner x; Inner y; };
    struct Other { Inner x; int k; };
    int main() {
        Pair p = {{1, 2}, {3, 4}};
        int sum = 0;
        p.x.a = 10;
        {
            Other p = {{20, 21}, 5};
            sum += p.x.a;
            sum += p.k;
        }
        sum += 1;
        printf("%d\n", sum);
        return 0;
    }

22. Variables: `p` (Down, Right), `x` (Down, Right): `a = 1`, `b = 2`. F8 twice, to line 11: `▸ p  Other`, `▸ p #2  Pair`,
    both closed, and no `variables` for either.
23. Home, Down, Right on `p  Other` (opened by hand, in the shadow): `▸ x  Inner` and `k = 5  int`. `x` stays closed
    though `p`, `x` was saved from the outer `p`; `dap.log` has one `variables`, for `p`, and none for `x`. (Before the
    guard in `reopen_under` it read `x` as well: `▾ x`, `a = 20`, `b = 21`.) End, Up, Up to `p #2`, Right: its `y`, and
    `dap.log` has one `variables` for it.
24. F8 twice, to line 14: `▾ p  Pair`, `▾ x`, `a = 10`, `b = 2`: the outer `p` and its `x` are back, with
    `variables` for Locals, `p` and `x`. Ctrl+F2, Ctrl+Q: exit 0, empty stderr, no gdb or debuggee left.

Last run: `~/.claude/plans/backlog-run-25-S7-evidence/live/<size>/{kept,mapped}` (`shadow` for steps 22 to 24 is in
`.../fix/live/<size>/shadow`, with the run before the guard in `<size>-before`); the changed steps of the two paged
scenarios are in `paged-vars` and `paged-more`. The trees at each step are in `<step>.tree` (a walk of every row), and
the requests since the step's key in `<step>-wire.txt`.

What gdb cannot show, and the unit tests prove: an `expensive` scope, a literal repeat of a name, an old stop's answer
at a chosen moment, and a budget run out by arrays. Other adapters are unverified: a hint of 0 prices as small, and an
adapter need not honour that.

### Scenario: Debug build step

Needs `gdb` 14 or newer (the run used 17.2) and `cargo`, no network. At 80x24 and at 120x40, a private tmux socket,
`XDG_*` directories under `/tmp`, `[lsp] enabled = false`, `[ui] toolbar = false`. Fixture `/tmp/jetl-dap-rust`, a
Cargo package `r` with no dependencies: `src/main.rs` has `fn add` with `let sum = a + b;` on line 2 and prints
`x=3`; later steps add `src/bin/other.rs` and a type error on line 7. Start only when `pgrep -f '[g]db -q -i=dap'`
and `pgrep -fx 'sleep 300'` print nothing. A step reads the pane only with no toast on it, except where it greps its
own capture (a toast can cover code, not the gutter in the default layout). The script of the last run is
`~/.claude/plans/backlog-run-24-D3b-evidence/scripts/d3b-scenario.sh`.

1. No `.jetl/run.toml`, `cargo clean`, open `src/main.rs`, Ctrl+G 2, Ctrl+F8, Shift+F9: the toast `Building cargo build`,
   then the title `Debug: cargo build · paused (breakpoint)` and `2▶`. The default block for a detected Cargo task
   works with no file.
2. The Console (Alt+F5, Home) starts with `$ cargo build --message-format=json`, then `Compiling r` and `Finished`;
   no line holds `"reason"`, and no `No source file named` line follows (yman 427).
3. F9: `x=3` in the Console and `Debug session ended, exit code 0`. Shift+F9 again, up to date: it stops again and the
   Console has `Finished` and no `Compiling`.
4. A type error in `main.rs`, Shift+F9: the title `build failed`, a toast naming `exit code 101`, the `error[E0308]`
   text in the Console and a click on its `src/main.rs:7:18` link opens the file; no gdb is left. With the window
   hidden (Alt+F5) the same toast shows.
5. Restore, add `src/bin/other.rs`, Shift+F9: the dialog `The build made 2 executables. Which one?`; Esc gives the
   toast `Debug start cancelled`, the title `not started` and no gdb. Again, type `other`, Enter: gdb runs `other`.
6. A Run task `sleepy` (`sleep 300`) running, Shift+F9 on a two-executable build, Esc: the sleep is still there (a
   cancelled choice stops nothing else).
7. `build = ["sh", "-c", "echo building; sleep 300"]`: Shift+F9, then Ctrl+F2, or a click on Stop: `Debug start cancelled`,
   the title `not started`, and the build's sleep is gone. Rewriting `run.toml` and pressing Shift+F9 during a build
   kills the old build and the Console no longer holds its lines.
8. `build = ["yes", "spew"]`: Shift+F9, 5 s, Ctrl+F2: keys answer throughout, no `yes` is left. jetl's RSS was
   100360 kB at about 1 s, 103028 kB at about 5 s and 103028 kB after the cancel at 120x40; 100648, 103296 and
   103656 kB at 80x24 (yman 425).
9. `launch = { program = "${artifact}" }` with no `build`: the toast `Cannot debug 'sleepy': ${artifact} needs a build
   step`, no gdb, no process.
10. The sleeping build, Ctrl+Q: exit 0, empty stderr, no `sleep` left.

What gdb cannot show: Rust formatting under codelldb or lldb-dap, the default under any adapter but gdb 17.2, the
Variables tree, a real multi-member workspace, a compiler message over 16 KiB from a real rustc, other Cargo versions.

### Scenario: Save As

At 80x24 and at 120x40, with `run()` and `$T` from the crash recovery
scenario (fresh `$T` and `/tmp/jt-home`). Alt+Shift+S arrives under tmux as
`M-S`. The prompt is prefilled; clear it with Backspace (`send-keys -N 20
BSpace`) before typing a path:

```sh
run; sleep 2
/tmp/tm.sh send-keys -t jetl -l 'EDIT '; /tmp/tm.sh send-keys -t jetl M-S   # "Save As", "Where to save notes.txt?", prefilled notes.txt
/tmp/tm.sh send-keys -t jetl -N 20 BSpace; /tmp/tm.sh send-keys -t jetl -l sub/copy.txt; /tmp/tm.sh send-keys -t jetl Enter
cat $T/notes.txt $T/sub/copy.txt      # one, two unchanged; the copy has "EDIT one"
                                      # the tab is copy.txt (clean), the tree shows sub/
/tmp/tm.sh send-keys -t jetl M-S; /tmp/tm.sh send-keys -t jetl -N 20 BSpace; /tmp/tm.sh send-keys -t jetl -l b.txt; /tmp/tm.sh send-keys -t jetl Enter
                                      # "b.txt already exists. Overwrite it?" [ Overwrite ] [ Cancel ]; Enter writes it
```

Tab completes the typed path (`$T` holds `notes.txt`, `b.txt`,
`sub/copy.txt` and `.git/` by now):

```sh
/tmp/tm.sh send-keys -t jetl M-S; /tmp/tm.sh send-keys -t jetl -N 20 BSpace; /tmp/tm.sh send-keys -t jetl -l s; /tmp/tm.sh send-keys -t jetl Tab
                                      # the input reads sub/
/tmp/tm.sh send-keys -t jetl Tab      # sub/copy.txt (the only file)
/tmp/tm.sh send-keys -t jetl -N 20 BSpace; /tmp/tm.sh send-keys -t jetl Tab
                                      # "3 matches; Tab again lists them" (the hidden .git not counted)
/tmp/tm.sh send-keys -t jetl Tab      # b.txt  notes.txt  sub/ under the input
/tmp/tm.sh send-keys -t jetl -l .; /tmp/tm.sh send-keys -t jetl Tab   # .git/
/tmp/tm.sh send-keys -t jetl -N 20 BSpace; /tmp/tm.sh send-keys -t jetl -l '~/'; /tmp/tm.sh send-keys -t jetl Tab Tab
                                      # the home folder's entries, at most 8 rows ("… and N more" when
                                      # they do not fit); the text keeps ~/
/tmp/tm.sh send-keys -t jetl -l zz; /tmp/tm.sh send-keys -t jetl Tab    # "No match"
/tmp/tm.sh send-keys -t jetl Escape
```

The New Worktree directory prompt (last step of the create flow) completes the
same way relative to the project root, but offers folders only: Tab on a
parent folder prefix lists no files.

Tab in another input dialog (Rename, Shift+F6 in the Project window) does
nothing, and Tab in the Overwrite confirm still moves between its buttons.

With b.txt open in another tab (Ctrl+N `b.txt`), Save As of notes.txt to
`b.txt` is refused with a warning ("b.txt is open in another tab; close it
first").

Format on Save As: fresh `$T`, and before `run` write
`/tmp/jt-home/config/jetl/config.toml` with `[lsp] enabled = false`,
`[format] on_save = true` and `rust = ["tr", "a-z", "A-Z"]`. Type in
notes.txt, Alt+Shift+S, change the path to `up.rs`, Enter: `cat $T/up.rs`
is upper case, the tab is clean, Ctrl+Z undoes the reformat (the tab turns
dirty). Alt+Shift+S to `low.txt` (no formatter): lower case, the tab clean.
Alt+Shift+S of that clean tab to `copy.rs`: no reformat, copy.rs is lower
case too.

Untitled buffers: crash with notes.txt modified (as in the crash recovery
scenario), `chmod 000 $T/notes.txt`, `run`, Restore all: the tab is
`*notes.txt (recovered)` with no file.

- Ctrl+S opens Save As prefilled `notes.txt`; change it to `rescued.txt`,
  Enter: the tab is `rescued.txt`, the file has the text, Ctrl+Q exits with
  EXIT=0.
- Repeat with two unreadable files (notes.txt and b.txt modified before the
  crash, both `chmod 000`). Ctrl+Q, Enter ("Save all and exit"): a prompt
  for notes.txt (recovered); type `n2.txt`, Enter; a prompt for b.txt
  (recovered); Esc: jetl stays open, n2.txt is written. Ctrl+Q, Enter, then
  `b2.txt` Enter at the one prompt left: EXIT=0.

`chmod 644` the files afterwards, so `rm -rf $T` works.

### Scenario: scratch files

At 80x24 and at 120x40, with `run()` and `$T` from the crash recovery
scenario (fresh `$T` and `/tmp/jt-home`). Ctrl+Alt+Shift+Insert arrives
under tmux as `C-M-S-IC`; F1 "new scratch" does the same. Shift+Delete is
`S-DC`:

```sh
run; sleep 2
ls /tmp/jt-home/state/jetl                         # no scratches/ yet
/tmp/tm.sh send-keys -t jetl C-M-S-IC              # "New Scratch File", Plain text first, then the languages
/tmp/tm.sh send-keys -t jetl -l rust; /tmp/tm.sh send-keys -t jetl Enter
ls /tmp/jt-home/state/jetl/scratches               # scratch_1.rs, open in a tab, status bar "rust", no language server
/tmp/tm.sh send-keys -t jetl -l 'fn main() {}'; /tmp/tm.sh send-keys -t jetl C-s
/tmp/tm.sh send-keys -t jetl F1; /tmp/tm.sh send-keys -t jetl -l 'new scratch'; /tmp/tm.sh send-keys -t jetl Enter Enter
                                                   # scratch_1.txt (plain text) in a new tab
/tmp/tm.sh send-keys -t jetl F1; /tmp/tm.sh send-keys -t jetl -l 'scratch files'; /tmp/tm.sh send-keys -t jetl Enter
                                                   # "Scratch Files": scratch_1.txt, scratch_1.rs (newest first), language and date on the right
/tmp/tm.sh send-keys -t jetl S-DC                  # "Delete scratch file scratch_1.txt?", "Its tab closes; unsaved edits are lost."
/tmp/tm.sh send-keys -t jetl Enter                 # the file and its tab are gone, the palette is back with scratch_1.rs
/tmp/tm.sh send-keys -t jetl Escape; /tmp/tm.sh send-keys -t jetl C-q   # EXIT=0
/tmp/tm.sh kill-server; run; sleep 2               # the session brings the scratch_1.rs tab back
```

Tab Enter (Cancel) in the delete dialog keeps the file and reopens the
palette. Alt+F1 (Select in Project) on a scratch tab says it is a scratch
file instead of selecting anything.

### Scenario: Find Usages without a language server

With `[lsp] enabled = false` in `config.toml`, on a small Rust project:
`src/main.rs` = `mod util;`, `fn helper() {}`, `fn main() { helper(); }` on
separate lines; `src/util.rs`, `src/same.rs` and `lib/far.rs` each call
`helper();` inside a function. Open `src/main.rs`, put the caret on `helper()`
in `main` and press Alt+F7: the palette title reads `Usages of helper
(unverified)` and the rows run main.rs, util.rs, same.rs, far.rs. Enter jumps
to the row; Esc closes the palette. A name used once only jumps straight to it
with the toast `Only one usage (unverified)`.

### Scenario: SCIP index without a language server

With `[lsp] enabled = false`, `[index] scip = "index.scip"` and `[index]
scip_command = "rust-analyzer scip . --output index.scip"` in
`config.toml`, at 80x24 and 120x40. Project `/tmp/scip-demo`: `Cargo.toml`
(`[package] name = "t"`, `version = "0.1.0"`, `edition = "2021"`),
`src/main.rs` = `mod util;`, `fn main() {`, `    let s = "日本😀"; util::helper(s);`,
`    util::helper(s);`, `}` and `src/util.rs` = `/* 😀 */ pub fn helper(_s: &str) {}`;
then `git init -q` (the project root is the nearest git directory, else the file's folder) and `rust-analyzer scip . --output index.scip` in it.

1. `jetl /tmp/scip-demo/src/main.rs`; caret on `helper` in line 3, Ctrl+B:
   `src/util.rs` opens with the caret on `helper` (column 17 in the status bar: the display column, the emoji is 2 cells wide).
2. Ctrl+B again on that `helper`: the Locations palette `Usages of helper
   (SCIP)` with two rows in main.rs (lines 3 and 4). Esc.
3. In main.rs, Alt+F7 on the call in line 4: the same two rows.
4. In another shell `touch /tmp/scip-demo/src/util.rs`; Ctrl+B on `helper` in
   main.rs still jumps to util.rs (now through the symbol index; same place).
5. Type a character in main.rs (unsaved), Alt+F7 on `helper`: no SCIP rows
   (the symbol-index fallback; `Usages of helper (unverified)` when it finds
   plain calls). Undo.
6. Settings (Ctrl+Alt+S), Language Servers page: the row "SCIP index file"
   shows `index.scip`; set it to `missing.scip` and Apply (Ctrl+S): one Warn toast
   `SCIP index missing.scip: No such file or directory (os error 2)`, timed as
   in step 11. Set it back.
7. Run menu (Alt+R) > Build SCIP Index: the Run window shows `Build SCIP index`
   and rust-analyzer's output, then exit 0; Info toast `SCIP index built;
   loading it`.
8. Caret on `helper` in `src/util.rs`, Ctrl+Alt+H: the Hierarchy window
   `Callers of helper (approximate)` lists `main` (SCIP references in main.rs).
9. In config set `scip_command = "exit 3"`, run the action: Error toast `Run
   Build SCIP index failed with exit code 3`, no reload toast.
10. Set `scip_command = ""`, run it: Info toast `Set [index] scip_command to
    build the SCIP index`.
11. In another shell `truncate -s 257M /tmp/scip-demo/big.scip`; in Settings,
    Language Servers page, set "SCIP index file" to `big.scip` and Apply
    (Ctrl+S): one Warn toast `SCIP index big.scip: 257 MiB is above the limit
    of 256 MiB`. At 120x40 it shows at once below the Settings window. At 80x24
    there is no room below the window, so it is not drawn and its time does not
    start: close Settings, even much later, and it shows for its 8 s. Set it back and
    `rm /tmp/scip-demo/big.scip`.
12. Ctrl+Q exits 0; stderr is empty.

### Scenario: exact rename from a SCIP index

At 120x40 and 80x24 (yman 398), with `[lsp] enabled = false` and `[index] scip =
"index.scip"` in `config.toml`, the harness script `/tmp/tm24.sh` as above, and
config, state, cache and the project under `/tmp/jetl-398`. Shift+F6 is `S-F6`,
Alt+A `M-a`. The index is made by the real rust-analyzer:

```sh
T=/tmp/jetl-398/proj; mkdir -p $T/src
printf '[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2021"\n' > $T/Cargo.toml
printf 'mod util;\n// helper is called twice\nfn main() {\n    let s = "日本😀"; util::helper(s);\n    util::helper(s);\n    let helper = 1;\n    println!("{}", helper);\n}\n' > $T/src/main.rs
printf '/* 😀 */ pub fn helper(_s: &str) {}\n' > $T/src/util.rs
printf 'helper notes\n' > $T/notes.txt
echo target > $T/.gitignore
(cd $T && git init -q && rust-analyzer scip . --output index.scip)
sleep 1; touch $T/index.scip   # the index is newer than every source
```

After `touch index.scip` jetl loads the new index in the background; the first
Shift+F6 right after it may still say `as text`. Every session below therefore
starts with the warm-up `S-F6`, wait 0.6 s, `Escape`, wait 1.5 s. After every
step that names a disk state, `cat` the files (`src/main.rs`, `src/util.rs`,
`notes.txt`) into a numbered file. Keep each session's exit code and stderr in
files of their own. Start jetl on `src/main.rs`, then the warm-up, then `Down`
x4, `Home`, `Right` x10: the caret is inside `helper` of line 5.

1. `S-F6`: dialog `Rename 'helper' to:`, no check row, button `Preview`.
2. `C-a`, `-l assist`, `Enter`: Find opens focused. Rows: `notes.txt` 1
   (dim `text` marker); `src/main.rs` lines 4 and 5 (the comment and `let
   helper` are not listed); `src/util.rs` 1. Info toast `Rename helper to
   assist: review the results, then Replace All (Alt+A); 1 text match starts
   excluded`. On disk all three files still say `helper`.
3. `M-a`: dialog `Rename helper to assist: 3 occurrences in 2 files; 1
   excluded.` and `Text matches (guesses): 0 included, 1 left out.`. `Enter`:
   the `main.rs` buffer has both calls renamed (the emoji line intact, the
   comment and `let helper` unchanged); on disk `main.rs` still says `helper`,
   `util.rs` says `assist`, `notes.txt` says `helper`.
4. `Escape` (focus back to the editor; Ctrl+Z does nothing in Find), `C-z`:
   `main.rs` is restored by one undo step.
5. In the shell: restore `util.rs`, sleep 1, run `rust-analyzer scip . --output
   index.scip`, sleep 1, `touch index.scip`, sleep 1. Keys: `S-F6`, `Escape`,
   wait 1.5 s. In the shell: `touch src/util.rs` (its definition is changed).
   Keys: `S-F6`, `C-a`, `-l assist`, `Enter`: Info toast `The SCIP index has no
   usable definition of helper; renaming as text` and an ordinary Find on
   `helper`. Nothing is written (disk as in step 2). `Escape`, then in the shell
   `touch index.scip`, then `C-q`.
6. Start jetl on `src/util.rs`; warm-up; in the shell `touch src/main.rs`, sleep
   1; keys `Home`, `Right` x16 (the caret is on `helper`). `S-F6`, `C-a`, `-l
   assist`, `Enter`: `util.rs` is one exact row, the five `main.rs` matches and
   `notes.txt` are text rows (dim), Warn toast `SCIP index: 1 file changed or
   unreadable, text matches only`. `M-a`: the dialog has `SCIP index not used for
   1 file (changed or unreadable): text matches only.` `Escape`, then in the
   shell `touch index.scip`, sleep 1.
7. In the same session: `S-F6`, wait 0.6 s, `Escape`, wait 1.5 s, `Escape`, then
   `S-F6`, `C-a`, `-l assist`, `Enter`. In the shell append a line to
   `src/util.rs`. Keys: `M-a`, wait 0.5 s, `Enter`: Error toast `src/util.rs
   changed since the preview; rename again`, the rows stay, nothing is written
   except the appended line. In the shell restore the file, sleep 1, `touch
   index.scip`, then keys `C-q`.
8. Start jetl on `src/main.rs`; warm-up; `Down` x4, `Home`, `Right` x10; type
   `-l x`; `S-F6`: the dialog reads `Rename 'helpxer' as text to:` (a letter was
   typed inside the word, so the buffer is dirty). `Escape`, `C-z`.
9. `Down`, `Home`, `Right` x6 (the `helper` of `let helper = 1;`, line 6).
   `S-F6`, `C-a`, `-l value`, `Enter`: rows on lines 6 and 7 of `main.rs` only
   (rust-analyzer gives the use in `println!` an occurrence), no text rows.
   `Escape`, `C-q`.
10. Each session exited 0 and its stderr is empty.
11. Shorthand fields (a symbol sharing a span with another is renamed as text).
    In a second project, `src/lib.rs` is `struct S { x: i32 }`, `fn f(x: i32) ->
    S {`, `    S { x }`, `}`, `fn main() { f(1); }`; run the real `rust-analyzer
    scip`, `touch index.scip`, start jetl on `src/lib.rs`, warm-up. `Down`,
    `Home`, `Right` x5 (the parameter `x`), `S-F6`: dialog `Rename 'x' as text
    to:`. `Escape`. `Up`, `Home`, `Right` x11 (the field `x`), `S-F6`: dialog
    `Rename 'x' as text to:`. `Escape`. `Home`, `Right` x7 (`S`), `S-F6`: dialog
    `Rename 'S' to:` (no shorthand use of `S`). `Escape`, `C-q`.

### Scenario: local history

At 80x24 and at 120x40 (`-x 120 -y 40`), with config and state under
`/tmp/jetl-lh` and language servers off:

```sh
LH=/tmp/jetl-lh; rm -rf $LH; mkdir -p $LH/repo/d $LH/cfg/jetl
printf '[lsp]\nenabled = false\n' > $LH/cfg/jetl/config.toml
git -C $LH/repo init -q; printf 'one\ntwo\nthree\n' > $LH/repo/notes.txt; seq 1 3000 > $LH/repo/d/x.txt
git -C $LH/repo add . && git -C $LH/repo -c user.name=t -c user.email=t@t commit -qm init && git -C $LH/repo branch other
cargo build --no-default-features --features lang-rust
run() { /tmp/tm.sh -f /dev/null new-session -d -s jetl -x 80 -y 24 \
  "XDG_CONFIG_HOME=$LH/cfg XDG_STATE_HOME=$LH/state $PWD/target/debug/jetl ${1:-$LH/repo/notes.txt} 2>/tmp/jetl-stderr.txt; echo EXIT=\$?; sleep 600"; }
s() { /tmp/tm.sh send-keys -t jetl "$@"; sleep 0.4; }
run; sleep 2
s -l 'A '; s C-s; s -l 'B '; s C-s
s M-H                                  # Local History window (bottom) on notes.txt: two "Saved" rows, newest first,
                                       # "18 B  =" and "16 B"; the newest one's diff on the right, "type to filter"
s Down                                 # the diff follows: "Saved  <date time>  ↔  current", "A one" against "A B one"
s M-r                                  # *notes.txt is "A one", toast "Reverted notes.txt to … (Ctrl+Z undoes)", editor focused
s C-z                                  # "A B one" again, the tab clean
s C-Home; s Down; s Down; s -l X       # *notes.txt: "A B one", "two", "Xthree"
s M-H; s Down; s Right                 # the older "Saved" against it, the diff focused: two changes, the first's
                                       # line numbers highlighted, "[Revert Change]" in the toolbar
s S-Down                               # both highlighted, "[Revert 2 Changes]"
s C-M-z                                # *notes.txt "A one", "two", "three"; toast "Reverted 2 changes in notes.txt
                                       # to … (Ctrl+Z in the editor undoes)"; the window keeps focus, its diff
                                       # compared again: no change left
s Escape; s Escape; s C-z              # back in the editor; one Ctrl+Z: "A B one", "two", "Xthree"
s C-z                                  # "A B one", "two", "three", the tab clean
s C-Home; s Down; s Down; s -l X       # again "Xthree" (and "A B one" above): two changed lines in the file
s M-H; s Down; s Right; s M-Down       # the diff's first changed row: only its line number(s) reversed, "[Revert 1 Lines]"
s M-S-Down                             # the next changed row joins them, "[Revert 2 Lines]"; F7 or Esc leaves line mode
                                       # (the toolbar reads "[Revert Change]" again); a double click on a changed row sets it
s M-Down; s C-M-z                      # only that line goes back; the other changed line keeps its edit; Ctrl+Z undoes it
s Escape; s Escape; s C-z; s C-z       # back to "A B one", "two", "three"
s M-H; s Down; s Escape; s -l Q         # the window stays open beside the editor; typing in notes.txt: within half a second
                                       # the diff's right side shows the typed "Q", scroll and selected change kept
s C-z                                  # the diff follows again, without "Q"
echo outside >> $LH/repo/notes.txt     # the clean tab reloads (nothing new stored: its text was the last snapshot)
s F1; s -l 'Git: branches'; s Enter; s -l other; s Enter; s Enter   # Checkout: "Switched to other"
                                       # the window lists by itself: "Before git switch other" on top,
                                       # the older "Saved" still selected, its diff now with "+outside"
s M-H; s M-l; s -l 'before refactor'; s Enter   # Put Label from the window: toast, "Label: before refactor" on top, bold
s Home; s -l label                     # "filter: label" on the right, only the label row
s Escape; s DC                         # Esc clears the filter; Delete on the label: "Remove Label" dialog
s Tab; s Enter                         # Cancel: the label stays
s DC; s Enter                          # Remove: toast 'Removed label "before refactor"', the row goes in place
# Put Label on a folder: select a folder in the Project tree (Alt+1), Alt+L / F1 "Put Label"; the dialog reads
# "Label for src/:"; Enter after a name: toast 'Put label "x" on src/ (N files)'; the Local History window's
# project view (Alt+Shift+H on the folder) shows one label row for it, Enter lists its files.
s Down; s DC                           # on a snapshot that is not a label: "Only labels can be removed; …"
s Escape; s M-1; s Home; s Right; s Down; s Delete; s Enter   # delete d/x.txt from the tree
ls $LH/state/jetl/history/index        # a second index file (d/x.txt, "before_delete")
ls $LH/state/jetl/history/blobs-zlib/*/   # one blob: d/x.txt, compressed (notes.txt's are under 64 bytes, raw in blobs/)
wc -c $LH/state/jetl/history/blobs-zlib/*/*   # about 6.5 KB, under half the 13893 bytes of d/x.txt
printf '[lsp]\nenabled = false\n[local_history]\nsnapshot_unsaved_secs = 10\n' > $LH/cfg/jetl/config.toml   # live reload: the timer starts now
s Escape                               # back to the editor (the tree had focus after the delete)
s -l 'draft '; sleep 11                # the window (still open) gets an "Unsaved changes" row on top by itself
s -l 'more '; sleep 11                 # still one "Unsaved changes" row (merged), newer size
s C-s                                  # that row is now "Saved"
s M-F1                                 # Project tree focused, notes.txt selected
s S-F6; for i in 1 2 3 4 5 6 7 8 9; do s BSpace; done; s -l renamed.txt; s Enter   # "Renamed to renamed.txt", the tab follows
                                       # and the window: "Local History: renamed.txt", "Renamed from notes.txt" on top
ls $LH/state/jetl/history/index        # one index file more; notes.txt's is still there
# git renames: in a repo with local history on, commit `git mv a.txt b.txt` on a branch, switch back, then switch to
# that branch from the Git window: Local History of b.txt shows a's snapshots and "Renamed from a.txt" on top
# outside renames: with the project open, `mv notes.txt moved.txt` in another shell; the window on notes.txt
# retargets to moved.txt, "Renamed from notes.txt" on top (a rename made in jetl shows no second marker)
s Escape; s -l 'label me '             # editor focused, renamed.txt dirty
s F1; s -l 'put label'; s Enter        # "Put Label": "Label for renamed.txt:"
s Enter                                # toast "Put Label: a label needs a name"; nothing stored
s F1; s -l 'put label'; s Enter; s -l keep; s Enter   # a label that should survive the prune below
s C-s                                  # no new row: the saved text is the label's
s M-H; s BSpace                        # the project view, "Local History: repo/": renamed.txt on top, "▸ Label: keep
                                       # 1 file", d/x.txt crossed out "deleted", notes.txt crossed out "renamed to
                                       # renamed.txt"; "[Show History]" first in the toolbar
s Down; s Down; s Enter                # on the label: "▾", renamed.txt listed under it
s -l deleted                           # "filter: deleted": only d/x.txt, its diff against nothing ("↔  deleted")
s M-r                                  # a tab "*x.txt" with 1..3000 (unsaved), toast "Reverted x.txt to …"
s C-s                                  # d/x.txt is back on disk
s M-H; s BSpace                        # the project view again: "No file matches "deleted"" (x.txt is back)
s Escape                               # the filter cleared: x.txt "Saved" on top
s Enter                                # its own view, "Local History: d/x.txt": "Saved", "Before delete"
s BSpace                               # back, x.txt still selected
s C-q                                  # EXIT=0
/tmp/tm.sh kill-server
printf '[lsp]\nenabled = false\n[local_history]\nkeep_days = 0\n' > $LH/cfg/jetl/config.toml
run $LH/repo; sleep 2                  # the project (notes.txt is renamed.txt now)
ls $LH/state/jetl/history/index        # one index file left: renamed.txt's (the log says how many were pruned);
cat $LH/state/jetl/history/index/*     # only the "kind":"label" line of "keep": labels outlive keep_days;
                                       # the other blobs stay until they are 10 minutes old
/tmp/tm.sh kill-server; rm -rf $LH
```

Don't Save keeps the edits: with a dirty `notes.txt` ("A one"), Ctrl+Q, then
Don't Save, run again, M-H on notes.txt: "Before discarding unsaved edits"
on top, its diff against the file on disk showing "+A ". The same for a
tab closed with Don't Save, with `snapshot_unsaved_secs = 0`.

At 120 columns the list also shows each snapshot's size and line change,
and Tab moves to the diff (its header in the focused style); below 60
columns inside the window the list and the diff take turns. Shift+F10 in
the Project tree opens its context menu with Local History and Local
History: Put Label… under Show History; Shift+F10 in the window opens its
own (Revert, Revert Selected Changes, Put Label…, Remove Label…, Open File,
All Files; in the project view Show History, Revert, Revert Selected
Changes, Remove Label…). File > Local History… on a folder selected in the
Project tree shows the project view of that folder only.
Revert a whole label: Put Label "v1" on a folder with an open tab, a closed
file and (after deleting it) a missing one, edit all three, then in the
project view select the label row and press Alt+R: the confirm reads
`Revert 3 files to label "v1"?`; Revert puts each back to its labelled text
in an unsaved tab (the missing file's folder is made again), the toast reads
`Reverted 3 files to label "v1" (unsaved; Ctrl+Z in each tab undoes)`, and
the focus stays in the window.
A Ctrl+click on a change in the diff extends the selection to it (Shift+click
in terminals that pass it on); typing in the tab after the diff was made and
then Ctrl+Alt+Z in the window reverts nothing: toast "… changed since the
diff was made", and the diff is compared again. Alt+1 and Alt+9 hide
their window when it already has focus; under tmux, Ctrl+Space in the
editor is completion, not the branches popup, hence Find Action above.

### Scenario: Git Log

At 80x24 and at 120x40 (`-x 120 -y 40`), in a scratch project `$T` as above with `dev` one commit ahead:

```sh
git -C $T switch -qc dev && echo dev >> $T/README.md && git -C $T -c user.name=t -c user.email=t@t commit -qam 'dev change' && git -C $T switch -q -
# start jetl on $T as above, then:
/tmp/tm.sh send-keys -t jetl M-9; /tmp/tm.sh send-keys -t jetl M-l   # Git window, Log tab: "[Branch: all]", "dev change" listed
# "dev change" is the selected row, then:
/tmp/tm.sh send-keys -t jetl S-F10     # commit menu; "Branch 'dev' ▸" is the last row, after a separator
/tmp/tm.sh send-keys -t jetl Up Enter  # (Up wraps to that row) the "Branch 'dev'" dialog: Checkout, New Branch from 'dev'…,
                                       # Compare with '<main>', Merge 'dev' into '<main>'…, Rebase…, Rename…, Delete…
# pick Compare: the bar shows "[Branch: HEAD..dev]" and only "dev change"
# reopen the menu and the dialog, pick Merge: a confirm appears; Cancel
git -C $T log --oneline -1             # unchanged
# select "dev change", then Ctrl+D: an editor tab "Diff <sha8>", header "1/N  <parent8>..<sha8>  <path>" (no counter for one
# file), two panes at 120x40 (one unified pane at 80x24), the editor focused; Alt+N walks to the last file, then the
# toast "Last file in this diff"; F7 / Shift+F7 step the changes; typing changes nothing; Ctrl+D again on the same row
# activates the same tab (back on file 1). S-F10: "Show Diff  Ctrl+D", then "Show Diff as Patch": a tab "Patch <sha8>"
# with the "==" header and a green and a red row (capture-pane -e: different backgrounds); F7 moves the caret to the
# first change (status bar Ln), Shift+F7 there says "No more changes above"; Show Diff as Patch again adds no tab
# compare, with a file open and another text in the clipboard: View > Compare with Clipboard opens a tab "<name> vs
# Clipboard", header "Clipboard ↔ <path>", two panes at 120x40; change the clipboard and compare again: the same tab
# shows the new diff; Compare with File on a binary file says "Binary files differ" and opens nothing
# reset: add a second commit, edit README.md without committing it
git -C $T switch -q - ; echo x >> $T/README.md; git -C $T -c user.name=t -c user.email=t@t commit -qam two; echo edit >> $T/README.md
# in the Log select `init`, S-F10, "Reset Current Branch to Here…": a choice (Soft, Mixed, Hard); pick Hard: a confirm
# (Reset / Cancel) naming Local History; Reset: the toast "Reset to <sha8> (hard)"
git -C $T log --oneline                # only `init`
# Local History on README.md lists "Before git reset --hard <sha>" with the edit
# repeat with a new second commit and Soft: `git -C $T status --short` shows `M  README.md`
# multi-select, in a scratch repo with three commits on the current branch: Shift+Down twice: three highlighted rows,
# the details start with "3 commits selected"; Ctrl+D: one tab "Diff <oldest8>..<newest8>" over the union of the files
# (path order, "1/N" in the header); Show Diff as Patch from the menu: a tab "Patch <oldest8>..<newest8>" with one "==" header;
# Down: one row again. Ctrl+click on rows 1 and 3 (SGR button 16: press 16 <col> <row>; release 16 <col> <row>),
# then Show Diff as Patch: a tab "Patch of 2 commits (<top8>…)" with two "==" sections; Ctrl+D: "Diff of 2 commits (<top8>…)".
# with `[git] log_page = 2` in $C/config.toml (plus `[lsp] enabled = false`) and three commits, a Ctrl+click on the second
# (last loaded) row loads the third commit.
# fold sub-scenario, in a fresh scratch repo $T (tmux session started on it as above): a commit that changes two
# files (a.txt at lines 2 and 25, b.txt at line 5)
seq 1 30 > $T/a.txt; seq 1 30 > $T/b.txt
git -C $T add . && git -C $T -c user.name=t -c user.email=t@t commit -qm 'two files'
sed -i -e 's/^2$/two/' -e 's/^25$/twenty-five/' $T/a.txt; sed -i 's/^5$/five/' $T/b.txt
git -C $T -c user.name=t -c user.email=t@t commit -qam 'fold me'
/tmp/tm.sh send-keys -t jetl M-9; /tmp/tm.sh send-keys -t jetl M-l   # Log tab, "fold me" on top
/tmp/tm.sh send-keys -t jetl S-F10 Down Down Enter; sleep 1   # Show Diff as Patch: tab "Patch <sha8>"
# at 80x24 the Git window leaves 7 editor rows (and a warning toast covers some): Alt+9 twice (the first moves the focus
# to the editor, the second hides the window), wait for the toast to go; Alt+9 shows the window again
/tmp/tm.sh send-keys -t jetl M-9; /tmp/tm.sh send-keys -t jetl M-9
# ▾ on line 1 (==), lines 2 and 22 (diff --git) and lines 6, 13 and 26 (@@), and on no other line
/tmp/tm.sh send-keys -t jetl C-g; /tmp/tm.sh send-keys -t jetl -l '8'; /tmp/tm.sh send-keys -t jetl Enter   # a line of hunk 1
/tmp/tm.sh send-keys -t jetl M-,           # hunk 1 collapsed: line 6 "@@ -1,5 +1,5 @@…" with ▸, the next row is line 13
/tmp/tm.sh send-keys -t jetl M-,           # a.txt collapsed: line 2 "diff --git a/a.txt b/a.txt…" ▸, the next row is line 22
/tmp/tm.sh send-keys -t jetl M-.           # a.txt open again, hunk 1 still collapsed
/tmp/tm.sh send-keys -t jetl 'M->'         # expand all (Alt+Shift+.)
/tmp/tm.sh send-keys -t jetl 'M-<'         # collapse all: only line 1 "== <sha8> fold me…" (▸) and the empty last line 35
# click the ▸ of line 1 (the first ▸ after the "││" of the second screen row; column 42 at 80x24 and 120x40):
# lines 1, 2 (▸) and 22 (▸) are shown, the files still collapsed
press 0 42 2; release 0 42 2
/tmp/tm.sh send-keys -t jetl 'M->'         # everything open again
# With the Git window shown again (Alt+9), two Ctrl+clicked commits that are not neighbours (here "fold me" and "init"; SGR button 16: press 16 <col> <row>;
# release 16 <col> <row>), Show Diff as Patch from the menu, Alt+<: two "==" rows with the empty separator row between them.
# file tree, in a scratch repo $T with a commit that touches src/a/b/x.txt, src/y.txt and top.txt: select it in the Log, Right:
# "▾ <repo dir>", then "▾ src", "▾ a/b", "x.txt", "y.txt", "top.txt" (folders first)
# Down onto "a/b", Left: "▸ a/b" and its file hidden; Right opens it again
# Enter on y.txt: the diff pane (Esc returns); Esc, right click on y.txt: the menu Show Diff, Open Diff in Editor Tab, Open in Editor, Copy Path,
# Show History for File, then Copy Revision Number, Cherry-Pick, Revert Commit…, Checkout Revision; Escape
# right click on "src": Expand All, Collapse All, Copy Path, Show History for Folder, then the same four; pick Show
# History for Folder: the bar shows "[Path: src]"
# union, same repo $T with 3 commits on the branch: Shift+Down twice on the Log: "3 commits selected", the three
# "short subject" lines, the tree lists every file once; Right, Enter on a file changed in the first and last
# commit: the diff header reads "<base8>..<top8>  <file>" and shows the combined change; Ctrl+click a
# non-neighbour commit: "(not contiguous)" in the header
# diff tab (Ctrl+D on a file), in a scratch repo $T with a modified tracked file (a.txt) and the merge-conflict file:
# Alt+9, select a.txt in Changes, Ctrl+D: a full-size tab "Diff a.txt", header row "a.txt  (staged ↔ working tree)" with
# "[Side-by-side] [x] Collapse" at the right, two panes at 120x40, one unified pane at 80x24, the editor focused;
# F7 / Shift+F7 move the highlighted change ("No more changes below" / "above" at the ends); the wheel scrolls;
# Up/Down/PageUp/PageDown scroll; a click on "[Side-by-side]" flips only the tab (Alt+9: the Git window diff keeps its layout);
# typing and Ctrl+S change nothing; Ctrl+D in the Git window again on a.txt activates the same tab; Ctrl+W closes it
# Log tab: select a commit, Tab to its files, Ctrl+D on a file: its diff tab; S-F10 on a file: "Open Diff in Editor Tab" after
# "Show Diff"; Ctrl+D in the Log list opens the commit's files in a "Diff <sha8>" tab. Ctrl+D on a staged file and on the same file
# unstaged: two tabs "Diff a.txt (staged)" and "Diff a.txt". Stash tab: Ctrl+D on a stash file opens a tab.
# Ctrl+D on the conflicted file in Changes opens the resolver, not a tab
# Checkout Revision takes the branch, in the fixture repo /tmp/jt25-s1/make-repo.sh (S=/tmp/jt25-s1, T=$S/repo; a
# private socket via /tmp/tm25.sh, config.toml with [lsp] enabled = false). Log rows top down: top (main, origin/main),
# long name, no branch, clash remote (origin/clash; local clash is at init), two locals (left, right), only remote
# (origin/remote-only), one local (solo, origin/solo, upstream/solo), init (tag: v0.1.0, clash)
sh /tmp/jt25-s1/make-repo.sh
# the menu below is: S-F10, Down Down Down, Enter (Checkout Revision); after a step that switched branch, reset from
# outside with `git -C $T switch -q main` and F5
# top, the menu: toast "Already on main", no second menu
# Down 6 times to "one local", Alt+Shift+C (M-C): toast "Switched to solo"; `git -C $T branch --show-current` prints solo
# "only remote", the menu: toast "Created and switched to remote-only"; `git rev-parse --abbrev-ref remote-only@{u}` prints
# origin/remote-only (reset, then `git -C $T branch -qD remote-only`)
# "two locals", the menu: a second menu with exactly "Checkout 'left'" and "Checkout 'right'"; Escape leaves `main`
# checked out; again with Down Enter: toast "Switched to right"
# "clash remote", the menu: toasts "Local branch 'clash' is at another commit, so HEAD detaches" and "Checked out <sha8>
# (detached HEAD)"; `git branch --show-current` prints nothing and `git rev-parse clash` is still init's commit
# "no branch", the menu: only the detached toast
# right click "one local", click "Checkout Revision": toast "Switched to solo"
# with an uncommitted edit (`echo edit >> $T/a.txt`, then F5), the menu on "one local": an error toast from git
# switch, still `main`, the edit still in a.txt (restore with `git -C $T checkout -- a.txt`)
# Ref column, same fixture, Log open (Alt+9, Alt+L): every row reads graph, hash, subject, then the refs ending at one column,
# then (120x40 only) author and date. Refs take what a short subject leaves. The author and date go only when the row would leave refs under 15 columns, so a longer label is cut beside them.
# Labels read the current branch ("* x"), other local branches by name, remote-only branches, then tags; "origin & main" merges
# a remote into its local branch; under the cap the merged prefixes drop first ("solo", "* solo"), so the branch Checkout takes shows.
# 80x24: top "* origin & main"; long name "feature/a-rather-long-br…"; no branch: no label; clash remote "origin/clash";
# two locals "left, right" with the subject cut; only remote "origin/remote-only"; one local "origin & upstream & solo";
# init "clash, tag: v0.1.0" (clash before the tag). No author or date.
# 120x40 (branches pane on the left, narrower list, author and date shown): top "* origin & main" whole; long name "feature/a-rather…";
# two locals "left, right"; only remote "origin/remote-o…"; one local "solo"; init "clash, tag: v0.1.0"
# `capture-pane -e -p`: labels bold in the accent colour, bold in the row's colours on the cursor row
# Down through every row: the highlight spans the row, nothing of a longer earlier row is left behind
# left click the subject of "no branch", then the labels of "clash remote" (coordinates read off a capture): each selects its row
# S-F10 on "one local": the rows read Branch 'solo', Branch 'origin/solo', Branch 'upstream/solo' (full names); Escape
# Checkout Revision on it (M-C): the row reads "* solo" (80x24 and 120x40: the full "* origin & upstream & solo" is 26 columns and
# does not fit the cap), and top reads "origin & main" after the reload
# the several-branches menu: on "two locals" (4 Down from top) the menu lists "Checkout 'left'" above "Checkout 'right'", whatever order
# git lists them in; Down Enter: toast "Switched to right", the row reads "* right, left"
# a long author (`git -C $T -c user.name='Bronnikov Aleksandr' -c user.email=t@t commit -q --allow-empty -m wide`, F5) hides the
# author column at 120x40 and the one edge holds; the wide row reads "wide" with "* main", and top "origin/main"
/tmp/tm.sh kill-server
```

Real terminals often keep Shift+click for their own text selection, so Shift+Up/Down is the dependable route to a range.

### Scenario: LSP navigation (rust-analyzer)

Setup:

```sh
R=/tmp/jt-ra; rm -rf $R; mkdir -p $R
rsync -a --exclude target --exclude .git --exclude .yman /pro/tools/jetl/ $R/
mkdir -p /tmp/jt-home/config/jetl
printf '[lsp.servers.rust-analyzer]\ninit_options = { workspace = { symbol = { search = { scope = "workspace_and_dependencies", kind = "all_symbols" } } } }\n' > /tmp/jt-home/config/jetl/config.toml
cargo build --no-default-features --features lang-rust
/tmp/tm.sh -f /dev/null new-session -d -s jetl -x 120 -y 40 \
  "XDG_CONFIG_HOME=/tmp/jt-home/config XDG_STATE_HOME=/tmp/jt-home/state RUST_LOG=jetl=debug $PWD/target/debug/jetl $R 2>/tmp/jetl-stderr.txt"
```

rust-analyzer needs up to a minute to index; wait until the status bar spinner stops. Repeat with `-x 80 -y 24`.

- Ctrl+Shift+N, `index.rs`, Enter; put the caret on the field `symbol` in `pub symbol: Symbol`.
- Send Ctrl+Shift+B as CSI u (`/tmp/tm.sh send-keys -t jetl -H 1b 5b 39 38 3b 36 75`): `src/symbols/mod.rs` opens at `pub struct Symbol`; Back returns. `M-B` (Alt+Shift+B) does the same.
- Caret on `Default` in an `impl Default for` line, `C-M-b`: the Locations palette titled `Implementations of Default`.
- Caret on a trait method name inside a `trait` block, `C-M-b`: lists its impls (or jumps when there is one).
- Navigate > Declaration on a name with one declaration jumps; with none, the toast `Cannot find declaration to go to`.
- Parameter Info: open `$R/src/app/symbols.rs`, put the caret inside `self.symbols.on_batch(seq, batch)` after `seq, ` and send `C-p`: a popup above the line shows the signature with `batch` bold (check with `capture-pane -e`). `Escape` closes it. On an empty line type `-l 'self.request_symbols('`: the popup opens by itself; `-l ')'` closes it. Undo with `C-z` and never save.
- Workspace symbols: with a `.rs` file open and indexing finished, `M-N` (Alt+Shift+N), `-l 'HashMap'`: rows with absolute paths under `~/.rustup` or `~/.cargo` appear next to the project's rows; Enter on one opens it at the struct. `grep 'workspace/symbol' /tmp/jt-home/state/jetl/jetl.log` shows one request per typed prefix. Check at 120x40 and 80x24. `window/showMessageRequest` has no rust-analyzer trigger; the integration test `show_message_request_opens_a_choice_and_answers_the_pick` covers it.
- Kill the server with `/tmp/tm.sh kill-server` afterwards.

### Scenario: push options and stash untracked

Setup:

```sh
T=/tmp/jt-push; R=/tmp/jt-push.git; rm -rf $T $R; git init -q --bare $R; mkdir -p $T && cd $T && git init -q
git config user.name t; git config user.email t@t
echo a > a.txt; git add . && git commit -qm init; git remote add origin $R; git push -qu origin HEAD; git tag v1; cd -
```

In jetl, Git > Push…:

- T toggles Tags (`[x]`, the `Runs:` line updates); Push.
- `git -C $T ls-remote --tags origin` lists v1.
- Run `git -C $T commit -q --amend -m two`.
- Git > Push (plain) fails with `failed to push`.
- Git > Push…, F, Push succeeds.
- Stash tab (Alt+T): with `new.txt` untracked, click `[ ] Untracked`, then `[Stash…]`, Enter.
- `new.txt` is gone and `git -C $T stash show --include-untracked --name-only` lists it.
- `[Pop]` brings it back.

### Scenario: palette on a huge tree

200000 files (exactly the file index cap), language servers off, a
release build:

```sh
B=/tmp/jetl-big; rm -rf $B; mkdir -p $B/cfg/jetl $B/tree
printf '[lsp]\nenabled = false\n' > $B/cfg/jetl/config.toml
for d in $(seq 0 399); do mkdir -p $B/tree/m$d/sub; (cd $B/tree/m$d/sub && seq -f 'file_%g.rs' 0 499 | xargs touch); done   # 200000 files
cargo build --release --no-default-features --features lang-rust
/tmp/tm.sh -f /dev/null new-session -d -s jetl -x 120 -y 40 \
  "RUST_LOG=jetl=debug XDG_CONFIG_HOME=$B/cfg XDG_STATE_HOME=$B/state $PWD/target/release/jetl $B/tree 2>/tmp/jetl-stderr.txt"
sleep 5; s() { /tmp/tm.sh send-keys -t jetl "$@"; }
s C-n; sleep 3; s -l 'm24/file_345'    # typed in one burst: rows follow, no stall; count 22
/tmp/tm.sh capture-pane -t jetl -p     # top row "file_345.rs  m24/sub/", matched chars highlighted
s BSpace; s BSpace; s -l '99'; s Enter # opens m24/sub/file_399.rs at once
grep 'palette filter\|capped' $B/state/jetl/jetl.log   # one "N of 200000 in …ms" per settled query,
                                       # the superseded ones "cancelled"; no "capped" line
/tmp/tm.sh kill-server; rm -rf $B
```

Ctrl+N with an empty query lists the opened file first, so it shows which
one Enter took. Each filter takes about 15-80 ms in release at this size;
the keystrokes echo at once, and the previous rows stay on screen (with
their count) until the new ones arrive.

### Scenario: injections

Setup as above (default features). Create the fixtures first:

    printf '# Fence\n\n```rust\nfn main() {\n    let s = "hi";\n}\n```\n\n```toml\n[a]\nb = 1\n```\n' > $R/fence.md
    printf '<template>\n  <div>{{ msg }}</div>\n</template>\n<script lang="ts">\nconst msg: string = "hi";\nexport default { data() { return { msg } } }\n</script>\n<style>\n.a { color: red; }\n</style>\n' > $R/App.vue

1. Open `fence.md` (Ctrl+N). `capture-pane -p -e` shows `fn` and `let` in the keyword
   colour, `"hi"` as a string and `b = 1` in the TOML key colour; the prose and the heading
   text are not code coloured.
2. Put the caret on the `{` after `main()`: both braces of the pair are highlighted.
3. Type `-l 'let t = 1; '` inside the first fence: both fences stay highlighted correctly;
   Ctrl+Z restores the text.
4. Open `App.vue`: `const` and `string` are coloured as TypeScript, `color` as a CSS property.
5. Open `docs/CONFIG.md` and scroll through its `toml` fences.
6. Repeat at 120x40 and 80x24.

### Scenario: sort lines and case

Setup as above. Create the fixture first:

    printf 'pear\nApple\nfig\n' > $R/fruit.txt

1. Open `fruit.txt` (Ctrl+N). F1, `-l 'Sort lines'`, Enter: the lines read `Apple`, `fig`,
   `pear` (uppercase first). Ctrl+Z restores the order.
2. Select the first two lines (`S-Down` twice from the start), F1, `Reverse lines`: lines one
   and two swap, `pear` stays last.
3. Put the caret in `pear`, F1, `To upper case`: `PEAR`. Ctrl+Z once undoes it.
4. Open the Edit menu (F10 or a click on `Edit`): Toggle Case, To Upper Case, To Lower Case,
   Sort Lines and Reverse Lines are listed; at 80x24 the menu scrolls.
5. Repeat at 120x40 and 80x24.

### Scenario: indent guides

Setup as above.

1. Open `src/app/mod.rs` (Ctrl+N). At 80x24 and 120x40 `capture-pane -p` shows `│` at columns
   0, 4, 8 of nested bodies (text column) and none inside code or after the first non-blank char.
2. A blank line inside a function body shows the guides of the shallower of its neighbours; a
   blank line between two top-level items shows none. `capture-pane -e` shows the guide colour.
3. Put the caret on a blank line inside a body: the current-line background stays under its
   guides. Click on a guide cell of a blank line: the caret goes to that line (its end, or that
   virtual column with soft wrap off), as before.
4. Scroll sideways in a long line (soft wrap off): the guides, those on blank lines too, stay on
   their columns.
5. Append `[editor]\nindent_guides = false\n` to `/tmp/jt-home/config/jetl/config.toml`: the
   guides disappear without a restart; remove it: they come back.
6. Open `fence.md` (the injections fixture): guides inside the fence's indented line.
7. Repeat at 120x40 and 80x24.

### Scenario: inlay hints

Setup as above, but rust-analyzer on: no `[lsp] enabled = false`. Wait for indexing (the status
bar spinner stops). jetl asks again after a request timed out and when the server's progress
ends, so hints appear without scrolling or editing once rust-analyzer has loaded.

1. Open `src/app/lightbulb.rs`. Type hints (`: Option<…>`, `: &mut App`) and parameter-name hints
   (`range:`, `params:` before call arguments) are visible, in the `inlay_hint` colours
   (`capture-pane -e`).
2. Click on a type hint's label cell: the caret (terminal cursor) is right before the hint, and
   the status bar column is the anchor's text column. Click on a parameter hint: the caret is
   after it, on the char it names.
   On a `let` line with a type hint, click right after the variable name and type `x`. The char
   goes left of the hint and the caret stays between them. Left moves one char, Backspace
   removes it. Undo.
3. Toggle soft wrap (View > Soft-Wrap). Make a hinted line longer than the view (type at its
   end): it wraps, the hints are still shown, a hint never splits from the char after it, and
   clicks on continuation rows land right. Undo.
4. F1, `Show or hide inlay hints`: the hints disappear and the toast says `Inlay hints: off`.
   Scroll a page down and back: no hint shows. Run it again: the toast says on, and the hints
   reappear after about 300 ms.
5. View menu (F10): `Inlay Hints` is listed after `Soft-Wrap` and runs the same toggle.
6. Column mode (Alt+Shift+Insert or the menu): Alt+Shift+drag over a hinted line. The block is
   selected by text columns: on hinted lines it is shifted on screen, but the right chars are
   selected. Escape. In `github-dark` (`[ui] theme = "github-dark"`) the hint keeps
   a visible box (lighter than the current-line background) on the caret lines.
7. Undo every edit; do not save. Repeat at 120x40 and 80x24.

### Scenario: live templates

Setup as for the other scenarios with a fresh `$H` and LSP off; `S=$H/config/jetl/snippets`.

1. Open `src/lib.rs`, Ctrl+End, Enter, type `fn`, Tab: `fn name() {` with `name` selected; type
   `go`, Tab, Tab: the tabstops cycle (parens, then the indented body line). Ctrl+Z once. Edit
   menu: `Expand Live Template` is listed with its Tab hint.
2. `mkdir -p $S`, then write `$S/rust.toml` with a heredoc (`[[snippet]]`, `prefix = "pp"`,
   `description = "debug print"`, `body = 'println!("{:?}", ${1:x});$0'`). Within a second the toast
   `Reloaded snippets` shows. Type `pp`, Tab: `println!("{:?}", x);` with `x` selected. Undo. Type
   `p`, Ctrl+Space: the popup lists `pp` with `debug print`. Esc, undo.
3. `printf '[[snippet]\n' > $S/rust.toml`: an error toast `snippets/rust.toml is invalid: ...`;
   `fn` Tab still expands; `pp` Tab inserts an indent instead. Undo. Restore the good file: toast
   `Reloaded snippets`, `pp` Tab works again.
4. File > Settings…, Editor > General, select `Live templates (snippets)`: the help shows
   `Folder: $H/config/jetl/snippets`. Flip it off, OK: `fn` Tab indents, Ctrl+Space after `ma` shows
   `Language servers are off ...`. Flip it back on.
5. In `src/lib.rs` type `"fn"` (inside quotes) and Tab: an indent, no `fn name`. Same for `// fn` and Tab. Ctrl+Space after `fn` inside the string still lists `fn`. Undo.
6. Variables: write `$S/rust.toml` with a `hdr` snippet (`body = '// $TM_FILENAME ($CURRENT_YEAR-$CURRENT_MONTH-$CURRENT_DATE) ${NOPE:x}$0'`)
   and wait for `Reloaded snippets`. In `src/lib.rs`, `hdr` Tab gives `// lib.rs (<today>) x`. For a
   file without a language copy the same snippet to `$S/all.toml` (`rust.toml` does not apply to it),
   then File > New Scratch File: `hdr` from Ctrl+Space gives `// scratch_1.txt (<today>) x`. Undo.
7. Surround With: in `src/lib.rs`, select two whole lines (Shift+Down twice from a line start) and
   press Ctrl+Alt+T: the `Surround With` dialog lists `if  if block` and `while  while loop`.
   Choose `if`: the lines are wrapped and indented, `condition` is selected. Ctrl+Z restores them.
   With no selection on a code line, Ctrl+Alt+T and `while` wrap that line. The Edit menu lists
   `Surround With…` with `Ctrl+Alt+T`. If the terminal swallows Ctrl+Alt+T (some desktops bind
   it), use Edit > Surround With….
8. Undo every edit; do not save. Repeat at 120x40 and 80x24.

### Scenario: run configurations

Setup as for the other scenarios with a fresh `$H` and LSP off; write `$R/.jetl/run.toml` with
two `[[configs]]` entries, `where` (`command = "pwd"`) and `hello` (`command = "echo hello"`).

1. Alt+4: the toolbar reads ` Select Configuration ▾ `, ` ▶ Run `, ` ↻ Rerun ` (dim), ` ■ Stop `
   (dim); the second row is the hint `No configuration has run yet: ...`.
2. `send-keys M-S-F10`, pick `hello`: the picker shows ` hello ▾ `, the output is `hello`, the
   status line is on the second row. F1, `Run: choose`, pick `where`: it runs.
3. `send-keys S-F10`: `where` runs again (the last pick is the selected one). Pick `hello`, quit
   (`C-q`), start again: ` hello ▾ ` is back and nothing ran; `S-F10` runs `hello`.
4. Record whether `S-F10` and `M-S-F10` arrive under tmux (they must). Repeat at 120x40 and
   80x24: the picker is kept, the status line stays on the second row.
5. Add a `[[run.configs]]` `global check` to `$H/config/jetl/config.toml`. F1, `Run: edit run
   configurations` (or the toolbar button, or Run > Edit Configurations…): both project entries
   and `global check` (origin `config.toml`) are listed. Enter, Enter on the latter's `Command`:
   the toast `Defined in config.toml: Copy makes an editable project copy`.
6. Ctrl+D copies it; rename the copy `check2` (Enter on `Name`), set `Environment` to
   `A=1 B='x y'` and `Working directory` to `src`, Ctrl+S: `cat $R/.jetl/run.toml` keeps the
   comment and both entries and has the new `[[configs]]`; `M-S-F10` lists `check2`.
7. Edit again, Esc: `Discard changes to run configurations?`. Insert, Tab to the buttons, OK with
   no command: an error toast, nothing written. Repeat at 120x40 and 80x24: all six field rows,
   the help and the three buttons are visible.

### Scenario: select a run configuration without running it

Needs `gdb` 14 or newer and `gcc`. The fixture `/tmp/jetl-dap-demo` of `### Scenario: Debug and Stop`, at 80x24 and at
120x40, with `[lsp] enabled = false` and the toolbar on (the default). Add to its `.jetl/run.toml`, after `demo`:
`[[configs]] name = "marker"`, `command = ["sh", "-c", "touch marker-ran"]`, and `[debug.marker]` with `adapter = "gdb"`
and `launch = { program = "${workspaceFolder}/demo" }`. Start jetl on the file, `jetl /tmp/jetl-dap-demo/demo.c`, so the
editor has the focus (Shift+F10 in the Project tree opens its context menu). Delete `marker-ran` and use fresh `XDG_*`
directories before each size, since step 7 keeps `marker` in the session. After every step that must run nothing, `ls marker-ran` fails. At 80x24 the toast of step 3 wraps over two lines.

1. Negative control, nothing selected: Shift+F9 shows `Debugging demo with gdb`, then `Debug session ended, exit code 0`.
2. `send-keys M-S-F9`: the title `Select Run Configuration`, the rows `demo` and `marker`. Escape: the toolbar does not
   name `marker`.
3. `send-keys M-S-F9`, type `marker`, Enter: the toast `Selected run configuration 'marker'`, the toolbar names `marker`,
   the Run window stays hidden. `ls marker-ran` fails.
4. `send-keys S-F9`: `Debugging marker with gdb`, then `Debug session ended, exit code 0`. `ls marker-ran` still fails
   (Debug starts the block's `program`, not the command), and no `gdb` is left.
5. Positive control: `send-keys S-F10` runs `marker`, and `ls marker-ran` now succeeds. Delete it.
6. `send-keys M-S-F10`: the title `Run`. `send-keys M-S-F9`: the title `Select Run Configuration`. Escape.
   F1, type `Run: select`, Enter: the same picker. Escape. `ls marker-ran` fails.
7. Ctrl+Q and start again: the toolbar names `marker`, and `ls marker-ran` fails.

### Scenario: test runner tree

Setup as for the other scenarios (fresh `$H`, LSP off) with the scratch crate `/tmp/jt-crate`
(`adds`, `fails`, ignored `slow`, `tests/cli.rs`); open jetl on it instead of `$R`.

1. Make a configuration `cargo test --no-fail-fast` (Edit Configurations, or `[[configs]]` in
   `.jetl/run.toml`): plain `cargo test` stops at the first failing binary, so `tests/cli.rs` and
   `Doc-tests` would not run. `send-keys M-S-F10`, pick it (or F1, `Run: choose`): the Run window
   opens in the Tests view: suites `src/lib.rs`, `tests/cli.rs`, `Doc-tests jtc`; under
   `src/lib.rs` the group `tests` with `adds` `✓` and `slow` `○`, `fails` `✗` expanded with
   `panicked at src/lib.rs:...` underlined. The status line reads
   `Tests: 2 passed, 1 failed, 1 ignored`.
2. Select `fails`, Enter: `src/lib.rs` opens at the assert. Click the underlined link: the same.
   F4 from another window steps to the failure.
3. Left on `fails` hides its output, Right shows it; Left on a suite hides its tests.
4. ` Rerun Failed ` (toolbar, or F1 `Run: rerun the failed tests`): the header shows
   `cargo test --no-fail-fast -- --exact tests::fails`; the tree has only `fails`. With no failure the button is
   dim and F1 says `No failed tests`.
5. `t` (Run window focused) shows the raw output, `t` again the tree; the ` Output ` / ` Tests `
   tabs switch the same way.
6. `printf 'x' >> /tmp/jt-crate/src/lib.rs`, run `cargo test`: the window ends in the Output view
   with the rustc error linked. Restore the file.
7. Repeat at 120x40 and 80x24: the tabs show at 120 and go first at 80; the tree rows stay inside
   the window.

### Scenario: run test at caret

Same setup as the test runner tree (`/tmp/jt-crate`).

1. Open `src/lib.rs` (Ctrl+N), Ctrl+G to the line of `fails`' assert, `send-keys C-S-F10`: the Run
   header reads `cargo test tests::fails -- --exact` and the tree has only `fails`.
2. Caret on the `#[test]` line above `adds`: only `adds` runs. Caret in `pub fn add`: the note
   `No test at the caret`, nothing starts.
3. Open `tests/cli.rs`, caret in `outside`: `cargo test --test cli outside -- --exact`.
4. Run > Run Test at Caret (or F1, `Run: run the test at the caret`) does the same. A modified,
   unsaved buffer runs and says `Running the saved version of lib.rs`. A `.txt` file says
   `Run Test at Caret supports Rust, Python, JavaScript and TypeScript`.
5. Repeat at 120x40 and 80x24.

### Scenario: HTTP request at caret

Setup: `T=/tmp/jt-http; mkdir -p $T; python3 -m http.server 8765 -d $T &` (or any local echo server),
`$T/api.http` with a `GET http://127.0.0.1:8765/` block, a `### Post` block `POST http://{{host}}/x`
with `Content-Type: application/json` and a JSON body, and `$T/http-client.env.json`
`{"dev": {"host": "127.0.0.1:8765"}}`.

1. Caret in the GET block, `send-keys C-M-r`: the Run tab is named `GET http://127.0.0.1:8765/`,
   shows `HTTP/1.0 200 OK`, the headers and the listing, no `^M`.
2. Caret in the POST block: the request goes out (python's server answers 501, which is still shown).
   Ctrl+F5 reruns it.
3. Run > Send HTTP Request at Caret and F1 do the same. A `.txt` file says
   `Send HTTP Request needs a .http file`.
4. Start jetl with `env PATH=/nonexistent` via the absolute binary path: `curl not found in PATH`.
5. Repeat at 120x40 and 80x24.

### Scenario: jest tests tree

Setup: a scratch project `/tmp/jt-jest` (needs the npm registry). `package.json`:
`{"name":"jestprobe","version":"1.0.0","devDependencies":{"jest":"^29"}}`, then
`npm install --ignore-scripts --no-audit --no-fund`. `src/math.test.js`:

    describe('math', () => {
      test('adds', () => {
        expect(1 + 1).toBe(2);
      });
      describe('nested', () => {
        test('fails', () => {
          expect(1 + 1).toBe(3);
        });
      });
      test.skip('skipped', () => {});
      test.todo('later');
      test('f(x) [1] b.c', () => {
        console.log('hello from test');
        expect([1]).toEqual([2]);
      });
    });
    test('top level', () => {});

`src/other.test.js`:

    test('other ok', () => {});
    describe('deep', () => { describe('er', () => { test('leaf', () => {}); }); });

`src/broken.test.js`: `throw new Error('boom');` then `test('never', () => {});`. Check that
`npx jest --verbose` runs in that directory, then open jetl on it with the usual fresh `$H` and LSP
off. The parser fixtures `src/run/fixtures/jest_verbose.txt` and `jest_default.txt` were captured
from this project with jest 29.7.0 by `npx jest --verbose </dev/null > verbose.txt 2>&1` and
`npx jest </dev/null > nonverbose.txt 2>&1`; jest runs files in parallel, so the order of the file
blocks can differ between runs (the parser does not depend on it).

1. A configuration `command = ["npx", "jest"]`, run it: the header reads `npx jest --verbose`, the
   Tests view has suites `src/math.test.js`, `src/broken.test.js`, `src/other.test.js`, group
   `math › nested` with `fails` `✗` expanded, `Test suite failed to run` under the broken file, status
   line `Tests: 4 passed, 3 failed, 2 ignored`.
2. Enter on `fails` opens `src/math.test.js` at line 7.
3. Rerun Failed: the header shows the escaped files and `-t`, the tree has `fails`, `f(x) [1] b.c` and
   the broken file. The other tests of `math.test.js` show as `○` (jest prints the filtered-out
   tests as skipped): status `0 passed, 3 failed, 4 ignored`.
4. Caret inside `test('adds'` in `src/math.test.js`, `send-keys C-S-F10`:
   `npx jest src/math.test.js -t '^math adds$' --verbose`, one passed test (the other tests of the
   file show as `○`). Then caret inside `test('fails'`, C-S-F10:
   `npx jest src/math.test.js -t '^math nested fails$' --verbose`, one failed test. Rerun Failed
   after it: the header has exactly one `-t`, `^(?:math nested fails)$`, and `fails` fails again.
5. Repeat at 120x40 and 80x24.

### Scenario: last tab closed

Setup as above.

1. Open `src/main.rs` and `README.md` (Ctrl+N). Ctrl+W: `README.md` closes and the caret stays in
   `src/main.rs`; Down moves the caret, not the tree.
2. Ctrl+W again: the editor is empty, the Project window is shown and focused; Down moves the tree
   selection.
3. Hide the Project window (Alt+1), open a file, type a character, Ctrl+W, choose "Don't save": the Project
   window comes back focused. Repeat with the tab's close button (a click on its `x`).
4. Repeat at 120x40 and 80x24. No stderr; Ctrl+Q exits with status 0.

### Scenario: tool strip overflow

Setup as above, `[lsp] enabled = false`.

1. At 120x40 the strip shows two-row buttons (Alt-number over the icon) for all 13 windows.
2. At 80x24 every button is one row, icon only: the menu cell on screen row 1, Project, Worktrees and
   Structure on rows 3-5, the bottom group upwards from Find on row 23 to Hierarchy on row 14 (TODO `✎` on
   row 15). A click on each toggles its window; hovering shows its name and Alt key.
3. `tmux resize-window -t jetl -x 80 -y 12`: the strip's last row is `⋯`. Hover: "More Tool Windows".
   A click opens a menu above it with Local History, Tasks, TODO and Hierarchy; Enter on TODO opens it.
4. Move Git to the right (right-click its button, Move to > Right Top) at 80x12: the right strip shows it; the
   left More menu changes: Local History gets a button and the menu lists Tasks, TODO and Hierarchy.
5. Move Structure to Left Bottom and Git to Right Top, then `tmux resize-window -t jetl -x 80 -y 3`: the left strip's second row is
   `⋯`, and the right strip's last row is `⋯` too. Back to 80x24 with `resize-window`: the buttons return.
   No stderr; Ctrl+Q exits with status 0.

### Scenario: tool strip order and drag

Setup as above, `[lsp] enabled = false`, at 120x40 (two-row buttons) and 80x24 (one-row buttons). Use the
SGR `press`/`release` helpers; a drag is `press 0 c r; press 32 c2 r2; release 0 c2 r2`.

1. Click a button (press, then release on it): its window toggles on the release, not the press.
2. Drag Find (lowest button) onto Problems: while the pointer is over Problems its button is in the drop
   colour; after the release Find sits where Problems was.
3. Drag Git onto Worktrees: Git moves to the left side, its button right above Worktrees; Alt+9 opens it on
   the left.
4. Right-click Hierarchy, Move to > Right Top; drag Git onto Hierarchy on the right strip: Git moves right.
5. Right-click Git: Move Button Up/Down are greyed when there is no neighbour that way; Move Button Down
   swaps it with the button below.
6. Ctrl+Q and start again: the order and sides are back. No stderr; Ctrl+Q exits with status 0.

### Scenario: strip groups and Move to

Setup as above, `[lsp] enabled = false`, at 120x40 and then 80x24.

1. Right-click the Structure button, Move to > Left Bottom: a `───` row appears under Worktrees with Structure
   below it. Alt+1 and Alt+7: the left side shows Project over Structure with a splitter between them.
2. Right-click Problems, Move to > Bottom Right: a right strip appears with Problems at its foot; Alt+6 and
   Alt+4 (Find): the bottom shows Find and Problems side by side.
3. Right-click Structure again: in the Move to submenu "Left Bottom" is greyed.
4. Drag Git (bottom group) into the empty strip space just under Structure: the `━` marker shows there; on
   release Git joins Left Bottom (Alt+9 shows it in the lower left pane, replacing Structure).
5. Drag Structure onto Git's button: both stay in Left Bottom, their buttons swap.
6. Find the `───` row between the top and split groups on the left strip. Drag Structure (after step 5 it is
   the second button of Left Bottom) onto that row: Git's button shows the drop colour while hovering, and
   after the release Structure is still below the `───`, now above Git. Drag Project onto the row: Worktrees
   shows the drop colour, and Project stays above the `───`.
7. View menu: six Move Tool Window rows; with Git focused its own dock row is greyed.
8. At 80x24 the one-row buttons and separators fit; a strip that runs short shows `⋯`.
9. Shift+F12 (Restore Default Layout): separators and the right strip disappear. Ctrl+Q and restart after
   step 5: the groups come back. No stderr; Ctrl+Q exits with status 0.

### Scenario: Toolbar

Setup as above with the default config (`[lsp] enabled = false` only), at 120x40 and 80x24.

1. Row 1 is ` ≣ ` on the strip colour; the strip starts on row 2 without a menu cell (at 80x24 Project,
   Worktrees, Structure on rows 2-4, Find on row 23 up to Hierarchy on row 14).
2. Hover column 2, row 1: `Main Menu  F10` shows on row 2.
3. Click column 2, row 1: the menu bar opens over row 1; a second click closes it. F10 still toggles it.
4. Row 1 reads ` ≣ ` then ` <dir> ▾ ` from column 5 and ends with ` ⌕ ` ` ⚙ ` at the right edge. A click on the
   project widget opens Recent Projects, on ` ⌕ ` Search Everywhere, on ` ⚙ ` Settings (Esc closes each); hover on
   ` ⚙ ` shows `Settings` and its key on row 2. At 80x24 with a long run configuration name or
   `resize-window -x 50`, Settings goes first, then Search, then the project widget shrinks to ` ▾ `; ▶ stays.
   Started in a linked worktree (`git worktree add /tmp/tm22-wt` from the test repo, open `/tmp/tm22-wt`), the
   label reads `tm22-wt (<repo dir>)`.
5. Row 1 holds ` Select Configuration ▾ ` and ` ▶ ` before the ` ⌕ ` ` ⚙ ` pair; hover ▶: `Run  Shift+F10` shows on row 2. With no configuration selected, a click on ▶ opens the centred Run palette; Esc closes
   it and nothing has run.
6. With `$ROOT/.jetl/run.toml` holding `[[configs]]`, `name = "nap"`, `command = ["sh", "-c", "sleep 20"]`, click
   the name: a menu opens on row 2 under the widget's left edge (shifted left at the right edge) listing `nap`, a
   divider, `Edit Configurations…`, `Rerun Failed Tests` (dim), `Stop` (dim); pick `nap`: the widget reads
   ` nap ▾ ` and nothing runs; click ▶: the task starts and the widget reads ` ⠋ nap ▾ ` in the accent colour
   with ` ↻ ` and ` ■ `. Hide the Run window (Alt+4): the spinner keeps moving.
6a. Alt+4 to show the Run window, click its ` nap ▾ ` picker: the same menu opens at the picker's left column
   (above it when there is no room below); while `nap` runs `Stop` is enabled and choosing it stops the task;
   `M-S-F10` still opens the centred Run palette.
6b. Click the name widget, then `Edit Configurations…` (or reach it with Down and Enter): the menu closes and the Run
   Configurations window lists `nap`. Esc closes it with no question, and `.jetl/run.toml` is unchanged.
7. Click ■: the task stops and the widget returns to ` nap ▾ ` and ` ▶ `; click ▶: it runs again.
8. At 80x24 with a configuration named `integration-tests`, the toolbar still shows ▶ before ` ⌕ ` ` ⚙ ` (or at the right edge once those drop).
9. `resize-window -y 11` hides the row and the strip's menu cell is back on row 1. Zen (its key from
   `docs/KEYMAP.md`) hides it too.
10. `printf '[lsp]\nenabled = false\n[ui]\ntoolbar = false\n' > $C/config.toml` removes it at once;
   `toolbar = true` brings it back.
11. Default config again, 120x40: right press on column 40, row 1 (empty between the groups) opens a one-row menu "Hide Toolbar" below
   the press; Enter hides the row, the strip's menu cell is back on row 1 and `$C/config.toml` holds
   `toolbar = false` under `[ui]` with `[lsp] enabled = false` kept. F10, Right to View, the "Toolbar" row
   (just above "Zen Mode"), Enter brings the row back and the file holds `toolbar = true`. A right press on
   the ` ≣ ` cell opens the same menu, not the main menu. Stderr empty, Ctrl+Q exits 0.
12. In a repo stopped mid-merge (the conflict setup above), row 1 shows ` ⎇ main (Merging) ▾ ` after the project
   widget and the status bar's right end has no `main`; a click on the widget opens Git Branches. With
   `toolbar = false` written as in step 10 the status bar shows `main` again. At 50 columns the widget drops.
13. In the test repo at 120x40, row 1 shows ` ↓ ` ` ✓ ` ` ↑ ` right after ` ⎇ main ▾ `. Hover each: `Update Project`,
   `Commit`, `Push` with their keys from `docs/KEYMAP.md` on row 2. Click ✓: the Git window opens on the Commit
   tab with the message focused and nothing is committed. Click ↓ (the test repo has no upstream): a git error
   notification, no crash. `resize-window -x 99`: the three buttons are gone, the branch widget stays;
   `resize-window -x 100` brings them back. Open jetl
   at 120x40 on a directory that is not a git repository (`mkdir /tmp/jt-nogit`): row 1 has no ` ⎇ ` and none of ` ↓ ` ` ✓ ` ` ↑ `,
   5 s after the start and after `resize-window -x 100`. Stderr empty, Ctrl+Q exits 0.
14. In the test repo at 120x40, row 1 shows ` ⎇ main ▾ ` and the status bar's right end has no `main`. F10: the menu bar covers
   row 1 and the status bar's right end shows `main`. F10 again: the widget is back and the status bar's `main` is gone. At 80x24
   the same holds, with `main` right of the dropdown's last row. Stderr empty, Ctrl+Q exits 0.

### Scenario: unpinned tool window

Setup as above, `[lsp] enabled = false`, at 120x40 and 80x24.

1. Right-click the Git button: the last row is View Mode; choose View Mode > Dock Unpinned. Alt+9 focuses Git; click in the editor: Git
   hides.
2. Alt+9 again; Ctrl+Shift+A (palette) and Esc, then F10 and Esc: Git stays shown through both.
3. In Git press Esc until focus returns to the editor: Git hides. Alt+9, then Alt+1: Git hides, Project shows.
4. Right-click Git > View Mode: Dock Unpinned is greyed. Ctrl+Q and restart: it is still greyed. Shift+F12,
   right-click Git > View Mode: Dock Pinned is greyed (every window Pinned again). Stderr empty, Ctrl+Q exits 0.

### Scenario: named tool window layouts

Setup as above, `[lsp] enabled = false`, at 120x40 and 80x24, state in `/tmp/jt-home/state`; remove
`/tmp/jt-home/state/jetl/layouts.json` first.

1. F10, View: "Switch Layout…", "Rename Layout…", "Delete Layout…" and "Save Changes in Current Layout" are dimmed.
   Choose "Save Current Layout as New…", type `Default+`, Enter: toast `Saved layout 'Default+'`;
   `cat /tmp/jt-home/state/jetl/layouts.json` shows `"version": 1` and the layout.
2. Alt+9 (Git), move it to Right Bottom from its button's right-click menu, set Find to Undock from its menu,
   save as new `Review`. Shift+F12 (Restore Default Layout).
3. Ctrl+Shift+A, type `switch layout`, Enter: the dialog lists `Default+` and `Review (current)`; pick `Review`:
   Git is back at Right Bottom, Find is Undocked (opening it slides over the editor).
4. Rename `Review` to `Review2` (View | Rename Layout…): toast `Renamed layout 'Review' to 'Review2'`. Delete `Default+`
   (Delete, then confirm): toast `Deleted layout 'Default+'`; the file holds only `Review2`.
5. In another shell `chmod 555 /tmp/jt-home/state/jetl`. Save as new `Fail`: a warning toast starting `Saving layouts:`,
   no `Saved layout 'Fail'`. Ctrl+Shift+A "switch layout": the dialog lists `Review2 (current)` and no `Fail`; Esc. Delete
   `Review2` (Delete, then confirm): a warning again, no `Deleted layout`, and the dialog still shows `Review2 (current)`.
   `chmod 755 /tmp/jt-home/state/jetl`; View | Save Changes in Current Layout: toast `Saved layout 'Review2'`.
6. Ctrl+Q, start again in another root: Ctrl+Shift+A "switch layout" lists `Review2`. Stderr empty, Ctrl+Q exits 0.

### Scenario: undocked tool window

Setup as above, `[lsp] enabled = false`, at 80x24 and 120x40.

1. Open a file. Right-click the Structure strip button > Move to > Right Top, then right-click again > View Mode >
   Undock. Alt+7: Structure appears over the right part of the editor; the editor text does not reflow.
2. Click into the editor text: the caret moves, Structure stays. Click into Structure: it takes focus. Esc:
   Structure slides away, the editor has focus.
3. Alt+7 again, then Alt+1: Structure hides, Project is focused.
4. Undock Find too (right-click Find > View Mode > Undock), Alt+7 then Alt+3: only Find shows, at the bottom of
   the editor area.
5. With the file from step 1 at least 100 lines long and Find still shown from step 4 (Alt+3 again would hide it): click into the
   editor text above Find. Ctrl+End: the caret goes to the last line, under Find, and no terminal cursor shows on Find
   (`/tmp/tm.sh display-message -t jetl -p '#{cursor_flag}'` prints 0). Ctrl+Home: the cursor is back on the caret, the flag is 1.
6. Undock Project (right-click its button > View Mode > Undock), Alt+1, type `a`: the filter narrows the tree. Esc: the filter
   clears, Project stays shown and focused. Esc again: Project slides away, the editor has focus.
7. View menu: "Undock or Dock Tool Window" is greyed with the editor focused, enabled with a tool window focused.
8. Ctrl+Q, restart: Structure, Find and Project are still Undocked (View Mode submenu shows "Undock" greyed). Shift+F12
   (Restore Default Layout): all three are Pinned again. Stderr empty, Ctrl+Q exits 0.

### Scenario: Structure for C#, Kotlin, Ruby, PHP and Swift

Scratch project with `a.cs`, `a.kt`, `a.rb`, `a.php`, `a.swift` (a namespace/module/protocol, a class with a method and a property, a free function, a local variable in a method). Open each file and press Alt+7: the Structure window lists the definitions with their kind and nesting (methods under their class), and no local variable. Put the caret in a method: the breadcrumb row shows the enclosing class and method. Ctrl+Q exits 0, stderr empty.

### Scenario: Symbol index cache (warm start)

Env as in the other scenarios plus `XDG_CACHE_HOME=/tmp/jt-home/cache RUST_LOG=jetl=debug`; log at `/tmp/jt-home/state/jetl/jetl.log`. Use a scratch project with a few `.rs` files, `[lsp] enabled = false`, 120x40.

1. `rm -rf /tmp/jt-home/cache/jetl/index`; start jetl; the log shows `symbol index: N files` and `symbol cache: wrote N files`. Ctrl+Q exits 0.
2. `ls /tmp/jt-home/cache/jetl/index/` lists one `<16 hex>.json`.
3. Start again: the log shows `symbol cache: reused N of N files` and no new `symbol cache: wrote`; Go to Symbol (Alt+Shift+N, `palette.symbols`) finds a symbol at once.
4. `touch` one `.rs` file and start again: `reused N-1 of N files` and one `symbol cache: wrote`. Stderr empty each time.
5. `printf 'fn x() {}\0\n' > bin.rs` in the project and start twice: the second start's log has `reused` for every file except `bin.rs` and no new `symbol cache: wrote` line (`grep -c 'symbol cache: wrote'` before and after). Stderr empty each time.

### Scenario: approximate call hierarchy

Run with `[lsp] enabled = false`, jetl's own repo as the root, at 120x40 then 80x24.

1. Start jetl and wait a few seconds for the symbol index (Go to Symbol finds `fallback_usages`). Open `src/app/lsp_nav.rs` and put the caret on `fallback_usages` in `pub(crate) fn fallback_usages` (Ctrl+G to its line, see `grep -n "fn fallback_usages" src/app/lsp_nav.rs`).
2. Navigate > Call Hierarchy (Ctrl+Alt+H): the Hierarchy window opens focused, titled `Callers of fallback_usages (approximate)`, root `fallback_usages  lsp_nav.rs:<line>`, a child `lsp_usages_without_server  lsp_nav.rs:<line>`.
3. Tab: the title becomes `Callees of fallback_usages (approximate)`; the children include `usage_group` and `refs_named`. Right on a child expands it at once (no `…`). F5 shows the same tree.
4. Enter on `usage_group`: the editor opens `lsp_nav.rs` with the caret on the name `usage_group` of its definition.
5. Caret on a word that is no definition anywhere (a local variable): Ctrl+Alt+H shows the toast `No call hierarchy here`.
6. Large project (yman 394): root a copy under /tmp of part of the cargo registry (`cp -r ~/.cargo/registry/src/index.crates.io-*/[a-e]* "$T"`), with `XDG_*` under /tmp. Wait for the symbol index, open a long function, Ctrl+Alt+H, Tab: the callees appear with no visible pause, and Right on a child expands it at once. Stderr is empty, Ctrl+Q exits 0.

### Scenario: file dependencies

Run with `[lsp] enabled = false`, jetl's own repo as the root, at 120x40 then 80x24.

1. Start jetl and wait a few seconds for the symbol index. Open `src/app/hierarchy.rs` (Ctrl+Shift+N).
2. Navigate > File Dependencies: the Hierarchy window opens focused, titled `Imports of src/app/hierarchy.rs (approximate)`, root row `src/app/hierarchy.rs`, children such as `src/actions.rs` and `src/lsp/hierarchy.rs` (the files its `use crate::..` lines name), with no line label after them.
3. Down, Right on `src/actions.rs`: its imports appear at once (no `…`).
4. Tab: the title becomes `Imported by src/app/hierarchy.rs (approximate)` with `src/app/mod.rs` as a child (it declares `mod hierarchy;`). F5 shows the same tree. Export Hierarchy as Mermaid writes edges from the importing file to the imported one.
5. Enter on `src/app/mod.rs`: the editor opens it with the caret at line 1.
6. Open `README.md` and run Navigate > File Dependencies: toast `No file dependencies for this file type`, the tree is unchanged. Stderr is empty, Ctrl+Q exits 0.
7. Java split package (yman 388), root `$T` with `src/main/java/com/acme/Core.java`, `src/test/java/com/acme/CoreTest.java`, `src/test/java/com/other/T.java` (`package com.other; import com.acme.*;`), `src/main/java/com/solo/Impl.java` and `src/test/java/com/solo/OnlyTest.java` (`package com.solo; import com.solo.*;`). Open `src/test/java/com/other/T.java` and run File Dependencies: title `Imports of src/test/java/com/other/T.java (approximate)`, children `src/main/java/com/acme/Core.java` and `src/test/java/com/acme/CoreTest.java`.
8. Open `src/test/java/com/solo/OnlyTest.java` and run it again: the one child is `src/main/java/com/solo/Impl.java`. Stderr is empty, Ctrl+Q exits 0.

### Scenario: structural search

Run with `[lsp] enabled = false` in a scratch project, at 120x40 then 80x24 (the AST button must stay visible).

1. `mkdir -p $T/src; printf 'fn f() {\n    let a = p(i).unwrap();\n    a.unwrap_or(0);\n}\n' > $T/src/s.rs; printf 'x.unwrap()\n' > $T/notes.txt`. Start jetl, Alt+3 (or Ctrl+Shift+F), then Alt+T: the AST button is highlighted, Cc, W and .* are dimmed.
2. Type `$A.unwrap()`: status `1 match in 1 file`, row `src/s.rs`; `notes.txt` is absent (no grammar).
3. Alt+X: toast `Case, whole word and regex do not apply to a structural search`, `.*` stays off. Alt+A: toast `Structural search cannot replace; Alt+T turns it off`.
4. Enter opens `src/s.rs` at line 2, column 13. Back in the Find window, Alt+T: AST is off and the literal text `$A.unwrap()` finds no matches.
5. A build with `--no-default-features --features lang-rust` has no AST button; Alt+T toasts `Structural search is not in this build (cargo feature "structural")`. Stderr is empty, Ctrl+Q exits 0.

### Scenario: select all in an input dialog

At 120x40 and 80x24 (yman 391), `[lsp] enabled = false`, scratch project and session as above:

```sh
/tmp/tm.sh send-keys -t jetl Down S-F6; sleep 0.5   # the Project window starts focused (M-1 would hide it); Down selects src
/tmp/tm.sh send-keys -t jetl C-a; sleep 0.2
/tmp/tm.sh capture-pane -ep -t jetl      # the name in the field has the selection background, the rest of the field not
/tmp/tm.sh send-keys -t jetl -l x; sleep 0.2
/tmp/tm.sh capture-pane -p -t jetl       # the field reads x
/tmp/tm.sh send-keys -t jetl -l abc; /tmp/tm.sh send-keys -t jetl C-a BSpace; sleep 0.2
/tmp/tm.sh capture-pane -p -t jetl       # the field is empty
/tmp/tm.sh send-keys -t jetl -l abc; /tmp/tm.sh send-keys -t jetl C-a Left; /tmp/tm.sh send-keys -t jetl -l z; sleep 0.2
/tmp/tm.sh capture-pane -p -t jetl       # the field reads zabc
/tmp/tm.sh send-keys -t jetl C-a; /tmp/tm.sh set-buffer pasted; /tmp/tm.sh paste-buffer -p -t jetl; sleep 0.2
/tmp/tm.sh capture-pane -p -t jetl       # the field reads pasted
/tmp/tm.sh send-keys -t jetl Escape; sleep 0.5
git -C $T status --short                 # prints nothing
```

Ctrl+A on an empty field draws nothing. In other text fields (Find, the palette) Ctrl+A is unchanged.

### Scenario: refactoring keys

At 120x40 and 80x24 (yman 399), real servers, `/tmp/tm24.sh` as `/tmp/tm.sh` above, `XDG_*` under `/tmp/jetl-399/home`, a fresh copy of the demo per step that changes files. Needs plain `cargo build` (TypeScript and Python grammars). Move the caret with `C-g`, `-l 'LINE:COL'`, `Enter` and check `Ln`/`Col` first; select with `S-Right`; undo with `C-z`; wait 15 s for rust-analyzer. The keys are `C-M-v`, `C-M-c`, `C-M-n`, `C-F6`, `F6` and `M-DC`; Extract Method by the palette (`F1`, `extract method`) or `F10`, `Right`, Up to its row.

rust-analyzer (`src/main.rs` of a small crate):

1. `C-M-m`: arrives as Alt+Enter and opens the Context Actions dialog (the documented limit). Escape.
2. Select two statements, palette Extract Method: `fun_name` appears, no dialog. Undo. Again through the Edit menu row.
3. Select `first * 3`, `C-M-v`: Extract into variable runs. Select `1 + 2`, `C-M-c`: Extract into constant. Caret on a local, `C-M-n`: Inline variable. Each undone with `C-z`.
4. Caret on the `unused` parameter of `with_unused` (the fixture also calls it as `let plain = with_unused(3, 4);` and inside `println!`), `C-F6` and then `M-DC`: the signature loses `unused` and the plain call becomes `with_unused(3)`; the call inside `println!` stays `with_unused(1, 2)` (rust-analyzer does not rewrite it, so the code no longer compiles; jetl applies what the server sends). Undo after each.
5. Caret on `dead_code`, `F6` and `M-DC`: toast `No such refactoring here`, text unchanged. On an unused import, `M-DC`: the same (Remove all unused imports is not taken).
6. Caret on `mod inner`, `F6`: `src/inner.rs` exists (a file operation, not undone).
7. In a git repo, Alt+9 then `C-M-n`: the new branch dialog, not Inline.

typescript-language-server (`main.ts`, `tsconfig.json`): two statements selected, palette Extract Method: a dialog `Extract Method` with two rows; Escape changes nothing; choosing Extract to function in module scope extracts with no error toast. Selected `1 + 2`, `C-M-c`: two constant rows; `C-M-v`: the toast. `C-M-n` on a const: Inline variable. `C-F6` on a parameter list: Convert parameters to destructured object. `M-DC` on an unused function: Remove unused declaration, no dialog. `M-Enter` on the same unused function: the Context Actions dialog lists `Remove unused declaration for: 'deadCode'`, `Delete all unused declarations` and `Move to a new file`; choosing the second removes `deadCode` (undo restores it). `F6` on a class: Move to a new file (`K.ts` is made).

pyright and basedpyright (`[lsp.servers.pyright] command = ["basedpyright-langserver", "--stdio"]`): each key gives the toast. With `[lsp] enabled = false` a key toasts `Language servers are off ([lsp] enabled = false)`. Stderr is empty and Ctrl+Q exits 0 in every run.

### Limits under tmux

- No kitty keyboard protocol: Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter,
  Shift+Enter, Shift+F3, Alt+Shift+Tab, double Shift and Ctrl+digit do not arrive. Use the legacy
  fallbacks (Ctrl+N for files, Alt+Shift+N for symbols, F1 for actions, Alt+Left/Right for tabs,
  Alt+Enter to commit, Alt+B then a digit for numbered bookmarks) or check those keys in foot or
  kitty.
- Ctrl+Alt+M (Extract Method) arrives as Alt+Enter (`C-M-m` is `ESC CR`); use Edit >
  Extract Method or the palette.
- Ctrl+Alt+S arrives as `C-M-s` in legacy mode and opens the Settings window;
  File > Settings… is the fallback.
- Escape followed quickly by another key arrives as Alt+key; pause between
  them.
- Ctrl+Alt+Shift+J (select all occurrences) needs the kitty protocol; use
  Ctrl+Alt+J. Alt+Shift+J arrives as Alt+J with Shift and works.
- The folding keys Ctrl+-, Ctrl+=, Ctrl+Alt+-, Ctrl+Alt+=, Ctrl+Shift+- and
  Ctrl+. need the kitty protocol (Ctrl+- arrives as Ctrl+7, toggle comment;
  Ctrl+= and Ctrl+. as plain text); use Alt+, Alt+. Alt+< Alt+> or the View
  menu.
- Shift+drag never reaches jetl in foot, kitty or alacritty (they select
  natively), so neither does Alt+Shift+drag; some window managers take
  Alt+click and Alt+drag. Send the SGR bytes through tmux to test those
  gestures (Alt adds 8 to the button, Shift 4, motion 32).
- Pressing Alt+number for a focused tool window hides it; later keys go to
  the editor.
- OSC 52 copy needs `set -s set-clipboard on`.
- The Terminal window runs your `$SHELL` with its startup files. For a
  predictable prompt put `[terminal]`, `shell = "/bin/bash"` and
  `args = ["--norc", "--noprofile"]` in the scratch
  `$XDG_CONFIG_HOME/jetl/config.toml`. Check that quitting left no shell by
  writing `echo $$` to a file first and looking for that pid afterwards.
