//! `/run/init`: one request line per connection, judged by who connected:
//! uid 0 running a base program, or a person asking about the session their
//! process carries the login tag of.

use slopos_abi::fs::PERSON_UIDS;
use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::syscall::{MSG_NOSIGNAL, POLLIN, UserPollFd};

use crate::init_control::{At, Command, MAX_REQUEST, SOCKET};
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, net};

/// Connections of one uid waiting to finish their line, so no person crowds
/// the system or another person out.
const CLIENTS_EACH: usize = 4;
/// Connections taken per pass, so a flood of them cannot hold init here.
const ACCEPTS: usize = 16;
/// How long a connection has to send its line.
const LINE_MS: u64 = 2000;

pub struct Control {
    listener: OwnedFd,
    clients: Vec<Client>,
}

struct Client {
    fd: OwnedFd,
    uid: u32,
    pid: u32,
    line: Vec<u8>,
    handed: Option<OwnedFd>,
    deadline: u64,
}

/// A request init admitted: what, for whom, from which session, and where
/// to answer it; a console login hands its terminal beside it.
pub struct Request {
    pub command: Command,
    pub uid: u32,
    pub pid: u32,
    /// The login tag the requester carries, 0 for none.
    pub login: u32,
    pub handed: Option<OwnedFd>,
    pub reply: OwnedFd,
}

impl Control {
    pub fn bind() -> Result<Control, String> {
        let fd = net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0)
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(SOCKET);
        net::bind_unix(fd.raw(), SOCKET.as_bytes()).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))
            .map_err(|e| e.to_string())?;
        net::listen(fd.raw(), (2 * CLIENTS_EACH) as u32).map_err(|e| e.to_string())?;
        Ok(Control::adopt(fd))
    }

    /// The listener an earlier image of init bound.
    pub fn adopt(listener: OwnedFd) -> Control {
        let _ = net::set_nonblocking(listener.raw());
        Control {
            listener,
            clients: Vec::new(),
        }
    }

    pub fn listener(&self) -> RawFd {
        self.listener.raw()
    }

    pub fn poll_fds(&self, out: &mut Vec<UserPollFd>) {
        for fd in std::iter::once(&self.listener).chain(self.clients.iter().map(|c| &c.fd)) {
            out.push(UserPollFd {
                fd: fd.raw(),
                events: POLLIN,
                revents: 0,
            });
        }
    }

    /// When the earliest unfinished line runs out of time.
    pub fn deadline(&self) -> Option<u64> {
        self.clients.iter().map(|client| client.deadline).min()
    }

    /// Take new connections and what they sent: each request admitted. A
    /// refused one is answered here.
    pub fn serve(&mut self, now: u64, leads: impl Fn(u32) -> bool) -> Vec<Request> {
        for _ in 0..ACCEPTS {
            let fd = match net::accept(self.listener.raw(), None) {
                Ok(fd) => fd,
                Err(SyscallError::EINTR) => continue,
                Err(_) => break,
            };
            let _ = fs::set_fd_cloexec(fd.raw());
            let Ok(cred) = net::peer_cred(fd.raw()) else {
                continue;
            };
            let (uid, pid) = (cred.uid, cred.pid as u32);
            if uid != 0 && !PERSON_UIDS.contains(&uid) {
                answer(fd, &format!("refused: uid {uid} may ask init for nothing"));
                continue;
            }
            // A session's leader says what only it may, that its desktop
            // locked, beside whatever the rest of its person's processes hold.
            let held = match leads(pid) {
                true => self.clients.iter().filter(|c| c.pid == pid).count(),
                false => self
                    .clients
                    .iter()
                    .filter(|c| c.uid == uid && !leads(c.pid))
                    .count(),
            };
            if held >= CLIENTS_EACH || net::set_nonblocking(fd.raw()).is_err() {
                answer(fd, "refused: too many requests are waiting");
                continue;
            }
            self.clients.push(Client {
                fd,
                uid,
                pid,
                line: Vec::new(),
                handed: None,
                deadline: now + LINE_MS,
            });
        }
        let mut admitted = Vec::new();
        let mut kept = Vec::new();
        let mut tasks = None;
        for mut client in std::mem::take(&mut self.clients) {
            match client.read() {
                Some(Ok(line)) => {
                    let base = runs_base(&client.fd);
                    let login = tasks
                        .get_or_insert_with(crate::init_control::Tasks::read)
                        .login_of(client.pid)
                        .unwrap_or(0);
                    match judge(client.uid, base, &line, client.handed.is_some()) {
                        Ok(command) => admitted.push(Request {
                            command,
                            uid: client.uid,
                            pid: client.pid,
                            login,
                            handed: client.handed,
                            reply: client.fd,
                        }),
                        Err(why) => answer(client.fd, &format!("refused: {why}")),
                    }
                }
                Some(Err(why)) => answer(client.fd, &format!("refused: {why}")),
                None if now >= client.deadline => answer(client.fd, "refused: no request"),
                None => kept.push(client),
            }
        }
        self.clients = kept;
        admitted
    }
}

