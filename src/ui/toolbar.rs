//! The top row (`[ui] toolbar`): a left group from the left edge and a right
//! group flush right, built from `Item`s by `place` and collapsed by numbered
//! steps when the row is too narrow. A press runs the button's action.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::actions::{Action, AppAction, GitAction, PaletteKind, RunAction};
use crate::app::App;
use crate::config::Icons;
use crate::config::theme::Theme;
use crate::ui::focus::ToolWindowId;
use crate::ui::layout::ToolbarButton;
use crate::ui::menu_bar::menu_glyph;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Left,
    Right,
}

/// What a collapse step does to an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shrink {
    /// Remove the item.
    Drop,
    /// Replace the item's text.
    Text(String),
}

/// One widget of the row. `text` includes the item's own padding.
#[derive(Debug, Clone)]
pub struct Item {
    pub button: ToolbarButton,
    pub group: Group,
    pub text: String,
    pub style: Style,
    /// Collapse steps that apply to this item, by step number. An item without
    /// a `Drop` step is never dropped.
    pub shrinks: Vec<(u8, Shrink)>,
}

pub const STEP_SETTINGS: u8 = 1;
pub const STEP_SEARCH: u8 = 2;
pub const STEP_VCS: u8 = 3;
pub const STEP_PROJECT: u8 = 4;
pub const STEP_BRANCH_CUT: u8 = 5;
pub const STEP_BRANCH_DROP: u8 = 6;
/// Must stay the last step: `place` loops up to it.
pub const STEP_RUN_CONFIG_CUT: u8 = 7;

/// The VCS buttons show only on a row at least this wide.
pub const VCS_MIN_WIDTH: u16 = 100;

/// `name` cut to `cols` columns, with `…` after the cut.
pub fn shorten(name: &str, cols: usize) -> String {
    if name.width() <= cols {
        return name.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in name.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > cols {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

fn width(item: &Item) -> u16 {
    item.text.width() as u16
}

fn group_width(items: &[Item], group: Group) -> u16 {
    let mut n = 0u16;
    let mut sum = 0u16;
    for i in items.iter().filter(|i| i.group == group) {
        sum = sum.saturating_add(width(i));
        n += 1;
    }
    sum.saturating_add(n.saturating_sub(1))
}

fn needed(items: &[Item]) -> u16 {
    let both = items.iter().any(|i| i.group == Group::Left) && items.iter().any(|i| i.group == Group::Right);
    group_width(items, Group::Left).saturating_add(group_width(items, Group::Right)).saturating_add(u16::from(both))
}

/// Lays `items` out on the first row of `area`: the collapse steps in order (`STEP_SETTINGS` to
/// `STEP_RUN_CONFIG_CUT`) while
/// the row is too narrow, then clip. Returns the kept items with their final
/// text and visible cells, in input order.
pub fn place(mut items: Vec<Item>, area: Rect) -> Vec<(Item, Rect)> {
    for step in 1..=STEP_RUN_CONFIG_CUT {
        if needed(&items) <= area.width {
            break;
        }
        items.retain_mut(|item| {
            for (n, shrink) in item.shrinks.iter() {
                if *n != step {
                    continue;
                }
                match shrink {
                    Shrink::Drop => return false,
                    Shrink::Text(t) => item.text = t.clone(),
                }
            }
            true
        });
    }
    let left_first = items.iter().find(|i| i.group == Group::Left).map_or(0, width);
    let right_w = group_width(&items, Group::Right);
    let has_right = items.iter().any(|i| i.group == Group::Right);
    let right_x = area.right().saturating_sub(right_w).max(area.x.saturating_add(left_first).saturating_add(1));
    let limit = if has_right { right_x.saturating_sub(1) } else { area.right() };
    let (mut lx, mut rx) = (area.x, right_x);
    let mut out = Vec::new();
    for item in items {
        let w = width(&item);
        let (x, end) = match item.group {
            Group::Left => (&mut lx, limit),
            Group::Right => (&mut rx, area.right()),
        };
        let visible = w.min(end.saturating_sub(*x));
        let rect = Rect::new(*x, area.y, visible, 1);
        *x = x.saturating_add(w).saturating_add(1);
        if visible > 0 {
            out.push((item, rect));
        }
    }
    out
}

/// The run widget: the configuration selector, then Run when idle or Rerun
/// and Stop while a task runs. The selector names the running task, else the
/// selection.
pub fn run_items(selected: Option<&str>, running: Option<&str>, spinner: &str, theme: &Theme) -> Vec<Item> {
    let label = |name: &str| match running {
        Some(_) => format!(" {spinner} {name} ▾ "),
        None => format!(" {name} ▾ "),
    };
    let name = running.or(selected).unwrap_or("Select Configuration");
    let style = if running.is_some() { theme.strip_item.patch(theme.status_accent) } else { theme.strip_item };
    let button =
        |button, text: &str| Item { button, group: Group::Right, text: text.to_string(), style: theme.strip_item, shrinks: vec![] };
    let mut items = vec![Item {
        button: ToolbarButton::RunConfig,
        group: Group::Right,
        text: label(name),
        style,
        shrinks: vec![(STEP_RUN_CONFIG_CUT, Shrink::Text(label(&shorten(name, 8))))],
    }];
    if running.is_some() {
        items.push(button(ToolbarButton::Rerun, " ↻ "));
        items.push(button(ToolbarButton::Stop, " ■ "));
    } else {
        items.push(button(ToolbarButton::Run, " ▶ "));
    }
    items
}

/// The branch widget's text: ` ⎇ main ▾ `, or ` ⎇ main (Rebasing 2/3) ▾ ` during an operation.
pub fn branch_text(icon: &str, name: &str, tag: Option<&str>) -> String {
    let tag = tag.map(|t| format!(" ({t})")).unwrap_or_default();
    format!(" {icon} {name}{tag} ▾ ")
}

pub fn search_glyph(icons: Icons) -> &'static str {
    match icons {
        Icons::Unicode => "⌕",
        Icons::Emoji => "🔎",
    }
}

pub fn settings_glyph(icons: Icons) -> &'static str {
    match icons {
        Icons::Unicode => "⚙",
        Icons::Emoji => "🔧",
    }
}

/// The glyph of a VCS button (`""` for any other button).
pub fn vcs_glyph(button: ToolbarButton, icons: Icons) -> &'static str {
    match (button, icons) {
        (ToolbarButton::Update, Icons::Unicode) => "↓",
        (ToolbarButton::Update, Icons::Emoji) => "📥",
        (ToolbarButton::Commit, Icons::Unicode) => "✓",
        (ToolbarButton::Commit, Icons::Emoji) => "✅",
        (ToolbarButton::Push, Icons::Unicode) => "↑",
        (ToolbarButton::Push, Icons::Emoji) => "📤",
        _ => "",
    }
}

