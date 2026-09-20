//! `<setjmp.h>` — the non-local goto, in the System V x86-64 shape.
//!
//! What a jump has to restore is exactly what the ABI lets a function keep
//! across a call: the six callee-saved registers, the stack pointer, and the
//! address to resume at. Everything else either lives on the stack the
//! restored `rsp` re-establishes or was the callee's to clobber anyway.
//!
//! The x87 control word and `MXCSR` are callee-saved too, and neither call
//! touches them — as in glibc and musl, a `longjmp` out of a frame that
//! changed a rounding mode leaves the new mode in force.

#![allow(non_camel_case_types)]

use core::ffi::c_int;

use crate::signal::{SIG_SETMASK, sigprocmask};
use crate::types::sigset_t;

/// `struct __jmp_buf_tag`: the object glibc and musl both put behind
/// `jmp_buf`, so a translation unit compiled against either header agrees
/// with this one about all 200 bytes.
#[repr(C)]
pub struct JmpBufTag {
    pub core: [u64; 8],
    pub mask_was_saved: u64,
    pub mask: [u64; 16],
}

/// A `jmp_buf` is an array of one, which is what makes a C `jmp_buf`
/// parameter decay to the pointer these entry points take.
pub type jmp_buf = [JmpBufTag; 1];

/// Distinct name, identical object — as in glibc, where `sigjmp_buf` exists
/// to say which of the two `setjmp` flavours filled it and nothing more.
pub type sigjmp_buf = [JmpBufTag; 1];

// The asm spells the `core` slots as literal byte offsets and Rust reaches
// the mask half by name; both readings have to agree with the C object.
const _: () = assert!(size_of::<JmpBufTag>() == 200);
const _: () = assert!(align_of::<JmpBufTag>() == 8);
const _: () = assert!(core::mem::offset_of!(JmpBufTag, core) == 0);
const _: () = assert!(core::mem::offset_of!(JmpBufTag, mask_was_saved) == 64);
const _: () = assert!(core::mem::offset_of!(JmpBufTag, mask) == 72);
const _: () = assert!(size_of::<[u64; 16]>() == size_of::<sigset_t>());

/// `setjmp(3)`. Answers 0 on the way past and [`longjmp`]'s value on the way
/// back.
///
/// The signal mask is deliberately not saved. The C-standard `setjmp` has
/// never saved it on Linux — glibc's `<setjmp.h>` defines `setjmp(env)` as
/// `_setjmp(env)` — so saving one here would buy a syscall per call and
/// still not match what a program compiled against that header expects.
/// [`sigsetjmp`] is the one that saves.
///
/// Register contract: RDI = `env`. RDX is the scratch for the two computed
/// slots, RAX is the result, and no other register is disturbed.
///
/// # Safety
/// `env` must point at a writable [`JmpBufTag`]. Jumping back to it is
/// undefined once the frame that called `setjmp` has returned, and the jump
/// runs no destructor of any frame it skips, so a `Drop` value between the
/// two points leaks.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setjmp(_env: *mut JmpBufTag) -> c_int {
    core::arch::naked_asm!(
        "mov [rdi], rbx",
        "mov [rdi + 8], rbp",
        "mov [rdi + 16], r12",
        "mov [rdi + 24], r13",
        "mov [rdi + 32], r14",
        "mov [rdi + 40], r15",
        // Not this frame's `rsp` but the caller's: one slot up, past the
        // return address the `ret` below is about to pop.
        "lea rdx, [rsp + 8]",
        "mov [rdi + 48], rdx",
        "mov rdx, [rsp]",
        "mov [rdi + 56], rdx",
        // `siglongjmp` reads this slot unconditionally, so leaving it alone
        // would let an uninitialised `jmp_buf` install stack residue as a mask.
        "mov qword ptr [rdi + 64], 0",
        "xor eax, eax",
        "ret",
    );
}

/// `_setjmp(3)`, the name a C-standard `setjmp` macro expands to.
///
/// A tail jump rather than a call: it leaves RSP and the return address at
/// [RSP] exactly as this function was entered with, so what [`setjmp`]
/// captures is still this function's caller.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _setjmp(_env: *mut JmpBufTag) -> c_int {
    core::arch::naked_asm!("jmp {inner}", inner = sym setjmp);
}

