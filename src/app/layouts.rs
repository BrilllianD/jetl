//! Named tool window layouts in the app: the cached list, the View menu
//! actions and their dialogs (`layouts.rs` owns the file).

use super::App;
use crate::event::{AppMsg, Level};
use crate::layouts::{LayoutOp, NamedLayout};
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::workspace::NOT_PLACED;

impl App {
    /// Read the layouts file off the main thread (`main.rs` at startup; tests
    /// never call it).
    pub fn load_layouts(&mut self) {
        self.layouts_request(None);
    }

    fn layouts_request(&mut self, op: Option<LayoutOp>) {
        let (file, tx) = (self.paths.layouts_file(), self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let (version, layouts, error) = crate::layouts::update(&file, op.as_ref());
            let _ = tx.send(AppMsg::Layouts { version, layouts, error });
        });
    }

    pub(super) fn layouts_loaded(&mut self, version: u64, layouts: Vec<NamedLayout>, error: Option<String>) {
        if let Some(e) = error {
            self.notify(Level::Warn, e);
        }
        if version <= self.layouts_version {
            tracing::debug!("dropping stale layouts");
            return;
        }
        self.layouts_version = version;
        self.named_layouts = layouts;
    }

    fn layout_names(&self) -> Vec<String> {
        self.named_layouts.iter().map(|l| l.name.clone()).collect()
    }

    /// Tell the user there is nothing saved; true when so.
    fn no_layouts(&mut self) -> bool {
        let none = self.named_layouts.is_empty();
        if none {
            self.notify(Level::Info, "No saved tool window layouts");
        }
        none
    }

    pub(super) fn switch_layout(&mut self) {
        if self.no_layouts() {
            return;
        }
        let names = self.layout_names();
        let items =
            names.iter().map(|n| if self.current_layout.as_deref() == Some(n) { format!("{n} (current)") } else { n.clone() }).collect();
        self.dialog = Some(Dialog::choice(DialogId::LayoutSwitch(names), "Switch Layout", "Apply a saved tool window layout:", items));
    }

    pub(super) fn save_layout_as_new(&mut self) {
        self.dialog = Some(Dialog::input(DialogId::LayoutSaveAsNew, "Save Layout as New", "Name of the new layout:", "", "Save"));
    }

    pub(super) fn save_layout_changes(&mut self) {
        match self.current_layout.clone() {
            Some(name) => {
                self.put(name.clone());
                self.notify(Level::Info, format!("Saved layout '{name}'"));
            }
            None => self.notify(Level::Info, "No current layout: use Save Current Layout as New"),
        }
    }

    fn put(&mut self, name: String) {
        let layout = self.layout_placement();
        self.layouts_request(Some(LayoutOp::Put(NamedLayout { name, layout })));
    }

    pub(super) fn rename_layout(&mut self) {
        if self.no_layouts() {
            return;
        }
        let names = self.layout_names();
        self.dialog = Some(Dialog::choice(DialogId::LayoutRenamePick(names.clone()), "Rename Layout", "Layout to rename:", names));
    }

    pub(super) fn delete_layout(&mut self) {
        if self.no_layouts() {
            return;
        }
        let names = self.layout_names();
        self.dialog = Some(Dialog::choice(DialogId::LayoutDeletePick(names.clone()), "Delete Layout", "Layout to delete:", names));
    }

    /// Apply a saved layout: leave zen mode, then place the windows; TODO,
    /// Tasks and Hierarchy keep their dock and stay shown unless the layout
    /// puts another window in their slot.
    pub(crate) fn apply_named_layout(&mut self, name: &str) {
        let Some(layout) = self.named_layouts.iter().find(|l| l.name == name).map(|l| l.layout.clone()) else {
            self.notify(Level::Warn, format!("No layout named '{name}'"));
            return;
        };
        self.layout.leave_zen();
        let kept = NOT_PLACED.map(|tw| (tw, self.layout.dock_of(tw), self.layout.is_visible(tw)));
        self.apply_placement(&layout, &kept);
        self.current_layout = Some(name.to_string());
    }

    /// The answers of the layout dialogs.
    pub(super) fn finish_layouts_dialog(&mut self, id: &DialogId, res: &DialogResult) -> bool {
        let text = || match res {
            DialogResult::Text(t) => Some(t.trim().to_string()).filter(|t| !t.is_empty()),
            _ => None,
        };
        let pick = |names: &[String]| match res {
            DialogResult::Choice(i) => names.get(*i).cloned(),
            _ => None,
        };
        match id {
            DialogId::LayoutSwitch(names) => {
                if let Some(n) = pick(names) {
                    self.apply_named_layout(&n);
                }
            }
            DialogId::LayoutSaveAsNew => {
                if let Some(name) = text() {
                    if self.named_layouts.iter().any(|l| l.name == name) {
                        self.notify(Level::Warn, format!("A layout named '{name}' already exists"));
                    } else {
                        let layout = self.layout_placement();
                        self.layouts_request(Some(LayoutOp::Create(NamedLayout { name: name.clone(), layout })));
                        self.notify(Level::Info, format!("Saved layout '{name}'"));
                        self.current_layout = Some(name);
                    }
                }
            }
            DialogId::LayoutRenamePick(names) => {
                if let Some(old) = pick(names) {
                    self.dialog = Some(Dialog::input(
                        DialogId::LayoutRename(old.clone()),
                        "Rename Layout",
                        &format!("New name for '{old}':"),
                        &old,
                        "Rename",
                    ));
                }
            }
            DialogId::LayoutRename(old) => {
                if let Some(new) = text().filter(|n| n != old) {
                    if self.named_layouts.iter().any(|l| l.name == new) {
                        self.notify(Level::Warn, format!("A layout named '{new}' already exists"));
                    } else {
                        self.layouts_request(Some(LayoutOp::Rename { from: old.clone(), to: new.clone() }));
                        if self.current_layout.as_ref() == Some(old) {
                            self.current_layout = Some(new);
                        }
                    }
                }
            }
            DialogId::LayoutDeletePick(names) => {
                if let Some(name) = pick(names) {
                    self.dialog = Some(Dialog::confirm(
                        DialogId::LayoutDelete(name.clone()),
                        "Delete Layout",
                        &format!("Delete layout '{name}'?"),
                        &["Delete", "Cancel"],
                    ));
                }
            }
            DialogId::LayoutDelete(name) => {
                if matches!(res, DialogResult::Button(0)) {
                    self.layouts_request(Some(LayoutOp::Delete(name.clone())));
                    if self.current_layout.as_ref() == Some(name) {
                        self.current_layout = None;
                    }
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::actions::{Action, AppAction};
    use crate::app::tests::test_app;
    use crate::ui::focus::{Dock, Side, ToolWindowId};
    use crate::ui::layout::ViewMode;

    /// Handle bus messages up to and including the first `Layouts` reply.
    async fn settle_layouts(app: &mut App) {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), app.bus_rx.recv()).await.expect("no layouts reply").expect("bus closed");
            let layouts = matches!(msg, AppMsg::Layouts { .. });
            app.handle_msg(msg);
            if layouts {
                return;
            }
        }
    }

    fn answer(app: &mut App, res: DialogResult) {
        let d = app.dialog.take().expect("no dialog");
        app.finish_dialog(d.id, res);
    }

    fn names(app: &App) -> Vec<String> {
        app.layout_names()
    }

    async fn save_as(app: &mut App, name: &str) {
        app.perform(Action::App(AppAction::SaveLayoutAsNew));
        answer(app, DialogResult::Text(name.into()));
        settle_layouts(app).await;
    }

    fn last_warning(app: &App) -> Option<&str> {
        app.notifications.iter().rev().find(|n| n.level == Level::Warn).map(|n| n.text.as_str())
    }

    #[tokio::test]
    async fn save_as_new_then_switch_restores_the_placement() {
        use ToolWindowId::*;
        let (mut app, dir) = test_app("layouts-switch");
        app.toggle_tool_window(Git);
        app.move_tool_window(Git, Dock { side: Side::Right, split: true });
        app.layout.set_view_mode(Find, ViewMode::Unpinned);
        app.layout.set_view_mode(Problems, ViewMode::Undocked);
        app.layout.left_w = 33;
        app.layout.move_button(Git, true);
        save_as(&mut app, "  Review ").await;
        assert_eq!(names(&app), ["Review"]);
        assert_eq!(app.current_layout.as_deref(), Some("Review"));
        let text = std::fs::read_to_string(app.paths.layouts_file()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["layouts"][0]["name"], "Review");
        assert!(v["layouts"][0]["layout"].get("git_tab").is_none());
        let saved = app.layout_placement();
        app.restore_default_layout();
        app.layout.left_w = 20;
        app.current_layout = None;
        app.perform(Action::App(AppAction::SwitchLayout));
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::LayoutSwitch(vec!["Review".into()]));
        answer(&mut app, DialogResult::Choice(0));
        assert_eq!(app.layout_placement(), saved);
        assert_eq!(app.layout.dock_of(Git), Dock { side: Side::Right, split: true });
        assert_eq!(app.layout.view_mode(Problems), ViewMode::Undocked);
        assert_eq!(app.current_layout.as_deref(), Some("Review"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_as_new_refuses_a_taken_name_and_save_changes_overwrites() {
        let (mut app, dir) = test_app("layouts-overwrite");
        save_as(&mut app, "A").await;
        app.perform(Action::App(AppAction::SaveLayoutAsNew));
        answer(&mut app, DialogResult::Text("A".into()));
        assert!(last_warning(&app).is_some_and(|w| w.contains("already exists")));
        app.layout.left_w = 41;
        app.perform(Action::App(AppAction::SaveLayoutChanges));
        settle_layouts(&mut app).await;
        assert_eq!(names(&app), ["A"]);
        assert_eq!(app.named_layouts[0].layout, app.layout_placement());
        assert_eq!(app.named_layouts[0].layout.left_w, 41);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rename_and_delete_go_through_pick_dialogs() {
        let (mut app, dir) = test_app("layouts-rename");
        save_as(&mut app, "A").await;
        save_as(&mut app, "B").await;
        app.perform(Action::App(AppAction::RenameLayout));
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::LayoutRenamePick(vec!["A".into(), "B".into()]));
        answer(&mut app, DialogResult::Choice(0));
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::LayoutRename("A".into()));
        answer(&mut app, DialogResult::Text("B".into()));
        assert!(last_warning(&app).is_some_and(|w| w.contains("already exists")));
        assert_eq!(names(&app), ["A", "B"]);
        app.perform(Action::App(AppAction::RenameLayout));
        answer(&mut app, DialogResult::Choice(0));
        answer(&mut app, DialogResult::Text("C".into()));
        settle_layouts(&mut app).await;
        assert_eq!(names(&app), ["C", "B"]);
        assert_eq!(app.current_layout.as_deref(), Some("B"));
        app.perform(Action::App(AppAction::DeleteLayout));
        answer(&mut app, DialogResult::Choice(1));
        assert_eq!(app.dialog.as_ref().unwrap().id, DialogId::LayoutDelete("B".into()));
        answer(&mut app, DialogResult::Button(1));
        assert_eq!(names(&app), ["C", "B"]);
        assert_eq!(app.current_layout.as_deref(), Some("B"));
        app.perform(Action::App(AppAction::DeleteLayout));
        answer(&mut app, DialogResult::Choice(1));
        answer(&mut app, DialogResult::Button(0));
        settle_layouts(&mut app).await;
        assert_eq!(names(&app), ["C"]);
        assert_eq!(app.current_layout, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn view_row_enabled(app: &App, label: &str) -> bool {
        let bar = app.build_main_menu();
        let item = bar.menus().iter().flat_map(|m| m.items.iter()).find(|i| i.label == label).expect("menu row");
        item.enabled
    }

    #[tokio::test]
    async fn layout_actions_without_layouts_toast_and_menu_rows_are_disabled() {
        let (mut app, dir) = test_app("layouts-empty");
        for label in ["Switch Layout…", "Rename Layout…", "Delete Layout…", "Save Changes in Current Layout"] {
            assert!(!view_row_enabled(&app, label), "{label}");
        }
        assert!(view_row_enabled(&app, "Save Current Layout as New…"));
        app.perform(Action::App(AppAction::SwitchLayout));
        assert!(app.dialog.is_none());
        assert!(app.notifications.iter().any(|n| n.level == Level::Info && n.text == "No saved tool window layouts"));
        save_as(&mut app, "A").await;
        for label in ["Switch Layout…", "Rename Layout…", "Delete Layout…", "Save Changes in Current Layout"] {
            assert!(view_row_enabled(&app, label), "{label}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn applying_a_layout_keeps_tasks_todo_and_hierarchy() {
        use ToolWindowId::*;
        let (mut app, dir) = test_app("layouts-kept");
        app.layout.hide_all();
        app.layout.show(Project);
        save_as(&mut app, "Plain").await;
        app.toggle_tool_window(Todo);
        assert!(app.layout.is_visible(Todo));
        app.apply_named_layout("Plain");
        assert!(app.layout.is_visible(Todo));
        app.layout.hide(Todo);
        let todo_dock = app.layout.dock_of(Todo);
        app.move_tool_window(Run, todo_dock);
        app.layout.show(Run);
        save_as(&mut app, "Bottom").await;
        app.layout.hide(Run);
        app.toggle_tool_window(Todo);
        app.apply_named_layout("Bottom");
        assert!(app.layout.is_visible(Run));
        assert!(!app.layout.is_visible(Todo));
        assert_eq!(app.layout.dock_of(Todo), todo_dock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_layouts_reply_is_dropped() {
        let (mut app, dir) = test_app("layouts-stale");
        let a = NamedLayout { name: "a".into(), layout: app.layout_placement() };
        app.handle_msg(AppMsg::Layouts { version: 5, layouts: vec![a.clone()], error: None });
        app.handle_msg(AppMsg::Layouts { version: 3, layouts: vec![], error: None });
        assert_eq!(app.named_layouts, [a]);
        app.handle_msg(AppMsg::Layouts { version: 6, layouts: vec![], error: Some("x".into()) });
        assert_eq!(last_warning(&app), Some("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn applying_leaves_zen() {
        use ToolWindowId::*;
        let (mut app, dir) = test_app("layouts-zen");
        app.toggle_tool_window(Git);
        save_as(&mut app, "Z").await;
        app.perform(Action::App(AppAction::ToggleZen));
        assert!(app.layout.zen.is_some());
        app.apply_named_layout("Z");
        assert!(app.layout.zen.is_none());
        assert!(app.layout.is_visible(Git));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn layout_action_ids_resolve() {
        for (id, a) in [
            ("tool_window.switch_layout", AppAction::SwitchLayout),
            ("tool_window.save_layout_as_new", AppAction::SaveLayoutAsNew),
            ("tool_window.save_layout_changes", AppAction::SaveLayoutChanges),
            ("tool_window.rename_layout", AppAction::RenameLayout),
            ("tool_window.delete_layout", AppAction::DeleteLayout),
        ] {
            assert_eq!(Action::from_id(id), Some(Action::App(a)));
        }
    }
}
