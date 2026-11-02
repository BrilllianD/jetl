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
    /// The git operation that stopped halfway, if any.
    pub in_progress: Option<InProgress>,
}

/// A git operation that stopped halfway (`git2::RepositoryState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgressKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    Am,
}

impl InProgressKind {
    pub fn name(self) -> &'static str {
        match self {
            InProgressKind::Merge => "merge",
            InProgressKind::Rebase => "rebase",
            InProgressKind::CherryPick => "cherry-pick",
            InProgressKind::Revert => "revert",
            InProgressKind::Bisect => "bisect",
            InProgressKind::Am => "patch application",
        }
    }

    /// The git subcommand that continues, skips or aborts this operation.
    pub fn command(self) -> &'static str {
        match self {
            InProgressKind::Merge => "merge",
            InProgressKind::Rebase => "rebase",
            InProgressKind::CherryPick => "cherry-pick",
            InProgressKind::Revert => "revert",
            InProgressKind::Bisect => "bisect",
            InProgressKind::Am => "am",
        }
    }

    /// The `git` arguments for `action`; `None` when git has no such step
    /// (`merge --skip`) or jetl does not drive it (bisect, patch application).
    pub fn args(self, action: ProgressAction) -> Option<Vec<String>> {
        use InProgressKind::*;
        match (self, action) {
            (Bisect | Am, _) | (Merge, ProgressAction::Skip) => return None,
            _ => {}
        }
        let flag = match action {
            ProgressAction::Continue => "--continue",
            ProgressAction::Skip => "--skip",
            ProgressAction::Abort => "--abort",
        };
        Some(vec![self.command().to_string(), flag.to_string()])
    }

    /// Bisect checks out commits itself, so git allows switching during it.
    pub fn blocks_switching(self) -> bool {
        self != InProgressKind::Bisect
    }
}

/// `git reset` modes, in the dialog's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    pub const ALL: [ResetMode; 3] = [ResetMode::Soft, ResetMode::Mixed, ResetMode::Hard];

    pub fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ResetMode::Soft => "soft",
            ResetMode::Mixed => "mixed",
            ResetMode::Hard => "hard",
        }
    }

    pub fn choice_label(self) -> &'static str {
        match self {
            ResetMode::Soft => "Soft: keep the changes staged",
            ResetMode::Mixed => "Mixed: keep the changes in the files, unstaged",
            ResetMode::Hard => "Hard: discard all uncommitted changes",
        }
    }
}

/// What to do with an operation in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressAction {
    Continue,
    Skip,
    Abort,
}

