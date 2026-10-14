//! The compositor's protocol version range, held against the real compositor:
//! a client built against `PROTOCOL_VERSION_MIN` is served, and a version
//! outside the range or a request ahead of `Hello` is refused with a reason
//! before the connection closes.
//!
//! Under `tests=on` init starts no compositor, so this test starts its own.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use slopos_userland as _;

use slopos_abi::caps::{DISPLAY_SEAT, INPUT_SEAT};
use slopos_abi::net::{AF_UNIX, SOCK_STREAM};
use slopos_abi::signal::SIGKILL;
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_protocol::{
    Client, Connection, Event, PROTOCOL_VERSION, PROTOCOL_VERSION_MIN, ProtocolError, Request,
    SurfaceId, ToplevelId,
};
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::window::{SEAT_COMPOSITOR_PRIMARY, screen_acquire};
use slopos_userland::syscall::{ShmBuffer, fs, net};

const TIMEOUT_MS: i32 = 10_000;
const DEADLINE: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(20);

static COMPOSITOR: AtomicU32 = AtomicU32::new(0);

fn start_compositor() -> bool {
    let actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_endowed(
        b"/bin/compositor",
        &[],
        &[],
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: DISPLAY_SEAT | INPUT_SEAT,
            ..Endowment::NONE
        },
    );
    if tid <= 0 {
        eprintln!("protocol_test: spawning /bin/compositor gave {tid}");
        return false;
    }
    COMPOSITOR.store(tid as u32, Ordering::Relaxed);
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if try_connect().is_some() {
            return true;
        }
        if process::waitpid_nohang(tid as u32).is_some() {
            COMPOSITOR.store(0, Ordering::Relaxed);
            eprintln!("protocol_test: the compositor exited before it listened");
            return false;
        }
        std::thread::sleep(RETRY);
    }
    eprintln!("protocol_test: the compositor did not listen within {DEADLINE:?}");
    false
}

fn stop_compositor() {
    let tid = COMPOSITOR.swap(0, Ordering::Relaxed);
    if tid != 0 {
        process::kill(tid, SIGKILL);
        let _ = process::waitpid(tid);
    }
}

fn try_connect() -> Option<Connection> {
    let fd = net::socket(AF_UNIX, SOCK_STREAM, 0).ok()?;
    let path = slopos_protocol::socket_path(process::geteuid());
    net::connect_unix(fd.raw(), &path).ok()?;
    Some(Connection::new(fd.into_raw()))
}

fn connect() -> Option<Connection> {
    let conn = try_connect();
    if conn.is_none() {
        eprintln!("protocol_test: cannot connect to the compositor");
    }
    conn
}

fn greeted(conn: &mut Connection) -> bool {
    match conn.wait_recv::<Event>(TIMEOUT_MS) {
        Ok(Event::Hello { version, .. }) if version == PROTOCOL_VERSION => true,
        Ok(Event::Hello { version, .. }) => {
            eprintln!("protocol_test: compositor greeted with {version}, want {PROTOCOL_VERSION}");
            false
        }
        Ok(_) => {
            eprintln!("protocol_test: the first event was not Hello");
            false
        }
        Err(err) => {
            eprintln!("protocol_test: no Hello: {err:?}");
            false
        }
    }
}

fn refused(mut conn: Connection, reason: ProtocolError) -> bool {
    match conn.wait_recv::<Event>(TIMEOUT_MS) {
        Ok(Event::Error { object_id: 0, code }) if code == reason as u32 => {}
        Ok(Event::Error { code, .. }) => {
            eprintln!("protocol_test: refused with code {code}, want {reason:?}");
            return false;
        }
        Ok(_) => {
            eprintln!("protocol_test: served where {reason:?} was due");
            return false;
        }
        Err(err) => {
            eprintln!("protocol_test: no refusal, {err:?}");
            return false;
        }
    }
    match conn.wait_recv::<Event>(TIMEOUT_MS) {
        Err(ProtocolError::Disconnected) => true,
        other => {
            eprintln!(
                "protocol_test: the connection outlived its refusal ({:?})",
                other.err()
            );
            false
        }
    }
}

