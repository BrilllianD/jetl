//! Synchronous git2 operations. Each function opens the repository fresh and
//! is meant to run inside `spawn_blocking`; no `Repository` outlives a call.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use git2::{BranchType, IndexAddOption, ObjectType, Repository, Status, StatusOptions};

use super::merge::{ConflictSides, Special};
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

/// Index entry modes of a symbolic link and of a submodule.
const SYMLINK: u32 = 0o120000;
const GITLINK: u32 = 0o160000;
/// The bits of an index entry's flags that hold its stage.
const STAGE_MASK: u16 = 0x3000;

/// The index's conflict entry for `rel`, if it has one.
fn find_conflict(index: &git2::Index, rel: &str) -> Result<Option<git2::IndexConflict>> {
    for c in index.conflicts()? {
        let c = c?;
        let path = [&c.our, &c.their, &c.ancestor].into_iter().flatten().next().map(|e| e.path.clone());
        if path.as_deref() == Some(rel.as_bytes()) {
            return Ok(Some(c));
        }
    }
    Ok(None)
}

/// The stages of conflicted `abs` (each `None` when missing, all `None`
/// when the index records no conflict for it), its work tree file, and
/// what to call yours and theirs.
pub fn conflict_sides(root: &Path, abs: &Path) -> Result<ConflictSides> {
    let (repo, wd) = open(root)?;
    let rel = rel_path(&wd, abs).ok_or_else(|| anyhow!("{} is outside the repository", abs.display()))?;
    let index = repo.index()?;
    let blob = |e: &Option<git2::IndexEntry>| -> Result<Option<Vec<u8>>> {
        e.as_ref()
            .map(|e| if e.mode == GITLINK { Ok(e.id.to_string().into_bytes()) } else { Ok(repo.find_blob(e.id)?.content().to_vec()) })
            .transpose()
    };
    let mut sides = ConflictSides { rel: rel.clone(), ..Default::default() };
    if let Some(c) = find_conflict(&index, &rel)? {
        (sides.base, sides.ours, sides.theirs) = (blob(&c.ancestor)?, blob(&c.our)?, blob(&c.their)?);
        let modes = || [&c.ancestor, &c.our, &c.their].into_iter().flatten().map(|e| e.mode);
        sides.special = if modes().any(|m| m == GITLINK) {
            Some(Special::Submodule)
        } else if modes().any(|m| m == SYMLINK) {
            Some(Special::Symlink)
        } else {
            None
        };
    }
    if sides.special.is_none() {
        sides.work = std::fs::read(wd.join(&rel)).unwrap_or_default();
    }
    (sides.ours_label, sides.theirs_label) = conflict_labels(&repo);
    Ok(sides)
}

/// Names for yours and theirs from the operation that stopped: the
/// branch (else commit) for a merge, the commit picked or reverted, and
/// during a rebase the upstream as yours and the commit being replayed as
/// theirs.
fn conflict_labels(repo: &Repository) -> (String, String) {
    let short = |oid: git2::Oid| oid.to_string()[..7].to_string();
    let head = repo.head().ok();
    let head_sha = head.as_ref().and_then(|h| h.target()).map(short);
    let head_name = match &head {
        Some(h) if h.is_branch() => h.shorthand().ok().map(str::to_string),
        _ => head_sha.clone(),
    }
    .unwrap_or_else(|| "HEAD".into());
    let read = |name: &str| {
        let text = std::fs::read_to_string(repo.path().join(name)).ok()?;
        git2::Oid::from_str(text.lines().next()?.trim()).ok()
    };
    let branch_at = |oid: git2::Oid| {
        let branches = repo.branches(Some(BranchType::Local)).ok()?;
        branches.flatten().find(|(b, _)| b.get().target() == Some(oid)).and_then(|(b, _)| b.name().ok().flatten().map(str::to_string))
    };
    let subject = |oid: git2::Oid| {
        let summary = repo.find_commit(oid).ok().and_then(|c| c.summary().ok().flatten().map(str::to_string));
        format!("{} {}", short(oid), summary.unwrap_or_default()).trim_end().to_string()
    };
    use git2::RepositoryState as S;
    let theirs = match repo.state() {
        S::Merge => read("MERGE_HEAD").map(|o| branch_at(o).unwrap_or_else(|| short(o))),
        S::CherryPick | S::CherryPickSequence => read("CHERRY_PICK_HEAD").map(|o| format!("cherry-pick {}", subject(o))),
        S::Revert | S::RevertSequence => read("REVERT_HEAD").map(|o| format!("revert {}", subject(o))),
        S::Rebase | S::RebaseInteractive | S::RebaseMerge => {
            let replayed = read("REBASE_HEAD").map(subject).unwrap_or_else(|| "the commit being rebased".into());
            return (format!("upstream {}", head_sha.unwrap_or_default()).trim_end().to_string(), replayed);
        }
        _ => None,
    };
    (head_name, theirs.unwrap_or_else(|| "theirs".into()))
}

