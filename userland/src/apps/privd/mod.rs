//! `/sbin/privd` — runs a named action's mechanism, the base program the
//! sealed policy names or an enabled extension's program its activation
//! declares, as a fresh child holding what the action names and nothing of
//! its requester's but the descriptors it sent. An extension's mechanism runs
//! as the extension's first user.
//!
//! Who asks is what the kernel recorded when the connection was made: the
//! uid, a pidfd and the program; who that uid is, the accounts record says
//! as the request comes. Every failure on the way is a denial, and a
//! requester whose process has exited is refused, before consent and after.
//! A password typed in the dialog is checked by `/sbin/authcheck`, never
//! here.
//!
//! `SIGHUP` has it read the extensions' actions again, as init asks when they
//! change; a request reads the policy as it stands when the request comes.
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

mod answers;
mod consent;
mod people;

use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use slopos_abi::extension::Label;
use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::signal::{SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, sig_bit};
use slopos_abi::syscall::{
    F_GETFD, MSG_NOSIGNAL, O_CLOEXEC, O_NONBLOCK, POLLHUP, POLLIN, POLLOUT, UserPollFd,
};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_extension_core::declare::mount_point;
use slopos_priv_core::consent::{Ask, Kind, Refusal};
use slopos_priv_core::policy::{Principal, REMOTED};
use slopos_priv_core::wire::{self, CONNECTION, Exit, Reply, Request, kind};
use slopos_priv_core::{
    Action, ENV_ACTION, ENV_UID, INIT, MECHANISM_LANG, MECHANISM_PATH, POLICY, People, Policy,
    Question, Refused, Requester, SOCKET, Verdict, elf_interpreted,
};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};

use self::answers::{Asked, Checks, Kept};
use self::consent::{Broker, Waiting};
use crate::init_control;
use crate::readiness::ReadinessNotifier;
use crate::syscall::process::{self, Endowment, WaitStatus};
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, net, pidfd};

/// Init's readiness notifier is fd 3, the consent channel fd 4.
pub const CONSENT_FD: RawFd = 4;
const REMOTE_CONF: &str = "/usr/share/slopos/remote/remote.conf";
/// Per uid, for every requester but root and the paired agent, whom nothing
/// else can crowd out: no person takes another's connections.
const MAX_CONNECTIONS: usize = 32;
/// Of every uid together, so the people do not multiply privd's threads.
const MAX_POOLED: usize = 128;
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

/// Where an extension's action was read from.
struct Origin {
    action: String,
    extension: String,
    generation: u64,
    login: u32,
}

/// The base's actions and the running extensions', and where each of the
/// latter was read from.
struct Loaded {
    policy: Policy,
    origins: Vec<Origin>,
}

struct Daemon {
    base: Result<Policy, String>,
    loaded: Mutex<Result<Arc<Loaded>, String>>,
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
    kept: Kept,
    checks: Checks,
}

impl Daemon {
    fn loaded(&self) -> Result<Arc<Loaded>, String> {
        self.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Read the extensions' actions again, the base's kept as read at start.
    fn reload(&self) {
        let fresh = self
            .base
            .clone()
            .map(|base| Arc::new(with_extensions(base)));
        *self
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh;
        self.kept.clear();
        log!("privd: read the extensions' actions again");
    }

    fn inherited_runs(&self) -> bool {
        !self.inherited.is_empty()
            && init_control::privd_threads_elsewhere()
                .iter()
                .any(|thread| self.inherited.contains(thread))
    }
}

static DAEMON: OnceLock<Daemon> = OnceLock::new();

pub(super) static DRAINING: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);
/// Requests between being read and being answered.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// The write end of the pipe the accept loop polls beside the listener.
static WAKE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn drain(_: i32) {
    DRAINING.store(true, Ordering::SeqCst);
    let _ = fs::write_slice(WAKE.load(Ordering::Relaxed), &[0]);
}

