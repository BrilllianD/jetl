//! Terminal setup and teardown. Everything that touches the real terminal
//! state lives here so that normal exit and panics restore it identically.
//! The panic hook restores the terminal first, then writes the crash
//! recovery dump of unsaved buffers (`recovery.rs`).

use std::io::{self, Stdout, Write, stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::{
    cursor::{SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use ratatui::{
    Terminal,
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};
use unicode_width::UnicodeWidthStr;

pub type Term = Terminal<JetlBackend<Stdout>>;

/// One changed cell, as `Backend::draw` receives it.
type Drawn<'a> = (u16, u16, &'a Cell);

/// Writes each cell covered by a wide symbol before that symbol.
///
/// `ratatui-core`'s diff yields the trailing cell of a wide symbol holding U+FE0F, and `CrosstermBackend::draw`
/// writes no cursor move for a cell right after the last one, while the terminal has advanced two columns. Each
/// contiguous changed cell after it then lands one column to the right. Writing the trailing cell after a forced
/// move is no fix: a blank on the second half of a wide character erases the character. Written first, it clears
/// that column on a terminal that counts the symbol as one cell, and the symbol covers it on one that counts two.
/// The cells around it are no longer contiguous, so `CrosstermBackend::draw` writes every move itself.
fn trailing_first<'a>(content: impl Iterator<Item = Drawn<'a>>) -> impl Iterator<Item = Drawn<'a>> {
    let mut content = content.peekable();
    let mut held: Option<Drawn<'a>> = None;
    std::iter::from_fn(move || {
        let head = held.take().or_else(|| content.next())?;
        let width = head.2.symbol().width() as u16;
        match content.next_if(|c| c.1 == head.1 && c.0 > head.0 && c.0 - head.0 < width) {
            Some(covered) => {
                held = Some(head);
                Some(covered)
            }
            None => Some(head),
        }
    })
}

/// The backend jetl draws through: `CrosstermBackend` with its cells in the order [`trailing_first`] gives.
pub struct JetlBackend<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> JetlBackend<W> {
    pub fn new(writer: W) -> Self {
        Self { inner: CrosstermBackend::new(writer) }
    }
}

impl<W: Write> Backend for JetlBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = Drawn<'a>>,
    {
        self.inner.draw(trailing_first(content))
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

static ENHANCED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether the last caret shape written is the overwrite block.
static CARET_BLOCK: AtomicBool = AtomicBool::new(false);

/// Whether the terminal accepted the kitty keyboard protocol flags.
pub fn keyboard_enhanced() -> bool {
    ENHANCED.load(Ordering::Relaxed)
}

pub fn init(root: &Path) -> Result<Term> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste, EnableFocusChange, SetTitle(title(root)),)?;
    ACTIVE.store(true, Ordering::Relaxed);
    if supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )?;
        ENHANCED.store(true, Ordering::Relaxed);
        tracing::info!("keyboard enhancement enabled");
    } else {
        tracing::warn!("terminal lacks keyboard enhancement: ctrl+shift+letter, ctrl+tab and double-shift bindings unavailable");
    }
    let backend = JetlBackend::new(out);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn title(root: &Path) -> String {
    format!("jetl – {}", root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
}

/// Retitle the terminal after a root switch.
pub fn set_title(root: &Path) {
    if ACTIVE.load(Ordering::Relaxed) {
        let _ = execute!(stdout(), SetTitle(title(root)));
    }
}

/// Set the caret shape: `SteadyBlock` for overwrite mode, anything else
/// means the user's own shape. Writes DECSCUSR only when the shape
/// differs from the last one written, and only while the terminal is ours.
pub fn set_caret_style(style: SetCursorStyle) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let block = style == SetCursorStyle::SteadyBlock;
    if CARET_BLOCK.swap(block, Ordering::Relaxed) != block {
        let shape = if block { SetCursorStyle::SteadyBlock } else { SetCursorStyle::DefaultUserShape };
        let _ = execute!(stdout(), shape);
    }
}

/// Restore the terminal. Safe to call more than once and from the panic hook.
pub fn restore() {
    if !ACTIVE.swap(false, Ordering::Relaxed) {
        return;
    }
    CARET_BLOCK.store(false, Ordering::Relaxed);
    let mut out = stdout();
    if ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        LeaveAlternateScreen,
        SetCursorStyle::DefaultUserShape,
        Show
    );
    let _ = disable_raw_mode();
}

/// Which thread a panic ran on, relative to the one that installed the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicRoute {
    Main,
    Worker,
}

/// `Main` when `current` is the thread that installed the hook.
pub fn route(main: std::thread::ThreadId, current: std::thread::ThreadId) -> PanicRoute {
    if main == current { PanicRoute::Main } else { PanicRoute::Worker }
}

/// A panic caught on a thread other than the main one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerPanic {
    pub thread: String,
    pub message: String,
}

