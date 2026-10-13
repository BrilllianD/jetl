//! Render snapshots: whole frames (or the editor area) drawn on a
//! `TestBackend` and compared as text with `insta` (`app/snapshots/`).
//! The format and its marks are in `ui/snapshot.rs`; how to update them is
//! in `docs/TESTING.md`.
//!
//! Every test starts from [`stable_app`] and draws through [`draw_app`] or
//! [`draw_editor`], which settle the app first: background results that
//! `App::new` asked for are dropped unhandled, so nothing on screen depends
//! on how fast they arrive.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use insta::assert_snapshot;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use super::tests::{load, test_app};
use super::{App, Ctx};
use crate::actions::{Action, EditorAction, HistoryAction, MergeAction, Motion, PaletteKind};
use crate::config::theme::Theme;
use crate::editor::selection::Selection;
use crate::event::AppMsg;
use crate::git::model::{ChangeEntry, FileStatus};
use crate::git::{DiffKey, GitMsg, Section, StatusSnapshot};
use crate::ui::focus::{PanelId, ToolWindowId};
use crate::ui::snapshot::{Mark, snapshot};

/// A test app and its temp directory, removed on drop.
struct Fixture {
    app: App,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// [`test_app`] with nothing that depends on the terminal or the clock:
/// the kitty keyboard protocol on (no "legacy keys" part, no warning), no
/// notifications or toasts, and the startup requests' results dropped.
fn stable_app(name: &str) -> Fixture {
    let (mut app, dir) = test_app(&format!("snap-{name}"));
    app.keyboard_enhanced = true;
    app.notifications.clear();
    app.notification_shown = false;
    app.event_log.clear();
    app.toasts = Default::default();
    drain(&mut app);
    Fixture { app, dir }
}

/// Drop whatever background work has sent so far, unhandled.
fn drain(app: &mut App) {
    while app.bus_rx.try_recv().is_ok() {}
}

/// Make the app ready to draw: drop background results, bring the gutters
/// up to date (the run loop does it at tick rate). A notification would
/// show for a time that depends on the test's speed, so there must be none.
fn settle(app: &mut App) {
    drain(app);
    app.editor.update_gutters();
    assert!(app.notifications.is_empty(), "a notification would make the frame depend on timing: {:?}", app.notifications);
}

/// The whole frame at `w`x`h`.
fn draw_app(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
    settle(app);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| crate::ui::render(app, f)).unwrap();
    term
}

/// Only the editor, focused: its tab bars on the top row and its groups
/// below, without the tool strip, panels and status bar.
fn draw_editor(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
    settle(app);
    let theme = app.theme.clone();
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.editor.render(f, Rect::new(0, 0, w, 1), Rect::new(0, 1, w, h - 1), true, &theme)).unwrap();
    term
}

/// Carets other than the terminal cursor, selections, fold placeholders
/// and occurrence highlights.
fn editor_marks(theme: &Theme) -> Vec<Mark> {
    vec![
        Mark::modifier('C', "caret", Modifier::REVERSED),
        Mark::bg('S', "selection", theme.selection_bg),
        Mark::style('F', "fold", theme.fold_placeholder),
        Mark::bg('O', "occurrence", theme.occurrence_bg),
    ]
}

fn editor(app: &mut App, a: EditorAction) {
    app.perform(Action::Editor(a));
}

fn moves(app: &mut App, m: Motion, n: usize) {
    for _ in 0..n {
        editor(app, EditorAction::Move(m, false));
    }
}

/// Plain text with no grammar and no soft wrap by default (unlike `.txt`),
/// so the frame is the same with any set of language features.
const PLAIN: &str = "alpha beta gamma\n  two indented\n  three indented\nfour\n\nsix: the last line of the file\n";

