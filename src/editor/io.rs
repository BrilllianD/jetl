//! Saving: explicit save, save all, autosave, atomic writes.

use std::path::Path;
use std::time::{Duration, Instant};

use super::Editor;
use super::buffer::{self, Buffer};
use crate::app::Ctx;
use crate::config::Autosave;
use crate::event::{Effect, Level};

impl Editor {
    /// Write buffer `i` to disk atomically. Synchronous on purpose: saves are
    /// small and the result must be known before quitting.
    pub fn save(&mut self, i: usize, cx: &mut Ctx) -> bool {
        self.write(i, cx, false)
    }

    /// [`Editor::save`]; `quiet` drops the "Saved" toast (errors still show).
    pub(super) fn write(&mut self, i: usize, cx: &mut Ctx, quiet: bool) -> bool {
        let (trim, final_newline) = (self.trim_trailing_whitespace, self.final_newline);
        let Some(b) = self.buffers.get_mut(i) else { return false };
        // `.editorconfig` overrides the editor-wide options per file.
        let trim = b.settings.trim_trailing_whitespace.unwrap_or(trim);
        let final_newline = b.settings.insert_final_newline.unwrap_or(final_newline);
        let Some(path) = b.path.clone() else {
            cx.notify(Level::Error, "buffer has no file path");
            return false;
        };
        if !b.dirty() {
            return true;
        }
        // Trimming other lines leaves the caret line where it was, so a view
        // that was following the caret need not scroll back to it (autosave
        // must not undo wheel scrolling).
        let follow = b.follow_key.filter(|k| (k.0, k.1) == (b.sel.head, b.version));
        b.prepare_for_save(trim, final_newline);
        if let Some((_, _, w, h)) = follow {
            b.follow_key = Some((b.sel.head, b.version, w, h));
        }
        match write_atomic(&path, &b.text()) {
            Ok(()) => {
                b.mark_saved();
                b.disk_mtime = buffer::file_mtime(&path);
                b.autosave_failed = None;
                b.autosave_conflict = false;
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

    pub fn save_all(&mut self, cx: &mut Ctx) -> bool {
        let mut ok = true;
        for i in 0..self.buffers.len() {
            if self.buffers[i].dirty() {
                ok &= self.save(i, cx);
            }
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
                cx.notify(Level::Warn, format!("{} changed on disk; not autosaving it (save explicitly to overwrite)", b.name));
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

/// An idle autosave is due once `idle` has passed since the last edit and
/// some buffer can be autosaved.
pub(super) fn idle_autosave_due(mode: Autosave, last_edit: Instant, now: Instant, idle: Duration, any_dirty: bool) -> bool {
    mode == Autosave::Idle && any_dirty && now.saturating_duration_since(last_edit) >= idle
}

/// Write via a temp file in the same directory, then rename over the target.
pub(super) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.jetl-tmp", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Preserve permissions of the original file when it exists.
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)
}
