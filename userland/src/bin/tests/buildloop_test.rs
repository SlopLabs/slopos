//! The build loop's plumbing, as cargo and rustc use it: a jobserver's
//! tokens crossing `exec`, `std`'s file locks between processes, an rlib
//! mapped read-only, and a memory budget that refuses at `mmap` and `fork`
//! rather than killing a process at its first touch.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use slopos_abi::syscall::posix::{
    MAP_ANONYMOUS, MAP_NORESERVE, MAP_PRIVATE, POLLIN, PROT_READ, PROT_WRITE,
};
use slopos_abi::syscall::{F_GETFD, F_SETFD, FD_CLOEXEC, O_CLOEXEC, UserPollFd};
use slopos_userland as _;
use slopos_userland::syscall::{OwnedFd, UserSysInfo};
use slopos_userland::syscall::{core as sys_core, fs as sys_fs, memory, process};

const SELF_PATH: &str = "/bin/buildloop_test";
const WORK: &str = "/tmp/buildloop";
const PAGE: u64 = 4096;
const TOKENS: usize = 2;
const WORKERS: usize = 6;

unsafe extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

fn set_cloexec(fd: RawFd, on: bool) -> std::io::Result<()> {
    let previous = unsafe { fcntl(fd, F_GETFD as i32) };
    if previous < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let next = if on {
        previous | FD_CLOEXEC as i32
    } else {
        previous & !(FD_CLOEXEC as i32)
    };
    if unsafe { fcntl(fd, F_SETFD as i32, next) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn has_cloexec(fd: RawFd) -> bool {
    let flags = unsafe { fcntl(fd, F_GETFD as i32) };
    flags & FD_CLOEXEC as i32 != 0
}

fn sys_info() -> UserSysInfo {
    let mut info = UserSysInfo::default();
    sys_core::sys_info(&mut info);
    info
}

fn note(msg: String) {
    slopos_slibc::test_harness::note(&msg);
}

fn map_anonymous(pages: u64, flags: u64) -> Option<u64> {
    let addr = memory::mmap(
        0,
        pages * PAGE,
        PROT_READ | PROT_WRITE,
        MAP_ANONYMOUS | MAP_PRIVATE | flags,
        -1,
        0,
    );
    ((addr as i64) > 0).then_some(addr)
}

/// A worker's view of the jobserver: the two descriptors named in the
/// environment, exactly as the `jobserver` crate parses them.
fn jobserver_fds() -> Option<(RawFd, RawFd)> {
    let flags = std::env::var("CARGO_MAKEFLAGS").ok()?;
    let auth = flags
        .split_whitespace()
        .rev()
        .find_map(|arg| arg.strip_prefix("--jobserver-auth="))?;
    let (r, w) = auth.split_once(',')?;
    Some((r.parse().ok()?, w.parse().ok()?))
}

struct Counter {
    current: i32,
    peak: i32,
    done: i32,
}

/// Move the shared concurrency counter under `std`'s file lock, the way
/// cargo guards its target directory.
fn bump(path: &str, delta: i32) -> std::io::Result<Counter> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.lock()?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut words = text
        .split_whitespace()
        .map(|w| w.parse::<i32>().unwrap_or(0));
    let mut counter = Counter {
        current: words.next().unwrap_or(0),
        peak: words.next().unwrap_or(0),
        done: words.next().unwrap_or(0),
    };
    counter.current += delta;
    counter.peak = counter.peak.max(counter.current);
    if delta < 0 {
        counter.done += 1;
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(
        file,
        "{} {} {}",
        counter.current, counter.peak, counter.done
    )?;
    file.unlock()?;
    Ok(counter)
}

fn worker_mode(counter: &str) -> i32 {
    let Some((r, w)) = jobserver_fds() else {
        return 10;
    };
    let read_end = ManuallyDrop::new(unsafe { File::from_raw_fd(r) });
    match read_end.metadata() {
        Ok(meta) if meta.file_type().is_fifo() => {}
        _ => return 11,
    }
    let Ok(owned) = unsafe { BorrowedFd::borrow_raw(r) }.try_clone_to_owned() else {
        return 12;
    };
    if !has_cloexec(owned.as_raw_fd()) {
        return 13;
    }
    let mut pollfd = [UserPollFd {
        fd: owned.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    }];
    if sys_fs::poll(&mut pollfd, -1).is_err() || pollfd[0].revents & POLLIN == 0 {
        return 14;
    }
    let mut token = [0u8; 1];
    let mut reader = File::from(owned);
    if reader.read_exact(&mut token).is_err() {
        return 15;
    }
    if bump(counter, 1).is_err() {
        return 16;
    }
    std::thread::sleep(Duration::from_millis(30));
    if bump(counter, -1).is_err() {
        return 17;
    }
    let mut write_end = ManuallyDrop::new(unsafe { File::from_raw_fd(w) });
    if write_end.write_all(&token).is_err() {
        return 18;
    }
    0
}

fn lockprobe_mode(mode: &str, path: &str) -> i32 {
    let Ok(file) = File::open(path) else {
        return 4;
    };
    match mode {
        "try" => match file.try_lock() {
            Ok(()) => 0,
            Err(std::fs::TryLockError::WouldBlock) => 3,
            Err(_) => 4,
        },
        "shared" => match file.lock_shared() {
            Ok(()) => 0,
            Err(_) => 4,
        },
        _ => 4,
    }
}

fn hold_mode() -> i32 {
    let Some(addr) = map_anonymous(2048, 0) else {
        return 5;
    };
    for page in 0..2048u64 {
        unsafe { ((addr + page * PAGE) as *mut u64).write_volatile(page) };
    }
    0
}

/// A jobserver pipe, its tokens shared by children that inherit the ends
/// across `exec` exactly as `jobserver::Client::configure` arranges it.
struct JobServer {
    read: OwnedFd,
    write: OwnedFd,
}

impl JobServer {
    fn new(tokens: usize) -> Option<Self> {
        let (read, write) = sys_fs::pipe2(O_CLOEXEC as u32).ok()?;
        let server = Self { read, write };
        server.release(tokens).then_some(server)
    }

    fn release(&self, tokens: usize) -> bool {
        let mut write_end = ManuallyDrop::new(unsafe { File::from_raw_fd(self.write.raw()) });
        write_end.write_all(&vec![b'|'; tokens]).is_ok()
    }

    fn worker(&self, counter: &str) -> Command {
        let (r, w) = (self.read.raw(), self.write.raw());
        let mut cmd = Command::new(SELF_PATH);
        cmd.arg("worker").arg(counter).env(
            "CARGO_MAKEFLAGS",
            format!("-j --jobserver-fds={r},{w} --jobserver-auth={r},{w}"),
        );
        unsafe {
            cmd.pre_exec(move || {
                set_cloexec(r, false)?;
                set_cloexec(w, false)
            });
        }
        cmd
    }
}

fn fresh_counter(name: &str) -> Option<String> {
    fs::create_dir_all(WORK).ok()?;
    let path = format!("{WORK}/{name}");
    fs::write(&path, "0 0 0\n").ok()?;
    Some(path)
}

fn test_jobserver_tokens_cross_exec_and_bound_the_workers() -> bool {
    let Some(counter) = fresh_counter("tokens") else {
        return false;
    };
    let Some(server) = JobServer::new(TOKENS) else {
        return false;
    };
    let mut children = Vec::new();
    for _ in 0..WORKERS {
        match server.worker(&counter).spawn() {
            Ok(child) => children.push(child),
            Err(e) => {
                note(format!("spawn failed: {e}"));
                return false;
            }
        }
    }
    let mut ok = true;
    for mut child in children {
        match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                note(format!("worker exited {status}"));
                ok = false;
            }
            Err(e) => {
                note(format!("wait failed: {e}"));
                ok = false;
            }
        }
    }
    let Ok(final_state) = bump(&counter, 0) else {
        return false;
    };
    note(format!(
        "{} workers over {} tokens: peak concurrency {}, {} completions",
        WORKERS, TOKENS, final_state.peak, final_state.done
    ));
    ok && final_state.peak <= TOKENS as i32
        && final_state.peak >= 1
        && final_state.done == WORKERS as i32
}

