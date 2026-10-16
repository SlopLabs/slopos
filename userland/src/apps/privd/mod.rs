//! `/sbin/privd` — runs a named action's mechanism, the base program the
//! sealed policy names, as a fresh child holding what the action names and
//! nothing of its requester's but the descriptors it sent.
//!
//! Who asks is what the kernel recorded when the connection was made: the
//! uid, a pidfd and the program. Every failure on the way is a denial, and a
//! requester whose process has exited is refused, before consent and after.
//!
//! `SIGTERM` drains it, as init asks when a new base changes privd, its
//! policy or the pairing: it stops accepting, refuses what is asked next,
//! interrupts what waits on the person and lets go of the consent pair, and
//! exits once every request it took is answered. Init starts the new base's
//! privd then, or beside it once the drain has taken five seconds.

macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod consent;

use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;

use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::signal::{SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, sig_bit};
use slopos_abi::syscall::{
    F_GETFD, MSG_NOSIGNAL, O_CLOEXEC, O_NONBLOCK, POLLHUP, POLLIN, POLLOUT, UserPollFd,
};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::consent::{Ask, Refusal};
use slopos_priv_core::policy::{Principal, REMOTED};
use slopos_priv_core::wire::{self, CONNECTION, Exit, Reply, Request, kind};
use slopos_priv_core::{
    Action, ENV_ACTION, INIT, MECHANISM_LANG, MECHANISM_PATH, POLICY, Policy, Requester, SOCKET,
    Verdict, elf_interpreted,
};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};

use self::consent::{Broker, Waiting};
use crate::init_control;
use crate::readiness::ReadinessNotifier;
use crate::syscall::process::{self, Endowment, WaitStatus};
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, net, pidfd};

/// Init's readiness notifier is fd 3, the consent channel fd 4.
pub const CONSENT_FD: RawFd = 4;
const REMOTE_CONF: &str = "/usr/share/slopos/remote/remote.conf";
/// Shared by every requester but root and the paired agent, whom nothing
/// else can crowd out.
const MAX_CONNECTIONS: usize = 32;
/// Per uid and program, so no one requester takes every connection.
const MAX_CONNECTIONS_EACH: usize = 8;
/// One request's descriptors, which may arrive while the request before it is
/// served.
const MAX_PENDING_FDS: usize = wire::STDIO + 1;
/// A requester that leaves a reply unread this long loses the connection.
const REPLY_MS: i64 = 5000;
/// A mechanism's requester may relay these, and these alone start at their
/// default disposition in it whatever privd's are.
const DEFAULTED: u64 =
    sig_bit(SIGHUP) | sig_bit(SIGINT) | sig_bit(SIGQUIT) | sig_bit(SIGPIPE) | sig_bit(SIGTERM);

type Key = (u32, Option<String>);

/// A connection's place among those privd serves.
struct Admitted {
    daemon: &'static Daemon,
    key: Key,
    pooled: bool,
}

struct Daemon {
    policy: Result<Policy, String>,
    paired: bool,
    consent: Broker,
    running: Mutex<Vec<String>>,
    /// Held across the `fchdir` that names a requester's directory: the
    /// working directory is the process's, shared by every connection.
    cwd: Mutex<()>,
    connections: Mutex<Vec<(Key, bool)>>,
    /// The threads of what an earlier privd started that still ran when this
    /// one did, whose exclusive groups this one cannot know: none is taken
    /// while any of them runs.
    inherited: Vec<u32>,
}

impl Daemon {
    fn inherited_runs(&self) -> bool {
        !self.inherited.is_empty()
            && init_control::threads_elsewhere(init_control::PRIVD)
                .iter()
                .any(|thread| self.inherited.contains(thread))
    }
}

static DAEMON: OnceLock<Daemon> = OnceLock::new();

