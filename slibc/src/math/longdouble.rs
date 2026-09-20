//! The `long double` half of `<math.h>`.
//!
//! `long double` on x86-64 System V is the x87 80-bit type: an argument is a
//! 16-byte memory slot, so the first one sits at `[rsp + 8]` past this
//! function's own return address and the second at `[rsp + 24]`, and the
//! result comes back in `st(0)`. Integer and pointer arguments keep their
//! integer registers. Rust can name none of that, so every entry point here
//! is a naked function whose declared signature carries only the
//! register-passed arguments and whose real prototype is in its doc comment —
//! the same shape, and the same honesty about precision, as
//! [`crate::string::convert::strtold`].
//!
//! **Tier A** is exact at the x87's full 64-bit significand, because the
//! hardware has the operation or the answer is pure bit work: `ceill`,
//! `copysignl`, `fabsl`, `fdiml`, `floorl`, `fmaxl`, `fminl`, `fmodl`,
//! `frexpl`, `ilogbl`, `ldexpl`, `llrintl`, `llroundl`, `logbl`, `lrintl`,
//! `lroundl`, `modfl`, `nanl`, `nearbyintl`, `nextafterl`, `remainderl`,
//! `rintl`, `roundl`, `scalblnl`, `scalbnl`, `sqrtl`, `truncl`.
//!
//! `hypotl` works at extended precision but the x87 has no hypot, and the
//! scaled squares are summed before `fsqrt` sees the sum: measured against
//! glibc over 2 000 000 random normal pairs, 670 answers differ and each of
//! those by 1 ULP, with every special value exact.
//!
//! **Tier B** narrows its arguments to `double`, calls the `double` entry
//! point in [`super`], and widens the answer back into `st(0)`: `acosl`,
//! `acoshl`, `asinl`, `asinhl`, `atanl`, `atan2l`, `atanhl`, `cbrtl`, `cosl`,
//! `coshl`, `erfl`, `erfcl`, `expl`, `exp2l`, `expm1l`, `fmal`, `lgammal`,
//! `logl`, `log10l`, `log1pl`, `log2l`, `powl`, `remquol`, `sinl`, `sinhl`,
//! `tanl`, `tanhl`, `tgammal`. Those carry `double` precision rather than the
//! 64 significand bits their type promises: the entry point exists and links,
//! and an 80-bit elementary-function library is not here.
//!
//! Formatted I/O is in that tier too: `printf`'s `%Lf` reads the 16-byte x87
//! operand off the variadic stack and narrows it to `double` before any digit
//! is generated, and `scanf`'s widens `strtold`'s answer back, so a
//! `long double` crosses text at `double` precision.
//!
//! A task starts with FCW `0x037F`, so the x87 rounds to nearest-even at
//! extended precision and Tier A really is 64 significand bits wide. slibc
//! offers no `<fenv.h>`, so that mode never changes and `rintl` and
//! `nearbyintl` answer alike, as `rint` and `nearbyint` already do.
//!
//! No body quiets a signalling NaN — `fld tbyte` does not — so an sNaN
//! argument comes back as itself where C17 F.10 p11 asks for the quiet form.
//! With no `<fenv.h>` the invalid flag is unobservable, and an sNaN can only
//! reach here from punned bits.
//!
//! `lrintl`, `llrintl`, `lroundl` and `llroundl` hand back the x87's integer
//! indefinite, `LONG_MIN`, when the rounded value does not fit; C leaves that
//! case unspecified.

use core::ffi::{c_char, c_int, c_long, c_longlong};

/// `long double fool(long double)`: narrow, call the `double` entry point,
/// widen. The 24-byte frame is 16 bytes of spill plus the 8 that realign
/// `rsp` for the `call`, which the return address has already offset by 8.
macro_rules! widen1 {
    ($name:ident, $inner:ident) => {
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "sub rsp, 24",
                "fld tbyte ptr [rsp + 32]",
                "fstp qword ptr [rsp]",
                "movsd xmm0, qword ptr [rsp]",
                "call {inner}",
                "movsd qword ptr [rsp], xmm0",
                "fld qword ptr [rsp]",
                "add rsp, 24",
                "ret",
                inner = sym super::$inner,
            );
        }
    };
}

