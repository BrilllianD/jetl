//! File helpers.

use std::path::Path;

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
