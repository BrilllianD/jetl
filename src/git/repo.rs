//! Synchronous git2 operations. Each function opens the repository fresh and
//! is meant to run inside `spawn_blocking`; no `Repository` outlives a call.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use git2::{BranchType, IndexAddOption, ObjectType, Repository, Status, StatusOptions};

use super::model::{BranchInfo, ChangeEntry, FileStatus, Section, StatusSnapshot, tree_status};

/// Open the repository containing `root` and return it with its canonical
/// work tree.
pub fn open(root: &Path) -> Result<(Repository, PathBuf)> {
    let repo = Repository::discover(root).map_err(|e| anyhow!("not a git repository ({})", e.message()))?;
    let wd = repo.workdir().ok_or_else(|| anyhow!("bare repository has no work tree"))?;
    let wd = std::fs::canonicalize(wd).unwrap_or_else(|_| wd.to_path_buf());
    Ok((repo, wd))
}

/// Path of `abs` relative to the work tree, `/`-separated.
pub fn rel_path(workdir: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(workdir).ok()?;
    let s = rel.to_str()?;
    (!s.is_empty()).then(|| s.replace(std::path::MAIN_SEPARATOR, "/"))
}

pub fn status(root: &Path) -> Result<StatusSnapshot> {
    let (repo, workdir) = open(root)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false).exclude_submodules(false);
    let statuses = repo.statuses(Some(&mut opts)).context("git status")?;
    let mut entries = Vec::new();
    let mut files = Vec::new();
    for s in statuses.iter() {
        let rel = String::from_utf8_lossy(s.path_bytes()).into_owned();
        let abs = workdir.join(&rel);
        let st = s.status();
        let mut push = |section, status| entries.push(ChangeEntry { section, status, rel: rel.clone(), abs: abs.clone() });
        if st.is_conflicted() {
            push(Section::Unstaged, FileStatus::Conflicted);
            files.push((abs, FileStatus::Conflicted));
            continue;
        }
        let index = if st.contains(Status::INDEX_NEW) {
            Some(FileStatus::Added)
        } else if st.contains(Status::INDEX_DELETED) {
            Some(FileStatus::Deleted)
        } else if st.contains(Status::INDEX_RENAMED) {
            Some(FileStatus::Renamed)
        } else if st.intersects(Status::INDEX_MODIFIED | Status::INDEX_TYPECHANGE) {
            Some(FileStatus::Modified)
        } else {
            None
        };
        let wt = if st.contains(Status::WT_NEW) {
            Some(FileStatus::Untracked)
        } else if st.contains(Status::WT_DELETED) {
            Some(FileStatus::Deleted)
        } else if st.contains(Status::WT_RENAMED) {
            Some(FileStatus::Renamed)
        } else if st.intersects(Status::WT_MODIFIED | Status::WT_TYPECHANGE) {
            Some(FileStatus::Modified)
        } else {
            None
        };
        if let Some(i) = index {
            push(Section::Staged, i);
        }
        match wt {
            Some(FileStatus::Untracked) => push(Section::Untracked, FileStatus::Untracked),
            Some(w) => push(Section::Unstaged, w),
            None => {}
        }
        // Tree color: a staged new file stays "added" even when edited again.
        let tree = match (index, wt) {
            (Some(FileStatus::Added), _) => FileStatus::Added,
            (_, Some(FileStatus::Deleted)) | (Some(FileStatus::Deleted), None) => FileStatus::Deleted,
            (_, Some(w)) => w,
            (Some(i), None) => i,
            (None, None) => continue,
        };
        files.push((abs, tree));
    }
    entries.sort_by(|a, b| a.section.cmp(&b.section).then_with(|| a.rel.cmp(&b.rel)));
    let tree = tree_status(&workdir, files);
    let (branch, head) = head_info(&repo);
    Ok(StatusSnapshot { workdir, entries, tree, branch, head })
}

/// Current branch name (also for an unborn branch) and HEAD commit sha.
fn head_info(repo: &Repository) -> (Option<String>, Option<String>) {
    match repo.head() {
        Ok(h) => {
            let sha = h.target().map(|o| o.to_string());
            let branch = if h.is_branch() { h.shorthand().ok().map(str::to_string) } else { None };
            (branch, sha)
        }
        Err(_) => {
            // Unborn: HEAD is a symbolic ref to a branch that does not exist yet.
            let name = repo
                .find_reference("HEAD")
                .ok()
                .and_then(|r| r.symbolic_target().ok().flatten().map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_string()));
            (name, None)
        }
    }
}

/// Which stored version of a file to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobWhich {
    Head,
    Index,
}

