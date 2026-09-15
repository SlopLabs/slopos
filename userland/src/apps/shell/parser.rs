//! Path normalization, which every builtin and redirection target goes
//! through. Command parsing proper lives in `slopos_shell_core`.

#[inline(always)]
pub fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

pub fn normalize_path(input: &[u8], buffer: &mut [u8]) -> i32 {
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

pub fn normalize_path_with_cwd(input: &[u8], buffer: &mut [u8], cwd: &[u8]) -> i32 {
    if buffer.is_empty() {
        return -1;
    }
    if input.is_empty() {
        buffer[0] = b'/';
        if buffer.len() > 1 {
            buffer[1] = 0;
        }
        return 0;
    }

    if input[0] == b'/' {
        // Refusal, not truncation: a silently shortened path names a different
        // file.
        if input.len() >= buffer.len() {
            return -1;
        }
        let len = input.len();
        buffer[..len].copy_from_slice(&input[..len]);
        let collapsed_len = collapse_absolute_path(buffer, len);
        buffer[collapsed_len] = 0;
        return 0;
    }

    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(cwd.len());
    let input_len = input.len();

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
    buffer[cwd_len + sep_len..cwd_len + sep_len + input_len].copy_from_slice(&input[..input_len]);
    let collapsed_len = collapse_absolute_path(buffer, total);
    buffer[collapsed_len] = 0;
    0
}
