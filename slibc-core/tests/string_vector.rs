//! slibc's vectorised `mem*`/`str*` bodies against byte-at-a-time
//! references: every length to 256, every source and destination alignment
//! in a cache line, overlap in both directions, and strings that end at a
//! page followed by an unmapped one.

#![cfg(target_arch = "x86_64")]

#[allow(unsafe_op_in_unsafe_fn)]
#[path = "../../slibc/src/string/vector.rs"]
mod vector;

use std::ptr;

const PAD: usize = 64;
const MAX_LEN: usize = 256;
const ALIGNS: usize = 64;
/// Lengths past the short cases: both sides of the 64-byte loop's edges and
/// of the `rep` threshold.
const LONG_LENS: &[usize] = &[
    300, 511, 512, 513, 1000, 1023, 2047, 2048, 2049, 4095, 4096, 4097, 10_000, 65_537,
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    fn nonzero(&mut self) -> u8 {
        loop {
            let b = self.byte();
            if b != 0 {
                return b;
            }
        }
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.byte();
        }
    }
}

/// A 64-byte-aligned buffer, so an offset into it is that alignment.
struct Aligned {
    storage: Vec<u8>,
    start: usize,
    len: usize,
}

impl Aligned {
    fn new(len: usize) -> Self {
        let storage = vec![0u8; len + 128];
        let start = storage.as_ptr().align_offset(64);
        Self {
            storage,
            start,
            len,
        }
    }

    fn bytes(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }
}

/// Every index of a short range; the edges and a stride through a long one.
fn positions(n: usize) -> impl Iterator<Item = usize> {
    (0..n).filter(move |&i| n <= 48 || i < 24 || i + 24 >= n || i % 5 == 0)
}

fn ref_memcmp(a: &[u8], b: &[u8]) -> i32 {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return *x as i32 - *y as i32;
        }
    }
    0
}

fn ref_strncmp(a: &[u8], b: &[u8], n: usize) -> i32 {
    for i in 0..n {
        let (x, y) = (a[i], b[i]);
        if x != y || x == 0 {
            return x as i32 - y as i32;
        }
    }
    0
}

fn check_copy(buf: &mut [u8], dst: usize, src: usize, n: usize, rng: &mut Rng) {
    rng.fill(buf);
    let mut expect = buf.to_vec();
    expect.copy_within(src..src + n, dst);
    let base = buf.as_mut_ptr();
    unsafe { vector::copy(base.add(dst), base.add(src), n) };
    assert!(
        buf == &expect[..],
        "copy n={n} dst={dst} src={src}: first bad byte at {:?}",
        buf.iter().zip(&expect).position(|(a, b)| a != b)
    );
}

#[test]
fn copy_disjoint_every_length_and_alignment() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut buf = Aligned::new(2 * (MAX_LEN + ALIGNS + PAD));
    let half = MAX_LEN + ALIGNS + PAD;
    for n in 0..=MAX_LEN {
        for sa in 0..ALIGNS {
            for da in 0..ALIGNS {
                let bytes = buf.bytes();
                let window = 2 * half;
                let (src, dst) = (PAD / 2 + sa, half + PAD / 2 + da);
                // Only the window a stray write could reach needs refreshing.
                let lo = dst - PAD / 2;
                let hi = (dst + n + PAD / 2).min(window);
                rng.fill(&mut bytes[src..src + n]);
                rng.fill(&mut bytes[lo..hi]);
                let expect: Vec<u8> = {
                    let mut e = bytes[lo..hi].to_vec();
                    e[dst - lo..dst - lo + n].copy_from_slice(&bytes[src..src + n]);
                    e
                };
                let base = bytes.as_mut_ptr();
                unsafe { vector::copy(base.add(dst), base.add(src), n) };
                assert!(bytes[lo..hi] == expect[..], "copy n={n} sa={sa} da={da}");
            }
        }
    }
}

#[test]
fn copy_overlapping_both_directions() {
    let mut rng = Rng(7);
    let mut buf = vec![0u8; 2 * MAX_LEN + 4 * PAD];
    for n in 0..=MAX_LEN {
        for delta in 1..=(n.min(80) + 1) {
            for base in [PAD, PAD + 1, PAD + 7, PAD + 13, PAD + 31] {
                check_copy(&mut buf, base + delta, base, n, &mut rng);
                check_copy(&mut buf, base, base + delta, n, &mut rng);
            }
        }
        check_copy(&mut buf, PAD, PAD, n, &mut rng);
    }
}

