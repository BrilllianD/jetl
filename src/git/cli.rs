//! The `git` command line, for network, branch switching, log/show and
//! worktree operations. Never prompts: no controlling terminal, ssh in batch
//! mode, no editor, stdin closed, killed after a timeout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow};

use super::model::{CommitDetails, CommitFile, LogCommit, LogRow};

pub const TIMEOUT: Duration = Duration::from_secs(120);

/// Field separator in `--format` strings.
pub const SEP: char = '\x1f';

#[derive(Debug, Clone)]
pub struct CliOutput {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl CliOutput {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stdout followed by stderr, trimmed, for the console.
    pub fn combined(&self) -> String {
        let mut s = self.stdout_text().trim_end().to_string();
        let err = self.stderr.trim_end();
        if !err.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(err);
        }
        s
    }
}

/// Run `git <args>` in `root`. Errors only when git cannot be started or
/// times out; a non-zero exit is reported through `CliOutput::ok`.
pub async fn git(root: &Path, args: &[String]) -> Result<CliOutput> {
    git_with_input(root, args, None).await
}

/// [`git`] with `input` on stdin (a patch for `git apply`).
pub async fn git_with_input(root: &Path, args: &[String], input: Option<Vec<u8>>) -> Result<CliOutput> {
    use tokio::io::AsyncWriteExt;
    let ssh = if is_network(args) { ssh_command(root).await } else { None };
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["-c", "core.quotepath=off", "-c", "color.ui=never"])
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(ssh) = ssh {
        cmd.env("GIT_SSH_COMMAND", ssh);
    }
    // A new session has no controlling terminal, so nothing git or ssh runs
    // can open /dev/tty to prompt over the TUI or wait for an answer.
    #[cfg(unix)]
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| anyhow!("cannot run git: {e}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // Small inputs (patches); closing stdin ends the input.
        stdin.write_all(&input).await?;
    }
    // Dropping the future on timeout drops the child, which kills it.
    match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(out) => {
            let out = out?;
            Ok(CliOutput { ok: out.status.success(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
        }
        Err(_) => Err(anyhow!("git {} timed out after {} s", args.join(" "), TIMEOUT.as_secs())),
    }
}

/// Subcommands that may reach a remote over ssh.
fn is_network(args: &[String]) -> bool {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .is_some_and(|a| matches!(a.as_str(), "fetch" | "pull" | "push" | "ls-remote" | "clone" | "remote" | "submodule"))
}

/// `GIT_SSH_COMMAND` for network commands: the user's ssh command with
/// `-o BatchMode=yes`, so a passphrase or host key question fails at once
/// instead of waiting. Leaves a plain `GIT_SSH` program alone.
async fn ssh_command(root: &Path) -> Option<String> {
    let env = |k| std::env::var(k).ok().filter(|v: &String| !v.trim().is_empty());
    let (env_cmd, env_ssh) = (env("GIT_SSH_COMMAND"), env("GIT_SSH"));
    let config = if env_cmd.is_none() && env_ssh.is_none() {
        let out = tokio::process::Command::new("git")
            .args(["config", "--get", "core.sshCommand"])
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .await
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
    } else {
        None
    };
    batch_ssh_command(env_cmd.as_deref(), env_ssh.as_deref(), config.as_deref())
}

fn batch_ssh_command(env_cmd: Option<&str>, env_ssh: Option<&str>, config: Option<&str>) -> Option<String> {
    let base = match (env_cmd, env_ssh) {
        (Some(c), _) => c,
        (None, Some(_)) => return None,
        (None, None) => config.unwrap_or("ssh"),
    };
    Some(format!("{base} -o BatchMode=yes"))
}

/// Like [`git`], but a non-zero exit becomes an error carrying stderr.
pub async fn git_ok(root: &Path, args: &[String]) -> Result<CliOutput> {
    let out = git(root, args).await?;
    if out.ok {
        Ok(out)
    } else {
        let msg = out.stderr.trim();
        Err(anyhow!("{}", if msg.is_empty() { format!("git {} failed", args.join(" ")) } else { msg.to_string() }))
    }
}

pub fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// `git apply` of a patch on stdin: to the index (`cached`) or the work
/// tree, forwards or reversed.
pub fn apply_args(cached: bool, reverse: bool) -> Vec<String> {
    let mut v = args(&["apply", "--whitespace=nowarn"]);
    if cached {
        v.push("--cached".into());
    }
    if reverse {
        v.push("--reverse".into());
    }
    v.push("-".into());
    v
}

/// Arguments for one page of the log.
pub fn log_args(skip: usize, count: usize, all: bool) -> Vec<String> {
    let mut v = args(&["log", "--graph", "--date-order", "--no-color"]);
    v.push(format!("-n{count}"));
    if skip > 0 {
        v.push(format!("--skip={skip}"));
    }
    if all {
        v.push("--all".into());
    }
    v.push("--format=%x1f%H%x1f%h%x1f%an%x1f%ar%x1f%D%x1f%s".into());
    v
}

/// Parse `git log --graph` output made with [`log_args`]. Each line is a
/// graph prefix, optionally followed by `\x1f`-separated commit fields;
/// lines without fields are graph connectors.
pub fn parse_log(text: &str) -> Vec<LogRow> {
    text.lines()
        .map(|line| match line.split_once(SEP) {
            None => LogRow { graph: line.trim_end().to_string(), commit: None },
            Some((graph, rest)) => {
                let mut f = rest.splitn(6, SEP);
                let mut next = || f.next().unwrap_or("").to_string();
                let commit = LogCommit { sha: next(), short: next(), author: next(), date: next(), refs: next(), subject: next() };
                LogRow { graph: graph.to_string(), commit: Some(commit) }
            }
        })
        .collect()
}

/// Parse `git diff-tree --name-status` lines (`M\tpath`, `R100\told\tnew`).
pub fn parse_name_status(text: &str) -> Vec<CommitFile> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let status = parts.next()?.chars().next()?;
            let path = parts.next_back()?.to_string();
            let old = parts.next().filter(|_| matches!(status, 'R' | 'C')).map(str::to_string);
            Some(CommitFile { status, path, old })
        })
        .collect()
}

