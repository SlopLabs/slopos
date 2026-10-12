//! Task ABI types shared between kernel and userland. Kernel-internal detail
//! (the `Task` struct, register contexts, FPU state, scheduler linkage) lives
//! in `slopos_sched::task_struct`.

/// Maximum number of concurrently live tasks. Capped by the kernel-stack VA
/// region (`mm::memory_layout_defs::KSTACK_MAX_SLOTS`), since every live task
/// needs a KSTACK slot. An upper bound, not a resident set: the task pool is
/// heap-backed and grows lazily.
pub const MAX_TASKS: usize = 8192;
/// Task kernel-mode stack: 32 KiB usable in a 64 KiB slot (4 KiB guard + 32 KiB
/// usable + 28 KiB reserve).
pub const TASK_STACK_SIZE: u64 = 0x8000;
pub const TASK_KERNEL_STACK_SIZE: u64 = 0x8000;

/// SafeStack-sanitizer data stack. LLVM's SafeStack pass moves every
/// address-taken local onto it, and the `&mut`-passing safe-helper style here
/// produces many. 16 KiB matches Linux's x86_64 `THREAD_SIZE`; at the
/// 8192-task ceiling it costs 128 MiB at peak.
pub const TASK_UNSAFE_STACK_SIZE: u64 = 0x4000;

pub const TASK_NAME_MAX_LEN: usize = 32;
pub const INVALID_TASK_ID: u32 = 0xFFFF_FFFF;
pub const INVALID_PROCESS_ID: u32 = 0xFFFF_FFFF;

/// Maximum number of concurrently live processes. The process registry, the
/// address-space table and the descriptor tables are all sized by this and key
/// on each other's slot indices, so the bound has to be one number.
///
/// Four times the appliance's 256, which one `just test` reached exactly.
/// Measured: 768 more slots cost 36 960 bytes of `.bss`, so what bounds this
/// is neither memory nor `PROCESS_SLOT_BITS` but how many processes a `-j N`
/// build of this repository wants at once.
pub const MAX_PROCESSES: usize = 1024;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskStatus {
    #[default]
    Invalid = 0,
    Ready = 1,
    Running = 2,
    Blocked = 3,
    /// Task has terminated and is reapable. Slot is eligible for tier-2
    /// reuse once `ref_count == 0`.
    Terminated = 4,
    /// Task has exited but still holds its exit info awaiting a `waitpid`
    /// from a live parent. Tier-2 slot reuse skips Zombie slots so the
    /// parent's reaper observes a stable `Task::exit_info`.
    Zombie = 5,
    /// Job control: every task in the thread group parked on a stop signal.
    /// Not runnable and not reapable; only a `SIGCONT` or a kill moves it.
    Stopped = 6,
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
            5 => Self::Zombie,
            6 => Self::Stopped,
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
            Self::Ready => matches!(
                target,
                Self::Running | Self::Stopped | Self::Terminated | Self::Zombie
            ),
            Self::Running => matches!(
                target,
                Self::Ready | Self::Blocked | Self::Stopped | Self::Terminated | Self::Zombie
            ),
            Self::Blocked => matches!(
                target,
                Self::Ready | Self::Stopped | Self::Terminated | Self::Zombie
            ),
            Self::Terminated => matches!(target, Self::Invalid | Self::Terminated),
            Self::Zombie => matches!(target, Self::Terminated | Self::Zombie),
            // Zombie is reachable because a kill can race the stop.
            Self::Stopped => matches!(
                target,
                Self::Ready | Self::Running | Self::Terminated | Self::Zombie
            ),
        }
    }
}

/// Discriminant `1` is retired and reserved.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlockReason {
    #[default]
    None = 0,
    Sleep = 2,
    IoWait = 3,
    MutexWait = 4,
    KeyboardWait = 5,
    IpcWait = 6,
    Generic = 7,
    FutexWait = 8,
}

impl BlockReason {
    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::None,
            2 => Self::Sleep,
            3 => Self::IoWait,
            4 => Self::MutexWait,
            5 => Self::KeyboardWait,
            6 => Self::IpcWait,
            7 => Self::Generic,
            8 => Self::FutexWait,
            _ => Self::None,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Scheduler priority class. Lower numeric value = higher priority. The repr
/// value is the index into the per-CPU `ready_queues` array, so adding a variant
/// requires bumping `NUM_PRIORITY_LEVELS` in the scheduler.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskPriority {
    /// Latency-critical work: compositor, RT kernel paths.
    High = 0,
    /// Kernel I/O kthreads (NAPI, net-timer, …), above any user task. Never
    /// selectable from user space; `slopos_ostd::task::spawn_kernel_io` is the
    /// only spawn surface.
    KernelIo = 1,
    /// Default class for ordinary user tasks and kernel threads.
    #[default]
    Normal = 2,
    /// Background work that should yield to anything else.
    Low = 3,
    /// Per-CPU idle loop only — never used by user-spawned tasks.
    Idle = 4,
}

impl TaskPriority {
    /// Total decoder: out-of-range values coerce to `Normal`. For trusted
    /// kernel-internal reads of a `u8` the kernel itself wrote.
    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::High,
            1 => Self::KernelIo,
            2 => Self::Normal,
            3 => Self::Low,
            4 => Self::Idle,
            _ => Self::Normal,
        }
    }

    /// Strict decoder for the syscall boundary: rejects untrusted input rather
    /// than silently coercing it.
    #[inline]
    pub const fn try_from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::High),
            1 => Some(Self::KernelIo),
            2 => Some(Self::Normal),
            3 => Some(Self::Low),
            4 => Some(Self::Idle),
            _ => None,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn queue_index(self) -> usize {
        self as usize
    }
}

