use core::ffi::{c_char, c_int};
use core::mem::{self, MaybeUninit};
use core::slice;

use slopos_lib::{InitFlag, IrqMutex};

use slopos_abi::fs::{FS_TYPE_FILE, USER_FS_OPEN_CREAT, UserFsEntry, UserFsStat};
use slopos_abi::syscall::{
    F_DUPFD, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, SEEK_CUR, SEEK_END, SEEK_SET,
};

use crate::vfs::{FileSystem, InodeId, vfs_list, vfs_mkdir, vfs_open, vfs_stat, vfs_unlink};

#[allow(non_camel_case_types)]
type ssize_t = isize;

const FILE_OPEN_READ: u32 = 1 << 0;
const FILE_OPEN_WRITE: u32 = 1 << 1;
const FILE_OPEN_APPEND: u32 = 1 << 3;

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_mm::memory_layout_defs::MAX_PROCESSES;

use crate::MAX_PATH_LEN;

const FILEIO_MAX_OPEN_FILES: usize = 32;

#[derive(Clone, Copy)]
struct FileDescriptor {
    inode: InodeId,
    fs: Option<&'static dyn FileSystem>,
    position: usize,
    flags: u32,
    valid: bool,
    cloexec: bool,
    /// When true, reads/writes route to the platform console/TTY instead of a filesystem.
    console: bool,
}

impl FileDescriptor {
    const fn new() -> Self {
        Self {
            inode: 0,
            fs: None,
            position: 0,
            flags: 0,
            valid: false,
            cloexec: false,
            console: false,
        }
    }
}

unsafe impl Send for FileDescriptor {}

struct FileTableSlot {
    process_id: u32,
    in_use: bool,
    lock: IrqMutex<()>,
    descriptors: [FileDescriptor; FILEIO_MAX_OPEN_FILES],
}

impl FileTableSlot {
    const fn new(in_use: bool) -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            in_use,
            lock: IrqMutex::new(()),
            descriptors: [FileDescriptor::new(); FILEIO_MAX_OPEN_FILES],
        }
    }
}

unsafe impl Send for FileTableSlot {}

struct FileioState {
    initialized: bool,
    kernel: MaybeUninit<FileTableSlot>,
    processes: [MaybeUninit<FileTableSlot>; MAX_PROCESSES],
}

impl FileioState {
    const fn uninitialized() -> Self {
        let processes: [MaybeUninit<FileTableSlot>; MAX_PROCESSES] = unsafe {
            MaybeUninit::<[MaybeUninit<FileTableSlot>; MAX_PROCESSES]>::uninit().assume_init()
        };
        Self {
            initialized: false,
            kernel: MaybeUninit::uninit(),
            processes,
        }
    }
}

unsafe impl Send for FileioState {}

static FILEIO_STATE: IrqMutex<FileioState> = IrqMutex::new(FileioState::uninitialized());
static FILEIO_INIT: InitFlag = InitFlag::new();

fn with_state<R>(f: impl FnOnce(&mut FileioState) -> R) -> R {
    let mut guard = FILEIO_STATE.lock();
    f(&mut *guard)
}

fn with_tables<R>(
    f: impl FnOnce(&mut FileTableSlot, &mut [FileTableSlot; MAX_PROCESSES]) -> R,
) -> R {
    with_state(|state| {
        ensure_initialized(state);
        let kernel = unsafe { state.kernel.assume_init_mut() };
        let processes = unsafe {
            mem::transmute::<_, &mut [FileTableSlot; MAX_PROCESSES]>(&mut state.processes)
        };
        f(kernel, processes)
    })
}

fn reset_descriptor(desc: &mut FileDescriptor) {
    desc.inode = 0;
    desc.fs = None;
    desc.position = 0;
    desc.flags = 0;
    desc.valid = false;
    desc.cloexec = false;
    desc.console = false;
}

fn reset_table(table: &mut FileTableSlot) {
    for desc in table.descriptors.iter_mut() {
        reset_descriptor(desc);
    }
}

fn find_free_table(processes: &mut [FileTableSlot; MAX_PROCESSES]) -> Option<&mut FileTableSlot> {
    for slot in processes.iter_mut() {
        if !slot.in_use {
            return Some(slot);
        }
    }
    None
}

