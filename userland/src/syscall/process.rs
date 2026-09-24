//! Process management syscalls: spawn, exec, fork, halt, reboot.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall5};
use slopos_abi::signal::{
    SIG_DFL, SIG_IGN, SIGKILL, SigSet, UserSigaction, WAIT_STATUS_CONTINUED, WNOHANG,
};
use slopos_abi::spawn::{SpawnAttrs, SpawnFdAction, SpawnFdActionKind};
use slopos_abi::task::TaskPriority;

/// Signal restorer trampoline — called when a signal handler returns.
///
/// The restorer address sits in its own stack word ahead of the `SignalFrame`,
/// so once the handler's `ret` pops it RSP already points at the frame and
/// `rt_sigreturn` needs no stack adjustment.
#[unsafe(naked)]
extern "C" fn signal_restorer() {
    core::arch::naked_asm!(
        "mov eax, {sigreturn}",
        "syscall",
        "ud2",
        sigreturn = const SYSCALL_RT_SIGRETURN,
    );
}
use slopos_slibc::pal::{Pal, Sys};

#[inline(always)]
pub fn getpid() -> u32 {
    Sys::getpid() as u32
}

#[inline(always)]
pub fn getuid() -> u32 {
    Sys::getuid()
}

#[inline(always)]
pub fn chdir(path: *const u8) -> i64 {
    unsafe { syscall1(SYSCALL_CHDIR, path as u64) as i64 }
}

#[inline(always)]
pub fn getcwd(buf: &mut [u8]) -> i64 {
    unsafe { syscall2(SYSCALL_GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64) as i64 }
}

/// Shares the caller's `src` fd into the child's `target`.
#[inline(always)]
pub fn clone_fd(src: i32, target: i32) -> SpawnFdAction {
    SpawnFdAction {
        kind: SpawnFdActionKind::CloneFd as u32,
        src_fd: src,
        target_fd: target,
        _pad: 0,
        open_path_ptr: 0,
        open_path_len: 0,
        open_flags: 0,
        _pad2: 0,
    }
}

/// Spawn `path` with an explicit fd-action allow-list. The child starts with
/// an empty fd table; `actions` install exactly the descriptors it inherits.
/// `sigdefault_mask` forces those signals to their default disposition.
#[inline(always)]
pub fn spawn_path_with_actions(
    path: &[u8],
    argv: &[*const u8],
    priority: TaskPriority,
    flags: u16,
    actions: &[SpawnFdAction],
    sigdefault_mask: SigSet,
) -> i32 {
    spawn_path_with_env(path, argv, &[], priority, flags, actions, sigdefault_mask)
}