/// `long double fool(long double, long double)`, otherwise as [`widen1`].
macro_rules! widen2 {
    ($name:ident, $inner:ident) => {
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "sub rsp, 24",
                "fld tbyte ptr [rsp + 32]",
                "fstp qword ptr [rsp]",
                "movsd xmm0, qword ptr [rsp]",
                "fld tbyte ptr [rsp + 48]",
                "fstp qword ptr [rsp]",
                "movsd xmm1, qword ptr [rsp]",
                "call {inner}",
                "movsd qword ptr [rsp], xmm0",
                "fld qword ptr [rsp]",
                "add rsp, 24",
                "ret",
                inner = sym super::$inner,
            );
        }
    };
}

/// `long double fool(long double, long double, long double)`, otherwise as
/// [`widen1`].
macro_rules! widen3 {
    ($name:ident, $inner:ident) => {
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "sub rsp, 24",
                "fld tbyte ptr [rsp + 32]",
                "fstp qword ptr [rsp]",
                "movsd xmm0, qword ptr [rsp]",
                "fld tbyte ptr [rsp + 48]",
                "fstp qword ptr [rsp]",
                "movsd xmm1, qword ptr [rsp]",
                "fld tbyte ptr [rsp + 64]",
                "fstp qword ptr [rsp]",
                "movsd xmm2, qword ptr [rsp]",
                "call {inner}",
                "movsd qword ptr [rsp], xmm0",
                "fld qword ptr [rsp]",
                "add rsp, 24",
                "ret",
                inner = sym super::$inner,
            );
        }
    };
}

widen1!(acosl, acos);
widen1!(acoshl, acosh);
widen1!(asinl, asin);
widen1!(asinhl, asinh);
widen1!(atanl, atan);
widen1!(atanhl, atanh);
widen1!(cbrtl, cbrt);
widen1!(cosl, cos);
widen1!(coshl, cosh);
widen1!(erfl, erf);
widen1!(erfcl, erfc);
widen1!(expl, exp);
widen1!(exp2l, exp2);
widen1!(expm1l, expm1);
widen1!(lgammal, lgamma);
widen1!(logl, log);
widen1!(log10l, log10);
widen1!(log1pl, log1p);
widen1!(log2l, log2);
widen1!(sinl, sin);
widen1!(sinhl, sinh);
widen1!(tanl, tan);
widen1!(tanhl, tanh);
widen1!(tgammal, tgamma);

widen2!(atan2l, atan2);
widen2!(powl, pow);

widen3!(fmal, fma);

/// `long double remquol(long double x, long double y, int *quo)`. `rdi` is
/// the pointer, which the narrowing sequence leaves alone.
///
/// # Safety
/// As [`super::remquo`].
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remquol(_quo: *mut c_int) {
    core::arch::naked_asm!(
        "sub rsp, 24",
        "fld tbyte ptr [rsp + 32]",
        "fstp qword ptr [rsp]",
        "movsd xmm0, qword ptr [rsp]",
        "fld tbyte ptr [rsp + 48]",
        "fstp qword ptr [rsp]",
        "movsd xmm1, qword ptr [rsp]",
        "call {inner}",
        "movsd qword ptr [rsp], xmm0",
        "fld qword ptr [rsp]",
        "add rsp, 24",
        "ret",
        inner = sym super::remquo,
    );
}

/// `long double fabsl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fabsl() {
    core::arch::naked_asm!("fld tbyte ptr [rsp + 8]", "fabs", "ret");
}

/// `long double sqrtl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqrtl() {
    core::arch::naked_asm!("fld tbyte ptr [rsp + 8]", "fsqrt", "ret");
}

/// `long double copysignl(long double x, long double y)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copysignl() {
    core::arch::naked_asm!(
        "sub rsp, 16",
        "mov rdx, qword ptr [rsp + 24]",
        "movzx eax, word ptr [rsp + 32]",
        "movzx ecx, word ptr [rsp + 48]",
        "and eax, 0x7fff",
        "and ecx, 0x8000",
        "or eax, ecx",
        "mov qword ptr [rsp], rdx",
        "mov word ptr [rsp + 8], ax",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "ret",
    );
}

