//! Terminal tool window: the shell's screen (vt100, parsed on the UI thread
//! as output arrives) drawn cell by cell with its colors and attributes, the
//! real cursor while focused, and the scrollback under the mouse wheel. The
//! terminal takes the size of the window's text area at every frame.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::keys;
use super::pty::Session;
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::tool_window_block;

/// Lines one wheel step scrolls.
const WHEEL: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No shell; the next frame that shows the window starts one.
    Idle,
    Running,
    /// The shell ended (the text says how) or could not start; a key or
    /// opening the window again starts a new one.
    Ended(String),
}

/// Parser callbacks: answers to the program's queries, and its title.
#[derive(Debug, Default)]
pub struct Replies {
    out: Vec<u8>,
    title: String,
}

impl vt100::Callbacks for Replies {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).chars().filter(|c| !c.is_control()).take(120).collect();
    }

    /// Cursor position and status reports and device attributes: shells
    /// (fish) and editors wait for these.
    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, params: &[&[u16]], c: char) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (None, 'n', 5) => self.out.extend_from_slice(b"\x1b[0n"),
            // A VT220 with no options.
            (None, 'c', 0) => self.out.extend_from_slice(b"\x1b[?62;22c"),
            (Some(b'>'), 'c', 0) => self.out.extend_from_slice(b"\x1b[>1;10;0c"),
            _ => {}
        }
    }
}

pub struct TerminalPanel {
    parser: vt100::Parser<Replies>,
    pub(crate) session: Option<Session>,
    pub status: Status,
    /// Scrollback lines of new screens.
    scrollback: usize,
    /// The text area at the last frame.
    area: Rect,
    /// An Esc at the shell prompt held back: a second one soon after
    /// leaves the window instead (`App::terminal_key`).
    pub(crate) pending_esc: Option<Instant>,
}

impl std::fmt::Debug for TerminalPanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalPanel").field("session", &self.session).field("status", &self.status).finish()
    }
}

