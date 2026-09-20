//! Thread Control Block — per-thread state anchored by FS_BASE.

use core::ptr;

pub const PTHREAD_KEYS_MAX: usize = 64;

/// Room for the longest string [`crate::error::SyscallError::as_str`] answers
/// ("Invalid or incomplete multibyte or wide character", 49 bytes) and its NUL.
pub const STRERROR_BUF: usize = 64;

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
    /// `strerror`'s per-thread answer. Not a `#[thread_local]`: LLVM can
    /// speculate `@llvm.threadlocal.address` above the `tls_is_initialized`
    /// test that guards it, and [`Tcb::current`]'s `asm!` it cannot.
    pub strerror_buf: [u8; STRERROR_BUF],
    pub thread_local_keys: [*mut u8; PTHREAD_KEYS_MAX],
    /// `pthread_setname_np`'s NUL-padded name.
    pub name: [u8; super::PTHREAD_NAME_MAX],
    /// Bytes of [`crate::thread::create::THREAD_STACK_GUARD_SIZE`] actually
    /// mprotected at the low end of `stack_base`, which an attr with
    /// `guardsize == 0` sets to zero. `pthread_getattr_np` reports it.
    pub guard_size: usize,
    /// This thread's dynamic thread vector: `dtv[0]` is the module count and
    /// `dtv[m]` module `m`'s block. Null until TLS is installed.
    pub dtv: *mut usize,
    /// The TLS allocation's base. The thread pointer is `tls_block + tls_size`
    /// in the variant-II layout, so freeing the TCB address would hand the
    /// allocator the middle of a chunk.
    pub tls_block: *mut u8,
    /// This thread's `uselocale` handle; null is the global locale.
    pub locale: crate::locale::object::locale_t,
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
            strerror_buf: [0; STRERROR_BUF],
            thread_local_keys: [ptr::null_mut(); PTHREAD_KEYS_MAX],
            name: [0; super::PTHREAD_NAME_MAX],
            guard_size: 0,
            dtv: ptr::null_mut(),
            tls_block: ptr::null_mut(),
            locale: ptr::null_mut(),
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
