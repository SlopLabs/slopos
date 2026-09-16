//! The file-tree model: what a sidebar shows, minus the filesystem.
//!
//! Nothing here opens a directory. The app reads one and hands the entries back
//! through [`FileTree::populate`], which is what keeps the expand/collapse
//! logic, the ordering and the flattened row list testable on a host with no
//! SlopOS under it — and what lets a directory that cannot be read show as an
//! empty node rather than taking the sidebar down.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Nodes one tree may hold. A tree larger than this stops expanding rather than
/// growing a sidebar no one can scroll.
pub const MAX_NODES: usize = 20_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    /// Absolute path, so opening a file never depends on the walk that found it.
    pub path: String,
    pub is_dir: bool,
    pub depth: usize,
    pub expanded: bool,
    /// Whether this directory's children have been read yet.
    pub loaded: bool,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// Cleared when a re-read of the parent replaced this node. A dead slot
    /// keeps its index (children and parents name each other by index) and is
    /// handed back out by the next allocation.
    alive: bool,
}

/// One row of the flattened, visible tree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub node: usize,
    pub depth: usize,
}

pub struct FileTree {
    nodes: Vec<Node>,
    /// Indices of dead nodes, reused before the arena grows. Without this a
    /// directory re-read would leave its old children in the arena forever:
    /// visible nowhere, but found by every path lookup and listed by the file
    /// finder, once per refresh.
    free: Vec<usize>,
    root: usize,
    /// The flattened visible rows, rebuilt by every mutation rather than
    /// lazily: a view renders from `&self`, and a cache that rebuilt on read
    /// would make drawing a mutation.
    rows: Vec<Row>,
}

impl FileTree {
    /// A tree rooted at `path`, with the root expanded but not yet populated.
    pub fn new(path: &str) -> Self {
        let name = crate::document::file_name(path).to_string();
        let root = Node {
            name,
            path: normalize_dir(path),
            is_dir: true,
            depth: 0,
            expanded: true,
            loaded: false,
            parent: None,
            children: Vec::new(),
            alive: true,
        };
        let mut tree = Self {
            nodes: alloc::vec![root],
            free: Vec::new(),
            root: 0,
            rows: Vec::new(),
        };
        tree.rebuild_rows();
        tree
    }

    pub fn root_path(&self) -> &str {
        &self.nodes[self.root].path
    }

    pub fn node(&self, index: usize) -> Option<&Node> {
        self.nodes.get(index).filter(|n| n.alive)
    }

    /// Nodes the tree holds, dead slots excluded.
    pub fn len(&self) -> usize {
        self.nodes.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Directories whose children the app still has to read, in row order.
    pub fn pending(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.alive && n.is_dir && n.expanded && !n.loaded)
            .map(|(i, _)| i)
            .collect()
    }

    /// Takes a slot for `node`, reusing a dead one where there is one.
    fn alloc(&mut self, node: Node) -> usize {
        match self.free.pop() {
            Some(index) => {
                self.nodes[index] = node;
                index
            }
            None => {
                self.nodes.push(node);
                self.nodes.len() - 1
            }
        }
    }

