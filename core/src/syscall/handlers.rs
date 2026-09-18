use core::ffi::c_char;

use slopos_abi::syscall::*;

use slopos_ostd::authority::Capability;

use crate::syscall::common::SyscallEntry;
pub use crate::syscall::core_handlers::{
    syscall_clock_gettime, syscall_clock_settime, syscall_cpu_info, syscall_ctty_read,
    syscall_exit, syscall_exit_group, syscall_klog_write, syscall_nanosleep, syscall_percpu_stats,
    syscall_process_list, syscall_reboot, syscall_sched_yield, syscall_sys_info, syscall_uname,
};
use crate::syscall::font_handlers::syscall_font_set;
use crate::syscall::fs::{
    syscall_access, syscall_chmod, syscall_close, syscall_dup, syscall_dup2, syscall_dup3,
    syscall_faccessat, syscall_faccessat2, syscall_fchmod, syscall_fchmodat, syscall_fchmodat2,
    syscall_fcntl, syscall_fdatasync, syscall_flock, syscall_fstat, syscall_fstatfs, syscall_fsync,
    syscall_getdents64, syscall_ioctl, syscall_link, syscall_linkat, syscall_lseek, syscall_lstat,
    syscall_mkdir, syscall_mkdirat, syscall_mount, syscall_newfstatat, syscall_open,
    syscall_openat, syscall_pipe, syscall_pipe2, syscall_poll, syscall_pread64, syscall_pwrite64,
    syscall_read, syscall_readlink, syscall_readlinkat, syscall_readv, syscall_rename,
    syscall_renameat, syscall_rmdir, syscall_select, syscall_stat, syscall_statfs, syscall_symlink,
    syscall_symlinkat, syscall_sync, syscall_truncate, syscall_umount2, syscall_unlink,
    syscall_unlinkat, syscall_utimensat, syscall_write, syscall_writev,
};
use crate::syscall::keymap_handlers::{syscall_keymap_get_name, syscall_keymap_load};
pub use crate::syscall::memory_handlers::{
    syscall_brk, syscall_ftruncate, syscall_memfd_create, syscall_mmap, syscall_mprotect,
    syscall_msync, syscall_munmap,
};
use crate::syscall::net_config_handlers::{
    syscall_net_addr_ctl, syscall_net_iface_ctl, syscall_net_monitor, syscall_net_resolver_set,
    syscall_net_route_ctl,
};
use crate::syscall::net_handlers::{
    syscall_accept, syscall_bind, syscall_connect, syscall_getpeername, syscall_getsockname,
    syscall_getsockopt, syscall_listen, syscall_recvfrom, syscall_recvmsg, syscall_resolve,
    syscall_sendmsg, syscall_sendto, syscall_setsockopt, syscall_shutdown, syscall_socket,
};
use crate::syscall::net_query_handlers::syscall_net_query;
pub use crate::syscall::pidfd_handlers::syscall_pidfd_open;
pub use crate::syscall::process_handlers::{
    syscall_arch_prctl, syscall_chdir, syscall_clone, syscall_execve, syscall_fork, syscall_futex,
    syscall_getcpu, syscall_getcwd, syscall_getegid, syscall_geteuid, syscall_getgid,
    syscall_getpgid, syscall_getpid, syscall_getppid, syscall_gettid, syscall_getuid,
    syscall_prlimit64, syscall_sched_getaffinity, syscall_sched_setaffinity, syscall_setpgid,
    syscall_setsid, syscall_sigdefault, syscall_spawn_path, syscall_vhangup, syscall_wait4,
};
pub use crate::syscall::ring_handlers::{
    syscall_ring_enter, syscall_ring_register, syscall_ring_setup,
};
use crate::syscall::signal::{
    syscall_kill, syscall_rt_sigaction, syscall_rt_sigprocmask, syscall_rt_sigreturn,
    syscall_sigaltstack,
};
pub use crate::syscall::signalfd_handlers::syscall_signalfd4;
pub use crate::syscall::test_handlers::{
    syscall_run_userland_tests, syscall_test_panic, syscall_test_report,
};
pub use crate::syscall::ui_handlers::{
    syscall_clipboard_copy, syscall_clipboard_paste, syscall_cursor_move, syscall_cursor_set_image,
    syscall_fb_flip, syscall_fb_info, syscall_getrandom, syscall_input_poll_batch,
    syscall_input_sink_acquire, syscall_roulette_draw, syscall_roulette_result,
    syscall_roulette_spin, syscall_screen_acquire, syscall_set_display_mode,
};

