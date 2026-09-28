//! POSIX signal ABI definitions shared between kernel and userland.

/// Signals are numbered `1..=NSIG`; signal 0 is `kill()`'s existence probe.
pub const NSIG: usize = 64;

/// Realtime signals: each instance queues with its own `siginfo`. A libc may
/// reserve the lowest few for itself.
pub const SIGRTMIN: u8 = 32;
pub const SIGRTMAX: u8 = NSIG as u8;

/// Realtime instances one pending set, a process's or a thread's, may hold.
/// See [`SigInfo::survives_queue_overflow`] for a send past it.
pub const SIGQUEUE_MAX: usize = 32;

#[inline]
pub const fn sig_is_realtime(signum: u8) -> bool {
    signum >= SIGRTMIN && signum <= SIGRTMAX
}

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

/// What a pending signal instance carries: `si_code`, the sender's
/// `si_pid`/`si_uid`, and `si_value` (queued) or `si_status` (`SIGCHLD`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigInfo {
    pub code: i32,
    pub pid: u32,
    pub uid: u32,
    pub _pad: u32,
    pub value: u64,
}

impl SigInfo {
    pub const KERNEL: Self = Self {
        code: SI_KERNEL,
        pid: 0,
        uid: 0,
        _pad: 0,
        value: 0,
    };

    #[inline]
    pub const fn sent(code: i32, pid: u32, uid: u32, value: u64) -> Self {
        Self {
            code,
            pid,
            uid,
            _pad: 0,
            value,
        }
    }

    /// Whether an instance past [`SIGQUEUE_MAX`] pends without its record
    /// rather than failing `EAGAIN`: Linux lets `kill` and kernel signals
    /// overflow.
    #[inline]
    pub const fn survives_queue_overflow(&self) -> bool {
        self.code == SI_USER || self.code == SI_KERNEL
    }
}

/// One signal as `read()` on a signalfd returns it: Linux x86-64's
/// `struct signalfd_siginfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalfdSiginfo {
    pub ssi_signo: u32,
    pub ssi_errno: i32,
    pub ssi_code: i32,
    pub ssi_pid: u32,
    pub ssi_uid: u32,
    pub ssi_fd: i32,
    pub ssi_tid: u32,
    pub ssi_band: u32,
    pub ssi_overrun: u32,
    pub ssi_trapno: u32,
    pub ssi_status: i32,
    pub ssi_int: i32,
    pub ssi_ptr: u64,
    pub ssi_utime: u64,
    pub ssi_stime: u64,
    pub ssi_addr: u64,
    pub ssi_addr_lsb: u16,
    pub _pad: [u8; 46],
}

impl SignalfdSiginfo {
    pub const SERIALIZED_LEN: usize = 128;

    /// The record for `signo` taken with `info`: the union word goes to
    /// `ssi_status` for a child's report, `ssi_int`/`ssi_ptr` for a sender's.
    pub const fn new(signo: u8, info: &SigInfo) -> Self {
        let child = signo == SIGCHLD && info.code > 0;
        let queued = info.code < 0;
        Self {
            ssi_signo: signo as u32,
            ssi_errno: 0,
            ssi_code: info.code,
            ssi_pid: info.pid,
            ssi_uid: info.uid,
            ssi_fd: 0,
            ssi_tid: 0,
            ssi_band: 0,
            ssi_overrun: 0,
            ssi_trapno: 0,
            ssi_status: if child { info.value as i32 } else { 0 },
            ssi_int: if queued { info.value as i32 } else { 0 },
            ssi_ptr: if queued { info.value } else { 0 },
            ssi_utime: 0,
            ssi_stime: 0,
            ssi_addr: 0,
            ssi_addr_lsb: 0,
            _pad: [0; 46],
        }
    }

    /// Fixed-width little-endian byte image for the `read()` copy-out.
    pub fn to_bytes(&self) -> [u8; Self::SERIALIZED_LEN] {
        let mut b = [0u8; Self::SERIALIZED_LEN];
        b[0..4].copy_from_slice(&self.ssi_signo.to_le_bytes());
        b[4..8].copy_from_slice(&self.ssi_errno.to_le_bytes());
        b[8..12].copy_from_slice(&self.ssi_code.to_le_bytes());
        b[12..16].copy_from_slice(&self.ssi_pid.to_le_bytes());
        b[16..20].copy_from_slice(&self.ssi_uid.to_le_bytes());
        b[20..24].copy_from_slice(&self.ssi_fd.to_le_bytes());
        b[24..28].copy_from_slice(&self.ssi_tid.to_le_bytes());
        b[28..32].copy_from_slice(&self.ssi_band.to_le_bytes());
        b[32..36].copy_from_slice(&self.ssi_overrun.to_le_bytes());
        b[36..40].copy_from_slice(&self.ssi_trapno.to_le_bytes());
        b[40..44].copy_from_slice(&self.ssi_status.to_le_bytes());
        b[44..48].copy_from_slice(&self.ssi_int.to_le_bytes());
        b[48..56].copy_from_slice(&self.ssi_ptr.to_le_bytes());
        b[56..64].copy_from_slice(&self.ssi_utime.to_le_bytes());
        b[64..72].copy_from_slice(&self.ssi_stime.to_le_bytes());
        b[72..80].copy_from_slice(&self.ssi_addr.to_le_bytes());
        b[80..82].copy_from_slice(&self.ssi_addr_lsb.to_le_bytes());
        b
    }
}