/// `long double fmodl(long double x, long double y)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fmodl() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 24]",
        "fld tbyte ptr [rsp + 8]",
        // FPREM reduces by at most 64 binary digits of quotient per step and
        // reports the partial result with C2 set.
        "2:",
        "fprem",
        "fnstsw ax",
        "test ah, 4",
        "jnz 2b",
        "fstp st(1)",
        "ret",
    );
}

/// `long double remainderl(long double x, long double y)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remainderl() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 24]",
        "fld tbyte ptr [rsp + 8]",
        "2:",
        "fprem1",
        "fnstsw ax",
        "test ah, 4",
        "jnz 2b",
        "fstp st(1)",
        "ret",
    );
}

/// `long double ceill(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ceill() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 8]",
        "sub rsp, 8",
        "fnstcw word ptr [rsp]",
        "movzx eax, word ptr [rsp]",
        "and eax, 0xf3ff",
        "or eax, 0x0800",
        "mov word ptr [rsp + 4], ax",
        "fldcw word ptr [rsp + 4]",
        "frndint",
        "fldcw word ptr [rsp]",
        "add rsp, 8",
        "ret",
    );
}

/// `long double floorl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn floorl() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 8]",
        "sub rsp, 8",
        "fnstcw word ptr [rsp]",
        "movzx eax, word ptr [rsp]",
        "and eax, 0xf3ff",
        "or eax, 0x0400",
        "mov word ptr [rsp + 4], ax",
        "fldcw word ptr [rsp + 4]",
        "frndint",
        "fldcw word ptr [rsp]",
        "add rsp, 8",
        "ret",
    );
}

/// `long double truncl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn truncl() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 8]",
        "sub rsp, 8",
        "fnstcw word ptr [rsp]",
        "movzx eax, word ptr [rsp]",
        "and eax, 0xf3ff",
        "or eax, 0x0c00",
        "mov word ptr [rsp + 4], ax",
        "fldcw word ptr [rsp + 4]",
        "frndint",
        "fldcw word ptr [rsp]",
        "add rsp, 8",
        "ret",
    );
}

/// `long double rintl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rintl() {
    core::arch::naked_asm!("fld tbyte ptr [rsp + 8]", "frndint", "ret");
}

/// `long double nearbyintl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nearbyintl() {
    core::arch::naked_asm!("fld tbyte ptr [rsp + 8]", "frndint", "ret");
}

/// `long double roundl(long double)`: halfway cases go away from zero, which
/// no x87 rounding mode does, so the fraction is compared against a half.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn roundl() {
    core::arch::naked_asm!(
        // Biased exponent 16446 is 2^63: at or above it every value is
        // already integral, and so are the infinities and the NaNs.
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "cmp eax, 16446",
        "jae 4f",
        "fld tbyte ptr [rsp + 8]",
        "sub rsp, 8",
        "fnstcw word ptr [rsp]",
        "movzx eax, word ptr [rsp]",
        "and eax, 0xf3ff",
        "or eax, 0x0c00",
        "mov word ptr [rsp + 4], ax",
        "fldcw word ptr [rsp + 4]",
        "fld st(0)",
        "frndint",
        "fldcw word ptr [rsp]",
        "add rsp, 8",
        "fsub st(1), st(0)",
        "fxch st(1)",
        "fabs",
        "mov rax, 0x3fe0000000000000",
        "push rax",
        "fld qword ptr [rsp]",
        "add rsp, 8",
        "fcomip st, st(1)",
        "fstp st(0)",
        "jbe 3f",
        "ret",
        "3:",
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x8000",
        "or eax, 0x3fff",
        "mov rcx, 0x8000000000000000",
        "sub rsp, 16",
        "mov qword ptr [rsp], rcx",
        "mov word ptr [rsp + 8], ax",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "faddp st(1), st(0)",
        "ret",
        "4:",
        "fld tbyte ptr [rsp + 8]",
        "ret",
    );
}

/// `long lrintl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lrintl() -> c_long {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 8]",
        "sub rsp, 8",
        "fistp qword ptr [rsp]",
        "mov rax, qword ptr [rsp]",
        "add rsp, 8",
        "ret",
    );
}