impl Client {
    /// The request line once it is whole, `None` until it is.
    fn read(&mut self) -> Option<Result<Vec<u8>, &'static str>> {
        let mut buf = [0u8; MAX_REQUEST];
        loop {
            match net::recv_with_fds(self.fd.raw(), &mut buf) {
                Ok((0, _)) => return Some(Err("the request ended before its line")),
                Ok((n, fds)) => {
                    let mut fds = fds.into_iter();
                    if let Some(fd) = fds.next() {
                        if self.handed.is_some() || fds.next().is_some() {
                            return Some(Err("a request hands init one descriptor at most"));
                        }
                        self.handed = Some(fd);
                    }
                    self.line.extend_from_slice(&buf[..n]);
                    if let Some(end) = self.line.iter().position(|&b| b == b'\n') {
                        self.line.truncate(end);
                        return Some(Ok(std::mem::take(&mut self.line)));
                    }
                    if self.line.len() > MAX_REQUEST {
                        return Some(Err("the request is too long"));
                    }
                }
                Err(SyscallError::EAGAIN) => return None,
                Err(SyscallError::EINTR) => {}
                Err(_) => return Some(Err("the request could not be read")),
            }
        }
    }
}

/// Whether the peer at `fd` runs a base program, as of its request.
fn runs_base(fd: &OwnedFd) -> bool {
    net::peer_base_program(fd.raw()).is_ok_and(|path| path.is_some())
}

/// A person may restart, end and lock their own session, bring the login
/// screen in front, and their compositor say it locked; anything else is uid
/// 0 running a base program's, as privd's `root` principal is: `SO_PEERCRED`
/// names no capability, and uid 0 alone is no authority. Only a console
/// login hands init a descriptor.
fn judge(uid: u32, base: bool, line: &[u8], handed: bool) -> Result<Command, String> {
    let command = Command::parse(line).ok_or("not a request init takes")?;
    let console = matches!(command, Command::Login(At::Console, _));
    if handed != console {
        return Err(match console {
            true => "a console login hands init its terminal".into(),
            false => "only a console login hands init a descriptor".into(),
        });
    }
    let own = matches!(
        command,
        Command::RestartSession
            | Command::EndSession
            | Command::Greeter
            | Command::Lock
            | Command::Locked(_)
    );
    let allowed = (uid == 0 && base) || (PERSON_UIDS.contains(&uid) && own);
    if !allowed {
        let program = if base { "" } else { " outside the base" };
        return Err(format!(
            "uid {uid}{program} may not ask for {}",
            command.word()
        ));
    }
    Ok(command)
}

/// Best effort: a requester that does not take the line loses it.
pub fn answer(fd: OwnedFd, reply: &str) {
    let _ = net::send(fd.raw(), format!("{reply}\n").as_bytes(), MSG_NOSIGNAL);
}
