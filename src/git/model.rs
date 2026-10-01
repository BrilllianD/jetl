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
}

/// One line of `git log --graph`: the graph prefix plus the commit when the
/// line carries one (connector lines carry only graph characters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub graph: String,
    pub commit: Option<LogCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCommit {
    pub sha: String,
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
    /// (status letter, path) from `git diff-tree --name-status`.
    pub files: Vec<(char, String)>,
}

/// What the diff view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKey {
    /// A working tree change: Staged = HEAD vs index, Unstaged = index vs
    /// file, Untracked = nothing vs file.
    Change { section: Section, rel: String },
    /// A file in a commit: `sha^:path` vs `sha:path`.
    Commit { sha: String, rel: String },
}

impl DiffKey {
    pub fn rel(&self) -> &str {
        match self {
            DiffKey::Change { rel, .. } | DiffKey::Commit { rel, .. } => rel,
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
        let map = tree_status(&wd, vec![(PathBuf::from("/r/d/a"), FileStatus::Modified), (PathBuf::from("/r/d/b"), FileStatus::Conflicted)]);
        assert_eq!(map[&PathBuf::from("/r/d")], FileStatus::Conflicted);
    }

    #[test]
    fn label_prefers_branch_then_short_sha() {
        let mut s = StatusSnapshot { branch: Some("main".into()), head: Some("0123456789abcdef".into()), ..Default::default() };
        assert_eq!(s.label().as_deref(), Some("main"));
        s.branch = None;
        assert_eq!(s.label().as_deref(), Some("01234567"));
    }
}
