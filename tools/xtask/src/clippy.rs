//! `xtask clippy [host|kernel|userland ...]`: clippy over every target the
//! tree builds, each cargo invocation with the toolchain, target,
//! `-Zbuild-std` set, features and RUSTFLAGS of the build it stands for, so a
//! lint sees the code that build compiles and nothing else.
//!
//! - `host`: the crates `just test-host` tests and the `tools/` crates, with
//!   `--all-targets`.
//! - `kernel`: the `kernel` binary's crate graph as scripts/build_kernel.sh
//!   builds it, once with the dev variant's features and once with the tests
//!   variant's. `kernel/clippy.toml` (the kernel's bans) applies to this
//!   target only.
//! - `userland`: every cargo invocation scripts/build_userland.sh makes, its
//!   program lists read from scripts/lib/base.sh.
//!
//! The levels are `[workspace.lints.clippy]`, and `[workspace.lints.rust]`
//! denies warnings, so the run fails on any warning or error a workspace crate
//! raises. Each target checks into its own `builddir/clippy-<target>`, leaving
//! the real builds' caches alone.
//!
//! What is left out of the real builds' flags changes no code clippy sees:
//! `-C link-arg`s, `--artifact-dir` and `--timings` (clippy never links),
//! `-Z sanitizer=safestack` (it sets no cfg, and needs a runtime stub in the
//! sysroot), and the shell's `-Zon-broken-pipe=inherit`, which only changes
//! `main`'s entry shim and which `cargo clippy` cannot scope to one binary.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::Result;
use crate::repo::Repo;
use crate::toolchain::{self, SLOPOS};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Host,
    Kernel,
    Userland,
}

impl Target {
    const ALL: [Target; 3] = [Target::Host, Target::Kernel, Target::Userland];

    fn name(self) -> &'static str {
        match self {
            Target::Host => "host",
            Target::Kernel => "kernel",
            Target::Userland => "userland",
        }
    }

    fn target_dir(self) -> &'static str {
        match self {
            Target::Host => "builddir/clippy-host",
            Target::Kernel => "builddir/clippy-kernel",
            Target::Userland => "builddir/clippy-userland",
        }
    }
}

/// The targets named on the command line, in canonical order, each once; all
/// three when none is named.
fn parse_args(args: &[String]) -> Result<Vec<Target>> {
    if args.is_empty() {
        return Ok(Target::ALL.to_vec());
    }
    let mut targets = BTreeSet::new();
    for arg in args {
        let target = Target::ALL
            .into_iter()
            .find(|t| t.name() == arg)
            .ok_or_else(|| format!("clippy: unknown target `{arg}` (host, kernel or userland)"))?;
        targets.insert(target);
    }
    Ok(targets.into_iter().collect())
}

const KERNEL_TARGET: &str = "targets/x86_64-slos.json";
const KERNEL_RUSTFLAGS: &str = "-C force-frame-pointers=yes -Zunstable-options -Zemit-stack-sizes";
const KERNEL_CONF_DIR: &str = "kernel";

const USERLAND_TARGET: &str = "targets/x86_64-unknown-slopos.json";
const USERLAND_BUILD_STD: &str = "-Zbuild-std=core,alloc,std,panic_abort";
const BUILTINS_MEM: &str = "-Zbuild-std-features=compiler-builtins-mem";
const SYSTEM_RUSTFLAGS: &str = "-C linker=rust-lld -C linker-flavor=ld.lld -C panic=abort";

/// What the build recipes and scripts say, read once from the checkout.
#[derive(Debug, Clone, PartialEq)]
struct Tree {
    /// rust-toolchain.toml's `channel`, the host builds' toolchain.
    channel: String,
    /// `just test-host`'s packages, then the `tools/` crates.
    host_packages: Vec<String>,
    /// The justfile's `kernel_features_tests`.
    kernel_tests_features: String,
    /// base.sh's `BASE_PROGRAMS`: the shipped `slopos-userland` binaries.
    base_programs: Vec<String>,
    /// The `slopos-userland` binaries the tests base adds.
    test_programs: Vec<String>,
    /// base.sh's `APPS`: the `slopos-apps` binaries.
    apps: Vec<String>,
}

