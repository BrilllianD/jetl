//! Main menu, JetBrains-style: a menu bar drawn over the top screen row with
//! the open menu's dropdown (a `ContextMenu`) under its title. Hidden until
//! `app.main_menu` (F10). Left/Right and mnemonic letters switch menus, the
//! dropdown takes Up/Down/Enter, Esc or a click outside closes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Clear;
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, AppAction, EditorAction, GitAction, LspAction, TreeAction};
use crate::config::Icons;
use crate::config::theme::Theme;
use crate::event::Mouse;
use crate::ui::menu::{ContextMenu, MenuItem, MenuResult};

/// A row of the menu table: a label and a registry action id, or a divider.
pub enum Row {
    Item(&'static str, &'static str),
    Sep,
}

use Row::{Item, Sep};

/// The main menu: title, mnemonic and rows of each menu.
pub const MAIN_MENU: &[(&str, char, &[Row])] = &[
    (
        "File",
        'F',
        &[
            Item("Go to File…", "palette.files"),
            Item("Recent Files…", "palette.recent_files"),
            Item("Recent Locations…", "palette.recent_locations"),
            Item("Recent Projects…", "palette.recent_projects"),
            Item("New Scratch File…", "editor.new_scratch"),
            Item("New Task…", "tasks.new_task"),
            Item("Scratch Files…", "editor.scratch_files"),
            Item("Local History…", "editor.local_history"),
            Item("Local History: Put Label…", "editor.put_label"),
            Sep,
            Item("Save", "editor.save"),
            Item("Save As…", "editor.save_as"),
            Item("Save All", "editor.save_all"),
            Sep,
            Item("Close Tab", "editor.close_tab"),
            Item("Close Other Tabs", "editor.close_other_tabs"),
            Item("Close All Tabs", "editor.close_all_tabs"),
            Sep,
            Item("Settings…", "app.settings"),
            Sep,
            Item("Exit", "app.quit"),
        ],
    ),
    (
        "Edit",
        'E',
        &[
            Item("Undo", "editor.undo"),
            Item("Redo", "editor.redo"),
            Sep,
            Item("Cut", "editor.cut"),
            Item("Copy", "editor.copy"),
            Item("Paste", "editor.paste"),
            Item("Select All", "editor.select_all"),
            Item("Extend Selection", "editor.expand_selection"),
            Item("Shrink Selection", "editor.shrink_selection"),
            Sep,
            Item("Add Selection for Next Occurrence", "editor.add_next_occurrence"),
            Item("Unselect Occurrence", "editor.unselect_occurrence"),
            Item("Select All Occurrences", "editor.select_all_occurrences"),
            Item("Clone Caret Above", "editor.clone_caret_above"),
            Item("Clone Caret Below", "editor.clone_caret_below"),
            Item("Column Selection Mode", "editor.toggle_column_mode"),
            Sep,
            Item("Find…", "editor.find"),
            Item("Replace…", "editor.replace"),
            Item("Find in Files…", "find.in_files"),
            Item("Replace in Files…", "find.replace_in_files"),
            Sep,
            Item("Duplicate Line", "editor.duplicate_line"),
            Item("Delete Line", "editor.delete_line"),
            Item("Join Lines", "editor.join_lines"),
            Item("Move Line Up", "editor.move_line_up"),
            Item("Move Line Down", "editor.move_line_down"),
            Item("Indent", "editor.tab"),
            Item("Unindent", "editor.unindent"),
            Sep,
            Item("Toggle Case", "editor.toggle_case"),
            Item("To Upper Case", "editor.to_upper_case"),
            Item("To Lower Case", "editor.to_lower_case"),
            Item("Sort Lines", "editor.sort_lines"),
            Item("Reverse Lines", "editor.reverse_lines"),
            Sep,
            Item("Comment with Line Comment", "editor.toggle_comment"),
            Item("Comment with Block Comment", "editor.toggle_block_comment"),
            Item("Reformat Code", "editor.format"),
            Item("Code Completion", "lsp.complete"),
            Item("Expand Live Template", "editor.expand_snippet"),
            Item("Surround With…", "editor.surround_with"),
            Sep,
            Item("Rename…", "lsp.rename"),
            Item("Show Context Actions", "lsp.context_actions"),
            Item("Refactor This…", "lsp.refactor_this"),
            Item("Extract Method", "lsp.extract_method"),
            Item("Extract Variable", "lsp.extract_variable"),
            Item("Extract Constant", "lsp.extract_constant"),
            Item("Inline", "lsp.inline"),
            Item("Change Signature", "lsp.change_signature"),
            Item("Move", "lsp.move"),
            Item("Safe Delete", "lsp.safe_delete"),
            Item("Optimize Imports", "lsp.optimize_imports"),
        ],
    ),
    (
        "View",
        'V',
        &[
            Item("Project", "tool_window.project"),
            Item("Worktrees", "tool_window.worktrees"),
            Item("Structure", "tool_window.structure"),
            Item("Find", "tool_window.find"),
            Item("Run", "tool_window.run"),
            Item("Problems", "tool_window.problems"),
            Item("Terminal", "tool_window.terminal"),
            Item("New Terminal Tab", "terminal.new_tab"),
            Item("Git", "tool_window.git"),
            Item("Local History", "tool_window.local_history"),
            Item("Event Log", "tool_window.event_log"),
            Item("Tasks", "tool_window.tasks"),
            Item("TODO", "tool_window.todo"),
            Item("Hierarchy", "tool_window.hierarchy"),
            Sep,
            Item("Hide All Tool Windows", "tool_window.hide_all"),
            Item("Restore Default Layout", "tool_window.restore_default_layout"),
            Item("Switch Layout…", "tool_window.switch_layout"),
            Item("Save Current Layout as New…", "tool_window.save_layout_as_new"),
            Item("Save Changes in Current Layout", "tool_window.save_layout_changes"),
            Item("Rename Layout…", "tool_window.rename_layout"),
            Item("Delete Layout…", "tool_window.delete_layout"),
            Item("Maximize Tool Window", "tool_window.maximize"),
            Item("Pin or Unpin Tool Window", "tool_window.toggle_pinned"),
            Item("Undock or Dock Tool Window", "tool_window.toggle_undocked"),
            Item("Move Tool Window to Left Top", "tool_window.move_left"),
            Item("Move Tool Window to Left Bottom", "tool_window.move_left_bottom"),
            Item("Move Tool Window to Bottom Left", "tool_window.move_bottom"),
            Item("Move Tool Window to Bottom Right", "tool_window.move_bottom_right"),
            Item("Move Tool Window to Right Top", "tool_window.move_right"),
            Item("Move Tool Window to Right Bottom", "tool_window.move_right_bottom"),
            Item("Jump to Last Tool Window", "tool_window.last"),
            Item("Next Tool Window Tab", "tool_window.next_tab"),
            Item("Previous Tool Window Tab", "tool_window.prev_tab"),
            Item("Toolbar", "app.toggle_toolbar"),
            Item("Zen Mode", "app.zen"),
            Sep,
            Item("Split Right", "editor.split_right"),
            Item("Split Down", "editor.split_down"),
            Item("Move Tab to Opposite Group", "editor.move_tab_to_opposite_group"),
            Item("Next Tab Group", "editor.next_group"),
            Item("Focus Left Group", "editor.focus_group_left"),
            Item("Focus Right Group", "editor.focus_group_right"),
            Item("Focus Group Above", "editor.focus_group_up"),
            Item("Focus Group Below", "editor.focus_group_down"),
            Item("Unsplit", "editor.unsplit"),
            Item("Unsplit All", "editor.unsplit_all"),
            Sep,
            Item("Expand", "editor.fold_expand"),
            Item("Collapse", "editor.fold_collapse"),
            Item("Expand All", "editor.fold_expand_all"),
            Item("Collapse All", "editor.fold_collapse_all"),
            Item("Fold Selection", "editor.fold_selection"),
            Sep,
            Item("Quick Documentation", "lsp.quick_doc"),
            Item("Quick Definition", "lsp.quick_definition"),
            Item("Parameter Info", "lsp.parameter_info"),
            Item("Git Blame", "editor.toggle_blame"),
            Item("Soft-Wrap", "editor.toggle_soft_wrap"),
            Item("Inlay Hints", "lsp.toggle_inlay_hints"),
            Item("Markdown Preview", "markdown.preview"),
            Sep,
            Item("Compare with Clipboard", "editor.compare_with_clipboard"),
            Item("Compare with File…", "editor.compare_with_file"),
        ],
    ),
    (
        "Navigate",
        'N',
        &[
            Item("Search Everywhere…", "palette.search_everywhere"),
            Item("File…", "palette.files"),
            Item("Symbol…", "palette.symbols"),
            Item("File Structure…", "lsp.file_structure"),
            Item("Line…", "editor.goto_line"),
            Item("Recent Files…", "palette.recent_files"),
            Item("Recent Locations…", "palette.recent_locations"),
            Item("Switcher…", "editor.switcher"),
            Sep,
            Item("Declaration or Usages", "lsp.goto_definition"),
            Item("Implementation(s)", "lsp.goto_implementation"),
            Item("Type Declaration", "lsp.goto_type_definition"),
            Item("Declaration", "lsp.goto_declaration"),
            Item("Find Usages", "lsp.find_usages"),
            Item("Call Hierarchy", "lsp.call_hierarchy"),
            Item("Type Hierarchy", "lsp.type_hierarchy"),
            Item("File Dependencies", "hierarchy.file_dependencies"),
            Item("Export Hierarchy as Mermaid", "hierarchy.export_mermaid"),
            Item("Export Hierarchy as DOT", "hierarchy.export_dot"),
            Sep,
            Item("Back", "editor.nav_back"),
            Item("Forward", "editor.nav_forward"),
            Sep,
            Item("Toggle Bookmark", "editor.toggle_bookmark"),
            Item("Toggle Numbered Bookmark…", "editor.toggle_numbered_bookmark"),
            Item("Bookmarks…", "editor.show_bookmarks"),
            Sep,
            Item("Next Change", "editor.next_change"),
            Item("Previous Change", "editor.prev_change"),
            Item("Next Problem", "lsp.next_problem"),
            Item("Previous Problem", "lsp.prev_problem"),
            Item("Matching Bracket", "editor.goto_bracket"),
            Item("Select in Project", "tree.select_in_project"),
        ],
    ),
    (
        "Git",
        'G',
        &[
            Item("Commit…", "git.commit_box"),
            Item("Commit and Push", "git.commit_and_push"),
            Item("Push", "git.push"),
            Item("Push…", "git.push_options"),
            Item("Pull", "git.pull"),
            Item("Fetch", "git.fetch"),
            Sep,
            Item("Branches…", "git.branches_popup"),
            Item("New Branch…", "git.new_branch"),
            Sep,
            Item("Stash Changes…", "git.stash_push"),
            Item("Resolve Conflicts…", "git.resolve_conflicts"),
            Item("Continue Operation", "git.continue_operation"),
            Item("Skip Commit", "git.skip_operation"),
            Item("Abort Operation", "git.abort_operation"),
            Sep,
            Item("File History", "git.file_history"),
            Item("Revert Change", "editor.revert_change"),
        ],
    ),
    (
        "Run",
        'R',
        &[
            Item("Run…", "run.pick"),
            Item("Run Selected", "run.run_selected"),
            Item("Run Test at Caret", "run.test_at_caret"),
            Item("Send HTTP Request at Caret", "run.http_request_at_caret"),
            Item("Build SCIP Index", "run.build_scip_index"),
            Item("Rerun", "run.rerun"),
            Item("Rerun Failed Tests", "run.rerun_failed"),
            Item("Stop", "run.stop"),
            Item("Debug", "debug.start"),
            Item("Stop Debugging", "debug.stop"),
            Sep,
            Item("Edit Configurations…", "run.edit_configs"),
            Sep,
            Item("Next Problem", "run.next_problem"),
            Item("Previous Problem", "run.prev_problem"),
        ],
    ),
    (
        "Help",
        'H',
        &[
            Item("Find Action…", "palette.actions"),
            Item("Keymap Reference", "app.keymap_reference"),
            Sep,
            Item("Restart Language Servers", "lsp.restart"),
            Sep,
            Item("About jetl", "app.about"),
        ],
    ),
];

