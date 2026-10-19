//! Peer identity through the real syscall entry: `socketpair`, `SO_PEERCRED`,
//! `SO_PEERPIDFD` and `SO_PEERPROGRAM` as captured at `listen`, `connect` and
//! `socketpair`, `pidfd_open` on a process that is no child, and the program
//! identity exec attests and an executable mapping takes away. Each refusal
//! sits beside the positive control that shows the same caller succeeding.

use core::num::NonZeroU64;

use slopos_abi::Errno;
use slopos_abi::fs::{AT_FDCWD, O_RDONLY};
use slopos_abi::net::{AF_INET, AF_UNIX, SOCK_CLOEXEC, SOCK_DGRAM, SOCK_STREAM};
use slopos_abi::syscall::{
    F_GETFD, FD_CLOEXEC, IPPROTO_TCP, MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED, POLLIN, PROT_EXEC,
    PROT_READ, PROT_WRITE, SO_ERROR, SO_PEERCRED, SO_PEEREXTENSION, SO_PEERPIDFD, SO_PEERPROGRAM,
    SOL_SLOPOS, SOL_SOCKET, UCred,
};
use slopos_fs::fileio::fileio_get_handle_and_ops;
use slopos_mm::user_copy::copy_from_user;
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::authority::CAP_UNIVERSAL;
use slopos_ostd::process::Program;
use slopos_testing::{TestResult, assert_eq_test, assert_test, fail, pass};

use crate::syscall::common::SyscallHandler;
use crate::syscall::memory_handlers::{syscall_mmap, syscall_mprotect};
use crate::syscall::net_handlers::{
    syscall_accept, syscall_bind, syscall_connect, syscall_getsockopt, syscall_listen,
    syscall_socket, syscall_socketpair,
};
use crate::syscall::tests_build_floor_fs::{
    PageSetRegistry, stage_cstr, with_user_process_context,
};
use crate::syscall::tests_cred::Principal;

type SyscallFixture = slopos_sched::test_fixture::KernelTestScope;

const USER: u32 = 1000;
const OTHER: u32 = 2000;
const THIRD: u32 = 3000;

/// Where in a principal's page each argument is staged.
const SV: u64 = 0;
const OPTLEN: u64 = 64;
const OPTVAL: u64 = 128;
const ADDR: u64 = 1024;
const DATA: u64 = 2048;

fn errno(e: Errno) -> Option<i64> {
    Some(i64::from(e.raw()))
}

fn read_user<T: Copy>(who: &Principal, addr: u64) -> Option<T> {
    with_user_process_context(who.table, || {
        UserPtr::<T>::try_new(addr)
            .ok()
            .and_then(|ptr| copy_from_user(ptr).ok())
    })?
}

fn pid_of(who: &Principal) -> u32 {
    crate::syscall::context::identity_of(&who.task).pid
}

fn set_program(who: &Principal, program: Program) {
    if let Some(process) = who.task.process() {
        process.set_program(program);
    }
}

fn program_of(who: &Principal) -> Program {
    who.task
        .process()
        .map_or(Program::Other, |process| process.program())
}

/// A program to stand in for a base one: the rules under test read only
/// whether a process has one, never which. The base itself is installed after
/// the kernel phase, so its files are graded by `peer_test`.
const BASE: Program = Program::Base(NonZeroU64::MIN);

fn call(who: &Principal, handler: SyscallHandler, args: [u64; 6]) -> Option<i64> {
    who.call(handler, args)
}

fn socket(who: &Principal) -> Option<i64> {
    call(
        who,
        syscall_socket,
        [AF_UNIX as u64, u64::from(SOCK_STREAM), 0, 0, 0, 0],
    )
}

/// `socketpair(AF_UNIX, SOCK_STREAM | extra, 0)`: its return, and the two
/// descriptors it wrote.
fn socketpair(who: &Principal, extra: u32) -> Option<(i64, [i32; 2])> {
    let rc = call(
        who,
        syscall_socketpair,
        [
            AF_UNIX as u64,
            u64::from(u32::from(SOCK_STREAM) | extra),
            0,
            who.page + SV,
            0,
            0,
        ],
    )?;
    Some((rc, read_user::<[i32; 2]>(who, who.page + SV)?))
}

