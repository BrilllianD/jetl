//! Reformatting: with the external formatter of the file's language
//! (`[format]` in config.toml), else with its language server
//! (`textDocument/formatting`, or `textDocument/rangeFormatting` for the
//! selection of `editor.format` when the server has it). Requests the app
//! runs (`Effect::Format`),
//! and their results applied as one undoable edit that touches only the
//! changed lines, with the caret kept on the same code.

use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use super::Editor;
use super::buffer::{Buffer, BufferId, LineEnding};
use super::undo::{Change, TxKind};
use crate::app::Ctx;
use crate::event::{Effect, Level};
use crate::util::format::Failure;

/// The buffer as it was handed to the formatter.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub text: String,
    /// `Buffer::version`: a result is applied only while it is unchanged.
    pub version: u64,
    /// `UndoStack::state_id` of `text`, for saving it as it was.
    pub state: u64,
    /// `Buffer::saved_state` at the request; a save meanwhile wins.
    pub saved: u64,
    /// The buffer's path at the request: a result is applied only while it
    /// is unchanged (a rename or Save As meanwhile makes it stale).
    pub path: Option<PathBuf>,
}

/// What formats a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Formatter {
    /// The `[format]` command of its language (`{path}` expanded); it wins.
    Command(Vec<String>),
    /// Its language server, if it formats (the app checks); without one a
    /// save writes the text as it is.
    Lsp,
}

/// Format buffer `id` with `via`; `save` writes it once formatted.
#[derive(Debug)]
pub struct FormatRequest {
    pub id: BufferId,
    pub via: Formatter,
    pub timeout: Duration,
    pub snapshot: Snapshot,
    pub save: bool,
    /// Char range in `snapshot.text` to format (the selection of
    /// `editor.format`); `None` formats the whole file. Only a language
    /// server that formats ranges uses it.
    pub range: Option<Range<usize>>,
}

/// The span covering every non-empty selection of `b`, or `None` when all
/// are empty.
fn selection_span(b: &Buffer) -> Option<Range<usize>> {
    b.view.sels.iter().map(|s| s.range()).filter(|r| !r.is_empty()).reduce(|a, r| a.start.min(r.start)..a.end.max(r.end))
}

impl Editor {
    /// Buffer `i`'s formatter command (`{path}` expanded), if its language
    /// has one.
    pub(super) fn formatter(&self, i: usize) -> Option<Vec<String>> {
        let b = self.buffers.get(i)?;
        let argv = self.format.commands.get(b.language()?.name).filter(|a| !a.is_empty())?;
        let path = b.path.as_deref().map_or_else(|| b.name.clone(), |p| p.display().to_string());
        Some(crate::util::format::expand(argv, &path))
    }

    /// What may format buffer `i`: its `[format]` command, else its
    /// language server; `None` without a language.
    pub(super) fn format_via(&self, i: usize) -> Option<Formatter> {
        match self.formatter(i) {
            Some(argv) => Some(Formatter::Command(argv)),
            None => self.buffers.get(i)?.language().map(|_| Formatter::Lsp),
        }
    }

    /// A request to format buffer `i`, or `None` when it has no language.
    /// Edits made while it runs start their own undo step.
    pub(super) fn format_request(&mut self, i: usize, save: bool) -> Option<FormatRequest> {
        let via = self.format_via(i)?;
        let b = &mut self.buffers[i];
        b.undo.break_group = true;
        let snapshot =
            Snapshot { text: b.text(), version: b.version, state: b.undo.state_id(), saved: b.saved_state, path: b.path.clone() };
        Some(FormatRequest { id: b.id, via, timeout: Duration::from_millis(self.format.timeout_ms), snapshot, save, range: None })
    }

    /// `editor.format`: reformat the active buffer (or the span of its
    /// selections) without saving.
    pub(super) fn format_active(&mut self, cx: &mut Ctx) {
        let Some(i) = self.active() else { return };
        match self.format_request(i, false) {
            Some(mut req) => {
                req.range = selection_span(&self.buffers[i]);
                cx.effects.push(Effect::Format(req));
            }
            None => cx.notify(Level::Warn, format!("No formatter for {}: it has no language", self.buffers[i].name)),
        }
    }

