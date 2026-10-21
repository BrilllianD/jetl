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

use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemLabelDetails, InsertTextFormat};

use super::App;
use crate::config::snippets::Snippet;
use crate::dispatch::Mode;
use crate::editor::brackets::in_literal_at;
use crate::editor::buffer::Buffer;
use crate::editor::selection::is_word_char;
use crate::lsp::completion::{CompletionState, Origin, parse_snippet, word_start};
use crate::ui::completion::CompletionPopup;
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

#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::actions::{Action, EditorAction};
    use crate::app::tests::{load, test_app};
    use crate::config::FileText;
    use crate::config::snippets::SnippetSet;
    use crate::editor::selection::pos_to_line_col;

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
}