impl Tree {
    fn load(repo: &Repo) -> Result<Self> {
        let justfile = repo.read("justfile")?;
        let base = repo.read("scripts/lib/base.sh")?;
        let ws = Workspace::load(repo)?;

        let mut host_packages = recipe_packages(&justfile, "test-host")?;
        for pkg in &ws.packages {
            if crate::repo::under(&pkg.dir, "tools") && !host_packages.contains(&pkg.name) {
                host_packages.push(pkg.name.clone());
            }
        }

        let userland_bins: BTreeSet<&str> = ws
            .package("slopos-userland")
            .ok_or("clippy: no slopos-userland package in the workspace")?
            .targets
            .iter()
            .filter(|t| t.kinds.iter().any(|k| k == "bin"))
            .map(|t| t.name.as_str())
            .collect();
        let base_programs = shell_words(&base, "BASE_PROGRAMS")?;
        let test_programs: Vec<String> = shell_words(&base, "BASE_TEST_PROGRAMS")?
            .into_iter()
            .filter(|p| !base_programs.contains(p) && userland_bins.contains(p.as_str()))
            .collect();
        if test_programs.is_empty() {
            return Err("clippy: scripts/lib/base.sh's BASE_TEST_PROGRAMS names no slopos-userland test binary".into());
        }

        Ok(Tree {
            channel: toolchain::channel(repo)?,
            host_packages,
            kernel_tests_features: just_variable(&justfile, "kernel_features_tests")?,
            base_programs,
            test_programs,
            apps: shell_words(&base, "APPS")?,
        })
    }
}

/// Every `-p NAME` in the body of justfile recipe `name`.
fn recipe_packages(justfile: &str, name: &str) -> Result<Vec<String>> {
    let header = format!("{name}:");
    let mut lines = justfile.lines().skip_while(|l| !l.starts_with(&header));
    if lines.next().is_none() {
        return Err(format!("clippy: the justfile has no `{name}` recipe").into());
    }
    let mut packages = Vec::new();
    for line in lines.take_while(|l| l.starts_with([' ', '\t']) || l.is_empty()) {
        let mut words = line.split_whitespace();
        while let Some(word) = words.next() {
            if word == "-p"
                && let Some(pkg) = words.next()
            {
                packages.push(pkg.to_string());
            }
        }
    }
    if packages.is_empty() {
        return Err(format!("clippy: the justfile's `{name}` recipe names no `-p` package").into());
    }
    Ok(packages)
}

