//! Git data types shared between the service, panels and the tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::Style;

use crate::config::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileStatus {
    Ignored,
    Untracked,
    Added,
    Modified,
    Deleted,
    Renamed,
    Conflicted,
}

impl FileStatus {
    pub fn style(self, theme: &Theme) -> Style {
        match self {
            FileStatus::Ignored => theme.git_ignored,
            FileStatus::Untracked => theme.git_untracked,
            FileStatus::Added => theme.git_added,
            FileStatus::Modified | FileStatus::Renamed => theme.git_modified,
            FileStatus::Deleted => theme.git_deleted,
            FileStatus::Conflicted => theme.git_conflicted,
        }
    }

    pub fn letter(self) -> char {
        match self {
            FileStatus::Ignored => '!',
            FileStatus::Untracked => '?',
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Conflicted => 'U',
        }
    }

    /// How alarming a status is when several meet in one directory.
    fn rank(self) -> u8 {
        match self {
            FileStatus::Ignored => 0,
            FileStatus::Untracked => 1,
            FileStatus::Added => 2,
            FileStatus::Deleted => 3,
            FileStatus::Renamed => 4,
            FileStatus::Modified => 5,
            FileStatus::Conflicted => 6,
        }
    }

    pub fn worst(a: FileStatus, b: FileStatus) -> FileStatus {
        if b.rank() > a.rank() { b } else { a }
    }
}

/// Which list of the Commit tab an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Staged,
    Unstaged,
    Untracked,
}

impl Section {
    pub const ALL: [Section; 3] = [Section::Staged, Section::Unstaged, Section::Untracked];

    pub fn title(self) -> &'static str {
        match self {
            Section::Staged => "Staged",
            Section::Unstaged => "Unstaged",
            Section::Untracked => "Untracked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEntry {
    pub section: Section,
    pub status: FileStatus,
    /// Path relative to the work tree, `/`-separated as git reports it.
    pub rel: String,
    pub abs: PathBuf,
    /// Path before a rename (`status` is `Renamed`).
    pub old_rel: Option<String>,
}

impl ChangeEntry {
    /// `rel`, or `old -> new` for a rename.
    pub fn label(&self) -> String {
        match &self.old_rel {
            Some(old) => format!("{old} -> {}", self.rel),
            None => self.rel.clone(),
        }
    }

    /// Paths to stage or unstage: a rename is its old and new path.
    pub fn paths(&self) -> Vec<String> {
        self.old_rel.iter().cloned().chain([self.rel.clone()]).collect()
    }
}

/// Result of one status scan.
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    pub workdir: PathBuf,
    /// Sorted by section, then path.
    pub entries: Vec<ChangeEntry>,
    /// Absolute path -> status for the project tree; directories carry the
    /// worst status of anything below them.
    pub tree: HashMap<PathBuf, FileStatus>,
    /// Current branch; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Full sha of the HEAD commit; `None` on an unborn branch.
    pub head: Option<String>,
}

impl StatusSnapshot {
    /// Branch name, or the short sha when detached.
    pub fn label(&self) -> Option<String> {
        self.branch.clone().or_else(|| self.head.as_ref().map(|h| h[..h.len().min(8)].to_string()))
    }

    pub fn count(&self, section: Section) -> usize {
        self.entries.iter().filter(|e| e.section == section).count()
    }
}

/// Build the tree color map from per-file statuses. Every ancestor directory
/// below `workdir` inherits the worst status of its children; a deleted or
/// renamed child marks its directory modified.
pub fn tree_status(workdir: &Path, files: impl IntoIterator<Item = (PathBuf, FileStatus)>) -> HashMap<PathBuf, FileStatus> {
    let mut map: HashMap<PathBuf, FileStatus> = HashMap::new();
    for (path, st) in files {
        let e = map.entry(path.clone()).or_insert(st);
        *e = FileStatus::worst(*e, st);
        let dir_st = match st {
            FileStatus::Deleted | FileStatus::Renamed => FileStatus::Modified,
            s => s,
        };
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if dir == workdir || !dir.starts_with(workdir) {
                break;
            }
            let e = map.entry(dir.to_path_buf()).or_insert(dir_st);
            *e = FileStatus::worst(*e, dir_st);
            cur = dir.parent();
        }
    }
    map
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub current: bool,
    /// A remote-tracking branch (`origin/main`); listed after local ones.
    pub remote: bool,
}

