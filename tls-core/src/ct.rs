//! Constant-time comparison and masking, and wiping the optimiser may not elide.

pub fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    core::hint::black_box(diff) == 0
}

/// All ones when `bit` is 1, zero when it is 0. `black_box` hides that the mask
/// has two values, which would otherwise let a select on it become a branch.
pub fn mask64(bit: u64) -> u64 {
    core::hint::black_box(0u64.wrapping_sub(bit & 1))
}

/// Zero `buf` in a way the optimiser may not drop as a dead store.
pub fn wipe<T: Copy + Default>(buf: &mut [T]) {
    buf.fill(T::default());
    core::hint::black_box(buf);
}