/// The quoted value of justfile variable `name := "..."`.
fn just_variable(justfile: &str, name: &str) -> Result<String> {
    justfile
        .lines()
        .filter_map(|l| l.strip_prefix(name))
        .filter_map(|rest| rest.trim_start().strip_prefix(":="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .next()
        .ok_or_else(|| format!("clippy: the justfile has no `{name}` variable").into())
}

/// The words of POSIX sh assignment `NAME="..."`, with `$VAR` and `${VAR}`
/// words expanded from the same file's assignments.
fn shell_words(text: &str, name: &str) -> Result<Vec<String>> {
    let prefix = format!("{name}=\"");
    let value = text
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| format!("clippy: scripts/lib/base.sh assigns no {name}"))?;
    let mut words = Vec::new();
    for word in value.split_whitespace() {
        match word.strip_prefix('$') {
            Some(var) => {
                let var = var.trim_start_matches('{').trim_end_matches('}');
                words.extend(shell_words(text, var)?);
            }
            None => words.push(word.to_string()),
        }
    }
    if words.is_empty() {
        return Err(format!("clippy: scripts/lib/base.sh's {name} is empty").into());
    }
    Ok(words)
}

/// One `cargo clippy` run.
#[derive(Debug, Clone, PartialEq)]
struct Invocation {
    label: String,
    target: Target,
    toolchain: String,
    /// `None` clears RUSTFLAGS, as the host build has none of its own.
    rustflags: Option<String>,
    /// Everything after `cargo +TOOLCHAIN`.
    args: Vec<String>,
}

impl Invocation {
    fn new(label: &str, target: Target, toolchain: &str, rustflags: Option<&str>) -> Self {
        Invocation {
            label: label.to_string(),
            target,
            toolchain: toolchain.to_string(),
            rustflags: rustflags.map(str::to_string),
            args: Vec::new(),
        }
    }

    fn arg(mut self, a: &str) -> Self {
        self.args.push(a.to_string());
        self
    }

    fn args<'a>(mut self, args: impl IntoIterator<Item = &'a str>) -> Self {
        self.args.extend(args.into_iter().map(str::to_string));
        self
    }

    fn each(mut self, flag: &str, values: &[String]) -> Self {
        for v in values {
            self.args.push(flag.to_string());
            self.args.push(v.clone());
        }
        self
    }

    fn command(&self, repo: &Repo) -> Command {
        let mut cmd = toolchain::cargo(&self.toolchain);
        cmd.current_dir(&repo.root)
            .args(&self.args)
            .args(["--message-format=json", "--keep-going"])
            .env("CARGO_TARGET_DIR", repo.path(self.target.target_dir()))
            .env_remove("CARGO_ENCODED_RUSTFLAGS");
        match &self.rustflags {
            Some(flags) => cmd.env("RUSTFLAGS", flags),
            None => cmd.env_remove("RUSTFLAGS"),
        };
        match self.target {
            Target::Kernel => cmd.env("CLIPPY_CONF_DIR", repo.path(KERNEL_CONF_DIR)),
            Target::Host | Target::Userland => cmd.env_remove("CLIPPY_CONF_DIR"),
        };
        cmd
    }

    fn display(&self) -> String {
        let mut s = String::new();
        if self.target == Target::Kernel {
            s.push_str(&format!("CLIPPY_CONF_DIR={KERNEL_CONF_DIR} "));
        }
        if let Some(flags) = &self.rustflags {
            s.push_str(&format!("RUSTFLAGS='{flags}' "));
        }
        s.push_str(&format!(
            "{} {}",
            toolchain::shown(&self.toolchain),
            self.args.join(" ")
        ));
        s
    }
}

fn plan(target: Target, tree: &Tree) -> Vec<Invocation> {
    match target {
        Target::Host => vec![
            Invocation::new("host", target, &tree.channel, None)
                .args(["clippy", "--locked", "--all-targets"])
                .each("-p", &tree.host_packages),
        ],
        Target::Kernel => [("dev", ""), ("tests", tree.kernel_tests_features.as_str())]
            .into_iter()
            .map(|(variant, features)| {
                Invocation::new(
                    &format!("kernel {variant}"),
                    target,
                    SLOPOS,
                    Some(KERNEL_RUSTFLAGS),
                )
                .args([
                    "clippy",
                    "--locked",
                    "-Zbuild-std=core,alloc",
                    BUILTINS_MEM,
                    "-Zunstable-options",
                    "--config",
                    "profile.dev.trim-paths=\"all\"",
                    "--config",
                    "profile.release.trim-paths=\"all\"",
                    "--config",
                    "future-incompat-report.frequency=\"never\"",
                    "--target",
                    KERNEL_TARGET,
                    "--package",
                    "kernel",
                    "--bin",
                    "kernel",
                ])
                .arg(&format!("--features={features}"))
            })
            .collect(),
        Target::Userland => userland_plan(tree),
    }
}

