//! Breakpoints across buffers and the marks of closed files, stored like
//! bookmarks: a line's mark lives in `Buffer::breakpoints` and follows edits;
//! when a file's last tab closes its lines wait in `Editor::closed_breakpoints`
//! and go back into the buffer when the file opens again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::Editor;
use super::buffer::{BreakpointMark, Buffer};
use crate::app::Ctx;
use crate::event::Level;

/// Why a tab takes no breakpoint: the hint to show, or `None`.
pub(super) fn refusal(in_merge: bool, b: &Buffer) -> Option<&'static str> {
    if in_merge {
        Some(super::merge::BUSY)
    } else if b.preview.is_some() {
        Some("A Markdown preview has no source lines: set the breakpoint in the file")
    } else if b.path.is_none() {
        Some("A breakpoint needs a file: this tab has none")
    } else {
        None
    }
}

impl Editor {
    /// Ctrl+F8: toggle a breakpoint on every caret's line, once per line.
    pub(super) fn toggle_breakpoints_at_carets(&mut self, cx: &mut Ctx) {
        let in_merge = self.merge.is_some();
        let Some(b) = self.active_buffer_mut() else { return };
        if let Some(hint) = refusal(in_merge, b) {
            cx.notify(Level::Info, hint);
            return;
        }
        let lines: BTreeSet<usize> = b.view.sels.iter().map(|s| b.rope.char_to_line(s.head)).collect();
        for line in lines {
            b.toggle_breakpoint_at(line);
        }
    }

    /// The breakpoint lines of every file: closed files, with open buffers
    /// that have a path and a mark over them. A file with no mark is absent.
    pub fn all_breakpoints(&self) -> BTreeMap<PathBuf, Vec<usize>> {
        let mut out: BTreeMap<PathBuf, Vec<usize>> =
            self.closed_breakpoints.iter().map(|(p, l)| (p.clone(), l.iter().copied().collect())).collect();
        for b in &self.buffers {
            let Some(path) = &b.path else { continue };
            if b.breakpoints.is_empty() {
                out.remove(path);
            } else {
                out.insert(path.clone(), b.breakpoints.keys().copied().collect());
            }
        }
        out
    }

    /// The marks of the open buffer for `path`; `None` when it is not open.
    pub fn breakpoint_marks_mut(&mut self, path: &Path) -> Option<&mut BTreeMap<usize, BreakpointMark>> {
        self.index_of(path).map(|i| &mut self.buffers[i].breakpoints)
    }

    /// Every mark the adapter knows by `id`, in any open buffer, with its line.
    pub fn breakpoint_marks_by_id_mut(&mut self, id: i64) -> impl Iterator<Item = (usize, &mut BreakpointMark)> {
        self.buffers.iter_mut().flat_map(|b| b.breakpoints.iter_mut()).filter(move |(_, m)| m.id == Some(id)).map(|(l, m)| (*l, m))
    }

    /// Every mark back to its default state, keeping its line.
    pub fn reset_breakpoint_states(&mut self) {
        for b in &mut self.buffers {
            for mark in b.breakpoints.values_mut() {
                *mark = BreakpointMark::default();
            }
        }
    }

    /// Keep the breakpoints of buffer `i`, which is closing.
    pub(super) fn park_breakpoints(&mut self, i: usize) {
        let b = &self.buffers[i];
        let Some(path) = &b.path else { return };
        if b.breakpoints.is_empty() {
            self.closed_breakpoints.remove(path);
        } else {
            self.closed_breakpoints.insert(path.clone(), b.breakpoints.keys().copied().collect());
        }
    }

    /// The lines parked for `path`, taken out of the closed list (the file is opening).
    pub(super) fn unpark_breakpoints(&mut self, path: &Path) -> BTreeSet<usize> {
        self.closed_breakpoints.remove(path).unwrap_or_default()
    }

    /// The breakpoint lines of closed files, by path, lines sorted.
    pub fn closed_breakpoints(&self) -> Vec<(PathBuf, Vec<usize>)> {
        self.closed_breakpoints.iter().map(|(p, l)| (p.clone(), l.iter().copied().collect())).collect()
    }

