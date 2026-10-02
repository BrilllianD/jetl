//! Recently opened project roots, newest first, stored as JSON in
//! `paths.recent_projects_file()`. The app records its root at startup and
//! on every root switch, and lists the others in the Recent Projects palette.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Entries kept in the file.
pub const MAX: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProject {
    pub root: PathBuf,
    /// Unix time in seconds.
    pub last_opened: u64,
}

/// Serializes read-modify-write of the file between background tasks.
static FILE_LOCK: Mutex<()> = Mutex::new(());

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Move `root` to the front with time `at`, newest first, without
/// duplicates, at most [`MAX`] entries. An older `at` (a late task) sorts
/// behind newer entries.
pub fn touch(list: &mut Vec<RecentProject>, root: &Path, at: u64) {
    list.retain(|p| p.root != root);
    list.insert(0, RecentProject { root: root.to_path_buf(), last_opened: at });
    // Stable: on equal times the entry just touched stays first.
    list.sort_by_key(|p| std::cmp::Reverse(p.last_opened));
    list.truncate(MAX);
}

/// The saved list. A missing file is silent; unreadable or invalid JSON is
/// logged and reads as empty.
pub fn load(file: &Path) -> Vec<RecentProject> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!("recent projects {}: {e}; ignoring", file.display());
            return Vec::new();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!("recent projects {}: invalid JSON ({e}); ignoring", file.display());
        Vec::new()
    })
}

/// Write atomically (temp file, then rename).
pub fn save(file: &Path, list: &[RecentProject]) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(list)?).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, file).with_context(|| format!("rename {}", tmp.display()))?;
    Ok(())
}

/// Blocking: record `root` (when given) at time `at`, and return the roots
/// that still exist as directories, newest first.
pub fn update(file: &Path, root: Option<&Path>, at: u64) -> Vec<PathBuf> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = load(file);
    if let Some(root) = root {
        touch(&mut list, root, at);
        if let Err(e) = save(file, &list) {
            tracing::warn!("saving recent projects: {e:#}");
        }
    }
    list.into_iter().map(|p| p.root).filter(|r| r.is_dir()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(list: &[RecentProject]) -> Vec<&str> {
        list.iter().map(|p| p.root.to_str().unwrap()).collect()
    }

    #[test]
    fn touch_orders_dedups_and_caps() {
        let mut list = Vec::new();
        touch(&mut list, Path::new("/a"), 10);
        touch(&mut list, Path::new("/b"), 20);
        touch(&mut list, Path::new("/c"), 20);
        assert_eq!(roots(&list), ["/c", "/b", "/a"]);
        // Reopening moves to the front, once.
        touch(&mut list, Path::new("/a"), 30);
        assert_eq!(roots(&list), ["/a", "/c", "/b"]);
        assert_eq!(list[0].last_opened, 30);
        // A late task with an older time does not jump ahead.
        touch(&mut list, Path::new("/d"), 15);
        assert_eq!(roots(&list), ["/a", "/c", "/b", "/d"]);
        for i in 0..40 {
            touch(&mut list, &PathBuf::from(format!("/p{i}")), 100 + i);
        }
        assert_eq!(list.len(), MAX);
        assert_eq!(list[0].root, PathBuf::from("/p39"));
        assert_eq!(list[MAX - 1].root, PathBuf::from("/p10"));
    }

    #[test]
    fn missing_or_corrupt_file_reads_empty_and_update_keeps_existing_dirs() {
        let dir = std::env::temp_dir().join(format!("jetl-test-recent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("state/recent_projects.json");
        assert!(load(&file).is_empty());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{not json").unwrap();
        assert!(load(&file).is_empty());

        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(update(&file, Some(&a), 1), vec![a.clone()]);
        assert_eq!(update(&file, Some(&b), 2), [b.clone(), a.clone()]);
        assert_eq!(load(&file).len(), 2);
        // A deleted project stays in the file but is not listed.
        std::fs::remove_dir_all(&a).unwrap();
        assert_eq!(update(&file, None, 3), vec![b.clone()]);
        assert_eq!(load(&file).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
