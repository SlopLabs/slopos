crate::define_service! {
    tty => TtyServices {
        read_cooked(buf: *mut u8, max: usize, nonblock: bool) -> isize;
        has_cooked_data() -> bool;
        set_termios(t: *const slopos_abi::syscall::UserTermios);
        get_termios(t: *mut slopos_abi::syscall::UserTermios);
        set_focus(target: u32) -> i32;
        get_focus() -> u32;
        set_foreground_pgrp(pgid: u32) -> i32;
        get_foreground_pgrp() -> u32;
    }
}
