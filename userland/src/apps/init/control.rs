//! `/run/init`: one request line per connection, judged by who connected.

use slopos_abi::fs::SESSION_UID;
use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::syscall::{MSG_NOSIGNAL, POLLIN, UserPollFd};

use crate::init_control::{Command, MAX_REQUEST, SOCKET};
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, net};

/// Connections of one uid waiting to finish their line, uid 0's apart from
/// the session's, so the session cannot crowd the system out.
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
    line: Vec<u8>,
    deadline: u64,
}

/// A request init admitted: what, for whom, and where to answer it.
pub struct Request {
    pub command: Command,
    pub uid: u32,
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
    pub fn serve(&mut self, now: u64) -> Vec<Request> {
        for _ in 0..ACCEPTS {
            let fd = match net::accept(self.listener.raw(), None) {
                Ok(fd) => fd,
                Err(SyscallError::EINTR) => continue,
                Err(_) => break,
            };
            let _ = fs::set_fd_cloexec(fd.raw());
            let Ok(uid) = net::peer_cred(fd.raw()).map(|cred| cred.uid) else {
                continue;
            };
            if uid != 0 && uid != SESSION_UID {
                answer(fd, &format!("refused: uid {uid} may ask init for nothing"));
                continue;
            }
            let held = self.clients.iter().filter(|c| c.uid == uid).count();
            if held >= CLIENTS_EACH || net::set_nonblocking(fd.raw()).is_err() {
                answer(fd, "refused: too many requests are waiting");
                continue;
            }
            self.clients.push(Client {
                fd,
                uid,
                line: Vec::new(),
                deadline: now + LINE_MS,
            });
        }
        let mut admitted = Vec::new();
        let mut kept = Vec::new();
        for mut client in std::mem::take(&mut self.clients) {
            match client.read() {
                Some(Ok(line)) => match judge(client.uid, base_program(&client.fd), &line) {
                    Ok(command) => admitted.push(Request {
                        command,
                        uid: client.uid,
                        reply: client.fd,
                    }),
                    Err(why) => answer(client.fd, &format!("refused: {why}")),
                },
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
            match fs::read_slice(self.fd.raw(), &mut buf) {
                Ok(0) => return Some(Err("the request ended before its line")),
                Ok(n) => {
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
fn base_program(fd: &OwnedFd) -> bool {
    net::peer_base_program(fd.raw()).is_ok_and(|program| program.is_some())
}

/// The session may restart itself; anything else is uid 0 running a base
/// program's, as privd's `root` principal is: `SO_PEERCRED` names no
/// capability, and uid 0 alone is no authority.
fn judge(uid: u32, base: bool, line: &[u8]) -> Result<Command, String> {
    let command = Command::parse(line).ok_or("not a request init takes")?;
    let allowed = (uid == 0 && base) || (uid == SESSION_UID && command == Command::RestartSession);
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
