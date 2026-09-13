//! Advisory locks, dirfd bases and the fd-shaped mutators.
//!
//! Beside the code rather than in `fs/src/tests.rs`: several need the lock
//! table's module-private constants and its parked-request registry.

use slopos_abi::Errno;
use slopos_abi::fs::{O_RDONLY, O_RDWR, O_WRONLY, S_IFCHR, S_IFMT, UserFlock, UserFsStat};
use slopos_abi::syscall::{
    F_GETLK, F_RDLCK, F_SETLK, F_UNLCK, F_WRLCK, LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN, SEEK_SET,
};
use slopos_ostd::KArc;
use slopos_ostd::process::Process;
use slopos_testing::{TestResult, assert_eq_test, assert_test, fail, pass};

use super::flock::{
    MAX_LOCK_ROWS, MAX_PRINCIPAL_ROWS, file_locks_active, lock_wake_count, occupy_rows_for_test,
    park_record_for_test, release_test_rows, would_deadlock_for_test,
};
use super::{
    FdTable, FileRef, OpenMode, file_close_fd, file_fchmod_fd, file_fcntl_lock_fd, file_flock_fd,
    file_fstat_fd, file_getdents_commit_fd, file_getdents_fd, file_open_at, file_set_times_fd,
    lock_key_for_test, new_open_file,
};
use crate::vfs::path::RESOLVE_FOLLOW;
use crate::vfs::{FileStat, FileSystem, FileType, FsStats, InodeId, VfsError, VfsResult};

/// A registered process with an empty descriptor table, released on drop: a
/// table lives in its process's own registry slot.
struct Scratch {
    process: KArc<Process>,
}

impl Scratch {
    fn new() -> Option<Self> {
        let process = slopos_ostd::process::process_spawn_root().ok()?;
        let handle = process.handle()?;
        if super::fileio_create_empty_table_for_process(handle) != 0 {
            slopos_ostd::process::process_retire(handle);
            return None;
        }
        Some(Self { process })
    }

    fn table(&self) -> FdTable {
        FdTable::of(&self.process).expect("a registered process has a table")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(handle) = self.process.handle() {
            super::fileio_destroy_table_for_process(handle);
            slopos_ostd::process::process_retire(handle);
        }
    }
}

fn vfs_ready() -> bool {
    crate::vfs::vfs_init_builtin_filesystems().is_ok()
}

