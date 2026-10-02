//! File operations started from the Project tree. They run off the main
//! thread (`FsOp::run`); the app applies the outcome in `fs_op_done`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsOp {
    Create { path: PathBuf, is_dir: bool },
    Rename { from: PathBuf, to: PathBuf },
    Delete { path: PathBuf },
}

impl FsOp {
    pub fn run(&self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        let exists = |p: &Path| p.symlink_metadata().is_ok();
        match self {
            FsOp::Create { path, is_dir } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if *is_dir {
                    std::fs::create_dir(path)
                } else {
                    std::fs::OpenOptions::new().write(true).create_new(true).open(path).map(drop)
                }
            }
            FsOp::Rename { from, to } => {
                // rename(2) replaces an existing file silently.
                if exists(to) {
                    return Err(Error::new(ErrorKind::AlreadyExists, format!("{} already exists", to.display())));
                }
                std::fs::rename(from, to)
            }
            FsOp::Delete { path } => {
                // A symlink to a directory is removed as a link, not followed.
                if path.symlink_metadata()?.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }
            }
        }
    }

    /// Short description for messages: "create src/a.rs".
    pub fn describe(&self, root: &Path) -> String {
        let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        match self {
            FsOp::Create { path, .. } => format!("create {}", rel(path)),
            FsOp::Rename { from, to } => format!("rename {} to {}", rel(from), rel(to)),
            FsOp::Delete { path } => format!("delete {}", rel(path)),
        }
    }
}

/// Check a name typed in the New or Rename dialog. New entries may name
/// subdirectories (`a/b.rs`); a rename stays in its directory.
pub fn check_name(name: &str, allow_subdirs: bool) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name is empty".into());
    }
    if name.starts_with('/') || name.contains('\0') {
        return Err(format!("invalid name {name:?}"));
    }
    if !allow_subdirs && name.contains('/') {
        return Err("a rename cannot move to another directory".into());
    }
    if name.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(format!("invalid name {name:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        assert!(check_name("a.rs", false).is_ok());
        assert!(check_name("src/a.rs", true).is_ok());
        assert!(check_name("src/a.rs", false).is_err());
        for bad in ["", "  ", "/abs", "a/../b", "a//b", ".", "x/"] {
            assert!(check_name(bad, true).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn create_rename_delete() {
        let dir = std::env::temp_dir().join(format!("jetl-test-ops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("sub/new.txt");
        FsOp::Create { path: f.clone(), is_dir: false }.run().unwrap();
        assert!(f.is_file());
        assert!(FsOp::Create { path: f.clone(), is_dir: false }.run().is_err(), "never overwrites");
        std::fs::write(dir.join("taken.txt"), "keep").unwrap();
        assert!(FsOp::Rename { from: f.clone(), to: dir.join("taken.txt") }.run().is_err(), "never replaces");
        assert_eq!(std::fs::read_to_string(dir.join("taken.txt")).unwrap(), "keep");
        FsOp::Rename { from: dir.join("sub"), to: dir.join("moved") }.run().unwrap();
        assert!(dir.join("moved/new.txt").is_file());
        FsOp::Delete { path: dir.join("moved") }.run().unwrap();
        assert!(!dir.join("moved").exists());
        assert_eq!(FsOp::Delete { path: dir.join("x") }.describe(&dir), "delete x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
