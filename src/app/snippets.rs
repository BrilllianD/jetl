//! Live templates in the app: the snippets of `config::snippets` offered as
//! completion rows and expanded by Tab after their prefix.
//!
//! Tab precedence: an open popup accepts first, then an active snippet
//! session walks to its next tabstop, then an exact prefix expands
//! (`editor.snippet_prefix`), else `editor.tab` indents. Expansion needs the
//! editor focused, no find bar or merge view, Insert mode, one empty
//! selection, a writable buffer, the word before the caret (nothing
//! word-like right after it) equal to a prefix, and the prefix not inside a
//! string or comment (tree-sitter; plain text has none).
//!
//! A snippet becomes a [`CompletionItem`] with its `\t` turned into the
//! buffer's indent unit and later lines indented like the caret line, so
//! `completion_apply` accepts it like a server snippet: one undo step, and a
//! body with tabstops starts the snippet session.
//!
//! Surround With (`editor.surround_with`) lists the snippets whose body uses
//! `$TM_SELECTED_TEXT` or `$SELECTION` in a Choice dialog. The chosen one
//! replaces each selection's [`surround_range`] (an empty selection: the
//! caret line from its first non-blank char; whole lines: line-wise; else
//! the selection exactly) with the expanded body, the range's text
//! dedented by its line's indentation and re-indented by the template. It
//! is one transaction, so one undo step. One range starts the tabstop
//! session; several put a caret at each copy's `$0` and start none.

use std::cell::Cell;
use std::ops::Range;

use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemLabelDetails, InsertTextFormat};
use ropey::Rope;

use super::App;
use crate::config::snippets::Snippet;
use crate::dispatch::Mode;
use crate::editor::brackets::in_literal_at;
use crate::editor::buffer::{Buffer, BufferId, LineEnding};
use crate::editor::selection::{Selection, Selections, indent_of, is_word_char, line_len_no_eol};
use crate::editor::snippet::{SnippetSession, Tabstop};
use crate::editor::snippet_vars::SnippetVars;
use crate::editor::undo::TxKind;
use crate::lsp::completion::{CompletionState, Origin, parse_snippet, parse_snippet_with, snippet_text, word_start};
use crate::lsp::edits::{self, CharEdit};
use crate::ui::completion::CompletionPopup;
use crate::ui::dialog::{Dialog, DialogId};
use crate::ui::focus::PanelId;

/// Unix seconds now and the local offset east of UTC at that time.
pub(super) fn snippet_clock() -> (i64, i64) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    (secs, crate::ui::event_log::local_offset(secs))
}

/// Whether `pos` of `b` lies in a string or comment by its parse; false
/// without a grammar or a tree.
fn in_string_or_comment(b: &Buffer, pos: usize) -> bool {
    b.syntax.as_ref().and_then(|s| s.tree_at_char(&b.rope, pos)).is_some_and(|t| in_literal_at(&b.rope, t, pos))
}

/// `body` with each `\t` as `unit` and each later line indented by `indent`.
pub(super) fn adjust_body(body: &str, indent: &str, unit: &str) -> String {
    body.replace('\t', unit).replace('\n', &format!("\n{indent}"))
}

/// Whether `body` references `$TM_SELECTED_TEXT` or `$SELECTION` (by parsing, so `\$TM_SELECTED_TEXT` does not count).
pub(super) fn uses_selection(body: &str) -> bool {
    let seen = Cell::new(false);
    parse_snippet_with(
        body,
        &|n| {
            if n == "TM_SELECTED_TEXT" || n == "SELECTION" {
                seen.set(true);
            }
            None
        },
        "",
    );
    seen.get()
}

/// The text Surround With replaces for `sel`: no selection, the caret line
/// from its first non-blank char; a selection covering whole lines (from at
/// or before the first non-blank char of its first line to the end of its
/// last, which may be shown as column 0 of the next, or inside the leading
/// whitespace of the next, which counts as column 0), those lines; else the
/// selection. May be empty.
pub(super) fn surround_range(rope: &Rope, sel: &Selection) -> Range<usize> {
    let first = |l: usize| rope.line_to_char(l) + indent_of(rope, l);
    let end = |l: usize| rope.line_to_char(l) + line_len_no_eol(rope, l);
    let mut r = sel.range();
    let l0 = rope.char_to_line(r.start);
    if r.is_empty() {
        return first(l0)..end(l0);
    }
    let le = rope.char_to_line(r.end);
    if le > l0 && r.end > rope.line_to_char(le) && r.end <= first(le) {
        r.end = rope.line_to_char(le);
    }
    let mut l1 = rope.char_to_line(r.end);
    if l1 > l0 && r.end == rope.line_to_char(l1) {
        l1 -= 1;
    }
    if r.start <= first(l0) && r.end >= end(l1) { first(l0)..end(l1) } else { r }
}

/// `text` (line breaks normalised to `\n`) with each line after the first losing the leading chars it shares with `indent`.
pub(super) fn dedent(text: &str, indent: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let mut lines = text.split('\n');
    let mut out = lines.next().unwrap_or_default().to_string();
    for l in lines {
        let k = l.chars().zip(indent.chars()).take_while(|(a, b)| a == b).count();
        out.push('\n');
        out.extend(l.chars().skip(k));
    }
    out
}

/// The completion row of snippet `s`.
pub(super) fn snippet_item(s: &Snippet, indent: &str, unit: &str) -> CompletionItem {
    let body = adjust_body(&s.body, indent, unit);
    let description = if s.description.is_empty() { "snippet".to_string() } else { s.description.clone() };
    CompletionItem {
        label: s.prefix.clone(),
        kind: Some(CompletionItemKind::SNIPPET),
        label_details: Some(CompletionItemLabelDetails { detail: None, description: Some(description) }),
        detail: Some(parse_snippet(&body).0),
        filter_text: Some(s.prefix.clone()),
        insert_text: Some(body),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        commit_characters: Some(Vec::new()),
        ..Default::default()
    }
}