/// Which side of a conflict to take whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    Ours,
    Theirs,
}

/// Make room for a new file or link at `abs`: remove a file or symlink
/// there (never following it); refuse a directory.
fn clear_path(abs: &Path, rel: &str) -> Result<()> {
    match abs.symlink_metadata() {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => bail!("{rel} is a directory in the work tree; resolve it with the git command line"),
        Ok(_) => std::fs::remove_file(abs).with_context(|| format!("replace {rel}")),
    }
}

/// Resolve conflicted `rel` by taking one side whole, like `git checkout
/// --ours/--theirs` and `git add`: that stage's blob is written to the
/// work tree (its executable bit kept) and staged; a symbolic link is
/// recreated as a link; a submodule's commit is staged without touching
/// the work tree; a side that deleted the path deletes it (a submodule's
/// checkout stays) and stages the deletion. Whatever file or link is in
/// the way is replaced, never written through. Returns a line for the
/// console.
pub fn resolve_side(root: &Path, rel: &str, side: ConflictSide) -> Result<String> {
    let (repo, wd) = open(root)?;
    let mut index = repo.index()?;
    let c = find_conflict(&index, rel)?.ok_or_else(|| anyhow!("{rel} has no conflict to resolve"))?;
    let submodule = [&c.ancestor, &c.our, &c.their].into_iter().flatten().any(|e| e.mode == GITLINK);
    let (entry, name) = match side {
        ConflictSide::Ours => (c.our, "yours"),
        ConflictSide::Theirs => (c.their, "theirs"),
    };
    let abs = wd.join(rel);
    let mut line = format!("Resolved {rel} with {name}");
    match entry {
        Some(e) if e.mode == GITLINK => {
            let id = e.id;
            index.remove_path(Path::new(rel)).with_context(|| format!("clear conflict of {rel}"))?;
            index.add(&git2::IndexEntry { flags: e.flags & !STAGE_MASK, ..e }).with_context(|| format!("stage {rel}"))?;
            let id = id.to_string();
            line = format!("{line} (submodule commit {}; run `git submodule update` to check it out)", &id[..7]);
        }
        Some(e) if e.mode == SYMLINK => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let blob = repo.find_blob(e.id)?;
                clear_path(&abs, rel)?;
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(blob.content()), &abs).with_context(|| format!("link {rel}"))?;
                index.add_path(Path::new(rel)).with_context(|| format!("stage {rel}"))?;
            }
            #[cfg(not(unix))]
            bail!("{rel} is a symbolic link; resolve it with the git command line");
        }
        Some(e) => {
            let blob = repo.find_blob(e.id)?;
            clear_path(&abs, rel)?;
            std::fs::write(&abs, blob.content()).with_context(|| format!("write {rel}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if e.mode & 0o111 != 0 { 0o755 } else { 0o644 };
                std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(mode)).with_context(|| format!("chmod {rel}"))?;
            }
            index.add_path(Path::new(rel)).with_context(|| format!("stage {rel}"))?;
        }
        None if submodule => {
            index.remove_path(Path::new(rel)).with_context(|| format!("stage deletion of {rel}"))?;
        }
        None => {
            clear_path(&abs, rel)?;
            index.remove_path(Path::new(rel)).with_context(|| format!("stage deletion of {rel}"))?;
        }
    }
    index.write()?;
    Ok(line)
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