pub(super) static DRAINING: AtomicBool = AtomicBool::new(false);
/// Requests between being read and being answered.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// The write end of the pipe the accept loop polls beside the listener.
static WAKE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn drain(_: i32) {
    DRAINING.store(true, Ordering::SeqCst);
    let _ = fs::write_slice(WAKE.load(Ordering::Relaxed), &[0]);
}

fn finish_draining() -> ! {
    log!("privd: draining for a new base");
    while IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        thread::sleep(std::time::Duration::from_millis(50));
    }
    std::process::exit(0);
}

pub fn privd_main() -> ! {
    // Before any descriptor is opened: one could land at the number of an
    // inherited one privd was started without.
    let ready = ReadinessNotifier::acquire();
    let channel = consent_channel();
    let policy = load_policy();
    if let Err(why) = &policy {
        log!("privd: {why}; every request will be refused");
    }
    let daemon = Daemon {
        policy,
        paired: std::path::Path::new(REMOTE_CONF).is_file(),
        consent: Broker::start(channel),
        running: Mutex::new(Vec::new()),
        cwd: Mutex::new(()),
        connections: Mutex::new(Vec::new()),
        inherited: init_control::threads_elsewhere(init_control::PRIVD),
    };
    let daemon: &'static Daemon = DAEMON.get_or_init(|| daemon);
    let listener = match listen() {
        Ok(listener) => listener,
        Err(why) => {
            log!("privd: {SOCKET}: {why}");
            std::process::exit(1);
        }
    };
    let Ok((woken, wake)) = fs::pipe2((O_CLOEXEC | O_NONBLOCK) as u32) else {
        log!("privd: no pipe to hear a drain on");
        std::process::exit(1);
    };
    WAKE.store(wake.raw(), Ordering::Relaxed);
    process::set_signal_handler(SIGTERM, drain);
    if let Some(ready) = ready {
        ready.signal_ready();
    }
    loop {
        let mut fds = [listener.raw(), woken.raw()].map(|fd| UserPollFd {
            fd,
            events: POLLIN,
            revents: 0,
        });
        let _ = fs::poll(&mut fds, -1);
        if DRAINING.load(Ordering::SeqCst) {
            drop(listener);
            finish_draining();
        }
        if fds[0].revents & POLLIN == 0 {
            continue;
        }
        let fd = match net::accept(listener.raw(), None) {
            Ok(fd) => fd,
            Err(SyscallError::EINTR) => continue,
            Err(e) => {
                log!("privd: accept: {e}");
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        let _ = fs::set_fd_cloexec(fd.raw());
        let peer = match Peer::of(&fd) {
            Ok(peer) => peer,
            Err(why) => {
                log!("privd: a connection whose peer is unknown: {why}");
                let denial = Reply::denied(format!("who is asking is unknown: {why}"));
                let _ = send(&fd, &denial, CONNECTION);
                continue;
            }
        };
        let Some(admitted) = Admitted::take(daemon, &peer, daemon.paired) else {
            let denial = Reply::denied("privd is serving too many connections");
            let _ = send(&fd, &denial, CONNECTION);
            continue;
        };
        if net::set_nonblocking(fd.raw()).is_err() {
            continue;
        }
        let _ = thread::Builder::new()
            .name("privd-conn".into())
            .spawn(move || {
                serve(daemon, &peer, fd);
                drop(admitted);
            });
    }
}

impl Admitted {
    fn take(daemon: &'static Daemon, peer: &Peer, paired: bool) -> Option<Admitted> {
        let root = peer.uid == 0 && peer.program.is_some();
        let pooled = !root && !peer.requester(paired).is_paired();
        let key = (peer.uid, peer.program.clone());
        let mut connections = daemon.connections.lock().ok()?;
        let in_pool = connections.iter().filter(|(_, pooled)| *pooled).count();
        if (pooled && in_pool >= MAX_CONNECTIONS)
            || connections.iter().filter(|(k, _)| *k == key).count() >= MAX_CONNECTIONS_EACH
        {
            return None;
        }
        connections.push((key.clone(), pooled));
        Some(Admitted {
            daemon,
            key,
            pooled,
        })
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        if let Ok(mut connections) = self.daemon.connections.lock()
            && let Some(at) = connections
                .iter()
                .position(|(k, pooled)| *k == self.key && *pooled == self.pooled)
        {
            connections.swap_remove(at);
        }
    }
}

/// The end of the socket pair init made: either end of a pair names its
/// maker, so no other process can hand privd one that passes.
fn consent_channel() -> Option<OwnedFd> {
    Sys::fcntl(CONSENT_FD, F_GETFD as i32, 0).ok()?;
    let _ = fs::set_fd_cloexec(CONSENT_FD);
    // SAFETY: init handed this descriptor over and nothing else here owns it.
    let fd = unsafe { OwnedFd::from_raw(CONSENT_FD) };
    let from_init = net::peer_cred(fd.raw()).is_ok_and(|cred| cred.uid == 0)
        && net::peer_program(fd.raw()).ok().flatten().as_deref() == Some(INIT.as_bytes());
    if !from_init {
        log!("privd: fd {CONSENT_FD} is not init's consent channel; nobody will be asked");
    }
    from_init.then_some(fd)
}

/// The base's policy as it stands on this boot, refused whole when a
/// principal of any action names a dynamically linked program: the loader
/// would take a library from outside the base for it while its identity still
/// reads as the base's.
fn load_policy() -> Result<Policy, String> {
    let policy = crate::privd::whole_policy()?;
    for action in policy.actions() {
        for allow in &action.allows {
            let path = match &allow.principal {
                Principal::Program(path) => path.as_str(),
                Principal::Paired => REMOTED,
                Principal::Root | Principal::Session => continue,
            };
            let image = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            if elf_interpreted(&image) != Some(false) {
                return Err(format!(
                    "{POLICY}: {}: {path} is not a statically linked program",
                    action.name
                ));
            }
        }
    }
    Ok(policy.for_boot(crate::privd::on_the_medium()))
}

fn listen() -> Result<OwnedFd, String> {
    let fd =
        net::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC as u16, 0).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(SOCKET);
    net::bind_unix(fd.raw(), SOCKET.as_bytes()).map_err(|e| e.to_string())?;
    std::os::unix::fs::chown(SOCKET, Some(0), Some(0)).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| e.to_string())?;
    net::listen(fd.raw(), 16).map_err(|e| e.to_string())?;
    Ok(fd)
}

