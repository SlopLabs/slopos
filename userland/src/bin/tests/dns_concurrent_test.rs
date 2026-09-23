//! Lookups run side by side, against a nameserver this test runs on loopback.
//!
//! The stub withholds one name's reply until every other lookup has returned,
//! so a resolver that serialises lookups machine-wide cannot pass. It also
//! sends forged replies ahead of one real answer, and answers one name with
//! NXDOMAIN, which must end the lookup without a retry.

use slopos_userland as _;

use std::ffi::CStr;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use slopos_abi::net::{
    NET_IFINDEX_NONE, NET_Q_RESOLVER, NET_RESOLVER_SRC_STATIC, UserResolver, UserResolverReq,
};
use slopos_slibc::net::dns::{
    AI_ADDRCONFIG, AI_NUMERICHOST, AI_NUMERICSERV, AddrInfo, EAI_BADFLAGS, EAI_NONAME, EAI_SERVICE,
    freeaddrinfo, getaddrinfo,
};
use slopos_slibc::net::{AF_INET, IPPROTO_TCP, IPPROTO_UDP, SOCK_DGRAM, SOCK_STREAM, SockAddrIn};
use slopos_slibc::test_harness::note;
use slopos_userland::net::{ResolveError, resolve_host};
use slopos_userland::net_query as query;
use slopos_userland::syscall::net::net_resolver_set;

const LOOKUP_TIMEOUT_MS: u32 = 10_000;
const HELD: &str = "held.dnstest";
const HELD_ADDR: [u8; 4] = [10, 1, 2, 3];
const FORGED: &str = "forged.dnstest";
const FORGED_ADDR: [u8; 4] = [10, 1, 1, 1];
const MISSING: &str = "missing.dnstest";
const PARALLEL: u8 = 8;

fn question_name(query: &[u8]) -> Option<String> {
    let mut pos = 12;
    let mut name = String::new();
    loop {
        let len = *query.get(pos)? as usize;
        if len == 0 {
            return Some(name);
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(query.get(pos + 1..pos + 1 + len)?).ok()?);
        pos += 1 + len;
    }
}

fn question_end(query: &[u8]) -> usize {
    let mut pos = 12;
    while query[pos] != 0 {
        pos += 1 + query[pos] as usize;
    }
    pos + 1 + 4
}

fn reply(query: &[u8], rcode: u16, addr: Option<[u8; 4]>) -> Vec<u8> {
    let end = question_end(query);
    let mut out = query[..end].to_vec();
    out[2..4].copy_from_slice(&(0x8180 | rcode).to_be_bytes());
    out[6..8].copy_from_slice(&u16::from(addr.is_some()).to_be_bytes());
    if let Some(addr) = addr {
        out.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        out.extend_from_slice(&addr);
    }
    out
}

fn forgeries(query: &[u8]) -> [Vec<u8>; 2] {
    let mut wrong_id = reply(query, 0, Some([6, 6, 6, 6]));
    wrong_id[1] ^= 1;
    let mut wrong_question = reply(query, 0, Some([6, 6, 6, 7]));
    wrong_question[13] = b'x';
    [wrong_id, wrong_question]
}

/// The nameserver, installed as the only resolver for as long as it lives.
struct Stub {
    stop: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    held_asked: Arc<AtomicBool>,
    missing_asked: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
    saved: Option<UserResolver>,
}