/// Whether a menu row needs an active editor tab: editor actions (but
/// Back/Forward, which reopen closed files), language server actions (but
/// Restart), file history and Select in Project. Such rows are disabled
/// while no tab is open.
pub fn needs_buffer(action: &Action) -> bool {
    match action {
        Action::Editor(a) => {
            !matches!(a, EditorAction::NavBack | EditorAction::NavForward | EditorAction::Switcher | EditorAction::SwitcherBack)
        }
        Action::Lsp(a) => !matches!(a, LspAction::Restart),
        Action::Git(GitAction::FileHistory)
        | Action::Tree(TreeAction::SelectInProject)
        | Action::App(AppAction::LocalHistory)
        | Action::App(AppAction::PutLabel) => true,
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub title: String,
    /// Letter that opens this menu while the bar is up (underlined).
    pub mnemonic: char,
    pub items: Vec<MenuItem>,
}

/// The main menu's symbol, also the tool strip's menu button. `☰` is East
/// Asian wide, so two cells everywhere; `≣` is one cell everywhere, where
/// `≡` would be ambiguous.
pub fn menu_glyph(icons: Icons) -> &'static str {
    match icons {
        Icons::Emoji => "☰",
        Icons::Unicode => "≣",
    }
}

#[derive(Debug, Clone)]
pub struct MenuBar {
    menus: Vec<Menu>,
    open: usize,
    dropdown: ContextMenu,
    /// Leading cells of the bar: the menu symbol padded.
    icon: String,
    /// The bar row and each title's cell from the last frame, for the mouse.
    bar: Rect,
    titles: Vec<Rect>,
}

