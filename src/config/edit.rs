//! Writing settings back into `config.toml`.
//!
//! Edits go through `toml_edit`, so the user's comments, key order and
//! spacing survive. The whole result is validated with `Config::parse`
//! before anything is written, and the file is replaced atomically.

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item};

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

/// `text` with `edits` applied (key as `section.field`), or why it cannot be.
pub fn apply_edits(text: &str, edits: &[(&str, Edit)]) -> Result<String, String> {
    let mut doc: DocumentMut =
        text.parse().map_err(|e: toml_edit::TomlError| format!("config.toml is not valid TOML: {}", error_summary(&e.to_string())))?;
    for (key, edit) in edits {
        let (section, field) = key.split_once('.').ok_or_else(|| format!("bad key {key}"))?;
        match edit {
            Edit::Set(v) => {
                if doc.get(section).is_none() {
                    let mut t = toml_edit::table();
                    // A document of only comments keeps them in `trailing`, which
                    // renders after every table: move them before the new one.
                    let comments =
                        if doc.as_table().is_empty() { doc.trailing().as_str().unwrap_or("").to_string() } else { String::new() };
                    if !comments.is_empty() {
                        doc.set_trailing("");
                    }
                    let has_content = !doc.to_string().trim().is_empty() || !comments.trim().is_empty();
                    if has_content && let Some(t) = t.as_table_mut() {
                        t.decor_mut().set_prefix(format!("{comments}\n"));
                    }
                    doc.insert(section, t);
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
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let target = if is_link { std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))? } else { path.to_path_buf() };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    match crate::util::fs::write_atomic(&target, &text) {
        Ok(Some(perm_err)) => tracing::warn!("config.toml permissions not kept: {perm_err}"),
        Ok(None) => {}
        Err(e) => return Err(format!("write {}: {e}", target.display())),
    }
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
}
