//! `<fenv.h>` over both x86-64 floating-point units, with musl's semantics.
//!
//! The x87 and SSE each keep their own exception flags and rounding mode.
//! The flags a program sees are the union of the two; a rounding change is
//! made to both, so `double` arithmetic (SSE) and `long double` (x87) agree;
//! and `fegetround` reads the MXCSR, which is what `double` code runs under.
//! `feraiseexcept` sets the flags in the MXCSR without trapping, as musl does.

use core::arch::asm;
use core::ffi::c_int;

#[allow(non_camel_case_types)]
pub type fexcept_t = u16;

/// The 28-byte `fnstenv` image followed by the MXCSR: glibc's and musl's
/// x86-64 layout, so `fegetenv`/`fesetenv` are one instruction each per unit.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct fenv_t {
    pub __control_word: u16,
    pub __unused1: u16,
    pub __status_word: u16,
    pub __unused2: u16,
    pub __tags: u16,
    pub __unused3: u16,
    pub __eip: u32,
    pub __cs_selector: u16,
    pub __opcode: u16,
    pub __data_offset: u32,
    pub __data_selector: u16,
    pub __unused5: u16,
    pub __mxcsr: u32,
}

const _: () = assert!(core::mem::size_of::<fenv_t>() == 32);
const _: () = assert!(core::mem::offset_of!(fenv_t, __mxcsr) == 28);

pub const FE_INVALID: c_int = 1;
pub const FE_DIVBYZERO: c_int = 4;
pub const FE_OVERFLOW: c_int = 8;
pub const FE_UNDERFLOW: c_int = 16;
pub const FE_INEXACT: c_int = 32;
pub const FE_ALL_EXCEPT: c_int = 0x3f;

pub const FE_TONEAREST: c_int = 0;
pub const FE_DOWNWARD: c_int = 0x400;
pub const FE_UPWARD: c_int = 0x800;
pub const FE_TOWARDZERO: c_int = 0xc00;
const ROUND_MASK: c_int = 0xc00;

/// The MXCSR keeps its rounding field three bits above the x87 control word's.
const MXCSR_ROUND_SHIFT: u32 = 3;
/// The MXCSR's six exception-mask bits. All set, round to nearest and no flag
/// raised is the power-on MXCSR.
const MXCSR_EXCEPTION_MASKS: u32 = 0x1f80;
const MXCSR_DEFAULT: u32 = MXCSR_EXCEPTION_MASKS;
/// Every x87 exception masked, 64-bit precision, round to nearest.
const X87_CW_DEFAULT: u16 = 0x037f;
const X87_EXCEPTION_MASKS: u16 = 0x3f;

fn mxcsr() -> u32 {
    let mut value = 0u32;
    // SAFETY: `stmxcsr` stores four bytes to the operand and nothing else.
    unsafe { asm!("stmxcsr [{}]", in(reg) &raw mut value, options(nostack, preserves_flags)) };
    value
}

fn set_mxcsr(value: u32) {
    // SAFETY: `ldmxcsr` reads four bytes; every value this module forms keeps
    // the reserved bits clear, which is the only way it can fault.
    unsafe { asm!("ldmxcsr [{}]", in(reg) &raw const value, options(nostack, preserves_flags)) };
}

fn x87_status() -> u16 {
    let status: u16;
    // SAFETY: `fnstsw ax` only reads the x87 status word.
    unsafe { asm!("fnstsw ax", out("ax") status, options(nomem, nostack, preserves_flags)) };
    status
}

fn x87_control() -> u16 {
    let mut value = 0u16;
    // SAFETY: `fnstcw` stores two bytes to the operand.
    unsafe { asm!("fnstcw [{}]", in(reg) &raw mut value, options(nostack, preserves_flags)) };
    value
}

fn set_x87_control(value: u16) {
    // SAFETY: `fldcw` reads two bytes. Pending exceptions are cleared by every
    // caller that unmasks, so loading a control word cannot trap here.
    unsafe { asm!("fldcw [{}]", in(reg) &raw const value, options(nostack, preserves_flags)) };
}

fn x87_clear() {
    // SAFETY: `fnclex` clears the x87 exception flags and touches nothing else.
    unsafe { asm!("fnclex", options(nomem, nostack, preserves_flags)) };
}

/// `feclearexcept(3)`. Clearing one x87 flag means clearing them all, so any
/// x87 flag the caller did not name moves to the MXCSR instead of being lost.
#[unsafe(no_mangle)]
pub extern "C" fn feclearexcept(excepts: c_int) -> c_int {
    let excepts = (excepts & FE_ALL_EXCEPT) as u32;
    let x87 = u32::from(x87_status()) & FE_ALL_EXCEPT as u32;
    let mut sse = mxcsr();
    if x87 & excepts != 0 {
        x87_clear();
        sse |= x87;
    }
    set_mxcsr(sse & !excepts);
    0
}

