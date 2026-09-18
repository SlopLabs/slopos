//! Signal-on-IRQ-exit end-to-end test.
//!
//! The child spins in userspace issuing no syscalls, so only a timer IRQ
//! can pull it out of user mode; the parent's SIGINT must still terminate
//! it and `waitpid` report it as a SIGINT death.

use slopos_abi::signal::SIGINT;
use slopos_userland as _;
use slopos_userland::syscall::process::WaitStatus;
use slopos_userland::syscall::{core as sys_core, process};

/// Returns the child task id in the parent; never returns in the child.
fn fork_spinning_child() -> i32 {
    let pid = process::fork();
    if pid == 0 {
        loop {
            core::hint::spin_loop();
        }
    }
    pid
}

fn test_spin_child_killed_by_sigint() -> bool {
    let pid = fork_spinning_child();
    if pid <= 0 {
        eprintln!("spin_signal_test: fork failed (pid={pid})");
        return false;
    }

    // Let the child reach its spin loop first; a kill landing mid-spawn is
    // still correct, the pending bit is acted on at the first IRQ exit.
    sys_core::yield_now();

    let rc = process::kill_pid(pid, SIGINT);
    if rc != 0 {
        eprintln!("spin_signal_test: kill(SIGINT) failed (rc={rc})");
        let _ = process::waitpid(pid as u32);
        return false;
    }

    let Some((reaped, status)) = process::waitpid(pid as u32) else {
        eprintln!("spin_signal_test: the child could not be reaped");
        return false;
    };
    if reaped != pid as u32 {
        eprintln!("spin_signal_test: waitpid reaped {reaped}, expected {pid}");
        return false;
    }
    let report = process::wait_status(status);
    if report != WaitStatus::Signalled(SIGINT) {
        eprintln!("spin_signal_test: child reported {report:?}, expected a SIGINT death");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[(
    "spin_child_killed_by_sigint",
    test_spin_child_killed_by_sigint,
)];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