fn read_blob(repo: &Repository, which: BlobWhich, rel: &str) -> Option<Vec<u8>> {
    match which {
        BlobWhich::Head => {
            let tree = repo.head().ok()?.peel_to_tree().ok()?;
            let entry = tree.get_path(Path::new(rel)).ok()?;
            if entry.kind() != Some(ObjectType::Blob) {
                return None;
            }
            let blob = repo.find_blob(entry.id()).ok()?;
            Some(blob.content().to_vec())
        }
        BlobWhich::Index => {
            let index = repo.index().ok()?;
            let entry = index.get_path(Path::new(rel), 0)?;
            let blob = repo.find_blob(entry.id).ok()?;
            Some(blob.content().to_vec())
        }
    }
}

/// A stored version of the file at `abs` as text. `None` when the file is
/// not in that version, is binary, or the root is not a repository.
pub fn blob_text(root: &Path, abs: &Path, which: BlobWhich) -> Option<String> {
    let (repo, wd) = open(root).ok()?;
    let rel = rel_path(&wd, abs)?;
    let bytes = read_blob(&repo, which, &rel)?;
    if super::diff::is_binary(&bytes) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Old and new contents for a working tree change.
pub fn change_texts(root: &Path, section: Section, rel: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let (repo, wd) = open(root)?;
    let workfile = || std::fs::read(wd.join(rel)).unwrap_or_default();
    Ok(match section {
        Section::Staged => {
            (read_blob(&repo, BlobWhich::Head, rel).unwrap_or_default(), read_blob(&repo, BlobWhich::Index, rel).unwrap_or_default())
        }
        Section::Unstaged => (read_blob(&repo, BlobWhich::Index, rel).unwrap_or_default(), workfile()),
        Section::Untracked => (Vec::new(), workfile()),
    })
}

/// Stage paths: add files that exist, record deletions for the rest.
pub fn stage(root: &Path, rels: &[String]) -> Result<()> {
    let (repo, wd) = open(root)?;
    let mut index = repo.index()?;
    for rel in rels {
        let p = Path::new(rel);
        if wd.join(rel).symlink_metadata().is_ok() {
            index.add_path(p).with_context(|| format!("stage {rel}"))?;
        } else {
            index.remove_path(p).with_context(|| format!("stage deletion of {rel}"))?;
        }
    }
    index.write()?;
    Ok(())
}

pub fn stage_all(root: &Path) -> Result<()> {
    let (repo, _) = open(root)?;
    let mut index = repo.index()?;
    index.add_all(["*"], IndexAddOption::DEFAULT, None)?;
    // add_all does not record deletions of tracked files.
    index.update_all(["*"], None)?;
    index.write()?;
    Ok(())
}

/// Unstage paths: reset them to HEAD, or drop them from the index when
/// there is no HEAD commit yet.
pub fn unstage(root: &Path, rels: &[String]) -> Result<()> {
    let (repo, _) = open(root)?;
    match repo.head().and_then(|h| h.peel_to_commit()) {
        Ok(commit) => repo.reset_default(Some(commit.as_object()), rels.iter().map(String::as_str))?,
        Err(_) => {
            let mut index = repo.index()?;
            for rel in rels {
                index.remove_path(Path::new(rel))?;
            }
            index.write()?;
        }
    }
    Ok(())
}

pub fn unstage_all(root: &Path) -> Result<()> {
    let snap = status(root)?;
    let staged: Vec<String> = snap.entries.iter().filter(|e| e.section == Section::Staged).map(|e| e.rel.clone()).collect();
    if staged.is_empty() {
        return Ok(());
    }
    unstage(root, &staged)
}

/// Commit the index on top of HEAD (or as the root commit of an unborn
/// branch). Returns the short sha.
pub fn commit(root: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        bail!("Commit message is empty");
    }
    let (repo, _) = open(root)?;
    let mut index = repo.index()?;
    if index.has_conflicts() {
        bail!("Cannot commit: resolve merge conflicts first");
    }
    let tree_id = index.write_tree()?;
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let unchanged = match &parent {
        Some(p) => p.tree_id() == tree_id,
        None => index.is_empty(),
    };
    if unchanged {
        bail!("Nothing to commit: no staged changes");
    }
    let sig = repo.signature().map_err(|e| {
        anyhow!("Cannot commit: git user.name and user.email are not configured ({}). Set them with `git config --global user.email you@example.com` and `git config --global user.name \"Your Name\"`.", e.message())
    })?;
    let tree = repo.find_tree(tree_id)?;
    let mut msg = message.trim_end().to_string();
    msg.push('\n');
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &parents)?;
    Ok(oid.to_string()[..8].to_string())
}

