//! `qsort(3)` and `bsearch(3)`.
//!
//! No `qsort_r`: LLVM's `array_pod_sort`, the caller that decides this
//! function's shape, says in as many words that it uses plain `qsort` on
//! purpose, and nothing else in the toolchain asks for the context-passing
//! form.
//!
//! The comparator is an arbitrary C function pointer. It may disagree with
//! itself between calls and it may re-enter `qsort`, and neither is allowed to
//! move an index past the end of the array: every index below is derived from
//! the range's own `lo`/`hi` and tested against them before it is used, rather
//! than trusting the pivot to stop a scan. Re-entrancy costs nothing because
//! all of the sort's state, the pending-range stack included, is automatic.

use core::ffi::{c_int, c_void};

use crate::ffi::size_t;

type Compar = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;

/// Ranges this short go to binary insertion sort instead of partitioning.
/// `array_pod_sort` nearly always lands here.
const INSERTION_MAX: usize = 16;

/// Above this many elements a ninther's six extra comparisons buy a better
/// pivot than a plain median-of-three does.
const NINTHER_MIN: usize = 128;

/// Pending ranges. Only the *larger* half is pushed and the loop continues on
/// the smaller, so the range at stack depth `t` holds at most `n >> t`
/// elements; pushing at all needs more than [`INSERTION_MAX`], which bounds
/// `t` below `log2(n) - 4`. No array a 64-bit address space can hold fills 64
/// slots; pushing the smaller half would have no such bound.
const STACK_MAX: usize = 64;

/// How to exchange two elements. Chosen once per `qsort` call, and never by
/// way of an element-sized temporary: `size` is a runtime value, so that
/// temporary would have to be an allocation.
#[derive(Clone, Copy)]
enum Swap {
    Word8,
    Word4,
    /// `n` aligned 8-byte words.
    Words(usize),
    Bytes(usize),
}

impl Swap {
    fn select(base: *mut u8, size: usize) -> Self {
        // Every element sits at `base + k * size`, so these two divisibility
        // facts are what make each element pointer aligned.
        let word_aligned = base as usize % 8 == 0;
        match size {
            8 if word_aligned => Swap::Word8,
            4 if base as usize % 4 == 0 => Swap::Word4,
            _ if word_aligned && size % 8 == 0 => Swap::Words(size / 8),
            _ => Swap::Bytes(size),
        }
    }

    #[inline]
    unsafe fn exchange(self, a: *mut u8, b: *mut u8) {
        match self {
            Swap::Word8 => {
                let (a, b) = (a.cast::<u64>(), b.cast::<u64>());
                let held = *a;
                *a = *b;
                *b = held;
            }
            Swap::Word4 => {
                let (a, b) = (a.cast::<u32>(), b.cast::<u32>());
                let held = *a;
                *a = *b;
                *b = held;
            }
            Swap::Words(words) => {
                let (a, b) = (a.cast::<u64>(), b.cast::<u64>());
                for i in 0..words {
                    let held = *a.add(i);
                    *a.add(i) = *b.add(i);
                    *b.add(i) = held;
                }
            }
            Swap::Bytes(bytes) => {
                for i in 0..bytes {
                    let held = *a.add(i);
                    *a.add(i) = *b.add(i);
                    *b.add(i) = held;
                }
            }
        }
    }
}

struct Array {
    base: *mut u8,
    size: usize,
    compar: Compar,
    swap: Swap,
}

impl Array {
    #[inline]
    unsafe fn at(&self, i: usize) -> *mut u8 {
        self.base.add(i * self.size)
    }

    #[inline]
    unsafe fn lt(&self, a: usize, b: usize) -> bool {
        (self.compar)(self.at(a).cast(), self.at(b).cast()) < 0
    }

    #[inline]
    unsafe fn le(&self, a: usize, b: usize) -> bool {
        (self.compar)(self.at(a).cast(), self.at(b).cast()) <= 0
    }

    #[inline]
    unsafe fn exchange(&self, a: usize, b: usize) {
        self.swap.exchange(self.at(a), self.at(b));
    }

    /// Introsort over `[0, n)`: quicksort, insertion sort under
    /// [`INSERTION_MAX`], and heapsort once a branch has partitioned
    /// `2 * floor(log2(n))` times without getting anywhere.
    unsafe fn sort(&self, n: usize) {
        let mut stack = [(0usize, 0usize, 0u32); STACK_MAX];
        let mut top = 0usize;
        let mut lo = 0usize;
        let mut count = n;
        let mut depth = 2 * (usize::BITS - 1 - n.leading_zeros());

        loop {
            if count > INSERTION_MAX && depth > 0 {
                depth -= 1;
                let pivot = self.partition(lo, lo + count - 1);
                let left = pivot - lo;
                let right = count - left - 1;
                if left < right {
                    stack[top] = (pivot + 1, right, depth);
                    count = left;
                } else {
                    stack[top] = (lo, left, depth);
                    lo = pivot + 1;
                    count = right;
                }
                top += 1;
                continue;
            }
            if count > INSERTION_MAX {
                self.heapsort(lo, count);
            } else {
                self.insertion_sort(lo, count);
            }
            if top == 0 {
                return;
            }
            top -= 1;
            (lo, count, depth) = stack[top];
        }
    }

