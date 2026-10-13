//! Credentials and permission bits: the id calls' rules, the mode bits on
//! every path operation and ownership for `chmod`, `chown`, times and flags,
//! all through the real syscall entry, and the signal rule's uid half. Each
//! refusal sits beside the positive control that shows the same caller one bit
//! away succeeding.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::Errno;
use slopos_abi::fs::{AT_EACCESS, AT_FDCWD, O_CREAT, O_RDONLY, O_WRONLY, R_OK, UserFsStat};
use slopos_abi::signal::{SIGCONT, SIGTERM};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
use slopos_fs::fileio::{FdTable, file_stat_at};
use slopos_fs::vfs::path::RESOLVE_FOLLOW;
use slopos_fs::vfs::{
    Subject, vfs_mkdir, vfs_open, vfs_rmdir, vfs_set_mode, vfs_set_owner_at, vfs_unlink,
};
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
pub(crate) struct Principal {
    pub(crate) task: TaskRef,
    pub(crate) table: FdTable,
    pub(crate) page: u64,
}

impl Principal {
    #[inline(never)]
    pub(crate) fn user(uid: u32, caps: u64) -> Option<Self> {
        Self::new(Cred::of(uid, uid), caps)
    }

    /// Real, effective and saved uids as given, every gid the real uid.
    #[inline(never)]
    pub(crate) fn with_uids(uids: [u32; 3], caps: u64) -> Option<Self> {
        Self::new(
            Cred::of(uids[0], uids[0]).set_resuid(uids, true).ok()?,
            caps,
        )
    }

