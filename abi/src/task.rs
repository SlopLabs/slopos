//! Task ABI types shared between kernel subsystems.
//!
//! This module is the single source of truth for task-related types and
//! constants. All subsystems (scheduler, syscall, boot, mm) import from
//! here rather than defining their own copies.
//!
//! # Layout contracts
//!
//! Several types have `#[repr(C)]` or `#[repr(C, packed)]` layouts that
//! are relied upon by assembly code in `core/context_switch.s` and
//! `core/src/scheduler/switch_asm.rs`. Compile-time assertions verify
//! that offsets match the assembly expectations.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, Ordering};

// --- Task Configuration ---

pub const MAX_TASKS: usize = 32;
pub const TASK_STACK_SIZE: u64 = 0x8000; // 32 KiB
pub const TASK_KERNEL_STACK_SIZE: u64 = 0x8000; // 32 KiB
pub const TASK_NAME_MAX_LEN: usize = 32;
pub const INVALID_TASK_ID: u32 = 0xFFFF_FFFF;
pub const INVALID_PROCESS_ID: u32 = 0xFFFF_FFFF;

// --- TaskStatus ---

/// Type-safe task status with explicit state-machine semantics.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskStatus {
    /// Task slot is not in use.
    #[default]
    Invalid = 0,
    /// Task is ready to run, waiting in a run queue.
    Ready = 1,
    /// Task is currently executing on a CPU.
    Running = 2,
    /// Task is blocked waiting for some event.
    Blocked = 3,
    /// Task has terminated and is awaiting cleanup.
    Terminated = 4,
}

impl TaskStatus {
    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Invalid,
            1 => Self::Ready,
            2 => Self::Running,
            3 => Self::Blocked,
            4 => Self::Terminated,
            _ => Self::Invalid,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn can_transition_to(self, target: Self) -> bool {
        match self {
            Self::Invalid => matches!(target, Self::Ready),
            Self::Ready => matches!(target, Self::Running | Self::Terminated),
            Self::Running => matches!(target, Self::Ready | Self::Blocked | Self::Terminated),
            Self::Blocked => matches!(target, Self::Ready | Self::Terminated),
            Self::Terminated => matches!(target, Self::Invalid | Self::Terminated),
        }
    }
}

// --- BlockReason ---

/// Reason why a task is in the Blocked state.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlockReason {
    #[default]
    None = 0,
    /// Target task ID stored in `waiting_on`.
    WaitingOnTask = 1,
    Sleep = 2,
    IoWait = 3,
    MutexWait = 4,
    KeyboardWait = 5,
    IpcWait = 6,
    Generic = 7,
}

impl BlockReason {
    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::WaitingOnTask,
            2 => Self::Sleep,
            3 => Self::IoWait,
            4 => Self::MutexWait,
            5 => Self::KeyboardWait,
            6 => Self::IpcWait,
            7 => Self::Generic,
            _ => Self::None,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

// --- Task Priority ---

pub const TASK_PRIORITY_HIGH: u8 = 0;
pub const TASK_PRIORITY_NORMAL: u8 = 1;
pub const TASK_PRIORITY_LOW: u8 = 2;
pub const TASK_PRIORITY_IDLE: u8 = 3;

// --- Task Flags ---

pub const TASK_FLAG_USER_MODE: u16 = 0x01;
pub const TASK_FLAG_KERNEL_MODE: u16 = 0x02;
pub const TASK_FLAG_NO_PREEMPT: u16 = 0x04;
pub const TASK_FLAG_SYSTEM: u16 = 0x08;
pub const TASK_FLAG_COMPOSITOR: u16 = 0x10;
pub const TASK_FLAG_DISPLAY_EXCLUSIVE: u16 = 0x20;
pub const TASK_FLAG_FPU_INITIALIZED: u16 = 0x40;

// --- TaskContext ---

/// CPU register state saved during context switches.
/// Size: 200 bytes (0xC8) — 25 × 8-byte registers.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct TaskContext {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub cs: u64,
    pub ds: u64,
    pub es: u64,
    pub fs: u64,
    pub gs: u64,
    pub ss: u64,
    pub cr3: u64,
}

