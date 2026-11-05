//! A crate under `apps/` depends on nothing but the crates an app is written
//! against: appkit, editor-core and image. The kernel ABI, the drawing
//! internals and the compositor connection beneath appkit change when the base
//! does, and an app naming one would have to change with it. Every dependency
//! but a dev one counts: normal, renamed, target-specific and build alike.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::Result;
use crate::repo::Repo;
use crate::report::Report;

const ALLOWED: &[&str] = &["slopos-appkit", "slopos-editor-core", "slopos-image"];

pub fn run(repo: &Repo) -> Result<Report> {
    let json = metadata(&repo.root, &repo.root)?;
    check(&json, &repo.root)
}

/// The manifests of the workspace rooted at `root`, as declared (no
/// resolution, so no registry is needed); cargo runs from `cwd` so the repo's
/// toolchain pin picks it.
fn metadata(cwd: &Path, root: &Path) -> Result<String> {
    let out = Command::new("cargo")
        .current_dir(cwd)
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn check(json: &str, root: &Path) -> Result<Report> {
    let meta: Value = serde_json::from_str(json)?;
    let root = real(root);
    let apps = root.join("apps");
    let mut report = Report::new();
    let mut found = 0;
    for pkg in meta["packages"]
        .as_array()
        .ok_or("cargo metadata: no packages")?
    {
        let manifest = real(Path::new(pkg["manifest_path"].as_str().unwrap_or_default()));
        if !manifest.starts_with(&apps) {
            continue;
        }
        found += 1;
        let at = manifest
            .strip_prefix(&root)
            .unwrap_or(&manifest)
            .to_string_lossy()
            .into_owned();
        let name = pkg["name"].as_str().unwrap_or_default();
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            let dep_name = dep["name"].as_str().unwrap_or_default();
            if dep["kind"].as_str() == Some("dev") || ALLOWED.contains(&dep_name) {
                continue;
            }
            report.finding(
                at.clone(),
                format!(
                    "app `{name}` depends on `{dep_name}` — an app is written against {} alone; \
                     reach what it needs through appkit",
                    ALLOWED.join(", ")
                ),
            );
        }
    }
    if found == 0 {
        return Err("no package found under apps/, so there is nothing to hold".into());
    }
    report.summary(format!(
        "checked the dependencies of {found} package(s) under apps/ against {}",
        ALLOWED.join(", ")
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const ROOT: &str = "[workspace]\nmembers = [\"apps\", \"other\"]\nresolver = \"2\"\n";
    const APPS: &str = r#"[package]
name = "fixture-apps"
version = "0.1.0"
edition = "2024"

[dependencies]
slopos-appkit = "0.1"
slopos-editor-core = "0.1"
slopos-image = "0.1"
slopos-abi = "0.1"
gfx = { package = "slopos-gfx", version = "0.1" }

[target.'cfg(unix)'.dependencies]
slopos-cabi = "0.1"

[build-dependencies]
slopos-windowing = "0.1"

[dev-dependencies]
slopos-protocol = "0.1"
"#;
    const OTHER: &str = r#"[package]
name = "fixture-other"
version = "0.1.0"
edition = "2024"

[dependencies]
slopos-userland = "0.1"
"#;

    fn scan(tree: &Tree) -> Result<Report> {
        let json = metadata(Path::new(env!("CARGO_MANIFEST_DIR")), &tree.repo.root)?;
        check(&json, &tree.repo.root)
    }

    #[test]
    fn direct_renamed_target_and_build_dependencies_fail() {
        let tree = Tree::new(&[
            ("Cargo.toml", ROOT),
            ("apps/Cargo.toml", APPS),
            ("other/Cargo.toml", OTHER),
            ("apps/src/lib.rs", ""),
            ("other/src/lib.rs", ""),
        ]);
        let report = scan(&tree).unwrap();
        let mut deps: Vec<&str> = report
            .findings
            .iter()
            .map(|f| {
                assert_eq!(f.location, "apps/Cargo.toml");
                f.message.split('`').nth(3).unwrap()
            })
            .collect();
        deps.sort();
        assert_eq!(
            deps,
            [
                "slopos-abi",
                "slopos-cabi",
                "slopos-gfx",
                "slopos-windowing"
            ]
        );
        assert!(report.summary.contains("1 package"), "{}", report.summary);
    }

    #[test]
    fn no_app_package_is_an_error() {
        let tree = Tree::new(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"other\"]\nresolver = \"2\"\n",
            ),
            ("other/Cargo.toml", OTHER),
            ("other/src/lib.rs", ""),
        ]);
        assert!(scan(&tree).is_err());
    }
}
