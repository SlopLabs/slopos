//! Coverage for the Phase 1 filesystem syscall surface.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::Errno;
use slopos_abi::fs::{
    O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_WRONLY, S_IFLNK, S_IFMT, S_IFREG, UIO_MAXIOV,
    USER_PATH_MAX, UserFsStat, UserIovec,
};
use slopos_abi::io::{KernelIoBuf, KernelIoBufRef};
use slopos_abi::syscall::{LOCK_EX, LOCK_NB, LOCK_UN, SEEK_CUR, SEEK_SET};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};

use slopos_fs::fileio::{
    FdTable, file_close_fd, file_dup_fd, file_flock_fd, file_getdents_commit_fd, file_getdents_fd,
    file_mkdir_at, file_open_at, file_pread_fd, file_read_fd, file_rmdir_at, file_seek_fd,
    file_stat_at, file_symlink_at, file_unlink_at, file_write_fd, with_fd_dir_path,
};
use slopos_fs::vfs::path::{RESOLVE_FOLLOW, RESOLVE_NOFOLLOW_FINAL};

use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::process_vm_alloc;
use slopos_mm::user_copy::set_test_process_id;
use slopos_mm::user_io_buf::UserIovecBuf;
use slopos_ostd::user::context::UserContext;
use slopos_ostd::{KBox, KVec};
use slopos_sched::task::{task_create, task_find_by_id, task_terminate};
use slopos_testing::{TestResult, assert_eq_test, assert_test, fail, pass};

use crate::syscall::args::UserPath;
use crate::syscall::fs::io_handlers::stage_iovec;
use crate::syscall::ui_handlers::syscall_getrandom;

type SyscallFixture = slopos_sched::test_fixture::KernelTestScope;

/// Where every fixture in this file lives: `/tmp` is not guaranteed to exist
/// on a boot whose root is the initramfs.
const ROOT: &[u8] = b"/phase1_fs";

fn create_user_task() -> u32 {
    let entry = slopos_sched::task::task_entry_from_kernel_va(
        slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA as u64,
    );
    task_create(
        b"Phase1Fs\0".as_ptr() as *const c_char,
        entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    )
}

struct Scratch {
    task_id: u32,
    table: FdTable,
}

impl Scratch {
    fn new() -> Option<Self> {
        if slopos_fs::vfs::vfs_init_builtin_filesystems().is_err() {
            return None;
        }
        let task_id = create_user_task();
        if task_id == INVALID_TASK_ID {
            return None;
        }
        let table = task_find_by_id(task_id)
            .as_deref()
            .and_then(|task| task.process())
            .as_deref()
            .and_then(FdTable::of)?;
        // `EEXIST` from a previous test in this suite is success here.
        let _ = file_mkdir_at(ROOT, b"/");
        Some(Self { task_id, table })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        task_terminate(self.task_id);
    }
}

fn join(name: &[u8]) -> KVec<u8> {
    let mut path = KVec::<u8>::zeroed(ROOT.len() + 1 + name.len()).expect("path alloc");
    path[..ROOT.len()].copy_from_slice(ROOT);
    path[ROOT.len()] = b'/';
    path[ROOT.len() + 1..].copy_from_slice(name);
    path
}

fn make_file(table: FdTable, name: &[u8], contents: &[u8]) -> Option<KVec<u8>> {
    let path = join(name);
    let _ = file_unlink_at(&path, b"/");
    let fd = file_open_at(table, &path, b"/", O_RDWR | O_CREAT, RESOLVE_FOLLOW, None);
    if fd < 0 {
        return None;
    }
    if !contents.is_empty() {
        let buf = KernelIoBufRef::new(contents);
        if file_write_fd(table, fd, &buf) != contents.len() as isize {
            let _ = file_close_fd(table, fd);
            return None;
        }
    }
    let _ = file_close_fd(table, fd);
    Some(path)
}

fn map_user_pages(table: FdTable, pages: usize) -> Option<u64> {
    let process = table.process()?;
    let len = (pages * 4096) as u64;
    let base = process_vm_alloc(process, len, PageFlags::USER_RW.bits() as u32);
    if base == 0 {
        return None;
    }
    for page in 0..pages {
        let addr = base + (page * 4096) as u64;
        let mapped = slopos_mm::process_vm::process_vm_with_vm_space(process, |vs| {
            slopos_mm::user_mappings::ostd_map_4kb_user_fresh(
                vs,
                slopos_abi::addr::VirtAddr::new(addr),
                PageFlags::USER_RW.bits(),
            )
            .is_ok()
        });
        if !matches!(mapped, Some(true)) {
            return None;
        }
    }
    Some(base)
}

fn with_user_process_context<R>(table: FdTable, f: impl FnOnce() -> R) -> Option<R> {
    let process = table.process()?;
    if slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr(process) == 0 {
        return None;
    }
    if !slopos_mm::process_vm::process_vm_activate(process) {
        return None;
    }
    set_test_process_id(table.id());
    let out = f();
    set_test_process_id(slopos_abi::task::INVALID_PROCESS_ID);
    slopos_kernel_services::kernel_vm_space::kernel_vm_space()
        .lock()
        .activate_kernel_master();
    Some(out)
}

fn fill_user_bytes(table: FdTable, addr: u64, bytes: &[u8]) -> bool {
    with_user_process_context(table, || {
        let Ok(user) = slopos_mm::user_ptr::UserBytes::try_new(addr, bytes.len()) else {
            return false;
        };
        slopos_mm::user_copy::copy_bytes_to_user(user, bytes).is_ok()
    })
    .unwrap_or(false)
}

