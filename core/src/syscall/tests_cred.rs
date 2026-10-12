//! Credentials and permission bits through the real syscall entry: the id
//! calls' rules, the mode bits on every path operation, ownership for
//! `chmod`/`chown`, and the signal rule's uid half. Each refusal sits beside
//! the positive control that shows the same caller one bit away succeeding.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::Errno;
use slopos_abi::fs::{AT_EACCESS, AT_FDCWD, O_CREAT, O_RDONLY, O_WRONLY, R_OK, UserFsStat};
use slopos_abi::signal::{SIGCONT, SIGTERM};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
use slopos_fs::fileio::{FdTable, file_stat_at};
use slopos_fs::vfs::path::RESOLVE_FOLLOW;
use slopos_fs::vfs::{Subject, vfs_mkdir, vfs_open, vfs_rmdir, vfs_set_mode, vfs_unlink};
use slopos_ostd::authority::{CAP_UNIVERSAL, Capability};
use slopos_ostd::process::Cred;
use slopos_sched::task::{TaskRef, task_create, task_find_by_id, task_terminate};
use slopos_testing::{TestResult, assert_eq_test, assert_test, fail, pass};

use crate::syscall::common::SyscallHandler;
use crate::syscall::tests_build_floor_fs::{call_syscall, map_user_pages, stage_cstr};

type SyscallFixture = slopos_sched::test_fixture::KernelTestScope;

const ROOT: &[u8] = b"/cred_tests";
const USER: u32 = 1000;
const OTHER: u32 = 2000;

fn errno(e: Errno) -> Option<i64> {
    Some(i64::from(e.raw()))
}

/// A user task of its own process, holding `caps`, with `cred` as its ids
/// and a user page to stage path arguments in.
struct Principal {
    task: TaskRef,
    table: FdTable,
    page: u64,
}

impl Principal {
    #[inline(never)]
    fn user(uid: u32, caps: u64) -> Option<Self> {
        Self::new(Cred::of(uid, uid), caps)
    }

    /// Real, effective and saved uids as given, every gid the real uid.
    #[inline(never)]
    fn with_uids(uids: [u32; 3], caps: u64) -> Option<Self> {
        Self::new(
            Cred::of(uids[0], uids[0]).set_resuid(uids, true).ok()?,
            caps,
        )
    }

    #[inline(never)]
    fn new(cred: Cred, caps: u64) -> Option<Self> {
        let entry = slopos_sched::task::task_entry_from_kernel_va(
            slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA as u64,
        );
        let id = task_create(
            b"CredTest\0".as_ptr() as *const c_char,
            entry,
            ptr::null_mut(),
            1,
            TASK_FLAG_USER_MODE,
        );
        if id == INVALID_TASK_ID {
            return None;
        }
        let task = task_find_by_id(id)?;
        slopos_ostd::task::ops::task_set_caps(&task, caps);
        let process = task.process()?;
        process.change_cred(|_| Ok(cred)).ok()?;
        let table = FdTable::of(&process)?;
        let page = map_user_pages(table, 1)?;
        Some(Self { task, table, page })
    }

    fn call(&self, handler: SyscallHandler, args: [u64; 6]) -> Option<i64> {
        call_syscall(self.table, &self.task, handler, args)
    }

    /// `path` staged at the start of the page, `second` halfway in.
    fn paths(&self, path: &[u8], second: &[u8]) -> Option<(u64, u64)> {
        let half = self.page + 2048;
        (stage_cstr(self.table, self.page, path) && stage_cstr(self.table, half, second))
            .then_some((self.page, half))
    }

    fn openat(&self, path: &[u8], flags: u32) -> Option<i64> {
        let (at, _) = self.paths(path, b"")?;
        let rc = self.call(
            crate::syscall::fs::at_handlers::syscall_openat,
            [AT_FDCWD as i64 as u64, at, u64::from(flags), 0o644, 0, 0],
        )?;
        if rc >= 0 {
            let _ = slopos_fs::fileio::file_close_fd(self.table, rc as i32);
            return Some(0);
        }
        Some(rc)
    }

