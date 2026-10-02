//! Git worktrees: the `git worktree list --porcelain` parser, path helpers
//! for the create flow, and the tool window. Listing and every mutation go
//! through the `git` CLI via `GitService`; the list arrives as
//! `AppMsg::Worktrees`.

pub mod flow;
pub mod panel;
pub use panel::WorktreePanel;

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Checked-out commit; empty for a bare main repository.
    pub head: String,
    /// Short branch name (`refs/heads/` stripped).
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    /// `Some(reason)` when locked; the reason may be empty.
    pub locked: Option<String>,
    /// `Some(reason)` when git would prune it (its directory is gone).
    pub prunable: Option<String>,
    pub is_main: bool,
}

impl Worktree {
    fn new(path: PathBuf, is_main: bool) -> Self {
        Self { path, head: String::new(), branch: None, detached: false, bare: false, locked: None, prunable: None, is_main }
    }

    /// Branch name, or a short description when there is none.
    pub fn label(&self) -> String {
        match &self.branch {
            Some(b) => b.clone(),
            None if self.bare => "(bare)".into(),
            None if self.head.is_empty() => "(no branch)".into(),
            None => format!("(detached {})", &self.head[..self.head.len().min(7)]),
        }
    }
}

/// Parse `git worktree list --porcelain`. Records are separated by blank
/// lines; the first one is the main worktree. Unknown keys are ignored.
pub fn parse_porcelain(text: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in text.lines() {
        if line.is_empty() {
            out.extend(cur.take());
            continue;
        }
        let (key, value) = match line.split_once(' ') {
            Some((k, v)) => (k, Some(v)),
            None => (line, None),
        };
        if key == "worktree" {
            out.extend(cur.take());
            let is_main = out.is_empty();
            cur = Some(Worktree::new(PathBuf::from(value.unwrap_or_default()), is_main));
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        match key {
            "HEAD" => w.head = value.unwrap_or_default().to_string(),
            "branch" => {
                let r = value.unwrap_or_default();
                w.branch = Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string());
            }
            "detached" => w.detached = true,
            "bare" => w.bare = true,
            "locked" => w.locked = Some(value.unwrap_or_default().to_string()),
            "prunable" => w.prunable = Some(value.unwrap_or_default().to_string()),
            _ => {}
        }
    }
    out.extend(cur);
    out
}

/// Index of the worktree containing `root` (the deepest match, since a
/// linked worktree may live inside the main one).
pub fn current_index(list: &[Worktree], root: &Path) -> Option<usize> {
    list.iter().enumerate().filter(|(_, w)| root.starts_with(&w.path)).max_by_key(|(_, w)| w.path.components().count()).map(|(i, _)| i)
}

/// Local branches that no worktree has checked out.
pub fn free_branches(branches: &[String], list: &[Worktree]) -> Vec<String> {
    branches.iter().filter(|b| !list.iter().any(|w| w.branch.as_deref() == Some(b.as_str()))).cloned().collect()
}

/// Expand a leading `~` to the home directory.
pub fn expand_tilde(p: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if p == "~" {
        if let Some(h) = home() {
            return h;
        }
    } else if let Some(rest) = p.strip_prefix("~/")
        && let Some(h) = home()
    {
        return h.join(rest);
    }
    PathBuf::from(p)
}

