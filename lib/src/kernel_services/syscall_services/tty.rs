crate::define_service! {
    tty => TtyServices {
        read_line(buf: *mut u8, len: usize) -> usize;
        read_char_blocking(buf: *mut u8) -> i32;
        read_char_nonblocking(buf: *mut u8) -> i32;
        set_focus(target: u32) -> i32;
        get_focus() -> u32;
        set_foreground_pgrp(pgid: u32) -> i32;
        get_foreground_pgrp() -> u32;
    }
}
