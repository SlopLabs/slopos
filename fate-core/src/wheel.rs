//! The wheel's pockets, the boot ring's twelve segments numbered as a
//! clock's hours, and the pocket a fate lands in.

use slopos_splash_core::SEGMENTS;

pub const POCKETS: usize = SEGMENTS;

/// The number of pocket `k`, clockwise from the top: 12, then 1 to 11, so
/// odd and even alternate round the ring.
pub const fn number(pocket: usize) -> u8 {
    match pocket % POCKETS {
        0 => 12,
        k => k as u8,
    }
}

/// Whether landing on `number` starts the system: odd does.
pub const fn starts(number: u8) -> bool {
    number % 2 == 1
}

/// The kernel's verdict on a fate, which the wheel only shows.
pub const fn fate_starts(fate: u32) -> bool {
    fate & 1 == 1
}

/// The pocket a fate lands in: one whose number has the fate's parity,
/// picked by its other bits.
pub fn landing(fate: u32) -> usize {
    let want = fate_starts(fate);
    let pick = (fate >> 1) % (POCKETS as u32 / 2);
    (0..POCKETS)
        .filter(|&k| starts(number(k)) == want)
        .nth(pick as usize)
        .unwrap_or(0)
}