/// `long long llrintl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn llrintl() -> c_longlong {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 8]",
        "sub rsp, 8",
        "fistp qword ptr [rsp]",
        "mov rax, qword ptr [rsp]",
        "add rsp, 8",
        "ret",
    );
}

/// `long lroundl(long double)`. The 24-byte frame holds the forwarded
/// argument and realigns `rsp`; the rounded value is integral, so the
/// rounding mode FISTP uses cannot change it.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lroundl() -> c_long {
    core::arch::naked_asm!(
        "sub rsp, 24",
        "mov rax, qword ptr [rsp + 32]",
        "mov qword ptr [rsp], rax",
        "movzx eax, word ptr [rsp + 40]",
        "mov word ptr [rsp + 8], ax",
        "call {round}",
        "fistp qword ptr [rsp]",
        "mov rax, qword ptr [rsp]",
        "add rsp, 24",
        "ret",
        round = sym roundl,
    );
}

/// `long long llroundl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn llroundl() -> c_longlong {
    core::arch::naked_asm!(
        "sub rsp, 24",
        "mov rax, qword ptr [rsp + 32]",
        "mov qword ptr [rsp], rax",
        "movzx eax, word ptr [rsp + 40]",
        "mov word ptr [rsp + 8], ax",
        "call {round}",
        "fistp qword ptr [rsp]",
        "mov rax, qword ptr [rsp]",
        "add rsp, 24",
        "ret",
        round = sym roundl,
    );
}

/// `long double ldexpl(long double x, int n)`, `n` in `edi`. One FSCALE
/// spans 16 bits of exponent, short of the x87's 32830-wide range, so `n`
/// saturates at ±65534 and goes in two steps, remainder first: any
/// intermediate that rounds is one the second step then drives to zero.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ldexpl(_n: c_int) {
    core::arch::naked_asm!(
        "mov eax, 65534",
        "cmp edi, eax",
        "cmovl eax, edi",
        "mov ecx, -65534",
        "cmp eax, ecx",
        "cmovl eax, ecx",
        "mov ecx, 32767",
        "cmp eax, ecx",
        "cmovl ecx, eax",
        "mov edx, -32767",
        "cmp ecx, edx",
        "cmovl ecx, edx",
        "sub eax, ecx",
        "sub rsp, 8",
        "mov dword ptr [rsp], eax",
        "mov dword ptr [rsp + 4], ecx",
        "fild dword ptr [rsp + 4]",
        "fild dword ptr [rsp]",
        "fld tbyte ptr [rsp + 16]",
        "fscale",
        "fstp st(1)",
        "fscale",
        "fstp st(1)",
        "add rsp, 8",
        "ret",
    );
}

/// `long double scalbnl(long double x, int n)`, `n` in `edi`, otherwise as
/// [`ldexpl`].
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scalbnl(_n: c_int) {
    core::arch::naked_asm!(
        "mov eax, 65534",
        "cmp edi, eax",
        "cmovl eax, edi",
        "mov ecx, -65534",
        "cmp eax, ecx",
        "cmovl eax, ecx",
        "mov ecx, 32767",
        "cmp eax, ecx",
        "cmovl ecx, eax",
        "mov edx, -32767",
        "cmp ecx, edx",
        "cmovl ecx, edx",
        "sub eax, ecx",
        "sub rsp, 8",
        "mov dword ptr [rsp], eax",
        "mov dword ptr [rsp + 4], ecx",
        "fild dword ptr [rsp + 4]",
        "fild dword ptr [rsp]",
        "fld tbyte ptr [rsp + 16]",
        "fscale",
        "fstp st(1)",
        "fscale",
        "fstp st(1)",
        "add rsp, 8",
        "ret",
    );
}

