//! Filesystem watcher: one debounced, recursive watch on the project root
//! (plus the git directory of a linked worktree, which lives outside it).
//! Each batch becomes an `AppMsg::FsChanged` tagged with the workspace
//! generation; the app refreshes the tree, git state and open buffers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_full::notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RenameMode};
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;

/// How long a burst of events is gathered (also the config watcher's).
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(150);

/// Keeps the watch alive; dropping it stops watching.
pub struct Watcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Watcher")
    }
}

/// Changes of one batch, split by what they affect.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Files and directories of the working tree.
    pub paths: Vec<PathBuf>,
    /// HEAD, the index or refs changed.
    pub git: bool,
    /// Moves inside the root reported as one event (`old`, `new`).
    pub renames: Vec<(PathBuf, PathBuf)>,
    /// Per-path created/changed/deleted events under the root, for language
    /// servers' `workspace/didChangeWatchedFiles`.
    pub events: Vec<(PathBuf, FileChange)>,
}

/// What happened to a path, as language servers see it (LSP `FileChangeType`
/// 1, 2, 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChange {
    Created,
    Changed,
    Deleted,
}

/// Start watching `root`. Blocking: inotify adds a watch per directory, so
/// call it off the main thread.
pub fn start(root: &Path, generation: u64, tx: UnboundedSender<AppMsg>) -> anyhow::Result<Watcher> {
    let git_dirs = git_dirs(root);
    let (root_c, dirs_c) = (root.to_path_buf(), git_dirs.clone());
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
        Ok(events) => {
            let mut changes = classify(&root_c, &dirs_c, events.iter().filter(|e| changes_content(&e.kind)).flat_map(|e| e.paths.iter()));
            changes.renames =
                renames_of(&root_c, &dirs_c, events.iter().filter(|e| changes_content(&e.kind)).map(|e| (&e.kind, e.paths.as_slice())));
            changes.events = events_of(
                Some(&root_c),
                &dirs_c,
                events.iter().filter(|e| changes_content(&e.kind)).map(|e| (&e.kind, e.paths.as_slice())),
            );
            if !changes.paths.is_empty() || changes.git || !changes.renames.is_empty() {
                let _ = tx.send(AppMsg::FsChanged { generation, changes });
            }
        }
        Err(errors) => {
            for e in errors {
                tracing::warn!("watcher: {e}");
            }
        }
    })?;
    debouncer.watch(root, RecursiveMode::Recursive)?;
    // A linked worktree's HEAD and index live in the main repository.
    for d in git_dirs.iter().filter(|d| !d.starts_with(root)) {
        if let Err(e) = debouncer.watch(d, RecursiveMode::NonRecursive) {
            tracing::warn!("watcher: cannot watch {}: {e}", d.display());
        }
    }
    Ok(Watcher { _debouncer: debouncer })
}

/// Watch the folders language servers registered that lie outside the
/// project root. Blocking, like [`start`]. A folder that cannot be watched
/// is logged and skipped; `Err` when none could be.
pub fn start_outside(dirs: &[(PathBuf, bool)], generation: u64, tx: UnboundedSender<AppMsg>) -> anyhow::Result<Watcher> {
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
        Ok(events) => {
            let events = events_of(None, &[], events.iter().filter(|e| changes_content(&e.kind)).map(|e| (&e.kind, e.paths.as_slice())));
            if !events.is_empty() {
                let _ = tx.send(AppMsg::LspWatched { generation, events });
            }
        }
        Err(errors) => {
            for e in errors {
                tracing::warn!("outside watcher: {e}");
            }
        }
    })?;
    let mut watched = 0;
    let mut last_err = None;
    for (dir, recursive) in dirs {
        let mode = if *recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
        match debouncer.watch(dir, mode) {
            Ok(()) => watched += 1,
            Err(e) => {
                tracing::warn!("outside watcher: cannot watch {}: {e}", dir.display());
                last_err = Some(e);
            }
        }
    }
    match last_err {
        Some(e) if watched == 0 => Err(e.into()),
        _ => Ok(Watcher { _debouncer: debouncer }),
    }
}

