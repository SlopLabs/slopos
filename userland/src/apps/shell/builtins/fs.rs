//! File system builtin commands: ls, cat, write, mkdir, rm.

use core::cmp;
use core::ffi::c_char;
use core::ptr;

use crate::runtime;
use crate::syscall::{USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, UserFsList, fs};

use super::super::buffers;
use super::super::display::shell_write;
use super::super::parser::normalize_path;
use super::super::{
    ERR_MISSING_FILE, ERR_MISSING_OPERAND, ERR_MISSING_TEXT, ERR_NO_SUCH, ERR_TOO_MANY_ARGS,
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
            b"/\0".as_ptr()
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
        let mut list = UserFsList {
            entries: entries.as_mut_ptr(),
            max_entries: entries.len() as u32,
            count: 0,
        };

        if fs::list_dir(path as *const c_char, &mut list).is_err() {
            shell_write(ERR_NO_SUCH);
            return 1;
        }

        for i in 0..list.count {
            let entry = &entries[i as usize];
            if entry.is_directory() {
                shell_write(b"[");
                shell_write(
                    &entry.name[..runtime::u_strnlen(entry.name.as_ptr(), entry.name.len())],
                );
                shell_write(b"]\n");
            } else {
                let name_len = runtime::u_strnlen(entry.name.as_ptr(), entry.name.len());
                shell_write(&entry.name[..name_len]);
                shell_write(b" (");
                print_kv(b"", entry.size as u64);
            }
        }
        0
    });

    result
}

pub fn cmd_cat(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_FILE);
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
        shell_write(&tmp[..len]);
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
        let text_slice = unsafe { core::slice::from_raw_parts(text, len) };
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