fn test_an_empty_jobserver_blocks_the_reader_until_a_token_arrives() -> bool {
    let Some(counter) = fresh_counter("blocking") else {
        return false;
    };
    let Some(server) = JobServer::new(0) else {
        return false;
    };
    let started = Instant::now();
    let Ok(mut child) = server.worker(&counter).spawn() else {
        return false;
    };
    let delay = Duration::from_millis(150);
    std::thread::sleep(delay);
    if !server.release(1) {
        return false;
    }
    let Ok(status) = child.wait() else {
        return false;
    };
    note(format!(
        "the reader woke after {} ms, {} ms after the token",
        started.elapsed().as_millis(),
        started.elapsed().saturating_sub(delay).as_millis()
    ));
    status.success()
}

fn probe(mode: &str, path: &str) -> Option<i32> {
    Command::new(SELF_PATH)
        .arg("lockprobe")
        .arg(mode)
        .arg(path)
        .status()
        .ok()?
        .code()
}

fn test_std_file_locks_exclude_a_second_process() -> bool {
    let path = format!("{WORK}/lock");
    if fs::create_dir_all(WORK).is_err() {
        return false;
    }
    let Ok(file) = File::create(&path) else {
        return false;
    };
    if file.lock().is_err() {
        note("File::lock is unsupported".to_string());
        return false;
    }
    let while_held = probe("try", &path);
    if file.unlock().is_err() {
        return false;
    }
    let after_unlock = probe("try", &path);
    if file.lock_shared().is_err() {
        return false;
    }
    let shared_beside_shared = probe("shared", &path);
    let exclusive_beside_shared = probe("try", &path);
    let _ = file.unlock();
    note(format!(
        "held: {while_held:?}, unlocked: {after_unlock:?}, shared: {shared_beside_shared:?}, exclusive beside shared: {exclusive_beside_shared:?}"
    ));
    while_held == Some(3)
        && after_unlock == Some(0)
        && shared_beside_shared == Some(0)
        && exclusive_beside_shared == Some(3)
}

