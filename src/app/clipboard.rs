//! Copy and the explicit paste action (`editor.paste`, Ctrl+V). Copies go
//! out through OSC 52, the copy tool when there is one, and jetl's own
//! register. Paste reads the system clipboard with the paste tool off the
//! main thread; without one it pastes the register.

use super::App;
use crate::actions::{Action, EditorAction};
use crate::editor::buffer::BufferId;
use crate::event::{AppMsg, Level};
use crate::ui::focus::PanelId;
use crate::util::clipboard::{self, Paste, Tools};

#[derive(Debug, Default)]
pub struct ClipboardState {
    /// Resolved on first use (`util::clipboard::detect`).
    pub(super) tools: Option<Tools>,
    /// jetl's last copy, pasted when the system clipboard cannot be read.
    pub(super) register: String,
    /// Paste requests so far; only the newest one's result applies.
    seq: u64,
    /// The request in flight and the text field it was made in.
    pending: Option<(u64, PasteTarget)>,
    /// The "system clipboard unavailable" warning was shown; cleared by a
    /// successful read so a later failure warns again.
    warned: bool,
}

/// The text field a paste goes to; the same order as `App::handle_paste`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PasteTarget {
    Dialog,
    Palette,
    FindBar(BufferId),
    CommitMessage,
    FindWindow,
    Terminal,
    Editor(BufferId),
}

impl App {
    pub(super) fn clipboard_tools(&mut self) -> &Tools {
        self.clipboard.tools.get_or_insert_with(|| {
            let tools = clipboard::detect(&self.config.clipboard);
            tracing::info!("clipboard tools: {tools:?}");
            tools
        })
    }

    /// Whether `key` is bound to `editor.paste` for an open dialog, which
    /// takes keys before the keymap.
    pub(super) fn is_paste_key(&self, key: crossterm::event::KeyEvent) -> bool {
        let chord = [crate::config::keymap::KeyCombo::normalize(key.code, key.modifiers)];
        let paste = Action::Editor(EditorAction::Paste);
        ["dialog", "global"].iter().any(|c| self.keymap.table(c).and_then(|t| t.bindings.get(chord.as_slice())) == Some(&paste))
    }

    pub(super) fn paste_target(&self) -> Option<PasteTarget> {
        if let Some(d) = &self.dialog {
            return matches!(d.kind, crate::ui::dialog::DialogKind::Input { .. } | crate::ui::dialog::DialogKind::NewTask(_))
                .then_some(PasteTarget::Dialog);
        }
        if self.palette.is_some() {
            return Some(PasteTarget::Palette);
        }
        if self.focus.is(PanelId::Terminal) {
            return Some(PasteTarget::Terminal);
        }
        let active = self.editor.active_buffer().map(|b| b.id);
        if self.focus.is(PanelId::Editor) && self.editor.find_focused() {
            return active.map(PasteTarget::FindBar);
        }
        if self.focus.is(PanelId::Git) && self.git_panel.message_focused() {
            return Some(PasteTarget::CommitMessage);
        }
        if self.focus.is(PanelId::Find) {
            return Some(PasteTarget::FindWindow);
        }
        if self.focus.is(PanelId::Editor) {
            return active.map(PasteTarget::Editor);
        }
        None
    }

    /// `editor.paste`: read the system clipboard into the focused text
    /// field. The result lands only if that field still has focus when it
    /// arrives (see [`App::paste_done`]).
    pub(super) fn paste(&mut self) {
        let Some(target) = self.paste_target() else { return };
        let argv = match &self.clipboard_tools().paste {
            Paste::Command(argv) => argv.clone(),
            Paste::Disabled => return self.paste_register(),
            Paste::Missing => return self.paste_fallback("no wl-paste, xclip or xsel found".into()),
        };
        self.clipboard.seq += 1;
        let seq = self.clipboard.seq;
        self.clipboard.pending = Some((seq, target));
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::spawn(async move {
            let result = clipboard::read(&argv).await;
            let _ = tx.send(AppMsg::Paste { generation, seq, result });
        });
    }

