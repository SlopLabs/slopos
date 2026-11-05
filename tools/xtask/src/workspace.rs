//! The workspace as cargo resolves it: its members, where each lives, and
//! which link into which. The kernel's crates are the `kernel` binary's
//! closure over normal dependencies, so a crate joining the image is covered
//! the moment it links, with no list to forget.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::Result;
use crate::repo::Repo;

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    /// `lib`, `bin`, `proc-macro`, `rlib`, `cdylib`, `test`, ...
    pub kinds: Vec<String>,
    /// Repo-relative path of the target's root source file.
    pub src_path: String,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub id: String,
    pub name: String,
    /// Repo-relative directory of the package's manifest.
    pub dir: String,
    pub targets: Vec<Target>,
}

impl Package {
    /// The library target's root, else the first binary's. A library target's
    /// kind is `lib`, or the crate types it names (`rlib`, `cdylib`, ...).
    pub fn crate_root(&self) -> Option<&str> {
        const LIBRARY: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
        let first = |kinds: &[&str]| {
            self.targets
                .iter()
                .find(|t| t.kinds.iter().any(|k| kinds.contains(&k.as_str())))
        };
        first(LIBRARY)
            .or_else(|| first(&["bin"]))
            .map(|t| t.src_path.as_str())
    }
}

#[derive(Debug)]
pub struct Workspace {
    /// Workspace members, sorted by directory.
    pub packages: Vec<Package>,
    /// Member id -> the members it depends on through a normal dependency.
    normal_deps: HashMap<String, Vec<String>>,
}

impl Workspace {
    pub fn load(repo: &Repo) -> Result<Self> {
        let out = Command::new("cargo")
            .current_dir(&repo.root)
            .args(["metadata", "--format-version", "1", "--locked"])
            .output()?;
        if !out.status.success() {
            return Err(format!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into());
        }
        Self::from_metadata(&String::from_utf8(out.stdout)?, &repo.root)
    }

    /// Parse `cargo metadata --format-version 1` output whose workspace sits at
    /// `root`; tests feed it a synthetic document.
    pub fn from_metadata(json: &str, root: &Path) -> Result<Self> {
        let meta: Value = serde_json::from_str(json)?;
        let rel = |p: &str| -> String {
            Path::new(p)
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| p.to_string())
        };
        let members: BTreeSet<String> = meta["workspace_members"]
            .as_array()
            .ok_or("cargo metadata: no workspace_members")?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();

