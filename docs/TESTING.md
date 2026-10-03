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
`cfg(test)`), never your shell or its startup files. Language servers are
off in `test_app`; `app::lsp::tests::with_fake_servers` turns them on with
in-process servers (`lsp::fake::FakeServer`: answers `initialize` and
`shutdown` itself, hands the test every message it gets, and sends
notifications, requests and responses), so tests never start a real
server. `docs/KEYMAP.md` is checked by a test; regenerate
it with `cargo run -q -- --dump-keymap > docs/KEYMAP.md`.

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

### Limits under tmux

- No kitty keyboard protocol: Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter,
  Shift+Enter, Shift+F3, Alt+Shift+Tab and double Shift do not arrive. Use the legacy
  fallbacks (Ctrl+N for files, Alt+Shift+N for symbols, F1 for actions, Alt+Left/Right for tabs,
  Alt+Enter to commit) or check those keys in foot or kitty.
- Escape followed quickly by another key arrives as Alt+key; pause between
  them.
- Ctrl+Alt+Shift+J (select all occurrences) needs the kitty protocol; use
  Ctrl+Alt+J. Alt+Shift+J arrives as Alt+J with Shift and works.
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
