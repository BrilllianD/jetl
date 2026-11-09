//! Bookmarks across buffers: the chooser (Shift+F11), mnemonic bookmarks
//! (Ctrl+F11 sets a digit or a letter, Ctrl+0..9 or Alt+B then a digit or
//! letter jumps), bookmark groups and the bookmarks of closed files.
//!
//! A mnemonic belongs to at most one bookmark of the session: giving it to a
//! line removes the bookmark that had it (JetBrains). A group is a name a
//! bookmark carries (`None` is the default group), so it exists while a
//! bookmark names it. Line bookmarks live in `Buffer::bookmarks`, so they
//! follow edits; when a file's last tab closes its bookmarks wait in
//! `Editor::closed_bookmarks` (by path and line) and go back into the buffer
//! when the file opens again.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::Editor;
use super::buffer::BufferId;
use super::nav::PendingAt;
use crate::app::Ctx;
use crate::event::{Effect, Level};
use crate::ui::dialog::{Dialog, DialogId, DialogKind};

/// The default group's name; a bookmark with `group: None` is in it. Never stored.
pub const DEFAULT_GROUP: &str = "Bookmarks";

/// A bookmark's mnemonic. Digits sort before letters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mnemonic {
    /// 0 to 9.
    Digit(u8),
    /// 'A' to 'Z', always upper case.
    Letter(char),
}

impl Mnemonic {
    /// The digit mnemonic `d`; `None` past 9.
    pub fn digit(d: u8) -> Option<Mnemonic> {
        (d <= 9).then_some(Mnemonic::Digit(d))
    }

    /// The letter mnemonic `c`, stored upper case; `None` for anything but an ASCII letter.
    pub fn letter(c: char) -> Option<Mnemonic> {
        c.is_ascii_alphabetic().then(|| Mnemonic::Letter(c.to_ascii_uppercase()))
    }

    /// The character the gutter and the lists show.
    pub fn glyph(self) -> char {
        match self {
            Mnemonic::Digit(d) => char::from(b'0' + d),
            Mnemonic::Letter(c) => c,
        }
    }
}

impl fmt::Display for Mnemonic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.glyph())
    }
}

/// A bookmarked line's mnemonic and group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bookmark {
    pub mnemonic: Option<Mnemonic>,
    /// `None`: the default group, `DEFAULT_GROUP`.
    pub group: Option<String>,
}

impl Bookmark {
    /// Two bookmarks land on one line (an edit joined lines, a file reopened
    /// shorter, a session listed a line twice); `self` was there first. It
    /// stays whole, unless it has no mnemonic and `other` has one: then
    /// `other` replaces it whole, group included.
    pub fn merge(&mut self, other: Bookmark) {
        if self.mnemonic.is_none() && other.mnemonic.is_some() {
            *self = other;
        }
    }
}

/// A group name typed or loaded: trimmed; the default group's name is `None`.
pub fn group_name(text: &str) -> Result<Option<String>, &'static str> {
    let name = text.trim();
    if name.is_empty() {
        Err("A bookmark group needs a name")
    } else if name.chars().any(char::is_control) {
        Err("A bookmark group name cannot hold control characters")
    } else if name == DEFAULT_GROUP {
        Ok(None)
    } else {
        Ok(Some(name.to_string()))
    }
}

/// Where an item of the bookmarks dialog goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkTarget {
    /// A line of an open buffer.
    Open(BufferId, usize),
    /// A bookmarked line of a closed file.
    Closed(PathBuf, usize),
}

/// A bookmark of a closed file: its path, 0-based line, mnemonic and group.
pub type ClosedBookmark = (PathBuf, usize, Bookmark);

/// The gutter glyph of a bookmark: its mnemonic, else ◆.
fn glyph(mnemonic: Option<Mnemonic>) -> char {
    mnemonic.map_or('◆', Mnemonic::glyph)
}

/// The digit preselected for a new mnemonic bookmark:
/// the first free one, 1 to 9, then 0.
const PREFERRED: [u8; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 0];

impl Editor {
    /// Shift+F11: a choice dialog of the bookmarks of all open buffers,
    /// then those of closed files, group by group (the default group first;
    /// a prefix names the group once any named group exists).
    pub(super) fn show_bookmarks(&mut self, cx: &mut Ctx) {
        let root = cx.workspace.root();
        let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        let groups = self.groups_in_use();
        let rank = |g: &Option<String>| g.as_ref().map_or(0, |g| 1 + groups.iter().position(|n| n == g).unwrap_or(0));
        let mut rows: Vec<(usize, String, String, BookmarkTarget)> = Vec::new();
        for b in &self.buffers {
            let name = b.path.as_deref().map(rel).unwrap_or_else(|| b.name.clone());
            for (&line, bm) in &b.bookmarks {
                let text = format!("{} {name}:{} {}", glyph(bm.mnemonic), line + 1, b.line_text(line).trim());
                let group = bm.group.clone().unwrap_or_else(|| DEFAULT_GROUP.to_string());
                rows.push((rank(&bm.group), group, text, BookmarkTarget::Open(b.id, line)));
            }
        }
        for (path, lines) in &self.closed_bookmarks {
            for (&line, bm) in lines {
                let text = format!("{} {}:{} (closed)", glyph(bm.mnemonic), rel(path), line + 1);
                let group = bm.group.clone().unwrap_or_else(|| DEFAULT_GROUP.to_string());
                rows.push((rank(&bm.group), group, text, BookmarkTarget::Closed(path.clone(), line)));
            }
        }
        if rows.is_empty() {
            cx.notify(Level::Info, "No bookmarks; F11 toggles one on the caret line, Ctrl+F11 one with a digit or letter");
            return;
        }
        rows.sort_by_key(|r| r.0);
        let named = !groups.is_empty();
        let items = rows.iter().map(|(_, g, text, _)| if named { format!("{g}: {text}") } else { text.clone() }).collect();
        let targets = rows.into_iter().map(|r| r.3).collect();
        cx.effects.push(Effect::Dialog(Dialog::choice(DialogId::Bookmarks(targets), "Bookmarks", "Go to bookmark:", items)));
    }

