//! File system builtin commands: ls, cat, write, mkdir, rm, cd, pwd,
//! stat, touch, cp, mv, head, tail, wc, hexdump, tee, diff.

use core::cmp;
use core::ffi::c_char;
use core::ptr;

use crate::runtime;
use crate::syscall::{
    USER_FS_OPEN_APPEND, USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, UserFsEntry,
    UserFsList, UserFsStat, fs,
};

use super::super::buffers;
use super::super::display::shell_write;
use super::super::jobs;
use super::super::parser::normalize_path;
use super::super::{
    ERR_MISSING_FILE, ERR_MISSING_OPERAND, ERR_MISSING_TEXT, ERR_NO_SUCH, ERR_TOO_MANY_ARGS, NL,
    PATH_TOO_LONG, SHELL_IO_MAX,
};

pub fn cmd_ls(argc: i32, argv: &[*const u8]) -> i32 {
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let path_ptr = if argc == 2 { argv[1] } else { ptr::null() };

    let path = buffers::with_path_buf(|path_buf| {
        if path_ptr.is_null() {
            let cwd = super::super::cwd_bytes();
            let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(1);
            let len = cwd_len.min(path_buf.len() - 1);
            path_buf[..len].copy_from_slice(&cwd[..len]);
            path_buf[len] = 0;
            path_buf.as_ptr()
        } else {
            if normalize_path(path_ptr, path_buf) != 0 {
                shell_write(PATH_TOO_LONG);
                return ptr::null();
            }
            path_buf.as_ptr()
        }
    });

    if path.is_null() {
        return 1;
    }

    let result = buffers::with_list_entries(|entries| {
        let mut stat = UserFsStat::default();
        if fs::stat_path(path as *const c_char, &mut stat).is_err() {
            shell_write(ERR_NO_SUCH);
            return 1;
        }
        if !stat.is_directory() {
            shell_write(b"ls: not a directory\n");
            return 1;
        }

        let mut list = UserFsList {
            entries: entries.as_mut_ptr(),
            max_entries: entries.len() as u32,
            count: 0,
        };

        if fs::list_dir(path as *const c_char, &mut list).is_err() {
            shell_write(ERR_NO_SUCH);
            return 1;
        }

        let count = list.count as usize;

        // Sort entries alphabetically (case-insensitive)
        if count > 1 {
            for i in 0..count - 1 {
                for j in 0..count - 1 - i {
                    if entry_name_gt(&entries[j], &entries[j + 1]) {
                        entries.swap(j, j + 1);
                    }
                }
            }
        }

        let mut shown = 0usize;

        for i in 0..count {
            let entry = &entries[i];
            let name_len = runtime::u_strnlen(entry.name.as_ptr(), entry.name.len());
            if name_len == 1 && entry.name[0] == b'.' {
                continue;
            }
            if name_len == 2 && entry.name[0] == b'.' && entry.name[1] == b'.' {
                continue;
            }
            if entry.is_directory() {
                shell_write(&entry.name[..name_len]);
                shell_write(b"/\n");
            } else {
                shell_write(&entry.name[..name_len]);
                shell_write(b" (");
                jobs::write_u64(entry.size as u64);
                shell_write(b")\n");
            }
            shown += 1;
        }

        if shown == 0 {
            shell_write(b"(empty)\n");
        }
        0
    });

    result
}

