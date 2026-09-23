//! POSIX signal ABI definitions shared between kernel and userland.

/// Signals are numbered 1..NSIG; signal 0 is reserved for error checking in kill().
pub const NSIG: usize = 32;

// Numbering follows the POSIX / Linux-compatible subset.

pub const SIGHUP: u8 = 1;
pub const SIGINT: u8 = 2;
pub const SIGQUIT: u8 = 3;
pub const SIGILL: u8 = 4;
pub const SIGTRAP: u8 = 5;
pub const SIGABRT: u8 = 6;
pub const SIGBUS: u8 = 7;
pub const SIGFPE: u8 = 8;
pub const SIGKILL: u8 = 9;
pub const SIGUSR1: u8 = 10;
pub const SIGSEGV: u8 = 11;
pub const SIGUSR2: u8 = 12;
pub const SIGPIPE: u8 = 13;
pub const SIGALRM: u8 = 14;
pub const SIGTERM: u8 = 15;
// 16 is unused
pub const SIGCHLD: u8 = 17;
pub const SIGCONT: u8 = 18;
pub const SIGSTOP: u8 = 19;
pub const SIGTSTP: u8 = 20;
pub const SIGTTIN: u8 = 21;
pub const SIGTTOU: u8 = 22;
pub const SIGWINCH: u8 = 28;

/// Bit N is signal N+1: bit 0 = SIGHUP. Signal 0 does not exist.
pub type SigSet = u64;

pub const SIG_EMPTY: SigSet = 0;

/// One drained signal, returned by `read()` on a `FileKind::Signalfd`. SlopOS's
/// analogue of Linux `struct signalfd_siginfo`, trimmed to 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignalfdSiginfo {
    /// Signal number (1-based).
    pub ssi_signo: u32,
    /// Signal-specific code (0 — SlopOS does not track si_code yet).
    pub ssi_code: i32,
    /// Sending task id, when known (0 otherwise).
    pub ssi_pid: u32,
    pub _pad: u32,
}

impl SignalfdSiginfo {
    pub const SERIALIZED_LEN: usize = 16;

    /// Fixed-width little-endian byte image for the `read()` copy-out.
    pub fn to_bytes(&self) -> [u8; Self::SERIALIZED_LEN] {
        let mut b = [0u8; Self::SERIALIZED_LEN];
        b[0..4].copy_from_slice(&self.ssi_signo.to_le_bytes());
        b[4..8].copy_from_slice(&self.ssi_code.to_le_bytes());
        b[8..12].copy_from_slice(&self.ssi_pid.to_le_bytes());
        b
    }
}

const _: () = assert!(core::mem::size_of::<SignalfdSiginfo>() == SignalfdSiginfo::SERIALIZED_LEN);

/// Convert a signal number (1-based) to its bitmask.
#[inline]
pub const fn sig_bit(signum: u8) -> SigSet {
    if signum == 0 || signum as usize > NSIG {
        0
    } else {
        1u64 << (signum - 1)
    }
}

/// Every bit `sig_bit` can produce: signals `1..=NSIG` occupy bits `0..NSIG`.
///
/// Bits at and above `NSIG` are kernel-private and must be masked off before a
/// signal number is derived from a pending set: an unmasked one yields
/// `signum = NSIG + 1`, for which [`sig_bit`] returns 0 — so the bit never
/// clears — and it indexes past a `[_; NSIG]` table.
pub const SIGNAL_MASK: SigSet = (1u64 << NSIG) - 1;

/// Kernel-private: the task is marked for death and every blocking primitive
/// must abort rather than park. Outside [`SIGNAL_MASK`] deliberately, so it is
/// invisible to `kill`, `sigprocmask`, `sigaction`, `signalfd` and delivery, and
/// unreachable from userland — [`sig_bit`] cannot produce it.
pub const SIGNAL_KILLED: SigSet = 1u64 << NSIG;

const _: () = assert!(SIGNAL_KILLED & SIGNAL_MASK == 0);
const _: () = assert!(sig_bit(NSIG as u8) & SIGNAL_MASK != 0);

/// Signals that cannot be caught, blocked, or ignored.
pub const SIG_UNCATCHABLE: SigSet = sig_bit(SIGKILL) | sig_bit(SIGSTOP);

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

pub const SA_RESTORER: u64 = 0x04000000;
pub const SA_SIGINFO: u64 = 0x00000004;
pub const SA_ONSTACK: u64 = 0x08000000;
pub const SA_NODEFER: u64 = 0x40000000;
pub const SA_RESETHAND: u64 = 0x80000000;
pub const SA_RESTART: u64 = 0x10000000;

/// `sigaltstack` flags. `SS_ONSTACK` is output-only: a caller cannot claim it.
pub const SS_ONSTACK: u32 = 1;
pub const SS_DISABLE: u32 = 2;

/// Smallest alternate stack the kernel will accept. Above Linux's x86-64
/// `MINSIGSTKSZ` of 2048 deliberately: the frame this kernel pushes does not
/// fit in 2 KiB. Equal to Linux's `SIGSTKSZ`, so a userland sizing its stack
/// the usual way is unaffected.
pub const MINSIGSTKSZ: usize = 8192;

