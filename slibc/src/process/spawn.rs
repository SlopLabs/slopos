//! `posix_spawn(3)` over the kernel's spawn primitive, which builds the child
//! from an explicit descriptor list: the parent's table is walked here, the
//! file actions applied to that copy, and what the child would hold at `exec`
//! is what it is handed. No address space is duplicated, which is what makes
//! this cheaper than `fork` under commit accounting; attributes the primitive
//! cannot express fall back to fork and exec.

use core::ffi::{c_int, c_short, c_void};
use core::ptr;

use slopos_abi::spawn::{SPAWN_MAX_FD_ACTIONS, SpawnAttrs, SpawnFdAction, SpawnFdActionKind};
use slopos_abi::task::{TASK_FLAG_NEW_PGRP, TaskPriority};

use crate::env::environ;
use crate::errno::{EACCES, EBADF, EINVAL, ENOENT, ENOMEM, ENOTDIR, Errno};
use crate::pal::{Pal, Sys};
use crate::signal::{SIG_DFL, SIG_SETMASK, signal, sigprocmask};
use crate::string::{strdup, u_strlen};
use crate::types::{mode_t, pid_t, posix_spawn_file_actions_t, posix_spawnattr_t, sigset_t};

pub const POSIX_SPAWN_RESETIDS: c_int = 0x01;
pub const POSIX_SPAWN_SETPGROUP: c_int = 0x02;
pub const POSIX_SPAWN_SETSIGDEF: c_int = 0x04;
pub const POSIX_SPAWN_SETSIGMASK: c_int = 0x08;
pub const POSIX_SPAWN_SETSCHEDPARAM: c_int = 0x10;
pub const POSIX_SPAWN_SETSCHEDULER: c_int = 0x20;
pub const POSIX_SPAWN_SETSID: c_int = 0x80;

const KNOWN_FLAGS: c_int = POSIX_SPAWN_RESETIDS
    | POSIX_SPAWN_SETPGROUP
    | POSIX_SPAWN_SETSIGDEF
    | POSIX_SPAWN_SETSIGMASK
    | POSIX_SPAWN_SETSCHEDPARAM
    | POSIX_SPAWN_SETSCHEDULER
    | POSIX_SPAWN_SETSID;

/// Descriptor numbers the fast path can describe: the kernel's table width.
const TRACKED_FDS: usize = 256;

