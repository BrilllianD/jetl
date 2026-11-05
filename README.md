# jetl

jetl is a terminal IDE written in Rust with ratatui and crossterm. It brings the JetBrains way of
working to the terminal: a Project tree on the left, a tabbed editor with tree-sitter syntax
highlighting, a Git tool window with diff, log and stash, a Worktrees panel, find in files, a
built-in terminal and a Run window, all driven by JetBrains-style key bindings and the mouse.

## Features

**Editor**

- Tabs with pinning, reordering and an optional tab limit; undo grouping; smart Home, word
  motions, duplicate, delete, move and join lines, toggle case.
- Syntax highlighting with incremental tree-sitter reparsing for Rust, JavaScript, TypeScript,
  TSX, Python, JSON, TOML, Bash, Markdown, C#, YAML, HTML, CSS, SCSS, XML, Go, C, C++, Java, PHP,
  Lua, Dockerfile, Make, INI (also `.conf`, `.cfg` and systemd units), CMake, diff and patch files,
  Java properties, pip requirements, nginx configs, Git commit messages, SQL, Ruby, Kotlin, Swift, Zig, HCL and Terraform, Nix, Protocol Buffers
  and GraphQL. Language detection by extension, file name, shebang and modeline.
- Bracket matching and jump, auto-closed brackets and quotes, language-aware indent on Enter,
  indent and unindent of a selection, line and block comment toggle, expand and shrink selection
  by syntax node, highlighting of the word under the caret.
- Per-buffer indentation from `.editorconfig` or detection; CRLF detection; atomic save with
  optional trailing whitespace trimming and final newline; autosave on focus loss or idle.
- Format on save (or on demand) through an external formatter configured per language.
- Back/forward caret navigation and bookmarks. Open files reload, or ask, when they change on
  disk.
- Find and replace in the file, go to line, paste from the system clipboard (`wl-paste`, `xclip`
  or `xsel`) and copy over OSC 52.

**Project and navigation**

- Project tree with new, rename, delete and copy path, a right-click context menu, speed search,
  and toggles for hidden and ignored files.
- A filesystem watcher refreshes the tree, the Git window and open files after outside changes.
- Palette with Files, Symbols, Actions, Search Everywhere, Recent Files, Recent Locations, Branches and Recent
  Projects modes.
- Symbol search and a Structure tool window based on tree-sitter tags.
- Find and replace in files in a bottom tool window.
- `jetl path/to/file.rs:12:5` opens a file at a position, rooted at its git repository.
- Sessions per project root: open and pinned tabs, carets, bookmarks, tool window layout and
  expanded tree folders.

**Git**

- Stage, unstage and discard whole files or single changes; renames shown as `R old -> new`.
- Commit with amend and per-repository message history; push, pull (fast-forward, rebase or
  merge).
- Diff view with changed-character marks, syntax highlighting, folding of unchanged lines, and
  side-by-side or unified layout.
- Gutter change marks against HEAD, revert of the change at the caret, next/previous change, and
  a blame column.
- Log with its own graph, filters by branch, text, author and path, file history, and commit
  actions: checkout, new branch, cherry-pick, revert and tags.
- Stash tab; new branches; a branches popup to check out, merge, rebase, rename and delete
  branches.
- Worktrees panel to create, switch, remove and prune git worktrees.

**Terminal and Run**

- Terminal tool window: a PTY shell with vt100 emulation and scrollback.
- Run tool window: tasks from the config file or detected from project files, with problem
  matchers whose results jump to the source on click.

**UI and configuration**

- Follows the Omarchy desktop theme live; otherwise GitHub Dark, Darcula and light themes (or your own theme file) with color overrides, rebindable keymap, and a TOML config file.
- Zen mode, keyboard resize of tool windows, an Event Log tool window, toasts and dialogs.
- Mouse support for clicks, selection and wheel scrolling.

## Install

### Release tarball

Download `jetl-0.8.0-x86_64-unknown-linux-gnu.tar.gz` (and its `.sha256`), extract it, and put
the `jetl` binary on your `PATH`, for example in `~/.local/bin`:

    sha256sum -c jetl-0.8.0-x86_64-unknown-linux-gnu.tar.gz.sha256
    tar xzf jetl-0.8.0-x86_64-unknown-linux-gnu.tar.gz
    install -Dm755 jetl-0.8.0-x86_64-unknown-linux-gnu/jetl ~/.local/bin/jetl

Run the commands in the directory that holds both downloaded files. The archive unpacks to
`jetl-0.8.0-x86_64-unknown-linux-gnu/`, which holds `jetl`, `README.md`, `LICENSE`,
`CHANGELOG.md` and `docs/` (`CONFIG.md`, `KEYMAP.md`).