/// Who made a connection, as the kernel recorded it at `connect`: the base
/// program it ran then, which stands only while it still runs it.
struct Peer {
    pid: u32,
    uid: u32,
    program: Option<String>,
    pidfd: OwnedFd,
}

const LAPSED: &str = "the requester no longer runs the program it connected as";

/// The base program the peer at `fd` runs now, `None` for any other.
fn base_program(fd: &OwnedFd) -> Result<Option<String>, String> {
    match net::peer_base_program(fd.raw()).map_err(|e| format!("SO_PEERPROGRAM: {e}"))? {
        None => Ok(None),
        Some(path) => String::from_utf8(path)
            .map(Some)
            .map_err(|_| "SO_PEERPROGRAM: not UTF-8".to_owned()),
    }
}

impl Peer {
    fn of(fd: &OwnedFd) -> Result<Peer, String> {
        let cred = net::peer_cred(fd.raw()).map_err(|e| format!("SO_PEERCRED: {e}"))?;
        let pidfd = net::peer_pidfd(fd.raw()).map_err(|e| format!("SO_PEERPIDFD: {e}"))?;
        Ok(Peer {
            pid: cred.pid as u32,
            uid: cred.uid,
            program: base_program(fd)?,
            pidfd,
        })
    }

    /// Whether the program read at `accept` still stands: the peer has not
    /// exec'd, mapped code from elsewhere or exited since.
    fn still_runs(&self, fd: &OwnedFd) -> bool {
        base_program(fd).is_ok_and(|now| now == self.program)
    }

