//! Init re-executing itself: what it knows goes into a pipe whose read end,
//! like the descriptors that state names, survives the exec, and the new
//! image reads it back. systemd's `daemon-reexec` carries its state across the
//! same way, in a file descriptor. Extensions' services go in as many more
//! pipes as they fill, named after it, for an image that reads them.

use std::ffi::CString;

use slopos_abi::syscall::{F_GETFD, F_SETFD, FD_CLOEXEC, O_CLOEXEC, O_NONBLOCK};
use slopos_slibc::pal::{Pal, Sys};

use super::extensions::{CARRIED, Carried};
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

/// The read end of a pipe holding `text`; a state larger than a pipe holds
/// is refused rather than waiting on a reader that is this process.
fn handed(text: &str) -> Result<OwnedFd, String> {
    let (pipe, writer) =
        fs::pipe2((O_CLOEXEC | O_NONBLOCK) as u32).map_err(|e| format!("pipe: {e}"))?;
    if fs::write_slice(writer.raw(), text.as_bytes()) != Ok(text.len()) {
        return Err("the state does not fit a pipe".into());
    }
    Ok(pipe)
}

/// What an empty pipe takes in one write while nobody reads it, the kernel's
/// pipe buffer.
const PIPE_CAPACITY: usize = 4096;

/// Pipes holding `text` in order, each as many whole lines as it takes.
fn handed_lines(text: &str) -> Result<Vec<OwnedFd>, String> {
    let mut pipes = Vec::new();
    let mut chunk = String::new();
    for line in text.split_inclusive('\n') {
        if !chunk.is_empty() && chunk.len() + line.len() > PIPE_CAPACITY {
            pipes.push(handed(&chunk)?);
            chunk.clear();
        }
        chunk.push_str(line);
    }
    if !chunk.is_empty() || pipes.is_empty() {
        pipes.push(handed(&chunk)?);
    }
    Ok(pipes)
}

/// Whether the image at `/sbin/init` reads the services a re-execution hands
/// on, its text holding the header they are written under; an error when it
/// cannot be read.
pub fn reads_units() -> Result<bool, String> {
    let header = CARRIED.as_bytes();
    let image = std::fs::read(ARGV0).map_err(|e| format!("{ARGV0}: {e}"))?;
    Ok(image.windows(header.len()).any(|w| w == header))
}

/// Execute `/sbin/init` again carrying `state` and, for an image that reads
/// them, `units`. Returns only when the exec failed, with every descriptor
/// as it was.
pub fn reexec(state: &State, units: Option<&Carried>) -> String {
    let pipe = match handed(&state.encode()) {
        Ok(pipe) => pipe,
        Err(why) => return why,
    };
    let units_pipes = match units.map(|units| handed_lines(&units.encode())).transpose() {
        Ok(units_pipes) => units_pipes.unwrap_or_default(),
        Err(why) => return why,
    };
    let kept: Vec<RawFd> = state
        .descriptors()
        .chain(units.iter().flat_map(|units| units.descriptors()))
        .chain([pipe.raw()])
        .chain(units_pipes.iter().map(OwnedFd::raw))
        .collect();
    for &fd in &kept {
        set_cloexec(fd, false);
    }
    let fd = pipe.raw().to_string();
    let units_fd = (!units_pipes.is_empty()).then(|| {
        let fds: Vec<String> = units_pipes.iter().map(|fd| fd.raw().to_string()).collect();
        fds.join(",")
    });
    let argv: Vec<CString> = [ARGV0, RESUME, &fd]
        .into_iter()
        .chain(units_fd.as_deref())
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

/// What the pipe at `fd` holds, closing it, and whether that is all of it.
fn read_handed(fd: &str) -> Option<(String, bool)> {
    let fd: RawFd = fd.parse().ok()?;
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
    Some((String::from_utf8_lossy(&text).into_owned(), whole))
}

/// The state an earlier image left in the pipe at `fd`, closing it, or the
/// descriptors it kept when the rest cannot be read.
pub fn resume(fd: &str) -> Result<State, Vec<RawFd>> {
    let (text, whole) = read_handed(fd).ok_or_else(Vec::new)?;
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

/// The extensions' services the pipes `fds` names, `,` between, hold in
/// turn, each descriptor they name closed when the rest cannot be read.
pub fn resume_units(fds: Option<&str>) -> Carried {
    let Some(fds) = fds else {
        return Carried::default();
    };
    let (mut text, mut whole) = (String::new(), true);
    for fd in fds.split(',') {
        match read_handed(fd) {
            Some((part, all)) => {
                text.push_str(&part);
                whole &= all;
            }
            None => whole = false,
        }
    }
    let decoded = if whole {
        Carried::decode(&text)
    } else {
        Err(Carried::kept(&text))
    };
    match decoded {
        Ok(units) => {
            for fd in units.descriptors() {
                set_cloexec(fd, true);
            }
            units
        }
        Err(kept) => {
            for fd in kept {
                crate::syscall::memory::close(fd);
            }
            Carried::default()
        }
    }
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
