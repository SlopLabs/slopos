//! The wheel's pockets and where a fate lands among them.
//!
//! Angles are fixed point: a whole turn is [`TURN`], clockwise, so a rotation
//! wraps by plain modular arithmetic.

/// One full turn.
pub const TURN: u32 = 1 << 16;

pub const POCKETS: usize = 12;

/// The pockets' numbers, clockwise from the wheel's zero. Odd and even
/// alternate, so every divider separates a win from a loss.
pub const NUMBERS: [u8; POCKETS] = [1, 8, 3, 10, 5, 12, 7, 2, 9, 4, 11, 6];

/// Whether landing on `number` starts the system: odd does.
pub const fn starts(number: u8) -> bool {
    number % 2 == 1
}

/// The kernel's verdict on a fate, which the wheel only shows.
pub const fn fate_starts(fate: u32) -> bool {
    fate & 1 == 1
}

/// Where the pointer comes to rest.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Landing {
    pub pocket: usize,
    /// How far into the pocket, from its leading divider, in 65536ths of its
    /// width. Kept to the middle two fifths, so the rest is never in doubt.
    pub depth: u32,
}

impl Landing {
    pub const fn number(self) -> u8 {
        NUMBERS[self.pocket]
    }

    /// The wheel angle under the pointer at rest.
    pub const fn angle(self) -> u32 {
        (((self.pocket as u32) << 16) + self.depth) / POCKETS as u32
    }
}

/// The pocket a fate lands in: one whose number has the fate's parity, picked
/// and placed by the fate's other bits.
pub fn landing(fate: u32) -> Landing {
    let want = fate_starts(fate);
    let pick = (fate >> 1) % (POCKETS as u32 / 2);
    let pocket = NUMBERS
        .iter()
        .enumerate()
        .filter(|(_, n)| starts(**n) == want)
        .nth(pick as usize)
        .map_or(0, |(i, _)| i);
    let spread = (fate >> 4) & 0xFFFF;
    let depth = 0x4CCC + ((spread * 0x6666) >> 16);
    Landing { pocket, depth }
}

/// The pocket under the pointer when the wheel is turned by `rotation`, and
/// how far into it the pointer is, as [`Landing::depth`] counts.
pub fn under_pointer(rotation: u32) -> (usize, u32) {
    let angle = TURN.wrapping_sub(rotation) % TURN;
    let at = angle * POCKETS as u32;
    ((at >> 16) as usize, at & 0xFFFF)
}

/// The wheel angle at the middle of `pocket`.
pub const fn pocket_centre(pocket: usize) -> u32 {
    (((pocket as u32) << 16) + 0x8000) / POCKETS as u32
}

/// The wheel angle of the divider that leads `pocket`.
pub const fn divider(pocket: usize) -> u32 {
    ((pocket as u32) << 16) / POCKETS as u32
}
