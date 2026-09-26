//! The bodies of the hot `mem*`/`str*` exports: SSE2 over 16-byte blocks,
//! `rep movsb`/`rep stosb` for long runs.
//!
//! Self-contained over `core` so the host test suite can compile this file
//! as-is (`slibc-core/tests/string_vector.rs`).
//!
//! Every block load and store in a loop is an `asm!` statement. LLVM turns a
//! loop of plain loads and stores into a `memcpy`/`memset` call, which inside
//! those very exports is infinite recursion; an asm statement is opaque to
//! that recognition.
//!
//! The string scans read whole aligned 16-byte blocks, which may reach past
//! the terminator. An aligned block never straddles a page, so it is mapped
//! whenever the byte that put it in range is; the reads are asm because
//! surface Rust calls an out-of-bounds read undefined whatever the page
//! tables say.

use core::arch::asm;
use core::arch::x86_64::{
    __m128i, _mm_cmpeq_epi8, _mm_cmpeq_epi32, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    _mm_set1_epi32, _mm_setzero_si128,
};
use core::ptr;

const BLOCK: usize = 16;
const PAGE: usize = 4096;

/// From here up, `rep movsb`/`rep stosb` beat the 64-byte vector loop on
/// ERMS parts: their fixed start-up cost (tens of cycles) stops mattering
/// once a copy spans a few dozen cache lines. glibc's default for 16-byte
/// vectors is the same 2 KiB.
const REP_MIN: usize = 2048;

#[cfg(target_feature = "avx")]
macro_rules! vex {
    ($op:literal) => {
        concat!("v", $op)
    };
}
// Legacy-SSE encodings only when the whole image is: mixing them into VEX
// code costs a state transition per instruction on Intel parts.
#[cfg(not(target_feature = "avx"))]
macro_rules! vex {
    ($op:literal) => {
        $op
    };
}

#[inline(always)]
unsafe fn load(p: *const u8) -> __m128i {
    let v;
    asm!(
        concat!(vex!("movdqu"), " {v}, xmmword ptr [{p}]"),
        p = in(reg) p,
        v = out(xmm_reg) v,
        options(pure, readonly, nostack, preserves_flags),
    );
    v
}

/// # Safety
/// `p` is 16-byte aligned and at least one byte of `[p, p + 16)` is
/// readable.
#[inline(always)]
unsafe fn load_block(p: *const u8) -> __m128i {
    let v;
    asm!(
        concat!(vex!("movdqa"), " {v}, xmmword ptr [{p}]"),
        p = in(reg) p,
        v = out(xmm_reg) v,
        options(pure, readonly, nostack, preserves_flags),
    );
    v
}

#[inline(always)]
unsafe fn store(p: *mut u8, v: __m128i) {
    asm!(
        concat!(vex!("movdqu"), " xmmword ptr [{p}], {v}"),
        p = in(reg) p,
        v = in(xmm_reg) v,
        options(nostack, preserves_flags),
    );
}

#[inline(always)]
unsafe fn store_block(p: *mut u8, v: __m128i) {
    asm!(
        concat!(vex!("movdqa"), " xmmword ptr [{p}], {v}"),
        p = in(reg) p,
        v = in(xmm_reg) v,
        options(nostack, preserves_flags),
    );
}

#[inline(always)]
fn mask_eq(a: __m128i, b: __m128i) -> u32 {
    // SAFETY: SSE2 is part of the x86-64 baseline.
    unsafe { _mm_movemask_epi8(_mm_cmpeq_epi8(a, b)) as u32 }
}

#[inline(always)]
fn byte_diff(a: u8, b: u8) -> i32 {
    a as i32 - b as i32
}

