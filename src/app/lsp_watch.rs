//! File change events for language servers (`workspace/didChangeWatchedFiles`).
//!
//! While the root watcher runs (`[files] watch`), servers are offered the
//! `workspace.didChangeWatchedFiles` capability; a server that takes it
//! (rust-analyzer) then stops running a watcher of its own and registers
//! watchers instead. The root watcher's events (`Changes::events`) are
//! filtered through each server's registrations here. Registered folders
//! outside the root get one extra watcher (`files::watch::start_outside`),
//! recursive, except an ancestor of the root, which is watched non-recursively.
//! A registered folder that does not exist is skipped; one created later is
//! watched from the next registration change or restart. When that watcher
//! cannot start, the servers restart once without the capability
//! (`LspService::outside_failed`, until the root changes) and watch by
//! themselves.
//! Without the root watcher the capability is not offered, so servers keep
//! watching by themselves; toggling that restarts them to renegotiate.

use std::path::PathBuf;

use super::App;
use crate::event::{AppMsg, Level};
use crate::files::watch::{self, FileChange, Watcher};
use crate::lsp::caps::DID_CHANGE_WATCHED_FILES;
use crate::lsp::client::State;
use crate::lsp::sync;
use serde_json::json;

/// At most this many folders outside the root are watched.
const MAX_OUTSIDE: usize = 64;

impl App {
    /// Send `events` to the servers whose registrations want them.
    pub(super) fn lsp_watched(&mut self, events: Vec<(PathBuf, FileChange)>) {
        if events.is_empty() {
            return;
        }
        let mut names: Vec<&String> = self.lsp.clients.keys().collect();
        names.sort();
        for name in names {
            let c = &self.lsp.clients[name];
            if !c.watch_files || c.capabilities().is_none() {
                continue;
            }
            let changes: Vec<_> = events
                .iter()
                .filter(|(path, change)| c.registry.watches(path, *change))
                .filter_map(|(path, change)| {
                    let kind = match change {
                        FileChange::Created => 1,
                        FileChange::Changed => 2,
                        FileChange::Deleted => 3,
                    };
                    Some(json!({"uri": sync::uri(path)?, "type": kind}))
                })
                .collect();
            if !changes.is_empty() {
                c.notify(DID_CHANGE_WATCHED_FILES, json!({"changes": changes}));
            }
        }
    }

