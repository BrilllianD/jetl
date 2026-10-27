//! Interactive rebase: the plan model, the checks before a run, and the
//! `git rebase -i` command that applies jetl's todo.
//!
//! The todo reaches git through `-c sequence.editor=cp <todo>`: the sequence
//! editor copies jetl's file over git's. Three things were found with real git
//! and shape the command:
//! - a `GIT_SEQUENCE_EDITOR` in the environment beats `-c sequence.editor`, so
//!   `git::cli` removes it from every child;
//! - a user's `commit.cleanup = verbatim` breaks `squash` under
//!   `GIT_EDITOR=true`, so the rebase runs with `-c commit.cleanup=strip`;
//! - that setting would reach the `exec`'d `git commit --amend` of a reword and
//!   strip its `#42 ...` lines, so the amend passes `--cleanup=whitespace`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::cli;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoAction {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
}

impl TodoAction {
    pub fn word(self) -> &'static str {
        match self {
            TodoAction::Pick => "pick",
            TodoAction::Reword => "reword",
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoEntry {
    pub sha: String,
    pub subject: String,
    /// The full original message.
    pub message: String,
    pub action: TodoAction,
    /// The message of a reword.
    pub new_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebasePlan {
    /// The selected commit's parent; `None` rebases from the root.
    pub base: Option<String>,
    /// HEAD when the plan was loaded.
    pub head: String,
    /// Oldest first, git's todo order.
    pub entries: Vec<TodoEntry>,
}

const REFUSE_IN_PROGRESS: &str = "An operation is in progress: finish or abort it first";
const REFUSE_DIRTY: &str = "Commit or stash your changes before an interactive rebase";
const REFUSE_MERGES: &str = "The range holds merge commits: interactive rebase keeps no merges";
const REFUSE_FIRST_SQUASH: &str = "The first commit cannot be squashed or fixed up";
const REFUSE_ALL_DROPPED: &str = "Every commit is dropped: use Reset Current Branch to Here instead";
const REFUSE_NO_MESSAGE: &str = "A reworded commit needs a message";
const HEAD_MOVED: &str = "HEAD moved since the rebase was planned; start it again";

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// Read the commits from `sha` to HEAD as a plan of picks, or say why not.
pub async fn load_plan(root: &Path, sha: &str) -> Result<RebasePlan, String> {
    let err = |e: anyhow::Error| format!("{e:#}");
    let dir = root.to_path_buf();
    let busy = tokio::task::spawn_blocking(move || {
        git2::Repository::open(&dir).map(|r| r.state() != git2::RepositoryState::Clean).map_err(|e| format!("{e}"))
    })
    .await
    .map_err(|e| format!("{e}"))??;
    if busy {
        return Err(REFUSE_IN_PROGRESS.into());
    }
    let unknown = cli::git(root, &[String::from("rev-parse"), "--verify".into(), "--quiet".into(), format!("{sha}^{{commit}}")])
        .await
        .map_err(err)?;
    if !unknown.ok {
        return Err(format!("Unknown commit {}", short(sha)));
    }
    let full = unknown.stdout_text().trim().to_string();
    let head = cli::git_ok(root, &cli::args(&["rev-parse", "--verify", "HEAD"])).await.map_err(err)?.stdout_text().trim().to_string();
    let status = cli::git(root, &cli::args(&["status", "--porcelain", "--untracked-files=no"])).await.map_err(err)?;
    if !status.stdout_text().trim().is_empty() {
        return Err(REFUSE_DIRTY.into());
    }
    let ancestor = cli::git(root, &cli::args(&["merge-base", "--is-ancestor", &full, &head])).await.map_err(err)?;
    if !ancestor.ok {
        return Err(format!("{} is not on the current branch", short(&full)));
    }
    let parent =
        cli::git(root, &[String::from("rev-parse"), "--verify".into(), "--quiet".into(), format!("{full}^")]).await.map_err(err)?;
    let base = parent.ok.then(|| parent.stdout_text().trim().to_string());
    let mut range = vec![head.clone()];
    if let Some(b) = &base {
        range.push(format!("^{b}"));
    }
    let mut merges = cli::args(&["rev-list", "--min-parents=2"]);
    merges.extend(range.iter().cloned());
    if !cli::git(root, &merges).await.map_err(err)?.stdout_text().trim().is_empty() {
        return Err(REFUSE_MERGES.into());
    }
    let mut log = cli::args(&["log", "--no-show-signature", "--reverse", "-z", "--format=%H%x1f%s%x1f%B"]);
    log.extend(range);
    let out = cli::git_ok(root, &log).await.map_err(err)?.stdout_text();
    let entries = out
        .split('\0')
        .map(|r| r.trim_start_matches('\n'))
        .filter(|r| !r.trim().is_empty())
        .filter_map(|r| {
            let mut f = r.splitn(3, '\x1f');
            let (sha, subject, message) = (f.next()?, f.next()?, f.next()?);
            Some(TodoEntry {
                sha: sha.to_string(),
                subject: subject.to_string(),
                message: message.trim_end().to_string(),
                action: TodoAction::Pick,
                new_message: None,
            })
        })
        .collect();
    Ok(RebasePlan { base, head, entries })
}

pub fn check_plan(entries: &[TodoEntry]) -> Result<(), String> {
    let first = entries.iter().find(|e| e.action != TodoAction::Drop).ok_or(REFUSE_ALL_DROPPED)?;
    if matches!(first.action, TodoAction::Squash | TodoAction::Fixup) {
        return Err(REFUSE_FIRST_SQUASH.into());
    }
    let empty = |e: &TodoEntry| e.new_message.as_deref().is_none_or(|m| m.trim().is_empty());
    if entries.iter().any(|e| e.action == TodoAction::Reword && empty(e)) {
        return Err(REFUSE_NO_MESSAGE.into());
    }
    Ok(())
}

/// The todo file's text; `msg_file(i)` is the message file of entry `i`.
pub fn todo_text(entries: &[TodoEntry], msg_file: impl Fn(usize) -> PathBuf) -> String {
    let mut text = String::new();
    for (i, e) in entries.iter().enumerate() {
        if e.action == TodoAction::Reword {
            text.push_str(&format!("pick {} {}\n", e.sha, e.subject));
            let file = msg_file(i);
            text.push_str(&format!(
                "exec git commit --amend --only --no-verify --cleanup=whitespace -F {}\n",
                crate::run::quote_word(&file.to_string_lossy())
            ));
        } else {
            text.push_str(&format!("{} {} {}\n", e.action.word(), e.sha, e.subject));
        }
    }
    text
}

/// Git arguments (after `cli`'s own) of the run.
pub fn rebase_args(plan: &RebasePlan, todo: &Path) -> Vec<String> {
    let editor = format!("sequence.editor=cp {}", crate::run::quote_word(&todo.to_string_lossy()));
    let mut v = cli::args(&["-c", "commit.cleanup=strip", "-c", &editor, "rebase", "-i"]);
    v.push(plan.base.clone().unwrap_or_else(|| "--root".into()));
    v
}

/// Check the plan and HEAD, write the todo and message files, and return the git arguments.
pub async fn prepare(root: &Path, plan: &RebasePlan) -> Result<Vec<String>, String> {
    check_plan(&plan.entries)?;
    let head = cli::git_ok(root, &cli::args(&["rev-parse", "HEAD"])).await.map_err(|e| format!("{e:#}"))?;
    if head.stdout_text().trim() != plan.head {
        return Err(HEAD_MOVED.into());
    }
    let dir = work_dir()?;
    let n = RUN.fetch_add(1, Ordering::SeqCst);
    let write = |path: &Path, text: &str| std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()));
    for (i, e) in plan.entries.iter().enumerate() {
        if let (TodoAction::Reword, Some(m)) = (e.action, &e.new_message) {
            write(&dir.join(format!("{n}-msg-{i}")), m)?;
        }
    }
    let todo = dir.join(format!("{n}-todo"));
    write(&todo, &todo_text(&plan.entries, |i| dir.join(format!("{n}-msg-{i}"))))?;
    Ok(rebase_args(plan, &todo))
}

/// The private directory of the todo and message files. Its name is
/// predictable in a shared temp dir and its todo is executed (`exec` lines), so
/// a directory planted by another user is never used.
fn work_dir() -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let path = std::env::temp_dir().join(format!("jetl-rebase-{}", std::process::id()));
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let bad = || format!("Cannot use {}: it is not a private directory of this user", path.display());
            let meta = std::fs::symlink_metadata(&path).map_err(|_| bad())?;
            // SAFETY: geteuid has no preconditions.
            let uid = unsafe { libc::geteuid() };
            if meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0 { Ok(path) } else { Err(bad()) }
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

static RUN: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::repo::tests::TempRepo;

