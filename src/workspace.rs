//! The project root the IDE is currently showing, switching it (worktrees),
//! and per-root sessions: open tabs in order with their pins, caret and
//! scroll, the split editor's groups, tool window layout and expanded tree
//! folders, stored as JSON in `paths.sessions_dir()/<fnv hash of root>.json`.
//! The editor sets of other git branches live in the same file (`BranchSet`),
//! and the running app rewrites the file every few seconds when it changed
//! (`App::session_tick`). The editor set follows the checked-out branch:
//! `Session::follow_branch` at load, `App::sync_branch_set` while running.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::editor::buffer::BreakpointSpec;
use crate::editor::split::{Axis, SplitNode};
use crate::event::{AppMsg, Level};
use crate::git::panel::GitTab;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::{Dock, PanelId, Side, ToolWindowId};

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    /// Bumped on every root switch. Background work tags its result with
    /// the generation it started under; the app drops results whose tag is
    /// no longer current.
    pub generation: u64,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root, generation: 1 }
    }

    pub fn name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.root.display().to_string())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Set the root and bump the generation; returns the previous root. The
    /// app does the rest of a switch in [`App::switch_root`].
    pub fn switch_root(&mut self, root: PathBuf) -> PathBuf {
        self.generation += 1;
        std::mem::replace(&mut self.root, root)
    }
}

/// Open tabs, layout and tree state of one root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub root: PathBuf,
    /// Tabs in display order.
    pub files: Vec<SessionFile>,
    #[serde(default)]
    pub active: Option<usize>,
    /// Indices into `files`, most recently used first.
    #[serde(default)]
    pub recent: Vec<usize>,
    /// `None` in sessions saved before it existed: the layout stays as is.
    #[serde(default)]
    pub layout: Option<SessionLayout>,
    /// Expanded tree directories relative to the root, parents first.
    #[serde(default)]
    pub expanded: Vec<PathBuf>,
    /// The split editor: written only with more than one tab group. `files`
    /// still lists every tab once, so sessions without it load as one group.
    #[serde(default)]
    pub splits: Option<SessionSplits>,
    /// Numbered bookmarks of files without a tab.
    #[serde(default)]
    pub closed_bookmarks: Vec<SessionBookmark>,
    /// Bookmarks without a digit of files without a tab, lettered ones included (a field of its own, so
    /// sessions stay readable by builds that know only numbered ones, which keep a letter as a plain bookmark).
    #[serde(default)]
    pub closed_plain_bookmarks: Vec<SessionClosedFile>,
    /// The lettered bookmarks of files without a tab; their lines are also in `closed_plain_bookmarks`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_bookmark_letters: Vec<SessionLetterBookmark>,
    /// The group of the closed bookmarks outside the default group, digit, letter or plain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_bookmark_groups: Vec<SessionGroupBookmark>,
    /// Breakpoint lines of files without a tab, with their specs in `specs`; never their state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_breakpoints: Vec<SessionClosedFile>,
    /// The debugger's watch expressions, in the order they were added; per root, never per branch set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watches: Vec<String>,
    /// Each debug adapter's exception filters as it last declared them, by adapter name, with the user's choices; per root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exception_filters: BTreeMap<String, Vec<SavedFilter>>,
    /// The Run window's selected configuration, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_config: Option<String>,
    /// The branch whose editor set the top-level `files`, `active`, `recent`
    /// and `splits` hold; `None` before git reported one or outside git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The editor sets of the other branches, by short branch name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, BranchSet>,
}

/// One of a debug adapter's exception breakpoint filters as it last declared them, with the user's choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilter {
    pub filter: String,
    pub label: String,
    pub on: bool,
}

/// One branch's editor set: the tabs with their state, the active and
/// recent ones, and the split groups. Layout, tree and bookmarks of
/// closed files stay per root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BranchSet {
    pub files: Vec<SessionFile>,
    pub active: Option<usize>,
    pub recent: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub splits: Option<SessionSplits>,
}

/// How often the running app saves the session when it changed.
pub(crate) const SESSION_SAVE_EVERY: Duration = Duration::from_secs(5);

/// Orders session writes: background saves and the synchronous one on
/// quit and root switch. Holds the sequence number of the last write; an
/// older one arriving late is dropped.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionWriter {
    last: Arc<Mutex<u64>>,
    /// Set when a write failed; the next tick writes again.
    failed: Arc<AtomicBool>,
}

impl SessionWriter {
    /// Write `session` unless a save with a higher `seq` already landed;
    /// true when it wrote.
    pub(crate) fn write(&self, seq: u64, session: &Session, dir: &Path) -> Result<bool> {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if seq <= *last {
            return Ok(false);
        }
        if let Err(e) = session.save(dir) {
            self.failed.store(true, Ordering::Relaxed);
            return Err(e);
        }
        *last = seq;
        Ok(true)
    }

    /// True once after a failed write.
    fn take_failed(&self) -> bool {
        self.failed.swap(false, Ordering::Relaxed)
    }
}

/// The periodic session save (`App::session_tick`).
#[derive(Debug, Default)]
pub(crate) struct SessionSave {
    /// The generation whose session `session_loaded` applied; saving
    /// waits for it so an empty editor never overwrites the file.
    loaded: Option<u64>,
    /// When the next check is due.
    due: Option<Instant>,
    /// The last session handed to the writer.
    last: Option<Session>,
    /// Sequence numbers for `SessionWriter`.
    seq: u64,
    writer: SessionWriter,
}

/// The branch swap in flight (`App::sync_branch_set`).
#[derive(Debug, Default)]
pub(crate) struct BranchSwap {
    /// The last swap number handed out.
    seq: u64,
    /// The swap whose files are being read and the set it opens. Until it
    /// lands this set counts as the current one: the session saves it and a
    /// further switch stores it for the branch left.
    pub(crate) pending: Option<(u64, BranchSet)>,
}

/// The plain bookmarks or the breakpoints of a file without a tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionClosedFile {
    pub path: PathBuf,
    /// Bookmarked or breakpoint lines, 0-based.
    pub lines: Vec<usize>,
    /// The spec (condition, log message, hit count, disabled) of some of the breakpoint `lines`; always empty for
    /// bookmarks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<SessionBreakpointSpec>,
}

/// The spec of the breakpoint on a line; a key is left out when it holds its default (no text, enabled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionBreakpointSpec {
    /// 0-based.
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
}

impl Default for SessionBreakpointSpec {
    /// Written by hand: a derived `Default` would make a spec without an `enabled` key disabled.
    fn default() -> Self {
        SessionBreakpointSpec { line: 0, condition: None, log_message: None, hit_condition: None, enabled: true }
    }
}

fn is_true(b: &bool) -> bool {
    *b
}

/// The non-plain specs among `marks`, in the order given (lines ascending).
fn session_specs<'a>(marks: impl Iterator<Item = (usize, &'a BreakpointSpec)>) -> Vec<SessionBreakpointSpec> {
    marks
        .filter(|(_, s)| !s.is_plain())
        .map(|(line, s)| SessionBreakpointSpec {
            line,
            condition: s.condition.clone(),
            log_message: s.log_message.clone(),
            hit_condition: s.hit_condition.clone(),
            enabled: s.enabled,
        })
        .collect()
}

/// The specs of a session file as the editor holds them: a field with a control character is dropped, the texts are
/// trimmed, and a spec left plain is dropped.
fn loaded_specs(specs: &[SessionBreakpointSpec]) -> Vec<(usize, BreakpointSpec)> {
    let clean = |text: &Option<String>, line: usize, what: &str| {
        let text = text.as_deref()?;
        if text.chars().any(char::is_control) {
            tracing::info!("session: dropping the {what} of the breakpoint on line {} (a control character)", line + 1);
            return None;
        }
        BreakpointSpec::text(text)
    };
    specs
        .iter()
        .map(|s| {
            (
                s.line,
                BreakpointSpec {
                    condition: clean(&s.condition, s.line, "condition"),
                    log_message: clean(&s.log_message, s.line, "log message"),
                    hit_condition: clean(&s.hit_condition, s.line, "hit count"),
                    enabled: s.enabled,
                },
            )
        })
        .filter(|(_, spec)| !spec.is_plain())
        .collect()
}

/// A numbered bookmark of a file without a tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionBookmark {
    pub digit: u8,
    pub path: PathBuf,
    /// 0-based.
    pub line: usize,
}

/// A lettered bookmark of a file without a tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLetterBookmark {
    pub letter: char,
    pub path: PathBuf,
    /// 0-based.
    pub line: usize,
}

/// A bookmark of a file without a tab that is in a named group.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionGroupBookmark {
    pub group: String,
    pub path: PathBuf,
    /// 0-based.
    pub line: usize,
}

/// The bookmarks of closed files a session lists, merged per (path, line): plain first, then digits, then
/// letters; a group entry sets the group of a bookmark that exists and never makes one.
fn closed_bookmarks_of(session: &Session) -> Vec<(PathBuf, usize, crate::editor::Bookmark)> {
    use crate::editor::{Bookmark, Mnemonic, group_name};
    let mut merged: std::collections::BTreeMap<(PathBuf, usize), Bookmark> = Default::default();
    let mut put = |path: &Path, line: usize, bookmark: Bookmark| match merged.entry((path.to_path_buf(), line)) {
        std::collections::btree_map::Entry::Vacant(e) => {
            e.insert(bookmark);
        }
        std::collections::btree_map::Entry::Occupied(mut e) => e.get_mut().merge(bookmark),
    };
    for f in &session.closed_plain_bookmarks {
        for &line in &f.lines {
            put(&f.path, line, Bookmark::default());
        }
    }
    for b in &session.closed_bookmarks {
        if let Some(m) = Mnemonic::digit(b.digit) {
            put(&b.path, b.line, Bookmark { mnemonic: Some(m), group: None });
        }
    }
    for b in &session.closed_bookmark_letters {
        if let Some(m) = Mnemonic::letter(b.letter) {
            put(&b.path, b.line, Bookmark { mnemonic: Some(m), group: None });
        }
    }
    for g in &session.closed_bookmark_groups {
        if let (Some(b), Ok(group)) = (merged.get_mut(&(g.path.clone(), g.line)), group_name(&g.group)) {
            b.group = group;
        }
    }
    merged.into_iter().map(|((path, line), b)| (path, line, b)).collect()
}

/// The editor's split tree and the focused group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSplits {
    pub tree: SessionNode,
    /// The focused group's place among the leaves, in layout order.
    pub focused: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionNode {
    Leaf(SessionGroup),
    Split { axis: Axis, ratio: u16, a: Box<SessionNode>, b: Box<SessionNode> },
}

/// One tab group's tabs in order and the shown one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGroup {
    pub views: Vec<SessionView>,
    pub active: Option<usize>,
}

/// A tab of a group: an index into `Session::files` and its own caret and
/// scroll.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionView {
    pub file: usize,
    pub line: usize,
    pub col: usize,
    pub scroll_line: usize,
    pub scroll_col: usize,
    pub pinned: bool,
    /// The view's collapsed folds.
    pub folds: Vec<SessionFold>,
    /// Soft wrap when toggled away from the configured default; `None`
    /// follows the config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap: Option<bool>,
}

/// Tool windows, their sizes, the Git tab and the tree's visibility toggles.
/// `files.show_hidden` and `files.show_ignored` set the toggles at startup;
/// a saved layout overrides them, a root without one keeps the current ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLayout {
    /// Tool window open on each side.
    pub left: Option<ToolWindowId>,
    pub left_w: u16,
    pub bottom: Option<ToolWindowId>,
    pub bottom_h: u16,
    pub git_tab: GitTab,
    pub show_hidden: bool,
    pub show_ignored: bool,
    /// Right-side window (none before side choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<ToolWindowId>,
    /// Its width; missing keeps the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_w: Option<u16>,
    /// Windows moved off their default side, in strip order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sides: Vec<(ToolWindowId, Side)>,
    /// The Tasks window (none before it existed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<SessionTasks>,
    /// Tool strip order when it is not the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "known_tool_windows")]
    pub order: Vec<ToolWindowId>,
    /// Unpinned tool windows (all are Pinned before 0.9).
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "known_tool_windows")]
    pub unpinned: Vec<ToolWindowId>,
    /// Windows in their side's second pane; older builds put them on the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "known_tool_windows")]
    pub split: Vec<ToolWindowId>,
    /// Left Bottom window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left2: Option<ToolWindowId>,
    /// Bottom Right window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom2: Option<ToolWindowId>,
    /// Right Bottom window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right2: Option<ToolWindowId>,
    /// Rows of the Left Bottom pane; missing: half the side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left2_h: Option<u16>,
    /// Columns of the Bottom Right pane; missing: half the side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom2_w: Option<u16>,
    /// Rows of the Right Bottom pane; missing: half the side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right2_h: Option<u16>,
    /// Undocked (slide-over) tool windows; older builds dock them Pinned.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "known_tool_windows")]
    pub undocked: Vec<ToolWindowId>,
}

/// Tool windows kept out of every saved slot, side and split field (sessions and
/// named layouts): Tasks has its own session field, TODO and Hierarchy are rebuilt on demand.
pub(crate) const NOT_PLACED: [ToolWindowId; 3] = [ToolWindowId::Tasks, ToolWindowId::Todo, ToolWindowId::Hierarchy];

impl SessionLayout {
    /// The placement part (what a named layout stores).
    pub fn placement(&self) -> crate::layouts::LayoutPlacement {
        crate::layouts::LayoutPlacement {
            left: self.left,
            left_w: self.left_w,
            bottom: self.bottom,
            bottom_h: self.bottom_h,
            right: self.right,
            right_w: self.right_w,
            sides: self.sides.clone(),
            order: self.order.clone(),
            unpinned: self.unpinned.clone(),
            split: self.split.clone(),
            left2: self.left2,
            bottom2: self.bottom2,
            right2: self.right2,
            left2_h: self.left2_h,
            bottom2_w: self.bottom2_w,
            right2_h: self.right2_h,
            undocked: self.undocked.clone(),
        }
    }
}

