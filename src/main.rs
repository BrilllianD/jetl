use std::path::PathBuf;

use anyhow::Result;
use jetl::{app::App, config, tui};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("jetl {}\n\nUsage: jetl [PATH]\n\nOpens a terminal IDE rooted at PATH (default: current directory).", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("jetl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let root = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = std::fs::canonicalize(&root)
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", root.display()))?;

    let paths = config::Paths::new()?;
    let _log_guard = config::init_logging(&paths)?;
    let cfg = config::Config::load(&paths);

    tui::install_panic_hook();
    let mut terminal = tui::init(&root)?;
    let mut app = App::new(root, cfg, paths);
    let result = app.run(&mut terminal).await;
    tui::restore();
    result
}
