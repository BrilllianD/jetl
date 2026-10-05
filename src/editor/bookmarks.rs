//! Bookmarks across buffers: the chooser (Shift+F11), numbered bookmarks
//! (Ctrl+F11 sets a digit, Ctrl+0..9 or Alt+B then a digit jumps) and the
//! bookmarks of closed files.
//!
//! A digit belongs to at most one bookmark of the session: giving it to a
//! line removes the bookmark that had it (JetBrains). Line bookmarks live in
//! `Buffer::bookmarks`, so they follow edits; when a file's last tab closes
//! its bookmarks, plain and numbered, wait in `Editor::closed_bookmarks` (by
//! path and line) and go back into the buffer when the file opens again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Editor;
use super::buffer::BufferId;
use super::nav::PendingAt;
use crate::app::Ctx;
use crate::event::{Effect, Level};
use crate::ui::dialog::{Dialog, DialogId, DialogKind};

/// Where an item of the bookmarks dialog goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkTarget {
    /// A line of an open buffer.
    Open(BufferId, usize),
    /// A bookmarked line of a closed file.
    Closed(PathBuf, usize),
}

/// A bookmark of a closed file: its path, 0-based line and digit.
pub type ClosedBookmark = (PathBuf, usize, Option<u8>);

/// The gutter glyph of a bookmark: its digit, else ◆.
fn glyph(digit: Option<u8>) -> char {
    digit.map_or('◆', |d| char::from(b'0' + d))
}

/// The digit preselected for a new numbered bookmark:
/// the first free one, 1 to 9, then 0.
const PREFERRED: [u8; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 0];

