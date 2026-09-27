//! What the allocator in [`super::heap`] needs of the system. The host tests
//! supply the same functions over `std`.

use core::ptr::null_mut;
use core::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

use slopos_abi::syscall::{
    MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE, SYSCALL_MMAP, SYSCALL_MUNMAP,
};

use super::heap::Heap;
use crate::pal::raw::{syscall2, syscall6};
use crate::thread::tcb::Tcb;
use crate::thread::tls::tls_is_initialized;

/// The heap slot of the one thread that runs before TLS exists. Its heap is
/// handed to the first thread that asks for one once TLS is up.
static PRE_TLS: AtomicPtr<Heap> = AtomicPtr::new(null_mut());

/// Zeroed read-write private memory, or null. Raw syscalls: errno is the
/// caller's to set.
pub fn map(len: usize) -> *mut u8 {
    // SAFETY: an anonymous mapping at an address of the kernel's choosing
    // touches no existing memory.
    let ret = unsafe {
        syscall6(
            SYSCALL_MMAP,
            0,
            len as u64,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            (-1i64) as u64,
            0,
        )
    };
    match crate::demux(ret) {
        Ok(addr) => addr as *mut u8,
        Err(_) => null_mut(),
    }
}

/// # Safety
/// `[p, p + len)` must be mapped and referenced by nothing.
pub unsafe fn unmap(p: *mut u8, len: usize) {
    let _ = syscall2(SYSCALL_MUNMAP, p as u64, len as u64);
}

/// The calling thread's heap slot.
#[inline]
pub fn heap_slot() -> *mut *mut Heap {
    if !tls_is_initialized() {
        return PRE_TLS.as_ptr();
    }
    // SAFETY: TLS is up, so `fs_base` is the calling thread's TCB.
    unsafe { &raw mut (*Tcb::current()).heap }
}

/// The heap made before TLS existed, once, to the first thread asking after.
pub fn take_orphan() -> *mut Heap {
    if !tls_is_initialized() {
        return null_mut();
    }
    PRE_TLS.swap(null_mut(), Ordering::AcqRel)
}

#[inline]
pub fn lock(state: &AtomicI32) {
    crate::thread::mutex::lock_state(state, crate::pal::FutexScope::Private);
}

#[inline]
pub fn unlock(state: &AtomicI32) {
    crate::thread::mutex::unlock_state(state, crate::pal::FutexScope::Private);
}

#[cold]
pub fn fatal(msg: &str) -> ! {
    let _ = <crate::pal::Sys as crate::pal::Pal>::write(2, msg.as_ptr(), msg.len());
    // SAFETY: `abort` takes no arguments and does not return.
    unsafe { crate::signal::abort() }
}
