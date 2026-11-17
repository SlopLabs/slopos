//! Each app names its window as its dock entry does, and each of the image
//! slopos-essentials links `libc.so`.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slopos_abi::pixel::PixelFormat;
use slopos_protocol::{Event, Request, Server};

const DEADLINE: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_millis(20);
const INTERP: &[u8] = b"/lib/ld-slopos.so.1\0";

const APPS: &[(&str, &str)] = &[
    ("/opt/slopos-essentials/bin/editor", "org.slopos.editor"),
    (
        "/opt/slopos-essentials/bin/file_manager",
        "org.slopos.files",
    ),
    (
        "/opt/slopos-essentials/bin/image_viewer",
        "org.slopos.image-viewer",
    ),
    ("/bin/settings", "org.slopos.settings"),
    ("/bin/sysmon", "org.slopos.sysmon"),
    ("/bin/terminal", "org.slopos.terminal"),
    ("/opt/slopos-essentials/bin/breach", "org.slopos.breach"),
];
const ESSENTIALS: &str = "/opt/slopos-essentials/";

fn entry_name(program: &str, id: &str) -> Option<String> {
    let dir = match program.starts_with(ESSENTIALS) {
        true => "/opt/slopos-essentials/share",
        false => "/usr/share",
    };
    let share = slopos_userland::app_entry::Share {
        dir: dir.into(),
        only: None,
    };
    slopos_userland::app_entry::read(&[share], false)
        .into_iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.name)
}

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

/// The app id `program` names its window with, and the title before it.
fn app_id_of(program: &str) -> Result<(String, String), String> {
    let path = slopos_protocol::socket_path();
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

fn serve(server: &mut Server, child: &mut Child) -> Result<(String, String), String> {
    let mut title = String::new();
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
    server
        .greet(client)
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
            Ok(Some(Request::ToplevelSetTitle {
                title: said, len, ..
            })) => {
                title = String::from_utf8_lossy(&said[..len as usize]).into_owned();
            }
            Ok(Some(Request::ToplevelSetAppId { app_id, len, .. })) => {
                let id = String::from_utf8_lossy(&app_id[..len as usize]).into_owned();
                return Ok((id, title));
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
    let loose = program.starts_with(ESSENTIALS);
    match loose.then(|| interpreter(program)) {
        None => {}
        Some(Ok(interp)) if interp == INTERP => {}
        Some(Ok(interp)) => {
            eprintln!(
                "apps_test: {program} names the interpreter {:?}",
                String::from_utf8_lossy(&interp)
            );
            return false;
        }
        Some(Err(e)) => {
            eprintln!("apps_test: {e}");
            return false;
        }
    }
    let Some(name) = entry_name(program, want) else {
        eprintln!("apps_test: no app entry {want}");
        return false;
    };
    let one_name = |title: &str| title == name || title.ends_with(&format!(" — {name}"));
    match app_id_of(program) {
        Ok((id, title)) if id == want && one_name(&title) => true,
        Ok((id, title)) => {
            eprintln!(
                "apps_test: {program} named its window {id:?}, titled {title:?}, not {want:?} and {name:?}"
            );
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

fn the_system_monitor_runs() -> bool {
    run(APPS[3].0, APPS[3].1)
}

fn the_terminal_runs() -> bool {
    run(APPS[4].0, APPS[4].1)
}

fn breach_runs() -> bool {
    run(APPS[6].0, APPS[6].1)
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("the_editor_runs", the_editor_runs),
    ("the_file_manager_runs", the_file_manager_runs),
    ("the_image_viewer_runs", the_image_viewer_runs),
    ("the_system_monitor_runs", the_system_monitor_runs),
    ("the_terminal_runs", the_terminal_runs),
    ("breach_runs", breach_runs),
    (
        "a_refusal_names_the_cause_the_kernel_reports",
        a_refusal_names_the_cause_the_kernel_reports,
    ),
];

/// What the session may not change is told apart by what the kernel says of
/// the file: the base's, a sealed one, and one its permissions refuse; and a
/// folder it may not read, by its permissions.
fn a_refusal_names_the_cause_the_kernel_reports() -> bool {
    use slopos_appkit::Refusal;
    let changes = [
        ("/bin/shell", Refusal::System),
        ("/etc/passwd", Refusal::Sealed),
        ("/var/lib/slopos/apps_test_probe", Refusal::Denied),
    ];
    for (path, cause) in changes {
        let wrote = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path);
        match wrote {
            Err(e) if Refusal::is(e.kind()) => {}
            other => return fail(format!("a write to {path}: {other:?}")),
        }
        let said = Refusal::changing(Path::new(path));
        if said != cause {
            return fail(format!(
                "changing {path} is refused as {said:?}, want {cause:?}"
            ));
        }
    }
    match fs::read_dir("/root") {
        Err(e) if Refusal::is(e.kind()) => {}
        other => return fail(format!("listing /root: {:?}", other.map(drop))),
    }
    let said = Refusal::reading(Path::new("/root"));
    said == Refusal::Denied || fail(format!("listing /root is refused as {said:?}"))
}

fn fail(why: impl std::fmt::Display) -> bool {
    eprintln!("apps_test: {why}");
    false
}

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
