pub mod completion;
pub mod dialog;
pub mod event_log;
pub mod focus;
pub mod hover;
pub mod layout;
pub mod menu;
pub mod menu_bar;
pub mod palette;
pub mod run_configs;
pub mod settings;
pub mod signature;
pub mod status_bar;
pub mod switcher;
pub mod tabs;
pub mod toasts;
pub mod tool_strip;
pub mod widgets;

#[cfg(test)]
pub(crate) mod snapshot;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders};

use crate::app::App;
use crate::config::theme::Theme;
use crate::ui::focus::{PanelId, ToolWindowId};

/// A bordered tool window frame. Returns the inner area.
pub fn tool_window_block<'a>(title: &'a str, focused: bool, theme: &Theme) -> Block<'a> {
    let (border, title_style) = if focused { (theme.border_focused, theme.title_focused) } else { (theme.border, theme.title) };
    Block::default().borders(Borders::ALL).border_type(BorderType::Plain).border_style(border).title(title).title_style(title_style)
}

/// The one place that maps a tool window to its panel's render call, so any
/// window renders on any side.
pub fn render_tool_window(app: &mut App, frame: &mut Frame, tw: ToolWindowId, r: Rect, theme: &Theme) {
    let focused = app.focus.is(tw.panel());
    match tw {
        ToolWindowId::Project => app.project.render(frame, r, focused, theme),
        ToolWindowId::Worktrees => app.worktrees.render(frame, r, focused, theme),
        ToolWindowId::Structure => app.structure.render(frame, r, focused, theme),
        ToolWindowId::Git => app.git_panel.render(frame, r, focused, theme),
        ToolWindowId::Find => app.find.render(frame, r, focused, theme),
        ToolWindowId::EventLog => app.event_log.render(frame, r, focused, theme, &app.notifications),
        ToolWindowId::Run => app.run_panel.render(frame, r, focused, theme, app.run.selected.as_deref()),
        ToolWindowId::Terminal => app.render_terminal(frame, r, focused, theme),
        ToolWindowId::Problems => {
            let hint = if app.config.lsp.enabled {
                "No problems reported by the language servers"
            } else {
                "Language servers are off ([lsp] enabled = false in config.toml)"
            };
            let active = app.editor.active_path();
            app.problems.render(frame, r, focused, theme, &app.lsp.diagnostics, &app.workspace.root, active, hint);
        }
        ToolWindowId::LocalHistory => app.history_panel.render(frame, r, focused, theme),
    }
}

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let theme = app.theme.clone();
    frame.buffer_mut().set_style(area, Style::default().bg(theme.bg).fg(theme.fg));
    app.layout.keep_maximized(app.focus.current);
    app.layout.compute(area);
    let rects = app.layout.rects.clone();

    tool_strip::render(app, frame, rects.strip, rects.strip_right);
    app.sync_structure();
    app.sync_diag_marks();

    for (tw, r) in [(app.layout.left, rects.left), (app.layout.bottom, rects.bottom), (app.layout.right, rects.right)] {
        if let Some(tw) = tw
            && r.width > 0
            && r.height > 0
        {
            render_tool_window(app, frame, tw, r, &theme);
        }
    }

    for (_, r) in &rects.splitters {
        frame.buffer_mut().set_style(*r, theme.splitter);
        if r.width == 1 {
            for y in r.y..r.bottom() {
                frame.buffer_mut()[(r.x, y)].set_symbol("│");
            }
        } else {
            for x in r.x..r.right() {
                frame.buffer_mut()[(x, r.y)].set_symbol("─");
            }
        }
    }

    let editor_focused = app.focus.is(PanelId::Editor);
    app.editor.render(frame, rects.tabs, rects.editor, editor_focused, &theme);

    status_bar::render(app, frame, rects.status);
    // The hover popup sits at the caret, over everything but the overlays.
    app.hover_check();
    if let (Some(h), Some(anchor)) = (&mut app.hover, app.editor.caret_cell()) {
        let above_status = ratatui::layout::Rect { height: rects.status.y.saturating_sub(area.y), ..area };
        h.render(frame, above_status, anchor, &theme);
    }
    // The completion popup too, its rows under the word being completed.
    app.completion_check();
    if let (Some(c), Some(anchor)) = (&mut app.completion, app.editor.caret_cell()) {
        let above_status = ratatui::layout::Rect { height: rects.status.y.saturating_sub(area.y), ..area };
        let word = unicode_width::UnicodeWidthStr::width(c.state.prefix.as_str()) as u16;
        c.render(frame, above_status, anchor, word, &theme);
    }
    // The Parameter Info popup, above the caret line.
    app.signature_check();
    if let (Some(s), Some(anchor)) = (&mut app.signature, app.editor.caret_cell()) {
        let above_status = ratatui::layout::Rect { height: rects.status.y.saturating_sub(area.y), ..area };
        s.render(frame, above_status, anchor, &theme);
    }
    tool_strip::render_tooltip(app, frame, area);

    if let Some(p) = &mut app.palette {
        p.render(frame, area, &theme);
    }
    if let Some(s) = &mut app.switcher {
        s.render(frame, area, &theme);
    }
    if let Some(m) = &mut app.menu {
        m.render(frame, area, &theme);
    }
    if let Some(m) = &mut app.menu_bar {
        m.render(frame, area, &theme);
    }
    if let Some(s) = &mut app.settings {
        s.render(frame, area, &theme);
    }
    if let Some(s) = &mut app.run_configs {
        s.render(frame, area, &theme);
    }
    if let Some(d) = &mut app.dialog {
        d.render(frame, area, &theme);
    }
    // Toasts are drawn last but never over an overlay, so a visible toast is
    // always clickable. The menu bar's own row is row 0, which toasts never use.
    let avoid: Vec<ratatui::layout::Rect> = [
        app.palette.as_ref().map(|p| p.rect()),
        app.switcher.as_ref().map(|s| s.rect()),
        app.menu.as_ref().map(|m| m.rect()),
        app.menu_bar.as_ref().map(|b| b.dropdown().rect()),
        app.settings.as_ref().map(|s| s.rect()),
        app.run_configs.as_ref().map(|s| s.rect()),
        app.dialog.as_ref().map(|d| d.rect()),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Toasts keep left of the right strip so its buttons stay visible and clickable.
    let strip = app.layout.rects.strip_right;
    let mut toast_area = area;
    if !strip.is_empty() && strip.x > area.x {
        toast_area.width = strip.x - area.x;
    }
    app.toasts.render(frame, toast_area, &theme, &avoid);
}
