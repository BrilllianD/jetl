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
split panes, carets, occurrences, folding, soft wrap, bookmarks, the
numbered bookmark chooser and the bookmarks list with closed files, the
editor's find/replace bar, the project tree and its context menu, the Git
window's Commit tab (also at 80 columns, its diff header cut before the
toggles), its Log, Stash and Console tabs, the Find and Problems windows
(diagnostic underlines in the editor too), the lightbulb in the gutter, the main menu bar, confirm,
input and choice dialogs, the hover and completion popups, toasts, the
palettes (Scratch Files and Recent Projects included), the conflict resolver (filler rows included), the crash
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
or different snapshot fails the test. Tests that need a grammar (`.rs`
folding) are behind `#[cfg(feature = "lang-rust")]`; the rest use plain
file types, so every feature set gives the same frames.

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

### Scenario: column selection

At 80x24 and at 120x40, with a file of tabs, wide characters and a short
line (`printf '\tab日本語x\n\tab日本語x\nab\n\tab日本語x\n' > $T/docs/notes.md`,
opened as `jetl $T/docs/notes.md`, the Project window hidden with Alt+1).
`X` is the 1-based screen column of the text's first cell (`X=10` past the
tool strip and the gutter; check with `display -p '#{cursor_x}'` plus one
while the caret is at a line start); the first line is on row 2:

```sh
press 8 $((X+5)) 2; press 40 $((X+6)) 4; press 40 $((X+7)) 5; release 8 $((X+7)) 5
/tmp/tm.sh capture-pane -t jetl -p | tail -1     # Ln 4, Col 9 (4 carets, 6 chars)
/tmp/tm.sh send-keys -t jetl -l 'Q'              # b日 becomes Q on the long lines, Q after "ab"
/tmp/tm.sh send-keys -t jetl C-z                 # the block again
press 12 $((X+30)) 3; press 44 $((X+12)) 2; release 12 $((X+12)) 2   # Alt+Shift, right to left: x, x
/tmp/tm.sh send-keys -t jetl Escape; sleep 0.6; /tmp/tm.sh send-keys -t jetl Escape
/tmp/tm.sh send-keys -t jetl C-Home Right        # after the tab of line 1
/tmp/tm.sh send-keys -t jetl M-S-IC              # Alt+Shift+Insert: status ends in " Column"
for i in 1 2 3; do /tmp/tm.sh send-keys -t jetl S-Down; done   # Ln 4, Col 5 (4 carets) Column
/tmp/tm.sh send-keys -t jetl -l '#'              # a # on all four lines, at the end of "ab"
/tmp/tm.sh send-keys -t jetl C-z
```

`capture-pane -e` shows the selected cells in the selection background
(the whole 日 when the edge cuts it) and the caret on the short line as
reverse video after "ab". Alt+click without moving (`press 8`, `release
8` on one cell) still adds a caret.

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
/tmp/tm.sh send-keys -t jetl F2                               # "jetl 0.2.0"
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

Then open File > Settings…, change `scrolloff`, press Ctrl+S: "Reloaded
config.toml". Run `touch $C/config.toml`: nothing.

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

### Limits under tmux

- No kitty keyboard protocol: Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter,
  Shift+Enter, Shift+F3, Alt+Shift+Tab, double Shift and Ctrl+digit do not arrive. Use the legacy
  fallbacks (Ctrl+N for files, Alt+Shift+N for symbols, F1 for actions, Alt+Left/Right for tabs,
  Alt+Enter to commit, Alt+B then a digit for numbered bookmarks) or check those keys in foot or
  kitty.
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