/// `sockaddr_un` for `name`, staged at [`ADDR`]; answers its length.
fn stage_unix_addr(who: &Principal, name: &[u8]) -> Option<u64> {
    let mut addr = [0u8; 64];
    addr[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
    addr[2..2 + name.len()].copy_from_slice(name);
    stage_cstr(who.table, who.page + ADDR, &addr[..2 + name.len()]).then_some(2 + name.len() as u64)
}

/// A socket listening at `name`, which anyone may connect to.
fn listening(who: &Principal, name: &[u8]) -> Option<i64> {
    slopos_fs::vfs::vfs_init_builtin_filesystems().ok()?;
    let _ = slopos_fs::vfs::vfs_unlink(name);
    let fd = socket(who)?;
    let len = stage_unix_addr(who, name)?;
    let bound = call(
        who,
        syscall_bind,
        [fd as u64, who.page + ADDR, len, 0, 0, 0],
    )?;
    let opened = slopos_fs::vfs::vfs_set_mode(name, 0o666).is_ok();
    let listened = call(who, syscall_listen, [fd as u64, 4, 0, 0, 0, 0])?;
    (fd >= 0 && bound == 0 && opened && listened == 0).then_some(fd)
}

fn connected(who: &Principal, name: &[u8]) -> Option<i64> {
    let fd = socket(who)?;
    let len = stage_unix_addr(who, name)?;
    let rc = call(
        who,
        syscall_connect,
        [fd as u64, who.page + ADDR, len, 0, 0, 0],
    )?;
    (fd >= 0 && rc == 0).then_some(fd)
}

fn accept(who: &Principal, listener: i64) -> Option<i64> {
    call(who, syscall_accept, [listener as u64, 0, 0, 0, 0, 0]).filter(|&fd| fd >= 0)
}

/// `getsockopt` with `len` as the buffer's size: its return, the length it
/// reported, and the bytes it wrote.
fn getsockopt(
    who: &Principal,
    fd: i64,
    level: i32,
    name: i32,
    len: u32,
) -> Option<(i64, u32, [u8; 64])> {
    if !stage_cstr(who.table, who.page + OPTLEN, &len.to_ne_bytes())
        || !stage_cstr(who.table, who.page + OPTVAL, &[0u8; 64])
    {
        return None;
    }
    let rc = call(
        who,
        syscall_getsockopt,
        [
            fd as u64,
            level as u64,
            name as u64,
            who.page + OPTVAL,
            who.page + OPTLEN,
            0,
        ],
    )?;
    Some((
        rc,
        read_user::<u32>(who, who.page + OPTLEN)?,
        read_user::<[u8; 64]>(who, who.page + OPTVAL)?,
    ))
}

fn peer_cred(who: &Principal, fd: i64) -> Option<UCred> {
    match getsockopt(who, fd, SOL_SOCKET, SO_PEERCRED, 12)? {
        (0, 12, value) => Some(UCred::from_bytes(value[..12].try_into().ok()?)),
        _ => None,
    }
}

fn ucred(pid: u32, uid: u32, gid: u32) -> UCred {
    UCred {
        pid: pid as i32,
        uid,
        gid,
    }
}

fn close(who: &Principal, fd: i64) {
    let _ = slopos_fs::fileio::file_close_fd(who.table, fd as i32);
}

fn cloexec(who: &Principal, fd: i64) -> bool {
    call(
        who,
        crate::syscall::fs::syscall_fcntl,
        [fd as u64, F_GETFD, 0, 0, 0, 0],
    )
    .is_some_and(|flags| flags >= 0 && flags as u64 & FD_CLOEXEC != 0)
}

fn readable(who: &Principal, fd: i64) -> bool {
    fileio_get_handle_and_ops(who.table, fd as i32)
        .is_some_and(|(handle, ops)| ops.poll_events(handle, POLLIN) & POLLIN != 0)
}

pub fn test_socketpair_attests_its_creator_to_both_ends() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(user) = Principal::with_uids([USER, OTHER, USER], CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let Some((rc, [a, b])) = socketpair(&user, SOCK_CLOEXEC) else {
        return fail!("could not run socketpair");
    };
    assert_eq_test!(rc, 0, "socketpair");
    assert_test!(a >= 0 && b >= 0 && a != b, "two descriptors");
    let creator = ucred(pid_of(&user), OTHER, USER);
    assert_eq_test!(
        peer_cred(&user, a.into()),
        Some(creator),
        "one end names the creator's pid and effective ids"
    );
    assert_eq_test!(
        peer_cred(&user, b.into()),
        Some(creator),
        "so does the other"
    );
    assert_test!(
        cloexec(&user, a.into()) && cloexec(&user, b.into()),
        "SOCK_CLOEXEC reaches both descriptors"
    );

    assert_test!(
        stage_cstr(user.table, user.page + DATA, b"ping"),
        "could not stage the payload"
    );
    assert_eq_test!(
        call(
            &user,
            crate::syscall::fs::syscall_write,
            [a as u64, user.page + DATA, 4, 0, 0, 0]
        ),
        Some(4),
        "write into one end"
    );
    assert_eq_test!(
        call(
            &user,
            crate::syscall::fs::syscall_read,
            [b as u64, user.page + DATA + 64, 4, 0, 0, 0]
        ),
        Some(4),
        "read it from the other"
    );
    assert_eq_test!(
        read_user::<[u8; 4]>(&user, user.page + DATA + 64),
        Some(*b"ping"),
        "the bytes that went in come out"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_socketpair_attests_its_creator_to_both_ends,
    suite = peer
);

/// What `socketpair` refuses, as Linux would or, for a datagram pair, as
/// `socket` does here; none leaves a descriptor behind, so the next pair
/// takes the very numbers the first did.
pub fn test_socketpair_refuses_what_linux_refuses() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let sv = user.page + SV;
    let pair = |domain: u64, ty: u64, protocol: u64, at: u64| {
        call(&user, syscall_socketpair, [domain, ty, protocol, at, 0, 0])
    };
    let stream = u64::from(SOCK_STREAM);
    assert_eq_test!(
        pair(AF_INET as u64, stream, 0, sv),
        errno(Errno::EOPNOTSUPP),
        "AF_INET has no pairs"
    );
    assert_eq_test!(
        pair(AF_INET as u64, stream, 17, sv),
        errno(Errno::EPROTONOSUPPORT),
        "but is asked for a protocol it has first"
    );
    assert_eq_test!(
        pair(99, stream, 0, sv),
        errno(Errno::EAFNOSUPPORT),
        "an unknown family"
    );
    assert_eq_test!(
        pair(AF_UNIX as u64, u64::from(SOCK_DGRAM), 0, sv),
        errno(Errno::ESOCKTNOSUPPORT),
        "AF_UNIX datagrams, which `socket` does not serve either"
    );
    assert_eq_test!(
        pair(AF_UNIX as u64, stream, 6, sv),
        errno(Errno::EPROTONOSUPPORT),
        "a protocol AF_UNIX does not have"
    );
    assert_eq_test!(
        pair(AF_UNIX as u64, stream | 0x10_0000, 0, sv),
        errno(Errno::EINVAL),
        "a flag that is not SOCK_NONBLOCK or SOCK_CLOEXEC"
    );

    let Some((0, first)) = socketpair(&user, 0) else {
        return fail!("the positive control's socketpair failed");
    };
    close(&user, first[0].into());
    close(&user, first[1].into());
    assert_eq_test!(
        pair(AF_UNIX as u64, stream, 0, 0x10),
        errno(Errno::EFAULT),
        "an unwritable sv"
    );
    let Some((0, again)) = socketpair(&user, 0) else {
        return fail!("socketpair after the fault failed");
    };
    assert_eq_test!(again, first, "the refused call kept no descriptor");
    pass!()
}

slopos_testing::stest!(
    name = test_socketpair_refuses_what_linux_refuses,
    suite = peer
);

/// The client is told who listened, the server who connected, each as of
/// that call: a change of ids afterwards reaches a new connection only.
pub fn test_peer_cred_is_captured_at_listen_and_connect() -> TestResult {
    use crate::syscall::cred_handlers::syscall_setresuid;
    const NAME: &[u8] = b"/tmp/peer-test-capture";
    let _fixture = SyscallFixture::new();
    let (Some(server), Some(client)) = (
        Principal::user(USER, CAP_UNIVERSAL),
        Principal::with_uids([OTHER, OTHER, THIRD], CAP_UNIVERSAL),
    ) else {
        return fail!("could not build the principals");
    };
    let Some(listener) = listening(&server, NAME) else {
        return fail!("could not listen");
    };
    let Some(outbound) = connected(&client, NAME) else {
        return fail!("could not connect");
    };
    let Some(inbound) = accept(&server, listener) else {
        return fail!("could not accept");
    };
    let server_cred = ucred(pid_of(&server), USER, USER);
    let client_cred = ucred(pid_of(&client), OTHER, OTHER);
    assert_eq_test!(
        peer_cred(&server, inbound),
        Some(client_cred),
        "the accepted socket names the connector"
    );
    assert_eq_test!(
        peer_cred(&client, outbound),
        Some(server_cred),
        "the connecting socket names the listener"
    );
    assert_eq_test!(
        peer_cred(&server, listener),
        Some(server_cred),
        "a listener names itself"
    );

    let keep = u64::from(u32::MAX);
    assert_eq_test!(
        call(
            &client,
            syscall_setresuid,
            [keep, u64::from(THIRD), keep, 0, 0, 0]
        ),
        Some(0),
        "the client moves to its saved uid"
    );
    assert_eq_test!(
        peer_cred(&server, inbound),
        Some(client_cred),
        "an established connection keeps what was captured"
    );
    let Some(again) = connected(&client, NAME) else {
        return fail!("could not connect again");
    };
    let Some(second) = accept(&server, listener) else {
        return fail!("could not accept again");
    };
    assert_eq_test!(
        peer_cred(&server, second),
        Some(ucred(pid_of(&client), THIRD, OTHER)),
        "a new connection carries the ids it was made with"
    );
    for fd in [listener, inbound, second] {
        close(&server, fd);
    }
    for fd in [outbound, again] {
        close(&client, fd);
    }
    pass!()
}

slopos_testing::stest!(
    name = test_peer_cred_is_captured_at_listen_and_connect,
    suite = peer
);

/// What Linux answers for a socket with no peer, and what a short or negative
/// length gets.
pub fn test_peer_options_without_a_peer() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let Some(lone) = socket(&user).filter(|&fd| fd >= 0) else {
        return fail!("could not make a socket");
    };
    assert_eq_test!(
        peer_cred(&user, lone),
        Some(UCred::NONE),
        "no peer reads as pid 0 and ids -1"
    );
    assert_eq_test!(
        getsockopt(&user, lone, SOL_SOCKET, SO_PEERPIDFD, 4).map(|r| r.0),
        errno(Errno::ENODATA),
        "no peer has no pidfd"
    );
    assert_eq_test!(
        getsockopt(&user, lone, SOL_SLOPOS, SO_PEERPROGRAM, 64).map(|r| r.0),
        errno(Errno::ENOTCONN),
        "no peer has no program"
    );
    assert_eq_test!(
        getsockopt(&user, lone, SOL_SOCKET, 999, 4).map(|r| r.0),
        errno(Errno::ENOPROTOOPT),
        "an option AF_UNIX does not have"
    );
    assert_eq_test!(
        getsockopt(&user, lone, IPPROTO_TCP, 1, 4).map(|r| r.0),
        errno(Errno::EOPNOTSUPP),
        "a level AF_UNIX does not have"
    );
    assert_eq_test!(
        getsockopt(&user, lone, SOL_SOCKET, SO_ERROR, 4).map(|r| (r.0, r.1, r.2[..4] == [0; 4])),
        Some((0, 4, true)),
        "and no error pending"
    );

    let Some((0, [a, _])) = socketpair(&user, 0) else {
        return fail!("the positive control's socketpair failed");
    };
    let pid = pid_of(&user) as i32;
    assert_eq_test!(
        getsockopt(&user, a.into(), SOL_SOCKET, SO_PEERCRED, 4).map(|r| (
            r.0,
            r.1,
            r.2[..4] == pid.to_ne_bytes()
        )),
        Some((0, 4, true)),
        "a short buffer takes the pid alone"
    );
    assert_eq_test!(
        getsockopt(&user, a.into(), SOL_SOCKET, SO_PEERCRED, u32::MAX).map(|r| r.0),
        errno(Errno::EINVAL),
        "a negative length"
    );
    pass!()
}

