//! A base activated while the system runs: a fixture packed from the running
//! base that drops this program for a script and adds a few bytes to privd,
//! then the running base again.
//!
//! Across the fixture: a new exec runs its bytes, this program no longer
//! resolves for a new exec while a process of it started before is still
//! attested by its path in its own generation, and init starts privd again
//! from the fixture, which serves. Across the base again: privd starts again
//! once more and this program resolves again.
//!
//! The runner hands this binary every capability as uid 0. The fixture is
//! packed from the manifest of the running base and what the base
//! directories hold, since nothing outside them is reachable on a disk root
//! or served from a live generation; the base again carries the running
//! manifest itself, so what runs after reads as the base that booted.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slopos_abi::fs::{BASE_DIRS, O_RDWR, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::syscall::{POLLIN, UserPollFd};
use slopos_boot_core::base::{MANIFEST_PATH, ManifestLine, manifest_lines};
use slopos_boot_core::newc::Record;
use slopos_priv_core::SOCKET as PRIVD;
use slopos_priv_core::wire::{Exit, Reply, Request};
use slopos_slibc::test_harness::note;
use slopos_tls_core::hash::{Hash, Sha256};
use slopos_userland::init_control;
use slopos_userland::privd::{Connection, request_id};
use slopos_userland::syscall::base::base_activate;
use slopos_userland::syscall::{fs, net};

const SELF: &str = "/bin/live_base_test";
const PROBE: &str = "/bin/live-base-probe";
const PROBE_SAYS: &str = "live-base-fixture";
const PRIVD_PROGRAM: &str = "/sbin/privd";
/// `argv[1]` of a child that connects to `argv[2]` once its stdin says so.
const PEER: &str = "--peer";
const SOCKET: &str = "/tmp/live-base-test.sock";
/// Long enough for privd to drain and start again under TCG.
const DEADLINE: Duration = Duration::from_secs(120);

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut digest = [0u8; 32];
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finish_into(&mut digest);
    digest
}

