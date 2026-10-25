//! The system clipboard through external tools (`wl-paste`/`wl-copy`,
//! `xclip`, `xsel`, `pbpaste`/`pbcopy`), for the explicit paste action:
//! terminals hand jetl the clipboard only on their own paste shortcut
//! (bracketed paste), and crossterm cannot read it through OSC 52.

use std::process::Stdio;
use std::time::Duration;

use crate::config::ClipboardConfig;

/// A clipboard tool that has not answered by then is killed.
pub const TIMEOUT: Duration = Duration::from_secs(1);

/// How paste reads the system clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paste {
    Command(Vec<String>),
    /// `paste_command = "none"`: jetl's own last copy only.
    Disabled,
    /// No tool for this session's display server on PATH.
    Missing,
}

/// The tools for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub paste: Paste,
    /// Copies also go here besides OSC 52, so a terminal that ignores OSC 52
    /// cannot leave the clipboard paste reads behind jetl's last copy.
    pub copy: Option<Vec<String>>,
}

/// Resolve the tools: the config's commands win, else the first known tool
/// for the display server in `env` (Wayland, then X11, then macOS) that
/// `on_path` finds. The copy tool defaults to the detected paste tool's
/// partner only, never to a guess next to a configured paste command.
pub fn select(cfg: &ClipboardConfig, env: impl Fn(&str) -> Option<String>, macos: bool, on_path: impl Fn(&str) -> bool) -> Tools {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    let mut known: Vec<(&[&str], &[&str])> = Vec::new();
    if set("WAYLAND_DISPLAY") {
        known.push((&["wl-paste", "--no-newline"], &["wl-copy"]));
    }
    if set("DISPLAY") {
        known.push((&["xclip", "-selection", "clipboard", "-o"], &["xclip", "-selection", "clipboard", "-i"]));
        known.push((&["xsel", "--clipboard", "--output"], &["xsel", "--clipboard", "--input"]));
    }
    if macos {
        known.push((&["pbpaste"], &["pbcopy"]));
    }
    let detected = known.into_iter().find(|(p, _)| on_path(p[0]));
    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let paste = match cfg.paste_command.trim() {
        "none" => Paste::Disabled,
        "" => detected.map_or(Paste::Missing, |(p, _)| Paste::Command(argv(p))),
        cmd => Paste::Command(split(cmd)),
    };
    let copy = match cfg.copy_command.trim() {
        "none" => None,
        "" if cfg.paste_command.trim().is_empty() => detected.map(|(_, c)| argv(c)).filter(|c| on_path(&c[0])),
        "" => None,
        cmd => Some(split(cmd)),
    };
    Tools { paste, copy }
}

/// [`select`] for this process.
pub fn detect(cfg: &ClipboardConfig) -> Tools {
    select(cfg, |k| std::env::var(k).ok(), cfg!(target_os = "macos"), on_path)
}

/// Whitespace-separated words; no quoting.
fn split(cmd: &str) -> Vec<String> {
    cmd.split_whitespace().map(str::to_string).collect()
}

/// An executable file named `name` in a `PATH` directory.
fn on_path(name: &str) -> bool {
    crate::util::fs::which(name).is_some()
}

/// Run the paste tool and return its output. The child is killed when it
/// outlives [`TIMEOUT`] (dropping it kills it) or when the task is dropped.
pub async fn read(argv: &[String]) -> Result<String, String> {
    let name = argv.first().ok_or("empty paste command")?;
    let child = tokio::process::Command::new(name)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {name}: {e}"))?;
    let out = match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(out) => out.map_err(|e| format!("{name}: {e}"))?,
        Err(_) => return Err(format!("{name} did not answer within {} s", TIMEOUT.as_secs())),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.lines().next().unwrap_or("").trim();
        return Err(if err.is_empty() { format!("{name} failed ({})", out.status) } else { format!("{name}: {err}") });
    }
    String::from_utf8(out.stdout).map_err(|_| "the clipboard does not hold text".into())
}

