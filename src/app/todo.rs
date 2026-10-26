//! The TODO window in the app: the first show of a root starts its scan
//! (`files/todo.rs` holds the panel).

use super::{App, Ctx};

impl App {
    /// From the TODO window's render arm: the first show of a root starts its scan.
    pub(crate) fn sync_todo(&mut self) {
        if self.todo.started() {
            return;
        }
        let mut cx = ctx!(self);
        self.todo.scan(&mut cx);
        let fx = cx.effects;
        self.apply_effects(fx);
    }
}

#[cfg(test)]
mod tests {
    use crate::app::tests::{handle_next, init_repo, test_app};
    use crate::event::AppMsg;
    use crate::ui::focus::ToolWindowId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    async fn scan_done(app: &mut crate::app::App) {
        while app.todo.scanning() {
            handle_next(app, |m| matches!(m, AppMsg::Todo { .. })).await;
        }
    }

    #[tokio::test]
    async fn first_show_scans_and_enter_opens_the_match() {
        let (mut app, dir) = test_app("todo-open");
        let root = app.workspace.root.clone();
        init_repo(&app, &[(".gitignore", "ignored.txt\n")]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("a.txt"), "one\n    // TODO: two\n").unwrap();
        std::fs::write(root.join("src/b.txt"), "FIXME later\ntodo!()\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "TODO\n").unwrap();
        app.toggle_tool_window(ToolWindowId::Todo);
        app.sync_todo();
        scan_done(&mut app).await;
        assert_eq!(app.todo.files(), [root.join("a.txt"), root.join("src/b.txt")]);
        assert_eq!(app.todo.count(), 2);
        app.sync_todo();
        assert!(!app.todo.scanning());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let g = app.workspace.generation;
        let a = root.join("a.txt");
        let bytes = std::fs::read(&a).unwrap();
        app.handle_msg(AppMsg::FileLoaded { generation: g, path: a.clone(), result: Ok(bytes), focus: true });
        assert_eq!(app.editor.active_path(), Some(a.as_path()));
        assert_eq!(app.editor.status_position().as_deref(), Some("Ln 2, Col 8"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn f5_scans_again() {
        let (mut app, dir) = test_app("todo-f5");
        let root = app.workspace.root.clone();
        init_repo(&app, &[("a.txt", "TODO\n")]);
        app.toggle_tool_window(ToolWindowId::Todo);
        app.sync_todo();
        scan_done(&mut app).await;
        assert_eq!(app.todo.count(), 1);
        std::fs::write(root.join("b.txt"), "FIXME\n").unwrap();
        app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
        scan_done(&mut app).await;
        assert_eq!(app.todo.count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_generation_is_dropped() {
        let (mut app, dir) = test_app("todo-stale");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("a.txt"), "TODO\n").unwrap();
        let s = app.todo.search(&root, false, false);
        let mut batches = Vec::new();
        s.run(|b| batches.push(b));
        let generation = app.workspace.generation + 1;
        for batch in batches {
            app.handle_msg(AppMsg::Todo { generation, batch });
        }
        assert_eq!(app.todo.count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
