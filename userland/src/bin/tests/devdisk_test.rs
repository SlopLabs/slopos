use slopos_userland as _;

use slopos_abi::fs::MS_RDONLY;
use slopos_slibc::test_harness::note;
use slopos_tls_core::server::{Server, ServerConfig};
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs as fs_syscall;
use slopos_userland::tls::{self, CipherSuite};
use std::ffi::c_char;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::thread;
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

/// The tree that reached the guest needs no registry: the config in the
/// directory above it reads a vendor directory holding every locked crate by
/// checksum.
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
    let Some((above, _)) = source.rsplit_once('/') else {
        note(&format!("{source} has no directory above it to configure"));
        return false;
    };
    let base = format!("{MOUNT_POINT}/{above}");
    let config = match fs::read_to_string(format!("{base}/.cargo/config.toml")) {
        Ok(config) => config,
        Err(e) => {
            note(&format!("{above}/.cargo/config.toml: {e}"));
            return false;
        }
    };
    let vendor = match vendor_directory(&config) {
        Some(dir) if config.contains("replace-with = \"vendored-sources\"") => dir,
        _ => {
            note(&format!(
                "{above}/.cargo/config.toml reads no vendored sources"
            ));
            return false;
        }
    };
    let (lock, std_lock) = match (
        fs::read_to_string(format!("{root}/Cargo.lock")),
        fs::read_to_string(format!("{base}/{vendor}/library.lock")),
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
        let manifest = format!("{base}/{vendor}/{name}-{version}/.cargo-checksum.json");
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

/// The value of the marker's `<tag> <value>` line.
fn marker_entry(tag: &str) -> Option<String> {
    let text = fs::read_to_string(marker_path()).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(tag)?.strip_prefix(' ').map(str::to_owned))
}