fn rlib_byte(index: u64) -> u8 {
    (index.wrapping_mul(2654435761) >> 7) as u8
}

fn write_rlib(name: &str, len: u64) -> Option<String> {
    fs::create_dir_all(WORK).ok()?;
    let path = format!("{WORK}/{name}");
    let bytes: Vec<u8> = (0..len).map(rlib_byte).collect();
    fs::write(&path, bytes).ok()?;
    Some(path)
}

fn test_an_rlib_maps_private_read_only_and_costs_no_commit() -> bool {
    const LEN: u64 = 3 * 1024 * 1024 + 4099;
    let Some(path) = write_rlib("dep.rlib", LEN) else {
        return false;
    };
    let Ok(file) = File::open(&path) else {
        return false;
    };
    let before = sys_info().committed_pages;
    let extent = LEN.div_ceil(PAGE) * PAGE;
    let addr = memory::mmap(
        0,
        extent,
        PROT_READ,
        MAP_PRIVATE,
        file.as_raw_fd() as i64,
        0,
    );
    if (addr as i64) <= 0 {
        return false;
    }
    let after = sys_info().committed_pages;
    let intact = [0u64, 1024 * 1024 + 17, LEN - 1]
        .iter()
        .all(|&i| unsafe { ((addr + i) as *const u8).read_volatile() } == rlib_byte(i));
    memory::munmap(addr, extent);
    note(format!("commit before {before}, after mapping {after}"));
    intact && after == before
}

fn test_a_reservation_past_the_ceiling_is_refused_not_killed() -> bool {
    let info = sys_info();
    if info.commit_limit_pages == u32::MAX {
        note("no commit ceiling is configured".to_string());
        return true;
    }
    let headroom = info.commit_headroom_pages as u64;
    let refused = map_anonymous(headroom + 65536, 0);
    if let Some(addr) = refused {
        memory::munmap(addr, (headroom + 65536) * PAGE);
        note(format!(
            "{} pages past the ceiling were granted",
            headroom + 65536
        ));
        return false;
    }
    let Some(addr) = map_anonymous(1024, 0) else {
        return false;
    };
    for page in 0..1024u64 {
        unsafe { ((addr + page * PAGE) as *mut u64).write_volatile(page) };
    }
    memory::munmap(addr, 1024 * PAGE);
    let Some(sparse) = map_anonymous(headroom + 65536, MAP_NORESERVE) else {
        note("MAP_NORESERVE past the ceiling was refused".to_string());
        return false;
    };
    memory::munmap(sparse, (headroom + 65536) * PAGE);
    note(format!(
        "ceiling {} pages, headroom {}: the over-ask was refused, a fit was granted",
        info.commit_limit_pages, headroom
    ));
    true
}

/// Most of the headroom, promised but untouched: what a compiler holds when
/// it goes to spawn its linker. `Ok(None)` is a machine with no ceiling.
fn reserve_most_of_the_headroom() -> Result<Option<(u64, u64)>, String> {
    let info = sys_info();
    if info.commit_limit_pages == u32::MAX {
        return Ok(None);
    }
    let pages = (info.commit_headroom_pages as u64) * 6 / 10;
    match map_anonymous(pages, 0) {
        Some(addr) => Ok(Some((addr, pages * PAGE))),
        None => Err(format!(
            "reserving {pages} of {} headroom pages was refused",
            info.commit_headroom_pages
        )),
    }
}

fn test_fork_past_the_ceiling_is_refused_not_killed() -> bool {
    let (addr, len) = match reserve_most_of_the_headroom() {
        Ok(Some(held)) => held,
        Ok(None) => {
            note("no commit ceiling is configured".to_string());
            return true;
        }
        Err(why) => {
            note(why);
            return false;
        }
    };
    let refused = process::fork();
    if refused == 0 {
        std::process::exit(0);
    }
    if refused > 0 {
        process::wait_exit_code(refused as u32);
    }
    memory::munmap(addr, len);
    let granted = process::fork();
    if granted == 0 {
        std::process::exit(0);
    }
    let child_ok = granted > 0 && process::wait_exit_code(granted as u32) == 0;
    note(format!(
        "fork with the reservation held: {refused}; without it: {granted}"
    ));
    refused < 0 && child_ok
}