/// A truncated path names a different file, which is how a caller ends up
/// writing to one.
pub fn test_user_path_over_max_is_refused_not_truncated() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let Some(page) = map_user_pages(scratch.table, 2) else {
        return fail!("could not map user pages");
    };

    // 4095 payload bytes plus the NUL is exactly what the buffer holds.
    let mut fits = KVec::<u8>::zeroed(USER_PATH_MAX).expect("alloc");
    fits[..USER_PATH_MAX - 1].fill(b'a');
    fits[0] = b'/';
    assert_test!(
        fill_user_bytes(scratch.table, page, &fits),
        "could not stage the longest legal path"
    );

    let short = with_user_process_context(scratch.table, || {
        UserPath::from_user_addr(page).map(|p| p.len())
    });
    assert_eq_test!(
        short,
        Some(Ok(USER_PATH_MAX - 1)),
        "the longest legal path did not round-trip at full length"
    );

    // Two mapped pages are 8192 bytes, so a 4608-byte unterminated path fits.
    let long_len = USER_PATH_MAX + 512;
    let mut too_long = KVec::<u8>::zeroed(long_len + 1).expect("alloc");
    too_long[..long_len].fill(b'b');
    too_long[0] = b'/';
    assert_test!(
        fill_user_bytes(scratch.table, page, &too_long),
        "could not stage an over-long path"
    );
    let refused = with_user_process_context(scratch.table, || {
        UserPath::from_user_addr(page).map(|p| p.len())
    });
    assert_eq_test!(
        refused,
        Some(Err(Errno::ENAMETOOLONG)),
        "an over-long path was accepted"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_user_path_over_max_is_refused_not_truncated,
    suite = syscall_fs_phase1
);

pub fn test_openat_resolves_against_cwd_and_dirfd() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let dir = join(b"atdir");
    let _ = file_mkdir_at(&dir, b"/");
    let inner = join(b"atdir/leaf.txt");
    let _ = file_unlink_at(&inner, b"/");
    let created = file_open_at(table, &inner, b"/", O_RDWR | O_CREAT, RESOLVE_FOLLOW, None);
    assert_test!(created >= 0, "could not create the fixture file");
    let _ = file_close_fd(table, created);

    let by_cwd = file_open_at(table, b"leaf.txt", &dir, O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(by_cwd >= 0, "a relative open against a cwd failed");
    let _ = file_close_fd(table, by_cwd);

    let dirfd = file_open_at(table, &dir, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(dirfd >= 0, "a directory could not be opened");
    let by_dirfd = with_fd_dir_path(table, dirfd, |base| {
        file_open_at(table, b"leaf.txt", base, O_RDONLY, RESOLVE_FOLLOW, None)
    });
    assert_test!(
        matches!(by_dirfd, Ok(fd) if fd >= 0),
        "a relative open against a dirfd failed"
    );
    if let Ok(fd) = by_dirfd {
        let _ = file_close_fd(table, fd);
    }

    let filefd = file_open_at(table, &inner, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(filefd >= 0, "could not open the fixture file");
    assert_eq_test!(
        with_fd_dir_path(table, filefd, |_| ()).err(),
        Some(Errno::ENOTDIR),
        "a file descriptor was accepted as a dirfd"
    );

    let _ = file_close_fd(table, filefd);
    let _ = file_close_fd(table, dirfd);
    let _ = file_unlink_at(&inner, b"/");
    let _ = file_rmdir_at(&dir, b"/");
    pass!()
}

slopos_testing::stest!(
    name = test_openat_resolves_against_cwd_and_dirfd,
    suite = syscall_fs_phase1
);

/// The errnos are pinned because they are the whole answer a caller acts on:
/// `EISDIR` tells a remover to retry with the flag, and `FileSystem::rmdir`
/// defaults to `unlink`, so the filesystem draws no distinction of its own.
pub fn test_unlinkat_removedir_separates_file_from_directory() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let dir = join(b"rmdir_target");
    let _ = file_rmdir_at(&dir, b"/");
    assert_eq_test!(file_mkdir_at(&dir, b"/"), 0, "mkdir of the fixture failed");

    assert_eq_test!(
        file_unlink_at(&dir, b"/"),
        Errno::EISDIR.raw(),
        "unlinkat without AT_REMOVEDIR did not refuse a directory with EISDIR"
    );
    assert_eq_test!(
        file_rmdir_at(&dir, b"/"),
        0,
        "unlinkat with AT_REMOVEDIR did not remove the directory"
    );

    let Some(file) = make_file(table, b"unlink_target", b"x") else {
        return fail!("could not create the fixture file");
    };
    assert_eq_test!(
        file_rmdir_at(&file, b"/"),
        Errno::ENOTDIR.raw(),
        "AT_REMOVEDIR did not refuse a regular file with ENOTDIR"
    );
    assert_eq_test!(
        file_unlink_at(&file, b"/"),
        0,
        "unlinkat did not remove a regular file"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_unlinkat_removedir_separates_file_from_directory,
    suite = syscall_fs_phase1
);

/// Reporting the target for both forms is what makes a symlink invisible to
/// a build system that has to copy one.
pub fn test_fstatat_nofollow_reports_the_link() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(target) = make_file(table, b"link_target", b"hello") else {
        return fail!("could not create the link target");
    };
    let link = join(b"the_link");
    let _ = file_unlink_at(&link, b"/");
    if file_symlink_at(&target, &link, b"/") != 0 {
        return fail!("this filesystem refused a symlink");
    }

    let mut followed = UserFsStat::default();
    assert_eq_test!(
        file_stat_at(&link, b"/", RESOLVE_FOLLOW, &mut followed),
        0,
        "the follow form failed"
    );
    let mut raw = UserFsStat::default();
    assert_eq_test!(
        file_stat_at(&link, b"/", RESOLVE_NOFOLLOW_FINAL, &mut raw),
        0,
        "the nofollow form failed"
    );

    assert_eq_test!(
        followed.st_mode & S_IFMT,
        S_IFREG,
        "the follow form did not report the target"
    );
    assert_eq_test!(
        raw.st_mode & S_IFMT,
        S_IFLNK,
        "the nofollow form did not report the link"
    );
    assert_eq_test!(followed.st_size, 5, "the target's size was not reported");

    let _ = file_unlink_at(&link, b"/");
    let _ = file_unlink_at(&target, b"/");
    pass!()
}

slopos_testing::stest!(
    name = test_fstatat_nofollow_reports_the_link,
    suite = syscall_fs_phase1
);

/// The bug the widened `struct stat` fixes: `FileType as u8` landed in the
/// `FS_TYPE_*` space, so a regular file reported as a directory.
pub fn test_stat_reports_a_regular_file_as_s_ifreg() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let Some(path) = make_file(scratch.table, b"plain.txt", b"0123456789") else {
        return fail!("could not create the fixture file");
    };
    let mut stat = UserFsStat::default();
    assert_eq_test!(
        file_stat_at(&path, b"/", RESOLVE_FOLLOW, &mut stat),
        0,
        "stat of a regular file failed"
    );
    assert_eq_test!(
        stat.st_mode & S_IFMT,
        S_IFREG,
        "a regular file did not report S_IFREG"
    );
    assert_test!(stat.st_ino != 0, "st_ino was left zero");
    assert_eq_test!(stat.st_size, 10, "st_size did not carry the file's length");
    assert_eq_test!(stat.st_blksize, 4096, "st_blksize was not filled");

    let mut dir_stat = UserFsStat::default();
    assert_eq_test!(
        file_stat_at(ROOT, b"/", RESOLVE_FOLLOW, &mut dir_stat),
        0,
        "stat of a directory failed"
    );
    assert_eq_test!(
        dir_stat.st_mode & S_IFMT,
        slopos_abi::fs::S_IFDIR,
        "a directory did not report S_IFDIR"
    );

    let _ = file_unlink_at(&path, b"/");
    pass!()
}