impl TerminalPanel {
    pub fn new(scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(24, 80, scrollback, Replies::default()),
            session: None,
            status: Status::Idle,
            scrollback,
            area: Rect::default(),
            pending_esc: None,
        }
    }

    /// The text area inside the window's border, below the tab row when `tab_row`.
    pub fn text_area(area: Rect, tab_row: bool) -> Rect {
        let top = u16::from(tab_row);
        Rect::new(area.x + 1, area.y + 1 + top, area.width.saturating_sub(2), area.height.saturating_sub(2).saturating_sub(top))
    }

    /// Parse `bytes` as shell output, for tests.
    #[cfg(test)]
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn running(&self) -> bool {
        self.status == Status::Running
    }

    /// The shell's pid while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.session.as_ref().map(|s| s.pid())
    }

    /// The screen as text, for tests.
    #[cfg(test)]
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// The terminal's size in rows and columns.
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    /// Lines kept for scrolling back, from the next shell on (hot reload
    /// of `terminal.scrollback`).
    pub fn set_scrollback(&mut self, n: usize) {
        self.scrollback = n;
    }

    /// A new shell of `rows` x `cols` on a clean screen.
    pub fn start(&mut self, session: Session, rows: u16, cols: u16) {
        self.parser = vt100::Parser::new_with_callbacks(rows.max(1), cols.max(1), self.scrollback, Replies::default());
        self.session = Some(session);
        self.status = Status::Running;
    }

    /// The shell could not start.
    pub fn failed(&mut self, error: String) {
        self.session = None;
        self.status = Status::Ended(error);
    }

    /// Start over at the next frame if the shell has ended.
    pub fn restart(&mut self) {
        if matches!(self.status, Status::Ended(_)) {
            self.status = Status::Idle;
        }
    }

    /// Kill the shell and forget its screen (quit, root switch).
    pub fn reset(&mut self) {
        self.session = None;
        self.parser = vt100::Parser::new_with_callbacks(24, 80, self.scrollback, Replies::default());
        self.status = Status::Idle;
        self.pending_esc = None;
    }

    /// Whether `id` is the running shell's.
    pub fn is_current(&self, id: u64) -> bool {
        self.session.as_ref().is_some_and(|s| s.id == id)
    }

    /// Parse what the shell wrote, answer its queries, and on its exit
    /// show how it ended and kill what it left behind.
    pub fn pump(&mut self) {
        let Some(s) = &self.session else { return };
        let d = s.drain();
        if !d.bytes.is_empty() {
            self.parser.process(&d.bytes);
            s.write(std::mem::take(&mut self.parser.callbacks_mut().out));
        }
        if let Some(exit) = d.exit {
            let text = match exit.signal() {
                Some(sig) => format!("terminated: {sig}"),
                None => format!("exited {}", exit.exit_code()),
            };
            self.parser.screen_mut().set_scrollback(0);
            self.parser.process(format!("\x1b[0m\r\n[process {text}]").as_bytes());
            self.session = None;
            self.status = Status::Ended(text);
        }
    }

    /// Resize the screen and the terminal to `rows` x `cols`.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 || self.size() == (rows, cols) {
            return;
        }
        self.parser.screen_mut().set_size(rows, cols);
        if let Some(s) = &self.session {
            s.resize(rows, cols);
        }
    }

    fn write(&mut self, bytes: Vec<u8>) {
        if let Some(s) = &self.session {
            s.write(bytes);
        }
    }

    /// Whether a full-screen program (vim, less) has the alternate screen.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Send a held-back Esc.
    pub fn flush_esc(&mut self) {
        if self.pending_esc.take().is_some() {
            self.send_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
    }

    /// A key for the shell; the view returns to the bottom.
    pub fn send_key(&mut self, key: &KeyEvent) {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(0);
        if let Some(bytes) = keys::encode_key(key, screen.application_cursor()) {
            self.write(bytes);
        }
    }

    pub fn paste(&mut self, text: &str) {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(0);
        let bytes = keys::encode_paste(text, screen.bracketed_paste());
        self.write(bytes);
    }

    /// Lines scrolled back from the bottom.
    pub fn scrolled(&self) -> usize {
        self.parser.screen().scrollback()
    }

    fn scroll(&mut self, up: bool) {
        let screen = self.parser.screen_mut();
        let at = screen.scrollback();
        screen.set_scrollback(if up { at + WHEEL } else { at.saturating_sub(WHEEL) });
    }

    /// The program gets the mouse when it asked for it (Shift keeps it for
    /// jetl, as in xterm). Otherwise the wheel scrolls back, or on the
    /// alternate screen (less, man) sends arrow keys.
    pub fn handle_mouse(&mut self, m: &Mouse) -> bool {
        let screen = self.parser.screen();
        let mode = screen.mouse_protocol_mode();
        let a = self.area;
        if mode != vt100::MouseProtocolMode::None && self.running() && !m.mods.contains(KeyModifiers::SHIFT) && a.width > 0 && a.height > 0
        {
            let col = m.x.clamp(a.x, a.right() - 1) - a.x;
            let row = m.y.clamp(a.y, a.bottom() - 1) - a.y;
            if let Some(bytes) = keys::encode_mouse(m.kind, m.mods, col, row, mode, screen.mouse_protocol_encoding()) {
                self.write(bytes);
            }
            return true;
        }
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if screen.alternate_screen() {
                    let key = KeyEvent::new(if up { KeyCode::Up } else { KeyCode::Down }, KeyModifiers::NONE);
                    if let Some(bytes) = keys::encode_key(&key, screen.application_cursor()) {
                        self.write(bytes.repeat(WHEEL));
                    }
                } else {
                    self.scroll(up);
                }
                true
            }
            _ => false,
        }
    }

    /// Draw the screen; without `truecolor` the cells' RGB colors become
    /// their nearest 256-color palette entries, like the theme's.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, tab_row: bool, focused: bool, theme: &Theme, truecolor: bool) {
        let inner = Self::text_area(area, tab_row);
        self.area = inner;
        self.resize(inner.height, inner.width);
        let screen = self.parser.screen();
        let shell_title = &self.parser.callbacks().title;
        let mut title = match &self.status {
            Status::Ended(text) => format!(" Terminal: {text}, a key restarts "),
            _ if !shell_title.is_empty() => format!(" Terminal: {shell_title} "),
            _ => " Terminal ".to_string(),
        };
        if screen.scrollback() > 0 {
            title.push_str(&format!("[{} lines up] ", screen.scrollback()));
        }
        frame.render_widget(tool_window_block(&title, focused, theme), area);
        let (rows, cols) = screen.size();
        let buf = frame.buffer_mut();
        for row in 0..rows.min(inner.height) {
            for col in 0..cols.min(inner.width) {
                let Some(cell) = screen.cell(row, col) else { continue };
                // The cell before it draws a wide character over both.
                if cell.is_wide_continuation() {
                    continue;
                }
                let out = &mut buf[(inner.x + col, inner.y + row)];
                let text = cell.contents();
                out.set_symbol(if text.is_empty() { " " } else { text });
                out.set_style(cell_style(cell, theme, truecolor));
            }
        }
        if focused && self.running() && !screen.hide_cursor() && screen.scrollback() == 0 {
            let (row, col) = screen.cursor_position();
            if row < inner.height && col < inner.width {
                frame.set_cursor_position((inner.x + col, inner.y + row));
            }
        }
    }
}

