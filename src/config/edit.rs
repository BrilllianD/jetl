//! Writing settings back into `config.toml`, and key bindings into `keymap.toml`.
//!
//! Edits go through `toml_edit`, so the user's comments, key order and
//! spacing survive. The whole result is validated with `Config::parse`
//! before anything is written, and the file is replaced atomically.

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Key, TableLike};

use super::keymap::{KeyCombo, Keymap, chord_key, default_keymap, parse_chord};
use super::theme::error_summary;
use super::{Config, FileText};

/// What Open Settings writes into a `config.toml` it creates.
pub const SETTINGS_STUB: &str = "# jetl settings; every key and its default: docs/CONFIG.md\n";

/// A value a setting can hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<String>),
}

/// A change to one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Set(Value),
    /// Remove the key, so the default applies.
    Reset,
}

impl Value {
    fn to_toml(&self) -> toml_edit::Value {
        match self {
            Self::Bool(b) => (*b).into(),
            Self::Int(i) => (*i).into(),
            Self::Str(s) => s.as_str().into(),
            Self::List(l) => l.iter().map(String::as_str).collect::<Array>().into(),
        }
    }
}

/// An empty table for a section about to be added to `doc`. The trailing
/// comments of any document render after every table: they move into the
/// new table's prefix, so it lands after them.
fn new_table(doc: &mut DocumentMut) -> toml_edit::Table {
    let mut t = toml_edit::Table::new();
    let trailing = doc.trailing().as_str().unwrap_or("").to_string();
    let comments = if doc.as_table().is_empty() || !trailing.trim().is_empty() { trailing } else { String::new() };
    if !comments.is_empty() {
        doc.set_trailing("");
    }
    let has_content = !doc.to_string().trim().is_empty() || !comments.trim().is_empty();
    if has_content {
        t.decor_mut().set_prefix(format!("{comments}\n"));
    }
    t
}

/// `text` with `edits` applied (key as `section.field`), or why it cannot be.
pub fn apply_edits(text: &str, edits: &[(&str, Edit)]) -> Result<String, String> {
    let mut doc: DocumentMut =
        text.parse().map_err(|e: toml_edit::TomlError| format!("config.toml is not valid TOML: {}", error_summary(&e.to_string())))?;
    for (key, edit) in edits {
        let (section, field) = key.split_once('.').ok_or_else(|| format!("bad key {key}"))?;
        match edit {
            Edit::Set(v) => {
                if doc.get(section).is_none() {
                    let t = new_table(&mut doc);
                    doc.insert(section, Item::Table(t));
                }
                let table = doc
                    .get_mut(section)
                    .and_then(Item::as_table_like_mut)
                    .ok_or_else(|| format!("{section} in config.toml is not a table"))?;
                let mut new = v.to_toml();
                // Replace in place: the key keeps its leading comment, the value its
                // spacing and trailing comment.
                if let Some(slot) = table.get_mut(field) {
                    if let Some(old) = slot.as_value() {
                        *new.decor_mut() = old.decor().clone();
                    }
                    *slot = Item::Value(new);
                } else {
                    table.insert(field, Item::Value(new));
                }
            }
            Edit::Reset => {
                if let Some(table) = doc.get_mut(section).and_then(Item::as_table_like_mut) {
                    table.remove(field);
                }
            }
        }
    }
    let out = doc.to_string();
    Config::parse(&out).map_err(|e| error_summary(&e.to_string()))?;
    Ok(out)
}

/// Replace the file at `path` atomically with `text`. A symlink stays a
/// symlink and its target is written.
fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let target = if is_link { std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))? } else { path.to_path_buf() };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    match crate::util::fs::write_atomic(&target, text) {
        Ok(Some(perm_err)) => {
            tracing::warn!(
                "{} permissions not kept: {perm_err}",
                path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            )
        }
        Ok(None) => {}
        Err(e) => return Err(format!("write {}: {e}", target.display())),
    }
    Ok(())
}