fn oldest_served_version_draws() -> bool {
    let Some(mut conn) = connect() else {
        return false;
    };
    if !greeted(&mut conn)
        || conn
            .send(&Request::Hello {
                version: PROTOCOL_VERSION_MIN,
            })
            .is_err()
    {
        return false;
    }
    match conn.wait_recv::<Event>(TIMEOUT_MS) {
        Ok(Event::OutputInfo { width, height, .. }) if width > 0 && height > 0 => {}
        _ => {
            eprintln!("protocol_test: no output after Hello");
            return false;
        }
    }

    const SIDE: u32 = 16;
    let Ok(buffer) = ShmBuffer::create((SIDE * SIDE * 4) as usize) else {
        eprintln!("protocol_test: no memfd for the buffer");
        return false;
    };
    let surface = SurfaceId::from_raw(1);
    let sent = conn
        .send(&Request::CreateSurface { new_id: surface })
        .is_ok()
        && conn
            .send(&Request::GetToplevel {
                surface,
                new_id: ToplevelId::from_raw(1),
            })
            .is_ok()
        && conn
            .send_with_fd(
                &Request::SurfaceAttach {
                    surface,
                    buffer_id: 0,
                    width: SIDE,
                    height: SIDE,
                    has_fd: true,
                    buffer_fd: None,
                },
                buffer.fd(),
            )
            .is_ok()
        && conn.send(&Request::SurfaceFrame { surface }).is_ok()
        && conn.send(&Request::SurfaceCommit { surface }).is_ok();
    if !sent {
        eprintln!("protocol_test: could not send the frame");
        return false;
    }
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        match conn.wait_recv::<Event>(TIMEOUT_MS) {
            Ok(Event::FrameDone { surface: done, .. }) if done == surface => return true,
            Ok(Event::Error { code, .. }) => {
                eprintln!("protocol_test: the frame was refused with code {code}");
                return false;
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("protocol_test: no FrameDone: {err:?}");
                return false;
            }
        }
    }
    eprintln!("protocol_test: no FrameDone within {DEADLINE:?}");
    false
}

fn newest_version_connects() -> bool {
    let path = slopos_protocol::socket_path(process::geteuid());
    match Client::connect(&path).and_then(|mut client| client.ensure_output_info()) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("protocol_test: Client::connect: {err:?}");
            false
        }
    }
}

fn refuses_version(version: u32) -> bool {
    let Some(mut conn) = connect() else {
        return false;
    };
    greeted(&mut conn)
        && conn.send(&Request::Hello { version }).is_ok()
        && refused(conn, ProtocolError::VersionMismatch)
}

fn version_below_oldest_is_refused() -> bool {
    refuses_version(PROTOCOL_VERSION_MIN - 1)
}

fn version_above_newest_is_refused() -> bool {
    refuses_version(PROTOCOL_VERSION + 1)
}

fn request_before_hello_is_refused() -> bool {
    let Some(mut conn) = connect() else {
        return false;
    };
    greeted(&mut conn)
        && conn
            .send(&Request::CreateSurface {
                new_id: SurfaceId::from_raw(1),
            })
            .is_ok()
        && refused(conn, ProtocolError::MalformedMessage)
}

fn reused_surface_id_is_refused() -> bool {
    let Some(mut conn) = connect() else {
        return false;
    };
    let surface = SurfaceId::from_raw(1);
    greeted(&mut conn)
        && conn
            .send(&Request::Hello {
                version: PROTOCOL_VERSION,
            })
            .is_ok()
        && matches!(
            conn.wait_recv::<Event>(TIMEOUT_MS),
            Ok(Event::OutputInfo { .. })
        )
        && conn
            .send(&Request::CreateSurface { new_id: surface })
            .is_ok()
        && conn
            .send(&Request::CreateSurface { new_id: surface })
            .is_ok()
        && refused(conn, ProtocolError::MalformedMessage)
}

/// Last, since it stops the compositor. The exit path wakes the parent before
/// its cleanup hooks revoke the seat, so the seat is waited for.
fn seat_is_free_once_the_compositor_is_gone() -> bool {
    stop_compositor();
    let start = Instant::now();
    loop {
        let fd = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
        if fd >= 0 {
            let _ = fs::close_fd_raw(fd as i32);
            return true;
        }
        if start.elapsed() >= DEADLINE {
            eprintln!("protocol_test: the seat stayed taken: {fd}");
            return false;
        }
        std::thread::sleep(RETRY);
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        stop_compositor();
        eprintln!("protocol_test: {info}");
    }));
    if !start_compositor() {
        stop_compositor();
        std::process::exit(1);
    }
    slopos_slibc::test_harness::run(&[
        ("oldest_served_version_draws", oldest_served_version_draws),
        ("newest_version_connects", newest_version_connects),
        (
            "version_below_oldest_is_refused",
            version_below_oldest_is_refused,
        ),
        (
            "version_above_newest_is_refused",
            version_above_newest_is_refused,
        ),
        (
            "request_before_hello_is_refused",
            request_before_hello_is_refused,
        ),
        ("reused_surface_id_is_refused", reused_surface_id_is_refused),
        (
            "seat_is_free_once_the_compositor_is_gone",
            seat_is_free_once_the_compositor_is_gone,
        ),
    ]);
}