/// `sigaltstack(2)` descriptor. Linux `stack_t`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSigAltStack {
    pub ss_sp: u64,
    pub ss_flags: u32,
    pub _pad: u32,
    pub ss_size: u64,
}

const _: () = assert!(
    core::mem::size_of::<UserSigAltStack>() == 24,
    "UserSigAltStack must match the Linux x86-64 stack_t"
);

/// `si_code` values this kernel produces. Linux numbering.
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
/// SIGSEGV: address not mapped to an object.
pub const SEGV_MAPERR: i32 = 1;
/// SIGSEGV: mapped, but the access was not permitted.
pub const SEGV_ACCERR: i32 = 2;
/// SIGBUS: object-specific hardware error.
pub const BUS_OBJERR: i32 = 3;
/// SIGILL: illegal opcode.
pub const ILL_ILLOPC: i32 = 1;

/// Byte offsets into [`UserSiginfo`]'s `_sifields` union, Linux x86-64's.
///
/// Exported because the union is a word array rather than named fields, so a
/// consumer that cannot name those fields — `libc`'s own `siginfo_t`, which
/// does not depend on this crate — has nothing to point `offset_of!` at. The
/// asserts below hold them to the struct's real shape.
pub const SI_ADDR_OFFSET: usize = 16;
/// `si_pid` — `_sifields._kill._pid`, overlapping [`SI_ADDR_OFFSET`].
pub const SI_PID_OFFSET: usize = 16;
/// `si_uid` — `_sifields._kill._uid`.
pub const SI_UID_OFFSET: usize = 20;
/// `si_status` — `_sifields._sigchld._status`, the union's second word. This
/// kernel delivers no `SIGCHLD` `siginfo`, so it always reads 0.
pub const SI_STATUS_OFFSET: usize = 24;

/// The `siginfo_t` an `SA_SIGINFO` handler receives. Linux x86-64's layout:
/// three `int`s, four bytes of padding, then the 112-byte `_sifields` union —
/// 128 bytes in all.
///
/// The union is a word array behind an accessor rather than a Rust `union`:
/// this crate is `#![forbid(unsafe_code)]`, and reading a union field is
/// `unsafe`. Word 0 is the overlap that matters — `si_addr` for a fault
/// signal, `si_pid`/`si_uid` for a `kill`-originated one. The rest of the
/// union stays zero, as Linux's tail padding is.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSiginfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    _pad0: i32,
    _sifields: [u64; 14],
}

impl UserSiginfo {
    /// The `siginfo` for `si_signo`. `si_addr` is the faulting address for
    /// `SIGSEGV`/`SIGBUS`/`SIGILL`, and 0 for every other signal — which is
    /// the same word `si_pid`/`si_uid` read as 0 from.
    #[inline]
    pub const fn new(si_signo: i32, si_code: i32, si_addr: u64) -> Self {
        let mut sifields = [0u64; 14];
        sifields[0] = si_addr;
        Self {
            si_signo,
            si_errno: 0,
            si_code,
            _pad0: 0,
            _sifields: sifields,
        }
    }

    /// The `siginfo` for a signal process `si_pid` sent: `si_pid` and `si_uid`
    /// share the union's first word, low half and high half.
    #[inline]
    pub const fn sent(si_signo: i32, si_code: i32, si_pid: u32, si_uid: u32) -> Self {
        Self::new(si_signo, si_code, (si_pid as u64) | ((si_uid as u64) << 32))
    }

    /// The sending process of a `kill`-originated signal.
    #[inline]
    pub const fn si_pid(&self) -> u32 {
        self._sifields[0] as u32
    }

    /// The faulting address a `SIGSEGV`/`SIGBUS`/`SIGILL` handler reads.
    #[inline]
    pub const fn si_addr(&self) -> u64 {
        self._sifields[0]
    }
}

const _: () = assert!(
    core::mem::size_of::<UserSiginfo>() == 128,
    "UserSiginfo must match the Linux x86-64 siginfo_t size"
);
const _: () = assert!(
    core::mem::align_of::<UserSiginfo>() == 8,
    "UserSiginfo must match the Linux x86-64 siginfo_t alignment"
);
const _: () = assert!(core::mem::offset_of!(UserSiginfo, si_signo) == 0);
const _: () = assert!(core::mem::offset_of!(UserSiginfo, si_errno) == 4);
const _: () = assert!(core::mem::offset_of!(UserSiginfo, si_code) == 8);
// Four bytes of padding before the union is what puts `si_addr` at 16 rather
// than at 24 behind an `si_pid`/`si_uid` pair laid out as struct fields.
const _: () = assert!(core::mem::offset_of!(UserSiginfo, _sifields) == SI_ADDR_OFFSET);
const _: () = assert!(SI_ADDR_OFFSET == 16);
const _: () = assert!(SI_PID_OFFSET == SI_ADDR_OFFSET);
const _: () = assert!(SI_UID_OFFSET == SI_PID_OFFSET + 4);
const _: () = assert!(SI_STATUS_OFFSET == SI_ADDR_OFFSET + 8);

