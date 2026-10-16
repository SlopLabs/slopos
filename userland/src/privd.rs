//! Asking `/sbin/privd` to run a mechanism.
//!
//! A mechanism started without its action's capabilities calls [`elevate`],
//! which sends privd its own argv and stdio and exits with the status of the
//! copy privd runs. The connection it asks through is the one it inherited
//! ([`ENV_FD`]), which speaks as whoever made it, or else a fresh one, which
//! speaks as this process.
//!
//! A terminal is never handed over. A mechanism runs outside the requester's
//! session, where the terminal answers `EIO`, so output for one, or for a
//! socket, is relayed through pipes, and input only for a mechanism that
//! reads it.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slopos_abi::fs::{O_DIRECTORY, O_RDONLY, O_RDWR, S_IFSOCK, UserFsStat};
use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::syscall::{
    F_DUPFD_CLOEXEC, F_GETFD, F_SETFD, FD_CLOEXEC, MSG_NOSIGNAL, O_CLOEXEC, POLLIN, POLLOUT,
    UserPollFd,
};
use slopos_priv_core::wire::{self, CONNECTION, Exit, Reply, Request};
use slopos_priv_core::{ENV_ACTION, ENV_FD, POLICY, PROGRAM, Policy, SOCKET};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};

use crate::syscall::{OwnedFd, RawFd, SyscallError, core as sys_core, fs, net, process};

/// What a mechanism that reads its standard input gets from a terminal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Nothing: the terminal stays the requester's.
    Closed,
    /// Lines typed at the terminal while the mechanism runs.
    Forwarded,
}

/// How long a request waits for privd while init starts it again.
pub const RESTART_WAIT: Duration = Duration::from_secs(2);

/// How long a program whose requests may span a base switch waits for the
/// new base's privd, which starts once its predecessor has drained.
pub const SWITCH_WAIT: Duration = Duration::from_secs(60);

/// Exit statuses for a request that ran nothing, as pkexec gives them.
pub const STATUS_REFUSED_BY_PERSON: i32 = 126;
pub const STATUS_REFUSED: i32 = 127;

/// A connection to privd; one request at a time.
pub struct Connection {
    fd: OwnedFd,
    frames: Deframer,
}

impl Connection {
    /// A fresh connection, attested as this process.
    pub fn open() -> Result<Connection, String> {
        Self::open_within(Duration::ZERO)
    }