    fn requester(&self, paired: bool) -> Requester<'_> {
        Requester::new(self.uid, self.program.as_deref(), paired)
    }

    fn alive(&self) -> bool {
        let mut fds = [UserPollFd {
            fd: self.pidfd.raw(),
            events: POLLIN,
            revents: 0,
        }];
        matches!(fs::poll(&mut fds, 0), Ok(0))
    }

    fn describe(&self) -> String {
        format!(
            "pid {} (uid {}, {})",
            self.pid,
            self.uid,
            self.program.as_deref().unwrap_or("outside the base")
        )
    }
}

struct Conn {
    fd: OwnedFd,
    frames: Deframer,
    fds: VecDeque<OwnedFd>,
    /// A request that came while another was served: a connection several
    /// processes inherited is served one request after another.
    waiting: Option<Parked>,
    open: bool,
}

struct Parked {
    payload: Vec<u8>,
    id: Option<u64>,
    withdrawn: bool,
}

impl Conn {
    /// Read what has arrived, keeping the descriptors that came with it, and
    /// first wait for something to arrive when `wait`. `false` once the
    /// requester has gone or sent descriptors no request carries.
    fn fill(&mut self, wait: bool) -> bool {
        let mut buf = [0u8; 4096];
        loop {
            match net::recv_with_fds(self.fd.raw(), &mut buf) {
                Ok((n, fds)) if n > 0 && self.fds.len() + fds.len() <= MAX_PENDING_FDS => {
                    self.frames.push(&buf[..n]);
                    self.fds.extend(fds);
                    return true;
                }
                Err(SyscallError::EAGAIN) if wait => {
                    let _ = fs::poll(&mut [self.poll(POLLIN)], -1);
                }
                Err(SyscallError::EAGAIN) => return true,
                Err(SyscallError::EINTR) => {}
                _ => {
                    self.open = false;
                    return false;
                }
            }
        }
    }

    fn poll(&self, events: u16) -> UserPollFd {
        UserPollFd {
            fd: self.fd.raw(),
            events,
            revents: 0,
        }
    }

    /// The next request, its descriptors, and whether its requester withdrew
    /// it while it waited; `None` at end of stream.
    fn next_request(&mut self) -> Result<Option<(Request, Vec<OwnedFd>, bool)>, String> {
        loop {
            let (payload, withdrawn) = match self.waiting.take() {
                Some(parked) => (parked.payload, parked.withdrawn),
                None => match self.frames.next_frame().map_err(|e| e.to_string())? {
                    Some((kind::REQUEST, payload)) => (payload, false),
                    Some((kind::SIGNAL, _)) => continue,
                    Some((other, _)) => return Err(format!("frame {other:#04x} is not a request")),
                    None if self.fill(true) => continue,
                    None => return Ok(None),
                },
            };
            let request = Request::decode(&payload).map_err(|_| "a malformed request")?;
            if self.fds.len() != request.descriptors() {
                return Err(format!(
                    "the request carried {} descriptors, not {}",
                    self.fds.len(),
                    request.descriptors()
                ));
            }
            let fds = self.fds.drain(..).collect();
            return Ok(Some((request, fds, withdrawn)));
        }
    }

    /// Take what arrived while request `id` is served, giving the signals
    /// sent for it; `None` once the requester has left or broken the protocol.
    fn signals(&mut self, id: u64) -> Option<Vec<u8>> {
        if !self.open || !self.fill(false) {
            return None;
        }
        let mut signals = Vec::new();
        loop {
            match self.frames.next_frame() {
                Ok(None) => return Some(signals),
                Ok(Some((kind::SIGNAL, payload))) => match wire::decode_signal(&payload) {
                    Ok((of, signal)) if of == id => signals.push(signal),
                    Ok((of, _)) => {
                        if let Some(parked) = self.waiting.as_mut().filter(|p| p.id == Some(of)) {
                            parked.withdrawn = true;
                        }
                    }
                    Err(_) => break,
                },
                Ok(Some((kind::REQUEST, payload))) if self.waiting.is_none() => {
                    let id = Request::decode(&payload).ok().map(|r| r.id);
                    self.waiting = Some(Parked {
                        payload,
                        id,
                        withdrawn: false,
                    });
                }
                Ok(Some(_)) | Err(_) => break,
            }
        }
        self.open = false;
        None
    }

