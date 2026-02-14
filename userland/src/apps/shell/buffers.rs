//! Static command buffer management for the shell.

use crate::syscall::UserFsEntry;

use super::SyncUnsafeCell;
use super::parser::{SHELL_MAX_TOKEN_LENGTH, SHELL_MAX_TOKENS};

pub const SHELL_PATH_BUF: usize = 128;
pub const EXPAND_BUF_SIZE: usize = 512;

static LINE_BUF: SyncUnsafeCell<[u8; 256]> = SyncUnsafeCell::new([0; 256]);

static EXPAND_BUF: SyncUnsafeCell<[u8; EXPAND_BUF_SIZE]> =
    SyncUnsafeCell::new([0; EXPAND_BUF_SIZE]);

static TOKEN_STORAGE: SyncUnsafeCell<[[u8; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS]> =
    SyncUnsafeCell::new([[0; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS]);

static PATH_BUF: SyncUnsafeCell<[u8; SHELL_PATH_BUF]> = SyncUnsafeCell::new([0; SHELL_PATH_BUF]);

static LIST_ENTRIES: SyncUnsafeCell<[UserFsEntry; 32]> =
    SyncUnsafeCell::new([UserFsEntry::new(); 32]);

pub fn with_line_buf<R, F: FnOnce(&mut [u8; 256]) -> R>(f: F) -> R {
    f(unsafe { &mut *LINE_BUF.get() })
}

pub fn with_expand_buf<R, F: FnOnce(&mut [u8; EXPAND_BUF_SIZE]) -> R>(f: F) -> R {
    f(unsafe { &mut *EXPAND_BUF.get() })
}

pub fn with_token_storage<
    R,
    F: FnOnce(&mut [[u8; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS]) -> R,
>(
    f: F,
) -> R {
    f(unsafe { &mut *TOKEN_STORAGE.get() })
}

pub fn with_path_buf<R, F: FnOnce(&mut [u8; SHELL_PATH_BUF]) -> R>(f: F) -> R {
    f(unsafe { &mut *PATH_BUF.get() })
}

pub fn with_list_entries<R, F: FnOnce(&mut [UserFsEntry; 32]) -> R>(f: F) -> R {
    f(unsafe { &mut *LIST_ENTRIES.get() })
}

pub fn token_ptr(idx: usize) -> *const u8 {
    unsafe { (*TOKEN_STORAGE.get())[idx].as_ptr() }
}
