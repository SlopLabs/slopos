//! privd end to end: who may ask for what, what a mechanism runs with, and
//! every way a request is refused, between real processes and the privd init
//! started.
//!
//! The runner hands this binary every capability as uid 0, which the policy
//! admits to every action without asking. The session's half runs in a child
//! at uid 1000 holding nothing; a test boot runs no compositor until a case
//! asks init for a session, and until then nobody can be asked, so an action
//! that needs consent is refused there.

use slopos_abi::syscall::{BOOT_FLAG_MEDIUM, POLLIN, UserPollFd};
use slopos_priv_core::consent::Refusal;
use slopos_priv_core::wire::{self, Exit, Reply, Request};
use slopos_priv_core::{ENV_ACTION, ENV_FD, POLICY, PROGRAM, Policy};
use slopos_userland::init_control::{self, Command as InitCommand};
use slopos_userland::privd::{Connection, STATUS_REFUSED, request_id};
use slopos_userland::syscall::core::boot_flag;
use slopos_userland::syscall::{OwnedFd, RawFd, fs, net, process};

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SELF: &str = "/bin/privd_test";
const USER: u32 = 1000;
/// A uid the policy names no principal for.
const STRANGER: u32 = 4242;
const RESCUE: &str = "org.slopos.rescue";

/// `argv[1]` of the uid-1000 child, which holds a connection this process
/// made at [`ENV_FD`].
const SESSION: &str = "--session";
/// `argv[1]` of a child that connects, hands its connection to an [`ASK`]
/// child and then leaves or stays as `argv[2]` says.
const CONNECT: &str = "--connect";
/// `argv[1]` of a child that asks through the connection it was handed
/// once its maker has done what `argv[2]` says; its verdict goes to stdout.
const ASK: &str = "--ask";
/// `argv[1]` of the [`STRANGER`] child.
const STRANGER_ARG: &str = "--stranger";
/// `argv[1]` of a child that holds as many connections as privd lets one
/// requester, at [`CROWD_UID`].
const CROWD: &str = "--crowd";
const CROWD_UID: u32 = 4243;
/// `argv[1]` of a probe that exits 1 when it holds a connection to privd.
const HOLDS_PRIVD: &str = "--holds-privd";
/// `argv[1]` of a uid-1000 child that asks for an action needing a person and
/// leaves once it is asked: [`ASKED`], or [`NOBODY`] when nobody can be.
const CONSENT: &str = "--consent";
/// `argv[1]` of a uid-1000 child that asks as [`CONSENT`] does, says `asked`
/// on stdout, and exits 0 once the question is interrupted.
const CONSENT_HELD: &str = "--consent-held";
const ASKED: i32 = 0;
const NOBODY: i32 = 3;
/// The probe's question was interrupted by a session ending or starting.
const INTERRUPTED: i32 = 4;
/// Long enough for a session to end and a compositor to start under TCG.
const SESSION_DEADLINE: Duration = Duration::from_secs(120);
/// How long a consent probe may take to be asked or refused.
const PROBE_DEADLINE: Duration = Duration::from_secs(60);

fn note(why: &str) {
    eprintln!("privd_test: {why}");
}

fn null() -> OwnedFd {
    fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR).expect("/dev/null")
}

fn request(action: &str, args: &[&str]) -> Request {
    Request {
        id: request_id(),
        action: action.to_owned(),
        args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
        cwd: false,
    }
}

fn send(connection: &Connection, request: &Request, stdout: RawFd) -> bool {
    let stdin = null();
    connection.send(request, &[stdin.raw(), stdout, 2]).is_ok()
}

/// Send `args` for `action` with stdout at `stdout`, and every reply up to
/// the one that ends the request.
fn ask(connection: &mut Connection, action: &str, args: &[&str], stdout: RawFd) -> Vec<Reply> {
    let request = request(action, args);
    if !send(connection, &request, stdout) {
        note(&format!("{action} could not be sent"));
        return Vec::new();
    }
    replies(connection, request.id)
}

