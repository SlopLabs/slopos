//! `res_init`, `res_query` and `dn_expand`, as resolver(3) describes them:
//! a query of any type and class to the nameservers the kernel's resolver
//! uses, asked over UDP and again over TCP when the answer is truncated
//! (RFC 1035 section 4.2). The kernel's own resolver answers `getaddrinfo`;
//! this is the path for the records it does not look up.

use core::ffi::{c_char, c_int, c_uchar, c_ulong};

use slopos_abi::net::{NET_MAX_RESOLVERS, NET_Q_RESOLVER, UserResolver};
use slopos_slibc_core::dns;

use super::addr::{
    AF_INET, SO_ERROR, SOCK_CLOEXEC, SOCK_DGRAM, SOCK_NONBLOCK, SOCK_STREAM, SOL_SOCKET, SockAddrIn,
};
use super::ifaddrs::Records;
use super::netdb::{__h_errno_location, HOST_NOT_FOUND, NO_DATA, NO_RECOVERY, TRY_AGAIN};
use crate::errno::{EADDRINUSE, EAGAIN, EINPROGRESS, EINTR, EINVAL, Errno, errno_set};
use crate::io::poll::{POLLIN, POLLOUT, Pollfd};
use crate::pal::{Pal, Sys};
use crate::string::u_strlen;

pub const MAXNS: usize = 3;
const _: () = assert!(MAXNS == NET_MAX_RESOLVERS);
pub const RES_INIT: c_ulong = 0x1;
pub const RES_RECURSE: c_ulong = 0x40;
pub const RES_DEFAULT: c_ulong = 0x40;
const NAMESERVER_PORT: u16 = 53;
const EPHEMERAL_FIRST: u16 = 49_152;
const EPHEMERAL_COUNT: u16 = 16_384;
const SOURCE_PORT_DRAWS: usize = 32;

/// The resolver's state. `retrans` is the seconds one try waits, `retry`
/// how many rounds of the servers a query makes.
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct __res_state {
    pub retrans: c_int,
    pub retry: c_int,
    pub options: c_ulong,
    pub nscount: c_int,
    pub nsaddr_list: [SockAddrIn; MAXNS],
}

/// One state for the process, as on the BSDs: `res_init` fills it and a
/// caller may set `options` between calls.
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static mut _res: __res_state = __res_state {
    retrans: 0,
    retry: 0,
    options: 0,
    nscount: 0,
    nsaddr_list: [SockAddrIn {
        sin_family: 0,
        sin_port: 0,
        sin_addr: 0,
        sin_zero: [0; 8],
    }; MAXNS],
};

/// Reads the kernel resolver's nameservers, per-try timeout and attempts into
/// `_res`, keeping the option bits a caller set. The kernel answers the
/// query only for a caller that may inspect the system, so another gets -1
/// and `EPERM`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn res_init() -> c_int {
    let config = match Records::fetch(NET_Q_RESOLVER) {
        Ok(records) if records.count >= 1 => records.get::<UserResolver>(0),
        Ok(_) => UserResolver::default(),
        Err(e) => {
            errno_set(e);
            return -1;
        }
    };
    let state = &raw mut _res;
    let count = usize::from(config.n_servers).min(MAXNS);
    for (slot, server) in (&mut (*state).nsaddr_list).iter_mut().zip(&config.servers) {
        *slot = SockAddrIn {
            sin_family: AF_INET as u16,
            sin_port: NAMESERVER_PORT.to_be(),
            sin_addr: u32::from_ne_bytes(*server),
            sin_zero: [0; 8],
        };
    }
    (*state).nscount = count as c_int;
    (*state).retrans = config.timeout_ms.div_ceil(1000).max(1) as c_int;
    (*state).retry = config.attempts.max(1) as c_int;
    if (*state).options & RES_INIT == 0 {
        (*state).options = RES_DEFAULT;
    }
    (*state).options |= RES_INIT;
    0
}

