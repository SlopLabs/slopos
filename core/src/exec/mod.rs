//! exec() syscall implementation for loading and executing ELF binaries from filesystem.

pub mod grants;
#[cfg(feature = "test-hooks")]
pub mod tests;
#[cfg(feature = "test-hooks")]
pub mod utest;

use core::ffi::{c_char, c_int};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use slopos_fs::fileio::FdTable;

use slopos_abi::Errno;
use slopos_abi::quota::CommitPagesAxis;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::process::quota::try_charge;
use slopos_ostd::{KArc, KBox, KVec};

use slopos_abi::auxv::{
    AT_BASE, AT_ENTRY, AT_EXECFN, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM, AT_SECURE,
};
use slopos_abi::fs::USER_PATH_MAX;
use slopos_abi::task::{TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_NAME_MAX_LEN, TaskPriority};
use slopos_fs::VfsError;
use slopos_fs::fileio::{
    FileRef, file_close_fd, fileio_clone_file_ref, fileio_create_empty_table_for_process,
    fileio_destroy_table_for_process, fileio_install_file_ref_at, fileio_take_file_ref_matching,
};
use slopos_fs::vfs::CanonPath;
use slopos_fs::vfs::ops::{VfsHandle, vfs_open};
use slopos_fs::vfs::path::{RESOLVE_FOLLOW, resolve_path_canon_at};
use slopos_mm::elf::{
    ELF_HEADER_WINDOW, ElfExecInfo, MAX_LOAD_SEGMENTS, SegmentBudget, ValidatedSegment,
    interpreter_extent,
};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm::{
    InterpreterImage, exec_commit_pages, process_vm_end_prepay, process_vm_get_stack_top,
    process_vm_get_vm_space, process_vm_map_elf_image, process_vm_map_interpreter,
    process_vm_prepay_commit, process_vm_reset_for_exec, process_vm_reset_stack,
    process_vm_write_user_bytes,
};
use slopos_ostd::klog_info;

use slopos_abi::task::INVALID_TASK_ID;
use slopos_ostd::task::new_group_in_session;
use slopos_sched::scheduler::publish_new_task;
use slopos_sched::task::{SpawnGuard, link_child, task_build, task_find_by_id, task_terminate};
use slopos_sched::task::{TaskEntry, task_default_signals_in_mask, task_entry_from_kernel_va};

pub use slopos_abi::spawn::{EXEC_MAX_ARG_BYTES, EXEC_MAX_ARG_PAGES, EXEC_MAX_ARG_STRLEN};

/// Pointers walked before a user array missing its NULL is given up on. A loop
/// bound, not a policy limit: [`EXEC_MAX_ARG_PAGES`] decides what fits.
pub const EXEC_MAX_ARG_STRINGS: usize = 4096;
/// Ceiling on an executable's file size. Nothing is staged in kernel memory to
/// load one; the mapped extent is bounded by
/// [`slopos_mm::elf::MAX_TOTAL_MAPPED_SIZE`].
pub const EXEC_MAX_ELF_SIZE: usize = 512 * 1024 * 1024;

/// Bytes per `FileSystem::read` while staging an ELF.
///
/// One block per call costs a mount-lock acquisition and a device round trip
/// each: on ext2 the read runs under `CACHED_EXT2`, so a 712 KiB shell took 178
/// acquisitions of a lock every path walk on that mount waits behind. Bounded
/// rather than the whole file, so how long one `exec` may hold that lock stays
/// a stated number.
pub const EXEC_READ_CHUNK: usize = 64 * 1024;

pub const INIT_PATH: &[u8] = b"/sbin/init";

/// What `setup_user_stack` spends whatever the argument count: red zone, two
/// realignments, the odd-slot pad, nine auxv pairs, argc and the two NULL
/// sentinels, the `AT_EXECFN` string at its longest — plus the word
/// `setup_user_stack` drops before any of it. The path is charged at its
/// ceiling so `execve` can be refused before it resolves what it opens.
const EXEC_ARG_STACK_FIXED: usize =
    8 + 128 + 16 + 16 + 8 + 9 * 16 + 3 * 8 + (USER_PATH_MAX + 1).next_multiple_of(8);