/// Build a dispatch table; unregistered slots stay `Unimplemented`.
///
/// Each arm reads `$handler::DEF` — the constant `define_syscall!` emitted
/// beside the handler — rather than rebuilding an entry here. That is the whole
/// mechanism: the capability reaches this table *through* the handler, so no
/// parallel list exists to drift out of step with the checks, and there is
/// exactly one artifact for the totality assert to read.
macro_rules! syscall_table {
    (size: $size:expr; base: $base:expr; $( [$num:expr] => $handler:ident, $name:literal; )*) => {{
        let mut table: [SyscallEntry; $size] = [SyscallEntry::EMPTY; $size];
        $(
            table[($num - $base) as usize] = SyscallEntry {
                // Both from `DEF`, so the slot cannot name one handler and
                // another's classification.
                handler: $handler::DEF.handler,
                cap: $handler::DEF.cap,
                // The ABI spelling, which the handler's own identifier does
                // not always carry.
                name: ::slopos_ostd::sync::KernelSync::new(
                    concat!($name, "\0").as_ptr() as *const c_char,
                ),
            };
        )*
        table
    }};
}

/// Linux-numbered entry points. A slot here answers the Linux call of that
/// number or nothing at all.
static SYSCALL_TABLE: [SyscallEntry; SYSCALL_TABLE_SIZE] = syscall_table! {
    size: SYSCALL_TABLE_SIZE;
    base: 0;

    [SYSCALL_READ]              => syscall_read,              "read";
    [SYSCALL_WRITE]             => syscall_write,             "write";
    [SYSCALL_OPEN]              => syscall_open,              "open";
    [SYSCALL_CLOSE]             => syscall_close,             "close";
    [SYSCALL_STAT]              => syscall_stat,              "stat";
    [SYSCALL_FSTAT]             => syscall_fstat,             "fstat";
    [SYSCALL_LSTAT]             => syscall_lstat,             "lstat";
    [SYSCALL_POLL]              => syscall_poll,              "poll";
    [SYSCALL_LSEEK]             => syscall_lseek,             "lseek";
    [SYSCALL_MMAP]              => syscall_mmap,              "mmap";
    [SYSCALL_MPROTECT]          => syscall_mprotect,          "mprotect";
    [SYSCALL_MUNMAP]            => syscall_munmap,            "munmap";
    [SYSCALL_BRK]               => syscall_brk,               "brk";
    [SYSCALL_RT_SIGACTION]      => syscall_rt_sigaction,      "rt_sigaction";
    [SYSCALL_RT_SIGPROCMASK]    => syscall_rt_sigprocmask,    "rt_sigprocmask";
    [SYSCALL_RT_SIGRETURN]      => syscall_rt_sigreturn,      "rt_sigreturn";
    [SYSCALL_IOCTL]             => syscall_ioctl,             "ioctl";
    [SYSCALL_PREAD64]           => syscall_pread64,           "pread64";
    [SYSCALL_PWRITE64]          => syscall_pwrite64,          "pwrite64";
    [SYSCALL_READV]             => syscall_readv,             "readv";
    [SYSCALL_WRITEV]            => syscall_writev,            "writev";
    [SYSCALL_ACCESS]            => syscall_access,            "access";
    [SYSCALL_PIPE]              => syscall_pipe,              "pipe";
    [SYSCALL_SELECT]            => syscall_select,            "select";
    [SYSCALL_SCHED_YIELD]       => syscall_sched_yield,       "sched_yield";
    [SYSCALL_MSYNC]             => syscall_msync,             "msync";
    [SYSCALL_DUP]               => syscall_dup,               "dup";
    [SYSCALL_DUP2]              => syscall_dup2,              "dup2";
    [SYSCALL_NANOSLEEP]         => syscall_nanosleep,         "nanosleep";
    [SYSCALL_GETPID]            => syscall_getpid,            "getpid";
    [SYSCALL_SOCKET]            => syscall_socket,            "socket";
    [SYSCALL_CONNECT]           => syscall_connect,           "connect";
    [SYSCALL_ACCEPT]            => syscall_accept,            "accept";
    [SYSCALL_SENDTO]            => syscall_sendto,            "sendto";
    [SYSCALL_RECVFROM]          => syscall_recvfrom,          "recvfrom";
    [SYSCALL_SENDMSG]           => syscall_sendmsg,           "sendmsg";
    [SYSCALL_RECVMSG]           => syscall_recvmsg,           "recvmsg";
    [SYSCALL_SHUTDOWN]          => syscall_shutdown,          "shutdown";
    [SYSCALL_BIND]              => syscall_bind,              "bind";
    [SYSCALL_LISTEN]            => syscall_listen,            "listen";
    [SYSCALL_GETSOCKNAME]       => syscall_getsockname,       "getsockname";
    [SYSCALL_GETPEERNAME]       => syscall_getpeername,       "getpeername";
    [SYSCALL_SETSOCKOPT]        => syscall_setsockopt,        "setsockopt";
    [SYSCALL_GETSOCKOPT]        => syscall_getsockopt,        "getsockopt";
    [SYSCALL_CLONE]             => syscall_clone,             "clone";
    [SYSCALL_FORK]              => syscall_fork,              "fork";
    [SYSCALL_EXECVE]            => syscall_execve,            "execve";
    [SYSCALL_EXIT]              => syscall_exit,              "exit";
    [SYSCALL_WAIT4]             => syscall_wait4,             "wait4";
    [SYSCALL_KILL]              => syscall_kill,              "kill";
    [SYSCALL_UNAME]             => syscall_uname,             "uname";
    [SYSCALL_FCNTL]             => syscall_fcntl,             "fcntl";
    [SYSCALL_FLOCK]             => syscall_flock,             "flock";
    [SYSCALL_FSYNC]             => syscall_fsync,             "fsync";
    [SYSCALL_FDATASYNC]         => syscall_fdatasync,         "fdatasync";
    [SYSCALL_TRUNCATE]          => syscall_truncate,          "truncate";
    [SYSCALL_FTRUNCATE]         => syscall_ftruncate,         "ftruncate";
    [SYSCALL_GETCWD]            => syscall_getcwd,            "getcwd";
    [SYSCALL_CHDIR]             => syscall_chdir,             "chdir";
    [SYSCALL_RENAME]            => syscall_rename,            "rename";
    [SYSCALL_MKDIR]             => syscall_mkdir,             "mkdir";
    [SYSCALL_RMDIR]             => syscall_rmdir,             "rmdir";
    [SYSCALL_LINK]              => syscall_link,              "link";
    [SYSCALL_UNLINK]            => syscall_unlink,            "unlink";
    [SYSCALL_SYMLINK]           => syscall_symlink,           "symlink";
    [SYSCALL_READLINK]          => syscall_readlink,          "readlink";
    [SYSCALL_CHMOD]             => syscall_chmod,             "chmod";
    [SYSCALL_FCHMOD]            => syscall_fchmod,            "fchmod";
    [SYSCALL_GETUID]            => syscall_getuid,            "getuid";
    [SYSCALL_GETGID]            => syscall_getgid,            "getgid";
    [SYSCALL_GETEUID]           => syscall_geteuid,           "geteuid";
    [SYSCALL_GETEGID]           => syscall_getegid,           "getegid";
    [SYSCALL_SETPGID]           => syscall_setpgid,           "setpgid";
    [SYSCALL_GETPPID]           => syscall_getppid,           "getppid";
    [SYSCALL_SETSID]            => syscall_setsid,            "setsid";
    [SYSCALL_GETPGID]           => syscall_getpgid,           "getpgid";
    [SYSCALL_SIGALTSTACK]       => syscall_sigaltstack,       "sigaltstack";
    [SYSCALL_STATFS]            => syscall_statfs,            "statfs";
    [SYSCALL_FSTATFS]           => syscall_fstatfs,           "fstatfs";
    [SYSCALL_VHANGUP]           => syscall_vhangup,           "vhangup";
    [SYSCALL_ARCH_PRCTL]        => syscall_arch_prctl,        "arch_prctl";
    [SYSCALL_SYNC]              => syscall_sync,              "sync";
    [SYSCALL_MOUNT]             => syscall_mount,             "mount";
    [SYSCALL_UMOUNT2]           => syscall_umount2,           "umount2";
    [SYSCALL_REBOOT]            => syscall_reboot,            "reboot";
    [SYSCALL_GETTID]            => syscall_gettid,            "gettid";
    [SYSCALL_FUTEX]             => syscall_futex,             "futex";
    [SYSCALL_SCHED_SETAFFINITY] => syscall_sched_setaffinity, "sched_setaffinity";
    [SYSCALL_SCHED_GETAFFINITY] => syscall_sched_getaffinity, "sched_getaffinity";
    [SYSCALL_GETDENTS64]        => syscall_getdents64,        "getdents64";
    [SYSCALL_CLOCK_SETTIME]     => syscall_clock_settime,     "clock_settime";
    [SYSCALL_CLOCK_GETTIME]     => syscall_clock_gettime,     "clock_gettime";
    [SYSCALL_EXIT_GROUP]        => syscall_exit_group,        "exit_group";
    [SYSCALL_OPENAT]            => syscall_openat,            "openat";
    [SYSCALL_MKDIRAT]           => syscall_mkdirat,           "mkdirat";
    [SYSCALL_NEWFSTATAT]        => syscall_newfstatat,        "newfstatat";
    [SYSCALL_UNLINKAT]          => syscall_unlinkat,          "unlinkat";
    [SYSCALL_RENAMEAT]          => syscall_renameat,          "renameat";
    [SYSCALL_LINKAT]            => syscall_linkat,            "linkat";
    [SYSCALL_SYMLINKAT]         => syscall_symlinkat,         "symlinkat";
    [SYSCALL_READLINKAT]        => syscall_readlinkat,        "readlinkat";
    [SYSCALL_FCHMODAT]          => syscall_fchmodat,          "fchmodat";
    [SYSCALL_FACCESSAT]         => syscall_faccessat,         "faccessat";
    [SYSCALL_UTIMENSAT]         => syscall_utimensat,         "utimensat";
    [SYSCALL_SIGNALFD4]         => syscall_signalfd4,         "signalfd4";
    [SYSCALL_DUP3]              => syscall_dup3,              "dup3";
    [SYSCALL_PIPE2]             => syscall_pipe2,             "pipe2";
    [SYSCALL_PRLIMIT64]         => syscall_prlimit64,         "prlimit64";
    [SYSCALL_GETCPU]            => syscall_getcpu,            "getcpu";
    [SYSCALL_GETRANDOM]         => syscall_getrandom,         "getrandom";
    [SYSCALL_MEMFD_CREATE]      => syscall_memfd_create,      "memfd_create";
    [SYSCALL_PIDFD_OPEN]        => syscall_pidfd_open,        "pidfd_open";
    [SYSCALL_FACCESSAT2]        => syscall_faccessat2,        "faccessat2";
    [SYSCALL_FCHMODAT2]         => syscall_fchmodat2,         "fchmodat2";
};