    /// Go to an item of the bookmarks dialog.
    pub fn goto_bookmark_target(&mut self, target: &BookmarkTarget, cx: &mut Ctx) {
        match target {
            BookmarkTarget::Open(id, line) => self.goto_bookmark(*id, *line),
            BookmarkTarget::Closed(path, line) => self.open_closed_at(path.clone(), *line, cx),
        }
    }

    /// Open the closed file `path` with the caret going to `line` once it
    /// loads (already open: just go there).
    fn open_closed_at(&mut self, path: PathBuf, line: usize, cx: &mut Ctx) {
        if self.merge.is_some() {
            return;
        }
        if let Some(id) = self.index_of(&path).map(|i| self.buffers[i].id) {
            self.goto_bookmark(id, line);
            return;
        }
        self.push_nav_here();
        self.pending_nav = Some((path.clone(), PendingAt::Line(line)));
        cx.effects.push(Effect::OpenFile(path));
    }

    /// Jump to bookmarked `line` of buffer `id`; the position left is
    /// remembered for back.
    pub fn goto_bookmark(&mut self, id: BufferId, line: usize) {
        let Some(i) = self.buffers.iter().position(|b| b.id == id).filter(|_| self.merge.is_none()) else { return };
        self.push_nav_here();
        self.set_active(i);
        self.buffers[i].goto_line(line, 0);
    }

    /// Ctrl+0..9, Alt+B then a digit or letter: jump to the bookmark with
    /// mnemonic `m`, opening its file when it was closed (the caret goes to
    /// its line once it loads).
    pub(super) fn goto_mnemonic(&mut self, m: Mnemonic, cx: &mut Ctx) {
        if self.merge.is_some() {
            return;
        }
        if let Some((id, line)) = self.buffers.iter().find_map(|b| b.mnemonic_line(m).map(|l| (b.id, l))) {
            self.goto_bookmark(id, line);
        } else if let Some((path, line)) = self.closed_mnemonic(m) {
            self.open_closed_at(path, line, cx);
        } else {
            cx.notify(Level::Info, format!("No bookmark {m}; Ctrl+F11 gives the caret line a digit or letter"));
        }
    }

    /// The closed file's bookmark with mnemonic `m`: its path and line.
    fn closed_mnemonic(&self, m: Mnemonic) -> Option<(PathBuf, usize)> {
        self.closed_bookmarks.iter().find_map(|(p, lines)| lines.iter().find(|(_, b)| b.mnemonic == Some(m)).map(|(l, _)| (p.clone(), *l)))
    }

    /// Ctrl+F11: ask for the mnemonic of the caret line's bookmark. Each
    /// digit is a button, a letter key answers too; Esc cancels.
    pub(super) fn choose_mnemonic(&mut self, cx: &mut Ctx) {
        let Some(b) = self.active_buffer() else { return };
        let line = b.cursor_line_col().0;
        let current = b.bookmarks.get(&line).and_then(|bm| bm.mnemonic);
        let message = match current {
            Some(m) => format!(
                "Line {} is bookmark {m}: {m} removes it, another digit\nor letter replaces it (one used elsewhere moves here).",
                line + 1
            ),
            None => {
                format!("Press a digit or a letter to bookmark line {} with it.\nOne used elsewhere moves here; Esc cancels.", line + 1)
            }
        };
        let labels: Vec<String> = (0..10).map(|d| d.to_string()).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut dialog = Dialog::confirm(DialogId::BookmarkMnemonic(b.id, line), "Mnemonic Bookmark", &message, &labels).with_letter_keys();
        let used = self.used_mnemonics();
        let pick = match current {
            Some(Mnemonic::Digit(d)) => d,
            _ => PREFERRED.into_iter().find(|d| !used.contains(&Mnemonic::Digit(*d))).unwrap_or(1),
        };
        if let DialogKind::Confirm { selected, .. } = &mut dialog.kind {
            *selected = usize::from(pick);
        }
        cx.effects.push(Effect::Dialog(dialog));
    }

    /// Whether a bookmark has mnemonic `m`.
    pub fn has_mnemonic(&self, m: Mnemonic) -> bool {
        self.used_mnemonics().contains(&m)
    }

    /// The mnemonics in use, in open buffers and closed files.
    fn used_mnemonics(&self) -> Vec<Mnemonic> {
        let open = self.buffers.iter().flat_map(|b| b.bookmarks.values().filter_map(|bm| bm.mnemonic));
        open.chain(self.closed_bookmarks.values().flat_map(|lines| lines.values().filter_map(|bm| bm.mnemonic))).collect()
    }

    /// The answer of the Ctrl+F11 dialog: give mnemonic `m` to the bookmark
    /// on `line` of buffer `id` (making one), or remove that bookmark when
    /// it already has `m`. The bookmark that had `m` elsewhere goes.
    pub fn set_bookmark_mnemonic(&mut self, id: BufferId, line: usize, m: Mnemonic) {
        let Some(i) = self.index_of_id(id).filter(|&i| line < self.buffers[i].rope.len_lines()) else { return };
        if self.buffers[i].bookmarks.get(&line).is_some_and(|bm| bm.mnemonic == Some(m)) {
            self.buffers[i].bookmarks.remove(&line);
            return;
        }
        self.drop_mnemonic(m);
        self.buffers[i].bookmarks.entry(line).or_default().mnemonic = Some(m);
    }

