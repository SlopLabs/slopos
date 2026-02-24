//! Syscall number definitions (kernel-userland ABI).
//!
//! This module is the **single source of truth** for all syscall numbers.
//! Both kernel and userland import from here to ensure ABI consistency.
//!
//! # Adding New Syscalls
//!
//! 1. Add the constant here with the next available number
//! 2. Use the `SYSCALL_` prefix for consistency
//! 3. Group with related syscalls under the appropriate section
//! 4. Update the dispatch table in `core/src/syscall/handlers.rs`
//!
//! # Number Allocation
//!
//! Numbers are not required to be contiguous. Gaps exist from removed or
//! reserved syscalls. New syscalls should use the next highest number
//! to avoid ABI breakage with existing userland binaries.

// =============================================================================
// Core syscalls
// =============================================================================

pub const SYSCALL_YIELD: u64 = 0;
pub const SYSCALL_EXIT: u64 = 1;
pub const SYSCALL_WRITE: u64 = 2;
pub const SYSCALL_READ: u64 = 3;
pub const SYSCALL_ROULETTE: u64 = 4;
pub const SYSCALL_SLEEP_MS: u64 = 5;
pub const SYSCALL_FB_INFO: u64 = 6;

// =============================================================================
// Random / Roulette
// =============================================================================

pub const SYSCALL_RANDOM_NEXT: u64 = 12;
pub const SYSCALL_ROULETTE_RESULT: u64 = 13;
pub const SYSCALL_ROULETTE_DRAW: u64 = 24;

// =============================================================================
// Filesystem
// =============================================================================

pub const SYSCALL_FS_OPEN: u64 = 14;
pub const SYSCALL_FS_CLOSE: u64 = 15;
pub const SYSCALL_FS_READ: u64 = 16;
pub const SYSCALL_FS_WRITE: u64 = 17;
pub const SYSCALL_FS_STAT: u64 = 18;
pub const SYSCALL_FS_MKDIR: u64 = 19;
pub const SYSCALL_FS_UNLINK: u64 = 20;
pub const SYSCALL_FS_LIST: u64 = 21;

// =============================================================================
// System
// =============================================================================

pub const SYSCALL_SYS_INFO: u64 = 22;
pub const SYSCALL_HALT: u64 = 23;
pub const SYSCALL_READ_CHAR: u64 = 25;
pub const SYSCALL_READ_CHAR_NB: u64 = 119;
pub const SYSCALL_NET_SCAN: u64 = 120;
pub const SYSCALL_TTY_SET_FOCUS: u64 = 28;
pub const SYSCALL_GET_TIME_MS: u64 = 39;
pub const SYSCALL_REBOOT: u64 = 85;

// =============================================================================
// Window management
// =============================================================================

pub const SYSCALL_ENUMERATE_WINDOWS: u64 = 30;
pub const SYSCALL_SET_WINDOW_POSITION: u64 = 31;
pub const SYSCALL_SET_WINDOW_STATE: u64 = 32;
pub const SYSCALL_RAISE_WINDOW: u64 = 33;
pub const SYSCALL_SET_CURSOR_SHAPE: u64 = 118;

// =============================================================================
// Input events
// =============================================================================

pub const SYSCALL_INPUT_POLL_BATCH: u64 = 34;
pub const SYSCALL_INPUT_POLL: u64 = 60;
pub const SYSCALL_INPUT_HAS_EVENTS: u64 = 61;
pub const SYSCALL_INPUT_SET_FOCUS: u64 = 62;
pub const SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET: u64 = 65;
pub const SYSCALL_INPUT_GET_POINTER_POS: u64 = 66;
pub const SYSCALL_INPUT_GET_BUTTON_STATE: u64 = 67;
pub const SYSCALL_INPUT_REQUEST_CLOSE: u64 = 84;
pub const SYSCALL_CLIPBOARD_COPY: u64 = 116;
pub const SYSCALL_CLIPBOARD_PASTE: u64 = 117;

// =============================================================================
// Surface / Compositor
// =============================================================================

pub const SYSCALL_SURFACE_COMMIT: u64 = 38;
pub const SYSCALL_SURFACE_ATTACH: u64 = 44;
pub const SYSCALL_SURFACE_FRAME: u64 = 50;
pub const SYSCALL_POLL_FRAME_DONE: u64 = 51;
pub const SYSCALL_MARK_FRAMES_DONE: u64 = 52;
pub const SYSCALL_SURFACE_DAMAGE: u64 = 55;
pub const SYSCALL_BUFFER_AGE: u64 = 56;
pub const SYSCALL_SURFACE_SET_ROLE: u64 = 57;
pub const SYSCALL_SURFACE_SET_PARENT: u64 = 58;
pub const SYSCALL_SURFACE_SET_REL_POS: u64 = 59;
pub const SYSCALL_SURFACE_SET_TITLE: u64 = 63;