/// The text of a panic payload: a `&str` or `String`, else `Box<dyn Any>`.
pub fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Box<dyn Any>".to_string()
    }
}

/// Most worker panics kept for the next tick; further ones are only logged.
pub const MAX_QUEUED_PANICS: usize = 16;

/// Worker panics waiting for the UI thread to show them.
pub struct PanicQueue(std::sync::Mutex<Vec<WorkerPanic>>);

impl PanicQueue {
    pub const fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()))
    }

    /// Queue `p`; beyond [`MAX_QUEUED_PANICS`] it is dropped (it is already logged).
    pub fn push(&self, p: WorkerPanic) {
        let mut q = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if q.len() < MAX_QUEUED_PANICS {
            q.push(p);
        }
    }

    pub fn take(&self) -> Vec<WorkerPanic> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

impl Default for PanicQueue {
    fn default() -> Self {
        Self::new()
    }
}

static WORKER_PANICS: PanicQueue = PanicQueue::new();

/// Worker panics queued since the last call; `App::on_tick` shows them.
pub fn take_worker_panics() -> Vec<WorkerPanic> {
    WORKER_PANICS.take()
}

/// Chain a panic hook, keyed on the thread that installs it (the main
/// thread, which runs the UI loop). A panic there restores the terminal
/// first, then writes the unsaved buffers (`recovery::dump_on_panic`), then
/// logs and runs the default hook, and last tells the user where the buffers
/// went. A panic on any other thread is caught by its runtime while jetl
/// keeps running: the hook only logs it and queues a notice for
/// [`take_worker_panics`], so the terminal, the buffers and stderr stay
/// untouched.
pub fn install_panic_hook() {
    let main = std::thread::current().id();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let current = std::thread::current();
        if route(main, current.id()) == PanicRoute::Worker {
            let thread = current.name().unwrap_or("<unnamed>").to_string();
            tracing::error!("panic on thread {thread}: {info}");
            let mut message = payload_text(info.payload());
            if let Some(l) = info.location() {
                message.push_str(&format!(" ({}:{})", l.file(), l.line()));
            }
            WORKER_PANICS.push(WorkerPanic { thread, message });
            return;
        }
        restore();
        // After the terminal is back, so a failure here cannot leave it raw.
        let saved = crate::recovery::dump_on_panic();
        tracing::error!("panic: {info}");
        if let Some((dir, n)) = &saved {
            tracing::error!("recovery: {n} unsaved buffers written to {}", dir.display());
        }
        default(info);
        if let Some((dir, n)) = saved {
            crate::recovery::print_dump_note(&dir, n);
        }
    }));
}