fn test_posix_spawn_from_a_large_process_needs_no_second_commit() -> bool {
    let (addr, len) = match reserve_most_of_the_headroom() {
        Ok(Some(held)) => held,
        Ok(None) => {
            note("no commit ceiling is configured".to_string());
            return true;
        }
        Err(why) => {
            note(why);
            return false;
        }
    };
    let status = Command::new(SELF_PATH).arg("noop").status();
    memory::munmap(addr, len);
    match status {
        Ok(status) => status.success(),
        Err(e) => {
            note(format!("spawn failed: {e}"));
            false
        }
    }
}

/// A child's promise comes back when its address space is torn down, on the
/// CPU it left and not necessarily before `wait` returns here.
fn test_commit_is_returned_when_a_process_exits() -> bool {
    let before = sys_info().committed_pages;
    let Ok(status) = Command::new(SELF_PATH).arg("hold").status() else {
        return false;
    };
    let started = Instant::now();
    let mut after = sys_info().committed_pages;
    while after > before && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
        after = sys_info().committed_pages;
    }
    note(format!(
        "committed before {before}, {after} once the child was torn down ({} ms)",
        started.elapsed().as_millis()
    ));
    status.success() && after == before
}

fn test_hundreds_of_pipes_are_available() -> bool {
    let mut pipes = Vec::new();
    for _ in 0..100 {
        match sys_fs::pipe2(O_CLOEXEC as u32) {
            Ok(pair) => pipes.push(pair),
            Err(e) => {
                note(format!("pipe {} refused: {:?}", pipes.len() + 1, e));
                return false;
            }
        }
    }
    true
}

fn test_dozens_of_rlibs_map_at_once() -> bool {
    const RLIBS: usize = 48;
    let mut mapped = Vec::new();
    for i in 0..RLIBS {
        let Some(path) = write_rlib(&format!("dep{i}.rlib"), 2 * PAGE + 1) else {
            return false;
        };
        let Ok(file) = File::open(&path) else {
            return false;
        };
        let addr = memory::mmap(
            0,
            3 * PAGE,
            PROT_READ,
            MAP_PRIVATE,
            file.as_raw_fd() as i64,
            0,
        );
        if (addr as i64) <= 0 {
            note(format!("rlib {} refused a mapping", i + 1));
            return false;
        }
        if unsafe { (addr as *const u8).read_volatile() } != rlib_byte(0) {
            return false;
        }
        mapped.push((addr, file));
    }
    for (addr, _) in mapped {
        memory::munmap(addr, 3 * PAGE);
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "jobserver_tokens_cross_exec_and_bound_the_workers",
        test_jobserver_tokens_cross_exec_and_bound_the_workers,
    ),
    (
        "an_empty_jobserver_blocks_the_reader_until_a_token_arrives",
        test_an_empty_jobserver_blocks_the_reader_until_a_token_arrives,
    ),
    (
        "std_file_locks_exclude_a_second_process",
        test_std_file_locks_exclude_a_second_process,
    ),
    (
        "an_rlib_maps_private_read_only_and_costs_no_commit",
        test_an_rlib_maps_private_read_only_and_costs_no_commit,
    ),
    (
        "a_reservation_past_the_ceiling_is_refused_not_killed",
        test_a_reservation_past_the_ceiling_is_refused_not_killed,
    ),
    (
        "fork_past_the_ceiling_is_refused_not_killed",
        test_fork_past_the_ceiling_is_refused_not_killed,
    ),
    (
        "posix_spawn_from_a_large_process_needs_no_second_commit",
        test_posix_spawn_from_a_large_process_needs_no_second_commit,
    ),
    (
        "commit_is_returned_when_a_process_exits",
        test_commit_is_returned_when_a_process_exits,
    ),
    (
        "hundreds_of_pipes_are_available",
        test_hundreds_of_pipes_are_available,
    ),
    (
        "dozens_of_rlibs_map_at_once",
        test_dozens_of_rlibs_map_at_once,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("worker") => worker_mode(args.get(1).map(String::as_str).unwrap_or("")),
        Some("lockprobe") => lockprobe_mode(
            args.get(1).map(String::as_str).unwrap_or(""),
            args.get(2).map(String::as_str).unwrap_or(""),
        ),
        Some("hold") => hold_mode(),
        Some("noop") => 0,
        _ => slopos_slibc::test_harness::run_with_progress("buildloop", CASES),
    };
    std::process::exit(code);
}
