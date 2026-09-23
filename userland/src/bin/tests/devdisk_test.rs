use slopos_userland as _;

use slopos_abi::fs::MS_RDONLY;
use slopos_slibc::test_harness::note;
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs as fs_syscall;
use std::ffi::c_char;
use std::fs;
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Where the boot's `mount=LABEL=slopos-dev:/devel` puts the volume; every
/// root image carries the empty directory.
const MOUNT_POINT: &str = "/devel";
const MOUNT_POINT_C: &[u8] = b"/devel\0";

/// The label `scripts/build_devdisk.sh` gives the volume. Disk letters are
/// probe order, so this is the only stable name the guest has for it.
const LABEL_SOURCE: &[u8] = b"LABEL=slopos-dev";

/// A second, read-only view of the volume, used only to tell "not attached"
/// from "attached but not mounted at /devel".
const PROBE_POINT: &str = "/tmp/devdisk";

/// Written by `scripts/build_devdisk.sh` at the volume root. Line 1 is the
/// magic and a format version; `file <bytes> <path>` and `dir <path>` lines
/// are the inventory this test grades the volume against, and a `source
/// <path>` line names the guest's source tree.
const MARKER: &str = "SLOPOS-DEVDISK";

fn marker_path() -> String {
    format!("{MOUNT_POINT}/{MARKER}")
}

fn mounted_at_devel() -> bool {
    fs::read_to_string(marker_path())
        .map(|text| text.starts_with(MARKER))
        .unwrap_or(false)
}

/// Whether the boot mounted the dev disk at `/devel`. `Err(false)`, a failure,
/// when a volume labelled `slopos-dev` is attached but `/devel` does not hold
/// it; `Err(true)`, a pass with a note, when none is attached, as the
/// kernel-side capacity report is: `DEV_DISK_IMG` is opt-in and an ordinary
/// run sets none. A utest's own stdout is init's console, not the serial line
/// the run is read from, so the note is the only channel that reaches KTAP.
static DEV_DISK: LazyLock<Result<(), bool>> = LazyLock::new(|| {
    if mounted_at_devel() {
        return Ok(());
    }
    let _ = fs::create_dir(PROBE_POINT);
    match fs_syscall::mount(LABEL_SOURCE, PROBE_POINT.as_bytes(), b"ext2", MS_RDONLY) {
        Ok(()) => {
            let _ = fs_syscall::umount2(b"/tmp/devdisk\0".as_ptr() as *const c_char, 0);
            note(
                "a slopos-dev volume is attached but /devel does not hold it: the boot's mount= is missing or failed",
            );
            Err(false)
        }
        Err(e) => {
            note(&format!("no dev disk attached (LABEL=slopos-dev: {e})"));
            Err(true)
        }
    }
});

/// Every `file` line names a path that is there and stats as the byte count
/// the host recorded when it staged the volume. A dev disk whose `lib/` never
/// arrived mounts, reads and passes every structural check; the inventory is
/// what turns that into a failure here rather than a link error in a guest
/// build hours later.
fn devdisk_inventory_reads_back() -> bool {
    if let Err(verdict) = *DEV_DISK {
        return verdict;
    }
    match fs::read_to_string(marker_path()) {
        Ok(text) => grade_inventory(&text),
        Err(e) => {
            note(&format!("{MOUNT_POINT}: reading {MARKER} back: {e}"));
            false
        }
    }
}

