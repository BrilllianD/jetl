//! Filesystem watcher: one debounced, recursive watch on the project root
//! (plus the git directory of a linked worktree, which lives outside it).
//! Each batch becomes an `AppMsg::FsChanged` tagged with the workspace
//! generation; the app refreshes the tree, git state and open buffers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;

const DEBOUNCE: Duration = Duration::from_millis(150);

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
}

/// Start watching `root`. Blocking: inotify adds a watch per directory, so
/// call it off the main thread.
pub fn start(root: &Path, generation: u64, tx: UnboundedSender<AppMsg>) -> anyhow::Result<Watcher> {
    let git_dirs = git_dirs(root);
    let (root_c, dirs_c) = (root.to_path_buf(), git_dirs.clone());
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
        Ok(events) => {
            let changes = classify(&root_c, &dirs_c, events.iter().flat_map(|e| e.paths.iter()));
            if !changes.paths.is_empty() || changes.git {
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
            out.git |= interesting && !s.ends_with(".lock");
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
        let both = c(&["/p/src/a.rs", "/p/src/a.rs", "/p/.git/HEAD", "/elsewhere/x"]);
        assert_eq!(both, Changes { paths: vec![PathBuf::from("/p/src/a.rs")], git: true });
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
