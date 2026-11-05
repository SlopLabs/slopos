//! A throwaway checkout for a gate's tests: planted files under a fresh
//! directory, removed when the test ends.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::repo::Repo;

pub struct Tree {
    pub repo: Repo,
}

impl Tree {
    pub fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root: PathBuf = std::env::temp_dir().join(format!(
            "slopos-xtask-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create fixture root");
        let tree = Self {
            repo: Repo::at(root),
        };
        for (rel, contents) in files {
            tree.write(rel, contents);
        }
        tree
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.repo.path(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture dir");
        }
        std::fs::write(&path, contents).expect("write fixture file");
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.repo.root);
    }
}
