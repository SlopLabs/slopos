use core::mem;
use core::ptr;

use crate::errno;
use crate::pal::{Pal, Sys};
use slopos_abi::PAGE_SIZE;
use slopos_abi::syscall::{
    CLONE_CHILD_CLEARTID, CLONE_FILES, CLONE_FS, CLONE_PARENT_SETTID, CLONE_SETTLS, CLONE_SIGHAND,
    CLONE_THREAD, CLONE_VM, MAP_ANONYMOUS, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE,
};

use super::keys::run_key_destructors;
use super::tcb::Tcb;
use super::tls::tls_init_new_thread;
use super::{DEFAULT_STACK_SIZE, pthread_attr_t, pthread_t};

/// Unmapped page at the low end of every thread stack. Without it a stack
/// overflow silently writes into whatever the allocator put below.
///
/// Callers learn the range through `pthread_getattr_np` plus
/// `pthread_attr_getstack`/`pthread_attr_getguardsize`, which is where a
/// stack-overflow handler looks for it.
pub const THREAD_STACK_GUARD_SIZE: usize = PAGE_SIZE as usize;

const CLONE_THREAD_FLAGS: u64 = CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_THREAD
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_CLEARTID;

unsafe extern "C" fn thread_trampoline(tcb_raw: *mut u8) -> ! {
    let tcb = tcb_raw as *mut Tcb;
    tls_init_new_thread(tcb);

    let start: unsafe extern "C" fn(*mut u8) -> *mut u8 = mem::transmute((*tcb).start_fn);
    let arg = (*tcb).start_arg;

    let ret = start(arg);
    (*tcb).retval = ret;

    crate::cxa::run_thread_destructors();
    run_key_destructors(tcb);
    crate::mem::tcache::release(tcb);
    Sys::exit(0)
}

/// x86_64 clone wrapper handling parent/child fork in assembly.
///
/// Child pops `(arg, func)` from its new stack and calls `func(arg)`.
/// Parent returns the child TID (or negative errno).
///
/// # Calling convention (System V AMD64)
/// rdi=flags, rsi=stack, rdx=ptid, rcx=ctid, r8=tls, r9=func, [rsp+8]=arg
#[unsafe(naked)]
unsafe extern "C" fn raw_clone(
    _flags: u64,
    _stack: *mut u8,
    _ptid: *mut i32,
    _ctid: *mut i32,
    _tls: u64,
    _func: usize,
    _arg: *mut u8,
) -> i64 {
    core::arch::naked_asm!(
        "mov rax, [rsp + 8]",
        "and rsi, -16",
        "sub rsi, 16",
        "mov [rsi], rax",
        "mov [rsi + 8], r9",
        "mov r10, rcx",
        "mov eax, {clone}",
        "syscall",
        "test rax, rax",
        "jz 2f",
        "ret",
        "2:",
        "xor ebp, ebp",
        "pop rdi",
        "pop rax",
        "and rsp, -16",
        "call rax",
        "xor edi, edi",
        "mov eax, {exit}",
        "syscall",
        "ud2",
        clone = const slopos_abi::syscall::SYSCALL_CLONE,
        exit = const slopos_abi::syscall::SYSCALL_EXIT,
    )
}

/// # Safety
/// All pointer arguments must be valid. `start` must be a valid function pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    attr: *const pthread_attr_t,
    start: unsafe extern "C" fn(*mut u8) -> *mut u8,
    arg: *mut u8,
) -> i32 {
    let stack_size = if attr.is_null() {
        DEFAULT_STACK_SIZE
    } else {
        let sz = (*attr).stack_size;
        if sz == 0 { DEFAULT_STACK_SIZE } else { sz }
    };

    let detach = if attr.is_null() {
        false
    } else {
        (*attr).detach_state == super::PTHREAD_CREATE_DETACHED
    };

    // A default attr, and a null one, carry the guard page; only an explicit
    // `pthread_attr_setguardsize(attr, 0)` drops it.
    let guard_size = if attr.is_null() {
        THREAD_STACK_GUARD_SIZE
    } else if (*attr).guard_size == 0 && (*attr).stack_size != 0 {
        0
    } else {
        THREAD_STACK_GUARD_SIZE
    };

    // The guard page is part of the mapping, and `stack_size` in the TCB is the
    // whole mapping, so the teardown `munmap` still covers it.
    let mapped_size = stack_size + guard_size;
    let stack_base = match Sys::mmap(
        ptr::null_mut(),
        mapped_size,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    ) {
        Ok(p) => p,
        Err(e) => {
            errno::errno_set(e.raw());
            return e.raw();
        }
    };

    if guard_size != 0 {
        if let Err(e) = Sys::mprotect(stack_base, guard_size, PROT_NONE) {
            let _ = Sys::munmap(stack_base, mapped_size);
            errno::errno_set(e.raw());
            return e.raw();
        }
    }

    // A full block (TLS image + TCB) is what leaves the new thread's `.tbss`
    // thread-locals zero-initialized.
    let (_tls_base, tcb_ptr) = super::tls::alloc_thread_tls();
    if tcb_ptr.is_null() {
        let _ = Sys::munmap(stack_base, mapped_size);
        return crate::errno::ENOMEM.raw();
    }

    // `alloc_thread_tls` already zeroed the TCB.
    (*tcb_ptr).self_ptr = tcb_ptr;
    (*tcb_ptr).stack_base = stack_base;
    (*tcb_ptr).stack_size = mapped_size;
    (*tcb_ptr).start_fn = start as usize;
    (*tcb_ptr).start_arg = arg;
    (*tcb_ptr).detached = detach;
    (*tcb_ptr).child_tid = -1;
    (*tcb_ptr).guard_size = guard_size;

    let stack_top = stack_base.add(mapped_size);

    let ret = raw_clone(
        CLONE_THREAD_FLAGS,
        stack_top,
        &raw mut (*tcb_ptr).tid,
        &raw mut (*tcb_ptr).child_tid,
        tcb_ptr as u64,
        thread_trampoline as *const () as usize,
        tcb_ptr as *mut u8,
    );

    if ret < 0 {
        let err = (-ret) as i32;
        super::tls::free_thread_tls(tcb_ptr);
        let _ = Sys::munmap(stack_base, mapped_size);
        errno::errno_set(err);
        return err;
    }

    if !thread.is_null() {
        *thread = tcb_ptr as pthread_t;
    }
    0
}