pub fn stdout_handle() -> io::Stdout {
    stdout()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    use super::*;

    #[test]
    fn route_is_main_only_on_the_installing_thread() {
        let main = std::thread::current().id();
        assert_eq!(route(main, main), PanicRoute::Main);
        let other = std::thread::spawn(|| std::thread::current().id()).join().unwrap();
        assert_eq!(route(main, other), PanicRoute::Worker);
    }

    #[test]
    fn a_tokio_task_runs_and_panics_off_the_main_thread() {
        let main = std::thread::current().id();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap();
        rt.block_on(async {
            let (route_of_task, name) = tokio::spawn(async move {
                let t = std::thread::current();
                (route(main, t.id()), t.name().map(str::to_owned))
            })
            .await
            .unwrap();
            assert_eq!(route_of_task, PanicRoute::Worker);
            assert_eq!(name.as_deref(), Some("tokio-rt-worker"));

            // Tokio catches the panic; a quiet hook keeps the test output clean.
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let a = tokio::spawn(async { panic!("boom") }).await;
            let b = tokio::spawn(async { panic!("{}", 7) }).await;
            let c = tokio::spawn(async { std::panic::panic_any(5u8) }).await;
            std::panic::set_hook(prev);
            let text = |r: Result<(), tokio::task::JoinError>| payload_text(&*r.unwrap_err().into_panic());
            assert_eq!(text(a), "boom");
            assert_eq!(text(b), "7");
            assert_eq!(text(c), "Box<dyn Any>");
        });
    }

    #[test]
    fn panic_queue_caps_and_drains() {
        let q = PanicQueue::new();
        for i in 0..20 {
            q.push(WorkerPanic { thread: "t".into(), message: i.to_string() });
        }
        let got: Vec<String> = q.take().into_iter().map(|p| p.message).collect();
        let want: Vec<String> = (0..MAX_QUEUED_PANICS).map(|i| i.to_string()).collect();
        assert_eq!(got, want);
        assert!(q.take().is_empty());
    }

    const EMOJI: &str = "\u{274C}\u{FE0F}";

    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn frame(w: u16, h: u16, rows: &[&str]) -> Buffer {
        let mut b = Buffer::empty(Rect::new(0, 0, w, h));
        for (y, row) in rows.iter().enumerate() {
            b.set_string(0, y as u16, row, Style::default());
        }
        b
    }

    /// Paints frame 1 over an empty buffer, then each next frame over the one before; returns the bytes and the screen.
    fn paint<B: Backend<Error = io::Error>>(mut backend: B, sink: &Sink, frames: &[Buffer]) -> (Vec<u8>, vt100::Parser) {
        let (w, h) = (frames[0].area.width, frames[0].area.height);
        let mut prev = Buffer::empty(frames[0].area);
        for next in frames {
            backend.draw(prev.diff(next).into_iter()).unwrap();
            prev = next.clone();
        }
        let bytes = sink.0.borrow().clone();
        let mut parser = vt100::Parser::new(h, w, 0);
        parser.process(&bytes);
        (bytes, parser)
    }

    fn rows(p: &vt100::Parser, w: u16) -> Vec<String> {
        p.screen().rows(0, w).collect()
    }

    fn jetl(frames: &[Buffer]) -> (Vec<u8>, vt100::Parser) {
        let sink = Sink::default();
        paint(JetlBackend::new(sink.clone()), &sink, frames)
    }

    #[test]
    fn text_after_a_vs16_emoji_stays_in_its_column() {
        let (_, p) = jetl(&[frame(14, 1, &["abcdefghijKL"]), frame(14, 1, &[&format!("{EMOJI}12345678KL")])]);
        assert_eq!(rows(&p, 14), [format!("{EMOJI}12345678KL")]);
        assert_eq!(p.screen().cell(0, 10).unwrap().contents(), "K");
        assert_eq!(p.screen().cell(0, 0).unwrap().contents(), EMOJI);
    }

    /// Pins the upstream defect. When this fails after a ratatui upgrade, upstream is fixed and `JetlBackend` can go.
    #[test]
    fn a_bare_crossterm_backend_lands_one_column_off() {
        let sink = Sink::default();
        let frames = [frame(14, 1, &["abcdefghijKL"]), frame(14, 1, &[&format!("{EMOJI}12345678KL")])];
        let (_, p) = paint(CrosstermBackend::new(sink.clone()), &sink, &frames);
        assert_eq!(rows(&p, 14), [format!("{EMOJI} 12345678L")]);
    }

    #[test]
    fn two_emoji_in_one_row_both_stay() {
        let want = format!("{EMOJI}12{EMOJI}34ijKL");
        let (_, p) = jetl(&[frame(14, 1, &["abcdefghijKL"]), frame(14, 1, &[&want])]);
        assert_eq!(rows(&p, 14), [want]);
    }

    #[test]
    fn an_emoji_ending_at_the_last_column_keeps_the_next_row() {
        let want = [format!("abcd{EMOJI}"), "ghijkl".to_string()];
        let (_, p) = jetl(&[frame(6, 2, &["abcdef", "ghijkl"]), frame(6, 2, &[&want[0], &want[1]])]);
        assert_eq!(rows(&p, 6), want);
    }

    #[test]
    fn text_replacing_an_emoji_leaves_no_ghost() {
        let (_, p) = jetl(&[frame(14, 1, &[&format!("{EMOJI}12345678KL")]), frame(14, 1, &["abcdefghijKL"])]);
        assert_eq!(rows(&p, 14), ["abcdefghijKL"]);
    }

    #[test]
    fn rows_with_no_covered_cell_write_what_crossterm_writes() {
        let frames = [frame(14, 2, &["abcdefghijKL", "abcdefghijKL"]), frame(14, 2, &["xbcdefghijKL", "\u{65E5}12345678KL"])];
        let (ours, _) = jetl(&frames);
        let sink = Sink::default();
        let (theirs, _) = paint(CrosstermBackend::new(sink.clone()), &sink, &frames);
        assert_eq!(ours, theirs);
    }

    #[test]
    fn a_covered_cell_comes_before_its_symbol_and_nothing_else_moves() {
        let wide = Cell::new(EMOJI);
        let cjk = Cell::new("\u{65E5}");
        let blank = Cell::new(" ");
        let a = Cell::new("a");
        let b = Cell::new("b");
        let order = |cells: Vec<Drawn>| -> Vec<(u16, u16)> { trailing_first(cells.into_iter()).map(|(x, y, _)| (x, y)).collect() };
        assert_eq!(order(vec![(2, 0, &wide), (3, 0, &blank), (4, 0, &a)]), [(3, 0), (2, 0), (4, 0)]);
        assert_eq!(order(vec![(2, 0, &wide), (4, 0, &a)]), [(2, 0), (4, 0)]);
        assert_eq!(order(vec![(9, 0, &wide), (10, 1, &b)]), [(9, 0), (10, 1)]);
        assert_eq!(order(vec![(2, 0, &cjk), (3, 0, &blank)]), [(3, 0), (2, 0)]);
        assert_eq!(order(vec![(2, 0, &a), (3, 0, &b)]), [(2, 0), (3, 0)]);
    }
}