slopos_testing::stest!(
    name = test_stat_reports_a_regular_file_as_s_ifreg,
    suite = syscall_fs_phase1
);

fn collect_dirents(buf: &[u8], names: &mut KVec<KVec<u8>>) -> bool {
    let mut offset = 0usize;
    while offset + 24 <= buf.len() {
        let reclen = u16::from_le_bytes([buf[offset + 16], buf[offset + 17]]) as usize;
        if reclen < 24 || offset + reclen > buf.len() {
            return false;
        }
        let name_start = offset + 24;
        let name_end = buf[name_start..offset + reclen]
            .iter()
            .position(|&b| b == 0)
            .map(|n| name_start + n)
            .unwrap_or(offset + reclen);
        let mut owned = KVec::<u8>::zeroed(name_end - name_start).expect("alloc");
        owned.copy_from_slice(&buf[name_start..name_end]);
        if names.push(owned).is_err() {
            return false;
        }
        offset += reclen;
    }
    offset == buf.len()
}

fn names_contain(names: &KVec<KVec<u8>>, want: &[u8]) -> usize {
    names.iter().filter(|n| n.as_slice() == want).count()
}

static DENT_ENTRIES: [&[u8]; 4] = [b"alpha", b"bravo", b"charlie", b"delta"];

#[inline(never)]
fn dent_path(dir: &[u8], name: &[u8]) -> KVec<u8> {
    let mut path = KVec::<u8>::zeroed(dir.len() + 1 + name.len()).expect("alloc");
    path[..dir.len()].copy_from_slice(dir);
    path[dir.len()] = b'/';
    path[dir.len() + 1..].copy_from_slice(name);
    path
}

#[inline(never)]
fn seed_dirents(table: FdTable, dir: &[u8]) -> bool {
    for name in DENT_ENTRIES.iter() {
        let path = dent_path(dir, name);
        let _ = file_unlink_at(&path, b"/");
        let fd = file_open_at(table, &path, b"/", O_WRONLY | O_CREAT, RESOLVE_FOLLOW, None);
        if fd < 0 {
            return false;
        }
        let _ = file_close_fd(table, fd);
    }
    true
}

#[inline(never)]
fn drop_dirents(dir: &[u8]) {
    for name in DENT_ENTRIES.iter() {
        let path = dent_path(dir, name);
        let _ = file_unlink_at(&path, b"/");
    }
    let _ = file_rmdir_at(dir, b"/");
}