/// The Update, Commit and Push buttons; none on a row under `VCS_MIN_WIDTH`.
pub fn vcs_items(icons: Icons, theme: &Theme, width: u16) -> Vec<Item> {
    if width < VCS_MIN_WIDTH {
        return Vec::new();
    }
    [ToolbarButton::Update, ToolbarButton::Commit, ToolbarButton::Push]
        .into_iter()
        .map(|button| Item {
            button,
            group: Group::Left,
            text: format!(" {} ", vcs_glyph(button, icons)),
            style: theme.strip_item,
            shrinks: vec![(STEP_VCS, Shrink::Drop)],
        })
        .collect()
}

fn dir_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The Project widget's label: the root's directory name, with the main
/// worktree's name (or `worktree`) after it when the root is a linked worktree.
pub fn project_label(root: &std::path::Path, worktrees: &crate::worktree::WorktreePanel) -> String {
    let name = dir_name(root);
    let linked = worktrees.current.and_then(|i| worktrees.list.get(i)).is_some_and(|w| !w.is_main);
    match worktrees.main_root() {
        Some(main) if linked => {
            let main = dir_name(main);
            if main == name { format!("{name} (worktree)") } else { format!("{name} ({main})") }
        }
        _ => name,
    }
}

/// The widgets in group order (left, then right).
fn items(app: &App, width: u16) -> Vec<Item> {
    let mut items = vec![
        Item {
            button: ToolbarButton::Menu,
            group: Group::Left,
            text: format!(" {} ", menu_glyph(app.config.ui.icons)),
            style: app.theme.strip_item,
            shrinks: vec![],
        },
        Item {
            button: ToolbarButton::Project,
            group: Group::Left,
            text: format!(" {} ▾ ", project_label(&app.workspace.root, &app.worktrees)),
            style: app.theme.strip_item,
            shrinks: vec![(STEP_PROJECT, Shrink::Text(" ▾ ".into()))],
        },
    ];
    if let Some(name) = app.branch_label() {
        let icon = ToolWindowId::Git.icon(app.config.ui.icons);
        let tag = app.in_progress().map(|ip| ip.label());
        let style = if tag.is_some() { app.theme.strip_item_active } else { app.theme.strip_item };
        items.push(Item {
            button: ToolbarButton::Branch,
            group: Group::Left,
            text: branch_text(icon, &name, tag.as_deref()),
            style,
            shrinks: vec![
                (STEP_BRANCH_CUT, Shrink::Text(branch_text(icon, &shorten(&name, 12), tag.as_deref()))),
                (STEP_BRANCH_DROP, Shrink::Drop),
            ],
        });
        items.extend(vcs_items(app.config.ui.icons, &app.theme, width));
    }
    items.extend(run_items(app.run.selected.as_deref(), app.run.running_name(), app.spinner(), &app.theme));
    items.push(Item {
        button: ToolbarButton::Search,
        group: Group::Right,
        text: format!(" {} ", search_glyph(app.config.ui.icons)),
        style: app.theme.strip_item,
        shrinks: vec![(STEP_SEARCH, Shrink::Drop)],
    });
    items.push(Item {
        button: ToolbarButton::Settings,
        group: Group::Right,
        text: format!(" {} ", settings_glyph(app.config.ui.icons)),
        style: app.theme.strip_item,
        shrinks: vec![(STEP_SETTINGS, Shrink::Drop)],
    });
    items
}