    /// A formatter answered for buffer `id`. The text replaces the buffer
    /// only if the buffer is unchanged since the request (same version, same
    /// path); a failure leaves it alone with a warning (the first stderr
    /// line, all of it in the Event Log). A save request writes either way:
    /// the formatted text, the unformatted one after a failure, or, when the
    /// buffer was edited or moved meanwhile, the text as it was when the
    /// save was asked for (unless it was saved since). Nothing is written
    /// when that text is already the saved state (Save As wrote it before
    /// formatting, or a save happened meanwhile).
    pub fn format_done(&mut self, id: BufferId, save: bool, snap: Snapshot, result: Result<String, Failure>, cx: &mut Ctx) {
        let Some(i) = self.index_of_id(id) else { return };
        let b = &mut self.buffers[i];
        let fresh = b.version == snap.version && b.path == snap.path;
        match result {
            Ok(text) if fresh => {
                if !b.apply_formatted(&text) && !save {
                    cx.notify(Level::Info, format!("{} is already formatted", b.name));
                }
            }
            Ok(_) => tracing::debug!("{} was edited or moved while formatting; format dropped", b.name),
            Err(f) => {
                let then = if save { "; saved unformatted" } else { "" };
                let mut msg = format!("Format {}: {}{then}", b.name, f.summary);
                if f.stderr.lines().filter(|l| !l.trim().is_empty()).nth(1).is_some() {
                    msg.push('\n');
                    msg.push_str(&f.stderr);
                }
                cx.notify(Level::Warn, msg);
            }
        }
        if !save {
            return;
        }
        let b = &self.buffers[i];
        if !fresh && b.saved_state != snap.saved {
            return;
        }
        let state = if fresh { b.undo.state_id() } else { snap.state };
        // Already on disk: Save As wrote it before formatting, or a save
        // happened meanwhile.
        if state == b.saved_state {
            return;
        }
        if b.dirty() && super::io::disk_conflict(b) {
            cx.effects.extend(self.conflict_dialog(i).map(Effect::Dialog));
            return;
        }
        let text = if fresh { b.text() } else { snap.text };
        self.write_text(i, text, state, cx, false);
    }
}

impl Buffer {
    /// Replace the text with `formatted` (its line breaks made the buffer's)
    /// as one undoable transaction of the changed lines only. The selection
    /// stays on the same code and the view on the same lines. Returns false,
    /// with no transaction, when nothing changes.
    pub fn apply_formatted(&mut self, formatted: &str) -> bool {
        let lf = formatted.replace("\r\n", "\n");
        let new = if self.line_ending == LineEnding::CrLf { lf.replace('\n', "\r\n") } else { lf };
        self.replace_by_lines(&new)
    }

    /// Replace the text with `new` (taken as is) like [`Buffer::apply_formatted`]:
    /// one undoable transaction of the changed lines, selections, folds and
    /// bookmarks mapped by line, the view kept on the same lines. Returns
    /// false, with no transaction, when nothing changes.
    pub fn replace_by_lines(&mut self, new: &str) -> bool {
        let old = self.text();
        if old == new {
            return false;
        }
        let hunks = crate::git::diff::rope_hunks(&old, new);
        let (os, ns) = (line_starts(&old), line_starts(new));
        // In order; each at its place in the new text, since everything
        // before it already matches the new text when it is applied.
        let changes: Vec<Change> = hunks
            .iter()
            .map(|h| {
                let (b0, b1, a0, a1) = (h.before.start as usize, h.before.end as usize, h.after.start as usize, h.after.end as usize);
                Change::replace(ns[a0].1, &old[os[b0].0..os[b1].0], &new[ns[a0].0..ns[a1].0])
            })
            .collect();
        let map = |p: usize| map_pos(&old, new, &os, &ns, &hunks, p);
        let mut sels = self.view.sels.clone();
        sels.map(map);
        let (old_line, old_scroll) = (self.cursor_line_col().0, self.view.scroll_line);
        let shown = self.view.follow_key.filter(|k| (k.0, k.1) == (self.view.sel().head, self.version)).map(|k| k.3 as usize);
        self.apply(changes, sels, TxKind::Other);
        let new_line = self.cursor_line_col().0;
        match shown {
            // The caret was on screen: it keeps its row.
            Some(h) if (old_scroll..old_scroll + h).contains(&old_line) => {
                self.view.scroll_line = new_line.saturating_sub(old_line - old_scroll);
            }
            // Scrolled away from the caret: the view keeps its lines and
            // does not jump to the caret.
            Some(_) => {
                self.view.scroll_line = map_line(&hunks, old_scroll);
                self.view.follow_key = self.view.follow_key.map(|(_, _, w, h)| (self.view.sel().head, self.version, w, h));
            }
            None => self.view.scroll_line = map_line(&hunks, old_scroll),
        }
        self.view.scroll_line = super::rows::Rows::new(&self.view.folds, self.len_lines()).visible(self.view.scroll_line);
        self.view.scroll_seg = 0;
        true
    }
}