impl MenuBar {
    #[cfg(test)]
    pub fn menus(&self) -> &[Menu] {
        &self.menus
    }

    /// A bar with the first menu open. `menus` must not be empty.
    pub fn new(menus: Vec<Menu>, icons: Icons) -> Self {
        let dropdown = ContextMenu::new(menus[0].items.clone(), 0, 0).shifted();
        let icon = format!(" {} ", menu_glyph(icons));
        Self { menus, open: 0, dropdown, icon, bar: Rect::default(), titles: Vec::new() }
    }

    pub fn open_title(&self) -> &str {
        &self.menus[self.open].title
    }

    pub fn dropdown(&self) -> &ContextMenu {
        &self.dropdown
    }

    fn open(&mut self, i: usize) {
        if i != self.open {
            self.open = i;
            // Anchored under its title by the next render; until then the
            // old rect keeps a click on the dropdown from closing the bar.
            let rect = self.dropdown.rect();
            self.dropdown = ContextMenu::new(self.menus[i].items.clone(), 0, 0).shifted();
            self.dropdown.set_rect(rect);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<MenuResult> {
        let n = self.menus.len();
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if let KeyCode::Char(c) = key.code
            && plain
            && let Some(i) = self.menus.iter().position(|m| m.mnemonic.eq_ignore_ascii_case(&c))
        {
            self.open(i);
            return None;
        }
        match key.code {
            KeyCode::Esc => Some(MenuResult::Cancel),
            KeyCode::Left => {
                self.open((self.open + n - 1) % n);
                None
            }
            KeyCode::Right => {
                self.open((self.open + 1) % n);
                None
            }
            _ => self.dropdown.handle_key(key),
        }
    }

    /// Whether a point is on the bar or the open dropdown.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.bar.contains((x, y).into()) || self.dropdown.contains(x, y)
    }

    pub fn handle_mouse(&mut self, m: &Mouse) -> Option<MenuResult> {
        if !self.bar.contains((m.x, m.y).into()) {
            // The dropdown runs its rows, follows the pointer and closes on
            // a press outside it.
            return self.dropdown.handle_mouse(m);
        }
        let title = self.titles.iter().position(|r| r.contains((m.x, m.y).into()));
        match (m.kind, title) {
            // A press on the open title or the icon closes, as the button would.
            (MouseEventKind::Down(_), Some(i)) if i == self.open => Some(MenuResult::Cancel),
            (MouseEventKind::Down(_), None) if m.x < self.bar.x + self.icon.width() as u16 => Some(MenuResult::Cancel),
            (MouseEventKind::Down(_) | MouseEventKind::Moved | MouseEventKind::Drag(_), Some(i)) => {
                self.open(i);
                None
            }
            _ => None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area.height < 2 {
            (self.bar, self.titles) = (Rect::default(), Vec::new());
            self.dropdown.set_rect(Rect::default());
            return;
        }
        self.bar = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(Clear, self.bar);
        let style = Style::default().bg(theme.popup_bg).fg(theme.fg);
        let buf = frame.buffer_mut();
        buf.set_style(self.bar, style);
        let icon_style = style.patch(theme.popup_border.bg(theme.popup_bg));
        let (mut x, _) = buf.set_stringn(self.bar.x, self.bar.y, &self.icon, self.bar.width as usize, icon_style);
        // A wide glyph's trailing cell was reset; paint the bar back over it.
        buf.set_style(Rect::new(self.bar.x, self.bar.y, x - self.bar.x, 1), icon_style);
        self.titles.clear();
        for (i, menu) in self.menus.iter().enumerate() {
            let w = (menu.title.width() as u16 + 2).min(self.bar.right().saturating_sub(x));
            let cell = Rect::new(x, self.bar.y, w, 1);
            let s = if i == self.open { theme.list_selected } else { style };
            buf.set_style(cell, s);
            buf.set_stringn(x + 1, cell.y, &menu.title, w.saturating_sub(1) as usize, s);
            if let Some((at, _)) = menu.title.char_indices().find(|(_, c)| c.eq_ignore_ascii_case(&menu.mnemonic)) {
                let mx = x + 1 + menu.title[..at].width() as u16;
                if mx < cell.right() {
                    buf[(mx, cell.y)].set_style(Style::default().add_modifier(Modifier::UNDERLINED));
                }
            }
            self.titles.push(cell);
            x += w;
        }
        let anchor = self.titles.get(self.open).map_or(self.bar.x, |r| r.x);
        self.dropdown.move_to(anchor, self.bar.y);
        let below = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        self.dropdown.render(frame, below, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::AppAction;
    use crossterm::event::MouseButton;

    fn bar() -> MenuBar {
        bar_with(Icons::Unicode)
    }

    fn bar_with(icons: Icons) -> MenuBar {
        let a = |id| Action::from_id(id).unwrap();
        MenuBar::new(
            vec![
                Menu { title: "File".into(), mnemonic: 'F', items: vec![MenuItem::new("Exit", a("app.quit"))] },
                Menu {
                    title: "Edit".into(),
                    mnemonic: 'E',
                    items: vec![MenuItem::new("Undo", a("editor.undo")), MenuItem::separator(), MenuItem::new("Copy", a("editor.copy"))],
                },
                Menu { title: "Help".into(), mnemonic: 'H', items: vec![MenuItem::new("About jetl", a("app.about"))] },
            ],
            icons,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn draw(b: &mut MenuBar) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();
        term.draw(|f| b.render(f, f.area(), &Theme::default())).unwrap().buffer.clone()
    }

    fn at(kind: MouseEventKind, x: u16, y: u16) -> Mouse {
        Mouse { kind, x, y, clicks: 1, mods: KeyModifiers::NONE }
    }

    #[test]
    fn glyph_widths_hold_everywhere() {
        let unicode = menu_glyph(Icons::Unicode);
        assert_eq!((unicode.width(), unicode.width_cjk()), (1, 1));
        let emoji = menu_glyph(Icons::Emoji);
        assert_eq!((emoji.width(), emoji.width_cjk()), (2, 2));
        assert_eq!(bar_with(Icons::Emoji).icon, " ☰ ");
    }

    #[test]
    fn wide_glyph_shifts_titles_and_keeps_the_bar_colour() {
        let mut b = bar_with(Icons::Emoji);
        let buf = draw(&mut b);
        // " ☰  File  Edit  Help ": the glyph takes cells 1 and 2.
        assert_eq!((buf[(1, 0)].symbol(), buf[(2, 0)].symbol()), ("☰", " "));
        assert_eq!(buf[(2, 0)].bg, buf[(0, 0)].bg, "the cell behind the glyph keeps the bar colour");
        assert_eq!(b.titles[0].x, 4);
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 3, 0)), Some(MenuResult::Cancel), "icon");
        let mut b = bar_with(Icons::Emoji);
        draw(&mut b);
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 10, 0)), None);
        assert_eq!(b.open_title(), "Edit");
    }

    #[test]
    fn left_right_wrap() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Left));
        assert_eq!(b.open_title(), "Help");
        b.handle_key(key(KeyCode::Right));
        assert_eq!(b.open_title(), "File");
        b.handle_key(key(KeyCode::Right));
        assert_eq!(b.open_title(), "Edit");
        assert_eq!(b.handle_key(key(KeyCode::Esc)), Some(MenuResult::Cancel));
        assert_eq!(b.handle_key(key(KeyCode::F(10))), None, "F10 closes through the keymap (App::main_menu_key)");
    }

    #[test]
    fn mnemonics_jump_case_insensitively() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Char('h')));
        assert_eq!(b.open_title(), "Help");
        b.handle_key(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT));
        assert_eq!(b.open_title(), "Edit");
        b.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(b.open_title(), "Edit", "not with Ctrl");
        b.handle_key(key(KeyCode::Char('x')));
        assert_eq!(b.open_title(), "Edit");
    }

    #[test]
    fn dropdown_skips_separators_and_enter_runs() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Char('e')));
        b.handle_key(key(KeyCode::Down));
        assert_eq!(b.handle_key(key(KeyCode::Enter)), Some(MenuResult::Run(Action::from_id("editor.copy").unwrap())));
        b.handle_key(key(KeyCode::Right));
        assert_eq!(b.handle_key(key(KeyCode::Char(' '))), Some(MenuResult::Run(Action::App(AppAction::About))));
    }

    #[test]
    fn mouse_switches_titles_and_cancels_outside() {
        let mut b = bar();
        draw(&mut b);
        // " ≣  File  Edit  Help ": Edit's cell starts at 9.
        assert_eq!(b.titles[1].x, 9);
        assert!(b.dropdown.contains(3, 2), "File's dropdown is under its title");
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 10, 0)), None);
        assert_eq!(b.open_title(), "Edit");
        draw(&mut b);
        assert!(b.dropdown.contains(10, 2) && !b.dropdown.contains(3, 2), "the dropdown follows");
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Moved, 16, 0)), None);
        assert_eq!(b.open_title(), "Help", "hover switches while open");
        draw(&mut b);
        assert_eq!(
            b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 17, 2)),
            Some(MenuResult::Run(Action::App(AppAction::About)))
        );
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 40, 8)), Some(MenuResult::Cancel));
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 30, 0)), None, "empty bar space");
        assert_eq!(b.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left), 16, 0)), Some(MenuResult::Cancel), "open title");
    }

    #[test]
    fn switching_menus_keeps_the_dropdown_hit_area_until_redrawn() {
        let mut b = bar();
        draw(&mut b);
        b.handle_key(key(KeyCode::Right));
        assert!(b.dropdown.contains(3, 2), "a click before the next frame stays inside");
        draw(&mut b);
        assert!(b.dropdown.contains(10, 2));
    }

    #[test]
    fn a_dropdown_past_the_right_edge_shifts_left() {
        let mut b = bar();
        b.menus[2].items[0].label = "A label long enough to overflow".into();
        b.open(2);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        term.draw(|f| b.render(f, f.area(), &Theme::default())).unwrap();
        let r = b.dropdown.rect();
        assert_eq!(r.right(), 40, "flush with the right edge, not flipped to column 0: {r:?}");
        assert!(r.x > 0);
    }

    #[test]
    fn a_too_short_area_clears_the_hit_areas() {
        let mut b = bar();
        draw(&mut b);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 1)).unwrap();
        term.draw(|f| b.render(f, f.area(), &Theme::default())).unwrap();
        assert!(!b.contains(3, 0) && !b.contains(3, 2));
    }

    #[test]
    fn view_menu_has_toolbar_before_zen() {
        let (_, _, rows) = MAIN_MENU.iter().find(|m| m.0 == "View").unwrap();
        let labels: Vec<&str> = rows.iter().filter_map(|r| if let Item(l, _) = r { Some(*l) } else { None }).collect();
        let at = |l| labels.iter().position(|x| *x == l).unwrap();
        assert_eq!(at("Toolbar") + 1, at("Zen Mode"));
    }

    #[test]
    fn run_menu_has_the_debug_rows() {
        let (_, _, rows) = MAIN_MENU.iter().find(|m| m.0 == "Run").unwrap();
        let items: Vec<(&str, &str)> = rows.iter().filter_map(|r| if let Item(l, id) = r { Some((*l, *id)) } else { None }).collect();
        let at = items.iter().position(|i| *i == ("Stop", "run.stop")).unwrap();
        assert_eq!(items[at + 1..at + 3], [("Debug", "debug.start"), ("Stop Debugging", "debug.stop")]);
    }

    #[test]
    fn view_menu_has_layout_rows_after_restore_default() {
        let (_, _, rows) = MAIN_MENU.iter().find(|m| m.0 == "View").unwrap();
        let labels: Vec<&str> = rows.iter().filter_map(|r| if let Item(l, _) = r { Some(*l) } else { None }).collect();
        let at = labels.iter().position(|x| *x == "Restore Default Layout").unwrap();
        assert_eq!(
            labels[at + 1..at + 6],
            ["Switch Layout…", "Save Current Layout as New…", "Save Changes in Current Layout", "Rename Layout…", "Delete Layout…"]
        );
    }

    #[test]
    fn edit_menu_has_the_seven_refactoring_rows_between_refactor_this_and_optimize_imports() {
        let (_, _, rows) = MAIN_MENU.iter().find(|m| m.0 == "Edit").unwrap();
        let items: Vec<(&str, &str)> = rows.iter().filter_map(|r| if let Item(l, id) = r { Some((*l, *id)) } else { None }).collect();
        let at = items.iter().position(|x| x.0 == "Refactor This…").unwrap();
        assert_eq!(
            items[at + 1..at + 9],
            [
                ("Extract Method", "lsp.extract_method"),
                ("Extract Variable", "lsp.extract_variable"),
                ("Extract Constant", "lsp.extract_constant"),
                ("Inline", "lsp.inline"),
                ("Change Signature", "lsp.change_signature"),
                ("Move", "lsp.move"),
                ("Safe Delete", "lsp.safe_delete"),
                ("Optimize Imports", "lsp.optimize_imports"),
            ]
        );
    }

    #[test]
    fn table_ids_resolve_and_menus_are_not_empty() {
        for (title, mnemonic, rows) in MAIN_MENU {
            assert!(rows.iter().any(|r| matches!(r, Item(..))), "{title} is empty");
            assert!(title.contains(*mnemonic), "{title} has no '{mnemonic}'");
            for row in *rows {
                if let Item(label, id) = row {
                    assert!(Action::from_id(id).is_some(), "{title} > {label}: unknown action {id}");
                }
            }
        }
        let mnemonics: std::collections::HashSet<_> = MAIN_MENU.iter().map(|m| m.1.to_ascii_lowercase()).collect();
        assert_eq!(mnemonics.len(), MAIN_MENU.len(), "mnemonics are unique");
    }
}
