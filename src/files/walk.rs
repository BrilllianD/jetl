//! The directory walk shared by the tree, Go to File and find in files:
//! `.gitignore`-aware (inside a git repository), hidden files per config,
//! symlinks not followed, `.git` skipped.

use std::path::Path;

pub fn builder(root: &Path, show_hidden: bool, show_ignored: bool) -> ignore::WalkBuilder {
    let mut wb = ignore::WalkBuilder::new(root);
    wb.hidden(!show_hidden)
        .git_ignore(!show_ignored)
        .git_global(!show_ignored)
        .git_exclude(!show_ignored)
        .follow_links(false)
        .require_git(true)
        .filter_entry(|e| e.file_name() != ".git");
    wb
}