        let mut packages = Vec::new();
        for pkg in meta["packages"]
            .as_array()
            .ok_or("cargo metadata: no packages")?
        {
            let id = pkg["id"].as_str().unwrap_or_default().to_string();
            if !members.contains(&id) {
                continue;
            }
            let manifest = pkg["manifest_path"].as_str().unwrap_or_default();
            let dir = rel(Path::new(manifest)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_str());
            let targets = pkg["targets"]
                .as_array()
                .map(|ts| {
                    ts.iter()
                        .map(|t| Target {
                            name: t["name"].as_str().unwrap_or_default().to_string(),
                            kinds: t["kind"]
                                .as_array()
                                .map(|k| {
                                    k.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default(),
                            src_path: rel(t["src_path"].as_str().unwrap_or_default()),
                        })
                        .collect()
                })
                .unwrap_or_default();
            packages.push(Package {
                id,
                name: pkg["name"].as_str().unwrap_or_default().to_string(),
                dir,
                targets,
            });
        }
        packages.sort_by(|a, b| a.dir.cmp(&b.dir));

        let mut normal_deps = HashMap::new();
        if let Some(nodes) = meta["resolve"]["nodes"].as_array() {
            for node in nodes {
                let id = node["id"].as_str().unwrap_or_default().to_string();
                if !members.contains(&id) {
                    continue;
                }
                let deps = node["deps"]
                    .as_array()
                    .map(|ds| {
                        ds.iter()
                            .filter(|d| {
                                d["dep_kinds"]
                                    .as_array()
                                    .is_some_and(|ks| ks.iter().any(|k| k["kind"].is_null()))
                            })
                            .filter_map(|d| d["pkg"].as_str())
                            .filter(|p| members.contains(*p))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                normal_deps.insert(id, deps);
            }
        }
        Ok(Self {
            packages,
            normal_deps,
        })
    }

    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// `root` and every member it reaches through normal dependencies, sorted
    /// by directory.
    pub fn closure(&self, root: &str) -> Result<Vec<&Package>> {
        let start = self
            .package(root)
            .ok_or_else(|| format!("no workspace member named `{root}`"))?;
        let mut seen = BTreeSet::new();
        let mut stack = vec![start.id.clone()];
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone()) {
                if let Some(deps) = self.normal_deps.get(&id) {
                    stack.extend(deps.iter().cloned());
                }
            }
        }
        Ok(self
            .packages
            .iter()
            .filter(|p| seen.contains(&p.id))
            .collect())
    }

    /// The crates the kernel image links: the `kernel` binary's closure.
    pub fn kernel_crates(&self) -> Result<Vec<&Package>> {
        self.closure("kernel")
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A metadata document for members `(name, dir, normal deps by name)`,
    /// rooted at `/ws`, each with a `src/lib.rs` library target.
    pub fn metadata(members: &[(&str, &str, &[&str])]) -> String {
        let id = |n: &str| format!("path+file:///ws/{n}#0.1.0");
        let packages: Vec<Value> = members
            .iter()
            .map(|(name, dir, _)| {
                serde_json::json!({
                    "id": id(name),
                    "name": name,
                    "manifest_path": format!("/ws/{dir}/Cargo.toml"),
                    "targets": [{"name": name, "kind": ["lib"], "src_path": format!("/ws/{dir}/src/lib.rs")}],
                })
            })
            .collect();
        let nodes: Vec<Value> = members
            .iter()
            .map(|(name, _, deps)| {
                serde_json::json!({
                    "id": id(name),
                    "deps": deps.iter().map(|d| serde_json::json!({
                        "pkg": id(d),
                        "dep_kinds": [{"kind": null}],
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::json!({
            "packages": packages,
            "workspace_members": members.iter().map(|(n, _, _)| id(n)).collect::<Vec<_>>(),
            "resolve": {"nodes": nodes},
        })
        .to_string()
    }

    #[test]
    fn the_kernel_closure_follows_normal_deps_only() {
        let json = metadata(&[
            ("kernel", "kernel", &["slopos-mm"]),
            ("slopos-mm", "mm", &["slopos-ostd"]),
            ("slopos-ostd", "slopos-ostd", &[]),
            ("slopos-userland", "userland", &["slopos-ostd"]),
        ]);
        let ws = Workspace::from_metadata(&json, Path::new("/ws")).unwrap();
        let dirs: Vec<&str> = ws
            .kernel_crates()
            .unwrap()
            .iter()
            .map(|p| p.dir.as_str())
            .collect();
        assert_eq!(dirs, ["kernel", "mm", "slopos-ostd"]);
        assert_eq!(
            ws.package("slopos-mm").unwrap().crate_root(),
            Some("mm/src/lib.rs")
        );
    }

    #[test]
    fn a_crate_root_is_the_library_whatever_crate_types_it_names() {
        let target = |name: &str, kinds: &[&str], src: &str| Target {
            name: name.to_string(),
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            src_path: src.to_string(),
        };
        let pkg = |targets| Package {
            id: String::new(),
            name: "p".to_string(),
            dir: "p".to_string(),
            targets,
        };
        let rlib = pkg(vec![
            target("tool", &["bin"], "p/src/main.rs"),
            target("p", &["rlib"], "p/src/lib.rs"),
        ]);
        assert_eq!(rlib.crate_root(), Some("p/src/lib.rs"));
        let cdylib = pkg(vec![target("p", &["cdylib", "rlib"], "p/src/lib.rs")]);
        assert_eq!(cdylib.crate_root(), Some("p/src/lib.rs"));
        let bin = pkg(vec![target("p", &["bin"], "p/src/main.rs")]);
        assert_eq!(bin.crate_root(), Some("p/src/main.rs"));
    }
}