fn grade_inventory(text: &str) -> bool {
    let mut files = 0usize;
    let mut dirs = 0usize;
    for line in text.lines() {
        let mut field = line.split(' ');
        match field.next() {
            Some("file") => {
                let (Some(want), Some(rel)) = (field.next(), field.next()) else {
                    note(&format!("malformed inventory line {line:?}"));
                    return false;
                };
                let Ok(want) = want.parse::<u64>() else {
                    note(&format!("unparsable size in {line:?}"));
                    return false;
                };
                let got = match fs::metadata(format!("{MOUNT_POINT}/{rel}")) {
                    Ok(meta) => meta.len(),
                    Err(e) => {
                        note(&format!("{rel} is not on the volume: {e}"));
                        return false;
                    }
                };
                if got != want {
                    note(&format!("{rel} is {got} bytes, want {want}"));
                    return false;
                }
                files += 1;
            }
            Some("dir") => {
                let Some(rel) = field.next() else {
                    note(&format!("malformed inventory line {line:?}"));
                    return false;
                };
                match fs::metadata(format!("{MOUNT_POINT}/{rel}")) {
                    Ok(meta) if meta.is_dir() => dirs += 1,
                    Ok(_) => {
                        note(&format!("{rel} is not a directory"));
                        return false;
                    }
                    Err(e) => {
                        note(&format!("{rel} is not on the volume: {e}"));
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    if files == 0 || dirs == 0 {
        note(&format!(
            "the marker lists {files} files and {dirs} directories"
        ));
        return false;
    }
    note(&format!(
        "{MOUNT_POINT}: {files} files and {dirs} directories verified"
    ));
    true
}

/// The boot's mount holds the device's exclusive write claim, so a second
/// writable mount is refused; `umount2` gives the claim back, so the same
/// volume mounts again by label. A leaked claim answers `EBUSY` forever,
/// which is a failure no amount of reading detects. Leaves `/devel` mounted.
fn devdisk_remounts_after_umount() -> bool {
    if let Err(verdict) = *DEV_DISK {
        return verdict;
    }
    let _ = fs::create_dir(PROBE_POINT);
    match fs_syscall::mount(LABEL_SOURCE, PROBE_POINT.as_bytes(), b"ext2", 0) {
        Err(e) if e == SyscallError::EBUSY => {}
        Err(e) => {
            note(&format!("a second writable mount gave {e}, want EBUSY"));
            return false;
        }
        Ok(()) => {
            let _ = fs_syscall::umount2(b"/tmp/devdisk\0".as_ptr() as *const c_char, 0);
            note("the volume mounted writable twice: the boot's mount holds no claim");
            return false;
        }
    }
    if let Err(e) = fs_syscall::umount2(MOUNT_POINT_C.as_ptr() as *const c_char, 0) {
        note(&format!("umount of {MOUNT_POINT} failed: {e}"));
        return false;
    }
    if mounted_at_devel() {
        note(&format!(
            "{MOUNT_POINT} still shows the volume after its umount"
        ));
        return false;
    }
    if let Err(e) = fs_syscall::mount(LABEL_SOURCE, MOUNT_POINT.as_bytes(), b"ext2", 0) {
        note(&format!("re-mount by LABEL=slopos-dev failed: {e}"));
        return false;
    }
    if !mounted_at_devel() {
        note(&format!("re-mounted without its {MARKER}"));
        return false;
    }
    true
}

fn vendor_directory(config: &str) -> Option<&str> {
    config.lines().find_map(|l| {
        l.trim()
            .strip_prefix("directory")?
            .trim_start()
            .strip_prefix('=')?
            .trim()
            .strip_prefix('"')?
            .strip_suffix('"')
    })
}

/// `(name, version, checksum)` for each registry package `lock` names.
fn registry_packages(lock: &str) -> Vec<(&str, &str, &str)> {
    let mut out = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let field = |key: &str| {
            block.lines().find_map(|l| {
                l.strip_prefix(key)
                    .and_then(|rest| rest.trim().strip_prefix('='))
                    .map(|v| v.trim().trim_matches('"'))
            })
        };
        if let (Some(name), Some(version), Some(checksum)) =
            (field("name"), field("version"), field("checksum"))
        {
            out.push((name, version, checksum));
        }
    }
    out
}

/// The copy of the source tree that reached the guest needs no registry: its
/// config reads a vendor directory holding every locked crate by checksum.
fn devdisk_source_is_vendored() -> bool {
    if let Err(verdict) = *DEV_DISK {
        return verdict;
    }
    match fs::read_to_string(marker_path()) {
        Ok(text) => match text.lines().find_map(|l| l.strip_prefix("source ")) {
            Some(source) => grade_source(source),
            None => {
                note("the volume carries no source tree; it predates the seeding");
                true
            }
        },
        Err(e) => {
            note(&format!("{MOUNT_POINT}: reading {MARKER} back: {e}"));
            false
        }
    }
}

fn grade_source(source: &str) -> bool {
    let root = format!("{MOUNT_POINT}/{source}");
    let config = match fs::read_to_string(format!("{root}/.cargo/config.toml")) {
        Ok(config) => config,
        Err(e) => {
            note(&format!("{source}/.cargo/config.toml: {e}"));
            return false;
        }
    };
    let vendor = match vendor_directory(&config) {
        Some(dir) if config.contains("replace-with = \"vendored-sources\"") => dir,
        _ => {
            note(&format!(
                "{source}/.cargo/config.toml reads no vendored sources"
            ));
            return false;
        }
    };
    let (lock, std_lock) = match (
        fs::read_to_string(format!("{root}/Cargo.lock")),
        fs::read_to_string(format!("{root}/{vendor}/library.lock")),
    ) {
        (Ok(l), Ok(s)) => (l, s),
        (l, s) => {
            note(&format!(
                "{source} lacks a lockfile: {:?} {:?}",
                l.err(),
                s.err()
            ));
            return false;
        }
    };
    let (workspace, std) = (registry_packages(&lock), registry_packages(&std_lock));
    if workspace.is_empty() || std.is_empty() {
        note(&format!(
            "a lockfile names no registry package ({} and {})",
            workspace.len(),
            std.len()
        ));
        return false;
    }
    let packages: Vec<_> = workspace.into_iter().chain(std).collect();
    for (name, version, checksum) in &packages {
        let manifest = format!("{root}/{vendor}/{name}-{version}/.cargo-checksum.json");
        match fs::read_to_string(&manifest) {
            Ok(json) if json.contains(&format!("\"package\":\"{checksum}\"")) => {}
            Ok(_) => {
                note(&format!(
                    "{name} {version} is vendored with another checksum"
                ));
                return false;
            }
            Err(e) => {
                note(&format!("{name} {version} is not vendored: {e}"));
                return false;
            }
        }
    }
    note(&format!(
        "{} vendored packages match both lockfiles",
        packages.len()
    ));
    true
}

/// The staged toolchain prefix, absolute, when the marker names one.
fn toolchain() -> Option<String> {
    let text = fs::read_to_string(marker_path()).ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("toolchain "))?;
    Some(format!("{MOUNT_POINT}/{rel}"))
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    took: Duration,
}

impl Ran {
    fn ok(&self, what: &str) -> bool {
        if self.code == Some(0) {
            return true;
        }
        note(&format!(
            "{what} exited {:?}: {}{}",
            self.code,
            self.stdout.trim_end(),
            self.stderr.trim_end()
        ));
        false
    }
}

/// Runs `program` in `dir` the way a developer's shell would: the toolchain's
/// `bin/` first on `PATH`, and `CARGO_HOME` on the volume, since `HOME` is the
/// root, which is read-only on the shipped image.
fn run(prefix: &str, dir: &str, program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Ran> {
    let started = Instant::now();
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("PATH", format!("{prefix}/bin:/bin"))
        .env("CARGO_HOME", format!("{MOUNT_POINT}/cargo-home"))
        .env_remove("LD_LIBRARY_PATH")
        .envs(env.iter().copied())
        .output();
    match out {
        Ok(out) => Some(Ran {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            took: started.elapsed(),
        }),
        Err(e) => {
            note(&format!("spawning {program}: {e}"));
            None
        }
    }
}

/// A fresh directory for one rung, so every run compiles from nothing.
fn scratch(rung: &str, files: &[(&str, &str)]) -> Option<String> {
    let dir = format!("{MOUNT_POINT}/ladder/{rung}");
    let _ = fs::remove_dir_all(&dir);
    for (rel, body) in files {
        let path = format!("{dir}/{rel}");
        let parent = &path[..path.rfind('/')?];
        if let Err(e) = fs::create_dir_all(parent).and_then(|()| fs::write(&path, body)) {
            note(&format!("writing {path}: {e}"));
            return None;
        }
    }
    Some(dir)
}

/// Gates a rung on the volume carrying a toolchain; `Err` is the verdict.
fn prefix() -> Result<String, bool> {
    if let Err(verdict) = *DEV_DISK {
        return Err(verdict);
    }
    toolchain().ok_or_else(|| {
        note("the volume carries no toolchain");
        true
    })
}

/// Rung 1. Every startup binds every relocation of rustc, `librustc_driver`,
/// `libLLVM` and `libstd` before `main`, so this is where eager binding's cost
/// at compiler scale shows.
fn toolchain_starts() -> bool {
    let prefix = match prefix() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(rustc) = run(
        &prefix,
        "/",
        &format!("{prefix}/bin/rustc"),
        &["--version"],
        &[("LD_DEBUG", "statistics")],
    ) else {
        return false;
    };
    if !rustc.ok("rustc --version") || !rustc.stdout.starts_with("rustc ") {
        return false;
    }
    let bound = rustc
        .stderr
        .lines()
        .find(|l| l.starts_with("ld.so: "))
        .unwrap_or("ld.so: no statistics");
    note(&format!(
        "{} in {} ms; {bound}",
        rustc.stdout.trim_end(),
        rustc.took.as_millis()
    ));
    ["cargo", "clang"].iter().all(|tool| {
        run(
            &prefix,
            "/",
            &format!("{prefix}/bin/{tool}"),
            &["--version"],
            &[],
        )
        .is_some_and(|r| r.ok(&format!("{tool} --version")))
    })
}

/// Rung 2: rustc compiles, links through `cc`, and the program runs.
fn rustc_links_a_program() -> bool {
    let prefix = match prefix() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(dir) = scratch(
        "rustc",
        &[(
            "hello.rs",
            "fn main() {\n    println!(\"hello from rustc\");\n}\n",
        )],
    ) else {
        return false;
    };
    let Some(built) = run(
        &prefix,
        &dir,
        &format!("{prefix}/bin/rustc"),
        &["hello.rs", "-o", "hello"],
        &[],
    ) else {
        return false;
    };
    if !built.ok("rustc hello.rs") {
        return false;
    }
    let Some(ran) = run(&prefix, &dir, &format!("{dir}/hello"), &[], &[]) else {
        return false;
    };
    note(&format!("rustc hello.rs in {} ms", built.took.as_millis()));
    ran.ok("hello") && ran.stdout == "hello from rustc\n"
}

const LADDER_MANIFEST: &str = "[package]
name = \"ladder\"
version = \"0.1.0\"
edition = \"2021\"

[dependencies]
ladder-macro = { path = \"macro\" }

[workspace]
";

const LADDER_BUILD: &str = "fn main() {
    println!(\"cargo:rustc-env=LADDER_BUILT_BY=build-script\");
}
";

const LADDER_MAIN: &str = "fn main() {
    println!(\"{} {}\", env!(\"LADDER_BUILT_BY\"), ladder_macro::answer!());
}
";

const MACRO_MANIFEST: &str = "[package]
name = \"ladder-macro\"
version = \"0.1.0\"
edition = \"2021\"

[lib]
proc-macro = true
";

const MACRO_LIB: &str = "use proc_macro::TokenStream;

#[proc_macro]
pub fn answer(_: TokenStream) -> TokenStream {
    \"6 * 7\".parse().unwrap()
}
";

/// Rung 3: cargo runs a build script, loads a proc macro into rustc with
/// `dlopen`, and drives both through its spawn and jobserver road.
fn cargo_builds_a_crate() -> bool {
    let prefix = match prefix() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(dir) = scratch(
        "cargo",
        &[
            ("Cargo.toml", LADDER_MANIFEST),
            ("build.rs", LADDER_BUILD),
            ("src/main.rs", LADDER_MAIN),
            ("macro/Cargo.toml", MACRO_MANIFEST),
            ("macro/src/lib.rs", MACRO_LIB),
        ],
    ) else {
        return false;
    };
    let Some(built) = run(
        &prefix,
        &dir,
        &format!("{prefix}/bin/cargo"),
        &["build", "--offline"],
        &[],
    ) else {
        return false;
    };
    if !built.ok("cargo build") {
        return false;
    }
    let Some(ran) = run(
        &prefix,
        &dir,
        &format!("{dir}/target/debug/ladder"),
        &[],
        &[],
    ) else {
        return false;
    };
    note(&format!("cargo build in {} ms", built.took.as_millis()));
    ran.ok("ladder") && ran.stdout == "build-script 42\n"
}

/// Rung 4: clang finds its resource directory and its config through its own
/// path, compiles C and C++, and links both against the prefix's sysroot.
fn clang_links_c_and_cxx() -> bool {
    let prefix = match prefix() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(dir) = scratch(
        "clang",
        &[
            (
                "hello.c",
                "#include <stdio.h>\nint main(void) {\n    puts(\"hello from clang\");\n    return 0;\n}\n",
            ),
            (
                "hello.cpp",
                "#include <iostream>\n#include <stdexcept>\nint main() {\n    try {\n        throw std::runtime_error(\"caught\");\n    } catch (const std::exception &e) {\n        std::cout << e.what() << '\\n';\n    }\n}\n",
            ),
        ],
    ) else {
        return false;
    };
    let cases = [
        ("cc", "hello.c", "helloc", "hello from clang\n"),
        ("c++", "hello.cpp", "hellocxx", "caught\n"),
    ];
    cases.iter().all(|&(driver, source, out, want)| {
        let Some(built) = run(
            &prefix,
            &dir,
            &format!("{prefix}/bin/{driver}"),
            &[source, "-o", out],
            &[],
        ) else {
            return false;
        };
        if !built.ok(&format!("{driver} {source}")) {
            return false;
        }
        run(&prefix, &dir, &format!("{dir}/{out}"), &[], &[])
            .is_some_and(|ran| ran.ok(out) && ran.stdout == want)
    })
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("devdisk_inventory_reads_back", devdisk_inventory_reads_back),
        ("devdisk_source_is_vendored", devdisk_source_is_vendored),
        (
            "devdisk_remounts_after_umount",
            devdisk_remounts_after_umount,
        ),
        ("toolchain_starts", toolchain_starts),
        ("rustc_links_a_program", rustc_links_a_program),
        ("cargo_builds_a_crate", cargo_builds_a_crate),
        ("clang_links_c_and_cxx", clang_links_c_and_cxx),
    ]);
}