/// The machine state an `SA_SIGINFO` handler receives as its third argument.
/// Layout is the leading part of the Linux x86-64 `ucontext_t`: the
/// `uc_mcontext` register block at offset 40.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserUcontext {
    pub uc_flags: u64,
    pub uc_link: u64,
    pub uc_stack: UserSigAltStack,
    pub uc_mcontext_gregs: [u64; 23],
    pub uc_sigmask: SigSet,
}

/// Linux's `REG_*` indices into [`UserUcontext::uc_mcontext_gregs`].
pub const REG_R8: usize = 0;
pub const REG_R9: usize = 1;
pub const REG_R10: usize = 2;
pub const REG_R11: usize = 3;
pub const REG_R12: usize = 4;
pub const REG_R13: usize = 5;
pub const REG_R14: usize = 6;
pub const REG_R15: usize = 7;
pub const REG_RDI: usize = 8;
pub const REG_RSI: usize = 9;
pub const REG_RBP: usize = 10;
pub const REG_RBX: usize = 11;
pub const REG_RDX: usize = 12;
pub const REG_RAX: usize = 13;
pub const REG_RCX: usize = 14;
pub const REG_RSP: usize = 15;
pub const REG_RIP: usize = 16;
pub const REG_EFL: usize = 17;
pub const REG_CSGSFS: usize = 18;
pub const REG_ERR: usize = 19;
pub const REG_TRAPNO: usize = 20;
pub const REG_OLDMASK: usize = 21;
pub const REG_CR2: usize = 22;

/// User-visible sigaction passed via the `rt_sigaction` syscall. Layout matches
/// the Linux x86-64 `struct sigaction`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserSigaction {
    /// Signal handler function pointer, or SIG_DFL / SIG_IGN.
    pub sa_handler: u64,
    pub sa_flags: u64,
    /// Restorer function pointer (called after handler returns via SA_RESTORER).
    pub sa_restorer: u64,
    /// Signal mask to apply while handler is executing.
    pub sa_mask: SigSet,
}

impl UserSigaction {
    pub const fn default() -> Self {
        Self {
            sa_handler: SIG_DFL,
            sa_flags: 0,
            sa_restorer: 0,
            sa_mask: SIG_EMPTY,
        }
    }
}

// `how` values for rt_sigprocmask.
pub const SIG_BLOCK: u32 = 0;
pub const SIG_UNBLOCK: u32 = 1;
pub const SIG_SETMASK: u32 = 2;

#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum SigDefault {
    Terminate = 0,
    Ignore = 1,
    /// Stop every task in the thread group until a `SIGCONT` arrives.
    Stop = 2,
    /// Resume a stopped thread group.
    Continue = 3,
}

/// Default disposition per the POSIX default-action table; everything else,
/// including any unknown signal number, terminates the process.
pub const fn sig_default_action(signum: u8) -> SigDefault {
    match signum {
        SIGCHLD | SIGWINCH => SigDefault::Ignore,
        SIGCONT => SigDefault::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => SigDefault::Stop,
        _ => SigDefault::Terminate,
    }
}

/// True when `signum`'s default disposition is `Ignore`. The send-time drop
/// check uses this so a default-ignored signal never spuriously wakes a blocked
/// task. `Stop` and `Continue` are excluded deliberately: those are delivered.
pub const fn sig_default_ignores(signum: u8) -> bool {
    matches!(sig_default_action(signum), SigDefault::Ignore)
}

/// `waitpid(2)` status encoding, as every libc's `W*` macros read it.
pub const fn wait_status_exited(code: u8) -> u32 {
    (code as u32) << 8
}

pub const fn wait_status_signalled(signum: u8) -> u32 {
    (signum & 0x7f) as u32
}

pub const fn wait_status_stopped(signum: u8) -> u32 {
    (((signum & 0xff) as u32) << 8) | 0x7f
}

pub const WAIT_STATUS_CONTINUED: u32 = 0xffff;

/// `waitpid(2)` `options` bits. Linux values.
pub const WNOHANG: u32 = 1;
/// Also report a child that stopped and has not been reported yet.
pub const WUNTRACED: u32 = 2;
/// Also report a child that was resumed by `SIGCONT`.
pub const WCONTINUED: u32 = 8;
pub const WAIT_OPTIONS_MASK: u32 = WNOHANG | WUNTRACED | WCONTINUED;

/// Signal frame pushed onto the user stack when delivering a signal;
/// `rt_sigreturn` restores from it. The restorer address is pushed as a separate
/// 8-byte word *before* the frame (Linux convention), so the handler's `ret` pops
/// it into RIP and leaves RSP pointing at this frame.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SignalFrame {
    pub signum: u64,
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
    /// Saved instruction pointer (where to resume after sigreturn).
    pub rip: u64,
    pub rflags: u64,
    /// Saved signal mask (restored by sigreturn).
    pub saved_mask: SigSet,
}