pub const TASK_FLAG_USER_MODE: u16 = 0x01;
pub const TASK_FLAG_KERNEL_MODE: u16 = 0x02;
pub const TASK_FLAG_NO_PREEMPT: u16 = 0x04;
/// Place the spawned task into its own process group (`pgid = task_id`) instead
/// of inheriting the parent's, closing the SMP race between spawn and the
/// parent's `setpgid` + `tcsetpgrp` calls.
pub const TASK_FLAG_NEW_PGRP: u16 = 0x80;
/// Make the spawned task's process group the foreground group of its inherited
/// controlling terminal *before* the task becomes schedulable, closing the race
/// against the parent's `tcsetpgrp`. Only honoured when the child's session
/// matches the terminal's controlling session.
pub const TASK_FLAG_FOREGROUND: u16 = 0x100;

// The four masks below partition the word, so "may a caller set this bit?" is
// answered once, here.

/// Flag bits a `spawn_path` caller may set for its own child. `NEW_PGRP` only
/// mints a group inside the parent's own session and `FOREGROUND` is
/// re-validated against the terminal's controlling session, so neither hands the
/// child authority the parent did not already hold.
pub const SPAWN_USER_SETTABLE: u16 = TASK_FLAG_NEW_PGRP | TASK_FLAG_FOREGROUND;

/// Flag bits only the kernel sets, refused from a spawn with `EPERM`. The timer
/// tick and the deferred post-IRQ reschedule both return early for a task
/// carrying `NO_PREEMPT`, so one non-preemptible spinner pinned per CPU wedges
/// the machine.
pub const SPAWN_PRIVILEGED: u16 = TASK_FLAG_NO_PREEMPT;

/// The two ring bits. `USER_MODE` is forced on regardless, so a caller that
/// sets it is redundant rather than wrong; `KERNEL_MODE` is refused with
/// `EINVAL` here because `task_build`'s own refusal reaches the exec layer only
/// as `NoMem`.
pub const SPAWN_MODE_BITS: u16 = TASK_FLAG_USER_MODE | TASK_FLAG_KERNEL_MODE;

/// Undefined flag bits, all failing closed with `EINVAL`: `0x0040` is the
/// retired `TASK_FLAG_FPU_INITIALIZED`, and the rest are the privileges that
/// moved to the capability mask. A literal on purpose: deriving it from the
/// complement of the other three would make the partition assert below
/// unfailable, which is the only reason that assert exists.
pub const SPAWN_RESERVED: u16 = 0xFE78;

const _: () = assert!(
    (SPAWN_USER_SETTABLE | SPAWN_PRIVILEGED | SPAWN_MODE_BITS | SPAWN_RESERVED) == u16::MAX,
    "spawn flag classes must cover all 16 bits",
);
const _: () = assert!((SPAWN_USER_SETTABLE & SPAWN_PRIVILEGED) == 0);
const _: () = assert!((SPAWN_USER_SETTABLE & SPAWN_MODE_BITS) == 0);
const _: () = assert!((SPAWN_PRIVILEGED & SPAWN_MODE_BITS) == 0);
const _: () = assert!(
    (SPAWN_RESERVED & (SPAWN_USER_SETTABLE | SPAWN_PRIVILEGED | SPAWN_MODE_BITS)) == 0,
    "a defined flag bit is also marked reserved",
);

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskExitReason {
    #[default]
    None = 0,
    Normal = 1,
    UserFault = 2,
    Kernel = 3,
    /// Killed by an uncaught signal whose default action terminates. Distinct
    /// from [`Normal`](Self::Normal) because `waitpid` must report the signal
    /// number, and `exit(139)` is not a segfault.
    Signalled = 4,
}

impl TaskExitReason {
    /// Widen to the `AtomicU16` storage this lives in on `TaskInner`.
    #[inline]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Narrow back, saturating an unrecognised encoding to `None`. Total rather
    /// than fallible: the only writer is the kernel itself.
    #[inline]
    pub const fn from_u16(raw: u16) -> Self {
        match raw {
            1 => Self::Normal,
            2 => Self::UserFault,
            3 => Self::Kernel,
            4 => Self::Signalled,
            _ => Self::None,
        }
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskFaultReason {
    #[default]
    None = 0,
    UserPage = 1,
    UserGp = 2,
    UserUd = 3,
    UserDeviceNa = 4,
    /// The task's own write found no page and the OOM killer had nothing it
    /// could take for it. Reported as `SIGKILL`, the signal the killer's
    /// victims die of, and distinct from [`UserPage`](Self::UserPage) so
    /// `waitpid` can tell a machine short of memory from a wrong program. A
    /// kernel write made for the task that finds the same, or cannot wait for
    /// the killer, answers a user copy's `EFAULT` or fails a signal frame's
    /// push into `SIGSEGV`.
    UserOom = 5,
}

impl TaskFaultReason {
    /// See [`TaskExitReason::as_u16`].
    #[inline]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// See [`TaskExitReason::from_u16`].
    #[inline]
    pub const fn from_u16(raw: u16) -> Self {
        match raw {
            1 => Self::UserPage,
            2 => Self::UserGp,
            3 => Self::UserUd,
            4 => Self::UserDeviceNa,
            5 => Self::UserOom,
            _ => Self::None,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskExitRecord {
    pub task_id: u32,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
}

impl TaskExitRecord {
    pub const fn empty() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
        }
    }
}
