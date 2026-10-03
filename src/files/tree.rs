//! Project tool window: a lazily loaded, gitignore-aware file tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::actions::{Action, TreeAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level, Mouse};
use crate::git::model::FileStatus;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::menu::{ContextMenu, MenuItem};
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
    /// Speed search: typed text narrows the rows to matching names (and
    /// their folders) among the loaded entries.
    filter: String,
    /// At most one refresh walk at a time.
    refresh: crate::util::coalesce::Coalesce,
    /// Last refresh requested and last one applied.
    refresh_seq: u64,
    applied_seq: u64,
}

fn read_children(dir: &Path, show_hidden: bool, show_ignored: bool) -> Vec<Node> {
    let mut out = Vec::new();
    for entry in crate::files::walk::builder(dir, show_hidden, show_ignored).max_depth(Some(1)).build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path().to_path_buf();
        let name = entry.file_name().to_string_lossy().into_owned();
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
            filter: String::new(),
            refresh: Default::default(),
            refresh_seq: 0,
            applied_seq: 0,
        };
        p.reroot(root);
        p
    }

    pub fn reroot(&mut self, root: PathBuf) {
        self.root = root;
        self.refresh.reset();
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
    /// `None` while one is running; it then runs again when that finishes.
    pub fn begin_refresh(&mut self) -> Option<TreeRefresh> {
        self.refresh.request().then(|| self.refresh_request())
    }

    fn refresh_request(&mut self) -> TreeRefresh {
        let mut dirs = vec![self.root.clone()];
        dirs.extend(self.expanded_dirs());
        self.refresh_seq += 1;
        TreeRefresh { seq: self.refresh_seq, dirs, show_hidden: self.show_hidden, show_ignored: self.show_ignored }
    }

    /// The expanded directories the tree shows, parents first. One inside a
    /// collapsed folder is left out.
    pub fn expanded_dirs(&self) -> Vec<PathBuf> {
        fn walk(nodes: &[Node], out: &mut Vec<PathBuf>) {
            for n in nodes.iter().filter(|n| n.is_dir && n.expanded) {
                out.push(n.path.clone());
                walk(n.children.as_deref().unwrap_or_default(), out);
            }
        }
        let mut out = Vec::new();
        walk(&self.nodes, &mut out);
        out
    }

    /// Expand each directory in `dirs` (absolute) and its ancestors, reading
    /// listings on this thread as needed (session restore). Missing
    /// directories are skipped; the selected path is kept.
    pub fn expand_paths(&mut self, dirs: &[PathBuf]) {
        let (sh, si) = (self.show_hidden, self.show_ignored);
        let mut dirs: Vec<&PathBuf> = dirs.iter().collect();
        dirs.sort_by_key(|d| d.components().count());
        for dir in dirs {
            let Ok(rel) = dir.strip_prefix(&self.root) else { continue };
            if !dir.is_dir() {
                continue;
            }
            let mut cur = self.root.clone();
            for c in rel.components() {
                cur.push(c);
                let Some(n) = self.node_mut(&cur) else { break };
                if !n.is_dir {
                    break;
                }
                n.expanded = true;
                if n.children.is_none() {
                    n.children = Some(read_children(&n.path, sh, si));
                }
            }
        }
        let selected = self.selected_row().map(|r| r.path.clone());
        self.reflatten();
        if let Some(i) = selected.and_then(|p| self.rows.iter().position(|r| r.path == p)) {
            self.list.select(i);
        }
    }

    /// Whether hidden and ignored entries show.
    pub fn visibility(&self) -> (bool, bool) {
        (self.show_hidden, self.show_ignored)
    }

    /// Set whether hidden and ignored entries show. On a change every listed
    /// directory is read again on this thread, keeping expansion, and a walk
    /// still running with the old settings is dropped when it lands.
    pub fn set_visibility(&mut self, hidden: bool, ignored: bool) {
        if (hidden, ignored) == (self.show_hidden, self.show_ignored) {
            return;
        }
        (self.show_hidden, self.show_ignored) = (hidden, ignored);
        self.applied_seq = self.refresh_seq + 1;
        let mut dirs = vec![self.root.clone()];
        dirs.extend(self.expanded_dirs());
        self.merge_listing(dirs.into_iter().map(|d| (d.clone(), read_children(&d, hidden, ignored))).collect());
    }

    /// Merge re-read directories, keeping expansion and the selected path.
    /// Directories expanded after the request keep the children they
    /// loaded; a refresh older than the last applied one is ignored.
    /// Returns the refresh to run next when one was asked for meanwhile.
    pub fn finish_refresh(&mut self, seq: u64, listing: DirListing) -> Option<TreeRefresh> {
        self.merge_refresh(seq, listing);
        self.refresh.done().then(|| self.refresh_request())
    }

    fn merge_refresh(&mut self, seq: u64, listing: DirListing) -> bool {
        if seq < self.applied_seq {
            return false;
        }
        self.applied_seq = seq;
        self.merge_listing(listing);
        true
    }

    /// Re-read one directory now (after a file operation in it), keeping
    /// expansion and selection.
    pub fn reload_dir(&mut self, dir: &Path) {
        if self.lists(dir) {
            self.merge_listing(HashMap::from([(dir.to_path_buf(), read_children(dir, self.show_hidden, self.show_ignored))]));
        } else if let Some(n) = self.node_mut(dir) {
            // Collapsed: forget the old listing; expanding reads it afresh.
            n.children = None;
        }
    }

    fn merge_listing(&mut self, mut listing: DirListing) {
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
        let selected = self.selected_row().map(|r| r.path.clone());
        let nodes = std::mem::take(&mut self.nodes);
        self.nodes = merge(nodes, &self.root.clone(), &mut listing);
        self.reflatten();
        if let Some(i) = selected.and_then(|p| self.rows.iter().position(|r| r.path == p)) {
            self.list.select(i);
        }
    }

    #[cfg(test)]
    pub fn row_names(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.name.clone()).collect()
    }

    #[cfg(test)]
    pub fn refresh_requests(&self) -> u64 {
        self.refresh.requests
    }

    /// Re-read every expanded directory on this thread.
    #[cfg(test)]
    pub fn refresh(&mut self) {
        let req = self.begin_refresh().expect("no refresh running");
        let listing = req.run();
        assert!(self.finish_refresh(req.seq, listing).is_none());
    }

    /// Whether the tree lists the entries of `dir` (the root or an expanded
    /// directory), so a change inside it shows.
    pub fn lists(&self, dir: &Path) -> bool {
        fn find<'a>(nodes: &'a [Node], path: &Path) -> Option<&'a Node> {
            let n = nodes.iter().find(|n| path.starts_with(&n.path))?;
            if n.path == path { Some(n) } else { find(n.children.as_deref()?, path) }
        }
        dir == self.root || find(&self.nodes, dir).is_some_and(|n| n.is_dir && n.expanded)
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
        /// Rows whose name contains `filter`, with the folders leading to
        /// them shown open; returns whether any row matched.
        fn walk_filtered(nodes: &[Node], depth: usize, filter: &str, out: &mut Vec<Row>) -> bool {
            let mut any = false;
            for n in nodes {
                let mut sub = Vec::new();
                let inner = n.children.as_deref().is_some_and(|c| walk_filtered(c, depth + 1, filter, &mut sub));
                if inner || n.name.to_lowercase().contains(filter) {
                    let expanded = n.expanded || inner;
                    out.push(Row { path: n.path.clone(), name: n.name.clone(), is_dir: n.is_dir, expanded, depth });
                    out.append(&mut sub);
                    any = true;
                }
            }
            any
        }
        let mut rows = Vec::new();
        if self.filter.is_empty() {
            walk(&self.nodes, 0, &mut rows);
        } else {
            walk_filtered(&self.nodes, 0, &self.filter.to_lowercase(), &mut rows);
        }
        self.rows = rows;
        self.list.set_len(self.rows.len());
    }

    /// Typing narrows the tree (speed search): printable keys extend the
    /// filter, Backspace shortens it, Esc clears it. False for other keys
    /// and for Space, Backspace and Esc with no filter (tree bindings).
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return false;
        }
        match key.code {
            KeyCode::Char(' ') if self.filter.is_empty() => return false,
            KeyCode::Char(c) => self.filter.push(c),
            KeyCode::Backspace if !self.filter.is_empty() => {
                self.filter.pop();
            }
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            _ => return false,
        }
        let keep = self.selected_row().map(|r| r.path.clone());
        self.reflatten();
        // Select the first name that matches, else stay on the same entry.
        let f = self.filter.to_lowercase();
        let first = (!f.is_empty()).then(|| self.rows.iter().position(|r| r.name.to_lowercase().contains(&f))).flatten();
        if let Some(i) = first.or_else(|| keep.and_then(|p| self.rows.iter().position(|r| r.path == p))) {
            self.list.select(i);
        }
        true
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The directory new entries go to: the selected directory, or the
    /// selected file's directory, or the root.
    fn target_dir(&self) -> PathBuf {
        match self.selected_row() {
            Some(r) if r.is_dir => r.path.clone(),
            Some(r) => r.path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        }
    }

    fn rel(&self, p: &Path) -> String {
        let rel = p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy().replace('\\', "/");
        if rel.is_empty() { ".".into() } else { rel }
    }

    /// Menu for the selected row (`row`) or for empty space (`None`).
    fn context_menu(&self, row: Option<&Row>, x: u16, y: u16) -> ContextMenu {
        let t = |a| Action::Tree(a);
        let on_row = row.is_some();
        let check = |on: bool| if on { "[x]" } else { "[ ]" };
        ContextMenu::new(
            vec![
                MenuItem::new("New File…", t(TreeAction::NewFile)),
                MenuItem::new("New Directory…", t(TreeAction::NewDir)),
                MenuItem::new("Rename…", t(TreeAction::Rename)).enabled(on_row),
                MenuItem::new("Delete…", t(TreeAction::Delete)).enabled(on_row),
                MenuItem::new("Copy Path", t(TreeAction::CopyPath)).enabled(on_row),
                MenuItem::new("Copy Relative Path", t(TreeAction::CopyRelPath)).enabled(on_row),
                MenuItem::new("Show History", Action::Git(crate::actions::GitAction::FileHistory)).enabled(on_row),
                MenuItem::new(format!("{} Show Hidden Files", check(self.show_hidden)), t(TreeAction::ToggleHidden)),
                MenuItem::new(format!("{} Show Ignored Files", check(self.show_ignored)), t(TreeAction::ToggleIgnored)),
                MenuItem::new("Refresh", t(TreeAction::Refresh)),
            ],
            x,
            y,
        )
    }

    fn file_action(&mut self, a: &TreeAction, cx: &mut Ctx) {
        let row = self.selected_row().cloned();
        match a {
            TreeAction::New => {
                let d = Dialog::choice(DialogId::TreeNewKind(self.target_dir()), "New", "", vec!["File".into(), "Directory".into()]);
                cx.effects.push(Effect::Dialog(d));
            }
            TreeAction::NewFile | TreeAction::NewDir => {
                cx.effects.push(Effect::Dialog(new_entry_dialog(&self.target_dir(), &self.root, *a == TreeAction::NewDir)));
            }
            TreeAction::Rename => {
                let Some(r) = row else { return };
                let msg = format!("Rename {}:", self.rel(&r.path));
                let d = Dialog::input(DialogId::TreeRename(r.path.clone()), "Rename", &msg, &r.name, "Rename");
                cx.effects.push(Effect::Dialog(d));
            }
            TreeAction::Delete => {
                let Some(r) = row else { return };
                let what = if r.is_dir { "directory" } else { "file" };
                let msg = format!(
                    "Delete the {what} {}{}?\nThis cannot be undone. Open tabs of it close; unsaved edits there are lost.",
                    self.rel(&r.path),
                    if r.is_dir { " and everything in it" } else { "" }
                );
                cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::TreeDelete(r.path), "Delete", &msg, &["Delete", "Cancel"])));
            }
            TreeAction::CopyPath | TreeAction::CopyRelPath => {
                let Some(r) = row else { return };
                let text = if *a == TreeAction::CopyPath { r.path.display().to_string() } else { self.rel(&r.path) };
                cx.notify(Level::Info, format!("Copied {text}"));
                cx.effects.push(Effect::Clipboard(text));
            }
            TreeAction::ToggleHidden | TreeAction::ToggleIgnored => {
                let (flag, what) =
                    if *a == TreeAction::ToggleHidden { (&mut self.show_hidden, "Hidden") } else { (&mut self.show_ignored, "Ignored") };
                *flag = !*flag;
                let state = if *flag { "shown" } else { "hidden" };
                cx.notify(Level::Info, format!("{what} files {state} in the Project tree"));
                cx.effects.push(Effect::RefreshTree);
            }
            TreeAction::ContextMenu => {
                let (x, y) = match self.list.selected.checked_sub(self.list.offset) {
                    Some(vi) if row.is_some() && (vi as u16) < self.inner.height => {
                        let depth = row.as_ref().map_or(0, |r| r.depth) as u16;
                        (self.inner.x + depth * 2 + 2, self.inner.y + vi as u16)
                    }
                    _ => (self.inner.x + 2, self.inner.y),
                };
                cx.effects.push(Effect::ContextMenu(self.context_menu(row.as_ref(), x, y)));
            }
            _ => {}
        }
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
            TreeAction::Refresh => cx.effects.push(Effect::RefreshTree),
            TreeAction::New
            | TreeAction::NewFile
            | TreeAction::NewDir
            | TreeAction::Rename
            | TreeAction::Delete
            | TreeAction::CopyPath
            | TreeAction::CopyRelPath
            | TreeAction::ToggleHidden
            | TreeAction::ToggleIgnored
            | TreeAction::ContextMenu => self.file_action(a, cx),
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
            MouseEventKind::Down(MouseButton::Right) => {
                let row = self.list.hit(self.inner, m.x, m.y).map(|i| {
                    self.list.select(i);
                    self.rows[i].clone()
                });
                cx.effects.push(Effect::ContextMenu(self.context_menu(row.as_ref(), m.x, m.y)));
                true
            }
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
        let mut inner = block.inner(area);
        frame.render_widget(block, area);
        // Speed search: the typed text sits on the last row.
        if !self.filter.is_empty() && inner.height > 1 {
            inner.height -= 1;
            let style = if self.rows.is_empty() { theme.warn } else { theme.dim };
            let text = format!("Filter: {}", self.filter);
            frame.buffer_mut().set_stringn(inner.x, inner.bottom(), &text, inner.width as usize, style);
        }
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

