//! The Markdown preview tab: a read-only pathless buffer holding a file's
//! rendered text (`ui::markdown::render`), coloured by the render's spans
//! instead of tree-sitter, re-rendered when its source changes.

use std::ops::Range;

use ratatui::style::Style;

use super::Editor;
use super::buffer::{Buffer, BufferId};
use super::selection::Selection;
use super::syntax::Highlights;
use super::undo::{Change, TxKind, UndoStack};
use crate::config::theme::Theme;
use crate::ui::markdown::Rendered;

/// What a preview tab shows: the buffer it renders, the version of that
/// buffer it rendered and the styles of its text.
#[derive(Debug, Clone)]
pub struct MarkdownPreview {
    pub source: BufferId,
    /// The source's `Buffer::version` at the last render; `u64::MAX` marks
    /// the preview stale (a theme change), so the next tick renders again.
    pub version: u64,
    /// Sorted, non-overlapping char ranges of the preview's text.
    pub spans: Vec<(Range<usize>, Style)>,
}

impl MarkdownPreview {
    /// The spans that meet the lines `lines` of `rope`, as the view's
    /// highlights.
    pub fn highlights(&self, rope: &ropey::Rope, lines: Range<usize>) -> Highlights {
        let end_line = lines.end.min(rope.len_lines());
        let start_line = lines.start.min(end_line);
        let start = rope.line_to_char(start_line);
        let end = if end_line >= rope.len_lines() { rope.len_chars() } else { rope.line_to_char(end_line) };
        let first = self.spans.partition_point(|(r, _)| r.end <= start);
        let spans = self.spans[first..].iter().take_while(|(r, _)| r.start < end).cloned().collect();
        Highlights { spans }
    }
}

/// Whether `b` is a Markdown file: its language, or else its extension.
pub fn is_markdown(b: &Buffer) -> bool {
    b.language().is_some_and(|d| d.name == "markdown")
        || b.path.as_deref().and_then(|p| p.extension()).is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Every style over the theme's text colour, so a span with only a
/// modifier (bold, italic) does not fall back to the terminal's colour.
fn preview_spans(spans: Vec<(Range<usize>, Style)>, theme: &Theme) -> Vec<(Range<usize>, Style)> {
    spans.into_iter().map(|(r, s)| (r, Style::default().fg(theme.fg).patch(s))).collect()
}

impl Editor {
    /// Open the preview tab of buffer `source` showing `rendered`; returns
    /// its index.
    pub fn open_markdown_preview(&mut self, source: BufferId, rendered: Rendered, theme: &Theme) -> usize {
        let (name, version) = match self.index_of_id(source).map(|i| &self.buffers[i]) {
            Some(b) => (b.path.as_deref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()), b.version),
            None => (None, 0),
        };
        let id = self.next_id;
        self.next_id += 1;
        let mut buf = Buffer::from_text(id, None, &rendered.text, false);
        buf.name = format!("Preview {}", name.as_deref().unwrap_or("untitled"));
        buf.read_only = true;
        buf.indent = super::indent::FileSettings::default().indent(&buf.rope, self.tab_width);
        buf.view.wrap.on = true;
        buf.preview = Some(MarkdownPreview { source, version, spans: preview_spans(rendered.spans, theme) });
        self.buffers.push(buf);
        let at = self.buffers.len() - 1;
        self.activate_index(at);
        at
    }

    /// The preview tab of buffer `source`, if one is open.
    pub fn markdown_preview_of(&self, source: BufferId) -> Option<BufferId> {
        self.buffers.iter().find(|b| b.preview.as_ref().is_some_and(|p| p.source == source)).map(|b| b.id)
    }

    /// Show buffer `id`: in the focused group when it has the tab, else in
    /// the first group that does, else as a new tab in the focused group.
    pub fn focus_buffer_tab(&mut self, id: BufferId) {
        let Some(at) = self.index_of_id(id) else { return };
        if self.focused_group().slot_of(id).is_none()
            && let Some((g, slot)) = self.group_ids().into_iter().find_map(|g| Some((g, self.group(g)?.slot_of(id)?)))
        {
            self.activate_slot(g, slot);
            return;
        }
        self.activate_index(at);
    }

    /// Render again every preview whose source changed since (or that a
    /// theme change marked stale). True when any was rendered.
    pub fn refresh_markdown_previews(&mut self, theme: &Theme) -> bool {
        let due: Vec<(usize, String, u64)> = self
            .buffers
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let p = b.preview.as_ref()?;
                let src = &self.buffers[self.index_of_id(p.source)?];
                (src.version != p.version).then(|| (i, src.text(), src.version))
            })
            .collect();
        let any = !due.is_empty();
        for (i, text, version) in due {
            let r = crate::ui::markdown::render(&text, theme);
            let b = &mut self.buffers[i];
            b.set_preview_text(&r.text);
            if let Some(p) = b.preview.as_mut() {
                p.spans = preview_spans(r.spans, theme);
                p.version = version;
            }
        }
        any
    }
}

