//! Key handling, line buffer, and cursor management.

use core::cmp;
use core::ffi::c_void;
use core::ptr;

use crate::runtime;
use crate::syscall::core as sys_core;
use crate::syscall::tty;

use super::buffers;
use super::display::{
    DISPLAY, shell_console_follow_bottom, shell_console_page_down, shell_console_page_up,
    shell_redraw_input,
};
use super::parser::{SHELL_MAX_TOKENS, shell_parse_line};

const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;

/// Read one command line from the user, returning the number of tokens parsed.
/// Tokens are written into the provided `tokens` slice.
pub fn read_command_line(tokens: &mut [*const u8; SHELL_MAX_TOKENS]) -> i32 {
    let (_, line_row) = super::display::shell_console_get_cursor();

    // Clear line buffer
    buffers::with_line_buf(|buf| {
        runtime::u_memset(buf.as_mut_ptr() as *mut c_void, 0, buf.len());
    });

    let mut len = 0usize;

    loop {
        let rc = tty::read_char();
        if rc < 0 {
            // No character available - yield to let other tasks run
            sys_core::yield_now();
            continue;
        }
        let c = rc as u8;

        if c == KEY_PAGE_UP {
            shell_console_page_up();
            continue;
        }
        if c == KEY_PAGE_DOWN {
            shell_console_page_down();
            continue;
        }

        // Return to follow mode if we were scrolled up
        if DISPLAY.enabled.get() && !DISPLAY.follow.get() {
            shell_console_follow_bottom();
        }

        if c == b'\n' || c == b'\r' {
            super::display::shell_echo_char(b'\n');
            break;
        }

        if c == b'\x08' || c == 0x7f {
            if len > 0 {
                len -= 1;
                buffers::with_line_buf(|buf| {
                    shell_redraw_input(line_row, &buf[..len]);
                });
            }
            continue;
        }

        if c < 0x20 {
            continue;
        }

        let max_len = buffers::with_line_buf(|buf| buf.len());
        if len + 1 >= max_len {
            continue;
        }

        buffers::with_line_buf(|buf| {
            buf[len] = c;
        });
        len += 1;

        buffers::with_line_buf(|buf| {
            shell_redraw_input(line_row, &buf[..len]);
        });
    }

    // Null-terminate
    buffers::with_line_buf(|buf| {
        let capped = cmp::min(len, buf.len() - 1);
        buf[capped] = 0;
    });

    // Parse
    *tokens = [ptr::null(); SHELL_MAX_TOKENS];
    buffers::with_line_buf(|buf| shell_parse_line(buf, tokens))
}