/// The change one path of an event stands for; `i` is its index in the event.
fn change_of(kind: &EventKind, i: usize, path: &Path) -> FileChange {
    match kind {
        EventKind::Create(_) => FileChange::Created,
        EventKind::Remove(_) => FileChange::Deleted,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if i == 0 {
                FileChange::Deleted
            } else {
                FileChange::Created
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => FileChange::Deleted,
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => FileChange::Created,
        EventKind::Modify(ModifyKind::Name(_)) => {
            if path.symlink_metadata().is_ok() {
                FileChange::Created
            } else {
                FileChange::Deleted
            }
        }
        _ => FileChange::Changed,
    }
}

/// Per-path events of one batch, merged in arrival order. Under `Some(root)`
/// only paths inside it and outside `git_dirs` count; with `None` (the
/// outside watcher) paths with a `.git` component are dropped.
fn events_of<'a>(
    root: Option<&Path>,
    git_dirs: &[PathBuf],
    events: impl Iterator<Item = (&'a EventKind, &'a [PathBuf])>,
) -> Vec<(PathBuf, FileChange)> {
    use FileChange::{Changed, Created, Deleted};
    let mut out: Vec<(PathBuf, FileChange)> = Vec::new();
    for (kind, paths) in events {
        for (i, p) in paths.iter().enumerate() {
            let keep = match root {
                Some(r) => p.starts_with(r) && !git_dirs.iter().any(|d| p.starts_with(d)),
                None => !p.components().any(|c| c.as_os_str() == ".git"),
            };
            if !keep {
                continue;
            }
            let new = change_of(kind, i, p);
            match out.iter().position(|(q, _)| q == p) {
                None => out.push((p.clone(), new)),
                Some(at) => {
                    let merged = match (out[at].1, new) {
                        (Created, Changed) => Some(Created),
                        (Created, Deleted) => None,
                        (Deleted, Created) | (Deleted, Changed) => Some(Changed),
                        (Changed, Created) => Some(Changed),
                        (_, k) => Some(k),
                    };
                    match merged {
                        Some(k) => out[at].1 = k,
                        None => {
                            out.remove(at);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Whether an event can change what a file holds. inotify also reports
/// opens and reads, which jetl itself causes (git status, find in files,
/// the symbol index); reacting to them would loop.
pub(crate) fn changes_content(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(a) if *a != AccessKind::Close(AccessMode::Write))
}

/// Renames reported with both paths (`RenameMode::Both`), both inside
/// `root` and outside the git directories.
fn renames_of<'a>(
    root: &Path,
    git_dirs: &[PathBuf],
    events: impl Iterator<Item = (&'a EventKind, &'a [PathBuf])>,
) -> Vec<(PathBuf, PathBuf)> {
    let inside = |p: &PathBuf| p.starts_with(root) && !git_dirs.iter().any(|d| p.starts_with(d));
    events
        .filter(|(kind, _)| matches!(kind, EventKind::Modify(ModifyKind::Name(RenameMode::Both))))
        .filter_map(|(_, paths)| match paths {
            [from, to] if inside(from) && inside(to) => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect()
}

/// Git directories whose changes mean a git refresh: `root/.git`, or the
/// directory a `.git` file points to (linked worktree).
fn git_dirs(root: &Path) -> Vec<PathBuf> {
    let dot = root.join(".git");
    let mut dirs = vec![dot.clone()];
    if let Ok(text) = std::fs::read_to_string(&dot)
        && let Some(target) = text.trim().strip_prefix("gitdir:")
    {
        let target = root.join(target.trim());
        dirs.push(std::fs::canonicalize(&target).unwrap_or(target));
    }
    dirs
}

/// Split changed paths into working tree paths (deduplicated) and a git
/// flag. Inside a git directory only HEAD, the index, refs and the
/// worktree list count; objects, logs and lock files do not.
fn classify<'a>(root: &Path, git_dirs: &[PathBuf], paths: impl Iterator<Item = &'a PathBuf>) -> Changes {
    let mut out = Changes::default();
    for p in paths {
        if let Some(rel) = git_dirs.iter().find_map(|d| p.strip_prefix(d).ok()) {
            let s = rel.to_string_lossy();
            let interesting = matches!(s.as_ref(), "HEAD" | "index" | "MERGE_HEAD" | "packed-refs" | "FETCH_HEAD")
                || s.starts_with("refs/")
                || s.starts_with("worktrees/");
            // yman's read-only commands rewrite its worktree index; counting
            // that would make every tracker load trigger the next. Its
            // mutations also move `refs/yman/local`, which does count.
            let yman_index = s == "worktrees/-yman/index";
            out.git |= interesting && !yman_index && !s.ends_with(".lock");
        } else if p.starts_with(root) && !out.paths.contains(p) {
            out.paths.push(p.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_metadata_is_filtered_and_paths_deduplicated() {
        let root = Path::new("/p");
        let dirs = [root.join(".git"), PathBuf::from("/main/.git/worktrees/p")];
        let c = |v: &[&str]| {
            let paths: Vec<PathBuf> = v.iter().map(PathBuf::from).collect();
            classify(root, &dirs, paths.iter())
        };
        assert_eq!(c(&["/p/.git/objects/ab/cdef", "/p/.git/index.lock", "/p/.git/logs/HEAD"]), Changes::default());
        assert!(c(&["/p/.git/index"]).git);
        assert!(c(&["/p/.git/refs/heads/main"]).git);
        assert!(c(&["/main/.git/worktrees/p/HEAD"]).git, "linked worktree git dir");
        assert_eq!(c(&["/p/.git/worktrees/-yman/index", "/p/.git/worktrees/-yman/index.lock"]), Changes::default(), "yman reads");
        assert!(c(&["/p/.git/refs/yman/local"]).git, "yman writes");
        assert!(c(&["/p/.git/worktrees/-yman/HEAD"]).git);
        let both = c(&["/p/src/a.rs", "/p/src/a.rs", "/p/.git/HEAD", "/elsewhere/x"]);
        assert_eq!(both, Changes { paths: vec![PathBuf::from("/p/src/a.rs")], git: true, ..Default::default() });
    }

    #[test]
    fn renames_need_both_paths_inside_the_root() {
        let root = Path::new("/p");
        let dirs = [root.join(".git")];
        let both = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        let from = EventKind::Modify(ModifyKind::Name(RenameMode::From));
        let v = |a: &[&str]| a.iter().map(PathBuf::from).collect::<Vec<_>>();
        let run = |kind: &EventKind, paths: &[PathBuf]| renames_of(root, &dirs, std::iter::once((kind, paths)));
        assert_eq!(run(&both, &v(&["/p/a.txt", "/p/b.txt"])), vec![(PathBuf::from("/p/a.txt"), PathBuf::from("/p/b.txt"))]);
        assert!(run(&from, &v(&["/p/a.txt"])).is_empty());
        assert!(run(&from, &v(&["/p/a.txt", "/p/b.txt"])).is_empty());
        assert!(run(&both, &v(&["/p/.git/x", "/p/b.txt"])).is_empty());
        assert!(run(&both, &v(&["/p/a.txt", "/elsewhere/b.txt"])).is_empty());
        assert!(run(&both, &v(&["/p/a.txt"])).is_empty());
    }

    #[test]
    fn event_kinds_are_classified() {
        use notify_debouncer_full::notify::event::{CreateKind, DataChange, RemoveKind};
        let p = Path::new("/nonexistent-jetl/x");
        let name = |m| EventKind::Modify(ModifyKind::Name(m));
        assert_eq!(change_of(&EventKind::Create(CreateKind::File), 0, p), FileChange::Created);
        assert_eq!(change_of(&EventKind::Remove(RemoveKind::File), 0, p), FileChange::Deleted);
        assert_eq!(change_of(&name(RenameMode::Both), 0, p), FileChange::Deleted);
        assert_eq!(change_of(&name(RenameMode::Both), 1, p), FileChange::Created);
        assert_eq!(change_of(&name(RenameMode::From), 0, p), FileChange::Deleted);
        assert_eq!(change_of(&name(RenameMode::To), 0, p), FileChange::Created);
        assert_eq!(change_of(&name(RenameMode::Any), 0, p), FileChange::Deleted, "missing path");
        assert_eq!(change_of(&name(RenameMode::Any), 0, Path::new("/")), FileChange::Created, "existing path");
        assert_eq!(change_of(&EventKind::Modify(ModifyKind::Data(DataChange::Any)), 0, p), FileChange::Changed);
        assert_eq!(change_of(&EventKind::Modify(ModifyKind::Any), 0, p), FileChange::Changed);
        assert_eq!(change_of(&EventKind::Access(AccessKind::Close(AccessMode::Write)), 0, p), FileChange::Changed);
        assert_eq!(change_of(&EventKind::Any, 0, p), FileChange::Changed);
        assert_eq!(change_of(&EventKind::Other, 0, p), FileChange::Changed);
    }

    #[test]
    fn events_merge_per_path() {
        use FileChange::{Changed, Created, Deleted};
        use notify_debouncer_full::notify::event::{CreateKind, RemoveKind};
        let create = EventKind::Create(CreateKind::File);
        let remove = EventKind::Remove(RemoveKind::File);
        let modify = EventKind::Modify(ModifyKind::Any);
        let root = Path::new("/p");
        let dirs = [root.join(".git")];
        let a = vec![PathBuf::from("/p/a")];
        let run = |root: Option<&Path>, seq: &[(&EventKind, &[PathBuf])]| events_of(root, &dirs, seq.iter().copied());
        let one = |first: &EventKind, second: &EventKind| run(Some(root), &[(first, &a), (second, &a)]);
        let at = |k| vec![(PathBuf::from("/p/a"), k)];
        assert_eq!(run(Some(root), &[(&modify, &a)]), at(Changed));
        assert_eq!(one(&create, &modify), at(Created));
        assert_eq!(one(&create, &remove), vec![]);
        assert_eq!(one(&remove, &create), at(Changed));
        assert_eq!(one(&remove, &modify), at(Changed));
        assert_eq!(one(&modify, &remove), at(Deleted));
        assert_eq!(one(&modify, &create), at(Changed));
        assert_eq!(one(&create, &create), at(Created));
        assert_eq!(one(&remove, &remove), at(Deleted));
        // First-seen order of paths, filtering.
        let b = vec![PathBuf::from("/p/b")];
        let junk = vec![PathBuf::from("/p/.git/index"), PathBuf::from("/elsewhere/x")];
        let got = run(Some(root), &[(&modify, &b), (&modify, &junk), (&create, &a), (&modify, &b)]);
        assert_eq!(got, vec![(PathBuf::from("/p/b"), Changed), (PathBuf::from("/p/a"), Created)]);
        // Outside watcher: no root filter, .git paths dropped.
        let mixed = vec![PathBuf::from("/q/dep/.git/HEAD"), PathBuf::from("/q/dep/lib.rs")];
        assert_eq!(run(None, &[(&modify, &mixed)]), vec![(PathBuf::from("/q/dep/lib.rs"), Changed)]);
    }

    #[test]
    fn opens_and_reads_are_not_changes() {
        use notify_debouncer_full::notify::event::{CreateKind, ModifyKind};
        assert!(!changes_content(&EventKind::Access(AccessKind::Open(AccessMode::Any))));
        assert!(!changes_content(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
        assert!(changes_content(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
        assert!(changes_content(&EventKind::Modify(ModifyKind::Any)));
        assert!(changes_content(&EventKind::Create(CreateKind::File)));
    }

    #[tokio::test]
    async fn reports_a_write_and_a_git_change() {
        let dir = std::env::temp_dir().join(format!("jetl-test-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _w = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || start(&dir, 7, tx)
        })
        .await
        .unwrap()
        .unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join(".git/refs/heads/main"), "0000").unwrap();
        let (mut paths, mut git) = (Vec::new(), false);
        while !(git && paths.contains(&dir.join("a.txt"))) {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(AppMsg::FsChanged { generation: 7, changes })) => {
                    paths.extend(changes.paths);
                    git |= changes.git;
                }
                other => panic!("expected FsChanged, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // FSEvents never reports `RenameMode::Both`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reports_a_rename_as_a_pair() {
        let dir = std::env::temp_dir().join(format!("jetl-test-watch-mv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _w = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || start(&dir, 7, tx)
        })
        .await
        .unwrap()
        .unwrap();
        std::fs::rename(dir.join("a.txt"), dir.join("b.txt")).unwrap();
        let want = (dir.join("a.txt"), dir.join("b.txt"));
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(AppMsg::FsChanged { changes, .. })) if changes.renames.contains(&want) => break,
                Ok(Some(_)) => {}
                other => panic!("expected the rename pair, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linked_worktree_git_dir_is_found() {
        let dir = std::env::temp_dir().join(format!("jetl-test-gitdirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("main/.git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(dir.join("wt")).unwrap();
        std::fs::write(dir.join("wt/.git"), format!("gitdir: {}\n", dir.join("main/.git/worktrees/wt").display())).unwrap();
        let dirs = git_dirs(&dir.join("wt"));
        assert_eq!(dirs[1], std::fs::canonicalize(dir.join("main/.git/worktrees/wt")).unwrap());
        assert_eq!(git_dirs(&dir.join("main")).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
