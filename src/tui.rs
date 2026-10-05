//! Terminal setup and teardown. Everything that touches the real terminal
//! state lives here so that normal exit and panics restore it identically.
//! The panic hook restores the terminal first, then writes the crash
//! recovery dump of unsaved buffers (`recovery.rs`).

use std::io::{self, Stdout, stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type Term = Terminal<CrosstermBackend<Stdout>>;

static ENHANCED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);

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
    let backend = CrosstermBackend::new(out);
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

/// Restore the terminal. Safe to call more than once and from the panic hook.
pub fn restore() {
    if !ACTIVE.swap(false, Ordering::Relaxed) {
        return;
    }
    let mut out = stdout();
    if ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste, DisableFocusChange, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
}

/// Chain a panic hook that restores the terminal first, then writes the
/// unsaved buffers (`recovery::dump_on_panic`), then logs and runs the
/// default hook, and last tells the user where the buffers went.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
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
