//! Synchronous git2 operations. Each function opens the repository fresh and
//! is meant to run inside `spawn_blocking`; no `Repository` outlives a call.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use git2::{BranchType, IndexAddOption, ObjectType, Repository, Status, StatusOptions};

use super::model::{Blame, BlameCommit, BranchInfo, ChangeEntry, FileStatus, Section, StashEntry, StatusSnapshot, tree_status};

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
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(false)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    let statuses = repo.statuses(Some(&mut opts)).context("git status")?;
    let mut entries = Vec::new();
    let mut files = Vec::new();
    for s in statuses.iter() {
        // The entry path is the old one of a rename; each side's new path
        // comes from its delta.
        let rel = String::from_utf8_lossy(s.path_bytes()).into_owned();
        let st = s.status();
        let side = |delta: Option<git2::DiffDelta>, renamed: bool| {
            let path = |f: git2::DiffFile| f.path_bytes().map(|p| String::from_utf8_lossy(p).into_owned());
            match delta {
                Some(d) => (path(d.new_file()).unwrap_or_else(|| rel.clone()), if renamed { path(d.old_file()) } else { None }),
                None => (rel.clone(), None),
            }
        };
        let index_side = side(s.head_to_index(), st.contains(Status::INDEX_RENAMED));
        let wt_side = side(s.index_to_workdir(), st.contains(Status::WT_RENAMED));
        let mut push = |section, status, (rel, old_rel): (String, Option<String>)| {
            entries.push(ChangeEntry { section, status, abs: workdir.join(&rel), rel, old_rel })
        };
        let abs = workdir.join(&wt_side.0);
        if st.is_conflicted() {
            push(Section::Unstaged, FileStatus::Conflicted, (rel.clone(), None));
            files.push((workdir.join(&rel), FileStatus::Conflicted));
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
            push(Section::Staged, i, index_side.clone());
        }
        match wt {
            Some(FileStatus::Untracked) => push(Section::Untracked, FileStatus::Untracked, wt_side),
            Some(w) => push(Section::Unstaged, w, wt_side),
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

/// Old and new contents for a working tree change; `old` is the path
/// before a rename, read on the old side.
pub fn change_texts(root: &Path, section: Section, rel: &str, old: Option<&str>) -> Result<(Vec<u8>, Vec<u8>)> {
    let (repo, wd) = open(root)?;
    let workfile = || std::fs::read(wd.join(rel)).unwrap_or_default();
    let old_rel = old.unwrap_or(rel);
    Ok(match section {
        Section::Staged => {
            (read_blob(&repo, BlobWhich::Head, old_rel).unwrap_or_default(), read_blob(&repo, BlobWhich::Index, rel).unwrap_or_default())
        }
        Section::Unstaged => (read_blob(&repo, BlobWhich::Index, old_rel).unwrap_or_default(), workfile()),
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
    let sig = signature(&repo)?;
    let tree = repo.find_tree(tree_id)?;
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, &commit_message(message), &tree, &parents)?;
    Ok(oid.to_string()[..8].to_string())
}

fn signature(repo: &Repository) -> Result<git2::Signature<'static>> {
    repo.signature().map_err(|e| {
        anyhow!("Cannot commit: git user.name and user.email are not configured ({}). Set them with `git config --global user.email you@example.com` and `git config --global user.name \"Your Name\"`.", e.message())
    })
}

fn commit_message(message: &str) -> String {
    let mut msg = message.trim_end().to_string();
    msg.push('\n');
    msg
}

/// Replace the HEAD commit with one of the index and `message`, keeping
/// its parents and author. Works without staged changes (a new message
/// only). Returns the short sha.
pub fn amend(root: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        bail!("Commit message is empty");
    }
    let (repo, _) = open(root)?;
    let mut index = repo.index()?;
    if index.has_conflicts() {
        bail!("Cannot amend: resolve merge conflicts first");
    }
    let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).ok_or_else(|| anyhow!("Nothing to amend: no commit yet"))?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let sig = signature(&repo)?;
    let oid = head.amend(Some("HEAD"), None, Some(&sig), None, Some(&commit_message(message)), Some(&tree))?;
    Ok(oid.to_string()[..8].to_string())
}

