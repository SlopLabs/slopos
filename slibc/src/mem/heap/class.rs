//! Size classes: exact 16-byte steps to 128 bytes, then eight classes per
//! doubling, so a block is never more than 12.5% larger than its request.

use super::{MAX_CLASS_SIZE, SLICE_SIZE};

/// Classes run `1..=CLASSES`; 0 marks a span that is not class-managed.
pub const CLASSES: usize = 96;

/// Spans of a class hold at least this many blocks where the cap allows.
const MIN_BLOCKS_PER_SPAN: usize = 8;
const MAX_SPAN_SLICES: usize = 16;

#[inline]
pub fn class_of(size: usize) -> usize {
    debug_assert!(size <= MAX_CLASS_SIZE);
    let words = (size + 15) >> 4;
    if words <= 8 {
        return if words == 0 { 1 } else { words };
    }
    let w = words - 1;
    let bit = (usize::BITS - 1 - w.leading_zeros()) as usize;
    ((bit - 2) << 3) + ((w >> (bit - 3)) & 7) + 1
}

#[inline]
pub const fn class_size(class: usize) -> usize {
    if class <= 8 {
        return class << 4;
    }
    let bit = (class - 1) / 8 + 2;
    let step = (class - 1) % 8;
    ((9 + step) << (bit - 3)) << 4
}

/// Slices one span of `class` covers.
pub const fn span_slices(class: usize) -> usize {
    let wanted = class_size(class) * MIN_BLOCKS_PER_SPAN;
    let slices = wanted.div_ceil(SLICE_SIZE);
    if slices == 0 {
        1
    } else if slices > MAX_SPAN_SLICES {
        MAX_SPAN_SLICES
    } else {
        slices
    }
}

const _: () = assert!(class_size(CLASSES) == MAX_CLASS_SIZE);
const _: () = assert!(span_slices(CLASSES) * SLICE_SIZE / MAX_CLASS_SIZE >= 4);