    /// [`open`](Self::open), waiting `wait` for a privd that is not listening
    /// yet, as one init has just started again is not.
    pub fn open_within(wait: Duration) -> Result<Connection, String> {
        let started = Instant::now();
        let fd = loop {
            let fd = net::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC as u16, 0)
                .map_err(|e| format!("socket: {e}"))?;
            let fd = above_stdio(fd)?;
            match net::connect_unix(fd.raw(), SOCKET.as_bytes()) {
                Ok(()) => break fd,
                Err(SyscallError::ECONNREFUSED | SyscallError::ENOENT)
                    if started.elapsed() < wait =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("{SOCKET}: {e}")),
            }
        };
        is_privd(fd.raw())
            .then(|| Connection::new(fd))
            .ok_or_else(|| format!("{SOCKET} is not privd"))
    }

    /// The connection [`ENV_FD`] named when this process started, if it is
    /// one to privd.
    pub fn inherited() -> Option<Connection> {
        let dup = Sys::fcntl(inherited_fd()?, F_DUPFD_CLOEXEC as i32, 3).ok()?;
        // SAFETY: fcntl just opened this descriptor for us.
        Some(Connection::new(unsafe { OwnedFd::from_raw(dup as RawFd) }))
    }

    fn new(fd: OwnedFd) -> Connection {
        Connection {
            fd,
            frames: wire::deframer(),
        }
    }

    pub fn raw(&self) -> RawFd {
        self.fd.raw()
    }

    /// Let `cmd`'s child, and what it starts, ask through this connection.
    /// Leaves the descriptor inheritable by every later spawn of this process.
    pub fn hand_to(&self, cmd: &mut Command) {
        self.set_inheritable(true);
        cmd.env(ENV_FD, self.raw().to_string());
    }

    pub fn set_inheritable(&self, on: bool) {
        if let Ok(flags) = Sys::fcntl(self.raw(), F_GETFD as i32, 0) {
            let flags = flags as u64;
            let flags = if on {
                flags & !FD_CLOEXEC
            } else {
                flags | FD_CLOEXEC
            };
            let _ = Sys::fcntl(self.raw(), F_SETFD as i32, flags);
        }
    }

    /// Send `request` with its descriptors: stdin, stdout, stderr and, when
    /// it names one, the working directory.
    pub fn send(&self, request: &Request, fds: &[RawFd]) -> Result<(), String> {
        self.send_frame(&request.frame(), fds)
    }

    pub fn send_frame(&self, frame: &[u8], fds: &[RawFd]) -> Result<(), String> {
        let mut sent = net::send_with_fds(self.raw(), frame, fds)
            .map_err(|e| format!("sending to privd: {e}"))?;
        while sent < frame.len() {
            sent += net::send(self.raw(), &frame[sent..], MSG_NOSIGNAL)
                .map_err(|e| format!("sending to privd: {e}"))?;
        }
        Ok(())
    }

    /// The next reply to request `id`, blocking; `None` once privd has closed
    /// the connection.
    pub fn reply(&mut self, id: u64) -> Result<Option<Reply>, String> {
        loop {
            if let Some(reply) = self.buffered(id)? {
                return Ok(Some(reply));
            }
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    /// A reply to request `id` that has already arrived. What an earlier
    /// request on the connection left is passed over.
    fn buffered(&mut self, id: u64) -> Result<Option<Reply>, String> {
        loop {
            let (kind, payload) = match self.frames.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(None),
                Err(e) => return Err(format!("privd: {e}")),
            };
            let (of, reply) = Reply::decode(kind, &payload)
                .map_err(|_| format!("privd sent frame {kind:#04x} this client does not read"))?;
            if of == id || of == CONNECTION {
                return Ok(Some(reply));
            }
        }
    }

    /// Read what has arrived; `false` at end of stream.
    fn fill(&mut self) -> Result<bool, String> {
        let mut buf = [0u8; 4096];
        match fs::read_slice(self.raw(), &mut buf) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.frames.push(&buf[..n]);
                Ok(true)
            }
            Err(e) => Err(format!("reading from privd: {e}")),
        }
    }
}

/// Only the real privd: uid 0, running the base's `/sbin/privd`. Whatever a
/// stale or forged [`ENV_FD`] names is not asked anything.
fn is_privd(fd: RawFd) -> bool {
    net::peer_cred(fd).is_ok_and(|cred| cred.uid == 0)
        && net::peer_program(fd).ok().flatten().as_deref() == Some(PROGRAM.as_bytes())
}

/// `fd`, moved clear of stdin, stdout and stderr, which a process started
/// with any of them closed would otherwise find it at.
fn above_stdio(fd: OwnedFd) -> Result<OwnedFd, String> {
    if fd.raw() > 2 {
        return Ok(fd);
    }
    let moved =
        Sys::fcntl(fd.raw(), F_DUPFD_CLOEXEC as i32, 3).map_err(|e| format!("fcntl: {e}"))?;
    // SAFETY: fcntl just opened this descriptor for us.
    Ok(unsafe { OwnedFd::from_raw(moved as RawFd) })
}

/// The connection this process inherited, close-on-exec from the first call
/// on: a spawn hands it on by naming it, and the child finds it at the same
/// number, which its environment names. A `dup2` onto that number clears the
/// flag, so whatever restores the descriptor so must restore the flag too.
pub fn inherited_fd() -> Option<RawFd> {
    static INHERITED: OnceLock<Option<RawFd>> = OnceLock::new();
    *INHERITED.get_or_init(|| {
        let raw: RawFd = std::env::var(ENV_FD).ok()?.parse().ok()?;
        (raw > 2 && is_privd(raw) && fs::set_fd_cloexec(raw).is_ok()).then_some(raw)
    })
}

/// Keep the connection this process inherited from whatever it execs.
pub fn hold_inherited() {
    inherited_fd();
}

/// Whether `program`, a canonical path, may be handed a privd connection:
/// the shell, which hands it on only to mechanisms, and the mechanisms
/// themselves. Anything else could pass the connection, and whoever made it,
/// to code from outside the base.
pub fn takes_connection(program: &str) -> bool {
    program == SHELL || is_mechanism(program)
}