/// The action a press on `button` runs.
pub fn action(button: ToolbarButton) -> Action {
    match button {
        ToolbarButton::Menu => Action::App(AppAction::MainMenu),
        ToolbarButton::RunConfig => Action::App(AppAction::OpenPalette(PaletteKind::RunTasks)),
        ToolbarButton::Run => Action::Run(RunAction::RunSelected),
        ToolbarButton::Rerun => Action::Run(RunAction::Rerun),
        ToolbarButton::Stop => Action::Run(RunAction::Stop),
        ToolbarButton::Branch => Action::Git(GitAction::Branches),
        ToolbarButton::Update => Action::Git(GitAction::Pull),
        ToolbarButton::Commit => Action::Git(GitAction::CommitBox),
        ToolbarButton::Push => Action::Git(GitAction::Push),
        ToolbarButton::Project => Action::App(AppAction::OpenPalette(PaletteKind::RecentProjects)),
        ToolbarButton::Search => Action::App(AppAction::OpenPalette(PaletteKind::Everywhere)),
        ToolbarButton::Settings => Action::App(AppAction::Settings),
    }
}

/// The action a press on `button` at `rect` runs: the run configuration
/// widget opens its dropdown under itself; the rest run [`action`].
pub fn press(button: ToolbarButton, rect: Rect) -> Action {
    match button {
        ToolbarButton::RunConfig => Action::Run(RunAction::ConfigMenu { x: rect.x, y: rect.y }),
        b => action(b),
    }
}

/// The name shown in the tooltip.
pub fn title(button: ToolbarButton) -> &'static str {
    match button {
        ToolbarButton::Menu => "Main Menu",
        ToolbarButton::RunConfig => "Run Configuration",
        ToolbarButton::Run => "Run",
        ToolbarButton::Rerun => "Rerun",
        ToolbarButton::Stop => "Stop",
        ToolbarButton::Branch => "Branches",
        ToolbarButton::Update => "Update Project",
        ToolbarButton::Commit => "Commit",
        ToolbarButton::Push => "Push",
        ToolbarButton::Project => "Recent Projects",
        ToolbarButton::Search => "Search Everywhere",
        ToolbarButton::Settings => "Settings",
    }
}

pub fn render(app: &mut App, frame: &mut Frame, area: Rect) {
    app.layout.toolbar_hits.clear();
    if area.is_empty() {
        return;
    }
    let placed = place(items(app, area.width), area);
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::default().bg(app.theme.strip_bg));
    for (item, rect) in placed {
        buf.set_style(rect, item.style);
        let under = buf[(rect.x, rect.y)].style();
        let (end, _) = buf.set_stringn(rect.x, rect.y, &item.text, rect.width as usize, item.style);
        // set_stringn resets the cell behind a wide glyph, colours included.
        buf.set_style(Rect::new(rect.x, rect.y, end - rect.x, 1), under);
        app.layout.toolbar_hits.push((item.button, rect));
    }
}

