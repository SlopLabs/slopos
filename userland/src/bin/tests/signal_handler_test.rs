//! libc signal()/sigaction() handler-install end-to-end test.
//!
//! The kernel rejects a catchable handler whose `sa_restorer` is 0, so libc
//! must inject its own restorer trampoline. These cases install a handler
//! through libc, `raise()` the signal, and observe a flag the handler sets.

use slopos_userland as _;

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::signal::{UserSigaction, UserSiginfo};
use slopos_slibc::signal::{self, SIG_DFL, SIGUSR1, SIGUSR2};
use slopos_slibc::types::{sigaction as SigAction, sigset_t as SigSet};

static SIGUSR1_COUNT: AtomicU32 = AtomicU32::new(0);
static SIGUSR2_COUNT: AtomicU32 = AtomicU32::new(0);
static CLOBBER_RAN: AtomicU32 = AtomicU32::new(0);
static MXCSR_CLOBBER_RAN: AtomicU32 = AtomicU32::new(0);

/// Backing store for a hand-built signal frame. Static rather than a local so
/// pointing RSP at it cannot land the kernel's stack view inside a live frame.
#[repr(C, align(16))]
struct SigreturnScratch([u8; 4096]);

static mut SIGRETURN_SCRATCH: SigreturnScratch = SigreturnScratch([0; 4096]);

