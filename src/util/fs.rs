//! File helpers.

use std::path::{Component, Path, PathBuf};

/// Write via a temp file in the same directory, then rename over the target.
/// The written file keeps the original's permissions; failing to copy them
/// does not fail the save and comes back as `Ok(Some(error))`.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<Option<std::io::Error>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    // Unique per call, so overlapping writes to one path never share a temp file.
    let tmp = dir.join(format!(
        ".{}.{}.{}.jetl-tmp",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let res = write_and_rename(&tmp, path, text);
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn write_and_rename(tmp: &Path, path: &Path, text: &str) -> std::io::Result<Option<std::io::Error>> {
    use std::io::Write;
    {
        let mut f = std::fs::File::create(tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Preserve permissions of the original file when it exists.
    let perm_err = std::fs::metadata(path).ok().and_then(|meta| std::fs::set_permissions(tmp, meta.permissions()).err());
    std::fs::rename(tmp, path)?;
    Ok(perm_err)
}

/// `p` without `.` components and with `..` applied, so a file reached as
/// `./src/a.rs` opens the same tab as `src/a.rs`.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// A regular file with an execute bit (unix); a regular file elsewhere.
pub fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// The first executable file named `name` in a `PATH` directory.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_atomic_writes_never_corrupt_the_target() {
        let dir = std::env::temp_dir().join(format!("jetl-fs-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("data.json");
        let a = "a".repeat(5000);
        let b = "bb".repeat(40);
        for _ in 0..200 {
            std::thread::scope(|s| {
                s.spawn(|| write_atomic(&target, &a).unwrap());
                s.spawn(|| write_atomic(&target, &b).unwrap());
            });
            let got = std::fs::read_to_string(&target).unwrap();
            assert!(got == a || got == b, "torn write, len {}", got.len());
        }
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".jetl-tmp"))
            .collect();
        assert!(left.is_empty(), "temp file left: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_write_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("jetl-fs-fail-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("target")).unwrap();
        // Renaming a file over a directory fails.
        assert!(write_atomic(&dir.join("target"), "x").is_err());
        let left =
            std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".jetl-tmp")).count();
        assert_eq!(left, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("jetl-no-such-tool-xyz").is_none());
    }
}
