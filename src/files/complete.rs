//! Shell-like completion of a typed path (the Save As prompt's Tab): the
//! entries of one directory, read on the main thread (see ARCHITECTURE).

use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Completion {
    /// Goes in at the cursor: the rest of the only match (a directory with
    /// its `/`), or of the matches' longest common prefix.
    pub insert: String,
    /// Every match, sorted, directories ending in `/`; empty: nothing matched.
    pub matches: Vec<String>,
}

/// Complete `typed` (the text before the cursor): relative to `base`, `~`
/// through `home`, absolute as is, `.`/`..` applied as `App::resolve_save_as`
/// does. A missing directory matches nothing; another read error is returned.
///
/// `folders_only` leaves files out of the insert, the matches and the count
/// (the worktree path prompt).
///
/// `.` and `..` are never offered (`read_dir` leaves them out): `..` then
/// Tab matches only a `..foo` entry, `../` then Tab lists the parent.
pub fn complete(typed: &str, base: &Path, home: Option<&Path>, folders_only: bool) -> std::io::Result<Completion> {
    if typed == "~" {
        return Ok(match home {
            Some(h) if h.is_dir() => Completion { insert: "/".into(), matches: vec!["~/".into()] },
            _ => Completion::default(),
        });
    }
    let (dir_part, prefix) = match typed.rfind('/') {
        Some(i) => typed.split_at(i + 1),
        None => ("", typed),
    };
    let dir = if dir_part.is_empty() {
        base.to_path_buf()
    } else {
        crate::util::fs::normalize(&base.join(crate::worktree::expand_tilde_in(dir_part, home)))
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return Ok(Completion::default()),
        Err(e) => return Err(e),
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        // Bytes first: only matching names pay for a String and a type.
        if !name.as_encoded_bytes().starts_with(prefix.as_bytes()) {
            continue;
        }
        let Ok(name) = name.into_string() else { continue };
        if prefix.is_empty() && name.starts_with('.') {
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(t) if t.is_symlink() => entry.path().is_dir(),
            Ok(t) => t.is_dir(),
            Err(_) => false,
        };
        if folders_only && !is_dir {
            continue;
        }
        matches.push(if is_dir { format!("{name}/") } else { name });
    }
    matches.sort();
    let insert = match matches.as_slice() {
        [] => String::new(),
        [m] => m[prefix.len()..].to_string(),
        _ => common_prefix(&matches)[prefix.len()..].to_string(),
    };
    Ok(Completion { insert, matches })
}

/// The longest prefix all of `names` share, whole chars only.
fn common_prefix(names: &[String]) -> &str {
    let Some(first) = names.first() else { return "" };
    let mut end = first.len();
    for n in &names[1..] {
        // No differing char: the shorter one is a prefix of the other, so
        // `n.len()` is a char boundary of `first` too.
        end = first[..end].char_indices().zip(n.chars()).find(|((_, a), b)| a != b).map_or(end.min(n.len()), |((i, _), _)| i);
    }
    &first[..end]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-complete-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    fn c(insert: &str, matches: &[&str]) -> Completion {
        Completion { insert: insert.into(), matches: matches.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn completes_names_in_the_base() {
        let d = tmp("base");
        touch(&d, "README.md");
        touch(&d, "notes.txt");
        touch(&d, "notes.md");
        std::fs::create_dir_all(d.join("notebook")).unwrap();
        touch(&d, "src/main.rs");
        touch(&d, "src/lib.rs");
        let run = |t: &str| complete(t, &d, None, false).unwrap();
        assert_eq!(run("REA"), c("DME.md", &["README.md"]));
        assert_eq!(run("sr"), c("c/", &["src/"]), "a directory gets its slash");
        assert_eq!(run("no"), c("te", &["notebook/", "notes.md", "notes.txt"]), "the common prefix");
        assert_eq!(run("notes."), c("", &["notes.md", "notes.txt"]), "nothing more to add");
        assert_eq!(run("zz"), Completion::default());
        assert_eq!(run("nope/x"), Completion::default(), "a missing directory");
        assert_eq!(run("README.md/x"), Completion::default(), "a file as a directory");
        assert_eq!(run("readme"), Completion::default(), "case-sensitive");
        assert_eq!(run("src/ma"), c("in.rs", &["main.rs"]));
        assert_eq!(run("src/"), c("", &["lib.rs", "main.rs"]));
        let abs = format!("{}/sr", d.display());
        assert_eq!(run(&abs), c("c/", &["src/"]), "absolute");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn folders_only_skips_files() {
        let d = tmp("folders");
        touch(&d, "notes.txt");
        std::fs::create_dir_all(d.join("notebook")).unwrap();
        std::fs::create_dir_all(d.join("nothing")).unwrap();
        assert_eq!(complete("no", &d, None, true).unwrap(), c("t", &["notebook/", "nothing/"]));
        assert_eq!(complete("notes", &d, None, true).unwrap(), Completion::default());
        assert_eq!(complete("no", &d, None, false).unwrap(), c("t", &["notebook/", "notes.txt", "nothing/"]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hidden_entries_need_a_dot() {
        let d = tmp("hidden");
        touch(&d, ".env");
        std::fs::create_dir_all(d.join(".git")).unwrap();
        touch(&d, "a.txt");
        assert_eq!(complete("", &d, None, false).unwrap(), c("a.txt", &["a.txt"]));
        assert_eq!(complete(".", &d, None, false).unwrap(), c("", &[".env", ".git/"]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn parent_folders_resolve_like_save_as() {
        let d = tmp("parent");
        touch(&d, "notes.txt");
        touch(&d, "notes.md");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        assert_eq!(complete("../no", &d.join("sub"), None, false).unwrap(), c("tes.", &["notes.md", "notes.txt"]));
        assert_eq!(complete("./x/../no", &d, None, false).unwrap(), c("tes.", &["notes.md", "notes.txt"]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tilde_goes_through_home() {
        let d = tmp("tilde");
        let home = d.join("home");
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        std::fs::create_dir_all(d.join("base")).unwrap();
        let base = d.join("base");
        assert_eq!(complete("~/Doc", &base, Some(&home), false).unwrap(), c("uments/", &["Documents/"]));
        assert_eq!(complete("~", &base, Some(&home), false).unwrap(), c("/", &["~/"]));
        assert_eq!(complete("~", &base, None, false).unwrap(), Completion::default());
        assert_eq!(complete("~/Doc", &base, None, false).unwrap(), Completion::default(), "a relative ~ folder");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn common_prefix_keeps_whole_chars() {
        let d = tmp("unicode");
        touch(&d, "été.txt");
        touch(&d, "étude.txt");
        assert_eq!(complete("é", &d, None, false).unwrap(), c("t", &["étude.txt", "été.txt"]));
        let names = vec!["éa".to_string(), "éb".to_string()];
        assert_eq!(common_prefix(&names), "é");
        let names = vec!["ab".to_string(), "a".to_string()];
        assert_eq!(common_prefix(&names), "a");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_odd_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let d = tmp("links");
        std::fs::create_dir_all(d.join("real")).unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        std::os::unix::fs::symlink(d.join("gone"), d.join("dangling")).unwrap();
        std::fs::write(d.join(OsStr::from_bytes(b"f\xff")), "").unwrap();
        assert_eq!(complete("li", &d, None, false).unwrap(), c("nk/", &["link/"]));
        assert_eq!(complete("da", &d, None, false).unwrap(), c("ngling", &["dangling"]));
        assert_eq!(complete("f", &d, None, false).unwrap(), Completion::default(), "not UTF-8: skipped");
        let _ = std::fs::remove_dir_all(&d);
    }
}
