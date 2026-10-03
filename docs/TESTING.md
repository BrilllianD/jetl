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
`cfg(test)`), never your shell or its startup files. `docs/KEYMAP.md` is checked by a test; regenerate
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
/tmp/tm.sh send-keys -t jetl -l 'text'               # literal typing
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

### Limits under tmux

- No kitty keyboard protocol: Ctrl+Shift+letter, Ctrl+Tab, Ctrl+Enter,
  Shift+Enter, Shift+F3 and double Shift do not arrive. Use the legacy
  fallbacks (Ctrl+N for files, Alt+Shift+N for symbols, F1 for actions, Alt+Left/Right for tabs,
  Alt+Enter to commit) or check those keys in foot or kitty.
- Escape followed quickly by another key arrives as Alt+key; pause between
  them.
- Pressing Alt+number for a focused tool window hides it; later keys go to
  the editor.
- OSC 52 copy needs `set -s set-clipboard on`.
- The Terminal window runs your `$SHELL` with its startup files. For a
  predictable prompt put `[terminal]`, `shell = "/bin/bash"` and
  `args = ["--norc", "--noprofile"]` in the scratch
  `$XDG_CONFIG_HOME/jetl/config.toml`. Check that quitting left no shell by
  writing `echo $$` to a file first and looking for that pid afterwards.
