//! Asking `/sbin/privd` to run a mechanism.
//!
//! A mechanism started without its action's capabilities calls [`elevate`],
//! which sends privd its own argv and stdio and exits with the status of the
//! copy privd runs. The connection it asks through is the one it inherited
//! ([`ENV_FD`]), which speaks as whoever made it, or else a fresh one, which
//! speaks as this process.
//!
//! The requester's terminal is never handed over. A mechanism runs outside
//! the requester's session, where that terminal answers `EIO`, so output for
//! one, or for a socket, is relayed through pipes, and input only for a
//! mechanism that reads it; one that runs a terminal session gets a
//! terminal of its own, whose master the requester relays.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use slopos_abi::fs::{O_DIRECTORY, O_RDONLY, O_RDWR, S_IFSOCK, UserFsStat};
use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::signal::SIGWINCH;
use slopos_abi::syscall::{
    BOOT_FLAG_MEDIUM, F_DUPFD_CLOEXEC, F_GETFD, F_SETFD, FD_CLOEXEC, MSG_NOSIGNAL, O_CLOEXEC,
    POLLIN, POLLOUT, TIOCSPTLCK, UserPollFd, UserTermios,
};
use slopos_priv_core::wire::{self, CONNECTION, Exit, Reply, Request};
use slopos_priv_core::{ENV_ACTION, ENV_FD, POLICY, PROGRAM, Policy, PolicyError, RunsAs, SOCKET};
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
    /// A terminal of the mechanism's own in place of each descriptor that is
    /// this process's terminal, when standard input is one: relayed byte for
    /// byte once the mechanism starts, with this terminal raw until it exits,
    /// and shown on standard error when neither output is a terminal. Lines
    /// forwarded otherwise.
    Terminal,
}

/// How long a request waits for privd while init starts it again.
pub const RESTART_WAIT: Duration = Duration::from_secs(2);

/// How long a program whose requests may span a base switch waits for the
/// new base's privd, which starts once its predecessor has drained.
pub const SWITCH_WAIT: Duration = Duration::from_secs(60);

/// What a requester says while the dialog asks its person.
pub const WAITING: &str = "waiting for you to allow this on the screen";

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
            let fd = net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0)
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

/// The base's policy, every action of it, held as privd holds its own.
pub fn whole_policy() -> Result<Policy, String> {
    policy_as(Policy::parse)
}

fn policy_as(parse: fn(&str) -> Result<Policy, PolicyError>) -> Result<Policy, String> {
    let text = std::fs::read_to_string(POLICY).map_err(|e| format!("{POLICY}: {e}"))?;
    parse(&text).map_err(|e| format!("{POLICY}: {e}"))
}

/// The base's policy as it stands on this boot, as a requester reads it
/// ([`Policy::read`]): an action marked `medium` exists only when the kernel
/// serves the install medium it booted from.
pub fn policy() -> Result<Policy, String> {
    Ok(policy_as(Policy::read)?.for_boot(on_the_medium()))
}

pub fn on_the_medium() -> bool {
    sys_core::boot_flag(BOOT_FLAG_MEDIUM)
}

/// Whether the base's policy names `program`, a canonical path, for an
/// action. A mechanism keeps the connection from what it starts.
pub fn is_mechanism(program: &str) -> bool {
    static MECHANISMS: LazyLock<Vec<String>> = LazyLock::new(|| {
        policy()
            .map(|policy| {
                let programs = policy.actions().iter().map(|action| action.program.clone());
                programs.collect()
            })
            .unwrap_or_default()
    });
    MECHANISMS.iter().any(|mechanism| mechanism == program)
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
    let policy = policy()?;
    let spec = policy
        .action(action)
        .ok_or_else(|| format!("{POLICY} has no action {action}"))?;
    let held = sys_core::caps_drop(0);
    let as_itself = match spec.runs_as {
        RunsAs::Uid(uid) => process::geteuid() == uid,
        RunsAs::Requester => true,
    };
    if held & spec.caps == spec.caps && as_itself {
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
        terminal: fs::isatty(0),
    };
    run(&mut connection, request, input).map(Asked::Ran)
}

/// Why privd ran nothing.
#[derive(Debug)]
pub struct NotRun {
    pub reason: String,
    /// The person at the machine answered privd's question so.
    pub by_person: bool,
}

impl From<String> for NotRun {
    fn from(reason: String) -> NotRun {
        NotRun {
            reason,
            by_person: false,
        }
    }
}

