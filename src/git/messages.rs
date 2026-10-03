//! Commit message history per repository, newest first, stored as JSON in
//! `<state>/commit_messages/<fnv hash of the repository's git dir>.json`.
//! Worktrees of one repository share their git dir, so they share the
//! history. Every call is blocking (run it in `spawn_blocking`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Messages kept per repository.
pub const MAX: usize = 50;

/// Serializes read-modify-write of the files between background tasks.
static FILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Serialize, Deserialize)]
struct History {
    /// The repository's git dir, to detect a hash collision.
    repo: PathBuf,
    messages: Vec<String>,
}

/// The git dir shared by all worktrees of the repository at `root`.
fn repo_key(root: &Path) -> Option<PathBuf> {
    let (repo, _) = super::repo::open(root).ok()?;
    let dir = repo.commondir();
    Some(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
}

fn file_for(dir: &Path, key: &Path) -> PathBuf {
    dir.join(format!("{:016x}.json", crate::workspace::fnv1a64(key.as_os_str().as_encoded_bytes())))
}

fn read(file: &Path, key: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!("commit messages {}: {e}; ignoring", file.display());
            return Vec::new();
        }
    };
    match serde_json::from_str::<History>(&text) {
        Ok(h) if h.repo == key => h.messages,
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::warn!("commit messages {}: invalid JSON ({e}); ignoring", file.display());
            Vec::new()
        }
    }
}

/// Put `message` first, without duplicates, at most [`MAX`] entries.
pub fn push(list: &mut Vec<String>, message: &str) {
    let message = message.trim_end();
    if message.trim().is_empty() {
        return;
    }
    list.retain(|m| m != message);
    list.insert(0, message.to_string());
    list.truncate(MAX);
}

/// The history of the repository at `root`; empty outside a repository.
pub fn load(dir: &Path, root: &Path) -> Vec<String> {
    let Some(key) = repo_key(root) else { return Vec::new() };
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read(&file_for(dir, &key), &key)
}

/// Add a message to the history of the repository at `root`.
pub fn record(dir: &Path, root: &Path, message: &str) -> Result<()> {
    let Some(key) = repo_key(root) else { return Ok(()) };
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = file_for(dir, &key);
    let mut messages = read(&file, &key);
    push(&mut messages, message);
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(&History { repo: key, messages })?;
    crate::util::fs::write_atomic(&file, &json).with_context(|| format!("write {}", file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::repo::tests::TempRepo;

    #[test]
    fn newest_first_without_duplicates_per_repository() {
        let r = TempRepo::new("messages");
        let other = TempRepo::new("messages-other");
        let dir = r.dir.join(".git/jetl-state");
        assert!(load(&dir, &r.dir).is_empty());
        record(&dir, &r.dir, "first\n").unwrap();
        record(&dir, &r.dir, "second").unwrap();
        record(&dir, &r.dir, "first").unwrap();
        record(&dir, &r.dir, "  ").unwrap();
        assert_eq!(load(&dir, &r.dir), ["first", "second"]);
        // A subdirectory is the same repository; another one is not.
        std::fs::create_dir_all(r.dir.join("sub")).unwrap();
        assert_eq!(load(&dir, &r.dir.join("sub")).len(), 2);
        assert!(load(&dir, &other.dir).is_empty());
        let mut list = Vec::new();
        for i in 0..MAX + 5 {
            push(&mut list, &i.to_string());
        }
        assert_eq!((list.len(), list[0].as_str()), (MAX, "54"));
    }
}