    /// The binary search costs `log2(i)` comparator calls per element where a
    /// linear scan costs `i`, and the comparator is an indirect call.
    unsafe fn insertion_sort(&self, lo: usize, n: usize) {
        for i in lo + 1..lo + n {
            let mut left = lo;
            let mut right = i;
            while left < right {
                let mid = left + (right - left) / 2;
                if self.lt(i, mid) {
                    right = mid;
                } else {
                    left = mid + 1;
                }
            }
            let mut at = i;
            while at > left {
                self.exchange(at, at - 1);
                at -= 1;
            }
        }
    }

    /// Partitions the inclusive range `[lo, hi]` and answers where the pivot
    /// came to rest. Each scan stops at `i > hi` or `j == lo` rather than on
    /// the pivot, so a comparator that answers inconsistently can only
    /// produce a bad split, never a read outside `[lo, hi]`.
    ///
    /// The pivot stays parked at `lo` for the whole scan: with a runtime
    /// element size, a pivot copy would have to be an allocation. Both scans
    /// stop on an element that compares equal, so an all-equal range splits
    /// down the middle instead of shedding one element a level.
    unsafe fn partition(&self, lo: usize, hi: usize) -> usize {
        self.place_pivot(lo, hi);

        let mut i = lo;
        let mut j = hi + 1;
        loop {
            loop {
                i += 1;
                if i > hi || !self.lt(i, lo) {
                    break;
                }
            }
            loop {
                j -= 1;
                if j == lo || self.le(j, lo) {
                    break;
                }
            }
            if i >= j {
                break;
            }
            self.exchange(i, j);
        }

        if j != lo {
            self.exchange(lo, j);
        }
        j
    }

    /// Moves a median-of-three (median-of-nine above [`NINTHER_MIN`]) to `lo`.
    ///
    /// The last two steps sort the three rather than swapping the median into
    /// `lo`: a bare swap deposits whatever was at `lo` mid-range, which on
    /// descending input is the maximum and becomes the right half's median —
    /// a feedback that degenerates into heapsort.
    unsafe fn place_pivot(&self, lo: usize, hi: usize) {
        let n = hi - lo + 1;
        let mid = lo + n / 2;
        if n >= NINTHER_MIN {
            let step = n / 8;
            let low = self.median3(lo, lo + step, lo + 2 * step);
            let centre = self.median3(mid - step, mid, mid + step);
            let high = self.median3(hi - 2 * step, hi - step, hi);
            let ninther = self.median3(low, centre, high);
            if ninther != mid {
                self.exchange(ninther, mid);
            }
        }
        self.sort3(lo, mid, hi);
        self.exchange(lo, mid);
    }

    /// Orders `a <= b <= c` in at most three comparisons.
    unsafe fn sort3(&self, a: usize, b: usize, c: usize) {
        if self.lt(b, a) {
            self.exchange(a, b);
        }
        if self.lt(c, b) {
            self.exchange(b, c);
            if self.lt(b, a) {
                self.exchange(a, b);
            }
        }
    }

    unsafe fn median3(&self, a: usize, b: usize, c: usize) -> usize {
        if self.lt(a, b) {
            if self.lt(b, c) {
                b
            } else if self.lt(a, c) {
                c
            } else {
                a
            }
        } else if !self.lt(b, c) {
            b
        } else if self.lt(a, c) {
            a
        } else {
            c
        }
    }

    unsafe fn heapsort(&self, lo: usize, n: usize) {
        let mut parent = n / 2;
        while parent > 0 {
            parent -= 1;
            self.sift_down(lo, parent, n);
        }
        let mut end = n;
        while end > 1 {
            end -= 1;
            self.exchange(lo, lo + end);
            self.sift_down(lo, 0, end);
        }
    }

    unsafe fn sift_down(&self, lo: usize, mut root: usize, n: usize) {
        loop {
            let mut child = 2 * root + 1;
            if child >= n {
                return;
            }
            if child + 1 < n && self.lt(lo + child, lo + child + 1) {
                child += 1;
            }
            if !self.lt(lo + root, lo + child) {
                return;
            }
            self.exchange(lo + root, lo + child);
            root = child;
        }
    }
}

/// `qsort(3)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qsort(
    base: *mut c_void,
    nmemb: size_t,
    size: size_t,
    compar: Option<Compar>,
) {
    let Some(compar) = compar else {
        return;
    };
    if base.is_null() || nmemb < 2 || size == 0 {
        return;
    }
    let base = base.cast::<u8>();
    Array {
        base,
        size,
        compar,
        swap: Swap::select(base, size),
    }
    .sort(nmemb);
}

/// `bsearch(3)`. `NULL` when no element compares equal to `key`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    nmemb: size_t,
    size: size_t,
    compar: Option<Compar>,
) -> *mut c_void {
    let Some(compar) = compar else {
        return core::ptr::null_mut();
    };
    if key.is_null() || base.is_null() || nmemb == 0 || size == 0 {
        return core::ptr::null_mut();
    }
    let base = base.cast::<u8>();
    let mut lo = 0usize;
    let mut hi = nmemb;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let elem = base.add(mid * size);
        // `key` first: C fixes this order, and the whole search inverts if it
        // is the other way round.
        let order = compar(key, elem.cast());
        if order == 0 {
            return elem as *mut c_void;
        }
        if order < 0 {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    core::ptr::null_mut()
}
