//! Filesystem builtins that change the *shell*: `cd`, `pwd`, `write`.
//!
//! Everything else that used to live here — `ls`, `cat`, `cp`, `mv`, `rm`,
//! `mkdir`, `stat`, `touch`, `head`, `tail`, `wc`, `hexdump`, `diff`, `tee` —
//! is now a real executable in `apps::coreutils`, reached through `PATH` like
//! any other program. A builtin copy would be a second implementation of each,
//! and the one a spawned build tool cannot reach.

use core::option::Option::Some;
use core::result::Result::{Err, Ok};

use std::env;
use std::fs as stdfs;
use std::io::ErrorKind;

use super::super::buffers;
use super::super::display::{COLOR_ERROR_RED, shell_write, shell_write_idx};
use super::super::parser::normalize_path;
use super::super::{
    ERR_MISSING_FILE, ERR_MISSING_TEXT, ERR_NO_SUCH, ERR_TOO_MANY_ARGS, NL, PATH_TOO_LONG,
    SHELL_IO_MAX,
};

pub fn cmd_write(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(ERR_MISSING_FILE.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc < 3 {
        shell_write_idx(ERR_MISSING_TEXT.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let text = argv[2];
        if text.is_empty() {
            shell_write_idx(ERR_MISSING_TEXT.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let len = text.len().min(SHELL_IO_MAX);
        let path_str = path_buf_to_str(path_buf);
        if stdfs::write(path_str, &text[..len]).is_err() {
            shell_write_idx(b"write failed\n", COLOR_ERROR_RED);
            return 1;
        }

        0
    })
}

pub fn cmd_cd(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc > 2 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let mut resolved = buffers::path_scratch();

    if argc < 2 {
        resolved[0] = b'/';
        resolved[1] = 0;
    } else {
        let arg = argv[1];
        if arg.is_empty() {
            resolved[0] = b'/';
            resolved[1] = 0;
        } else if arg == b".." {
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
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
    }

    let resolved_len = resolved.iter().position(|&b| b == 0).unwrap_or(0);
    if resolved_len == 0 {
        resolved[0] = b'/';
        resolved[1] = 0;
    }

    let path_str = path_buf_to_str(&resolved);
    if let Err(e) = env::set_current_dir(path_str) {
        if e.kind() == ErrorKind::NotADirectory {
            shell_write_idx(b"cd: not a directory\n", COLOR_ERROR_RED);
        } else {
            shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
        }
        return 1;
    }

    super::super::cwd_set(&resolved);
    0
}

pub fn cmd_pwd(_argc: i32, _argv: &[&[u8]]) -> i32 {
    if let Ok(path) = env::current_dir() {
        if let Some(path_str) = path.to_str() {
            shell_write(path_str.as_bytes());
            shell_write(NL.as_bytes());
            return 0;
        }
    }

    let cwd = super::super::cwd_bytes();
    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(1);
    shell_write(&cwd[..cwd_len]);
    shell_write(NL.as_bytes());
    0
}

fn path_buf_to_str(path: &[u8]) -> &str {
    let len = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    core::str::from_utf8(&path[..len]).unwrap_or("/")
}