/// Whether the base's policy names `program`, a canonical path, for an
/// action. A mechanism keeps the connection from what it starts.
pub fn is_mechanism(program: &str) -> bool {
    static MECHANISMS: OnceLock<Vec<String>> = OnceLock::new();
    MECHANISMS
        .get_or_init(|| {
            std::fs::read_to_string(POLICY)
                .ok()
                .and_then(|text| Policy::parse(&text).ok())
                .map(|policy| {
                    policy
                        .actions()
                        .iter()
                        .map(|action| action.program.clone())
                        .collect()
                })
                .unwrap_or_default()
        })
        .iter()
        .any(|mechanism| mechanism == program)
}

const SHELL: &str = "/bin/shell";

/// This process's argv past its first word, as privd is sent it.
pub fn own_args() -> Vec<Vec<u8>> {
    std::env::args_os()
        .skip(1)
        .map(OsString::into_vec)
        .collect()
}

/// Run as `action`'s mechanism: return when this process may act for it,
/// or have privd run it and exit with what that copy exited with.
///
/// `action` is the one the parsed argv belongs to, `None` for argv that needs
/// no authority. Started by privd ([`ENV_ACTION`]), the process acts only for
/// the action privd matched; otherwise it acts itself when it already holds
/// the action's capabilities and runs as its uid.
pub fn elevate(action: Option<&str>, args: &[Vec<u8>], input: Input) {
    hold_inherited();
    let program = program_name();
    if let Some(started_for) = std::env::var_os(ENV_ACTION) {
        if action.map(OsStr::new) == Some(started_for.as_os_str()) {
            return;
        }
        eprintln!(
            "{program}: privd started this for {}, which these arguments are not",
            started_for.to_string_lossy()
        );
        sys_core::exit_with_code(STATUS_REFUSED);
    }
    let Some(action) = action else {
        return;
    };
    let status = match ask(action, args, input) {
        Ok(Asked::Itself) => return,
        Ok(Asked::Ran(status)) => status,
        Err(why) => {
            eprintln!("{program}: {why}");
            STATUS_REFUSED
        }
    };
    sys_core::exit_with_code(status);
}

fn program_name() -> String {
    std::env::args_os()
        .next()
        .map(|arg0| {
            let arg0 = arg0.to_string_lossy().into_owned();
            arg0.rsplit('/').next().unwrap_or(&arg0).to_owned()
        })
        .unwrap_or_default()
}

enum Asked {
    Itself,
    Ran(i32),
}

fn ask(action: &str, args: &[Vec<u8>], input: Input) -> Result<Asked, String> {
    let text = std::fs::read_to_string(POLICY).map_err(|e| format!("{POLICY}: {e}"))?;
    let policy = Policy::parse(&text).map_err(|e| format!("{POLICY}: {e}"))?;
    let spec = policy
        .action(action)
        .ok_or_else(|| format!("{POLICY} has no action {action}"))?;
    let held = sys_core::caps_drop(0);
    if held & spec.caps == spec.caps && process::geteuid() == spec.uid {
        return Ok(Asked::Itself);
    }
    let mut connection = match Connection::inherited() {
        Some(inherited) => inherited,
        None => Connection::open_within(RESTART_WAIT)
            .map_err(|why| format!("cannot reach privd: {why}"))?,
    };
    let request = Request {
        id: request_id(),
        action: action.to_owned(),
        args: args.to_vec(),
        cwd: spec.cwd,
    };
    run(&mut connection, request, input).map(Asked::Ran)
}

/// A fresh id, so no reply left on an inherited connection passes as this
/// request's.
pub fn request_id() -> u64 {
    let mut bytes = [0u8; 8];
    if sys_core::getrandom(&mut bytes) != bytes.len() as isize {
        bytes[..4].copy_from_slice(&std::process::id().to_le_bytes());
        bytes[4..].copy_from_slice(&sys_core::random_next().to_le_bytes());
    }
    u64::from_le_bytes(bytes).max(1)
}

/// The descriptors a request carries and the pumps that stand in for a
/// terminal among them.
struct Stdio {
    passed: Vec<OwnedFd>,
    to_stdin: Option<Forward>,
    from_out: [Option<OwnedFd>; 2],
}

/// Input typed at the terminal on its way to the mechanism, held here while
/// the mechanism is not reading, so its output keeps being relayed meanwhile.
struct Forward {
    pipe: OwnedFd,
    held: Vec<u8>,
    ended: bool,
}