    /// Remove the bookmark with mnemonic `m`, wherever it is.
    fn drop_mnemonic(&mut self, m: Mnemonic) {
        for b in &mut self.buffers {
            b.bookmarks.retain(|_, bm| bm.mnemonic != Some(m));
        }
        for lines in self.closed_bookmarks.values_mut() {
            lines.retain(|_, bm| bm.mnemonic != Some(m));
        }
        self.closed_bookmarks.retain(|_, lines| !lines.is_empty());
    }

    /// The named groups in use, across open buffers and closed files, in
    /// group order (case-insensitive alphabetical).
    pub fn groups_in_use(&self) -> Vec<String> {
        let open = self.buffers.iter().flat_map(|b| b.bookmarks.values());
        let closed = self.closed_bookmarks.values().flat_map(|lines| lines.values());
        let mut groups: Vec<String> = open.chain(closed).filter_map(|bm| bm.group.clone()).collect();
        groups.sort_by_key(|g| (g.to_lowercase(), g.clone()));
        groups.dedup();
        groups
    }

    /// Move Bookmark to Group: ask for the group of the caret line's bookmark.
    pub(super) fn choose_group(&mut self, cx: &mut Ctx) {
        let Some(b) = self.active_buffer() else { return };
        let line = b.cursor_line_col().0;
        let current = b.bookmarks.get(&line).and_then(|bm| bm.group.clone());
        let mut groups = vec![DEFAULT_GROUP.to_string()];
        groups.extend(self.groups_in_use());
        let mut items = groups.clone();
        items.push("New Group…".to_string());
        let selected = current.and_then(|g| groups.iter().position(|n| *n == g)).unwrap_or(0);
        let message = format!("Group of line {}:", line + 1);
        let mut dialog = Dialog::choice(DialogId::BookmarkGroup { buffer: b.id, line, groups }, "Move to Group", &message, items);
        if let DialogKind::Choice { list, .. } = &mut dialog.kind {
            list.select(selected);
        }
        cx.effects.push(Effect::Dialog(dialog));
    }

    /// Put the bookmark on `line` of buffer `id` into `group` (`None`: the
    /// default one); a line without a bookmark gets a plain one.
    pub fn move_bookmark_to_group(&mut self, id: BufferId, line: usize, group: Option<String>) {
        let Some(i) = self.index_of_id(id).filter(|&i| line < self.buffers[i].rope.len_lines()) else { return };
        self.buffers[i].bookmarks.entry(line).or_default().group = group;
    }

    /// Rename Bookmark Group: ask which named group to rename.
    pub(super) fn choose_group_to_rename(&mut self, cx: &mut Ctx) {
        let groups = self.groups_in_use();
        if groups.is_empty() {
            cx.notify(Level::Info, "No bookmark groups; Move Bookmark to Group makes one");
            return;
        }
        let items = groups.clone();
        cx.effects.push(Effect::Dialog(Dialog::choice(
            DialogId::RenameBookmarkGroupPick(groups),
            "Rename Bookmark Group",
            "Group to rename:",
            items,
        )));
    }

    /// Rename the group `old` to `new` (typed): every bookmark in it, open
    /// or closed, moves. The same name changes nothing.
    pub fn rename_bookmark_group(&mut self, old: &str, new: &str) -> Result<(), String> {
        let new = match group_name(new)? {
            Some(n) => n,
            None => return Err(format!("'{DEFAULT_GROUP}' is the default group")),
        };
        if new == old {
            return Ok(());
        }
        if self.groups_in_use().contains(&new) {
            return Err(format!("A bookmark group named '{new}' exists"));
        }
        let moved = |bm: &mut Bookmark| {
            if bm.group.as_deref() == Some(old) {
                bm.group = Some(new.clone());
            }
        };
        self.buffers.iter_mut().flat_map(|b| b.bookmarks.values_mut()).for_each(moved);
        self.closed_bookmarks.values_mut().flat_map(|lines| lines.values_mut()).for_each(moved);
        Ok(())
    }

    /// Keep the bookmarks of buffer `i`, which is closing.
    pub(super) fn park_bookmarks(&mut self, i: usize) {
        let b = &self.buffers[i];
        let Some(path) = &b.path else { return };
        if !b.bookmarks.is_empty() {
            self.closed_bookmarks.insert(path.clone(), b.bookmarks.clone());
        }
    }

    /// The bookmarks parked for `path`, line to bookmark, taken out of the
    /// closed list (the file is opening).
    pub(super) fn unpark_bookmarks(&mut self, path: &Path) -> BTreeMap<usize, Bookmark> {
        self.closed_bookmarks.remove(path).unwrap_or_default()
    }

    /// Give buffer `i` the numbered bookmarks of a session, (digit, line);
    /// a digit already in use, or a line past the end, is skipped.
    pub fn restore_digits(&mut self, i: usize, digits: &[(u8, usize)]) {
        for &(digit, line) in digits {
            if let Some(m) = Mnemonic::digit(digit) {
                self.restore_mnemonic(i, m, line);
            }
        }
    }

    /// Give buffer `i` the lettered bookmarks of a session, (letter, line);
    /// a letter that is refused or in use, a line past the end and a line
    /// that already has a mnemonic are skipped.
    pub fn restore_letters(&mut self, i: usize, letters: &[(char, usize)]) {
        for &(letter, line) in letters {
            if let Some(m) = Mnemonic::letter(letter) {
                self.restore_mnemonic(i, m, line);
            }
        }
    }

    /// Set mnemonic `m` on `line` of buffer `i` (making the bookmark when
    /// the line has none), unless `m` is in use or the line has a mnemonic.
    fn restore_mnemonic(&mut self, i: usize, m: Mnemonic, line: usize) {
        if self.has_mnemonic(m) {
            return;
        }
        let Some(b) = self.buffers.get_mut(i).filter(|b| line < b.rope.len_lines()) else { return };
        let bm = b.bookmarks.entry(line).or_default();
        if bm.mnemonic.is_none() {
            bm.mnemonic = Some(m);
        }
    }