/// The word before the single empty caret of `b` and where it starts; none
/// when there is no such word or a word char follows the caret.
fn prefix_before_caret(b: &Buffer) -> Option<(usize, String)> {
    if b.view.sels.is_multi() || !b.view.sel().is_empty() {
        return None;
    }
    let head = b.view.sel().head;
    let start = word_start(&b.rope, head);
    if start == head || (head < b.rope.len_chars() && is_word_char(b.rope.char(head))) {
        return None;
    }
    Some((start, b.rope.slice(start..head).to_string()))
}

impl App {
    /// The snippet rows for buffer `id` at char `at`, indented like the line
    /// holding `at`.
    pub(super) fn snippet_items(&self, id: crate::editor::buffer::BufferId, at: usize) -> Vec<CompletionItem> {
        if !self.config.editor.snippets {
            return Vec::new();
        }
        let Some(b) = self.editor.index_of_id(id).map(|i| &self.editor.buffers[i]) else { return Vec::new() };
        let lang = b.language().map(|d| d.name);
        let at = at.min(b.rope.len_chars());
        let line = b.rope.line(b.rope.char_to_line(at));
        let indent: String = line.chars().take_while(|&c| c == ' ' || c == '\t').collect();
        let unit = b.indent.unit();
        self.snippets.for_language(lang).into_iter().map(|s| snippet_item(s, &indent, &unit)).collect()
    }

    /// The start of the exact snippet prefix before the caret of the active
    /// buffer when Tab could expand it (every gate but the string/comment one).
    fn snippet_prefix_start(&self) -> Option<usize> {
        if !self.config.editor.snippets {
            return None;
        }
        if !(self.focus.is(PanelId::Editor) && !self.editor.find_focused() && !self.editor.merge_active() && self.mode == Mode::Insert) {
            return None;
        }
        let b = self.editor.active_buffer().filter(|b| !b.read_only)?;
        let (start, p) = prefix_before_caret(b)?;
        self.snippets.exact(b.language().map(|d| d.name), &p).map(|_| start)
    }

    /// Bring the active buffer's parse up to date when an exact prefix sits
    /// before the caret, so `contexts()` (`&self`) sees a fresh tree. Cheap: a
    /// no-op on a clean tree, else one incremental parse, and only right after
    /// a prefix; the next draw runs the same parse.
    pub(super) fn snippet_parse(&mut self) {
        if self.snippet_prefix_start().is_none() {
            return;
        }
        if let Some(b) = self.editor.active_buffer_mut()
            && let Some(s) = b.syntax.as_mut()
        {
            s.reparse(&b.rope);
        }
    }

    /// Whether Tab would expand a snippet here (`editor.snippet_prefix`): an
    /// exact prefix whose first char is not in a string or comment. Reads the
    /// cached tree, which `snippet_parse` refreshed for this key. Runs on
    /// every key: one word scan and a lookup.
    pub(super) fn snippet_prefix_here(&self) -> bool {
        self.snippet_prefix_start().is_some_and(|start| self.editor.active_buffer().is_some_and(|b| !in_string_or_comment(b, start)))
    }

    /// `editor.expand_snippet`: insert the snippet whose prefix is the word
    /// before the caret.
    pub(super) fn expand_snippet(&mut self) {
        if !self.config.editor.snippets {
            self.lsp_hint("Live templates are off ([editor] snippets = false)");
            return;
        }
        let found = (|| {
            if !self.focus.is(PanelId::Editor) || self.editor.find_focused() || self.editor.merge_active() || self.mode != Mode::Insert {
                return None;
            }
            let b = self.editor.active_buffer().filter(|b| !b.read_only)?;
            let (start, prefix) = prefix_before_caret(b)?;
            let item = self.snippet_items(b.id, start).into_iter().find(|i| i.label == prefix)?;
            Some((Origin::new(b.id, &b.rope, b.view.sel().head), item))
        })();
        let Some((origin, item)) = found else {
            self.lsp_hint("No snippet for the word before the caret");
            return;
        };
        self.completion_close();
        self.completion_apply(&origin, &item);
    }

    /// `editor.surround_with`: ask which live template wraps the selection.
    pub(super) fn surround_with(&mut self) {
        self.hover = None;
        self.completion_close();
        if !self.config.editor.snippets {
            return self.lsp_hint("Live templates are off ([editor] snippets = false)");
        }
        let ready =
            self.focus.is(PanelId::Editor) && !self.editor.find_focused() && !self.editor.merge_active() && self.mode == Mode::Insert;
        let Some(b) = self.editor.active_buffer().filter(|b| ready && !b.read_only) else {
            return self.lsp_hint("Surround With needs a writable editor");
        };
        let found: Vec<&Snippet> =
            self.snippets.for_language(b.language().map(|d| d.name)).into_iter().filter(|s| uses_selection(&s.body)).collect();
        if found.is_empty() {
            return self.lsp_hint("No live template uses $TM_SELECTED_TEXT here");
        }
        if b.view.sels.iter().all(|s| surround_range(&b.rope, s).is_empty()) {
            return self.lsp_hint("Nothing to surround");
        }
        let rows = found
            .iter()
            .map(|s| if s.description.is_empty() { s.prefix.clone() } else { format!("{}  {}", s.prefix, s.description) })
            .collect();
        let prefixes = found.iter().map(|s| s.prefix.clone()).collect();
        self.dialog =
            Some(Dialog::choice(DialogId::SurroundWith { buffer: b.id, version: b.version, prefixes }, "Surround With", "", rows));
        self.dispatcher.reset();
    }

