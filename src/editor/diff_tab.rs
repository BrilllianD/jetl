//! The diff tab: a file's side-by-side (or unified) diff in a full-size
//! editor tab, opened from the Git window (`git.show_diff`).
//!
//! A diff tab is a pathless, read-only, empty `Buffer` whose `diff` holds a
//! `DiffPane`. Its text never changes, so it is never dirty and is never in
//! the session, autosaved or recovered. Rendering, actions and mouse each hand
//! a diff tab to this module before any text-view code runs.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::Rect;

use super::{Buffer, Editor};
use crate::actions::{EditorAction, Motion};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Level, Mouse};
use crate::git::diff::DiffDoc;
use crate::git::model::{DiffKey, Section};
use crate::git::panel::{DiffOpts, DiffPane, DiffToggle, render_diff, render_diff_header};

/// Columns one sideways wheel step or Left/Right key scrolls.
const HSTEP: usize = 4;

/// What a diff tab shows: the files of the diff, the one on screen, its pane.
#[derive(Debug)]
pub(crate) struct DiffTab {
    pub(crate) files: Vec<DiffKey>,
    pub(crate) at: usize,
    pub(crate) pane: DiffPane,
    /// This tab's layout; starts as the Git window's, the header toggles flip only this tab.
    pub(crate) opts: DiffOpts,
    /// Header toggle buttons drawn last frame.
    hits: Vec<(DiffToggle, Rect)>,
}

impl DiffTab {
    pub(crate) fn new(files: Vec<DiffKey>, at: usize, opts: DiffOpts) -> DiffTab {
        DiffTab { files, at, pane: DiffPane::new(opts.collapse), opts, hits: Vec::new() }
    }

    fn key(&self) -> Option<&DiffKey> {
        self.files.get(self.at)
    }

    /// Show the next (previous) file, without wrapping; false at an end.
    pub(crate) fn step_file(&mut self, forward: bool, cx: &mut Ctx) -> bool {
        let next = if forward { self.at + 1 } else { self.at.wrapping_sub(1) };
        let Some(key) = self.files.get(next).cloned() else { return false };
        self.at = next;
        self.pane.request(key, cx);
        true
    }

    /// The wheel: three rows up or down, four columns sideways.
    pub(crate) fn wheel(&mut self, kind: MouseEventKind) -> bool {
        match kind {
            MouseEventKind::ScrollUp => self.pane.scroll_by(-3),
            MouseEventKind::ScrollDown => self.pane.scroll_by(3),
            MouseEventKind::ScrollLeft => self.pane.hscroll = self.pane.hscroll.saturating_sub(HSTEP),
            MouseEventKind::ScrollRight => self.pane.hscroll += HSTEP,
            _ => return false,
        }
        true
    }

    /// The header row, then the diff under it over the whole `area`.
    pub(crate) fn render(&mut self, buf: &mut Screen, area: Rect, focused: bool, theme: &Theme) {
        if area.height == 0 {
            return;
        }
        let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        render_diff(buf, body, &mut self.pane, theme, self.opts);
        let mut title = self.key().map(header).unwrap_or_default();
        if self.files.len() > 1 {
            title = format!("{}/{}  {title}", self.at + 1, self.files.len());
        }
        let style = if focused { theme.title_focused } else { theme.title };
        let toggles = self.pane.doc.is_some().then_some((self.pane.unified, self.opts));
        self.hits = render_diff_header(buf, (area.x + 1, area.y, area.right()), &title, style, toggles, theme);
    }
}

/// Tab title of a one-file diff: `Diff <file name>`.
pub(crate) fn file_title(key: &DiffKey) -> String {
    let rel = key.rel();
    format!("Diff {}", rel.rsplit('/').next().unwrap_or(rel))
}