pub fn cmd_cat(argc: i32, argv: &[*const u8]) -> i32 {
    if argc == 1 {
        let mut tmp = [0u8; SHELL_IO_MAX + 1];
        let r = match fs::read_slice(0, &mut tmp[..SHELL_IO_MAX]) {
            Ok(n) => n,
            Err(_) => {
                shell_write(b"cat: stdin read failed\n");
                return 1;
            }
        };
        let len = cmp::min(r, tmp.len() - 1);
        tmp[len] = 0;
        if len == 0 {
            return 0;
        }
        shell_write(&tmp[..len]);
        if tmp[len - 1] != b'\n' {
            shell_write(NL);
        }
        if r as usize == SHELL_IO_MAX {
            shell_write(b"\n[truncated]\n");
        }
        return 0;
    }

    let mut rc = 0;
    for i in 1..argc as usize {
        if i >= argv.len() || argv[i].is_null() {
            continue;
        }
        let result = buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[i], path_buf) != 0 {
                shell_write(PATH_TOO_LONG);
                return 1;
            }

            let mut tmp = [0u8; SHELL_IO_MAX + 1];
            let fd = match fs::open_path(path_buf.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
                Ok(fd) => fd,
                Err(_) => {
                    shell_write(b"cat: ");
                    shell_write(ERR_NO_SUCH);
                    return 1;
                }
            };
            let r = match fs::read_slice(fd, &mut tmp[..SHELL_IO_MAX]) {
                Ok(n) => n,
                Err(_) => {
                    let _ = fs::close_fd(fd);
                    shell_write(b"cat: read error\n");
                    return 1;
                }
            };
            let _ = fs::close_fd(fd);
            let len = cmp::min(r, tmp.len() - 1);
            tmp[len] = 0;
            if len == 0 {
                return 0;
            }
            shell_write(&tmp[..len]);
            if tmp[len - 1] != b'\n' {
                shell_write(NL);
            }
            if r as usize == SHELL_IO_MAX {
                shell_write(b"[truncated]\n");
            }
            0
        });
        if result != 0 {
            rc = result;
        }
    }
    rc
}

pub fn cmd_write(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc < 3 {
        shell_write(ERR_MISSING_TEXT);
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let text = argv[2];
        if text.is_null() {
            shell_write(ERR_MISSING_TEXT);
            return 1;
        }

        let mut len = runtime::u_strlen(text);
        if len > SHELL_IO_MAX {
            len = SHELL_IO_MAX;
        }

        let text_slice = unsafe { core::slice::from_raw_parts(text, len) };
        let _ = fs::unlink_path(path_buf.as_ptr() as *const c_char);
        let fd = match fs::open_path(
            path_buf.as_ptr() as *const c_char,
            USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT,
        ) {
            Ok(fd) => fd,
            Err(_) => {
                shell_write(b"write failed\n");
                return 1;
            }
        };
        let w = match fs::write_slice(fd, text_slice) {
            Ok(n) => n,
            Err(_) => {
                let _ = fs::close_fd(fd);
                shell_write(b"write failed\n");
                return 1;
            }
        };
        let _ = fs::close_fd(fd);
        if w != len {
            shell_write(b"write failed\n");
            return 1;
        }
        0
    })
}

pub fn cmd_mkdir(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }
        if fs::mkdir_path(path_buf.as_ptr() as *const c_char).is_err() {
            shell_write(b"mkdir failed\n");
            return 1;
        }
        0
    })
}

pub fn cmd_rm(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }
        if fs::unlink_path(path_buf.as_ptr() as *const c_char).is_err() {
            shell_write(b"rm failed\n");
            return 1;
        }
        0
    })
}

