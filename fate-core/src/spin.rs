//! The spin as a function of time: the wheel's rotation, its speed, the
//! flapper the pegs lift, and which phase a moment falls in.
//!
//! Everything is a function of milliseconds since the spin began, never of
//! frames drawn, so a slow machine shows fewer frames of the same spin rather
//! than a longer one.

use crate::wheel::{self, Landing, POCKETS, TURN};

/// The wheel sits still to be seen, then draws back for the flick.
pub const WIND_MS: u32 = 400;
const WIND_STILL_MS: u32 = 150;
/// The wheel turns and slows to a stop.
pub const SPIN_MS: u32 = 4600;
pub const LAND_MS: u32 = WIND_MS + SPIN_MS;
/// The pocket it stopped on lights and its number fills the hub.
pub const SETTLE_MS: u32 = 700;
/// The verdict, with a bar running down to what follows it.
pub const VERDICT_MS: u32 = 1600;
pub const END_MS: u32 = LAND_MS + SETTLE_MS + VERDICT_MS;

/// Whole turns before the last partial one.
const TURNS: u64 = 4;
/// How far the wheel draws back before the flick: an eighth of a pocket,
/// short of any peg.
const WIND_BACK: u32 = TURN / POCKETS as u32 / 8;

/// How far a peg lifts the flapper before it passes, and how far past the
/// pointer it is when the flapper has dropped back, in 65536ths of a pocket.
/// Both are under the two fifths of a pocket a landing keeps clear.
const LIFT_REACH: u32 = 0x3852;
const DROP_REACH: u32 = 0x0F5C;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Phase {
    Spinning,
    Settling,
    Verdict,
    Done,
}

pub const fn phase(t_ms: u32) -> Phase {
    if t_ms < LAND_MS {
        Phase::Spinning
    } else if t_ms < LAND_MS + SETTLE_MS {
        Phase::Settling
    } else if t_ms < END_MS {
        Phase::Verdict
    } else {
        Phase::Done
    }
}

/// `part` of `whole`, as 0..=256.
pub const fn progress(part: u32, whole: u32) -> u32 {
    if part >= whole {
        256
    } else {
        (part as u64 * 256 / whole as u64) as u32
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Spin {
    landing: Landing,
    start: u32,
    travel: u64,
}

impl Spin {
    pub fn new(fate: u32) -> Self {
        let landing = wheel::landing(fate);
        // At rest with pocket 0 centred under the pointer.
        let start = TURN - wheel::pocket_centre(0);
        let end = (TURN - landing.angle()) % TURN;
        let last = (end + TURN - start) % TURN;
        Self {
            landing,
            start,
            travel: TURNS * TURN as u64 + last as u64 + WIND_BACK as u64,
        }
    }

    pub const fn landing(&self) -> Landing {
        self.landing
    }

    pub const fn number(&self) -> u8 {
        self.landing.number()
    }

    pub const fn starts(&self) -> bool {
        wheel::starts(self.landing.number())
    }

    /// The wheel's rotation `t_ms` into the spin, clockwise, in `[0, TURN)`.
    pub fn rotation(&self, t_ms: u32) -> u32 {
        let drawn = TURN + self.start - WIND_BACK;
        if t_ms < WIND_MS {
            let back = ease_in_out(progress(
                t_ms.saturating_sub(WIND_STILL_MS),
                WIND_MS - WIND_STILL_MS,
            ));
            return (TURN + self.start - ((WIND_BACK * back) >> 8)) % TURN;
        }
        let done = ease_out(progress_q16(t_ms - WIND_MS));
        let moved = (self.travel * done as u64) >> 16;
        ((drawn as u64 + moved) % TURN as u64) as u32
    }

    /// Pockets a second the wheel turns at `t_ms`.
    pub fn speed(&self, t_ms: u32) -> f32 {
        if !(WIND_MS..LAND_MS).contains(&t_ms) {
            return 0.0;
        }
        let left = 1.0 - (t_ms - WIND_MS) as f32 / SPIN_MS as f32;
        let pockets = self.travel as f32 * POCKETS as f32 / TURN as f32;
        3.0 * left * left * pockets * 1000.0 / SPIN_MS as f32
    }
}

fn progress_q16(t_ms: u32) -> u32 {
    if t_ms >= SPIN_MS {
        1 << 16
    } else {
        ((t_ms as u64) << 16) as u32 / SPIN_MS
    }
}

/// `0..=256` eased in and out.
fn ease_in_out(t: u32) -> u32 {
    let t = t.min(256);
    (t * t * (768 - 2 * t)) >> 16
}

/// `1 - (1 - u)^3` in Q16: a flick that bleeds speed until it stops.
pub fn ease_out(u: u32) -> u32 {
    let left = (1u64 << 16) - u.min(1 << 16) as u64;
    let cubed = (((left * left) >> 16) * left) >> 16;
    ((1u64 << 16) - cubed) as u32
}

/// How far the peg nearest the pointer lifts the flapper at `rotation`,
/// 0..=256. A clockwise wheel brings each divider's peg up to the pointer, so
/// the lift grows as one nears and falls away quickly once it has passed.
pub fn flap(rotation: u32) -> u32 {
    let (_, depth) = wheel::under_pointer(rotation);
    if depth < LIFT_REACH {
        (LIFT_REACH - depth) * 256 / LIFT_REACH
    } else if TURN - depth < DROP_REACH {
        (DROP_REACH - (TURN - depth)) * 256 / DROP_REACH
    } else {
        0
    }
}

/// How much a pocket's colours run together at `speed` pockets a second,
/// 0..=256: past a few pockets a frame the eye sees a blur, not pockets, and
/// drawing them sharp only shows them strobing backwards.
pub fn blur(speed: f32) -> u32 {
    const SHARP: f32 = 8.0;
    const SMEARED: f32 = 40.0;
    const MOST: f32 = 0.82;
    let t = ((speed - SHARP) / (SMEARED - SHARP)).clamp(0.0, 1.0);
    (t * MOST * 256.0) as u32
}