    fn path_call(&self, handler: SyscallHandler, path: &[u8], rest: [u64; 3]) -> Option<i64> {
        let (at, _) = self.paths(path, b"")?;
        self.call(handler, [at, rest[0], rest[1], rest[2], 0, 0])
    }

    fn rename(&self, from: &[u8], to: &[u8]) -> Option<i64> {
        let (old, new) = self.paths(from, to)?;
        self.call(
            crate::syscall::fs::path_handlers::syscall_rename,
            [old, new, 0, 0, 0, 0],
        )
    }

    #[inline(never)]
    fn uids(&self) -> [u32; 3] {
        let cred = self
            .task
            .process()
            .map_or(Cred::ROOT, |process| process.cred());
        [cred.ruid, cred.euid, cred.suid]
    }

    #[inline(never)]
    fn groups_are(&self, want: &[u32]) -> bool {
        self.task
            .process()
            .is_some_and(|process| process.cred().groups() == want)
    }

    #[inline(never)]
    fn set_uids(&self, uids: [u32; 3]) -> bool {
        self.task.process().is_some_and(|process| {
            process
                .change_cred(|_| Cred::of(uids[0], uids[0]).set_resuid(uids, true))
                .is_ok()
        })
    }

    #[inline(never)]
    fn may_signal(&self, target: &Principal, signum: u8) -> bool {
        crate::syscall::signal::signal_permitted(
            &crate::syscall::signal::Sender::of(&self.task),
            &target.task,
            u64::from(signum),
        )
    }
}

impl Drop for Principal {
    fn drop(&mut self) {
        task_terminate(self.task.task_id);
    }
}

fn join(name: &[u8]) -> slopos_ostd::KVec<u8> {
    let mut path = slopos_ostd::KVec::<u8>::zeroed(ROOT.len() + 1 + name.len()).expect("alloc");
    path[..ROOT.len()].copy_from_slice(ROOT);
    path[ROOT.len()] = b'/';
    path[ROOT.len() + 1..].copy_from_slice(name);
    path
}

/// A root-owned file of `mode` under [`ROOT`], made by the kernel.
fn root_file(name: &[u8], mode: u16) -> Option<slopos_ostd::KVec<u8>> {
    let path = join(name);
    let _ = vfs_unlink(&path);
    vfs_open(&path, true).ok()?;
    vfs_set_mode(&path, mode).ok()?;
    Some(path)
}

fn root_dir(name: &[u8], mode: u16) -> Option<slopos_ostd::KVec<u8>> {
    let path = join(name);
    match vfs_mkdir(&path) {
        Ok(()) | Err(slopos_fs::VfsError::AlreadyExists) => {}
        Err(_) => return None,
    }
    vfs_set_mode(&path, mode).ok()?;
    Some(path)
}

fn fixtures_root() -> bool {
    slopos_fs::vfs::vfs_init_builtin_filesystems().is_ok()
        && matches!(
            vfs_mkdir(ROOT),
            Ok(()) | Err(slopos_fs::VfsError::AlreadyExists)
        )
}

fn owner_of(path: &[u8]) -> Option<(u32, u32, u32)> {
    let mut stat = UserFsStat::default();
    (file_stat_at(path, b"/", RESOLVE_FOLLOW, &mut stat, &Subject::KERNEL) == 0).then_some((
        stat.st_uid,
        stat.st_gid,
        stat.st_mode & 0o7777,
    ))
}

