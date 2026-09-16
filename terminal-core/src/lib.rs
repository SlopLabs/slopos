//! Pure terminal-emulator core: the VT grid model and the input/selection
//! logic, with zero dependency on syscalls, the compositor protocol, or font
//! globals — which is what makes it host-testable. The userland terminal app
//! supplies the IO, rendering, and protocol bridge; the kernel does not link
//! it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod damage;
pub mod grid;
pub mod input;

/// `n` as decimal digits, most significant first, with the count. One writer
/// for both the key/mouse encoder's fixed buffer and the grid's reply queue.
pub(crate) fn decimal(n: u16) -> ([u8; 5], usize) {
    let mut digits = [0u8; 5];
    let mut count = 0;
    let mut v = n;
    loop {
        digits[count] = b'0' + (v % 10) as u8;
        count += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    digits[..count].reverse();
    (digits, count)
}