    fn reply(&mut self, reply: &Reply, id: u64) {
        if self.open && !send(&self.fd, reply, id) {
            self.open = false;
        }
    }
}

/// `false` when the requester would not take the reply in time.
fn send(fd: &OwnedFd, reply: &Reply, id: u64) -> bool {
    let frame = reply.frame(id);
    let mut sent = 0;
    while sent < frame.len() {
        match net::send(fd.raw(), &frame[sent..], MSG_NOSIGNAL) {
            Ok(n) if n > 0 => sent += n,
            Err(SyscallError::EAGAIN) => {
                let mut fds = [UserPollFd {
                    fd: fd.raw(),
                    events: POLLOUT,
                    revents: 0,
                }];
                match fs::poll(&mut fds, REPLY_MS) {
                    Ok(n) if n > 0 => {}
                    Err(SyscallError::EINTR) => {}
                    _ => return false,
                }
            }
            Err(SyscallError::EINTR) => {}
            _ => return false,
        }
    }
    true
}

fn serve(daemon: &Daemon, peer: &Peer, fd: OwnedFd) {
    let mut conn = Conn {
        fd,
        frames: wire::deframer(),
        fds: VecDeque::new(),
        waiting: None,
        open: true,
    };
    while conn.open {
        match conn.next_request() {
            Ok(Some((request, fds, withdrawn))) => {
                IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
                if DRAINING.load(Ordering::SeqCst) {
                    let denial = Reply::denied("privd is restarting for a new base; ask again");
                    conn.reply(&denial, request.id);
                    IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                handle(daemon, peer, &mut conn, &request, &fds, withdrawn);
                IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(None) => return,
            Err(why) => {
                log!("privd: {}: {why}", peer.describe());
                conn.reply(&Reply::denied(why), CONNECTION);
                return;
            }
        }
    }
}

/// Released when the mechanism it was taken for has been reaped.
struct Claim<'a> {
    running: &'a Mutex<Vec<String>>,
    group: Option<String>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if let Some(group) = &self.group
            && let Ok(mut running) = self.running.lock()
        {
            running.retain(|g| g != group);
        }
    }
}

fn claim<'a>(daemon: &'a Daemon, action: &Action) -> Result<Claim<'a>, Reply> {
    let busy = || {
        Reply::denied(format!(
            "another {} is running",
            action.exclusive_group().unwrap_or(&action.name)
        ))
    };
    let group = action.exclusive_group().map(str::to_owned);
    if group.is_some() && daemon.inherited_runs() {
        return Err(Reply::denied(
            "a mechanism an earlier privd started is still running",
        ));
    }
    let mut running = daemon.running.lock().map_err(|_| busy())?;
    if let Some(group) = &group {
        if running.contains(group) {
            return Err(busy());
        }
        running.push(group.clone());
    }
    Ok(Claim {
        running: &daemon.running,
        group,
    })
}

fn handle(
    daemon: &Daemon,
    peer: &Peer,
    conn: &mut Conn,
    request: &Request,
    fds: &[OwnedFd],
    withdrawn: bool,
) {
    let outcome = if withdrawn {
        Err(Reply::denied(Refusal::Withdrawn.to_string()))
    } else {
        run(daemon, peer, conn, request, fds)
    };
    let reply = outcome.map_or_else(|denial| denial, Reply::Exited);
    if let Reply::Denied { reason, .. } = &reply {
        log!(
            "privd: {} for {}: refused: {reason}",
            request.action,
            peer.describe()
        );
    }
    conn.reply(&reply, request.id);
}