/// The cursor is committed explicitly, as the syscall does after its
/// copy-out: the read itself no longer advances it.
#[inline(never)]
fn drain_dirents(
    table: FdTable,
    dirfd: i32,
    names: &mut KVec<KVec<u8>>,
) -> Result<usize, &'static str> {
    let mut batch = KVec::<u8>::zeroed(64).expect("alloc");
    let mut calls = 0usize;
    loop {
        let (written, cookie) = match file_getdents_fd(table, dirfd, &mut batch) {
            Ok(pair) => pair,
            Err(_) => return Err("getdents64 reported an error"),
        };
        if written == 0 {
            return Ok(calls);
        }
        calls += 1;
        if calls > 32 {
            return Err("getdents64 never reached the end of the directory");
        }
        if !collect_dirents(&batch[..written], names) {
            return Err("a getdents64 batch was not a whole number of records");
        }
        if file_getdents_commit_fd(table, dirfd, cookie).is_err() {
            return Err("the cursor could not be committed");
        }
    }
}

pub fn test_getdents64_resumes_across_calls() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let dir = join(b"dents");
    let _ = file_mkdir_at(&dir, b"/");
    if !seed_dirents(table, &dir) {
        return fail!("could not create a directory entry");
    }

    let dirfd = file_open_at(table, &dir, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(dirfd >= 0, "the directory could not be opened");

    let mut names = KVec::<KVec<u8>>::new();
    let drained = drain_dirents(table, dirfd, &mut names);
    let _ = file_close_fd(table, dirfd);
    let calls = match drained {
        Ok(calls) => calls,
        Err(why) => return fail!("{}", why),
    };

    assert_test!(calls >= 2, "the whole directory fit in one 64-byte batch");
    for name in DENT_ENTRIES.iter() {
        assert_eq_test!(
            names_contain(&names, name),
            1,
            "an entry was skipped or repeated across the resume"
        );
    }
    assert_eq_test!(names_contain(&names, b"."), 1, "'.' was not reported once");
    assert_eq_test!(
        names_contain(&names, b".."),
        1,
        "'..' was not reported once"
    );

    drop_dirents(&dir);
    pass!()
}

slopos_testing::stest!(
    name = test_getdents64_resumes_across_calls,
    suite = syscall_fs_phase1
);