/// Apply `edits` to the file at `path` (a missing file starts from
/// `SETTINGS_STUB`) and write the result; returns the text written. A
/// symlinked `config.toml` stays a symlink and its target is written.
pub fn write_config(path: &Path, edits: &[(&str, Edit)]) -> Result<String, String> {
    let base = match FileText::read(path) {
        FileText::Missing => SETTINGS_STUB.to_string(),
        FileText::Unreadable(e) => return Err(format!("{}: {e}", path.display())),
        FileText::Text(t) => t,
    };
    let text = apply_edits(&base, edits).map_err(|e| format!("{}: {e}", path.display()))?;
    write_file(path, &text)?;
    Ok(text)
}

/// A staged change to `keymap.toml` (Settings › Keymap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapEdit {
    /// Bind `chord` to action `id` in context `ctx`.
    Bind { ctx: String, chord: Vec<KeyCombo>, id: &'static str },
    /// Remove `chord` from context `ctx` (written as `"none"` when the default binds it).
    Unbind { ctx: String, chord: Vec<KeyCombo> },
}

/// Follow `segs` down from `root` through table-like items; `None` when one is
/// missing or is not a table.
fn walk<'a>(root: &'a mut dyn TableLike, segs: &[&str]) -> Option<&'a mut dyn TableLike> {
    let mut cur = root;
    for s in segs {
        cur = cur.get_mut(s)?.as_table_like_mut()?;
    }
    Some(cur)
}

/// The table of context `ctx` (`git.changes` is `[git.changes]`). With `create`
/// a missing one is added (its parents implicit, so no empty `[git]` header
/// is written); without, a missing one is `Ok(None)`.
fn ctx_table<'a>(doc: &'a mut DocumentMut, ctx: &str, create: bool) -> Result<Option<&'a mut dyn TableLike>, String> {
    let segs: Vec<&str> = ctx.split('.').collect();
    let not_table = || format!("[{ctx}] in keymap.toml is not a table");
    let mut first_missing = None;
    {
        let mut cur: &dyn TableLike = doc.as_table();
        for (i, s) in segs.iter().enumerate() {
            match cur.get(s) {
                None => {
                    first_missing = Some(i);
                    break;
                }
                Some(item) => cur = item.as_table_like().ok_or_else(not_table)?,
            }
        }
    }
    if let Some(k) = first_missing {
        if !create {
            return Ok(None);
        }
        // The leaf carries the moved trailing comments; the parents above it are implicit.
        let mut chain = if k == 0 { new_table(doc) } else { toml_edit::Table::new() };
        for j in (k..segs.len() - 1).rev() {
            let mut parent = toml_edit::Table::new();
            parent.set_implicit(true);
            parent.insert(segs[j + 1], Item::Table(chain));
            chain = parent;
        }
        let parent = walk(doc.as_table_mut(), &segs[..k]).ok_or_else(not_table)?;
        parent.insert(segs[k], Item::Table(chain));
    }
    walk(doc.as_table_mut(), &segs).map(Some).ok_or_else(not_table)
}

