//! Code completion in the app: the popup (`ui::completion`) over the list
//! logic (`lsp::completion`).
//!
//! Requests: Ctrl+Space (`lsp.complete`) asks at the caret; typing asks by
//! itself (`[lsp] completion_auto`) after a server trigger character, or
//! once the word at the caret has `completion_min_chars` chars and no list
//! for it is open or on its way. A newer request cancels the one in flight
//! (`lsp_tagged_request`). An answer is used while the caret is still in
//! the word it was asked for (`Origin::prefix`), not only at the exact
//! caret and version it was asked at: typing on filters the list locally,
//! and only an incomplete list (`isIncomplete`) is asked again as the word
//! changes. Leaving the word closes the popup.
//!
//! Accepting resolves the item first when the server supports it
//! (`completionItem/resolve`, for lazily computed `additionalTextEdits`),
//! waiting at most [`RESOLVE_WAIT`]; a key, click or paste meanwhile
//! accepts at once without it. The item's edits are one transaction (one
//! undo step).
//!
//! The documentation panel of the selected item (`ui::completion`) is
//! filled 150 ms after the selection settles (`completion_docs_tick` on the
//! tick): resolved with `completionItem/resolve` when the server supports
//! it (the answer counts only while the popup, its origin and the selected
//! item are unchanged; the resolved item is merged into the list, so
//! accepting reuses it), else from the item itself.
//!
//! A commit character typed after the popup was asked for with Ctrl+Space,
//! or after the selection was moved, accepts the item and then goes on to
//! the editor as typed text.
//!
//! Accepting a snippet at one caret selects its first tabstop (its mirrors
//! as extra carets); Tab and Shift+Tab then walk the tabstops
//! (`editor.snippet`, see `editor::snippet`) until the last one, Esc, or
//! the caret leaving them.
//!
//! Live templates (`app::snippets`) follow the server's rows, are never
//! resolved, and Ctrl+Space without a server lists them alone.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use lsp_types::CompletionItem;
use nucleo_matcher::Matcher;
use serde_json::{Value, json};

use super::App;
use super::lsp_nav::{COMPLETION, RESOLVE};
use crate::dispatch::Mode;
use crate::editor::selection::{Selection, is_word_char};
use crate::editor::snippet::SnippetSession;
use crate::editor::undo::TxKind;
use crate::lsp::completion::{self, CompletionState, Origin};
use crate::lsp::transport::ResponseError;
use crate::lsp::{DocTag, PendingTag};
use crate::ui::completion::{CompletionPopup, Docs, docs_lines};
use crate::ui::focus::PanelId;
use crate::ui::hover::Highlight;

/// How long accepting waits for `completionItem/resolve`.
pub(super) const RESOLVE_WAIT: Duration = Duration::from_millis(300);
/// How long the selection must rest before the documentation is fetched.
pub(super) const DOCS_DELAY: Duration = Duration::from_millis(150);

/// `CompletionTriggerKind`.
const INVOKED: u8 = 1;
const TRIGGER_CHARACTER: u8 = 2;
const INCOMPLETE: u8 = 3;

/// Completion requests (in `LspService`).
#[derive(Default)]
pub(super) struct CompletionReqs {
    /// The newest completion request: its seq, where it was made, and
    /// whether it was asked for (Ctrl+Space; an empty answer says so).
    pub request: Option<(u64, Origin, bool)>,
    /// An accepted item waiting for its resolve.
    pub accepting: Option<Accepting>,
    /// The resolve for the documentation panel: its seq, the popup's
    /// origin and the item index.
    pub docs: Option<(u64, Origin, usize)>,
    pub matcher: Matcher,
}

/// An accepted item waiting for `completionItem/resolve`.
pub(super) struct Accepting {
    seq: u64,
    item: CompletionItem,
    origin: Origin,
    deadline: Instant,
}

impl App {
    /// `lsp.complete` (Ctrl+Space): ask for completions at the caret.
    pub(super) fn lsp_complete(&mut self) {
        self.hover = None;
        if !self.focus.is(PanelId::Editor) || self.editor.find_focused() {
            return;
        }
        self.completion_request(INVOKED, None, true);
    }

    /// Ask at the caret; `trigger` is the trigger character typed.
    fn completion_request(&mut self, kind: u8, trigger: Option<char>, explicit: bool) {
        let Some(b) = self.editor.active_buffer() else { return };
        let origin = Origin::new(b.id, &b.rope, b.view.sel().head);
        let mut context = json!({"triggerKind": kind});
        if let Some(c) = trigger {
            context["triggerCharacter"] = json!(c.to_string());
        }
        match self.lsp_doc_request(origin.buffer, origin.pos, COMPLETION, json!({"context": context}), PendingTag::Completion) {
            Ok(seq) => self.lsp.completion.request = Some((seq, origin, explicit)),
            Err(why) => {
                if explicit && self.snippet_popup(origin) {
                    return;
                }
                if explicit && !why.is_empty() {
                    self.lsp_hint(why);
                }
            }
        }
    }

    /// The trigger characters of buffer `id`'s server, when it completes.
    fn completion_triggers(&self, id: crate::editor::buffer::BufferId) -> Option<Vec<String>> {
        let caps = self.lsp_caps(id)?;
        caps.has(COMPLETION).then(|| caps.completion_triggers())
    }

