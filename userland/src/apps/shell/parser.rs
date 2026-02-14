//! Command line parsing and path normalization.

use core::cmp;
use core::ptr;

use crate::runtime;

use super::buffers;

pub const SHELL_MAX_TOKENS: usize = 16;
pub const SHELL_MAX_TOKEN_LENGTH: usize = 64;

#[inline(always)]
pub fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

pub fn u_streq_slice(a: *const u8, b: &[u8]) -> bool {
    if a.is_null() {
        return b.is_empty();
    }
    let len = runtime::u_strlen(a);
    if len != b.len() {
        return false;
    }
    for i in 0..len {
        unsafe {
            if *a.add(i) != b[i] {
                return false;
            }
        }
    }
    true
}

pub fn normalize_path(input: *const u8, buffer: &mut [u8]) -> i32 {
    let cwd = super::cwd_bytes();
    normalize_path_with_cwd(input, buffer, &cwd)
}

fn collapse_absolute_path(buffer: &mut [u8], len: usize) -> usize {
    if buffer.is_empty() {
        return 0;
    }
    if len == 0 || buffer[0] != b'/' {
        buffer[0] = b'/';
        return 1;
    }

    let mut write = 1usize;
    let mut read = 1usize;

    while read < len {
        while read < len && buffer[read] == b'/' {
            read += 1;
        }
        if read >= len {
            break;
        }

        let seg_start = read;
        while read < len && buffer[read] != b'/' {
            read += 1;
        }
        let seg_len = read - seg_start;

        if seg_len == 1 && buffer[seg_start] == b'.' {
            continue;
        }
        if seg_len == 2 && buffer[seg_start] == b'.' && buffer[seg_start + 1] == b'.' {
            if write > 1 {
                write -= 1;
                while write > 0 && buffer[write] != b'/' {
                    write -= 1;
                }
                if write == 0 {
                    write = 1;
                }
            }
            continue;
        }

        if write > 1 {
            buffer[write] = b'/';
            write += 1;
        }
        for j in 0..seg_len {
            buffer[write + j] = buffer[seg_start + j];
        }
        write += seg_len;
    }

    if write == 0 { 1 } else { write }
}

pub fn normalize_path_with_cwd(input: *const u8, buffer: &mut [u8], cwd: &[u8]) -> i32 {
    if buffer.is_empty() {
        return -1;
    }
    if input.is_null() || unsafe { *input } == 0 {
        buffer[0] = b'/';
        if buffer.len() > 1 {
            buffer[1] = 0;
        }
        return 0;
    }

    unsafe {
        if *input == b'/' {
            let len = runtime::u_strnlen(input, buffer.len().saturating_sub(1));
            if len >= buffer.len() {
                return -1;
            }
            ptr::copy_nonoverlapping(input, buffer.as_mut_ptr(), len);
            let collapsed_len = collapse_absolute_path(buffer, len);
            buffer[collapsed_len] = 0;
            return 0;
        }
    }

    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(cwd.len());
    let input_len = runtime::u_strnlen(input, buffer.len());

    let needs_sep = cwd_len > 0 && cwd[cwd_len - 1] != b'/';
    let sep_len = if needs_sep { 1 } else { 0 };
    let total = cwd_len + sep_len + input_len;

    if total >= buffer.len() {
        return -1;
    }

    buffer[..cwd_len].copy_from_slice(&cwd[..cwd_len]);
    if needs_sep {
        buffer[cwd_len] = b'/';
    }
    unsafe {
        ptr::copy_nonoverlapping(input, buffer.as_mut_ptr().add(cwd_len + sep_len), input_len);
    }
    let collapsed_len = collapse_absolute_path(buffer, total);
    buffer[collapsed_len] = 0;
    0
}

pub fn shell_parse_line(line: &[u8], tokens: &mut [*const u8]) -> i32 {
    if line.is_empty() || tokens.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut cursor = 0usize;

    while cursor < line.len() {
        while cursor < line.len() && is_space(line[cursor]) {
            cursor += 1;
        }
        if cursor >= line.len() || line[cursor] == 0 {
            break;
        }

        let start = cursor;
        let token_len;

        if line[cursor] == b'|' || line[cursor] == b'<' || line[cursor] == b'&' {
            token_len = 1;
            cursor += 1;
        } else if line[cursor] == b'>' {
            cursor += 1;
            token_len = if cursor < line.len() && line[cursor] == b'>' {
                cursor += 1;
                2
            } else {
                1
            };
        } else {
            while cursor < line.len()
                && line[cursor] != 0
                && !is_space(line[cursor])
                && line[cursor] != b'|'
                && line[cursor] != b'<'
                && line[cursor] != b'>'
                && line[cursor] != b'&'
            {
                cursor += 1;
            }
            token_len = cursor - start;
        }

        if count >= tokens.len() {
            continue;
        }
        let token_len = cmp::min(token_len, SHELL_MAX_TOKEN_LENGTH - 1);

        buffers::with_token_storage(|storage| {
            storage[count][..token_len].copy_from_slice(&line[start..start + token_len]);
            storage[count][token_len] = 0;
        });
        tokens[count] = buffers::token_ptr(count);
        count += 1;
    }

    if count < tokens.len() {
        tokens[count] = ptr::null();
    }
    count as i32
}
