//! Named pipes through slibc's `mkfifo`/`mknodat` and std, on the ext2 root
//! (`/var`) and the RAM filesystem behind `/tmp`.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;

use slopos_abi::fs::{S_IFCHR, S_IFIFO, S_IFMT, S_IFSOCK};
use slopos_abi::signal::{SIGKILL, SIGPIPE, SIGUSR1};
use slopos_slibc::errno::{EAGAIN, EEXIST, EINTR, EINVAL, ENXIO, EPERM, EPIPE, ESPIPE};
use slopos_slibc::ffi::syscalls::{mkfifo, mknodat};
use slopos_slibc::ffi::{O_NONBLOCK, O_RDONLY};
use slopos_slibc::signal;
use slopos_slibc::test_harness::note;
use slopos_slibc::types::{sigaction as SigAction, sigset_t as SigSet};
use slopos_userland as _;
use slopos_userland::syscall::{core as sys_core, process};

const EXT2_DIR: &str = "/var/fifo_test";
const TMP_DIR: &str = "/tmp/fifo_test";
const AT_FDCWD: i32 = slopos_abi::fs::AT_FDCWD;

/// A wait for another task to block, and how many before it counts as stuck.
const SETTLE_MS: u32 = 50;
const PATIENCE: u32 = 100;

fn fail(msg: &str) -> bool {
    note(msg);
    false
}

fn fresh(dir: &str, name: &str) -> String {
    let _ = fs::create_dir_all(dir);
    let path = format!("{dir}/{name}");
    let _ = fs::remove_file(&path);
    path
}

