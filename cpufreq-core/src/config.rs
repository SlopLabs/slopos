//! The kernel command line's frequency and placement knobs:
//!
//! - `cpufreq=hwp` (default) enables HWP and asks for autonomous selection
//!   between the CPU's lowest and highest levels; `cpufreq=firmware` leaves
//!   every frequency register as the firmware left it.
//! - `cpufreq.epp=<name|byte>` is the energy-performance preference HWP is
//!   given: `performance`, `balance_performance` (default), `balance_power`,
//!   `power`, or a value 0-255.
//! - `sched.hybrid=on` (default) ranks idle CPUs by core type and SMT state;
//!   `off` treats every idle CPU alike.
//!
//! The last occurrence of a key wins, so a value appended after the loader's
//! (a built-in command line) overrides it.

use crate::hwp::{EPP_BALANCE_PERFORMANCE, parse_epp};
use crate::place::Placement;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Firmware,
    Hwp,
}

impl Policy {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Firmware),
            1 => Some(Self::Hwp),
            _ => None,
        }
    }

    pub const fn raw(self) -> u32 {
        match self {
            Self::Firmware => 0,
            Self::Hwp => 1,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Firmware => "firmware",
            Self::Hwp => "hwp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub policy: Policy,
    pub epp: u8,
    pub placement: Placement,
    /// Bit per knob whose value was not understood and kept its default:
    /// [`Config::BAD_POLICY`], [`Config::BAD_EPP`], [`Config::BAD_PLACEMENT`].
    pub rejected: u8,
}

impl Config {
    pub const BAD_POLICY: u8 = 1 << 0;
    pub const BAD_EPP: u8 = 1 << 1;
    pub const BAD_PLACEMENT: u8 = 1 << 2;

    pub const DEFAULT: Self = Self {
        policy: Policy::Hwp,
        epp: EPP_BALANCE_PERFORMANCE,
        placement: Placement::Ranked,
        rejected: 0,
    };

    pub fn parse(cmdline: &str) -> Self {
        let mut config = Self::DEFAULT;
        for token in cmdline.split_ascii_whitespace() {
            if let Some(value) = token.strip_prefix("cpufreq=") {
                match value {
                    "hwp" => config.policy = Policy::Hwp,
                    "firmware" => config.policy = Policy::Firmware,
                    _ => config.rejected |= Self::BAD_POLICY,
                }
            } else if let Some(value) = token.strip_prefix("cpufreq.epp=") {
                match parse_epp(value) {
                    Some(epp) => config.epp = epp,
                    None => config.rejected |= Self::BAD_EPP,
                }
            } else if let Some(value) = token.strip_prefix("sched.hybrid=") {
                match value {
                    "on" => config.placement = Placement::Ranked,
                    "off" => config.placement = Placement::Flat,
                    _ => config.rejected |= Self::BAD_PLACEMENT,
                }
            }
        }
        config
    }
}