    /// The Surround With dialog chose snippet `prefix`: wrap, unless the
    /// buffer changed since the dialog opened.
    pub(super) fn surround_chosen(&mut self, buffer: BufferId, version: u64, prefix: Option<String>) {
        let Some(prefix) = prefix else { return };
        let Some(b) = self.editor.active_buffer().filter(|b| b.id == buffer && b.version == version) else { return };
        let Some(s) = self.snippets.exact(b.language().map(|d| d.name), &prefix).cloned() else { return };
        self.surround_apply(&s);
    }

    /// Replace each selection's surround range with the expansion of `s`, in
    /// one transaction.
    fn surround_apply(&mut self, s: &Snippet) {
        let group = self.editor.focused_group().id;
        let (now, offset) = snippet_clock();
        let root = self.workspace.root.clone();
        let Some(b) = self.editor.active_buffer() else { return };
        let crlf = b.line_ending == LineEnding::CrLf;
        let unit = b.indent.unit();
        let mut kept: Vec<(usize, Range<usize>)> = Vec::new();
        for (i, sel) in b.view.sels.iter().enumerate() {
            let r = surround_range(&b.rope, sel);
            if !r.is_empty() && kept.last().is_none_or(|(_, p)| *p != r) {
                kept.push((i, r));
            }
        }
        let mut edits: Vec<CharEdit> = Vec::new();
        let mut stops_at: Vec<(usize, Vec<Tabstop>)> = Vec::new();
        for (k, (_, r)) in kept.iter().enumerate() {
            let line = b.rope.char_to_line(r.start);
            let indent: String = b.rope.line(line).chars().take_while(|&c| c == ' ' || c == '\t').collect();
            let selected = dedent(&b.rope.slice(r.clone()).to_string(), &indent);
            let body = adjust_body(&s.body, &indent, &unit);
            let vars = SnippetVars { selected, cursor: k, ..SnippetVars::new(b, &root, r.clone(), now, offset) };
            let (text, at, stops) = snippet_text(&body, &|n| vars.get(n), &indent, crlf);
            edits.push(CharEdit::new(r.start, r.end, text));
            stops_at.push((at, stops));
        }
        let Ok(plan) = edits::plan(&b.rope, &edits) else {
            return self.lsp_hint("Surround With: the selections overlap");
        };
        let mut session = None;
        let sels = if kept.len() == 1 && !b.view.sels.is_multi() {
            let (at, stops) = stops_at.remove(0);
            let shift = plan.starts[0];
            let stops: Vec<Tabstop> =
                stops.into_iter().map(|t| Tabstop { range: t.range.start + shift..t.range.end + shift, ..t }).collect();
            session = SnippetSession::new(b.id, group, stops);
            match &session {
                Some(s) => s.selections(),
                None => Selections::point(shift + at),
            }
        } else {
            let mut ranges: Vec<Selection> = Vec::new();
            let mut primary = None;
            let mut next = 0;
            for (i, sel) in b.view.sels.iter().enumerate() {
                if kept.get(next).is_some_and(|(j, _)| *j == i) {
                    ranges.push(Selection::point(plan.starts[next] + stops_at[next].0));
                    next += 1;
                } else if let Some(k) = kept.iter().position(|(_, r)| r.start <= sel.head && sel.head <= r.end) {
                    ranges.push(Selection::point(plan.starts[k] + stops_at[k].0));
                } else {
                    ranges.push(Selection::new(edits::map_pos(&plan.changes, sel.anchor), edits::map_pos(&plan.changes, sel.head)));
                }
                if i == b.view.sels.primary_index() {
                    primary = Some(ranges.len() - 1);
                }
            }
            // A selection without a copy of its own (a blank line) maps through the edits; one inside a kept range goes to that copy's caret.
            Selections::new(ranges, primary.unwrap_or(0))
        };
        let Some(b) = self.editor.active_buffer_mut() else { return };
        let before = b.version;
        b.apply(plan.changes, sels, TxKind::Other);
        if let Some(mut s) = session.filter(|_| b.version != before) {
            s.attach(b);
            self.editor.set_snippet(Some(s));
        }
        self.dirty_frame = true;
    }

    /// Expand a file template into the active, just-opened buffer as one
    /// transaction; its tabstops start a snippet session.
    pub(super) fn template_apply(&mut self, body: &str) {
        let group = self.editor.focused_group().id;
        let (now, offset) = snippet_clock();
        let root = self.workspace.root.clone();
        let Some(b) = self.editor.active_buffer().filter(|b| !b.read_only) else { return };
        let crlf = b.line_ending == LineEnding::CrLf;
        let at_end = b.rope.len_chars();
        let vars = SnippetVars::new(b, &root, at_end..at_end, now, offset);
        let (text, at, stops) = snippet_text(body, &|n| vars.get(n), "", crlf);
        if text.is_empty() {
            return;
        }
        let Ok(plan) = edits::plan(&b.rope, &[CharEdit::new(at_end, at_end, text)]) else { return };
        let shift = plan.starts[0];
        let stops: Vec<Tabstop> = stops.into_iter().map(|t| Tabstop { range: t.range.start + shift..t.range.end + shift, ..t }).collect();
        let session = SnippetSession::new(b.id, group, stops);
        let sels = match &session {
            Some(s) => s.selections(),
            None => Selections::point(shift + at),
        };
        let Some(b) = self.editor.active_buffer_mut() else { return };
        let before = b.version;
        b.apply(plan.changes, sels, TxKind::Other);
        if let Some(mut s) = session.filter(|_| b.version != before) {
            s.attach(b);
            self.editor.set_snippet(Some(s));
        }
        self.dirty_frame = true;
    }

