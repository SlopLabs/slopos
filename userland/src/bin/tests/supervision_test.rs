//! init as a service manager, against the init that booted: a re-executed
//! init keeps what it serves, a privd that dies is started again, and a
//! session restart ends what carries the desktop's login tag and spares what
//! carries remoted's.
//!
//! The runner hands this binary every capability as uid 0. A test boot runs
//! no desktop session until one is asked for, and this one ends the one it
//! starts.

use std::ffi::CString;
use std::time::{Duration, Instant};

use slopos_abi::fs::{O_RDWR, SESSION_UID};
use slopos_abi::signal::SIGKILL;
use slopos_abi::syscall::{POLLIN, UserPollFd};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::wire::{Exit, Reply, Request};
use slopos_userland::init_control::{self, Command, DESKTOP, REMOTE};
use slopos_userland::privd::{Connection, request_id};
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::{OwnedFd, fs, pidfd};

const SELF: &str = "/bin/supervision_test";
/// `argv[1]` of a child that sleeps until it is killed.
const LINGER: &str = "--linger";
/// `argv[1]` of a child that asks init for `argv[2]` and exits 0 when init
/// does it, 1 when init refuses, 2 when init is not reached.
const ASK: &str = "--ask";
/// A uid that is neither the session's nor 0.
const STRANGER: u32 = 4242;
/// Long enough for a session to end and a compositor to start under TCG.
const DEADLINE: Duration = Duration::from_secs(120);

fn note(why: &str) {
    eprintln!("supervision_test: {why}");
}

fn spawn_self(args: &[&str], endowment: Endowment<'_>) -> Option<u32> {
    let owned: Vec<CString> = std::iter::once(SELF)
        .chain(args.iter().copied())
        .map(|arg| CString::new(arg).expect("no argument holds a NUL"))
        .collect();
    let argv: Vec<*const u8> = owned.iter().map(|arg| arg.as_ptr().cast()).collect();
    let stdio = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_endowed(
        SELF.as_bytes(),
        &argv,
        &[],
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &stdio,
        0,
        endowment,
    );
    (tid > 0).then_some(tid as u32)
}

fn linger(login: u32) -> Option<u32> {
    spawn_self(
        &[LINGER],
        Endowment {
            login: Some(login),
            ..Endowment::NONE
        },
    )
}