fn in_base(path: &str) -> bool {
    BASE_DIRS.iter().any(|dir| {
        path == *dir
            || path
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// What a record of the packed base holds in place of the running base's.
enum Change<'a> {
    Keep,
    Drop,
    Bytes(&'a [u8]),
}

/// The running base as its manifest lists it, each path's record taken from
/// what the base directories hold unless `change` says otherwise, `added`
/// after it, and last a manifest of what it holds, or `verbatim`.
fn pack<'a>(
    manifest: &str,
    change: impl Fn(&str) -> Change<'a>,
    added: &[(&str, u32, &[u8])],
    verbatim: Option<&str>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut listed = String::new();
    let mut put = |path: &str, mode: u32, data: &[u8], listed: &mut String| {
        let record = Record::new(path.as_bytes(), mode, data).ok_or("a record too large")?;
        record
            .pieces()
            .iter()
            .for_each(|p| out.extend_from_slice(p));
        if verbatim.is_some() {
            return Ok(());
        }
        ManifestLine {
            digest: sha256(data),
            mode,
            path,
        }
        .write(listed)
        .map_err(|_| format!("{path}: no manifest line"))
    };
    for line in manifest_lines(manifest) {
        let line = line.map_err(|bad| format!("{MANIFEST_PATH}: line {}", bad.line))?;
        let kind = line.mode & S_IFMT;
        if kind != S_IFDIR && !in_base(line.path) {
            continue;
        }
        let data = match (kind, change(line.path)) {
            (_, Change::Drop) => continue,
            (_, Change::Bytes(bytes)) => bytes.to_vec(),
            (S_IFDIR, Change::Keep) => Vec::new(),
            (S_IFLNK, Change::Keep) => std::fs::read_link(line.path)
                .map_err(|e| format!("{}: {e}", line.path))?
                .into_os_string()
                .into_encoded_bytes(),
            (S_IFREG, Change::Keep) => {
                std::fs::read(line.path).map_err(|e| format!("{}: {e}", line.path))?
            }
            _ => return Err(format!("{}: a record of no kind the base holds", line.path)),
        };
        put(line.path, line.mode, &data, &mut listed)?;
    }
    for &(path, mode, data) in added {
        put(path, mode, data, &mut listed)?;
    }
    let mut last = Vec::new();
    let listed = verbatim.unwrap_or(&listed);
    Record::new(MANIFEST_PATH.as_bytes(), S_IFREG | 0o644, listed.as_bytes())
        .ok_or("the manifest is too large")?
        .pieces()
        .iter()
        .for_each(|p| last.extend_from_slice(p));
    out.extend_from_slice(&last);
    Record::trailer()
        .pieces()
        .iter()
        .for_each(|p| out.extend_from_slice(p));
    Ok(out)
}

fn until(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// The privd listening now once it is another than `old`.
fn privd_replaced(old: u32) -> Option<u32> {
    let mut new = None;
    until(DEADLINE, || {
        new = init_control::listener(PRIVD);
        new.is_some_and(|pid| pid != old)
    })
    .then_some(new)
    .flatten()
}

/// Whether privd runs `prof status` for this uid-0 process, which the
/// policy lets it ask without consent.
fn privd_serves() -> bool {
    let Ok(mut privd) = Connection::open_within(DEADLINE) else {
        return false;
    };
    let Ok(null) = fs::open_cstr(c"/dev/null", O_RDWR) else {
        return false;
    };
    let request = Request {
        id: request_id(),
        action: "org.slopos.prof".into(),
        args: vec![b"status".to_vec()],
        cwd: false,
    };
    if privd
        .send(&request, &[null.raw(), null.raw(), null.raw()])
        .is_err()
    {
        return false;
    }
    loop {
        match privd.reply(request.id) {
            Ok(Some(Reply::Exited(exit))) => return exit == Exit::Code(0),
            Ok(Some(Reply::Denied { reason, .. })) => {
                note(&format!("prof status was refused: {reason}"));
                return false;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return false,
        }
    }
}

/// A process of this program, started before a switch, connected after it.
struct Peer {
    child: std::process::Child,
    listener: slopos_userland::syscall::OwnedFd,
}

impl Peer {
    fn start() -> Result<Peer, String> {
        let _ = std::fs::remove_file(SOCKET);
        let listener = net::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC as u16, 0)
            .map_err(|e| format!("socket: {e}"))?;
        net::bind_unix(listener.raw(), SOCKET.as_bytes()).map_err(|e| format!("bind: {e}"))?;
        net::listen(listener.raw(), 1).map_err(|e| format!("listen: {e}"))?;
        let child = Command::new(SELF)
            .args([PEER, SOCKET])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{SELF} {PEER}: {e}"))?;
        Ok(Peer { child, listener })
    }

    /// The program the kernel attests the peer runs, once it has connected.
    fn attested(&mut self) -> Result<Option<String>, String> {
        let stdin = self.child.stdin.as_mut().ok_or("no stdin")?;
        stdin
            .write_all(b"g")
            .map_err(|e| format!("the peer's stdin: {e}"))?;
        let mut fds = [UserPollFd {
            fd: self.listener.raw(),
            events: POLLIN,
            revents: 0,
        }];
        if !matches!(fs::poll(&mut fds, DEADLINE.as_millis() as i64), Ok(n) if n > 0) {
            return Err("the peer did not connect".into());
        }
        let conn = net::accept(self.listener.raw(), None).map_err(|e| format!("accept: {e}"))?;
        let program = net::peer_program(conn.raw()).map_err(|e| format!("SO_PEERPROGRAM: {e}"))?;
        Ok(program.map(|p| String::from_utf8_lossy(&p).into_owned()))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(SOCKET);
    }
}

fn peer(path: &str) -> ! {
    let mut go = [0u8; 1];
    let _ = std::io::stdin().read(&mut go);
    let connected = net::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC as u16, 0)
        .ok()
        .filter(|fd| net::connect_unix(fd.raw(), path.as_bytes()).is_ok());
    let _ = std::io::stdin().read(&mut go);
    drop(connected);
    std::process::exit(0);
}

fn activate(image: &[u8], what: &str) -> Result<u32, String> {
    base_activate(image).map_err(|e| format!("activating {what}: {e}"))
}

fn a_base_runs_once_activated() -> bool {
    match run() {
        Ok(()) => true,
        Err(why) => {
            note(&why);
            false
        }
    }
}

fn run() -> Result<(), String> {
    let manifest =
        std::fs::read_to_string(MANIFEST_PATH).map_err(|e| format!("{MANIFEST_PATH}: {e}"))?;
    let original_privd =
        std::fs::read(PRIVD_PROGRAM).map_err(|e| format!("{PRIVD_PROGRAM}: {e}"))?;
    let mut fixture_privd = original_privd.clone();
    fixture_privd.extend_from_slice(b"\0live-base-test");
    let original_self = std::fs::read(SELF).map_err(|e| format!("{SELF}: {e}"))?;
    let script = format!("#!/bin/shell\necho {PROBE_SAYS}\n");
    let fixture = pack(
        &manifest,
        |path| match path {
            PRIVD_PROGRAM => Change::Bytes(&fixture_privd),
            SELF => Change::Drop,
            _ => Change::Keep,
        },
        &[(PROBE, S_IFREG | 0o755, script.as_bytes())],
        None,
    )?;

    let mut peer = Peer::start()?;
    let privd = init_control::listener(PRIVD).ok_or("no privd listens")?;
    let generation = activate(&fixture, "the fixture")?;
    drop(fixture);
    println!("LIVE-BASE: the fixture is generation {generation}");
    let fixture_ran = fixture_runs(&mut peer, privd);
    drop(peer);
    let restored = restore(&manifest, &original_privd, &original_self);
    fixture_ran.and(restored)
}

/// Across the fixture: the probe runs, this program is gone for a new exec
/// and attested for one started before, and privd is another that serves.
fn fixture_runs(peer: &mut Peer, privd: u32) -> Result<(), String> {
    let said = Command::new(PROBE)
        .output()
        .map_err(|e| format!("{PROBE}: {e}"))?;
    if String::from_utf8_lossy(&said.stdout).trim() != PROBE_SAYS {
        return Err(format!("{PROBE} said {:?}", said.stdout));
    }
    if std::fs::metadata(SELF).is_ok() {
        return Err(format!("{SELF} still resolves in the fixture"));
    }
    match peer.attested()? {
        Some(program) if program == SELF => {}
        other => {
            return Err(format!(
                "a process started before was attested as {other:?}"
            ));
        }
    }
    privd_replaced(privd).ok_or("privd was not started again for the fixture")?;
    if !privd_serves() {
        return Err("the fixture's privd does not serve".into());
    }
    Ok(())
}

/// The running base again, its manifest the very one it booted with,
/// whatever the fixture did.
fn restore(manifest: &str, privd: &[u8], this: &[u8]) -> Result<(), String> {
    let before = init_control::listener(PRIVD);
    let original = pack(
        manifest,
        |path| match path {
            PRIVD_PROGRAM => Change::Bytes(privd),
            SELF => Change::Bytes(this),
            PROBE => Change::Drop,
            _ => Change::Keep,
        },
        &[],
        Some(manifest),
    )?;
    activate(&original, "the base again")?;
    drop(original);
    if !std::fs::metadata(SELF).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        || std::fs::metadata(PROBE).is_ok()
    {
        return Err("the base again does not hold what it held".into());
    }
    if let Some(before) = before {
        privd_replaced(before).ok_or("privd was not started again for the base")?;
    }
    if !privd_serves() {
        return Err("privd does not serve on the base again".into());
    }
    Ok(())
}

const CASES: &[(&str, fn() -> bool)] =
    &[("a_base_runs_once_activated", a_base_runs_once_activated)];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some(PEER), Some(path)) => peer(path),
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