    fn entry(sha: &str, subject: &str, action: TodoAction) -> TodoEntry {
        TodoEntry { sha: sha.into(), subject: subject.into(), message: subject.into(), action, new_message: None }
    }

    fn with(actions: &[TodoAction]) -> Vec<TodoEntry> {
        actions.iter().map(|a| entry("a", "s", *a)).collect()
    }

    #[test]
    fn word_names_every_action() {
        let words: Vec<_> = [TodoAction::Pick, TodoAction::Reword, TodoAction::Squash, TodoAction::Fixup, TodoAction::Drop]
            .iter()
            .map(|a| a.word())
            .collect();
        assert_eq!(words, ["pick", "reword", "squash", "fixup", "drop"]);
    }

    #[test]
    fn check_plan_refusals() {
        use TodoAction::*;
        assert_eq!(check_plan(&with(&[Pick, Squash, Fixup])), Ok(()));
        assert_eq!(check_plan(&with(&[Drop, Pick, Squash])), Ok(()));
        assert_eq!(check_plan(&with(&[Squash, Pick])), Err(REFUSE_FIRST_SQUASH.to_string()));
        assert_eq!(check_plan(&with(&[Drop, Fixup, Pick])), Err(REFUSE_FIRST_SQUASH.to_string()));
        assert_eq!(check_plan(&with(&[Drop, Drop])), Err(REFUSE_ALL_DROPPED.to_string()));
        assert_eq!(check_plan(&[]), Err(REFUSE_ALL_DROPPED.to_string()));
        assert_eq!(REFUSE_ALL_DROPPED, "Every commit is dropped: use Reset Current Branch to Here instead");
        let mut reword = with(&[Reword]);
        assert_eq!(check_plan(&reword), Err(REFUSE_NO_MESSAGE.to_string()));
        reword[0].new_message = Some(" \n ".into());
        assert_eq!(check_plan(&reword), Err(REFUSE_NO_MESSAGE.to_string()));
        reword[0].new_message = Some("x".into());
        assert_eq!(check_plan(&reword), Ok(()));
    }