fn table_for_pid<'a>(
    kernel: &'a mut FileTableSlot,
    processes: &'a mut [FileTableSlot; MAX_PROCESSES],
    pid: u32,
) -> Option<&'a mut FileTableSlot> {
    if pid == INVALID_PROCESS_ID {
        return Some(kernel);
    }
    for slot in processes.iter_mut() {
        if slot.in_use && slot.process_id == pid {
            return Some(slot);
        }
    }
    None
}

fn get_descriptor<'a>(table: &'a mut FileTableSlot, fd: c_int) -> Option<&'a mut FileDescriptor> {
    if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
        return None;
    }
    let desc = &mut table.descriptors[fd as usize];
    if !desc.valid {
        return None;
    }
    Some(desc)
}

fn find_free_slot(table: &FileTableSlot) -> Option<usize> {
    find_free_slot_from(table, 0)
}

fn find_free_slot_from(table: &FileTableSlot, min_fd: usize) -> Option<usize> {
    for idx in min_fd..FILEIO_MAX_OPEN_FILES {
        if !table.descriptors[idx].valid {
            return Some(idx);
        }
    }
    None
}

fn ensure_initialized(state: &mut FileioState) {
    if !FILEIO_INIT.init_once() {
        return;
    }

    state.kernel.write(FileTableSlot::new(true));
    for slot in state.processes.iter_mut() {
        slot.write(FileTableSlot::new(false));
    }
    let kernel = unsafe { state.kernel.assume_init_mut() };
    reset_table(kernel);
    let processes =
        unsafe { mem::transmute::<_, &mut [FileTableSlot; MAX_PROCESSES]>(&mut state.processes) };
    for slot in processes.iter_mut() {
        reset_table(slot);
        slot.process_id = INVALID_PROCESS_ID;
        slot.in_use = false;
    }
    state.initialized = true;
}

unsafe fn cstr_len(ptr_in: *const c_char) -> usize {
    if ptr_in.is_null() {
        return 0;
    }
    let mut len = 0usize;
    unsafe {
        while *ptr_in.add(len) != 0 {
            len += 1;
        }
    }
    len
}

unsafe fn path_bytes<'a>(path: *const c_char) -> Option<&'a [u8]> {
    if path.is_null() {
        return None;
    }
    unsafe {
        let len = cstr_len(path);
        Some(slice::from_raw_parts(
            path as *const u8,
            len.min(MAX_PATH_LEN),
        ))
    }
}

/// Bootstrap FD 0 (stdin), 1 (stdout), 2 (stderr) as console descriptors.
///
/// Console descriptors are valid file descriptors that route reads/writes
/// through the platform console/TTY instead of a filesystem.  This ensures
/// every new user process satisfies the POSIX FD bootstrap contract.
fn bootstrap_console_fds(table: &mut FileTableSlot) {
    // FD 0 = stdin (read-only console)
    table.descriptors[0] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_READ,
        valid: true,
        cloexec: false,
        console: true,
    };
    // FD 1 = stdout (write-only console)
    table.descriptors[1] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_WRITE,
        valid: true,
        cloexec: false,
        console: true,
    };
    // FD 2 = stderr (write-only console)
    table.descriptors[2] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_WRITE,
        valid: true,
        cloexec: false,
        console: true,
    };
}

pub fn fileio_create_table_for_process(process_id: u32) -> c_int {
    if process_id == INVALID_PROCESS_ID {
        return 0;
    }
    with_tables(|kernel, processes| {
        if table_for_pid(kernel, processes, process_id).is_some() {
            return 0;
        }
        let Some(slot) = find_free_table(processes) else {
            return -1;
        };
        reset_table(slot);
        slot.process_id = process_id;
        slot.in_use = true;
        bootstrap_console_fds(slot);
        0
    })
}

pub fn fileio_destroy_table_for_process(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        if let Some(table) = table_for_pid(kernel, processes, process_id) {
            let table_ptr = table as *mut FileTableSlot;
            if table_ptr == kernel_ptr {
                return;
            }
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            unsafe {
                reset_table(&mut *table_ptr);
                (*table_ptr).process_id = INVALID_PROCESS_ID;
                (*table_ptr).in_use = false;
            }
            drop(guard);
        }
    });
}

