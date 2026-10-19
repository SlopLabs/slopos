//! Peer identity end to end: what an `AF_UNIX` socket tells one process about
//! the process at the other end, between real processes started from the base
//! and from outside it.

use slopos_abi::syscall::{
    F_GETFD, F_GETFL, FD_CLOEXEC, MAP_ANONYMOUS, MAP_PRIVATE, O_NONBLOCK, POLLIN, PROT_EXEC,
    PROT_READ, UCred,
};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_slibc::pal::{Pal, Sys};
use slopos_userland as _;
use slopos_userland::syscall::{OwnedFd, UserPollFd, base, core as sys_core, fs, net, process};

use std::os::unix::fs::PermissionsExt;

const SELF: &[u8] = b"/bin/peer_test";
const COPY: &str = "/tmp/peer_test_copy";
const USER: u32 = 1000;

/// `argv[1]` of a child that connects to the listener `argv[2]` names, reports
/// its pid and the listener it reached, and exits once told to.
const CONNECT: &str = "--connect";
/// `argv[1]` of a child that connects, hands the connection to a `SPEAK`
/// child of its own and exits.
const HAND: &str = "--hand";
/// `argv[1]` of a child that reports its pid on fd 3 and exits once told to.
const SPEAK: &str = "--speak";
/// `argv[1]` of a child that connects and then execs a `SPEAK` child of
/// itself on the connection.
const EXEC_AFTER: &str = "--exec-after-connect";
/// `argv[1]` of a child that exits once fd 3 reaches its end.
const HOLD: &str = "--hold";

fn cstr(word: &str) -> Vec<u8> {
    let mut s = word.as_bytes().to_vec();
    s.push(0);
    s
}

/// Spawn `path` with `words` as its argv and `extra` as its fd 3.
fn spawn(path: &[u8], words: &[&str], extra: Option<i32>) -> i32 {
    let owned: Vec<Vec<u8>> = words.iter().map(|w| cstr(w)).collect();
    let argv: Vec<*const u8> = owned.iter().map(|s| s.as_ptr()).collect();
    let mut actions = vec![process::clone_fd(2, 2)];
    if let Some(fd) = extra {
        actions.push(process::clone_fd(fd, 3));
    }
    process::spawn_path_with_actions(
        path,
        &argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
    )
}

/// A socket listening at `name`, which anyone may connect to.
fn listen(name: &str) -> Option<OwnedFd> {
    let _ = std::fs::remove_file(name);
    let fd = net::socket(slopos_abi::net::AF_UNIX, slopos_abi::net::SOCK_STREAM, 0).ok()?;
    net::bind_unix(fd.raw(), name.as_bytes()).ok()?;
    std::fs::set_permissions(name, std::fs::Permissions::from_mode(0o666)).ok()?;
    net::listen(fd.raw(), 4).ok()?;
    Some(fd)
}

/// The connection `child` makes, or `None` once it has exited or a minute has
/// passed without one, so a child that fails before connecting fails its case
/// rather than hanging the suite. A child that never exits is killed.
fn accept_from(listener: &OwnedFd, child: i32) -> Option<OwnedFd> {
    for _ in 0..120 {
        if readable_within(listener.raw(), 500) {
            return net::accept(listener.raw(), None).ok();
        }
        if process::waitpid_nohang(child as u32).is_some() {
            eprintln!("peer_test: child {child} exited without connecting");
            return None;
        }
    }
    eprintln!("peer_test: child {child} never connected");
    let _ = process::terminate_task(child as u32);
    let _ = process::waitpid(child as u32);
    None
}