/// `memmove`: copy `n` bytes from `src` to `dst`, which may overlap.
///
/// # Safety
/// `src` is readable and `dst` writable for `n` bytes.
#[inline(always)]
pub unsafe fn copy(dst: *mut u8, src: *const u8, n: usize) {
    // Every short case loads all of its bytes before storing any, which is
    // what makes it overlap-safe in both directions.
    if n <= 16 {
        if n >= 8 {
            let a = ptr::read_unaligned(src.cast::<u64>());
            let b = ptr::read_unaligned(src.add(n - 8).cast::<u64>());
            ptr::write_unaligned(dst.cast::<u64>(), a);
            ptr::write_unaligned(dst.add(n - 8).cast::<u64>(), b);
        } else if n >= 4 {
            let a = ptr::read_unaligned(src.cast::<u32>());
            let b = ptr::read_unaligned(src.add(n - 4).cast::<u32>());
            ptr::write_unaligned(dst.cast::<u32>(), a);
            ptr::write_unaligned(dst.add(n - 4).cast::<u32>(), b);
        } else if n >= 2 {
            let a = ptr::read_unaligned(src.cast::<u16>());
            let b = ptr::read_unaligned(src.add(n - 2).cast::<u16>());
            ptr::write_unaligned(dst.cast::<u16>(), a);
            ptr::write_unaligned(dst.add(n - 2).cast::<u16>(), b);
        } else if n == 1 {
            *dst = *src;
        }
        return;
    }
    if n <= 32 {
        let a = load(src);
        let b = load(src.add(n - 16));
        store(dst, a);
        store(dst.add(n - 16), b);
        return;
    }
    if n <= 64 {
        let a = load(src);
        let b = load(src.add(16));
        let c = load(src.add(n - 32));
        let d = load(src.add(n - 16));
        store(dst, a);
        store(dst.add(16), b);
        store(dst.add(n - 32), c);
        store(dst.add(n - 16), d);
        return;
    }
    if n <= 128 {
        let a = load(src);
        let b = load(src.add(16));
        let c = load(src.add(32));
        let d = load(src.add(48));
        let e = load(src.add(n - 64));
        let f = load(src.add(n - 48));
        let g = load(src.add(n - 32));
        let h = load(src.add(n - 16));
        store(dst, a);
        store(dst.add(16), b);
        store(dst.add(32), c);
        store(dst.add(48), d);
        store(dst.add(n - 64), e);
        store(dst.add(n - 48), f);
        store(dst.add(n - 32), g);
        store(dst.add(n - 16), h);
        return;
    }
    let dst_ahead = (dst as usize).wrapping_sub(src as usize);
    if dst_ahead >= n {
        let src_ahead = (src as usize).wrapping_sub(dst as usize);
        // A forward `rep movsb` is correct on overlap too, but falls off its
        // fast path when the regions are close.
        if n >= REP_MIN && src_ahead >= n {
            rep_movsb(dst, src, n);
        } else {
            copy_forward(dst, src, n);
        }
    } else {
        copy_backward(dst, src, n);
    }
}

#[inline(always)]
unsafe fn rep_movsb(dst: *mut u8, src: *const u8, n: usize) {
    // The SysV ABI guarantees DF clear on entry, and nothing here sets it.
    asm!(
        "rep movsb",
        inout("rcx") n => _,
        inout("rdi") dst => _,
        inout("rsi") src => _,
        options(nostack, preserves_flags),
    );
}

/// `n > 128`, and `dst` does not start inside `(src, src + n)`.
#[inline(always)]
unsafe fn copy_forward(dst: *mut u8, src: *const u8, n: usize) {
    let head = load(src);
    let t0 = load(src.add(n - 64));
    let t1 = load(src.add(n - 48));
    let t2 = load(src.add(n - 32));
    let t3 = load(src.add(n - 16));
    let tail = dst.add(n - 64);
    let skew = BLOCK - (dst as usize & (BLOCK - 1));
    let mut d = dst.add(skew);
    let mut s = src.add(skew);
    while d < tail {
        let a = load(s);
        let b = load(s.add(16));
        let c = load(s.add(32));
        let e = load(s.add(48));
        store_block(d, a);
        store_block(d.add(16), b);
        store_block(d.add(32), c);
        store_block(d.add(48), e);
        d = d.add(64);
        s = s.add(64);
    }
    store(tail, t0);
    store(tail.add(16), t1);
    store(tail.add(32), t2);
    store(tail.add(48), t3);
    store(dst, head);
}

