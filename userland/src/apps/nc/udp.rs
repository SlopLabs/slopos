use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Instant;

use slopos_abi::net::SockAddrIn;

use crate::ring::{Ring, slopfut};

use super::{NcConfig, StdinResult, verbose_addr, verbose_bytes, verbose_msg, verbose_recv};

/// A connected datagram socket runs the TCP path's [`Session`](super::ring_io),
/// which reads an empty datagram as data rather than as a close.
pub(super) fn udp_client(config: &NcConfig) -> u8 {
    use slopos_abi::net::{AF_INET, SOCK_DGRAM};

    let remote = SocketAddrV4::new(Ipv4Addr::from(config.remote_addr), config.remote_port);
    let dest = super::tcp::to_sockaddr(remote);

    let fd = match crate::syscall::net::socket(AF_INET, SOCK_DGRAM, 0) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("nc: socket creation failed");
            return 1;
        }
    };

    if config.local_port != 0 && crate::syscall::net::bind_any(fd.raw(), config.local_port).is_err()
    {
        eprintln!("nc: bind failed (port in use?)");
        return 1;
    }

    if crate::syscall::net::connect(fd.raw(), &dest).is_err() {
        eprintln!("nc: connect failed");
        return 1;
    }

    let conn = super::tcp::TcpConn::from_fd(fd);

    if crate::syscall::net::set_nonblocking(conn.raw()).is_err() {
        eprintln!("nc: failed to set non-blocking");
        return 1;
    }

    verbose_addr(
        config,
        "connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, "protocol: udp");

    super::ring_io::Session::new(config, &conn, true)
        .run()
        .code()
}

/// Listen mode must learn each datagram's source address to reply, which is
/// what `OP_RECVFROM` (SLOPRING § 12) returns alongside the data. Replies go
/// to the last peer heard from, and stdin is not read until there is one.
pub(super) fn udp_listen(config: &NcConfig) -> u8 {
    use slopos_abi::net::{AF_INET, SOCK_DGRAM};

    let fd = match crate::syscall::net::socket(AF_INET, SOCK_DGRAM, 0) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("nc: socket creation failed");
            return 1;
        }
    };
    if crate::syscall::net::set_reuse_addr(fd.raw()).is_err() {
        // Non-fatal: reuse-addr is a convenience, not a correctness need.
    }
    if crate::syscall::net::bind_any(fd.raw(), config.local_port).is_err() {
        eprintln!("nc: bind failed (port in use?)");
        return 1;
    }
    if crate::syscall::net::set_nonblocking(fd.raw()).is_err() {
        eprintln!("nc: failed to set non-blocking");
        return 1;
    }

    verbose_msg(
        config,
        &format!("listening on 0.0.0.0:{} (udp)", config.local_port),
    );

    let ring = match Ring::setup(16) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("nc: ring setup failed");
            return 1;
        }
    };

    let sock_fd = fd.raw();
    slopfut::block_on(ring, listen_async(config, sock_fd))
}

const TTY_CAP: usize = 64;
const RECV_CAP: usize = 64 * 1024;
/// Bounds the otherwise I/O-only `select` so the inactivity timeout is checked
/// even while no data flows.
const TIMER_TICK_NS: u64 = 200_000_000;