slopos_testing::stest!(name = test_peer_options_without_a_peer, suite = peer);

/// A peer's pidfd names the process that connected, polls readable once it
/// is gone, and is still handed out after.
pub fn test_peer_pidfd_names_the_peer_and_outlives_it() -> TestResult {
    const NAME: &[u8] = b"/tmp/peer-test-pidfd";
    let _fixture = SyscallFixture::new();
    let Some(server) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the server");
    };
    let Some(client) = Principal::user(OTHER, CAP_UNIVERSAL) else {
        return fail!("could not build the client");
    };
    let Some(listener) = listening(&server, NAME) else {
        return fail!("could not listen");
    };
    let Some(outbound) = connected(&client, NAME) else {
        return fail!("could not connect");
    };
    let Some(inbound) = accept(&server, listener) else {
        return fail!("could not accept");
    };
    let pidfd = |who: &Principal| -> Option<i64> {
        match getsockopt(who, inbound, SOL_SOCKET, SO_PEERPIDFD, 4)? {
            (0, 4, value) => Some(i64::from(i32::from_ne_bytes(value[..4].try_into().ok()?))),
            _ => None,
        }
    };
    let Some(live) = pidfd(&server).filter(|&fd| fd >= 0) else {
        return fail!("no pidfd for a connected peer");
    };
    assert_test!(cloexec(&server, live), "a peer's pidfd is close-on-exec");
    assert_test!(!readable(&server, live), "the peer is still running");

    close(&client, outbound);
    drop(client);
    assert_test!(
        readable(&server, live),
        "the peer exited and its pidfd says so"
    );
    let Some(stale) = pidfd(&server).filter(|&fd| fd >= 0) else {
        return fail!("no pidfd once the peer is gone");
    };
    assert_test!(
        readable(&server, stale),
        "a pidfd for a gone peer is readable at once"
    );
    for fd in [listener, inbound, live, stale] {
        close(&server, fd);
    }
    pass!()
}