/// A request waiting on the person at the machine.
struct Pending<'a> {
    peer: &'a Peer,
    conn: &'a mut Conn,
    id: u64,
}

impl Waiting for Pending<'_> {
    fn shown(&mut self) {
        self.conn.reply(&Reply::Asking, self.id);
    }

    fn withdrawn(&mut self) -> bool {
        still_wanted(self.peer, self.conn, self.id).is_err()
    }
}

/// Refused once the requester has exited, runs another program than it
/// connected as, or has withdrawn request `id`.
fn still_wanted(peer: &Peer, conn: &mut Conn, id: u64) -> Result<(), Reply> {
    if !peer.alive() {
        return Err(Reply::denied("the requester has exited"));
    }
    if !peer.still_runs(&conn.fd) {
        return Err(Reply::denied(LAPSED));
    }
    if conn.signals(id).is_none_or(|s| !s.is_empty()) {
        return Err(Reply::denied(Refusal::Withdrawn.to_string()));
    }
    Ok(())
}

fn run(
    daemon: &Daemon,
    peer: &Peer,
    conn: &mut Conn,
    request: &Request,
    fds: &[OwnedFd],
) -> Result<Exit, Reply> {
    let policy = daemon
        .policy
        .as_ref()
        .map_err(|why| Reply::denied(format!("privd has no policy: {why}")))?;
    let action = policy
        .action(&request.action)
        .ok_or_else(|| Reply::denied(format!("no action {}", request.action)))?;
    if !action.accepts(&request.args()) {
        return Err(Reply::denied(format!(
            "{} is not an argument list {} takes",
            shown(&request.args),
            action.name
        )));
    }
    if request.cwd != action.cwd {
        return Err(Reply::denied(if action.cwd {
            "the action runs in the requester's working directory, which was not sent"
        } else {
            "the action takes no working directory"
        }));
    }
    if !peer.still_runs(&conn.fd) {
        return Err(Reply::denied(LAPSED));
    }
    let verdict = action.admits(&peer.requester(daemon.paired));
    if verdict == Verdict::Refuse {
        return Err(Reply::denied(format!(
            "{} may not ask for {}",
            peer.describe(),
            action.name
        )));
    }
    still_wanted(peer, conn, request.id)?;
    let cwd = match fds.get(wire::STDIO) {
        Some(dir) => directory_of(daemon, dir)?,
        None => "/".to_owned(),
    };
    if verdict == Verdict::Ask {
        drop(claim(daemon, action)?);
        let mut command = vec![action.program.as_bytes().to_vec()];
        command.extend(request.args.iter().cloned());
        let ask = Ask {
            id: 0,
            action: action.name.clone(),
            describe: action.describe.clone(),
            program: peer.program.clone(),
            uid: peer.uid,
            pid: peer.pid,
            cwd: cwd.clone(),
            command,
        };
        if !ask.fits() {
            return Err(Reply::denied(
                "the command is too long to show the person at the machine",
            ));
        }
        let mut pending = Pending {
            peer,
            conn,
            id: request.id,
        };
        daemon
            .consent
            .ask(ask, &mut pending)
            .map_err(|refusal| Reply::Denied {
                reason: refusal.to_string(),
                by_person: refusal.by_person(),
            })?;
    }
    let _claim = claim(daemon, action)?;
    still_wanted(peer, conn, request.id)?;
    let pid = spawn(action, &request.args, &fds[..wire::STDIO], &cwd)?;
    log!(
        "privd: {} for {}: started pid {pid}{}",
        action.name,
        peer.describe(),
        if verdict == Verdict::Ask {
            ", allowed at the machine"
        } else {
            ""
        }
    );
    conn.reply(&Reply::Started { pid }, request.id);
    Ok(wait(conn, pid, request.id))
}

fn shown(args: &[Vec<u8>]) -> String {
    let words: Vec<String> = args
        .iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    format!("{words:?}")
}