fn replies(connection: &mut Connection, id: u64) -> Vec<Reply> {
    let mut replies = Vec::new();
    loop {
        match connection.reply(id) {
            Ok(Some(reply)) => {
                let last = matches!(reply, Reply::Exited(_) | Reply::Denied { .. });
                replies.push(reply);
                if last {
                    return replies;
                }
            }
            Ok(None) => return replies,
            Err(why) => {
                note(&why);
                return replies;
            }
        }
    }
}

fn ended(replies: &[Reply]) -> Option<&Reply> {
    replies.last()
}

fn denied(replies: &[Reply]) -> Option<&str> {
    match ended(replies) {
        Some(Reply::Denied {
            reason,
            by_person: false,
        }) => Some(reason),
        _ => None,
    }
}

fn exited(replies: &[Reply]) -> Option<Exit> {
    match ended(replies) {
        Some(Reply::Exited(exit)) => Some(*exit),
        _ => None,
    }
}

fn connect() -> Option<Connection> {
    Connection::open().map_err(|why| note(&why)).ok()
}

fn the_policy_is_the_bases_and_parses() -> bool {
    let policy = std::fs::read_to_string(POLICY)
        .map_err(|e| e.to_string())
        .and_then(|text| Policy::parse(&text).map_err(|e| e.to_string()));
    match policy {
        Ok(policy) if policy.action("org.slopos.install").is_some() => true,
        other => {
            note(&format!("{POLICY}: {:?}", other.map(|p| p.actions().len())));
            false
        }
    }
}

/// A uid-0 requester is run without being asked, and the mechanism holds what
/// its action names: `prof status` needs `SysInspect`, which this request
/// sends none of.
fn root_runs_an_action_holding_its_capabilities() -> bool {
    let Some(mut privd) = connect() else {
        return false;
    };
    let Ok((read, write)) = fs::pipe() else {
        return false;
    };
    let replies = ask(&mut privd, "org.slopos.prof", &["status"], write.raw());
    drop(write);
    let mut out = String::new();
    // SAFETY: `read` is a pipe end this function owns.
    let _ = unsafe { std::fs::File::from_raw_fd(read.into_raw()) }.read_to_string(&mut out);
    let started = matches!(replies.first(), Some(Reply::Started { pid }) if *pid > 0);
    if !started || exited(&replies) != Some(Exit::Code(0)) {
        note(&format!("prof status asked as root: {replies:?}"));
        return false;
    }
    if out.trim() != "running" && out.trim() != "stopped" {
        note(&format!("prof status wrote {out:?}"));
        return false;
    }
    true
}

/// What the mechanism exits with is what the requester is told.
fn the_mechanism_s_exit_status_comes_back() -> bool {
    let Some(mut privd) = connect() else {
        return false;
    };
    let devnull = null();
    let replies = ask(&mut privd, "org.slopos.kconsole", &[""], devnull.raw());
    if exited(&replies) != Some(Exit::Code(2)) {
        note(&format!("kconsole with an empty key list: {replies:?}"));
        return false;
    }
    true
}

/// argv outside every shape of the action is refused before anything runs,
/// as is an action the policy does not name; the same connection then still
/// runs one inside.
fn argv_outside_the_action_is_refused() -> bool {
    let Some(mut privd) = connect() else {
        return false;
    };
    let devnull = null();
    for (action, args, why) in [
        (
            "org.slopos.net.toggle",
            &["net", "flap"][..],
            "argument list",
        ),
        (
            "org.slopos.boot.status",
            &["status", "--all"],
            "argument list",
        ),
        (
            "org.slopos.boot.install",
            &["--boot", "/dev/vda", "install"],
            "argument list",
        ),
        ("org.slopos.no-such-action", &["status"], "no action"),
    ] {
        let replies = ask(&mut privd, action, args, devnull.raw());
        if !denied(&replies).is_some_and(|reason| reason.contains(why)) || replies.len() != 1 {
            note(&format!("{action} {args:?}: {replies:?}"));
            return false;
        }
    }
    let replies = ask(&mut privd, "org.slopos.prof", &["status"], devnull.raw());
    if exited(&replies) != Some(Exit::Code(0)) {
        note(&format!("prof status after the refusals: {replies:?}"));
        return false;
    }
    true
}

