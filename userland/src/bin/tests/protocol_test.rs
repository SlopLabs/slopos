//! A compositor this test starts, since a test boot runs none, serves a client
//! of `PROTOCOL_VERSION` and refuses another version, or a request before `Hello`, with a reason.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use slopos_userland as _;

use slopos_abi::caps::{DISPLAY_SEAT, INPUT_SEAT};
use slopos_abi::net::{AF_UNIX, SOCK_STREAM};
use slopos_abi::signal::SIGKILL;
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_protocol::codec::tag::{EVT_END, REQ_END};
use slopos_protocol::connection::MAX_PENDING_FDS;
use slopos_protocol::{
    Client, Connection, Decode, Encode, Event, FdFifo, PROTOCOL_VERSION, ProtocolError, Request,
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
    let path = slopos_protocol::socket_path();
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
        Ok(Event::Hello { version }) if version == PROTOCOL_VERSION => true,
        Ok(Event::Hello { version }) => {
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

fn a_client_of_the_version_draws() -> bool {
    let Some(mut conn) = connect() else {
        return false;
    };
    if !greeted(&mut conn)
        || conn
            .send(&Request::Hello {
                version: PROTOCOL_VERSION,
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

fn the_client_connects() -> bool {
    let path = slopos_protocol::socket_path();
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

fn an_older_version_is_refused() -> bool {
    refuses_version(PROTOCOL_VERSION - 1)
}

fn a_newer_version_is_refused() -> bool {
    refuses_version(PROTOCOL_VERSION + 1)
}

/// Version 1's tags and Hellos; a change to any of them is a new version.
fn the_wire_is_this_versions() -> bool {
    use slopos_protocol::codec::tag::*;
    let requests = [
        REQ_HELLO,
        REQ_CREATE_SURFACE,
        REQ_SURFACE_ATTACH,
        REQ_SURFACE_DAMAGE,
        REQ_SURFACE_COMMIT,
        REQ_SURFACE_FRAME,
        REQ_SURFACE_DESTROY,
        REQ_GET_TOPLEVEL,
        REQ_TOPLEVEL_SET_TITLE,
        REQ_TOPLEVEL_SET_APP_ID,
        REQ_TOPLEVEL_DESTROY,
        REQ_SET_CURSOR_SHAPE,
        REQ_CLIPBOARD_COPY,
        REQ_CLIPBOARD_PASTE,
        REQ_CLIPBOARD_READ,
        REQ_TOPLEVEL_SET_SIZE_LIMITS,
        REQ_END,
    ];
    let events = [
        EVT_HELLO,
        EVT_OUTPUT_INFO,
        EVT_FRAME_DONE,
        EVT_BUFFER_RELEASE,
        EVT_CONFIGURE,
        EVT_CLOSE,
        EVT_POINTER_ENTER,
        EVT_POINTER_LEAVE,
        EVT_POINTER_MOTION,
        EVT_POINTER_BUTTON,
        EVT_POINTER_AXIS,
        EVT_KEY,
        EVT_MODIFIERS,
        EVT_PASTE_READY,
        EVT_PASTE_RESULT,
        EVT_ERROR,
        EVT_END,
    ];
    let mut request = [0u8; 8];
    let mut event = [0u8; 8];
    PROTOCOL_VERSION == 1
        && requests.iter().copied().eq(0..=16)
        && events.iter().copied().eq(0..=16)
        && Request::Hello { version: 1 }.encode(&mut request) == Ok(5)
        && Event::Hello { version: 1 }.encode(&mut event) == Ok(5)
        && request[..5] == [0, 1, 0, 0, 0]
        && event[..5] == [0, 1, 0, 0, 0]
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

/// A connection past its handshake.
fn served() -> Option<Connection> {
    let mut conn = connect()?;
    let ready = greeted(&mut conn)
        && conn
            .send(&Request::Hello {
                version: PROTOCOL_VERSION,
            })
            .is_ok()
        && matches!(
            conn.wait_recv::<Event>(TIMEOUT_MS),
            Ok(Event::OutputInfo { .. })
        );
    ready.then_some(conn)
}

fn reused_surface_id_is_refused() -> bool {
    let Some(conn) = served() else {
        return false;
    };
    let surface = SurfaceId::from_raw(1);
    conn.send(&Request::CreateSurface { new_id: surface })
        .is_ok()
        && conn
            .send(&Request::CreateSurface { new_id: surface })
            .is_ok()
        && refused(conn, ProtocolError::MalformedMessage)
}

/// A message's bytes as they go on the wire after its length.
struct Raw<'a>(&'a [u8]);

impl Encode for Raw<'_> {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        let out = buf
            .get_mut(..self.0.len())
            .ok_or(ProtocolError::MessageTooLarge)?;
        out.copy_from_slice(self.0);
        Ok(self.0.len())
    }
}

fn an_unknown_request_is_refused() -> bool {
    [REQ_END, 0xff].into_iter().all(|tag| {
        let Some(conn) = served() else {
            return false;
        };
        let sent = conn.send(&Raw(&[tag, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]));
        let refused = sent.is_ok() && refused(conn, ProtocolError::MalformedMessage);
        if !refused {
            eprintln!("protocol_test: request {tag} was not refused");
        }
        refused
    })
}

fn an_unknown_event_is_refused() -> bool {
    [EVT_END, 0xff].into_iter().all(|tag| {
        let mut fds = [-1i32; MAX_PENDING_FDS];
        let mut count = 0u8;
        let mut fifo = FdFifo::new(&mut fds, &mut count);
        let refused = matches!(
            Event::decode(&[tag, 1, 0, 0, 0], &mut fifo),
            Err(ProtocolError::MalformedMessage)
        );
        if !refused {
            eprintln!("protocol_test: event {tag} was not refused");
        }
        refused
    })
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
        (
            "a_client_of_the_version_draws",
            a_client_of_the_version_draws,
        ),
        ("the_client_connects", the_client_connects),
        ("an_older_version_is_refused", an_older_version_is_refused),
        ("the_wire_is_this_versions", the_wire_is_this_versions),
        ("a_newer_version_is_refused", a_newer_version_is_refused),
        (
            "request_before_hello_is_refused",
            request_before_hello_is_refused,
        ),
        ("reused_surface_id_is_refused", reused_surface_id_is_refused),
        (
            "an_unknown_request_is_refused",
            an_unknown_request_is_refused,
        ),
        ("an_unknown_event_is_refused", an_unknown_event_is_refused),
        (
            "seat_is_free_once_the_compositor_is_gone",
            seat_is_free_once_the_compositor_is_gone,
        ),
    ]);
}