/// `n > 128`, and `dst` starts inside `(src, src + n)`.
#[inline(always)]
unsafe fn copy_backward(dst: *mut u8, src: *const u8, n: usize) {
    let h0 = load(src);
    let h1 = load(src.add(16));
    let h2 = load(src.add(32));
    let h3 = load(src.add(48));
    let tail = load(src.add(n - 16));
    let end = dst.add(n);
    let mut d = end.sub(end as usize & (BLOCK - 1));
    let mut s = src.add(d.offset_from_unsigned(dst));
    let floor = dst.add(64);
    while d > floor {
        d = d.sub(64);
        s = s.sub(64);
        let a = load(s);
        let b = load(s.add(16));
        let c = load(s.add(32));
        let e = load(s.add(48));
        store_block(d, a);
        store_block(d.add(16), b);
        store_block(d.add(32), c);
        store_block(d.add(48), e);
    }
    store(end.sub(16), tail);
    store(dst, h0);
    store(dst.add(16), h1);
    store(dst.add(32), h2);
    store(dst.add(48), h3);
}

/// `memset`.
///
/// # Safety
/// `dst` is writable for `n` bytes.
#[inline(always)]
pub unsafe fn fill(dst: *mut u8, byte: u8, n: usize) {
    if n < 16 {
        let w = u64::from(byte) * 0x0101_0101_0101_0101;
        if n >= 8 {
            ptr::write_unaligned(dst.cast::<u64>(), w);
            ptr::write_unaligned(dst.add(n - 8).cast::<u64>(), w);
        } else if n >= 4 {
            ptr::write_unaligned(dst.cast::<u32>(), w as u32);
            ptr::write_unaligned(dst.add(n - 4).cast::<u32>(), w as u32);
        } else if n >= 2 {
            ptr::write_unaligned(dst.cast::<u16>(), w as u16);
            ptr::write_unaligned(dst.add(n - 2).cast::<u16>(), w as u16);
        } else if n == 1 {
            *dst = byte;
        }
        return;
    }
    let v = _mm_set1_epi8(byte as i8);
    if n <= 32 {
        store(dst, v);
        store(dst.add(n - 16), v);
        return;
    }
    if n <= 64 {
        store(dst, v);
        store(dst.add(16), v);
        store(dst.add(n - 32), v);
        store(dst.add(n - 16), v);
        return;
    }
    if n >= REP_MIN {
        asm!(
            "rep stosb",
            inout("rcx") n => _,
            inout("rdi") dst => _,
            in("al") byte,
            options(nostack, preserves_flags),
        );
        return;
    }
    let tail = dst.add(n - 64);
    store(dst, v);
    let mut d = dst.add(BLOCK - (dst as usize & (BLOCK - 1)));
    while d < tail {
        store_block(d, v);
        store_block(d.add(16), v);
        store_block(d.add(32), v);
        store_block(d.add(48), v);
        d = d.add(64);
    }
    store(tail, v);
    store(tail.add(16), v);
    store(tail.add(32), v);
    store(tail.add(48), v);
}

/// `wmemset`: `n` copies of `value`.
///
/// # Safety
/// `dst` is writable for `n` elements.
#[inline(always)]
pub unsafe fn fill_u32(dst: *mut u32, value: u32, n: usize) {
    if n < 4 {
        if n > 0 {
            ptr::write_unaligned(dst, value);
        }
        if n > 1 {
            ptr::write_unaligned(dst.add(1), value);
        }
        if n > 2 {
            ptr::write_unaligned(dst.add(2), value);
        }
        return;
    }
    let v = _mm_set1_epi32(value as i32);
    let bytes = n * 4;
    let base = dst.cast::<u8>();
    let last = base.add(bytes - 16);
    let mut d = base;
    while d < last {
        store(d, v);
        d = d.add(16);
    }
    store(last, v);
}

/// `memcmp`: the difference of the first differing byte pair, or zero.
///
/// # Safety
/// `a` and `b` are readable for `n` bytes.
#[inline(always)]
pub unsafe fn compare(a: *const u8, b: *const u8, n: usize) -> i32 {
    if n < 16 {
        return compare_short(a, b, n);
    }
    let mut i = 0usize;
    while i + 16 <= n {
        let diff = !mask_eq(
            _mm_loadu_si128(a.add(i).cast()),
            _mm_loadu_si128(b.add(i).cast()),
        ) & 0xffff;
        if diff != 0 {
            let at = i + diff.trailing_zeros() as usize;
            return byte_diff(*a.add(at), *b.add(at));
        }
        i += 16;
    }
    if i < n {
        // The last block overlaps bytes already found equal, so its first
        // difference is still the first one overall.
        let i = n - 16;
        let diff = !mask_eq(
            _mm_loadu_si128(a.add(i).cast()),
            _mm_loadu_si128(b.add(i).cast()),
        ) & 0xffff;
        if diff != 0 {
            let at = i + diff.trailing_zeros() as usize;
            return byte_diff(*a.add(at), *b.add(at));
        }
    }
    0
}

