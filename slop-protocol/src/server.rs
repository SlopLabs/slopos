//! The compositor's end: a listening socket and its clients, whose events wait
//! for [`Server::flush_clients`] past `EAGAIN`; only an overflow disconnects.

use crate::codec::Encode;
use crate::connection::Connection;
use crate::types::{Event, PROTOCOL_VERSION, ProtocolError, Request};
use slopos_abi::errno::Errno;
use slopos_abi::net::{AF_UNIX, SOCK_STREAM};
use slopos_abi::syscall::MSG_NOSIGNAL;

pub const MAX_CLIENTS: usize = 32;

/// Some 200 events, three seconds of them at 60 fps.
const WRITE_BUF_SIZE: usize = 4096;

struct WriteBuf {
    data: [u8; WRITE_BUF_SIZE],
    len: usize,
}

impl WriteBuf {
    const fn new() -> Self {
        Self {
            data: [0; WRITE_BUF_SIZE],
            len: 0,
        }
    }

    /// `false` when there is no room.
    fn put(&mut self, payload: &[u8]) -> bool {
        let framed_len = 4 + payload.len();
        if self.len + framed_len > WRITE_BUF_SIZE {
            return false;
        }
        let header = (payload.len() as u32).to_le_bytes();
        self.data[self.len..self.len + 4].copy_from_slice(&header);
        self.data[self.len + 4..self.len + framed_len].copy_from_slice(payload);
        self.len += framed_len;
        true
    }

    /// Whether it drained; an error is a dead connection.
    fn flush(&mut self, fd: i32) -> Result<bool, ProtocolError> {
        while self.len > 0 {
            match slopos_cabi::send(fd, &self.data[..self.len], MSG_NOSIGNAL as i32) {
                Ok(n) if n > 0 => {
                    let remaining = self.len - n;
                    if remaining > 0 {
                        self.data.copy_within(n..self.len, 0);
                    }
                    self.len = remaining;
                }
                Ok(0) => return Err(ProtocolError::Disconnected),
                Err(Errno::EAGAIN) => return Ok(false),
                Err(_) => return Err(ProtocolError::Io),
                Ok(_) => unreachable!(),
            }
        }
        Ok(true)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub struct ClientConn {
    pub conn: Connection,
    pub active: bool,
    write_buf: WriteBuf,
    greeted: bool,
}

pub struct Server {
    listen_fd: i32,
    // Boxed for one niche: inline, `[None; N]` was const-initialised through
    // one niche and read through another, as `Some(garbage)`.
    pub clients: alloc::boxed::Box<[Option<alloc::boxed::Box<ClientConn>>; MAX_CLIENTS]>,
}

impl Server {
    pub fn bind(path: &[u8]) -> Result<Self, ProtocolError> {
        let fd = slopos_cabi::socket(AF_UNIX as i32, SOCK_STREAM as i32, 0)
            .map_err(|_| ProtocolError::Io)?;
        slopos_cabi::bind_unix(fd, path).map_err(|_| ProtocolError::Io)?;
        slopos_cabi::listen(fd, 32).map_err(|_| ProtocolError::Io)?;
        crate::connection::set_nonblock(fd);

        const NONE: Option<alloc::boxed::Box<ClientConn>> = None;
        Ok(Self {
            listen_fd: fd,
            clients: alloc::boxed::Box::new([NONE; MAX_CLIENTS]),
        })
    }

    /// The new client's index, or `None` with none waiting.
    pub fn accept(&mut self) -> Result<Option<usize>, ProtocolError> {
        let client_fd = match slopos_cabi::accept(self.listen_fd) {
            Ok(fd) => fd,
            Err(Errno::EAGAIN) => return Ok(None),
            Err(_) => return Err(ProtocolError::Io),
        };

        let idx = match self.clients.iter().position(|c| c.is_none()) {
            Some(i) => i,
            None => {
                let _ = slopos_cabi::close(client_fd);
                return Err(ProtocolError::BufferFull);
            }
        };

        let conn = Connection::new(client_fd);
        self.clients[idx] = Some(alloc::boxed::Box::new(ClientConn {
            conn,
            active: true,
            write_buf: WriteBuf::new(),
            greeted: false,
        }));
        Ok(Some(idx))
    }

    /// The first request must be a `Hello` naming [`PROTOCOL_VERSION`], and
    /// only the first may be one; anything else is an error the caller ends
    /// the connection on.
    pub fn recv_request(&mut self, client: usize) -> Result<Option<Request>, ProtocolError> {
        let c = match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c,
            _ => return Ok(None),
        };
        let request = c.conn.recv::<Request>()?;
        match (c.greeted, request) {
            (false, Some(Request::Hello { version })) => {
                if version != PROTOCOL_VERSION {
                    return Err(ProtocolError::VersionMismatch);
                }
                c.greeted = true;
                Ok(Some(Request::Hello { version }))
            }
            (false, Some(_)) | (true, Some(Request::Hello { .. })) => {
                Err(ProtocolError::MalformedMessage)
            }
            (_, request) => Ok(request),
        }
    }

    /// A full buffer is flushed once; a client whose buffer stays full is
    /// flagged disconnected, its slot left for [`take_disconnected`].
    pub fn queue_event(&mut self, client: usize, event: &Event) -> Result<(), ProtocolError> {
        let mut scratch = [0u8; 8192];
        let payload_len = event.encode(&mut scratch)?;
        let payload = &scratch[..payload_len];

        let c = match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c,
            _ => return Err(ProtocolError::Disconnected),
        };

        if c.write_buf.put(payload) {
            return Ok(());
        }

        match c.write_buf.flush(c.conn.fd()) {
            Ok(_) => {}
            Err(_) => {
                self.mark_disconnected(client);
                return Err(ProtocolError::Disconnected);
            }
        }

        let c = match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c,
            _ => return Err(ProtocolError::Disconnected),
        };
        if c.write_buf.put(payload) {
            return Ok(());
        }