    /// The bookmarks of closed files, by path then line.
    pub fn closed_bookmarks(&self) -> Vec<ClosedBookmark> {
        self.closed_bookmarks.iter().flat_map(|(p, lines)| lines.iter().map(|(l, b)| (p.clone(), *l, b.clone()))).collect()
    }

    /// The numbered bookmarks of closed files, (digit, path, line), by
    /// digit.
    pub fn closed_digits(&self) -> Vec<(u8, PathBuf, usize)> {
        let mut out: Vec<_> = self
            .closed_bookmarks()
            .into_iter()
            .filter_map(|(p, l, b)| match b.mnemonic {
                Some(Mnemonic::Digit(d)) => Some((d, p, l)),
                _ => None,
            })
            .collect();
        out.sort();
        out
    }

    /// The lettered bookmarks of closed files, (letter, path, line), by
    /// path then line.
    pub fn closed_letters(&self) -> Vec<(char, PathBuf, usize)> {
        self.closed_bookmarks()
            .into_iter()
            .filter_map(|(p, l, b)| match b.mnemonic {
                Some(Mnemonic::Letter(c)) => Some((c, p, l)),
                _ => None,
            })
            .collect()
    }

    /// The closed bookmarks outside the default group, (group, path, line),
    /// by path then line.
    pub fn closed_groups(&self) -> Vec<(String, PathBuf, usize)> {
        self.closed_bookmarks().into_iter().filter_map(|(p, l, b)| b.group.map(|g| (g, p, l))).collect()
    }

