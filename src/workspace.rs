//! The project root the IDE is currently showing, switching it (worktrees),
//! and per-root sessions: open tabs in order with their pins, caret and
//! scroll, the split editor's groups, tool window layout and expanded tree
//! folders, stored as JSON in `paths.sessions_dir()/<fnv hash of root>.json`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::editor::split::{Axis, SplitNode};
use crate::event::{AppMsg, Level};
use crate::git::panel::GitTab;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::{PanelId, ToolWindowId};

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
    /// Plain bookmarks of files without a tab (a field of its own, so
    /// sessions stay readable by builds that know only numbered ones).
    #[serde(default)]
    pub closed_plain_bookmarks: Vec<SessionClosedFile>,
}

/// The plain bookmarks of a file without a tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionClosedFile {
    pub path: PathBuf,
    /// Bookmarked lines, 0-based.
    pub lines: Vec<usize>,
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
        // The resolver's work is not kept across roots.
        self.editor.merge_close();
        self.format.abort_all();
        self.run_root_changed();
        // The shell belongs to the old root; the next one starts in the new.
        self.terminal.reset();
        // So do the language servers; new ones start for the open buffers.
        self.lsp_root_changed();
        if self.config.worktree.switch_closes_tabs {
            self.editor.force_close_all();
        }
        let old = self.workspace.switch_root(root.clone());
        tracing::info!("switch root {} -> {}", old.display(), root.display());
        // Points git at the new root and generation, resets the Git panel,
        // and requests status and branches.
        self.git_root_changed(root.clone());
        self.start_watcher();
        self.find.reset();
        self.symbols.reset();
        self.worktrees.reset();
        self.git.load_worktrees();
        self.project.reroot(root.clone());
        // The palette lists the old root; close it and re-index.
        self.palette = None;
        self.refresh_file_index();
        crate::tui::set_title(&root);
        self.editor.clear_nav();
        self.pending_revert.clear();
        self.history_root_changed();
        // The closed files' numbered bookmarks come with the new session.
        self.editor.restore_closed_bookmarks(&[]);
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

    /// The open tabs of the current root as a session, in tab order.
    pub fn session(&self) -> Session {
        let root = self.workspace.root.clone();
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
                pinned: pinned.contains(&b.id),
                folds: b.view.folds.to_session(&b.rope),
                wrap: (b.view.wrap.on != self.editor.soft_wrap_for(b.path.as_deref())).then_some(b.view.wrap.on),
            });
        }
        let recent = self.editor.recent_paths().iter().filter_map(|p| files.iter().position(|f| &f.path == p)).collect();
        let (show_hidden, show_ignored) = self.project.visibility();
        // Zen mode is not saved: the windows it hides are.
        let (left, bottom) = self.layout.saved_windows();
        let layout = Some(SessionLayout {
            left,
            left_w: self.layout.left_w,
            bottom,
            bottom_h: self.layout.bottom_h,
            git_tab: self.git_panel.tab,
            show_hidden,
            show_ignored,
        });
        let expanded = self.project.expanded_dirs().iter().filter_map(|d| d.strip_prefix(&root).ok()).map(Path::to_path_buf).collect();
        let splits = self.session_splits(&files);
        let closed_bookmarks =
            self.editor.closed_digits().into_iter().map(|(digit, path, line)| SessionBookmark { digit, path, line }).collect();
        let mut closed_plain_bookmarks: Vec<SessionClosedFile> = Vec::new();
        // By path then line, so one entry per file.
        for (path, line, _) in self.editor.closed_bookmarks().into_iter().filter(|(_, _, d)| d.is_none()) {
            match closed_plain_bookmarks.last_mut() {
                Some(f) if f.path == path => f.lines.push(line),
                _ => closed_plain_bookmarks.push(SessionClosedFile { path, lines: vec![line] }),
            }
        }
        Session { root, files, active, recent, layout, expanded, splits, closed_bookmarks, closed_plain_bookmarks }
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

    /// Save the current root's session (on switch and quit). Failures are
    /// logged only.
    pub fn save_session(&self) {
        if let Err(e) = self.session().save(&self.paths.sessions_dir()) {
            tracing::warn!("saving session: {e:#}");
        }
    }

    /// Load the current root's session and its files off the main loop; the
    /// tabs open in [`App::session_loaded`]. No session arrives as an empty
    /// one, so a pending command line file still opens.
    pub fn restore_session(&self) {
        let (dir, root, generation, tx) =
            (self.paths.sessions_dir(), self.workspace.root.clone(), self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let session = Session::load(&dir, &root).unwrap_or_else(|| Session { root, ..Default::default() });
            let files = session.files.iter().map(|f| std::fs::read(&f.path)).collect();
            let _ = tx.send(AppMsg::SessionLoaded { generation, session, files });
        });
    }

    /// Open the tabs of a loaded session (dropped when the root changed
    /// since), restore its layout and tree folders, then open the pending
    /// command line file and offer the root's crash dumps. Missing files and
    /// folders, and files already open, are skipped.
    pub fn session_loaded(&mut self, generation: u64, session: Session, files: Vec<std::io::Result<Vec<u8>>>) {
        if generation != self.workspace.generation {
            tracing::debug!("dropping stale session for {}", session.root.display());
            return;
        }
        for (f, result) in session.files.iter().zip(files) {
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
            if f.pinned {
                self.editor.set_pinned(i, true);
            }
            self.git.load_blob(f.path.clone(), crate::git::BlobWhich::Head);
        }
        if let Some(splits) = &session.splits {
            let buffers: Vec<Option<usize>> = session.files.iter().map(|f| self.editor.index_of(&f.path)).collect();
            self.editor.restore_splits(splits, &buffers);
        }
        let recent: Vec<PathBuf> = session.recent.iter().filter_map(|i| session.files.get(*i)).map(|f| f.path.clone()).collect();
        let active = session.active.and_then(|i| session.files.get(i)).map(|f| f.path.as_path());
        self.editor.restore_recent(&recent, active);
        // Applied once recency is known, so the limit keeps the recent tabs.
        self.editor.enforce_tab_limit();
        // Opening the tabs is not navigation.
        self.editor.clear_nav();
        let numbered = session.closed_bookmarks.iter().map(|b| (b.path.clone(), b.line, Some(b.digit)));
        let plain = session.closed_plain_bookmarks.iter().flat_map(|f| f.lines.iter().map(|&l| (f.path.clone(), l, None)));
        let closed: Vec<_> = numbered.chain(plain).collect();
        self.editor.restore_closed_bookmarks(&closed);
        if let Some(l) = &session.layout {
            self.layout.restore(l.left, l.left_w, l.bottom, l.bottom_h);
            if let Some(tw) = self.focus.current.tool_window()
                && !self.layout.is_visible(tw)
            {
                self.focus.set(PanelId::Editor);
            }
            self.set_git_tab(l.git_tab);
            self.project.set_visibility(l.show_hidden, l.show_ignored);
        }
        let expanded: Vec<PathBuf> = session.expanded.iter().map(|d| session.root.join(d)).collect();
        self.project.expand_paths(&expanded);
        if let Some((path, line, col)) = self.pending_open.take() {
            self.open_file_at(path, line.saturating_sub(1), col.saturating_sub(1));
        }
        self.offer_recovery();
        self.dirty_frame = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    bookmarks: vec![3, 40],
                    bookmark_digits: vec![(7, 40)],
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
            closed_plain_bookmarks: vec![SessionClosedFile { path: root.join("src/lib.rs"), lines: vec![3, 40] }],
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
        assert!(old.files[0].bookmark_digits.is_empty() && old.closed_bookmarks.is_empty() && old.closed_plain_bookmarks.is_empty());
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
        app.editor.set_bookmark_digit(id, 1, 5);
        let gone = app.workspace.root.join("gone.txt");
        app.editor.restore_closed_bookmarks(&[(gone.clone(), 4, Some(2)), (gone.clone(), 1, None), (gone.clone(), 7, None)]);
        let b = &mut app.editor.buffers[0];
        let fold = crate::editor::fold::Fold { start: 0, end: b.rope.line_to_char(2) + 3 };
        assert!(b.view.folds.collapse(fold, &b.rope));
        b.goto_line(3, 0);
        let session = app.session();
        assert_eq!(session.files[0].bookmarks, vec![1, 3]);
        assert_eq!(session.files[0].bookmark_digits, vec![(5, 1)]);
        assert_eq!(session.closed_bookmarks, vec![SessionBookmark { digit: 2, path: gone.clone(), line: 4 }]);
        assert_eq!(session.closed_plain_bookmarks, vec![SessionClosedFile { path: gone.clone(), lines: vec![1, 7] }]);
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
        assert_eq!(fresh.editor.closed_bookmarks(), vec![(gone.clone(), 1, None), (gone.clone(), 4, Some(2)), (gone, 7, None)]);
        assert_eq!(b.view.folds.hidden().first(), Some(&(1..3)), "the fold is back");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh_dir);
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
        assert_eq!(b.bookmarks.iter().map(|(l, d)| (*l, *d)).collect::<Vec<_>>(), vec![(1, None), (2, None)]);
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
        let layout = SessionLayout {
            left: None,
            left_w: 0,
            bottom: Some(ToolWindowId::Git),
            bottom_h: 9,
            git_tab: GitTab::Console,
            show_hidden: false,
            show_ignored: false,
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
        assert_eq!(saved.layout, Some(SessionLayout { left_w: crate::ui::layout::MIN_LEFT_W, ..layout }));
        assert_eq!(saved.expanded, [PathBuf::from("a"), PathBuf::from("a/deep")]);
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
}