#[test]
fn copy_long_disjoint_and_overlapping() {
    let mut rng = Rng(11);
    for &n in LONG_LENS {
        let mut buf = vec![0u8; 2 * n + 3 * 4096];
        for (sa, da) in [
            (0, 0),
            (1, 0),
            (0, 1),
            (7, 63),
            (63, 17),
            (32, 16),
            (15, 15),
        ] {
            check_copy(&mut buf, n + 4096 + da, PAD + sa, n, &mut rng);
            check_copy(&mut buf, PAD + da, n + 4096 + sa, n, &mut rng);
        }
        for delta in [
            1,
            2,
            15,
            16,
            17,
            63,
            64,
            65,
            127,
            128,
            129,
            4096,
            n / 2,
            n - 1,
        ] {
            if delta == 0 || delta >= n {
                continue;
            }
            for sa in [0, 3, 16, 61] {
                check_copy(&mut buf, PAD + sa + delta, PAD + sa, n, &mut rng);
                check_copy(&mut buf, PAD + sa, PAD + sa + delta, n, &mut rng);
            }
        }
    }
}

#[test]
fn fill_every_length_and_alignment() {
    let mut rng = Rng(3);
    let mut buf = Aligned::new(MAX_LEN + ALIGNS + 2 * PAD);
    let mut lens: Vec<usize> = (0..=MAX_LEN).collect();
    lens.extend_from_slice(LONG_LENS);
    for n in lens {
        let mut big;
        let bytes = if n + ALIGNS + 2 * PAD > buf.len {
            big = Aligned::new(n + ALIGNS + 2 * PAD);
            big.bytes()
        } else {
            buf.bytes()
        };
        for align in 0..ALIGNS {
            for value in [0u8, 0xff, 0x5a, 0x80, 1] {
                let dst = PAD + align;
                let hi = dst + n + PAD;
                rng.fill(&mut bytes[align..hi]);
                let mut expect = bytes[align..hi].to_vec();
                expect[dst - align..dst - align + n].fill(value);
                unsafe { vector::fill(bytes.as_mut_ptr().add(dst), value, n) };
                assert!(bytes[align..hi] == expect[..], "fill n={n} align={align}");
            }
        }
    }
}

#[test]
fn fill_u32_every_length_and_alignment() {
    let mut rng = Rng(5);
    let mut buf = Aligned::new(4 * 1100 + ALIGNS + 2 * PAD);
    for n in (0..=260).chain([511, 1024, 1025]) {
        for align in 0..ALIGNS {
            let bytes = buf.bytes();
            let dst = PAD + align;
            let hi = dst + 4 * n + PAD;
            rng.fill(&mut bytes[..hi]);
            let value = 0x8012_34fe_u32;
            let mut expect = bytes[..hi].to_vec();
            for k in 0..n {
                expect[dst + 4 * k..dst + 4 * k + 4].copy_from_slice(&value.to_ne_bytes());
            }
            unsafe { vector::fill_u32(bytes.as_mut_ptr().add(dst).cast(), value, n) };
            assert!(bytes[..hi] == expect[..], "fill_u32 n={n} align={align}");
        }
    }
}

#[test]
fn compare_every_length_alignment_and_difference() {
    let mut rng = Rng(13);
    let mut a = Aligned::new(MAX_LEN + ALIGNS + PAD);
    let mut b = Aligned::new(MAX_LEN + ALIGNS + PAD);
    for n in 0..=MAX_LEN {
        for aa in 0..ALIGNS {
            for ba in 0..ALIGNS {
                let (x, y) = (a.bytes(), b.bytes());
                rng.fill(&mut x[aa..aa + n]);
                y[ba..ba + n].copy_from_slice(&x[aa..aa + n]);
                // Differing bytes just past the end must not count.
                x[aa + n] = 1;
                y[ba + n] = 2;
                let got = unsafe { vector::compare(x.as_ptr().add(aa), y.as_ptr().add(ba), n) };
                assert_eq!(got, 0, "equal n={n} aa={aa} ba={ba}");
                if n == 0 || ![0, 1, 15, ALIGNS - 1].contains(&ba) {
                    continue;
                }
                for at in positions(n) {
                    let keep = y[ba + at];
                    for delta in [1u8, 0xff] {
                        y[ba + at] = x[aa + at].wrapping_add(delta);
                        // A later difference in the other direction must
                        // not mask the first.
                        let later = (at + 1 + (at % 5)).min(n - 1);
                        let keep_later = y[ba + later];
                        if later > at {
                            y[ba + later] = x[aa + later].wrapping_sub(delta);
                        }
                        let want = x[aa + at] as i32 - y[ba + at] as i32;
                        if n <= 32 {
                            assert_eq!(want, ref_memcmp(&x[aa..aa + n], &y[ba..ba + n]));
                        }
                        let got =
                            unsafe { vector::compare(x.as_ptr().add(aa), y.as_ptr().add(ba), n) };
                        assert_eq!(got, want, "diff n={n} aa={aa} ba={ba} at={at}");
                        y[ba + later] = keep_later;
                    }
                    y[ba + at] = keep;
                }
            }
        }
    }
}

