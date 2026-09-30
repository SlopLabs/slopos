use slopos_userland as _;

use slopos_abi::fs::DEFAULT_PATH;
use slopos_slibc::test_harness::note;
use slopos_tls_core::pem;
use slopos_tls_core::server::{Server, ServerConfig};
use slopos_tls_core::testpki::{self, Key, Profile};
use slopos_userland::selfhost::{SCRATCH, SOURCE};
use slopos_userland::tls::{self, CipherSuite};
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

const PREFIX: &str = "/usr/local";
/// What the host installed there: an `identity` line, then
/// `<kind> <size or -> <path>`.
const MANIFEST: &str = "/var/lib/slopos/trees/usr_local";
/// What the host seeds on the self-hosting root for the ladder: a bare
/// repository of one crate and a sparse registry of one crate.
const FIXTURES: &str = "/srv/ladder";

/// Gates a rung on the root carrying a toolchain; `Err` is the verdict, a pass
/// with a note on any root but the self-hosting one.
fn toolchain() -> Result<&'static str, bool> {
    if Path::new(PREFIX).join("bin/rustc").is_file() {
        return Ok(PREFIX);
    }
    note("the root carries no toolchain at /usr/local");
    Err(true)
}

/// Every entry the host installed is there as the kind, and each file at the
/// size, it installed: a toolchain whose `lib/` never arrived, or arrived
/// short, says so here rather than as a link error in a guest build hours
/// later.
fn toolchain_matches_its_manifest() -> bool {
    if let Err(verdict) = toolchain() {
        return verdict;
    }
    let text = match fs::read_to_string(MANIFEST) {
        Ok(text) => text,
        Err(e) => {
            note(&format!("{MANIFEST}: {e}"));
            return false;
        }
    };
    let mut counts = [0usize; 3];
    for line in text.lines().filter(|l| !l.starts_with("identity ")) {
        let mut fields = line.splitn(3, ' ');
        let (Some(kind), Some(size), Some(rel)) = (fields.next(), fields.next(), fields.next())
        else {
            note(&format!("malformed manifest line {line:?}"));
            return false;
        };
        let path = format!("{PREFIX}/{rel}");
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                note(&format!("{path}: {e}"));
                return false;
            }
        };
        let (slot, held) = match kind {
            "f" => (0, meta.is_file() && size.parse() == Ok(meta.len())),
            "d" => (1, meta.is_dir()),
            "l" => (2, meta.file_type().is_symlink()),
            _ => (0, false),
        };
        if !held {
            note(&format!(
                "{path} is not the {kind} of {size} bytes the host installed"
            ));
            return false;
        }
        counts[slot] += 1;
    }
    if counts[0] == 0 {
        note(&format!("{MANIFEST} lists no file"));
        return false;
    }
    note(&format!(
        "{PREFIX}: {} files, {} directories and {} links as installed",
        counts[0], counts[1], counts[2]
    ));
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

/// The workspace needs no registry: the config in the directory above it
/// reads a vendor directory holding every locked crate by checksum.
fn source_is_vendored() -> bool {
    if !Path::new(SOURCE).join(".git").is_dir() {
        note("the root carries no workspace");
        return true;
    }
    grade_source(SOURCE)
}