slopos_testing::stest!(
    name = test_peer_pidfd_names_the_peer_and_outlives_it,
    suite = peer
);

/// Any process's leader may be opened, as on Linux; what is not one is not.
pub fn test_pidfd_open_names_any_leader() -> TestResult {
    use crate::syscall::pidfd_handlers::syscall_pidfd_open;
    let _fixture = SyscallFixture::new();
    let (Some(opener), Some(stranger)) = (
        Principal::user(USER, CAP_UNIVERSAL),
        Principal::user(OTHER, CAP_UNIVERSAL),
    ) else {
        return fail!("could not build the principals");
    };
    let open = |pid: u64| call(&opener, syscall_pidfd_open, [pid, 0, 0, 0, 0, 0]);
    let fd = open(u64::from(pid_of(&stranger)));
    assert_test!(
        fd.is_some_and(|fd| fd >= 0),
        "a process of another uid, no child of the caller"
    );
    assert_test!(
        fd.is_some_and(|fd| cloexec(&opener, fd)),
        "a pidfd is close-on-exec"
    );
    assert_eq_test!(open(0), errno(Errno::EINVAL), "pid 0");
    assert_eq_test!(
        open(u64::from(u32::MAX)),
        errno(Errno::EINVAL),
        "a negative pid"
    );
    assert_eq_test!(
        open(u64::from(i32::MAX as u32)),
        errno(Errno::ESRCH),
        "a pid no task has had"
    );
    if let Some(fd) = fd {
        close(&opener, fd);
    }
    pass!()
}

slopos_testing::stest!(name = test_pidfd_open_names_any_leader, suite = peer);