/// Whether `argv` + `envp` fit the budget, counting exactly what
/// [`setup_user_stack`] will push.
pub fn exec_arg_bytes_fit(argv: Option<&[&[u8]]>, envp: Option<&[&[u8]]>) -> bool {
    let mut total = EXEC_ARG_STACK_FIXED;
    for list in [argv, envp].into_iter().flatten() {
        for s in list.iter() {
            let Some(with_nul) = s.len().checked_add(1) else {
                return false;
            };
            if with_nul > EXEC_MAX_ARG_STRLEN {
                return false;
            }
            // `setup_user_stack` realigns sp to 8 after each string.
            let padded = with_nul.next_multiple_of(8);
            let Some(next) = total
                .checked_add(padded)
                .and_then(|t| t.checked_add(core::mem::size_of::<u64>()))
            else {
                return false;
            };
            if next > EXEC_MAX_ARG_BYTES {
                return false;
            }
            total = next;
        }
    }

    true
}

/// A decoded spawn file action.
pub enum FdAction {
    /// Share the parent's `src_fd` description into the child's `target_fd`.
    Clone {
        src_fd: i32,
        target_fd: i32,
    },
    /// Move the parent's `src_fd` into the child's `target_fd`.
    Transfer {
        src_fd: i32,
        target_fd: i32,
    },
    Close {
        target_fd: i32,
    },
    // `Open` is retired: it opened an arbitrary VFS path into the child with
    // no reference to what the parent held, which is endowment by *name* and
    // voids this list as an attenuating channel. Open-then-transfer is the
    // replacement and cannot exceed the spawner's own authority.
}

fn trim_nul_bytes(bytes: &[u8]) -> &[u8] {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..len]
}

/// The task id `launch_init` handed to `/sbin/init`, or [`INVALID_TASK_ID`]
/// before it runs.
static INIT_TASK_ID: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);

/// Which task is init. There is no structural marker — `TASK_FLAG_SYSTEM` is
/// shared with the utest runner — so the launch id names it.
pub fn init_task_id() -> u32 {
    INIT_TASK_ID.load(Ordering::Acquire)
}

pub fn launch_init() -> Result<u32, Errno> {
    let task_id = spawn_program_with_attrs(
        INIT_PATH,
        None,
        None,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM,
        &[],
        0,
        None,
        INVALID_TASK_ID,
    )?;
    INIT_TASK_ID.store(task_id, Ordering::Release);
    Ok(task_id)
}

/// Apply the spawn fd-action list to the child's empty, unpublished table.
/// All-or-nothing: `Transfer` installs a shared alias first and empties its
/// parent slot only once the whole list applied, so any failure leaves the
/// parent holding every descriptor.
pub(crate) fn apply_fd_actions(
    parent_table: FdTable,
    child_table: FdTable,
    actions: &[FdAction],
) -> Result<(), Errno> {
    let mut transfers: KVec<(i32, FileRef)> =
        KVec::with_capacity(actions.len()).map_err(|_| Errno::ENOMEM)?;
    for action in actions {
        let rc: c_int = match action {
            FdAction::Clone { src_fd, target_fd } => {
                match fileio_clone_file_ref(parent_table, *src_fd) {
                    Some(file) => fileio_install_file_ref_at(child_table, *target_fd, file, false),
                    None => Errno::EBADF.raw(),
                }
            }
            FdAction::Transfer { src_fd, target_fd } => {
                match fileio_clone_file_ref(parent_table, *src_fd) {
                    Some(file) => {
                        let moved = file.alias();
                        let rc = fileio_install_file_ref_at(child_table, *target_fd, file, false);
                        if rc >= 0 && transfers.push((*src_fd, moved)).is_err() {
                            return Err(Errno::ENOMEM);
                        }
                        rc
                    }
                    None => Errno::EBADF.raw(),
                }
            }
            FdAction::Close { target_fd } => {
                let rc = file_close_fd(child_table, *target_fd);
                // A fresh child table holds nothing at most fds; closing an absent one succeeds.
                if rc == Errno::EBADF.raw() { 0 } else { rc }
            }
        };
        if rc < 0 {
            return Err(Errno::from_raw(rc).unwrap_or(Errno::EIO));
        }
    }
    // The identity match skips a slot the parent concurrently closed or repopulated.
    for (src_fd, moved) in transfers.iter() {
        drop(fileio_take_file_ref_matching(parent_table, *src_fd, moved));
    }
    Ok(())
}

fn task_name_from_path(path: &[u8]) -> Result<[u8; TASK_NAME_MAX_LEN], Errno> {
    let trimmed = trim_nul_bytes(path);
    if trimmed.is_empty() {
        return Err(Errno::ENAMETOOLONG);
    }

    let basename_start = trimmed
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |idx| idx + 1);
    let basename = &trimmed[basename_start..];

    if basename.is_empty() || basename.len() >= TASK_NAME_MAX_LEN {
        return Err(Errno::ENAMETOOLONG);
    }

    let mut name = [0u8; TASK_NAME_MAX_LEN];
    name[..basename.len()].copy_from_slice(basename);
    Ok(name)
}