extern "C" fn on_sigusr1(_sig: i32) {
    SIGUSR1_COUNT.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn on_sigusr2(_sig: i32) {
    SIGUSR2_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Poisons the vector register file: if the kernel preserves the interrupted
/// task's FPU/vector state, sigreturn undoes it and the caller never sees it.
extern "C" fn on_clobber_vectors(_sig: i32) {
    CLOBBER_RAN.fetch_add(1, Ordering::SeqCst);
    let poison: [u32; 4] = [0xDEAD_BEEF; 4];
    // SAFETY: overwrites xmm0..xmm7 with poison; all are caller-saved in
    // the SysV ABI, so the handler may clobber them freely.
    unsafe {
        core::arch::asm!(
            "movups xmm0, [{p}]",
            "movups xmm1, [{p}]",
            "movups xmm2, [{p}]",
            "movups xmm3, [{p}]",
            "movups xmm4, [{p}]",
            "movups xmm5, [{p}]",
            "movups xmm6, [{p}]",
            "movups xmm7, [{p}]",
            p = in(reg) poison.as_ptr(),
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
        );
    }
}

/// `signal()` must install a real handler rather than fail with SIG_ERR.
fn test_signal_installs_and_delivers() -> bool {
    SIGUSR1_COUNT.store(0, Ordering::SeqCst);

    let prev = unsafe { signal::signal(SIGUSR1, on_sigusr1 as *const () as usize) };
    if prev == usize::MAX {
        eprintln!("signal_handler_test: signal() returned SIG_ERR (install rejected)");
        return false;
    }

    if unsafe { signal::raise(SIGUSR1) } != 0 {
        eprintln!("signal_handler_test: raise(SIGUSR1) failed");
        return false;
    }

    let count = SIGUSR1_COUNT.load(Ordering::SeqCst);
    if count != 1 {
        eprintln!("signal_handler_test: handler ran {count} times, expected 1");
        return false;
    }

    // Restore default so a stray later signal doesn't re-enter the handler.
    let _ = unsafe { signal::signal(SIGUSR1, SIG_DFL) };
    true
}

/// `sigaction()` with a null `sa_restorer` on a real handler must have libc
/// substitute its own restorer (glibc behavior) so the install succeeds.
///
/// The struct is libc's 152-byte one, not the kernel's 32-byte
/// `UserSigaction`: narrowing between the two is slibc's job, and this is the
/// call that exercises it.
fn test_sigaction_injects_restorer() -> bool {
    SIGUSR2_COUNT.store(0, Ordering::SeqCst);

    let act = SigAction {
        sa_sigaction: on_sigusr2 as *const () as usize,
        sa_mask: SigSet::empty(),
        sa_flags: 0,
        sa_restorer: None,
    };

    let rc = unsafe { signal::sigaction(SIGUSR2, &act, core::ptr::null_mut()) };
    if rc != 0 {
        eprintln!("signal_handler_test: sigaction() returned {rc} (restorer not injected)");
        return false;
    }

    if unsafe { signal::raise(SIGUSR2) } != 0 {
        eprintln!("signal_handler_test: raise(SIGUSR2) failed");
        return false;
    }

    let count = SIGUSR2_COUNT.load(Ordering::SeqCst);
    if count != 1 {
        eprintln!("signal_handler_test: sigaction handler ran {count} times, expected 1");
        return false;
    }

    let _ = unsafe { signal::signal(SIGUSR2, SIG_DFL) };
    true
}

/// The kernel must preserve a task's vector (XMM/YMM) registers across signal
/// delivery. The kill is issued inline in the asm block so the loaded pattern
/// stays live in xmm0..xmm3 across delivery.
fn test_signal_preserves_vector_regs() -> bool {
    CLOBBER_RAN.store(0, Ordering::SeqCst);
    let prev = unsafe { signal::signal(SIGUSR1, on_clobber_vectors as *const () as usize) };
    if prev == usize::MAX {
        eprintln!("signal_handler_test: vector-preserve handler install rejected");
        return false;
    }

    let pid = unsafe { slopos_slibc::process::getpid() } as u64;
    let pattern: [u32; 16] = [
        0x1111_1111,
        0x2222_2222,
        0x3333_3333,
        0x4444_4444,
        0x5555_5555,
        0x6666_6666,
        0x7777_7777,
        0x8888_8888,
        0x9999_9999,
        0xAAAA_AAAA,
        0xBBBB_BBBB,
        0xCCCC_CCCC,
        0xDDDD_DDDD,
        0xEEEE_EEEE,
        0x0F0F_0F0F,
        0xF0F0_F0F0,
    ];
    let mut result: [u32; 16] = [0; 16];

    // SAFETY: one asm block so the vector regs stay live across delivery:
    // load xmm0..3 from `pattern`, run kill(pid, SIGUSR1) inline, then store
    // xmm0..3 into `result`. rcx/r11 are syscall-clobbered; xmm0..3 are
    // listed as outputs so the compiler reloads from `result`.
    unsafe {
        core::arch::asm!(
            "movups xmm0, [{pat}]",
            "movups xmm1, [{pat} + 16]",
            "movups xmm2, [{pat} + 32]",
            "movups xmm3, [{pat} + 48]",
            "syscall",
            "movups [{res}], xmm0",
            "movups [{res} + 16], xmm1",
            "movups [{res} + 32], xmm2",
            "movups [{res} + 48], xmm3",
            pat = in(reg) pattern.as_ptr(),
            res = in(reg) result.as_mut_ptr(),
            inout("rax") slopos_abi::syscall::SYSCALL_KILL => _,
            in("rdi") pid,
            in("rsi") SIGUSR1 as u64,
            out("rcx") _,
            out("r11") _,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
        );
    }

    let _ = unsafe { signal::signal(SIGUSR1, SIG_DFL) };

    if CLOBBER_RAN.load(Ordering::SeqCst) != 1 {
        eprintln!("signal_handler_test: clobber handler did not run exactly once");
        return false;
    }
    if result != pattern {
        eprintln!("signal_handler_test: vector regs corrupted across signal: {result:08x?}");
        return false;
    }
    true
}

/// Signal handler that leaves MXCSR at the plain default, so a test can tell
/// whether sigreturn put the interrupted code's own value back.
extern "C" fn on_clobber_mxcsr(_sig: i32) {
    MXCSR_CLOBBER_RAN.fetch_add(1, Ordering::SeqCst);
    let plain: u32 = 0x1F80;
    // SAFETY: `plain` is a live, 4-byte-aligned u32; MXCSR is caller-saved.
    unsafe {
        core::arch::asm!("ldmxcsr [{}]", in(reg) &plain, options(nostack));
    }
}

/// This CPU's `MXCSR_MASK`, read the way the kernel reads it.
fn mxcsr_mask() -> u32 {
    #[repr(C, align(16))]
    struct FxsaveArea([u8; 512]);

    let mut area = FxsaveArea([0u8; 512]);
    // SAFETY: a live, exclusively-borrowed, 16-byte-aligned 512-byte buffer,
    // which is exactly what `fxsave64` writes.
    unsafe {
        core::arch::asm!("fxsave64 [{}]", in(reg) area.0.as_mut_ptr(), options(nostack));
    }
    let mask = u32::from_le_bytes([area.0[28], area.0[29], area.0[30], area.0[31]]);
    if mask == 0 { 0xFFBF } else { mask }
}

/// `rt_sigreturn` must reject a signal frame it never wrote, rather than
/// feeding the bytes that follow it to `XRSTOR64` in ring 0.
///
/// Every byte of the frame is `0xFF`, so the XSTATE header names components
/// XCR0 does not enable and the MXCSR word is nothing but reserved bits. Issued
/// as a raw syscall with RSP pointed at the frame, the only way to reach
/// `rt_sigreturn` without a signal having been delivered.
///
/// The process surviving is half the assertion: EFAULT must be returned
/// *without* committing the frame's registers, or execution would resume at an
/// all-ones RIP instead of the instruction after `syscall`.
fn test_sigreturn_rejects_poisoned_frame() -> bool {
    let frame = &raw mut SIGRETURN_SCRATCH as *mut u8;
    // SAFETY: `frame` names a live static of exactly this size, and nothing
    // else refers to it.
    unsafe {
        core::ptr::write_bytes(frame, 0xFF, core::mem::size_of::<SigreturnScratch>());
    }

    let ret: i64;
    // SAFETY: RSP is pointed at the crafted frame only for the duration of the
    // syscall and restored from a callee-saved register immediately after. The
    // kernel reads the frame and never writes the user stack on this path.
    unsafe {
        core::arch::asm!(
            "mov r12, rsp",
            "mov rsp, {frame}",
            "syscall",
            "mov rsp, r12",
            frame = in(reg) frame,
            inout("rax") slopos_abi::syscall::SYSCALL_RT_SIGRETURN => ret,
            out("rcx") _,
            out("r11") _,
            out("r12") _,
        );
    }

    if ret != -14 {
        eprintln!("signal_handler_test: rt_sigreturn(poisoned frame) returned {ret}, want -EFAULT");
        return false;
    }
    true
}

/// A *valid* sigreturn must round-trip MXCSR, including the bits this CPU
/// implements beyond the classic mask.
///
/// The kernel validates the frame's MXCSR word against the CPU's own
/// `MXCSR_MASK`; a fixed mask would clear DAZ. The handler resets MXCSR to the
/// plain default, so the value only comes back if sigreturn restored it.
fn test_signal_preserves_mxcsr() -> bool {
    MXCSR_CLOBBER_RAN.store(0, Ordering::SeqCst);
    let prev = unsafe { signal::signal(SIGUSR1, on_clobber_mxcsr as *const () as usize) };
    if prev == usize::MAX {
        eprintln!("signal_handler_test: mxcsr-clobber handler install rejected");
        return false;
    }

    // All exceptions masked, plus DAZ and FTZ where the CPU implements them.
    let want: u32 = (0x1F80 | (1 << 6) | (1 << 15)) & mxcsr_mask();
    let pid = unsafe { slopos_slibc::process::getpid() } as u64;
    let mut saved: u32 = 0;
    let mut readback: u32 = 0;

    // SAFETY: one asm block so MXCSR stays live across delivery — set it, run
    // kill(pid, SIGUSR1) inline, read it back, then put the caller's value
    // back before returning to compiled code.
    unsafe {
        core::arch::asm!(
            "stmxcsr [{saved}]",
            "ldmxcsr [{want}]",
            "syscall",
            "stmxcsr [{res}]",
            "ldmxcsr [{saved}]",
            saved = in(reg) &mut saved,
            want = in(reg) &want,
            res = in(reg) &mut readback,
            inout("rax") slopos_abi::syscall::SYSCALL_KILL => _,
            in("rdi") pid,
            in("rsi") SIGUSR1 as u64,
            out("rcx") _,
            out("r11") _,
        );
    }

    let _ = unsafe { signal::signal(SIGUSR1, SIG_DFL) };

    if MXCSR_CLOBBER_RAN.load(Ordering::SeqCst) != 1 {
        eprintln!("signal_handler_test: mxcsr-clobber handler did not run exactly once");
        return false;
    }
    if readback != want {
        eprintln!("signal_handler_test: MXCSR came back {readback:#06x}, want {want:#06x}");
        return false;
    }
    true
}

/// `rt_sigaction` must take every signal up to 64, the last realtime one, and
/// reject a number past it with `EINVAL` rather than indexing off the end of
/// the 64-entry action table.
///
/// Issued as a raw syscall because libc's `sigaction()` never produces one; the
/// query-only form (`new == 0`, `old != 0`) reaches the table read before any
/// other validation.
fn test_sigaction_rejects_signum_past_nsig() -> bool {
    let mut old = UserSigaction {
        sa_handler: 0,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };
    for (signum, want) in [(32u64, 0i64), (64, 0), (65, -22), (128, -22)] {
        let ret = unsafe {
            slopos_slibc::pal::raw::syscall4(
                slopos_abi::syscall::SYSCALL_RT_SIGACTION,
                signum,
                0,
                &mut old as *mut UserSigaction as u64,
                core::mem::size_of::<u64>() as u64,
            )
        } as i64;
        if ret != want {
            eprintln!("signal_handler_test: rt_sigaction({signum}) returned {ret}, want {want}");
            return false;
        }
    }
    true
}

static QUEUED_SEEN: AtomicU32 = AtomicU32::new(0);
/// Per delivery: signal, `si_code`, `si_pid`, `si_value`'s `sival_int`.
static QUEUED_LOG: [[AtomicU32; 4]; 4] = [const { [const { AtomicU32::new(0) }; 4] }; 4];

extern "C" fn on_queued(sig: i32, info: *mut UserSiginfo, _uc: *mut core::ffi::c_void) {
    let at = QUEUED_SEEN.fetch_add(1, Ordering::SeqCst) as usize;
    if at >= QUEUED_LOG.len() || info.is_null() {
        return;
    }
    // SAFETY: the kernel hands an `SA_SIGINFO` handler a live siginfo.
    let info = unsafe { &*info };
    let row = &QUEUED_LOG[at];
    row[0].store(sig as u32, Ordering::SeqCst);
    row[1].store(info.si_code as u32, Ordering::SeqCst);
    row[2].store(info.si_pid(), Ordering::SeqCst);
    row[3].store(info.si_value() as u32, Ordering::SeqCst);
}

/// `sigqueue` round trip: three instances queued while blocked — two of
/// `SIGRTMIN`, one of `SIGRTMIN+1`, sent in the order +1, 0, 0 — arrive on
/// unblock lowest signal first and FIFO within one, each with its own
/// `si_value`, `SI_QUEUE` and this process as `si_pid`.
fn test_sigqueue_delivers_every_instance_in_order() -> bool {
    use slopos_slibc::signal::{__libc_current_sigrtmin, SI_QUEUE};
    use slopos_slibc::types::sigval;

    QUEUED_SEEN.store(0, Ordering::SeqCst);
    let rtmin = __libc_current_sigrtmin();
    let act = SigAction {
        sa_sigaction: on_queued as *const () as usize,
        sa_mask: SigSet::empty(),
        sa_flags: slopos_abi::signal::SA_SIGINFO as i32,
        sa_restorer: None,
    };
    let mut block = SigSet::empty();
    unsafe {
        if signal::sigaction(rtmin, &act, core::ptr::null_mut()) != 0
            || signal::sigaction(rtmin + 1, &act, core::ptr::null_mut()) != 0
        {
            eprintln!("signal_handler_test: installing the realtime handlers failed");
            return false;
        }
        signal::sigaddset(&mut block, rtmin);
        signal::sigaddset(&mut block, rtmin + 1);
        signal::sigprocmask(signal::SIG_BLOCK, &block, core::ptr::null_mut());
    }
    let me = unsafe { slopos_slibc::process::getpid() };
    let sent = [(rtmin + 1, 3), (rtmin, 1), (rtmin, 2)].iter().all(
        |&(sig, value)| unsafe { signal::sigqueue(me, sig, sigval { sival_int: value }) } == 0,
    );
    let early = QUEUED_SEEN.load(Ordering::SeqCst);
    unsafe {
        signal::sigprocmask(signal::SIG_UNBLOCK, &block, core::ptr::null_mut());
        let _ = signal::signal(rtmin, SIG_DFL);
        let _ = signal::signal(rtmin + 1, SIG_DFL);
    }
    if !sent || early != 0 {
        eprintln!("signal_handler_test: sigqueue failed ({sent}) or a blocked one ran ({early})");
        return false;
    }
    let seen = QUEUED_SEEN.load(Ordering::SeqCst);
    let want = [(rtmin, 1), (rtmin, 2), (rtmin + 1, 3)];
    if seen != 3 {
        eprintln!("signal_handler_test: {seen} queued deliveries, want 3");
        return false;
    }
    for (row, (sig, value)) in QUEUED_LOG.iter().zip(want) {
        let got = [0, 1, 2, 3].map(|i| row[i].load(Ordering::SeqCst));
        if got != [sig as u32, SI_QUEUE as u32, me as u32, value as u32] {
            eprintln!(
                "signal_handler_test: delivery {got:?}, want signal {sig} code {SI_QUEUE} pid {me} value {value}"
            );
            return false;
        }
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "sigqueue_delivers_every_instance_in_order",
        test_sigqueue_delivers_every_instance_in_order,
    ),
    (
        "signal_installs_and_delivers",
        test_signal_installs_and_delivers,
    ),
    (
        "sigaction_injects_restorer",
        test_sigaction_injects_restorer,
    ),
    (
        "signal_preserves_vector_regs",
        test_signal_preserves_vector_regs,
    ),
    (
        "sigaction_rejects_signum_past_nsig",
        test_sigaction_rejects_signum_past_nsig,
    ),
    (
        "sigreturn_rejects_poisoned_frame",
        test_sigreturn_rejects_poisoned_frame,
    ),
    ("signal_preserves_mxcsr", test_signal_preserves_mxcsr),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
