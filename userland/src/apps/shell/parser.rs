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
            buffer[len] = 0;
            return 0;
        }
    }

    let maxlen = buffer.len().saturating_sub(2);
    let len = runtime::u_strnlen(input, maxlen);
    if len > maxlen {
        return -1;
    }
    buffer[0] = b'/';
    unsafe {
        ptr::copy_nonoverlapping(input, buffer.as_mut_ptr().add(1), len);
    }
    let term_idx = cmp::min(len + 1, buffer.len() - 1);
    buffer[term_idx] = 0;
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
        while cursor < line.len() && line[cursor] != 0 && !is_space(line[cursor]) {
            cursor += 1;
        }
        if count >= tokens.len() {
            continue;
        }
        let token_len = cmp::min(cursor - start, SHELL_MAX_TOKEN_LENGTH - 1);

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