/// The path of the directory `dir` names, which the requester opened and so
/// could reach.
fn directory_of(daemon: &Daemon, dir: &OwnedFd) -> Result<String, Reply> {
    let _held = daemon
        .cwd
        .lock()
        .map_err(|_| Reply::denied("privd's directory lock is poisoned"))?;
    match Sys::fchdir(dir.raw()) {
        Ok(()) => {}
        Err(e) if e.raw() == -slopos_abi::Errno::ESTALE.raw() => {
            return Err(Reply::denied(
                "the working directory sent is no longer in the tree",
            ));
        }
        Err(_) => {
            return Err(Reply::denied(
                "the working directory sent is not a directory",
            ));
        }
    }
    let path = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned));
    if std::env::set_current_dir("/").is_err() {
        log!("privd: cannot return to /");
    }
    path.filter(|p| p.starts_with('/'))
        .ok_or_else(|| Reply::denied("the working directory has no path"))
}

fn spawn(action: &Action, args: &[Vec<u8>], stdio: &[OwnedFd], cwd: &str) -> Result<u32, Reply> {
    let name = action.program.rsplit('/').next().unwrap_or(&action.program);
    let mut argv = vec![CString::new(name).map_err(|_| Reply::denied("bad program name"))?];
    for arg in args {
        argv.push(
            CString::new(arg.as_slice()).map_err(|_| Reply::denied("an argument holds NUL"))?,
        );
    }
    let env = [
        format!("PATH={MECHANISM_PATH}"),
        format!("LANG={MECHANISM_LANG}"),
        format!("{ENV_ACTION}={}", action.name),
    ];
    let env: Vec<CString> = env
        .into_iter()
        .map(|var| CString::new(var).expect("no NUL in a fixed variable"))
        .collect();
    let argv_ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr().cast()).collect();
    let env_ptrs: Vec<*const u8> = env.iter().map(|v| v.as_ptr().cast()).collect();
    let actions = [
        process::clone_fd(stdio[0].raw(), 0),
        process::clone_fd(stdio[1].raw(), 1),
        process::clone_fd(stdio[2].raw(), 2),
    ];
    let pid = process::spawn_endowed(
        action.program.as_bytes(),
        &argv_ptrs,
        &env_ptrs,
        cwd.as_bytes(),
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        DEFAULTED,
        Endowment {
            caps: action.caps,
            ids: Some((action.uid, action.uid)),
            ..Endowment::NONE
        },
    );
    if pid <= 0 {
        return Err(Reply::denied(format!(
            "{} could not be started: {}",
            action.program,
            SyscallError::from_errno(-pid)
        )));
    }
    Ok(pid as u32)
}

/// Wait for the mechanism, relaying the signals its requester sends. A
/// requester that goes away leaves it running.
fn wait(conn: &mut Conn, pid: u32, id: u64) -> Exit {
    let exited = pidfd::pidfd_open_owned(pid);
    let relay = |conn: &mut Conn| {
        for signal in conn.signals(id).unwrap_or_default() {
            let _ = process::kill(pid, signal);
        }
    };
    relay(conn);
    while let Some(exited) = &exited {
        let mut fds = vec![UserPollFd {
            fd: exited.raw(),
            events: POLLIN,
            revents: 0,
        }];
        if conn.open {
            fds.push(conn.poll(POLLIN));
        }
        if fs::poll(&mut fds, -1).is_err() {
            continue;
        }
        if fds[0].revents & (POLLIN | POLLHUP) != 0 {
            break;
        }
        relay(conn);
    }
    let mut status = 0;
    let reaped = loop {
        match process::waitpid_raw(pid as i32, &mut status, 0) {
            rc if rc == i64::from(slopos_abi::Errno::EINTR.raw()) => {}
            rc => break rc > 0,
        }
    };
    match reaped.then(|| process::wait_status(status)) {
        Some(WaitStatus::Exited(code)) => Exit::Code(code),
        Some(WaitStatus::Signalled(signal)) => Exit::Signal(signal),
        _ => Exit::Code(-1),
    }
}
