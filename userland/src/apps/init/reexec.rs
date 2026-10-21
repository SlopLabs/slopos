//! Init re-executing itself: what it knows goes into a pipe whose read end,
//! like the descriptors that state names, survives the exec, and the new
//! image reads it back. systemd's `daemon-reexec` carries its state across the
//! same way, in a file descriptor. Extensions' services go in as many more
//! pipes as they fill, named after it, for an image that reads them.

use std::ffi::CString;

use slopos_abi::syscall::{F_GETFD, F_SETFD, FD_CLOEXEC, O_CLOEXEC, O_NONBLOCK};
use slopos_slibc::pal::{Pal, Sys};

use slopos_priv_core::sessions::Class;

use super::extensions::{CARRIED, Carried as Units};
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, process};

pub const RESUME: &str = "--resume";
/// The state's first line after the descriptors, which an image's text holds
/// when it reads that state: the session table's version, and the one before.
const VERSION: &str = "slopos-init 2";
const VERSION_1: &str = "slopos-init 1";
const ARGV0: &str = "/sbin/init";

/// What init carries across an exec. A service's pid is `None` while it is
/// not running; its starts are the times its budget counts.
#[derive(Default, Debug)]
pub struct State {
    /// Each consent link's privd and compositor ends.
    pub links: Vec<(RawFd, RawFd)>,
    /// Init's and privd's ends of the pair init tells privd the sessions on.
    pub sessions_link: Option<(RawFd, RawFd)>,
    pub control: Option<RawFd>,
    /// The request to re-execute, which the new image answers.
    pub reply: Option<RawFd>,
    pub privd: Service,
    pub remoted: Option<Service>,
    pub login: Option<Service>,
    pub sessions: Vec<Carried>,
    pub front: Option<u32>,
    pub next_tag: u32,
    pub held: bool,
    pub wanted: bool,
    pub fresh: bool,
    pub default_starts: Vec<u64>,
    pub tests: Option<u32>,
    /// A first-version image's one desktop: its compositor's pid while it
    /// ran, which this image takes over as the first person's.
    pub legacy: Option<Service>,
}

#[derive(Default, Debug)]
pub struct Service {
    pub pid: Option<u32>,
    pub starts: Vec<u64>,
}

/// A session as a re-execution carries it.
#[derive(Debug)]
pub struct Carried {
    pub tag: u32,
    pub uid: u32,
    pub class: Class,
    pub leader: Option<u32>,
    pub link: Option<u8>,
    pub locked: bool,
    /// The pipe init tells its compositor whether it is in front on.
    pub pipe: Option<RawFd>,
    /// A console session's terminal, which `/bin/login` relays to while
    /// `/run/sessions` names it.
    pub tty: Option<String>,
    pub starts: Vec<u64>,
}

impl State {
    fn descriptors(&self) -> impl Iterator<Item = RawFd> + use<> {
        let links: Vec<RawFd> = self.links.iter().flat_map(|&(a, b)| [a, b]).collect();
        let pipes: Vec<RawFd> = self.sessions.iter().filter_map(|s| s.pipe).collect();
        links
            .into_iter()
            .chain(self.sessions_link.into_iter().flat_map(|(a, b)| [a, b]))
            .chain(self.control)
            .chain(self.reply)
            .chain(pipes)
    }

