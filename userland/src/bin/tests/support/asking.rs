//! A request to privd from a child of the test, at the uid, login tag and
//! directory the test names, which says on its stdout how it went: `asked by
//! <app>` once the question is on screen, naming the app the dialog does,
//! then `ran <code>` or `denied[ by the person] <reason>`. With a desktop up
//! the test answers the question with keys the kernel delivers to the
//! compositor as a keyboard's, which only a test boot takes. Each test binary
//! includes it and uses what it needs.

#![allow(dead_code)]

use std::ffi::CString;
use std::io::Write;
use std::time::{Duration, Instant};

use slopos_abi::input::keycode::{KEY_ENTER, KEY_ESC, KEY_TAB};
use slopos_abi::syscall::{O_CLOEXEC, POLLIN, UserPollFd};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::consent::{ARMING_MS, UNIDENTIFIED};
use slopos_priv_core::wire::{Exit, Reply, Request};
use slopos_userland::privd::{Connection, request_id};
use slopos_userland::syscall::core::test_key;
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::{OwnedFd, fs, net};

/// `argv[1]` of the child, then whether to send its directory, the action
/// and its arguments.
pub const REQUEST: &str = "--request";

/// A child of the test binary at `path`, its stdout in a pipe read line by
/// line and its stdin one that ends it; killed once dropped.
pub struct Child {
    pid: u32,
    out: OwnedFd,
    stdin: OwnedFd,
    seen: Vec<u8>,
    reaped: bool,
}

impl Child {
    pub fn start(
        path: &str,
        uid: u32,
        login: Option<u32>,
        cwd: &str,
        args: &[&str],
    ) -> Option<Child> {
        let (out, out_end) = fs::pipe2(O_CLOEXEC as u32).ok()?;
        let (stdin, stdin_end) = fs::pipe2(O_CLOEXEC as u32).ok()?;
        let name = path.rsplit('/').next().unwrap_or(path);
        let argv: Vec<CString> = std::iter::once(name)
            .chain(args.iter().copied())
            .map(|arg| CString::new(arg).expect("no NUL in an argument"))
            .collect();
        let argv: Vec<*const u8> = argv.iter().map(|a| a.as_ptr().cast()).collect();
        let actions = [
            process::clone_fd(stdin.raw(), 0),
            process::clone_fd(out_end.raw(), 1),
            process::clone_fd(2, 2),
        ];
        let pid = process::spawn_endowed(
            path.as_bytes(),
            &argv,
            &[],
            cwd.as_bytes(),
            TaskPriority::Normal,
            TASK_FLAG_USER_MODE,
            &actions,
            0,
            Endowment {
                caps: 0,
                ids: Some((uid, uid)),
                groups: Some(&[]),
                login,
            },
        );
        if pid <= 0 {
            eprintln!("{name} {args:?} as uid {uid} did not start: {pid}");
            return None;
        }
        let _ = net::set_nonblocking(out.raw());
        Some(Child {
            pid: pid as u32,
            out,
            stdin: stdin_end,
            seen: Vec::new(),
            reaped: false,
        })
    }

    pub fn tell(&self, line: &str) -> bool {
        let line = format!("{line}\n");
        fs::write_slice(self.stdin.raw(), line.as_bytes()).is_ok_and(|n| n == line.len())
    }

    /// The next line it says, `None` once `within` has passed or it ended.
    pub fn line(&mut self, within: Duration) -> Option<String> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(end) = self.seen.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.seen.drain(..=end).collect();
                return Some(String::from_utf8_lossy(&line[..end]).into_owned());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let mut fds = [UserPollFd {
                fd: self.out.raw(),
                events: POLLIN,
                revents: 0,
            }];
            let _ = fs::poll(&mut fds, left.as_millis() as i64);
            let mut buf = [0u8; 256];
            match fs::read_slice(self.out.raw(), &mut buf) {
                Ok(0) => return None,
                Ok(n) => self.seen.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
        }
    }
}

impl Child {
    /// Its exit status once it exits within `within`; `None` past it.
    pub fn exited_within(mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(code) = process::wait_exit_code_nohang(self.pid) {
                self.reaped = true;
                return Some(code);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = process::kill(self.pid, slopos_abi::signal::SIGKILL);
            let _ = process::wait_exit_code(self.pid);
        }
    }
}

const ASKED_BY: &str = "asked by ";