    /// A clipboard read finished. Dropped when a newer paste was asked for,
    /// the root changed, or focus moved to another field (or tab) meanwhile:
    /// text appearing in a field the user has left would be a surprise.
    pub(super) fn paste_done(&mut self, generation: u64, seq: u64, result: Result<String, String>) {
        let Some((want, target)) = self.clipboard.pending.take() else { return };
        if want != seq {
            self.clipboard.pending = Some((want, target));
            tracing::debug!("dropping an older paste");
            return;
        }
        if generation != self.workspace.generation || self.paste_target().as_ref() != Some(&target) {
            tracing::debug!("dropping a paste: its field lost focus");
            return;
        }
        match result {
            Ok(text) => {
                self.clipboard.warned = false;
                if !text.is_empty() {
                    self.handle_paste(text);
                }
            }
            Err(e) => self.paste_fallback(e),
        }
    }

    /// The system clipboard could not be read: warn (once until a read
    /// works) and paste the register.
    fn paste_fallback(&mut self, reason: String) {
        if self.clipboard.warned {
            tracing::debug!("system clipboard unavailable: {reason}");
        } else {
            self.clipboard.warned = true;
            let then = if self.clipboard.register.is_empty() {
                "use the terminal's paste shortcut"
            } else {
                "pasting jetl's last copy; the terminal's paste shortcut reads the system clipboard"
            };
            self.notify(Level::Warn, format!("System clipboard unavailable ({reason}): {then}"));
        }
        self.paste_register();
    }

    fn paste_register(&mut self) {
        if !self.clipboard.register.is_empty() {
            let text = self.clipboard.register.clone();
            self.handle_paste(text);
        }
    }