pub fn branches(root: &Path) -> Result<Vec<BranchInfo>> {
    let (repo, _) = open(root)?;
    let mut out = Vec::new();
    for b in repo.branches(Some(BranchType::Local))? {
        let (b, _) = b?;
        let Some(name) = b.name()?.map(str::to_string) else { continue };
        let current = b.is_head();
        let (mut upstream, mut ahead, mut behind) = (None, 0, 0);
        if let Ok(up) = b.upstream() {
            upstream = up.name().ok().flatten().map(str::to_string);
            if let (Some(l), Some(u)) = (b.get().target(), up.get().target())
                && let Ok((a, bh)) = repo.graph_ahead_behind(l, u)
            {
                ahead = a;
                behind = bh;
            }
        }
        out.push(BranchInfo { name, upstream, ahead, behind, current });
    }
    out.sort_by(|a, b| b.current.cmp(&a.current).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh repository under the system temp dir, removed on drop.
    pub(crate) struct TempRepo {
        pub dir: PathBuf,
    }

    impl TempRepo {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("jetl-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Repository::init(&dir).unwrap();
            // Write the repository's own config file only.
            let mut cfg = git2::Config::open(&dir.join(".git/config")).unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
            let dir = std::fs::canonicalize(&dir).unwrap();
            Self { dir }
        }
        pub fn write(&self, rel: &str, text: &str) {
            let p = self.dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sections(root: &Path) -> Vec<(Section, FileStatus, String)> {
        status(root).unwrap().entries.into_iter().map(|e| (e.section, e.status, e.rel)).collect()
    }

    #[test]
    fn stage_commit_unstage_round_trip() {
        let r = TempRepo::new("roundtrip");
        r.write("a.txt", "one\n");
        r.write("dir/b.txt", "two\n");
        let snap = status(&r.dir).unwrap();
        assert_eq!(snap.head, None);
        assert!(snap.branch.is_some(), "unborn branch still has a name");
        assert_eq!(
            sections(&r.dir),
            vec![
                (Section::Untracked, FileStatus::Untracked, "a.txt".into()),
                (Section::Untracked, FileStatus::Untracked, "dir/b.txt".into())
            ]
        );
        assert_eq!(snap.tree[&r.dir.join("dir")], FileStatus::Untracked);

        // Nothing staged yet on an unborn branch.
        assert!(commit(&r.dir, "init").unwrap_err().to_string().contains("Nothing to commit"));
        stage(&r.dir, &["a.txt".into()]).unwrap();
        assert_eq!(sections(&r.dir)[0], (Section::Staged, FileStatus::Added, "a.txt".into()));
        // Unstage on an unborn HEAD removes the entry from the index.
        unstage(&r.dir, &["a.txt".into()]).unwrap();
        assert_eq!(sections(&r.dir)[0].0, Section::Untracked);

        stage_all(&r.dir).unwrap();
        assert!(sections(&r.dir).iter().all(|(s, st, _)| *s == Section::Staged && *st == FileStatus::Added));
        assert!(commit(&r.dir, "  ").unwrap_err().to_string().contains("empty"));
        commit(&r.dir, "first").unwrap();
        let snap = status(&r.dir).unwrap();
        assert!(snap.entries.is_empty());
        assert!(snap.head.is_some());

        // Modify, delete and stage; then unstage against HEAD.
        r.write("a.txt", "one\nmore\n");
        std::fs::remove_file(r.dir.join("dir/b.txt")).unwrap();
        assert_eq!(
            sections(&r.dir),
            vec![(Section::Unstaged, FileStatus::Modified, "a.txt".into()), (Section::Unstaged, FileStatus::Deleted, "dir/b.txt".into())]
        );
        stage(&r.dir, &["a.txt".into(), "dir/b.txt".into()]).unwrap();
        assert_eq!(
            sections(&r.dir),
            vec![(Section::Staged, FileStatus::Modified, "a.txt".into()), (Section::Staged, FileStatus::Deleted, "dir/b.txt".into())]
        );
        let (old, new) = change_texts(&r.dir, Section::Staged, "a.txt").unwrap();
        assert_eq!((old.as_slice(), new.as_slice()), (b"one\n".as_slice(), b"one\nmore\n".as_slice()));
        unstage_all(&r.dir).unwrap();
        assert!(sections(&r.dir).iter().all(|(s, _, _)| *s == Section::Unstaged));
        assert_eq!(blob_text(&r.dir, &r.dir.join("a.txt"), BlobWhich::Head).as_deref(), Some("one\n"));

        let b = branches(&r.dir).unwrap();
        assert_eq!(b.len(), 1);
        assert!(b[0].current);
    }
}
