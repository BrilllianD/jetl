//! Saving: explicit save, save all, autosave, atomic writes.

use std::path::Path;
use std::time::{Duration, Instant};

use super::Editor;
use super::buffer::{self, Buffer, BufferId};
use super::selection::Selection;
use super::undo::{Change, TxKind};
use crate::app::Ctx;
use crate::config::Autosave;
use crate::event::{Effect, Level};
use crate::ui::dialog::{Dialog, DialogId};
use crate::util::fs::write_atomic;

/// What an outside change of a file means for its tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskChange {
    /// Not open, our own save, deleted, or already asked about.
    None,
    /// The tab has no unsaved edits: read the file and swap it in.
    Reload,
    /// The tab at this index has unsaved edits: ask (once).
    Conflict(usize),
}

impl Editor {
    /// Classify a change the watcher saw at `path`.
    pub fn on_disk_change(&mut self, path: &Path) -> DiskChange {
        let Some(i) = self.index_of(path) else { return DiskChange::None };
        let b = &mut self.buffers[i];
        if !disk_conflict(b) {
            return DiskChange::None;
        }
        if !b.dirty() {
            return DiskChange::Reload;
        }
        if b.autosave_conflict {
            return DiskChange::None;
        }
        b.autosave_conflict = true;
        DiskChange::Conflict(i)
    }

    /// Swap in the disk text of `path` if its tab still has no unsaved
    /// edits and the file still differs from what the tab last saw.
    pub fn reload_clean(&mut self, path: &Path, text: String) -> bool {
        let Some(i) = self.index_of(path) else { return false };
        let b = &mut self.buffers[i];
        if b.dirty() || !b.changed_on_disk() {
            return false;
        }
        replace_with_disk_text(b, text, path);
        true
    }

    /// Replace buffer `i`'s text with text recovered after a crash: one
    /// undoable change, so the buffer is dirty against the file and undo
    /// returns to it. Nothing is written; autosave leaves it alone until an
    /// explicit save. False when the text is already the same.
    pub fn recover_text(&mut self, i: usize, text: &str) -> bool {
        let Some(b) = self.buffers.get_mut(i) else { return false };
        let old = b.text();
        if old == text {
            return false;
        }
        let caret = b.view.sel().head.min(text.chars().count());
        b.apply(vec![Change::replace(0, old, text)], Selection::point(caret), TxKind::Other);
        b.recovered = true;
        true
    }

    /// Replace every match of `re` in tab `i` as one undoable edit; returns
    /// the number replaced. The tab stays modified (not saved).
    pub fn replace_in_buffer(&mut self, i: usize, re: &regex::Regex, with: &str, expand: bool) -> usize {
        let Some(b) = self.buffers.get_mut(i) else { return 0 };
        let reps = crate::files::find::replacements(&b.text(), re, with, expand);
        let n = reps.len();
        // Last to first so earlier offsets stay valid.
        let changes: Vec<Change> = reps.into_iter().rev().map(|r| Change::replace(r.start, r.old, r.new)).collect();
        let caret = b.view.sel().head;
        b.apply_then(changes, TxKind::Other, |rope| Selection::point(caret.min(rope.len_chars())));
        n
    }

    /// Overwrite / Reload / Cancel for tab `i`, whose file changed on disk
    /// under unsaved edits.
    pub fn conflict_dialog(&self, i: usize) -> Option<Dialog> {
        let b = self.buffers.get(i)?;
        let msg = format!(
            "{} changed on disk since it was opened or saved.\nOverwrite it with your version, or reload it and drop your edits?",
            b.name
        );
        Some(Dialog::confirm(DialogId::OverwriteExternal(b.id), "File changed on disk", &msg, &["Overwrite", "Reload", "Cancel"]))
    }

    /// Write buffer `i` to disk atomically. Synchronous on purpose: saves are
    /// small and the result must be known before quitting. A file changed
    /// on disk since it was loaded or saved is not written; the user picks
    /// Overwrite / Reload / Cancel (`DialogId::OverwriteExternal`).
    pub fn save(&mut self, i: usize, cx: &mut Ctx) -> bool {
        self.save_with(i, cx, false)
    }

