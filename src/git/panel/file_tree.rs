//! The Log details' commit files as a folder tree, built from paths only
//! (no disk reads).

use std::collections::BTreeSet;

use crate::git::model::CommitFile;

/// One row of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TreeNode {
    /// The work tree; always open.
    Root,
    /// A folder (the last one of a compacted chain); `path` is relative to the work tree.
    Dir { path: String, open: bool },
    /// An index into the `files` slice the tree was built from.
    File(usize),
}

#[derive(Debug, Clone, Default)]
struct Node {
    path: String,
    label: String,
    dirs: Vec<Node>,
    files: Vec<usize>,
}

impl Node {
    fn compact(&mut self) {
        for d in &mut self.dirs {
            while d.files.is_empty() && d.dirs.len() == 1 {
                let child = d.dirs.remove(0);
                d.label = format!("{}/{}", d.label, child.label);
                d.path = child.path;
                d.dirs = child.dirs;
                d.files = child.files;
            }
            d.compact();
        }
    }

    fn sort(&mut self, names: &[String], paths: &[&str]) {
        self.dirs.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.path.cmp(&b.path)));
        self.files.sort_by(|&a, &b| names[a].cmp(&names[b]).then_with(|| paths[a].cmp(paths[b])));
        for d in &mut self.dirs {
            d.sort(names, paths);
        }
    }

    fn collect_paths(&self, out: &mut BTreeSet<String>) {
        for d in &self.dirs {
            out.insert(d.path.clone());
            d.collect_paths(out);
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct FileTree {
    root: String,
    top: Node,
    closed: BTreeSet<String>,
    rows: Vec<(u16, TreeNode)>,
    labels: Vec<String>,
    files_names: Vec<String>,
}

impl FileTree {
    /// The tree of `files`, every folder open. Empty `files` give no rows.
    pub(super) fn build(root: &str, files: &[CommitFile]) -> FileTree {
        let mut top = Node::default();
        let mut files_names = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            let mut segs: Vec<&str> = f.path.split('/').collect();
            let name = segs.pop().unwrap_or_default();
            files_names.push(name.to_string());
            let mut node = &mut top;
            for seg in segs {
                let path = if node.path.is_empty() { seg.to_string() } else { format!("{}/{seg}", node.path) };
                let at = match node.dirs.iter().position(|d| d.path == path) {
                    Some(at) => at,
                    None => {
                        node.dirs.push(Node { path, label: seg.to_string(), ..Default::default() });
                        node.dirs.len() - 1
                    }
                };
                node = &mut node.dirs[at];
            }
            node.files.push(i);
        }
        top.compact();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        top.sort(&files_names, &paths);
        let mut t = FileTree { root: root.to_string(), top, files_names, ..Default::default() };
        t.flatten();
        t
    }

    pub(super) fn rows(&self) -> &[(u16, TreeNode)] {
        &self.rows
    }

    /// The text of row `row`: the root name, a folder's compacted label or a file name.
    pub(super) fn label(&self, row: usize) -> &str {
        self.labels.get(row).map_or("", String::as_str)
    }

    /// Open or close the folder on `row`; the root and files do nothing.
    pub(super) fn toggle(&mut self, row: usize) {
        let Some((_, TreeNode::Dir { path, open })) = self.rows.get(row) else { return };
        if *open {
            self.closed.insert(path.clone());
        } else {
            self.closed.remove(path);
        }
        self.flatten();
    }

    /// Open every folder, or close every folder (the root stays open).
    pub(super) fn set_all(&mut self, open: bool) {
        self.closed.clear();
        if !open {
            let mut all = BTreeSet::new();
            self.top.collect_paths(&mut all);
            self.closed = all;
        }
        self.flatten();
    }

    /// Take over the closed folders of `old` that exist in this tree.
    pub(super) fn keep_state(&mut self, old: &FileTree) {
        let mut all = BTreeSet::new();
        self.top.collect_paths(&mut all);
        self.closed = old.closed.iter().filter(|p| all.contains(*p)).cloned().collect();
        self.flatten();
    }

    fn flatten(&mut self) {
        self.rows.clear();
        self.labels.clear();
        if self.files_names.is_empty() {
            return;
        }
        self.rows.push((0, TreeNode::Root));
        self.labels.push(self.root.clone());
        let top = std::mem::take(&mut self.top);
        self.walk(&top, 1);
        self.top = top;
    }

    fn walk(&mut self, node: &Node, depth: u16) {
        for d in &node.dirs {
            let open = !self.closed.contains(&d.path);
            self.rows.push((depth, TreeNode::Dir { path: d.path.clone(), open }));
            self.labels.push(d.label.clone());
            if open {
                self.walk(d, depth + 1);
            }
        }
        for &i in &node.files {
            self.rows.push((depth, TreeNode::File(i)));
            self.labels.push(self.files_names[i].clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(status: char, path: &str, old: Option<&str>) -> CommitFile {
        CommitFile { status, path: path.to_string(), old: old.map(str::to_string) }
    }

    fn sample() -> Vec<CommitFile> {
        vec![
            f('M', "src/git/panel/log.rs", None),
            f('M', "src/git/panel/mod.rs", None),
            f('M', "src/main.rs", None),
            f('A', "README.md", None),
            f('A', "docs/a.md", None),
        ]
    }

    fn dir(path: &str, open: bool) -> TreeNode {
        TreeNode::Dir { path: path.to_string(), open }
    }

    fn shape(t: &FileTree) -> Vec<(u16, TreeNode, String)> {
        t.rows().iter().enumerate().map(|(i, (d, n))| (*d, n.clone(), t.label(i).to_string())).collect()
    }

    #[test]
    fn builds_folders_first_sorted_and_compacted() {
        let t = FileTree::build("root", &sample());
        let want = vec![
            (0, TreeNode::Root, "root"),
            (1, dir("docs", true), "docs"),
            (2, TreeNode::File(4), "a.md"),
            (1, dir("src", true), "src"),
            (2, dir("src/git/panel", true), "git/panel"),
            (3, TreeNode::File(0), "log.rs"),
            (3, TreeNode::File(1), "mod.rs"),
            (2, TreeNode::File(2), "main.rs"),
            (1, TreeNode::File(3), "README.md"),
        ];
        let want: Vec<_> = want.into_iter().map(|(d, n, l)| (d, n, l.to_string())).collect();
        assert_eq!(shape(&t), want);
    }

    #[test]
    fn compacts_a_top_level_chain() {
        let t = FileTree::build("root", &[f('M', "a/b/c/x.rs", None)]);
        let want = vec![
            (0, TreeNode::Root, "root".to_string()),
            (1, dir("a/b/c", true), "a/b/c".to_string()),
            (2, TreeNode::File(0), "x.rs".to_string()),
        ];
        assert_eq!(shape(&t), want);
    }

    #[test]
    fn toggle_hides_and_shows_children() {
        let mut t = FileTree::build("root", &sample());
        let full = shape(&t);
        t.toggle(3);
        let nodes: Vec<_> = t.rows().iter().map(|(_, n)| n.clone()).collect();
        assert_eq!(nodes, [TreeNode::Root, dir("docs", true), TreeNode::File(4), dir("src", false), TreeNode::File(3)]);
        t.toggle(3);
        assert_eq!(shape(&t), full);
        t.toggle(0);
        t.toggle(2);
        t.toggle(99);
        assert_eq!(shape(&t), full);
    }

    #[test]
    fn set_all_closes_and_opens_every_folder() {
        let mut t = FileTree::build("root", &sample());
        let full = shape(&t);
        t.set_all(false);
        let nodes: Vec<_> = t.rows().iter().map(|(_, n)| n.clone()).collect();
        assert_eq!(nodes, [TreeNode::Root, dir("docs", false), dir("src", false), TreeNode::File(3)]);
        t.set_all(true);
        assert_eq!(shape(&t), full);
    }

    #[test]
    fn keep_state_carries_closed_folders() {
        let mut old = FileTree::build("root", &sample());
        old.toggle(1);
        old.toggle(old.rows().iter().position(|(_, n)| *n == dir("src", true)).unwrap());
        let mut t = FileTree::build("root", &sample());
        t.keep_state(&old);
        assert!(t.rows().iter().any(|(_, n)| *n == dir("docs", false)));
        assert!(t.rows().iter().any(|(_, n)| *n == dir("src", false)));
        let mut t = FileTree::build("root", &[f('M', "docs/a.md", None)]);
        t.keep_state(&old);
        assert_eq!(t.closed.iter().cloned().collect::<Vec<_>>(), ["docs"]);
    }

    #[test]
    fn rename_sits_under_its_new_path() {
        let t = FileTree::build("root", &[f('R', "docs/syntax.md", Some("old/grammar.md"))]);
        let want = vec![
            (0, TreeNode::Root, "root".to_string()),
            (1, dir("docs", true), "docs".to_string()),
            (2, TreeNode::File(0), "syntax.md".to_string()),
        ];
        assert_eq!(shape(&t), want);
    }

    #[test]
    fn no_files_no_rows() {
        let t = FileTree::build("root", &[]);
        assert!(t.rows().is_empty());
        assert_eq!(t.label(0), "");
    }
}