/// `long double scalblnl(long double x, long n)`, `n` in `rdi`, otherwise as
/// [`ldexpl`].
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scalblnl(_n: c_long) {
    core::arch::naked_asm!(
        "mov rax, 65534",
        "cmp rdi, rax",
        "cmovl rax, rdi",
        "mov rcx, -65534",
        "cmp rax, rcx",
        "cmovl rax, rcx",
        "mov ecx, 32767",
        "cmp eax, ecx",
        "cmovl ecx, eax",
        "mov edx, -32767",
        "cmp ecx, edx",
        "cmovl ecx, edx",
        "sub eax, ecx",
        "sub rsp, 8",
        "mov dword ptr [rsp], eax",
        "mov dword ptr [rsp + 4], ecx",
        "fild dword ptr [rsp + 4]",
        "fild dword ptr [rsp]",
        "fld tbyte ptr [rsp + 16]",
        "fscale",
        "fstp st(1)",
        "fscale",
        "fstp st(1)",
        "add rsp, 8",
        "ret",
    );
}

/// `long double frexpl(long double x, int *exp)`, the pointer in `rdi`.
/// FXTRACT reports the true exponent of a subnormal, so only zero and the
/// non-finite values need a branch.
///
/// # Safety
/// `exp` is a writable `int` or null.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frexpl(_exp: *mut c_int) {
    core::arch::naked_asm!(
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "cmp eax, 0x7fff",
        "je 4f",
        "test eax, eax",
        "jnz 2f",
        "cmp qword ptr [rsp + 8], 0",
        "je 4f",
        "2:",
        "fld tbyte ptr [rsp + 8]",
        "fxtract",
        "fxch st(1)",
        "sub rsp, 8",
        "fistp dword ptr [rsp]",
        "mov eax, dword ptr [rsp]",
        "add rsp, 8",
        // FXTRACT normalises to [1, 2); frexp wants [0.5, 1).
        "inc eax",
        "test rdi, rdi",
        "jz 3f",
        "mov dword ptr [rdi], eax",
        "3:",
        "mov rax, 0x3fe0000000000000",
        "push rax",
        "fmul qword ptr [rsp]",
        "add rsp, 8",
        "ret",
        "4:",
        "test rdi, rdi",
        "jz 5f",
        "mov dword ptr [rdi], 0",
        "5:",
        "fld tbyte ptr [rsp + 8]",
        "ret",
    );
}

/// `long double modfl(long double x, long double *iptr)`, the pointer in
/// `rdi`. C gives the fractional part the sign of the argument, which a
/// subtraction that cancels exactly would lose.
///
/// # Safety
/// `iptr` is a writable `long double` or null.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn modfl(_iptr: *mut c_char) {
    core::arch::naked_asm!(
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "cmp eax, 0x7fff",
        "je 5f",
        "fld tbyte ptr [rsp + 8]",
        "sub rsp, 8",
        "fnstcw word ptr [rsp]",
        "movzx eax, word ptr [rsp]",
        "and eax, 0xf3ff",
        "or eax, 0x0c00",
        "mov word ptr [rsp + 4], ax",
        "fldcw word ptr [rsp + 4]",
        "fld st(0)",
        "frndint",
        "fldcw word ptr [rsp]",
        "add rsp, 8",
        "fsub st(1), st(0)",
        "test rdi, rdi",
        "jz 2f",
        "fstp tbyte ptr [rdi]",
        "jmp 3f",
        "2:",
        "fstp st(0)",
        "3:",
        "ftst",
        "fnstsw ax",
        "test ah, 0x40",
        "jz 4f",
        "fstp st(0)",
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x8000",
        "sub rsp, 16",
        "mov qword ptr [rsp], 0",
        "mov word ptr [rsp + 8], ax",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "4:",
        "ret",
        "5:",
        "test rdi, rdi",
        "jz 6f",
        "fld tbyte ptr [rsp + 8]",
        "fstp tbyte ptr [rdi]",
        "6:",
        "mov rcx, 0x8000000000000000",
        "cmp qword ptr [rsp + 8], rcx",
        "jne 7f",
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x8000",
        "sub rsp, 16",
        "mov qword ptr [rsp], 0",
        "mov word ptr [rsp + 8], ax",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "ret",
        "7:",
        "fld tbyte ptr [rsp + 8]",
        "ret",
    );
}