pub fn fileio_clone_table_for_process(src_process_id: u32, dst_process_id: u32) -> c_int {
    if src_process_id == INVALID_PROCESS_ID || dst_process_id == INVALID_PROCESS_ID {
        return -1;
    }
    if src_process_id == dst_process_id {
        return 0;
    }

    with_tables(|kernel, processes| {
        let src_table = match table_for_pid(kernel, processes, src_process_id) {
            Some(t) => t as *const FileTableSlot,
            None => return -1,
        };

        let dst_slot = match find_free_table(processes) {
            Some(s) => s,
            None => return -1,
        };

        reset_table(dst_slot);
        dst_slot.process_id = dst_process_id;
        dst_slot.in_use = true;

        for (i, src_desc) in unsafe { (*src_table).descriptors.iter().enumerate() } {
            if src_desc.valid {
                dst_slot.descriptors[i] = *src_desc;
            }
        }

        0
    })
}

pub fn file_open_for_process(process_id: u32, path: *const c_char, flags: u32) -> c_int {
    if path.is_null() || (flags & (FILE_OPEN_READ | FILE_OPEN_WRITE)) == 0 {
        return -1;
    }
    if (flags & FILE_OPEN_APPEND) != 0 && (flags & FILE_OPEN_WRITE) == 0 {
        return -1;
    }

    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };

    let create = (flags & USER_FS_OPEN_CREAT) != 0;

    let handle = match vfs_open(path_bytes, create) {
        Ok(h) => h,
        Err(_) => return -1,
    };

    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
            t as *mut FileTableSlot
        } else if let Some(t) = find_free_table(processes) {
            t as *mut FileTableSlot
        } else {
            kernel_ptr
        };
        let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };

        let position = if (flags & FILE_OPEN_APPEND) != 0 {
            match handle.size() {
                Ok(size) => size as usize,
                Err(_) => {
                    drop(guard);
                    return -1;
                }
            }
        } else {
            0
        };

        desc.inode = handle.inode;
        desc.fs = Some(handle.fs);
        desc.flags = flags;
        desc.position = position;
        desc.valid = true;

        drop(guard);
        slot_idx as c_int
    })
}

pub fn file_read_fd(process_id: u32, fd: c_int, buffer: *mut c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }

    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };
        if (desc.flags & FILE_OPEN_READ) == 0 {
            drop(guard);
            return -1;
        }

        // Console descriptors: stdin returns 0 (no data available).
        // Interactive console input is handled by SYSCALL_READ / SYSCALL_READ_CHAR.
        if desc.console {
            drop(guard);
            return 0;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        let buf = unsafe { slice::from_raw_parts_mut(buffer as *mut u8, count) };
        let rc = fs.read(desc.inode, desc.position as u64, buf);
        if let Ok(read_len) = rc {
            desc.position = desc.position.saturating_add(read_len);
            drop(guard);
            return read_len as ssize_t;
        }
        drop(guard);
        -1
    })
}

pub fn file_write_fd(process_id: u32, fd: c_int, buffer: *const c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };
        if (desc.flags & FILE_OPEN_WRITE) == 0 {
            drop(guard);
            return -1;
        }

        // Console descriptors: route stdout/stderr writes to serial port.
        if desc.console {
            drop(guard);
            let bytes = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
            // SAFETY: COM1 is always valid on x86_64 QEMU targets.
            unsafe {
                slopos_lib::ports::serial_write_bytes(slopos_lib::ports::COM1, bytes);
            }
            return count as ssize_t;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        let buf = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
        let rc = fs.write(desc.inode, desc.position as u64, buf);
        if let Ok(written) = rc {
            desc.position = desc.position.saturating_add(written);
            drop(guard);
            return written as ssize_t;
        }
        drop(guard);
        -1
    })
}

pub fn file_close_fd(process_id: u32, fd: c_int) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };
        reset_descriptor(desc);
        drop(guard);
        0
    })
}