/// A request that carries other than its descriptors, or is not one, is
/// refused and ends the connection.
fn a_request_without_its_descriptors_is_refused() -> bool {
    let request = request("org.slopos.prof", &["status"]);
    for frame in [
        request.frame(),
        slopos_remote_core::frame(slopos_priv_core::wire::kind::REQUEST, b"\x05bogus"),
        slopos_remote_core::frame(0x7f, b""),
    ] {
        let Some(mut privd) = connect() else {
            return false;
        };
        if let Err(why) = privd.send_frame(&frame, &[]) {
            note(&why);
            return false;
        }
        match privd.reply(request.id) {
            Ok(Some(Reply::Denied { .. })) => {}
            other => {
                note(&format!("a request without descriptors: {other:?}"));
                return false;
            }
        }
        if !matches!(privd.reply(request.id), Ok(None)) {
            note("the connection stayed open after a malformed request");
            return false;
        }
    }
    let Some(mut privd) = connect() else {
        return false;
    };
    let stdin = null();
    let surplus = [stdin.raw(), stdin.raw(), 2, 2];
    if privd.send(&request, &surplus).is_err()
        || !matches!(privd.reply(request.id), Ok(Some(Reply::Denied { .. })))
        || !matches!(privd.reply(request.id), Ok(None))
    {
        note("a request carrying a descriptor too many was not refused");
        return false;
    }
    true
}

/// What an earlier request on the connection left unread is passed over, and
/// a signal naming it reaches nothing else. The second request goes once the
/// first has started: the kernel hands one `recvmsg` every descriptor waiting,
/// so privd reads a connection's requests one at a time.
fn what_an_earlier_request_left_is_passed_over() -> bool {
    let Some(mut privd) = connect() else {
        return false;
    };
    let devnull = null();
    let first = request("org.slopos.prof", &["status"]);
    let second = request("org.slopos.prof", &["status"]);
    if !send(&privd, &first, devnull.raw()) {
        return false;
    }
    if !matches!(privd.reply(first.id), Ok(Some(Reply::Started { .. }))) {
        note("the first request did not start");
        return false;
    }
    let stale = wire::signal_frame(first.id, slopos_abi::signal::SIGTERM);
    if privd.send_frame(&stale, &[]).is_err() || !send(&privd, &second, devnull.raw()) {
        return false;
    }
    let replies = replies(&mut privd, second.id);
    if exited(&replies) != Some(Exit::Code(0)) {
        note(&format!(
            "the second request on the connection: {replies:?}"
        ));
        return false;
    }
    true
}

/// One requester holds only so many connections; the next is refused before
/// it asks anything. Counted at a uid of its own, which no earlier case left
/// connections under.
fn one_requester_holds_only_so_many_connections() -> bool {
    match run_self(&[CROWD], CROWD_UID, None) {
        Some(out) if out.status.success() => true,
        other => {
            note(&format!("the crowd's half: {other:?}"));
            false
        }
    }
}

fn crowd() -> i32 {
    let devnull = null();
    let mut held = Vec::new();
    let mut seen = String::new();
    for _ in 0..12 {
        let Some(mut privd) = connect() else {
            return 1;
        };
        // A refused connection is closed before it is written to, so what
        // privd said is read whether or not the request went.
        let request = request("org.slopos.prof", &["status"]);
        let _ = send(&privd, &request, devnull.raw());
        let replies = replies(&mut privd, request.id);
        seen.push(match denied(&replies) {
            Some(reason) if reason.contains("may not ask") => 'a',
            Some(reason) if reason.contains("too many") => 'r',
            _ => '?',
        });
        held.push(privd);
    }
    if seen != "aaaaaaaarrrr" {
        note(&format!("connections admitted (a) and refused (r): {seen}"));
        return 2;
    }
    0
}

