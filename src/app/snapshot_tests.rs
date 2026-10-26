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
use crate::actions::{Action, AppAction, EditorAction, HistoryAction, MergeAction, Motion, PaletteKind};
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
async fn editor_patch_tab() {
    let mut fx = stable_app("patch-tab");
    let app = &mut fx.app;
    app.editor.open_patch(
        "Diff 1a2b3c4d",
        Vec::new(),
        "== 1a2b3c4d Fix\ndiff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n same\n",
    );
    let term = draw_editor(app, 60, 12);
    let marks = [Mark::bg('+', "added", app.theme.diff_add_bg), Mark::bg('-', "removed", app.theme.diff_del_bg)];
    assert_snapshot!("editor_patch_tab", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_patch_tab_folded() {
    let mut fx = stable_app("patch-tab-folded");
    let app = &mut fx.app;
    app.editor.open_patch(
        "Diff of 2 commits",
        Vec::new(),
        "== 1a2b3c4d Fix\ndiff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n same\ndiff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-b\n+b2\n\n== 5e6f7a8b Two\n(no changes)\n",
    );
    app.editor.active_buffer_mut().unwrap().toggle_fold(1);
    let term = draw_editor(app, 60, 16);
    let marks = [Mark::bg('+', "added", app.theme.diff_add_bg), Mark::bg('-', "removed", app.theme.diff_del_bg)];
    assert_snapshot!("editor_patch_tab_folded", snapshot(&term, &marks, None));
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
async fn editor_carets_virtual_without_block() {
    use crate::editor::selection::{Selection, Selections};
    let mut fx = stable_app("carets-virtual-without-block");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nab\nabcdefgh\nabc\n");
    let b = app.editor.active_buffer_mut().unwrap();
    b.view.sels = Selections::new(vec![Selection::virtual_point(11, 6), Selection::point(15), Selection::virtual_point(24, 10)], 2);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_virtual_without_block", snapshot(&term, &marks, None));
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
async fn editor_carets_column_virtual_arrows() {
    let mut fx = stable_app("carets-column-virtual-arrows");
    let app = &mut fx.app;
    load(app, "cols.log", "abcdefgh\nab\nab\nabcdefgh\n");
    editor(app, EditorAction::ToggleColumnMode);
    moves(app, Motion::Right, 6);
    editor(app, EditorAction::Move(Motion::Down, true));
    moves(app, Motion::Down, 2);
    moves(app, Motion::Right, 3);
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual_arrows", snapshot(&term, &marks, None));
    editor(app, EditorAction::InsertChar('X'));
    let term = draw_editor(app, 40, 8);
    let marks = editor_marks(&app.theme);
    assert_snapshot!("editor_carets_column_virtual_arrows_typed", snapshot(&term, &marks, None));
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

#[cfg(all(feature = "lang-md", feature = "lang-rust"))]
#[tokio::test]
async fn editor_markdown_fence() {
    let mut fx = stable_app("markdown-fence");
    let app = &mut fx.app;
    load(app, "notes.md", "# Title\n\nSome prose.\n\n```rust\nfn main() {\n    let s = \"hi\";\n}\n```\n");
    let term = draw_editor(app, 50, 12);
    let mut marks = editor_marks(&app.theme);
    marks.push(Mark::style('K', "keyword", app.theme.capture_style("keyword").unwrap()));
    marks.push(Mark::style('S', "string", app.theme.capture_style("string").unwrap()));
    assert_snapshot!("editor_markdown_fence", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_indent_guides() {
    let mut fx = stable_app("indent-guides");
    let app = &mut fx.app;
    load(app, "guides.log", "fn a() {\n    if x {\n        y();\n\n        z();\n    }\n\n}\n");
    let term = draw_editor(app, 40, 10);
    let mut marks = vec![Mark::style('G', "guide", app.theme.indent_guide)];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_indent_guides", snapshot(&term, &marks, None));
    let mut c = app.config.clone();
    c.editor.indent_guides = false;
    app.editor.apply_config(&c);
    let term = draw_editor(app, 40, 10);
    assert_snapshot!("editor_indent_guides_off", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_inline_blame() {
    use crate::git::model::{Blame, BlameCommit};
    let mut fx = stable_app("inline-blame");
    let app = &mut fx.app;
    let path = load(app, "notes.log", PLAIN);
    let mut c = app.config.clone();
    c.editor.inline_blame = true;
    app.editor.apply_config(&c);
    assert_eq!(app.editor.want_inline_blame().map(|(p, _)| p), Some(path.clone()));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let commit = BlameCommit {
        sha: "s".into(),
        author: "Ann".into(),
        time: now - 3 * 86_400 - 100,
        offset_minutes: 0,
        summary: "Fix the status bar width".into(),
    };
    app.editor.set_blame(&path, 1, Ok(Blame { commits: vec![commit], lines: vec![0; 4] }));
    moves(app, Motion::Down, 1);
    // A style of its own, so the tab-bar filler (dim) is not marked too.
    app.theme.inline_blame = app.theme.inline_blame.fg(ratatui::style::Color::Magenta).add_modifier(Modifier::ITALIC);
    let term = draw_editor(app, 70, 8);
    let mut marks = vec![Mark::style('B', "inline blame", app.theme.inline_blame)];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_inline_blame", snapshot(&term, &marks, None));
    let shown = |term: &Terminal<TestBackend>, w: u16| {
        (0..8).filter(|&y| crate::ui::snapshot::row_text(term.backend().buffer(), y, w).contains("Ann,")).count()
    };
    assert_eq!(shown(&term, 70), 1);
    // A selection hides it.
    editor(app, EditorAction::Move(Motion::Right, true));
    assert_eq!(shown(&draw_editor(app, 70, 8), 70), 0);
    // Lines past the blamed ones have none.
    moves(app, Motion::Down, 3);
    assert_eq!(shown(&draw_editor(app, 70, 8), 70), 0);
    // Fewer than 10 cells left.
    moves(app, Motion::Up, 3);
    assert_eq!(shown(&draw_editor(app, 26, 8), 26), 0);
    // Only the focused group paints it.
    draw_editor(app, 140, 8);
    editor(app, EditorAction::SplitRight);
    assert_eq!(shown(&draw_editor(app, 140, 8), 140), 1);
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
async fn editor_breadcrumbs() {
    let mut fx = stable_app("breadcrumbs");
    let app = &mut fx.app;
    load(app, "point.rs", RUST);
    // Into `fn new`'s body.
    moves(app, Motion::Down, 7);
    let mut c = app.config.clone();
    c.editor.breadcrumbs = true;
    app.editor.apply_config(&c);
    // A style of its own, so the tab-bar filler (dim) is not marked too.
    app.theme.breadcrumbs = app.theme.breadcrumbs.fg(ratatui::style::Color::Magenta).add_modifier(Modifier::ITALIC);
    let term = draw_editor(app, 60, 12);
    let mut marks = vec![
        Mark::style('D', "breadcrumbs separator", app.theme.breadcrumbs.patch(app.theme.dim)),
        Mark::style('B', "breadcrumbs", app.theme.breadcrumbs),
    ];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_breadcrumbs", snapshot(&term, &marks, None));
}

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn editor_breadcrumbs_split() {
    let mut fx = stable_app("breadcrumbs-split");
    let app = &mut fx.app;
    load(app, "point.rs", RUST);
    moves(app, Motion::Down, 7);
    let mut c = app.config.clone();
    c.editor.breadcrumbs = true;
    app.editor.apply_config(&c);
    editor(app, EditorAction::SplitRight);
    // The new group's caret into `fn main`.
    moves(app, Motion::Down, 9);
    // A style of its own, so the tab-bar filler (dim) is not marked too.
    app.theme.breadcrumbs = app.theme.breadcrumbs.fg(ratatui::style::Color::Magenta).add_modifier(Modifier::ITALIC);
    let term = draw_editor(app, 80, 12);
    let mut marks = vec![
        Mark::style('D', "breadcrumbs separator", app.theme.breadcrumbs.patch(app.theme.dim)),
        Mark::style('B', "breadcrumbs", app.theme.breadcrumbs),
    ];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_breadcrumbs_split", snapshot(&term, &marks, None));
}

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn editor_sticky_scroll() {
    let mut fx = stable_app("sticky-scroll");
    let app = &mut fx.app;
    load(app, "point.rs", RUST);
    // Into `fn sum`'s body, scrolled past `impl Point {`.
    moves(app, Motion::Down, 11);
    let mut c = app.config.clone();
    c.editor.sticky_scroll = true;
    app.editor.apply_config(&c);
    app.editor.active_buffer_mut().unwrap().view.set_top(crate::editor::rows::Row::line(7));
    let term = draw_editor(app, 60, 12);
    let mut marks = vec![Mark::bg('P', "sticky scroll", app.theme.sticky_scroll.bg.unwrap())];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_sticky_scroll", snapshot(&term, &marks, None));
}

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn editor_sticky_scroll_split() {
    let mut fx = stable_app("sticky-scroll-split");
    let app = &mut fx.app;
    load(app, "point.rs", RUST);
    moves(app, Motion::Down, 11);
    let mut c = app.config.clone();
    c.editor.sticky_scroll = true;
    app.editor.apply_config(&c);
    app.editor.active_buffer_mut().unwrap().view.set_top(crate::editor::rows::Row::line(7));
    editor(app, EditorAction::SplitRight);
    app.editor.active_buffer_mut().unwrap().view.set_top(crate::editor::rows::Row::line(6));
    let term = draw_editor(app, 80, 12);
    let mut marks = vec![Mark::bg('P', "sticky scroll", app.theme.sticky_scroll.bg.unwrap())];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_sticky_scroll_split", snapshot(&term, &marks, None));
}

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
        in_progress: None,
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

/// A rebase stopped at step 2 of 5: the Git window header and the status
/// bar both carry the label.
#[tokio::test]
async fn git_window_rebasing() {
    use crate::git::model::{InProgress, InProgressKind};
    let mut fx = stable_app("git-rebasing");
    let app = &mut fx.app;
    git_commit_fixture(app);
    let mut status = crate::git::repo::status(&app.workspace.root).unwrap();
    status.branch = None;
    status.head = Some(sha('a'));
    status.in_progress = Some(InProgress { kind: InProgressKind::Rebase, step: Some((2, 5)) });
    app.handle_msg(AppMsg::Git(GitMsg::Status { generation: app.git.generation, seq: 2, result: Ok(status) }));
    app.dialog = None;
    let term = draw_app(app, 80, 24);
    let marks = [
        Mark::style('w', "in progress (Git window)", app.theme.warn.bg(app.theme.panel_bg)),
        Mark::style('s', "in progress (status bar)", app.theme.warn.bg(app.theme.status_bg)),
    ];
    assert_snapshot!("git_window_rebasing", snapshot(&term, &marks, None));
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

#[tokio::test]
async fn palette_recent_locations() {
    use crate::lsp::locations::LocationItem;
    let mut fx = stable_app("palette-recent-locations");
    let app = &mut fx.app;
    let root = app.workspace.root.clone();
    let item = |file: &str, line, text: &str| LocationItem::at_line(root.join(file), line, 4, text);
    let items = vec![
        item("src/a.rs", 9, "    helper(total);"),
        item("src/b.rs", 4, "    println!(\"{}\", helper(2));"),
        item("src/a.rs", 2, "    let total = helper(1);"),
    ];
    app.open_locations("Recent Locations".into(), items, true);
    let term = draw_app(app, 100, 30);
    let marks = palette_marks(&app.theme);
    assert_snapshot!("palette_recent_locations", snapshot(&term, &marks, None));
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

#[tokio::test]
async fn todo_window() {
    let mut fx = stable_app("todo");
    let app = &mut fx.app;
    let root = app.workspace.root.clone();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/parse.log"), "fn parse() {\n    // TODO: handle errors\n}\n").unwrap();
    std::fs::write(root.join("notes.txt"), "FIXME: wording\nnothing\n// TODO and FIXME on one line\n").unwrap();
    std::fs::write(root.join("other.txt"), "no hits here\n").unwrap();
    app.toggle_tool_window(ToolWindowId::Todo);
    // The scan runs here, so the render arm finds it started.
    let search = app.todo.search(&root, false, false);
    let mut batches = Vec::new();
    search.run(|b| batches.push(b));
    let generation = app.workspace.generation;
    for batch in batches {
        app.handle_msg(AppMsg::Todo { generation, batch });
    }
    let term = draw_app(app, 80, 24);
    let marks = find_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("todo_window", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn hierarchy_window() {
    use crate::actions::HierarchyAction;
    use crate::lsp::hierarchy::{Direction, HierarchyTree, parse_items};
    let mut fx = stable_app("hierarchy");
    let app = &mut fx.app;
    let root = app.workspace.root.clone();
    let item = |name: &str, file: &str, line: u32, detail: Option<&str>| {
        let range = serde_json::json!({"start": {"line": line, "character": 0}, "end": {"line": line, "character": 4}});
        serde_json::json!({
            "name": name, "kind": 12, "detail": detail, "uri": crate::lsp::sync::uri(&root.join(file)).unwrap().as_str(),
            "range": range, "selectionRange": range,
        })
    };
    let mut tree = HierarchyTree::new(
        1,
        "rust-analyzer".into(),
        Direction::Callers,
        parse_items(serde_json::json!([item("helper", "src/b.rs", 1, Some("fn helper(s: &str)"))])),
    );
    tree.set_children(
        0,
        parse_items(serde_json::json!([
            item("main", "src/a.rs", 0, None),
            item("run", "src/run.rs", 41, None),
            item("check", "src/a.rs", 9, None),
        ])),
    );
    tree.nodes[2].loading = true;
    tree.set_children(3, Vec::new());
    app.hierarchy = Some(tree);
    app.toggle_tool_window(ToolWindowId::Hierarchy);
    app.perform(Action::Hierarchy(HierarchyAction::Down));
    app.theme.dim = app.theme.dim.fg(ratatui::style::Color::Magenta);
    let term = draw_app(app, 80, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap()), Mark::style('D', "dim", app.theme.dim)];
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("hierarchy_window", snapshot(&term, &marks, Some(bottom)));
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
async fn git_log_multi_select() {
    use crate::git::model::{CommitDetails, CommitFile};
    let mut fx = stable_app("git-log-multi");
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
    // The cursor moves from the merge to the next row; both are selected.
    app.perform(Action::Git(crate::actions::GitAction::ExtendDown));
    let details = CommitDetails {
        sha: sha('d'),
        body: "Parse nested lists".into(),
        files: vec![CommitFile { status: 'M', path: "src/parser.rs".into(), old: None }],
    };
    app.handle_msg(AppMsg::Git(GitMsg::CommitDetails { generation: g, sha: sha('d'), result: Ok(details) }));
    let term = draw_app(app, 120, 30);
    assert_ne!(app.theme.list_selected.bg, app.theme.list_selected_unfocused.bg);
    let marks = [
        Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap()),
        Mark::bg('U', "also selected", app.theme.list_selected_unfocused.bg.unwrap()),
    ];
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("git_log_multi_select", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn git_log_context_menu() {
    use crate::git::BranchInfo;
    let mut fx = stable_app("git-log-menu");
    let app = &mut fx.app;
    app.toggle_tool_window(ToolWindowId::Git);
    app.perform(Action::Git(crate::actions::GitAction::TabLog));
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
    let b = |name: &str, current| BranchInfo { name: name.into(), upstream: None, ahead: 0, behind: 0, current, remote: false };
    app.git_panel.branches = vec![b("main", true), b("parser", false)];
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.perform(Action::Git(crate::actions::GitAction::ContextMenu));
    let term = draw_app(app, 120, 30);
    let marks = menu_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("git_log_context_menu", snapshot(&term, &marks, Some(bottom)));
}

#[tokio::test]
async fn git_reset_dialog() {
    use crate::git::model::StatusSnapshot;
    let mut fx = stable_app("git-reset");
    let app = &mut fx.app;
    app.toggle_tool_window(ToolWindowId::Git);
    app.perform(Action::Git(crate::actions::GitAction::TabLog));
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
    let snap = StatusSnapshot { branch: Some("main".into()), head: Some(sha('a')), ..Default::default() };
    app.handle_msg(AppMsg::Git(GitMsg::Status { generation: g, seq: 1, result: Ok(snap) }));
    app.perform(Action::Git(crate::actions::GitAction::Down));
    app.perform(Action::Git(crate::actions::GitAction::LogReset));
    let term = draw_app(app, 80, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    assert_snapshot!("git_reset_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn git_push_dialog() {
    use crate::git::model::StatusSnapshot;
    use crate::ui::dialog::DialogResult;
    let mut fx = stable_app("git-push-dialog");
    let app = &mut fx.app;
    let g = app.git.generation;
    let snap = StatusSnapshot { branch: Some("main".into()), ..Default::default() };
    app.handle_msg(AppMsg::Git(GitMsg::Status { generation: g, seq: 1, result: Ok(snap) }));
    app.perform(Action::Git(crate::actions::GitAction::PushOptions));
    let d = app.dialog.take().unwrap();
    app.finish_dialog(d.id, DialogResult::Button(1));
    let term = draw_app(app, 80, 24);
    let marks = [Mark::style('B', "selected button", app.theme.list_selected)];
    assert_snapshot!("git_push_dialog", snapshot(&term, &marks, None));
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

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn surround_with_dialog() {
    let mut fx = stable_app("surround-with");
    let app = &mut fx.app;
    load(app, "main.rs", "fn main() {\n    a();\n    b();\n}\n");
    app.focus.set(PanelId::Editor);
    let b = app.editor.active_buffer_mut().unwrap();
    let (a, h) = (b.rope.line_to_char(1), b.rope.line_to_char(3));
    b.view.set_sel(Selection::new(a, h));
    app.perform(Action::Editor(EditorAction::SurroundWith));
    let term = draw_app(app, 80, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    assert_snapshot!("surround_with_dialog", snapshot(&term, &marks, None));
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

#[cfg(feature = "lang-rust")]
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

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn quick_definition_popup() {
    let mut fx = stable_app("quick-def");
    let app = &mut fx.app;
    load(app, "main.log", "fn main() {\n    let n = add_one(1);\n}\n");
    moves(app, Motion::Down, 1);
    moves(app, Motion::Right, 14);
    app.focus.set(PanelId::Editor);
    let b = app.editor.active_buffer().unwrap();
    let (group, id, version, head) = (app.editor.focused_group().id, b.id, b.version, b.view.sel().head);
    let code = "pub fn add_one(x: u32) -> u32 {\n    x.wrapping_add(1)\n}";
    let lines = super::lsp_nav::definition_lines("src/math.rs:12", code, crate::editor::syntax::language_for_name("rust"), &app.theme);
    app.hover = crate::ui::hover::HoverPopup::from_lines(group, id, version, head, lines);
    let term = draw_app(app, 80, 20);
    let kw = app.theme.capture_style("keyword").unwrap();
    let marks = [Mark::bg('P', "popup", app.theme.popup_bg), Mark::style('K', "keyword", kw.bg(app.theme.bg))];
    assert_snapshot!("quick_definition_popup", snapshot(&term, &marks, None));
    assert!(app.hover.is_some(), "the popup still applies");
}

#[tokio::test]
async fn signature_popup() {
    use lsp_types::{ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation};
    let mut fx = stable_app("signature");
    let app = &mut fx.app;
    load(app, "main.log", "fn main() {\n\n\n\n\n    let n = add(1, 2);\n}\n");
    moves(app, Motion::Down, 5);
    moves(app, Motion::Right, 19);
    app.focus.set(PanelId::Editor);
    let param = |a, b| ParameterInformation { label: ParameterLabel::LabelOffsets([a, b]), documentation: None };
    let sig = |label: &str, parameters| SignatureInformation {
        label: label.into(),
        documentation: None,
        parameters: Some(parameters),
        active_parameter: None,
    };
    let help = SignatureHelp {
        signatures: vec![
            sig("fn add(a: u32, b: u32) -> u32", vec![param(7, 13), param(15, 21)]),
            sig(
                "fn add(a: u64) -> u64",
                vec![ParameterInformation { label: ParameterLabel::Simple("a: u64".into()), documentation: None }],
            ),
        ],
        active_signature: Some(0),
        active_parameter: Some(1),
    };
    let b = app.editor.active_buffer().unwrap();
    app.signature = crate::ui::signature::SignaturePopup::new(app.editor.focused_group().id, b.id, help);
    assert!(app.signature.is_some());
    let term = draw_app(app, 80, 20);
    // Bold first: it wins over the popup background; the area leaves out the bold chrome.
    let marks = [Mark::modifier('B', "bold", Modifier::BOLD), Mark::bg('P', "popup", app.theme.popup_bg)];
    let area = ratatui::layout::Rect::new(36, 0, 44, 18);
    assert_snapshot!("signature_popup", snapshot(&term, &marks, Some(area)));
    assert!(app.signature.is_some(), "the popup still applies");
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

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn completion_popup_snippets() {
    use crate::lsp::completion::{CompletionState, Origin};
    let mut fx = stable_app("completion-snippets");
    let app = &mut fx.app;
    load(app, "main.rs", "fn main() {\n    fn\n}\n");
    moves(app, Motion::Down, 1);
    editor(app, EditorAction::Move(Motion::LineEnd, false));
    app.focus.set(PanelId::Editor);
    let b = app.editor.active_buffer().unwrap();
    let id = b.id;
    let origin = Origin::new(id, &b.rope, b.view.sel().head);
    let at = origin.start;
    let (items, incomplete) = crate::lsp::completion::parse(serde_json::json!([
        {"label": "fn_ptr", "kind": 3, "detail": "fn()"},
        {"label": "FnMut", "kind": 8, "detail": "trait"},
    ]));
    let mut state = CompletionState::new(origin, items, incomplete, "fn", &mut app.lsp.completion.matcher);
    let snippets = app.snippet_items(id, at);
    state.add_snippets(snippets, &mut app.lsp.completion.matcher);
    let names: Vec<&str> = state.order.iter().map(|&i| state.items[i].label.as_str()).collect();
    assert_eq!(names, ["fn_ptr", "FnMut", "fn"]);
    app.completion = Some(crate::ui::completion::CompletionPopup::new(state, app.editor.focused_group().id));
    let term = draw_app(app, 80, 20);
    let marks = [
        Mark::modifier('U', "underlined", Modifier::UNDERLINED),
        Mark::style('M', "matched", ratatui::style::Style::default().fg(app.theme.status_accent.fg.unwrap()).add_modifier(Modifier::BOLD)),
        Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap()),
        Mark::modifier('X', "deprecated", Modifier::CROSSED_OUT),
    ];
    assert_snapshot!("completion_popup_snippets", snapshot(&term, &marks, None));
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

fn press(app: &mut App, code: KeyCode, times: usize) {
    for _ in 0..times {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
}

fn settings_marks(theme: &Theme) -> [Mark; 2] {
    [Mark::bg('L', "selected", theme.list_selected.bg.unwrap()), Mark::bg('U', "unfocused", theme.list_selected_unfocused.bg.unwrap())]
}

#[tokio::test]
async fn settings_window() {
    let mut fx = stable_app("settings-window");
    let app = &mut fx.app;
    std::fs::create_dir_all(&app.paths.config_dir).unwrap();
    std::fs::write(app.paths.config_file(), "[lsp]\nenabled = false\n[editor]\nscrolloff = 5\n").unwrap();
    app.perform(Action::App(AppAction::Settings));
    press(app, KeyCode::Down, 2);
    press(app, KeyCode::Enter, 1);
    press(app, KeyCode::Down, 1);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("settings_window", snapshot(&term, &settings_marks(&app.theme), None));
}

#[tokio::test]
async fn settings_window_wide() {
    let mut fx = stable_app("settings-window-wide");
    let app = &mut fx.app;
    app.perform(Action::App(AppAction::Settings));
    let term = draw_app(app, 120, 40);
    assert_snapshot!("settings_window_wide", snapshot(&term, &settings_marks(&app.theme), None));
}

#[tokio::test]
async fn settings_window_error() {
    let mut fx = stable_app("settings-window-error");
    let app = &mut fx.app;
    std::fs::create_dir_all(&app.paths.config_dir).unwrap();
    std::fs::write(app.paths.config_file(), "[editor\n").unwrap();
    app.perform(Action::App(AppAction::Settings));
    press(app, KeyCode::Enter, 1);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("settings_window_error", snapshot(&term, &settings_marks(&app.theme), None));
}

#[tokio::test]
async fn settings_window_pending() {
    let mut fx = stable_app("settings-window-pending");
    let app = &mut fx.app;
    std::fs::create_dir_all(&app.paths.config_dir).unwrap();
    std::fs::write(app.paths.config_file(), "[lsp]\nenabled = false\n").unwrap();
    app.perform(Action::App(AppAction::Settings));
    press(app, KeyCode::Down, 4);
    press(app, KeyCode::Enter, 1);
    // Autosave: off -> focus -> idle; Final newline toggled.
    press(app, KeyCode::Right, 2);
    press(app, KeyCode::Down, 3);
    press(app, KeyCode::Enter, 1);
    let term = draw_app(app, 80, 24);
    let [l, u] = settings_marks(&app.theme);
    let m = Mark::style('M', "pending", app.theme.git_modified);
    assert_snapshot!("settings_window_pending", snapshot(&term, &[l, u, m], None));
}

fn fixed_report() -> crate::doctor::Report {
    use crate::doctor::{Check, Group, Report, Status};
    let c = |group, name: &str, status, detail: &str| Check { group, name: name.into(), status, detail: detail.into() };
    Report {
        checks: vec![
            c(Group::Lsp, "rust-analyzer", Status::Ok, "/usr/bin/rust-analyzer (rust)"),
            c(Group::Lsp, "gopls", Status::Missing, "gopls not found (go)"),
            c(Group::Formatter, "none", Status::Off, "no formatter commands in [format]"),
            c(
                Group::Clipboard,
                "paste",
                Status::Missing,
                "no wl-paste, xclip, xsel or pbpaste for this session: paste uses jetl's last copy",
            ),
            c(Group::Grammar, "rust", Status::Ok, "compiled"),
            c(Group::Terminal, "kitty keyboard", Status::Unknown, "could not ask the terminal"),
            c(Group::Tool, "git", Status::Ok, "/usr/bin/git"),
        ],
    }
}

/// Open the Settings window on Dependencies with `fixed_report()` shown;
/// the real probe the page starts is answered with a seq nobody awaits.
fn open_dependencies(app: &mut App) {
    std::fs::create_dir_all(&app.paths.config_dir).unwrap();
    std::fs::write(app.paths.config_file(), "[lsp]\nenabled = false\n").unwrap();
    app.perform(Action::App(AppAction::Settings));
    press(app, KeyCode::End, 1);
    let w = app.settings.as_mut().unwrap();
    w.probe_started(u64::MAX);
    w.dependencies_loaded(u64::MAX, fixed_report());
}

#[tokio::test]
async fn settings_window_dependencies() {
    let mut fx = stable_app("settings-window-deps");
    let app = &mut fx.app;
    open_dependencies(app);
    press(app, KeyCode::Enter, 1);
    press(app, KeyCode::Down, 2);
    drain(app);
    let term = draw_app(app, 80, 24);
    let [l, u] = settings_marks(&app.theme);
    let w = Mark::style('W', "missing", app.theme.warn);
    assert_snapshot!("settings_window_dependencies", snapshot(&term, &[l, u, w], None));
}

#[tokio::test]
async fn settings_window_dependencies_not_checked() {
    let mut fx = stable_app("settings-window-deps-not-checked");
    let app = &mut fx.app;
    std::fs::create_dir_all(&app.paths.config_dir).unwrap();
    std::fs::write(app.paths.config_file(), "[lsp]\nenabled = false\n").unwrap();
    app.perform(Action::App(AppAction::Settings));
    press(app, KeyCode::End, 1);
    let w = app.settings.as_mut().unwrap();
    let (_, seq) = w.awaited_probe();
    w.probe_dropped(seq);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("settings_window_dependencies_not_checked", snapshot(&term, &settings_marks(&app.theme), None));
}

#[tokio::test]
async fn settings_window_dependencies_narrow_does_not_panic() {
    let mut fx = stable_app("settings-deps-narrow");
    let app = &mut fx.app;
    open_dependencies(app);
    for w in 26..=60u16 {
        for h in [9u16, 10, 14, 24] {
            let term = draw_app(app, w, h);
            assert_settings_inside(app, &term, w, h);
        }
    }
}

/// Nothing drew over the window's right border and every rect lies inside it.
fn assert_settings_inside(app: &App, term: &Terminal<TestBackend>, w: u16, h: u16) {
    let s = app.settings.as_ref().unwrap();
    let r = s.rect();
    if r.width < 3 || r.height < 3 {
        return;
    }
    let buf = term.backend().buffer();
    for y in r.y + 1..r.bottom() - 1 {
        assert_eq!(buf[(r.right() - 1, y)].symbol(), "│", "right border overdrawn at {w}x{h}, row {y}");
    }
    for b in s.button_rects() {
        assert!(
            b.x >= r.x && b.right() < r.right() && b.y > r.y && b.bottom() < r.bottom(),
            "button {b:?} outside window {r:?} at {w}x{h}"
        );
    }
    let rr = s.rows_rect();
    assert!(rr.width == 0 || rr.right() < r.right(), "rows {rr:?} outside window {r:?} at {w}x{h}");
}

#[tokio::test]
async fn settings_window_narrow_appearance_does_not_panic() {
    let mut fx = stable_app("settings-narrow");
    let app = &mut fx.app;
    app.perform(Action::App(AppAction::Settings));
    for w in 26..=60u16 {
        for h in [9u16, 10, 14, 24] {
            let term = draw_app(app, w, h);
            assert_settings_inside(app, &term, w, h);
            let tree = app.settings.as_ref().unwrap().tree_rect();
            if tree.width == 0 {
                continue;
            }
            let sep = tree.right();
            let buf = term.backend().buffer();
            for y in tree.y..tree.bottom() {
                assert_eq!(buf[(sep, y)].symbol(), "│", "no separator at {w}x{h}, row {y}");
                let line = crate::ui::snapshot::row_text(buf, y, w);
                if let Some(byte) = line.find("restart") {
                    let col = line[..byte].chars().count() as u16;
                    assert!(col > sep, "tag over the tree at {w}x{h}: {line}");
                }
            }
        }
    }
}

#[tokio::test]
async fn settings_window_long_error_fits_short_terminals() {
    let mut fx = stable_app("settings-error-short");
    let app = &mut fx.app;
    std::fs::create_dir_all(&app.paths.config_dir).unwrap();
    std::fs::write(
        app.paths.config_file(),
        "[editor\nscrolloff = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    )
    .unwrap();
    app.perform(Action::App(AppAction::Settings));
    for w in [36u16, 40, 80] {
        for h in 6..=12u16 {
            let term = draw_app(app, w, h);
            assert_settings_inside(app, &term, w, h);
            let buf = term.backend().buffer();
            let text: String = (0..h).map(|y| crate::ui::snapshot::row_text(buf, y, w)).collect::<Vec<_>>().join("\n");
            if h >= 9 {
                assert!(text.contains("Open config"), "no open row at {w}x{h}:\n{text}");
            }
        }
    }
}

/// Hints of the two inlay snapshots: line 0 `let v = vec(1, 2);`, line 1
/// `let n = add(v, 3);`.
fn set_hints(app: &mut App) {
    use crate::editor::inlay::Inlay;
    let hint = |at, label, param| Inlay::new(at, label, false, param, param).unwrap();
    let hints = vec![hint(5, ": Vec", false), hint(12, "a:", true), hint(15, "b:", true), hint(24, ": u8", false), hint(31, "x:", true)];
    app.editor.active_buffer_mut().unwrap().inlays.set(hints, 0, 0..3);
}

#[tokio::test]
async fn editor_inlay_hints() {
    let mut fx = stable_app("inlay-hints");
    let app = &mut fx.app;
    load(app, "hints.log", "let v = vec(1, 2);\nlet n = add(v, 3);\n");
    set_hints(app);
    // The caret on a type hint's anchor is drawn before the hint.
    app.editor.active_buffer_mut().unwrap().view.set_sel(Selection::point(5));
    let term = draw_editor(app, 50, 6);
    let mut marks = vec![Mark::style('H', "inlay", app.theme.inlay_hint)];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_inlay_hints", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_inlay_hints_carets() {
    let mut fx = stable_app("inlay-hints-carets");
    let app = &mut fx.app;
    load(app, "hints.log", "let v = vec(1, 2);\nlet n = add(v, 3);\n");
    set_hints(app);
    // Carets at a type hint's anchor (primary and secondary) stand before the
    // hint, at a parameter hint's anchor after it, at the line end before the
    // line-end hint.
    let b = app.editor.active_buffer_mut().unwrap();
    let mut hints = b.inlays.all().to_vec();
    hints.push(crate::editor::inlay::Inlay::new(18, "-> X", true, false, false).unwrap());
    b.inlays.set(hints, 0, 0..4);
    b.view.sels = crate::editor::selection::Selections::new(
        vec![Selection::point(5), Selection::point(12), Selection::point(18), Selection::point(24)],
        3,
    );
    let term = draw_editor(app, 50, 6);
    // The caret mark comes first, so that a caret on a hint's label shows.
    let mut marks = editor_marks(&app.theme);
    marks.insert(1, Mark::style('H', "inlay", app.theme.inlay_hint));
    assert_snapshot!("editor_inlay_hints_carets", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn editor_inlay_hints_wrapped() {
    let mut fx = stable_app("inlay-hints-wrapped");
    let app = &mut fx.app;
    let third = "see the docs for details on how this works\n";
    load(app, "hints.log", &format!("let v = vec(1, 2);\nlet n = add(v, 3);\n{third}"));
    editor(app, EditorAction::ToggleSoftWrap);
    set_hints(app);
    // Text rows are 16 cells (24 wide, 7 of gutter, one kept free). Line 2's
    // first row is `see the docs for` (16 cells), so `details` starts the
    // second row, and its hint `why:` goes with it to the start of that row.
    let at = 19 + 19 + third.find("details").unwrap();
    let b = app.editor.active_buffer_mut().unwrap();
    let mut hints = b.inlays.all().to_vec();
    hints.push(crate::editor::inlay::Inlay::new(at, "why:", false, true, true).unwrap());
    b.inlays.set(hints, 0, 0..4);
    b.view.set_sel(Selection::point(12));
    let term = draw_editor(app, 24, 10);
    let mut marks = vec![Mark::style('H', "inlay", app.theme.inlay_hint)];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_inlay_hints_wrapped", snapshot(&term, &marks, None));
}

#[cfg(feature = "lang-rust")]
#[tokio::test]
async fn editor_inlay_hints_rust() {
    use std::time::Instant;

    use super::inlay::INLAY_DELAY;
    use super::lsp::tests::{incremental, is_lsp, open_on_server, with_fake_servers};
    use super::tests::handle_next;

    const SRC: &str =
        "fn main() {\n    let v = vec![1, 2];\n    let n = add(v.len(), 3);\n}\nfn add(a: usize, b: usize) -> usize { a + b }\n";
    let mut fx = stable_app("inlay-hints-rust");
    let app = &mut fx.app;
    let mut caps = incremental();
    caps["inlayHintProvider"] = serde_json::json!(true);
    let mut servers = with_fake_servers(app, caps);
    let mut server = open_on_server(app, &mut servers, "a.rs", SRC).await;
    draw_editor(app, 60, 8);
    let t0 = Instant::now();
    app.inlay_tick(t0);
    app.inlay_tick(t0 + INLAY_DELAY);
    let req = server.recv("textDocument/inlayHint").await;
    let hint = |l: u32, c: u32, label: &str, kind: u8, pad: bool| serde_json::json!({"position": {"line": l, "character": c}, "label": label, "kind": kind, "paddingRight": pad});
    let hints = serde_json::json!([
        hint(1, 9, ": Vec<i32>", 1, false),
        hint(2, 9, ": usize", 1, false),
        hint(2, 16, "a:", 2, true),
        hint(2, 25, "b:", 2, true),
    ]);
    server.respond(req["id"].as_i64().unwrap(), hints);
    handle_next(app, is_lsp).await;
    let term = draw_editor(app, 60, 8);
    let mut marks = vec![Mark::style('H', "inlay", app.theme.inlay_hint)];
    marks.extend(editor_marks(&app.theme));
    assert_snapshot!("editor_inlay_hints_rust", snapshot(&term, &marks, None));
}

fn run_marks(theme: &Theme) -> [Mark; 4] {
    [
        Mark::bg('L', "selected", theme.list_selected_unfocused.bg.unwrap()),
        Mark::style('T', "picker", theme.tab_active),
        Mark::style('D', "dim button", theme.dim.bg(theme.panel_bg)),
        Mark::modifier('U', "link", Modifier::UNDERLINED),
    ]
}

/// The Run window after a failed `check` with one linked error.
fn run_fixture(app: &mut App) {
    let root = app.workspace.root.clone();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
    app.run.selected = Some("check".into());
    app.toggle_tool_window(ToolWindowId::Run);
    app.run_panel.start("check", "cargo check", false);
    let mut matcher = crate::run::matcher::Matcher::new(&root, &root);
    let lines = ["error: x", " --> src/a.rs:2:3", "   Compiling jtc v0.1.0", "    Finished"].map(|t| matcher.line(t.to_string()));
    app.run_panel.push(lines.to_vec());
    app.run_panel.finish(crate::run::panel::Status::Exited(101));
    app.run_panel.freeze_elapsed();
}

#[tokio::test]
async fn run_window_toolbar() {
    let mut fx = stable_app("run-toolbar");
    let app = &mut fx.app;
    run_fixture(app);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("run_window_toolbar", snapshot(&term, &run_marks(&app.theme), None));
}

#[tokio::test]
async fn run_window_toolbar_wide() {
    let mut fx = stable_app("run-toolbar-wide");
    let app = &mut fx.app;
    run_fixture(app);
    let term = draw_app(app, 120, 40);
    assert_snapshot!("run_window_toolbar_wide", snapshot(&term, &run_marks(&app.theme), None));
}

fn run_tests_marks(theme: &Theme) -> [Mark; 4] {
    [
        Mark::style('E', "error", theme.error),
        Mark::style('P', "passed", theme.git_added),
        Mark::modifier('U', "link", Modifier::UNDERLINED),
        Mark::bg('L', "selected", theme.list_selected.bg.unwrap()),
    ]
}

/// The Run window after a failed `cargo test` (the libtest fixture: two
/// passed, one failed with its output, one ignored), focused, on `fails`.
fn run_tests_fixture(app: &mut App) {
    let root = app.workspace.root.clone();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn x() {}\n".repeat(14)).unwrap();
    app.toggle_tool_window(ToolWindowId::Run);
    app.run_panel.start("cargo test", "cargo test", true);
    let mut matcher = crate::run::matcher::Matcher::new(&root, &root);
    let lines = include_str!("../run/fixtures/libtest.txt").lines().map(|t| matcher.line(t.to_string())).collect();
    app.run_panel.push(lines);
    app.run_panel.finish(crate::run::panel::Status::Exited(101));
    app.run_panel.freeze_elapsed();
    app.focus.set(crate::ui::focus::PanelId::Run);
    app.run_panel.select_test("fails");
}

#[tokio::test]
async fn run_window_tests() {
    let mut fx = stable_app("run-tests");
    let app = &mut fx.app;
    run_tests_fixture(app);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("run_window_tests", snapshot(&term, &run_tests_marks(&app.theme), None));
}

#[tokio::test]
async fn run_window_tests_wide() {
    let mut fx = stable_app("run-tests-wide");
    let app = &mut fx.app;
    run_tests_fixture(app);
    let term = draw_app(app, 120, 40);
    assert_snapshot!("run_window_tests_wide", snapshot(&term, &run_tests_marks(&app.theme), None));
}

fn run_configs_marks(theme: &Theme) -> [Mark; 3] {
    [
        Mark::bg('L', "selected", theme.list_selected.bg.unwrap()),
        Mark::bg('U', "unfocused", theme.list_selected_unfocused.bg.unwrap()),
        Mark::style('M', "modified", theme.git_modified),
    ]
}

/// The Run Configurations window over one config.toml and one project
/// configuration (`where`, selected, form focused on Arguments), with
/// `Cwd` changed to `web` and not saved.
fn run_configs_fixture(app: &mut App) {
    std::fs::create_dir_all(&app.paths.config_dir).unwrap();
    std::fs::write(
        app.paths.config_file(),
        "[lsp]\nenabled = false\n[[run.configs]]\nname = \"global check\"\ncommand = \"cargo check\"\n",
    )
    .unwrap();
    app.config_changed(vec![crate::config::watch::ConfigFile::Config]);
    app.notifications.clear();
    app.toasts = Default::default();
    let file = app.workspace.root.join(".jetl/run.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "[[configs]]\nname = \"where\"\ncommand = [\"sh\", \"-c\", \"pwd\"]\nargs = [\"x\"]\ncwd = \"src\"\nenv = { FOO = \"bar\" }\n",
    )
    .unwrap();
    app.perform(Action::Run(crate::actions::RunAction::EditConfigs));
    press(app, KeyCode::Down, 1);
    press(app, KeyCode::Enter, 1);
    press(app, KeyCode::Down, 3);
    press(app, KeyCode::Enter, 1);
    let d = app.dialog.take().unwrap();
    app.finish_dialog(d.id, crate::ui::dialog::DialogResult::Text("web".into()));
    press(app, KeyCode::Up, 1);
}

#[tokio::test]
async fn run_configs_window() {
    let mut fx = stable_app("run-configs");
    let app = &mut fx.app;
    run_configs_fixture(app);
    let term = draw_app(app, 80, 24);
    assert_snapshot!("run_configs_window", snapshot(&term, &run_configs_marks(&app.theme), None));
}

#[tokio::test]
async fn run_configs_window_wide() {
    let mut fx = stable_app("run-configs-wide");
    let app = &mut fx.app;
    run_configs_fixture(app);
    let term = draw_app(app, 120, 40);
    assert_snapshot!("run_configs_window_wide", snapshot(&term, &run_configs_marks(&app.theme), None));
}

#[tokio::test]
async fn switcher_popup() {
    let mut fx = stable_app("switcher-popup");
    let app = &mut fx.app;
    load(app, "alpha.txt", "one\n");
    load(app, "beta.txt", "two\n");
    app.perform(Action::Editor(EditorAction::InsertText("x".into())));
    load(app, "gamma.txt", "three\n");
    app.perform(Action::Editor(EditorAction::Switcher));
    let term = draw_app(app, 80, 24);
    let marks = menu_marks(&app.theme);
    assert_snapshot!("switcher_popup", snapshot(&term, &marks, None));
}

/// The four sample tasks, task 1 selected with details (related ids, an
/// attachment, two of three comments) loaded; no yman process runs.
fn tasks_setup(app: &mut App) {
    use crate::yman::panel::tests::{sample, set_snapshot};
    use crate::yman::{Attachment, Comment, LoadedTask, TaskDetails};
    app.yman.program = "jetl-no-such-yman-xyz".into();
    app.toggle_tool_window(ToolWindowId::Tasks);
    set_snapshot(&mut app.yman, sample());
    app.sync_tasks();
    let details = TaskDetails {
        id: "1".into(),
        priority: 2,
        status: "todo".into(),
        title: "Fix the parser".into(),
        tags: vec!["ui".into()],
        assignee: Some("claude".into()),
        links: vec![],
        related: vec!["2".into()],
        related_by: vec!["10".into()],
        created: "2026-10-25T16:06:18Z".into(),
        updated: "2026-10-25T17:30:00Z".into(),
        dir: String::new(),
        body: "First line\n\n## Notes\n- item".into(),
        attachments: vec![Attachment {
            name: "trace.txt".into(),
            added: "2026-10-25T16:10:00Z".into(),
            by: Some("claude".into()),
            missing: false,
        }],
        discussion: vec![
            Comment { ts: "2026-10-25T16:20:00Z".into(), author: Some("claude".into()), text: "Looks like the lexer.".into() },
            Comment { ts: "2026-10-25T16:40:00Z".into(), author: None, text: "Confirmed.\nSecond line.".into() },
        ],
        discussion_total: 3,
    };
    app.yman.task = Some(LoadedTask { id: "1".into(), result: Ok(details) });
    app.yman.revision += 1;
}

#[tokio::test]
async fn tasks_window() {
    let mut fx = stable_app("tasks");
    let app = &mut fx.app;
    tasks_setup(app);
    let term = draw_app(app, 120, 30);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    assert_snapshot!("tasks_window", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn tasks_window_narrow() {
    let mut fx = stable_app("tasks-narrow");
    let app = &mut fx.app;
    tasks_setup(app);
    let term = draw_app(app, 80, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    assert_snapshot!("tasks_window_narrow", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn tasks_new_task_dialog() {
    use crate::actions::TasksAction;
    let mut fx = stable_app("tasks-new");
    let app = &mut fx.app;
    tasks_setup(app);
    app.perform(Action::Tasks(TasksAction::NewTask));
    let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    for c in "Parser crash".chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Char('u')));
    let term = draw_app(app, 80, 24);
    let marks = [Mark::bg('L', "selected", app.theme.list_selected.bg.unwrap())];
    assert_snapshot!("tasks_new_task_dialog", snapshot(&term, &marks, None));
}

#[tokio::test]
async fn tasks_window_unavailable() {
    let mut fx = stable_app("tasks-unavailable");
    let app = &mut fx.app;
    app.yman.program = "jetl-no-such-yman-xyz".into();
    app.toggle_tool_window(ToolWindowId::Tasks);
    app.yman.state = crate::yman::TrackerState::Unavailable(crate::yman::YmanError::NotInitialized);
    app.yman.revision += 1;
    let term = draw_app(app, 80, 14);
    assert_snapshot!("tasks_window_unavailable", snapshot(&term, &[], None));
}

#[tokio::test]
async fn terminal_tabs() {
    let mut fx = stable_app("terminal-tabs");
    let app = &mut fx.app;
    app.terminal.push();
    app.terminal.push();
    app.terminal.activate(1);
    for i in 1..=3 {
        app.terminal.tabs[i - 1].panel.feed(format!("$ echo tab {i}\r\ntab {i}\r\n$ ").as_bytes());
    }
    // A style of its own, so the mark isolates the active tab.
    app.theme.tab_active = ratatui::style::Style::default().fg(ratatui::style::Color::Magenta).add_modifier(Modifier::BOLD);
    let theme = app.theme.clone();
    let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
    term.draw(|f| app.terminal.render(f, f.area(), true, &theme, true)).unwrap();
    let marks = [Mark::style('A', "active tab", app.theme.tab_active)];
    assert_snapshot!("terminal_tabs", snapshot(&term, &marks, None));
}
