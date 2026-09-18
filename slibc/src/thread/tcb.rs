//! Thread Control Block — per-thread state anchored by FS_BASE.

use core::ptr;

pub const PTHREAD_KEYS_MAX: usize = 64;

/// Per-thread state. `self_ptr` at offset 0 is required by the x86_64
/// TLS ABI (`mov rax, fs:[0]` must yield the TCB address).
#[repr(C)]
pub struct Tcb {
    pub self_ptr: *mut Tcb,
    pub errno_val: i32,
    pub tid: i32,
    pub stack_base: *mut u8,
    pub stack_size: usize,
    pub start_fn: usize,
    pub start_arg: *mut u8,
    pub retval: *mut u8,
    pub detached: bool,
    _pad: [u8; 3],
    /// Kernel writes 0 here on exit (`CLONE_CHILD_CLEARTID`) + futex-wakes it.
    pub child_tid: i32,
    pub tls_data: [u8; 64],
    pub thread_local_keys: [*mut u8; PTHREAD_KEYS_MAX],
    /// `pthread_setname_np`'s NUL-padded name. Appended last on purpose: the
    /// x86_64 TLS ABI fixes `self_ptr` at 0 and the rest of the offsets are
    /// what `thread::tests` pins, so growth has to happen at the tail.
    pub name: [u8; super::PTHREAD_NAME_MAX],
    /// Bytes of [`crate::thread::create::THREAD_STACK_GUARD_SIZE`] actually
    /// mprotected at the low end of `stack_base`, which an attr with
    /// `guardsize == 0` sets to zero. `pthread_getattr_np` reports it.
    pub guard_size: usize,
}

unsafe impl Send for Tcb {}
unsafe impl Sync for Tcb {}

impl Tcb {
    pub const fn zeroed() -> Self {
        Tcb {
            self_ptr: ptr::null_mut(),
            errno_val: 0,
            tid: 0,
            stack_base: ptr::null_mut(),
            stack_size: 0,
            start_fn: 0,
            start_arg: ptr::null_mut(),
            retval: ptr::null_mut(),
            detached: false,
            _pad: [0; 3],
            child_tid: 0,
            tls_data: [0; 64],
            thread_local_keys: [ptr::null_mut(); PTHREAD_KEYS_MAX],
            name: [0; super::PTHREAD_NAME_MAX],
            guard_size: 0,
        }
    }

    /// # Safety
    /// FS_BASE must point to a valid TCB (call `tls_is_initialized()` first).
    #[inline]
    pub unsafe fn current() -> *mut Tcb {
        let ptr: *mut Tcb;
        core::arch::asm!(
            "mov {}, fs:[0]",
            out(reg) ptr,
            options(nostack, pure, readonly)
        );
        ptr
    }

    /// # Safety
    /// Same as [`current()`].
    #[inline]
    pub unsafe fn errno_ptr() -> *mut i32 {
        let tcb = Self::current();
        &raw mut (*tcb).errno_val
    }
}
