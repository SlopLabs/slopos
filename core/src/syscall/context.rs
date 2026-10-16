//! Per-syscall context.
//!
//! Built once by [`crate::syscall::dispatch::syscall_handle`] at syscall entry.
//! Handler bodies receive `&SyscallContext` and typed arguments; the full
//! [`UserContext`] is exposed for the handlers that manipulate the whole frame
//! (`exec`, `fork`, `clone`, `rt_sigreturn`).

use slopos_abi::Errno;
use slopos_fs::fileio::FdTable;
use slopos_ostd::KArc;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::process::cred::Subject;
use slopos_ostd::process::{Cred, Identity, Program};
use slopos_ostd::user::context::UserContext;
use slopos_ostd::wl_currency::{self, WL_DELTA};
use slopos_sched::task::TaskRef;
use slopos_sched::task_struct::{Current, Task};

use crate::syscall::result::SyscallResult;

/// Six-register argument payload, snapshotted at syscall entry: `rdi`, `rsi`,
/// `rdx`, `r10`, `r8`, `r9` — System V AMD64 with `r10` standing in for `rcx`,
/// which the `syscall` instruction clobbers.
pub type SyscallRegs = [u64; 6];

/// `task`'s identity, or `None` for a kernel task.
pub fn cred_of(task: &Task) -> Option<Cred> {
    if task.flags & slopos_abi::task::TASK_FLAG_USER_MODE == 0 {
        return None;
    }
    Some(
        task.process()
            .map_or(Cred::NOBODY, |process| process.cred()),
    )
}

/// What `task` attests to a peer it connects to. A kernel task is the kernel.
pub fn identity_of(task: &Task) -> Identity {
    if task.flags & slopos_abi::task::TASK_FLAG_USER_MODE == 0 {
        return Identity::KERNEL;
    }
    let process = task.process();
    let cred = process
        .as_deref()
        .map_or(Cred::NOBODY, |process| process.cred());
    let tgid = task.tgid;
    let (program, instance) = process
        .as_deref()
        .map_or((Program::Other, 0), |process| process.program_as_of_now());
    Identity {
        pid: if tgid == slopos_abi::task::INVALID_TASK_ID {
            task.task_id
        } else {
            tgid
        },
        uid: cred.euid,
        gid: cred.egid,
        program,
        process: process.map_or(0, |process| process.handle_raw()),
        instance,
    }
}

/// Who `task`'s permission checks are for. A kernel task is the kernel.
pub fn subject_of(task: &Task) -> Subject {
    match cred_of(task) {
        Some(cred) => Subject {
            cred,
            dac_override: slopos_ostd::authority::mask_permits(
                slopos_ostd::task::ops::task_caps(task),
                slopos_ostd::authority::Capability::DacOverride,
            ),
        },
        None => Subject::KERNEL,
    }
}