/// The hovered button's name and shortcut on the row below it. Hidden while
/// an overlay is open, and once the button is gone from the row.
pub fn render_tooltip(app: &App, frame: &mut Frame, area: Rect) {
    let Some(hover) = app.toolbar_hover else { return };
    if crate::ui::tool_strip::overlay_open(app) || !app.layout.toolbar_hits.contains(&hover) {
        return;
    }
    let (button, rect) = hover;
    let key = app.shortcut(&action(button)).map(|k| format!(" {k} ")).unwrap_or_default();
    let theme = &app.theme;
    let base = Style::default().bg(theme.popup_bg).fg(theme.fg);
    let name = format!(" {} ", title(button));
    let w = (name.width() + key.width()) as u16;
    let y = rect.bottom();
    if y >= area.bottom() {
        return;
    }
    let x = rect.x.min(area.right().saturating_sub(w));
    let room = area.right().saturating_sub(x);
    let buf = frame.buffer_mut();
    buf.set_style(Rect::new(x, y, w.min(room), 1), base);
    let (end, _) = buf.set_stringn(x, y, &name, room as usize, base);
    buf.set_stringn(end, y, &key, (x + room).saturating_sub(end) as usize, base.patch(theme.dim));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(group: Group, text: &str, shrinks: Vec<(u8, Shrink)>) -> Item {
        Item { button: ToolbarButton::Menu, group, text: text.into(), style: Style::default(), shrinks }
    }

    fn xs(placed: &[(Item, Rect)]) -> Vec<(u16, u16)> {
        placed.iter().map(|(_, r)| (r.x, r.width)).collect()
    }

    #[test]
    fn groups_sit_at_their_edges() {
        let items = vec![
            it(Group::Left, "aaa", vec![]),
            it(Group::Left, "bb", vec![]),
            it(Group::Right, "cccc", vec![]),
            it(Group::Right, "dd", vec![]),
        ];
        let placed = place(items, Rect::new(0, 0, 40, 1));
        // Right group: 4 + 1 + 2 = 7 columns, flush right.
        assert_eq!(xs(&placed), vec![(0, 3), (4, 2), (33, 4), (38, 2)]);
    }

    #[test]
    fn steps_stop_once_the_row_fits() {
        let items = || {
            vec![
                it(Group::Left, "menu", vec![]),
                it(Group::Right, "set", vec![(STEP_SETTINGS, Shrink::Drop)]),
                it(Group::Right, "search", vec![(STEP_SEARCH, Shrink::Drop)]),
            ]
        };
        // 4 + 1 + (3 + 1 + 6) = 15 needed.
        assert_eq!(place(items(), Rect::new(0, 0, 15, 1)).len(), 3);
        let placed = place(items(), Rect::new(0, 0, 13, 1));
        let texts: Vec<_> = placed.iter().map(|(i, _)| i.text.as_str()).collect();
        assert_eq!(texts, ["menu", "search"]);
    }

    #[test]
    fn text_shrink_applies_only_when_earlier_steps_are_not_enough() {
        let items = || {
            vec![
                it(Group::Left, "menu", vec![]),
                it(Group::Left, "project", vec![(STEP_PROJECT, Shrink::Text("▾".into()))]),
                it(Group::Right, "set", vec![(STEP_SETTINGS, Shrink::Drop)]),
            ]
        };
        // 4 + 1 + 7 + 1 + 3 = 16 needed; dropping set gives 12.
        let placed = place(items(), Rect::new(0, 0, 12, 1));
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].0.text, "project");
        let placed = place(items(), Rect::new(0, 0, 8, 1));
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].0.text, "▾");
    }

    #[test]
    fn what_still_does_not_fit_is_clipped_and_the_menu_is_kept() {
        let items = vec![
            it(Group::Left, "menu", vec![]),
            it(Group::Left, "branch", vec![]),
            it(Group::Left, "lost", vec![]),
            it(Group::Right, "run", vec![]),
        ];
        // right_x = 14 - 3 = 11; left limit 10.
        let placed = place(items, Rect::new(0, 0, 14, 1));
        assert_eq!(xs(&placed), vec![(0, 4), (5, 5), (11, 3)]);
        let placed = place(vec![it(Group::Left, "menu", vec![]), it(Group::Right, "run-config", vec![])], Rect::new(0, 0, 9, 1));
        assert_eq!(placed[0].1, Rect::new(0, 0, 4, 1));
    }

    fn texts(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.text.as_str()).collect()
    }

    #[test]
    fn run_items_idle_and_running() {
        let t = Theme::default();
        let idle = run_items(None, None, "⠋", &t);
        let buttons: Vec<_> = idle.iter().map(|i| i.button).collect();
        assert_eq!(buttons, [ToolbarButton::RunConfig, ToolbarButton::Run]);
        assert_eq!(texts(&idle), [" Select Configuration ▾ ", " ▶ "]);
        assert_eq!(texts(&run_items(Some("check"), None, "⠋", &t)), [" check ▾ ", " ▶ "]);
        let run = run_items(Some("check"), Some("cargo test"), "⠋", &t);
        let buttons: Vec<_> = run.iter().map(|i| i.button).collect();
        assert_eq!(buttons, [ToolbarButton::RunConfig, ToolbarButton::Rerun, ToolbarButton::Stop]);
        assert_eq!(texts(&run), [" ⠋ cargo test ▾ ", " ↻ ", " ■ "]);
        assert_eq!(run[0].style, t.strip_item.patch(t.status_accent));
    }

    #[test]
    fn run_config_name_is_cut_last_and_buttons_stay() {
        let t = Theme::default();
        let with = |running: Option<&str>| {
            let mut items = vec![it(Group::Left, " ≣ ", vec![])];
            items.extend(run_items(Some("integration-tests"), running, "⠋", &t));
            items
        };
        let placed = place(with(None), Rect::new(0, 0, 40, 1));
        assert_eq!(placed[1].0.text, " integration-tests ▾ ");
        let placed = place(with(None), Rect::new(0, 0, 22, 1));
        assert_eq!(placed[1].0.text, " integrat… ▾ ");
        assert_eq!(placed[2].1, Rect::new(19, 0, 3, 1));
        let placed = place(with(Some("integration-tests")), Rect::new(0, 0, 28, 1));
        assert_eq!(placed[1].0.text, " ⠋ integrat… ▾ ");
        assert_eq!((placed[2].1.width, placed[3].1.width), (3, 3));
        assert_eq!(placed[3].1.right(), 28);
    }

    #[test]
    fn run_actions_map_to_the_run_ids() {
        assert_eq!(action(ToolbarButton::RunConfig), Action::App(AppAction::OpenPalette(PaletteKind::RunTasks)));
        assert_eq!(action(ToolbarButton::Run), Action::Run(RunAction::RunSelected));
        assert_eq!(action(ToolbarButton::Rerun), Action::Run(RunAction::Rerun));
        assert_eq!(action(ToolbarButton::Stop), Action::Run(RunAction::Stop));
    }

    #[test]
    fn press_anchors_the_run_config_dropdown() {
        assert_eq!(press(ToolbarButton::RunConfig, Rect::new(70, 0, 12, 1)), Action::Run(RunAction::ConfigMenu { x: 70, y: 0 }));
        assert_eq!(press(ToolbarButton::Run, Rect::default()), action(ToolbarButton::Run));
    }

    #[test]
    fn branch_text_has_the_tag_and_the_arrow() {
        assert_eq!(branch_text("⎇", "main", None), " ⎇ main ▾ ");
        assert_eq!(branch_text("⎇", "main", Some("Rebasing 2/3")), " ⎇ main (Rebasing 2/3) ▾ ");
    }

    #[test]
    fn branch_name_is_cut_then_dropped() {
        let items = || {
            let mut branch = it(
                Group::Left,
                &branch_text("⎇", "feature/top-bar", None),
                vec![
                    (STEP_BRANCH_CUT, Shrink::Text(branch_text("⎇", &shorten("feature/top-bar", 12), None))),
                    (STEP_BRANCH_DROP, Shrink::Drop),
                ],
            );
            branch.button = ToolbarButton::Branch;
            vec![it(Group::Left, " ≣ ", vec![]), branch, it(Group::Right, "run", vec![])]
        };
        let kept = |w| place(items(), Rect::new(0, 0, w, 1)).into_iter().map(|(i, _)| i.text).collect::<Vec<_>>();
        assert_eq!(kept(29)[1], " ⎇ feature/top-bar ▾ ");
        assert_eq!(kept(27)[1], " ⎇ feature/top-… ▾ ");
        assert_eq!(kept(26), [" ≣ ", "run"]);
    }

    fn wt(path: &str, is_main: bool) -> crate::worktree::Worktree {
        crate::worktree::Worktree {
            path: path.into(),
            head: String::new(),
            branch: Some("x".into()),
            detached: false,
            bare: false,
            locked: None,
            prunable: None,
            is_main,
        }
    }

    #[test]
    fn project_label_names_the_root_and_a_linked_worktree() {
        use crate::worktree::WorktreePanel;
        use std::path::Path;
        let label = |list: Vec<crate::worktree::Worktree>, root: &str| {
            let mut p = WorktreePanel::new();
            if !list.is_empty() {
                p.set_list(Ok(list), Path::new(root));
            }
            project_label(Path::new(root), &p)
        };
        assert_eq!(label(vec![], "/w/jetl"), "jetl");
        assert_eq!(label(vec![wt("/w/jetl", true)], "/w/jetl"), "jetl");
        let linked = vec![wt("/src/jetl", true), wt("/src/jetl/.worktrees/feat", false)];
        assert_eq!(label(linked, "/src/jetl/.worktrees/feat"), "feat (jetl)");
        let same = vec![wt("/a/jetl", true), wt("/b/jetl", false)];
        assert_eq!(label(same, "/b/jetl"), "jetl (worktree)");
    }

    #[test]
    fn glyphs_have_one_width_per_icon_mode() {
        let vcs = [ToolbarButton::Update, ToolbarButton::Commit, ToolbarButton::Push];
        for (icons, w) in [(Icons::Unicode, 1), (Icons::Emoji, 2)] {
            for g in [search_glyph(icons), settings_glyph(icons)] {
                assert_eq!((g.width(), g.width_cjk()), (w, w), "{g}");
            }
            assert_ne!(search_glyph(icons), settings_glyph(icons));
            let mut seen = vec![search_glyph(icons), settings_glyph(icons)];
            for b in vcs {
                let g = vcs_glyph(b, icons);
                assert_eq!(g.width(), w, "{g}");
                // The Unicode arrows are East Asian Ambiguous (wide under width_cjk), as in the find bar.
                if icons == Icons::Emoji {
                    assert_eq!(g.width_cjk(), w, "{g}");
                }
                assert!(!seen.contains(&g), "{g} repeats");
                seen.push(g);
            }
        }
    }

    #[test]
    fn vcs_items_from_100_columns() {
        let t = Theme::default();
        assert!(vcs_items(Icons::Unicode, &t, VCS_MIN_WIDTH - 1).is_empty());
        let v = vcs_items(Icons::Unicode, &t, VCS_MIN_WIDTH);
        let buttons: Vec<_> = v.iter().map(|i| i.button).collect();
        assert_eq!(buttons, [ToolbarButton::Update, ToolbarButton::Commit, ToolbarButton::Push]);
        let texts: Vec<_> = v.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, [" ↓ ", " ✓ ", " ↑ "]);
        assert!(v.iter().all(|i| i.group == Group::Left && i.shrinks == vec![(STEP_VCS, Shrink::Drop)]));
    }

    #[test]
    fn vcs_actions_map_to_the_git_ids() {
        assert_eq!(action(ToolbarButton::Update), Action::Git(GitAction::Pull));
        assert_eq!(action(ToolbarButton::Commit), Action::Git(GitAction::CommitBox));
        assert_eq!(action(ToolbarButton::Push), Action::Git(GitAction::Push));
    }

    #[test]
    fn vcs_buttons_drop_after_search_and_before_project() {
        let items = || {
            let mut v = vec![it(Group::Left, " ≣ ", vec![]), it(Group::Left, "project", vec![(STEP_PROJECT, Shrink::Text("▾".into()))])];
            v.extend((0..3).map(|_| it(Group::Left, " x ", vec![(STEP_VCS, Shrink::Drop)])));
            v.push(it(Group::Right, "sch", vec![(STEP_SEARCH, Shrink::Drop)]));
            v.push(it(Group::Right, "set", vec![(STEP_SETTINGS, Shrink::Drop)]));
            v
        };
        let kept = |w| place(items(), Rect::new(0, 0, w, 1)).into_iter().map(|(i, _)| i.text).collect::<Vec<_>>();
        assert_eq!(kept(31).len(), 7);
        let k = kept(27);
        assert_eq!((k.len(), k.contains(&"set".to_string())), (6, false));
        let k = kept(23);
        assert_eq!((k.len(), k.contains(&"sch".to_string())), (5, false));
        assert_eq!(kept(22), [" ≣ ", "project"]);
        assert_eq!(kept(10), [" ≣ ", "▾"]);
    }

    #[test]
    fn shorten_cuts_by_width() {
        assert_eq!(shorten("feature/top-bar", 12), "feature/top-…");
        assert_eq!(shorten("main", 12), "main");
        let cut = shorten("日本語日本語日本語", 5);
        assert!(cut.width() <= 6, "{cut}");
        assert_eq!(cut, "日本…");
    }
}
