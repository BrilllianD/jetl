//! The Terminal window's tabs: one `TerminalPanel` and shell each, the active
//! one drawn under a tab row with the labels on the left and New / Close
//! buttons on the right.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::panel::TerminalPanel;
use crate::actions::{Action, AppAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Mouse};

pub struct TerminalTab {
    /// 1 for the first tab; shown as `Local`, else `Local (n)`.
    pub(crate) number: u32,
    pub(crate) panel: TerminalPanel,
}

impl TerminalTab {
    fn new(number: u32, scrollback: usize) -> Self {
        Self { number, panel: TerminalPanel::new(scrollback) }
    }
}

impl std::fmt::Debug for TerminalTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalTab").field("number", &self.number).field("panel", &self.panel).finish()
    }
}

/// What a cell of the tab row is: a label, or a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowHit {
    Tab(usize),
    New,
    Close,
}

/// The tab row shows whenever the window has room for it, at any tab count.
fn has_row(area: Rect) -> bool {
    area.height >= 3
}

#[derive(Debug)]
pub struct TerminalTabs {
    /// Never empty.
    pub(crate) tabs: Vec<TerminalTab>,
    pub(crate) active: usize,
    /// Shells started so far in any tab; the newest one's session id. Never reset.
    pub(crate) seq: u64,
    scrollback: usize,
    /// The tab row at the last frame (zero height without one) and each label's and button's rect.
    row: Rect,
    hits: Vec<(RowHit, Rect)>,
}

impl TerminalTabs {
    pub fn new(scrollback: usize) -> Self {
        Self { tabs: vec![TerminalTab::new(1, scrollback)], active: 0, seq: 0, scrollback, row: Rect::default(), hits: Vec::new() }
    }

    pub fn active(&self) -> &TerminalPanel {
        &self.tabs[self.active].panel
    }

    pub fn active_mut(&mut self) -> &mut TerminalPanel {
        &mut self.tabs[self.active].panel
    }

    // Never empty, so there is no `is_empty`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// `Local` or `Local (n)`.
    pub fn label(&self, i: usize) -> String {
        match self.tabs[i].number {
            1 => "Local".to_string(),
            n => format!("Local ({n})"),
        }
    }

    /// A new idle tab at the end, made active (its shell starts on the next
    /// frame), numbered with the lowest number no open tab holds.
    pub fn push(&mut self) {
        let number = (1u32..).find(|n| self.tabs.iter().all(|t| t.number != *n)).unwrap_or(1);
        self.tabs.push(TerminalTab::new(number, self.scrollback));
        self.activate(self.tabs.len() - 1);
    }

    /// Make tab `i` active; flushes the held-back Esc of the one it leaves.
    pub fn activate(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.active {
            self.active_mut().flush_esc();
            self.active = i;
        }
    }

    /// Next (`forward`) or previous tab, wrapping; no-op with one tab.
    pub fn step(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n > 1 {
            self.activate(if forward { (self.active + 1) % n } else { (self.active + n - 1) % n });
        }
    }

    /// Remove the active tab (dropping its session kills the shell) and
    /// activate its left neighbour. With one tab it does what `reset` does.
    pub fn close_active(&mut self) {
        if self.tabs.len() <= 1 {
            self.reset();
            return;
        }
        self.active_mut().pending_esc = None;
        self.tabs.remove(self.active);
        self.active = self.active.saturating_sub(1);
    }

    /// End every shell: one fresh idle tab, numbers start over (quit, root
    /// switch, closing the only tab). `seq` stays, so a late wake of a killed
    /// shell is never taken for a new one.
    pub fn reset(&mut self) {
        self.tabs = vec![TerminalTab::new(1, self.scrollback)];
        self.active = 0;
    }

    /// `TerminalPanel::restart` on the active tab.
    pub fn restart(&mut self) {
        self.active_mut().restart();
    }

    /// Every tab and every later one.
    pub fn set_scrollback(&mut self, n: usize) {
        self.scrollback = n;
        for t in &mut self.tabs {
            t.panel.set_scrollback(n);
        }
    }

