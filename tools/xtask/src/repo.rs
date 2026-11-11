//! The checkout a gate reads: its root, its files, and the Rust sources in it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;

/// Directories a walk outside a git work tree never enters: build output, the
/// materialised toolchain trees, and cargo's own.
const PRUNED: &[&str] = &["builddir", "third_party", "target", ".git"];

#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// The checkout holding the working directory: the nearest ancestor with a
    /// `justfile` and a `Cargo.toml`, else the one this binary was built from.
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        for dir in cwd.ancestors() {
            if dir.join("justfile").is_file() && dir.join("Cargo.toml").is_file() {
                return Ok(Self::at(dir));
            }
        }
        let built = Path::new(env!("CARGO_MANIFEST_DIR"));
        match built.ancestors().nth(2) {
            Some(root) if root.join("justfile").is_file() => Ok(Self::at(root)),
            _ => Err(format!("no SlopOS checkout above {}", cwd.display()).into()),
        }
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    pub fn read(&self, rel: &str) -> Result<String> {
        std::fs::read_to_string(self.path(rel)).map_err(|e| format!("{rel}: {e}").into())
    }

    /// Every `.rs` path in the checkout, repo-relative, sorted, once each.
    /// Tracked and untracked alike, since a gate that saw only committed files
    /// would pass on the change being made; ignored files are left out.
    pub fn rs_files(&self) -> Result<Vec<String>> {
        self.files("*.rs", |f| f.ends_with(".rs"))
    }

    /// Every file called `name`, at any depth, as `rs_files` lists sources.
    pub fn files_named(&self, name: &str) -> Result<Vec<String>> {
        self.files(&format!("*{name}"), |f| f.rsplit('/').next() == Some(name))
    }

    fn files(&self, pathspec: &str, wanted: impl Fn(&str) -> bool) -> Result<Vec<String>> {
        let mut files = match self.git_files(pathspec) {
            Some(files) => files,
            None => self.walk_files()?,
        };
        files.retain(|f| wanted(f) && self.path(f).is_file());
        files.sort();
        files.dedup();
        Ok(files)
    }

    /// `rs_files` restricted to the given repo-relative directories.
    pub fn rs_files_under(&self, dirs: &[&str]) -> Result<Vec<String>> {
        Ok(self
            .rs_files()?
            .into_iter()
            .filter(|f| dirs.iter().any(|d| under(f, d)))
            .collect())
    }

    fn git_files(&self, pathspec: &str) -> Option<Vec<String>> {
        if !self.root.join(".git").exists() {
            return None;
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                pathspec,
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(
            out.stdout
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect(),
        )
    }

    fn walk_files(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if entry.file_type()?.is_dir() {
                    if !PRUNED.contains(&name.as_ref()) {
                        stack.push(path);
                    }
                } else if let Ok(rel) = path.strip_prefix(&self.root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        Ok(out)
    }
}

/// Whether repo-relative `path` lies in directory `dir` (also repo-relative).
pub fn under(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    dir.is_empty()
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    #[test]
    fn under_matches_whole_components_only() {
        assert!(under("mm/src/lib.rs", "mm"));
        assert!(under("mm/src/lib.rs", "mm/"));
        assert!(!under("mmx/src/lib.rs", "mm"));
        assert!(!under("mm", "mm"));
    }

    #[test]
    fn a_walk_outside_git_skips_build_output_and_finds_every_source() {
        let tree = Tree::new(&[
            ("mm/src/lib.rs", ""),
            ("mm/src/a/b.rs", ""),
            ("builddir/x.rs", ""),
            ("third_party/y/src/lib.rs", ""),
            ("README.md", ""),
        ]);
        assert_eq!(
            tree.repo.rs_files().unwrap(),
            vec!["mm/src/a/b.rs".to_string(), "mm/src/lib.rs".to_string()]
        );
        assert_eq!(
            tree.repo.rs_files_under(&["mm/src/a"]).unwrap(),
            vec!["mm/src/a/b.rs".to_string()]
        );
    }
}