/// An action that runs in the requester's directory takes it from the
/// descriptor sent, which must be a directory still in the tree, and is
/// refused without one; an action that does not refuses any.
fn the_working_directory_is_the_descriptor_sent() -> bool {
    let devnull = null();
    let Ok(tmp) = fs::open_cstr(
        c"/tmp",
        slopos_abi::fs::O_RDONLY | slopos_abi::fs::O_DIRECTORY,
    ) else {
        note("/tmp did not open");
        return false;
    };
    let with_dir = |action: &str, args: &[&str], dir: RawFd| {
        let mut privd = connect()?;
        let request = Request {
            cwd: true,
            ..request(action, args)
        };
        let stdin = null();
        privd
            .send(&request, &[stdin.raw(), devnull.raw(), 2, dir])
            .ok()?;
        Some(replies(&mut privd, request.id))
    };
    let usage = with_dir("org.slopos.boot.install", &["install"], tmp.raw());
    if !matches!(
        usage.as_deref(),
        Some([Reply::Started { .. }, Reply::Exited(Exit::Code(1))])
    ) {
        note(&format!("bootctl install with no slot, in /tmp: {usage:?}"));
        return false;
    }
    let gone = "/tmp/privd_test.gone";
    let _ = std::fs::remove_dir(gone);
    let removed = std::fs::create_dir(gone).ok().and_then(|()| {
        let dir = fs::open_cstr(
            c"/tmp/privd_test.gone",
            slopos_abi::fs::O_RDONLY | slopos_abi::fs::O_DIRECTORY,
        );
        std::fs::remove_dir(gone).ok()?;
        dir.ok()
    });
    let Some(removed) = removed else {
        note("no directory to remove");
        return false;
    };
    let nowhere = with_dir("org.slopos.boot.install", &["install"], removed.raw());
    if !nowhere
        .as_deref()
        .and_then(denied)
        .is_some_and(|reason| reason.contains("no longer in the tree"))
    {
        note(&format!(
            "bootctl install in a removed directory: {nowhere:?}"
        ));
        return false;
    }
    let Some(mut privd) = connect() else {
        return false;
    };
    let unsent = request("org.slopos.boot.install", &["install"]);
    let _ = send(&privd, &unsent, devnull.raw());
    let replies = replies(&mut privd, unsent.id);
    if !denied(&replies).is_some_and(|reason| reason.contains("was not sent")) {
        note(&format!(
            "bootctl install with no directory sent: {replies:?}"
        ));
        return false;
    }
    for (action, args, dir, why) in [
        (
            "org.slopos.boot.install",
            &["install"][..],
            devnull.raw(),
            "not a directory",
        ),
        (
            "org.slopos.prof",
            &["status"],
            tmp.raw(),
            "no working directory",
        ),
    ] {
        let replies = with_dir(action, args, dir).unwrap_or_default();
        if !denied(&replies).is_some_and(|reason| reason.contains(why)) {
            note(&format!("{action} with a directory: {replies:?}"));
            return false;
        }
    }
    true
}

fn run_self(args: &[&str], uid: u32, privd: Option<&Connection>) -> Option<std::process::Output> {
    let mut cmd = Command::new(SELF);
    cmd.args(args).uid(uid).gid(uid).env_remove(ENV_FD);
    if let Some(privd) = privd {
        privd.hand_to(&mut cmd);
    }
    let out = cmd.stderr(Stdio::inherit()).output();
    if let Some(privd) = privd {
        privd.set_inheritable(false);
    }
    out.map_err(|e| note(&format!("{SELF} {args:?}: {e}"))).ok()
}

/// The session runs what the policy lets it without asking, is refused what
/// needs a person when nobody can be asked, and through a connection a uid-0
/// process made and handed it, asks as that process, as does a mechanism the
/// shell runs with it, which keeps it from what it starts, but not one a
/// subshell runs, and not one whose environment names another descriptor,
/// which would keep nothing back.
fn the_session_asks_as_itself_or_as_who_handed_it_a_connection() -> bool {
    let Some(privd) = connect() else {
        return false;
    };
    match run_self(&[SESSION], USER, Some(&privd)) {
        Some(out) if out.status.success() => true,
        other => {
            note(&format!("the session's half: {other:?}"));
            false
        }
    }
}

