//! `<math.h>` over the vendored `libm` crate.
//!
//! `double` and `float` are complete to C99. The `long double` family is not
//! here: this target's `long double` is x87 80-bit and `libm` has no 80-bit
//! code, so an 80-bit entry point would have to compute in `double` and
//! widen. `strtold` does exactly that and says so; nothing else does.
//!
//! The classification and comparison macros stay in the header as compiler
//! builtins, so they cost no symbol at all. Each wrapper below is written out
//! rather than macro-generated because `slibc/build/exports.rs` reads this
//! source to check the header against, and a macro expansion is invisible to
//! it: generating the bodies would trade the ABI check for the typing.

use core::ffi::{c_char, c_int, c_long, c_longlong};

#[unsafe(no_mangle)]
pub extern "C" fn acos(x: f64) -> f64 {
    libm::acos(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn acosf(x: f32) -> f32 {
    libm::acosf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn acosh(x: f64) -> f64 {
    libm::acosh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn acoshf(x: f32) -> f32 {
    libm::acoshf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn asin(x: f64) -> f64 {
    libm::asin(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn asinf(x: f32) -> f32 {
    libm::asinf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn asinh(x: f64) -> f64 {
    libm::asinh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn asinhf(x: f32) -> f32 {
    libm::asinhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn atan(x: f64) -> f64 {
    libm::atan(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn atanf(x: f32) -> f32 {
    libm::atanf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn atanh(x: f64) -> f64 {
    libm::atanh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn atanhf(x: f32) -> f32 {
    libm::atanhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn cbrt(x: f64) -> f64 {
    libm::cbrt(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn cbrtf(x: f32) -> f32 {
    libm::cbrtf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn ceil(x: f64) -> f64 {
    libm::ceil(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn ceilf(x: f32) -> f32 {
    libm::ceilf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn cos(x: f64) -> f64 {
    libm::cos(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn cosf(x: f32) -> f32 {
    libm::cosf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn cosh(x: f64) -> f64 {
    libm::cosh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn coshf(x: f32) -> f32 {
    libm::coshf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn erf(x: f64) -> f64 {
    libm::erf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn erfc(x: f64) -> f64 {
    libm::erfc(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn erfcf(x: f32) -> f32 {
    libm::erfcf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn erff(x: f32) -> f32 {
    libm::erff(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn exp(x: f64) -> f64 {
    libm::exp(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn exp2(x: f64) -> f64 {
    libm::exp2(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn exp2f(x: f32) -> f32 {
    libm::exp2f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn expf(x: f32) -> f32 {
    libm::expf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn expm1(x: f64) -> f64 {
    libm::expm1(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn expm1f(x: f32) -> f32 {
    libm::expm1f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn fabs(x: f64) -> f64 {
    libm::fabs(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn fabsf(x: f32) -> f32 {
    libm::fabsf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn floor(x: f64) -> f64 {
    libm::floor(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn floorf(x: f32) -> f32 {
    libm::floorf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn lgamma(x: f64) -> f64 {
    libm::lgamma(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn lgammaf(x: f32) -> f32 {
    libm::lgammaf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn log(x: f64) -> f64 {
    libm::log(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn log10(x: f64) -> f64 {
    libm::log10(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn log10f(x: f32) -> f32 {
    libm::log10f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn log1p(x: f64) -> f64 {
    libm::log1p(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn log1pf(x: f32) -> f32 {
    libm::log1pf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn log2(x: f64) -> f64 {
    libm::log2(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn log2f(x: f32) -> f32 {
    libm::log2f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn logf(x: f32) -> f32 {
    libm::logf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn rint(x: f64) -> f64 {
    libm::rint(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn rintf(x: f32) -> f32 {
    libm::rintf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn round(x: f64) -> f64 {
    libm::round(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn roundf(x: f32) -> f32 {
    libm::roundf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn sin(x: f64) -> f64 {
    libm::sin(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn sinf(x: f32) -> f32 {
    libm::sinf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn sinh(x: f64) -> f64 {
    libm::sinh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn sinhf(x: f32) -> f32 {
    libm::sinhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn sqrt(x: f64) -> f64 {
    libm::sqrt(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn sqrtf(x: f32) -> f32 {
    libm::sqrtf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn tan(x: f64) -> f64 {
    libm::tan(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn tanf(x: f32) -> f32 {
    libm::tanf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn tanh(x: f64) -> f64 {
    libm::tanh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn tanhf(x: f32) -> f32 {
    libm::tanhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn tgamma(x: f64) -> f64 {
    libm::tgamma(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn tgammaf(x: f32) -> f32 {
    libm::tgammaf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn trunc(x: f64) -> f64 {
    libm::trunc(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn truncf(x: f32) -> f32 {
    libm::truncf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

#[unsafe(no_mangle)]
pub extern "C" fn atan2f(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

#[unsafe(no_mangle)]
pub extern "C" fn copysign(x: f64, y: f64) -> f64 {
    libm::copysign(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn copysignf(x: f32, y: f32) -> f32 {
    libm::copysignf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fdim(x: f64, y: f64) -> f64 {
    libm::fdim(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fdimf(x: f32, y: f32) -> f32 {
    libm::fdimf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmax(x: f64, y: f64) -> f64 {
    libm::fmax(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmaxf(x: f32, y: f32) -> f32 {
    libm::fmaxf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmin(x: f64, y: f64) -> f64 {
    libm::fmin(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fminf(x: f32, y: f32) -> f32 {
    libm::fminf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    libm::fmod(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmodf(x: f32, y: f32) -> f32 {
    libm::fmodf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn hypotf(x: f32, y: f32) -> f32 {
    libm::hypotf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn nextafter(x: f64, y: f64) -> f64 {
    libm::nextafter(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn nextafterf(x: f32, y: f32) -> f32 {
    libm::nextafterf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn powf(x: f32, y: f32) -> f32 {
    libm::powf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn remainder(x: f64, y: f64) -> f64 {
    libm::remainder(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn remainderf(x: f32, y: f32) -> f32 {
    libm::remainderf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fma(x: f64, y: f64, z: f64) -> f64 {
    libm::fma(x, y, z)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmaf(x: f32, y: f32, z: f32) -> f32 {
    libm::fmaf(x, y, z)
}

/// # Safety
/// `exp` is a writable `int` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frexp(x: f64, exp: *mut c_int) -> f64 {
    let (fraction, exponent) = libm::frexp(x);
    if !exp.is_null() {
        *exp = exponent;
    }
    fraction
}

/// # Safety
/// `exp` is a writable `int` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frexpf(x: f32, exp: *mut c_int) -> f32 {
    let (fraction, exponent) = libm::frexpf(x);
    if !exp.is_null() {
        *exp = exponent;
    }
    fraction
}

/// # Safety
/// `iptr` is a writable `double` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn modf(x: f64, iptr: *mut f64) -> f64 {
    let (fraction, integral) = libm::modf(x);
    if !iptr.is_null() {
        *iptr = integral;
    }
    fraction
}

/// # Safety
/// `iptr` is a writable `float` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn modff(x: f32, iptr: *mut f32) -> f32 {
    let (fraction, integral) = libm::modff(x);
    if !iptr.is_null() {
        *iptr = integral;
    }
    fraction
}

/// # Safety
/// `quo` is a writable `int` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remquo(x: f64, y: f64, quo: *mut c_int) -> f64 {
    let (remainder, quotient) = libm::remquo(x, y);
    if !quo.is_null() {
        *quo = quotient;
    }
    remainder
}

/// # Safety
/// `quo` is a writable `int` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remquof(x: f32, y: f32, quo: *mut c_int) -> f32 {
    let (remainder, quotient) = libm::remquof(x, y);
    if !quo.is_null() {
        *quo = quotient;
    }
    remainder
}

#[unsafe(no_mangle)]
pub extern "C" fn ilogb(x: f64) -> c_int {
    libm::ilogb(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn ilogbf(x: f32) -> c_int {
    libm::ilogbf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn ldexp(x: f64, n: c_int) -> f64 {
    libm::ldexp(x, n)
}

#[unsafe(no_mangle)]
pub extern "C" fn ldexpf(x: f32, n: c_int) -> f32 {
    libm::ldexpf(x, n)
}

#[unsafe(no_mangle)]
pub extern "C" fn scalbn(x: f64, n: c_int) -> f64 {
    libm::scalbn(x, n)
}

#[unsafe(no_mangle)]
pub extern "C" fn scalbnf(x: f32, n: c_int) -> f32 {
    libm::scalbnf(x, n)
}

/// Saturating rather than wrapping: an exponent past `int` can only overflow
/// or underflow the result, which is what `scalbn` already answers for the
/// extremes it is clamped to.
fn saturate_exponent(n: c_long) -> c_int {
    n.clamp(c_int::MIN as c_long, c_int::MAX as c_long) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn scalbln(x: f64, n: c_long) -> f64 {
    libm::scalbn(x, saturate_exponent(n))
}

#[unsafe(no_mangle)]
pub extern "C" fn scalblnf(x: f32, n: c_long) -> f32 {
    libm::scalbnf(x, saturate_exponent(n))
}

/// slibc offers no `<fenv.h>`, so the rounding direction is always the
/// default one and `nearbyint` and `rint` answer alike. The two differ only
/// in whether the inexact exception is raised, which nothing here can read.
#[unsafe(no_mangle)]
pub extern "C" fn nearbyint(x: f64) -> f64 {
    libm::rint(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn nearbyintf(x: f32) -> f32 {
    libm::rintf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn logb(x: f64) -> f64 {
    if x == 0.0 {
        f64::NEG_INFINITY
    } else if x.is_nan() || x.is_infinite() {
        libm::fabs(x)
    } else {
        libm::ilogb(x) as f64
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn logbf(x: f32) -> f32 {
    if x == 0.0 {
        f32::NEG_INFINITY
    } else if x.is_nan() || x.is_infinite() {
        libm::fabsf(x)
    } else {
        libm::ilogbf(x) as f32
    }
}

/// C leaves the result unspecified when the rounded value does not fit, and
/// an `as` cast in Rust saturates rather than trapping, so the out-of-range
/// answer is the nearest representable one.
#[unsafe(no_mangle)]
pub extern "C" fn lrint(x: f64) -> c_long {
    libm::rint(x) as c_long
}

#[unsafe(no_mangle)]
pub extern "C" fn lrintf(x: f32) -> c_long {
    libm::rintf(x) as c_long
}

#[unsafe(no_mangle)]
pub extern "C" fn llrint(x: f64) -> c_longlong {
    libm::rint(x) as c_longlong
}

#[unsafe(no_mangle)]
pub extern "C" fn llrintf(x: f32) -> c_longlong {
    libm::rintf(x) as c_longlong
}

#[unsafe(no_mangle)]
pub extern "C" fn lround(x: f64) -> c_long {
    libm::round(x) as c_long
}

#[unsafe(no_mangle)]
pub extern "C" fn lroundf(x: f32) -> c_long {
    libm::roundf(x) as c_long
}

#[unsafe(no_mangle)]
pub extern "C" fn llround(x: f64) -> c_longlong {
    libm::round(x) as c_longlong
}

#[unsafe(no_mangle)]
pub extern "C" fn llroundf(x: f32) -> c_longlong {
    libm::roundf(x) as c_longlong
}

/// The tag is ignored, which C permits: the quiet NaN this returns carries no
/// payload a program here can read back.
#[unsafe(no_mangle)]
pub extern "C" fn nan(_tag: *const c_char) -> f64 {
    f64::NAN
}

#[unsafe(no_mangle)]
pub extern "C" fn nanf(_tag: *const c_char) -> f32 {
    f32::NAN
}