/// SlopOS-private entry points: operations with no Linux analogue.
static SYSCALL_PRIVATE_TABLE: [SyscallEntry; SYSCALL_PRIVATE_TABLE_SIZE] = syscall_table! {
    size: SYSCALL_PRIVATE_TABLE_SIZE;
    base: SYSCALL_PRIVATE_BASE;

    [SYSCALL_KLOG_WRITE]         => syscall_klog_write,         "klog_write";
    [SYSCALL_CTTY_READ]          => syscall_ctty_read,          "ctty_read";
    [SYSCALL_SYS_INFO]           => syscall_sys_info,           "sys_info";
    [SYSCALL_PROCESS_LIST]       => syscall_process_list,       "process_list";
    [SYSCALL_CPU_INFO]           => syscall_cpu_info,           "cpu_info";
    [SYSCALL_PERCPU_STATS]       => syscall_percpu_stats,       "percpu_stats";
    [SYSCALL_SPAWN_PATH]         => syscall_spawn_path,         "spawn_path";
    [SYSCALL_SIGDEFAULT]         => syscall_sigdefault,         "sigdefault";
    [SYSCALL_RESOLVE]            => syscall_resolve,            "resolve";
    [SYSCALL_NET_QUERY]          => syscall_net_query,          "net_query";
    [SYSCALL_NET_IFACE_CTL]      => syscall_net_iface_ctl,      "net_iface_ctl";
    [SYSCALL_NET_ADDR_CTL]       => syscall_net_addr_ctl,       "net_addr_ctl";
    [SYSCALL_NET_ROUTE_CTL]      => syscall_net_route_ctl,      "net_route_ctl";
    [SYSCALL_NET_RESOLVER_SET]   => syscall_net_resolver_set,   "net_resolver_set";
    [SYSCALL_NET_MONITOR]        => syscall_net_monitor,        "net_monitor";
    [SYSCALL_FB_INFO]            => syscall_fb_info,            "fb_info";
    [SYSCALL_FB_FLIP]            => syscall_fb_flip,            "fb_flip";
    [SYSCALL_CURSOR_SET_IMAGE]   => syscall_cursor_set_image,   "cursor_set_image";
    [SYSCALL_CURSOR_MOVE]        => syscall_cursor_move,        "cursor_move";
    [SYSCALL_SET_DISPLAY_MODE]   => syscall_set_display_mode,   "set_display_mode";
    [SYSCALL_SCREEN_ACQUIRE]     => syscall_screen_acquire,     "screen_acquire";
    [SYSCALL_INPUT_SINK_ACQUIRE] => syscall_input_sink_acquire, "input_sink_acquire";
    [SYSCALL_INPUT_POLL_BATCH]   => syscall_input_poll_batch,   "input_poll_batch";
    [SYSCALL_CLIPBOARD_COPY]     => syscall_clipboard_copy,     "clipboard_copy";
    [SYSCALL_CLIPBOARD_PASTE]    => syscall_clipboard_paste,    "clipboard_paste";
    [SYSCALL_FONT_SET]           => syscall_font_set,           "font_set";
    [SYSCALL_KEYMAP_LOAD]        => syscall_keymap_load,        "keymap_load";
    [SYSCALL_KEYMAP_GET_NAME]    => syscall_keymap_get_name,    "keymap_get_name";
    [SYSCALL_RING_SETUP]         => syscall_ring_setup,         "ring_setup";
    [SYSCALL_RING_ENTER]         => syscall_ring_enter,         "ring_enter";
    [SYSCALL_RING_REGISTER]      => syscall_ring_register,      "ring_register";
    [SYSCALL_ROULETTE]           => syscall_roulette_spin,      "roulette";
    [SYSCALL_ROULETTE_RESULT]    => syscall_roulette_result,    "roulette_result";
    [SYSCALL_ROULETTE_DRAW]      => syscall_roulette_draw,      "roulette_draw";
    [SYSCALL_TEST_REPORT]        => syscall_test_report,        "test_report";
    [SYSCALL_RUN_USERLAND_TESTS] => syscall_run_userland_tests, "run_userland_tests";
    [SYSCALL_TEST_PANIC]         => syscall_test_panic,         "test_panic";
};

