//! Dialog answers.

use super::{App, Ctx};
use crate::event::Level;
use crate::git::GitOp;
use crate::ui::dialog::{Dialog, DialogId, DialogResult};
use crate::ui::focus::PanelId;

impl App {
    pub(super) fn request_quit(&mut self) {
        if self.editor.has_dirty() {
            self.dialog = Some(Dialog::confirm(
                DialogId::QuitDirty,
                "Unsaved changes",
                "Some files have unsaved changes.",
                &["Save all and exit", "Discard and exit", "Cancel"],
            ));
        } else {
            self.should_quit = true;
        }
    }

    pub(super) fn finish_dialog(&mut self, id: DialogId, res: DialogResult) {
        self.dirty_frame = true;
        match (id, res) {
            (DialogId::QuitDirty, DialogResult::Button(0)) => {
                let mut cx = ctx!(self);
                // A failed save or a disk conflict keeps the app open.
                let saved = self.editor.save_all(&mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
                self.should_quit = saved;
            }
            (DialogId::QuitDirty, DialogResult::Button(1)) => self.should_quit = true,
            (DialogId::CloseTabDirty(id), DialogResult::Button(0)) => {
                let Some(i) = self.editor.index_of_id(id) else { return };
                let mut cx = ctx!(self);
                if self.editor.save(i, &mut cx) {
                    self.editor.force_close(i);
                }
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::CloseTabDirty(id), DialogResult::Button(1)) => {
                if let Some(i) = self.editor.index_of_id(id) {
                    self.editor.force_close(i);
                }
            }
            (DialogId::CloseAllDirty, DialogResult::Button(0)) => {
                let mut cx = ctx!(self);
                if self.editor.save_all(&mut cx) {
                    self.editor.force_close_all();
                }
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::CloseAllDirty, DialogResult::Button(1)) => self.editor.force_close_all(),
            (DialogId::ReplaceInFiles, DialogResult::Button(0)) => self.replace_in_files(),
            (DialogId::OverwriteExternal(id), DialogResult::Button(0)) => {
                let mut cx = ctx!(self);
                self.editor.overwrite(id, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::OverwriteExternal(id), DialogResult::Button(1)) => {
                let mut cx = ctx!(self);
                self.editor.reload_from_disk(id, &mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
            }
            (DialogId::GotoLine, DialogResult::Text(t)) => {
                let mut it = t.trim().split(':');
                let line = it.next().and_then(|s| s.trim().parse::<usize>().ok());
                let col = it.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                match line {
                    Some(l) => self.editor.goto_line(l, col),
                    None => self.notify(Level::Warn, "Go to line: expected [line][:column]"),
                }
            }
            (DialogId::Bookmarks(targets), DialogResult::Choice(i)) => {
                if let Some(&(id, line)) = targets.get(i) {
                    self.editor.goto_bookmark(id, line);
                    self.focus.set(PanelId::Editor);
                }
            }
            (DialogId::LineEnding(id), DialogResult::Choice(i)) => {
                let le = if i == 0 { crate::editor::buffer::LineEnding::Lf } else { crate::editor::buffer::LineEnding::CrLf };
                self.editor.set_line_ending(id, le);
            }
            (DialogId::Indent(id), DialogResult::Choice(i)) => self.editor.set_indent(id, i),
            (DialogId::Language(id), DialogResult::Choice(i)) => {
                let def = i.checked_sub(1).and_then(|i| crate::editor::syntax::LANGUAGES.get(i));
                self.editor.set_language(id, def, &self.theme);
            }
            (DialogId::NewBranchName, DialogResult::Text(t)) => {
                let name = t.trim().to_string();
                if name.is_empty() || name.starts_with('-') || name.contains(char::is_whitespace) {
                    self.notify(Level::Warn, format!("Invalid branch name {name:?}"));
                } else {
                    self.git.run_cli(GitOp::NewBranch(name.clone()), vec!["switch".into(), "-c".into(), name]);
                }
            }
            (DialogId::PushSetUpstream(branch), DialogResult::Button(0)) => {
                self.git.run_cli(GitOp::PushUpstream(branch.clone()), vec!["push".into(), "-u".into(), "origin".into(), branch]);
            }
            (DialogId::SwitchRootDirty(root), DialogResult::Button(0)) => {
                let mut cx = ctx!(self);
                let saved = self.editor.save_all(&mut cx);
                let fx = cx.effects;
                self.apply_effects(fx);
                if saved {
                    self.do_switch_root(root);
                }
            }
            (DialogId::SwitchRootDirty(root), DialogResult::Button(1)) => {
                if !self.config.worktree.switch_closes_tabs {
                    self.close_dirty_tabs();
                }
                self.do_switch_root(root);
            }
            (DialogId::Info, _) => {}
            (_, DialogResult::Cancel) => {}
            (
                id @ (DialogId::WorktreeCreateKind
                | DialogId::WorktreeNewBranchName
                | DialogId::WorktreeExistingBranch(_)
                | DialogId::WorktreePath { .. }
                | DialogId::WorktreeSwitchAfterCreate(_)
                | DialogId::WorktreeRemove { .. }),
                res,
            ) => self.finish_worktree_dialog(id, res),
            (id, res) => tracing::debug!("dialog {id:?} -> {res:?} (no handler yet)"),
        }
    }
}