    /// Replace the breakpoints of closed files (a session loaded, or the
    /// root changed). A file that is open is skipped.
    pub fn restore_closed_breakpoints(&mut self, files: &[(PathBuf, Vec<usize>)]) {
        self.closed_breakpoints.clear();
        for (path, lines) in files {
            if lines.is_empty() || self.index_of(path).is_some() {
                continue;
            }
            self.closed_breakpoints.entry(path.clone()).or_default().extend(lines.iter().copied());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    use super::*;
    use crate::actions::EditorAction;
    use crate::app::tests::{load, test_app};
    use crate::config::Config;
    use crate::config::theme::Theme;
    use crate::editor::buffer::BpState;
    use crate::editor::gutter::{GutterMark, MarkKind};
    use crate::editor::selection::Selection;
    use crate::event::{Effect, Mouse};
    use crate::git::merge::MergeTexts;
    use crate::ui::focus::PanelId;
    use crate::ui::markdown::Rendered;
    use crate::workspace::Workspace;

    fn editor(text: &str) -> Editor {
        let mut ed = Editor::new(&Config::default());
        ed.open_text(PathBuf::from("/tmp/a.log"), text, false, &Theme::default());
        ed
    }

    fn act(ed: &mut Editor, a: EditorAction) -> Vec<Effect> {
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/tmp")));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        ed.handle_action(&a, &mut cx);
        cx.effects
    }

    fn press(ed: &mut Editor, kind: MouseEventKind, x: u16, y: u16, mods: KeyModifiers) -> Vec<Effect> {
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/tmp")));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        ed.handle_mouse(&Mouse { kind, x, y, clicks: 1, mods }, Rect::default(), &mut cx);
        cx.effects
    }

    fn down(ed: &mut Editor, x: u16, y: u16) -> Vec<Effect> {
        press(ed, MouseEventKind::Down(MouseButton::Left), x, y, KeyModifiers::NONE)
    }

    fn draw(ed: &mut Editor, w: u16, h: u16) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let theme = Theme::default();
        term.draw(|f| ed.render(f, Rect::new(0, 0, w, 1), Rect::new(0, 1, w, h - 1), true, &theme)).unwrap();
    }

    fn keys(ed: &Editor) -> Vec<usize> {
        ed.active_buffer().unwrap().breakpoints.keys().copied().collect()
    }

    fn notes(fx: &[Effect]) -> Vec<(Level, String)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Notify(l, t) => Some((*l, t.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn toggle_covers_every_caret_line_once() {
        let mut ed = editor("l0 abc\nl1\nl2\nl3\n");
        let b = ed.active_buffer_mut().unwrap();
        b.view.sels.push(Selection::point(2), false);
        let p = b.rope.line_to_char(2);
        b.view.sels.push(Selection::point(p), false);
        let carets = b.view.sels.len();
        assert_eq!(carets, 3);
        act(&mut ed, EditorAction::ToggleBreakpoint);
        assert_eq!(keys(&ed), vec![0, 2]);
        act(&mut ed, EditorAction::ToggleBreakpoint);
        assert!(keys(&ed).is_empty());
        assert_eq!(ed.active_buffer().unwrap().view.sels.len(), carets);
    }

    #[test]
    fn a_press_on_the_mark_cell_toggles_and_leaves_the_caret() {
        let mut ed = editor("l0\nl1\nl2\nl3\n");
        draw(&mut ed, 40, 8);
        let t = ed.text_area();
        down(&mut ed, t.x - 4, t.y + 2);
        assert_eq!(keys(&ed), vec![2]);
        let b = ed.active_buffer().unwrap();
        assert_eq!(b.cursor_line_col(), (0, 0));
        assert!(b.view.sels.iter().all(|s| s.is_empty()));
        press(&mut ed, MouseEventKind::Drag(MouseButton::Left), t.x + 3, t.y + 3, KeyModifiers::NONE);
        assert!(ed.active_buffer().unwrap().view.sels.iter().all(|s| s.is_empty()), "no drag selects");
        down(&mut ed, t.x - 4, t.y + 2);
        assert!(keys(&ed).is_empty());
    }

    #[test]
    fn a_press_elsewhere_in_the_gutter_behaves_as_before() {
        let mut ed = editor("head\n  one\n  two\nlast\n");
        draw(&mut ed, 40, 8);
        let t = ed.text_area();
        let caret = |ed: &Editor| ed.active_buffer().unwrap().cursor_line_col().0;
        // Number cell.
        down(&mut ed, t.x - 5, t.y + 3);
        assert_eq!((caret(&ed), keys(&ed)), (3, vec![]));
        // Git cell without a git mark.
        down(&mut ed, t.x - 3, t.y + 2);
        assert_eq!((caret(&ed), keys(&ed)), (2, vec![]));
        // Git cell with one: its menu.
        ed.active_buffer_mut().unwrap().gutter = vec![GutterMark { line: 1, kind: MarkKind::Modified }];
        draw(&mut ed, 40, 8);
        let fx = down(&mut ed, t.x - 3, t.y + 1);
        assert!(fx.iter().any(|e| matches!(e, Effect::ContextMenu(_))));
        assert!(keys(&ed).is_empty());
        // Fold cell.
        draw(&mut ed, 40, 8);
        down(&mut ed, t.x - 2, t.y);
        assert!(ed.active_buffer().unwrap().view.folds.header_at(0).is_some());
        assert!(keys(&ed).is_empty());
        // Space cell.
        down(&mut ed, t.x - 1, t.y + 3);
        assert!(keys(&ed).is_empty());
        // With a modifier the press is the plain one.
        let mut ed = editor("head\n  one\n  two\nlast\n");
        draw(&mut ed, 40, 8);
        press(&mut ed, MouseEventKind::Down(MouseButton::Left), t.x - 4, t.y + 3, KeyModifiers::SHIFT);
        let b = ed.active_buffer().unwrap();
        assert!(b.view.sels.iter().any(|s| !s.is_empty()), "shift extends the selection");
        assert!(keys(&ed).is_empty());
        for mods in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
            press(&mut ed, MouseEventKind::Down(MouseButton::Left), t.x - 4, t.y + 2, mods);
            assert!(keys(&ed).is_empty(), "{mods:?}");
        }
    }

    #[test]
    fn a_press_below_the_text_or_on_a_wrapped_row_does_not_toggle() {
        let long = "word ".repeat(30);
        let mut ed = editor(&long);
        ed.active_buffer_mut().unwrap().view.wrap.on = true;
        draw(&mut ed, 30, 8);
        let t = ed.text_area();
        down(&mut ed, t.x - 4, t.y + 1);
        assert!(keys(&ed).is_empty(), "a continuation row");
        down(&mut ed, t.x - 4, t.y);
        assert_eq!(keys(&ed), vec![0]);
        let mut ed = editor("a\nb\n");
        draw(&mut ed, 30, 8);
        let t = ed.text_area();
        down(&mut ed, t.x - 4, t.y + 5);
        assert!(keys(&ed).is_empty(), "below the last line");
    }

    #[test]
    fn tabs_without_a_file_refuse_with_a_hint() {
        let theme = Theme::default();
        for which in 0..3 {
            let mut ed = editor("src\n");
            match which {
                0 => drop(ed.open_untitled("x")),
                1 => drop(ed.open_patch("p", Vec::new(), "x\n")),
                _ => {
                    let source = ed.active_buffer().unwrap().id;
                    ed.open_markdown_preview(source, Rendered { text: "a\nb".into(), spans: Vec::new() }, &theme);
                }
            }
            let hint = refusal(false, ed.active_buffer().unwrap()).expect("refuses");
            let fx = act(&mut ed, EditorAction::ToggleBreakpoint);
            assert_eq!(notes(&fx), vec![(Level::Info, hint.to_string())], "key, tab {which}");
            draw(&mut ed, 40, 8);
            let t = ed.text_area();
            let fx = down(&mut ed, t.x - 4, t.y);
            assert_eq!(notes(&fx), vec![(Level::Info, hint.to_string())], "press, tab {which}");
            let b = ed.active_buffer().unwrap();
            assert!(b.breakpoints.is_empty());
            assert_eq!(b.cursor_line_col(), (0, 0));
        }
    }

    #[test]
    fn the_conflict_resolver_refuses_with_a_hint() {
        let mut ed = Editor::new(&Config::default());
        let texts = MergeTexts { base: "a\n".into(), ours: "b\n".into(), theirs: "c\n".into() };
        ed.open_merge(
            PathBuf::from("/tmp/main.rs"),
            "main.rs".into(),
            texts,
            ["main".into(), "dev".into()],
            "a\n".into(),
            &Theme::default(),
        );
        draw(&mut ed, 100, 12);
        let fx = act(&mut ed, EditorAction::ToggleBreakpoint);
        assert_eq!(notes(&fx), vec![(Level::Info, super::super::merge::BUSY.to_string())]);
        let t = ed.text_area();
        let fx = down(&mut ed, t.x - 4, t.y);
        assert_eq!(notes(&fx), vec![(Level::Info, super::super::merge::BUSY.to_string())]);
        assert!(ed.all_breakpoints().is_empty());
    }

    #[test]
    fn closed_files_keep_their_breakpoints() {
        let theme = Theme::default();
        let mut ed = Editor::new(&Config::default());
        let a = PathBuf::from("/tmp/a.log");
        ed.open_text(a.clone(), "a0\na1\na2\na3\n", false, &theme);
        ed.active_buffer_mut().unwrap().set_breakpoints(&[1, 3]);
        ed.force_close(ed.index_of(&a).unwrap());
        assert_eq!(ed.closed_breakpoints(), vec![(a.clone(), vec![1, 3])]);
        // Reopened shorter: lines past the end land on the last one.
        ed.open_text(a.clone(), "a0\na1\na2", false, &theme);
        let b = ed.active_buffer().unwrap();
        assert_eq!(b.breakpoints.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert!(b.breakpoints.values().all(|m| *m == BreakpointMark::default()));
        assert!(ed.closed_breakpoints().is_empty());
        // Two marks that land on one line merge.
        ed.force_close(ed.index_of(&a).unwrap());
        ed.restore_closed_breakpoints(&[(a.clone(), vec![2, 9])]);
        ed.open_text(a.clone(), "a0\na1\na2", false, &theme);
        assert_eq!(keys(&ed), vec![2]);
        // Close All parks them too; a rename follows; a delete drops them.
        ed.force_close_all();
        assert_eq!(ed.closed_breakpoints(), vec![(a.clone(), vec![2])]);
        let moved = PathBuf::from("/tmp/b.log");
        ed.retarget(&a, &moved, &theme);
        assert_eq!(ed.closed_breakpoints(), vec![(moved.clone(), vec![2])]);
        ed.close_under(&moved);
        assert!(ed.closed_breakpoints().is_empty());
    }

    #[test]
    fn an_open_buffer_with_no_marks_wins_over_lines_parked_for_its_path() {
        let parked = PathBuf::from("/tmp/parked.log");
        let mut ed = editor("one\ntwo\n");
        ed.restore_closed_breakpoints(&[(parked.clone(), vec![1])]);
        // Save As onto a path whose old file was closed with a breakpoint.
        ed.buffers[0].path = Some(parked.clone());
        assert!(ed.all_breakpoints().is_empty());
        ed.park_breakpoints(0);
        assert!(ed.closed_breakpoints().is_empty());
    }

    #[test]
    fn all_breakpoints_lists_open_and_closed_files() {
        let theme = Theme::default();
        let mut ed = Editor::new(&Config::default());
        let (a, c) = (PathBuf::from("/tmp/a.log"), PathBuf::from("/tmp/c.log"));
        ed.open_text(c.clone(), "c0\nc1\nc2\n", false, &theme);
        ed.active_buffer_mut().unwrap().set_breakpoints(&[2, 0]);
        ed.force_close(ed.index_of(&c).unwrap());
        ed.open_text(a.clone(), "a0\na1\na2\n", false, &theme);
        ed.active_buffer_mut().unwrap().set_breakpoints(&[1]);
        ed.open_untitled("u");
        ed.active_buffer_mut().unwrap().breakpoints.insert(0, BreakpointMark::default());
        assert_eq!(ed.all_breakpoints(), [(a.clone(), vec![1]), (c.clone(), vec![0, 2])].into_iter().collect());
        let i = ed.index_of(&a).unwrap();
        ed.buffers[i].toggle_breakpoint_at(1);
        assert_eq!(ed.all_breakpoints(), [(c, vec![0, 2])].into_iter().collect());
    }

    #[test]
    fn marks_mut_and_reset_keep_lines() {
        let theme = Theme::default();
        let mut ed = Editor::new(&Config::default());
        let (a, gone) = (PathBuf::from("/tmp/a.log"), PathBuf::from("/tmp/gone.log"));
        ed.open_text(a.clone(), "a0\na1\na2\n", false, &theme);
        ed.active_buffer_mut().unwrap().set_breakpoints(&[0, 2]);
        assert!(ed.breakpoint_marks_mut(&gone).is_none());
        let marks = ed.breakpoint_marks_mut(&a).unwrap();
        let m = marks.get_mut(&2).unwrap();
        m.state = BpState::Verified;
        m.id = Some(4);
        m.message = Some("hi".into());
        assert_eq!(ed.active_buffer().unwrap().breakpoints[&2].id, Some(4));
        ed.reset_breakpoint_states();
        let b = ed.active_buffer().unwrap();
        assert_eq!(b.breakpoints.keys().copied().collect::<Vec<_>>(), vec![0, 2]);
        assert!(b.breakpoints.values().all(|m| *m == BreakpointMark::default()));
    }

    #[test]
    fn marks_by_id_cover_every_buffer() {
        let theme = Theme::default();
        let mut ed = Editor::new(&Config::default());
        let (a, b) = (PathBuf::from("/tmp/a.log"), PathBuf::from("/tmp/b.log"));
        ed.open_text(a.clone(), "a0\na1\na2\n", false, &theme);
        ed.active_buffer_mut().unwrap().set_breakpoints(&[0, 2]);
        ed.open_text(b.clone(), "b0\nb1\nb2\n", false, &theme);
        ed.active_buffer_mut().unwrap().set_breakpoints(&[1]);
        for (path, line, id) in [(&a, 0, 7), (&a, 2, 8), (&b, 1, 7)] {
            ed.breakpoint_marks_mut(path).unwrap().get_mut(&line).unwrap().id = Some(id);
        }
        let mut seven: Vec<usize> = ed.breakpoint_marks_by_id_mut(7).map(|(l, _)| l).collect();
        seven.sort();
        assert_eq!(seven, vec![0, 1]);
        assert_eq!(ed.breakpoint_marks_by_id_mut(8).map(|(l, _)| l).collect::<Vec<_>>(), vec![2]);
        assert_eq!(ed.breakpoint_marks_by_id_mut(9).count(), 0);
    }

    #[tokio::test]
    async fn ctrl_f8_toggles_in_the_editor_only() {
        let (mut app, dir) = test_app("bp-ctrl-f8");
        load(&mut app, "a.log", "one\ntwo\n");
        app.focus.set(PanelId::Editor);
        let marks = |app: &crate::app::App| app.editor.active_buffer().unwrap().breakpoints.keys().copied().collect::<Vec<_>>();
        let f8 = || KeyEvent::new(KeyCode::F(8), KeyModifiers::CONTROL);
        app.handle_key(f8());
        assert_eq!(marks(&app), vec![0]);
        app.keyboard_enhanced = true;
        app.handle_key(f8());
        assert!(marks(&app).is_empty());
        app.focus.set(PanelId::Project);
        app.handle_key(f8());
        assert!(marks(&app).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