impl TaskContext {
    /// All-zero context. Required for `const` contexts where `Default` is unavailable.
    pub const fn zero() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            rsp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags: 0,
            cs: 0,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
            ss: 0,
            cr3: 0,
        }
    }
}

// --- SwitchContext ---

use core::mem::offset_of;

/// Callee-saved register state for the software context switch.
///
/// Layout must match the assembly in `core/context_switch.s` and
/// `core/src/scheduler/switch_asm.rs`. Compile-time assertions below
/// verify every offset.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SwitchContext {
    pub rbx: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub rip: u64,
}

impl SwitchContext {
    /// Zero-initialized context with interrupts enabled (rflags = 0x202).
    pub const fn zero() -> Self {
        Self {
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: 0,
            rflags: 0x202,
            rip: 0,
        }
    }

    /// Create a context for a new task.
    ///
    /// The entry point is stored in r12, the argument in r13 (picked up by
    /// the trampoline in `switch_asm.rs`). `rsp` is set to `stack_top - 8`
    /// to simulate call-frame alignment. `rip` points at the trampoline.
    pub const fn new_for_task(entry_point: u64, arg: u64, stack_top: u64, trampoline: u64) -> Self {
        Self {
            rbx: 0,
            r12: entry_point,
            r13: arg,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: stack_top - 8,
            rflags: 0x202,
            rip: trampoline,
        }
    }
}

pub const SWITCH_CTX_OFF_RBX: usize = offset_of!(SwitchContext, rbx);
pub const SWITCH_CTX_OFF_R12: usize = offset_of!(SwitchContext, r12);
pub const SWITCH_CTX_OFF_R13: usize = offset_of!(SwitchContext, r13);
pub const SWITCH_CTX_OFF_R14: usize = offset_of!(SwitchContext, r14);
pub const SWITCH_CTX_OFF_R15: usize = offset_of!(SwitchContext, r15);
pub const SWITCH_CTX_OFF_RBP: usize = offset_of!(SwitchContext, rbp);
pub const SWITCH_CTX_OFF_RSP: usize = offset_of!(SwitchContext, rsp);
pub const SWITCH_CTX_OFF_RFLAGS: usize = offset_of!(SwitchContext, rflags);
pub const SWITCH_CTX_OFF_RIP: usize = offset_of!(SwitchContext, rip);

const _: () = assert!(core::mem::size_of::<SwitchContext>() == 72);

const _: () = {
    assert!(offset_of!(SwitchContext, rbx) == 0);
    assert!(offset_of!(SwitchContext, r12) == 8);
    assert!(offset_of!(SwitchContext, r13) == 16);
    assert!(offset_of!(SwitchContext, r14) == 24);
    assert!(offset_of!(SwitchContext, r15) == 32);
    assert!(offset_of!(SwitchContext, rbp) == 40);
    assert!(offset_of!(SwitchContext, rsp) == 48);
    assert!(offset_of!(SwitchContext, rflags) == 56);
    assert!(offset_of!(SwitchContext, rip) == 64);
};

// --- FpuState ---

pub const FPU_STATE_SIZE: usize = 512;
pub const MXCSR_DEFAULT: u32 = 0x1F80;

// FXSAVE area offsets (Intel SDM Vol. 1, Table 10-2).
const FXSAVE_FCW_OFFSET: usize = 0;
const FXSAVE_MXCSR_OFFSET: usize = 24;

/// FXSAVE area for x87/MMX/SSE state. Must be 16-byte aligned.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpuState {
    pub data: [u8; FPU_STATE_SIZE],
}

impl FpuState {
    pub const fn zero() -> Self {
        Self {
            data: [0u8; FPU_STATE_SIZE],
        }
    }