    /// The tab whose running shell has session id `id`.
    pub fn find_mut(&mut self, id: u64) -> Option<&mut TerminalPanel> {
        self.tabs.iter_mut().map(|t| &mut t.panel).find(|p| p.is_current(id))
    }

    /// The shell's text area inside `area`, the window's rect.
    pub fn text_area(&self, area: Rect) -> Rect {
        TerminalPanel::text_area(area, has_row(area))
    }

    /// Mouse on the tab row: consumed; a left press on a label activates it, a
    /// middle press closes it, a left press on New or Close performs that
    /// action. Elsewhere: the active panel's.
    pub fn handle_mouse(&mut self, m: &Mouse, cx: &mut Ctx) -> bool {
        let pos = Position::new(m.x, m.y);
        if self.row.height > 0 && self.row.contains(pos) {
            let hit = self.hits.iter().find(|(_, r)| r.contains(pos)).map(|(h, _)| *h);
            let close = || Effect::Perform(Action::App(AppAction::CloseTerminalTab));
            match (m.kind, hit) {
                (MouseEventKind::Down(MouseButton::Left), Some(RowHit::Tab(i))) => self.activate(i),
                (MouseEventKind::Down(MouseButton::Middle), Some(RowHit::Tab(i))) => {
                    self.activate(i);
                    cx.effects.push(close());
                }
                (MouseEventKind::Down(MouseButton::Left), Some(RowHit::New)) => {
                    cx.effects.push(Effect::Perform(Action::App(AppAction::NewTerminalTab)))
                }
                (MouseEventKind::Down(MouseButton::Left), Some(RowHit::Close)) => cx.effects.push(close()),
                _ => {}
            }
            return true;
        }
        self.active_mut().handle_mouse(m)
    }

    /// The rect of `h` in the last frame's hits.
    #[cfg(test)]
    pub(crate) fn hit_rect(&self, h: RowHit) -> Option<Rect> {
        self.hits.iter().find(|(x, _)| *x == h).map(|(_, r)| *r)
    }

    /// Draw the active panel (below the tab row), the tab row, and resize every tab to the text area.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, truecolor: bool) {
        let tab_row = has_row(area);
        self.active_mut().render(frame, area, tab_row, focused, theme, truecolor);
        let text = TerminalPanel::text_area(area, tab_row);
        for t in &mut self.tabs {
            t.panel.resize(text.height, text.width);
        }
        self.hits.clear();
        if !tab_row {
            self.row = Rect::default();
            return;
        }
        self.row = Rect::new(text.x, text.y - 1, text.width, 1);
        let row = self.row;
        let buf = frame.buffer_mut();
        buf.set_style(row, Style::default().bg(theme.panel_bg));
        let mut x = row.x;
        for i in 0..self.tabs.len() {
            let style = if i == self.active { theme.tab_active } else { theme.tab };
            let rect = put(buf, &mut x, row.y, row.right(), &format!(" {} ", self.label(i)), style);
            if rect.width > 0 {
                self.hits.push((RowHit::Tab(i), rect));
            }
            x += 1;
            if x >= row.right() {
                break;
            }
        }
        // As many buttons as fit right of the labels, Close dropped first.
        let all = [(RowHit::New, " New "), (RowHit::Close, " Close ")];
        let width = |b: &[(RowHit, &str)]| -> u16 { b.iter().map(|(_, l)| l.width() as u16 + 1).sum() };
        let mut buttons: &[(RowHit, &str)] = &all;
        while !buttons.is_empty() && row.right() <= x + width(buttons) {
            buttons = &buttons[..buttons.len() - 1];
        }
        let mut bx = row.right() - width(buttons);
        for &(h, label) in buttons {
            let rect = put(buf, &mut bx, row.y, row.right(), label, theme.title.bg(theme.panel_bg));
            self.hits.push((h, rect));
            bx += 1;
        }
    }
}