    /// After an editor action from the keyboard: refilter the open list or
    /// close it, ask again for an incomplete list whose word changed, and
    /// after `typed` (an `InsertChar`) ask by itself when it applies.
    pub(super) fn completion_after_edit(&mut self, typed: Option<char>) {
        let word_changed = self.completion_refilter();
        if !self.focus.is(PanelId::Editor) || self.editor.find_focused() || self.mode != Mode::Insert {
            return;
        }
        if word_changed && self.completion.as_ref().is_some_and(|p| p.state.incomplete) {
            return self.completion_request(INCOMPLETE, None, false);
        }
        let Some(c) = typed else { return };
        if !self.config.lsp.completion_auto {
            return;
        }
        let Some(b) = self.editor.active_buffer() else { return };
        // Not while typing at several carets; Ctrl+Space still asks.
        if b.view.sels.is_multi() {
            return;
        }
        let Some(triggers) = self.completion_triggers(b.id) else { return };
        if triggers.iter().any(|t| t.ends_with(c)) && !is_word_char(c) {
            return self.completion_request(TRIGGER_CHARACTER, Some(c), false);
        }
        if !is_word_char(c) || !b.view.sel().is_empty() || self.completion.is_some() {
            return;
        }
        let head = b.view.sel().head;
        let coming = self.lsp.completion.request.as_ref().is_some_and(|(_, o, _)| o.buffer == b.id && o.prefix(&b.rope, head).is_some());
        let word = head - completion::word_start(&b.rope, head);
        if !coming && word >= self.config.lsp.completion_min_chars.max(1) {
            self.completion_request(INVOKED, None, false);
        }
    }

    /// Filter the open list by the word now at the caret, or close it when
    /// the caret left that word or the editor is not where the list is.
    /// Returns whether the word changed (the list stays open).
    fn completion_refilter(&mut self) -> bool {
        let Some(p) = &mut self.completion else { return false };
        let overlay = self.palette.is_some()
            || self.dialog.is_some()
            || self.menu.is_some()
            || self.menu_bar.is_some()
            || self.settings.is_some()
            || self.run_configs.is_some();
        let here = !overlay
            && self.focus.is(PanelId::Editor)
            && p.group == self.editor.focused_group().id
            && !self.editor.find_focused()
            && self.mode == Mode::Insert;
        let prefix = self
            .editor
            .active_buffer()
            .filter(|b| here && b.id == p.state.origin.buffer && b.view.sel().is_empty())
            .and_then(|b| p.state.origin.prefix(&b.rope, b.view.sel().head));
        let Some(prefix) = prefix else {
            self.completion_close();
            return false;
        };
        if prefix == p.state.prefix {
            return false;
        }
        p.state.refilter(&prefix, &mut self.lsp.completion.matcher);
        self.dirty_frame = true;
        if p.state.order.is_empty() && !p.state.incomplete {
            self.completion = None;
            return false;
        }
        true
    }

    /// Drop the popup when it no longer applies (called by `ui::render`, so
    /// mouse moves and background changes are caught too).
    pub(crate) fn completion_check(&mut self) {
        if self.completion.is_some() {
            self.completion_refilter();
        }
    }

    /// The tick at `now`: schedule the documentation of the selected item
    /// once the selection has rested for [`DOCS_DELAY`], then fetch it.
    pub(super) fn completion_docs_tick(&mut self, now: Instant) {
        let Some(p) = &mut self.completion else { return };
        let want = p.state.order.get(p.state.selected).copied();
        if p.docs_want != want {
            p.docs_want = want;
            p.docs_due = want.map(|_| now + DOCS_DELAY);
            return;
        }
        if !p.docs_due.is_some_and(|d| now >= d) {
            return;
        }
        p.docs_due = None;
        if let Some(i) = want
            && p.docs.as_ref().is_none_or(|d| d.item != i)
        {
            self.completion_docs_fetch(i);
        }
    }

    /// Resolve item `i` for the panel when the server can, else show it as it is.
    fn completion_docs_fetch(&mut self, i: usize) {
        let Some(p) = &self.completion else { return };
        let origin = p.state.origin.clone();
        if !p.resolved.contains(&i) && !p.state.is_snippet(i) && self.lsp_caps(origin.buffer).is_some_and(|c| c.has(RESOLVE)) {
            let params = serde_json::to_value(&p.state.items[i]).unwrap_or(Value::Null);
            if let Ok(seq) = self.lsp_tagged_request(origin.buffer, origin.pos, RESOLVE, params, PendingTag::CompletionDocs) {
                self.lsp.completion.docs = Some((seq, origin, i));
                return;
            }
        }
        self.completion_docs_show(i);
    }

    /// Fill the panel from item `i` as it is now.
    pub(super) fn completion_docs_show(&mut self, i: usize) {
        let Some(p) = &mut self.completion else { return };
        let fallback = self.editor.index_of_id(p.state.origin.buffer).and_then(|ix| self.editor.buffers[ix].language());
        let lines = docs_lines(&p.state.items[i], Highlight { theme: &self.theme, fallback });
        p.docs = Some(Docs { item: i, lines, scroll: 0 });
        self.dirty_frame = true;
    }

    /// Close the popup and forget the request on its way.
    pub(super) fn completion_close(&mut self) {
        if self.completion.take().is_some() {
            self.dirty_frame = true;
        }
        if let Some((seq, ..)) = self.lsp.completion.docs.take()
            && self.lsp.inflight.get(RESOLVE).is_some_and(|f| f.seq == seq)
            && let Some(old) = self.lsp.inflight.remove(RESOLVE)
            && let Some(c) = self.lsp.clients.get(&old.server)
        {
            c.cancel(old.id);
        }
        if self.lsp.completion.request.take().is_some()
            && let Some(old) = self.lsp.inflight.remove(COMPLETION)
            && let Some(c) = self.lsp.clients.get(&old.server)
        {
            c.cancel(old.id);
        }
    }

