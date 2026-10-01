use std::path::PathBuf;

use anyhow::Result;
use jetl::{app::App, config, tui};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "jetl {}\n\nUsage: jetl [PATH]\n\nOpens a terminal IDE rooted at PATH (default: current directory).\n\nOptions:\n  -h, --help         Show this help\n  -V, --version      Show the version\n      --dump-keymap  Print the default key bindings as Markdown (docs/KEYMAP.md)\n\nConfig: $XDG_CONFIG_HOME/jetl/{{config,keymap,theme}}.toml (see docs/CONFIG.md)\nLog:    $XDG_STATE_HOME/jetl/jetl.log",
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
    let root = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = std::fs::canonicalize(&root)
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", root.display()))?;

    let paths = config::Paths::new()?;
    let _log_guard = config::init_logging(&paths)?;
    let (cfg, cfg_error) = config::Config::load(&paths);

    tui::install_panic_hook();
    let mut terminal = tui::init(&root)?;
    let mut app = App::new(root, cfg, paths);
    if let Some(e) = cfg_error {
        app.notify(jetl::event::Level::Error, e);
    }
    let result = app.run(&mut terminal).await;
    tui::restore();
    result
}