/// A list of tool window ids, without the ones this build does not know
/// (a newer build's windows), so they do not drop the whole session.
pub(crate) fn known_tool_windows<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ToolWindowId>, D::Error> {
    let raw = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(raw.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
}

/// The Tasks window: where it sits, whether it is shown, its filters and
/// selected task. Kept out of `left`, `bottom`, `right` and `sides` so
/// builds without the Tasks window still read the layout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTasks {
    pub shown: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,
    /// In its side's second pane.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub split: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub show_closed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionFile {
    pub path: PathBuf,
    /// Caret line and character column, 0-based.
    pub line: usize,
    pub col: usize,
    pub scroll_line: usize,
    pub scroll_col: usize,
    /// Bookmarked lines, 0-based.
    #[serde(default)]
    pub bookmarks: Vec<usize>,
    /// The numbered ones among `bookmarks`: (digit, line). Sessions saved
    /// before numbered bookmarks have none.
    #[serde(default)]
    pub bookmark_digits: Vec<(u8, usize)>,
    /// The lettered ones among `bookmarks`: (letter, line).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmark_letters: Vec<(char, usize)>,
    /// The ones among `bookmarks` outside the default group: (group, line).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmark_groups: Vec<(String, usize)>,
    /// Lines with a breakpoint, 0-based; never their state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakpoints: Vec<usize>,
    /// The condition or log message of some of the `breakpoints`; plain ones are not listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakpoint_specs: Vec<SessionBreakpointSpec>,
    /// Pinned tabs come first in `files`.
    #[serde(default)]
    pub pinned: bool,
    /// The collapsed folds of the view the file was shown in.
    #[serde(default)]
    pub folds: Vec<SessionFold>,
    /// Soft wrap when toggled away from the configured default; `None`
    /// follows the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<bool>,
}

/// A collapsed fold of a view: its anchors as 0-based lines and character
/// columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFold {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

/// 64-bit FNV-1a; stable across runs and platforms, unlike `DefaultHasher`.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl Session {
    /// The top-level editor fields as a set.
    pub fn editor_set(&self) -> BranchSet {
        BranchSet { files: self.files.clone(), active: self.active, recent: self.recent.clone(), splits: self.splits.clone() }
    }

    /// Replace the top-level editor fields by `set`.
    pub fn set_editor_set(&mut self, set: BranchSet) {
        let BranchSet { files, active, recent, splits } = set;
        (self.files, self.active, self.recent, self.splits) = (files, active, recent, splits);
    }

    /// Make the top-level editor set the one of `current`, the branch
    /// checked out now (`None`, detached HEAD: the set stays and so does
    /// `branch`). The set of the recorded branch moves into `branches`; a
    /// branch without a stored set keeps the current one; a session without
    /// a recorded branch adopts `current`. Then stored sets of branches not
    /// in `local` are dropped (`None`: the listing failed, nothing is dropped).
    pub fn follow_branch(&mut self, current: Option<String>, local: Option<&[String]>) {
        if let Some(now) = current
            && let Some(old) = self.branch.replace(now.clone())
            && old != now
        {
            let outgoing = self.editor_set();
            self.branches.insert(old, outgoing);
            if let Some(set) = self.branches.remove(&now) {
                self.set_editor_set(set);
            }
        }
        if let Some(local) = local {
            self.branches.retain(|k, _| local.contains(k));
        }
        if let Some(b) = &self.branch {
            self.branches.remove(b);
        }
    }

    pub fn file_for(sessions_dir: &Path, root: &Path) -> PathBuf {
        sessions_dir.join(format!("{:016x}.json", fnv1a64(root.as_os_str().as_encoded_bytes())))
    }

    /// The saved session of `root`. A missing file is silent; unreadable or
    /// invalid JSON is logged and ignored.
    pub fn load(sessions_dir: &Path, root: &Path) -> Option<Session> {
        let file = Self::file_for(sessions_dir, root);
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("session {}: {e}; ignoring", file.display());
                return None;
            }
        };
        match serde_json::from_str::<Session>(&text) {
            Ok(s) if s.root == root => Some(s),
            Ok(s) => {
                tracing::warn!("session {} belongs to {}; ignoring", file.display(), s.root.display());
                None
            }
            Err(e) => {
                tracing::warn!("session {}: invalid JSON ({e}); ignoring", file.display());
                None
            }
        }
    }

    /// Write atomically (temp file, then rename).
    pub fn save(&self, sessions_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(sessions_dir).with_context(|| format!("create {}", sessions_dir.display()))?;
        let file = Self::file_for(sessions_dir, &self.root);
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &file).with_context(|| format!("rename {}", tmp.display()))?;
        Ok(())
    }
}

impl App {
    /// `Effect::SwitchRoot`: make `path` the project root. Unsaved buffers
    /// are asked about first (`DialogId::SwitchRootDirty`).
    pub fn switch_root(&mut self, path: PathBuf) {
        let root = match std::fs::canonicalize(&path) {
            Ok(p) if p.is_dir() => p,
            Ok(p) => return self.notify(Level::Error, format!("{} is not a directory", p.display())),
            Err(e) => return self.notify(Level::Error, format!("cannot open {}: {e}", path.display())),
        };
        if root == self.workspace.root {
            return self.notify(Level::Info, format!("Already in {}", root.display()));
        }
        if self.editor.has_dirty() {
            let msg = format!("Some files have unsaved changes.\nSave them before switching to {}?", root.display());
            self.dialog =
                Some(Dialog::confirm(DialogId::SwitchRootDirty(root), "Unsaved changes", &msg, &["Save all", "Discard", "Cancel"]));
            return;
        }
        self.do_switch_root(root);
    }

    /// Switch without asking: save the old session, close tabs (per
    /// `worktree.switch_closes_tabs`), point every service at the new root,
    /// restore its session and record it as a recent project.
    pub fn do_switch_root(&mut self, root: PathBuf) {
        self.save_session();
        // The old root's swap must not be saved as the new root's set.
        self.branch_swap.pending = None;
        // Nor are its branch sets.
        self.branch_sets.clear();
        self.session_branch = None;
        // The resolver's work is not kept across roots.
        self.editor.merge_close();
        self.format.abort_all();
        self.run_root_changed();
        self.debug_root_changed();
        // The shell belongs to the old root; the next one starts in the new.
        self.terminal.reset();
        // So do the language servers; new ones start for the open buffers.
        self.lsp_root_changed();
        if self.config.worktree.switch_closes_tabs {
            self.editor.force_close_all();
        }
        // Queued label reverts name themselves before the root changes.
        self.pending_reverts_dropped();
        let old = self.workspace.switch_root(root.clone());
        tracing::info!("switch root {} -> {}", old.display(), root.display());
        // Points git at the new root and generation, resets the Git panel,
        // and requests status and branches.
        self.git_root_changed(root.clone());
        self.yman.set_root(root.clone(), self.workspace.generation);
        self.tasks.reset();
        self.start_watcher();
        self.find.reset();
        self.todo.reset();
        self.symbols.reset();
        self.worktrees.reset();
        self.git.load_worktrees();
        self.project.reroot(root.clone());
        // The palette lists the old root; close it and re-index.
        self.close_palette();
        self.refresh_file_index();
        // The old root's index must not answer for the new one.
        self.scip.index = None;
        self.scip.failed_mtime = None;
        self.load_scip();
        self.coverage_restart();
        crate::tui::set_title(&root);
        self.editor.clear_nav();
        self.history_root_changed();
        // The closed files' numbered bookmarks come with the new session.
        self.editor.restore_closed_bookmarks(&[]);
        self.editor.restore_closed_breakpoint_marks(&[]);
        self.restore_session();
        self.load_recent_projects(true);
        self.notify(Level::Info, format!("Switched to {}", root.display()));
    }

    /// Drop unsaved changes by closing the dirty tabs (the "Discard" answer
    /// when tabs survive a switch).
    pub fn close_dirty_tabs(&mut self) {
        for i in (0..self.editor.buffers.len()).rev() {
            let b = &self.editor.buffers[i];
            if b.dirty() && !self.editor.is_merge_buffer(b.id) {
                self.editor.force_close(i);
            }
        }
    }

    /// The open tabs of the current editor as a set, in tab order.
    pub(crate) fn editor_set(&self) -> BranchSet {
        let mut files = Vec::new();
        let mut active = None;
        let pinned = self.editor.pinned_ids();
        for i in self.editor.tab_order() {
            let b = &self.editor.buffers[i];
            let Some(path) = b.path.clone() else { continue };
            if self.editor.session_active() == Some(i) {
                active = Some(files.len());
            }
            let (line, col) = b.cursor_line_col();
            let bookmarks = b.bookmarks.keys().copied().collect();
            files.push(SessionFile {
                path,
                line,
                col,
                scroll_line: b.view.scroll_line,
                scroll_col: b.view.scroll_col,
                bookmarks,
                bookmark_digits: b.digits(),
                bookmark_letters: b.letters(),
                bookmark_groups: b.grouped(),
                breakpoints: b.breakpoints.keys().copied().collect(),
                breakpoint_specs: session_specs(b.breakpoints.iter().map(|(l, m)| (*l, &m.spec))),
                pinned: pinned.contains(&b.id),
                folds: b.view.folds.to_session(&b.rope),
                wrap: (b.view.wrap.on != self.editor.soft_wrap_for(b.path.as_deref())).then_some(b.view.wrap.on),
            });
        }
        let recent = self.editor.recent_paths().iter().filter_map(|p| files.iter().position(|f| &f.path == p)).collect();
        let splits = self.session_splits(&files);
        BranchSet { files, active, recent, splits }
    }

    /// The placement part of the layout as sessions and named layouts save it:
    /// the pre-zen windows, without [`NOT_PLACED`] in slot, side and split fields.
    pub(crate) fn layout_placement(&self) -> crate::layouts::LayoutPlacement {
        // Zen mode is not saved: the windows it hides are.
        let (left, bottom, right) = self.layout.saved_windows();
        let (left2, bottom2, right2) = self.layout.saved_split_windows();
        let saved = |tw: Option<ToolWindowId>| tw.filter(|t| !NOT_PLACED.contains(t));
        let mut sides = self.layout.side_overrides();
        sides.retain(|(tw, _)| !NOT_PLACED.contains(tw));
        let mut split = self.layout.split_windows();
        split.retain(|tw| !NOT_PLACED.contains(tw));
        let size = |v: u16| Some(v).filter(|v| *v != 0);
        crate::layouts::LayoutPlacement {
            left: saved(left),
            left_w: self.layout.left_w,
            bottom: saved(bottom),
            bottom_h: self.layout.bottom_h,
            right: saved(right),
            right_w: Some(self.layout.right_w),
            sides,
            order: self.layout.order_overrides(),
            unpinned: self.layout.unpinned(),
            split,
            left2: saved(left2),
            bottom2: saved(bottom2),
            right2: saved(right2),
            left2_h: size(self.layout.left2_h),
            bottom2_w: size(self.layout.bottom2_w),
            right2_h: size(self.layout.right2_h),
            undocked: self.layout.undocked(),
        }
    }

    /// Apply a placement (session restore, a named layout). Each `kept` window
    /// (one the placement does not hold, see [`NOT_PLACED`]) goes to its dock
    /// and, when its flag says shown, into that dock's slot unless the
    /// placement fills it. Focus on a tool window that ends hidden moves to the editor.
    pub(crate) fn apply_placement(&mut self, p: &crate::layouts::LayoutPlacement, kept: &[(ToolWindowId, Dock, bool)]) {
        let mut sides = p.sides.clone();
        let mut split = p.split.clone();
        for &(tw, dock, _) in kept {
            sides.retain(|(t, _)| *t != tw);
            sides.push((tw, dock.side));
            split.retain(|t| *t != tw);
            if dock.split {
                split.push(tw);
            }
        }
        self.layout.set_sides(&sides);
        self.layout.set_split(&split);
        self.layout.set_order(&p.order);
        self.layout.set_unpinned(&p.unpinned);
        let slot = |tw: Option<ToolWindowId>, side: Side, split: bool| {
            tw.or_else(|| kept.iter().find(|k| k.2 && k.1 == (Dock { side, split })).map(|k| k.0))
        };
        self.layout.restore(
            slot(p.left, Side::Left, false),
            p.left_w,
            slot(p.bottom, Side::Bottom, false),
            p.bottom_h,
            slot(p.right, Side::Right, false),
            p.right_w,
        );
        self.layout.restore_split(
            slot(p.left2, Side::Left, true),
            slot(p.bottom2, Side::Bottom, true),
            slot(p.right2, Side::Right, true),
            p.left2_h,
            p.bottom2_w,
            p.right2_h,
        );
        self.layout.set_undocked(&p.undocked);
        if let Some(tw) = self.focus.current.tool_window()
            && !self.layout.is_visible(tw)
        {
            self.focus.set(PanelId::Editor);
        }
    }

