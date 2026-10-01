//! Project tool window: a lazily loaded, gitignore-aware file tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::actions::TreeAction;
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::git::model::FileStatus;
use crate::ui::tool_window_block;
use crate::ui::widgets::list::ListState;

#[derive(Debug, Clone)]
struct Node {
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

pub struct ProjectPanel {
    root: PathBuf,
    nodes: Vec<Node>,
    rows: Vec<Row>,
    pub list: ListState,
    inner: Rect,
    show_hidden: bool,
    show_ignored: bool,
    status: HashMap<PathBuf, FileStatus>,
}

fn read_children(dir: &Path, show_hidden: bool, show_ignored: bool) -> Vec<Node> {
    let mut out = Vec::new();
    let mut wb = ignore::WalkBuilder::new(dir);
    wb.max_depth(Some(1)).hidden(!show_hidden).git_ignore(!show_ignored).git_global(!show_ignored).git_exclude(!show_ignored).follow_links(false).require_git(true);
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
        let mut p = Self { root: PathBuf::new(), nodes: Vec::new(), rows: Vec::new(), list: ListState::default(), inner: Rect::default(), show_hidden, show_ignored, status: HashMap::new() };
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

    /// Re-read every expanded directory, keeping expansion state.
    pub fn refresh(&mut self) {
        fn reload(nodes: &mut Vec<Node>, dir: &Path, show_hidden: bool, show_ignored: bool) {
            let old: HashMap<PathBuf, Node> = nodes.drain(..).map(|n| (n.path.clone(), n)).collect();
            let mut fresh = read_children(dir, show_hidden, show_ignored);
            for n in &mut fresh {
                if let Some(o) = old.get(&n.path) {
                    if o.is_dir && o.expanded {
                        n.expanded = true;
                        n.children = Some(Vec::new());
                        let mut kids = o.children.clone().unwrap_or_default();
                        reload(&mut kids, &n.path, show_hidden, show_ignored);
                        n.children = Some(kids);
                    }
                }
            }
            *nodes = fresh;
        }
        let root = self.root.clone();
        let mut nodes = std::mem::take(&mut self.nodes);
        reload(&mut nodes, &root, self.show_hidden, self.show_ignored);
        self.nodes = nodes;
        self.reflatten();
    }

    fn reflatten(&mut self) {
        fn walk(nodes: &[Node], depth: usize, out: &mut Vec<Row>) {
            for n in nodes {
                out.push(Row { path: n.path.clone(), name: n.name.clone(), is_dir: n.is_dir, expanded: n.expanded, depth });
                if n.is_dir && n.expanded {
                    if let Some(c) = &n.children {
                        walk(c, depth + 1, out);
                    }
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
                if n.is_dir && path.starts_with(&n.path) {
                    if let Some(c) = &mut n.children {
                        if let Some(f) = find(c, path) {
                            return Some(f);
                        }
                    }
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
                if let Some(r) = self.selected_row().cloned() {
                    if r.is_dir {
                        self.set_expanded(&r.path, !r.expanded);
                    }
                }
            }
            TreeAction::Open => self.open_or_toggle(cx),
            TreeAction::Refresh => self.refresh(),
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
            let _ = r.name.width();
        }
    }
}
