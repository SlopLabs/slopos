crate::define_service! {
    tty => TtyServices {
        read_cooked(tty_index: u8, buf: *mut u8, max: usize, nonblock: bool) -> isize;
        has_cooked_data(tty_index: u8) -> bool;
        set_termios(tty_index: u8, t: *const slopos_abi::syscall::UserTermios);
        get_termios(tty_index: u8, t: *mut slopos_abi::syscall::UserTermios);
        get_winsize(tty_index: u8, ws: *mut slopos_abi::syscall::UserWinsize);
        set_winsize(tty_index: u8, ws: *const slopos_abi::syscall::UserWinsize);
        set_focus(target: u32) -> i32;
        get_focus() -> u32;
        set_foreground_pgrp(tty_index: u8, pgid: u32) -> i32;
        get_foreground_pgrp(tty_index: u8) -> u32;
        get_session_id(tty_index: u8) -> u32;
        set_foreground_pgrp_checked(tty_index: u8, pgid: u32, caller_sid: u32) -> i32;
        write_bytes(tty_index: u8, buf: *const u8, len: usize) -> usize;
        detach_session_by_id(session_id: u32);
    }
}
