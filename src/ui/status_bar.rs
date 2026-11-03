//! Bottom status bar: the active file's path as crumbs (relative to the
//! project root, cut from the start), the git operation in progress and
//! running git operations, notifications (else the diagnostic at the
//! caret), then on the right the active file's problem counts, cursor
//! position and `OVR` in overwrite mode, line ending, encoding, indent and language of the active
//! buffer, its language server, and the current branch at the right edge
//! (unless the toolbar's branch widget shows it).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::path::{Component, Path};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::editor::breadcrumbs::{Part, fit_tail};
use crate::event::Level;
use crate::ui::layout::ToolbarButton;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusHit {
    Branch,
    Position,
    Notification,
    LineEnding,
    Encoding,
    Indent,
    Language,
    /// Error and warning counts: opens the Problems window.
    Problems,
}

pub fn render(app: &mut App, frame: &mut Frame, area: Rect) {
    let theme = app.theme.clone();
    let buf = frame.buffer_mut();
    let base = Style::default().bg(theme.status_bg).fg(theme.status_fg);
    buf.set_style(area, base);
    app.status_hits.clear();
    if area.width < 10 {
        return;
    }
    // Left: the active file's path as crumbs, cut from the start.
    let mut x = area.x + 1;
    let crumbs = app
        .editor
        .active_path()
        .map(|p| {
            let root = &app.workspace.root;
            let home = if p.starts_with(root) { None } else { directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) };
            path_crumbs(p, root, home.as_deref())
        })
        .unwrap_or_default();
    if let Some(last) = crumbs.len().checked_sub(1) {
        let dim = theme.dim.bg(theme.status_bg);
        for (text, part) in fit_tail(&crumbs, (area.width / 3) as usize) {
            let style = match part {
                Part::Crumb(i) if i == last => base.add_modifier(Modifier::BOLD),
                Part::Crumb(_) => base,
                Part::Sep | Part::Ellipsis => dim,
            };
            buf.set_stringn(x, area.y, &text, area.right().saturating_sub(x) as usize, style);
            x += text.width() as u16;
        }
        x += 2;
    }

    if let Some(ip) = app.in_progress() {
        let s = ip.label();
        let w = (s.width() as u16).min(area.right().saturating_sub(x));
        buf.set_stringn(x, area.y, &s, w as usize, theme.warn.bg(theme.status_bg));
        x += w + 2;
    }

    // Running git operations.
    if let Some(busy) = app.busy_label() {
        let w = (busy.width() as u16).min(area.width / 3);
        buf.set_stringn(x, area.y, &busy, w as usize, theme.status_accent.bg(theme.status_bg));
        x += w + 2;
    }

    // Right side, JetBrains order: position, line ending, encoding, indent,
    // language; then the mode (modal only) and keyboard mode; the branch
    // comes last, so it sits at the right edge. Each part has a drop rank:
    // on narrow widths the highest ranks go first.
    let mut right_parts: Vec<(String, Style, Option<StatusHit>, u8)> = Vec::new();
    if let Some(c) = app.problem_counts() {
        let style = if c.errors > 0 {
            theme.diag_error.fg.map_or(theme.error, |fg| Style::default().fg(fg))
        } else if c.warnings > 0 {
            theme.diag_warn.fg.map_or(theme.warn, |fg| Style::default().fg(fg))
        } else {
            theme.dim
        };
        right_parts.push((format!("✖ {} ⚠ {}", c.errors, c.warnings), style.bg(theme.status_bg), Some(StatusHit::Problems), 1));
    }
    if let Some(pos) = app.editor.status_position() {
        right_parts.push((pos, base, Some(StatusHit::Position), 0));
    }
    if app.editor.overwrite() {
        right_parts.push(("OVR".into(), theme.status_accent.bg(theme.status_bg), None, 1));
    }
    if let Some(le) = app.editor.status_line_ending() {
        right_parts.push((le, base, Some(StatusHit::LineEnding), 2));
        right_parts.push(("UTF-8".into(), base, Some(StatusHit::Encoding), 6));
    }
    if let Some(indent) = app.editor.status_indent() {
        right_parts.push((indent, base, Some(StatusHit::Indent), 5));
    }
    if let Some(lang) = app.editor.status_language() {
        right_parts.push((lang, base, Some(StatusHit::Language), 3));
    }
    if let Some(lsp) = app.lsp_status() {
        use crate::app::LspStatus;
        let part = match lsp {
            LspStatus::Idle(s) => (s, base),
            LspStatus::Busy(s) => (format!("{} {s}", app.spinner()), theme.status_accent.bg(theme.status_bg)),
            LspStatus::Down(s) => (s, theme.dim.bg(theme.status_bg)),
        };
        // Progress messages can be long ("Indexing 120/800 (some_crate)").
        right_parts.push((crate::util::text::fit(&part.0, 40), part.1, None, 4));
    }
    if app.dispatcher.modal {
        right_parts.push((app.mode.label().to_string(), theme.status_accent.bg(theme.status_bg), None, 1));
    }
    if !app.keyboard_enhanced {
        right_parts.push(("legacy keys".into(), theme.dim.bg(theme.status_bg), None, 4));
    }
    // The branch sits at the right edge, unless the toolbar shows it this
    // frame (`toolbar::render` runs first); a click opens the branches popup.
    let on_toolbar = app.layout.toolbar_hits.iter().any(|(b, _)| *b == ToolbarButton::Branch);
    if !on_toolbar && let Some(branch) = app.branch_label() {
        right_parts.push((branch, theme.status_accent.bg(theme.status_bg), Some(StatusHit::Branch), 1));
    }
    let room = area.right().saturating_sub(x);
    while right_parts.iter().map(|p| p.0.width() as u16 + 2).sum::<u16>() > room {
        let Some(i) = right_parts.iter().enumerate().max_by_key(|(_, p)| p.3).map(|(i, _)| i) else { break };
        right_parts.remove(i);
    }
    let mut rx = area.right();
    for (s, st, hit, _) in right_parts.iter().rev() {
        let w = s.width() as u16 + 2;
        rx -= w;
        buf.set_stringn(rx + 1, area.y, s, w as usize, *st);
        if let Some(h) = hit {
            app.status_hits.push((*h, Rect::new(rx, area.y, w, 1)));
        }
    }

    // Middle: pending chord or latest notification.
    let middle_w = rx.saturating_sub(x) as usize;
    if middle_w > 4 {
        if let Some(p) = app.dispatcher.pending_display() {
            buf.set_stringn(x, area.y, format!("{p} …"), middle_w, theme.status_accent.bg(theme.status_bg));
        } else if let Some(n) = app.current_notification() {
            let st = match n.level {
                Level::Info => theme.info,
                Level::Warn => theme.warn,
                Level::Error => theme.error,
            }
            .bg(theme.status_bg);
            let text = crate::util::text::fit(&n.text, middle_w);
            buf.set_stringn(x, area.y, &text, middle_w, st);
            app.status_hits.push((StatusHit::Notification, Rect::new(x, area.y, middle_w as u16, 1)));
        } else if let Some((sev, message)) = app.caret_problem() {
            let st = Style::default().fg(sev.style(&theme).fg.unwrap_or(theme.status_fg)).bg(theme.status_bg);
            let text = crate::util::text::fit(&format!("{} {message}", sev.glyph()), middle_w);
            buf.set_stringn(x, area.y, &text, middle_w, st);
        }
    }
}

