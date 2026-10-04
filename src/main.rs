use std::path::{Path, PathBuf};

use anyhow::Result;
use jetl::{app::App, config, tui};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "jetl {}\n\nUsage: jetl [PATH[:LINE[:COL]]]\n\nOpens a terminal IDE rooted at PATH (default: current directory). A file\nopens at LINE and COL, rooted at its git repository (else its directory).\nStarted without PATH outside a git repository, it offers the recent projects.\n\nOptions:\n  -h, --help         Show this help\n  -V, --version      Show the version\n      --dump-keymap  Print the default key bindings as Markdown (docs/KEYMAP.md)\n\nConfig: $XDG_CONFIG_HOME/jetl/{{config,keymap,theme}}.toml (see docs/CONFIG.md)\nLog:    $XDG_STATE_HOME/jetl/jetl.log",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--dump-keymap") {
        print!("{}", config::keymap::default_keymap().markdown());
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("jetl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(bad) = args.iter().find(|a| a.starts_with('-') && a.len() > 1) {
        anyhow::bail!("unknown option {bad} (see jetl --help)");
    }
    let arg = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);
    let (root, open) = resolve_target(arg.unwrap_or("."))?;
    // Started bare outside a repository: likely not in a project yet.
    let pick_project = arg.is_none() && git2::Repository::discover(&root).is_err();

    let paths = config::Paths::new()?;
    let _log_guard = config::init_logging(&paths)?;
    let (cfg, cfg_error) = config::Config::load(&paths);

    tui::install_panic_hook();
    let mut terminal = tui::init(&root)?;
    let mut app = App::new(root, cfg, paths, open);
    app.arm_recovery();
    if pick_project {
        app.pick_project_at_startup();
    }
    if let Some(e) = cfg_error {
        app.notify(jetl::event::Level::Error, e);
    }
    let result = app.run(&mut terminal).await;
    tui::restore();
    result
}

/// A file to open with its 1-based line and column.
type OpenAt = (PathBuf, usize, usize);

/// The project root and the file to open for a `PATH[:LINE[:COL]]`
/// argument. A file is rooted at its repository's work tree, else at its
/// directory.
fn resolve_target(arg: &str) -> Result<(PathBuf, Option<OpenAt>)> {
    let (path, line, col) = parse_target(arg, |p| p.exists());
    let path = std::fs::canonicalize(&path).map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
    if path.is_dir() {
        return Ok((path, None));
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    let toplevel = git2::Repository::discover(&parent).ok().and_then(|r| r.workdir().and_then(|w| std::fs::canonicalize(w).ok()));
    let root = toplevel.filter(|t| path.starts_with(t)).unwrap_or(parent);
    Ok((root, Some((path, line.unwrap_or(1), col.unwrap_or(1)))))
}

/// Split `PATH[:LINE[:COL]]`. Trailing numeric parts are stripped only
/// while the argument as given does not exist, so a file named `notes:12`
/// is opened as is.
fn parse_target(arg: &str, exists: impl Fn(&Path) -> bool) -> (PathBuf, Option<usize>, Option<usize>) {
    fn split_num(s: &str) -> Option<(&str, usize)> {
        let (head, tail) = s.rsplit_once(':')?;
        let n = tail.parse().ok().filter(|_| !head.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))?;
        Some((head, n))
    }
    if exists(Path::new(arg)) {
        return (PathBuf::from(arg), None, None);
    }
    let Some((one, a)) = split_num(arg) else { return (PathBuf::from(arg), None, None) };
    match split_num(one) {
        Some((two, l)) if !exists(Path::new(one)) => (PathBuf::from(two), Some(l), Some(a)),
        _ => (PathBuf::from(one), Some(a), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arg: &str, existing: &[&str]) -> (PathBuf, Option<usize>, Option<usize>) {
        parse_target(arg, |p| existing.iter().any(|e| Path::new(e) == p))
    }

    #[test]
    fn parses_line_and_column_suffixes() {
        assert_eq!(parse("src/a.rs", &["src/a.rs"]), (PathBuf::from("src/a.rs"), None, None));
        assert_eq!(parse("src/a.rs:40", &["src/a.rs"]), (PathBuf::from("src/a.rs"), Some(40), None));
        assert_eq!(parse("src/a.rs:40:5", &["src/a.rs"]), (PathBuf::from("src/a.rs"), Some(40), Some(5)));
        // Nothing exists: the suffixes are still taken, so the error names the file.
        assert_eq!(parse("gone.rs:3:1", &[]), (PathBuf::from("gone.rs"), Some(3), Some(1)));
    }

    #[test]
    fn existing_names_with_colons_are_kept() {
        assert_eq!(parse("notes:12", &["notes:12"]), (PathBuf::from("notes:12"), None, None));
        assert_eq!(parse("notes:12:7", &["notes:12"]), (PathBuf::from("notes:12"), Some(7), None));
        assert_eq!(parse("notes:12:7:2", &["notes:12"]), (PathBuf::from("notes:12"), Some(7), Some(2)));
    }

    #[test]
    fn non_numeric_suffixes_are_kept() {
        assert_eq!(parse("a.rs:x", &[]), (PathBuf::from("a.rs:x"), None, None));
        assert_eq!(parse("a.rs:+3", &[]), (PathBuf::from("a.rs:+3"), None, None));
        assert_eq!(parse("a.rs:", &[]), (PathBuf::from("a.rs:"), None, None));
        assert_eq!(parse(":12", &[]), (PathBuf::from(":12"), None, None));
        assert_eq!(parse("a.rs:x:3", &[]), (PathBuf::from("a.rs:x"), Some(3), None));
    }

    #[test]
    fn missing_file_is_an_error_and_a_file_is_rooted_at_its_directory() {
        let dir = std::env::temp_dir().join(format!("jetl-test-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nonexistent.rs");
        let err = resolve_target(&format!("{}:3", missing.display())).unwrap_err().to_string();
        assert!(err.starts_with(&format!("cannot open {}:", missing.display())), "{err}");

        let dir = std::fs::canonicalize(&dir).unwrap();
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let (root, open) = resolve_target(&format!("{}:2:4", file.display())).unwrap();
        let expected = git2::Repository::discover(&dir).ok().and_then(|r| r.workdir().map(|w| std::fs::canonicalize(w).unwrap()));
        assert_eq!(root, expected.unwrap_or_else(|| dir.clone()));
        assert_eq!(open, Some((file, 2, 4)));
        assert_eq!(resolve_target(&dir.display().to_string()).unwrap(), (dir.clone(), None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