    /// The first line names the descriptors kept, in a form every version
    /// reads, so an image that cannot read the rest still closes them.
    fn encode(&self) -> String {
        let mut out = kept_line(self.descriptors());
        out += &format!("{VERSION}\n");
        if !self.links.is_empty() {
            let fds: Vec<String> = self
                .links
                .iter()
                .flat_map(|&(a, b)| [a.to_string(), b.to_string()])
                .collect();
            out += &format!("links {}\n", fds.join(" "));
        }
        if let Some((init, privd)) = self.sessions_link {
            out += &format!("sessions-link {init} {privd}\n");
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
        if let Some(login) = &self.login {
            out += &service("login", login.pid, &login.starts);
        }
        out += &service("default", None, &self.default_starts);
        for s in &self.sessions {
            let mut line = format!(
                "session {} {} {} {} {} {} {} {}",
                s.tag,
                s.uid,
                s.class.word(),
                s.leader.unwrap_or(0),
                s.link.map_or("-".to_owned(), |l| l.to_string()),
                u8::from(s.locked),
                s.pipe.map_or("-".to_owned(), |p| p.to_string()),
                s.tty.as_deref().filter(|tty| carries(tty)).unwrap_or("-"),
            );
            for start in &s.starts {
                line += &format!(" {start}");
            }
            out += &line;
            out.push('\n');
        }
        if let Some(front) = self.front {
            out += &format!("front {front}\n");
        }
        out += &format!("next {}\n", self.next_tag);
        for (flag, word) in [
            (self.held, "held"),
            (self.wanted, "wanted"),
            (self.fresh, "fresh"),
        ] {
            if flag {
                out += &format!("{word}\n");
            }
        }
        if let Some(tests) = self.tests {
            out += &format!("tests {tests}\n");
        }
        out
    }

    /// The state for an image of the first version, which knows one desktop
    /// and one consent link: what it carries of the services, and none of
    /// the sessions, which end before it runs.
    fn encode_first(&self) -> String {
        let consent = self.links.first().copied();
        let kept = consent
            .into_iter()
            .flat_map(|(a, b)| [a, b])
            .chain(self.control)
            .chain(self.reply);
        let mut out = kept_line(kept);
        out += &format!("{VERSION_1}\n");
        if let Some((privd, agent)) = consent {
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
        out += &service("session", None, &self.default_starts);
        if self.held {
            out += "held\n";
        }
        if let Some(tests) = self.tests {
            out += &format!("tests {tests}\n");
        }
        out
    }

    fn decode<'a>(mut lines: impl Iterator<Item = &'a str>) -> Option<State> {
        let first = match lines.next()? {
            VERSION => false,
            VERSION_1 => true,
            _ => return None,
        };
        let mut state = State {
            wanted: first,
            ..State::default()
        };
        for line in lines {
            let mut words = line.split(' ');
            let key = words.next()?;
            let rest: Vec<&str> = words.collect();
            if key == "session" && !first {
                state.sessions.push(carried_session(&rest)?);
                continue;
            }
            let numbers: Option<Vec<u64>> = rest.iter().map(|w| w.parse().ok()).collect();
            let numbers = numbers?;
            let fd = |i: usize| numbers.get(i).and_then(|&n| RawFd::try_from(n).ok());
            match key {
                "consent" if first => state.links = vec![(fd(0)?, fd(1)?)],
                "links" if !first && numbers.len() % 2 == 0 => {
                    state.links = numbers
                        .chunks(2)
                        .map(|pair| {
                            Some((
                                RawFd::try_from(pair[0]).ok()?,
                                RawFd::try_from(pair[1]).ok()?,
                            ))
                        })
                        .collect::<Option<_>>()?;
                }
                "sessions-link" => state.sessions_link = Some((fd(0)?, fd(1)?)),
                "control" => state.control = Some(fd(0)?),
                "reply" => state.reply = Some(fd(0)?),
                "privd" => state.privd = Service::decode(&numbers)?,
                "remoted" => state.remoted = Some(Service::decode(&numbers)?),
                "login" => state.login = Some(Service::decode(&numbers)?),
                "default" => state.default_starts = Service::decode(&numbers)?.starts,
                "session" => {
                    let session = Service::decode(&numbers)?;
                    state.default_starts = session.starts.clone();
                    state.legacy = Some(session);
                }
                "front" => state.front = Some(u32::try_from(*numbers.first()?).ok()?),
                "next" => state.next_tag = u32::try_from(*numbers.first()?).ok()?,
                "tests" => state.tests = Some(u32::try_from(*numbers.first()?).ok()?),
                "held" if numbers.is_empty() => state.held = true,
                "wanted" if numbers.is_empty() => state.wanted = true,
                "fresh" if numbers.is_empty() => state.fresh = true,
                _ => return None,
            }
        }
        Some(state)
    }
}

fn kept_line(fds: impl Iterator<Item = RawFd>) -> String {
    std::iter::once("kept".to_owned())
        .chain(fds.map(|fd| fd.to_string()))
        .collect::<Vec<_>>()
        .join(" ")
        + "\n"
}

/// A terminal name a session line can carry as one word.
fn carries(tty: &str) -> bool {
    tty != "-" && !tty.is_empty() && !tty.contains(char::is_whitespace)
}

fn carried_session(words: &[&str]) -> Option<Carried> {
    let [
        tag,
        uid,
        class,
        leader,
        link,
        locked,
        pipe,
        tty,
        starts @ ..,
    ] = words
    else {
        return None;
    };
    let optional = |word: &str| -> Option<Option<u64>> {
        match word {
            "-" => Some(None),
            n => n.parse().ok().map(Some),
        }
    };
    Some(Carried {
        tag: tag.parse().ok()?,
        uid: uid.parse().ok()?,
        class: Class::parse(class)?,
        leader: match leader.parse::<u32>().ok()? {
            0 => None,
            pid => Some(pid),
        },
        link: optional(link)?.map(u8::try_from).transpose().ok()?,
        locked: match *locked {
            "0" => false,
            "1" => true,
            _ => return None,
        },
        pipe: optional(pipe)?.map(RawFd::try_from).transpose().ok()?,
        tty: (*tty != "-").then(|| (*tty).to_owned()),
        starts: starts
            .iter()
            .map(|w| w.parse().ok())
            .collect::<Option<_>>()?,
    })
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

/// What the image at `/sbin/init` reads of what a re-execution hands on.
pub struct Reads {
    /// The extensions' services, its text holding the header they are
    /// written under.
    pub units: bool,
    /// The session table, its text holding this version's header.
    pub sessions: bool,
}

/// What the image at `/sbin/init` reads; an error when it cannot be read.
pub fn reads() -> Result<Reads, String> {
    let image = std::fs::read(ARGV0).map_err(|e| format!("{ARGV0}: {e}"))?;
    let holds = |header: &str| image.windows(header.len()).any(|w| w == header.as_bytes());
    Ok(Reads {
        units: holds(CARRIED),
        sessions: holds(VERSION),
    })
}

/// Execute `/sbin/init` again carrying `state`, in the version the image
/// reads, and, for an image that reads them, `units`. Returns only when the
/// exec failed, with every descriptor as it was.
pub fn reexec(state: &State, sessions: bool, units: Option<&Units>) -> String {
    let encoded = match sessions {
        true => state.encode(),
        false => state.encode_first(),
    };
    let pipe = match handed(&encoded) {
        Ok(pipe) => pipe,
        Err(why) => return why,
    };
    let units_pipes = match units.map(|units| handed_lines(&units.encode())).transpose() {
        Ok(units_pipes) => units_pipes.unwrap_or_default(),
        Err(why) => return why,
    };
    let carried: Vec<RawFd> = match sessions {
        true => state.descriptors().collect(),
        false => state
            .links
            .first()
            .into_iter()
            .flat_map(|&(a, b)| [a, b])
            .chain(state.control)
            .chain(state.reply)
            .collect(),
    };
    let kept: Vec<RawFd> = carried
        .into_iter()
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
pub fn resume_units(fds: Option<&str>) -> Units {
    let Some(fds) = fds else {
        return Units::default();
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
        Units::decode(&text)
    } else {
        Err(Units::kept(&text))
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
            Units::default()
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
