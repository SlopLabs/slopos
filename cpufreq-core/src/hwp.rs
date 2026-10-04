//! Hardware-controlled performance states: the capability and request
//! registers and the energy-performance preference.
//!
//! A performance level is an abstract unit. On a part that is not hybrid it is
//! the bus-clock ratio; on a hybrid one the P-cores' levels are scaled
//! differently from the E-cores', which [`crate::freq::perf_scaling_khz`]
//! accounts for.

/// `IA32_HWP_CAPABILITIES` (`0x771`), per logical CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HwpCaps {
    pub highest: u8,
    pub guaranteed: u8,
    pub efficient: u8,
    pub lowest: u8,
}

impl HwpCaps {
    pub const fn from_msr(value: u64) -> Self {
        Self {
            highest: value as u8,
            guaranteed: (value >> 8) as u8,
            efficient: (value >> 16) as u8,
            lowest: (value >> 24) as u8,
        }
    }
}

/// `IA32_HWP_REQUEST` (`0x774`), per logical CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HwpRequest {
    pub min: u8,
    pub max: u8,
    /// `0` leaves the operating point to the hardware.
    pub desired: u8,
    pub epp: u8,
    /// Bits 41:32, `0` letting the hardware choose the averaging window.
    pub activity_window: u16,
    /// Bit 42: take the fields from `IA32_HWP_REQUEST_PKG` instead.
    pub package_control: bool,
}

impl HwpRequest {
    const ACTIVITY_WINDOW_MASK: u64 = 0x3FF;
    const PACKAGE_CONTROL: u64 = 1 << 42;

    pub const fn from_msr(value: u64) -> Self {
        Self {
            min: value as u8,
            max: (value >> 8) as u8,
            desired: (value >> 16) as u8,
            epp: (value >> 24) as u8,
            activity_window: ((value >> 32) & Self::ACTIVITY_WINDOW_MASK) as u16,
            package_control: value & Self::PACKAGE_CONTROL != 0,
        }
    }

    pub const fn to_msr(self) -> u64 {
        let mut value = self.min as u64
            | (self.max as u64) << 8
            | (self.desired as u64) << 16
            | (self.epp as u64) << 24
            | ((self.activity_window as u64) & Self::ACTIVITY_WINDOW_MASK) << 32;
        if self.package_control {
            value |= Self::PACKAGE_CONTROL;
        }
        value
    }
}

/// The preference values the SDM names; anything between is a blend.
pub const EPP_PERFORMANCE: u8 = 0x00;
pub const EPP_BALANCE_PERFORMANCE: u8 = 0x80;
pub const EPP_BALANCE_POWER: u8 = 0xC0;
pub const EPP_POWER: u8 = 0xFF;

const EPP_NAMES: [(&str, u8); 4] = [
    ("performance", EPP_PERFORMANCE),
    ("balance_performance", EPP_BALANCE_PERFORMANCE),
    ("balance_power", EPP_BALANCE_POWER),
    ("power", EPP_POWER),
];

/// A preference by name, or as a decimal or `0x` hexadecimal byte.
pub fn parse_epp(text: &str) -> Option<u8> {
    if let Some(&(_, value)) = EPP_NAMES.iter().find(|(name, _)| *name == text) {
        return Some(value);
    }
    match text.strip_prefix("0x") {
        Some(hex) => u8::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// The name of a preference the SDM names, `None` for a blend.
pub fn epp_name(epp: u8) -> Option<&'static str> {
    EPP_NAMES
        .iter()
        .find(|&&(_, value)| value == epp)
        .map(|&(name, _)| name)
}

/// The bounds the kernel asks HWP to choose within; `0` is the capability's
/// own bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub min: u8,
    pub max: u8,
}

impl Limits {
    /// Packed as `cpu_perf_ctl` carries them: `min | max << 8`.
    pub const fn from_packed(packed: u64) -> Self {
        Self {
            min: packed as u8,
            max: (packed >> 8) as u8,
        }
    }

    pub const fn packed(self) -> u64 {
        self.min as u64 | (self.max as u64) << 8
    }
}

/// Autonomous selection between `limits` at preference `epp`: what the kernel
/// writes to each CPU's `IA32_HWP_REQUEST`. With turbo disabled the ceiling is
/// the guaranteed level, the highest one sustainable without it.
pub fn autonomous_request(caps: HwpCaps, epp: u8, limits: Limits, turbo: bool) -> HwpRequest {
    let floor = caps.lowest.min(caps.highest);
    let ceiling = if turbo {
        caps.highest
    } else {
        caps.guaranteed.clamp(floor, caps.highest)
    };
    let bound = |asked: u8, default: u8| {
        if asked == 0 {
            default
        } else {
            asked.clamp(floor, ceiling)
        }
    };
    let max = bound(limits.max, ceiling);
    let min = bound(limits.min, floor).min(max);
    HwpRequest {
        min,
        max,
        desired: 0,
        epp,
        activity_window: 0,
        package_control: false,
    }
}
