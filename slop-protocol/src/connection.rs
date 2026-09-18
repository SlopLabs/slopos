//! Socket connection with length-prefixed framing.
//!
//! The socket is always non-blocking (`O_NONBLOCK` set once at creation); all
//! blocking goes through `poll()`.
//!
//! Every read is a `recvmsg`, so SCM_RIGHTS fds are queued in a pending FIFO
//! and consumed inline by `Decode`: a single socket read carrying several
//! messages cannot mis-assign an fd.

use crate::codec::{Decode, Encode, FdFifo};
use crate::types::ProtocolError;
use slopos_abi::fs::UserIovec;
use slopos_abi::syscall::posix::{F_SETFL, O_NONBLOCK, POLLERR, POLLHUP, POLLIN, POLLOUT};
use slopos_abi::syscall::types::UserPollFd;
use slopos_abi::syscall::{
    CMSG_DATA_OFFSET, CmsgHdr, MsgHdr, SCM_MAX_FDS, SCM_RIGHTS, SOL_SOCKET, cmsg_len, cmsg_space,
};
use slopos_slibc::errno;
use slopos_slibc::pal::{Pal, Sys};

const READ_BUF_SIZE: usize = 16384;
const MAX_MSG_SIZE: usize = 8192;

/// Maximum queued fds from recvmsg ancillary data.
pub const MAX_PENDING_FDS: usize = 8;

/// A control buffer holding exactly one `SCM_RIGHTS` item, sized for the
/// kernel's `SCM_MAX_FDS` cap — one `recvmsg` drains the socket's ancillary
/// queue, not just the item that rode with this message's bytes.
///
/// Typed rather than a byte array because the kernel writes a `CmsgHdr` at its
/// head and this side reads one back: a `[u8; N]` is 1-aligned, so the read
/// would be unaligned.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ScmRightsBuf {
    hdr: CmsgHdr,
    fds: [i32; SCM_MAX_FDS],
}

const _: () = assert!(
    core::mem::size_of::<ScmRightsBuf>() == cmsg_space(SCM_MAX_FDS * core::mem::size_of::<i32>()),
    "ScmRightsBuf must be exactly one SCM_RIGHTS item's CMSG_SPACE"
);

/// Set O_NONBLOCK on a socket FD, preserving any existing flags.
pub fn set_nonblock(fd: i32) {
    use slopos_abi::syscall::posix::F_GETFL;
    let flags = Sys::fcntl(fd, F_GETFL as i32, 0).unwrap_or(0) as u64;
    let _ = Sys::fcntl(fd, F_SETFL as i32, flags | O_NONBLOCK);
}

pub struct Connection {
    fd: i32,
    read_buf: alloc::boxed::Box<[u8; READ_BUF_SIZE]>,
    read_len: usize,
    read_pos: usize,
    /// FIFO of fds received via SCM_RIGHTS but not yet consumed by the codec.
    pending_fds: [i32; MAX_PENDING_FDS],
    pending_fd_count: u8,
}

