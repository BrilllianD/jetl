//! Line coverage from the lcov report of `[coverage] lcov`: loading it off
//! the main thread, the toggle, and the reactions to the watcher, focus
//! gain, a root switch and a changed key. The data lives in the editor
//! (`Editor::coverage`); the gutter paints it (`editor::view::render`).

use std::path::PathBuf;

use super::App;
use crate::coverage::Coverage;
use crate::event::{AppMsg, Level};

#[derive(Debug, Default)]
pub struct CoverageState {
    /// Toggled by `coverage.toggle`; not saved, so coverage shows again after a restart.
    pub hidden: bool,
    /// Bumped by every load, by hiding, by a key change and by a root switch; an older result is dropped.
    pub seq: u64,
    pub loading: bool,
    /// The last load was asked for by the toggle: say what came of it.
    pub explicit: bool,
    /// Paths the watcher reported while a load was in flight; marked stale in its result.
    pub marked: Vec<PathBuf>,
}

impl App {
    /// Start (or restart) loading the report; the data on show stays until the result swaps it.
    /// Hidden or an empty key: drop the data instead.
    pub(crate) fn load_coverage(&mut self) {
        self.coverage.seq += 1;
        self.coverage.marked.clear();
        let name = self.config.coverage.lcov.trim().to_string();
        if self.coverage.hidden || name.is_empty() {
            self.coverage.loading = false;
            self.coverage.explicit = false;
            self.editor.set_coverage(None);
            return;
        }
        self.coverage.loading = true;
        self.coverage.explicit = false;
        let (seq, generation, tx) = (self.coverage.seq, self.workspace.generation, self.bus_tx.clone());
        let root = self.workspace.root.clone();
        let file = self.coverage_file(&name);
        tokio::task::spawn_blocking(move || {
            let result = crate::coverage::load(&file, &root);
            let _ = tx.send(AppMsg::Coverage { generation, seq, result });
        });
    }

    /// Drop what is shown, then load for the current root and key (root switch, key change).
    pub(crate) fn coverage_restart(&mut self) {
        self.editor.set_coverage(None);
        self.load_coverage();
    }

    pub(super) fn coverage_loaded(&mut self, generation: u64, seq: u64, result: Result<Option<Coverage>, String>) {
        if generation != self.workspace.generation || seq != self.coverage.seq {
            tracing::debug!("dropping stale coverage");
            return;
        }
        self.coverage.loading = false;
        let explicit = std::mem::take(&mut self.coverage.explicit);
        let name = self.config.coverage.lcov.trim().to_string();
        match result {
            Ok(Some(mut c)) => {
                for p in std::mem::take(&mut self.coverage.marked) {
                    c.mark_stale(&p);
                }
                if explicit {
                    let n = c.len();
                    let files = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
                    self.notify(Level::Info, format!("Coverage: {files} from {name}"));
                }
                self.editor.set_coverage(Some(c));
            }
            Ok(None) => {
                self.editor.set_coverage(None);
                if explicit {
                    self.notify(Level::Warn, format!("Coverage: {name} not found"));
                }
            }
            Err(e) => {
                self.editor.set_coverage(None);
                self.notify(Level::Warn, format!("Coverage {name}: {e}"));
            }
        }
    }

    /// `coverage.toggle`: hide the coverage, or show it and load the report.
    pub(super) fn toggle_coverage(&mut self) {
        if !self.coverage.hidden {
            self.coverage.hidden = true;
            self.coverage.seq += 1;
            self.coverage.loading = false;
            self.coverage.explicit = false;
            self.coverage.marked.clear();
            self.editor.set_coverage(None);
            self.notify(Level::Info, "Coverage hidden");
        } else if self.config.coverage.lcov.trim().is_empty() {
            self.notify(Level::Info, "Set [coverage] lcov to an lcov file to show coverage");
        } else {
            self.coverage.hidden = false;
            self.load_coverage();
            self.coverage.explicit = true;
        }
    }

    /// The report's path: `[coverage] lcov` joined to the root, with `.` and `..` applied, so it
    /// compares equal to the watcher's paths and a `../` path counts as outside the root.
    fn coverage_file(&self, name: &str) -> PathBuf {
        crate::util::fs::normalize(&self.workspace.root.join(name))
    }

    /// The watcher's batch: the report changed (or a directory above it was made): load it again;
    /// else a source file the report lists changed: its colours go until the next load.
    pub(super) fn coverage_fs_changed(&mut self, paths: &[PathBuf]) {
        let name = self.config.coverage.lcov.trim();
        if self.coverage.hidden || name.is_empty() {
            return;
        }
        let root = &self.workspace.root;
        let lcov = self.coverage_file(name);
        if paths.iter().any(|p| *p == lcov || (p != root && lcov.starts_with(p))) {
            self.load_coverage();
            return;
        }
        for p in paths {
            self.editor.coverage_mark_stale(p);
            if self.coverage.loading {
                self.coverage.marked.push(p.clone());
            }
        }
    }