/// Code mapped executable from anywhere but the base ends the attestation;
/// memory that cannot run keeps it.
pub fn test_executable_mappings_outside_the_base_forfeit_the_program() -> TestResult {
    const FILE: &[u8] = b"/tmp/peer_test_code";
    let _fixture = SyscallFixture::new();
    let _registry = PageSetRegistry::publish();
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    set_program(&user, BASE);
    let anon = |prot: u64| {
        call(
            &user,
            syscall_mmap,
            [0, 4096, prot, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX, 0],
        )
    };
    let Some(data) = anon(PROT_READ | PROT_WRITE).filter(|&a| a > 0) else {
        return fail!("an anonymous mapping failed");
    };
    assert_eq_test!(program_of(&user), BASE, "memory that cannot run");
    assert_eq_test!(
        call(
            &user,
            syscall_mprotect,
            [data as u64, 4096, PROT_READ, 0, 0, 0]
        ),
        Some(0),
        "re-protecting it still unable to run"
    );
    assert_eq_test!(program_of(&user), BASE, "keeps the program");
    assert_eq_test!(
        call(
            &user,
            syscall_mprotect,
            [data as u64, 4096, PROT_READ | PROT_EXEC, 0, 0, 0]
        ),
        Some(0),
        "making written memory executable"
    );
    assert_eq_test!(program_of(&user), Program::Other, "forfeits the program");

    set_program(&user, BASE);
    assert_test!(
        anon(PROT_READ | PROT_EXEC).is_some_and(|a| a > 0),
        "an executable anonymous mapping"
    );
    assert_eq_test!(program_of(&user), Program::Other, "forfeits it as well");

    let _ = slopos_fs::vfs::vfs_unlink(FILE);
    let written = slopos_fs::vfs::vfs_open(FILE, true)
        .is_ok_and(|handle| handle.write(0, &[0xc3; 64]) == Ok(64));
    assert_test!(written, "could not write a file outside the base");
    assert_test!(
        stage_cstr(user.table, user.page + DATA, FILE),
        "could not stage the path"
    );
    let Some(fd) = call(
        &user,
        crate::syscall::fs::at_handlers::syscall_openat,
        [
            AT_FDCWD as i64 as u64,
            user.page + DATA,
            u64::from(O_RDONLY),
            0,
            0,
            0,
        ],
    )
    .filter(|&fd| fd >= 0) else {
        return fail!("could not open the file");
    };
    set_program(&user, BASE);
    assert_test!(
        call(
            &user,
            syscall_mmap,
            [0, 4096, PROT_READ, MAP_PRIVATE, fd as u64, 0]
        )
        .is_some_and(|a| a > 0),
        "a file mapped readable"
    );
    assert_eq_test!(program_of(&user), BASE, "keeps the program");
    assert_test!(
        call(
            &user,
            syscall_mmap,
            [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd as u64, 0]
        )
        .is_some_and(|a| a > 0),
        "the same file mapped executable"
    );
    assert_eq_test!(program_of(&user), Program::Other, "forfeits it");
    close(&user, fd);
    let _ = slopos_fs::vfs::vfs_unlink(FILE);

    set_program(&user, BASE);
    assert_test!(
        stage_cstr(user.table, user.page + DATA, b"code"),
        "could not stage the memfd's name"
    );
    let Some(memfd) = call(
        &user,
        crate::syscall::memory_handlers::syscall_memfd_create,
        [user.page + DATA, 0, 0, 0, 0, 0],
    )
    .filter(|&fd| fd >= 0) else {
        return fail!("could not make a memfd");
    };
    assert_eq_test!(
        call(
            &user,
            crate::syscall::memory_handlers::syscall_ftruncate,
            [memfd as u64, 4096, 0, 0, 0, 0]
        ),
        Some(0),
        "sizing the memfd"
    );
    assert_test!(
        call(
            &user,
            syscall_mmap,
            [0, 4096, PROT_READ | PROT_EXEC, MAP_SHARED, memfd as u64, 0]
        )
        .is_some_and(|a| a > 0),
        "a memfd mapped executable"
    );
    assert_eq_test!(program_of(&user), Program::Other, "forfeits it too");
    close(&user, memfd);
    pass!()
}

slopos_testing::stest!(
    name = test_executable_mappings_outside_the_base_forfeit_the_program,
    suite = peer
);

/// An exec with another thread alive attests nothing.
pub fn test_an_exec_beside_another_thread_attests_nothing() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let Some(process) = user.task.process() else {
        return fail!("the principal has no process");
    };
    set_program(&user, BASE);
    assert_test!(
        crate::exec::exec_attests(&user.task),
        "a sole thread's exec attests what it loads"
    );
    crate::exec::attest_exec(&user.task, BASE, true, false);
    assert_eq_test!(program_of(&user), BASE, "and takes it");

    process.task_join();
    let attests = crate::exec::exec_attests(&user.task);
    crate::exec::attest_exec(&user.task, BASE, attests, false);
    let loaded = program_of(&user);
    process.task_leave();
    assert_test!(!attests, "an exec beside a sibling does not attest");
    assert_eq_test!(loaded, Program::Other, "so what it loads is never taken");
    pass!()
}

slopos_testing::stest!(
    name = test_an_exec_beside_another_thread_attests_nothing,
    suite = peer
);

/// `ET_EXEC` with one `PT_LOAD` at the code base and its entry far past it:
/// whole enough to be charged and to reset the address space for, refused
/// only when it is mapped.
fn elf_entering_nowhere() -> [u8; 120] {
    let code = slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
    let mut elf = [0u8; 120];
    elf[0..4].copy_from_slice(b"\x7fELF");
    elf[4..7].copy_from_slice(&[2, 1, 1]);
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&0x3eu16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&(code + 0x50_0000).to_le_bytes());
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1u32.to_le_bytes());
    elf[68..72].copy_from_slice(&5u32.to_le_bytes());
    elf[80..88].copy_from_slice(&code.to_le_bytes());
    elf[88..96].copy_from_slice(&code.to_le_bytes());
    elf[96..104].copy_from_slice(&120u64.to_le_bytes());
    elf[104..112].copy_from_slice(&0x1000u64.to_le_bytes());
    elf[112..120].copy_from_slice(&0x1000u64.to_le_bytes());
    elf
}

fn stage_executable(path: &[u8], bytes: &[u8]) -> bool {
    use slopos_fs::vfs::Subject;
    let _ = slopos_fs::vfs::vfs_unlink(path);
    slopos_fs::vfs::vfs_open(path, true).is_ok_and(|file| file.write(0, bytes) == Ok(bytes.len()))
        && slopos_fs::fileio::file_chmod_at(
            path,
            b"/",
            0o755,
            slopos_fs::vfs::path::RESOLVE_FOLLOW,
            &Subject::KERNEL,
        ) == 0
}