    /// Open a popup of the snippets matching the word at the caret, for
    /// Ctrl+Space without a server. False when none matches.
    pub(super) fn snippet_popup(&mut self, origin: Origin) -> bool {
        let Some(b) = self.editor.active_buffer().filter(|b| b.id == origin.buffer) else { return false };
        let Some(prefix) = origin.prefix(&b.rope, b.view.sel().head) else { return false };
        let items = self.snippet_items(origin.buffer, origin.start);
        let mut state = CompletionState::new(origin, Vec::new(), false, &prefix, &mut self.lsp.completion.matcher);
        state.add_snippets(items, &mut self.lsp.completion.matcher);
        if state.order.is_empty() {
            return false;
        }
        self.hover = None;
        let mut popup = CompletionPopup::new(state, self.editor.focused_group().id);
        popup.armed = true;
        self.completion = Some(popup);
        self.dirty_frame = true;
        true
    }
}

#[cfg(test)]
mod surround_tests {
    use super::*;
    use crate::editor::selection::line_col_to_pos;

    const BODY: &str = "fn main() {\n    a();\n    b();\n}\n";

    fn range(text: &str, from: (usize, usize), to: (usize, usize)) -> String {
        let rope = Rope::from_str(text);
        let sel = Selection::new(line_col_to_pos(&rope, from.0, from.1), line_col_to_pos(&rope, to.0, to.1));
        rope.slice(surround_range(&rope, &sel)).to_string()
    }

    #[test]
    fn surround_range_rules() {
        assert_eq!(range(BODY, (1, 6), (1, 6)), "a();", "a caret takes its line");
        assert_eq!(range(BODY, (1, 0), (3, 0)), "a();\n    b();", "whole lines, end at column 0");
        assert_eq!(range(BODY, (1, 4), (2, 10)), "a();\n    b();", "whole lines, end at the line end");
        assert_eq!(range(BODY, (1, 4), (1, 7)), "a()", "an inline selection is exact");
        assert_eq!(range(BODY, (1, 6), (2, 5)), ");\n    b", "a partial multi-line selection is exact");
        assert_eq!(range("x\n\n", (1, 0), (1, 0)), "", "a blank line has nothing");
        assert_eq!(range(BODY, (1, 4), (2, 1)), "a();", "whole lines, end at column 1 of the next line's indent");
        assert_eq!(range(BODY, (1, 4), (2, 4)), "a();", "whole lines, end at the next line's first non-blank");
        assert_eq!(range(BODY, (1, 0), (2, 2)), "a();", "from column 0, end inside the next line's indent");
        assert_eq!(range(BODY, (1, 6), (2, 2)), ");\n", "a mid-line start ends at the next line's start");
        assert_eq!(range(BODY, (1, 1), (1, 3)), "  ", "a selection inside one line's indent is exact");
    }

    #[test]
    fn dedent_drops_the_shared_indent() {
        assert_eq!(dedent("a();\n    b();\n  c\r\nd", "    "), "a();\nb();\nc\nd");
    }

    #[test]
    fn uses_selection_by_parsing() {
        for yes in ["x$TM_SELECTED_TEXT", "${SELECTION:d}", "${1:$TM_SELECTED_TEXT}"] {
            assert!(uses_selection(yes), "{yes}");
        }
        for no in ["\\$TM_SELECTED_TEXT", "$TM_SELECTED_TEXTX", "$0"] {
            assert!(!uses_selection(no), "{no}");
        }
    }
}

