//! File helpers.

use std::path::{Component, Path, PathBuf};

/// Write via a temp file in the same directory, then rename over the target.
/// The written file keeps the original's permissions; failing to copy them
/// does not fail the save and comes back as `Ok(Some(error))`.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<Option<std::io::Error>> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.jetl-tmp", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Preserve permissions of the original file when it exists.
    let perm_err = std::fs::metadata(path).ok().and_then(|meta| std::fs::set_permissions(&tmp, meta.permissions()).err());
    std::fs::rename(&tmp, path)?;
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
