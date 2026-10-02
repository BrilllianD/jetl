//! Project tool window: a lazily loaded, gitignore-aware file tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::actions::TreeAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::git::model::FileStatus;
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;

#[derive(Debug, Clone)]
pub struct Node {
    path: PathBuf,
    name: String,
    is_dir: bool,
    expanded: bool,
    children: Option<Vec<Node>>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub expanded: bool,
    pub depth: usize,
}

/// Children of each directory a refresh re-read, sorted for display.
pub type DirListing = HashMap<PathBuf, Vec<Node>>;

/// The directories a refresh re-reads (the root and every expanded one),
/// detached from the panel so the walk can run off the main thread.
#[derive(Debug)]
pub struct TreeRefresh {
    pub seq: u64,
    dirs: Vec<PathBuf>,
    show_hidden: bool,
    show_ignored: bool,
}

impl TreeRefresh {
    pub fn run(&self) -> DirListing {
        self.dirs.iter().map(|d| (d.clone(), read_children(d, self.show_hidden, self.show_ignored))).collect()
    }
}

pub struct ProjectPanel {
    root: PathBuf,
    nodes: Vec<Node>,
    rows: Vec<Row>,
    pub list: ListState,
    inner: Rect,
    show_hidden: bool,
    show_ignored: bool,
    status: HashMap<PathBuf, FileStatus>,
    /// Last refresh requested and last one applied.
    refresh_seq: u64,
    applied_seq: u64,
}

