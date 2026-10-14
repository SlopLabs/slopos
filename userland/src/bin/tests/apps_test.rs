//! The apps outside the base, run as the session runs them: each program at
//! `/usr/local/bin` is dynamically linked against `libc.so`, loads, finds its
//! session's compositor and names its window with the id its dock entry has.
//!
//! The compositor here is this test, serving the session's socket through
//! the protocol's own server: under `tests=on` init starts none, and a
//! client's requests are what show the app came up.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slopos_userland as _;

use slopos_abi::pixel::PixelFormat;
use slopos_protocol::{Event, PROTOCOL_VERSION, Request, Server};

const DEADLINE: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_millis(20);
const INTERP: &[u8] = b"/lib/ld-slopos.so.1\0";

const APPS: &[(&str, &str)] = &[
    ("/usr/local/bin/editor", "org.slopos.editor"),
    ("/usr/local/bin/file_manager", "org.slopos.files"),
    ("/usr/local/bin/image_viewer", "org.slopos.image-viewer"),
];

/// The program's `PT_INTERP`, read from its ELF header and program headers.
fn interpreter(program: &str) -> Result<Vec<u8>, String> {
    let elf = std::fs::read(program).map_err(|e| format!("{program}: {e}"))?;
    let word = |at: usize, len: usize| -> Option<u64> {
        let bytes = elf.get(at..at + len)?;
        Some(bytes.iter().rev().fold(0, |v, b| v << 8 | u64::from(*b)))
    };
    let (phoff, phentsize, phnum) = (word(32, 8), word(54, 2), word(56, 2));
    let (Some(phoff), Some(phentsize), Some(phnum)) = (phoff, phentsize, phnum) else {
        return Err(format!("{program}: not an ELF file"));
    };
    for i in 0..phnum {
        let at = (phoff + i * phentsize) as usize;
        if word(at, 4) == Some(3) {
            let (offset, size) = (word(at + 8, 8), word(at + 32, 8));
            if let (Some(offset), Some(size)) = (offset, size) {
                return elf
                    .get(offset as usize..(offset + size) as usize)
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| format!("{program}: PT_INTERP past the file"));
            }
        }
    }
    Err(format!(
        "{program}: no PT_INTERP, so not linked against libc.so"
    ))
}

fn exited(child: &mut Child) -> Option<String> {
    match child.try_wait() {
        Ok(Some(status)) => Some(format!("exited with {status}")),
        Ok(None) => None,
        Err(e) => Some(format!("cannot be waited for: {e}")),
    }
}

/// Serve `program` as its compositor until it names its window's app id.
fn app_id_of(program: &str) -> Result<String, String> {
    let path = slopos_protocol::socket_path(slopos_userland::syscall::process::geteuid());
    let shown = String::from_utf8_lossy(&path).into_owned();
    let _ = std::fs::remove_file(&shown);
    let mut server = Server::bind(&path).map_err(|e| format!("listening on {shown}: {e:?}"))?;
    let mut child = Command::new(program)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let answer = serve(&mut server, &mut child);
    let _ = child.kill();
    let _ = child.wait();
    drop(server);
    let _ = std::fs::remove_file(&shown);
    answer.map_err(|e| format!("{program}: {e}"))
}

fn serve(server: &mut Server, child: &mut Child) -> Result<String, String> {
    let start = Instant::now();
    let client = loop {
        match server.accept() {
            Ok(Some(client)) => break client,
            Ok(None) => {}
            Err(e) => return Err(format!("accepting: {e:?}")),
        }
        if let Some(how) = exited(child) {
            return Err(format!("{how} before it connected"));
        }
        if start.elapsed() > DEADLINE {
            return Err(format!("did not connect within {DEADLINE:?}"));
        }
        std::thread::sleep(RETRY);
    };
    let hello = Event::Hello {
        version: PROTOCOL_VERSION,
        capabilities: 0,
    };
    server
        .queue_event(client, &hello)
        .map_err(|e| format!("greeting: {e:?}"))?;
    server.flush_clients();
    while start.elapsed() < DEADLINE {
        match server.recv_request(client) {
            Ok(Some(Request::Hello { .. })) => {
                let output = Event::OutputInfo {
                    width: 1024,
                    height: 768,
                    format: PixelFormat::Argb8888 as u32,
                    pitch: 1024 * 4,
                    scale: 1,
                };
                server
                    .queue_event(client, &output)
                    .map_err(|e| format!("describing the output: {e:?}"))?;
                server.flush_clients();
            }
            Ok(Some(Request::ToplevelSetAppId { app_id, len, .. })) => {
                return Ok(String::from_utf8_lossy(&app_id[..len as usize]).into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Some(how) = exited(child) {
                    return Err(format!("{how} before it named its window"));
                }
                std::thread::sleep(RETRY);
            }
            Err(e) => return Err(format!("reading its requests: {e:?}")),
        }
    }
    Err(format!("named no window within {DEADLINE:?}"))
}

fn run(program: &str, want: &str) -> bool {
    match interpreter(program) {
        Ok(interp) if interp == INTERP => {}
        Ok(interp) => {
            eprintln!(
                "apps_test: {program} names the interpreter {:?}",
                String::from_utf8_lossy(&interp)
            );
            return false;
        }
        Err(e) => {
            eprintln!("apps_test: {e}");
            return false;
        }
    }
    match app_id_of(program) {
        Ok(id) if id == want => true,
        Ok(id) => {
            eprintln!("apps_test: {program} named its window {id:?}, not {want:?}");
            false
        }
        Err(e) => {
            eprintln!("apps_test: {e}");
            false
        }
    }
}

fn the_editor_runs() -> bool {
    run(APPS[0].0, APPS[0].1)
}

fn the_file_manager_runs() -> bool {
    run(APPS[1].0, APPS[1].1)
}

fn the_image_viewer_runs() -> bool {
    run(APPS[2].0, APPS[2].1)
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("the_editor_runs", the_editor_runs),
    ("the_file_manager_runs", the_file_manager_runs),
    ("the_image_viewer_runs", the_image_viewer_runs),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
