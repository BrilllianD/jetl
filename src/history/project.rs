//! The Local History window's project view: the files under a folder (the
//! project root, or one picked in the Project tree) that have local
//! history, newest change first, deleted and renamed-away files included,
//! and their labels grouped: every file's label entry with the same time
//! and name is one row (a label put on several files), which Enter expands
//! into its files. The rows come from a scan on the history worker
//! (`store::recent`, `HistoryMsg::Recent`); the panel (`panel.rs`) shows the
//! diff of the selected row's snapshot and drills into a file's own view.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer as Screen;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{Entry, FileState, RecentFile};
use crate::config::theme::Theme;
use crate::ui::widgets::list::ListState;
use crate::util::text::fit;

/// Columns of the short date (`MM-DD HH:MM`).
const DATE_W: usize = 11;
/// Below this, a row's snapshot label is left out.
const LABEL_MIN: usize = 6;

/// What a row stands for, kept across a new scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowId {
    File(PathBuf),
    Label(u64, String),
    Member(u64, String, PathBuf),
}

/// A shown row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    File(usize),
    Label(usize),
    /// A file of a label (group, member).
    Member(usize, usize),
}

/// A file with history, formatted once.
#[derive(Debug, Clone)]
struct FileRow {
    path: PathBuf,
    rel: String,
    name: String,
    /// Its folder relative to the root with a trailing `/`; empty at the top.
    dir: String,
    newest: Entry,
    /// The newest snapshot's label ("Saved", "Label: v1").
    label: String,
    /// `YYYY-MM-DD HH:MM:SS` of the newest snapshot.
    when: String,
    state: FileState,
    /// "deleted", "renamed to b.txt", or empty.
    gone: String,
}

impl FileRow {
    /// What the right side says: where the file went, else its newest
    /// snapshot's label.
    fn what(&self) -> &str {
        if self.gone.is_empty() { &self.label } else { &self.gone }
    }
}

/// Every file's label with one time and name.
#[derive(Debug, Clone)]
struct Group {
    at: u64,
    name: String,
    when: String,
    /// (index into `files`, the file's label entry), by path.
    members: Vec<(usize, Entry)>,
}

/// A label row's time, name and every file's entry of it.
pub type LabelGroup = (u64, String, Vec<(PathBuf, Entry)>);

/// The selected row's file and snapshot, for the diff and the revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub path: PathBuf,
    pub rel: String,
    pub entry: Entry,
    /// The snapshot's label (relative paths), for the diff header.
    pub label: String,
    /// `YYYY-MM-DD HH:MM:SS`.
    pub when: String,
    pub deleted: bool,
}

#[derive(Debug)]
pub struct ProjectView {
    /// The folder shown, as asked (the scan answers with it).
    pub scope: PathBuf,
    /// The title: the folder relative to the root with a trailing `/`, or
    /// the root's name.
    pub rel: String,
    files: Vec<FileRow>,
    groups: Vec<Group>,
    expanded: HashSet<(u64, String)>,
    /// Top-level rows (files and labels), newest first.
    order: Vec<Row>,
    /// The rows that match the filter, expanded labels followed by their files.
    shown: Vec<Row>,
    pub list: ListState,
    pub filter: String,
    /// No scan answered yet.
    pub loading: bool,
    /// A scan is in flight; `again`: ask once more when it lands.
    pub scanning: bool,
    pub again: bool,
    /// Select this file's row when the rows arrive (back from its own view).
    pub want: Option<PathBuf>,
}

/// `scope` as a title: relative to `root` with a trailing `/`, or the
/// root's folder name.
fn scope_title(scope: &Path, root: &Path) -> String {
    match scope.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => {
            format!("{}/", root.file_name().map(|n| n.to_string_lossy()).unwrap_or_else(|| root.to_string_lossy()))
        }
        _ => format!("{}/", crate::ui::palette::display_path(scope, root)),
    }
}