/// Write `text` at `*x` clipped to `right`, advance `*x`, return its rect.
fn put(buf: &mut Buffer, x: &mut u16, y: u16, right: u16, text: &str, style: Style) -> Rect {
    let w = (text.width() as u16).min(right.saturating_sub(*x));
    if w > 0 {
        buf.set_stringn(*x, y, text, w as usize, style);
    }
    let r = Rect::new(*x, y, w, 1);
    *x += w;
    r
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::Config;
    use crate::workspace::Workspace;
    use std::path::PathBuf;

    fn labels(t: &TerminalTabs) -> Vec<String> {
        (0..t.len()).map(|i| t.label(i)).collect()
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Mouse {
        Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE }
    }

    #[test]
    fn new_tab_takes_the_lowest_free_number() {
        let mut t = TerminalTabs::new(0);
        t.push();
        t.push();
        assert_eq!(labels(&t), ["Local", "Local (2)", "Local (3)"]);
        assert_eq!(t.active, 2);
        t.activate(1);
        t.close_active();
        assert_eq!((t.len(), t.active), (2, 0));
        assert_eq!(labels(&t), ["Local", "Local (3)"]);
        t.push();
        assert_eq!(labels(&t), ["Local", "Local (3)", "Local (2)"]);
        assert_eq!(t.active, 2);
        t.activate(0);
        t.close_active();
        assert_eq!(t.active, 0, "the first tab has no left neighbour");
        assert_eq!(labels(&t), ["Local (3)", "Local (2)"]);
        t.push();
        assert_eq!(labels(&t), ["Local (3)", "Local (2)", "Local"]);
        t.reset();
        assert_eq!(labels(&t), ["Local"]);
        t.push();
        assert_eq!(t.label(1), "Local (2)");
    }

    #[test]
    fn step_wraps() {
        let mut t = TerminalTabs::new(0);
        t.step(true);
        assert_eq!(t.active, 0);
        t.push();
        t.push();
        t.step(true);
        assert_eq!(t.active, 0);
        t.step(false);
        assert_eq!(t.active, 2);
    }

    /// Run `f` with a throwaway `Ctx`; returns the effects it pushed.
    fn with_cx(f: impl FnOnce(&mut Ctx)) -> Vec<Effect> {
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(PathBuf::from("/tmp"));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        f(&mut cx);
        cx.effects
    }

    fn is_new(e: &Effect) -> bool {
        matches!(e, Effect::Perform(Action::App(AppAction::NewTerminalTab)))
    }

    fn is_close(e: &Effect) -> bool {
        matches!(e, Effect::Perform(Action::App(AppAction::CloseTerminalTab)))
    }

    fn draw(t: &mut TerminalTabs, w: u16) -> Terminal<TestBackend> {
        let theme = Theme::darcula();
        let mut term = Terminal::new(TestBackend::new(w, 8)).unwrap();
        term.draw(|f| t.render(f, f.area(), true, &theme, true)).unwrap();
        term
    }

    fn drawn_two() -> (TerminalTabs, Terminal<TestBackend>, Theme) {
        let theme = Theme::darcula();
        let mut t = TerminalTabs::new(0);
        let mut term = Terminal::new(TestBackend::new(40, 8)).unwrap();
        term.draw(|f| t.render(f, f.area(), true, &theme, true)).unwrap();
        assert_eq!(t.text_area(Rect::new(0, 0, 40, 8)), Rect::new(1, 2, 38, 5));
        assert_eq!(t.active().size(), (5, 38));
        t.push();
        term.draw(|f| t.render(f, f.area(), true, &theme, true)).unwrap();
        (t, term, theme)
    }

    #[test]
    fn tab_row_shows_labels_and_buttons() {
        let (t, term, theme) = drawn_two();
        assert_eq!((t.tabs[0].panel.size(), t.tabs[1].panel.size()), ((5, 38), (5, 38)));
        let buf = term.backend().buffer();
        let row: String = (1..20).map(|x| buf[(x, 1)].symbol()).collect();
        assert_eq!(row, " Local   Local (2) ");
        assert_eq!(buf[(10, 1)].fg, theme.tab_active.fg.unwrap());
        assert_eq!(buf[(2, 1)].fg, theme.tab.fg.unwrap());
        let buttons: String = (25..39).map(|x| buf[(x, 1)].symbol()).collect();
        assert_eq!(buttons, " New   Close  ");
        // The blanks 30 and 38 only carry the row's background.
        for x in (25..30).chain(31..38) {
            assert_eq!(buf[(x, 1)].fg, theme.title.fg.unwrap());
            assert_eq!(buf[(x, 1)].bg, theme.panel_bg);
        }
    }

    #[test]
    fn one_tab_has_the_row_too() {
        let theme = Theme::darcula();
        let mut t = TerminalTabs::new(0);
        let term = draw(&mut t, 40);
        let buf = term.backend().buffer();
        let row: String = (1..8).map(|x| buf[(x, 1)].symbol()).collect();
        assert_eq!(row, " Local ");
        assert_eq!(buf[(3, 1)].fg, theme.tab_active.fg.unwrap());
        assert_eq!(t.hit_rect(RowHit::New), Some(Rect::new(25, 1, 5, 1)));
        assert_eq!(t.hit_rect(RowHit::Close), Some(Rect::new(31, 1, 7, 1)));

        let mut short = TerminalTabs::new(0);
        let mut term = Terminal::new(TestBackend::new(40, 2)).unwrap();
        term.draw(|f| short.render(f, f.area(), true, &theme, true)).unwrap();
        assert_eq!(short.hit_rect(RowHit::New), None);
        assert_eq!(short.text_area(Rect::new(0, 0, 40, 2)).height, 0);
    }

    #[test]
    fn buttons_drop_close_first_and_never_cut_labels() {
        let mut t = TerminalTabs::new(0);
        t.push();
        t.push();
        draw(&mut t, 49);
        assert!(t.hit_rect(RowHit::New).is_some() && t.hit_rect(RowHit::Close).is_some());
        draw(&mut t, 48);
        assert_eq!(t.hit_rect(RowHit::Close), None);
        assert_eq!(t.hit_rect(RowHit::New), Some(Rect::new(41, 1, 5, 1)));
        draw(&mut t, 40);
        assert_eq!((t.hit_rect(RowHit::New), t.hit_rect(RowHit::Close)), (None, None));
        assert_eq!(t.hit_rect(RowHit::Tab(2)).unwrap().width, 11);
    }

    #[test]
    fn click_on_the_tab_row_switches_and_is_consumed() {
        let (mut t, _term, _) = drawn_two();
        assert_eq!(t.active, 1);
        let fx = with_cx(|cx| {
            assert!(t.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 3, 1), cx));
            assert_eq!(t.active, 0);
            assert!(t.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 22, 1), cx));
            assert_eq!(t.active, 0);
            assert!(t.handle_mouse(&mouse(MouseEventKind::ScrollUp, 5, 3), cx));
            assert_eq!(t.active, 0);
        });
        assert!(fx.is_empty());
    }

    #[test]
    fn buttons_and_middle_press_perform_tab_actions() {
        let (mut t, _term, _) = drawn_two();
        let center = |r: Rect| (r.x + 1, r.y);
        let (nx, ny) = center(t.hit_rect(RowHit::New).unwrap());
        let (cx_, cy) = center(t.hit_rect(RowHit::Close).unwrap());
        let (lx, ly) = center(t.hit_rect(RowHit::Tab(0)).unwrap());
        let down = |b| MouseEventKind::Down(b);

        let fx = with_cx(|cx| assert!(t.handle_mouse(&mouse(down(MouseButton::Left), nx, ny), cx)));
        assert!(matches!(fx.as_slice(), [e] if is_new(e)));
        assert_eq!(t.len(), 2);

        let fx = with_cx(|cx| assert!(t.handle_mouse(&mouse(down(MouseButton::Left), cx_, cy), cx)));
        assert!(matches!(fx.as_slice(), [e] if is_close(e)));

        assert_eq!(t.active, 1);
        let fx = with_cx(|cx| assert!(t.handle_mouse(&mouse(down(MouseButton::Middle), lx, ly), cx)));
        assert_eq!(t.active, 0);
        assert!(matches!(fx.as_slice(), [e] if is_close(e)));

        let fx = with_cx(|cx| {
            t.handle_mouse(&mouse(down(MouseButton::Right), lx, ly), cx);
            t.handle_mouse(&mouse(down(MouseButton::Middle), nx, ny), cx);
        });
        assert!(fx.is_empty());
    }
}