/// Name prompt for a new file or directory in `dir`.
pub fn new_entry_dialog(dir: &Path, root: &Path, is_dir: bool) -> Dialog {
    let rel = dir.strip_prefix(root).unwrap_or(dir).display().to_string();
    let (title, what) = if is_dir { ("New Directory", "directory") } else { ("New File", "file") };
    let msg = format!("New {what} in {}/ (a/b creates folders too):", if rel.is_empty() { "." } else { &rel });
    Dialog::input(DialogId::TreeNew { dir: dir.to_path_buf(), is_dir }, title, &msg, "", "Create")
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
    fn expand_paths_and_visibility_keep_folders_open() {
        let root = tmp_tree("expand");
        std::fs::create_dir_all(root.join("a/.dot")).unwrap();
        let mut p = ProjectPanel::new(root.clone(), false, false);
        // Children before parents, a file and a missing folder are fine.
        p.expand_paths(&[root.join("a/deep"), root.join("a"), root.join("z.txt"), root.join("b/gone"), root.join("c")]);
        assert_eq!(names(&p), ["a", "  deep", "    x.txt", "  one.txt", "b", "c", "  three.txt", "z.txt"]);
        assert_eq!(p.expanded_dirs(), [root.join("a"), root.join("a/deep"), root.join("c")]);
        p.set_visibility(true, false);
        assert_eq!(names(&p), ["a", "  .dot", "  deep", "    x.txt", "  one.txt", "b", "c", "  three.txt", "z.txt"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refreshes_coalesce_and_late_results_merge_safely() {
        let root = tmp_tree("late");
        let mut p = ProjectPanel::new(root.clone(), false, false);
        let first = p.begin_refresh().unwrap();
        let first_listing = first.run();
        // Expanded while the walk runs: keeps the children it loaded.
        p.select_path(&root.join("b/two.txt"));
        assert!(p.begin_refresh().is_none(), "one walk at a time");
        std::fs::write(root.join("b/new.txt"), "").unwrap();
        let second = p.finish_refresh(first.seq, first_listing).expect("the held-back refresh runs next");
        assert_eq!(names(&p), ["a", "b", "  two.txt", "c", "z.txt"]);
        assert!(p.finish_refresh(second.seq, second.run()).is_none());
        assert_eq!(names(&p), ["a", "b", "  new.txt", "  two.txt", "c", "z.txt"]);
        // An older listing arriving late (its request was presumed lost) is ignored.
        assert!(!p.merge_refresh(first.seq, HashMap::new()));
        assert_eq!(names(&p), ["a", "b", "  new.txt", "  two.txt", "c", "z.txt"]);
        // A listing without `c` (requested before it was expanded) leaves it.
        let req = p.begin_refresh().unwrap();
        p.select_path(&root.join("c/three.txt"));
        p.finish_refresh(req.seq, req.run());
        assert_eq!(names(&p), ["a", "b", "  new.txt", "  two.txt", "c", "  three.txt", "z.txt"]);
        assert!(p.lists(&root.join("c")) && p.lists(&root) && !p.lists(&root.join("a")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