    #[inline(never)]
    pub(crate) fn new(cred: Cred, caps: u64) -> Option<Self> {
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

    pub(crate) fn call(&self, handler: SyscallHandler, args: [u64; 6]) -> Option<i64> {
        call_syscall(self.table, &self.task, handler, args)
    }

    /// `path` staged at the start of the page, `second` halfway in.
    fn paths(&self, path: &[u8], second: &[u8]) -> Option<(u64, u64)> {
        let half = self.page + 2048;
        (stage_cstr(self.table, self.page, path) && stage_cstr(self.table, half, second))
            .then_some((self.page, half))
    }

    fn openat(&self, path: &[u8], flags: u32) -> Option<i64> {
        self.openat_mode(path, flags, 0o644)
    }

    fn openat_mode(&self, path: &[u8], flags: u32, mode: u64) -> Option<i64> {
        let fd = self.open_fd(path, flags, mode)?;
        if fd >= 0 {
            let _ = slopos_fs::fileio::file_close_fd(self.table, fd as i32);
            return Some(0);
        }
        Some(fd)
    }

    fn open_fd(&self, path: &[u8], flags: u32, mode: u64) -> Option<i64> {
        let (at, _) = self.paths(path, b"")?;
        self.call(
            crate::syscall::fs::at_handlers::syscall_openat,
            [AT_FDCWD as i64 as u64, at, u64::from(flags), mode, 0, 0],
        )
    }

    /// `bytes` staged in the second half of the page, for an argument that is
    /// not a path.
    pub(crate) fn stage(&self, bytes: &[u8]) -> Option<u64> {
        let half = self.page + 2048;
        stage_cstr(self.table, half, bytes).then_some(half)
    }

    fn path_call(&self, handler: SyscallHandler, path: &[u8], rest: [u64; 3]) -> Option<i64> {
        let (at, _) = self.paths(path, b"")?;
        self.call(handler, [at, rest[0], rest[1], rest[2], 0, 0])
    }

    fn rename(&self, from: &[u8], to: &[u8]) -> Option<i64> {
        self.two_paths(crate::syscall::fs::path_handlers::syscall_rename, from, to)
    }

    fn two_paths(&self, handler: SyscallHandler, first: &[u8], second: &[u8]) -> Option<i64> {
        let (first, second) = self.paths(first, second)?;
        self.call(handler, [first, second, 0, 0, 0, 0])
    }

    /// `FS_IOC_SETFLAGS` on `path` opened read-only, asking for the flags it
    /// already has.
    #[inline(never)]
    fn keep_flags(&self, path: &[u8]) -> Option<i64> {
        use slopos_abi::fs::inode_flags_ioctl::FS_IOC_SETFLAGS;
        let fd = self.open_fd(path, O_RDONLY, 0)?;
        let rc = slopos_fs::fileio::file_inode_flags_fd(self.table, fd as i32)
            .ok()
            .and_then(|flags| self.stage(&flags.to_le_bytes()))
            .and_then(|staged| {
                self.call(
                    crate::syscall::fs::poll_ioctl_handlers::syscall_ioctl,
                    [fd as u64, u64::from(FS_IOC_SETFLAGS), staged, 0, 0, 0],
                )
            });
        let _ = slopos_fs::fileio::file_close_fd(self.table, fd as i32);
        rc
    }

    /// `kill(target, 0)` through the syscall.
    #[inline(never)]
    fn probe_kill(&self, target: &Principal) -> Option<i64> {
        let pid = crate::syscall::signal::sender_pid(&target.task);
        self.call(
            crate::syscall::signal::syscall_kill,
            [u64::from(pid), 0, 0, 0, 0, 0],
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
    let mut list = [0u8; 8];
    list[..4].copy_from_slice(&7u32.to_le_bytes());
    list[4..].copy_from_slice(&8u32.to_le_bytes());
    let group_list = admin.page;
    assert_test!(
        stage_cstr(admin.table, group_list, &list),
        "could not stage the group list"
    );
    assert_eq_test!(
        admin.call(syscall_setgroups, [2, group_list, 0, 0, 0, 0]),
        Some(0),
        "setgroups with SetId"
    );
    assert_eq_test!(
        admin.call(syscall_getgroups, [0, 0, 0, 0, 0, 0]),
        Some(2),
        "getgroups(0) counts the groups set"
    );
    assert_eq_test!(
        admin.call(syscall_getgroups, [1, group_list, 0, 0, 0, 0]),
        errno(Errno::EINVAL),
        "getgroups into a list too short for the set"
    );
    assert_test!(
        admin.groups_are(&[7, 8]),
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
    let private = join(b"closed/private");
    assert_eq_test!(
        user.openat_mode(&private, O_WRONLY | O_CREAT, 0o600),
        Some(0),
        "create a private file"
    );
    assert_eq_test!(
        owner_of(&private).map(|(_, _, mode)| mode),
        Some(0o600),
        "a new file is born with the mode it asked for"
    );
    let _ = vfs_unlink(&private);

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
    let _ = vfs_rmdir(&join(b"owned"));
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
    assert_eq_test!(
        sender.probe_kill(&stranger),
        errno(Errno::EPERM),
        "kill another uid's task"
    );
    assert_eq_test!(sender.probe_kill(&same), Some(0), "kill its own uid's task");
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

pub fn test_a_new_name_needs_write_and_search() -> TestResult {
    use crate::syscall::fs::at_handlers::syscall_link;
    use crate::syscall::fs::path_handlers::{syscall_mkdir, syscall_symlink};
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let (Some(names), Some(target)) =
        (root_dir(b"names", 0o755), root_file(b"names_target", 0o644))
    else {
        return fail!("could not lay out the fixtures");
    };
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let (sub, soft, hard) = (join(b"names/sub"), join(b"names/soft"), join(b"names/hard"));

    assert_eq_test!(
        user.path_call(syscall_mkdir, &sub, [0o755, 0, 0]),
        errno(Errno::EACCES),
        "mkdir in a 0755 root directory"
    );
    assert_eq_test!(
        user.two_paths(syscall_symlink, &target, &soft),
        errno(Errno::EACCES),
        "symlink in a 0755 root directory"
    );
    assert_eq_test!(
        user.two_paths(syscall_link, &target, &hard),
        errno(Errno::EACCES),
        "link in a 0755 root directory"
    );
    assert_test!(
        vfs_set_mode(&names, 0o777).is_ok(),
        "could not open the directory"
    );
    assert_eq_test!(
        user.path_call(syscall_mkdir, &sub, [0o755, 0, 0]),
        Some(0),
        "mkdir in a 0777 directory"
    );
    assert_eq_test!(
        user.two_paths(syscall_symlink, &target, &soft),
        Some(0),
        "symlink in a 0777 directory"
    );
    let linked = user.two_paths(syscall_link, &target, &hard);
    assert_test!(
        linked == Some(0) || linked == errno(Errno::EOPNOTSUPP),
        "link in a 0777 directory, which ramfs answers EOPNOTSUPP"
    );
    let _ = vfs_unlink(&hard);
    let _ = vfs_unlink(&soft);
    let _ = vfs_rmdir(&sub);
    let _ = vfs_rmdir(&names);
    let _ = vfs_unlink(&target);
    pass!()
}

slopos_testing::stest!(name = test_a_new_name_needs_write_and_search, suite = cred);

pub fn test_a_setgid_directory_hands_down_its_group() -> TestResult {
    use crate::syscall::fs::path_handlers::syscall_mkdir;
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let Some(team) = root_dir(b"team", 0o777) else {
        return fail!("could not lay out the fixture");
    };
    let regrouped = vfs_set_owner_at(
        &team,
        b"/",
        None,
        Some(OTHER),
        RESOLVE_FOLLOW,
        &Subject::KERNEL,
    );
    assert_test!(
        regrouped.is_ok() && vfs_set_mode(&team, 0o2777).is_ok(),
        "could not make the directory set-group-ID"
    );
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let (file, sub) = (join(b"team/file"), join(b"team/sub"));

    assert_eq_test!(
        user.openat_mode(&file, O_WRONLY | O_CREAT, 0o2654),
        Some(0),
        "create in a set-group-ID directory"
    );
    assert_eq_test!(
        owner_of(&file),
        Some((USER, OTHER, 0o654)),
        "a file takes the directory's group, and no group-executable set-group-ID outside \
         the creator's"
    );
    assert_eq_test!(
        user.path_call(syscall_mkdir, &sub, [0o755, 0, 0]),
        Some(0),
        "mkdir in a set-group-ID directory"
    );
    assert_eq_test!(
        owner_of(&sub),
        Some((USER, OTHER, 0o2755)),
        "a directory takes the group and the set-group-ID bit"
    );
    let _ = vfs_unlink(&file);
    let _ = vfs_rmdir(&sub);
    let _ = vfs_rmdir(&team);
    pass!()
}

slopos_testing::stest!(
    name = test_a_setgid_directory_hands_down_its_group,
    suite = cred
);

pub fn test_moving_a_directory_needs_write_on_it() -> TestResult {
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let (Some(from), Some(to), Some(theirs)) = (
        root_dir(b"from", 0o777),
        root_dir(b"to", 0o777),
        root_dir(b"from/theirs", 0o755),
    ) else {
        return fail!("could not lay out the fixtures");
    };
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let (away, renamed) = (join(b"to/theirs"), join(b"from/renamed"));

    assert_eq_test!(
        user.rename(&theirs, &away),
        errno(Errno::EACCES),
        "move another's directory to a new parent"
    );
    assert_eq_test!(
        user.rename(&theirs, &renamed),
        Some(0),
        "rename another's directory within its parent"
    );
    let _ = vfs_rmdir(&renamed);
    let _ = vfs_rmdir(&from);
    let _ = vfs_rmdir(&to);
    pass!()
}

slopos_testing::stest!(
    name = test_moving_a_directory_needs_write_on_it,
    suite = cred
);

pub fn test_times_size_and_flags_follow_the_bits() -> TestResult {
    use crate::syscall::fs::at_handlers::syscall_utimensat;
    use crate::syscall::fs::path_handlers::syscall_truncate;
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let (Some(shared), Some(read_only)) = (
        root_file(b"times_shared", 0o666),
        root_file(b"times_read_only", 0o644),
    ) else {
        return fail!("could not lay out the fixtures");
    };
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let fdcwd = AT_FDCWD as i64 as u64;
    let Some((at, _)) = user.paths(&shared, b"") else {
        return fail!("could not stage the path");
    };
    assert_eq_test!(
        user.call(syscall_utimensat, [fdcwd, at, 0, 0, 0, 0]),
        Some(0),
        "a writer sets another's times to now"
    );
    let mut explicit = [0u8; 32];
    explicit[..8].copy_from_slice(&1i64.to_le_bytes());
    explicit[16..24].copy_from_slice(&1i64.to_le_bytes());
    let Some(times) = user.stage(&explicit) else {
        return fail!("could not stage the times");
    };
    assert_eq_test!(
        user.call(syscall_utimensat, [fdcwd, at, times, 0, 0, 0]),
        errno(Errno::EPERM),
        "a writer sets another's times to a chosen moment"
    );

    assert_eq_test!(
        user.path_call(syscall_truncate, &read_only, [0, 0, 0]),
        errno(Errno::EACCES),
        "truncate a 0644 root file"
    );
    assert_eq_test!(
        user.path_call(syscall_truncate, &shared, [0, 0, 0]),
        Some(0),
        "truncate a 0666 root file"
    );

    let (Some(theirs), Some(owner)) = (
        user.keep_flags(&shared),
        Principal::user(0, CAP_UNIVERSAL).and_then(|root| root.keep_flags(&shared)),
    ) else {
        return fail!("could not reach the flags");
    };
    assert_eq_test!(
        theirs,
        i64::from(Errno::EPERM.raw()),
        "set the flags of another's file"
    );
    assert_eq_test!(owner, 0, "set the flags of its own file");
    let _ = vfs_unlink(&shared);
    let _ = vfs_unlink(&read_only);
    pass!()
}

slopos_testing::stest!(
    name = test_times_size_and_flags_follow_the_bits,
    suite = cred
);

/// The check `chdir` and `fchdir` share, as the principal: storing a cwd acts
/// on the running task, which here is the test's own.
pub fn test_chdir_needs_search() -> TestResult {
    use crate::syscall::process_handlers::resolve_new_cwd;
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let Some(locked) = root_dir(b"locked", 0o700) else {
        return fail!("could not lay out the fixture");
    };
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let who = user_subject(&user);
    assert_eq_test!(
        resolve_new_cwd(&locked, b"/", &who).err(),
        Some(Errno::EACCES),
        "chdir into a 0700 root directory"
    );
    assert_test!(
        vfs_set_mode(&locked, 0o711).is_ok(),
        "could not open the directory to search"
    );
    assert_test!(
        resolve_new_cwd(&locked, b"/", &who).is_ok(),
        "chdir into a 0711 directory"
    );
    let _ = vfs_rmdir(&locked);
    pass!()
}

slopos_testing::stest!(name = test_chdir_needs_search, suite = cred);

/// uid 0 owns the system's files and passes no mode bit on its own.
pub fn test_uid_zero_meets_the_mode_bits() -> TestResult {
    let _fixture = SyscallFixture::new();
    assert_test!(fixtures_root(), "could not lay out the fixture root");
    let Some(sealed_off) = root_file(b"read_only_to_root", 0o444) else {
        return fail!("could not lay out the fixture");
    };
    let (Some(root), Some(overrider)) = (
        Principal::user(0, CAP_UNIVERSAL),
        Principal::user(0, CAP_UNIVERSAL | Capability::DacOverride.bit()),
    ) else {
        return fail!("could not build the principals");
    };
    assert_eq_test!(
        root.openat(&sealed_off, O_WRONLY),
        errno(Errno::EACCES),
        "uid 0 writes its own 0444 file"
    );
    assert_eq_test!(
        overrider.openat(&sealed_off, O_WRONLY),
        Some(0),
        "DacOverride writes a 0444 file"
    );
    let _ = vfs_unlink(&sealed_off);
    pass!()
}

slopos_testing::stest!(name = test_uid_zero_meets_the_mode_bits, suite = cred);

pub fn test_exec_needs_the_execute_bit_of_its_class() -> TestResult {
    use crate::exec::resolve_exec;
    use crate::tests::helpers::install_static_program;
    let _fixture = SyscallFixture::new();
    let tool = join(b"tool");
    assert_test!(
        fixtures_root()
            && install_static_program(&tool, &[0xeb, 0xfe])
            && vfs_set_mode(&tool, 0o700).is_ok(),
        "could not write the program"
    );
    let Some(user) = Principal::user(USER, CAP_UNIVERSAL) else {
        return fail!("could not build the principal");
    };
    let who = user_subject(&user);
    assert_eq_test!(
        resolve_exec(&tool, b"/", &who).err(),
        Some(Errno::EACCES),
        "exec a 0700 root program"
    );
    assert_test!(
        vfs_set_mode(&tool, 0o755).is_ok(),
        "could not open the program to others"
    );
    assert_test!(
        resolve_exec(&tool, b"/", &who).is_ok(),
        "exec a 0755 root program"
    );
    let _ = vfs_unlink(&tool);
    pass!()
}

slopos_testing::stest!(
    name = test_exec_needs_the_execute_bit_of_its_class,
    suite = cred
);

/// A spawned child saves its effective ids, as an exec does.
pub fn test_spawn_saves_the_effective_ids() -> TestResult {
    use crate::tests::helpers::install_static_program;
    let _fixture = SyscallFixture::new();
    let spin = join(b"spin");
    assert_test!(
        fixtures_root() && install_static_program(&spin, &[0xeb, 0xfe]),
        "could not write the program"
    );
    let parent = Principal::with_uids([USER, USER, 0], CAP_UNIVERSAL);
    let child_uids = parent
        .as_ref()
        .and_then(|parent| spawned_uids(&spin, parent));
    let _ = vfs_unlink(&spin);
    let Some(parent) = parent else {
        return fail!("could not build the spawner");
    };
    assert_eq_test!(
        child_uids,
        Some([USER; 3]),
        "the child kept a saved uid its spawner had set aside"
    );
    assert_eq_test!(
        parent.uids(),
        [USER, USER, 0],
        "spawning moved the spawner's own ids"
    );
    pass!()
}

slopos_testing::stest!(name = test_spawn_saves_the_effective_ids, suite = cred);

/// The uids of a child `parent` spawns running `program`, which is then ended.
#[inline(never)]
fn spawned_uids(program: &[u8], parent: &Principal) -> Option<[u32; 3]> {
    let child_id = crate::exec::spawn_program_with_attrs(
        program,
        None,
        None,
        slopos_abi::task::TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        CAP_UNIVERSAL,
        &[],
        0,
        None,
        parent.task.task_id,
    )
    .ok()?;
    let uids = task_find_by_id(child_id)
        .and_then(|child| child.process())
        .map(|process| process.cred())
        .map(|cred| [cred.ruid, cred.euid, cred.suid]);
    task_terminate(child_id);
    let _ = slopos_sched::task::task_consume_zombie(child_id);
    uids
}