async fn listen_async(config: &NcConfig, sock_fd: i32) -> u8 {
    type DynStdin = core::pin::Pin<Box<dyn core::future::Future<Output = slopfut::BufResult>>>;
    type DynRecv = core::pin::Pin<Box<dyn core::future::Future<Output = slopfut::RecvFromResult>>>;
    type DynInt = core::pin::Pin<Box<dyn core::future::Future<Output = i32>>>;

    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0usize;
    let mut last_peer: Option<SocketAddrV4> = None;
    let mut stdin_closed = false;
    let clock_start = Instant::now();
    let mut last_activity_ms = clock_start.elapsed().as_millis() as u64;
    let stdin_cap = if config.stdin_tty {
        TTY_CAP
    } else {
        super::UNFRAGMENTED_DATAGRAM
    };

    let mut stdin_read: Option<DynStdin> = None;
    let mut recv_read: Option<DynRecv> = None;
    loop {
        let stdin = stdin_read.get_or_insert_with(|| {
            if stdin_closed || last_peer.is_none() {
                Box::pin(core::future::pending())
            } else {
                Box::pin(slopfut::read(0, vec![0u8; stdin_cap], stdin_cap as u32))
            }
        });
        let recv = recv_read.get_or_insert_with(|| {
            Box::pin(slopfut::recvfrom(
                sock_fd,
                vec![0u8; RECV_CAP],
                RECV_CAP as u32,
            ))
        });
        let timer: DynInt = if config.timeout_ms > 0 {
            Box::pin(slopfut::timeout(TIMER_TICK_NS))
        } else {
            Box::pin(core::future::pending())
        };

        match slopfut::select3(stdin, recv, timer).await {
            slopfut::Either3::A(br) => {
                stdin_read = None;
                match on_stdin(
                    config,
                    sock_fd,
                    &mut line_buf,
                    &mut line_pos,
                    last_peer,
                    br.res,
                    &br.buf,
                )
                .await
                {
                    StdinAction::Quit => return 0,
                    StdinAction::Sent => {
                        last_activity_ms = clock_start.elapsed().as_millis() as u64
                    }
                    StdinAction::Eof => {
                        stdin_closed = true;
                        verbose_msg(config, "stdin EOF");
                    }
                    StdinAction::Continue => {}
                }
            }
            slopfut::Either3::B(rr) => {
                recv_read = None;
                if rr.res > 0 {
                    let received = (rr.res as usize).min(rr.buf.len());
                    if !super::emit_received(config, &rr.buf[..received]) {
                        return 1;
                    }
                    let ip = rr.src.addr;
                    let port = u16::from_be(rr.src.port);
                    verbose_recv(config, received, ip, port);
                    if last_peer.is_none() {
                        stdin_read = None;
                    }
                    last_peer = Some(SocketAddrV4::new(Ipv4Addr::from(ip), port));
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
            }
            slopfut::Either3::C(_) => {
                if config.timeout_ms > 0 {
                    let now = clock_start.elapsed().as_millis() as u64;
                    if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                        eprintln!("nc: timeout");
                        return 1;
                    }
                }
            }
        }
    }
}

enum StdinAction {
    Continue,
    Sent,
    Eof,
    Quit,
}

async fn on_stdin(
    config: &NcConfig,
    sock_fd: i32,
    line_buf: &mut [u8; 1024],
    line_pos: &mut usize,
    last_peer: Option<SocketAddrV4>,
    res: i32,
    buf: &[u8],
) -> StdinAction {
    if res == 0 {
        return StdinAction::Eof;
    }
    if res < 0 {
        // Would-block stays in-flight, so this is a genuine stdin error.
        return StdinAction::Eof;
    }
    let n = (res as usize).min(buf.len());
    if !config.stdin_tty {
        return match last_peer {
            Some(peer) if send_to_peer(config, sock_fd, peer, &buf[..n]).await => StdinAction::Sent,
            _ => StdinAction::Continue,
        };
    }
    let mut sent_any = false;
    for &byte in &buf[..n] {
        match super::process_raw_stdin_char(config, byte, line_buf, line_pos) {
            StdinResult::SendLine(len) => {
                if let Some(peer) = last_peer {
                    let line: Vec<u8> = line_buf[..len].to_vec();
                    if send_to_peer(config, sock_fd, peer, &line).await {
                        sent_any = true;
                    }
                }
                *line_pos = 0;
            }
            StdinResult::Quit => return StdinAction::Quit,
            StdinResult::Continue => {}
        }
    }
    if sent_any {
        StdinAction::Sent
    } else {
        StdinAction::Continue
    }
}

async fn send_to_peer(config: &NcConfig, sock_fd: i32, peer: SocketAddrV4, data: &[u8]) -> bool {
    let dest = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: peer.port().to_be(),
        addr: peer.ip().octets(),
        _pad: [0; 8],
    };
    if crate::syscall::net::connect(sock_fd, &dest).is_err() {
        eprintln!("nc: send failed");
        return false;
    }
    let br = slopfut::write(sock_fd, data.to_vec()).await;
    if br.res > 0 {
        verbose_bytes(config, "sent ", br.res as usize);
        true
    } else {
        eprintln!("nc: send failed");
        false
    }
}