/// Run a stash push or drop; returns a line for the console. Pop and apply
/// go through `cli::stash_apply`, so conflicts reach the resolver.
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
        StashOp::Pop { .. } | StashOp::Apply { .. } => {
            bail!("stash pop and apply run through the git command line (cli::stash_apply)")
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
        out.push(BranchInfo { name, upstream, ahead, behind, current, remote: false });
    }
    out.sort_by(|a, b| b.current.cmp(&a.current).then_with(|| a.name.cmp(&b.name)));
    let mut remotes = Vec::new();
    for b in repo.branches(Some(BranchType::Remote))? {
        let (b, _) = b?;
        // `origin/HEAD` points at another remote branch.
        if b.get().symbolic_target().ok().flatten().is_some() {
            continue;
        }
        let Some(name) = b.name()?.map(str::to_string) else { continue };
        remotes.push(BranchInfo { name, upstream: None, ahead: 0, behind: 0, current: false, remote: true });
    }
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(remotes);
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

    #[tokio::test]
    async fn stash_push_list_apply_pop_and_drop() {
        use crate::git::cli::stash_apply;
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
        let applied = stash_apply(&r.dir, &list[1].oid, false).await.unwrap();
        assert_eq!(applied, (true, "Applied stash@{1}".to_string()));
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "one\n");
        assert_eq!(stashes(&r.dir).unwrap().len(), 2);
        // Popping over local changes to the same file is refused and keeps them.
        let (ok, output) = stash_apply(&r.dir, &list[0].oid, true).await.unwrap();
        assert!(!ok && !output.contains("CONFLICT"), "{output}");
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "one\n");
        assert_eq!(stashes(&r.dir).unwrap().len(), 2);
        discard(&r.dir, &[(Section::Unstaged, "a.txt".into())]).unwrap();
        let popped = stash_apply(&r.dir, &list[0].oid, true).await.unwrap();
        assert_eq!(popped, (true, "Popped stash@{0}".to_string()));
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "two\n");
        let left = stashes(&r.dir).unwrap();
        assert_eq!((left.len(), left[0].oid.as_str()), (1, list[1].oid.as_str()));
        let stale = stash_apply(&r.dir, &list[0].oid, true).await.unwrap_err();
        assert!(stale.to_string().contains("no longer exists"), "{stale}");
        assert!(stash(&r.dir, &StashOp::Drop { oid: list[0].oid.clone() }).unwrap_err().to_string().contains("no longer exists"));
        stash(&r.dir, &StashOp::Drop { oid: list[1].oid.clone() }).unwrap();
        assert!(stashes(&r.dir).unwrap().is_empty());
    }

    #[tokio::test]
    async fn stash_pop_conflicting_with_head_leaves_conflicts() {
        let r = TempRepo::new("stash-conflict");
        r.write("a.txt", "a\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        r.write("a.txt", "mine\n");
        stash(&r.dir, &StashOp::Push { message: "wip".into() }).unwrap();
        r.write("a.txt", "head\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "second").unwrap();
        let oid = stashes(&r.dir).unwrap()[0].oid.clone();
        let (ok, output) = crate::git::cli::stash_apply(&r.dir, &oid, true).await.unwrap();
        assert!(!ok && output.contains("CONFLICT"), "{output}");
        assert!(open(&r.dir).unwrap().0.index().unwrap().has_conflicts());
        assert_eq!(stashes(&r.dir).unwrap().len(), 1, "a conflicting pop keeps the entry");
        let sides = conflict_sides(&r.dir, &r.dir.join("a.txt")).unwrap();
        assert_eq!(sides.ours, Some(b"head\n".to_vec()));
        assert_eq!(sides.theirs, Some(b"mine\n".to_vec()));
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

    /// Run the git command line in `dir`; `ok` false allows a failure
    /// (a merge stopping on conflicts).
    fn git_cli(dir: &Path, args: &[&str], ok: bool) {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(!ok || out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repository where merging `dev` into the first branch conflicts in
    /// a.txt (both changed line two; only the first branch added line
    /// four) and b.txt (modified on the first branch, deleted on dev).
    fn conflicted_merge(name: &str) -> TempRepo {
        let r = TempRepo::new(name);
        r.write("a.txt", "one\ntwo\nthree\n");
        r.write("b.txt", "b\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        let main = status(&r.dir).unwrap().branch.unwrap();
        git_cli(&r.dir, &["switch", "-qc", "dev"], true);
        r.write("a.txt", "one\nTWO dev\nthree\n");
        std::fs::remove_file(r.dir.join("b.txt")).unwrap();
        git_cli(&r.dir, &["commit", "-qam", "dev change"], true);
        git_cli(&r.dir, &["switch", "-q", &main], true);
        r.write("a.txt", "one\nTWO main\nthree\nfour\n");
        r.write("b.txt", "b changed\n");
        git_cli(&r.dir, &["commit", "-qam", "main change"], true);
        git_cli(&r.dir, &["merge", "-q", "dev"], false);
        r
    }

    #[test]
    fn conflict_sides_read_the_stages_and_resolve_side_takes_one() {
        use crate::git::merge::{ChunkKind, MergeInput, chunks};
        let r = conflicted_merge("conflict-sides");
        let main = status(&r.dir).unwrap().branch.unwrap();
        let s = conflict_sides(&r.dir, &r.dir.join("a.txt")).unwrap();
        assert_eq!(s.base.as_deref(), Some(b"one\ntwo\nthree\n".as_slice()));
        assert_eq!(s.ours.as_deref(), Some(b"one\nTWO main\nthree\nfour\n".as_slice()));
        assert_eq!(s.theirs.as_deref(), Some(b"one\nTWO dev\nthree\n".as_slice()));
        assert!(String::from_utf8_lossy(&s.work).contains("<<<<<<<"));
        assert_eq!((s.ours_label.as_str(), s.theirs_label.as_str()), (main.as_str(), "dev"));
        let MergeInput::Text(t) = MergeInput::classify(&s).unwrap() else { panic!("a text merge") };
        let kinds: Vec<ChunkKind> = chunks(&t.base, &t.ours, &t.theirs).into_iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [ChunkKind::Conflict, ChunkKind::OursOnly]);
        // Modified here, deleted on dev: no line merge.
        let b = conflict_sides(&r.dir, &r.dir.join("b.txt")).unwrap();
        assert!(b.theirs.is_none() && b.ours.is_some());
        assert!(matches!(MergeInput::classify(&b).unwrap(), MergeInput::SidesOnly(_)));
        // Taking a side writes and stages it.
        resolve_side(&r.dir, "a.txt", ConflictSide::Theirs).unwrap();
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "one\nTWO dev\nthree\n");
        resolve_side(&r.dir, "b.txt", ConflictSide::Theirs).unwrap();
        assert!(!r.dir.join("b.txt").exists(), "theirs deleted it");
        let repo = Repository::open(&r.dir).unwrap();
        let index = repo.index().unwrap();
        assert!(!index.has_conflicts());
        assert!(index.get_path(Path::new("b.txt"), 0).is_none());
        assert!(resolve_side(&r.dir, "a.txt", ConflictSide::Ours).unwrap_err().to_string().contains("no conflict"));
        // No stages left: the work file has no markers either.
        let s = conflict_sides(&r.dir, &r.dir.join("a.txt")).unwrap();
        assert!(s.ours.is_none());
        assert!(MergeInput::classify(&s).unwrap_err().to_string().contains("no conflict markers"));
    }

    #[test]
    fn resolve_side_keeps_a_file_the_other_side_deleted() {
        let r = conflicted_merge("conflict-keep");
        resolve_side(&r.dir, "b.txt", ConflictSide::Ours).unwrap();
        assert_eq!(std::fs::read_to_string(r.dir.join("b.txt")).unwrap(), "b changed\n");
        // Yours is HEAD's version: nothing left to show for it.
        assert!(status(&r.dir).unwrap().entries.iter().all(|e| e.rel != "b.txt"));
        let repo = Repository::open(&r.dir).unwrap();
        assert!(repo.index().unwrap().get_path(Path::new("b.txt"), 0).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_conflicts_take_a_side_as_a_link() {
        use crate::git::merge::{MergeInput, Special};
        let r = TempRepo::new("conflict-symlink");
        r.write("a.txt", "a\n");
        std::os::unix::fs::symlink("x", r.dir.join("l")).unwrap();
        git_cli(&r.dir, &["add", "-A"], true);
        git_cli(&r.dir, &["commit", "-qm", "first"], true);
        let main = status(&r.dir).unwrap().branch.unwrap();
        git_cli(&r.dir, &["switch", "-qc", "dev"], true);
        std::fs::remove_file(r.dir.join("l")).unwrap();
        std::os::unix::fs::symlink("dev-target", r.dir.join("l")).unwrap();
        git_cli(&r.dir, &["commit", "-qam", "dev"], true);
        git_cli(&r.dir, &["switch", "-q", &main], true);
        std::fs::remove_file(r.dir.join("l")).unwrap();
        std::os::unix::fs::symlink("main-target", r.dir.join("l")).unwrap();
        git_cli(&r.dir, &["commit", "-qam", "main"], true);
        git_cli(&r.dir, &["merge", "-q", "dev"], false);
        let s = conflict_sides(&r.dir, &r.dir.join("l")).unwrap();
        assert_eq!(s.special, Some(Special::Symlink));
        assert_eq!(s.ours, Some(b"main-target".to_vec()));
        assert_eq!(s.theirs, Some(b"dev-target".to_vec()));
        assert!(matches!(MergeInput::classify(&s).unwrap(), MergeInput::SidesOnly(r) if r.contains("symbolic link")));
        resolve_side(&r.dir, "l", ConflictSide::Theirs).unwrap();
        assert_eq!(std::fs::read_link(r.dir.join("l")).unwrap(), Path::new("dev-target"));
        let repo = Repository::open(&r.dir).unwrap();
        let index = repo.index().unwrap();
        assert!(!index.has_conflicts());
        assert_eq!(index.get_path(Path::new("l"), 0).unwrap().mode, 0o120000);
    }

    #[test]
    fn submodule_conflicts_stage_the_commit_without_touching_the_work_tree() {
        use crate::git::merge::{MergeInput, Special};
        let r = TempRepo::new("conflict-gitlink");
        r.write("a.txt", "a\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        let mut shas = Vec::new();
        for m in ["c1", "c2", "c3"] {
            git_cli(&r.dir, &["commit", "-q", "--allow-empty", "-m", m], true);
            shas.push(Repository::open(&r.dir).unwrap().head().unwrap().target().unwrap().to_string());
        }
        let [c1, c2, c3] = [shas[0].clone(), shas[1].clone(), shas[2].clone()];
        let main = status(&r.dir).unwrap().branch.unwrap();
        git_cli(&r.dir, &["update-index", "--add", "--cacheinfo", &format!("160000,{c1},sub")], true);
        git_cli(&r.dir, &["commit", "-qm", "add sub"], true);
        git_cli(&r.dir, &["switch", "-qc", "dev"], true);
        git_cli(&r.dir, &["update-index", "--cacheinfo", &format!("160000,{c2},sub")], true);
        git_cli(&r.dir, &["commit", "-qm", "dev sub"], true);
        git_cli(&r.dir, &["switch", "-q", &main], true);
        git_cli(&r.dir, &["update-index", "--cacheinfo", &format!("160000,{c3},sub")], true);
        git_cli(&r.dir, &["commit", "-qm", "main sub"], true);
        git_cli(&r.dir, &["merge", "-q", "dev"], false);
        let s = conflict_sides(&r.dir, &r.dir.join("sub")).unwrap();
        assert_eq!(s.special, Some(Special::Submodule));
        assert_eq!(s.ours, Some(c3.clone().into_bytes()));
        assert!(matches!(MergeInput::classify(&s).unwrap(), MergeInput::SidesOnly(r) if r.contains("submodule")));
        let st = status(&r.dir).unwrap();
        assert!(st.entries.iter().any(|e| e.rel == "sub" && e.status == FileStatus::Conflicted));
        let before = r.dir.join("sub").exists();
        let line = resolve_side(&r.dir, "sub", ConflictSide::Theirs).unwrap();
        assert!(line.contains("submodule commit") && line.contains(&c2[..7]), "{line}");
        assert_eq!(r.dir.join("sub").exists(), before);
        let repo = Repository::open(&r.dir).unwrap();
        let index = repo.index().unwrap();
        assert!(!index.has_conflicts());
        let e = index.get_path(Path::new("sub"), 0).unwrap();
        assert_eq!((e.mode, e.id.to_string()), (0o160000, c2));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_side_never_writes_through_a_symlink() {
        let r = conflicted_merge("conflict-through-link");
        std::fs::remove_file(r.dir.join("a.txt")).unwrap();
        std::os::unix::fs::symlink("b.txt", r.dir.join("a.txt")).unwrap();
        resolve_side(&r.dir, "a.txt", ConflictSide::Theirs).unwrap();
        assert!(r.dir.join("a.txt").symlink_metadata().unwrap().file_type().is_file());
        assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "one\nTWO dev\nthree\n");
        assert_eq!(std::fs::read_to_string(r.dir.join("b.txt")).unwrap(), "b changed\n");
    }

    #[test]
    fn branches_list_local_then_remote() {
        let r = TempRepo::new("branches");
        r.write("a.txt", "a\n");
        stage_all(&r.dir).unwrap();
        commit(&r.dir, "first").unwrap();
        let repo = Repository::open(&r.dir).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("zeta", &head, false).unwrap();
        repo.reference("refs/remotes/origin/main", head.id(), false, "test").unwrap();
        repo.reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", false, "test").unwrap();
        let b = branches(&r.dir).unwrap();
        let names: Vec<(&str, bool, bool)> = b.iter().map(|b| (b.name.as_str(), b.current, b.remote)).collect();
        assert_eq!(names.len(), 3);
        assert!(names[0].1, "current first");
        assert_eq!(names[1], ("zeta", false, false));
        assert_eq!(names[2], ("origin/main", false, true));
    }
}