/// `long double logbl(long double)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logbl() {
    core::arch::naked_asm!(
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "cmp eax, 0x7fff",
        "je 3f",
        "test eax, eax",
        "jnz 2f",
        "cmp qword ptr [rsp + 8], 0",
        "je 4f",
        "2:",
        "fld tbyte ptr [rsp + 8]",
        "fxtract",
        "fstp st(0)",
        "ret",
        "3:",
        "fld tbyte ptr [rsp + 8]",
        "fabs",
        "ret",
        "4:",
        "sub rsp, 16",
        "mov rax, 0x8000000000000000",
        "mov qword ptr [rsp], rax",
        "mov eax, 0xffff",
        "mov word ptr [rsp + 8], ax",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "ret",
    );
}

/// `int ilogbl(long double)`, answering `INT_MIN` for zero and for a NaN and
/// `INT_MAX` for an infinity, as `ilogb` does.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilogbl() -> c_int {
    core::arch::naked_asm!(
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "cmp eax, 0x7fff",
        "je 4f",
        "test eax, eax",
        "jnz 2f",
        "cmp qword ptr [rsp + 8], 0",
        "je 3f",
        "2:",
        "fld tbyte ptr [rsp + 8]",
        "fxtract",
        "fstp st(0)",
        "sub rsp, 8",
        "fistp dword ptr [rsp]",
        "mov eax, dword ptr [rsp]",
        "add rsp, 8",
        "ret",
        "3:",
        "mov eax, 0x80000000",
        "ret",
        "4:",
        "mov rcx, 0x8000000000000000",
        "cmp qword ptr [rsp + 8], rcx",
        "mov eax, 0x80000000",
        "mov ecx, 0x7fffffff",
        "cmove eax, ecx",
        "ret",
    );
}

/// `long double fmaxl(long double x, long double y)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fmaxl() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 8]",
        "fld tbyte ptr [rsp + 24]",
        "fucomi st, st(1)",
        "jp 4f",
        "ja 2f",
        "jb 3f",
        // Equal but for the sign of a zero: fmax answers the +0.
        "test word ptr [rsp + 16], 0x8000",
        "jz 3f",
        "2:",
        "fstp st(1)",
        "ret",
        "3:",
        "fstp st(0)",
        "ret",
        // Unordered: C wants the operand that is not a NaN, and a NaN only
        // when both are.
        "4:",
        "mov rdx, 0x8000000000000000",
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "cmp eax, 0x7fff",
        "jne 3b",
        "cmp qword ptr [rsp + 8], rdx",
        "jne 2b",
        "jmp 3b",
    );
}

/// `long double fminl(long double x, long double y)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fminl() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 8]",
        "fld tbyte ptr [rsp + 24]",
        "fucomi st, st(1)",
        "jp 4f",
        "jb 2f",
        "ja 3f",
        // Equal but for the sign of a zero: fmin answers the -0.
        "test word ptr [rsp + 16], 0x8000",
        "jnz 3f",
        "2:",
        "fstp st(1)",
        "ret",
        "3:",
        "fstp st(0)",
        "ret",
        "4:",
        "mov rdx, 0x8000000000000000",
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "cmp eax, 0x7fff",
        "jne 3b",
        "cmp qword ptr [rsp + 8], rdx",
        "jne 2b",
        "jmp 3b",
    );
}

/// `long double fdiml(long double x, long double y)`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdiml() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 24]",
        "fld tbyte ptr [rsp + 8]",
        "fucomi st, st(1)",
        "jp 3f",
        "jbe 2f",
        "fsub st(0), st(1)",
        "fstp st(1)",
        "ret",
        "2:",
        "fstp st(0)",
        "fstp st(0)",
        "fldz",
        "ret",
        "3:",
        "faddp st(1), st(0)",
        "ret",
    );
}