/// Message of the HEAD commit (the amend prefill).
pub fn head_message(root: &Path) -> Result<String> {
    let (repo, _) = open(root)?;
    let head = repo.head().and_then(|h| h.peel_to_commit()).map_err(|_| anyhow!("Nothing to amend: no commit yet"))?;
    Ok(String::from_utf8_lossy(head.message_bytes()).trim_end().to_string())
}

/// Throw away changes, like `git restore`. Staged paths go back to HEAD in
/// the index and the working tree (`checkout_head`); unstaged paths go back
/// to the index, so staged work survives (`checkout_index`); untracked
/// files, and new files that HEAD does not have, are deleted. Returns a
/// line for the console.
pub fn discard(root: &Path, items: &[(Section, String)]) -> Result<String> {
    let (repo, wd) = open(root)?;
    let of = |sec: Section| items.iter().filter(move |(s, _)| *s == sec).map(|(_, p)| p.clone()).collect::<Vec<_>>();
    let (staged, unstaged, untracked) = (of(Section::Staged), of(Section::Unstaged), of(Section::Untracked));
    let checkout = |paths: &[String]| {
        let mut b = git2::build::CheckoutBuilder::new();
        b.force().disable_pathspec_match(true);
        for p in paths {
            b.path(p);
        }
        b
    };
    let mut deleted = untracked.clone();
    if !staged.is_empty() {
        unstage(root, &staged)?;
        let head = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        let (known, new): (Vec<String>, Vec<String>) =
            staged.iter().cloned().partition(|p| head.as_ref().is_some_and(|t| t.get_path(Path::new(p)).is_ok()));
        if !known.is_empty() {
            repo.checkout_head(Some(&mut checkout(&known))).context("checkout from HEAD")?;
        }
        deleted.extend(new);
    }
    if !unstaged.is_empty() {
        let index = repo.index()?;
        // The new side of a rename in the working tree is not in the index.
        let (known, new): (Vec<String>, Vec<String>) = unstaged.iter().cloned().partition(|p| index.get_path(Path::new(p), 0).is_some());
        if !known.is_empty() {
            repo.checkout_index(None, Some(&mut checkout(&known))).context("checkout from the index")?;
        }
        deleted.extend(new);
    }
    for p in &deleted {
        let abs = wd.join(p);
        if abs.symlink_metadata().is_ok() {
            std::fs::remove_file(&abs).with_context(|| format!("delete {p}"))?;
        }
    }
    let n = staged.len() + unstaged.len() + untracked.len();
    Ok(format!("Discarded changes to {n} path{}", if n == 1 { "" } else { "s" }))
}

/// Blame of the file at `abs` as of HEAD.
pub fn blame(root: &Path, abs: &Path) -> Result<Blame> {
    let (repo, wd) = open(root)?;
    let rel = rel_path(&wd, abs).ok_or_else(|| anyhow!("{} is outside the repository", abs.display()))?;
    let blame = repo.blame_file(Path::new(&rel), None).map_err(|e| anyhow!("{rel}: {}", e.message()))?;
    let mut out = Blame::default();
    let mut index: std::collections::HashMap<git2::Oid, u32> = Default::default();
    for h in blame.iter() {
        let id = h.final_commit_id();
        let i = *index.entry(id).or_insert_with(|| {
            let (author, when) = match h.final_signature() {
                Some(s) => (String::from_utf8_lossy(s.name_bytes()).into_owned(), s.when()),
                None => (String::new(), git2::Time::new(0, 0)),
            };
            out.commits.push(BlameCommit { sha: id.to_string(), author, time: when.seconds(), offset_minutes: when.offset_minutes() });
            (out.commits.len() - 1) as u32
        });
        // Hunks come in line order (1-based start lines).
        let start = h.final_start_line().saturating_sub(1);
        out.lines.resize(start, i);
        out.lines.extend(std::iter::repeat_n(i, h.lines_in_hunk()));
    }
    Ok(out)
}

/// Stashes, newest first (`index` 0 is `stash@{0}`).
pub fn stashes(root: &Path) -> Result<Vec<StashEntry>> {
    let (mut repo, _) = open(root)?;
    let mut out = Vec::new();
    repo.stash_foreach(|index, message, oid| {
        out.push(StashEntry { index, message: message.to_string(), oid: oid.to_string() });
        true
    })?;
    Ok(out)
}