        self.mark_disconnected(client);
        Err(ProtocolError::Disconnected)
    }

    /// A client that cannot be written is flagged for [`take_disconnected`].
    pub fn flush_clients(&mut self) {
        for i in 0..MAX_CLIENTS {
            let died = match self.clients.get_mut(i) {
                Some(Some(c)) if c.active && !c.write_buf.is_empty() => {
                    c.write_buf.flush(c.conn.fd()).is_err()
                }
                _ => false,
            };
            if died {
                self.mark_disconnected(i);
            }
        }
    }

    pub fn is_connected(&self, client: usize) -> bool {
        matches!(self.clients.get(client), Some(Some(c)) if c.active)
    }

    /// What a probe reads stays for the next `recv_request`; end of file flags
    /// the client disconnected.
    pub fn probe_disconnected(&mut self, client: usize) -> bool {
        match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => {
                if c.conn.probe_disconnected() {
                    c.active = false;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// The slot stays until its owner, which keys state on it, takes it
    /// through [`take_disconnected`].
    fn mark_disconnected(&mut self, client: usize) {
        if let Some(Some(c)) = self.clients.get_mut(client) {
            c.active = false;
        }
    }

    /// The clients flagged disconnected, whose slots stay until the caller has
    /// torn its own state down and called [`disconnect`] on each.
    pub fn take_disconnected(&mut self, out: &mut [usize]) -> usize {
        let mut n = 0;
        for (i, slot) in self.clients.iter().enumerate() {
            if n >= out.len() {
                break;
            }
            if let Some(c) = slot {
                if !c.active {
                    out[n] = i;
                    n += 1;
                }
            }
        }
        n
    }

    pub fn disconnect(&mut self, client: usize) {
        if let Some(slot) = self.clients.get_mut(client) {
            *slot = None;
        }
    }

    pub fn listen_fd(&self) -> i32 {
        self.listen_fd
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = slopos_cabi::close(self.listen_fd);
    }
}
