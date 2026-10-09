use ratatui::style::Style;

pub mod file_rows;
pub mod list;
pub mod text_input;

/// Split `text` into runs, styling the chars for which `marked(char index)`
/// holds with `hl`.
pub fn highlight(text: &str, marked: impl Fn(usize) -> bool, base: Style, hl: Style) -> Vec<(String, Style)> {
    runs(text, |ci| if marked(ci) { hl } else { base })
}

/// Split `text` into runs of equal `style_of(char index)`.
pub fn runs(text: &str, style_of: impl Fn(usize) -> Style) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    for (ci, c) in text.chars().enumerate() {
        let st = style_of(ci);
        match out.last_mut() {
            Some((s, last)) if *last == st => s.push(c),
            _ => out.push((c.to_string(), st)),
        }
    }
    out
}