/// Asks for `dname`'s records of `rtype` in `rclass` and copies the reply,
/// at most `anslen` octets of it, to `answer`, returning its length. A reply
/// cut short by `anslen` keeps its header's truncation bit set. -1 with
/// `h_errno` set when no reply answers: `HOST_NOT_FOUND` for a name that does
/// not exist, `NO_DATA` for one with no such records, `TRY_AGAIN` when no
/// server replied or the one that did failed, and `NO_RECOVERY` otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn res_query(
    dname: *const c_char,
    rclass: c_int,
    rtype: c_int,
    answer: *mut c_uchar,
    anslen: c_int,
) -> c_int {
    let fail = |h: c_int| {
        *__h_errno_location() = h;
        -1
    };
    if dname.is_null() || answer.is_null() || anslen < dns::HFIXEDSZ as c_int {
        errno_set(EINVAL.raw());
        return fail(NO_RECOVERY);
    }
    let (Ok(class), Ok(rtype)) = (u16::try_from(rclass), u16::try_from(rtype)) else {
        errno_set(EINVAL.raw());
        return fail(NO_RECOVERY);
    };
    let state = &raw mut _res;
    if (*state).options & RES_INIT == 0 && res_init() != 0 {
        return fail(NO_RECOVERY);
    }
    let Some(id) = random_u16() else {
        return fail(NO_RECOVERY);
    };
    let name = core::slice::from_raw_parts(dname as *const u8, u_strlen(dname as *const u8));
    let mut query = [0u8; dns::PACKETSZ];
    let Some(qlen) = dns::encode_query(
        id,
        (*state).options & RES_RECURSE != 0,
        name,
        class,
        rtype,
        &mut query,
    ) else {
        errno_set(EINVAL.raw());
        return fail(NO_RECOVERY);
    };
    let query = &query[..qlen];
    let answer = core::slice::from_raw_parts_mut(answer, anslen as usize);
    let list = (*state).nsaddr_list;
    let servers = usize::try_from((*state).nscount).unwrap_or(0).min(MAXNS);
    let wait_ms = (*state).retrans.max(1).saturating_mul(1000);
    let mut verdict = TRY_AGAIN;
    for _ in 0..(*state).retry.max(1) {
        for server in &list[..servers] {
            let Some(len) = ask(server, query, answer, wait_ms) else {
                continue;
            };
            let Some(reply) = dns::header(&answer[..len]) else {
                continue;
            };
            match reply.rcode {
                dns::NOERROR if reply.ancount > 0 => return len as c_int,
                dns::NOERROR => return fail(NO_DATA),
                dns::NXDOMAIN => return fail(HOST_NOT_FOUND),
                dns::SERVFAIL => verdict = TRY_AGAIN,
                _ => verdict = NO_RECOVERY,
            }
        }
    }
    fail(verdict)
}

/// One server's reply to `query` in `answer`, over UDP and then TCP if that
/// reply was truncated, or `None` when it sent none that answers. The
/// truncated reply stands when the TCP retry brings none that answers.
unsafe fn ask(
    server: &SockAddrIn,
    query: &[u8],
    answer: &mut [u8],
    wait_ms: c_int,
) -> Option<usize> {
    let mut datagram = [0u8; dns::PACKETSZ];
    let len = exchange(server, SOCK_DGRAM, query, &mut datagram, wait_ms)?;
    let reply = &datagram[..len];
    if !dns::answers(query, reply) {
        return None;
    }
    if dns::header(reply)?.truncated
        && let Some(len) = exchange(server, SOCK_STREAM, query, answer, wait_ms)
        && dns::answers(query, &answer[..len])
    {
        return Some(len);
    }
    Some(copy_reply(reply, answer))
}

/// `reply`, or as much as fits, in `answer`, marked truncated if it was cut.
fn copy_reply(reply: &[u8], answer: &mut [u8]) -> usize {
    let n = reply.len().min(answer.len());
    answer[..n].copy_from_slice(&reply[..n]);
    if n < reply.len() {
        answer[2] |= 0x02;
    }
    n
}

/// Sends `query` to `server` over a fresh socket of `kind` and reads one
/// reply into `out`: a datagram, or a two-octet length and that many octets,
/// of which those that do not fit are read and dropped. Each step, the
/// connect included, waits at most `wait_ms`.
unsafe fn exchange(
    server: &SockAddrIn,
    kind: c_int,
    query: &[u8],
    out: &mut [u8],
    wait_ms: c_int,
) -> Option<usize> {
    let fd = Sys::socket(AF_INET, kind | SOCK_CLOEXEC | SOCK_NONBLOCK, 0).ok()?;
    let got = converse(fd, server, kind, query, out, wait_ms);
    let _ = Sys::close(fd);
    got
}

unsafe fn converse(
    fd: c_int,
    server: &SockAddrIn,
    kind: c_int,
    query: &[u8],
    out: &mut [u8],
    wait_ms: c_int,
) -> Option<usize> {
    if kind == SOCK_DGRAM {
        bind_random_port(fd)?;
    }
    connect(fd, server, wait_ms)?;
    if kind == SOCK_DGRAM {
        Sys::send(fd, query.as_ptr(), query.len(), 0).ok()?;
        return when_ready(fd, POLLIN, wait_ms, || {
            Sys::recv(fd, out.as_mut_ptr(), out.len(), 0)
        });
    }
    let prefix = (query.len() as u16).to_be_bytes();
    send_all(fd, &prefix, wait_ms)?;
    send_all(fd, query, wait_ms)?;
    let mut prefix = [0u8; 2];
    recv_all(fd, &mut prefix, wait_ms)?;
    let len = usize::from(u16::from_be_bytes(prefix));
    let keep = len.min(out.len());
    recv_all(fd, &mut out[..keep], wait_ms)?;
    let mut rest = len - keep;
    let mut sink = [0u8; 256];
    while rest > 0 {
        let n = rest.min(sink.len());
        recv_all(fd, &mut sink[..n], wait_ms)?;
        rest -= n;
    }
    if keep < len {
        out[2] |= 0x02;
    }
    Some(keep)
}

