//! Send HTTP Request at Caret (`run.http_request_at_caret`): the request at
//! the caret of a `.http` / `.rest` buffer runs with `curl` through the Run
//! window. The typed text is sent, saved or not. The body goes to a temp
//! file kept in `App::http_body`, so Rerun finds it again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::App;
use crate::event::Level;
use crate::run::{MatcherKind, Origin, RunTask, TaskCommand};

/// Makes the body file names unique within the process.
static NEXT_BODY: AtomicU64 = AtomicU64::new(0);

impl App {
    /// `run.http_request_at_caret`: send the request at the caret of the active `.http` file.
    pub(super) fn http_request_at_caret(&mut self) {
        match self.http_request_task(crate::util::fs::which) {
            Ok(task) => self.run_task(task),
            Err(e) => self.notify(Level::Info, e),
        }
    }

    /// The task that sends the request at the caret. `which` finds curl; it
    /// is a parameter so tests need no `PATH` change. Writes the body file
    /// only when everything else is fine.
    pub(crate) fn http_request_task(&mut self, which: impl Fn(&str) -> Option<PathBuf>) -> Result<RunTask, String> {
        let root = self.workspace.root.clone();
        let b = self.editor.active_buffer();
        let path = b
            .and_then(|b| b.path.clone())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("http") || e.eq_ignore_ascii_case("rest")))
            .ok_or("Send HTTP Request needs a .http file")?;
        let b = b.ok_or("Send HTTP Request needs a .http file")?;
        let req = crate::http::request_at(&b.text(), b.cursor_line_col().0).ok_or("No HTTP request at the caret")?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| root.clone());
        let vars = crate::http::load_env(&[&dir, &root])?;
        let req = crate::http::substitute(&req, &vars)?;
        if which("curl").is_none() {
            return Err("curl not found in PATH".into());
        }
        let body = self.write_body(req.body.as_deref())?;
        Ok(RunTask {
            name: req.name.clone(),
            command: TaskCommand::Argv(crate::http::curl_argv(&req, body.as_deref())),
            cwd: None,
            env: BTreeMap::new(),
            args: Vec::new(),
            matcher: Some(MatcherKind::Plain),
            origin: Origin::Detected,
        })
    }

    /// Replaces the previous body file by a new one (none for a request without body).
    fn write_body(&mut self, body: Option<&str>) -> Result<Option<PathBuf>, String> {
        if let Some(old) = self.http_body.take() {
            let _ = std::fs::remove_file(old);
        }
        let Some(body) = body else { return Ok(None) };
        let n = NEXT_BODY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("jetl-http-{}-{n}.body", std::process::id()));
        std::fs::write(&path, body).map_err(|e| format!("Cannot write the request body: {e}"))?;
        self.http_body = Some(path.clone());
        Ok(Some(path))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;
    use crate::actions::{Action, RunAction};
    use crate::app::tests::{handle_next, load, test_app};
    use crate::event::{AppMsg, Level};
    use crate::run::panel::Status;
    use crate::run::{MatcherKind, Origin};
    use crate::ui::focus::ToolWindowId;

    fn curl_or_skip(test: &str) -> bool {
        if crate::util::fs::which("curl").is_none() {
            eprintln!("{test}: skipped, curl not found in PATH");
            return false;
        }
        let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty());
        let proxy = ["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"].iter().any(|k| set(k));
        let covered = ["no_proxy", "NO_PROXY"].iter().any(|k| std::env::var(k).is_ok_and(|v| v.contains("127.0.0.") || v.trim() == "*"));
        if proxy && !covered {
            eprintln!("{test}: skipped, an HTTP proxy is set and no_proxy does not cover 127.0.0.1");
            return false;
        }
        true
    }

    /// A one-shot HTTP server: answers `count` requests with `reply` and
    /// sends each raw request (head and body) on the channel.
    fn serve(count: usize, reply: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..count {
                let Ok((mut s, _)) = listener.accept() else { return };
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut all = Vec::new();
                let mut buf = [0u8; 1024];
                let head_end = loop {
                    if let Some(i) = all.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break all.len(),
                        Ok(n) => all.extend_from_slice(&buf[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&all[..head_end]).to_lowercase();
                let want: usize =
                    head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                while all.len() < head_end + want {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => all.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&all).into_owned());
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (port, rx)
    }

    async fn until_exit(app: &mut App) {
        while app.run.running() {
            handle_next(app, |m| matches!(m, AppMsg::RunOutput { .. } | AppMsg::RunExited { .. })).await;
        }
    }

    fn texts(app: &App) -> Vec<String> {
        app.run_panel.lines.iter().map(|l| l.text.clone()).collect()
    }

    fn infos(app: &App) -> Vec<String> {
        app.notifications.iter().filter(|n| n.level == Level::Info).map(|n| n.text.clone()).collect()
    }

    fn goto_line(app: &mut App, line: usize) {
        let b = app.editor.active_buffer_mut().unwrap();
        let at = b.rope.line_to_char(line);
        b.view.sels.primary_mut().head = at;
    }

    fn send(app: &mut App) {
        app.perform(Action::Run(RunAction::HttpRequestAtCaret));
    }

    #[tokio::test]
    async fn get_shows_the_response_under_the_request_name_and_reruns() {
        if !curl_or_skip("get_shows_the_response_under_the_request_name_and_reruns") {
            return;
        }
        let (port, rx) = serve(2, "hello");
        let (mut app, dir) = test_app("http-get");
        let text = format!("### Ping\nGET http://127.0.0.1:{port}/typed\nAccept: text/plain\n");
        let path = load(&mut app, "api.http", &text);
        std::fs::write(&path, text.replace("/typed", "/on-disk")).unwrap();
        goto_line(&mut app, 1);
        send(&mut app);
        assert!(app.run.running());
        assert!(app.layout.is_visible(ToolWindowId::Run));
        until_exit(&mut app).await;
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(got.starts_with("GET /typed HTTP/1.1"), "{got}");
        assert!(got.contains("\r\nAccept: text/plain\r\n"), "{got}");
        let last = app.run.last.clone().unwrap();
        assert_eq!(last.name, "Ping");
        assert_eq!(last.matcher, Some(MatcherKind::Plain));
        assert_eq!((last.cwd.as_deref(), last.origin), (None, Origin::Detected));
        assert!(app.run_panel.command().unwrap().starts_with("curl -sS -i --max-time 30 -X GET"));
        let lines = texts(&app);
        assert!(lines.iter().any(|l| l == "HTTP/1.1 200 OK"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "hello"), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\r')), "{lines:?}");
        assert_eq!(app.run_panel.status(), Some(&Status::Exited(0)));
        assert!(app.http_body.is_none());
        app.perform(Action::Run(RunAction::Rerun));
        until_exit(&mut app).await;
        let again = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(again.starts_with("GET /typed HTTP/1.1"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn post_sends_headers_body_and_env_variables() {
        if !curl_or_skip("post_sends_headers_body_and_env_variables") {
            return;
        }
        let (port, rx) = serve(3, "created");
        let (mut app, dir) = test_app("http-post");
        let root = app.workspace.root.clone();
        std::fs::write(root.join("http-client.env.json"), format!("{{\"dev\": {{\"host\": \"127.0.0.1:{port}\", \"id\": 7}}}}")).unwrap();
        std::fs::create_dir_all(root.join("api")).unwrap();
        load(
            &mut app,
            "api/items.http",
            "### First\nGET http://{{host}}/first\n\n###\n# @name create\nPOST http://{{ host }}/items/{{id}}\nContent-Type: application/json\nX-Token: abc\n\n{\"a\": 1}\n\n> {% client.log(1) %}\n",
        );
        goto_line(&mut app, 6);
        send(&mut app);
        until_exit(&mut app).await;
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(got.starts_with("POST /items/7 HTTP/1.1"), "{got}");
        assert!(got.contains("\r\nX-Token: abc\r\n"), "{got}");
        assert!(got.contains("\r\nContent-Type: application/json\r\n"), "{got}");
        assert!(got.ends_with("{\"a\": 1}"), "{got}");
        assert_eq!(app.run.last.as_ref().unwrap().name, "create");
        let body = app.http_body.clone().unwrap();
        assert!(body.starts_with(std::env::temp_dir()));
        let name = body.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(&format!("jetl-http-{}-", std::process::id())) && name.ends_with(".body"), "{name}");
        assert_eq!(std::fs::read_to_string(&body).unwrap(), "{\"a\": 1}");
        assert!(texts(&app).iter().any(|l| l == "created"));
        app.perform(Action::Run(RunAction::Rerun));
        until_exit(&mut app).await;
        let again = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(again.ends_with("{\"a\": 1}"), "{again}");
        goto_line(&mut app, 1);
        send(&mut app);
        until_exit(&mut app).await;
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!body.exists());
        assert!(app.http_body.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hints_and_nothing_runs() {
        let (mut app, dir) = test_app("http-hints");
        let root = app.workspace.root.clone();
        load(&mut app, "notes.txt", "GET http://example.invalid/\n");
        send(&mut app);
        assert_eq!(infos(&app).last().map(String::as_str), Some("Send HTTP Request needs a .http file"));
        load(&mut app, "a.rest", "# only a comment\n");
        goto_line(&mut app, 0);
        send(&mut app);
        assert_eq!(infos(&app).last().map(String::as_str), Some("No HTTP request at the caret"));
        load(&mut app, "b.http", "GET http://{{nope}}/\n");
        send(&mut app);
        assert_eq!(infos(&app).last().map(String::as_str), Some("Unknown variable {{nope}}"));
        std::fs::write(root.join("http-client.env.json"), "{bad").unwrap();
        send(&mut app);
        assert!(infos(&app).last().unwrap().starts_with("http-client.env.json: "), "{:?}", infos(&app));
        std::fs::remove_file(root.join("http-client.env.json")).unwrap();
        load(&mut app, "c.http", "POST http://example.invalid/\n\nbody\n");
        goto_line(&mut app, 0);
        assert_eq!(app.http_request_task(|_| None).unwrap_err(), "curl not found in PATH");
        assert!(app.http_body.is_none());
        assert!(!app.run.running());
        assert!(app.run.last.is_none());
        assert!(!app.layout.is_visible(ToolWindowId::Run));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