/// `child`'s wait status, or `None` once a minute has passed, when it is
/// killed, so a child stuck in the kernel fails its case rather than hanging
/// the suite.
fn reaped_within_a_minute(child: i32) -> Option<i32> {
    for _ in 0..600 {
        if let Some((_, status)) = process::waitpid_nohang(child as u32) {
            return Some(status);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    eprintln!("peer_test: child {child} never finished");
    let _ = process::terminate_task(child as u32);
    let _ = process::waitpid(child as u32);
    None
}

fn send_all(fd: i32, bytes: &[u8]) -> bool {
    matches!(net::send(fd, bytes, 0), Ok(n) if n == bytes.len())
}

fn recv_exact(fd: i32, buf: &mut [u8]) -> bool {
    let mut got = 0;
    while got < buf.len() {
        match net::recv(fd, &mut buf[got..], 0) {
            Ok(0) | Err(_) => return false,
            Ok(n) => got += n,
        }
    }
    true
}

fn readable_within(fd: i32, ms: i64) -> bool {
    let mut pfds = [UserPollFd {
        fd,
        events: POLLIN,
        revents: 0,
    }];
    for _ in 0..10 {
        pfds[0].revents = 0;
        match fs::poll(&mut pfds, ms) {
            Ok(n) if n >= 1 => return pfds[0].revents & POLLIN != 0,
            Ok(_) => return false,
            Err(_) => continue,
        }
    }
    false
}

fn cloexec(fd: i32) -> bool {
    Sys::fcntl(fd, F_GETFD as i32, 0).is_ok_and(|flags| flags as u64 & FD_CLOEXEC != 0)
}

fn nonblocking(fd: i32) -> bool {
    Sys::fcntl(fd, F_GETFL as i32, 0).is_ok_and(|flags| flags as u64 & O_NONBLOCK != 0)
}

fn own_cred() -> UCred {
    UCred {
        pid: process::getpid() as i32,
        uid: Sys::geteuid(),
        gid: Sys::getegid(),
    }
}

fn is_self(program: Result<Option<Vec<u8>>, impl core::fmt::Debug>) -> bool {
    matches!(program, Ok(Some(path)) if path == SELF)
}

/// What a `CONNECT` child sent: its pid and the listener's credentials as it
/// saw them.
fn read_report(fd: i32) -> Option<(u32, UCred)> {
    let mut report = [0u8; 16];
    recv_exact(fd, &mut report).then(|| {
        let pid = u32::from_ne_bytes(report[..4].try_into().unwrap_or_default());
        let cred = UCred::from_bytes(report[4..].try_into().unwrap_or(&[0; 12]));
        (pid, cred)
    })
}

/// Both ends of a pair name the process that made it, by pid, effective ids
/// and base program.
fn socketpair_names_its_creator() -> bool {
    let Ok((a, b)) = net::socketpair(slopos_abi::net::SOCK_CLOEXEC) else {
        eprintln!("peer_test: socketpair failed");
        return false;
    };
    let me = own_cred();
    let creds = (net::peer_cred(a.raw()), net::peer_cred(b.raw()));
    if creds != (Ok(me), Ok(me)) {
        eprintln!("peer_test: the pair named {creds:?}, not {me:?}");
        return false;
    }
    if !is_self(net::peer_program(a.raw())) || !is_self(net::peer_program(b.raw())) {
        eprintln!("peer_test: the pair does not name this program");
        return false;
    }
    if !cloexec(a.raw()) || !cloexec(b.raw()) {
        eprintln!("peer_test: SOCK_CLOEXEC did not reach both ends");
        return false;
    }
    let mut echo = [0u8; 5];
    if !send_all(a.raw(), b"hello") || !recv_exact(b.raw(), &mut echo) || &echo != b"hello" {
        eprintln!("peer_test: the pair does not carry bytes");
        return false;
    }
    let Ok((quiet, other)) = net::socketpair(slopos_abi::net::SOCK_NONBLOCK) else {
        return false;
    };
    if !nonblocking(quiet.raw()) || !nonblocking(other.raw()) {
        eprintln!("peer_test: SOCK_NONBLOCK did not reach both ends");
        return false;
    }
    let mut none = [0u8; 1];
    if net::recv(quiet.raw(), &mut none, 0).is_ok() {
        eprintln!("peer_test: an empty SOCK_NONBLOCK end did not refuse a read");
        return false;
    }
    true
}

/// A child that changed its ids and then exec'd this program connects: the
/// server is told the child's pid, ids and program, and the child the
/// server's as of its `listen`.
fn connected_pair_names_each_other() -> bool {
    const NAME: &str = "/tmp/peer-e2e-pair";
    let Some(listener) = listen(NAME) else {
        eprintln!("peer_test: could not listen");
        return false;
    };
    let pid = process::fork();
    if pid == 0 {
        let to_user = unsafe {
            slopos_slibc::process::ids::setresgid(USER, USER, USER) == 0
                && slopos_slibc::process::ids::setresuid(USER, USER, USER) == 0
        };
        if to_user {
            let words = [cstr("peer_test"), cstr(CONNECT), cstr(NAME)];
            let argv = [
                words[0].as_ptr(),
                words[1].as_ptr(),
                words[2].as_ptr(),
                core::ptr::null(),
            ];
            let path = cstr("/bin/peer_test");
            process::execve(path.as_ptr(), argv.as_ptr(), core::ptr::null());
        }
        sys_core::exit_with_code(99);
    }
    if pid <= 0 {
        return false;
    }
    let Some(conn) = accept_from(&listener, pid) else {
        return false;
    };
    let seen = net::peer_cred(conn.raw());
    let program = net::peer_program(conn.raw());
    let report = read_report(conn.raw());
    let _ = send_all(conn.raw(), b"x");
    let code = process::wait_exit_code(pid as u32);
    let child = UCred {
        pid,
        uid: USER,
        gid: USER,
    };
    let ok = seen == Ok(child)
        && is_self(program)
        && report == Some((pid as u32, own_cred()))
        && code == 0;
    if !ok {
        eprintln!("peer_test: server saw {seen:?}, child reported {report:?}, child exited {code}");
    }
    ok
}

/// A connection handed to a spawned child still names the process that
/// connected it, and once that has exited, no program.
fn inherited_connection_keeps_its_connector() -> bool {
    const NAME: &str = "/tmp/peer-e2e-hand";
    let Some(listener) = listen(NAME) else {
        eprintln!("peer_test: could not listen");
        return false;
    };
    let connector = spawn(SELF, &["peer_test", HAND, NAME], None);
    if connector <= 0 {
        eprintln!("peer_test: spawning the connector returned {connector}");
        return false;
    }
    let Some(conn) = accept_from(&listener, connector) else {
        return false;
    };
    let handed = process::wait_exit_code(connector as u32);
    let mut speaker = [0u8; 4];
    let spoke = recv_exact(conn.raw(), &mut speaker);
    let speaker = u32::from_ne_bytes(speaker);
    let seen = net::peer_cred(conn.raw());
    let program = net::peer_program(conn.raw());
    let pidfd = net::peer_pidfd(conn.raw());
    let _ = send_all(conn.raw(), b"x");
    let gone = pidfd
        .as_ref()
        .is_ok_and(|pidfd| readable_within(pidfd.raw(), 0));
    let ok = handed == 0
        && spoke
        && speaker != connector as u32
        && seen.is_ok_and(|cred| cred.pid == connector)
        && matches!(program, Ok(None))
        && gone;
    if !ok {
        eprintln!(
            "peer_test: connector {connector} exited {handed}, speaker {speaker}, \
             server saw {seen:?} running {program:?}, its pidfd readable: {gone}"
        );
    }
    ok
}

/// A connection names its connector's program until the connector execs, even
/// into the same program, after which it names no program while the ids and
/// the pid stand.
fn an_exec_after_a_connect_ends_the_program() -> bool {
    const NAME: &str = "/tmp/peer-e2e-exec";
    let Some(listener) = listen(NAME) else {
        eprintln!("peer_test: could not listen");
        return false;
    };
    let child = spawn(SELF, &["peer_test", EXEC_AFTER, NAME], None);
    if child <= 0 {
        eprintln!("peer_test: spawning the connector returned {child}");
        return false;
    }
    let Some(conn) = accept_from(&listener, child) else {
        return false;
    };
    let before = net::peer_program(conn.raw());
    let _ = send_all(conn.raw(), b"x");
    let mut spoke = [0u8; 4];
    let exec_d = recv_exact(conn.raw(), &mut spoke) && u32::from_ne_bytes(spoke) == child as u32;
    let after = net::peer_program(conn.raw());
    let seen = net::peer_cred(conn.raw());
    let _ = send_all(conn.raw(), b"x");
    let code = process::wait_exit_code(child as u32);
    let ok = is_self(before.clone())
        && exec_d
        && matches!(after, Ok(None))
        && seen.is_ok_and(|cred| cred.pid == child)
        && code == 0;
    if !ok {
        eprintln!(
            "peer_test: connected as {before:?}, after its exec ({exec_d}) {after:?}, \
             {seen:?}, exited {code}"
        );
    }
    ok
}

/// The `EXEC_AFTER` child: connect, wait for the word, then become a `SPEAK`
/// child on the connection.
fn connect_then_exec(name: &str) -> i32 {
    let Ok(fd) = net::socket(slopos_abi::net::AF_UNIX, slopos_abi::net::SOCK_STREAM, 0) else {
        return 2;
    };
    let mut go = [0u8; 1];
    if net::connect_unix(fd.raw(), name.as_bytes()).is_err()
        || !recv_exact(fd.raw(), &mut go)
        || fs::dup2(fd.raw(), 3).is_err()
    {
        return 3;
    }
    let words = [cstr("peer_test"), cstr(SPEAK)];
    let argv = [words[0].as_ptr(), words[1].as_ptr(), core::ptr::null()];
    let path = cstr("/bin/peer_test");
    process::execve(path.as_ptr(), argv.as_ptr(), core::ptr::null());
    4
}

/// A peer's pidfd is close-on-exec, quiet while the peer runs and readable
/// once it exits.
fn peer_pidfd_polls_on_exit() -> bool {
    const NAME: &str = "/tmp/peer-e2e-pidfd";
    let Some(listener) = listen(NAME) else {
        eprintln!("peer_test: could not listen");
        return false;
    };
    let child = spawn(SELF, &["peer_test", CONNECT, NAME], None);
    if child <= 0 {
        return false;
    }
    let Some(conn) = accept_from(&listener, child) else {
        return false;
    };
    let _ = read_report(conn.raw());
    let Ok(pidfd) = net::peer_pidfd(conn.raw()) else {
        eprintln!("peer_test: no pidfd for a connected peer");
        let _ = process::waitpid(child as u32);
        return false;
    };
    let quiet = !readable_within(pidfd.raw(), 0);
    let _ = send_all(conn.raw(), b"x");
    let exited = readable_within(pidfd.raw(), 5000);
    let code = process::wait_exit_code(child as u32);
    let ok = cloexec(pidfd.raw()) && quiet && exited && code == 0;
    if !ok {
        eprintln!("peer_test: pidfd quiet {quiet}, readable on exit {exited}, child exited {code}");
    }
    ok
}

/// The same program copied out of the base connects as no program at all.
fn a_program_outside_the_base_is_other() -> bool {
    const NAME: &str = "/tmp/peer-e2e-other";
    let copied = std::fs::copy("/bin/peer_test", COPY).is_ok()
        && std::fs::set_permissions(COPY, std::fs::Permissions::from_mode(0o755)).is_ok();
    if !copied {
        eprintln!("peer_test: could not copy this program out of the base");
        return false;
    }
    let Some(listener) = listen(NAME) else {
        eprintln!("peer_test: could not listen");
        return false;
    };
    let child = spawn(COPY.as_bytes(), &["peer_test", CONNECT, NAME], None);
    if child <= 0 {
        eprintln!("peer_test: spawning the copy returned {child}");
        return false;
    }
    let Some(conn) = accept_from(&listener, child) else {
        return false;
    };
    let program = net::peer_program(conn.raw());
    let seen = net::peer_cred(conn.raw());
    let _ = read_report(conn.raw());
    let _ = send_all(conn.raw(), b"x");
    let code = process::wait_exit_code(child as u32);
    let _ = std::fs::remove_file(COPY);
    let ok = matches!(program, Ok(None)) && seen.is_ok_and(|cred| cred.pid == child) && code == 0;
    if !ok {
        eprintln!("peer_test: the copy connected as {program:?}, {seen:?}, exited {code}");
    }
    ok
}

const EXTENSION: &str = "peer-test";
const EXTENSION_SELF: &str = "/opt/peer-test/bin/peer_test";

/// This program, activated as an extension, connects as that extension's:
/// its path under `/opt` and the name and digest it was activated as. Taken
/// away again, the path shows nothing.
fn an_extension_program_names_its_activation() -> bool {
    const NAME: &str = "/tmp/peer-e2e-extension";
    let image = match std::fs::read("/bin/peer_test").map(|program| extension_of(&program)) {
        Ok(Some(image)) => image,
        other => {
            eprintln!("peer_test: could not pack this program: {:?}", other.err());
            return false;
        }
    };
    const IMAGE: &str = "/tmp/peer-e2e-extension.ext";
    let Ok(digest) = slopos_extension_core::image::root_of(&mut &image[..]).map(|(_, root)| root)
    else {
        eprintln!("peer_test: the packed image has no root");
        return false;
    };
    let file = std::fs::write(IMAGE, &image).and_then(|()| std::fs::File::open(IMAGE));
    let _ = std::fs::remove_file(IMAGE);
    let activated = file.map_err(|e| e.to_string()).and_then(|file| {
        use std::os::fd::AsRawFd;
        base::ext_activate(EXTENSION.as_bytes(), file.as_raw_fd(), &digest)
            .map_err(|e| e.to_string())
    });
    if let Err(e) = activated {
        eprintln!("peer_test: ext_activate: {e}");
        return false;
    }
    let outcome = (|| {
        let listener = listen(NAME).ok_or("could not listen")?;
        let child = spawn(
            EXTENSION_SELF.as_bytes(),
            &["peer_test", CONNECT, NAME],
            None,
        );
        if child <= 0 {
            return Err(format!("spawning the extension's program returned {child}"));
        }
        let conn =
            accept_from(&listener, child).ok_or("the extension's program did not connect")?;
        let program = net::peer_program(conn.raw());
        let extension = net::peer_extension(conn.raw());
        let base = net::peer_base_program(conn.raw());
        let _ = read_report(conn.raw());
        let _ = send_all(conn.raw(), b"x");
        let code = process::wait_exit_code(child as u32);
        let named = program
            .as_ref()
            .is_ok_and(|p| p.as_deref() == Some(EXTENSION_SELF.as_bytes()));
        let labelled = extension.as_ref().is_ok_and(|label| {
            label.is_some_and(|l| l.name() == EXTENSION.as_bytes() && l.digest == digest)
        });
        if !named || !labelled || !matches!(base, Ok(None)) || code != 0 {
            return Err(format!(
                "connected as {program:?} of {extension:?}, base {base:?}, exited {code}"
            ));
        }
        Ok(())
    })();
    let taken = base::ext_deactivate(EXTENSION.as_bytes());
    let gone = std::fs::metadata(EXTENSION_SELF).is_err();
    if let Err(why) = outcome {
        eprintln!("peer_test: {why}");
        return false;
    }
    if taken.is_err() || !gone {
        eprintln!("peer_test: the extension stayed: {taken:?}");
        return false;
    }
    true
}

/// `program` as an extension's `bin/peer_test`.
fn extension_of(program: &[u8]) -> Option<Vec<u8>> {
    let mut packer = slopos_extension_core::image::Packer::default();
    packer.add(
        "/bin/peer_test",
        slopos_abi::fs::S_IFREG | 0o755,
        program.to_vec(),
    );
    packer
        .finish(&format!("name {EXTENSION}\nversion 1\n"), "")
        .ok()
}

/// Mapping memory executable that the base did not supply ends a process's
/// claim to be this program.
fn foreign_code_forfeits_the_program() -> bool {
    let pid = process::fork();
    if pid == 0 {
        let ok = self_attested() && forfeit() && !self_attested();
        sys_core::exit_with_code(if ok { 0 } else { 1 });
    }
    pid > 0 && process::wait_exit_code(pid as u32) == 0
}

/// A ring's `OP_CONNECT` attests the process that submitted it, as the
/// syscall does, not the kernel that ran it.
fn ring_connect_attests_its_submitter() -> bool {
    use slopos_userland::ring::{Ring, slopfut};
    const NAME: &str = "/tmp/peer-e2e-ring";
    let Some(listener) = listen(NAME) else {
        eprintln!("peer_test: could not listen");
        return false;
    };
    let Ok(client) = net::socket(slopos_abi::net::AF_UNIX, slopos_abi::net::SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        eprintln!("peer_test: no ring");
        return false;
    };
    let res = slopfut::block_on(ring, slopfut::connect_unix(client.raw(), NAME.as_bytes()));
    if res != 0 {
        eprintln!("peer_test: the ring's connect failed ({res})");
        return false;
    }
    let Ok(conn) = net::accept(listener.raw(), None) else {
        eprintln!("peer_test: the ring's connect reached no listener ({res})");
        return false;
    };
    let seen = net::peer_cred(conn.raw());
    let ok = res == 0 && seen == Ok(own_cred()) && is_self(net::peer_program(conn.raw()));
    if !ok {
        eprintln!("peer_test: the ring connected as {seen:?} ({res})");
    }
    ok
}

/// A ring's connect that waits for room in a full backlog attests who connects
/// when it does: the ids change while it waits, within one `ring_enter`.
fn a_waiting_ring_connect_attests_as_of_its_connect() -> bool {
    use slopos_userland::ring::{Ring, slopfut};
    const NAME: &str = "/tmp/peer-e2e-ring-wait";
    const BACKLOG: usize = 32;
    let pid = process::fork();
    if pid != 0 {
        return pid > 0 && reaped_within_a_minute(pid) == Some(0);
    }
    let stream = || net::socket(slopos_abi::net::AF_UNIX, slopos_abi::net::SOCK_STREAM, 0);
    let Some(listener) = listen(NAME) else {
        sys_core::exit_with_code(2);
    };
    let queued: Vec<OwnedFd> = (0..BACKLOG)
        .filter_map(|_| stream().ok())
        .filter(|fd| net::connect_unix(fd.raw(), NAME.as_bytes()).is_ok())
        .collect();
    let (Ok(client), Ok(ring), BACKLOG) = (stream(), Ring::setup(8), queued.len()) else {
        sys_core::exit_with_code(3);
    };
    let raw_listener = listener.raw();
    let changer = std::thread::spawn(move || {
        let past_a_reprobe = std::time::Duration::from_millis(200);
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let changed = unsafe { slopos_slibc::process::ids::setresuid(USER, USER, USER) == 0 };
        std::thread::sleep(past_a_reprobe);
        net::accept(raw_listener, None).is_ok() && changed
    });
    let res = slopfut::block_on(ring, slopfut::connect_unix(client.raw(), NAME.as_bytes()));
    let changed = changer.join().unwrap_or(false);
    let last = (res == 0)
        .then(|| {
            (0..BACKLOG)
                .filter_map(|_| net::accept(raw_listener, None).ok())
                .last()
        })
        .flatten();
    let seen = last.map(|conn| net::peer_cred(conn.raw()));
    let ok = changed && matches!(&seen, Some(Ok(cred)) if cred.uid == USER);
    if !ok {
        eprintln!(
            "peer_test: a waiting ring connect ({res}, ids changed {changed}) named {seen:?}"
        );
    }
    drop(queued);
    sys_core::exit_with_code(if ok { 0 } else { 1 });
}

/// A forfeit reaches a child forked after it and not one forked before, nor
/// the parent of the process that forfeited.
fn a_forfeit_follows_fork_order() -> bool {
    let me = process::fork();
    if me == 0 {
        let Ok((go_read, go_write)) = fs::pipe() else {
            sys_core::exit_with_code(2);
        };
        let before = process::fork();
        if before == 0 {
            drop(go_write);
            let mut go = [0u8; 1];
            let _ = Sys::read(go_read.raw(), go.as_mut_ptr(), 1);
            sys_core::exit_with_code(if self_attested() { 0 } else { 1 });
        }
        drop(go_read);
        if !forfeit() {
            sys_core::exit_with_code(3);
        }
        drop(go_write);
        let after = process::fork();
        if after == 0 {
            sys_core::exit_with_code(if self_attested() { 1 } else { 0 });
        }
        let ok = before > 0
            && after > 0
            && process::wait_exit_code(before as u32) == 0
            && process::wait_exit_code(after as u32) == 0;
        sys_core::exit_with_code(if ok { 0 } else { 1 });
    }
    let child = me > 0 && process::wait_exit_code(me as u32) == 0;
    if !child {
        eprintln!("peer_test: a forfeit did not follow fork order");
    }
    child && self_attested()
}

/// Whether a pair this process makes names it as this program.
fn self_attested() -> bool {
    net::socketpair(0).is_ok_and(|(a, _b)| is_self(net::peer_program(a.raw())))
}

/// Map anonymous memory executable, which the base did not supply.
fn forfeit() -> bool {
    Sys::mmap(
        core::ptr::null_mut(),
        4096,
        PROT_READ | PROT_EXEC,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    )
    .is_ok()
}

/// `pidfd_open` takes a process that is no child of the caller and refuses a
/// thread that leads none, and every pidfd is close-on-exec.
fn pidfd_open_takes_any_leader() -> bool {
    use slopos_userland::syscall::pidfd;
    let (Ok((pid_read, pid_write)), Ok((hold_read, hold_write))) = (fs::pipe(), fs::pipe()) else {
        return false;
    };
    let middle = process::fork();
    if middle == 0 {
        drop(pid_read);
        drop(hold_write);
        let grandchild = spawn(SELF, &["peer_test", HOLD], Some(hold_read.raw()));
        let sent = grandchild > 0
            && Sys::write(pid_write.raw(), grandchild.to_ne_bytes().as_ptr(), 4) == Ok(4);
        sys_core::exit_with_code(if sent { 0 } else { 1 });
    }
    drop(pid_write);
    drop(hold_read);
    let mut raw = [0u8; 4];
    let read = Sys::read(pid_read.raw(), raw.as_mut_ptr(), 4) == Ok(4);
    let middle_ok = middle > 0 && process::wait_exit_code(middle as u32) == 0;
    let stranger = i32::from_ne_bytes(raw);
    let Some(pidfd) = pidfd::pidfd_open_owned(stranger as u32).filter(|_| read && middle_ok) else {
        eprintln!("peer_test: no pidfd for a process that is not this one's child");
        return false;
    };
    let quiet = !readable_within(pidfd.raw(), 0);
    drop(hold_write);
    let exited = readable_within(pidfd.raw(), 5000);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let (tx, rx) = std::sync::mpsc::channel();
    let held = barrier.clone();
    let thread = std::thread::spawn(move || {
        let _ = tx.send(Sys::gettid());
        held.wait();
    });
    let tid = rx.recv().unwrap_or(0);
    let refused = pidfd::pidfd_open(tid as u32) == -22;
    barrier.wait();
    let _ = thread.join();

    let ok = cloexec(pidfd.raw()) && quiet && exited && refused;
    if !ok {
        eprintln!(
            "peer_test: pidfd quiet {quiet}, readable on exit {exited}, thread refused {refused}"
        );
    }
    ok
}

/// The base's own code mapped executable keeps the program, and a buffer too
/// short for its path is told the length it needs.
fn base_code_keeps_the_program() -> bool {
    use std::os::fd::AsRawFd;
    let pid = process::fork();
    if pid == 0 {
        let Ok(image) = std::fs::File::open("/bin/peer_test") else {
            sys_core::exit_with_code(2);
        };
        let code = Sys::mmap(
            core::ptr::null_mut(),
            4096,
            PROT_READ | PROT_EXEC,
            MAP_PRIVATE,
            image.as_raw_fd(),
            0,
        );
        let reprotected =
            code.is_ok_and(|code| Sys::mprotect(code, 4096, PROT_READ | PROT_EXEC).is_ok());
        let Ok((a, _b)) = net::socketpair(0) else {
            sys_core::exit_with_code(3);
        };
        let mut short = [0u8; 4];
        let mut len = short.len() as u32;
        let sized = Sys::getsockopt(
            a.raw(),
            slopos_abi::syscall::SOL_SLOPOS,
            slopos_abi::syscall::SO_PEERPROGRAM,
            short.as_mut_ptr(),
            &mut len,
        );
        let ok = reprotected
            && is_self(net::peer_program(a.raw()))
            && sized == Err(slopos_slibc::errno::ERANGE)
            && len as usize == SELF.len();
        sys_core::exit_with_code(if ok { 0 } else { 1 });
    }
    pid > 0 && process::wait_exit_code(pid as u32) == 0
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("socketpair_names_its_creator", socketpair_names_its_creator),
    (
        "connected_pair_names_each_other",
        connected_pair_names_each_other,
    ),
    (
        "inherited_connection_keeps_its_connector",
        inherited_connection_keeps_its_connector,
    ),
    (
        "an_exec_after_a_connect_ends_the_program",
        an_exec_after_a_connect_ends_the_program,
    ),
    ("peer_pidfd_polls_on_exit", peer_pidfd_polls_on_exit),
    (
        "a_program_outside_the_base_is_other",
        a_program_outside_the_base_is_other,
    ),
    (
        "foreign_code_forfeits_the_program",
        foreign_code_forfeits_the_program,
    ),
    ("base_code_keeps_the_program", base_code_keeps_the_program),
    (
        "ring_connect_attests_its_submitter",
        ring_connect_attests_its_submitter,
    ),
    (
        "a_waiting_ring_connect_attests_as_of_its_connect",
        a_waiting_ring_connect_attests_as_of_its_connect,
    ),
    ("a_forfeit_follows_fork_order", a_forfeit_follows_fork_order),
    (
        "an_extension_program_names_its_activation",
        an_extension_program_names_its_activation,
    ),
    ("pidfd_open_takes_any_leader", pidfd_open_takes_any_leader),
];

/// The `CONNECT` child: connect, report, wait for the word to go.
fn connect_and_report(name: &str) -> i32 {
    let Ok(fd) = net::socket(slopos_abi::net::AF_UNIX, slopos_abi::net::SOCK_STREAM, 0) else {
        return 2;
    };
    if net::connect_unix(fd.raw(), name.as_bytes()).is_err() {
        return 3;
    }
    let listener = net::peer_cred(fd.raw()).unwrap_or(UCred::NONE);
    let mut report = [0u8; 16];
    report[..4].copy_from_slice(&process::getpid().to_ne_bytes());
    report[4..].copy_from_slice(&listener.to_bytes());
    let mut go = [0u8; 1];
    if !send_all(fd.raw(), &report) || !recv_exact(fd.raw(), &mut go) {
        return 4;
    }
    0
}

/// The `HAND` child: connect and give the connection to a `SPEAK` child.
fn connect_and_hand(name: &str) -> i32 {
    let Ok(fd) = net::socket(slopos_abi::net::AF_UNIX, slopos_abi::net::SOCK_STREAM, 0) else {
        return 2;
    };
    if net::connect_unix(fd.raw(), name.as_bytes()).is_err() {
        return 3;
    }
    if spawn(SELF, &["peer_test", SPEAK], Some(fd.raw())) <= 0 {
        return 4;
    }
    0
}

/// The `SPEAK` child: its pid on fd 3, then wait for the word to go.
fn speak() -> i32 {
    let mut go = [0u8; 1];
    if !send_all(3, &process::getpid().to_ne_bytes()) || !recv_exact(3, &mut go) {
        return 2;
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(2).map_or("", String::as_str);
    match args.get(1).map(String::as_str) {
        Some(CONNECT) => std::process::exit(connect_and_report(name)),
        Some(HAND) => std::process::exit(connect_and_hand(name)),
        Some(SPEAK) => std::process::exit(speak()),
        Some(EXEC_AFTER) => std::process::exit(connect_then_exec(name)),
        Some(HOLD) => {
            let mut end = [0u8; 1];
            let _ = Sys::read(3, end.as_mut_ptr(), 1);
            std::process::exit(0)
        }
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