/// build_userland.sh's cargo invocations, in its order.
fn userland_plan(tree: &Tree) -> Vec<Invocation> {
    let userland_flags = format!("{SYSTEM_RUSTFLAGS} -C relocation-model=static");
    let so_flags = format!(
        "{SYSTEM_RUSTFLAGS} -C relocation-model=pic -Z tls-model=initial-exec -C force-unwind-tables"
    );
    let clippy = |label: &str, flags: &str, build_std: &str| {
        Invocation::new(label, Target::Userland, SLOPOS, Some(flags)).args([
            "--config",
            "profile.release.package.compiler_builtins.codegen-units=10000",
            "clippy",
            "--locked",
            build_std,
            "-Zunstable-options",
            "-Zjson-target-spec",
            "--target",
            USERLAND_TARGET,
            "--release",
        ])
    };
    let programs = |label: &str| {
        clippy(label, &userland_flags, USERLAND_BUILD_STD).args([
            BUILTINS_MEM,
            "--package",
            "slopos-userland",
        ])
    };
    vec![
        clippy("crt0", &userland_flags, "-Zbuild-std=core").args(["--package", "slopos-crt0"]),
        programs("base programs")
            .each("--bin", &tree.base_programs)
            .arg("--no-default-features"),
        programs("test programs")
            .each("--bin", &tree.test_programs)
            .args(["--features", "testbins", "--no-default-features"]),
        clippy(
            "libc.a",
            &format!("{userland_flags} -C force-unwind-tables"),
            USERLAND_BUILD_STD,
        )
        .args([BUILTINS_MEM, "--package", "slopos-slibc-staticlib"]),
        clippy(
            "libbuiltins.a",
            &format!("{SYSTEM_RUSTFLAGS} -C relocation-model=pic"),
            "-Zbuild-std=core",
        )
        .args(["--package", "slopos-slibc-builtins"]),
        clippy("libc.so", &so_flags, "-Zbuild-std=core,alloc")
            .args(["--package", "slopos-slibc-cdylib"]),
        clippy(
            "apps",
            "-C linker=rust-lld -C linker-flavor=ld.lld",
            "-Zbuild-std=std,panic_unwind",
        )
        .args(["--package", "slopos-apps"])
        .each("--bin", &tree.apps),
        clippy(
            "libdltest.so",
            &format!("{so_flags} -Z tls-model=global-dynamic"),
            "-Zbuild-std=core",
        )
        .args(["--package", "slopos-dltest", "--lib"]),
        clippy("dl_probe", &userland_flags, "-Zbuild-std=core").args([
            "--package",
            "slopos-dltest",
            "--bin",
            "dl_probe",
        ]),
    ]
}

/// What one invocation reported for the workspace's crates.
#[derive(Debug, Default)]
struct Outcome {
    crates: BTreeSet<String>,
    /// Diagnostics by level, then by lint (`""` for one with no code).
    diagnostics: BTreeMap<String, BTreeMap<String, usize>>,
}

impl Outcome {
    fn findings(&self) -> usize {
        self.diagnostics
            .values()
            .flat_map(|by_lint| by_lint.values())
            .sum()
    }

    /// Fold one line of cargo's `--message-format=json` stream in, printing
    /// any diagnostic it carries for a crate in `members`.
    fn record(&mut self, line: &str, members: &BTreeMap<String, String>) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            println!("{line}");
            return;
        };
        let Some(name) = msg["package_id"].as_str().and_then(|id| members.get(id)) else {
            return;
        };
        match msg["reason"].as_str() {
            Some("compiler-artifact") => {
                self.crates.insert(name.clone());
            }
            Some("compiler-message") => {
                let diag = &msg["message"];
                let level = diag["level"].as_str().unwrap_or_default();
                if !matches!(level, "warning" | "error") {
                    return;
                }
                if let Some(rendered) = diag["rendered"].as_str() {
                    eprint!("{rendered}");
                }
                let lint = diag["code"]["code"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                *self
                    .diagnostics
                    .entry(level.to_string())
                    .or_default()
                    .entry(lint)
                    .or_default() += 1;
            }
            _ => {}
        }
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (level, by_lint) in &self.diagnostics {
            let total: usize = by_lint.values().sum();
            let lints: Vec<String> = by_lint
                .iter()
                .map(|(lint, n)| format!("{} {n}", if lint.is_empty() { "uncoded" } else { lint }))
                .collect();
            parts.push(format!("{total} {level}(s): {}", lints.join(", ")));
        }
        if parts.is_empty() {
            "clean".to_string()
        } else {
            parts.join("; ")
        }
    }
}