    /// Nothing watches the report (no watcher, or it lies outside the root): look again.
    pub(super) fn coverage_focus_gained(&mut self) {
        let name = self.config.coverage.lcov.trim();
        if self.coverage.hidden || name.is_empty() {
            return;
        }
        if self.watcher.is_none() || !self.coverage_file(name).starts_with(&self.workspace.root) {
            self.load_coverage();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::app::tests::{handle_next, load, test_app};
    use crate::coverage::LineCoverage;
    use crate::files::watch::Changes;

    const LCOV: &str = "SF:a.rs\nDA:1,1\nDA:2,0\nend_of_record\n";

    fn set_mtime(p: &Path, t: SystemTime) {
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    fn toasts(app: &App) -> Vec<String> {
        app.toasts.items().iter().map(|t| t.text.clone()).collect()
    }

    /// `a.rs` (old) and `lcov.info` (newer) in the root; the load that `App::new` asked for is not waited for.
    async fn fixture(name: &str) -> (App, PathBuf, PathBuf) {
        let (mut app, dir) = test_app(name);
        let a = load(&mut app, "a.rs", "fn a() {}\nfn b() {}\n");
        let lcov = app.workspace.root.join("lcov.info");
        std::fs::write(&lcov, LCOV).unwrap();
        let now = SystemTime::now();
        set_mtime(&a, now - Duration::from_secs(100));
        set_mtime(&lcov, now - Duration::from_secs(50));
        app.notifications.clear();
        app.toasts = Default::default();
        (app, dir, a)
    }

    async fn wait(app: &mut App) {
        let seq = app.coverage.seq;
        handle_next(app, move |m| matches!(m, AppMsg::Coverage { seq: s, .. } if *s == seq)).await;
    }

    fn states(app: &App, a: &Path) -> Vec<Option<LineCoverage>> {
        let f = app.editor.coverage().and_then(|c| c.file(a));
        (0..3).map(|l| f.and_then(|f| f.state(l..=l))).collect()
    }

    #[tokio::test]
    async fn load_applies_the_newest_result_only() {
        let (mut app, dir, a) = fixture("cov-newest").await;
        app.load_coverage();
        let first = app.coverage.seq;
        app.load_coverage();
        assert!(app.coverage.seq > first);
        let generation = app.workspace.generation;
        let stale = crate::coverage::Coverage::parse("SF:/x\nend_of_record\n", Path::new("/"));
        app.handle_msg(AppMsg::Coverage { generation, seq: first, result: Ok(Some(stale.clone())) });
        assert!(app.editor.coverage().is_none(), "an older seq is dropped");
        wait(&mut app).await;
        assert_eq!(states(&app, &a), vec![Some(LineCoverage::Covered), Some(LineCoverage::Uncovered), None]);
        let seq = app.coverage.seq;
        app.handle_msg(AppMsg::Coverage { generation: generation - 1, seq, result: Ok(Some(stale)) });
        assert_eq!(app.editor.coverage().unwrap().len(), 1, "another generation is dropped");
        assert!(toasts(&app).is_empty(), "{:?}", toasts(&app));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn toggle_hides_and_shows_with_toasts() {
        let (mut app, dir, _) = fixture("cov-toggle").await;
        app.load_coverage();
        wait(&mut app).await;
        assert!(app.editor.coverage().is_some());
        app.toggle_coverage();
        assert_eq!(toasts(&app), ["Coverage hidden"]);
        assert!(app.editor.coverage().is_none());
        app.toasts = Default::default();
        app.toggle_coverage();
        wait(&mut app).await;
        assert_eq!(toasts(&app), ["Coverage: 1 file from lcov.info"]);
        assert!(app.editor.coverage().is_some());
        std::fs::remove_file(app.workspace.root.join("lcov.info")).unwrap();
        app.toggle_coverage();
        app.toasts = Default::default();
        app.toggle_coverage();
        wait(&mut app).await;
        assert_eq!(toasts(&app), ["Coverage: lcov.info not found"]);
        assert_eq!(app.notifications.last().unwrap().level, Level::Warn);
        assert!(app.editor.coverage().is_none());
        app.toggle_coverage();
        app.toasts = Default::default();
        app.config.coverage.lcov = String::new();
        app.toggle_coverage();
        assert_eq!(toasts(&app), ["Set [coverage] lcov to an lcov file to show coverage"]);
        assert!(app.coverage.hidden);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_load_warns_even_when_silent() {
        let (mut app, dir, _) = fixture("cov-err").await;
        let generation = app.workspace.generation;
        app.load_coverage();
        let seq = app.coverage.seq;
        app.handle_msg(AppMsg::Coverage { generation, seq, result: Err("boom".into()) });
        assert_eq!(toasts(&app), ["Coverage lcov.info: boom"]);
        assert_eq!(app.notifications.last().unwrap().level, Level::Warn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watcher_change_of_lcov_reloads() {
        let (mut app, dir, a) = fixture("cov-watch-lcov").await;
        app.load_coverage();
        wait(&mut app).await;
        let lcov = app.workspace.root.join("lcov.info");
        std::fs::write(&lcov, "SF:a.rs\nDA:1,1\nDA:2,4\nend_of_record\n").unwrap();
        set_mtime(&lcov, SystemTime::now() - Duration::from_secs(40));
        let before = app.coverage.seq;
        app.fs_changed(Changes { paths: vec![lcov], ..Default::default() });
        assert!(app.coverage.seq > before);
        wait(&mut app).await;
        assert_eq!(states(&app, &a)[1], Some(LineCoverage::Covered));
        let before = app.coverage.seq;
        let root = app.workspace.root.clone();
        app.fs_changed(Changes { paths: vec![root], ..Default::default() });
        assert_eq!(app.coverage.seq, before, "the root itself is no report change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_report_path_through_dot_dot_reloads_on_change() {
        let (mut app, dir, _a) = fixture("cov-watch-dotdot").await;
        app.config.coverage.lcov = "cov/../lcov.info".into();
        let before = app.coverage.seq;
        let lcov = app.workspace.root.join("lcov.info");
        app.fs_changed(Changes { paths: vec![lcov], ..Default::default() });
        assert!(app.coverage.seq > before);
        wait(&mut app).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watcher_change_of_a_source_marks_it_stale() {
        let (mut app, dir, a) = fixture("cov-watch-src").await;
        let root = app.workspace.root.clone();
        let b = root.join("b.rs");
        std::fs::write(&b, "fn c() {}\n").unwrap();
        set_mtime(&b, SystemTime::now() - Duration::from_secs(100));
        let lcov = root.join("lcov.info");
        std::fs::write(&lcov, "SF:a.rs\nDA:1,1\nend_of_record\nSF:b.rs\nDA:1,1\nend_of_record\n").unwrap();
        set_mtime(&lcov, SystemTime::now() - Duration::from_secs(50));
        app.load_coverage();
        wait(&mut app).await;
        assert!(app.editor.coverage().unwrap().file(&a).is_some());
        app.fs_changed(Changes { paths: vec![a.clone()], ..Default::default() });
        assert!(app.editor.coverage().unwrap().file(&a).is_none());
        assert!(app.editor.coverage().unwrap().file(&b).is_some(), "another file stays");
        // A change reported while a load is in flight is applied to its result.
        app.load_coverage();
        app.fs_changed(Changes { paths: vec![a.clone()], ..Default::default() });
        wait(&mut app).await;
        assert!(app.editor.coverage().unwrap().file(&a).is_none());
        assert!(app.editor.coverage().unwrap().file(&b).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn editing_keeps_the_data_and_the_buffer_is_dirty() {
        let (mut app, dir, a) = fixture("cov-dirty").await;
        app.load_coverage();
        wait(&mut app).await;
        app.perform(crate::actions::Action::Editor(crate::actions::EditorAction::InsertChar('x')));
        assert!(app.editor.coverage().unwrap().file(&a).is_some(), "the data stays; render hides it");
        assert!(app.editor.active_buffer().is_some_and(|b| b.dirty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn focus_gain_reloads_without_a_watcher() {
        let (mut app, dir, _) = fixture("cov-focus").await;
        assert!(app.watcher.is_none(), "test_app starts no watcher");
        let before = app.coverage.seq;
        app.handle_term_event(crossterm::event::Event::FocusGained);
        assert!(app.coverage.seq > before);
        wait(&mut app).await;
        app.toggle_coverage();
        let hidden = app.coverage.seq;
        app.handle_term_event(crossterm::event::Event::FocusGained);
        assert_eq!(app.coverage.seq, hidden, "hidden coverage is not reloaded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn root_switch_reloads_coverage() {
        let (mut app, dir, _) = fixture("cov-root").await;
        app.load_coverage();
        wait(&mut app).await;
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other = std::fs::canonicalize(&other).unwrap();
        let c = other.join("c.rs");
        std::fs::write(&c, "fn c() {}\n").unwrap();
        std::fs::write(other.join("lcov.info"), "SF:c.rs\nDA:1,0\nend_of_record\n").unwrap();
        let t = SystemTime::now();
        set_mtime(&c, t - Duration::from_secs(100));
        set_mtime(&other.join("lcov.info"), t - Duration::from_secs(50));
        let before = app.coverage.seq;
        app.do_switch_root(other);
        assert!(app.editor.coverage().is_none());
        assert!(app.coverage.seq > before);
        wait(&mut app).await;
        assert!(app.editor.coverage().unwrap().file(&c).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