const F_GETFD: i32 = slopos_abi::syscall::F_GETFD as i32;
const F_SETFD: i32 = slopos_abi::syscall::F_SETFD as i32;
const FD_CLOEXEC: u64 = slopos_abi::syscall::FD_CLOEXEC;
const O_CLOEXEC: i32 = slopos_abi::syscall::O_CLOEXEC as i32;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
enum ActionKind {
    Close = 1,
    Dup2 = 2,
    Open = 3,
    Chdir = 4,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FileAction {
    kind: ActionKind,
    fd: c_int,
    newfd: c_int,
    oflag: c_int,
    mode: mode_t,
    path: *mut u8,
}

unsafe fn actions_of<'a>(actions: *const posix_spawn_file_actions_t) -> &'a [FileAction] {
    if actions.is_null() {
        return &[];
    }
    let table = &*actions;
    if table.__actions.is_null() || table.__used <= 0 {
        return &[];
    }
    core::slice::from_raw_parts(table.__actions as *const FileAction, table.__used as usize)
}

unsafe fn push_action(actions: *mut posix_spawn_file_actions_t, action: FileAction) -> c_int {
    if actions.is_null() {
        return EINVAL.raw();
    }
    let table = &mut *actions;
    if table.__used < 0 || table.__allocated < 0 || table.__used > table.__allocated {
        return EINVAL.raw();
    }
    if table.__used == table.__allocated {
        let want = if table.__allocated == 0 {
            8
        } else {
            table.__allocated * 2
        };
        let bytes = want as usize * size_of::<FileAction>();
        let grown = crate::ffi::realloc(table.__actions, bytes);
        if grown.is_null() {
            return ENOMEM.raw();
        }
        table.__actions = grown;
        table.__allocated = want;
    }
    let slot = (table.__actions as *mut FileAction).add(table.__used as usize);
    ptr::write(slot, action);
    table.__used += 1;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn_file_actions_init(
    actions: *mut posix_spawn_file_actions_t,
) -> c_int {
    if actions.is_null() {
        return EINVAL.raw();
    }
    ptr::write(
        actions,
        posix_spawn_file_actions_t {
            __allocated: 0,
            __used: 0,
            __actions: ptr::null_mut(),
            __pad: [0; 16],
        },
    );
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn_file_actions_destroy(
    actions: *mut posix_spawn_file_actions_t,
) -> c_int {
    if actions.is_null() {
        return EINVAL.raw();
    }
    for action in actions_of(actions) {
        if !action.path.is_null() {
            crate::ffi::free(action.path as *mut c_void);
        }
    }
    let table = &mut *actions;
    if !table.__actions.is_null() {
        crate::ffi::free(table.__actions);
    }
    table.__actions = ptr::null_mut();
    table.__allocated = 0;
    table.__used = 0;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn_file_actions_addopen(
    actions: *mut posix_spawn_file_actions_t,
    fd: c_int,
    path: *const u8,
    oflag: c_int,
    mode: mode_t,
) -> c_int {
    if fd < 0 || path.is_null() {
        return EBADF.raw();
    }
    let copy = strdup(path);
    if copy.is_null() {
        return ENOMEM.raw();
    }
    let rc = push_action(
        actions,
        FileAction {
            kind: ActionKind::Open,
            fd,
            newfd: -1,
            oflag,
            mode,
            path: copy,
        },
    );
    if rc != 0 {
        crate::ffi::free(copy as *mut c_void);
    }
    rc
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn_file_actions_addclose(
    actions: *mut posix_spawn_file_actions_t,
    fd: c_int,
) -> c_int {
    if fd < 0 {
        return EBADF.raw();
    }
    push_action(
        actions,
        FileAction {
            kind: ActionKind::Close,
            fd,
            newfd: -1,
            oflag: 0,
            mode: 0,
            path: ptr::null_mut(),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn_file_actions_adddup2(
    actions: *mut posix_spawn_file_actions_t,
    fd: c_int,
    newfd: c_int,
) -> c_int {
    if fd < 0 || newfd < 0 {
        return EBADF.raw();
    }
    push_action(
        actions,
        FileAction {
            kind: ActionKind::Dup2,
            fd,
            newfd,
            oflag: 0,
            mode: 0,
            path: ptr::null_mut(),
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn_file_actions_addchdir_np(
    actions: *mut posix_spawn_file_actions_t,
    path: *const u8,
) -> c_int {
    if path.is_null() {
        return EINVAL.raw();
    }
    let copy = strdup(path);
    if copy.is_null() {
        return ENOMEM.raw();
    }
    let rc = push_action(
        actions,
        FileAction {
            kind: ActionKind::Chdir,
            fd: -1,
            newfd: -1,
            oflag: 0,
            mode: 0,
            path: copy,
        },
    );
    if rc != 0 {
        crate::ffi::free(copy as *mut c_void);
    }
    rc
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_init(attr: *mut posix_spawnattr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    ptr::write(
        attr,
        posix_spawnattr_t {
            __flags: 0,
            __pgrp: 0,
            __sd: sigset_t::empty(),
            __ss: sigset_t::empty(),
            __sp: crate::types::sched_param { sched_priority: 0 },
            __policy: 0,
            __pad: [0; 16],
        },
    );
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_destroy(attr: *mut posix_spawnattr_t) -> c_int {
    if attr.is_null() { EINVAL.raw() } else { 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_getflags(
    attr: *const posix_spawnattr_t,
    flags: *mut c_short,
) -> c_int {
    if attr.is_null() || flags.is_null() {
        return EINVAL.raw();
    }
    *flags = (*attr).__flags;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_setflags(
    attr: *mut posix_spawnattr_t,
    flags: c_short,
) -> c_int {
    if attr.is_null() || (flags as c_int) & !KNOWN_FLAGS != 0 {
        return EINVAL.raw();
    }
    (*attr).__flags = flags;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_getpgroup(
    attr: *const posix_spawnattr_t,
    pgroup: *mut pid_t,
) -> c_int {
    if attr.is_null() || pgroup.is_null() {
        return EINVAL.raw();
    }
    *pgroup = (*attr).__pgrp;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_setpgroup(
    attr: *mut posix_spawnattr_t,
    pgroup: pid_t,
) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).__pgrp = pgroup;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_getsigdefault(
    attr: *const posix_spawnattr_t,
    sigdefault: *mut sigset_t,
) -> c_int {
    if attr.is_null() || sigdefault.is_null() {
        return EINVAL.raw();
    }
    *sigdefault = (*attr).__sd;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_setsigdefault(
    attr: *mut posix_spawnattr_t,
    sigdefault: *const sigset_t,
) -> c_int {
    if attr.is_null() || sigdefault.is_null() {
        return EINVAL.raw();
    }
    (*attr).__sd = *sigdefault;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_getsigmask(
    attr: *const posix_spawnattr_t,
    sigmask: *mut sigset_t,
) -> c_int {
    if attr.is_null() || sigmask.is_null() {
        return EINVAL.raw();
    }
    *sigmask = (*attr).__ss;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnattr_setsigmask(
    attr: *mut posix_spawnattr_t,
    sigmask: *const sigset_t,
) -> c_int {
    if attr.is_null() || sigmask.is_null() {
        return EINVAL.raw();
    }
    (*attr).__ss = *sigmask;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn(
    pid: *mut pid_t,
    path: *const u8,
    file_actions: *const posix_spawn_file_actions_t,
    attrp: *const posix_spawnattr_t,
    argv: *const *const u8,
    envp: *const *const u8,
) -> c_int {
    spawn(pid, path, false, file_actions, attrp, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnp(
    pid: *mut pid_t,
    file: *const u8,
    file_actions: *const posix_spawn_file_actions_t,
    attrp: *const posix_spawnattr_t,
    argv: *const *const u8,
    envp: *const *const u8,
) -> c_int {
    spawn(pid, file, true, file_actions, attrp, argv, envp)
}

#[derive(Clone, Copy)]
struct Attrs {
    flags: c_int,
    pgrp: pid_t,
    sigdefault: sigset_t,
    sigmask: sigset_t,
}

impl Attrs {
    unsafe fn read(attrp: *const posix_spawnattr_t) -> Self {
        if attrp.is_null() {
            return Self {
                flags: 0,
                pgrp: 0,
                sigdefault: sigset_t::empty(),
                sigmask: sigset_t::empty(),
            };
        }
        let a = &*attrp;
        Self {
            flags: a.__flags as c_int,
            pgrp: a.__pgrp,
            sigdefault: a.__sd,
            sigmask: a.__ss,
        }
    }

    /// Whether the kernel primitive can express every attribute asked for.
    fn direct(&self) -> bool {
        let unexpressible = POSIX_SPAWN_SETSIGMASK
            | POSIX_SPAWN_SETSCHEDPARAM
            | POSIX_SPAWN_SETSCHEDULER
            | POSIX_SPAWN_SETSID;
        if self.flags & unexpressible != 0 {
            return false;
        }
        !(self.flags & POSIX_SPAWN_SETPGROUP != 0 && self.pgrp != 0)
    }
}

unsafe fn spawn(
    pid: *mut pid_t,
    file: *const u8,
    search: bool,
    file_actions: *const posix_spawn_file_actions_t,
    attrp: *const posix_spawnattr_t,
    argv: *const *const u8,
    envp: *const *const u8,
) -> c_int {
    if file.is_null() || argv.is_null() {
        return EINVAL.raw();
    }
    let attrs = Attrs::read(attrp);
    let envp = if envp.is_null() {
        environ as *const *const u8
    } else {
        envp
    };
    let actions = actions_of(file_actions);
    if attrs.direct()
        && let Some(plan) = ChildTable::plan(actions)
    {
        let outcome = plan.fd_actions().map(|(fd_actions, len)| {
            spawn_direct(
                file,
                search,
                &fd_actions[..len],
                plan.cwd,
                &attrs,
                argv,
                envp,
            )
        });
        plan.close_temporaries();
        match outcome {
            Some(Ok(child)) => {
                if !pid.is_null() {
                    *pid = child;
                }
                return 0;
            }
            Some(Err(e)) => return e.raw(),
            None => {}
        }
    }
    spawn_via_fork(pid, file, search, actions, &attrs, argv, envp)
}

/// The descriptor table the child would hold at `exec`: for each number,
/// which of the parent's descriptors it aliases and whether it survives.
struct ChildTable {
    source: [c_int; TRACKED_FDS],
    cloexec: [bool; TRACKED_FDS],
    opened: [c_int; SPAWN_MAX_FD_ACTIONS],
    opened_len: usize,
    cwd: *const u8,
}

impl ChildTable {
    /// `None` when the actions reach past what the fast path can describe.
    unsafe fn plan(actions: &[FileAction]) -> Option<Self> {
        let mut table = Self {
            source: [-1; TRACKED_FDS],
            cloexec: [false; TRACKED_FDS],
            opened: [-1; SPAWN_MAX_FD_ACTIONS],
            opened_len: 0,
            cwd: ptr::null(),
        };
        for fd in 0..TRACKED_FDS {
            if let Ok(flags) = Sys::fcntl(fd as i32, F_GETFD, 0) {
                table.source[fd] = fd as c_int;
                table.cloexec[fd] = flags as u64 & FD_CLOEXEC != 0;
            }
        }
        for action in actions {
            if !table.apply(action) {
                table.close_temporaries();
                return None;
            }
        }
        Some(table)
    }

    unsafe fn apply(&mut self, action: &FileAction) -> bool {
        match action.kind {
            ActionKind::Close => {
                let Some(fd) = self.index(action.fd) else {
                    return false;
                };
                self.source[fd] = -1;
            }
            ActionKind::Dup2 => {
                let (Some(from), Some(to)) = (self.index(action.fd), self.index(action.newfd))
                else {
                    return false;
                };
                if self.source[from] < 0 {
                    return false;
                }
                self.source[to] = self.source[from];
                self.cloexec[to] = false;
            }
            ActionKind::Open => {
                let Some(fd) = self.index(action.fd) else {
                    return false;
                };
                if self.opened_len == self.opened.len() {
                    return false;
                }
                if !self.cwd.is_null() && *action.path != b'/' {
                    return false;
                }
                let Ok(parent_fd) = Sys::open(action.path, action.oflag | O_CLOEXEC, action.mode)
                else {
                    return false;
                };
                self.opened[self.opened_len] = parent_fd;
                self.opened_len += 1;
                self.source[fd] = parent_fd;
                self.cloexec[fd] = action.oflag & O_CLOEXEC != 0;
            }
            ActionKind::Chdir => {
                if !self.cwd.is_null() {
                    return false;
                }
                self.cwd = action.path;
            }
        }
        true
    }

    fn index(&self, fd: c_int) -> Option<usize> {
        usize::try_from(fd).ok().filter(|&fd| fd < TRACKED_FDS)
    }

    /// The kernel's action list, or `None` when the child holds more than it
    /// can be handed.
    fn fd_actions(&self) -> Option<([SpawnFdAction; SPAWN_MAX_FD_ACTIONS], usize)> {
        let mut out = [clone_fd(0, 0); SPAWN_MAX_FD_ACTIONS];
        let mut n = 0usize;
        for fd in 0..TRACKED_FDS {
            if self.source[fd] < 0 || self.cloexec[fd] {
                continue;
            }
            if n == out.len() {
                return None;
            }
            out[n] = clone_fd(self.source[fd], fd as c_int);
            n += 1;
        }
        Some((out, n))
    }

    unsafe fn close_temporaries(&self) {
        for &fd in &self.opened[..self.opened_len] {
            let _ = Sys::close(fd);
        }
    }
}

fn clone_fd(src: c_int, target: c_int) -> SpawnFdAction {
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

unsafe fn count_strings(list: *const *const u8) -> usize {
    let mut n = 0usize;
    while !(*list.add(n)).is_null() {
        n += 1;
    }
    n
}

unsafe fn spawn_direct(
    file: *const u8,
    search: bool,
    fd_actions: &[SpawnFdAction],
    cwd: *const u8,
    attrs: &Attrs,
    argv: *const *const u8,
    envp: *const *const u8,
) -> Result<pid_t, Errno> {
    let mut flags = 0u16;
    if attrs.flags & POSIX_SPAWN_SETPGROUP != 0 {
        flags |= TASK_FLAG_NEW_PGRP;
    }
    let sigdefault_mask = if attrs.flags & POSIX_SPAWN_SETSIGDEF != 0 {
        attrs.sigdefault.kernel_mask()
    } else {
        0
    };
    let kernel_attrs = SpawnAttrs {
        priority: TaskPriority::Normal.as_u8(),
        _pad: [0; 3],
        flags,
        _pad2: 0,
        actions_ptr: fd_actions.as_ptr() as u64,
        actions_len: fd_actions.len() as u64,
        sigdefault_mask,
        envp_ptr: envp as u64,
        envp_len: count_strings(envp) as u64,
        cwd_ptr: cwd as u64,
        cwd_len: if cwd.is_null() {
            0
        } else {
            u_strlen(cwd) as u64
        },
    };
    let argc = count_strings(argv) as u32;
    let launch =
        |path: *const u8, len: usize| Sys::spawn_path(path, len, argv, argc, &kernel_attrs);

    let file_len = u_strlen(file);
    if !search || core::slice::from_raw_parts(file, file_len).contains(&b'/') {
        return launch(file, file_len);
    }
    let path_val = crate::env::getenv(b"PATH\0".as_ptr());
    if path_val.is_null() {
        return Err(ENOENT);
    }
    let path_len = u_strlen(path_val);
    let mut buf = [0u8; 4096];
    let mut last = ENOENT;
    let mut seg_start = 0usize;
    while seg_start < path_len {
        let mut seg_end = seg_start;
        while seg_end < path_len && *path_val.add(seg_end) != b':' {
            seg_end += 1;
        }
        // POSIX: an empty PATH element names the current directory.
        let (dir, dir_len) = if seg_end == seg_start {
            (b".".as_ptr(), 1)
        } else {
            (path_val.add(seg_start).cast_const(), seg_end - seg_start)
        };
        let total = dir_len + 1 + file_len;
        if total < buf.len() {
            ptr::copy_nonoverlapping(dir, buf.as_mut_ptr(), dir_len);
            buf[dir_len] = b'/';
            ptr::copy_nonoverlapping(file, buf.as_mut_ptr().add(dir_len + 1), file_len);
            buf[total] = 0;
            match launch(buf.as_ptr(), total) {
                Ok(child) => return Ok(child),
                Err(e) if e == ENOENT || e == ENOTDIR || e == EACCES => last = e,
                Err(e) => return Err(e),
            }
        }
        seg_start = seg_end + 1;
    }
    Err(last)
}

unsafe fn spawn_via_fork(
    pid: *mut pid_t,
    file: *const u8,
    search: bool,
    actions: &[FileAction],
    attrs: &Attrs,
    argv: *const *const u8,
    envp: *const *const u8,
) -> c_int {
    let child = match Sys::fork() {
        Ok(child) => child,
        Err(e) => return e.raw(),
    };
    if child != 0 {
        if !pid.is_null() {
            *pid = child;
        }
        return 0;
    }
    if attrs.flags & POSIX_SPAWN_SETSIGMASK != 0 {
        sigprocmask(SIG_SETMASK, &attrs.sigmask, ptr::null_mut());
    }
    if attrs.flags & POSIX_SPAWN_SETSIGDEF != 0 {
        let mask = attrs.sigdefault.kernel_mask();
        for sig in 1..64 {
            if mask & (1u64 << (sig - 1)) != 0 {
                signal(sig, SIG_DFL);
            }
        }
    }
    if attrs.flags & POSIX_SPAWN_SETSID != 0 && Sys::setsid().is_err() {
        crate::process::_exit(127);
    }
    if attrs.flags & POSIX_SPAWN_SETPGROUP != 0 && Sys::setpgid(0, attrs.pgrp).is_err() {
        crate::process::_exit(127);
    }
    for action in actions {
        let ok = match action.kind {
            ActionKind::Close => match Sys::close(action.fd) {
                Ok(_) => true,
                Err(e) => e == EBADF,
            },
            ActionKind::Dup2 if action.fd == action.newfd => {
                Sys::fcntl(action.fd, F_SETFD, 0).is_ok()
            }
            ActionKind::Dup2 => Sys::dup2(action.fd, action.newfd).is_ok(),
            ActionKind::Open => match Sys::open(action.path, action.oflag, action.mode) {
                Ok(fd) if fd == action.fd => true,
                Ok(fd) => {
                    let placed = Sys::dup2(fd, action.fd).is_ok();
                    let _ = Sys::close(fd);
                    placed
                }
                Err(_) => false,
            },
            ActionKind::Chdir => Sys::chdir(action.path).is_ok(),
        };
        if !ok {
            crate::process::_exit(127);
        }
    }
    if search {
        environ = envp as *mut *mut u8;
        crate::process::execvp(file, argv);
    } else {
        crate::process::execve(file, argv, envp);
    }
    crate::process::_exit(127);
}