/// An empty `cwd` inherits the caller's.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn spawn_path_in(
    path: &[u8],
    argv: &[*const u8],
    envp: &[*const u8],
    cwd: &[u8],
    priority: TaskPriority,
    flags: u16,
    actions: &[SpawnFdAction],
    sigdefault_mask: SigSet,
) -> i32 {
    let attrs = SpawnAttrs {
        priority: priority.as_u8(),
        _pad: [0; 3],
        flags,
        _pad2: 0,
        actions_ptr: actions.as_ptr() as u64,
        actions_len: actions.len() as u64,
        sigdefault_mask,
        envp_ptr: if envp.is_empty() {
            0
        } else {
            envp.as_ptr() as u64
        },
        envp_len: envp.len() as u64,
        cwd_ptr: if cwd.is_empty() {
            0
        } else {
            cwd.as_ptr() as u64
        },
        cwd_len: cwd.len() as u64,
    };
    unsafe {
        syscall5(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            argv.as_ptr() as u64,
            argv.len() as u64,
            &attrs as *const SpawnAttrs as u64,
        ) as i32
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn spawn_path_with_env(
    path: &[u8],
    argv: &[*const u8],
    envp: &[*const u8],
    priority: TaskPriority,
    flags: u16,
    actions: &[SpawnFdAction],
    sigdefault_mask: SigSet,
) -> i32 {
    spawn_path_in(
        path,
        argv,
        envp,
        &[],
        priority,
        flags,
        actions,
        sigdefault_mask,
    )
}

/// Clones the caller's stdio (fd 0/1/2) into the child, which is what
/// preserves console inheritance for service and app spawns.
#[inline(always)]
pub fn spawn_path(path: impl AsRef<[u8]>) -> i32 {
    spawn_path_with_attrs(path, TaskPriority::Normal, 0)
}

#[inline(always)]
pub fn spawn_path_with_attrs(path: impl AsRef<[u8]>, priority: TaskPriority, flags: u16) -> i32 {
    let stdio = [clone_fd(0, 0), clone_fd(1, 1), clone_fd(2, 2)];
    spawn_path_with_actions(path.as_ref(), &[], priority, flags, &stdio, 0)
}

#[inline(always)]
pub fn sigdefault(mask: SigSet) -> i64 {
    unsafe { syscall1(SYSCALL_SIGDEFAULT, mask) as i64 }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitStatus {
    Exited(i32),
    Signalled(u8),
    Stopped(u8),
    Continued,
}

impl WaitStatus {
    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            Self::Signalled(signum) => Some(128 + signum as i32),
            Self::Stopped(_) | Self::Continued => None,
        }
    }

    pub fn terminated(self) -> bool {
        matches!(self, Self::Exited(_) | Self::Signalled(_))
    }
}

/// `Continued` is `0xffff`, which shares its low seven bits with a signalled
/// status, so it must be tested first.
pub fn wait_status(status: i32) -> WaitStatus {
    let raw = status as u32;
    if raw == WAIT_STATUS_CONTINUED {
        WaitStatus::Continued
    } else if raw & 0xff == 0x7f {
        WaitStatus::Stopped(((raw >> 8) & 0xff) as u8)
    } else if raw & 0x7f == 0 {
        WaitStatus::Exited(((raw >> 8) & 0xff) as i32)
    } else {
        WaitStatus::Signalled((raw & 0x7f) as u8)
    }
}

#[inline(always)]
pub fn waitpid_raw(pid: i32, status: &mut i32, options: u32) -> i64 {
    unsafe {
        syscall4(
            SYSCALL_WAIT4,
            pid as i64 as u64,
            status as *mut i32 as u64,
            options as u64,
            0,
        ) as i64
    }
}

/// `pid` is `-1` for any child; decode the status with [`wait_status`].
#[inline(always)]
pub fn wait_with(pid: i32, options: u32) -> Option<(u32, i32)> {
    let mut status = 0i32;
    let rc = waitpid_raw(pid, &mut status, options);
    if rc <= 0 {
        None
    } else {
        Some((rc as u32, status))
    }
}

#[inline(always)]
pub fn waitpid(tid: u32) -> Option<(u32, i32)> {
    wait_with(tid as i32, 0)
}

/// Answers `-1` when `tid` cannot be reaped at all.
#[inline(always)]
pub fn wait_exit_code(tid: u32) -> i32 {
    let Some((_, status)) = waitpid(tid) else {
        return -1;
    };
    wait_status(status).exit_code().unwrap_or(-1)
}

/// Path the PTY multiplexor lives at; opening it allocates a master.
const PTMX_PATH: &[u8] = b"/dev/ptmx\0";

/// Returns the master as an owned fd plus the slave pts number; open the
/// slave via `/dev/pts/N` or `TIOCGPTPEER`. `Err` carries a negated errno.
///
/// A freshly allocated slave is locked and answers `EIO` until `TIOCSPTLCK`
/// clears it — Linux's `grantpt`, which the retired `openpty` syscall used to
/// do on the caller's behalf.
pub fn openpty() -> Result<(super::OwnedFd, u32), i64> {
    let master = Sys::open(PTMX_PATH.as_ptr(), slopos_abi::fs::O_RDWR as i32, 0)
        .map_err(|e| -(e.raw() as i64))?;
    let mut unlock: i32 = 0;
    if let Err(e) = Sys::ioctl(master, TIOCSPTLCK, (&mut unlock as *mut i32) as u64) {
        let _ = Sys::close(master);
        return Err(-(e.raw() as i64));
    }
    let mut slave_num: u32 = 0;
    if let Err(e) = Sys::ioctl(master, TIOCGPTN, (&mut slave_num as *mut u32) as u64) {
        let _ = Sys::close(master);
        return Err(-(e.raw() as i64));
    }
    // SAFETY: master is a valid fd just installed by the kernel.
    Ok((unsafe { super::OwnedFd::from_raw(master) }, slave_num))
}

#[inline(always)]
pub fn waitpid_nohang(tid: u32) -> Option<(u32, i32)> {
    wait_with(tid as i32, WNOHANG)
}

#[inline(always)]
pub fn wait_exit_code_nohang(tid: u32) -> Option<i32> {
    let (_, status) = waitpid_nohang(tid)?;
    wait_status(status).exit_code()
}

/// Reap one already-exited child, whichever it is, without blocking. `None`
/// covers both no child having exited and the caller having none.
#[inline(always)]
pub fn wait_any_nohang() -> Option<(u32, i32)> {
    wait_with(-1, WNOHANG)
}

#[inline(always)]
pub fn reap_exited_children() -> usize {
    let mut reaped = 0usize;
    while wait_any_nohang().is_some() {
        reaped += 1;
    }
    reaped
}

/// Kill `task_id` outright: `SIGKILL` is the only disposition a task cannot
/// catch, block or ignore.
#[inline(always)]
pub fn terminate_task(task_id: u32) -> i32 {
    kill_pid(task_id as i32, SIGKILL)
}

/// Replaces the current image, passing neither argv nor envp.
#[inline(always)]
pub fn exec(path: &[u8]) -> i64 {
    exec_ptr(path.as_ptr())
}

#[inline(always)]
pub fn exec_ptr(path: *const u8) -> i64 {
    execve(path, core::ptr::null(), core::ptr::null())
}

#[inline(always)]
pub fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> i64 {
    unsafe { syscall3(SYSCALL_EXECVE, path as u64, argv as u64, envp as u64) as i64 }
}

#[inline(always)]
pub fn fork() -> i32 {
    unsafe { syscall0(SYSCALL_FORK) as i32 }
}

#[inline(always)]
pub fn setsid() -> i32 {
    unsafe { syscall0(SYSCALL_SETSID) as i32 }
}

#[inline(always)]
pub fn setpgid(pid: u32, pgid: u32) -> i32 {
    unsafe { syscall2(SYSCALL_SETPGID, pid as u64, pgid as u64) as i32 }
}

#[inline(always)]
pub fn getpgid(pid: u32) -> i32 {
    unsafe { syscall1(SYSCALL_GETPGID, pid as u64) as i32 }
}

#[inline(always)]
pub fn kill(pid: u32, signum: u8) -> i32 {
    kill_pid(pid as i32, signum)
}

#[inline(always)]
pub fn kill_pid(pid: i32, signum: u8) -> i32 {
    unsafe { syscall2(SYSCALL_KILL, pid as i64 as u64, signum as u64) as i32 }
}

#[inline(always)]
pub fn ignore_signal(signum: u8) -> i32 {
    let action = UserSigaction {
        sa_handler: SIG_IGN,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };
    unsafe {
        syscall4(
            SYSCALL_RT_SIGACTION,
            signum as u64,
            (&action as *const UserSigaction) as u64,
            0,
            core::mem::size_of::<SigSet>() as u64,
        ) as i32
    }
}

/// Forked children call this before running a command so terminal-generated
/// signals act on the job rather than on the shell's interactive handlers.
#[inline(always)]
pub fn default_signal(signum: u8) -> i32 {
    let action = UserSigaction {
        sa_handler: SIG_DFL,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };
    unsafe {
        syscall4(
            SYSCALL_RT_SIGACTION,
            signum as u64,
            (&action as *const UserSigaction) as u64,
            0,
            core::mem::size_of::<SigSet>() as u64,
        ) as i32
    }
}

/// `SA_RESTART` is deliberately omitted, so blocking syscalls (e.g. `poll`)
/// return early once the handler has run.
#[inline(always)]
pub fn set_signal_handler(signum: u8, handler: extern "C" fn(i32)) -> i32 {
    let action = UserSigaction {
        sa_handler: handler as *const () as u64,
        sa_flags: 0,
        sa_restorer: signal_restorer as *const () as u64,
        sa_mask: 0,
    };
    unsafe {
        syscall4(
            SYSCALL_RT_SIGACTION,
            signum as u64,
            (&action as *const UserSigaction) as u64,
            0,
            core::mem::size_of::<SigSet>() as u64,
        ) as i32
    }
}

#[inline(always)]
pub fn halt() -> ! {
    Sys::halt()
}

#[inline(always)]
pub fn reboot() -> ! {
    Sys::reboot()
}
