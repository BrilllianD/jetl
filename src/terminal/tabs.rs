//! The Terminal window's tabs: one `TerminalPanel` and shell each, the active
//! one drawn; with two or more a tab row under the top border.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::panel::TerminalPanel;
use crate::config::theme::Theme;
use crate::event::Mouse;

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

#[derive(Debug)]
pub struct TerminalTabs {
    /// Never empty.
    pub(crate) tabs: Vec<TerminalTab>,
    pub(crate) active: usize,
    /// Number of the next new tab.
    next_number: u32,
    /// Shells started so far in any tab; the newest one's session id. Never reset.
    pub(crate) seq: u64,
    scrollback: usize,
    /// The tab row at the last frame (zero height without one) and each label's rect.
    row: Rect,
    hits: Vec<(usize, Rect)>,
}

impl TerminalTabs {
    pub fn new(scrollback: usize) -> Self {
        Self {
            tabs: vec![TerminalTab::new(1, scrollback)],
            active: 0,
            next_number: 2,
            seq: 0,
            scrollback,
            row: Rect::default(),
            hits: Vec::new(),
        }
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

    /// A new idle tab at the end, made active (its shell starts on the next frame).
    pub fn push(&mut self) {
        self.tabs.push(TerminalTab::new(self.next_number, self.scrollback));
        self.next_number += 1;
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
        self.next_number = 2;
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
        TerminalPanel::text_area(area, self.len() > 1)
    }

    /// Mouse on the tab row: consumed, a left press on a label activates it.
    /// Elsewhere: the active panel's.
    pub fn handle_mouse(&mut self, m: &Mouse) -> bool {
        let pos = Position::new(m.x, m.y);
        if self.row.height > 0 && self.row.contains(pos) {
            if m.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(i) = self.hits.iter().find(|(_, r)| r.contains(pos)).map(|(i, _)| *i)
            {
                self.activate(i);
            }
            return true;
        }
        self.active_mut().handle_mouse(m)
    }

    /// Draw the active panel (leaving the tab row when there are two or
    /// more tabs), the tab row, and resize every tab to the text area.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme, truecolor: bool) {
        let tab_row = self.len() > 1;
        self.active_mut().render(frame, area, tab_row, focused, theme, truecolor);
        let text = TerminalPanel::text_area(area, tab_row);
        for t in &mut self.tabs {
            t.panel.resize(text.height, text.width);
        }
        self.hits.clear();
        if !(tab_row && area.height >= 3) {
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
                self.hits.push((i, rect));
            }
            x += 1;
            if x >= row.right() {
                break;
            }
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

    fn labels(t: &TerminalTabs) -> Vec<String> {
        (0..t.len()).map(|i| t.label(i)).collect()
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Mouse {
        Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE }
    }

    #[test]
    fn labels_number_up_and_are_not_reused() {
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
        assert_eq!(t.label(2), "Local (4)");
        t.activate(0);
        t.close_active();
        assert_eq!(t.active, 0, "the first tab has no left neighbour");
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

    fn drawn_two() -> (TerminalTabs, Terminal<TestBackend>, Theme) {
        let theme = Theme::darcula();
        let mut t = TerminalTabs::new(0);
        let mut term = Terminal::new(TestBackend::new(40, 8)).unwrap();
        term.draw(|f| t.render(f, f.area(), true, &theme, true)).unwrap();
        assert_eq!(t.text_area(Rect::new(0, 0, 40, 8)), Rect::new(1, 1, 38, 6));
        assert_eq!(t.active().size(), (6, 38));
        t.push();
        term.draw(|f| t.render(f, f.area(), true, &theme, true)).unwrap();
        (t, term, theme)
    }

    #[test]
    fn one_tab_draws_no_tab_row_and_two_do() {
        let (t, term, theme) = drawn_two();
        assert_eq!((t.tabs[0].panel.size(), t.tabs[1].panel.size()), ((5, 38), (5, 38)));
        let buf = term.backend().buffer();
        let row: String = (1..20).map(|x| buf[(x, 1)].symbol()).collect();
        assert_eq!(row, " Local   Local (2) ");
        assert_eq!(buf[(10, 1)].fg, theme.tab_active.fg.unwrap());
        assert_eq!(buf[(2, 1)].fg, theme.tab.fg.unwrap());
    }

    #[test]
    fn click_on_the_tab_row_switches_and_is_consumed() {
        let (mut t, _term, _) = drawn_two();
        assert_eq!(t.active, 1);
        assert!(t.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 3, 1)));
        assert_eq!(t.active, 0);
        assert!(t.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 30, 1)));
        assert_eq!(t.active, 0);
        assert!(t.handle_mouse(&mouse(MouseEventKind::ScrollUp, 5, 3)));
        assert_eq!(t.active, 0);
    }
}
