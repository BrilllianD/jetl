# jetl

Terminal IDE in Rust (ratatui + crossterm) with JetBrains-like UX: project tree, tabbed editor with tree-sitter highlighting, Git tool window, worktree panel. Design notes live in the plan under `~/.claude/plans/` and in `docs/ARCHITECTURE.md`.

## Build and run

    cargo run -- .                                   # open current dir
    cargo run --no-default-features --features lang-rust -- .   # faster iteration (fewer grammars)
    RUST_LOG=jetl=debug cargo run -- .               # log to ~/.local/state/jetl/jetl.log
    cargo test

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

## Conventions

- All buffer mutation goes through `Buffer::apply(Transaction)`; never touch the rope directly.
- Rendering and mouse mapping in the editor share one `layout_line()` walk.
- git2 `Repository` is opened fresh inside `spawn_blocking`; never stored in `App`.
- Network and worktree git operations go through the `git` CLI (`src/git/cli.rs`).