/// (byte, char) offset of each line start the way the rope splits lines,
/// plus one for the end of the text.
fn line_starts(s: &str) -> Vec<(usize, usize)> {
    let mut out = vec![(0, 0)];
    let (mut bytes, mut chars) = (0, 0);
    for line in crate::util::text::lines_inclusive(s) {
        bytes += line.len();
        chars += line.chars().count();
        out.push((bytes, chars));
    }
    out
}

/// Where char position `p` of `old` lands in `new`. Outside the changed
/// lines it moves with its line. Inside them it keeps its place in the
/// code: formatters mostly change whitespace, so it goes after as many
/// non-whitespace characters of the new lines as preceded it in the old
/// ones, and on to the next code when it was right before code (a caret
/// before `2` in `(1,2)` stays before it in `(1, 2)`, one at the start of a
/// reindented line goes past the new indentation).
fn map_pos(old: &str, new: &str, os: &[(usize, usize)], ns: &[(usize, usize)], hunks: &[imara_diff::Hunk], p: usize) -> usize {
    let mut shift = (0, 0);
    for h in hunks {
        let (b0, b1, a0, a1) = (h.before.start as usize, h.before.end as usize, h.after.start as usize, h.after.end as usize);
        if p < os[b0].1 {
            break;
        }
        if p >= os[b1].1 {
            shift = (os[b1].1, ns[a1].1);
            continue;
        }
        let mut old_chars = old[os[b0].0..].chars();
        let code = old_chars.by_ref().take(p - os[b0].1).filter(|c| !c.is_whitespace()).count();
        let before_code = old_chars.next().is_some_and(|c| !c.is_whitespace());
        let mut at = ns[a0].1;
        let mut seen = 0;
        let mut chars = new[ns[a0].0..ns[a1].0].chars().peekable();
        while seen < code {
            let Some(c) = chars.next() else { break };
            at += 1;
            seen += usize::from(!c.is_whitespace());
        }
        if before_code {
            while chars.next_if(|c| c.is_whitespace()).is_some() {
                at += 1;
            }
        }
        // Not past the hunk's last line break.
        let head = &new[..ns[a1].0];
        let brk = if a1 == a0 {
            0
        } else if head.ends_with("\r\n") {
            2
        } else {
            usize::from(head.ends_with('\n') || head.ends_with('\r'))
        };
        let end = ns[a1].1 - brk;
        return at.min(end.max(ns[a0].1));
    }
    p - shift.0 + shift.1
}