    #[test]
    fn todo_text_for_every_action() {
        use TodoAction::*;
        let mut entries = vec![
            entry("a1", "one", Pick),
            entry("b2", "two", Reword),
            entry("c3", "three", Squash),
            entry("d4", "four", Fixup),
            entry("e5", "five", Drop),
        ];
        entries[1].new_message = Some("m".into());
        let text = todo_text(&entries, |i| PathBuf::from(format!("/t m/{i}")));
        assert_eq!(
            text,
            "pick a1 one\npick b2 two\nexec git commit --amend --only --no-verify --cleanup=whitespace -F '/t m/1'\nsquash c3 three\nfixup d4 four\ndrop e5 five\n"
        );
    }

    #[test]
    fn rebase_args_with_base_and_root() {
        let plan = |base: Option<&str>| RebasePlan { base: base.map(Into::into), head: "h".into(), entries: Vec::new() };
        assert_eq!(
            rebase_args(&plan(Some("abc")), Path::new("/x/1-todo")),
            ["-c", "commit.cleanup=strip", "-c", "sequence.editor=cp /x/1-todo", "rebase", "-i", "abc"]
        );
        assert_eq!(
            rebase_args(&plan(None), Path::new("/x y/1-todo")),
            ["-c", "commit.cleanup=strip", "-c", "sequence.editor=cp '/x y/1-todo'", "rebase", "-i", "--root"]
        );
    }