impl Connection {
    /// Create a connection from an already-connected socket FD.
    pub fn new(fd: i32) -> Self {
        set_nonblock(fd);
        Self {
            fd,
            read_buf: alloc::boxed::Box::new([0u8; READ_BUF_SIZE]),
            read_len: 0,
            read_pos: 0,
            pending_fds: [-1; MAX_PENDING_FDS],
            pending_fd_count: 0,
        }
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// The only place that adds fds to the pending FIFO. If the FIFO is full
    /// the fd is closed immediately rather than leaked.
    fn enqueue_fd(&mut self, fd: i32) {
        if (self.pending_fd_count as usize) < MAX_PENDING_FDS {
            self.pending_fds[self.pending_fd_count as usize] = fd;
            self.pending_fd_count += 1;
        } else if fd >= 0 {
            let _ = Sys::close(fd);
        }
    }

    /// Send a message immediately to the socket. No write buffer.
    pub fn send<T: Encode>(&self, msg: &T) -> Result<(), ProtocolError> {
        let mut buf = [0u8; MAX_MSG_SIZE];
        let payload_len = msg.encode(&mut buf[4..])?;
        if payload_len > MAX_MSG_SIZE - 4 {
            return Err(ProtocolError::MessageTooLarge);
        }
        let len_bytes = (payload_len as u32).to_le_bytes();
        buf[0..4].copy_from_slice(&len_bytes);
        let total = 4 + payload_len;

        let mut sent = 0usize;
        while sent < total {
            let ptr = unsafe { buf.as_ptr().add(sent) };
            let remaining = total - sent;
            match Sys::send(self.fd, ptr, remaining, 0) {
                Ok(n) if n > 0 => sent += n,
                Ok(0) => return Err(ProtocolError::Disconnected),
                Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => {
                    self.poll_writable(2000)?;
                }
                Err(_) => return Err(ProtocolError::Io),
                Ok(_) => unreachable!(),
            }
        }
        Ok(())
    }

    /// Send a message with an attached file descriptor via SCM_RIGHTS.
    pub fn send_with_fd<T: Encode>(&self, msg: &T, fd: i32) -> Result<(), ProtocolError> {
        let mut buf = [0u8; MAX_MSG_SIZE];
        let payload_len = msg.encode(&mut buf[4..])?;
        if payload_len > MAX_MSG_SIZE - 4 {
            return Err(ProtocolError::MessageTooLarge);
        }
        let len_bytes = (payload_len as u32).to_le_bytes();
        buf[0..4].copy_from_slice(&len_bytes);
        let total = 4 + payload_len;

        let mut fds = [-1i32; SCM_MAX_FDS];
        fds[0] = fd;
        let cmsg = ScmRightsBuf {
            hdr: CmsgHdr {
                cmsg_len: cmsg_len(core::mem::size_of::<i32>()) as u64,
                cmsg_level: SOL_SOCKET,
                cmsg_type: SCM_RIGHTS,
            },
            fds,
        };
        let iov = UserIovec {
            iov_base: buf.as_ptr() as u64,
            iov_len: total as u64,
        };

        let msg_hdr = MsgHdr {
            msg_iov: &iov as *const UserIovec as u64,
            msg_iovlen: 1,
            msg_control: &cmsg as *const ScmRightsBuf as u64,
            msg_controllen: cmsg_len(core::mem::size_of::<i32>()) as u64,
            ..Default::default()
        };

        // The fd attaches to the first byte via SCM_RIGHTS, so it rides exactly
        // one sendmsg; a short write here commits the fd plus `n` bytes and the
        // tail must still be drained below, or the receiver's framing desyncs.
        let mut sent = loop {
            match Sys::sendmsg(self.fd, &msg_hdr, 0) {
                Ok(n) if n > 0 => break n,
                Ok(_) => return Err(ProtocolError::Disconnected),
                Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => {
                    self.poll_writable(2000)?;
                }
                Err(_) => return Err(ProtocolError::Io),
            }
        };

        while sent < total {
            let ptr = unsafe { buf.as_ptr().add(sent) };
            let remaining = total - sent;
            match Sys::send(self.fd, ptr, remaining, 0) {
                Ok(n) if n > 0 => sent += n,
                Ok(_) => return Err(ProtocolError::Disconnected),
                Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => {
                    self.poll_writable(2000)?;
                }
                Err(_) => return Err(ProtocolError::Io),
            }
        }
        Ok(())
    }

    /// Try to receive one complete message (non-blocking).
    /// Returns `Ok(None)` if no complete message is available.
    pub fn recv<T: Decode>(&mut self) -> Result<Option<T>, ProtocolError> {
        if let Some(msg) = self.try_decode::<T>()? {
            return Ok(Some(msg));
        }
        self.try_fill_buf()?;
        self.try_decode::<T>()
    }

    /// Block via poll() until a complete message arrives or timeout expires.
    pub fn wait_recv<T: Decode>(&mut self, timeout_ms: i32) -> Result<T, ProtocolError> {
        if let Some(msg) = self.recv::<T>()? {
            return Ok(msg);
        }

        let start = crate::timestamp_ms();
        let deadline = if timeout_ms < 0 {
            u64::MAX
        } else {
            start.saturating_add(timeout_ms as u64)
        };
        loop {
            let now = crate::timestamp_ms();
            if now >= deadline {
                return Err(ProtocolError::Timeout);
            }
            let remaining = (deadline - now) as i32;
            self.poll_readable(remaining)?;
            self.try_fill_buf()?;
            if let Some(msg) = self.try_decode::<T>()? {
                return Ok(msg);
            }
        }
    }

    fn poll_readable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        let mut pfd = UserPollFd {
            fd: self.fd,
            events: POLLIN,
            revents: 0,
        };
        loop {
            match Sys::poll(&mut pfd as *mut _ as *mut u8, 1, timeout_ms) {
                Ok(rc) if rc < 0 => return Err(ProtocolError::Io),
                Err(_) => {
                    let e = errno::errno_get();
                    if e == errno::EINTR.raw() {
                        continue;
                    }
                    return Err(ProtocolError::Io);
                }
                _ => {}
            }
            if pfd.revents & (POLLERR | POLLHUP) != 0 {
                return Err(ProtocolError::Disconnected);
            }
            return Ok(());
        }
    }