pub fn cmd_cd(argc: i32, argv: &[*const u8]) -> i32 {
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let mut resolved = [0u8; 256];

    if argc < 2 {
        resolved[0] = b'/';
        resolved[1] = 0;
    } else {
        let arg = argv[1];
        if arg.is_null() {
            resolved[0] = b'/';
            resolved[1] = 0;
        } else {
            let arg_len = runtime::u_strlen(arg);
            let arg_bytes = unsafe { core::slice::from_raw_parts(arg, arg_len) };

            if arg_len == 2 && arg_bytes[0] == b'.' && arg_bytes[1] == b'.' {
                let cwd = super::super::cwd_bytes();
                let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(1);
                if cwd_len <= 1 {
                    resolved[0] = b'/';
                    resolved[1] = 0;
                } else {
                    let mut last_slash = 0;
                    for i in 0..cwd_len {
                        if cwd[i] == b'/' && i > 0 {
                            last_slash = i;
                        }
                    }
                    if last_slash == 0 {
                        resolved[0] = b'/';
                        resolved[1] = 0;
                    } else {
                        resolved[..last_slash].copy_from_slice(&cwd[..last_slash]);
                        resolved[last_slash] = 0;
                    }
                }
            } else if normalize_path(arg, &mut resolved) != 0 {
                shell_write(PATH_TOO_LONG);
                return 1;
            }
        }
    }

    let resolved_len = resolved.iter().position(|&b| b == 0).unwrap_or(0);
    if resolved_len == 0 {
        resolved[0] = b'/';
        resolved[1] = 0;
    }

    let mut stat = UserFsStat::default();
    if fs::stat_path(resolved.as_ptr() as *const c_char, &mut stat).is_err() {
        shell_write(ERR_NO_SUCH);
        return 1;
    }
    if !stat.is_directory() {
        shell_write(b"cd: not a directory\n");
        return 1;
    }

    super::super::cwd_set(&resolved);
    0
}

pub fn cmd_pwd(_argc: i32, _argv: &[*const u8]) -> i32 {
    let cwd = super::super::cwd_bytes();
    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(1);
    shell_write(&cwd[..cwd_len]);
    shell_write(NL);
    0
}

pub fn cmd_stat(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let mut stat = UserFsStat::default();
        if fs::stat_path(path_buf.as_ptr() as *const c_char, &mut stat).is_err() {
            shell_write(ERR_NO_SUCH);
            return 1;
        }

        let path_len = path_buf.iter().position(|&b| b == 0).unwrap_or(0);
        shell_write(b"  File: ");
        shell_write(&path_buf[..path_len]);
        shell_write(NL);

        shell_write(b"  Type: ");
        match stat.type_ {
            0 => shell_write(b"regular file"),
            1 => shell_write(b"directory"),
            2 => shell_write(b"character device"),
            _ => shell_write(b"unknown"),
        }
        shell_write(NL);

        shell_write(b"  Size: ");
        jobs::write_u64(stat.size as u64);
        shell_write(NL);

        0
    })
}

pub fn cmd_touch(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_OPERAND);
        return 1;
    }

    let mut rc = 0;
    for i in 1..argc as usize {
        if i >= argv.len() || argv[i].is_null() {
            continue;
        }
        let result = buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[i], path_buf) != 0 {
                shell_write(PATH_TOO_LONG);
                return 1;
            }

            let mut stat = UserFsStat::default();
            if fs::stat_path(path_buf.as_ptr() as *const c_char, &mut stat).is_ok() {
                return 0;
            }

            let fd = match fs::open_path(
                path_buf.as_ptr() as *const c_char,
                USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT,
            ) {
                Ok(fd) => fd,
                Err(_) => {
                    shell_write(b"touch: cannot create file\n");
                    return 1;
                }
            };
            let _ = fs::close_fd(fd);
            0
        });
        if result != 0 {
            rc = result;
        }
    }
    rc
}