fn session() -> i32 {
    let run = |program: &str, args: &[&str], inherited: bool| {
        let mut cmd = Command::new(program);
        cmd.args(args).stdout(Stdio::null());
        if !inherited {
            cmd.env_remove(ENV_FD);
        }
        cmd.status().ok().and_then(|s| s.code())
    };
    if slopos_userland::syscall::process::geteuid() != USER {
        note("the session's half is not uid 1000");
        return 1;
    }
    if run("/bin/prof", &["status"], false) != Some(0) {
        note("prof status, which the session may run, did not");
        return 2;
    }
    if run("/bin/ip", &["link", "set", "lo", "up"], false) != Some(STATUS_REFUSED) {
        note("ip link set, which needs a person and nobody can be asked, was not refused");
        return 3;
    }
    if run("/bin/ip", &["link", "set", "lo", "up"], true) != Some(0) {
        note("ip link set through the root connection this process was handed did not run");
        return 4;
    }
    if run("/bin/shell", &["-c", "ip link set lo up"], true) != Some(0) {
        note("the shell did not hand its connection to the mechanism it ran");
        return 7;
    }
    if run("/bin/shell", &["-c", "(ip link set lo up)"], true) != Some(STATUS_REFUSED) {
        note("a subshell handed on the connection the shell holds");
        return 8;
    }
    if run(SELF, &[HOLDS_PRIVD], true) != Some(1) {
        note("the probe found no connection where one was inherited");
        return 9;
    }
    for line in [
        format!("cpufreq run -- {SELF} {HOLDS_PRIVD}"),
        format!("PRIVD_FD=99 cpufreq run -- {SELF} {HOLDS_PRIVD}"),
        format!("unset PRIVD_FD; cpufreq run -- {SELF} {HOLDS_PRIVD}"),
    ] {
        if run("/bin/shell", &["-c", &line], true) != Some(0) {
            note(&format!("`{line}` passed the connection on"));
            return 10;
        }
    }
    let Some(mut privd) = connect() else {
        return 5;
    };
    let devnull = null();
    let replies = ask(
        &mut privd,
        "org.slopos.net.configure",
        &["link", "set", "lo", "up"],
        devnull.raw(),
    );
    if !denied(&replies).is_some_and(|why| why.contains("nothing can ask them")) {
        note(&format!("net.configure with nobody to ask: {replies:?}"));
        return 6;
    }
    0
}

fn holds_privd() -> i32 {
    let to_privd = |fd| net::peer_program(fd).ok().flatten().as_deref() == Some(PROGRAM.as_bytes());
    i32::from((3..256).any(to_privd))
}

/// A uid no principal names is refused every action.
fn a_uid_the_policy_names_nowhere_is_refused() -> bool {
    match run_self(&[STRANGER_ARG], STRANGER, None) {
        Some(out) if out.status.success() => true,
        other => {
            note(&format!("the stranger's half: {other:?}"));
            false
        }
    }
}

fn stranger() -> i32 {
    let Some(mut privd) = connect() else {
        return 1;
    };
    let devnull = null();
    let replies = ask(&mut privd, "org.slopos.prof", &["status"], devnull.raw());
    match denied(&replies) {
        Some(_) => 0,
        None => {
            note(&format!("uid {STRANGER} asking for prof: {replies:?}"));
            2
        }
    }
}

/// A connection outlives its maker only as a refusal: once the process that
/// connected has exited, nothing is run for whoever holds the connection. Its
/// control is the same hand-off with the maker still there.
fn a_requester_that_has_exited_is_refused() -> bool {
    for (maker, verdict) in [("stay", "RAN"), ("leave", "REFUSED")] {
        let out = Command::new(SELF)
            .args([CONNECT, maker])
            .env_remove(ENV_FD)
            .stderr(Stdio::inherit())
            .output();
        let said = out
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        if said != verdict {
            note(&format!("a maker that would {maker}: {out:?}"));
            return false;
        }
    }
    true
}

fn connect_and_hand(maker: &str) -> i32 {
    let Some(privd) = connect() else {
        return 1;
    };
    let Ok((gone_read, gone_write)) = fs::pipe() else {
        return 2;
    };
    let _ = fs::set_fd_cloexec(gone_write.raw());
    let mut cmd = Command::new(SELF);
    cmd.args([ASK, maker, &gone_read.raw().to_string()]);
    privd.hand_to(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return 3;
    };
    drop(gone_read);
    if maker == "stay" {
        let _ = child.wait();
    }
    drop(gone_write);
    0
}