    fn poll_writable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        let mut pfd = UserPollFd {
            fd: self.fd,
            events: POLLOUT,
            revents: 0,
        };
        match Sys::poll(&mut pfd as *mut _ as *mut u8, 1, timeout_ms) {
            Ok(rc) if rc < 0 => return Err(ProtocolError::Io),
            Err(_) => return Err(ProtocolError::Io),
            _ => {}
        }
        if pfd.revents & (POLLERR | POLLHUP) != 0 {
            return Err(ProtocolError::Io);
        }
        Ok(())
    }

    /// Decode one length-prefixed frame from the read buffer.
    fn try_decode<T: Decode>(&mut self) -> Result<Option<T>, ProtocolError> {
        let available = self.read_len - self.read_pos;
        if available < 4 {
            return Ok(None);
        }

        let len_bytes = &self.read_buf[self.read_pos..self.read_pos + 4];
        let payload_len =
            u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;

        if payload_len > MAX_MSG_SIZE {
            return Err(ProtocolError::MalformedMessage);
        }
        if available < 4 + payload_len {
            return Ok(None);
        }

        let payload_start = self.read_pos + 4;
        let payload = &self.read_buf[payload_start..payload_start + payload_len];
        let mut fifo = FdFifo::new(&mut self.pending_fds, &mut self.pending_fd_count);
        let (msg, _consumed) = T::decode(payload, &mut fifo)?;
        self.read_pos += 4 + payload_len;
        Ok(Some(msg))
    }

    /// Fill the read buffer using recvmsg (captures SCM_RIGHTS fds too).
    fn try_fill_buf(&mut self) -> Result<(), ProtocolError> {
        if self.read_len >= READ_BUF_SIZE {
            self.compact_buf();
            if self.read_len >= READ_BUF_SIZE {
                return Err(ProtocolError::BufferFull);
            }
        }

        let ptr = unsafe { self.read_buf.as_mut_ptr().add(self.read_len) };
        let avail = READ_BUF_SIZE - self.read_len;

        let mut cmsg = ScmRightsBuf::default();
        let iov = UserIovec {
            iov_base: ptr as u64,
            iov_len: avail as u64,
        };

        let mut msg_hdr = MsgHdr {
            msg_iov: &iov as *const UserIovec as u64,
            msg_iovlen: 1,
            msg_control: &mut cmsg as *mut ScmRightsBuf as u64,
            msg_controllen: core::mem::size_of::<ScmRightsBuf>() as u64,
            ..Default::default()
        };

        match Sys::recvmsg(self.fd, &mut msg_hdr, 0) {
            Ok(n) if n > 0 => {
                self.read_len += n;
            }
            Ok(0) => return Err(ProtocolError::Disconnected),
            Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => return Ok(()),
            Err(_) => return Err(ProtocolError::Io),
            Ok(_) => return Ok(()),
        }

        // One item is all the kernel writes, and it names between one and
        // `SCM_MAX_FDS` descriptors: a single `recvmsg` drains the socket's
        // whole ancillary queue, so reading only the first would leak the rest.
        let reported = msg_hdr.msg_controllen as usize;
        if reported >= cmsg_len(core::mem::size_of::<i32>())
            && cmsg.hdr.cmsg_level == SOL_SOCKET
            && cmsg.hdr.cmsg_type == SCM_RIGHTS
        {
            let payload = (cmsg.hdr.cmsg_len as usize).saturating_sub(CMSG_DATA_OFFSET);
            let n_fds = (payload / core::mem::size_of::<i32>()).min(SCM_MAX_FDS);
            for &fd in cmsg.fds.iter().take(n_fds) {
                self.enqueue_fd(fd);
            }
        }

        Ok(())
    }

    fn compact_buf(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let remaining = self.read_len - self.read_pos;
        self.read_buf.copy_within(self.read_pos..self.read_len, 0);
        self.read_pos = 0;
        self.read_len = remaining;
    }

    /// Check if the peer has disconnected by attempting a non-blocking read.
    pub fn probe_disconnected(&mut self) -> bool {
        matches!(self.try_fill_buf(), Err(ProtocolError::Disconnected))
    }

    /// Consume the connection and return the raw FD without closing it.
    pub fn into_raw_fd(mut self) -> i32 {
        let fd = self.fd;
        self.fd = -1;
        fd
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self.fd >= 0 {
            let _ = Sys::close(self.fd);
        }
        for i in 0..self.pending_fd_count as usize {
            if self.pending_fds[i] >= 0 {
                let _ = Sys::close(self.pending_fds[i]);
            }
        }
    }
}