    /// The open tabs of the current root as a session, in tab order.
    pub fn session(&self) -> Session {
        let root = self.workspace.root.clone();
        // A swap in flight: its set is the current one.
        let BranchSet { files, active, recent, splits } =
            self.branch_swap.pending.as_ref().map_or_else(|| self.editor_set(), |(_, s)| s.clone());
        let (show_hidden, show_ignored) = self.project.visibility();
        // Zen mode is not saved: the windows it hides are.
        let (left, bottom, right) = self.layout.saved_windows();
        // The Tasks window has its own field: builds without it read the rest.
        let (left2, bottom2, right2) = self.layout.saved_split_windows();
        let shown = [left, bottom, right, left2, bottom2, right2].contains(&Some(ToolWindowId::Tasks));
        let side = self.layout.side_overrides().iter().find(|(tw, _)| *tw == ToolWindowId::Tasks).map(|(_, s)| *s);
        let tasks_split = self.layout.dock_of(ToolWindowId::Tasks).split;
        let tasks = SessionTasks { shown, side, split: tasks_split, ..self.tasks.session_state() };
        let p = self.layout_placement();
        let layout = Some(SessionLayout {
            left: p.left,
            left_w: p.left_w,
            bottom: p.bottom,
            bottom_h: p.bottom_h,
            git_tab: self.git_panel.tab,
            show_hidden,
            show_ignored,
            right: p.right,
            right_w: p.right_w,
            sides: p.sides,
            tasks: Some(tasks),
            order: p.order,
            unpinned: p.unpinned,
            split: p.split,
            left2: p.left2,
            bottom2: p.bottom2,
            right2: p.right2,
            left2_h: p.left2_h,
            bottom2_w: p.bottom2_w,
            right2_h: p.right2_h,
            undocked: p.undocked,
        });
        let expanded = self.project.expanded_dirs().iter().filter_map(|d| d.strip_prefix(&root).ok()).map(Path::to_path_buf).collect();
        let closed_bookmarks =
            self.editor.closed_digits().into_iter().map(|(digit, path, line)| SessionBookmark { digit, path, line }).collect();
        let mut closed_plain_bookmarks: Vec<SessionClosedFile> = Vec::new();
        // By path then line, so one entry per file.
        let not_digit = |m: &Option<crate::editor::Mnemonic>| !matches!(m, Some(crate::editor::Mnemonic::Digit(_)));
        for (path, line, _) in self.editor.closed_bookmarks().into_iter().filter(|(_, _, b)| not_digit(&b.mnemonic)) {
            match closed_plain_bookmarks.last_mut() {
                Some(f) if f.path == path => f.lines.push(line),
                _ => closed_plain_bookmarks.push(SessionClosedFile { path, lines: vec![line], specs: vec![] }),
            }
        }
        let closed_bookmark_letters =
            self.editor.closed_letters().into_iter().map(|(letter, path, line)| SessionLetterBookmark { letter, path, line }).collect();
        let closed_bookmark_groups =
            self.editor.closed_groups().into_iter().map(|(group, path, line)| SessionGroupBookmark { group, path, line }).collect();
        Session {
            root,
            files,
            active,
            recent,
            layout,
            expanded,
            splits,
            closed_bookmarks,
            closed_plain_bookmarks,
            closed_bookmark_letters,
            closed_bookmark_groups,
            closed_breakpoints: self
                .editor
                .closed_breakpoint_marks()
                .into_iter()
                .map(|(path, marks)| SessionClosedFile {
                    path,
                    lines: marks.iter().map(|(l, _)| *l).collect(),
                    specs: session_specs(marks.iter().map(|(l, s)| (*l, s))),
                })
                .collect(),
            watches: self.debug_panel.watches().to_vec(),
            exception_filters: self.debug.exception_filters.clone(),
            run_config: self.run.selected.clone(),
            branch: self.session_branch.clone(),
            branches: self
                .branch_sets
                .iter()
                .filter(|(k, _)| Some(*k) != self.session_branch.as_ref())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// The editor's groups as a session, `None` with one group.
    fn session_splits(&self, files: &[SessionFile]) -> Option<SessionSplits> {
        let ed = &self.editor;
        let (groups, tree, focused) = ed.session_layout();
        let leaves = tree.leaves();
        if leaves.len() < 2 {
            return None;
        }
        let file = |path: &Path| files.iter().position(|f| f.path == path);
        let leaf = |g| {
            let group = groups.iter().find(|x| x.id == g)?;
            let mut out = SessionGroup::default();
            for (slot, v) in group.views.iter().enumerate() {
                let Some(b) = ed.index_of_id(v.id).map(|i| &ed.buffers[i]) else { continue };
                let Some(file) = b.path.as_deref().and_then(file) else { continue };
                let state = v.parked.as_ref().map_or(&b.view, |p| &p.state);
                let (line, col) = crate::editor::selection::pos_to_line_col(&b.rope, state.sel().head);
                if group.active == Some(slot) {
                    out.active = Some(out.views.len());
                }
                let (scroll_line, scroll_col, folds) = (state.scroll_line, state.scroll_col, state.folds.to_session(&b.rope));
                let wrap = (state.wrap.on != ed.soft_wrap_for(b.path.as_deref())).then_some(state.wrap.on);
                out.views.push(SessionView { file, line, col, scroll_line, scroll_col, pinned: v.pinned, folds, wrap });
            }
            Some(SessionNode::Leaf(out))
        };
        fn node(n: &SplitNode, leaf: &dyn Fn(u32) -> Option<SessionNode>) -> Option<SessionNode> {
            Some(match n {
                SplitNode::Leaf(g) => leaf(*g)?,
                SplitNode::Split { axis, ratio, a, b } => {
                    SessionNode::Split { axis: *axis, ratio: *ratio, a: Box::new(node(a, leaf)?), b: Box::new(node(b, leaf)?) }
                }
            })
        }
        let focused = leaves.iter().position(|g| *g == focused)?;
        Some(SessionSplits { tree: node(tree, &leaf)?, focused })
    }

    /// Save the current root's session now (on switch and quit; ordered
    /// after any background save). Failures are logged only.
    pub fn save_session(&mut self) {
        // Before the session loads the editor is empty and holds no branch
        // sets (or those of the previous root): the file stays as it is.
        if self.session_save.loaded != Some(self.workspace.generation) {
            return;
        }
        let session = self.session();
        self.session_save.seq += 1;
        match self.session_save.writer.write(self.session_save.seq, &session, &self.paths.sessions_dir()) {
            Ok(_) => self.session_save.last = Some(session),
            Err(e) => tracing::warn!("saving session: {e:#}"),
        }
    }

    /// The next tick saves the session (when loaded and changed).
    pub(crate) fn request_session_save(&mut self) {
        self.session_save.due = Some(Instant::now());
    }

    /// Every `SESSION_SAVE_EVERY` once the current root's session is
    /// loaded: build it here, write it off the main loop when it changed
    /// since the last write. True when a write started.
    pub(crate) fn session_tick(&mut self, now: Instant) -> bool {
        if self.session_save.loaded != Some(self.workspace.generation) || self.session_save.due.is_none_or(|d| now < d) {
            return false;
        }
        self.session_save.due = Some(now + SESSION_SAVE_EVERY);
        if self.session_save.writer.take_failed() {
            self.session_save.last = None;
        }
        let session = self.session();
        if self.session_save.last.as_ref() == Some(&session) {
            return false;
        }
        self.session_save.seq += 1;
        let (seq, writer, dir) = (self.session_save.seq, self.session_save.writer.clone(), self.paths.sessions_dir());
        self.session_save.last = Some(session.clone());
        tokio::task::spawn_blocking(move || {
            if let Err(e) = writer.write(seq, &session, &dir) {
                tracing::warn!("saving session: {e:#}");
            }
        });
        true
    }

    /// Load the current root's session and its files off the main loop; the
    /// tabs open in [`App::session_loaded`]. No session arrives as an empty
    /// one, so a pending command line file still opens. The session follows
    /// the checked-out branch before its files are read.
    pub fn restore_session(&self) {
        let (dir, root, generation, tx) =
            (self.paths.sessions_dir(), self.workspace.root.clone(), self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let names = crate::git::repo::branch_names(&root);
            let mut session = Session::load(&dir, &root).unwrap_or_else(|| Session { root, ..Default::default() });
            // The branch may have changed since the session was saved.
            if let Some((current, local)) = names {
                session.follow_branch(current, local.as_deref());
            }
            let files = session.files.iter().map(|f| std::fs::read(&f.path)).collect();
            let _ = tx.send(AppMsg::SessionLoaded { generation, session: Box::new(session), files });
        });
    }

    /// Follow the checked-out branch once this root's session is loaded:
    /// store the outgoing branch's editor set; when the new branch has one,
    /// close the clean tabs it lacks and read its files off the main loop
    /// (`branch_set_loaded` opens them). A branch without a stored set keeps
    /// the current set; detached HEAD, no git and an open conflict resolver
    /// change nothing.
    pub(crate) fn sync_branch_set(&mut self) {
        if self.session_save.loaded != Some(self.workspace.generation) || self.editor.merge_active() {
            return;
        }
        let Some(branch) = self.git_panel.snapshot().and_then(|s| s.branch.clone()) else { return };
        let old = match self.session_branch.replace(branch.clone()) {
            Some(old) if old == branch => return,
            Some(old) => old,
            // The first branch seen: the current set is its set.
            None => {
                self.request_session_save();
                return;
            }
        };
        tracing::info!("branch {old} -> {branch}: switching the editor set");
        let outgoing = match &self.branch_swap.pending {
            Some((_, set)) => set.clone(),
            None => self.editor_set(),
        };
        self.branch_sets.insert(old, outgoing);
        self.request_session_save();
        // Never used: keep the current set (and a read still in flight).
        let Some(incoming) = self.branch_sets.remove(&branch) else { return };
        let keep: Vec<PathBuf> = incoming.files.iter().map(|f| f.path.clone()).collect();
        self.editor.close_for_branch_swap(&keep);
        self.branch_swap.seq += 1;
        let seq = self.branch_swap.seq;
        self.branch_swap.pending = Some((seq, incoming.clone()));
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let files = incoming.files.iter().map(|f| std::fs::read(&f.path)).collect();
            let _ = tx.send(AppMsg::BranchSetLoaded { generation, seq, set: incoming, files });
        });
        self.dirty_frame = true;
    }

    /// The files of a branch swap's set arrived: open them unless the root
    /// changed or a later swap superseded this one.
    pub(crate) fn branch_set_loaded(&mut self, generation: u64, seq: u64, set: BranchSet, files: Vec<std::io::Result<Vec<u8>>>) {
        if generation != self.workspace.generation || self.branch_swap.pending.as_ref().map(|(s, _)| *s) != Some(seq) {
            tracing::debug!("dropping a superseded branch set");
            return;
        }
        self.branch_swap.pending = None;
        self.open_editor_set(&set, files);
        self.request_session_save();
        self.dirty_frame = true;
    }

    /// Open a set's files that are not open yet (`files[i]` read from
    /// `set.files[i]`), lay them out in its groups and restore recency and
    /// the active tab. Open tabs stay; missing and unreadable files are
    /// skipped.
    pub(crate) fn open_editor_set(&mut self, set: &BranchSet, files: Vec<std::io::Result<Vec<u8>>>) {
        for (f, result) in set.files.iter().zip(files) {
            if self.editor.index_of(&f.path).is_some() {
                continue;
            }
            let bytes = match result {
                Ok(b) => b,
                Err(e) => {
                    tracing::info!("session: skipping {}: {e}", f.path.display());
                    continue;
                }
            };
            let Some((text, lossy)) = self.decode_file(&f.path, bytes) else { continue };
            let i = self.editor.open_text(f.path.clone(), &text, lossy, &self.theme);
            self.editor.restore_view(i, (f.line, f.col), (f.scroll_line, f.scroll_col), &f.folds, f.wrap);
            self.editor.buffers[i].set_bookmarks(&f.bookmarks);
            self.editor.restore_digits(i, &f.bookmark_digits);
            self.editor.restore_letters(i, &f.bookmark_letters);
            self.editor.buffers[i].set_bookmark_groups(&f.bookmark_groups);
            self.editor.buffers[i].set_breakpoints(&f.breakpoints);
            self.editor.buffers[i].restore_breakpoint_specs(&loaded_specs(&f.breakpoint_specs));
            if f.pinned {
                self.editor.set_pinned(i, true);
            }
            self.git.load_blob(f.path.clone(), crate::git::BlobWhich::Head, None);
        }
        if let Some(splits) = &set.splits {
            let buffers: Vec<Option<usize>> = set.files.iter().map(|f| self.editor.index_of(&f.path)).collect();
            self.editor.restore_splits(splits, &buffers);
        }
        let recent: Vec<PathBuf> = set.recent.iter().filter_map(|i| set.files.get(*i)).map(|f| f.path.clone()).collect();
        let active = set.active.and_then(|i| set.files.get(i)).map(|f| f.path.as_path());
        self.editor.restore_recent(&recent, active);
        // Applied once recency is known, so the limit keeps the recent tabs.
        self.editor.enforce_tab_limit();
        // Opening the tabs is not navigation.
        self.editor.clear_nav();
    }

    /// Open the tabs of a loaded session (dropped when the root changed
    /// since), restore its layout and tree folders, then open the pending
    /// command line file and offer the root's crash dumps. Missing files and
    /// folders, and files already open, are skipped. It also takes the
    /// per-branch sets and starts the periodic save. With no tab open and no
    /// pending file, the Project window is shown and focused.
    pub fn session_loaded(&mut self, generation: u64, session: Session, files: Vec<std::io::Result<Vec<u8>>>) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping stale session for {}", session.root.display());
            return;
        }
        self.open_editor_set(&session.editor_set(), files);
        self.editor.restore_closed_bookmarks(&closed_bookmarks_of(&session));
        let breakpoints: Vec<_> = session
            .closed_breakpoints
            .iter()
            .map(|f| {
                let specs = loaded_specs(&f.specs);
                let marks = f
                    .lines
                    .iter()
                    .map(|l| (*l, specs.iter().find(|(sl, _)| sl == l).map(|(_, s)| s.clone()).unwrap_or_default()))
                    .collect();
                (f.path.clone(), marks)
            })
            .collect();
        self.editor.restore_closed_breakpoint_marks(&breakpoints);
        self.debug_panel.set_watches(&session.watches);
        self.debug.exception_filters = session
            .exception_filters
            .iter()
            .map(|(adapter, list)| (adapter.clone(), list.iter().filter(|f| !f.filter.is_empty()).cloned().collect::<Vec<_>>()))
            .filter(|(_, list)| !list.is_empty())
            .collect();
        if let Some(l) = &session.layout {
            let tasks = l.tasks.clone().unwrap_or_default();
            let dock = Dock { side: tasks.side.unwrap_or(ToolWindowId::Tasks.default_side()), split: tasks.split };
            self.apply_placement(&l.placement(), &[(ToolWindowId::Tasks, dock, tasks.shown)]);
            self.tasks.restore(&tasks);
            self.set_git_tab(l.git_tab);
            self.project.set_visibility(l.show_hidden, l.show_ignored);
        }
        self.run.selected = session.run_config.clone();
        // Reads project files only; nothing starts.
        self.load_run_tasks();
        let expanded: Vec<PathBuf> = session.expanded.iter().map(|d| session.root.join(d)).collect();
        self.project.expand_paths(&expanded);
        self.branch_sets = session.branches.clone();
        self.session_branch = session.branch.clone();
        // From now on the editor shows this root's session: the tick may save it.
        self.session_save.loaded = Some(generation);
        self.session_save.due = Some(Instant::now() + SESSION_SAVE_EVERY);
        self.session_save.last = None;
        // A status that arrived before the session may name another branch.
        self.branch_swap.pending = None;
        self.sync_branch_set();
        let pending = self.pending_open.take();
        let opening = pending.is_some();
        if let Some((path, line, col)) = pending {
            self.open_file_at(path, line.saturating_sub(1), col.saturating_sub(1));
        }
        // Nothing to edit: start in the tree. A pending file is still loading
        // (`buffers` is empty until `file_loaded`), and zen mode is the user's.
        if !opening && self.editor.buffers.is_empty() && self.layout.zen.is_none() {
            self.layout.show(ToolWindowId::Project);
            self.focus.set(PanelId::Project);
        }
        // Hot exit first: its buffers are restored silently, then crash dumps are offered.
        self.restore_hot_exit();
        self.offer_recovery();
        self.dirty_frame = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Bookmark;

    fn plain() -> Bookmark {
        Bookmark::default()
    }

    fn digit(d: u8) -> Bookmark {
        Bookmark { mnemonic: crate::editor::Mnemonic::digit(d), group: None }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-test-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_ne!(Session::file_for(Path::new("/s"), Path::new("/tmp/a")), Session::file_for(Path::new("/s"), Path::new("/tmp/b")));
    }

    #[test]
    fn session_json_round_trip() {
        let dir = temp_dir("roundtrip");
        let root = PathBuf::from("/tmp/some/project");
        let s = Session {
            root: root.clone(),
            files: vec![
                SessionFile {
                    path: root.join("src/main.rs"),
                    line: 10,
                    col: 4,
                    scroll_line: 2,
                    scroll_col: 0,
                    bookmarks: vec![3, 40, 50],
                    bookmark_digits: vec![(7, 40)],
                    bookmark_letters: vec![('K', 3)],
                    bookmark_groups: vec![("API".into(), 3)],
                    breakpoints: vec![2, 40],
                    breakpoint_specs: vec![SessionBreakpointSpec { line: 40, condition: Some("n > 1".into()), ..Default::default() }],
                    pinned: true,
                    folds: vec![SessionFold { start_line: 2, start_col: 10, end_line: 5, end_col: 1 }],
                    wrap: Some(true),
                },
                SessionFile {
                    path: root.join("README.md"),
                    line: 0,
                    col: 0,
                    scroll_line: 0,
                    scroll_col: 3,
                    bookmarks: vec![],
                    bookmark_digits: vec![],
                    bookmark_letters: vec![],
                    bookmark_groups: vec![],
                    breakpoints: vec![],
                    breakpoint_specs: vec![],
                    pinned: false,
                    folds: vec![],
                    wrap: None,
                },
            ],
            active: Some(1),
            recent: vec![1, 0],
            layout: Some(SessionLayout {
                left: Some(ToolWindowId::Structure),
                left_w: 40,
                bottom: Some(ToolWindowId::Git),
                bottom_h: 12,
                git_tab: GitTab::Log,
                show_hidden: false,
                show_ignored: true,
                right: None,
                right_w: None,
                sides: vec![],
                tasks: None,
                order: vec![],
                unpinned: vec![],
                split: vec![],
                left2: None,
                bottom2: None,
                right2: None,
                left2_h: None,
                bottom2_w: None,
                right2_h: None,
                undocked: vec![],
            }),
            expanded: vec![PathBuf::from("src"), PathBuf::from("src/app")],
            splits: Some(SessionSplits {
                tree: SessionNode::Split {
                    axis: Axis::Horizontal,
                    ratio: 400,
                    a: Box::new(SessionNode::Leaf(SessionGroup {
                        views: vec![SessionView { file: 1, ..Default::default() }],
                        active: Some(0),
                    })),
                    b: Box::new(SessionNode::Leaf(SessionGroup {
                        views: vec![SessionView {
                            file: 0,
                            line: 3,
                            col: 1,
                            scroll_line: 2,
                            scroll_col: 0,
                            pinned: true,
                            folds: vec![SessionFold { start_line: 0, start_col: 4, end_line: 9, end_col: 0 }],
                            wrap: Some(false),
                        }],
                        active: Some(0),
                    })),
                },
                focused: 1,
            }),
            closed_bookmarks: vec![SessionBookmark { digit: 2, path: root.join("src/lib.rs"), line: 12 }],
            closed_plain_bookmarks: vec![SessionClosedFile { path: root.join("src/lib.rs"), lines: vec![3, 40], specs: vec![] }],
            closed_bookmark_letters: vec![SessionLetterBookmark { letter: 'Q', path: root.join("src/lib.rs"), line: 40 }],
            closed_bookmark_groups: vec![SessionGroupBookmark { group: "API".into(), path: root.join("src/lib.rs"), line: 12 }],
            closed_breakpoints: vec![SessionClosedFile { path: root.join("src/lib.rs"), lines: vec![5], specs: vec![] }],
            watches: vec!["a".into(), "b.c".into()],
            exception_filters: BTreeMap::new(),
            run_config: Some("cargo test".into()),
            branch: Some("main".into()),
            branches: BTreeMap::from([(
                "feature".to_string(),
                BranchSet {
                    files: vec![SessionFile { path: root.join("f.rs"), line: 7, ..Default::default() }],
                    active: Some(0),
                    recent: vec![0],
                    splits: None,
                },
            )]),
        };
        s.save(&dir).unwrap();
        assert_eq!(Session::load(&dir, &root), Some(s.clone()));
        // Another root has no session.
        assert_eq!(Session::load(&dir, Path::new("/tmp/other")), None);
        // Older files without the optional fields still load.
        let file = Session::file_for(&dir, &root);
        std::fs::write(&file, r#"{"root":"/tmp/some/project","files":[{"path":"/tmp/some/project/a"}]}"#).unwrap();
        let old = Session::load(&dir, &root).unwrap();
        assert_eq!((old.files[0].line, old.active, old.recent.len(), old.files[0].bookmarks.len()), (0, None, 0, 0));
        assert_eq!((old.layout, old.expanded.len(), old.files[0].pinned, old.splits), (None, 0, false, None));
        assert!(old.files[0].folds.is_empty());
        assert_eq!(old.run_config, None, "sessions without the field still load");
        assert!(old.branch.is_none() && old.branches.is_empty());
        let json = serde_json::to_value(Session { root: root.clone(), ..Default::default() }).unwrap();
        assert!(json.get("branch").is_none() && json.get("branches").is_none(), "no branch data, no keys");
        assert!(old.files[0].bookmark_digits.is_empty() && old.closed_bookmarks.is_empty() && old.closed_plain_bookmarks.is_empty());
        assert!(old.files[0].breakpoints.is_empty() && old.closed_breakpoints.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn branch_sets_survive_the_app_round_trip() {
        let (mut app, dir) = crate::app::tests::test_app("session-branches");
        let set = |name: &str| BranchSet {
            files: vec![SessionFile { path: app.workspace.root.join(name), ..Default::default() }],
            ..Default::default()
        };
        let (feature, main) = (set("f.rs"), set("m.rs"));
        app.session_branch = Some("main".into());
        app.branch_sets.insert("feature".into(), feature);
        app.branch_sets.insert("main".into(), main);
        let s = app.session();
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.branches.keys().collect::<Vec<_>>(), ["feature"], "the current branch is filtered");

        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-branches-2");
        fresh.workspace.root = app.workspace.root.clone();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, s.clone(), vec![]);
        assert_eq!(fresh.branch_sets, s.branches);
        assert_eq!(fresh.session_branch.as_deref(), Some("main"));
        for d in [dir, fresh_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[tokio::test]
    async fn editor_set_is_the_session_editor_part() {
        let (mut app, dir) = crate::app::tests::test_app("session-editor-set");
        for n in ["a.txt", "b.txt"] {
            let path = app.workspace.root.join(n);
            app.editor.open_text(path, "x\n", false, &app.theme);
        }
        let set = app.editor_set();
        assert_eq!(set.files.len(), 2);
        assert_eq!(set, app.session().editor_set());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_of(root: &Path, names: &[&str]) -> BranchSet {
        BranchSet { files: names.iter().map(|n| SessionFile { path: root.join(n), ..Default::default() }).collect(), ..Default::default() }
    }

    fn file_names(set: &BranchSet) -> Vec<String> {
        set.files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn follow_branch_cases() {
        let root = PathBuf::from("/r");
        let both = ["main".to_string(), "dev".to_string()];
        let mut s = Session { root: root.clone(), branch: Some("main".into()), ..Default::default() };
        s.set_editor_set(set_of(&root, &["a"]));
        s.branches = BTreeMap::from([("dev".to_string(), set_of(&root, &["b"])), ("gone".to_string(), set_of(&root, &["c"]))]);
        // Same branch: nothing moves, deleted branches are dropped.
        s.follow_branch(Some("main".into()), Some(&both));
        assert_eq!(file_names(&s.editor_set()), ["a"]);
        assert_eq!(s.branches.keys().collect::<Vec<_>>(), ["dev"]);
        // Another branch with a stored set.
        s.follow_branch(Some("dev".into()), Some(&both));
        assert_eq!((file_names(&s.editor_set()), s.branch.as_deref()), (vec!["b".to_string()], Some("dev")));
        assert_eq!(s.branches.keys().collect::<Vec<_>>(), ["main"]);
        assert_eq!(file_names(&s.branches["main"]), ["a"]);
        // Detached HEAD changes nothing.
        let before = s.clone();
        s.follow_branch(None, Some(&both));
        assert_eq!(s, before);
        // A branch never used keeps the set.
        let all = ["main".to_string(), "dev".to_string(), "new".to_string()];
        s.follow_branch(Some("new".into()), Some(&all));
        assert_eq!((file_names(&s.editor_set()), s.branch.as_deref()), (vec!["b".to_string()], Some("new")));
        assert_eq!(s.branches.keys().collect::<Vec<_>>(), ["dev", "main"]);
        assert_eq!(file_names(&s.branches["dev"]), ["b"]);
        assert_eq!(file_names(&s.branches["main"]), ["a"]);
        // A failed branch listing prunes nothing.
        s.follow_branch(Some("new".into()), None);
        assert_eq!(s.branches.keys().collect::<Vec<_>>(), ["dev", "main"]);
        // A session without a branch adopts the current one.
        let mut old = Session { root: root.clone(), ..Default::default() };
        old.set_editor_set(set_of(&root, &["a"]));
        old.follow_branch(Some("main".into()), Some(&["main".to_string()]));
        assert_eq!((old.branch.as_deref(), file_names(&old.editor_set())), (Some("main"), vec!["a".to_string()]));
        assert!(old.branches.is_empty());
    }

    #[tokio::test]
    async fn restore_follows_a_branch_changed_while_closed() {
        use crate::app::tests::{handle_next, init_repo, test_app};
        use crate::git::repo::tests::git_cli;
        let (mut app, dir) = test_app("restore-branch");
        // `App::new` started a restore of its own.
        handle_next(&mut app, |m| matches!(m, AppMsg::SessionLoaded { .. })).await;
        init_repo(&app, &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let root = app.workspace.root.clone();
        let (main, _) = crate::git::repo::branch_names(&root).unwrap();
        let main = main.unwrap();
        let mut s = Session { root: root.clone(), branch: Some("dev".into()), ..Default::default() };
        s.set_editor_set(set_of(&root, &["a.txt"]));
        s.branches.insert(main.clone(), set_of(&root, &["b.txt"]));
        s.save(&app.paths.sessions_dir()).unwrap();
        git_cli(&root, &["branch", "dev"], true);
        app.restore_session();
        handle_next(&mut app, |m| matches!(m, AppMsg::SessionLoaded { .. })).await;
        assert_eq!(app.editor.tab_names(), ["b.txt"]);
        assert_eq!(app.session_branch.as_deref(), Some(main.as_str()));
        assert_eq!(app.branch_sets.keys().collect::<Vec<_>>(), ["dev"]);
        assert_eq!(file_names(&app.branch_sets["dev"]), ["a.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn superseded_branch_set_is_dropped() {
        let (mut app, dir) = crate::app::tests::test_app("superseded-set");
        let root = app.workspace.root.clone();
        let generation = app.workspace.generation;
        app.session_loaded(generation, Session { root: root.clone(), ..Default::default() }, vec![]);
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.join(n), "x\n").unwrap();
        }
        let set_a = set_of(&root, &["a.txt"]);
        let mut set_b = set_of(&root, &["b.txt", "c.txt"]);
        let leaf =
            |file: usize| SessionNode::Leaf(SessionGroup { views: vec![SessionView { file, ..Default::default() }], active: Some(0) });
        set_b.splits = Some(SessionSplits {
            tree: SessionNode::Split { axis: Axis::Horizontal, ratio: 500, a: Box::new(leaf(0)), b: Box::new(leaf(1)) },
            focused: 0,
        });
        let ok = || vec![Ok(b"x\n".to_vec())];
        app.branch_swap.pending = Some((2, set_b.clone()));
        app.branch_set_loaded(generation, 1, set_a, ok());
        assert!(app.editor.buffers.is_empty() && app.branch_swap.pending.is_some());
        app.branch_set_loaded(generation - 1, 2, set_b.clone(), vec![Ok(b"x\n".to_vec()), Ok(b"x\n".to_vec())]);
        assert!(app.editor.buffers.is_empty());
        // The second group's only file is unreadable: its group collapses.
        app.branch_set_loaded(generation, 2, set_b, vec![Ok(b"x\n".to_vec()), Err(std::io::Error::other("gone"))]);
        assert_eq!(app.editor.tab_names(), ["b.txt"]);
        assert!(app.branch_swap.pending.is_none());
        assert_eq!(app.editor.group_ids().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_writer_drops_an_older_save() {
        let dir = temp_dir("writer");
        let root = PathBuf::from("/tmp/writer/project");
        let a = Session { root: root.clone(), run_config: Some("a".into()), ..Default::default() };
        let b = Session { root: root.clone(), run_config: Some("b".into()), ..Default::default() };
        let writer = SessionWriter::default();
        assert!(writer.write(2, &b, &dir).unwrap());
        assert!(!writer.write(1, &a, &dir).unwrap());
        assert_eq!(Session::load(&dir, &root), Some(b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn periodic_save_waits_for_the_session() {
        let (mut app, dir) = crate::app::tests::test_app("session-periodic");
        let root = app.workspace.root.clone();
        let path = root.join("a.txt");
        std::fs::write(&path, "x\n").unwrap();
        app.editor.open_text(path.clone(), "x\n", false, &app.theme);
        let file = Session::file_for(&app.paths.sessions_dir(), &root);
        assert!(!app.session_tick(Instant::now() + SESSION_SAVE_EVERY * 2), "not loaded yet");
        assert!(!file.exists());
        let generation = app.workspace.generation;
        app.session_loaded(generation, Session { root: root.clone(), ..Default::default() }, vec![]);
        assert!(!app.session_tick(Instant::now()), "not due");
        let later = Instant::now() + SESSION_SAVE_EVERY * 2;
        assert!(app.session_tick(later));
        let mut saved = None;
        for _ in 0..500 {
            saved = Session::load(&app.paths.sessions_dir(), &root);
            if saved.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(saved.expect("the background write landed").files[0].path, path);
        assert!(!app.session_tick(later + SESSION_SAVE_EVERY * 2), "unchanged");
        app.request_session_save();
        assert!(!app.session_tick(Instant::now()), "a request does not force a write of an unchanged session");
        app.workspace.generation += 1;
        assert!(!app.session_tick(later + SESSION_SAVE_EVERY * 4), "stale generation");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_before_load_leaves_the_file_alone() {
        let (mut app, dir) = crate::app::tests::test_app("save-before-load");
        let root = app.workspace.root.clone();
        let mut stored = Session { root: root.clone(), branch: Some("main".into()), ..Default::default() };
        stored.branches.insert("dev".into(), set_of(&root, &["a.txt"]));
        stored.save(&app.paths.sessions_dir()).unwrap();
        app.save_session();
        assert_eq!(Session::load(&app.paths.sessions_dir(), &root), Some(stored));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn root_switch_drops_branch_sets() {
        let (mut app, dir) = crate::app::tests::test_app("switch-drops-sets");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let root = app.workspace.root.clone();
        let generation = app.workspace.generation;
        app.session_loaded(generation, Session { root: root.clone(), ..Default::default() }, vec![]);
        app.session_branch = Some("main".into());
        app.branch_sets.insert("dev".into(), set_of(&root, &["a.txt"]));
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(app.session_branch.is_none() && app.branch_sets.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_is_retried() {
        let dir = temp_dir("write-fails");
        std::fs::create_dir_all(&dir).unwrap();
        let root = PathBuf::from("/tmp/write-fails/project");
        let s = Session { root: root.clone(), ..Default::default() };
        // A directory where the temp file goes makes the write fail.
        std::fs::create_dir_all(Session::file_for(&dir, &root).with_extension("json.tmp")).unwrap();
        let writer = SessionWriter::default();
        assert!(writer.write(1, &s, &dir).is_err());
        assert!(writer.take_failed() && !writer.take_failed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_or_foreign_session_is_ignored() {
        let dir = temp_dir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let root = Path::new("/tmp/p");
        std::fs::write(Session::file_for(&dir, root), "{not json").unwrap();
        assert_eq!(Session::load(&dir, root), None);
        // A hash collision (or a copied file) for a different root.
        let other = Session { root: PathBuf::from("/tmp/q"), ..Default::default() };
        std::fs::write(Session::file_for(&dir, root), serde_json::to_string(&other).unwrap()).unwrap();
        assert_eq!(Session::load(&dir, root), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bookmarks_survive_a_session_round_trip() {
        let (mut app, dir) = crate::app::tests::test_app("session-bookmarks");
        let path = app.workspace.root.join("a.txt");
        let text = "zero\none\ntwo\nthree\n";
        app.editor.open_text(path.clone(), text, false, &app.theme);
        app.editor.buffers[0].set_bookmarks(&[1, 3]);
        let id = app.editor.buffers[0].id;
        app.editor.set_bookmark_mnemonic(id, 1, crate::editor::Mnemonic::Digit(5));
        let gone = app.workspace.root.join("gone.txt");
        app.editor.restore_closed_bookmarks(&[(gone.clone(), 4, digit(2)), (gone.clone(), 1, plain()), (gone.clone(), 7, plain())]);
        let b = &mut app.editor.buffers[0];
        let fold = crate::editor::fold::Fold { start: 0, end: b.rope.line_to_char(2) + 3 };
        assert!(b.view.folds.collapse(fold, &b.rope));
        b.goto_line(3, 0);
        let session = app.session();
        assert_eq!(session.files[0].bookmarks, vec![1, 3]);
        assert_eq!(session.files[0].bookmark_digits, vec![(5, 1)]);
        assert_eq!(session.closed_bookmarks, vec![SessionBookmark { digit: 2, path: gone.clone(), line: 4 }]);
        assert_eq!(session.closed_plain_bookmarks, vec![SessionClosedFile { path: gone.clone(), lines: vec![1, 7], specs: vec![] }]);
        assert_eq!(session.files[0].folds, [SessionFold { start_line: 0, start_col: 0, end_line: 2, end_col: 3 }]);
        app.session().save(&app.paths.sessions_dir()).unwrap();

        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-bookmarks-2");
        fresh.workspace.root = app.workspace.root.clone();
        let loaded = Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);
        // The file lost a line since; the mark past the end is dropped.
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, vec![Ok(b"zero\none\ntwo".to_vec())]);
        let b = &fresh.editor.buffers[0];
        assert_eq!(b.bookmarks.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(b.digits(), vec![(5, 1)]);
        assert_eq!(fresh.editor.closed_bookmarks(), vec![(gone.clone(), 1, plain()), (gone.clone(), 4, digit(2)), (gone, 7, plain())]);
        assert_eq!(b.view.folds.hidden().first(), Some(&(1..3)), "the fold is back");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    fn marked(m: Option<crate::editor::Mnemonic>, group: Option<&str>) -> Bookmark {
        Bookmark { mnemonic: m, group: group.map(String::from) }
    }

    /// A session with letters and groups, from an app with a.txt open and gone.txt closed.
    fn lettered_session(name: &str) -> (Session, crate::app::App, PathBuf) {
        use crate::editor::Mnemonic;
        let (mut app, dir) = crate::app::tests::test_app(name);
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path, "l0\nl1\nl2\nl3\nl4\nl5\n", false, &app.theme);
        app.editor.buffers[0].set_bookmarks(&[1, 3, 4]);
        let id = app.editor.buffers[0].id;
        app.editor.set_bookmark_mnemonic(id, 1, Mnemonic::Letter('K'));
        app.editor.set_bookmark_mnemonic(id, 3, Mnemonic::Digit(5));
        app.editor.move_bookmark_to_group(id, 1, Some("API".into()));
        app.editor.move_bookmark_to_group(id, 4, Some("later".into()));
        let gone = app.workspace.root.join("gone.txt");
        app.editor.restore_closed_bookmarks(&[
            (gone.clone(), 4, marked(Mnemonic::letter('q'), Some("API"))),
            (gone.clone(), 1, plain()),
            (gone.clone(), 7, marked(Mnemonic::digit(2), Some("later"))),
        ]);
        (app.session(), app, dir)
    }

    #[tokio::test]
    async fn letters_and_groups_survive_a_session_round_trip() {
        use crate::editor::Mnemonic;
        let (session, app, dir) = lettered_session("session-letters");
        let gone = app.workspace.root.join("gone.txt");
        let f = &session.files[0];
        assert_eq!(f.bookmarks, vec![1, 3, 4]);
        assert_eq!(f.bookmark_digits, vec![(5, 3)]);
        assert_eq!(f.bookmark_letters, vec![('K', 1)]);
        assert_eq!(f.bookmark_groups, vec![("API".to_string(), 1), ("later".to_string(), 4)]);
        assert_eq!(session.closed_bookmarks, vec![SessionBookmark { digit: 2, path: gone.clone(), line: 7 }]);
        assert_eq!(session.closed_plain_bookmarks, vec![SessionClosedFile { path: gone.clone(), lines: vec![1, 4], specs: vec![] }]);
        assert_eq!(session.closed_bookmark_letters, vec![SessionLetterBookmark { letter: 'Q', path: gone.clone(), line: 4 }]);
        assert_eq!(
            session.closed_bookmark_groups,
            vec![
                SessionGroupBookmark { group: "API".into(), path: gone.clone(), line: 4 },
                SessionGroupBookmark { group: "later".into(), path: gone.clone(), line: 7 },
            ]
        );
        // Through the file, into a fresh app.
        session.save(&app.paths.sessions_dir()).unwrap();
        let loaded = Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);
        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-letters-2");
        fresh.workspace.root = app.workspace.root.clone();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, vec![Ok(b"l0\nl1\nl2\nl3\nl4\nl5\n".to_vec())]);
        let got: Vec<_> = fresh.editor.buffers[0].bookmarks.iter().map(|(l, b)| (*l, b.clone())).collect();
        let want: Vec<_> = app.editor.buffers[0].bookmarks.iter().map(|(l, b)| (*l, b.clone())).collect();
        assert_eq!(got, want);
        assert_eq!(got[0], (1, marked(Mnemonic::letter('k'), Some("API"))));
        assert_eq!(fresh.editor.closed_bookmarks(), app.editor.closed_bookmarks());
        assert_eq!(
            fresh.editor.closed_bookmarks(),
            vec![
                (gone.clone(), 1, plain()),
                (gone.clone(), 4, marked(Mnemonic::letter('q'), Some("API"))),
                (gone, 7, marked(Mnemonic::digit(2), Some("later"))),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[tokio::test]
    async fn session_without_letters_or_groups_writes_no_new_keys() {
        let (mut app, dir) = crate::app::tests::test_app("session-no-letters");
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path, "l0\nl1\nl2\n", false, &app.theme);
        app.editor.buffers[0].set_bookmarks(&[0, 2]);
        let id = app.editor.buffers[0].id;
        app.editor.set_bookmark_mnemonic(id, 2, crate::editor::Mnemonic::Digit(3));
        let gone = app.workspace.root.join("gone.txt");
        app.editor.restore_closed_bookmarks(&[(gone.clone(), 4, digit(2)), (gone, 1, plain())]);
        let json = serde_json::to_string(&app.session()).unwrap();
        for key in ["bookmark_letters", "bookmark_groups", "closed_bookmark_letters", "closed_bookmark_groups"] {
            assert!(!json.contains(key), "{key} in {json}");
        }
        assert!(json.contains("bookmark_digits") && json.contains("closed_plain_bookmarks"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn older_builds_keep_every_bookmark_of_a_newer_session() {
        let (session, app, dir) = lettered_session("session-older-build");
        let mut json = serde_json::to_value(&session).unwrap();
        // The file, as a build that knows none of the new keys reads it.
        json["files"][0].as_object_mut().unwrap().remove("bookmark_letters").unwrap();
        json["files"][0].as_object_mut().unwrap().remove("bookmark_groups").unwrap();
        for key in ["closed_bookmark_letters", "closed_bookmark_groups"] {
            json.as_object_mut().unwrap().remove(key).unwrap();
        }
        let old: Session = serde_json::from_value(json).unwrap();
        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-older-build-2");
        fresh.workspace.root = app.workspace.root.clone();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, old, vec![Ok(b"l0\nl1\nl2\nl3\nl4\nl5\n".to_vec())]);
        let open: Vec<_> = fresh.editor.buffers[0].bookmarks.iter().map(|(l, b)| (*l, b.clone())).collect();
        assert_eq!(open, vec![(1, plain()), (3, digit(5)), (4, plain())], "every line is a bookmark; the letter and groups are gone");
        let gone = fresh.workspace.root.join("gone.txt");
        assert_eq!(fresh.editor.closed_bookmarks(), vec![(gone.clone(), 1, plain()), (gone.clone(), 4, plain()), (gone, 7, digit(2))]);
        // The newer file with keys it does not know loads.
        let mut json = serde_json::to_value(&session).unwrap();
        json["future"] = serde_json::json!(1);
        json["files"][0]["future"] = serde_json::json!(1);
        let newer: Session = serde_json::from_value(json).unwrap();
        assert_eq!(newer, session);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[tokio::test]
    async fn bad_letters_and_groups_in_a_session_are_skipped() {
        use crate::editor::Mnemonic;
        let (mut app, dir) = crate::app::tests::test_app("session-bad-letters");
        let path = app.workspace.root.join("a.txt");
        let json = serde_json::json!({
            "root": app.workspace.root,
            "files": [{
                "path": path,
                "bookmarks": [0, 1, 2],
                "bookmark_letters": [["1", 0], ["k", 1], ["K", 2], ["Q", 9]],
                "bookmark_groups": [["a\u{1}b", 0], ["Bookmarks", 1], ["X", 5]],
            }],
        });
        let session: Session = serde_json::from_value(json).unwrap();
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![Ok(b"a\nb\nc".to_vec())]);
        let got: Vec<_> = app.editor.buffers[0].bookmarks.iter().map(|(l, b)| (*l, b.clone())).collect();
        assert_eq!(got, vec![(0, plain()), (1, marked(Mnemonic::letter('K'), None)), (2, plain())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cond_spec(line: usize, text: &str) -> SessionBreakpointSpec {
        SessionBreakpointSpec { line, condition: Some(text.into()), ..Default::default() }
    }

    fn log_spec(line: usize, text: &str) -> SessionBreakpointSpec {
        SessionBreakpointSpec { line, log_message: Some(text.into()), ..Default::default() }
    }

    #[tokio::test]
    async fn breakpoints_survive_a_session_round_trip() {
        use crate::editor::buffer::{BpState, BreakpointMark, BreakpointSpec};
        let (mut app, dir) = crate::app::tests::test_app("session-breakpoints");
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path.clone(), "zero\none\ntwo\nthree\n", false, &app.theme);
        app.editor.buffers[0].set_breakpoints(&[1, 3]);
        let mark = app.editor.buffers[0].breakpoints.get_mut(&1).unwrap();
        mark.state = BpState::Verified;
        mark.id = Some(9);
        mark.spec = BreakpointSpec { condition: Some("i == 3".into()), ..Default::default() };
        let gone = app.workspace.root.join("gone.txt");
        app.editor.restore_closed_breakpoint_marks(&[(
            gone.clone(),
            vec![(7, BreakpointSpec { log_message: Some("n={n}".into()), ..Default::default() }), (1, BreakpointSpec::default())],
        )]);
        app.debug_panel.set_watches(&["sum".to_string(), "p.x".to_string()]);
        let session = app.session();
        assert_eq!(session.files[0].breakpoints, vec![1, 3]);
        assert_eq!(session.files[0].breakpoint_specs, vec![cond_spec(1, "i == 3")]);
        assert_eq!(session.watches, ["sum", "p.x"]);
        assert_eq!(
            session.closed_breakpoints,
            vec![SessionClosedFile { path: gone.clone(), lines: vec![1, 7], specs: vec![log_spec(7, "n={n}")] }]
        );
        assert!(!serde_json::to_string(&session).unwrap().contains("Verified"), "state is never written");
        session.save(&app.paths.sessions_dir()).unwrap();

        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-breakpoints-2");
        fresh.workspace.root = app.workspace.root.clone();
        let loaded = Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, vec![Ok(b"zero\none\ntwo".to_vec())]);
        let b = &fresh.editor.buffers[0];
        assert_eq!(b.breakpoints.keys().copied().collect::<Vec<_>>(), vec![1], "line 3 is past the end");
        let want = BreakpointMark { spec: BreakpointSpec { condition: Some("i == 3".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(b.breakpoints[&1], want, "the condition is back, the state is Idle");
        assert_eq!(fresh.editor.closed_breakpoints(), vec![(gone.clone(), vec![1, 7])]);
        assert_eq!(
            fresh.editor.closed_breakpoint_marks(),
            vec![(
                gone,
                vec![(1, BreakpointSpec::default()), (7, BreakpointSpec { log_message: Some("n={n}".into()), ..Default::default() })]
            )]
        );
        assert_eq!(fresh.debug_panel.watches(), ["sum", "p.x"]);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[test]
    fn sessions_without_breakpoints_write_no_keys() {
        let root = PathBuf::from("/tmp/some/project");
        let json = serde_json::to_value(Session { root, ..Default::default() }).unwrap();
        assert!(json.get("closed_breakpoints").is_none());
        let json = serde_json::to_value(SessionFile::default()).unwrap();
        assert!(json.get("breakpoints").is_none());
        assert!(json.get("breakpoint_specs").is_none());
        let json = serde_json::to_value(Session { root: PathBuf::from("/tmp/p"), ..Default::default() }).unwrap();
        assert!(json.get("watches").is_none());
        assert!(json.get("exception_filters").is_none());
        let json = serde_json::to_value(SessionClosedFile::default()).unwrap();
        assert!(json.get("specs").is_none());
        let json = serde_json::to_value(SessionBreakpointSpec { line: 2, condition: Some("c".into()), ..Default::default() }).unwrap();
        assert!(json.get("log_message").is_none() && json["condition"] == "c");
    }

    #[tokio::test]
    async fn an_old_session_file_reads_as_before() {
        let (mut app, dir) = crate::app::tests::test_app("session-old-breakpoints");
        let root = app.workspace.root.clone();
        let (path, gone) = (root.join("a.txt"), root.join("gone.txt"));
        // As the build before this one wrote it: no spec keys, no watches.
        let json = serde_json::json!({
            "root": root,
            "files": [{ "path": path, "breakpoints": [1] }],
            "closed_breakpoints": [{ "path": gone, "lines": [2] }],
            "closed_plain_bookmarks": [{ "path": gone, "lines": [3] }],
        });
        let session: Session = serde_json::from_value(json).unwrap();
        assert!(session.watches.is_empty() && session.files[0].breakpoint_specs.is_empty());
        assert!(session.closed_breakpoints[0].specs.is_empty());
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![Ok(b"a\nb\nc\n".to_vec())]);
        assert_eq!(app.editor.buffers[0].breakpoints.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert!(app.editor.buffers[0].breakpoints[&1].spec.is_plain());
        assert_eq!(app.editor.closed_breakpoints(), vec![(gone, vec![2])]);
        assert!(app.debug_panel.watches().is_empty());
        assert!(app.debug.exception_filters.is_empty());
        let written = serde_json::to_value(app.session()).unwrap();
        assert!(written["closed_plain_bookmarks"][0].get("specs").is_none(), "bookmarks carry no specs");
        assert!(written.get("watches").is_none());
        assert!(written.get("exception_filters").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn saved(filter: &str, on: bool) -> SavedFilter {
        SavedFilter { filter: filter.into(), label: format!("label {filter}"), on }
    }

    #[tokio::test]
    async fn exception_filters_survive_a_session_round_trip() {
        let (mut app, dir) = crate::app::tests::test_app("session-exception-filters");
        app.debug.exception_filters.insert("gdb".into(), vec![saved("throw", true), saved("catch", false)]);
        let session = app.session();
        assert_eq!(session.exception_filters["gdb"], vec![saved("throw", true), saved("catch", false)]);
        session.save(&app.paths.sessions_dir()).unwrap();
        let loaded = Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);
        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-exception-filters-2");
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, vec![]);
        assert_eq!(fresh.debug.exception_filters, app.debug.exception_filters);
        // Empty ids from a hand-edited file are dropped, and an adapter left with none goes.
        let edited = Session {
            root: fresh.workspace.root.clone(),
            exception_filters: BTreeMap::from([
                ("gdb".to_string(), vec![saved("", true), saved("throw", true)]),
                ("other".to_string(), vec![saved("", true)]),
            ]),
            ..Default::default()
        };
        fresh.session_loaded(generation, edited, vec![]);
        assert_eq!(fresh.debug.exception_filters, BTreeMap::from([("gdb".to_string(), vec![saved("throw", true)])]));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[test]
    fn a_session_spec_without_the_new_keys_is_enabled() {
        let spec: SessionBreakpointSpec = serde_json::from_value(serde_json::json!({ "line": 2 })).unwrap();
        assert!(spec.enabled, "a spec written before disabling existed is enabled");
        assert_eq!(spec.hit_condition, None);
        let spec: SessionBreakpointSpec = serde_json::from_value(serde_json::json!({ "line": 2, "enabled": false })).unwrap();
        assert!(!spec.enabled);
    }

    #[test]
    fn enabled_and_hit_condition_are_written_only_when_set() {
        let json = serde_json::to_value(SessionBreakpointSpec { line: 2, condition: Some("c".into()), ..Default::default() }).unwrap();
        assert!(json.get("enabled").is_none() && json.get("hit_condition").is_none(), "{json}");
        let json =
            serde_json::to_value(SessionBreakpointSpec { line: 2, enabled: false, hit_condition: Some("3".into()), ..Default::default() })
                .unwrap();
        assert_eq!(json["enabled"], false);
        assert_eq!(json["hit_condition"], "3");
    }

    #[tokio::test]
    async fn disabled_and_hit_count_specs_survive_a_session_round_trip() {
        use crate::editor::buffer::BreakpointSpec;
        let (mut app, dir) = crate::app::tests::test_app("session-disabled-specs");
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path.clone(), "zero\none\ntwo\nthree\n", false, &app.theme);
        app.editor.buffers[0].set_breakpoints(&[1, 2, 3]);
        let off = BreakpointSpec { enabled: false, ..Default::default() };
        let hits = BreakpointSpec { hit_condition: Some("3".into()), ..Default::default() };
        app.editor.buffers[0].breakpoints.get_mut(&1).unwrap().spec = off.clone();
        app.editor.buffers[0].breakpoints.get_mut(&2).unwrap().spec = hits.clone();
        let gone = app.workspace.root.join("gone.txt");
        app.editor.restore_closed_breakpoint_marks(&[(gone.clone(), vec![(4, off.clone()), (6, hits.clone())])]);
        let session = app.session();
        assert_eq!(
            session.files[0].breakpoint_specs,
            vec![
                SessionBreakpointSpec { line: 1, enabled: false, ..Default::default() },
                SessionBreakpointSpec { line: 2, hit_condition: Some("3".into()), ..Default::default() },
            ]
        );
        session.save(&app.paths.sessions_dir()).unwrap();
        let loaded = Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);

        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-disabled-specs-2");
        fresh.workspace.root = app.workspace.root.clone();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, vec![Ok(b"zero\none\ntwo\nthree\n".to_vec())]);
        let b = &fresh.editor.buffers[0];
        assert_eq!(b.breakpoints[&1].spec, off, "open file: disabled");
        assert_eq!(b.breakpoints[&2].spec, hits, "open file: hit count");
        assert!(b.breakpoints[&3].spec.is_plain());
        assert_eq!(fresh.editor.closed_breakpoint_marks(), vec![(gone, vec![(4, off), (6, hits)])], "closed file");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[tokio::test]
    async fn a_hit_count_with_a_control_character_is_dropped_on_load() {
        let (mut app, dir) = crate::app::tests::test_app("session-bad-hit-count");
        let root = app.workspace.root.clone();
        let session = Session {
            root: root.clone(),
            files: vec![SessionFile {
                path: root.join("a.txt"),
                breakpoints: vec![1, 2],
                breakpoint_specs: vec![
                    SessionBreakpointSpec { line: 1, hit_condition: Some("3\u{1b}".into()), ..Default::default() },
                    SessionBreakpointSpec { line: 2, hit_condition: Some(" 4 ".into()), ..Default::default() },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![Ok(b"a\nb\nc\n".to_vec())]);
        let marks = &app.editor.buffers[0].breakpoints;
        assert!(marks[&1].spec.is_plain(), "{:?}", marks[&1]);
        assert_eq!(marks[&2].spec.hit_condition.as_deref(), Some("4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_hand_edited_spec_is_cleaned() {
        let (mut app, dir) = crate::app::tests::test_app("session-bad-specs");
        let root = app.workspace.root.clone();
        let (path, gone) = (root.join("a.txt"), root.join("gone.txt"));
        let bad = |line, c: Option<&str>, m: Option<&str>| SessionBreakpointSpec {
            line,
            condition: c.map(Into::into),
            log_message: m.map(Into::into),
            ..Default::default()
        };
        let session = Session {
            root,
            files: vec![SessionFile {
                path,
                breakpoints: vec![1, 2],
                breakpoint_specs: vec![
                    bad(1, Some("a\nb"), Some("  ")),
                    // Kept clean: trimmed; the control character in the other field is dropped alone.
                    bad(2, Some("  ok "), Some("x\u{1b}y")),
                    // No breakpoint on line 0.
                    bad(0, Some("c"), None),
                ],
                ..Default::default()
            }],
            closed_breakpoints: vec![SessionClosedFile {
                path: gone.clone(),
                lines: vec![4],
                specs: vec![bad(4, None, Some("a\r")), bad(9, Some("c"), None)],
            }],
            watches: vec![" w ".into(), "".into(), "x\ny".into()],
            ..Default::default()
        };
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![Ok(b"a\nb\nc\n".to_vec())]);
        let marks = &app.editor.buffers[0].breakpoints;
        assert_eq!(marks.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert!(marks[&1].spec.is_plain(), "{:?}", marks[&1]);
        assert_eq!(marks[&2].spec.condition.as_deref(), Some("ok"));
        assert_eq!(marks[&2].spec.log_message, None);
        assert_eq!(app.editor.closed_breakpoint_marks(), vec![(gone, vec![(4, Default::default())])]);
        assert_eq!(app.debug_panel.watches(), ["w"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_root_switch_keeps_each_roots_specs_and_watches() {
        use crate::editor::buffer::BreakpointSpec;
        let (mut app, dir) = crate::app::tests::test_app("session-roots-specs");
        let other = std::fs::canonicalize({
            std::fs::create_dir_all(dir.join("other")).unwrap();
            dir.join("other")
        })
        .unwrap();
        let root = app.workspace.root.clone();
        let generation = app.workspace.generation;
        app.session_loaded(generation, Session { root: root.clone(), ..Default::default() }, vec![]);
        let gone = root.join("gone.txt");
        let spec = BreakpointSpec { condition: Some("i == 1".into()), ..Default::default() };
        app.editor.restore_closed_breakpoint_marks(&[(gone.clone(), vec![(2, spec.clone())])]);
        app.debug_panel.set_watches(&["sum".to_string()]);
        app.debug.exception_filters.insert("gdb".into(), vec![saved("throw", true)]);
        app.do_switch_root(other.clone());
        // The other root has its own: none.
        let generation = app.workspace.generation;
        let b = Session::load(&app.paths.sessions_dir(), &other).unwrap_or_else(|| Session { root: other.clone(), ..Default::default() });
        app.session_loaded(generation, b, vec![]);
        assert!(app.editor.closed_breakpoint_marks().is_empty());
        assert!(app.debug_panel.watches().is_empty());
        assert!(app.debug.exception_filters.is_empty(), "filters are per root");
        app.do_switch_root(root.clone());
        let a = Session::load(&app.paths.sessions_dir(), &root).expect("saved at the switch");
        assert_eq!(a.watches, ["sum"]);
        assert_eq!(a.exception_filters["gdb"], vec![saved("throw", true)]);
        let generation = app.workspace.generation;
        app.session_loaded(generation, a, vec![]);
        assert_eq!(app.editor.closed_breakpoint_marks(), vec![(gone, vec![(2, spec)])]);
        assert_eq!(app.debug_panel.watches(), ["sum"]);
        assert_eq!(app.debug.exception_filters["gdb"], vec![saved("throw", true)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn root_switch_forgets_closed_breakpoints() {
        let (mut app, dir) = crate::app::tests::test_app("switch-drops-breakpoints");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let root = app.workspace.root.clone();
        let generation = app.workspace.generation;
        app.session_loaded(generation, Session { root: root.clone(), ..Default::default() }, vec![]);
        app.editor.restore_closed_breakpoints(&[(root.join("gone.txt"), vec![2])]);
        assert_eq!(app.editor.closed_breakpoints().len(), 1);
        app.do_switch_root(std::fs::canonicalize(&other).unwrap());
        assert!(app.editor.closed_breakpoints().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn branch_sets_keep_their_breakpoints() {
        let (mut app, dir) = crate::app::tests::test_app("branch-breakpoints");
        let root = app.workspace.root.clone();
        let set = BranchSet {
            files: vec![SessionFile { path: root.join("a.txt"), breakpoints: vec![1], ..Default::default() }],
            ..Default::default()
        };
        let mut set = set;
        set.files[0].breakpoint_specs = vec![cond_spec(1, "i == 2")];
        app.open_editor_set(&set, vec![Ok(b"a\nb\n".to_vec())]);
        assert_eq!(app.editor.buffers[0].breakpoints.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(app.editor.buffers[0].breakpoints[&1].spec.condition.as_deref(), Some("i == 2"));
        assert_eq!(app.editor_set().files[0].breakpoints, vec![1]);
        assert_eq!(app.editor_set().files[0].breakpoint_specs, vec![cond_spec(1, "i == 2")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn old_session_bookmarks_load_without_digits() {
        let (mut app, dir) = crate::app::tests::test_app("session-old-bookmarks");
        let path = app.workspace.root.join("a.txt");
        // Saved before numbered bookmarks: no `bookmark_digits`, no
        // `closed_bookmarks`.
        let json = serde_json::json!({ "root": app.workspace.root, "files": [{ "path": path, "bookmarks": [1, 2] }] });
        let session: Session = serde_json::from_value(json).unwrap();
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![Ok(b"a\nb\nc\n".to_vec())]);
        let b = &app.editor.buffers[0];
        assert_eq!(b.bookmarks.iter().map(|(l, d)| (*l, d.clone())).collect::<Vec<_>>(), vec![(1, plain()), (2, plain())]);
        assert!(app.editor.closed_digits().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn soft_wrap_toggles_survive_a_session_round_trip() {
        let (mut app, dir) = crate::app::tests::test_app("session-wrap");
        // Default: .txt and .md wrap, .rs does not.
        crate::app::tests::load(&mut app, "a.txt", "a\n");
        crate::app::tests::load(&mut app, "b.rs", "b\n");
        crate::app::tests::load(&mut app, "c.md", "c\n");
        let wrap = |app: &mut App, name: &str, on: Option<bool>| {
            let i = app.editor.buffers.iter().position(|b| b.name == name).unwrap();
            if let Some(on) = on {
                app.editor.buffers[i].view.wrap.on = on;
            }
            app.editor.buffers[i].view.wrap.on
        };
        assert_eq!((wrap(&mut app, "a.txt", None), wrap(&mut app, "b.rs", None), wrap(&mut app, "c.md", None)), (true, false, true));
        wrap(&mut app, "a.txt", Some(false));
        wrap(&mut app, "b.rs", Some(true));
        let session = app.session();
        let by_name = |s: &Session, n: &str| s.files.iter().find(|f| f.path.ends_with(n)).unwrap().wrap;
        assert_eq!((by_name(&session, "a.txt"), by_name(&session, "b.rs"), by_name(&session, "c.md")), (Some(false), Some(true), None));
        let json = serde_json::to_value(&session).unwrap();
        let c = json["files"].as_array().unwrap().iter().find(|f| f["path"].as_str().unwrap().ends_with("c.md")).unwrap();
        assert!(c.get("wrap").is_none(), "an untouched file stores nothing");
        session.save(&app.paths.sessions_dir()).unwrap();

        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-wrap-2");
        fresh.workspace.root = app.workspace.root.clone();
        let loaded = Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);
        let files = loaded.files.iter().map(|f| std::fs::read(&f.path)).collect();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, files);
        assert_eq!((wrap(&mut fresh, "a.txt", None), wrap(&mut fresh, "b.rs", None), wrap(&mut fresh, "c.md", None)), (false, true, true));
        for d in [dir, fresh_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[tokio::test]
    async fn a_session_without_wrap_fields_loads() {
        let (mut app, dir) = crate::app::tests::test_app("session-no-wrap");
        let (txt, rs) = (app.workspace.root.join("a.txt"), app.workspace.root.join("b.rs"));
        std::fs::write(&txt, "a\n").unwrap();
        std::fs::write(&rs, "b\n").unwrap();
        let json = serde_json::json!({
            "root": app.workspace.root,
            "files": [{ "path": txt, "line": 0 }, { "path": rs }],
            "splits": { "tree": { "Leaf": { "views": [{ "file": 0 }, { "file": 1 }], "active": 0 } }, "focused": 0 },
        });
        let session: Session = serde_json::from_value(json).unwrap();
        assert_eq!((session.files[0].wrap, session.files[1].wrap), (None, None));
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![Ok(b"a\n".to_vec()), Ok(b"b\n".to_vec())]);
        let on: Vec<bool> = app.editor.buffers.iter().map(|b| b.view.wrap.on).collect();
        assert_eq!(on, [true, false], "defaults apply");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn split_views_keep_their_own_soft_wrap() {
        use crate::actions::{Action, EditorAction};
        let (mut app, dir) = crate::app::tests::test_app("session-split-wrap");
        crate::app::tests::load(&mut app, "a.txt", "a\n");
        app.editor.active_buffer_mut().unwrap().view.wrap.on = false;
        // The new group's view starts as a copy; wrap it again there.
        app.perform(Action::Editor(EditorAction::SplitRight));
        app.editor.active_buffer_mut().unwrap().view.wrap.on = true;
        let session = app.session();
        assert_eq!(session.files[0].wrap, None, "the live view is at the default");
        let SessionNode::Split { a, b, .. } = &session.splits.as_ref().expect("two groups").tree else { panic!("a split") };
        let wraps: Vec<Option<bool>> = [a, b]
            .iter()
            .map(|n| match &***n {
                SessionNode::Leaf(g) => g.views[0].wrap,
                SessionNode::Split { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(wraps, [Some(false), None]);
        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-split-wrap-2");
        fresh.workspace.root = app.workspace.root.clone();
        let files = session.files.iter().map(|f| std::fs::read(&f.path)).collect();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, session, files);
        let ed = &mut fresh.editor;
        let ids = ed.group_ids();
        ed.focus_group(ids[0]);
        assert!(!ed.active_buffer().unwrap().view.wrap.on, "first group keeps its off");
        ed.focus_group(ids[1]);
        assert!(ed.active_buffer().unwrap().view.wrap.on);
        for d in [dir, fresh_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[tokio::test]
    async fn split_groups_survive_a_session_round_trip() {
        use crate::actions::{Action, EditorAction};
        let (mut app, dir) = crate::app::tests::test_app("session-splits");
        let text: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let a = crate::app::tests::load(&mut app, "a.txt", &text);
        crate::app::tests::load(&mut app, "b.txt", "b\n");
        app.editor.show_slot(0);
        app.perform(Action::Editor(EditorAction::SplitRight));
        app.editor.goto_line(40, 3);
        // A fold, which the split below copies.
        let b = app.editor.active_buffer_mut().unwrap();
        let fold = crate::editor::fold::Fold { start: b.rope.line_to_char(10), end: b.rope.line_to_char(20) };
        assert!(b.view.folds.collapse(fold, &b.rope));
        app.perform(Action::Editor(EditorAction::SplitDown));
        crate::app::tests::load(&mut app, "c.txt", "c\n");
        app.perform(Action::Editor(EditorAction::TogglePinTab));
        let session = app.session();
        let splits = session.splits.clone().expect("three groups");
        assert_eq!(splits.focused, 2);
        assert_eq!(session.files.len(), 3, "each file once");
        session.save(&app.paths.sessions_dir()).unwrap();

        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-splits-2");
        fresh.workspace.root = app.workspace.root.clone();
        let loaded = Session::load(&app.paths.sessions_dir(), &app.workspace.root).unwrap();
        assert_eq!(loaded, session);
        let files = loaded.files.iter().map(|f| std::fs::read(&f.path)).collect();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, loaded, files);
        let ed = &mut fresh.editor;
        ed.check_invariants();
        assert_eq!(ed.group_ids().len(), 3);
        assert_eq!(ed.tab_names(), ["c.txt", "a.txt"], "the focused group, its pin first");
        assert!(ed.focused_group().views[0].pinned);
        assert_eq!(ed.active_buffer().unwrap().name, "c.txt");
        // Each view of a.txt has its own caret and folds.
        let hidden = |ed: &crate::editor::Editor| {
            ed.active_buffer().unwrap().view.folds.hidden().iter().map(|r| (r.start, r.end)).collect::<Vec<_>>()
        };
        ed.show_slot(1);
        assert_eq!(ed.active_buffer().unwrap().cursor_line_col(), (39, 2));
        assert_eq!(hidden(ed), [(11, 20)]);
        ed.focus_group(ed.group_ids()[0]);
        assert_eq!(ed.tab_names(), ["a.txt", "b.txt"]);
        assert_eq!(ed.active_buffer().unwrap().cursor_line_col(), (0, 0));
        assert!(hidden(ed).is_empty());
        assert_eq!(ed.index_of(&a), Some(0));
        // A session from before splits loads as one group.
        let mut json = serde_json::to_value(&session).unwrap();
        json.as_object_mut().unwrap().remove("splits");
        let old: Session = serde_json::from_value(json).unwrap();
        let (mut one, one_dir) = crate::app::tests::test_app("session-splits-3");
        one.workspace.root = app.workspace.root.clone();
        let files = old.files.iter().map(|f| std::fs::read(&f.path)).collect();
        let generation = one.workspace.generation;
        one.session_loaded(generation, old, files);
        assert_eq!(one.editor.group_ids().len(), 1);
        assert_eq!(one.editor.tab_names(), ["c.txt", "a.txt", "b.txt"], "c was pinned");
        for d in [dir, fresh_dir, one_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[tokio::test]
    async fn session_restores_layout_and_tree_folders() {
        let (mut app, dir) = crate::app::tests::test_app("session-layout");
        let root = app.workspace.root.clone();
        for d in ["a/deep", "b", ".hidden"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("a/deep/x.txt"), "").unwrap();
        app.project.reroot(root.clone());
        assert!(app.project.row_names().contains(&".hidden".to_string()), "the default config shows hidden files");
        app.focus.set(PanelId::Project);
        app.editor.open_text(root.join("a/deep/x.txt"), "", false, &app.theme);
        let layout = SessionLayout {
            left: None,
            left_w: 0,
            bottom: Some(ToolWindowId::Git),
            bottom_h: 9,
            git_tab: GitTab::Console,
            show_hidden: false,
            show_ignored: false,
            right: None,
            right_w: None,
            sides: vec![],
            tasks: None,
            order: vec![],
            unpinned: vec![],
            split: vec![],
            left2: None,
            bottom2: None,
            right2: None,
            left2_h: None,
            bottom2_w: None,
            right2_h: None,
            undocked: vec![],
        };
        // A folder deleted since is skipped.
        let expanded = vec![PathBuf::from("a"), PathBuf::from("a/deep"), PathBuf::from("gone")];
        let session = Session { root: root.clone(), layout: Some(layout.clone()), expanded, ..Default::default() };
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![]);
        assert_eq!((app.layout.left, app.layout.bottom, app.layout.bottom_h), (None, Some(ToolWindowId::Git), 9));
        assert_eq!(app.layout.left_w, crate::ui::layout::MIN_LEFT_W, "sizes are clamped");
        assert!(app.focus.is(PanelId::Editor), "focus leaves the closed Project window");
        assert_eq!(app.git_panel.tab, GitTab::Console);
        assert_eq!(app.project.row_names(), ["a", "deep", "x.txt", "b"]);
        // Saving gives the same state back.
        let saved = app.session();
        assert_eq!(
            saved.layout,
            Some(SessionLayout {
                left_w: crate::ui::layout::MIN_LEFT_W,
                right_w: Some(app.layout.right_w),
                tasks: Some(SessionTasks::default()),
                ..layout
            })
        );
        assert_eq!(saved.expanded, [PathBuf::from("a"), PathBuf::from("a/deep")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_session_with_the_debug_window_round_trips() {
        let (mut app, dir) = crate::app::tests::test_app("session-debug-window");
        let mut session = empty_session(&app.workspace.root);
        let layout = session.layout.as_mut().unwrap();
        layout.bottom = Some(ToolWindowId::Debug);
        layout.order = vec![ToolWindowId::Debug, ToolWindowId::Git];
        let json = serde_json::to_string(&session).unwrap();
        let session: Session = serde_json::from_str(&json).unwrap();
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![]);
        assert_eq!(app.layout.bottom, Some(ToolWindowId::Debug));
        assert_eq!(&app.layout.order()[..2], [ToolWindowId::Debug, ToolWindowId::Git]);
        let saved = app.session().layout.unwrap();
        assert_eq!(saved.bottom, Some(ToolWindowId::Debug));
        assert_eq!(&saved.order[..2], [ToolWindowId::Debug, ToolWindowId::Git]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_session_without_the_debug_window_loads() {
        let (mut app, dir) = crate::app::tests::test_app("session-no-debug-window");
        let mut session = empty_session(&app.workspace.root);
        let others: Vec<ToolWindowId> = ToolWindowId::ALL.into_iter().filter(|t| *t != ToolWindowId::Debug).rev().collect();
        assert_eq!(others.len(), 13);
        session.layout.as_mut().unwrap().order = others.clone();
        // As an older build wrote it: the thirteen names it knew.
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("Debug"));
        let session: Session = serde_json::from_str(&json).unwrap();
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![]);
        assert_eq!(app.layout.order().last(), Some(&ToolWindowId::Debug));
        assert_eq!(&app.layout.order()[..13], others);
        assert!(!app.layout.is_visible(ToolWindowId::Debug));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn empty_session(root: &Path) -> Session {
        let layout = SessionLayout {
            left: None,
            left_w: 30,
            bottom: None,
            bottom_h: 10,
            git_tab: GitTab::Commit,
            show_hidden: true,
            show_ignored: false,
            right: None,
            right_w: None,
            sides: vec![],
            tasks: None,
            order: vec![],
            unpinned: vec![],
            split: vec![],
            left2: None,
            bottom2: None,
            right2: None,
            left2_h: None,
            bottom2_w: None,
            right2_h: None,
            undocked: vec![],
        };
        Session { root: root.to_path_buf(), layout: Some(layout), ..Default::default() }
    }

    #[tokio::test]
    async fn empty_session_focuses_the_project_tree() {
        let (mut app, dir) = crate::app::tests::test_app("session-empty-focus");
        let session = empty_session(&app.workspace.root);
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![]);
        assert!(app.focus.is(PanelId::Project));
        assert_eq!(app.layout.left, Some(ToolWindowId::Project));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_with_a_tab_keeps_the_editor_focused() {
        let (mut app, dir) = crate::app::tests::test_app("session-tab-focus");
        let root = app.workspace.root.clone();
        let path = root.join("a.txt");
        std::fs::write(&path, "x\n").unwrap();
        let session = Session { files: vec![SessionFile { path, ..Default::default() }], ..empty_session(&root) };
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![Ok(b"x\n".to_vec())]);
        assert_eq!(app.editor.buffers.len(), 1);
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pending_file_or_zen_keeps_the_focus() {
        let (mut app, dir) = crate::app::tests::test_app("session-pending-focus");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        app.pending_open = Some((root.join("b.txt"), 1, 1));
        let generation = app.workspace.generation;
        app.session_loaded(generation, empty_session(&root), vec![]);
        assert!(app.focus.is(PanelId::Editor));
        assert!(app.pending_open.is_none());
        let _ = std::fs::remove_dir_all(&dir);

        let (mut app, dir) = crate::app::tests::test_app("session-zen-focus");
        let root = app.workspace.root.clone();
        app.layout.enter_zen(PanelId::Editor);
        let generation = app.workspace.generation;
        app.session_loaded(generation, empty_session(&root), vec![]);
        assert!(app.layout.zen.is_some());
        assert!(app.focus.is(PanelId::Editor));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_round_trips_the_problems_window() {
        let (mut app, dir) = crate::app::tests::test_app("session-problems");
        app.toggle_tool_window(ToolWindowId::Problems);
        assert_eq!(app.layout.bottom, Some(ToolWindowId::Problems));
        let json = serde_json::to_string(&app.session()).unwrap();
        assert!(json.contains("\"Problems\""), "{json}");
        let session: Session = serde_json::from_str(&json).unwrap();
        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-problems-fresh");
        fresh.layout.hide_all();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, Session { root: fresh.workspace.root.clone(), ..session }, vec![]);
        assert_eq!(fresh.layout.bottom, Some(ToolWindowId::Problems));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[tokio::test]
    async fn session_round_trips_the_local_history_window_and_drops_the_history_tab() {
        let (mut app, dir) = crate::app::tests::test_app("session-local-history");
        app.toggle_tool_window(ToolWindowId::LocalHistory);
        let json = serde_json::to_string(&app.session()).unwrap();
        assert!(json.contains("\"LocalHistory\""), "{json}");
        // A session saved on the Git window's old History tab opens on Commit.
        let json = json.replace("\"git_tab\":\"Commit\"", "\"git_tab\":\"History\"");
        assert!(json.contains("\"History\""), "{json}");
        let session: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(session.layout.as_ref().map(|l| l.git_tab), Some(GitTab::Commit));
        let (mut fresh, fresh_dir) = crate::app::tests::test_app("session-local-history-fresh");
        fresh.layout.hide_all();
        let generation = fresh.workspace.generation;
        fresh.session_loaded(generation, Session { root: fresh.workspace.root.clone(), ..session }, vec![]);
        assert_eq!(fresh.layout.bottom, Some(ToolWindowId::LocalHistory));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
    }

    #[tokio::test]
    async fn session_saves_and_restores_sides() {
        let (mut app, dir) = crate::app::tests::test_app("session-sides");
        app.layout.move_to(ToolWindowId::Structure, Side::Bottom);
        app.layout.move_to(ToolWindowId::Git, Side::Right);
        app.layout.show(ToolWindowId::Git);
        let session = app.session();
        let l = session.layout.clone().unwrap();
        assert_eq!(l.right, Some(ToolWindowId::Git));
        assert_eq!(l.sides, vec![(ToolWindowId::Structure, Side::Bottom), (ToolWindowId::Git, Side::Right)]);
        app.layout.set_sides(&[]);
        app.layout.hide_all();
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![]);
        assert_eq!(app.layout.right, Some(ToolWindowId::Git));
        assert_eq!(app.layout.side_of(ToolWindowId::Structure), Side::Bottom);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_saves_and_restores_the_strip_order() {
        use ToolWindowId::*;
        let (mut app, dir) = crate::app::tests::test_app("session-order");
        app.layout.move_button(Git, true);
        let saved = app.session();
        assert_eq!(saved.layout.as_ref().unwrap().order, app.layout.order());
        let (mut other, dir2) = crate::app::tests::test_app("session-order2");
        let generation = other.workspace.generation;
        other.session_loaded(generation, saved, vec![]);
        assert_eq!(other.layout.order(), app.layout.order());
        let mut plain = app.session();
        plain.layout.as_mut().unwrap().order = vec![];
        let generation = app.workspace.generation;
        app.session_loaded(generation, plain, vec![]);
        assert_eq!(app.layout.order(), ToolWindowId::ALL);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[tokio::test]
    async fn session_never_saves_the_todo_window() {
        let (mut app, dir) = crate::app::tests::test_app("session-todo");
        app.layout.move_to(ToolWindowId::Todo, Side::Right);
        app.layout.show(ToolWindowId::Todo);
        let layout = app.session().layout.unwrap();
        let json = serde_json::to_string(&layout).unwrap();
        assert!(!json.contains("\"Todo\""), "{json}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_never_saves_the_hierarchy_window() {
        let (mut app, dir) = crate::app::tests::test_app("session-hierarchy");
        app.layout.move_to(ToolWindowId::Hierarchy, Side::Right);
        app.layout.show(ToolWindowId::Hierarchy);
        let layout = app.session().layout.unwrap();
        let json = serde_json::to_string(&layout).unwrap();
        assert!(!json.contains("\"Hierarchy\""), "{json}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_keeps_the_tasks_window_in_its_own_field() {
        let (mut app, dir) = crate::app::tests::test_app("session-tasks");
        app.layout.move_to(ToolWindowId::Tasks, Side::Right);
        app.layout.show(ToolWindowId::Tasks);
        app.tasks.restore(&SessionTasks { filter: "ui".into(), show_closed: true, selected: Some("3".into()), ..Default::default() });
        let session = app.session();
        let layout = session.layout.clone().unwrap();
        assert_eq!(layout.right, None);
        assert!(layout.sides.iter().all(|(tw, _)| *tw != ToolWindowId::Tasks));
        let want = SessionTasks {
            shown: true,
            side: Some(Side::Right),
            filter: "ui".into(),
            show_closed: true,
            selected: Some("3".into()),
            ..Default::default()
        };
        assert_eq!(layout.tasks, Some(want));
        // Builds without the window read the layout: no Tasks anywhere else.
        let json = serde_json::to_string(&SessionLayout { tasks: None, ..layout }).unwrap();
        assert!(!json.contains("\"Tasks\""), "{json}");
        app.layout.set_sides(&[]);
        app.layout.hide_all();
        app.tasks.reset();
        let generation = app.workspace.generation;
        app.session_loaded(generation, session, vec![]);
        assert_eq!(app.layout.right, Some(ToolWindowId::Tasks));
        assert_eq!(app.layout.side_of(ToolWindowId::Tasks), Side::Right);
        assert_eq!(app.tasks.session_state().filter, "ui");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_session_layout_loads_with_default_sides() {
        let old =
            r#"{"left":"Project","left_w":30,"bottom":null,"bottom_h":10,"git_tab":"Commit","show_hidden":false,"show_ignored":false}"#;
        let l: SessionLayout = serde_json::from_str(old).unwrap();
        assert_eq!((l.right, l.right_w, l.sides.is_empty()), (None, None, true));
        assert!(l.tasks.is_none());
        let new = SessionLayout { right: Some(ToolWindowId::Git), right_w: Some(20), sides: vec![(ToolWindowId::Git, Side::Right)], ..l };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<SessionLayout>(&json).unwrap(), new);
    }

    #[test]
    fn session_layout_split_fields_are_optional() {
        let old =
            r#"{"left":"Project","left_w":30,"bottom":null,"bottom_h":10,"git_tab":"Commit","show_hidden":false,"show_ignored":false}"#;
        let l: SessionLayout = serde_json::from_str(old).unwrap();
        assert!(l.split.is_empty());
        assert_eq!((l.left2, l.bottom2, l.right2, l.left2_h, l.bottom2_w, l.right2_h), (None, None, None, None, None, None));
        let json = serde_json::to_string(&l).unwrap();
        for key in ["\"split\"", "\"left2\"", "\"bottom2_w\""] {
            assert!(!json.contains(key), "{key} in {json}");
        }
        let new = SessionLayout {
            split: vec![ToolWindowId::Structure, ToolWindowId::Find],
            left2: Some(ToolWindowId::Structure),
            bottom2: Some(ToolWindowId::Find),
            right2: Some(ToolWindowId::Git),
            left2_h: Some(7),
            bottom2_w: Some(40),
            right2_h: Some(9),
            ..l
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<SessionLayout>(&json).unwrap(), new);
        let newer = json.replace(r#""split":["Structure","#, r#""split":["Nope","Structure","#);
        assert_eq!(serde_json::from_str::<SessionLayout>(&newer).unwrap().split, [ToolWindowId::Structure, ToolWindowId::Find]);
    }

    #[tokio::test]
    async fn session_saves_and_restores_split_docks() {
        use crate::ui::focus::Dock;
        let (mut app, dir) = crate::app::tests::test_app("session-split");
        let dock = |side, split| Dock { side, split };
        app.layout.move_to_dock(ToolWindowId::Structure, dock(Side::Left, true));
        app.layout.show(ToolWindowId::Structure);
        app.layout.move_to_dock(ToolWindowId::Tasks, dock(Side::Bottom, true));
        app.layout.show(ToolWindowId::Tasks);
        app.layout.move_to_dock(ToolWindowId::Todo, dock(Side::Right, true));
        app.layout.show(ToolWindowId::Todo);
        app.layout.left2_h = 9;
        let session = app.session();
        let layout = session.layout.clone().unwrap();
        assert_eq!(layout.split, [ToolWindowId::Structure]);
        assert_eq!((layout.left2, layout.bottom2, layout.right2), (Some(ToolWindowId::Structure), None, None));
        assert_eq!((layout.left2_h, layout.bottom2_w), (Some(9), None));
        let tasks = layout.tasks.clone().unwrap();
        assert!(tasks.split && tasks.shown);
        assert!(layout.sides.is_empty(), "a split on the default side writes no side");
        let json = serde_json::to_string(&SessionLayout { tasks: None, ..layout }).unwrap();
        assert!(!json.contains("\"Todo\"") && !json.contains("\"Tasks\""), "{json}");
        let (mut other, dir2) = crate::app::tests::test_app("session-split2");
        let generation = other.workspace.generation;
        other.session_loaded(generation, session, vec![]);
        assert!(other.layout.dock_of(ToolWindowId::Structure).split);
        assert_eq!(other.layout.left2, Some(ToolWindowId::Structure));
        assert_eq!(other.layout.bottom2, Some(ToolWindowId::Tasks));
        assert_eq!((other.layout.left2_h, other.layout.bottom2_w), (9, 0));
        // A split on another side keeps its side entry for older builds.
        app.layout.move_to_dock(ToolWindowId::Git, dock(Side::Right, true));
        let layout = app.session().layout.unwrap();
        assert_eq!(layout.sides, [(ToolWindowId::Git, Side::Right)]);
        assert_eq!(layout.split, [ToolWindowId::Structure, ToolWindowId::Git]);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[tokio::test]
    async fn session_keeps_unpinned_windows() {
        use crate::ui::layout::ViewMode;
        let (mut app, dir) = crate::app::tests::test_app("session-unpinned");
        app.layout.set_view_mode(ToolWindowId::Find, ViewMode::Unpinned);
        let saved = app.session();
        let (mut other, dir2) = crate::app::tests::test_app("session-unpinned2");
        let generation = other.workspace.generation;
        other.session_loaded(generation, saved, vec![]);
        assert_eq!(other.layout.view_mode(ToolWindowId::Find), ViewMode::Unpinned);
        assert_eq!(other.layout.view_mode(ToolWindowId::Git), ViewMode::Pinned);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[tokio::test]
    async fn session_keeps_undocked_windows() {
        use crate::ui::layout::ViewMode;
        let (mut app, dir) = crate::app::tests::test_app("session-undocked");
        app.layout.show(ToolWindowId::Git);
        app.layout.set_view_mode(ToolWindowId::Git, ViewMode::Undocked);
        app.layout.set_view_mode(ToolWindowId::Find, ViewMode::Unpinned);
        let saved = app.session();
        let (mut other, dir2) = crate::app::tests::test_app("session-undocked2");
        let generation = other.workspace.generation;
        other.session_loaded(generation, saved, vec![]);
        assert_eq!(other.layout.view_mode(ToolWindowId::Git), ViewMode::Undocked);
        assert!(other.layout.is_visible(ToolWindowId::Git));
        assert_eq!(other.layout.view_mode(ToolWindowId::Find), ViewMode::Unpinned);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn session_layout_from_0_8_0_loads_with_default_order() {
        // As 0.8.0 writes it.
        let old = r#"{"left":"Project","left_w":30,"bottom":"Git","bottom_h":12,"git_tab":"Log","show_hidden":false,"show_ignored":true,"right_w":30,"sides":[["Find","Right"]],"tasks":{"shown":false,"show_closed":false}}"#;
        let l: SessionLayout = serde_json::from_str(old).unwrap();
        assert!(l.order.is_empty() && l.unpinned.is_empty() && l.undocked.is_empty());
        let mut state = crate::ui::layout::LayoutState::new(30, 10);
        state.set_order(&l.order);
        assert_eq!(state.order(), ToolWindowId::ALL);
        assert!(!serde_json::to_string(&l).unwrap().contains("\"order\""), "the default order is not written");
        assert!(!serde_json::to_string(&l).unwrap().contains("\"unpinned\""));
        let new = SessionLayout { order: vec![ToolWindowId::Git, ToolWindowId::Project], unpinned: vec![ToolWindowId::Git], ..l };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<SessionLayout>(&json).unwrap(), new);
        // A window this build does not know is dropped, not an error.
        let newer = json.replace(r#""order":["Git","#, r#""order":["Git","Nope","#);
        assert_eq!(serde_json::from_str::<SessionLayout>(&newer).unwrap().order, [ToolWindowId::Git, ToolWindowId::Project]);
        let newer = json.replace(r#""unpinned":["Git"#, r#""unpinned":["Nope","Git"#);
        assert_eq!(serde_json::from_str::<SessionLayout>(&newer).unwrap().unpinned, [ToolWindowId::Git]);
        assert!(!json.contains("\"undocked\""));
        let undocked = SessionLayout { undocked: vec![ToolWindowId::Find], ..new };
        let json = serde_json::to_string(&undocked).unwrap();
        assert_eq!(serde_json::from_str::<SessionLayout>(&json).unwrap(), undocked);
        let newer = json.replace(r#""undocked":["Find"#, r#""undocked":["Nope","Find"#);
        assert_eq!(serde_json::from_str::<SessionLayout>(&newer).unwrap().undocked, [ToolWindowId::Find]);
    }

    #[test]
    fn named_layout_placement_matches_the_session_layout() {
        use ToolWindowId::*;
        let l = SessionLayout {
            left: Some(Project),
            left_w: 31,
            bottom: Some(Run),
            bottom_h: 9,
            git_tab: GitTab::Console,
            show_hidden: true,
            show_ignored: true,
            right: Some(Git),
            right_w: Some(22),
            sides: vec![(Git, Side::Right), (Structure, Side::Left)],
            tasks: Some(SessionTasks::default()),
            order: vec![Git, Project],
            unpinned: vec![Find],
            split: vec![Structure],
            left2: Some(Structure),
            bottom2: Some(Problems),
            right2: Some(EventLog),
            left2_h: Some(5),
            bottom2_w: Some(20),
            right2_h: Some(6),
            undocked: vec![Terminal],
        };
        let p = l.placement();
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(serde_json::from_value::<crate::layouts::LayoutPlacement>(value.clone()).unwrap(), p);
        let mut session = serde_json::to_value(&l).unwrap();
        let obj = session.as_object_mut().unwrap();
        for k in ["git_tab", "show_hidden", "show_ignored", "tasks"] {
            assert!(obj.remove(k).is_some(), "{k}");
        }
        assert_eq!(value, session, "a placement field was added to one struct only");
    }
}