/// What to do with the stash list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashOp {
    /// Stash tracked changes; an empty message lets git write "WIP on …".
    Push {
        message: String,
    },
    /// The stash is named by its commit, so a list that changed since it was
    /// shown cannot hit the wrong entry.
    Pop {
        oid: String,
    },
    Apply {
        oid: String,
    },
    Drop {
        oid: String,
    },
}

/// Run a stash operation; returns a line for the console.
pub fn stash(root: &Path, op: &StashOp) -> Result<String> {
    let (mut repo, _) = open(root)?;
    let find = |repo: &mut Repository, oid: &str| -> Result<usize> {
        let mut found = None;
        repo.stash_foreach(|i, _, o| {
            if o.to_string() == oid {
                found = Some(i);
            }
            found.is_none()
        })?;
        found.ok_or_else(|| anyhow!("That stash no longer exists; the list changed"))
    };
    let applied = |e: git2::Error| match e.code() {
        git2::ErrorCode::Conflict | git2::ErrorCode::MergeConflict => {
            anyhow!("Cannot apply the stash: it conflicts with local changes; commit or stash them first ({})", e.message())
        }
        _ => anyhow!("{}", e.message()),
    };
    Ok(match op {
        StashOp::Push { message } => {
            let sig = signature(&repo)?;
            let msg = Some(message.trim()).filter(|m| !m.is_empty());
            match repo.stash_save2(&sig, msg, None) {
                Ok(_) => "Stashed changes as stash@{0}".to_string(),
                Err(e) if e.code() == git2::ErrorCode::NotFound => bail!("No local changes to stash"),
                Err(e) => return Err(e.into()),
            }
        }
        StashOp::Pop { oid } => {
            let i = find(&mut repo, oid)?;
            repo.stash_pop(i, None).map_err(applied)?;
            format!("Popped stash@{{{i}}}")
        }
        StashOp::Apply { oid } => {
            let i = find(&mut repo, oid)?;
            repo.stash_apply(i, None).map_err(applied)?;
            format!("Applied stash@{{{i}}}")
        }
        StashOp::Drop { oid } => {
            let i = find(&mut repo, oid)?;
            repo.stash_drop(i)?;
            format!("Dropped stash@{{{i}}}")
        }
    })
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

/// Where `git push -u` sends a branch that has no upstream yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRemote {
    /// Configured (`branch.<name>.pushRemote`, `remote.pushDefault`) or the
    /// only remote.
    One(String),
    /// Several remotes and no configuration: ask.
    Choose(Vec<String>),
    NoRemote,
}

/// Pick the push remote the way git does for `push -u`: the branch's
/// `pushRemote`, then `remote.pushDefault`, then the only remote.
pub fn pick_push_remote(branch_push_remote: Option<String>, push_default: Option<String>, remotes: Vec<String>) -> PushRemote {
    if let Some(r) = branch_push_remote.or(push_default).filter(|r| !r.is_empty()) {
        return PushRemote::One(r);
    }
    match remotes.len() {
        0 => PushRemote::NoRemote,
        1 => PushRemote::One(remotes.into_iter().next().unwrap_or_default()),
        _ => PushRemote::Choose(remotes),
    }
}

