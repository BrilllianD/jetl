pub mod completion;
pub mod dialog;
pub mod event_log;
pub mod focus;
pub mod hover;
pub mod layout;
pub mod menu;
pub mod menu_bar;
pub mod palette;
pub mod status_bar;
pub mod tabs;
pub mod toasts;
pub mod tool_strip;
pub mod widgets;

#[cfg(test)]
pub(crate) mod snapshot;

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders};

use crate::app::App;
use crate::config::theme::Theme;
use crate::ui::focus::PanelId;

/// A bordered tool window frame. Returns the inner area.
pub fn tool_window_block<'a>(title: &'a str, focused: bool, theme: &Theme) -> Block<'a> {
    let (border, title_style) = if focused { (theme.border_focused, theme.title_focused) } else { (theme.border, theme.title) };
    Block::default().borders(Borders::ALL).border_type(BorderType::Plain).border_style(border).title(title).title_style(title_style)
}

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let theme = app.theme.clone();
    frame.buffer_mut().set_style(area, Style::default().bg(theme.bg).fg(theme.fg));
    app.layout.compute(area);
    let rects = app.layout.rects.clone();

    tool_strip::render(app, frame, rects.strip);
    app.sync_structure();
    app.sync_diag_marks();

    if let (Some(tw), r) = (app.layout.left, rects.left)
        && r.width > 0
    {
        let focused = app.focus.is(tw.panel());
        match tw.panel() {
            PanelId::Project => app.project.render(frame, r, focused, &theme),
            PanelId::Worktrees => app.worktrees.render(frame, r, focused, &theme),
            PanelId::Structure => app.structure.render(frame, r, focused, &theme),
            _ => {}
        }
    }
    if let (Some(tw), r) = (app.layout.bottom, rects.bottom)
        && r.height > 0
    {
        let focused = app.focus.is(tw.panel());
        match tw.panel() {
            PanelId::Git => app.git_panel.render(frame, r, focused, &theme),
            PanelId::Find => app.find.render(frame, r, focused, &theme),
            PanelId::EventLog => app.event_log.render(frame, r, focused, &theme, &app.notifications),
            PanelId::Run => app.run_panel.render(frame, r, focused, &theme),
            PanelId::Terminal => app.render_terminal(frame, r, focused, &theme),
            PanelId::Problems => {
                let hint = if app.config.lsp.enabled {
                    "No problems reported by the language servers"
                } else {
                    "Language servers are off ([lsp] enabled = false in config.toml)"
                };
                app.problems.render(frame, r, focused, &theme, &app.lsp.diagnostics, &app.workspace.root, hint);
            }
            PanelId::LocalHistory => app.history_panel.render(frame, r, focused, &theme),
            _ => {}
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
    tool_strip::render_tooltip(app, frame, area);

    // Toasts sit below the palette and dialogs, which own the input.
    app.toasts.render(frame, area, &theme);
    if let Some(p) = &mut app.palette {
        p.render(frame, area, &theme);
    }
    if let Some(m) = &mut app.menu {
        m.render(frame, area, &theme);
    }
    if let Some(m) = &mut app.menu_bar {
        m.render(frame, area, &theme);
    }
    if let Some(d) = &mut app.dialog {
        d.render(frame, area, &theme);
    }
}