/// An exec that fails once the old image is gone leaves no program; one
/// refused while the old image stands keeps it.
pub fn test_an_exec_failing_past_the_reset_forfeits_the_program() -> TestResult {
    use slopos_fs::vfs::Subject;
    const BROKEN: &[u8] = b"/tmp/peer_test_broken";
    const NOT_ELF: &[u8] = b"/tmp/peer_test_not_elf";
    let _fixture = SyscallFixture::new();
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let staged = stage_executable(BROKEN, &elf_entering_nowhere())
        && stage_executable(NOT_ELF, b"not a program\n");
    let exec = |path: &[u8], reexec: bool| {
        let program = crate::exec::resolve_exec(path, b"/", &Subject::KERNEL)?;
        let (mut entry, mut stack, mut tp) = (0, 0, 0);
        crate::exec::do_exec(
            user.table,
            &program,
            None,
            None,
            reexec,
            &Subject::KERNEL,
            &mut entry,
            &mut stack,
            &mut tp,
        )
        .map(|(loaded, standing)| {
            standing.keep();
            loaded
        })
    };
    set_program(&user, BASE);
    let refused = exec(NOT_ELF, false);
    let kept = program_of(&user);
    let failed = exec(BROKEN, false);
    let forfeit = program_of(&user);
    set_program(&user, BASE);
    let failed_reexec = exec(BROKEN, true);
    let reexec_forfeit = program_of(&user);
    let _ = slopos_fs::vfs::vfs_unlink(BROKEN);
    let _ = slopos_fs::vfs::vfs_unlink(NOT_ELF);
    assert_test!(staged, "could not stage the two files");
    assert_eq_test!(refused, Err(Errno::ENOEXEC), "a file that is no ELF");
    assert_eq_test!(kept, BASE, "is refused before the reset");
    assert_test!(failed.is_err(), "an entry outside every segment");
    assert_eq_test!(forfeit, Program::Other, "fails after it");
    assert_test!(failed_reexec.is_err(), "a re-execution entering nowhere");
    assert_eq_test!(reexec_forfeit, Program::Other, "fails after the reset too");
    pass!()
}

slopos_testing::stest!(
    name = test_an_exec_failing_past_the_reset_forfeits_the_program,
    suite = peer
);

/// Only an absolute spelling with no link or `..` names a script as found.
pub fn test_a_script_is_named_as_found_only_absolutely() -> TestResult {
    use crate::exec::script::named_as_found;
    let Ok(found) = slopos_fs::vfs::canon::canonicalise(b"/bin/tool") else {
        return fail!("could not canonicalise");
    };
    assert_test!(named_as_found(b"/bin/tool", &found), "the path itself");
    assert_test!(
        named_as_found(b"//bin/./tool\0", &found),
        "a lexical respelling"
    );
    assert_test!(!named_as_found(b"tool", &found), "a working directory's");
    assert_test!(!named_as_found(b"/usr/../bin/tool", &found), "a walk up");
    pass!()
}

slopos_testing::stest!(
    name = test_a_script_is_named_as_found_only_absolutely,
    suite = peer
);