/// A transaction ID or port draw from `getrandom`, or `None` if it did not
/// fill one.
fn random_u16() -> Option<u16> {
    let mut bytes = [0u8; 2];
    (Sys::getrandom(bytes.as_mut_ptr(), bytes.len(), 0) == Ok(bytes.len()))
        .then(|| u16::from_ne_bytes(bytes))
}

/// Binds `fd` to an ephemeral port drawn at random, so the source port adds
/// to the ID's entropy against a spoofed reply (RFC 5452 section 9.2).
unsafe fn bind_random_port(fd: c_int) -> Option<()> {
    for _ in 0..SOURCE_PORT_DRAWS {
        let local = SockAddrIn {
            sin_family: AF_INET as u16,
            sin_port: (EPHEMERAL_FIRST + random_u16()? % EPHEMERAL_COUNT).to_be(),
            sin_addr: 0,
            sin_zero: [0; 8],
        };
        match Sys::bind(
            fd,
            (&raw const local).cast(),
            size_of::<SockAddrIn>() as u32,
        ) {
            Ok(()) => return Some(()),
            Err(e) if e == EADDRINUSE => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Connects the non-blocking `fd` to `server`, waiting at most `wait_ms` for
/// the handshake.
unsafe fn connect(fd: c_int, server: &SockAddrIn, wait_ms: c_int) -> Option<()> {
    match Sys::connect(
        fd,
        (server as *const SockAddrIn).cast(),
        size_of::<SockAddrIn>() as u32,
    ) {
        Ok(()) => return Some(()),
        Err(e) if e == EINPROGRESS => {}
        Err(_) => return None,
    }
    ready(fd, POLLOUT, wait_ms).then_some(())?;
    let mut error: c_int = 0;
    let mut len = size_of::<c_int>() as u32;
    Sys::getsockopt(fd, SOL_SOCKET, SO_ERROR, (&raw mut error).cast(), &mut len).ok()?;
    (error == 0).then_some(())
}

unsafe fn send_all(fd: c_int, mut buf: &[u8], wait_ms: c_int) -> Option<()> {
    while !buf.is_empty() {
        let n = when_ready(fd, POLLOUT, wait_ms, || {
            Sys::send(fd, buf.as_ptr(), buf.len(), 0)
        })?;
        if n == 0 {
            return None;
        }
        buf = &buf[n..];
    }
    Some(())
}

unsafe fn recv_all(fd: c_int, buf: &mut [u8], wait_ms: c_int) -> Option<()> {
    let mut at = 0;
    while at < buf.len() {
        let n = when_ready(fd, POLLIN, wait_ms, || {
            Sys::recv(fd, buf[at..].as_mut_ptr(), buf.len() - at, 0)
        })?;
        if n == 0 {
            return None;
        }
        at += n;
    }
    Some(())
}

/// `io` once `fd` is ready for `events`, and again after each `EAGAIN`
/// despite the readiness; every wait is at most `wait_ms`.
fn when_ready(
    fd: c_int,
    events: i16,
    wait_ms: c_int,
    mut io: impl FnMut() -> Result<usize, Errno>,
) -> Option<usize> {
    loop {
        ready(fd, events, wait_ms).then_some(())?;
        match io() {
            Ok(n) => return Some(n),
            Err(e) if e == EAGAIN => continue,
            Err(_) => return None,
        }
    }
}

/// Whether `fd` is ready for `events` within `wait_ms`; an interrupted wait
/// is waited again.
fn ready(fd: c_int, events: i16, wait_ms: c_int) -> bool {
    let mut pfd = Pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        match Sys::poll((&raw mut pfd).cast(), 1, wait_ms) {
            Ok(n) => return n > 0 && pfd.revents & events != 0,
            Err(e) if e == EINTR => continue,
            Err(_) => return false,
        }
    }
}

/// Writes the name at `src` in the message `msg..eom` to `dst` as text, in at
/// most `dstsiz` octets with the NUL, and returns how many octets the name
/// occupies at `src`; -1 for a name that is malformed or does not fit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dn_expand(
    msg: *const c_uchar,
    eom: *const c_uchar,
    src: *const c_uchar,
    dst: *mut c_char,
    dstsiz: c_int,
) -> c_int {
    if msg.is_null() || eom < msg || src < msg || src >= eom || dst.is_null() || dstsiz <= 0 {
        return -1;
    }
    let message = core::slice::from_raw_parts(msg, eom.offset_from(msg) as usize);
    let out = core::slice::from_raw_parts_mut(dst as *mut u8, dstsiz as usize);
    match dns::expand(message, src.offset_from(msg) as usize, out) {
        Some(used) => used as c_int,
        None => -1,
    }
}