    /// [`Editor::save`]; with `format`, a buffer with a language is cleaned
    /// up, handed to its formatter (`Effect::Format`: its `[format]`
    /// command, else its language server, else none) and written when it
    /// answers ([`Editor::format_done`]); true then means started.
    /// Only explicit saves format: closing, quitting and switching roots
    /// save through [`Editor::save`] and need the file written at once.
    pub(super) fn save_with(&mut self, i: usize, cx: &mut Ctx, format: bool) -> bool {
        let Some(b) = self.buffers.get(i) else { return false };
        if b.dirty() && disk_conflict(b) {
            cx.effects.extend(self.conflict_dialog(i).map(Effect::Dialog));
            return false;
        }
        if format && b.path.is_some() && b.dirty() && self.format_via(i).is_some() {
            if let Err(done) = self.prepare_write(i, cx) {
                return done;
            }
            if let Some(req) = self.format_request(i, true) {
                cx.effects.push(Effect::Format(req));
                return true;
            }
        }
        self.write(i, cx, false)
    }

    /// Save buffer `id` even though the file changed on disk.
    pub fn overwrite(&mut self, id: BufferId, cx: &mut Ctx) -> bool {
        let Some(i) = self.index_of_id(id) else { return false };
        self.write(i, cx, false)
    }