const _: () = assert!(core::mem::size_of::<SignalfdSiginfo>() == SignalfdSiginfo::SERIALIZED_LEN);
const _: () = assert!(core::mem::offset_of!(SignalfdSiginfo, ssi_status) == 40);
const _: () = assert!(core::mem::offset_of!(SignalfdSiginfo, ssi_ptr) == 48);
const _: () = assert!(core::mem::offset_of!(SignalfdSiginfo, ssi_addr_lsb) == 80);

/// Convert a signal number (1-based) to its bitmask.
#[inline]
pub const fn sig_bit(signum: u8) -> SigSet {
    if signum == 0 || signum as usize > NSIG {
        0
    } else {
        1u64 << (signum - 1)
    }
}

const _: () = assert!(NSIG == SigSet::BITS as usize);
const _: () = assert!(sig_bit(NSIG as u8) == 1 << 63);

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

/// `si_code` values, Linux numbering. Codes at or above 0 are the kernel's
/// alone to write.
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_QUEUE: i32 = -1;
pub const SI_TKILL: i32 = -6;
/// `SIGCHLD` codes.
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;
pub const CLD_DUMPED: i32 = 3;
pub const CLD_TRAPPED: i32 = 4;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;
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
/// `si_status` — `_sifields._sigchld._status`, the union's second word.
pub const SI_STATUS_OFFSET: usize = 24;
/// `si_value` — `_sifields._rt._sigval`, the same word.
pub const SI_VALUE_OFFSET: usize = 24;

/// The `siginfo_t` an `SA_SIGINFO` handler receives. Linux x86-64's layout:
/// three `int`s, four bytes of padding, then the 112-byte `_sifields` union —
/// 128 bytes in all.
///
/// The union is a word array behind an accessor rather than a Rust `union`:
/// this crate is `#![forbid(unsafe_code)]`, and reading a union field is
/// `unsafe`. Word 0 is `si_addr` for a fault signal and `si_pid`/`si_uid`
/// for a sent one; word 1 is `si_value` or `si_status`. The rest of the union
/// stays zero, as Linux's tail padding is.
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

    /// The `siginfo` a handler receives for an instance that carried `info`.
    #[inline]
    pub const fn from_info(si_signo: i32, info: &SigInfo) -> Self {
        let mut out = Self::new(
            si_signo,
            info.code,
            (info.pid as u64) | ((info.uid as u64) << 32),
        );
        out._sifields[1] = info.value;
        out
    }

    /// The sending process of a sent signal, or the child of a `SIGCHLD`.
    #[inline]
    pub const fn si_pid(&self) -> u32 {
        self._sifields[0] as u32
    }

    #[inline]
    pub const fn si_uid(&self) -> u32 {
        (self._sifields[0] >> 32) as u32
    }

    /// `si_value`, as `sival_ptr`; `sival_int` is its low half.
    #[inline]
    pub const fn si_value(&self) -> u64 {
        self._sifields[1]
    }

    #[inline]
    pub const fn si_status(&self) -> i32 {
        self._sifields[1] as i32
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
const _: () = assert!(SI_VALUE_OFFSET == SI_STATUS_OFFSET);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signalfd_record_carries_the_value_of_every_sender_supplied_code() {
        const SI_TIMER: i32 = -2;
        const SI_MESGQ: i32 = -3;
        const SI_ASYNCIO: i32 = -4;
        for code in [SI_QUEUE, SI_TIMER, SI_MESGQ, SI_ASYNCIO] {
            let info = SigInfo::sent(code, 7, 0, 0x1_2345_6789);
            let record = SignalfdSiginfo::new(SIGRTMIN, &info);
            let handler = UserSiginfo::from_info(SIGRTMIN as i32, &info);
            assert_eq!(record.ssi_ptr, handler.si_value(), "code {code}");
            assert_eq!(record.ssi_int, 0x2345_6789, "code {code}");
        }
        let kill = SignalfdSiginfo::new(SIGUSR1, &SigInfo::sent(SI_USER, 7, 0, 0));
        assert_eq!((kill.ssi_int, kill.ssi_ptr), (0, 0));
    }
}
