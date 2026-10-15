//! Init re-executing itself: what it knows goes into a pipe whose read end,
//! like the descriptors that state names, survives the exec, and the new
//! image reads it back. systemd's `daemon-reexec` carries its state across the
//! same way, in a file descriptor.

use std::ffi::CString;

use slopos_abi::syscall::{F_GETFD, F_SETFD, FD_CLOEXEC, O_CLOEXEC, O_NONBLOCK};
use slopos_slibc::pal::{Pal, Sys};

use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, process};

pub const RESUME: &str = "--resume";
const VERSION: &str = "slopos-init 1";
const ARGV0: &str = "/sbin/init";

/// What init carries across an exec. A service's pid is `None` while it is
/// not running; its starts are the times its budget counts.
#[derive(Default, Debug)]
pub struct State {
    pub consent: Option<(RawFd, RawFd)>,
    pub control: Option<RawFd>,
    /// The request to re-execute, which the new image answers.
    pub reply: Option<RawFd>,
    pub privd: Service,
    pub remoted: Option<Service>,
    /// The compositor's pid while the session runs.
    pub session: Option<u32>,
    pub session_held: bool,
    pub session_starts: Vec<u64>,
    pub tests: Option<u32>,
}

#[derive(Default, Debug)]
pub struct Service {
    pub pid: Option<u32>,
    pub starts: Vec<u64>,
}

impl State {
    fn descriptors(&self) -> impl Iterator<Item = RawFd> + use<> {
        self.consent
            .into_iter()
            .flat_map(|(privd, agent)| [privd, agent])
            .chain(self.control)
            .chain(self.reply)
    }

    /// The first line names the descriptors kept, in a form every version
    /// reads, so an image that cannot read the rest still closes them.
    fn encode(&self) -> String {
        let mut out: String = std::iter::once("kept".to_owned())
            .chain(self.descriptors().map(|fd| fd.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        out += &format!("\n{VERSION}\n");
        if let Some((privd, agent)) = self.consent {
            out += &format!("consent {privd} {agent}\n");
        }
        if let Some(control) = self.control {
            out += &format!("control {control}\n");
        }
        if let Some(reply) = self.reply {
            out += &format!("reply {reply}\n");
        }
        out += &service("privd", self.privd.pid, &self.privd.starts);
        if let Some(remoted) = &self.remoted {
            out += &service("remoted", remoted.pid, &remoted.starts);
        }
        out += &service("session", self.session, &self.session_starts);
        if self.session_held {
            out += "held\n";
        }
        if let Some(tests) = self.tests {
            out += &format!("tests {tests}\n");
        }
        out
    }

    fn decode<'a>(mut lines: impl Iterator<Item = &'a str>) -> Option<State> {
        if lines.next() != Some(VERSION) {
            return None;
        }
        let mut state = State::default();
        for line in lines {
            let mut words = line.split(' ');
            let key = words.next()?;
            let numbers: Option<Vec<u64>> = words.map(|w| w.parse().ok()).collect();
            let numbers = numbers?;
            let fd = |i: usize| numbers.get(i).and_then(|&n| RawFd::try_from(n).ok());
            match key {
                "consent" => state.consent = Some((fd(0)?, fd(1)?)),
                "control" => state.control = Some(fd(0)?),
                "reply" => state.reply = Some(fd(0)?),
                "privd" => state.privd = Service::decode(&numbers)?,
                "remoted" => state.remoted = Some(Service::decode(&numbers)?),
                "session" => {
                    let session = Service::decode(&numbers)?;
                    state.session = session.pid;
                    state.session_starts = session.starts;
                }
                "tests" => state.tests = Some(u32::try_from(*numbers.first()?).ok()?),
                "held" if numbers.is_empty() => state.session_held = true,
                _ => return None,
            }
        }
        Some(state)
    }
}

impl Service {
    fn decode(numbers: &[u64]) -> Option<Service> {
        let (&pid, starts) = numbers.split_first()?;
        Some(Service {
            pid: (pid != 0).then_some(u32::try_from(pid).ok()?),
            starts: starts.to_vec(),
        })
    }
}

fn service(key: &str, pid: Option<u32>, starts: &[u64]) -> String {
    let mut line = format!("{key} {}", pid.unwrap_or(0));
    for start in starts {
        line += &format!(" {start}");
    }
    line + "\n"
}

/// Execute `/sbin/init` again carrying `state`. Returns only when the exec
/// failed, with every descriptor as it was.
pub fn reexec(state: &State) -> String {
    let text = state.encode();
    // Non-blocking, so a state larger than the pipe holds is refused rather
    // than waiting on a reader that is this process.
    let (pipe, writer) = match fs::pipe2((O_CLOEXEC | O_NONBLOCK) as u32) {
        Ok(ends) => ends,
        Err(e) => return format!("pipe: {e}"),
    };
    if fs::write_slice(writer.raw(), text.as_bytes()) != Ok(text.len()) {
        return "the state does not fit a pipe".into();
    }
    drop(writer);
    let kept: Vec<RawFd> = state.descriptors().chain([pipe.raw()]).collect();
    for &fd in &kept {
        set_cloexec(fd, false);
    }
    let fd = pipe.raw().to_string();
    let argv: Vec<CString> = [ARGV0, RESUME, &fd]
        .into_iter()
        .map(|arg| CString::new(arg).expect("no NUL in init's argv"))
        .collect();
    let mut argv_ptrs: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
    argv_ptrs.push(core::ptr::null());
    let envp: [*const u8; 1] = [core::ptr::null()];
    let rc = process::execve(
        c"/sbin/init".as_ptr().cast(),
        argv_ptrs.as_ptr(),
        envp.as_ptr(),
    );
    for &fd in &kept {
        set_cloexec(fd, true);
    }
    format!("execve answered {rc}")
}

/// The state an earlier image left in the pipe at `fd`, closing it, or the
/// descriptors it kept when the rest cannot be read.
pub fn resume(fd: &str) -> Result<State, Vec<RawFd>> {
    let fd: RawFd = fd.parse().map_err(|_| Vec::new())?;
    // SAFETY: the earlier image passed this descriptor for us alone.
    let pipe = unsafe { OwnedFd::from_raw(fd) };
    let mut text = Vec::new();
    let mut buf = [0u8; 512];
    let whole = loop {
        match fs::read_slice(pipe.raw(), &mut buf) {
            Ok(0) => break true,
            Ok(n) => text.extend_from_slice(&buf[..n]),
            Err(SyscallError::EINTR) => {}
            Err(_) => break false,
        }
    };
    let text = String::from_utf8_lossy(&text);
    let mut lines = text.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let kept: Vec<RawFd> = match first.next() {
        Some("kept") => first.filter_map(|fd| fd.parse().ok()).collect(),
        _ => Vec::new(),
    };
    let state = whole.then(|| State::decode(lines)).flatten().ok_or(kept)?;
    for fd in state.descriptors() {
        set_cloexec(fd, true);
    }
    Ok(state)
}

fn set_cloexec(fd: RawFd, on: bool) {
    let Ok(flags) = Sys::fcntl(fd, F_GETFD as i32, 0) else {
        return;
    };
    let flags = if on {
        flags as u64 | FD_CLOEXEC
    } else {
        flags as u64 & !FD_CLOEXEC
    };
    let _ = Sys::fcntl(fd, F_SETFD as i32, flags);
}