    /// Default FCW (0x037F) and MXCSR (0x1F80) — all exceptions masked.
    pub const fn new() -> Self {
        let mut state = Self::zero();
        state.data[FXSAVE_FCW_OFFSET] = 0x7F;
        state.data[FXSAVE_FCW_OFFSET + 1] = 0x03;
        state.data[FXSAVE_MXCSR_OFFSET] = 0x80;
        state.data[FXSAVE_MXCSR_OFFSET + 1] = 0x1F;
        state
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
}

impl Default for FpuState {
    fn default() -> Self {
        Self::new()
    }
}

// --- Task Exit/Fault Reason ---

/// Reason for task termination.
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskExitReason {
    #[default]
    None = 0,
    Normal = 1,
    UserFault = 2,
    Kernel = 3,
}

/// Specific fault that caused task termination.
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskFaultReason {
    #[default]
    None = 0,
    UserPage = 1,
    UserGp = 2,
    UserUd = 3,
    UserDeviceNa = 4,
}

// --- Task Struct ---

// Verify assembly FPU_STATE_OFFSET (0xD0) matches the actual field distance.
// Assembly in core/context_switch.s: `.equ FPU_STATE_OFFSET, 0xD0`
const _: () = assert!(offset_of!(Task, fpu_state) - offset_of!(Task, context) == 0xD0);

#[repr(C)]
pub struct Task {
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    state_atomic: AtomicU8,
    pub priority: u8,
    pub flags: u16,
    pub block_reason: BlockReason,
    _pad0: [u8; 3],
    pub process_id: u32,
    pub stack_base: u64,
    pub stack_size: u64,
    pub stack_pointer: u64,
    pub kernel_stack_base: u64,
    pub kernel_stack_top: u64,
    pub kernel_stack_size: u64,
    pub entry_point: u64,
    pub entry_arg: *mut c_void,
    pub context: TaskContext,
    pub fpu_state: FpuState,
    pub time_slice: u64,
    pub time_slice_remaining: u64,
    pub total_runtime: u64,
    pub creation_time: u64,
    pub yield_count: u32,
    pub last_run_timestamp: u64,
    pub waiting_on: AtomicU32,
    pub user_started: u8,
    pub context_from_user: u8,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
    pub fate_token: u32,
    pub fate_value: u32,
    pub fate_pending: u8,
    pub cpu_affinity: u32,
    pub last_cpu: u8,
    pub migration_count: u32,
    pub switch_ctx: SwitchContext,
    pub next_ready: *mut Task,
    /// Linkage for remote wake inbox (separate from ready-queue linkage).
    pub next_inbox: AtomicPtr<Task>,
    /// Reference count for safe deferred reclamation.
    pub refcnt: AtomicU32,
}

impl Task {
    pub const fn invalid() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            name: [0; TASK_NAME_MAX_LEN],
            state_atomic: AtomicU8::new(TaskStatus::Invalid.as_u8()),
            priority: TASK_PRIORITY_NORMAL,
            flags: 0,
            block_reason: BlockReason::None,
            _pad0: [0; 3],
            process_id: INVALID_PROCESS_ID,
            stack_base: 0,
            stack_size: 0,
            stack_pointer: 0,
            kernel_stack_base: 0,
            kernel_stack_top: 0,
            kernel_stack_size: 0,
            entry_point: 0,
            entry_arg: ptr::null_mut(),
            context: TaskContext::zero(),
            fpu_state: FpuState::new(),
            time_slice: 0,
            time_slice_remaining: 0,
            total_runtime: 0,
            creation_time: 0,
            yield_count: 0,
            last_run_timestamp: 0,
            waiting_on: AtomicU32::new(INVALID_TASK_ID),
            user_started: 0,
            context_from_user: 0,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
            fate_token: 0,
            fate_value: 0,
            fate_pending: 0,
            cpu_affinity: 0,
            last_cpu: 0,
            migration_count: 0,
            switch_ctx: SwitchContext::zero(),
            next_ready: ptr::null_mut(),
            next_inbox: AtomicPtr::new(ptr::null_mut()),
            refcnt: AtomicU32::new(0),
        }
    }

    #[inline]
    fn state(&self) -> u8 {
        self.state_atomic.load(Ordering::Acquire)
    }

    #[inline]
    fn set_state(&self, state: u8) {
        self.state_atomic.store(state, Ordering::Release);
    }

    #[inline]
    pub fn status(&self) -> TaskStatus {
        TaskStatus::from_u8(self.state())
    }

    #[inline]
    pub fn set_status(&self, status: TaskStatus) {
        self.set_state(status.as_u8());
    }

    #[inline]
    pub fn try_transition_to(&self, target: TaskStatus) -> bool {
        let current = self.state();
        let current_status = TaskStatus::from_u8(current);
        if !current_status.can_transition_to(target) {
            return false;
        }
        self.state_atomic
            .compare_exchange(current, target.as_u8(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[inline]
    pub fn mark_ready(&self) -> bool {
        self.try_transition_to(TaskStatus::Ready)
    }

    #[inline]
    pub fn mark_running(&self) -> bool {
        self.try_transition_to(TaskStatus::Running)
    }

    #[inline]
    pub fn block(&mut self, reason: BlockReason) -> bool {
        if self.try_transition_to(TaskStatus::Blocked) {
            self.block_reason = reason;
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn terminate(&self) -> bool {
        self.try_transition_to(TaskStatus::Terminated)
    }

    #[inline]
    pub fn is_blocked(&self) -> bool {
        self.status() == TaskStatus::Blocked
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        self.status() == TaskStatus::Ready
    }

    #[inline]
    pub fn is_running(&self) -> bool {
        self.status() == TaskStatus::Running
    }

    #[inline]
    pub fn is_terminated(&self) -> bool {
        self.status() == TaskStatus::Terminated
    }

    /// Copy all task state from `other` into `self`.
    ///
    /// Atomic fields are snapshot-copied. The reference count is reset to
    /// zero and linkage pointers are cleared — the new copy starts with no
    /// references and belongs to no queue.
    ///
    /// Field-by-field copy is required because `abi` forbids unsafe code.
    /// When adding new fields to `Task`, remember to update this method.
    pub fn clone_from(&mut self, other: &Task) {
        self.task_id = other.task_id;
        self.name = other.name;
        self.set_state(other.state());
        self.priority = other.priority;
        self.flags = other.flags;
        self.block_reason = other.block_reason;
        self._pad0 = other._pad0;
        self.process_id = other.process_id;
        self.stack_base = other.stack_base;
        self.stack_size = other.stack_size;
        self.stack_pointer = other.stack_pointer;
        self.kernel_stack_base = other.kernel_stack_base;
        self.kernel_stack_top = other.kernel_stack_top;
        self.kernel_stack_size = other.kernel_stack_size;
        self.entry_point = other.entry_point;
        self.entry_arg = other.entry_arg;
        self.context = other.context;
        self.fpu_state = other.fpu_state;
        self.time_slice = other.time_slice;
        self.time_slice_remaining = other.time_slice_remaining;
        self.total_runtime = other.total_runtime;
        self.creation_time = other.creation_time;
        self.yield_count = other.yield_count;
        self.last_run_timestamp = other.last_run_timestamp;
        self.waiting_on
            .store(other.waiting_on.load(Ordering::Acquire), Ordering::Release);
        self.user_started = other.user_started;
        self.context_from_user = other.context_from_user;
        self.exit_reason = other.exit_reason;
        self.fault_reason = other.fault_reason;
        self.exit_code = other.exit_code;
        self.fate_token = other.fate_token;
        self.fate_value = other.fate_value;
        self.fate_pending = other.fate_pending;
        self.cpu_affinity = other.cpu_affinity;
        self.last_cpu = other.last_cpu;
        self.migration_count = other.migration_count;
        self.switch_ctx = other.switch_ctx;
        // New copy: clean linkage and zero references.
        self.next_ready = ptr::null_mut();
        self.next_inbox = AtomicPtr::new(ptr::null_mut());
        self.refcnt = AtomicU32::new(0);
    }

    /// Increment reference count. Returns new count, saturating at `u32::MAX`.
    #[inline]
    pub fn inc_ref(&self) -> u32 {
        let prev = self.refcnt.load(Ordering::Acquire);
        if prev == u32::MAX {
            return u32::MAX;
        }
        self.refcnt.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Decrement reference count. Returns `true` if this was the last reference.
    ///
    /// Returns `false` without modifying the counter if it is already zero.
    #[inline]
    pub fn dec_ref(&self) -> bool {
        let prev = self.refcnt.load(Ordering::Acquire);
        if prev == 0 {
            return false;
        }
        self.refcnt.fetch_sub(1, Ordering::AcqRel) == 1
    }

    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.refcnt.load(Ordering::Acquire)
    }
}

// --- TaskExitRecord ---

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskExitRecord {
    pub task_id: u32,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
}

impl TaskExitRecord {
    /// Create an empty exit record.
    pub const fn empty() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
        }
    }
}
