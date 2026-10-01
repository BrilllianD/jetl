pub mod panel;
pub use panel::WorktreePanel;

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: std::path::PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
    pub is_main: bool,
}
