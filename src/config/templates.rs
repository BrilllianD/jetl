//! File templates for Project tree > New > From Template.
//!
//! A template is a regular file directly in `~/.config/jetl/templates/` or
//! in `<root>/.jetl/templates/` (no recursion; a name ending in `.jetl-tmp`
//! is skipped). A project template overrides a global one with the same file
//! name. The body is snippet syntax, as in the `snippets/*.toml` bodies (see
//! [`super::snippets`]): tabstops, placeholders, choices and variables such as
//! `$TM_FILENAME_BASE`; a literal `$`, `}` or `\` is written `\$`, `\}`, `\\`.
//! A body must be UTF-8 and at most [`MAX_BYTES`] long. The new file's name is
//! the typed one, plus the template's extension when the typed name has none
//! ([`file_name`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Global template directory under the config dir.
pub const TEMPLATES_DIR: &str = "templates";
/// Project template directory under the root.
pub const PROJECT_DIR: &str = ".jetl/templates";
/// Largest body read.
pub const MAX_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The file name, verbatim.
    pub name: String,
    pub path: PathBuf,
    /// From `<root>/.jetl/templates/`.
    pub project: bool,
}

impl Template {
    /// The chooser row: the name, plus "  (project)" for a project template.
    pub fn row(&self) -> String {
        if self.project { format!("{}  (project)", self.name) } else { self.name.clone() }
    }
}

/// `<root>/.jetl/templates`.
pub fn project_dir(root: &Path) -> PathBuf {
    root.join(PROJECT_DIR)
}

/// Both dirs merged, project over global by name, sorted by name.
pub fn list(global: &Path, project: &Path) -> Vec<Template> {
    let mut by_name: BTreeMap<String, Template> = BTreeMap::new();
    for (dir, is_project) in [(global, false), (project, true)] {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!("templates dir {}: {e}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
            if name.ends_with(".jetl-tmp") || !path.is_file() {
                continue;
            }
            by_name.insert(name.clone(), Template { name, path, project: is_project });
        }
    }
    by_name.into_values().collect()
}

/// The body: UTF-8, at most [`MAX_BYTES`]; Err is the message tail.
pub fn read_body(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_BYTES {
        return Err("the file is over 1 MiB".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|_| "the file is not UTF-8".to_string())
}

/// The created file's name for `typed`: trimmed, with the template's
/// extension added when the last component has none.
pub fn file_name(typed: &str, template: &str) -> String {
    let typed = typed.trim();
    let last = typed.rsplit('/').next().unwrap_or(typed);
    match Path::new(template).extension().and_then(|e| e.to_str()) {
        Some(ext) if Path::new(last).extension().is_none() => format!("{typed}.{ext}"),
        _ => typed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-templates-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn list_merges_project_over_global() {
        let d = tmp("merge");
        let (g, p) = (d.join("g"), d.join("p"));
        std::fs::create_dir_all(&g).unwrap();
        std::fs::create_dir_all(p.join("sub")).unwrap();
        std::fs::write(g.join("a.rs"), "a").unwrap();
        std::fs::write(g.join("b.txt"), "g").unwrap();
        std::fs::write(p.join("b.txt"), "p").unwrap();
        std::fs::write(p.join("c.md"), "c").unwrap();
        std::fs::write(p.join("sub/x"), "x").unwrap();
        std::fs::write(p.join(".x.jetl-tmp"), "x").unwrap();
        let l = list(&g, &p);
        let names: Vec<&str> = l.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a.rs", "b.txt", "c.md"]);
        assert!(!l[0].project);
        assert!(l[1].project);
        assert_eq!(l[1].path, p.join("b.txt"));
        assert!(l[2].project);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn list_of_missing_dirs_is_empty() {
        let d = tmp("missing");
        assert!(list(&d.join("nope"), &d.join("nope2")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_name_adds_the_extension() {
        assert_eq!(file_name("foo", "test.rs"), "foo.rs");
        assert_eq!(file_name(" foo ", "test.rs"), "foo.rs");
        assert_eq!(file_name("foo.txt", "test.rs"), "foo.txt");
        assert_eq!(file_name("src/foo", "test.rs"), "src/foo.rs");
        assert_eq!(file_name("a.b/c", "t.rs"), "a.b/c.rs");
        assert_eq!(file_name("x", "Makefile"), "x");
        assert_eq!(file_name("x", ".gitignore"), "x");
    }

    #[test]
    fn read_body_checks_size_and_encoding() {
        let d = tmp("body");
        std::fs::write(d.join("big"), vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        std::fs::write(d.join("bad"), [b'a', 0xff]).unwrap();
        std::fs::write(d.join("ok"), "fine $1").unwrap();
        assert_eq!(read_body(&d.join("big")), Err("the file is over 1 MiB".into()));
        assert_eq!(read_body(&d.join("bad")), Err("the file is not UTF-8".into()));
        assert_eq!(read_body(&d.join("ok")), Ok("fine $1".into()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn row_marks_project_templates() {
        let t = |project| Template { name: "t.rs".into(), path: PathBuf::from("t.rs"), project };
        assert_eq!(t(false).row(), "t.rs");
        assert_eq!(t(true).row(), "t.rs  (project)");
    }
}
