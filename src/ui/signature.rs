//! The Parameter Info popup (Ctrl+P): a language server's signature help at
//! the caret, one row per signature, the active parameter bold. Drawn above
//! the caret line, below it when there is no room. The app (`app/signature.rs`)
//! asks again after every editor action while it is up and closes it on a
//! null answer, Esc, or when the editor is no longer where it was asked.

use std::ops::Range;

use lsp_types::{ParameterLabel, SignatureHelp};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::theme::Theme;
use crate::editor::buffer::BufferId;
use crate::editor::groups::GroupId;

/// Most signatures drawn.
const MAX_ROWS: usize = 8;
/// Widest the popup gets (with its border), in cells.
const MAX_WIDTH: u16 = 100;

pub struct SignaturePopup {
    pub group: GroupId,
    pub buffer: BufferId,
    /// The answer, sent back as `activeSignatureHelp` when asking again.
    pub help: SignatureHelp,
    rect: Rect,
}

impl SignaturePopup {
    /// `None` when the answer has no signatures.
    pub fn new(group: GroupId, buffer: BufferId, help: SignatureHelp) -> Option<Self> {
        if help.signatures.is_empty() {
            return None;
        }
        Some(Self { group, buffer, help, rect: Rect::default() })
    }

    /// The active signature index (clamped) and its active parameter
    /// (`SignatureInformation::active_parameter`, else `SignatureHelp::active_parameter`,
    /// else 0); `None` when the signature lists no parameters.
    pub fn active(&self) -> (usize, Option<u32>) {
        let i = (self.help.active_signature.unwrap_or(0) as usize).min(self.help.signatures.len() - 1);
        let sig = &self.help.signatures[i];
        if sig.parameters.as_ref().is_none_or(Vec::is_empty) {
            return (i, None);
        }
        (i, Some(sig.active_parameter.or(self.help.active_parameter).unwrap_or(0)))
    }

    /// Whether a screen cell is inside the popup as last drawn.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rect.contains(ratatui::layout::Position::new(x, y))
    }

    /// Draw next to the caret cell `anchor`, inside `area` (the screen
    /// above the status bar).
    pub fn render(&mut self, frame: &mut Frame, area: Rect, anchor: (u16, u16), theme: &Theme) {
        self.rect = Rect::default();
        let (ax, ay) = anchor;
        let labels: Vec<String> = self.help.signatures.iter().take(MAX_ROWS).map(|s| s.label.replace('\t', " ")).collect();
        let max_w = MAX_WIDTH.min(area.width);
        if max_w < 12 || ay < area.y || ay >= area.bottom() {
            return;
        }
        let widest = labels.iter().map(|l| l.width()).max().unwrap_or(0);
        let w = (widest.min(u16::MAX as usize - 4) as u16 + 4).min(max_w);
        let h = labels.len() as u16 + 2;
        let y = if ay - area.y >= h {
            ay - h
        } else if area.bottom() - (ay + 1) >= h {
            ay + 1
        } else {
            return;
        };
        let x = ax.min(area.right() - w).max(area.x);
        let rect = Rect::new(x, y, w, h);
        self.rect = rect;
        let (active, param) = self.active();
        frame.render_widget(Clear, rect);
        let bg = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let mut block = Block::default().borders(Borders::ALL).border_style(theme.popup_border).style(bg);
        if self.help.signatures.len() > 1 {
            block = block.title_bottom(Line::from(format!(" {}/{} ", active + 1, self.help.signatures.len())).right_aligned());
        }
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let room = inner.width.saturating_sub(2) as usize;
        let buf = frame.buffer_mut();
        for (i, label) in labels.iter().enumerate() {
            let row = inner.y + i as u16;
            let style = if i == active { bg } else { theme.dim.bg(theme.popup_bg) };
            buf.set_stringn(inner.x + 1, row, label, room, style);
            if i != active {
                continue;
            }
            let sig = &self.help.signatures[i];
            let range = param.and_then(|p| sig.parameters.as_ref()?.get(p as usize)).and_then(|p| param_range(label, &p.label));
            let Some(range) = range else { continue };
            let mut cell = 0usize;
            for (n, c) in label.chars().enumerate() {
                let cw = c.width().unwrap_or(0);
                if range.contains(&n) {
                    for dx in 0..cw.max(1) {
                        if cell + dx < room {
                            buf[(inner.x + 1 + (cell + dx) as u16, row)].modifier.insert(Modifier::BOLD);
                        }
                    }
                }
                cell += cw;
            }
        }
    }
}

/// The char range of parameter `p` in `label`: a `Simple` label is its first
/// occurrence (`None` when absent), `LabelOffsets` are UTF-16 offsets
/// (`None` when out of range).
pub fn param_range(label: &str, p: &ParameterLabel) -> Option<Range<usize>> {
    match p {
        ParameterLabel::Simple(s) => {
            let at = label.find(s.as_str())?;
            let start = label[..at].chars().count();
            Some(start..start + s.chars().count())
        }
        ParameterLabel::LabelOffsets([s, e]) => {
            let (mut u16s, mut start, mut end) = (0u32, None, None);
            for (n, c) in label.chars().enumerate() {
                if u16s == *s {
                    start = Some(n);
                }
                if u16s == *e {
                    end = Some(n);
                }
                u16s += c.len_utf16() as u32;
            }
            if u16s == *s {
                start = Some(label.chars().count());
            }
            if u16s == *e {
                end = Some(label.chars().count());
            }
            let (start, end) = (start?, end?);
            (start <= end).then_some(start..end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_range_simple_and_offsets() {
        let l = "fn f(a: u32, b: &str)";
        assert_eq!(param_range(l, &ParameterLabel::Simple("b: &str".into())), Some(13..20));
        assert_eq!(param_range(l, &ParameterLabel::Simple("zzz".into())), None);
        assert_eq!(param_range(l, &ParameterLabel::LabelOffsets([5, 11])), Some(5..11));
        assert_eq!(param_range(l, &ParameterLabel::LabelOffsets([5, 99])), None);
        assert_eq!(param_range(l, &ParameterLabel::LabelOffsets([13, 5])), None);
        // A char outside the BMP is two UTF-16 units but one char.
        let l = "fn f(😀: u32, b: &str)";
        assert_eq!(param_range(l, &ParameterLabel::LabelOffsets([5, 12])), Some(5..11));
        assert_eq!(param_range(l, &ParameterLabel::Simple("b: &str".into())), Some(13..20));
        // The middle of a surrogate pair is no boundary.
        assert_eq!(param_range(l, &ParameterLabel::LabelOffsets([6, 12])), None);
    }
}
