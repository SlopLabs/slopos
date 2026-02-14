//! File system builtin commands: ls, cat, write, mkdir, rm.

use core::cmp;
use core::ffi::c_char;
use core::ptr;

use crate::runtime;
use crate::syscall::{
    USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, UserFsList, UserFsStat, fs,
};

use super::super::buffers;
use super::super::display::shell_write;
use super::super::parser::normalize_path;
use super::super::{
    ERR_MISSING_FILE, ERR_MISSING_OPERAND, ERR_MISSING_TEXT, ERR_NO_SUCH, ERR_TOO_MANY_ARGS, NL,
    PATH_TOO_LONG, SHELL_IO_MAX,
};
use super::print_kv;

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

        let mut shown = 0usize;

        for i in 0..list.count {
            let entry = &entries[i as usize];
            let name_len = runtime::u_strnlen(entry.name.as_ptr(), entry.name.len());
            if name_len == 1 && entry.name[0] == b'.' {
                continue;
            }
            if name_len == 2 && entry.name[0] == b'.' && entry.name[1] == b'.' {
                continue;
            }
            if entry.is_directory() {
                shell_write(b"[");
                shell_write(&entry.name[..name_len]);
                shell_write(b"]\n");
            } else {
                shell_write(&entry.name[..name_len]);
                shell_write(b" (");
                print_kv(b"", entry.size as u64);
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
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

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

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let mut tmp = [0u8; SHELL_IO_MAX + 1];
        let fd = match fs::open_path(path_buf.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
            Ok(fd) => fd,
            Err(_) => {
                shell_write(ERR_NO_SUCH);
                return 1;
            }
        };
        let r = match fs::read_slice(fd, &mut tmp[..SHELL_IO_MAX]) {
            Ok(n) => n,
            Err(_) => {
                let _ = fs::close_fd(fd);
                shell_write(ERR_NO_SUCH);
                return 1;
            }
        };
        let _ = fs::close_fd(fd);
        let len = cmp::min(r, tmp.len() - 1);
        tmp[len] = 0;
        if len == 0 {
            shell_write(b"cat: empty file\n");
            return 0;
        }
        shell_write(&tmp[..len]);
        if tmp[len - 1] != b'\n' {
            shell_write(NL);
        }
        if r as usize == SHELL_IO_MAX {
            shell_write(b"\n[truncated]\n");
        }
        0
    })
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
