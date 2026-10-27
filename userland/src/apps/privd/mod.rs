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
mod sessions;

use std::collections::VecDeque;
use std::ffi::CString;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use slopos_abi::extension::Label;
use slopos_abi::fs::UserFsStat;
use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::signal::{SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, sig_bit};
use slopos_abi::syscall::{
    F_GETFD, MSG_NOSIGNAL, O_CLOEXEC, O_NONBLOCK, POLLHUP, POLLIN, POLLOUT, UserPollFd,
};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_extension_core::declare::mount_point;
use slopos_priv_core::consent::{
    Ask, Dialog, EXPIRY_MS, Kind, Refusal, Secret, WRONG_PASSWORD, icon_path, named_extension,
    valid_app,
};
use slopos_priv_core::policy::{Principal, REMOTED};
use slopos_priv_core::sessions::LINKS;
use slopos_priv_core::wire::{self, CONNECTION, Exit, Reply, Request, kind};
use slopos_priv_core::words::privd as said;
use slopos_priv_core::{
    Action, ENV_ACTION, ENV_UID, INIT, MECHANISM_LANG, MECHANISM_PATH, POLICY, People, Policy,
    Question, Reach, Refused, Requester, RunsAs, SOCKET, Verdict, elf_interpreted,
};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};

use self::answers::{Asked, Checks, Kept};
use self::consent::{Broker, Waiting};
use self::sessions::Seen;
use crate::readiness::ReadinessNotifier;
use crate::syscall::process::{self, Endowment, WaitStatus};
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, net, pidfd};
use crate::{app_entry, init_control};