fn read_children(dir: &Path, show_hidden: bool, show_ignored: bool) -> Vec<Node> {
    let mut out = Vec::new();
    let mut wb = ignore::WalkBuilder::new(dir);
    wb.max_depth(Some(1))
        .hidden(!show_hidden)
        .git_ignore(!show_ignored)
        .git_global(!show_ignored)
        .git_exclude(!show_ignored)
        .follow_links(false)
        .require_git(true);
    for entry in wb.build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path().to_path_buf();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(Node { path, name, is_dir, expanded: false, children: None });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

impl ProjectPanel {
    pub fn new(root: PathBuf, show_hidden: bool, show_ignored: bool) -> Self {
        let mut p = Self {
            root: PathBuf::new(),
            nodes: Vec::new(),
            rows: Vec::new(),
            list: ListState::default(),
            inner: Rect::default(),
            show_hidden,
            show_ignored,
            status: HashMap::new(),
            refresh_seq: 0,
            applied_seq: 0,
        };
        p.reroot(root);
        p
    }

    pub fn reroot(&mut self, root: PathBuf) {
        self.root = root;
        self.nodes = read_children(&self.root, self.show_hidden, self.show_ignored);
        self.list = ListState::default();
        self.reflatten();
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_git_status(&mut self, status: HashMap<PathBuf, FileStatus>) {
        self.status = status;
    }

    /// Start a refresh: the root and every expanded directory, to be read
    /// with [`TreeRefresh::run`] and merged by [`ProjectPanel::finish_refresh`].
    pub fn begin_refresh(&mut self) -> TreeRefresh {
        fn expanded(nodes: &[Node], out: &mut Vec<PathBuf>) {
            for n in nodes.iter().filter(|n| n.is_dir && n.expanded) {
                out.push(n.path.clone());
                expanded(n.children.as_deref().unwrap_or_default(), out);
            }
        }
        let mut dirs = vec![self.root.clone()];
        expanded(&self.nodes, &mut dirs);
        self.refresh_seq += 1;
        TreeRefresh { seq: self.refresh_seq, dirs, show_hidden: self.show_hidden, show_ignored: self.show_ignored }
    }

    /// Merge re-read directories, keeping expansion and the selected path.
    /// Directories expanded after the request keep the children they
    /// loaded. Returns false for a refresh older than the last applied.
    pub fn finish_refresh(&mut self, seq: u64, mut listing: DirListing) -> bool {
        fn merge(old: Vec<Node>, dir: &Path, listing: &mut DirListing) -> Vec<Node> {
            let Some(mut fresh) = listing.remove(dir) else {
                // Not re-read: keep as is, but descend for deeper listings.
                return old
                    .into_iter()
                    .map(|mut n| {
                        if n.expanded {
                            n.children = n.children.map(|c| merge(c, &n.path, listing));
                        }
                        n
                    })
                    .collect();
            };
            let mut old: HashMap<PathBuf, Node> = old.into_iter().map(|n| (n.path.clone(), n)).collect();
            for n in &mut fresh {
                if let Some(o) = old.remove(&n.path)
                    && o.is_dir
                    && n.is_dir
                    && o.expanded
                {
                    n.expanded = true;
                    n.children = o.children.map(|c| merge(c, &n.path, listing));
                }
            }
            fresh
        }
        if seq < self.applied_seq {
            return false;
        }
        self.applied_seq = seq;
        let selected = self.selected_row().map(|r| r.path.clone());
        let nodes = std::mem::take(&mut self.nodes);
        self.nodes = merge(nodes, &self.root.clone(), &mut listing);
        self.reflatten();
        if let Some(i) = selected.and_then(|p| self.rows.iter().position(|r| r.path == p)) {
            self.list.select(i);
        }
        true
    }

    #[cfg(test)]
    pub fn refresh_requests(&self) -> u64 {
        self.refresh_seq
    }

    /// Re-read every expanded directory on this thread.
    #[cfg(test)]
    pub fn refresh(&mut self) {
        let req = self.begin_refresh();
        let listing = req.run();
        self.finish_refresh(req.seq, listing);
    }

    fn reflatten(&mut self) {
        fn walk(nodes: &[Node], depth: usize, out: &mut Vec<Row>) {
            for n in nodes {
                out.push(Row { path: n.path.clone(), name: n.name.clone(), is_dir: n.is_dir, expanded: n.expanded, depth });
                if n.is_dir
                    && n.expanded
                    && let Some(c) = &n.children
                {
                    walk(c, depth + 1, out);
                }
            }
        }
        let mut rows = Vec::new();
        walk(&self.nodes, 0, &mut rows);
        self.rows = rows;
        self.list.set_len(self.rows.len());
    }

    fn node_mut(&mut self, path: &Path) -> Option<&mut Node> {
        fn find<'a>(nodes: &'a mut [Node], path: &Path) -> Option<&'a mut Node> {
            for n in nodes.iter_mut() {
                if n.path == path {
                    return Some(n);
                }
                if n.is_dir
                    && path.starts_with(&n.path)
                    && let Some(c) = &mut n.children
                    && let Some(f) = find(c, path)
                {
                    return Some(f);
                }
            }
            None
        }
        find(&mut self.nodes, path)
    }

    fn set_expanded(&mut self, path: &Path, expanded: bool) {
        let (sh, si) = (self.show_hidden, self.show_ignored);
        if let Some(n) = self.node_mut(path) {
            if !n.is_dir {
                return;
            }
            n.expanded = expanded;
            if expanded && n.children.is_none() {
                n.children = Some(read_children(&n.path, sh, si));
            }
        }
        self.reflatten();
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.list.selected)
    }

    /// Expand ancestors of `path` and select it.
    pub fn select_path(&mut self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else { return false };
        let mut cur = self.root.clone();
        let comps: Vec<_> = rel.components().collect();
        for (i, c) in comps.iter().enumerate() {
            cur.push(c);
            if i + 1 < comps.len() {
                self.set_expanded(&cur.clone(), true);
            }
        }
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.list.select(i);
            true
        } else {
            false
        }
    }

    fn open_or_toggle(&mut self, cx: &mut Ctx) {
        let Some(r) = self.selected_row().cloned() else { return };
        if r.is_dir {
            self.set_expanded(&r.path, !r.expanded);
        } else {
            cx.open_file(r.path);
        }
    }

    pub fn handle_action(&mut self, a: &TreeAction, cx: &mut Ctx) -> bool {
        let page = self.list.viewport.max(1);
        match a {
            TreeAction::Up => self.list.up(1),
            TreeAction::Down => self.list.down(1),
            TreeAction::PageUp => self.list.up(page),
            TreeAction::PageDown => self.list.down(page),
            TreeAction::Home => self.list.home(),
            TreeAction::End => self.list.end(),
            TreeAction::Expand => {
                if let Some(r) = self.selected_row().cloned() {
                    if r.is_dir && !r.expanded {
                        self.set_expanded(&r.path, true);
                    } else if r.is_dir {
                        self.list.down(1);
                    }
                }
            }
            TreeAction::Collapse => {
                if let Some(r) = self.selected_row().cloned() {
                    if r.is_dir && r.expanded {
                        self.set_expanded(&r.path, false);
                    } else {
                        self.go_parent();
                    }
                }
            }
            TreeAction::Parent => self.go_parent(),
            TreeAction::Toggle => {
                if let Some(r) = self.selected_row().cloned()
                    && r.is_dir
                {
                    self.set_expanded(&r.path, !r.expanded);
                }
            }
            TreeAction::Open => self.open_or_toggle(cx),
            TreeAction::Refresh => cx.effects.push(crate::event::Effect::RefreshTree),
            TreeAction::SelectInProject => return false,
        }
        true
    }

    fn go_parent(&mut self) {
        let Some(r) = self.selected_row() else { return };
        let depth = r.depth;
        if depth == 0 {
            return;
        }
        let mut i = self.list.selected;
        while i > 0 {
            i -= 1;
            if self.rows[i].depth < depth {
                self.list.select(i);
                return;
            }
        }
    }

    pub fn handle_mouse(&mut self, m: &Mouse, _area: Rect, cx: &mut Ctx) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(i) = self.list.hit(self.inner, m.x, m.y) else { return false };
                self.list.select(i);
                let r = self.rows[i].clone();
                let arrow_x = self.inner.x + (r.depth * 2) as u16;
                let on_arrow = r.is_dir && m.x >= arrow_x && m.x < arrow_x + 2;
                if m.clicks >= 2 || on_arrow {
                    if r.is_dir {
                        self.set_expanded(&r.path, !r.expanded);
                    } else {
                        cx.open_file(r.path);
                    }
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.list.scroll(-3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.list.scroll(3);
                true
            }
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        let title = format!(" Project: {} ", self.root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
        let block = tool_window_block(&title, focused, theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.inner = inner;
        self.list.set_viewport(inner.height as usize);
        let screen = frame.buffer_mut();
        for (vi, i) in self.list.visible().enumerate() {
            let r = &self.rows[i];
            let y = inner.y + vi as u16;
            let selected = i == self.list.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                screen.set_style(Rect::new(inner.x, y, inner.width, 1), base);
            }
            let indent = (r.depth * 2) as u16;
            let mut x = inner.x + indent;
            if x >= inner.right() {
                continue;
            }
            let glyph = if r.is_dir { if r.expanded { "▾ " } else { "▸ " } } else { "  " };
            screen.set_stringn(x, y, glyph, (inner.right() - x) as usize, base.add_modifier(Modifier::DIM));
            x += 2;
            let mut name_style = base;
            if let Some(st) = self.status.get(&r.path) {
                let s = st.style(theme);
                if let Some(fg) = s.fg {
                    name_style = name_style.fg(fg);
                }
            } else if r.is_dir {
                name_style = name_style.add_modifier(Modifier::BOLD);
            }
            if x < inner.right() {
                screen.set_stringn(x, y, &r.name, (inner.right() - x) as usize, name_style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-tree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["a/deep", "b", "c"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in ["a/deep/x.txt", "a/one.txt", "b/two.txt", "c/three.txt", "z.txt"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::canonicalize(&dir).unwrap()
    }

    fn names(p: &ProjectPanel) -> Vec<String> {
        p.rows.iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.name)).collect()
    }

    #[test]
    fn refresh_keeps_expansion_and_selected_path() {
        let root = tmp_tree("keep");
        let mut p = ProjectPanel::new(root.clone(), false, false);
        p.select_path(&root.join("a/deep/x.txt"));
        assert_eq!(names(&p), ["a", "  deep", "    x.txt", "  one.txt", "b", "c", "z.txt"]);
        // A new entry sorts in above the selection.
        std::fs::write(root.join("a/deep/w.txt"), "").unwrap();
        p.refresh();
        assert_eq!(names(&p), ["a", "  deep", "    w.txt", "    x.txt", "  one.txt", "b", "c", "z.txt"]);
        assert_eq!(p.selected_row().unwrap().path, root.join("a/deep/x.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn late_or_overtaken_refresh_merges_safely() {
        let root = tmp_tree("late");
        let mut p = ProjectPanel::new(root.clone(), false, false);
        let old = p.begin_refresh();
        let old_listing = old.run();
        // Expanded after the request: keeps the children it loaded.
        p.select_path(&root.join("b/two.txt"));
        let new = p.begin_refresh();
        std::fs::write(root.join("b/new.txt"), "").unwrap();
        let new_listing = new.run();
        assert!(p.finish_refresh(new.seq, new_listing));
        assert_eq!(names(&p), ["a", "b", "  new.txt", "  two.txt", "c", "z.txt"]);
        assert!(!p.finish_refresh(old.seq, old_listing), "older than the applied one");
        assert_eq!(names(&p), ["a", "b", "  new.txt", "  two.txt", "c", "z.txt"]);
        // A listing without `c` (requested before it was expanded) leaves it.
        let req = p.begin_refresh();
        p.select_path(&root.join("c/three.txt"));
        p.finish_refresh(req.seq, req.run());
        assert_eq!(names(&p), ["a", "b", "  new.txt", "  two.txt", "c", "  three.txt", "z.txt"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