#[inline(always)]
unsafe fn compare_short(a: *const u8, b: *const u8, n: usize) -> i32 {
    if n >= 8 {
        let r = compare_u64(
            ptr::read_unaligned(a.cast::<u64>()),
            ptr::read_unaligned(b.cast::<u64>()),
        );
        if r != 0 {
            return r;
        }
        return compare_u64(
            ptr::read_unaligned(a.add(n - 8).cast::<u64>()),
            ptr::read_unaligned(b.add(n - 8).cast::<u64>()),
        );
    }
    if n >= 4 {
        let r = compare_u64(
            u64::from(ptr::read_unaligned(a.cast::<u32>())),
            u64::from(ptr::read_unaligned(b.cast::<u32>())),
        );
        if r != 0 {
            return r;
        }
        return compare_u64(
            u64::from(ptr::read_unaligned(a.add(n - 4).cast::<u32>())),
            u64::from(ptr::read_unaligned(b.add(n - 4).cast::<u32>())),
        );
    }
    if n == 0 {
        return 0;
    }
    let r = byte_diff(*a, *b);
    if r != 0 || n == 1 {
        return r;
    }
    let r = byte_diff(*a.add(1), *b.add(1));
    if r != 0 || n == 2 {
        return r;
    }
    byte_diff(*a.add(2), *b.add(2))
}

/// Two little-endian words: the first differing byte is the lowest one.
#[inline(always)]
fn compare_u64(x: u64, y: u64) -> i32 {
    let diff = x ^ y;
    if diff == 0 {
        return 0;
    }
    let shift = diff.trailing_zeros() & !7;
    byte_diff((x >> shift) as u8, (y >> shift) as u8)
}

#[inline(always)]
fn block_of(p: *const u8) -> (*const u8, u32) {
    let skew = p as usize & (BLOCK - 1);
    (p.wrapping_sub(skew), skew as u32)
}

/// `memchr`: the first `byte` in `[s, s + n)`, or null.
///
/// # Safety
/// `s` is readable for `n` bytes.
#[inline(always)]
pub unsafe fn find_byte(s: *const u8, byte: u8, n: usize) -> *const u8 {
    if n == 0 {
        return ptr::null();
    }
    let v = _mm_set1_epi8(byte as i8);
    let (mut block, skew) = block_of(s);
    let mut mask = mask_eq(load_block(block), v) >> skew;
    let mut scanned = 0usize;
    let mut span = BLOCK - skew as usize;
    loop {
        if mask != 0 {
            let at = scanned + mask.trailing_zeros() as usize;
            return if at < n { s.add(at) } else { ptr::null() };
        }
        scanned += span;
        if scanned >= n {
            return ptr::null();
        }
        span = BLOCK;
        block = block.add(BLOCK);
        mask = mask_eq(load_block(block), v);
    }
}

/// `strlen`.
///
/// # Safety
/// `s` is a NUL-terminated string.
#[inline(always)]
pub unsafe fn c_len(s: *const u8) -> usize {
    let zero = _mm_setzero_si128();
    let (mut block, skew) = block_of(s);
    let mut mask = mask_eq(load_block(block), zero) >> skew << skew;
    while mask == 0 {
        block = block.add(BLOCK);
        mask = mask_eq(load_block(block), zero);
    }
    block
        .add(mask.trailing_zeros() as usize)
        .offset_from_unsigned(s)
}

/// `strnlen`.
///
/// # Safety
/// `s` is readable up to its NUL or for `max` bytes, whichever is first.
#[inline(always)]
pub unsafe fn c_len_bounded(s: *const u8, max: usize) -> usize {
    let nul = find_byte(s, 0, max);
    if nul.is_null() {
        max
    } else {
        nul.offset_from_unsigned(s)
    }
}