/// The staged toolchain prefix, absolute, when the marker names one.
fn toolchain() -> Option<String> {
    marker_entry("toolchain").map(|rel| format!("{MOUNT_POINT}/{rel}"))
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
    if !rustc.ok("rustc --version") {
        return false;
    }
    let version = rustc.stdout.trim_end();
    let bound = rustc
        .stderr
        .lines()
        .find_map(|l| l.strip_prefix("ld.so: "))
        .unwrap_or("no statistics");
    note(&format!(
        "{version} in {} ms; {bound}",
        rustc.took.as_millis()
    ));
    let rust_lld = format!("{prefix}/lib/rustlib/x86_64-unknown-slopos/bin/rust-lld");
    let tools: [(String, &[&str]); 4] = [
        (format!("{prefix}/bin/cargo"), &["--version"]),
        (format!("{prefix}/bin/clang"), &["--version"]),
        (format!("{prefix}/bin/ld.lld"), &["--version"]),
        (rust_lld, &["-flavor", "gnu", "--version"]),
    ];
    tools.iter().all(|(tool, args)| {
        run(&prefix, "/", tool, args, &[]).is_some_and(|r| r.ok(&format!("{tool} --version")))
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

const FETCH_MAIN: &str = "fn main() {
    println!(\"{}\", greeting::greeting());
}
";

/// Rung 4: cargo fetches a `git` dependency from the volume's bare repository
/// through libgit2, into a fresh `CARGO_HOME` so it is never a cache hit. Not
/// `--offline`, which refuses every git fetch.
fn cargo_fetches_a_git_dependency() -> bool {
    let prefix = match prefix() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(repo) = marker_entry("git") else {
        note("the volume carries no git fixture");
        return true;
    };
    let url = format!("file://{MOUNT_POINT}/{repo}");
    let manifest = format!(
        "[package]\nname = \"fetch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ngreeting = {{ git = \"{url}\" }}\n\n[workspace]\n"
    );
    let Some(dir) = scratch(
        "git",
        &[
            ("Cargo.toml", manifest.as_str()),
            ("src/main.rs", FETCH_MAIN),
        ],
    ) else {
        return false;
    };
    let cargo_home = format!("{dir}/cargo-home");
    let Some(built) = run(
        &prefix,
        &dir,
        &format!("{prefix}/bin/cargo"),
        &["build"],
        &[
            ("CARGO_HOME", cargo_home.as_str()),
            ("CARGO_NET_OFFLINE", "false"),
            ("CARGO_NET_GIT_FETCH_WITH_CLI", "false"),
        ],
    ) else {
        return false;
    };
    if !built.ok("cargo build with a git dependency") {
        return false;
    }
    let lock = fs::read_to_string(format!("{dir}/Cargo.lock")).unwrap_or_default();
    if !lock.contains(&format!("source = \"git+{url}#")) {
        note(&format!("Cargo.lock does not pin greeting to {url}"));
        return false;
    }
    let Some(ran) = run(
        &prefix,
        &dir,
        &format!("{dir}/target/debug/fetch"),
        &[],
        &[],
    ) else {
        return false;
    };
    note(&format!(
        "cargo build with a git dependency in {} ms",
        built.took.as_millis()
    ));
    ran.ok("fetch") && ran.stdout == "fetched through libgit2\n"
}

/// Bounds every loopback read, so a client that never hangs up fails the rung.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Connection {
    offered_alpn: Vec<Vec<u8>>,
    requests: Vec<(String, u16)>,
    error: Option<String>,
}

/// What `www` holds at `path`, refusing a path that climbs out of it.
fn served_file(www: &str, path: &str) -> Option<Vec<u8>> {
    if !path.starts_with('/') || path.split('/').any(|s| s == "..") {
        return None;
    }
    fs::read(format!("{www}{path}")).ok()
}

/// One client's kept-alive HTTP/1.1 GETs.
fn serve_connection(mut sock: TcpStream, www: &str, chain: &[Vec<u8>], key: &[u8]) -> Connection {
    let mut conn = Connection::default();
    if let Err(e) = sock
        .set_nonblocking(false)
        .and_then(|()| sock.set_read_timeout(Some(IO_TIMEOUT)))
    {
        conn.error = Some(e.to_string());
        return conn;
    }
    let mut entropy = [0u8; 64];
    tls::random(&mut entropy);
    let mut server = Server::new(ServerConfig {
        chain,
        key,
        suites: &CipherSuite::ALL,
        alpn: &[b"http/1.1"],
        request_certificate: false,
        entropy,
    });
    let mut plain = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        while let Some(end) = plain.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&plain[..end]).into_owned();
            plain.drain(..end + 4);
            let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
            let (status, reason, body) = match served_file(www, &path) {
                Some(body) => (200, "OK", body),
                None => (404, "Not Found", Vec::new()),
            };
            let reply = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            if let Err(e) = server
                .write(reply.as_bytes())
                .and_then(|()| server.write(&body))
            {
                conn.error = Some(e.to_string());
                return conn;
            }
            conn.requests.push((path, status));
        }
        if let Err(e) = sock.write_all(&server.take_output()) {
            conn.error = Some(format!("write: {e}"));
            return conn;
        }
        if server.peer_closed() {
            return conn;
        }
        let n = match sock.read(&mut buf) {
            Ok(0) => return conn,
            Ok(n) => n,
            Err(e) => {
                conn.error = Some(format!("read: {e}"));
                return conn;
            }
        };
        let fed = server.read_tls(&buf[..n]);
        conn.offered_alpn = server.client_alpn().to_vec();
        if let Err(e) = fed {
            let _ = sock.write_all(&server.take_output());
            conn.error = Some(e.to_string());
            return conn;
        }
        loop {
            let k = server.read(&mut buf);
            if k == 0 {
                break;
            }
            plain.extend_from_slice(&buf[..k]);
        }
    }
}

/// A thread per connection, so one curl keeps alive never blocks the next.
fn serve_registry(
    listener: &TcpListener,
    www: &str,
    chain: &[Vec<u8>],
    key: &[u8],
    stop: &AtomicBool,
) -> Vec<Connection> {
    let served = Mutex::new(Vec::new());
    let record = |conn| {
        served
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(conn)
    };
    thread::scope(|s| {
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((sock, _)) => {
                    s.spawn(|| record(serve_connection(sock, www, chain, key)));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    record(Connection {
                        error: Some(format!("accept: {e}")),
                        ..Connection::default()
                    });
                    break;
                }
            }
        }
    });
    served.into_inner().unwrap_or_else(PoisonError::into_inner)
}