/// What the branches popup offers for a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchOp {
    Checkout,
    /// A remote branch: a local branch of the same name tracking it.
    CheckoutTracking,
    NewFrom,
    Merge,
    Rebase,
    Rename,
    Delete,
}

impl BranchOp {
    /// The operations for `b`, and their labels (`current` is the branch
    /// checked out now, if any).
    pub fn for_branch(b: &BranchInfo, current: Option<&str>) -> Vec<(BranchOp, String)> {
        let name = &b.name;
        let cur = current.unwrap_or("HEAD");
        let mut v = Vec::new();
        if b.remote {
            let local = name.split_once('/').map_or(name.as_str(), |(_, l)| l);
            v.push((BranchOp::CheckoutTracking, format!("Checkout as new local branch '{local}'")));
        } else if !b.current {
            v.push((BranchOp::Checkout, "Checkout".to_string()));
        }
        v.push((BranchOp::NewFrom, format!("New Branch from '{name}'…")));
        if !b.current {
            v.push((BranchOp::Merge, format!("Merge '{name}' into '{cur}'")));
            v.push((BranchOp::Rebase, format!("Rebase '{cur}' onto '{name}'…")));
        }
        if !b.remote {
            v.push((BranchOp::Rename, "Rename…".to_string()));
            if !b.current {
                v.push((BranchOp::Delete, "Delete…".to_string()));
            }
        }
        v
    }
}

/// `git blame` of a file at HEAD: the commit of every line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blame {
    pub commits: Vec<BlameCommit>,
    /// HEAD line -> index into `commits`.
    pub lines: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    pub sha: String,
    pub author: String,
    /// Author time: seconds since the epoch and the author's UTC offset.
    pub time: i64,
    pub offset_minutes: i32,
}

/// One entry of the stash list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    /// Position: `stash@{index}`.
    pub index: usize,
    pub message: String,
    /// The stash commit; its first parent is HEAD at the time.
    pub oid: String,
}

/// Where the Log tab's history starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LogRev {
    /// HEAD, or every branch with `[git] log_all`.
    #[default]
    Default,
    Head,
    All,
    /// A branch or commit.
    Rev(String),
}

impl LogRev {
    pub fn label(&self, log_all: bool) -> String {
        match self {
            LogRev::Default if log_all => "all".into(),
            LogRev::Default | LogRev::Head => "HEAD".into(),
            LogRev::All => "all".into(),
            LogRev::Rev(r) if r.len() == 40 && r.chars().all(|c| c.is_ascii_hexdigit()) => r[..8].to_string(),
            LogRev::Rev(r) => r.clone(),
        }
    }
}

/// What the Log tab lists: the history from `rev`, narrowed to commits
/// whose message contains `text`, whose author matches `author`, and that
/// touch `path` (each ignored when empty).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    pub rev: LogRev,
    pub text: String,
    pub author: String,
    /// Relative to the project root.
    pub path: String,
}

impl LogFilter {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One row of the Log tab: the graph cells (`git::graph`) and the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub graph: String,
    pub commit: LogCommit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCommit {
    pub sha: String,
    /// Full parent shas, first parent first.
    pub parents: Vec<String>,
    pub short: String,
    pub author: String,
    pub date: String,
    pub refs: String,
    pub subject: String,
}

#[derive(Debug, Clone, Default)]
pub struct CommitDetails {
    pub sha: String,
    pub body: String,
    /// From `git diff-tree --name-status`.
    pub files: Vec<CommitFile>,
}

/// One changed file of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    pub status: char,
    pub path: String,
    /// Source path of a rename or copy.
    pub old: Option<String>,
}