// =============================================================================
// Shared memory
// =============================================================================

pub const SYSCALL_SHM_CREATE: u64 = 40;
pub const SYSCALL_SHM_MAP: u64 = 41;
pub const SYSCALL_SHM_UNMAP: u64 = 42;
pub const SYSCALL_SHM_DESTROY: u64 = 43;
pub const SYSCALL_FB_FLIP: u64 = 45;
pub const SYSCALL_DRAIN_QUEUE: u64 = 46;
pub const SYSCALL_SHM_ACQUIRE: u64 = 47;
pub const SYSCALL_SHM_RELEASE: u64 = 48;
pub const SYSCALL_SHM_POLL_RELEASED: u64 = 49;
pub const SYSCALL_SHM_GET_FORMATS: u64 = 53;
pub const SYSCALL_SHM_CREATE_WITH_FORMAT: u64 = 54;

// =============================================================================
// Task management
// =============================================================================

/// Spawn a new userspace task by absolute executable path.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to path bytes (NUL-terminated or explicit length)
/// * rsi (arg1): path length in bytes
/// * rdx (arg2): task priority (`u8`)
/// * r10 (arg3): task flags (`u16`, kernel enforces user-mode bit)
///
/// # Returns
/// * positive task ID on success
/// * negative `ExecError` code on failure
pub const SYSCALL_SPAWN_PATH: u64 = 64;
pub const SYSCALL_WAITPID: u64 = 68;
pub const SYSCALL_TERMINATE_TASK: u64 = 69;

// =============================================================================
// Process execution
// =============================================================================

/// Execute an ELF binary from the filesystem, replacing the current process.
///
/// # Arguments (via registers)
/// * rdi (arg0): Pointer to null-terminated path string
/// * rsi (arg1): Reserved for future argv support (must be zero)
/// * rdx (arg2): Reserved for future envp support (must be zero)
///
/// # Returns
/// * Does not return on success (process image is replaced)
/// * -ENOENT: File not found
/// * -ENOEXEC: Not a valid ELF executable
/// * -ENOMEM: Insufficient memory
/// * -EFAULT: Invalid pointer
pub const SYSCALL_EXEC: u64 = 70;

// =============================================================================
// Memory management
// =============================================================================

pub const SYSCALL_BRK: u64 = 71;

// =============================================================================
// Process management
// =============================================================================

/// Fork the current process, creating a child with copy-on-write address space.
///
/// # Returns
/// * In parent: child's task ID (positive)
/// * In child: 0
/// * On error: negative error code
pub const SYSCALL_FORK: u64 = 72;

// =============================================================================
// SMP / CPU Affinity
// =============================================================================

pub const SYSCALL_GET_CPU_COUNT: u64 = 80;
pub const SYSCALL_GET_CURRENT_CPU: u64 = 81;
pub const SYSCALL_SET_CPU_AFFINITY: u64 = 82;
pub const SYSCALL_GET_CPU_AFFINITY: u64 = 83;

// =============================================================================
// Process identity
// =============================================================================

pub const SYSCALL_GETPID: u64 = 86;
pub const SYSCALL_GETPPID: u64 = 87;
pub const SYSCALL_GETUID: u64 = 88;
pub const SYSCALL_GETGID: u64 = 89;
pub const SYSCALL_GETEUID: u64 = 90;
pub const SYSCALL_GETEGID: u64 = 91;

// =============================================================================
// Memory management (POSIX)
// =============================================================================

/// Map anonymous memory into the process address space.
///
/// # Arguments (via registers)
/// * rdi (arg0): requested address (hint, or 0 for kernel-chosen)
/// * rsi (arg1): length in bytes (must be > 0, rounded up to page size)
/// * rdx (arg2): protection flags (PROT_READ | PROT_WRITE | PROT_EXEC)
/// * r10 (arg3): mapping flags (MAP_ANONYMOUS | MAP_PRIVATE | MAP_FIXED)
/// * r8  (arg4): file descriptor (must be -1 for MAP_ANONYMOUS)
/// * r9  (arg5): offset (must be 0 for MAP_ANONYMOUS)
///
/// # Returns
/// * Virtual address of the mapping on success
/// * Negative errno on failure (-EINVAL, -ENOMEM)
pub const SYSCALL_MMAP: u64 = 92;

/// Unmap a previously mapped memory region.
///
/// # Arguments (via registers)
/// * rdi (arg0): start address (must be page-aligned)
/// * rsi (arg1): length in bytes (rounded up to page size)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL)
pub const SYSCALL_MUNMAP: u64 = 93;