/// Hand `text` to the copy tool on stdin. Its output goes nowhere: the
/// tools fork a process that serves the selection until something else is
/// copied, and that process would hold captured pipes open.
pub async fn write(argv: &[String], text: String) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let name = argv.first().ok_or("empty copy command")?;
    let mut child = tokio::process::Command::new(name)
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {name}: {e}"))?;
    let run = async {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await.map_err(|e| format!("{name}: {e}"))?;
        }
        child.wait().await.map_err(|e| format!("{name}: {e}"))
    };
    match tokio::time::timeout(TIMEOUT, run).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("{name} failed ({status})")),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("{name} did not finish within {} s", TIMEOUT.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn cmd(c: &Paste) -> String {
        match c {
            Paste::Command(a) => a.join(" "),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn tool_follows_the_display_server() {
        let cfg = ClipboardConfig::default();
        let all = |_: &str| true;
        let wayland = select(&cfg, env(&[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":0")]), false, all);
        assert_eq!(cmd(&wayland.paste), "wl-paste --no-newline");
        assert_eq!(wayland.copy, Some(vec!["wl-copy".to_string()]));
        // Wayland without wl-clipboard falls back to X11 tools (XWayland).
        let no_wl = |n: &str| !n.starts_with("wl-");
        let t = select(&cfg, env(&[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":0")]), false, no_wl);
        assert_eq!(cmd(&t.paste), "xclip -selection clipboard -o");
        let t = select(&cfg, env(&[("DISPLAY", ":0")]), false, |n| n == "xsel");
        assert_eq!(cmd(&t.paste), "xsel --clipboard --output");
        assert_eq!(t.copy.unwrap().join(" "), "xsel --clipboard --input");
        let t = select(&cfg, env(&[]), true, all);
        assert_eq!(cmd(&t.paste), "pbpaste");
        // An empty variable is not a display; nothing found is Missing.
        let t = select(&cfg, env(&[("WAYLAND_DISPLAY", ""), ("DISPLAY", "")]), false, all);
        assert_eq!(t, Tools { paste: Paste::Missing, copy: None });
        let t = select(&cfg, env(&[("WAYLAND_DISPLAY", "wayland-1")]), false, |_| false);
        assert_eq!(t.paste, Paste::Missing);
    }

    #[test]
    fn config_commands_win() {
        let vars = [("WAYLAND_DISPLAY", "wayland-1")];
        let cfg = ClipboardConfig { paste_command: " my-paste  -x ".into(), copy_command: String::new() };
        let t = select(&cfg, env(&vars), false, |_| true);
        assert_eq!(t.paste, Paste::Command(vec!["my-paste".into(), "-x".into()]));
        assert_eq!(t.copy, None, "no guessed copy tool next to a configured paste command");
        let cfg = ClipboardConfig { paste_command: "none".into(), copy_command: "none".into() };
        assert_eq!(select(&cfg, env(&vars), false, |_| true), Tools { paste: Paste::Disabled, copy: None });
        let cfg = ClipboardConfig { paste_command: String::new(), copy_command: "my-copy".into() };
        let t = select(&cfg, env(&vars), false, |_| true);
        assert_eq!(cmd(&t.paste), "wl-paste --no-newline");
        assert_eq!(t.copy, Some(vec!["my-copy".to_string()]));
    }

    #[tokio::test]
    async fn read_reports_output_failure_and_timeout() {
        let sh = |s: &str| vec!["sh".to_string(), "-c".to_string(), s.to_string()];
        assert_eq!(read(&sh("printf 'a\\nb'")).await.unwrap(), "a\nb");
        assert_eq!(read(&sh("echo 'Nothing is copied' >&2; exit 1")).await.unwrap_err(), "sh: Nothing is copied");
        let start = std::time::Instant::now();
        assert!(read(&sh("sleep 5")).await.unwrap_err().contains("did not answer"));
        assert!(start.elapsed() < Duration::from_secs(3), "the tool is killed at the timeout");
        assert!(read(&["jetl-no-such-tool".to_string()]).await.unwrap_err().starts_with("cannot run"));
        write(&sh("cat >/dev/null"), "x".into()).await.unwrap();
    }
}