#[test]
fn compare_long() {
    let mut rng = Rng(17);
    for &n in LONG_LENS {
        let mut x = vec![0u8; n + 64];
        let mut y = vec![0u8; n + 64];
        rng.fill(&mut x);
        for (aa, ba) in [(0, 0), (1, 3), (17, 0), (63, 62)] {
            y[ba..ba + n].copy_from_slice(&x[aa..aa + n]);
            let got = unsafe { vector::compare(x.as_ptr().add(aa), y.as_ptr().add(ba), n) };
            assert_eq!(got, 0);
            for at in [0, 1, 15, 16, 17, n / 2, n - 17, n - 16, n - 1] {
                let keep = y[ba + at];
                y[ba + at] = x[aa + at] ^ 0x81;
                let want = ref_memcmp(&x[aa..aa + n], &y[ba..ba + n]);
                let got = unsafe { vector::compare(x.as_ptr().add(aa), y.as_ptr().add(ba), n) };
                assert_eq!(got, want, "long n={n} at={at}");
                y[ba + at] = keep;
            }
        }
    }
}

#[test]
fn find_byte_every_length_alignment_and_position() {
    let mut rng = Rng(19);
    let mut buf = Aligned::new(MAX_LEN + ALIGNS + PAD);
    for n in 0..=MAX_LEN {
        for align in 0..ALIGNS {
            let bytes = buf.bytes();
            for b in bytes.iter_mut() {
                *b = rng.nonzero() | 1;
            }
            let s = unsafe { bytes.as_ptr().add(align) };
            // Present only just outside the range, on both sides.
            if align > 0 {
                bytes[align - 1] = 0;
            }
            bytes[align + n] = 0;
            assert!(
                unsafe { vector::find_byte(s, 0, n) }.is_null(),
                "n={n} align={align}"
            );
            for at in (0..n)
                .step_by(if n > 64 { 7 } else { 1 })
                .chain(n.checked_sub(1))
            {
                bytes[align + at] = 0;
                let got = unsafe { vector::find_byte(s, 0, n) };
                assert_eq!(got, unsafe { s.add(at) }, "n={n} align={align} at={at}");
                // A later match does not move the answer.
                if at + 1 < n {
                    bytes[align + n - 1] = 0;
                    assert_eq!(unsafe { vector::find_byte(s, 0, n) }, unsafe { s.add(at) });
                    bytes[align + n - 1] = 3;
                }
                bytes[align + at] = 5;
            }
            // A value with the high bit set.
            bytes[align..align + n].fill(0x7f);
            if n > 0 {
                bytes[align + n - 1] = 0xfe;
                assert_eq!(unsafe { vector::find_byte(s, 0xfe, n) }, unsafe {
                    s.add(n - 1)
                });
            }
        }
    }
}

fn nul_terminated(rng: &mut Rng, bytes: &mut [u8], at: usize, len: usize) {
    for b in bytes.iter_mut() {
        *b = rng.byte();
    }
    for b in &mut bytes[at..at + len] {
        *b = rng.nonzero();
    }
    bytes[at + len] = 0;
}

#[test]
fn c_len_and_bounded_every_length_and_alignment() {
    let mut rng = Rng(23);
    let mut buf = Aligned::new(MAX_LEN + ALIGNS + PAD);
    for len in 0..=MAX_LEN {
        for align in 0..ALIGNS {
            let bytes = buf.bytes();
            nul_terminated(&mut rng, bytes, align, len);
            if align > 0 {
                bytes[align - 1] = 0;
            }
            let s = unsafe { bytes.as_ptr().add(align) };
            assert_eq!(unsafe { vector::c_len(s) }, len, "len={len} align={align}");
            for max in [
                0,
                1,
                len.saturating_sub(1),
                len,
                len + 1,
                len + 17,
                usize::MAX,
            ] {
                assert_eq!(
                    unsafe { vector::c_len_bounded(s, max) },
                    len.min(max),
                    "len={len} align={align} max={max}"
                );
            }
        }
    }
}