impl Editor {
    /// Shift+F11: a choice dialog of the bookmarks of all open buffers,
    /// then those of closed files.
    pub(super) fn show_bookmarks(&mut self, cx: &mut Ctx) {
        let root = cx.workspace.root();
        let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        let mut targets = Vec::new();
        let mut items = Vec::new();
        for b in &self.buffers {
            let name = b.path.as_deref().map(rel).unwrap_or_else(|| b.name.clone());
            for (&line, &digit) in &b.bookmarks {
                items.push(format!("{} {name}:{} {}", glyph(digit), line + 1, b.line_text(line).trim()));
                targets.push(BookmarkTarget::Open(b.id, line));
            }
        }
        for (path, lines) in &self.closed_bookmarks {
            for (&line, &digit) in lines {
                items.push(format!("{} {}:{} (closed)", glyph(digit), rel(path), line + 1));
                targets.push(BookmarkTarget::Closed(path.clone(), line));
            }
        }
        if items.is_empty() {
            cx.notify(Level::Info, "No bookmarks; F11 toggles one on the caret line, Ctrl+F11 a numbered one");
            return;
        }
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

    /// Ctrl+0..9: jump to the bookmark numbered `digit`, opening its file
    /// when it was closed (the caret goes to its line once it loads).
    pub(super) fn goto_digit(&mut self, digit: u8, cx: &mut Ctx) {
        if self.merge.is_some() {
            return;
        }
        if let Some((id, line)) = self.buffers.iter().find_map(|b| b.digit_line(digit).map(|l| (b.id, l))) {
            self.goto_bookmark(id, line);
        } else if let Some((path, line)) = self.closed_digit(digit) {
            self.open_closed_at(path, line, cx);
        } else {
            cx.notify(Level::Info, format!("No bookmark {digit}; Ctrl+F11 numbers the caret line"));
        }
    }

    /// The closed file's bookmark numbered `digit`: its path and line.
    fn closed_digit(&self, digit: u8) -> Option<(PathBuf, usize)> {
        self.closed_bookmarks.iter().find_map(|(p, lines)| lines.iter().find(|(_, d)| **d == Some(digit)).map(|(l, _)| (p.clone(), *l)))
    }

    /// Ctrl+F11: ask for the digit of the caret line's bookmark. Each digit
    /// is a button, so pressing it answers; Esc cancels.
    pub(super) fn choose_digit(&mut self, cx: &mut Ctx) {
        let Some(b) = self.active_buffer() else { return };
        let line = b.cursor_line_col().0;
        let current = b.bookmarks.get(&line).copied().flatten();
        let message = match current {
            Some(d) => {
                format!("Line {} is bookmark {d}: {d} removes it, another digit\nrenumbers it (one used elsewhere moves here).", line + 1)
            }
            None => format!("Press a digit to bookmark line {} with it.\nA digit used elsewhere moves here; Esc cancels.", line + 1),
        };
        let labels: Vec<String> = (0..10).map(|d| d.to_string()).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut dialog = Dialog::confirm(DialogId::BookmarkDigit(b.id, line), "Numbered Bookmark", &message, &labels);
        let used = self.used_digits();
        let pick = current.or_else(|| PREFERRED.into_iter().find(|d| !used.contains(d))).unwrap_or(1);
        if let DialogKind::Confirm { selected, .. } = &mut dialog.kind {
            *selected = usize::from(pick);
        }
        cx.effects.push(Effect::Dialog(dialog));
    }

    /// Whether a bookmark has `digit`.
    pub fn has_digit(&self, digit: u8) -> bool {
        self.used_digits().contains(&digit)
    }

    /// The digits in use, in open buffers and closed files.
    fn used_digits(&self) -> Vec<u8> {
        let open = self.buffers.iter().flat_map(|b| b.digits().into_iter().map(|(d, _)| d));
        open.chain(self.closed_bookmarks.values().flat_map(|lines| lines.values().filter_map(|d| *d))).collect()
    }

    /// The answer of the Ctrl+F11 dialog: give `digit` to the bookmark on
    /// `line` of buffer `id` (making one), or remove that bookmark when it
    /// already has `digit`. The bookmark that had the digit elsewhere goes.
    pub fn set_bookmark_digit(&mut self, id: BufferId, line: usize, digit: u8) {
        let Some(i) = self.index_of_id(id).filter(|&i| line < self.buffers[i].rope.len_lines()) else { return };
        if self.buffers[i].bookmarks.get(&line) == Some(&Some(digit)) {
            self.buffers[i].bookmarks.remove(&line);
            return;
        }
        self.drop_digit(digit);
        self.buffers[i].bookmarks.insert(line, Some(digit));
    }

    /// Remove the bookmark numbered `digit`, wherever it is.
    fn drop_digit(&mut self, digit: u8) {
        for b in &mut self.buffers {
            b.bookmarks.retain(|_, d| *d != Some(digit));
        }
        for lines in self.closed_bookmarks.values_mut() {
            lines.retain(|_, d| *d != Some(digit));
        }
        self.closed_bookmarks.retain(|_, lines| !lines.is_empty());
    }

    /// Keep the bookmarks of buffer `i`, which is closing.
    pub(super) fn park_bookmarks(&mut self, i: usize) {
        let b = &self.buffers[i];
        let Some(path) = &b.path else { return };
        if !b.bookmarks.is_empty() {
            self.closed_bookmarks.insert(path.clone(), b.bookmarks.clone());
        }
    }

    /// The bookmarks parked for `path`, line to digit, taken out of the
    /// closed list (the file is opening).
    pub(super) fn unpark_bookmarks(&mut self, path: &Path) -> BTreeMap<usize, Option<u8>> {
        self.closed_bookmarks.remove(path).unwrap_or_default()
    }

    /// Give buffer `i` the numbered bookmarks of a session, (digit, line);
    /// a digit already in use, or a line past the end, is skipped.
    pub fn restore_digits(&mut self, i: usize, digits: &[(u8, usize)]) {
        for &(digit, line) in digits {
            if digit > 9 || self.used_digits().contains(&digit) {
                continue;
            }
            let Some(b) = self.buffers.get_mut(i).filter(|b| line < b.rope.len_lines()) else { continue };
            b.bookmarks.insert(line, Some(digit));
        }
    }

    /// The bookmarks of closed files, by path then line.
    pub fn closed_bookmarks(&self) -> Vec<ClosedBookmark> {
        self.closed_bookmarks.iter().flat_map(|(p, lines)| lines.iter().map(|(l, d)| (p.clone(), *l, *d))).collect()
    }

    /// The numbered bookmarks of closed files, (digit, path, line), by
    /// digit.
    pub fn closed_digits(&self) -> Vec<(u8, PathBuf, usize)> {
        let mut out: Vec<_> = self.closed_bookmarks().into_iter().filter_map(|(p, l, d)| d.map(|d| (d, p, l))).collect();
        out.sort();
        out
    }

    /// Replace the bookmarks of closed files (a session loaded, or the root
    /// changed). A file that is open, a digit past 9 or already in use is
    /// skipped; a line given twice keeps its first digit.
    pub fn restore_closed_bookmarks(&mut self, bookmarks: &[ClosedBookmark]) {
        self.closed_bookmarks.clear();
        for (path, line, digit) in bookmarks {
            if self.index_of(path).is_some() || digit.is_some_and(|d| d > 9 || self.used_digits().contains(&d)) {
                continue;
            }
            let slot = self.closed_bookmarks.entry(path.clone()).or_default().entry(*line).or_insert(*digit);
            if slot.is_none() {
                *slot = *digit;
            }
        }
    }

    /// Forget the bookmarks of closed files under `path` (it was deleted).
    pub(super) fn drop_closed_under(&mut self, path: &Path) {
        self.closed_bookmarks.retain(|p, _| !p.starts_with(path));
    }

    /// Follow a rename of `from` to `to` in the closed files' bookmarks.
    pub(super) fn retarget_closed(&mut self, from: &Path, to: &Path) {
        let old = std::mem::take(&mut self.closed_bookmarks);
        for (p, lines) in old {
            let p = super::nav::moved_path(&p, from, to).unwrap_or(p);
            self.closed_bookmarks.entry(p).or_default().extend(lines);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::actions::{Action, EditorAction};
    use crate::app::App;
    use crate::app::tests::{handle_next, test_app};
    use crate::event::AppMsg;
    use crate::ui::dialog::{DialogId, DialogKind};
    use crate::ui::focus::PanelId;

    fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.handle_key(KeyEvent::new(code, mods));
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
        assert!(matches!(app.dialog.as_ref().map(|d| &d.id), Some(DialogId::BookmarkDigit(..))));
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
        assert_eq!(app.editor.closed_bookmarks(), vec![(a.clone(), 1, None), (a.clone(), 3, Some(6))]);
        // Reopened (shorter on disk now): both are back, the numbered one
        // on the last line.
        app.editor.open_text(a.clone(), "a0\na1\na2", false, &app.theme);
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.bookmarks.iter().map(|(l, d)| (*l, *d)).collect::<Vec<_>>(), vec![(1, None), (2, Some(6))]);
        assert!(app.editor.closed_bookmarks().is_empty());
        // Close All parks them too; a rename follows; a delete drops them.
        app.editor.force_close_all();
        let moved = app.workspace.root.join("b.txt");
        app.editor.retarget(&a, &moved);
        assert_eq!(app.editor.closed_bookmarks(), vec![(moved.clone(), 1, None), (moved.clone(), 2, Some(6))]);
        app.editor.close_under(&moved);
        assert!(app.editor.closed_bookmarks().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
}