pub fn push_remote(root: &Path, branch: &str) -> Result<PushRemote> {
    let (repo, _) = open(root)?;
    let cfg = repo.config()?;
    let get = |key: &str| cfg.get_string(key).ok();
    let remotes = repo.remotes()?.iter().filter_map(|r| r.ok().flatten().map(str::to_string)).collect();
    Ok(pick_push_remote(get(&format!("branch.{branch}.pushRemote")), get("remote.pushDefault"), remotes))
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
        let (old, new) = change_texts(&r.dir, Section::Staged, "a.txt", None).unwrap();
        assert_eq!((old.as_slice(), new.as_slice()), (b"one\n".as_slice(), b"one\nmore\n".as_slice()));
        unstage_all(&r.dir).unwrap();
        assert!(sections(&r.dir).iter().all(|(s, _, _)| *s == Section::Unstaged));
        assert_eq!(blob_text(&r.dir, &r.dir.join("a.txt"), BlobWhich::Head).as_deref(), Some("one\n"));

        let b = branches(&r.dir).unwrap();
        assert_eq!(b.len(), 1);
        assert!(b[0].current);
    }

    #[test]
    fn push_remote_prefers_configuration_then_the_only_remote() {
        let r = TempRepo::new("push-remote");
        assert_eq!(push_remote(&r.dir, "main").unwrap(), PushRemote::NoRemote);
        let repo = Repository::open(&r.dir).unwrap();
        repo.remote("upstream", "https://example.com/a.git").unwrap();
        assert_eq!(push_remote(&r.dir, "main").unwrap(), PushRemote::One("upstream".into()));
        repo.remote("fork", "https://example.com/b.git").unwrap();
        assert_eq!(push_remote(&r.dir, "main").unwrap(), PushRemote::Choose(vec!["fork".into(), "upstream".into()]));
        let mut cfg = git2::Config::open(&r.dir.join(".git/config")).unwrap();
        cfg.set_str("remote.pushDefault", "fork").unwrap();
        assert_eq!(push_remote(&r.dir, "main").unwrap(), PushRemote::One("fork".into()));
        cfg.set_str("branch.main.pushRemote", "upstream").unwrap();
        assert_eq!(push_remote(&r.dir, "main").unwrap(), PushRemote::One("upstream".into()));
        assert_eq!(push_remote(&r.dir, "other").unwrap(), PushRemote::One("fork".into()));
    }

    #[test]
    fn renames_show_old_and_new_path() {
        let r = TempRepo::new("renames");
        let text = "line one\nline two\nline three\nline four\n";
        r.write("old.txt", text);
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        // A rename in the working tree: deleted tracked file plus a new one.
        std::fs::rename(r.dir.join("old.txt"), r.dir.join("new.txt")).unwrap();
        r.write("new.txt", &format!("{text}line five\n"));
        let snap = status(&r.dir).unwrap();
        let e = &snap.entries[0];
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(
            (e.section, e.status, e.rel.as_str(), e.old_rel.as_deref()),
            (Section::Unstaged, FileStatus::Renamed, "new.txt", Some("old.txt"))
        );
        assert_eq!(e.label(), "old.txt -> new.txt");
        let (old, new) = change_texts(&r.dir, Section::Unstaged, "new.txt", Some("old.txt")).unwrap();
        assert_eq!(old, text.as_bytes());
        assert!(new.ends_with(b"five\n"));
        // Staging both paths records the rename in the index.
        stage(&r.dir, &e.paths()).unwrap();
        let snap = status(&r.dir).unwrap();
        let e = &snap.entries[0];
        assert_eq!(
            (e.section, e.status, e.rel.as_str(), e.old_rel.as_deref()),
            (Section::Staged, FileStatus::Renamed, "new.txt", Some("old.txt"))
        );
        let (old, _) = change_texts(&r.dir, Section::Staged, "new.txt", Some("old.txt")).unwrap();
        assert_eq!(old, text.as_bytes());
        // Edited again after staging: the unstaged entry is the new path.
        r.write("new.txt", "edited\n");
        let snap = status(&r.dir).unwrap();
        let e = &snap.entries[1];
        assert_eq!((e.section, e.status, e.rel.as_str(), e.old_rel.as_deref()), (Section::Unstaged, FileStatus::Modified, "new.txt", None));
        unstage(&r.dir, &snap.entries[0].paths()).unwrap();
        assert_eq!(status(&r.dir).unwrap().entries[0].section, Section::Unstaged);
    }

    #[test]
    fn discard_restores_head_or_index_and_deletes_new_files() {
        let r = TempRepo::new("discard");
        r.write("a.txt", "a\n");
        r.write("b.txt", "b\n");
        r.write("c.txt", "c\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        // a: staged edit plus an unstaged edit on top; discarding the
        // unstaged side keeps the staged one.
        r.write("a.txt", "a staged\n");
        stage(&r.dir, &["a.txt".into()]).unwrap();
        r.write("a.txt", "a unstaged\n");
        // b: deleted; c: staged edit; new.txt: staged new file; u.txt: untracked.
        std::fs::remove_file(r.dir.join("b.txt")).unwrap();
        r.write("c.txt", "c staged\n");
        r.write("new.txt", "new\n");
        stage(&r.dir, &["c.txt".into(), "new.txt".into()]).unwrap();
        r.write("u.txt", "u\n");
        discard(&r.dir, &[(Section::Unstaged, "a.txt".into()), (Section::Unstaged, "b.txt".into())]).unwrap();
        let read = |p: &str| std::fs::read_to_string(r.dir.join(p)).ok();
        assert_eq!(read("a.txt").as_deref(), Some("a staged\n"));
        assert_eq!(read("b.txt").as_deref(), Some("b\n"));
        discard(&r.dir, &[(Section::Staged, "a.txt".into()), (Section::Staged, "c.txt".into()), (Section::Staged, "new.txt".into())])
            .unwrap();
        assert_eq!(read("a.txt").as_deref(), Some("a\n"));
        assert_eq!(read("c.txt").as_deref(), Some("c\n"));
        assert_eq!(read("new.txt"), None);
        assert_eq!(sections(&r.dir), vec![(Section::Untracked, FileStatus::Untracked, "u.txt".into())]);
        discard(&r.dir, &[(Section::Untracked, "u.txt".into())]).unwrap();
        assert!(status(&r.dir).unwrap().entries.is_empty());
    }

    #[test]
    fn amend_replaces_head_keeping_its_parent() {
        let r = TempRepo::new("amend");
        assert!(amend(&r.dir, "x").unwrap_err().to_string().contains("no commit yet"));
        r.write("a.txt", "a\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        r.write("b.txt", "b\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "second").unwrap();
        assert_eq!(head_message(&r.dir).unwrap(), "second");
        // Message only, then with a staged file.
        amend(&r.dir, "second, reworded").unwrap();
        r.write("c.txt", "c\n");
        stage_all(&r.dir).unwrap();
        amend(&r.dir, "second, with c\n\nbody").unwrap();
        assert_eq!(head_message(&r.dir).unwrap(), "second, with c\n\nbody");
        let repo = Repository::open(&r.dir).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent(0).unwrap().message_bytes(), b"first\n");
        assert!(head.tree().unwrap().get_path(Path::new("c.txt")).is_ok());
        assert!(status(&r.dir).unwrap().entries.is_empty());
    }

    #[test]
    fn stash_push_list_apply_pop_and_drop() {
        let r = TempRepo::new("stash");
        r.write("a.txt", "a\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        let push = |m: &str| stash(&r.dir, &StashOp::Push { message: m.into() });
        assert!(push("x").unwrap_err().to_string().contains("No local changes"));
        r.write("a.txt", "one\n");
        push("first change").unwrap();
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "a\n");
        r.write("a.txt", "two\n");
        push("").unwrap();
        let list = stashes(&r.dir).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].message.starts_with("WIP on"), "{}", list[0].message);
        assert!(list[1].message.ends_with("first change"), "{}", list[1].message);
        // Apply keeps the entry; pop removes it.
        stash(&r.dir, &StashOp::Apply { oid: list[1].oid.clone() }).unwrap();
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "one\n");
        assert_eq!(stashes(&r.dir).unwrap().len(), 2);
        // Applying over local changes to the same file fails and keeps them.
        assert!(stash(&r.dir, &StashOp::Pop { oid: list[0].oid.clone() }).is_err());
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "one\n");
        discard(&r.dir, &[(Section::Unstaged, "a.txt".into())]).unwrap();
        stash(&r.dir, &StashOp::Pop { oid: list[0].oid.clone() }).unwrap();
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "two\n");
        let left = stashes(&r.dir).unwrap();
        assert_eq!((left.len(), left[0].oid.as_str()), (1, list[1].oid.as_str()));
        assert!(stash(&r.dir, &StashOp::Drop { oid: list[0].oid.clone() }).unwrap_err().to_string().contains("no longer exists"));
        stash(&r.dir, &StashOp::Drop { oid: list[1].oid.clone() }).unwrap();
        assert!(stashes(&r.dir).unwrap().is_empty());
    }

    #[test]
    fn blame_names_the_commit_of_every_line() {
        let r = TempRepo::new("blame");
        r.write("a.txt", "one\ntwo\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        r.write("a.txt", "one\nTWO\nthree\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "second").unwrap();
        let b = blame(&r.dir, &r.dir.join("a.txt")).unwrap();
        assert_eq!(b.lines.len(), 3);
        assert_eq!(b.commits.len(), 2);
        assert_ne!(b.lines[0], b.lines[1]);
        assert_eq!(b.lines[1], b.lines[2]);
        assert_eq!(b.commits[0].author, "Test");
        r.write("new.txt", "x\n");
        assert!(blame(&r.dir, &r.dir.join("new.txt")).is_err(), "untracked");
    }
}
