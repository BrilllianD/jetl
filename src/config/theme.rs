//! Colors. Darcula-like defaults; TOML override comes in a later phase.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub panel_bg: Color,
    pub border: Style,
    pub border_focused: Style,
    pub title: Style,
    pub title_focused: Style,
    pub strip_bg: Color,
    pub strip_item: Style,
    pub strip_item_active: Style,
    pub status_bg: Color,
    pub status_fg: Color,
    pub status_accent: Style,
    pub tab: Style,
    pub tab_active: Style,
    pub tab_dirty: Style,
    pub line_number: Style,
    pub line_number_current: Style,
    pub current_line_bg: Color,
    pub selection_bg: Color,
    pub search_match_bg: Color,
    pub search_current_bg: Color,
    pub gutter_added: Color,
    pub gutter_modified: Color,
    pub gutter_deleted: Color,
    pub git_added: Style,
    pub git_modified: Style,
    pub git_deleted: Style,
    pub git_untracked: Style,
    pub git_conflicted: Style,
    pub git_ignored: Style,
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
    pub diff_mod_bg: Color,
    pub popup_bg: Color,
    pub popup_border: Style,
    pub list_selected: Style,
    pub list_selected_unfocused: Style,
    pub dim: Style,
    pub error: Style,
    pub warn: Style,
    pub info: Style,
    pub splitter: Style,
    /// Tree-sitter capture name -> style, looked up by longest dotted prefix.
    pub syntax: HashMap<String, Style>,
}

impl Theme {
    pub fn darcula() -> Self {
        let bg = Color::Rgb(43, 43, 43);
        let panel_bg = Color::Rgb(60, 63, 65);
        let fg = Color::Rgb(169, 183, 198);
        let accent = Color::Rgb(75, 110, 175);
        Self {
            bg,
            fg,
            panel_bg,
            border: Style::default().fg(Color::Rgb(85, 85, 85)),
            border_focused: Style::default().fg(accent),
            title: Style::default().fg(Color::Rgb(187, 187, 187)),
            title_focused: Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            strip_bg: Color::Rgb(49, 51, 53),
            strip_item: Style::default().fg(Color::Rgb(150, 150, 150)).bg(Color::Rgb(49, 51, 53)),
            strip_item_active: Style::default().fg(Color::White).bg(accent).add_modifier(Modifier::BOLD),
            status_bg: Color::Rgb(49, 51, 53),
            status_fg: Color::Rgb(187, 187, 187),
            status_accent: Style::default().fg(Color::Rgb(104, 151, 187)),
            tab: Style::default().fg(Color::Rgb(187, 187, 187)).bg(panel_bg),
            tab_active: Style::default().fg(Color::White).bg(Color::Rgb(78, 82, 84)).add_modifier(Modifier::BOLD),
            tab_dirty: Style::default().fg(Color::Rgb(255, 198, 109)),
            line_number: Style::default().fg(Color::Rgb(96, 99, 102)),
            line_number_current: Style::default().fg(Color::Rgb(164, 167, 171)),
            current_line_bg: Color::Rgb(50, 50, 50),
            selection_bg: Color::Rgb(33, 66, 131),
            search_match_bg: Color::Rgb(50, 89, 60),
            search_current_bg: Color::Rgb(155, 110, 30),
            gutter_added: Color::Rgb(98, 150, 85),
            gutter_modified: Color::Rgb(53, 146, 196),
            gutter_deleted: Color::Rgb(140, 140, 140),
            git_added: Style::default().fg(Color::Rgb(98, 150, 85)),
            git_modified: Style::default().fg(Color::Rgb(104, 151, 187)),
            git_deleted: Style::default().fg(Color::Rgb(140, 140, 140)),
            git_untracked: Style::default().fg(Color::Rgb(204, 120, 50)),
            git_conflicted: Style::default().fg(Color::Rgb(255, 100, 100)),
            git_ignored: Style::default().fg(Color::Rgb(110, 110, 110)),
            diff_add_bg: Color::Rgb(41, 66, 54),
            diff_del_bg: Color::Rgb(72, 44, 44),
            diff_mod_bg: Color::Rgb(44, 55, 72),
            popup_bg: Color::Rgb(53, 53, 56),
            popup_border: Style::default().fg(accent),
            list_selected: Style::default().fg(Color::White).bg(Color::Rgb(44, 83, 148)),
            list_selected_unfocused: Style::default().fg(Color::White).bg(Color::Rgb(13, 41, 62)),
            dim: Style::default().fg(Color::Rgb(120, 120, 120)),
            error: Style::default().fg(Color::Rgb(255, 107, 104)),
            warn: Style::default().fg(Color::Rgb(255, 198, 109)),
            info: Style::default().fg(Color::Rgb(104, 151, 187)),
            splitter: Style::default().fg(Color::Rgb(85, 85, 85)).bg(bg),
            syntax: darcula_syntax(fg),
        }
    }

    /// Style for a capture such as `function.method`: tries the full name,
    /// then `function`.
    pub fn capture_style(&self, name: &str) -> Option<Style> {
        let mut n = name;
        loop {
            if let Some(s) = self.syntax.get(n) {
                return Some(*s);
            }
            match n.rfind('.') {
                Some(i) => n = &n[..i],
                None => return None,
            }
        }
    }
}

fn darcula_syntax(fg: Color) -> HashMap<String, Style> {
    let c = |r, g, b| Style::default().fg(Color::Rgb(r, g, b));
    let keyword = c(204, 120, 50);
    let string = c(106, 135, 89);
    let comment = c(128, 128, 128);
    let number = c(104, 151, 187);
    let function = c(255, 198, 109);
    let constant = c(152, 118, 170);
    let meta = c(187, 181, 41);
    let plain = Style::default().fg(fg);
    // Identifier-level captures drawn in the default color (variables,
    // types) are left out so they never shadow a more specific capture of
    // the same node (e.g. `@variable` listed before `@function`).
    let pairs: Vec<(&str, Style)> = vec![
        ("keyword", keyword),
        ("keyword.directive", meta),
        ("string", string),
        ("string.special", c(106, 135, 89)),
        ("string.special.key", c(152, 118, 170)),
        ("string.escape", keyword),
        ("escape", keyword),
        ("comment", comment.add_modifier(Modifier::ITALIC)),
        ("comment.documentation", c(98, 151, 85).add_modifier(Modifier::ITALIC)),
        ("number", number),
        ("boolean", keyword),
        ("constant", constant),
        ("constant.builtin", keyword),
        ("function", function),
        ("function.builtin", c(136, 136, 198)),
        ("function.macro", meta),
        ("type.builtin", keyword),
        ("property", constant),
        ("attribute", meta),
        ("label", c(32, 153, 157)),
        ("operator", plain),
        ("punctuation", plain),
        ("punctuation.special", keyword),
        ("variable.builtin", c(148, 85, 141)),
        ("tag", c(232, 191, 106)),
        ("embedded", plain),
        ("text.title", c(255, 198, 109).add_modifier(Modifier::BOLD)),
        ("text.literal", c(106, 135, 89)),
        ("text.reference", c(104, 151, 187)),
        ("text.uri", c(104, 151, 187).add_modifier(Modifier::UNDERLINED)),
        ("none", plain),
    ];
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

impl Default for Theme {
    fn default() -> Self {
        Self::darcula()
    }
}