/// Asks once the maker has left, which closing its end of the pipe tells;
/// privd may not have seen the exit yet, so a run is retried for a while.
fn ask_handed(maker: &str, gone: &str) -> i32 {
    let gone: RawFd = gone.parse().unwrap_or(-1);
    if maker == "leave" {
        let mut byte = [0u8; 1];
        while fs::read_slice(gone, &mut byte).is_ok_and(|n| n > 0) {}
    }
    let Some(mut privd) = Connection::inherited() else {
        note("no connection was handed over");
        return 1;
    };
    let devnull = null();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let replies = ask(&mut privd, "org.slopos.prof", &["status"], devnull.raw());
        match (denied(&replies), exited(&replies)) {
            (Some(_), _) => {
                println!("REFUSED");
                return 0;
            }
            (None, Some(Exit::Code(0))) if maker == "stay" => {
                println!("RAN");
                return 0;
            }
            (None, Some(Exit::Code(0))) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {
                println!("{replies:?}");
                return 2;
            }
        }
    }
}

/// Started by privd, a mechanism acts only for the action privd started it
/// for; started by anyone else holding nothing, it asks.
fn a_mechanism_holds_its_argv_to_its_action() -> bool {
    let kconsole = |action: Option<&str>| {
        let mut cmd = Command::new("/bin/kconsole");
        cmd.arg("h").stdout(Stdio::null()).stderr(Stdio::null());
        match action {
            Some(action) => cmd.env(ENV_ACTION, action),
            None => cmd.env_remove(ENV_ACTION),
        };
        cmd.status().ok().and_then(|s| s.code())
    };
    let other = kconsole(Some("org.slopos.prof"));
    let own = kconsole(Some("org.slopos.kconsole"));
    let asked = kconsole(None);
    if (other, own, asked) != (Some(STATUS_REFUSED), Some(1), Some(0)) {
        note(&format!(
            "kconsole for another action, its own unprivileged, and asking: {other:?} {own:?} {asked:?}"
        ));
        return false;
    }
    true
}

/// A terminal is never handed to the mechanism, which runs in another
/// session; its output reaches the terminal through the requester, whose
/// controlling terminal it is.
fn output_for_a_terminal_is_relayed() -> bool {
    let Ok((master, _)) = process::openpty() else {
        note("no pty");
        return false;
    };
    let Ok(slave) = fs::ioctl_tiocgptpeer(master.raw()) else {
        note("no pty slave");
        return false;
    };
    let tty = |fd: &OwnedFd| {
        fs::dup(fd.raw())
            // SAFETY: `dup` just opened this descriptor for us.
            .map(|dup| Stdio::from(unsafe { std::fs::File::from_raw_fd(dup.into_raw()) }))
    };
    let (Ok(stdin), Ok(stdout)) = (tty(&slave), tty(&slave)) else {
        return false;
    };
    let mut cmd = Command::new("/bin/prof");
    cmd.arg("status")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::inherit());
    // SAFETY: the closure makes two system calls and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if process::setsid() < 0 || fs::tiocsctty(0).is_err() {
                return Err(std::io::Error::other("no controlling terminal"));
            }
            Ok(())
        });
    }
    let status = cmd.status().ok().and_then(|s| s.code());
    drop(slave);
    let _ = slopos_userland::syscall::net::set_nonblocking(master.raw());
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    while let Ok(n) = fs::read_slice(master.raw(), &mut buf) {
        if n == 0 {
            break;
        }
        seen.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&seen);
    if status != Some(0) || !(text.contains("running") || text.contains("stopped")) {
        note(&format!(
            "prof status on its controlling terminal exited {status:?}, wrote {text:?}"
        ));
        return false;
    }
    true
}

