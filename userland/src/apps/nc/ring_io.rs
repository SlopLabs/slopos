//! Ring-driven, `async`/`await` I/O session for nc's connected recv/send loop.
//!
//! The established-connection loop races three leaf futures with
//! [`slopfut::select3`]: an `OP_READ` on stdin or the `OP_WRITE` sending what
//! it read, an `OP_READ` on the socket, and a periodic `OP_TIMEOUT` tick that
//! bounds the wait so the inactivity timeout is enforced. Multiplexing is the
//! kernel's caller-as-waiter harvest (SLOPRING § 7.1/§ 8.3).
//!
//! `connect`/`listen`/`accept`/`shutdown` stay regular syscalls, outside the
//! nine-opcode data plane (SLOPRING § 12).
//!
//! Each read stays in flight until it completes, across however many turns
//! the other one wins: cancelling a read the kernel has already completed
//! would drop the bytes it took.

use std::time::Instant;

use super::tcp::TcpConn;
use super::{NcConfig, StdinResult, verbose_bytes, verbose_msg};
use crate::ring::{Ring, slopfut};

const STDIN_FD: i32 = 0;
const TTY_CAP: usize = 64;
const PIPE_CAP: usize = 64 * 1024;
const SOCK_CAP: usize = 64 * 1024;
/// Periodic timer tick (ns). Bounds an otherwise I/O-only `select` so the
/// inactivity timeout is checked even while no data flows
/// (SLOPRING § 12 OP_TIMEOUT note).
const TIMER_TICK_NS: u64 = 200_000_000;

pub(super) enum Ended {
    /// The connection is over, with this exit status; a `-k` listener
    /// accepts again.
    Conn(u8),
    /// The user quit or local I/O failed: nc exits with this status.
    Quit(u8),
}

impl Ended {
    pub(super) fn code(self) -> u8 {
        match self {
            Ended::Conn(code) | Ended::Quit(code) => code,
        }
    }
}

/// One ring-driven established-connection session.
pub(super) struct Session<'a> {
    config: &'a NcConfig,
    conn: &'a TcpConn,
    datagram: bool,

    line_buf: [u8; 1024],
    line_pos: usize,

    stdin_closed: bool,
    /// The peer half-closed while redirected stdin still had bytes for it.
    sock_closed: bool,

    clock_start: Instant,
    last_activity_ms: u64,
}

impl<'a> Session<'a> {
    pub(super) fn new(config: &'a NcConfig, conn: &'a TcpConn, datagram: bool) -> Self {
        Self {
            config,
            conn,
            datagram,
            line_buf: [0u8; 1024],
            line_pos: 0,
            stdin_closed: false,
            sock_closed: false,
            clock_start: Instant::now(),
            last_activity_ms: 0,
        }
    }