    /// Watch the folders outside the root that the servers' registrations
    /// name; cheap when nothing changed.
    pub(super) fn lsp_retarget_watch(&mut self) {
        let root = &self.workspace.root;
        let mut wanted: Vec<(PathBuf, bool)> = self
            .lsp
            .clients
            .values()
            .filter(|c| c.watch_files && !matches!(c.state, State::Failed(_)))
            .flat_map(|c| c.registry.watch_bases())
            .filter(|base| !base.starts_with(root))
            .filter(|base| {
                let exists = base.is_dir();
                if !exists {
                    tracing::debug!("not watching {} for language servers: it does not exist", base.display());
                }
                exists
            })
            .map(|base| {
                let recursive = !root.starts_with(&base);
                (base, recursive)
            })
            .collect();
        wanted.sort();
        wanted.dedup();
        let recursive: Vec<PathBuf> = wanted.iter().filter(|(_, r)| *r).map(|(b, _)| b.clone()).collect();
        wanted.retain(|(b, r)| !*r || !recursive.iter().any(|o| o != b && b.starts_with(o)));
        if wanted.len() > MAX_OUTSIDE {
            for (b, _) in &wanted[MAX_OUTSIDE..] {
                tracing::warn!("not watching {} for language servers: more than {MAX_OUTSIDE} folders outside the project", b.display());
            }
            wanted.truncate(MAX_OUTSIDE);
        }
        if wanted == self.lsp.outside {
            return;
        }
        self.lsp.outside = wanted.clone();
        self.lsp.outside_watcher = None;
        if wanted.is_empty() {
            return;
        }
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let result = watch::start_outside(&wanted, generation, tx.clone());
            let _ = tx.send(AppMsg::LspWatcherStarted { generation, dirs: wanted, result });
        });
    }

    pub(super) fn lsp_watcher_started(&mut self, dirs: Vec<(PathBuf, bool)>, result: anyhow::Result<Watcher>) {
        if dirs != self.lsp.outside {
            return;
        }
        match result {
            Ok(w) => self.lsp.outside_watcher = Some(w),
            Err(e) if self.lsp.outside_failed => tracing::debug!("outside watcher failed again: {e:#}"),
            Err(e) => {
                self.lsp.outside_failed = true;
                self.notify(
                    Level::Warn,
                    format!("Cannot watch files outside the project for language servers ({e:#}); restarting them to watch by themselves"),
                );
                self.lsp_restart();
            }
        }
    }

    /// The root watcher runs (or not): servers started from now on are offered
    /// the capability accordingly, and those started with the other setting
    /// restart.
    pub(super) fn lsp_set_watch_files(&mut self, on: bool) {
        if self.lsp.watch_files == on {
            return;
        }
        self.lsp.watch_files = on;
        let offer = on && !self.lsp.outside_failed;
        if self.lsp.clients.values().any(|c| !matches!(c.state, State::Failed(_)) && c.watch_files != offer) {
            self.lsp_restart();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::lsp::tests::{incremental, open_on_server, sent, with_fake_servers};
    use crate::app::tests::{handle_next, test_app};
    use crate::event::AppMsg;
    use serde_json::{Value, json};

    fn uri_of(path: &std::path::Path) -> Value {
        json!(sync::uri(path).unwrap().as_str())
    }

    fn register(id: &str, options: Value) -> Value {
        json!({"registrations": [{"id": id, "method": "workspace/didChangeWatchedFiles", "registerOptions": options}]})
    }

    async fn registered(app: &mut App, server: &mut crate::lsp::fake::FakeServer, rid: i64, params: Value, method: &str) {
        server.request(rid, method, params);
        handle_next(app, crate::app::lsp::tests::is_lsp).await;
        assert_eq!(server.response(rid).await["result"], Value::Null);
    }

    fn inject(app: &mut App, events: Vec<(PathBuf, FileChange)>) {
        let generation = app.workspace.generation;
        let changes = watch::Changes { events, ..Default::default() };
        app.handle_msg(AppMsg::FsChanged { generation, changes });
    }

    #[tokio::test]
    async fn watched_file_events_follow_registrations() {
        let (mut app, dir) = test_app("lsp-watched");
        let root = app.workspace.root.clone();
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "x\n").await;
        assert!(app.lsp.clients["rust-analyzer"].watch_files);
        let options = json!({"watchers": [
            {"globPattern": "**/*.rs"},
            {"globPattern": {"baseUri": uri_of(&root), "pattern": "Cargo.toml"}, "kind": 4},
        ]});
        registered(&mut app, &mut server, 1, register("w", options), "client/registerCapability").await;
        let (x, cargo, notes) = (root.join("x.rs"), root.join("Cargo.toml"), root.join("notes.txt"));
        inject(&mut app, vec![(x.clone(), FileChange::Created), (cargo.clone(), FileChange::Changed), (notes, FileChange::Changed)]);
        let got = server.recv("workspace/didChangeWatchedFiles").await;
        assert_eq!(got["params"]["changes"], json!([{"uri": uri_of(&x), "type": 1}]));
        inject(&mut app, vec![(cargo.clone(), FileChange::Deleted)]);
        let got = server.recv("workspace/didChangeWatchedFiles").await;
        assert_eq!(got["params"]["changes"], json!([{"uri": uri_of(&cargo), "type": 3}]));
        let unreg = json!({"unregistrations": [{"id": "w", "method": "workspace/didChangeWatchedFiles"}]});
        registered(&mut app, &mut server, 2, unreg, "client/unregisterCapability").await;
        inject(&mut app, vec![(x, FileChange::Changed)]);
        app.lsp_sync();
        assert!(sent(&app, &mut server).await.iter().all(|m| m["method"] != "workspace/didChangeWatchedFiles"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outside_bases_get_their_own_watcher() {
        let (mut app, dir) = test_app("lsp-outside");
        let root = app.workspace.root.clone();
        let parent = root.parent().unwrap().to_path_buf();
        let outside = parent.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "x\n").await;
        let options = |base: &std::path::Path| json!({"watchers": [{"globPattern": {"baseUri": uri_of(base), "pattern": "**/*.rs"}}]});
        registered(&mut app, &mut server, 1, register("o", options(&outside)), "client/registerCapability").await;
        assert_eq!(app.lsp.outside, vec![(outside.clone(), true)]);
        handle_next(&mut app, |m| matches!(m, AppMsg::LspWatcherStarted { .. })).await;
        assert!(app.lsp.outside_watcher.is_some());
        std::fs::write(outside.join("lib.rs"), "x").unwrap();
        handle_next(&mut app, |m| matches!(m, AppMsg::LspWatched { .. })).await;
        let got = server.recv("workspace/didChangeWatchedFiles").await;
        assert_eq!(got["params"]["changes"][0]["uri"], uri_of(&outside.join("lib.rs")));
        assert!(matches!(got["params"]["changes"][0]["type"].as_u64(), Some(1 | 2)));
        registered(&mut app, &mut server, 2, register("p", options(&parent)), "client/registerCapability").await;
        assert_eq!(app.lsp.outside, vec![(parent.clone(), false), (outside.clone(), true)]);
        let unreg = json!({"unregistrations": [{"id": "o", "method": "workspace/didChangeWatchedFiles"}, {"id": "p", "method": "workspace/didChangeWatchedFiles"}]});
        registered(&mut app, &mut server, 3, unreg, "client/unregisterCapability").await;
        assert!(app.lsp.outside.is_empty() && app.lsp.outside_watcher.is_none());
        // A folder that does not exist is skipped, without a warning.
        let missing = parent.join("missing-outside");
        registered(&mut app, &mut server, 4, register("q", options(&missing)), "client/registerCapability").await;
        assert!(app.lsp.outside.is_empty());
        assert!(app.notifications.iter().all(|n| !n.text.starts_with("Cannot watch files outside")));
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn turning_watching_off_restarts_servers_without_the_capability() {
        let (mut app, dir) = test_app("lsp-watch-off");
        let mut servers = with_fake_servers(&mut app, incremental());
        let _first = open_on_server(&mut app, &mut servers, "a.rs", "x\n").await;
        assert!(app.lsp.clients["rust-analyzer"].watch_files);
        app.lsp_set_watch_files(false);
        app.lsp_sync();
        let mut second = servers.try_recv().expect("a new server after the restart");
        let init = second.recv("initialize").await;
        assert!(init["params"]["capabilities"]["workspace"]["didChangeWatchedFiles"].is_null());
        assert!(!app.lsp.clients["rust-analyzer"].watch_files);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_outside_watcher_restarts_servers_without_the_capability() {
        let (mut app, dir) = test_app("lsp-outside-failed");
        let outside = app.workspace.root.parent().unwrap().join("outside-failed");
        std::fs::create_dir_all(&outside).unwrap();
        let mut servers = with_fake_servers(&mut app, incremental());
        let mut server = open_on_server(&mut app, &mut servers, "a.rs", "x\n").await;
        let options = json!({"watchers": [{"globPattern": {"baseUri": uri_of(&outside), "pattern": "**/*.rs"}}]});
        registered(&mut app, &mut server, 1, register("o", options), "client/registerCapability").await;
        handle_next(&mut app, |m| matches!(m, AppMsg::LspWatcherStarted { .. })).await;
        app.handle_msg(AppMsg::LspWatcherStarted {
            generation: app.workspace.generation,
            dirs: app.lsp.outside.clone(),
            result: Err(anyhow::anyhow!("boom")),
        });
        let warns: Vec<&str> =
            app.notifications.iter().filter(|n| n.text.starts_with("Cannot watch files outside")).map(|n| n.text.as_str()).collect();
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].starts_with("Cannot watch files outside the project"), "{warns:?}");
        assert!(app.lsp.outside_failed);
        app.lsp_sync();
        let mut second = servers.try_recv().expect("a new server after the restart");
        let init = second.recv("initialize").await;
        assert!(init["params"]["capabilities"]["workspace"]["didChangeWatchedFiles"].is_null());
        assert!(!app.lsp.clients["rust-analyzer"].watch_files);
        // A second failure while `outside_failed` is set: no second warning, no second restart.
        let id = app.lsp.clients["rust-analyzer"].id;
        let restarts = app.notifications.iter().filter(|n| n.text == "Restarting language servers").count();
        app.handle_msg(AppMsg::LspWatcherStarted {
            generation: app.workspace.generation,
            dirs: app.lsp.outside.clone(),
            result: Err(anyhow::anyhow!("boom again")),
        });
        app.lsp_sync();
        let warns = app.notifications.iter().filter(|n| n.text.starts_with("Cannot watch files outside")).count();
        assert_eq!(warns, 1);
        assert_eq!(app.notifications.iter().filter(|n| n.text == "Restarting language servers").count(), restarts);
        assert!(servers.try_recv().is_err(), "no third server");
        assert_eq!(app.lsp.clients["rust-analyzer"].id, id);
        assert!(app.lsp.outside_failed);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