#[test]
fn c_find_and_find_last_every_position() {
    let mut rng = Rng(29);
    let mut buf = Aligned::new(MAX_LEN + ALIGNS + PAD);
    for len in 0..=MAX_LEN {
        for align in 0..ALIGNS {
            let bytes = buf.bytes();
            for b in bytes.iter_mut() {
                *b = 0x41;
            }
            if align > 0 {
                bytes[align - 1] = 0x42;
            }
            bytes[align + len] = 0;
            // Past the terminator the byte must not be found.
            bytes[align + len + 1] = 0x42;
            let s = unsafe { bytes.as_ptr().add(align) };
            unsafe {
                assert!(vector::c_find(s, 0x42).is_null(), "len={len} align={align}");
                assert!(vector::c_find_last(s, 0x42).is_null());
                assert_eq!(vector::c_find(s, 0), s.add(len));
                assert_eq!(vector::c_find_last(s, 0), s.add(len));
                if len > 0 {
                    assert_eq!(vector::c_find(s, 0x41), s);
                    assert_eq!(vector::c_find_last(s, 0x41), s.add(len - 1));
                }
            }
            let stride = if len > 64 { 5 } else { 1 };
            for at in (0..len).step_by(stride) {
                bytes[align + at] = 0xc3;
                unsafe {
                    assert_eq!(vector::c_find(s, 0xc3), s.add(at), "len={len} at={at}");
                    assert_eq!(vector::c_find_last(s, 0xc3), s.add(at));
                }
                let later = (at + 1 + rng.next() as usize % (len - at)).min(len - 1);
                if later > at {
                    bytes[align + later] = 0xc3;
                    unsafe {
                        assert_eq!(vector::c_find(s, 0xc3), s.add(at));
                        assert_eq!(vector::c_find_last(s, 0xc3), s.add(later), "later={later}");
                    }
                    bytes[align + later] = 0x41;
                }
                bytes[align + at] = 0x41;
            }
        }
    }
}