    pub(super) fn run(mut self) -> Ended {
        // 16 SQ slots is comfortably more than the loop's peak in-flight count
        // (stdin + socket + one write + one timer).
        let ring = match Ring::setup(16) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("nc: ring setup failed");
                self.conn.shutdown_both();
                return Ended::Quit(1);
            }
        };
        self.last_activity_ms = self.clock_start.elapsed().as_millis() as u64;
        slopfut::block_on(ring, self.run_async())
    }

    async fn run_async(mut self) -> Ended {
        type DynBuf = core::pin::Pin<Box<dyn core::future::Future<Output = slopfut::BufResult>>>;
        type DynInt = core::pin::Pin<Box<dyn core::future::Future<Output = i32>>>;

        let stdin_cap = match (self.config.stdin_tty, self.datagram) {
            (true, _) => TTY_CAP,
            (false, true) => super::UNFRAGMENTED_DATAGRAM,
            (false, false) => PIPE_CAP,
        };
        // The write races the socket read, so a peer that writes before it
        // reads cannot deadlock against it.
        let mut outbound: Option<DynBuf> = None;
        let mut outbound_is_write = false;
        let mut sock_read: Option<DynBuf> = None;
        loop {
            let outbound_fut = outbound.get_or_insert_with(|| {
                if self.stdin_closed {
                    Box::pin(core::future::pending())
                } else {
                    Box::pin(slopfut::read(
                        STDIN_FD,
                        vec![0u8; stdin_cap],
                        stdin_cap as u32,
                    ))
                }
            });
            let fd_sock = self.conn.raw();
            let sock = sock_read.get_or_insert_with(|| {
                if self.sock_closed {
                    Box::pin(core::future::pending())
                } else {
                    Box::pin(slopfut::read(fd_sock, vec![0u8; SOCK_CAP], SOCK_CAP as u32))
                }
            });
            let timer: DynInt = if self.config.timeout_ms > 0 {
                Box::pin(slopfut::timeout(TIMER_TICK_NS))
            } else {
                Box::pin(core::future::pending())
            };

            match slopfut::select3(outbound_fut, sock, timer).await {
                slopfut::Either3::A(br) if outbound_is_write => {
                    outbound = None;
                    if br.res <= 0 {
                        eprintln!("nc: send failed (broken pipe)");
                        self.conn.shutdown_both();
                        return Ended::Conn(1);
                    }
                    let sent = (br.res as usize).min(br.buf.len());
                    verbose_bytes(self.config, "sent ", sent);
                    self.touch();
                    if sent < br.buf.len() {
                        let rest = br.buf[sent..].to_vec();
                        outbound = Some(Box::pin(slopfut::write(fd_sock, rest)));
                    } else {
                        outbound_is_write = false;
                    }
                }
                slopfut::Either3::A(mut br) if !self.config.stdin_tty && br.res > 0 => {
                    br.buf.truncate(br.res as usize);
                    outbound = Some(Box::pin(slopfut::write(fd_sock, br.buf)));
                    outbound_is_write = true;
                }
                slopfut::Either3::A(br) => {
                    outbound = None;
                    if let Some(out) = self.on_stdin(br.res, &br.buf).await {
                        return out;
                    }
                }
                slopfut::Either3::B(br) => {
                    sock_read = None;
                    if let Some(out) = self.on_sock(br.res, &br.buf) {
                        return out;
                    }
                }
                slopfut::Either3::C(_) => {
                    if let Some(out) = self.check_timeout() {
                        return out;
                    }
                }
            }
        }
    }

    async fn on_stdin(&mut self, res: i32, buf: &[u8]) -> Option<Ended> {
        if res <= 0 {
            // EOF or a genuine error; would-block never reaches here, the kernel
            // keeps those in-flight. Re-arming on an error would busy-spin.
            self.stdin_closed = true;
            if res == 0 {
                verbose_msg(self.config, "stdin EOF");
            }
            self.conn.shutdown_write();
            if self.sock_closed {
                self.conn.shutdown_both();
                return Some(Ended::Conn(0));
            }
            return None;
        }
        let n = (res as usize).min(buf.len());
        for &byte in &buf[..n] {
            let result = super::process_raw_stdin_char(
                self.config,
                byte,
                &mut self.line_buf,
                &mut self.line_pos,
            );
            match result {
                StdinResult::SendLine(len) => {
                    let line: Vec<u8> = self.line_buf[..len].to_vec();
                    if self.send_all(&line).await {
                        verbose_bytes(self.config, "sent ", line.len());
                        self.touch();
                    } else {
                        eprintln!("nc: send failed (broken pipe)");
                        self.conn.shutdown_both();
                        return Some(Ended::Conn(1));
                    }
                    self.line_pos = 0;
                }
                StdinResult::Quit => {
                    self.conn.shutdown_both();
                    return Some(Ended::Quit(0));
                }
                StdinResult::Continue => {}
            }
        }
        None
    }

    /// Send all of `data` over the socket via `OP_WRITE`, awaiting each
    /// chunk. Returns `false` on a write error (broken pipe).
    async fn send_all(&self, data: &[u8]) -> bool {
        let mut total = 0usize;
        while total < data.len() {
            let chunk = data[total..].to_vec();
            let br = slopfut::write(self.conn.raw(), chunk).await;
            if br.res <= 0 {
                return false;
            }
            total += br.res as usize;
        }
        true
    }

    fn on_sock(&mut self, res: i32, buf: &[u8]) -> Option<Ended> {
        if res == 0 && self.datagram {
            self.touch();
            return None;
        }
        if res == 0 {
            verbose_msg(self.config, "connection closed by remote");
            if self.stdin_closed || self.config.stdin_tty {
                self.conn.shutdown_both();
                return Some(Ended::Conn(0));
            }
            self.sock_closed = true;
            return None;
        }
        if res < 0 {
            eprintln!("nc: connection error");
            self.conn.shutdown_both();
            return Some(Ended::Conn(1));
        }
        let received = (res as usize).min(buf.len());
        if !super::emit_received(self.config, &buf[..received]) {
            self.conn.shutdown_both();
            return Some(Ended::Quit(1));
        }
        verbose_bytes(self.config, "received ", received);
        self.touch();
        None
    }

    fn touch(&mut self) {
        self.last_activity_ms = self.clock_start.elapsed().as_millis() as u64;
    }

    fn check_timeout(&mut self) -> Option<Ended> {
        if self.config.timeout_ms == 0 {
            return None;
        }
        let now = self.clock_start.elapsed().as_millis() as u64;
        if now.wrapping_sub(self.last_activity_ms) >= self.config.timeout_ms as u64 {
            eprintln!("nc: timeout");
            self.conn.shutdown_both();
            return Some(Ended::Conn(1));
        }
        None
    }
}