/// Default directory for a new worktree from `config.worktree.default_dir`:
/// `{parent}` is the directory holding the main worktree, `{repo}` its
/// name, `{branch}` the branch with `/` turned into `-`.
pub fn default_path(template: &str, main_root: &Path, branch: &str) -> PathBuf {
    let parent = main_root.parent().unwrap_or(main_root).to_string_lossy();
    let repo = main_root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let s = template.replace("{parent}", &parent).replace("{repo}", &repo).replace("{branch}", &branch.replace('/', "-"));
    expand_tilde(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "worktree /src/app\n\
                        HEAD 1111111111111111111111111111111111111111\n\
                        branch refs/heads/main\n\
                        \n\
                        worktree /src/app.feature-x\n\
                        HEAD 2222222222222222222222222222222222222222\n\
                        branch refs/heads/feature/x\n\
                        locked\n\
                        \n\
                        worktree /src/app.detached\n\
                        HEAD 3333333333333333333333333333333333333333\n\
                        detached\n\
                        locked moved to the backup disk\n\
                        \n\
                        worktree /gone/app.old\n\
                        HEAD 4444444444444444444444444444444444444444\n\
                        branch refs/heads/old\n\
                        prunable gitdir file points to non-existent location\n\
                        \n";

    #[test]
    fn parses_porcelain_records() {
        let v = parse_porcelain(LIST);
        assert_eq!(v.len(), 4);
        assert!(v[0].is_main && !v[1].is_main);
        assert_eq!(v[0].path, PathBuf::from("/src/app"));
        assert_eq!(v[0].branch.as_deref(), Some("main"));
        assert_eq!(v[0].head, "1".repeat(40));
        assert_eq!((v[0].locked.clone(), v[0].prunable.clone(), v[0].detached), (None, None, false));
        assert_eq!(v[1].branch.as_deref(), Some("feature/x"));
        assert_eq!(v[1].locked.as_deref(), Some(""));
        assert!(v[2].detached && v[2].branch.is_none());
        assert_eq!(v[2].locked.as_deref(), Some("moved to the backup disk"));
        assert_eq!(v[2].label(), "(detached 3333333)");
        assert_eq!(v[3].prunable.as_deref(), Some("gitdir file points to non-existent location"));
        assert_eq!(v[3].locked, None);
    }

    #[test]
    fn parses_bare_main_and_missing_trailing_blank() {
        let v = parse_porcelain("worktree /srv/app.git\nbare\n\nworktree /srv/app-main\nHEAD abc\nbranch refs/heads/main");
        assert_eq!(v.len(), 2);
        assert!(v[0].is_main && v[0].bare && v[0].head.is_empty());
        assert_eq!(v[0].label(), "(bare)");
        assert_eq!(v[1].branch.as_deref(), Some("main"));
        assert!(parse_porcelain("").is_empty());
    }

    #[test]
    fn current_is_deepest_containing_worktree() {
        let v = parse_porcelain(
            "worktree /r\nbranch refs/heads/a\n\nworktree /r/.wt/b\nbranch refs/heads/b\n\nworktree /r2\nbranch refs/heads/c\n",
        );
        assert_eq!(current_index(&v, Path::new("/r/src")), Some(0));
        assert_eq!(current_index(&v, Path::new("/r/.wt/b")), Some(1));
        assert_eq!(current_index(&v, Path::new("/r2")), Some(2));
        assert_eq!(current_index(&v, Path::new("/elsewhere")), None);
        let all = ["a".to_string(), "b".into(), "c".into(), "d".into()];
        assert_eq!(free_branches(&all, &v), vec!["d".to_string()]);
    }

    #[test]
    fn expands_default_path_template() {
        let main = Path::new("/home/u/src/app");
        assert_eq!(
            default_path("{parent}/{repo}.{branch}", main, "feature/login-form"),
            PathBuf::from("/home/u/src/app.feature-login-form")
        );
        assert_eq!(default_path("{parent}/wt/{repo}/{branch}", main, "a/b/c"), PathBuf::from("/home/u/src/wt/app/a-b-c"));
        assert_eq!(default_path("/tmp/{branch}", main, "x"), PathBuf::from("/tmp/x"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(default_path("~/wt/{repo}-{branch}", main, "x"), PathBuf::from(home).join("wt/app-x"));
        }
    }

    /// The CLI calls the app makes, against a scratch repository: add with a
    /// new branch, list, refuse a dirty remove, force remove.
    #[tokio::test]
    async fn worktree_add_list_remove_round_trip() {
        use crate::git::{cli, repo};
        let r = repo::tests::TempRepo::new("worktrees");
        r.write("a.txt", "one\n");
        repo::stage_all(&r.dir).unwrap();
        repo::commit(&r.dir, "init").unwrap();
        let path = default_path("{parent}/{repo}.{branch}", &r.dir, "feature/x");
        let _ = std::fs::remove_dir_all(&path);
        let p = path.to_string_lossy().into_owned();
        let main = r.dir.to_string_lossy().into_owned();
        cli::git_ok(&r.dir, &cli::args(&["worktree", "add", "-b", "feature/x", &p])).await.unwrap();

        let list = |root: PathBuf| async move {
            parse_porcelain(&cli::git_ok(&root, &cli::args(&["worktree", "list", "--porcelain"])).await.unwrap().stdout_text())
        };
        let v = list(path.clone()).await;
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].is_main && !v[1].is_main);
        assert_eq!(v[0].path, r.dir);
        assert_eq!((v[1].path.clone(), v[1].branch.as_deref()), (path.clone(), Some("feature/x")));
        assert_eq!(current_index(&v, &path.join("sub")), Some(1));
        assert_eq!(free_branches(&["feature/x".to_string(), "spare".into()], &v), vec!["spare".to_string()]);

        std::fs::write(path.join("new.txt"), "x").unwrap();
        let out = cli::git(&r.dir, &cli::args(&["-C", &main, "worktree", "remove", &p])).await.unwrap();
        assert!(!out.ok && out.stderr.contains(flow::DIRTY_MARKER), "{}", out.stderr);
        cli::git_ok(&r.dir, &cli::args(&["-C", &main, "worktree", "remove", "--force", &p])).await.unwrap();
        assert!(!path.exists());
        assert_eq!(list(r.dir.clone()).await.len(), 1);
    }
}
