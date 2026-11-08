# jetl

Terminal IDE in Rust (ratatui + crossterm) with JetBrains-like UX: project tree, tabbed editor with tree-sitter highlighting, Git tool window, worktree panel. Feature backlog and phase order: `docs/ROADMAP.md`. Architecture (module map, event flow, stale-result rules, extension points): `docs/ARCHITECTURE.md`. Manual tmux test harness: `docs/TESTING.md`.

Knowledge base: ~/mem/projects/jetl/README.md (search: qmd search "<terms>" -c mem -n 8). Read it before non-trivial work; record decisions there.

## Build and run

    cargo run -- .                                   # open current dir
    cargo run --no-default-features --features lang-rust -- .   # faster iteration (fewer grammars)
    RUST_LOG=jetl=debug cargo run -- .               # log to ~/.local/state/jetl/jetl.log
    cargo test
    cargo clippy --all-targets -- -D warnings && cargo fmt --check   # CI runs both

## Task tracking (yman)

Tasks live in `.yman/` and are managed with the `yman` CLI. Every mutation is its own commit inside `.yman`; nothing to save.

    yman ls -n 10 --assignee -                        # pick: unassigned, best priority first
    yman show <id> -n 3                               # read: header, body, last 3 comments
    yman set <id> --status doing -a claude -m "on it" # claim: status, assignee, note, one commit
    yman done <id> -m "what changed"                  # close and explain in one call
    yman add "Title" -m "body" -a claude --relate <id>  # follow-up, linked to its parent

Never `edit` or `-e` (needs a terminal); change a body with `yman set <id> --body "..."`.
Set `YMAN_ACTOR=claude` so comments are attributed. Phase tasks are ids 1-6; subtasks relate to them.
Start the subtask before working on it, `done` it after its verification steps pass.
Current work is the Stage E epic: `yman plan 452 -n 3` picks the next step (priority is the
order); the working mode is Stage D's, in `yman show 107 -n 0` (107 is closed). Design docs are `yman attach`ed to their task.
Implementer subagents run one at a time across the whole run, in the main checkout: no parallel
worktree slots, even for slices with `Depends on: none` (overrides the global slot rule). Designers
and reviewers may overlap a running implementer.

## Conventions

- All buffer mutation goes through `Buffer::apply(Transaction)`; never touch the rope directly.
- Rendering and mouse mapping in the editor share one `layout_line()` walk.
- git2 `Repository` is opened fresh inside `spawn_blocking`; never stored in `App`.
- Network and worktree git operations go through the `git` CLI (`src/git/cli.rs`).
- A new theme key goes into `theme_fields!` and every `assets/themes/*.toml` (`builtin_themes_set_every_key` fails otherwise). Never hard-code `Color::Rgb`; take colours from `Theme` so the 256-colour fallback applies.
- A new config key needs a reload class in `src/config/reload.rs` (compile error otherwise); a Live key is applied in `App::apply_config` or `Editor::apply_config`.
- Tests that write `config.toml` set `[lsp] enabled = false`.
- Crash-recovered text goes through `Buffer::apply` and sets `Buffer::recovered`, so autosave skips it.
- MSRV is 1.90: run `CARGO_INCREMENTAL=0 cargo +1.90 test` before committing; avoid std APIs and syntax stabilised later.
