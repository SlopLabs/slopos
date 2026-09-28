//! signalfd end-to-end test: with SIGCHLD blocked, a child's exit arrives as an
//! in-band `POLLIN` on the signalfd, so a single `poll(2)` — no EINTR-retry
//! loop — observes it.

use slopos_abi::signal::{CLD_EXITED, SIGCHLD, SignalfdSiginfo, sig_bit};
use slopos_abi::syscall::POLLIN;
use slopos_userland as _;
use slopos_userland::syscall::{UserPollFd, core as sys_core, fs, process, signalfd};

fn test_sigchld_inband() -> bool {
    let mask = sig_bit(SIGCHLD);

    let _ = signalfd::block_signals(mask);

    let sfd = signalfd::signalfd(mask, 0);
    if sfd < 0 {
        eprintln!("signalfd_test: signalfd create failed ({sfd})");
        return false;
    }

    let pid = process::fork();
    if pid == 0 {
        sys_core::exit_with_code(CHILD_EXIT_CODE);
    }
    if pid < 0 {
        let _ = slopos_slibc::ffi::close(sfd);
        eprintln!("signalfd_test: fork failed ({pid})");
        return false;
    }
    let child = pid as u32;

    let mut pfds = [UserPollFd {
        fd: sfd,
        events: POLLIN,
        revents: 0,
    }];
    let ready =
        matches!(fs::poll(&mut pfds, 5000), Ok(n) if n >= 1) && (pfds[0].revents & POLLIN) != 0;

    // Linux's `signalfd_siginfo` offsets, little-endian.
    let mut buf = [0u8; SignalfdSiginfo::SERIALIZED_LEN];
    let full = matches!(fs::read_slice(sfd, &mut buf), Ok(n) if n == buf.len());
    let word = |at: usize| u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    let signo_ok = full && word(0) == SIGCHLD as u32;
    let info_ok = full
        && word(8) as i32 == CLD_EXITED
        && word(12) == child
        && word(40) as i32 == CHILD_EXIT_CODE;

    let _ = process::waitpid(child);
    let _ = slopos_slibc::ffi::close(sfd);

    if !ready {
        eprintln!("signalfd_test: poll did not report POLLIN (footgun not fixed?)");
        return false;
    }
    if !signo_ok {
        eprintln!("signalfd_test: drained siginfo ssi_signo != SIGCHLD");
        return false;
    }
    if !info_ok {
        eprintln!(
            "signalfd_test: SIGCHLD record code={} pid={} status={}, want {CLD_EXITED}/{child}/{CHILD_EXIT_CODE}",
            word(8) as i32,
            word(12),
            word(40) as i32
        );
        return false;
    }
    true
}

const CHILD_EXIT_CODE: i32 = 7;

const CASES: &[(&str, fn() -> bool)] = &[("sigchld_inband", test_sigchld_inband)];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
