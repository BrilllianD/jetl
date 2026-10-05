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

use super::App;
use super::tests::{load, test_app};
use crate::actions::{Action, EditorAction, MergeAction, Motion, PaletteKind};
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

#[tokio::test]
async fn app_git_commit_tab() {
    let mut fx = stable_app("git-commit");
    let app = &mut fx.app;
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
    let term = draw_app(app, 120, 30);
    let marks = diff_marks(&app.theme);
    let bottom = app.layout.rects.bottom;
    assert_snapshot!("app_git_commit_tab", snapshot(&term, &marks, Some(bottom)));
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

/// The resolver's fixture (as in `editor/merge.rs`): yours changes `one`,
/// theirs changes `three`, and both change `five`. A plain extension keeps
/// the frame the same with any set of language features.
const BASE: &str = "fn main() {\n    one();\n    two();\n    three();\n    four();\n    five();\n}\n";
const OURS: &str = "fn main() {\n    ONE();\n    two();\n    three();\n    four();\n    five_ours();\n}\n";
const THEIRS: &str = "fn main() {\n    one();\n    two();\n    THREE();\n    four();\n    five_theirs();\n}\n";

fn open_merge(app: &mut App) {
    let path = app.workspace.root.join("main.log");
    let texts = crate::git::merge::MergeTexts { base: BASE.into(), ours: OURS.into(), theirs: THEIRS.into() };
    let theme = app.theme.clone();
    app.editor.open_merge(path, "main.log".into(), texts, ["main".into(), "dev".into()], BASE.into(), &theme);
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
