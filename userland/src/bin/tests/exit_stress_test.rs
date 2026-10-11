//! Process exit under load: every road that tears an address space down —
//! a spawned child ending, a forked child ending, a child that forks and
//! ends — hundreds of times over, on every CPU at once.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicU32;

use slopos_abi::syscall::SYSCALL_FUTEX;
use slopos_abi::syscall::posix::{
    FUTEX_PRIVATE_FLAG, FUTEX_WAIT, FUTEX_WAKE, MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE,
};
use slopos_userland as _;
use slopos_userland::syscall::{core as sys_core, memory, process};

const SELF_PATH: &str = "/bin/exit_stress_test";
const PAGE: u64 = 4096;
const THREADS: usize = 4;
const SPAWNS_PER_THREAD: usize = 60;
const FORK_WORKERS: usize = 3;
const FORKS_PER_WORKER: usize = 60;
const TOUCH_PAGES: u64 = 64;

fn note(msg: String) {
    slopos_slibc::test_harness::note(&msg);
}

fn touch_some_memory() -> bool {
    let addr = memory::mmap(
        0,
        TOUCH_PAGES * PAGE,
        PROT_READ | PROT_WRITE,
        MAP_ANONYMOUS | MAP_PRIVATE,
        -1,
        0,
    );
    if (addr as i64) <= 0 {
        return false;
    }
    for page in 0..TOUCH_PAGES {
        unsafe { core::ptr::write_volatile((addr + page * PAGE) as *mut u64, page) };
    }
    true
}

/// A child that holds memory, forks a grandchild that ends at once, waits for
/// it, and ends itself.
fn child_mode() -> i32 {
    if !touch_some_memory() {
        return 2;
    }
    let pid = process::fork();
    if pid == 0 {
        sys_core::exit_with_code(0);
    }
    if pid < 0 {
        return 3;
    }
    process::wait_exit_code(pid as u32)
}

fn spawn_storm(mode: &'static str, per_thread: usize) -> bool {
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..per_thread {
                    let out = match Command::new(SELF_PATH).arg(mode).output() {
                        Ok(out) => out,
                        Err(e) => {
                            note(format!("thread {t} spawn {i}: {e}"));
                            return false;
                        }
                    };
                    if !out.status.success() {
                        note(format!(
                            "thread {t} spawn {i}: exit {:?}",
                            out.status.code()
                        ));
                        return false;
                    }
                }
                true
            })
        })
        .collect();
    handles.into_iter().all(|h| h.join().unwrap_or(false))
}

fn test_spawned_children_end_under_load() -> bool {
    let ok = spawn_storm("noop", SPAWNS_PER_THREAD);
    note(format!(
        "{} spawns across {THREADS} threads",
        THREADS * SPAWNS_PER_THREAD
    ));
    ok
}

fn test_forked_children_end_under_load() -> bool {
    let mut workers = Vec::new();
    for _ in 0..FORK_WORKERS {
        let pid = process::fork();
        if pid == 0 {
            for _ in 0..FORKS_PER_WORKER {
                if !touch_some_memory() {
                    sys_core::exit_with_code(2);
                }
                let child = process::fork();
                if child == 0 {
                    sys_core::exit_with_code(0);
                }
                if child < 0 || process::wait_exit_code(child as u32) != 0 {
                    sys_core::exit_with_code(3);
                }
            }
            sys_core::exit_with_code(0);
        }
        if pid < 0 {
            note("worker fork failed".to_string());
            return false;
        }
        workers.push(pid as u32);
    }
    let mut ok = true;
    for pid in workers {
        let code = process::wait_exit_code(pid);
        if code != 0 {
            note(format!("worker {pid} exited {code}"));
            ok = false;
        }
    }
    note(format!(
        "{} forks across {FORK_WORKERS} workers",
        FORK_WORKERS * FORKS_PER_WORKER
    ));
    ok
}

fn test_spawned_children_that_fork_end_under_load() -> bool {
    let ok = spawn_storm("child", SPAWNS_PER_THREAD / 2);
    note(format!(
        "{} spawned children, each forking once",
        THREADS * (SPAWNS_PER_THREAD / 2)
    ));
    ok
}

/// Children whose threads keep entering `futex_wait` while the main thread
/// ends the process, each holding the pipe the parent reads to its end.
const FUTEX_EXITS: usize = 60;

fn futex(word: &AtomicU32, op: u64, val: u64) {
    // SAFETY: `word` is a live, aligned `u32`; no timeout pointer is passed.
    unsafe {
        slopos_slibc::pal::raw::syscall4(
            SYSCALL_FUTEX,
            word.as_ptr() as u64,
            op | FUTEX_PRIVATE_FLAG,
            val,
            0,
        )
    };
}

/// Two threads wait on a word a third wakes without pause, so one of them is
/// usually between entering `futex_wait` and parking when `exit` kills it.
fn futex_mode(delay_us: u32) -> i32 {
    static WORD: AtomicU32 = AtomicU32::new(0);
    for _ in 0..2 {
        std::thread::spawn(|| {
            loop {
                futex(&WORD, FUTEX_WAIT, 0);
            }
        });
    }
    std::thread::spawn(|| {
        loop {
            futex(&WORD, FUTEX_WAKE, u64::from(u32::MAX));
        }
    });
    // SAFETY: `usleep` touches no caller memory.
    unsafe { slopos_slibc::time::usleep(delay_us) };
    0
}

/// A kill that lands while a thread is on its way into `futex_wait` must still
/// end it: a straggler keeps the process's descriptors open after the parent
/// has reaped it, and whoever reads its pipe waits for an end that never comes.
fn test_exit_ends_threads_entering_futex_wait() -> bool {
    for round in 0..FUTEX_EXITS {
        let mut child = match Command::new(SELF_PATH)
            .arg("futex")
            .arg(format!("{}", 200 + (round * 137) % 1800))
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                note(format!("spawn {round} failed: {e}"));
                return false;
            }
        };
        let Some(mut stdout) = child.stdout.take() else {
            note(format!("spawn {round} gave no pipe"));
            return false;
        };
        if child.wait().is_err() {
            note(format!("wait {round} failed"));
            return false;
        }
        let mut pfd = slopos_slibc::io::poll::Pollfd {
            fd: stdout.as_raw_fd(),
            events: slopos_slibc::io::poll::POLLIN,
            revents: 0,
        };
        // SAFETY: one live `Pollfd`.
        let ready = unsafe { slopos_slibc::io::poll::poll(&mut pfd, 1, 5000) };
        let mut rest = [0u8; 16];
        if ready != 1 || !matches!(stdout.read(&mut rest), Ok(0)) {
            note(format!(
                "round {round}: a thread outlived its reaped process and holds its pipe"
            ));
            return false;
        }
    }
    note(format!(
        "{FUTEX_EXITS} exits with threads entering futex_wait"
    ));
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "spawned_children_end_under_load",
        test_spawned_children_end_under_load,
    ),
    (
        "forked_children_end_under_load",
        test_forked_children_end_under_load,
    ),
    (
        "spawned_children_that_fork_end_under_load",
        test_spawned_children_that_fork_end_under_load,
    ),
    (
        "exit_ends_threads_entering_futex_wait",
        test_exit_ends_threads_entering_futex_wait,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("noop") => 0,
        Some("child") => child_mode(),
        Some("futex") => futex_mode(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(500)),
        _ => slopos_slibc::test_harness::run_with_progress("exit_stress", CASES),
    };
    std::process::exit(code);
}