/// Where line `line` of the old text is in the new one (a changed line:
/// the same line of its new block, or the block's last).
fn map_line(hunks: &[imara_diff::Hunk], line: usize) -> usize {
    let mut shift: isize = 0;
    for h in hunks {
        let (b0, b1, a0, a1) = (h.before.start as usize, h.before.end as usize, h.after.start as usize, h.after.end as usize);
        if line < b0 {
            break;
        }
        if line >= b1 {
            shift = a1 as isize - b1 as isize;
            continue;
        }
        return a0 + (line - b0).min((a1 - a0).saturating_sub(1));
    }
    (line as isize + shift).max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::selection::Selection;

    fn buf(text: &str) -> Buffer {
        Buffer::from_text(1, None, text, false)
    }

    #[test]
    fn lone_cr_lines_are_lines() {
        let mut b = buf("a\rb\rc\r");
        b.view.set_sel(Selection::point(4));
        assert!(b.replace_by_lines("a\rXYZ\rc\r"));
        assert_eq!(b.text(), "a\rXYZ\rc\r");
        assert_eq!(b.view.sel().head, 6);
    }

    #[test]
    fn only_changed_lines_are_replaced() {
        let old = "fn a() {\n  x;\n}\n\nfn b() {\n  y;\n}\n";
        let new = "fn a() {\n    x;\n}\n\nfn b() {\n    y;\n}\n";
        let mut b = buf(old);
        assert!(b.apply_formatted(new));
        assert_eq!(b.text(), new);
        assert_eq!(b.undo.len(), 1, "one transaction");
        assert!(b.undo(), "one undo step");
        assert_eq!(b.text(), old);
        // Only the two indented lines were in it.
        b.redo();
        let tx = b.undo.last_mut().unwrap();
        let olds: Vec<&str> = tx.changes.iter().map(|c| c.old.as_str()).collect();
        assert_eq!(olds, ["  x;\n", "  y;\n"]);
    }

    #[test]
    fn unchanged_output_makes_no_transaction() {
        let mut b = buf("a\r\nb\r\n");
        // The formatter's LF line breaks become the buffer's CRLF.
        assert!(!b.apply_formatted("a\nb\n"));
        assert!(b.undo.is_empty() && !b.dirty());
        assert!(b.apply_formatted("a\nc\n"));
        assert_eq!(b.text(), "a\r\nc\r\n");
    }

    #[test]
    fn caret_stays_on_the_same_code() {
        let old = "fn main(){let x=foo( 1,2 );\nbar();\n}\n";
        let new = "fn main() {\n    let x = foo(1, 2);\n    bar();\n}\n";
        let mut b = buf(old);
        let at = |s: &str, needle: &str| s.find(needle).map(|i| s[..i].chars().count()).unwrap();
        b.view.set_sel(Selection::point(at(old, "2 )")));
        assert!(b.apply_formatted(new));
        assert_eq!(b.view.sel().head, at(new, "2);"), "on the 2");
        // Before the first non-blank of a reindented line: after the new
        // indentation, not at the line start.
        let mut b = buf("{\nx\n}\n");
        b.view.set_sel(Selection::point(2));
        b.apply_formatted("{\n    x\n}\n");
        assert_eq!(b.cursor_line_col(), (1, 4));
        // Below the changes it moves with its line.
        let mut b = buf("a\nb\nc\n");
        b.view.set_sel(Selection::point(5));
        b.apply_formatted("a\n\nb\nc\n");
        assert_eq!(b.cursor_line_col(), (3, 1));
        // The view keeps the caret's row.
        let mut b = buf(&"x\n".repeat(50));
        b.view.set_sel(Selection::point(60));
        b.view.scroll_line = 20;
        b.view.follow_key = Some((60, b.version, 80, 20));
        b.apply_formatted(&format!("y\ny\n{}", "x\n".repeat(50)));
        assert_eq!((b.cursor_line_col().0, b.view.scroll_line), (32, 22));
    }

    #[test]
    fn lines_map_through_hunks() {
        let hunks = crate::git::diff::hunks("a\nb\nc\nd\n", "a\nB\nB2\nc\nd\n");
        assert_eq!([0, 1, 2, 3].map(|l| map_line(&hunks, l)), [0, 1, 3, 4]);
    }

    #[test]
    fn selection_span_covers_every_non_empty_selection() {
        use crate::editor::selection::Selections;
        let mut b = buf("ab\ncd\nef\ngh\n");
        assert_eq!(selection_span(&b), None, "no selection");
        b.view.sels = Selections::new(vec![Selection::point(1), Selection::point(4)], 0);
        assert_eq!(selection_span(&b), None, "carets only");
        b.view.set_sel(Selection::new(5, 3));
        assert_eq!(selection_span(&b), Some(3..5), "one, backwards");
        b.view.sels = Selections::new(vec![Selection::new(0, 1), Selection::point(4), Selection::new(9, 7)], 2);
        assert_eq!(selection_span(&b), Some(0..9), "two disjoint, the caret between ignored");
        // A block selection: columns 0..1 of lines 1 to 3.
        b.view.sels = Selections::new(vec![Selection::new(3, 4), Selection::new(6, 7), Selection::new(9, 10)], 2);
        assert_eq!(selection_span(&b), Some(3..10), "a block: from its first cell to its last, not whole lines");
    }
}