impl core::fmt::Display for NotRun {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Have privd run `action` with `args`, the mechanism reading `input` and
/// writing to `out`, its errors to `err`: its status, or why nothing ran.
pub fn run_feeding(
    action: &str,
    args: &[&str],
    input: &[u8],
    out: RawFd,
    err: RawFd,
) -> Result<i32, NotRun> {
    let mut connection = Connection::open_within(RESTART_WAIT)
        .map_err(|why| format!("cannot reach privd: {why}"))?;
    let (read, write) = pipe()?;
    if fs::write_slice(write.raw(), input) != Ok(input.len()) {
        return Err(String::from("what was typed does not fit a pipe").into());
    }
    drop(write);
    let request = Request {
        id: request_id(),
        action: action.to_owned(),
        args: args.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
        cwd: false,
        terminal: false,
    };
    connection.send(&request, &[read.raw(), out, err])?;
    drop(read);
    loop {
        match connection.reply(request.id)? {
            Some(Reply::Asking { .. } | Reply::Started { .. }) => {}
            Some(Reply::Exited(Exit::Code(code))) => return Ok(code),
            Some(Reply::Exited(Exit::Signal(signal))) => return Ok(128 + i32::from(signal)),
            Some(Reply::Denied { reason, by_person }) => {
                return Err(NotRun { reason, by_person });
            }
            Some(Reply::Prompt { .. }) => {
                return Err(String::from("privd asked for a terminal").into());
            }
            None => return Err(String::from("privd closed the connection").into()),
        }
    }
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
    /// The master of the mechanism's own terminal, under [`Input::Terminal`].
    terminal: Option<OwnedFd>,
    /// What that terminal shows, relayed to `shown_at` only once the
    /// mechanism has started and this terminal is raw, so a reply typed at a
    /// prompt is never echoed by it.
    terminal_out: Option<OwnedFd>,
    shown_at: RawFd,
    raw: Option<RawTerminal>,
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

fn dup(fd: &OwnedFd) -> Result<OwnedFd, String> {
    above_stdio(fs::dup(fd.raw()).map_err(|e| format!("dup: {e}"))?)
}

/// A pseudo-terminal's master and slave, with this terminal's size. The slave
/// is locked again once this process holds it, so no other process opens it
/// by its path before the mechanism makes it its own.
fn terminal_pair() -> Result<(OwnedFd, OwnedFd), String> {
    let fail = |why: String| format!("no terminal for the mechanism: {why}");
    let (master, _) =
        process::openpty().map_err(|e| fail(SyscallError::from_errno((-e) as i32).to_string()))?;
    let slave = fs::ioctl_tiocgptpeer(master.raw()).map_err(|e| fail(e.to_string()))?;
    let mut locked: i32 = 1;
    Sys::ioctl(master.raw(), TIOCSPTLCK, (&mut locked as *mut i32) as u64)
        .map_err(|e| fail(SyscallError::from(e).to_string()))?;
    let (master, slave) = (above_stdio(master)?, above_stdio(slave)?);
    for end in [&master, &slave] {
        fs::set_fd_cloexec(end.raw()).map_err(|e| format!("fcntl: {e}"))?;
    }
    if let Ok(size) = fs::tiocgwinsz(0) {
        let _ = fs::tiocswinsz(master.raw(), &size);
    }
    Ok((master, slave))
}

/// This process's terminal, raw while the mechanism's own terminal stands in
/// for it, so every byte typed reaches that one unchanged; as it was again
/// once dropped.
struct RawTerminal(UserTermios);

impl RawTerminal {
    fn enter() -> Option<RawTerminal> {
        let saved = fs::tcgetattr(0).ok()?;
        let mut raw = saved;
        // SAFETY: `raw` is a termios this function owns, which the call only
        // rewrites in place.
        unsafe { slopos_slibc::tty::cfmakeraw(&mut raw) };
        fs::tcsetattr(0, &raw).ok()?;
        Some(RawTerminal(saved))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = fs::tcsetattr(0, &self.0);
    }
}

impl Stdio {
    fn build(input: Input) -> Result<Stdio, String> {
        let mut stdio = Stdio {
            passed: Vec::new(),
            to_stdin: None,
            from_out: [None, None],
            terminal: None,
            terminal_out: None,
            shown_at: 2,
            raw: None,
        };
        let pair = match input == Input::Terminal && fs::isatty(0) {
            true => Some(terminal_pair()?),
            false => None,
        };
        for fd in 0..3 {
            let open = Sys::fcntl(fd, F_GETFD as i32, 0).is_ok();
            let passed = if !open {
                null()?
            } else if !relayed(fd) {
                above_stdio(fs::dup(fd).map_err(|e| format!("fd {fd}: {e}"))?)?
            } else if let Some((_, slave)) = pair.as_ref().filter(|_| fs::isatty(fd)) {
                dup(slave)?
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
        if let Some((master, _)) = pair {
            let shown_at = [1, 2].into_iter().find(|&fd| fs::isatty(fd)).unwrap_or(2);
            net::set_nonblocking(master.raw()).map_err(|e| format!("the terminal: {e}"))?;
            stdio.to_stdin = Some(Forward {
                pipe: dup(&master)?,
                held: Vec::new(),
                ended: false,
            });
            stdio.terminal_out = Some(dup(&master)?);
            stdio.shown_at = shown_at;
            stdio.terminal = Some(master);
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
    if stdio.terminal.is_some() {
        process::set_signal_handler(SIGWINCH, note_signal);
    }
    connection.send(&request, &fds)?;
    drop(cwd);
    stdio.passed.clear();
    let program = program_name();
    // Typed at a prompt privd sends, never forwarded before the mechanism
    // starts.
    let typed_here = fs::isatty(0);
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
        if pending & (1 << SIGWINCH) != 0
            && let (Some(master), Ok(size)) = (&stdio.terminal, fs::tiocgwinsz(0))
        {
            let _ = fs::tiocswinsz(master.raw(), &size);
        }
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
                Some(Reply::Started { .. }) => {
                    started = true;
                    if stdio.terminal.is_some() {
                        stdio.raw = RawTerminal::enter();
                    }
                }
                Some(Reply::Asking { .. }) => {
                    eprintln!("{program}: {WAITING}");
                }
                Some(Reply::Prompt { about, prompt }) => {
                    if let Some(about) = about {
                        eprintln!("{about}");
                    }
                    let typed = crate::secret::read(&prompt)?;
                    let secret = typed.as_ref().map_or(&[][..], |typed| typed.bytes());
                    let mut frame = wire::secret_frame(request.id, secret);
                    let sent = connection.send_frame(&frame, &[]);
                    slopos_priv_core::consent::wipe(&mut frame);
                    sent?;
                }
                Some(Reply::Exited(exit)) => {
                    exited = Some(exit);
                    drain_until = Some(Instant::now() + DRAIN);
                    stdio.to_stdin = None;
                    stdio.raw = None;
                }
                Some(Reply::Denied { reason, by_person }) => {
                    if let Some(signal) = withdrawn_by {
                        return Ok(128 + i32::from(signal));
                    }
                    eprintln!("{program}: {reason}");
                    return Ok(if by_person {
                        STATUS_REFUSED_BY_PERSON
                    } else {
                        STATUS_REFUSED
                    });
                }
                None => break,
            }
        }
        let outputs_open =
            stdio.from_out.iter().any(Option::is_some) || stdio.terminal_out.is_some();
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
        if let Some(out) = stdio.terminal_out.as_ref().filter(|_| started) {
            polled.push(poll(out.raw(), POLLIN));
        }
        if let Some(forward) = &stdio.to_stdin {
            if forward.wants_input() && (started || (stdio.terminal.is_none() && !typed_here)) {
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
            } else if stdio
                .terminal_out
                .as_ref()
                .is_some_and(|out| out.raw() == entry.fd)
            {
                relay(&mut stdio.terminal_out, stdio.shown_at);
            } else if let Some(index) = stdio
                .from_out
                .iter()
                .position(|out| out.as_ref().is_some_and(|out| out.raw() == entry.fd))
            {
                relay(&mut stdio.from_out[index], index as RawFd + 1);
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

fn relay(from: &mut Option<OwnedFd>, to: RawFd) {
    let Some(fd) = from.as_ref().map(OwnedFd::raw) else {
        return;
    };
    let mut buf = [0u8; 4096];
    match fs::read_slice(fd, &mut buf) {
        Ok(n) if n > 0 => {
            let _ = write_all(to, &buf[..n]);
        }
        Err(SyscallError::EAGAIN | SyscallError::EINTR) => {}
        _ => *from = None,
    }
}

fn write_all(fd: RawFd, mut bytes: &[u8]) -> Result<(), ()> {
    while !bytes.is_empty() {
        match fs::write_slice(fd, bytes) {
            Err(SyscallError::EINTR) => {}
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => bytes = &bytes[n..],
        }
    }
    Ok(())
}
