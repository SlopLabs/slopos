use core::cmp;
use core::ffi::c_void;
use core::ptr;

use crate::runtime;
use crate::syscall::core as sys_core;
use crate::syscall::tty;

use super::buffers;
use super::completion;
use super::display::{
    DISPLAY, shell_console_clear, shell_console_follow_bottom, shell_console_page_down,
    shell_console_page_up, shell_redraw_input, shell_write,
};
use super::history;
use super::parser::{SHELL_MAX_TOKENS, shell_parse_line};

const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;
const KEY_UP: u8 = 0x82;
const KEY_DOWN: u8 = 0x83;
const KEY_LEFT: u8 = 0x84;
const KEY_RIGHT: u8 = 0x85;
const KEY_HOME: u8 = 0x86;
const KEY_END: u8 = 0x87;
const KEY_DELETE: u8 = 0x88;

const CTRL_A: u8 = 0x01;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const CTRL_E: u8 = 0x05;
const CTRL_K: u8 = 0x0B;
const CTRL_L: u8 = 0x0C;
const CTRL_U: u8 = 0x15;
const CTRL_W: u8 = 0x17;

pub fn read_command_line(tokens: &mut [*const u8; SHELL_MAX_TOKENS], prompt: &[u8]) -> i32 {
    buffers::with_line_buf(|buf| {
        runtime::u_memset(buf.as_mut_ptr() as *mut c_void, 0, buf.len());
    });
    input_loop(tokens, prompt, 0, 0)
}

