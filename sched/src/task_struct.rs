//! Kernel-side aliases for the OSTD-owned generic `TaskInner<K, U>`, plus the
//! razor assertions that pin the field-offset contract at the kernel boundary.

use core::mem::offset_of;

use crate::task_stack::{KernelStack, UnsafeStack};

pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_KERNEL_MODE,
    TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN,
    TASK_STACK_SIZE, TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord, TaskFaultReason,
    TaskPriority, TaskStatus,
};
pub use slopos_ostd::task::SwitchContext;
pub use slopos_ostd::task::abi::TASK_UNSAFE_STACK_SP_OFFSET;
pub use slopos_ostd::task::fpu::{FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT};
pub use slopos_ostd::task::kernel_task::{
    SigHandTable, SignalAction, SignalActionCell, TaskInner, fpu_reset_in_place,
};

// Declared so `CurrentTask` and the PCR publisher agree on the monomorphisation
// the current-task slot holds. Both names are aliases, so the impl heads are the
// local `TaskStack<_>` — which is what makes them legal to write here at all.
slopos_ostd::declare_pcr_stack_type!(KernelStack);
slopos_ostd::declare_pcr_stack_type!(UnsafeStack);

pub type Task = TaskInner<KernelStack, UnsafeStack>;

/// Borrow of the task running on this CPU, at the concrete kernel
/// monomorphisation.
pub type Current = slopos_ostd::task::CurrentTask<KernelStack, UnsafeStack>;

/// Borrow of this CPU's idle task, at the concrete kernel monomorphisation.
pub type Idle = slopos_ostd::task::IdleTask<KernelStack, UnsafeStack>;

/// Exclusive access to one endpoint of a context switch, at the concrete
/// kernel monomorphisation. Minted only by `slopos_ostd::task::run_switch`.
pub type Switching<'a> = slopos_ostd::task::SwitchWindow<'a, KernelStack, UnsafeStack>;

/// Racy, lock-free, allocation-free snapshot of the running task, at the
/// concrete kernel monomorphisation.
///
/// Exists so the fault handlers in `boot/` can get one without naming the
/// stack-handle types. See `slopos_ostd::task::diag` for what it promises.
#[inline]
pub fn current_task_diag() -> Option<slopos_ostd::task::TaskDiag> {
    slopos_ostd::task::current_task_diag::<KernelStack, UnsafeStack>()
}

// Bounded so the static array fits comfortably in `.bss` and heap callers
// budget a single-page allocation.
const _: () = assert!(core::mem::size_of::<Task>() <= 8192);

// `abi: TaskAbi` must be field #0 so OSTD's `TASK_UNSAFE_STACK_SP_OFFSET`,
// computed inside `TaskAbi`, matches the asm-readable offset of
// `unsafe_stack_sp` within `Task`.
const _: () = assert!(offset_of!(Task, abi) == 0);

const _: () = {
    // XSAVE requires 64-byte alignment.
    assert!(core::mem::align_of::<FpuState>() >= 64);
    assert!(FPU_STATE_SIZE >= FXSAVE_AREA_SIZE);
    assert!(FPU_STATE_SIZE % core::mem::align_of::<FpuState>() == 0);
};

/// Panics at boot if the CPU's XSAVE area exceeds our compile-time maximum.
///
/// Call once from a boot step after `xsave::init()`, to fail early rather than
/// silently corrupting adjacent task memory.
pub fn validate_fpu_state_size() {
    let hw_size = slopos_ostd::cpu::x86_64::xsave::area_size();
    assert!(
        hw_size <= FPU_STATE_SIZE,
        "XSAVE area size ({} B) exceeds compile-time FPU_STATE_SIZE ({} B) — \
         increase FPU_STATE_SIZE in the OSTD task module",
        hw_size,
        FPU_STATE_SIZE,
    );
}