/// `strchr`: the first `byte` in the string, its terminator included.
///
/// # Safety
/// `s` is a NUL-terminated string.
#[inline(always)]
pub unsafe fn c_find(s: *const u8, byte: u8) -> *const u8 {
    let zero = _mm_setzero_si128();
    let v = _mm_set1_epi8(byte as i8);
    let (mut block, skew) = block_of(s);
    let mut x = load_block(block);
    let mut mask = (mask_eq(x, v) | mask_eq(x, zero)) >> skew << skew;
    while mask == 0 {
        block = block.add(BLOCK);
        x = load_block(block);
        mask = mask_eq(x, v) | mask_eq(x, zero);
    }
    let at = block.add(mask.trailing_zeros() as usize);
    if *at == byte { at } else { ptr::null() }
}

/// `strrchr`: the last `byte` in the string, its terminator included.
///
/// # Safety
/// `s` is a NUL-terminated string.
#[inline(always)]
pub unsafe fn c_find_last(s: *const u8, byte: u8) -> *const u8 {
    let zero = _mm_setzero_si128();
    let v = _mm_set1_epi8(byte as i8);
    let (mut block, skew) = block_of(s);
    let mut found: *const u8 = ptr::null();
    let mut floor = u32::MAX << skew;
    loop {
        let x = load_block(block);
        let mut hits = mask_eq(x, v) & floor;
        let nul = mask_eq(x, zero) & floor;
        if nul != 0 {
            // Bits up to and including the first NUL.
            hits &= nul ^ (nul - 1);
        }
        if hits != 0 {
            found = block.add(31 - hits.leading_zeros() as usize);
        }
        if nul != 0 {
            return found;
        }
        floor = u32::MAX;
        block = block.add(BLOCK);
    }
}

/// Whether the 16 bytes at `p` lie in one page, so an unaligned load there
/// cannot fault when `p` itself is readable.
#[inline(always)]
fn block_fits_page(p: *const u8) -> bool {
    p as usize & (PAGE - 1) <= PAGE - BLOCK
}

/// `strncmp` with `n = usize::MAX` is `strcmp`.
///
/// # Safety
/// `a` and `b` are readable up to their NUL or for `n` bytes, whichever is
/// first.
#[inline(always)]
pub unsafe fn c_compare(a: *const u8, b: *const u8, n: usize) -> i32 {
    let zero = _mm_setzero_si128();
    let mut i = 0usize;
    while i < n {
        let (pa, pb) = (a.add(i), b.add(i));
        if block_fits_page(pa) && block_fits_page(pb) {
            let x = load(pa);
            let y = load(pb);
            let mut stop = (!mask_eq(x, y) | mask_eq(x, zero)) & 0xffff;
            let left = n - i;
            if left < BLOCK {
                stop &= (1u32 << left) - 1;
            }
            if stop != 0 {
                let at = stop.trailing_zeros() as usize;
                return byte_diff(*pa.add(at), *pb.add(at));
            }
            i += BLOCK;
        } else {
            let (ca, cb) = (*pa, *pb);
            if ca != cb || ca == 0 {
                return byte_diff(ca, cb);
            }
            i += 1;
        }
    }
    0
}

/// `wcslen`.
///
/// # Safety
/// `s` is a NUL-terminated wide string.
#[inline(always)]
pub unsafe fn wide_len(s: *const u32) -> usize {
    if s as usize & 3 != 0 {
        return wide_len_unaligned(s);
    }
    let zero = _mm_setzero_si128();
    let (mut block, skew) = block_of(s.cast());
    let mut x = _mm_cmpeq_epi32(load_block(block), zero);
    let mut mask = _mm_movemask_epi8(x) as u32 >> skew << skew;
    while mask == 0 {
        block = block.add(BLOCK);
        x = _mm_cmpeq_epi32(load_block(block), zero);
        mask = _mm_movemask_epi8(x) as u32;
    }
    block
        .add(mask.trailing_zeros() as usize)
        .offset_from_unsigned(s.cast())
        / 4
}

/// A `wchar_t *` off its natural alignment is already undefined in C; this
/// only keeps such a caller from reading past a page end.
#[inline(never)]
unsafe fn wide_len_unaligned(s: *const u32) -> usize {
    let mut n = 0usize;
    // Opaque step: the plain loop is the `wcslen` idiom LLVM would replace
    // with a call to `wcslen`.
    while ptr::read_unaligned(core::hint::black_box(s.add(n))) != 0 {
        n += 1;
    }
    n
}