#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::actions::{Action, EditorAction};
    use crate::app::tests::{load, test_app};
    use crate::config::FileText;
    use crate::config::snippets::SnippetSet;
    use crate::editor::selection::{line_col_to_pos, pos_to_line_col};
    use crate::ui::dialog::DialogResult;

    const MAIN: &str = "fn main() {\n    \n}\n";

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn typ(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    fn ctrl_space(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
    }

    fn text(app: &App) -> String {
        app.editor.active_buffer().unwrap().text()
    }

    fn selected(app: &App) -> String {
        let b = app.editor.active_buffer().unwrap();
        b.rope.slice(b.view.sel().range()).to_string()
    }

    fn caret_line(app: &App) -> String {
        let b = app.editor.active_buffer().unwrap();
        let (line, col) = pos_to_line_col(&b.rope, b.view.sel().head);
        let mut l: Vec<char> = b.line_text(line).chars().collect();
        l.insert(col, '|');
        l.into_iter().collect()
    }

    fn labels(app: &App) -> Vec<String> {
        let s = &app.completion.as_ref().expect("the popup").state;
        s.order.iter().map(|&i| s.items[i].label.clone()).collect()
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    fn undo(app: &mut App) {
        app.perform(Action::Editor(EditorAction::Undo));
    }

    /// An app with `a.rs` open and the caret on its blank line.
    fn rust_app(name: &str) -> (App, PathBuf) {
        let (mut app, dir) = test_app(name);
        load(&mut app, "a.rs", MAIN);
        app.focus.set(PanelId::Editor);
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        (app, dir)
    }

    fn user_snippets(file: &str, text: &str) -> SnippetSet {
        SnippetSet::build(&[(PathBuf::from(file), FileText::Text(text.into()))]).0
    }

    #[test]
    fn adjust_body_indents_later_lines() {
        assert_eq!(adjust_body("a {\n\t$0\n}", "    ", "  "), "a {\n      $0\n    }");
    }

    #[tokio::test]
    async fn tab_expands_an_exact_prefix_with_tabstops() {
        let (mut app, dir) = rust_app("snip-tab-expand");
        typ(&mut app, "fn");
        assert!(app.contexts().contains(&"editor.snippet_prefix"));
        key(&mut app, KeyCode::Tab);
        assert_eq!(text(&app), "fn main() {\n    fn name() {\n        \n    }\n}\n");
        assert_eq!(selected(&app), "name");
        assert!(app.contexts().contains(&"editor.snippet"));
        typ(&mut app, "go");
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "    fn go(|) {");
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "        |");
        assert!(app.editor.snippet.is_none());
        undo(&mut app);
        assert_eq!(text(&app), "fn main() {\n    fn name() {\n        \n    }\n}\n", "typing over `name` is its own step");
        undo(&mut app);
        assert_eq!(text(&app), "fn main() {\n    fn\n}\n", "the expansion is one undo step");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tab_expansion_resolves_variables() {
        let (mut app, dir) = rust_app("snip-vars");
        app.snippets = user_snippets(
            "rust.toml",
            "[[snippet]]\nprefix = \"hdr\"\nbody = '// $TM_FILENAME_BASE:$TM_LINE_NUMBER ${NOPE:dflt} $CURRENT_YEAR$0'\n",
        );
        typ(&mut app, "hdr");
        key(&mut app, KeyCode::Tab);
        let t = text(&app);
        let line = t.lines().nth(1).unwrap();
        let year = line.strip_prefix("    // a:2 dflt ").unwrap_or_else(|| panic!("{line:?}"));
        assert!(year.len() == 4 && year.bytes().all(|c| c.is_ascii_digit()), "{line:?}");
        undo(&mut app);
        assert_eq!(text(&app), MAIN.replace("    \n", "    hdr\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tab_without_an_exact_prefix_indents() {
        // (a) not a prefix.
        let (mut app, dir) = rust_app("snip-tab-a");
        typ(&mut app, "fnx");
        let before = text(&app).len();
        key(&mut app, KeyCode::Tab);
        assert!(text(&app).starts_with("fn main() {\n    fnx"));
        assert!(!text(&app).contains("name"));
        assert!(text(&app).len() > before, "editor.tab inserted indentation");
        let _ = std::fs::remove_dir_all(&dir);
        // (b) a word char right after the caret.
        let (mut app, dir) = rust_app("snip-tab-b");
        typ(&mut app, "fnz");
        key(&mut app, KeyCode::Left);
        assert!(!app.contexts().contains(&"editor.snippet_prefix"));
        key(&mut app, KeyCode::Tab);
        assert!(!text(&app).contains("name"));
        let _ = std::fs::remove_dir_all(&dir);
        // (c) a selection.
        let (mut app, dir) = rust_app("snip-tab-c");
        typ(&mut app, "fn");
        for _ in 0..2 {
            app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        }
        assert_eq!(selected(&app), "fn");
        assert!(!app.contexts().contains(&"editor.snippet_prefix"));
        let _ = std::fs::remove_dir_all(&dir);
        // (d) two carets.
        let (mut app, dir) = rust_app("snip-tab-d");
        app.perform(Action::Editor(EditorAction::CloneCaretBelow));
        typ(&mut app, "fn");
        assert!(app.editor.active_buffer().unwrap().view.sels.is_multi());
        assert!(!app.contexts().contains(&"editor.snippet_prefix"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tab_precedence() {
        let (mut app, dir) = rust_app("snip-tab-precedence");
        app.snippets = user_snippets(
            "/u/rust.toml",
            "[[snippet]]\nprefix = \"fn\"\nbody = \"FN()\"\n[[snippet]]\nprefix = \"fnx\"\nbody = \"FNX()\"\n[[snippet]]\nprefix = \"s2\"\nbody = \"a(${1:x}, ${2:y})\"\n",
        );
        // (1) the prefix over indentation.
        typ(&mut app, "fn");
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "    FN()|");
        undo(&mut app);
        // (2) the popup over the prefix.
        ctrl_space(&mut app);
        assert_eq!(labels(&app), ["fn", "fnx"]);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "    FNX()|");
        undo(&mut app);
        undo(&mut app);
        assert_eq!(text(&app), MAIN);
        // (3) a session over the prefix.
        typ(&mut app, "s2");
        key(&mut app, KeyCode::Tab);
        assert_eq!(text(&app), "fn main() {\n    a(x, y)\n}\n");
        assert_eq!(selected(&app), "x");
        typ(&mut app, "fn");
        let ctx = app.contexts();
        let at = |c| ctx.iter().position(|&x| x == c).unwrap();
        assert!(at("editor.snippet") < at("editor.snippet_prefix"));
        key(&mut app, KeyCode::Tab);
        assert_eq!(selected(&app), "y");
        assert_eq!(text(&app), "fn main() {\n    a(fn, y)\n}\n");
        // (4) the popup over the session: typing `fn` replaces `x` again.
        for _ in 0..5 {
            if text(&app) != "fn main() {\n    s2\n}\n" {
                undo(&mut app);
            }
        }
        key(&mut app, KeyCode::Tab);
        assert_eq!(selected(&app), "x", "{:?}", text(&app));
        typ(&mut app, "fn");
        ctrl_space(&mut app);
        key(&mut app, KeyCode::Tab);
        assert_eq!(text(&app), "fn main() {\n    a(FN(), y)\n}\n");
        assert!(app.editor.snippet.is_some());
        key(&mut app, KeyCode::Tab);
        assert_eq!(selected(&app), "y");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ctrl_space_without_a_server_lists_matching_snippets() {
        let (mut app, dir) = rust_app("snip-ctrl-space");
        typ(&mut app, "ma");
        ctrl_space(&mut app);
        assert_eq!(labels(&app), ["match"]);
        key(&mut app, KeyCode::Enter);
        assert!(text(&app).contains("\n    match value {\n"), "{:?}", text(&app));
        assert_eq!(selected(&app), "value");
        key(&mut app, KeyCode::Esc);
        app.editor.active_buffer_mut().unwrap().goto_line(3, 0);
        app.toasts = Default::default();
        typ(&mut app, "zz");
        ctrl_space(&mut app);
        assert!(app.completion.is_none());
        assert!(toasts(&app).iter().any(|t| t.starts_with("Language servers are off")), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn expand_action_without_a_snippet_hints() {
        let (mut app, dir) = rust_app("snip-expand-hint");
        typ(&mut app, "zz");
        let before = text(&app);
        app.perform(Action::Editor(EditorAction::ExpandSnippet));
        assert_eq!(text(&app), before);
        assert!(toasts(&app).contains(&"No snippet for the word before the caret".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plain_text_gets_only_all_toml() {
        let (mut app, dir) = test_app("snip-plain");
        app.snippets = user_snippets("/u/all.toml", "[[snippet]]\nprefix = \"sig\"\nbody = \"SIG\"\n");
        load(&mut app, "a.txt", "\n");
        app.focus.set(PanelId::Editor);
        app.editor.active_buffer_mut().unwrap().goto_line(0, 0);
        typ(&mut app, "fn");
        key(&mut app, KeyCode::Tab);
        assert!(!text(&app).contains("name"));
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        typ(&mut app, "sig");
        key(&mut app, KeyCode::Tab);
        assert_eq!(text(&app).lines().next(), Some("SIG"));
        load(&mut app, "a.rs", MAIN);
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        typ(&mut app, "sig");
        key(&mut app, KeyCode::Tab);
        assert_eq!(text(&app), "fn main() {\n    SIG\n}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const LITERALS: &str = "fn main() {\n    let s = \"\";\n    // \n    \n}\n";

    fn literals_app(name: &str) -> (App, PathBuf) {
        let (mut app, dir) = test_app(name);
        load(&mut app, "a.rs", LITERALS);
        app.focus.set(PanelId::Editor);
        (app, dir)
    }

    #[tokio::test]
    async fn tab_does_not_expand_in_strings_or_comments() {
        let (mut app, dir) = literals_app("snip-literals");
        // (a) inside a string: indents.
        app.editor.active_buffer_mut().unwrap().goto_line(1, 13);
        typ(&mut app, "fn");
        assert!(!app.contexts().contains(&"editor.snippet_prefix"));
        let before = text(&app).len();
        key(&mut app, KeyCode::Tab);
        assert!(!text(&app).contains("fn name"), "{:?}", text(&app));
        assert!(text(&app).lines().nth(1).unwrap().starts_with("    let s = \"fn"));
        assert!(text(&app).len() > before, "editor.tab inserted indentation");
        // (b) inside a comment.
        app.editor.active_buffer_mut().unwrap().goto_line(2, 7);
        typ(&mut app, "fn");
        key(&mut app, KeyCode::Tab);
        assert!(!text(&app).contains("fn name"), "{:?}", text(&app));
        // (c) plain code expands.
        app.editor.active_buffer_mut().unwrap().goto_line(3, 4);
        typ(&mut app, "fn");
        key(&mut app, KeyCode::Tab);
        assert!(text(&app).contains("    fn name() {"), "{:?}", text(&app));
        assert_eq!(selected(&app), "name");
        let _ = std::fs::remove_dir_all(&dir);
        // (d) Ctrl+Space still lists the snippet inside a string.
        let (mut app, dir) = literals_app("snip-literals-popup");
        app.editor.active_buffer_mut().unwrap().goto_line(1, 13);
        typ(&mut app, "fn");
        ctrl_space(&mut app);
        assert!(labels(&app).contains(&"fn".to_string()), "{:?}", labels(&app));
        let _ = std::fs::remove_dir_all(&dir);
        // (e) the explicit action still expands there.
        let (mut app, dir) = literals_app("snip-literals-action");
        app.editor.active_buffer_mut().unwrap().goto_line(1, 13);
        typ(&mut app, "fn");
        app.perform(Action::Editor(EditorAction::ExpandSnippet));
        assert!(text(&app).contains("fn name()"), "{:?}", text(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-md")]
    #[tokio::test]
    async fn tab_expands_in_markdown_prose() {
        let (mut app, dir) = test_app("snip-md-prose");
        app.snippets = user_snippets("/u/all.toml", "[[snippet]]\nprefix = \"sig\"\nbody = \"SIG\"\n");
        load(&mut app, "a.md", "# Title\n\nsome words \n");
        app.focus.set(PanelId::Editor);
        assert!(app.editor.active_buffer().unwrap().syntax.is_some(), "markdown has a grammar");
        app.editor.active_buffer_mut().unwrap().goto_line(2, 11);
        typ(&mut app, "sig");
        key(&mut app, KeyCode::Tab);
        assert_eq!(text(&app), "# Title\n\nsome words SIG\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn crlf_buffer_gets_crlf_breaks() {
        let (mut app, dir) = test_app("snip-crlf");
        load(&mut app, "b.rs", "fn main() {\r\n    \r\n}\r\n");
        app.focus.set(PanelId::Editor);
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        typ(&mut app, "fn");
        key(&mut app, KeyCode::Tab);
        assert!(text(&app).contains("fn name() {\r\n        \r\n    }"), "{:?}", text(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn expand_snippet_needs_insert_mode() {
        let (mut app, dir) = rust_app("snip-normal");
        typ(&mut app, "fn");
        app.mode = Mode::Normal;
        app.perform(Action::Editor(EditorAction::ExpandSnippet));
        assert_eq!(text(&app), "fn main() {\n    fn\n}\n");
        assert!(toasts(&app).contains(&"No snippet for the word before the caret".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "lang-rust")]
    #[tokio::test]
    async fn snippets_key_turns_live_templates_off() {
        use crate::config::watch::ConfigFile;
        use crate::event::AppMsg;
        let (mut app, dir) = rust_app("snip-key-off");
        let set = |app: &mut App, on: bool| {
            std::fs::write(app.paths.config_file(), format!("[editor]\nsnippets = {on}\n[lsp]\nenabled = false\n")).unwrap();
            app.handle_msg(AppMsg::ConfigChanged { files: vec![ConfigFile::Config] });
        };
        assert_eq!(crate::config::reload::class_of("editor.snippets"), Some(crate::config::reload::ReloadClass::Live));
        set(&mut app, false);
        typ(&mut app, "fn");
        assert!(!app.contexts().contains(&"editor.snippet_prefix"));
        ctrl_space(&mut app);
        assert!(app.completion.is_none());
        let before = text(&app).len();
        key(&mut app, KeyCode::Tab);
        assert!(!text(&app).contains("name") && text(&app).len() > before, "Tab indents: {:?}", text(&app));
        assert!(toasts(&app).iter().all(|t| !t.starts_with("Live templates")), "Tab did not try to expand");
        undo(&mut app);
        app.perform(Action::Editor(EditorAction::ExpandSnippet));
        assert!(toasts(&app).contains(&"Live templates are off ([editor] snippets = false)".to_string()), "{:?}", toasts(&app));
        set(&mut app, true);
        undo(&mut app);
        typ(&mut app, "fn");
        key(&mut app, KeyCode::Tab);
        assert!(text(&app).contains("fn name()"), "{:?}", text(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    const TWO: &str = "fn main() {\n    a();\n    b();\n}\n";

    fn load_two(app: &mut App) {
        load(app, "a.rs", TWO);
        app.focus.set(PanelId::Editor);
    }

    fn select(app: &mut App, from: (usize, usize), to: (usize, usize)) {
        let b = app.editor.active_buffer_mut().unwrap();
        let (a, h) = (line_col_to_pos(&b.rope, from.0, from.1), line_col_to_pos(&b.rope, to.0, to.1));
        b.view.set_sel(Selection::new(a, h));
    }

    /// Run Surround With and choose `prefix`; the offered prefixes.
    fn surround(app: &mut App, prefix: &str) -> Vec<String> {
        app.perform(Action::Editor(EditorAction::SurroundWith));
        let d = app.dialog.take().expect("the dialog");
        let DialogId::SurroundWith { ref prefixes, .. } = d.id else { panic!("{:?}", d.id) };
        let prefixes = prefixes.clone();
        let i = prefixes.iter().position(|p| p == prefix).expect("the prefix");
        app.finish_dialog(d.id, DialogResult::Choice(i));
        prefixes
    }

    const SM: &str = "[[snippet]]\nprefix = \"sm\"\nbody = 'Some($TM_SELECTED_TEXT)'\n";

    #[tokio::test]
    async fn surround_wraps_selected_lines_in_one_undo_step() {
        let (mut app, dir) = test_app("surround-lines");
        load_two(&mut app);
        select(&mut app, (1, 0), (3, 0));
        assert_eq!(surround(&mut app, "if"), ["for", "if", "ifelse", "loop", "unsafe", "while"]);
        assert_eq!(text(&app), "fn main() {\n    if condition {\n        a();\n        b();\n    }\n}\n");
        assert_eq!(selected(&app), "condition");
        assert!(app.editor.snippet.is_some());
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "        b();|");
        undo(&mut app);
        assert_eq!(text(&app), TWO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn surround_selection_ending_in_the_next_indent_keeps_the_level() {
        if crate::editor::syntax::language_for_name("go").is_none() {
            return;
        }
        let (mut app, dir) = test_app("surround-indent-end");
        let src = "func main() {\n\ta()\n\tb()\n}\n";
        load(&mut app, "a.go", src);
        app.focus.set(PanelId::Editor);
        select(&mut app, (1, 1), (2, 1));
        surround(&mut app, "if");
        assert_eq!(text(&app), "func main() {\n\tif condition {\n\t\ta()\n\t}\n\tb()\n}\n");
        app.editor.set_snippet(None);
        undo(&mut app);
        assert_eq!(text(&app), src);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (stem, file, text before, indent, first line, second line, text after)
    type Sample = (&'static str, &'static str, &'static str, &'static str, &'static str, &'static str, &'static str);
    /// (prefix, head line, tail after the body)
    type Case = (&'static str, &'static str, &'static str);

    const TS_CASES: &[Case] = &[
        ("for", "for (const item of items) {", "}"),
        ("if", "if (condition) {", "}"),
        ("ifelse", "if (condition) {", "} else {\n        \n    }"),
        ("try", "try {", "} catch (e) {\n        \n    }"),
        ("while", "while (condition) {", "}"),
    ];

    #[tokio::test]
    async fn every_builtin_surround_template_wraps_a_selection() {
        let ts: Sample = ("typescript", "a.ts", "function main() {\n", "    ", "a();", "b();", "}\n");
        let samples: [(Sample, &[Case]); 5] = [
            (
                ("rust", "a.rs", "fn main() {\n", "    ", "a();", "b();", "}\n"),
                &[
                    ("for", "for item in iter {", "}"),
                    ("if", "if condition {", "}"),
                    ("ifelse", "if condition {", "} else {\n        \n    }"),
                    ("loop", "loop {", "}"),
                    ("unsafe", "unsafe {", "}"),
                    ("while", "while condition {", "}"),
                ],
            ),
            (
                ("python", "a.py", "def main():\n", "    ", "a()", "b()", "\nmain()\n"),
                &[
                    ("for", "for item in items:", ""),
                    ("if", "if condition:", ""),
                    ("ifelse", "if condition:", "else:\n        pass"),
                    ("try", "try:", "except Exception as e:\n        raise"),
                    ("while", "while condition:", ""),
                    ("with", "with expr as name:", ""),
                ],
            ),
            (ts, TS_CASES),
            (("javascript", "a.js", ts.2, ts.3, ts.4, ts.5, ts.6), TS_CASES),
            (
                ("go", "a.go", "func main() {\n", "\t", "a()", "b()", "}\n"),
                &[
                    ("for", "for condition {", "}"),
                    ("forr", "for _, v := range items {", "}"),
                    ("if", "if condition {", "}"),
                    ("ifelse", "if condition {", "} else {\n\t\t\n\t}"),
                ],
            ),
        ];
        for ((stem, file, pre, ind, a, b, post), cases) in samples {
            if crate::editor::syntax::language_for_name(stem).is_none() {
                continue;
            }
            let (mut app, dir) = test_app(&format!("surround-builtin-{stem}"));
            let original = format!("{pre}{ind}{a}\n{ind}{b}\n{post}");
            load(&mut app, file, &original);
            app.focus.set(PanelId::Editor);
            let mut want: Vec<&str> = cases.iter().map(|c| c.0).collect();
            want.sort_unstable();
            let mut have: Vec<String> =
                app.snippets.for_language(Some(stem)).iter().filter(|s| uses_selection(&s.body)).map(|s| s.prefix.clone()).collect();
            have.sort();
            assert_eq!(have, want, "{stem}: built-in surround templates without a table row");
            for (prefix, head, tail) in cases {
                select(&mut app, (1, 0), (3, 0));
                let offered = surround(&mut app, prefix);
                assert_eq!(offered, want, "{stem} {prefix}: offered");
                let tail = if tail.is_empty() { String::new() } else { format!("{ind}{tail}\n") };
                let expected = format!("{pre}{ind}{head}\n{ind}{ind}{a}\n{ind}{ind}{b}\n{tail}{post}");
                assert_eq!(text(&app), expected, "{stem} {prefix}");
                app.editor.set_snippet(None);
                undo(&mut app);
                assert_eq!(text(&app), original, "{stem} {prefix}: one undo");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn surround_wraps_an_inline_selection_exactly() {
        let (mut app, dir) = test_app("surround-inline");
        app.snippets = user_snippets("rust.toml", SM);
        load(&mut app, "a.rs", "fn main() {\n    let x = foo;\n}\n");
        app.focus.set(PanelId::Editor);
        select(&mut app, (1, 12), (1, 15));
        surround(&mut app, "sm");
        assert_eq!(text(&app), "fn main() {\n    let x = Some(foo);\n}\n");
        assert_eq!(caret_line(&app), "    let x = Some(foo)|;");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn surround_without_a_selection_takes_the_caret_line() {
        let (mut app, dir) = test_app("surround-caret");
        load_two(&mut app);
        app.editor.active_buffer_mut().unwrap().goto_line(1, 6);
        surround(&mut app, "if");
        assert_eq!(text(&app), "fn main() {\n    if condition {\n        a();\n    }\n    b();\n}\n");
        let _ = std::fs::remove_dir_all(&dir);
        let (mut app, dir) = rust_app("surround-blank");
        let before = text(&app);
        app.perform(Action::Editor(EditorAction::SurroundWith));
        assert!(app.dialog.is_none());
        assert_eq!(text(&app), before);
        assert!(toasts(&app).contains(&"Nothing to surround".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn surround_at_several_carets() {
        let (mut app, dir) = test_app("surround-multi");
        app.snippets = user_snippets("rust.toml", SM);
        load_two(&mut app);
        app.editor.active_buffer_mut().unwrap().goto_line(1, 4);
        app.perform(Action::Editor(EditorAction::CloneCaretBelow));
        surround(&mut app, "sm");
        assert_eq!(text(&app), "fn main() {\n    Some(a();)\n    Some(b();)\n}\n");
        let b = app.editor.active_buffer().unwrap();
        let heads: Vec<usize> = b.view.sels.iter().map(|s| s.head).collect();
        assert_eq!(heads.len(), 2);
        assert!(b.view.sels.iter().all(|s| s.is_empty()));
        assert_eq!(heads, [b.rope.line_to_char(1) + 14, b.rope.line_to_char(2) + 14]);
        assert!(app.editor.snippet.is_none());
        undo(&mut app);
        assert_eq!(text(&app), TWO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn surround_two_carets_on_one_line_share_one_copy() {
        let (mut app, dir) = test_app("surround-same-line");
        app.snippets = user_snippets("rust.toml", SM);
        load_two(&mut app);
        let b = app.editor.active_buffer_mut().unwrap();
        let l = b.rope.line_to_char(1);
        b.view.sels = Selections::new(vec![Selection::point(l + 5), Selection::point(l + 6)], 0);
        surround(&mut app, "sm");
        assert_eq!(text(&app), "fn main() {\n    Some(a();)\n    b();\n}\n");
        let b = app.editor.active_buffer().unwrap();
        let heads: Vec<usize> = b.view.sels.iter().map(|s| s.head).collect();
        let at = b.rope.line_to_char(1) + 14;
        assert!(heads.iter().all(|&h| h == at), "{heads:?} want {at}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn surround_hints_without_a_template() {
        let (mut app, dir) = test_app("surround-none");
        load(&mut app, "a.txt", "abc\n");
        app.focus.set(PanelId::Editor);
        app.perform(Action::Editor(EditorAction::SurroundWith));
        assert!(app.dialog.is_none());
        assert!(toasts(&app).contains(&"No live template uses $TM_SELECTED_TEXT here".to_string()), "{:?}", toasts(&app));
        app.config.editor.snippets = false;
        app.perform(Action::Editor(EditorAction::SurroundWith));
        assert!(toasts(&app).contains(&"Live templates are off ([editor] snippets = false)".to_string()), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn surround_dialog_goes_stale_after_an_edit() {
        let (mut app, dir) = test_app("surround-stale");
        load_two(&mut app);
        select(&mut app, (1, 0), (3, 0));
        app.perform(Action::Editor(EditorAction::SurroundWith));
        let d = app.dialog.take().expect("the dialog");
        let b = app.editor.active_buffer_mut().unwrap();
        b.goto_line(0, 0);
        b.insert("z", TxKind::Typing);
        app.finish_dialog(d.id, DialogResult::Choice(0));
        assert_eq!(text(&app), format!("z{TWO}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tab_expands_a_surround_template_without_a_selection() {
        let (mut app, dir) = rust_app("surround-tab");
        typ(&mut app, "if");
        key(&mut app, KeyCode::Tab);
        assert_eq!(text(&app), "fn main() {\n    if condition {\n        \n    }\n}\n");
        assert_eq!(selected(&app), "condition");
        key(&mut app, KeyCode::Tab);
        assert_eq!(caret_line(&app), "        |");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