    /// Replace the bookmarks of closed files (a session loaded, or the root
    /// changed). A file that is open, a mnemonic already in use is skipped;
    /// a line given twice merges (`Bookmark::merge`).
    pub fn restore_closed_bookmarks(&mut self, bookmarks: &[ClosedBookmark]) {
        self.closed_bookmarks.clear();
        for (path, line, bookmark) in bookmarks {
            if self.index_of(path).is_some() || bookmark.mnemonic.is_some_and(|m| self.has_mnemonic(m)) {
                continue;
            }
            let bookmark = bookmark.clone();
            match self.closed_bookmarks.entry(path.clone()).or_default().entry(*line) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(bookmark);
                }
                std::collections::btree_map::Entry::Occupied(mut e) => e.get_mut().merge(bookmark),
            }
        }
    }

    /// Forget the bookmarks of closed files under `path` (it was deleted).
    pub(super) fn drop_closed_under(&mut self, path: &Path) {
        self.closed_bookmarks.retain(|p, _| !p.starts_with(path));
        self.closed_breakpoints.retain(|p, _| !p.starts_with(path));
    }

    /// Follow a rename of `from` to `to` in the closed files' bookmarks.
    pub(super) fn retarget_closed(&mut self, from: &Path, to: &Path) {
        let old = std::mem::take(&mut self.closed_bookmarks);
        for (p, lines) in old {
            let p = super::nav::moved_path(&p, from, to).unwrap_or(p);
            self.closed_bookmarks.entry(p).or_default().extend(lines);
        }
        for (p, lines) in std::mem::take(&mut self.closed_breakpoints) {
            let p = super::nav::moved_path(&p, from, to).unwrap_or(p);
            self.closed_breakpoints.entry(p).or_default().extend(lines);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Bookmark, Mnemonic, group_name};
    use crate::actions::{Action, EditorAction};
    use crate::app::App;
    use crate::app::tests::{handle_next, test_app};
    use crate::event::AppMsg;
    use crate::ui::dialog::{DialogId, DialogKind};
    use crate::ui::focus::PanelId;

    fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.handle_key(KeyEvent::new(code, mods));
    }

    fn plain() -> Bookmark {
        Bookmark::default()
    }

    fn digit(d: u8) -> Bookmark {
        Bookmark { mnemonic: Mnemonic::digit(d), group: None }
    }

    fn letter(c: char) -> Bookmark {
        Bookmark { mnemonic: Mnemonic::letter(c), group: None }
    }

    /// Open `name` (written to disk too) as the active tab.
    fn open(app: &mut App, name: &str, text: &str) {
        let path = app.workspace.root.join(name);
        std::fs::write(&path, text).unwrap();
        app.editor.open_text(path, text, false, &app.theme);
        app.focus.set(PanelId::Editor);
    }

    fn goto(app: &mut App, line: usize) {
        app.editor.active_buffer_mut().unwrap().goto_line(line, 0);
    }

    /// (file name, caret line) of the active tab.
    fn here(app: &App) -> (String, usize) {
        let b = app.editor.active_buffer().unwrap();
        (b.name.clone(), b.cursor_line_col().0)
    }

    /// Every numbered bookmark: (digit, file name, line), by digit.
    fn digits(app: &App) -> Vec<(u8, String, usize)> {
        let mut out: Vec<_> = app.editor.buffers.iter().flat_map(|b| b.digits().into_iter().map(|(d, l)| (d, b.name.clone(), l))).collect();
        out.extend(
            app.editor.closed_digits().into_iter().map(|(d, p, l)| (d, format!("{} (closed)", p.file_name().unwrap().display()), l)),
        );
        out.sort();
        out
    }

    /// Ctrl+F11, then `answer` in the digit chooser.
    fn number(app: &mut App, answer: KeyCode) {
        app.perform(Action::Editor(EditorAction::ToggleNumberedBookmark));
        assert!(matches!(app.dialog.as_ref().map(|d| &d.id), Some(DialogId::BookmarkMnemonic(..))));
        key(app, answer, KeyModifiers::NONE);
        assert!(app.dialog.is_none());
    }

    #[tokio::test]
    async fn digit_chooser_sets_moves_and_removes() {
        let (mut app, dir) = test_app("bookmark-digits");
        open(&mut app, "a.txt", "a0\na1\na2\na3\n");
        goto(&mut app, 2);
        // The chooser has a button per digit and preselects the first free
        // one, 1.
        app.perform(Action::Editor(EditorAction::ToggleNumberedBookmark));
        let Some(DialogKind::Confirm { buttons, selected }) = app.dialog.as_ref().map(|d| &d.kind) else { panic!("no chooser") };
        assert_eq!((buttons.len(), buttons[0].as_str(), *selected), (10, "0", 1));
        key(&mut app, KeyCode::Char('3'), KeyModifiers::NONE);
        assert_eq!(digits(&app), vec![(3, "a.txt".into(), 2)]);
        // Esc cancels: nothing changes.
        goto(&mut app, 0);
        number(&mut app, KeyCode::Esc);
        assert_eq!(digits(&app), vec![(3, "a.txt".into(), 2)]);
        // A plain bookmark on line 0 gets digit 1 by Enter on the preselected button.
        app.perform(Action::Editor(EditorAction::ToggleBookmark));
        number(&mut app, KeyCode::Enter);
        assert_eq!(digits(&app), vec![(1, "a.txt".into(), 0), (3, "a.txt".into(), 2)]);
        // Digit 3 given to a line of another file moves there: the old
        // bookmark goes.
        open(&mut app, "b.txt", "b0\nb1\n");
        goto(&mut app, 1);
        number(&mut app, KeyCode::Char('3'));
        assert_eq!(digits(&app), vec![(1, "a.txt".into(), 0), (3, "b.txt".into(), 1)]);
        assert_eq!(app.editor.buffers[0].bookmarks.keys().copied().collect::<Vec<_>>(), vec![0]);
        // Another digit renumbers the line; its own digit removes the bookmark.
        number(&mut app, KeyCode::Char('5'));
        assert_eq!(digits(&app), vec![(1, "a.txt".into(), 0), (5, "b.txt".into(), 1)]);
        number(&mut app, KeyCode::Char('5'));
        assert_eq!(digits(&app), vec![(1, "a.txt".into(), 0)]);
        assert!(app.editor.active_buffer().unwrap().bookmarks.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn jumps_across_files_follow_edits_and_reopen_closed_files() {
        let (mut app, dir) = test_app("bookmark-jumps");
        open(&mut app, "a.txt", &"a\n".repeat(30));
        goto(&mut app, 20);
        number(&mut app, KeyCode::Char('1'));
        open(&mut app, "b.txt", "b0\nb1\nb2\n");
        goto(&mut app, 2);
        // Ctrl+1 (kitty protocol) goes to a.txt line 20; back returns.
        key(&mut app, KeyCode::Char('1'), KeyModifiers::CONTROL);
        assert_eq!(here(&app), ("a.txt".into(), 20));
        // Lines typed above the bookmark move it.
        goto(&mut app, 0);
        app.editor.active_buffer_mut().unwrap().insert("new\nnew\n", crate::editor::undo::TxKind::Other);
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!(here(&app), ("b.txt".into(), 2));
        // Alt+B 1 (any terminal) follows.
        key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        key(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(here(&app), ("a.txt".into(), 22));
        // A jump from a tool window takes the keyboard to the editor.
        app.focus.set(PanelId::Project);
        app.perform(Action::Editor(EditorAction::ShowBookmarks));
        app.dialog = None;
        // Close All keeps numbered bookmarks too.
        let mut all = test_app("bookmark-close-all");
        open(&mut all.0, "c.txt", "c\n");
        number(&mut all.0, KeyCode::Char('2'));
        all.0.editor.force_close_all();
        assert_eq!(digits(&all.0), vec![(2, "c.txt (closed)".into(), 0)]);
        let _ = std::fs::remove_dir_all(&all.1);
        // Closing a.txt keeps its numbered bookmark; Ctrl+1 reopens it there.
        app.editor.force_close(app.editor.index_of(&app.workspace.root.join("a.txt")).unwrap());
        assert_eq!(digits(&app), vec![(1, "a.txt (closed)".into(), 22)]);
        key(&mut app, KeyCode::Char('1'), KeyModifiers::CONTROL);
        assert!(app.focus.is(PanelId::Editor));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(here(&app), ("a.txt".into(), 22));
        assert_eq!(digits(&app), vec![(1, "a.txt".into(), 22)], "the digit is back in the buffer");
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!(here(&app).0, "b.txt");
        // A digit nobody has only notifies.
        key(&mut app, KeyCode::Char('9'), KeyModifiers::CONTROL);
        assert_eq!(here(&app).0, "b.txt");
        assert!(app.notifications.iter().any(|n| n.text.starts_with("No bookmark 9")), "{:?}", app.notifications);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plain_bookmarks_of_a_closed_file_come_back() {
        let (mut app, dir) = test_app("bookmark-plain-closed");
        open(&mut app, "a.txt", "a0\na1\na2\na3\n");
        goto(&mut app, 1);
        app.perform(Action::Editor(EditorAction::ToggleBookmark));
        goto(&mut app, 3);
        number(&mut app, KeyCode::Char('6'));
        let a = app.workspace.root.join("a.txt");
        app.editor.force_close(app.editor.index_of(&a).unwrap());
        assert_eq!(app.editor.closed_bookmarks(), vec![(a.clone(), 1, plain()), (a.clone(), 3, digit(6))]);
        // Reopened (shorter on disk now): both are back, the numbered one
        // on the last line.
        app.editor.open_text(a.clone(), "a0\na1\na2", false, &app.theme);
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.bookmarks.iter().map(|(l, b)| (*l, b.clone())).collect::<Vec<_>>(), vec![(1, plain()), (2, digit(6))]);
        assert!(app.editor.closed_bookmarks().is_empty());
        // Close All parks them too; a rename follows; a delete drops them.
        app.editor.force_close_all();
        let moved = app.workspace.root.join("b.txt");
        app.editor.retarget(&a, &moved, &app.theme);
        assert_eq!(app.editor.closed_bookmarks(), vec![(moved.clone(), 1, plain()), (moved.clone(), 2, digit(6))]);
        app.editor.close_under(&moved);
        assert!(app.editor.closed_bookmarks().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn ctrl_7_comments_in_legacy_terminals_and_jumps_with_kitty() {
        let (mut app, dir) = test_app("bookmark-ctrl-7");
        open(&mut app, "a.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        goto(&mut app, 2);
        number(&mut app, KeyCode::Char('7'));
        goto(&mut app, 0);
        let line0 = |app: &App| app.editor.active_buffer().unwrap().line_text(0).trim_end().to_string();
        // Legacy terminal: Ctrl+/ arrives as Ctrl+7 and comments.
        assert!(!app.keyboard_enhanced);
        key(&mut app, KeyCode::Char('7'), KeyModifiers::CONTROL);
        assert_eq!((line0(&app), here(&app)), ("// fn a() {}".into(), ("a.rs".into(), 1)), "commented, the caret moves down");
        goto(&mut app, 0);
        key(&mut app, KeyCode::Char('/'), KeyModifiers::CONTROL);
        assert_eq!(line0(&app), "fn a() {}");
        // Kitty protocol: Ctrl+7 is bookmark 7, Ctrl+/ still comments.
        app.keyboard_enhanced = true;
        goto(&mut app, 0);
        key(&mut app, KeyCode::Char('7'), KeyModifiers::CONTROL);
        assert_eq!((line0(&app), here(&app)), ("fn a() {}".into(), ("a.rs".into(), 2)));
        goto(&mut app, 0);
        key(&mut app, KeyCode::Char('/'), KeyModifiers::CONTROL);
        assert_eq!(line0(&app), "// fn a() {}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bookmarks_dialog_lists_digits_and_closed_files() {
        let (mut app, dir) = test_app("bookmark-list");
        open(&mut app, "a.txt", "one\ntwo\n");
        app.perform(Action::Editor(EditorAction::ToggleBookmark));
        goto(&mut app, 1);
        number(&mut app, KeyCode::Char('4'));
        open(&mut app, "b.txt", "x\ny\n");
        app.perform(Action::Editor(EditorAction::ToggleBookmark));
        goto(&mut app, 1);
        number(&mut app, KeyCode::Char('2'));
        app.editor.force_close(app.editor.index_of(&app.workspace.root.join("b.txt")).unwrap());
        app.perform(Action::Editor(EditorAction::ShowBookmarks));
        let Some(DialogKind::Choice { items, .. }) = app.dialog.as_ref().map(|d| &d.kind) else { panic!("no dialog") };
        assert_eq!(items, &["◆ a.txt:1 one", "4 a.txt:2 two", "◆ b.txt:1 (closed)", "2 b.txt:2 (closed)"]);
        // Choosing the closed one opens its file at the line.
        key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        key(&mut app, KeyCode::End, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(here(&app), ("b.txt".into(), 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The active buffer's bookmarks: (line, bookmark).
    fn marks(app: &App) -> Vec<(usize, Bookmark)> {
        app.editor.active_buffer().unwrap().bookmarks.iter().map(|(l, b)| (*l, b.clone())).collect()
    }

    /// Ctrl+F11, then `answer` with `mods` in the mnemonic chooser.
    fn mnemonic(app: &mut App, answer: KeyCode, mods: KeyModifiers) {
        app.perform(Action::Editor(EditorAction::ToggleNumberedBookmark));
        assert!(matches!(app.dialog.as_ref().map(|d| &d.id), Some(DialogId::BookmarkMnemonic(..))));
        key(app, answer, mods);
        assert!(app.dialog.is_none());
    }

    /// The items of the open Choice dialog.
    fn items(app: &App) -> Vec<String> {
        let Some(DialogKind::Choice { items, .. }) = app.dialog.as_ref().map(|d| &d.kind) else { panic!("no choice dialog") };
        items.clone()
    }

    /// Type `text` and Enter in the open input dialog.
    fn type_in(app: &mut App, text: &str) {
        assert!(matches!(app.dialog.as_ref().map(|d| &d.kind), Some(DialogKind::Input { .. })), "no input dialog");
        for c in text.chars() {
            key(app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        key(app, KeyCode::Enter, KeyModifiers::NONE);
    }

    /// Move Bookmark to Group, then pick item `i` of its chooser.
    fn move_to_group(app: &mut App, i: usize) {
        app.perform(Action::Editor(EditorAction::MoveBookmarkToGroup));
        assert!(matches!(app.dialog.as_ref().map(|d| &d.id), Some(DialogId::BookmarkGroup { .. })));
        key(app, KeyCode::Home, KeyModifiers::NONE);
        for _ in 0..i {
            key(app, KeyCode::Down, KeyModifiers::NONE);
        }
        key(app, KeyCode::Enter, KeyModifiers::NONE);
    }

    /// Rename Bookmark Group: pick `old` and type `new`.
    fn rename_group(app: &mut App, old: &str, new: &str) {
        app.perform(Action::Editor(EditorAction::RenameBookmarkGroup));
        let at = items(app).iter().position(|g| g == old).unwrap();
        for _ in 0..at {
            key(app, KeyCode::Down, KeyModifiers::NONE);
        }
        key(app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.dialog.as_ref().map(|d| &d.id), Some(DialogId::RenameBookmarkGroup(o)) if o == old));
        key(app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        type_in(app, new);
    }

    #[test]
    fn mnemonics_groups_and_merges() {
        assert_eq!(Mnemonic::letter('k'), Some(Mnemonic::Letter('K')));
        assert_eq!((Mnemonic::letter('1'), Mnemonic::letter('é'), Mnemonic::digit(10)), (None, None, None));
        assert_eq!(Mnemonic::digit(3), Some(Mnemonic::Digit(3)));
        assert!(Mnemonic::Digit(9) < Mnemonic::Letter('A'));
        assert_eq!((Mnemonic::Digit(4).glyph(), Mnemonic::Letter('Q').to_string()), ('4', "Q".to_string()));
        let named = |g: &str| Bookmark { mnemonic: None, group: Some(g.into()) };
        let lettered = |g: Option<&str>| Bookmark { mnemonic: Mnemonic::letter('k'), group: g.map(String::from) };
        // Plain into plain keeps the first.
        let mut b = named("X");
        b.merge(plain());
        assert_eq!(b, named("X"));
        // A mnemonic replaces a plain one whole, its group with it.
        let mut b = named("X");
        b.merge(lettered(None));
        assert_eq!(b, lettered(None));
        // A plain one never replaces a mnemonic.
        let mut b = lettered(Some("Y"));
        b.merge(named("X"));
        assert_eq!(b, lettered(Some("Y")));
        assert_eq!(group_name("  API "), Ok(Some("API".to_string())));
        assert!(group_name("").is_err() && group_name("   ").is_err());
        assert!(group_name("a\tb").is_err());
        assert_eq!(group_name("Bookmarks"), Ok(None));
    }

    #[tokio::test]
    async fn letter_chooser_sets_moves_and_removes() {
        let (mut app, dir) = test_app("bookmark-letters");
        open(&mut app, "a.txt", "a0\na1\na2\na3\n");
        let none = KeyModifiers::NONE;
        goto(&mut app, 2);
        mnemonic(&mut app, KeyCode::Char('k'), none);
        assert_eq!(marks(&app), vec![(2, letter('K'))]);
        // Shift+K answers too, and K moves: the old bookmark goes whole.
        goto(&mut app, 0);
        mnemonic(&mut app, KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(marks(&app), vec![(0, letter('K'))]);
        // A digit replaces the letter, which is free again.
        mnemonic(&mut app, KeyCode::Char('4'), none);
        assert_eq!(marks(&app), vec![(0, digit(4))]);
        goto(&mut app, 1);
        mnemonic(&mut app, KeyCode::Char('k'), none);
        assert_eq!(marks(&app), vec![(0, digit(4)), (1, letter('K'))]);
        // Its own letter removes the bookmark.
        mnemonic(&mut app, KeyCode::Char('k'), none);
        assert_eq!(marks(&app), vec![(0, digit(4))]);
        // Esc cancels.
        mnemonic(&mut app, KeyCode::Esc, none);
        assert_eq!(marks(&app), vec![(0, digit(4))]);
        // Ctrl+letter is no answer: the dialog stays open.
        app.perform(Action::Editor(EditorAction::ToggleNumberedBookmark));
        key(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(app.dialog.is_some());
        key(&mut app, KeyCode::Esc, none);
        // A lettered line in group API keeps it when a digit replaces the letter.
        goto(&mut app, 3);
        mnemonic(&mut app, KeyCode::Char('k'), none);
        move_to_group(&mut app, 0);
        let last = marks(&app).last().unwrap().clone();
        assert_eq!(last, (3, letter('K')), "moving to the default group changes nothing");
        app.editor.move_bookmark_to_group(app.editor.active_buffer().unwrap().id, 3, Some("API".into()));
        mnemonic(&mut app, KeyCode::Char('7'), none);
        assert_eq!(marks(&app).last().unwrap().1, Bookmark { mnemonic: Mnemonic::digit(7), group: Some("API".into()) });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_b_letter_jumps_and_reopens_closed_files() {
        let (mut app, dir) = test_app("bookmark-letter-jumps");
        open(&mut app, "a.txt", &"a\n".repeat(30));
        goto(&mut app, 20);
        mnemonic(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        open(&mut app, "b.txt", "b0\nb1\nb2\n");
        goto(&mut app, 2);
        key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(here(&app), ("a.txt".into(), 20));
        // Lines typed above the bookmark move it.
        goto(&mut app, 0);
        app.editor.active_buffer_mut().unwrap().insert("new\nnew\n", crate::editor::undo::TxKind::Other);
        app.perform(Action::Editor(EditorAction::NavBack));
        assert_eq!(here(&app), ("b.txt".into(), 2), "back returns");
        key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(here(&app), ("a.txt".into(), 22));
        // Closed: the letter waits with its line.
        app.editor.force_close(app.editor.index_of(&app.workspace.root.join("a.txt")).unwrap());
        let a = app.workspace.root.join("a.txt");
        assert_eq!(app.editor.closed_bookmarks(), vec![(a, 22, letter('Q'))]);
        // From the Project panel the jump takes focus and reopens the file.
        app.focus.set(PanelId::Project);
        key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.focus.is(PanelId::Editor));
        handle_next(&mut app, |m| matches!(m, AppMsg::FileLoaded { .. })).await;
        assert_eq!(here(&app), ("a.txt".into(), 22));
        assert_eq!(marks(&app), vec![(22, letter('Q'))], "the letter is back in the buffer");
        // A letter nobody has only notifies.
        app.focus.set(PanelId::Project);
        key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        key(&mut app, KeyCode::Char('z'), KeyModifiers::NONE);
        assert!(app.focus.is(PanelId::Project));
        assert!(app.notifications.iter().any(|n| n.text.starts_with("No bookmark Z")), "{:?}", app.notifications);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Alt+B then `q` with `focus` set by `setup`: the letter is the keymap's, not the window's field.
    fn jump_from(app: &mut App, name: &str, setup: impl Fn(&mut App)) {
        app.perform(Action::Editor(EditorAction::NavBack));
        goto(app, 2);
        assert_eq!(here(app), ("b.txt".into(), 2), "{name}: start");
        setup(app);
        key(app, KeyCode::Char('b'), KeyModifiers::ALT);
        key(app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(here(app), ("a.txt".into(), 20), "{name}: the jump");
        assert!(app.focus.is(PanelId::Editor), "{name}: focus");
    }

    #[tokio::test]
    async fn alt_b_letter_jumps_from_every_window_with_a_field() {
        let (mut app, dir) = test_app("bookmark-letter-fields");
        open(&mut app, "a.txt", &"a\n".repeat(30));
        goto(&mut app, 20);
        mnemonic(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        open(&mut app, "b.txt", "b0\nb1\nb2\n");
        goto(&mut app, 2);
        key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(here(&app), ("a.txt".into(), 20));
        jump_from(&mut app, "find window", |a| a.focus.set(PanelId::Find));
        jump_from(&mut app, "project", |a| a.focus.set(PanelId::Project));
        jump_from(&mut app, "local history", |a| {
            let (path, root) = (a.workspace.root.join("b.txt"), a.workspace.root.clone());
            a.history_panel.set_target(path, &root);
            a.focus.set(PanelId::LocalHistory);
        });
        jump_from(&mut app, "tasks", |a| a.focus.set(PanelId::Tasks));
        // The editor's find bar and the commit message box type everything else.
        jump_from(&mut app, "find bar", |a| {
            a.perform(Action::Editor(EditorAction::Find));
            assert!(a.editor.find_focused());
        });
        jump_from(&mut app, "commit message", |a| {
            a.perform(Action::Git(crate::actions::GitAction::FocusMessage));
            assert!(a.git_panel.message_focused());
        });
        assert!(app.git_panel.message.text.is_empty(), "the letter did not reach the box");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn groups_move_rename_and_list() {
        let (mut app, dir) = test_app("bookmark-groups");
        // Nothing to rename in a fresh app.
        app.perform(Action::Editor(EditorAction::RenameBookmarkGroup));
        assert!(app.dialog.is_none());
        assert!(app.notifications.iter().any(|n| n.text == "No bookmark groups; Move Bookmark to Group makes one"));
        open(&mut app, "a.txt", "one\ntwo\n");
        app.perform(Action::Editor(EditorAction::ToggleBookmark));
        goto(&mut app, 1);
        mnemonic(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        // Move to a new group: the chooser lists the default group and New Group….
        app.perform(Action::Editor(EditorAction::MoveBookmarkToGroup));
        assert_eq!(items(&app), ["Bookmarks", "New Group…"]);
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        move_to_group(&mut app, 1);
        type_in(&mut app, "API");
        assert_eq!(marks(&app)[1].1, Bookmark { mnemonic: Mnemonic::letter('k'), group: Some("API".into()) });
        open(&mut app, "b.txt", "x\ny\n");
        // No bookmark on line 0 yet: New Group… makes a plain one.
        move_to_group(&mut app, 2);
        type_in(&mut app, "later");
        assert_eq!(marks(&app), vec![(0, Bookmark { mnemonic: None, group: Some("later".into()) })]);
        goto(&mut app, 1);
        mnemonic(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        app.perform(Action::Editor(EditorAction::MoveBookmarkToGroup));
        assert_eq!(items(&app), ["Bookmarks", "API", "later", "New Group…"]);
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.editor.force_close(app.editor.index_of(&app.workspace.root.join("b.txt")).unwrap());
        let listed = |app: &mut App| {
            app.perform(Action::Editor(EditorAction::ShowBookmarks));
            let items = items(app);
            app.dialog = None;
            items
        };
        assert_eq!(
            listed(&mut app),
            ["Bookmarks: ◆ a.txt:1 one", "API: K a.txt:2 two", "API: 2 b.txt:2 (closed)", "later: ◆ b.txt:1 (closed)"]
        );
        // Rename API to Net: the closed bookmark follows.
        rename_group(&mut app, "API", "Net");
        assert_eq!(
            listed(&mut app),
            ["Bookmarks: ◆ a.txt:1 one", "later: ◆ b.txt:1 (closed)", "Net: K a.txt:2 two", "Net: 2 b.txt:2 (closed)"]
        );
        // A group in use, the default group and an empty name are refused; nothing changes.
        for (new, warn) in [
            ("later", "A bookmark group named 'later' exists"),
            ("Bookmarks", "'Bookmarks' is the default group"),
            ("  ", "A bookmark group needs a name"),
        ] {
            app.notifications.clear();
            rename_group(&mut app, "Net", new);
            assert!(app.notifications.iter().any(|n| n.text == warn), "{new:?}: {:?}", app.notifications);
            assert_eq!(app.editor.groups_in_use(), ["later", "Net"]);
        }
        // The same name is fine and changes nothing.
        rename_group(&mut app, "Net", "Net");
        assert_eq!(app.editor.groups_in_use(), ["later", "Net"]);
        // Back to the default group: the line has no group; Net lives on in the closed file.
        goto(&mut app, 1);
        move_to_group(&mut app, 0);
        assert_eq!(marks(&app).last().unwrap().1.group, None);
        // Reopen b.txt: its groups are back in the buffer.
        app.editor.open_text(app.workspace.root.join("b.txt"), "x\ny\n", false, &app.theme);
        assert_eq!(
            marks(&app),
            vec![
                (0, Bookmark { mnemonic: None, group: Some("later".into()) }),
                (1, Bookmark { mnemonic: Mnemonic::digit(2), group: Some("Net".into()) })
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