impl ProgressAction {
    /// The notification after `git <kind> --<action>` succeeded.
    pub fn done(self, kind: InProgressKind) -> String {
        match self {
            ProgressAction::Continue => match kind {
                InProgressKind::Rebase => "Rebase finished".to_string(),
                InProgressKind::Merge => "Merge committed".to_string(),
                InProgressKind::CherryPick => "Cherry-pick committed".to_string(),
                InProgressKind::Revert => "Revert committed".to_string(),
                _ => "Continued".to_string(),
            },
            ProgressAction::Skip => "Skipped the commit".to_string(),
            ProgressAction::Abort => {
                let name = kind.name();
                let mut c = name.chars();
                let first = c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default();
                format!("{first}{} aborted", c.as_str())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InProgress {
    pub kind: InProgressKind,
    /// Rebase only: the commit being applied and the total (1-based).
    pub step: Option<(usize, usize)>,
}

impl InProgress {
    pub fn label(&self) -> String {
        match (self.kind, self.step) {
            (InProgressKind::Rebase, Some((n, total))) => format!("Rebasing {n}/{total}"),
            (InProgressKind::Rebase, None) => "Rebasing".to_string(),
            (InProgressKind::Merge, _) => "Merging".to_string(),
            (InProgressKind::CherryPick, _) => "Cherry-picking".to_string(),
            (InProgressKind::Revert, _) => "Reverting".to_string(),
            (InProgressKind::Bisect, _) => "Bisecting".to_string(),
            (InProgressKind::Am, _) => "Applying patches".to_string(),
        }
    }

    pub fn refusal(&self) -> String {
        format!("A {} is in progress: finish or abort it first", self.kind.name())
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BranchOp {
    Checkout,
    /// A remote branch: a local branch of the same name tracking it.
    CheckoutTracking,
    NewFrom,
    /// The Log tab filtered to the commits of the branch that HEAD lacks.
    Compare,
    Merge,
    Rebase,
    /// Push the current branch.
    Push,
    Rename,
    Delete,
}

impl BranchOp {
    /// Whether git refuses this while an operation is in progress.
    pub fn refused_mid_operation(self) -> bool {
        matches!(self, BranchOp::Checkout | BranchOp::CheckoutTracking | BranchOp::NewFrom | BranchOp::Merge | BranchOp::Rebase)
    }

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
            v.push((BranchOp::Compare, format!("Compare with '{cur}'")));
            v.push((BranchOp::Merge, format!("Merge '{name}' into '{cur}'…")));
            v.push((BranchOp::Rebase, format!("Rebase '{cur}' onto '{name}'…")));
        } else if !b.remote {
            v.push((BranchOp::Push, "Push".to_string()));
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

/// One action on a node of the Log details' file tree (a context menu row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogFileOp {
    /// Show the file's diff in the Log diff pane.
    ShowDiff,
    /// Open the working-tree file.
    OpenInEditor,
    /// Copy the absolute path.
    CopyPath,
    /// The Log filtered to the node's path.
    ShowHistory,
    /// Open every folder.
    ExpandAll,
    /// Close every folder.
    CollapseAll,
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
    /// The commit message's first line, empty when unreadable.
    pub summary: String,
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
    /// Branches, remote branches, tags and HEAD (the default `[git] log_all = true`), or
    /// HEAD only with `log_all = false`.
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

/// One piece of a combined diff: a first-parent chain of selected commits,
/// diffed from the parent of its oldest commit (`None`: the empty tree) to
/// its newest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSpan {
    pub base: Option<String>,
    pub top: String,
    pub header: String,
}

fn short8(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// The tab title and diff spans of `commits` (log order, newest first). A
/// commit and the next listed one join a chain when the next is the first
/// commit's first parent; unselected commits are never spanned.
pub fn commits_diff(commits: &[&LogCommit]) -> (String, Vec<DiffSpan>) {
    let mut chains: Vec<Vec<&LogCommit>> = Vec::new();
    for c in commits {
        match chains.last_mut() {
            Some(chain) if chain.last().and_then(|l| l.parents.first()) == Some(&c.sha) => chain.push(c),
            _ => chains.push(vec![c]),
        }
    }
    let spans: Vec<DiffSpan> = chains
        .iter()
        .map(|chain| {
            let (top, oldest) = (chain[0], chain[chain.len() - 1]);
            let header = if chain.len() == 1 {
                format!("== {} {}", short8(&top.sha), top.subject)
            } else {
                format!("== {}..{} ({} commits)", short8(&oldest.sha), short8(&top.sha), chain.len())
            };
            DiffSpan { base: oldest.parents.first().cloned(), top: top.sha.clone(), header }
        })
        .collect();
    let title = match chains.as_slice() {
        [chain] if chain.len() == 1 => format!("Diff {}", short8(&chain[0].sha)),
        [chain] => format!("Diff {}..{}", short8(&chain[chain.len() - 1].sha), short8(&chain[0].sha)),
        _ => format!("Diff of {} commits ({}…)", commits.len(), short8(&spans[0].top)),
    };
    (title, spans)
}

/// Running state of one file across the spans of `union_files`.
struct Acc {
    first: CommitFile,
    last: CommitFile,
    base: Option<String>,
    top: String,
    base_path: String,
    touches: usize,
}

/// The files changed by `spans` (newest first, as `commits_diff` returns
/// them) with `per_span` their `--name-status` rows. A file is listed once,
/// by its newest path, sorted by path. Its key spans from the base of the
/// oldest span touching it to the top of the newest one. Status: `D` when
/// the newest touching span deleted it, else `A` when the oldest added it,
/// else `R` when the base path differs, else `M`; a file touched by one
/// span keeps git's row.
pub fn union_files(spans: &[DiffSpan], per_span: &[Vec<CommitFile>]) -> Vec<(CommitFile, DiffKey)> {
    let mut map: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();
    for (span, files) in spans.iter().zip(per_span).rev() {
        let mut staged = Vec::new();
        for f in files {
            let from = match (&f.old, f.status) {
                (Some(old), 'R') => old.clone(),
                _ => f.path.clone(),
            };
            staged.push((f, map.remove(&from), from));
        }
        for (f, prev, from) in staged {
            let acc = match prev {
                Some(mut a) => {
                    a.last = f.clone();
                    a.top = span.top.clone();
                    a.touches += 1;
                    a
                }
                None => {
                    Acc { first: f.clone(), last: f.clone(), base: span.base.clone(), top: span.top.clone(), base_path: from, touches: 1 }
                }
            };
            map.insert(f.path.clone(), acc);
        }
    }
    map.into_iter()
        .map(|(path, a)| {
            let file = if a.touches == 1 {
                a.first
            } else {
                let status = if a.last.status == 'D' {
                    'D'
                } else if a.first.status == 'A' {
                    'A'
                } else if a.base_path != path {
                    'R'
                } else {
                    'M'
                };
                let old = (status != 'A' && a.base_path != path).then(|| a.base_path.clone());
                CommitFile { status, path: path.clone(), old }
            };
            let key = DiffKey::Range { base: a.base, top: a.top, rel: path, old: file.old.clone() };
            (file, key)
        })
        .collect()
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
    /// A local history snapshot (`history::store`, blob `id` taken at `at`,
    /// Unix ms) vs the file's current text (its open buffer, else the
    /// disk). `rel` is the path as shown: relative to the root when inside
    /// it. Loaded by `App::load_local_diff`, not the git service.
    Local { path: PathBuf, rel: String, id: String, at: u64 },
    /// `base:old_or_rel` vs `top:rel`, `base: None` the empty tree: the
    /// combined change of several commits; see `union_files`.
    Range { base: Option<String>, top: String, rel: String, old: Option<String> },
}

impl DiffKey {
    pub fn rel(&self) -> &str {
        match self {
            DiffKey::Change { rel, .. } | DiffKey::Commit { rel, .. } | DiffKey::Local { rel, .. } | DiffKey::Range { rel, .. } => rel,
        }
    }

    /// `rel`, or `old -> new` for a rename.
    pub fn label(&self) -> String {
        match self {
            DiffKey::Change { rel, old: Some(old), .. }
            | DiffKey::Commit { rel, old: Some(old), .. }
            | DiffKey::Range { rel, old: Some(old), .. } => format!("{old} -> {rel}"),
            _ => self.rel().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_args_per_kind() {
        use InProgressKind::*;
        use ProgressAction::*;
        let v = |a: &[&str]| Some(a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(Rebase.args(Continue), v(&["rebase", "--continue"]));
        assert_eq!(Merge.args(Skip), None);
        assert_eq!(Merge.args(Abort), v(&["merge", "--abort"]));
        assert_eq!(CherryPick.args(Skip), v(&["cherry-pick", "--skip"]));
        assert_eq!(Revert.args(Continue), v(&["revert", "--continue"]));
        assert_eq!(Bisect.args(Continue), None);
        assert_eq!(Am.args(Abort), None);
        assert_eq!(Abort.done(CherryPick), "Cherry-pick aborted");
    }

    #[test]
    fn in_progress_labels() {
        let rb = |step| InProgress { kind: InProgressKind::Rebase, step };
        assert_eq!(rb(Some((2, 5))).label(), "Rebasing 2/5");
        assert_eq!(rb(None).label(), "Rebasing");
        let m = InProgress { kind: InProgressKind::Merge, step: None };
        assert_eq!(m.label(), "Merging");
        assert_eq!(m.refusal(), "A merge is in progress: finish or abort it first");
        assert_eq!(InProgress { kind: InProgressKind::CherryPick, step: None }.label(), "Cherry-picking");
    }

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
        assert_eq!(ops(&b("dev", false, false)), [Checkout, NewFrom, Compare, Merge, Rebase, Rename, Delete]);
        assert_eq!(ops(&b("main", true, false)), [NewFrom, Push, Rename]);
        assert_eq!(ops(&b("origin/dev", false, true)), [CheckoutTracking, NewFrom, Compare, Merge, Rebase]);
        let labels = BranchOp::for_branch(&b("origin/dev", false, true), Some("main"));
        assert_eq!(labels[0].1, "Checkout as new local branch 'dev'");
        assert_eq!(labels[3].1, "Merge 'origin/dev' into 'main'…");
    }

    fn lc(sha: &str, parents: &[&str]) -> LogCommit {
        LogCommit {
            sha: sha.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            short: sha.into(),
            author: String::new(),
            date: String::new(),
            refs: String::new(),
            subject: format!("subject {sha}"),
        }
    }

    #[test]
    fn diff_of_one_commit() {
        let c = lc("aaaaaaaaaaaa", &["bbbbbbbbbbbb"]);
        let (title, spans) = commits_diff(&[&c]);
        assert_eq!(title, "Diff aaaaaaaa");
        assert_eq!(
            spans,
            [DiffSpan { base: Some("bbbbbbbbbbbb".into()), top: "aaaaaaaaaaaa".into(), header: "== aaaaaaaa subject aaaaaaaaaaaa".into() }]
        );
    }

    #[test]
    fn diff_of_a_first_parent_chain_is_one_span() {
        let (a, b, c) = (lc("aaaaaaaa1", &["bbbbbbbb2"]), lc("bbbbbbbb2", &["cccccccc3"]), lc("cccccccc3", &["dddddddd4"]));
        let (title, spans) = commits_diff(&[&a, &b, &c]);
        assert_eq!(title, "Diff cccccccc..aaaaaaaa");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].base.as_deref(), Some("dddddddd4"));
        assert_eq!(spans[0].top, "aaaaaaaa1");
        assert_eq!(spans[0].header, "== cccccccc..aaaaaaaa (3 commits)");
    }

    #[test]
    fn diff_with_a_gap_has_two_spans() {
        let (a, b, c) = (lc("aaaaaaaa1", &["bbbbbbbb2"]), lc("bbbbbbbb2", &["cccccccc3"]), lc("cccccccc3", &["dddddddd4"]));
        let (title, spans) = commits_diff(&[&a, &c]);
        assert_eq!(title, "Diff of 2 commits (aaaaaaaa…)");
        assert_eq!(spans.iter().map(|s| s.top.as_str()).collect::<Vec<_>>(), ["aaaaaaaa1", "cccccccc3"]);
        assert_eq!(spans[0].base.as_deref(), Some("bbbbbbbb2"));
        let _ = b;
    }

    #[test]
    fn diff_of_a_root_commit_has_no_base() {
        let r = lc("rrrrrrrr1", &[]);
        assert_eq!(commits_diff(&[&r]).1[0].base, None);
    }

    #[test]
    fn diff_of_a_merge_and_its_second_parent_has_two_spans() {
        let (m, side) = (lc("mmmmmmmm1", &["pppppppp2", "ssssssss3"]), lc("ssssssss3", &["pppppppp2"]));
        let (title, spans) = commits_diff(&[&m, &side]);
        assert_eq!(title, "Diff of 2 commits (mmmmmmmm…)");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].base.as_deref(), Some("pppppppp2"));
    }

    fn span(base: Option<&str>, top: &str) -> DiffSpan {
        DiffSpan { base: base.map(str::to_string), top: top.to_string(), header: String::new() }
    }

    fn cf(status: char, path: &str, old: Option<&str>) -> CommitFile {
        CommitFile { status, path: path.to_string(), old: old.map(str::to_string) }
    }

    fn range(base: Option<&str>, top: &str, rel: &str, old: Option<&str>) -> DiffKey {
        DiffKey::Range { base: base.map(str::to_string), top: top.to_string(), rel: rel.to_string(), old: old.map(str::to_string) }
    }

    #[test]
    fn union_of_one_span_keeps_git_rows() {
        let rows = vec![cf('M', "a", None), cf('C', "c", Some("a")), cf('R', "z", Some("y"))];
        let got = union_files(&[span(Some("b"), "t")], std::slice::from_ref(&rows));
        assert_eq!(got.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(), vec![rows[0].clone(), rows[1].clone(), rows[2].clone()]);
        assert_eq!(got[0].1, range(Some("b"), "t", "a", None));
        assert_eq!(got[1].1, range(Some("b"), "t", "c", Some("a")));
        assert_eq!(got[2].1, range(Some("b"), "t", "z", Some("y")));
    }

    #[test]
    fn union_of_two_spans_uses_the_widest_range_per_file() {
        let spans = [span(Some("n0"), "n1"), span(Some("o0"), "o1")];
        let per = [vec![cf('M', "both", None), cf('M', "new", None)], vec![cf('M', "both", None)]];
        let got = union_files(&spans, &per);
        assert_eq!(got[0].1, range(Some("o0"), "n1", "both", None));
        assert_eq!(got[1].1, range(Some("n0"), "n1", "new", None));
        assert_eq!(got[0].0.status, 'M');
    }

    #[test]
    fn union_add_then_modify_is_added() {
        let spans = [span(Some("n0"), "n1"), span(None, "o1")];
        let per = [vec![cf('M', "f", None)], vec![cf('A', "f", None)]];
        let got = union_files(&spans, &per);
        assert_eq!(got[0].0, cf('A', "f", None));
        assert_eq!(got[0].1, range(None, "n1", "f", None));
    }

    #[test]
    fn union_modify_then_delete_is_deleted() {
        let spans = [span(Some("n0"), "n1"), span(Some("o0"), "o1")];
        let per = [vec![cf('D', "f", None)], vec![cf('M', "f", None)]];
        assert_eq!(union_files(&spans, &per)[0].0, cf('D', "f", None));
    }

    #[test]
    fn union_delete_then_readd_is_modified() {
        let spans = [span(Some("n0"), "n1"), span(Some("o0"), "o1")];
        let per = [vec![cf('A', "f", None)], vec![cf('D', "f", None)]];
        assert_eq!(union_files(&spans, &per)[0].0, cf('M', "f", None));
    }

    #[test]
    fn union_rename_then_modify_is_one_renamed_row() {
        let spans = [span(Some("n0"), "n1"), span(Some("o0"), "o1")];
        let per = [vec![cf('M', "b", None)], vec![cf('R', "b", Some("a"))]];
        let got = union_files(&spans, &per);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, cf('R', "b", Some("a")));
        assert_eq!(got[0].1, range(Some("o0"), "n1", "b", Some("a")));
    }

    #[test]
    fn union_keeps_the_empty_tree_base_and_sorts_by_path() {
        let got = union_files(&[span(None, "t")], &[vec![cf('A', "z", None), cf('A', "a/x", None), cf('A', "m", None)]]);
        assert_eq!(got.iter().map(|(f, _)| f.path.as_str()).collect::<Vec<_>>(), ["a/x", "m", "z"]);
        assert!(matches!(&got[0].1, DiffKey::Range { base: None, .. }));
    }
}