    /// Keys while the popup is up: Up/Down/PgUp/PgDn select, Enter and Tab
    /// accept, Esc closes; other keys are not consumed (they go on to the
    /// editor, after which the list is filtered or closed).
    pub(super) fn completion_key(&mut self, key: KeyEvent) -> bool {
        self.completion_check();
        let Some(p) = &mut self.completion else { return false };
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) || p.state.order.is_empty() {
            return false;
        }
        if key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            if p.docs_shown() {
                p.scroll_docs(if key.code == KeyCode::Up { -1 } else { 1 });
                self.dirty_frame = true;
                self.dispatcher.reset();
                return true;
            }
            return false;
        }
        if let KeyCode::Char(c) = key.code
            && p.armed
            && (key.modifiers - KeyModifiers::SHIFT).is_empty()
        {
            let own = p.state.selected_item().and_then(|it| it.commit_characters.clone());
            let buffer = p.state.origin.buffer;
            let set = own.unwrap_or_else(|| self.lsp_caps(buffer).map(|c| c.completion_commit_chars()).unwrap_or_default());
            if set.contains(&c.to_string()) {
                self.completion_accept();
                self.completion_flush();
            }
            return false;
        }
        if !key.modifiers.is_empty() {
            return false;
        }
        let page = p.page() as isize;
        match key.code {
            KeyCode::Up => {
                p.state.step(-1, true);
                p.armed = true;
            }
            KeyCode::Down => {
                p.state.step(1, true);
                p.armed = true;
            }
            KeyCode::PageUp => {
                p.state.step(-page, false);
                p.armed = true;
            }
            KeyCode::PageDown => {
                p.state.step(page, false);
                p.armed = true;
            }
            KeyCode::Enter | KeyCode::Tab => self.completion_accept(),
            KeyCode::Esc => self.completion_close(),
            _ => return false,
        }
        self.dirty_frame = true;
        self.dispatcher.reset();
        true
    }

    /// Accept the selected item (also a click on a row).
    pub(super) fn completion_accept(&mut self) {
        let Some(p) = self.completion.take() else { return };
        self.completion_close();
        self.dirty_frame = true;
        let Some(item) = p.state.selected_item().cloned() else { return };
        let resolved = p.state.order.get(p.state.selected).is_some_and(|i| p.resolved.contains(i));
        let origin = p.state.origin.clone();
        let Some(b) = self.editor.active_buffer() else { return };
        let pos = crate::lsp::pos::position(&b.rope, b.view.sel().head);
        let params = serde_json::to_value(&item).unwrap_or(Value::Null);
        let snippet = p.state.order.get(p.state.selected).is_some_and(|&i| p.state.is_snippet(i));
        let resolvable = !resolved && !snippet && self.lsp_caps(origin.buffer).is_some_and(|c| c.has(RESOLVE));
        if resolvable && let Ok(seq) = self.lsp_tagged_request(origin.buffer, pos, RESOLVE, params, PendingTag::CompletionResolve) {
            self.lsp.completion.accepting = Some(Accepting { seq, item, origin, deadline: Instant::now() + RESOLVE_WAIT });
            return;
        }
        self.completion_apply(&origin, &item);
    }

    /// Accept the item waiting for its resolve now, without it (input came
    /// first, or the wait is over).
    pub(super) fn completion_flush(&mut self) {
        let Some(a) = self.lsp.completion.accepting.take() else { return };
        if let Some(old) = self.lsp.inflight.remove(RESOLVE)
            && let Some(c) = self.lsp.clients.get(&old.server)
        {
            c.cancel(old.id);
        }
        self.completion_apply(&a.origin, &a.item);
    }

    /// The resolve wait is over at `now` (the tick).
    pub(super) fn completion_timeout(&mut self, now: Instant) {
        if self.lsp.completion.accepting.as_ref().is_some_and(|a| now >= a.deadline) {
            self.completion_flush();
        }
    }

    /// Make the edit of `item` as one transaction, if the caret is still
    /// in the word the list was for.
    pub(super) fn completion_apply(&mut self, origin: &Origin, item: &CompletionItem) {
        let group = self.editor.focused_group().id;
        let Some(b) = self.editor.active_buffer_mut() else { return };
        if b.id != origin.buffer || !b.view.sel().is_empty() || origin.prefix(&b.rope, b.view.sel().head).is_none() {
            return tracing::debug!("completion: the caret left the word; not accepted");
        }
        let crlf = b.line_ending == crate::editor::buffer::LineEnding::CrLf;
        // The other carets (not selections) get the edit too when they typed the same prefix.
        let primary = b.view.sels.primary_index();
        let others: Vec<(usize, usize)> =
            b.view.sels.iter().enumerate().filter(|&(i, s)| i != primary && s.is_empty()).map(|(i, s)| (i, s.head)).collect();
        let heads: Vec<usize> = others.iter().map(|o| o.1).collect();
        let acc = completion::edit_for(item, origin, &b.rope, b.view.sel().head, &heads, crlf);
        // A snippet's tabstops are walked only from a single caret.
        let session = if b.view.sels.is_multi() { None } else { SnippetSession::new(b.id, group, acc.stops.clone()) };
        let sels = match &session {
            Some(s) => s.selections(),
            None => {
                // The other carets move along, or land after their copy of the edit.
                let mut sels = b.view.sels.clone();
                let mut landing: std::collections::HashMap<usize, usize> =
                    others.iter().zip(&acc.carets).filter_map(|(&(i, _), l)| l.map(|l| (i, l))).collect();
                for (i, (sel, _)) in sels.iter_mut_desired().enumerate() {
                    *sel = match landing.remove(&i) {
                        Some(l) => Selection::point(l),
                        None => Selection::new(
                            crate::lsp::edits::map_pos(&acc.changes, sel.anchor),
                            crate::lsp::edits::map_pos(&acc.changes, sel.head),
                        ),
                    };
                }
                *sels.primary_mut() = Selection::point(acc.caret);
                sels.normalize();
                sels
            }
        };
        let before = b.version;
        b.apply(acc.changes, sels, TxKind::Other);
        if let Some(mut s) = session.filter(|_| b.version != before) {
            s.attach(b);
            self.editor.set_snippet(Some(s));
        }
        self.dirty_frame = true;
    }

    /// End the snippet session when it no longer applies (before a key
    /// is looked up, so Tab indents again once it is over).
    pub(super) fn snippet_check(&mut self) {
        if !self.editor.snippet_active() {
            return;
        }
        let overlay = self.palette.is_some()
            || self.dialog.is_some()
            || self.menu.is_some()
            || self.menu_bar.is_some()
            || self.settings.is_some()
            || self.run_configs.is_some();
        let here = !overlay && self.focus.is(PanelId::Editor) && !self.editor.find_focused() && self.mode == Mode::Insert;
        self.editor.snippet_check(here);
    }

    /// A completion or resolve answer (the newest of its method).
    pub(super) fn completion_response(&mut self, name: &str, tag: PendingTag, result: Result<Value, ResponseError>) {
        match tag {
            PendingTag::Completion(t) => self.completion_answer(name, t, result),
            PendingTag::CompletionResolve(t) => {
                let Some(a) = self.lsp.completion.accepting.take_if(|a| a.seq == t.seq) else { return };
                let item = match result.map(serde_json::from_value::<CompletionItem>) {
                    Ok(Ok(resolved)) => merge_resolved(a.item, resolved),
                    _ => a.item,
                };
                self.completion_apply(&a.origin, &item);
            }
            PendingTag::CompletionDocs(t) => {
                let Some((_, origin, i)) = self.lsp.completion.docs.take_if(|d| d.0 == t.seq) else { return };
                if t.group != self.editor.focused_group().id {
                    return tracing::debug!("dropping a stale completion docs answer");
                }
                let Some(p) =
                    self.completion.as_mut().filter(|p| p.state.origin == origin && p.state.order.get(p.state.selected) == Some(&i))
                else {
                    return tracing::debug!("dropping a stale completion docs answer");
                };
                if let Ok(Ok(resolved)) = result.map(serde_json::from_value::<CompletionItem>) {
                    p.state.items[i] = merge_resolved(p.state.items[i].clone(), resolved);
                    // A failed resolve leaves the item unresolved: accepting asks again.
                    p.resolved.push(i);
                }
                self.completion_docs_show(i);
            }
            _ => {}
        }
    }

    fn completion_answer(&mut self, name: &str, t: DocTag, result: Result<Value, ResponseError>) {
        let Some((_, origin, explicit)) = self.lsp.completion.request.take_if(|r| r.0 == t.seq) else { return };
        if t.group != self.editor.focused_group().id {
            return tracing::debug!("dropping a stale completion answer");
        }
        let value = match result {
            Ok(v) => v,
            Err(e) if [ResponseError::REQUEST_CANCELLED, ResponseError::CONTENT_MODIFIED].contains(&e.code) => return,
            Err(e) if explicit => return self.lsp_hint(format!("{name}: {}", e.message)),
            Err(e) => return tracing::debug!("{name}: completion failed: {}", e.message),
        };
        let overlay = self.palette.is_some()
            || self.dialog.is_some()
            || self.menu.is_some()
            || self.menu_bar.is_some()
            || self.settings.is_some()
            || self.run_configs.is_some();
        let here = !overlay && self.focus.is(PanelId::Editor) && !self.editor.find_focused() && self.mode == Mode::Insert;
        let prefix = self
            .editor
            .active_buffer()
            .filter(|b| here && b.id == origin.buffer && b.view.sel().is_empty())
            .and_then(|b| origin.prefix(&b.rope, b.view.sel().head));
        let Some(prefix) = prefix else { return tracing::debug!("dropping a stale completion answer") };
        let armed = explicit || self.completion.as_ref().is_some_and(|p| p.armed);
        let (items, incomplete) = completion::parse(value);
        let mut state = CompletionState::new(origin, items, incomplete, &prefix, &mut self.lsp.completion.matcher);
        let snippets = self.snippet_items(state.origin.buffer, state.origin.start);
        state.add_snippets(snippets, &mut self.lsp.completion.matcher);
        self.dirty_frame = true;
        if state.order.is_empty() && !state.incomplete {
            self.completion = None;
            if explicit {
                self.lsp_hint("No suggestions");
            }
            return;
        }
        self.hover = None;
        let mut popup = CompletionPopup::new(state, self.editor.focused_group().id);
        popup.armed = armed;
        self.completion = Some(popup);
    }
}

