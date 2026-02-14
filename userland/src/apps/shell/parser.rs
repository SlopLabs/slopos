//! Command line parsing and path normalization.

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

fn is_var_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn write_u32_to_buf(value: u32, dst: &mut [u8]) -> usize {
    if value == 0 {
        if !dst.is_empty() {
            dst[0] = b'0';
        }
        return 1;
    }
    let mut rev = [0u8; 12];
    let mut r = 0usize;
    let mut n = value;
    while n != 0 && r < rev.len() {
        rev[r] = b'0' + (n % 10) as u8;
        n /= 10;
        r += 1;
    }
    let len = r.min(dst.len());
    for i in 0..len {
        dst[i] = rev[r - 1 - i];
    }
    len
}

fn write_i32_to_buf(value: i32, dst: &mut [u8]) -> usize {
    if value < 0 {
        if dst.is_empty() {
            return 0;
        }
        dst[0] = b'-';
        1 + write_u32_to_buf(value.unsigned_abs(), &mut dst[1..])
    } else {
        write_u32_to_buf(value as u32, dst)
    }
}

fn emit(dst: &mut [u8], pos: &mut usize, b: u8) {
    if *pos < dst.len() - 1 {
        dst[*pos] = b;
        *pos += 1;
    }
}

fn emit_slice(dst: &mut [u8], pos: &mut usize, src: &[u8], src_len: usize) {
    let avail = dst.len().saturating_sub(*pos + 1);
    let n = src_len.min(avail);
    dst[*pos..*pos + n].copy_from_slice(&src[..n]);
    *pos += n;
}

pub fn expand_variables(input: &[u8], input_len: usize, output: &mut [u8]) -> usize {
    let mut out = 0usize;
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < input_len && input[i] != 0 {
        let c = input[i];

        if c == b'\'' && !in_double {
            in_single = !in_single;
            emit(output, &mut out, c);
            i += 1;
            continue;
        }
        if c == b'"' && !in_single {
            in_double = !in_double;
            emit(output, &mut out, c);
            i += 1;
            continue;
        }

        if in_single {
            emit(output, &mut out, c);
            i += 1;
            continue;
        }

        if c == b'\\' && i + 1 < input_len {
            let next = input[i + 1];
            if next == b'$' {
                emit(output, &mut out, b'$');
                i += 2;
                continue;
            }
            if in_double && (next == b'"' || next == b'\\') {
                emit(output, &mut out, next);
                i += 2;
                continue;
            }
        }

        if c == b'$' && i + 1 < input_len {
            let next = input[i + 1];

            if next == b'?' {
                let val = super::last_exit_code();
                let n = write_i32_to_buf(val, &mut output[out..]);
                out += n;
                i += 2;
                continue;
            }
            if next == b'$' {
                let val = super::shell_pid();
                let n = write_u32_to_buf(val, &mut output[out..]);
                out += n;
                i += 2;
                continue;
            }
            if next == b'!' {
                let val = super::last_bg_pid();
                let n = write_u32_to_buf(val, &mut output[out..]);
                out += n;
                i += 2;
                continue;
            }
            if next == b'{' {
                i += 2;
                let var_start = i;
                while i < input_len && input[i] != b'}' && input[i] != 0 {
                    i += 1;
                }
                let var_name = &input[var_start..i];
                if i < input_len && input[i] == b'}' {
                    i += 1;
                }
                if let Some((val, val_len)) = super::env::get(var_name) {
                    emit_slice(output, &mut out, &val, val_len);
                }
                continue;
            }
            if is_var_char(next) && next != b'0' || next == b'_' || next.is_ascii_alphabetic() {
                i += 1;
                let var_start = i;
                while i < input_len && is_var_char(input[i]) {
                    i += 1;
                }
                let var_name = &input[var_start..i];
                if let Some((val, val_len)) = super::env::get(var_name) {
                    emit_slice(output, &mut out, &val, val_len);
                }
                continue;
            }
        }

        emit(output, &mut out, c);
        i += 1;
    }

    if out < output.len() {
        output[out] = 0;
    }
    out
}

fn is_operator(b: u8) -> bool {
    b == b'|' || b == b'<' || b == b'>' || b == b'&'
}

pub fn shell_parse_line(line: &[u8], tokens: &mut [*const u8]) -> i32 {
    if line.is_empty() || tokens.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut cursor = 0usize;

    while cursor < line.len() && count < tokens.len() {
        while cursor < line.len() && is_space(line[cursor]) {
            cursor += 1;
        }
        if cursor >= line.len() || line[cursor] == 0 {
            break;
        }

        if line[cursor] == b'|' || line[cursor] == b'<' || line[cursor] == b'&' {
            let mut tok = [0u8; SHELL_MAX_TOKEN_LENGTH];
            tok[0] = line[cursor];
            cursor += 1;
            buffers::with_token_storage(|storage| {
                storage[count][0] = tok[0];
                storage[count][1] = 0;
            });
            tokens[count] = buffers::token_ptr(count);
            count += 1;
            continue;
        }
        if line[cursor] == b'>' {
            cursor += 1;
            let is_append = cursor < line.len() && line[cursor] == b'>';
            if is_append {
                cursor += 1;
            }
            buffers::with_token_storage(|storage| {
                storage[count][0] = b'>';
                if is_append {
                    storage[count][1] = b'>';
                    storage[count][2] = 0;
                } else {
                    storage[count][1] = 0;
                }
            });
            tokens[count] = buffers::token_ptr(count);
            count += 1;
            continue;
        }

        let mut tok = [0u8; SHELL_MAX_TOKEN_LENGTH];
        let mut tok_len = 0usize;
        let mut in_single = false;
        let mut in_double = false;

        while cursor < line.len() && line[cursor] != 0 {
            let c = line[cursor];

            if c == b'\'' && !in_double {
                in_single = !in_single;
                cursor += 1;
                continue;
            }
            if c == b'"' && !in_single {
                in_double = !in_double;
                cursor += 1;
                continue;
            }

            if in_single || in_double {
                if tok_len < SHELL_MAX_TOKEN_LENGTH - 1 {
                    tok[tok_len] = c;
                    tok_len += 1;
                }
                cursor += 1;
                continue;
            }

            if is_space(c) || is_operator(c) {
                break;
            }

            if c == b'\\' && cursor + 1 < line.len() {
                cursor += 1;
                if tok_len < SHELL_MAX_TOKEN_LENGTH - 1 {
                    tok[tok_len] = line[cursor];
                    tok_len += 1;
                }
                cursor += 1;
                continue;
            }

            if tok_len < SHELL_MAX_TOKEN_LENGTH - 1 {
                tok[tok_len] = c;
                tok_len += 1;
            }
            cursor += 1;
        }

        if tok_len > 0 {
            buffers::with_token_storage(|storage| {
                storage[count][..tok_len].copy_from_slice(&tok[..tok_len]);
                storage[count][tok_len] = 0;
            });
            tokens[count] = buffers::token_ptr(count);
            count += 1;
        }
    }

    if count < tokens.len() {
        tokens[count] = ptr::null();
    }
    count as i32
}
