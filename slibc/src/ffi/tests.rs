use super::shim;
use crate::ffi::syscalls::{self, SloposStat};

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

    // `std`'s copy in `slibc/std_pal/fs/slopos.rs` repeats these numbers, and
    // cargo's fingerprinting rests on `st_mtim` landing where the kernel writes it.
    check!(
        "SloposStat_size_144",
        core::mem::size_of::<SloposStat>() == 144
    );
    check!(
        "SloposStat_st_mode_offset",
        core::mem::offset_of!(SloposStat, st_mode) == 24
    );
    check!(
        "SloposStat_st_size_offset",
        core::mem::offset_of!(SloposStat, st_size) == 48
    );
    check!(
        "SloposStat_st_atim_offset",
        core::mem::offset_of!(SloposStat, st_atim) == 72
    );
    check!(
        "SloposStat_st_mtim_offset",
        core::mem::offset_of!(SloposStat, st_mtim) == 88
    );
    check!(
        "SloposStat_st_ctim_offset",
        core::mem::offset_of!(SloposStat, st_ctim) == 104
    );

    check!("slopos_yield_no_crash", {
        syscalls::slopos_yield();
        true
    });

    check!("slopos_yield_no_crash", {
        syscalls::slopos_yield();
        true
    });

    check!("slopos_clock_gettime_returns_time", {
        let mut sec: i64 = 0;
        let mut nsec: i64 = 0;
        shim::slopos_clock_gettime(1, &mut sec, &mut nsec) == 0
    });

    check!("slopos_stat_invalid_path", {
        let path = b"/nonexistent_path_12345\0";
        let mut stat = SloposStat::default();
        shim::slopos_stat(path, &mut stat) < 0
    });

    check!("slopos_lseek_invalid_fd", shim::slopos_lseek(-1, 0, 0) < 0);

    check!("slopos_futex_wake_no_waiters", {
        let val: u32 = 0;
        shim::slopos_futex_wake(&val, 1) >= 0
    });

    check!("slopos_pipe_creates_fds", {
        let mut fds = [0i32; 2];
        let ret = shim::slopos_pipe(&mut fds);
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