/// What the header row says about `key`.
pub(crate) fn header(key: &DiffKey) -> String {
    let label = key.label();
    let short = |s: &str| s[..s.len().min(8)].to_string();
    match key {
        DiffKey::Change { section: Section::Staged, .. } => format!("{label}  (HEAD ↔ staged)"),
        DiffKey::Change { section: Section::Unstaged, .. } => format!("{label}  (staged ↔ working tree)"),
        DiffKey::Change { section: Section::Untracked, .. } => format!("{label}  (new file)"),
        DiffKey::Commit { sha, .. } => format!("{}  {label}", short(sha)),
        DiffKey::Range { base, top, .. } => format!("{}..{}  {label}", base.as_deref().map_or("root".into(), short), short(top)),
        DiffKey::Local { .. } => label,
    }
}

impl Editor {
    /// Open a diff tab over `files`, showing `files[at]`; a tab already open
    /// for the same `files` is activated and moved to `at`. `files` must not
    /// be empty. Returns the buffer index.
    pub fn open_diff_tab(&mut self, title: &str, files: Vec<DiffKey>, at: usize, cx: &mut Ctx) -> usize {
        debug_assert!(!files.is_empty());
        let at = at.min(files.len().saturating_sub(1));
        if let Some(i) = self.buffers.iter().position(|b| b.diff.as_ref().is_some_and(|d| d.files == files)) {
            if let Some(d) = self.buffers[i].diff.as_mut() {
                d.at = at;
                d.pane.request(files[at].clone(), cx);
            }
            self.activate_index(i);
            return i;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut buf = Buffer::from_text(id, None, "", false);
        buf.name = title.to_string();
        buf.read_only = true;
        buf.view.wrap.on = false;
        let mut tab = DiffTab::new(files, at, self.diff_opts);
        let key = tab.files[at].clone();
        tab.pane.request(key, cx);
        buf.diff = Some(Box::new(tab));
        self.buffers.push(buf);
        let i = self.buffers.len() - 1;
        self.activate_index(i);
        i
    }

    /// Feed `result` to every diff tab showing `key`; whether any did.
    pub fn set_diff(&mut self, key: &DiffKey, result: &anyhow::Result<DiffDoc>) -> bool {
        let mut fed = false;
        for t in self.buffers.iter_mut().filter_map(|b| b.diff.as_mut()) {
            if t.pane.key.as_ref() == Some(key) {
                let r = match result {
                    Ok(d) => Ok(d.clone()),
                    Err(e) => Err(anyhow::anyhow!("{e:#}")),
                };
                t.pane.set(key, r);
                fed = true;
            }
        }
        fed
    }

    /// An action on the active diff tab: `None` when it is not one, or when
    /// the action is a tab or group command that works as on any tab.
    pub(super) fn diff_tab_action(&mut self, a: &EditorAction, cx: &mut Ctx) -> Option<bool> {
        use EditorAction as E;
        let i = self.active()?;
        self.buffers[i].diff.as_ref()?;
        if matches!(
            a,
            E::NextTab
                | E::PrevTab
                | E::CloseTab
                | E::CloseAllTabs
                | E::CloseOtherTabs
                | E::CloseUnpinnedTabs
                | E::TogglePinTab
                | E::MoveTabLeft
                | E::MoveTabRight
                | E::SplitRight
                | E::SplitDown
                | E::NextGroup
                | E::PrevGroup
                | E::FocusGroupLeft
                | E::FocusGroupRight
                | E::FocusGroupUp
                | E::FocusGroupDown
                | E::Unsplit
                | E::UnsplitAll
                | E::MoveTabToOppositeGroup
                | E::Switcher
                | E::SwitcherBack
                | E::SaveAll
                | E::NavBack
                | E::NavForward
                | E::ShowBookmarks
                | E::GotoNumberedBookmark(_)
                | E::Merge(_)
        ) {
            return None;
        }
        let t = self.buffers[i].diff.as_mut()?;
        match a {
            E::NextChange | E::PrevChange => {
                let forward = *a == E::NextChange;
                if !t.pane.step_block(forward) {
                    cx.notify(Level::Info, if forward { "No more changes below" } else { "No more changes above" });
                }
            }
            E::DiffNextFile | E::DiffPrevFile => {
                let forward = *a == E::DiffNextFile;
                if !t.step_file(forward, cx) && t.files.len() > 1 {
                    cx.notify(Level::Info, if forward { "Last file in this diff" } else { "First file in this diff" });
                }
            }
            E::Move(Motion::Up, _) => t.pane.scroll_by(-1),
            E::Move(Motion::Down, _) => t.pane.scroll_by(1),
            E::Move(Motion::PageUp, _) => t.pane.scroll_by(-(t.pane.page.max(1) as isize)),
            E::Move(Motion::PageDown, _) => t.pane.scroll_by(t.pane.page.max(1) as isize),
            E::Move(Motion::DocStart, _) => t.pane.scroll = 0,
            E::Move(Motion::DocEnd, _) => t.pane.scroll_by(isize::MAX),
            E::Move(Motion::Left, _) => t.pane.hscroll = t.pane.hscroll.saturating_sub(HSTEP),
            E::Move(Motion::Right, _) => t.pane.hscroll += HSTEP,
            E::ScrollUp(n) => t.pane.scroll_by(-(*n as isize)),
            E::ScrollDown(n) => t.pane.scroll_by(*n as isize),
            // Swallowed: Save cannot turn the tab into an empty file, Find opens no bar that never draws.
            _ => {}
        }
        Some(true)
    }

    /// A mouse event on the active diff tab; `None` when it is not one.
    pub(super) fn diff_tab_mouse(&mut self, m: &Mouse) -> Option<bool> {
        let i = self.active()?;
        let t = self.buffers[i].diff.as_mut()?;
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            let hit = t.hits.iter().find(|(_, r)| r.contains(ratatui::layout::Position::new(m.x, m.y))).map(|(k, _)| *k);
            match hit {
                Some(DiffToggle::Layout) => t.opts.unified = Some(!t.pane.unified),
                Some(DiffToggle::Collapse) => t.opts.collapse = !t.opts.collapse,
                None => {
                    t.pane.click(m.y, false);
                }
            }
            return Some(true);
        }
        t.wheel(m.kind);
        Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::event::Effect;
    use crate::git::GitRequest;
    use crate::workspace::Workspace;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    /// Run `f` with a throwaway `Ctx`; returns the effects it produced.
    fn with_cx(f: impl FnOnce(&mut Ctx)) -> Vec<Effect> {
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(PathBuf::from("/tmp"));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        f(&mut cx);
        cx.effects
    }

    fn key(rel: &str) -> DiffKey {
        DiffKey::Commit { sha: "1a2b3c4d5e6f".into(), rel: rel.into(), old: None }
    }

    fn loads(fx: &[Effect]) -> Vec<DiffKey> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Git(GitRequest::LoadDiff(k)) => Some(k.clone()),
                _ => None,
            })
            .collect()
    }

    fn notes(fx: &[Effect]) -> usize {
        fx.iter().filter(|e| matches!(e, Effect::Notify(..))).count()
    }

    fn doc() -> anyhow::Result<DiffDoc> {
        Ok(DiffDoc::build(b"a\nb\nc\n", b"a\nx\nc\n", 4))
    }

    fn tab(ed: &mut Editor, i: usize) -> &mut DiffTab {
        ed.buffers[i].diff.as_mut().unwrap()
    }

    #[test]
    fn open_requests_the_key_and_reuses_the_tab() {
        let mut ed = Editor::new(&Config::default());
        let files = vec![key("a.rs"), key("b.rs")];
        let mut i = 0;
        let fx = with_cx(|cx| i = ed.open_diff_tab("Diff a.rs", files.clone(), 0, cx));
        assert_eq!(loads(&fx), [key("a.rs")]);
        assert_eq!(fx.len(), 1);
        let b = &ed.buffers[i];
        assert!(b.read_only && b.path.is_none() && b.rope.len_chars() == 0 && !b.dirty());
        assert_eq!(b.name, "Diff a.rs");
        let mut j = usize::MAX;
        let fx = with_cx(|cx| j = ed.open_diff_tab("Diff a.rs", files, 1, cx));
        assert_eq!((j, ed.buffers.len()), (i, 1));
        assert_eq!(loads(&fx), [key("b.rs")]);
        assert_eq!(tab(&mut ed, i).at, 1);
    }

    #[test]
    fn next_file_requests_the_next_key_and_stops_at_the_end() {
        let mut ed = Editor::new(&Config::default());
        with_cx(|cx| {
            ed.open_diff_tab("Diff", vec![key("a.rs"), key("b.rs")], 0, cx);
        });
        let fx = with_cx(|cx| assert!(ed.handle_action(&EditorAction::DiffNextFile, cx)));
        assert_eq!(loads(&fx), [key("b.rs")]);
        let fx = with_cx(|cx| assert!(ed.handle_action(&EditorAction::DiffNextFile, cx)));
        assert!(loads(&fx).is_empty());
        assert_eq!(notes(&fx), 1);
    }

    #[test]
    fn a_stale_result_is_ignored() {
        let mut ed = Editor::new(&Config::default());
        with_cx(|cx| {
            ed.open_diff_tab("Diff", vec![key("a.rs"), key("b.rs")], 0, cx);
            ed.handle_action(&EditorAction::DiffNextFile, cx);
        });
        assert!(!ed.set_diff(&key("a.rs"), &doc()));
        assert!(tab(&mut ed, 0).pane.doc.is_none());
        assert!(ed.set_diff(&key("b.rs"), &doc()));
        assert!(tab(&mut ed, 0).pane.doc.is_some());
    }

    #[test]
    fn editing_and_save_do_nothing() {
        let mut ed = Editor::new(&Config::default());
        let key = key("a.rs");
        with_cx(|cx| {
            ed.open_diff_tab("Diff", vec![key.clone()], 0, cx);
        });
        let blocks = DiffDoc::build(b"a\n1\n2\n3\n4\n5\n6\n7\n8\n9\nb\n", b"x\n1\n2\n3\n4\n5\n6\n7\n8\n9\ny\n", 4);
        assert!(ed.set_diff(&key, &Ok(blocks)));
        let fx = with_cx(|cx| {
            for a in
                [EditorAction::InsertChar('x'), EditorAction::Save, EditorAction::SaveAs, EditorAction::Find, EditorAction::InsertNewline]
            {
                assert!(ed.handle_action(&a, cx));
            }
        });
        assert!(fx.is_empty());
        assert_eq!(ed.buffers[0].rope.len_chars(), 0);
        let before = tab(&mut ed, 0).pane.block;
        with_cx(|cx| {
            ed.handle_action(&EditorAction::NextChange, cx);
        });
        assert_ne!(tab(&mut ed, 0).pane.block, before);
    }

    #[test]
    fn a_header_toggle_flips_only_this_tab() {
        let mut ed = Editor::new(&Config::default());
        let key = key("a.rs");
        with_cx(|cx| {
            ed.open_diff_tab("Diff", vec![key.clone()], 0, cx);
        });
        ed.set_diff(&key, &doc());
        let theme = Theme::default();
        let area = Rect::new(0, 0, 120, 20);
        let mut screen = Screen::empty(area);
        tab(&mut ed, 0).render(&mut screen, area, true, &theme);
        let (_, r) = *tab(&mut ed, 0).hits.iter().find(|(k, _)| *k == DiffToggle::Layout).unwrap();
        let m = Mouse { kind: MouseEventKind::Down(MouseButton::Left), x: r.x, y: r.y, clicks: 1, mods: KeyModifiers::NONE };
        assert_eq!(ed.diff_tab_mouse(&m), Some(true));
        assert_eq!(tab(&mut ed, 0).opts.unified, Some(true));
        assert_eq!(ed.diff_opts, DiffOpts { unified: None, collapse: true });
    }
}
