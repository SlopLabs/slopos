use std::fs;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use slopos_abi::syscall::posix::{MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED, PROT_READ, PROT_WRITE};
use slopos_slibc::thread::PTHREAD_PROCESS_SHARED;
use slopos_slibc::thread::mutex::{
    pthread_mutex_init, pthread_mutex_lock, pthread_mutex_t, pthread_mutex_unlock,
    pthread_mutexattr_getpshared, pthread_mutexattr_init, pthread_mutexattr_setpshared,
    pthread_mutexattr_t,
};
use slopos_slibc::thread::semaphore::{sem_init, sem_post, sem_t, sem_timedwait};
use slopos_slibc::time::{CLOCK_REALTIME, Timespec, clock_gettime};
use slopos_userland::apps::shell;
use slopos_userland::syscall::memory;

const EXPECTED: &[u8] = b"piped text\n";

fn test_fork_pipe_echo_tee() -> bool {
    eprintln!("fork_test: pipeline repro start");

    shell::cwd_set(b"/");
    shell::env::initialize();
    shell::exec::initialize_job_control();

    let _ = fs::remove_file("/tmp/tee.txt");

    let mut tokens = shell::buffers::ParsedTokens::new();
    tokens.push_token(b"echo");
    tokens.push_token(b"piped text");
    tokens.push_operator(b"|");
    tokens.push_token(b"tee");
    tokens.push_token(b"/tmp/tee.txt");

    let rc = shell::exec::execute_tokens(&tokens);
    if rc != 0 {
        eprintln!("fork_test: execute_tokens failed (rc={rc})");
        return false;
    }

    let out = match fs::read("/tmp/tee.txt") {
        Ok(out) => out,
        Err(e) => {
            eprintln!("fork_test: verify read failed: {e:?}");
            return false;
        }
    };

    if out.as_slice() != EXPECTED {
        eprintln!("fork_test: verify mismatch");
        return false;
    }

    eprintln!("fork_test: pipeline repro PASS");
    true
}

#[repr(C)]
struct Rendezvous {
    mutex: pthread_mutex_t,
    ready: sem_t,
    value: AtomicU32,
}

const RENDEZVOUS_LEN: u64 = 4096;
const CHILD_VALUE: u32 = 42;

fn deadline_after_secs(secs: i64) -> Timespec {
    let mut now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a live, writable `Timespec`.
    unsafe { clock_gettime(CLOCK_REALTIME, &mut now) };
    Timespec {
        tv_sec: now.tv_sec + secs,
        tv_nsec: now.tv_nsec,
    }
}

/// The wait status, or `None` after killing a child still running at `secs`:
/// one parked on a futex nobody can wake would otherwise hang the suite.
fn reap_within(pid: i32, secs: u32) -> Option<i32> {
    let mut status = 0;
    for _ in 0..secs * 100 {
        // SAFETY: `status` is a live, writable int.
        if unsafe { slopos_slibc::process::waitpid(pid, &mut status, 1) } == pid {
            return Some(status);
        }
        // SAFETY: `usleep` touches no caller memory.
        unsafe { slopos_slibc::time::usleep(10_000) };
    }
    // SAFETY: plain syscalls on a child this test owns.
    unsafe {
        slopos_slibc::signal::kill(pid, slopos_slibc::signal::SIGKILL);
        slopos_slibc::process::waitpid(pid, &mut status, 0);
    }
    None
}