/// Keys of `t` with a plain value that parse to `chord`, in table order.
fn matching_keys(t: &dyn TableLike, chord: &[KeyCombo]) -> Vec<String> {
    t.iter()
        .filter(|(k, item)| item.is_value() && !item.is_inline_table() && parse_chord(k).is_ok_and(|c| c == chord))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// Bind `chord` to `value` in `t`. A key that already spells the chord keeps
/// its place and comments (other spellings are dropped); otherwise the
/// chord is written as [`chord_key`].
fn put_chord(t: &mut dyn TableLike, chord: &[KeyCombo], value: &str) -> Result<(), String> {
    let keys = matching_keys(t, chord);
    if let Some((first, others)) = keys.split_first() {
        let mut new = toml_edit::value(value);
        if let Some(slot) = t.get_mut(first) {
            if let (Some(old), Some(nv)) = (slot.as_value(), new.as_value_mut()) {
                *nv.decor_mut() = old.decor().clone();
            }
            *slot = new;
        }
        for k in others {
            t.remove(k);
        }
        return Ok(());
    }
    let key = writable_key(chord)?;
    t.insert(&key, toml_edit::value(value));
    Ok(())
}

/// [`chord_key`] of `chord`, or why it would not read back as `chord`.
fn writable_key(chord: &[KeyCombo]) -> Result<String, String> {
    let key = chord_key(chord);
    if parse_chord(&key).ok().as_deref() != Some(chord) {
        return Err(format!("{key} cannot be written to keymap.toml"));
    }
    Ok(key)
}

/// Rewrite the entry for `old` as `new` = `value` where it stands, keeping its
/// comments; `Ok(false)` when `t` has no entry for `old` or already has one for `new`.
/// The table is untouched when it returns `Ok(false)` or an error.
fn rename_chord(t: &mut dyn TableLike, old: &[KeyCombo], new: &[KeyCombo], value: &str) -> Result<bool, String> {
    let keys = matching_keys(t, old);
    let Some((first, others)) = keys.split_first() else {
        return Ok(false);
    };
    if !matching_keys(t, new).is_empty() {
        return Ok(false);
    }
    let name = writable_key(new)?;
    let Some(old_key) = t.key(first).cloned() else {
        return Ok(false);
    };
    for k in others {
        t.remove(k);
    }
    let tail_names: Vec<String> = t.iter().map(|(k, _)| k.to_string()).skip_while(|k| k != first).skip(1).collect();
    let mut tail: Vec<(Key, Item)> = Vec::new();
    for n in &tail_names {
        let k = t.key(n).cloned();
        let item = t.remove(n);
        if let (Some(k), Some(item)) = (k, item) {
            tail.push((k, item));
        }
    }
    let old_item = t.remove(first).expect("`first` was looked up above and only other keys were removed since");
    let new_key = Key::new(name).with_leaf_decor(old_key.leaf_decor().clone()).with_dotted_decor(old_key.dotted_decor().clone());
    let mut item = toml_edit::value(value);
    if let (Some(o), Some(nv)) = (old_item.as_value(), item.as_value_mut()) {
        *nv.decor_mut() = o.decor().clone();
    }
    if let toml_edit::Entry::Vacant(e) = t.entry_format(&new_key) {
        e.insert(item);
    }
    for (k, item) in tail {
        if let toml_edit::Entry::Vacant(e) = t.entry_format(&k) {
            e.insert(item);
        }
    }
    Ok(true)
}

fn remove_chord(t: &mut dyn TableLike, chord: &[KeyCombo]) {
    for k in matching_keys(t, chord) {
        t.remove(&k);
    }
}

/// `text` (a `keymap.toml`) with `edits` applied in order, or why it cannot be.
/// Comments and the order of the user's entries survive. The result is refused
/// when it merges with an error the input did not already have.
pub fn apply_keymap_edits(text: &str, edits: &[KeymapEdit]) -> Result<String, String> {
    let mut doc: DocumentMut =
        text.parse().map_err(|e: toml_edit::TomlError| format!("keymap.toml is not valid TOML: {}", error_summary(&e.to_string())))?;
    let defaults = default_keymap();
    let mut i = 0;
    while i < edits.len() {
        if let (KeymapEdit::Unbind { ctx, chord: old }, Some(KeymapEdit::Bind { ctx: c2, chord: new, id })) = (&edits[i], edits.get(i + 1))
            && c2 == ctx
            && !defaults.table(ctx).is_some_and(|t| t.bindings.contains_key(old))
            && let Some(t) = ctx_table(&mut doc, ctx, false)?
        {
            let done = if old == new {
                put_chord(t, new, id)?;
                true
            } else {
                rename_chord(t, old, new, id)?
            };
            if done {
                i += 2;
                continue;
            }
        }
        match &edits[i] {
            KeymapEdit::Bind { ctx, chord, id } => {
                let t = ctx_table(&mut doc, ctx, true)?.ok_or_else(|| format!("[{ctx}] in keymap.toml is not a table"))?;
                put_chord(t, chord, id)?;
            }
            KeymapEdit::Unbind { ctx, chord } => {
                if defaults.table(ctx).is_some_and(|t| t.bindings.contains_key(chord)) {
                    let t = ctx_table(&mut doc, ctx, true)?.ok_or_else(|| format!("[{ctx}] in keymap.toml is not a table"))?;
                    put_chord(t, chord, "none")?;
                } else if let Some(t) = ctx_table(&mut doc, ctx, false)? {
                    remove_chord(t, chord);
                }
            }
        }
        i += 1;
    }
    let out = doc.to_string();
    let before = Keymap::default().merge_toml(text).unwrap_or_default();
    match Keymap::default().merge_toml(&out) {
        Err(e) => Err(error_summary(&e.to_string())),
        Ok(errs) => match errs.into_iter().find(|e| !before.contains(e)) {
            Some(e) => Err(e),
            None => Ok(out),
        },
    }
}

/// Apply `edits` to the `keymap.toml` at `path` (a missing file is empty) and
/// write the result; returns the text written.
pub fn write_keymap(path: &Path, edits: &[KeymapEdit]) -> Result<String, String> {
    let base = match FileText::read(path) {
        FileText::Missing => String::new(),
        FileText::Unreadable(e) => return Err(format!("{}: {e}", path.display())),
        FileText::Text(t) => t,
    };
    let text = apply_keymap_edits(&base, edits).map_err(|e| format!("{}: {e}", path.display()))?;
    write_file(path, &text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(text: &str, key: &str, v: Value) -> Result<String, String> {
        apply_edits(text, &[(key, Edit::Set(v))])
    }

    #[test]
    fn comments_and_order_survive() {
        let input = "# top\n[editor]\n# caret margin\nscrolloff = 3   # lines\ntab_width = 4\n\n[git]\nlog_all = true\n";
        let out = set(input, "editor.scrolloff", Value::Int(9)).unwrap();
        assert_eq!(out, "# top\n[editor]\n# caret margin\nscrolloff = 9   # lines\ntab_width = 4\n\n[git]\nlog_all = true\n");
    }

    #[test]
    fn new_key_and_new_section() {
        let out = set("[editor]\ntab_width = 4\n", "editor.scrolloff", Value::Int(2)).unwrap();
        assert_eq!(out, "[editor]\ntab_width = 4\nscrolloff = 2\n");
        let out = set("[editor]\ntab_width = 4\n", "ui.icons", Value::Str("emoji".into())).unwrap();
        assert_eq!(out, "[editor]\ntab_width = 4\n\n[ui]\nicons = \"emoji\"\n");
        assert_eq!(set("", "ui.icons", Value::Str("emoji".into())).unwrap(), "[ui]\nicons = \"emoji\"\n");
    }

    #[test]
    fn new_section_goes_after_trailing_comments() {
        let input = "[editor]\ntab_width = 4\n\n# [git]\n# log_all = false\n";
        let out = set(input, "ui.icons", Value::Str("emoji".into())).unwrap();
        assert_eq!(out, "[editor]\ntab_width = 4\n\n# [git]\n# log_all = false\n\n[ui]\nicons = \"emoji\"\n");
        assert!(Config::parse(&out).is_ok());
    }

    #[test]
    fn new_key_keeps_trailing_comments_last() {
        let input = "[editor]\ntab_width = 4\n\n# [git]\n# log_all = false\n";
        let out = set(input, "editor.scrolloff", Value::Int(2)).unwrap();
        assert_eq!(out, "[editor]\ntab_width = 4\nscrolloff = 2\n\n# [git]\n# log_all = false\n");
    }

    #[test]
    fn dotted_and_inline_sections() {
        let out = set("editor.tab_width = 2\n", "editor.scrolloff", Value::Int(5)).unwrap();
        let c = Config::parse(&out).unwrap();
        assert_eq!((c.editor.tab_width, c.editor.scrolloff), (2, 5));
        let out = set("editor = { tab_width = 2 }\n", "editor.scrolloff", Value::Int(5)).unwrap();
        let c = Config::parse(&out).unwrap();
        assert_eq!((c.editor.tab_width, c.editor.scrolloff), (2, 5));
    }

    #[test]
    fn reset_removes_only_the_key() {
        let input = "[editor]\nscrolloff = 3\ntab_width = 4\n";
        let out = apply_edits(input, &[("editor.scrolloff", Edit::Reset)]).unwrap();
        assert_eq!(out, "[editor]\ntab_width = 4\n");
        assert_eq!(apply_edits(input, &[("editor.nope", Edit::Reset)]).unwrap(), input);
        assert_eq!(apply_edits(input, &[("git.log_all", Edit::Reset)]).unwrap(), input);
    }

    #[test]
    fn lists_and_strings_round_trip() {
        let out = apply_edits(
            "",
            &[
                ("editor.soft_wrap_files", Edit::Set(Value::List(vec!["*.md".into(), "*.txt".into()]))),
                ("terminal.args", Edit::Set(Value::List(vec!["-l".into()]))),
                ("worktree.default_dir", Edit::Set(Value::Str("../wt \"x\"".into()))),
            ],
        )
        .unwrap();
        let c = Config::parse(&out).unwrap();
        assert_eq!(c.editor.soft_wrap_files, vec!["*.md", "*.txt"]);
        assert_eq!(c.terminal.args, vec!["-l"]);
        assert_eq!(c.worktree.default_dir, "../wt \"x\"");
    }

    #[test]
    fn invalid_input_is_an_error() {
        assert!(set("", "editor.tab_width", Value::Str("x".into())).is_err());
        let e = set("[editor\n", "editor.tab_width", Value::Int(2)).unwrap_err();
        assert!(e.starts_with("config.toml is not valid TOML"), "{e}");
        let e = set("editor = 3\n", "editor.tab_width", Value::Int(2)).unwrap_err();
        assert!(e.contains("not a table"), "{e}");
        assert!(set("", "nodot", Value::Int(1)).unwrap_err().starts_with("bad key"));
    }

    fn test_dir(case: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jetl-test-edit-{case}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_config_missing_file_starts_from_stub() {
        let dir = test_dir("missing");
        let path = dir.join("sub").join("config.toml");
        let text = write_config(&path, &[("editor.scrolloff", Edit::Set(Value::Int(4)))]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(text.starts_with(SETTINGS_STUB));
        assert!(text.contains("[editor]\nscrolloff = 4\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_config_invalid_edit_leaves_file_alone() {
        let dir = test_dir("invalid");
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\n[editor]\ntab_width = 4\n").unwrap();
        let e = write_config(&path, &[("editor.tab_width", Edit::Set(Value::Str("x".into())))]).unwrap_err();
        assert!(e.starts_with(&path.display().to_string()), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# mine\n[editor]\ntab_width = 4\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_config_keeps_symlink() {
        let dir = test_dir("symlink");
        let target = dir.join("real.toml");
        let link = dir.join("config.toml");
        std::fs::write(&target, "[editor]\ntab_width = 4\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_config(&link, &[("editor.tab_width", Edit::Set(Value::Int(8)))]).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "[editor]\ntab_width = 8\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ch(s: &str) -> Vec<KeyCombo> {
        parse_chord(s).unwrap()
    }

    fn bind(ctx: &str, key: &str, id: &'static str) -> KeymapEdit {
        KeymapEdit::Bind { ctx: ctx.into(), chord: ch(key), id }
    }

    fn unbind(ctx: &str, key: &str) -> KeymapEdit {
        KeymapEdit::Unbind { ctx: ctx.into(), chord: ch(key) }
    }

    #[test]
    fn keymap_bind_keeps_comments_and_other_entries() {
        let input = "# mine\n[global]\n# about\n\"f2\" = \"app.about\" # keep\n";
        let out = apply_keymap_edits(input, &[bind("global", "ctrl-alt-y", "app.quit")]).unwrap();
        assert_eq!(out, "# mine\n[global]\n# about\n\"f2\" = \"app.about\" # keep\n\"Ctrl+Alt+Y\" = \"app.quit\"\n");
    }

    #[test]
    fn keymap_bind_nested_context() {
        let out = apply_keymap_edits("", &[bind("git.changes", "f5", "app.about")]).unwrap();
        let mut km = Keymap::default();
        assert!(km.merge_toml(&out).unwrap().is_empty());
        assert_eq!(km.table("git.changes").unwrap().bindings.get(&ch("f5")), crate::actions::Action::from_id("app.about").as_ref());
        assert!(!out.lines().any(|l| l == "[git]"), "{out}");
        let out = apply_keymap_edits("[git.changes]\n\"f5\" = \"app.about\"\n", &[bind("global", "ctrl-alt-y", "app.quit")]).unwrap();
        let mut km = Keymap::default();
        km.merge_toml(&out).unwrap();
        assert!(km.table("git.changes").unwrap().bindings.contains_key(&ch("f5")));
        assert!(km.table("global").unwrap().bindings.contains_key(&ch("ctrl-alt-y")));
    }

    #[test]
    fn keymap_unbind_default_writes_none() {
        assert!(default_keymap().table("global").unwrap().bindings.contains_key(&ch("ctrl-q")));
        let out = apply_keymap_edits("", &[unbind("global", "ctrl-q")]).unwrap();
        assert_eq!(out, "[global]\n\"Ctrl+Q\" = \"none\"\n");
    }

    #[test]
    fn keymap_unbind_user_entry_removes_it() {
        assert!(!default_keymap().table("global").unwrap().bindings.contains_key(&ch("f2")));
        let out = apply_keymap_edits("[global]\n\"f2\" = \"app.about\"\n\"f3\" = \"app.quit\"\n", &[unbind("global", "f2")]).unwrap();
        assert_eq!(out, "[global]\n\"f3\" = \"app.quit\"\n");
    }

    #[test]
    fn keymap_same_chord_other_spelling_is_replaced() {
        let input = "[global]\n\"ctrl-alt-y\" = \"app.about\" # mine\n\"C-A-y\" = \"app.about\"\n";
        let out = apply_keymap_edits(input, &[bind("global", "ctrl-alt-y", "app.quit")]).unwrap();
        assert_eq!(out, "[global]\n\"ctrl-alt-y\" = \"app.quit\" # mine\n");
    }

    #[test]
    fn keymap_edit_is_unbind_then_bind() {
        let input = "[global]\n# about\n\"f2\" = \"app.about\"   # keep me\n\"f3\" = \"app.quit\"\n";
        let out = apply_keymap_edits(input, &[unbind("global", "f2"), bind("global", "f4", "app.about")]).unwrap();
        assert_eq!(out, "[global]\n# about\nF4 = \"app.about\"   # keep me\n\"f3\" = \"app.quit\"\n");
        let input = "[git.changes]\n\"f5\" = \"app.about\" # x\n\"f6\" = \"app.quit\"\n";
        let out = apply_keymap_edits(input, &[unbind("git.changes", "f5"), bind("git.changes", "f7", "app.about")]).unwrap();
        assert_eq!(out, "[git.changes]\nF7 = \"app.about\" # x\n\"f6\" = \"app.quit\"\n");
    }

    #[test]
    fn keymap_edit_in_a_table_followed_by_a_sub_table() {
        for k in ["f2", "f3", "f4", "f6"] {
            assert!(!default_keymap().table("git").unwrap().bindings.contains_key(&ch(k)), "{k}");
        }
        let input = "[git]\n# about\n\"f2\" = \"app.about\" # keep\n\"f3\" = \"app.quit\"\n\n# changes view\n[git.changes]\n\"f6\" = \"app.quit\"\n";
        let out = apply_keymap_edits(input, &[unbind("git", "f2"), bind("git", "f4", "app.about")]).unwrap();
        assert_eq!(
            out,
            "[git]\n# about\nF4 = \"app.about\" # keep\n\"f3\" = \"app.quit\"\n\n# changes view\n[git.changes]\n\"f6\" = \"app.quit\"\n"
        );
    }

    #[test]
    fn keymap_edit_of_a_default_key_writes_none() {
        assert!(default_keymap().table("global").unwrap().bindings.contains_key(&ch("ctrl-q")));
        let input = "[global]\n\"ctrl-q\" = \"app.about\" # x\n";
        let out = apply_keymap_edits(input, &[unbind("global", "ctrl-q"), bind("global", "ctrl-alt-y", "app.about")]).unwrap();
        assert_eq!(out, "[global]\n\"ctrl-q\" = \"none\" # x\n\"Ctrl+Alt+Y\" = \"app.about\"\n");
    }

    #[test]
    fn keymap_edit_onto_an_existing_key_updates_that_key() {
        let input = "[global]\n\"f2\" = \"app.about\"\n\"f4\" = \"app.quit\" # mine\n";
        let out = apply_keymap_edits(input, &[unbind("global", "f2"), bind("global", "f4", "app.about")]).unwrap();
        assert_eq!(out, "[global]\n\"f4\" = \"app.about\" # mine\n");
    }

    #[test]
    fn keymap_edit_same_key_keeps_spelling() {
        let input = "[global]\n\"f2\" = \"app.about\" # k\n\"f3\" = \"app.quit\"\n";
        let out = apply_keymap_edits(input, &[unbind("global", "f2"), bind("global", "f2", "app.quit")]).unwrap();
        assert_eq!(out, "[global]\n\"f2\" = \"app.quit\" # k\n\"f3\" = \"app.quit\"\n");
    }

    #[test]
    fn keymap_edit_in_an_inline_table() {
        let input = "global = { \"f2\" = \"app.about\", \"f3\" = \"app.quit\" }\n";
        let out = apply_keymap_edits(input, &[unbind("global", "f2"), bind("global", "f4", "app.about")]).unwrap();
        let mut k = Keymap::default();
        k.merge_toml(&out).unwrap();
        let g = &k.table("global").unwrap().bindings;
        assert_eq!(g.get(&ch("f4")).and_then(|b| b.id()), Some("app.about"));
        assert!(!g.contains_key(&ch("f2")));
        assert_eq!(g.get(&ch("f3")).and_then(|b| b.id()), Some("app.quit"));
    }

    #[test]
    fn keymap_invalid_result_is_refused() {
        let e = apply_keymap_edits("", &[bind("global", "f2", "no.such")]).unwrap_err();
        assert!(e.contains("unknown action"), "{e}");
        let e = apply_keymap_edits("[global\n", &[]).unwrap_err();
        assert!(e.starts_with("keymap.toml is not valid TOML"), "{e}");
        let e = apply_keymap_edits("global = 3\n", &[bind("global", "f2", "app.about")]).unwrap_err();
        assert!(e.contains("not a table"), "{e}");
        let out = apply_keymap_edits("[global]\n\"f9\" = \"no.such\"\n", &[bind("global", "f2", "app.about")]).unwrap();
        assert!(out.contains("F2 = \"app.about\""));
    }

    #[test]
    fn apply_edit_matches_a_reload_of_the_written_text() {
        let base = "# mine\n[global]\n\"f2\" = \"app.about\"\n\"f6\" = \"app.about\"\n[mode.normal]\n\"x\" = \"editor.undo\"\n";
        let edits = [
            unbind("global", "ctrl-q"),
            unbind("global", "f2"),
            bind("global", "f2", "app.quit"),
            bind("mode.normal", "g h", "editor.left"),
            unbind("mode.normal", "x"),
            unbind("global", "f6"),
            bind("global", "f7", "app.about"),
        ];
        let mut a = default_keymap();
        a.merge_toml(base).unwrap();
        for e in &edits {
            a.apply_edit(e);
        }
        let mut b = default_keymap();
        b.merge_toml(&apply_keymap_edits(base, &edits).unwrap()).unwrap();
        let ctxs: std::collections::BTreeSet<&String> = a.tables.keys().chain(b.tables.keys()).collect();
        let empty = Default::default();
        for c in ctxs {
            let get = |k: &Keymap| k.tables.get(c).map_or(&empty, |t| &t.bindings).clone();
            assert_eq!(get(&a), get(&b), "[{c}]");
        }
    }

    #[test]
    fn write_keymap_missing_file_and_symlink() {
        let dir = test_dir("keymap");
        let path = dir.join("sub").join("keymap.toml");
        let text = write_keymap(&path, &[bind("global", "ctrl-alt-y", "app.quit")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(text.contains("\"Ctrl+Alt+Y\" = \"app.quit\""));
        #[cfg(unix)]
        {
            let target = dir.join("real.toml");
            let link = dir.join("keymap.toml");
            std::fs::write(&target, "[global]\n\"f3\" = \"app.about\"\n").unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            write_keymap(&link, &[unbind("global", "f3")]).unwrap();
            assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "[global]\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