/// The calling task, its arguments, and its user-mode frame, for the duration
/// of one syscall. The task is a **borrow**: it is alive because it is
/// executing this code.
pub struct SyscallContext<'a> {
    task: &'a Task,
    task_id: u32,
    user_ctx: &'a UserContext,
    regs: SyscallRegs,
    /// Both borrows belong to one CPU; without this the context would be
    /// auto-`Send + Sync`.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl<'a> SyscallContext<'a> {
    /// Build a context for the task this CPU is running. Borrowing the guard
    /// rather than storing it ties the context's lifetime to it.
    pub fn from_current(current: &'a Current, ctx: &'a UserContext) -> Self {
        Self::new(current.task(), current.id(), ctx)
    }

    /// Build a context from a registry guard. Tests only: the fixture parks the
    /// BSP on a pre-heap stub where `Current::get()` returns `None`, so the
    /// production constructor is unusable there.
    #[doc(hidden)]
    pub fn from_task_ref(task: &'a TaskRef, ctx: &'a UserContext) -> Self {
        Self::new(task, task.task_id, ctx)
    }

    #[inline]
    fn new(task: &'a Task, task_id: u32, ctx: &'a UserContext) -> Self {
        // Snapshot the argument registers once: signal delivery or a restart
        // decision may rewrite the user context before a handler reads them.
        Self {
            task,
            task_id,
            regs: ctx.syscall_args(),
            user_ctx: ctx,
            _not_send: core::marker::PhantomData,
        }
    }

    /// Macro-internal hook for `define_syscall!`; handler bodies take typed
    /// args through `SyscallArg::from_raw` instead.
    #[inline]
    pub fn regs(&self) -> &SyscallRegs {
        &self.regs
    }

    #[inline]
    pub fn task(&self) -> &'a Task {
        self.task
    }

    #[inline]
    pub fn task_id(&self) -> u32 {
        self.task_id
    }

    /// The calling task's process id, or `INVALID_PROCESS_ID` for a task bound
    /// to no process. ABI only — acting on the caller's process needs
    /// [`require_process`](Self::require_process), which id recycling cannot
    /// confuse.
    #[inline]
    pub fn process_id(&self) -> u32 {
        self.task.process_id
    }

    #[inline]
    pub fn require_task_id(&self) -> Result<u32, Errno> {
        Ok(self.task_id)
    }

    /// This caller's effective capability mask.
    #[inline]
    pub fn caps(&self) -> u64 {
        slopos_ostd::task::ops::task_caps(self.task)
    }

    /// Whether this caller holds `cap`.
    #[inline]
    pub fn holds(&self, cap: slopos_ostd::authority::Capability) -> bool {
        slopos_ostd::authority::mask_permits(self.caps(), cap)
    }

    /// Mint the witness for `R`, or `EPERM`.
    ///
    /// Branded to this context's borrow of the calling task, so the witness
    /// cannot outlive the request that produced it. The dispatcher has already
    /// made the same decision for the slot's declared capability; this is what
    /// a *primitive* demands, and the two read the same mask.
    #[inline]
    pub fn require_cap<R: slopos_ostd::authority::CapKind>(
        &self,
    ) -> Result<slopos_ostd::authority::Cap<'_, R>, Errno> {
        slopos_ostd::authority::check_mask::<R, _>(self.task, self.caps())
    }

    /// The caller's identity. A kernel task acts as uid 0.
    pub fn cred(&self) -> Cred {
        cred_of(self.task).unwrap_or(Cred::ROOT)
    }

    /// Who this caller's permission checks are for.
    pub fn subject(&self) -> Subject {
        subject_of(self.task)
    }

    /// What this caller attests to a socket it connects.
    pub fn identity(&self) -> Identity {
        identity_of(self.task)
    }

    /// The calling process's descriptor table, read from the task's own
    /// generation-checked handle rather than resolved from its id. `ESRCH` for
    /// a kernel task — never [`FdTable::Kernel`], whose descriptors every
    /// kernel task shares.
    #[inline]
    pub fn require_process(&self) -> Result<FdTable, Errno> {
        self.task
            .process()
            .as_deref()
            .and_then(FdTable::of)
            .ok_or(Errno::ESRCH)
    }

    /// The caller's process's working directory, shared by its threads;
    /// NUL-terminated.
    pub fn with_cwd<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        self.task.with_cwd(f)
    }

    pub fn vm_space(&self) -> Result<KArc<VmSpace>, Errno> {
        let vm_process = self.require_process()?.process().ok_or(Errno::ESRCH)?;
        slopos_mm::process_vm::process_vm_get_vm_space(vm_process).ok_or(Errno::EFAULT)
    }

    /// The user-mode register file this syscall entered from. Writes go
    /// through it directly — see the write contract on [`UserContext`].
    #[inline]
    pub fn user_ctx(&self) -> &'a UserContext {
        self.user_ctx
    }

    #[inline]
    pub fn user_rsp(&self) -> u64 {
        self.user_ctx().rsp()
    }

    // The only sites that write `rax`: the dispatcher matches on
    // `SyscallResult` and calls one; handler bodies never invoke them.

    /// Write a successful return value to user `rax`, bumping the
    /// `wl_currency` balance.
    pub fn write_ok(&self, value: u64) {
        wl_currency::adjust_balance(WL_DELTA);
        self.user_ctx.set_rax(value);
    }

    /// Write an errno return value to user `rax`, decrementing the
    /// `wl_currency` balance.
    pub fn write_err(&self, errno: Errno) {
        wl_currency::adjust_balance(-WL_DELTA);
        self.user_ctx.set_rax(errno.as_u64());
    }

    /// Write a raw u64, for the `ERRNO_ERESTARTSYS` sentinel that lies outside
    /// the `[-4095, -1]` `Errno` range.
    pub fn write_err_u64(&self, raw: u64) {
        wl_currency::adjust_balance(-WL_DELTA);
        self.user_ctx.set_rax(raw);
    }

    /// Write the post-handler `SyscallResult`; `NoReturn` leaves `rax` alone.
    pub fn write_result(&self, result: SyscallResult) {
        match result {
            SyscallResult::Ok(v) => self.write_ok(v),
            SyscallResult::Err(e) => {
                if e == Errno::ERESTARTSYS {
                    self.write_err_u64(slopos_abi::syscall::ERRNO_ERESTARTSYS);
                } else {
                    self.write_err(e);
                }
            }
            SyscallResult::NoReturn => {}
        }
    }
}