/// `__setjmp`, the reserved-namespace spelling of the same entry point.
/// musl exports it too; a caller that was compiled against that name links.
/// `<setjmp.h>` does not declare it, because clang's builtin table carries
/// `returns_twice` for `setjmp`, `_setjmp` and `sigsetjmp` and not for this
/// spelling — as in glibc and musl, it is a link-time symbol only.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __setjmp(_env: *mut JmpBufTag) -> c_int {
    core::arch::naked_asm!("jmp {inner}", inner = sym setjmp);
}

/// `longjmp(3)`. Resumes the [`setjmp`] that filled `env`, which answers
/// `val`, or 1 when `val` is 0 — C forbids a second return of 0.
///
/// Register contract: RDI = `env`, ESI = `val`. RAX carries the result and
/// RDX the resume address; the transfer is an indirect jump because a `ret`
/// would consume the word the restored RSP points at, which belongs to the
/// frame being returned to.
///
/// # Safety
/// As [`setjmp`]: `env` must have been filled by a `setjmp` whose frame is
/// still live, and every frame skipped leaks whatever it was holding.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn longjmp(_env: *mut JmpBufTag, _val: c_int) -> ! {
    core::arch::naked_asm!(
        "mov eax, 1",
        "test esi, esi",
        "cmovnz eax, esi",
        "mov rbx, [rdi]",
        "mov rbp, [rdi + 8]",
        "mov r12, [rdi + 16]",
        "mov r13, [rdi + 24]",
        "mov r14, [rdi + 32]",
        "mov r15, [rdi + 40]",
        "mov rdx, [rdi + 56]",
        "mov rsp, [rdi + 48]",
        "jmp rdx",
    );
}

/// `_longjmp(3)`, the name that pairs with [`_setjmp`].
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _longjmp(_env: *mut JmpBufTag, _val: c_int) -> ! {
    core::arch::naked_asm!("jmp {inner}", inner = sym longjmp);
}

/// `sigsetjmp(3)`: [`setjmp`] plus, when `savemask` is non-zero, the current
/// signal mask.
///
/// Register contract: RDI = `env`, ESI = `savemask`, both already in place
/// for the helper this calls.
///
/// # Safety
/// As [`setjmp`].
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigsetjmp(_env: *mut JmpBufTag, _savemask: c_int) -> c_int {
    core::arch::naked_asm!(
        // The capture reads `rsp` at its entry value, so the push that
        // carries RDI across the call is popped before it. That push also
        // supplies the 16-byte alignment the ABI wants at the call.
        "push rdi",
        "call {save_mask}",
        "pop rdi",
        "mov [rdi], rbx",
        "mov [rdi + 8], rbp",
        "mov [rdi + 16], r12",
        "mov [rdi + 24], r13",
        "mov [rdi + 32], r14",
        "mov [rdi + 40], r15",
        "lea rdx, [rsp + 8]",
        "mov [rdi + 48], rdx",
        "mov rdx, [rsp]",
        "mov [rdi + 56], rdx",
        "xor eax, eax",
        "ret",
        save_mask = sym sigsetjmp_save_mask,
    );
}

/// The mask half of [`sigsetjmp`], reached only from that naked entry with
/// its own arguments still in RDI and ESI.
unsafe extern "C" fn sigsetjmp_save_mask(env: *mut JmpBufTag, savemask: c_int) {
    if savemask == 0 {
        (*env).mask_was_saved = 0;
        return;
    }

    let mut current = sigset_t::empty();
    let queried = sigprocmask(SIG_SETMASK, core::ptr::null(), &raw mut current) == 0;
    (*env).mask_was_saved = u64::from(queried);
    if queried {
        (*env).mask = current.__val;
    }
}

/// `siglongjmp(3)`: [`longjmp`], preceded by the mask [`sigsetjmp`] saved.
///
/// The restore has to happen here, while there is still a frame to make the
/// call from — past the jump there is no code of ours left to run, and the
/// resumed `sigsetjmp` caller must already be under its old mask.
///
/// # Safety
/// As [`longjmp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siglongjmp(env: *mut JmpBufTag, val: c_int) -> ! {
    if (*env).mask_was_saved != 0 {
        let saved = sigset_t { __val: (*env).mask };
        let _ = sigprocmask(SIG_SETMASK, &raw const saved, core::ptr::null_mut());
    }
    longjmp(env, val)
}