/// The theme's colors stand in for the defaults; inverse swaps after that.
/// Without `truecolor`, RGB cells map to the 256-color palette.
fn cell_style(cell: &vt100::Cell, theme: &Theme, truecolor: bool) -> Style {
    let depth = |c: Color| if truecolor { c } else { crate::config::color::to_ansi256(c) };
    let mut fg = color(cell.fgcolor()).map(depth).unwrap_or(theme.fg);
    let mut bg = color(cell.bgcolor()).map(depth).unwrap_or(theme.bg);
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut m = Modifier::empty();
    for (on, flag) in [
        (cell.bold(), Modifier::BOLD),
        (cell.dim(), Modifier::DIM),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
    ] {
        if on {
            m |= flag;
        }
    }
    Style::default().fg(fg).bg(bg).add_modifier(m)
}

/// The 16 basic colors by name (SGR 30-37 and 90-97 on the host), the rest
/// of the 256 by index, RGB as is.
fn color(c: vt100::Color) -> Option<Color> {
    const BASIC: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(BASIC.get(usize::from(i)).copied().unwrap_or(Color::Indexed(i))),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_render_with_colors_attributes_and_wide_characters() {
        let theme = Theme::darcula();
        let mut p = TerminalPanel::new(100);
        p.parser.process(b"\x1b[1;31mred\x1b[0m \x1b[38;5;200mx\x1b[48;2;1;2;3my\x1b[0m \x1b[7;4mi\x1b[0m\r\n\xe4\xb8\x96z");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 5)).unwrap();
        term.draw(|f| p.render(f, f.area(), false, true, &theme, true)).unwrap();
        assert_eq!(p.size(), (3, 18), "the screen takes the text area's size");
        let buf = term.backend().buffer();
        let at = |x: u16, y: u16| &buf[(x + 1, y + 1)];
        assert_eq!(at(0, 0).symbol(), "r");
        assert_eq!(at(0, 0).fg, Color::Red);
        assert!(at(0, 0).modifier.contains(Modifier::BOLD));
        assert_eq!(at(4, 0).fg, Color::Indexed(200));
        assert_eq!(at(5, 0).bg, Color::Rgb(1, 2, 3));
        assert_eq!(at(3, 0).bg, theme.bg, "default colors are the theme's");
        // Inverse swaps the theme's colors.
        assert_eq!((at(7, 0).fg, at(7, 0).bg), (theme.bg, theme.fg));
        assert!(at(7, 0).modifier.contains(Modifier::UNDERLINED));
        assert_eq!(at(0, 1).symbol(), "世");
        assert_eq!(at(2, 1).symbol(), "z", "the wide character takes two cells");
    }

    #[test]
    fn rgb_cells_map_to_the_palette_without_true_color() {
        let theme = Theme::darcula();
        let mut p = TerminalPanel::new(100);
        p.parser.process(b"\x1b[38;2;255;0;0;48;2;128;128;128mx\x1b[0m\x1b[38;5;200my\x1b[0m\x1b[31mz\x1b[0m");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(10, 3)).unwrap();
        term.draw(|f| p.render(f, f.area(), false, true, &theme, false)).unwrap();
        let buf = term.backend().buffer();
        let at = |x: u16| &buf[(x + 1, 1)];
        assert_eq!((at(0).fg, at(0).bg), (Color::Indexed(196), Color::Indexed(244)));
        assert_eq!(at(1).fg, Color::Indexed(200), "palette colors are kept");
        assert_eq!(at(2).fg, Color::Red);
        assert_eq!(at(3).bg, theme.bg);
    }

    #[test]
    fn wheel_scrolls_back_unless_the_program_takes_the_mouse() {
        let mut p = TerminalPanel::new(100);
        p.resize(5, 20);
        for i in 0..30 {
            p.parser.process(format!("line {i}\r\n").as_bytes());
        }
        let wheel = |kind| Mouse { kind, x: 0, y: 0, clicks: 0, mods: KeyModifiers::NONE };
        assert!(p.handle_mouse(&wheel(MouseEventKind::ScrollUp)));
        p.handle_mouse(&wheel(MouseEventKind::ScrollUp));
        assert_eq!(p.scrolled(), 6);
        assert!(p.parser.screen().contents().starts_with("line 20"));
        p.handle_mouse(&wheel(MouseEventKind::ScrollDown));
        assert_eq!(p.scrolled(), 3);
        // Any key goes back to the bottom.
        p.send_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(p.scrolled(), 0);
        // The alternate screen has no scrollback.
        p.parser.process(b"\x1b[?1049h");
        p.handle_mouse(&wheel(MouseEventKind::ScrollUp));
        assert_eq!(p.scrolled(), 0);
    }

    #[test]
    fn queries_are_answered() {
        let mut p = TerminalPanel::new(0);
        p.parser.process(b"ab\x1b[6n\x1b[c");
        assert_eq!(p.parser.callbacks().out, b"\x1b[1;3R\x1b[?62;22c");
    }
}
