//! File operations started from the Project tree and from language servers'
//! workspace edits. They run off the main thread (`FsOp::run`); the app
//! applies the outcome in `fs_op_done` (the tree) or `edits_applied` (a
//! workspace edit). The tree never overwrites and deletes recursively.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsOp {
    /// `overwrite`: an existing file is emptied instead of refused.
    Create { path: PathBuf, is_dir: bool, overwrite: bool },
    /// `overwrite`: an existing file at `to` is replaced. Missing parent
    /// folders of `to` are made.
    Rename { from: PathBuf, to: PathBuf, overwrite: bool },
    /// `recursive`: a folder goes with everything in it; else only an
    /// empty folder can be deleted.
    Delete { path: PathBuf, recursive: bool },
}

impl FsOp {
    pub fn run(&self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        let exists = |p: &Path| p.symlink_metadata().is_ok();
        match self {
            FsOp::Create { path, is_dir, overwrite } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if *is_dir {
                    std::fs::create_dir(path)
                } else if *overwrite {
                    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path).map(drop)
                } else {
                    std::fs::OpenOptions::new().write(true).create_new(true).open(path).map(drop)
                }
            }
            FsOp::Rename { from, to, overwrite } => {
                // rename(2) replaces an existing file silently.
                if !overwrite && exists(to) {
                    return Err(Error::new(ErrorKind::AlreadyExists, format!("{} already exists", to.display())));
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(from, to)
            }
            FsOp::Delete { path, recursive } => {
                // A symlink to a directory is removed as a link, not followed.
                match (path.symlink_metadata()?.is_dir(), recursive) {
                    (true, true) => std::fs::remove_dir_all(path),
                    (true, false) => std::fs::remove_dir(path),
                    (false, _) => std::fs::remove_file(path),
                }
            }
        }
    }

    /// Short description for messages: "create src/a.rs".
    pub fn describe(&self, root: &Path) -> String {
        let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        match self {
            FsOp::Create { path, .. } => format!("create {}", rel(path)),
            FsOp::Rename { from, to, .. } => format!("rename {} to {}", rel(from), rel(to)),
            FsOp::Delete { path, .. } => format!("delete {}", rel(path)),
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
        let create = |overwrite| FsOp::Create { path: f.clone(), is_dir: false, overwrite };
        create(false).run().unwrap();
        assert!(f.is_file());
        assert!(create(false).run().is_err(), "never overwrites");
        std::fs::write(&f, "text").unwrap();
        create(true).run().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "", "overwrite empties it");
        std::fs::write(dir.join("taken.txt"), "keep").unwrap();
        let rename = |from: &Path, to: &Path, overwrite| FsOp::Rename { from: from.into(), to: to.into(), overwrite };
        assert!(rename(&f, &dir.join("taken.txt"), false).run().is_err(), "never replaces");
        assert_eq!(std::fs::read_to_string(dir.join("taken.txt")).unwrap(), "keep");
        std::fs::write(dir.join("other.txt"), "other").unwrap();
        rename(&dir.join("other.txt"), &dir.join("taken.txt"), true).run().unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("taken.txt")).unwrap(), "other", "overwrite replaces it");
        rename(&dir.join("taken.txt"), &dir.join("new/sub/x.txt"), false).run().unwrap();
        assert!(dir.join("new/sub/x.txt").is_file(), "parents made");
        assert!(FsOp::Delete { path: dir.join("new"), recursive: false }.run().is_err(), "not empty");
        std::fs::create_dir(dir.join("empty")).unwrap();
        FsOp::Delete { path: dir.join("empty"), recursive: false }.run().unwrap();
        assert!(!dir.join("empty").exists());
        rename(&dir.join("sub"), &dir.join("moved"), false).run().unwrap();
        assert!(dir.join("moved/new.txt").is_file());
        FsOp::Delete { path: dir.join("moved"), recursive: true }.run().unwrap();
        assert!(!dir.join("moved").exists());
        assert_eq!(FsOp::Delete { path: dir.join("x"), recursive: true }.describe(&dir), "delete x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
