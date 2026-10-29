//! The spin as a function of time: the splash's lit ring dimming to the
//! dial, a light running round it and slowing to a stop on the fate's
//! pocket, the verdict, and the fade to black.
//!
//! Everything is a function of milliseconds since the wheel was first shown,
//! never of frames drawn, so a slow machine shows fewer frames of the same
//! spin rather than a longer one.

use crate::wheel::{self, POCKETS};

/// The ring the splash left lit dims to the dial but for the top segment,
/// where the light starts.
pub const INTRO_MS: u32 = 500;
/// The light runs round and slows to a stop.
pub const CHASE_MS: u32 = 4400;
pub const LAND_MS: u32 = INTRO_MS + CHASE_MS;
/// The pocket it stopped on takes the verdict's colour and its number fills
/// the middle.
pub const SETTLE_MS: u32 = 600;
pub const HOLD_MS: u32 = 1400;
/// Everything goes to black before what follows.
pub const FADE_MS: u32 = 500;
pub const FADE_AT: u32 = LAND_MS + SETTLE_MS + HOLD_MS;
pub const END_MS: u32 = FADE_AT + FADE_MS;

/// Whole turns before the last partial one.
const TURNS: u32 = 5;
/// How long a segment takes to go dark once the light has left it, as an
/// incandescent lamp does: a long trail at speed, a short one as it slows.
const AFTERGLOW_MS: f32 = 110.0;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Phase {
    Intro,
    Chasing,
    Settling,
    Holding,
    Fading,
    Done,
}

pub const fn phase(t_ms: u32) -> Phase {
    if t_ms < INTRO_MS {
        Phase::Intro
    } else if t_ms < LAND_MS {
        Phase::Chasing
    } else if t_ms < LAND_MS + SETTLE_MS {
        Phase::Settling
    } else if t_ms < FADE_AT {
        Phase::Holding
    } else if t_ms < END_MS {
        Phase::Fading
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

/// `0..=256` eased in and out.
pub const fn ease(t: u32) -> u32 {
    let t = if t > 256 { 256 } else { t };
    (t * t * (768 - 2 * t)) >> 16
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Spin {
    pocket: usize,
    /// Segments the light runs, from the top to its pocket.
    travel: u32,
}

impl Spin {
    pub fn new(fate: u32) -> Self {
        let pocket = wheel::landing(fate);
        Self {
            pocket,
            travel: TURNS * POCKETS as u32 + pocket as u32,
        }
    }

    pub const fn pocket(&self) -> usize {
        self.pocket
    }

    pub const fn number(&self) -> u8 {
        wheel::number(self.pocket)
    }

    pub const fn starts(&self) -> bool {
        wheel::starts(self.number())
    }

    /// How far the light has run `t_ms` in, in segments from the top: none
    /// before the chase, all of it from the landing on, slowing at a steady
    /// rate between, as a thing let go to coast does.
    pub fn position(&self, t_ms: u32) -> f32 {
        let u = (t_ms.saturating_sub(INTRO_MS) as f32 / CHASE_MS as f32).min(1.0);
        self.travel as f32 * (1.0 - (1.0 - u) * (1.0 - u))
    }

    /// When the light had run `position` segments.
    fn reached(&self, position: f32) -> f32 {
        let left = (1.0 - position / self.travel as f32).max(0.0);
        INTRO_MS as f32 + (1.0 - libm::sqrtf(left)) * CHASE_MS as f32
    }

    /// The segment under the light, counted from the top with every turn:
    /// the nearest to where it has run.
    pub fn head(&self, t_ms: u32) -> u32 {
        (self.position(t_ms) + 0.5) as u32
    }

    /// How lit segment `k` is `t_ms` in, `0.0..=1.0`: whole under the light,
    /// dimming after it leaves, dark where it has not been.
    pub fn glow(&self, t_ms: u32, k: usize) -> f32 {
        if t_ms < INTRO_MS {
            return 0.0;
        }
        let head = self.head(t_ms);
        let behind = (head as usize + POCKETS - k % POCKETS) % POCKETS;
        if behind == 0 {
            return 1.0;
        }
        let Some(visit) = head.checked_sub(behind as u32) else {
            return 0.0;
        };
        let since = t_ms as f32 - self.reached(visit as f32 + 0.5);
        libm::expf(-since.max(0.0) / AFTERGLOW_MS)
    }
}