/// Change protection on a memory region.
///
/// # Arguments (via registers)
/// * rdi (arg0): start address (must be page-aligned)
/// * rsi (arg1): length in bytes (rounded up to page size)
/// * rdx (arg2): new protection flags (PROT_READ | PROT_WRITE | PROT_EXEC)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -ENOMEM)
pub const SYSCALL_MPROTECT: u64 = 94;

// =============================================================================
// File descriptor operations
// =============================================================================

pub const SYSCALL_DUP: u64 = 95;
pub const SYSCALL_DUP2: u64 = 96;
pub const SYSCALL_DUP3: u64 = 97;
pub const SYSCALL_FCNTL: u64 = 98;
pub const SYSCALL_LSEEK: u64 = 99;
pub const SYSCALL_FSTAT: u64 = 100;
pub const SYSCALL_POLL: u64 = 108;
pub const SYSCALL_SELECT: u64 = 109;
pub const SYSCALL_PIPE: u64 = 110;
pub const SYSCALL_PIPE2: u64 = 111;
pub const SYSCALL_IOCTL: u64 = 112;
pub const SYSCALL_SETPGID: u64 = 113;
pub const SYSCALL_GETPGID: u64 = 114;
pub const SYSCALL_SETSID: u64 = 115;

// =============================================================================
// mmap constants
// =============================================================================

/// Protection flags for mmap/mprotect
pub const PROT_NONE: u64 = 0;
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;

/// Mapping flags for mmap
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_FIXED: u64 = 0x10;

// =============================================================================
// fcntl constants
// =============================================================================

pub const F_DUPFD: u64 = 0;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_GETFL: u64 = 3;
pub const F_SETFL: u64 = 4;
pub const FD_CLOEXEC: u64 = 1;

pub const O_NONBLOCK: u64 = 0x800;
pub const O_CLOEXEC: u64 = 0x80_000;

// =============================================================================
// lseek whence constants
// =============================================================================

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

pub const POLLIN: u16 = 0x0001;
pub const POLLPRI: u16 = 0x0002;
pub const POLLOUT: u16 = 0x0004;
pub const POLLERR: u16 = 0x0008;
pub const POLLHUP: u16 = 0x0010;
pub const POLLNVAL: u16 = 0x0020;

pub const FDSET_WORD_BITS: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserPollFd {
    pub fd: i32,
    pub events: u16,
    pub revents: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserTimeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TCSETSW: u64 = 0x5403;
pub const TCSETSF: u64 = 0x5404;
pub const TIOCGPGRP: u64 = 0x540F;
pub const TIOCSPGRP: u64 = 0x5410;
pub const TIOCGWINSZ: u64 = 0x5413;
pub const TIOCSWINSZ: u64 = 0x5414;

pub const NCCS: usize = 19;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UserTermios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; NCCS],
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