/// The rescue shell exists only on a boot of the install medium, which this
/// is not: the policy carries it marked so, privd answers it as no action,
/// and the shell asked for one refuses as a mechanism does.
fn the_rescue_shell_is_offered_only_on_the_medium() -> bool {
    if boot_flag(BOOT_FLAG_MEDIUM) {
        note("booted from the install medium, where installer_test asks for a rescue shell");
        return true;
    }
    let policy = std::fs::read_to_string(POLICY)
        .map_err(|e| e.to_string())
        .and_then(|text| Policy::parse(&text).map_err(|e| e.to_string()));
    if !policy
        .as_ref()
        .is_ok_and(|p| p.action(RESCUE).is_some_and(|a| a.medium))
    {
        note(&format!("{POLICY} names no medium action {RESCUE}"));
        return false;
    }
    let Some(mut privd) = connect() else {
        return false;
    };
    let devnull = null();
    let replies = ask(&mut privd, RESCUE, &["--rescue"], devnull.raw());
    if !denied(&replies).is_some_and(|reason| reason.contains("no action")) {
        note(&format!("{RESCUE} off the medium: {replies:?}"));
        return false;
    }
    let shell = Command::new("/bin/shell")
        .arg("--rescue")
        .stdin(Stdio::null())
        .output();
    match shell {
        Ok(out) if out.status.code() == Some(STATUS_REFUSED) => true,
        other => {
            note(&format!("shell --rescue off the medium: {other:?}"));
            false
        }
    }
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "the_policy_is_the_bases_and_parses",
        the_policy_is_the_bases_and_parses,
    ),
    (
        "root_runs_an_action_holding_its_capabilities",
        root_runs_an_action_holding_its_capabilities,
    ),
    (
        "the_mechanism_s_exit_status_comes_back",
        the_mechanism_s_exit_status_comes_back,
    ),
    (
        "argv_outside_the_action_is_refused",
        argv_outside_the_action_is_refused,
    ),
    (
        "a_request_without_its_descriptors_is_refused",
        a_request_without_its_descriptors_is_refused,
    ),
    (
        "the_working_directory_is_the_descriptor_sent",
        the_working_directory_is_the_descriptor_sent,
    ),
    (
        "what_an_earlier_request_left_is_passed_over",
        what_an_earlier_request_left_is_passed_over,
    ),
    (
        "one_requester_holds_only_so_many_connections",
        one_requester_holds_only_so_many_connections,
    ),
    (
        "the_session_asks_as_itself_or_as_who_handed_it_a_connection",
        the_session_asks_as_itself_or_as_who_handed_it_a_connection,
    ),
    (
        "a_uid_the_policy_names_nowhere_is_refused",
        a_uid_the_policy_names_nowhere_is_refused,
    ),
    (
        "a_requester_that_has_exited_is_refused",
        a_requester_that_has_exited_is_refused,
    ),
    (
        "a_mechanism_holds_its_argv_to_its_action",
        a_mechanism_holds_its_argv_to_its_action,
    ),
    (
        "output_for_a_terminal_is_relayed",
        output_for_a_terminal_is_relayed,
    ),
    (
        "the_rescue_shell_is_offered_only_on_the_medium",
        the_rescue_shell_is_offered_only_on_the_medium,
    ),
    (
        "consent_is_asked_across_a_compositor_restart",
        consent_is_asked_across_a_compositor_restart,
    ),
];

/// With a desktop session up, the session's request that needs a person is
/// put to the compositor. Ending the session interrupts a question on screen,
/// though no compositor is left to say so, and nobody can be asked after; a
/// compositor started again is asked the next one.
fn consent_is_asked_across_a_compositor_restart() -> bool {
    let outcome = (|| {
        ask_init(InitCommand::RestartSession)
            && asked_within(SESSION_DEADLINE)
            && held_across_the_end()
            && nobody_within(SESSION_DEADLINE)
            && ask_init(InitCommand::RestartSession)
            && asked_within(SESSION_DEADLINE)
    })();
    ask_init(InitCommand::EndSession) && outcome
}

fn ask_init(command: InitCommand) -> bool {
    init_control::ask(command)
        .map_err(|why| note(&format!("{}: {why}", command.word())))
        .is_ok()
}