/// `long double hypotl(long double x, long double y)`. The larger operand is
/// scaled to [1, 2) before squaring, so no representable result overflows.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hypotl() {
    core::arch::naked_asm!(
        "movzx eax, word ptr [rsp + 16]",
        "and eax, 0x7fff",
        "movzx ecx, word ptr [rsp + 32]",
        "and ecx, 0x7fff",
        "cmp eax, 0x7fff",
        "je 6f",
        "cmp ecx, 0x7fff",
        "je 6f",
        "fld tbyte ptr [rsp + 8]",
        "fabs",
        "fld tbyte ptr [rsp + 24]",
        "fabs",
        "fcomi st, st(1)",
        "ja 2f",
        "fxch st(1)",
        "2:",
        "ftst",
        "fnstsw ax",
        "test ah, 0x40",
        "jnz 5f",
        "fxtract",
        "fxch st(1)",
        "fchs",
        "fxch st(2)",
        "fld st(2)",
        "fxch st(1)",
        "fscale",
        "fstp st(1)",
        "fmul st(0), st(0)",
        "fxch st(1)",
        "fmul st(0), st(0)",
        "faddp st(1), st(0)",
        "fsqrt",
        "fxch st(1)",
        "fchs",
        "fxch st(1)",
        "fscale",
        "fstp st(1)",
        "ret",
        "5:",
        "fstp st(1)",
        "ret",
        // An infinity wins over a NaN in the other operand.
        "6:",
        "mov rdx, 0x8000000000000000",
        "cmp eax, 0x7fff",
        "jne 7f",
        "cmp qword ptr [rsp + 8], rdx",
        "je 9f",
        "7:",
        "cmp ecx, 0x7fff",
        "jne 8f",
        "cmp qword ptr [rsp + 24], rdx",
        "je 9f",
        "8:",
        "fld tbyte ptr [rsp + 8]",
        "fld tbyte ptr [rsp + 24]",
        "faddp st(1), st(0)",
        "ret",
        "9:",
        "sub rsp, 16",
        "mov qword ptr [rsp], rdx",
        "mov eax, 0x7fff",
        "mov word ptr [rsp + 8], ax",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "ret",
    );
}

/// `long double nextafterl(long double x, long double y)`. The 80-bit format
/// carries its integer bit explicitly, so the step is a 64-bit increment on
/// the significand with the two boundaries that redundancy creates handled by
/// hand: a carry out of the significand bumps the exponent, and the smallest
/// normal borrows into the subnormals without renormalising.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nextafterl() {
    core::arch::naked_asm!(
        "fld tbyte ptr [rsp + 24]",
        "fld tbyte ptr [rsp + 8]",
        "fucomi st, st(1)",
        "jp 2f",
        "je 3f",
        "seta dl",
        "fstp st(0)",
        "fstp st(0)",
        "mov rax, qword ptr [rsp + 8]",
        "movzx ecx, word ptr [rsp + 16]",
        "mov r9d, ecx",
        "and r9d, 0x8000",
        "and ecx, 0x7fff",
        "jnz 4f",
        "test rax, rax",
        "jnz 4f",
        "movzx r9d, word ptr [rsp + 32]",
        "and r9d, 0x8000",
        "mov eax, 1",
        "jmp 8f",
        "2:",
        "faddp st(1), st(0)",
        "ret",
        "3:",
        "fstp st(0)",
        "ret",
        "4:",
        "mov r10d, r9d",
        "shr r10d, 15",
        "cmp dl, r10b",
        "je 6f",
        "sub rax, 1",
        "js 8f",
        "test ecx, ecx",
        "jz 8f",
        "dec ecx",
        "jz 8f",
        "mov rax, -1",
        "jmp 8f",
        "6:",
        "add rax, 1",
        "jnz 7f",
        "inc ecx",
        "mov rax, 0x8000000000000000",
        "jmp 8f",
        "7:",
        "test ecx, ecx",
        "jnz 8f",
        "test rax, rax",
        "jns 8f",
        "mov ecx, 1",
        "8:",
        "or ecx, r9d",
        "sub rsp, 16",
        "mov qword ptr [rsp], rax",
        "mov word ptr [rsp + 8], cx",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "ret",
    );
}

/// `long double nanl(const char *tag)`. The tag is ignored, which C permits.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nanl(_tag: *const c_char) {
    core::arch::naked_asm!(
        "sub rsp, 16",
        "mov rax, 0xc000000000000000",
        "mov qword ptr [rsp], rax",
        "mov eax, 0x7fff",
        "mov word ptr [rsp + 8], ax",
        "fld tbyte ptr [rsp]",
        "add rsp, 16",
        "ret",
    );
}