fn alpn_list(offered: &[Vec<u8>]) -> String {
    let names: Vec<_> = offered
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    names.join(", ")
}

/// Rung 5: cargo fetches a crate from the volume's sparse registry over
/// loopback TLS, through libcurl and OpenSSL, trusting the volume's test root.
/// The image's own CA bundle must refuse the same server first, or the
/// verification proves nothing.
fn cargo_fetches_over_https() -> bool {
    let prefix = match prefix() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(rel) = marker_entry("registry") else {
        note("the volume carries no registry");
        return true;
    };
    let root = format!("{MOUNT_POINT}/{rel}");
    let www = format!("{root}/www");
    let config = fs::read_to_string(format!("{www}/index/config.json")).unwrap_or_default();
    let Some(authority) = config
        .split("\"dl\":\"https://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .map(str::to_owned)
    else {
        note(&format!(
            "{www}/index/config.json names no https download URL"
        ));
        return false;
    };
    let (Ok(cert), Ok(key)) = (
        fs::read(format!("{root}/server.der")),
        fs::read(format!("{root}/server.key")),
    ) else {
        note("the registry's server certificate or key is missing");
        return false;
    };
    let chain = [cert];
    let listener = match TcpListener::bind(authority.as_str())
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
    {
        Ok(l) => l,
        Err(e) => {
            note(&format!("bind {authority}: {e}"));
            return false;
        }
    };
    let index = format!("sparse+https://{authority}/index/");
    let manifest = "[package]\nname = \"fetch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ngreeting = { version = \"0.1.0\", registry = \"devdisk\" }\n\n[workspace]\n";
    let Some(dir) = scratch(
        "https",
        &[("Cargo.toml", manifest), ("src/main.rs", FETCH_MAIN)],
    ) else {
        return false;
    };
    let cargo = format!("{prefix}/bin/cargo");
    let untrusting_home = format!("{dir}/untrusting-home");
    let cargo_home = format!("{dir}/cargo-home");
    let ca = format!("{root}/ca.pem");
    let registry = [
        ("CARGO_REGISTRIES_DEVDISK_INDEX", index.as_str()),
        ("CARGO_NET_OFFLINE", "false"),
        ("CARGO_NET_RETRY", "0"),
    ];
    let stop = AtomicBool::new(false);
    let (refused, built, served) = thread::scope(|s| {
        let server = s.spawn(|| serve_registry(&listener, &www, &chain, &key, &stop));
        let refused = run(
            &prefix,
            &dir,
            &cargo,
            &["fetch"],
            &[&registry[..], &[("CARGO_HOME", untrusting_home.as_str())]].concat(),
        );
        let built = run(
            &prefix,
            &dir,
            &cargo,
            &["build"],
            &[
                &registry[..],
                &[
                    ("CARGO_HOME", cargo_home.as_str()),
                    ("CARGO_HTTP_CAINFO", ca.as_str()),
                ],
            ]
            .concat(),
        );
        stop.store(true, Ordering::Release);
        (refused, built, server.join().unwrap_or_default())
    });
    let (Some(refused), Some(built)) = (refused, built) else {
        return false;
    };
    let mut log = String::new();
    for c in &served {
        log += &format!(
            "connection: ALPN offered [{}], {:?}, {}\n",
            alpn_list(&c.offered_alpn),
            c.requests,
            c.error.as_deref().unwrap_or("closed cleanly")
        );
    }
    log += &format!(
        "cargo fetch without the root, exit {:?}:\n{}\ncargo build with it, exit {:?}:\n{}\n",
        refused.code, refused.stderr, built.code, built.stderr
    );
    let log_path = format!("{dir}/https.log");
    let _ = fs::write(&log_path, log);
    let last = |text: &str| {
        text.trim_end()
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    const CURLE_PEER_FAILED_VERIFICATION: &str = "[60]";
    if refused.code == Some(0) || !refused.stderr.contains(CURLE_PEER_FAILED_VERIFICATION) {
        note(&format!(
            "without the root, cargo exited {:?}: {} (see {log_path})",
            refused.code,
            last(&refused.stderr)
        ));
        return false;
    }
    if built.code != Some(0) {
        note(&format!(
            "cargo build exited {:?}: {} (see {log_path})",
            built.code,
            last(&built.stderr)
        ));
        return false;
    }
    let lock = fs::read_to_string(format!("{dir}/Cargo.lock")).unwrap_or_default();
    if !lock.contains(&format!("source = \"{index}\"")) {
        note(&format!("Cargo.lock does not pin greeting to {index}"));
        return false;
    }
    let requests: Vec<&(String, u16)> = served.iter().flat_map(|c| &c.requests).collect();
    for want in [
        "/index/config.json",
        "/index/gr/ee/greeting",
        "/crates/greeting-0.1.0.crate",
    ] {
        if !requests
            .iter()
            .any(|(path, status)| path == want && *status == 200)
        {
            note(&format!("the registry never served {want}: {requests:?}"));
            return false;
        }
    }
    let serving: Vec<&Connection> = served.iter().filter(|c| !c.requests.is_empty()).collect();
    if let Some(c) = serving
        .iter()
        .find(|c| !c.offered_alpn.iter().any(|p| p == b"h2"))
    {
        note(&format!(
            "libcurl offered ALPN [{}], without h2: nghttp2 is not in it",
            alpn_list(&c.offered_alpn)
        ));
        return false;
    }
    let Some(ran) = run(
        &prefix,
        &dir,
        &format!("{dir}/target/debug/fetch"),
        &[],
        &[],
    ) else {
        return false;
    };
    note(&format!(
        "built in {} ms, {} GETs/{} conn, ALPN [{}] -> http/1.1; test root refused in {} ms",
        built.took.as_millis(),
        requests.len(),
        serving.len(),
        alpn_list(&serving[0].offered_alpn),
        refused.took.as_millis(),
    ));
    ran.ok("fetch") && ran.stdout == "fetched over https\n"
}

/// Rung 6: clang finds its resource directory and its config through its own
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

/// Rung 7: git reads the clone the volume was seeded with and reaches the
/// checkout the host serves; the host holds a volume it has just created to a
/// clean status.
fn git_reads_the_clone_and_reaches_the_host() -> bool {
    let prefix = match prefix() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(source) = marker_entry("source") else {
        note("the volume carries no source tree");
        return true;
    };
    let root = format!("{MOUNT_POINT}/{source}");
    let git = format!("{prefix}/bin/git");
    let Some(status) = run(&prefix, &root, &git, &["status", "--porcelain"], &[]) else {
        return false;
    };
    if !status.ok("git status") {
        return false;
    }
    let tree = match status.stdout.lines().count() {
        0 => format!("clean in {} ms", status.took.as_millis()),
        n => format!(
            "{n} changes, first {:?}",
            status.stdout.lines().next().unwrap_or_default()
        ),
    };
    let Some(remote) = run(
        &prefix,
        &root,
        &git,
        &["ls-remote", "origin", "HEAD"],
        &[("GIT_TERMINAL_PROMPT", "0")],
    ) else {
        return false;
    };
    if !remote.ok("git ls-remote origin HEAD") {
        return false;
    }
    note(&format!(
        "git status: {tree}; origin HEAD is {}",
        remote.stdout.split_whitespace().next().unwrap_or("-")
    ));
    true
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
        (
            "cargo_fetches_a_git_dependency",
            cargo_fetches_a_git_dependency,
        ),
        ("cargo_fetches_over_https", cargo_fetches_over_https),
        ("clang_links_c_and_cxx", clang_links_c_and_cxx),
        (
            "git_reads_the_clone_and_reaches_the_host",
            git_reads_the_clone_and_reaches_the_host,
        ),
    ]);
}