fn input_loop(
    tokens: &mut [*const u8; SHELL_MAX_TOKENS],
    prompt: &[u8],
    mut len: usize,
    mut cursor_pos: usize,
) -> i32 {
    let (_, line_row) = super::display::shell_console_get_cursor();
    redraw(line_row, prompt, len, cursor_pos);

    loop {
        let rc = tty::read_char();
        if rc < 0 {
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

        if DISPLAY.enabled.get() && !DISPLAY.follow.get() {
            shell_console_follow_bottom();
        }

        match c {
            b'\n' | b'\r' => {
                super::display::shell_echo_char(b'\n');
                buffers::with_line_buf(|buf| {
                    history::push(buf, len);
                });
                history::reset_cursor();
                break;
            }

            b'\x08' | 0x7f => {
                if cursor_pos > 0 {
                    delete_char_before_cursor(&mut len, &mut cursor_pos);
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            KEY_DELETE => {
                if cursor_pos < len {
                    delete_char_at_cursor(&mut len, cursor_pos);
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            KEY_UP => {
                let mut snapshot = [0u8; 256];
                buffers::with_line_buf(|buf| {
                    snapshot[..len].copy_from_slice(&buf[..len]);
                });
                let new_len =
                    buffers::with_line_buf(|buf| history::navigate_up(&snapshot[..len], len, buf));
                if let Some(nl) = new_len {
                    len = nl;
                    cursor_pos = nl;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            KEY_DOWN => {
                let new_len = buffers::with_line_buf(|buf| history::navigate_down(buf));
                if let Some(nl) = new_len {
                    len = nl;
                    cursor_pos = nl;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            KEY_LEFT => {
                if cursor_pos > 0 {
                    cursor_pos -= 1;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            KEY_RIGHT => {
                if cursor_pos < len {
                    cursor_pos += 1;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            KEY_HOME | CTRL_A => {
                if cursor_pos != 0 {
                    cursor_pos = 0;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            KEY_END | CTRL_E => {
                if cursor_pos != len {
                    cursor_pos = len;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            CTRL_K => {
                if cursor_pos < len {
                    buffers::with_line_buf(|buf| {
                        for i in cursor_pos..len {
                            buf[i] = 0;
                        }
                    });
                    len = cursor_pos;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            CTRL_U => {
                if cursor_pos > 0 {
                    let shift = len - cursor_pos;
                    buffers::with_line_buf(|buf| {
                        for i in 0..shift {
                            buf[i] = buf[cursor_pos + i];
                        }
                        for i in shift..len {
                            buf[i] = 0;
                        }
                    });
                    len = shift;
                    cursor_pos = 0;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            CTRL_W => {
                if cursor_pos > 0 {
                    let old_cursor = cursor_pos;
                    let mut new_cursor = cursor_pos;
                    buffers::with_line_buf(|buf| {
                        while new_cursor > 0 && buf[new_cursor - 1] == b' ' {
                            new_cursor -= 1;
                        }
                        while new_cursor > 0 && buf[new_cursor - 1] != b' ' {
                            new_cursor -= 1;
                        }
                        let tail = len - old_cursor;
                        for i in 0..tail {
                            buf[new_cursor + i] = buf[old_cursor + i];
                        }
                        for i in new_cursor + tail..len {
                            buf[i] = 0;
                        }
                    });
                    len -= old_cursor - new_cursor;
                    cursor_pos = new_cursor;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            CTRL_L => {
                shell_write(b"\x1B[2J\x1B[H");
                shell_console_clear();
                shell_write(prompt);
                return input_loop(tokens, prompt, len, cursor_pos);
            }

            CTRL_C => {
                shell_write(b"^C\n");
                history::reset_cursor();
                return 0;
            }

            CTRL_D => {
                if len == 0 {
                    return -1;
                }
                if cursor_pos < len {
                    delete_char_at_cursor(&mut len, cursor_pos);
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            0x09 => {
                let cwd = super::cwd_bytes();
                let comp = buffers::with_line_buf(|buf| {
                    completion::try_complete(buf, len, cursor_pos, &cwd)
                });

                if comp.show_matches {
                    shell_write(b"\n");
                    shell_write(&comp.matches_buf[..comp.matches_len]);
                    shell_write(b"\n");
                    shell_write(prompt);

                    if comp.insertion_len > 0 {
                        insert_text(
                            &comp.insertion,
                            comp.insertion_len,
                            &mut len,
                            &mut cursor_pos,
                        );
                    }

                    return input_loop(tokens, prompt, len, cursor_pos);
                } else if comp.insertion_len > 0 {
                    insert_text(
                        &comp.insertion,
                        comp.insertion_len,
                        &mut len,
                        &mut cursor_pos,
                    );
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            0x20..=0x7E => {
                let max_len = buffers::with_line_buf(|buf| buf.len());
                if len + 1 < max_len {
                    buffers::with_line_buf(|buf| {
                        let mut i = len;
                        while i > cursor_pos {
                            buf[i] = buf[i - 1];
                            i -= 1;
                        }
                        buf[cursor_pos] = c;
                    });
                    len += 1;
                    cursor_pos += 1;
                    redraw(line_row, prompt, len, cursor_pos);
                }
            }

            _ => {}
        }
    }

    buffers::with_line_buf(|buf| {
        let capped = cmp::min(len, buf.len() - 1);
        buf[capped] = 0;
    });

    *tokens = [ptr::null(); SHELL_MAX_TOKENS];
    buffers::with_line_buf(|buf| shell_parse_line(buf, tokens))
}

fn delete_char_before_cursor(len: &mut usize, cursor_pos: &mut usize) {
    buffers::with_line_buf(|buf| {
        for i in *cursor_pos - 1..len.saturating_sub(1) {
            buf[i] = buf[i + 1];
        }
        if *len > 0 {
            buf[*len - 1] = 0;
        }
    });
    *len = len.saturating_sub(1);
    *cursor_pos -= 1;
}

fn delete_char_at_cursor(len: &mut usize, cursor_pos: usize) {
    buffers::with_line_buf(|buf| {
        for i in cursor_pos..len.saturating_sub(1) {
            buf[i] = buf[i + 1];
        }
        if *len > 0 {
            buf[*len - 1] = 0;
        }
    });
    *len = len.saturating_sub(1);
}

fn insert_text(text: &[u8], text_len: usize, len: &mut usize, cursor_pos: &mut usize) {
    let max_len = buffers::with_line_buf(|buf| buf.len());
    let available = max_len.saturating_sub(*len + 1);
    let insert_len = text_len.min(available);
    if insert_len == 0 {
        return;
    }

    buffers::with_line_buf(|buf| {
        let mut i = *len;
        while i > *cursor_pos {
            if i - 1 + insert_len < max_len {
                buf[i - 1 + insert_len] = buf[i - 1];
            }
            i -= 1;
        }
        for i in 0..insert_len {
            buf[*cursor_pos + i] = text[i];
        }
    });
    *len += insert_len;
    *cursor_pos += insert_len;
}

fn redraw(line_row: i32, prompt: &[u8], len: usize, cursor_pos: usize) {
    buffers::with_line_buf(|buf| {
        shell_redraw_input(line_row, prompt, &buf[..len], cursor_pos);
    });
}