/// Whether init did `command` for a requester running as `uid`.
fn asked_as(uid: u32, command: Command) -> Option<bool> {
    let child = spawn_self(
        &[ASK, command.word()],
        Endowment {
            ids: Some((uid, uid)),
            groups: Some(&[]),
            ..Endowment::NONE
        },
    )?;
    match process::wait_exit_code(child) {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

fn readable_within(fd: &OwnedFd, within: Duration) -> bool {
    let mut fds = [UserPollFd {
        fd: fd.raw(),
        events: POLLIN,
        revents: 0,
    }];
    matches!(fs::poll(&mut fds, within.as_millis() as i64), Ok(n) if n > 0)
}

fn until(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// Whether privd runs `prof status` for this uid-0 process, which the
/// policy lets it ask without consent.
fn privd_serves() -> bool {
    let Ok(mut privd) = Connection::open_within(DEADLINE) else {
        return false;
    };
    let Ok(null) = fs::open_cstr(c"/dev/null", O_RDWR) else {
        return false;
    };
    let request = Request {
        id: request_id(),
        action: "org.slopos.prof".into(),
        args: vec![b"status".to_vec()],
        cwd: false,
    };
    if privd
        .send(&request, &[null.raw(), null.raw(), null.raw()])
        .is_err()
    {
        return false;
    }
    loop {
        match privd.reply(request.id) {
            Ok(Some(Reply::Exited(exit))) => return exit == Exit::Code(0),
            Ok(Some(Reply::Denied { reason, .. })) => {
                note(&format!("prof status was refused: {reason}"));
                return false;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return false,
        }
    }
}

/// A uid-0 request to re-execute is answered by the new image, which keeps
/// the privd it ran, starts privd again when it dies, and is still the
/// program its sockets attest: the consent pair rests on that.
fn init_reexecutes_keeping_what_it_serves() -> bool {
    if asked_as(SESSION_UID, Command::Reexec) != Some(false) {
        note("the session had init re-execute");
        return false;
    }
    if !init_listens() {
        note("/run/init does not attest /sbin/init before the re-execution");
        return false;
    }
    let privd = || init_control::listener(slopos_priv_core::SOCKET);
    let before = privd();
    if let Err(why) = init_control::ask(Command::Reexec) {
        note(&format!("reexec: {why}"));
        return false;
    }
    let after = privd();
    if before.is_none() || before != after {
        note(&format!("privd was {before:?} and is {after:?}"));
        return false;
    }
    if !init_listens() {
        note("/run/init no longer attests /sbin/init after the re-execution");
        return false;
    }
    a_killed_privd_is_started_again()
}

/// Whether `/run/init`, which init bound at boot and keeps across a
/// re-execution, attests `/sbin/init`.
fn init_listens() -> bool {
    use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
    use slopos_userland::syscall::net;
    let Ok(fd) = net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0) else {
        return false;
    };
    net::connect_unix(fd.raw(), init_control::SOCKET.as_bytes()).is_ok()
        && net::peer_program(fd.raw()).ok().flatten().as_deref()
            == Some(slopos_priv_core::INIT.as_bytes())
}

/// A privd killed outright is replaced by one that serves.
fn a_killed_privd_is_started_again() -> bool {
    let Some(old) = init_control::listener(slopos_priv_core::SOCKET) else {
        note("no privd listens");
        return false;
    };
    let Some(gone) = pidfd::pidfd_open_owned(old) else {
        return false;
    };
    if process::kill(old, SIGKILL) != 0 {
        note(&format!("privd (pid {old}) could not be killed"));
        return false;
    }
    if !readable_within(&gone, DEADLINE) {
        note("privd did not exit");
        return false;
    }
    let mut new = None;
    if !until(DEADLINE, || {
        new = init_control::listener(slopos_priv_core::SOCKET);
        new.is_some_and(|pid| pid != old)
    }) {
        note(&format!("privd was pid {old} and is {new:?}"));
        return false;
    }
    privd_serves()
}

/// Only uid 0 ends the session, and only uid 0 starts a session it ended.
fn only_uid_0_ends_the_session() -> bool {
    if asked_as(STRANGER, Command::RestartSession) != Some(false)
        || asked_as(SESSION_UID, Command::EndSession) != Some(false)
    {
        note("a uid other than the session's and 0 restarted the session, or the session ended it");
        return false;
    }
    if init_control::ask(Command::EndSession).is_err()
        || asked_as(SESSION_UID, Command::RestartSession) != Some(false)
    {
        note("the session started a session uid 0 ended");
        return false;
    }
    if let Err(why) = init_control::ask(Command::RestartSession) {
        note(&format!("restart-session: {why}"));
        return false;
    }
    true
}

/// The session may restart itself, which ends every process carrying the
/// desktop's tag and leaves remoted's running.
fn a_session_restart_spares_remoteds_processes() -> bool {
    let (desktop, remote) = (linger(DESKTOP), linger(REMOTE));
    let (Some(desktop), Some(remote)) = (desktop, remote) else {
        for pid in [desktop, remote].into_iter().flatten() {
            let _ = process::kill(pid, SIGKILL);
            let _ = process::waitpid(pid);
        }
        note("the stand-ins did not start");
        return false;
    };
    let tagged = (
        init_control::login_of(desktop),
        init_control::login_of(remote),
    );
    let desktop_gone = pidfd::pidfd_open_owned(desktop);
    let outcome = (|| {
        if tagged != (Some(DESKTOP), Some(REMOTE)) {
            note(&format!("the stand-ins carry the login tags {tagged:?}"));
            return false;
        }
        if asked_as(SESSION_UID, Command::RestartSession) != Some(true) {
            note("the session could not restart itself");
            return false;
        }
        if !desktop_gone
            .as_ref()
            .is_some_and(|fd| readable_within(fd, DEADLINE))
        {
            note("the desktop's process outlived the session");
            return false;
        }
        if init_control::login_of(remote) != Some(REMOTE) {
            note("remoted's process did not outlive the desktop session");
            return false;
        }
        if !until(DEADLINE, || !init_control::processes(DESKTOP).is_empty()) {
            note("no new session started");
            return false;
        }
        true
    })();
    let ended = init_control::ask(Command::EndSession);
    for pid in [desktop, remote] {
        let _ = process::kill(pid, SIGKILL);
        let _ = process::waitpid(pid);
    }
    if let Err(why) = &ended {
        note(&format!("end-session: {why}"));
    }
    let left = init_control::processes(DESKTOP);
    if !left.is_empty() {
        note(&format!("processes of an ended session run on: {left:?}"));
    }
    outcome && ended.is_ok() && left.is_empty()
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "init_reexecutes_keeping_what_it_serves",
        init_reexecutes_keeping_what_it_serves,
    ),
    (
        "a_killed_privd_is_started_again",
        a_killed_privd_is_started_again,
    ),
    ("only_uid_0_ends_the_session", only_uid_0_ends_the_session),
    (
        "a_session_restart_spares_remoteds_processes",
        a_session_restart_spares_remoteds_processes,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(LINGER) => loop {
            std::thread::sleep(Duration::from_secs(60));
        },
        Some(ASK) => {
            let command = args.get(2).and_then(|word| Command::parse(word.as_bytes()));
            let Some(command) = command else {
                std::process::exit(2);
            };
            match init_control::ask(command) {
                Ok(()) => std::process::exit(0),
                Err(why) => {
                    note(&format!("{}: {why}", command.word()));
                    std::process::exit(if why.starts_with("refused") { 1 } else { 2 });
                }
            }
        }
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