/// A resolved item, with what the server left out taken from the item it
/// was asked about.
fn merge_resolved(orig: CompletionItem, mut r: CompletionItem) -> CompletionItem {
    r.text_edit = r.text_edit.or(orig.text_edit);
    r.insert_text = r.insert_text.or(orig.insert_text);
    r.insert_text_format = r.insert_text_format.or(orig.insert_text_format);
    r.additional_text_edits = r.additional_text_edits.or(orig.additional_text_edits);
    r.filter_text = r.filter_text.or(orig.filter_text);
    r.documentation = r.documentation.or(orig.documentation);
    r.detail = r.detail.or(orig.detail);
    r.label_details = r.label_details.or(orig.label_details);
    r.commit_characters = r.commit_characters.or(orig.commit_characters);
    if r.label.is_empty() {
        r.label = orig.label;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, EditorAction, Motion};
    use crate::app::lsp::tests::{incremental, is_lsp, open_on_server, sent, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::lsp::fake::FakeServer;
    use crossterm::event::KeyModifiers;

    /// Completion with `.` and `:` as trigger characters.
    fn caps(resolve: bool) -> Value {
        let mut c = incremental();
        c["completionProvider"] = json!({"triggerCharacters": [".", ":"], "resolveProvider": resolve});
        c
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn typ(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    fn text(app: &App) -> String {
        app.editor.active_buffer().unwrap().text()
    }

    /// The caret line with `|` at the caret.
    fn caret_line(app: &App) -> String {
        let b = app.editor.active_buffer().unwrap();
        let (line, col) = crate::editor::selection::pos_to_line_col(&b.rope, b.view.sel().head);
        let mut l: Vec<char> = b.line_text(line).chars().collect();
        l.insert(col, '|');
        l.into_iter().collect()
    }

    fn labels(app: &App) -> Vec<String> {
        let s = &app.completion.as_ref().expect("the popup").state;
        s.order.iter().map(|&i| s.items[i].label.clone()).collect()
    }

    fn draw(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let buf = term.draw(|f| crate::ui::render(app, f)).unwrap().buffer.clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect()).collect()
    }

    /// Items as rust-analyzer sends them: a snippet with a text edit over
    /// the typed word (`l0..c` on line `line`), plain ones without.
    fn items(line: u32, l0: u32, c: u32) -> Value {
        let r = json!({"start": {"line": line, "character": l0}, "end": {"line": line, "character": c}});
        json!([
            {"label": "println!", "kind": 3, "detail": "macro_rules! println", "insertTextFormat": 2,
             "textEdit": {"range": r, "newText": "println!(\"$1\")$0"}},
            {"label": "print!", "kind": 3, "insertTextFormat": 2, "textEdit": {"range": r, "newText": "print!($0)"}},
            {"label": "ptr", "kind": 9, "detail": "module"},
            {"label": "Option", "kind": 13},
        ])
    }

    async fn completion_req(server: &mut FakeServer) -> Value {
        server.recv("textDocument/completion").await
    }

    async fn reply(app: &mut App, server: &mut FakeServer, req: &Value, result: Value) {
        server.respond(req["id"].as_i64().unwrap(), result);
        handle_next(app, is_lsp).await;
    }

    const MAIN: &str = "fn main() {\n    \n}\n";

    async fn setup(name: &str, resolve: bool) -> (App, FakeServer, std::path::PathBuf) {
        let (mut app, dir) = test_app(name);
        let mut servers = with_fake_servers(&mut app, caps(resolve));
        let server = open_on_server(&mut app, &mut servers, "a.rs", MAIN).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        (app, server, dir)
    }

    #[tokio::test]
    async fn typing_opens_the_popup_and_enter_accepts_in_one_undo_step() {
        let (mut app, mut server, dir) = setup("compl-accept", false).await;
        typ(&mut app, "p");
        let req = completion_req(&mut server).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 5}));
        assert_eq!(req["params"]["context"], json!({"triggerKind": 1}));
        // Typed on before the answer: it is kept and filtered by "pr".
        typ(&mut app, "r");
        reply(&mut app, &mut server, &req, items(1, 4, 5)).await;
        assert_eq!(labels(&app), ["print!", "println!", "ptr", "Option"].map(String::from)[..3]);
        app.toasts = Default::default();
        // Drawn under the caret line, rows lined up with the word.
        let rows = draw(&mut app, 80, 24);
        let (cx, cy) = app.editor.caret_cell().unwrap();
        let below = &rows[cy as usize + 1..cy as usize + 6];
        assert!(below[0].contains('┌'), "{below:#?}");
        assert!(below[1].contains("f print!"), "{below:#?}");
        assert!(below[2].contains("f println!") && below[2].contains("macro_rules! println"), "{below:#?}");
        assert!(below[3].contains("M ptr") && below[3].contains("module"), "{below:#?}");
        let label_x = below[1].chars().position(|c| c == 'p').unwrap() as u16;
        assert_eq!(label_x, cx - 2, "the label starts under the word");
        // Typing filters; Down selects; Enter accepts the snippet.
        typ(&mut app, "i");
        assert_eq!(labels(&app), ["print!", "println!"]);
        key(&mut app, KeyCode::Down);
        assert_eq!(caret_line(&app), "    pri|");
        key(&mut app, KeyCode::Enter);
        assert!(app.completion.is_none());
        // The caret is at the snippet's `$1`, inside the quotes.
        assert_eq!(caret_line(&app), "    println!(\"|\")");
        assert!(app.editor.snippet.is_some());
        // One undo step back to the typed word, another to nothing.
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(caret_line(&app), "    pri|");
        app.snippet_check();
        assert!(app.editor.snippet.is_none(), "undoing the insertion ends the snippet");
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&app), MAIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A snippet item over `line` from char `l0` to `c`.
    fn snippet_item(line: u32, l0: u32, c: u32, label: &str, text: &str) -> Value {
        let r = json!({"start": {"line": line, "character": l0}, "end": {"line": line, "character": c}});
        json!([{"label": label, "kind": 3, "insertTextFormat": 2, "textEdit": {"range": r, "newText": text}}])
    }

    /// The selected text of the primary caret.
    fn selected(app: &App) -> String {
        let b = app.editor.active_buffer().unwrap();
        b.rope.slice(b.view.sel().range()).to_string()
    }

    #[tokio::test]
    async fn snippet_tabstops_are_walked_with_tab() {
        let (mut app, mut server, dir) = setup("compl-snippet-tab", false).await;
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, snippet_item(1, 4, 5, "foo(…)", "foo(${1:a}, ${2:b})$0")).await;
        key(&mut app, KeyCode::Enter);
        assert_eq!(text(&app), "fn main() {\n    foo(a, b)\n}\n");
        assert_eq!(selected(&app), "a");
        assert!(app.contexts().contains(&"editor.snippet"));
        // Typing replaces the placeholder; Tab and Shift+Tab walk the stops.
        typ(&mut app, "x1");
        key(&mut app, KeyCode::Tab);
        assert_eq!(selected(&app), "b");
        key(&mut app, KeyCode::BackTab);
        assert_eq!(selected(&app), "x1");
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "    foo(x1, b)|");
        assert!(app.editor.snippet.is_none());
        assert!(!app.contexts().contains(&"editor.snippet"));
        // Over: Tab indents again.
        key(&mut app, KeyCode::Tab);
        let line = caret_line(&app);
        assert!(line.starts_with("    foo(x1, b)") && line.len() > "    foo(x1, b)|".len(), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snippet_mirrors_esc_and_leaving_end_it() {
        let (mut app, mut server, dir) = setup("compl-snippet-esc", false).await;
        typ(&mut app, "l");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, snippet_item(1, 4, 5, "let", "let ${1:v} = 1; $1")).await;
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.editor.active_buffer().unwrap().view.sels.len(), 2, "a caret at each $1");
        typ(&mut app, "w");
        assert_eq!(text(&app), "fn main() {\n    let w = 1; w\n}\n");
        // One Esc ends the snippet and drops the mirror caret.
        key(&mut app, KeyCode::Esc);
        assert!(app.editor.snippet.is_none());
        assert!(!app.editor.active_buffer().unwrap().view.sels.is_multi());
        // Again on the next line; moving out of the stops ends it.
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Enter);
        assert_eq!(caret_line(&app), "    |");
        typ(&mut app, "l");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, snippet_item(2, 4, 5, "let", "let ${1:v} = 1; $1")).await;
        key(&mut app, KeyCode::Enter);
        assert!(app.editor.snippet.is_some());
        app.perform(Action::Editor(EditorAction::Move(Motion::Up, false)));
        app.snippet_check();
        assert!(app.editor.snippet.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn completing_inside_a_tabstop_keeps_the_session() {
        let (mut app, mut server, dir) = setup("compl-snippet-nested", false).await;
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, snippet_item(1, 4, 5, "foo(…)", "foo(${1:a}, ${2:b})$0")).await;
        key(&mut app, KeyCode::Enter);
        // Typing in the first argument asks; a plain item is accepted there.
        typ(&mut app, "x");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([{"label": "xval"}])).await;
        key(&mut app, KeyCode::Enter);
        assert_eq!(caret_line(&app), "    foo(xval|, b)");
        assert!(app.editor.snippet.is_some());
        key(&mut app, KeyCode::Tab);
        assert_eq!(selected(&app), "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn several_carets_ask_only_on_ctrl_space_and_accept_at_every_caret() {
        let (mut app, mut server, dir) = setup("compl-carets", false).await;
        // A second caret at the end of line 0; the one on line 1 stays primary.
        let b = app.editor.active_buffer_mut().unwrap();
        b.view.sels = crate::editor::selection::Selections::new(vec![Selection::point(11), Selection::point(16)], 1);
        typ(&mut app, "p");
        assert_eq!(text(&app), "fn main() {p\n    p\n}\n");
        app.lsp_sync();
        server.recv("textDocument/didChange").await;
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/completion"), "no asking by itself");
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let req = completion_req(&mut server).await;
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 5}), "about the primary caret");
        reply(&mut app, &mut server, &req, items(1, 4, 5)).await;
        key(&mut app, KeyCode::Enter);
        assert_eq!(text(&app), "fn main() {print!()\n    print!()\n}\n");
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.view.sels.heads().collect::<Vec<_>>(), [18, 31], "both inside the parentheses");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_caret_with_another_prefix_gets_no_completion() {
        let (mut app, mut server, dir) = setup("compl-carets-other", false).await;
        typ(&mut app, "p");
        let _auto = completion_req(&mut server).await;
        // A caret after "fn ma" typed something else than "p".
        let b = app.editor.active_buffer_mut().unwrap();
        b.view.sels = crate::editor::selection::Selections::new(vec![Selection::point(5), Selection::point(17)], 1);
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        server.recv("$/cancelRequest").await;
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, items(1, 4, 5)).await;
        key(&mut app, KeyCode::Enter);
        assert_eq!(text(&app), "fn main() {\n    print!()\n}\n");
        let b = app.editor.active_buffer().unwrap();
        assert_eq!(b.view.sels.heads().collect::<Vec<_>>(), [5, 23]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn superseded_and_stale_answers_are_dropped() {
        let (mut app, mut server, dir) = setup("compl-stale", false).await;
        typ(&mut app, "p");
        let first = completion_req(&mut server).await;
        // Ctrl+Space asks again and cancels the first.
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let cancel = server.recv("$/cancelRequest").await;
        assert_eq!(cancel["params"]["id"], first["id"]);
        let second = completion_req(&mut server).await;
        assert!(app.palette.is_none(), "Ctrl+Space in the editor is not the branches popup");
        reply(&mut app, &mut server, &first, items(1, 4, 5)).await;
        assert!(app.completion.is_none(), "the cancelled request's answer");
        reply(&mut app, &mut server, &second, items(1, 4, 5)).await;
        assert!(app.completion.is_some());
        // Esc closes it; Enter then is a newline.
        key(&mut app, KeyCode::Esc);
        assert!(app.completion.is_none());
        key(&mut app, KeyCode::Enter);
        assert_eq!(text(&app), "fn main() {\n    p\n    \n}\n");
        // An answer after the caret left the word is dropped.
        typ(&mut app, "p");
        let req = completion_req(&mut server).await;
        app.perform(Action::Editor(EditorAction::Move(Motion::Left, false)));
        reply(&mut app, &mut server, &req, items(2, 4, 5)).await;
        assert!(app.completion.is_none());
        // Leaving the word closes an open popup; a typed bracket still pairs.
        app.perform(Action::Editor(EditorAction::Move(Motion::Right, false)));
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, items(2, 4, 5)).await;
        assert!(app.completion.is_some());
        typ(&mut app, "(");
        assert!(app.completion.is_none());
        assert_eq!(caret_line(&app), "    p(|)");
        // Nothing to offer: Ctrl+Space says so.
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!(null)).await;
        assert!(app.toasts.items().iter().any(|t| t.text == "No suggestions"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn incomplete_lists_are_asked_again_and_trigger_characters_open_at_once() {
        let (mut app, mut server, dir) = setup("compl-incomplete", false).await;
        app.config.lsp.completion_min_chars = 3;
        // Below the minimum nothing is asked; a trigger character asks.
        typ(&mut app, "v");
        app.lsp_sync();
        server.recv("textDocument/didChange").await;
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/completion"));
        typ(&mut app, ".");
        let req = completion_req(&mut server).await;
        assert_eq!(req["params"]["context"], json!({"triggerKind": 2, "triggerCharacter": "."}));
        assert_eq!(req["params"]["position"], json!({"line": 1, "character": 6}));
        let list = json!({"isIncomplete": true, "items": [{"label": "len"}, {"label": "last"}]});
        reply(&mut app, &mut server, &req, list).await;
        assert_eq!(labels(&app), ["last", "len"]);
        // Typing on asks again (the old list stays up meanwhile).
        typ(&mut app, "l");
        let req = completion_req(&mut server).await;
        assert_eq!(req["params"]["context"], json!({"triggerKind": 3}));
        assert_eq!(labels(&app), ["last", "len"]);
        reply(&mut app, &mut server, &req, json!({"isIncomplete": false, "items": [{"label": "len"}, {"label": "lines"}]})).await;
        assert_eq!(labels(&app), ["len", "lines"]);
        // Complete now: more typing filters without asking.
        typ(&mut app, "i");
        assert_eq!(labels(&app), ["lines"]);
        app.lsp_sync();
        server.recv("textDocument/didChange").await;
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/completion"));
        // Tab accepts too; without a text edit the label replaces the word.
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "    v.lines|");
        // `::` triggers on each colon; auto completion off asks nothing.
        app.config.lsp.completion_auto = false;
        typ(&mut app, ":");
        app.lsp_sync();
        server.recv("textDocument/didChange").await;
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "textDocument/completion"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn accepting_resolves_first_and_applies_the_import_with_the_item() {
        let (mut app, mut server, dir) = setup("compl-resolve", true).await;
        typ(&mut app, "Ha");
        let req = completion_req(&mut server).await;
        let item_to = |end: u32| {
            json!({"label": "HashMap", "kind": 22, "data": {"id": 7},
                   "textEdit": {"range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": end}}, "newText": "HashMap"}})
        };
        let item = item_to(5);
        reply(&mut app, &mut server, &req, json!([item])).await;
        key(&mut app, KeyCode::Enter);
        let res = server.recv("completionItem/resolve").await;
        assert_eq!(res["params"]["data"], json!({"id": 7}));
        assert!(res["params"].get("textDocument").is_none());
        // Nothing yet; the resolved item brings the import.
        assert_eq!(caret_line(&app), "    Ha|");
        let mut resolved = item.clone();
        resolved["additionalTextEdits"] = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "use std::collections::HashMap;\n"}]);
        reply(&mut app, &mut server, &res, resolved).await;
        assert_eq!(text(&app), "use std::collections::HashMap;\nfn main() {\n    HashMap\n}\n");
        assert_eq!(caret_line(&app), "    HashMap|");
        app.perform(Action::Editor(EditorAction::Undo));
        assert_eq!(text(&app), "fn main() {\n    Ha\n}\n");

        // No answer in time: accepted as it is; the late answer is ignored.
        typ(&mut app, "s");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([item_to(7)])).await;
        key(&mut app, KeyCode::Enter);
        let res = server.recv("completionItem/resolve").await;
        app.completion_timeout(Instant::now() + RESOLVE_WAIT);
        assert_eq!(caret_line(&app), "    HashMap|");
        let cancel = server.recv("$/cancelRequest").await;
        assert_eq!(cancel["params"]["id"], res["id"]);
        server.respond(res["id"].as_i64().unwrap(), json!({"label": "HashMap", "insertText": "nope"}));
        handle_next(&mut app, is_lsp).await;
        assert_eq!(text(&app), "fn main() {\n    HashMap\n}\n");

        // A key while waiting accepts first, then goes on.
        app.perform(Action::Editor(EditorAction::Undo));
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([item_to(7)])).await;
        key(&mut app, KeyCode::Enter);
        server.recv("completionItem/resolve").await;
        typ(&mut app, ";");
        assert_eq!(caret_line(&app), "    HashMap;|");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two items with resolve data, completing the word `p` on line 1.
    fn two_items() -> Value {
        let r = json!({"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 5}});
        json!([
            {"label": "pa", "data": 1, "textEdit": {"range": r, "newText": "pa"}},
            {"label": "pb", "data": 2, "textEdit": {"range": r, "newText": "pb"}},
        ])
    }

    #[tokio::test]
    async fn docs_resolve_lazily_and_stale_answers_are_dropped() {
        let (mut app, mut server, dir) = setup("compl-docs", true).await;
        typ(&mut app, "p");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, two_items()).await;
        let tick = |app: &mut App| {
            let now = Instant::now();
            app.completion_docs_tick(now);
            app.completion_docs_tick(now + DOCS_DELAY);
        };
        tick(&mut app);
        let res = server.recv("completionItem/resolve").await;
        assert_eq!(res["params"]["data"], json!(1));
        // The selection moved before the answer: it is dropped.
        key(&mut app, KeyCode::Down);
        let docs = |text: &str| json!({"label": "pa", "documentation": {"kind": "markdown", "value": text}});
        reply(&mut app, &mut server, &res, docs("Old")).await;
        let p = app.completion.as_ref().unwrap();
        assert!(p.docs.is_none() && p.resolved.is_empty());
        // The new selection resolves and shows.
        tick(&mut app);
        let res = server.recv("completionItem/resolve").await;
        assert_eq!(res["params"]["data"], json!(2));
        let mut resolved = docs("Docs **here**");
        resolved["label"] = json!("pb");
        resolved["additionalTextEdits"] =
            json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "use a::pb;\n"}]);
        reply(&mut app, &mut server, &res, resolved).await;
        let p = app.completion.as_ref().unwrap();
        let d = p.docs.as_ref().expect("the docs");
        assert_eq!(d.item, 1);
        assert_eq!(d.lines.len(), 1);
        assert_eq!(d.lines[0].text, "Docs here");
        assert_eq!(d.lines[0].kind, crate::ui::hover::Kind::Text);
        // Accepting reuses the resolved item: no second resolve.
        sent(&app, &mut server).await;
        key(&mut app, KeyCode::Enter);
        assert_eq!(text(&app), "use a::pb;\nfn main() {\n    pb\n}\n");
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "completionItem/resolve"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_docs_resolve_leaves_the_item_for_accept_to_resolve() {
        let (mut app, mut server, dir) = setup("compl-docs-fail", true).await;
        typ(&mut app, "p");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, two_items()).await;
        let now = Instant::now();
        app.completion_docs_tick(now);
        app.completion_docs_tick(now + DOCS_DELAY);
        let res = server.recv("completionItem/resolve").await;
        assert_eq!(res["params"]["data"], json!(1));
        server.respond_error(res["id"].as_i64().unwrap(), ResponseError::CONTENT_MODIFIED, "modified");
        handle_next(&mut app, is_lsp).await;
        assert!(app.completion.as_ref().unwrap().resolved.is_empty());
        // Accepting resolves on its own and applies the auto-import.
        sent(&app, &mut server).await;
        key(&mut app, KeyCode::Enter);
        let res = server.recv("completionItem/resolve").await;
        let mut resolved = json!({"label": "pa"});
        resolved["additionalTextEdits"] =
            json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "use a::pa;\n"}]);
        reply(&mut app, &mut server, &res, resolved).await;
        assert_eq!(text(&app), "use a::pa;\nfn main() {\n    pa\n}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commit_characters_accept_only_when_armed() {
        let (mut app, dir) = test_app("compl-commit");
        let mut c = incremental();
        c["completionProvider"] = json!({"allCommitCharacters": ["("], "triggerCharacters": ["."]});
        let mut servers = with_fake_servers(&mut app, c);
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", MAIN).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        // Auto-opened and not moved in: `(` is only typed.
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([{"label": "foo"}])).await;
        assert!(!app.completion.as_ref().unwrap().armed);
        typ(&mut app, "(");
        assert_eq!(caret_line(&app), "    f(|)");
        while text(&app) != MAIN {
            app.perform(Action::Editor(EditorAction::Undo));
        }
        assert_eq!(caret_line(&app), "    |");
        // Asked for with Ctrl+Space: accepted, then `(` typed (auto-paired).
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([{"label": "foo"}])).await;
        typ(&mut app, "(");
        assert_eq!(caret_line(&app), "    foo(|)");
        while text(&app) != MAIN {
            app.perform(Action::Editor(EditorAction::Undo));
        }
        // Auto-opened, then the selection moved.
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([{"label": "foo"}])).await;
        // (The built-in `fn` snippet follows `foo`: down and back up.)
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Up);
        typ(&mut app, "(");
        assert_eq!(caret_line(&app), "    foo(|)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn server_items_come_before_snippets() {
        let (mut app, mut server, dir) = setup("compl-snippets-after", false).await;
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([{"label": "fnord"}, {"label": "fold"}])).await;
        let l = labels(&app);
        assert_eq!(l.len(), 3, "{l:?}");
        assert_eq!(l.last().unwrap(), "fn");
        assert!(l[..2].contains(&"fnord".to_string()) && l[..2].contains(&"fold".to_string()), "{l:?}");
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(text(&app), "fn main() {\n    fn name() {\n        \n    }\n}\n");
        assert_eq!(selected(&app), "name");
        app.perform(Action::Editor(EditorAction::Undo));
        app.perform(Action::Editor(EditorAction::Undo));
        while text(&app) != MAIN {
            app.perform(Action::Editor(EditorAction::Undo));
        }
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([])).await;
        assert_eq!(labels(&app), ["fn"], "an empty answer still lists the snippet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snippet_rows_have_no_commit_characters() {
        let (mut app, dir) = test_app("compl-snippets-commit");
        let mut c = incremental();
        c["completionProvider"] = json!({"allCommitCharacters": ["("], "triggerCharacters": ["."]});
        let mut servers = with_fake_servers(&mut app, c);
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", MAIN).await;
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([])).await;
        assert_eq!(labels(&app), ["fn"]);
        key(&mut app, KeyCode::Down);
        typ(&mut app, "(");
        assert_eq!(text(&app), "fn main() {\n    f()\n}\n", "typed, not expanded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snippet_rows_are_never_resolved() {
        let (mut app, mut server, dir) = setup("compl-snippets-resolve", true).await;
        typ(&mut app, "f");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, json!([{"label": "fnord"}])).await;
        key(&mut app, KeyCode::Down);
        let now = Instant::now();
        app.completion_docs_tick(now);
        app.completion_docs_tick(now + DOCS_DELAY);
        let docs = app.completion.as_ref().unwrap().docs.as_ref().expect("the docs panel");
        assert!(docs.lines[0].text.contains("fn name() {"), "{:?}", docs.lines[0].text);
        key(&mut app, KeyCode::Enter);
        assert!(text(&app).contains("fn name()"), "accepted at once: {}", text(&app));
        let sent = server.sent_so_far(&app.lsp.clients["rust-analyzer"]).await;
        assert!(sent.iter().all(|m| m["method"] != "completionItem/resolve"), "{sent:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn alt_up_down_scroll_the_docs() {
        let (mut app, mut server, dir) = setup("compl-docs-scroll", false).await;
        typ(&mut app, "p");
        let req = completion_req(&mut server).await;
        let long = (1..=40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        reply(&mut app, &mut server, &req, json!([{"label": "pa", "documentation": long}])).await;
        let now = Instant::now();
        app.completion_docs_tick(now);
        app.completion_docs_tick(now + DOCS_DELAY);
        draw(&mut app, 120, 40);
        assert!(app.completion.as_ref().unwrap().docs_shown());
        let before = text(&app);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
        assert_eq!(app.completion.as_ref().unwrap().docs.as_ref().unwrap().scroll, 1);
        assert_eq!(text(&app), before);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.completion.as_ref().unwrap().docs.as_ref().unwrap().scroll, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_popup_anchors_in_the_focused_group_and_closes_when_it_changes() {
        let (mut app, mut server, dir) = setup("compl-split", false).await;
        app.perform(Action::Editor(EditorAction::SplitRight));
        draw(&mut app, 100, 24);
        typ(&mut app, "p");
        let req = completion_req(&mut server).await;
        reply(&mut app, &mut server, &req, items(1, 4, 5)).await;
        app.toasts = Default::default();
        let rows = draw(&mut app, 100, 24);
        let (cx, cy) = app.editor.caret_cell().unwrap();
        assert!(cx > 66, "the caret of the right group");
        let x = rows[cy as usize + 2].chars().position(|c| c == 'p').unwrap() as u16;
        // Under the word, or moved left to fit the screen, but in the group.
        assert!(x > 66 && x < cx, "{x} {cx}");
        app.perform(Action::Editor(EditorAction::PrevGroup));
        draw(&mut app, 100, 24);
        assert!(app.completion.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