fn make_fifo(path: &str, mode: u32) -> Result<(), i32> {
    let c = CString::new(path).expect("no NUL in a test path");
    // SAFETY: `c` is a NUL-terminated string that outlives the call.
    if unsafe { mkfifo(c.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(slopos_slibc::errno_get())
    }
}

fn make_node(path: &str, mode: u32) -> Result<(), i32> {
    let c = CString::new(path).expect("no NUL in a test path");
    // SAFETY: as in `make_fifo`.
    if unsafe { mknodat(AT_FDCWD, c.as_ptr(), mode, 0) } == 0 {
        Ok(())
    } else {
        Err(slopos_slibc::errno_get())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Access {
    Read,
    Write,
    Both,
}

fn open(path: &str, access: Access, nonblock: bool) -> Result<File, i32> {
    let mut options = OpenOptions::new();
    match access {
        Access::Read => options.read(true),
        Access::Write => options.write(true),
        Access::Both => options.read(true).write(true),
    };
    if nonblock {
        options.custom_flags(O_NONBLOCK);
    }
    options
        .open(path)
        .map_err(|e| e.raw_os_error().unwrap_or(-1))
}

fn errno_of(result: std::io::Result<usize>) -> Option<i32> {
    result.err().and_then(|e| e.raw_os_error())
}

fn made_node_is_a_fifo(dir: &str) -> bool {
    let path = fresh(dir, "node");
    if let Err(e) = make_fifo(&path, 0o640) {
        return fail(&format!("mkfifo({path}) failed: errno {e}"));
    }
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) => return fail(&format!("lstat({path}) failed: {e}")),
    };
    if !meta.file_type().is_fifo() || meta.permissions().mode() & 0o7777 != 0o640 {
        return fail(&format!(
            "lstat reports mode {:o}, want a FIFO with 0640",
            meta.permissions().mode()
        ));
    }
    let listed = fs::read_dir(dir).ok().and_then(|entries| {
        entries
            .flatten()
            .find(|e| e.file_name() == "node")
            .and_then(|e| e.file_type().ok())
    });
    if !listed.is_some_and(|t| t.is_fifo()) {
        return fail("getdents64 does not list the node as DT_FIFO");
    }
    if make_fifo(&path, 0o600) != Err(EEXIST.raw()) {
        return fail("a second mkfifo of the same name was not EEXIST");
    }
    let _ = fs::remove_file(&path);
    true
}

fn fifo_on_ext2_root() -> bool {
    made_node_is_a_fifo(EXT2_DIR)
}

fn fifo_on_tmp() -> bool {
    made_node_is_a_fifo(TMP_DIR)
}

fn mknod_refuses_other_node_kinds() -> bool {
    let path = fresh(TMP_DIR, "kinds");
    if make_node(&path, S_IFCHR | 0o600) != Err(EPERM.raw()) {
        return fail("mknod(S_IFCHR) was not EPERM");
    }
    if make_node(&path, S_IFSOCK | 0o600) != Err(EPERM.raw()) {
        return fail("mknod(S_IFSOCK) was not EPERM");
    }
    if make_node(&path, S_IFMT | 0o600) != Err(EINVAL.raw()) {
        return fail("mknod of an unknown type was not EINVAL");
    }
    if make_node(&path, S_IFIFO | 0o600).is_err() {
        return fail("mknod(S_IFIFO) failed");
    }
    let is_fifo = fs::metadata(&path).is_ok_and(|m| m.file_type().is_fifo());
    let _ = fs::remove_file(&path);
    is_fifo || fail("mknod(S_IFIFO) did not make a FIFO")
}

fn nonblocking_writer_without_reader_is_enxio() -> bool {
    let path = fresh(EXT2_DIR, "enxio");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let rc = open(&path, Access::Write, true).err();
    let _ = fs::remove_file(&path);
    rc == Some(ENXIO.raw()) || fail(&format!("open(O_WRONLY|O_NONBLOCK) answered {rc:?}"))
}

fn nonblocking_reader_opens_at_once() -> bool {
    let path = fresh(EXT2_DIR, "nbread");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let mut reader = match open(&path, Access::Read, true) {
        Ok(f) => f,
        Err(e) => return fail(&format!("open(O_RDONLY|O_NONBLOCK) failed: errno {e}")),
    };
    let mut buf = [0u8; 8];
    let first = reader.read(&mut buf);
    let _ = fs::remove_file(&path);
    matches!(first, Ok(0)) || fail(&format!("read with no writer answered {first:?}"))
}

fn read_write_open_does_not_block() -> bool {
    let path = fresh(TMP_DIR, "rdwr");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let mut both = match open(&path, Access::Both, false) {
        Ok(f) => f,
        Err(e) => return fail(&format!("open(O_RDWR) failed: errno {e}")),
    };
    let mut buf = [0u8; 5];
    let round_trip =
        both.write_all(b"hello").is_ok() && both.read_exact(&mut buf).is_ok() && &buf == b"hello";
    let _ = fs::remove_file(&path);
    round_trip || fail("an O_RDWR descriptor did not read back its own write")
}

fn openers_share_one_pipe_until_eof() -> bool {
    let path = fresh(EXT2_DIR, "share");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let Ok(mut reader) = open(&path, Access::Read, true) else {
        return fail("reader open failed");
    };
    let Ok(mut writer) = open(&path, Access::Write, true) else {
        return fail("writer open failed");
    };
    let _ = fs::remove_file(&path);
    if writer.write_all(b"through the node").is_err() {
        return fail("write failed");
    }
    let mut buf = [0u8; 32];
    match reader.read(&mut buf) {
        Ok(16) if &buf[..16] == b"through the node" => {}
        other => return fail(&format!("reader got {other:?}")),
    }
    if errno_of(reader.read(&mut buf)) != Some(EAGAIN.raw()) {
        return fail("an empty pipe with a writer was not EAGAIN");
    }
    drop(writer);
    matches!(reader.read(&mut buf), Ok(0)) || fail("no end of file once the writer closed")
}

/// With `SIGPIPE` ignored, as `main` leaves it, the write reports `EPIPE`.
fn write_without_reader_is_epipe() -> bool {
    let path = fresh(TMP_DIR, "epipe");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let Ok(reader) = open(&path, Access::Read, true) else {
        return fail("reader open failed");
    };
    let Ok(mut writer) = open(&path, Access::Write, true) else {
        return fail("writer open failed");
    };
    let _ = fs::remove_file(&path);
    drop(reader);
    let rc = errno_of(writer.write(b"x"));
    rc == Some(EPIPE.raw()) || fail(&format!("write with no reader answered {rc:?}"))
}

fn write_without_reader_raises_sigpipe() -> bool {
    let path = fresh(TMP_DIR, "sigpipe");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let child = process::fork();
    if child == 0 {
        process::default_signal(SIGPIPE);
        let (Ok(reader), Ok(mut writer)) = (
            open(&path, Access::Read, true),
            open(&path, Access::Write, true),
        ) else {
            sys_core::exit_with_code(2);
        };
        drop(reader);
        let _ = writer.write(b"x");
        sys_core::exit_with_code(1);
    }
    let status = process::waitpid(child as u32).map(|(_, s)| process::wait_status(s));
    let _ = fs::remove_file(&path);
    matches!(status, Some(process::WaitStatus::Signalled(SIGPIPE)))
        || fail("the writer was not killed by SIGPIPE")
}

fn settled(done: &AtomicI32) -> bool {
    for _ in 0..PATIENCE {
        if done.load(Ordering::SeqCst) != 0 {
            return true;
        }
        sys_core::sleep_ms(SETTLE_MS);
    }
    false
}

/// A thread left stuck by a failure dies with the process.
fn blocking_open_is_released_by_partner(waiter: Access, partner: Access, dir: &str) -> bool {
    let path = fresh(dir, "block");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let done = Arc::new(AtomicI32::new(0));
    let flag = Arc::clone(&done);
    let waiter_path = path.clone();
    let handle = thread::spawn(move || {
        let opened = open(&waiter_path, waiter, false);
        flag.store(if opened.is_ok() { 1 } else { -1 }, Ordering::SeqCst);
        opened.ok()
    });
    sys_core::sleep_ms(SETTLE_MS * 2);
    if done.load(Ordering::SeqCst) != 0 {
        return fail("the open did not wait for its partner");
    }
    let Ok(_partner) = open(&path, partner, false) else {
        return fail("the partner's open failed");
    };
    if !settled(&done) {
        return fail("the partner's open did not release the waiter");
    }
    let released = done.load(Ordering::SeqCst) == 1 && handle.join().ok().flatten().is_some();
    let _ = fs::remove_file(&path);
    released || fail("the released open failed")
}

fn blocking_reader_is_released_by_writer() -> bool {
    blocking_open_is_released_by_partner(Access::Read, Access::Write, EXT2_DIR)
}

fn blocking_writer_is_released_by_reader() -> bool {
    blocking_open_is_released_by_partner(Access::Write, Access::Read, TMP_DIR)
}

extern "C" fn on_sigusr1(_sig: i32) {}

fn blocked_open_is_interrupted_by_a_signal() -> bool {
    let path = fresh(TMP_DIR, "eintr");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let c_path = CString::new(path.as_str()).expect("no NUL in a test path");
    let child = process::fork();
    if child == 0 {
        let act = SigAction {
            sa_sigaction: on_sigusr1 as *const () as usize,
            sa_mask: SigSet::empty(),
            sa_flags: 0,
            sa_restorer: None,
        };
        // SAFETY: `act` is a valid sigaction for the duration of the call.
        if unsafe { signal::sigaction(SIGUSR1 as i32, &act, core::ptr::null_mut()) } != 0 {
            sys_core::exit_with_code(2);
        }
        // slibc's `open` rather than std's, which retries on `EINTR`.
        // SAFETY: `c_path` is NUL-terminated and outlives the call.
        let fd = unsafe { slopos_slibc::ffi::open(c_path.as_ptr(), O_RDONLY) };
        if fd < 0 && slopos_slibc::errno_get() == EINTR.raw() {
            sys_core::exit_with_code(0);
        }
        sys_core::exit_with_code(1);
    }
    // Repeated: a signal landing before the child reaches the open is spent
    // on the handler, and the next one finds it blocked.
    let mut code = None;
    for _ in 0..PATIENCE {
        sys_core::sleep_ms(SETTLE_MS);
        let _ = process::kill(child as u32, SIGUSR1);
        code = process::wait_exit_code_nohang(child as u32);
        if code.is_some() {
            break;
        }
    }
    let _ = fs::remove_file(&path);
    if code.is_none() {
        let _ = process::terminate_task(child as u32);
        let _ = process::waitpid(child as u32);
    }
    code == Some(0) || fail(&format!("the interrupted open's child answered {code:?}"))
}

fn blocked_open_yields_to_a_kill() -> bool {
    let path = fresh(EXT2_DIR, "kill");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let child = process::fork();
    if child == 0 {
        let _ = open(&path, Access::Write, false);
        sys_core::exit_with_code(1);
    }
    sys_core::sleep_ms(SETTLE_MS * 2);
    let _ = process::terminate_task(child as u32);
    let status = process::waitpid(child as u32).map(|(_, s)| process::wait_status(s));
    let _ = fs::remove_file(&path);
    matches!(status, Some(process::WaitStatus::Signalled(SIGKILL)))
        || fail("the blocked opener did not die of the kill")
}

fn contents_are_discarded_after_the_last_close() -> bool {
    let path = fresh(EXT2_DIR, "discard");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    {
        let Ok(mut both) = open(&path, Access::Both, false) else {
            return fail("open(O_RDWR) failed");
        };
        if both.write_all(b"stale").is_err() {
            return fail("write failed");
        }
    }
    let Ok(mut reader) = open(&path, Access::Read, true) else {
        return fail("reopen failed");
    };
    let Ok(_writer) = open(&path, Access::Write, true) else {
        return fail("writer reopen failed");
    };
    let mut buf = [0u8; 8];
    let rc = errno_of(reader.read(&mut buf));
    let _ = fs::remove_file(&path);
    rc == Some(EAGAIN.raw()) || fail("a reopened FIFO still held the old contents")
}

fn unlink_and_rename_keep_open_pipes() -> bool {
    let path = fresh(EXT2_DIR, "unlinked");
    let moved = fresh(EXT2_DIR, "renamed");
    if make_fifo(&path, 0o600).is_err() {
        return fail("mkfifo failed");
    }
    let (Ok(mut old_reader), Ok(mut old_writer)) = (
        open(&path, Access::Read, true),
        open(&path, Access::Write, true),
    ) else {
        return fail("opening the first node failed");
    };
    if fs::remove_file(&path).is_err() || make_fifo(&path, 0o600).is_err() {
        return fail("replacing the node failed");
    }
    let Ok(mut new_both) = open(&path, Access::Both, true) else {
        return fail("opening the replacement failed");
    };
    if old_writer.write_all(b"old").is_err() || new_both.write_all(b"new").is_err() {
        return fail("writes failed");
    }
    let mut buf = [0u8; 8];
    match old_reader.read(&mut buf) {
        Ok(3) if &buf[..3] == b"old" => {}
        other => return fail(&format!("the unlinked node's reader got {other:?}")),
    }
    if fs::rename(&path, &moved).is_err() {
        return fail("rename failed");
    }
    let Ok(mut renamed_reader) = open(&moved, Access::Read, true) else {
        return fail("opening the renamed node failed");
    };
    let got = renamed_reader.read(&mut buf);
    let _ = fs::remove_file(&moved);
    matches!(got, Ok(3)) && &buf[..3] == b"new"
        || fail(&format!("the renamed node's new opener got {got:?}"))
}

fn existing_fifo_opens_as_a_pipe() -> bool {
    let path = fresh(TMP_DIR, "creat");
    if make_fifo(&path, 0o620).is_err() {
        return fail("mkfifo failed");
    }
    let Ok(mut both) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
    else {
        return fail("open(O_RDWR|O_CREAT|O_TRUNC) failed");
    };
    let seek = both
        .seek(SeekFrom::Start(0))
        .err()
        .and_then(|e| e.raw_os_error());
    if seek != Some(ESPIPE.raw()) {
        return fail(&format!("lseek answered {seek:?}, want ESPIPE"));
    }
    let meta = both.metadata();
    let _ = fs::remove_file(&path);
    match meta {
        Ok(m) if m.file_type().is_fifo() && m.permissions().mode() & 0o7777 == 0o620 => true,
        Ok(m) => fail(&format!("fstat reports mode {:o}", m.permissions().mode())),
        Err(e) => fail(&format!("fstat failed: {e}")),
    }
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("fifo_on_ext2_root", fifo_on_ext2_root),
    ("fifo_on_tmp", fifo_on_tmp),
    (
        "mknod_refuses_other_node_kinds",
        mknod_refuses_other_node_kinds,
    ),
    (
        "nonblocking_writer_without_reader_is_enxio",
        nonblocking_writer_without_reader_is_enxio,
    ),
    (
        "nonblocking_reader_opens_at_once",
        nonblocking_reader_opens_at_once,
    ),
    (
        "read_write_open_does_not_block",
        read_write_open_does_not_block,
    ),
    (
        "openers_share_one_pipe_until_eof",
        openers_share_one_pipe_until_eof,
    ),
    (
        "write_without_reader_is_epipe",
        write_without_reader_is_epipe,
    ),
    (
        "write_without_reader_raises_sigpipe",
        write_without_reader_raises_sigpipe,
    ),
    (
        "blocking_reader_is_released_by_writer",
        blocking_reader_is_released_by_writer,
    ),
    (
        "blocking_writer_is_released_by_reader",
        blocking_writer_is_released_by_reader,
    ),
    (
        "blocked_open_is_interrupted_by_a_signal",
        blocked_open_is_interrupted_by_a_signal,
    ),
    (
        "blocked_open_yields_to_a_kill",
        blocked_open_yields_to_a_kill,
    ),
    (
        "contents_are_discarded_after_the_last_close",
        contents_are_discarded_after_the_last_close,
    ),
    (
        "unlink_and_rename_keep_open_pipes",
        unlink_and_rename_keep_open_pipes,
    ),
    (
        "existing_fifo_opens_as_a_pipe",
        existing_fifo_opens_as_a_pipe,
    ),
];

fn main() {
    process::ignore_signal(SIGPIPE);
    slopos_slibc::test_harness::run(CASES);
}
