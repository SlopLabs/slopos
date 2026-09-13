unsafe extern "C" {
    fn slopos_getrandom(buf: *mut u8, len: usize, flags: u32) -> isize;
}

/// Fill `bytes` with cryptographically secure random data from the kernel.
pub fn fill_bytes(bytes: &mut [u8]) {
    let mut filled = 0usize;
    while filled < bytes.len() {
        let ptr = unsafe { bytes.as_mut_ptr().add(filled) };
        let n = unsafe { slopos_getrandom(ptr, bytes.len() - filled, 0) };
        if n <= 0 {
            // A seeded CSPRNG never refuses, but a bounded loop beats a hang.
            break;
        }
        filled += n as usize;
    }
}
