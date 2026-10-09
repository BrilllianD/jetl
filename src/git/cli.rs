//! The `git` command line, for network, branch switching, log/show and
//! worktree operations. Never prompts: no controlling terminal, ssh in batch
//! mode, no editor, stdin closed, killed after a timeout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow};

use super::model::{CommitDetails, CommitFile, LogCommit, LogFilter, LogRev};

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

/// `git stash pop` or `apply` of the stash whose commit is `oid` (looked
/// up in `git stash list`, so a list that changed cannot hit another
/// entry). A stash that conflicts with HEAD is applied with conflict
/// stages and markers, like a merge stop (pop keeps the entry then);
/// local changes in the way make git refuse and change nothing. Returns
/// whether git succeeded and the console text: a short line on success,
/// git's output otherwise.
pub async fn stash_apply(root: &Path, oid: &str, pop: bool) -> Result<(bool, String)> {
    let list = git_ok(root, &args(&["stash", "list", "--format=%H"])).await?;
    let text = list.stdout_text();
    let i = text.lines().position(|l| l.trim() == oid).ok_or_else(|| anyhow!("That stash no longer exists; the list changed"))?;
    let verb = if pop { "pop" } else { "apply" };
    let out = git(root, &["stash".into(), verb.into(), format!("stash@{{{i}}}")]).await?;
    if out.ok {
        let done = if pop { "Popped" } else { "Applied" };
        Ok((true, format!("{done} stash@{{{i}}}")))
    } else {
        Ok((false, out.combined()))
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

/// Arguments for one page of the log. The graph is drawn from the parents
/// (`git::graph`); `--parents` rewrites them past commits a path filter
/// leaves out, and `--date-order` lists no parent before its children.
pub fn log_args(skip: usize, count: usize, all: bool, filter: &LogFilter) -> Vec<String> {
    let mut v = args(&["log", "--parents", "--date-order", "--no-color"]);
    v.push(format!("-n{count}"));
    if skip > 0 {
        v.push(format!("--skip={skip}"));
    }
    if !filter.text.is_empty() || !filter.author.is_empty() {
        // Plain substrings, any case.
        v.extend(args(&["--regexp-ignore-case", "--fixed-strings"]));
    }
    if !filter.text.is_empty() {
        v.push(format!("--grep={}", filter.text));
    }
    if !filter.author.is_empty() {
        v.push(format!("--author={}", filter.author));
    }
    v.push("--format=%H%x1f%P%x1f%h%x1f%an%x1f%ar%x1f%D%x1f%s".into());
    match &filter.rev {
        LogRev::Rev(rev) => v.push(rev.clone()),
        LogRev::All => v.push("--all".into()),
        LogRev::Default if all => v.push("--all".into()),
        LogRev::Default | LogRev::Head => {}
    }
    v.push("--".into());
    if !filter.path.is_empty() {
        v.push(filter.path.clone());
    }
    v
}

/// Parse `git log` output made with [`log_args`]: one commit per line,
/// fields separated by `\x1f`.
pub fn parse_log(text: &str) -> Vec<LogCommit> {
    text.lines()
        .filter(|l| l.contains(SEP))
        .map(|line| {
            let mut f = line.splitn(7, SEP);
            let mut next = || f.next().unwrap_or("").to_string();
            let sha = next();
            let parents = next().split_whitespace().map(str::to_string).collect();
            LogCommit { sha, parents, short: next(), author: next(), date: next(), refs: next(), subject: next() }
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
    fn parses_commits_with_parents() {
        let text = "aaaa1111\x1fcccc3333 bbbb2222\x1faaaa\x1fAnn\x1f2 hours ago\x1fHEAD -> main, origin/main\x1fMerge branch 'x'\n\
                    bbbb2222\x1fcccc3333\x1fbbbb\x1fBob\x1f3 days ago\x1f\x1fsubject with \t tab\n\
                    cccc3333\x1f\x1fcccc\x1fAnn\x1f1 year ago\x1ftag: v1\x1froot\n";
        let c = parse_log(text);
        assert_eq!(c.len(), 3);
        assert_eq!(
            (c[0].sha.as_str(), c[0].short.as_str(), c[0].author.as_str(), c[0].date.as_str()),
            ("aaaa1111", "aaaa", "Ann", "2 hours ago")
        );
        assert_eq!(c[0].parents, ["cccc3333", "bbbb2222"]);
        assert_eq!(c[0].refs, "HEAD -> main, origin/main");
        assert_eq!(c[0].subject, "Merge branch 'x'");
        assert_eq!(c[1].refs, "");
        assert_eq!(c[1].subject, "subject with \t tab");
        assert!(c[2].parents.is_empty());
        assert_eq!(c[2].refs, "tag: v1");
    }

    #[test]
    fn parses_name_status() {
        let v = parse_name_status("M\tsrc/a.rs\nA\tnew file.txt\nR100\told\tnew\n");
        let f = |status, path: &str, old: Option<&str>| CommitFile { status, path: path.into(), old: old.map(Into::into) };
        assert_eq!(v, vec![f('M', "src/a.rs", None), f('A', "new file.txt", None), f('R', "new", Some("old"))]);
    }

    #[test]
    fn log_args_page() {
        let a = log_args(500, 500, false, &LogFilter::default());
        assert!(a.contains(&"--skip=500".to_string()));
        assert!(a.contains(&"-n500".to_string()));
        assert!(!a.contains(&"--all".to_string()));
        assert_eq!(a.last().map(String::as_str), Some("--"));
        let f = LogFilter { rev: LogRev::Rev("dev".into()), text: "fix".into(), author: "ann".into(), path: "src/a.rs".into() };
        let a = log_args(0, 10, true, &f);
        assert!(!a.contains(&"--all".to_string()), "a branch replaces --all");
        for want in ["--grep=fix", "--author=ann", "--fixed-strings", "dev"] {
            assert!(a.contains(&want.to_string()), "{want} in {a:?}");
        }
        assert_eq!(&a[a.len() - 2..], ["--", "src/a.rs"]);
    }
}