impl Default for UserTermios {
    fn default() -> Self {
        Self {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; NCCS],
            c_ispeed: 0,
            c_ospeed: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserWinsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

// =============================================================================
// Thread / clone
// =============================================================================

/// Create a new thread or process via clone.
///
/// # Arguments (via registers)
/// * rdi (arg0): clone flags (CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD etc.)
/// * rsi (arg1): child stack pointer (0 = share parent stack, i.e. fork-like)
/// * rdx (arg2): parent_tid pointer (written if CLONE_PARENT_SETTID)
/// * r10 (arg3): child_tid pointer  (written/cleared per CLONE_CHILD_SETTID / CLONE_CHILD_CLEARTID)
/// * r8  (arg4): tls value           (new FS_BASE if CLONE_SETTLS)
///
/// # Returns
/// * child task ID to parent on success
/// * 0 to child on success
/// * Negative errno on failure (-EINVAL, -ENOMEM, -EAGAIN)
pub const SYSCALL_CLONE: u64 = 101;

// =============================================================================
// clone flags — Linux-compatible values
// =============================================================================

/// Child and parent share the same virtual address space.
pub const CLONE_VM: u64 = 0x0000_0100;
/// Child and parent share the same filesystem information (cwd, root).
pub const CLONE_FS: u64 = 0x0000_0200;
/// Child and parent share the same file descriptor table.
pub const CLONE_FILES: u64 = 0x0000_0400;
/// Child and parent share the same signal handler table.
pub const CLONE_SIGHAND: u64 = 0x0000_0800;
/// Write the child's TID into the parent's memory at `parent_tid`.
pub const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
/// Write the child's TID into the child's memory at `child_tid`.
pub const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
/// Clear the child's TID at `child_tid` on exit (for futex-based join).
pub const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
/// Set the TLS (FS_BASE) for the new thread.
pub const CLONE_SETTLS: u64 = 0x0008_0000;
/// New thread shares the parent's thread group (POSIX thread semantics).
pub const CLONE_THREAD: u64 = 0x0001_0000;

/// Mask of all clone flags that SlopOS currently recognises.
pub const CLONE_SUPPORTED_MASK: u64 = CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_SETTLS
    | CLONE_THREAD;

// =============================================================================
// Signals
// =============================================================================

/// Install or query a signal handler for a given signal.
///
/// # Arguments (via registers)
/// * rdi (arg0): signal number (1-31)
/// * rsi (arg1): pointer to new `UserSigaction` (or 0 to query only)
/// * rdx (arg2): pointer to old `UserSigaction` output (or 0 to skip)
/// * r10 (arg3): size of signal set (must be 8)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -EFAULT)
pub const SYSCALL_RT_SIGACTION: u64 = 102;

/// Examine and change blocked signal mask.
///
/// # Arguments (via registers)
/// * rdi (arg0): how (SIG_BLOCK=0, SIG_UNBLOCK=1, SIG_SETMASK=2)
/// * rsi (arg1): pointer to new signal set (or 0 to query only)
/// * rdx (arg2): pointer to old signal set output (or 0 to skip)
/// * r10 (arg3): size of signal set (must be 8)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -EFAULT)
pub const SYSCALL_RT_SIGPROCMASK: u64 = 103;

/// Send a signal to a process or task.
///
/// # Arguments (via registers)
/// * rdi (arg0): target task ID (or 0 for self)
/// * rsi (arg1): signal number (1-31, or 0 to check task existence)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -ESRCH, -EPERM)
pub const SYSCALL_KILL: u64 = 104;

/// Restore execution state after a signal handler completes.
///
/// # Arguments
/// * The signal frame is on the user stack (set up by the kernel during
///   signal delivery). No explicit register arguments needed.
///
/// # Returns
/// * Does not return to caller -- restores saved execution context.
pub const SYSCALL_RT_SIGRETURN: u64 = 105;

// =============================================================================
// Futex
// =============================================================================

/// Futex system call -- fast userspace locking primitive.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to the futex word (u32, must be 4-byte aligned)
/// * rsi (arg1): futex operation (FUTEX_WAIT, FUTEX_WAKE)
/// * rdx (arg2): value (expected value for WAIT, max waiters for WAKE)
/// * r10 (arg3): timeout in milliseconds (0 = no timeout; only for FUTEX_WAIT)
///
/// # Returns
/// * FUTEX_WAIT: 0 on success, -EAGAIN if value mismatch, -ETIMEDOUT on timeout
/// * FUTEX_WAKE: number of waiters woken
/// * -ENOSYS for unsupported operations
/// * -EINVAL for bad arguments
pub const SYSCALL_FUTEX: u64 = 106;

/// Futex operations
pub const FUTEX_WAIT: u64 = 0;
pub const FUTEX_WAKE: u64 = 1;

// =============================================================================
// TLS / arch_prctl
// =============================================================================

/// Set or get architecture-specific thread state (TLS base).
///
/// # Arguments (via registers)
/// * rdi (arg0): sub-command (ARCH_SET_FS, ARCH_GET_FS)
/// * rsi (arg1): for SET_FS: new FS_BASE value; for GET_FS: pointer to u64 output
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -EFAULT)
pub const SYSCALL_ARCH_PRCTL: u64 = 107;

/// arch_prctl sub-commands (Linux-compatible values)
pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;

// =============================================================================
// Errno constants (Linux-compatible negative values)
// =============================================================================

pub const ERRNO_EINVAL: u64 = (-22i64) as u64;
pub const ERRNO_ENOMEM: u64 = (-12i64) as u64;
pub const ERRNO_EAGAIN: u64 = (-11i64) as u64;
pub const ERRNO_ESRCH: u64 = (-3i64) as u64;
pub const ERRNO_EFAULT: u64 = (-14i64) as u64;
pub const ERRNO_ETIMEDOUT: u64 = (-110i64) as u64;

// =============================================================================
// Syscall ABI stability
// =============================================================================

/// Total size of the dispatch table. All syscall numbers must be below this.
pub const SYSCALL_TABLE_SIZE: usize = 128;

/// Standard return value for unimplemented syscalls: -ENOSYS (negated errno 38).
pub const ENOSYS_RETURN: u64 = (-38i64) as u64;

// =============================================================================
// Syscall data structures
// =============================================================================

/// System information returned by SYSCALL_SYS_INFO
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSysInfo {
    pub total_pages: u32,
    pub free_pages: u32,
    pub allocated_pages: u32,
    pub total_tasks: u32,
    pub active_tasks: u32,
    pub task_context_switches: u64,
    pub scheduler_context_switches: u64,
    pub scheduler_yields: u64,
    pub ready_tasks: u32,
    pub schedule_calls: u32,
    pub wl_balance: i64,
}
