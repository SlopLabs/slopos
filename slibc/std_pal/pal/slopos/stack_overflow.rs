//! Stack-overflow reporting for SlopOS.
//!
//! slibc maps a `PROT_NONE` guard page below every thread stack, so running off
//! the bottom of one is a SIGSEGV. Reporting it needs an alternate signal
//! stack: the stack that ran out cannot host the handler.

#![allow(dead_code)]

const SIGSEGV: i32 = 11;
const SIGBUS: i32 = 7;

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;

const STDERR: i32 = 2;

/// `si_addr`'s byte offset in `slopos_abi::signal::UserSiginfo`.
const SI_ADDR_OFFSET: usize = 24;

const SIG_DFL: usize = 0;
const SIG_UNBLOCK: i32 = 1;

/// The kernel's `MINSIGSTKSZ` is 8 KiB; the rest is the handler's own.
const SIGSTKSZ: usize = 16 * 1024;

unsafe extern "C" {
    fn slopos_sigaction_onstack(
        signum: i32,
        handler: unsafe extern "C" fn(i32, *mut u8, *mut u8),
    ) -> i32;
    fn slopos_sigaltstack_install(sp: *mut u8, size: usize) -> i32;
    fn slopos_sigaltstack_disable() -> i32;
    fn slopos_thread_stack_guard(lo: *mut u64, hi: *mut u64) -> i32;
    fn slopos_mmap(
        addr: *mut u8,
        len: usize,
        prot: u64,
        flags: u64,
        fd: i32,
        offset: u64,
    ) -> *mut u8;
    fn slopos_munmap(addr: *mut u8, len: usize) -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn signal(signum: i32, handler: usize) -> usize;
    fn sigprocmask(how: i32, set: *const u64, oldset: *mut u64) -> i32;
    fn raise(sig: i32) -> i32;
    fn abort() -> !;
}

/// Owns one thread's alternate signal stack.
pub struct Handler {
    data: *mut u8,
    size: usize,
}

impl Handler {
    pub unsafe fn new() -> Handler {
        unsafe { make_handler(false) }
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        if !self.data.is_null() {
            unsafe {
                let _ = slopos_sigaltstack_disable();
                let _ = slopos_munmap(self.data, self.size);
            }
        }
    }
}

pub unsafe fn init() {
    unsafe {
        let _ = slopos_sigaction_onstack(SIGSEGV, fault_handler);
        let _ = slopos_sigaction_onstack(SIGBUS, fault_handler);
    }
}

pub unsafe fn cleanup() {}

pub unsafe fn make_handler(_main_thread: bool) -> Handler {
    let data = unsafe {
        slopos_mmap(
            core::ptr::null_mut(),
            SIGSTKSZ,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if data.is_null() {
        return Handler {
            data: core::ptr::null_mut(),
            size: 0,
        };
    }
    if unsafe { slopos_sigaltstack_install(data, SIGSTKSZ) } != 0 {
        unsafe {
            let _ = slopos_munmap(data, SIGSTKSZ);
        }
        return Handler {
            data: core::ptr::null_mut(),
            size: 0,
        };
    }
    Handler {
        data,
        size: SIGSTKSZ,
    }
}

fn report(msg: &[u8]) {
    unsafe {
        let _ = write(STDERR, msg.as_ptr(), msg.len());
    }
}

/// Runs on the alternate stack: async-signal-safe only — no allocation, no
/// locks, no thread name lookup.
///
/// Re-raises rather than exiting, because `abort()` here would turn every
/// fault into a normal exit status and hide a `SIGSEGV` death from `waitpid`.
unsafe extern "C" fn fault_handler(signum: i32, info: *mut u8, _ctx: *mut u8) {
    let mut lo: u64 = 0;
    let mut hi: u64 = 0;
    let guarded = unsafe { slopos_thread_stack_guard(&mut lo, &mut hi) } == 0;
    let addr = if info.is_null() {
        0
    } else {
        unsafe { core::ptr::read_unaligned(info.add(SI_ADDR_OFFSET) as *const u64) }
    };

    if guarded && addr >= lo && addr < hi {
        report(b"\nthread has overflowed its stack\n");
    } else if signum == SIGSEGV {
        report(b"\nfatal runtime error: SIGSEGV\n");
    } else {
        report(b"\nfatal runtime error: SIGBUS\n");
    }

    unsafe { die_of(signum) }
}

/// Unblocking matters: delivery blocked `signum` on the way in, so the
/// re-raised signal would otherwise only be acted on after the handler
/// returned into the instruction that faulted.
unsafe fn die_of(signum: i32) -> ! {
    unsafe {
        signal(signum, SIG_DFL);
        let unblock: u64 = 1u64 << ((signum - 1) as u32);
        sigprocmask(SIG_UNBLOCK, &unblock, core::ptr::null_mut());
        raise(signum);
        // A default disposition that terminates never comes back; a kernel that
        // somehow declined the signal must not fall through into the fault.
        abort()
    }
}