/// The entry for `sysno`, or `None` when nothing is registered there.
///
/// The two ranges are separate tables rather than one sparse array: Linux's
/// space is dense enough to index directly, and the private base sits far
/// enough above it that a shared array would be mostly holes.
pub fn syscall_lookup(sysno: u64) -> Option<&'static SyscallEntry> {
    let entry = if sysno >= SYSCALL_PRIVATE_BASE {
        SYSCALL_PRIVATE_TABLE.get((sysno - SYSCALL_PRIVATE_BASE) as usize)?
    } else {
        SYSCALL_TABLE.get(sysno as usize)?
    };
    if entry.handler.is_none() {
        None
    } else {
        Some(entry)
    }
}

// Upstream's catch-all grew because nothing coordinated capability use. These
// asserts are that coordination: breadth is a compile error, not a measurement
// taken a decade late.

/// Entry points classified `cap`, counted over both tables at compile time.
///
/// Unregistered slots are skipped rather than counted: with Linux's numbering
/// the tables are mostly holes, and a hole count that moves whenever an
/// unrelated call is added is a ratchet nobody reads.
const fn count_of(cap: Capability) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < SYSCALL_TABLE_SIZE {
        // `Capability` is a fieldless enum, so a discriminant compare is a
        // `const`-evaluable equality.
        if SYSCALL_TABLE[i].handler.is_some() && SYSCALL_TABLE[i].cap as u8 == cap as u8 {
            n += 1;
        }
        i += 1;
    }
    let mut j = 0;
    while j < SYSCALL_PRIVATE_TABLE_SIZE {
        if SYSCALL_PRIVATE_TABLE[j].handler.is_some()
            && SYSCALL_PRIVATE_TABLE[j].cap as u8 == cap as u8
        {
            n += 1;
        }
        j += 1;
    }
    n
}