/// The crumbs of the status bar's path: relative to `root` when inside it,
/// else relative to `home` with a first crumb `~`, else absolute with the
/// leading `/` glued to the first component. Only normal components count.
fn path_crumbs(path: &Path, root: &Path, home: Option<&Path>) -> Vec<String> {
    let names = |p: &Path| -> Vec<String> {
        p.components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    if let Ok(rel) = path.strip_prefix(root) {
        return names(rel);
    }
    if let Some(rel) = home.and_then(|h| path.strip_prefix(h).ok()) {
        let mut crumbs = vec!["~".to_string()];
        crumbs.extend(names(rel));
        return crumbs;
    }
    let mut crumbs = names(path);
    if path.has_root()
        && let Some(first) = crumbs.first_mut()
    {
        first.insert(0, '/');
    }
    crumbs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn path_crumbs_inside_root_under_home_and_absolute() {
        let root = Path::new("/p/r");
        let home = Some(Path::new("/h"));
        assert_eq!(path_crumbs(Path::new("/p/r/src/app/mod.rs"), root, home), v(&["src", "app", "mod.rs"]));
        assert_eq!(path_crumbs(Path::new("/p/r/a.txt"), root, home), v(&["a.txt"]));
        assert_eq!(
            path_crumbs(Path::new("/h/.local/state/jetl/scratches/scratch_1.rs"), root, home),
            v(&["~", ".local", "state", "jetl", "scratches", "scratch_1.rs"])
        );
        assert_eq!(path_crumbs(Path::new("/h/x.rs"), root, home), v(&["~", "x.rs"]));
        assert_eq!(path_crumbs(Path::new("/etc/hosts"), root, home), v(&["/etc", "hosts"]));
        assert_eq!(path_crumbs(Path::new("/etc/hosts"), root, None), v(&["/etc", "hosts"]));
    }

    fn status_row(app: &mut App, w: u16) -> (String, ratatui::buffer::Buffer) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 1)).unwrap();
        let frame = term.draw(|f| render(app, f, Rect::new(0, 0, w, 1))).unwrap();
        (crate::ui::snapshot::row_text(frame.buffer, 0, w), frame.buffer.clone())
    }

    #[tokio::test]
    async fn status_bar_marks_overwrite_mode() {
        use crate::actions::{Action, EditorAction};
        let (mut app, dir) = crate::app::tests::test_app("status-ovr");
        let path = app.workspace.root.join("a.txt");
        app.editor.open_text(path, "abc\n", false, &app.theme);
        let (row, _) = status_row(&mut app, 120);
        assert!(!row.contains("OVR"), "{row:?}");
        app.perform(Action::Editor(EditorAction::ToggleOverwrite));
        let (row, buf) = status_row(&mut app, 120);
        let at = row.find("Ln 1, Col 1  OVR").unwrap_or_else(|| panic!("{row:?}"));
        let x = row[..at].chars().count() + "Ln 1, Col 1  ".chars().count();
        assert_eq!(buf[(x as u16, 0)].fg, app.theme.status_accent.fg.unwrap_or(app.theme.status_fg));
        app.perform(Action::Editor(EditorAction::ToggleOverwrite));
        let (row, _) = status_row(&mut app, 120);
        assert!(!row.contains("OVR"), "{row:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_bar_draws_path_crumbs_in_place_of_the_name() {
        let (mut app, dir) = crate::app::tests::test_app("status-crumbs");
        let path = app.workspace.root.join("src/app/mod.rs");
        app.editor.open_text(path, "x\n", false, &app.theme);
        let (row, buf) = status_row(&mut app, 90);
        assert!(row.starts_with(" src › app › mod.rs  "), "{row:?}");
        assert!(!row.contains("root"), "{row:?}");
        assert!(buf[(13, 0)].modifier.contains(Modifier::BOLD));
        assert!(!buf[(1, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(5, 0)].fg, app.theme.dim.fg.unwrap_or(app.theme.status_fg));
        app.editor.open_text(PathBuf::from("/etc/hosts"), "x\n", false, &app.theme);
        let (row, _) = status_row(&mut app, 90);
        assert!(row.starts_with(" /etc › hosts  "), "{row:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_bar_cuts_a_long_path_from_the_start() {
        let (mut app, dir) = crate::app::tests::test_app("status-crumbs-cut");
        let path = app.workspace.root.join("very/deep/nested/dir/file.rs");
        app.editor.open_text(path, "x\n", false, &app.theme);
        let (row, buf) = status_row(&mut app, 30);
        assert!(row.starts_with(" …› file.rs  "), "{row:?}");
        assert!(!row.contains("very"), "{row:?}");
        assert_eq!(buf[(1, 0)].fg, app.theme.dim.fg.unwrap_or(app.theme.status_fg));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_bar_left_is_blank_without_a_file() {
        let (mut app, dir) = crate::app::tests::test_app("status-crumbs-blank");
        let (row, buf) = status_row(&mut app, 90);
        // The startup note may fill the middle; no cell carries the bold file name.
        assert!((0..90).all(|x| !buf[(x, 0)].modifier.contains(Modifier::BOLD)), "{row:?}");
        assert!(!row.contains("root"), "{row:?}");
        // No crumbs: the startup note starts right after the one-cell margin, no 2-blank gap.
        assert!(row.starts_with(" Terminal lacks"), "{row:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