/// `feraiseexcept(3)`: the flags are set; with every exception masked, as it
/// is unless a program unmasks one through `fesetenv`, that is all raising is.
#[unsafe(no_mangle)]
pub extern "C" fn feraiseexcept(excepts: c_int) -> c_int {
    set_mxcsr(mxcsr() | (excepts & FE_ALL_EXCEPT) as u32);
    0
}

/// `fetestexcept(3)`.
#[unsafe(no_mangle)]
pub extern "C" fn fetestexcept(excepts: c_int) -> c_int {
    let raised = (u32::from(x87_status()) | mxcsr()) as c_int;
    raised & excepts & FE_ALL_EXCEPT
}

/// `fegetexceptflag(3)`.
///
/// # Safety
/// `flagp` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fegetexceptflag(flagp: *mut fexcept_t, excepts: c_int) -> c_int {
    *flagp = fetestexcept(excepts) as fexcept_t;
    0
}

/// `fesetexceptflag(3)`: each named flag takes the state `*flagp` records.
///
/// # Safety
/// `flagp` is readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fesetexceptflag(flagp: *const fexcept_t, excepts: c_int) -> c_int {
    let saved = c_int::from(*flagp);
    feclearexcept(!saved & excepts);
    feraiseexcept(saved & excepts);
    0
}

/// `fegetround(3)`.
#[unsafe(no_mangle)]
pub extern "C" fn fegetround() -> c_int {
    (mxcsr() >> MXCSR_ROUND_SHIFT) as c_int & ROUND_MASK
}

/// `fesetround(3)`. Nonzero, with nothing changed, for a mode that is not one
/// of the four.
#[unsafe(no_mangle)]
pub extern "C" fn fesetround(round: c_int) -> c_int {
    if !matches!(
        round,
        FE_TONEAREST | FE_DOWNWARD | FE_UPWARD | FE_TOWARDZERO
    ) {
        return -1;
    }
    set_x87_control((x87_control() & !(ROUND_MASK as u16)) | round as u16);
    let sse_mask = (ROUND_MASK as u32) << MXCSR_ROUND_SHIFT;
    set_mxcsr((mxcsr() & !sse_mask) | ((round as u32) << MXCSR_ROUND_SHIFT));
    0
}

/// `fegetenv(3)`. `fnstenv` masks every x87 exception as a side effect, so the
/// saved control word is loaded back.
///
/// # Safety
/// `envp` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fegetenv(envp: *mut fenv_t) -> c_int {
    asm!("fnstenv [{}]", in(reg) envp, options(nostack, preserves_flags));
    set_x87_control((*envp).__control_word);
    (*envp).__mxcsr = mxcsr();
    0
}

/// `fesetenv(3)`. `FE_DFL_ENV`, `(const fenv_t *)-1`, is the power-on state.
///
/// # Safety
/// `envp` is `FE_DFL_ENV` or readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fesetenv(envp: *const fenv_t) -> c_int {
    let default;
    let envp = if envp as usize == usize::MAX {
        default = fenv_t {
            __control_word: X87_CW_DEFAULT,
            __unused1: 0,
            __status_word: 0,
            __unused2: 0,
            __tags: 0xffff,
            __unused3: 0,
            __eip: 0,
            __cs_selector: 0,
            __opcode: 0,
            __data_offset: 0,
            __data_selector: 0,
            __unused5: 0,
            __mxcsr: MXCSR_DEFAULT,
        };
        &raw const default
    } else {
        envp
    };
    asm!("fldenv [{}]", in(reg) envp, options(nostack, preserves_flags));
    set_mxcsr((*envp).__mxcsr);
    0
}

/// `feholdexcept(3)`: save the environment, clear the flags, and mask every
/// exception so the code that follows runs non-stop.
///
/// # Safety
/// `envp` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn feholdexcept(envp: *mut fenv_t) -> c_int {
    fegetenv(envp);
    x87_clear();
    set_x87_control(x87_control() | X87_EXCEPTION_MASKS);
    set_mxcsr((mxcsr() & !(FE_ALL_EXCEPT as u32)) | MXCSR_EXCEPTION_MASKS);
    0
}

/// `feupdateenv(3)`: install `*envp`, then raise again what was raised before.
///
/// # Safety
/// As [`fesetenv`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn feupdateenv(envp: *const fenv_t) -> c_int {
    let raised = fetestexcept(FE_ALL_EXCEPT);
    fesetenv(envp);
    feraiseexcept(raised);
    0
}
