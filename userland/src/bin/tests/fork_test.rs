use std::fs;
use std::os::fd::AsRawFd;

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::syscall::posix::{MAP_SHARED, PROT_READ, PROT_WRITE};
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

/// A process-shared mutex and semaphore in one `MAP_SHARED` region, which the
/// forked child sees at the same address but through its own address space.
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

/// Reap `pid` within `secs`, or kill it: a child parked on a futex nobody can
/// wake would otherwise hang the suite. The wait status, or `None` on timeout.
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

/// The parent holds a `PTHREAD_PROCESS_SHARED` mutex; the child posts a shared
/// semaphore, then blocks on the mutex in the kernel. Only a futex keyed on
/// the shared object lets the parent's unlock wake it.
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
        // Unlocking before the child parks proves nothing: wait until its
        // lock attempt has marked the mutex contended.
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
    ]);
}