#[tokio::test]
async fn editor_split_right() {
    let mut fx = stable_app("split-right");
    let app = &mut fx.app;
    load(app, "notes.log", PLAIN);
    moves(app, Motion::Down, 2);
    moves(app, Motion::Right, 4);
    editor(app, EditorAction::SplitRight);
    // The new group has its own caret.
    moves(app, Motion::Down, 3);
    let term = draw_editor(app, 80, 14);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_split_right", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_tab_drag_hint() {
    let mut fx = stable_app("tab-drag-hint");
    let app = &mut fx.app;
    load(app, "notes.log", PLAIN);
    load(app, "other.log", "first line of other\nsecond\n");
    editor(app, EditorAction::SplitRight);
    // Drag notes.log from the left bar over the right group's text.
    let (left, right) = (app.editor.group_ids()[0], app.editor.group_ids()[1]);
    app.editor.activate_slot(left, 0);
    draw_editor(app, 80, 14);
    let id = app.editor.active_buffer().unwrap().id;
    let body = app.editor.group(right).unwrap().text_area;
    app.editor.drag_tab(left, id, body.x + 4, body.y + 3);
    let term = draw_editor(app, 80, 14);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_tab_drag_hint", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_split_nested() {
    let mut fx = stable_app("split-nested");
    let app = &mut fx.app;
    load(app, "notes.log", PLAIN);
    editor(app, EditorAction::SplitRight);
    editor(app, EditorAction::SplitDown);
    load(app, "other.log", "first line of other\nsecond\n");
    let term = draw_editor(app, 80, 14);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_split_nested", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column() {
    let mut fx = stable_app("carets-column");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    moves(app, Motion::Right, 4);
    for _ in 0..3 {
        editor(app, EditorAction::CloneCaretBelow);
    }
    // Each caret selects two characters to its right; the short line's
    // caret runs on over the line break into the next line.
    editor(app, EditorAction::Move(Motion::Right, true));
    editor(app, EditorAction::Move(Motion::Right, true));
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column_virtual() {
    let mut fx = stable_app("carets-column-virtual");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    // A zero-width block at column 5 passing the short line: its caret is
    // drawn at the block column, past the line end.
    let b = app.editor.active_buffer_mut().unwrap();
    b.set_block(crate::editor::view_state::Block { anchor: (0, 5), head: (3, 5) }, 4);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column_virtual_backspace() {
    let mut fx = stable_app("carets-column-virtual-bs");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    let b = app.editor.active_buffer_mut().unwrap();
    b.set_block(crate::editor::view_state::Block { anchor: (0, 5), head: (3, 5) }, 4);
    editor(app, EditorAction::Backspace);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual_backspace", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column_virtual_delete() {
    let mut fx = stable_app("carets-column-virtual-del");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    let b = app.editor.active_buffer_mut().unwrap();
    b.set_block(crate::editor::view_state::Block { anchor: (0, 5), head: (3, 5) }, 4);
    editor(app, EditorAction::Backspace);
    editor(app, EditorAction::Delete);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual_delete", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column_virtual_tab() {
    let mut fx = stable_app("carets-column-virtual-tab");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    let b = app.editor.active_buffer_mut().unwrap();
    b.indent = crate::editor::indent::IndentStyle::spaces(4);
    b.set_block(crate::editor::view_state::Block { anchor: (0, 5), head: (3, 5) }, 4);
    editor(app, EditorAction::InsertTab);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual_tab", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column_virtual_enter() {
    let mut fx = stable_app("carets-column-virtual-enter");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    let b = app.editor.active_buffer_mut().unwrap();
    b.set_block(crate::editor::view_state::Block { anchor: (0, 5), head: (3, 5) }, 4);
    editor(app, EditorAction::InsertNewline);
    let term = draw_editor(app, 40, 14);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual_enter", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column_virtual_rect() {
    let mut fx = stable_app("carets-column-virtual-rect");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    let b = app.editor.active_buffer_mut().unwrap();
    b.set_block(crate::editor::view_state::Block { anchor: (0, 6), head: (3, 12) }, 4);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual_rect", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_column_past_widest() {
    let mut fx = stable_app("carets-column-past-widest");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nabcdefgh\nab\nabcdefgh\n");
    editor(app, EditorAction::ToggleColumnMode);
    moves(app, Motion::Right, 5);
    for _ in 0..3 {
        editor(app, EditorAction::Move(Motion::Down, true));
    }
    for _ in 0..10 {
        editor(app, EditorAction::Move(Motion::Right, true));
    }
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_past_widest", snapshot(&term, &marks, None));
    for _ in 0..20 {
        editor(app, EditorAction::Move(Motion::Right, true));
    }
    let term = draw_editor(app, 40, 8);
    assert!(app.editor.active_buffer().unwrap().view.scroll_col > 0);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_past_widest_scrolled", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_occurrences() {
    let mut fx = stable_app("occurrences");
    let app = &mut fx.app;
    load(app, "occ.log", "let foo = 1;\nfoo + bar;\nprint(foo);\nfoofoo foo\n");
    moves(app, Motion::Right, 5);
    // The word at the caret, then its next occurrence; the others are
    // highlighted.
    editor(app, EditorAction::AddNextOccurrence);
    editor(app, EditorAction::AddNextOccurrence);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_occurrences", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_unusual_line_breaks() {
    let mut fx = stable_app("unusual-breaks");
    let app = &mut fx.app;
    let text = "form\u{c}feed\nvtab\u{b}x\nnel\u{85}y\nls\u{2028}ps\u{2029}end\nlast\n";
    load(app, "seps.log", text);
    // VT, FF, NEL, U+2028 and U+2029 break no line: five lines and the
    // empty one after the last break, each separator a dim escape inside
    // its line. The caret is right after U+2028.
    let ps = text.chars().collect::<String>().find("ps").unwrap();
    let ps = text[..ps].chars().count();
    app.editor.active_buffer_mut().unwrap().view.set_sel(Selection::point(ps));
    let term = draw_editor(app, 40, 8);
    let mut marks = editor_marks(&app.theme);
    marks.push(Mark::modifier('D', "dim", Modifier::DIM));
    assert_snapshot!("editor_unusual_line_breaks", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_carets_split() {
    let mut fx = stable_app("carets-split");
    let app = &mut fx.app;
    load(app, "notes.log", PLAIN);
    moves(app, Motion::Right, 2);
    editor(app, EditorAction::CloneCaretBelow);
    editor(app, EditorAction::CloneCaretBelow);
    editor(app, EditorAction::SplitRight);
    // The new view drops to one caret; the left one, unfocused, keeps its
    // three, all drawn as cells.
    editor(app, EditorAction::Escape);
    moves(app, Motion::Down, 3);
    let term = draw_editor(app, 80, 10);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_split", snapshot(&term, &marks, None));
}

#[cfg(feature = "lang-rust")]
const RUST: &str = "\
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    fn sum(&self) -> i32 {
        self.x + self.y
    }
}

fn main() {
    let p = Point::new(1, 2);
    println!(\"{}\", p.sum());
}
";

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn editor_fold_rust() {
    let mut fx = stable_app("fold-rust");
    let app = &mut fx.app;
    load(app, "point.rs", RUST);
    // Into `fn new`'s body.
    moves(app, Motion::Down, 7);
    editor(app, EditorAction::FoldCollapse);
    let term = draw_editor(app, 60, 20);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_fold_rust", snapshot(&term, &marks, None));
}

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn editor_fold_line_ops() {
    let mut fx = stable_app("fold-line-ops");
    let app = &mut fx.app;
    load(app, "point.rs", RUST);
    editor(app, EditorAction::FoldCollapseAll);
    editor(app, EditorAction::MoveLineDown);
    let term = draw_editor(app, 80, 14);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_fold_line_ops", snapshot(&term, &marks, None));
}

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn editor_fold_all_rust() {
    let mut fx = stable_app("fold-all-rust");
    let app = &mut fx.app;
    load(app, "point.rs", RUST);
    editor(app, EditorAction::FoldCollapseAll);
    let term = draw_editor(app, 60, 12);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_fold_all_rust", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_fold_indent_txt() {
    let mut fx = stable_app("fold-indent");
    let app = &mut fx.app;
    load(app, "list.txt", "Groceries:\n  milk\n  eggs\n  bread\nChores:\n  dishes\n  laundry\nDone\n");
    // Select from `eggs` into `Chores` and fold those lines.
    moves(app, Motion::Down, 2);
    editor(app, EditorAction::Move(Motion::Down, true));
    editor(app, EditorAction::Move(Motion::Down, true));
    editor(app, EditorAction::FoldSelection);
    let term = draw_editor(app, 50, 10);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_fold_indent_txt", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_bookmarks() {
    let mut fx = stable_app("bookmarks");
    let app = &mut fx.app;
    load(app, "notes.log", PLAIN);
    draw_editor(app, 40, 8);
    // A plain bookmark on line 1, bookmark 3 on line 3, bookmark 7 on
    // line 6; the caret ends on line 4.
    editor(app, EditorAction::ToggleBookmark);
    for (down, digit) in [(2, '3'), (3, '7')] {
        moves(app, Motion::Down, down);
        editor(app, EditorAction::ToggleNumberedBookmark);
        app.handle_key(KeyEvent::new(KeyCode::Char(digit), KeyModifiers::NONE));
    }
    moves(app, Motion::Up, 2);
    let term = draw_editor(app, 40, 8);
    // A digit is drawn reversed, so it does not read as part of the line
    // number (the caret mark is reversed too: B goes first).
    let mut marks = vec![Mark::style('B', "bookmark", app.theme.bookmark)];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_bookmarks", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn bookmark_digit_dialog() {
    let mut fx = stable_app("bookmark-digit-dialog");
    let app = &mut fx.app;
    load(app, "notes.log", PLAIN);
    moves(app, Motion::Down, 2);
    // Digit 1 is taken, so 2 is preselected; all ten buttons fit inside.
    editor(app, EditorAction::ToggleNumberedBookmark);
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    moves(app, Motion::Down, 1);
    editor(app, EditorAction::ToggleNumberedBookmark);
    let term = draw_app(app, 80, 24);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("bookmark_digit_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn bookmarks_dialog_with_closed_files() {
    let mut fx = stable_app("bookmarks-dialog-closed");
    let app = &mut fx.app;
    // notes.log: a plain bookmark on line 1 and bookmark 3 on line 4,
    // then closed; other.log stays open with a plain bookmark on line 2.
    let notes = load(app, "notes.log", PLAIN);
    editor(app, EditorAction::ToggleBookmark);
    moves(app, Motion::Down, 3);
    editor(app, EditorAction::ToggleNumberedBookmark);
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    load(app, "other.log", PLAIN);
    moves(app, Motion::Down, 1);
    editor(app, EditorAction::ToggleBookmark);
    app.editor.force_close(app.editor.index_of(&notes).unwrap());
    editor(app, EditorAction::ShowBookmarks);
    let term = draw_app(app, 80, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    assert_snapshot!("bookmarks_dialog_with_closed_files", snapshot(&term, &marks, None));
}

const WRAPPED: &str = "short\nthe quick brown fox jumps over the lazy dog héllo 世界 again and again\n\ttabbed line that wraps after the tab and more words\nend\n";

#[tokio::test]
async fn editor_soft_wrap() {
    let mut fx = stable_app("soft-wrap");
    let app = &mut fx.app;
    load(app, "wrap.log", WRAPPED);
    editor(app, EditorAction::ToggleSoftWrap);
    // The caret on `世`, on a continuation row of line 2.
    let at = WRAPPED.find('世').map(|b| WRAPPED[..b].chars().count()).unwrap();
    app.editor.active_buffer_mut().unwrap().view.set_sel(Selection::point(at));
    let term = draw_editor(app, 40, 12);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_soft_wrap", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_soft_wrap_row_end_caret() {
    let mut fx = stable_app("wrap-row-end");
    let app = &mut fx.app;
    load(app, "wrap.log", WRAPPED);
    editor(app, EditorAction::ToggleSoftWrap);
    draw_editor(app, 40, 12);
    // Line 2's first row, End: the caret after the row's last char, not on
    // the next row's first. A caret cloned below it sits at the end of the
    // second row; the first is then a secondary one drawn upstream.
    app.editor.active_buffer_mut().unwrap().view.set_sel(Selection::point(6));
    editor(app, EditorAction::Move(Motion::LineEnd, false));
    let term = draw_editor(app, 40, 12);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_soft_wrap_row_end_caret", snapshot(&term, &marks, None));
    editor(app, EditorAction::CloneCaretBelow);
    let term = draw_editor(app, 40, 12);
    assert_snapshot!("editor_soft_wrap_row_end_carets", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_wrap_and_fold() {
    let mut fx = stable_app("wrap-fold");
    let app = &mut fx.app;
    let text = "Notes:\n  a long indented line that has to wrap at least once here\n  short\n  another long one that wraps across the edge of the pane\nAfter the fold: one more long line that wraps too\n";
    load(app, "notes.txt", text);
    // `.txt` wraps by default. Rows exist once a frame has given the view
    // its width, as in the run loop, where a frame comes before any key.
    draw_editor(app, 40, 10);
    // From the end of line 2's first row down two rows (into line 3), then
    // fold lines 2-3.
    moves(app, Motion::Down, 1);
    editor(app, EditorAction::Move(Motion::LineEnd, false));
    for _ in 0..2 {
        editor(app, EditorAction::Move(Motion::Down, true));
    }
    editor(app, EditorAction::FoldSelection);
    let term = draw_editor(app, 40, 10);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_wrap_and_fold", snapshot(&term, &marks, None));
}

/// List the root as the tree's background refresh would.
fn load_tree(app: &mut App) {
    let req = app.project.begin_refresh().expect("no refresh in flight");
    let generation = app.workspace.generation;
    app.handle_msg(AppMsg::TreeLoaded { generation, seq: req.seq, dirs: req.run() });
}

/// Git colors and the selected row of the project tree.
fn tree_marks(theme: &Theme) -> Vec<Mark> {
    vec![
        Mark::style('m', "modified", theme.git_modified),
        Mark::style('u', "untracked", theme.git_untracked),
        Mark::bg('L', "selected", theme.list_selected.bg.unwrap()),
    ]
}

#[tokio::test]
async fn app_project_tree() {
    let mut fx = stable_app("project-tree");
    let app = &mut fx.app;
    let root = app.workspace.root.clone();
    std::fs::create_dir_all(root.join("src")).unwrap();
    for (name, text) in [("src/main.rs", "fn main() {}\n"), ("src/lib.rs", ""), ("Cargo.toml", ""), ("README.md", ""), ("notes.txt", "")] {
        std::fs::write(root.join(name), text).unwrap();
    }
    load_tree(app);
    assert!(app.project.select_path(&root.join("src/main.rs")));
    // A status as the service would send it: `src/main.rs` modified (its
    // folder too), `notes.txt` untracked.
    let entry = |rel: &str, section, status| ChangeEntry { section, status, rel: rel.into(), abs: root.join(rel), old_rel: None };
    let status = StatusSnapshot {
        workdir: root.clone(),
        entries: vec![
            entry("src/main.rs", Section::Unstaged, FileStatus::Modified),
            entry("notes.txt", Section::Untracked, FileStatus::Untracked),
        ],
        tree: [
            (root.join("src"), FileStatus::Modified),
            (root.join("src/main.rs"), FileStatus::Modified),
            (root.join("notes.txt"), FileStatus::Untracked),
        ]
        .into_iter()
        .collect(),
        branch: Some("main".into()),
        head: None,
    };
    app.handle_msg(AppMsg::Git(GitMsg::Status { generation: app.git.generation, seq: 1, result: Ok(status) }));
    app.focus.set(PanelId::Project);
    // Known issue, kept as it draws today: the empty editor's hint is cut
    // at the pane's edge ("…the Project view;" loses "Ctrl+Shift+N finds a
    // file.") instead of wrapping (yman 153).
    let term = draw_app(app, 80, 24);
    let marks = tree_marks(&app.theme);
    let left = app.layout.rects.left;
    assert_snapshot!("app_project_tree", snapshot(&term, &marks, Some(left)));
}

/// Changed lines of the Git window's diff.
fn diff_marks(theme: &Theme) -> Vec<Mark> {
    vec![
        Mark::bg('A', "added", theme.diff_add_bg),
        Mark::bg('D', "deleted", theme.diff_del_bg),
        Mark::bg('~', "modified", theme.diff_mod_bg),
        Mark::bg('+', "added word", theme.diff_add_word_bg),
        Mark::bg('-', "deleted word", theme.diff_del_word_bg),
        Mark::bg('L', "selected", theme.list_selected.bg.unwrap()),
    ]
}

/// A repository with a modified and an untracked file, the Git window on
/// its Commit tab and the first change's diff loaded.
fn git_commit_fixture(app: &mut App) {
    super::tests::init_repo(app, &[("main.rs", "fn main() {\n    println!(\"hello\");\n}\n"), ("README.md", "# demo\n")]);
    let root = app.workspace.root.clone();
    std::fs::write(root.join("main.rs"), "fn main() {\n    println!(\"hello, world\");\n    run();\n}\n").unwrap();
    std::fs::write(root.join("notes.txt"), "todo\n").unwrap();
    load_tree(app);
    // The real status, on a branch named the same whatever
    // `init.defaultBranch` says.
    let mut status = crate::git::repo::status(&root).unwrap();
    status.branch = Some("main".into());
    let first = status.entries[0].clone();
    assert_eq!(first.rel, "main.rs");
    app.toggle_tool_window(ToolWindowId::Git);
    app.handle_msg(AppMsg::Git(GitMsg::Status { generation: app.git.generation, seq: 1, result: Ok(status) }));
    // The diff of the selected change, as the service would send it.
    let key = DiffKey::Change { section: first.section, rel: first.rel.clone(), old: None };
    let result = crate::git::repo::change_texts(&root, first.section, &first.rel, None)
        .map(|(old, new)| crate::git::diff::DiffDoc::build(&old, &new, app.git.tab_width));
    app.handle_msg(AppMsg::Git(GitMsg::Diff { generation: app.git.generation, key, result }));
    assert_eq!(app.busy_label(), None);
}

#[tokio::test]
async fn app_git_commit_tab() {
    let mut fx = stable_app("git-commit");
    let app = &mut fx.app;
    git_commit_fixture(app);
    let term = draw_app(app, 120, 30);
    let marks = diff_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("app_git_commit_tab", snapshot(&term, &marks, Some(bottom)));
}

/// At 80 columns the diff header is cut before the layout toggles instead
/// of running under them.
#[tokio::test]
async fn app_git_commit_tab_80_columns() {
    let mut fx = stable_app("git-commit-80");
    let app = &mut fx.app;
    git_commit_fixture(app);
    let term = draw_app(app, 80, 24);
    let marks = diff_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("app_git_commit_tab_80_columns", snapshot(&term, &marks, Some(bottom)));
}

/// The palette's selected row.
fn palette_marks(theme: &Theme) -> Vec<Mark> {
    vec![Mark::bg('L', "selected", theme.list_selected.bg.unwrap())]
}

#[tokio::test]
async fn palette_actions() {
    let mut fx = stable_app("palette-actions");
    let app = &mut fx.app;
    app.open_palette(PaletteKind::Actions);
    app.palette.as_mut().unwrap().set_query("split");
    let term = draw_app(app, 80, 24);
    let marks = palette_marks(&app.theme);
    assert_snapshot!("palette_actions", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn palette_files() {
    let mut fx = stable_app("palette-files");
    let app = &mut fx.app;
    app.open_palette(PaletteKind::Files);
    // The walk the palette asked for, finished.
    let files = ["src/main.rs", "src/map.rs", "docs/manual.md", "Makefile", "README.md", "tests/smoke.rs"];
    let (generation, seq) = (app.workspace.generation, app.file_index.requested);
    app.handle_msg(AppMsg::FileIndex { generation, seq, files: files.map(String::from).to_vec() });
    app.palette.as_mut().unwrap().set_query("ma");
    let term = draw_app(app, 80, 24);
    let marks = palette_marks(&app.theme);
    assert_snapshot!("palette_files", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn palette_scratches() {
    let mut fx = stable_app("palette-scratches");
    let app = &mut fx.app;
    // Noon UTC, so the local date is the same in nearly every time zone;
    // no grammar-dependent extensions, so the languages are the same with
    // any set of language features.
    let dir = app.paths.scratches_dir();
    std::fs::create_dir_all(&dir).unwrap();
    for (name, day) in [("scratch_1.txt", 19_000), ("notes.log", 19_850), ("scratch_2.txt", 20_000)] {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(day * 86_400 + 12 * 3600);
        std::fs::File::create(dir.join(name)).unwrap().set_modified(at).unwrap();
    }
    app.open_palette(PaletteKind::Scratches);
    let term = draw_app(app, 80, 24);
    let marks = palette_marks(&app.theme);
    assert_snapshot!("palette_scratches", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn palette_recent_projects() {
    let mut fx = stable_app("palette-recent-projects");
    let app = &mut fx.app;
    app.open_palette(PaletteKind::RecentProjects);
    // Fixed roots outside the home directory, not the temp state's list.
    let roots: Vec<std::path::PathBuf> = ["/work/alpha", "/work/beta", "/srv/gamma"].iter().map(Into::into).collect();
    app.palette.as_mut().unwrap().set_projects(&roots);
    let term = draw_app(app, 80, 24);
    let marks = palette_marks(&app.theme);
    assert_snapshot!("palette_recent_projects", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn palette_locations_grouped() {
    use crate::lsp::locations::LocationItem;
    use lsp_types::{Position, Range};
    let mut fx = stable_app("palette-locations-grouped");
    let app = &mut fx.app;
    let root = app.workspace.root.clone();
    let r = |l, c0, c1| Range::new(Position::new(l, c0), Position::new(l, c1));
    let item = |file: &str, range, text: &str| LocationItem::at(root.join(file), range, Some(text));
    let items = vec![
        item("src/a.rs", r(2, 16, 22), "    let total = helper(1);"),
        item("src/a.rs", r(9, 4, 10), "    helper(total);"),
        item("src/b.rs", r(4, 19, 25), "    println!(\"{}\", helper(2));"),
        item("src/b.rs", r(7, 8, 14), "        helper(3)"),
    ];
    app.show_locations("Usages of helper".into(), items);
    let term = draw_app(app, 100, 30);
    let mut marks = palette_marks(&app.theme);
    marks.push(Mark::bg('H', "usage", app.theme.search_match_bg));
    assert_snapshot!("palette_locations_grouped", snapshot(&term, &marks, None));
}

/// The Local History window on `notes.txt` (a plain `.txt`: no grammar,
/// so the frame is the same with any set of language features): seven
/// snapshots at noon UTC on seven days, shown at UTC, the `row`th row
/// selected, `filter` typed, and its diff loaded as the app would.
fn local_history_fixture(app: &mut App, row: usize, filter: &str) {
    use crate::history::{HistoryMsg, Kind, Request};
    let path = app.workspace.root.join("notes.txt");
    app.show_local_history(path.clone());
    let entry = |day: u64, len: u64, lines: u64, kind, detail: Option<&str>| crate::history::Entry {
        v: 1,
        path: path.to_string_lossy().into_owned(),
        at: (day * 86_400 + 12 * 3600) * 1000,
        blob: format!("{day:040}"),
        len,
        lines,
        kind,
        detail: detail.map(String::from),
        since: None,
    };
    let old = app.workspace.root.join("old/notes.txt").to_string_lossy().into_owned();
    let entries = vec![
        entry(20_001, 1340, 41, Kind::Unsaved, None),
        entry(20_000, 1300, 40, Kind::Save, None),
        entry(19_999, 1200, 37, Kind::BeforeGit, Some("git switch main")),
        entry(19_998, 900, 39, Kind::BeforeReload, None),
        // A label: bold.
        entry(19_997, 900, 39, Kind::Label, Some("before refactor")),
        // Shown relative to the root: "Renamed from old/notes.txt".
        entry(19_995, 900, 39, Kind::Renamed, Some(&old)),
        entry(19_990, 512, 39, Kind::Autosave, None),
    ];
    // Two snapshots of one minute: both rows show the seconds.
    let mut entries = entries;
    let mut same_minute = entries[6].clone();
    same_minute.at += 7_000;
    same_minute.blob = format!("{:040}", 19_989);
    entries.insert(6, same_minute);
    // The requests go nowhere: the diff is answered below.
    let root = app.workspace.root.clone();
    let mut cx = ctx!(app);
    app.history_panel.set_entries(entries, &root, |_| 0, &mut cx);
    for c in filter.chars() {
        app.history_panel.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &mut cx);
    }
    for _ in 0..row {
        app.history_panel.handle_action(&crate::actions::HistoryAction::Down, &mut cx);
    }
    let Some((key, seq)) = cx.effects.into_iter().rev().find_map(|e| match e {
        crate::event::Effect::History(Request::Diff { key, seq }) => Some((key, seq)),
        _ => None,
    }) else {
        panic!("a diff request")
    };
    let old = "alpha\nbeta\ngamma\ndelta\n";
    let new = "alpha\nbeta two\ngamma\nepsilon\ndelta\n";
    let result = Ok(crate::git::diff::DiffDoc::build(old.as_bytes(), new.as_bytes(), app.git.tab_width));
    app.handle_msg(AppMsg::LocalHistory(HistoryMsg::Diff { key, seq, result }));
}

/// The diff's changes, the selected row and the label's bold.
fn history_marks(theme: &Theme) -> Vec<Mark> {
    let mut marks = diff_marks(theme);
    marks.push(Mark::modifier('B', "bold", Modifier::BOLD));
    marks
}

#[tokio::test]
async fn local_history_window() {
    let mut fx = stable_app("local-history-window");
    let app = &mut fx.app;
    local_history_fixture(app, 1, "");
    let term = draw_app(app, 80, 24);
    let marks = history_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("local_history_window", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn local_history_window_wide() {
    let mut fx = stable_app("local-history-window-wide");
    let app = &mut fx.app;
    local_history_fixture(app, 1, "save");
    // Unified at this width; Alt+U flips the layout the frame drew.
    draw_app(app, 140, 40);
    app.history_panel.handle_action(&crate::actions::HistoryAction::ToggleDiffLayout, &mut ctx!(app));
    let term = draw_app(app, 140, 40);
    let marks = history_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("local_history_window_wide", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn local_history_window_changes() {
    let mut fx = stable_app("local-history-window-changes");
    let app = &mut fx.app;
    local_history_fixture(app, 1, "");
    // The diff focused, both changes selected: their line numbers stand
    // out and the toolbar counts them.
    let mut cx = ctx!(app);
    app.history_panel.handle_action(&crate::actions::HistoryAction::Right, &mut cx);
    app.history_panel.handle_action(&crate::actions::HistoryAction::SelectAllChanges, &mut cx);
    let term = draw_app(app, 80, 24);
    let mut marks = history_marks(&app.theme);
    marks.push(Mark::style('W', "selected change", app.theme.warn));
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("local_history_window_changes", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn local_history_line_selection() {
    let mut fx = stable_app("local-history-line-selection");
    let app = &mut fx.app;
    local_history_fixture(app, 1, "");
    // The diff focused, a line cursor on the modified line grown to the
    // inserted one: only those line numbers are reversed.
    // (Drawn once first: the rows follow the layout the frame chose.)
    draw_app(app, 80, 24);
    let mut cx = ctx!(app);
    app.history_panel.handle_action(&crate::actions::HistoryAction::Right, &mut cx);
    app.history_panel.handle_action(&crate::actions::HistoryAction::LineDown, &mut cx);
    app.history_panel.handle_action(&crate::actions::HistoryAction::LineExtendDown, &mut cx);
    let term = draw_app(app, 80, 24);
    let mut marks = history_marks(&app.theme);
    marks.push(Mark::style('W', "selected line", app.theme.warn.add_modifier(Modifier::REVERSED)));
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("local_history_line_selection", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn local_history_window_narrow() {
    let mut fx = stable_app("local-history-window-narrow");
    let app = &mut fx.app;
    local_history_fixture(app, 4, "");
    // Narrow: the diff takes the whole width once it has focus.
    app.history_panel.handle_action(&crate::actions::HistoryAction::Right, &mut ctx!(app));
    let term = draw_app(app, 60, 24);
    let marks = history_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("local_history_window_narrow", snapshot(&term, &marks, Some(bottom)));
}

/// The Local History window's project view on the root: a file with
/// unsaved changes (selected, its diff loaded), a deleted one, a label put
/// on both (opened: its files below it), a file renamed away and where it
/// went (with a label of its own), and a deep path. Noon UTC on six days,
/// shown at UTC; `filter` typed.
fn local_history_project_fixture(app: &mut App, filter: &str) {
    use crate::history::{FileState, HistoryMsg, Kind, RecentFile, Request};
    let root = app.workspace.root.clone();
    let entry = |rel: &str, day: u64, kind, detail: Option<&str>| crate::history::Entry {
        v: 1,
        path: root.join(rel).to_string_lossy().into_owned(),
        at: (day * 86_400 + 12 * 3600) * 1000,
        blob: format!("{day:039}{}", rel.len() % 10),
        len: 100,
        lines: 4,
        kind,
        detail: detail.map(String::from),
        since: None,
    };
    let file =
        |rel: &str, newest, labels: Vec<crate::history::Entry>, state| RecentFile { path: root.join(rel), newest, count: 3, labels, state };
    let release = |rel: &str| entry(rel, 19_999, Kind::Label, Some("release 1.2"));
    let old = root.join("old/todo.txt").to_string_lossy().into_owned();
    let files = vec![
        file("notes.txt", entry("notes.txt", 20_001, Kind::Unsaved, None), vec![release("notes.txt")], FileState::Present),
        file("docs/guide.md", entry("docs/guide.md", 20_000, Kind::BeforeDelete, None), vec![release("docs/guide.md")], FileState::Deleted),
        file(
            "todo.txt",
            entry("todo.txt", 19_998, Kind::Renamed, Some(&old)),
            vec![entry("todo.txt", 19_997, Kind::Label, Some("before refactor"))],
            FileState::Present,
        ),
        file("old/todo.txt", entry("old/todo.txt", 19_996, Kind::Save, None), vec![], FileState::Renamed(root.join("todo.txt"))),
        file(
            "src/very/deep/path/to/a/module_name.txt",
            entry("src/very/deep/path/to/a/module_name.txt", 19_990, Kind::Autosave, None),
            vec![],
            FileState::Present,
        ),
    ];
    app.show_local_history_dir(root.clone());
    let mut cx = ctx!(app);
    app.history_panel.set_files(&root, files, &root, |_| 0, &mut cx);
    // Open the label, then back to the top row.
    for a in [HistoryAction::Down, HistoryAction::Down, HistoryAction::OpenFile, HistoryAction::Home] {
        app.history_panel.handle_action(&a, &mut cx);
    }
    for c in filter.chars() {
        app.history_panel.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &mut cx);
    }
    let Some((key, seq)) = cx.effects.into_iter().rev().find_map(|e| match e {
        crate::event::Effect::History(Request::Diff { key, seq }) => Some((key, seq)),
        _ => None,
    }) else {
        panic!("a diff request")
    };
    let result = Ok(crate::git::diff::DiffDoc::build(b"alpha\nbeta\n", b"alpha\nbeta two\n", app.git.tab_width));
    app.handle_msg(AppMsg::LocalHistory(HistoryMsg::Diff { key, seq, result }));
}

/// The project view's marks: crossed-out (gone) names and `deleted` in warn.
fn project_marks(theme: &Theme) -> Vec<Mark> {
    let mut marks = history_marks(theme);
    marks.insert(0, Mark::modifier('X', "crossed out", Modifier::CROSSED_OUT));
    marks.push(Mark::style('W', "warn", theme.warn));
    marks
}

#[tokio::test]
async fn local_history_project() {
    let mut fx = stable_app("local-history-project");
    let app = &mut fx.app;
    local_history_project_fixture(app, "");
    let term = draw_app(app, 80, 24);
    let marks = project_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("local_history_project", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn local_history_revert_label_dialog() {
    let mut fx = stable_app("local-history-revert-label");
    let app = &mut fx.app;
    local_history_project_fixture(app, "");
    app.perform(Action::History(HistoryAction::Down));
    app.perform(Action::History(HistoryAction::Down));
    app.perform(Action::History(HistoryAction::Revert));
    assert!(app.dialog.is_some(), "the confirm");
    let term = draw_app(app, 80, 24);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("local_history_revert_label_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn local_history_project_wide() {
    let mut fx = stable_app("local-history-project-wide");
    let app = &mut fx.app;
    local_history_project_fixture(app, "txt");
    let term = draw_app(app, 120, 40);
    let marks = project_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("local_history_project_wide", snapshot(&term, &marks, Some(bottom)));
}

/// The resolver's fixture (as in `editor/merge.rs`): yours changes `one`,
/// theirs changes `three`, and both change `five`. A plain extension keeps
/// the frame the same with any set of language features.
const BASE: &str = "fn main() {\n    one();\n    two();\n    three();\n    four();\n    five();\n}\n";
const OURS: &str = "fn main() {\n    ONE();\n    two();\n    three();\n    four();\n    five_ours();\n}\n";
const THEIRS: &str = "fn main() {\n    one();\n    two();\n    THREE();\n    four();\n    five_theirs();\n}\n";

fn open_merge(app: &mut App) {
    open_merge_texts(app, [BASE, OURS, THEIRS]);
}

/// One conflict whose sides differ in length: yours 1 line, theirs 3.
const FILL_BASE: &str = "fn main() {\n    one();\n    two();\n}\n";
const FILL_OURS: &str = "fn main() {\n    ONE();\n    two();\n}\n";
const FILL_THEIRS: &str = "fn main() {\n    uno();\n    dos();\n    tres();\n    two();\n}\n";

fn open_merge_texts(app: &mut App, [base, ours, theirs]: [&str; 3]) {
    let path = app.workspace.root.join("main.log");
    let texts = crate::git::merge::MergeTexts { base: base.into(), ours: ours.into(), theirs: theirs.into() };
    let theme = app.theme.clone();
    app.editor.open_merge(path, "main.log".into(), texts, ["main".into(), "dev".into()], base.into(), &theme);
}

/// The resolver's chunk backgrounds, and the carets.
fn merge_marks(theme: &Theme) -> Vec<Mark> {
    vec![
        Mark::modifier('C', "caret", Modifier::REVERSED),
        Mark::bg('M', "conflict", theme.merge_conflict_bg),
        Mark::bg('A', "added", theme.diff_add_bg),
        Mark::bg('D', "deleted", theme.diff_del_bg),
        Mark::bg('~', "modified", theme.diff_mod_bg),
        Mark::bg('R', "resolved", theme.merge_resolved_bg),
    ]
}

#[tokio::test]
async fn merge_resolver() {
    let mut fx = stable_app("merge");
    let app = &mut fx.app;
    open_merge(app);
    let term = draw_editor(app, 120, 12);
    let marks = merge_marks(&app.theme);
    assert_snapshot!("merge_resolver", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn merge_after_accept() {
    let mut fx = stable_app("merge-accept");
    let app = &mut fx.app;
    open_merge(app);
    draw_editor(app, 120, 12);
    // Yours for the conflict at the caret: nothing is left to resolve.
    editor(app, EditorAction::Merge(MergeAction::AcceptLeft));
    let term = draw_editor(app, 120, 12);
    let marks = merge_marks(&app.theme);
    assert_snapshot!("merge_after_accept", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn merge_filler_rows() {
    let mut fx = stable_app("merge-fill");
    let app = &mut fx.app;
    open_merge_texts(app, [FILL_BASE, FILL_OURS, FILL_THEIRS]);
    let term = draw_editor(app, 120, 12);
    let marks = merge_marks(&app.theme);
    assert_snapshot!("merge_filler_rows", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn merge_filler_rows_at_top() {
    const TOP_BASE: &str = "fn main() {\n    one();\n}\n";
    const TOP_OURS: &str = TOP_BASE;
    const TOP_THEIRS: &str = "// added\n// lines\nfn main() {\n    one();\n}\n";
    let mut fx = stable_app("merge-fill-top");
    let app = &mut fx.app;
    open_merge_texts(app, [TOP_BASE, TOP_OURS, TOP_THEIRS]);
    let term = draw_editor(app, 120, 12);
    let marks = merge_marks(&app.theme);
    assert_snapshot!("merge_filler_rows_at_top", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn merge_filler_rows_both_accepted() {
    let mut fx = stable_app("merge-fill-both");
    let app = &mut fx.app;
    open_merge_texts(app, [FILL_BASE, FILL_OURS, FILL_THEIRS]);
    draw_editor(app, 120, 12);
    editor(app, EditorAction::Merge(MergeAction::AcceptLeft));
    editor(app, EditorAction::Merge(MergeAction::AcceptRight));
    let term = draw_editor(app, 120, 12);
    let marks = merge_marks(&app.theme);
    assert_snapshot!("merge_filler_rows_both_accepted", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn recovery_dialog() {
    let mut fx = stable_app("recovery-dialog");
    let app = &mut fx.app;
    super::recovery::tests::seed_dump(
        app,
        1_700_000_000,
        &[(Some("src/main.rs"), "main.rs", "fn main() {}\n"), (None, "scratch", "note\n")],
    );
    let (generation, root) = (app.workspace.generation, app.workspace.root.clone());
    app.handle_msg(AppMsg::SessionLoaded { generation, session: crate::workspace::Session { root, ..Default::default() }, files: vec![] });
    let term = draw_app(app, 80, 24);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("recovery_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn recovery_review_dialog() {
    let mut fx = stable_app("recovery-review");
    let app = &mut fx.app;
    super::recovery::tests::seed_dump(
        app,
        1_700_000_000,
        &[(Some("src/main.rs"), "main.rs", "fn main() {}\n"), (None, "scratch", "note\n")],
    );
    app.offer_recovery();
    let d = app.dialog.take().expect("the recovery dialog");
    // Review…: the first entry, the title wider than the message.
    app.finish_dialog(d.id, crate::ui::dialog::DialogResult::Button(2));
    let term = draw_app(app, 80, 24);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("recovery_review_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn save_as_dialog() {
    let mut fx = stable_app("save-as-dialog");
    let app = &mut fx.app;
    // A buffer restored after a crash that has no file: quitting saves the
    // files, then asks where this one goes.
    let i = app.editor.open_untitled("notes (recovered)");
    app.editor.buffers[i].insert("rescued text\n", crate::editor::undo::TxKind::Typing);
    app.focus.set(PanelId::Editor);
    app.request_quit();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let term = draw_app(app, 80, 24);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("save_as_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn short_screen_input_dialog() {
    let mut fx = stable_app("short-input-dialog");
    let app = &mut fx.app;
    let msg: Vec<String> = (1..=12).map(|n| format!("line {n}")).collect();
    app.dialog = Some(crate::ui::dialog::Dialog::input(
        crate::ui::dialog::DialogId::PutLabel(std::path::PathBuf::from("/x")),
        "Put Label",
        &msg.join("\n"),
        "v1",
        "OK",
    ));
    let term = draw_app(app, 60, 10);
    assert_snapshot!("short_screen_input_dialog", snapshot(&term, &[], None));
}

#[tokio::test]
async fn save_as_path_completion() {
    let mut fx = stable_app("save-as-complete");
    let app = &mut fx.app;
    let root = app.workspace.root.clone();
    for n in 0..40 {
        std::fs::write(root.join(format!("f{n:02}.txt")), "").unwrap();
    }
    // Sorts among the visible rows as "f0dir/".
    std::fs::create_dir(root.join("f0dir")).unwrap();
    // Never listed for "f".
    std::fs::write(root.join(".fhidden"), "").unwrap();
    let i = app.editor.open_untitled("draft");
    app.editor.buffers[i].insert("text\n", crate::editor::undo::TxKind::Typing);
    app.focus.set(PanelId::Editor);
    app.perform(Action::Editor(EditorAction::SaveAs));
    // Replace the prefill "draft" with "f", then Tab Tab: the list.
    for _ in 0.."draft".len() {
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let term = draw_app(app, 80, 24);
    let area = app.dialog.as_ref().unwrap().rect();
    let mut dim = app.theme.dim;
    dim.bg = Some(app.theme.popup_bg);
    let marks = [Mark::style('D', "dim", dim)];
    assert_snapshot!("save_as_path_completion", snapshot(&term, &marks, Some(area)));
}

#[tokio::test]
async fn input_dialog_scrolled() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut fx = stable_app("input-dialog-scrolled");
    let app = &mut fx.app;
    let i = app.editor.open_untitled("draft");
    app.editor.buffers[i].insert("text\n", crate::editor::undo::TxKind::Typing);
    app.focus.set(PanelId::Editor);
    app.perform(Action::Editor(EditorAction::SaveAs));
    for _ in 0.."draft".len() {
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    }
    let long = "very/long/folder/name/that/does/not/fit/in/the/dialog/notes-for-the-meeting.txt";
    for c in long.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let term = draw_app(app, 60, 16);
    let area = app.dialog.as_ref().unwrap().rect();
    assert_snapshot!("input_dialog_scrolled", snapshot(&term, &[], Some(area)));
    app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    let term = draw_app(app, 60, 16);
    let buf = term.backend().buffer();
    let row = (0..16).find(|&y| crate::ui::snapshot::row_text(buf, y, 60).contains("very/long")).expect("input row starts with the path");
    let line = crate::ui::snapshot::row_text(buf, row, 60);
    let column = line[..line.find("very/long").unwrap()].chars().count() as u16;
    app.handle_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: column + 2, row, modifiers: KeyModifiers::NONE });
    let Some(crate::ui::dialog::Dialog { kind: crate::ui::dialog::DialogKind::Input { input, .. }, .. }) = app.dialog.as_ref() else {
        panic!("input dialog still open");
    };
    assert_eq!(input.cursor, 2);
}

/// Search hits and the selected row of a results list.
fn find_marks(theme: &Theme) -> Vec<Mark> {
    vec![Mark::bg('L', "selected", theme.list_selected.bg.unwrap()), Mark::bg('H', "match", theme.search_match_bg)]
}

#[tokio::test]
async fn find_in_files_window() {
    let mut fx = stable_app("find-in-files");
    let app = &mut fx.app;
    let root = app.workspace.root.clone();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/parse.log"), "let tokens = lex(input);\nparse(tokens)\n    .map(|t| parse_one(t))\n").unwrap();
    std::fs::write(root.join("notes.txt"), "nothing to see\nparse later\n").unwrap();
    std::fs::write(root.join("other.txt"), "no hits here\n").unwrap();
    // Replace in files, the query typed, the search run here.
    app.perform(Action::Find(crate::actions::FindAction::ReplaceInFiles));
    for c in "parse".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    super::tests::run_find(app);
    app.find.replace.set_text("read");
    let term = draw_app(app, 80, 24);
    let marks = find_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("find_in_files_window", snapshot(&term, &marks, Some(bottom)));
}

/// Diagnostics in two files, the first error with related locations in
/// its own file and in the other; `main.log` active.
fn problems_setup(app: &mut App) {
    use crate::lsp::diagnostics::tests::diag;
    use crate::lsp::diagnostics::{Related, Severity};
    let root = app.workspace.root.clone();
    let main = load(app, "main.log", "fn main() {\n    let x: u8 = \"one\";\n    unused();\n}\n");
    let with = |d: crate::lsp::diagnostics::Diagnostic, source: &str, code: Option<&str>| crate::lsp::diagnostics::Diagnostic {
        source: Some(source.into()),
        code: code.map(String::from),
        ..d
    };
    let at = |l, c| lsp_types::Range::new(lsp_types::Position::new(l, c), lsp_types::Position::new(l, c + 1));
    let mut error = with(diag(1, 16, 1, 21, Severity::Error, "mismatched types\nexpected `u8`, found `&str`"), "rustc", Some("E0308"));
    error.related = vec![
        Related { path: main.clone(), range: at(1, 11), message: "expected due to this".into() },
        Related { path: root.join("src/lib.log"), range: at(0, 0), message: "the type is declared here".into() },
    ];
    let per_file = app.config.lsp.max_diagnostics_per_file;
    app.lsp.diagnostics.publish(
        "fake",
        main,
        vec![error, with(diag(2, 4, 2, 10, Severity::Warning, "function `unused` is never used"), "rustc", None)],
        per_file,
    );
    app.lsp.diagnostics.publish(
        "fake",
        root.join("src/lib.log"),
        vec![with(diag(0, 0, 0, 3, Severity::Hint, "consider a doc comment"), "clippy", None)],
        per_file,
    );
    app.toggle_tool_window(ToolWindowId::Problems);
}

fn problems_marks(theme: &Theme) -> [Mark; 3] {
    [
        Mark::bg('L', "selected", theme.list_selected.bg.unwrap()),
        Mark::underline('E', "error", theme.diag_error.fg.unwrap()),
        Mark::underline('W', "warning", theme.diag_warn.fg.unwrap()),
    ]
}

#[tokio::test]
async fn problems_window() {
    let mut fx = stable_app("problems");
    let app = &mut fx.app;
    problems_setup(app);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("problems_window", snapshot(&term, &problems_marks(&app.theme), None));
}

#[tokio::test]
async fn problems_window_current_file() {
    let mut fx = stable_app("problems-current");
    let app = &mut fx.app;
    problems_setup(app);
    app.perform(Action::Problems(crate::actions::ProblemsAction::ToggleCurrentFile));
    let term = draw_app(app, 80, 24);
    assert_snapshot!("problems_window_current_file", snapshot(&term, &problems_marks(&app.theme), None));
}

#[tokio::test]
async fn editor_diagnostic_tags() {
    use crate::lsp::diagnostics::tests::diag;
    use crate::lsp::diagnostics::{Severity, Tags};
    let mut fx = stable_app("diagnostic-tags");
    let app = &mut fx.app;
    let path = load(app, "tags.log", "use old::api;\nlet unused = 1;\ncall(api);\n");
    let mut unused = diag(1, 4, 1, 10, Severity::Warning, "unused variable");
    unused.tags = Tags { unnecessary: true, deprecated: false };
    let mut deprecated = diag(0, 4, 0, 12, Severity::Hint, "deprecated");
    deprecated.tags = Tags { unnecessary: false, deprecated: true };
    let per_file = app.config.lsp.max_diagnostics_per_file;
    app.lsp.diagnostics.publish("fake", path, vec![unused, deprecated], per_file);
    app.sync_diag_marks();
    let term = draw_editor(app, 40, 6);
    let marks = [
        Mark::modifier('X', "deprecated", Modifier::CROSSED_OUT),
        Mark::style('U', "unnecessary", ratatui::style::Style::default().fg(app.theme.dim.fg.unwrap())),
        Mark::underline('W', "warning", app.theme.diag_warn.fg.unwrap()),
    ];
    let text = app.editor.text_area();
    assert_snapshot!("editor_diagnostic_tags", snapshot(&term, &marks, Some(text)));
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

/// A commit as `git log` sends it, its relative date already formatted.
fn commit(c: char, parents: &[char], author: &str, date: &str, refs: &str, subject: &str) -> crate::git::model::LogCommit {
    crate::git::model::LogCommit {
        sha: sha(c),
        parents: parents.iter().map(|&p| sha(p)).collect(),
        short: sha(c)[..7].to_string(),
        author: author.into(),
        date: date.into(),
        refs: refs.into(),
        subject: subject.into(),
    }
}

#[tokio::test]
async fn git_log_tab() {
    use crate::git::model::{CommitDetails, CommitFile};
    let mut fx = stable_app("git-log");
    let app = &mut fx.app;
    app.toggle_tool_window(ToolWindowId::Git);
    app.perform(Action::Git(crate::actions::GitAction::TabLog));
    // The first page, with dates as `git log` formats them (relative to
    // when it ran), so the frame does not depend on the clock: a merge of
    // a two-commit branch.
    let commits = vec![
        commit('a', &['b', 'd'], "Ann", "2 hours ago", "HEAD -> main", "Merge branch 'parser'"),
        commit('d', &['e'], "Bob", "3 days ago", "parser", "Parse nested lists"),
        commit('b', &['c'], "Ann", "4 days ago", "", "Fix the status bar width"),
        commit('e', &['c'], "Bob", "5 days ago", "", "Add a parser"),
        commit('c', &[], "Ann", "2 weeks ago", "tag: v0.1.0", "Initial commit"),
    ];
    let g = app.git.generation;
    let filter = app.git_panel.log_filter().clone();
    app.handle_msg(AppMsg::Git(GitMsg::Log { generation: g, skip: 0, filter, result: Ok(commits) }));
    let details = CommitDetails {
        sha: sha('a'),
        body: "Merge branch 'parser'\n\nBrings in nested list parsing.".into(),
        files: vec![
            CommitFile { status: 'A', path: "src/parser.rs".into(), old: None },
            CommitFile { status: 'M', path: "src/main.rs".into(), old: None },
            CommitFile { status: 'R', path: "docs/syntax.md".into(), old: Some("docs/grammar.md".into()) },
        ],
    };
    app.handle_msg(AppMsg::Git(GitMsg::CommitDetails { generation: g, sha: sha('a'), result: Ok(details) }));
    let term = draw_app(app, 120, 30);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("git_log_tab", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn git_stash_tab() {
    use crate::git::model::{CommitDetails, CommitFile, StashEntry};
    let mut fx = stable_app("git-stash");
    let app = &mut fx.app;
    app.toggle_tool_window(ToolWindowId::Git);
    app.perform(Action::Git(crate::actions::GitAction::TabStash));
    let g = app.git.generation;
    let stashes = vec![
        StashEntry { index: 0, message: "On main: wip parser".into(), oid: sha('1') },
        StashEntry { index: 1, message: "WIP on main: 1111111 Add a parser".into(), oid: sha('2') },
    ];
    app.handle_msg(AppMsg::Git(GitMsg::Stashes { generation: g, result: Ok(stashes) }));
    let details = CommitDetails {
        sha: sha('1'),
        body: "On main: wip parser".into(),
        files: vec![
            CommitFile { status: 'M', path: "src/parser.rs".into(), old: None },
            CommitFile { status: 'D', path: "old.txt".into(), old: None },
        ],
    };
    app.handle_msg(AppMsg::Git(GitMsg::CommitDetails { generation: g, sha: sha('1'), result: Ok(details) }));
    let term = draw_app(app, 100, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("git_stash_tab", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn git_console_tab() {
    let mut fx = stable_app("git-console");
    let app = &mut fx.app;
    app.toggle_tool_window(ToolWindowId::Git);
    app.perform(Action::Git(crate::actions::GitAction::TabConsole));
    app.git_panel.log_op("git fetch --prune", true, "From example.org:demo\n   1111111..2222222  main       -> origin/main");
    app.git_panel.log_op("git push", false, "error: failed to push some refs to 'example.org:demo'\nhint: Updates were rejected");
    app.git_panel.log_op("git switch dev", false, "");
    let term = draw_app(app, 100, 24);
    let marks =
        [Mark::style('$', "command", app.theme.status_accent.add_modifier(Modifier::BOLD)), Mark::style('E', "error", app.theme.error)];
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("git_console_tab", snapshot(&term, &marks, Some(bottom)));
}

/// A project tree with a few entries, `src/main.rs` selected and drawn
/// once (a context menu opens at the selected row as last drawn).
fn tree_fixture(app: &mut App) {
    let root = app.workspace.root.clone();
    std::fs::create_dir_all(root.join("src")).unwrap();
    for name in ["src/main.rs", "src/lib.rs", "README.md"] {
        std::fs::write(root.join(name), "").unwrap();
    }
    load_tree(app);
    assert!(app.project.select_path(&root.join("src/main.rs")));
    app.focus.set(PanelId::Project);
    draw_app(app, 80, 24);
}

fn tree(app: &mut App, a: crate::actions::TreeAction) {
    app.perform(Action::Tree(a));
}

/// The selected row or button, and disabled rows and shortcuts.
fn menu_marks(theme: &Theme) -> Vec<Mark> {
    vec![Mark::bg('L', "selected", theme.list_selected.bg.unwrap()), Mark::style('d', "dim", theme.dim.bg(theme.popup_bg))]
}

#[tokio::test]
async fn tree_context_menu() {
    let mut fx = stable_app("tree-context-menu");
    let app = &mut fx.app;
    tree_fixture(app);
    tree(app, crate::actions::TreeAction::ContextMenu);
    let term = draw_app(app, 80, 24);
    let marks = menu_marks(&app.theme);
    assert_snapshot!("tree_context_menu", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn confirm_dialog() {
    let mut fx = stable_app("confirm-dialog");
    let app = &mut fx.app;
    tree_fixture(app);
    tree(app, crate::actions::TreeAction::Delete);
    let term = draw_app(app, 80, 24);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("confirm_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn toasts_below_dialog() {
    use crate::event::Level;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut fx = stable_app("toasts-below-dialog");
    let app = &mut fx.app;
    tree_fixture(app);
    tree(app, crate::actions::TreeAction::Delete);
    app.toasts.push(
        Level::Warn,
        "Language servers are off, so completion, hover and diagnostics are unavailable until they are on",
        std::time::Instant::now(),
    );
    let term = draw_app(app, 80, 24);
    let marks =
        [Mark::style('w', "warn", app.theme.warn.bg(app.theme.popup_bg)), Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("toasts_below_dialog", snapshot(&term, &marks, None));
    let row = (0..24).find(|&y| crate::ui::snapshot::row_text(term.backend().buffer(), y, 80).contains("Warning")).unwrap();
    let column = (0..80).find(|&x| term.backend().buffer()[(x, row)].symbol() == "W").unwrap();
    app.handle_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE });
    assert!(app.toasts.is_empty());
    assert!(app.dialog.is_some());
}

#[tokio::test]
async fn confirm_dialog_narrow() {
    let mut fx = stable_app("confirm-dialog-narrow");
    let app = &mut fx.app;
    tree_fixture(app);
    tree(app, crate::actions::TreeAction::Delete);
    let term = draw_app(app, 50, 20);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("confirm_dialog_narrow", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn input_dialog() {
    let mut fx = stable_app("input-dialog");
    let app = &mut fx.app;
    tree_fixture(app);
    tree(app, crate::actions::TreeAction::Rename);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("input_dialog", snapshot(&term, &[], None));
}

#[tokio::test]
async fn choice_dialog() {
    let mut fx = stable_app("choice-dialog");
    let app = &mut fx.app;
    tree_fixture(app);
    tree(app, crate::actions::TreeAction::New);
    let term = draw_app(app, 80, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    assert_snapshot!("choice_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn main_menu_bar() {
    let mut fx = stable_app("main-menu");
    let app = &mut fx.app;
    // No tab open: the rows that need one are disabled. Edit is open.
    app.toggle_main_menu();
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    let term = draw_app(app, 100, 30);
    let marks = menu_marks(&app.theme);
    assert_snapshot!("main_menu_bar", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn hover_popup() {
    let mut fx = stable_app("hover");
    let app = &mut fx.app;
    load(app, "main.log", "fn main() {\n    let n = add_one(1);\n}\n");
    moves(app, Motion::Down, 1);
    moves(app, Motion::Right, 14);
    app.focus.set(PanelId::Editor);
    // The popup as a hover answer makes it.
    let b = app.editor.active_buffer().unwrap();
    let hl = crate::ui::hover::Highlight { theme: &app.theme, fallback: None };
    let contents = lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
        kind: lsp_types::MarkupKind::Markdown,
        value: "```rust\nfn add_one(x: u32) -> u32\n```\n---\n# add_one\nAdds **one** to `x`, wrapping at `u32::MAX`; see [the docs](https://example.org/add_one) for more.".into(),
    });
    let popup = crate::ui::hover::HoverPopup::new(app.editor.focused_group().id, b.id, b.version, b.view.sel().head, contents, hl);
    app.hover = Some(popup.expect("hover text"));
    let term = draw_app(app, 80, 20);
    // Code lines keep the editor background; prose is on the popup's.
    let kw = app.theme.capture_style("keyword").unwrap();
    let marks = [Mark::bg('P', "popup", app.theme.popup_bg), Mark::style('K', "keyword", kw.bg(app.theme.bg))];
    assert_snapshot!("hover_popup", snapshot(&term, &marks, None));
    assert!(app.hover.is_some(), "the popup still applies");
}

#[tokio::test]
async fn completion_popup() {
    use crate::lsp::completion::{CompletionState, Origin};
    let mut fx = stable_app("completion");
    let app = &mut fx.app;
    load(app, "main.log", "fn main() {\n    pri\n}\n");
    moves(app, Motion::Down, 1);
    editor(app, EditorAction::Move(Motion::LineEnd, false));
    app.focus.set(PanelId::Editor);
    // The list as a completion answer makes it, filtered by `pri`.
    let b = app.editor.active_buffer().unwrap();
    let origin = Origin::new(b.id, &b.rope, b.view.sel().head);
    let (items, incomplete) = crate::lsp::completion::parse(serde_json::json!([
        {"label": "println!", "kind": 3, "detail": "macro_rules! println"},
        {"label": "print!", "kind": 3, "detail": "macro_rules! print", "labelDetails": {"detail": "(…)"}},
        {"label": "primary_color", "kind": 6, "detail": "Color"},
        {"label": "priority", "kind": 5, "detail": "u8", "deprecated": true},
        {"label": "unrelated", "kind": 14},
    ]));
    let state = CompletionState::new(origin, items, incomplete, "pri", &mut app.lsp.completion.matcher);
    app.completion = Some(crate::ui::completion::CompletionPopup::new(state, app.editor.focused_group().id));
    let term = draw_app(app, 80, 20);
    let marks = [
        Mark::modifier('U', "underlined", Modifier::UNDERLINED),
        Mark::style('M', "matched", ratatui::style::Style::default().fg(app.theme.status_accent.fg.unwrap()).add_modifier(Modifier::BOLD)),
        Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap()),
        Mark::modifier('X', "deprecated", Modifier::CROSSED_OUT),
    ];
    assert_snapshot!("completion_popup", snapshot(&term, &marks, None));
    assert!(app.completion.is_some(), "the popup still applies");
}

/// A popup over `pri` with documentation on its items, drawn at `w`x`h`
/// after the panel's delay (no server: the item's own docs are used).
/// `wide` adds an item that makes the list too wide for a panel beside it.
fn completion_with_docs(app: &mut App, w: u16, h: u16, wide: bool) -> Terminal<TestBackend> {
    use crate::lsp::completion::{CompletionState, Origin};
    load(app, "main.log", "fn main() {\n    pri\n}\n");
    moves(app, Motion::Down, 1);
    editor(app, EditorAction::Move(Motion::LineEnd, false));
    app.focus.set(PanelId::Editor);
    let b = app.editor.active_buffer().unwrap();
    let origin = Origin::new(b.id, &b.rope, b.view.sel().head);
    let docs = "Prints to the standard output, with a newline.\n\n```rust\nprintln!(\"{}\", 1);\n```\n\nSee also `print!`.";
    let mut list = vec![
        serde_json::json!({"label": "println!", "kind": 3, "detail": "fn println!(…)", "documentation": {"kind": "markdown", "value": docs}}),
        serde_json::json!({"label": "print!", "kind": 3, "detail": "fn print!(…)", "documentation": {"kind": "markdown", "value": docs}}),
    ];
    if wide {
        list.push(serde_json::json!({"label": "print_everything_to_the_log", "kind": 3,
            "labelDetails": {"description": "crate::logging::print_everything_to_the_log"}}));
    }
    let (items, incomplete) = crate::lsp::completion::parse(serde_json::Value::Array(list));
    let state = CompletionState::new(origin, items, incomplete, "pri", &mut app.lsp.completion.matcher);
    app.completion = Some(crate::ui::completion::CompletionPopup::new(state, app.editor.focused_group().id));
    let now = std::time::Instant::now();
    app.completion_docs_tick(now);
    app.completion_docs_tick(now + std::time::Duration::from_millis(150));
    assert!(app.completion.as_ref().unwrap().docs.is_some(), "the docs are shown");
    draw_app(app, w, h)
}

fn docs_marks(app: &App) -> [Mark; 2] {
    [Mark::bg('P', "popup bg", app.theme.popup_bg), Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())]
}

#[tokio::test]
async fn completion_docs_beside() {
    let mut fx = stable_app("completion-docs-beside");
    let app = &mut fx.app;
    let term = completion_with_docs(app, 120, 40, false);
    assert_snapshot!("completion_docs_beside", snapshot(&term, &docs_marks(app), None));
}

#[tokio::test]
async fn completion_docs_stacked() {
    let mut fx = stable_app("completion-docs-stacked");
    let app = &mut fx.app;
    let term = completion_with_docs(app, 80, 24, true);
    assert_snapshot!("completion_docs_stacked", snapshot(&term, &docs_marks(app), None));
}

#[tokio::test]
async fn toasts() {
    use crate::event::Level;
    let mut fx = stable_app("toasts");
    let app = &mut fx.app;
    // Toasts expire on the tick, never while drawing, so the time they
    // were pushed at does not show.
    let now = std::time::Instant::now();
    app.toasts.push(Level::Info, "Saved notes.txt", now);
    app.toasts.push(Level::Warn, "Language servers are off", now);
    app.toasts.push(
        Level::Error,
        "Push failed: error: failed to push some refs to 'example.org:demo'; the remote has commits you do not have yet, so pull first",
        now,
    );
    let term = draw_app(app, 100, 30);
    let marks = [
        Mark::style('i', "info", app.theme.info.bg(app.theme.popup_bg)),
        Mark::style('w', "warn", app.theme.warn.bg(app.theme.popup_bg)),
        Mark::style('e', "error", app.theme.error.bg(app.theme.popup_bg)),
    ];
    assert_snapshot!("toasts", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_find_replace_bar() {
    let mut fx = stable_app("find-replace-bar");
    let app = &mut fx.app;
    load(app, "notes.log", "alpha beta\nbeta gamma beta\nBeta four\n");
    app.focus.set(PanelId::Editor);
    editor(app, EditorAction::Replace);
    for c in "beta".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    // The second match (2/4) is current; it is selected, so it shows as
    // the selection. Matching ignores case.
    editor(app, EditorAction::FindNext);
    let term = draw_editor(app, 80, 10);
    let marks = [Mark::bg('H', "match", app.theme.search_match_bg), Mark::bg('S', "selection", app.theme.selection_bg)];
    assert_snapshot!("editor_find_replace_bar", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_lightbulb() {
    use crate::lsp::diagnostics::Severity;
    use crate::lsp::diagnostics::tests::diag;
    use crate::ui::snapshot::row_text;
    let mut fx = stable_app("lightbulb");
    let app = &mut fx.app;
    let main = load(app, "main.log", "fn main() {\n    let x: u8 = \"one\";\n    unused();\n}\n");
    let per_file = app.config.lsp.max_diagnostics_per_file;
    app.lsp.diagnostics.publish(
        "fake",
        main,
        vec![diag(1, 16, 1, 21, Severity::Error, "mismatched types"), diag(2, 4, 2, 10, Severity::Warning, "never used")],
        per_file,
    );
    app.sync_diag_marks();
    let b = app.editor.active_buffer_mut().unwrap();
    b.goto_line(1, 4);
    b.lightbulb = Some((b.version, 1));
    let term = draw_editor(app, 40, 6);
    let mut marks = vec![Mark::style('L', "lightbulb", app.theme.lightbulb)];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_lightbulb", snapshot(&term, &marks, None));

    // Unfocused, or for an older version: the diagnostic's glyph.
    let theme = app.theme.clone();
    let mut term = Terminal::new(TestBackend::new(40, 6)).unwrap();
    term.draw(|f| app.editor.render(f, Rect::new(0, 0, 40, 1), Rect::new(0, 1, 40, 5), false, &theme)).unwrap();
    let line = row_text(term.backend().buffer(), 2, 40);
    assert!(line.contains('✖') && !line.contains('✱'), "{line}");
    let b = app.editor.active_buffer_mut().unwrap();
    b.lightbulb = Some((b.version + 1, 1));
    let term = draw_editor(app, 40, 6);
    let line = row_text(term.backend().buffer(), 2, 40);
    assert!(line.contains('✖') && !line.contains('✱'), "{line}");
}