pub fn cmd_cp(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 3 {
        shell_write(b"cp: missing operand\n");
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let mut src_path = [0u8; 256];
    let mut dst_path = [0u8; 256];

    if normalize_path(argv[1], &mut src_path) != 0 {
        shell_write(PATH_TOO_LONG);
        return 1;
    }
    if normalize_path(argv[2], &mut dst_path) != 0 {
        shell_write(PATH_TOO_LONG);
        return 1;
    }

    if paths_equal(&src_path, &dst_path) {
        shell_write(b"cp: source and destination are the same\n");
        return 1;
    }

    copy_file_inner(&src_path, &dst_path)
}

pub fn cmd_mv(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 3 {
        shell_write(b"mv: missing operand\n");
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let mut src_path = [0u8; 256];
    let mut dst_path = [0u8; 256];

    if normalize_path(argv[1], &mut src_path) != 0 {
        shell_write(PATH_TOO_LONG);
        return 1;
    }
    if normalize_path(argv[2], &mut dst_path) != 0 {
        shell_write(PATH_TOO_LONG);
        return 1;
    }

    if paths_equal(&src_path, &dst_path) {
        shell_write(b"mv: source and destination are the same\n");
        return 1;
    }

    let rc = copy_file_inner(&src_path, &dst_path);
    if rc != 0 {
        return rc;
    }

    if fs::unlink_path(src_path.as_ptr() as *const c_char).is_err() {
        shell_write(b"mv: cannot remove source\n");
        return 1;
    }
    0
}

pub fn cmd_head(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let n_lines: usize = if argc >= 3 {
        match jobs::parse_u32_arg(argv[2]) {
            Some(n) if n > 0 => n as usize,
            _ => {
                shell_write(b"head: invalid line count\n");
                return 1;
            }
        }
    } else {
        10
    };

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let fd = match fs::open_path(path_buf.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
            Ok(fd) => fd,
            Err(_) => {
                shell_write(ERR_NO_SUCH);
                return 1;
            }
        };

        let mut lines_seen = 0usize;
        let mut buf = [0u8; SHELL_IO_MAX];
        let mut done = false;

        while !done {
            let n = match fs::read_slice(fd, &mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }

            let mut output_end = n;
            for i in 0..n {
                if buf[i] == b'\n' {
                    lines_seen += 1;
                    if lines_seen >= n_lines {
                        output_end = i + 1;
                        done = true;
                        break;
                    }
                }
            }
            shell_write(&buf[..output_end]);
        }

        let _ = fs::close_fd(fd);
        0
    })
}

pub fn cmd_tail(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let n_lines: usize = if argc >= 3 {
        match jobs::parse_u32_arg(argv[2]) {
            Some(n) if n > 0 => n as usize,
            _ => {
                shell_write(b"tail: invalid line count\n");
                return 1;
            }
        }
    } else {
        10
    };

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let fd = match fs::open_path(path_buf.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
            Ok(fd) => fd,
            Err(_) => {
                shell_write(ERR_NO_SUCH);
                return 1;
            }
        };

        const TAIL_BUF: usize = 4096;
        let mut data = [0u8; TAIL_BUF];
        let mut total = 0usize;

        loop {
            if total >= TAIL_BUF {
                break;
            }
            let chunk = (TAIL_BUF - total).min(SHELL_IO_MAX);
            let n = match fs::read_slice(fd, &mut data[total..total + chunk]) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            total += n;
        }

        let _ = fs::close_fd(fd);

        if total == 0 {
            return 0;
        }

        // Skip trailing newline so it doesn't count as an extra empty line
        let mut count = 0usize;
        let scan_start = if data[total - 1] == b'\n' {
            total.saturating_sub(1)
        } else {
            total
        };

        let mut start = 0usize;
        let mut pos = scan_start;
        while pos > 0 {
            pos -= 1;
            if data[pos] == b'\n' {
                count += 1;
                if count >= n_lines {
                    start = pos + 1;
                    break;
                }
            }
        }

        shell_write(&data[start..total]);
        if data[total - 1] != b'\n' {
            shell_write(NL);
        }
        0
    })
}