### Arch Linux

    sudo pacman -U jetl-bin-0.8.0-1-x86_64.pkg.tar.zst

### From source

    cargo install --locked --path .

### Building the packages yourself

    scripts/package.sh          # dist/jetl-<version>-x86_64-unknown-linux-gnu.tar.gz and .sha256
    scripts/package.sh --arch   # also dist/jetl-bin-<version>-1-x86_64.pkg.tar.zst

`--arch` builds the Arch package with `makepkg` from `packaging/arch/PKGBUILD`.

## Building

Requirements:

- Rust 1.90 or newer.
- A C compiler. The tree-sitter grammars and the bundled libgit2 are compiled from C sources.
- At run time, `git` on `PATH` (push, pull, merge, rebase and worktree operations use the git
  CLI).

Commands:

    cargo build --release
    cargo test

Each grammar is a cargo feature, and all of them are on by default:

`lang-rust`, `lang-js`, `lang-ts`, `lang-python`, `lang-json`, `lang-toml`, `lang-bash`,
`lang-md`, `lang-csharp`, `lang-yaml`, `lang-html`, `lang-css`, `lang-scss`, `lang-xml`,
`lang-go`, `lang-c`, `lang-cpp` (pulls in `lang-c`), `lang-java`, `lang-php`, `lang-lua`,
`lang-dockerfile`, `lang-make`, `lang-ini`, `lang-cmake`,
`lang-diff`, `lang-properties`, `lang-requirements`, `lang-nginx`, `lang-gitcommit`,
`lang-sql`, `lang-ruby`, `lang-kotlin`, `lang-swift`, `lang-zig`, `lang-hcl`, `lang-nix`, `lang-proto`, `lang-graphql`.

To build faster or smaller with only the grammars you need, turn off the defaults:

    cargo build --release --no-default-features --features lang-rust,lang-toml,lang-md

Files in a language whose grammar is not compiled in open as plain text.

`structural` (on by default) adds structural search to Find in files through `ast-grep-core`;
`--no-default-features` turns it off with the grammars, so list it again when you want it.

## Usage

```
Usage: jetl [PATH[:LINE[:COL]]]

Opens a terminal IDE rooted at PATH (default: current directory). A file
opens at LINE and COL, rooted at its git repository (else its directory).
Started without PATH outside a git repository, it offers the recent projects.

Options:
  -h, --help         Show this help
  -V, --version      Show the version
      --dump-keymap  Print the default key bindings as Markdown (docs/KEYMAP.md)
      --doctor       Check the optional tools jetl uses (language servers, debug adapters, formatters, clipboard, grammars, terminal)

Config: $XDG_CONFIG_HOME/jetl/{config,keymap,theme}.toml (see docs/CONFIG.md)
Log:    $XDG_STATE_HOME/jetl/jetl.log
```

A few keys to start with: `F1` finds an action, `Ctrl+N` goes to a file, `Alt+1` opens the
Project tree, `Alt+9` the Git window, `Alt+F12` the Terminal, and `Ctrl+Q` quits. Some bindings
need the kitty keyboard protocol (foot, kitty, WezTerm, Ghostty); other terminals and tmux get
fallbacks. The full list is in [docs/KEYMAP.md](docs/KEYMAP.md).

## Configuration

jetl reads three optional files from `$XDG_CONFIG_HOME/jetl/` (usually `~/.config/jetl/`):

- `config.toml`: editor, UI, git, worktree, files, clipboard, format, run and terminal settings.
- `keymap.toml`: key bindings merged on top of the built-in keymap.
- `theme.toml`: color overrides on top of the theme chosen by `ui.theme`.

Changes to these files apply while jetl runs.

The log, sessions and the recent projects list live in `$XDG_STATE_HOME/jetl/` (usually
`~/.local/state/jetl/`). Set `RUST_LOG=jetl=debug` for verbose logging.

See [docs/CONFIG.md](docs/CONFIG.md) for every setting and [docs/KEYMAP.md](docs/KEYMAP.md) for
the default key bindings.

## Documentation

- [docs/CONFIG.md](docs/CONFIG.md): configuration files and settings.
- [docs/KEYMAP.md](docs/KEYMAP.md): default key bindings.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): module map, event flow and extension points.
- [docs/ROADMAP.md](docs/ROADMAP.md): feature backlog and phase order.
- [docs/TESTING.md](docs/TESTING.md): automated tests and the manual tmux test harness.

## License

MIT. See [LICENSE](LICENSE).