/// The child blocks in the kernel on a mutex the parent holds; only a futex
/// keyed on the shared object lets the parent's unlock wake it.
fn pshared_rendezvous(region: u64, label: &str) -> bool {
    let shared = region as *mut Rendezvous;
    // SAFETY: `region` is a fresh, zero-filled, writable mapping of
    // `RENDEZVOUS_LEN` bytes, which holds a `Rendezvous`.
    unsafe {
        let mut attr = pthread_mutexattr_t { kind: 0 };
        let mut reported = 0;
        if pthread_mutexattr_init(&mut attr) != 0
            || pthread_mutexattr_setpshared(&mut attr, PTHREAD_PROCESS_SHARED) != 0
            || pthread_mutexattr_getpshared(&attr, &mut reported) != 0
            || reported != PTHREAD_PROCESS_SHARED
        {
            eprintln!("fork_test[{label}]: pshared mutex attribute refused");
            return false;
        }
        if pthread_mutex_init(&raw mut (*shared).mutex, &attr) != 0 {
            eprintln!("fork_test[{label}]: pshared mutex init failed");
            return false;
        }
        if sem_init(&raw mut (*shared).ready, 1, 0) != 0 {
            eprintln!("fork_test[{label}]: sem_init(pshared=1) refused");
            return false;
        }
        pthread_mutex_lock(&raw mut (*shared).mutex);

        let pid = slopos_slibc::process::fork();
        if pid < 0 {
            eprintln!("fork_test[{label}]: fork failed");
            return false;
        }
        if pid == 0 {
            sem_post(&raw mut (*shared).ready);
            pthread_mutex_lock(&raw mut (*shared).mutex);
            (*shared).value.store(CHILD_VALUE, Ordering::SeqCst);
            pthread_mutex_unlock(&raw mut (*shared).mutex);
            slopos_slibc::process::_exit(0);
        }

        let deadline = deadline_after_secs(5);
        if sem_timedwait(&raw mut (*shared).ready, &deadline) != 0 {
            eprintln!("fork_test[{label}]: the child's post never arrived");
            let _ = reap_within(pid, 0);
            return false;
        }
        // Unlocking before the child parks proves nothing: wait for its lock
        // attempt to mark the mutex contended (state 2).
        let mut contended = false;
        for _ in 0..200 {
            if (*shared).mutex.state.load(Ordering::SeqCst) == 2 {
                contended = true;
                break;
            }
            slopos_slibc::time::usleep(10_000);
        }
        pthread_mutex_unlock(&raw mut (*shared).mutex);
        let status = reap_within(pid, 5);

        if !contended {
            eprintln!("fork_test[{label}]: the child never contended the mutex");
            return false;
        }
        let Some(status) = status else {
            eprintln!("fork_test[{label}]: the unlock never woke the child");
            return false;
        };
        if status != 0 {
            eprintln!("fork_test[{label}]: child exited with status {status:#x}");
            return false;
        }
        if (*shared).value.load(Ordering::SeqCst) != CHILD_VALUE {
            eprintln!("fork_test[{label}]: the child's write is not visible");
            return false;
        }
    }
    true
}

fn test_pshared_mutex_and_semaphore_over_memfd() -> bool {
    let fd = memory::memfd_create(0);
    if fd < 0 || memory::ftruncate(fd, RENDEZVOUS_LEN) < 0 {
        eprintln!("fork_test: memfd setup failed");
        return false;
    }
    let region = memory::mmap(
        0,
        RENDEZVOUS_LEN,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd as i64,
        0,
    );
    let _ = memory::close(fd);
    if region == 0 || (region as i64) < 0 {
        eprintln!("fork_test: memfd mmap failed");
        return false;
    }
    let ok = pshared_rendezvous(region, "memfd");
    let _ = memory::munmap(region, RENDEZVOUS_LEN);
    ok
}

fn test_pshared_mutex_and_semaphore_over_shm_file() -> bool {
    const PATH: &str = "/dev/shm/fork_test_pshared";
    let _ = fs::remove_file(PATH);
    let file = match fs::File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(PATH)
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!("fork_test: shm create failed: {e:?}");
            return false;
        }
    };
    if file.set_len(RENDEZVOUS_LEN).is_err() {
        eprintln!("fork_test: shm size failed");
        let _ = fs::remove_file(PATH);
        return false;
    }
    let region = memory::mmap(
        0,
        RENDEZVOUS_LEN,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        file.as_raw_fd() as i64,
        0,
    );
    drop(file);
    if region == 0 || (region as i64) < 0 {
        eprintln!("fork_test: shm mmap failed");
        let _ = fs::remove_file(PATH);
        return false;
    }
    let ok = pshared_rendezvous(region, "shm");
    let _ = memory::munmap(region, RENDEZVOUS_LEN);
    let _ = fs::remove_file(PATH);
    ok
}

/// Resident pages the parent touches before forking, so each fork's window
/// between marking the parent copy-on-write and returning is long.
const BALLAST_BYTES: u64 = 16 << 20;
const FORK_ROUNDS: u32 = 100;