fn grade_source(root: &str) -> bool {
    let Some((above, _)) = root.rsplit_once('/') else {
        note(&format!("{root} has no directory above it to configure"));
        return false;
    };
    let config = match fs::read_to_string(format!("{above}/.cargo/config.toml")) {
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
        fs::read_to_string(format!("{above}/{vendor}/library.lock")),
    ) {
        (Ok(l), Ok(s)) => (l, s),
        (l, s) => {
            note(&format!(
                "{root} lacks a lockfile: {:?} {:?}",
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
        let manifest = format!("{above}/{vendor}/{name}-{version}/.cargo-checksum.json");
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

/// Runs `program` in `dir` with the `PATH` a developer's shell exports, the
/// default one, and a `CARGO_HOME` of the ladder's own. rustc hands its linker
/// its own tools directories ahead of the `PATH` it inherited, and only them
/// when it inherited none, so the ladder cannot leave `PATH` unset.
fn run(dir: &str, program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Ran> {
    let started = Instant::now();
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("PATH", OsStr::from_bytes(DEFAULT_PATH.to_bytes()))
        .env("CARGO_HOME", format!("{SCRATCH}/ladder/cargo-home"))
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
    let dir = format!("{SCRATCH}/ladder/{rung}");
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

/// Rung 1. Every startup binds every relocation of rustc, `librustc_driver`,
/// `libLLVM` and `libstd` before `main`, so this is where eager binding's cost
/// at compiler scale shows.
fn toolchain_starts() -> bool {
    let prefix = match toolchain() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let Some(rustc) = run(
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
        run("/", tool, args, &[]).is_some_and(|r| r.ok(&format!("{tool} --version")))
    })
}

/// Rung 2: rustc compiles, links through `cc`, and the program runs.
fn rustc_links_a_program() -> bool {
    let prefix = match toolchain() {
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
    let Some(ran) = run(&dir, &format!("{dir}/hello"), &[], &[]) else {
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
    let prefix = match toolchain() {
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
    let Some(ran) = run(&dir, &format!("{dir}/target/debug/ladder"), &[], &[]) else {
        return false;
    };
    note(&format!("cargo build in {} ms", built.took.as_millis()));
    ran.ok("ladder") && ran.stdout == "build-script 42\n"
}

const FETCH_MAIN: &str = "fn main() {
    println!(\"{}\", greeting::greeting());
}
";

/// Rung 4: cargo fetches a `git` dependency from the root's bare repository
/// through libgit2, into a fresh `CARGO_HOME` so it is never a cache hit. Not
/// `--offline`, which refuses every git fetch.
fn cargo_fetches_a_git_dependency() -> bool {
    let prefix = match toolchain() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let repo = format!("{FIXTURES}/git/greeting.git");
    if !Path::new(&repo).is_dir() {
        note("the root carries no git fixture");
        return true;
    }
    let url = format!("file://{repo}");
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
    let Some(ran) = run(&dir, &format!("{dir}/target/debug/fetch"), &[], &[]) else {
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

/// Rung 5: cargo fetches a crate from the root's sparse registry over
/// loopback TLS, through libcurl and Mbed TLS, trusting the root's test root.
/// The image's own CA bundle must refuse the same server first, or the
/// verification proves nothing.
fn cargo_fetches_over_https() -> bool {
    let prefix = match toolchain() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let root = format!("{FIXTURES}/registry");
    if !Path::new(&root).is_dir() {
        note("the root carries no registry");
        return true;
    }
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
    let manifest = "[package]\nname = \"fetch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ngreeting = { version = \"0.1.0\", registry = \"ladder\" }\n\n[workspace]\n";
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
        ("CARGO_REGISTRIES_LADDER_INDEX", index.as_str()),
        ("CARGO_NET_OFFLINE", "false"),
        ("CARGO_NET_RETRY", "0"),
    ];
    let stop = AtomicBool::new(false);
    let (refused, built, served) = thread::scope(|s| {
        let server = s.spawn(|| serve_registry(&listener, &www, &chain, &key, &stop));
        let refused = run(
            &dir,
            &cargo,
            &["fetch"],
            &[&registry[..], &[("CARGO_HOME", untrusting_home.as_str())]].concat(),
        );
        let built = run(
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
    let Some(ran) = run(&dir, &format!("{dir}/target/debug/fetch"), &[], &[]) else {
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
    let prefix = match toolchain() {
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
        run(&dir, &format!("{dir}/{out}"), &[], &[])
            .is_some_and(|ran| ran.ok(out) && ran.stdout == want)
    })
}

/// Rung 7: git reads the workspace the root was seeded with and reaches the
/// checkout the host serves; the host holds a root it has just seeded to a
/// clean status.
fn git_reads_the_clone_and_reaches_the_host() -> bool {
    let prefix = match toolchain() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    if !Path::new(SOURCE).join(".git").is_dir() {
        note("the root carries no workspace");
        return true;
    }
    let root = SOURCE;
    let git = format!("{prefix}/bin/git");
    let Some(status) = run(root, &git, &["status", "--porcelain"], &[]) else {
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
        root,
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

const GITHUB_REMOTE: &str = "https://github.com/SlopLabs/slopos";

/// The `fatal:` line git ended on, if it failed with one.
fn fatal_line(ran: &Ran) -> &str {
    ran.stderr
        .lines()
        .rfind(|l| l.starts_with("fatal:"))
        .unwrap_or_default()
}

/// `git ls-remote <url> HEAD`'s commit, when it printed one.
fn advertised_head(ran: &Ran) -> Option<String> {
    ran.stdout
        .split_whitespace()
        .next()
        .filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
}

/// What a rung ran, written beside its work however the rung returns.
struct Transcript {
    path: String,
    text: String,
}

impl Transcript {
    fn record(&mut self, args: &[&str], ran: &Ran) {
        self.text += &format!(
            "$ git {}\nexit {:?} in {} ms\n{}{}\n",
            args.join(" "),
            ran.code,
            ran.took.as_millis(),
            ran.stdout,
            ran.stderr
        );
    }
}

impl Drop for Transcript {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.text);
    }
}

/// Rung 8: git clones from GitHub over HTTPS — the kernel's resolver, then
/// git-remote-https, libcurl, nghttp2 and Mbed TLS — trusting the image's CA
/// bundle. A root minted here must be refused first, or the trust proves
/// nothing.
fn git_clones_over_https() -> bool {
    let prefix = match toolchain() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let dir = format!("{SCRATCH}/ladder/https-git");
    let _ = fs::remove_dir_all(&dir);
    let foreign = format!("{dir}/foreign-root.pem");
    let key = Key::from_seed(b"ladder foreign root");
    let root = testpki::issue(
        &Profile::ca("Ladder Foreign Root"),
        &key,
        "Ladder Foreign Root",
        &key,
        1,
    );
    if let Err(e) =
        fs::create_dir_all(&dir).and_then(|()| fs::write(&foreign, pem::encode_certificate(&root)))
    {
        note(&format!("writing {foreign}: {e}"));
        return false;
    }
    let mut transcript = Transcript {
        path: format!("{dir}/https.log"),
        text: String::new(),
    };
    let log = transcript.path.clone();
    let mut git = |cwd: &str, args: &[&str], env: &[(&str, &str)]| {
        let traced = [
            &[
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_TRACE_CURL", "1"),
                ("GIT_TRACE_CURL_NO_DATA", "1"),
                // A stall fails the rung in a minute, not the boot's silence budget.
                ("GIT_HTTP_LOW_SPEED_LIMIT", "1000"),
                ("GIT_HTTP_LOW_SPEED_TIME", "60"),
            ][..],
            env,
        ]
        .concat();
        let ran = run(cwd, &format!("{prefix}/bin/git"), args, &traced)?;
        transcript.record(args, &ran);
        Some(ran)
    };
    let ls_remote = ["ls-remote", GITHUB_REMOTE, "HEAD"];

    let Some(refused) = git(&dir, &ls_remote, &[("GIT_SSL_CAINFO", foreign.as_str())]) else {
        return false;
    };
    const NOT_TRUSTED: &str = "not correctly signed by the trusted CA";
    if refused.code == Some(0) || !refused.stderr.contains(NOT_TRUSTED) {
        note(&format!(
            "under a foreign root, git ls-remote exited {:?} without refusing the certificate: {} (see {log})",
            refused.code,
            fatal_line(&refused)
        ));
        return false;
    }

    let Some(before) = git(&dir, &ls_remote, &[]) else {
        return false;
    };
    let Some(advertised) = advertised_head(&before).filter(|_| before.code == Some(0)) else {
        note(&format!(
            "git ls-remote exited {:?}: {} (see {log})",
            before.code,
            fatal_line(&before)
        ));
        return false;
    };
    let negotiated: Vec<String> = before
        .stderr
        .lines()
        .filter_map(|l| {
            let tls = l
                .split_once("mbedTLS: ")
                .and_then(|(_, rest)| rest.split_once(" Handshake complete, cipher is "))
                .map(|(version, cipher)| format!("{version} {cipher}"));
            tls.or_else(|| {
                l.split_once("ALPN: server accepted ")
                    .map(|(_, proto)| proto.to_owned())
            })
        })
        .collect();
    if !negotiated.iter().any(|p| p == "h2") {
        note(&format!(
            "GitHub did not take HTTP/2 through libcurl's nghttp2: [{}] (see {log})",
            negotiated.join(", ")
        ));
        return false;
    }

    let Some(cloned) = git(
        &dir,
        &["clone", "--depth", "1", GITHUB_REMOTE, "slopos"],
        &[],
    ) else {
        return false;
    };
    if cloned.code != Some(0) {
        note(&format!(
            "git clone exited {:?}: {} (see {log})",
            cloned.code,
            fatal_line(&cloned)
        ));
        return false;
    }
    let checkout = format!("{dir}/slopos");
    let (Some(rev), Some(status), Some(fsck)) = (
        git(&checkout, &["rev-parse", "HEAD"], &[]),
        git(&checkout, &["status", "--porcelain"], &[]),
        git(&checkout, &["fsck", "--no-progress"], &[]),
    ) else {
        return false;
    };
    if [&rev, &status, &fsck].iter().any(|r| r.code != Some(0)) || !status.stdout.is_empty() {
        note(&format!("the clone does not read back clean (see {log})"));
        return false;
    }
    let rev = rev.stdout.trim();
    // A push landing between the advertisement and the clone moves HEAD once.
    if rev != advertised
        && git(&dir, &ls_remote, &[])
            .as_ref()
            .and_then(advertised_head)
            .as_deref()
            != Some(rev)
    {
        note(&format!(
            "cloned {rev}, but {GITHUB_REMOTE} advertised {advertised}"
        ));
        return false;
    }
    note(&format!(
        "{GITHUB_REMOTE} at {} cloned in {} ms ({}); a foreign root refused in {} ms",
        &rev[..12],
        cloned.took.as_millis(),
        negotiated.join(", "),
        refused.took.as_millis()
    ));
    true
}

/// Rung 9: in the clone rung 8 made, which no vendored configuration reaches,
/// cargo resolves both lockfiles from crates.io over HTTPS as the host's cargo
/// does: the workspace's, and the standard library's that `-Zbuild-std`
/// reads. A fresh `CARGO_HOME`, so every crate is a download; cargo holds each
/// to the checksum its lockfile records.
fn cargo_resolves_the_lockfiles_from_crates_io() -> bool {
    let prefix = match toolchain() {
        Ok(p) => p,
        Err(verdict) => return verdict,
    };
    let clone = format!("{SCRATCH}/ladder/https-git/slopos");
    let lock = match fs::read_to_string(format!("{clone}/Cargo.lock")) {
        Ok(lock) => lock,
        Err(e) => {
            note(&format!("no clone from rung 8 at {clone}: {e}"));
            return false;
        }
    };
    let cargo_home = format!("{SCRATCH}/ladder/crates-io-home");
    let _ = fs::remove_dir_all(&cargo_home);
    let Some(fetched) = run(
        &clone,
        &format!("{prefix}/bin/cargo"),
        &[
            "fetch",
            "--locked",
            "-Zbuild-std=core,alloc",
            "--target",
            "targets/x86_64-slos.json",
        ],
        &[
            ("CARGO_HOME", cargo_home.as_str()),
            ("CARGO_NET_OFFLINE", "false"),
        ],
    ) else {
        return false;
    };
    if !fetched.ok("cargo fetch -Zbuild-std") {
        return false;
    }
    let Some(cache) = fs::read_dir(format!("{cargo_home}/registry/cache"))
        .ok()
        .and_then(|mut dirs| dirs.find_map(|d| d.ok()))
        .map(|d| d.path())
    else {
        note("cargo fetch left no registry cache");
        return false;
    };
    if !cache.to_string_lossy().contains("index.crates.io-") {
        note(&format!(
            "the crates came from {}, not crates.io",
            cache.display()
        ));
        return false;
    }
    let workspace = registry_packages(&lock);
    if let Some((name, version, _)) = workspace
        .iter()
        .find(|(name, version, _)| !cache.join(format!("{name}-{version}.crate")).is_file())
    {
        note(&format!("{name} {version} is not in {}", cache.display()));
        return false;
    }
    let crates = fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0);
    if crates <= workspace.len() {
        note(&format!(
            "{crates} crates for the workspace's {}: -Zbuild-std fetched nothing of its own",
            workspace.len()
        ));
        return false;
    }
    note(&format!(
        "{crates} crates from crates.io, the workspace's {} among them, in {} ms",
        workspace.len(),
        fetched.took.as_millis()
    ));
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "toolchain_matches_its_manifest",
            toolchain_matches_its_manifest,
        ),
        ("source_is_vendored", source_is_vendored),
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
        ("git_clones_over_https", git_clones_over_https),
        (
            "cargo_resolves_the_lockfiles_from_crates_io",
            cargo_resolves_the_lockfiles_from_crates_io,
        ),
    ]);
}