/// Message and changed files of one commit (merges against their first
/// parent, matching the `sha^` side of the file diff).
pub async fn commit_details(root: &Path, sha: &str) -> Result<CommitDetails> {
    let body = git_ok(root, &args(&["show", "-s", "--format=%B", sha])).await?.stdout_text();
    let files =
        git_ok(root, &args(&["diff-tree", "--no-commit-id", "--name-status", "-r", "-M", "--root", "--diff-merges=first-parent", sha]))
            .await?
            .stdout_text();
    Ok(CommitDetails { sha: sha.to_string(), body: body.trim_end().to_string(), files: parse_name_status(&files) })
}

/// `git show <rev>:<path>`, or nothing when the path is absent there.
pub async fn show_file(root: &Path, rev: &str, rel: &str) -> Result<Vec<u8>> {
    let out = git(root, &[String::from("show"), format!("{rev}:{rel}")]).await?;
    Ok(if out.ok { out.stdout } else { Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_runs_in_batch_mode_on_top_of_the_users_command() {
        assert_eq!(batch_ssh_command(None, None, None).as_deref(), Some("ssh -o BatchMode=yes"));
        assert_eq!(batch_ssh_command(None, None, Some("ssh -i ~/.ssh/work")).as_deref(), Some("ssh -i ~/.ssh/work -o BatchMode=yes"));
        assert_eq!(batch_ssh_command(Some("ssh -p 2222"), None, Some("ignored")).as_deref(), Some("ssh -p 2222 -o BatchMode=yes"));
        assert_eq!(batch_ssh_command(None, Some("/usr/bin/plink"), None), None);
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_network(&a(&["push", "-u", "origin", "main"])));
        assert!(is_network(&a(&["fetch", "--prune"])));
        assert!(!is_network(&a(&["switch", "-c", "x"])));
        assert!(!is_network(&a(&["log", "--format=%H"])));
    }

    #[test]
    fn parses_graph_rows_and_connectors() {
        let text = "* \x1faaaa1111\x1faaaa\x1fAnn\x1f2 hours ago\x1fHEAD -> main, origin/main\x1fMerge branch 'x'\n\
                    |\\  \n\
                    | * \x1fbbbb2222\x1fbbbb\x1fBob\x1f3 days ago\x1f\x1fsubject with \t tab\n\
                    |/  \n\
                    * \x1fcccc3333\x1fcccc\x1fAnn\x1f1 year ago\x1ftag: v1\x1froot\n";
        let rows = parse_log(text);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].graph, "* ");
        let c = rows[0].commit.as_ref().unwrap();
        assert_eq!((c.sha.as_str(), c.short.as_str(), c.author.as_str(), c.date.as_str()), ("aaaa1111", "aaaa", "Ann", "2 hours ago"));
        assert_eq!(c.refs, "HEAD -> main, origin/main");
        assert_eq!(c.subject, "Merge branch 'x'");
        assert_eq!(rows[1], LogRow { graph: "|\\".into(), commit: None });
        assert_eq!(rows[2].graph, "| * ");
        let c = rows[2].commit.as_ref().unwrap();
        assert_eq!(c.refs, "");
        assert_eq!(c.subject, "subject with \t tab");
        assert!(rows[3].commit.is_none());
        assert_eq!(rows[4].commit.as_ref().unwrap().refs, "tag: v1");
    }

    #[test]
    fn parses_name_status() {
        let v = parse_name_status("M\tsrc/a.rs\nA\tnew file.txt\nR100\told\tnew\n");
        let f = |status, path: &str, old: Option<&str>| CommitFile { status, path: path.into(), old: old.map(Into::into) };
        assert_eq!(v, vec![f('M', "src/a.rs", None), f('A', "new file.txt", None), f('R', "new", Some("old"))]);
    }

    #[test]
    fn log_args_page() {
        let a = log_args(500, 500, false);
        assert!(a.contains(&"--skip=500".to_string()));
        assert!(a.contains(&"-n500".to_string()));
        assert!(!a.contains(&"--all".to_string()));
    }
}
