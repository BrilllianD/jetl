//! Effects panels push through `Ctx`, and git requests.

use super::{App, Ctx};
use crate::actions::PaletteKind;
use crate::event::{AppMsg, Effect, Level};
use crate::git::{GitOp, GitRequest};
use crate::ui::dialog::{Dialog, DialogId};

impl App {
    /// Apply effects in order. Refreshes are collected and run once after the
    /// batch, so saving N files re-reads the tree and git status once.
    pub fn apply_effects(&mut self, effects: Vec<Effect>) {
        let (mut tree, mut status, mut lists, mut log) = (false, false, false, false);
        for e in effects {
            match e {
                Effect::Notify(level, text) => self.notify(level, text),
                Effect::Perform(a) => self.perform(a),
                Effect::Focus(p) => {
                    if let Some(tw) = p.tool_window() {
                        self.layout.show(tw);
                    }
                    self.focus.set(p);
                }
                Effect::OpenFile(p) => self.open_file(p, true),
                Effect::OpenOrResolve(p) => self.open_or_resolve(p),
                Effect::OpenFileAt { path, line, col } => self.open_file_at(path, line, col),
                Effect::OpenFileAtUtf16 { path, pos } => self.open_file_at_utf16(path, pos),
                Effect::Dialog(d) => self.dialog = Some(d),
                Effect::SaveAs { id, then } => self.ask_save_as(id, then),
                Effect::ContextMenu(mut m) => {
                    self.fill_shortcuts(&mut m.items);
                    self.menu_bar = None;
                    self.menu = Some(m);
                }
                Effect::FileSaved { path: p, quiet } => {
                    self.lsp_saved(&p);
                    // A deleted file saved again with its last snapshot's
                    // text stores nothing, yet its project view row is back.
                    self.history_files_changed(&p);
                    if !quiet {
                        self.log_only(Level::Info, format!("Saved {}", p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
                    }
                    (tree, status) = (true, true);
                }
                Effect::Clipboard(text) => self.copy_to_clipboard(&text),
                Effect::Format(req) => self.start_format(req),
                Effect::Lsp(req) => self.lsp_request(req),
                Effect::History(req) => self.history_request(req),
                Effect::SwitchRoot(p) => self.switch_root(p),
                Effect::RefreshTree => tree = true,
                Effect::GitRefresh => (tree, status, lists, log) = (true, true, true, true),
                Effect::Git(req) => self.git_request(req),
                Effect::Quit => self.request_quit(),
            }
        }
        if tree {
            self.refresh_tree();
        }
        if status {
            self.git.refresh_status();
        }
        if lists {
            self.git.load_branches();
            self.git.load_stashes();
            self.git.load_worktrees();
        }
        // F5 and Refresh also list commits made outside jetl in a shown Log.
        if log && self.git_panel.log_loaded() {
            let mut cx = ctx!(self);
            self.git_panel.request_log(&mut cx);
            let fx = cx.effects;
            self.apply_effects(fx);
        }
    }

    /// Re-read the tree's expanded directories in `spawn_blocking`; the
    /// panel merges the result in `AppMsg::TreeLoaded`.
    pub fn refresh_tree(&mut self) {
        if let Some(req) = self.project.begin_refresh() {
            self.spawn_tree_refresh(req);
        }
    }

    pub(super) fn spawn_tree_refresh(&mut self, req: crate::files::tree::TreeRefresh) {
        let (generation, tx) = (self.workspace.generation, self.bus_tx.clone());
        tokio::task::spawn_blocking(move || {
            let dirs = req.run();
            let _ = tx.send(AppMsg::TreeLoaded { generation, seq: req.seq, dirs });
        });
    }

    /// Run a request a panel made through `Effect::Git`.
    pub(super) fn git_request(&mut self, req: GitRequest) {
        use crate::git::repo;
        if let Some(ip) = self.in_progress()
            && req.refused_mid_operation(ip.kind)
        {
            let text = if matches!(req, GitRequest::Commit { .. }) && ip.kind == crate::git::model::InProgressKind::Am {
                "A patch application (git am) is in progress; finish it with git am --continue or --abort in the Terminal".to_string()
            } else if matches!(req, GitRequest::Commit { .. }) {
                format!("A {} is in progress: Git > Continue Operation commits it", ip.kind.name())
            } else {
                ip.refusal()
            };
            return self.notify(Level::Warn, text);
        }
        match req {
            GitRequest::Progress(a) => self.progress_request(a),
            GitRequest::Refresh => self.apply_effects(vec![Effect::GitRefresh]),
            GitRequest::Stage(rels) => {
                let label = format!("stage {}", rels.join(" "));
                self.git.run_repo(GitOp::Stage, label, move |root| repo::stage(root, &rels).map(|_| String::new()));
            }
            GitRequest::Unstage(rels) => {
                let label = format!("unstage {}", rels.join(" "));
                self.git.run_repo(GitOp::Unstage, label, move |root| repo::unstage(root, &rels).map(|_| String::new()));
            }
            GitRequest::StageAll => {
                self.git.run_repo(GitOp::Stage, "stage all".into(), |root| repo::stage_all(root).map(|_| String::new()));
            }
            GitRequest::UnstageAll => {
                self.git.run_repo(GitOp::Unstage, "unstage all".into(), |root| repo::unstage_all(root).map(|_| String::new()));
            }
            GitRequest::LoadConflict(path) => self.git.load_conflict(path),
            GitRequest::ResolveSide { rel, side } => {
                let label = format!("resolve {rel} with {}", if side == repo::ConflictSide::Ours { "yours" } else { "theirs" });
                let op = GitOp::Resolve(rel.clone());
                self.git.run_repo(op, label, move |root| repo::resolve_side(root, &rel, side));
            }
            GitRequest::StageResolved(rel) => {
                let label = format!("stage {rel}");
                let op = GitOp::Resolve(rel.clone());
                self.git.run_repo(op, label, move |root| repo::stage(root, std::slice::from_ref(&rel)).map(|_| String::new()));
            }
            GitRequest::Discard(items) => {
                let label = format!("discard {}", items.iter().map(|(_, p)| p.as_str()).collect::<Vec<_>>().join(" "));
                self.git.run_repo(GitOp::Discard, label, move |root| repo::discard(root, &items));
            }
            GitRequest::Commit { message, push, amend } => {
                let subject = message.lines().next().unwrap_or("").to_string();
                let label = format!("{}: {subject}", if amend { "amend" } else { "commit" });
                let dir = self.git.messages_dir.clone();
                self.git.run_repo(GitOp::Commit { push, amend }, label, move |root| {
                    let sha = if amend { repo::amend(root, &message) } else { repo::commit(root, &message) }?;
                    if let Err(e) = crate::git::messages::record(&dir, root, &message) {
                        tracing::warn!("commit message history: {e:#}");
                    }
                    Ok(sha)
                });
            }
            GitRequest::LoadHeadMessage => self.git.load_head_message(),
            GitRequest::Stash(op) => {
                if let repo::StashOp::Push { message, untracked: true } = &op {
                    self.git.run_cli(GitOp::Stash, crate::git::cli::stash_push_args(message));
                    return;
                }
                let label = match &op {
                    repo::StashOp::Push { message, .. } if message.trim().is_empty() => "stash push".to_string(),
                    repo::StashOp::Push { message, .. } => format!("stash push: {}", message.trim()),
                    repo::StashOp::Pop { oid } => format!("stash pop {}", &oid[..oid.len().min(8)]),
                    repo::StashOp::Apply { oid } => format!("stash apply {}", &oid[..oid.len().min(8)]),
                    repo::StashOp::Drop { oid } => format!("stash drop {}", &oid[..oid.len().min(8)]),
                };
                match op {
                    repo::StashOp::Pop { oid } => {
                        self.git.stash_apply(oid, true, label);
                    }
                    repo::StashOp::Apply { oid } => {
                        self.git.stash_apply(oid, false, label);
                    }
                    op => {
                        self.git.run_repo(GitOp::Stash, label, move |root| repo::stash(root, &op));
                    }
                }
            }
            GitRequest::LoadStashes => self.git.load_stashes(),
            GitRequest::LoadBlame(path, fallback) => self.git.load_blame(path, fallback),
            GitRequest::CheckoutCommit(sha) => {
                self.git.run_cli(GitOp::Checkout(sha.clone()), vec!["switch".into(), "--detach".into(), sha]);
            }
            GitRequest::CherryPick(sha) => {
                self.git.run_cli(GitOp::CherryPick(sha.clone()), vec!["cherry-pick".into(), sha]);
            }
            GitRequest::Reset { sha, mode } => {
                self.git.run_cli(GitOp::Reset { sha: sha.clone(), mode }, vec!["reset".into(), mode.flag().into(), sha]);
            }
            GitRequest::Revert(sha) => {
                self.git.run_cli(GitOp::Revert(sha.clone()), vec!["revert".into(), "--no-edit".into(), sha]);
            }
            GitRequest::NewBranchAt { name, rev } => {
                self.git.run_cli(GitOp::NewBranch(name.clone()), vec!["switch".into(), "-c".into(), name, rev]);
            }
            GitRequest::BranchOp { branch, op } => {
                if op.refused_mid_operation()
                    && let Some(ip) = self.in_progress().filter(|ip| ip.kind.blocks_switching())
                {
                    return self.notify(Level::Warn, ip.refusal());
                }
                self.branch_op(branch, op);
            }
            GitRequest::BranchOps(b) => {
                let ops = crate::git::model::BranchOp::for_branch(&b, self.git_panel.snapshot().and_then(|s| s.branch.as_deref()));
                let mut ops = ops;
                let mut message = String::new();
                if let Some(ip) = self.in_progress().filter(|ip| ip.kind.blocks_switching()) {
                    ops.retain(|(op, _)| !op.refused_mid_operation());
                    message =
                        format!("A {} is in progress: checkout, new branch, merge and rebase are hidden until it ends.", ip.kind.name());
                }
                let (ops, items): (Vec<_>, Vec<_>) = ops.into_iter().unzip();
                let title = format!("Branch '{}'", b.name);
                self.dialog = Some(Dialog::choice(DialogId::BranchOps { branch: b, ops }, &title, &message, items));
            }
            GitRequest::Merge(b) => {
                self.git.run_cli(GitOp::Merge(b.clone()), vec!["merge".into(), "--no-edit".into(), b]);
            }
            GitRequest::Rebase(onto) => {
                self.git.run_cli(GitOp::Rebase(onto.clone()), vec!["rebase".into(), onto]);
            }
            GitRequest::InteractiveRebase(plan) => {
                self.git.interactive_rebase(plan);
            }
            GitRequest::LoadRebasePlan(sha) => self.git.load_rebase_plan(sha),
            GitRequest::CheckoutTracking(remote) => {
                let local = remote.split_once('/').map_or(remote.as_str(), |(_, l)| l).to_string();
                self.git.run_cli(GitOp::NewBranch(local), vec!["switch".into(), "--track".into(), remote]);
            }
            GitRequest::RenameBranch { from, to } => {
                self.git.run_cli(GitOp::RenameBranch(to.clone()), vec!["branch".into(), "-m".into(), from, to]);
            }
            GitRequest::DeleteBranch { name, force } => {
                let flag = if force { "-D" } else { "-d" };
                self.git.run_cli(GitOp::DeleteBranch { name: name.clone(), force }, vec!["branch".into(), flag.into(), name]);
            }
            GitRequest::NewTag { name, sha } => {
                self.git.run_cli(GitOp::Tag(name.clone()), vec!["tag".into(), name, sha]);
            }
            GitRequest::DeleteTag(name) => {
                self.git.run_cli(GitOp::DeleteTag(name.clone()), vec!["tag".into(), "-d".into(), name]);
            }
            GitRequest::ShowCommit(sha) => {
                self.layout.show(crate::ui::focus::ToolWindowId::Git);
                self.focus.set(crate::ui::focus::PanelId::Git);
                let mut cx = ctx!(self);
                self.git_panel.show_commit(&sha, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            GitRequest::ApplyPatch { patch, cached, reverse, op } => {
                self.git.run_cli_input(op, crate::git::cli::apply_args(cached, reverse), Some(patch.into_bytes()));
            }
            GitRequest::Push => {
                self.git.run_cli(GitOp::Push, vec!["push".into()]);
            }
            GitRequest::PushDialog => self.push_dialog(false, false, 0),
            GitRequest::PushWith { tags, force_lease } => {
                self.git.run_cli_steps(GitOp::Push, crate::git::cli::push_steps(force_lease, tags));
            }
            GitRequest::Pull => {
                let mode = match self.config.git.pull_mode.as_str() {
                    "rebase" => "--rebase",
                    "merge" => "--no-rebase",
                    _ => "--ff-only",
                };
                self.git.run_cli(GitOp::Pull, vec!["pull".into(), mode.into()]);
            }
            GitRequest::Fetch => {
                self.git.fetch();
            }
            GitRequest::BranchesPopup => self.open_palette(PaletteKind::Branches),
            GitRequest::NewBranch => {
                self.dialog = Some(Dialog::input(
                    DialogId::NewBranchName,
                    "New Branch",
                    "Create and switch to a new branch from HEAD:",
                    "",
                    "Create",
                ));
            }
            GitRequest::LoadDiff(key) => self.git.load_diff(key),
            GitRequest::LoadLog { skip, filter } => self.git.load_log(skip, self.config.git.log_page, self.config.git.log_all, filter),
            GitRequest::LoadCommit(sha) => self.git.load_commit(sha),
            GitRequest::LoadCommitsFiles(spans) => self.git.load_commits_files(spans),
            GitRequest::CommitsDiff { title, spans } => self.git.load_commits_diff(title, spans),
            GitRequest::LoadWorktrees => self.git.load_worktrees(),
            GitRequest::WorktreePrune => self.prune_worktrees(),
        }
    }

    pub(super) fn switch_branch(&mut self, name: String) {
        if let Some(ip) = self.in_progress().filter(|ip| ip.kind.blocks_switching()) {
            return self.notify(Level::Warn, ip.refusal());
        }
        if self.branch_label().as_deref() == Some(name.as_str()) {
            self.notify(Level::Info, format!("Already on {name}"));
        } else {
            self.git.run_cli(GitOp::Switch(name.clone()), vec!["switch".into(), name]);
        }
    }
}