    /// Replace buffer `id` with the file's text, dropping unsaved edits
    /// (undoable).
    pub fn reload_from_disk(&mut self, id: BufferId, cx: &mut Ctx) {
        let Some(i) = self.index_of_id(id) else { return };
        let b = &mut self.buffers[i];
        let Some(path) = b.path.clone() else { return };
        match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(text)) => replace_with_disk_text(b, text, &path),
            Ok(Err(_)) => cx.notify(Level::Error, format!("{} is not valid UTF-8; not reloaded", b.name)),
            Err(e) => cx.notify(Level::Error, format!("reload {}: {e}", path.display())),
        }
    }

    /// [`Editor::save`]; `quiet` drops the "Saved" toast (errors still show).
    pub(super) fn write(&mut self, i: usize, cx: &mut Ctx, quiet: bool) -> bool {
        if let Err(done) = self.prepare_write(i, cx) {
            return done;
        }
        let b = &self.buffers[i];
        let (text, state) = (b.text(), b.undo.state_id());
        self.write_text(i, text, state, cx, quiet)
    }

    /// Before writing buffer `i`: it needs a path and unsaved edits (`Err`
    /// carries the save's result otherwise), then the save cleanup
    /// (`trim_trailing_whitespace`, `final_newline`).
    fn prepare_write(&mut self, i: usize, cx: &mut Ctx) -> Result<(), bool> {
        let (trim, final_newline) = (self.trim_trailing_whitespace, self.final_newline);
        let Some(b) = self.buffers.get_mut(i) else { return Err(false) };
        // `.editorconfig` overrides the editor-wide options per file.
        let trim = b.settings.trim_trailing_whitespace.unwrap_or(trim);
        let final_newline = b.settings.insert_final_newline.unwrap_or(final_newline);
        if b.path.is_none() {
            cx.notify(Level::Error, "buffer has no file path");
            return Err(false);
        }
        if !b.dirty() {
            return Err(true);
        }
        // Trimming other lines leaves the caret line where it was, so a view
        // that was following the caret need not scroll back to it (autosave
        // must not undo wheel scrolling).
        let follow = b.view.follow_key.filter(|k| (k.0, k.1) == (b.view.sel().head, b.version));
        b.prepare_for_save(trim, final_newline);
        if let Some((_, _, w, h)) = follow {
            b.view.follow_key = Some((b.view.sel().head, b.version, w, h));
        }
        Ok(())
    }

    /// Write `text`, the buffer's undo state `state`, as buffer `i`'s file.
    pub(super) fn write_text(&mut self, i: usize, text: String, state: u64, cx: &mut Ctx, quiet: bool) -> bool {
        let Some(b) = self.buffers.get_mut(i) else { return false };
        let Some(path) = b.path.clone() else { return false };
        match write_atomic(&path, &text) {
            Ok(perm_err) => {
                if let Some(e) = perm_err {
                    cx.notify(Level::Warn, format!("{}: file mode not kept: {e}", b.name));
                }
                b.mark_saved_at(state);
                b.disk_mtime = buffer::file_mtime(&path);
                b.autosave_failed = None;
                b.autosave_conflict = false;
                b.recovered = false;
                if b.lossy {
                    cx.notify(Level::Warn, format!("{} was not valid UTF-8; saved re-encoded as UTF-8", b.name));
                    b.lossy = false;
                }
                cx.effects.push(Effect::FileSaved { path, quiet });
                true
            }
            Err(e) => {
                cx.notify(Level::Error, format!("save {}: {e}", path.display()));
                false
            }
        }
    }

    /// Save every dirty buffer. Only the first file changed on disk gets the
    /// Overwrite / Reload dialog; later ones stay unsaved with a warning.
    pub fn save_all(&mut self, cx: &mut Ctx) -> bool {
        self.save_all_with(cx, false)
    }

    /// [`Editor::save_all`], formatting like [`Editor::save_with`].
    pub(super) fn save_all_with(&mut self, cx: &mut Ctx, format: bool) -> bool {
        let mut ok = true;
        let mut asked = false;
        for i in 0..self.buffers.len() {
            let b = &self.buffers[i];
            if !b.dirty() || self.is_merge_buffer(b.id) {
                continue;
            }
            if disk_conflict(b) {
                if asked {
                    cx.notify(Level::Warn, format!("{} changed on disk; not saved", b.name));
                    ok = false;
                    continue;
                }
                asked = true;
            }
            ok &= self.save_with(i, cx, format);
        }
        ok
    }

    /// Save every buffer [`Buffer::autosave_candidate`] allows, without the
    /// "Saved" toast. A file changed on disk since it was loaded or saved is
    /// left alone (with a warning) until an explicit save; a failed write is
    /// not retried before the buffer's next edit.
    pub fn autosave(&mut self, cx: &mut Ctx) {
        for i in 0..self.buffers.len() {
            let b = &mut self.buffers[i];
            if !b.autosave_candidate() {
                continue;
            }
            if b.changed_on_disk() {
                b.autosave_conflict = true;
                cx.notify(Level::Warn, format!("{} changed on disk; not autosaving it (save explicitly to resolve)", b.name));
                continue;
            }
            if !self.write(i, cx, true) {
                let b = &mut self.buffers[i];
                b.autosave_failed = Some(b.version);
            }
        }
    }

    /// Whether `editor.autosave = "focus"`.
    pub fn autosave_on_focus_lost(&self) -> bool {
        self.autosave == Autosave::Focus
    }

    /// Note buffer edits since the last call (the app tick) and say whether
    /// an idle autosave is due.
    pub fn autosave_due(&mut self, now: Instant) -> bool {
        if self.autosave != Autosave::Idle {
            return false;
        }
        for b in &mut self.buffers {
            if b.autosave_seen != b.version {
                b.autosave_seen = b.version;
                self.last_edit = now;
            }
        }
        let any = self.buffers.iter().any(Buffer::autosave_candidate);
        idle_autosave_due(self.autosave, self.last_edit, now, self.autosave_idle, any)
    }
}

/// The file changed on disk since the buffer loaded or saved it. A file
/// deleted meanwhile is not a conflict: saving recreates it.
pub(super) fn disk_conflict(b: &Buffer) -> bool {
    b.changed_on_disk() && b.path.as_deref().is_some_and(Path::exists)
}

/// Make `b` the clean text of `path` as read from disk, keeping the caret
/// where it can stay.
pub(super) fn replace_with_disk_text(b: &mut Buffer, text: String, path: &Path) {
    b.disk_mtime = buffer::file_mtime(path);
    b.autosave_conflict = false;
    let old = b.text();
    if old != text {
        let caret = b.view.sel().head.min(text.chars().count());
        b.apply(vec![Change::replace(0, old, text)], Selection::point(caret), TxKind::Other);
    }
    b.mark_saved();
}

/// An idle autosave is due once `idle` has passed since the last edit and
/// some buffer can be autosaved.
pub(super) fn idle_autosave_due(mode: Autosave, last_edit: Instant, now: Instant, idle: Duration, any_dirty: bool) -> bool {
    mode == Autosave::Idle && any_dirty && now.saturating_duration_since(last_edit) >= idle
}