/// Answering `0` would read as end-of-directory and silently lose every
/// remaining entry.
pub fn test_getdents64_rejects_a_buffer_below_one_record() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let dirfd = file_open_at(table, ROOT, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(dirfd >= 0, "the fixture root could not be opened");

    let mut tiny = KVec::<u8>::zeroed(16).expect("alloc");
    let rc = file_getdents_fd(table, dirfd, &mut tiny);
    let _ = file_close_fd(table, dirfd);
    assert_eq_test!(
        rc.err(),
        Some(Errno::EINVAL),
        "a buffer too small for one record did not give EINVAL"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_getdents64_rejects_a_buffer_below_one_record,
    suite = syscall_fs_phase1
);

pub fn test_pread_leaves_the_position_where_read_moves_it() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(path) = make_file(table, b"pread.txt", b"ABCDEFGH") else {
        return fail!("could not create the fixture file");
    };
    let fd = file_open_at(table, &path, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(fd >= 0, "the fixture file could not be opened");

    let mut first = [0u8; 4];
    assert_eq_test!(
        file_read_fd(table, fd, &mut KernelIoBuf::new(&mut first)),
        4,
        "the initial read was short"
    );
    assert_eq_test!(&first, b"ABCD", "the initial read returned the wrong bytes");
    assert_eq_test!(
        file_seek_fd(table, fd, 0, SEEK_CUR as u32),
        4,
        "read did not advance the position"
    );

    let mut at_zero = [0u8; 4];
    assert_eq_test!(
        file_pread_fd(table, fd, &mut KernelIoBuf::new(&mut at_zero), 0),
        4,
        "pread was short"
    );
    assert_eq_test!(&at_zero, b"ABCD", "pread ignored its offset");
    assert_eq_test!(
        file_seek_fd(table, fd, 0, SEEK_CUR as u32),
        4,
        "pread moved the descriptor's position"
    );

    let mut tail = [0u8; 4];
    assert_eq_test!(
        file_read_fd(table, fd, &mut KernelIoBuf::new(&mut tail)),
        4,
        "the follow-up read was short"
    );
    assert_eq_test!(
        &tail,
        b"EFGH",
        "the follow-up read did not continue from the position"
    );

    let _ = file_close_fd(table, fd);
    let _ = file_unlink_at(&path, b"/");
    pass!()
}

slopos_testing::stest!(
    name = test_pread_leaves_the_position_where_read_moves_it,
    suite = syscall_fs_phase1
);

pub fn test_vectored_io_spans_segments_and_the_staging_bound() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(base) = map_user_pages(table, 2) else {
        return fail!("could not map user pages");
    };
    let Some(path) = make_file(table, b"iov.txt", b"") else {
        return fail!("could not create the fixture file");
    };

    const HEAD: usize = 4000;
    const TAIL: usize = 4000;
    let mut source = KVec::<u8>::zeroed(HEAD + TAIL).expect("alloc");
    for (i, slot) in source.iter_mut().enumerate() {
        *slot = (i % 251) as u8;
    }
    assert_test!(
        fill_user_bytes(table, base, &source),
        "could not stage the source bytes"
    );

    let segments = [
        UserIovec {
            iov_base: base,
            iov_len: HEAD as u64,
        },
        UserIovec {
            iov_base: base + HEAD as u64,
            iov_len: 0,
        },
        UserIovec {
            iov_base: base + HEAD as u64,
            iov_len: TAIL as u64,
        },
    ];

    let fd = file_open_at(table, &path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(fd >= 0, "the fixture file could not be opened");

    let written = with_user_process_context(table, || {
        let buf = UserIovecBuf::new(&segments).ok()?;
        Some(file_write_fd(table, fd, &buf))
    })
    .flatten();
    assert_eq_test!(
        written,
        Some((HEAD + TAIL) as isize),
        "writev did not write every segment"
    );

    assert_eq_test!(
        file_seek_fd(table, fd, 0, SEEK_SET as u32),
        0,
        "rewind failed"
    );
    // Read back into the second page only, so the readv path lands in a
    // different place than the writev path read from.
    let read_segments = [
        UserIovec {
            iov_base: base,
            iov_len: 1,
        },
        UserIovec {
            iov_base: base + 1,
            iov_len: 0,
        },
        UserIovec {
            iov_base: base + 1,
            iov_len: (HEAD + TAIL - 1) as u64,
        },
    ];
    let read = with_user_process_context(table, || {
        let mut buf = UserIovecBuf::new(&read_segments).ok()?;
        Some(file_read_fd(table, fd, &mut buf))
    })
    .flatten();
    assert_eq_test!(
        read,
        Some((HEAD + TAIL) as isize),
        "readv did not fill every segment"
    );

    let _ = file_close_fd(table, fd);
    let _ = file_unlink_at(&path, b"/");
    pass!()
}

slopos_testing::stest!(
    name = test_vectored_io_spans_segments_and_the_staging_bound,
    suite = syscall_fs_phase1
);

/// The count is refused before any of the array is read.
pub fn test_iovcnt_above_uio_maxiov_is_refused() -> TestResult {
    let _fixture = SyscallFixture::new();
    assert_eq_test!(
        stage_iovec(0x1000, UIO_MAXIOV + 1).err(),
        Some(Errno::EINVAL),
        "an oversized iovcnt was accepted"
    );
    assert_test!(
        stage_iovec(0, 0).is_ok(),
        "an empty segment list was refused"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_iovcnt_above_uio_maxiov_is_refused,
    suite = syscall_fs_phase1
);

pub fn test_flock_exclusive_conflict_is_eagain() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(path) = make_file(table, b"flock_a.txt", b"x") else {
        return fail!("could not create the fixture file");
    };
    let first = file_open_at(table, &path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let second = file_open_at(table, &path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(first >= 0 && second >= 0, "the fixture opens failed");

    assert_eq_test!(
        file_flock_fd(table, first, (LOCK_EX | LOCK_NB) as u32),
        0,
        "the first LOCK_EX was refused"
    );
    assert_eq_test!(
        file_flock_fd(table, second, (LOCK_EX | LOCK_NB) as u32),
        Errno::EAGAIN.raw(),
        "a second LOCK_EX was granted over the first"
    );
    assert_eq_test!(
        file_flock_fd(table, first, LOCK_UN as u32),
        0,
        "LOCK_UN failed"
    );
    assert_eq_test!(
        file_flock_fd(table, second, (LOCK_EX | LOCK_NB) as u32),
        0,
        "the lock was not released by LOCK_UN"
    );

    let _ = file_flock_fd(table, second, LOCK_UN as u32);
    let _ = file_close_fd(table, first);
    let _ = file_close_fd(table, second);
    let _ = file_unlink_at(&path, b"/");
    pass!()
}

slopos_testing::stest!(
    name = test_flock_exclusive_conflict_is_eagain,
    suite = syscall_fs_phase1
);

/// The lock belongs to the description, not to the descriptor number, so it
/// survives a `dup` and dies with the last alias.
pub fn test_flock_survives_dup_and_dies_with_the_last_close() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(path) = make_file(table, b"flock_b.txt", b"x") else {
        return fail!("could not create the fixture file");
    };
    let holder = file_open_at(table, &path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    let rival = file_open_at(table, &path, b"/", O_RDWR, RESOLVE_FOLLOW, None);
    assert_test!(holder >= 0 && rival >= 0, "the fixture opens failed");

    assert_eq_test!(
        file_flock_fd(table, holder, (LOCK_EX | LOCK_NB) as u32),
        0,
        "LOCK_EX was refused"
    );
    let alias = file_dup_fd(table, holder);
    assert_test!(alias >= 0, "dup failed");

    assert_eq_test!(
        file_close_fd(table, holder),
        0,
        "closing the original failed"
    );
    assert_eq_test!(
        file_flock_fd(table, rival, (LOCK_EX | LOCK_NB) as u32),
        Errno::EAGAIN.raw(),
        "closing one of two descriptors released the description's lock"
    );

    assert_eq_test!(file_close_fd(table, alias), 0, "closing the dup failed");
    assert_eq_test!(
        file_flock_fd(table, rival, (LOCK_EX | LOCK_NB) as u32),
        0,
        "the last close did not release the lock"
    );

    let _ = file_flock_fd(table, rival, LOCK_UN as u32);
    let _ = file_close_fd(table, rival);
    let _ = file_unlink_at(&path, b"/");
    pass!()
}

slopos_testing::stest!(
    name = test_flock_survives_dup_and_dies_with_the_last_close,
    suite = syscall_fs_phase1
);

/// The 256-byte cap is on the CSPRNG's lock hold, not on the request.
pub fn test_getrandom_serves_a_four_kilobyte_request() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    // `dispatch_handler` takes the task only as a `SyscallContext` source: it
    // bypasses ISR entry, so the fixture task's scheduler state is irrelevant.
    let Some(task_guard) = task_find_by_id(scratch.task_id) else {
        return fail!("the fixture task vanished");
    };
    let Some(base) = map_user_pages(table, 2) else {
        return fail!("could not map user pages");
    };

    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = base;
    frame.regs_mut().rsi = 4096;
    frame.regs_mut().rdx = 0;
    let ran = with_user_process_context(table, || {
        crate::syscall::dispatch::dispatch_handler(syscall_getrandom, &task_guard, &mut *frame)
    });
    assert_test!(ran.is_some(), "could not enter the user address space");
    assert_eq_test!(
        frame.rax(),
        4096,
        "a 4 KiB getrandom did not return 4096 bytes"
    );

    // An all-zero tail would mean the loop never ran past the first slice.
    let tail = with_user_process_context(table, || {
        let mut out = [0u8; 256];
        let user = slopos_mm::user_ptr::UserBytes::try_new(base + 3840, 256).ok()?;
        slopos_mm::user_copy::copy_bytes_from_user(user, &mut out).ok()?;
        Some(out.iter().any(|&b| b != 0))
    })
    .flatten();
    assert_eq_test!(tail, Some(true), "the tail of the buffer was never filled");

    drop(task_guard);
    pass!()
}

slopos_testing::stest!(
    name = test_getrandom_serves_a_four_kilobyte_request,
    suite = syscall_fs_phase1
);

// These go through `dispatch_handler` rather than the `file_*` layer: every
// defect below lived in what the handler computed before calling down.
#[inline(never)]
fn call_syscall(
    table: FdTable,
    task: &slopos_sched::task::TaskRef,
    handler: crate::syscall::common::SyscallHandler,
    args: [u64; 6],
) -> Option<i64> {
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    {
        let regs = frame.regs_mut();
        regs.rdi = args[0];
        regs.rsi = args[1];
        regs.rdx = args[2];
        regs.r10 = args[3];
        regs.r8 = args[4];
        regs.r9 = args[5];
    }
    with_user_process_context(table, || {
        crate::syscall::dispatch::dispatch_handler(handler, task, &mut frame);
        frame.rax() as i64
    })
}

fn stage_cstr(table: FdTable, addr: u64, bytes: &[u8]) -> bool {
    let Ok(mut buf) = KVec::<u8>::zeroed(bytes.len() + 1) else {
        return false;
    };
    buf[..bytes.len()].copy_from_slice(bytes);
    fill_user_bytes(table, addr, buf.as_slice())
}

/// Canonicalisation drops the slash as an empty component, so the assertion
/// has to be read at this boundary or not at all — dropping it made
/// `unlink("file/")` remove `file`.
pub fn test_a_trailing_slash_requires_a_directory() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(task) = task_find_by_id(scratch.task_id) else {
        return fail!("the fixture task vanished");
    };
    let Some(base) = map_user_pages(table, 1) else {
        return fail!("could not map user pages");
    };
    let Some(file) = make_file(table, b"slash_target", b"kept") else {
        return fail!("could not create the fixture file");
    };

    let mut slashed = KVec::<u8>::zeroed(file.len() + 1).expect("alloc");
    slashed[..file.len()].copy_from_slice(&file);
    slashed[file.len()] = b'/';
    assert_test!(
        stage_cstr(table, base, slashed.as_slice()),
        "could not stage the slashed path"
    );

    let notdir = Some(Errno::ENOTDIR.raw() as i64);
    assert_eq_test!(
        call_syscall(
            table,
            &task,
            crate::syscall::fs::path_handlers::syscall_stat,
            [base, base + 2048, 0, 0, 0, 0]
        ),
        notdir,
        "stat of a regular file with a trailing slash was not ENOTDIR"
    );
    assert_eq_test!(
        call_syscall(
            table,
            &task,
            crate::syscall::fs::path_handlers::syscall_open,
            [base, O_RDONLY as u64, 0, 0, 0, 0]
        ),
        notdir,
        "open of a regular file with a trailing slash was not ENOTDIR"
    );
    assert_eq_test!(
        call_syscall(
            table,
            &task,
            crate::syscall::fs::path_handlers::syscall_unlink,
            [base, 0, 0, 0, 0, 0]
        ),
        notdir,
        "unlink of a regular file with a trailing slash was not ENOTDIR"
    );
    let survivor = file_open_at(table, &file, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(
        survivor >= 0,
        "unlink with a trailing slash removed the file anyway"
    );
    let _ = file_close_fd(table, survivor);

    assert_test!(stage_cstr(table, base, &file), "could not stage the path");
    assert_eq_test!(
        call_syscall(
            table,
            &task,
            crate::syscall::fs::path_handlers::syscall_open,
            [base, (O_RDONLY | O_DIRECTORY) as u64, 0, 0, 0, 0]
        ),
        notdir,
        "O_DIRECTORY on a regular file was accepted"
    );

    // A directory takes both spellings, or the check is just a refusal.
    assert_test!(stage_cstr(table, base, b"/phase1_fs/"), "could not stage");
    let dirfd = call_syscall(
        table,
        &task,
        crate::syscall::fs::path_handlers::syscall_open,
        [base, (O_RDONLY | O_DIRECTORY) as u64, 0, 0, 0, 0],
    );
    assert_test!(
        matches!(dirfd, Some(fd) if fd >= 0),
        "a directory with a trailing slash and O_DIRECTORY was refused"
    );
    if let Some(fd) = dirfd {
        let _ = file_close_fd(table, fd as i32);
    }

    let _ = file_unlink_at(&file, b"/");
    drop(task);
    pass!()
}

slopos_testing::stest!(
    name = test_a_trailing_slash_requires_a_directory,
    suite = syscall_fs_phase1
);

/// Linux's `path_init` never touches `dfd` for an absolute path, which is why
/// libc wrappers pass whatever descriptor they happen to hold.
pub fn test_openat_ignores_the_dirfd_for_an_absolute_path() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(task) = task_find_by_id(scratch.task_id) else {
        return fail!("the fixture task vanished");
    };
    let Some(base) = map_user_pages(table, 1) else {
        return fail!("could not map user pages");
    };
    let Some(file) = make_file(table, b"absdirfd", b"open me") else {
        return fail!("could not create the fixture file");
    };
    assert_test!(stage_cstr(table, base, &file), "could not stage the path");

    let openat = crate::syscall::fs::at_handlers::syscall_openat;
    // Not `AT_FDCWD` and not a descriptor: previously `EBADF`.
    let junk = call_syscall(
        table,
        &task,
        openat,
        [(-1i32) as u32 as u64, base, O_RDONLY as u64, 0, 0, 0],
    );
    assert_test!(
        matches!(junk, Some(fd) if fd >= 0),
        "an absolute path with a junk dirfd was refused"
    );
    if let Some(fd) = junk {
        let _ = file_close_fd(table, fd as i32);
    }

    // A regular-file descriptor is equally irrelevant: previously `ENOTDIR`.
    let filefd = file_open_at(table, &file, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(filefd >= 0, "could not open the fixture file");
    let via_file = call_syscall(
        table,
        &task,
        openat,
        [filefd as u32 as u64, base, O_RDONLY as u64, 0, 0, 0],
    );
    assert_test!(
        matches!(via_file, Some(fd) if fd >= 0),
        "an absolute path beside a file descriptor was refused"
    );
    if let Some(fd) = via_file {
        let _ = file_close_fd(table, fd as i32);
    }

    // A *relative* path still resolves against the base: the branch narrows
    // nothing else.
    assert_test!(
        stage_cstr(table, base, b"absdirfd"),
        "could not stage the relative name"
    );
    assert_eq_test!(
        call_syscall(
            table,
            &task,
            openat,
            [filefd as u32 as u64, base, O_RDONLY as u64, 0, 0, 0]
        ),
        Some(Errno::ENOTDIR.raw() as i64),
        "a relative path stopped consulting its dirfd"
    );

    let _ = file_close_fd(table, filefd);
    let _ = file_unlink_at(&file, b"/");
    drop(task);
    pass!()
}

slopos_testing::stest!(
    name = test_openat_ignores_the_dirfd_for_an_absolute_path,
    suite = syscall_fs_phase1
);

/// Applying the mode by re-resolving the path is a second walk with a
/// different answer: a mount shadowing the name lands it on the *mounted*
/// filesystem's root, and the `let _ =` on that chmod hid its failure.
pub fn test_mkdirat_modes_the_directory_it_created() -> TestResult {
    const SHADOW: &[u8] = b"/phase1_fs/mkdir_shadow";
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(task) = task_find_by_id(scratch.task_id) else {
        return fail!("the fixture task vanished");
    };
    let Some(base) = map_user_pages(table, 1) else {
        return fail!("could not map user pages");
    };
    let _ = file_rmdir_at(SHADOW, b"/");
    assert_test!(stage_cstr(table, base, SHADOW), "could not stage the path");

    // devfs is a registered static whose node tree is global, so a second
    // mount of it costs no lock class and shows the same tree.
    let devfs = slopos_fs::vfs::init::vfs_devfs_instance();
    if slopos_fs::vfs::mount(SHADOW, devfs, 0).is_err() {
        return fail!("could not mount the shadowing filesystem");
    }

    let rc = call_syscall(
        table,
        &task,
        crate::syscall::fs::at_handlers::syscall_mkdirat,
        [
            (slopos_abi::fs::AT_FDCWD as i64) as u64,
            base,
            0o701,
            0,
            0,
            0,
        ],
    );
    let _ = slopos_fs::vfs::unmount(SHADOW);
    assert_eq_test!(rc, Some(0), "mkdirat under a shadowing mount failed");

    let mut stat = UserFsStat::default();
    assert_eq_test!(
        file_stat_at(SHADOW, b"/", RESOLVE_FOLLOW, &mut stat),
        0,
        "the created directory does not resolve"
    );
    assert_eq_test!(
        stat.st_mode & 0o7777,
        0o701,
        "the mode landed somewhere other than the created directory"
    );

    let _ = file_rmdir_at(SHADOW, b"/");
    drop(task);
    pass!()
}

slopos_testing::stest!(
    name = test_mkdirat_modes_the_directory_it_created,
    suite = syscall_fs_phase1
);

/// The all-`UTIME_OMIT` short-circuit used to answer 0 before the descriptor
/// or the name was looked at, and `UTIME_NOW` without a set wall clock
/// resolved to "omit", so `touch` reported success and moved nothing.
pub fn test_utimensat_validates_before_it_no_ops() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(task) = task_find_by_id(scratch.task_id) else {
        return fail!("the fixture task vanished");
    };
    let Some(base) = map_user_pages(table, 1) else {
        return fail!("could not map user pages");
    };

    // Built byte-wise so no layout assumption about `Timespec` reaches user
    // memory.
    let times_at = base + 2048;
    let mut pair = [0u8; 32];
    pair[8..16].copy_from_slice(&slopos_abi::fs::UTIME_OMIT.to_le_bytes());
    pair[24..32].copy_from_slice(&slopos_abi::fs::UTIME_OMIT.to_le_bytes());
    assert_test!(
        fill_user_bytes(table, times_at, &pair),
        "could not stage the timestamps"
    );

    let utimensat = crate::syscall::fs::at_handlers::syscall_utimensat;
    let at_fdcwd = (slopos_abi::fs::AT_FDCWD as i64) as u64;

    assert_test!(
        stage_cstr(table, base, b"/phase1_fs/utimens_absent"),
        "could not stage the path"
    );
    assert_eq_test!(
        call_syscall(table, &task, utimensat, [at_fdcwd, base, times_at, 0, 0, 0]),
        Some(Errno::ENOENT.raw() as i64),
        "an all-omitted utimensat on a missing name reported success"
    );
    assert_test!(
        stage_cstr(table, base, b"utimens_absent"),
        "could not stage the relative name"
    );
    assert_eq_test!(
        call_syscall(table, &task, utimensat, [999, base, times_at, 0, 0, 0]),
        Some(Errno::EBADF.raw() as i64),
        "an all-omitted utimensat on a junk dirfd reported success"
    );

    // A NULL `times` means "both to now". Without a wall clock that is
    // `EINVAL`; with one, a reported success has to have moved the stamp.
    let Some(file) = make_file(table, b"utimens_target", b"x") else {
        return fail!("could not create the fixture file");
    };
    if slopos_fs::vfs::vfs_utimens(
        &file,
        b"/",
        Some(1_000_000),
        Some(2_000_000),
        RESOLVE_FOLLOW,
    )
    .is_err()
    {
        return fail!("could not park a known mtime");
    }
    assert_test!(stage_cstr(table, base, &file), "could not stage the path");
    let rc = call_syscall(table, &task, utimensat, [at_fdcwd, base, 0, 0, 0, 0]);
    let mut stat = UserFsStat::default();
    assert_eq_test!(
        file_stat_at(&file, b"/", RESOLVE_FOLLOW, &mut stat),
        0,
        "the fixture file does not resolve"
    );
    match rc {
        Some(0) => assert_test!(
            stat.st_mtim.tv_sec != 2_000_000,
            "utimensat reported success without moving the mtime"
        ),
        Some(code) => {
            assert_eq_test!(
                code,
                Errno::EINVAL.raw() as i64,
                "an unstampable utimensat gave neither success nor EINVAL"
            );
            assert_eq_test!(
                stat.st_mtim.tv_sec,
                2_000_000,
                "a refused utimensat moved the mtime anyway"
            );
        }
        None => return fail!("could not enter the user address space"),
    }

    let _ = file_unlink_at(&file, b"/");
    drop(task);
    pass!()
}

slopos_testing::stest!(
    name = test_utimensat_validates_before_it_no_ops,
    suite = syscall_fs_phase1
);

/// A `getdents64` batch is not re-readable: whatever the read consumed is
/// gone from the filesystem's view, so a cursor advanced ahead of a faulting
/// destination loses the batch outright.
pub fn test_getdents64_keeps_its_cursor_when_the_copy_out_faults() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(scratch) = Scratch::new() else {
        return fail!("could not build the fixture");
    };
    let table = scratch.table;
    let Some(task) = task_find_by_id(scratch.task_id) else {
        return fail!("the fixture task vanished");
    };
    let Some(base) = map_user_pages(table, 1) else {
        return fail!("could not map user pages");
    };
    let dir = join(b"dents_fault");
    let _ = file_mkdir_at(&dir, b"/");
    if !seed_dirents(table, &dir) {
        return fail!("could not create a directory entry");
    }

    let dirfd = file_open_at(table, &dir, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    assert_test!(dirfd >= 0, "the directory could not be opened");

    // A user address a long way past the one mapped page: the read runs, the
    // copy-out does not.
    let unmapped = base + (1u64 << 30);
    assert_eq_test!(
        call_syscall(
            table,
            &task,
            crate::syscall::fs::io_handlers::syscall_getdents64,
            [dirfd as u32 as u64, unmapped, 64, 0, 0, 0]
        ),
        Some(Errno::EFAULT.raw() as i64),
        "a getdents64 into unmapped memory did not fault"
    );

    let mut names = KVec::<KVec<u8>>::new();
    if let Err(why) = drain_dirents(table, dirfd, &mut names) {
        let _ = file_close_fd(table, dirfd);
        return fail!("{}", why);
    }
    let _ = file_close_fd(table, dirfd);
    for name in DENT_ENTRIES.iter() {
        assert_eq_test!(
            names_contain(&names, name),
            1,
            "the faulted batch was consumed: an entry is missing or doubled"
        );
    }

    drop_dirents(&dir);
    let _ = file_rmdir_at(&dir, b"/");
    drop(task);
    pass!()
}

slopos_testing::stest!(
    name = test_getdents64_keeps_its_cursor_when_the_copy_out_faults,
    suite = syscall_fs_phase1
);