impl Buffer {
    /// Replace a preview's text, keeping its first visible line and caret
    /// line. The edit is not undoable and leaves the buffer clean. Shared
    /// with Find results tabs (`search_tab`).
    pub(super) fn set_preview_text(&mut self, text: &str) {
        let old = self.text();
        if old == text {
            return;
        }
        let (line, col) = self.cursor_line_col();
        let scroll = self.view.scroll_line;
        let shown = self.view.follow_key.filter(|k| (k.0, k.1) == (self.view.sel().head, self.version)).is_some();
        self.read_only = false;
        self.apply_then(vec![Change::replace(0, old, text)], TxKind::Other, |rope| {
            let l = line.min(rope.len_lines() - 1);
            let len = rope.line(l).chars().filter(|&c| c != '\n' && c != '\r').count();
            Selection::point(rope.line_to_char(l) + col.min(len))
        });
        self.read_only = true;
        self.undo = UndoStack::new();
        self.saved_state = self.undo.state_id();
        self.view.scroll_line = super::rows::Rows::new(&self.view.folds, self.len_lines()).visible(scroll);
        self.view.scroll_seg = 0;
        if shown {
            self.view.follow_key = self.view.follow_key.map(|(_, _, w, h)| (self.view.sel().head, self.version, w, h));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::style::Modifier;

    use super::*;

    fn buf(path: Option<&str>) -> Buffer {
        Buffer::from_text(1, path.map(PathBuf::from), "", false)
    }

    #[test]
    fn markdown_files_are_told_by_extension() {
        assert!(is_markdown(&buf(Some("a.md"))));
        assert!(is_markdown(&buf(Some("b.markdown"))));
        assert!(is_markdown(&buf(Some("C.MD"))));
        // With language detection off, only the extension decides.
        let mut plain = buf(Some("C.MD"));
        plain.language_choice = crate::editor::buffer::LanguageChoice::Plain;
        assert!(is_markdown(&plain));
        assert!(!is_markdown(&buf(Some("a.rs"))));
        assert!(!is_markdown(&buf(None)));
    }

    #[test]
    fn highlights_keep_only_the_spans_of_the_window() {
        let rope = ropey::Rope::from_str("aa\nbb\ncc\ndd");
        let (s0, s3) = (Style::default().fg(ratatui::style::Color::Red), Style::default().fg(ratatui::style::Color::Blue));
        let p = MarkdownPreview { source: 0, version: 0, spans: vec![(0..2, s0), (9..11, s3)] };
        assert!(p.highlights(&rope, 1..3).spans.is_empty());
        assert_eq!(p.highlights(&rope, 0..1).spans, vec![(0..2, s0)]);
        assert_eq!(p.highlights(&rope, 3..4).spans, vec![(9..11, s3)]);
        assert_eq!(p.highlights(&rope, 0..99).spans.len(), 2);
    }

    #[test]
    fn spans_take_the_text_colour_unless_they_have_their_own() {
        let theme = Theme::default();
        let own = Style::default().fg(ratatui::style::Color::Red);
        let out = preview_spans(vec![(0..1, Style::default().add_modifier(Modifier::BOLD)), (1..2, own)], &theme);
        assert_eq!(out[0].1, Style::default().fg(theme.fg).add_modifier(Modifier::BOLD));
        assert_eq!(out[1].1, own);
    }
}