fn run(repo: &Repo, inv: &Invocation, members: &BTreeMap<String, String>) -> Result<bool> {
    eprintln!("xtask clippy: {}: {}", inv.label, inv.display());
    let mut child = inv.command(repo).stdout(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().ok_or("clippy: no stdout from cargo")?;
    let mut outcome = Outcome::default();
    for line in BufReader::new(stdout).lines() {
        outcome.record(&line?, members);
    }
    let status = child.wait()?;
    let passed = status.success() && outcome.findings() == 0;
    if passed && outcome.crates.is_empty() {
        return Err(format!("clippy: {} checked no workspace crate", inv.label).into());
    }
    eprintln!(
        "xtask clippy: {}: {} workspace crates, {}{}",
        inv.label,
        outcome.crates.len(),
        outcome.summary(),
        if status.success() {
            String::new()
        } else {
            format!(" (cargo {status})")
        }
    );
    Ok(passed)
}

pub fn main(repo: &Repo, args: &[String]) -> Result<bool> {
    let targets = parse_args(args)?;
    let tree = Tree::load(repo)?;
    let members: BTreeMap<String, String> = Workspace::load(repo)?
        .packages
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let mut failed = Vec::new();
    let mut runs = 0;
    for target in targets {
        for inv in plan(target, &tree) {
            runs += 1;
            if !run(repo, &inv, &members)? {
                failed.push(inv.label);
            }
        }
    }
    if failed.is_empty() {
        eprintln!("xtask clippy: OK — {runs} cargo clippy runs, no warning or error");
        Ok(true)
    } else {
        eprintln!(
            "xtask clippy: {} of {runs} runs failed: {}",
            failed.len(),
            failed.join(", ")
        );
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn tree() -> Tree {
        Tree {
            channel: "nightly-2026-09-03".into(),
            host_packages: strings(&["slopos-abi", "slopos-xtask"]),
            kernel_tests_features: "slopos-testing/qemu-exit kernel/tests".into(),
            base_programs: strings(&["init", "shell"]),
            test_programs: strings(&["fork_test"]),
            apps: strings(&["editor"]),
        }
    }

    #[test]
    fn no_argument_means_every_target() {
        assert_eq!(parse_args(&[]).unwrap(), Target::ALL.to_vec());
    }

    #[test]
    fn named_targets_run_once_in_canonical_order() {
        let got = parse_args(&strings(&["userland", "host", "userland"])).unwrap();
        assert_eq!(got, vec![Target::Host, Target::Userland]);
    }

    #[test]
    fn an_unknown_target_is_an_error() {
        let err = parse_args(&strings(&["kernel", "kernal"])).unwrap_err();
        assert!(err.to_string().contains("`kernal`"), "{err}");
    }

    #[test]
    fn host_checks_every_target_of_the_named_packages() {
        let plan = plan(Target::Host, &tree());
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].display(),
            format!(
                "{} clippy --locked --all-targets -p slopos-abi -p slopos-xtask",
                toolchain::shown("nightly-2026-09-03")
            )
        );
        assert_eq!(plan[0].rustflags, None);
    }

    #[test]
    fn kernel_checks_the_dev_and_tests_variants_as_build_kernel_builds_them() {
        let plan = plan(Target::Kernel, &tree());
        let shown: Vec<String> = plan.iter().map(Invocation::display).collect();
        let common = format!(
            "CLIPPY_CONF_DIR=kernel \
            RUSTFLAGS='-C force-frame-pointers=yes -Zunstable-options -Zemit-stack-sizes' \
            {} clippy --locked -Zbuild-std=core,alloc \
            -Zbuild-std-features=compiler-builtins-mem -Zunstable-options \
            --config profile.dev.trim-paths=\"all\" --config profile.release.trim-paths=\"all\" \
            --config future-incompat-report.frequency=\"never\" \
            --target targets/x86_64-slos.json --package kernel --bin kernel",
            toolchain::shown(SLOPOS)
        );
        assert_eq!(
            shown,
            vec![
                format!("{common} --features="),
                format!("{common} --features=slopos-testing/qemu-exit kernel/tests"),
            ]
        );
        assert_eq!(plan[0].label, "kernel dev");
        assert_eq!(plan[1].label, "kernel tests");
    }

    #[test]
    fn userland_mirrors_build_userland() {
        let plan = plan(Target::Userland, &tree());
        let labels: Vec<&str> = plan.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "crt0",
                "base programs",
                "test programs",
                "libc.a",
                "libbuiltins.a",
                "libc.so",
                "apps",
                "libdltest.so",
                "dl_probe"
            ]
        );
        let common = format!(
            "{} --config profile.release.package.compiler_builtins.codegen-units=10000 clippy --locked",
            toolchain::shown(SLOPOS)
        );
        let spec = "-Zunstable-options -Zjson-target-spec --target targets/x86_64-unknown-slopos.json --release";
        let system = "-C linker=rust-lld -C linker-flavor=ld.lld -C panic=abort";
        let shown: Vec<String> = plan.iter().map(Invocation::display).collect();
        assert_eq!(
            shown[0],
            format!(
                "RUSTFLAGS='{system} -C relocation-model=static' {common} -Zbuild-std=core {spec} --package slopos-crt0"
            )
        );
        assert_eq!(
            shown[1],
            format!(
                "RUSTFLAGS='{system} -C relocation-model=static' {common} -Zbuild-std=core,alloc,std,panic_abort {spec} \
                 -Zbuild-std-features=compiler-builtins-mem --package slopos-userland --bin init --bin shell --no-default-features"
            )
        );
        assert_eq!(
            shown[2],
            format!(
                "RUSTFLAGS='{system} -C relocation-model=static' {common} -Zbuild-std=core,alloc,std,panic_abort {spec} \
                 -Zbuild-std-features=compiler-builtins-mem --package slopos-userland --bin fork_test \
                 --features testbins --no-default-features"
            )
        );
        assert_eq!(
            shown[3],
            format!(
                "RUSTFLAGS='{system} -C relocation-model=static -C force-unwind-tables' {common} \
                 -Zbuild-std=core,alloc,std,panic_abort {spec} -Zbuild-std-features=compiler-builtins-mem \
                 --package slopos-slibc-staticlib"
            )
        );
        assert_eq!(
            shown[4],
            format!(
                "RUSTFLAGS='{system} -C relocation-model=pic' {common} -Zbuild-std=core {spec} --package slopos-slibc-builtins"
            )
        );
        let so = format!(
            "{system} -C relocation-model=pic -Z tls-model=initial-exec -C force-unwind-tables"
        );
        assert_eq!(
            shown[5],
            format!(
                "RUSTFLAGS='{so}' {common} -Zbuild-std=core,alloc {spec} --package slopos-slibc-cdylib"
            )
        );
        assert_eq!(
            shown[6],
            format!(
                "RUSTFLAGS='-C linker=rust-lld -C linker-flavor=ld.lld' {common} -Zbuild-std=std,panic_unwind {spec} \
                 --package slopos-apps --bin editor"
            )
        );
        assert_eq!(
            shown[7],
            format!(
                "RUSTFLAGS='{so} -Z tls-model=global-dynamic' {common} -Zbuild-std=core {spec} --package slopos-dltest --lib"
            )
        );
        assert_eq!(
            shown[8],
            format!(
                "RUSTFLAGS='{system} -C relocation-model=static' {common} -Zbuild-std=core {spec} \
                 --package slopos-dltest --bin dl_probe"
            )
        );
    }

    #[test]
    fn every_target_checks_into_its_own_directory() {
        let repo = Repo::at("/checkout");
        for target in Target::ALL {
            for inv in plan(target, &tree()) {
                let cmd = inv.command(&repo);
                let env: BTreeMap<_, _> = cmd
                    .get_envs()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into_owned(),
                            v.map(|v| v.to_string_lossy().into_owned()),
                        )
                    })
                    .collect();
                assert_eq!(
                    env["CARGO_TARGET_DIR"].as_deref(),
                    Some(format!("/checkout/builddir/clippy-{}", target.name()).as_str())
                );
                let conf = env["CLIPPY_CONF_DIR"].as_deref();
                match target {
                    Target::Kernel => assert_eq!(conf, Some("/checkout/kernel")),
                    Target::Host | Target::Userland => assert_eq!(conf, None),
                }
                assert_eq!(env["CARGO_ENCODED_RUSTFLAGS"], None);
                let args: Vec<_> = cmd
                    .get_args()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                let expected = toolchain::cargo(&inv.toolchain);
                let lead: Vec<_> = expected
                    .get_args()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                assert!(args.starts_with(&lead));
                assert!(args.ends_with(&strings(&["--message-format=json", "--keep-going"])));
            }
        }
    }

    const JUSTFILE: &str = "\
kernel_features_tests := \"slopos-testing/qemu-exit kernel/tests\"