impl ProjectView {
    pub fn new(scope: PathBuf, root: &Path) -> Self {
        Self {
            rel: scope_title(&scope, root),
            scope,
            files: Vec::new(),
            groups: Vec::new(),
            expanded: HashSet::new(),
            order: Vec::new(),
            shown: Vec::new(),
            list: ListState::default(),
            filter: String::new(),
            loading: true,
            scanning: false,
            again: false,
            want: None,
        }
    }

    /// A tree rename of the scope folder or one above it: follow it.
    /// Returns whether it moved.
    pub fn retarget(&mut self, from: &Path, to: &Path, root: &Path) -> bool {
        let Ok(rest) = self.scope.strip_prefix(from) else { return false };
        self.scope = to.join(rest);
        self.rel = scope_title(&self.scope, root);
        true
    }

    /// Show the files of a new scan. The filter, the expanded labels and
    /// the selected row (by what it stands for, else its index) stay.
    /// `offset` gives seconds east of UTC at a time (fixed in tests).
    pub fn set_files(&mut self, files: Vec<RecentFile>, root: &Path, offset: impl Fn(i64) -> i64) {
        let keep = match self.want.take() {
            Some(p) => Some(RowId::File(p)),
            None => self.selected_id(),
        };
        let at = self.list.selected;
        let when = |at: u64| super::local_time(at, &offset);
        let mut labels: Vec<(usize, Entry)> = Vec::new();
        self.files = files
            .into_iter()
            .enumerate()
            .map(|(fi, f)| {
                labels.extend(f.labels.into_iter().map(|e| (fi, e)));
                let rel = crate::ui::palette::display_path(&f.path, root);
                let (dir, name) = match rel.rfind('/') {
                    Some(k) => (rel[..=k].to_string(), rel[k + 1..].to_string()),
                    None => (String::new(), rel.clone()),
                };
                let gone = match &f.state {
                    FileState::Present => String::new(),
                    FileState::Deleted => "deleted".into(),
                    FileState::Renamed(to) => format!("renamed to {}", crate::ui::palette::display_path(to, root)),
                };
                FileRow {
                    label: f.newest.label_in(root),
                    when: when(f.newest.at),
                    rel,
                    name,
                    dir,
                    gone,
                    path: f.path,
                    newest: f.newest,
                    state: f.state,
                }
            })
            .collect();
        // Labels by (time, name), their files by path.
        let mut groups: Vec<Group> = Vec::new();
        for (fi, e) in labels {
            let name = e.detail.clone().unwrap_or_default();
            match groups.iter_mut().find(|g| g.at == e.at && g.name == name) {
                Some(g) => g.members.push((fi, e)),
                None => groups.push(Group { at: e.at, when: when(e.at), name, members: vec![(fi, e)] }),
            }
        }
        for g in &mut groups {
            let files = &self.files;
            g.members.sort_by(|a, b| files[a.0].path.cmp(&files[b.0].path));
        }
        self.groups = groups;
        let mut order: Vec<(std::cmp::Reverse<u64>, u8, String, Row)> = self
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (std::cmp::Reverse(f.newest.at), 1, f.rel.clone(), Row::File(i)))
            .chain(self.groups.iter().enumerate().map(|(i, g)| (std::cmp::Reverse(g.at), 0, g.name.clone(), Row::Label(i))))
            .collect();
        order.sort_by(|a, b| (&a.0, a.1, &a.2).cmp(&(&b.0, b.1, &b.2)));
        self.order = order.into_iter().map(|(.., r)| r).collect();
        self.loading = false;
        self.refilter(keep.as_ref(), at);
    }

    /// Recompute the rows shown; select `keep` when it is still shown, else
    /// row `at` (clamped).
    pub fn refilter(&mut self, keep: Option<&RowId>, at: usize) {
        let terms: Vec<String> = self.filter.split_whitespace().map(str::to_lowercase).collect();
        let mut shown = Vec::new();
        for &row in &self.order {
            let text = self.search_text(row).to_lowercase();
            if !terms.iter().all(|t| text.contains(t.as_str())) {
                continue;
            }
            shown.push(row);
            if let Row::Label(g) = row {
                let grp = &self.groups[g];
                if self.expanded.contains(&(grp.at, grp.name.clone())) {
                    shown.extend((0..grp.members.len()).map(|m| Row::Member(g, m)));
                }
            }
        }
        self.shown = shown;
        self.list.set_len(self.shown.len());
        let found = keep.and_then(|k| self.shown.iter().position(|&r| self.id(r) == *k));
        self.list.select(found.unwrap_or(at));
    }

    /// What the filter matches a row against.
    fn search_text(&self, row: Row) -> String {
        match row {
            Row::File(i) => {
                let f = &self.files[i];
                format!("{} {} {} {}", f.when, f.rel, f.label, f.gone)
            }
            Row::Label(g) => {
                let grp = &self.groups[g];
                let paths: Vec<&str> = grp.members.iter().map(|(fi, _)| self.files[*fi].rel.as_str()).collect();
                format!("{} Label: {} {}", grp.when, grp.name, paths.join(" "))
            }
            // Shown with their label.
            Row::Member(..) => String::new(),
        }
    }

    fn id(&self, row: Row) -> RowId {
        match row {
            Row::File(i) => RowId::File(self.files[i].path.clone()),
            Row::Label(g) => RowId::Label(self.groups[g].at, self.groups[g].name.clone()),
            Row::Member(g, m) => {
                let grp = &self.groups[g];
                RowId::Member(grp.at, grp.name.clone(), self.files[grp.members[m].0].path.clone())
            }
        }
    }

    fn selected_row(&self) -> Option<Row> {
        self.shown.get(self.list.selected).copied()
    }

    pub fn selected_id(&self) -> Option<RowId> {
        self.selected_row().map(|r| self.id(r))
    }

    /// The file and snapshot of the selected file row (its newest) or label
    /// file row (the label); `None` on a label row.
    pub fn selection(&self) -> Option<Selection> {
        let (f, entry) = match self.selected_row()? {
            Row::File(i) => (&self.files[i], &self.files[i].newest),
            Row::Member(g, m) => {
                let (fi, e) = &self.groups[g].members[m];
                (&self.files[*fi], e)
            }
            Row::Label(_) => return None,
        };
        let same = entry == &f.newest;
        Some(Selection {
            path: f.path.clone(),
            rel: f.rel.clone(),
            entry: entry.clone(),
            label: if same { f.label.clone() } else { entry.label() },
            when: if same {
                f.when.clone()
            } else {
                self.groups.iter().find(|g| g.at == entry.at).map(|g| g.when.clone()).unwrap_or_default()
            },
            deleted: f.state != FileState::Present,
        })
    }

    /// The selected label row's name and every file's entry of it, or the
    /// selected label file row's own entry.
    pub fn selected_label(&self) -> Option<(String, Vec<(PathBuf, Entry)>)> {
        match self.selected_row()? {
            Row::Label(g) => {
                let grp = &self.groups[g];
                Some((grp.name.clone(), grp.members.iter().map(|(fi, e)| (self.files[*fi].path.clone(), e.clone())).collect()))
            }
            Row::Member(g, m) => {
                let (fi, e) = &self.groups[g].members[m];
                Some((self.groups[g].name.clone(), vec![(self.files[*fi].path.clone(), e.clone())]))
            }
            Row::File(_) => None,
        }
    }

    /// The selected label row's time, name and every file's entry of it;
    /// `None` on any other row.
    pub fn label_group(&self) -> Option<LabelGroup> {
        let Row::Label(g) = self.selected_row()? else { return None };
        let grp = &self.groups[g];
        Some((grp.at, grp.name.clone(), grp.members.iter().map(|(fi, e)| (self.files[*fi].path.clone(), e.clone())).collect()))
    }

    /// Whether the view has a label `name` put at `at`.
    pub fn has_label(&self, at: u64, name: &str) -> bool {
        self.groups.iter().any(|g| g.at == at && g.name == name)
    }

    /// Whether a label row (not one of its files) is selected.
    pub fn on_label(&self) -> bool {
        matches!(self.selected_row(), Some(Row::Label(_)))
    }

    /// The right side's text for a selected label row.
    pub fn label_note(&self) -> Option<String> {
        let Some(Row::Label(g)) = self.selected_row() else { return None };
        let grp = &self.groups[g];
        let n = grp.members.len();
        let files = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        let how = if self.expanded.contains(&(grp.at, grp.name.clone())) { "Enter hides them" } else { "Enter lists them" };
        Some(format!("Label \"{}\" on {files}: {how}", grp.name))
    }

    /// Expand or collapse the selected label (on one of its files: collapse
    /// it and select it). `false` when no label row is selected.
    pub fn toggle(&mut self) -> bool {
        let g = match self.selected_row() {
            Some(Row::Label(g) | Row::Member(g, _)) => g,
            _ => return false,
        };
        let key = (self.groups[g].at, self.groups[g].name.clone());
        let label = RowId::Label(key.0, key.1.clone());
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        let at = self.list.selected;
        self.refilter(Some(&label), at);
        true
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn shown_len(&self) -> usize {
        self.shown.len()
    }

    // ---- rendering -------------------------------------------------------

    /// The rows (or the empty state) in `r`; returns whether rows were drawn.
    pub fn render_list(&mut self, buf: &mut Screen, r: Rect, focused: bool, theme: &Theme) -> bool {
        let msg = |buf: &mut Screen, text: &str| {
            buf.set_stringn(r.x + 1, r.y, text, r.width.saturating_sub(1) as usize, theme.dim);
        };
        if self.files.is_empty() {
            let text =
                if self.loading { "Loading…".to_string() } else { format!("No local history under {} yet: saves record it", self.rel) };
            msg(buf, &text);
            return false;
        }
        if self.shown.is_empty() {
            msg(buf, &format!("No file matches \"{}\"", self.filter));
            return false;
        }
        self.list.fit(self.shown.len(), r.height as usize);
        for (vi, i) in self.list.visible().enumerate() {
            let row = Rect::new(r.x, r.y + vi as u16, r.width, 1);
            let selected = i == self.list.selected;
            let base = if selected {
                if focused { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(row, base);
            }
            self.render_row(buf, row, self.shown[i], base, theme);
        }
        true
    }

    fn render_row(&self, buf: &mut Screen, r: Rect, row: Row, base: Style, theme: &Theme) {
        let dim = base.patch(theme.dim);
        // (left parts: the first is never cut but by the edge, the last is
        // cut to the room left), right label, right date.
        let (indent, head, head_style, tail, what, what_style, date): (&str, String, Style, &str, String, Style, &str) = match row {
            Row::File(i) => {
                let f = &self.files[i];
                let (style, what_style) = file_styles(f, base, theme);
                ("", f.name.clone(), style, f.dir.as_str(), f.what().to_string(), what_style, f.when.get(5..16).unwrap_or(""))
            }
            Row::Member(g, m) => {
                let f = &self.files[self.groups[g].members[m].0];
                let (style, what_style) = file_styles(f, base, theme);
                ("    ", f.name.clone(), style, f.dir.as_str(), f.gone.clone(), what_style, "")
            }
            Row::Label(g) => {
                let grp = &self.groups[g];
                let open = if self.expanded.contains(&(grp.at, grp.name.clone())) { "▾ " } else { "▸ " };
                let n = grp.members.len();
                let files = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
                let head = format!("{open}Label: {}", grp.name);
                ("", head, base.add_modifier(Modifier::BOLD), "", files, dim, grp.when.get(5..16).unwrap_or(""))
            }
        };
        // One cell of padding on each side, two between parts. The date
        // first; the folder and the label share what is left.
        let room = (r.width as usize).saturating_sub(2);
        let head = fit(&head, room.saturating_sub(indent.width()));
        let avail = room.saturating_sub(indent.width() + head.width());
        let show_date = !date.is_empty() && avail >= DATE_W + 2;
        let rest = avail - if show_date { DATE_W + 2 } else { 0 };
        let deleted = match row {
            Row::File(i) => self.files[i].state == FileState::Deleted,
            Row::Member(g, m) => self.files[self.groups[g].members[m].0].state == FileState::Deleted,
            Row::Label(_) => false,
        };
        let (dir_room, what_room) = share(rest, tail.width(), what.width(), deleted);
        let tail = fit(tail, dir_room);
        let what = fit(&what, what_room);
        crate::ui::widgets::file_rows::spans(buf, r, &[(indent, base), (&head, head_style), ("  ", base), (&tail, dim)]);
        let right_w = what.width() + if show_date { DATE_W + 2 } else { 0 };
        let mut x = r.x + 1 + (room - right_w.min(room)) as u16;
        if !what.is_empty() {
            buf.set_stringn(x, r.y, &what, what.width(), what_style);
            x += what.width() as u16;
        }
        if show_date {
            buf.set_stringn(x + 2, r.y, date, DATE_W, dim);
        }
    }

    /// The rows as text (tests): the head, the right side, `>` before an
    /// expanded label's files.
    #[cfg(test)]
    pub fn rows(&self) -> Vec<String> {
        self.shown
            .iter()
            .map(|&r| match r {
                Row::File(i) => format!("{} · {}", self.files[i].rel, self.files[i].what()),
                Row::Label(g) => format!("Label: {} ({})", self.groups[g].name, self.groups[g].members.len()),
                Row::Member(g, m) => {
                    let f = &self.files[self.groups[g].members[m].0];
                    format!("> {}{}", f.rel, if f.gone.is_empty() { String::new() } else { format!(" · {}", f.gone) })
                }
            })
            .collect()
    }
}

/// Split `rest` cells between a row's folder (`dir` wide) and its label
/// (`what` wide), each after a two-cell gap: both whole when they fit, else
/// the label first when `first` (a deleted file's "deleted"), else each
/// half unless the other needs less; a label cut below [`LABEL_MIN`] is
/// left out and its room goes to the folder. Returns the cells of each.
fn share(rest: usize, dir: usize, what: usize, first: bool) -> (usize, usize) {
    let (d, w) = (if dir > 0 { dir + 2 } else { 0 }, if what > 0 { what + 2 } else { 0 });
    let half = rest / 2;
    let (d, w) = if d + w <= rest {
        (d, w)
    } else if first && w <= rest {
        (rest - w, w)
    } else if d <= half {
        (d, rest - d)
    } else if w <= rest - half {
        (rest - w, w)
    } else {
        (half, rest - half)
    };
    let w = if w >= LABEL_MIN.min(what) + 2 { w } else { 0 };
    let d = if w == 0 && dir > 0 { rest.min(dir + 2) } else { d };
    (d.saturating_sub(2), w.saturating_sub(2))
}

/// A file name's style (dim and crossed out when gone) and its right
/// side's (deleted in `warn`).
fn file_styles(f: &FileRow, base: Style, theme: &Theme) -> (Style, Style) {
    match f.state {
        FileState::Present => (base, base.patch(theme.dim)),
        FileState::Deleted => (base.patch(theme.dim).add_modifier(Modifier::CROSSED_OUT), base.patch(theme.warn)),
        FileState::Renamed(_) => (base.patch(theme.dim).add_modifier(Modifier::CROSSED_OUT), base.patch(theme.dim)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::history::Kind;

    /// An entry of `/w/<rel>` at minute `min` past midnight UTC, day 20 000.
    pub(crate) fn e(rel: &str, min: u64, kind: Kind, detail: Option<&str>) -> Entry {
        Entry {
            v: 1,
            path: format!("/w/{rel}"),
            at: (20_000 * 86_400 + min * 60) * 1000,
            blob: format!("{min:040}"),
            len: 10,
            lines: 1,
            kind,
            detail: detail.map(Into::into),
            since: None,
        }
    }

    /// `/w/<rel>` whose newest snapshot is a save at `min` (or its newest
    /// label, when later), with those labels.
    pub(crate) fn file(rel: &str, min: u64, labels: &[(u64, &str)], state: FileState) -> RecentFile {
        let labels: Vec<Entry> = labels.iter().map(|(m, n)| e(rel, *m, Kind::Label, Some(n))).collect();
        let save = e(rel, min, Kind::Save, None);
        let newest = labels.iter().filter(|l| l.at > save.at).max_by_key(|l| l.at).cloned().unwrap_or(save);
        RecentFile { path: PathBuf::from(format!("/w/{rel}")), newest, count: 1 + labels.len(), labels, state }
    }

    fn view(files: Vec<RecentFile>) -> ProjectView {
        let mut v = ProjectView::new(PathBuf::from("/w"), Path::new("/w"));
        v.set_files(files, Path::new("/w"), |_| 0);
        v
    }

    pub(crate) fn fixture() -> Vec<RecentFile> {
        let mut files = vec![
            file("src/a.rs", 9, &[(5, "v1")], FileState::Present),
            file("b.txt", 7, &[(5, "v1")], FileState::Deleted),
            file("old.txt", 3, &[], FileState::Renamed(PathBuf::from("/w/new.txt"))),
            file("c.txt", 2, &[(1, "solo")], FileState::Present),
        ];
        files.sort_by_key(|f| std::cmp::Reverse(f.newest.at));
        files
    }

    #[test]
    fn labels_with_one_time_and_name_are_one_row() {
        let mut v = view(fixture());
        assert_eq!(v.scope, PathBuf::from("/w"));
        assert_eq!(v.rel, "w/");
        assert_eq!(
            v.rows(),
            ["src/a.rs · Saved", "b.txt · deleted", "Label: v1 (2)", "old.txt · renamed to new.txt", "c.txt · Saved", "Label: solo (1)"]
        );
        // Enter on the label lists its files (by path, gone ones marked);
        // on one of them, closes it again.
        v.list.select(2);
        assert!(v.selection().is_none());
        assert_eq!(v.label_note().as_deref(), Some("Label \"v1\" on 2 files: Enter lists them"));
        let (name, entries) = v.selected_label().unwrap();
        assert_eq!((name.as_str(), entries.len()), ("v1", 2));
        assert!(v.toggle());
        assert_eq!(v.rows()[2..5], ["Label: v1 (2)", "> b.txt · deleted", "> src/a.rs"]);
        v.list.down(1);
        let s = v.selection().unwrap();
        assert_eq!(
            (s.path, s.label.as_str(), s.deleted, s.when.as_str()),
            (PathBuf::from("/w/b.txt"), "Label: v1", true, "2024-10-04 00:05:00")
        );
        assert_eq!(v.selected_label().unwrap().1.len(), 1, "one file's label");
        assert!(v.toggle());
        assert_eq!((v.rows().len(), v.list.selected), (6, 2), "closed, the label selected");
        // A file row: its newest snapshot.
        v.list.select(0);
        let s = v.selection().unwrap();
        assert_eq!((s.rel.as_str(), s.entry.at, s.label.as_str(), s.deleted), ("src/a.rs", e("", 9, Kind::Save, None).at, "Saved", false));
        assert!(v.selected_label().is_none());
    }

    #[test]
    fn a_new_scan_keeps_the_filter_the_open_labels_and_the_selection() {
        let mut v = view(fixture());
        v.list.select(2);
        v.toggle();
        v.list.select(4);
        assert_eq!(v.selected_id(), Some(RowId::Member(e("", 5, Kind::Label, None).at, "v1".into(), PathBuf::from("/w/src/a.rs"))));
        // A newer file on top: the same row selected, the label still open.
        let mut files = fixture();
        files.insert(0, file("d.txt", 20, &[], FileState::Present));
        v.set_files(files, Path::new("/w"), |_| 0);
        assert_eq!(v.rows()[0], "d.txt · Saved");
        assert_eq!(v.rows()[v.list.selected], "> src/a.rs");
        // The filter: every term, deleted files by that word, a label by
        // its files' paths.
        v.filter = "deleted".into();
        v.refilter(None, 0);
        assert_eq!(v.rows(), ["b.txt · deleted"]);
        v.filter = "TXT 00:02".into();
        v.refilter(None, 0);
        assert_eq!(v.rows(), ["c.txt · Saved"]);
        v.filter = "src/a".into();
        v.refilter(None, 0);
        assert_eq!(v.rows(), ["src/a.rs · Saved", "Label: v1 (2)", "> b.txt · deleted", "> src/a.rs"]);
        // Gone from the scan: the same index, clamped.
        v.filter.clear();
        v.refilter(None, 0);
        v.list.select(1);
        v.set_files(vec![file("x.txt", 1, &[], FileState::Present)], Path::new("/w"), |_| 0);
        assert_eq!(v.list.selected, 0);
        // Back from a file's view: its row.
        v.want = Some(PathBuf::from("/w/c.txt"));
        v.set_files(fixture(), Path::new("/w"), |_| 0);
        assert_eq!(v.selected_id(), Some(RowId::File(PathBuf::from("/w/c.txt"))));
    }

    #[test]
    fn rows_drop_the_label_then_the_date_when_narrow() {
        let mut v = view(fixture());
        let theme = Theme::default();
        let text = |v: &mut ProjectView, w: u16| {
            let r = Rect::new(0, 0, w, 6);
            let mut buf = Screen::empty(r);
            v.render_list(&mut buf, r, true, &theme);
            (0..6).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>()
        };
        let wide = text(&mut v, 44);
        assert_eq!(
            wide,
            [
                " a.rs  src/              Saved  10-04 00:09",
                " b.txt                 deleted  10-04 00:07",
                " ▸ Label: v1           2 files  10-04 00:05",
                " old.txt    renamed to new.txt  10-04 00:03",
                " c.txt                   Saved  10-04 00:02",
                " ▸ Label: solo          1 file  10-04 00:01",
            ]
        );
        // Narrower: the label goes first, then the date; the name stays.
        let narrow = text(&mut v, 24);
        assert_eq!(narrow[..3], [" a.rs  sr…  10-04 00:09", " b.txt      10-04 00:07", " ▸ Label: v1    2 files"]);
        let tiny = text(&mut v, 12);
        assert_eq!(tiny[..3], [" a.rs  src/", " b.txt", " ▸ Label: …"]);
        // A scope with nothing, and the filter with no match.
        let mut empty = ProjectView::new(PathBuf::from("/w/d"), Path::new("/w"));
        assert_eq!(text(&mut empty, 44)[0], " Loading…");
        empty.set_files(Vec::new(), Path::new("/w"), |_| 0);
        assert_eq!(text(&mut empty, 60)[0], " No local history under d/ yet: saves record it");
        v.filter = "zzz".into();
        v.refilter(None, 0);
        assert_eq!(text(&mut v, 44)[0], " No file matches \"zzz\"");
    }

    #[test]
    fn the_scope_follows_a_rename_above_it() {
        let mut v = ProjectView::new(PathBuf::from("/w/d/sub"), Path::new("/w"));
        assert_eq!(v.rel, "d/sub/");
        assert!(!v.retarget(Path::new("/w/dd"), Path::new("/w/x"), Path::new("/w")));
        assert!(v.retarget(Path::new("/w/d"), Path::new("/w/e"), Path::new("/w")));
        assert_eq!((v.scope.as_path(), v.rel.as_str()), (Path::new("/w/e/sub"), "e/sub/"));
    }
}