/// What the diff view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKey {
    /// A working tree change: Staged = HEAD vs index, Unstaged = index vs
    /// file, Untracked = nothing vs file. `old` is the path before a
    /// rename, read on the old side.
    Change { section: Section, rel: String, old: Option<String> },
    /// A file in a commit: `sha^:old` (or `sha^:path`) vs `sha:path`.
    Commit { sha: String, rel: String, old: Option<String> },
}

impl DiffKey {
    pub fn rel(&self) -> &str {
        match self {
            DiffKey::Change { rel, .. } | DiffKey::Commit { rel, .. } => rel,
        }
    }

    /// `rel`, or `old -> new` for a rename.
    pub fn label(&self) -> String {
        match self {
            DiffKey::Change { rel, old: Some(old), .. } | DiffKey::Commit { rel, old: Some(old), .. } => format!("{old} -> {rel}"),
            _ => self.rel().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_inherit_worst_child() {
        let wd = PathBuf::from("/r");
        let map = tree_status(
            &wd,
            vec![
                (PathBuf::from("/r/src/a/new.rs"), FileStatus::Untracked),
                (PathBuf::from("/r/src/a/x.rs"), FileStatus::Added),
                (PathBuf::from("/r/src/b/gone.rs"), FileStatus::Deleted),
                (PathBuf::from("/r/docs/c.md"), FileStatus::Untracked),
                (PathBuf::from("/r/src/main.rs"), FileStatus::Modified),
            ],
        );
        assert_eq!(map[&PathBuf::from("/r/src/a/new.rs")], FileStatus::Untracked);
        assert_eq!(map[&PathBuf::from("/r/src/a")], FileStatus::Added);
        // A deleted child marks the directory modified, not deleted.
        assert_eq!(map[&PathBuf::from("/r/src/b")], FileStatus::Modified);
        assert_eq!(map[&PathBuf::from("/r/src/b/gone.rs")], FileStatus::Deleted);
        assert_eq!(map[&PathBuf::from("/r/src")], FileStatus::Modified);
        assert_eq!(map[&PathBuf::from("/r/docs")], FileStatus::Untracked);
        // The work tree root itself is not part of the map.
        assert!(!map.contains_key(&wd));
    }

    #[test]
    fn conflict_beats_everything() {
        let wd = PathBuf::from("/r");
        let map =
            tree_status(&wd, vec![(PathBuf::from("/r/d/a"), FileStatus::Modified), (PathBuf::from("/r/d/b"), FileStatus::Conflicted)]);
        assert_eq!(map[&PathBuf::from("/r/d")], FileStatus::Conflicted);
    }

    #[test]
    fn label_prefers_branch_then_short_sha() {
        let mut s = StatusSnapshot { branch: Some("main".into()), head: Some("0123456789abcdef".into()), ..Default::default() };
        assert_eq!(s.label().as_deref(), Some("main"));
        s.branch = None;
        assert_eq!(s.label().as_deref(), Some("01234567"));
    }

    #[test]
    fn branch_ops_depend_on_the_branch() {
        let b = |name: &str, current, remote| BranchInfo { name: name.into(), upstream: None, ahead: 0, behind: 0, current, remote };
        let ops = |b: &BranchInfo| BranchOp::for_branch(b, Some("main")).into_iter().map(|(op, _)| op).collect::<Vec<_>>();
        use BranchOp::*;
        assert_eq!(ops(&b("dev", false, false)), [Checkout, NewFrom, Merge, Rebase, Rename, Delete]);
        assert_eq!(ops(&b("main", true, false)), [NewFrom, Rename]);
        assert_eq!(ops(&b("origin/dev", false, true)), [CheckoutTracking, NewFrom, Merge, Rebase]);
        let labels = BranchOp::for_branch(&b("origin/dev", false, true), Some("main"));
        assert_eq!(labels[0].1, "Checkout as new local branch 'dev'");
        assert_eq!(labels[2].1, "Merge 'origin/dev' into 'main'");
    }
}