extern "C" fn reload(_: i32) {
    RELOAD.store(true, Ordering::SeqCst);
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
    process::set_signal_handler(SIGHUP, reload);
    let channel = consent_channel();
    let base = base_policy();
    if let Err(why) = &base {
        log!("privd: {why}; every request will be refused");
    }
    let daemon = Daemon {
        loaded: Mutex::new(base.clone().map(|base| Arc::new(with_extensions(base)))),
        base,
        paired: std::path::Path::new(REMOTE_CONF).is_file(),
        consent: Broker::start(channel),
        running: Mutex::new(Vec::new()),
        cwd: Mutex::new(()),
        connections: Mutex::new(Vec::new()),
        inherited: init_control::privd_threads_elsewhere(),
        kept: Kept::default(),
        checks: Checks::default(),
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
        if RELOAD.swap(false, Ordering::SeqCst) {
            daemon.reload();
        }
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
        if fds[1].revents & POLLIN != 0 {
            while matches!(fs::read_slice(woken.raw(), &mut [0u8; 16]), Ok(n) if n > 0) {}
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
        let root = peer.uid == 0 && peer.base().is_some();
        let pooled = !root && !peer.requester(paired).is_paired();
        let key = (peer.uid, peer.program.path().map(str::to_owned));
        let mut connections = daemon.connections.lock().ok()?;
        let pool = || connections.iter().filter(|(_, pooled)| *pooled);
        let full = pool().count() >= MAX_POOLED
            || pool().filter(|((uid, _), _)| *uid == key.0).count() >= MAX_CONNECTIONS;
        if (pooled && full)
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
/// principal of any action names a dynamically linked base program: the
/// loader would take a library from outside the base for it while its
/// identity still read as the base's.
fn base_policy() -> Result<Policy, String> {
    let policy = crate::privd::whole_policy()?;
    for action in policy.actions() {
        static_principals(action).map_err(|why| format!("{POLICY}: {why}"))?;
    }
    Ok(policy.for_boot(crate::privd::on_the_medium()))
}

/// `policy` with the running extensions' actions beside it, each refused
/// alone. An extension's own program stands as a principal only while it
/// maps nothing from outside the base and its activation.
fn with_extensions(mut policy: Policy) -> Loaded {
    let mut origins = Vec::new();
    let actions = crate::activation::actions().unwrap_or_else(|why| {
        log!("privd: no extension's actions: {why}");
        Default::default()
    });
    for skipped in actions.skipped {
        log!("privd: no actions of {skipped}");
    }
    for loaded in actions.loaded {
        let name = loaded.action.name.clone();
        if let Err(why) = static_principals(&loaded.action) {
            log!("privd: {}: {why}", loaded.extension);
        } else if !policy.add(loaded.action) {
            log!("privd: {}: {name} is an action already", loaded.extension);
        } else {
            origins.push(Origin {
                action: name,
                extension: loaded.extension,
                generation: loaded.generation,
                login: loaded.login,
            });
        }
    }
    Loaded { policy, origins }
}

fn static_principals(action: &Action) -> Result<(), String> {
    for allow in &action.allows {
        let path = match &allow.principal {
            Principal::Program(path) => path.as_str(),
            Principal::Paired => REMOTED,
            Principal::Root | Principal::User | Principal::Admin | Principal::Extension(..) => {
                continue;
            }
        };
        let image = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        if elf_interpreted(&image) != Some(false) {
            return Err(format!(
                "{}: {path} is not a statically linked program",
                action.name
            ));
        }
    }
    Ok(())
}

fn listen() -> Result<OwnedFd, String> {
    let fd = net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0)
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(SOCKET);
    net::bind_unix(fd.raw(), SOCKET.as_bytes()).map_err(|e| e.to_string())?;
    std::os::unix::fs::chown(SOCKET, Some(0), Some(0)).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| e.to_string())?;
    net::listen(fd.raw(), 16).map_err(|e| e.to_string())?;
    Ok(fd)
}

/// Who made a connection, as the kernel recorded it at `connect`: the
/// program it ran then, which stands only while it still runs it.
struct Peer {
    pid: u32,
    uid: u32,
    program: Attested,
    pidfd: OwnedFd,
}

/// A program as the kernel attests it.
#[derive(Clone, PartialEq, Eq)]
enum Attested {
    Base(String),
    /// A program of an extension's activation, by its path at `/opt/<name>`.
    Extension(String, Label),
    Other,
}

impl Attested {
    fn path(&self) -> Option<&str> {
        match self {
            Attested::Base(path) | Attested::Extension(path, _) => Some(path),
            Attested::Other => None,
        }
    }
}

const LAPSED: &str = "the requester no longer runs the program it connected as";

/// What the peer at `fd` runs now.
fn attested(fd: &OwnedFd) -> Result<Attested, String> {
    // Label first: a program that stops holding never holds again, so one
    // that lapses between the two reads answers no path.
    let label = net::peer_extension(fd.raw()).map_err(|e| format!("SO_PEEREXTENSION: {e}"))?;
    let Some(path) = net::peer_program(fd.raw()).map_err(|e| format!("SO_PEERPROGRAM: {e}"))?
    else {
        return Ok(Attested::Other);
    };
    let path = String::from_utf8(path).map_err(|_| "SO_PEERPROGRAM: not UTF-8".to_owned())?;
    Ok(match label {
        Some(label) => Attested::Extension(path, label),
        None => Attested::Base(path),
    })
}

impl Peer {
    fn of(fd: &OwnedFd) -> Result<Peer, String> {
        let cred = net::peer_cred(fd.raw()).map_err(|e| format!("SO_PEERCRED: {e}"))?;
        let pidfd = net::peer_pidfd(fd.raw()).map_err(|e| format!("SO_PEERPIDFD: {e}"))?;
        Ok(Peer {
            pid: cred.pid as u32,
            uid: cred.uid,
            program: attested(fd)?,
            pidfd,
        })
    }

    fn base(&self) -> Option<&str> {
        match &self.program {
            Attested::Base(path) => Some(path),
            _ => None,
        }
    }

    /// Whether the program read at `accept` still stands: the peer has not
    /// exec'd, mapped code from elsewhere or exited since.
    fn still_runs(&self, fd: &OwnedFd) -> bool {
        attested(fd).is_ok_and(|now| now == self.program)
    }

    fn requester(&self, paired: bool) -> Requester<'_> {
        match &self.program {
            Attested::Extension(path, label) => Requester::extension(self.uid, path, *label),
            _ => Requester::new(self.uid, self.base(), paired),
        }
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
            self.program.path().unwrap_or("outside the base")
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
    let loaded = daemon
        .loaded()
        .map_err(|why| Reply::denied(format!("privd has no policy: {why}")))?;
    let action = loaded
        .policy
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
    let tasks = action
        .names_people()
        .then(init_control::Tasks::try_read)
        .flatten();
    let people = match action.names_people() {
        true => people::now(tasks.as_ref()),
        false => Ok(People::default()),
    };
    let requester = peer.requester(daemon.paired);
    let question = match action.admits(&requester, people.as_ref().unwrap_or(&People::default())) {
        Verdict::Run => None,
        Verdict::Ask(question) => Some(question),
        Verdict::Refuse(Refused::NotAllowed) => {
            let unknown = people
                .err()
                .map(|why| format!(": {why}"))
                .unwrap_or_default();
            return Err(Reply::denied(format!(
                "{} may not ask for {}{unknown}",
                peer.describe(),
                action.name
            )));
        }
        Verdict::Refuse(why) => return Err(Reply::denied(why.to_string())),
    };
    still_wanted(peer, conn, request.id)?;
    let cwd = match fds.get(wire::STDIO) {
        Some(dir) => directory_of(daemon, dir)?,
        None => "/".to_owned(),
    };
    let origin = loaded.origins.iter().find(|o| o.action == action.name);
    let mut allowed = "";
    if let Some(question) = question {
        drop(claim(daemon, action)?);
        let mut command = vec![action.program.as_bytes().to_vec()];
        command.extend(request.args.iter().cloned());
        let session = tasks.as_ref().and_then(|tasks| {
            let login = tasks.login_of(peer.pid).filter(|&login| login != 0)?;
            Some((login, tasks.eldest(login)?))
        });
        let kept = action.keep.zip(session).map(|(keep, session)| {
            let asked = Asked {
                uid: peer.uid,
                session,
                action: action.name.clone(),
                origin: origin.map(|o| (o.extension.clone(), o.generation)),
                kind: question.kind,
                command: command.clone(),
                cwd: cwd.clone(),
            };
            (asked, keep)
        });
        allowed = ", allowed earlier at the machine";
        if !kept
            .as_ref()
            .is_some_and(|(asked, keep)| daemon.kept.holds(asked, *keep, &question.admins))
        {
            let ask = Ask {
                id: 0,
                action: action.name.clone(),
                describe: action.describe.clone(),
                program: peer.program.path().map(str::to_owned),
                uid: peer.uid,
                pid: peer.pid,
                cwd: cwd.clone(),
                command,
                kind: question.kind,
                person: Some(question.person.name.clone()),
                admins: question.admins.clone(),
            };
            let by = answer(daemon, peer, conn, request.id, ask)?;
            judged_again(action, &requester, &question, by.as_deref())?;
            if let Some((asked, _)) = kept {
                daemon.kept.keep(asked, by);
            }
            allowed = ", allowed at the machine";
        }
    }
    let _claim = claim(daemon, action)?;
    still_wanted(peer, conn, request.id)?;
    if let Some(origin) = origin {
        crate::activation::holds(&origin.extension, origin.generation, &action.program)
            .map_err(Reply::denied)?;
    }
    let pid = spawn(
        action,
        origin,
        peer.uid,
        &request.args,
        &fds[..wire::STDIO],
        &cwd,
    )?;
    log!(
        "privd: {} for {}: started pid {pid}{allowed}",
        action.name,
        peer.describe(),
    );
    conn.reply(&Reply::Started { pid }, request.id);
    Ok(wait(conn, pid, request.id))
}

/// Put `ask` to the person at the machine and hold what they answer to what
/// it takes: a click, or the password of the person it names; the
/// administrator who answered, for a question an administrator answers.
fn answer(
    daemon: &Daemon,
    peer: &Peer,
    conn: &mut Conn,
    id: u64,
    ask: Ask,
) -> Result<Option<String>, Reply> {
    if !ask.fits() {
        return Err(Reply::denied(
            "the command is too long to show the person at the machine",
        ));
    }
    let (action, kind, person) = (ask.action.clone(), ask.kind, ask.person.clone());
    let mut pending = Pending { peer, conn, id };
    let answer = daemon
        .consent
        .ask(ask, &mut pending)
        .map_err(|refusal| Reply::Denied {
            reason: refusal.to_string(),
            by_person: refusal.by_person(),
        })?;
    let whose = match kind {
        Kind::Confirm => return Ok(None),
        Kind::Password => person.ok_or_else(|| Reply::denied("the question named nobody"))?,
        Kind::Admin => answer
            .admin
            .clone()
            .ok_or_else(|| Reply::denied("no administrator answered"))?,
    };
    let secret = answer
        .secret
        .ok_or_else(|| Reply::denied("no password was typed"))?;
    match daemon.checks.check(&whose, &secret) {
        Ok(true) => {
            log!(
                "privd: {action} for {}: allowed by {whose}'s password",
                peer.describe()
            );
            Ok(answer.admin)
        }
        Ok(false) => Err(Reply::Denied {
            reason: "the password typed was wrong".into(),
            by_person: true,
        }),
        Err(why) => Err(Reply::denied(format!(
            "the password could not be checked: {why}"
        ))),
    }
}

/// Hold an answered request to the people as they stand now: it runs once
/// it would ask the same of the same person, the administrator who answered
/// still listed, or would run unasked.
fn judged_again(
    action: &Action,
    requester: &Requester<'_>,
    question: &Question,
    by: Option<&str>,
) -> Result<(), Reply> {
    let tasks = init_control::Tasks::try_read();
    let people = people::now(tasks.as_ref())
        .map_err(|why| Reply::denied(format!("the answer cannot be held to the people: {why}")))?;
    match action.admits(requester, &people) {
        Verdict::Run => Ok(()),
        Verdict::Ask(again)
            if again.kind == question.kind
                && again.person.uid == question.person.uid
                && by.is_none_or(|by| again.admins.iter().any(|admin| admin == by)) =>
        {
            Ok(())
        }
        _ => Err(Reply::denied(
            "who may allow it changed while the person was asked",
        )),
    }
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

/// Start `action`'s mechanism for the requester `uid`; an extension's sees
/// its own `bin` on its path, runs with no supplementary group and carries
/// its extension's login tag, by which init ends it once the extension stops.
fn spawn(
    action: &Action,
    extension: Option<&Origin>,
    uid: u32,
    args: &[Vec<u8>],
    stdio: &[OwnedFd],
    cwd: &str,
) -> Result<u32, Reply> {
    let name = action.program.rsplit('/').next().unwrap_or(&action.program);
    let mut argv = vec![CString::new(name).map_err(|_| Reply::denied("bad program name"))?];
    for arg in args {
        argv.push(
            CString::new(arg.as_slice()).map_err(|_| Reply::denied("an argument holds NUL"))?,
        );
    }
    let path = match extension {
        Some(origin) => format!("{MECHANISM_PATH}:{}/bin", mount_point(&origin.extension)),
        None => MECHANISM_PATH.to_owned(),
    };
    let env = [
        format!("PATH={path}"),
        format!("LANG={MECHANISM_LANG}"),
        format!("{ENV_ACTION}={}", action.name),
        format!("{ENV_UID}={uid}"),
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
            groups: extension.map(|_| &[][..]),
            login: extension.map(|origin| origin.login),
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