/// POSIX lseek: reposition file offset.
///
/// Returns the new offset on success, or -1 on error (ESPIPE for console FDs).
/// The offset parameter is signed to support negative seeks with SEEK_CUR/SEEK_END.
pub fn file_seek_fd(process_id: u32, fd: c_int, offset: i64, whence: u32) -> i64 {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };

        // Console descriptors are not seekable (POSIX ESPIPE).
        if desc.console {
            drop(guard);
            return -1;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        let size = match fs.stat(desc.inode) {
            Ok(stat) => stat.size as i64,
            Err(_) => {
                drop(guard);
                return -1;
            }
        };

        let new_pos = match whence as u64 {
            SEEK_SET => offset,
            SEEK_CUR => (desc.position as i64).saturating_add(offset),
            SEEK_END => size.saturating_add(offset),
            _ => {
                drop(guard);
                return -1;
            }
        };

        if new_pos < 0 {
            drop(guard);
            return -1;
        }

        desc.position = new_pos as usize;
        drop(guard);
        new_pos
    })
}

pub fn file_get_size_fd(process_id: u32, fd: c_int) -> usize {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return usize::MAX;
        };
        if !table.in_use {
            return usize::MAX;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let desc = unsafe { get_descriptor(&mut *table_ptr, fd) };
        let size = if let Some(desc) = desc {
            if let Some(fs) = desc.fs {
                match fs.stat(desc.inode) {
                    Ok(stat) => stat.size as usize,
                    Err(_) => usize::MAX,
                }
            } else {
                usize::MAX
            }
        } else {
            usize::MAX
        };
        drop(guard);
        size
    })
}

pub fn file_exists_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return 0,
    };
    let rc = vfs_stat(path_bytes);
    if let Ok((kind, _)) = rc {
        return if kind == FS_TYPE_FILE { 1 } else { 0 };
    }
    0
}

pub fn file_unlink_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if vfs_unlink(path_bytes).is_ok() {
        0
    } else {
        -1
    }
}

pub fn file_mkdir_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if vfs_mkdir(path_bytes).is_ok() { 0 } else { -1 }
}

pub fn file_stat_path(path: *const c_char, out_type: &mut u8, out_size: &mut u32) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if let Ok((kind, size)) = vfs_stat(path_bytes) {
        *out_type = kind;
        *out_size = size;
        return 0;
    }
    -1
}

pub fn file_list_path(
    path: *const c_char,
    entries: *mut UserFsEntry,
    max: u32,
    out_count: &mut u32,
) -> c_int {
    if path.is_null() || entries.is_null() || max == 0 {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    let cap = max as usize;
    let out_slice = unsafe { slice::from_raw_parts_mut(entries, cap) };
    match vfs_list(path_bytes, out_slice) {
        Ok(count) => {
            *out_count = count as u32;
            0
        }
        Err(_) => -1,
    }
}

// =============================================================================
// POSIX FD operations: dup, dup2, dup3, fcntl, fstat
// =============================================================================

/// Duplicate a file descriptor to the lowest available fd.
/// Returns the new fd on success, -1 on error.
pub fn file_dup_fd(process_id: u32, old_fd: c_int) -> c_int {
    file_dup_fd_min(process_id, old_fd, 0)
}

/// Duplicate a file descriptor to the lowest available fd >= min_fd.
/// Used by both dup() (min_fd=0) and fcntl F_DUPFD.
fn file_dup_fd_min(process_id: u32, old_fd: c_int, min_fd: usize) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let copy = *src;

        let table = unsafe { &mut *table_ptr };
        let Some(new_idx) = find_free_slot_from(table, min_fd) else {
            drop(guard);
            return -1;
        };

        table.descriptors[new_idx] = copy;
        // dup() clears FD_CLOEXEC on the new descriptor
        table.descriptors[new_idx].cloexec = false;
        drop(guard);
        new_idx as c_int
    })
}

/// Duplicate old_fd to exactly new_fd. If new_fd is already open it is closed first.
/// If old_fd == new_fd, return new_fd without closing.
/// Returns new_fd on success, -1 on error.
pub fn file_dup2_fd(process_id: u32, old_fd: c_int, new_fd: c_int) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return -1;
    }
    if old_fd == new_fd {
        // Verify old_fd is valid, return new_fd if so
        return with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let valid = unsafe { get_descriptor(&mut *table_ptr, old_fd) }.is_some();
            drop(guard);
            if valid { new_fd } else { -1 }
        });
    }

    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let copy = *src;

        let table = unsafe { &mut *table_ptr };
        // Silently close new_fd if it was open
        if table.descriptors[new_fd as usize].valid {
            reset_descriptor(&mut table.descriptors[new_fd as usize]);
        }
        table.descriptors[new_fd as usize] = copy;
        // dup2 clears FD_CLOEXEC on the new descriptor
        table.descriptors[new_fd as usize].cloexec = false;
        drop(guard);
        new_fd
    })
}

