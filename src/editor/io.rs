//! Saving: explicit save, save all, autosave, atomic writes, snapshots of
//! unsaved edits.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Editor;
use super::buffer::{self, Buffer, BufferId};
use super::selection::Selection;
use super::undo::{Change, TxKind};
use crate::app::Ctx;
use crate::config::Autosave;
use crate::event::{Effect, Level};
use crate::history::{Capture, Kind};
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
    /// edits and the file still differs from what the tab last saw. The
    /// text replaced goes to `sink` as a local history snapshot.
    pub fn reload_clean(&mut self, path: &Path, text: String, sink: &mut Vec<Capture>) -> bool {
        let Some(i) = self.index_of(path) else { return false };
        let b = &mut self.buffers[i];
        if b.dirty() || !b.changed_on_disk() {
            return false;
        }
        if let Some(old) = replace_with_disk_text(b, text, path) {
            sink.push(Capture::rope(path.to_path_buf(), old, Kind::BeforeReload));
        }
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

    /// Replace buffer `i`'s text with a local history snapshot: one
    /// undoable change, an ordinary edit (dirty, autosave may write it).
    /// False when the text is already the same.
    pub fn restore_text(&mut self, i: usize, text: &str) -> bool {
        let Some(b) = self.buffers.get_mut(i) else { return false };
        let old = b.text();
        if old == text {
            return false;
        }
        let caret = b.view.sel().head.min(text.chars().count());
        b.apply(vec![Change::replace(0, old, text)], Selection::point(caret), TxKind::Other);
        true
    }

    /// Put changes of a local history diff back into buffer `i` as one
    /// undoable edit (`changes` last first, `gutter::revert_spans`), the
    /// caret at the topmost. False, with nothing changed, when the buffer is
    /// read-only or its text is no longer `base`, the text the diff compared.
    pub fn restore_changes(&mut self, i: usize, base: &str, changes: Vec<Change>) -> bool {
        let Some(b) = self.buffers.get_mut(i) else { return false };
        if b.read_only || b.rope != base {
            return false;
        }
        let caret = changes.last().map_or(0, |c| c.at);
        b.apply(changes, Selection::point(caret), TxKind::Other);
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
    pub fn save_with(&mut self, i: usize, cx: &mut Ctx, format: bool) -> bool {
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
    /// (undoable; the edits also go to local history).
    pub fn reload_from_disk(&mut self, id: BufferId, cx: &mut Ctx) {
        let Some(i) = self.index_of_id(id) else { return };
        let b = &mut self.buffers[i];
        let Some(path) = b.path.clone() else { return };
        match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(text)) => {
                if let Some(old) = replace_with_disk_text(b, text, &path) {
                    let c = Capture::rope(path, old, Kind::BeforeReload);
                    cx.effects.push(Effect::History(crate::history::Request::Capture(c)));
                }
            }
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
    /// carries the save's result otherwise), then the save cleanup.
    fn prepare_write(&mut self, i: usize, cx: &mut Ctx) -> Result<(), bool> {
        let Some(b) = self.buffers.get(i) else { return Err(false) };
        if b.path.is_none() {
            cx.notify(Level::Error, "buffer has no file path");
            return Err(false);
        }
        if !b.dirty() {
            return Err(true);
        }
        self.cleanup_for_save(i);
        Ok(())
    }

    /// The save cleanup of buffer `i` (`trim_trailing_whitespace`,
    /// `final_newline`, each overridden by `.editorconfig`).
    fn cleanup_for_save(&mut self, i: usize) {
        let (trim, final_newline) = (self.trim_trailing_whitespace, self.final_newline);
        let Some(b) = self.buffers.get_mut(i) else { return };
        // `.editorconfig` overrides the editor-wide options per file.
        let trim = b.settings.trim_trailing_whitespace.unwrap_or(trim);
        let final_newline = b.settings.insert_final_newline.unwrap_or(final_newline);
        // Trimming other lines leaves the caret line where it was, so a view
        // that was following the caret need not scroll back to it (autosave
        // must not undo wheel scrolling).
        let follow = b.view.follow_key.filter(|k| (k.0, k.1) == (b.view.sel().head, b.version));
        b.prepare_for_save(trim, final_newline);
        if let Some((_, _, w, h)) = follow {
            b.view.follow_key = Some((b.view.sel().head, b.version, w, h));
        }
    }

    /// Write `text`, the buffer's undo state `state`, as buffer `i`'s file.
    pub(super) fn write_text(&mut self, i: usize, text: String, state: u64, cx: &mut Ctx, quiet: bool) -> bool {
        let Some(b) = self.buffers.get_mut(i) else { return false };
        let Some(path) = b.path.clone() else { return false };
        match write_atomic(&path, &text) {
            Ok(perm_err) => {
                let kind = if quiet { Kind::Autosave } else { Kind::Save };
                mark_written(b, path, state, perm_err, cx, quiet, Capture::text(PathBuf::new(), text, kind));
                true
            }
            Err(e) => {
                cx.notify(Level::Error, format!("save {}: {e}", path.display()));
                false
            }
        }
    }

    /// Save As: write buffer `i` (cleaned up first when it has unsaved
    /// edits) to `path`, then make it that file's buffer: name, language,
    /// `.editorconfig` and the rest are taken from the new path
    /// ([`super::adopt_path`]). Nothing changes when the write fails. No
    /// disk conflict check (the old file is not written). With `format`
    /// and unsaved edits, the new path's formatter then runs
    /// (`Effect::Format` behind `FileSaved`), its answer written again
    /// ([`Editor::format_done`]). The caller has checked that no other
    /// buffer has `path`. The snapshot's detail names the old path; its
    /// history is not carried (Save As is a copy).
    pub fn save_as(&mut self, i: usize, path: PathBuf, cx: &mut Ctx, format: bool, force_format: bool) -> bool {
        let Some(b) = self.buffers.get(i) else { return false };
        // As with Ctrl+S, only unsaved edits are formatted, or the text of a
        // save whose format `force_format` says was still pending.
        let format = format && (b.dirty() || force_format);
        if b.dirty() {
            self.cleanup_for_save(i);
        }
        let b = &self.buffers[i];
        let (text, state) = (b.text(), b.undo.state_id());
        let from = b.path.as_ref().filter(|p| **p != path).and_then(|p| p.to_str()).map(String::from);
        match write_atomic(&path, &text) {
            Ok(perm_err) => {
                let setup = self.path_setup(&path, &text, cx.theme);
                let b = &mut self.buffers[i];
                super::adopt_path(b, path.clone(), setup, self.tab_width);
                b.patch = None;
                b.read_only = false;
                let capture = Capture { detail: from, ..Capture::text(PathBuf::new(), text, Kind::SaveAs) };
                mark_written(b, path, state, perm_err, cx, false, capture);
                // Behind `FileSaved`, so the language server has the new
                // document first.
                if format && let Some(req) = self.format_request(i, true) {
                    cx.effects.push(Effect::Format(req));
                }
                true
            }
            Err(e) => {
                cx.notify(Level::Error, format!("save {}: {e}", path.display()));
                false
            }
        }
    }

    /// Save every dirty buffer that has a file (untitled ones are left to
    /// Save As). Only the first file changed on disk gets the Overwrite /
    /// Reload dialog; later ones stay unsaved with a warning.
    pub fn save_all(&mut self, cx: &mut Ctx) -> bool {
        self.save_all_with(cx, false)
    }

    /// [`Editor::save_all`], formatting like [`Editor::save_with`].
    pub(super) fn save_all_with(&mut self, cx: &mut Ctx, format: bool) -> bool {
        let mut ok = true;
        let mut asked = false;
        for i in 0..self.buffers.len() {
            let b = &self.buffers[i];
            // Untitled buffers are left to Save As (`Effect::SaveAs`).
            if !b.dirty() || b.path.is_none() || self.is_merge_buffer(b.id) {
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

    /// Local history snapshots of the unsaved edits of titled buffers changed
    /// since their last one (rope clones, O(1)); merge buffers, read-only
    /// panes and texts over `max_bytes` are left out. Marks each buffer seen
    /// at its version.
    pub fn unsaved_captures(&mut self, max_bytes: u64) -> Vec<Capture> {
        let merge = self.merge.as_ref().map(|m| m.ids);
        let mut out = Vec::new();
        for b in &mut self.buffers {
            let Some(path) = &b.path else { continue };
            if !b.dirty() || b.read_only || merge.is_some_and(|ids| ids.contains(&b.id)) || b.history_seen == b.version {
                continue;
            }
            b.history_seen = b.version;
            if b.rope.len_bytes() as u64 <= max_bytes {
                out.push(Capture::rope(path.clone(), b.rope.clone(), Kind::Unsaved));
            }
        }
        out
    }

    /// Local history captures of the unsaved edits about to be discarded:
    /// the buffers in `ids` (all with `None`) that are titled, dirty,
    /// writable and not merge buffers, each as `Kind::BeforeGit` with
    /// `DISCARD_DETAIL`. `history_seen` is ignored and left alone; texts
    /// over `max_bytes` are left out.
    pub fn discard_captures(&self, ids: Option<&[BufferId]>, max_bytes: u64) -> Vec<Capture> {
        let merge = self.merge.as_ref().map(|m| m.ids);
        let mut out = Vec::new();
        for b in &self.buffers {
            let Some(path) = &b.path else { continue };
            if !b.dirty() || b.read_only || merge.is_some_and(|m| m.contains(&b.id)) || ids.is_some_and(|ids| !ids.contains(&b.id)) {
                continue;
            }
            if b.rope.len_bytes() as u64 <= max_bytes {
                let mut c = Capture::rope(path.clone(), b.rope.clone(), Kind::BeforeGit);
                c.detail = Some(crate::history::DISCARD_DETAIL.to_string());
                out.push(c);
            }
        }
        out
    }
}

/// `b` was written to `path` at undo state `state`: it is clean, the
/// file's mtime is noted, autosave may try again, the text written goes to
/// local history (`snap`, its path set here), and `Effect::FileSaved`
/// follows (with the "Saved" toast unless `quiet`).
fn mark_written(b: &mut Buffer, path: PathBuf, state: u64, perm_err: Option<std::io::Error>, cx: &mut Ctx, quiet: bool, mut snap: Capture) {
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
    snap.path = path.clone();
    cx.effects.push(Effect::History(crate::history::Request::Capture(snap)));
    cx.effects.push(Effect::FileSaved { path, quiet });
}

/// The file changed on disk since the buffer loaded or saved it. A file
/// deleted meanwhile is not a conflict: saving recreates it.
pub(super) fn disk_conflict(b: &Buffer) -> bool {
    b.changed_on_disk() && b.path.as_deref().is_some_and(Path::exists)
}

/// Make `b` the clean text of `path` as read from disk, keeping the caret
/// where it can stay. Returns the text replaced, when it differed.
pub(super) fn replace_with_disk_text(b: &mut Buffer, text: String, path: &Path) -> Option<ropey::Rope> {
    b.disk_mtime = buffer::file_mtime(path);
    b.autosave_conflict = false;
    // A rope clone is O(1); the worker makes it bytes.
    let before = b.rope.clone();
    let replaced = b.replace_by_lines(&text).then_some(before);
    b.mark_saved();
    replaced
}

/// An idle autosave is due once `idle` has passed since the last edit and
/// some buffer can be autosaved.
pub(super) fn idle_autosave_due(mode: Autosave, last_edit: Instant, now: Instant, idle: Duration, any_dirty: bool) -> bool {
    mode == Autosave::Idle && any_dirty && now.saturating_duration_since(last_edit) >= idle
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::config::Config;
    use crate::config::theme::Theme;
    use crate::editor::buffer::LineEnding;
    use crate::history::{CaptureText, Request};
    use crate::workspace::Workspace;

    /// A temp dir removed on drop, so a failing test cleans up too.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("jetl-test-io-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run `f` with a throwaway `Ctx`; returns every effect it produced.
    fn effects(f: impl FnOnce(&mut Ctx)) -> Vec<Effect> {
        let config = Config::default();
        let theme = Theme::default();
        let workspace = Workspace::new(PathBuf::from("/tmp"));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        f(&mut cx);
        cx.effects
    }

    /// The notifications among `fx`.
    fn notes(fx: &[Effect]) -> Vec<(Level, String)> {
        fx.iter().filter_map(|e| if let Effect::Notify(l, t) = e { Some((*l, t.clone())) } else { None }).collect()
    }

    /// An editor with `path` (written with `text` first) open.
    fn open(path: &Path, text: &str) -> Editor {
        std::fs::write(path, text).unwrap();
        let mut ed = Editor::new(&Config::default());
        ed.open_text(path.to_path_buf(), text, false, &Theme::default());
        ed
    }

    #[test]
    fn save_writes_through_a_renamed_temp_file() {
        let dir = TmpDir::new("atomic");
        let a = dir.0.join("a.txt");
        std::fs::write(&a, "one\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let link = dir.0.join("link.txt");
        std::fs::hard_link(&a, &link).unwrap();
        let mut ed = open(&a, "one\n");
        ed.buffers[0].view.set_sel(Selection::point(0));
        ed.buffers[0].insert("x", TxKind::Typing);
        effects(|cx| assert!(ed.save(0, cx)));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "xone\n");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "one\n", "the old inode was replaced, not rewritten");
        let left: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".jetl-tmp"))
            .collect();
        assert!(left.is_empty(), "temp file left: {left:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn save_reports_the_text_written_and_marks_the_buffer_clean() {
        let dir = TmpDir::new("effects");
        let path = dir.0.join("a.txt");
        let mut ed = open(&path, "one\n");
        ed.buffers[0].view.set_sel(Selection::point(0));
        ed.buffers[0].insert("x", TxKind::Typing);
        let fx = effects(|cx| assert!(ed.save(0, cx)));
        match fx.as_slice() {
            [
                Effect::History(Request::Capture(Capture { path: p, text: CaptureText::Text(t), kind: Kind::Save, detail: None })),
                Effect::FileSaved { path: q, quiet: false },
            ] => {
                assert_eq!(p, &path);
                assert_eq!(q, &path);
                assert_eq!(t, "xone\n");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
        let b = &ed.buffers[0];
        assert!(!b.dirty());
        assert_eq!(b.disk_mtime, buffer::file_mtime(&path));
        assert!(!b.recovered);
        assert_eq!(b.autosave_failed, None);
    }

    #[test]
    fn save_of_a_clean_buffer_writes_nothing() {
        let dir = TmpDir::new("clean");
        let path = dir.0.join("a.txt");
        let mut ed = open(&path, "one\n");
        std::fs::write(&path, "theirs\n").unwrap();
        let fx = effects(|cx| assert!(ed.save(0, cx)));
        assert!(fx.is_empty(), "{fx:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
    }

    #[test]
    fn failed_write_keeps_the_edits_and_reports_the_error() {
        let dir = TmpDir::new("fail");
        let path = dir.0.join("missing").join("a.txt");
        let mut ed = Editor::new(&Config::default());
        ed.open_text(path, "one\n", false, &Theme::default());
        ed.buffers[0].view.set_sel(Selection::point(0));
        ed.buffers[0].insert("x", TxKind::Typing);
        let fx = effects(|cx| assert!(!ed.save(0, cx)));
        let n = notes(&fx);
        assert_eq!(n.len(), 1, "{n:?}");
        assert_eq!(n[0].0, Level::Error);
        assert!(n[0].1.starts_with("save "), "{}", n[0].1);
        assert!(n[0].1.contains("missing"), "{}", n[0].1);
        assert!(!fx.iter().any(|e| matches!(e, Effect::FileSaved { .. })));
        assert!(ed.buffers[0].dirty());
    }

    #[test]
    fn save_all_writes_dirty_titled_buffers_only() {
        let dir = TmpDir::new("all");
        let (a, b) = (dir.0.join("a.txt"), dir.0.join("b.txt"));
        let mut ed = open(&a, "a\n");
        std::fs::write(&b, "b\n").unwrap();
        ed.open_text(b.clone(), "b\n", false, &Theme::default());
        let u = ed.open_untitled("untitled");
        ed.buffers[u].insert("u", TxKind::Typing);
        ed.buffers[0].view.set_sel(Selection::point(0));
        ed.buffers[0].insert("x", TxKind::Typing);
        let fx = effects(|cx| assert!(ed.save_all(cx)));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "xa\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b\n");
        let saved: Vec<_> =
            fx.iter().filter_map(|e| if let Effect::FileSaved { path, .. } = e { Some(path.clone()) } else { None }).collect();
        assert_eq!(saved, vec![a]);
        assert!(notes(&fx).is_empty(), "{:?}", notes(&fx));
        assert!(ed.buffers[u].dirty());
    }

    #[test]
    fn autosave_skips_recovered_and_lossy_buffers() {
        let dir = TmpDir::new("auto-skip");
        let (a, b) = (dir.0.join("a.txt"), dir.0.join("b.txt"));
        let mut ed = open(&a, "disk a\n");
        std::fs::write(&b, "disk b\n").unwrap();
        ed.open_text(b.clone(), "disk b\n", true, &Theme::default());
        assert!(ed.recover_text(0, "recovered\n"));
        ed.buffers[1].view.set_sel(Selection::point(0));
        ed.buffers[1].insert("x", TxKind::Typing);
        let fx = effects(|cx| ed.autosave(cx));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "disk a\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "disk b\n");
        assert!(fx.is_empty(), "{fx:?}");

        effects(|cx| assert!(ed.save(0, cx)));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "recovered\n");
        assert!(!ed.buffers[0].recovered);
        let fx = effects(|cx| assert!(ed.save(1, cx)));
        assert!(
            notes(&fx).contains(&(Level::Warn, "b.txt was not valid UTF-8; saved re-encoded as UTF-8".to_string())),
            "{:?}",
            notes(&fx)
        );
        assert!(!ed.buffers[1].lossy);
    }

    #[test]
    fn disk_changes_reload_clean_tabs_and_ask_once_for_dirty_ones() {
        let dir = TmpDir::new("disk");
        let a = dir.0.join("a.txt");
        let mut ed = open(&a, "a\n");
        assert_eq!(ed.on_disk_change(&dir.0.join("other.txt")), DiskChange::None);
        assert_eq!(ed.on_disk_change(&a), DiskChange::None);
        // The seam for "changed on disk", no sleep.
        ed.buffers[0].disk_mtime = Some(UNIX_EPOCH);
        assert_eq!(ed.on_disk_change(&a), DiskChange::Reload);
        ed.buffers[0].insert("x", TxKind::Typing);
        assert_eq!(ed.on_disk_change(&a), DiskChange::Conflict(0));
        assert!(ed.buffers[0].autosave_conflict);
        assert_eq!(ed.on_disk_change(&a), DiskChange::None, "asked once");
        let id = ed.buffers[0].id;
        effects(|cx| assert!(ed.overwrite(id, cx)));
        assert!(!ed.buffers[0].autosave_conflict);

        ed.buffers[0].insert("y", TxKind::Typing);
        std::fs::remove_file(&a).unwrap();
        ed.buffers[0].disk_mtime = Some(UNIX_EPOCH);
        assert_eq!(ed.on_disk_change(&a), DiskChange::None, "a vanished file is not a conflict");
    }

    #[test]
    fn reload_from_disk_refuses_invalid_utf8_and_reports_a_missing_file() {
        let dir = TmpDir::new("reload-err");
        let a = dir.0.join("a.txt");
        let mut ed = open(&a, "a\n");
        ed.buffers[0].view.set_sel(Selection::point(0));
        ed.buffers[0].insert("x", TxKind::Typing);
        let id = ed.buffers[0].id;
        std::fs::write(&a, [0xff, b'\n']).unwrap();
        let fx = effects(|cx| ed.reload_from_disk(id, cx));
        assert_eq!(notes(&fx), vec![(Level::Error, "a.txt is not valid UTF-8; not reloaded".to_string())]);
        assert_eq!(ed.buffers[0].text(), "xa\n");
        assert!(ed.buffers[0].dirty());

        std::fs::remove_file(&a).unwrap();
        let fx = effects(|cx| ed.reload_from_disk(id, cx));
        let n = notes(&fx);
        assert_eq!(n.len(), 1, "{n:?}");
        assert_eq!(n[0].0, Level::Error);
        assert!(n[0].1.starts_with("reload "), "{}", n[0].1);
        assert!(n[0].1.contains("a.txt"), "{}", n[0].1);

        std::fs::write(&a, "xa\n").unwrap();
        let fx = effects(|cx| ed.reload_from_disk(id, cx));
        assert!(notes(&fx).is_empty());
        assert!(!fx.iter().any(|e| matches!(e, Effect::History(_))), "same text, nothing captured");
        assert!(!ed.buffers[0].dirty());
    }

    #[test]
    fn crlf_and_bom_survive_open_edit_save_and_reload() {
        let dir = TmpDir::new("crlf-bom");
        let a = dir.0.join("a.txt");
        let bytes: &[u8] = b"\xEF\xBB\xBFone\r\ntwo";
        std::fs::write(&a, bytes).unwrap();
        let mut ed = Editor::new(&Config::default());
        ed.open_text(a.clone(), std::str::from_utf8(bytes).unwrap(), false, &Theme::default());
        assert_eq!(ed.buffers[0].line_ending, LineEnding::CrLf);
        assert_eq!(ed.buffers[0].text().chars().next(), Some('\u{feff}'));
        let end = ed.buffers[0].len_chars();
        ed.buffers[0].view.set_sel(Selection::point(end));
        ed.buffers[0].insert("!", TxKind::Typing);
        effects(|cx| assert!(ed.save(0, cx)));
        // `.editorconfig` may override the final newline; the default is the
        // reference, as in `save_keeps_unicode_separators_byte_for_byte`.
        let want: &[u8] = if Config::default().editor.final_newline { b"\xEF\xBB\xBFone\r\ntwo!\r\n" } else { b"\xEF\xBB\xBFone\r\ntwo!" };
        assert_eq!(std::fs::read(&a).unwrap(), want);

        std::fs::write(&a, b"\xEF\xBB\xBFthree\r\n").unwrap();
        let id = ed.buffers[0].id;
        effects(|cx| ed.reload_from_disk(id, cx));
        assert_eq!(ed.buffers[0].text(), "\u{feff}three\r\n");
        assert_eq!(ed.buffers[0].line_ending, LineEnding::CrLf);
        effects(|cx| assert!(ed.save(0, cx)));
        assert_eq!(std::fs::read(&a).unwrap(), b"\xEF\xBB\xBFthree\r\n");
    }
}