    /// Marks `index` and everything under it dead and returns their slots.
    fn release(&mut self, index: usize) {
        let mut stack = alloc::vec![index];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get_mut(current) else {
                continue;
            };
            if !node.alive {
                continue;
            }
            node.alive = false;
            node.loaded = false;
            node.expanded = false;
            stack.append(&mut node.children);
            self.free.push(current);
        }
    }

    /// Installs `entries` as `index`'s children: directories first, then files,
    /// each group by case-insensitive name — the order a file manager uses.
    pub fn populate(&mut self, index: usize, mut entries: Vec<DirEntry>) {
        let Some(node) = self.nodes.get(index).filter(|n| n.alive) else {
            return;
        };
        if !node.is_dir {
            return;
        }
        let depth = node.depth + 1;
        let base = node.path.clone();

        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.name.cmp(&b.name))
            })
        });

        let old_children = core::mem::take(&mut self.nodes[index].children);
        // Expansion state survives a refresh: a reloaded directory keeps the
        // subdirectories the user had opened, open.
        let previously_expanded: Vec<String> = old_children
            .iter()
            .filter_map(|c| self.nodes.get(*c))
            .filter(|n| n.alive && n.expanded)
            .map(|n| n.name.clone())
            .collect();
        for child in old_children {
            self.release(child);
        }

        let mut children = Vec::with_capacity(entries.len());
        for entry in entries {
            if self.len() >= MAX_NODES {
                break;
            }
            let expanded = entry.is_dir && previously_expanded.iter().any(|n| *n == entry.name);
            let path = join_path(&base, &entry.name);
            let child = self.alloc(Node {
                name: entry.name,
                path,
                is_dir: entry.is_dir,
                depth,
                expanded,
                loaded: false,
                parent: Some(index),
                children: Vec::new(),
                alive: true,
            });
            children.push(child);
        }

        self.nodes[index].children = children;
        self.nodes[index].loaded = true;
        self.rebuild_rows();
    }

    /// Marks a directory as needing to be read again.
    pub fn invalidate(&mut self, index: usize) {
        if let Some(node) = self.nodes.get_mut(index).filter(|n| n.alive) {
            node.loaded = false;
        }
    }

    pub fn set_expanded(&mut self, index: usize, expanded: bool) {
        let changed = match self.nodes.get_mut(index) {
            Some(node) if node.alive && node.is_dir => {
                node.expanded = expanded;
                true
            }
            _ => false,
        };
        if changed {
            self.rebuild_rows();
        }
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(node) = self.node(index) {
            let expanded = node.expanded;
            self.set_expanded(index, !expanded);
        }
    }

    /// The visible rows, depth-first through expanded directories.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let mut stack = alloc::vec![self.root];
        while let Some(index) = stack.pop() {
            let Some(node) = self.nodes.get(index).filter(|n| n.alive) else {
                continue;
            };
            let depth = node.depth;
            if node.is_dir && node.expanded {
                for child in node.children.iter().rev() {
                    stack.push(*child);
                }
            }
            self.rows.push(Row { node: index, depth });
        }
    }

    /// The row a node occupies, when it is visible.
    pub fn row_of(&self, index: usize) -> Option<usize> {
        self.rows.iter().position(|r| r.node == index)
    }

    /// The node at `path`, when its parents are all populated.
    pub fn find_path(&self, path: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.alive && n.path == path)
    }

    /// Expands every ancestor of `index` so it becomes a visible row.
    pub fn reveal(&mut self, index: usize) {
        let mut cursor = self.node(index).and_then(|n| n.parent);
        while let Some(parent) = cursor {
            if let Some(node) = self.nodes.get_mut(parent).filter(|n| n.alive) {
                node.expanded = true;
                cursor = node.parent;
            } else {
                break;
            }
        }
        self.rebuild_rows();
    }

    /// Every file path under the tree, for the file finder. Directories the
    /// tree has not read are simply not in it.
    pub fn file_paths(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|n| n.alive && !n.is_dir)
            .map(|n| n.path.as_str())
            .collect()
    }
}

/// A directory path with no trailing slash, except for the root itself.
pub fn normalize_dir(path: &str) -> String {
    if path.is_empty() {
        return String::from("/");
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        String::from("/")
    } else {
        String::from(trimmed)
    }
}

/// `path` relative to directory `base`, or `None` when it is not under it.
///
/// The prefix has to end on a separator: `/foo` is not a parent of `/foobar`,
/// and a bare `starts_with` says it is — which walks the tree to a path that
/// cannot exist and labels a finder row with a mangled name.
pub fn relative_to<'a>(base: &str, path: &'a str) -> Option<&'a str> {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return Some(path.trim_start_matches('/'));
    }
    let rest = path.strip_prefix(base)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

/// `base` and `name` joined with exactly one separator.
pub fn join_path(base: &str, name: &str) -> String {
    let mut out = String::from(base.trim_end_matches('/'));
    out.push('/');
    out.push_str(name.trim_start_matches('/'));
    out
}