const HELD_MAX: usize = 64 * 1024;

impl Forward {
    fn wants_input(&self) -> bool {
        !self.ended && self.held.len() < HELD_MAX
    }

    fn read(&mut self) {
        let mut buf = [0u8; 4096];
        match fs::read_slice(0, &mut buf) {
            Ok(n) if n > 0 => self.held.extend_from_slice(&buf[..n]),
            Err(SyscallError::EINTR) => {}
            _ => self.ended = true,
        }
    }

    /// `false` once the mechanism reads no more.
    fn write(&mut self) -> bool {
        while !self.held.is_empty() {
            match fs::write_slice(self.pipe.raw(), &self.held) {
                Ok(n) if n > 0 => {
                    self.held.drain(..n);
                }
                Err(SyscallError::EAGAIN) => return true,
                Err(SyscallError::EINTR) => {}
                _ => return false,
            }
        }
        true
    }
}

fn null() -> Result<OwnedFd, String> {
    above_stdio(
        fs::open_cstr(c"/dev/null", O_RDWR | O_CLOEXEC as u32)
            .map_err(|e| format!("/dev/null: {e}"))?,
    )
}

fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let (read, write) = fs::pipe2(O_CLOEXEC as u32).map_err(|e| format!("pipe: {e}"))?;
    Ok((above_stdio(read)?, above_stdio(write)?))
}

impl Stdio {
    fn build(input: Input) -> Result<Stdio, String> {
        let mut stdio = Stdio {
            passed: Vec::new(),
            to_stdin: None,
            from_out: [None, None],
        };
        for fd in 0..3 {
            let open = Sys::fcntl(fd, F_GETFD as i32, 0).is_ok();
            let passed = if !open {
                null()?
            } else if !relayed(fd) {
                above_stdio(fs::dup(fd).map_err(|e| format!("fd {fd}: {e}"))?)?
            } else if fd == 0 {
                if input == Input::Closed {
                    null()?
                } else {
                    let (read, write) = pipe()?;
                    net::set_nonblocking(write.raw()).map_err(|e| format!("pipe: {e}"))?;
                    stdio.to_stdin = Some(Forward {
                        pipe: write,
                        held: Vec::new(),
                        ended: false,
                    });
                    read
                }
            } else {
                let (read, write) = pipe()?;
                stdio.from_out[fd as usize - 1] = Some(read);
                write
            };
            stdio.passed.push(passed);
        }
        Ok(stdio)
    }
}

/// A terminal answers `EIO` to a mechanism in privd's session, and a socket
/// cannot travel as `SCM_RIGHTS`: either reaches it through a pipe.
fn relayed(fd: RawFd) -> bool {
    if fs::isatty(fd) {
        return true;
    }
    let mut stat = UserFsStat::default();
    Sys::fstat(fd, (&mut stat as *mut UserFsStat).cast()).is_ok() && stat.file_kind() == S_IFSOCK
}

static SIGNALS: AtomicU64 = AtomicU64::new(0);

extern "C" fn note_signal(signal: i32) {
    if (0..64).contains(&signal) {
        SIGNALS.fetch_or(1 << signal, Ordering::Relaxed);
    }
}

/// How long output still trickles in from what the mechanism started once
/// the mechanism itself has exited.
const DRAIN: Duration = Duration::from_millis(500);
const TICK_MS: i64 = 250;