#[test]
fn c_compare_every_length_alignment_and_difference() {
    let mut rng = Rng(31);
    let mut a = Aligned::new(MAX_LEN + ALIGNS + PAD);
    let mut b = Aligned::new(MAX_LEN + ALIGNS + PAD);
    for len in 0..=MAX_LEN {
        for aa in 0..ALIGNS {
            for ba in (0..ALIGNS).filter(|ba| [0, 1, 15, ALIGNS - 1, aa].contains(ba)) {
                let (x, y) = (a.bytes(), b.bytes());
                nul_terminated(&mut rng, x, aa, len);
                y.copy_from_slice(x);
                y.copy_within(aa..aa + len + 1, ba);
                y[ba + len + 1..].fill(0x33);
                let (px, py) = unsafe { (x.as_ptr().add(aa), y.as_ptr().add(ba)) };
                unsafe {
                    assert_eq!(vector::c_compare(px, py, usize::MAX), 0, "len={len}");
                    assert_eq!(vector::c_compare(px, py, len + 1), 0);
                }
                for at in positions(len + 1) {
                    let keep = y[ba + at];
                    for alt in [rng.nonzero(), 0, 0xff] {
                        if alt == keep {
                            continue;
                        }
                        y[ba + at] = alt;
                        let (xs, ys) = (&x[aa..], &y[ba..]);
                        for n in [usize::MAX, at, at + 1, at + 2, len + 1] {
                            let want = if n.min(len + 1) > at {
                                x[aa + at] as i32 - alt as i32
                            } else {
                                0
                            };
                            if len <= 32 {
                                assert_eq!(want, ref_strncmp(xs, ys, n.min(len + 1)));
                            }
                            let got = unsafe { vector::c_compare(px, py, n) };
                            assert_eq!(got, want, "len={len} aa={aa} ba={ba} at={at} n={n}");
                        }
                    }
                    y[ba + at] = keep;
                }
            }
        }
    }
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

/// Two readable pages followed by an inaccessible one.
struct Guarded(*mut u8);

impl Guarded {
    const PAGE: usize = 4096;

    fn new() -> Self {
        const PROT_READ_WRITE: i32 = 3;
        const MAP_PRIVATE_ANON: i32 = 0x02 | 0x20;
        let p = unsafe {
            mmap(
                ptr::null_mut(),
                3 * Self::PAGE,
                PROT_READ_WRITE,
                MAP_PRIVATE_ANON,
                -1,
                0,
            )
        };
        assert!(!p.is_null() && p as isize != -1);
        assert_eq!(unsafe { mprotect(p.add(2 * Self::PAGE), Self::PAGE, 0) }, 0);
        Self(p)
    }

    fn end(&self) -> *mut u8 {
        unsafe { self.0.add(2 * Self::PAGE) }
    }

    /// A string of `len` bytes whose terminator is the last readable byte.
    fn string_at_end(&self, len: usize, fill: u8) -> *const u8 {
        unsafe {
            let s = self.end().sub(len + 1);
            ptr::write_bytes(self.0, fill ^ 0x10, 2 * Self::PAGE);
            ptr::write_bytes(s, fill, len);
            *s.add(len) = 0;
            s
        }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        unsafe { munmap(self.0, 3 * Self::PAGE) };
    }
}

#[test]
fn scans_stop_at_the_page_end() {
    let g = Guarded::new();
    let h = Guarded::new();
    for len in 0..=300 {
        let s = g.string_at_end(len, b'a');
        unsafe {
            assert_eq!(vector::c_len(s), len);
            assert_eq!(vector::c_len_bounded(s, usize::MAX), len);
            assert_eq!(vector::c_len_bounded(s, len + 1), len);
            assert!(vector::c_find(s, b'z').is_null());
            assert_eq!(vector::c_find(s, 0), s.add(len));
            assert!(vector::c_find_last(s, b'z').is_null());
            assert_eq!(vector::find_byte(s, 0, len + 1), s.add(len));
            assert!(vector::find_byte(s, b'z', len + 1).is_null());
            assert_eq!(vector::compare(s, s, len + 1), 0);
            if len > 0 {
                assert_eq!(vector::c_find_last(s, b'a'), s.add(len - 1));
            }
            // Both strings at a page end, and every pairing of lengths that
            // makes one run into its guard page while the other has not.
            let t = h.string_at_end(len, b'a');
            assert_eq!(vector::c_compare(s, t, usize::MAX), 0);
            for shorter in [0, len / 2, len.saturating_sub(1)] {
                let t = h.string_at_end(shorter, b'a');
                let want = if shorter == len { 0 } else { b'a' as i32 };
                assert_eq!(
                    vector::c_compare(s, t, usize::MAX),
                    want,
                    "len={len} shorter={shorter}"
                );
                assert_eq!(vector::c_compare(t, s, usize::MAX), -want);
                assert_eq!(vector::c_compare(s, t, len + 1), want);
            }
            // A bounded compare may stop short of the terminator; the bytes
            // it may not read are the guard page's.
            let unterminated = g.end().sub(len);
            ptr::write_bytes(unterminated, b'q', len);
            let t = h.end().sub(len);
            ptr::write_bytes(t, b'q', len);
            assert_eq!(vector::c_compare(unterminated, t, len), 0);
            assert_eq!(vector::compare(unterminated, t, len), 0);
            assert!(vector::find_byte(unterminated, 0, len).is_null());
            assert_eq!(vector::c_len_bounded(unterminated, len), len);
        }
    }
}

#[test]
fn wide_len_every_length_and_alignment() {
    let mut buf = Aligned::new(4 * 200 + ALIGNS + PAD);
    for len in 0..=150 {
        for align in 0..ALIGNS {
            let bytes = buf.bytes();
            bytes.fill(0);
            let s = unsafe { bytes.as_mut_ptr().add(PAD / 2 + align) };
            for k in 0..len {
                unsafe { ptr::write_unaligned(s.cast::<u32>().add(k), 0x0100_0000 + k as u32) };
            }
            // Zero bytes inside nonzero elements must not stop the scan.
            if len > 2 {
                unsafe { ptr::write_unaligned(s.cast::<u32>().add(1), 0x0000_0100) };
            }
            unsafe { ptr::write_unaligned(s.cast::<u32>().add(len), 0) };
            assert_eq!(
                unsafe { vector::wide_len(s.cast()) },
                len,
                "len={len} align={align}"
            );
        }
    }
    let g = Guarded::new();
    for len in 0..=300 {
        unsafe {
            let s = g.end().cast::<u32>().sub(len + 1);
            for k in 0..len {
                *s.add(k) = 0x41;
            }
            *s.add(len) = 0;
            assert_eq!(vector::wide_len(s), len);
        }
    }
}