struct InheritedJobControl {
    pgid: u32,
    sid: u32,
    ctty: Option<slopos_abi::syscall::TtyIndex>,
    group: Option<slopos_ostd::KArc<slopos_ostd::task::ProcessGroup>>,
}

/// Point the child's group at the identity its inherited pgid names.
fn resolve_inherited_job_control(
    parent: &slopos_sched::task_struct::Task,
    child_task_id: u32,
    flags: u16,
) -> InheritedJobControl {
    let new_pgrp = flags & slopos_abi::task::TASK_FLAG_NEW_PGRP != 0;
    let (pgid, group) = if new_pgrp {
        let group = parent
            .process_group
            .load()
            .and_then(|pg| new_group_in_session(child_task_id, pg.session().clone()));
        (child_task_id, group)
    } else {
        (parent.pgid(), parent.process_group.load())
    };
    InheritedJobControl {
        pgid,
        sid: parent.sid(),
        ctty: parent.controlling_tty(),
        group,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_program_with_attrs(
    path: &[u8],
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    priority: TaskPriority,
    flags: u16,
    actions: &[FdAction],
    sigdefault_mask: u64,
    parent_table: Option<FdTable>,
    parent_task_id: u32,
) -> Result<u32, Errno> {
    spawn_program_with_cwd(
        path,
        argv,
        envp,
        priority,
        flags,
        actions,
        sigdefault_mask,
        parent_table,
        parent_task_id,
        b"/",
    )
}

/// `cwd` must be an absolute canonical path: the child has no context of its
/// own to resolve a relative one against.
#[allow(clippy::too_many_arguments)]
pub fn spawn_program_with_cwd(
    path: &[u8],
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    mut priority: TaskPriority,
    mut flags: u16,
    actions: &[FdAction],
    sigdefault_mask: u64,
    parent_table: Option<FdTable>,
    parent_task_id: u32,
    cwd: &[u8],
) -> Result<u32, Errno> {
    let result = (|| {
        // Resolved once, here: the grant table, the task name and the loader
        // must all agree on which file this is.
        let program = resolve_program(path, cwd)?;
        let normalized_path = program.as_bytes();

        // Privilege enters a spawn only here — the syscall boundary already
        // refused every privileged bit the caller asked for, so flags follow
        // the program, not the requester.
        let (granted_flags, granted_priority) = grants::grant_for(normalized_path);

        // A raise needs `Launch`; an ordinary spawn raises nothing and needs
        // no right. Not an intersection with the spawner's own authority: the
        // shell holds no display authority, so `/bin/roulette` could not draw.
        if granted_flags != 0 {
            let spawner_may_launch = match task_find_by_id(parent_task_id) {
                Some(parent) => slopos_ostd::authority::mask_permits(
                    slopos_ostd::task::ops::task_caps(&parent),
                    slopos_ostd::authority::Capability::Launch,
                ),
                // No parent task: `launch_init`, the kernel-only root. That is
                // exactly the raise `Launch` exists to bound, performed by the
                // one caller that cannot be userland.
                None => true,
            };
            if !spawner_may_launch {
                return Err(Errno::ENOEXEC);
            }
        }

        flags |= granted_flags;
        if let Some(granted) = granted_priority {
            priority = granted;
        }

        flags |= TASK_FLAG_USER_MODE;
        let task_name = task_name_from_path(normalized_path)?;
        let user_code_entry: TaskEntry = task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64);

        // Unregistered and singly owned until `task_commit`, so no lookup,
        // walk or other CPU can observe the field writes below.
        let Some(pending) = task_build(
            task_name.as_ptr() as *const c_char,
            user_code_entry,
            ptr::null_mut(),
            priority.as_u8(),
            flags,
        ) else {
            return Err(Errno::ENOMEM);
        };
        // Nothing else can find the orphan, so every exit from here — `?`, a
        // panic, a kill aborting the blocking calls — must release it here.
        let mut spawn = SpawnGuard::new(pending);
        let task_id = spawn.child_id();

        let mut entry = 0u64;
        let mut stack_ptr = 0u64;
        let mut tls_tp = 0u64;

        // Refused rather than defaulted: a child with no table of its own must
        // not be exec'd against the kernel's, which every kernel task shares.
        let Some(child_table) = spawn.child_table() else {
            return Err(Errno::ENOMEM);
        };

        // Before the fd actions: a path-taking action resolves against it.
        // The `_exclusive` form because `set_cwd`'s witness names the
        // spawner, not this not-yet-published child.
        let Some(cwd_stored) = spawn.with_child(|child| child.set_cwd_exclusive(cwd)) else {
            return Err(Errno::ENOMEM);
        };
        if !cwd_stored {
            return Err(Errno::ENOMEM);
        }

        exec_image(
            child_table,
            &program,
            argv,
            envp,
            granted_flags != 0,
            &mut entry,
            &mut stack_ptr,
            &mut tls_tp,
        )?;

        // A caller with no parent process (`launch_init`) keeps its console
        // bootstrap table untouched.
        if let Some(parent_table) = parent_table {
            // Taken from the guard rather than re-resolved: the id could have
            // been returned to the allocator between the two lookups.
            let Some(FdTable::Process(child_process)) = spawn.child_table() else {
                return Err(Errno::ENOMEM);
            };
            fileio_destroy_table_for_process(child_process.handle());
            if fileio_create_empty_table_for_process(child_process.handle()) != 0 {
                return Err(Errno::ENOMEM);
            }
            apply_fd_actions(parent_table, FdTable::Process(child_process), actions)?;
        }

        // Inherited so the child joins the parent's session; without it the
        // shell cannot make the child its foreground group.
        let parent_ref = task_find_by_id(parent_task_id);

        // Resolved — and for NEW_PGRP allocated — before the child borrow
        // opens: that borrow runs preempt-disabled, where a fallible allocation
        // has no business.
        let inherited = parent_ref
            .as_ref()
            .map(|parent| resolve_inherited_job_control(parent, task_id, flags));

        // Authority enters here, from the program-identity grant applied
        // above -- the single raise site. Stamped on the child before it is
        // findable, so no other CPU can observe it with an unset mask.
        let child_caps = slopos_ostd::authority::caps_from_task_flags(flags);

        let Some((fg_handoff, displaced_group)) = spawn.with_child(|child| {
            slopos_ostd::task::ops::task_set_caps(child, child_caps);
            child.entry_point = entry;
            child.context.get_mut().rip = entry;
            child.context.get_mut().rsp = stack_ptr;
            child.set_fs_base(tls_tp);

            // The kernel stack stays as `task_build` left it; the iretq frame
            // is rebuilt from `user_ctx` on every round trip.
            slopos_sched::task::init_user_ctx_for_new_task(
                child.user_ctx.get_mut(),
                entry,
                stack_ptr,
                0,
            );

            // POSIX_SPAWN_SETSIGDEF.
            if sigdefault_mask != 0 {
                task_default_signals_in_mask(child, sigdefault_mask);
            }

            let Some(inherited) = inherited else {
                return (None, None);
            };
            child.set_pgid(inherited.pgid);
            child.set_sid(inherited.sid);
            child.set_controlling_tty(inherited.ctty);
            // Exclusive rather than RCU: no reader can observe the child yet.
            // The displaced handle travels out because dropping a `KArc` under
            // the preempt guard could reach the buddy allocator's reuse path.
            let displaced = child.process_group.replace_exclusive(inherited.group);

            let fg = (flags & slopos_abi::task::TASK_FLAG_FOREGROUND != 0 && inherited.pgid != 0)
                .then_some(inherited.ctty)
                .flatten()
                .map(|ctty| (ctty, inherited.pgid, inherited.sid));
            (fg, displaced)
        }) else {
            return Err(Errno::ENOMEM);
        };
        drop(displaced_group);

        // Findable from here but still not runnable: `publish_new_task` below
        // is the sole schedulable edge.
        let Some(registered) = spawn.commit() else {
            return Err(Errno::ENOMEM);
        };

        // Only after `commit`: the edge must point at a live registry entry.
        if let Some(parent) = parent_ref.as_ref()
            && let Some(child_nn) = core::ptr::NonNull::new(registered.as_ptr())
        {
            link_child(parent, child_nn);
        }

        // Must precede the Ready-publish: a parent-side `tcsetpgrp` after spawn
        // returns leaves a window where the child is schedulable but still
        // background, and its first terminal read fails the foreground check.
        if let Some((ctty, child_pgid, child_sid)) = fg_handoff {
            use slopos_kernel_services::syscall_services::tty;
            // Checked variant: session match and set under one TTY lock, no
            // read-then-write TOCTOU.
            let _ = tty::set_foreground_pgrp_checked(ctty, child_pgid, child_sid);
        }

        // `publish_new_task`'s Release status store is what makes every write
        // above visible to the CPU that runs this task.
        if publish_new_task(&registered) != 0 {
            task_terminate(task_id);
            return Err(Errno::ENOMEM);
        }

        Ok(task_id)
    })();

    result
}

/// Resolve a program path the way the loader will open it: against `cwd`,
/// symlinks expanded, to the canonical path the grant table is keyed on.
///
/// Resolving *before* the grant comparison is what lets `./halt` from `/sbin`
/// match the grant `/sbin/halt` carries, and cannot invent one: every grant
/// path is sealed, so no name can be pointed at one.
///
/// `#[inline(never)]` keeps the walk off the frame of a caller already
/// holding a `SpawnGuard`.
#[inline(never)]
pub fn resolve_program(path: &[u8], cwd: &[u8]) -> Result<CanonPath, Errno> {
    let trimmed = trim_nul_bytes(path);
    if trimmed.is_empty() || trimmed.len() > USER_PATH_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    let (_, canon) = resolve_path_canon_at(trimmed, cwd, RESOLVE_FOLLOW).map_err(|e| match e {
        VfsError::NotFound | VfsError::NotDirectory => Errno::ENOENT,
        VfsError::NameTooLong => Errno::ENAMETOOLONG,
        VfsError::IsDirectory
        | VfsError::PermissionDenied
        | VfsError::TooManySymlinks
        | VfsError::InvalidPath => Errno::ENOEXEC,
        _ => Errno::EIO,
    })?;
    Ok(canon)
}

/// `program` is [`resolve_program`]'s output, not a caller's spelling: the
/// type is what stops a relative path reaching the loader, which resolves
/// against `/`.
///
/// `execve`'s road: it can only narrow the caller's authority, so it never
/// confers a grant and the image never runs `AT_SECURE`.
pub fn do_exec(
    table: FdTable,
    program: &CanonPath,
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    entry_out: &mut u64,
    stack_ptr_out: &mut u64,
    tls_tp_out: &mut u64,
) -> Result<(), Errno> {
    exec_image(
        table,
        program,
        argv,
        envp,
        false,
        entry_out,
        stack_ptr_out,
        tls_tp_out,
    )
}

/// `secure` is whether this exec conferred a grant, which is what the image
/// finds as `AT_SECURE`.
#[allow(clippy::too_many_arguments)]
fn exec_image(
    table: FdTable,
    program: &CanonPath,
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    secure: bool,
    entry_out: &mut u64,
    stack_ptr_out: &mut u64,
    tls_tp_out: &mut u64,
) -> Result<(), Errno> {
    let vm_process = table.process().ok_or(Errno::ENOMEM)?;
    let exec_info = load_image(program.as_bytes(), vm_process, entry_out)?;

    if process_vm_reset_stack(vm_process) != 0 {
        return Err(Errno::ENOMEM);
    }
    process_vm_end_prepay(vm_process);

    let stack_top = setup_user_stack(table, argv, envp, &exec_info, program.as_bytes(), secure)?;
    *stack_ptr_out = stack_top;
    *tls_tp_out = exec_info.tls_tp;

    // POSIX: close all FDs with FD_CLOEXEC set after point of no return.
    slopos_fs::fileio_close_on_exec(table);

    klog_info!(
        "exec: loaded ELF for process {}, entry={:#x}, stack={:#x}, tls_tp={:#x}",
        table.id(),
        *entry_out,
        stack_top,
        *tls_tp_out,
    );

    Ok(())
}

/// Open `path` as an executable and answer its size.
///
/// Out of line because [`FileStat`](slopos_fs::FileStat) is large enough to
/// push its caller over the 2 KiB stack gate.
#[inline(never)]
fn open_executable(path: &[u8]) -> Result<(VfsHandle, u64), Errno> {
    let handle = vfs_open(path, false).map_err(|e| match e {
        slopos_fs::VfsError::NotFound => Errno::ENOENT,
        slopos_fs::VfsError::IsDirectory => Errno::ENOEXEC,
        slopos_fs::VfsError::PermissionDenied => Errno::ENOEXEC,
        _ => Errno::EIO,
    })?;

    let stat = handle.fs.stat(handle.inode).map_err(|_| Errno::EIO)?;
    if (stat.mode & 0o111) == 0 {
        return Err(Errno::ENOEXEC);
    }
    if stat.size == 0 || stat.size > EXEC_MAX_ELF_SIZE as u64 {
        return Err(Errno::ENOEXEC);
    }
    Ok((handle, stat.size))
}

/// The interpreter, opened and read before `exec` reaches its point of no
/// return so that a bad `PT_INTERP` is an errno rather than a dead process.
struct StagedInterpreter {
    handle: VfsHandle,
    header: KVec<u8>,
    file_len: u64,
}

/// Open `path`, validate it, and install its image in `process`'s address
/// space. Out of line to keep its locals out of `do_exec`'s frame, which is
/// measured against the 2 KiB stack gate.
///
/// The *filesystem* is resolved before `process_vm_reset_for_exec` — both
/// images are open and their header windows are in hand — because a missing
/// or unreadable `PT_INTERP` is the likeliest failure on a dynamically
/// linked system and past that call the old image is gone, so an error the
/// caller is handed is an error it cannot return to act on. Header
/// *validation* still runs after it, and a malformed image therefore still
/// costs the caller its address space.
#[inline(never)]
fn load_image(
    path: &[u8],
    process: slopos_ostd::process::ProcessId,
    entry_out: &mut u64,
) -> Result<ElfExecInfo, Errno> {
    let (handle, file_size) = open_executable(path)?;

    // Only the header window is staged; the rest goes from the file straight
    // into the mapping, so the image may exceed the 1 MiB slab ceiling.
    let window_len = (file_size as usize).min(ELF_HEADER_WINDOW);
    let mut header: KVec<u8> = KVec::<u8>::zeroed(window_len).map_err(|_| Errno::ENOMEM)?;
    read_exact_at(&handle, 0, header.as_mut_slice())?;

    let interp = match interpreter_extent(header.as_slice(), file_size).map_err(Errno::from)? {
        Some((offset, len)) => Some(stage_interpreter(&handle, offset, len)?),
        None => None,
    };

    // Charged beside the old image, so a program that cannot fit is refused
    // while the caller still has one to return to.
    let pages = exec_commit_pages(
        header.as_slice(),
        file_size,
        interp
            .as_deref()
            .map(|staged| (staged.header.as_slice(), staged.file_len)),
    )
    .map_err(Errno::from)?;
    let funds =
        try_charge::<CommitPagesAxis>(process.account(), pages).map_err(|_| Errno::ENOMEM)?;

    // The address-space boundary. Everything the old image mapped — heap,
    // mmap arena, shared memfds, rings — is severed here, before the new
    // image exists, so no mapping outlives the program that made it.
    if process_vm_reset_for_exec(process) != 0 {
        return Err(Errno::ENOMEM);
    }
    process_vm_prepay_commit(process, funds);

    install_images(
        &handle,
        header.as_slice(),
        file_size,
        interp.as_deref(),
        process,
        entry_out,
    )
}

/// Map the executable, then the interpreter, then stream the executable's
/// bytes in.
///
/// The ordering is load-bearing: mapping takes the page-table cursor, which
/// refuses to install a leaf while a second reference to the address space is
/// live, and streaming holds exactly such a reference.
///
/// Out of line so its segment array and address-space reference stay off
/// [`load_image`]'s frame, which is measured against the 2 KiB stack gate.
#[inline(never)]
fn install_images(
    handle: &VfsHandle,
    header: &[u8],
    file_size: u64,
    interp: Option<&StagedInterpreter>,
    process: slopos_ostd::process::ProcessId,
    entry_out: &mut u64,
) -> Result<ElfExecInfo, Errno> {
    let mut segments =
        KVec::<ValidatedSegment>::zeroed(MAX_LOAD_SEGMENTS).map_err(|_| Errno::ENOMEM)?;
    let (mut exec_info, segment_count) = process_vm_map_elf_image(
        process,
        header,
        file_size,
        segments.as_mut_slice(),
        entry_out,
    )
    .map_err(Errno::from)?;

    if let Some(staged) = interp {
        let image = map_interpreter(staged, process, &segments.as_slice()[..segment_count])?;
        exec_info.interp_base = image.base;
        exec_info.interp_entry = image.entry;
        *entry_out = image.entry;
    }

    let vm_space = process_vm_get_vm_space(process).ok_or(Errno::EFAULT)?;
    stream_segments(handle, &vm_space, &segments.as_slice()[..segment_count])?;
    Ok(exec_info)
}

/// Read `PT_INTERP`'s path out of `image` and open what it names.
///
/// Out of line so the path buffer stays off [`load_image`]'s frame, which is
/// measured against the 2 KiB stack gate.
#[inline(never)]
fn stage_interpreter(
    image: &VfsHandle,
    offset: u64,
    len: u64,
) -> Result<KBox<StagedInterpreter>, Errno> {
    let mut path: KVec<u8> = KVec::<u8>::zeroed(len as usize).map_err(|_| Errno::ENOMEM)?;
    read_exact_at(image, offset, path.as_mut_slice())?;
    let name = trim_nul_bytes(path.as_slice());
    // Resolved against `/`, never against the caller's cwd: the interpreter
    // is part of the program's identity, and a relative one would name a
    // different file per caller.
    if name.is_empty() || name[0] != b'/' {
        return Err(Errno::ENOEXEC);
    }

    let (handle, file_len) = open_executable(name)?;
    let window_len = (file_len as usize).min(ELF_HEADER_WINDOW);
    let mut header: KVec<u8> = KVec::<u8>::zeroed(window_len).map_err(|_| Errno::ENOMEM)?;
    read_exact_at(&handle, 0, header.as_mut_slice())?;
    // Boxed: `load_image`'s frame is measured against the 2 KiB stack gate
    // and an open handle plus a header window does not fit in it.
    KBox::try_new(StagedInterpreter {
        handle,
        header,
        file_len,
    })
    .map_err(|_| Errno::ENOMEM)
}

/// Place the staged interpreter and stream it in.
///
/// `mapped` is the executable's segment set, whose extent the interpreter's
/// budget is the residue of: an `exec` spends the image caps once, not once
/// per image.
///
/// Out of line so its segment array and address-space reference stay off
/// [`load_image`]'s frame.
#[inline(never)]
fn map_interpreter(
    staged: &StagedInterpreter,
    process: slopos_ostd::process::ProcessId,
    mapped: &[ValidatedSegment],
) -> Result<InterpreterImage, Errno> {
    let mut segments =
        KVec::<ValidatedSegment>::zeroed(MAX_LOAD_SEGMENTS).map_err(|_| Errno::ENOMEM)?;
    let image = process_vm_map_interpreter(
        process,
        staged.header.as_slice(),
        staged.file_len,
        SegmentBudget::FULL.less(mapped),
        segments.as_mut_slice(),
    )
    .map_err(Errno::from)?;

    let vm_space = process_vm_get_vm_space(process).ok_or(Errno::EFAULT)?;
    stream_segments(
        &staged.handle,
        &vm_space,
        &segments.as_slice()[..image.segment_count],
    )?;
    Ok(image)
}

/// Fill `buf` from `offset`. A short read fails the load rather than leaving
/// the tail as whatever the buffer held.
fn read_exact_at(handle: &VfsHandle, offset: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let mut done = 0usize;
    while done < buf.len() {
        let read = handle
            .read(offset + done as u64, &mut buf[done..])
            .map_err(|_| Errno::EIO)?;
        if read == 0 {
            return Err(Errno::ENOEXEC);
        }
        done += read;
    }
    Ok(())
}

/// Copy each `PT_LOAD` segment's file bytes into the already-mapped, zeroed
/// pages, reusing one [`EXEC_READ_CHUNK`] buffer, so an `exec`'s kernel heap
/// cost is independent of the image's size. No VM lock is held: the mount lock
/// the read takes is a sleeping one.
fn stream_segments(
    handle: &VfsHandle,
    vm_space: &KArc<VmSpace>,
    segments: &[ValidatedSegment],
) -> Result<(), Errno> {
    let mut staging: KVec<u8> = KVec::<u8>::zeroed(EXEC_READ_CHUNK).map_err(|_| Errno::ENOMEM)?;

    for segment in segments.iter() {
        let mut done = 0u64;
        while done < segment.file_size {
            let chunk = core::cmp::min(segment.file_size - done, EXEC_READ_CHUNK as u64) as usize;
            let buf = &mut staging.as_mut_slice()[..chunk];
            read_exact_at(handle, segment.file_offset + done, buf)?;
            process_vm_write_user_bytes(vm_space, segment.original_vaddr + done, buf)
                .map_err(|_| Errno::EFAULT)?;
            done += chunk as u64;
        }
    }
    Ok(())
}

/// `execfn` is the canonical path of the image, pushed above the argument
/// strings for `AT_EXECFN`.
fn setup_user_stack(
    table: FdTable,
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    exec_info: &ElfExecInfo,
    execfn: &[u8],
    secure: bool,
) -> Result<u64, Errno> {
    let vm_process = table.process().ok_or(Errno::EFAULT)?;
    let stack_top_raw = process_vm_get_stack_top(vm_process);
    if stack_top_raw == 0 {
        return Err(Errno::EFAULT);
    }
    // Resolved once, or every string and pointer below re-takes the slot lock.
    let vm_space = process_vm_get_vm_space(vm_process).ok_or(Errno::EFAULT)?;
    let stack_top = stack_top_raw.wrapping_sub(8);

    let argc = argv.map(|a| a.len()).unwrap_or(0);
    let envc = envp.map(|e| e.len()).unwrap_or(0);

    if !exec_arg_bytes_fit(argv, envp) {
        return Err(Errno::E2BIG);
    }
    let execfn = trim_nul_bytes(execfn);
    if execfn.len() > USER_PATH_MAX {
        return Err(Errno::ENAMETOOLONG);
    }

    let mut sp = stack_top;
    sp = sp.wrapping_sub(128);
    sp &= !0xF;

    sp = sp.wrapping_sub(execfn.len() as u64 + 1);
    sp &= !0x7;
    write_to_user_stack(&vm_space, sp, execfn)?;
    write_byte_to_user_stack(&vm_space, sp + execfn.len() as u64, 0)?;
    let execfn_ptr = sp;

    let mut string_ptrs: KVec<u64> =
        KVec::<u64>::with_capacity(argc + envc + 2).map_err(|_| Errno::ENOMEM)?;

    if let Some(args) = argv {
        for arg in args.iter() {
            let len = arg.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(&vm_space, sp, arg)?;
            write_byte_to_user_stack(&vm_space, sp + arg.len() as u64, 0)?;
            string_ptrs.push(sp).map_err(|_| Errno::ENOMEM)?;
        }
    }

    let argv_start = string_ptrs.len();

    if let Some(envs) = envp {
        for env in envs.iter() {
            let len = env.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(&vm_space, sp, env)?;
            write_byte_to_user_stack(&vm_space, sp + env.len() as u64, 0)?;
            string_ptrs.push(sp).map_err(|_| Errno::ENOMEM)?;
        }
    }

    sp &= !0xF;

    // SysV ABI: rsp must be 16-byte aligned at _start with argc at [rsp].
    let total_slots = argc + envc + 3 + 2 * AUXV_PAIRS;
    if total_slots % 2 != 0 {
        sp = sp.wrapping_sub(8);
    }

    sp = sp.wrapping_sub((AUXV_PAIRS * 16) as u64);
    write_auxv(&vm_space, sp, exec_info, execfn_ptr, secure)?;

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(&vm_space, sp, 0)?;

    for i in (argv_start..string_ptrs.len()).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(&vm_space, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(&vm_space, sp, 0)?;

    for i in (0..argv_start).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(&vm_space, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(&vm_space, sp, argc as u64)?;

    Ok(sp)
}

fn write_to_user_stack(vm_space: &KArc<VmSpace>, addr: u64, data: &[u8]) -> Result<(), Errno> {
    process_vm_write_user_bytes(vm_space, addr, data).map_err(|_| Errno::EFAULT)
}

fn write_byte_to_user_stack(vm_space: &KArc<VmSpace>, addr: u64, byte: u8) -> Result<(), Errno> {
    write_to_user_stack(vm_space, addr, &[byte])
}

fn write_u64_to_user_stack(vm_space: &KArc<VmSpace>, addr: u64, value: u64) -> Result<(), Errno> {
    let bytes = value.to_le_bytes();
    write_to_user_stack(vm_space, addr, &bytes)
}

/// Pairs [`write_auxv`] emits, `AT_NULL` included.
const AUXV_PAIRS: usize = 9;

/// Out of line so the vector stays off [`setup_user_stack`]'s frame, which is
/// measured against the 2 KiB stack gate.
///
/// `AT_BASE` is emitted even for a static image, where it reads 0, because a
/// fixed-size vector is what the budget above can be stated against. Linux
/// does the same.
#[inline(never)]
fn write_auxv(
    vm_space: &KArc<VmSpace>,
    at: u64,
    exec_info: &ElfExecInfo,
    execfn: u64,
    secure: bool,
) -> Result<(), Errno> {
    let auxv: [(u64, u64); AUXV_PAIRS] = [
        (AT_PHDR, exec_info.phdr_addr),
        (AT_PHENT, exec_info.phent_size as u64),
        (AT_PHNUM, exec_info.phnum as u64),
        (AT_PAGESZ, PAGE_SIZE_4KB),
        (AT_BASE, exec_info.interp_base),
        (AT_ENTRY, exec_info.entry),
        (AT_SECURE, secure as u64),
        (AT_EXECFN, execfn),
        (AT_NULL, 0),
    ];
    let mut bytes = [0u8; AUXV_PAIRS * 16];
    for (pair, (a_type, a_val)) in bytes.chunks_exact_mut(16).zip(auxv.iter()) {
        pair[..8].copy_from_slice(&a_type.to_le_bytes());
        pair[8..].copy_from_slice(&a_val.to_le_bytes());
    }
    write_to_user_stack(vm_space, at, &bytes)
}