/// A question on screen when the session ends is interrupted.
fn held_across_the_end() -> bool {
    let held = Command::new(SELF)
        .arg(CONSENT_HELD)
        .uid(USER)
        .gid(USER)
        .env_remove(ENV_FD)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let Ok(mut held) = held else {
        note("the held request did not start");
        return false;
    };
    let mut said = [0u8; 6];
    let asked = held.stdout.take().is_some_and(|mut out| {
        let mut fds = [UserPollFd {
            fd: out.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        }];
        let ready = fs::poll(&mut fds, PROBE_DEADLINE.as_millis() as i64);
        matches!(ready, Ok(n) if n > 0) && out.read_exact(&mut said).is_ok() && &said == b"asked\n"
    });
    let ended = asked && ask_init(InitCommand::EndSession);
    if !ended {
        let _ = held.kill();
    }
    let interrupted = held.wait().ok().and_then(|status| status.code());
    if !ended || interrupted != Some(0) {
        note(&format!(
            "a question across the end: asked {asked}, ended {ended}, the requester exited \
             {interrupted:?}"
        ));
        return false;
    }
    true
}

/// Whether the consent probe says nobody can be asked within `deadline`:
/// privd learns that the compositor has gone a moment after init does.
fn nobody_within(deadline: Duration) -> bool {
    let start = Instant::now();
    loop {
        match probe() {
            Some(NOBODY) => return true,
            Some(ASKED | INTERRUPTED) if start.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(250))
            }
            other => {
                note(&format!(
                    "with the session ended, the consent probe exited {other:?}"
                ));
                return false;
            }
        }
    }
}

/// The consent probe's exit status, `None` once it has run past
/// [`PROBE_DEADLINE`].
fn probe() -> Option<i32> {
    let mut child = Command::new(SELF)
        .arg(CONSENT)
        .uid(USER)
        .gid(USER)
        .env_remove(ENV_FD)
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if start.elapsed() < PROBE_DEADLINE => {
                std::thread::sleep(Duration::from_millis(100))
            }
            _ => {
                note("the consent probe was neither asked nor refused in time");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Whether a uid-1000 request is put to the person within `deadline`, asking
/// again while nobody can be asked yet.
fn asked_within(deadline: Duration) -> bool {
    let start = Instant::now();
    loop {
        match probe() {
            Some(ASKED) => return true,
            Some(NOBODY | INTERRUPTED) if start.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(250))
            }
            other => {
                note(&format!("the consent probe exited {other:?}"));
                return false;
            }
        }
    }
}

/// Ask for `ip link set lo up`, which the session needs a person for, and
/// answer how the request went: asked, nobody to ask, or anything else.
fn consent(hold: bool) -> i32 {
    let Some(mut privd) = connect() else {
        return 1;
    };
    let devnull = null();
    let request = request("org.slopos.net.configure", &["link", "set", "lo", "up"]);
    if !send(&privd, &request, devnull.raw()) {
        return 1;
    }
    match privd.reply(request.id) {
        Ok(Some(Reply::Asking)) => {}
        Ok(Some(Reply::Denied { reason, .. })) if reason.contains("nothing can ask them") => {
            return NOBODY;
        }
        Ok(Some(Reply::Denied { reason, .. })) if reason == Refusal::Interrupted.to_string() => {
            return INTERRUPTED;
        }
        other => {
            note(&format!("the consent probe was answered {other:?}"));
            return 1;
        }
    }
    if !hold {
        return ASKED;
    }
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        if out
            .write_all(b"asked\n")
            .and_then(|()| out.flush())
            .is_err()
        {
            return 1;
        }
    }
    let interrupted = Refusal::Interrupted.to_string();
    match privd.reply(request.id) {
        Ok(Some(Reply::Denied { reason, .. })) if reason == interrupted => 0,
        other => {
            note(&format!("the held question ended as {other:?}"));
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| args.get(i).map_or("", String::as_str);
    match arg(1) {
        SESSION => std::process::exit(session()),
        STRANGER_ARG => std::process::exit(stranger()),
        CROWD => std::process::exit(crowd()),
        HOLDS_PRIVD => std::process::exit(holds_privd()),
        CONSENT => std::process::exit(consent(false)),
        CONSENT_HELD => std::process::exit(consent(true)),
        CONNECT => std::process::exit(connect_and_hand(arg(2))),
        ASK => std::process::exit(ask_handed(arg(2), arg(3))),
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
