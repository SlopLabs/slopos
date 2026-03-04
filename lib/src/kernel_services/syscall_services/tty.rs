crate::define_service! {
    tty => TtyServices {
        read_cooked(tty_index: slopos_abi::syscall::TtyIndex, buf: *mut u8, max: usize, nonblock: bool) -> isize;
        has_cooked_data(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        set_termios(tty_index: slopos_abi::syscall::TtyIndex, t: *const slopos_abi::syscall::UserTermios);
        get_termios(tty_index: slopos_abi::syscall::TtyIndex, t: *mut slopos_abi::syscall::UserTermios);
        get_winsize(tty_index: slopos_abi::syscall::TtyIndex, ws: *mut slopos_abi::syscall::UserWinsize);
        set_winsize(tty_index: slopos_abi::syscall::TtyIndex, ws: *const slopos_abi::syscall::UserWinsize);
        set_compositor_focus(target: u32) -> i32;
        get_compositor_focus() -> u32;
        set_foreground_pgrp(tty_index: slopos_abi::syscall::TtyIndex, pgid: u32) -> i32;
        get_foreground_pgrp(tty_index: slopos_abi::syscall::TtyIndex) -> u32;
        get_session_id(tty_index: slopos_abi::syscall::TtyIndex) -> u32;
        set_foreground_pgrp_checked(tty_index: slopos_abi::syscall::TtyIndex, pgid: u32, caller_sid: u32) -> i32;
        write_bytes(tty_index: slopos_abi::syscall::TtyIndex, buf: *const u8, len: usize) -> usize;
        detach_session_by_id(session_id: u32);
    }
}