    fn git(dir: &Path, a: &[&str]) -> String {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(a).output().unwrap();
        assert!(out.status.success(), "git {a:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `init` (base.txt) then `one`..`four`, each adding `<n>.txt`; shas oldest first.
    fn four(name: &str) -> (TempRepo, Vec<String>) {
        let r = TempRepo::new(name);
        let mut shas = Vec::new();
        r.write("base.txt", "b\n");
        git(&r.dir, &["add", "."]);
        git(&r.dir, &["commit", "-q", "-m", "init"]);
        shas.push(git(&r.dir, &["rev-parse", "HEAD"]));
        for n in ["one", "two", "three", "four"] {
            r.write(&format!("{n}.txt"), &format!("body {n}\n"));
            git(&r.dir, &["add", "."]);
            git(&r.dir, &["commit", "-q", "-m", n, "-m", &format!("body {n}")]);
            shas.push(git(&r.dir, &["rev-parse", "HEAD"]));
        }
        (r, shas)
    }

    #[tokio::test]
    async fn load_plan_lists_the_range_oldest_first() {
        let (r, shas) = four("rb-load");
        r.write("untracked.txt", "u");
        let plan = load_plan(&r.dir, &shas[1]).await.unwrap();
        assert_eq!(plan.base.as_deref(), Some(shas[0].as_str()));
        assert_eq!(plan.head, shas[4]);
        let names = ["one", "two", "three", "four"];
        assert_eq!(plan.entries.len(), 4);
        for (i, e) in plan.entries.iter().enumerate() {
            assert_eq!(e.sha, shas[i + 1]);
            assert_eq!(e.subject, names[i]);
            assert_eq!(e.message, format!("{}\n\nbody {}", names[i], names[i]));
            assert_eq!((e.action, e.new_message.as_ref()), (TodoAction::Pick, None));
        }
    }

    #[tokio::test]
    async fn load_plan_from_the_root_commit() {
        let (r, shas) = four("rb-root");
        let plan = load_plan(&r.dir, &shas[0]).await.unwrap();
        assert_eq!(plan.base, None);
        assert_eq!(plan.entries.len(), 5);
    }

    #[tokio::test]
    async fn load_plan_refusals() {
        let (r, shas) = four("rb-refuse");
        let head = shas[4].clone();
        r.write(".git/MERGE_HEAD", &format!("{head}\n"));
        assert_eq!(load_plan(&r.dir, &shas[1]).await, Err(REFUSE_IN_PROGRESS.to_string()));
        std::fs::remove_file(r.dir.join(".git/MERGE_HEAD")).unwrap();
        assert_eq!(load_plan(&r.dir, "deadbeef").await, Err("Unknown commit deadbeef".to_string()));
        r.write("one.txt", "changed\n");
        assert_eq!(load_plan(&r.dir, &shas[1]).await, Err(REFUSE_DIRTY.to_string()));
        git(&r.dir, &["checkout", "--", "one.txt"]);
        git(&r.dir, &["switch", "-q", "-c", "side", &shas[2]]);
        r.write("side.txt", "s\n");
        git(&r.dir, &["add", "."]);
        git(&r.dir, &["commit", "-q", "-m", "side"]);
        let side = git(&r.dir, &["rev-parse", "HEAD"]);
        git(&r.dir, &["switch", "-q", "-"]);
        assert_eq!(load_plan(&r.dir, &side).await, Err(format!("{} is not on the current branch", short(&side))));
        git(&r.dir, &["merge", "-q", "--no-ff", "--no-edit", "side"]);
        assert_eq!(load_plan(&r.dir, &shas[1]).await, Err(REFUSE_MERGES.to_string()));
    }

    fn msg_path(args: &[String]) -> PathBuf {
        let arg = args.iter().find(|a| a.starts_with("sequence.editor=cp ")).unwrap();
        PathBuf::from(arg.strip_prefix("sequence.editor=cp ").unwrap())
    }

    #[tokio::test]
    async fn prepare_refuses_when_head_moved() {
        let (r, shas) = four("rb-moved");
        let plan = load_plan(&r.dir, &shas[1]).await.unwrap();
        r.write("five.txt", "5\n");
        git(&r.dir, &["add", "."]);
        git(&r.dir, &["commit", "-q", "-m", "five"]);
        assert_eq!(prepare(&r.dir, &plan).await, Err(HEAD_MOVED.to_string()));
        assert!(!r.dir.join(".git/rebase-merge").exists());
    }

    #[tokio::test]
    async fn prepare_writes_the_todo_and_messages() {
        use std::os::unix::fs::PermissionsExt;
        let (r, shas) = four("rb-prepare");
        let mut plan = load_plan(&r.dir, &shas[1]).await.unwrap();
        plan.entries[1].action = TodoAction::Reword;
        plan.entries[1].new_message = Some("New\n\nbody".into());
        let args = prepare(&r.dir, &plan).await.unwrap();
        assert_eq!(args[args.len() - 3..], ["rebase", "-i", shas[0].as_str()]);
        let todo = msg_path(&args);
        let text = std::fs::read_to_string(&todo).unwrap();
        let exec = text.lines().find(|l| l.starts_with("exec ")).unwrap();
        let msg = exec.rsplit(" -F ").next().unwrap();
        assert_eq!(std::fs::read_to_string(msg).unwrap(), "New\n\nbody");
        let mode = std::fs::metadata(todo.parent().unwrap()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn user_sequence_editor_and_cleanup_do_not_change_the_outcome() {
        use TodoAction::*;
        let (r, shas) = four("rb-hostile");
        git(&r.dir, &["config", "sequence.editor", "sed -i s/^pick/drop/"]);
        git(&r.dir, &["config", "commit.cleanup", "verbatim"]);
        let mut plan = load_plan(&r.dir, &shas[1]).await.unwrap();
        let mut e = plan.entries.clone();
        // one Pick, three Squash, two Reword, four Drop
        plan.entries = vec![e.remove(0), e.remove(1), e.remove(0), e.remove(0)];
        plan.entries[1].action = Squash;
        plan.entries[2].action = Reword;
        plan.entries[2].new_message = Some("Two again\n\n#7 stays\n".into());
        plan.entries[3].action = Drop;
        let args = prepare(&r.dir, &plan).await.unwrap();
        let mut cmd = tokio::process::Command::new("git");
        cmd.env("GIT_SEQUENCE_EDITOR", "sed -i s/^pick/drop/");
        cli::prepare_command(&mut cmd, &r.dir, &args, false);
        let out = cli::run_command(cmd, &args, None).await.unwrap();
        assert!(out.ok, "{}", out.combined());
        let log = git(&r.dir, &["log", "--format=%s%n%b"]);
        assert_eq!(log, "Two again\n#7 stays\n\none\nbody one\n\nthree\n\nbody three\n\ninit");
    }
}