/// Send `request` with this process's stdio, relay until the mechanism
/// exits, and give its status. A signal before it starts withdraws the
/// request, and a second one stops waiting for it.
pub fn run(connection: &mut Connection, request: Request, input: Input) -> Result<i32, String> {
    let mut stdio = Stdio::build(input)?;
    let cwd = match request.cwd {
        true => Some(above_stdio(
            fs::open_cstr(c".", O_RDONLY | O_DIRECTORY | O_CLOEXEC as u32)
                .map_err(|e| format!("the working directory: {e}"))?,
        )?),
        false => None,
    };
    let mut fds: Vec<RawFd> = stdio.passed.iter().map(OwnedFd::raw).collect();
    fds.extend(cwd.as_ref().map(OwnedFd::raw));
    for signal in wire::RELAYED_SIGNALS {
        process::set_signal_handler(signal, note_signal);
    }
    connection.send(&request, &fds)?;
    drop(cwd);
    stdio.passed.clear();
    let program = program_name();
    let mut started = false;
    let mut signalled = false;
    let mut withdrawn_by: Option<u8> = None;
    let mut exited: Option<Exit> = None;
    let mut drain_until: Option<Instant> = None;
    loop {
        let pending = SIGNALS.swap(0, Ordering::Relaxed);
        let caught: Vec<u8> = wire::RELAYED_SIGNALS
            .into_iter()
            .filter(|&signal| pending & (1 << signal) != 0)
            .collect();
        if let Some(&first) = caught.first()
            && exited.is_none()
        {
            if signalled {
                return Ok(128 + i32::from(first));
            }
            signalled = true;
            for &signal in &caught {
                if !started {
                    withdrawn_by.get_or_insert(signal);
                }
                let _ = connection.send_frame(&wire::signal_frame(request.id, signal), &[]);
            }
        }
        while exited.is_none() {
            match connection.buffered(request.id)? {
                Some(Reply::Started { .. }) => started = true,
                Some(Reply::Asking) => {
                    eprintln!("{program}: asking the person at this machine to allow it");
                }
                Some(Reply::Exited(exit)) => {
                    exited = Some(exit);
                    drain_until = Some(Instant::now() + DRAIN);
                    stdio.to_stdin = None;
                }
                Some(Reply::Denied { reason, by_person }) => {
                    if let Some(signal) = withdrawn_by {
                        return Ok(128 + i32::from(signal));
                    }
                    eprintln!("{program}: privd refused {}: {reason}", request.action);
                    return Ok(if by_person {
                        STATUS_REFUSED_BY_PERSON
                    } else {
                        STATUS_REFUSED
                    });
                }
                None => break,
            }
        }
        let outputs_open = stdio.from_out.iter().any(Option::is_some);
        if let Some(exit) = exited
            && (!outputs_open || drain_until.is_some_and(|until| Instant::now() >= until))
        {
            return Ok(match exit {
                Exit::Code(code) => code,
                Exit::Signal(signal) => 128 + i32::from(signal),
            });
        }

        let mut polled = Vec::with_capacity(5);
        if exited.is_none() {
            polled.push(poll(connection.raw(), POLLIN));
        }
        for out in stdio.from_out.iter().flatten() {
            polled.push(poll(out.raw(), POLLIN));
        }
        if let Some(forward) = &stdio.to_stdin {
            if forward.wants_input() {
                polled.push(poll(0, POLLIN));
            }
            if !forward.held.is_empty() {
                polled.push(poll(forward.pipe.raw(), POLLOUT));
            }
        }
        match fs::poll(&mut polled, TICK_MS) {
            Ok(_) => {}
            Err(SyscallError::EINTR) => continue,
            Err(e) => return Err(format!("poll: {e}")),
        }
        for entry in polled.iter().filter(|p| p.revents != 0) {
            if entry.fd == connection.raw() {
                if !connection.fill()? {
                    return Err("privd closed the connection before the mechanism exited".into());
                }
            } else if let Some(forward) = stdio
                .to_stdin
                .as_mut()
                .filter(|f| entry.fd == 0 || entry.fd == f.pipe.raw())
            {
                if entry.fd == 0 {
                    forward.read();
                }
                if !forward.write() || (forward.ended && forward.held.is_empty()) {
                    stdio.to_stdin = None;
                }
            } else {
                relay_output(&mut stdio.from_out, entry.fd);
            }
        }
    }
}

fn poll(fd: RawFd, events: u16) -> UserPollFd {
    UserPollFd {
        fd,
        events,
        revents: 0,
    }
}

fn relay_output(from: &mut [Option<OwnedFd>; 2], fd: RawFd) {
    let Some(index) = from
        .iter()
        .position(|o| o.as_ref().is_some_and(|f| f.raw() == fd))
    else {
        return;
    };
    let mut buf = [0u8; 4096];
    match fs::read_slice(fd, &mut buf) {
        Ok(n) if n > 0 => {
            let _ = write_all(index as RawFd + 1, &buf[..n]);
        }
        _ => from[index] = None,
    }
}

fn write_all(fd: RawFd, mut bytes: &[u8]) -> Result<(), ()> {
    while !bytes.is_empty() {
        match fs::write_slice(fd, bytes) {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => bytes = &bytes[n..],
        }
    }
    Ok(())
}
