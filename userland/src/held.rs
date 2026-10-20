//! Handing this process's whole mask to a program. Exec keeps only the
//! universal capabilities, so a process that hands its mask on spawns the
//! program instead and ends as it ends: the rescue shell and a shell `priv`
//! runs, for each program they run, and `priv` for its command.

use core::ffi::c_void;
use core::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::Errno;
use slopos_abi::signal::{SA_SIGINFO, UserSiginfo};
use slopos_abi::spawn::SPAWN_MAX_FD_ACTIONS;
use slopos_abi::syscall::{F_GETFD, FD_CLOEXEC};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::wire::RELAYED_SIGNALS;
use slopos_slibc::pal::{Pal, Sys};
use slopos_slibc::signal;
use slopos_slibc::types::{sigaction as SigAction, sigset_t as SigSet};

use crate::syscall::core as sys_core;
use crate::syscall::process::{self, Endowment, WaitStatus};

/// Marks every shell a rescue shell starts, which hands on what it holds.
pub const RESCUE_ENV: &str = "SLOPOS_RESCUE";
/// Marks what `priv` runs: a shell among it hands on what it holds.
pub const PRIV_ENV: &str = "SLOPOS_PRIV";

/// The width of the kernel's descriptor table.
const DESCRIPTORS: i32 = 256;

/// The program a process running one waits for, once it runs.
static PROGRAM: AtomicU32 = AtomicU32::new(0);

/// Pass on a signal another process sent; one the kernel raised, as an
/// interrupt typed at the terminal or a hangup, reached the program's group
/// already.
extern "C" fn forward(signal: i32, info: *mut UserSiginfo, _: *mut c_void) {
    let program = PROGRAM.load(Ordering::Relaxed);
    // SAFETY: the kernel hands an `SA_SIGINFO` handler a live siginfo.
    if program != 0 && (info.is_null() || unsafe { (*info).si_code } <= 0) {
        let _ = process::kill(program, signal as u8);
    }
}

/// Run the program at `path` holding this process's whole mask, with the
/// descriptors an exec would keep and in this process's group, so what the
/// terminal raises for the job reaches it, then end as it ended. A signal
/// another process sends this one, privd's relays among them, is passed on,
/// so one sent to both reaches it twice. Returns only the errno of a program
/// that could not start.
pub fn run(path: &[u8], argv: &[*const u8], envp: &[*const u8]) -> i64 {
    let kept: Vec<_> = (0..DESCRIPTORS)
        .filter(|&fd| {
            Sys::fcntl(fd, F_GETFD as i32, 0).is_ok_and(|flags| flags as u64 & FD_CLOEXEC == 0)
        })
        .map(|fd| process::clone_fd(fd, fd))
        .collect();
    if kept.len() > SPAWN_MAX_FD_ACTIONS {
        return i64::from(Errno::EMFILE.raw());
    }
    let passed_on = SigAction {
        sa_sigaction: forward as *const () as usize,
        sa_mask: SigSet::empty(),
        sa_flags: SA_SIGINFO as i32,
        sa_restorer: None,
    };
    let mut held = SigSet::empty();
    let mut was = SigSet::empty();
    // SAFETY: the action and both sets are live, `forward` takes the
    // `SA_SIGINFO` arguments, and held signals wait for `PROGRAM`.
    unsafe {
        for passed in RELAYED_SIGNALS {
            signal::sigaddset(&mut held, passed.into());
        }
        signal::sigprocmask(signal::SIG_BLOCK, &held, &mut was);
        for passed in RELAYED_SIGNALS {
            signal::sigaction(passed.into(), &passed_on, ptr::null_mut());
        }
    }
    let pid = process::spawn_endowed(
        path,
        argv,
        envp,
        b"",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &kept,
        0,
        Endowment {
            caps: sys_core::caps_drop(0),
            ..Endowment::NONE
        },
    );
    if pid > 0 {
        PROGRAM.store(pid as u32, Ordering::Relaxed);
    }
    // SAFETY: `was` is the mask this process had.
    unsafe { signal::sigprocmask(signal::SIG_SETMASK, &was, ptr::null_mut()) };
    if pid <= 0 {
        return i64::from(pid);
    }
    let ended = loop {
        let mut status = 0;
        match process::waitpid_raw(pid, &mut status, 0) {
            rc if rc > 0 => break process::wait_status(status),
            rc if rc == i64::from(Errno::EINTR.raw()) => {}
            _ => sys_core::exit_with_code(1),
        }
    };
    if let WaitStatus::Signalled(signal) = ended {
        let _ = process::default_signal(signal);
        let _ = process::kill(process::getpid(), signal);
    }
    sys_core::exit_with_code(ended.exit_code().unwrap_or(1))
}