/// Registered entry points across both tables.
pub const SYSCALL_ENTRY_COUNT: usize = 151;

/// The recorded shape of the classification.
///
/// Every capability appears, including the ones at zero, so adding an entry
/// point to a capability that had none still moves a number here.
/// `Unimplemented` is the absence of a handler, so its count is 0 by
/// construction and the assert below holds it there.
const CAP_COUNTS: [(Capability, usize); 17] = [
    (Capability::Unimplemented, 0),
    (Capability::NoneSelf, 46),
    (Capability::NoneFd, 71),
    (Capability::NoneRelation, 13),
    (Capability::Power, 1),
    (Capability::Launch, 0),
    (Capability::ProcSignal, 0),
    (Capability::SysInspect, 6),
    (Capability::DisplaySeat, 1),
    (Capability::InputSeat, 1),
    (Capability::ConsoleConfig, 2),
    (Capability::ConsoleIo, 1),
    (Capability::ClipboardGlobal, 2),
    (Capability::Fate, 2),
    (Capability::TestHarness, 2),
    (Capability::Mount, 2),
    (Capability::Clock, 1),
];

const _: () = {
    // Every capability the enum defines is recorded here: a new variant that
    // nobody adds a row for fails this, rather than silently counting zero.
    assert!(
        CAP_COUNTS.len() == Capability::ALL.len(),
        "every Capability needs a row in CAP_COUNTS",
    );

    let mut i = 0;
    let mut total = 0;
    while i < CAP_COUNTS.len() {
        let (cap, recorded) = CAP_COUNTS[i];
        assert!(
            cap as u8 == Capability::ALL[i] as u8,
            "CAP_COUNTS must be in Capability::ALL order",
        );
        let measured = count_of(cap);
        assert!(
            measured == recorded,
            "a capability's entry-point count moved; re-record it in CAP_COUNTS \
             and justify the growth in the commit message",
        );
        total += measured;
        i += 1;
    }

    // Totality: every registered slot is classified, so "did you classify it"
    // is not a question a script has to ask.
    assert!(
        total == SYSCALL_ENTRY_COUNT,
        "the classification must cover every registered entry point",
    );
};

/// Every registered slot carries a handler, and every unregistered one does
/// not. `Unimplemented` is the absence of a handler, not a classification, so
/// the two must agree or the dispatcher's check could run against a slot whose
/// capability nobody chose.
const _: () = {
    let mut i = 0;
    while i < SYSCALL_TABLE_SIZE {
        let entry = SYSCALL_TABLE[i];
        let unimplemented = entry.cap as u8 == Capability::Unimplemented as u8;
        assert!(
            entry.handler.is_none() == unimplemented,
            "a registered slot must not be Unimplemented, and an empty slot must be",
        );
        i += 1;
    }
    let mut j = 0;
    while j < SYSCALL_PRIVATE_TABLE_SIZE {
        let entry = SYSCALL_PRIVATE_TABLE[j];
        let unimplemented = entry.cap as u8 == Capability::Unimplemented as u8;
        assert!(
            entry.handler.is_none() == unimplemented,
            "a registered slot must not be Unimplemented, and an empty slot must be",
        );
        j += 1;
    }
};

/// Per-capability entry-point counts, for the boot-time dump and the tests.
pub fn cap_counts() -> &'static [(Capability, usize); 17] {
    &CAP_COUNTS
}