impl Stub {
    fn start(attempts: u32) -> Result<Self, String> {
        let saved = query::fetch::<UserResolver>(NET_Q_RESOLVER, NET_IFINDEX_NONE)
            .map_err(|e| format!("read resolver config: {e:?}"))?
            .records
            .first()
            .copied();
        let socket = UdpSocket::bind("127.0.0.1:53").map_err(|e| format!("bind :53: {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(|e| format!("read timeout: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let held_asked = Arc::new(AtomicBool::new(false));
        let missing_asked = Arc::new(AtomicUsize::new(0));
        let thread = {
            let asked = (held_asked.clone(), missing_asked.clone());
            let (stop, release) = (stop.clone(), release.clone());
            thread::spawn(move || serve(&socket, &stop, &release, &asked.0, &asked.1))
        };
        let stub = Self {
            stop,
            release,
            held_asked,
            missing_asked,
            thread: Some(thread),
            saved,
        };
        install(&[[127, 0, 0, 1]], LOOKUP_TIMEOUT_MS, attempts)?;
        Ok(stub)
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // A lease-learned list comes back pinned, which the later network
        // cases cannot tell apart.
        let restored = match self.saved {
            Some(cfg) if cfg.n_servers > 0 => install(
                &cfg.servers[..cfg.n_servers as usize],
                cfg.timeout_ms,
                cfg.attempts,
            ),
            _ => install(&[], 0, 0),
        };
        if let Err(e) = restored {
            eprintln!("dns_concurrent: {e}");
        }
    }
}

fn install(servers: &[[u8; 4]], timeout_ms: u32, attempts: u32) -> Result<(), String> {
    let mut req = UserResolverReq::default();
    req.servers[..servers.len()].copy_from_slice(servers);
    req.n_servers = servers.len() as u8;
    req.source = NET_RESOLVER_SRC_STATIC;
    req.timeout_ms = timeout_ms;
    req.attempts = attempts;
    net_resolver_set(&req).map_err(|e| format!("set resolver: {e:?}"))
}

fn serve(
    socket: &UdpSocket,
    stop: &AtomicBool,
    release: &AtomicBool,
    held_asked: &AtomicBool,
    missing_asked: &AtomicUsize,
) {
    let mut buf = [0u8; 512];
    let mut held: Option<(std::net::SocketAddr, Vec<u8>)> = None;
    while !stop.load(Ordering::Acquire) {
        if release.load(Ordering::Acquire)
            && let Some((peer, answer)) = held.take()
        {
            let _ = socket.send_to(&answer, peer);
        }
        let Ok((n, peer)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let query = &buf[..n];
        let Some(name) = question_name(query) else {
            continue;
        };
        if name == HELD {
            held = Some((peer, reply(query, 0, Some(HELD_ADDR))));
            held_asked.store(true, Ordering::Release);
        } else if name == FORGED {
            for forged in forgeries(query) {
                let _ = socket.send_to(&forged, peer);
            }
            let _ = socket.send_to(&reply(query, 0, Some(FORGED_ADDR)), peer);
        } else if let Some(n) = name
            .strip_prefix("fast-")
            .and_then(|rest| rest.strip_suffix(".dnstest"))
            .and_then(|n| n.parse::<u8>().ok())
        {
            let _ = socket.send_to(&reply(query, 0, Some([10, 9, 8, n])), peer);
        } else {
            if name == MISSING {
                missing_asked.fetch_add(1, Ordering::AcqRel);
            }
            let _ = socket.send_to(&reply(query, 3, None), peer);
        }
    }
}

fn wait_for(flag: &AtomicBool, budget: Duration) -> bool {
    let start = Instant::now();
    while !flag.load(Ordering::Acquire) {
        if start.elapsed() > budget {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}

fn lookups_do_not_wait_for_each_other() -> bool {
    let stub = match Stub::start(1) {
        Ok(s) => s,
        Err(e) => {
            note(&format!("{e}"));
            return false;
        }
    };
    let held = thread::spawn(|| resolve_host(HELD));
    if !wait_for(&stub.held_asked, Duration::from_secs(5)) {
        note("the held query never reached the stub");
        return false;
    }

    let others: Vec<_> = (0..PARALLEL)
        .map(|i| thread::spawn(move || (i, resolve_host(&format!("fast-{i}.dnstest")))))
        .collect();
    let mut ok = true;
    for t in others {
        match t.join() {
            Ok((i, Ok(addr))) if addr.0 == [10, 9, 8, i] => {}
            Ok((i, got)) => {
                note(&format!("fast-{i} answered {got:?}"));
                ok = false;
            }
            Err(_) => ok = false,
        }
    }
    if held.is_finished() {
        note("the held lookup finished before its reply was sent");
        ok = false;
    }

    stub.release.store(true, Ordering::Release);
    match held.join() {
        Ok(Ok(addr)) if addr.0 == HELD_ADDR => {}
        other => {
            note(&format!("the held lookup ended {other:?}"));
            ok = false;
        }
    }
    ok
}

fn forged_replies_are_ignored() -> bool {
    let _stub = match Stub::start(1) {
        Ok(s) => s,
        Err(e) => {
            note(&format!("{e}"));
            return false;
        }
    };
    match resolve_host(FORGED) {
        Ok(addr) if addr.0 == FORGED_ADDR => true,
        other => {
            note(&format!("{FORGED} resolved to {other:?}"));
            false
        }
    }
}

fn nxdomain_ends_the_lookup() -> bool {
    let stub = match Stub::start(3) {
        Ok(s) => s,
        Err(e) => {
            note(&format!("{e}"));
            return false;
        }
    };
    let start = Instant::now();
    let got = resolve_host(MISSING);
    let elapsed = start.elapsed();
    if got != Err(ResolveError::NameNotFound) {
        note(&format!("missing.dnstest gave {got:?}"));
        return false;
    }
    if elapsed >= Duration::from_millis(u64::from(LOOKUP_TIMEOUT_MS)) {
        note(&format!("NXDOMAIN took {elapsed:?}, as long as a timeout"));
        return false;
    }
    match stub.missing_asked.load(Ordering::Acquire) {
        1 => true,
        n => {
            note(&format!(
                "NXDOMAIN was asked {n} times with three attempts allowed"
            ));
            false
        }
    }
}

/// Each node's socket type and protocol, as `socket()` takes them.
type Nodes = Vec<(i32, i32)>;

const BOTH: [(i32, i32); 2] = [(SOCK_STREAM, IPPROTO_TCP), (SOCK_DGRAM, IPPROTO_UDP)];

/// The first node's address and port, and every node's type; `hints` is
/// `(ai_flags, ai_socktype)`.
fn gai(
    node: &CStr,
    service: Option<&CStr>,
    hints: Option<(i32, i32)>,
) -> Result<([u8; 4], u16, Nodes), i32> {
    let hints = hints.map(|(flags, socktype)| AddrInfo {
        ai_flags: flags,
        ai_family: AF_INET,
        ai_socktype: socktype,
        ai_protocol: 0,
        ai_addrlen: 0,
        ai_addr: std::ptr::null_mut(),
        ai_canonname: std::ptr::null_mut(),
        ai_next: std::ptr::null_mut(),
    });
    let mut res: *mut AddrInfo = std::ptr::null_mut();
    // SAFETY: both strings are NUL-terminated and `res` is freed below.
    let rc = unsafe {
        getaddrinfo(
            node.as_ptr().cast(),
            service.map_or(std::ptr::null(), |s| s.as_ptr().cast()),
            hints.as_ref().map_or(std::ptr::null(), |h| h),
            &mut res,
        )
    };
    if rc != 0 {
        return Err(rc);
    }
    // SAFETY: a zero return leaves a list of AF_INET nodes whose addresses
    // are `sockaddr_in`s, freed once here.
    unsafe {
        let sa = &*((*res).ai_addr as *const SockAddrIn);
        let found = (sa.sin_addr.to_ne_bytes(), u16::from_be(sa.sin_port));
        let mut nodes = Vec::new();
        let mut node = res;
        while !node.is_null() {
            nodes.push(((*node).ai_socktype, (*node).ai_protocol));
            node = (*node).ai_next;
        }
        freeaddrinfo(res);
        Ok((found.0, found.1, nodes))
    }
}

fn getaddrinfo_reports_through_the_resolver() -> bool {
    let _stub = match Stub::start(1) {
        Ok(s) => s,
        Err(e) => {
            note(&format!("{e}"));
            return false;
        }
    };
    let checks = [
        (
            "a name and a numeric service",
            gai(c"fast-9.dnstest", Some(c"8080"), Some((0, 0))),
            Ok(([10, 9, 8, 9], 8080, BOTH.to_vec())),
        ),
        (
            "NXDOMAIN",
            gai(c"missing.dnstest", None, Some((0, 0))),
            Err(EAI_NONAME),
        ),
        (
            "the broadcast address, numerically",
            gai(c"255.255.255.255", None, Some((AI_NUMERICHOST, 0))),
            Ok(([255, 255, 255, 255], 0, BOTH.to_vec())),
        ),
        (
            "a service name, with no services database",
            gai(c"127.0.0.1", Some(c"http"), Some((0, 0))),
            Err(EAI_SERVICE),
        ),
        (
            "a service name where only a number was allowed",
            gai(c"127.0.0.1", Some(c"http"), Some((AI_NUMERICSERV, 0))),
            Err(EAI_NONAME),
        ),
        (
            "a flag POSIX defines but IPv4-only has no use for",
            gai(
                c"127.0.0.1",
                Some(c"53"),
                Some((AI_ADDRCONFIG, SOCK_STREAM)),
            ),
            Ok(([127, 0, 0, 1], 53, vec![(SOCK_STREAM, IPPROTO_TCP)])),
        ),
        (
            "a flag no one defines",
            gai(c"127.0.0.1", Some(c"53"), Some((0x4000, 0))),
            Err(EAI_BADFLAGS),
        ),
        (
            "no hints, which asks for every socket type",
            gai(c"127.0.0.1", Some(c"53"), None),
            Ok(([127, 0, 0, 1], 53, BOTH.to_vec())),
        ),
        (
            "a datagram socket, with its protocol filled in",
            gai(c"127.0.0.1", Some(c"53"), Some((0, SOCK_DGRAM))),
            Ok(([127, 0, 0, 1], 53, vec![(SOCK_DGRAM, IPPROTO_UDP)])),
        ),
    ];
    let mut ok = true;
    for (what, got, want) in checks {
        if got != want {
            note(&format!("getaddrinfo, {what}: {got:?}, want {want:?}"));
            ok = false;
        }
    }
    ok
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "lookups_do_not_wait_for_each_other",
            lookups_do_not_wait_for_each_other,
        ),
        ("forged_replies_are_ignored", forged_replies_are_ignored),
        ("nxdomain_ends_the_lookup", nxdomain_ends_the_lookup),
        (
            "getaddrinfo_reports_through_the_resolver",
            getaddrinfo_reports_through_the_resolver,
        ),
    ]);
}