/// Init's readiness notifier is fd 3. An init that keeps one desktop hands
/// privd its one link at `CONSENT_FD`; one that keeps sessions leaves that
/// empty, so a privd of an older base asks nobody on a screen that may be
/// anyone's, and hands consent link `n` at `LINKS_FD + n` and past them the
/// pair a privd started with [`SESSIONS_ARG`] hears the table on. A
/// compositor holds its own link at `CONSENT_FD`.
pub const CONSENT_FD: RawFd = 4;
pub const LINKS_FD: RawFd = CONSENT_FD + 1;
pub const SESSIONS_FD: RawFd = LINKS_FD + LINKS as RawFd;
pub const SESSIONS_ARG: &str = "--hear-sessions";
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
    /// One per consent link, by its number.
    consent: Vec<Broker>,
    seen: &'static Seen,
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
    let hears = std::env::args().nth(1).as_deref() == Some(SESSIONS_ARG);
    let (seen, links) = match hears {
        true => (
            Seen::hear(init_channel(SESSIONS_FD)),
            (0..LINKS)
                .map(|i| init_channel(LINKS_FD + i as RawFd))
                .collect(),
        ),
        false => (Seen::legacy(), vec![init_channel(CONSENT_FD)]),
    };
    let consent = links
        .into_iter()
        .zip(&seen.shown)
        .map(|(link, shown)| Broker::start(link, shown.clone()))
        .collect();
    let base = base_policy();
    if let Err(why) = &base {
        log!("privd: {why}; every request will be refused");
    }
    let daemon = Daemon {
        loaded: Mutex::new(base.clone().map(|base| Arc::new(with_extensions(base)))),
        base,
        paired: std::path::Path::new(REMOTE_CONF).is_file(),
        consent,
        seen,
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
                let denial = Reply::denied(said::FAULT);
                let _ = send(&fd, &denial, CONNECTION);
                continue;
            }
        };
        let Some(admitted) = Admitted::take(daemon, &peer, daemon.paired) else {
            let denial = Reply::denied(said::CROWDED);
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

/// The end of a socket pair init made at `at`: either end of a pair names
/// its maker, so no other process can hand privd one that passes.
fn init_channel(at: RawFd) -> Option<OwnedFd> {
    Sys::fcntl(at, F_GETFD as i32, 0).ok()?;
    let _ = fs::set_fd_cloexec(at);
    // SAFETY: init handed this descriptor over and nothing else here owns it.
    let fd = unsafe { OwnedFd::from_raw(at) };
    let from_init = net::peer_cred(fd.raw()).is_ok_and(|cred| cred.uid == 0)
        && net::peer_program(fd.raw()).ok().flatten().as_deref() == Some(INIT.as_bytes());
    if !from_init {
        log!("privd: fd {at} is not a channel init made; it is not heard");
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
            Principal::Root
            | Principal::Greeter
            | Principal::User
            | Principal::Admin
            | Principal::Extension(..) => continue,
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

fn fault(said: &str, why: impl std::fmt::Display) -> Reply {
    log!("privd: {why}");
    Reply::denied(said)
}

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
    /// What the requester's person typed at its terminal, for the request
    /// that asked.
    secret: Option<(u64, Secret)>,
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
                    Some((kind::SECRET, mut payload)) => {
                        slopos_priv_core::consent::wipe(&mut payload);
                        self.frames.scrub();
                        continue;
                    }
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
                Ok(Some((kind::SECRET, mut payload))) => {
                    let decoded = wire::decode_secret(&payload);
                    slopos_priv_core::consent::wipe(&mut payload);
                    self.frames.scrub();
                    match decoded {
                        Ok((of, secret)) if of == id && self.secret.is_none() => {
                            self.secret = Some((of, secret));
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
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

    /// What the person typed at the requester's terminal for request `id`
    /// once privd asked with `prompt`, a [`Reply::Prompt`], waiting as long
    /// as the dialog would.
    fn typed(&mut self, peer: &Peer, id: u64, prompt: Reply) -> Result<Secret, Reply> {
        self.secret = None;
        self.reply(&prompt, id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(EXPIRY_MS);
        loop {
            still_wanted(peer, self, id)?;
            if let Some((_, secret)) = self.secret.take() {
                return Ok(secret);
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(Reply::Denied {
                    reason: Refusal::Expired.to_string(),
                    by_person: true,
                });
            }
            let _ = fs::poll(&mut [self.poll(POLLIN)], left.as_millis().min(250) as i64);
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
        secret: None,
        open: true,
    };
    while conn.open {
        match conn.next_request() {
            Ok(Some((request, fds, withdrawn))) => {
                IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
                if DRAINING.load(Ordering::SeqCst) {
                    let denial = Reply::denied(said::RESTARTING);
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
                conn.reply(&Reply::denied(said::UNREADABLE), CONNECTION);
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
    let group = action.exclusive_group().map(str::to_owned);
    if let Some(group) = &group
        && daemon.inherited_runs()
    {
        let why = format_args!("what a predecessor started may hold {group}");
        return Err(fault(said::INHERITED, why));
    }
    let mut running = daemon
        .running
        .lock()
        .map_err(|_| fault(said::FAULT, "the exclusive groups' lock is poisoned"))?;
    if let Some(group) = &group {
        if running.contains(group) {
            return Err(fault(said::BUSY, format_args!("{group} is running")));
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
    app: Option<String>,
}

impl Waiting for Pending<'_> {
    fn shown(&mut self) {
        let asking = Reply::Asking {
            app: self.app.clone(),
        };
        self.conn.reply(&asking, self.id);
    }

    fn withdrawn(&mut self) -> bool {
        still_wanted(self.peer, self.conn, self.id).is_err()
    }
}

/// Refused once the requester has exited, runs another program than it
/// connected as, or has withdrawn request `id`.
fn still_wanted(peer: &Peer, conn: &mut Conn, id: u64) -> Result<(), Reply> {
    if !peer.alive() {
        return Err(fault(said::LAPSED, "the requester has exited"));
    }
    if !peer.still_runs(&conn.fd) {
        return Err(fault(
            said::LAPSED,
            "the requester runs another program than it connected as",
        ));
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
        .map_err(|why| fault(said::FAULT, format_args!("no policy: {why}")))?;
    let action = loaded.policy.action(&request.action).ok_or_else(|| {
        fault(
            said::NO_ACTION,
            format_args!("no action {}", request.action),
        )
    })?;
    if !action.accepts(&request.args()) {
        let why = format_args!(
            "{} is not an argument list {} takes",
            shown(&request.args),
            action.name
        );
        return Err(fault(said::NOT_THAT_WAY, why));
    }
    if request.cwd != action.cwd {
        return Err(Reply::denied(match action.cwd {
            true => said::NO_FOLDER,
            false => said::UNWANTED_FOLDER,
        }));
    }
    if !peer.still_runs(&conn.fd) {
        return Err(fault(
            said::LAPSED,
            "the requester runs another program than it connected as",
        ));
    }
    let as_requester = action.runs_as == RunsAs::Requester;
    let tasks = (action.names_people() || as_requester)
        .then(init_control::Tasks::try_read)
        .flatten();
    let people = match action.names_people() {
        true => people::now(tasks.as_ref(), daemon.seen),
        false => Ok(People::default()),
    };
    let (reach, link) = reach(daemon, peer, tasks.as_ref(), request.terminal);
    let requester = peer.requester(daemon.paired).reached(reach);
    // People unknown are judged as if someone were logged in, so `alone`
    // never holds for want of a reading.
    let unknown = People {
        people: Vec::new(),
        logged_in: vec![slopos_abi::fs::NOBODY_UID],
    };
    let question = match action.admits(&requester, people.as_ref().unwrap_or(&unknown)) {
        Verdict::Run => None,
        Verdict::Ask(question) => Some(question),
        Verdict::Refuse(Refused::NotAllowed) => {
            if let Err(why) = people {
                log!("privd: {}: the people are unknown: {why}", action.name);
            }
            return Err(Reply::denied(Refused::NotAllowed.to_string()));
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
            Some(((login, tasks.eldest(login)?), tasks.parent_of(peer.pid)?))
        });
        let kept = action.keep.zip(session).map(|(keep, (session, parent))| {
            let asked = Asked {
                uid: peer.uid,
                session,
                parent,
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
            let ask = ask_about(action, peer, &question, origin, cwd.clone(), command);
            let by = match (reach, link) {
                (Reach::Terminal, _) => {
                    let runs_it = origin
                        .map(|o| named_extension(&o.extension))
                        .filter(|runs_it| ask.app.as_ref() != Some(runs_it));
                    let headline = Dialog::new(ask).headline();
                    let about = match runs_it {
                        Some(runs_it) => format!("{headline} {runs_it} runs it."),
                        None => headline,
                    };
                    let asked = (action.name.as_str(), about.as_str());
                    typed_answer(daemon, peer, conn, request.id, asked, &question)?;
                    None
                }
                (_, Some(link)) => answer(daemon, peer, conn, request.id, link, ask)?,
                (_, None) => return Err(Reply::denied(Refused::Unreached.to_string())),
            };
            judged_again(daemon, peer, request, action, &question, by.as_deref())?;
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
            .map_err(|why| fault(said::EXTENSION_CHANGED, why))?;
    }
    let runs_as = match action.runs_as {
        RunsAs::Uid(uid) => Identity {
            ids: (uid, uid),
            groups: origin.map(|_| Vec::new()),
            login: origin.map(|origin| origin.login),
        },
        RunsAs::Requester => requester_identity(daemon, peer, tasks.as_ref())?,
    };
    if DRAINING.load(Ordering::SeqCst) {
        return Err(Reply::denied(said::RESTARTING));
    }
    if fds.get(wire::STDIO).is_some_and(|dir| !still_at(dir, &cwd)) {
        return Err(Reply::denied(said::FOLDER_MOVED));
    }
    let pid = spawn(
        action,
        origin,
        peer.uid,
        &runs_as,
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

fn ask_about(
    action: &Action,
    peer: &Peer,
    question: &Question,
    origin: Option<&Origin>,
    cwd: String,
    command: Vec<Vec<u8>>,
) -> Ask {
    let (app, icon) = named(&peer.program);
    Ask {
        id: 0,
        action: action.name.clone(),
        describe: action.describe.clone(),
        program: peer.program.path().map(str::to_owned),
        uid: peer.uid,
        pid: peer.pid,
        cwd,
        command,
        kind: question.kind,
        person: Some(question.person.name.clone()),
        admins: question.admins.clone(),
        app,
        icon,
        detail: action.detail.clone(),
        verb: Some(action.verb().to_owned()),
        destructive: action.destructive,
        extension: origin.map(|o| o.extension.clone()),
    }
}

/// What the dialog names a requester running `program`, and its icon: the
/// base's app entry that runs it, else its file name; an extension's program
/// as its extension, with neither a name nor an icon its own entries could
/// borrow from another app; nothing for a program the kernel attests as none
/// of the system's.
fn named(program: &Attested) -> (Option<String>, Option<String>) {
    match program {
        Attested::Base(path) => match app_entry::of_base(path).filter(|e| valid_app(&e.name)) {
            Some(entry) => {
                let icon = entry
                    .icon
                    .and_then(|icon| icon.into_os_string().into_string().ok())
                    .filter(|icon| icon_path(icon));
                (Some(entry.name), icon)
            }
            None => {
                let file = path.rsplit('/').next().filter(|file| valid_app(file));
                (file.map(str::to_owned), None)
            }
        },
        Attested::Extension(_, label) => {
            let extension = String::from_utf8_lossy(label.name());
            (Some(named_extension(&extension)), None)
        }
        Attested::Other => (None, None),
    }
}

/// Put `ask` to the person at the machine and hold what they answer to what
/// it takes: a click, or the password of the person it names; the
/// administrator who answered, for a question an administrator answers.
fn answer(
    daemon: &Daemon,
    peer: &Peer,
    conn: &mut Conn,
    id: u64,
    link: u8,
    ask: Ask,
) -> Result<Option<String>, Reply> {
    if !ask.fits() {
        return Err(Reply::denied(said::TOO_LONG));
    }
    let (action, kind, person) = (ask.action.clone(), ask.kind, ask.person.clone());
    let mut pending = Pending {
        peer,
        conn,
        id,
        app: ask.app.clone(),
    };
    let broker = daemon
        .consent
        .get(usize::from(link))
        .ok_or_else(|| Reply::denied(Refusal::NoAgent.to_string()))?;
    let answer = broker
        .ask(ask, &mut pending)
        .map_err(|refusal| Reply::Denied {
            reason: refusal.to_string(),
            by_person: refusal.by_person(),
        })?;
    let whose = match kind {
        Kind::Confirm => return Ok(None),
        Kind::Password => person.ok_or_else(|| Reply::denied(said::INCOMPLETE))?,
        Kind::Admin => answer
            .admin
            .clone()
            .ok_or_else(|| fault(said::INCOMPLETE, "no administrator answered"))?,
    };
    let secret = answer
        .secret
        .ok_or_else(|| fault(said::INCOMPLETE, "no password was typed"))?;
    checked(daemon, peer, &action, &whose, &secret)?;
    Ok(answer.admin)
}

/// The person's own password, typed at the requester's terminal after a line
/// saying what it allows: a console session's question, which no dialog
/// shows.
fn typed_answer(
    daemon: &Daemon,
    peer: &Peer,
    conn: &mut Conn,
    id: u64,
    (action, about): (&str, &str),
    question: &Question,
) -> Result<(), Reply> {
    if question.kind != Kind::Password {
        return Err(Reply::denied(Refused::Unreached.to_string()));
    }
    let whose = &question.person.name;
    let prompt = Reply::Prompt {
        about: Some(about.to_owned()),
        prompt: format!("Password for {whose}: "),
    };
    let secret = conn.typed(peer, id, prompt)?;
    checked(daemon, peer, action, whose, &secret)
}

fn checked(
    daemon: &Daemon,
    peer: &Peer,
    action: &str,
    whose: &str,
    secret: &Secret,
) -> Result<(), Reply> {
    match daemon.checks.check(whose, secret) {
        Ok(true) => {
            log!(
                "privd: {action} for {}: allowed by {whose}'s password",
                peer.describe()
            );
            Ok(())
        }
        Ok(false) => Err(Reply::Denied {
            reason: WRONG_PASSWORD.into(),
            by_person: true,
        }),
        Err(why) => {
            log!("privd: {whose}'s password could not be checked: {why}");
            Err(Reply::denied(said::UNCHECKED))
        }
    }
}

/// Where the person asking is asked: their session's desktop on screen, its
/// terminal, or nowhere, and the link that asks on screen.
fn reach(
    daemon: &Daemon,
    peer: &Peer,
    tasks: Option<&init_control::Tasks>,
    terminal: bool,
) -> (Reach, Option<u8>) {
    let login = tasks
        .and_then(|tasks| tasks.login_of(peer.pid))
        .unwrap_or(0);
    daemon.seen.reach(login, peer.uid, terminal)
}

/// Hold an answered request to the people as they stand now: it runs once
/// it would ask the same of the same person, the administrator who answered
/// still listed, or would run unasked.
fn judged_again(
    daemon: &Daemon,
    peer: &Peer,
    request: &Request,
    action: &Action,
    question: &Question,
    by: Option<&str>,
) -> Result<(), Reply> {
    let tasks = init_control::Tasks::try_read();
    let people = people::now(tasks.as_ref(), daemon.seen).map_err(|why| fault(said::FAULT, why))?;
    let (reach, _) = reach(daemon, peer, tasks.as_ref(), request.terminal);
    let requester = peer.requester(daemon.paired).reached(reach);
    match action.admits(&requester, &people) {
        Verdict::Run => Ok(()),
        Verdict::Ask(again)
            if again.kind == question.kind
                && again.person.uid == question.person.uid
                && by.is_none_or(|by| again.admins.iter().any(|admin| admin == by)) =>
        {
            Ok(())
        }
        _ => Err(Reply::denied(said::ANSWERED_ELSEWHERE)),
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
        .map_err(|_| fault(said::FAULT, "the directory lock is poisoned"))?;
    match Sys::fchdir(dir.raw()) {
        Ok(()) => {}
        Err(e) if e.raw() == -slopos_abi::Errno::ESTALE.raw() => {
            return Err(Reply::denied(said::FOLDER_GONE));
        }
        Err(_) => return Err(Reply::denied(said::NOT_A_FOLDER)),
    }
    let path = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned));
    if std::env::set_current_dir("/").is_err() {
        log!("privd: cannot return to /");
    }
    path.filter(|p| p.starts_with('/'))
        .ok_or_else(|| fault(said::FAULT, "the working directory has no path"))
}

/// Whether `path` still leads to the directory `dir`, the one the request
/// sent: a mechanism is given its directory by path, which the requester's
/// processes could otherwise point elsewhere once the person was asked.
fn still_at(dir: &OwnedFd, path: &str) -> bool {
    let mut sent = UserFsStat::default();
    let stat = (&mut sent as *mut UserFsStat).cast();
    Sys::fstat(dir.raw(), stat).is_ok()
        && std::fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == (sent.st_dev, sent.st_ino))
}

/// Whom a mechanism runs as: its uid and gid, and its groups and login tag,
/// `None` keeping privd's.
struct Identity {
    ids: (u32, u32),
    groups: Option<Vec<u32>>,
    login: Option<u32>,
}

/// The requester as a mechanism run as them: their uid, their own group and
/// `admin` while the accounts record names them an administrator, as
/// `initgroups` gives them, and the login tag of their session.
fn requester_identity(
    daemon: &Daemon,
    peer: &Peer,
    tasks: Option<&init_control::Tasks>,
) -> Result<Identity, Reply> {
    let login = tasks
        .and_then(|tasks| tasks.login_of(peer.pid))
        .ok_or_else(|| fault(said::FAULT, "the requester's session is unknown"))?;
    let admin = match peer.uid {
        0 => false,
        uid => {
            people::now(tasks, daemon.seen)
                .map_err(|why| fault(said::FAULT, why))?
                .person(uid)
                .ok_or_else(|| Reply::denied(said::NO_ACCOUNT))?
                .admin
        }
    };
    let mut groups = vec![peer.uid];
    if admin {
        groups.push(slopos_abi::fs::ADMIN_GID);
    }
    Ok(Identity {
        ids: (peer.uid, peer.uid),
        groups: Some(groups),
        login: Some(login),
    })
}

/// Start `action`'s mechanism for the requester `uid`, as `runs_as`; an
/// extension's sees its own `bin` on its path.
fn spawn(
    action: &Action,
    extension: Option<&Origin>,
    uid: u32,
    runs_as: &Identity,
    args: &[Vec<u8>],
    stdio: &[OwnedFd],
    cwd: &str,
) -> Result<u32, Reply> {
    let name = action.program.rsplit('/').next().unwrap_or(&action.program);
    let mut argv =
        vec![CString::new(name).map_err(|_| fault(said::FAULT, "a program name holds NUL"))?];
    for arg in args {
        argv.push(CString::new(arg.as_slice()).map_err(|_| Reply::denied(said::NOT_THAT_WAY))?);
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
            ids: Some(runs_as.ids),
            groups: runs_as.groups.as_deref(),
            login: runs_as.login,
        },
    );
    if pid <= 0 {
        let why = format_args!(
            "{} could not be started: {}",
            action.program,
            SyscallError::from_errno(-pid)
        );
        return Err(fault(said::FAULT, why));
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