    /// OSC 52 copy so the selection reaches the system clipboard through the
    /// terminal, the copy tool for terminals without OSC 52, and the
    /// register for paste.
    pub(super) fn copy_to_clipboard(&mut self, text: &str) {
        use crossterm::clipboard::CopyToClipboard;
        use std::io::Write;
        self.clipboard.register = text.to_string();
        if let Some(argv) = self.clipboard_tools().copy.clone() {
            let text = text.to_string();
            tokio::spawn(async move {
                if let Err(e) = clipboard::write(&argv, text).await {
                    tracing::debug!("clipboard copy tool: {e}");
                }
            });
        }
        let mut out = std::io::stdout();
        if crossterm::execute!(out, CopyToClipboard::to_clipboard_from(text)).is_err() {
            tracing::debug!("OSC 52 copy failed");
        }
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{load, test_app};
    use crate::ui::dialog::DialogId;

    fn tools(paste: Paste) -> Option<Tools> {
        Some(Tools { paste, copy: None })
    }

    /// An editor with one empty tab, focused.
    fn editor_app(name: &str) -> (App, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        load(&mut app, "a.txt", "");
        app.focus.set(PanelId::Editor);
        (app, dir)
    }

    fn text(app: &App) -> String {
        app.editor.active_buffer().unwrap().text()
    }

    #[tokio::test]
    async fn paste_reads_the_tool_into_the_editor_as_one_undo_step() {
        let (mut app, dir) = editor_app("paste-tool");
        let sh = |s: &str| vec!["sh".to_string(), "-c".to_string(), s.to_string()];
        app.clipboard.tools = tools(Paste::Command(sh("printf 'one\\ntwo'")));
        app.perform(Action::Editor(EditorAction::Paste));
        assert_eq!(text(&app), "", "the read runs off the main thread");
        crate::app::tests::handle_next(&mut app, |m| matches!(m, AppMsg::Paste { .. })).await;
        assert_eq!(text(&app), "one\ntwo");
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&app), "", "one transaction");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_paste_is_dropped() {
        let (mut app, dir) = editor_app("paste-stale");
        app.clipboard.tools = tools(Paste::Command(vec!["jetl-test-unused".into()]));
        let g = app.workspace.generation;
        // Focus moves to another tab while the read is in flight.
        app.perform(Action::Editor(EditorAction::Paste));
        let first = app.editor.active_buffer().unwrap().id;
        load(&mut app, "b.txt", "");
        app.handle_msg(AppMsg::Paste { generation: g, seq: 1, result: Ok("x".into()) });
        assert_eq!(text(&app), "");
        assert!(app.editor.buffers.iter().all(|b| b.text().is_empty()), "nothing lands in buffer {first} either");
        // A newer request supersedes an older one still in flight.
        app.perform(Action::Editor(EditorAction::Paste));
        app.perform(Action::Editor(EditorAction::Paste));
        app.handle_msg(AppMsg::Paste { generation: g, seq: 2, result: Ok("old".into()) });
        assert_eq!(text(&app), "");
        app.handle_msg(AppMsg::Paste { generation: g, seq: 3, result: Ok("new".into()) });
        assert_eq!(text(&app), "new");
        // A root switch drops it too.
        app.perform(Action::Editor(EditorAction::Paste));
        app.handle_msg(AppMsg::Paste { generation: g + 1, seq: 4, result: Ok("y".into()) });
        assert_eq!(text(&app), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_tool_warns_once_and_pastes_the_last_copy() {
        let (mut app, dir) = editor_app("paste-missing");
        app.clipboard.tools = tools(Paste::Missing);
        app.perform(Action::Editor(EditorAction::Paste));
        assert_eq!(text(&app), "");
        let warns = |app: &App| app.notifications.iter().filter(|n| n.level == Level::Warn && n.text.contains("clipboard")).count();
        assert_eq!(warns(&app), 1);
        app.copy_to_clipboard("abc");
        app.perform(Action::Editor(EditorAction::Paste));
        assert_eq!(text(&app), "abc");
        assert_eq!(warns(&app), 1, "warned once");
        // A failed read falls back the same way.
        app.clipboard.tools = tools(Paste::Command(vec!["jetl-test-unused".into()]));
        app.perform(Action::Editor(EditorAction::Paste));
        let (g, seq) = (app.workspace.generation, app.clipboard.seq);
        app.handle_msg(AppMsg::Paste { generation: g, seq, result: Err("wl-paste: Nothing is copied".into()) });
        assert_eq!(text(&app), "abcabc");
        assert_eq!(warns(&app), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn paste_goes_to_the_focused_field() {
        let (mut app, dir) = test_app("paste-fields");
        app.clipboard.tools = tools(Paste::Disabled);
        app.copy_to_clipboard("a\nb");
        // Nothing to paste into: no request at all.
        app.focus.set(PanelId::Project);
        app.perform(Action::Editor(EditorAction::Paste));
        assert_eq!(app.paste_target(), None);
        app.open_palette(crate::actions::PaletteKind::Actions);
        assert_eq!(app.paste_target(), Some(PasteTarget::Palette));
        // Ctrl+V in the palette pastes instead of closing it.
        let ctrl_v = crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('v'), crossterm::event::KeyModifiers::CONTROL);
        app.handle_key(ctrl_v);
        let p = app.palette.as_ref().expect("palette stays open");
        assert_eq!(p.input.text, "a", "the palette takes the first line");
        // Dialogs take keys before the keymap; Ctrl+V still pastes.
        app.palette = None;
        let root = app.workspace.root.clone();
        app.dialog = Some(crate::ui::dialog::Dialog::input(DialogId::TreeRename(root), "Rename", "", "x", "OK"));
        app.handle_key(ctrl_v);
        let Some(crate::ui::dialog::DialogKind::Input { input, .. }) = app.dialog.as_ref().map(|d| &d.kind) else { panic!() };
        assert_eq!(input.text, "xa b", "newlines become spaces");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