pub fn cmd_wc(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let mut buf = [0u8; SHELL_IO_MAX];
        let n = match fs::read_slice(0, &mut buf) {
            Ok(n) => n,
            Err(_) => {
                shell_write(b"wc: read error\n");
                return 1;
            }
        };
        let (lines, words, chars) = count_lwc(&buf[..n]);
        write_wc_line(lines, words, chars, b"");
        return 0;
    }

    let mut total_lines = 0usize;
    let mut total_words = 0usize;
    let mut total_chars = 0usize;
    let file_count = (argc - 1) as usize;
    let mut rc = 0;

    for i in 1..argc as usize {
        if i >= argv.len() || argv[i].is_null() {
            continue;
        }
        let result = buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[i], path_buf) != 0 {
                shell_write(PATH_TOO_LONG);
                return 1;
            }

            let fd = match fs::open_path(path_buf.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
                Ok(fd) => fd,
                Err(_) => {
                    shell_write(b"wc: ");
                    shell_write(ERR_NO_SUCH);
                    return 1;
                }
            };

            let mut lines = 0usize;
            let mut words = 0usize;
            let mut chars = 0usize;
            let mut in_word = false;
            let mut buf = [0u8; SHELL_IO_MAX];

            loop {
                let n = match fs::read_slice(fd, &mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                chars += n;
                for j in 0..n {
                    if buf[j] == b'\n' {
                        lines += 1;
                    }
                    if is_wc_space(buf[j]) {
                        if in_word {
                            words += 1;
                            in_word = false;
                        }
                    } else {
                        in_word = true;
                    }
                }
            }
            if in_word {
                words += 1;
            }
            let _ = fs::close_fd(fd);

            let path_len = path_buf.iter().position(|&b| b == 0).unwrap_or(0);
            write_wc_line(lines, words, chars, &path_buf[..path_len]);

            total_lines += lines;
            total_words += words;
            total_chars += chars;
            0
        });
        if result != 0 {
            rc = result;
        }
    }

    if file_count > 1 {
        write_wc_line(total_lines, total_words, total_chars, b"total");
    }
    rc
}

pub fn cmd_hexdump(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let max_bytes: usize = if argc >= 3 {
        match jobs::parse_u32_arg(argv[2]) {
            Some(n) => n as usize,
            None => {
                shell_write(b"hexdump: invalid byte count\n");
                return 1;
            }
        }
    } else {
        256
    };

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let fd = match fs::open_path(path_buf.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
            Ok(fd) => fd,
            Err(_) => {
                shell_write(ERR_NO_SUCH);
                return 1;
            }
        };

        let read_len = max_bytes.min(SHELL_IO_MAX);
        let mut buf = [0u8; SHELL_IO_MAX];
        let n = match fs::read_slice(fd, &mut buf[..read_len]) {
            Ok(n) => n,
            Err(_) => {
                let _ = fs::close_fd(fd);
                shell_write(b"hexdump: read error\n");
                return 1;
            }
        };
        let _ = fs::close_fd(fd);

        if n == 0 {
            shell_write(b"(empty)\n");
            return 0;
        }

        let mut offset = 0usize;
        while offset < n {
            let line_len = (n - offset).min(16);

            write_hex_u16(offset as u16);
            shell_write(b": ");

            for i in 0..16usize {
                if i < line_len {
                    write_hex_byte(buf[offset + i]);
                    shell_write(b" ");
                } else {
                    shell_write(b"   ");
                }
                if i == 7 {
                    shell_write(b" ");
                }
            }

            shell_write(b" |");
            for i in 0..line_len {
                let b = buf[offset + i];
                if (0x20..=0x7E).contains(&b) {
                    let ch = [b];
                    shell_write(&ch);
                } else {
                    shell_write(b".");
                }
            }
            shell_write(b"|\n");

            offset += 16;
        }

        0
    })
}

