//! Process exit under load: every road that tears an address space down —
//! a spawned child ending, a forked child ending, a child that forks and
//! ends — hundreds of times over, on every CPU at once.

use std::process::Command;

use slopos_abi::syscall::posix::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
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
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("noop") => 0,
        Some("child") => child_mode(),
        _ => slopos_slibc::test_harness::run_with_progress("exit_stress", CASES),
    };
    std::process::exit(code);
}