/// Duplicate old_fd to exactly new_fd with flags.
/// Unlike dup2, dup3 fails if old_fd == new_fd.
/// The only supported flag is O_CLOEXEC (mapped to bit 0 of flags).
/// Returns new_fd on success, -1 on error.
pub fn file_dup3_fd(process_id: u32, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return -1;
    }
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return -1;
    }

    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let copy = *src;

        let table = unsafe { &mut *table_ptr };
        if table.descriptors[new_fd as usize].valid {
            reset_descriptor(&mut table.descriptors[new_fd as usize]);
        }
        table.descriptors[new_fd as usize] = copy;
        // dup3 sets cloexec based on flags
        table.descriptors[new_fd as usize].cloexec = (flags & FD_CLOEXEC as u32) != 0;
        drop(guard);
        new_fd
    })
}

/// Minimal fcntl implementation.
///
/// Supported commands:
/// - F_DUPFD: duplicate fd to lowest available >= arg
/// - F_GETFD: get FD_CLOEXEC flag
/// - F_SETFD: set FD_CLOEXEC flag
/// - F_GETFL: get file status flags (open mode)
/// - F_SETFL: set file status flags (currently only APPEND)
///
/// Returns command-specific value on success, -1 on error.
pub fn file_fcntl_fd(process_id: u32, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(process_id, fd, arg as usize) as i64,
        F_GETFD => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let val = if desc.cloexec { FD_CLOEXEC as i64 } else { 0 };
            drop(guard);
            val
        }),
        F_SETFD => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            desc.cloexec = (arg & FD_CLOEXEC) != 0;
            drop(guard);
            0
        }),
        F_GETFL => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let val = desc.flags as i64;
            drop(guard);
            val
        }),
        F_SETFL => {
            with_tables(|kernel, processes| {
                let Some(table) = table_for_pid(kernel, processes, process_id) else {
                    return -1i64;
                };
                if !table.in_use {
                    return -1;
                }
                let table_ptr: *mut FileTableSlot = table;
                let guard = unsafe { (&(*table_ptr).lock).lock() };
                let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                    drop(guard);
                    return -1;
                };
                // Only allow modifying APPEND; preserve read/write mode bits
                let mode_bits = desc.flags & (FILE_OPEN_READ | FILE_OPEN_WRITE);
                desc.flags = mode_bits | (arg as u32 & FILE_OPEN_APPEND);
                drop(guard);
                0
            })
        }
        _ => -1,
    }
}

/// Close all file descriptors with FD_CLOEXEC set for a process.
///
/// Called during exec() to satisfy the POSIX close-on-exec contract.
/// Console FDs (0/1/2) are never marked cloexec by default, so they
/// survive exec transitions automatically.
pub fn fileio_close_on_exec(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let table = unsafe { &mut *table_ptr };
        for desc in table.descriptors.iter_mut() {
            if desc.valid && desc.cloexec {
                reset_descriptor(desc);
            }
        }
        drop(guard);
    });
}

/// Stat an open file descriptor.
/// Returns 0 on success and fills out_stat, -1 on error.
pub fn file_fstat_fd(process_id: u32, fd: c_int, out_stat: &mut UserFsStat) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };

        // Console descriptors report as character devices with size 0.
        if desc.console {
            out_stat.type_ = slopos_abi::fs::FS_TYPE_CHARDEV;
            out_stat.size = 0;
            drop(guard);
            return 0;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        match fs.stat(desc.inode) {
            Ok(stat) => {
                out_stat.type_ = stat.file_type as u8;
                out_stat.size = stat.size as u32;
                drop(guard);
                0
            }
            Err(_) => {
                drop(guard);
                -1
            }
        }
    })
}