pub fn cmd_diff(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 3 {
        shell_write(b"diff: missing operand\n");
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let mut path1 = [0u8; 256];
    let mut path2 = [0u8; 256];

    if normalize_path(argv[1], &mut path1) != 0 {
        shell_write(PATH_TOO_LONG);
        return 1;
    }
    if normalize_path(argv[2], &mut path2) != 0 {
        shell_write(PATH_TOO_LONG);
        return 1;
    }

    const DIFF_BUF: usize = 2048;
    let mut data1 = [0u8; DIFF_BUF];
    let mut data2 = [0u8; DIFF_BUF];

    let len1 = match read_file_into_buf(&path1, &mut data1) {
        Some(n) => n,
        None => return 1,
    };
    let len2 = match read_file_into_buf(&path2, &mut data2) {
        Some(n) => n,
        None => return 1,
    };

    let mut pos1 = 0usize;
    let mut pos2 = 0usize;
    let mut line_num = 1usize;
    let mut differ = false;

    while pos1 < len1 || pos2 < len2 {
        let (end1, line1_len) = if pos1 < len1 {
            find_line_end(&data1[pos1..len1])
        } else {
            (0, 0)
        };
        let (end2, line2_len) = if pos2 < len2 {
            find_line_end(&data2[pos2..len2])
        } else {
            (0, 0)
        };

        let line1 = &data1[pos1..pos1 + line1_len];
        let line2 = &data2[pos2..pos2 + line2_len];

        if line1 != line2 {
            differ = true;
            jobs::write_u64(line_num as u64);
            shell_write(b"c");
            jobs::write_u64(line_num as u64);
            shell_write(NL);
            shell_write(b"< ");
            shell_write(line1);
            shell_write(NL);
            shell_write(b"---\n");
            shell_write(b"> ");
            shell_write(line2);
            shell_write(NL);
        }

        if end1 == 0 && end2 == 0 {
            break;
        }
        pos1 += end1;
        pos2 += end2;
        line_num += 1;
    }

    if differ { 1 } else { 0 }
}

pub fn cmd_tee(argc: i32, argv: &[*const u8]) -> i32 {
    let mut append = false;
    let mut file_arg: Option<usize> = None;

    // Parse arguments: tee [-a] [file]
    let mut i = 1usize;
    while i < argc as usize {
        if i >= argv.len() || argv[i].is_null() {
            i += 1;
            continue;
        }
        let arg = argv[i];
        let len = runtime::u_strlen(arg);
        let bytes = unsafe { core::slice::from_raw_parts(arg, len) };
        if len == 2 && bytes[0] == b'-' && bytes[1] == b'a' {
            append = true;
        } else {
            file_arg = Some(i);
        }
        i += 1;
    }

    // Open file if specified
    let file_fd = if let Some(idx) = file_arg {
        buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[idx], path_buf) != 0 {
                shell_write(PATH_TOO_LONG);
                return -1i32;
            }
            let flags = if append {
                USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT | USER_FS_OPEN_APPEND
            } else {
                USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT
            };
            // For truncate mode, remove existing file first
            if !append {
                let _ = fs::unlink_path(path_buf.as_ptr() as *const c_char);
            }
            match fs::open_path(path_buf.as_ptr() as *const c_char, flags) {
                Ok(fd) => fd as i32,
                Err(_) => {
                    shell_write(b"tee: cannot open file\n");
                    -1i32
                }
            }
        })
    } else {
        -1i32
    };

    if file_arg.is_some() && file_fd < 0 {
        return 1;
    }

    let mut buf = [0u8; SHELL_IO_MAX];
    loop {
        let n = match fs::read_slice(0, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        shell_write(&buf[..n]);
        if file_fd >= 0 {
            let _ = fs::write_slice(file_fd as i32, &buf[..n]);
        }
    }

    if file_fd >= 0 {
        let _ = fs::close_fd(file_fd as i32);
    }
    0
}

fn entry_name_gt(a: &UserFsEntry, b: &UserFsEntry) -> bool {
    let a_len = a.name.iter().position(|&c| c == 0).unwrap_or(a.name.len());
    let b_len = b.name.iter().position(|&c| c == 0).unwrap_or(b.name.len());
    let min_len = a_len.min(b_len);

    for i in 0..min_len {
        let ca = a.name[i].to_ascii_lowercase();
        let cb = b.name[i].to_ascii_lowercase();
        if ca != cb {
            return ca > cb;
        }
    }
    a_len > b_len
}