/// A file under `/locktest`, created if absent.
fn make_file(path: &'static [u8]) -> Option<&'static [u8]> {
    if !vfs_ready() {
        return None;
    }
    let _ = crate::vfs::vfs_mkdir(b"/locktest");
    let handle = crate::vfs::vfs_open(path, true).ok()?;
    let _ = handle.write(0, b"x");
    Some(path)
}

fn record(table: FdTable, fd: i32, cmd: u64, l_type: i16, start: i64, len: i64) -> i64 {
    let mut lock = UserFlock {
        l_type,
        l_whence: SEEK_SET as i16,
        _pad: [0; 4],
        l_start: start,
        l_len: len,
        l_pid: 0,
        _pad2: [0; 4],
    };
    file_fcntl_lock_fd(table, fd, cmd, &mut lock)
}

/// Release every row this descriptor's process holds on the file.
fn drain_locks(table: FdTable, fd: i32) {
    let _ = record(table, fd, F_SETLK, F_UNLCK, 0, 0);
}

/// A process is bounded to its own share of the lock table, and a neighbour
/// still gets locks while it sits at that bound.
pub fn test_record_lock_share_is_bounded_per_principal() -> TestResult {
    let Some(path) = make_file(b"/locktest/share.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(greedy) = Scratch::new() else {
        return fail!("could not create the greedy process");
    };
    let Some(neighbour) = Scratch::new() else {
        return fail!("could not create the neighbour process");
    };
    let greedy_table = greedy.table();
    let neighbour_table = neighbour.table();

    let greedy_fd = file_open_at(greedy_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let neighbour_fd = file_open_at(neighbour_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(greedy_fd >= 0 && neighbour_fd >= 0, "fixture opens failed");

    // Gaps between the ranges, or coalescing would merge them into one row and
    // the share would never be reached.
    let mut taken = 0usize;
    let mut refusal = 0i64;
    for i in 0..(MAX_PRINCIPAL_ROWS + 4) {
        let start = (i as i64) * 8192;
        let rc = record(greedy_table, greedy_fd, F_SETLK, F_WRLCK, start, 4096);
        if rc != 0 {
            refusal = rc;
            break;
        }
        taken += 1;
    }

    // Disjoint from everything the greedy process holds, so a refusal here is
    // the table's and not a conflict.
    let base = ((MAX_PRINCIPAL_ROWS + 8) as i64) * 8192;
    let neighbour_rc = record(neighbour_table, neighbour_fd, F_SETLK, F_WRLCK, base, 4096);

    drain_locks(greedy_table, greedy_fd);
    drain_locks(neighbour_table, neighbour_fd);
    let _ = file_close_fd(greedy_table, greedy_fd);
    let _ = file_close_fd(neighbour_table, neighbour_fd);

    assert_eq_test!(
        taken,
        MAX_PRINCIPAL_ROWS,
        "a principal took a different number of rows than its share"
    );
    assert_eq_test!(
        refusal,
        Errno::ENOLCK.raw() as i64,
        "the row past the share was not refused with ENOLCK"
    );
    assert_eq_test!(
        neighbour_rc,
        0,
        "a neighbour was denied a lock while another process sat at its share"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_record_lock_share_is_bounded_per_principal,
    suite = fs_locks
);

/// Abutting same-type ranges become one row, as they do on Linux.
pub fn test_record_lock_coalesces_abutting_ranges() -> TestResult {
    let Some(path) = make_file(b"/locktest/coalesce.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let fd = file_open_at(table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(fd >= 0, "the fixture open failed");

    let baseline = file_locks_active();
    let mut failed = 0i64;
    for i in 0..(MAX_PRINCIPAL_ROWS as i64 * 3) {
        let rc = record(table, fd, F_SETLK, F_WRLCK, i * 4096, 4096);
        if rc != 0 {
            failed = rc;
            break;
        }
    }
    let rows = file_locks_active().saturating_sub(baseline);

    drain_locks(table, fd);
    let _ = file_close_fd(table, fd);

    assert_eq_test!(failed, 0, "an abutting range was refused");
    assert_eq_test!(rows, 1, "abutting ranges did not coalesce into one row");
    pass!()
}

slopos_testing::stest!(
    name = test_record_lock_coalesces_abutting_ranges,
    suite = fs_locks
);

/// The property the admission design rests on: a principal holding no row can
/// always take one while any slot is free.
pub fn test_first_lock_is_admitted_while_the_reserve_holds() -> TestResult {
    let Some(path) = make_file(b"/locktest/reserve.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(holder) = Scratch::new() else {
        return fail!("could not create the holder process");
    };
    let Some(newcomer) = Scratch::new() else {
        return fail!("could not create the newcomer process");
    };
    let holder_table = holder.table();
    let newcomer_table = newcomer.table();
    let holder_fd = file_open_at(holder_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let newcomer_fd = file_open_at(newcomer_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(holder_fd >= 0 && newcomer_fd >= 0, "fixture opens failed");

    // Two slots left: enough for one row each, and well inside the reserve
    // watermark so a second row for a holder must be refused.
    let want = MAX_LOCK_ROWS
        .saturating_sub(file_locks_active())
        .saturating_sub(2);
    let placed = occupy_rows_for_test(want);

    let first = record(holder_table, holder_fd, F_SETLK, F_WRLCK, 0, 4096);
    let second = record(holder_table, holder_fd, F_SETLK, F_WRLCK, 1 << 20, 4096);
    let newcomers = record(newcomer_table, newcomer_fd, F_SETLK, F_WRLCK, 1 << 30, 4096);

    drain_locks(holder_table, holder_fd);
    drain_locks(newcomer_table, newcomer_fd);
    release_test_rows();
    let _ = file_close_fd(holder_table, holder_fd);
    let _ = file_close_fd(newcomer_table, newcomer_fd);

    assert_eq_test!(placed, want, "the fixture could not fill the table");
    assert_eq_test!(first, 0, "a principal's first row was refused");
    assert_eq_test!(
        second,
        Errno::ENOLCK.raw() as i64,
        "a holder grew into the reserve, which is what starves a newcomer"
    );
    assert_eq_test!(
        newcomers,
        0,
        "a principal holding no row was refused while a reserve slot was free"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_first_lock_is_admitted_while_the_reserve_holds,
    suite = fs_locks
);

/// A downgrade frees a row without any `release` running, so it has to wake
/// the queue itself.
///
/// Asserted on the wake the table issues rather than on a resumed task: the
/// kernel phase runs before the scheduler is online, so
/// `WaitQueue::enqueue_current` refuses to register and no task can be parked
/// to observe a resume.
pub fn test_downgrade_wakes_lock_waiters() -> TestResult {
    let Some(path) = make_file(b"/locktest/downgrade.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let fd = file_open_at(table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(fd >= 0, "the fixture open failed");

    // A plain acquire frees nothing, so it must not wake: that is what makes
    // the downgrade's wake attributable rather than unconditional.
    let before_acquire = lock_wake_count();
    let took = record(table, fd, F_SETLK, F_WRLCK, 0, 100);
    let acquire_wakes = lock_wake_count() - before_acquire;

    let before_downgrade = lock_wake_count();
    let downgraded = record(table, fd, F_SETLK, F_RDLCK, 0, 100);
    let downgrade_wakes = lock_wake_count() - before_downgrade;
    drain_locks(table, fd);

    // `flock(LOCK_EX)` -> `flock(LOCK_SH)` is the same shape.
    let ex = file_flock_fd(table, fd, (LOCK_EX | LOCK_NB) as u32);
    let before_flock_downgrade = lock_wake_count();
    let sh = file_flock_fd(table, fd, (LOCK_SH | LOCK_NB) as u32);
    let flock_wakes = lock_wake_count() - before_flock_downgrade;
    let _ = file_flock_fd(table, fd, LOCK_UN as u32);
    let _ = file_close_fd(table, fd);

    assert_eq_test!(took, 0, "the exclusive record lock was refused");
    assert_eq_test!(downgraded, 0, "the downgrade to a shared lock was refused");
    assert_eq_test!(ex, 0, "LOCK_EX was refused");
    assert_eq_test!(sh, 0, "the downgrade to LOCK_SH was refused");
    assert_eq_test!(
        acquire_wakes,
        0,
        "an acquire that freed no row woke the queue anyway"
    );
    assert_eq_test!(
        downgrade_wakes,
        1,
        "a record-lock downgrade freed the exclusive row without waking the queue"
    );
    assert_eq_test!(
        flock_wakes,
        1,
        "a flock downgrade freed the exclusive row without waking the queue"
    );
    pass!()
}

slopos_testing::stest!(name = test_downgrade_wakes_lock_waiters, suite = fs_locks);

/// An unlock strictly inside a held row splits it, and the tail needs a slot.
/// On a full table `ENOLCK` must leave the whole row standing.
pub fn test_midrange_unlock_keeps_the_tail_it_cannot_split() -> TestResult {
    let Some(path) = make_file(b"/locktest/split.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(holder) = Scratch::new() else {
        return fail!("could not create the holder process");
    };
    let Some(rival) = Scratch::new() else {
        return fail!("could not create the rival process");
    };
    let holder_table = holder.table();
    let rival_table = rival.table();
    let holder_fd = file_open_at(holder_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let rival_fd = file_open_at(rival_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(holder_fd >= 0 && rival_fd >= 0, "fixture opens failed");

    let held = record(holder_table, holder_fd, F_SETLK, F_WRLCK, 0, 3 * 4096);
    let want = MAX_LOCK_ROWS.saturating_sub(file_locks_active());
    let placed = occupy_rows_for_test(want);
    let unlocked = record(holder_table, holder_fd, F_SETLK, F_UNLCK, 4096, 4096);
    release_test_rows();

    // The tail the split would have produced: still the holder's, so the rival
    // is refused rather than granted it.
    let tail = record(rival_table, rival_fd, F_SETLK, F_WRLCK, 2 * 4096, 4096);

    drain_locks(holder_table, holder_fd);
    drain_locks(rival_table, rival_fd);
    let _ = file_close_fd(holder_table, holder_fd);
    let _ = file_close_fd(rival_table, rival_fd);

    assert_eq_test!(held, 0, "the fixture lock was refused");
    assert_eq_test!(placed, want, "the fixture could not fill the table");
    assert_eq_test!(
        unlocked,
        Errno::ENOLCK.raw() as i64,
        "a split with no slot for its tail was not refused"
    );
    assert_eq_test!(
        tail,
        Errno::EAGAIN.raw() as i64,
        "a refused unlock gave away the tail of a lock still held"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_midrange_unlock_keeps_the_tail_it_cannot_split,
    suite = fs_locks
);

/// POSIX: a process's record locks on a file die when it closes *any*
/// descriptor naming that file.
pub fn test_close_drops_record_locks_on_that_file() -> TestResult {
    let Some(path) = make_file(b"/locktest/close.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(holder) = Scratch::new() else {
        return fail!("could not create the holder process");
    };
    let Some(rival) = Scratch::new() else {
        return fail!("could not create the rival process");
    };
    let holder_table = holder.table();
    let rival_table = rival.table();
    let first = file_open_at(holder_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let second = file_open_at(holder_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let rival_fd = file_open_at(rival_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(
        first >= 0 && second >= 0 && rival_fd >= 0,
        "fixture opens failed"
    );

    let taken = record(holder_table, first, F_SETLK, F_WRLCK, 0, 4096);
    let contended = record(rival_table, rival_fd, F_SETLK, F_WRLCK, 0, 4096);
    // The second descriptor on the same file is still open; POSIX drops them
    // anyway.
    let closed = file_close_fd(holder_table, first);
    let after_close = record(rival_table, rival_fd, F_SETLK, F_WRLCK, 0, 4096);

    drain_locks(rival_table, rival_fd);
    let _ = file_close_fd(holder_table, second);
    let _ = file_close_fd(rival_table, rival_fd);

    assert_eq_test!(taken, 0, "the fixture lock was refused");
    assert_eq_test!(
        contended,
        Errno::EAGAIN.raw() as i64,
        "a rival was granted a conflicting range"
    );
    assert_eq_test!(closed, 0, "closing the descriptor failed");
    assert_eq_test!(
        after_close,
        0,
        "closing a descriptor did not drop the process's record locks on the file"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_close_drops_record_locks_on_that_file,
    suite = fs_locks
);

/// `flock` locks and record locks are independent, per `flock(2)`: a process's
/// own `flock(fd, LOCK_EX)` must not refuse its own `F_SETLK`.
pub fn test_flock_and_record_locks_are_independent() -> TestResult {
    let Some(path) = make_file(b"/locktest/kinds.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let fd = file_open_at(table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(fd >= 0, "the fixture open failed");

    let flocked = file_flock_fd(table, fd, (LOCK_EX | LOCK_NB) as u32);
    let recorded = record(table, fd, F_SETLK, F_WRLCK, 0, 0);
    // `F_GETLK` must not report the `flock` row either.
    let mut probe = UserFlock {
        l_type: F_WRLCK,
        l_whence: SEEK_SET as i16,
        _pad: [0; 4],
        l_start: 0,
        l_len: 0,
        l_pid: 0,
        _pad2: [0; 4],
    };
    drain_locks(table, fd);
    let getlk = file_fcntl_lock_fd(table, fd, F_GETLK, &mut probe);

    let _ = file_flock_fd(table, fd, LOCK_UN as u32);
    let _ = file_close_fd(table, fd);

    assert_eq_test!(flocked, 0, "LOCK_EX was refused");
    assert_eq_test!(
        recorded,
        0,
        "a process's own flock refused its own record lock"
    );
    assert_eq_test!(getlk, 0, "F_GETLK failed");
    assert_eq_test!(
        probe.l_type,
        F_UNLCK,
        "F_GETLK reported a flock holder as a record-lock conflict"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_flock_and_record_locks_are_independent,
    suite = fs_locks
);

/// A two-process cycle is as permanent as a self-deadlock, and is what Linux
/// answers `EDEADLK` to.
pub fn test_record_lock_cycle_is_a_deadlock() -> TestResult {
    let Some(path) = make_file(b"/locktest/cycle.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(first) = Scratch::new() else {
        return fail!("could not create the first process");
    };
    let Some(second) = Scratch::new() else {
        return fail!("could not create the second process");
    };
    let first_table = first.table();
    let second_table = second.table();
    let first_fd = file_open_at(first_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let second_fd = file_open_at(second_table, path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(first_fd >= 0 && second_fd >= 0, "fixture opens failed");
    let Some(key) = lock_key_for_test(first_table, first_fd) else {
        return fail!("could not derive the lock key");
    };

    let held_low = record(first_table, first_fd, F_SETLK, F_WRLCK, 0, 4096);
    let held_high = record(second_table, second_fd, F_SETLK, F_WRLCK, 4096, 4096);

    // No cycle yet: the second process holds [4096, 8192) but waits for
    // nothing, so it can still release.
    let no_cycle = would_deadlock_for_test(first_table, key, true, 4096, 8192);

    // The state the second process would be in parked on `[0, 4096)`.
    let parked = park_record_for_test(second_table, key, true, 0, 4096);
    let cycle = would_deadlock_for_test(first_table, key, true, 4096, 8192);
    drop(parked);

    drain_locks(first_table, first_fd);
    drain_locks(second_table, second_fd);
    let _ = file_close_fd(first_table, first_fd);
    let _ = file_close_fd(second_table, second_fd);

    assert_eq_test!(held_low, 0, "the first fixture lock was refused");
    assert_eq_test!(held_high, 0, "the second fixture lock was refused");
    assert_test!(
        !no_cycle,
        "a holder that waits for nothing was called a deadlock"
    );
    assert_test!(cycle, "a two-process wait-for cycle went undetected");
    pass!()
}

slopos_testing::stest!(
    name = test_record_lock_cycle_is_a_deadlock,
    suite = fs_locks
);

/// `fcntl(2)`: a read lock needs a descriptor open for reading, a write lock
/// one open for writing.
pub fn test_fcntl_lock_needs_the_matching_access_mode() -> TestResult {
    let Some(path) = make_file(b"/locktest/mode.txt") else {
        return fail!("could not build the fixture file");
    };
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let ro = file_open_at(table, path, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    let wo = file_open_at(table, path, b"/", O_WRONLY, RESOLVE_FOLLOW, None);
    assert_test!(ro >= 0 && wo >= 0, "fixture opens failed");

    let write_on_ro = record(table, ro, F_SETLK, F_WRLCK, 0, 4096);
    let read_on_wo = record(table, wo, F_SETLK, F_RDLCK, 0, 4096);
    let read_on_ro = record(table, ro, F_SETLK, F_RDLCK, 0, 4096);

    drain_locks(table, ro);
    let _ = file_close_fd(table, ro);
    let _ = file_close_fd(table, wo);

    assert_eq_test!(
        write_on_ro,
        Errno::EBADF.raw() as i64,
        "F_WRLCK was taken through an O_RDONLY descriptor"
    );
    assert_eq_test!(
        read_on_wo,
        Errno::EBADF.raw() as i64,
        "F_RDLCK was taken through an O_WRONLY descriptor"
    );
    assert_eq_test!(
        read_on_ro,
        0,
        "F_RDLCK was refused on a readable descriptor"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_fcntl_lock_needs_the_matching_access_mode,
    suite = fs_locks
);

/// A dirfd's base is the path the walk *ended* on, so `openat(dirfd, "../x")`
/// names the target's parent rather than the symlink's.
pub fn test_dirfd_base_is_the_path_the_walk_ended_on() -> TestResult {
    if !vfs_ready() {
        return fail!("the VFS is unavailable");
    }
    let _ = crate::vfs::vfs_mkdir(b"/locktest");
    let _ = crate::vfs::vfs_mkdir(b"/locktest/real");
    let _ = crate::vfs::vfs_unlink(b"/locktest/link");
    if crate::vfs::vfs_symlink(b"/locktest/real", b"/locktest/link").is_err() {
        return fail!("could not create the fixture symlink");
    }
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let dirfd = file_open_at(
        table,
        b"/locktest/link",
        b"/",
        O_RDONLY,
        RESOLVE_FOLLOW,
        None,
    );
    assert_test!(dirfd >= 0, "the symlinked directory could not be opened");

    let mut base = [0u8; 64];
    let mut base_len = 0usize;
    let got = super::with_fd_dir_path(table, dirfd, |path| {
        base_len = path.len().min(base.len());
        base[..base_len].copy_from_slice(&path[..base_len]);
    });

    let _ = file_close_fd(table, dirfd);
    let _ = crate::vfs::vfs_unlink(b"/locktest/link");

    assert_test!(got.is_ok(), "the dirfd base was unavailable");
    assert_eq_test!(
        &base[..base_len],
        b"/locktest/real".as_slice(),
        "the dirfd stored its lexical canonicalisation, not the resolved path"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_dirfd_base_is_the_path_the_walk_ended_on,
    suite = fs_locks
);

/// A dirfd base is revalidated against the descriptor's own inode, so a rename
/// that puts a different directory at the same name is `ESTALE`.
pub fn test_dirfd_base_answers_estale_after_a_rename() -> TestResult {
    if !vfs_ready() {
        return fail!("the VFS is unavailable");
    }
    let _ = crate::vfs::vfs_mkdir(b"/locktest");
    let _ = crate::vfs::vfs_rmdir(b"/locktest/moved");
    let _ = crate::vfs::vfs_rmdir(b"/locktest/base");
    if crate::vfs::vfs_mkdir(b"/locktest/base").is_err() {
        return fail!("could not create the fixture directory");
    }
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let dirfd = file_open_at(
        table,
        b"/locktest/base",
        b"/",
        O_RDONLY,
        RESOLVE_FOLLOW,
        None,
    );
    assert_test!(dirfd >= 0, "the directory could not be opened");

    let fresh = super::with_fd_dir_path(table, dirfd, |_| ());
    let renamed = crate::vfs::vfs_rename(b"/locktest/base", b"/locktest/moved").is_ok()
        && crate::vfs::vfs_mkdir(b"/locktest/base").is_ok();
    let stale = super::with_fd_dir_path(table, dirfd, |_| ());

    let _ = file_close_fd(table, dirfd);
    let _ = crate::vfs::vfs_rmdir(b"/locktest/base");
    let _ = crate::vfs::vfs_rmdir(b"/locktest/moved");

    assert_test!(fresh.is_ok(), "a fresh dirfd base was rejected");
    assert_test!(renamed, "the fixture rename failed");
    assert_eq_test!(
        stale.err(),
        Some(Errno::ESTALE),
        "a renamed dirfd base was resolved against the new occupant of its name"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_dirfd_base_answers_estale_after_a_rename,
    suite = fs_locks
);

/// The cursor belongs to the caller until the bytes have reached userland: an
/// `EFAULT` on the destination must not lose a whole batch.
pub fn test_getdents_cursor_is_committed_by_the_caller() -> TestResult {
    if !vfs_ready() {
        return fail!("the VFS is unavailable");
    }
    let _ = crate::vfs::vfs_mkdir(b"/locktest");
    let _ = crate::vfs::vfs_mkdir(b"/locktest/dents");
    for name in [
        b"/locktest/dents/a".as_slice(),
        b"/locktest/dents/b".as_slice(),
    ] {
        let _ = crate::vfs::vfs_open(name, true);
    }
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let dirfd = file_open_at(
        table,
        b"/locktest/dents",
        b"/",
        O_RDONLY,
        RESOLVE_FOLLOW,
        None,
    );
    assert_test!(dirfd >= 0, "the directory could not be opened");

    let mut buf = [0u8; 512];
    let first = file_getdents_fd(table, dirfd, &mut buf);
    let again = file_getdents_fd(table, dirfd, &mut buf);
    let committed = first.map(|(_, cookie)| file_getdents_commit_fd(table, dirfd, cookie));
    let after = file_getdents_fd(table, dirfd, &mut buf);

    let _ = file_close_fd(table, dirfd);

    let Ok((first_bytes, first_cookie)) = first else {
        return fail!("the first getdents batch failed");
    };
    let Ok((again_bytes, again_cookie)) = again else {
        return fail!("the repeated getdents batch failed");
    };
    assert_test!(first_bytes > 0, "the directory read produced no entries");
    assert_eq_test!(
        (again_bytes, again_cookie),
        (first_bytes, first_cookie),
        "an uncommitted batch advanced the shared cursor"
    );
    assert_test!(
        matches!(committed, Ok(Ok(()))),
        "committing the cookie failed"
    );
    let Ok((after_bytes, after_cookie)) = after else {
        return fail!("the batch after the commit failed");
    };
    assert_test!(
        after_bytes == 0 || after_cookie != first_cookie,
        "the committed cookie did not advance the walk"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_getdents_cursor_is_committed_by_the_caller,
    suite = fs_locks
);

struct StubFs {
    read_only: bool,
}

static READ_ONLY_FS: StubFs = StubFs { read_only: true };
static WRITABLE_FS: StubFs = StubFs { read_only: false };

impl FileSystem for StubFs {
    fn name(&self) -> &'static str {
        "stubfs"
    }

    fn root_inode(&self) -> InodeId {
        1
    }

    fn lookup(&self, _parent: InodeId, _name: &[u8]) -> VfsResult<InodeId> {
        Err(VfsError::NotFound)
    }

    fn readdir(
        &self,
        _inode: InodeId,
        _offset: usize,
        _callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        Ok(0)
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        Ok(FileStat::new_file(inode, 0))
    }

    fn read(&self, _inode: InodeId, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, _inode: InodeId, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::ReadOnly)
    }

    fn create(&self, _parent: InodeId, _name: &[u8], _file_type: FileType) -> VfsResult<InodeId> {
        Err(VfsError::ReadOnly)
    }

    fn unlink(&self, _parent: InodeId, _name: &[u8]) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn set_mode(&self, _inode: InodeId, _mode: u16) -> VfsResult<()> {
        Ok(())
    }

    fn set_times(
        &self,
        _inode: InodeId,
        _atime: Option<u64>,
        _mtime: Option<u64>,
    ) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self) -> VfsResult<FsStats> {
        Ok(FsStats {
            magic: 0,
            block_size: 4096,
            blocks: 1,
            blocks_free: 0,
            blocks_available: 0,
            inodes: 1,
            inodes_free: 0,
            max_name_len: 255,
            read_only: self.read_only,
        })
    }
}

/// An `O_RDONLY` descriptor on a stub filesystem, so the mutators are reached
/// without any open-time writability check having run.
fn install_stub_fd(table: FdTable, fs: &'static StubFs, fd: i32) -> Option<usize> {
    let handle = crate::vfs_file_ops::vnode_handle_for_tests(fs, 2)?;
    let open_file = new_open_file(
        &crate::vfs_file_ops::VFS_FILE_OPS,
        handle,
        OpenMode::READ,
        0,
        None,
    )?;
    let file = FileRef { open_file };
    if super::fileio_install_file_ref_at(table, fd, file, false) != fd {
        crate::vfs_file_ops::drop_vnode_for_tests(handle);
        return None;
    }
    Some(handle)
}

/// `fchmod` and `utimensat(fd, NULL, ..)` owe the read-only check themselves:
/// `vfs_open_flags_at` only reaches `check_writable` for a writable open.
pub fn test_read_only_filesystem_refuses_fd_shaped_mutators() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let Some(ro_handle) = install_stub_fd(table, &READ_ONLY_FS, 3) else {
        return fail!("could not install the read-only stub descriptor");
    };
    let Some(rw_handle) = install_stub_fd(table, &WRITABLE_FS, 4) else {
        crate::vfs_file_ops::drop_vnode_for_tests(ro_handle);
        return fail!("could not install the writable stub descriptor");
    };

    let chmod_ro = file_fchmod_fd(table, 3, 0o600);
    let times_ro = file_set_times_fd(table, 3, Some(1), Some(1));
    let chmod_rw = file_fchmod_fd(table, 4, 0o600);
    let times_rw = file_set_times_fd(table, 4, Some(1), Some(1));

    let _ = file_close_fd(table, 3);
    let _ = file_close_fd(table, 4);
    // The stub descriptions carry no backing, so nothing else reclaims the
    // vnode table rows the fixture minted.
    crate::vfs_file_ops::drop_vnode_for_tests(ro_handle);
    crate::vfs_file_ops::drop_vnode_for_tests(rw_handle);

    assert_eq_test!(
        chmod_ro,
        Errno::EROFS.raw() as i32,
        "fchmod reached a read-only filesystem"
    );
    assert_eq_test!(
        times_ro,
        Errno::EROFS.raw() as i32,
        "utimensat on a descriptor reached a read-only filesystem"
    );
    assert_eq_test!(chmod_rw, 0, "fchmod was refused on a writable filesystem");
    assert_eq_test!(
        times_rw,
        0,
        "utimensat on a descriptor was refused on a writable filesystem"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_read_only_filesystem_refuses_fd_shaped_mutators,
    suite = fs_locks
);

/// The `fstat` a tty answers before any driver registers one — the fallback
/// `effective_tty_ops` returns. Its type bits must come from the shared filler.
pub fn test_tty_fstat_reports_a_character_device() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create the process");
    };
    let table = scratch.table();
    let Some(open_file) = new_open_file(&super::LOCAL_TTY_OPS, 3, OpenMode::READ, 0, None) else {
        return fail!("could not build the tty description");
    };
    let file = FileRef { open_file };
    if super::fileio_install_file_ref_at(table, 5, file, false) != 5 {
        return fail!("could not install the tty descriptor");
    }

    let mut stat = UserFsStat::default();
    let rc = file_fstat_fd(table, 5, &mut stat);
    let _ = file_close_fd(table, 5);

    assert_eq_test!(rc, 0, "fstat on a tty descriptor failed");
    assert_eq_test!(
        stat.st_mode & S_IFMT,
        S_IFCHR,
        "a tty did not report itself as a character device"
    );
    assert_eq_test!(stat.st_rdev >> 8, 4, "the tty major was not Linux's");
    assert_eq_test!(stat.st_rdev & 0xFF, 3, "the tty minor was not the handle");
    pass!()
}

slopos_testing::stest!(
    name = test_tty_fstat_reports_a_character_device,
    suite = fs_locks
);
