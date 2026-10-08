//! The userland half of `just test-usb`: a shell on the console, and the
//! command the host types at it on a USB keyboard.

use slopos_userland as _;

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use slopos_abi::syscall::termios::TCIFLUSH;
use slopos_slibc::test_harness::note;
use slopos_userland::syscall::tty;

/// The status the typed command exits the shell with.
const TYPED_STATUS: i32 = 7;
const HOST_WAIT: Duration = Duration::from_secs(300);
/// The line editor turns bracketed paste on once the console is raw, after
/// which nothing typed is lost to the mode switch.
const EDITING: &[u8] = b"\x1b[?2004h";

fn wait(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + HOST_WAIT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let _ = child.kill();
    None
}

/// The shell reads the console; its output comes back here, so the host is
/// asked to type only once the line editor reads. Keys the kernel tests
/// pressed are still queued on the console and are dropped first.
fn types_into_the_shell() -> bool {
    slopos_slibc::tty::tcflush(0, TCIFLUSH);
    let Ok(mut shell) = Command::new("/bin/shell").stdout(Stdio::piped()).spawn() else {
        note("no shell");
        return false;
    };
    let Some(mut output) = shell.stdout.take() else {
        note("no shell output");
        return false;
    };
    let (editing, ready) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut chunk = [0u8; 256];
        let mut told = false;
        while let Ok(n) = output.read(&mut chunk) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&chunk[..n]);
            if !told && seen.windows(EDITING.len()).any(|w| w == EDITING) {
                told = editing.send(()).is_ok();
            }
        }
    });
    if ready.recv_timeout(HOST_WAIT).is_err() {
        note("the shell never read a line");
        let _ = shell.kill();
        return false;
    }
    tty::write(format!("USB-TEST: type exit {TYPED_STATUS}\n").as_bytes());
    let status = wait(&mut shell);
    if status != Some(TYPED_STATUS) {
        note(&format!("the shell exited {status:?}"));
        return false;
    }
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[("types_into_the_shell", types_into_the_shell)]);
}