/// How long a request may take to be asked, answered or refused.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// Who asks: a child of the test binary at `path`, as `uid` carrying the
/// login tag `login`, in `cwd`, which it sends when `with_cwd`.
pub struct Asker<'a> {
    pub path: &'a str,
    pub uid: u32,
    pub login: Option<u32>,
    pub cwd: &'a str,
    pub with_cwd: bool,
}

impl Asker<'_> {
    /// What asking for `request`, the action and then its arguments, ended
    /// with: `answer` is run once its question is on screen, and without one
    /// the request is withdrawn there and `asked` is what it ended with.
    pub fn ask(&self, request: &[&str], answer: Option<fn()>) -> String {
        self.ask_naming(request, answer).0
    }

    /// [`ask`](Self::ask), and the app the dialog named once it asked.
    pub fn ask_naming(&self, request: &[&str], answer: Option<fn()>) -> (String, Option<String>) {
        let mut args = vec![REQUEST, if self.with_cwd { "cwd" } else { "-" }];
        args.extend_from_slice(request);
        let Some(mut child) = Child::start(self.path, self.uid, self.login, self.cwd, &args) else {
            return ("did not start".into(), None);
        };
        let said = child.line(DEADLINE).unwrap_or_default();
        let Some(app) = said.strip_prefix(ASKED_BY).map(str::to_owned) else {
            return (said, None);
        };
        match answer {
            Some(answer) => {
                answer();
                (child.line(DEADLINE).unwrap_or_default(), Some(app))
            }
            None => ("asked".into(), Some(app)),
        }
    }
}

/// The child's half: `args` past [`REQUEST`].
pub fn request_child(args: &[String]) -> i32 {
    let [cwd, action, rest @ ..] = args else {
        return 1;
    };
    let Ok(mut privd) = Connection::open() else {
        return 1;
    };
    let null = |path: &core::ffi::CStr, flags| fs::open_cstr(path, flags).ok();
    let (Some(stdin), Some(dir)) = (
        null(c"/dev/null", slopos_abi::fs::O_RDWR),
        null(c".", slopos_abi::fs::O_RDONLY | slopos_abi::fs::O_DIRECTORY),
    ) else {
        return 1;
    };
    let request = Request {
        id: request_id(),
        action: action.clone(),
        args: rest.iter().map(|a| a.as_bytes().to_vec()).collect(),
        cwd: cwd == "cwd",
        terminal: false,
    };
    let mut fds = vec![stdin.raw(), stdin.raw(), 2];
    if request.cwd {
        fds.push(dir.raw());
    }
    if privd.send(&request, &fds).is_err() {
        return 1;
    }
    let mut out = std::io::stdout();
    loop {
        let said = match privd.reply(request.id) {
            Ok(Some(Reply::Asking { app })) => {
                format!("{ASKED_BY}{}", app.as_deref().unwrap_or(UNIDENTIFIED))
            }
            Ok(Some(Reply::Started { .. })) => continue,
            Ok(Some(Reply::Exited(Exit::Code(code)))) => format!("ran {code}"),
            Ok(Some(Reply::Exited(Exit::Signal(signal)))) => format!("signalled {signal}"),
            Ok(Some(Reply::Denied { reason, by_person })) => {
                let who = if by_person { " by the person" } else { "" };
                format!("denied{who} {reason}")
            }
            other => format!("lost {other:?}"),
        };
        let ends = !said.starts_with(ASKED_BY);
        if writeln!(out, "{said}").and_then(|()| out.flush()).is_err() || ends {
            return 0;
        }
    }
}

/// A key pressed and let go, typing `codepoint`, as a keyboard would.
pub fn key(keycode: u16, codepoint: u32) {
    for pressed in [true, false] {
        let rc = test_key(keycode, codepoint, pressed);
        if rc != 0 {
            eprintln!("test_key {keycode:#x} {codepoint:#x}: {rc}");
        }
    }
    std::thread::sleep(Duration::from_millis(20));
}

/// Past the time a question on screen ignores input for.
pub fn armed() {
    std::thread::sleep(Duration::from_millis(ARMING_MS + 750));
}

pub fn type_text(text: &str) {
    for ch in text.chars() {
        key(0, ch as u32);
    }
}

/// Past the time a move re-arms the dialog for, counted from when the
/// compositor takes the key, which a loaded machine takes late.
pub fn moved() {
    std::thread::sleep(Duration::from_millis(4 * ARMING_MS));
}

pub fn tab_and_enter() {
    armed();
    key(KEY_TAB, 0);
    moved();
    key(KEY_ENTER, 0);
}

pub fn escape() {
    armed();
    key(KEY_ESC, 0);
}
