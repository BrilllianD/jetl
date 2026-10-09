use ratatui::style::Style;

pub mod file_rows;
pub mod list;
pub mod text_input;

/// Split `text` into runs, styling the chars for which `marked(char index)`
/// holds with `hl`.
pub fn highlight(text: &str, marked: impl Fn(usize) -> bool, base: Style, hl: Style) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    for (ci, c) in text.chars().enumerate() {
        let st = if marked(ci) { hl } else { base };
        match out.last_mut() {
            Some((s, last)) if *last == st => s.push(c),
            _ => out.push((c.to_string(), st)),
        }
    }
    out
}
