use super::shim;
use crate::ffi::syscalls::SloposStat;

pub fn run_ffi_syscall_tests() -> (u32, u32) {
    let mut pass = 0u32;
    let mut fail = 0u32;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                pass += 1;
            } else {
                fail += 1;
            }
        };
    }

    check!("clock_gettime_returns_time", {
        let mut ts = crate::time::Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        shim::clock_gettime(crate::time::CLOCK_MONOTONIC, &mut ts) == 0
    });

    // The C convention, not a negated errno: a failing metadata call answers
    // exactly -1 and leaves the reason in `errno`.
    check!("stat_invalid_path_returns_minus_one", {
        let path = b"/nonexistent_path_12345\0";
        let mut stat = SloposStat::default();
        crate::errno::errno_set(0);
        shim::stat(path, &mut stat) == -1 && crate::errno::errno_get() != 0
    });

    check!("lseek_invalid_fd_returns_minus_one", {
        crate::errno::errno_set(0);
        shim::lseek(-1, 0, 0) == -1 && crate::errno::errno_get() != 0
    });

    check!("futex_wake_no_waiters", {
        let val: u32 = 0;
        shim::slopos_futex_wake(&val, 1) >= 0
    });

    check!("pipe_creates_fds", {
        let mut fds = [0i32; 2];
        let ret = shim::pipe(&mut fds);
        if ret == 0 {
            let valid = fds[0] > 0 && fds[1] > 0 && fds[0] != fds[1];
            shim::close(fds[0]);
            shim::close(fds[1]);
            valid
        } else {
            false
        }
    });

    (pass, fail)
}