/// A sibling spinning on a page while another thread forks must never lose a
/// write: its CPU caches the page writable from before the fork, and if a third
/// thread's fault copies the page before that translation is shot down, every
/// write the sibling makes in between lands in the frame the child keeps.
fn test_fork_keeps_sibling_writes() -> bool {
    let ballast = memory::mmap(
        0,
        BALLAST_BYTES,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    let page = memory::mmap(
        0,
        4096,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    if ballast == 0 || (ballast as i64) < 0 || page == 0 || (page as i64) < 0 {
        eprintln!("fork_test: sibling-write mappings failed");
        return false;
    }
    for offset in (0..BALLAST_BYTES).step_by(4096) {
        // SAFETY: inside the fresh writable mapping of `BALLAST_BYTES`.
        unsafe { ((ballast + offset) as *mut u8).write_volatile(1) };
    }
    // SAFETY: a fresh, zero-filled, writable, page-aligned page holds two
    // `AtomicU64`s, which every thread below only touches atomically.
    let (spun, poked) = unsafe {
        (
            &*(page as *const AtomicU64),
            &*((page + 64) as *const AtomicU64),
        )
    };

    let stop = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicU64::new(0));
    let spinner = {
        let (stop, lost) = (stop.clone(), lost.clone());
        std::thread::spawn(move || {
            let mut expected = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let seen = spun.load(Ordering::Relaxed);
                if seen != expected {
                    lost.store(expected.wrapping_sub(seen), Ordering::SeqCst);
                    return;
                }
                expected += 1;
                spun.store(expected, Ordering::Relaxed);
            }
        })
    };
    // Wakes from a sleep, so its CPU holds no translation of the page and its
    // write faults: the copy that strands the spinner's writes.
    let poker = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: `usleep` touches no caller memory.
                unsafe { slopos_slibc::time::usleep(200) };
                poked.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut rounds = 0;
    let mut forked = true;
    while rounds < FORK_ROUNDS && lost.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        // SAFETY: the child calls nothing but `_exit`, which a forked copy of
        // a threaded process may.
        let pid = unsafe { slopos_slibc::process::fork() };
        if pid == 0 {
            // SAFETY: ends the child at once, as the comment above relies on.
            unsafe { slopos_slibc::process::_exit(0) };
        }
        if pid < 0 || reap_within(pid, 5).is_none() {
            forked = false;
            break;
        }
        rounds += 1;
    }
    stop.store(true, Ordering::SeqCst);
    let _ = spinner.join();
    let _ = poker.join();
    let _ = memory::munmap(page, 4096);
    let _ = memory::munmap(ballast, BALLAST_BYTES);

    let lost = lost.load(Ordering::SeqCst);
    if !forked {
        eprintln!("fork_test: fork {rounds} failed or never exited");
        return false;
    }
    if lost != 0 {
        eprintln!("fork_test: a sibling lost {lost} writes to its page across fork {rounds}");
        return false;
    }
    true
}

const COPY_FORKERS: usize = 2;
const COPY_FORKS_EACH: u32 = 150;

/// A `read` into a page user code never touched must land while siblings
/// fork: the kernel's first copy finds the page absent and maps it, and a
/// sibling's fork can make the fresh page copy-on-write before the copy runs
/// again.
fn test_reads_into_fresh_pages_across_sibling_forks() -> bool {
    let ballast = memory::mmap(
        0,
        BALLAST_BYTES,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    if ballast == 0 || (ballast as i64) < 0 {
        eprintln!("fork_test: copy ballast mapping failed");
        return false;
    }
    for offset in (0..BALLAST_BYTES).step_by(4096) {
        // SAFETY: inside the fresh writable mapping of `BALLAST_BYTES`.
        unsafe { ((ballast + offset) as *mut u8).write_volatile(1) };
    }

    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = stop.clone();
        std::thread::spawn(move || -> Result<u64, String> {
            use std::io::{Read, Write};
            let (mut rx, mut tx) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
            let mut reads = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let page = memory::mmap(
                    0,
                    4096,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    -1,
                    0,
                );
                if page == 0 || (page as i64) < 0 {
                    return Err("page mapping failed".into());
                }
                tx.write_all(&[7]).map_err(|e| format!("write: {e}"))?;
                // SAFETY: a fresh writable page of which nothing else holds a
                // reference, unmapped only after the slice's last use.
                let fresh = unsafe { core::slice::from_raw_parts_mut(page as *mut u8, 1) };
                let read = rx.read_exact(fresh);
                let _ = memory::munmap(page, 4096);
                read.map_err(|e| format!("read {reads}: {e}"))?;
                reads += 1;
            }
            Ok(reads)
        })
    };
    let forkers: Vec<_> = (0..COPY_FORKERS)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..COPY_FORKS_EACH {
                    // SAFETY: the child calls nothing but `_exit`.
                    let pid = unsafe { slopos_slibc::process::fork() };
                    if pid == 0 {
                        // SAFETY: ends the child at once.
                        unsafe { slopos_slibc::process::_exit(0) };
                    }
                    if pid < 0 || reap_within(pid, 5).is_none() {
                        return false;
                    }
                }
                true
            })
        })
        .collect();
    let forked = forkers.into_iter().all(|f| f.join().unwrap_or(false));
    stop.store(true, Ordering::SeqCst);
    let read = reader
        .join()
        .unwrap_or_else(|_| Err("reader panicked".into()));
    let _ = memory::munmap(ballast, BALLAST_BYTES);
    if !forked {
        eprintln!("fork_test: a sibling fork failed or never exited");
        return false;
    }
    match read {
        Ok(_) => true,
        Err(e) => {
            eprintln!("fork_test: a read beside sibling forks failed: {e}");
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("fork_pipe_echo_tee", test_fork_pipe_echo_tee),
        (
            "pshared_mutex_and_semaphore_over_memfd",
            test_pshared_mutex_and_semaphore_over_memfd,
        ),
        (
            "pshared_mutex_and_semaphore_over_shm_file",
            test_pshared_mutex_and_semaphore_over_shm_file,
        ),
        ("fork_keeps_sibling_writes", test_fork_keeps_sibling_writes),
        (
            "reads_into_fresh_pages_across_sibling_forks",
            test_reads_into_fresh_pages_across_sibling_forks,
        ),
    ]);
}