[doc(\"Run the host tests\")]
test-host:
    {{cargo}} +{{rust_channel}} test -p slopos-abi -p slopos-gfx \\
        -p slopos-font

other:
    cargo test -p not-this-one
";

    #[test]
    fn host_packages_come_from_the_test_host_recipe() {
        assert_eq!(
            recipe_packages(JUSTFILE, "test-host").unwrap(),
            strings(&["slopos-abi", "slopos-gfx", "slopos-font"])
        );
        assert!(recipe_packages(JUSTFILE, "test-guest").is_err());
        assert!(recipe_packages("test-host:\n    cargo test\n", "test-host").is_err());
    }

    #[test]
    fn kernel_tests_features_come_from_the_justfile() {
        assert_eq!(
            just_variable(JUSTFILE, "kernel_features_tests").unwrap(),
            "slopos-testing/qemu-exit kernel/tests"
        );
        assert!(just_variable(JUSTFILE, "kernel_features").is_err());
    }

    #[test]
    fn base_lists_expand_their_references() {
        let base = "\
BASE_PROGRAMS=\"init shell\"
APPS=\"editor\"
BASE_TEST_PROGRAMS=\"$BASE_PROGRAMS fork_test ${APPS}\"
EMPTY=\"\"
";
        assert_eq!(
            shell_words(base, "BASE_PROGRAMS").unwrap(),
            strings(&["init", "shell"])
        );
        assert_eq!(
            shell_words(base, "BASE_TEST_PROGRAMS").unwrap(),
            strings(&["init", "shell", "fork_test", "editor"])
        );
        assert!(shell_words(base, "EMPTY").is_err());
        assert!(shell_words(base, "MISSING").is_err());
    }

    #[test]
    fn diagnostics_are_counted_for_workspace_crates_only() {
        let members = BTreeMap::from([(
            "path+file:///w/abi#0.1.0".to_string(),
            "slopos-abi".to_string(),
        )]);
        let mut outcome = Outcome::default();
        for line in [
            r#"{"reason":"compiler-artifact","package_id":"path+file:///w/abi#0.1.0"}"#,
            r#"{"reason":"compiler-message","package_id":"path+file:///w/abi#0.1.0","message":{"level":"error","code":{"code":"clippy::eq_op"},"rendered":""}}"#,
            r#"{"reason":"compiler-message","package_id":"path+file:///w/abi#0.1.0","message":{"level":"warning","code":null,"rendered":""}}"#,
            r#"{"reason":"compiler-message","package_id":"path+file:///w/abi#0.1.0","message":{"level":"note","code":null,"rendered":""}}"#,
            r#"{"reason":"compiler-message","package_id":"path+file:///sysroot/core#0.0.0","message":{"level":"warning","code":null,"rendered":""}}"#,
            r#"{"reason":"build-finished","success":false}"#,
        ] {
            outcome.record(line, &members);
        }
        assert_eq!(outcome.crates, BTreeSet::from(["slopos-abi".to_string()]));
        assert_eq!(outcome.findings(), 2);
        assert_eq!(
            outcome.summary(),
            "1 error(s): clippy::eq_op 1; 1 warning(s): uncoded 1"
        );
        assert_eq!(Outcome::default().summary(), "clean");
    }
}
