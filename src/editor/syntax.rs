//! Tree-sitter highlighting (filled in phase 2).

use ratatui::style::Style;
use ropey::Rope;

use crate::config::theme::Theme;

#[derive(Debug)]
pub struct Syntax {}

#[derive(Debug, Default)]
pub struct Highlights {
    pub spans: Vec<(std::ops::Range<usize>, Style)>,
}

impl Highlights {
    pub fn style_at(&self, char_idx: usize) -> Option<Style> {
        // Spans are sorted by start; inner spans come later and win.
        let mut found = None;
        for (r, s) in &self.spans {
            if r.start > char_idx {
                break;
            }
            if r.contains(&char_idx) {
                found = Some(*s);
            }
        }
        found
    }
}

impl Syntax {
    pub fn note_change(&mut self, _rope: &Rope, _at: usize, _old_len: usize, _new_len: usize, _old: &str, _new: &str) {}
    pub fn highlights(&mut self, _rope: &Rope, _first_line: usize, _rows: usize, _theme: &Theme) -> Highlights {
        Highlights::default()
    }
}
