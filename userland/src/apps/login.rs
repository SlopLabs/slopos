//! `/bin/login` — the console's prompt, which init runs on its terminal as
//! the login screen's user. A person who names themselves and types their
//! password gets a shell of their own, a session init keeps, on a terminal
//! this program makes and relays the console to while init lists the
//! session: a console the kernel hangs up when one session's leader exits
//! serves the next login only through a terminal of each login's own, and
//! closing it once the session ends hangs up whatever of it still holds the
//! terminal, so nothing it left can pose as this prompt.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use slopos_abi::syscall::{POLLHUP, POLLIN, TIOCSPTLCK, UserPollFd};
use slopos_slibc::pal::{Pal, Sys};

use crate::privd::run_feeding;
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, process};

const LOGIN: &str = "org.slopos.login";

pub fn login_main() {
    loop {
        let Some(name) = prompt_name() else {
            std::process::exit(0);
        };
        if name.is_empty() {
            continue;
        }
        let secret = match crate::secret::read("Password: ") {
            Ok(Some(secret)) => secret,
            Ok(None) => std::process::exit(0),
            Err(why) => {
                eprintln!("login: {why}");
                continue;
            }
        };
        let (master, slave) = match terminal() {
            Ok(pair) => pair,
            Err(why) => {
                eprintln!("login: {why}");
                continue;
            }
        };
        let mut line = secret.bytes().to_vec();
        line.push(b'\n');
        let tty = crate::syscall::tty::name_of(slave.raw());
        let ran = run_feeding(LOGIN, &["login", "console", &name], &line, slave.raw());
        slopos_tls_core::ct::wipe(&mut line);
        drop((secret, slave));
        match (ran, tty) {
            (Ok(0), Some(tty)) => relay(master, &tty),
            (Ok(0), None) => println!("login: the shell's terminal has no name"),
            (Ok(_), _) => println!("Login incorrect"),
            (Err(why), _) => println!("login: {why}"),
        }
    }
}

fn prompt_name() -> Option<String> {
    print!("\nslopos login: ");
    let _ = std::io::stdout().flush();
    let mut name = String::new();
    match std::io::stdin().lock().read_line(&mut name) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(name.trim().to_owned()),
    }
}

/// A terminal for the shell: its master here, its slave locked against
/// opens by its path, so nothing but the descriptor init hands on reaches it.
fn terminal() -> Result<(OwnedFd, OwnedFd), String> {
    let fail = |why: String| format!("no terminal for the shell: {why}");
    let (master, _) =
        process::openpty().map_err(|e| fail(SyscallError::from_errno((-e) as i32).to_string()))?;
    let slave = fs::ioctl_tiocgptpeer(master.raw()).map_err(|e| fail(e.to_string()))?;
    let mut locked: i32 = 1;
    Sys::ioctl(master.raw(), TIOCSPTLCK, (&mut locked as *mut i32) as u64)
        .map_err(|e| fail(SyscallError::from(e).to_string()))?;
    if let Ok(size) = fs::tiocgwinsz(0) {
        let _ = fs::tiocswinsz(master.raw(), &size);
    }
    Ok((master, slave))
}

/// How long init has to list the session it was asked for.
const LISTED_WITHIN: Duration = Duration::from_secs(10);
const TICK_MS: i64 = 500;

/// Relay the console to `master` while init lists a session on `tty`, the
/// console raw meanwhile so every byte reaches the shell's own terminal.
fn relay(master: OwnedFd, tty: &str) {
    let raw = Raw::enter();
    let mut buf = [0u8; 1024];
    let since = Instant::now();
    let mut seen = false;
    loop {
        let listed = crate::sessions::read()
            .iter()
            .any(|s| s.tty.as_deref() == Some(tty));
        seen |= listed;
        if !listed && (seen || since.elapsed() > LISTED_WITHIN) {
            break;
        }
        let mut fds = [poll(0), poll(master.raw())];
        match fs::poll(&mut fds, TICK_MS) {
            Ok(_) => {}
            Err(SyscallError::EINTR) => continue,
            Err(_) => break,
        }
        if fds[1].revents & POLLIN != 0 {
            match fs::read_slice(master.raw(), &mut buf) {
                Ok(n) if n > 0 => write_all(1, &buf[..n]),
                Err(SyscallError::EINTR | SyscallError::EAGAIN) => {}
                _ => break,
            }
        } else if fds[1].revents & POLLHUP != 0 {
            break;
        }
        if fds[0].revents & POLLIN != 0 {
            match fs::read_slice(0, &mut buf) {
                Ok(n) if n > 0 => write_all(master.raw(), &buf[..n]),
                Err(SyscallError::EINTR | SyscallError::EAGAIN) => {}
                _ => break,
            }
        }
    }
    drop((raw, master));
}

fn poll(fd: RawFd) -> UserPollFd {
    UserPollFd {
        fd,
        events: POLLIN,
        revents: 0,
    }
}

fn write_all(fd: RawFd, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match fs::write_slice(fd, bytes) {
            Ok(n) if n > 0 => bytes = &bytes[n..],
            Err(SyscallError::EINTR) => {}
            _ => return,
        }
    }
}

/// The console raw, as it was again once dropped.
struct Raw(Option<slopos_abi::syscall::UserTermios>);

impl Raw {
    fn enter() -> Raw {
        let Ok(saved) = fs::tcgetattr(0) else {
            return Raw(None);
        };
        let mut raw = saved;
        // SAFETY: `raw` is a termios this function owns, which the call only
        // rewrites in place.
        unsafe { slopos_slibc::tty::cfmakeraw(&mut raw) };
        let _ = fs::tcsetattr(0, &raw);
        Raw(Some(saved))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            let _ = fs::tcsetattr(0, saved);
        }
    }
}