pub fn test_id_calls_follow_posix_without_set_id() -> TestResult {
    use crate::syscall::cred_handlers::{
        syscall_getgroups, syscall_setgroups, syscall_setresuid, syscall_setuid,
    };
    let _fixture = SyscallFixture::new();
    let Some(user) = Principal::with_uids([USER, OTHER, USER], CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let keep = u64::from(u32::MAX);

    assert_eq_test!(
        user.call(syscall_setuid, [u64::from(OTHER), 0, 0, 0, 0, 0]),
        errno(Errno::EPERM),
        "setuid to an effective-only id must be refused"
    );
    assert_eq_test!(
        user.call(syscall_setuid, [u64::from(USER), 0, 0, 0, 0, 0]),
        Some(0),
        "setuid to the real id must be allowed"
    );
    assert_eq_test!(
        user.uids(),
        [USER; 3],
        "unprivileged setuid moved more than the effective id"
    );
    assert_eq_test!(
        user.call(syscall_setresuid, [keep, 0, keep, 0, 0, 0]),
        errno(Errno::EPERM),
        "uid 1000 became uid 0 without SetId"
    );
    assert_eq_test!(
        user.call(syscall_setgroups, [0, 0, 0, 0, 0, 0]),
        errno(Errno::EPERM),
        "setgroups without SetId"
    );

    let Some(admin) = Principal::user(USER, CAP_UNIVERSAL | Capability::SetId.bit()) else {
        return fail!("could not build the SetId principal");
    };
    assert_eq_test!(
        admin.call(syscall_setuid, [0, 0, 0, 0, 0, 0]),
        Some(0),
        "SetId must take any uid"
    );
    assert_eq_test!(admin.uids(), [0; 3], "setuid with SetId sets all three");
    let group_list = admin.page;
    assert_test!(
        stage_cstr(admin.table, group_list, &7u32.to_le_bytes()),
        "could not stage the group list"
    );
    assert_eq_test!(
        admin.call(syscall_setgroups, [1, group_list, 0, 0, 0, 0]),
        Some(0),
        "setgroups with SetId"
    );
    assert_eq_test!(
        admin.call(syscall_getgroups, [0, 0, 0, 0, 0, 0]),
        Some(1),
        "getgroups(0) counts the groups set"
    );
    assert_test!(
        admin.groups_are(&[7]),
        "the group set is not the one asked for"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_id_calls_follow_posix_without_set_id,
    suite = cred
);

pub fn test_mode_bits_refuse_open_and_create() -> TestResult {
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let (Some(secret), Some(public), Some(closed)) = (
        root_file(b"secret", 0o600),
        root_file(b"public", 0o644),
        root_dir(b"closed", 0o755),
    ) else {
        return fail!("could not lay out the fixtures");
    };
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };

    assert_eq_test!(
        user.openat(&secret, O_RDONLY),
        errno(Errno::EACCES),
        "read a 0600 root file"
    );
    assert_eq_test!(
        user.openat(&public, O_RDONLY),
        Some(0),
        "read a 0644 root file"
    );
    assert_eq_test!(
        user.openat(&public, O_WRONLY),
        errno(Errno::EACCES),
        "write a 0644 root file"
    );

    let inside = join(b"closed/new");
    assert_eq_test!(
        user.openat(&inside, O_WRONLY | O_CREAT),
        errno(Errno::EACCES),
        "create in a 0755 root directory"
    );
    assert_test!(
        vfs_set_mode(&closed, 0o777).is_ok(),
        "could not open the directory"
    );
    assert_eq_test!(
        user.openat(&inside, O_WRONLY | O_CREAT),
        Some(0),
        "create in a 0777 directory"
    );
    assert_eq_test!(
        owner_of(&inside).map(|(uid, gid, _)| (uid, gid)),
        Some((USER, USER)),
        "a new file belongs to its creator"
    );

    assert_test!(
        vfs_set_mode(&closed, 0o700).is_ok(),
        "could not close the directory"
    );
    let mut stat = UserFsStat::default();
    let blocked = file_stat_at(
        &inside,
        b"/",
        RESOLVE_FOLLOW,
        &mut stat,
        &user_subject(&user),
    );
    assert_eq_test!(
        blocked,
        Errno::EACCES.raw(),
        "stat through a 0700 root directory"
    );

    let _ = vfs_unlink(&inside);
    let _ = vfs_rmdir(&closed);
    let _ = vfs_unlink(&secret);
    let _ = vfs_unlink(&public);
    pass!()
}

fn user_subject(principal: &Principal) -> Subject {
    crate::syscall::context::subject_of(&principal.task)
}

slopos_testing::stest!(name = test_mode_bits_refuse_open_and_create, suite = cred);

pub fn test_a_sticky_directory_keeps_names_to_their_owners() -> TestResult {
    use crate::syscall::fs::path_handlers::syscall_unlink;
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let (Some(shared), Some(theirs)) = (
        root_dir(b"shared", 0o1777),
        root_file(b"shared/theirs", 0o666),
    ) else {
        return fail!("could not lay out the fixtures");
    };
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let mine = join(b"shared/mine");
    let moved = join(b"shared/moved");

    assert_eq_test!(
        user.openat(&theirs, O_WRONLY),
        Some(0),
        "write another's 0666 file"
    );
    assert_eq_test!(
        user.path_call(syscall_unlink, &theirs, [0; 3]),
        errno(Errno::EPERM),
        "unlink another's file in a sticky directory"
    );
    assert_eq_test!(
        user.openat(&mine, O_WRONLY | O_CREAT),
        Some(0),
        "create in a sticky directory"
    );
    assert_eq_test!(
        user.rename(&mine, &theirs),
        errno(Errno::EPERM),
        "rename over another's file in a sticky directory"
    );
    assert_eq_test!(
        user.rename(&mine, &moved),
        Some(0),
        "rename its own file there"
    );
    assert_eq_test!(
        user.path_call(syscall_unlink, &moved, [0; 3]),
        Some(0),
        "unlink its own file there"
    );

    assert_test!(
        vfs_set_mode(&shared, 0o777).is_ok(),
        "could not clear the sticky bit"
    );
    assert_eq_test!(
        user.path_call(syscall_unlink, &theirs, [0; 3]),
        Some(0),
        "without the sticky bit a writable directory gives up any name"
    );
    let _ = vfs_rmdir(&shared);
    pass!()
}

slopos_testing::stest!(
    name = test_a_sticky_directory_keeps_names_to_their_owners,
    suite = cred
);

pub fn test_chmod_and_chown_are_the_owners() -> TestResult {
    use crate::syscall::fs::path_handlers::{syscall_chmod, syscall_chown};
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let (Some(_), Some(theirs)) = (root_dir(b"owned", 0o777), root_file(b"owned/theirs", 0o644))
    else {
        return fail!("could not lay out the fixtures");
    };
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let mine = join(b"owned/mine");
    let _ = vfs_unlink(&mine);
    assert_eq_test!(
        user.openat(&mine, O_WRONLY | O_CREAT),
        Some(0),
        "create its own file"
    );
    let keep = u64::from(u32::MAX);

    assert_eq_test!(
        user.path_call(syscall_chmod, &theirs, [0o666, 0, 0]),
        errno(Errno::EPERM),
        "chmod another's file"
    );
    assert_eq_test!(
        user.path_call(syscall_chmod, &mine, [0o600, 0, 0]),
        Some(0),
        "chmod its own file"
    );
    assert_eq_test!(
        user.path_call(syscall_chown, &mine, [keep, 0, 0]),
        errno(Errno::EPERM),
        "give its own file to group 0"
    );
    assert_eq_test!(
        user.path_call(syscall_chown, &mine, [0, keep, 0]),
        errno(Errno::EPERM),
        "give its own file to uid 0"
    );
    assert_eq_test!(
        user.path_call(syscall_chown, &mine, [keep, u64::from(USER), 0]),
        Some(0),
        "keep its own file in its own group"
    );
    assert_eq_test!(
        owner_of(&mine),
        Some((USER, USER, 0o600)),
        "the file's owner and mode"
    );

    let Some(admin) = Principal::user(USER, CAP_UNIVERSAL | Capability::DacOverride.bit()) else {
        return fail!("could not build the DacOverride principal");
    };
    assert_test!(
        vfs_set_mode(&mine, 0o6755).is_ok(),
        "could not set the set-ID bits"
    );
    assert_eq_test!(
        admin.path_call(syscall_chown, &mine, [0, 0, 0]),
        Some(0),
        "DacOverride gives a file to anyone"
    );
    assert_eq_test!(
        owner_of(&mine),
        Some((0, 0, 0o755)),
        "a new owner takes the set-ID bits off an executable"
    );
    let _ = vfs_unlink(&mine);
    let _ = vfs_unlink(&theirs);
    pass!()
}

slopos_testing::stest!(name = test_chmod_and_chown_are_the_owners, suite = cred);

pub fn test_access_checks_the_real_ids_unless_asked() -> TestResult {
    use crate::syscall::fs::at_handlers::syscall_faccessat2;
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let Some(secret) = root_file(b"access_secret", 0o600) else {
        return fail!("could not lay out the fixture");
    };
    let Some(user) = Principal::with_uids([USER, 0, USER], CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let Some((at, _)) = user.paths(&secret, b"") else {
        return fail!("could not stage the path");
    };
    let fdcwd = AT_FDCWD as i64 as u64;
    assert_eq_test!(
        user.call(syscall_faccessat2, [fdcwd, at, u64::from(R_OK), 0, 0, 0]),
        errno(Errno::EACCES),
        "access checks the real uid"
    );
    assert_eq_test!(
        user.call(
            syscall_faccessat2,
            [fdcwd, at, u64::from(R_OK), u64::from(AT_EACCESS), 0, 0]
        ),
        Some(0),
        "AT_EACCESS checks the effective uid"
    );
    let _ = vfs_unlink(&secret);
    pass!()
}

slopos_testing::stest!(
    name = test_access_checks_the_real_ids_unless_asked,
    suite = cred
);

pub fn test_signals_need_the_uid_rule() -> TestResult {
    let _fixture = SyscallFixture::new();
    let (Some(sender), Some(same), Some(stranger), Some(privileged)) = (
        Principal::user(USER, CAP_UNIVERSAL),
        Principal::user(USER, CAP_UNIVERSAL),
        Principal::user(OTHER, CAP_UNIVERSAL),
        Principal::user(USER, CAP_UNIVERSAL | Capability::ProcSignal.bit()),
    ) else {
        return fail!("could not build the principals");
    };

    assert_test!(
        sender.may_signal(&same, SIGTERM),
        "signal its own uid's task"
    );
    assert_test!(
        !sender.may_signal(&stranger, SIGTERM),
        "signal another uid's task"
    );
    assert_test!(!sender.may_signal(&stranger, 0), "probe another uid's task");

    stranger.task.set_sid(sender.task.sid().wrapping_add(1));
    assert_test!(
        !sender.may_signal(&stranger, SIGCONT),
        "SIGCONT across sessions"
    );
    stranger.task.set_sid(sender.task.sid());
    assert_test!(
        sender.may_signal(&stranger, SIGCONT),
        "SIGCONT within the sender's session"
    );

    assert_test!(
        stranger.set_uids([OTHER, OTHER, USER]),
        "could not move the target's ids"
    );
    assert_test!(
        sender.may_signal(&stranger, SIGTERM),
        "the target's saved uid admits its old owner"
    );
    assert_test!(
        stranger.set_uids([OTHER; 3]),
        "could not restore the target's ids"
    );
    assert_test!(
        privileged.may_signal(&stranger, SIGTERM),
        "ProcSignal waives the uid rule"
    );
    assert_test!(
        !same.may_signal(&privileged, SIGTERM),
        "a task that lacks ProcSignal may not signal one that holds it"
    );
    pass!()
}

slopos_testing::stest!(name = test_signals_need_the_uid_rule, suite = cred);