/// Exec attests a file of the base by its inode there, through a link to it
/// as well, and a script outside the base as nothing, whatever runs it. Run
/// in the userland phase, by which the base is installed; it creates no task.
pub fn test_exec_attests_only_what_the_base_serves() -> TestResult {
    use crate::exec::resolve_exec;
    use slopos_fs::vfs::Subject;
    const DIR: &[u8] = b"/tmp/attest";
    let base = &slopos_fs::basefs::BASE_FS;
    let Some(shell) = base.resolve(b"/bin/shell").ok().and_then(NonZeroU64::new) else {
        return fail!("the base carries no /bin/shell");
    };
    let attested = |path: &[u8]| resolve_exec(path, DIR, &Subject::KERNEL).map(|p| p.program);
    let _ = slopos_fs::vfs::vfs_mkdir(DIR);
    let _ = slopos_fs::vfs::vfs_unlink(b"/tmp/attest/link");
    let staged = slopos_fs::vfs::vfs_symlink(b"/bin/shell", b"/tmp/attest/link").is_ok()
        && stage_executable(b"/tmp/attest/script", b"#!/bin/shell\n");
    let link = attested(b"link");
    let script = attested(b"script");
    let _ = slopos_fs::vfs::vfs_unlink(b"/tmp/attest/link");
    let _ = slopos_fs::vfs::vfs_unlink(b"/tmp/attest/script");
    let _ = slopos_fs::vfs::vfs_rmdir(DIR);
    assert_test!(staged, "could not stage the link and the script");
    assert_eq_test!(
        attested(b"/bin/sh"),
        Ok(Program::Base(shell)),
        "the base's link to its shell is the shell"
    );
    assert_eq_test!(
        link,
        Ok(Program::Base(shell)),
        "a link outside the base to a base binary, which exec opens itself"
    );
    assert_eq_test!(
        script,
        Ok(Program::Other),
        "a script outside the base run by the base's shell"
    );
    let mut path = [0u8; 16];
    assert_eq_test!(
        base.path_of(shell.get(), &mut path)
            .map(|len| &path[..len] == b"/bin/shell"),
        Ok(true),
        "the attested inode is named by its path"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_exec_attests_only_what_the_base_serves,
    flags = 0,
    kind = Userland
);

/// An image of `records` activated as `name`; answers its root.
fn activate_extension(name: &[u8], records: &[(&str, u32, &[u8])]) -> Option<[u8; 32]> {
    use slopos_fs::tests::extensions::{activate_bytes, image};
    slopos_fs::vfs::vfs_init_builtin_filesystems().ok()?;
    let built = image(records)?;
    activate_bytes(name, &built.bytes, built.root).ok()?;
    Some(built.root)
}

fn deactivate_extension(name: &[u8]) {
    let _ = slopos_fs::extension::deactivate(name);
}

/// The program a file of the VFS at `path` attests as.
fn file_program(path: &[u8]) -> Program {
    use slopos_fs::vfs::path::{RESOLVE_FOLLOW, resolve_path_at};
    resolve_path_at(path, b"/", RESOLVE_FOLLOW, &slopos_fs::vfs::Subject::KERNEL)
        .map_or(Program::Other, |r| {
            slopos_fs::basefs::program_of(r.fs, r.inode)
        })
}

/// `SO_PEERPROGRAM` and `SO_PEEREXTENSION` on `fd`: the path and the label's
/// name, each empty for none.
fn peer_program_and_extension(who: &Principal, fd: i64) -> Option<([u8; 64], u32, [u8; 64], u32)> {
    let (rc, len, path) = getsockopt(who, fd, SOL_SLOPOS, SO_PEERPROGRAM, 64)?;
    let (rc2, label_len, label) = getsockopt(who, fd, SOL_SLOPOS, SO_PEEREXTENSION, 64)?;
    (rc == 0 && rc2 == 0).then_some((path, len, label, label_len))
}

fn names(who: &Principal, fd: i64, path: &[u8], extension: &[u8], root: [u8; 32]) -> bool {
    let expected = slopos_abi::extension::Label::new(extension, root);
    peer_program_and_extension(who, fd).is_some_and(|(p, len, label, label_len)| {
        p.get(..len as usize) == Some(path)
            && label_len as usize == slopos_abi::extension::Label::LEN
            && slopos_abi::extension::Label::from_bytes(&label[..label_len as usize]) == expected
    })
}

fn names_nothing(who: &Principal, fd: i64) -> bool {
    peer_program_and_extension(who, fd)
        .is_some_and(|(_, len, _, label_len)| len == 0 && label_len == 0)
}

const TOOL: u32 = slopos_abi::fs::S_IFREG | 0o755;
const LIB: u32 = slopos_abi::fs::S_IFREG | 0o644;

/// Exec attests an activation's file as that activation's program, a script
/// of it run by another of its files included, and as nothing a script whose
/// interpreter is another activation's or one outside both run by it.
pub fn test_exec_attests_an_activation_s_files_as_its_own() -> TestResult {
    use crate::exec::resolve_exec;
    use slopos_fs::vfs::Subject;
    const OUTSIDE: &[u8] = b"/tmp/ext-outside";
    let _fixture = SyscallFixture::new();
    let shown = activate_extension(
        b"ext-exec",
        &[
            ("bin/tool", TOOL, b"\x7fELF tool"),
            ("bin/script", TOOL, b"#!/opt/ext-exec/bin/tool\n"),
            ("bin/stray", TOOL, b"#!/opt/ext-other/bin/tool\n"),
        ],
    )
    .is_some()
        && activate_extension(b"ext-other", &[("bin/tool", TOOL, b"\x7fELF other")]).is_some()
        && stage_executable(OUTSIDE, b"#!/opt/ext-exec/bin/tool\n");
    let attested = |path: &[u8]| resolve_exec(path, b"/", &Subject::KERNEL).map(|p| p.program);
    let tool = file_program(b"/opt/ext-exec/bin/tool");
    let script = file_program(b"/opt/ext-exec/bin/script");
    let (as_tool, as_script, as_stray, as_outside) = (
        attested(b"/opt/ext-exec/bin/tool"),
        attested(b"/opt/ext-exec/bin/script"),
        attested(b"/opt/ext-exec/bin/stray"),
        attested(OUTSIDE),
    );
    let _ = slopos_fs::vfs::vfs_unlink(OUTSIDE);
    deactivate_extension(b"ext-exec");
    deactivate_extension(b"ext-other");
    assert_test!(shown, "could not show the two activations and the script");
    assert_test!(
        matches!(tool, Program::Extension(_)),
        "the tool is an activation's"
    );
    assert_eq_test!(as_tool, Ok(tool), "the tool attests as itself");
    assert_eq_test!(as_script, Ok(script), "a script its own tool runs");
    assert_eq_test!(
        as_stray,
        Ok(Program::Other),
        "a script another activation runs"
    );
    assert_eq_test!(as_outside, Ok(Program::Other), "a script outside both");
    pass!()
}

slopos_testing::stest!(
    name = test_exec_attests_an_activation_s_files_as_its_own,
    suite = peer
);

/// An extension program keeps its identity mapping its own activation's code
/// and the base's, and loses it, with what its sockets attest, mapping
/// another activation's.
pub fn test_code_of_another_activation_ends_an_extension_program() -> TestResult {
    let _fixture = SyscallFixture::new();
    let _registry = PageSetRegistry::publish();
    let page = [0xc3u8; 64];
    let root = activate_extension(
        b"ext-map",
        &[
            ("bin/tool", TOOL, b"\x7fELF tool"),
            ("lib/libown.so", LIB, &page),
        ],
    );
    let shown = activate_extension(b"ext-alien", &[("lib/libalien.so", LIB, &page)]).is_some();
    let outcome = (|| {
        let Some(root) = root.filter(|_| shown) else {
            return Err("could not show the two activations");
        };
        let user = Principal::user(USER, CAP_UNIVERSAL).ok_or("no principal")?;
        let tool = file_program(b"/opt/ext-map/bin/tool");
        set_program(&user, tool);
        let (rc, [a, _b]) = socketpair(&user, 0).ok_or("no socketpair")?;
        if rc != 0 || !names(&user, a.into(), b"/opt/ext-map/bin/tool", b"ext-map", root) {
            return Err("the pair does not name the extension program");
        }
        let map = |path: &[u8]| {
            stage_cstr(user.table, user.page + DATA, path)
                .then(|| {
                    call(
                        &user,
                        crate::syscall::fs::at_handlers::syscall_openat,
                        [
                            AT_FDCWD as i64 as u64,
                            user.page + DATA,
                            u64::from(O_RDONLY),
                            0,
                            0,
                            0,
                        ],
                    )
                })
                .flatten()
                .filter(|&fd| fd >= 0)
                .and_then(|fd| {
                    call(
                        &user,
                        syscall_mmap,
                        [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd as u64, 0],
                    )
                })
                .is_some_and(|addr| addr > 0)
        };
        if !map(b"/opt/ext-map/lib/libown.so") || program_of(&user) != tool {
            return Err("its own activation's code ended the program");
        }
        if !names(&user, a.into(), b"/opt/ext-map/bin/tool", b"ext-map", root) {
            return Err("its own activation's code ended what the pair attests");
        }
        if !map(b"/opt/ext-alien/lib/libalien.so") || program_of(&user) != Program::Other {
            return Err("another activation's code did not end the program");
        }
        if !names_nothing(&user, a.into()) {
            return Err("the pair still attests a program that ended");
        }
        Ok(())
    })();
    deactivate_extension(b"ext-map");
    deactivate_extension(b"ext-alien");
    match outcome {
        Ok(()) => pass!(),
        Err(why) => fail!("{}", why),
    }
}

slopos_testing::stest!(
    name = test_code_of_another_activation_ends_an_extension_program,
    suite = peer
);

/// A socket attests its creator's program while that process runs it: an
/// exec ends it, a new pair names the new instance, init's re-execution of
/// itself keeps it, and an exit ends it.
pub fn test_an_exec_or_an_exit_ends_what_a_socket_attests() -> TestResult {
    use slopos_ostd::process::{Cred, Identity, process_retire, process_spawn, root_account};
    let _fixture = SyscallFixture::new();
    let root = activate_extension(b"ext-life", &[("bin/tool", TOOL, b"\x7fELF tool")]);
    let tool = file_program(b"/opt/ext-life/bin/tool");
    let outcome = (|| {
        let Some(root) = root else {
            return Err("could not show the activation");
        };
        let user = Principal::user(USER, CAP_UNIVERSAL).ok_or("no principal")?;
        set_program(&user, tool);
        let (_, [before, _]) = socketpair(&user, 0).ok_or("no socketpair")?;
        if !names(
            &user,
            before.into(),
            b"/opt/ext-life/bin/tool",
            b"ext-life",
            root,
        ) {
            return Err("the pair does not name the program");
        }
        crate::exec::attest_exec(&user.task, tool, true, true);
        if !names(
            &user,
            before.into(),
            b"/opt/ext-life/bin/tool",
            b"ext-life",
            root,
        ) {
            return Err("an exec that keeps its authority ended the pair's program");
        }
        crate::exec::attest_exec(&user.task, tool, true, false);
        if !names_nothing(&user, before.into()) {
            return Err("an exec of the same program kept what the pair attests");
        }
        let (_, [after, _]) = socketpair(&user, 0).ok_or("no second socketpair")?;
        if !names(
            &user,
            after.into(),
            b"/opt/ext-life/bin/tool",
            b"ext-life",
            root,
        ) {
            return Err("a pair after the exec does not name the program");
        }

        let process = process_spawn(None, root_account(), Cred::ROOT).map_err(|_| "no process")?;
        process.set_program(tool);
        let (program, instance) = process.program_as_of_now();
        let identity = Identity {
            pid: process.id(),
            uid: 0,
            gid: 0,
            program,
            process: process.handle_raw(),
            instance,
        };
        let running = identity.now().program;
        process.mark_exited();
        let exited = identity.now().program;
        if let Some(handle) = process.handle() {
            process_retire(handle);
        }
        let reaped = identity.now().program;
        if running != tool || exited != Program::Other || reaped != Program::Other {
            return Err("an identity did not end with its process");
        }
        Ok(())
    })();
    deactivate_extension(b"ext-life");
    match outcome {
        Ok(()) => pass!(),
        Err(why) => fail!("{}", why),
    }
}

slopos_testing::stest!(
    name = test_an_exec_or_an_exit_ends_what_a_socket_attests,
    suite = peer
);

/// `ext_activate` of a descriptor that names no file is refused before
/// anything is read.
pub fn test_an_extension_of_no_file_is_refused() -> TestResult {
    use crate::syscall::fs::syscall_ext_activate;
    let _fixture = SyscallFixture::new();
    let Some(holder) = Principal::user(0, slopos_ostd::authority::CAP_MASK_ALL) else {
        return fail!("could not build the principal");
    };
    assert_test!(
        stage_cstr(holder.table, holder.page + DATA, b"ext-none"),
        "could not stage the name"
    );
    let refused = call(
        &holder,
        syscall_ext_activate,
        [holder.page + DATA, 999, holder.page + OPTVAL, 0, 0, 0],
    );
    assert_eq_test!(refused, errno(Errno::EBADF), "a descriptor of nothing");
    pass!()
}

slopos_testing::stest!(name = test_an_extension_of_no_file_is_refused, suite = peer);