fn copy_file_inner(src_path: &[u8], dst_path: &[u8]) -> i32 {
    let mut stat = UserFsStat::default();
    if fs::stat_path(src_path.as_ptr() as *const c_char, &mut stat).is_err() {
        shell_write(ERR_NO_SUCH);
        return 1;
    }
    if stat.is_directory() {
        shell_write(b"cannot copy directory\n");
        return 1;
    }

    let src_fd = match fs::open_path(src_path.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
        Ok(fd) => fd,
        Err(_) => {
            shell_write(b"cannot open source\n");
            return 1;
        }
    };

    let _ = fs::unlink_path(dst_path.as_ptr() as *const c_char);
    let dst_fd = match fs::open_path(
        dst_path.as_ptr() as *const c_char,
        USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT,
    ) {
        Ok(fd) => fd,
        Err(_) => {
            let _ = fs::close_fd(src_fd);
            shell_write(b"cannot create destination\n");
            return 1;
        }
    };

    let mut buf = [0u8; SHELL_IO_MAX];
    loop {
        let n = match fs::read_slice(src_fd, &mut buf) {
            Ok(n) => n,
            Err(_) => {
                let _ = fs::close_fd(src_fd);
                let _ = fs::close_fd(dst_fd);
                shell_write(b"read error\n");
                return 1;
            }
        };
        if n == 0 {
            break;
        }
        if fs::write_slice(dst_fd, &buf[..n]).is_err() {
            let _ = fs::close_fd(src_fd);
            let _ = fs::close_fd(dst_fd);
            shell_write(b"write error\n");
            return 1;
        }
    }

    let _ = fs::close_fd(src_fd);
    let _ = fs::close_fd(dst_fd);
    0
}

fn read_file_into_buf(path: &[u8], buf: &mut [u8]) -> Option<usize> {
    let fd = match fs::open_path(path.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
        Ok(fd) => fd,
        Err(_) => {
            shell_write(ERR_NO_SUCH);
            return None;
        }
    };

    let mut total = 0usize;
    loop {
        if total >= buf.len() {
            break;
        }
        let chunk = (buf.len() - total).min(SHELL_IO_MAX);
        let n = match fs::read_slice(fd, &mut buf[total..total + chunk]) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        total += n;
    }

    let _ = fs::close_fd(fd);
    Some(total)
}

fn find_line_end(data: &[u8]) -> (usize, usize) {
    if data.is_empty() {
        return (0, 0);
    }
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            return (i + 1, i);
        }
    }
    (data.len(), data.len())
}

fn count_lwc(data: &[u8]) -> (usize, usize, usize) {
    let mut lines = 0usize;
    let mut words = 0usize;
    let chars = data.len();
    let mut in_word = false;

    for &b in data {
        if b == b'\n' {
            lines += 1;
        }
        if is_wc_space(b) {
            if in_word {
                words += 1;
                in_word = false;
            }
        } else {
            in_word = true;
        }
    }
    if in_word {
        words += 1;
    }

    (lines, words, chars)
}

fn is_wc_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

fn write_wc_line(lines: usize, words: usize, chars: usize, name: &[u8]) {
    shell_write(b"  ");
    jobs::write_u64(lines as u64);
    shell_write(b"  ");
    jobs::write_u64(words as u64);
    shell_write(b"  ");
    jobs::write_u64(chars as u64);
    if !name.is_empty() {
        shell_write(b" ");
        shell_write(name);
    }
    shell_write(NL);
}

fn write_hex_byte(b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let out = [HEX[(b >> 4) as usize], HEX[(b & 0x0F) as usize]];
    shell_write(&out);
}

fn write_hex_u16(val: u16) {
    write_hex_byte((val >> 8) as u8);
    write_hex_byte(val as u8);
}

fn paths_equal(a: &[u8], b: &[u8]) -> bool {
    let a_len = a.iter().position(|&c| c == 0).unwrap_or(a.len());
    let b_len = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    if a_len != b_len {
        return false;
    }
    a[..a_len] == b[..b_len]
}
