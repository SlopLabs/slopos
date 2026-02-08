slopos_lib::define_service! {
    tty => TtyServices {
        read_line(buf: *mut u8, len: usize) -> usize;
        read_char_blocking(buf: *mut u8) -> i32;
        set_focus(target: u32) -> i32;
        get_focus() -> u32;
    }
}
